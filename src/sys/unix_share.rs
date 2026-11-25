//! Sharing an eventfd with other unix processes, the Linux way: inherit the
//! descriptor(s) across `exec`, or pass them over a unix socket
//! (`SCM_RIGHTS`). Used by both the native backend (one descriptor: the
//! kernel eventfd) and the shim (three: shared memory object, pipe read end,
//! pipe write end).
//!
//! Everything is identified by an 8-byte tag: `gkefd1:n` / `gkefd1:u` for
//! an eventfd (native / unix shim), `gkch1:Sn` etc. for channel ends,
//! `gkshm1:r` for shared memory, `gkbyt1:b` for a byte message
//! (`GkProcess::send_bytes`: the tag, a little-endian `u32` length and the
//! bytes, no descriptors). Tokens
//! are text: `<tag>:<fd>[:<fd>...]` with decimal descriptor numbers. A
//! native eventfd token is simply
//! the inherited eventfd's number.
//!
//! Over a socket, the tag is sent as data, with the descriptors attached as
//! `SCM_RIGHTS`.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

/// Length of every tag (`gkefd1:n`, `gkch1:Su`, ...).
pub(crate) const TAG_LEN: usize = 8;
/// Most descriptors a message may carry (shim channel end: 1 + 3 + 3).
const MAX_FDS: usize = 8;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub(crate) fn invalid_token() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid eventfd token")
}

/// Inheritable copies of an eventfd's descriptors (close-on-exec cleared),
/// for a child spawned while this value is alive, plus the token naming
/// them. Dropping it closes this process's copies.
#[derive(Debug)]
pub(crate) struct InheritableExport {
    token: String,
    _fds: Vec<OwnedFd>,
}

impl InheritableExport {
    pub(crate) fn new(tag: &str, fds: &[BorrowedFd<'_>]) -> io::Result<Self> {
        debug_assert_eq!(tag.len(), TAG_LEN);
        let copies = fds
            .iter()
            .map(|fd| {
                // SAFETY: F_DUPFD makes a new descriptor without
                // FD_CLOEXEC, so it survives exec in a child.
                let n = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD, 3) })?;
                // SAFETY: a fresh descriptor we own.
                Ok(unsafe { OwnedFd::from_raw_fd(n) })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut token = tag.to_owned();
        for fd in &copies {
            token.push_str(&format!(":{}", fd.as_raw_fd()));
        }
        Ok(Self { token, _fds: copies })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

/// Parses a token with `tag` naming exactly `n` descriptors.
pub(crate) fn parse(token: &str, tag: &str, n: usize) -> io::Result<Vec<RawFd>> {
    let rest = token.trim().strip_prefix(tag).and_then(|r| r.strip_prefix(':')).ok_or_else(invalid_token)?;
    let fds =
        rest.split(':').map(|p| p.parse::<RawFd>().ok().filter(|&fd| fd >= 0)).collect::<Option<Vec<_>>>().ok_or_else(invalid_token)?;
    if fds.len() != n {
        return Err(invalid_token());
    }
    Ok(fds)
}

/// True if `fd` is an open descriptor.
pub(crate) fn is_open(fd: RawFd) -> bool {
    // SAFETY: F_GETFD only queries; bad values fail with EBADF.
    let r = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    r != -1
}

pub(crate) fn set_cloexec(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: plain fcntl on an open descriptor.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
}

fn header(tag: &str) -> [u8; TAG_LEN] {
    tag.as_bytes().try_into().expect("8-byte tag")
}

#[cfg(not(target_vendor = "apple"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(target_vendor = "apple")]
const SEND_FLAGS: libc::c_int = 0;

#[cfg(not(target_vendor = "apple"))]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(target_vendor = "apple")]
const RECV_FLAGS: libc::c_int = 0;

/// Control-message buffer, aligned for `cmsghdr`.
#[repr(C, align(8))]
struct CmsgBuf([u8; 64]);

/// Sends `tag` with `fds` attached (`SCM_RIGHTS`) on the
/// connected unix socket `sock`.
pub(crate) fn send(sock: BorrowedFd<'_>, tag: &str, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    assert!(fds.len() <= MAX_FDS);
    let head = header(tag);
    let mut iov = libc::iovec { iov_base: head.as_ptr().cast_mut().cast(), iov_len: TAG_LEN };
    let payload = (fds.len() * size_of::<libc::c_int>()) as libc::c_uint;
    let mut cbuf = CmsgBuf([0; 64]);
    // SAFETY: CMSG_* are pure arithmetic / pointer helpers on our buffers.
    unsafe {
        let space = libc::CMSG_SPACE(payload) as usize;
        assert!(space <= cbuf.0.len());
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1 as _;
        msg.msg_control = cbuf.0.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(payload) as _;
        let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
        for (i, fd) in fds.iter().enumerate() {
            data.add(i).write_unaligned(fd.as_raw_fd());
        }
        let sent = loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &msg, SEND_FLAGS);
            if n >= 0 {
                break n as usize;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        };
        // The descriptors went with the first byte; send any rest as data.
        write_all(sock, &head[sent..])
    }
}

fn write_all(sock: BorrowedFd<'_>, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: writes from a live buffer to an open socket.
        let n = unsafe { libc::send(sock.as_raw_fd(), buf.as_ptr().cast(), buf.len(), SEND_FLAGS) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// Receives a message sent by [`send`] with `tag` and exactly `n`
/// descriptors. The descriptors are close-on-exec and owned by the caller;
/// on any mismatch they are closed and an error is returned. A byte message
/// ([`send_bytes`]) found instead is read to its end, so the connection stays
/// usable, and refused the same way.
pub(crate) fn recv(sock: BorrowedFd<'_>, tag: &str, n: usize) -> io::Result<Vec<OwnedFd>> {
    let (head, fds) = recv_head(sock)?;
    if head == BYTES_TAG {
        drop(fds);
        skip_bytes_body(sock)?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not the expected kind of object: a byte message was sent"));
    }
    if head != header(tag) || fds.len() != n {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not the expected kind of object (sent by a different backend?)"));
    }
    for fd in &fds {
        set_cloexec(fd.as_fd())?;
    }
    Ok(fds)
}

/// Reads the next message's 8-byte tag and the descriptors attached to it
/// (owned, so dropping them closes them).
fn recv_head(sock: BorrowedFd<'_>) -> io::Result<([u8; TAG_LEN], Vec<OwnedFd>)> {
    let mut head = [0u8; TAG_LEN];
    let mut iov = libc::iovec { iov_base: head.as_mut_ptr().cast(), iov_len: TAG_LEN };
    let mut cbuf = CmsgBuf([0; 64]);
    let mut fds = Vec::new();
    // SAFETY: recvmsg into our buffers; CMSG_* walk the received control
    // data within `cbuf`, and every received descriptor is owned right away.
    let got = unsafe {
        let space = libc::CMSG_SPACE((MAX_FDS * size_of::<libc::c_int>()) as libc::c_uint) as usize;
        assert!(space <= cbuf.0.len());
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1 as _;
        msg.msg_control = cbuf.0.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let got = loop {
            let r = libc::recvmsg(sock.as_raw_fd(), &mut msg, RECV_FLAGS);
            if r >= 0 {
                break r as usize;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        };
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
                for i in 0..bytes / size_of::<libc::c_int>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "eventfd message: too many descriptors"));
        }
        got
    };
    if got == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    read_exact(sock, &mut head[got..])?;
    Ok((head, fds))
}

/// Tag of a byte message.
const BYTES_TAG: [u8; TAG_LEN] = crate::process::BYTES_TAG;

/// Sends a byte message (`GkProcess::send_bytes`): the tag, the length as
/// a little-endian `u32`, the bytes. No descriptors. The caller checked
/// `msg.len() <= BYTES_MAX`.
pub(crate) fn send_bytes(sock: BorrowedFd<'_>, msg: &[u8]) -> io::Result<()> {
    debug_assert!(msg.len() <= crate::process::BYTES_MAX);
    let mut frame = Vec::with_capacity(TAG_LEN + 4 + msg.len());
    frame.extend_from_slice(&BYTES_TAG);
    frame.extend_from_slice(&(msg.len() as u32).to_le_bytes());
    frame.extend_from_slice(msg);
    write_all(sock, &frame)
}

/// Receives a byte message sent with [`send_bytes`] into `buf` (replacing
/// its contents). An object found instead is refused with `InvalidData`
/// (its descriptors are closed); the connection stays usable.
pub(crate) fn recv_bytes(sock: BorrowedFd<'_>, buf: &mut Vec<u8>) -> io::Result<()> {
    let (head, fds) = recv_head(sock)?;
    if head != BYTES_TAG {
        drop(fds);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a byte message: an object was sent"));
    }
    let len = read_bytes_len(sock)?;
    buf.clear();
    buf.resize(len, 0);
    read_exact(sock, buf)?;
    if !fds.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "byte message with descriptors attached"));
    }
    Ok(())
}

/// Reads a byte message's length, refusing more than `BYTES_MAX`.
fn read_bytes_len(sock: BorrowedFd<'_>) -> io::Result<usize> {
    let mut len = [0u8; 4];
    read_exact(sock, &mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > crate::process::BYTES_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "byte message longer than BYTES_MAX"));
    }
    Ok(len)
}

/// Reads and discards the rest of a byte message whose tag was read.
fn skip_bytes_body(sock: BorrowedFd<'_>) -> io::Result<()> {
    let mut rest = read_bytes_len(sock)?;
    let mut scratch = [0u8; 4096];
    while rest > 0 {
        let n = rest.min(scratch.len());
        read_exact(sock, &mut scratch[..n])?;
        rest -= n;
    }
    Ok(())
}

/// The process id of the peer of the connected unix socket `sock`, as the
/// kernel recorded it when the connection was made; `None` where the OS
/// can't tell.
pub(crate) fn peer_pid(sock: BorrowedFd<'_>) -> Option<u32> {
    let pid = peer_pid_os(sock.as_raw_fd())?;
    u32::try_from(pid).ok().filter(|&p| p != 0)
}

/// Reads one socket option of type `T`.
#[allow(dead_code)]
fn sockopt<T>(fd: RawFd, level: libc::c_int, name: libc::c_int) -> Option<T> {
    let mut v = std::mem::MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `v`, which has room
    // for a `T`; on success `T` (plain C data, zero-initialised) is valid.
    let r = unsafe { libc::getsockopt(fd, level, name, v.as_mut_ptr().cast(), &mut len) };
    if r == -1 || (len as usize) < size_of::<T>() {
        return None;
    }
    // SAFETY: zero-initialised and filled by the kernel; plain C data.
    Some(unsafe { v.assume_init() })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_pid_os(fd: RawFd) -> Option<libc::pid_t> {
    /// `struct ucred` (declared here: the libc crate lacks it on Android).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Ucred {
        pid: libc::pid_t,
        uid: libc::uid_t,
        gid: libc::gid_t,
    }
    sockopt::<Ucred>(fd, libc::SOL_SOCKET, libc::SO_PEERCRED).map(|c| c.pid)
}

#[cfg(target_vendor = "apple")]
fn peer_pid_os(fd: RawFd) -> Option<libc::pid_t> {
    sockopt::<libc::pid_t>(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID)
}

#[cfg(target_os = "freebsd")]
fn peer_pid_os(fd: RawFd) -> Option<libc::pid_t> {
    // FreeBSD 13+ fills `cr_pid`; older kernels leave it 0 (-> None).
    let cred = sockopt::<libc::xucred>(fd, libc::SOL_LOCAL, libc::LOCAL_PEERCRED)?;
    if cred.cr_version != libc::XUCRED_VERSION {
        return None;
    }
    // SAFETY: a union of a pointer-sized pad and `cr_pid`; the kernel wrote
    // `cr_pid` (and it was zero-initialised before).
    Some(unsafe { cred.cr_pid__c_anonymous_union.cr_pid })
}

#[cfg(target_os = "netbsd")]
fn peer_pid_os(fd: RawFd) -> Option<libc::pid_t> {
    // Level 0 is SOL_LOCAL.
    sockopt::<libc::unpcbid>(fd, 0, libc::LOCAL_PEEREID).map(|c| c.unp_pid)
}

#[cfg(target_os = "openbsd")]
fn peer_pid_os(fd: RawFd) -> Option<libc::pid_t> {
    sockopt::<libc::sockpeercred>(fd, libc::SOL_SOCKET, libc::SO_PEERCRED).map(|c| c.pid)
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
fn peer_pid_os(fd: RawFd) -> Option<libc::pid_t> {
    let mut cred: *mut libc::ucred_t = std::ptr::null_mut();
    // SAFETY: getpeerucred allocates a ucred_t for us on success.
    if unsafe { libc::getpeerucred(fd, &mut cred) } == -1 || cred.is_null() {
        return None;
    }
    // SAFETY: `cred` is the live ucred_t getpeerucred returned.
    let pid = unsafe { libc::ucred_getpid(cred) };
    // SAFETY: freed once, as getpeerucred(3C) requires.
    unsafe { libc::ucred_free(cred) };
    // ucred_getpid returns -1 when the pid is not available.
    (pid >= 0).then_some(pid)
}

/// DragonFly and other unix: no way to learn the peer's pid.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos",
    target_os = "solaris"
)))]
fn peer_pid_os(_fd: RawFd) -> Option<libc::pid_t> {
    None
}

fn read_exact(sock: BorrowedFd<'_>, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: reads into a live buffer from an open socket.
        let n = unsafe { libc::recv(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn parse_tokens() {
        assert_eq!(parse("gkefd1:n:7", "gkefd1:n", 1).unwrap(), vec![7]);
        assert_eq!(parse(" gkefd1:u:3:4:5\n", "gkefd1:u", 3).unwrap(), vec![3, 4, 5]);
        for bad in ["", "gkefd1", "gkefd1:n", "gkefd1:u:1:2:3", "gkefd2:n:1", "gkefd1:n:-1", "gkefd1:n:x"] {
            assert_eq!(parse(bad, "gkefd1:n", 1).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[test]
    fn export_clears_cloexec() {
        let (a, _b) = UnixStream::pair().unwrap();
        let e = InheritableExport::new("gkefd1:n", &[a.as_fd()]).unwrap();
        let fd = parse(e.token(), "gkefd1:n", 1).unwrap()[0];
        // SAFETY: query only.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
        drop(e);
        assert!(!is_open(fd) || fd != a.as_raw_fd());
    }

    #[test]
    fn send_and_receive_descriptors() {
        let (tx, rx) = UnixStream::pair().unwrap();
        let (p, q) = UnixStream::pair().unwrap();
        send(tx.as_fd(), "gkefd1:u", &[p.as_fd(), q.as_fd(), p.as_fd()]).unwrap();
        let fds = recv(rx.as_fd(), "gkefd1:u", 3).unwrap();
        assert_eq!(fds.len(), 3);
        for fd in &fds {
            // SAFETY: query only.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        // Wrong kind or count: rejected (and the descriptors closed).
        send(tx.as_fd(), "gkefd1:n", &[p.as_fd()]).unwrap();
        assert_eq!(recv(rx.as_fd(), "gkefd1:u", 3).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Peer gone.
        drop(tx);
        assert_eq!(recv(rx.as_fd(), "gkefd1:u", 3).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}

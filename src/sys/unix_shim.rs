//! eventfd shim for unix systems without a kernel eventfd (macOS, iOS, the BSDs).
//!
//! Also built on Linux with the `force-shim` feature, so that it can be tested
//! against the real kernel eventfd.
//!
//! # Design
//!
//! The counter lives in a page of anonymous shared memory ([`SharedPage`]),
//! guarded by a cross-process spinlock in the same page, so it stays shared
//! between parent and child after `fork(2)`, like a kernel eventfd. A
//! non-blocking pipe makes it pollable: it holds **exactly one byte while the
//! counter is non-zero** and is empty otherwise. That byte is written and
//! drained under the lock, so the pipe is readable if and only if the counter
//! is non-zero, in every process.
//!
//! Both pipe ends are always `O_NONBLOCK`. A blocking `read` that finds the
//! counter at 0 waits in `poll(2)` for the pipe to become readable (which
//! works across processes) and tries again. A blocking `write` that would
//! overflow the counter (only possible near `EFD_MAX`) retries after short
//! sleeps of up to 1 ms. Every handle (the first
//! one and each [`Inner::try_clone`]) owns its own `dup` of the pipe's read
//! end. That descriptor is what [`AsFd`] returns, so each handle has a distinct
//! fd number and can be registered with kqueue / mio independently. The
//! pipe's own two ends are internal and never handed out.
//!
//! # Limitations
//!
//! * Raw `read(2)` / `write(2)` on the descriptor do **not** have eventfd
//!   semantics: it is a pipe. Use [`GkEventFd::read`](crate::GkEventFd::read)
//!   and [`GkEventFd::write`](crate::GkEventFd::write) (or the C ABI). Raw I/O
//!   on the descriptor desynchronises its readiness from the counter.
//! * The counter is shared across `fork(2)`, but not across `exec(2)` or
//!   with unrelated processes (the shared page is not passed along).
//! * If a process dies while holding the lock, another process takes it
//!   over and resynchronises the pipe with the counter (see `src/sys/shm.rs`).
//! * A plain `dup(2)` of the descriptor shares readiness (same pipe), but
//!   it is not a new eventfd handle; use `try_clone` to get one. (The compat
//!   / C layer recognises such dups by their pipe identity, see [`pipe_id`].)
//! * Only read readiness (`POLLIN`, `EVFILT_READ`) is meaningful.
//!   Write readiness (`POLLOUT`) is not reported, as it is for a Linux eventfd
//!   whose counter is below `EFD_MAX`. Writes that would block are rare and
//!   simply block (or fail with `EAGAIN`) in [`Inner::write`].

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use super::shm::{SharedPage, Signal};
use crate::flags::EfdFlags;

/// State shared by every handle of one eventfd in this process. The counter
/// itself is in `page`, which is also shared with forked processes.
#[derive(Debug)]
struct Shared {
    page: SharedPage,
    /// Read end: holds one byte iff the counter is > 0.
    pipe_r: OwnedFd,
    pipe_w: OwnedFd,
}

#[derive(Debug)]
pub(crate) struct Inner {
    shared: Arc<Shared>,
    /// This handle's own `dup` of `shared.pipe_r` (what `AsFd` exposes).
    fd: OwnedFd,
}

/// Retries a libc call returning `-1` on error while it fails with `EINTR`.
fn cvt_retry(mut f: impl FnMut() -> isize) -> io::Result<isize> {
    loop {
        let ret = f();
        if ret != -1 {
            return Ok(ret);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Number of bytes buffered in the pipe (crate tests only).
#[cfg(test)]
fn pending_bytes(fd: BorrowedFd<'_>) -> Option<libc::c_int> {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one `c_int` through the pointer, which points to
    // a live local; `fd` is a valid open descriptor for the call's duration.
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &mut n) };
    (ret != -1).then_some(n)
}

impl Signal for Shared {
    fn set_readable(&self) -> io::Result<()> {
        // Checked only in this crate's tests: raw I/O on the fd by users is
        // unsupported but must not panic their debug builds.
        #[cfg(test)]
        if let Some(n) = pending_bytes(self.pipe_r.as_fd()) {
            assert_eq!(n, 0, "eventfd shim: pipe not empty while counter was 0");
        }
        let byte = 1u8;
        let res = cvt_retry(|| {
            // SAFETY: writes 1 byte from a live local to a valid open fd.
            unsafe { libc::write(self.pipe_w.as_raw_fd(), (&raw const byte).cast(), 1) }
        });
        match res {
            Ok(_) => Ok(()),
            // Pipe already full (only possible after raw writes to the fd):
            // it is readable, which is all we need.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn clear_readable(&self) -> io::Result<()> {
        // Drain until empty. Normally this reads exactly one byte; looping
        // keeps readiness correct even if the fd was written to directly.
        let mut buf = [0u8; 64];
        let mut drained = 0usize;
        loop {
            let res = cvt_retry(|| {
                // SAFETY: reads at most `buf.len()` bytes into a live local
                // buffer from a valid open fd.
                unsafe { libc::read(self.pipe_r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) }
            });
            match res {
                Ok(0) => break, // write end closed: cannot happen while `self` lives
                Ok(n) => drained += n as usize,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if cfg!(test) {
            assert_eq!(drained, 1, "eventfd shim: pipe did not hold exactly 1 byte");
        }
        Ok(())
    }

    fn drain(&self) -> io::Result<()> {
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: reads into a live local buffer from a valid open fd.
            let r = cvt_retry(|| unsafe {
                libc::read(self.pipe_r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
            });
            match r {
                Ok(n) if n > 0 => continue,
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn wait_readable(&self) -> io::Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.pipe_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: one valid pollfd, no timeout.
            if unsafe { libc::poll(&mut pfd, 1, -1) } >= 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

/// Takes ownership of the two fds written by `pipe`/`pipe2`.
fn own_pair(fds: [RawFd; 2]) -> (OwnedFd, OwnedFd) {
    // SAFETY: a successful pipe()/pipe2() returned two fresh descriptors that
    // nothing else owns.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Creates a non-blocking pipe; both ends are close-on-exec iff `cloexec`.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn make_pipe(cloexec: bool) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [RawFd; 2] = [-1; 2];
    let flags = libc::O_NONBLOCK | if cloexec { libc::O_CLOEXEC } else { 0 };
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), flags) })?;
    Ok(own_pair(fds))
}

/// Creates a non-blocking pipe; both ends are close-on-exec iff `cloexec`.
///
/// No `pipe2` here (macOS, iOS): there is a short window in which another
/// thread's `fork`+`exec` could inherit the fds before `FD_CLOEXEC` is set.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
fn make_pipe(cloexec: bool) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [RawFd; 2] = [-1; 2];
    // SAFETY: `fds` has room for the two descriptors pipe writes.
    cvt(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    let (r, w) = own_pair(fds);
    for fd in [r.as_raw_fd(), w.as_raw_fd()] {
        // SAFETY (all four calls): plain fcntl on a valid fd owned by `r`/`w`.
        let fl = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) })?;
        if cloexec {
            let fd_fl = cvt(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
            cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, fd_fl | libc::FD_CLOEXEC) })?;
        }
    }
    Ok((r, w))
}

/// `dup`s `fd` to a new descriptor (>= 3, like std), close-on-exec iff `cloexec`.
fn dup(fd: BorrowedFd<'_>, cloexec: bool) -> io::Result<OwnedFd> {
    let cmd = if cloexec {
        libc::F_DUPFD_CLOEXEC
    } else {
        libc::F_DUPFD
    };
    // SAFETY: plain fcntl on a valid open fd.
    let new = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), cmd, 3) })?;
    // SAFETY: F_DUPFD(_CLOEXEC) returned a fresh descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

impl Inner {
    pub(crate) fn new(initval: u32, flags: EfdFlags) -> io::Result<Self> {
        let cloexec = flags.contains(EfdFlags::CLOEXEC);
        let (pipe_r, pipe_w) = make_pipe(cloexec)?;
        let shared = Shared {
            page: SharedPage::new(initval, flags)?,
            pipe_r,
            pipe_w,
        };
        if initval > 0 {
            shared.set_readable()?;
        }
        let fd = dup(shared.pipe_r.as_fd(), cloexec)?;
        Ok(Self {
            shared: Arc::new(shared),
            fd,
        })
    }

    pub(crate) fn read(&self) -> io::Result<u64> {
        self.shared.page.read(&*self.shared)
    }

    pub(crate) fn write(&self, value: u64) -> io::Result<()> {
        self.shared.page.write(value, &*self.shared)
    }

    /// New handle on the same counter with its own descriptor. Like
    /// `OwnedFd::try_clone`, the new descriptor is always close-on-exec.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            shared: Arc::clone(&self.shared),
            fd: dup(self.fd.as_fd(), true)?,
        })
    }

    pub(crate) fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.shared.page.set_nonblocking(nb);
        Ok(())
    }

    pub(crate) fn is_nonblocking(&self) -> io::Result<bool> {
        Ok(self.shared.page.is_nonblocking())
    }
}

impl AsFd for Inner {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Identity of the pipe behind a descriptor (`st_dev`, `st_ino`). Every `dup`
/// of an eventfd's descriptor (raw or not) has the identity of its pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PipeId {
    dev: u64,
    ino: u64,
}

/// Hashes to a single `u64` so it can key a `nohash_hasher` map (which
/// accepts exactly one integer write). Equality still compares both fields.
impl std::hash::Hash for PipeId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.ino ^ self.dev.rotate_left(32));
    }
}

impl nohash_hasher::IsEnabled for PipeId {}

/// The pipe identity of `fd`, or `None` if `fd` is not an open FIFO.
pub(crate) fn pipe_id(fd: RawFd) -> Option<PipeId> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat writes a `stat` through a valid pointer; any fd value is
    // allowed (invalid ones fail with EBADF).
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstat succeeded, so `st` is initialised.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::unnecessary_cast)] // field types differ between OSes
    ((st.st_mode & libc::S_IFMT) == libc::S_IFIFO).then_some(PipeId {
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
    })
}

/// A counter handle without a descriptor of its own, for the compat registry,
/// which owns the descriptor numbers itself (so `dup2` can replace them).
#[derive(Debug, Clone)]
pub(crate) struct Core(Arc<Shared>);

impl Core {
    pub(crate) fn read(&self) -> io::Result<u64> {
        self.0.page.read(&*self.0)
    }

    pub(crate) fn write(&self, value: u64) -> io::Result<()> {
        self.0.page.write(value, &*self.0)
    }

    /// Identity of this eventfd's pipe (see [`pipe_id`]).
    pub(crate) fn pipe_id(&self) -> Option<PipeId> {
        pipe_id(self.0.pipe_r.as_raw_fd())
    }
}

impl Inner {
    /// Splits the handle into its counter and its descriptor.
    pub(crate) fn into_core(self) -> (Core, OwnedFd) {
        (Core(self.shared), self.fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn nb() -> EfdFlags {
        EfdFlags::NONBLOCK
    }

    /// True if `fd` polls readable within `timeout_ms`.
    fn readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> bool {
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
        assert!(n >= 0, "poll: {}", io::Error::last_os_error());
        n == 1 && p.revents & libc::POLLIN != 0
    }

    fn now(e: &Inner) -> bool {
        readable(e.as_fd(), 0)
    }

    fn fd_flags(fd: RawFd) -> io::Result<libc::c_int> {
        // SAFETY: F_GETFD only queries; an invalid fd yields EBADF.
        cvt(unsafe { libc::fcntl(fd, libc::F_GETFD) })
    }

    fn all_fds(e: &Inner) -> [RawFd; 3] {
        [
            e.fd.as_raw_fd(),
            e.shared.pipe_r.as_raw_fd(),
            e.shared.pipe_w.as_raw_fd(),
        ]
    }

    #[test]
    fn initval_is_readable() {
        let e = Inner::new(3, nb()).unwrap();
        assert!(now(&e));
        assert_eq!(e.read().unwrap(), 3);
        assert!(!now(&e));
        assert_eq!(e.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn zero_initval_not_readable() {
        let e = Inner::new(0, nb()).unwrap();
        assert!(!now(&e));
    }

    #[test]
    fn poll_tracks_counter() {
        let e = Inner::new(0, nb()).unwrap();
        e.write(0).unwrap();
        assert!(!now(&e));
        e.write(2).unwrap();
        assert!(now(&e));
        e.write(5).unwrap();
        assert!(now(&e));
        assert_eq!(e.read().unwrap(), 7);
        assert!(!now(&e));
        e.write(crate::EFD_MAX).unwrap();
        assert_eq!(e.write(1).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(now(&e));
        assert_eq!(e.read().unwrap(), crate::EFD_MAX);
        assert!(!now(&e));
    }

    #[test]
    fn poll_tracks_semaphore() {
        let e = Inner::new(2, nb() | EfdFlags::SEMAPHORE).unwrap();
        assert!(now(&e));
        assert_eq!(e.read().unwrap(), 1);
        assert!(now(&e));
        e.write(1).unwrap();
        assert_eq!(e.read().unwrap(), 1);
        assert!(now(&e));
        assert_eq!(e.read().unwrap(), 1);
        assert!(!now(&e));
        assert_eq!(e.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    #[test]
    fn kqueue_read_filter() {
        let e = Inner::new(0, nb()).unwrap();
        // SAFETY: kqueue() takes no arguments; the result is checked.
        let kq = cvt(unsafe { libc::kqueue() }).unwrap();
        // SAFETY: fresh descriptor from kqueue().
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        // SAFETY: an all-zero kevent is a valid value.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = e.as_fd().as_raw_fd() as _;
        ev.filter = libc::EVFILT_READ;
        ev.flags = libc::EV_ADD | libc::EV_CLEAR;
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let wait = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        // SAFETY: one input event, no output buffer.
        let r = unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, &zero) };
        assert_eq!(r, 0, "{}", io::Error::last_os_error());

        let poll = |ts: &libc::timespec| {
            // SAFETY: an all-zero kevent is a valid value.
            let mut out: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: no input events, one output slot.
            let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut out, 1, ts) };
            assert!(n >= 0);
            (n == 1).then_some(out)
        };
        assert!(poll(&zero).is_none());
        let e2 = e.try_clone().unwrap();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            e2.write(1).unwrap();
        });
        let out = poll(&wait).expect("EVFILT_READ did not fire");
        let (ident, filter) = (out.ident, out.filter);
        assert_eq!(ident, e.as_fd().as_raw_fd() as _);
        assert_eq!(filter, libc::EVFILT_READ);
        t.join().unwrap();
        assert_eq!(e.read().unwrap(), 1);
        assert!(poll(&zero).is_none());
    }

    #[test]
    fn blocking_read_woken_by_other_thread() {
        let e = Inner::new(0, EfdFlags::NONE).unwrap();
        let e2 = e.try_clone().unwrap();
        let t = thread::spawn(move || e2.read().unwrap());
        thread::sleep(Duration::from_millis(50));
        e.write(42).unwrap();
        assert_eq!(t.join().unwrap(), 42);
        assert!(!now(&e));
    }

    #[test]
    fn clone_shares_counter_distinct_fd() {
        let a = Inner::new(0, nb()).unwrap();
        let b = a.try_clone().unwrap();
        let c = b.try_clone().unwrap();
        let fds = [
            a.as_fd().as_raw_fd(),
            b.as_fd().as_raw_fd(),
            c.as_fd().as_raw_fd(),
        ];
        assert!(fds[0] != fds[1] && fds[1] != fds[2] && fds[0] != fds[2]);
        assert!(fds.iter().all(|&f| f != a.shared.pipe_r.as_raw_fd()));
        assert!(!now(&a) && !now(&b) && !now(&c));
        a.write(4).unwrap();
        assert!(now(&a) && now(&b) && now(&c));
        b.write(1).unwrap();
        assert_eq!(c.read().unwrap(), 5);
        assert!(!now(&a) && !now(&b) && !now(&c));
        drop(a);
        b.write(1).unwrap();
        assert!(now(&c));
    }

    #[test]
    fn nonblocking_shared_by_clones() {
        let a = Inner::new(0, EfdFlags::NONE).unwrap();
        let b = a.try_clone().unwrap();
        assert!(!a.is_nonblocking().unwrap() && !b.is_nonblocking().unwrap());
        b.set_nonblocking(true).unwrap();
        assert!(a.is_nonblocking().unwrap());
        assert_eq!(a.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        a.set_nonblocking(false).unwrap();
        assert!(!b.is_nonblocking().unwrap());

        let n = Inner::new(0, nb()).unwrap();
        assert!(n.is_nonblocking().unwrap());
    }

    #[test]
    fn cloexec_flag() {
        let e = Inner::new(0, EfdFlags::CLOEXEC).unwrap();
        for fd in all_fds(&e) {
            assert_ne!(fd_flags(fd).unwrap() & libc::FD_CLOEXEC, 0, "fd {fd}");
        }
        let e = Inner::new(0, EfdFlags::NONE).unwrap();
        for fd in all_fds(&e) {
            assert_eq!(fd_flags(fd).unwrap() & libc::FD_CLOEXEC, 0, "fd {fd}");
        }
        // Like OwnedFd::try_clone, clones are always close-on-exec.
        let c = e.try_clone().unwrap();
        assert_ne!(
            fd_flags(c.as_fd().as_raw_fd()).unwrap() & libc::FD_CLOEXEC,
            0
        );
    }

    #[test]
    fn pipe_is_always_nonblocking() {
        for flags in [EfdFlags::NONE, nb()] {
            let e = Inner::new(0, flags).unwrap();
            for fd in all_fds(&e) {
                // SAFETY: query only.
                let fl = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) }).unwrap();
                assert_ne!(fl & libc::O_NONBLOCK, 0);
            }
        }
    }

    /// Identity of an open fd, to detect that a number was closed even if
    /// another test reused it in the meantime.
    fn identity(fd: RawFd) -> Option<(u64, u64)> {
        // SAFETY: an all-zero stat is a valid value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a live, writable stat buffer.
        let r = unsafe { libc::fstat(fd, &mut st) };
        #[allow(clippy::unnecessary_cast)]
        (r == 0).then_some((st.st_dev as u64, st.st_ino as u64))
    }

    #[test]
    fn drop_closes_all_fds() {
        let a = Inner::new(1, nb()).unwrap();
        let b = a.try_clone().unwrap();
        let mut fds = all_fds(&a).to_vec();
        fds.push(b.as_fd().as_raw_fd());
        let ids: Vec<_> = fds.iter().map(|&f| identity(f).unwrap()).collect();
        drop(a);
        // b still holds the pipe; a's own fd is gone.
        assert!(fd_flags(fds[1]).is_ok() && fd_flags(fds[2]).is_ok());
        drop(b);
        for (fd, id) in fds.into_iter().zip(ids) {
            match fd_flags(fd) {
                Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
                // Reused concurrently by another test: must be a different file.
                Ok(_) => assert_ne!(identity(fd), Some(id), "fd {fd} still open"),
            }
        }
    }

    /// Forks a child that takes the lock, runs `half_update` (leaving the
    /// pipe out of step with the counter) and dies holding the lock. Returns
    /// once the child has exited, unreaped, so the lock is orphaned.
    fn die_mid_update(e: &Inner, half_update: impl FnOnce(&Shared)) -> libc::pid_t {
        // SAFETY: the child only touches the shared page and calls _exit.
        let child = unsafe { libc::fork() };
        if child == 0 {
            std::mem::forget(e.shared.page.lock_raw());
            half_update(&e.shared);
            // SAFETY: async-signal-safe exit in the forked child.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0);
        // Wait for it to exit, without reaping it (WNOWAIT).
        loop {
            // SAFETY: an all-zero siginfo_t is a valid value.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waits on our own child; WNOWAIT keeps it a zombie.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if r == 0 {
                break;
            }
            assert_eq!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::Interrupted
            );
        }
        child
    }

    fn reap(child: libc::pid_t) {
        let mut status = 0;
        // SAFETY: reaps our own child.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    }

    #[test]
    fn recovers_from_holder_dying_before_signalling() {
        let e = Inner::new(0, nb()).unwrap();
        // Counter set, readiness byte never written.
        let child = die_mid_update(&e, |s| s.page.poke(5));
        assert!(!now(&e), "pipe should still be empty");
        assert_eq!(e.read().unwrap(), 5);
        assert!(!now(&e));
        e.write(1).unwrap();
        assert!(now(&e));
        assert_eq!(e.read().unwrap(), 1);
        reap(child);
    }

    #[test]
    fn recovers_from_holder_dying_before_draining() {
        let e = Inner::new(3, nb()).unwrap();
        // Counter cleared, readiness byte left in the pipe.
        let child = die_mid_update(&e, |s| s.page.poke(0));
        assert!(now(&e), "stale byte should still be there");
        // Any operation takes the lock over and resynchronises the pipe.
        e.write(0).unwrap();
        assert!(!now(&e));
        assert_eq!(e.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        reap(child);
    }

    #[test]
    fn stress_keeps_pipe_invariant() {
        let e = Inner::new(0, nb() | EfdFlags::SEMAPHORE).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let h = e.try_clone().unwrap();
                thread::spawn(move || {
                    let mut got = 0u64;
                    for _ in 0..2000 {
                        if i % 2 == 0 {
                            h.write(1).unwrap();
                        } else if h.read().is_ok() {
                            got += 1;
                        }
                    }
                    got
                })
            })
            .collect();
        let consumed: u64 = handles.into_iter().map(|t| t.join().unwrap()).sum();
        let left = e.shared.page.peek();
        assert_eq!(consumed + left, 4 * 2000);
        assert_eq!(now(&e), left > 0);
        assert_eq!(
            pending_bytes(e.shared.pipe_r.as_fd()),
            Some(i32::from(left > 0))
        );
        // Drain and re-check.
        e.set_nonblocking(true).unwrap();
        while e.read().is_ok() {}
        assert!(!now(&e));
        assert_eq!(e.shared.page.peek(), 0);
    }
}

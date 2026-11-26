//! A small, uniform way to hand eventfds to other processes.
//!
//! [`GkProcess`] is a connection to another process. [`GkProcess::send`]
//! hands it an eventfd and [`GkProcess::recv`] receives one, with the same
//! code on every OS. Afterwards both processes share the counter. Channel
//! ends and shared memory travel the same way ([`GkProcess::recv_as`]).
//! Small byte messages (a handshake, a bootstrap) travel on the same
//! connection with [`GkProcess::send_bytes`] / [`GkProcess::recv_bytes`],
//! and [`GkProcess::peer_pid`] names the process at the other end;
//! [`process_alive`] tells whether a process still runs (a zombie counts
//! as exited).
//!
//! | | unix (Linux, macOS, BSDs, ...) | Windows |
//! |---|---|---|
//! | connection | unix stream socket | named pipe |
//! | name | socket path (`$TMPDIR/gkefd-<name>.sock`, or a path containing `/`) | `\\.\pipe\gkefd-<name>` (or a full `\\.\pipe\...` name) |
//! | `send` | `GkEventFd::send_to` (`SCM_RIGHTS`) | records the peer's pid; the peer pulls the handles into itself |
//!
//! Plain pipes can't carry descriptors on unix, so the connection is always
//! a unix socket there.
//!
//! Three ways to get a `GkProcess`:
//!
//! * unrelated processes: one side [`GkListener::bind`]s a name and
//!   [`accept`](GkListener::accept)s, the other [`GkProcess::connect`]s;
//! * a spawned child: the parent calls [`GkProcess::spawn`], the child
//!   [`GkProcess::from_parent`];
//! * unix: from an existing connected `UnixStream` (`From`).
//!
//! ```no_run
//! use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd, GkProcess};
//! use std::process::Command;
//!
//! // Parent
//! let efd = GkEventFd::new(0, EfdFlags::NONE)?;
//! let (mut child, mut peer) = GkProcess::spawn(&mut Command::new("./worker"))?;
//! peer.send(&efd)?;
//! let rung = efd.read()?; // woken by the worker
//! # child.wait()?;
//!
//! // Worker
//! let efd = GkProcess::from_parent()?.recv()?;
//! efd.write(1)?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! The existing per-OS calls (`send_to`/`recv_from`, `export_to`,
//! `export_inheritable`/`import`) are unchanged; this is a thin layer over
//! them.
//!
//! # Byte messages and the connection as a lifeline
//!
//! ```no_run
//! use adaptif_gatotkaca_eventfd::GkProcess;
//!
//! let mut peer = GkProcess::connect("server")?;
//! peer.send_bytes(b"HELLO")?;
//! let mut reply = Vec::new();
//! peer.recv_bytes(&mut reply)?;
//! // unix: the descriptor turns readable (EOF) when the server exits.
//! # #[cfg(unix)]
//! let fd = std::os::fd::AsRawFd::as_raw_fd(&peer);
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io;
use std::process::{Child, Command};

use crate::GkEventFd;

/// Environment variable through which [`GkProcess::spawn`] tells the child
/// how to reach its parent (read by [`GkProcess::from_parent`]).
pub const PARENT_ENV: &str = "GK_EVENTFD_PARENT";

/// Largest byte message [`GkProcess::send_bytes`] sends and
/// [`GkProcess::recv_bytes`] accepts: 64 KiB.
pub const BYTES_MAX: usize = 64 * 1024;

/// Tag of a byte message on the wire (objects carry their own tags).
pub(crate) const BYTES_TAG: [u8; 8] = *b"gkbyt1:b";

fn wrong_kind_bytes() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "not the expected kind of object: a byte message was sent")
}

/// A connection to another process, over which eventfds (and other
/// [`Shareable`] objects) and small byte messages can be sent and
/// received. See the [module docs](self).
#[derive(Debug)]
pub struct GkProcess {
    inner: imp::Process,
    /// C API: a byte message that did not fit the caller's buffer, kept for
    /// the next receive.
    held: Option<Vec<u8>>,
}

/// Accepts [`GkProcess`] connections under a name. See the
/// [module docs](self).
#[derive(Debug)]
pub struct GkListener(imp::Listener);

/// Who may connect to a [`GkListener`]: the second argument of
/// [`GkListener::bind_with`] (since 0.3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ListenerAccess {
    /// What [`GkListener::bind`] does: the OS default. Unix: the socket
    /// file gets the mode the process's umask leaves (typically `0755`, so
    /// only the owner can connect on most systems, but not guaranteed).
    /// Windows: the default security descriptor of the process token: full
    /// control for the creator, SYSTEM and Administrators, read access for
    /// Everyone and anonymous.
    #[default]
    Default,
    /// Only processes running as the current user may connect.
    ///
    /// * Windows: every pipe instance (the first and each later one) is
    ///   created with the protected DACL `D:P(A;;FA;;;<user SID>)`: full
    ///   access for the user of this process's token, nobody else, nothing
    ///   inherited. Other users' `connect` fails with
    ///   [`io::ErrorKind::PermissionDenied`].
    /// * Unix: the socket file is made `0600` right after it is created, and
    ///   [`accept`](GkListener::accept) closes (and skips) any connection
    ///   whose peer's effective user id is not this process's (read with
    ///   `SO_PEERCRED` / `getpeereid` / `getpeerucred`), which also covers
    ///   a connection made before the mode changed, and `root`. Where the
    ///   peer's user can't be read (no such call on the OS) every connection
    ///   is refused. The directory holding the socket is the caller's
    ///   business: for a private one, create it `0700` and pass a path.
    CurrentUser,
}

/// Whether process `pid` is still running (since 0.3.3).
///
/// Answers `false` only on evidence that the process has exited or does not
/// exist, and `true` otherwise, **including when the OS won't tell**
/// (another user's process, a sandbox, an OS without a zombie check): a
/// caller that frees a dead process's resources on `false` never frees a
/// live one's. This is the check the crate itself uses for `peer_gone()`
/// on channel ends, for blocking channel calls noticing a dead peer and for
/// the shims' takeover of a lock whose holder died. It never blocks, never
/// allocates and takes no lock.
///
/// A process that has exited but not been reaped by its parent yet (a
/// zombie) counts as exited:
///
/// * **unix, your own children**: `waitid(P_PID, WEXITED | WNOHANG |
///   WNOWAIT)`. `false` once the child has exited, reaped or not. The query
///   never reaps it (`WNOWAIT`): its exit status stays for your own
///   `wait` / [`Child::wait`](std::process::Child::wait).
/// * **unix, other processes**: `kill(pid, 0)`. `ESRCH` is `false`. A
///   process that exists, or exists but may not be signalled (`EPERM`:
///   another user's), is `true` unless it is a zombie, which is recognised
///   on Linux and Android (`/proc/<pid>/stat` state `X`, or `Z` with at most
///   one thread: a process whose main thread exited while other threads
///   run is alive), macOS and iOS (`proc_pidinfo` no longer finds it),
///   FreeBSD (`kinfo_proc.ki_stat`) and NetBSD (`kinfo_proc2.p_stat`); this
///   works for other users' processes too where the OS shows their state
///   (not under Linux `hidepid`, for example). On OpenBSD, DragonFly and
///   illumos a zombie counts as alive until its parent reaps it.
/// * **Windows**: `OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION)`
///   and a zero-timeout wait. `false` once the process has exited, even
///   while handles keep its object, and when no such process exists
///   (`ERROR_INVALID_PARAMETER`); `true` on any other failure (access
///   denied).
/// * `pid == 0`, and on unix a `pid` above `i32::MAX`, is not looked up:
///   `true`.
///
/// A pid names whatever process carries it at the time of the call. On unix
/// a zombie keeps its pid until it is reaped, after which the pid may be
/// reused; on Windows a pid stays reserved while any handle to the process
/// is open. To ask about one particular process reliably, keep it from
/// being reaped (your own child: ask before you `wait` for it) or hold a
/// handle to it (Windows).
///
/// ```
/// use adaptif_gatotkaca_eventfd::process_alive;
///
/// assert!(process_alive(std::process::id()));
/// ```
///
/// C: `int gk_process_alive(uint32_t pid)` (1 or 0).
pub fn process_alive(pid: u32) -> bool {
    crate::sys::process_alive(pid)
}

impl GkListener {
    /// Listens under `name`: a unix socket path (`$TMPDIR/gkefd-<name>.sock`
    /// unless `name` contains `/`), or the named pipe
    /// `\\.\pipe\gkefd-<name>` on Windows (unless `name` starts with `\\`).
    ///
    /// On unix the listener holds a lock on `<path>.lock` while it lives, so
    /// a socket file left by a dead listener is replaced, and a live listener
    /// under the same name makes this fail with
    /// [`io::ErrorKind::AddrInUse`]. Both files are removed on drop.
    ///
    /// Who may connect is left to the OS defaults; see
    /// [`bind_with`](Self::bind_with) to restrict it.
    pub fn bind(name: &str) -> io::Result<Self> {
        Self::bind_with(name, ListenerAccess::Default)
    }

    /// Like [`bind`](Self::bind), with control over who may connect (see
    /// [`ListenerAccess`]). With [`ListenerAccess::CurrentUser`] no
    /// process of another user can connect at any point: on Windows the
    /// pipe exists only with the restricted DACL (no window between
    /// creating it and securing it), on unix connections of other users are
    /// refused by `accept`.
    ///
    /// ```no_run
    /// use adaptif_gatotkaca_eventfd::process::{GkListener, ListenerAccess};
    ///
    /// let listener = GkListener::bind_with("doorbell", ListenerAccess::CurrentUser)?;
    /// let peer = listener.accept()?; // a process of this user
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn bind_with(name: &str, access: ListenerAccess) -> io::Result<Self> {
        Ok(Self(imp::Listener::bind(name, access)?))
    }

    /// Waits for the next process to [`connect`](GkProcess::connect).
    pub fn accept(&self) -> io::Result<GkProcess> {
        Ok(GkProcess::new(self.0.accept()?))
    }
}

impl GkProcess {
    fn new(inner: imp::Process) -> Self {
        Self { inner, held: None }
    }

    /// Connects to the process listening under `name` (see
    /// [`GkListener::bind`]).
    pub fn connect(name: &str) -> io::Result<Self> {
        Ok(Self::new(imp::Process::connect(name)?))
    }

    /// Spawns `cmd` and returns the child with a connection to it. The child
    /// calls [`GkProcess::from_parent`] (the way to reach the parent is passed
    /// in [`PARENT_ENV`]). The connection is completed the first time this
    /// side sends or receives, which waits for the child to call
    /// `from_parent`. On Windows only that child (by pid) is accepted.
    pub fn spawn(cmd: &mut Command) -> io::Result<(Child, Self)> {
        let (mut process, name) = Self::for_child()?;
        let child = cmd.env(PARENT_ENV, name).spawn()?;
        process.expect_pid(child.id());
        Ok((child, process))
    }

    /// A connection waiting for a child the caller spawns itself (C API):
    /// returns it and the value for the child's [`PARENT_ENV`].
    pub(crate) fn for_child() -> io::Result<(Self, String)> {
        let (process, name) = imp::Process::for_child()?;
        Ok((Self::new(process), name))
    }

    /// Only accept the child with this process id (Windows; no-op on unix,
    /// where the private socket path is the protection).
    pub(crate) fn expect_pid(&mut self, pid: u32) {
        self.inner.expect_pid(pid);
    }

    /// The connected unix socket (connecting a pending child first).
    #[cfg(unix)]
    #[cfg_attr(gk_native, allow(dead_code))]
    pub(crate) fn unix_stream(&mut self) -> io::Result<&std::os::unix::net::UnixStream> {
        self.inner.stream()
    }

    /// In a child started by [`GkProcess::spawn`]: connects to the parent.
    /// Fails with [`io::ErrorKind::NotFound`] if [`PARENT_ENV`] is not set.
    pub fn from_parent() -> io::Result<Self> {
        let name = std::env::var(PARENT_ENV)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "not started by GkProcess::spawn (GK_EVENTFD_PARENT unset)"))?;
        Self::connect(&name)
    }

    /// Hands `obj` (a [`GkEventFd`], [`GkSender`](crate::GkSender),
    /// [`GkReceiver`](crate::GkReceiver) or
    /// [`GkSharedMemory`](crate::GkSharedMemory)) to the other process, which
    /// receives it with [`recv`](Self::recv) (eventfd) or
    /// [`recv_as`](Self::recv_as); both then share it. Only this crate's
    /// traffic may use the connection: objects and
    /// [byte messages](Self::send_bytes), from one thread at a time.
    ///
    /// A channel end sent this way now belongs to the other process: this
    /// process's copy is detached (dropping it doesn't close the channel).
    /// Shared memory is not: both processes keep using it.
    ///
    /// Windows: the other process pulls the handles out of this one, so a
    /// copy of the shared memory section stays open in this `GkProcess` until
    /// it is dropped (one small handle per object per `send`).
    pub fn send<T: Shareable>(&mut self, obj: &T) -> io::Result<()> {
        self.inner.send(obj)
    }

    /// Receives an eventfd sent with [`send`](Self::send). Fails with
    /// [`io::ErrorKind::InvalidData`] for anything else (closing whatever
    /// was received; a byte message is read to its end, so the connection
    /// stays usable) and [`io::ErrorKind::UnexpectedEof`] if the other
    /// process closed the connection.
    pub fn recv(&mut self) -> io::Result<GkEventFd> {
        self.recv_as()
    }

    /// Receives an object of type `T` sent with [`send`](Self::send), e.g.
    /// `peer.recv_as::<GkReceiver>()`. Fails like [`recv`](Self::recv) if
    /// something else was sent.
    pub fn recv_as<T: Shareable>(&mut self) -> io::Result<T> {
        if self.held.take().is_some() {
            return Err(wrong_kind_bytes());
        }
        self.inner.recv()
    }

    /// Sends `msg` as one byte message (empty, or up to [`BYTES_MAX`]
    /// bytes; longer is [`io::ErrorKind::InvalidInput`] and nothing is
    /// sent). The other process receives it with
    /// [`recv_bytes`](Self::recv_bytes). Byte messages and objects
    /// ([`send`](Self::send)) may be mixed on one connection and arrive in
    /// the order they were sent, but the receiver must expect the right
    /// kind each time.
    ///
    /// Frame: an 8-byte tag, the length as a little-endian `u32`, the bytes
    /// (unix socket, no descriptors; on Windows inside the named pipe's
    /// length-prefixed frame). Blocks until the bytes are written, which
    /// may wait for the peer to read. A connection from
    /// [`spawn`](Self::spawn) is completed first (waits for the child).
    ///
    /// Don't call this from several threads on clones of the same
    /// connection, or interleave it with object sends from another thread:
    /// frames would mix.
    pub fn send_bytes(&mut self, msg: &[u8]) -> io::Result<()> {
        if msg.len() > BYTES_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "byte message longer than BYTES_MAX (64 KiB)"));
        }
        self.inner.send_bytes(msg)
    }

    /// Receives a byte message sent with [`send_bytes`](Self::send_bytes),
    /// replacing the contents of `buf`; returns its length. Blocks until it
    /// arrives.
    ///
    /// Fails with [`io::ErrorKind::InvalidData`] if an object was sent
    /// instead (whatever it carried is closed; the connection stays usable)
    /// or the frame is malformed, and [`io::ErrorKind::UnexpectedEof`] if the
    /// other process closed the connection or exited.
    pub fn recv_bytes(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        if let Some(held) = self.held.take() {
            buf.clear();
            buf.extend_from_slice(&held);
            return Ok(buf.len());
        }
        self.inner.recv_bytes(buf)?;
        Ok(buf.len())
    }

    /// C API: receives a byte message into `out`. One that doesn't fit is
    /// kept for the next call and reported as `InvalidInput`.
    #[cfg_attr(not(feature = "ffi"), allow(dead_code))]
    pub(crate) fn recv_bytes_into(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let msg = match self.held.take() {
            Some(msg) => msg,
            None => {
                let mut msg = Vec::new();
                self.inner.recv_bytes(&mut msg)?;
                msg
            }
        };
        if msg.len() > out.len() {
            self.held = Some(msg);
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "buffer smaller than the byte message"));
        }
        out[..msg.len()].copy_from_slice(&msg);
        Ok(msg.len())
    }

    /// The id of the process at the other end, or `None` if it is not
    /// known: not connected yet (a [`spawn`](Self::spawn) whose child hasn't
    /// been reached), or the OS can't tell.
    ///
    /// | OS | source |
    /// |---|---|
    /// | Linux, Android | `SO_PEERCRED` |
    /// | macOS, iOS | `LOCAL_PEERPID` |
    /// | FreeBSD 13+ | `LOCAL_PEERCRED` (`xucred.cr_pid`) |
    /// | NetBSD | `LOCAL_PEEREID` |
    /// | OpenBSD | `SO_PEERCRED` |
    /// | illumos | `getpeerucred` |
    /// | DragonFly, other unix | `None` |
    /// | Windows | the named pipe's client / server process id |
    ///
    /// On unix it is the process that made the connection (`connect`) or
    /// listened for it (`bind`), as the kernel recorded it when the
    /// connection was made; it is read once, when this `GkProcess` is
    /// connected, and kept (so it stays known after the peer exits). A
    /// socket handed to another process later still names the original one.
    pub fn peer_pid(&self) -> Option<u32> {
        self.inner.peer_pid()
    }

    /// Windows: the other process's id, once connected. Same as
    /// [`peer_pid`](Self::peer_pid), which exists on every OS.
    #[cfg(windows)]
    pub fn pid(&self) -> Option<u32> {
        self.peer_pid()
    }
}

/// Unix: the connection's socket, for a poller (`poll`, `epoll`, `kqueue`,
/// tokio `AsyncFd`, asyncio `add_reader`) to watch for **readable**.
///
/// It turns readable when a message arrives, and stays readable (`recv`
/// returns 0, EOF) once the other process closed the connection or exited:
/// that makes the connection a lifeline. Read and write it only through
/// this crate's calls; a raw `read` would take part of a frame.
///
/// For a [`spawn`](GkProcess::spawn) (or C `gk_process_for_child`)
/// connection whose child has not been reached yet, this is the private
/// listening socket instead, which turns readable when the child connects
/// and is closed once the connection completes. Exchange a message first
/// (any send or receive completes it) before registering the descriptor.
#[cfg(unix)]
impl std::os::fd::AsFd for GkProcess {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.inner.fd()
    }
}

/// Unix: see [`AsFd`](std::os::fd::AsFd) above.
#[cfg(unix)]
impl std::os::fd::AsRawFd for GkProcess {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.inner.fd())
    }
}

/// Objects [`GkProcess`] can hand to other processes: [`GkEventFd`],
/// [`GkSender`](crate::GkSender), [`GkReceiver`](crate::GkReceiver) and
/// [`GkSharedMemory`](crate::GkSharedMemory).
/// Sealed: it can't be implemented outside this crate.
pub trait Shareable: private::Sealed {}

pub(crate) mod private {
    use std::io;

    /// How an object travels: as descriptors over a unix socket, or on
    /// Windows as an offer the receiving process pulls into itself.
    pub trait Sealed: Sized {
        #[cfg(unix)]
        fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()>;
        #[cfg(unix)]
        fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self>;
        #[cfg(windows)]
        fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)>;
        #[cfg(windows)]
        fn take(token: &str, pid: u32) -> io::Result<Self>;
    }
}

impl Shareable for GkEventFd {}

impl private::Sealed for GkEventFd {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.inner.send_to(sock)
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        Ok(Self { inner: crate::sys::Inner::recv_from(sock)? })
    }
    #[cfg(windows)]
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        let (token, keep) = self.inner.offer_to(pid)?;
        Ok((token, vec![keep]))
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        // Another kind of object (channel end, shared memory): InvalidData,
        // as on unix.
        if !token.trim_start().starts_with("gkefd1:") {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not the expected kind of object"));
        }
        Ok(Self { inner: crate::sys::Inner::take_offer(token, pid)? })
    }
}

/// Unix: wraps a connected unix socket.
#[cfg(unix)]
impl From<std::os::unix::net::UnixStream> for GkProcess {
    fn from(stream: std::os::unix::net::UnixStream) -> Self {
        Self::new(imp::Process::ready(stream))
    }
}

/// A name nobody else uses, for `spawn`.
fn unique_name() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("p{:x}-{:x}-{:x}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed), nanos)
}

// ---------------------------------------------------------------------------
// unix: unix stream sockets
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::private::Sealed;
    use super::{ListenerAccess, unique_name};
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    fn path_for(name: &str) -> PathBuf {
        if name.contains('/') { PathBuf::from(name) } else { std::env::temp_dir().join(format!("gkefd-{name}.sock")) }
    }

    #[derive(Debug)]
    pub(super) struct Listener {
        sock: UnixListener,
        path: PathBuf,
        lock_path: PathBuf,
        /// Holds a write lock on `lock_path` for the listener's lifetime.
        _lock: std::fs::File,
        /// `ListenerAccess::CurrentUser`: the only effective user id
        /// `accept` lets through.
        only_uid: Option<libc::uid_t>,
    }

    /// Takes the lock that marks `path` as owned by a live listener. The
    /// kernel drops it when the process exits, so a socket file left by a
    /// dead listener is recognised without connecting to it (a probe
    /// connection would land in a live listener's accept queue).
    fn lock(lock_path: &PathBuf) -> io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(lock_path)?;
        // flock locks belong to the open file, so a second bind in the same
        // process is refused too (fcntl record locks would allow it).
        // SAFETY: plain flock on our open file.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let err = io::Error::last_os_error();
            return Err(if err.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(io::ErrorKind::AddrInUse, "another listener is bound to this name")
            } else {
                err
            });
        }
        Ok(file)
    }

    impl Listener {
        pub(super) fn bind(name: &str, access: ListenerAccess) -> io::Result<Self> {
            let path = path_for(name);
            let mut lock_path = path.clone().into_os_string();
            lock_path.push(".lock");
            let lock_path = PathBuf::from(lock_path);
            let lock = lock(&lock_path)?;
            // We own the name: any socket file left there is stale.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let sock = UnixListener::bind(&path)?;
            let mut listener = Self { sock, path, lock_path, _lock: lock, only_uid: None };
            if access == ListenerAccess::CurrentUser {
                use std::os::unix::fs::PermissionsExt;
                // On failure dropping `listener` removes both files.
                std::fs::set_permissions(&listener.path, std::fs::Permissions::from_mode(0o600))?;
                // SAFETY: geteuid has no preconditions.
                listener.only_uid = Some(unsafe { libc::geteuid() });
            }
            Ok(listener)
        }

        pub(super) fn accept(&self) -> io::Result<Process> {
            loop {
                let stream = self.sock.accept()?.0;
                if let Some(uid) = self.only_uid {
                    if crate::sys::unix_share::peer_euid(std::os::fd::AsFd::as_fd(&stream)) != Some(uid) {
                        // Another user (or unknown): closed, wait for the next.
                        continue;
                    }
                }
                return Ok(Process::ready(stream));
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }

    #[derive(Debug)]
    pub(super) enum Process {
        Ready {
            stream: UnixStream,
            /// The peer's pid, read when the connection was made (some
            /// OSes, e.g. macOS, forget it once the peer has gone).
            pid: Option<u32>,
        },
        /// `spawn`ed: waiting for the child to connect.
        Pending(Listener),
    }

    impl Process {
        pub(super) fn ready(stream: UnixStream) -> Self {
            let pid = crate::sys::unix_share::peer_pid(std::os::fd::AsFd::as_fd(&stream));
            Self::Ready { stream, pid }
        }

        pub(super) fn connect(name: &str) -> io::Result<Self> {
            Ok(Self::ready(UnixStream::connect(path_for(name))?))
        }

        pub(super) fn for_child() -> io::Result<(Self, String)> {
            let listener = Listener::bind(&unique_name(), ListenerAccess::Default)?;
            let name = listener.path.to_string_lossy().into_owned();
            Ok((Self::Pending(listener), name))
        }

        pub(super) fn expect_pid(&mut self, _pid: u32) {}

        pub(super) fn stream(&mut self) -> io::Result<&UnixStream> {
            if let Self::Pending(listener) = self {
                let ready = listener.accept()?;
                // Dropping the listener removes the socket file.
                *self = ready;
            }
            match self {
                Self::Ready { stream, .. } => Ok(stream),
                Self::Pending(_) => unreachable!(),
            }
        }

        pub(super) fn send<T: Sealed>(&mut self, obj: &T) -> io::Result<()> {
            obj.send_over(std::os::fd::AsFd::as_fd(self.stream()?))
        }

        pub(super) fn recv<T: Sealed>(&mut self) -> io::Result<T> {
            T::recv_over(std::os::fd::AsFd::as_fd(self.stream()?))
        }

        pub(super) fn send_bytes(&mut self, msg: &[u8]) -> io::Result<()> {
            crate::sys::unix_share::send_bytes(std::os::fd::AsFd::as_fd(self.stream()?), msg)
        }

        pub(super) fn recv_bytes(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
            crate::sys::unix_share::recv_bytes(std::os::fd::AsFd::as_fd(self.stream()?), buf)
        }

        pub(super) fn peer_pid(&self) -> Option<u32> {
            match self {
                Self::Ready { pid, .. } => *pid,
                Self::Pending(_) => None,
            }
        }

        /// The connected socket, or the listening one while pending.
        pub(super) fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
            match self {
                Self::Ready { stream, .. } => std::os::fd::AsFd::as_fd(stream),
                Self::Pending(listener) => std::os::fd::AsFd::as_fd(&listener.sock),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Listener, ListenerAccess, path_for};
        use std::io;
        use std::os::unix::net::UnixStream;

        /// `CurrentUser`: a connection from another effective user is closed
        /// by `accept`, which goes on waiting. Simulated by expecting another
        /// uid.
        #[test]
        fn current_user_listener_drops_other_users() {
            let name = format!("unit-cu-{}", std::process::id());
            let mut listener = Listener::bind(&name, ListenerAccess::CurrentUser).unwrap();
            // SAFETY: geteuid has no preconditions.
            let me = unsafe { libc::geteuid() };
            assert_eq!(listener.only_uid, Some(me));
            listener.only_uid = Some(me.wrapping_add(1));
            listener.sock.set_nonblocking(true).unwrap();
            let mut client = UnixStream::connect(path_for(&name)).unwrap();
            // The queued connection is refused, then nothing is left.
            assert_eq!(listener.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
            let mut buf = [0u8; 1];
            assert_eq!(io::Read::read(&mut client, &mut buf).unwrap(), 0, "the refused connection must be closed");
            // The right user gets through.
            listener.only_uid = Some(me);
            let _client = UnixStream::connect(path_for(&name)).unwrap();
            assert!(listener.accept().is_ok());
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: named pipes, recording the peer's pid
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::private::Sealed;
    use super::{BYTES_MAX, BYTES_TAG, ListenerAccess, unique_name};
    use crate::compat::TOKEN_MAX;
    use std::fs::File;

    /// Upper bound for one offer on the pipe. A single eventfd offer is under
    /// `TOKEN_MAX`, but a channel end carries two eventfds plus a section
    /// (about 5 KB), so allow a few of them.
    const OFFER_MAX: usize = 4 * TOKEN_MAX;
    /// Largest frame on the pipe: an offer, or a byte message's tag and bytes.
    const FRAME_MAX: usize = if OFFER_MAX > 8 + BYTES_MAX { OFFER_MAX } else { 8 + BYTES_MAX };
    use std::io::{self, Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn pipe_name(name: &str) -> String {
        if name.starts_with(r"\\") { name.to_owned() } else { format!(r"\\.\pipe\gkefd-{name}") }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A new, unconnected instance of the pipe `name`, with the security
    /// descriptor `sd` (a self-relative one, see `current_user_only`) or the
    /// default one.
    fn instance(name: &[u16], first: bool, sd: Option<&[u64]>) -> io::Result<OwnedHandle> {
        let first_instance = first;
        let first = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let attrs = sd.map(|sd| SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            // Only read by the call; `sd` outlives it.
            lpSecurityDescriptor: sd.as_ptr().cast_mut().cast(),
            bInheritHandle: 0,
        });
        let attrs_ptr = attrs.as_ref().map_or(std::ptr::null(), |a| a as *const SECURITY_ATTRIBUTES);
        // SAFETY: `name` is NUL-terminated; `attrs_ptr` is null (default
        // security) or points to live attributes whose descriptor is a valid
        // self-relative one; local clients only.
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | first,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                attrs_ptr,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            // With FIRST_PIPE_INSTANCE, a taken name reports ACCESS_DENIED
            // (5); surface it as the "address in use" it is.
            if first_instance && e.raw_os_error() == Some(5) {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, e));
            }
            return Err(e);
        }
        // SAFETY: a fresh handle we own.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// The current user's SID as a string (`S-1-5-21-...`), from this
    /// process's token.
    fn current_user_sid() -> io::Result<String> {
        let mut token = std::ptr::null_mut();
        // SAFETY: the current process's pseudo-handle; `token` receives a
        // handle we own.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle we own.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut len = 0u32;
        // SAFETY: a size query (no buffer); fails with
        // ERROR_INSUFFICIENT_BUFFER and sets `len`.
        unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64 words: aligned for `TOKEN_USER`.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` writable bytes.
        if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: filled by the call above with a `TOKEN_USER` (and the SID
        // it points to, inside `buf`).
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        let mut s = std::ptr::null_mut();
        // SAFETY: a valid SID; `s` receives a `LocalAlloc`ed wide string,
        // freed below.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut s) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `s` is NUL-terminated (the conversion succeeded); read up
        // to the NUL.
        let sid = unsafe {
            let n = (0..).take_while(|&i| *s.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
        };
        // SAFETY: allocated by ConvertSidToStringSidW, freed once.
        unsafe { LocalFree(s.cast()) };
        Ok(sid)
    }

    /// A self-relative security descriptor with the protected DACL
    /// `D:P(A;;FA;;;<user SID>)` (full access for the current user only),
    /// copied into words we own (self-relative descriptors hold no
    /// pointers, so the copy is valid wherever it lives).
    fn current_user_only() -> io::Result<Vec<u64>> {
        let sddl = wide(&format!("D:P(A;;FA;;;{})", current_user_sid()?));
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: `sddl` is NUL-terminated; `sd` receives a `LocalAlloc`ed
        // self-relative descriptor of `len` bytes, freed below.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut words = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `sd` holds `len` readable bytes and `words` at least `len`
        // writable ones; they don't overlap.
        unsafe { std::ptr::copy_nonoverlapping(sd.cast::<u8>(), words.as_mut_ptr().cast::<u8>(), len as usize) };
        // SAFETY: allocated by the conversion above, freed once.
        unsafe { LocalFree(sd) };
        Ok(words)
    }

    #[derive(Debug)]
    pub(super) struct Listener {
        name: Vec<u16>,
        /// `ListenerAccess::CurrentUser`: the self-relative security
        /// descriptor every instance is created with.
        sd: Option<Vec<u64>>,
        /// The instance the next client will connect to.
        next: Mutex<OwnedHandle>,
    }

    impl Listener {
        pub(super) fn bind(name: &str, access: ListenerAccess) -> io::Result<Self> {
            let name = wide(&pipe_name(name));
            let sd = match access {
                ListenerAccess::Default => None,
                ListenerAccess::CurrentUser => Some(current_user_only()?),
            };
            // FIRST_PIPE_INSTANCE: fail if someone else already owns the name.
            let next = Mutex::new(instance(&name, true, sd.as_deref())?);
            Ok(Self { name, sd, next })
        }

        pub(super) fn accept(&self) -> io::Result<Process> {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: a valid pipe instance; synchronous (no OVERLAPPED).
            if unsafe { ConnectNamedPipe(next.as_raw_handle(), std::ptr::null_mut()) } == 0
                // SAFETY: no preconditions.
                && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED
            {
                return Err(io::Error::last_os_error());
            }
            let mut pid = 0;
            // SAFETY: a connected pipe instance; `pid` is a valid out pointer.
            if unsafe { GetNamedPipeClientProcessId(next.as_raw_handle(), &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let conn = std::mem::replace(&mut *next, instance(&self.name, false, self.sd.as_deref())?);
            Ok(Process::ready(conn, pid))
        }
    }

    #[derive(Debug)]
    pub(super) struct Process {
        state: State,
        /// Section copies our peer may still pull (see `GkProcess::send`).
        offered: Vec<OwnedHandle>,
    }

    #[derive(Debug)]
    enum State {
        Ready {
            pipe: File,
            pid: u32,
        },
        /// Waiting for a spawned child to connect (only that pid, if known).
        Pending {
            listener: Listener,
            child: Option<u32>,
        },
    }

    impl Process {
        fn ready(pipe: OwnedHandle, pid: u32) -> Self {
            Self { state: State::Ready { pipe: File::from(pipe), pid }, offered: Vec::new() }
        }

        pub(super) fn peer_pid(&self) -> Option<u32> {
            match self.state {
                State::Ready { pid, .. } => Some(pid),
                State::Pending { .. } => None,
            }
        }

        pub(super) fn connect(name: &str) -> io::Result<Self> {
            let name = wide(&pipe_name(name));
            let pipe = loop {
                // SAFETY: `name` is NUL-terminated; plain synchronous open.
                let h = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_NONE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if h != INVALID_HANDLE_VALUE {
                    // SAFETY: a fresh handle we own.
                    break unsafe { OwnedHandle::from_raw_handle(h) };
                }
                // SAFETY: no preconditions.
                if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
                    return Err(io::Error::last_os_error());
                }
                // All instances busy: wait for one (up to 5 s), then retry.
                // SAFETY: `name` is NUL-terminated.
                if unsafe { WaitNamedPipeW(name.as_ptr(), 5_000) } == 0 {
                    return Err(io::Error::last_os_error());
                }
            };
            let mut pid = 0;
            // SAFETY: a connected pipe; `pid` is a valid out pointer.
            if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self::ready(pipe, pid))
        }

        pub(super) fn for_child() -> io::Result<(Self, String)> {
            let name = pipe_name(&unique_name());
            let listener = Listener::bind(&name, ListenerAccess::Default)?;
            let state = State::Pending { listener, child: None };
            Ok((Self { state, offered: Vec::new() }, name))
        }

        pub(super) fn expect_pid(&mut self, pid: u32) {
            if let State::Pending { child, .. } = &mut self.state {
                *child = Some(pid);
            }
        }

        fn connection(&mut self) -> io::Result<(&mut File, u32)> {
            if let State::Pending { listener, child } = &self.state {
                // Only our child may connect; anyone else is dropped.
                let ready = loop {
                    let p = listener.accept()?;
                    if child.is_none() || p.peer_pid() == *child {
                        break p.state;
                    }
                };
                self.state = ready;
            }
            match &mut self.state {
                State::Ready { pipe, pid } => Ok((pipe, *pid)),
                State::Pending { .. } => unreachable!(),
            }
        }

        pub(super) fn send<T: Sealed>(&mut self, obj: &T) -> io::Result<()> {
            let (_, pid) = self.connection()?;
            let (token, keep) = obj.offer(pid)?;
            self.offered.extend(keep);
            let (pipe, _) = self.connection()?;
            pipe.write_all(&(token.len() as u32).to_le_bytes())?;
            pipe.write_all(token.as_bytes())?;
            pipe.flush()
        }

        /// Reads one frame: a little-endian `u32` length and that many
        /// bytes (an offer's token, or a byte message's tag and bytes).
        fn read_frame(pipe: &mut File) -> io::Result<Vec<u8>> {
            let mut len = [0u8; 4];
            pipe.read_exact(&mut len)?;
            let len = u32::from_le_bytes(len) as usize;
            if len > FRAME_MAX {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "eventfd message too long"));
            }
            let mut frame = vec![0u8; len];
            pipe.read_exact(&mut frame)?;
            Ok(frame)
        }

        pub(super) fn recv<T: Sealed>(&mut self) -> io::Result<T> {
            let (pipe, pid) = self.connection()?;
            let frame = Self::read_frame(pipe)?;
            if frame.starts_with(&BYTES_TAG) {
                return Err(super::wrong_kind_bytes());
            }
            if frame.len() > OFFER_MAX {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "eventfd message too long"));
            }
            let token = String::from_utf8(frame).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "eventfd message"))?;
            T::take(&token, pid)
        }

        pub(super) fn send_bytes(&mut self, msg: &[u8]) -> io::Result<()> {
            let (pipe, _) = self.connection()?;
            let len = BYTES_TAG.len() + msg.len();
            let mut frame = Vec::with_capacity(4 + len);
            frame.extend_from_slice(&(len as u32).to_le_bytes());
            frame.extend_from_slice(&BYTES_TAG);
            frame.extend_from_slice(msg);
            pipe.write_all(&frame)?;
            pipe.flush()
        }

        pub(super) fn recv_bytes(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
            let (pipe, _) = self.connection()?;
            let frame = Self::read_frame(pipe)?;
            // An offer is just text naming handles in the sender, which keeps
            // them; ignoring it leaks nothing here.
            let Some(msg) = frame.strip_prefix(&BYTES_TAG) else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a byte message: an object was sent"));
            };
            buf.clear();
            buf.extend_from_slice(msg);
            Ok(())
        }
    }
}

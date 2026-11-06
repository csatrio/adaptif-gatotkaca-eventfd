//! A small, uniform way to hand eventfds to other processes.
//!
//! [`GkProcess`] is a connection to another process. [`GkProcess::send`]
//! hands it an eventfd and [`GkProcess::recv`] receives one, with the same
//! code on every OS. Afterwards both processes share the counter.
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

use std::io;
use std::process::{Child, Command};

use crate::GkEventFd;

/// Environment variable through which [`GkProcess::spawn`] tells the child
/// how to reach its parent (read by [`GkProcess::from_parent`]).
pub const PARENT_ENV: &str = "GK_EVENTFD_PARENT";

/// A connection to another process, over which eventfds can be sent and
/// received. See the [module docs](self).
#[derive(Debug)]
pub struct GkProcess(imp::Process);

/// Accepts [`GkProcess`] connections under a name. See the
/// [module docs](self).
#[derive(Debug)]
pub struct GkListener(imp::Listener);

impl GkListener {
    /// Listens under `name`: a unix socket path (`$TMPDIR/gkefd-<name>.sock`
    /// unless `name` contains `/`), or the named pipe
    /// `\\.\pipe\gkefd-<name>` on Windows (unless `name` starts with `\\`).
    ///
    /// On unix a stale socket file left by a dead listener is replaced; an
    /// active listener under the same name makes this fail with
    /// [`io::ErrorKind::AddrInUse`]. The socket file is removed on drop.
    pub fn bind(name: &str) -> io::Result<Self> {
        Ok(Self(imp::Listener::bind(name)?))
    }

    /// Waits for the next process to [`connect`](GkProcess::connect).
    pub fn accept(&self) -> io::Result<GkProcess> {
        Ok(GkProcess(self.0.accept()?))
    }
}

impl GkProcess {
    /// Connects to the process listening under `name` (see
    /// [`GkListener::bind`]).
    pub fn connect(name: &str) -> io::Result<Self> {
        Ok(Self(imp::Process::connect(name)?))
    }

    /// Spawns `cmd` and returns the child with a connection to it. The child
    /// calls [`GkProcess::from_parent`] (the way to reach the parent is passed
    /// in [`PARENT_ENV`]). The connection is completed the first time this
    /// side sends or receives, which waits for the child to call
    /// `from_parent`. On Windows only that child (by pid) is accepted.
    pub fn spawn(cmd: &mut Command) -> io::Result<(Child, Self)> {
        let (child, process) = imp::Process::spawn(cmd)?;
        Ok((child, Self(process)))
    }

    /// In a child started by [`GkProcess::spawn`]: connects to the parent.
    /// Fails with [`io::ErrorKind::NotFound`] if [`PARENT_ENV`] is not set.
    pub fn from_parent() -> io::Result<Self> {
        let name = std::env::var(PARENT_ENV).map_err(|_| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "not started by GkProcess::spawn (GK_EVENTFD_PARENT unset)",
            )
        })?;
        Self::connect(&name)
    }

    /// Hands `efd` to the other process, which receives it with
    /// [`recv`](Self::recv); both then share the counter. Don't interleave
    /// other traffic on the same connection.
    ///
    /// Windows: the other process pulls the handles out of this one, so a
    /// copy of the eventfd's section stays open in this `GkProcess` until it
    /// is dropped (one small handle per `send`).
    pub fn send(&mut self, efd: &GkEventFd) -> io::Result<()> {
        self.0.send(efd)
    }

    /// Receives an eventfd sent with [`send`](Self::send). Fails with
    /// [`io::ErrorKind::InvalidData`] for anything else (closing whatever
    /// was received) and [`io::ErrorKind::UnexpectedEof`] if the other process
    /// closed the connection.
    pub fn recv(&mut self) -> io::Result<GkEventFd> {
        self.0.recv()
    }

    /// Windows: the other process's id, once connected.
    #[cfg(windows)]
    pub fn pid(&self) -> Option<u32> {
        self.0.pid()
    }
}

/// Unix: wraps a connected unix socket.
#[cfg(unix)]
impl From<std::os::unix::net::UnixStream> for GkProcess {
    fn from(stream: std::os::unix::net::UnixStream) -> Self {
        Self(imp::Process::Ready(stream))
    }
}

/// A name nobody else uses, for `spawn`.
fn unique_name() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "p{:x}-{:x}-{:x}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

// ---------------------------------------------------------------------------
// unix: unix stream sockets
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::{PARENT_ENV, unique_name};
    use crate::GkEventFd;
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::process::{Child, Command};

    fn path_for(name: &str) -> PathBuf {
        if name.contains('/') {
            PathBuf::from(name)
        } else {
            std::env::temp_dir().join(format!("gkefd-{name}.sock"))
        }
    }

    #[derive(Debug)]
    pub(super) struct Listener {
        sock: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        pub(super) fn bind(name: &str) -> io::Result<Self> {
            let path = path_for(name);
            let sock = match UnixListener::bind(&path) {
                Ok(sock) => sock,
                // A leftover file: replace it unless someone is listening.
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    if UnixStream::connect(&path).is_ok() {
                        return Err(e);
                    }
                    std::fs::remove_file(&path)?;
                    UnixListener::bind(&path)?
                }
                Err(e) => return Err(e),
            };
            Ok(Self { sock, path })
        }

        pub(super) fn accept(&self) -> io::Result<Process> {
            Ok(Process::Ready(self.sock.accept()?.0))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[derive(Debug)]
    pub(super) enum Process {
        Ready(UnixStream),
        /// `spawn`ed: waiting for the child to connect.
        Pending(Listener),
    }

    impl Process {
        pub(super) fn connect(name: &str) -> io::Result<Self> {
            Ok(Self::Ready(UnixStream::connect(path_for(name))?))
        }

        pub(super) fn spawn(cmd: &mut Command) -> io::Result<(Child, Self)> {
            let listener = Listener::bind(&unique_name())?;
            let child = cmd.env(PARENT_ENV, &listener.path).spawn()?;
            Ok((child, Self::Pending(listener)))
        }

        fn stream(&mut self) -> io::Result<&UnixStream> {
            if let Self::Pending(listener) = self {
                let ready = listener.accept()?;
                // Dropping the listener removes the socket file.
                *self = ready;
            }
            match self {
                Self::Ready(stream) => Ok(stream),
                Self::Pending(_) => unreachable!(),
            }
        }

        pub(super) fn send(&mut self, efd: &GkEventFd) -> io::Result<()> {
            efd.send_to(self.stream()?)
        }

        pub(super) fn recv(&mut self) -> io::Result<GkEventFd> {
            GkEventFd::recv_from(self.stream()?)
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: named pipes, recording the peer's pid
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{PARENT_ENV, unique_name};
    use crate::GkEventFd;
    use crate::compat::TOKEN_MAX;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::{Child, Command};
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
        GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };

    fn pipe_name(name: &str) -> String {
        if name.starts_with(r"\\") {
            name.to_owned()
        } else {
            format!(r"\\.\pipe\gkefd-{name}")
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A new, unconnected instance of the pipe `name`.
    fn instance(name: &[u16], first: bool) -> io::Result<OwnedHandle> {
        let first = if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
        // SAFETY: `name` is NUL-terminated; default security (this user),
        // local clients only.
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | first,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle we own.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    #[derive(Debug)]
    pub(super) struct Listener {
        name: Vec<u16>,
        /// The instance the next client will connect to.
        next: Mutex<OwnedHandle>,
    }

    impl Listener {
        pub(super) fn bind(name: &str) -> io::Result<Self> {
            let name = wide(&pipe_name(name));
            // FIRST_PIPE_INSTANCE: fail if someone else already owns the name.
            let next = Mutex::new(instance(&name, true)?);
            Ok(Self { name, next })
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
            let conn = std::mem::replace(&mut *next, instance(&self.name, false)?);
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
        /// `spawn`ed: waiting for that child (by pid) to connect.
        Pending {
            listener: Listener,
            child: u32,
        },
    }

    impl Process {
        fn ready(pipe: OwnedHandle, pid: u32) -> Self {
            Self {
                state: State::Ready {
                    pipe: File::from(pipe),
                    pid,
                },
                offered: Vec::new(),
            }
        }

        pub(super) fn pid(&self) -> Option<u32> {
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

        pub(super) fn spawn(cmd: &mut Command) -> io::Result<(Child, Self)> {
            let name = pipe_name(&unique_name());
            let listener = Listener::bind(&name)?;
            let child = cmd.env(PARENT_ENV, &name).spawn()?;
            let state = State::Pending {
                listener,
                child: child.id(),
            };
            Ok((
                child,
                Self {
                    state,
                    offered: Vec::new(),
                },
            ))
        }

        fn connection(&mut self) -> io::Result<(&mut File, u32)> {
            if let State::Pending { listener, child } = &self.state {
                // Only our child may connect; anyone else is dropped.
                let ready = loop {
                    let p = listener.accept()?;
                    if p.pid() == Some(*child) {
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

        pub(super) fn send(&mut self, efd: &GkEventFd) -> io::Result<()> {
            let (_, pid) = self.connection()?;
            let (token, keep) = efd.inner.offer_to(pid)?;
            self.offered.push(keep);
            let (pipe, _) = self.connection()?;
            pipe.write_all(&(token.len() as u32).to_le_bytes())?;
            pipe.write_all(token.as_bytes())?;
            pipe.flush()
        }

        pub(super) fn recv(&mut self) -> io::Result<GkEventFd> {
            let (pipe, pid) = self.connection()?;
            let mut len = [0u8; 4];
            pipe.read_exact(&mut len)?;
            let len = u32::from_le_bytes(len) as usize;
            if len > TOKEN_MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "eventfd message too long",
                ));
            }
            let mut token = vec![0u8; len];
            pipe.read_exact(&mut token)?;
            let token = String::from_utf8(token)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "eventfd message"))?;
            Ok(GkEventFd {
                inner: crate::sys::Inner::take_offer(&token, pid)?,
            })
        }
    }
}

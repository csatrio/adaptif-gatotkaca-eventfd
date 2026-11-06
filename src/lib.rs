//! # adaptif-gatotkaca-eventfd
//!
//! A cross-platform `eventfd(2)`.
//!
//! [`GkEventFd`] behaves like a Linux eventfd on every supported OS:
//!
//! | Platform | Backend | Pollable object |
//! |---|---|---|
//! | Linux, Android, FreeBSD 13+, NetBSD 10+, illumos | the kernel `eventfd(2)` (no shim) | the eventfd itself |
//! | macOS, iOS, OpenBSD, DragonFly | counter in shared memory + pipe | read end of the pipe (`AsFd`) |
//! | Windows | counter in a shareable section + loopback socket pair | read socket (`AsSocket`) |
//!
//! On the shim platforms the pollable object is readable exactly while the
//! counter is non-zero, so it works with `poll`/`kqueue`/`WSAPoll`/`select`
//! and mio. Always read and write the counter through this crate, never with
//! raw `read(2)`/`recv`.
//!
//! The [`compat`] module offers the `<sys/eventfd.h>` functions with C
//! semantics, and the `ffi` feature exports them as a C ABI.
//!
//! ```
//! use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
//!
//! let efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
//! efd.write(2)?;
//! efd.write(3)?;
//! assert_eq!(efd.read()?, 5);
//! assert_eq!(efd.read().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
//! # Ok::<(), std::io::Error>(())
//! ```

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use core::ffi::c_int;
use std::fmt;
use std::io;

mod counter;
mod error;
mod flags;
mod sys;

pub mod compat;
#[cfg(feature = "ffi")]
mod ffi;
pub mod process;

pub use flags::{EFD_CLOEXEC, EFD_MAX, EFD_NONBLOCK, EFD_SEMAPHORE, EfdFlags, eventfd_t};
pub use process::{GkListener, GkProcess};

/// A cross-platform eventfd. See the [crate docs](crate).
///
/// `GkEventFd` is `Send + Sync`; share it with `Arc` or duplicate it with
/// [`try_clone`](Self::try_clone). Dropping it closes the descriptor.
pub struct GkEventFd {
    inner: sys::Inner,
}

impl GkEventFd {
    /// Creates an eventfd, like `eventfd(initval, flags)`.
    pub fn new(initval: u32, flags: EfdFlags) -> io::Result<Self> {
        Ok(Self {
            inner: sys::Inner::new(initval, flags)?,
        })
    }

    /// Like [`new`](Self::new) but takes raw `EFD_*` bits.
    /// Unknown bits fail with `EINVAL` ([`io::ErrorKind::InvalidInput`]).
    pub fn with_raw_flags(initval: u32, flags: c_int) -> io::Result<Self> {
        let flags = EfdFlags::from_bits(flags).ok_or_else(error::invalid_input)?;
        Self::new(initval, flags)
    }

    /// Reads the counter.
    ///
    /// * Normal mode: returns the value and resets it to 0.
    /// * [`EfdFlags::SEMAPHORE`]: returns 1 and decrements the value.
    /// * Counter is 0: blocks, or fails with [`io::ErrorKind::WouldBlock`] if non-blocking.
    pub fn read(&self) -> io::Result<u64> {
        self.inner.read()
    }

    /// Adds `value` to the counter.
    ///
    /// Fails with [`io::ErrorKind::InvalidInput`] for `u64::MAX`. If the sum
    /// would exceed [`EFD_MAX`] it blocks until a read, or fails with
    /// [`io::ErrorKind::WouldBlock`] if non-blocking.
    pub fn write(&self, value: u64) -> io::Result<()> {
        self.inner.write(value)
    }

    /// Creates a new handle to the same counter (like `dup(2)`).
    /// The clone has its own pollable descriptor.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
        })
    }

    /// Switches non-blocking mode. Like `O_NONBLOCK`, this is shared by all
    /// clones of the eventfd.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// True if the eventfd is in non-blocking mode.
    pub fn is_nonblocking(&self) -> io::Result<bool> {
        self.inner.is_nonblocking()
    }

    /// Prepares this eventfd for a child process spawned while the returned
    /// [`InheritableExport`] is alive: the cross-platform form of letting a
    /// Linux child inherit the eventfd across `exec`.
    ///
    /// It makes inheritable copies of the eventfd's descriptors (handles on
    /// Windows). Pass [`InheritableExport::token`] to the child, for example
    /// in an environment variable, and call [`GkEventFd::import`] there. The
    /// child then shares the counter with this process. Drop the export once
    /// the child has been spawned: that closes this process's copies, not the
    /// child's.
    ///
    /// * Linux and other native platforms: the copy is a plain inheritable
    ///   duplicate of the kernel eventfd, and the token is `gkefd1:n:<fd>`; a
    ///   child that is not using this crate can use that fd number directly.
    /// * macOS and other unix shims: copies of the shared memory object and
    ///   both pipe ends.
    /// * Windows: inheritable copies of the shared section and both sockets.
    ///
    /// As with any inheritable descriptor, every child spawned while the
    /// export is alive inherits the copies (harmless, but they stay open in
    /// that child until it exits). For an unrelated or already running
    /// process, use `send_to` on unix or
    /// `export_to` on Windows instead.
    ///
    /// ```no_run
    /// # use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
    /// let efd = GkEventFd::new(0, EfdFlags::NONE)?;
    /// let export = efd.export_inheritable()?;
    /// let mut child = std::process::Command::new("./worker")
    ///     .env("GK_EVENTFD", export.token())
    ///     .spawn()?;
    /// drop(export);
    /// let value = efd.read()?; // woken by a write in the child
    /// # child.wait()?; Ok::<(), std::io::Error>(())
    /// ```
    pub fn export_inheritable(&self) -> io::Result<InheritableExport> {
        Ok(InheritableExport(self.inner.export_inheritable()?))
    }

    /// Rebuilds an eventfd from a token made by
    /// [`export_inheritable`](Self::export_inheritable) (in a child that
    /// inherited the descriptors) or, on Windows,
    /// `export_to` (for this process). The result shares
    /// the counter with the exporting process and works like any other
    /// `GkEventFd` (polling, clones, mio). Its descriptors are made
    /// close-on-exec (non-inheritable) again.
    ///
    /// Fails with [`io::ErrorKind::InvalidInput`] for a malformed token or
    /// descriptors of the wrong kind, and [`io::ErrorKind::InvalidData`] for
    /// shared memory that is not an eventfd's. In those cases nothing named
    /// by the token is closed.
    ///
    /// # Safety
    ///
    /// On success this takes ownership of the descriptors or handles the
    /// token names (like `FromRawFd`/`FromRawHandle`), so the token must have
    /// been made for this process, be imported only once, and those
    /// descriptors must not be owned by anything else in this process.
    pub unsafe fn import(token: &str) -> io::Result<Self> {
        // SAFETY: forwarded to the caller.
        Ok(Self {
            inner: unsafe { sys::Inner::import(token) }?,
        })
    }

    /// Unix: sends this eventfd to the process at the other end of `stream`,
    /// which receives it with [`recv_from`](Self::recv_from). This is how
    /// Linux passes an eventfd to an unrelated process (`SCM_RIGHTS`); on the
    /// unix shims the shared memory object and pipe ends travel the same
    /// way. Both sides then share the counter.
    ///
    /// Writes an 8-byte header with the descriptors attached; don't
    /// interleave other writes on `stream` with it.
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        self.inner.send_to(std::os::fd::AsFd::as_fd(stream))
    }

    /// Unix: receives an eventfd sent with [`send_to`](Self::send_to). Fails
    /// with [`io::ErrorKind::InvalidData`] if the message is not an eventfd
    /// from the same kind of backend (closing whatever was received), and
    /// [`io::ErrorKind::UnexpectedEof`] if the peer closed the stream.
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self {
            inner: sys::Inner::recv_from(std::os::fd::AsFd::as_fd(stream))?,
        })
    }

    /// Windows: duplicates this eventfd into the running process `pid` and
    /// returns a token for it: the Windows counterpart of
    /// `send_to`, since Windows can't pass handles over a
    /// socket. Send the token to that process (stdin, a pipe, a socket, ...)
    /// and call [`GkEventFd::import`] there. Nothing becomes inheritable.
    /// Needs `PROCESS_DUP_HANDLE` access to the target.
    ///
    /// If the target never imports the token, the duplicated section handle
    /// stays open in it until it exits.
    #[cfg(windows)]
    pub fn export_to(&self, pid: u32) -> io::Result<String> {
        self.inner.export_to(pid)
    }

    /// Splits the handle into its counter and descriptor (compat registry).
    #[cfg(all(unix, not(gk_native)))]
    pub(crate) fn into_core(self) -> (sys::Core, std::os::fd::OwnedFd) {
        self.inner.into_core()
    }
}

/// Inheritable copies of an eventfd's descriptors (handles on Windows), from
/// [`GkEventFd::export_inheritable`]. Spawn the child while this is alive and
/// pass it [`token`](Self::token); dropping it closes this process's copies.
#[derive(Debug)]
pub struct InheritableExport(sys::InheritableExport);

impl InheritableExport {
    /// The text to give the child, for [`GkEventFd::import`].
    pub fn token(&self) -> &str {
        self.0.token()
    }
}

impl fmt::Display for InheritableExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

impl fmt::Debug for GkEventFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("GkEventFd");
        #[cfg(unix)]
        d.field("fd", &std::os::fd::AsRawFd::as_raw_fd(self));
        #[cfg(windows)]
        d.field(
            "socket",
            &std::os::windows::io::AsRawSocket::as_raw_socket(self),
        );
        d.finish_non_exhaustive()
    }
}

/// Byte-level eventfd I/O: buffers must be at least 8 bytes (else `EINVAL`)
/// and carry a native-endian `u64`. Returns 8 on success.
impl io::Read for &GkEventFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.len() < 8 {
            return Err(error::invalid_input());
        }
        counter::encode(GkEventFd::read(self)?, buf)
    }
}

/// See the [`io::Read`] impl for `&GkEventFd`.
impl io::Read for GkEventFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &*self, buf)
    }
}

/// Byte-level eventfd I/O: reads a native-endian `u64` from the first 8
/// bytes (`EINVAL` if shorter) and returns 8.
impl io::Write for &GkEventFd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        GkEventFd::write(self, counter::decode(buf)?)?;
        Ok(8)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// See the [`io::Write`] impl for `&GkEventFd`.
impl io::Write for GkEventFd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
mod unix_impls {
    use super::GkEventFd;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

    /// On Linux this is the kernel eventfd. On the shim it is the read end
    /// of a pipe that is readable while the counter is non-zero.
    impl AsFd for GkEventFd {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.inner.as_fd()
        }
    }

    impl AsRawFd for GkEventFd {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_fd().as_raw_fd()
        }
    }

    #[cfg(feature = "mio")]
    impl mio::event::Source for GkEventFd {
        fn register(
            &mut self,
            registry: &mio::Registry,
            token: mio::Token,
            interests: mio::Interest,
        ) -> std::io::Result<()> {
            mio::unix::SourceFd(&self.as_raw_fd()).register(registry, token, interests)
        }
        fn reregister(
            &mut self,
            registry: &mio::Registry,
            token: mio::Token,
            interests: mio::Interest,
        ) -> std::io::Result<()> {
            mio::unix::SourceFd(&self.as_raw_fd()).reregister(registry, token, interests)
        }
        fn deregister(&mut self, registry: &mio::Registry) -> std::io::Result<()> {
            mio::unix::SourceFd(&self.as_raw_fd()).deregister(registry)
        }
    }
}

/// Raw-descriptor conversions only exist where the descriptor *is* the eventfd.
#[cfg(gk_native)]
mod native_impls {
    use super::GkEventFd;
    use crate::sys::Inner;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};

    impl From<GkEventFd> for OwnedFd {
        fn from(e: GkEventFd) -> OwnedFd {
            e.inner.into()
        }
    }

    /// The fd must be an eventfd.
    impl From<OwnedFd> for GkEventFd {
        fn from(fd: OwnedFd) -> Self {
            Self {
                inner: Inner::from(fd),
            }
        }
    }

    impl IntoRawFd for GkEventFd {
        fn into_raw_fd(self) -> RawFd {
            OwnedFd::from(self).into_raw_fd()
        }
    }

    impl FromRawFd for GkEventFd {
        /// # Safety
        /// `fd` must be an open eventfd owned by the caller.
        unsafe fn from_raw_fd(fd: RawFd) -> Self {
            // SAFETY: forwarded to the caller.
            Self::from(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }
}

#[cfg(windows)]
mod windows_impls {
    use super::GkEventFd;
    use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};

    /// The read side of the internal socket pair; readable while the counter
    /// is non-zero. Use it with `WSAPoll`/`select`.
    impl AsSocket for GkEventFd {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            self.inner.as_socket()
        }
    }

    impl AsRawSocket for GkEventFd {
        fn as_raw_socket(&self) -> RawSocket {
            self.inner.as_socket().as_raw_socket()
        }
    }

    #[cfg(feature = "mio")]
    impl mio::event::Source for GkEventFd {
        fn register(
            &mut self,
            registry: &mio::Registry,
            token: mio::Token,
            interests: mio::Interest,
        ) -> std::io::Result<()> {
            self.inner.register(registry, token, interests)
        }
        fn reregister(
            &mut self,
            registry: &mio::Registry,
            token: mio::Token,
            interests: mio::Interest,
        ) -> std::io::Result<()> {
            self.inner.reregister(registry, token, interests)
        }
        fn deregister(&mut self, registry: &mio::Registry) -> std::io::Result<()> {
            self.inner.deregister(registry)
        }
    }
}

/// Compiles and runs the README's Rust examples as doctests. They use mio and
/// `tokio::io::unix::AsyncFd`, so this only exists with `--features mio` on Unix.
#[cfg(all(doctest, unix, feature = "mio"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn is_send_sync() {
        assert_send_sync::<GkEventFd>();
    }
}

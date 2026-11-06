//! Native backend for Linux and Android: a thin wrapper around the kernel
//! `eventfd(2)` object. No shim, no extra validation: every error (`EAGAIN`,
//! `EINVAL`, ...) comes straight from the kernel, so semantics are exactly the
//! kernel's.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use crate::flags::EfdFlags;

const COUNTER_SIZE: usize = core::mem::size_of::<u64>();

#[derive(Debug)]
pub(crate) struct Inner {
    fd: OwnedFd,
}

/// Converts a `-1`-on-error libc return value into an `io::Result`.
fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Converts a short transfer on an eventfd (which the kernel never produces
/// for an 8-byte buffer) into an error rather than silently mis-reporting.
fn short_transfer() -> io::Error {
    io::Error::from_raw_os_error(libc::EIO)
}

impl Inner {
    pub(crate) fn new(initval: u32, flags: EfdFlags) -> io::Result<Self> {
        // SAFETY: `eventfd` takes no pointers; it either fails with -1 or
        // returns a fresh descriptor that nothing else owns.
        let fd = cvt(unsafe { libc::eventfd(initval, flags.bits()) })?;
        // SAFETY: `fd` is a valid, open descriptor exclusively owned by us.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub(crate) fn read(&self) -> io::Result<u64> {
        let mut value: u64 = 0;
        loop {
            // SAFETY: `value` is a live, writable 8-byte buffer for the whole
            // call, and `self.fd` is an open descriptor.
            let n = unsafe { libc::read(self.raw(), (&raw mut value).cast(), COUNTER_SIZE) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return if n as usize == COUNTER_SIZE {
                Ok(value)
            } else {
                Err(short_transfer())
            };
        }
    }

    pub(crate) fn write(&self, value: u64) -> io::Result<()> {
        let buf = value.to_ne_bytes();
        loop {
            // SAFETY: `buf` is a live, readable 8-byte buffer for the whole
            // call, and `self.fd` is an open descriptor.
            let n = unsafe { libc::write(self.raw(), buf.as_ptr().cast(), COUNTER_SIZE) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return if n as usize == COUNTER_SIZE {
                Ok(())
            } else {
                Err(short_transfer())
            };
        }
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        // `OwnedFd::try_clone` uses `fcntl(F_DUPFD_CLOEXEC, 0)`.
        Ok(Self {
            fd: self.fd.try_clone()?,
        })
    }

    fn get_fl(&self) -> io::Result<libc::c_int> {
        // SAFETY: F_GETFL takes no pointer argument; `self.fd` is open.
        cvt(unsafe { libc::fcntl(self.raw(), libc::F_GETFL) })
    }

    pub(crate) fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        let flags = self.get_fl()?;
        let new = if nb {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        if new != flags {
            // SAFETY: F_SETFL takes an int argument; `self.fd` is open.
            cvt(unsafe { libc::fcntl(self.raw(), libc::F_SETFL, new) })?;
        }
        Ok(())
    }

    pub(crate) fn is_nonblocking(&self) -> io::Result<bool> {
        Ok(self.get_fl()? & libc::O_NONBLOCK != 0)
    }
}

impl AsFd for Inner {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl From<OwnedFd> for Inner {
    fn from(fd: OwnedFd) -> Self {
        Self { fd }
    }
}

impl From<Inner> for OwnedFd {
    fn from(inner: Inner) -> Self {
        inner.fd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::EFD_MAX;
    use std::time::Duration;

    fn nb() -> EfdFlags {
        EfdFlags::NONBLOCK
    }

    fn readable(e: &Inner) -> bool {
        let mut pfd = libc::pollfd {
            fd: e.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid pollfd array of length 1; timeout 0.
        let n = unsafe { libc::poll(&mut pfd, 1, 0) };
        assert!(n >= 0, "poll failed: {}", io::Error::last_os_error());
        pfd.revents & libc::POLLIN != 0
    }

    #[test]
    fn initval_is_read_back() {
        let e = Inner::new(42, nb()).unwrap();
        assert_eq!(e.read().unwrap(), 42);
    }

    #[test]
    fn read_resets_counter() {
        let e = Inner::new(0, nb()).unwrap();
        e.write(3).unwrap();
        e.write(4).unwrap();
        assert_eq!(e.read().unwrap(), 7);
        assert_eq!(e.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn semaphore_mode_decrements_by_one() {
        let e = Inner::new(3, nb() | EfdFlags::SEMAPHORE).unwrap();
        for _ in 0..3 {
            assert_eq!(e.read().unwrap(), 1);
        }
        assert_eq!(e.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn empty_nonblocking_read_is_eagain() {
        let e = Inner::new(0, nb()).unwrap();
        let err = e.read().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn write_u64_max_is_einval() {
        let e = Inner::new(0, nb()).unwrap();
        let err = e.write(u64::MAX).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn overflow_at_efd_max_is_eagain() {
        let e = Inner::new(0, nb()).unwrap();
        e.write(EFD_MAX).unwrap();
        let err = e.write(1).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
        assert_eq!(e.read().unwrap(), EFD_MAX);
        e.write(EFD_MAX - 1).unwrap();
        e.write(1).unwrap();
        assert_eq!(e.write(1).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(e.read().unwrap(), EFD_MAX);
    }

    #[test]
    fn try_clone_shares_counter_with_new_fd() {
        let a = Inner::new(0, nb()).unwrap();
        let b = a.try_clone().unwrap();
        assert_ne!(a.raw(), b.raw());
        a.write(5).unwrap();
        assert_eq!(b.read().unwrap(), 5);
        assert_eq!(a.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        // The dup is close-on-exec.
        // SAFETY: F_GETFD takes no pointer argument; `b.fd` is open.
        let fdflags = unsafe { libc::fcntl(b.raw(), libc::F_GETFD) };
        assert!(fdflags >= 0 && fdflags & libc::FD_CLOEXEC != 0);
    }

    #[test]
    fn set_nonblocking_round_trip_shared_by_clones() {
        let a = Inner::new(0, EfdFlags::NONE).unwrap();
        assert!(!a.is_nonblocking().unwrap());
        a.set_nonblocking(true).unwrap();
        assert!(a.is_nonblocking().unwrap());
        let b = a.try_clone().unwrap();
        assert!(b.is_nonblocking().unwrap());
        assert_eq!(a.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        b.set_nonblocking(false).unwrap();
        assert!(!a.is_nonblocking().unwrap());
        a.set_nonblocking(false).unwrap();
        assert!(!a.is_nonblocking().unwrap());

        let c = Inner::new(0, nb()).unwrap();
        assert!(c.is_nonblocking().unwrap());
    }

    #[test]
    fn cloexec_flag_sets_fd_cloexec() {
        let fd_flags = |e: &Inner| {
            // SAFETY: F_GETFD takes no pointer argument; the fd is open.
            let f = unsafe { libc::fcntl(e.raw(), libc::F_GETFD) };
            assert!(f >= 0);
            f
        };
        let with = Inner::new(0, EfdFlags::CLOEXEC).unwrap();
        assert!(fd_flags(&with) & libc::FD_CLOEXEC != 0);
        let without = Inner::new(0, EfdFlags::NONE).unwrap();
        assert!(fd_flags(&without) & libc::FD_CLOEXEC == 0);
    }

    #[test]
    fn poll_readiness_tracks_counter() {
        let e = Inner::new(0, nb()).unwrap();
        assert!(!readable(&e));
        e.write(1).unwrap();
        assert!(readable(&e));
        e.read().unwrap();
        assert!(!readable(&e));

        let s = Inner::new(2, nb() | EfdFlags::SEMAPHORE).unwrap();
        assert!(readable(&s));
        s.read().unwrap();
        assert!(readable(&s));
        s.read().unwrap();
        assert!(!readable(&s));
    }

    #[test]
    fn blocking_read_is_woken_by_other_thread() {
        let e = Inner::new(0, EfdFlags::NONE).unwrap();
        std::thread::scope(|s| {
            let reader = s.spawn(|| e.read());
            std::thread::sleep(Duration::from_millis(50));
            e.write(9).unwrap();
            assert_eq!(reader.join().unwrap().unwrap(), 9);
        });
    }

    #[test]
    fn owned_fd_round_trip() {
        let e = Inner::new(6, nb()).unwrap();
        let raw = e.raw();
        let fd: OwnedFd = e.into();
        assert_eq!(fd.as_raw_fd(), raw);
        let e = Inner::from(fd);
        assert_eq!(e.as_fd().as_raw_fd(), raw);
        assert_eq!(e.read().unwrap(), 6);
    }
}

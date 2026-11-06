//! The `<sys/eventfd.h>` interface, with C semantics.
//!
//! Every function returns `0` (or a descriptor / byte count) on success, and
//! `-1` with `errno` set on failure, exactly like the libc functions:
//!
//! | Function | C equivalent |
//! |---|---|
//! | [`eventfd`] | `eventfd(2)` |
//! | [`eventfd_read`] | glibc `eventfd_read(3)` |
//! | [`eventfd_write`] | glibc `eventfd_write(3)` |
//! | [`eventfd_close`] | `close(2)` on an eventfd |
//! | [`eventfd_read_buf`] | `read(2)` on an eventfd |
//! | [`eventfd_write_buf`] | `write(2)` on an eventfd |
//! | `eventfd_socket` (Windows only) | — (the `SOCKET` to `WSAPoll`/`select` on) |
//!
//! # Backends
//!
//! * **Native: Linux, Android, FreeBSD 13+, NetBSD 10+, illumos** (without
//!   the `force-shim` feature): every function is a plain passthrough to
//!   libc (`eventfd`, `read`, `write`, `close`). The returned descriptor *is*
//!   the kernel eventfd, so raw `read(2)`, `write(2)`, `close(2)` and
//!   `dup(2)` work too.
//! * **macOS, iOS, OpenBSD, DragonFly** (and native targets with
//!   `force-shim`): descriptors are kept
//!   in a process-wide registry. The returned `int` is the read end of the
//!   shim's pipe, a real descriptor that `poll`/`select`/`kqueue` report
//!   readable exactly while the counter is non-zero. Read, write and close it
//!   only through this module: raw `read(2)`/`write(2)` reach the signalling
//!   pipe, not the counter, and a raw `close(2)` leaks the registry entry.
//! * **Windows**: the returned `int` is a synthetic number `>= 65536`: never
//!   a truncated `SOCKET`, and never a valid CRT descriptor. Use
//!   `eventfd_socket` to get the pollable socket.
//!
//! Descriptors not created by [`eventfd`] fail with `EBADF` on the shim
//! backends.
//!
//! ```
//! use adaptif_gatotkaca_eventfd::compat::{self, EFD_NONBLOCK, eventfd_t};
//!
//! let fd = compat::eventfd(0, EFD_NONBLOCK);
//! assert!(fd >= 0);
//! assert_eq!(compat::eventfd_write(fd, 7), 0);
//! let mut value: eventfd_t = 0;
//! assert_eq!(unsafe { compat::eventfd_read(fd, &mut value) }, 0);
//! assert_eq!(value, 7);
//! assert_eq!(compat::eventfd_close(fd), 0);
//! ```

use core::ffi::{c_int, c_uint, c_void};

pub use crate::flags::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, eventfd_t};

/// Creates an eventfd, like `eventfd(2)`.
///
/// Returns a descriptor `>= 0`, or `-1` with `errno` set (`EINVAL` for
/// unknown flag bits, `EMFILE`/`ENFILE`/`ENOMEM` on resource exhaustion).
/// Close it with [`eventfd_close`].
pub fn eventfd(initval: c_uint, flags: c_int) -> c_int {
    imp::eventfd(initval, flags)
}

/// Reads the counter into `*value`, like glibc's `eventfd_read`.
///
/// Normal mode returns the value and resets the counter; `EFD_SEMAPHORE`
/// returns 1 and decrements it. If the counter is 0 this blocks, or fails
/// with `EAGAIN` on a non-blocking eventfd. Returns `0` or `-1` with `errno`.
///
/// # Safety
///
/// `value` must be valid for writing an [`eventfd_t`] (a null pointer fails
/// with `EFAULT`).
pub unsafe fn eventfd_read(fd: c_int, value: *mut eventfd_t) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { imp::eventfd_read(fd, value) }
}

/// Adds `value` to the counter, like glibc's `eventfd_write`.
///
/// Fails with `EINVAL` for `u64::MAX`. If the counter would exceed
/// [`EFD_MAX`](crate::EFD_MAX) this blocks, or fails with `EAGAIN` on a
/// non-blocking eventfd. Returns `0` or `-1` with `errno`.
pub fn eventfd_write(fd: c_int, value: eventfd_t) -> c_int {
    imp::eventfd_write(fd, value)
}

/// Closes an eventfd returned by [`eventfd`]. Returns `0` or `-1` with
/// `errno` (`EBADF` for an unknown descriptor).
///
/// On native platforms this is `close(2)`, so the usual rule applies: only close
/// descriptors you own. On the shim backends a thread blocked in a read or
/// write on `fd` keeps the underlying eventfd alive until it returns (as with
/// the kernel eventfd).
pub fn eventfd_close(fd: c_int) -> c_int {
    imp::eventfd_close(fd)
}

/// `read(2)` with eventfd semantics: stores the counter value (see
/// [`eventfd_read`]) as a native-endian `u64` in the first 8 bytes of `buf`.
///
/// Returns `8`, or `-1` with `errno`: `EINVAL` if `count < 8`, `EAGAIN` if
/// non-blocking and the counter is 0, `EBADF` for an unknown descriptor.
///
/// # Safety
///
/// `buf` must be valid for writing `count` bytes (null fails with `EFAULT`
/// on the shim backends). No alignment is required.
pub unsafe fn eventfd_read_buf(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    // SAFETY: forwarded to the caller.
    unsafe { imp::eventfd_read_buf(fd, buf, count) }
}

/// `write(2)` with eventfd semantics: adds the native-endian `u64` in the
/// first 8 bytes of `buf` to the counter (see [`eventfd_write`]).
///
/// Returns `8`, or `-1` with `errno`: `EINVAL` if `count < 8` or the value is
/// `u64::MAX`, `EAGAIN` if non-blocking and the counter would overflow,
/// `EBADF` for an unknown descriptor.
///
/// # Safety
///
/// `buf` must be valid for reading `count` bytes (null fails with `EFAULT`
/// on the shim backends). No alignment is required.
pub unsafe fn eventfd_write_buf(fd: c_int, buf: *const c_void, count: usize) -> isize {
    // SAFETY: forwarded to the caller.
    unsafe { imp::eventfd_write_buf(fd, buf, count) }
}

/// Returns the `SOCKET` of an eventfd, for `WSAPoll`/`select`/IOCP readiness.
/// It is readable exactly while the counter is non-zero.
///
/// Returns `RawSocket::MAX` (`INVALID_SOCKET`) with `errno = EBADF` for an
/// unknown descriptor. The socket stays owned by the eventfd: don't
/// `recv` from or close it, and don't use it after [`eventfd_close`].
#[cfg(windows)]
pub fn eventfd_socket(fd: c_int) -> std::os::windows::io::RawSocket {
    imp::eventfd_socket(fd)
}

// ---------------------------------------------------------------------------
// errno
// ---------------------------------------------------------------------------

#[cfg(windows)]
unsafe extern "C" {
    /// The CRT's thread-local `errno` (`<errno.h>`: `#define errno (*_errno())`).
    fn _errno() -> *mut c_int;
}

/// True if `fd` was returned by [`eventfd`] and not closed yet, on the shim
/// backends. Always false on native platforms, where there is no registry.
#[cfg(feature = "ffi")]
pub(crate) fn is_registered(fd: c_int) -> bool {
    imp::is_registered(fd)
}

/// Pointer to the calling thread's C `errno`.
#[cfg_attr(all(gk_native, not(feature = "ffi")), allow(dead_code))]
fn errno_location() -> *mut c_int {
    // SAFETY: all of these return the calling thread's errno and have no
    // preconditions.
    unsafe {
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "emscripten",
            target_os = "redox",
            target_os = "hurd",
            target_os = "fuchsia",
        ))]
        return libc::__errno_location();
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        return libc::__error();
        #[cfg(any(
            target_os = "android",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "cygwin",
            target_os = "nuttx",
        ))]
        return libc::__errno();
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        return libc::___errno();
        #[cfg(target_os = "haiku")]
        return libc::_errnop();
        #[cfg(windows)]
        return _errno();
    }
}

/// Sets the calling thread's C `errno`.
#[cfg_attr(all(gk_native, not(feature = "ffi")), allow(dead_code))]
pub(crate) fn set_errno(code: c_int) {
    // SAFETY: errno_location() is valid for the calling thread.
    unsafe { *errno_location() = code }
}

#[cfg(test)]
pub(crate) fn errno() -> c_int {
    // SAFETY: errno_location() is valid for the calling thread.
    unsafe { *errno_location() }
}

// ---------------------------------------------------------------------------
// Native (kernel eventfd): passthrough to libc
// ---------------------------------------------------------------------------

#[cfg(gk_native)]
mod imp {
    use core::ffi::{c_int, c_uint, c_void};

    #[cfg(feature = "ffi")]
    pub(super) fn is_registered(_fd: c_int) -> bool {
        false
    }

    pub(super) fn eventfd(initval: c_uint, flags: c_int) -> c_int {
        // SAFETY: no pointers involved.
        unsafe { libc::eventfd(initval, flags) }
    }

    // eventfd_read/eventfd_write are implemented as in glibc (an 8-byte
    // read/write) rather than called from libc, because not every native
    // libc has them (illumos only has eventfd()).

    pub(super) unsafe fn eventfd_read(fd: c_int, value: *mut u64) -> c_int {
        // SAFETY: the caller guarantees `value` is writable for 8 bytes.
        let n = unsafe { libc::read(fd, value.cast(), 8) };
        if n == 8 { 0 } else { -1 }
    }

    pub(super) fn eventfd_write(fd: c_int, value: u64) -> c_int {
        // SAFETY: `value` is a local readable for 8 bytes.
        let n = unsafe { libc::write(fd, (&raw const value).cast(), 8) };
        if n == 8 { 0 } else { -1 }
    }

    pub(super) fn eventfd_close(fd: c_int) -> c_int {
        // SAFETY: no pointers involved; ownership of `fd` is the caller's
        // business, as with close(2).
        unsafe { libc::close(fd) }
    }

    pub(super) unsafe fn eventfd_read_buf(fd: c_int, buf: *mut c_void, count: usize) -> isize {
        // SAFETY: the caller guarantees `buf` is writable for `count` bytes.
        unsafe { libc::read(fd, buf, count) }
    }

    pub(super) unsafe fn eventfd_write_buf(fd: c_int, buf: *const c_void, count: usize) -> isize {
        // SAFETY: the caller guarantees `buf` is readable for `count` bytes.
        unsafe { libc::write(fd, buf, count) }
    }
}

// ---------------------------------------------------------------------------
// Shim backends: registry of GkEventFd
// ---------------------------------------------------------------------------

#[cfg(not(gk_native))]
mod imp {
    use super::set_errno;
    use crate::GkEventFd;
    use crate::error::{errno, to_errno};
    use core::ffi::{c_int, c_uint, c_void};
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, OnceLock, PoisonError, RwLock};

    #[cfg(unix)]
    const EFAULT: c_int = libc::EFAULT;
    #[cfg(windows)]
    const EFAULT: c_int = 14;

    /// Live compat descriptors. Entries are `Arc`s so that a blocking read
    /// or write runs on a clone, *without* holding the lock.
    struct Registry {
        map: HashMap<c_int, Arc<GkEventFd>>,
        /// Next synthetic descriptor to try (Windows only).
        #[cfg(windows)]
        next: c_int,
    }

    /// First synthetic descriptor on Windows. The CRT allows at most 8192
    /// descriptors, so numbers from here on never collide with CRT fds (which
    /// lets `gk_read`/`gk_write`/`gk_close` tell the two apart).
    #[cfg(windows)]
    pub(super) const FIRST_SYNTHETIC_FD: c_int = 0x1_0000;

    fn registry() -> &'static RwLock<Registry> {
        static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            RwLock::new(Registry {
                map: HashMap::new(),
                #[cfg(windows)]
                next: FIRST_SYNTHETIC_FD,
            })
        })
    }

    /// Clones the entry for `fd` out of the registry (lock released on return).
    fn lookup(fd: c_int) -> Option<Arc<GkEventFd>> {
        let reg = registry().read().unwrap_or_else(PoisonError::into_inner);
        reg.map.get(&fd).cloned()
    }

    #[cfg(feature = "ffi")]
    pub(super) fn is_registered(fd: c_int) -> bool {
        let reg = registry().read().unwrap_or_else(PoisonError::into_inner);
        reg.map.contains_key(&fd)
    }

    fn fail(e: &io::Error) -> c_int {
        set_errno(to_errno(e));
        -1
    }

    fn fail_errno(code: c_int) -> c_int {
        set_errno(code);
        -1
    }

    pub(super) fn eventfd(initval: c_uint, flags: c_int) -> c_int {
        let efd = match GkEventFd::with_raw_flags(initval, flags) {
            Ok(efd) => Arc::new(efd),
            Err(e) => return fail(&e),
        };
        let mut reg = registry().write().unwrap_or_else(PoisonError::into_inner);
        insert(&mut reg, efd)
    }

    /// The key is the pipe's read end: a real, pollable descriptor number.
    #[cfg(unix)]
    fn insert(reg: &mut Registry, efd: Arc<GkEventFd>) -> c_int {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&*efd);
        if let Some(stale) = reg.map.insert(fd, efd) {
            // The OS handed out this number again, so the old entry's
            // descriptors were closed behind our back (raw close(2)).
            // Dropping it would close descriptors that now belong to someone
            // else (including the new eventfd), so leak it instead.
            std::mem::forget(stale);
        }
        fd
    }

    /// Synthetic numbers in `FIRST_SYNTHETIC_FD..=c_int::MAX`; SOCKETs don't
    /// fit in an int.
    #[cfg(windows)]
    fn insert(reg: &mut Registry, efd: Arc<GkEventFd>) -> c_int {
        if reg.map.len() > (c_int::MAX - FIRST_SYNTHETIC_FD) as usize {
            return fail_errno(errno::EMFILE);
        }
        loop {
            let fd = reg.next;
            reg.next = if fd == c_int::MAX {
                FIRST_SYNTHETIC_FD
            } else {
                fd + 1
            };
            if let std::collections::hash_map::Entry::Vacant(slot) = reg.map.entry(fd) {
                slot.insert(efd);
                return fd;
            }
        }
    }

    pub(super) unsafe fn eventfd_read(fd: c_int, value: *mut u64) -> c_int {
        let Some(efd) = lookup(fd) else {
            return fail_errno(errno::EBADF);
        };
        if value.is_null() {
            return fail_errno(EFAULT);
        }
        match efd.read() {
            Ok(v) => {
                // SAFETY: the caller guarantees `value` is writable.
                unsafe { value.write_unaligned(v) };
                0
            }
            Err(e) => fail(&e),
        }
    }

    pub(super) fn eventfd_write(fd: c_int, value: u64) -> c_int {
        let Some(efd) = lookup(fd) else {
            return fail_errno(errno::EBADF);
        };
        match efd.write(value) {
            Ok(()) => 0,
            Err(e) => fail(&e),
        }
    }

    pub(super) fn eventfd_close(fd: c_int) -> c_int {
        let removed = {
            let mut reg = registry().write().unwrap_or_else(PoisonError::into_inner);
            reg.map.remove(&fd)
        };
        // Dropped outside the lock; a concurrent reader may still hold a clone.
        match removed {
            Some(_) => 0,
            None => fail_errno(errno::EBADF),
        }
    }

    pub(super) unsafe fn eventfd_read_buf(fd: c_int, buf: *mut c_void, count: usize) -> isize {
        let Some(efd) = lookup(fd) else {
            return fail_errno(errno::EBADF) as isize;
        };
        if count < 8 {
            return fail_errno(errno::EINVAL) as isize;
        }
        if buf.is_null() {
            return fail_errno(EFAULT) as isize;
        }
        match efd.read() {
            Ok(v) => {
                // SAFETY: the caller guarantees `buf` is writable for
                // `count >= 8` bytes.
                unsafe { buf.cast::<u64>().write_unaligned(v) };
                8
            }
            Err(e) => fail(&e) as isize,
        }
    }

    pub(super) unsafe fn eventfd_write_buf(fd: c_int, buf: *const c_void, count: usize) -> isize {
        let Some(efd) = lookup(fd) else {
            return fail_errno(errno::EBADF) as isize;
        };
        if count < 8 {
            return fail_errno(errno::EINVAL) as isize;
        }
        if buf.is_null() {
            return fail_errno(EFAULT) as isize;
        }
        // SAFETY: the caller guarantees `buf` is readable for `count >= 8` bytes.
        let v = unsafe { buf.cast::<u64>().read_unaligned() };
        match efd.write(v) {
            Ok(()) => 8,
            Err(e) => fail(&e) as isize,
        }
    }

    #[cfg(windows)]
    pub(super) fn eventfd_socket(fd: c_int) -> std::os::windows::io::RawSocket {
        match lookup(fd) {
            Some(efd) => std::os::windows::io::AsRawSocket::as_raw_socket(&*efd),
            None => {
                set_errno(errno::EBADF);
                std::os::windows::io::RawSocket::MAX
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::errno::{EAGAIN, EBADF, EINVAL};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    const NATIVE: bool = cfg!(gk_native);

    /// Serializes these tests so a closed descriptor number isn't reused by
    /// another compat test while we check that it's gone. Every test that
    /// creates compat descriptors (including the `ffi` ones) must hold it.
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read(fd: c_int) -> Result<u64, c_int> {
        let mut v = 0;
        // SAFETY: `v` is a valid eventfd_t.
        match unsafe { eventfd_read(fd, &mut v) } {
            0 => Ok(v),
            r => {
                assert_eq!(r, -1);
                Err(errno())
            }
        }
    }

    fn write(fd: c_int, v: u64) -> Result<(), c_int> {
        match eventfd_write(fd, v) {
            0 => Ok(()),
            r => {
                assert_eq!(r, -1);
                Err(errno())
            }
        }
    }

    #[test]
    fn counter_semantics() {
        let _g = lock();
        let fd = eventfd(3, EFD_NONBLOCK | EFD_CLOEXEC);
        assert!(fd >= 0, "errno {}", errno());
        assert_eq!(write(fd, 2), Ok(()));
        assert_eq!(read(fd), Ok(5));
        assert_eq!(read(fd), Err(EAGAIN));
        // Overflow on a non-blocking eventfd.
        assert_eq!(write(fd, crate::EFD_MAX), Ok(()));
        assert_eq!(write(fd, 1), Err(EAGAIN));
        assert_eq!(read(fd), Ok(crate::EFD_MAX));
        assert_eq!(eventfd_close(fd), 0);
    }

    #[test]
    fn semaphore() {
        let _g = lock();
        let fd = eventfd(3, EFD_SEMAPHORE | EFD_NONBLOCK);
        assert!(fd >= 0);
        for _ in 0..3 {
            assert_eq!(read(fd), Ok(1));
        }
        assert_eq!(read(fd), Err(EAGAIN));
        assert_eq!(eventfd_close(fd), 0);
    }

    #[test]
    fn einval() {
        let _g = lock();
        assert_eq!(eventfd(0, 0x4000_0000), -1);
        assert_eq!(errno(), EINVAL);

        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        assert_eq!(write(fd, u64::MAX), Err(EINVAL));
        let mut small = [0u8; 4];
        // SAFETY: buffers are valid for the given lengths.
        unsafe {
            assert_eq!(eventfd_read_buf(fd, small.as_mut_ptr().cast(), 4), -1);
            assert_eq!(errno(), EINVAL);
            assert_eq!(eventfd_write_buf(fd, small.as_ptr().cast(), 4), -1);
            assert_eq!(errno(), EINVAL);
        }
        assert_eq!(eventfd_close(fd), 0);
    }

    #[test]
    fn buffer_io() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        let mut buf = [0u8; 17];
        buf[1..9].copy_from_slice(&40u64.to_ne_bytes());
        // SAFETY: buffers are valid for the given lengths (unaligned on purpose).
        unsafe {
            assert_eq!(eventfd_write_buf(fd, buf.as_ptr().add(1).cast(), 8), 8);
            assert_eq!(eventfd_write_buf(fd, buf.as_ptr().add(1).cast(), 16), 8);
            let mut out = [0u8; 17];
            assert_eq!(eventfd_read_buf(fd, out.as_mut_ptr().add(1).cast(), 16), 8);
            assert_eq!(u64::from_ne_bytes(out[1..9].try_into().unwrap()), 80);
            assert_eq!(eventfd_read_buf(fd, out.as_mut_ptr().cast(), 8), -1);
            assert_eq!(errno(), EAGAIN);
        }
        assert_eq!(eventfd_close(fd), 0);
    }

    #[test]
    fn ebadf() {
        let _g = lock();
        assert_eq!(write(-1, 1), Err(EBADF));
        assert_eq!(read(-1), Err(EBADF));
        assert_eq!(eventfd_close(-1), -1);
        assert_eq!(errno(), EBADF);

        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        assert_eq!(eventfd_close(fd), 0);
        // On native platforms `fd` is an ordinary closed descriptor that other tests in
        // this process may reuse at any time, so only the registry backends
        // can check it deterministically.
        if !NATIVE {
            assert_eq!(write(fd, 1), Err(EBADF));
            assert_eq!(read(fd), Err(EBADF));
            assert_eq!(eventfd_close(fd), -1);
            assert_eq!(errno(), EBADF);
        }
    }

    #[cfg(not(gk_native))]
    #[test]
    fn shim_rejects_foreign_and_null() {
        let _g = lock();
        // A real descriptor that wasn't created by compat::eventfd.
        #[cfg(unix)]
        {
            let other = crate::GkEventFd::new(1, crate::EfdFlags::NONBLOCK).unwrap();
            let foreign = std::os::fd::AsRawFd::as_raw_fd(&other);
            assert_eq!(write(foreign, 1), Err(EBADF));
        }
        #[cfg(windows)]
        assert_eq!(write(c_int::MAX, 1), Err(EBADF));

        let fd = eventfd(1, EFD_NONBLOCK);
        // SAFETY: null is rejected before any access.
        unsafe {
            assert_eq!(eventfd_read(fd, core::ptr::null_mut()), -1);
            assert_eq!(eventfd_read_buf(fd, core::ptr::null_mut(), 8), -1);
            assert_eq!(eventfd_write_buf(fd, core::ptr::null(), 8), -1);
        }
        assert_eq!(read(fd), Ok(1), "EFAULT must not consume the counter");
        assert_eq!(eventfd_close(fd), 0);
    }

    #[test]
    fn blocking_read_does_not_hold_registry() {
        let _g = lock();
        let fd = eventfd(0, 0);
        assert!(fd >= 0);
        let reader = std::thread::spawn(move || read(fd));
        std::thread::sleep(Duration::from_millis(50));
        // Both of these need the registry while the reader is blocked.
        let other = eventfd(0, EFD_NONBLOCK);
        assert!(other >= 0);
        assert_eq!(eventfd_close(other), 0);
        assert_eq!(write(fd, 9), Ok(()));
        assert_eq!(reader.join().unwrap(), Ok(9));
        assert_eq!(eventfd_close(fd), 0);
    }

    #[cfg(unix)]
    #[test]
    fn poll_readiness() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        let poll = || {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, 0) };
            assert!(n >= 0);
            pfd.revents & libc::POLLIN != 0
        };
        assert!(!poll());
        assert_eq!(write(fd, 1), Ok(()));
        assert!(poll());
        assert_eq!(read(fd), Ok(1));
        assert!(!poll());
        assert_eq!(eventfd_close(fd), 0);
    }

    #[cfg(windows)]
    #[test]
    fn poll_readiness() {
        use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        let sock = eventfd_socket(fd);
        assert_ne!(sock, std::os::windows::io::RawSocket::MAX);
        let poll = || {
            let mut pfd = WSAPOLLFD {
                fd: sock as usize,
                events: POLLRDNORM,
                revents: 0,
            };
            // SAFETY: one valid WSAPOLLFD.
            let n = unsafe { WSAPoll(&mut pfd, 1, 0) };
            assert!(n >= 0);
            pfd.revents & POLLRDNORM != 0
        };
        assert!(!poll());
        assert_eq!(write(fd, 1), Ok(()));
        assert!(poll());
        assert_eq!(read(fd), Ok(1));
        assert!(!poll());
        assert_eq!(eventfd_close(fd), 0);
        assert_eq!(eventfd_socket(fd), std::os::windows::io::RawSocket::MAX);
        assert_eq!(errno(), EBADF);
    }

    /// The C header hard-codes the flag values for shim platforms.
    #[test]
    fn header_flags_match() {
        let header = include_str!("../include/adaptif_gatotkaca_eventfd.h");
        // The numeric `#define NAME <literal>` (the native branch defines the
        // same names as aliases of libc's EFD_*).
        let value = |name: &str| -> c_int {
            let lit = header
                .lines()
                .find_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
                    ["#define", n, lit]
                        if n == name && lit.starts_with(|c: char| c.is_ascii_digit()) =>
                    {
                        Some(lit)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} not defined numerically in header"));
            match lit.strip_prefix("0x") {
                Some(hex) => c_int::from_str_radix(hex, 16).unwrap(),
                None if lit.len() > 1 && lit.starts_with('0') => {
                    c_int::from_str_radix(&lit[1..], 8).unwrap()
                }
                None => lit.parse().unwrap(),
            }
        };
        // These are the shim values of src/flags.rs (also what C code on
        // shim platforms sees).
        let (sem, nb, cloexec) = if NATIVE {
            (0o1, 0o4000, 0o2000000)
        } else {
            (EFD_SEMAPHORE, EFD_NONBLOCK, EFD_CLOEXEC)
        };
        assert_eq!(value("GK_EFD_SEMAPHORE"), sem);
        assert_eq!(value("GK_EFD_NONBLOCK"), nb);
        assert_eq!(value("GK_EFD_CLOEXEC"), cloexec);
    }
}

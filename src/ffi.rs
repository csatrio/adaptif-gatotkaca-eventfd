//! C ABI (feature `ffi`): the [`compat`](crate::compat) functions exported as
//! `gk_eventfd*`. See `include/adaptif_gatotkaca_eventfd.h`.
//!
//! The symbols carry a `gk_` prefix on every platform so they never clash
//! with libc's `eventfd` on Linux. A panic never unwinds into C: it is caught
//! and reported as `-1` with `errno = EIO`.

use crate::compat;
use crate::error::errno::EIO;
use core::ffi::{c_int, c_uint, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Runs `f`, turning a panic into `err` with `errno = EIO`.
fn guard<T>(err: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        compat::set_errno(EIO);
        err
    })
}

/// `int gk_eventfd(unsigned int initval, int flags);`
#[unsafe(no_mangle)]
pub extern "C" fn gk_eventfd(initval: c_uint, flags: c_int) -> c_int {
    guard(-1, || compat::eventfd(initval, flags))
}

/// `int gk_eventfd_read(int fd, eventfd_t *value);`
///
/// # Safety
/// `value` must be null or valid for writing an `eventfd_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_read(fd: c_int, value: *mut compat::eventfd_t) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_read(fd, value) })
}

/// `int gk_eventfd_write(int fd, eventfd_t value);`
#[unsafe(no_mangle)]
pub extern "C" fn gk_eventfd_write(fd: c_int, value: compat::eventfd_t) -> c_int {
    guard(-1, || compat::eventfd_write(fd, value))
}

/// `int gk_eventfd_close(int fd);`
#[unsafe(no_mangle)]
pub extern "C" fn gk_eventfd_close(fd: c_int) -> c_int {
    guard(-1, || compat::eventfd_close(fd))
}

/// `intptr_t gk_eventfd_read_buf(int fd, void *buf, size_t count);`
///
/// # Safety
/// `buf` must be null or valid for writing `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_read_buf(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_read_buf(fd, buf, count) })
}

/// `intptr_t gk_eventfd_write_buf(int fd, const void *buf, size_t count);`
///
/// # Safety
/// `buf` must be null or valid for reading `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_write_buf(
    fd: c_int,
    buf: *const c_void,
    count: usize,
) -> isize {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_write_buf(fd, buf, count) })
}

/// `uintptr_t gk_eventfd_socket(int fd);` (Windows only). Returns
/// `INVALID_SOCKET` (`~0`) with `errno = EBADF` for an unknown descriptor.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn gk_eventfd_socket(fd: c_int) -> usize {
    // A SOCKET is a UINT_PTR, so it always fits in usize.
    guard(usize::MAX, || match compat::eventfd_socket(fd) {
        u64::MAX => usize::MAX,
        s => s as usize,
    })
}

// ---------------------------------------------------------------------------
// Raw-I/O redirect targets (GK_EVENTFD_WRAP_IO in the header)
// ---------------------------------------------------------------------------

/// Return type of `read`/`write`: `ssize_t` on unix, `int` (as the CRT's
/// `_read`/`_write`) on Windows.
#[cfg(unix)]
type SSize = isize;
#[cfg(windows)]
type SSize = c_int;

#[cfg(windows)]
unsafe extern "C" {
    fn _read(fd: c_int, buf: *mut c_void, count: c_uint) -> c_int;
    fn _write(fd: c_int, buf: *const c_void, count: c_uint) -> c_int;
    fn _close(fd: c_int) -> c_int;
}

/// The CRT takes an `unsigned int` count but returns an `int`; larger counts
/// are clamped (a short read/write, which callers must handle anyway).
#[cfg(windows)]
fn crt_count(count: usize) -> c_uint {
    count.min(c_int::MAX as usize) as c_uint
}

/// `ssize_t gk_read(int fd, void *buf, size_t count);` (`int` on Windows)
///
/// `read(2)` that understands eventfds: eventfds from `gk_eventfd` get
/// eventfd semantics (see `gk_eventfd_read_buf`); any other descriptor is
/// forwarded to the real `read` (CRT `_read` on Windows).
///
/// # Safety
/// As for `read(2)`: `buf` must be valid for writing `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_read(fd: c_int, buf: *mut c_void, count: usize) -> SSize {
    guard(-1, || {
        if compat::is_eventfd(fd) {
            // SAFETY: forwarded to the caller.
            let n = unsafe { compat::eventfd_read_buf(fd, buf, count) };
            return n as SSize;
        }
        // SAFETY: forwarded to the caller; this is the real function, not
        // the header's macro.
        #[cfg(unix)]
        return unsafe { libc::read(fd, buf, count) };
        #[cfg(windows)]
        return unsafe { _read(fd, buf, crt_count(count)) };
    })
}

/// `ssize_t gk_write(int fd, const void *buf, size_t count);` (`int` on
/// Windows)
///
/// `write(2)` that understands eventfds: eventfds from `gk_eventfd` get
/// eventfd semantics (see `gk_eventfd_write_buf`); any other descriptor is
/// forwarded to the real `write` (CRT `_write` on Windows).
///
/// # Safety
/// As for `write(2)`: `buf` must be valid for reading `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_write(fd: c_int, buf: *const c_void, count: usize) -> SSize {
    guard(-1, || {
        if compat::is_eventfd(fd) {
            // SAFETY: forwarded to the caller.
            let n = unsafe { compat::eventfd_write_buf(fd, buf, count) };
            return n as SSize;
        }
        // SAFETY: forwarded to the caller.
        #[cfg(unix)]
        return unsafe { libc::write(fd, buf, count) };
        #[cfg(windows)]
        return unsafe { _write(fd, buf, crt_count(count)) };
    })
}

/// `int gk_close(int fd);`
///
/// Closes an eventfd from `gk_eventfd` (like `gk_eventfd_close`); any other
/// descriptor is forwarded to the real `close` (CRT `_close` on Windows).
///
/// # Safety
/// As for `close(2)`: `fd` must not be owned by someone else.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_close(fd: c_int) -> c_int {
    guard(-1, || {
        if compat::is_eventfd(fd) {
            return compat::eventfd_close(fd);
        }
        // SAFETY: forwarded to the caller.
        #[cfg(unix)]
        return unsafe { libc::close(fd) };
        #[cfg(windows)]
        return unsafe { _close(fd) };
    })
}

/// `int gk_eventfd_export_inheritable(int fd, char *buf, size_t len);`
/// (Windows only; see `compat::eventfd_export_inheritable`).
///
/// # Safety
/// `buf` must be valid for writing `len` bytes.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_export_inheritable(
    fd: c_int,
    buf: *mut core::ffi::c_char,
    len: usize,
) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe {
        compat::eventfd_export_inheritable(fd, buf, len)
    })
}

/// `int gk_eventfd_export_release(const char *token);` (Windows only).
///
/// # Safety
/// `token` must be a valid NUL-terminated string.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_export_release(token: *const core::ffi::c_char) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_export_release(token) })
}

/// `int gk_eventfd_export_to(int fd, unsigned long pid, char *buf, size_t len);`
/// (Windows only).
///
/// # Safety
/// `buf` must be valid for writing `len` bytes.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_export_to(
    fd: c_int,
    pid: core::ffi::c_ulong,
    buf: *mut core::ffi::c_char,
    len: usize,
) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe {
        compat::eventfd_export_to(fd, pid, buf, len)
    })
}

/// `int gk_eventfd_import(const char *token);` (Windows only).
///
/// # Safety
/// See `compat::eventfd_import`.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_import(token: *const core::ffi::c_char) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_import(token) })
}

#[cfg(windows)]
unsafe extern "C" {
    fn _dup(fd: c_int) -> c_int;
    fn _dup2(fd: c_int, fd2: c_int) -> c_int;
}

/// `int gk_dup(int fd);`
///
/// `dup(2)` that understands eventfds: an eventfd (including a raw dup of
/// one) gets a new descriptor on the same counter (see `eventfd_dup`); any
/// other descriptor is forwarded to the real `dup` (CRT `_dup` on Windows).
#[unsafe(no_mangle)]
pub extern "C" fn gk_dup(fd: c_int) -> c_int {
    guard(-1, || {
        if compat::is_eventfd(fd) {
            return compat::eventfd_dup(fd);
        }
        // SAFETY: no pointers involved.
        #[cfg(unix)]
        return unsafe { libc::dup(fd) };
        // SAFETY: as above.
        #[cfg(windows)]
        return unsafe { _dup(fd) };
    })
}

/// `int gk_dup2(int oldfd, int newfd);`
///
/// `dup2(2)` that understands eventfds. If `oldfd` is an eventfd, `newfd`
/// becomes another descriptor of it (see `eventfd_dup2`). Otherwise the call
/// is forwarded to the real `dup2` (CRT `_dup2` on Windows), and if that
/// replaced an eventfd descriptor the crate forgets it.
///
/// Returns `newfd` on unix and `0` on Windows on success (as `dup2` /
/// `_dup2` do there), or `-1` with `errno`.
///
/// # Safety
/// As for `dup2(2)`: whatever `newfd` was is closed, so it must not be owned
/// by someone else.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_dup2(oldfd: c_int, newfd: c_int) -> c_int {
    guard(-1, || {
        if compat::is_eventfd(oldfd) {
            // SAFETY: forwarded to the caller.
            let r = unsafe { compat::eventfd_dup2(oldfd, newfd) };
            #[cfg(windows)]
            let r = if r >= 0 { 0 } else { r };
            return r;
        }
        // SAFETY: forwarded to the caller.
        #[cfg(unix)]
        let r = unsafe { libc::dup2(oldfd, newfd) };
        // SAFETY: as above.
        #[cfg(windows)]
        let r = unsafe { _dup2(oldfd, newfd) };
        if r >= 0 && oldfd != newfd {
            compat::forget(newfd);
        }
        r
    })
}

/// `int gk_dup3(int oldfd, int newfd, int flags);` (unix only)
///
/// `dup3(2)` that understands eventfds (see `gk_dup2`). `flags` is `0` or
/// `O_CLOEXEC`. On macOS, iOS and Android, which have no `dup3`, foreign
/// descriptors use `dup2` + `fcntl`, so close-on-exec is not atomic there.
///
/// # Safety
/// As for `gk_dup2`.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    guard(-1, || {
        if compat::is_eventfd(oldfd) {
            // SAFETY: forwarded to the caller.
            return unsafe { compat::eventfd_dup3(oldfd, newfd, flags) };
        }
        // SAFETY: forwarded to the caller.
        let r = unsafe { compat::raw_dup3(oldfd, newfd, flags) };
        if r >= 0 {
            compat::forget(newfd);
        }
        r
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::errno::{EAGAIN, EBADF, EINVAL};

    #[cfg(windows)]
    unsafe extern "C" {
        fn _pipe(fds: *mut c_int, size: c_uint, mode: c_int) -> c_int;
    }

    /// An ordinary (non-eventfd) pipe: `[read, write]`.
    fn pipe() -> [c_int; 2] {
        let mut fds = [-1; 2];
        // SAFETY: `fds` has room for two descriptors.
        #[cfg(unix)]
        let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
        // SAFETY: as above; 0x8000 is _O_BINARY.
        #[cfg(windows)]
        let r = unsafe { _pipe(fds.as_mut_ptr(), 256, 0x8000) };
        assert_eq!(r, 0);
        fds
    }

    #[test]
    fn raw_io_on_eventfd() {
        let _g = compat::tests::lock();
        let fd = gk_eventfd(0, compat::EFD_NONBLOCK);
        assert!(fd >= 0);
        let v = 11u64.to_ne_bytes();
        let mut out = [0u8; 8];
        // SAFETY: valid buffers of the given sizes.
        unsafe {
            assert_eq!(gk_write(fd, v.as_ptr().cast(), 8), 8);
            assert_eq!(gk_write(fd, v.as_ptr().cast(), 4), -1);
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_read(fd, out.as_mut_ptr().cast(), 4), -1);
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_read(fd, out.as_mut_ptr().cast(), 8), 8);
            assert_eq!(u64::from_ne_bytes(out), 11);
            assert_eq!(gk_read(fd, out.as_mut_ptr().cast(), 8), -1);
            assert_eq!(compat::errno(), EAGAIN);
            assert_eq!(gk_close(fd), 0);
        }
    }

    #[test]
    fn raw_io_forwards_other_descriptors() {
        let _g = compat::tests::lock();
        let [r, w] = pipe();
        let mut out = [0u8; 8];
        // SAFETY: valid buffers of the given sizes; we own both descriptors.
        unsafe {
            // Short reads and writes are fine on a pipe (no 8-byte rule).
            assert_eq!(gk_write(w, b"hey".as_ptr().cast(), 3), 3);
            assert_eq!(gk_read(r, out.as_mut_ptr().cast(), 8), 3);
            assert_eq!(&out[..3], b"hey");
            assert_eq!(gk_close(w), 0);
            // EOF once the write end is closed.
            assert_eq!(gk_read(r, out.as_mut_ptr().cast(), 8), 0);
            assert_eq!(gk_close(r), 0);
            assert_eq!(gk_close(-1), -1);
            assert_eq!(compat::errno(), EBADF);
        }
    }

    #[test]
    fn exported_functions_work() {
        let _g = compat::tests::lock();
        let fd = gk_eventfd(0, compat::EFD_NONBLOCK);
        assert!(fd >= 0);
        assert_eq!(gk_eventfd_write(fd, 5), 0);
        let mut v = 0;
        // SAFETY: valid pointer.
        assert_eq!(unsafe { gk_eventfd_read(fd, &mut v) }, 0);
        assert_eq!(v, 5);
        let bytes = 6u64.to_ne_bytes();
        let mut out = [0u8; 8];
        // SAFETY: valid 8-byte buffers.
        unsafe {
            assert_eq!(gk_eventfd_write_buf(fd, bytes.as_ptr().cast(), 8), 8);
            assert_eq!(gk_eventfd_read_buf(fd, out.as_mut_ptr().cast(), 8), 8);
        }
        assert_eq!(u64::from_ne_bytes(out), 6);
        #[cfg(windows)]
        assert_ne!(gk_eventfd_socket(fd), usize::MAX);
        assert_eq!(gk_eventfd_close(fd), 0);
    }

    #[test]
    fn dup_on_eventfd() {
        let _g = compat::tests::lock();
        let fd = gk_eventfd(0, compat::EFD_NONBLOCK);
        let d = gk_dup(fd);
        assert!(fd >= 0 && d >= 0 && d != fd);
        let v = 9u64.to_ne_bytes();
        let mut out = [0u8; 8];
        // SAFETY: valid buffers; we own every descriptor involved.
        unsafe {
            assert_eq!(gk_write(fd, v.as_ptr().cast(), 8), 8);
            assert_eq!(gk_read(d, out.as_mut_ptr().cast(), 8), 8);
            assert_eq!(u64::from_ne_bytes(out), 9);
            let other = gk_eventfd(0, compat::EFD_NONBLOCK);
            let r = gk_dup2(fd, other);
            assert!(r >= 0);
            assert_eq!(gk_write(other, v.as_ptr().cast(), 8), 8);
            assert_eq!(gk_read(d, out.as_mut_ptr().cast(), 8), 8);
            for x in [fd, d, other] {
                assert_eq!(gk_close(x), 0);
            }
        }
    }

    #[test]
    fn dup_forwards_other_descriptors() {
        let _g = compat::tests::lock();
        let [r, w] = pipe();
        let mut out = [0u8; 8];
        // SAFETY: valid buffers; we own every descriptor involved.
        unsafe {
            let w2 = gk_dup(w);
            assert!(w2 >= 0);
            assert_eq!(gk_write(w2, b"ab".as_ptr().cast(), 2), 2);
            assert_eq!(gk_read(r, out.as_mut_ptr().cast(), 8), 2);
            assert_eq!(gk_close(w2), 0);
            assert_eq!(gk_close(w), 0);
            assert_eq!(gk_close(r), 0);
            assert_eq!(gk_dup(-1), -1);
            assert_eq!(compat::errno(), EBADF);
        }
    }

    /// A foreign dup2 onto an eventfd number replaces it; the crate must
    /// forget that number and treat it as the foreign descriptor afterwards.
    #[cfg(unix)]
    #[test]
    fn foreign_dup2_over_eventfd() {
        let _g = compat::tests::lock();
        let fd = gk_eventfd(1, compat::EFD_NONBLOCK);
        let [r, w] = pipe();
        let mut out = [0u8; 8];
        // SAFETY: valid buffers; we own every descriptor involved.
        unsafe {
            assert_eq!(gk_dup2(w, fd), fd);
            // `fd` is now the pipe's write end: plain write semantics.
            assert_eq!(gk_write(fd, b"xyz".as_ptr().cast(), 3), 3);
            assert_eq!(gk_read(r, out.as_mut_ptr().cast(), 8), 3);
            assert!(!compat::is_eventfd(fd));
            for x in [fd, r, w] {
                assert_eq!(gk_close(x), 0);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn dup3_flags() {
        let _g = compat::tests::lock();
        let fd = gk_eventfd(0, compat::EFD_NONBLOCK);
        let [r, w] = pipe();
        // SAFETY: we own every descriptor involved.
        unsafe {
            let t = libc::fcntl(0, libc::F_DUPFD, 300);
            assert!(t >= 0);
            libc::close(t);
            assert_eq!(gk_dup3(fd, t, libc::O_CLOEXEC), t);
            assert!(libc::fcntl(t, libc::F_GETFD) & libc::FD_CLOEXEC != 0);
            assert_eq!(gk_dup3(fd, fd, 0), -1);
            assert_eq!(compat::errno(), EINVAL);
            // Foreign descriptors go to the real (or emulated) dup3.
            assert_eq!(gk_dup3(w, w, 0), -1);
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_dup3(w, t, 0), t);
            assert!(!compat::is_eventfd(t));
            for x in [fd, t, r, w] {
                assert_eq!(gk_close(x), 0);
            }
        }
    }

    #[test]
    fn panic_becomes_eio() {
        let r = guard(-1, || -> c_int { panic!("boom") });
        assert_eq!(r, -1);
        assert_eq!(compat::errno(), EIO);
    }
}

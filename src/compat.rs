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
//! | [`eventfd_dup`] | `dup(2)` on an eventfd |
//! | [`eventfd_dup2`] | `dup2(2)` on an eventfd |
//! | `eventfd_dup3` (unix only) | `dup3(2)` on an eventfd |
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
//!   pipe, not the counter. Duplicate it with [`eventfd_dup`] and friends;
//!   a raw `dup(2)`/`fcntl(F_DUPFD)` copy is also recognised (by the pipe's
//!   identity) the first time it is passed to this module. The eventfd lives
//!   until every descriptor known to this module is closed.
//! * **Windows**: the returned `int` is a synthetic number `>= 65536`: never
//!   a truncated `SOCKET`, and never a valid CRT descriptor. Use
//!   `eventfd_socket` to get the pollable socket. Dups get new synthetic
//!   numbers and share the eventfd; `eventfd_dup2` only accepts targets
//!   `>= 65536` (`EBADF` otherwise).
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

/// Duplicates an eventfd descriptor, like `dup(2)`: returns the lowest free
/// descriptor (a new synthetic number on Windows) referring to the same
/// counter, with close-on-exec cleared. Close each copy separately; the
/// eventfd lives until all are closed. Returns `-1` with `errno` (`EBADF`
/// for an unknown descriptor on the shim backends, `EMFILE`).
pub fn eventfd_dup(fd: c_int) -> c_int {
    imp::eventfd_dup(fd)
}

/// Makes `newfd` refer to the same eventfd as `oldfd`, like `dup2(2)`:
/// if `newfd` was open it is closed first (atomically). If
/// `oldfd == newfd` it just returns `newfd`. Returns `newfd` or `-1` with
/// `errno` (`EBADF` for an unknown `oldfd` or a negative `newfd`; on Windows
/// also for `newfd < 65536`, which belong to the CRT).
///
/// # Safety
///
/// Like `dup2(2)`, this closes whatever `newfd` was: it must not be a
/// descriptor owned by someone else (for example a Rust `OwnedFd`).
pub unsafe fn eventfd_dup2(oldfd: c_int, newfd: c_int) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { imp::eventfd_dup2(oldfd, newfd) }
}

/// Like [`eventfd_dup2`] but, as `dup3(2)`, `flags` may only be `0` or
/// `O_CLOEXEC`, and `oldfd == newfd` fails with `EINVAL`. Unix only.
///
/// # Safety
///
/// As for [`eventfd_dup2`].
#[cfg(unix)]
pub unsafe fn eventfd_dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { imp::eventfd_dup3(oldfd, newfd, flags) }
}

// ---------------------------------------------------------------------------
// Sharing with other processes (see GkEventFd::export_inheritable, send_to,
// export_to, import), with C conventions.
// ---------------------------------------------------------------------------

/// Buffer size that always fits a token from the export functions,
/// including the NUL.
pub const TOKEN_MAX: usize = 4096;

/// Inheritable exports kept alive until `eventfd_export_release` (C callers
/// have no destructor to end them).
fn exports()
-> std::sync::MutexGuard<'static, std::collections::HashMap<String, crate::InheritableExport>> {
    static EXPORTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, crate::InheritableExport>>,
    > = std::sync::OnceLock::new();
    EXPORTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `-1` with `errno` from `e`; bad tokens or descriptors of the wrong kind
/// become `EINVAL`.
fn fail_share(e: &std::io::Error) -> c_int {
    use std::io::ErrorKind::{InvalidData, InvalidInput};
    set_errno(match e.kind() {
        InvalidInput | InvalidData => crate::error::errno::EINVAL,
        _ => crate::error::to_errno(e),
    });
    -1
}

/// Copies `token` and a NUL into `buf` (`ERANGE` if it doesn't fit).
/// Returns the token length.
unsafe fn copy_token(token: &str, buf: *mut core::ffi::c_char, len: usize) -> c_int {
    use crate::error::errno::ERANGE;
    if buf.is_null() {
        set_errno(EFAULT);
        return -1;
    }
    if token.len() >= len {
        set_errno(ERANGE);
        return -1;
    }
    // SAFETY: the caller guarantees `buf` is writable for `len` bytes, and
    // `token.len() + 1 <= len`.
    unsafe {
        core::ptr::copy_nonoverlapping(token.as_ptr(), buf.cast::<u8>(), token.len());
        *buf.add(token.len()) = 0;
    }
    token.len() as c_int
}

#[cfg(unix)]
const EFAULT: c_int = libc::EFAULT;
#[cfg(windows)]
const EFAULT: c_int = 14;

/// Makes `fd` shareable with a child process spawned afterwards (the
/// cross-platform form of inheriting an eventfd across `exec` on Linux),
/// writing a NUL-terminated token into `buf` (`len` bytes; [`TOKEN_MAX`] is
/// enough). Give the token to the child, e.g. in an environment variable,
/// and call [`eventfd_import`] there. On native platforms the token is
/// `gkefd1:n:<fd>` and `<fd>` is an inheritable copy of the kernel eventfd.
///
/// This process's inheritable copies stay open until
/// [`eventfd_export_release`]; release them once the child is spawned. Any
/// other child spawned meanwhile inherits them too.
///
/// Returns the token length, or `-1` with `errno` (`EBADF`, `ERANGE` if
/// `buf` is too small, `EFAULT`).
///
/// # Safety
///
/// `buf` must be valid for writing `len` bytes.
pub unsafe fn eventfd_export_inheritable(
    fd: c_int,
    buf: *mut core::ffi::c_char,
    len: usize,
) -> c_int {
    match imp::export_inheritable(fd) {
        Ok(export) => {
            // SAFETY: forwarded to the caller.
            let n = unsafe { copy_token(export.token(), buf, len) };
            if n >= 0 {
                exports().insert(export.token().to_owned(), export);
            }
            n
        }
        Err(e) => fail_share(&e),
    }
}

/// Closes this process's copies made by [`eventfd_export_inheritable`] for
/// `token`. Returns `0`, or `-1` with `errno = EINVAL` for an unknown token.
///
/// # Safety
///
/// `token` must be a valid NUL-terminated string.
pub unsafe fn eventfd_export_release(token: *const core::ffi::c_char) -> c_int {
    if token.is_null() {
        set_errno(EFAULT);
        return -1;
    }
    // SAFETY: the caller guarantees a NUL-terminated string.
    let token = unsafe { core::ffi::CStr::from_ptr(token) }.to_string_lossy();
    let removed = exports().remove(token.as_ref());
    match removed {
        Some(_) => 0, // dropped here: closes this process's copies
        None => {
            set_errno(crate::error::errno::EINVAL);
            -1
        }
    }
}

/// Opens an eventfd from a token made for this process by
/// [`eventfd_export_inheritable`] (in another process) or, on Windows,
/// `eventfd_export_to`. Returns a new descriptor sharing that eventfd's
/// counter, or `-1` with `errno` (`EINVAL` for a bad token, which leaves
/// every descriptor it names open).
///
/// # Safety
///
/// `token` must be a NUL-terminated string made for this process and
/// imported only once (see [`GkEventFd::import`](crate::GkEventFd::import)).
pub unsafe fn eventfd_import(token: *const core::ffi::c_char) -> c_int {
    if token.is_null() {
        set_errno(EFAULT);
        return -1;
    }
    // SAFETY: the caller guarantees a NUL-terminated string.
    let Ok(token) = unsafe { core::ffi::CStr::from_ptr(token) }.to_str() else {
        set_errno(crate::error::errno::EINVAL);
        return -1;
    };
    // SAFETY: forwarded to the caller.
    match unsafe { imp::import(token) } {
        Ok(fd) => fd,
        Err(e) => fail_share(&e),
    }
}

/// Unix: sends eventfd `fd` over the connected unix socket `sock`, like
/// passing an eventfd with `SCM_RIGHTS` on Linux (see
/// [`GkEventFd::send_to`](crate::GkEventFd::send_to)). The other process
/// calls [`eventfd_recv`]. Returns `0`, or `-1` with `errno`.
#[cfg(unix)]
pub fn eventfd_send(fd: c_int, sock: c_int) -> c_int {
    match imp::send(fd, sock) {
        Ok(()) => 0,
        Err(e) => fail_share(&e),
    }
}

/// Unix: receives an eventfd sent with [`eventfd_send`] on `sock`. Returns
/// a new descriptor sharing the sender's counter, or `-1` with `errno`
/// (`EINVAL` if the message is not an eventfd).
#[cfg(unix)]
pub fn eventfd_recv(sock: c_int) -> c_int {
    match imp::recv(sock) {
        Ok(fd) => fd,
        Err(e) => fail_share(&e),
    }
}

/// Windows: duplicates `fd` into the running process `pid`, writing a
/// NUL-terminated token for it into `buf`: the Windows counterpart of
/// `eventfd_send` (Windows can't pass handles over a socket). Send the
/// token to that process and call [`eventfd_import`] there. Returns the
/// token length, or `-1` with `errno`.
///
/// # Safety
///
/// `buf` must be valid for writing `len` bytes.
#[cfg(windows)]
pub unsafe fn eventfd_export_to(
    fd: c_int,
    pid: u32,
    buf: *mut core::ffi::c_char,
    len: usize,
) -> c_int {
    match imp::export_to(fd, pid) {
        // SAFETY: forwarded to the caller.
        Ok(token) => unsafe { copy_token(&token, buf, len) },
        Err(e) => fail_share(&e),
    }
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

/// True if `fd` refers to an eventfd of this module (including a raw dup of
/// one) on the shim backends. Always false on native platforms, where there
/// is no registry.
#[cfg(feature = "ffi")]
pub(crate) fn is_eventfd(fd: c_int) -> bool {
    imp::is_eventfd(fd)
}

/// Forgets `fd` without closing it, after a foreign `dup2` replaced it.
#[cfg(feature = "ffi")]
pub(crate) fn forget(fd: c_int) {
    imp::forget(fd)
}

/// `dup3(2)` for any descriptor; emulated with `dup2` + `fcntl` where libc
/// has no `dup3` (macOS, iOS, Android), so close-on-exec is not atomic there.
#[cfg(unix)]
#[cfg_attr(not(feature = "ffi"), allow(dead_code))]
pub(crate) unsafe fn raw_dup3(old: c_int, new: c_int, flags: c_int) -> c_int {
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "illumos",
        target_os = "solaris",
    ))]
    // SAFETY: forwarded to the caller.
    return unsafe { libc::dup3(old, new, flags) };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "illumos",
        target_os = "solaris",
    )))]
    {
        if flags & !libc::O_CLOEXEC != 0 || old == new {
            set_errno(crate::error::errno::EINVAL);
            return -1;
        }
        // SAFETY: forwarded to the caller.
        let r = unsafe { libc::dup2(old, new) };
        if r >= 0 && flags & libc::O_CLOEXEC != 0 {
            // SAFETY: plain fcntl on the descriptor just created.
            unsafe { libc::fcntl(new, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        r
    }
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
    pub(super) fn is_eventfd(_fd: c_int) -> bool {
        false
    }

    #[cfg(feature = "ffi")]
    pub(super) fn forget(_fd: c_int) {}

    pub(super) fn eventfd_dup(fd: c_int) -> c_int {
        // SAFETY: no pointers involved.
        unsafe { libc::dup(fd) }
    }

    pub(super) unsafe fn eventfd_dup2(old: c_int, new: c_int) -> c_int {
        // SAFETY: the caller owns `new` (see compat::eventfd_dup2).
        unsafe { libc::dup2(old, new) }
    }

    pub(super) unsafe fn eventfd_dup3(old: c_int, new: c_int, flags: c_int) -> c_int {
        // SAFETY: as above.
        unsafe { super::raw_dup3(old, new, flags) }
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
    /// A `GkEventFd` view of the raw eventfd `fd`, never closed.
    fn view(fd: c_int) -> std::io::Result<std::mem::ManuallyDrop<crate::GkEventFd>> {
        if !crate::sys::is_open(fd) {
            return Err(crate::error::bad_descriptor());
        }
        // SAFETY: `fd` is open; ManuallyDrop means we never close it.
        Ok(std::mem::ManuallyDrop::new(unsafe {
            <crate::GkEventFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
        }))
    }

    pub(super) fn export_inheritable(fd: c_int) -> std::io::Result<crate::InheritableExport> {
        view(fd)?.export_inheritable()
    }

    pub(super) unsafe fn import(token: &str) -> std::io::Result<c_int> {
        // SAFETY: forwarded to the caller.
        let efd = unsafe { crate::GkEventFd::import(token) }?;
        Ok(std::os::fd::IntoRawFd::into_raw_fd(efd))
    }

    pub(super) fn send(fd: c_int, sock: c_int) -> std::io::Result<()> {
        let efd = view(fd)?;
        if !crate::sys::is_open(sock) {
            return Err(crate::error::bad_descriptor());
        }
        // SAFETY: `sock` is open; only borrowed for this call.
        efd.inner
            .send_to(unsafe { std::os::fd::BorrowedFd::borrow_raw(sock) })
    }

    pub(super) fn recv(sock: c_int) -> std::io::Result<c_int> {
        if !crate::sys::is_open(sock) {
            return Err(crate::error::bad_descriptor());
        }
        // SAFETY: `sock` is open; only borrowed for this call.
        let inner =
            crate::sys::Inner::recv_from(unsafe { std::os::fd::BorrowedFd::borrow_raw(sock) })?;
        Ok(std::os::fd::IntoRawFd::into_raw_fd(crate::GkEventFd {
            inner,
        }))
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
    use nohash_hasher::IntMap;
    use std::io;
    use std::sync::{OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    #[cfg(unix)]
    const EFAULT: c_int = libc::EFAULT;
    #[cfg(windows)]
    const EFAULT: c_int = 14;

    fn fail(e: &io::Error) -> c_int {
        set_errno(to_errno(e));
        -1
    }

    fn fail_errno(code: c_int) -> c_int {
        set_errno(code);
        -1
    }

    fn registry() -> &'static RwLock<Registry> {
        static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
        REGISTRY.get_or_init(|| RwLock::new(Registry::new()))
    }

    fn read_lock() -> RwLockReadGuard<'static, Registry> {
        registry().read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_lock() -> RwLockWriteGuard<'static, Registry> {
        registry().write().unwrap_or_else(PoisonError::into_inner)
    }

    // -- unix: the registry owns the descriptor numbers ---------------------

    /// What a descriptor resolves to: a handle on the counter. Lookups return
    /// a clone, so a blocking read or write runs *without* the registry lock.
    #[cfg(unix)]
    type Handle = crate::sys::Core;

    /// Every key is a real descriptor owned by the registry: a `dup` of the
    /// eventfd's pipe read end, so `poll`/`kqueue` work on it directly. Each
    /// key remembers its pipe's identity, which lets lookups notice a number
    /// that was closed behind our back and reused, and recognise raw `dup`s
    /// (`dup(2)`, `fcntl(F_DUPFD)`, ...) of our descriptors.
    #[cfg(unix)]
    struct Registry {
        map: IntMap<c_int, (Handle, PipeId)>,
        /// Pipe identity -> its counter and how many keys refer to it.
        by_pipe: IntMap<PipeId, (Handle, usize)>,
    }

    #[cfg(unix)]
    use crate::sys::{PipeId, pipe_id};

    #[cfg(unix)]
    impl Registry {
        fn new() -> Self {
            Self {
                map: IntMap::default(),
                by_pipe: IntMap::default(),
            }
        }

        /// Registers `fd` (which must be open on the pipe `id`). A stale
        /// entry with the same number is forgotten, not closed.
        fn insert(&mut self, fd: c_int, core: Handle, id: PipeId) {
            if let Some((_, old)) = self.map.insert(fd, (core.clone(), id)) {
                self.unref(old);
            }
            self.by_pipe.entry(id).or_insert((core, 0)).1 += 1;
        }

        /// Forgets `fd` without closing it.
        fn remove(&mut self, fd: c_int) {
            if let Some((_, id)) = self.map.remove(&fd) {
                self.unref(id);
            }
        }

        fn unref(&mut self, id: PipeId) {
            if let Some(entry) = self.by_pipe.get_mut(&id) {
                entry.1 -= 1;
                if entry.1 == 0 {
                    self.by_pipe.remove(&id);
                }
            }
        }

        /// Verified lookup that also repairs the registry: forgets a stale
        /// number and adopts a raw dup of one of our pipes.
        fn resolve(&mut self, fd: c_int) -> Option<(Handle, PipeId)> {
            if fd < 0 {
                return None;
            }
            let actual = pipe_id(fd);
            if let Some((core, id)) = self.map.get(&fd) {
                if actual == Some(*id) {
                    return Some((core.clone(), *id));
                }
                // Closed behind our back and the number reused.
                self.remove(fd);
            }
            let id = actual?;
            let core = self.by_pipe.get(&id)?.0.clone();
            self.insert(fd, core.clone(), id);
            Some((core, id))
        }
    }

    #[cfg(unix)]
    fn lookup(fd: c_int) -> Option<Handle> {
        if fd < 0 {
            return None;
        }
        {
            let reg = read_lock();
            match reg.map.get(&fd) {
                Some((core, id)) => {
                    if pipe_id(fd) == Some(*id) {
                        return Some(core.clone());
                    }
                }
                // Fast path for foreign descriptors (gk_read on a file, ...).
                None if reg.by_pipe.is_empty() => return None,
                None => {
                    let id = pipe_id(fd)?;
                    if !reg.by_pipe.contains_key(&id) {
                        return None;
                    }
                }
            }
        }
        // Needs a repair (stale number or raw dup to adopt).
        write_lock().resolve(fd).map(|(core, _)| core)
    }

    #[cfg(unix)]
    pub(super) fn eventfd(initval: c_uint, flags: c_int) -> c_int {
        let (core, fd) = match GkEventFd::with_raw_flags(initval, flags) {
            Ok(efd) => efd.into_core(),
            Err(e) => return fail(&e),
        };
        let Some(id) = core.pipe_id() else {
            return fail_errno(errno::EIO);
        };
        let fd = std::os::fd::IntoRawFd::into_raw_fd(fd);
        write_lock().insert(fd, core, id);
        fd
    }

    #[cfg(unix)]
    pub(super) fn eventfd_close(fd: c_int) -> c_int {
        let mut reg = write_lock();
        if reg.resolve(fd).is_none() {
            return fail_errno(errno::EBADF);
        }
        reg.remove(fd);
        // Closed under the lock, so the number can't be handed out and
        // registered again before it is actually free.
        // SAFETY: the registry owned `fd` and has just forgotten it.
        unsafe { libc::close(fd) }
    }

    #[cfg(unix)]
    pub(super) fn eventfd_dup(fd: c_int) -> c_int {
        let mut reg = write_lock();
        let Some((core, id)) = reg.resolve(fd) else {
            return fail_errno(errno::EBADF);
        };
        // Like dup(2): lowest free number, close-on-exec cleared.
        // SAFETY: plain fcntl on a descriptor the registry owns.
        let new = unsafe { libc::fcntl(fd, libc::F_DUPFD, 0) };
        if new >= 0 {
            reg.insert(new, core, id);
        }
        new
    }

    /// `dup2`/`dup3` core: makes `new` a descriptor of `old`'s eventfd.
    #[cfg(unix)]
    fn dup_to(
        old: c_int,
        new: c_int,
        check: impl FnOnce() -> Option<c_int>,
        cloexec: bool,
    ) -> c_int {
        let mut reg = write_lock();
        let Some((core, id)) = reg.resolve(old) else {
            return fail_errno(errno::EBADF);
        };
        if new < 0 {
            return fail_errno(errno::EBADF);
        }
        if let Some(code) = check() {
            return fail_errno(code);
        }
        if old == new {
            return new;
        }
        // Atomically replaces whatever `new` was (closing it), like dup2(2).
        loop {
            // SAFETY: `old` is owned by the registry; replacing `new` is the
            // caller's responsibility (see eventfd_dup2).
            if unsafe { libc::dup2(old, new) } >= 0 {
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return -1;
            }
        }
        if cloexec {
            // SAFETY: plain fcntl on the descriptor we just created.
            unsafe { libc::fcntl(new, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        reg.insert(new, core, id);
        new
    }

    #[cfg(unix)]
    pub(super) unsafe fn eventfd_dup2(old: c_int, new: c_int) -> c_int {
        dup_to(old, new, || None, false)
    }

    #[cfg(unix)]
    pub(super) unsafe fn eventfd_dup3(old: c_int, new: c_int, flags: c_int) -> c_int {
        let check = || (flags & !libc::O_CLOEXEC != 0 || old == new).then_some(errno::EINVAL);
        dup_to(old, new, check, flags & libc::O_CLOEXEC != 0)
    }

    // -- Windows: synthetic descriptor numbers ------------------------------

    #[cfg(windows)]
    type Handle = std::sync::Arc<GkEventFd>;

    /// Descriptors are synthetic numbers; several numbers (dups) may share
    /// one `GkEventFd`, which is dropped when the last of them is closed.
    #[cfg(windows)]
    struct Registry {
        map: IntMap<c_int, Handle>,
        /// Next synthetic descriptor to try.
        next: c_int,
    }

    /// First synthetic descriptor on Windows. The CRT allows at most 8192
    /// descriptors, so numbers from here on never collide with CRT fds (which
    /// lets `gk_read`/`gk_write`/`gk_close`/`gk_dup` tell the two apart).
    #[cfg(windows)]
    pub(super) const FIRST_SYNTHETIC_FD: c_int = 0x1_0000;

    #[cfg(windows)]
    impl Registry {
        fn new() -> Self {
            Self {
                map: IntMap::default(),
                next: FIRST_SYNTHETIC_FD,
            }
        }

        /// Registers `efd` under the next free synthetic number.
        fn insert_new(&mut self, efd: Handle) -> c_int {
            if self.map.len() > (c_int::MAX - FIRST_SYNTHETIC_FD) as usize {
                return fail_errno(errno::EMFILE);
            }
            loop {
                let fd = self.next;
                self.next = if fd == c_int::MAX {
                    FIRST_SYNTHETIC_FD
                } else {
                    fd + 1
                };
                if let std::collections::hash_map::Entry::Vacant(slot) = self.map.entry(fd) {
                    slot.insert(efd);
                    return fd;
                }
            }
        }

        #[cfg(feature = "ffi")]
        fn remove(&mut self, fd: c_int) {
            self.map.remove(&fd);
        }
    }

    #[cfg(windows)]
    fn lookup(fd: c_int) -> Option<Handle> {
        read_lock().map.get(&fd).cloned()
    }

    #[cfg(windows)]
    pub(super) fn eventfd(initval: c_uint, flags: c_int) -> c_int {
        match GkEventFd::with_raw_flags(initval, flags) {
            Ok(efd) => write_lock().insert_new(std::sync::Arc::new(efd)),
            Err(e) => fail(&e),
        }
    }

    #[cfg(windows)]
    pub(super) fn eventfd_close(fd: c_int) -> c_int {
        // Dropped outside the lock; a concurrent reader may still hold a clone.
        let removed = write_lock().map.remove(&fd);
        match removed {
            Some(_) => 0,
            None => fail_errno(errno::EBADF),
        }
    }

    #[cfg(windows)]
    pub(super) fn eventfd_dup(fd: c_int) -> c_int {
        let mut reg = write_lock();
        match reg.map.get(&fd).cloned() {
            Some(efd) => reg.insert_new(efd),
            None => fail_errno(errno::EBADF),
        }
    }

    #[cfg(windows)]
    pub(super) unsafe fn eventfd_dup2(old: c_int, new: c_int) -> c_int {
        let replaced = {
            let mut reg = write_lock();
            let Some(efd) = reg.map.get(&old).cloned() else {
                return fail_errno(errno::EBADF);
            };
            // Only synthetic numbers: lower ones belong to the CRT.
            if new < FIRST_SYNTHETIC_FD {
                return fail_errno(errno::EBADF);
            }
            reg.map.insert(new, efd)
        };
        drop(replaced); // outside the lock
        new
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

    #[cfg(windows)]
    pub(super) fn export_inheritable(fd: c_int) -> io::Result<crate::InheritableExport> {
        lookup(fd)
            .ok_or_else(crate::error::bad_descriptor)?
            .export_inheritable()
    }

    #[cfg(windows)]
    pub(super) fn export_to(fd: c_int, pid: u32) -> io::Result<String> {
        lookup(fd)
            .ok_or_else(crate::error::bad_descriptor)?
            .export_to(pid)
    }

    #[cfg(windows)]
    pub(super) unsafe fn import(token: &str) -> io::Result<c_int> {
        // SAFETY: forwarded to the caller.
        let efd = unsafe { GkEventFd::import(token) }?;
        match write_lock().insert_new(std::sync::Arc::new(efd)) {
            -1 => Err(io::Error::other("too many eventfds")),
            fd => Ok(fd),
        }
    }

    #[cfg(unix)]
    pub(super) fn export_inheritable(fd: c_int) -> io::Result<crate::InheritableExport> {
        let core = lookup(fd).ok_or_else(crate::error::bad_descriptor)?;
        Ok(crate::InheritableExport(core.export_inheritable()?))
    }

    /// Registers a `GkEventFd` received from another process.
    #[cfg(unix)]
    fn register(efd: GkEventFd) -> io::Result<c_int> {
        let (core, fd) = efd.into_core();
        let id = core
            .pipe_id()
            .ok_or_else(|| io::Error::from_raw_os_error(errno::EIO))?;
        let fd = std::os::fd::IntoRawFd::into_raw_fd(fd);
        write_lock().insert(fd, core, id);
        Ok(fd)
    }

    #[cfg(unix)]
    pub(super) unsafe fn import(token: &str) -> io::Result<c_int> {
        // SAFETY: forwarded to the caller.
        register(unsafe { GkEventFd::import(token) }?)
    }

    #[cfg(unix)]
    pub(super) fn send(fd: c_int, sock: c_int) -> io::Result<()> {
        let core = lookup(fd).ok_or_else(crate::error::bad_descriptor)?;
        if !crate::sys::is_open(sock) {
            return Err(crate::error::bad_descriptor());
        }
        // SAFETY: `sock` is open; only borrowed for this call.
        core.send_to(unsafe { std::os::fd::BorrowedFd::borrow_raw(sock) })
    }

    #[cfg(unix)]
    pub(super) fn recv(sock: c_int) -> io::Result<c_int> {
        if !crate::sys::is_open(sock) {
            return Err(crate::error::bad_descriptor());
        }
        // SAFETY: `sock` is open; only borrowed for this call.
        let inner =
            crate::sys::Inner::recv_from(unsafe { std::os::fd::BorrowedFd::borrow_raw(sock) })?;
        register(GkEventFd { inner })
    }

    // -- common ---------------------------------------------------------------

    #[cfg(feature = "ffi")]
    pub(super) fn is_eventfd(fd: c_int) -> bool {
        lookup(fd).is_some()
    }

    #[cfg(feature = "ffi")]
    pub(super) fn forget(fd: c_int) {
        write_lock().remove(fd);
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

    #[test]
    fn dup_shares_counter() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        let d = eventfd_dup(fd);
        assert!(d >= 0 && d != fd, "errno {}", errno());
        assert_eq!(write(fd, 3), Ok(()));
        assert_eq!(read(d), Ok(3));
        // Closing one copy leaves the other working.
        assert_eq!(eventfd_close(fd), 0);
        assert_eq!(write(d, 1), Ok(()));
        assert_eq!(read(d), Ok(1));
        assert_eq!(read(d), Err(EAGAIN));
        assert_eq!(eventfd_close(d), 0);
        if !NATIVE {
            assert_eq!(read(d), Err(EBADF));
            assert_eq!(eventfd_dup(d), -1);
            assert_eq!(errno(), EBADF);
        }
    }

    #[test]
    fn dup2_replaces_target() {
        let _g = lock();
        let a = eventfd(0, EFD_NONBLOCK);
        let b = eventfd(5, EFD_NONBLOCK);
        assert!(a >= 0 && b >= 0);
        // SAFETY: we own both descriptors.
        unsafe {
            assert_eq!(eventfd_dup2(a, a), a);
            assert_eq!(eventfd_dup2(a, b), b);
        }
        // `b` now refers to `a`'s counter; its own counter (5) is gone.
        assert_eq!(read(b), Err(EAGAIN));
        assert_eq!(write(a, 2), Ok(()));
        assert_eq!(read(b), Ok(2));
        assert_eq!(eventfd_close(a), 0);
        assert_eq!(write(b, 4), Ok(()));
        assert_eq!(read(b), Ok(4));
        assert_eq!(eventfd_close(b), 0);
        // SAFETY: invalid descriptors only.
        unsafe {
            assert_eq!(eventfd_dup2(-1, 100), -1);
            assert_eq!(errno(), EBADF);
        }
    }

    #[test]
    fn dup2_rejects_negative_target() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        // SAFETY: a negative target can't close anything.
        assert_eq!(unsafe { eventfd_dup2(fd, -5) }, -1);
        assert_eq!(errno(), EBADF);
        assert_eq!(eventfd_close(fd), 0);
    }

    #[cfg(unix)]
    fn cloexec(fd: c_int) -> bool {
        // SAFETY: plain fcntl.
        let f = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(f >= 0);
        f & libc::FD_CLOEXEC != 0
    }

    /// A free descriptor number (opened and closed again).
    #[cfg(unix)]
    fn free_fd() -> c_int {
        // SAFETY: plain dup/close of stdin's number space.
        unsafe {
            let fd = libc::fcntl(0, libc::F_DUPFD, 200);
            assert!(fd >= 0);
            libc::close(fd);
            fd
        }
    }

    #[cfg(unix)]
    #[test]
    fn dup_cloexec_rules() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
        assert!(fd >= 0);
        assert!(cloexec(fd));
        // dup and dup2 clear close-on-exec, dup3(O_CLOEXEC) sets it.
        let d = eventfd_dup(fd);
        assert!(!cloexec(d));
        let t = free_fd();
        // SAFETY: `t` is a free number; `d` and `fd` are ours.
        unsafe {
            assert_eq!(eventfd_dup2(fd, t), t);
            assert!(!cloexec(t));
            assert_eq!(eventfd_dup3(d, t, libc::O_CLOEXEC), t);
            assert!(cloexec(t));
            assert_eq!(eventfd_dup3(d, t, 0), t);
            assert!(!cloexec(t));
            assert_eq!(eventfd_dup3(d, d, 0), -1);
            assert_eq!(errno(), EINVAL);
            assert_eq!(eventfd_dup3(d, t, 0x4000_0000), -1);
            assert_eq!(errno(), EINVAL);
        }
        assert_eq!(write(t, 7), Ok(()));
        assert_eq!(read(fd), Ok(7));
        for x in [fd, d, t] {
            assert_eq!(eventfd_close(x), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn dup_is_pollable() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        let d = eventfd_dup(fd);
        assert!(fd >= 0 && d >= 0);
        let readable = |fd: c_int| {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            assert!(unsafe { libc::poll(&mut pfd, 1, 0) } >= 0);
            pfd.revents & libc::POLLIN != 0
        };
        assert!(!readable(d));
        assert_eq!(write(fd, 1), Ok(()));
        assert!(readable(d) && readable(fd));
        assert_eq!(read(d), Ok(1));
        assert!(!readable(d) && !readable(fd));
        assert_eq!(eventfd_close(fd), 0);
        assert_eq!(eventfd_close(d), 0);
    }

    /// Raw dup(2) / fcntl(F_DUPFD) copies are recognised by pipe identity.
    #[cfg(all(unix, not(gk_native)))]
    #[test]
    fn shim_adopts_raw_dups() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        // SAFETY: plain dups of our descriptor.
        let (raw, fcntl_dup) = unsafe { (libc::dup(fd), libc::fcntl(fd, libc::F_DUPFD, 0)) };
        assert!(raw >= 0 && fcntl_dup >= 0);
        assert_eq!(write(raw, 4), Ok(()));
        assert_eq!(read(fd), Ok(4));
        assert_eq!(write(fd, 6), Ok(()));
        assert_eq!(read(fcntl_dup), Ok(6));
        // Adopted copies are owned by the registry and closed by it.
        assert_eq!(eventfd_close(raw), 0);
        assert_eq!(eventfd_close(fcntl_dup), 0);
        assert_eq!(eventfd_close(fd), 0);
        // Once every known descriptor is closed the eventfd is gone.
        assert_eq!(read(fd), Err(EBADF));
    }

    /// A number closed behind the registry's back and reused for something
    /// else is not mistaken for the eventfd.
    #[cfg(all(unix, not(gk_native)))]
    #[test]
    fn shim_detects_reused_numbers() {
        let _g = lock();
        let fd = eventfd(1, EFD_NONBLOCK);
        assert!(fd >= 0);
        let mut p = [0; 2];
        // SAFETY: `p` has room for two descriptors; dup2 replaces our own
        // eventfd number with an unrelated pipe, as a raw dup2 would.
        unsafe {
            assert_eq!(libc::pipe(p.as_mut_ptr()), 0);
            assert_eq!(libc::dup2(p[0], fd), fd);
        }
        assert_eq!(read(fd), Err(EBADF));
        assert_eq!(eventfd_close(fd), -1);
        // SAFETY: we own all three.
        unsafe {
            libc::close(fd);
            libc::close(p[0]);
            libc::close(p[1]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_dup_numbers() {
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        let d = eventfd_dup(fd);
        assert!(fd >= 0x1_0000 && d >= 0x1_0000 && d != fd);
        assert_eq!(eventfd_socket(fd), eventfd_socket(d));
        // SAFETY: CRT-range and free synthetic targets only.
        unsafe {
            assert_eq!(eventfd_dup2(fd, 3), -1);
            assert_eq!(errno(), EBADF);
            let t = 0x7fff_0000;
            assert_eq!(eventfd_dup2(fd, t), t);
            assert_eq!(write(t, 2), Ok(()));
            assert_eq!(read(d), Ok(2));
            assert_eq!(eventfd_close(t), 0);
        }
        assert_eq!(eventfd_close(fd), 0);
        assert_eq!(write(d, 1), Ok(()));
        assert_eq!(read(d), Ok(1));
        assert_eq!(eventfd_close(d), 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_export_import_c_style() {
        use crate::error::errno::ERANGE;
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        let mut buf = vec![0 as core::ffi::c_char; TOKEN_MAX];
        // SAFETY: `buf` is valid for its length; the token is for us.
        unsafe {
            let n = eventfd_export_to(fd, std::process::id(), buf.as_mut_ptr(), buf.len());
            assert!(n > 0, "errno {}", errno());
            let other = eventfd_import(buf.as_ptr());
            assert!(other >= 0 && other != fd, "errno {}", errno());
            assert_eq!(write(fd, 4), Ok(()));
            assert_eq!(read(other), Ok(4));
            assert_eq!(eventfd_close(other), 0);

            // Too small a buffer.
            let mut small = [0 as core::ffi::c_char; 8];
            assert_eq!(
                eventfd_export_to(fd, std::process::id(), small.as_mut_ptr(), 8),
                -1
            );
            assert_eq!(errno(), ERANGE);

            // Inheritable export: release closes our copies; unknown tokens fail.
            let n = eventfd_export_inheritable(fd, buf.as_mut_ptr(), buf.len());
            assert!(n > 0);
            assert_eq!(eventfd_export_release(buf.as_ptr()), 0);
            assert_eq!(eventfd_export_release(buf.as_ptr()), -1);
            assert_eq!(errno(), EINVAL);

            // Garbage is rejected.
            assert_eq!(eventfd_import(c"gkefd1:i:1".as_ptr()), -1);
            assert_eq!(errno(), EINVAL);
            assert_eq!(eventfd_export_to(-1, 0, buf.as_mut_ptr(), buf.len()), -1);
            assert_eq!(errno(), EBADF);
        }
        assert_eq!(eventfd_close(fd), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unix_send_recv_and_exports_c_style() {
        use std::os::fd::AsRawFd;
        let _g = lock();
        let fd = eventfd(0, EFD_NONBLOCK);
        assert!(fd >= 0);
        let (tx, rx) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(eventfd_send(fd, tx.as_raw_fd()), 0, "errno {}", errno());
        let got = eventfd_recv(rx.as_raw_fd());
        assert!(got >= 0 && got != fd, "errno {}", errno());
        assert_eq!(write(fd, 3), Ok(()));
        assert_eq!(read(got), Ok(3));
        assert_eq!(eventfd_close(got), 0);

        assert_eq!(eventfd_send(-1, tx.as_raw_fd()), -1);
        assert_eq!(errno(), EBADF);
        assert_eq!(eventfd_send(fd, -1), -1);
        assert_eq!(errno(), EBADF);

        let mut buf = vec![0 as core::ffi::c_char; TOKEN_MAX];
        // SAFETY: `buf` is valid for its length; tokens are NUL-terminated.
        unsafe {
            let n = eventfd_export_inheritable(fd, buf.as_mut_ptr(), buf.len());
            assert!(n > 0, "errno {}", errno());
            let mut small = [0 as core::ffi::c_char; 4];
            assert_eq!(eventfd_export_inheritable(fd, small.as_mut_ptr(), 4), -1);
            assert_eq!(errno(), crate::error::errno::ERANGE);
            assert_eq!(eventfd_export_release(buf.as_ptr()), 0);
            assert_eq!(eventfd_export_release(buf.as_ptr()), -1);
            assert_eq!(errno(), EINVAL);
            assert_eq!(eventfd_import(c"gkefd1:x".as_ptr()), -1);
            assert_eq!(errno(), EINVAL);
        }
        assert_eq!(eventfd_close(fd), 0);
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

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
pub unsafe extern "C" fn gk_eventfd_write_buf(fd: c_int, buf: *const c_void, count: usize) -> isize {
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

#[cfg(windows)]
type InvalidParameterHandler = Option<unsafe extern "C" fn(*const u16, *const u16, *const u16, c_uint, usize)>;

#[cfg(windows)]
unsafe extern "C" {
    fn _set_thread_local_invalid_parameter_handler(h: InvalidParameterHandler) -> InvalidParameterHandler;
}

#[cfg(windows)]
unsafe extern "C" fn ignore_invalid_parameter(_: *const u16, _: *const u16, _: *const u16, _: c_uint, _: usize) {}

/// Runs a CRT descriptor call with the invalid-parameter handler silenced.
/// Without it the CRT aborts the process (`STATUS_STACK_BUFFER_OVERRUN`) on a
/// bad descriptor such as `-1`, where POSIX returns `-1` with `EBADF`. The
/// handler is thread-local and restored afterwards; the cost is two TLS stores.
#[cfg(windows)]
fn crt_call<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: installs/restores a thread-local handler; no pointers involved.
    let prev = unsafe { _set_thread_local_invalid_parameter_handler(Some(ignore_invalid_parameter)) };
    let r = f();
    // SAFETY: as above.
    unsafe { _set_thread_local_invalid_parameter_handler(prev) };
    r
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
        // SAFETY: forwarded to the caller (the CRT function itself).
        return crt_call(|| unsafe { _read(fd, buf, crt_count(count)) });
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
        // SAFETY: forwarded to the caller (the CRT function itself).
        return crt_call(|| unsafe { _write(fd, buf, crt_count(count)) });
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
        // SAFETY: forwarded to the caller (the CRT function itself).
        return crt_call(|| unsafe { _close(fd) });
    })
}

/// `int gk_eventfd_export_inheritable(int fd, char *buf, size_t len);`
/// (see `compat::eventfd_export_inheritable`).
///
/// # Safety
/// `buf` must be valid for writing `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_export_inheritable(fd: c_int, buf: *mut core::ffi::c_char, len: usize) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_export_inheritable(fd, buf, len) })
}

/// `int gk_eventfd_export_release(const char *token);`
///
/// # Safety
/// `token` must be a valid NUL-terminated string.
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
pub unsafe extern "C" fn gk_eventfd_export_to(fd: c_int, pid: core::ffi::c_ulong, buf: *mut core::ffi::c_char, len: usize) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_export_to(fd, pid, buf, len) })
}

/// `int gk_eventfd_import(const char *token);`
///
/// # Safety
/// See `compat::eventfd_import`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_eventfd_import(token: *const core::ffi::c_char) -> c_int {
    // SAFETY: forwarded to the caller.
    guard(-1, || unsafe { compat::eventfd_import(token) })
}

/// `int gk_eventfd_send(int fd, int sock);` (unix only; see
/// `compat::eventfd_send`).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn gk_eventfd_send(fd: c_int, sock: c_int) -> c_int {
    guard(-1, || compat::eventfd_send(fd, sock))
}

/// `int gk_eventfd_recv(int sock);` (unix only; see `compat::eventfd_recv`).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn gk_eventfd_recv(sock: c_int) -> c_int {
    guard(-1, || compat::eventfd_recv(sock))
}

// ---------------------------------------------------------------------------
// Process API (GkProcess / GkListener as opaque C handles)
// ---------------------------------------------------------------------------

use crate::{GkListener, GkProcess, ListenerAccess};
use core::ffi::{CStr, c_char};

/// Reads a NUL-terminated UTF-8 name, or sets `errno` (`EFAULT`/`EINVAL`).
unsafe fn c_name<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        compat::set_errno(crate::error::errno::EINVAL);
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string.
    let s = unsafe { CStr::from_ptr(name) }.to_str().ok();
    if s.is_none() {
        compat::set_errno(crate::error::errno::EINVAL);
    }
    s
}

/// Boxes `r` for C, or returns null with `errno` set.
fn boxed<T>(r: std::io::Result<T>) -> *mut T {
    match r {
        Ok(v) => Box::into_raw(Box::new(v)),
        Err(e) => {
            compat::set_errno(crate::error::to_errno(&e));
            core::ptr::null_mut()
        }
    }
}

/// `gk_listener *gk_listener_bind(const char *name);`
///
/// # Safety
/// `name` must be a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_listener_bind(name: *const c_char) -> *mut GkListener {
    guard(core::ptr::null_mut(), || {
        // SAFETY: forwarded to the caller.
        match unsafe { c_name(name) } {
            Some(name) => boxed(GkListener::bind(name)),
            None => core::ptr::null_mut(),
        }
    })
}

/// `GK_LISTENER_ACCESS_*` in C.
const GK_LISTENER_ACCESS_DEFAULT: c_int = 0;
const GK_LISTENER_ACCESS_CURRENT_USER: c_int = 1;

/// `gk_listener *gk_listener_bind_ex(const char *name, int access);`
/// (`GK_LISTENER_ACCESS_DEFAULT` = `gk_listener_bind`,
/// `GK_LISTENER_ACCESS_CURRENT_USER`; anything else is `EINVAL`).
///
/// # Safety
/// `name` must be a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_listener_bind_ex(name: *const c_char, access: c_int) -> *mut GkListener {
    guard(core::ptr::null_mut(), || {
        let access = match access {
            GK_LISTENER_ACCESS_DEFAULT => ListenerAccess::Default,
            GK_LISTENER_ACCESS_CURRENT_USER => ListenerAccess::CurrentUser,
            _ => {
                compat::set_errno(crate::error::errno::EINVAL);
                return core::ptr::null_mut();
            }
        };
        // SAFETY: forwarded to the caller.
        match unsafe { c_name(name) } {
            Some(name) => boxed(GkListener::bind_with(name, access)),
            None => core::ptr::null_mut(),
        }
    })
}

/// `gk_process *gk_listener_accept(gk_listener *listener);`
///
/// # Safety
/// `listener` must come from `gk_listener_bind` and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_listener_accept(listener: *mut GkListener) -> *mut GkProcess {
    guard(core::ptr::null_mut(), || {
        // SAFETY: the caller guarantees a live listener (or null).
        match unsafe { listener.as_ref() } {
            Some(l) => boxed(l.accept()),
            None => {
                compat::set_errno(crate::error::errno::EINVAL);
                core::ptr::null_mut()
            }
        }
    })
}

/// `void gk_listener_free(gk_listener *listener);` (null is ignored)
///
/// # Safety
/// `listener` must come from `gk_listener_bind` and be freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_listener_free(listener: *mut GkListener) {
    if !listener.is_null() {
        // SAFETY: the caller guarantees ownership of a box we handed out.
        guard((), || drop(unsafe { Box::from_raw(listener) }));
    }
}

/// `gk_process *gk_process_connect(const char *name);`
///
/// # Safety
/// `name` must be a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_connect(name: *const c_char) -> *mut GkProcess {
    guard(core::ptr::null_mut(), || {
        // SAFETY: forwarded to the caller.
        match unsafe { c_name(name) } {
            Some(name) => boxed(GkProcess::connect(name)),
            None => core::ptr::null_mut(),
        }
    })
}

/// `gk_process *gk_process_from_parent(void);`
#[unsafe(no_mangle)]
pub extern "C" fn gk_process_from_parent() -> *mut GkProcess {
    guard(core::ptr::null_mut(), || boxed(GkProcess::from_parent()))
}

/// `gk_process *gk_process_for_child(char *env_value, size_t len);`
///
/// For a child the caller spawns itself: returns a connection waiting for
/// it, and writes the NUL-terminated value of `GK_EVENTFD_PARENT` for the
/// child's environment into `env_value` (`len` bytes; `GK_EVENTFD_TOKEN_MAX`
/// is enough).
///
/// # Safety
/// `env_value` must be valid for writing `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_for_child(env_value: *mut c_char, len: usize) -> *mut GkProcess {
    guard(core::ptr::null_mut(), || match GkProcess::for_child() {
        Ok((process, name)) => {
            if env_value.is_null() || name.len() >= len {
                compat::set_errno(crate::error::errno::ERANGE);
                return core::ptr::null_mut();
            }
            // SAFETY: `env_value` is writable for `len > name.len()` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(name.as_ptr(), env_value.cast(), name.len());
                *env_value.add(name.len()) = 0;
            }
            Box::into_raw(Box::new(process))
        }
        Err(e) => {
            compat::set_errno(crate::error::to_errno(&e));
            core::ptr::null_mut()
        }
    })
}

/// `int gk_process_expect_pid(gk_process *process, unsigned long pid);`
///
/// # Safety
/// `process` must be a live handle from this API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_expect_pid(process: *mut GkProcess, pid: core::ffi::c_ulong) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        match unsafe { process.as_mut() } {
            Some(p) => {
                #[allow(clippy::unnecessary_cast)] // c_ulong is u32 on Windows
                p.expect_pid(pid as u32);
                0
            }
            None => {
                compat::set_errno(crate::error::errno::EINVAL);
                -1
            }
        }
    })
}

/// `int gk_process_send(gk_process *process, int fd);`
///
/// # Safety
/// `process` must be a live handle from this API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send(process: *mut GkProcess, fd: c_int) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        match unsafe { process.as_mut() } {
            Some(p) => compat::eventfd_process_send(p, fd),
            None => {
                compat::set_errno(crate::error::errno::EINVAL);
                -1
            }
        }
    })
}

/// `int gk_process_recv(gk_process *process);`
///
/// # Safety
/// `process` must be a live handle from this API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv(process: *mut GkProcess) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        match unsafe { process.as_mut() } {
            Some(p) => compat::eventfd_process_recv(p),
            None => {
                compat::set_errno(crate::error::errno::EINVAL);
                -1
            }
        }
    })
}

/// `void gk_process_free(gk_process *process);` (null is ignored)
///
/// # Safety
/// `process` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_free(process: *mut GkProcess) {
    if !process.is_null() {
        // SAFETY: the caller guarantees ownership of a box we handed out.
        guard((), || drop(unsafe { Box::from_raw(process) }));
    }
}

/// Sets `errno` for a byte-message error: `EMSGSIZE` for sizes (message too
/// long, buffer too small), `EINVAL` for the wrong kind (as
/// `gk_process_recv`).
fn bytes_errno(e: &std::io::Error) {
    use crate::error::errno::{EINVAL, EMSGSIZE};
    compat::set_errno(match e.kind() {
        std::io::ErrorKind::InvalidInput => EMSGSIZE,
        std::io::ErrorKind::InvalidData => EINVAL,
        _ => crate::error::to_errno(e),
    });
}

/// `int gk_process_send_bytes(gk_process *process, const void *buf, size_t len);`
///
/// Sends `len` bytes (at most `GK_PROCESS_BYTES_MAX`, may be 0) as one
/// byte message. `0`, or `-1` with `errno` (`EMSGSIZE` if too long).
///
/// # Safety
/// `process` must be a live handle (or null); `buf` must be valid for
/// reading `len` bytes (it may be null when `len` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send_bytes(process: *mut GkProcess, buf: *const c_void, len: usize) -> c_int {
    guard(-1, || {
        with(process, -1, |p| {
            let msg: &[u8] = if len == 0 {
                &[]
            } else if buf.is_null() {
                compat::set_errno(crate::error::errno::EINVAL);
                return -1;
            } else {
                // SAFETY: the caller guarantees `len` readable bytes.
                unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) }
            };
            match p.send_bytes(msg) {
                Ok(()) => 0,
                Err(e) => {
                    bytes_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `intptr_t gk_process_recv_bytes(gk_process *process, void *buf, size_t cap);`
///
/// Receives a byte message into `buf` and returns its length, or `-1` with
/// `errno`: `EMSGSIZE` if `cap` is too small (the message is kept for the
/// next call; `GK_PROCESS_BYTES_MAX` always fits), `EINVAL` if an object
/// was sent instead.
///
/// # Safety
/// `process` must be a live handle (or null); `buf` must be valid for
/// writing `cap` bytes (it may be null when `cap` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_bytes(process: *mut GkProcess, buf: *mut c_void, cap: usize) -> isize {
    guard(-1, || {
        with(process, -1, |p| {
            let out: &mut [u8] = if cap == 0 {
                &mut []
            } else if buf.is_null() {
                compat::set_errno(crate::error::errno::EINVAL);
                return -1;
            } else {
                // SAFETY: the caller guarantees `cap` writable bytes.
                unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), cap) }
            };
            match p.recv_bytes_into(out) {
                Ok(n) => n as isize,
                Err(e) => {
                    bytes_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `int gk_process_fd(gk_process *process);` (unix only)
///
/// The connection's socket, to poll for readable (a message, or EOF once
/// the peer closed or exited). Completes a pending `gk_process_for_child`
/// connection first (waits for the child). Owned by `process`: don't close
/// it, and read it only through this API. `-1` with `errno` on failure.
///
/// # Safety
/// `process` must be a live handle (or null).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_fd(process: *mut GkProcess) -> c_int {
    guard(-1, || {
        with(process, -1, |p| match p.unix_stream() {
            Ok(stream) => std::os::fd::AsRawFd::as_raw_fd(stream),
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        })
    })
}

/// `int64_t gk_process_peer_pid(const gk_process *process);`
///
/// The id of the process at the other end, `0` if it is not known (not
/// connected yet, or the OS can't tell), `-1` with `errno = EINVAL` for a
/// null handle.
///
/// # Safety
/// `process` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_peer_pid(process: *const GkProcess) -> i64 {
    guard(-1, || with_ref(process, -1, |p| p.peer_pid().map_or(0, i64::from)))
}

/// `int gk_process_alive(uint32_t pid);` (since 0.3.3)
///
/// `1` while process `pid` runs and whenever the OS can't tell (another
/// user's process, pid 0), `0` once it has exited (an unreaped zombie
/// counts as exited; an own child is never reaped by the check) or does
/// not exist. There is no error return (`errno` may change). See
/// [`crate::process::process_alive`].
#[unsafe(no_mangle)]
pub extern "C" fn gk_process_alive(pid: u32) -> c_int {
    guard(1, || c_int::from(crate::process::process_alive(pid)))
}

// ---------------------------------------------------------------------------
// Channels (GkSender / GkReceiver as opaque C handles)
// ---------------------------------------------------------------------------

use crate::{ChannelOptions, GkChannel, GkReceiver, GkSender};

/// Sets `errno` for a channel error: size problems are `EMSGSIZE`.
fn channel_errno(e: &std::io::Error) {
    use crate::error::errno::EMSGSIZE;
    compat::set_errno(if e.kind() == std::io::ErrorKind::InvalidInput { EMSGSIZE } else { crate::error::to_errno(e) });
}

/// Runs `f` on a live handle, or sets `errno = EINVAL` for null.
fn with<T, R>(p: *mut T, err: R, f: impl FnOnce(&mut T) -> R) -> R {
    // SAFETY: callers pass handles from this API (or null).
    match unsafe { p.as_mut() } {
        Some(v) => f(v),
        None => {
            compat::set_errno(crate::error::errno::EINVAL);
            err
        }
    }
}

/// `gk_channel_options` in C.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GkChannelOptionsC {
    capacity: usize,
    max_message: usize,
    bulk_capacity: usize,
    chunk: usize,
    huge_pages: c_int,
}

impl From<ChannelOptions> for GkChannelOptionsC {
    fn from(o: ChannelOptions) -> Self {
        Self {
            capacity: o.capacity,
            max_message: o.max_message,
            bulk_capacity: o.bulk_capacity,
            chunk: o.chunk,
            huge_pages: huge_to_c(o.huge_pages),
        }
    }
}

/// `void gk_channel_options_default(gk_channel_options *opts);`
///
/// # Safety
/// `opts` must be null or valid for writing a `gk_channel_options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_channel_options_default(opts: *mut GkChannelOptionsC) {
    if !opts.is_null() {
        // SAFETY: checked non-null; the caller guarantees validity.
        unsafe { opts.write(ChannelOptions::default().into()) };
    }
}

/// `int gk_channel_new(const gk_channel_options *opts, gk_sender **tx, gk_receiver **rx);`
/// (`opts` NULL: the defaults)
///
/// # Safety
/// `opts` must be null or valid for reading; `tx` and `rx` valid for writing
/// a pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_channel_new(opts: *const GkChannelOptionsC, tx: *mut *mut GkSender, rx: *mut *mut GkReceiver) -> c_int {
    guard(-1, || {
        if tx.is_null() || rx.is_null() {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        }
        // SAFETY: the caller guarantees `opts` is null or readable.
        let o = match unsafe { opts.as_ref() } {
            Some(o) => {
                let Some(huge_pages) = huge_from_c(o.huge_pages) else {
                    compat::set_errno(crate::error::errno::EINVAL);
                    return -1;
                };
                ChannelOptions {
                    capacity: o.capacity,
                    max_message: o.max_message,
                    bulk_capacity: o.bulk_capacity,
                    chunk: o.chunk,
                    huge_pages,
                }
            }
            None => ChannelOptions::default(),
        };
        match GkChannel::with_options(o) {
            Ok((s, r)) => {
                // SAFETY: checked non-null; the caller guarantees validity.
                unsafe {
                    *tx = Box::into_raw(Box::new(s));
                    *rx = Box::into_raw(Box::new(r));
                }
                0
            }
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        }
    })
}

/// `int gk_sender_send(gk_sender *tx, const void *buf, size_t len);`
/// (kind 0, unspecified)
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_send(tx: *mut GkSender, buf: *const c_void, len: usize) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { gk_sender_send_kind(tx, 0, buf, len) }
}

/// `int gk_sender_send_kind(gk_sender *tx, uint16_t kind, const void *buf, size_t len);`
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_send_kind(tx: *mut GkSender, kind: u16, buf: *const c_void, len: usize) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| {
            let msg = if len == 0 {
                &[][..]
            } else if buf.is_null() {
                compat::set_errno(crate::error::errno::EINVAL);
                return -1;
            } else {
                // SAFETY: the caller guarantees `len` readable bytes.
                unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) }
            };
            match tx.send_kind(kind, msg) {
                Ok(()) => 0,
                Err(e) => {
                    channel_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `intptr_t gk_receiver_recv(gk_receiver *rx, void *buf, size_t cap);`
///
/// # Safety
/// `rx` must be a live handle; `buf` valid for writing `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_recv(rx: *mut GkReceiver, buf: *mut c_void, cap: usize) -> isize {
    // SAFETY: forwarded to the caller.
    unsafe { gk_receiver_recv_kind(rx, std::ptr::null_mut(), buf, cap) }
}

/// `intptr_t gk_receiver_recv_kind(gk_receiver *rx, uint16_t *kind, void *buf, size_t cap);`
///
/// # Safety
/// `rx` must be a live handle; `kind` null or writable; `buf` valid for
/// writing `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_recv_kind(rx: *mut GkReceiver, kind: *mut u16, buf: *mut c_void, cap: usize) -> isize {
    guard(-1, || {
        with(rx, -1, |rx| {
            let out = if cap == 0 {
                &mut [][..]
            } else if buf.is_null() {
                compat::set_errno(crate::error::errno::EINVAL);
                return -1;
            } else {
                // SAFETY: the caller guarantees `cap` writable bytes.
                unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), cap) }
            };
            match rx.recv_kind(out) {
                Ok((k, n)) => {
                    // SAFETY: forwarded to the caller.
                    unsafe { put(kind, k) };
                    n as isize
                }
                Err(e) => {
                    channel_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `intptr_t gk_receiver_next_len(gk_receiver *rx);` (-1 + `EAGAIN` if empty)
///
/// # Safety
/// `rx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_next_len(rx: *mut GkReceiver) -> isize {
    // SAFETY: forwarded to the caller.
    unsafe { gk_receiver_next_kind(rx, std::ptr::null_mut()) }
}

/// `intptr_t gk_receiver_next_kind(gk_receiver *rx, uint16_t *kind);`
/// Like `gk_receiver_next_len`, also storing the next message's kind.
///
/// # Safety
/// `rx` must be a live handle; `kind` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_next_kind(rx: *mut GkReceiver, kind: *mut u16) -> isize {
    guard(-1, || {
        with(rx, -1, |rx| match rx.next_kind() {
            Ok(Some((k, n))) => {
                // SAFETY: forwarded to the caller.
                unsafe { put(kind, k) };
                n as isize
            }
            Ok(None) => {
                compat::set_errno(crate::error::errno::EAGAIN);
                -1
            }
            Err(e) => {
                channel_errno(&e);
                -1
            }
        })
    })
}

/// `int gk_sender_set_nonblocking(gk_sender *tx, int nonblocking);`
///
/// # Safety
/// `tx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_set_nonblocking(tx: *mut GkSender, nonblocking: c_int) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| {
            tx.set_nonblocking(nonblocking != 0);
            0
        })
    })
}

/// `int gk_receiver_set_nonblocking(gk_receiver *rx, int nonblocking);`
///
/// # Safety
/// `rx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_set_nonblocking(rx: *mut GkReceiver, nonblocking: c_int) -> c_int {
    guard(-1, || {
        with(rx, -1, |rx| {
            rx.set_nonblocking(nonblocking != 0);
            0
        })
    })
}

/// `int gk_sender_claim(gk_sender *tx);` (after `fork`)
///
/// # Safety
/// `tx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_claim(tx: *mut GkSender) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| {
            tx.claim();
            0
        })
    })
}

/// `int gk_receiver_claim(gk_receiver *rx);` (after `fork`)
///
/// # Safety
/// `rx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_claim(rx: *mut GkReceiver) -> c_int {
    guard(-1, || {
        with(rx, -1, |rx| {
            rx.claim();
            0
        })
    })
}

/// `int gk_sender_fd(gk_sender *tx);` (unix: readable when space was freed)
///
/// # Safety
/// `tx` must be a live handle.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_fd(tx: *mut GkSender) -> c_int {
    guard(-1, || with(tx, -1, |tx| std::os::fd::AsRawFd::as_raw_fd(&*tx)))
}

/// `int gk_receiver_fd(gk_receiver *rx);` (unix: readable when a message may wait)
///
/// # Safety
/// `rx` must be a live handle.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_fd(rx: *mut GkReceiver) -> c_int {
    guard(-1, || with(rx, -1, |rx| std::os::fd::AsRawFd::as_raw_fd(&*rx)))
}

/// `uintptr_t gk_sender_socket(gk_sender *tx);` (Windows)
///
/// # Safety
/// `tx` must be a live handle.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_socket(tx: *mut GkSender) -> usize {
    guard(usize::MAX, || with(tx, usize::MAX, |tx| std::os::windows::io::AsRawSocket::as_raw_socket(&*tx) as usize))
}

/// `uintptr_t gk_receiver_socket(gk_receiver *rx);` (Windows)
///
/// # Safety
/// `rx` must be a live handle.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_socket(rx: *mut GkReceiver) -> usize {
    guard(usize::MAX, || with(rx, usize::MAX, |rx| std::os::windows::io::AsRawSocket::as_raw_socket(&*rx) as usize))
}

/// `void gk_sender_free(gk_sender *tx);` (closes the sending side; NULL ok)
///
/// # Safety
/// `tx` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_free(tx: *mut GkSender) {
    if !tx.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(tx) }));
    }
}

/// `void gk_receiver_free(gk_receiver *rx);` (closes the receiving side; NULL ok)
///
/// # Safety
/// `rx` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_free(rx: *mut GkReceiver) {
    if !rx.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(rx) }));
    }
}

/// `void gk_sender_detach(gk_sender *tx);` (frees without closing; after `fork`)
///
/// # Safety
/// `tx` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_detach(tx: *mut GkSender) {
    if !tx.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || unsafe { Box::from_raw(tx) }.detach());
    }
}

/// `void gk_receiver_detach(gk_receiver *rx);` (frees without closing; after `fork`)
///
/// # Safety
/// `rx` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_detach(rx: *mut GkReceiver) {
    if !rx.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || unsafe { Box::from_raw(rx) }.detach());
    }
}

/// Writes `id` through an optional out pointer.
///
/// # Safety
/// `out` must be null or valid for writing.
unsafe fn put<T>(out: *mut T, v: T) {
    if !out.is_null() {
        // SAFETY: forwarded.
        unsafe { out.write(v) };
    }
}

/// Bytes `buf`/`len` from C (empty if `len` is 0), or `EINVAL` for null.
///
/// # Safety
/// `buf` must be valid for reading `len` bytes.
unsafe fn c_bytes<'a>(buf: *const c_void, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if buf.is_null() {
        compat::set_errno(crate::error::errno::EINVAL);
        return None;
    }
    // SAFETY: forwarded.
    Some(unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) })
}

/// Sets `errno` for a bulk error (`ECONNABORTED`, `EPIPE`, ...).
fn bulk_errno(e: &std::io::Error) {
    compat::set_errno(crate::error::to_errno(e));
}

/// `int gk_sender_send_bulk(gk_sender *tx, const void *buf, size_t len, uint64_t *id);`
///
/// # Safety
/// `tx` live; `buf` readable for `len` bytes; `id` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_send_bulk(tx: *mut GkSender, buf: *const c_void, len: usize, id: *mut u64) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { gk_sender_send_bulk_kind(tx, 0, buf, len, id) }
}

/// `int gk_sender_send_bulk_kind(gk_sender *tx, uint16_t kind, const void *buf, size_t len, uint64_t *id);`
///
/// # Safety
/// As `gk_sender_send_bulk`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_send_bulk_kind(tx: *mut GkSender, kind: u16, buf: *const c_void, len: usize, id: *mut u64) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| {
            // SAFETY: forwarded to the caller.
            let Some(data) = (unsafe { c_bytes(buf, len) }) else {
                return -1;
            };
            match tx.send_bulk_kind(kind, data) {
                Ok(v) => {
                    // SAFETY: forwarded to the caller.
                    unsafe { put(id, v) };
                    0
                }
                Err(e) => {
                    bulk_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `int gk_sender_bulk_begin(gk_sender *tx, int64_t len, uint64_t *id);`
/// (`len < 0`: unknown length, ended by `gk_sender_bulk_finish`)
///
/// # Safety
/// `tx` live; `id` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_bulk_begin(tx: *mut GkSender, len: i64, id: *mut u64) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { gk_sender_bulk_begin_kind(tx, 0, len, id) }
}

/// `int gk_sender_bulk_begin_kind(gk_sender *tx, uint16_t kind, int64_t len, uint64_t *id);`
///
/// # Safety
/// As `gk_sender_bulk_begin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_bulk_begin_kind(tx: *mut GkSender, kind: u16, len: i64, id: *mut u64) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| {
            match tx.bulk_begin(kind, u64::try_from(len).ok()) {
                Ok(v) => {
                    // SAFETY: forwarded to the caller.
                    unsafe { put(id, v) };
                    0
                }
                Err(e) => {
                    bulk_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `intptr_t gk_sender_bulk_write(gk_sender *tx, const void *buf, size_t len);`
///
/// # Safety
/// `tx` live; `buf` readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_bulk_write(tx: *mut GkSender, buf: *const c_void, len: usize) -> isize {
    guard(-1, || {
        with(tx, -1, |tx| {
            // SAFETY: forwarded to the caller.
            let Some(data) = (unsafe { c_bytes(buf, len) }) else {
                return -1;
            };
            match tx.bulk_write(data) {
                Ok(n) => n as isize,
                Err(e) => {
                    bulk_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `int gk_sender_bulk_finish(gk_sender *tx);`
///
/// # Safety
/// `tx` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_bulk_finish(tx: *mut GkSender) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| match tx.bulk_finish() {
            Ok(()) => 0,
            Err(e) => {
                bulk_errno(&e);
                -1
            }
        })
    })
}

/// `int gk_sender_bulk_abort(gk_sender *tx);`
///
/// # Safety
/// `tx` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_bulk_abort(tx: *mut GkSender) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| match tx.bulk_abort() {
            Ok(()) => 0,
            Err(e) => {
                bulk_errno(&e);
                -1
            }
        })
    })
}

/// `int gk_receiver_bulk_begin(gk_receiver *rx, int64_t *len, uint64_t *id);`
/// (`*len` is -1 for an unknown length)
///
/// # Safety
/// `rx` live; `len`, `id` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_bulk_begin(rx: *mut GkReceiver, len: *mut i64, id: *mut u64) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { gk_receiver_bulk_begin_kind(rx, std::ptr::null_mut(), len, id) }
}

/// `int gk_receiver_bulk_begin_kind(gk_receiver *rx, uint16_t *kind, int64_t *len, uint64_t *id);`
///
/// # Safety
/// `rx` live; `kind`, `len`, `id` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_bulk_begin_kind(rx: *mut GkReceiver, kind: *mut u16, len: *mut i64, id: *mut u64) -> c_int {
    guard(-1, || {
        with(rx, -1, |rx| match rx.bulk_begin() {
            Ok((v, total)) => {
                let k = rx.bulk_info().map_or(0, |i| i.3);
                // SAFETY: forwarded to the caller.
                unsafe {
                    put(kind, k);
                    put(id, v);
                    put(len, total.map_or(-1, |t| i64::try_from(t).unwrap_or(i64::MAX)));
                }
                0
            }
            Err(e) => {
                bulk_errno(&e);
                -1
            }
        })
    })
}

/// `intptr_t gk_receiver_bulk_read(gk_receiver *rx, void *buf, size_t cap);`
/// (0 at the end of the transfer)
///
/// # Safety
/// `rx` live; `buf` writable for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_bulk_read(rx: *mut GkReceiver, buf: *mut c_void, cap: usize) -> isize {
    guard(-1, || {
        with(rx, -1, |rx| {
            let out = if cap == 0 {
                &mut [][..]
            } else if buf.is_null() {
                compat::set_errno(crate::error::errno::EINVAL);
                return -1;
            } else {
                // SAFETY: the caller guarantees `cap` writable bytes.
                unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), cap) }
            };
            match rx.bulk_read(out) {
                Ok(n) => n as isize,
                Err(e) => {
                    bulk_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `int gk_receiver_bulk_end(gk_receiver *rx);` (discards the unread rest)
///
/// # Safety
/// `rx` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_bulk_end(rx: *mut GkReceiver) -> c_int {
    guard(-1, || {
        with(rx, -1, |rx| {
            rx.bulk_end();
            0
        })
    })
}

/// `int gk_process_send_sender(gk_process *p, gk_sender *tx);`
///
/// # Safety
/// Both must be live handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send_sender(p: *mut GkProcess, tx: *mut GkSender) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        let Some(tx) = (unsafe { tx.as_ref() }) else {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        };
        with(p, -1, |p| match p.send(tx) {
            Ok(()) => 0,
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        })
    })
}

/// `int gk_process_send_receiver(gk_process *p, gk_receiver *rx);`
///
/// # Safety
/// Both must be live handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send_receiver(p: *mut GkProcess, rx: *mut GkReceiver) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        let Some(rx) = (unsafe { rx.as_ref() }) else {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        };
        with(p, -1, |p| match p.send(rx) {
            Ok(()) => 0,
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        })
    })
}

/// `gk_sender *gk_process_recv_sender(gk_process *p);`
///
/// # Safety
/// `p` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_sender(p: *mut GkProcess) -> *mut GkSender {
    guard(core::ptr::null_mut(), || with(p, core::ptr::null_mut(), |p| boxed(p.recv_as::<GkSender>())))
}

/// `gk_receiver *gk_process_recv_receiver(gk_process *p);`
///
/// # Safety
/// `p` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_receiver(p: *mut GkProcess) -> *mut GkReceiver {
    guard(core::ptr::null_mut(), || with(p, core::ptr::null_mut(), |p| boxed(p.recv_as::<GkReceiver>())))
}

// ---------------------------------------------------------------------------
// Shared memory (GkSharedMemory as an opaque C handle)
// ---------------------------------------------------------------------------

use crate::{GkSharedMemory, HugePages, SharedMemoryOptions};

/// Like `with` for a handle used through a shared reference (`const` in
/// C, may be used from several threads at once).
fn with_ref<T, R>(p: *const T, err: R, f: impl FnOnce(&T) -> R) -> R {
    // SAFETY: callers pass handles from this API (or null).
    match unsafe { p.as_ref() } {
        Some(v) => f(v),
        None => {
            compat::set_errno(crate::error::errno::EINVAL);
            err
        }
    }
}

/// `GK_HUGE_PAGES_*` in C.
const GK_HUGE_PAGES_OFF: c_int = 0;
const GK_HUGE_PAGES_PREFER: c_int = 1;
const GK_HUGE_PAGES_REQUIRE: c_int = 2;

fn huge_to_c(h: HugePages) -> c_int {
    match h {
        HugePages::Off => GK_HUGE_PAGES_OFF,
        HugePages::Prefer => GK_HUGE_PAGES_PREFER,
        HugePages::Require => GK_HUGE_PAGES_REQUIRE,
    }
}

/// `None` for an unknown value.
fn huge_from_c(h: c_int) -> Option<HugePages> {
    match h {
        GK_HUGE_PAGES_OFF => Some(HugePages::Off),
        GK_HUGE_PAGES_PREFER => Some(HugePages::Prefer),
        GK_HUGE_PAGES_REQUIRE => Some(HugePages::Require),
        _ => None,
    }
}

/// `gk_shm_options` in C.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GkShmOptionsC {
    huge_pages: c_int,
    /// 0: the system default.
    huge_page_size: usize,
    populate: c_int,
}

impl From<SharedMemoryOptions> for GkShmOptionsC {
    fn from(o: SharedMemoryOptions) -> Self {
        Self { huge_pages: huge_to_c(o.huge_pages), huge_page_size: o.huge_page_size.unwrap_or(0), populate: c_int::from(o.populate) }
    }
}

impl GkShmOptionsC {
    /// The Rust options, or `None` for an unknown `huge_pages` value.
    fn to_options(self) -> Option<SharedMemoryOptions> {
        let huge_pages = huge_from_c(self.huge_pages)?;
        Some(SharedMemoryOptions {
            huge_pages,
            huge_page_size: (self.huge_page_size != 0).then_some(self.huge_page_size),
            populate: self.populate != 0,
        })
    }
}

/// `void gk_shm_options_default(gk_shm_options *opts);`
///
/// # Safety
/// `opts` must be null or valid for writing a `gk_shm_options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_options_default(opts: *mut GkShmOptionsC) {
    // SAFETY: forwarded to the caller.
    unsafe { put(opts, SharedMemoryOptions::default().into()) };
}

/// `gk_shm *gk_shm_new(size_t len);`
#[unsafe(no_mangle)]
pub extern "C" fn gk_shm_new(len: usize) -> *mut GkSharedMemory {
    guard(core::ptr::null_mut(), || boxed(GkSharedMemory::new(len)))
}

/// `gk_shm *gk_shm_new_ex(size_t len, const gk_shm_options *opts);`
/// (`opts` NULL: the defaults)
///
/// # Safety
/// `opts` must be null or valid for reading a `gk_shm_options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_new_ex(len: usize, opts: *const GkShmOptionsC) -> *mut GkSharedMemory {
    guard(core::ptr::null_mut(), || {
        // SAFETY: the caller guarantees `opts` is null or readable.
        let opts = match unsafe { opts.as_ref() } {
            None => SharedMemoryOptions::default(),
            Some(o) => match o.to_options() {
                Some(o) => o,
                None => {
                    compat::set_errno(crate::error::errno::EINVAL);
                    return core::ptr::null_mut();
                }
            },
        };
        boxed(GkSharedMemory::with_options(len, opts))
    })
}

/// `void *gk_shm_ptr(const gk_shm *shm);` (NULL + `EINVAL` for NULL)
///
/// # Safety
/// `shm` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_ptr(shm: *const GkSharedMemory) -> *mut c_void {
    guard(core::ptr::null_mut(), || with_ref(shm, core::ptr::null_mut(), |s| s.as_ptr().cast::<c_void>()))
}

/// `size_t gk_shm_len(const gk_shm *shm);` (0 + `EINVAL` for NULL)
///
/// # Safety
/// `shm` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_len(shm: *const GkSharedMemory) -> usize {
    guard(0, || with_ref(shm, 0, |s| s.len()))
}

/// `size_t gk_shm_page_size(const gk_shm *shm);` (0 + `EINVAL` for NULL)
///
/// # Safety
/// `shm` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_page_size(shm: *const GkSharedMemory) -> usize {
    guard(0, || with_ref(shm, 0, |s| s.page_size()))
}

/// `int gk_shm_is_huge(const gk_shm *shm);` (1, 0, or -1 + `EINVAL`)
///
/// # Safety
/// `shm` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_is_huge(shm: *const GkSharedMemory) -> c_int {
    guard(-1, || with_ref(shm, -1, |s| c_int::from(s.is_huge())))
}

/// `int gk_shm_fd(const gk_shm *shm);` (unix: the shared memory object,
/// still owned by `shm`)
///
/// # Safety
/// `shm` must be a live handle (or null).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_fd(shm: *const GkSharedMemory) -> c_int {
    guard(-1, || with_ref(shm, -1, |s| std::os::fd::AsRawFd::as_raw_fd(&std::os::fd::AsFd::as_fd(s))))
}

/// `void *gk_shm_handle(const gk_shm *shm);` (Windows: the section handle,
/// still owned by `shm`)
///
/// # Safety
/// `shm` must be a live handle (or null).
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_handle(shm: *const GkSharedMemory) -> *mut c_void {
    guard(core::ptr::null_mut(), || {
        with_ref(shm, core::ptr::null_mut(), |s| {
            std::os::windows::io::AsRawHandle::as_raw_handle(&std::os::windows::io::AsHandle::as_handle(s)).cast()
        })
    })
}

/// `void gk_shm_free(gk_shm *shm);` (unmaps this process's view; NULL ok)
///
/// # Safety
/// `shm` must be a handle from this API, freed only once, with no pointer
/// into the memory used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_free(shm: *mut GkSharedMemory) {
    if !shm.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(shm) }));
    }
}

/// `int gk_process_send_shm(gk_process *p, const gk_shm *shm);`
///
/// # Safety
/// Both must be live handles (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send_shm(p: *mut GkProcess, shm: *const GkSharedMemory) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        let Some(shm) = (unsafe { shm.as_ref() }) else {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        };
        with(p, -1, |p| match p.send(shm) {
            Ok(()) => 0,
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        })
    })
}

/// `gk_shm *gk_process_recv_shm(gk_process *p);`
///
/// # Safety
/// `p` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_shm(p: *mut GkProcess) -> *mut GkSharedMemory {
    guard(core::ptr::null_mut(), || {
        with(p, core::ptr::null_mut(), |p| {
            // Something else sent (or a bad offer): EINVAL, as gk_process_recv.
            boxed(p.recv_as::<GkSharedMemory>().map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => crate::error::invalid_input(),
                _ => e,
            }))
        })
    })
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
        return crt_call(|| unsafe { _dup(fd) });
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
        let r = crt_call(|| unsafe { _dup2(oldfd, newfd) });
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

// ---------------------------------------------------------------------------
// Multi-producer channels, batch receive, peer liveness
// ---------------------------------------------------------------------------

use crate::GkMpSender;

/// Reads `len` bytes at `buf` as a message (NULL is fine for `len == 0`).
///
/// # Safety
/// `buf` must be valid for reading `len` bytes when `len > 0`.
unsafe fn message<'a>(buf: *const c_void, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        Some(&[])
    } else if buf.is_null() {
        compat::set_errno(crate::error::errno::EINVAL);
        None
    } else {
        // SAFETY: forwarded to the caller.
        Some(unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) })
    }
}

/// `0` or `-1` with `errno` (channel rules: size problems are `EMSGSIZE`).
fn channel_status(r: std::io::Result<()>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => {
            channel_errno(&e);
            -1
        }
    }
}

/// `int gk_channel_new_mpsc(const gk_channel_options *opts, gk_mp_sender **tx, gk_receiver **rx);`
/// (`opts` NULL: the defaults)
///
/// # Safety
/// `opts` must be null or valid for reading; `tx` and `rx` valid for writing
/// a pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_channel_new_mpsc(opts: *const GkChannelOptionsC, tx: *mut *mut GkMpSender, rx: *mut *mut GkReceiver) -> c_int {
    guard(-1, || {
        if tx.is_null() || rx.is_null() {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        }
        // SAFETY: the caller guarantees `opts` is null or readable.
        let o = match unsafe { opts.as_ref() } {
            Some(o) => {
                let Some(huge_pages) = huge_from_c(o.huge_pages) else {
                    compat::set_errno(crate::error::errno::EINVAL);
                    return -1;
                };
                ChannelOptions {
                    capacity: o.capacity,
                    max_message: o.max_message,
                    bulk_capacity: o.bulk_capacity,
                    chunk: o.chunk,
                    huge_pages,
                }
            }
            None => ChannelOptions::default(),
        };
        match GkChannel::mpsc_with_options(o) {
            Ok((s, r)) => {
                // SAFETY: checked non-null; the caller guarantees validity.
                unsafe {
                    *tx = Box::into_raw(Box::new(s));
                    *rx = Box::into_raw(Box::new(r));
                }
                0
            }
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        }
    })
}

/// `gk_mp_sender *gk_mp_sender_clone(const gk_mp_sender *tx);`
///
/// # Safety
/// `tx` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_clone(tx: *const GkMpSender) -> *mut GkMpSender {
    guard(core::ptr::null_mut(), || with_ref(tx, core::ptr::null_mut(), |tx| Box::into_raw(Box::new(tx.clone()))))
}

/// `int gk_mp_sender_send(const gk_mp_sender *tx, const void *buf, size_t len);`
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_send(tx: *const GkMpSender, buf: *const c_void, len: usize) -> c_int {
    // SAFETY: forwarded to the caller.
    unsafe { gk_mp_sender_send_kind(tx, 0, buf, len) }
}

/// `int gk_mp_sender_send_kind(const gk_mp_sender *tx, uint16_t kind, const void *buf, size_t len);`
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_send_kind(tx: *const GkMpSender, kind: u16, buf: *const c_void, len: usize) -> c_int {
    guard(-1, || {
        with_ref(tx, -1, |tx| {
            // SAFETY: the caller guarantees `len` readable bytes.
            let Some(msg) = (unsafe { message(buf, len) }) else { return -1 };
            channel_status(tx.send_kind(kind, msg))
        })
    })
}

/// `int gk_mp_sender_try_send_kind(const gk_mp_sender *tx, uint16_t kind, const void *buf, size_t len);`
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_try_send_kind(tx: *const GkMpSender, kind: u16, buf: *const c_void, len: usize) -> c_int {
    guard(-1, || {
        with_ref(tx, -1, |tx| {
            // SAFETY: the caller guarantees `len` readable bytes.
            let Some(msg) = (unsafe { message(buf, len) }) else { return -1 };
            channel_status(tx.try_send_kind(kind, msg))
        })
    })
}

/// `int gk_mp_sender_send_bulk_kind(const gk_mp_sender *tx, uint16_t kind, const void *buf, size_t len, uint64_t *id);`
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes; `id`
/// null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_send_bulk_kind(
    tx: *const GkMpSender,
    kind: u16,
    buf: *const c_void,
    len: usize,
    id: *mut u64,
) -> c_int {
    guard(-1, || {
        with_ref(tx, -1, |tx| {
            // SAFETY: the caller guarantees `len` readable bytes.
            let Some(data) = (unsafe { message(buf, len) }) else { return -1 };
            match tx.bulk().send_bulk_kind(kind, data) {
                Ok(v) => {
                    // SAFETY: the caller guarantees `id` is null or writable.
                    if let Some(id) = unsafe { id.as_mut() } {
                        *id = v;
                    }
                    0
                }
                Err(e) => {
                    channel_errno(&e);
                    -1
                }
            }
        })
    })
}

/// `int gk_mp_sender_set_nonblocking(const gk_mp_sender *tx, int nonblocking);` (every clone)
///
/// # Safety
/// `tx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_set_nonblocking(tx: *const GkMpSender, nonblocking: c_int) -> c_int {
    guard(-1, || {
        with_ref(tx, -1, |tx| {
            tx.set_nonblocking(nonblocking != 0);
            0
        })
    })
}

/// `int gk_mp_sender_claim(const gk_mp_sender *tx);` (after `fork`)
///
/// # Safety
/// `tx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_claim(tx: *const GkMpSender) -> c_int {
    guard(-1, || {
        with_ref(tx, -1, |tx| {
            tx.claim();
            0
        })
    })
}

/// `int gk_mp_sender_fd(const gk_mp_sender *tx);` (unix: readable when space was freed)
///
/// # Safety
/// `tx` must be a live handle.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_fd(tx: *const GkMpSender) -> c_int {
    guard(-1, || with_ref(tx, -1, std::os::fd::AsRawFd::as_raw_fd))
}

/// `uintptr_t gk_mp_sender_socket(const gk_mp_sender *tx);` (Windows)
///
/// # Safety
/// `tx` must be a live handle.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_socket(tx: *const GkMpSender) -> usize {
    guard(usize::MAX, || with_ref(tx, usize::MAX, |tx| std::os::windows::io::AsRawSocket::as_raw_socket(tx) as usize))
}

/// `int gk_mp_sender_peer_gone(const gk_mp_sender *tx);` (1: the receiver is gone)
///
/// # Safety
/// `tx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_peer_gone(tx: *const GkMpSender) -> c_int {
    guard(-1, || with_ref(tx, -1, |tx| c_int::from(tx.peer_gone())))
}

/// `int gk_sender_peer_gone(gk_sender *tx);` (1: the receiver is gone)
///
/// # Safety
/// `tx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_peer_gone(tx: *mut GkSender) -> c_int {
    guard(-1, || with(tx, -1, |tx| c_int::from(tx.peer_gone())))
}

/// `int gk_receiver_peer_gone(gk_receiver *rx);` (1: the sender is gone)
///
/// # Safety
/// `rx` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_peer_gone(rx: *mut GkReceiver) -> c_int {
    guard(-1, || with(rx, -1, |rx| c_int::from(rx.peer_gone())))
}

/// `void gk_mp_sender_free(gk_mp_sender *tx);` (the channel closes with the last handle; NULL ok)
///
/// # Safety
/// `tx` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_free(tx: *mut GkMpSender) {
    if !tx.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(tx) }));
    }
}

/// `void gk_mp_sender_detach(gk_mp_sender *tx);` (frees; no handle of this process closes the channel)
///
/// # Safety
/// `tx` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_mp_sender_detach(tx: *mut GkMpSender) {
    if !tx.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || unsafe { Box::from_raw(tx) }.detach());
    }
}

/// `int gk_process_send_mp_sender(gk_process *p, const gk_mp_sender *tx);`
///
/// # Safety
/// Both must be live handles (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send_mp_sender(p: *mut GkProcess, tx: *const GkMpSender) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        let Some(tx) = (unsafe { tx.as_ref() }) else {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        };
        with(p, -1, |p| match p.send(tx) {
            Ok(()) => 0,
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        })
    })
}

/// `gk_mp_sender *gk_process_recv_mp_sender(gk_process *p);`
///
/// # Safety
/// `p` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_mp_sender(p: *mut GkProcess) -> *mut GkMpSender {
    guard(core::ptr::null_mut(), || with(p, core::ptr::null_mut(), |p| boxed(p.recv_as::<GkMpSender>())))
}

/// `gk_recv_fn`: `void (*)(void *ctx, uint16_t kind, const void *msg, size_t len)`.
pub type GkRecvFn = unsafe extern "C" fn(ctx: *mut c_void, kind: u16, msg: *const c_void, len: usize);

/// `intptr_t gk_receiver_recv_batch(gk_receiver *rx, size_t max, gk_recv_fn f, void *ctx);`
///
/// # Safety
/// `rx` must be a live handle; `f` a valid function that does not unwind,
/// called with `ctx` and a message valid only during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_recv_batch(rx: *mut GkReceiver, max: usize, f: Option<GkRecvFn>, ctx: *mut c_void) -> isize {
    guard(-1, || {
        let Some(f) = f else {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        };
        with(rx, -1, |rx| {
            match rx.recv_batch(max, |kind, msg| {
                // SAFETY: forwarded to the caller; `msg` lives for the call.
                unsafe { f(ctx, kind, msg.as_ptr().cast(), msg.len()) }
            }) {
                Ok(n) => n as isize,
                Err(e) => {
                    channel_errno(&e);
                    -1
                }
            }
        })
    })
}

// ---------------------------------------------------------------------------
// Shared memory pools (zero-copy lending)
// ---------------------------------------------------------------------------

use crate::{GkShmPool, GkShmPoolView, ShmBlock, ShmBuf, ShmLease, ShmPoolOptions, ShmPoolStats};

/// `gk_shm_pool_options` in C.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GkShmPoolOptionsC {
    size: usize,
    granule: usize,
    huge_pages: c_int,
}

/// `gk_shm_lease` in C.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GkShmLeaseC {
    index: u32,
    generation: u32,
    len: u64,
}

/// `gk_shm_pool_stats` in C.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GkShmPoolStatsC {
    size: usize,
    granule: usize,
    free: usize,
    allocated: usize,
    lent: usize,
    bad_returns: u64,
}

impl From<ShmPoolStats> for GkShmPoolStatsC {
    fn from(s: ShmPoolStats) -> Self {
        Self { size: s.size, granule: s.granule, free: s.free, allocated: s.allocated, lent: s.lent, bad_returns: s.bad_returns }
    }
}

/// `void gk_shm_pool_options_default(gk_shm_pool_options *opts);`
///
/// # Safety
/// `opts` must be null or valid for writing a `gk_shm_pool_options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_options_default(opts: *mut GkShmPoolOptionsC) {
    if !opts.is_null() {
        let d = ShmPoolOptions::default();
        // SAFETY: checked non-null; the caller guarantees validity.
        unsafe { opts.write(GkShmPoolOptionsC { size: d.size, granule: d.granule, huge_pages: huge_to_c(d.huge_pages) }) };
    }
}

/// `gk_shm_pool *gk_shm_pool_new(const gk_shm_pool_options *opts);` (NULL: the defaults)
///
/// # Safety
/// `opts` must be null or valid for reading.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_new(opts: *const GkShmPoolOptionsC) -> *mut GkShmPool {
    guard(core::ptr::null_mut(), || {
        // SAFETY: the caller guarantees `opts` is null or readable.
        let o = match unsafe { opts.as_ref() } {
            Some(o) => {
                let Some(huge_pages) = huge_from_c(o.huge_pages) else {
                    compat::set_errno(crate::error::errno::EINVAL);
                    return core::ptr::null_mut();
                };
                ShmPoolOptions { size: o.size, granule: o.granule, huge_pages }
            }
            None => ShmPoolOptions::default(),
        };
        boxed(GkShmPool::with_options(o))
    })
}

/// `gk_shm_block *gk_shm_pool_alloc(const gk_shm_pool *pool, size_t len);`
/// (NULL + `ENOMEM` when exhausted, `EINVAL` if it can never fit)
///
/// # Safety
/// `pool` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_alloc(pool: *const GkShmPool, len: usize) -> *mut ShmBlock {
    guard(core::ptr::null_mut(), || with_ref(pool, core::ptr::null_mut(), |p| boxed(p.alloc(len))))
}

/// `int gk_shm_pool_get_stats(const gk_shm_pool *pool, gk_shm_pool_stats *stats);`
///
/// # Safety
/// `pool` must be a live handle; `stats` valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_get_stats(pool: *const GkShmPool, stats: *mut GkShmPoolStatsC) -> c_int {
    guard(-1, || {
        if stats.is_null() {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        }
        with_ref(pool, -1, |p| {
            // SAFETY: checked non-null; the caller guarantees validity.
            unsafe { stats.write(p.stats().into()) };
            0
        })
    })
}

/// `int gk_shm_pool_reclaim_all(const gk_shm_pool *pool);`
///
/// # Safety
/// `pool` must be a live handle, and no borrower may still use a lent block.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_reclaim_all(pool: *const GkShmPool) -> c_int {
    guard(-1, || {
        with_ref(pool, -1, |p| {
            // SAFETY: forwarded to the caller.
            unsafe { p.reclaim_all() };
            0
        })
    })
}

/// `void gk_shm_pool_free(gk_shm_pool *pool);` (blocks and views keep the memory; NULL ok)
///
/// # Safety
/// `pool` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_free(pool: *mut GkShmPool) {
    if !pool.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(pool) }));
    }
}

/// `void *gk_shm_block_ptr(gk_shm_block *block);` (writable, `capacity` bytes)
///
/// # Safety
/// `block` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_block_ptr(block: *mut ShmBlock) -> *mut c_void {
    guard(core::ptr::null_mut(), || with(block, core::ptr::null_mut(), |b| b.as_mut_slice().as_mut_ptr().cast()))
}

/// `size_t gk_shm_block_capacity(const gk_shm_block *block);`
///
/// # Safety
/// `block` must be a live handle (or null: returns 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_block_capacity(block: *const ShmBlock) -> usize {
    guard(0, || with_ref(block, 0, ShmBlock::capacity))
}

/// `int gk_shm_block_lend(gk_shm_block *block, size_t len, gk_shm_lease *lease);`
///
/// Frees the handle on success (the block now belongs to the borrower).
///
/// # Safety
/// `block` must be a live handle; `lease` valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_block_lend(block: *mut ShmBlock, len: usize, lease: *mut GkShmLeaseC) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        let bad = lease.is_null() || block.is_null() || len > unsafe { (*block).capacity() };
        if bad {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        }
        // SAFETY: the caller hands back a box we created.
        let l = unsafe { Box::from_raw(block) }.lend(len);
        // SAFETY: checked non-null; the caller guarantees validity.
        unsafe { lease.write(GkShmLeaseC { index: l.index, generation: l.generation, len: l.len }) };
        0
    })
}

/// `void gk_shm_block_free(gk_shm_block *block);` (frees an unlent block; NULL ok)
///
/// # Safety
/// `block` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_block_free(block: *mut ShmBlock) {
    if !block.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(block) }));
    }
}

/// `gk_shm_pool_view *gk_shm_pool_new_view(const gk_shm_pool *pool);`
///
/// # Safety
/// `pool` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_new_view(pool: *const GkShmPool) -> *mut GkShmPoolView {
    guard(core::ptr::null_mut(), || with_ref(pool, core::ptr::null_mut(), |p| Box::into_raw(Box::new(p.view()))))
}

/// `gk_shm_buf *gk_shm_pool_view_take(const gk_shm_pool_view *view, const gk_shm_lease *lease);`
///
/// # Safety
/// `view` must be a live handle; `lease` readable, lent by this pool, and
/// taken once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_view_take(view: *const GkShmPoolView, lease: *const GkShmLeaseC) -> *mut ShmBuf {
    guard(core::ptr::null_mut(), || {
        // SAFETY: the caller guarantees `lease` is null or readable.
        let Some(l) = (unsafe { lease.as_ref() }) else {
            compat::set_errno(crate::error::errno::EINVAL);
            return core::ptr::null_mut();
        };
        let l = ShmLease { index: l.index, generation: l.generation, len: l.len };
        // SAFETY: forwarded to the caller (lent by this pool, taken once).
        with_ref(view, core::ptr::null_mut(), |v| boxed(unsafe { v.take(l) }.map_err(|_| crate::error::invalid_input())))
    })
}

/// `void gk_shm_pool_view_free(gk_shm_pool_view *view);` (NULL ok)
///
/// # Safety
/// `view` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_pool_view_free(view: *mut GkShmPoolView) {
    if !view.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(view) }));
    }
}

/// `const void *gk_shm_buf_ptr(const gk_shm_buf *buf);`
///
/// # Safety
/// `buf` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_buf_ptr(buf: *const ShmBuf) -> *const c_void {
    guard(core::ptr::null(), || with_ref(buf, core::ptr::null(), |b| b.as_ptr().cast()))
}

/// `size_t gk_shm_buf_len(const gk_shm_buf *buf);`
///
/// # Safety
/// `buf` must be a live handle (or null: returns 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_buf_len(buf: *const ShmBuf) -> usize {
    guard(0, || with_ref(buf, 0, |b| b.len()))
}

/// `void gk_shm_buf_free(gk_shm_buf *buf);` (gives the block back; NULL ok)
///
/// # Safety
/// `buf` must be a handle from this API, freed only once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_shm_buf_free(buf: *mut ShmBuf) {
    if !buf.is_null() {
        // SAFETY: the caller hands back a box we created.
        guard((), || drop(unsafe { Box::from_raw(buf) }));
    }
}

/// `int gk_process_send_shm_pool_view(gk_process *p, const gk_shm_pool_view *view);`
///
/// # Safety
/// Both must be live handles (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_send_shm_pool_view(p: *mut GkProcess, view: *const GkShmPoolView) -> c_int {
    guard(-1, || {
        // SAFETY: the caller guarantees a live handle (or null).
        let Some(view) = (unsafe { view.as_ref() }) else {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        };
        with(p, -1, |p| match p.send(view) {
            Ok(()) => 0,
            Err(e) => {
                compat::set_errno(crate::error::to_errno(&e));
                -1
            }
        })
    })
}

/// `gk_shm_pool_view *gk_process_recv_shm_pool_view(gk_process *p);`
///
/// # Safety
/// `p` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_shm_pool_view(p: *mut GkProcess) -> *mut GkShmPoolView {
    guard(core::ptr::null_mut(), || {
        with(p, core::ptr::null_mut(), |p| {
            boxed(p.recv_as::<GkShmPoolView>().map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => crate::error::invalid_input(),
                _ => e,
            }))
        })
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

    /// Sends an eventfd from this thread to another over `p`/`name`.
    fn round_trip(p: *mut GkProcess, connect: std::ffi::CString) {
        let t = std::thread::spawn(move || {
            // SAFETY: valid C strings and handles from this API.
            unsafe {
                let q = gk_process_connect(connect.as_ptr());
                assert!(!q.is_null(), "connect: errno {}", compat::errno());
                let fd = gk_process_recv(q);
                assert!(fd >= 0, "recv: errno {}", compat::errno());
                assert_eq!(gk_eventfd_write(fd, 6), 0);
                assert_eq!(gk_eventfd_close(fd), 0);
                gk_process_free(q);
            }
        });
        // SAFETY: handles from this API.
        unsafe {
            let fd = gk_eventfd(0, 0);
            assert!(fd >= 0);
            assert_eq!(gk_process_send(p, fd), 0, "send: errno {}", compat::errno());
            let mut v = 0;
            assert_eq!(gk_eventfd_read(fd, &mut v), 0);
            assert_eq!(v, 6);
            t.join().unwrap();
            assert_eq!(gk_eventfd_close(fd), 0);
            gk_process_free(p);
        }
    }

    #[test]
    fn process_api_listener_and_connect() {
        let _g = compat::tests::lock();
        let name = std::ffi::CString::new(format!("ffi-{}", std::process::id())).unwrap();
        // SAFETY: valid C string; handles from this API.
        unsafe {
            let l = gk_listener_bind(name.as_ptr());
            assert!(!l.is_null(), "bind: errno {}", compat::errno());
            assert!(gk_listener_bind(name.as_ptr()).is_null());
            assert_eq!(compat::errno(), crate::error::errno::EADDRINUSE);
            // Accept on a helper thread (the pointer travels as an address)
            // while this thread connects.
            let l_addr = l as usize;
            // SAFETY (closure): `l` stays alive until this thread is joined.
            let accepting = std::thread::spawn(move || gk_listener_accept(l_addr as *mut GkListener) as usize);
            let q = gk_process_connect(name.as_ptr());
            assert!(!q.is_null());
            let p = accepting.join().unwrap() as *mut GkProcess;
            assert!(!p.is_null());
            // p (accepted) sends to q (connected).
            let fd = gk_eventfd(0, EFD_NONBLOCK_FOR_TESTS);
            assert_eq!(gk_process_send(p, fd), 0, "send: errno {}", compat::errno());
            let got = gk_process_recv(q);
            assert!(got >= 0, "recv: errno {}", compat::errno());
            assert_eq!(gk_eventfd_write(got, 4), 0);
            let mut v = 0;
            assert_eq!(gk_eventfd_read(fd, &mut v), 0);
            assert_eq!(v, 4);
            for x in [fd, got] {
                assert_eq!(gk_eventfd_close(x), 0);
            }
            gk_process_free(p);
            gk_process_free(q);
            gk_listener_free(l);
        }
    }

    /// `gk_listener_bind_ex`: both access values work for this user; an
    /// unknown one is `EINVAL`. (That other users are kept out can't be
    /// tested from one account.)
    #[test]
    fn process_api_listener_access() {
        let _g = compat::tests::lock();
        for access in [GK_LISTENER_ACCESS_DEFAULT, GK_LISTENER_ACCESS_CURRENT_USER] {
            let name = std::ffi::CString::new(format!("ffi-access-{}-{access}", std::process::id())).unwrap();
            // SAFETY: valid C string; handles from this API.
            unsafe {
                let l = gk_listener_bind_ex(name.as_ptr(), access);
                assert!(!l.is_null(), "bind_ex({access}): errno {}", compat::errno());
                let l_addr = l as usize;
                let accepting = std::thread::spawn(move || gk_listener_accept(l_addr as *mut GkListener) as usize);
                let q = gk_process_connect(name.as_ptr());
                assert!(!q.is_null(), "connect({access}): errno {}", compat::errno());
                let p = accepting.join().unwrap() as *mut GkProcess;
                assert!(!p.is_null());
                let msg = b"same user";
                assert_eq!(gk_process_send_bytes(p, msg.as_ptr().cast(), msg.len()), 0);
                let mut buf = [0u8; 16];
                assert_eq!(gk_process_recv_bytes(q, buf.as_mut_ptr().cast(), buf.len()), msg.len() as isize);
                gk_process_free(p);
                gk_process_free(q);
                gk_listener_free(l);
            }
        }
        let name = std::ffi::CString::new(format!("ffi-access-bad-{}", std::process::id())).unwrap();
        // SAFETY: valid C string; null and unknown values are rejected.
        unsafe {
            assert!(gk_listener_bind_ex(name.as_ptr(), 2).is_null());
            assert_eq!(compat::errno(), crate::error::errno::EINVAL);
            assert!(gk_listener_bind_ex(core::ptr::null(), GK_LISTENER_ACCESS_DEFAULT).is_null());
        }
    }

    #[test]
    fn process_api_alive() {
        assert_eq!(gk_process_alive(std::process::id()), 1);
        assert_eq!(gk_process_alive(0), 1, "pid 0 is not looked up");
        // An own child: 1 while it runs, 0 once it exited (unreaped), and
        // the check leaves it for our `wait`.
        #[cfg(unix)]
        {
            let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            let pid = child.id();
            assert_eq!(gk_process_alive(pid), 1);
            child.kill().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while gk_process_alive(pid) == 1 {
                assert!(std::time::Instant::now() < deadline, "killed child still alive");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(child.wait().is_ok(), "the check reaped our child");
            assert_eq!(gk_process_alive(i32::MAX as u32), 0, "no such process");
        }
    }

    #[test]
    fn process_api_bytes_fd_and_peer_pid() {
        use crate::error::errno::{EINVAL, EMSGSIZE};
        let _g = compat::tests::lock();
        let name = std::ffi::CString::new(format!("ffi-bytes-{}", std::process::id())).unwrap();
        // SAFETY: valid C string and buffers; handles from this API.
        unsafe {
            let l = gk_listener_bind(name.as_ptr());
            assert!(!l.is_null(), "bind: errno {}", compat::errno());
            let l_addr = l as usize;
            let accepting = std::thread::spawn(move || gk_listener_accept(l_addr as *mut GkListener) as usize);
            let q = gk_process_connect(name.as_ptr());
            assert!(!q.is_null());
            let p = accepting.join().unwrap() as *mut GkProcess;
            assert!(!p.is_null());

            // Both ends name this process where the OS can tell (0 otherwise).
            let me = i64::from(std::process::id());
            for h in [p, q] {
                let pid = gk_process_peer_pid(h);
                assert!(pid == me || pid == 0, "peer pid {pid}");
                #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
                assert_eq!(pid, me);
            }
            #[cfg(unix)]
            assert!(gk_process_fd(q) >= 0);

            // Round trip; a buffer too small keeps the message (EMSGSIZE).
            let msg = b"hello, control link";
            assert_eq!(gk_process_send_bytes(p, msg.as_ptr().cast(), msg.len()), 0);
            let mut small = [0u8; 4];
            assert_eq!(gk_process_recv_bytes(q, small.as_mut_ptr().cast(), small.len()), -1);
            assert_eq!(compat::errno(), EMSGSIZE);
            let mut buf = vec![0u8; GK_BYTES_MAX_FOR_TESTS];
            let n = gk_process_recv_bytes(q, buf.as_mut_ptr().cast(), buf.len());
            assert_eq!(n, msg.len() as isize, "errno {}", compat::errno());
            assert_eq!(&buf[..msg.len()], msg);

            // Empty message, null buffer.
            assert_eq!(gk_process_send_bytes(p, core::ptr::null(), 0), 0);
            assert_eq!(gk_process_recv_bytes(q, core::ptr::null_mut(), 0), 0);

            // Too long: EMSGSIZE, nothing sent.
            let big = vec![7u8; GK_BYTES_MAX_FOR_TESTS + 1];
            assert_eq!(gk_process_send_bytes(p, big.as_ptr().cast(), big.len()), -1);
            assert_eq!(compat::errno(), EMSGSIZE);

            // An object where bytes are expected: EINVAL, connection usable.
            let fd = gk_eventfd(0, EFD_NONBLOCK_FOR_TESTS);
            assert_eq!(gk_process_send(p, fd), 0);
            assert_eq!(gk_process_recv_bytes(q, buf.as_mut_ptr().cast(), buf.len()), -1);
            assert_eq!(compat::errno(), EINVAL);
            // And the reverse.
            assert_eq!(gk_process_send_bytes(p, msg.as_ptr().cast(), msg.len()), 0);
            assert_eq!(gk_process_recv(q), -1);
            assert_eq!(compat::errno(), EINVAL);
            // The largest message, sent from another thread (it doesn't fit
            // the socket buffer, so the send waits for this receive).
            let p_addr = p as usize;
            let sending = std::thread::spawn(move || {
                let big = vec![7u8; GK_BYTES_MAX_FOR_TESTS];
                gk_process_send_bytes(p_addr as *mut GkProcess, big.as_ptr().cast(), big.len())
            });
            let n = gk_process_recv_bytes(q, buf.as_mut_ptr().cast(), buf.len());
            assert_eq!(sending.join().unwrap(), 0);
            assert_eq!(n, GK_BYTES_MAX_FOR_TESTS as isize);
            assert!(buf.iter().all(|&b| b == 7));

            // Null handles.
            assert_eq!(gk_process_send_bytes(core::ptr::null_mut(), core::ptr::null(), 0), -1);
            assert_eq!(gk_process_recv_bytes(core::ptr::null_mut(), core::ptr::null_mut(), 0), -1);
            assert_eq!(gk_process_peer_pid(core::ptr::null()), -1);
            #[cfg(unix)]
            assert_eq!(gk_process_fd(core::ptr::null_mut()), -1);

            assert_eq!(gk_eventfd_close(fd), 0);
            gk_process_free(p);
            gk_process_free(q);
            gk_listener_free(l);
        }
    }

    const GK_BYTES_MAX_FOR_TESTS: usize = crate::process::BYTES_MAX;

    const EFD_NONBLOCK_FOR_TESTS: c_int = compat::EFD_NONBLOCK;

    #[test]
    fn process_api_for_child() {
        let _g = compat::tests::lock();
        let mut env = vec![0 as core::ffi::c_char; compat::TOKEN_MAX];
        // SAFETY: buffers valid for their lengths; handles from this API.
        unsafe {
            let mut small = [0 as core::ffi::c_char; 2];
            assert!(gk_process_for_child(small.as_mut_ptr(), 2).is_null());
            assert_eq!(compat::errno(), crate::error::errno::ERANGE);
            let p = gk_process_for_child(env.as_mut_ptr(), env.len());
            assert!(!p.is_null());
            #[allow(clippy::useless_conversion)] // c_ulong is u32 on Windows
            let pid: core::ffi::c_ulong = std::process::id().into();
            assert_eq!(gk_process_expect_pid(p, pid), 0);
            // The "child" is a thread of this process, connecting with the
            // value it would find in GK_EVENTFD_PARENT.
            let value = std::ffi::CStr::from_ptr(env.as_ptr()).to_owned();
            round_trip(p, value);
        }
    }

    #[test]
    fn process_api_errors() {
        // SAFETY: null and invalid arguments are rejected.
        unsafe {
            assert!(gk_listener_bind(core::ptr::null()).is_null());
            assert_eq!(gk_process_send(core::ptr::null_mut(), 0), -1);
            assert_eq!(gk_process_recv(core::ptr::null_mut()), -1);
            gk_process_free(core::ptr::null_mut());
            gk_listener_free(core::ptr::null_mut());
            if std::env::var_os(crate::process::PARENT_ENV).is_none() {
                assert!(gk_process_from_parent().is_null());
                assert_eq!(compat::errno(), crate::error::errno::ENOENT);
            }
        }
    }

    #[test]
    fn shm_api() {
        use crate::error::errno::{EINVAL, ENOMEM, ENOTSUP, EPERM};
        let _g = compat::tests::lock();
        // SAFETY: handles and buffers from this API, used within bounds.
        unsafe {
            let shm = gk_shm_new(5000);
            assert!(!shm.is_null(), "errno {}", compat::errno());
            let page = gk_shm_page_size(shm);
            assert!(page.is_power_of_two());
            assert_eq!(gk_shm_len(shm), 5000usize.div_ceil(page) * page);
            assert_eq!(gk_shm_is_huge(shm), 0);
            #[cfg(unix)]
            assert!(gk_shm_fd(shm) >= 0);
            #[cfg(windows)]
            assert!(!gk_shm_handle(shm).is_null());
            let p = gk_shm_ptr(shm).cast::<u8>();
            assert_eq!(*p.add(4999), 0);
            *p.add(4999) = 7;

            // Hand it to another thread through the process API.
            let name = std::ffi::CString::new(format!("ffi-shm-{}", std::process::id())).unwrap();
            let l = gk_listener_bind(name.as_ptr());
            assert!(!l.is_null());
            let l_addr = l as usize;
            let accepting = std::thread::spawn(move || gk_listener_accept(l_addr as *mut GkListener) as usize);
            let q = gk_process_connect(name.as_ptr());
            let a = accepting.join().unwrap() as *mut GkProcess;
            assert!(!q.is_null() && !a.is_null());
            assert_eq!(gk_process_send_shm(a, shm), 0, "errno {}", compat::errno());
            let got = gk_process_recv_shm(q);
            assert!(!got.is_null(), "errno {}", compat::errno());
            assert_eq!(gk_shm_len(got), gk_shm_len(shm));
            let g = gk_shm_ptr(got).cast::<u8>();
            assert_eq!(*g.add(4999), 7);
            *g.add(1) = 9;
            assert_eq!(*p.add(1), 9);
            // An eventfd where shared memory is expected: refused.
            let fd = gk_eventfd(0, 0);
            assert_eq!(gk_process_send(a, fd), 0);
            assert!(gk_process_recv_shm(q).is_null());
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_eventfd_close(fd), 0);
            gk_shm_free(got);
            gk_process_free(a);
            gk_process_free(q);
            gk_listener_free(l);
            gk_shm_free(shm);

            // Options.
            let mut opts = GkShmOptionsC { huge_pages: 9, huge_page_size: 1, populate: 9 };
            gk_shm_options_default(&mut opts);
            assert_eq!(opts.huge_pages, GK_HUGE_PAGES_OFF);
            assert_eq!(opts.huge_page_size, 0);
            assert_eq!(opts.populate, 0);
            opts.huge_pages = GK_HUGE_PAGES_PREFER;
            opts.populate = 1;
            let shm = gk_shm_new_ex(1 << 20, &opts);
            assert!(!shm.is_null(), "errno {}", compat::errno());
            gk_shm_free(shm);
            let shm = gk_shm_new_ex(1, core::ptr::null());
            assert!(!shm.is_null());
            gk_shm_free(shm);
            opts.huge_pages = GK_HUGE_PAGES_REQUIRE;
            let shm = gk_shm_new_ex(1 << 21, &opts);
            if shm.is_null() {
                let e = compat::errno();
                assert!([ENOTSUP, ENOMEM, EPERM, crate::error::errno::EACCES].contains(&e), "errno {e}");
            } else {
                assert_eq!(gk_shm_is_huge(shm), 1);
                gk_shm_free(shm);
            }
            opts.huge_pages = 3;
            assert!(gk_shm_new_ex(1, &opts).is_null());
            assert_eq!(compat::errno(), EINVAL);

            // Errors.
            assert!(gk_shm_new(0).is_null());
            assert_eq!(compat::errno(), EINVAL);
            assert!(gk_shm_ptr(core::ptr::null()).is_null());
            assert_eq!(gk_shm_len(core::ptr::null()), 0);
            assert_eq!(gk_shm_is_huge(core::ptr::null()), -1);
            assert_eq!(gk_process_send_shm(core::ptr::null_mut(), core::ptr::null()), -1);
            assert!(gk_process_recv_shm(core::ptr::null_mut()).is_null());
            gk_shm_free(core::ptr::null_mut());
        }
    }

    unsafe extern "C" fn collect(ctx: *mut c_void, kind: u16, msg: *const c_void, len: usize) {
        // SAFETY: `ctx` is the test's Vec; `msg` holds `len` bytes.
        unsafe {
            let v = &mut *ctx.cast::<Vec<(u16, Vec<u8>)>>();
            v.push((kind, std::slice::from_raw_parts(msg.cast::<u8>(), len).to_vec()));
        }
    }

    #[test]
    fn mpsc_batch_and_pool_c_api() {
        use crate::error::errno::{EAGAIN, EMSGSIZE, ENOMEM, EPIPE};
        // SAFETY: handles from this API, each freed once; buffers valid.
        unsafe {
            let (mut tx, mut rx) = (core::ptr::null_mut(), core::ptr::null_mut());
            assert_eq!(gk_channel_new_mpsc(core::ptr::null(), &mut tx, &mut rx), 0);
            let tx2 = gk_mp_sender_clone(tx);
            assert_eq!(gk_mp_sender_send_kind(tx, 1, b"one".as_ptr().cast(), 3), 0);
            assert_eq!(gk_mp_sender_try_send_kind(tx2, 2, b"two".as_ptr().cast(), 3), 0);
            assert_eq!(gk_mp_sender_send(tx2, core::ptr::null(), 0), 0);
            assert_eq!(gk_mp_sender_send(tx, core::ptr::null(), 5), -1);
            assert_eq!(compat::errno(), EINVAL);
            let big = vec![0u8; (64 << 10) + 1];
            assert_eq!(gk_mp_sender_send(tx, big.as_ptr().cast(), big.len()), -1);
            assert_eq!(compat::errno(), EMSGSIZE);
            let mut id = 0;
            assert_eq!(gk_mp_sender_send_bulk_kind(tx, 9, big.as_ptr().cast(), big.len(), &mut id), 0);
            assert!(id != 0);
            let mut got: Vec<(u16, Vec<u8>)> = Vec::new();
            let ctx = (&raw mut got).cast::<c_void>();
            assert_eq!(gk_receiver_recv_batch(rx, 10, Some(collect), ctx), 3);
            assert_eq!(got, vec![(1, b"one".to_vec()), (2, b"two".to_vec()), (0, vec![])]);
            assert_eq!(gk_receiver_recv_batch(rx, 10, None, ctx), -1);
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_receiver_set_nonblocking(rx, 1), 0);
            assert_eq!(gk_receiver_recv_batch(rx, 10, Some(collect), ctx), -1);
            assert_eq!(compat::errno(), EAGAIN, "messages drained (the bulk transfer is another lane)");
            assert_eq!(gk_mp_sender_set_nonblocking(tx, 1), 0);
            assert_eq!(gk_mp_sender_claim(tx), 0);
            assert_eq!(gk_receiver_peer_gone(rx), 0);
            gk_mp_sender_free(tx);
            assert_eq!(gk_receiver_peer_gone(rx), 0, "a clone is left");
            gk_mp_sender_free(tx2);
            assert_eq!(gk_receiver_peer_gone(rx), 1);
            assert_eq!(gk_receiver_bulk_begin(rx, core::ptr::null_mut(), core::ptr::null_mut()), 0);
            assert_eq!(gk_receiver_bulk_end(rx), 0);
            assert_eq!(gk_receiver_recv_batch(rx, 10, Some(collect), ctx), -1);
            assert_eq!(compat::errno(), EPIPE);
            gk_receiver_free(rx);
            assert_eq!(gk_mp_sender_peer_gone(core::ptr::null()), -1);
            assert!(gk_mp_sender_clone(core::ptr::null()).is_null());
            gk_mp_sender_free(core::ptr::null_mut());
            gk_mp_sender_detach(core::ptr::null_mut());

            // Pools.
            let mut o = GkShmPoolOptionsC { size: 0, granule: 0, huge_pages: 0 };
            gk_shm_pool_options_default(&mut o);
            assert_eq!(o.granule, 4096);
            o.size = 64 << 10;
            let pool = gk_shm_pool_new(&o);
            assert!(!pool.is_null());
            let view = gk_shm_pool_new_view(pool);
            let b = gk_shm_pool_alloc(pool, 5000);
            assert_eq!(gk_shm_block_capacity(b), 8192);
            std::ptr::copy_nonoverlapping(b"zero-copy".as_ptr(), gk_shm_block_ptr(b).cast::<u8>(), 9);
            let mut lease = GkShmLeaseC { index: 0, generation: 0, len: 0 };
            assert_eq!(gk_shm_block_lend(b, 9000, &mut lease), -1, "more than the capacity: still ours");
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_shm_block_lend(b, 9, &mut lease), 0);
            let buf = gk_shm_pool_view_take(view, &lease);
            assert_eq!(gk_shm_buf_len(buf), 9);
            assert_eq!(std::slice::from_raw_parts(gk_shm_buf_ptr(buf).cast::<u8>(), 9), b"zero-copy");
            let mut st = GkShmPoolStatsC { size: 0, granule: 0, free: 0, allocated: 0, lent: 0, bad_returns: 0 };
            assert_eq!(gk_shm_pool_get_stats(pool, &mut st), 0);
            assert_eq!((st.lent, st.free), (1, (64 << 10) - 8192));
            gk_shm_buf_free(buf);
            assert_eq!(gk_shm_pool_get_stats(pool, &mut st), 0);
            assert_eq!((st.lent, st.free, st.bad_returns), (0, 64 << 10, 0));
            let all = gk_shm_pool_alloc(pool, 64 << 10);
            assert!(gk_shm_pool_alloc(pool, 1).is_null());
            assert_eq!(compat::errno(), ENOMEM);
            gk_shm_block_free(all);
            assert!(gk_shm_pool_alloc(pool, 1 << 20).is_null());
            assert_eq!(compat::errno(), EINVAL);
            let bad = GkShmLeaseC { index: 1000, generation: 1, len: 1 };
            assert!(gk_shm_pool_view_take(view, &bad).is_null());
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_shm_pool_reclaim_all(pool), 0);
            assert_eq!(gk_shm_pool_get_stats(pool, core::ptr::null_mut()), -1);
            o.granule = 100;
            assert!(gk_shm_pool_new(&o).is_null());
            assert_eq!(compat::errno(), EINVAL);
            assert_eq!(gk_process_send_shm_pool_view(core::ptr::null_mut(), view), -1);
            assert!(gk_process_recv_shm_pool_view(core::ptr::null_mut()).is_null());
            gk_shm_pool_view_free(view);
            gk_shm_pool_free(pool);
            gk_shm_buf_free(core::ptr::null_mut());
            gk_shm_block_free(core::ptr::null_mut());
        }
    }

    #[test]
    fn panic_becomes_eio() {
        let r = guard(-1, || -> c_int { panic!("boom") });
        assert_eq!(r, -1);
        assert_eq!(compat::errno(), EIO);
    }
}

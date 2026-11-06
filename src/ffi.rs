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

#[cfg(windows)]
type InvalidParameterHandler =
    Option<unsafe extern "C" fn(*const u16, *const u16, *const u16, c_uint, usize)>;

#[cfg(windows)]
unsafe extern "C" {
    fn _set_thread_local_invalid_parameter_handler(
        h: InvalidParameterHandler,
    ) -> InvalidParameterHandler;
}

#[cfg(windows)]
unsafe extern "C" fn ignore_invalid_parameter(
    _: *const u16,
    _: *const u16,
    _: *const u16,
    _: c_uint,
    _: usize,
) {
}

/// Runs a CRT descriptor call with the invalid-parameter handler silenced.
/// Without it the CRT aborts the process (`STATUS_STACK_BUFFER_OVERRUN`) on a
/// bad descriptor such as `-1`, where POSIX returns `-1` with `EBADF`. The
/// handler is thread-local and restored afterwards; the cost is two TLS stores.
#[cfg(windows)]
fn crt_call<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: installs/restores a thread-local handler; no pointers involved.
    let prev =
        unsafe { _set_thread_local_invalid_parameter_handler(Some(ignore_invalid_parameter)) };
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
        return crt_call(|| unsafe { _close(fd) });
    })
}

/// `int gk_eventfd_export_inheritable(int fd, char *buf, size_t len);`
/// (see `compat::eventfd_export_inheritable`).
///
/// # Safety
/// `buf` must be valid for writing `len` bytes.
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

use crate::{GkListener, GkProcess};
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
pub unsafe extern "C" fn gk_process_for_child(
    env_value: *mut c_char,
    len: usize,
) -> *mut GkProcess {
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
pub unsafe extern "C" fn gk_process_expect_pid(
    process: *mut GkProcess,
    pid: core::ffi::c_ulong,
) -> c_int {
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

// ---------------------------------------------------------------------------
// Channels (GkSender / GkReceiver as opaque C handles)
// ---------------------------------------------------------------------------

use crate::{ChannelOptions, GkChannel, GkReceiver, GkSender};

/// Sets `errno` for a channel error: size problems are `EMSGSIZE`.
fn channel_errno(e: &std::io::Error) {
    use crate::error::errno::EMSGSIZE;
    compat::set_errno(if e.kind() == std::io::ErrorKind::InvalidInput {
        EMSGSIZE
    } else {
        crate::error::to_errno(e)
    });
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
}

impl From<ChannelOptions> for GkChannelOptionsC {
    fn from(o: ChannelOptions) -> Self {
        Self {
            capacity: o.capacity,
            max_message: o.max_message,
            bulk_capacity: o.bulk_capacity,
            chunk: o.chunk,
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
pub unsafe extern "C" fn gk_channel_new(
    opts: *const GkChannelOptionsC,
    tx: *mut *mut GkSender,
    rx: *mut *mut GkReceiver,
) -> c_int {
    guard(-1, || {
        if tx.is_null() || rx.is_null() {
            compat::set_errno(crate::error::errno::EINVAL);
            return -1;
        }
        // SAFETY: the caller guarantees `opts` is null or readable.
        let o = match unsafe { opts.as_ref() } {
            Some(o) => ChannelOptions {
                capacity: o.capacity,
                max_message: o.max_message,
                bulk_capacity: o.bulk_capacity,
                chunk: o.chunk,
            },
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
///
/// # Safety
/// `tx` must be a live handle; `buf` valid for reading `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_send(
    tx: *mut GkSender,
    buf: *const c_void,
    len: usize,
) -> c_int {
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
            match tx.send(msg) {
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
pub unsafe extern "C" fn gk_receiver_recv(
    rx: *mut GkReceiver,
    buf: *mut c_void,
    cap: usize,
) -> isize {
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
            match rx.recv(out) {
                Ok(n) => n as isize,
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
    guard(-1, || {
        with(rx, -1, |rx| match rx.next_len() {
            Ok(Some(n)) => n as isize,
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
pub unsafe extern "C" fn gk_receiver_set_nonblocking(
    rx: *mut GkReceiver,
    nonblocking: c_int,
) -> c_int {
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
    guard(-1, || {
        with(tx, -1, |tx| std::os::fd::AsRawFd::as_raw_fd(&*tx))
    })
}

/// `int gk_receiver_fd(gk_receiver *rx);` (unix: readable when a message may wait)
///
/// # Safety
/// `rx` must be a live handle.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_fd(rx: *mut GkReceiver) -> c_int {
    guard(-1, || {
        with(rx, -1, |rx| std::os::fd::AsRawFd::as_raw_fd(&*rx))
    })
}

/// `uintptr_t gk_sender_socket(gk_sender *tx);` (Windows)
///
/// # Safety
/// `tx` must be a live handle.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_socket(tx: *mut GkSender) -> usize {
    guard(usize::MAX, || {
        with(tx, usize::MAX, |tx| {
            std::os::windows::io::AsRawSocket::as_raw_socket(&*tx) as usize
        })
    })
}

/// `uintptr_t gk_receiver_socket(gk_receiver *rx);` (Windows)
///
/// # Safety
/// `rx` must be a live handle.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_receiver_socket(rx: *mut GkReceiver) -> usize {
    guard(usize::MAX, || {
        with(rx, usize::MAX, |rx| {
            std::os::windows::io::AsRawSocket::as_raw_socket(&*rx) as usize
        })
    })
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
pub unsafe extern "C" fn gk_sender_send_bulk(
    tx: *mut GkSender,
    buf: *const c_void,
    len: usize,
    id: *mut u64,
) -> c_int {
    guard(-1, || {
        with(tx, -1, |tx| {
            // SAFETY: forwarded to the caller.
            let Some(data) = (unsafe { c_bytes(buf, len) }) else {
                return -1;
            };
            match tx.send_bulk(data) {
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
    guard(-1, || {
        with(tx, -1, |tx| match tx.bulk_begin(u64::try_from(len).ok()) {
            Ok(v) => {
                // SAFETY: forwarded to the caller.
                unsafe { put(id, v) };
                0
            }
            Err(e) => {
                bulk_errno(&e);
                -1
            }
        })
    })
}

/// `intptr_t gk_sender_bulk_write(gk_sender *tx, const void *buf, size_t len);`
///
/// # Safety
/// `tx` live; `buf` readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_sender_bulk_write(
    tx: *mut GkSender,
    buf: *const c_void,
    len: usize,
) -> isize {
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
pub unsafe extern "C" fn gk_receiver_bulk_begin(
    rx: *mut GkReceiver,
    len: *mut i64,
    id: *mut u64,
) -> c_int {
    guard(-1, || {
        with(rx, -1, |rx| match rx.bulk_begin() {
            Ok((v, total)) => {
                // SAFETY: forwarded to the caller.
                unsafe {
                    put(id, v);
                    put(
                        len,
                        total.map_or(-1, |t| i64::try_from(t).unwrap_or(i64::MAX)),
                    );
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
pub unsafe extern "C" fn gk_receiver_bulk_read(
    rx: *mut GkReceiver,
    buf: *mut c_void,
    cap: usize,
) -> isize {
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
    guard(core::ptr::null_mut(), || {
        with(p, core::ptr::null_mut(), |p| boxed(p.recv_as::<GkSender>()))
    })
}

/// `gk_receiver *gk_process_recv_receiver(gk_process *p);`
///
/// # Safety
/// `p` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gk_process_recv_receiver(p: *mut GkProcess) -> *mut GkReceiver {
    guard(core::ptr::null_mut(), || {
        with(p, core::ptr::null_mut(), |p| {
            boxed(p.recv_as::<GkReceiver>())
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
            let accepting =
                std::thread::spawn(move || gk_listener_accept(l_addr as *mut GkListener) as usize);
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
    fn panic_becomes_eio() {
        let r = guard(-1, || -> c_int { panic!("boom") });
        assert_eq!(r, -1);
        assert_eq!(compat::errno(), EIO);
    }
}

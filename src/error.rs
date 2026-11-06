//! Platform-native construction of the few errors eventfd can produce, plus
//! mapping back to C `errno` values for the FFI layer.

use core::ffi::c_int;
use std::io;

/// C `errno` values used by this crate. On unix these are the libc values; on
/// Windows they are the MSVC CRT values from `<errno.h>`.
#[allow(dead_code)]
pub(crate) mod errno {
    use core::ffi::c_int;

    #[cfg(unix)]
    pub const EAGAIN: c_int = libc::EAGAIN;
    #[cfg(unix)]
    pub const EINVAL: c_int = libc::EINVAL;
    #[cfg(unix)]
    pub const EBADF: c_int = libc::EBADF;
    #[cfg(unix)]
    pub const ENOMEM: c_int = libc::ENOMEM;
    #[cfg(unix)]
    pub const EMFILE: c_int = libc::EMFILE;
    #[cfg(unix)]
    pub const EINTR: c_int = libc::EINTR;
    #[cfg(unix)]
    pub const EIO: c_int = libc::EIO;

    #[cfg(windows)]
    pub const EAGAIN: c_int = 11;
    #[cfg(windows)]
    pub const EINVAL: c_int = 22;
    #[cfg(windows)]
    pub const EBADF: c_int = 9;
    #[cfg(windows)]
    pub const ENOMEM: c_int = 12;
    #[cfg(windows)]
    pub const EMFILE: c_int = 24;
    #[cfg(windows)]
    pub const EINTR: c_int = 4;
    #[cfg(windows)]
    pub const EIO: c_int = 5;
}

/// `EAGAIN`: the operation would block on a non-blocking eventfd.
/// `kind()` is always [`io::ErrorKind::WouldBlock`].
pub(crate) fn would_block() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::EAGAIN)
    }
    #[cfg(windows)]
    {
        // WSAEWOULDBLOCK, which std maps to ErrorKind::WouldBlock.
        io::Error::from_raw_os_error(10035)
    }
}

/// `EINVAL`: bad argument (write of `u64::MAX`, short buffer, unknown flags).
/// `kind()` is always [`io::ErrorKind::InvalidInput`].
pub(crate) fn invalid_input() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::EINVAL)
    }
    #[cfg(windows)]
    {
        // ERROR_INVALID_PARAMETER, which std maps to ErrorKind::InvalidInput.
        io::Error::from_raw_os_error(87)
    }
}

/// `EBADF`: descriptor not known to this crate (FFI / compat layer).
#[allow(dead_code)]
pub(crate) fn bad_descriptor() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::EBADF)
    }
    #[cfg(windows)]
    {
        // ERROR_INVALID_HANDLE
        io::Error::from_raw_os_error(6)
    }
}

/// Best-effort mapping of an `io::Error` to a C `errno` value.
#[allow(dead_code)]
pub(crate) fn to_errno(e: &io::Error) -> c_int {
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return code;
    }
    match e.kind() {
        io::ErrorKind::WouldBlock => errno::EAGAIN,
        io::ErrorKind::InvalidInput => errno::EINVAL,
        io::ErrorKind::OutOfMemory => errno::ENOMEM,
        io::ErrorKind::Interrupted => errno::EINTR,
        #[cfg(windows)]
        _ if e.raw_os_error() == Some(6) => errno::EBADF,
        _ => errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_portable() {
        assert_eq!(would_block().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(invalid_input().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(to_errno(&would_block()), errno::EAGAIN);
        assert_eq!(to_errno(&invalid_input()), errno::EINVAL);
        assert_eq!(to_errno(&bad_descriptor()), errno::EBADF);
    }
}

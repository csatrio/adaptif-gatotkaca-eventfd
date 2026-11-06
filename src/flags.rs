//! `eventfd` flag constants and the [`EfdFlags`] type.
//!
//! Where the kernel has a native eventfd (Linux, Android, FreeBSD, NetBSD,
//! illumos) the constants are taken from `libc`, so they always match that
//! kernel's ABI (e.g. FreeBSD's `EFD_NONBLOCK` is `0x4`). On the shim
//! platforms they use the x86/arm Linux values, so C code ported from Linux
//! keeps working.

use core::ffi::c_int;
use core::fmt;
use core::ops::{BitAnd, BitOr, BitOrAssign};

/// The eventfd counter type (`eventfd_t` in `<sys/eventfd.h>`).
#[allow(non_camel_case_types)]
pub type eventfd_t = u64;

#[cfg(gk_native)]
mod values {
    use core::ffi::c_int;
    pub const EFD_SEMAPHORE: c_int = libc::EFD_SEMAPHORE;
    pub const EFD_CLOEXEC: c_int = libc::EFD_CLOEXEC;
    pub const EFD_NONBLOCK: c_int = libc::EFD_NONBLOCK;
}

#[cfg(not(gk_native))]
mod values {
    use core::ffi::c_int;
    pub const EFD_SEMAPHORE: c_int = 0o1;
    pub const EFD_CLOEXEC: c_int = 0o2000000;
    pub const EFD_NONBLOCK: c_int = 0o4000;
}

/// Reads return 1 and decrement the counter instead of returning and resetting it.
pub const EFD_SEMAPHORE: c_int = values::EFD_SEMAPHORE;
/// Close the descriptor on `exec` (non-inheritable handle on Windows).
pub const EFD_CLOEXEC: c_int = values::EFD_CLOEXEC;
/// Reads and writes fail with `EAGAIN` / [`std::io::ErrorKind::WouldBlock`] instead of blocking.
pub const EFD_NONBLOCK: c_int = values::EFD_NONBLOCK;

/// Largest value the counter can hold: `0xffff_ffff_ffff_fffe`.
pub const EFD_MAX: u64 = u64::MAX - 1;

/// Type-safe set of eventfd flags.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EfdFlags(c_int);

impl EfdFlags {
    /// No flags.
    pub const NONE: Self = Self(0);
    /// See [`EFD_SEMAPHORE`].
    pub const SEMAPHORE: Self = Self(EFD_SEMAPHORE);
    /// See [`EFD_CLOEXEC`].
    pub const CLOEXEC: Self = Self(EFD_CLOEXEC);
    /// See [`EFD_NONBLOCK`].
    pub const NONBLOCK: Self = Self(EFD_NONBLOCK);

    const ALL: c_int = EFD_SEMAPHORE | EFD_CLOEXEC | EFD_NONBLOCK;

    /// Builds flags from raw bits. Returns `None` if unknown bits are set
    /// (the C `eventfd()` call fails with `EINVAL` in that case).
    pub const fn from_bits(bits: c_int) -> Option<Self> {
        if bits & !Self::ALL != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    /// Raw bits, suitable for passing to C `eventfd()`.
    pub const fn bits(self) -> c_int {
        self.0
    }

    /// True if every flag in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// True if no flags are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for EfdFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for EfdFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for EfdFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl fmt::Debug for EfdFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        f.write_str("EfdFlags(")?;
        for (flag, name) in [
            (Self::SEMAPHORE, "SEMAPHORE"),
            (Self::CLOEXEC, "CLOEXEC"),
            (Self::NONBLOCK, "NONBLOCK"),
        ] {
            if self.contains(flag) {
                if !first {
                    f.write_str(" | ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("NONE")?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_bits() {
        assert!(EfdFlags::from_bits(0x4000_0000).is_none());
        assert_eq!(
            EfdFlags::from_bits(EFD_NONBLOCK | EFD_SEMAPHORE),
            Some(EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE)
        );
    }

    #[test]
    fn debug_format() {
        assert_eq!(format!("{:?}", EfdFlags::NONE), "EfdFlags(NONE)");
        assert_eq!(
            format!("{:?}", EfdFlags::SEMAPHORE | EfdFlags::NONBLOCK),
            "EfdFlags(SEMAPHORE | NONBLOCK)"
        );
    }
}

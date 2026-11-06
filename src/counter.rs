//! Platform-neutral eventfd semantics, shared by the macOS/BSD and Windows shims.
//!
//! [`Counter`] implements every rule of Linux `eventfd(2)` (blocking,
//! non-blocking, semaphore mode, overflow) on top of a `Mutex` + `Condvar`.
//! The OS-specific part is only the [`Notifier`], which keeps a pollable
//! object (pipe, socket) "readable" exactly while the counter is non-zero.
//!
//! Invariant: notifier calls happen while the counter lock is held, and the
//! counter is only updated after the notifier call succeeded. So the pollable
//! object is readable if and only if the committed counter value is > 0.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::error;
use crate::flags::{EFD_MAX, EfdFlags};

/// OS hook that mirrors "counter > 0" onto a pollable object.
pub(crate) trait Notifier {
    /// Called (under the counter lock) when the counter goes from 0 to > 0.
    fn set_readable(&self) -> io::Result<()>;
    /// Called (under the counter lock) when the counter goes from > 0 to 0.
    fn clear_readable(&self) -> io::Result<()>;
}

#[derive(Debug)]
pub(crate) struct Counter {
    value: Mutex<u64>,
    /// Signalled when the counter increases.
    readers: Condvar,
    /// Signalled when the counter decreases.
    writers: Condvar,
    semaphore: bool,
    nonblocking: AtomicBool,
}

impl Counter {
    /// Creates a counter. The caller must call `notifier.set_readable()`
    /// itself when `initval > 0` (see [`Counter::is_readable`]).
    pub(crate) fn new(initval: u32, flags: EfdFlags) -> Self {
        Self {
            value: Mutex::new(u64::from(initval)),
            readers: Condvar::new(),
            writers: Condvar::new(),
            semaphore: flags.contains(EfdFlags::SEMAPHORE),
            nonblocking: AtomicBool::new(flags.contains(EfdFlags::NONBLOCK)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, u64> {
        // The value is updated in one step, so it is always consistent even
        // if a thread panicked while holding the lock.
        self.value.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True when the counter is non-zero.
    #[allow(dead_code)]
    pub(crate) fn is_readable(&self) -> bool {
        *self.lock() > 0
    }

    /// Current counter value (for debugging and tests).
    #[allow(dead_code)]
    pub(crate) fn peek(&self) -> u64 {
        *self.lock()
    }

    pub(crate) fn is_nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Relaxed)
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblocking.store(nonblocking, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn is_semaphore(&self) -> bool {
        self.semaphore
    }

    /// `read(2)` on an eventfd.
    pub(crate) fn read(&self, notifier: &impl Notifier) -> io::Result<u64> {
        // Like Linux, the blocking mode is sampled when the call starts.
        let nonblocking = self.is_nonblocking();
        let mut value = self.lock();
        while *value == 0 {
            if nonblocking {
                return Err(error::would_block());
            }
            value = self.readers.wait(value).unwrap_or_else(|e| e.into_inner());
        }
        let (ret, new) = if self.semaphore {
            (1, *value - 1)
        } else {
            (*value, 0)
        };
        if new == 0 {
            notifier.clear_readable()?;
        }
        *value = new;
        drop(value);
        self.writers.notify_all();
        Ok(ret)
    }

    /// `write(2)` on an eventfd.
    pub(crate) fn write(&self, add: u64, notifier: &impl Notifier) -> io::Result<()> {
        if add == u64::MAX {
            return Err(error::invalid_input());
        }
        let nonblocking = self.is_nonblocking();
        let mut value = self.lock();
        while EFD_MAX - *value < add {
            if nonblocking {
                return Err(error::would_block());
            }
            value = self.writers.wait(value).unwrap_or_else(|e| e.into_inner());
        }
        if add == 0 {
            return Ok(());
        }
        if *value == 0 {
            notifier.set_readable()?;
        }
        *value += add;
        drop(value);
        self.readers.notify_all();
        Ok(())
    }
}

/// Decodes an eventfd write buffer (`EINVAL` if shorter than 8 bytes).
#[allow(dead_code)]
pub(crate) fn decode(buf: &[u8]) -> io::Result<u64> {
    match buf.get(..8) {
        Some(b) => Ok(u64::from_ne_bytes(b.try_into().expect("8 bytes"))),
        None => Err(error::invalid_input()),
    }
}

/// Encodes a read result into `buf` (`EINVAL` if shorter than 8 bytes).
#[allow(dead_code)]
pub(crate) fn encode(value: u64, buf: &mut [u8]) -> io::Result<usize> {
    match buf.get_mut(..8) {
        Some(b) => {
            b.copy_from_slice(&value.to_ne_bytes());
            Ok(8)
        }
        None => Err(error::invalid_input()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;

    /// Checks the readable-iff-nonzero invariant from inside the lock.
    #[derive(Default)]
    struct Flag(AtomicBool);
    impl Notifier for Flag {
        fn set_readable(&self) -> io::Result<()> {
            assert!(!self.0.swap(true, Ordering::SeqCst), "set twice");
            Ok(())
        }
        fn clear_readable(&self) -> io::Result<()> {
            assert!(self.0.swap(false, Ordering::SeqCst), "cleared twice");
            Ok(())
        }
    }
    impl Flag {
        fn get(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn nb() -> EfdFlags {
        EfdFlags::NONBLOCK
    }

    #[test]
    fn read_resets() {
        let c = Counter::new(0, nb());
        let n = Flag::default();
        c.write(3, &n).unwrap();
        c.write(4, &n).unwrap();
        assert!(n.get());
        assert_eq!(c.read(&n).unwrap(), 7);
        assert!(!n.get());
        assert_eq!(c.read(&n).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn semaphore_mode() {
        let c = Counter::new(2, nb() | EfdFlags::SEMAPHORE);
        let n = Flag::default();
        n.set_readable().unwrap(); // initval > 0
        assert_eq!(c.read(&n).unwrap(), 1);
        assert!(n.get());
        assert_eq!(c.read(&n).unwrap(), 1);
        assert!(!n.get());
        assert_eq!(c.read(&n).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn write_limits() {
        let c = Counter::new(0, nb());
        let n = Flag::default();
        assert_eq!(
            c.write(u64::MAX, &n).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        c.write(0, &n).unwrap();
        assert!(!n.get());
        c.write(EFD_MAX, &n).unwrap();
        assert_eq!(
            c.write(1, &n).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        c.write(0, &n).unwrap();
        assert_eq!(c.read(&n).unwrap(), EFD_MAX);
    }

    #[test]
    fn blocking_read_wakes() {
        let c = Arc::new(Counter::new(0, EfdFlags::NONE));
        let n = Arc::new(Flag::default());
        let (c2, n2) = (c.clone(), n.clone());
        let t = thread::spawn(move || c2.read(&*n2).unwrap());
        thread::sleep(Duration::from_millis(50));
        c.write(5, &*n).unwrap();
        assert_eq!(t.join().unwrap(), 5);
    }

    #[test]
    fn blocking_write_wakes() {
        let c = Arc::new(Counter::new(0, EfdFlags::NONE));
        let n = Arc::new(Flag::default());
        c.write(EFD_MAX, &*n).unwrap();
        let (c2, n2) = (c.clone(), n.clone());
        let t = thread::spawn(move || c2.write(10, &*n2).unwrap());
        thread::sleep(Duration::from_millis(50));
        assert_eq!(c.read(&*n).unwrap(), EFD_MAX);
        t.join().unwrap();
        assert_eq!(c.peek(), 10);
    }

    #[test]
    fn notifier_failure_leaves_state_unchanged() {
        struct Fail;
        impl Notifier for Fail {
            fn set_readable(&self) -> io::Result<()> {
                Err(io::Error::other("boom"))
            }
            fn clear_readable(&self) -> io::Result<()> {
                Err(io::Error::other("boom"))
            }
        }
        let c = Counter::new(0, nb());
        assert!(c.write(1, &Fail).is_err());
        assert_eq!(c.peek(), 0);
    }

    #[test]
    fn buffers() {
        assert!(decode(&[0; 7]).is_err());
        assert_eq!(decode(&5u64.to_ne_bytes()).unwrap(), 5);
        let mut b = [0u8; 8];
        assert!(encode(1, &mut b[..4]).is_err());
        assert_eq!(encode(9, &mut b).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(b), 9);
    }

    #[test]
    fn mpmc_stress() {
        let c = Arc::new(Counter::new(0, EfdFlags::SEMAPHORE));
        let n = Arc::new(Flag::default());
        let producers: Vec<_> = (0..4)
            .map(|_| {
                let (c, n) = (c.clone(), n.clone());
                thread::spawn(move || (0..1000).for_each(|_| c.write(1, &*n).unwrap()))
            })
            .collect();
        let consumers: Vec<_> = (0..4)
            .map(|_| {
                let (c, n) = (c.clone(), n.clone());
                thread::spawn(move || (0..1000).map(|_| c.read(&*n).unwrap()).sum::<u64>())
            })
            .collect();
        producers.into_iter().for_each(|t| t.join().unwrap());
        let total: u64 = consumers.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(total, 4000);
        assert_eq!(c.peek(), 0);
        assert!(!n.get());
    }
}

//! Cross-process eventfd state, shared by the unix and Windows shims.
//!
//! Each eventfd keeps its counter in a [`SharedPage`]:
//!
//! * unix: a `MAP_SHARED` mapping of an unnamed shared memory object
//!   (`memfd_create`, `SHM_ANON`, or `shm_open` + immediate `shm_unlink`).
//!   `fork(2)` keeps the mapping shared, and the object's descriptor can be
//!   inherited across `exec` or passed over a unix socket like any fd;
//! * Windows: an unnamed file-mapping section, whose handle can be passed to
//!   child processes (see `src/sys/windows.rs`).
//!
//! Each process unmaps its view when it drops its last handle; the kernel
//! frees the memory when the last process is done.
//!
//! The counter is guarded by a spinlock in the same page, since
//! process-local locks (`std::sync::Mutex`) cannot synchronise two processes:
//!
//! * Uncontended, locking is one compare-and-swap.
//! * Contended, waiters back off with `crossbeam_utils::Backoff` (spin, then
//!   yield), then sleep in growing steps capped at 1 ms, so a long wait costs
//!   almost no CPU. Critical sections are short: a counter update and at most
//!   one non-blocking pipe/socket `read`/`write`.
//! * The lock word holds the holder's process id. A waiter that has backed off
//!   fully checks whether that process still exists; if it died while holding
//!   the lock (e.g. `SIGKILL`, `TerminateProcess`), the waiter takes the lock
//!   over and resynchronises the readiness signal with the counter.
//!
//! The eventfd rules themselves ([`SharedPage::read`], [`SharedPage::write`])
//! are written once here, over a [`Signal`]: the OS object (pipe, socket pair)
//! that is readable exactly while the counter is non-zero.
//!
//! Limitations: the lock is not re-entrant, so calling into the same eventfd
//! from a signal handler that interrupted one of its operations deadlocks. On
//! unix, if the holder's pid is reused by a new process before a waiter
//! notices the death, the waiter keeps waiting (Windows does not reuse a
//! process id while a handle to the old process is open).

use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_utils::Backoff;

use super::region::{SharedRegion, current_pid, process_alive};

use crate::error;
use crate::flags::{EFD_MAX, EfdFlags};

/// Marks a page as an eventfd page of this crate's layout (checked when
/// another process's page is imported). "GKE" + layout version 1.
const MAGIC: u32 = 0x474B_4501;

/// The state shared between every process that holds the eventfd.
#[repr(C)]
#[derive(Debug)]
struct State {
    magic: AtomicU32,
    /// `0` when free, else the id of the process holding the lock.
    lock: AtomicU32,
    /// Shared like `O_NONBLOCK` on a Linux eventfd's open file description.
    nonblocking: AtomicBool,
    /// Fixed at creation.
    semaphore: AtomicBool,
    /// Only written while holding `lock`.
    counter: AtomicU64,
}

/// The OS object that mirrors "counter > 0".
pub(crate) trait Signal {
    /// Counter went from 0 to > 0: become readable. Called under the lock.
    fn set_readable(&self) -> io::Result<()>;
    /// Counter went from > 0 to 0: stop being readable. Called under the lock.
    fn clear_readable(&self) -> io::Result<()>;
    /// Removes every pending readiness byte, without waiting for in-flight
    /// ones (lock recovery). Called under the lock.
    fn drain(&self) -> io::Result<()>;
    /// Blocks until readable (the counter may be > 0). Called without the lock.
    fn wait_readable(&self) -> io::Result<()>;
}

/// One shared region holding a [`State`].
#[derive(Debug)]
pub(crate) struct SharedPage {
    region: SharedRegion,
}

impl SharedPage {
    /// A fresh page for an eventfd created with `initval` and `flags`.
    pub(crate) fn new(initval: u32, flags: EfdFlags) -> io::Result<Self> {
        let region = SharedRegion::create(size_of::<State>())?;
        // SAFETY: the mapping is page-aligned, writable and large enough, and
        // nothing else can see it yet.
        unsafe {
            region.ptr().cast::<State>().write(State {
                magic: AtomicU32::new(MAGIC),
                lock: AtomicU32::new(0),
                nonblocking: AtomicBool::new(flags.contains(EfdFlags::NONBLOCK)),
                semaphore: AtomicBool::new(flags.contains(EfdFlags::SEMAPHORE)),
                counter: AtomicU64::new(u64::from(initval)),
            })
        };
        Ok(Self { region })
    }

    fn state(&self) -> &State {
        // SAFETY: initialised in `new` (or checked in `adopt`), unmapped
        // only when the region drops. Other processes only access it
        // through atomics too.
        unsafe { self.region.ptr().cast::<State>().as_ref() }
    }

    /// Accepts a mapped object as an eventfd page.
    fn is_page(ptr: NonNull<u8>, len: usize) -> bool {
        // SAFETY: the view is at least `len >= size_of::<State>()` bytes
        // (checked by the region before calling us) and page-aligned.
        len >= size_of::<State>()
            && unsafe { ptr.cast::<State>().as_ref() }
                .magic
                .load(Ordering::Acquire)
                == MAGIC
    }

    pub(crate) fn is_nonblocking(&self) -> bool {
        self.state().nonblocking.load(Ordering::Relaxed)
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) {
        self.state()
            .nonblocking
            .store(nonblocking, Ordering::Relaxed);
    }

    /// True if the counter is > 0 (initial readiness, recovery).
    pub(crate) fn is_readable(&self) -> bool {
        self.state().counter.load(Ordering::Relaxed) > 0
    }

    /// Takes the lock. If it was taken over from a process that died
    /// holding it, `signal` may disagree with the counter: resync it.
    fn lock(&self, signal: &impl Signal) -> io::Result<Guard<'_>> {
        let guard = self.state().lock();
        if guard.recovered {
            signal.drain()?;
            if self.is_readable() {
                signal.set_readable()?;
            }
        }
        Ok(guard)
    }

    /// `read(2)` on the eventfd.
    pub(crate) fn read(&self, signal: &impl Signal) -> io::Result<u64> {
        let state = self.state();
        // Like Linux, the blocking mode is sampled when the call starts.
        let nonblocking = self.is_nonblocking();
        loop {
            {
                let _guard = self.lock(signal)?;
                let value = state.counter.load(Ordering::Relaxed);
                if value > 0 {
                    let (ret, new) = if state.semaphore.load(Ordering::Relaxed) {
                        (1, value - 1)
                    } else {
                        (value, 0)
                    };
                    if new == 0 {
                        signal.clear_readable()?;
                    }
                    state.counter.store(new, Ordering::Relaxed);
                    return Ok(ret);
                }
                if nonblocking {
                    return Err(error::would_block());
                }
            }
            // Counter is 0: wait (outside the lock) until a write in any
            // process makes the signal readable, then try again.
            signal.wait_readable()?;
        }
    }

    /// `write(2)` on the eventfd.
    pub(crate) fn write(&self, add: u64, signal: &impl Signal) -> io::Result<()> {
        if add == u64::MAX {
            return Err(error::invalid_input());
        }
        let state = self.state();
        let nonblocking = self.is_nonblocking();
        let mut sleep = Duration::from_micros(50);
        loop {
            {
                let _guard = self.lock(signal)?;
                let value = state.counter.load(Ordering::Relaxed);
                if EFD_MAX - value >= add {
                    if add == 0 {
                        return Ok(());
                    }
                    if value == 0 {
                        signal.set_readable()?;
                    }
                    state.counter.store(value + add, Ordering::Relaxed);
                    return Ok(());
                }
                if nonblocking {
                    return Err(error::would_block());
                }
            }
            // Would overflow: wait for a read (in any process). There is no
            // pollable "space available" signal, and this only happens with
            // the counter near EFD_MAX, so poll the counter with short sleeps.
            thread::sleep(sleep);
            sleep = (sleep * 2).min(MAX_SLEEP);
        }
    }
}

/// Holds the page's lock; releases it on drop.
#[must_use]
struct Guard<'a> {
    lock: &'a AtomicU32,
    /// True if the lock was taken over from a process that died holding it.
    /// The protected state may be half-updated; the caller must repair it.
    recovered: bool,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.lock.store(0, Ordering::Release);
    }
}

/// Longest sleep between lock attempts once backoff is exhausted.
const MAX_SLEEP: Duration = Duration::from_millis(1);
const FIRST_SLEEP: Duration = Duration::from_micros(20);

impl State {
    /// Takes the lock (see the module docs for the waiting strategy).
    fn lock(&self) -> Guard<'_> {
        let me = current_pid();
        if self
            .lock
            .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Guard {
                lock: &self.lock,
                recovered: false,
            };
        }
        self.lock_slow(me)
    }

    #[cold]
    fn lock_slow(&self, me: u32) -> Guard<'_> {
        let backoff = Backoff::new();
        let mut sleep = FIRST_SLEEP;
        loop {
            // Test before test-and-set, so waiters don't fight over the line.
            let owner = self.lock.load(Ordering::Relaxed);
            if owner == 0 {
                if self
                    .lock
                    .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return Guard {
                        lock: &self.lock,
                        recovered: false,
                    };
                }
                backoff.spin();
                continue;
            }
            if !backoff.is_completed() {
                backoff.snooze();
                continue;
            }
            // Held for a while. Another thread of this process is alive by
            // definition; another process may have died holding it.
            if owner != me && !process_alive(owner) {
                if self
                    .lock
                    .compare_exchange(owner, me, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return Guard {
                        lock: &self.lock,
                        recovered: true,
                    };
                }
                continue;
            }
            thread::sleep(sleep);
            sleep = (sleep * 2).min(MAX_SLEEP);
        }
    }
}

/// Test hooks: raw access to simulate a process dying mid-update.
#[cfg(test)]
impl SharedPage {
    /// Current counter value.
    pub(crate) fn peek(&self) -> u64 {
        self.state().counter.load(Ordering::Relaxed)
    }

    /// Overwrites the counter without locking.
    pub(crate) fn poke(&self, value: u64) {
        self.state().counter.store(value, Ordering::Relaxed);
    }

    /// Takes the raw lock (no resync); returns it and whether it was recovered.
    pub(crate) fn lock_raw(&self) -> (impl Drop + '_, bool) {
        let g = self.state().lock();
        let recovered = g.recovered;
        (g, recovered)
    }

    /// Current holder's process id, or 0.
    pub(crate) fn holder(&self) -> u32 {
        self.state().lock.load(Ordering::Relaxed)
    }
}

#[cfg(unix)]
impl SharedPage {
    /// Takes over a shared memory descriptor received from another process,
    /// if it holds an eventfd page (left open otherwise).
    ///
    /// # Safety
    /// If it validates, `raw` must be a descriptor this process may own.
    pub(crate) unsafe fn adopt(raw: std::os::fd::RawFd) -> io::Result<Self> {
        // SAFETY: forwarded to the caller.
        let region = unsafe { SharedRegion::adopt(raw, size_of::<State>(), Self::is_page) }?;
        Ok(Self { region })
    }

    /// Like [`adopt`](Self::adopt) for a descriptor we already own (closed
    /// if it does not validate).
    pub(crate) fn from_owned(fd: std::os::fd::OwnedFd) -> io::Result<Self> {
        let region = SharedRegion::from_owned(fd, size_of::<State>(), Self::is_page)?;
        Ok(Self { region })
    }

    /// The shared memory object, to inherit or pass to other processes.
    pub(crate) fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.region.fd()
    }
}

#[cfg(windows)]
impl SharedPage {
    /// Takes over a section handle received from another process, if it
    /// holds an eventfd page (left open otherwise).
    ///
    /// # Safety
    /// If it validates, `raw` must be a handle this process may own.
    pub(crate) unsafe fn adopt(raw: std::os::windows::io::RawHandle) -> io::Result<Self> {
        // SAFETY: forwarded to the caller.
        let region = unsafe { SharedRegion::adopt(raw, size_of::<State>(), Self::is_page) }?;
        Ok(Self { region })
    }

    /// Like [`adopt`](Self::adopt) for a handle we already own (closed if it
    /// does not validate).
    pub(crate) fn from_owned(section: std::os::windows::io::OwnedHandle) -> io::Result<Self> {
        let region = SharedRegion::from_owned(section, size_of::<State>(), Self::is_page)?;
        Ok(Self { region })
    }

    /// The section handle, to duplicate into other processes.
    pub(crate) fn section(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.region.section()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_excludes_threads() {
        let page = Arc::new(SharedPage::new(0, EfdFlags::NONE).unwrap());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let page = Arc::clone(&page);
                thread::spawn(move || {
                    for _ in 0..5_000 {
                        let (g, recovered) = page.lock_raw();
                        assert!(!recovered);
                        // Non-atomic read-modify-write: only correct under the lock.
                        page.poke(page.peek() + 1);
                        drop(g);
                    }
                })
            })
            .collect();
        workers.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(page.peek(), 40_000);
        assert_eq!(page.holder(), 0);
    }

    #[test]
    fn holder_is_this_process() {
        let page = SharedPage::new(0, EfdFlags::NONE).unwrap();
        let (g, _) = page.lock_raw();
        assert_eq!(page.holder(), current_pid());
        drop(g);
        assert_eq!(page.holder(), 0);
    }

    #[test]
    fn this_process_is_alive() {
        assert!(process_alive(current_pid()));
    }

    #[cfg(unix)]
    fn wait_exited_unreaped(child: libc::pid_t) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_alive(child as u32) {
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_liveness_of_children() {
        // A child that exited but was not reaped yet counts as dead, and the
        // check must not reap it.
        // SAFETY: the child only calls _exit.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: async-signal-safe exit in the forked child.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0);
        wait_exited_unreaped(child);
        let mut status = 0;
        // SAFETY: reaps our own child; it must still be waitable.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        // Reaped and gone.
        assert!(!process_alive(child as u32));
    }

    #[cfg(unix)]
    #[test]
    fn unix_recovers_lock_of_dead_process() {
        let page = SharedPage::new(0, EfdFlags::NONE).unwrap();
        // SAFETY: the child only touches atomics and calls _exit.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // Take the lock and die holding it.
            std::mem::forget(page.lock_raw());
            // SAFETY: async-signal-safe exit in the forked child.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0);
        wait_exited_unreaped(child);
        assert_eq!(page.holder(), child as u32);
        let start = std::time::Instant::now();
        let (g, recovered) = page.lock_raw();
        assert!(recovered, "lock not taken over from the dead child");
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(g);
        assert!(!page.lock_raw().1);
        let mut status = 0;
        // SAFETY: reaps our own child.
        unsafe { libc::waitpid(child, &mut status, 0) };
    }

    #[cfg(windows)]
    #[test]
    fn windows_liveness_of_children() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(process_alive(pid));
        child.kill().unwrap();
        // `child` still holds a handle, so the exited process (and its id)
        // stays around and must be reported dead.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "child not seen as exited"
            );
            thread::sleep(Duration::from_millis(5));
        }
        child.wait().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_open_rejects_foreign_section() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        // SAFETY: a fresh unnamed, zero-filled section.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                4096,
                std::ptr::null(),
            )
        };
        assert!(!raw.is_null());
        // SAFETY: fresh handle we own; adopt must reject it and leave it open.
        let section = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: we own the handle; on failure adopt does not take it.
        let err = unsafe { SharedPage::adopt(section.as_raw_handle()) }.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        drop(section); // still ours to close
    }
}

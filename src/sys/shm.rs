//! Cross-process state for the unix shim.
//!
//! Each eventfd keeps its counter in a [`SharedPage`]: an anonymous
//! `MAP_SHARED` mapping. `fork(2)` keeps such mappings *shared* between parent
//! and child, so both keep working on one counter, as with a kernel eventfd.
//! Each process unmaps its view when it drops its last handle; the kernel
//! frees the page when the last process is done.
//!
//! The counter is guarded by a spinlock in the same page ([`State::lock`]):
//! process-local locks (`std::sync::Mutex`) cannot synchronise two processes.
//!
//! * Uncontended, locking is one compare-and-swap.
//! * Contended, waiters back off with `crossbeam_utils::Backoff` (spin, then
//!   yield), then sleep in growing steps capped at 1 ms, so a long wait costs
//!   almost no CPU. Critical sections are short: a counter update and at most
//!   one non-blocking pipe `read`/`write`.
//! * The lock word holds the holder's pid. A waiter that has backed off fully
//!   checks whether that process still exists; if it died while holding the
//!   lock (e.g. `SIGKILL`), the waiter takes the lock over and the guard
//!   reports [`Guard::recovered`] so the caller can repair its state.
//!
//! Limitations: the lock is not re-entrant, so calling into the same eventfd
//! from a signal handler that interrupted one of its operations deadlocks. If
//! the holder's pid is reused by a new process before a waiter notices the
//! death, the waiter keeps waiting.

use std::io;
use std::ptr::NonNull;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_utils::Backoff;

/// The state shared between every process that holds the eventfd.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct State {
    /// `0` when free, else the pid of the process holding the lock.
    lock: AtomicU32,
    /// Shared like `O_NONBLOCK` on a Linux eventfd's open file description.
    pub(crate) nonblocking: AtomicBool,
    /// Only written while holding `lock`.
    pub(crate) counter: AtomicU64,
}

/// One anonymous shared mapping holding a [`State`].
#[derive(Debug)]
pub(crate) struct SharedPage {
    ptr: NonNull<State>,
}

// SAFETY: `State` only contains atomics, which may be accessed from any thread
// (and any process mapping the page).
unsafe impl Send for SharedPage {}
// SAFETY: as above.
unsafe impl Sync for SharedPage {}

impl SharedPage {
    pub(crate) fn new(counter: u64, nonblocking: bool) -> io::Result<Self> {
        // SAFETY: a fresh anonymous mapping, no address hint, no fd.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size_of::<State>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast::<State>()).expect("mmap returned null");
        // SAFETY: the mapping is page-aligned, writable and large enough, and
        // nothing else can see it yet.
        unsafe {
            ptr.write(State {
                lock: AtomicU32::new(0),
                nonblocking: AtomicBool::new(nonblocking),
                counter: AtomicU64::new(counter),
            })
        };
        Ok(Self { ptr })
    }

    pub(crate) fn state(&self) -> &State {
        // SAFETY: initialised in `new`, unmapped only in `drop`. Other
        // processes only access it through atomics too.
        unsafe { self.ptr.as_ref() }
    }
}

impl Drop for SharedPage {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the mapping created in `new`; `&mut self`
        // means no reference into it is alive in this process. Other
        // processes keep their own views.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), size_of::<State>()) };
    }
}

/// Holds [`State::lock`]; releases it on drop.
#[must_use]
pub(crate) struct Guard<'a> {
    lock: &'a AtomicU32,
    /// True if the lock was taken over from a process that died holding it.
    /// The protected state may be half-updated; the caller must repair it.
    pub(crate) recovered: bool,
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
    pub(crate) fn lock(&self) -> Guard<'_> {
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

    /// Current holder's pid, or 0 (tests only).
    #[cfg(test)]
    pub(crate) fn holder(&self) -> u32 {
        self.lock.load(Ordering::Relaxed)
    }
}

/// This process's pid. Cached, and reset in the child after `fork` (where the
/// cached value would be the parent's).
fn current_pid() -> u32 {
    static PID: AtomicU32 = AtomicU32::new(0);
    static ATFORK: Once = Once::new();

    extern "C" fn reset_in_child() {
        PID.store(0, Ordering::Relaxed);
    }

    let pid = PID.load(Ordering::Relaxed);
    if pid != 0 {
        return pid;
    }
    ATFORK.call_once(|| {
        // SAFETY: registers a handler that only does an atomic store, which
        // is safe to run in the child right after fork.
        unsafe { libc::pthread_atfork(None, None, Some(reset_in_child)) };
    });
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() } as u32;
    PID.store(pid, Ordering::Relaxed);
    pid
}

/// False if process `pid` no longer exists, or is our child that has exited
/// but not been reaped yet (`kill(pid, 0)` still succeeds for such zombies).
fn process_alive(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    // SAFETY: an all-zero siginfo_t is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live siginfo_t. WNOWAIT leaves the child waitable,
    // so this never steals an exit status from the application.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r == 0 {
        // Our child: si_pid is set only if it has exited.
        // SAFETY: waitid filled `info` (or left it zeroed for "no change").
        return unsafe { info.si_pid() } == 0;
    }
    // Not our child (ECHILD): ask the kernel whether it exists.
    // SAFETY: signal 0 only checks for existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_excludes_threads() {
        let page = Arc::new(SharedPage::new(0, false).unwrap());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let page = Arc::clone(&page);
                thread::spawn(move || {
                    for _ in 0..5_000 {
                        let g = page.state().lock();
                        assert!(!g.recovered);
                        // Non-atomic read-modify-write: only correct under the lock.
                        let c = &page.state().counter;
                        c.store(c.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        workers.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(page.state().counter.load(Ordering::Relaxed), 40_000);
        assert_eq!(page.state().holder(), 0);
    }

    #[test]
    fn holder_is_this_process() {
        let page = SharedPage::new(0, false).unwrap();
        let g = page.state().lock();
        // SAFETY: getpid has no preconditions.
        assert_eq!(page.state().holder(), unsafe { libc::getpid() } as u32);
        drop(g);
        assert_eq!(page.state().holder(), 0);
    }

    #[test]
    fn liveness() {
        // SAFETY: getpid has no preconditions.
        assert!(process_alive(unsafe { libc::getpid() } as u32));
        // A child that exited but was not reaped yet counts as dead, and the
        // check must not reap it.
        // SAFETY: the child only calls _exit.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: async-signal-safe exit in the forked child.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_alive(child as u32) {
            assert!(
                std::time::Instant::now() < deadline,
                "child never seen as exited"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let mut status = 0;
        // SAFETY: reaps our own child; it must still be waitable.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        // Reaped and gone.
        assert!(!process_alive(child as u32));
    }

    #[test]
    fn recovers_lock_of_dead_process() {
        let page = SharedPage::new(0, false).unwrap();
        // SAFETY: the child only touches atomics and calls _exit.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // Take the lock and die holding it.
            std::mem::forget(page.state().lock());
            // SAFETY: async-signal-safe exit in the forked child.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0);
        // Wait until the child has exited (still unreaped) holding the lock.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_alive(child as u32) {
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(page.state().holder(), child as u32);
        let start = std::time::Instant::now();
        let g = page.state().lock();
        assert!(g.recovered, "lock not taken over from the dead child");
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(g);
        assert!(!page.state().lock().recovered);
        let mut status = 0;
        // SAFETY: reaps our own child.
        unsafe { libc::waitpid(child, &mut status, 0) };
    }
}

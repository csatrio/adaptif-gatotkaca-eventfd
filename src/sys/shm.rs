//! Cross-process eventfd state, shared by the unix and Windows shims.
//!
//! Each eventfd keeps its counter in a [`SharedPage`]:
//!
//! * unix: an anonymous `MAP_SHARED` mapping, which `fork(2)` keeps *shared*
//!   between parent and child;
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

use crate::error;
use crate::flags::{EFD_MAX, EfdFlags};

/// Marks a page as an eventfd page of this crate's layout (checked when a
/// Windows process imports one). "GKE" + layout version 1.
#[cfg_attr(unix, allow(dead_code))]
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

/// One shared mapping holding a [`State`].
#[derive(Debug)]
pub(crate) struct SharedPage {
    ptr: NonNull<State>,
    /// The section, kept open so it can be passed to other processes.
    #[cfg(windows)]
    section: std::os::windows::io::OwnedHandle,
}

// SAFETY: `State` only contains atomics, which may be accessed from any thread
// (and any process mapping the page).
unsafe impl Send for SharedPage {}
// SAFETY: as above.
unsafe impl Sync for SharedPage {}

impl SharedPage {
    /// A fresh page for an eventfd created with `initval` and `flags`.
    pub(crate) fn new(initval: u32, flags: EfdFlags) -> io::Result<Self> {
        let page = os::create()?;
        // SAFETY: the mapping is page-aligned, writable and large enough, and
        // nothing else can see it yet.
        unsafe {
            page.ptr.write(State {
                magic: AtomicU32::new(MAGIC),
                lock: AtomicU32::new(0),
                nonblocking: AtomicBool::new(flags.contains(EfdFlags::NONBLOCK)),
                semaphore: AtomicBool::new(flags.contains(EfdFlags::SEMAPHORE)),
                counter: AtomicU64::new(u64::from(initval)),
            })
        };
        Ok(page)
    }

    fn state(&self) -> &State {
        // SAFETY: initialised in `new` (or checked in `open`), unmapped only
        // in `drop`. Other processes only access it through atomics too.
        unsafe { self.ptr.as_ref() }
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

impl Drop for SharedPage {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the view created by `os::create`/`os::open`;
        // `&mut self` means no reference into it is alive in this process.
        // Other processes keep their own views.
        unsafe { os::unmap(self.ptr) };
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
        let me = os::current_pid();
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
            if owner != me && !os::process_alive(owner) {
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

// ---------------------------------------------------------------------------
// unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod os {
    use super::{SharedPage, State};
    use std::io;
    use std::ptr::NonNull;
    use std::sync::Once;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(super) fn create() -> io::Result<SharedPage> {
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
        Ok(SharedPage { ptr })
    }

    /// # Safety
    /// `ptr` must be a view created by `create`, not used afterwards.
    pub(super) unsafe fn unmap(ptr: NonNull<State>) {
        // SAFETY: forwarded to the caller.
        unsafe { libc::munmap(ptr.as_ptr().cast(), size_of::<State>()) };
    }

    /// This process's pid. Cached, and reset in the child after `fork`
    /// (where the cached value would be the parent's).
    pub(super) fn current_pid() -> u32 {
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
            // SAFETY: registers a handler that only does an atomic store,
            // which is safe to run in the child right after fork.
            unsafe { libc::pthread_atfork(None, None, Some(reset_in_child)) };
        });
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() } as u32;
        PID.store(pid, Ordering::Relaxed);
        pid
    }

    /// False if process `pid` no longer exists, or is our child that has
    /// exited but not been reaped yet (`kill(pid, 0)` still succeeds for
    /// such zombies).
    pub(super) fn process_alive(pid: u32) -> bool {
        let pid = pid as libc::pid_t;
        // SAFETY: an all-zero siginfo_t is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live siginfo_t. WNOWAIT leaves the child
        // waitable, so this never steals an exit status from the application.
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
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod os {
    use super::{MAGIC, SharedPage, State};
    use std::io;
    use std::mem::ManuallyDrop;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };

    /// Maps a view of `section`.
    fn map(section: &OwnedHandle) -> io::Result<NonNull<State>> {
        // SAFETY: `section` is an open file-mapping handle; we map exactly
        // the size it was created with (or less).
        let view = unsafe {
            MapViewOfFile(
                section.as_raw_handle(),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                size_of::<State>(),
            )
        };
        NonNull::new(view.Value.cast::<State>()).ok_or_else(io::Error::last_os_error)
    }

    pub(super) fn create() -> io::Result<SharedPage> {
        let size = size_of::<State>() as u32;
        // SAFETY: an unnamed, pagefile-backed section with default (not
        // inheritable) security; no other process can open it by name.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                size,
                std::ptr::null(),
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle that nothing else owns.
        let section = unsafe { OwnedHandle::from_raw_handle(raw) };
        let ptr = map(&section)?;
        Ok(SharedPage { ptr, section })
    }

    /// Takes over a section handle received from another process, if it
    /// holds an eventfd page. On failure the handle is left open (it may not
    /// be a section of ours at all).
    ///
    /// # Safety
    /// If it validates, `raw` must be a handle this process may own.
    pub(super) unsafe fn adopt(raw: RawHandle) -> io::Result<SharedPage> {
        // SAFETY: never dropped, so a bogus value is not closed here.
        let borrowed = ManuallyDrop::new(unsafe { OwnedHandle::from_raw_handle(raw) });
        let ptr = map(&borrowed)?;
        // SAFETY: `ptr` is a live view of at least size_of::<State>() bytes.
        let magic = unsafe { ptr.as_ref() }.magic.load(Ordering::Acquire);
        if magic != MAGIC {
            // SAFETY: unmaps the view we just created.
            unsafe { unmap(ptr) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an adaptif-gatotkaca-eventfd section",
            ));
        }
        Ok(SharedPage {
            ptr,
            section: ManuallyDrop::into_inner(borrowed),
        })
    }

    /// # Safety
    /// `ptr` must be a view created by `create`/`adopt`, not used afterwards.
    pub(super) unsafe fn unmap(ptr: NonNull<State>) {
        // SAFETY: forwarded to the caller. The section handle itself is
        // closed by `SharedPage`'s `OwnedHandle` field.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: ptr.as_ptr().cast(),
            })
        };
    }

    pub(super) fn current_pid() -> u32 {
        // SAFETY: no preconditions.
        unsafe { GetCurrentProcessId() }
    }

    /// False if process `pid` has exited or does not exist. An exited
    /// process's object stays signalled while handles to it are open, so
    /// this is exact for processes that still have a handle somewhere.
    pub(super) fn process_alive(pid: u32) -> bool {
        // SAFETY: plain call; the handle is closed below.
        let h = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if h.is_null() {
            // No such process (ERROR_INVALID_PARAMETER); any other error
            // (e.g. access denied) means it exists.
            // SAFETY: no preconditions.
            return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
        }
        // SAFETY: `h` is a valid process handle we own.
        let exited = unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0;
        // SAFETY: as above.
        unsafe { CloseHandle(h) };
        !exited
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
        unsafe { os::adopt(raw) }
    }

    /// The section handle, to duplicate into other processes.
    pub(crate) fn section(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        std::os::windows::io::AsHandle::as_handle(&self.section)
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
        assert_eq!(page.holder(), os::current_pid());
        drop(g);
        assert_eq!(page.holder(), 0);
    }

    #[test]
    fn this_process_is_alive() {
        assert!(os::process_alive(os::current_pid()));
    }

    #[cfg(unix)]
    fn wait_exited_unreaped(child: libc::pid_t) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while os::process_alive(child as u32) {
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
        assert!(!os::process_alive(child as u32));
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
        assert!(os::process_alive(pid));
        child.kill().unwrap();
        // `child` still holds a handle, so the exited process (and its id)
        // stays around and must be reported dead.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while os::process_alive(pid) {
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

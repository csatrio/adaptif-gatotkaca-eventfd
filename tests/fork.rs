//! The counter is shared across `fork(2)` on every unix platform: natively
//! (Linux, FreeBSD, ...) and on the shim (macOS, iOS, OpenBSD, DragonFly,
//! or any platform with `force-shim`). The same expectations run on both, so
//! on Linux they also check the shim against the kernel's behaviour.
//!
//! Children only call async-signal-safe-ish code (the eventfd operations,
//! which don't allocate) and leave with `_exit`, never through the test
//! harness.

#![cfg(unix)]

use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::{EFD_MAX, EfdFlags, GkEventFd};

/// Forks are serialised so no other test thread is mid-operation at fork time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `f` in a forked child; its result becomes the exit status (0 = ok).
fn spawn(f: impl FnOnce() -> bool) -> libc::pid_t {
    // SAFETY: the child runs `f` and exits with `_exit`, without returning
    // into the parent's stack or the test harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(false);
        // SAFETY: async-signal-safe exit from the forked child.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    pid
}

/// Exit status of `pid` if it has exited (reaps it), else `None`.
fn try_wait(pid: libc::pid_t) -> Option<i32> {
    let mut status = 0;
    // SAFETY: waits for our own child without blocking.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    assert!(r >= 0, "waitpid failed");
    (r == pid).then(|| {
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        }
    })
}

/// Waits up to 10 s for `pid` and asserts it succeeded (kills it on timeout).
fn join(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(code) = try_wait(pid) {
            assert_eq!(code, 0, "child {pid} failed");
            return;
        }
        if Instant::now() > deadline {
            // SAFETY: kills and reaps our own child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            panic!("child {pid} timed out");
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn readable(efd: &GkEventFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: efd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert!(n >= 0);
    pfd.revents & libc::POLLIN != 0
}

fn would_block<T>(r: std::io::Result<T>) -> bool {
    matches!(r, Err(e) if e.kind() == ErrorKind::WouldBlock)
}

#[test]
fn child_sees_parent_writes_and_back() {
    let _s = serial();
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    efd.write(7).unwrap();
    join(spawn(|| {
        efd.read().ok() == Some(7) && would_block(efd.read()) && efd.write(5).is_ok()
    }));
    assert!(readable(&efd));
    assert_eq!(efd.read().unwrap(), 5);
    assert!(!readable(&efd));
}

#[test]
fn blocking_read_in_child_woken_by_parent() {
    let _s = serial();
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let child = spawn(|| efd.read().ok() == Some(42));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(try_wait(child), None, "child did not block");
    efd.write(42).unwrap();
    join(child);
    efd.set_nonblocking(true).unwrap();
    assert!(would_block(efd.read()));
}

#[test]
fn blocking_read_in_parent_woken_by_child() {
    let _s = serial();
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let child = spawn(|| {
        thread::sleep(Duration::from_millis(100));
        efd.write(3).is_ok()
    });
    assert_eq!(efd.read().unwrap(), 3);
    join(child);
}

#[test]
fn blocked_write_in_child_unblocked_by_parent_read() {
    let _s = serial();
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    efd.write(EFD_MAX).unwrap();
    let child = spawn(|| efd.write(1).is_ok());
    thread::sleep(Duration::from_millis(100));
    assert_eq!(try_wait(child), None, "child did not block on overflow");
    assert_eq!(efd.read().unwrap(), EFD_MAX);
    join(child);
    assert_eq!(efd.read().unwrap(), 1);
}

#[test]
fn nonblocking_mode_is_shared() {
    let _s = serial();
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    join(spawn(|| efd.set_nonblocking(true).is_ok()));
    assert!(efd.is_nonblocking().unwrap());
    assert!(would_block(efd.read()));
}

#[test]
fn clones_made_before_fork_are_shared() {
    let _s = serial();
    let a = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    let b = a.try_clone().unwrap();
    join(spawn(|| b.write(2).is_ok() && a.write(3).is_ok()));
    assert_eq!(b.read().unwrap(), 5);
}

/// Several processes hammer one counter while the parent consumes it:
/// nothing may be lost or counted twice, and readiness must end up right.
#[test]
fn processes_contend_without_losing_counts() {
    let _s = serial();
    const CHILDREN: u64 = 4;
    const WRITES: u64 = 2_000;
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE).unwrap();
    let children: Vec<_> = (0..CHILDREN)
        .map(|_| spawn(|| (0..WRITES).all(|_| efd.write(1).is_ok())))
        .collect();
    let mut consumed = 0u64;
    let deadline = Instant::now() + Duration::from_secs(20);
    while consumed < CHILDREN * WRITES {
        assert!(Instant::now() < deadline, "only consumed {consumed}");
        match efd.read() {
            Ok(n) => consumed += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::yield_now(),
            Err(e) => panic!("{e}"),
        }
    }
    children.into_iter().for_each(join);
    assert_eq!(consumed, CHILDREN * WRITES);
    assert!(would_block(efd.read()));
    assert!(!readable(&efd));
}

/// A child dropping (closing) its copies must not affect the parent.
#[test]
fn child_exit_keeps_parent_working() {
    let _s = serial();
    let efd = GkEventFd::new(1, EfdFlags::NONBLOCK).unwrap();
    join(spawn(|| {
        let copy = efd.try_clone();
        let ok = copy.is_ok();
        drop(copy);
        ok
    }));
    assert_eq!(efd.read().unwrap(), 1);
    efd.write(9).unwrap();
    assert_eq!(efd.read().unwrap(), 9);
}

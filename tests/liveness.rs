//! The public liveness check `process_alive` (since 0.3.3): `false` only on
//! evidence that a process has exited (an unreaped zombie counts as
//! exited), `true` while it runs and whenever the OS can't tell; an own
//! child is never reaped by the check.
//!
//! Every test kills and reaps what it starts, also when an assertion fails.

use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::process_alive;

/// Waits until `process_alive(pid)` turns false.
fn wait_dead(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid}) still counts as alive");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Kills and reaps a child when dropped (also on a failed assertion).
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A child that runs until killed.
fn long_child() -> Reaped {
    #[cfg(unix)]
    let child = Command::new("sleep").arg("30").spawn();
    #[cfg(windows)]
    let child = Command::new("cmd").args(["/C", "ping -n 30 127.0.0.1 >NUL"]).spawn();
    Reaped(child.unwrap())
}

#[test]
fn this_process_is_alive() {
    assert!(process_alive(std::process::id()));
}

#[test]
fn pids_that_are_not_looked_up() {
    assert!(process_alive(0));
    #[cfg(unix)]
    {
        // Cast to pid_t these would be process groups (`kill(-1, 0)`).
        assert!(process_alive(i32::MAX as u32 + 1));
        assert!(process_alive(u32::MAX));
    }
}

/// Alive while it runs; dead once killed although nobody reaped it; the
/// check leaves it for our `wait`; dead after reaping.
#[test]
fn own_child_alive_killed_unreaped_and_reaped() {
    let mut child = long_child();
    let pid = child.0.id();
    assert!(process_alive(pid));
    child.0.kill().unwrap();
    wait_dead(pid, "a killed, unreaped child");
    for _ in 0..100 {
        assert!(!process_alive(pid));
    }
    #[cfg(unix)]
    {
        // Still a zombie: the checks above did not reap it.
        // SAFETY: signal 0 on our own child's pid (a zombie keeps it).
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0, "the child was reaped by the check");
        let status = child.0.wait().expect("the check reaped our child");
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(libc::SIGKILL));
    }
    #[cfg(windows)]
    child.0.wait().unwrap();
    assert!(!process_alive(pid), "a reaped child is gone");
}

#[test]
fn nonexistent_process_is_dead() {
    #[cfg(unix)]
    let mut child = Command::new("true").spawn().unwrap();
    #[cfg(windows)]
    let mut child = Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    drop(child);
    assert!(!process_alive(pid), "an exited, reaped child (pid {pid}) is gone");
    // No unix hands out this pid (Linux pid_max <= 2^22, Apple and the BSDs 99999).
    #[cfg(unix)]
    assert!(!process_alive(i32::MAX as u32));
}

/// Another parent's zombie: a grandchild killed while its parent (our
/// child) never waits for it.
///
/// Forked rather than started through a shell: a shell may reap a
/// background job killed before it `exec`s the non-reaping program.
#[cfg(unix)]
#[test]
fn unreaped_grandchild_is_dead() {
    /// Kills the middle process and the grandchild, and reaps the middle
    /// one (init reaps the grandchild once its parent is gone).
    struct Cleanup(libc::pid_t, libc::pid_t);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for pid in [self.1, self.0].into_iter().filter(|&p| p > 0) {
                // SAFETY: signals a process this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            // SAFETY: reaps our own child.
            unsafe { libc::waitpid(self.0, std::ptr::null_mut(), 0) };
        }
    }
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: the forked processes only call fork, write, pause and _exit
    // (async-signal-safe), never returning into the test harness.
    let middle = unsafe { libc::fork() };
    if middle == 0 {
        // SAFETY: as above.
        let grandchild = unsafe { libc::fork() };
        if grandchild == 0 {
            loop {
                // SAFETY: waits for a signal (SIGKILL ends it).
                unsafe { libc::pause() };
            }
        }
        // SAFETY: writes the pid from a live local.
        unsafe { libc::write(fds[1], (&raw const grandchild).cast(), size_of::<libc::pid_t>()) };
        // Never reaps it.
        loop {
            // SAFETY: as above.
            unsafe { libc::pause() };
        }
    }
    assert!(middle > 0);
    let mut cleanup = Cleanup(middle, 0);
    let mut grandchild: libc::pid_t = 0;
    // SAFETY: closes our copy of the write end, once; then reads one pid
    // into a live local and closes the read end, once.
    let n = unsafe {
        libc::close(fds[1]);
        let n = libc::read(fds[0], (&raw mut grandchild).cast(), size_of::<libc::pid_t>());
        libc::close(fds[0]);
        n
    };
    assert_eq!(n as usize, size_of::<libc::pid_t>());
    assert!(grandchild > 0);
    cleanup.1 = grandchild;
    let pid = grandchild as u32;

    assert!(process_alive(middle as u32), "our live child is alive");
    assert!(process_alive(pid), "a running grandchild is alive");
    // SAFETY: signals the grandchild this test started.
    assert_eq!(unsafe { libc::kill(grandchild, libc::SIGKILL) }, 0);
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "freebsd", target_os = "netbsd"))]
    wait_dead(pid, "a killed grandchild its parent does not reap");
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "freebsd", target_os = "netbsd")))]
    {
        // No zombie check here: alive until its parent reaps it (documented).
        thread::sleep(Duration::from_millis(200));
        assert!(process_alive(pid));
    }
    // SAFETY: signal 0; the zombie must still exist, or the test proved nothing.
    assert_eq!(unsafe { libc::kill(grandchild, 0) }, 0, "the grandchild was reaped: the test proves nothing");
    assert!(process_alive(middle as u32), "its parent still runs");
}

/// A process this one may not signal (or not open) counts as alive: unix
/// pid 1 (init / launchd; `EPERM` unless we are root), Windows pid 4
/// (System).
#[test]
fn process_we_may_not_signal_is_alive() {
    #[cfg(unix)]
    {
        // SAFETY: plain syscalls; signal 0 sends nothing.
        let (euid, r) = unsafe { (libc::geteuid(), libc::kill(1, 0)) };
        if euid != 0 {
            assert_eq!(r, -1, "pid 1 should not be ours to signal");
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
        }
        assert!(process_alive(1));
    }
    #[cfg(windows)]
    assert!(process_alive(4));
}

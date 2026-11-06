//! Windows: sharing an eventfd with child processes, through both handover
//! paths (`export_inheritable` + environment variable, `export_to` + stdin).
//!
//! Each test re-runs this test binary as the child, filtered to
//! `child_entry`, with `GK_CHILD_ROLE` naming what the child should do. The
//! child imports the eventfd and panics (non-zero exit) if a check fails.

#![cfg(windows)]

use std::io::{BufRead, ErrorKind, Write};
use std::os::windows::io::AsRawSocket;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};

/// How the child receives its token.
enum Via<'a> {
    /// In `GK_TOKEN` (inheritable export).
    Env(&'a str),
    /// On stdin, after spawn (`export_to(child.id())`).
    Stdin,
}

fn spawn(role: &str, via: Via<'_>) -> Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("GK_CHILD_ROLE", role);
    match via {
        Via::Env(token) => {
            cmd.env("GK_TOKEN", token);
        }
        Via::Stdin => {
            cmd.stdin(Stdio::piped());
        }
    }
    cmd.spawn().unwrap()
}

/// Spawns a child that reads its token from stdin and sends it one.
fn spawn_with_export_to(efd: &GkEventFd, role: &str) -> Child {
    let mut child = spawn(role, Via::Stdin);
    let token = efd.export_to(child.id()).unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{token}").unwrap();
    child
}

/// Waits up to 20 s for the child and asserts it succeeded.
fn join(mut child: Child) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child failed: {status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn readable_within(efd: &GkEventFd, ms: i32) -> bool {
    let mut fd = WSAPOLLFD {
        fd: efd.as_raw_socket() as SOCKET,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: one valid WSAPOLLFD; the socket is open.
    let n = unsafe { WSAPoll(&mut fd, 1, ms) };
    assert!(n >= 0);
    n > 0
}

fn would_block<T>(r: std::io::Result<T>) -> bool {
    matches!(r, Err(e) if e.kind() == ErrorKind::WouldBlock)
}

/// The child side. Does nothing unless spawned by a test below.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var("GK_CHILD_ROLE") else {
        return;
    };
    let token = match std::env::var("GK_TOKEN") {
        Ok(token) => token,
        Err(_) => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            line
        }
    };
    // SAFETY: the parent made this token for this process, once.
    let efd = unsafe { GkEventFd::import(&token) }.expect("import");
    match role.as_str() {
        "read5_write7" => {
            assert_eq!(efd.read().unwrap(), 5);
            assert!(would_block(efd.read()));
            efd.write(7).unwrap();
        }
        "blocking_read42" => assert_eq!(efd.read().unwrap(), 42),
        "sleep_write3" => {
            thread::sleep(Duration::from_millis(200));
            efd.write(3).unwrap();
        }
        "write_ones" => (0..1_000).for_each(|_| efd.write(1).unwrap()),
        "set_nonblocking" => efd.set_nonblocking(true).unwrap(),
        "readable_then_read1" => {
            // The parent wrote 1 before spawning us: our socket is readable.
            assert!(readable_within(&efd, 2_000));
            assert_eq!(efd.read().unwrap(), 1);
        }
        "clone_and_drop" => drop(efd.try_clone().unwrap()),
        other => panic!("unknown role {other}"),
    }
}

#[test]
fn inheritable_export_shares_both_ways() {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    efd.write(5).unwrap();
    let export = efd.export_inheritable().unwrap();
    let child = spawn("read5_write7", Via::Env(export.token()));
    drop(export);
    join(child);
    assert!(readable_within(&efd, 2_000));
    assert_eq!(efd.read().unwrap(), 7);
    assert!(!readable_within(&efd, 0));
}

#[test]
fn export_to_running_child_shares_both_ways() {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    efd.write(5).unwrap();
    join(spawn_with_export_to(&efd, "read5_write7"));
    assert_eq!(efd.read().unwrap(), 7);
}

#[test]
fn blocking_read_in_child_woken_by_parent() {
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let mut child = spawn_with_export_to(&efd, "blocking_read42");
    thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "child did not block");
    efd.write(42).unwrap();
    join(child);
}

#[test]
fn blocking_read_in_parent_woken_by_child() {
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let export = efd.export_inheritable().unwrap();
    let child = spawn("sleep_write3", Via::Env(export.token()));
    drop(export);
    assert_eq!(efd.read().unwrap(), 3);
    join(child);
}

/// Several processes hammer one counter while the parent consumes it.
#[test]
fn children_contend_without_losing_counts() {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE).unwrap();
    let export = efd.export_inheritable().unwrap();
    let children: Vec<_> = (0..3)
        .map(|_| spawn("write_ones", Via::Env(export.token())))
        .collect();
    drop(export);
    let mut consumed = 0u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while consumed < 3_000 {
        assert!(Instant::now() < deadline, "only consumed {consumed}");
        match efd.read() {
            Ok(n) => consumed += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::yield_now(),
            Err(e) => panic!("{e}"),
        }
    }
    children.into_iter().for_each(join);
    assert_eq!(consumed, 3_000);
    assert!(would_block(efd.read()));
    assert!(!readable_within(&efd, 0));
}

#[test]
fn nonblocking_mode_is_shared() {
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    join(spawn_with_export_to(&efd, "set_nonblocking"));
    assert!(efd.is_nonblocking().unwrap());
    assert!(would_block(efd.read()));
}

#[test]
fn child_sees_readiness() {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    efd.write(1).unwrap();
    join(spawn_with_export_to(&efd, "readable_then_read1"));
    assert!(!readable_within(&efd, 0));
}

#[test]
fn child_exit_keeps_parent_working() {
    let efd = GkEventFd::new(1, EfdFlags::NONBLOCK).unwrap();
    join(spawn_with_export_to(&efd, "clone_and_drop"));
    assert_eq!(efd.read().unwrap(), 1);
    efd.write(9).unwrap();
    assert_eq!(efd.read().unwrap(), 9);
}

#[test]
fn import_rejects_garbage() {
    // SAFETY: rejected before any handle is touched.
    let err = unsafe { GkEventFd::import("not a token") }.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

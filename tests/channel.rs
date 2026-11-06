//! `GkChannel`: one-way message channels over shared memory.

use std::io::ErrorKind;
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::{ChannelOptions, GkChannel, GkReceiver, GkSender};

fn small() -> (GkSender, GkReceiver) {
    GkChannel::with_options(ChannelOptions {
        capacity: 4096,
        max_message: 1000,
    })
    .unwrap()
}

fn kind<T: std::fmt::Debug>(r: std::io::Result<T>) -> ErrorKind {
    r.unwrap_err().kind()
}

/// Readable within `ms` milliseconds (poll / WSAPoll on the end).
#[cfg(unix)]
fn readable(fd: &impl std::os::fd::AsRawFd, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut p, 1, ms) };
    assert!(n >= 0);
    n > 0
}

#[cfg(windows)]
fn readable(s: &impl std::os::windows::io::AsRawSocket, ms: i32) -> bool {
    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};
    let mut fd = WSAPOLLFD {
        fd: s.as_raw_socket() as SOCKET,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: one valid WSAPOLLFD.
    let n = unsafe { WSAPoll(&mut fd, 1, ms) };
    assert!(n >= 0);
    n > 0
}

#[test]
fn defaults_and_options() {
    let (tx, rx) = GkChannel::new().unwrap();
    assert_eq!(tx.capacity(), 16 << 20);
    assert_eq!(rx.max_message(), 1 << 20);
    for (capacity, max_message) in [(100, 8), (32, 8), (4096, 2041), (4096, 4096)] {
        let r = GkChannel::with_options(ChannelOptions {
            capacity,
            max_message,
        });
        assert_eq!(kind(r), ErrorKind::InvalidInput, "{capacity}/{max_message}");
    }
}

#[test]
fn messages_keep_boundaries_and_order() {
    let (mut tx, mut rx) = small();
    tx.send(b"one").unwrap();
    tx.send(b"").unwrap();
    tx.send(&[7; 1000]).unwrap();
    let mut buf = [0u8; 1000];
    assert_eq!(rx.recv(&mut buf).unwrap(), 3);
    assert_eq!(&buf[..3], b"one");
    assert_eq!(rx.next_len().unwrap(), Some(0));
    assert_eq!(rx.recv(&mut buf).unwrap(), 0);
    let mut v = Vec::new();
    assert_eq!(rx.recv_vec(&mut v).unwrap(), 1000);
    assert!(v.iter().all(|&b| b == 7));
    assert_eq!(rx.next_len().unwrap(), None);
}

#[test]
fn size_limits() {
    let (mut tx, mut rx) = small();
    assert_eq!(kind(tx.send(&[0; 1001])), ErrorKind::InvalidInput);
    tx.send(b"twelve bytes").unwrap();
    // Too small a buffer: refused, and the message stays queued.
    let mut tiny = [0u8; 4];
    assert_eq!(kind(rx.recv(&mut tiny)), ErrorKind::InvalidInput);
    assert_eq!(rx.next_len().unwrap(), Some(12));
    assert_eq!(rx.recv_with(|m| m.to_vec()).unwrap(), b"twelve bytes");
}

#[test]
fn reserve_commit_in_place() {
    let (mut tx, mut rx) = small();
    let mut slot = tx.reserve(100).unwrap();
    slot[..5].copy_from_slice(b"hello");
    slot.commit(5); // shorter than reserved
    {
        let _abandoned = tx.reserve(10).unwrap(); // never committed: nothing sent
    }
    tx.send(b"next").unwrap();
    assert_eq!(rx.recv_with(|m| m.to_vec()).unwrap(), b"hello");
    assert_eq!(rx.recv_with(|m| m.to_vec()).unwrap(), b"next");
    assert_eq!(rx.next_len().unwrap(), None);
}

#[test]
fn nonblocking_empty_and_full() {
    let (mut tx, mut rx) = small();
    tx.set_nonblocking(true);
    rx.set_nonblocking(true);
    let mut buf = [0u8; 1000];
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::WouldBlock);
    let mut sent = 0;
    loop {
        match tx.send(&[1; 100]) {
            Ok(()) => sent += 1,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(sent >= 30, "only {sent} messages fit");
    assert_eq!(rx.recv(&mut buf).unwrap(), 100);
    tx.send(&[1; 100]).unwrap(); // space again
}

#[test]
fn closing_the_sender_drains_then_breaks() {
    let (mut tx, mut rx) = small();
    tx.send(b"last words").unwrap();
    drop(tx);
    let mut buf = [0u8; 64];
    assert_eq!(rx.recv(&mut buf).unwrap(), 10);
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::BrokenPipe);
    assert_eq!(kind(rx.next_len()), ErrorKind::BrokenPipe);
}

#[test]
fn closing_the_receiver_breaks_the_sender() {
    let (mut tx, rx) = small();
    drop(rx);
    assert_eq!(kind(tx.send(b"x")), ErrorKind::BrokenPipe);
}

#[test]
fn blocked_receiver_wakes_on_send_and_on_close() {
    let (mut tx, mut rx) = small();
    let t = thread::spawn(move || {
        let mut buf = [0u8; 16];
        let n = rx.recv(&mut buf).unwrap();
        let after = rx.recv(&mut buf);
        (n, after.map_err(|e| e.kind()))
    });
    thread::sleep(Duration::from_millis(100));
    tx.send(b"wake").unwrap();
    thread::sleep(Duration::from_millis(100));
    drop(tx);
    assert_eq!(t.join().unwrap(), (4, Err(ErrorKind::BrokenPipe)));
}

#[test]
fn blocked_sender_wakes_on_recv_and_on_close() {
    let (mut tx, mut rx) = small();
    tx.set_nonblocking(true);
    while tx.send(&[0; 500]).is_ok() {}
    tx.set_nonblocking(false);
    let t = thread::spawn(move || {
        tx.send(&[1; 500]).unwrap(); // blocks until a recv frees space
        while tx.send(&[2; 500]).is_ok() {} // then blocks until rx is dropped
        tx
    });
    thread::sleep(Duration::from_millis(100));
    let mut buf = [0u8; 500];
    rx.recv(&mut buf).unwrap();
    thread::sleep(Duration::from_millis(100));
    drop(rx);
    let mut tx = t.join().unwrap();
    assert_eq!(kind(tx.send(b"x")), ErrorKind::BrokenPipe);
}

#[test]
fn readiness_follows_the_waiting_protocol() {
    let (mut tx, mut rx) = small();
    rx.set_nonblocking(true);
    let mut buf = [0u8; 64];
    // The receiver announces it is waiting by hitting WouldBlock...
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::WouldBlock);
    assert!(!readable(&rx, 0));
    // ...so the next send rings its doorbell.
    tx.send(b"ding").unwrap();
    assert!(readable(&rx, 2000));
    assert_eq!(rx.recv(&mut buf).unwrap(), 4);
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::WouldBlock);
    assert!(!readable(&rx, 0), "doorbell drained by WouldBlock");

    // Same for a full sender.
    tx.set_nonblocking(true);
    while tx.send(&[0; 500]).is_ok() {}
    assert!(!readable(&tx, 0));
    rx.recv(&mut [0u8; 500]).unwrap();
    assert!(readable(&tx, 2000));
    tx.send(&[0; 500]).unwrap();
}

/// Many messages of random sizes through blocking ends on two threads.
#[test]
fn stress_two_threads() {
    let (mut tx, mut rx) = GkChannel::with_options(ChannelOptions {
        capacity: 1 << 16,
        max_message: 4000,
    })
    .unwrap();
    const N: u32 = 200_000;
    let size = |i: u32| (i.wrapping_mul(2654435761) % 4001) as usize;
    let writer = thread::spawn(move || {
        let mut msg = Vec::new();
        for i in 0..N {
            msg.clear();
            msg.extend((0..size(i)).map(|k| (k as u32 ^ i) as u8));
            tx.send(&msg).unwrap();
        }
    });
    let mut buf = vec![0u8; 4000];
    for i in 0..N {
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(n, size(i), "message {i}");
        assert!(
            buf[..n]
                .iter()
                .enumerate()
                .all(|(k, &b)| b == (k as u32 ^ i) as u8)
        );
    }
    writer.join().unwrap();
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::BrokenPipe);
}

#[cfg(feature = "mio")]
#[test]
fn mio_receiver_events() {
    use mio::{Events, Interest, Poll, Token};
    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(4);
    let (mut tx, mut rx) = small();
    rx.set_nonblocking(true);
    poll.registry()
        .register(&mut rx, Token(1), Interest::READABLE)
        .unwrap();
    let writer = thread::spawn(move || {
        for i in 0..50u8 {
            tx.send(&[i; 10]).unwrap();
            thread::sleep(Duration::from_millis(2));
        }
    });
    let mut got = 0u8;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 10];
    'outer: loop {
        // Drain until WouldBlock (also announces we are waiting).
        loop {
            match rx.recv(&mut buf) {
                Ok(_) => {
                    assert_eq!(buf, [got; 10]);
                    got += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::BrokenPipe => break 'outer,
                Err(e) => panic!("{e}"),
            }
        }
        assert!(Instant::now() < deadline, "stuck at {got}");
        poll.poll(&mut events, Some(Duration::from_millis(500)))
            .unwrap();
    }
    writer.join().unwrap();
    assert_eq!(got, 50);
}

#[cfg(unix)]
mod fork {
    use super::*;

    fn spawn(f: impl FnOnce() -> bool) -> libc::pid_t {
        // SAFETY: the child runs `f` and exits with `_exit`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(false);
            // SAFETY: async-signal-safe exit from the forked child.
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        pid
    }

    fn wait(pid: libc::pid_t) -> i32 {
        let mut status = 0;
        // SAFETY: waits for our own child.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        }
    }

    /// Serialise the fork tests (no other test thread mid-operation).
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn child_sends_parent_receives() {
        let _s = serial();
        let (tx, mut rx) = small();
        let mut tx = Some(tx);
        let child = spawn(|| {
            let mut tx = tx.take().unwrap();
            tx.claim();
            (0..1000u32).all(|i| tx.send(&i.to_le_bytes()).is_ok())
        });
        // The parent doesn't use the sender: detach so dropping it here does
        // not close the channel for the child.
        tx.take().unwrap().detach();
        let mut buf = [0u8; 4];
        for i in 0..1000u32 {
            assert_eq!(rx.recv(&mut buf).unwrap(), 4);
            assert_eq!(u32::from_le_bytes(buf), i);
        }
        // The child exits (dropping its owned sender): closed.
        assert_eq!(wait(child), 0);
        assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::BrokenPipe);
    }

    #[test]
    fn using_an_end_owned_by_another_process_fails() {
        let _s = serial();
        let (mut tx, rx) = small();
        let mut rx = Some(rx);
        let child = spawn(|| {
            let mut rx = rx.take().unwrap();
            rx.claim();
            rx.recv(&mut [0u8; 16]).is_ok()
        });
        // Wait until the child owns the receiver, then the parent's copy is
        // no longer usable.
        let mut parent_rx = rx.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while parent_rx.next_len().is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(kind(parent_rx.next_len()), ErrorKind::PermissionDenied);
        drop(parent_rx); // not the owner: doesn't close
        tx.send(b"for child").unwrap();
        assert_eq!(wait(child), 0);
    }

    #[test]
    fn killed_writer_is_detected() {
        let _s = serial();
        let (tx, mut rx) = small();
        let mut tx = Some(tx);
        let child = spawn(|| {
            let mut tx = tx.take().unwrap();
            tx.claim();
            tx.send(b"hello").unwrap();
            // SAFETY: die without dropping (no close flag).
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
            true
        });
        tx.take().unwrap().detach();
        let mut buf = [0u8; 8];
        assert_eq!(rx.recv(&mut buf).unwrap(), 5);
        let start = Instant::now();
        assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::BrokenPipe);
        assert!(start.elapsed() < Duration::from_secs(5));
        wait(child);
    }
}

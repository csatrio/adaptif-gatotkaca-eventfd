//! Multi-producer channels (`GkChannel::new_mpsc`, `GkMpSender`), batch
//! receive, and the receiver's readiness guarantees.

use std::collections::HashSet;
use std::io::{ErrorKind, Read};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::{ChannelOptions, GkChannel, GkMpSender, GkReceiver, GkSender};

fn small_mpsc() -> (GkMpSender, GkReceiver) {
    GkChannel::mpsc_with_options(ChannelOptions { capacity: 4096, max_message: 1000, ..Default::default() }).unwrap()
}

fn small() -> (GkSender, GkReceiver) {
    GkChannel::with_options(ChannelOptions { capacity: 4096, max_message: 1000, ..Default::default() }).unwrap()
}

/// Sends one message through either kind of sender.
type SendFn = Box<dyn Fn(&[u8])>;

fn kind<T: std::fmt::Debug>(r: std::io::Result<T>) -> ErrorKind {
    r.unwrap_err().kind()
}

/// Readable within `ms` milliseconds (poll / WSAPoll on the end).
#[cfg(unix)]
fn readable(fd: &impl std::os::fd::AsRawFd, ms: i32) -> bool {
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut p, 1, ms) };
    assert!(n >= 0);
    n > 0
}

#[cfg(windows)]
fn readable(s: &impl std::os::windows::io::AsRawSocket, ms: i32) -> bool {
    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};
    let mut fd = WSAPOLLFD { fd: s.as_raw_socket() as SOCKET, events: POLLRDNORM, revents: 0 };
    // SAFETY: one valid WSAPOLLFD.
    let n = unsafe { WSAPoll(&mut fd, 1, ms) };
    assert!(n >= 0);
    n > 0
}

// ---------------------------------------------------------------------------
// Readiness guarantees (both channel kinds)
// ---------------------------------------------------------------------------

/// A receiver registered with a poller before it ever called `recv` wakes on
/// the first message: a fresh channel starts with the receiver waiting.
#[test]
fn fresh_receiver_wakes_on_the_first_message() {
    let (mut tx, rx) = small();
    assert!(!readable(&rx, 0));
    tx.send(b"first").unwrap();
    assert!(readable(&rx, 2000));

    let (tx, rx) = small_mpsc();
    assert!(!readable(&rx, 0));
    tx.send(b"first").unwrap();
    assert!(readable(&rx, 2000));

    // Bulk lane too.
    let (mut tx, rx) = small();
    tx.send_bulk(&[1; 10]).unwrap();
    assert!(readable(&rx, 2000));
}

/// Stopping before `WouldBlock` (an event loop's budget) leaves the
/// receiver readable; only `WouldBlock` clears it.
#[test]
fn stopping_early_keeps_the_receiver_readable() {
    for mpsc in [false, true] {
        let (send, mut rx): (SendFn, GkReceiver) = if mpsc {
            let (tx, rx) = small_mpsc();
            (Box::new(move |m: &[u8]| tx.send(m).unwrap()), rx)
        } else {
            let (tx, rx) = small();
            let tx = std::cell::RefCell::new(tx);
            (Box::new(move |m: &[u8]| tx.borrow_mut().send(m).unwrap()), rx)
        };
        rx.set_nonblocking(true);
        for i in 0..10u8 {
            send(&[i; 16]);
        }
        assert!(readable(&rx, 2000));
        let mut seen = Vec::new();
        assert_eq!(rx.recv_batch(4, |_, m| seen.push(m[0])).unwrap(), 4);
        assert!(readable(&rx, 0), "6 left: still readable (mpsc={mpsc})");
        assert_eq!(rx.recv_batch(4, |_, m| seen.push(m[0])).unwrap(), 4);
        assert!(readable(&rx, 0));
        assert_eq!(rx.recv_batch(100, |_, m| seen.push(m[0])).unwrap(), 2);
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
        // Not drained to WouldBlock yet: the doorbell may still be set...
        assert_eq!(kind(rx.recv_batch(100, |_, _| ())), ErrorKind::WouldBlock);
        // ...but WouldBlock clears it.
        assert!(!readable(&rx, 0), "mpsc={mpsc}");
        send(b"again");
        assert!(readable(&rx, 2000));
    }
}

/// Data left in the other lane keeps the receiver readable after one lane
/// returned `WouldBlock`, so a poller never strands it.
#[test]
fn data_in_the_other_lane_keeps_the_receiver_readable() {
    let (mut tx, mut rx) = small();
    rx.set_nonblocking(true);
    tx.send_bulk(&[9; 100]).unwrap();
    tx.send(b"m").unwrap();
    assert!(readable(&rx, 2000));
    assert_eq!(rx.recv(&mut [0u8; 8]).unwrap(), 1);
    assert_eq!(kind(rx.recv(&mut [0u8; 8])), ErrorKind::WouldBlock);
    assert!(readable(&rx, 0), "the bulk transfer is still waiting");
    let mut data = Vec::new();
    rx.recv_bulk_vec(&mut data, 1 << 20).unwrap();
    assert_eq!(data, vec![9; 100]);
    assert_eq!(kind(rx.recv(&mut [0u8; 8])), ErrorKind::WouldBlock);
    assert_eq!(kind(rx.bulk_reader().map(drop)), ErrorKind::WouldBlock);
    assert!(!readable(&rx, 0));
}

#[test]
fn recv_batch_rules() {
    let (mut tx, mut rx) = small();
    assert_eq!(rx.recv_batch(0, |_, _| unreachable!()).unwrap(), 0);
    // Blocking: waits for the first message only.
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        tx.send_kind(7, b"late").unwrap();
        tx
    });
    let mut got = Vec::new();
    assert_eq!(rx.recv_batch(10, |k, m| got.push((k, m.to_vec()))).unwrap(), 1);
    assert_eq!(got, vec![(7, b"late".to_vec())]);
    let mut tx = t.join().unwrap();
    // A panicking callback: the messages it returned from count as received.
    for i in 0..3u8 {
        tx.send(&[i]).unwrap();
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rx.recv_batch(10, |_, m| assert_ne!(m[0], 1, "boom"))));
    assert!(r.is_err());
    let mut rest = Vec::new();
    rx.recv_batch(10, |_, m| rest.push(m[0])).unwrap();
    assert_eq!(rest, vec![1, 2], "0 was consumed; 1 panicked, so it is still queued");
    drop(tx);
    assert_eq!(kind(rx.recv_batch(10, |_, _| ())), ErrorKind::BrokenPipe);
}

// ---------------------------------------------------------------------------
// GkMpSender
// ---------------------------------------------------------------------------

/// Many threads, blocking sends through a small ring, one receiver: every
/// message arrives once, each thread's in its own order.
#[test]
fn many_threads_keep_per_thread_order() {
    let (tx, mut rx) = small_mpsc();
    let threads = 8u32;
    let per = 20_000u32;
    let start = Arc::new(Barrier::new(threads as usize));
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let tx = tx.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                for i in 0..per {
                    let len = 4 + (i as usize * 7 + t as usize) % 120;
                    let mut msg = vec![t as u8; len];
                    msg[..4].copy_from_slice(&i.to_le_bytes());
                    tx.send_kind(t as u16, &msg).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    let mut next = vec![0u32; threads as usize];
    let mut total = 0;
    loop {
        match rx.recv_batch(64, |k, m| {
            let i = u32::from_le_bytes(m[..4].try_into().unwrap());
            assert_eq!(i, next[k as usize], "thread {k} out of order");
            assert_eq!(m.len(), 4 + (i as usize * 7 + k as usize) % 120);
            assert!(m[4..].iter().all(|&b| b == k as u8));
            next[k as usize] += 1;
        }) {
            Ok(n) => total += n,
            Err(e) => {
                assert_eq!(e.kind(), ErrorKind::BrokenPipe, "closed after the last clone");
                break;
            }
        }
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(total, (threads * per) as usize);
}

#[test]
fn try_send_never_waits_and_arms_the_doorbell() {
    let (tx, mut rx) = small_mpsc();
    let mut sent = 0;
    while tx.try_send(&[1; 500]).is_ok() {
        sent += 1;
    }
    assert!(sent >= 6);
    assert_eq!(kind(tx.try_send(&[1; 500])), ErrorKind::WouldBlock);
    assert!(!readable(&tx, 0));
    rx.recv(&mut [0u8; 500]).unwrap();
    assert!(readable(&tx, 2000), "space freed: the sender's doorbell rang");
    tx.try_send(&[1; 500]).unwrap();
    // Non-blocking mode makes the blocking calls behave like try_*.
    tx.set_nonblocking(true);
    assert!(tx.is_nonblocking());
    while tx.send(&[1; 500]).is_ok() {}
    assert_eq!(kind(tx.send(&[1; 500])), ErrorKind::WouldBlock);
}

#[test]
fn blocked_senders_wake_when_space_is_freed() {
    let (tx, mut rx) = small_mpsc();
    while tx.try_send(&[0; 900]).is_ok() {}
    let waiters: Vec<_> = (0..4u8)
        .map(|t| {
            let tx = tx.clone();
            thread::spawn(move || tx.send(&[t + 1; 900]).unwrap())
        })
        .collect();
    thread::sleep(Duration::from_millis(50));
    let mut buf = vec![0u8; 1000];
    let mut late = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while late.len() < 4 {
        assert!(Instant::now() < deadline, "blocked senders never woke");
        let n = rx.recv(&mut buf).unwrap();
        if buf[0] != 0 {
            assert_eq!(n, 900);
            late.insert(buf[0]);
        }
    }
    for w in waiters {
        w.join().unwrap();
    }
}

#[test]
fn reserve_in_place_shrink_and_abandon() {
    let (tx, mut rx) = small_mpsc();
    let mut slot = tx.reserve_kind(3, 100).unwrap();
    slot[..5].copy_from_slice(b"hello");
    // Another thread's message reserved after ours waits behind it.
    let other = {
        let tx = tx.clone();
        thread::spawn(move || tx.send_kind(4, b"behind").unwrap())
    };
    other.join().unwrap();
    rx.set_nonblocking(true);
    assert_eq!(kind(rx.recv(&mut [0u8; 64])), ErrorKind::WouldBlock);
    slot.commit(5);
    let mut buf = [0u8; 128];
    assert_eq!(rx.recv_kind(&mut buf).unwrap(), (3, 5));
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(rx.recv_kind(&mut buf).unwrap(), (4, 6));
    // A dropped slot sends nothing, and doesn't block what follows.
    let dropped = tx.reserve(64).unwrap();
    tx.clone().send(b"after").unwrap_or_else(|e| panic!("{e}"));
    drop(dropped);
    assert_eq!(rx.recv(&mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"after");
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::WouldBlock);
    assert_eq!(kind(tx.reserve(1001).map(drop)), ErrorKind::InvalidInput);
    assert_eq!(tx.max_message(), 1000);
    assert_eq!(tx.capacity(), 4096);
}

#[test]
fn closing_rules() {
    // Dropping every clone closes the channel after the data.
    let (tx, mut rx) = small_mpsc();
    let tx2 = tx.clone();
    tx.send(b"a").unwrap();
    drop(tx);
    assert!(!rx.peer_gone());
    tx2.send(b"b").unwrap();
    drop(tx2);
    assert!(rx.peer_gone());
    let mut buf = [0u8; 8];
    assert_eq!(rx.recv(&mut buf).unwrap(), 1);
    assert_eq!(rx.recv(&mut buf).unwrap(), 1);
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::BrokenPipe);
    // Dropping the receiver breaks every clone.
    let (tx, rx) = small_mpsc();
    assert_eq!(rx.peer_pid(), Some(std::process::id()));
    assert!(!tx.peer_gone());
    drop(rx);
    assert!(tx.peer_gone());
    assert_eq!(kind(tx.try_send(b"x")), ErrorKind::BrokenPipe);
    assert_eq!(kind(tx.clone().send(b"x")), ErrorKind::BrokenPipe);
}

#[test]
fn bulk_transfers_are_serialised_between_threads() {
    let (tx, mut rx) = GkChannel::mpsc_with_options(ChannelOptions {
        capacity: 4096,
        max_message: 1000,
        bulk_capacity: 1 << 14,
        chunk: 1024,
        ..Default::default()
    })
    .unwrap();
    let senders: Vec<_> = (0..3u8)
        .map(|t| {
            let tx = tx.clone();
            thread::spawn(move || {
                for round in 0..5u8 {
                    let data = vec![t * 16 + round; 50_000];
                    tx.bulk().send_bulk_kind(u16::from(t), &data).unwrap();
                    tx.send_kind(u16::from(t), &[round]).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    let receiver = thread::spawn(move || {
        let mut transfers = 0;
        loop {
            match rx.bulk_reader() {
                Ok(mut r) => {
                    let t = r.kind() as u8;
                    let mut data = Vec::new();
                    r.read_to_end(&mut data).unwrap();
                    assert_eq!(data.len(), 50_000);
                    assert!(data.iter().all(|&b| b / 16 == t), "chunks of two transfers interleaved");
                    transfers += 1;
                }
                Err(e) => {
                    assert_eq!(e.kind(), ErrorKind::BrokenPipe);
                    break;
                }
            }
        }
        let mut msgs = 0;
        while rx.recv(&mut [0u8; 8]).is_ok() {
            msgs += 1;
        }
        (transfers, msgs)
    });
    for s in senders {
        s.join().unwrap();
    }
    assert_eq!(receiver.join().unwrap(), (15, 15));
}

/// A plain `GkSender` sent where a `GkMpSender` is expected (and back) is
/// refused; the end is closed so the receiver doesn't hang.
#[cfg(unix)]
#[test]
fn wrong_kind_of_sender_is_refused_and_closes() {
    use std::os::unix::net::UnixStream;
    let (a, b) = UnixStream::pair().unwrap();
    let (tx, mut rx) = small_mpsc();
    tx.send_to(&a).unwrap();
    assert_eq!(kind(tx.try_send(b"x")), ErrorKind::PermissionDenied, "handed over: unusable here");
    assert_eq!(kind(GkSender::recv_from(&b)), ErrorKind::InvalidData);
    assert_eq!(kind(rx.recv(&mut [0u8; 8])), ErrorKind::BrokenPipe);

    let (tx, mut rx) = small();
    tx.send_to(&a).unwrap();
    assert_eq!(kind(GkMpSender::recv_from(&b)), ErrorKind::InvalidData);
    assert_eq!(kind(rx.recv(&mut [0u8; 8])), ErrorKind::BrokenPipe);

    // The right kind works, and the receiver end takes either.
    let (tx, mut rx) = small_mpsc();
    tx.send_to(&a).unwrap();
    let moved = GkMpSender::recv_from(&b).unwrap();
    moved.send(b"moved").unwrap();
    assert_eq!(rx.recv(&mut [0u8; 8]).unwrap(), 5);
}

/// After `fork`, the child claims the multi-producer sender and the parent
/// detaches its copy: the channel stays open until the child's end closes.
#[cfg(unix)]
#[test]
fn fork_claim_and_detach() {
    let (tx, mut rx) = small_mpsc();
    let extra = tx.clone();
    // SAFETY: the child only sends through the channel and leaves with `_exit`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tx.claim();
            let sent = (0..1000u32).all(|i| tx.send(&i.to_le_bytes()).is_ok());
            drop((tx, extra));
            sent
        }))
        .unwrap_or(false);
        // SAFETY: async-signal-safe exit from the forked child.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    let mut buf = [0u8; 4];
    for i in 0..1000u32 {
        assert_eq!(rx.recv(&mut buf).unwrap(), 4);
        assert_eq!(u32::from_le_bytes(buf), i);
    }
    let mut status = 0;
    // SAFETY: waits for our own child.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(kind(rx.recv(&mut buf)), ErrorKind::BrokenPipe);
    // The parent's clones were claimed by the child: unusable here. Detach
    // them (dropping them would not close the channel anyway).
    assert_eq!(kind(extra.try_send(b"x")), ErrorKind::PermissionDenied);
    drop(extra);
    tx.detach();
}

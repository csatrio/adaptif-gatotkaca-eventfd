//! `GkChannel`: one-way message channels over shared memory.

use std::io::ErrorKind;
use std::thread;
use std::time::{Duration, Instant};

use std::io::{Read, Write};

use adaptif_gatotkaca_eventfd::{ChannelOptions, GkChannel, GkReceiver, GkSender};

fn small() -> (GkSender, GkReceiver) {
    GkChannel::with_options(ChannelOptions {
        capacity: 4096,
        max_message: 1000,
        ..Default::default()
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
    let (msg, bulk) = if cfg!(windows) {
        (256 << 10, 1 << 20)
    } else {
        (1 << 20, 4 << 20)
    };
    assert_eq!(tx.capacity(), msg);
    assert_eq!(tx.bulk_capacity(), bulk);
    assert_eq!(rx.max_message(), 64 << 10);
    assert_eq!(rx.chunk(), 64 << 10);
    let bad_chunk = GkChannel::with_options(ChannelOptions {
        bulk_capacity: 1 << 16,
        chunk: 32 << 10, // 32 KiB + 32 > 64 KiB / 2
        ..Default::default()
    });
    assert_eq!(kind(bad_chunk), ErrorKind::InvalidInput);
    for (capacity, max_message) in [(100, 8), (32, 8), (4096, 2041), (4096, 4096)] {
        let r = GkChannel::with_options(ChannelOptions {
            capacity,
            max_message,
            ..Default::default()
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
        ..Default::default()
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

// ---------------------------------------------------------------------------
// Bulk lane
// ---------------------------------------------------------------------------

/// A channel with a small bulk ring, so transfers wrap many times.
fn small_bulk() -> (GkSender, GkReceiver) {
    GkChannel::with_options(ChannelOptions {
        capacity: 4096,
        max_message: 1000,
        bulk_capacity: 1 << 16,
        chunk: 16 << 10,
    })
    .unwrap()
}

/// Deterministic test data.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2654435761).wrapping_add(seed) as u8)
        .collect()
}

#[test]
fn bulk_much_larger_than_the_ring() {
    let (mut tx, mut rx) = small_bulk();
    let data = pattern(64 << 20, 1); // 64 MiB through a 64 KiB ring
    let expect = data.clone();
    let t = thread::spawn(move || tx.send_bulk(&data).unwrap());
    let mut out = Vec::new();
    let id = rx.recv_bulk_vec(&mut out, 128 << 20).unwrap();
    assert_eq!(id, t.join().unwrap());
    assert!(out == expect, "64 MiB transfer differs");
}

#[test]
fn bulk_streams_of_unknown_length() {
    let (mut tx, mut rx) = small_bulk();
    let data = pattern(5_000_003, 2);
    let expect = data.clone();
    let t = thread::spawn(move || {
        let mut w = tx.bulk_writer(None).unwrap();
        for piece in data.chunks(7919) {
            w.write_all(piece).unwrap();
        }
        w.finish().unwrap();
        w.id()
    });
    let mut r = rx.bulk_reader().unwrap();
    assert_eq!(r.len(), None);
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    assert_eq!(r.received(), expect.len() as u64);
    assert_eq!(r.id(), t.join().unwrap());
    drop(r);
    assert!(out == expect);
}

#[test]
fn bulk_transfers_back_to_back() {
    let (mut tx, mut rx) = small_bulk();
    let sizes: Vec<usize> = (0..60u32)
        .map(|i| match i % 6 {
            0 => 0,
            1 => 1,
            2 => 16 << 10,       // exactly one chunk
            3 => (16 << 10) + 1, // one byte over
            4 => (i as usize) * 997,
            _ => 300_000,
        })
        .collect();
    let s2 = sizes.clone();
    let t = thread::spawn(move || {
        let mut ids = Vec::new();
        for (i, len) in s2.into_iter().enumerate() {
            ids.push(tx.send_bulk(&pattern(len, i as u32)).unwrap());
        }
        ids
    });
    let mut out = Vec::new();
    let mut got = Vec::new();
    for (i, len) in sizes.iter().enumerate() {
        got.push(rx.recv_bulk_vec(&mut out, 1 << 20).unwrap());
        assert_eq!(out.len(), *len, "transfer {i}");
        assert!(out == pattern(*len, i as u32), "transfer {i} differs");
    }
    let ids = t.join().unwrap();
    assert_eq!(got, ids);
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn messages_overtake_a_bulk_transfer() {
    let (mut tx, mut rx) = small_bulk();
    tx.set_nonblocking(true);
    // Fill the bulk ring mid-transfer (nobody reads the bulk lane yet).
    let data = pattern(1 << 20, 3);
    let mut w = tx.bulk_writer(Some(data.len() as u64)).unwrap();
    let mut off = w.write(&data).unwrap();
    assert_eq!(
        w.write(&data[off..]).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    drop(w); // abandons the transfer (blocks briefly only for the marker)
    // The message lane is independent: this goes straight through...
    let mut tx = tx;
    tx.send(b"urgent").unwrap();
    let mut buf = [0u8; 16];
    // ...and arrives before any of the bulk data is read.
    assert_eq!(rx.recv(&mut buf).unwrap(), 6);
    assert_eq!(&buf[..6], b"urgent");
    // The bulk data that made it, then the abort.
    let mut r = rx.bulk_reader().unwrap();
    let mut got = vec![0u8; off];
    r.read_exact(&mut got).unwrap();
    assert!(got == data[..off]);
    assert_eq!(
        r.read(&mut buf).unwrap_err().kind(),
        ErrorKind::ConnectionAborted
    );
    off = 0;
    let _ = off;
}

#[test]
fn messages_flow_while_a_transfer_is_in_progress() {
    let (mut tx, mut rx) = small_bulk();
    let t = thread::spawn(move || {
        let mut w = tx.bulk_writer(None).unwrap();
        w.write_all(&pattern(30 << 10, 4)).unwrap(); // partly in the ring
        drop(w);
        // The transfer is open on the bulk lane; messages still go through.
        tx.send(b"ping").unwrap();
        tx
    });
    let mut buf = [0u8; 8];
    // The message arrives without reading the bulk lane first.
    assert_eq!(rx.recv(&mut buf).unwrap(), 4);
    assert_eq!(&buf[..4], b"ping");
    let _tx = t.join().unwrap();
}

#[test]
fn bulk_abort_and_early_reader_drop() {
    let (mut tx, mut rx) = small_bulk();
    // A writer dropped mid-transfer: the reader gets ConnectionAborted.
    let mut w = tx.bulk_writer(Some(1000)).unwrap();
    w.write_all(&[1; 400]).unwrap();
    drop(w);
    tx.send_bulk(b"second").unwrap();
    let mut r = rx.bulk_reader().unwrap();
    let mut buf = [0u8; 1000];
    assert_eq!(r.read(&mut buf).unwrap(), 400);
    assert_eq!(
        r.read(&mut buf).unwrap_err().kind(),
        ErrorKind::ConnectionAborted
    );
    drop(r);
    // A reader dropped early: the rest is skipped before the next transfer.
    tx.send_bulk(&pattern(50_000, 5)).unwrap();
    tx.send_bulk(b"fourth").unwrap();
    let mut out = Vec::new();
    rx.recv_bulk_vec(&mut out, 100).unwrap();
    assert_eq!(out, b"second");
    let mut r = rx.bulk_reader().unwrap();
    assert_eq!(r.len(), Some(50_000));
    r.read_exact(&mut buf[..10]).unwrap();
    drop(r); // 49 990 bytes unread
    rx.recv_bulk_vec(&mut out, 100).unwrap();
    assert_eq!(out, b"fourth");
}

#[test]
fn bulk_length_rules_and_limit() {
    let (mut tx, mut rx) = small_bulk();
    // Writing more than declared is refused.
    let mut w = tx.bulk_writer(Some(4)).unwrap();
    assert_eq!(
        w.write(b"12345").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    w.write_all(b"1234").unwrap();
    w.finish().unwrap();
    drop(w);
    // Finishing short is refused, and the receiver sees an abort.
    let mut w = tx.bulk_writer(Some(10)).unwrap();
    w.write_all(b"123").unwrap();
    assert_eq!(w.finish().unwrap_err().kind(), ErrorKind::InvalidInput);
    drop(w);
    // A zero-length transfer.
    tx.send_bulk(&[]).unwrap();
    // Too large for the receiver's limit: refused and skipped.
    tx.send_bulk(&[9; 5000]).unwrap();
    tx.send_bulk(b"ok").unwrap();

    let mut out = Vec::new();
    rx.recv_bulk_vec(&mut out, 100).unwrap();
    assert_eq!(out, b"1234");
    assert_eq!(
        rx.recv_bulk_vec(&mut out, 100).unwrap_err().kind(),
        ErrorKind::ConnectionAborted
    );
    rx.recv_bulk_vec(&mut out, 100).unwrap();
    assert!(out.is_empty());
    assert_eq!(
        rx.recv_bulk_vec(&mut out, 100).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    rx.recv_bulk_vec(&mut out, 100).unwrap();
    assert_eq!(out, b"ok");
}

#[test]
fn bulk_nonblocking_partial_writes() {
    let (mut tx, mut rx) = small_bulk();
    tx.set_nonblocking(true);
    let data = pattern(1 << 20, 6);
    let mut w = tx.bulk_writer(Some(data.len() as u64)).unwrap();
    // The ring holds less than the data: the first write is partial.
    let n = w.write(&data).unwrap();
    assert!(n > 0 && n < data.len(), "wrote {n}");
    assert_eq!(
        w.write(&data[n..]).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    // Drain on another thread while we keep writing what fits.
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        rx.recv_bulk_vec(&mut out, 2 << 20).unwrap();
        out
    });
    let mut off = n;
    while off < data.len() {
        match w.write(&data[off..]) {
            Ok(k) => off += k,
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::yield_now(),
            Err(e) => panic!("{e}"),
        }
    }
    w.finish().unwrap(); // auto-completed at the declared length
    drop(w);
    assert!(reader.join().unwrap() == data);
}

#[test]
fn bulk_sender_gone_mid_transfer() {
    let (mut tx, mut rx) = small_bulk();
    let mut w = tx.bulk_writer(None).unwrap();
    w.write_all(&[3; 100]).unwrap();
    // Forget the writer so no abort record is written, then drop the sender.
    std::mem::forget(w);
    drop(tx);
    let mut r = rx.bulk_reader().unwrap();
    let mut buf = [0u8; 200];
    assert_eq!(r.read(&mut buf).unwrap(), 100);
    assert_eq!(r.read(&mut buf).unwrap_err().kind(), ErrorKind::BrokenPipe);
}

#[test]
fn bulk_for_each_chunk_in_place() {
    let (mut tx, mut rx) = small_bulk();
    let data = pattern(200_000, 7);
    let expect = data.clone();
    let t = thread::spawn(move || tx.send_bulk(&data).unwrap());
    let mut r = rx.bulk_reader().unwrap();
    let mut out = Vec::new();
    let mut chunks = 0;
    let n = r
        .for_each_chunk(|c| {
            assert!(c.len() <= 16 << 10);
            out.extend_from_slice(c);
            chunks += 1;
        })
        .unwrap();
    t.join().unwrap();
    assert_eq!(n, expect.len() as u64);
    assert!(chunks >= 13);
    assert!(out == expect);
}

/// An event loop draining both lanes in non-blocking mode, pausing bulk
/// reads in between: no wake-up may be lost (each lane's data must get
/// through even though both share one doorbell).
#[cfg(feature = "mio")]
#[test]
fn mio_event_loop_with_both_lanes() {
    use mio::{Events, Interest, Poll, Token};
    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(4);
    let (mut tx, mut rx) = small_bulk();
    rx.set_nonblocking(true);
    poll.registry()
        .register(&mut rx, Token(0), Interest::READABLE)
        .unwrap();
    const N: u32 = 600;
    let writer = thread::spawn(move || {
        for i in 0..N {
            if i % 3 == 0 {
                tx.send_bulk(&pattern((i as usize * 1013) % 70_000, i))
                    .unwrap();
            } else {
                tx.send(&i.to_le_bytes()).unwrap();
            }
            if i % 50 == 0 {
                thread::sleep(Duration::from_millis(1));
            }
        }
    });
    let (mut msgs, mut transfers, mut bulk) = (0u32, 0u32, Vec::new());
    let mut buf = [0u8; 8];
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut sender_gone = (false, false);
    while !(sender_gone.0 && sender_gone.1) {
        // Message lane until WouldBlock.
        loop {
            match rx.recv(&mut buf) {
                Ok(4) => msgs += 1,
                Ok(n) => panic!("message of {n} bytes"),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                    sender_gone.0 = true;
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        // Bulk lane until WouldBlock, pausing an unfinished transfer.
        loop {
            let mut r = match rx.bulk_reader() {
                Ok(r) => r,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                    sender_gone.1 = true;
                    break;
                }
                Err(e) => panic!("{e}"),
            };
            let mut chunk = [0u8; 4096];
            let finished = loop {
                match r.read(&mut chunk) {
                    Ok(0) => break true,
                    Ok(n) => bulk.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                    Err(e) => panic!("{e}"),
                }
            };
            if !finished {
                r.pause();
                break;
            }
            let i = transfers * 3;
            assert!(
                bulk == pattern((i as usize * 1013) % 70_000, i),
                "transfer {i}"
            );
            bulk.clear();
            transfers += 1;
        }
        assert!(
            Instant::now() < deadline,
            "stuck: {msgs} messages, {transfers} transfers"
        );
        if !(sender_gone.0 && sender_gone.1) {
            poll.poll(&mut events, Some(Duration::from_secs(2)))
                .unwrap();
        }
    }
    writer.join().unwrap();
    assert_eq!(transfers, N.div_ceil(3));
    assert_eq!(msgs, N - N.div_ceil(3));
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
    fn killed_bulk_sender_is_detected() {
        let _s = serial();
        let (tx, mut rx) = small_bulk();
        let mut tx = Some(tx);
        let child = spawn(|| {
            let mut tx = tx.take().unwrap();
            tx.claim();
            let mut w = tx.bulk_writer(Some(1 << 20)).unwrap();
            w.write_all(&[5; 1000]).unwrap();
            // SAFETY: die mid-transfer without dropping anything.
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
            true
        });
        tx.take().unwrap().detach();
        let mut r = rx.bulk_reader().unwrap();
        let mut buf = [0u8; 2000];
        assert_eq!(r.read(&mut buf).unwrap(), 1000);
        let start = Instant::now();
        assert_eq!(r.read(&mut buf).unwrap_err().kind(), ErrorKind::BrokenPipe);
        assert!(start.elapsed() < Duration::from_secs(5));
        wait(child);
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

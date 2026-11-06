//! `mio::event::Source` integration (`--features mio`).

#![cfg(feature = "mio")]

use std::io::ErrorKind;
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use mio::{Events, Interest, Poll, Token};

const SHORT: Duration = Duration::from_millis(50);
const LONG: Duration = Duration::from_secs(5);

/// Polls once and returns the readable tokens that fired.
fn poll_tokens(poll: &mut Poll, timeout: Duration) -> Vec<Token> {
    let mut events = Events::with_capacity(16);
    poll.poll(&mut events, Some(timeout)).unwrap();
    let mut tokens: Vec<Token> = events
        .iter()
        .filter(|e| e.is_readable())
        .map(|e| e.token())
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Polls until `want` tokens have all fired (or `LONG` elapses).
fn wait_for(poll: &mut Poll, want: &[Token]) -> Vec<Token> {
    let deadline = Instant::now() + LONG;
    let mut seen = Vec::new();
    while Instant::now() < deadline && !want.iter().all(|t| seen.contains(t)) {
        seen.extend(poll_tokens(poll, SHORT));
        seen.sort();
        seen.dedup();
    }
    seen
}

#[test]
fn readable_event_after_write() {
    let mut poll = Poll::new().unwrap();
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    poll.registry()
        .register(&mut efd, Token(1), Interest::READABLE)
        .unwrap();

    assert!(
        poll_tokens(&mut poll, SHORT).is_empty(),
        "no event before a write"
    );
    efd.write(3).unwrap();
    assert_eq!(wait_for(&mut poll, &[Token(1)]), vec![Token(1)]);
    assert_eq!(efd.read().unwrap(), 3);
}

#[test]
fn initval_is_readable_on_register() {
    let mut poll = Poll::new().unwrap();
    let mut efd = GkEventFd::new(1, EfdFlags::NONBLOCK).unwrap();
    poll.registry()
        .register(&mut efd, Token(0), Interest::READABLE)
        .unwrap();
    assert_eq!(wait_for(&mut poll, &[Token(0)]), vec![Token(0)]);
}

#[test]
fn edge_triggered_rearm_after_drain() {
    let mut poll = Poll::new().unwrap();
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    poll.registry()
        .register(&mut efd, Token(7), Interest::READABLE)
        .unwrap();

    for round in 1..=5u64 {
        efd.write(round).unwrap();
        assert_eq!(
            wait_for(&mut poll, &[Token(7)]),
            vec![Token(7)],
            "round {round}"
        );
        // Drain until WouldBlock, as edge-triggered readiness requires.
        assert_eq!(efd.read().unwrap(), round);
        assert_eq!(efd.read().unwrap_err().kind(), ErrorKind::WouldBlock);
        // Drained and nothing new: no further event.
        assert!(poll_tokens(&mut poll, SHORT).is_empty(), "round {round}");
    }
}

#[test]
fn semaphore_drain_loop() {
    let mut poll = Poll::new().unwrap();
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE).unwrap();
    poll.registry()
        .register(&mut efd, Token(0), Interest::READABLE)
        .unwrap();
    efd.write(4).unwrap();
    assert_eq!(wait_for(&mut poll, &[Token(0)]), vec![Token(0)]);
    let mut n = 0;
    loop {
        match efd.read() {
            Ok(v) => n += v,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(n, 4);
    assert!(poll_tokens(&mut poll, SHORT).is_empty());
}

#[test]
fn clones_with_distinct_tokens_both_fire() {
    let mut poll = Poll::new().unwrap();
    let mut a = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    let mut b = a.try_clone().unwrap();
    poll.registry()
        .register(&mut a, Token(10), Interest::READABLE)
        .unwrap();
    poll.registry()
        .register(&mut b, Token(20), Interest::READABLE)
        .unwrap();

    a.write(1).unwrap();
    assert_eq!(
        wait_for(&mut poll, &[Token(10), Token(20)]),
        vec![Token(10), Token(20)]
    );
    assert_eq!(b.read().unwrap(), 1);

    // Again, written through the other handle.
    b.write(2).unwrap();
    assert_eq!(
        wait_for(&mut poll, &[Token(10), Token(20)]),
        vec![Token(10), Token(20)]
    );
    assert_eq!(a.read().unwrap(), 2);
}

#[test]
fn deregister_and_reregister() {
    let mut poll = Poll::new().unwrap();
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    poll.registry()
        .register(&mut efd, Token(1), Interest::READABLE)
        .unwrap();
    poll.registry().deregister(&mut efd).unwrap();

    efd.write(1).unwrap();
    assert!(
        poll_tokens(&mut poll, SHORT).is_empty(),
        "no event after deregister"
    );
    assert_eq!(efd.read().unwrap(), 1);

    poll.registry()
        .register(&mut efd, Token(2), Interest::READABLE)
        .unwrap();
    poll.registry()
        .reregister(&mut efd, Token(3), Interest::READABLE)
        .unwrap();
    efd.write(1).unwrap();
    assert_eq!(wait_for(&mut poll, &[Token(3)]), vec![Token(3)]);
    poll.registry().deregister(&mut efd).unwrap();
}

#[test]
fn cross_thread_wakeup() {
    let mut poll = Poll::new().unwrap();
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    poll.registry()
        .register(&mut efd, Token(0), Interest::READABLE)
        .unwrap();

    const N: u64 = 100;
    let notifier = efd.try_clone().unwrap();
    let writer = thread::spawn(move || {
        for _ in 0..N {
            notifier.write(1).unwrap();
            thread::sleep(Duration::from_micros(200));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Events::with_capacity(8);
    let mut total = 0;
    while total < N {
        assert!(Instant::now() < deadline, "timed out with total = {total}");
        poll.poll(&mut events, Some(Duration::from_millis(100)))
            .unwrap();
        for event in &events {
            assert_eq!(event.token(), Token(0));
            loop {
                match efd.read() {
                    Ok(n) => total += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                }
            }
        }
    }
    writer.join().unwrap();
    assert_eq!(total, N);
}

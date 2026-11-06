//! Waking a mio event loop from other threads.
//!
//! Worker threads each hold a clone of the eventfd and `write` to it when
//! they have finished a job. The loop thread sleeps in `Poll::poll` and wakes
//! when the counter becomes non-zero, on every platform.
//!
//! Run with: `cargo run --example mio_loop --features mio`

#[cfg(feature = "mio")]
fn main() -> std::io::Result<()> {
    use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
    use mio::{Events, Interest, Poll, Token};
    use std::io::ErrorKind;
    use std::time::Duration;

    const WAKER: Token = Token(0);
    const WORKERS: u64 = 4;

    let mut poll = Poll::new()?;
    // Non-blocking: mio readiness is edge-triggered, so we read until WouldBlock.
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
    poll.registry()
        .register(&mut efd, WAKER, Interest::READABLE)?;

    for id in 1..=WORKERS {
        let notifier = efd.try_clone()?;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50 * id));
            // Each worker reports one finished job.
            notifier.write(1).expect("eventfd write");
        });
    }

    let mut events = Events::with_capacity(8);
    let mut finished = 0;
    while finished < WORKERS {
        poll.poll(&mut events, Some(Duration::from_secs(5)))?;
        if events.is_empty() {
            println!("timed out, still waiting");
        }
        for event in &events {
            if event.token() == WAKER {
                // Normal mode: one read returns the sum of all writes so far.
                loop {
                    match efd.read() {
                        Ok(n) => {
                            finished += n;
                            println!("woken: {n} job(s) done, {finished}/{WORKERS}");
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
    println!("all workers finished");
    Ok(())
}

#[cfg(not(feature = "mio"))]
fn main() {
    eprintln!("this example needs the `mio` feature: cargo run --example mio_loop --features mio");
}

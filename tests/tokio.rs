//! tokio integration patterns.
//!
//! * Unix: `tokio::io::unix::AsyncFd<GkEventFd>`.
//! * Windows: tokio cannot register a foreign socket, so the documented
//!   pattern is a blocking read inside `spawn_blocking`.

use std::sync::Arc;

use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io;
    use std::time::Duration;
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    /// Registers an eventfd with the tokio reactor for read readiness.
    fn register(efd: GkEventFd) -> AsyncFd<GkEventFd> {
        // SAFETY: `GkEventFd` owns its descriptor, which stays open until the
        // `GkEventFd` is dropped, and `as_raw_fd` always returns that same fd.
        unsafe { AsyncFd::register_with_interest(efd, Interest::READABLE) }.unwrap()
    }

    /// Awaits readiness and reads once; loops on spurious readiness.
    async fn read_async(efd: &AsyncFd<GkEventFd>) -> io::Result<u64> {
        loop {
            let mut guard = efd.readable().await?;
            match guard.try_io(|inner| inner.get_ref().read()) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }

    async fn consume(flags: EfdFlags, writes: u64) -> u64 {
        let efd = Arc::new(register(
            GkEventFd::new(0, flags | EfdFlags::NONBLOCK).unwrap(),
        ));
        // Expected sum: 1 + 2 + ... + writes.
        let expected = writes * (writes + 1) / 2;

        let producer = {
            let efd = Arc::clone(&efd);
            std::thread::spawn(move || {
                for i in 1..=writes {
                    efd.get_ref().write(i).unwrap();
                    if i % 16 == 0 {
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
            })
        };

        let mut sum = 0;
        while sum < expected {
            sum += tokio::time::timeout(Duration::from_secs(10), read_async(&efd))
                .await
                .expect("timed out waiting for the producer")
                .unwrap();
        }
        producer.join().unwrap();
        assert_eq!(sum, expected);
        // Nothing left over.
        assert_eq!(
            efd.get_ref().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        sum
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asyncfd_receives_sum_normal_mode() {
        consume(EfdFlags::NONE, 500).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asyncfd_receives_sum_semaphore_mode() {
        // In semaphore mode each read returns 1, so the sum counts reads.
        consume(EfdFlags::SEMAPHORE, 100).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asyncfd_producer_task() {
        let efd = Arc::new(register(GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap()));
        let producer = {
            let efd = Arc::clone(&efd);
            tokio::spawn(async move {
                for _ in 0..50 {
                    efd.get_ref().write(2).unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        let mut sum = 0;
        while sum < 100 {
            sum += read_async(&efd).await.unwrap();
        }
        producer.await.unwrap();
        assert_eq!(sum, 100);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asyncfd_many_waiters_on_clones() {
        // Several tasks, each with its own clone (own fd), share one counter.
        const TASKS: u64 = 4;
        let base = GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE).unwrap();
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            let efd = register(base.try_clone().unwrap());
            handles.push(tokio::spawn(async move { read_async(&efd).await.unwrap() }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        base.write(TASKS).unwrap();
        let mut total = 0;
        for h in handles {
            total += tokio::time::timeout(Duration::from_secs(10), h)
                .await
                .expect("waiter timed out")
                .unwrap();
        }
        assert_eq!(total, TASKS);
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    /// Blocking read on tokio's blocking pool.
    async fn read_blocking(efd: Arc<GkEventFd>) -> std::io::Result<u64> {
        tokio::task::spawn_blocking(move || efd.read())
            .await
            .expect("blocking task panicked")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_blocking_receives_sum() {
        const WRITES: u64 = 100;
        let efd = Arc::new(GkEventFd::new(0, EfdFlags::NONE).unwrap());
        let producer = {
            let efd = Arc::clone(&efd);
            std::thread::spawn(move || {
                for i in 1..=WRITES {
                    efd.write(i).unwrap();
                }
            })
        };
        let expected = WRITES * (WRITES + 1) / 2;
        let mut sum = 0;
        while sum < expected {
            sum += read_blocking(Arc::clone(&efd)).await.unwrap();
        }
        producer.join().unwrap();
        assert_eq!(sum, expected);
    }
}

/// A `GkReceiver` driven by tokio's reactor (unix): await readability, then
/// receive until `WouldBlock` (which re-arms the doorbell).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_receiver_with_asyncfd() {
    use adaptif_gatotkaca_eventfd::{ChannelOptions, GkChannel};
    use std::io::ErrorKind;
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    let (mut tx, mut rx) = GkChannel::with_options(ChannelOptions {
        capacity: 4096,
        max_message: 64,
    })
    .unwrap();
    rx.set_nonblocking(true);
    // SAFETY: the receiver owns its pollable descriptor until dropped, and
    // `as_raw_fd` always returns that same descriptor.
    let mut rx = unsafe { AsyncFd::register_with_interest(rx, Interest::READABLE) }.unwrap();
    let writer = std::thread::spawn(move || {
        for i in 0..200u32 {
            tx.send(&i.to_le_bytes()).unwrap();
            if i % 10 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    });
    let mut next = 0u32;
    let mut buf = [0u8; 4];
    'outer: loop {
        let mut guard = rx.readable_mut().await.unwrap();
        loop {
            match guard.get_inner_mut().recv(&mut buf) {
                Ok(4) => {
                    assert_eq!(u32::from_le_bytes(buf), next);
                    next += 1;
                }
                Ok(n) => panic!("unexpected length {n}"),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    guard.clear_ready();
                    break;
                }
                Err(e) if e.kind() == ErrorKind::BrokenPipe => break 'outer,
                Err(e) => panic!("{e}"),
            }
        }
    }
    writer.join().unwrap();
    assert_eq!(next, 200);
}

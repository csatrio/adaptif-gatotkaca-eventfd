//! Waking an async task from a plain OS thread with tokio.
//!
//! * Unix: wrap the eventfd in `tokio::io::unix::AsyncFd` and await readiness.
//! * Windows: tokio cannot register a foreign socket, so do a blocking read
//!   on tokio's blocking pool with `spawn_blocking`.
//!
//! Run with: `cargo run --example tokio_async`

use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use std::time::Duration;

const JOBS: u64 = 5;

/// Simulates a non-async worker that reports each finished job through its
/// own handle (`try_clone` works like `dup(2)`).
fn spawn_worker(notifier: GkEventFd) {
    std::thread::spawn(move || {
        for _ in 0..JOBS {
            std::thread::sleep(Duration::from_millis(30));
            notifier.write(1).expect("eventfd write");
        }
    });
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    // The fd must be non-blocking for AsyncFd. Only read readiness matters.
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
    // SAFETY: `GkEventFd` owns its descriptor, which stays open until it is
    // dropped, and `as_raw_fd` always returns that same fd.
    let efd = unsafe { AsyncFd::register_with_interest(efd, Interest::READABLE) }?;
    spawn_worker(efd.get_ref().try_clone()?);

    let mut done = 0;
    while done < JOBS {
        let mut guard = efd.readable().await?;
        match guard.try_io(|inner| inner.get_ref().read()) {
            Ok(n) => {
                done += n?;
                println!("{done}/{JOBS} jobs done");
            }
            // Readiness was stale; try_io cleared it, so wait again.
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    use std::sync::Arc;

    // Blocking mode: the read below waits on a blocking-pool thread.
    let efd = Arc::new(GkEventFd::new(0, EfdFlags::NONE)?);
    spawn_worker(efd.try_clone()?);

    let mut done = 0;
    while done < JOBS {
        let efd = Arc::clone(&efd);
        let n = tokio::task::spawn_blocking(move || efd.read())
            .await
            .expect("blocking task panicked")?;
        done += n;
        println!("{done}/{JOBS} jobs done");
    }
    Ok(())
}

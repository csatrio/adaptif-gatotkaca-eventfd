//! Rough benchmark: `GkChannel` against a kernel pipe and a unix socket
//! (both with a 4-byte length prefix, so they carry messages too), between
//! two threads.
//!
//! ```sh
//! cargo run --release --example channel_bench
//! ```
//!
//! Numbers depend heavily on the machine and on core placement; treat them
//! as relative, not absolute.

use std::io::{Read, Write};
use std::thread;
use std::time::Instant;

use adaptif_gatotkaca_eventfd::{ChannelOptions, GkChannel};

fn channel_rate(size: usize, count: usize) -> f64 {
    let (mut tx, mut rx) = GkChannel::with_options(ChannelOptions {
        capacity: 16 << 20,
        max_message: 1 << 20,
    })
    .unwrap();
    let start = Instant::now();
    let writer = thread::spawn(move || {
        let msg = vec![7u8; size];
        for _ in 0..count {
            tx.send(&msg).unwrap();
        }
    });
    let mut buf = vec![0u8; size];
    for _ in 0..count {
        rx.recv(&mut buf).unwrap();
    }
    writer.join().unwrap();
    count as f64 / start.elapsed().as_secs_f64()
}

fn channel_ping_pong(rounds: usize) -> f64 {
    let (mut to_b, mut from_a) = GkChannel::with_options(ChannelOptions {
        capacity: 1 << 16,
        max_message: 64,
    })
    .unwrap();
    let (mut to_a, mut from_b) = GkChannel::with_options(ChannelOptions {
        capacity: 1 << 16,
        max_message: 64,
    })
    .unwrap();
    let b = thread::spawn(move || {
        let mut buf = [0u8; 8];
        for _ in 0..rounds {
            from_a.recv(&mut buf).unwrap();
            to_a.send(&buf).unwrap();
        }
    });
    let mut buf = [0u8; 8];
    let start = Instant::now();
    for i in 0..rounds as u64 {
        to_b.send(&i.to_le_bytes()).unwrap();
        from_b.recv(&mut buf).unwrap();
    }
    let rtt = start.elapsed().as_secs_f64() / rounds as f64;
    b.join().unwrap();
    rtt * 1e6
}

/// Length-prefixed messages over any byte stream pair.
fn stream_rate<S: Read + Write + Send + 'static>(
    (mut a, mut b): (S, S),
    size: usize,
    count: usize,
) -> f64 {
    let start = Instant::now();
    let writer = thread::spawn(move || {
        let msg = vec![7u8; size];
        for _ in 0..count {
            a.write_all(&(size as u32).to_le_bytes()).unwrap();
            a.write_all(&msg).unwrap();
        }
    });
    let mut buf = vec![0u8; size];
    let mut len = [0u8; 4];
    for _ in 0..count {
        b.read_exact(&mut len).unwrap();
        b.read_exact(&mut buf[..u32::from_le_bytes(len) as usize])
            .unwrap();
    }
    writer.join().unwrap();
    count as f64 / start.elapsed().as_secs_f64()
}

#[cfg(unix)]
fn pipe() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: fresh descriptors we own.
    unsafe {
        (
            std::fs::File::from_raw_fd(fds[1]),
            std::fs::File::from_raw_fd(fds[0]),
        )
    }
}

#[cfg(unix)]
fn socket_ping_pong(rounds: usize) -> f64 {
    let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    let t = thread::spawn(move || {
        let mut buf = [0u8; 8];
        for _ in 0..rounds {
            b.read_exact(&mut buf).unwrap();
            b.write_all(&buf).unwrap();
        }
    });
    let mut buf = [0u8; 8];
    let start = Instant::now();
    for i in 0..rounds as u64 {
        a.write_all(&i.to_le_bytes()).unwrap();
        a.read_exact(&mut buf).unwrap();
    }
    let rtt = start.elapsed().as_secs_f64() / rounds as f64;
    t.join().unwrap();
    rtt * 1e6
}

fn main() {
    println!("messages per second (two threads, higher is better)");
    println!(
        "{:>8}  {:>14}  {:>14}  {:>14}",
        "size", "GkChannel", "pipe", "unix socket"
    );
    for (size, count) in [(64, 2_000_000), (1024, 1_000_000), (65536, 50_000)] {
        let ch = channel_rate(size, count);
        #[cfg(unix)]
        {
            let p = stream_rate(pipe(), size, count);
            let s = stream_rate(std::os::unix::net::UnixStream::pair().unwrap(), size, count);
            println!("{size:>8}  {ch:>14.0}  {p:>14.0}  {s:>14.0}");
        }
        #[cfg(not(unix))]
        {
            let _ = stream_rate::<std::net::TcpStream>;
            println!("{size:>8}  {ch:>14.0}");
        }
    }
    println!();
    println!("ping-pong round trip, microseconds (lower is better)");
    println!("  GkChannel:   {:.2}", channel_ping_pong(100_000));
    #[cfg(unix)]
    println!("  unix socket: {:.2}", socket_ping_pong(100_000));
}

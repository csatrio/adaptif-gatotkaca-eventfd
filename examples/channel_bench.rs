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
        ..Default::default()
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
        ..Default::default()
    })
    .unwrap();
    let (mut to_a, mut from_b) = GkChannel::with_options(ChannelOptions {
        capacity: 1 << 16,
        max_message: 64,
        ..Default::default()
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

/// One bulk transfer of `total` bytes through the default bulk ring.
fn bulk_rate(total: usize) -> f64 {
    let (mut tx, mut rx) = GkChannel::new().unwrap();
    let start = Instant::now();
    let writer = thread::spawn(move || {
        let piece = vec![7u8; 1 << 20];
        let mut w = tx.bulk_writer(Some(total as u64)).unwrap();
        for _ in 0..total / piece.len() {
            w.write_all(&piece).unwrap();
        }
        w.finish().unwrap();
    });
    // Copy the data out like the pipe/socket readers do (one copy per side).
    let mut r = rx.bulk_reader().unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0;
    loop {
        match r.read(&mut buf).unwrap() {
            0 => break,
            n => got += n,
        }
    }
    assert_eq!(got, total);
    drop(r);
    writer.join().unwrap();
    total as f64 / start.elapsed().as_secs_f64() / (1u64 << 30) as f64
}

/// The same amount through a byte stream with large writes.
#[cfg(unix)]
fn stream_bulk_rate<S: Read + Write + Send + 'static>((mut a, mut b): (S, S), total: usize) -> f64 {
    let start = Instant::now();
    let writer = thread::spawn(move || {
        let piece = vec![7u8; 1 << 20];
        for _ in 0..total / piece.len() {
            a.write_all(&piece).unwrap();
        }
    });
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0;
    while got < total {
        got += b.read(&mut buf).unwrap();
    }
    writer.join().unwrap();
    total as f64 / start.elapsed().as_secs_f64() / (1u64 << 30) as f64
}

/// Latency of messages sent while a large bulk transfer is in progress:
/// the sender interleaves messages with bulk chunks; the receiver reads
/// only messages, timestamping them on arrival.
fn message_latency_during_bulk() -> (f64, f64) {
    let (mut tx, mut rx) = GkChannel::new().unwrap();
    let origin = Instant::now();
    let total: u64 = 512 << 20;
    let writer = thread::spawn(move || {
        let piece = vec![7u8; 64 << 10];
        let mut sent = 0u64;
        let mut i = 0u64;
        while sent < total {
            let mut w = if sent == 0 {
                tx.bulk_writer(Some(total)).unwrap()
            } else {
                tx.resume_bulk().unwrap()
            };
            w.write_all(&piece).unwrap();
            sent += piece.len() as u64;
            if sent < total {
                w.pause();
            } else {
                w.finish().unwrap();
                drop(w);
            }
            if i % 16 == 0 {
                let t = origin.elapsed().as_nanos() as u64;
                tx.send(&t.to_le_bytes()).unwrap();
            }
            i += 1;
        }
    });
    // Messages first (as a latency-sensitive consumer would), then one bulk
    // chunk at a time so the transfer keeps moving.
    let reader = thread::spawn(move || {
        let mut lat = Vec::new();
        let mut buf = [0u8; 8];
        let mut bulk_done = false;
        rx.set_nonblocking(true);
        loop {
            loop {
                match rx.recv(&mut buf) {
                    Ok(_) => {
                        let sent = u64::from_le_bytes(buf);
                        lat.push(origin.elapsed().as_nanos() as u64 - sent);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => return lat,
                }
            }
            if !bulk_done {
                let mut r = match rx.bulk_reader() {
                    Ok(r) => r,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return lat,
                };
                // One chunk, then check messages again.
                let mut chunk = [0u8; 64 << 10];
                match r.read(&mut chunk) {
                    Ok(0) => bulk_done = true,
                    Ok(_) => r.pause(),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => r.pause(),
                    Err(_) => return lat,
                }
            }
        }
    });
    writer.join().unwrap();
    let mut lat = reader.join().unwrap();
    lat.sort_unstable();
    let p50 = lat[lat.len() / 2] as f64 / 1e3;
    let p99 = lat[lat.len() * 99 / 100] as f64 / 1e3;
    (p50, p99)
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
    println!("bulk transfer of 1 GiB, GiB/s (higher is better)");
    let gb = 1 << 30;
    #[cfg(unix)]
    println!(
        "  GkChannel: {:.2}   pipe: {:.2}   unix socket: {:.2}",
        bulk_rate(gb),
        stream_bulk_rate(pipe(), gb),
        stream_bulk_rate(std::os::unix::net::UnixStream::pair().unwrap(), gb)
    );
    #[cfg(not(unix))]
    println!("  GkChannel: {:.2}", bulk_rate(gb));
    let (p50, p99) = message_latency_during_bulk();
    println!();
    println!("message latency while a 512 MiB transfer is in progress, microseconds");
    println!("  median: {p50:.1}   99th percentile: {p99:.1}");
    println!();
    println!("ping-pong round trip, microseconds (lower is better)");
    println!("  GkChannel:   {:.2}", channel_ping_pong(100_000));
    #[cfg(unix)]
    println!("  unix socket: {:.2}", socket_ping_pong(100_000));
}

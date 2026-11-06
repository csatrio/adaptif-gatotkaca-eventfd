//! Linux `eventfd(2)` semantics, checked through the public API.
//!
//! This suite runs unchanged on every backend: the kernel eventfd (Linux,
//! Android), the pipe shim (macOS, BSDs, Linux with `force-shim`) and the
//! socket shim (Windows).

use std::io::{self, ErrorKind, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use adaptif_gatotkaca_eventfd::{
    EFD_CLOEXEC, EFD_MAX, EFD_NONBLOCK, EFD_SEMAPHORE, EfdFlags, GkEventFd,
};

fn nb() -> EfdFlags {
    EfdFlags::NONBLOCK
}

fn sem_nb() -> EfdFlags {
    EfdFlags::SEMAPHORE | EfdFlags::NONBLOCK
}

fn kind<T: std::fmt::Debug>(r: io::Result<T>) -> ErrorKind {
    r.expect_err("expected an error").kind()
}

/// Time a "should still be blocked" check waits before acting.
const SETTLE: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Readiness helpers: is the pollable object readable right now?
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn is_readable(efd: &GkEventFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: efd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert!(n >= 0, "poll: {}", io::Error::last_os_error());
    assert_eq!(
        pfd.revents & (libc::POLLERR | libc::POLLNVAL),
        0,
        "unexpected revents {:#x}",
        pfd.revents
    );
    pfd.revents & libc::POLLIN != 0
}

#[cfg(windows)]
fn is_readable(efd: &GkEventFd) -> bool {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};
    let mut pfd = WSAPOLLFD {
        fd: efd.as_raw_socket() as _,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: one valid WSAPOLLFD, zero timeout.
    let n = unsafe { WSAPoll(&mut pfd, 1, 0) };
    assert!(n >= 0, "WSAPoll: {}", io::Error::last_os_error());
    pfd.revents & POLLRDNORM != 0
}

// ---------------------------------------------------------------------------
// Basic semantics
// ---------------------------------------------------------------------------

#[test]
fn initval_is_read_back() {
    let efd = GkEventFd::new(7, nb()).unwrap();
    assert_eq!(efd.read().unwrap(), 7);
    let efd = GkEventFd::new(u32::MAX, nb()).unwrap();
    assert_eq!(efd.read().unwrap(), u64::from(u32::MAX));
}

#[test]
fn read_returns_sum_and_resets() {
    let efd = GkEventFd::new(1, nb()).unwrap();
    efd.write(2).unwrap();
    efd.write(3).unwrap();
    assert_eq!(efd.read().unwrap(), 6);
    assert_eq!(kind(efd.read()), ErrorKind::WouldBlock);
    efd.write(10).unwrap();
    assert_eq!(efd.read().unwrap(), 10);
}

#[test]
fn semaphore_mode_decrements_by_one() {
    let efd = GkEventFd::new(2, sem_nb()).unwrap();
    efd.write(1).unwrap();
    for _ in 0..3 {
        assert_eq!(efd.read().unwrap(), 1);
    }
    assert_eq!(kind(efd.read()), ErrorKind::WouldBlock);
}

#[test]
fn nonblocking_empty_read_is_wouldblock() {
    let efd = GkEventFd::new(0, nb()).unwrap();
    assert!(efd.is_nonblocking().unwrap());
    let err = efd.read().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::WouldBlock);
    #[cfg(unix)]
    assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
}

#[test]
fn write_u64_max_is_invalid_input() {
    for flags in [nb(), EfdFlags::NONE] {
        let efd = GkEventFd::new(0, flags).unwrap();
        let err = efd.write(u64::MAX).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        #[cfg(unix)]
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        // Nothing was added.
        efd.set_nonblocking(true).unwrap();
        assert_eq!(kind(efd.read()), ErrorKind::WouldBlock);
    }
}

#[test]
fn overflow_nonblocking_is_wouldblock() {
    let efd = GkEventFd::new(0, nb()).unwrap();
    efd.write(EFD_MAX).unwrap();
    assert_eq!(kind(efd.write(1)), ErrorKind::WouldBlock);
    // write(0) never overflows.
    efd.write(0).unwrap();
    assert_eq!(efd.read().unwrap(), EFD_MAX);

    let efd = GkEventFd::new(5, nb()).unwrap();
    efd.write(EFD_MAX - 5).unwrap();
    assert_eq!(kind(efd.write(1)), ErrorKind::WouldBlock);
    assert_eq!(efd.read().unwrap(), EFD_MAX);

    let efd = GkEventFd::new(1, nb()).unwrap();
    assert_eq!(kind(efd.write(EFD_MAX)), ErrorKind::WouldBlock);
    assert_eq!(efd.read().unwrap(), 1);
}

#[test]
fn blocking_write_is_unblocked_by_read() {
    let efd = Arc::new(GkEventFd::new(0, EfdFlags::NONE).unwrap());
    efd.write(EFD_MAX).unwrap();
    let done = Arc::new(AtomicBool::new(false));

    let writer = {
        let efd = Arc::clone(&efd);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            efd.write(3).unwrap();
            done.store(true, Ordering::SeqCst);
        })
    };
    thread::sleep(SETTLE);
    assert!(
        !done.load(Ordering::SeqCst),
        "write should block while full"
    );

    assert_eq!(efd.read().unwrap(), EFD_MAX);
    writer.join().unwrap();
    assert!(done.load(Ordering::SeqCst));
    assert_eq!(efd.read().unwrap(), 3);
}

#[test]
fn blocking_read_is_woken_by_write() {
    let efd = Arc::new(GkEventFd::new(0, EfdFlags::NONE).unwrap());
    assert!(!efd.is_nonblocking().unwrap());
    let reader = {
        let efd = Arc::clone(&efd);
        thread::spawn(move || efd.read().unwrap())
    };
    thread::sleep(SETTLE);
    assert!(!reader.is_finished(), "read should block while empty");
    efd.write(9).unwrap();
    assert_eq!(reader.join().unwrap(), 9);
}

#[test]
fn blocking_semaphore_read_is_woken_by_write() {
    let efd = Arc::new(GkEventFd::new(0, EfdFlags::SEMAPHORE).unwrap());
    let reader = {
        let efd = Arc::clone(&efd);
        thread::spawn(move || efd.read().unwrap())
    };
    thread::sleep(SETTLE);
    efd.write(4).unwrap();
    assert_eq!(reader.join().unwrap(), 1);
    efd.set_nonblocking(true).unwrap();
    for _ in 0..3 {
        assert_eq!(efd.read().unwrap(), 1);
    }
    assert_eq!(kind(efd.read()), ErrorKind::WouldBlock);
}

#[test]
fn write_zero_is_noop() {
    let efd = GkEventFd::new(0, nb()).unwrap();
    efd.write(0).unwrap();
    assert!(!is_readable(&efd));
    assert_eq!(kind(efd.read()), ErrorKind::WouldBlock);

    efd.write(2).unwrap();
    efd.write(0).unwrap();
    assert!(is_readable(&efd));
    assert_eq!(efd.read().unwrap(), 2);
}

#[test]
fn set_nonblocking_round_trip() {
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    assert!(!efd.is_nonblocking().unwrap());
    efd.set_nonblocking(true).unwrap();
    assert!(efd.is_nonblocking().unwrap());
    assert_eq!(kind(efd.read()), ErrorKind::WouldBlock);
    efd.set_nonblocking(false).unwrap();
    assert!(!efd.is_nonblocking().unwrap());
}

// ---------------------------------------------------------------------------
// io::Read / io::Write
// ---------------------------------------------------------------------------

#[test]
fn io_read_write_use_8_byte_native_endian_buffers() {
    let mut efd = GkEventFd::new(0, nb()).unwrap();
    assert_eq!(Write::write(&mut efd, &5u64.to_ne_bytes()).unwrap(), 8);
    // Longer buffers: only the first 8 bytes count.
    let mut long = [0xAAu8; 12];
    long[..8].copy_from_slice(&6u64.to_ne_bytes());
    assert_eq!(Write::write(&mut &efd, &long).unwrap(), 8);
    efd.flush().unwrap();

    let mut buf = [0u8; 16];
    assert_eq!(Read::read(&mut efd, &mut buf).unwrap(), 8);
    assert_eq!(u64::from_ne_bytes(buf[..8].try_into().unwrap()), 11);

    let mut buf = [0u8; 8];
    assert_eq!(kind(Read::read(&mut &efd, &mut buf)), ErrorKind::WouldBlock);

    // u64::MAX through io::Write is EINVAL too.
    assert_eq!(
        kind(Write::write(&mut &efd, &u64::MAX.to_ne_bytes())),
        ErrorKind::InvalidInput
    );
}

#[test]
fn io_short_buffers_are_invalid_input() {
    let efd = GkEventFd::new(3, nb()).unwrap();
    for len in 0..8 {
        let mut buf = vec![0u8; len];
        assert_eq!(
            kind(Read::read(&mut &efd, &mut buf)),
            ErrorKind::InvalidInput,
            "read len {len}"
        );
        assert_eq!(
            kind(Write::write(&mut &efd, &buf)),
            ErrorKind::InvalidInput,
            "write len {len}"
        );
    }
    // The short read did not consume the counter.
    assert_eq!(efd.read().unwrap(), 3);
}

// ---------------------------------------------------------------------------
// Handles and flags
// ---------------------------------------------------------------------------

#[test]
fn try_clone_shares_counter_and_nonblocking_flag() {
    let a = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let b = a.try_clone().unwrap();
    a.write(4).unwrap();
    assert!(is_readable(&a) && is_readable(&b));
    assert_eq!(b.read().unwrap(), 4);
    assert!(!is_readable(&a) && !is_readable(&b));

    assert!(!b.is_nonblocking().unwrap());
    a.set_nonblocking(true).unwrap();
    assert!(b.is_nonblocking().unwrap());
    assert_eq!(kind(b.read()), ErrorKind::WouldBlock);
    b.set_nonblocking(false).unwrap();
    assert!(!a.is_nonblocking().unwrap());

    // The clone outlives the original.
    let c = b.try_clone().unwrap();
    drop(a);
    drop(b);
    c.write(1).unwrap();
    assert_eq!(c.read().unwrap(), 1);
}

#[test]
fn try_clone_keeps_semaphore_mode() {
    let a = GkEventFd::new(3, sem_nb()).unwrap();
    let b = a.try_clone().unwrap();
    assert_eq!(b.read().unwrap(), 1);
    assert_eq!(a.read().unwrap(), 1);
    assert_eq!(b.read().unwrap(), 1);
    assert_eq!(kind(a.read()), ErrorKind::WouldBlock);
}

#[test]
fn with_raw_flags() {
    let efd = GkEventFd::with_raw_flags(1, EFD_NONBLOCK | EFD_CLOEXEC).unwrap();
    assert!(efd.is_nonblocking().unwrap());
    assert_eq!(efd.read().unwrap(), 1);

    let efd = GkEventFd::with_raw_flags(2, EFD_NONBLOCK | EFD_SEMAPHORE).unwrap();
    assert_eq!(efd.read().unwrap(), 1);

    let known = EFD_NONBLOCK | EFD_CLOEXEC | EFD_SEMAPHORE;
    let bad_bits = (0..31).map(|b| 1 << b).filter(|b| b & known == 0);
    for bad in bad_bits.chain([-1, known | 0x4000_0000]) {
        let err = GkEventFd::with_raw_flags(0, bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "flags {bad:#x}");
    }
    assert!(EfdFlags::from_bits(EFD_NONBLOCK | 0x4000_0000).is_none());
}

#[test]
fn is_send_sync_and_debug() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<GkEventFd>();
    let efd = GkEventFd::new(0, nb()).unwrap();
    assert!(format!("{efd:?}").starts_with("GkEventFd"));
}

// ---------------------------------------------------------------------------
// Readiness == (counter > 0)
// ---------------------------------------------------------------------------

#[test]
fn readiness_tracks_counter_normal_mode() {
    let efd = GkEventFd::new(0, nb()).unwrap();
    assert!(!is_readable(&efd));
    efd.write(1).unwrap();
    assert!(is_readable(&efd));
    efd.write(5).unwrap();
    assert!(is_readable(&efd));
    // Polling must not consume readiness.
    assert!(is_readable(&efd));
    assert_eq!(efd.read().unwrap(), 6);
    assert!(!is_readable(&efd));
    assert!(efd.read().is_err());
    assert!(!is_readable(&efd));

    efd.write(EFD_MAX).unwrap();
    assert!(is_readable(&efd));
    assert!(efd.write(1).is_err());
    assert!(is_readable(&efd));
    assert!(efd.write(u64::MAX).is_err());
    assert!(is_readable(&efd));
    assert_eq!(efd.read().unwrap(), EFD_MAX);
    assert!(!is_readable(&efd));

    let efd = GkEventFd::new(3, nb()).unwrap();
    assert!(is_readable(&efd), "initval > 0 must start readable");
}

#[test]
fn readiness_tracks_counter_semaphore_mode() {
    let efd = GkEventFd::new(2, sem_nb()).unwrap();
    assert!(is_readable(&efd));
    efd.read().unwrap();
    assert!(is_readable(&efd));
    efd.read().unwrap();
    assert!(!is_readable(&efd));
    efd.write(1).unwrap();
    assert!(is_readable(&efd));
    efd.read().unwrap();
    assert!(!is_readable(&efd));
}

#[test]
fn readiness_random_sequence() {
    // Deterministic xorshift so the sequence is reproducible everywhere.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for semaphore in [false, true] {
        let flags = if semaphore { sem_nb() } else { nb() };
        let efd = GkEventFd::new(0, flags).unwrap();
        let mut model: u64 = 0;
        for _ in 0..2_000 {
            let r = next();
            if r % 3 == 0 {
                let v = match r % 7 {
                    0 => 0,
                    1 => EFD_MAX,
                    _ => (r >> 8) % 4,
                };
                match efd.write(v) {
                    Ok(()) => model += v,
                    Err(e) => {
                        assert_eq!(e.kind(), ErrorKind::WouldBlock);
                        assert!(EFD_MAX - model < v);
                    }
                }
            } else {
                match efd.read() {
                    Ok(n) => {
                        let expect = if semaphore { 1 } else { model };
                        assert_eq!(n, expect);
                        model -= expect;
                    }
                    Err(e) => {
                        assert_eq!(e.kind(), ErrorKind::WouldBlock);
                        assert_eq!(model, 0);
                    }
                }
            }
            assert_eq!(is_readable(&efd), model > 0, "model = {model}");
        }
    }
}

// ---------------------------------------------------------------------------
// MPMC stress: every unit written is read exactly once.
// ---------------------------------------------------------------------------

fn mpmc(semaphore: bool) {
    const PRODUCERS: u64 = 4;
    const CONSUMERS: usize = 4;
    const WRITES: u64 = if cfg!(debug_assertions) {
        2_000
    } else {
        20_000
    };

    let flags = if semaphore {
        EfdFlags::SEMAPHORE
    } else {
        EfdFlags::NONE
    };
    let efd = Arc::new(GkEventFd::new(0, flags).unwrap());
    let consumed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let efd = Arc::clone(&efd);
            thread::spawn(move || {
                let mut sum = 0;
                for i in 0..WRITES {
                    let v = 1 + (p + i) % 3;
                    efd.write(v).unwrap();
                    sum += v;
                }
                sum
            })
        })
        .collect();

    // Consumers use blocking reads. Once everything has been consumed the
    // main thread sets `stop` and keeps writing wake-up units until they exit.
    let consumers: Vec<_> = (0..CONSUMERS)
        .map(|_| {
            let efd = Arc::clone(&efd);
            let consumed = Arc::clone(&consumed);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                loop {
                    let n = efd.read().unwrap();
                    if semaphore {
                        assert_eq!(n, 1);
                    }
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    consumed.fetch_add(n, Ordering::SeqCst);
                }
            })
        })
        .collect();

    let produced: u64 = producers.into_iter().map(|h| h.join().unwrap()).sum();
    while consumed.load(Ordering::SeqCst) < produced {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(consumed.load(Ordering::SeqCst), produced);
    // Everything has been consumed, so the counter is 0 here.
    assert!(!is_readable(&efd));

    stop.store(true, Ordering::SeqCst);
    while !consumers.iter().all(|h| h.is_finished()) {
        efd.write(1).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    for h in consumers {
        h.join().unwrap();
    }
    assert_eq!(consumed.load(Ordering::SeqCst), produced);
    // Drain leftover wake-up units; readiness must follow.
    efd.set_nonblocking(true).unwrap();
    while efd.read().is_ok() {}
    assert!(!is_readable(&efd));
}

#[test]
fn mpmc_stress_normal_mode() {
    mpmc(false);
}

#[test]
fn mpmc_stress_semaphore_mode() {
    mpmc(true);
}

#[test]
fn concurrent_writers_conserve_total_nonblocking() {
    let efd = Arc::new(GkEventFd::new(0, nb()).unwrap());
    let total = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let efd = Arc::clone(&efd);
            let total = Arc::clone(&total);
            thread::spawn(move || {
                for _ in 0..1_000 {
                    efd.write(1).unwrap();
                    if let Ok(n) = efd.read() {
                        total.fetch_add(n, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    if let Ok(n) = efd.read() {
        total.fetch_add(n, Ordering::SeqCst);
    }
    assert_eq!(total.load(Ordering::SeqCst), 8_000);
    assert!(!is_readable(&efd));
}

// ---------------------------------------------------------------------------
// Drop releases the descriptors.
// ---------------------------------------------------------------------------

/// Number of open descriptors below a cap. Other tests in this binary run in
/// parallel, so callers compare with some slack.
#[cfg(unix)]
fn open_fd_count() -> usize {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into a live local.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    let cap = lim.rlim_cur.min(1 << 16) as libc::c_int;
    // SAFETY: F_GETFD only queries the descriptor table.
    (0..cap)
        .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1)
        .count()
}

#[cfg(unix)]
#[test]
fn drop_releases_descriptors() {
    let before = open_fd_count();
    for i in 0..10_000u32 {
        let efd = GkEventFd::new(i, nb()).unwrap_or_else(|e| panic!("iteration {i}: {e}"));
        let clone = efd
            .try_clone()
            .unwrap_or_else(|e| panic!("iteration {i}: {e}"));
        clone.write(1).unwrap();
        drop(efd);
        drop(clone);
    }
    let after = open_fd_count();
    // A leak would be thousands of descriptors; allow for parallel tests.
    assert!(after <= before + 64, "fds before {before}, after {after}");
}

#[cfg(windows)]
#[test]
fn drop_releases_sockets() {
    // Fewer iterations: each shim instance is a loopback TCP pair, and closed
    // TCP connections linger in TIME_WAIT.
    for i in 0..1_000u32 {
        let efd = GkEventFd::new(i, nb()).unwrap_or_else(|e| panic!("iteration {i}: {e}"));
        let clone = efd
            .try_clone()
            .unwrap_or_else(|e| panic!("iteration {i}: {e}"));
        clone.write(1).unwrap();
        drop(efd);
        drop(clone);
    }
}

//! `GkShmPool`: zero-copy buffers lent through shared memory.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use adaptif_gatotkaca_eventfd::{GkShmPool, ShmLease, ShmPoolOptions};

fn pool(size: usize, granule: usize) -> GkShmPool {
    GkShmPool::with_options(ShmPoolOptions { size, granule, ..Default::default() }).unwrap()
}

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i ^ seed).wrapping_mul(2654435761).rotate_left(5) as u8).collect()
}

#[test]
fn lend_take_and_give_back() {
    let p = pool(1 << 20, 4096);
    let view = p.view();
    assert_eq!((view.size(), view.granule()), (1 << 20, 4096));
    let mut block = p.alloc(10_000).unwrap();
    assert_eq!(block.capacity(), 16384, "4 granules (10 000 bytes rounded up to a power of two)");
    let data = pattern(10_000, 1);
    block.as_mut_slice()[..10_000].copy_from_slice(&data);
    let lease = block.lend(10_000);
    assert_eq!(ShmLease::from_bytes(&lease.to_bytes()), lease);
    let s = p.stats();
    assert_eq!((s.lent, s.allocated, s.free), (1, 0, (1 << 20) - 16384));
    // SAFETY: lent by this pool, taken once.
    let buf = unsafe { view.take(lease) }.unwrap();
    assert_eq!(&buf[..], &data[..]);
    assert_eq!(buf.lease(), lease);
    drop(buf);
    let s = p.stats();
    assert_eq!((s.lent, s.free, s.bad_returns), (0, 1 << 20, 0));
    // An unlent block is freed by dropping it.
    drop(p.alloc(1).unwrap());
    assert_eq!(p.stats().free, 1 << 20);
}

#[test]
fn exhaustion_and_recovery_through_returns() {
    let p = pool(64 * 1024, 4096);
    let view = p.view();
    let mut held = Vec::new();
    while let Ok(b) = p.alloc(4096) {
        held.push(b.lend(4096));
    }
    assert_eq!(held.len(), 16);
    assert_eq!(p.alloc(1).unwrap_err().kind(), ErrorKind::OutOfMemory);
    assert_eq!(p.alloc(1 << 20).unwrap_err().kind(), ErrorKind::InvalidInput);
    // The borrower gives everything back; the next alloc collects it.
    // SAFETY: each lease taken once.
    let bufs: Vec<_> = held.iter().map(|&l| unsafe { view.take(l) }.unwrap()).collect();
    drop(bufs);
    let big = p.alloc(64 * 1024).unwrap();
    assert_eq!(big.capacity(), 64 * 1024, "returned blocks merged back into one");
}

#[test]
fn non_power_of_two_pools_and_granules() {
    let p = pool(100 * 64, 64);
    assert_eq!(p.size(), 6400);
    // The largest block is 64 granules; 100 granules never fit in one.
    assert_eq!(p.alloc(6400).unwrap_err().kind(), ErrorKind::InvalidInput);
    let a = p.alloc(64 * 64).unwrap();
    let b = p.alloc(32 * 64).unwrap();
    let c = p.alloc(4 * 64).unwrap();
    assert_eq!(a.capacity() + b.capacity() + c.capacity(), 100 * 64);
    assert_eq!(p.alloc(1).unwrap_err().kind(), ErrorKind::OutOfMemory);
}

/// The allocator keeps allocating and lending while other threads read and
/// give blocks back: no block is ever handed out twice at once.
#[test]
fn concurrent_returns_while_allocating() {
    let p = pool(4 << 20, 256);
    let view = p.view();
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (view, done) = (view.clone(), Arc::clone(&done));
            let (ltx, lrx) = std::sync::mpsc::channel::<(ShmLease, u32)>();
            let h = thread::spawn(move || {
                for (lease, seed) in lrx {
                    // SAFETY: each lease is taken once, by this thread.
                    let buf = unsafe { view.take(lease) }.unwrap();
                    assert_eq!(&buf[..], &pattern(lease.len as usize, seed)[..], "block reused while lent");
                }
                assert!(done.load(Ordering::Relaxed));
            });
            (ltx, h)
        })
        .collect();
    let n = if cfg!(debug_assertions) { 20_000 } else { 200_000 };
    for i in 0..n as u32 {
        let len = 1 + (i as usize * 7919) % 20_000;
        let mut block = loop {
            match p.alloc(len) {
                Ok(b) => break b,
                Err(e) if e.kind() == ErrorKind::OutOfMemory => thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        };
        block.as_mut_slice()[..len].copy_from_slice(&pattern(len, i));
        let lease = block.lend(len);
        readers[i as usize % 4].0.send((lease, i)).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    for (ltx, h) in readers {
        drop(ltx);
        h.join().unwrap();
    }
    let s = p.stats();
    assert_eq!((s.lent, s.allocated, s.bad_returns, s.free), (0, 0, 0, 4 << 20));
}

/// A stale return (a lease taken twice, the second after the block was lent
/// again) is refused: it never frees the block someone else holds.
#[test]
fn stale_and_duplicate_returns_are_refused() {
    let p = pool(1 << 16, 4096);
    let view = p.view();
    let lease = p.alloc(100).unwrap().lend(100);
    // SAFETY: deliberately breaking "take once" to test the defence; the
    // memory stays mapped and in bounds either way.
    let (first, second) = unsafe { (view.take(lease).unwrap(), view.take(lease).unwrap()) };
    drop(first);
    assert_eq!(p.stats().lent, 0);
    // The same block is lent again (new generation)...
    let again = p.alloc(100).unwrap();
    assert_eq!(again.offset(), (lease.index as usize) * 4096);
    let lease2 = again.lend(100);
    assert_eq!(lease2.generation, lease.generation.wrapping_add(1));
    // ...so the old buffer's return is refused.
    drop(second);
    let s = p.stats();
    assert_eq!((s.lent, s.bad_returns), (1, 1));
    // SAFETY: lease2 taken once.
    drop(unsafe { view.take(lease2) }.unwrap());
    assert_eq!(p.stats().lent, 0);
}

#[test]
fn leases_outside_the_pool_are_rejected() {
    let p = pool(1 << 16, 4096);
    let view = p.view();
    let bad = |l: ShmLease| {
        // SAFETY: rejected before anything is taken.
        unsafe { view.take(l) }.unwrap_err().kind()
    };
    assert_eq!(bad(ShmLease { index: 16, generation: 1, len: 1 }), ErrorKind::InvalidData);
    assert_eq!(bad(ShmLease { index: 15, generation: 1, len: 4097 }), ErrorKind::InvalidData);
    assert_eq!(bad(ShmLease { index: 0, generation: 1, len: u64::MAX }), ErrorKind::InvalidData);
}

#[test]
fn reclaim_all_takes_back_what_a_dead_borrower_held() {
    let p = pool(1 << 16, 4096);
    for _ in 0..5 {
        let _ = p.alloc(4096).unwrap().lend(4096);
    }
    let kept = p.alloc(4096).unwrap();
    assert_eq!(p.stats().lent, 5);
    // SAFETY: nobody took those leases.
    unsafe { p.reclaim_all() };
    let s = p.stats();
    assert_eq!((s.lent, s.allocated, s.free), (0, 1, (1 << 16) - 4096));
    drop(kept);
}

#[cfg(unix)]
#[test]
fn views_travel_over_unix_sockets() {
    use adaptif_gatotkaca_eventfd::{GkChannel, GkShmPoolView};
    use std::os::unix::net::UnixStream;
    let (a, b) = UnixStream::pair().unwrap();
    let p = pool(1 << 20, 4096);
    p.view().send_to(&a).unwrap();
    let remote = GkShmPoolView::recv_from(&b).unwrap();
    let mut block = p.alloc(5).unwrap();
    block.as_mut_slice()[..5].copy_from_slice(b"hello");
    let lease = block.lend(5);
    // SAFETY: taken once.
    let buf = unsafe { remote.take(lease) }.unwrap();
    assert_eq!(&buf[..], b"hello");
    drop(buf);
    assert_eq!(p.stats().lent, 0);
    // Something else sent instead of a view: refused.
    let (tx, _rx) = GkChannel::new().unwrap();
    tx.send_to(&a).unwrap();
    assert_eq!(GkShmPoolView::recv_from(&b).unwrap_err().kind(), ErrorKind::InvalidData);
}

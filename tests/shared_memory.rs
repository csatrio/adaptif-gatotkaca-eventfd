//! `GkSharedMemory`: creation, sizes, page options, and sharing between
//! threads, across `fork` (unix), over a unix socket and through
//! `GkProcess` in one process. Sharing with a spawned child is in
//! `process_share.rs`.
//!
//! Huge pages: `HugePages::Require` may fail where the system has none (or
//! the user lacks the right). Set `GK_TEST_HUGEPAGES=1` on a machine with
//! huge pages available to make it a hard requirement.

use std::io::ErrorKind;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use adaptif_gatotkaca_eventfd::{
    EfdFlags, GkEventFd, GkListener, GkProcess, GkSharedMemory, HugePages, SharedMemoryOptions,
};

fn pattern(i: usize) -> u8 {
    (i.wrapping_mul(2654435761) >> 11) as u8
}

fn fill(shm: &GkSharedMemory, from: usize, to: usize, x: u8) {
    // SAFETY: the test owns this range while it writes it.
    let mem = unsafe { shm.as_mut_slice() };
    for (i, b) in mem.iter_mut().enumerate().take(to).skip(from) {
        *b = pattern(i) ^ x;
    }
}

fn check(shm: &GkSharedMemory, from: usize, to: usize, x: u8) -> bool {
    // SAFETY: nobody writes this range while it is checked.
    let mem = unsafe { shm.as_slice() };
    (from..to).all(|i| mem[i] == pattern(i) ^ x)
}

#[test]
fn new_is_zeroed_and_writable() {
    let shm = GkSharedMemory::new(1 << 20).unwrap();
    assert_eq!(shm.len(), 1 << 20);
    assert!(!shm.is_empty());
    assert!(!shm.is_huge());
    assert_eq!(shm.as_ptr() as usize % shm.page_size(), 0);
    // SAFETY: only this thread uses the memory.
    assert!(unsafe { shm.as_slice() }.iter().all(|&b| b == 0));
    fill(&shm, 0, shm.len(), 0x3c);
    assert!(check(&shm, 0, shm.len(), 0x3c));
    let dbg = format!("{shm:?}");
    assert!(
        dbg.contains("GkSharedMemory") && dbg.contains("len"),
        "{dbg}"
    );
}

#[test]
fn lengths_round_up_to_the_page_size() {
    let one = GkSharedMemory::new(1).unwrap();
    let page = one.page_size();
    assert!(page.is_power_of_two() && page >= 4096);
    assert_eq!(one.len(), page);
    assert_eq!(GkSharedMemory::new(page).unwrap().len(), page);
    assert_eq!(GkSharedMemory::new(page + 1).unwrap().len(), 2 * page);
    assert_eq!(
        GkSharedMemory::new(0).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert!(GkSharedMemory::new(usize::MAX).is_err());
}

#[test]
fn options() {
    assert_eq!(
        SharedMemoryOptions::default(),
        SharedMemoryOptions {
            huge_pages: HugePages::Off,
            huge_page_size: None,
            populate: false,
        }
    );
    assert_eq!(HugePages::default(), HugePages::Off);
    let populated = GkSharedMemory::with_options(
        3 << 20,
        SharedMemoryOptions {
            populate: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(populated.len(), 3 << 20);
    // SAFETY: only this thread uses the memory.
    assert!(unsafe { populated.as_slice() }.iter().all(|&b| b == 0));
    fill(&populated, 0, populated.len(), 1);
    assert!(check(&populated, 0, populated.len(), 1));
}

/// `Prefer` never fails because of huge pages.
#[test]
fn prefer_works_everywhere() {
    for populate in [false, true] {
        let shm = GkSharedMemory::with_options(
            4 << 20,
            SharedMemoryOptions {
                huge_pages: HugePages::Prefer,
                populate,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(shm.len() >= 4 << 20);
        assert_eq!(shm.len() % shm.page_size(), 0);
        fill(&shm, 0, shm.len(), 7);
        assert!(check(&shm, 0, shm.len(), 7));
    }
}

#[test]
fn require_gives_huge_pages_or_a_clear_error() {
    let strict = std::env::var_os("GK_TEST_HUGEPAGES").is_some_and(|v| v == "1");
    let r = GkSharedMemory::with_options(
        4 << 20,
        SharedMemoryOptions {
            huge_pages: HugePages::Require,
            ..Default::default()
        },
    );
    match r {
        Ok(shm) => {
            assert!(shm.is_huge());
            assert!(shm.page_size() > 64 << 10, "page {}", shm.page_size());
            assert_eq!(shm.len() % shm.page_size(), 0);
            assert_eq!(shm.as_ptr() as usize % shm.page_size(), 0);
            fill(&shm, 0, shm.len(), 9);
            assert!(check(&shm, 0, shm.len(), 9));
        }
        Err(e) => {
            assert!(!strict, "GK_TEST_HUGEPAGES=1 but Require failed: {e}");
            assert!(
                matches!(
                    e.kind(),
                    ErrorKind::Unsupported | ErrorKind::PermissionDenied | ErrorKind::OutOfMemory
                ),
                "unexpected error: {e:?}"
            );
        }
    }
    // macOS/iOS and the unix systems without huge page support always say
    // Unsupported.
    #[cfg(any(
        target_vendor = "apple",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "illumos"
    ))]
    assert_eq!(
        GkSharedMemory::with_options(
            1,
            SharedMemoryOptions {
                huge_pages: HugePages::Require,
                ..Default::default()
            }
        )
        .unwrap_err()
        .kind(),
        ErrorKind::Unsupported
    );
}

/// One thread writes, the other checks after an eventfd doorbell, and back.
#[test]
fn two_threads() {
    let shm = Arc::new(GkSharedMemory::new(1 << 20).unwrap());
    let ping = Arc::new(GkEventFd::new(0, EfdFlags::NONE).unwrap());
    let pong = Arc::new(GkEventFd::new(0, EfdFlags::NONE).unwrap());
    let half = shm.len() / 2;
    let t = {
        let (shm, ping, pong) = (shm.clone(), ping.clone(), pong.clone());
        thread::spawn(move || {
            assert_eq!(ping.read().unwrap(), 1);
            assert!(check(&shm, 0, half, 0));
            fill(&shm, half, shm.len(), 0x5a);
            pong.write(1).unwrap();
        })
    };
    fill(&shm, 0, half, 0);
    ping.write(1).unwrap();
    assert_eq!(pong.read().unwrap(), 1);
    assert!(check(&shm, half, shm.len(), 0x5a));
    t.join().unwrap();
}

/// unix: a forked child shares the memory without any code.
#[cfg(unix)]
#[test]
fn fork_shares_the_memory() {
    let shm = GkSharedMemory::new(256 << 10).unwrap();
    let ping = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let pong = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let half = shm.len() / 2;
    // SAFETY: the child only touches the memory and the eventfds (no
    // allocation) and leaves with `_exit`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if ping.read().ok() != Some(1) || !check(&shm, 0, half, 0) {
                return false;
            }
            fill(&shm, half, shm.len(), 0xc3);
            pong.write(1).is_ok()
        }))
        .unwrap_or(false);
        // SAFETY: leaves the forked child without running the harness.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    fill(&shm, 0, half, 0);
    ping.write(1).unwrap();
    assert_eq!(pong.read().unwrap(), 1);
    assert!(check(&shm, half, shm.len(), 0xc3));
    let mut status = 0;
    // SAFETY: waits for our own child.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
}

/// unix: over a unix socket (`SCM_RIGHTS`), and wrong kinds are refused.
#[cfg(unix)]
#[test]
fn send_to_and_recv_from() {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    let (a, b) = UnixStream::pair().unwrap();
    let shm = GkSharedMemory::new(100_000).unwrap();
    fill(&shm, 0, 1000, 0);
    shm.send_to(&a).unwrap();
    let got = GkSharedMemory::recv_from(&b).unwrap();
    assert_eq!(got.len(), shm.len());
    assert_eq!(got.page_size(), shm.page_size());
    assert_ne!(got.as_ptr(), shm.as_ptr());
    assert!(check(&got, 0, 1000, 0));
    fill(&got, 1000, 2000, 1);
    assert!(check(&shm, 1000, 2000, 1));
    // The sender keeps its view after the receiver drops its own.
    drop(got);
    fill(&shm, 0, shm.len(), 2);
    let _ = shm.as_fd();

    // An eventfd where shared memory is expected, and the other way round.
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    efd.send_to(&a).unwrap();
    assert_eq!(
        GkSharedMemory::recv_from(&b).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    shm.send_to(&a).unwrap();
    assert_eq!(
        GkEventFd::recv_from(&b).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    drop(a);
    assert_eq!(
        GkSharedMemory::recv_from(&b).unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );
}

/// The uniform process API (all OS), between two threads of this process.
#[test]
fn through_gk_process() {
    let name = format!("shm-test-{}", std::process::id());
    let listener = GkListener::bind(&name).unwrap();
    let client = thread::spawn(move || {
        let mut peer = GkProcess::connect(&name).unwrap();
        let shm: GkSharedMemory = peer.recv_as().unwrap();
        let ping = peer.recv().unwrap();
        let pong = peer.recv().unwrap();
        // A channel end is not shared memory: refused.
        assert_eq!(
            peer.recv_as::<GkSharedMemory>().unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        // Shared memory is not an eventfd: refused.
        assert_eq!(peer.recv().unwrap_err().kind(), ErrorKind::InvalidData);
        assert_eq!(ping.read().unwrap(), 1);
        assert!(check(&shm, 0, 4096, 0));
        fill(&shm, 4096, 8192, 0x11);
        pong.write(1).unwrap();
        // Keep `peer` until the other side is done (Windows pull model).
        thread::sleep(Duration::from_millis(200));
    });
    let mut peer = listener.accept().unwrap();
    let shm = GkSharedMemory::new(8192).unwrap();
    let ping = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let pong = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    peer.send(&shm).unwrap();
    peer.send(&ping).unwrap();
    peer.send(&pong).unwrap();
    let (_tx, rx) = adaptif_gatotkaca_eventfd::GkChannel::new().unwrap();
    peer.send(&rx).unwrap();
    peer.send(&shm).unwrap();
    fill(&shm, 0, 4096, 0);
    ping.write(1).unwrap();
    assert_eq!(pong.read().unwrap(), 1);
    assert!(check(&shm, 4096, 8192, 0x11));
    client.join().unwrap();
}

/// Windows: the section handle is exposed.
#[cfg(windows)]
#[test]
fn section_handle() {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    let shm = GkSharedMemory::new(1).unwrap();
    assert!(!shm.as_handle().as_raw_handle().is_null());
}

//! Sharing an eventfd with other processes, on every OS, through each way
//! the crate offers (modelled on Linux):
//!
//! * `export_inheritable` + `import`: a spawned child inherits it (token in
//!   an environment variable);
//! * unix: `send_to` / `recv_from` over a unix socket the child connects to
//!   (`SCM_RIGHTS`, as for unrelated processes);
//! * Windows: `export_to(pid)` + `import` (token on the child's stdin);
//! * the uniform process API: `GkProcess::spawn` + `send` in the parent,
//!   `GkProcess::from_parent` + `recv` in the child.
//!
//! Each test re-runs this test binary as the child, filtered to
//! `child_entry`, with `GK_CHILD_ROLE` naming what the child should do. The
//! child panics (non-zero exit) if a check fails. On Linux this runs against
//! the kernel eventfd, so the shims are held to the same expectations.

use std::io::ErrorKind;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use adaptif_gatotkaca_eventfd::{
    ChannelOptions, EfdFlags, GkChannel, GkEventFd, GkListener, GkMpSender, GkProcess, GkReceiver, GkSender, GkSharedMemory, GkShmPool,
    GkShmPoolView, ShmLease,
};

/// How the child gets the eventfd.
enum Via<'a> {
    /// Token in `GK_TOKEN` (inheritable export).
    Env(&'a str),
    /// unix: connects to the socket at this path and receives it.
    #[cfg(unix)]
    Socket(&'a std::path::Path),
    /// Windows: token on stdin, after spawn (`export_to(child.id())`).
    #[cfg(windows)]
    Stdin,
}

fn spawn(role: &str, via: Via<'_>) -> Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"]).env("GK_CHILD_ROLE", role);
    match via {
        Via::Env(token) => {
            cmd.env("GK_TOKEN", token);
        }
        #[cfg(unix)]
        Via::Socket(path) => {
            cmd.env("GK_SOCKET", path);
        }
        #[cfg(windows)]
        Via::Stdin => {
            cmd.stdin(std::process::Stdio::piped());
        }
    }
    cmd.spawn().unwrap()
}

/// A spawned child, plus the connection that must outlive the hand-over
/// (Windows `GkProcess` keeps the offered section until dropped).
struct Spawned(Child, Option<GkProcess>);

/// Spawns a child via inheritance (all OS).
fn spawn_inherited(efd: &GkEventFd, role: &str) -> Spawned {
    let export = efd.export_inheritable().unwrap();
    let child = spawn(role, Via::Env(export.token()));
    drop(export);
    Spawned(child, None)
}

/// Spawns a child through the uniform process API (all OS).
fn spawn_process(efd: &GkEventFd, role: &str) -> Spawned {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"]).env("GK_CHILD_ROLE", role);
    let (child, mut peer) = GkProcess::spawn(&mut cmd).unwrap();
    peer.send(efd).unwrap();
    Spawned(child, Some(peer))
}

/// Spawns a child that gets the eventfd the "unrelated process" way: over a
/// unix socket on unix, `export_to` + stdin on Windows.
fn spawn_passed(efd: &GkEventFd, role: &str) -> Spawned {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("gkefd-{}-{}.sock", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let child = spawn(role, Via::Socket(&path));
        let (stream, _) = listener.accept().unwrap();
        efd.send_to(&stream).unwrap();
        let _ = std::fs::remove_file(&path);
        Spawned(child, None)
    }
    #[cfg(windows)]
    {
        use std::io::Write;
        let mut child = spawn(role, Via::Stdin);
        let token = efd.export_to(child.id()).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{token}").unwrap();
        Spawned(child, None)
    }
}

/// Waits up to 20 s for the child and asserts it succeeded.
fn join(Spawned(mut child, _keep): Spawned) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child failed: {status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Readable within `ms` milliseconds (poll / WSAPoll on the eventfd).
fn readable_within(efd: &GkEventFd, ms: i32) -> bool {
    #[cfg(unix)]
    {
        let mut pfd = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(efd), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        assert!(n >= 0);
        n > 0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};
        let mut fd = WSAPOLLFD { fd: std::os::windows::io::AsRawSocket::as_raw_socket(efd) as SOCKET, events: POLLRDNORM, revents: 0 };
        // SAFETY: one valid WSAPOLLFD; the socket is open.
        let n = unsafe { WSAPoll(&mut fd, 1, ms) };
        assert!(n >= 0);
        n > 0
    }
}

fn would_block<T>(r: std::io::Result<T>) -> bool {
    matches!(r, Err(e) if e.kind() == ErrorKind::WouldBlock)
}

/// The child side. Does nothing unless spawned by a test below.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var("GK_CHILD_ROLE") else {
        return;
    };
    if role.starts_with("channel_") {
        channel_child(&role);
        return;
    }
    if role == "shm_reply" {
        shm_child();
        return;
    }
    if role.starts_with("bytes_") {
        bytes_child(&role);
        return;
    }
    let efd = if let Ok(token) = std::env::var("GK_TOKEN") {
        // SAFETY: the parent made this token for this process, once.
        unsafe { GkEventFd::import(&token) }.expect("import")
    } else if std::env::var_os(adaptif_gatotkaca_eventfd::process::PARENT_ENV).is_some() {
        GkProcess::from_parent().unwrap().recv().expect("GkProcess::recv")
    } else {
        receive()
    };
    match role.as_str() {
        "read5_write7" => {
            assert_eq!(efd.read().unwrap(), 5);
            assert!(would_block(efd.read()));
            efd.write(7).unwrap();
        }
        "blocking_read42" => assert_eq!(efd.read().unwrap(), 42),
        "sleep_write3" => {
            thread::sleep(Duration::from_millis(200));
            efd.write(3).unwrap();
        }
        "write_ones" => (0..1_000).for_each(|_| efd.write(1).unwrap()),
        "set_nonblocking" => efd.set_nonblocking(true).unwrap(),
        "readable_then_read1" => {
            // The parent wrote 1 before handing it over: readable here too.
            assert!(readable_within(&efd, 2_000));
            assert_eq!(efd.read().unwrap(), 1);
        }
        "clone_and_drop" => drop(efd.try_clone().unwrap()),
        other => panic!("unknown role {other}"),
    }
}

#[cfg(unix)]
fn receive() -> GkEventFd {
    let path = std::env::var("GK_SOCKET").expect("GK_SOCKET");
    let stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    GkEventFd::recv_from(&stream).expect("recv_from")
}

#[cfg(windows)]
fn receive() -> GkEventFd {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    // SAFETY: the parent made this token for this process, once.
    unsafe { GkEventFd::import(&line) }.expect("import")
}

/// Runs `test` with every way of handing the eventfd over.
fn both(test: impl Fn(fn(&GkEventFd, &str) -> Spawned)) {
    test(spawn_inherited);
    test(spawn_passed);
    test(spawn_process);
}

#[test]
fn shares_both_ways() {
    both(|spawn| {
        let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
        efd.write(5).unwrap();
        join(spawn(&efd, "read5_write7"));
        assert!(readable_within(&efd, 2_000));
        assert_eq!(efd.read().unwrap(), 7);
        assert!(!readable_within(&efd, 0));
    });
}

#[test]
fn blocking_read_in_child_woken_by_parent() {
    both(|spawn| {
        let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
        let mut child = spawn(&efd, "blocking_read42");
        thread::sleep(Duration::from_millis(300));
        assert!(child.0.try_wait().unwrap().is_none(), "child did not block");
        efd.write(42).unwrap();
        join(child);
    });
}

#[test]
fn blocking_read_in_parent_woken_by_child() {
    both(|spawn| {
        let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
        let child = spawn(&efd, "sleep_write3");
        assert_eq!(efd.read().unwrap(), 3);
        join(child);
    });
}

/// Several processes hammer one counter while the parent consumes it.
#[test]
fn children_contend_without_losing_counts() {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE).unwrap();
    let children = vec![spawn_inherited(&efd, "write_ones"), spawn_passed(&efd, "write_ones"), spawn_process(&efd, "write_ones")];
    let mut consumed = 0u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while consumed < 3_000 {
        assert!(Instant::now() < deadline, "only consumed {consumed}");
        match efd.read() {
            Ok(n) => consumed += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::yield_now(),
            Err(e) => panic!("{e}"),
        }
    }
    children.into_iter().for_each(join);
    assert_eq!(consumed, 3_000);
    assert!(would_block(efd.read()));
    assert!(!readable_within(&efd, 0));
}

#[test]
fn nonblocking_mode_is_shared() {
    both(|spawn| {
        let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
        join(spawn(&efd, "set_nonblocking"));
        assert!(efd.is_nonblocking().unwrap());
        assert!(would_block(efd.read()));
    });
}

#[test]
fn child_sees_readiness() {
    both(|spawn| {
        let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
        efd.write(1).unwrap();
        join(spawn(&efd, "readable_then_read1"));
        assert!(!readable_within(&efd, 0));
    });
}

#[test]
fn child_exit_keeps_parent_working() {
    both(|spawn| {
        let efd = GkEventFd::new(1, EfdFlags::NONBLOCK).unwrap();
        join(spawn(&efd, "clone_and_drop"));
        assert_eq!(efd.read().unwrap(), 1);
        efd.write(9).unwrap();
        assert_eq!(efd.read().unwrap(), 9);
    });
}

#[test]
fn import_rejects_garbage() {
    for token in ["not a token", "gkefd1:x:1", "gkefd1:n:-1"] {
        // SAFETY: rejected before any descriptor is taken over.
        let err = unsafe { GkEventFd::import(token) }.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{token:?}");
    }
}

/// unix: export + import and send + receive inside one process.
#[cfg(unix)]
#[test]
fn same_process_round_trips() {
    let a = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
    let export = a.export_inheritable().unwrap();
    let token = export.token().to_owned();
    // The import takes over the export's copies, so the export must not
    // close them as well.
    std::mem::forget(export);
    // SAFETY: made in this process, imported once, and the export's
    // ownership was given up above.
    let b = unsafe { GkEventFd::import(&token) }.unwrap();
    a.write(2).unwrap();
    assert_eq!(b.read().unwrap(), 2);

    let (tx, rx) = std::os::unix::net::UnixStream::pair().unwrap();
    a.send_to(&tx).unwrap();
    let c = GkEventFd::recv_from(&rx).unwrap();
    c.write(4).unwrap();
    assert!(readable_within(&b, 0));
    assert_eq!(a.read().unwrap(), 4);
    assert!(!readable_within(&c, 0));
}

/// The uniform process API between two threads: bind/accept and connect,
/// send and recv in both directions.
#[test]
fn listener_and_connect() {
    let name = format!("test-{}", std::process::id());
    let listener = GkListener::bind(&name).unwrap();
    let client = thread::spawn(move || {
        let mut peer = GkProcess::connect(&name).unwrap();
        let efd = peer.recv().unwrap();
        efd.write(5).unwrap();
        let back = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
        peer.send(&back).unwrap();
        // Keep `peer` until the other side has received `back`.
        thread::sleep(Duration::from_millis(500));
        back.read().unwrap()
    });
    let mut peer = listener.accept().unwrap();
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    peer.send(&efd).unwrap();
    assert_eq!(efd.read().unwrap(), 5);
    let back = peer.recv().unwrap();
    back.write(8).unwrap();
    assert_eq!(client.join().unwrap(), 8);
}

#[test]
fn from_parent_without_parent_fails() {
    if std::env::var_os(adaptif_gatotkaca_eventfd::process::PARENT_ENV).is_some() {
        return; // running as a spawned child
    }
    let err = GkProcess::from_parent().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

// ---------------------------------------------------------------------------
// Channel ends handed to another process through GkProcess
// ---------------------------------------------------------------------------

const MESSAGES: u32 = 500;

fn message(i: u32) -> Vec<u8> {
    (0..(i * 37 % 3000) as usize).map(|k| (k as u32 ^ i) as u8).collect()
}

fn channel() -> (GkSender, GkReceiver) {
    GkChannel::with_options(ChannelOptions { capacity: 1 << 16, max_message: 4000, ..Default::default() }).unwrap()
}

fn channel_child(role: &str) {
    let mut parent = GkProcess::from_parent().unwrap();
    match role {
        // Receive the receiver and read everything the parent sends.
        "channel_recv" => {
            let mut rx: GkReceiver = parent.recv_as().expect("recv_as::<GkReceiver>");
            let mut buf = Vec::new();
            for i in 0..MESSAGES {
                rx.recv_vec(&mut buf).unwrap();
                assert_eq!(buf, message(i), "message {i}");
            }
            // The parent drops its sender after the last message.
            assert_eq!(rx.recv_vec(&mut buf).unwrap_err().kind(), ErrorKind::BrokenPipe);
        }
        // Receive the sender and write to the parent.
        "channel_send" => {
            let mut tx: GkSender = parent.recv_as().expect("recv_as::<GkSender>");
            for i in 0..MESSAGES {
                tx.send(&message(i)).unwrap();
            }
        }
        // Receive the receiver, then a large bulk transfer and a message.
        "channel_bulk" => {
            let mut rx: GkReceiver = parent.recv_as().expect("recv_as::<GkReceiver>");
            let mut data = Vec::new();
            let (kind, id) = rx.recv_bulk_vec_kind(&mut data, 64 << 20).unwrap();
            assert_eq!(kind, 0xb01c);
            assert_eq!(data.len(), BULK_LEN);
            assert!(data == bulk_data(), "bulk data differs");
            let mut buf = [0u8; 8];
            assert_eq!(rx.recv_kind(&mut buf).unwrap(), (0xfffe, 8));
            assert_eq!(u64::from_le_bytes(buf), id);
        }
        // Receive a multi-producer sender and write from several threads.
        "channel_mpsc_send" => {
            let tx: GkMpSender = parent.recv_as().expect("recv_as::<GkMpSender>");
            let threads: Vec<_> = (0..MPSC_THREADS)
                .map(|t| {
                    let tx = tx.clone();
                    thread::spawn(move || {
                        for i in 0..MESSAGES {
                            tx.send_kind(t, &message(i)).unwrap();
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
        }
        // Borrow pool blocks named by leases on the channel; check, drop.
        "channel_pool_borrow" => {
            let view: GkShmPoolView = parent.recv_as().expect("recv_as::<GkShmPoolView>");
            let mut rx: GkReceiver = parent.recv_as().expect("recv_as::<GkReceiver>");
            let mut i = 0u32;
            loop {
                let mut b = [0u8; 16];
                match rx.recv(&mut b) {
                    Ok(16) => {}
                    Ok(n) => panic!("lease of {n} bytes"),
                    Err(e) if e.kind() == ErrorKind::BrokenPipe => break,
                    Err(e) => panic!("{e}"),
                }
                // SAFETY: each lease is sent once, by the pool's owner.
                let buf = unsafe { view.take(ShmLease::from_bytes(&b)) }.unwrap();
                assert_eq!(&buf[..], &message(i)[..], "block {i}");
                i += 1;
            }
            assert_eq!(i, MESSAGES);
        }
        // Expect a plain sender but get a multi-producer one: refused.
        "channel_mpsc_wrong_kind" => {
            let r = parent.recv_as::<GkSender>();
            assert_eq!(r.unwrap_err().kind(), ErrorKind::InvalidData);
        }
        other => panic!("unknown role {other}"),
    }
}

const MPSC_THREADS: u16 = 4;

const BULK_LEN: usize = 12 << 20;

fn bulk_data() -> Vec<u8> {
    (0..BULK_LEN as u32).map(|i| i.wrapping_mul(2654435761).rotate_left(7) as u8).collect()
}

fn spawn_channel_child(role: &str) -> (Child, GkProcess) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"]).env("GK_CHILD_ROLE", role);
    GkProcess::spawn(&mut cmd).unwrap()
}

#[test]
fn channel_parent_sends_child_receives() {
    let (mut tx, rx) = channel();
    let (child, mut peer) = spawn_channel_child("channel_recv");
    peer.send(&rx).unwrap();
    // Our copy now belongs to the child: unusable here, and dropping it
    // doesn't close the channel.
    let mut rx = rx;
    assert_eq!(rx.next_len().unwrap_err().kind(), ErrorKind::PermissionDenied);
    drop(rx);
    for i in 0..MESSAGES {
        tx.send(&message(i)).unwrap();
    }
    drop(tx);
    join(Spawned(child, Some(peer)));
}

#[test]
fn channel_child_sends_parent_receives() {
    let (tx, mut rx) = channel();
    let (child, mut peer) = spawn_channel_child("channel_send");
    peer.send(&tx).unwrap();
    drop(tx);
    let mut buf = Vec::new();
    for i in 0..MESSAGES {
        rx.recv_vec(&mut buf).unwrap();
        assert_eq!(buf, message(i), "message {i}");
    }
    // The child exits after its last message: its sender is dropped.
    assert_eq!(rx.recv_vec(&mut buf).unwrap_err().kind(), ErrorKind::BrokenPipe);
    join(Spawned(child, Some(peer)));
}

/// The bulk test over a channel on huge pages where available (Prefer), or
/// required with `GK_TEST_HUGEPAGES=1`: the child maps the same pages.
#[test]
fn channel_bulk_to_child_on_huge_pages() {
    let huge_pages = if std::env::var_os("GK_TEST_HUGEPAGES").is_some() {
        adaptif_gatotkaca_eventfd::HugePages::Require
    } else {
        adaptif_gatotkaca_eventfd::HugePages::Prefer
    };
    let (mut tx, rx) = GkChannel::with_options(ChannelOptions { huge_pages, ..Default::default() }).unwrap();
    let (child, mut peer) = spawn_channel_child("channel_bulk");
    peer.send(&rx).unwrap();
    drop(rx);
    let id = tx.send_bulk_kind(0xb01c, &bulk_data()).unwrap();
    tx.send_kind(0xfffe, &id.to_le_bytes()).unwrap();
    join(Spawned(child, Some(peer)));
}

#[test]
fn channel_bulk_to_child() {
    let (mut tx, rx) = channel();
    let (child, mut peer) = spawn_channel_child("channel_bulk");
    peer.send(&rx).unwrap();
    drop(rx);
    // 12 MiB through the default ring, then its id as a message.
    let id = tx.send_bulk_kind(0xb01c, &bulk_data()).unwrap();
    tx.send_kind(0xfffe, &id.to_le_bytes()).unwrap();
    join(Spawned(child, Some(peer)));
}

// ---------------------------------------------------------------------------
// Shared memory handed to another process through GkProcess
// ---------------------------------------------------------------------------

const SHM_LEN: usize = 2 << 20;

fn shm_byte(i: usize) -> u8 {
    (i.wrapping_mul(2654435761) >> 9) as u8
}

/// Receives the memory and two doorbells, checks the parent's pattern in the
/// first half and writes a reply into the second.
fn shm_child() {
    let mut parent = GkProcess::from_parent().unwrap();
    let shm: GkSharedMemory = parent.recv_as().expect("recv_as::<GkSharedMemory>");
    let ping = parent.recv().expect("ping");
    let pong = parent.recv().expect("pong");
    assert_eq!(shm.len(), SHM_LEN);
    assert_eq!(ping.read().unwrap(), 1);
    let half = SHM_LEN / 2;
    {
        // SAFETY: the parent no longer writes the first half, and only this
        // process writes the second until `pong`.
        let mem = unsafe { shm.as_mut_slice() };
        assert!((0..half).all(|i| mem[i] == shm_byte(i)), "pattern differs");
        for (i, b) in mem.iter_mut().enumerate().skip(half) {
            *b = !shm_byte(i);
        }
    }
    pong.write(1).unwrap();
}

#[test]
fn shared_memory_with_child() {
    let shm = GkSharedMemory::new(SHM_LEN).unwrap();
    let ping = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let pong = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let (child, mut peer) = spawn_channel_child("shm_reply");
    peer.send(&shm).unwrap();
    peer.send(&ping).unwrap();
    peer.send(&pong).unwrap();
    let half = SHM_LEN / 2;
    {
        // SAFETY: the child reads the first half only after `ping`.
        let mem = unsafe { shm.as_mut_slice() };
        for (i, b) in mem.iter_mut().enumerate().take(half) {
            *b = shm_byte(i);
        }
    }
    ping.write(1).unwrap();
    assert_eq!(pong.read().unwrap(), 1);
    join(Spawned(child, Some(peer)));
    // The child has exited; this process still has the memory.
    // SAFETY: no other process uses it any more.
    let mem = unsafe { shm.as_slice() };
    assert!((half..SHM_LEN).all(|i| mem[i] == !shm_byte(i)), "reply differs");
    assert!((0..half).all(|i| mem[i] == shm_byte(i)));
}

#[test]
fn channel_mpsc_child_threads_send_parent_receives() {
    let (tx, mut rx) = GkChannel::mpsc_with_options(ChannelOptions { capacity: 1 << 16, max_message: 4000, ..Default::default() }).unwrap();
    let (child, mut peer) = spawn_channel_child("channel_mpsc_send");
    peer.send(&tx).unwrap();
    assert_eq!(tx.try_send(b"x").unwrap_err().kind(), ErrorKind::PermissionDenied);
    drop(tx);
    let mut next = vec![0u32; MPSC_THREADS as usize];
    let mut buf = Vec::new();
    for _ in 0..u32::from(MPSC_THREADS) * MESSAGES {
        let (t, _) = rx.recv_vec_kind(&mut buf).unwrap();
        let i = next[t as usize];
        assert_eq!(buf, message(i), "thread {t} message {i}");
        next[t as usize] += 1;
    }
    assert_eq!(rx.recv_vec(&mut buf).unwrap_err().kind(), ErrorKind::BrokenPipe);
    join(Spawned(child, Some(peer)));
}

#[test]
fn channel_mpsc_sender_received_as_plain_sender_is_refused() {
    let (tx, mut rx) = GkChannel::new_mpsc().unwrap();
    let (child, mut peer) = spawn_channel_child("channel_mpsc_wrong_kind");
    peer.send(&tx).unwrap();
    drop(tx);
    // The child closed the end it could not use: no hang here.
    assert_eq!(rx.recv(&mut [0u8; 8]).unwrap_err().kind(), ErrorKind::BrokenPipe);
    join(Spawned(child, Some(peer)));
}

#[test]
fn pool_blocks_lent_to_a_child_come_back() {
    let pool = GkShmPool::new(1 << 20).unwrap();
    let (tx, rx) = GkChannel::new_mpsc().unwrap();
    let (child, mut peer) = spawn_channel_child("channel_pool_borrow");
    peer.send(&pool.view()).unwrap();
    peer.send(&rx).unwrap();
    drop(rx);
    for i in 0..MESSAGES {
        let data = message(i);
        let mut block = loop {
            match pool.alloc(data.len()) {
                Ok(b) => break b,
                // The child gives blocks back as it reads them.
                Err(e) if e.kind() == ErrorKind::OutOfMemory => thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        };
        block.as_mut_slice()[..data.len()].copy_from_slice(&data);
        tx.send(&block.lend(data.len()).to_bytes()).unwrap();
    }
    drop(tx);
    join(Spawned(child, Some(peer)));
    let s = pool.stats();
    assert_eq!((s.lent, s.bad_returns, s.free), (0, 0, 1 << 20), "every block came back from the child");
}

// ---------------------------------------------------------------------------
// Byte messages, the connection's descriptor and the peer's pid
// ---------------------------------------------------------------------------

use adaptif_gatotkaca_eventfd::process::BYTES_MAX;

/// OSes where `GkProcess::peer_pid` knows the peer.
const PEER_PID_KNOWN: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos",
    windows
));

fn bytes_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8).collect()
}

fn spawn_bytes_child(role: &str) -> (Child, GkProcess) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("GK_CHILD_ROLE", role)
        .env("GK_PARENT_PID", std::process::id().to_string());
    GkProcess::spawn(&mut cmd).unwrap()
}

fn bytes_child(role: &str) {
    let mut parent = GkProcess::from_parent().unwrap();
    let mut buf = Vec::new();
    match role {
        // Echo every message back until an empty one arrives (echoed too).
        "bytes_echo" => {
            if PEER_PID_KNOWN {
                let want: u32 = std::env::var("GK_PARENT_PID").unwrap().parse().unwrap();
                assert_eq!(parent.peer_pid(), Some(want), "peer_pid in the child");
            }
            loop {
                let n = parent.recv_bytes(&mut buf).expect("recv_bytes");
                assert_eq!(n, buf.len());
                parent.send_bytes(&buf).unwrap();
                if n == 0 {
                    break;
                }
            }
        }
        // Bytes and objects mixed on one connection, received in order.
        "bytes_mixed" => {
            parent.recv_bytes(&mut buf).unwrap();
            assert_eq!(buf, b"first");
            let efd = parent.recv().expect("eventfd after bytes");
            parent.recv_bytes(&mut buf).unwrap();
            assert_eq!(buf, bytes_payload(BYTES_MAX));
            let shm: GkSharedMemory = parent.recv_as().expect("shared memory after bytes");
            parent.recv_bytes(&mut buf).unwrap();
            assert_eq!(buf, b"");
            // SAFETY: the parent wrote before sending and doesn't write now.
            let first = unsafe { shm.as_slice() }[0];
            parent.send_bytes(&[first]).unwrap();
            efd.write(9).unwrap();
            parent.send(&efd).unwrap();
            parent.send_bytes(b"last").unwrap();
            // Keep `parent` (Windows: the offered handles) until the parent
            // has received everything.
            let _ = parent.recv_bytes(&mut buf);
        }
        // The wrong kind is refused both ways; the connection stays usable.
        "bytes_wrong_kind" => {
            let e = parent.recv_bytes(&mut buf).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidData, "object received as bytes: {e}");
            let e = parent.recv().unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidData, "bytes received as an object: {e}");
            let e = parent.recv_as::<GkReceiver>().unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidData, "large bytes received as an object: {e}");
            parent.recv_bytes(&mut buf).unwrap();
            assert_eq!(buf, b"still in sync");
            let efd = parent.recv().expect("eventfd after the refusals");
            efd.write(1).unwrap();
            parent.send_bytes(b"ok").unwrap();
            let _ = parent.recv_bytes(&mut buf);
        }
        // Say we are ready, then wait to be killed.
        "bytes_wait" => {
            parent.send_bytes(b"ready").unwrap();
            thread::sleep(Duration::from_secs(60));
        }
        other => panic!("unknown role {other}"),
    }
}

/// Byte messages parent <-> child: empty, small, exactly `BYTES_MAX`, and
/// too large (refused before anything is sent); `peer_pid` on both sides.
#[test]
fn bytes_round_trip_with_child() {
    let (child, mut peer) = spawn_bytes_child("bytes_echo");
    assert_eq!(peer.peer_pid(), None, "not connected yet");
    let too_big = vec![0u8; BYTES_MAX + 1];
    assert_eq!(peer.send_bytes(&too_big).unwrap_err().kind(), ErrorKind::InvalidInput);
    let mut buf = Vec::new();
    for len in [1, 5, 4096, 65_000, BYTES_MAX, 0] {
        let msg = bytes_payload(len);
        // The largest messages don't fit the socket buffer: the child reads
        // while we write, then echoes.
        peer.send_bytes(&msg).unwrap();
        assert_eq!(peer.recv_bytes(&mut buf).unwrap(), len);
        assert!(buf == msg, "echo of {len} bytes differs");
        if PEER_PID_KNOWN {
            assert_eq!(peer.peer_pid(), Some(child.id()), "peer_pid in the parent");
        }
    }
    join(Spawned(child, Some(peer)));
}

/// Bytes and objects mixed on one connection arrive in order.
#[test]
fn bytes_and_objects_mixed_in_order() {
    let (child, mut peer) = spawn_bytes_child("bytes_mixed");
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    let shm = GkSharedMemory::new(4096).unwrap();
    // SAFETY: nobody else uses the memory yet.
    unsafe { shm.as_mut_slice()[0] = 0x5a };
    peer.send_bytes(b"first").unwrap();
    peer.send(&efd).unwrap();
    peer.send_bytes(&bytes_payload(BYTES_MAX)).unwrap();
    peer.send(&shm).unwrap();
    peer.send_bytes(b"").unwrap();
    let mut buf = Vec::new();
    peer.recv_bytes(&mut buf).unwrap();
    assert_eq!(buf, [0x5a]);
    let back = peer.recv().unwrap();
    assert_eq!(efd.read().unwrap(), 9);
    back.write(2).unwrap();
    assert_eq!(efd.read().unwrap(), 2, "the eventfd came back");
    peer.recv_bytes(&mut buf).unwrap();
    assert_eq!(buf, b"last");
    peer.send_bytes(b"done").unwrap();
    join(Spawned(child, Some(peer)));
}

/// Receiving the wrong kind is `InvalidData` and leaves the connection in
/// sync, both ways.
#[test]
fn bytes_wrong_kind_is_refused() {
    let (child, mut peer) = spawn_bytes_child("bytes_wrong_kind");
    let efd = GkEventFd::new(0, EfdFlags::NONE).unwrap();
    peer.send(&efd).unwrap(); // received as bytes: refused
    peer.send_bytes(b"not an object").unwrap(); // received as an eventfd: refused
    peer.send_bytes(&bytes_payload(BYTES_MAX)).unwrap(); // received as a receiver: refused
    peer.send_bytes(b"still in sync").unwrap();
    peer.send(&efd).unwrap();
    assert_eq!(efd.read().unwrap(), 1);
    let mut buf = Vec::new();
    peer.recv_bytes(&mut buf).unwrap();
    assert_eq!(buf, b"ok");
    peer.send_bytes(b"done").unwrap();
    join(Spawned(child, Some(peer)));
}

/// The connection is a lifeline: when the child is killed, its descriptor
/// turns readable (unix) and receiving reports EOF.
#[test]
fn connection_turns_readable_when_the_child_dies() {
    let (mut child, mut peer) = spawn_bytes_child("bytes_wait");
    let mut buf = Vec::new();
    peer.recv_bytes(&mut buf).unwrap();
    assert_eq!(buf, b"ready");
    #[cfg(unix)]
    let poll = |ms| {
        let mut pfd = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&peer), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        assert!(n >= 0);
        n > 0
    };
    #[cfg(unix)]
    assert!(!poll(0), "readable while the child lives and sends nothing");
    child.kill().unwrap();
    child.wait().unwrap();
    #[cfg(unix)]
    assert!(poll(5_000), "not readable after the child died");
    let e = peer.recv_bytes(&mut buf).unwrap_err();
    assert!(matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset), "{e}");
}

/// In one process both ends name this process.
#[test]
fn peer_pid_between_threads() {
    let name = format!("test-pid-{}", std::process::id());
    let listener = GkListener::bind(&name).unwrap();
    let client = thread::spawn(move || {
        let mut peer = GkProcess::connect(&name).unwrap();
        let pid = peer.peer_pid();
        let mut buf = Vec::new();
        peer.recv_bytes(&mut buf).unwrap();
        (pid, buf)
    });
    let mut peer = listener.accept().unwrap();
    peer.send_bytes(b"hi").unwrap();
    let (client_pid, buf) = client.join().unwrap();
    assert_eq!(buf, b"hi");
    let me = Some(std::process::id());
    if PEER_PID_KNOWN {
        assert_eq!(peer.peer_pid(), me);
        assert_eq!(client_pid, me);
    } else {
        assert_eq!(peer.peer_pid(), None);
    }
}

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
    ChannelOptions, EfdFlags, GkChannel, GkEventFd, GkListener, GkProcess, GkReceiver, GkSender,
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
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("GK_CHILD_ROLE", role);
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
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("GK_CHILD_ROLE", role);
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
        let path = std::env::temp_dir().join(format!(
            "gkefd-{}-{}.sock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
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
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(efd),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        assert!(n >= 0);
        n > 0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll};
        let mut fd = WSAPOLLFD {
            fd: std::os::windows::io::AsRawSocket::as_raw_socket(efd) as SOCKET,
            events: POLLRDNORM,
            revents: 0,
        };
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
    let efd = if let Ok(token) = std::env::var("GK_TOKEN") {
        // SAFETY: the parent made this token for this process, once.
        unsafe { GkEventFd::import(&token) }.expect("import")
    } else if std::env::var_os(adaptif_gatotkaca_eventfd::process::PARENT_ENV).is_some() {
        GkProcess::from_parent()
            .unwrap()
            .recv()
            .expect("GkProcess::recv")
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
    let children = vec![
        spawn_inherited(&efd, "write_ones"),
        spawn_passed(&efd, "write_ones"),
        spawn_process(&efd, "write_ones"),
    ];
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
    (0..(i * 37 % 3000) as usize)
        .map(|k| (k as u32 ^ i) as u8)
        .collect()
}

fn channel() -> (GkSender, GkReceiver) {
    GkChannel::with_options(ChannelOptions {
        capacity: 1 << 16,
        max_message: 4000,
        ..Default::default()
    })
    .unwrap()
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
            assert_eq!(
                rx.recv_vec(&mut buf).unwrap_err().kind(),
                ErrorKind::BrokenPipe
            );
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
        other => panic!("unknown role {other}"),
    }
}

const BULK_LEN: usize = 12 << 20;

fn bulk_data() -> Vec<u8> {
    (0..BULK_LEN as u32)
        .map(|i| i.wrapping_mul(2654435761).rotate_left(7) as u8)
        .collect()
}

fn spawn_channel_child(role: &str) -> (Child, GkProcess) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("GK_CHILD_ROLE", role);
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
    assert_eq!(
        rx.next_len().unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
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
    assert_eq!(
        rx.recv_vec(&mut buf).unwrap_err().kind(),
        ErrorKind::BrokenPipe
    );
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

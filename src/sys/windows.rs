//! Windows backend: user-space [`Counter`] + loopback TCP socket pair.
//!
//! # Design
//!
//! All eventfd semantics (blocking, non-blocking, semaphore mode, overflow)
//! live in the shared [`Counter`]. This file only provides a [`Notifier`]
//! that mirrors "counter > 0" onto a pollable socket:
//!
//! * A connected loopback TCP pair (`writer` → `reader`) is created per
//!   eventfd and shared by all clones ([`Shared`]).
//! * `set_readable` (counter 0 → >0) sends exactly one byte on `writer`;
//!   `clear_readable` (counter >0 → 0) receives it from `reader`. Both run
//!   under the counter lock, so **exactly one byte is in flight iff the
//!   counter is > 0**, and `reader` is readable iff the counter is > 0.
//! * Every [`Inner`] (the original and each `try_clone`) owns its *own*
//!   duplicate of `reader` (`WSADuplicateSocketW` via
//!   [`TcpStream::try_clone`]). That duplicate is what [`AsSocket`] returns and
//!   what mio registers, so registrations of different clones never collide.
//!   It refers to the same underlying socket, so it becomes readable when the
//!   byte arrives. No I/O is ever done on it: draining always goes through
//!   the shared `reader`.
//! * All sockets are non-blocking; blocking eventfd semantics come from the
//!   counter's `Condvar`, never from the socket.
//!
//! # Loopback delivery latency
//!
//! On loopback TCP a byte that `send` accepted is not guaranteed to be
//! `recv`-able in the same instant (it is almost always delivered
//! synchronously, but filter drivers / WFP callouts / scheduling can delay
//! it). `clear_readable` is only ever called while the counter is > 0, which
//! means our byte was sent. So if `recv` reports `WSAEWOULDBLOCK`, the byte is
//! merely in transit: we `WSAPoll` the reader until it arrives and then
//! receive it, keeping the invariant. The wait is bounded by
//! [`DRAIN_TIMEOUT`]: the only way it can expire is if someone else consumed
//! the byte (a raw `recv` on the socket returned by `AsSocket`, which the
//! crate docs forbid). In that case we treat the byte as consumed and return
//! `Ok`, so the counter stays usable instead of failing every future read.
//! Should a byte that was merely very late arrive afterwards, it is a stray:
//! `clear_readable` always receives with a 64-byte buffer, so the next
//! drain removes strays together with the legitimate byte (self-healing; the
//! only effect in between is a spurious readiness, which pollers tolerate).
//!
//! # Socket-pair construction and connect races
//!
//! The listener is created raw with `SO_EXCLUSIVEADDRUSE` (so no other socket
//! can bind the same address with `SO_REUSEADDR` and steal our connection),
//! bound to `127.0.0.1:0`, and is non-inheritable. After connecting, we accept
//! until the accepted peer address equals our connecting socket's local
//! address. Any other connection (another local process racing a connect to
//! the ephemeral port) is closed immediately. The `(127.0.0.1:port,
//! 127.0.0.1:listener)` 4-tuple is unique while our connection exists, so a
//! match proves the accepted socket is the peer of our `writer`. The listener
//! is dropped as soon as the pair exists. An `AF_UNIX` fallback was not added:
//! it needs Windows 10 1803+, cannot be wrapped as a `mio::net::TcpStream`,
//! and TCP loopback is available everywhere.
//!
//! # Handle inheritance (`EFD_CLOEXEC`)
//!
//! Sockets are *always* non-inheritable, whether or not `EFD_CLOEXEC` was
//! requested. The counter lives in this process's memory, so an inherited
//! socket would be useless to a child (it could only observe or corrupt the
//! readiness byte), and since `std::process::Command` spawns children with
//! `bInheritHandles = TRUE`, an inheritable socket would leak into every child.
//!
//! # mio
//!
//! mio on Windows cannot register arbitrary raw sockets, so under the `mio`
//! feature each `Inner`'s own socket *is* a [`mio::net::TcpStream`]
//! (`from_std` of the duplicate) and registration is delegated to it.
//!
//! mio's Windows selector emulates edge-triggered readiness with one-shot
//! AFD polls: after it reports an event it stops watching that interest
//! until the source sees a `WouldBlock` from an I/O call made through mio
//! (`do_io`/`try_io`), which re-arms the AFD poll. Because we drain through a
//! different handle, mio would never see that `WouldBlock` by itself. So after
//! every [`Inner::read`] (success or `WouldBlock`) we call
//! `try_io(|| Err(WouldBlock))` on this `Inner`'s mio stream, which re-arms its
//! registration (a no-op if it is not registered). If the counter is still
//! non-zero at that point (semaphore mode), the re-armed AFD poll completes
//! immediately and mio reports readable again, which matches level/edge
//! expectations (a spurious event at worst). If the counter is 0, the next
//! 0 → >0 write sends a new byte, the pending AFD poll completes, and mio
//! reports a new readable event. A write racing between the drain and the
//! re-arm is not lost: an AFD poll submitted while data is present completes
//! immediately.
//!
//! Draining through another handle does not confuse mio's AFD state: AFD
//! polls observe the shared underlying socket (endpoint) and simply report
//! whatever its receive state is when they complete; mio keeps no byte
//! accounting of its own. Duplicates refer to the same endpoint, so closing
//! one `Inner` does not raise `AFD_POLL_LOCAL_CLOSE` on another's registration.
//!
//! Limitation: the re-arm only applies to the `Inner` that performed the
//! read. A registration on clone A is re-armed when A is read (as mio's
//! contract requires: read the registered source until `WouldBlock`); if only
//! clone B is ever read, A's registration stays disarmed after its first
//! event. Unlike Linux eventfd under `EPOLLET`, a write that does not take the
//! counter from 0 to > 0 produces no new event (no new byte is sent); mio's
//! contract (drain until `WouldBlock`) makes this unobservable for correct
//! users.

use std::fmt;
use std::io::{self, Read, Write};
use std::mem::{MaybeUninit, size_of};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::{
    AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, OwnedSocket, RawSocket,
};
use std::ptr;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, IN_ADDR, IN_ADDR_0, INADDR_LOOPBACK, INVALID_SOCKET, IPPROTO_TCP, POLLRDNORM,
    POLLWRNORM, SO_EXCLUSIVEADDRUSE, SOCK_STREAM, SOCKADDR, SOCKADDR_IN, SOCKET, SOCKET_ERROR,
    SOL_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSAGetLastError,
    WSAPOLLFD, WSAPoll, WSASocketW, WSAStartup, bind, listen, setsockopt,
};

use crate::counter::{Counter, Notifier};
use crate::flags::EfdFlags;

/// Upper bound for building the socket pair (connect + accept).
const PAIR_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for waiting on the in-flight byte in `clear_readable`
/// (see the module docs for why expiry is treated as success).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound for waiting for send-buffer space in `set_readable`.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// The socket type each `Inner` exposes and registers.
#[cfg(feature = "mio")]
type PollSocket = mio::net::TcpStream;
#[cfg(not(feature = "mio"))]
type PollSocket = TcpStream;

/// The connected loopback pair; the [`Notifier`] of the counter.
#[derive(Debug)]
struct SocketPair {
    /// Receives the readiness byte. Only ever read by `clear_readable`.
    reader: TcpStream,
    /// Sends the readiness byte. Only ever written by `set_readable`.
    writer: TcpStream,
}

impl Notifier for SocketPair {
    fn set_readable(&self) -> io::Result<()> {
        let deadline = Instant::now() + SEND_TIMEOUT;
        loop {
            match (&self.writer).write(&[1]) {
                Ok(1) => return Ok(()),
                Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // Cannot happen with at most a few bytes queued, but never
                // spin: wait for buffer space.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(e);
                    }
                    wait(self.writer.as_socket(), POLLWRNORM, left)?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn clear_readable(&self) -> io::Result<()> {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        // Larger than 1 so that stray bytes (see module docs) are drained too.
        let mut buf = [0u8; 64];
        loop {
            match (&self.reader).read(&mut buf) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "eventfd socket pair was closed",
                    ));
                }
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // The counter is > 0, so our byte was sent and is in transit
                // on loopback. Wait for it so the invariant holds.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        // Consumed by someone else (raw recv); see module docs.
                        return Ok(());
                    }
                    wait(self.reader.as_socket(), POLLRDNORM, left)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// State shared by all clones of one eventfd.
#[derive(Debug)]
struct Shared {
    counter: Counter,
    pair: SocketPair,
}

/// Windows eventfd: shared counter + this handle's own pollable socket.
pub(crate) struct Inner {
    shared: Arc<Shared>,
    /// This handle's own duplicate of the shared reader. Only polled
    /// (`AsSocket`, mio), never read from.
    sock: PollSocket,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("socket", &self.sock.as_raw_socket())
            .field("counter", &self.shared.counter)
            .finish()
    }
}

impl Inner {
    pub(crate) fn new(initval: u32, flags: EfdFlags) -> io::Result<Self> {
        // EFD_CLOEXEC is implied: every socket is non-inheritable.
        let (reader, writer) = socket_pair()?;
        let shared = Arc::new(Shared {
            counter: Counter::new(initval, flags),
            pair: SocketPair { reader, writer },
        });
        if shared.counter.is_readable() {
            shared.pair.set_readable()?;
        }
        Self::with_shared(shared)
    }

    fn with_shared(shared: Arc<Shared>) -> io::Result<Self> {
        let sock = shared.pair.reader.try_clone()?;
        sock.set_nonblocking(true)?;
        set_no_inherit(sock.as_socket())?;
        #[cfg(feature = "mio")]
        let sock = mio::net::TcpStream::from_std(sock);
        Ok(Self { shared, sock })
    }

    pub(crate) fn read(&self) -> io::Result<u64> {
        let result = self.shared.counter.read(&self.shared.pair);
        #[cfg(feature = "mio")]
        self.rearm();
        result
    }

    pub(crate) fn write(&self, value: u64) -> io::Result<()> {
        self.shared.counter.write(value, &self.shared.pair)
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Self::with_shared(Arc::clone(&self.shared))
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.shared.counter.set_nonblocking(nonblocking);
        Ok(())
    }

    pub(crate) fn is_nonblocking(&self) -> io::Result<bool> {
        Ok(self.shared.counter.is_nonblocking())
    }
}

impl AsSocket for Inner {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.sock.as_socket()
    }
}

#[cfg(feature = "mio")]
impl Inner {
    /// Re-arms this handle's mio registration (see module docs). mio does
    /// this whenever an I/O closure passed to `try_io` reports `WouldBlock`;
    /// it is a no-op when the socket is not registered.
    fn rearm(&self) {
        // Best effort: the only possible error is from mio's internal
        // reregister, and the read result is what the caller asked for.
        let _ = self.sock.try_io(|| Err::<(), _>(io::Error::from(io::ErrorKind::WouldBlock)));
    }

    pub(crate) fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        mio::event::Source::register(&mut self.sock, registry, token, interests)
    }

    pub(crate) fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        mio::event::Source::reregister(&mut self.sock, registry, token, interests)
    }

    pub(crate) fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        mio::event::Source::deregister(&mut self.sock, registry)
    }
}

fn last_wsa_error() -> io::Error {
    // SAFETY: WSAGetLastError has no preconditions.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// Makes sure WinSock is initialised before we call raw WinSock functions.
/// `WSAStartup` is reference counted and std itself never calls
/// `WSACleanup` before exit, so a single never-balanced call is harmless.
fn init_winsock() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let mut data = MaybeUninit::<WSADATA>::zeroed();
        // SAFETY: `data` is a valid, writable WSADATA. On failure the next
        // WinSock call fails with WSANOTINITIALISED, which we report.
        let _ = unsafe { WSAStartup(0x0202, data.as_mut_ptr()) };
    });
}

/// Clears `HANDLE_FLAG_INHERIT` on a socket (see "Handle inheritance").
fn set_no_inherit(sock: BorrowedSocket<'_>) -> io::Result<()> {
    // SAFETY: the socket is open for the duration of the borrow; a SOCKET is
    // a kernel handle on which SetHandleInformation is valid.
    let ok = unsafe {
        SetHandleInformation(sock.as_raw_socket() as HANDLE, HANDLE_FLAG_INHERIT, 0)
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// `WSAPoll`s one socket. Returns `true` if any requested event (or an
/// error/hangup) is pending.
fn wait(sock: BorrowedSocket<'_>, events: i16, timeout: Duration) -> io::Result<bool> {
    let mut fd = WSAPOLLFD { fd: sock.as_raw_socket() as SOCKET, events, revents: 0 };
    // Round up so a sub-millisecond remainder does not become a busy loop.
    let ms = timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
    // SAFETY: `fd` is a valid WSAPOLLFD array of length 1 for the call, and
    // the socket is open for the duration of the borrow.
    let n = unsafe { WSAPoll(&mut fd, 1, ms) };
    if n == SOCKET_ERROR { Err(last_wsa_error()) } else { Ok(n > 0) }
}

/// A non-inheritable listener on `127.0.0.1:0` with `SO_EXCLUSIVEADDRUSE`.
fn loopback_listener() -> io::Result<TcpListener> {
    init_winsock();
    // SAFETY: plain FFI call with constant arguments and a null protocol info.
    let raw = unsafe {
        WSASocketW(
            i32::from(AF_INET),
            SOCK_STREAM,
            IPPROTO_TCP,
            ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if raw == INVALID_SOCKET {
        return Err(last_wsa_error());
    }
    // SAFETY: `raw` is a freshly created socket that nothing else owns.
    let owned = unsafe { OwnedSocket::from_raw_socket(raw as RawSocket) };

    let one: i32 = 1;
    // SAFETY: `optval` points to a live i32 and `optlen` is its size.
    let rc = unsafe {
        setsockopt(
            raw,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            ptr::from_ref(&one).cast(),
            size_of::<i32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(last_wsa_error());
    }

    let addr = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: 0,
        sin_addr: IN_ADDR { S_un: IN_ADDR_0 { S_addr: INADDR_LOOPBACK.to_be() } },
        sin_zero: [0; 8],
    };
    // SAFETY: `addr` is a valid SOCKADDR_IN and `namelen` is its size.
    let rc = unsafe {
        bind(raw, ptr::from_ref(&addr).cast::<SOCKADDR>(), size_of::<SOCKADDR_IN>() as i32)
    };
    if rc == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    // SAFETY: `raw` is a bound stream socket owned by `owned`.
    if unsafe { listen(raw, 8) } == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(TcpListener::from(owned))
}

/// Builds a connected, verified, non-blocking loopback pair `(reader, writer)`.
fn socket_pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = loopback_listener()?;
    let listen_addr = listener.local_addr()?;
    let writer = TcpStream::connect_timeout(&listen_addr, PAIR_TIMEOUT)?;
    let expected: SocketAddr = writer.local_addr()?;
    if writer.peer_addr()? != listen_addr {
        return Err(io::Error::other("eventfd socket pair: unexpected peer"));
    }

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + PAIR_TIMEOUT;
    let reader = loop {
        match listener.accept() {
            Ok((sock, peer)) if peer == expected => break sock,
            // Someone else raced a connect to our port: close it, keep going.
            Ok((intruder, _)) => drop(intruder),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "eventfd socket pair: connection never accepted",
                    ));
                }
                wait(listener.as_socket(), POLLRDNORM, left)?;
            }
            Err(e) => return Err(e),
        }
    };
    drop(listener);

    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    writer.set_nodelay(true)?;
    // std's accept does not set the flag itself; connect already did.
    set_no_inherit(reader.as_socket())?;
    set_no_inherit(writer.as_socket())?;
    Ok((reader, writer))
}

#[cfg(test)]
mod tests {
    use crate::{EfdFlags, GkEventFd};
    use std::io;
    use std::os::windows::io::{AsRawSocket, AsSocket};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use windows_sys::Win32::Networking::WinSock::POLLRDNORM;

    /// Readable within `ms` milliseconds?
    fn readable_within(efd: &GkEventFd, ms: u64) -> bool {
        super::wait(efd.as_socket(), POLLRDNORM, Duration::from_millis(ms)).unwrap()
    }

    /// Readable now or soon (the byte may still be in transit on loopback).
    fn readable(efd: &GkEventFd) -> bool {
        readable_within(efd, 2000)
    }

    /// Not readable. After a drain nothing is in flight, so no wait is needed.
    fn idle(efd: &GkEventFd) -> bool {
        !readable_within(efd, 0)
    }

    #[test]
    fn readiness_tracks_counter() {
        let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
        assert!(idle(&efd));
        efd.write(1).unwrap();
        assert!(readable(&efd));
        efd.write(2).unwrap();
        assert!(readable(&efd));
        assert_eq!(efd.read().unwrap(), 3);
        assert!(idle(&efd));
        assert_eq!(efd.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(idle(&efd));
        for i in 1..=200 {
            efd.write(i).unwrap();
            assert!(readable(&efd));
            assert_eq!(efd.read().unwrap(), i);
            assert!(idle(&efd));
        }
    }

    #[test]
    fn semaphore_readiness() {
        let efd = GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE).unwrap();
        efd.write(3).unwrap();
        for _ in 0..3 {
            assert!(readable(&efd));
            assert_eq!(efd.read().unwrap(), 1);
        }
        assert!(idle(&efd));
        assert_eq!(efd.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn initval_readable_immediately() {
        let efd = GkEventFd::new(7, EfdFlags::NONBLOCK).unwrap();
        assert!(readable(&efd));
        assert_eq!(efd.read().unwrap(), 7);
        assert!(idle(&efd));
    }

    #[test]
    fn overflow_does_not_disturb_readiness() {
        let efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
        efd.write(crate::EFD_MAX).unwrap();
        assert_eq!(efd.write(1).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(readable(&efd));
        assert_eq!(efd.read().unwrap(), crate::EFD_MAX);
        assert!(idle(&efd));
    }

    #[test]
    fn blocking_read_woken_by_other_thread() {
        let efd = Arc::new(GkEventFd::new(0, EfdFlags::NONE).unwrap());
        let e2 = efd.clone();
        let t = thread::spawn(move || e2.read().unwrap());
        thread::sleep(Duration::from_millis(50));
        efd.write(5).unwrap();
        assert_eq!(t.join().unwrap(), 5);
        assert!(idle(&efd));
    }

    #[test]
    fn clone_shares_counter_with_own_socket() {
        let a = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
        let b = a.try_clone().unwrap();
        assert_ne!(a.as_raw_socket(), b.as_raw_socket());
        a.write(4).unwrap();
        assert!(readable(&a));
        assert!(readable(&b));
        assert_eq!(b.read().unwrap(), 4);
        assert!(idle(&a));
        assert!(idle(&b));
        // Non-blocking mode is shared, like O_NONBLOCK on a dup'ed fd.
        b.set_nonblocking(false).unwrap();
        assert!(!a.is_nonblocking().unwrap());
        // Dropping the original keeps the clone working.
        drop(a);
        b.write(1).unwrap();
        assert!(readable(&b));
        assert_eq!(b.read().unwrap(), 1);
    }

    #[test]
    fn stress_keeps_invariant() {
        let efd = Arc::new(GkEventFd::new(0, EfdFlags::SEMAPHORE).unwrap());
        let producers: Vec<_> = (0..4)
            .map(|_| {
                let e = efd.try_clone().unwrap();
                thread::spawn(move || (0..500).for_each(|_| e.write(1).unwrap()))
            })
            .collect();
        let consumers: Vec<_> = (0..4)
            .map(|_| {
                let e = efd.try_clone().unwrap();
                thread::spawn(move || (0..500).map(|_| e.read().unwrap()).sum::<u64>())
            })
            .collect();
        producers.into_iter().for_each(|t| t.join().unwrap());
        let total: u64 = consumers.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(total, 2000);
        assert!(idle(&efd));
        efd.write(1).unwrap();
        assert!(readable(&efd));
        assert_eq!(efd.read().unwrap(), 1);
        assert!(idle(&efd));
    }

    #[cfg(feature = "mio")]
    mod mio_tests {
        use super::*;
        use mio::{Events, Interest, Poll, Token};

        fn tokens(poll: &mut Poll, events: &mut Events, ms: u64) -> Vec<Token> {
            poll.poll(events, Some(Duration::from_millis(ms))).unwrap();
            events.iter().filter(|e| e.is_readable()).map(|e| e.token()).collect()
        }

        #[test]
        fn readable_event_after_write_and_rewrite() {
            let mut poll = Poll::new().unwrap();
            let mut events = Events::with_capacity(8);
            let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
            poll.registry().register(&mut efd, Token(1), Interest::READABLE).unwrap();
            assert!(tokens(&mut poll, &mut events, 50).is_empty());

            for round in 1..=5u64 {
                efd.write(round).unwrap();
                assert_eq!(tokens(&mut poll, &mut events, 2000), vec![Token(1)]);
                assert_eq!(efd.read().unwrap(), round);
                assert_eq!(efd.read().unwrap_err().kind(), io::ErrorKind::WouldBlock);
                assert!(tokens(&mut poll, &mut events, 50).is_empty());
            }
            poll.registry().deregister(&mut efd).unwrap();
            efd.write(1).unwrap();
            assert!(tokens(&mut poll, &mut events, 50).is_empty());
        }

        #[test]
        fn clones_register_independently() {
            let mut poll = Poll::new().unwrap();
            let mut events = Events::with_capacity(8);
            let mut a = GkEventFd::new(0, EfdFlags::NONBLOCK).unwrap();
            let mut b = a.try_clone().unwrap();
            poll.registry().register(&mut a, Token(1), Interest::READABLE).unwrap();
            poll.registry().register(&mut b, Token(2), Interest::READABLE).unwrap();
            a.write(1).unwrap();
            let mut got = tokens(&mut poll, &mut events, 2000);
            // Both registrations may not complete in the same poll call.
            if got.len() < 2 {
                got.extend(tokens(&mut poll, &mut events, 500));
            }
            got.sort();
            assert_eq!(got, vec![Token(1), Token(2)]);
        }
    }
}

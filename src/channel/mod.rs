//! A one-way channel between threads or processes, through shared memory:
//! [`GkChannel`] creates a [`GkSender`] and a [`GkReceiver`].
//!
//! A channel has two independent lanes:
//!
//! * **Messages** ([`GkSender::send`], [`GkReceiver::recv`]): atomic
//!   messages of up to [`ChannelOptions::max_message`] bytes (including
//!   zero), delivered whole and in order. Blocking or non-blocking.
//! * **Bulk transfers** ([`GkSender::send_bulk`], [`GkSender::bulk_writer`],
//!   [`GkReceiver::recv_bulk_vec`], [`GkReceiver::bulk_reader`]): data of
//!   any size, sliced into chunks of [`ChannelOptions::chunk`] bytes and
//!   reassembled, or streamed with `io::Write` / `io::Read`. Transfers are
//!   delivered in order, each identified by an id.
//!
//! Every message and transfer also carries a 16-bit *kind* chosen by the
//! application to say what the payload is ([`GkSender::send_kind`],
//! [`GkReceiver::recv_kind`], [`GkSender::bulk_writer_kind`],
//! [`BulkReader::kind`]); the plain calls use [`KIND_UNSPECIFIED`] (0). It
//! costs nothing: it lives in the record header next to the length.
//!
//! The lanes are not ordered relative to each other: a message sent while a
//! transfer is in progress is delivered at once, without waiting behind the
//! transfer. Mention a transfer's id in a message to link the two.
//!
//! While both sides are busy, a message or chunk costs a few atomic
//! operations and one copy, with no system call. Two of this crate's
//! eventfds are the doorbells ("data ready", "space ready"), rung only when
//! the other side sleeps; blocking calls spin for a few microseconds first.
//!
//! * **One sender, one receiver.** The ends are not `Clone`. Each can be moved
//!   to another thread, or handed to another process (see "Sharing").
//! * **Blocking and non-blocking.** Calls block by default; after
//!   `set_nonblocking(true)` they fail with [`io::ErrorKind::WouldBlock`]
//!   instead (bulk writes may then be partial, like any `io::Write`).
//! * **Polling.** A [`GkReceiver`] is readable (`AsFd`/`AsSocket`, mio) when
//!   data may be waiting in a lane it drained to `WouldBlock`; a
//!   [`GkSender`] is readable when space may have been freed in a lane that
//!   was full. Drain every lane you use until `WouldBlock`, then wait.
//! * **Closing.** Dropping the sender lets the receiver drain what is left,
//!   then calls fail with [`io::ErrorKind::BrokenPipe`]. Dropping the
//!   receiver makes the sender's calls fail with `BrokenPipe`. A peer
//!   *process* that dies is detected by blocking calls (within about a
//!   quarter second) and treated the same way. A bulk transfer abandoned by
//!   the sender fails on the receiving side with
//!   [`io::ErrorKind::ConnectionAborted`]; partial data is never mistaken
//!   for a complete transfer.
//! * **Sharing.** Hand an end to another process with
//!   [`GkProcess::send`](crate::GkProcess::send) (the other side calls
//!   `recv_as::<GkReceiver>()` / `recv_as::<GkSender>()`), or on unix with
//!   `send_to`/`recv_from` over a `UnixStream`. After `fork()` the process
//!   that keeps using an end calls [`claim`](GkReceiver::claim) on it, and the
//!   other process [`detach`](GkReceiver::detach)es its copy. Each end
//!   records its owning process; using an end you don't own fails with
//!   [`io::ErrorKind::PermissionDenied`].
//!
//! Memory: both rings live in one shared memory object. On unix, memory is
//! only allocated as the rings are first written; on Windows the whole size
//! counts against the commit limit when the channel is created, which is
//! why the Windows defaults are smaller.
//!
//! ```
//! use adaptif_gatotkaca_eventfd::GkChannel;
//!
//! let (mut tx, mut rx) = GkChannel::new()?;
//! let big = vec![7u8; 3 << 20]; // larger than a chunk: sliced and reassembled
//! std::thread::spawn(move || {
//!     tx.send(b"hello").unwrap();
//!     let id = tx.send_bulk(&big).unwrap();
//!     tx.send(&id.to_le_bytes()).unwrap();
//! });
//! let mut buf = [0u8; 64];
//! assert_eq!(rx.recv(&mut buf)?, 5);
//! let mut data = Vec::new();
//! let id = rx.recv_bulk_vec(&mut data, 16 << 20)?;
//! assert_eq!(data.len(), 3 << 20);
//! assert_eq!(rx.recv(&mut buf)?, 8);
//! assert_eq!(u64::from_le_bytes(buf[..8].try_into().unwrap()), id);
//! # Ok::<(), std::io::Error>(())
//! ```

mod bulk;
mod lane;
mod share;

pub use bulk::{BulkReader, BulkWriter};

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::time::Duration;

use crate::sys::{SharedRegion, current_pid, process_alive};
use crate::{EfdFlags, GkEventFd};
use lane::{Lane, Layout, Line, Sizes};

/// The kind of messages and transfers sent without one: "unspecified".
pub const KIND_UNSPECIFIED: u16 = 0;

/// How long a blocking call sleeps before checking that the peer is alive.
const LIVENESS_PERIOD: Duration = Duration::from_millis(250);

/// Sizes of a channel. See [`GkChannel::with_options`].
///
/// Defaults: message ring 1 MiB, bulk ring 4 MiB, 64 KiB messages and
/// chunks; on Windows (where the memory is committed up front) a 256 KiB
/// message ring and a 1 MiB bulk ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelOptions {
    /// Message ring size in bytes: a power of two, at least 64.
    pub capacity: usize,
    /// Largest message in bytes; at most `capacity / 2 - 8`.
    pub max_message: usize,
    /// Bulk ring size in bytes: a power of two, at least 64.
    pub bulk_capacity: usize,
    /// Largest chunk of a bulk transfer in bytes; at most
    /// `bulk_capacity / 2 - 32`. Transfers themselves have no size limit.
    pub chunk: usize,
}

impl ChannelOptions {
    /// Message ring: 1 MiB (256 KiB on Windows).
    pub const DEFAULT_CAPACITY: usize = if cfg!(windows) { 256 << 10 } else { 1 << 20 };
    /// Largest message: 64 KiB.
    pub const DEFAULT_MAX_MESSAGE: usize = 64 << 10;
    /// Bulk ring: 4 MiB (1 MiB on Windows).
    pub const DEFAULT_BULK_CAPACITY: usize = if cfg!(windows) { 1 << 20 } else { 4 << 20 };
    /// Bulk chunk: 64 KiB.
    pub const DEFAULT_CHUNK: usize = 64 << 10;

    fn sizes(&self) -> Sizes {
        Sizes {
            msg_capacity: self.capacity as u64,
            max_message: self.max_message as u64,
            bulk_capacity: self.bulk_capacity as u64,
            chunk: self.chunk as u64,
        }
    }
}

impl Default for ChannelOptions {
    fn default() -> Self {
        Self {
            capacity: Self::DEFAULT_CAPACITY,
            max_message: Self::DEFAULT_MAX_MESSAGE,
            bulk_capacity: Self::DEFAULT_BULK_CAPACITY,
            chunk: Self::DEFAULT_CHUNK,
        }
    }
}

/// The two lanes, as indices.
const MSG: usize = 0;
const BULK: usize = 1;

/// A channel's shared memory, mapped in this process.
#[derive(Debug)]
struct Ring {
    region: SharedRegion,
    layout: Layout,
}

impl Ring {
    fn create(opts: ChannelOptions) -> io::Result<Self> {
        let sizes = opts.sizes();
        if !sizes.valid() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "channel sizes: rings must be powers of two >= 64, max_message <= \
                 capacity / 2 - 8 and chunk <= bulk_capacity / 2 - 32",
            ));
        }
        let len = usize::try_from(sizes.region_len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "channel too large"))?;
        let region = SharedRegion::create(len)?;
        // SAFETY: fresh, zeroed, page-aligned and `region_len` long.
        unsafe { Layout::init(region.ptr(), sizes, current_pid()) };
        Ok(Self::with_region(region))
    }

    /// For a region created by `create` or validated with
    /// `Layout::accepts`.
    fn with_region(region: SharedRegion) -> Self {
        // SAFETY: initialised and validated; mapped as long as `region`.
        let layout = unsafe { Layout::new(region.ptr()) };
        Self { region, layout }
    }

    fn lane(&self, lane: usize) -> &Lane {
        if lane == MSG {
            &self.layout.msg
        } else {
            &self.layout.bulk
        }
    }
}

/// Which end of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Sender,
    Receiver,
}

/// What both ends have: the ring, the two doorbells and the wait state.
#[derive(Debug)]
struct End {
    ring: Arc<Ring>,
    /// Rung by the sender when the sleeping receiver should look again.
    data_ready: GkEventFd,
    /// Rung by the receiver when the sleeping sender should look again.
    space_ready: GkEventFd,
    side: Side,
    nonblocking: bool,
    /// Don't close the channel on drop (see `detach`).
    detached: bool,
    /// Per lane: we returned `WouldBlock` with our waiting flag left set,
    /// so the peer will ring our doorbell for that lane.
    armed: [bool; 2],
}

impl End {
    fn new(ring: Arc<Ring>, data_ready: GkEventFd, space_ready: GkEventFd, side: Side) -> Self {
        Self {
            ring,
            data_ready,
            space_ready,
            side,
            nonblocking: false,
            detached: false,
            armed: [false; 2],
        }
    }

    fn lane(&self, lane: usize) -> &Lane {
        self.ring.lane(lane)
    }

    fn owner(&self) -> &AtomicU32 {
        let meta = self.ring.layout.meta();
        match self.side {
            Side::Sender => &meta.sender_pid,
            Side::Receiver => &meta.receiver_pid,
        }
    }

    fn peer_owner(&self) -> &AtomicU32 {
        let meta = self.ring.layout.meta();
        match self.side {
            Side::Sender => &meta.receiver_pid,
            Side::Receiver => &meta.sender_pid,
        }
    }

    fn peer_closed(&self) -> bool {
        let meta = self.ring.layout.meta();
        let flag = match self.side {
            Side::Sender => &meta.receiver_closed,
            Side::Receiver => &meta.sender_closed,
        };
        flag.load(Ordering::Acquire) != 0
    }

    /// The peer closed its end, or its owning process died.
    fn peer_gone(&self) -> bool {
        if self.peer_closed() {
            return true;
        }
        let pid = self.peer_owner().load(Ordering::Relaxed);
        pid != 0 && !process_alive(pid)
    }

    fn check_owner(&self) -> io::Result<()> {
        if self.owner().load(Ordering::Relaxed) == current_pid() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "this channel end belongs to another process (call claim() after fork)",
            ))
        }
    }

    /// The doorbell this end waits on (and registers for polling).
    fn bell(&self) -> &GkEventFd {
        match self.side {
            Side::Sender => &self.space_ready,
            Side::Receiver => &self.data_ready,
        }
    }

    #[cfg_attr(not(feature = "mio"), allow(dead_code))]
    fn bell_mut(&mut self) -> &mut GkEventFd {
        match self.side {
            Side::Sender => &mut self.space_ready,
            Side::Receiver => &mut self.data_ready,
        }
    }

    /// The doorbell this end rings for its peer.
    fn peer_bell(&self) -> &GkEventFd {
        match self.side {
            Side::Sender => &self.data_ready,
            Side::Receiver => &self.space_ready,
        }
    }

    /// My waiting line in `lane`, and the peer's.
    fn lines(&self, lane: usize) -> (&Line, &Line) {
        let l = self.lane(lane);
        match self.side {
            Side::Sender => (l.tail_line(), l.head_line()),
            Side::Receiver => (l.head_line(), l.tail_line()),
        }
    }

    /// After making progress in `lane`: wake the peer if it sleeps on it.
    fn wake_peer(&self, lane: usize) {
        fence(Ordering::SeqCst);
        if self.lines(lane).1.waiting.swap(0, Ordering::SeqCst) != 0 {
            // Non-blocking doorbell; it cannot overflow in practice.
            let _ = self.peer_bell().write(1);
        }
    }

    /// Called when `lane` has nothing for us (`ready()` false): announces
    /// that we wait, then returns `WouldBlock` (`nonblocking`) or sleeps
    /// until the doorbell rings. `Ok(())` means "look again".
    fn wait(&mut self, lane: usize, nonblocking: bool, ready: impl Fn() -> bool) -> io::Result<()> {
        // Blocking: spin briefly first, so a quick reply costs no system
        // call on either side (we never announce that we sleep).
        if !nonblocking {
            let backoff = crossbeam_utils::Backoff::new();
            while !backoff.is_completed() {
                if ready() {
                    return Ok(());
                }
                backoff.snooze();
            }
        }
        // Clear old rings, so a level-triggered poller doesn't spin...
        let _ = self.bell().read();
        // ...but if that may have swallowed a ring meant for the other lane
        // (it is armed and the peer has since cleared its flag), ring again
        // so the poller still wakes for it.
        let other = 1 - lane;
        if self.armed[other] && self.lines(other).0.waiting.load(Ordering::SeqCst) == 0 {
            self.armed[other] = false;
            let _ = self.bell().write(1);
        }
        let mine = self.lines(lane).0;
        mine.waiting.store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if ready() {
            mine.waiting.store(0, Ordering::Relaxed);
            self.armed[lane] = false;
            return Ok(());
        }
        if self.peer_closed() {
            return Err(broken());
        }
        if nonblocking {
            self.armed[lane] = true;
            return Err(crate::error::would_block());
        }
        loop {
            if wait_readable(self.bell(), LIVENESS_PERIOD)? || ready() {
                return Ok(());
            }
            if self.peer_gone() {
                return if ready() { Ok(()) } else { Err(broken()) };
            }
        }
    }

    fn close(&mut self) {
        if self.detached || self.owner().load(Ordering::Relaxed) != current_pid() {
            return;
        }
        let meta = self.ring.layout.meta();
        let flag = match self.side {
            Side::Sender => &meta.sender_closed,
            Side::Receiver => &meta.receiver_closed,
        };
        flag.store(1, Ordering::Release);
        fence(Ordering::SeqCst);
        let _ = self.peer_bell().write(1);
    }
}

impl Drop for End {
    fn drop(&mut self) {
        self.close();
    }
}

fn broken() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the other end of the channel is gone",
    )
}

/// Waits up to `timeout` for `efd` to become readable.
fn wait_readable(efd: &GkEventFd, timeout: Duration) -> io::Result<bool> {
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    #[cfg(unix)]
    {
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(efd),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(err)
            };
        }
        Ok(n > 0)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{
            POLLRDNORM, SOCKET, SOCKET_ERROR, WSAGetLastError, WSAPOLLFD, WSAPoll,
        };
        let mut fd = WSAPOLLFD {
            fd: std::os::windows::io::AsRawSocket::as_raw_socket(efd) as SOCKET,
            events: POLLRDNORM,
            revents: 0,
        };
        // SAFETY: one valid WSAPOLLFD; the socket is open.
        let n = unsafe { WSAPoll(&mut fd, 1, ms) };
        if n == SOCKET_ERROR {
            // SAFETY: no preconditions.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(n > 0)
    }
}

/// Creates channels. See the [module docs](self).
#[derive(Debug)]
pub struct GkChannel(());

impl GkChannel {
    /// A channel with the default sizes (see [`ChannelOptions`]).
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> io::Result<(GkSender, GkReceiver)> {
        Self::with_options(ChannelOptions::default())
    }

    /// A channel with the given sizes. Fails with
    /// [`io::ErrorKind::InvalidInput`] if a ring is not a power of two of at
    /// least 64 bytes, `max_message > capacity / 2 - 8`, or
    /// `chunk > bulk_capacity / 2 - 32`.
    pub fn with_options(opts: ChannelOptions) -> io::Result<(GkSender, GkReceiver)> {
        let ring = Arc::new(Ring::create(opts)?);
        let bell = || GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::CLOEXEC);
        let (data_ready, space_ready) = (bell()?, bell()?);
        let sender = End::new(
            Arc::clone(&ring),
            data_ready.try_clone()?,
            space_ready.try_clone()?,
            Side::Sender,
        );
        let receiver = End::new(ring, data_ready, space_ready, Side::Receiver);
        Ok((GkSender::from_end(sender), GkReceiver::from_end(receiver)))
    }
}

/// The sending end of a channel. See the [module docs](self).
#[derive(Debug)]
pub struct GkSender {
    end: End,
    /// Our copies of the published tails (we are their only writer).
    msg_tail: u64,
    bulk_tail: u64,
    bulk: bulk::TxState,
}

/// The receiving end of a channel. See the [module docs](self).
#[derive(Debug)]
pub struct GkReceiver {
    end: End,
    /// Our copies of the consumed heads (we are their only writer).
    msg_head: u64,
    bulk_head: u64,
    bulk: bulk::RxState,
}

/// Space reserved in the message ring by [`GkSender::reserve`]: write the
/// message into it (it derefs to `&mut [u8]` of the reserved length), then
/// [`commit`](Self::commit). Dropping it without committing sends nothing.
#[derive(Debug)]
pub struct SendSlot<'a> {
    sender: &'a mut GkSender,
    pad: u64,
    idx: u64,
    len: usize,
    kind: u16,
}

impl GkSender {
    fn from_end(end: End) -> Self {
        let msg_tail = end.lane(MSG).tail_line().pos.load(Ordering::Acquire);
        let bulk_tail = end.lane(BULK).tail_line().pos.load(Ordering::Acquire);
        Self {
            end,
            msg_tail,
            bulk_tail,
            bulk: bulk::TxState::default(),
        }
    }

    /// Sends one message (copied into the message ring). Blocks while the
    /// ring is full, or fails with [`io::ErrorKind::WouldBlock`] in
    /// non-blocking mode. Fails with [`io::ErrorKind::InvalidInput`] if `msg`
    /// is longer than [`max_message`](Self::max_message) (use
    /// [`send_bulk`](Self::send_bulk) for large data), and
    /// [`io::ErrorKind::BrokenPipe`] once the receiver is gone.
    pub fn send(&mut self, msg: &[u8]) -> io::Result<()> {
        self.send_kind(KIND_UNSPECIFIED, msg)
    }

    /// Like [`send`](Self::send), tagging the message with `kind` (the
    /// application's payload type; the receiver gets it from
    /// [`GkReceiver::recv_kind`]).
    pub fn send_kind(&mut self, kind: u16, msg: &[u8]) -> io::Result<()> {
        let mut slot = self.reserve_kind(kind, msg.len())?;
        slot.copy_from_slice(msg);
        slot.commit(msg.len());
        Ok(())
    }

    /// Reserves `len` bytes for a message written in place (no extra copy).
    /// Waits for space like [`send`](Self::send).
    pub fn reserve(&mut self, len: usize) -> io::Result<SendSlot<'_>> {
        self.reserve_kind(KIND_UNSPECIFIED, len)
    }

    /// Like [`reserve`](Self::reserve), for a message of the given `kind`.
    pub fn reserve_kind(&mut self, kind: u16, len: usize) -> io::Result<SendSlot<'_>> {
        self.end.check_owner()?;
        if len as u64 > self.end.lane(MSG).max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "message larger than max_message (use send_bulk)",
            ));
        }
        loop {
            if self.end.peer_closed() {
                return Err(broken());
            }
            if let Some((pad, idx)) = self.end.lane(MSG).reserve(self.msg_tail, len as u64) {
                return Ok(SendSlot {
                    sender: self,
                    pad,
                    idx,
                    len,
                    kind,
                });
            }
            let (lane, tail, nb) = (*self.end.lane(MSG), self.msg_tail, self.end.nonblocking);
            self.end
                .wait(MSG, nb, || lane.reserve(tail, len as u64).is_some())?;
        }
    }

    /// Message ring size in bytes.
    pub fn capacity(&self) -> usize {
        self.end.lane(MSG).cap as usize
    }

    /// Largest message in bytes.
    pub fn max_message(&self) -> usize {
        self.end.lane(MSG).max as usize
    }

    /// Bulk ring size in bytes.
    pub fn bulk_capacity(&self) -> usize {
        self.end.lane(BULK).cap as usize
    }

    /// Largest chunk of a bulk transfer in bytes.
    pub fn chunk(&self) -> usize {
        (self.end.lane(BULK).max - lane::BULK_HDR) as usize
    }

    /// Switches between blocking (default) and non-blocking operation.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.end.nonblocking = nonblocking;
    }

    /// True in non-blocking mode.
    pub fn is_nonblocking(&self) -> bool {
        self.end.nonblocking
    }

    /// Makes this process the owner of this end (after `fork`).
    pub fn claim(&mut self) {
        self.end.owner().store(current_pid(), Ordering::Relaxed);
        self.msg_tail = self.end.lane(MSG).tail_line().pos.load(Ordering::Acquire);
        self.bulk_tail = self.end.lane(BULK).tail_line().pos.load(Ordering::Acquire);
    }

    /// Drops this process's copy without closing the channel (e.g. the
    /// copy a process keeps after `fork` but doesn't use).
    pub fn detach(mut self) {
        self.end.detached = true;
        self.bulk = bulk::TxState::default();
    }
}

impl SendSlot<'_> {
    /// Publishes the first `n` bytes (at most the reserved length) as one
    /// message.
    ///
    /// # Panics
    /// If `n` is larger than the reserved length.
    pub fn commit(self, n: usize) {
        assert!(
            n <= self.len,
            "commit({n}) exceeds the reserved {} bytes",
            self.len
        );
        let s = self.sender;
        // SAFETY: reserved by `reserve(s.msg_tail, self.len)` and `n <= len`.
        s.msg_tail = unsafe {
            s.end
                .lane(MSG)
                .publish(s.msg_tail, self.pad, self.idx, n as u64, 0, self.kind)
        };
        s.end.wake_peer(MSG);
    }
}

impl std::ops::Deref for SendSlot<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: reserved space owned by this sender until published.
        unsafe { std::slice::from_raw_parts(self.sender.end.lane(MSG).payload(self.idx), self.len) }
    }
}

impl std::ops::DerefMut for SendSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; nobody else reads it before it is published.
        unsafe {
            std::slice::from_raw_parts_mut(self.sender.end.lane(MSG).payload(self.idx), self.len)
        }
    }
}

impl GkReceiver {
    fn from_end(end: End) -> Self {
        let msg_head = end.lane(MSG).head_line().pos.load(Ordering::Acquire);
        let bulk_head = end.lane(BULK).head_line().pos.load(Ordering::Acquire);
        Self {
            end,
            msg_head,
            bulk_head,
            bulk: bulk::RxState::default(),
        }
    }

    /// Waits for the next message.
    fn next(&mut self) -> io::Result<lane::Rec> {
        self.end.check_owner()?;
        loop {
            if let Some(rec) = self.end.lane(MSG).peek(self.msg_head)? {
                return Ok(rec);
            }
            let (lane, head, nb) = (*self.end.lane(MSG), self.msg_head, self.end.nonblocking);
            self.end
                .wait(MSG, nb, || !matches!(lane.peek(head), Ok(None)))?;
        }
    }

    /// Frees the space of the message just read.
    fn consume(&mut self, next: u64) {
        self.msg_head = next;
        self.end.lane(MSG).release(next);
        self.end.wake_peer(MSG);
    }

    /// Receives one message into `buf` and returns its length (0 for a
    /// zero-length message). Blocks while the ring is empty, or fails with
    /// [`io::ErrorKind::WouldBlock`] in non-blocking mode. Fails with
    /// [`io::ErrorKind::InvalidInput`] if `buf` is too small (the message
    /// stays queued; see [`next_len`](Self::next_len)), and
    /// [`io::ErrorKind::BrokenPipe`] once the sender is gone and every
    /// message was received.
    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv_kind(buf).map(|(_, len)| len)
    }

    /// Like [`recv`](Self::recv), also returning the message's kind
    /// (`0` = [`KIND_UNSPECIFIED`]): `(kind, len)`.
    pub fn recv_kind(&mut self, buf: &mut [u8]) -> io::Result<(u16, usize)> {
        let rec = self.next()?;
        if rec.len > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer smaller than the next message",
            ));
        }
        // SAFETY: `rec` points to `len` published bytes we own until consumed.
        buf[..rec.len].copy_from_slice(unsafe { std::slice::from_raw_parts(rec.ptr, rec.len) });
        self.consume(rec.next);
        Ok((rec.kind, rec.len))
    }

    /// Like [`recv`](Self::recv), replacing the contents of `out` with the
    /// message (any length).
    pub fn recv_vec(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        self.recv_vec_kind(out).map(|(_, len)| len)
    }

    /// Like [`recv_vec`](Self::recv_vec), also returning the kind:
    /// `(kind, len)`.
    pub fn recv_vec_kind(&mut self, out: &mut Vec<u8>) -> io::Result<(u16, usize)> {
        self.recv_with_kind(|kind, msg| {
            out.clear();
            out.extend_from_slice(msg);
            (kind, msg.len())
        })
    }

    /// Like [`recv`](Self::recv), but lends the message in place (no copy)
    /// to `f`; its space is freed when `f` returns.
    pub fn recv_with<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> io::Result<R> {
        self.recv_with_kind(|_, msg| f(msg))
    }

    /// Like [`recv_with`](Self::recv_with), also passing the kind to `f`.
    pub fn recv_with_kind<R>(&mut self, f: impl FnOnce(u16, &[u8]) -> R) -> io::Result<R> {
        let rec = self.next()?;
        // SAFETY: as in `recv`; the sender does not reuse it before `consume`.
        let r = f(rec.kind, unsafe {
            std::slice::from_raw_parts(rec.ptr, rec.len)
        });
        self.consume(rec.next);
        Ok(r)
    }

    /// Length of the next message without receiving it or waiting: `None`
    /// if the message ring is empty. Fails with
    /// [`io::ErrorKind::BrokenPipe`] if it is empty and the sender is gone.
    pub fn next_len(&mut self) -> io::Result<Option<usize>> {
        Ok(self.next_kind()?.map(|(_, len)| len))
    }

    /// Kind and length of the next message without receiving it or
    /// waiting, e.g. to pick a handler: `Some((kind, len))`, or `None` if the
    /// message ring is empty. Fails like [`next_len`](Self::next_len).
    pub fn next_kind(&mut self) -> io::Result<Option<(u16, usize)>> {
        self.end.check_owner()?;
        let closed = self.end.peer_closed();
        match self.end.lane(MSG).peek(self.msg_head)? {
            Some(rec) => Ok(Some((rec.kind, rec.len))),
            None if closed => Err(broken()),
            None => Ok(None),
        }
    }

    /// Message ring size in bytes.
    pub fn capacity(&self) -> usize {
        self.end.lane(MSG).cap as usize
    }

    /// Largest message in bytes.
    pub fn max_message(&self) -> usize {
        self.end.lane(MSG).max as usize
    }

    /// Bulk ring size in bytes.
    pub fn bulk_capacity(&self) -> usize {
        self.end.lane(BULK).cap as usize
    }

    /// Largest chunk of a bulk transfer in bytes.
    pub fn chunk(&self) -> usize {
        (self.end.lane(BULK).max - lane::BULK_HDR) as usize
    }

    /// Switches between blocking (default) and non-blocking operation.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.end.nonblocking = nonblocking;
    }

    /// True in non-blocking mode.
    pub fn is_nonblocking(&self) -> bool {
        self.end.nonblocking
    }

    /// Makes this process the owner of this end (after `fork`).
    pub fn claim(&mut self) {
        self.end.owner().store(current_pid(), Ordering::Relaxed);
        self.msg_head = self.end.lane(MSG).head_line().pos.load(Ordering::Acquire);
        self.bulk_head = self.end.lane(BULK).head_line().pos.load(Ordering::Acquire);
        self.bulk = bulk::RxState::default();
    }

    /// Drops this process's copy without closing the channel (e.g. the
    /// copy a process keeps after `fork` but doesn't use).
    pub fn detach(mut self) {
        self.end.detached = true;
    }
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod unix_impls {
    use super::{GkReceiver, GkSender};
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

    /// Readable when data may be waiting.
    impl AsFd for GkReceiver {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.end.bell().as_fd()
        }
    }

    impl AsRawFd for GkReceiver {
        fn as_raw_fd(&self) -> RawFd {
            self.end.bell().as_raw_fd()
        }
    }

    /// Readable when space may have been freed.
    impl AsFd for GkSender {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.end.bell().as_fd()
        }
    }

    impl AsRawFd for GkSender {
        fn as_raw_fd(&self) -> RawFd {
            self.end.bell().as_raw_fd()
        }
    }
}

#[cfg(windows)]
mod windows_impls {
    use super::{GkReceiver, GkSender};
    use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};

    /// Readable when data may be waiting.
    impl AsSocket for GkReceiver {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            self.end.bell().as_socket()
        }
    }

    impl AsRawSocket for GkReceiver {
        fn as_raw_socket(&self) -> RawSocket {
            self.end.bell().as_raw_socket()
        }
    }

    /// Readable when space may have been freed.
    impl AsSocket for GkSender {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            self.end.bell().as_socket()
        }
    }

    impl AsRawSocket for GkSender {
        fn as_raw_socket(&self) -> RawSocket {
            self.end.bell().as_raw_socket()
        }
    }
}

#[cfg(feature = "mio")]
mod mio_impls {
    use super::{GkReceiver, GkSender};
    use mio::event::Source;
    use mio::{Interest, Registry, Token};
    use std::io;

    /// Register for `READABLE`: data may be waiting.
    impl Source for GkReceiver {
        fn register(&mut self, r: &Registry, t: Token, i: Interest) -> io::Result<()> {
            self.end.bell_mut().register(r, t, i)
        }
        fn reregister(&mut self, r: &Registry, t: Token, i: Interest) -> io::Result<()> {
            self.end.bell_mut().reregister(r, t, i)
        }
        fn deregister(&mut self, r: &Registry) -> io::Result<()> {
            self.end.bell_mut().deregister(r)
        }
    }

    /// Register for `READABLE`: space may have been freed.
    impl Source for GkSender {
        fn register(&mut self, r: &Registry, t: Token, i: Interest) -> io::Result<()> {
            self.end.bell_mut().register(r, t, i)
        }
        fn reregister(&mut self, r: &Registry, t: Token, i: Interest) -> io::Result<()> {
            self.end.bell_mut().reregister(r, t, i)
        }
        fn deregister(&mut self, r: &Registry) -> io::Result<()> {
            self.end.bell_mut().deregister(r)
        }
    }
}

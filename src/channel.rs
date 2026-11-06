//! A one-way message channel between threads or processes, through shared
//! memory: [`GkChannel`] creates a [`GkSender`] and a [`GkReceiver`].
//!
//! Messages (any length up to [`ChannelOptions::max_message`], including
//! zero) are copied into a ring in shared memory and read out in order, with
//! no system call per message while both sides are busy. Two of this crate's
//! eventfds act as doorbells: "data ready" (rung by the sender) and "space
//! ready" (rung by the receiver), only when the other side is asleep.
//!
//! * **One sender, one receiver.** The ends are not `Clone`. Each can be moved
//!   to another thread, or handed to another process (see "Sharing").
//! * **Blocking and non-blocking.** `send`/`recv` block by default; after
//!   `set_nonblocking(true)` they fail with [`io::ErrorKind::WouldBlock`]
//!   instead. A blocking call spins for a few microseconds before sleeping,
//!   so a quick reply costs no system calls.
//! * **Polling.** A [`GkReceiver`] is readable (`AsFd`/`AsSocket`, mio) when a
//!   message may be waiting; a [`GkSender`] is readable when space may have
//!   been freed. With mio or tokio, read/write until `WouldBlock` and wait
//!   for the next event.
//! * **Closing.** Dropping the sender lets the receiver drain what is left,
//!   then `recv` fails with [`io::ErrorKind::BrokenPipe`]. Dropping the
//!   receiver makes `send` fail with `BrokenPipe`. A peer *process* that dies
//!   is detected by blocking calls (within about a quarter second) and treated
//!   the same way.
//! * **Sharing.** Hand an end to another process with
//!   [`GkProcess::send`](crate::GkProcess::send) (the other side calls
//!   `recv_as::<GkReceiver>()` / `recv_as::<GkSender>()`), or on unix with
//!   `send_to`/`recv_from` over a `UnixStream`. After `fork()` both processes
//!   hold both ends: the process that keeps using an end calls
//!   [`claim`](GkReceiver::claim) on it, and the other process drops (or
//!   [`detach`](GkReceiver::detach)es) its copy without closing the channel.
//!   Each end records the process that owns it; using an end you don't own
//!   fails with [`io::ErrorKind::PermissionDenied`].
//!
//! Memory: the ring is `capacity` bytes of shared memory plus a small header.
//! On unix, memory is only allocated as the ring is first written; on Windows
//! the whole capacity counts against the commit limit when the channel is
//! created.
//!
//! ```
//! use adaptif_gatotkaca_eventfd::GkChannel;
//!
//! let (mut tx, mut rx) = GkChannel::new()?;
//! std::thread::spawn(move || {
//!     tx.send(b"hello").unwrap();
//!     tx.send(b"").unwrap(); // zero-length messages are fine
//! });
//! let mut buf = [0u8; 64];
//! assert_eq!(rx.recv(&mut buf)?, 5);
//! assert_eq!(&buf[..5], b"hello");
//! assert_eq!(rx.recv(&mut buf)?, 0);
//! // The sender is gone and everything was received:
//! assert_eq!(rx.recv(&mut buf).unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::time::Duration;

use crate::sys::{SharedRegion, current_pid, process_alive};
use crate::{EfdFlags, GkEventFd};

/// "GKC" + layout version 1.
const MAGIC: u32 = 0x474B_4301;
/// Record header: `len: u32`, `flags: u32`.
const REC_HDR: u64 = 8;
/// Record flag: padding up to the end of the ring, skip it.
const PAD: u32 = 1;
/// How long a blocking call sleeps before checking that the peer is alive.
const LIVENESS_PERIOD: Duration = Duration::from_millis(250);

/// Sizes of a channel. See [`GkChannel::with_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelOptions {
    /// Ring size in bytes: a power of two, at least 64. Default 16 MiB.
    pub capacity: usize,
    /// Largest message in bytes. At most `capacity / 2 - 8`. Default 1 MiB.
    pub max_message: usize,
}

impl ChannelOptions {
    /// 16 MiB.
    pub const DEFAULT_CAPACITY: usize = 16 << 20;
    /// 1 MiB.
    pub const DEFAULT_MAX_MESSAGE: usize = 1 << 20;
}

impl Default for ChannelOptions {
    fn default() -> Self {
        Self {
            capacity: Self::DEFAULT_CAPACITY,
            max_message: Self::DEFAULT_MAX_MESSAGE,
        }
    }
}

/// Record size of a `len`-byte message: header + payload rounded up to 8.
fn record_len(len: u64) -> u64 {
    REC_HDR + ((len + 7) & !7)
}

fn valid_sizes(capacity: u64, max_message: u64) -> bool {
    capacity.is_power_of_two()
        && (64..=1 << 40).contains(&capacity)
        && max_message <= u64::from(u32::MAX)
        && record_len(max_message) <= capacity / 2
}

// ---------------------------------------------------------------------------
// Shared layout
// ---------------------------------------------------------------------------

#[repr(C, align(64))]
#[derive(Debug)]
struct Meta {
    magic: AtomicU32,
    _reserved: AtomicU32,
    capacity: AtomicU64,
    max_message: AtomicU64,
    /// Owning process of each end (0: in transit to another process).
    sender_pid: AtomicU32,
    receiver_pid: AtomicU32,
    sender_closed: AtomicU32,
    receiver_closed: AtomicU32,
}

/// One side's position and "I am waiting" flag, on its own cache line.
#[repr(C, align(64))]
#[derive(Debug)]
struct Line {
    pos: AtomicU64,
    waiting: AtomicU32,
}

#[repr(C)]
#[derive(Debug)]
struct Header {
    meta: Meta,
    /// Written by the sender: bytes published so far; `waiting`: the sender
    /// sleeps until space is freed.
    tail: Line,
    /// Written by the receiver: bytes consumed so far; `waiting`: the
    /// receiver sleeps until data arrives.
    head: Line,
}

/// Offset of the ring data (the header is a multiple of 64 bytes).
const DATA: usize = size_of::<Header>();

/// The ring logic, over any memory laid out as `Header` + `capacity` bytes.
#[derive(Debug, Clone, Copy)]
struct View {
    hdr: NonNull<Header>,
    data: NonNull<u8>,
    cap: u64,
    max: u64,
}

impl View {
    /// # Safety
    /// `base` must point to a live, initialised channel layout (header with
    /// the given sizes, then `capacity` bytes), 64-byte aligned.
    unsafe fn new(base: NonNull<u8>) -> Self {
        let hdr = base.cast::<Header>();
        // SAFETY: the caller guarantees an initialised header.
        let meta = unsafe { &hdr.as_ref().meta };
        Self {
            hdr,
            // SAFETY: the data follows the header inside the same mapping.
            data: unsafe { base.add(DATA) },
            cap: meta.capacity.load(Ordering::Relaxed),
            max: meta.max_message.load(Ordering::Relaxed),
        }
    }

    fn hdr(&self) -> &Header {
        // SAFETY: valid for the lifetime of the owning mapping (see `new`).
        unsafe { self.hdr.as_ref() }
    }

    /// Where a `len`-byte message can go, if there is room: `(pad, index)`.
    /// `pad` bytes at the end of the ring are skipped first (the message
    /// then starts at index 0) so that every message is contiguous.
    fn reserve(&self, tail: u64, len: u64) -> Option<(u64, u64)> {
        let need = record_len(len);
        let head = self.hdr().head.pos.load(Ordering::Acquire);
        let free = self.cap.checked_sub(tail.wrapping_sub(head))?;
        let idx = tail & (self.cap - 1);
        let to_end = self.cap - idx;
        if need <= to_end {
            (need <= free).then_some((0, idx))
        } else {
            (to_end + need <= free).then_some((to_end, 0))
        }
    }

    /// The payload area of a record starting at ring index `idx`.
    fn payload(&self, idx: u64) -> *mut u8 {
        // SAFETY: `idx + 8 + payload <= cap` was checked by `reserve`.
        unsafe { self.data.as_ptr().add((idx + REC_HDR) as usize) }
    }

    /// # Safety
    /// `idx` must be 8-aligned and leave room for a record header.
    unsafe fn write_header(&self, idx: u64, len: u32, flags: u32) {
        // SAFETY: forwarded; the record area is owned by the sender until
        // published.
        unsafe {
            let p = self.data.as_ptr().add(idx as usize).cast::<u32>();
            p.write(len);
            p.add(1).write(flags);
        }
    }

    /// Publishes a message of `len` bytes reserved at (`pad`, `idx`) from
    /// `tail` and returns the new tail.
    ///
    /// # Safety
    /// (`pad`, `idx`) must come from `reserve(tail, reserved_len)` with
    /// `len <= reserved_len`, and the payload must already be written.
    unsafe fn publish(&self, tail: u64, pad: u64, idx: u64, len: u64) -> u64 {
        // SAFETY: forwarded; headers lie inside the reserved space.
        unsafe {
            if pad > 0 {
                self.write_header(tail & (self.cap - 1), 0, PAD);
            }
            self.write_header(idx, len as u32, 0);
        }
        let new_tail = tail + pad + record_len(len);
        self.hdr().tail.pos.store(new_tail, Ordering::Release);
        new_tail
    }

    /// The next message at `head`: `(payload, len, head after it)`, or
    /// `None` if the ring is empty. Fails on a corrupted ring rather than
    /// reading out of bounds.
    fn peek(&self, mut head: u64) -> io::Result<Option<(*const u8, usize, u64)>> {
        let corrupt = || io::Error::new(io::ErrorKind::InvalidData, "corrupted channel ring");
        let tail = self.hdr().tail.pos.load(Ordering::Acquire);
        loop {
            if head == tail {
                return Ok(None);
            }
            if tail.wrapping_sub(head) > self.cap || head % 8 != 0 {
                return Err(corrupt());
            }
            let idx = head & (self.cap - 1);
            // SAFETY: `idx` is 8-aligned and inside the ring; the record was
            // published (Acquire above) and is not reused until we release it.
            let (len, flags) = unsafe {
                let p = self.data.as_ptr().add(idx as usize).cast::<u32>();
                (u64::from(p.read()), p.add(1).read())
            };
            if flags & PAD != 0 {
                head += self.cap - idx;
                continue;
            }
            let rec = record_len(len);
            if len > self.max || rec > self.cap - idx || rec > tail - head {
                return Err(corrupt());
            }
            return Ok(Some((self.payload(idx), len as usize, head + rec)));
        }
    }
}

/// A channel's shared memory, mapped in this process.
#[derive(Debug)]
struct Ring {
    region: SharedRegion,
    view: View,
}

// SAFETY: the view points into `region`, which is Send + Sync shared memory
// accessed through the channel protocol.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

impl Ring {
    fn create(opts: ChannelOptions) -> io::Result<Self> {
        let (cap, max) = (opts.capacity as u64, opts.max_message as u64);
        if !valid_sizes(cap, max) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capacity must be a power of two >= 64, and max_message <= capacity / 2 - 8",
            ));
        }
        let region = SharedRegion::create(DATA + opts.capacity)?;
        // SAFETY: fresh, zeroed, page-aligned and large enough; not shared yet.
        unsafe {
            let hdr = region.ptr().cast::<Header>();
            let meta = &hdr.as_ref().meta;
            meta.capacity.store(cap, Ordering::Relaxed);
            meta.max_message.store(max, Ordering::Relaxed);
            let me = current_pid();
            meta.sender_pid.store(me, Ordering::Relaxed);
            meta.receiver_pid.store(me, Ordering::Relaxed);
            meta.magic.store(MAGIC, Ordering::Release);
        }
        Self::with_region(region)
    }

    /// Accepts a mapped object as a channel ring.
    fn is_ring(ptr: NonNull<u8>, len: usize) -> bool {
        if len < DATA {
            return false;
        }
        // SAFETY: at least a header's worth of mapped, page-aligned memory.
        let meta = unsafe { &ptr.cast::<Header>().as_ref().meta };
        let cap = meta.capacity.load(Ordering::Relaxed);
        meta.magic.load(Ordering::Acquire) == MAGIC
            && valid_sizes(cap, meta.max_message.load(Ordering::Relaxed))
            && (DATA as u64).saturating_add(cap) <= len as u64
    }

    fn with_region(region: SharedRegion) -> io::Result<Self> {
        // SAFETY: created or validated (`is_ring`) above.
        let view = unsafe { View::new(region.ptr()) };
        Ok(Self { region, view })
    }

    fn meta(&self) -> &Meta {
        &self.view.hdr().meta
    }
}

// ---------------------------------------------------------------------------
// The ends
// ---------------------------------------------------------------------------

/// Which end of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Sender,
    Receiver,
}

/// What both ends have: the ring and the two doorbells.
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
}

impl End {
    fn view(&self) -> &View {
        &self.ring.view
    }

    fn owner(&self) -> &AtomicU32 {
        let meta = self.ring.meta();
        match self.side {
            Side::Sender => &meta.sender_pid,
            Side::Receiver => &meta.receiver_pid,
        }
    }

    fn peer_owner(&self) -> &AtomicU32 {
        let meta = self.ring.meta();
        match self.side {
            Side::Sender => &meta.receiver_pid,
            Side::Receiver => &meta.sender_pid,
        }
    }

    fn peer_closed(&self) -> bool {
        let meta = self.ring.meta();
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

    /// My "I am waiting" flag, and the peer's.
    fn lines(&self) -> (&Line, &Line) {
        let hdr = self.view().hdr();
        match self.side {
            Side::Sender => (&hdr.tail, &hdr.head),
            Side::Receiver => (&hdr.head, &hdr.tail),
        }
    }

    /// After making progress: wake the peer if it is asleep.
    fn wake_peer(&self) {
        fence(Ordering::SeqCst);
        if self.lines().1.waiting.swap(0, Ordering::SeqCst) != 0 {
            // Non-blocking doorbell; it cannot overflow in practice.
            let _ = self.peer_bell().write(1);
        }
    }

    /// Called when there is nothing to do (`ready()` false): announces that
    /// we are waiting, then either returns `WouldBlock` (non-blocking) or
    /// sleeps until the doorbell rings. `Ok(())` means "look again".
    fn wait(&self, ready: impl Fn() -> bool) -> io::Result<()> {
        // Blocking: spin briefly first. A reply that arrives within a few
        // microseconds (ping-pong, a busy peer) is then picked up without
        // any system call on either side, since we never announce that we
        // sleep. Non-blocking calls return `WouldBlock` right away instead.
        if !self.nonblocking {
            let backoff = crossbeam_utils::Backoff::new();
            while !backoff.is_completed() {
                if ready() {
                    return Ok(());
                }
                backoff.snooze();
            }
        }
        // Clear old rings first, so a level-triggered poller doesn't spin.
        let _ = self.bell().read();
        let mine = self.lines().0;
        mine.waiting.store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if ready() {
            mine.waiting.store(0, Ordering::Relaxed);
            return Ok(());
        }
        if self.peer_closed() {
            return Err(broken());
        }
        if self.nonblocking {
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
        let meta = self.ring.meta();
        let flag = match self.side {
            Side::Sender => &meta.sender_closed,
            Side::Receiver => &meta.receiver_closed,
        };
        flag.store(1, Ordering::Release);
        fence(Ordering::SeqCst);
        let _ = self.peer_bell().write(1);
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

impl Drop for End {
    fn drop(&mut self) {
        self.close();
    }
}

/// Creates channels. See the [module docs](self).
#[derive(Debug)]
pub struct GkChannel(());

impl GkChannel {
    /// A channel with the default sizes (16 MiB ring, 1 MiB messages).
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> io::Result<(GkSender, GkReceiver)> {
        Self::with_options(ChannelOptions::default())
    }

    /// A channel with the given sizes. Fails with
    /// [`io::ErrorKind::InvalidInput`] if `capacity` is not a power of two of
    /// at least 64 bytes or `max_message > capacity / 2 - 8`.
    pub fn with_options(opts: ChannelOptions) -> io::Result<(GkSender, GkReceiver)> {
        let ring = Arc::new(Ring::create(opts)?);
        let bell = || GkEventFd::new(0, EfdFlags::NONBLOCK | EfdFlags::CLOEXEC);
        let (data_ready, space_ready) = (bell()?, bell()?);
        let sender = End {
            ring: Arc::clone(&ring),
            data_ready: data_ready.try_clone()?,
            space_ready: space_ready.try_clone()?,
            side: Side::Sender,
            nonblocking: false,
            detached: false,
        };
        let receiver = End {
            ring,
            data_ready,
            space_ready,
            side: Side::Receiver,
            nonblocking: false,
            detached: false,
        };
        Ok((
            GkSender {
                end: sender,
                tail: 0,
            },
            GkReceiver {
                end: receiver,
                head: 0,
            },
        ))
    }
}

/// The sending end of a channel. See the [module docs](self).
#[derive(Debug)]
pub struct GkSender {
    end: End,
    /// Our copy of the published tail (we are its only writer).
    tail: u64,
}

/// The receiving end of a channel. See the [module docs](self).
#[derive(Debug)]
pub struct GkReceiver {
    end: End,
    /// Our copy of the consumed head (we are its only writer).
    head: u64,
}

/// Space reserved in the ring by [`GkSender::reserve`]: write the message
/// into it (it derefs to `&mut [u8]` of the reserved length), then
/// [`commit`](Self::commit). Dropping it without committing sends nothing.
#[derive(Debug)]
pub struct SendSlot<'a> {
    sender: &'a mut GkSender,
    pad: u64,
    idx: u64,
    len: usize,
}

impl GkSender {
    /// Sends one message (copied into the ring). Blocks while the ring is
    /// full, or fails with [`io::ErrorKind::WouldBlock`] in non-blocking
    /// mode. Fails with [`io::ErrorKind::InvalidInput`] if `msg` is longer
    /// than [`max_message`](Self::max_message), and
    /// [`io::ErrorKind::BrokenPipe`] once the receiver is gone.
    pub fn send(&mut self, msg: &[u8]) -> io::Result<()> {
        let mut slot = self.reserve(msg.len())?;
        slot.copy_from_slice(msg);
        slot.commit(msg.len());
        Ok(())
    }

    /// Reserves `len` bytes for a message written in place (no extra copy).
    /// Waits for space like [`send`](Self::send).
    pub fn reserve(&mut self, len: usize) -> io::Result<SendSlot<'_>> {
        self.end.check_owner()?;
        if len as u64 > self.end.view().max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "message larger than the channel's max_message",
            ));
        }
        loop {
            if self.end.peer_closed() {
                return Err(broken());
            }
            if let Some((pad, idx)) = self.end.view().reserve(self.tail, len as u64) {
                return Ok(SendSlot {
                    sender: self,
                    pad,
                    idx,
                    len,
                });
            }
            let (view, tail) = (*self.end.view(), self.tail);
            self.end.wait(|| view.reserve(tail, len as u64).is_some())?;
        }
    }

    /// Ring size in bytes.
    pub fn capacity(&self) -> usize {
        self.end.view().cap as usize
    }

    /// Largest message in bytes.
    pub fn max_message(&self) -> usize {
        self.end.view().max as usize
    }

    /// Switches between blocking (default) and non-blocking sends.
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
        self.tail = self.end.view().hdr().tail.pos.load(Ordering::Acquire);
    }

    /// Drops this process's copy without closing the channel (e.g. the
    /// copy a process keeps after `fork` but doesn't use).
    pub fn detach(mut self) {
        self.end.detached = true;
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
        // SAFETY: reserved by `reserve(s.tail, self.len)` and `n <= len`.
        s.tail = unsafe { s.end.view().publish(s.tail, self.pad, self.idx, n as u64) };
        s.end.wake_peer();
    }
}

impl std::ops::Deref for SendSlot<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: reserved space owned by this sender until published.
        unsafe { std::slice::from_raw_parts(self.sender.end.view().payload(self.idx), self.len) }
    }
}

impl std::ops::DerefMut for SendSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; nobody else reads it before it is published.
        unsafe {
            std::slice::from_raw_parts_mut(self.sender.end.view().payload(self.idx), self.len)
        }
    }
}

impl GkReceiver {
    /// Waits for the next message: `(payload, len, head after it)`.
    fn next(&mut self) -> io::Result<(*const u8, usize, u64)> {
        self.end.check_owner()?;
        loop {
            if let Some(msg) = self.end.view().peek(self.head)? {
                return Ok(msg);
            }
            let (view, head) = (*self.end.view(), self.head);
            self.end
                .wait(|| matches!(view.peek(head), Ok(Some(_)) | Err(_)))?;
        }
    }

    /// Frees the space of the message just read.
    fn consume(&mut self, new_head: u64) {
        self.head = new_head;
        self.end
            .view()
            .hdr()
            .head
            .pos
            .store(new_head, Ordering::Release);
        self.end.wake_peer();
    }

    /// Receives one message into `buf` and returns its length (0 for a
    /// zero-length message). Blocks while the ring is empty, or fails with
    /// [`io::ErrorKind::WouldBlock`] in non-blocking mode. Fails with
    /// [`io::ErrorKind::InvalidInput`] if `buf` is too small (the message
    /// stays queued; see [`next_len`](Self::next_len)), and
    /// [`io::ErrorKind::BrokenPipe`] once the sender is gone and every
    /// message was received.
    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (ptr, len, new_head) = self.next()?;
        if len > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer smaller than the next message",
            ));
        }
        // SAFETY: `ptr` points to `len` published bytes we own until consumed.
        buf[..len].copy_from_slice(unsafe { std::slice::from_raw_parts(ptr, len) });
        self.consume(new_head);
        Ok(len)
    }

    /// Like [`recv`](Self::recv), replacing the contents of `out` with the
    /// message (any length).
    pub fn recv_vec(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let len = self.recv_with(|msg| {
            out.clear();
            out.extend_from_slice(msg);
            msg.len()
        })?;
        Ok(len)
    }

    /// Like [`recv`](Self::recv), but lends the message in place (no copy)
    /// to `f`; its space is freed when `f` returns.
    pub fn recv_with<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> io::Result<R> {
        let (ptr, len, new_head) = self.next()?;
        // SAFETY: as in `recv`; the sender does not reuse it before `consume`.
        let r = f(unsafe { std::slice::from_raw_parts(ptr, len) });
        self.consume(new_head);
        Ok(r)
    }

    /// Length of the next message without receiving it or waiting: `None`
    /// if the ring is empty. Fails with [`io::ErrorKind::BrokenPipe`] if it
    /// is empty and the sender is gone.
    pub fn next_len(&mut self) -> io::Result<Option<usize>> {
        self.end.check_owner()?;
        match self.end.view().peek(self.head)? {
            Some((_, len, _)) => Ok(Some(len)),
            None if self.end.peer_closed() => match self.end.view().peek(self.head)? {
                Some((_, len, _)) => Ok(Some(len)),
                None => Err(broken()),
            },
            None => Ok(None),
        }
    }

    /// Ring size in bytes.
    pub fn capacity(&self) -> usize {
        self.end.view().cap as usize
    }

    /// Largest message in bytes.
    pub fn max_message(&self) -> usize {
        self.end.view().max as usize
    }

    /// Switches between blocking (default) and non-blocking receives.
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
        self.head = self.end.view().hdr().head.pos.load(Ordering::Acquire);
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

    /// Readable when a message may be waiting.
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

    /// Readable when a message may be waiting.
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

    /// Register for `READABLE`: a message may be waiting.
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

// ---------------------------------------------------------------------------
// Sharing with other processes
// ---------------------------------------------------------------------------

impl End {
    /// Marks this end as on its way to another process, so dropping our copy
    /// doesn't close the channel. Returns the previous owner for `restore`.
    fn hand_off(&self) -> u32 {
        self.owner().swap(0, Ordering::AcqRel)
    }

    fn restore(&self, owner: u32) {
        self.owner().store(owner, Ordering::Release);
    }

    /// Builds an end received by this process, which becomes its owner.
    fn received(ring: Ring, data_ready: GkEventFd, space_ready: GkEventFd, side: Side) -> Self {
        let end = Self {
            ring: Arc::new(ring),
            data_ready,
            space_ready,
            side,
            nonblocking: false,
            detached: false,
        };
        end.owner().store(current_pid(), Ordering::Release);
        end
    }

    fn tag(side: Side) -> &'static str {
        #[cfg(unix)]
        let native = crate::sys::TAG.ends_with('n');
        #[cfg(windows)]
        let native = false;
        match (side, native) {
            (Side::Sender, true) => "gkch1:Sn",
            (Side::Sender, false) => "gkch1:Su",
            (Side::Receiver, true) => "gkch1:Rn",
            (Side::Receiver, false) => "gkch1:Ru",
        }
    }
}

#[cfg(unix)]
impl End {
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        let mut fds = vec![self.ring.region.fd()];
        fds.extend(self.data_ready.inner.share_fds());
        fds.extend(self.space_ready.inner.share_fds());
        let owner = self.hand_off();
        let r = crate::sys::unix_share::send(sock, Self::tag(self.side), &fds);
        if r.is_err() {
            self.restore(owner);
        }
        r
    }

    fn recv_over(sock: std::os::fd::BorrowedFd<'_>, side: Side) -> io::Result<Self> {
        use crate::sys::{Inner, SHARE_FDS};
        let mut fds = crate::sys::unix_share::recv(sock, Self::tag(side), 1 + 2 * SHARE_FDS)?;
        let space = fds.split_off(1 + SHARE_FDS);
        let data = fds.split_off(1);
        let region_fd = fds.pop().expect("one region descriptor");
        let region = SharedRegion::from_owned(region_fd, DATA, Ring::is_ring)?;
        let data_ready = GkEventFd {
            inner: Inner::from_share_fds(data)?,
        };
        let space_ready = GkEventFd {
            inner: Inner::from_share_fds(space)?,
        };
        Ok(Self::received(
            Ring::with_region(region)?,
            data_ready,
            space_ready,
            side,
        ))
    }
}

#[cfg(windows)]
impl End {
    /// An offer the process `pid` can take with `take` (see
    /// `GkProcess::send`): `gkch1:<S|R>u|<region>|<eventfd offer>|<eventfd offer>`.
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        use std::os::windows::io::AsRawHandle;
        let owner = self.hand_off();
        let r = (|| {
            let keep = self.ring.region.section().try_clone_to_owned()?;
            let (d, kd) = self.data_ready.inner.offer_to(pid)?;
            let (s, ks) = self.space_ready.inner.offer_to(pid)?;
            let token = format!(
                "{}|{:x}|{d}|{s}",
                Self::tag(self.side),
                keep.as_raw_handle() as usize
            );
            Ok((token, vec![keep, kd, ks]))
        })();
        if r.is_err() {
            self.restore(owner);
        }
        r
    }

    fn take(token: &str, pid: u32, side: Side) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, "invalid channel offer");
        let mut parts = token.trim().split('|');
        if parts.next() != Some(Self::tag(side)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not the expected kind of object",
            ));
        }
        let section =
            usize::from_str_radix(parts.next().ok_or_else(bad)?, 16).map_err(|_| bad())?;
        let d = parts.next().ok_or_else(bad)?;
        let s = parts.next().ok_or_else(bad)?;
        if parts.next().is_some() {
            return Err(bad());
        }
        let region = crate::sys::duplicate_from(pid, section)?;
        let region = SharedRegion::from_owned(region, DATA, Ring::is_ring)?;
        let data_ready = GkEventFd {
            inner: crate::sys::Inner::take_offer(d, pid)?,
        };
        let space_ready = GkEventFd {
            inner: crate::sys::Inner::take_offer(s, pid)?,
        };
        Ok(Self::received(
            Ring::with_region(region)?,
            data_ready,
            space_ready,
            side,
        ))
    }
}

impl GkSender {
    /// Unix: hands this end to the process at the other end of `stream`
    /// (`SCM_RIGHTS`), which calls [`GkSender::recv_from`]. This process's
    /// copy is detached: dropping it does not close the channel.
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        self.end.send_over(std::os::fd::AsFd::as_fd(stream))
    }

    /// Unix: receives a sender passed with [`GkSender::send_to`]; this process
    /// becomes its owner.
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        let end = End::recv_over(std::os::fd::AsFd::as_fd(stream), Side::Sender)?;
        let tail = end.view().hdr().tail.pos.load(Ordering::Acquire);
        Ok(Self { end, tail })
    }
}

impl GkReceiver {
    /// Unix: hands this end to the process at the other end of `stream`
    /// (`SCM_RIGHTS`), which calls [`GkReceiver::recv_from`]. This process's
    /// copy is detached: dropping it does not close the channel.
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        self.end.send_over(std::os::fd::AsFd::as_fd(stream))
    }

    /// Unix: receives a receiver passed with [`GkReceiver::send_to`]; this
    /// process becomes its owner.
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        let end = End::recv_over(std::os::fd::AsFd::as_fd(stream), Side::Receiver)?;
        let head = end.view().hdr().head.pos.load(Ordering::Acquire);
        Ok(Self { end, head })
    }
}

/// Lets `GkProcess` move channel ends (see `process.rs`).
impl crate::process::private::Sealed for GkSender {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.end.send_over(sock)
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        let end = End::recv_over(sock, Side::Sender)?;
        let tail = end.view().hdr().tail.pos.load(Ordering::Acquire);
        Ok(Self { end, tail })
    }
    #[cfg(windows)]
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        self.end.offer(pid)
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        let end = End::take(token, pid, Side::Sender)?;
        let tail = end.view().hdr().tail.pos.load(Ordering::Acquire);
        Ok(Self { end, tail })
    }
}

impl crate::process::private::Sealed for GkReceiver {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.end.send_over(sock)
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        let end = End::recv_over(sock, Side::Receiver)?;
        let head = end.view().hdr().head.pos.load(Ordering::Acquire);
        Ok(Self { end, head })
    }
    #[cfg(windows)]
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        self.end.offer(pid)
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        let end = End::take(token, pid, Side::Receiver)?;
        let head = end.view().hdr().head.pos.load(Ordering::Acquire);
        Ok(Self { end, head })
    }
}

impl crate::process::Shareable for GkSender {}
impl crate::process::Shareable for GkReceiver {}

// ---------------------------------------------------------------------------
// Tests of the ring logic alone (also run under Miri: heap memory, no OS)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod ring_tests {
    use super::*;

    /// A channel layout in ordinary heap memory.
    struct HeapRing {
        _buf: Vec<u64>,
        view: View,
    }

    fn heap_ring(cap: usize, max: usize) -> HeapRing {
        assert!(valid_sizes(cap as u64, max as u64));
        // u64 storage, 64-byte aligned by over-allocating.
        let mut buf = vec![0u64; (DATA + cap) / 8 + 8];
        let base = buf.as_mut_ptr().cast::<u8>();
        // Keep the allocation's provenance (no integer-to-pointer casts).
        // SAFETY: the offset (< 64) stays inside the over-allocated buffer.
        let ptr = NonNull::new(unsafe { base.add(base.align_offset(64)) }).unwrap();
        // SAFETY: zeroed memory large enough for the layout.
        unsafe {
            let meta = &ptr.cast::<Header>().as_ref().meta;
            meta.capacity.store(cap as u64, Ordering::Relaxed);
            meta.max_message.store(max as u64, Ordering::Relaxed);
            meta.magic.store(MAGIC, Ordering::Release);
        }
        // SAFETY: initialised above.
        let view = unsafe { View::new(ptr) };
        HeapRing { _buf: buf, view }
    }

    fn put(v: &View, tail: &mut u64, msg: &[u8]) -> bool {
        match v.reserve(*tail, msg.len() as u64) {
            Some((pad, idx)) => {
                // SAFETY: reserved space of at least msg.len() bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(msg.as_ptr(), v.payload(idx), msg.len());
                    *tail = v.publish(*tail, pad, idx, msg.len() as u64);
                }
                true
            }
            None => false,
        }
    }

    fn take(v: &View, head: &mut u64) -> Option<Vec<u8>> {
        let (ptr, len, next) = v.peek(*head).unwrap()?;
        // SAFETY: a published message of `len` bytes.
        let msg = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        *head = next;
        v.hdr().head.pos.store(next, Ordering::Release);
        Some(msg)
    }

    #[test]
    fn sizes() {
        assert!(valid_sizes(16 << 20, 1 << 20));
        assert!(valid_sizes(64, 24));
        assert!(!valid_sizes(64, 25)); // record 40 > 32
        assert!(!valid_sizes(100, 8)); // not a power of two
        assert!(!valid_sizes(32, 8)); // too small
        assert_eq!(record_len(0), 8);
        assert_eq!(record_len(1), 16);
        assert_eq!(record_len(8), 16);
        assert_eq!(DATA % 64, 0);
    }

    #[test]
    fn fifo_with_wraparound_and_zero_length() {
        let r = heap_ring(256, 64);
        let v = &r.view;
        let (mut tail, mut head) = (0u64, 0u64);
        let mut expect = std::collections::VecDeque::new();
        for i in 0..2000u32 {
            let len = (i * 7 % 65) as usize; // 0..=64, including 0
            let msg: Vec<u8> = (0..len).map(|b| (b as u32 ^ i) as u8).collect();
            while !put(v, &mut tail, &msg) {
                assert_eq!(take(v, &mut head).as_ref(), expect.front());
                expect.pop_front();
            }
            expect.push_back(msg);
        }
        while let Some(m) = take(v, &mut head) {
            assert_eq!(Some(&m), expect.front());
            expect.pop_front();
        }
        assert!(expect.is_empty());
        assert_eq!(head, tail);
    }

    #[test]
    fn full_ring_refuses_until_space_is_freed() {
        let r = heap_ring(64, 24);
        let v = &r.view;
        let (mut tail, mut head) = (0u64, 0u64);
        assert!(put(v, &mut tail, &[1; 24])); // 32 bytes
        assert!(put(v, &mut tail, &[2; 24])); // 64: full
        assert!(!put(v, &mut tail, &[]));
        assert_eq!(take(v, &mut head).unwrap(), vec![1; 24]);
        assert!(put(v, &mut tail, &[3; 8]));
        assert_eq!(take(v, &mut head).unwrap(), vec![2; 24]);
        assert_eq!(take(v, &mut head).unwrap(), vec![3; 8]);
        assert!(take(v, &mut head).is_none());
    }

    #[test]
    fn corrupted_headers_are_rejected() {
        let r = heap_ring(256, 64);
        let v = &r.view;
        let mut tail = 0;
        assert!(put(v, &mut tail, b"hi"));
        // SAFETY: overwrite the record's length field in our heap ring.
        unsafe { v.data.as_ptr().cast::<u32>().write(1000) };
        assert_eq!(v.peek(0).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// Writer and reader threads over one ring, with random message sizes.
    #[test]
    fn two_threads() {
        let r = heap_ring(1024, 200);
        let v = r.view;
        let n: u32 = if cfg!(miri) { 300 } else { 100_000 };
        struct SendView(View);
        // SAFETY: test only; the ring outlives both threads (joined below).
        unsafe impl Send for SendView {}
        let wv = SendView(v);
        let writer = std::thread::spawn(move || {
            let v = wv;
            let mut tail = 0;
            for i in 0..n {
                let len = (i.wrapping_mul(2654435761) % 201) as usize;
                let msg = vec![i as u8; len];
                while !put(&v.0, &mut tail, &msg) {
                    std::thread::yield_now();
                }
            }
        });
        let mut head = 0;
        for i in 0..n {
            let msg = loop {
                if let Some(m) = take(&v, &mut head) {
                    break m;
                }
                std::thread::yield_now();
            };
            let len = (i.wrapping_mul(2654435761) % 201) as usize;
            assert_eq!(msg.len(), len);
            assert!(msg.iter().all(|&b| b == i as u8));
        }
        writer.join().unwrap();
        drop(r);
    }
}

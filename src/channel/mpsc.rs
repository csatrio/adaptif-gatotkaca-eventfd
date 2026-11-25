//! The multi-producer sender: [`GkMpSender`], a channel sender that many
//! threads of one process can use at once.
//!
//! The message lane of such a channel uses the multi-producer protocol (see
//! `lane.rs`): a producer claims its record with one CAS on the shared
//! reservation counter, copies the payload, and publishes it by storing the
//! record header; then it rings the receiver if it waits. No lock is taken on
//! that path. Each producer's messages arrive in the order it sent them;
//! messages of different producers interleave in reservation order.
//!
//! The rest of the end (ownership, closing, the bulk lane, and sleeping
//! while the ring is full) is an ordinary [`GkSender`] behind a mutex, so
//! bulk transfers are sent one at a time, and only threads that must wait
//! for space contend for it.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use super::lane::{Lane, Layout, record_len};
use super::{BulkWriter, GkSender, KIND_UNSPECIFIED, MSG, broken};
use crate::GkEventFd;
use crate::sys::{current_pid, process_alive};

/// The sending end of a multi-producer channel ([`GkChannel::new_mpsc`]):
/// `Clone`, `Send` and `Sync`, so any number of threads of the owning
/// process can send at once, through `&self`.
///
/// * **Messages** ([`send`](Self::send), [`try_send`](Self::try_send),
///   [`reserve`](Self::reserve) + [`MpSendSlot::commit`]) are lock-free:
///   one CAS to claim ring space, one copy, one atomic store to publish.
///   Each thread's messages arrive in the order it sent them.
/// * **Bulk transfers** go through [`bulk`](Self::bulk), which locks the
///   bulk lane for one transfer at a time.
/// * **Blocking.** `send`/`reserve` wait while the ring is full (unless
///   [`set_nonblocking`](Self::set_nonblocking) is on, shared by all clones);
///   `try_send`/`try_reserve` never wait. After a `WouldBlock`, the sender's
///   descriptor (`AsFd`/`AsSocket`) becomes readable when space may have been
///   freed.
/// * **Closing.** The channel closes when the last clone is dropped.
/// * **Processes.** All clones belong to one process. The end can be handed
///   to another process with [`GkProcess::send`](crate::GkProcess::send)
///   (`recv_as::<GkMpSender>()` there); the clones left behind then fail
///   with [`io::ErrorKind::PermissionDenied`]. After `fork()`, call
///   [`claim`](Self::claim) in the process that keeps sending and
///   [`detach`](Self::detach) in the other.
///
/// [`GkChannel::new_mpsc`]: super::GkChannel::new_mpsc
#[derive(Debug, Clone)]
pub struct GkMpSender {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    layout: Layout,
    /// The message lane (multi-producer protocol).
    lane: Lane,
    /// A handle on the receiver's doorbell, rung after publishing.
    data_ready: GkEventFd,
    /// A handle on our own doorbell, for polling.
    space_ready: GkEventFd,
    nonblocking: AtomicBool,
    /// The end itself: ownership, closing, the bulk lane, blocking waits.
    side: Mutex<GkSender>,
}

/// Space reserved in the message ring by [`GkMpSender::reserve`]: write the
/// message into it (it derefs to `&mut [u8]` of the reserved length), then
/// [`commit`](Self::commit). Dropping it without committing sends nothing.
///
/// Messages reserved after this one by other threads reach the receiver only
/// once this one is committed or dropped, so don't hold it long.
#[derive(Debug)]
pub struct MpSendSlot<'a> {
    sender: &'a GkMpSender,
    idx: u64,
    len: usize,
    kind: u16,
    done: bool,
}

/// The bulk lane of a [`GkMpSender`], locked for this thread
/// ([`GkMpSender::bulk`]): one transfer at a time, by one thread at a time.
#[derive(Debug)]
pub struct MpBulk<'a> {
    side: MutexGuard<'a, GkSender>,
}

impl GkMpSender {
    /// Wraps the sender end of a channel created in multi-producer mode.
    pub(super) fn from_sender(side: GkSender) -> io::Result<Self> {
        debug_assert!(side.end.is_mpsc());
        let layout = side.end.ring.layout;
        let data_ready = side.end.data_ready.try_clone()?;
        let space_ready = side.end.space_ready.try_clone()?;
        let nonblocking = AtomicBool::new(side.end.nonblocking);
        Ok(Self { shared: Arc::new(Shared { layout, lane: layout.msg, data_ready, space_ready, nonblocking, side: Mutex::new(side) }) })
    }

    fn side(&self) -> MutexGuard<'_, GkSender> {
        self.shared.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fails unless this process owns the end and the receiver is there.
    fn check(&self) -> io::Result<()> {
        let meta = self.shared.layout.meta();
        if meta.sender_pid.load(Ordering::Relaxed) != current_pid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "this channel end belongs to another process (call claim() after fork)",
            ));
        }
        if meta.receiver_closed.load(Ordering::Acquire) != 0 {
            return Err(broken());
        }
        Ok(())
    }

    /// Rings the receiver if it waits (after publishing or padding).
    fn wake(&self) {
        fence(Ordering::SeqCst);
        let waiting = &self.shared.lane.head_line().waiting;
        if waiting.load(Ordering::Relaxed) != 0 && waiting.swap(0, Ordering::SeqCst) != 0 {
            // Non-blocking doorbell; it cannot overflow in practice.
            let _ = self.shared.data_ready.write(1);
        }
    }

    /// Sends one message (copied into the message ring). Blocks while the
    /// ring is full, or fails with [`io::ErrorKind::WouldBlock`] in
    /// non-blocking mode. Fails with [`io::ErrorKind::InvalidInput`] if `msg`
    /// is longer than [`max_message`](Self::max_message), and
    /// [`io::ErrorKind::BrokenPipe`] once the receiver is gone.
    pub fn send(&self, msg: &[u8]) -> io::Result<()> {
        self.send_kind(KIND_UNSPECIFIED, msg)
    }

    /// Like [`send`](Self::send), tagging the message with `kind`.
    pub fn send_kind(&self, kind: u16, msg: &[u8]) -> io::Result<()> {
        let mut slot = self.reserve_kind(kind, msg.len())?;
        slot.copy_from_slice(msg);
        slot.commit(msg.len());
        Ok(())
    }

    /// Like [`send`](Self::send), but never waits: fails with
    /// [`io::ErrorKind::WouldBlock`] if the ring is full, whatever the
    /// blocking mode. The descriptor then becomes readable when space may
    /// have been freed.
    pub fn try_send(&self, msg: &[u8]) -> io::Result<()> {
        self.try_send_kind(KIND_UNSPECIFIED, msg)
    }

    /// Like [`try_send`](Self::try_send), tagging the message with `kind`.
    pub fn try_send_kind(&self, kind: u16, msg: &[u8]) -> io::Result<()> {
        let mut slot = self.try_reserve_kind(kind, msg.len())?;
        slot.copy_from_slice(msg);
        slot.commit(msg.len());
        Ok(())
    }

    /// Reserves `len` bytes for a message written in place (no extra copy).
    /// Waits for space like [`send`](Self::send).
    pub fn reserve(&self, len: usize) -> io::Result<MpSendSlot<'_>> {
        self.reserve_kind(KIND_UNSPECIFIED, len)
    }

    /// Like [`reserve`](Self::reserve), for a message of the given `kind`.
    pub fn reserve_kind(&self, kind: u16, len: usize) -> io::Result<MpSendSlot<'_>> {
        loop {
            match self.try_reserve_kind(kind, len) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && !self.is_nonblocking() => self.wait_room(len as u64)?,
                r => return r,
            }
        }
    }

    /// Like [`reserve`](Self::reserve), but never waits (see
    /// [`try_send`](Self::try_send)).
    pub fn try_reserve(&self, len: usize) -> io::Result<MpSendSlot<'_>> {
        self.try_reserve_kind(KIND_UNSPECIFIED, len)
    }

    /// Like [`try_reserve`](Self::try_reserve), for a message of the given
    /// `kind`.
    pub fn try_reserve_kind(&self, kind: u16, len: usize) -> io::Result<MpSendSlot<'_>> {
        let lane = &self.shared.lane;
        if len as u64 > lane.max {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "message larger than max_message (use bulk)"));
        }
        self.check()?;
        loop {
            if let Some((_, idx)) = lane.mp_reserve(len as u64) {
                return Ok(MpSendSlot { sender: self, idx, len, kind, done: false });
            }
            if !self.arm(len as u64)? {
                return Err(crate::error::would_block());
            }
        }
    }

    /// The ring is full: announce that we wait for space, so the receiver
    /// rings our doorbell. `Ok(true)` if space appeared meanwhile.
    fn arm(&self, len: u64) -> io::Result<bool> {
        let lane = self.shared.lane;
        let room = || lane.mp_has_room(len);
        match self.shared.side.try_lock() {
            Ok(mut side) => match side.end.wait(MSG, true, room, || false) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
                Err(e) => Err(e),
            },
            Err(TryLockError::Poisoned(p)) => {
                drop(p);
                self.arm_unlocked(len)
            }
            // Another thread is waiting or sending bulk data; it owns the
            // doorbell's state. Set the flag ourselves, without touching it.
            Err(TryLockError::WouldBlock) => self.arm_unlocked(len),
        }
    }

    fn arm_unlocked(&self, len: u64) -> io::Result<bool> {
        let lane = &self.shared.lane;
        lane.tail_line().waiting.store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if lane.mp_has_room(len) {
            return Ok(true);
        }
        if self.shared.layout.meta().receiver_closed.load(Ordering::Acquire) != 0 {
            return Err(broken());
        }
        Ok(false)
    }

    /// Blocks until a `len`-byte message may fit (or the receiver is gone).
    fn wait_room(&self, len: u64) -> io::Result<()> {
        let lane = self.shared.lane;
        let backoff = crossbeam_utils::Backoff::new();
        while !backoff.is_completed() {
            if lane.mp_has_room(len) {
                return Ok(());
            }
            backoff.snooze();
        }
        let mut side = self.side();
        side.end.check_owner()?;
        side.end.wait(MSG, false, || lane.mp_has_room(len), || false)
    }

    /// Locks the bulk lane for this thread (other threads' bulk calls wait
    /// until the returned guard is dropped; messages are not affected).
    pub fn bulk(&self) -> MpBulk<'_> {
        let mut side = self.side();
        side.end.nonblocking = self.is_nonblocking();
        MpBulk { side }
    }

    /// Message ring size in bytes.
    pub fn capacity(&self) -> usize {
        self.shared.lane.cap as usize
    }

    /// Largest message in bytes.
    pub fn max_message(&self) -> usize {
        self.shared.lane.max as usize
    }

    /// Bulk ring size in bytes.
    pub fn bulk_capacity(&self) -> usize {
        self.shared.layout.bulk.cap as usize
    }

    /// Largest chunk of a bulk transfer in bytes.
    pub fn chunk(&self) -> usize {
        (self.shared.layout.bulk.max - super::lane::BULK_HDR) as usize
    }

    /// Switches between blocking (default) and non-blocking operation, for
    /// every clone.
    pub fn set_nonblocking(&self, nonblocking: bool) {
        self.shared.nonblocking.store(nonblocking, Ordering::Relaxed);
    }

    /// True in non-blocking mode.
    pub fn is_nonblocking(&self) -> bool {
        self.shared.nonblocking.load(Ordering::Relaxed)
    }

    /// Makes this process the owner of this end (after `fork`).
    pub fn claim(&self) {
        self.side().claim();
    }

    /// Makes this process's clones let go of the end without closing the
    /// channel when they are dropped (e.g. the copies a process keeps after
    /// `fork` but doesn't use). Affects every clone in this process.
    pub fn detach(self) {
        self.side().end.detached = true;
    }

    /// True once the receiver is gone: it was dropped, or the process that
    /// owns it has exited.
    pub fn peer_gone(&self) -> bool {
        let meta = self.shared.layout.meta();
        if meta.receiver_closed.load(Ordering::Acquire) != 0 {
            return true;
        }
        let pid = meta.receiver_pid.load(Ordering::Relaxed);
        pid != 0 && !process_alive(pid)
    }

    /// The process that owns the receiver (`None` while it is being handed
    /// to another process).
    pub fn peer_pid(&self) -> Option<u32> {
        Some(self.shared.layout.meta().receiver_pid.load(Ordering::Relaxed)).filter(|&pid| pid != 0)
    }
}

impl MpSendSlot<'_> {
    /// Publishes the first `n` bytes (at most the reserved length) as one
    /// message.
    ///
    /// # Panics
    /// If `n` is larger than the reserved length.
    pub fn commit(mut self, n: usize) {
        assert!(n <= self.len, "commit({n}) exceeds the reserved {} bytes", self.len);
        // SAFETY: our reservation of `record_len(len)` bytes at `idx`, with
        // the payload written; `record_len(n) <= record_len(len)`.
        unsafe { self.sender.shared.lane.mp_commit(self.idx, record_len(self.len as u64), n as u64, 0, self.kind) };
        self.done = true;
        self.sender.wake();
    }
}

impl Drop for MpSendSlot<'_> {
    fn drop(&mut self) {
        if !self.done {
            // SAFETY: our uncommitted reservation; padding it lets the
            // receiver move past it to messages reserved after it.
            unsafe { self.sender.shared.lane.mp_pad(self.idx, record_len(self.len as u64)) };
            self.sender.wake();
        }
    }
}

impl std::ops::Deref for MpSendSlot<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: reserved space owned by this slot until published.
        unsafe { std::slice::from_raw_parts(self.sender.shared.lane.payload(self.idx), self.len) }
    }
}

impl std::ops::DerefMut for MpSendSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; nobody else reads it before it is published.
        unsafe { std::slice::from_raw_parts_mut(self.sender.shared.lane.payload(self.idx), self.len) }
    }
}

impl MpBulk<'_> {
    /// [`GkSender::send_bulk`] on the locked bulk lane.
    pub fn send_bulk(&mut self, data: &[u8]) -> io::Result<u64> {
        self.side.send_bulk(data)
    }

    /// [`GkSender::send_bulk_kind`] on the locked bulk lane.
    pub fn send_bulk_kind(&mut self, kind: u16, data: &[u8]) -> io::Result<u64> {
        self.side.send_bulk_kind(kind, data)
    }

    /// [`GkSender::bulk_writer`] on the locked bulk lane.
    pub fn bulk_writer(&mut self, len: Option<u64>) -> io::Result<BulkWriter<'_>> {
        self.side.bulk_writer(len)
    }

    /// [`GkSender::bulk_writer_kind`] on the locked bulk lane.
    pub fn bulk_writer_kind(&mut self, kind: u16, len: Option<u64>) -> io::Result<BulkWriter<'_>> {
        self.side.bulk_writer_kind(kind, len)
    }

    /// [`GkSender::resume_bulk`] on the locked bulk lane.
    pub fn resume_bulk(&mut self) -> io::Result<BulkWriter<'_>> {
        self.side.resume_bulk()
    }
}

#[cfg(unix)]
mod unix_impls {
    use super::GkMpSender;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

    /// Readable when space may have been freed.
    impl AsFd for GkMpSender {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.shared.space_ready.as_fd()
        }
    }

    impl AsRawFd for GkMpSender {
        fn as_raw_fd(&self) -> RawFd {
            self.shared.space_ready.as_raw_fd()
        }
    }
}

#[cfg(windows)]
mod windows_impls {
    use super::GkMpSender;
    use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};

    /// Readable when space may have been freed (for `WSAPoll`/`select`; the
    /// sender has no mio `Source`).
    impl AsSocket for GkMpSender {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            self.shared.space_ready.as_socket()
        }
    }

    impl AsRawSocket for GkMpSender {
        fn as_raw_socket(&self) -> RawSocket {
            self.shared.space_ready.as_raw_socket()
        }
    }
}

/// Lets `GkProcess` move the end (see `process.rs`).
impl crate::process::private::Sealed for GkMpSender {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.side().end.send_over(sock)
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        Self::from_sender(GkSender::from_end(super::End::recv_over(sock, super::Side::Sender, true)?))
    }
    #[cfg(windows)]
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        self.side().end.offer(pid)
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        Self::from_sender(GkSender::from_end(super::End::take(token, pid, super::Side::Sender, true)?))
    }
}

impl crate::process::Shareable for GkMpSender {}

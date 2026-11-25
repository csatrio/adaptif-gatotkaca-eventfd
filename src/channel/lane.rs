//! The shared memory layout of a channel and the lock-free logic of one
//! lane (a ring of records with one consumer and one producer, or, for the
//! message lane of a multi-producer channel, several producer threads).
//!
//! Layout: `Header | message ring | bulk ring`. The header holds the sizes,
//! the owners and closed flags, and for each lane a tail line (written by
//! the sender) and a head line (written by the receiver), each on its own
//! cache line with the side's "I am waiting" flag.
//!
//! A record is `[len: u32][flags: u16][kind: u16][payload][padding to 8
//! bytes]` (`kind`: the application's payload type, 0 = unspecified) and is
//! always contiguous: if it doesn't fit before the end of the ring, a PAD
//! record fills the rest and the record starts at index 0. Bulk records
//! begin their payload with `[transfer id: u64][chunk seq: u64][total: u64]`.
//!
//! Multi-producer message lane (`Meta::mode == MODE_MPSC`): the tail line's
//! position is a reservation counter that producers advance with a CAS, and
//! each record is published by storing its 8-byte header last, as one atomic
//! word with the `COMMIT` flag set. The receiver reads the header at its
//! head: zero means "not written yet" (empty, or a producer still copying).
//! So that stale bytes never look like a header, the receiver zeroes every
//! byte it consumes before it frees the space. PAD records there carry their
//! own span (`record_len(len)`) instead of "to the end of the ring", which
//! also lets a producer shrink or abandon a reservation.
//!
//! A fresh layout starts with the receiver's waiting flags set, so the first
//! record rings its doorbell: a receiver registered with a poller before it
//! ever called `recv` still wakes.

use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// "GKC" + layout version 4 (two lanes, 16-bit kinds, mode word, receiver
/// starts waiting).
pub(super) const MAGIC: u32 = 0x474B_4304;
/// `Meta::mode`: one producer per lane.
pub(super) const MODE_SPSC: u32 = 0;
/// `Meta::mode`: the message lane takes several producer threads.
pub(super) const MODE_MPSC: u32 = 1;
/// Record header: `len: u32`, `flags: u16`, `kind: u16`.
pub(super) const REC_HDR: u64 = 8;
/// Bulk record sub-header: transfer id, chunk sequence, total length.
pub(super) const BULK_HDR: u64 = 24;

/// Record flags.
pub(super) mod flag {
    /// Padding up to the end of the ring: skip it.
    pub(crate) const PAD: u32 = 1;
    /// Bulk: first chunk of a transfer.
    pub(crate) const FIRST: u32 = 2;
    /// Bulk: last chunk of a transfer.
    pub(crate) const LAST: u32 = 4;
    /// Bulk: the transfer was abandoned by the sender.
    pub(crate) const ABORT: u32 = 8;
    /// Bulk: the `total` field is valid.
    pub(crate) const LEN_KNOWN: u32 = 16;
    /// Multi-producer lane: the header is written (never zero once set).
    pub(crate) const COMMIT: u32 = 0x8000;
}

/// The header of a record as one word: `len | flags << 32 | kind << 48`
/// (the same bytes as the `[len: u32][flags: u16][kind: u16]` layout on the
/// little-endian targets this crate supports).
fn pack(len: u64, flags: u32, kind: u16) -> u64 {
    debug_assert!(len <= u64::from(u32::MAX) && flags <= u32::from(u16::MAX));
    len | u64::from(flags) << 32 | u64::from(kind) << 48
}

fn unpack(w: u64) -> (u64, u32, u16) {
    (w & 0xffff_ffff, ((w >> 32) & 0xffff) as u32, (w >> 48) as u16)
}

/// Record size of a `len`-byte payload: header + payload rounded up to 8.
pub(super) fn record_len(len: u64) -> u64 {
    REC_HDR + ((len + 7) & !7)
}

/// A ring of `capacity` bytes can carry payloads of up to `max` bytes.
pub(super) fn valid_sizes(capacity: u64, max: u64) -> bool {
    capacity.is_power_of_two() && (64..=1 << 40).contains(&capacity) && max <= u64::from(u32::MAX) && record_len(max) <= capacity / 2
}

#[repr(C, align(64))]
#[derive(Debug)]
pub(super) struct Meta {
    pub(super) magic: AtomicU32,
    /// `MODE_SPSC` or `MODE_MPSC` (the message lane's producers).
    pub(super) mode: AtomicU32,
    pub(super) msg_capacity: AtomicU64,
    pub(super) max_message: AtomicU64,
    pub(super) bulk_capacity: AtomicU64,
    pub(super) chunk: AtomicU64,
    /// Owning process of each end (0: in transit to another process).
    pub(super) sender_pid: AtomicU32,
    pub(super) receiver_pid: AtomicU32,
    pub(super) sender_closed: AtomicU32,
    pub(super) receiver_closed: AtomicU32,
}

/// One side's position in a lane and its "I am waiting" flag.
#[repr(C, align(64))]
#[derive(Debug)]
pub(super) struct Line {
    pub(super) pos: AtomicU64,
    pub(super) waiting: AtomicU32,
}

#[repr(C)]
#[derive(Debug)]
pub(super) struct Header {
    pub(super) meta: Meta,
    pub(super) msg_tail: Line,
    pub(super) msg_head: Line,
    pub(super) bulk_tail: Line,
    pub(super) bulk_head: Line,
}

/// Offset of the message ring (the header is a multiple of 64 bytes).
pub(super) const DATA: usize = size_of::<Header>();

/// One record found by [`Lane::peek`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Rec {
    pub(super) ptr: *const u8,
    pub(super) len: usize,
    pub(super) flags: u32,
    /// The application's payload type (0: unspecified).
    pub(super) kind: u16,
    /// The head after this record.
    pub(super) next: u64,
}

/// One lane: its two lines and its ring.
#[derive(Debug, Clone, Copy)]
pub(super) struct Lane {
    tail: NonNull<Line>,
    head: NonNull<Line>,
    data: NonNull<u8>,
    pub(super) cap: u64,
    /// Largest record payload.
    pub(super) max: u64,
    /// Multi-producer protocol (see the module docs).
    pub(super) mp: bool,
}

// SAFETY: a lane only points into shared memory accessed through atomics and
// the record protocol; the owner keeps the mapping alive.
unsafe impl Send for Lane {}
// SAFETY: as above.
unsafe impl Sync for Lane {}

impl Lane {
    pub(super) fn tail_line(&self) -> &Line {
        // SAFETY: valid while the owning mapping lives (see `Layout::new`).
        unsafe { self.tail.as_ref() }
    }

    pub(super) fn head_line(&self) -> &Line {
        // SAFETY: as above.
        unsafe { self.head.as_ref() }
    }

    /// True if something was published after `head` (a record, or the
    /// ring is inconsistent): what `peek(head)` would not answer `None` to,
    /// give or take a multi-producer PAD record.
    pub(super) fn has_data(&self, head: u64) -> bool {
        if self.mp {
            self.header(head & (self.cap - 1)).load(Ordering::Acquire) != 0
        } else {
            self.tail_line().pos.load(Ordering::Acquire) != head
        }
    }

    /// The header word of the record at ring index `idx` (8-aligned).
    fn header(&self, idx: u64) -> &AtomicU64 {
        debug_assert!(idx % 8 == 0 && idx < self.cap);
        // SAFETY: the ring is 64-byte aligned and `idx` is an 8-aligned index
        // inside it; shared memory is accessed through atomics here.
        unsafe { &*self.data.as_ptr().add(idx as usize).cast::<AtomicU64>() }
    }

    /// Where a `len`-byte payload can go, if there is room: `(pad, index)`.
    pub(super) fn reserve(&self, tail: u64, len: u64) -> Option<(u64, u64)> {
        let need = record_len(len);
        let head = self.head_line().pos.load(Ordering::Acquire);
        let free = self.cap.checked_sub(tail.wrapping_sub(head))?;
        let idx = tail & (self.cap - 1);
        let to_end = self.cap - idx;
        if need <= to_end { (need <= free).then_some((0, idx)) } else { (to_end + need <= free).then_some((to_end, 0)) }
    }

    /// The payload area of a record starting at ring index `idx`.
    pub(super) fn payload(&self, idx: u64) -> *mut u8 {
        // SAFETY: `idx + 8 + payload <= cap` was checked by `reserve`.
        unsafe { self.data.as_ptr().add((idx + REC_HDR) as usize) }
    }

    /// # Safety
    /// `idx` must be 8-aligned and leave room for a record header.
    unsafe fn write_header(&self, idx: u64, len: u32, flags: u32, kind: u16) {
        debug_assert!(flags <= u32::from(u16::MAX));
        // SAFETY: forwarded; the area is the sender's until published.
        unsafe {
            let p = self.data.as_ptr().add(idx as usize);
            p.cast::<u32>().write(len);
            p.add(4).cast::<u16>().write(flags as u16);
            p.add(6).cast::<u16>().write(kind);
        }
    }

    /// Publishes a `len`-byte payload reserved at (`pad`, `idx`) from `tail`
    /// and returns the new tail.
    ///
    /// # Safety
    /// (`pad`, `idx`) must come from `reserve(tail, reserved)` with
    /// `len <= reserved`, and the payload must already be written.
    pub(super) unsafe fn publish(&self, tail: u64, pad: u64, idx: u64, len: u64, flags: u32, kind: u16) -> u64 {
        // SAFETY: forwarded; headers lie inside the reserved space.
        unsafe {
            if pad > 0 {
                self.write_header(tail & (self.cap - 1), 0, flag::PAD, 0);
            }
            self.write_header(idx, len as u32, flags, kind);
        }
        let new_tail = tail + pad + record_len(len);
        self.tail_line().pos.store(new_tail, Ordering::Release);
        new_tail
    }

    /// The next record at `head`, or `None` if the lane is empty. Fails on
    /// a corrupted ring rather than reading out of bounds.
    pub(super) fn peek(&self, head: u64) -> io::Result<Option<Rec>> {
        if self.mp { self.peek_mp(head) } else { self.peek_sp(head) }
    }

    fn peek_sp(&self, mut head: u64) -> io::Result<Option<Rec>> {
        let tail = self.tail_line().pos.load(Ordering::Acquire);
        loop {
            if head == tail {
                return Ok(None);
            }
            if tail.wrapping_sub(head) > self.cap || head % 8 != 0 {
                return Err(corrupt());
            }
            let idx = head & (self.cap - 1);
            // SAFETY: `idx` is 8-aligned and inside the ring; the record was
            // published (Acquire above) and is not reused until released.
            let (len, flags, kind) = unsafe {
                let p = self.data.as_ptr().add(idx as usize);
                (u64::from(p.cast::<u32>().read()), u32::from(p.add(4).cast::<u16>().read()), p.add(6).cast::<u16>().read())
            };
            if flags & flag::PAD != 0 {
                head += self.cap - idx;
                continue;
            }
            let rec = record_len(len);
            if len > self.max || rec > self.cap - idx || rec > tail - head {
                return Err(corrupt());
            }
            return Ok(Some(Rec { ptr: self.payload(idx), len: len as usize, flags, kind, next: head + rec }));
        }
    }

    fn peek_mp(&self, mut head: u64) -> io::Result<Option<Rec>> {
        loop {
            if head % 8 != 0 {
                return Err(corrupt());
            }
            let idx = head & (self.cap - 1);
            let w = self.header(idx).load(Ordering::Acquire);
            if w == 0 {
                return Ok(None);
            }
            let (len, flags, kind) = unpack(w);
            let rec = record_len(len);
            // A committed record lies inside what producers reserved.
            let reserved = self.tail_line().pos.load(Ordering::Acquire);
            if flags & flag::COMMIT == 0 || rec > self.cap - idx || rec > reserved.wrapping_sub(head) {
                return Err(corrupt());
            }
            if flags & flag::PAD != 0 {
                head += rec;
                continue;
            }
            if len > self.max {
                return Err(corrupt());
            }
            return Ok(Some(Rec { ptr: self.payload(idx), len: len as usize, flags: flags & !flag::COMMIT, kind, next: head + rec }));
        }
    }

    /// Frees everything from `old_head` (the previous head) to `new_head`.
    /// A multi-producer lane is zeroed first (see the module docs).
    pub(super) fn release(&self, old_head: u64, new_head: u64) {
        if self.mp {
            let mut pos = old_head;
            while pos != new_head {
                let idx = pos & (self.cap - 1);
                let n = (new_head - pos).min(self.cap - idx);
                // SAFETY: `[idx, idx + n)` is inside the ring and consumed:
                // no producer writes it before the head moves past it below.
                unsafe { std::ptr::write_bytes(self.data.as_ptr().add(idx as usize), 0, n as usize) };
                pos += n;
            }
        }
        self.head_line().pos.store(new_head, Ordering::Release);
    }

    /// Multi-producer: claims room for a `len`-byte payload. Returns the
    /// record's position and ring index; a PAD up to the end of the ring is
    /// already committed if the record wraps.
    pub(super) fn mp_reserve(&self, len: u64) -> Option<(u64, u64)> {
        debug_assert!(self.mp);
        let need = record_len(len);
        let line = &self.tail_line().pos;
        loop {
            // Head first: a tail loaded after it is never behind it.
            let head = self.head_line().pos.load(Ordering::Acquire);
            let tail = line.load(Ordering::Relaxed);
            let free = self.cap.checked_sub(tail.wrapping_sub(head))?;
            let idx = tail & (self.cap - 1);
            let to_end = self.cap - idx;
            let pad = if need <= to_end { 0 } else { to_end };
            if pad + need > free {
                return None;
            }
            match line.compare_exchange_weak(tail, tail + pad + need, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    if pad > 0 {
                        // SAFETY: `[idx, cap)` was just reserved by us.
                        unsafe { self.mp_pad(idx, pad) };
                    }
                    return Some((tail + pad, if pad > 0 { 0 } else { idx }));
                }
                Err(_) => std::hint::spin_loop(),
            }
        }
    }

    /// Multi-producer: true if `mp_reserve(len)` would find room now.
    pub(super) fn mp_has_room(&self, len: u64) -> bool {
        let need = record_len(len);
        let head = self.head_line().pos.load(Ordering::Acquire);
        let tail = self.tail_line().pos.load(Ordering::Acquire);
        let Some(free) = self.cap.checked_sub(tail.wrapping_sub(head)) else { return false };
        let to_end = self.cap - (tail & (self.cap - 1));
        (if need <= to_end { need } else { to_end + need }) <= free
    }

    /// Multi-producer: commits a PAD record of `span` bytes at `idx`.
    ///
    /// # Safety
    /// `[idx, idx + span)` must be reserved by the caller and not committed
    /// yet; `span` a multiple of 8, at least 8.
    pub(super) unsafe fn mp_pad(&self, idx: u64, span: u64) {
        debug_assert!(span >= REC_HDR && span % 8 == 0);
        self.header(idx).store(pack(span - REC_HDR, flag::PAD | flag::COMMIT, 0), Ordering::Release);
    }

    /// Multi-producer: publishes a `len`-byte payload at `idx`, already
    /// written, in a reservation of `span` bytes (the rest becomes PAD).
    ///
    /// # Safety
    /// `[idx, idx + span)` must be reserved by the caller and not committed
    /// yet, with `record_len(len) <= span`.
    pub(super) unsafe fn mp_commit(&self, idx: u64, span: u64, len: u64, flags: u32, kind: u16) {
        let rec = record_len(len);
        debug_assert!(rec <= span);
        if rec < span {
            // SAFETY: forwarded; the rest of the reservation, published first
            // so the receiver finds it when it moves past this record.
            unsafe { self.mp_pad(idx + rec, span - rec) };
        }
        self.header(idx).store(pack(len, flags | flag::COMMIT, kind), Ordering::Release);
    }
}

fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "corrupted channel ring")
}

/// Sizes stored in a channel's header.
#[derive(Debug, Clone, Copy)]
pub(super) struct Sizes {
    pub(super) msg_capacity: u64,
    pub(super) max_message: u64,
    pub(super) bulk_capacity: u64,
    pub(super) chunk: u64,
}

impl Sizes {
    pub(super) fn valid(&self) -> bool {
        valid_sizes(self.msg_capacity, self.max_message) && valid_sizes(self.bulk_capacity, self.chunk + BULK_HDR)
    }

    pub(super) fn region_len(&self) -> u64 {
        DATA as u64 + self.msg_capacity + self.bulk_capacity
    }
}

/// A mapped channel layout.
#[derive(Debug, Clone, Copy)]
pub(super) struct Layout {
    hdr: NonNull<Header>,
    pub(super) msg: Lane,
    pub(super) bulk: Lane,
}

// SAFETY: see `Lane`.
unsafe impl Send for Layout {}
// SAFETY: see `Lane`.
unsafe impl Sync for Layout {}

impl Layout {
    /// Writes the header of a fresh layout (zeroed memory, not yet shared).
    ///
    /// # Safety
    /// `base` must be 64-byte aligned, zeroed and `sizes.region_len()` long.
    pub(super) unsafe fn init(base: NonNull<u8>, sizes: Sizes, pid: u32, mode: u32) {
        // SAFETY: forwarded.
        let hdr = unsafe { base.cast::<Header>().as_ref() };
        // The receiver starts out waiting: the first record rings its bell.
        hdr.msg_head.waiting.store(1, Ordering::Relaxed);
        hdr.bulk_head.waiting.store(1, Ordering::Relaxed);
        let meta = &hdr.meta;
        meta.mode.store(mode, Ordering::Relaxed);
        meta.msg_capacity.store(sizes.msg_capacity, Ordering::Relaxed);
        meta.max_message.store(sizes.max_message, Ordering::Relaxed);
        meta.bulk_capacity.store(sizes.bulk_capacity, Ordering::Relaxed);
        meta.chunk.store(sizes.chunk, Ordering::Relaxed);
        meta.sender_pid.store(pid, Ordering::Relaxed);
        meta.receiver_pid.store(pid, Ordering::Relaxed);
        meta.magic.store(MAGIC, Ordering::Release);
    }

    /// Reads the sizes of a mapped layout.
    ///
    /// # Safety
    /// `base` must point to at least `DATA` mapped, 64-byte aligned bytes.
    unsafe fn sizes(base: NonNull<u8>) -> Sizes {
        // SAFETY: forwarded.
        let meta = unsafe { &base.cast::<Header>().as_ref().meta };
        Sizes {
            msg_capacity: meta.msg_capacity.load(Ordering::Relaxed),
            max_message: meta.max_message.load(Ordering::Relaxed),
            bulk_capacity: meta.bulk_capacity.load(Ordering::Relaxed),
            chunk: meta.chunk.load(Ordering::Relaxed),
        }
    }

    /// Accepts a mapped object (`ptr`, `len`) as a channel layout.
    pub(super) fn accepts(ptr: NonNull<u8>, len: usize) -> bool {
        if len < DATA {
            return false;
        }
        // SAFETY: at least a header's worth of mapped, page-aligned memory.
        let (magic, mode, sizes) = unsafe {
            let meta = &ptr.cast::<Header>().as_ref().meta;
            (meta.magic.load(Ordering::Acquire), meta.mode.load(Ordering::Relaxed), Self::sizes(ptr))
        };
        magic == MAGIC && mode <= MODE_MPSC && sizes.valid() && sizes.region_len() <= len as u64
    }

    /// # Safety
    /// `base` must point to an initialised, validated layout that stays
    /// mapped while the result is used.
    pub(super) unsafe fn new(base: NonNull<u8>) -> Self {
        // SAFETY: forwarded.
        let sizes = unsafe { Self::sizes(base) };
        let hdr = base.cast::<Header>();
        // SAFETY: forwarded.
        let mp = unsafe { hdr.as_ref().meta.mode.load(Ordering::Relaxed) } == MODE_MPSC;
        // SAFETY: forwarded; the rings follow the header in the mapping.
        unsafe {
            let h = hdr.as_ptr();
            let msg_data = base.add(DATA);
            let bulk_data = msg_data.add(sizes.msg_capacity as usize);
            let lane = |tail: *mut Line, head: *mut Line, data, cap, max, mp| Lane {
                tail: NonNull::new_unchecked(tail),
                head: NonNull::new_unchecked(head),
                data,
                cap,
                max,
                mp,
            };
            Self {
                hdr,
                msg: lane(&raw mut (*h).msg_tail, &raw mut (*h).msg_head, msg_data, sizes.msg_capacity, sizes.max_message, mp),
                bulk: lane(&raw mut (*h).bulk_tail, &raw mut (*h).bulk_head, bulk_data, sizes.bulk_capacity, sizes.chunk + BULK_HDR, false),
            }
        }
    }

    pub(super) fn meta(&self) -> &Meta {
        // SAFETY: valid while the owning mapping lives.
        unsafe { &self.hdr.as_ref().meta }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A channel layout in ordinary heap memory (no OS; runs under Miri).
    pub(crate) struct HeapLayout {
        _buf: Vec<u64>,
        pub(crate) layout: Layout,
    }

    pub(crate) fn heap_layout(sizes: Sizes) -> HeapLayout {
        heap_layout_mode(sizes, MODE_SPSC)
    }

    pub(crate) fn heap_layout_mode(sizes: Sizes, mode: u32) -> HeapLayout {
        assert!(sizes.valid());
        let mut buf = vec![0u64; sizes.region_len() as usize / 8 + 8];
        let base = buf.as_mut_ptr().cast::<u8>();
        // Keep the allocation's provenance (no integer-to-pointer casts).
        // SAFETY: the offset (< 64) stays inside the over-allocated buffer.
        let ptr = NonNull::new(unsafe { base.add(base.align_offset(64)) }).unwrap();
        // SAFETY: zeroed, aligned, large enough; then initialised.
        let layout = unsafe {
            Layout::init(ptr, sizes, 1, mode);
            Layout::new(ptr)
        };
        HeapLayout { _buf: buf, layout }
    }

    fn sizes(msg: u64, max: u64) -> Sizes {
        Sizes { msg_capacity: msg, max_message: max, bulk_capacity: 256, chunk: 64 }
    }

    pub(crate) fn put(l: &Lane, tail: &mut u64, msg: &[u8]) -> bool {
        match l.reserve(*tail, msg.len() as u64) {
            Some((pad, idx)) => {
                // SAFETY: reserved space of at least msg.len() bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(msg.as_ptr(), l.payload(idx), msg.len());
                    *tail = l.publish(*tail, pad, idx, msg.len() as u64, 0, 0);
                }
                true
            }
            None => false,
        }
    }

    pub(crate) fn take(l: &Lane, head: &mut u64) -> Option<Vec<u8>> {
        let r = l.peek(*head).unwrap()?;
        // SAFETY: a published record of `len` bytes.
        let msg = unsafe { std::slice::from_raw_parts(r.ptr, r.len) }.to_vec();
        l.release(*head, r.next);
        *head = r.next;
        Some(msg)
    }

    /// Multi-producer put: reserve, copy, commit.
    pub(crate) fn mp_put(l: &Lane, msg: &[u8], kind: u16) -> bool {
        let Some((_, idx)) = l.mp_reserve(msg.len() as u64) else { return false };
        // SAFETY: reserved `record_len(len)` bytes at `idx`.
        unsafe {
            std::ptr::copy_nonoverlapping(msg.as_ptr(), l.payload(idx), msg.len());
            l.mp_commit(idx, record_len(msg.len() as u64), msg.len() as u64, 0, kind);
        }
        true
    }

    #[test]
    fn size_rules() {
        assert!(valid_sizes(16 << 20, 1 << 20));
        assert!(valid_sizes(64, 24));
        assert!(!valid_sizes(64, 25));
        assert!(!valid_sizes(100, 8));
        assert!(!valid_sizes(32, 8));
        assert_eq!(record_len(0), 8);
        assert_eq!(record_len(1), 16);
        assert_eq!(DATA % 64, 0);
        // Bulk lane: chunk + 24-byte sub-header must fit half the ring.
        assert!(sizes(256, 64).valid());
        let s = Sizes { bulk_capacity: 128, ..sizes(256, 64) };
        assert!(!s.valid());
    }

    #[test]
    fn lanes_are_independent() {
        let h = heap_layout(sizes(256, 64));
        let (msg, bulk) = (h.layout.msg, h.layout.bulk);
        let (mut mt, mut mh, mut bt, mut bh) = (0, 0, 0, 0);
        assert!(put(&msg, &mut mt, b"msg"));
        assert!(put(&bulk, &mut bt, b"bulk"));
        assert!(msg.has_data(mh) && bulk.has_data(bh));
        assert_eq!(take(&bulk, &mut bh).unwrap(), b"bulk");
        assert_eq!(take(&msg, &mut mh).unwrap(), b"msg");
        assert!(!msg.has_data(mh) && !bulk.has_data(bh));
    }

    #[test]
    fn fifo_with_wraparound_and_zero_length() {
        let h = heap_layout(sizes(256, 64));
        let l = &h.layout.msg;
        let (mut tail, mut head) = (0u64, 0u64);
        let mut expect = std::collections::VecDeque::new();
        for i in 0..2000u32 {
            let len = (i * 7 % 65) as usize;
            let msg: Vec<u8> = (0..len).map(|b| (b as u32 ^ i) as u8).collect();
            while !put(l, &mut tail, &msg) {
                assert_eq!(take(l, &mut head).as_ref(), expect.front());
                expect.pop_front();
            }
            expect.push_back(msg);
        }
        while let Some(m) = take(l, &mut head) {
            assert_eq!(Some(&m), expect.front());
            expect.pop_front();
        }
        assert!(expect.is_empty());
        assert_eq!(head, tail);
    }

    #[test]
    fn full_ring_refuses_until_space_is_freed() {
        let h = heap_layout(sizes(64, 24));
        let l = &h.layout.msg;
        let (mut tail, mut head) = (0u64, 0u64);
        assert!(put(l, &mut tail, &[1; 24]));
        assert!(put(l, &mut tail, &[2; 24]));
        assert!(!put(l, &mut tail, &[]));
        assert_eq!(take(l, &mut head).unwrap(), vec![1; 24]);
        assert!(put(l, &mut tail, &[3; 8]));
        assert_eq!(take(l, &mut head).unwrap(), vec![2; 24]);
        assert_eq!(take(l, &mut head).unwrap(), vec![3; 8]);
        assert!(take(l, &mut head).is_none());
    }

    #[test]
    fn kinds_and_flags_round_trip_across_the_wrap() {
        let h = heap_layout(sizes(256, 64));
        let l = &h.layout.msg;
        let (mut tail, mut head) = (0u64, 0u64);
        for i in 0..500u32 {
            let len = u64::from(i * 13 % 57);
            let kind = (i.wrapping_mul(40503) & 0xffff) as u16;
            let flags = i & 0xfff0; // high bits; PAD (1) stays reserved
            let (pad, idx) = l.reserve(tail, len).expect("ring drained each round");
            // SAFETY: `len` bytes reserved at `idx`.
            tail = unsafe { l.publish(tail, pad, idx, len, flags, kind) };
            let r = l.peek(head).unwrap().expect("published");
            assert_eq!((r.len as u64, r.flags, r.kind), (len, flags, kind));
            l.release(head, r.next);
            head = r.next;
        }
        assert_eq!(head, tail);
    }

    #[test]
    fn corrupted_headers_are_rejected() {
        let h = heap_layout(sizes(256, 64));
        let l = &h.layout.msg;
        let mut tail = 0;
        assert!(put(l, &mut tail, b"hi"));
        // SAFETY: overwrite the record's length field in our heap layout.
        unsafe { l.payload(0).sub(REC_HDR as usize).cast::<u32>().write(1000) };
        assert_eq!(l.peek(0).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// Writer and reader threads over one lane, random message sizes.
    #[test]
    fn two_threads() {
        let h = heap_layout(Sizes { msg_capacity: 1024, max_message: 200, bulk_capacity: 256, chunk: 64 });
        let l = h.layout.msg;
        let n: u32 = if cfg!(miri) { 300 } else { 100_000 };
        let size = |i: u32| (i.wrapping_mul(2654435761) % 201) as usize;
        let writer = std::thread::spawn(move || {
            let mut tail = 0;
            for i in 0..n {
                let msg = vec![i as u8; size(i)];
                while !put(&l, &mut tail, &msg) {
                    std::thread::yield_now();
                }
            }
        });
        let mut head = 0;
        for i in 0..n {
            let msg = loop {
                if let Some(m) = take(&l, &mut head) {
                    break m;
                }
                std::thread::yield_now();
            };
            assert_eq!(msg.len(), size(i));
            assert!(msg.iter().all(|&b| b == i as u8));
        }
        writer.join().unwrap();
        drop(h);
    }

    // -- multi-producer lane -------------------------------------------------

    fn mp_layout(msg: u64, max: u64) -> HeapLayout {
        heap_layout_mode(sizes(msg, max), MODE_MPSC)
    }

    #[test]
    fn layout_starts_with_the_receiver_waiting() {
        let h = heap_layout(sizes(256, 64));
        assert_eq!(h.layout.msg.head_line().waiting.load(Ordering::Relaxed), 1);
        assert_eq!(h.layout.bulk.head_line().waiting.load(Ordering::Relaxed), 1);
        assert_eq!(h.layout.msg.tail_line().waiting.load(Ordering::Relaxed), 0);
        assert!(!h.layout.msg.mp && !h.layout.bulk.mp);
        let m = mp_layout(256, 64);
        assert!(m.layout.msg.mp && !m.layout.bulk.mp);
    }

    #[test]
    fn mp_fifo_with_wraparound_zeroes_what_it_consumes() {
        let h = mp_layout(256, 64);
        let l = &h.layout.msg;
        let mut head = 0u64;
        let mut expect = std::collections::VecDeque::new();
        for i in 0..2000u32 {
            let len = (i * 7 % 65) as usize;
            let msg: Vec<u8> = (0..len).map(|b| (b as u32 ^ i) as u8 | 1).collect();
            while !mp_put(l, &msg, i as u16) {
                assert!(l.has_data(head));
                assert_eq!(take(l, &mut head).as_ref(), expect.front());
                expect.pop_front();
            }
            expect.push_back(msg);
        }
        while let Some(m) = take(l, &mut head) {
            assert_eq!(Some(&m), expect.front());
            expect.pop_front();
        }
        assert!(expect.is_empty() && !l.has_data(head));
        assert_eq!(head, l.tail_line().pos.load(Ordering::Relaxed));
        // Everything consumed was zeroed: no stale byte can pass for a header.
        // SAFETY: the whole ring of our heap layout.
        let ring = unsafe { std::slice::from_raw_parts(l.data.as_ptr(), l.cap as usize) };
        assert!(ring.iter().all(|&b| b == 0));
    }

    #[test]
    fn mp_uncommitted_record_blocks_later_ones_until_committed() {
        let h = mp_layout(256, 64);
        let l = &h.layout.msg;
        let (_, a) = l.mp_reserve(5).unwrap();
        assert!(mp_put(l, b"second", 2));
        assert!(!l.has_data(0) && l.peek(0).unwrap().is_none());
        // SAFETY: `a` holds a reservation of record_len(5) bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(b"first".as_ptr(), l.payload(a), 5);
            l.mp_commit(a, record_len(5), 5, 0, 1);
        }
        let mut head = 0;
        assert_eq!(take(l, &mut head).unwrap(), b"first");
        assert_eq!(take(l, &mut head).unwrap(), b"second");
        assert!(take(l, &mut head).is_none());
    }

    #[test]
    fn mp_shrunk_and_abandoned_reservations_become_padding() {
        let h = mp_layout(256, 64);
        let l = &h.layout.msg;
        let (_, a) = l.mp_reserve(40).unwrap();
        let (_, b) = l.mp_reserve(16).unwrap();
        assert!(mp_put(l, b"after", 3));
        // SAFETY: our reservations; `a` shrinks 40 -> 3 bytes, `b` is dropped.
        unsafe {
            std::ptr::copy_nonoverlapping(b"abc".as_ptr(), l.payload(a), 3);
            l.mp_commit(a, record_len(40), 3, 0, 1);
            l.mp_pad(b, record_len(16));
        }
        let mut head = 0;
        let r = l.peek(head).unwrap().unwrap();
        assert_eq!((r.len, r.kind, r.flags), (3, 1, 0));
        assert_eq!(take(l, &mut head).unwrap(), b"abc");
        assert_eq!(take(l, &mut head).unwrap(), b"after");
        assert!(take(l, &mut head).is_none());
        assert_eq!(head, l.tail_line().pos.load(Ordering::Relaxed));
    }

    #[test]
    fn mp_full_ring_refuses_and_room_check_agrees() {
        let h = mp_layout(64, 24);
        let l = &h.layout.msg;
        assert!(l.mp_has_room(24) && mp_put(l, &[1; 24], 0));
        assert!(l.mp_has_room(24) && mp_put(l, &[2; 24], 0));
        assert!(!l.mp_has_room(0) && !mp_put(l, &[], 0));
        let mut head = 0;
        assert_eq!(take(l, &mut head).unwrap(), vec![1; 24]);
        assert!(l.mp_has_room(8) && mp_put(l, &[3; 8], 0));
        assert_eq!(take(l, &mut head).unwrap(), vec![2; 24]);
        assert_eq!(take(l, &mut head).unwrap(), vec![3; 8]);
    }

    #[test]
    fn mp_corrupted_headers_are_rejected() {
        let h = mp_layout(256, 64);
        let l = &h.layout.msg;
        assert!(mp_put(l, b"hi", 0));
        // A header without COMMIT, then one past the reservation.
        l.header(0).store(pack(2, 0, 0), Ordering::Relaxed);
        assert_eq!(l.peek(0).unwrap_err().kind(), io::ErrorKind::InvalidData);
        l.header(0).store(pack(64, flag::COMMIT, 0), Ordering::Relaxed);
        assert_eq!(l.peek(0).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// Several producer threads, one consumer: every message arrives once,
    /// and each producer's messages arrive in its own order.
    #[test]
    fn mp_threads() {
        let h = mp_layout(1024, 200);
        let l = h.layout.msg;
        let producers: u32 = 4;
        let n: u32 = if cfg!(miri) { 60 } else { 50_000 };
        let size = |p: u32, i: u32| ((p * 31 + i).wrapping_mul(2654435761) % 193) as usize + 8;
        let threads: Vec<_> = (0..producers)
            .map(|p| {
                std::thread::spawn(move || {
                    for i in 0..n {
                        let mut msg = vec![p as u8; size(p, i)];
                        msg[..4].copy_from_slice(&i.to_le_bytes());
                        while !mp_put(&l, &msg, p as u16) {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        let mut next = vec![0u32; producers as usize];
        let mut head = 0;
        for _ in 0..producers * n {
            let (kind, msg) = loop {
                if let Some(r) = l.peek(head).unwrap() {
                    // SAFETY: a published record of `len` bytes.
                    let msg = unsafe { std::slice::from_raw_parts(r.ptr, r.len) }.to_vec();
                    l.release(head, r.next);
                    head = r.next;
                    break (r.kind, msg);
                }
                std::thread::yield_now();
            };
            let p = u32::from(kind);
            let i = u32::from_le_bytes(msg[..4].try_into().unwrap());
            assert_eq!(i, next[p as usize], "producer {p} out of order");
            assert_eq!(msg.len(), size(p, i));
            assert!(msg[4..].iter().all(|&b| b == p as u8));
            next[p as usize] += 1;
        }
        for t in threads {
            t.join().unwrap();
        }
        assert!(next.iter().all(|&c| c == n));
        drop(h);
    }
}

//! The shared memory layout of a channel and the lock-free logic of one
//! lane (a single-producer/single-consumer ring of records).
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

use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// "GKC" + layout version 3 (two lanes, 16-bit kinds).
pub(super) const MAGIC: u32 = 0x474B_4303;
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
}

/// Record size of a `len`-byte payload: header + payload rounded up to 8.
pub(super) fn record_len(len: u64) -> u64 {
    REC_HDR + ((len + 7) & !7)
}

/// A ring of `capacity` bytes can carry payloads of up to `max` bytes.
pub(super) fn valid_sizes(capacity: u64, max: u64) -> bool {
    capacity.is_power_of_two()
        && (64..=1 << 40).contains(&capacity)
        && max <= u64::from(u32::MAX)
        && record_len(max) <= capacity / 2
}

#[repr(C, align(64))]
#[derive(Debug)]
pub(super) struct Meta {
    pub(super) magic: AtomicU32,
    _reserved: AtomicU32,
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

    /// True if a published record is waiting (or the ring is inconsistent).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn has_data(&self) -> bool {
        self.tail_line().pos.load(Ordering::Acquire) != self.head_line().pos.load(Ordering::Acquire)
    }

    /// Where a `len`-byte payload can go, if there is room: `(pad, index)`.
    pub(super) fn reserve(&self, tail: u64, len: u64) -> Option<(u64, u64)> {
        let need = record_len(len);
        let head = self.head_line().pos.load(Ordering::Acquire);
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
    pub(super) unsafe fn publish(
        &self,
        tail: u64,
        pad: u64,
        idx: u64,
        len: u64,
        flags: u32,
        kind: u16,
    ) -> u64 {
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
    pub(super) fn peek(&self, mut head: u64) -> io::Result<Option<Rec>> {
        let corrupt = || io::Error::new(io::ErrorKind::InvalidData, "corrupted channel ring");
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
                (
                    u64::from(p.cast::<u32>().read()),
                    u32::from(p.add(4).cast::<u16>().read()),
                    p.add(6).cast::<u16>().read(),
                )
            };
            if flags & flag::PAD != 0 {
                head += self.cap - idx;
                continue;
            }
            let rec = record_len(len);
            if len > self.max || rec > self.cap - idx || rec > tail - head {
                return Err(corrupt());
            }
            return Ok(Some(Rec {
                ptr: self.payload(idx),
                len: len as usize,
                flags,
                kind,
                next: head + rec,
            }));
        }
    }

    /// Frees everything before `new_head`.
    pub(super) fn release(&self, new_head: u64) {
        self.head_line().pos.store(new_head, Ordering::Release);
    }
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
        valid_sizes(self.msg_capacity, self.max_message)
            && valid_sizes(self.bulk_capacity, self.chunk + BULK_HDR)
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
    pub(super) unsafe fn init(base: NonNull<u8>, sizes: Sizes, pid: u32) {
        // SAFETY: forwarded.
        let meta = unsafe { &base.cast::<Header>().as_ref().meta };
        meta.msg_capacity
            .store(sizes.msg_capacity, Ordering::Relaxed);
        meta.max_message.store(sizes.max_message, Ordering::Relaxed);
        meta.bulk_capacity
            .store(sizes.bulk_capacity, Ordering::Relaxed);
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
        let (magic, sizes) = unsafe {
            (
                ptr.cast::<Header>()
                    .as_ref()
                    .meta
                    .magic
                    .load(Ordering::Acquire),
                Self::sizes(ptr),
            )
        };
        magic == MAGIC && sizes.valid() && sizes.region_len() <= len as u64
    }

    /// # Safety
    /// `base` must point to an initialised, validated layout that stays
    /// mapped while the result is used.
    pub(super) unsafe fn new(base: NonNull<u8>) -> Self {
        // SAFETY: forwarded.
        let sizes = unsafe { Self::sizes(base) };
        let hdr = base.cast::<Header>();
        // SAFETY: forwarded; the rings follow the header in the mapping.
        unsafe {
            let h = hdr.as_ptr();
            let msg_data = base.add(DATA);
            let bulk_data = msg_data.add(sizes.msg_capacity as usize);
            let lane = |tail: *mut Line, head: *mut Line, data, cap, max| Lane {
                tail: NonNull::new_unchecked(tail),
                head: NonNull::new_unchecked(head),
                data,
                cap,
                max,
            };
            Self {
                hdr,
                msg: lane(
                    &raw mut (*h).msg_tail,
                    &raw mut (*h).msg_head,
                    msg_data,
                    sizes.msg_capacity,
                    sizes.max_message,
                ),
                bulk: lane(
                    &raw mut (*h).bulk_tail,
                    &raw mut (*h).bulk_head,
                    bulk_data,
                    sizes.bulk_capacity,
                    sizes.chunk + BULK_HDR,
                ),
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
        assert!(sizes.valid());
        let mut buf = vec![0u64; sizes.region_len() as usize / 8 + 8];
        let base = buf.as_mut_ptr().cast::<u8>();
        // Keep the allocation's provenance (no integer-to-pointer casts).
        // SAFETY: the offset (< 64) stays inside the over-allocated buffer.
        let ptr = NonNull::new(unsafe { base.add(base.align_offset(64)) }).unwrap();
        // SAFETY: zeroed, aligned, large enough; then initialised.
        let layout = unsafe {
            Layout::init(ptr, sizes, 1);
            Layout::new(ptr)
        };
        HeapLayout { _buf: buf, layout }
    }

    fn sizes(msg: u64, max: u64) -> Sizes {
        Sizes {
            msg_capacity: msg,
            max_message: max,
            bulk_capacity: 256,
            chunk: 64,
        }
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
        *head = r.next;
        l.release(r.next);
        Some(msg)
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
        let s = Sizes {
            bulk_capacity: 128,
            ..sizes(256, 64)
        };
        assert!(!s.valid());
    }

    #[test]
    fn lanes_are_independent() {
        let h = heap_layout(sizes(256, 64));
        let (msg, bulk) = (h.layout.msg, h.layout.bulk);
        let (mut mt, mut mh, mut bt, mut bh) = (0, 0, 0, 0);
        assert!(put(&msg, &mut mt, b"msg"));
        assert!(put(&bulk, &mut bt, b"bulk"));
        assert!(msg.has_data() && bulk.has_data());
        assert_eq!(take(&bulk, &mut bh).unwrap(), b"bulk");
        assert_eq!(take(&msg, &mut mh).unwrap(), b"msg");
        assert!(!msg.has_data() && !bulk.has_data());
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
            head = r.next;
            l.release(head);
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
        let h = heap_layout(Sizes {
            msg_capacity: 1024,
            max_message: 200,
            bulk_capacity: 256,
            chunk: 64,
        });
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
}

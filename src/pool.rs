//! Zero-copy buffers in shared memory: [`GkShmPool`] lends blocks of a
//! shared memory region to another process, which reads them in place and
//! gives them back by dropping them.
//!
//! ```text
//!  allocating process                              borrowing process
//!  pool.alloc(len) -> ShmBlock   (write data)
//!  block.lend(len) -> ShmLease  --- 16 bytes, e.g. in a GkChannel message -->
//!                                                  view.take(lease) -> ShmBuf (&[u8], no copy)
//!  pool.alloc(..) reuses it     <-- returned on drop (one CAS, any thread) --
//! ```
//!
//! * **One allocator.** Only the process that created the pool allocates
//!   (from any number of its threads). Its bookkeeping (a buddy allocator
//!   over `granule`-sized blocks) lives in that process's own memory, so a
//!   misbehaving peer cannot corrupt it.
//! * **Any number of borrowers.** A [`GkShmPoolView`] maps the same memory
//!   in another process (or this one). It takes leases and gives blocks back
//!   when the [`ShmBuf`]s are dropped, from any thread, lock-free and without
//!   a system call: the block is pushed on a return stack in the shared
//!   memory, which the allocator collects with one atomic swap the next time
//!   it allocates.
//! * **Checked returns.** Each lease carries the block's generation, written
//!   back into the block on return. The allocator frees a returned block only
//!   if it is lent and the generation matches, so a stale or duplicated
//!   return never frees a block someone else holds (it is counted in
//!   [`ShmPoolStats::bad_returns`] instead).
//! * **Sharing.** [`GkShmPool::view`] gives a view in this process;
//!   [`GkProcess::send`](crate::GkProcess::send) hands one to another process
//!   (`recv_as::<GkShmPoolView>()` there), and `fork()` shares the memory.
//!   After `fork`, only one of the two processes may keep allocating.
//! * **A dead borrower** keeps its blocks; once the allocator knows it is
//!   gone (e.g. its channel reports `BrokenPipe`),
//!   [`GkShmPool::reclaim_all`] takes every lent block back.
//!
//! Memory ordering: data written into a block before it is lent is visible
//! to the borrower once it has received the lease through anything that
//! orders memory (a [`GkChannel`](crate::GkChannel) message, an eventfd).
//!
//! Sizes: the pool is rounded up to whole pages; blocks are a power of two
//! number of granules (default granule 4 KiB), so a block wastes at most
//! half its size. On Windows the whole pool counts against the commit limit
//! at creation; on unix pages are allocated as they are first written.

use std::fmt;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::HugePages;
use crate::sys::{RegionOptions, SharedRegion};

/// "GKP" + layout version 1.
const MAGIC: u32 = 0x474B_5001;
/// Smallest and largest granule.
const MIN_GRANULE: usize = 64;
const MAX_GRANULE: usize = 1 << 30;
/// Most blocks a pool can have (block indices are `u32`, `NONE` excluded).
const MAX_BLOCKS: u64 = u32::MAX as u64 - 1;
/// End of a free list.
const NONE: u32 = u32::MAX;

/// The shared header, at the start of the region.
#[repr(C, align(64))]
struct Header {
    magic: AtomicU32,
    granule_shift: AtomicU32,
    /// Number of granules.
    blocks: AtomicU64,
    /// Offset of granule 0 from the start of the region.
    data_offset: AtomicU64,
    _pad: [u64; 5],
    /// The return stack: `index + 1` of the last returned block, 0 = empty.
    /// Each returned block holds `[next: u64][generation: u64]` at its start.
    returned: AtomicU64,
}

/// Sizes of a pool. See [`GkShmPool::with_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmPoolOptions {
    /// Usable size in bytes (rounded up to whole granules, then pages).
    pub size: usize,
    /// Allocation unit in bytes: a power of two from 64 to 1 GiB. Blocks are
    /// a power of two number of granules, aligned to their size.
    pub granule: usize,
    /// Back the pool with huge pages (see [`HugePages`]).
    pub huge_pages: HugePages,
}

impl ShmPoolOptions {
    /// The default granule: 4 KiB.
    pub const DEFAULT_GRANULE: usize = 4096;
}

impl Default for ShmPoolOptions {
    /// 64 MiB (16 MiB on Windows, where it is committed up front), 4 KiB
    /// granules, normal pages.
    fn default() -> Self {
        Self { size: if cfg!(windows) { 16 << 20 } else { 64 << 20 }, granule: Self::DEFAULT_GRANULE, huge_pages: HugePages::Off }
    }
}

/// A lent block, as sent to the borrower: 16 bytes
/// ([`to_bytes`](Self::to_bytes) / [`from_bytes`](Self::from_bytes)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShmLease {
    /// The block's first granule.
    pub index: u32,
    /// The block's generation when it was lent.
    pub generation: u32,
    /// Length of the data in bytes.
    pub len: u64,
}

impl ShmLease {
    /// Little-endian `[index: u32][generation: u32][len: u64]`.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&self.index.to_le_bytes());
        b[4..8].copy_from_slice(&self.generation.to_le_bytes());
        b[8..].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    /// The inverse of [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes"));
        Self { index: u32_at(0), generation: u32_at(4), len: u64::from_le_bytes(b[8..].try_into().expect("8 bytes")) }
    }
}

/// Counters of a pool ([`GkShmPool::stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShmPoolStats {
    /// Usable bytes.
    pub size: usize,
    /// Allocation unit in bytes.
    pub granule: usize,
    /// Bytes in free blocks.
    pub free: usize,
    /// Blocks allocated and not lent yet.
    pub allocated: usize,
    /// Blocks lent and not returned yet.
    pub lent: usize,
    /// Returns that were refused: not lent, wrong generation, or a broken
    /// return stack (each one stops the collection of that batch).
    pub bad_returns: u64,
}

// ---------------------------------------------------------------------------
// The mapping, shared by the pool and its views
// ---------------------------------------------------------------------------

struct Mapping {
    region: SharedRegion,
    data: NonNull<u8>,
    granule_shift: u32,
    blocks: u64,
}

// SAFETY: the mapping is only reached through atomics in the header and the
// lending protocol; `SharedRegion` keeps it mapped while this lives.
unsafe impl Send for Mapping {}
// SAFETY: as above.
unsafe impl Sync for Mapping {}

impl Mapping {
    fn header(&self) -> &Header {
        // SAFETY: the region starts with an initialised, validated header.
        unsafe { self.region.ptr().cast::<Header>().as_ref() }
    }

    fn granule(&self) -> usize {
        1 << self.granule_shift
    }

    /// Start of block `index` (in bounds: `index < blocks`).
    fn block(&self, index: u32) -> *mut u8 {
        debug_assert!(u64::from(index) < self.blocks);
        // SAFETY: `index < blocks`, so the offset is inside the data area.
        unsafe { self.data.as_ptr().add((index as usize) << self.granule_shift) }
    }

    /// Accepts a mapped object as a pool.
    fn accepts(ptr: NonNull<u8>, len: usize) -> bool {
        if len < size_of::<Header>() {
            return false;
        }
        // SAFETY: at least a header's worth of mapped, page-aligned memory.
        let h = unsafe { ptr.cast::<Header>().as_ref() };
        let shift = h.granule_shift.load(Ordering::Relaxed);
        let blocks = h.blocks.load(Ordering::Relaxed);
        let offset = h.data_offset.load(Ordering::Relaxed);
        h.magic.load(Ordering::Acquire) == MAGIC
            && (MIN_GRANULE.trailing_zeros()..=MAX_GRANULE.trailing_zeros()).contains(&shift)
            && (1..=MAX_BLOCKS).contains(&blocks)
            && offset >= size_of::<Header>() as u64
            && offset % 64 == 0
            && blocks.checked_mul(1 << shift).and_then(|b| b.checked_add(offset)).is_some_and(|end| end <= len as u64)
    }

    /// For a region created by `GkShmPool` or validated with `accepts`.
    fn new(region: SharedRegion) -> Self {
        // SAFETY: initialised and validated.
        let h = unsafe { region.ptr().cast::<Header>().as_ref() };
        let granule_shift = h.granule_shift.load(Ordering::Relaxed);
        let blocks = h.blocks.load(Ordering::Relaxed);
        let offset = h.data_offset.load(Ordering::Relaxed) as usize;
        // SAFETY: `offset` is inside the region (checked by `accepts`).
        let data = unsafe { region.ptr().add(offset) };
        Self { region, data, granule_shift, blocks }
    }
}

// ---------------------------------------------------------------------------
// The allocator (process-local)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not the start of a block, or inside a larger one.
    Inner,
    Free,
    Allocated,
    Lent,
}

/// A buddy allocator over `n` granules, with intrusive free lists.
struct Buddy {
    n: u32,
    order: Vec<u8>,
    state: Vec<State>,
    generation: Vec<u32>,
    next: Vec<u32>,
    prev: Vec<u32>,
    heads: Vec<u32>,
    free_granules: u64,
    allocated: usize,
    lent: usize,
    bad_returns: u64,
}

impl Buddy {
    fn new(n: u32) -> Self {
        let max_order = 31 - n.leading_zeros();
        let len = n as usize;
        let mut b = Self {
            n,
            order: vec![0; len],
            state: vec![State::Inner; len],
            generation: vec![0; len],
            next: vec![NONE; len],
            prev: vec![NONE; len],
            heads: vec![NONE; max_order as usize + 1],
            free_granules: 0,
            allocated: 0,
            lent: 0,
            bad_returns: 0,
        };
        // Carve [0, n) into the largest aligned power-of-two blocks.
        let mut i = 0u32;
        while i < n {
            let mut o = (i.trailing_zeros()).min(max_order);
            while u64::from(i) + (1u64 << o) > u64::from(n) {
                o -= 1;
            }
            b.push(i, o as u8);
            b.free_granules += 1 << o;
            i += 1 << o;
        }
        b
    }

    fn push(&mut self, i: u32, o: u8) {
        let head = self.heads[o as usize];
        self.next[i as usize] = head;
        self.prev[i as usize] = NONE;
        if head != NONE {
            self.prev[head as usize] = i;
        }
        self.heads[o as usize] = i;
        self.order[i as usize] = o;
        self.state[i as usize] = State::Free;
    }

    fn unlink(&mut self, i: u32) {
        let (p, n, o) = (self.prev[i as usize], self.next[i as usize], self.order[i as usize]);
        if p == NONE {
            self.heads[o as usize] = n;
        } else {
            self.next[p as usize] = n;
        }
        if n != NONE {
            self.prev[n as usize] = p;
        }
        self.state[i as usize] = State::Inner;
    }

    /// A block of `2^order` granules: its first granule.
    fn alloc(&mut self, order: u8) -> Option<u32> {
        let mut o = order as usize;
        while o < self.heads.len() && self.heads[o] == NONE {
            o += 1;
        }
        if o >= self.heads.len() {
            return None;
        }
        let i = self.heads[o];
        self.unlink(i);
        // Split down, keeping the lower half each time.
        while o > order as usize {
            o -= 1;
            self.push(i + (1 << o), o as u8);
        }
        self.order[i as usize] = order;
        self.state[i as usize] = State::Allocated;
        self.free_granules -= 1 << order;
        self.allocated += 1;
        Some(i)
    }

    /// Frees the block at `i` (allocated or lent), merging with free buddies.
    fn free(&mut self, mut i: u32) {
        debug_assert!(matches!(self.state[i as usize], State::Allocated | State::Lent));
        match self.state[i as usize] {
            State::Lent => self.lent -= 1,
            _ => self.allocated -= 1,
        }
        let mut o = self.order[i as usize];
        self.free_granules += 1 << o;
        while (o as usize) + 1 < self.heads.len() {
            let buddy = i ^ (1 << o);
            if u64::from(buddy) + (1u64 << o) > u64::from(self.n)
                || self.state[buddy as usize] != State::Free
                || self.order[buddy as usize] != o
            {
                break;
            }
            self.unlink(buddy);
            self.state[i as usize] = State::Inner;
            i = i.min(buddy);
            o += 1;
        }
        self.push(i, o);
    }

    fn lend(&mut self, i: u32) -> u32 {
        debug_assert_eq!(self.state[i as usize], State::Allocated);
        self.state[i as usize] = State::Lent;
        self.allocated -= 1;
        self.lent += 1;
        let g = self.generation[i as usize].wrapping_add(1);
        self.generation[i as usize] = g;
        g
    }

    /// A returned block, if it is lent with this generation.
    fn give_back(&mut self, i: u64, generation: u64) -> bool {
        if i >= u64::from(self.n) {
            return false;
        }
        let i = i as u32;
        if self.state[i as usize] != State::Lent || u64::from(self.generation[i as usize]) != generation {
            return false;
        }
        self.free(i);
        true
    }
}

struct PoolInner {
    map: Arc<Mapping>,
    buddy: Mutex<Buddy>,
}

impl PoolInner {
    fn buddy(&self) -> MutexGuard<'_, Buddy> {
        self.buddy.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the return stack and frees what it holds.
    fn collect(&self, b: &mut Buddy) {
        let returned = &self.map.header().returned;
        if returned.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut top = returned.swap(0, Ordering::Acquire);
        // At most every lent block is on the stack: a longer chain is a
        // cycle (a block returned twice) or garbage.
        let mut budget = b.lent;
        while top != 0 {
            let i = top - 1;
            if budget == 0 || i >= self.map.blocks {
                b.bad_returns += 1;
                return;
            }
            budget -= 1;
            // SAFETY: `i` is in bounds; the block was returned (Acquire swap
            // above), so the borrower wrote these words and let go of it.
            let (next, generation) = unsafe {
                let p = self.map.block(i as u32).cast::<u64>();
                (p.read(), p.add(1).read())
            };
            if !b.give_back(i, generation) {
                b.bad_returns += 1;
                return;
            }
            top = next;
        }
    }
}

/// The allocating side of a shared memory pool. See the
/// [module docs](self). `Clone` (a handle to the same pool), `Send` and
/// `Sync`: any thread of the creating process can allocate.
#[derive(Clone)]
pub struct GkShmPool {
    inner: Arc<PoolInner>,
}

/// A block allocated from a [`GkShmPool`] and not lent yet: write into it
/// ([`as_mut_slice`](Self::as_mut_slice)), then [`lend`](Self::lend) it.
/// Dropping it frees the block.
pub struct ShmBlock {
    pool: Arc<PoolInner>,
    index: u32,
    order: u8,
    /// Lent: the borrower gives it back, not our drop.
    lent: bool,
}

/// The borrowing side of a shared memory pool: takes [`ShmLease`]s and gives
/// the blocks back when the [`ShmBuf`]s are dropped. `Clone`, `Send`, `Sync`.
#[derive(Clone)]
pub struct GkShmPoolView {
    map: Arc<Mapping>,
}

/// A lent block, read in place (`Deref<Target = [u8]>`). Dropping it gives
/// the block back to the allocator. `Send` and `Sync`.
pub struct ShmBuf {
    map: Arc<Mapping>,
    lease: ShmLease,
}

impl GkShmPool {
    /// A pool of at least `size` usable bytes in 4 KiB granules.
    pub fn new(size: usize) -> io::Result<Self> {
        Self::with_options(ShmPoolOptions { size, ..Default::default() })
    }

    /// A pool with the given options. Fails with
    /// [`io::ErrorKind::InvalidInput`] if `size` is 0, the granule is not a
    /// power of two from 64 bytes to 1 GiB, or the pool would have more than
    /// `u32::MAX - 1` granules.
    pub fn with_options(opts: ShmPoolOptions) -> io::Result<Self> {
        let invalid = |what: &'static str| io::Error::new(io::ErrorKind::InvalidInput, what);
        if !opts.granule.is_power_of_two() || !(MIN_GRANULE..=MAX_GRANULE).contains(&opts.granule) {
            return Err(invalid("pool granule must be a power of two from 64 bytes to 1 GiB"));
        }
        if opts.size == 0 {
            return Err(invalid("pool size must not be 0"));
        }
        let blocks = opts.size.div_ceil(opts.granule) as u64;
        if blocks > MAX_BLOCKS {
            return Err(invalid("pool has too many granules (use a larger granule)"));
        }
        // Header, then the data aligned to the granule (at most a page).
        let align = opts.granule.clamp(64, 4096);
        let offset = size_of::<Header>().next_multiple_of(align);
        let len =
            (blocks as usize).checked_mul(opts.granule).and_then(|d| d.checked_add(offset)).ok_or_else(|| invalid("pool too large"))?;
        let region = SharedRegion::create_with(len, &RegionOptions { huge: opts.huge_pages, ..Default::default() })?;
        // SAFETY: a fresh, zeroed, page-aligned region of at least `len`
        // bytes, not shared yet.
        let h = unsafe { region.ptr().cast::<Header>().as_ref() };
        h.granule_shift.store(opts.granule.trailing_zeros(), Ordering::Relaxed);
        h.blocks.store(blocks, Ordering::Relaxed);
        h.data_offset.store(offset as u64, Ordering::Relaxed);
        h.magic.store(MAGIC, Ordering::Release);
        let map = Arc::new(Mapping::new(region));
        let buddy = Mutex::new(Buddy::new(blocks as u32));
        Ok(Self { inner: Arc::new(PoolInner { map, buddy }) })
    }

    /// Allocates a block of at least `len` bytes (at least one granule).
    /// Collects returned blocks first. Fails with
    /// [`io::ErrorKind::OutOfMemory`] if no free block is large enough now,
    /// and [`io::ErrorKind::InvalidInput`] if `len` exceeds the pool.
    pub fn alloc(&self, len: usize) -> io::Result<ShmBlock> {
        let map = &self.inner.map;
        let granules = len.div_ceil(map.granule()).max(1) as u64;
        if granules > map.blocks {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "allocation larger than the pool"));
        }
        let order = granules.next_power_of_two().trailing_zeros() as u8;
        let mut b = self.inner.buddy();
        if order as usize >= b.heads.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "allocation larger than the pool's largest block"));
        }
        let index = match b.alloc(order) {
            Some(i) => i,
            None => {
                self.inner.collect(&mut b);
                b.alloc(order).ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "shared memory pool exhausted"))?
            }
        };
        Ok(ShmBlock { pool: Arc::clone(&self.inner), index, order, lent: false })
    }

    /// A view on this pool in this process (to take leases here, or to
    /// [`send`](crate::GkProcess::send) to another process).
    pub fn view(&self) -> GkShmPoolView {
        GkShmPoolView { map: Arc::clone(&self.inner.map) }
    }

    /// Collects returned blocks and reports the counters.
    pub fn stats(&self) -> ShmPoolStats {
        let mut b = self.inner.buddy();
        self.inner.collect(&mut b);
        let map = &self.inner.map;
        ShmPoolStats {
            size: (map.blocks as usize) << map.granule_shift,
            granule: map.granule(),
            free: (b.free_granules as usize) << map.granule_shift,
            allocated: b.allocated,
            lent: b.lent,
            bad_returns: b.bad_returns,
        }
    }

    /// Takes back every lent block, e.g. once the borrower has died.
    ///
    /// # Safety
    /// No borrower may still use a block lent before this call: their
    /// memory is reused by later allocations.
    pub unsafe fn reclaim_all(&self) {
        let mut b = self.inner.buddy();
        // Drop returns in flight: their blocks are reclaimed below.
        self.inner.map.header().returned.swap(0, Ordering::Acquire);
        for i in 0..b.n {
            if b.state[i as usize] == State::Lent {
                b.free(i);
            }
        }
    }

    /// Usable size in bytes.
    pub fn size(&self) -> usize {
        (self.inner.map.blocks as usize) << self.inner.map.granule_shift
    }

    /// Allocation unit in bytes.
    pub fn granule(&self) -> usize {
        self.inner.map.granule()
    }
}

impl fmt::Debug for GkShmPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GkShmPool").field("size", &self.size()).field("granule", &self.granule()).finish()
    }
}

impl ShmBlock {
    /// The whole block (at least the requested length), writable.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the block is allocated to us and not lent: nobody else
        // reads or writes it.
        unsafe { std::slice::from_raw_parts_mut(self.pool.map.block(self.index), self.capacity()) }
    }

    /// The whole block, read-only.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: as in `as_mut_slice`.
        unsafe { std::slice::from_raw_parts(self.pool.map.block(self.index), self.capacity()) }
    }

    /// Block size in bytes (a power of two number of granules).
    pub fn capacity(&self) -> usize {
        self.pool.map.granule() << self.order
    }

    /// Offset of the block in the pool's data area.
    pub fn offset(&self) -> usize {
        (self.index as usize) << self.pool.map.granule_shift
    }

    /// Lends the block, with `len` bytes of data, and returns the lease to
    /// hand to the borrower. The block is the borrower's until its
    /// [`ShmBuf`] is dropped.
    ///
    /// # Panics
    /// If `len` is larger than [`capacity`](Self::capacity).
    pub fn lend(mut self, len: usize) -> ShmLease {
        assert!(len <= self.capacity(), "lend({len}) exceeds the block's {} bytes", self.capacity());
        let generation = self.pool.buddy().lend(self.index);
        self.lent = true;
        ShmLease { index: self.index, generation, len: len as u64 }
    }
}

impl Drop for ShmBlock {
    fn drop(&mut self) {
        if !self.lent {
            self.pool.buddy().free(self.index);
        }
    }
}

impl fmt::Debug for ShmBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmBlock").field("offset", &self.offset()).field("capacity", &self.capacity()).finish()
    }
}

impl GkShmPoolView {
    /// Takes a lent block, to read it in place.
    ///
    /// Fails with [`io::ErrorKind::InvalidData`] if the lease is out of the
    /// pool's bounds (nothing is taken then).
    ///
    /// # Safety
    /// `lease` must come from [`ShmBlock::lend`] on this pool, and be taken
    /// once: the block is given back when the returned [`ShmBuf`] is
    /// dropped. (A lease taken twice is a block returned twice: the
    /// allocator refuses the second return, but the first one may already
    /// have let it reuse memory the second buffer still reads.)
    pub unsafe fn take(&self, lease: ShmLease) -> io::Result<ShmBuf> {
        let map = &self.map;
        let in_bounds = u64::from(lease.index) < map.blocks
            && (u64::from(lease.index) << map.granule_shift)
                .checked_add(lease.len)
                .is_some_and(|end| end <= map.blocks << map.granule_shift);
        if !in_bounds {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "lease outside the pool"));
        }
        Ok(ShmBuf { map: Arc::clone(&self.map), lease })
    }

    /// Usable size of the pool in bytes.
    pub fn size(&self) -> usize {
        (self.map.blocks as usize) << self.map.granule_shift
    }

    /// Allocation unit in bytes.
    pub fn granule(&self) -> usize {
        self.map.granule()
    }
}

impl fmt::Debug for GkShmPoolView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GkShmPoolView").field("size", &self.size()).field("granule", &self.granule()).finish()
    }
}

impl ShmBuf {
    /// The lease this buffer was taken from.
    pub fn lease(&self) -> ShmLease {
        self.lease
    }
}

impl std::ops::Deref for ShmBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: the lease was checked to lie inside the pool, and the block
        // is ours (lent to us) until we give it back on drop.
        unsafe { std::slice::from_raw_parts(self.map.block(self.lease.index), self.lease.len as usize) }
    }
}

impl AsRef<[u8]> for ShmBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        let returned = &self.map.header().returned;
        let p = self.map.block(self.lease.index).cast::<u64>();
        // SAFETY: the block is at least one granule (>= 64 bytes) and ours
        // until the push below publishes it: write the generation, then the
        // link, before the Release CAS.
        unsafe { p.add(1).write(u64::from(self.lease.generation)) };
        let mut top = returned.load(Ordering::Relaxed);
        loop {
            // SAFETY: as above.
            unsafe { p.write(top) };
            match returned.compare_exchange_weak(top, u64::from(self.lease.index) + 1, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => return,
                Err(now) => top = now,
            }
        }
    }
}

impl fmt::Debug for ShmBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmBuf").field("lease", &self.lease).finish()
    }
}

// ---------------------------------------------------------------------------
// Sharing with other processes
// ---------------------------------------------------------------------------

/// Tag of a pool view message (see `sys/unix_share.rs`).
const TAG: &str = "gkpool:v";

impl GkShmPoolView {
    fn adopt(region: SharedRegion) -> Self {
        Self { map: Arc::new(Mapping::new(region)) }
    }

    /// Unix: hands a view of the pool to the process at the other end of
    /// `stream` (`SCM_RIGHTS`), which calls [`GkShmPoolView::recv_from`].
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        crate::sys::unix_share::send(std::os::fd::AsFd::as_fd(stream), TAG, &[self.map.region.fd()])
    }

    /// Unix: receives a view passed with [`GkShmPoolView::send_to`]. Fails
    /// with [`io::ErrorKind::InvalidData`] if something else was sent.
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        <Self as crate::process::private::Sealed>::recv_over(std::os::fd::AsFd::as_fd(stream))
    }
}

/// Lets `GkProcess` hand pool views to other processes (see `process.rs`).
impl crate::process::private::Sealed for GkShmPoolView {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        crate::sys::unix_share::send(sock, TAG, &[self.map.region.fd()])
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        let mut fds = crate::sys::unix_share::recv(sock, TAG, 1)?;
        let fd = fds.pop().expect("one descriptor");
        Ok(Self::adopt(SharedRegion::from_owned(fd, size_of::<Header>(), Mapping::accepts)?))
    }
    /// An offer `gkpool:v|<section>`, duplicated out of this process by
    /// the peer (from the copy kept in `GkProcess`).
    #[cfg(windows)]
    fn offer(&self, _pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        use std::os::windows::io::AsRawHandle;
        let keep = self.map.region.section().try_clone_to_owned()?;
        let token = format!("{TAG}|{:x}", keep.as_raw_handle() as usize);
        Ok((token, vec![keep]))
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, "invalid pool offer");
        let mut parts = token.trim().split('|');
        if parts.next() != Some(TAG) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not the expected kind of object"));
        }
        let section = usize::from_str_radix(parts.next().ok_or_else(bad)?, 16).map_err(|_| bad())?;
        if parts.next().is_some() {
            return Err(bad());
        }
        let section = crate::sys::duplicate_from(pid, section)?;
        Ok(Self::adopt(SharedRegion::from_owned(section, size_of::<Header>(), Mapping::accepts)?))
    }
}

impl crate::process::Shareable for GkShmPoolView {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks the buddy allocator's invariants against a model.
    fn check(b: &Buddy, live: &std::collections::BTreeMap<u32, u8>) {
        let mut covered = vec![false; b.n as usize];
        let mut free = 0u64;
        for (o, &head) in b.heads.iter().enumerate() {
            let mut i = head;
            let mut prev = NONE;
            while i != NONE {
                assert_eq!(b.state[i as usize], State::Free);
                assert_eq!(b.order[i as usize] as usize, o);
                assert_eq!(b.prev[i as usize], prev);
                assert_eq!(i % (1 << o), 0, "aligned");
                for g in i..i + (1 << o) {
                    assert!(!covered[g as usize], "overlap");
                    covered[g as usize] = true;
                }
                free += 1 << o;
                prev = i;
                i = b.next[i as usize];
            }
        }
        for (&i, &o) in live {
            assert!(matches!(b.state[i as usize], State::Allocated | State::Lent));
            for g in i..i + (1 << o) {
                assert!(!covered[g as usize], "live block overlaps");
                covered[g as usize] = true;
            }
        }
        assert!(covered.iter().all(|&c| c), "every granule is free or live");
        assert_eq!(free, b.free_granules);
    }

    #[test]
    fn buddy_random_alloc_free_keeps_invariants() {
        for n in [1u32, 2, 3, 7, 64, 100, 129] {
            let mut b = Buddy::new(n);
            let mut live = std::collections::BTreeMap::new();
            let mut seed = 0x9e37_79b9_u32.wrapping_mul(n);
            let steps = if cfg!(miri) { 200 } else { 5_000 };
            for _ in 0..steps {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if seed % 3 != 0 || live.is_empty() {
                    let o = (seed >> 8) as u8 % 5;
                    if let Some(i) = b.alloc(o) {
                        assert!(live.insert(i, o).is_none());
                    }
                } else {
                    let k = *live.keys().nth((seed >> 4) as usize % live.len()).unwrap();
                    live.remove(&k);
                    b.free(k);
                }
                check(&b, &live);
            }
            for (i, _) in std::mem::take(&mut live) {
                b.free(i);
            }
            check(&b, &live);
            assert_eq!(b.free_granules, u64::from(n), "everything merges back");
        }
    }

    #[test]
    fn lease_bytes_round_trip() {
        let l = ShmLease { index: 0xdead_beef, generation: 7, len: 1 << 40 };
        assert_eq!(ShmLease::from_bytes(&l.to_bytes()), l);
    }

    #[test]
    fn option_rules() {
        let bad = |o: ShmPoolOptions| GkShmPool::with_options(o).unwrap_err().kind();
        let d = ShmPoolOptions { size: 1 << 20, ..Default::default() };
        assert_eq!(bad(ShmPoolOptions { granule: 100, ..d }), io::ErrorKind::InvalidInput);
        assert_eq!(bad(ShmPoolOptions { granule: 32, ..d }), io::ErrorKind::InvalidInput);
        assert_eq!(bad(ShmPoolOptions { size: 0, ..d }), io::ErrorKind::InvalidInput);
        let p = GkShmPool::with_options(ShmPoolOptions { size: 1000, granule: 64, ..d }).unwrap();
        assert_eq!((p.size(), p.granule()), (1024, 64));
    }
}

//! Shared memory regions that can be shared with other processes, plus the
//! process helpers the cross-process code needs (own pid, peer liveness).
//!
//! A [`SharedRegion`] is a writable `MAP_SHARED` view of an unnamed shared
//! memory object:
//!
//! * unix: `memfd_create` (Linux, Android), `SHM_ANON` (FreeBSD), or
//!   `shm_open` + immediate `shm_unlink` elsewhere. `fork(2)` keeps the view
//!   shared, and the descriptor can be inherited across `exec` or passed
//!   over a unix socket;
//! * Windows: an unnamed pagefile-backed section, whose handle can be
//!   inherited or duplicated into other processes.
//!
//! Each process unmaps its view when it drops the region; the kernel frees
//! the memory when the last process is done. Used by the eventfd counter
//! page (`shm.rs`) and by channel rings (`channel/`).

use std::io;
use std::ptr::NonNull;

#[cfg(unix)]
#[path = "unix.rs"]
mod os;
#[cfg(windows)]
#[path = "windows.rs"]
mod os;

pub(crate) use crate::shared_memory::HugePages;

/// How to create a region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RegionOptions {
    /// Huge pages or not.
    pub(crate) huge: HugePages,
    /// The huge page size wanted (`None`: the system default).
    pub(crate) huge_size: Option<usize>,
    /// Touch every page at creation.
    pub(crate) populate: bool,
}

/// What `os::create` made.
pub(crate) struct Created<H> {
    pub(crate) ptr: NonNull<u8>,
    /// Mapped length: the requested length rounded up to `page_size`.
    pub(crate) len: usize,
    pub(crate) page_size: usize,
    pub(crate) huge: bool,
    pub(crate) handle: H,
}

/// The pages behind a received object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageInfo {
    pub(crate) page_size: usize,
    pub(crate) huge: bool,
}

/// `len` rounded up to a multiple of `page` (a power of two).
pub(crate) fn round_up(len: usize, page: usize) -> io::Result<usize> {
    debug_assert!(page.is_power_of_two());
    len.checked_add(page - 1)
        .map(|n| n & !(page - 1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "region too large"))
}

/// `HugePages::Require` on a system (or build) without huge page support.
pub(crate) fn huge_unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "huge pages are not available for shared memory on this system",
    )
}

/// A writable shared mapping of `len` bytes.
#[derive(Debug)]
pub(crate) struct SharedRegion {
    ptr: NonNull<u8>,
    len: usize,
    page_size: usize,
    huge: bool,
    /// The shared memory object, kept open so it can be passed along.
    #[cfg(unix)]
    handle: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

// SAFETY: the region is plain shared memory; every user synchronises access
// through atomics in it (or a protocol built on them).
unsafe impl Send for SharedRegion {}
// SAFETY: as above.
unsafe impl Sync for SharedRegion {}

fn invalid_data() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "not an adaptif-gatotkaca-eventfd shared memory object",
    )
}

impl SharedRegion {
    /// A fresh, zero-filled region of `len` bytes (normal pages).
    // Only the eventfd shims' counter page uses it outside tests.
    #[cfg_attr(gk_native, allow(dead_code))]
    pub(crate) fn create(len: usize) -> io::Result<Self> {
        Self::create_with(len, &RegionOptions::default())
    }

    /// A fresh, zero-filled region of at least `len` bytes (rounded up to
    /// the page size used), with the given page options.
    pub(crate) fn create_with(len: usize, opts: &RegionOptions) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared memory length must not be zero",
            ));
        }
        let c = os::create(len, opts)?;
        let region = Self {
            ptr: c.ptr,
            len: c.len,
            page_size: c.page_size,
            huge: c.huge,
            handle: c.handle,
        };
        if opts.populate {
            region.populate();
        }
        Ok(region)
    }

    /// Touches every page so later accesses don't fault. Only for a fresh
    /// region nobody else has seen yet.
    fn populate(&self) {
        let mut off = 0;
        while off < self.len {
            // SAFETY: inside our fresh mapping; rewrites the byte it reads
            // (zero), so the contents don't change.
            unsafe {
                let p = self.ptr.as_ptr().add(off);
                p.write_volatile(p.read_volatile());
            }
            off += self.page_size;
        }
    }

    /// The size of the pages backing the region.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn page_size(&self) -> usize {
        self.page_size
    }

    /// Whether the region uses huge (large) pages.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_huge(&self) -> bool {
        self.huge
    }

    /// Start of the mapping (page-aligned).
    pub(crate) fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    /// Mapped length in bytes (at least the length it was created with).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Takes over a shared memory object received from another process if
    /// it is at least `min_len` bytes long and `check` accepts its mapped
    /// contents (`ptr`, mapped length). On failure the object is left open:
    /// it may not be ours at all.
    ///
    /// # Safety
    /// If it validates, `raw` must be a descriptor/handle this process may
    /// own.
    #[cfg(unix)]
    pub(crate) unsafe fn adopt(
        raw: std::os::fd::RawFd,
        min_len: usize,
        check: impl FnOnce(NonNull<u8>, usize) -> bool,
    ) -> io::Result<Self> {
        let info = os::inspect(raw)?;
        let len = os::object_len(raw)?;
        if len < min_len || len == 0 {
            return Err(invalid_data());
        }
        let ptr = os::map(raw, len)?;
        if !check(ptr, len) {
            // SAFETY: unmaps the view we just created.
            unsafe { os::unmap(ptr, len) };
            return Err(invalid_data());
        }
        // SAFETY: validated; the caller guarantees we may own it.
        let handle = unsafe { std::os::fd::FromRawFd::from_raw_fd(raw) };
        Ok(Self {
            ptr,
            len,
            page_size: info.page_size,
            huge: info.huge,
            handle,
        })
    }

    /// Like [`adopt`](Self::adopt) for a descriptor we already own (closed
    /// if it does not validate).
    #[cfg(unix)]
    pub(crate) fn from_owned(
        fd: std::os::fd::OwnedFd,
        min_len: usize,
        check: impl FnOnce(NonNull<u8>, usize) -> bool,
    ) -> io::Result<Self> {
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        // SAFETY: we own `fd`; on success ownership moves into the region.
        let region = unsafe { Self::adopt(raw, min_len, check) }?;
        std::mem::forget(fd);
        Ok(region)
    }

    /// The shared memory object, to inherit or pass to other processes.
    #[cfg(unix)]
    pub(crate) fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.handle)
    }

    /// Windows counterpart of the unix `adopt` (same contract).
    ///
    /// # Safety
    /// If it validates, `raw` must be a handle this process may own.
    #[cfg(windows)]
    pub(crate) unsafe fn adopt(
        raw: std::os::windows::io::RawHandle,
        min_len: usize,
        check: impl FnOnce(NonNull<u8>, usize) -> bool,
    ) -> io::Result<Self> {
        let (ptr, len, info) = os::map_whole(raw)?;
        if len < min_len || !check(ptr, len) {
            // SAFETY: unmaps the view we just created.
            unsafe { os::unmap(ptr, len) };
            return Err(invalid_data());
        }
        // SAFETY: validated; the caller guarantees we may own it.
        let handle = unsafe { std::os::windows::io::FromRawHandle::from_raw_handle(raw) };
        Ok(Self {
            ptr,
            len,
            page_size: info.page_size,
            huge: info.huge,
            handle,
        })
    }

    /// Like [`adopt`](Self::adopt) for a handle we already own (closed if it
    /// does not validate).
    #[cfg(windows)]
    pub(crate) fn from_owned(
        section: std::os::windows::io::OwnedHandle,
        min_len: usize,
        check: impl FnOnce(NonNull<u8>, usize) -> bool,
    ) -> io::Result<Self> {
        let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&section);
        // SAFETY: we own `section`; on success ownership moves into the region.
        let region = unsafe { Self::adopt(raw, min_len, check) }?;
        std::mem::forget(section);
        Ok(region)
    }

    /// The section handle, to duplicate into other processes.
    #[cfg(windows)]
    pub(crate) fn section(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        std::os::windows::io::AsHandle::as_handle(&self.handle)
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly our view; `&mut self` means no reference
        // into it is alive in this process. Other processes keep theirs.
        unsafe { os::unmap(self.ptr, self.len) };
    }
}

/// This process's id (cached; correct in a `fork`ed child too).
pub(crate) fn current_pid() -> u32 {
    os::current_pid()
}

/// False if process `pid` has exited (or never existed). On unix an exited
/// but unreaped child of ours counts as exited.
pub(crate) fn process_alive(pid: u32) -> bool {
    os::process_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_is_zeroed_and_writable() {
        let r = SharedRegion::create(1 << 20).unwrap();
        assert!(r.len() >= 1 << 20);
        // SAFETY: the region is ours and at least 1 MiB long.
        unsafe {
            let p = r.ptr().as_ptr();
            assert_eq!(*p, 0);
            assert_eq!(*p.add((1 << 20) - 1), 0);
            *p.add(12345) = 7;
            assert_eq!(*p.add(12345), 7);
        }
    }

    /// A second mapping of the same object sees the same bytes.
    #[test]
    fn adopt_maps_the_same_memory() {
        let a = SharedRegion::create(4096).unwrap();
        // SAFETY: writes inside the region.
        unsafe { *a.ptr().as_ptr().add(100) = 42 };
        #[cfg(unix)]
        let copy = a.fd().try_clone_to_owned().unwrap();
        #[cfg(windows)]
        let copy = a.section().try_clone_to_owned().unwrap();
        let b = SharedRegion::from_owned(copy, 4096, |p, _| {
            // SAFETY: the view is at least 4096 bytes.
            unsafe { *p.as_ptr().add(100) == 42 }
        })
        .unwrap();
        // SAFETY: inside both views.
        unsafe {
            *b.ptr().as_ptr().add(200) = 9;
            assert_eq!(*a.ptr().as_ptr().add(200), 9);
        }
        // Too small, or rejected by the check: refused.
        #[cfg(unix)]
        let copy2 = a.fd().try_clone_to_owned().unwrap();
        #[cfg(windows)]
        let copy2 = a.section().try_clone_to_owned().unwrap();
        let err = SharedRegion::from_owned(copy2, 1 << 30, |_, _| true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn lengths_round_up_to_the_page_size() {
        let r = SharedRegion::create(1).unwrap();
        assert_eq!(r.len(), r.page_size());
        assert!(r.page_size().is_power_of_two());
        assert!(!r.is_huge());
        let r = SharedRegion::create(r.page_size() + 1).unwrap();
        assert_eq!(r.len(), 2 * r.page_size());
        assert_eq!(
            SharedRegion::create(0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            round_up(usize::MAX, 4096).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn populate_and_prefer_give_a_usable_region() {
        let opts = RegionOptions {
            huge: HugePages::Prefer,
            populate: true,
            ..Default::default()
        };
        let r = SharedRegion::create_with(3 << 20, &opts).unwrap();
        assert!(r.len() >= 3 << 20);
        // SAFETY: inside the region.
        unsafe {
            assert_eq!(*r.ptr().as_ptr().add(r.len() - 1), 0);
            *r.ptr().as_ptr().add(r.len() - 1) = 1;
        }
    }

    #[test]
    fn this_process_is_alive() {
        assert!(process_alive(current_pid()));
    }
}

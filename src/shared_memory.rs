//! Shared memory between threads and processes: [`GkSharedMemory`].
//!
//! A `GkSharedMemory` is a zero-filled, writable block of memory that other
//! processes can map too. The same code works on every OS:
//!
//! | | unix (Linux, macOS, BSDs, ...) | Windows |
//! |---|---|---|
//! | object | unnamed shared memory object (`memfd_create`, `SHM_ANON`, or `shm_open` + `shm_unlink`) | unnamed pagefile-backed section |
//! | `fork()` | the child shares it (no code needed) | (no `fork`) |
//! | to another process | [`GkProcess::send`](crate::GkProcess::send), or `send_to` over a `UnixStream` | [`GkProcess::send`](crate::GkProcess::send) |
//!
//! Sharing hands over a copy of the object: both processes keep using the
//! same memory, and it is freed when the last one drops it. Nothing in it
//! is synchronised for you; use atomics, or this crate's eventfds as
//! doorbells ("data is ready"):
//!
//! ```no_run
//! use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd, GkProcess, GkSharedMemory};
//! use std::process::Command;
//!
//! // Parent
//! let shm = GkSharedMemory::new(1 << 20)?;
//! let ready = GkEventFd::new(0, EfdFlags::NONE)?;
//! let (mut child, mut peer) = GkProcess::spawn(&mut Command::new("./worker"))?;
//! peer.send(&shm)?;
//! peer.send(&ready)?;
//! // SAFETY: the worker only reads after the doorbell below.
//! unsafe { shm.as_mut_slice()[..5].copy_from_slice(b"hello") };
//! ready.write(1)?; // the eventfd write orders the memory writes before it
//! # child.wait()?;
//!
//! // Worker
//! let mut parent = GkProcess::from_parent()?;
//! let shm: GkSharedMemory = parent.recv_as()?;
//! let ready = parent.recv()?;
//! ready.read()?;
//! // SAFETY: the parent no longer writes this part.
//! assert_eq!(unsafe { &shm.as_slice()[..5] }, b"hello");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Huge pages
//!
//! [`SharedMemoryOptions::huge_pages`] asks for huge (large) pages, which
//! cut TLB misses for big regions:
//!
//! * **Linux:** hugetlb pages, which must be reserved beforehand
//!   (`vm.nr_hugepages`); with [`HugePages::Prefer`] and none reserved,
//!   transparent huge pages are requested instead.
//! * **FreeBSD:** large pages (superpages) for shared memory objects.
//! * **Windows:** large pages, which need the "Lock pages in memory" user
//!   right (`SeLockMemoryPrivilege`) and enough contiguous free memory.
//! * **macOS, iOS and other unix:** none. `Prefer` gives normal pages,
//!   `Require` fails with [`io::ErrorKind::Unsupported`].
//!
//! [`is_huge`](GkSharedMemory::is_huge) and
//! [`page_size`](GkSharedMemory::page_size) tell what you got.

use std::fmt;
use std::io;

use crate::sys::{RegionOptions, SharedRegion};

/// Whether shared memory should use huge (large) pages. See the
/// [module docs](self#huge-pages) for what each OS offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HugePages {
    /// Normal pages (the default).
    #[default]
    Off,
    /// Huge pages if the system can provide them now, normal pages if not.
    /// Never fails because of huge pages.
    Prefer,
    /// Huge pages or an error: [`io::ErrorKind::Unsupported`] (no huge
    /// pages on this OS, or not that size), [`io::ErrorKind::PermissionDenied`]
    /// (Windows: missing the "Lock pages in memory" right), or
    /// [`io::ErrorKind::OutOfMemory`] (none free right now).
    Require,
}

/// How [`GkSharedMemory::with_options`] creates memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SharedMemoryOptions {
    /// Huge pages or not (default [`HugePages::Off`]).
    pub huge_pages: HugePages,
    /// The huge page size wanted, e.g. `1 << 30` for 1 GiB pages on Linux.
    /// `None` (default): the system's default huge page size. Ignored when
    /// `huge_pages` is `Off`.
    pub huge_page_size: Option<usize>,
    /// Touch every page at creation, so later accesses don't fault (and, on
    /// Linux, so a lack of memory shows up now rather than as `SIGBUS`
    /// later). Default `false`.
    pub populate: bool,
}

/// Zero-filled memory that can be shared with other threads and processes.
/// See the [module docs](self).
///
/// The length is rounded up to a whole number of pages. Dropping it unmaps
/// this process's view; the memory is freed when no process uses it.
///
/// `GkSharedMemory` is `Send + Sync`. Access goes through a raw pointer
/// ([`as_ptr`](Self::as_ptr)) or the `unsafe` slice views, because other
/// threads and processes can write the memory at any time.
pub struct GkSharedMemory {
    region: SharedRegion,
}

impl GkSharedMemory {
    /// Creates zero-filled shared memory of at least `len` bytes (rounded up
    /// to the page size). A `len` of 0 fails with
    /// [`io::ErrorKind::InvalidInput`].
    pub fn new(len: usize) -> io::Result<Self> {
        Self::with_options(len, SharedMemoryOptions::default())
    }

    /// Like [`new`](Self::new), with page options. With huge pages the
    /// length is rounded up to the huge page size.
    pub fn with_options(len: usize, options: SharedMemoryOptions) -> io::Result<Self> {
        let opts = RegionOptions {
            huge: options.huge_pages,
            huge_size: options.huge_page_size,
            populate: options.populate,
        };
        Ok(Self {
            region: SharedRegion::create_with(len, &opts)?,
        })
    }

    /// Start of the memory (page-aligned, never null). Valid for
    /// [`len`](Self::len) bytes while `self` lives.
    pub fn as_ptr(&self) -> *mut u8 {
        self.region.ptr().as_ptr()
    }

    /// Length in bytes: the requested length rounded up to the page size.
    pub fn len(&self) -> usize {
        self.region.len()
    }

    /// Always `false`: shared memory is at least one page long.
    pub fn is_empty(&self) -> bool {
        self.region.len() == 0
    }

    /// Size of the pages backing the memory (the huge page size if
    /// [`is_huge`](Self::is_huge)).
    pub fn page_size(&self) -> usize {
        self.region.page_size()
    }

    /// True if the memory uses huge (large) pages.
    pub fn is_huge(&self) -> bool {
        self.region.is_huge()
    }

    /// The memory as a byte slice.
    ///
    /// # Safety
    /// Other threads and processes may write the memory at any time. While
    /// the slice is alive, nobody may write the bytes you read through it
    /// (no `&mut` from [`as_mut_slice`](Self::as_mut_slice), no writes by
    /// another process). Coordinate with a protocol (an eventfd doorbell,
    /// atomics in the memory), or use [`as_ptr`](Self::as_ptr) with
    /// volatile or atomic accesses instead.
    pub unsafe fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is valid for `len` bytes while `self` lives;
        // the caller guarantees nobody writes it meanwhile.
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.len()) }
    }

    /// The memory as a mutable byte slice. Takes `&self`: the memory is
    /// shared, so Rust's borrow rules can't protect it.
    ///
    /// # Safety
    /// While the slice is alive, nobody else may read or write the bytes you
    /// access through it: no other slice from this or another handle, no
    /// other thread, no other process. Coordinate as for
    /// [`as_slice`](Self::as_slice).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut_slice(&self) -> &mut [u8] {
        // SAFETY: the mapping is valid and writable for `len` bytes while
        // `self` lives; the caller guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.as_ptr(), self.len()) }
    }
}

impl fmt::Debug for GkSharedMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GkSharedMemory")
            .field("ptr", &self.as_ptr())
            .field("len", &self.len())
            .field("page_size", &self.page_size())
            .field("is_huge", &self.is_huge())
            .finish()
    }
}

/// Unix: the shared memory object. Advanced use (e.g. passing it to code
/// that maps it itself); don't resize it.
#[cfg(unix)]
impl std::os::fd::AsFd for GkSharedMemory {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.region.fd()
    }
}

/// Windows: the section handle. Advanced use (e.g. passing it to code that
/// maps it itself).
#[cfg(windows)]
impl std::os::windows::io::AsHandle for GkSharedMemory {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.region.section()
    }
}

// ---------------------------------------------------------------------------
// Sharing with other processes
// ---------------------------------------------------------------------------

/// Tag of a shared memory message (see `sys/unix_share.rs`).
const TAG: &str = "gkshm1:r";

/// Received memory: any length (the content is the application's).
#[cfg(unix)]
fn adopt(fd: std::os::fd::OwnedFd) -> io::Result<SharedRegion> {
    SharedRegion::from_owned(fd, 1, |_, _| true)
}

impl GkSharedMemory {
    /// Unix: hands a copy of this memory to the process at the other end of
    /// `stream` (`SCM_RIGHTS`), which calls
    /// [`GkSharedMemory::recv_from`]. Both processes then use the same
    /// memory. Don't interleave other writes on `stream` with it.
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        crate::sys::unix_share::send(std::os::fd::AsFd::as_fd(stream), TAG, &[self.region.fd()])
    }

    /// Unix: receives memory passed with [`GkSharedMemory::send_to`]. Fails
    /// with [`io::ErrorKind::InvalidData`] if something else was sent
    /// (closing it).
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        Self::recv_over(std::os::fd::AsFd::as_fd(stream))
    }

    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        let mut fds = crate::sys::unix_share::recv(sock, TAG, 1)?;
        let fd = fds.pop().expect("one descriptor");
        Ok(Self { region: adopt(fd)? })
    }
}

/// Lets `GkProcess` hand shared memory to other processes (see
/// `process.rs`).
impl crate::process::private::Sealed for GkSharedMemory {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        crate::sys::unix_share::send(sock, TAG, &[self.region.fd()])
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        Self::recv_over(sock)
    }
    /// An offer `gkshm1:r|<section>`: the peer duplicates the section out
    /// of this process, from the copy kept in `GkProcess`.
    #[cfg(windows)]
    fn offer(&self, _pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        use std::os::windows::io::AsRawHandle;
        let keep = self.region.section().try_clone_to_owned()?;
        let token = format!("{TAG}|{:x}", keep.as_raw_handle() as usize);
        Ok((token, vec![keep]))
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, "invalid shared memory offer");
        let mut parts = token.trim().split('|');
        if parts.next() != Some(TAG) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not the expected kind of object",
            ));
        }
        let section =
            usize::from_str_radix(parts.next().ok_or_else(bad)?, 16).map_err(|_| bad())?;
        if parts.next().is_some() {
            return Err(bad());
        }
        let section = crate::sys::duplicate_from(pid, section)?;
        Ok(Self {
            region: SharedRegion::from_owned(section, 1, |_, _| true)?,
        })
    }
}

impl crate::process::Shareable for GkSharedMemory {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_map_onto_the_region() {
        let shm = GkSharedMemory::with_options(
            10,
            SharedMemoryOptions {
                huge_pages: HugePages::Prefer,
                huge_page_size: None,
                populate: true,
            },
        )
        .unwrap();
        assert!(shm.len() >= 10);
        assert!(!shm.is_empty());
        assert_eq!(shm.len() % shm.page_size(), 0);
    }
}

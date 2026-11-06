//! unix side of [`SharedRegion`](super::SharedRegion): unnamed shared memory
//! objects, mapping, and the process helpers.
//!
//! Objects and their size:
//!
//! * Linux, Android, FreeBSD 13+: `memfd_create` with sealing. Every object
//!   we make is sealed against shrinking and growing (`F_SEAL_SHRINK |
//!   F_SEAL_GROW | F_SEAL_SEAL`), and received objects must carry those
//!   seals. A peer that shares the object therefore can't `ftruncate` it
//!   under our mapping (which would raise SIGBUS on access).
//! * macOS, iOS: `shm_open` + `shm_unlink`. The kernel only allows the
//!   first `ftruncate` of a POSIX shared memory object, so once sized it
//!   can't shrink (tested below).
//! * NetBSD, illumos, OpenBSD, DragonFly: `shm_open` + `shm_unlink`, no
//!   sealing available (NetBSD 10, illumos, OpenBSD and DragonFly have no
//!   sealable memfd in the `libc` crate or the OS). Remaining risk: a peer
//!   process that we share an object with can shrink it and crash us with
//!   SIGBUS. Only share with processes you trust there.
//!
//! Huge pages:
//!
//! * Linux, Android: hugetlbfs memfds (`MFD_HUGETLB`), sealed like the
//!   others (needs Linux 4.16+). The pages are reserved when we map the
//!   object and prefaulted (`MADV_POPULATE_WRITE`, Linux 5.14+), so running
//!   out shows up as `OutOfMemory` at creation, never as SIGBUS later.
//!   `Prefer` falls back to normal pages with `MADV_HUGEPAGE`
//!   (transparent huge pages for shmem, if the system allows it), reported
//!   as normal pages.
//! * FreeBSD: largepage objects (`memfd_create(MFD_HUGETLB)`, which libc
//!   turns into `shm_open2(SHM_LARGEPAGE)` + `FIOSSHMLPGCNF`), sealed too.
//!   The pages are allocated by `ftruncate`, so a shortage is an error at
//!   creation.
//! * Elsewhere: `Prefer` gives normal pages, `Require` fails with
//!   `Unsupported`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{Created, HugePages, PageInfo, RegionOptions};

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "region too large")
}

/// Seals every object must carry: its size can't change.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
const SIZE_SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;

/// An unnamed, close-on-exec memfd that accepts seals. `extra`: huge page
/// flags.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn sealable_memfd(extra: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: a static C string name (debugging only) and flags.
    let fd = cvt(unsafe {
        libc::memfd_create(
            c"gk-eventfd".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | extra,
        )
    })?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Fixes the size of our fresh object for good.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn seal_size(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain fcntl on our descriptor.
    cvt(unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            SIZE_SEALS | libc::F_SEAL_SEAL,
        )
    })
    .map(drop)
}

/// `InvalidData` unless the size of `raw` is sealed.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn check_sealed(raw: RawFd) -> io::Result<()> {
    // SAFETY: F_GET_SEALS only reads; a bad or unsealable fd just fails.
    let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
    if seals == -1 || seals & SIZE_SEALS != SIZE_SEALS {
        return Err(super::invalid_data());
    }
    Ok(())
}

/// An unnamed, close-on-exec shared memory object for normal pages.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn anon_shm() -> io::Result<OwnedFd> {
    sealable_memfd(0)
}

/// An unnamed, close-on-exec shared memory object: created under a
/// unique name and unlinked right away, so no name is left behind.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
fn anon_shm() -> io::Result<OwnedFd> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    loop {
        // macOS limits names to 31 bytes: "/gkefd.<pid>.<seq>.<nanos>".
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "/gkefd.{:x}.{:x}.{:x}",
            current_pid(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos & 0xff_ffff
        );
        let cname = std::ffi::CString::new(name).expect("no NUL in name");
        // SAFETY: valid C string; O_EXCL never opens someone else's object.
        let fd = unsafe {
            libc::shm_open(
                cname.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            )
        };
        if fd == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(err);
        }
        // SAFETY: a fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: removes the name we just created; the object lives on
        // while it is open or mapped.
        unsafe { libc::shm_unlink(cname.as_ptr()) };
        // SAFETY: plain fcntl on our descriptor.
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        return Ok(fd);
    }
}

/// Maps `len` bytes of `fd` shared and writable.
pub(crate) fn map(fd: RawFd, len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: a fresh shared mapping of `fd`, no address hint.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(NonNull::new(ptr.cast::<u8>()).expect("mmap returned null"))
}

/// The system's normal page size.
pub(crate) fn base_page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(n).ok().filter(|&n| n > 0).unwrap_or(4096)
}

/// Sizes the fresh object `fd` to `len` (a multiple of `page_size`), seals
/// its size where the OS can, and maps it.
fn finish(fd: OwnedFd, len: usize, page_size: usize, huge: bool) -> io::Result<Created<OwnedFd>> {
    let size = libc::off_t::try_from(len).map_err(|_| too_large())?;
    // SAFETY: sizes our fresh object.
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), size) })?;
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    seal_size(&fd)?;
    let ptr = map(fd.as_raw_fd(), len)?;
    Ok(Created {
        ptr,
        len,
        page_size,
        huge,
        handle: fd,
    })
}

/// A normal-page object. `thp`: ask for transparent huge pages (Linux).
fn create_normal(len: usize, thp: bool) -> io::Result<Created<OwnedFd>> {
    let page = base_page_size();
    let len = super::round_up(len, page)?;
    let c = finish(anon_shm()?, len, page, false)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if thp {
        // SAFETY: advice on our own fresh mapping; failure (THP not built
        // in) is harmless, so it is ignored.
        unsafe { libc::madvise(c.ptr.as_ptr().cast(), c.len, libc::MADV_HUGEPAGE) };
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = thp;
    Ok(c)
}

/// `huge_size` must be a power of two above the normal page size.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn check_huge_size(size: usize) -> io::Result<u32> {
    if !size.is_power_of_two() || size <= base_page_size() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "huge page size must be a power of two larger than the normal page size",
        ));
    }
    Ok(size.trailing_zeros())
}

/// Page size of a hugetlbfs object, `None` for anything else.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn hugetlb_page_size(raw: RawFd) -> io::Result<Option<usize>> {
    const HUGETLBFS_MAGIC: u32 = 0x9584_58f6;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs writes a statfs through a valid pointer.
    cvt(unsafe { libc::fstatfs(raw, st.as_mut_ptr()) })?;
    // SAFETY: fstatfs succeeded.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::unnecessary_cast)] // f_type's type differs per target
    if st.f_type as u32 != HUGETLBFS_MAGIC {
        return Ok(None);
    }
    #[allow(clippy::unnecessary_cast)]
    let page = st.f_bsize as usize;
    if !page.is_power_of_two() {
        return Err(super::invalid_data());
    }
    Ok(Some(page))
}

/// Errors from setting up a hugetlb memfd that mean "no huge pages here"
/// (old kernel, no hugetlbfs, no pool of that size) become `Unsupported`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn hugetlb_setup_error(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS | libc::ENODEV | libc::ENOENT | libc::EOPNOTSUPP) => {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("huge pages are not available for shared memory: {e}"),
            )
        }
        _ => e,
    }
}

/// A hugetlbfs object (Linux 4.16+ for sealing).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn create_huge(len: usize, huge_size: Option<usize>) -> io::Result<Created<OwnedFd>> {
    let size_flag = match huge_size {
        // The kernel's default huge page size.
        None => 0,
        Some(s) => check_huge_size(s)? << libc::MFD_HUGE_SHIFT,
    };
    let fd = sealable_memfd(libc::MFD_HUGETLB | size_flag).map_err(hugetlb_setup_error)?;
    let page = hugetlb_page_size(fd.as_raw_fd())?.ok_or_else(super::huge_unsupported)?;
    let len = super::round_up(len, page)?;
    // The mmap reserves every huge page of the object (memfds are not
    // MAP_NORESERVE), so a short pool fails here with ENOMEM.
    let c = finish(fd, len, page, true).map_err(|e| {
        if e.raw_os_error() == Some(libc::ENOMEM) {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "not enough free huge pages for the shared memory region",
            )
        } else {
            hugetlb_setup_error(e)
        }
    })?;
    // Prefault every page now, so a page that can't be had after all
    // (e.g. a cgroup hugetlb limit) is an error here instead of SIGBUS on
    // first touch. EINVAL: kernel before 5.14; the reservation covers us.
    // SAFETY: advice on our own fresh mapping.
    let r = unsafe { libc::madvise(c.ptr.as_ptr().cast(), c.len, libc::MADV_POPULATE_WRITE) };
    if r == -1 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINVAL) {
            // SAFETY: our fresh view, not used afterwards.
            unsafe { unmap(c.ptr, c.len) };
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("could not populate huge pages: {e}"),
            ));
        }
    }
    Ok(c)
}

/// The page sizes the system supports, smallest (normal) first.
#[cfg(target_os = "freebsd")]
fn page_sizes() -> Vec<usize> {
    let mut sizes = [0usize; 16];
    // SAFETY: getpagesizes writes at most `nelem` entries into `sizes`.
    let n = unsafe { libc::getpagesizes(sizes.as_mut_ptr(), sizes.len() as libc::c_int) };
    let n = usize::try_from(n).unwrap_or(0).min(sizes.len());
    sizes[..n].to_vec()
}

/// A largepage object: FreeBSD's `memfd_create` with `MFD_HUGETLB` makes it
/// with `shm_open2(SHM_LARGEPAGE)` and `FIOSSHMLPGCNF`. The kernel
/// allocates the pages at `ftruncate`, so a shortage fails there (ENOMEM).
#[cfg(target_os = "freebsd")]
fn create_huge(len: usize, huge_size: Option<usize>) -> io::Result<Created<OwnedFd>> {
    let sizes = page_sizes();
    let page = match huge_size {
        None => *sizes.get(1).ok_or_else(super::huge_unsupported)?,
        Some(s) => {
            check_huge_size(s)?;
            if !sizes.contains(&s) {
                return Err(super::huge_unsupported());
            }
            s
        }
    };
    // MFD_HUGE_SHIFT is 26 on FreeBSD too (MFD_HUGE_2MB is 21 << 26).
    let flags = libc::MFD_HUGETLB | (page.trailing_zeros() << 26);
    let fd = sealable_memfd(flags).map_err(|e| match e.raw_os_error() {
        Some(libc::EINVAL | libc::EOPNOTSUPP | libc::ENOSYS) => super::huge_unsupported(),
        _ => e,
    })?;
    let len = super::round_up(len, page)?;
    finish(fd, len, page, true)
}

/// No huge pages for shared memory on this system.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
fn create_huge(_len: usize, _huge_size: Option<usize>) -> io::Result<Created<OwnedFd>> {
    Err(super::huge_unsupported())
}

/// Creates and maps a zero-filled object of at least `len` bytes.
pub(crate) fn create(len: usize, opts: &RegionOptions) -> io::Result<Created<OwnedFd>> {
    match opts.huge {
        HugePages::Off => create_normal(len, false),
        HugePages::Require => create_huge(len, opts.huge_size),
        HugePages::Prefer => match create_huge(len, opts.huge_size) {
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Unsupported
                        | io::ErrorKind::OutOfMemory
                        | io::ErrorKind::PermissionDenied
                ) =>
            {
                create_normal(len, true)
            }
            r => r,
        },
    }
}

/// FreeBSD `FIOGSHMLPGCNF`: `_IOR('f', 101, struct shm_largepage_conf)`.
#[cfg(target_os = "freebsd")]
const FIOGSHMLPGCNF: libc::c_ulong = 0x4000_0000
    | ((std::mem::size_of::<libc::shm_largepage_conf>() as libc::c_ulong & 0x1fff) << 16)
    | ((b'f' as libc::c_ulong) << 8)
    | 101;

/// Checks a received object before it is mapped (its size must be sealed
/// where the OS can seal) and reports its page size and whether it uses
/// huge pages. `InvalidData` if it must be refused.
pub(crate) fn inspect(raw: RawFd) -> io::Result<PageInfo> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        check_sealed(raw)?;
        let huge = hugetlb_page_size(raw).map_err(|_| super::invalid_data())?;
        Ok(match huge {
            Some(page_size) => PageInfo {
                page_size,
                huge: true,
            },
            None => PageInfo {
                page_size: base_page_size(),
                huge: false,
            },
        })
    }
    #[cfg(target_os = "freebsd")]
    {
        check_sealed(raw)?;
        // SAFETY: an all-zero shm_largepage_conf is a valid value.
        let mut conf: libc::shm_largepage_conf = unsafe { std::mem::zeroed() };
        // SAFETY: the ioctl only writes a shm_largepage_conf through a valid
        // pointer; it fails on anything that is not a largepage object.
        let r = unsafe { libc::ioctl(raw, FIOGSHMLPGCNF, &mut conf) };
        let psind = if r == 0 {
            usize::try_from(conf.psind).unwrap_or(0)
        } else {
            0
        };
        if psind > 0 {
            let page_size = *page_sizes().get(psind).ok_or_else(super::invalid_data)?;
            return Ok(PageInfo {
                page_size,
                huge: true,
            });
        }
        Ok(PageInfo {
            page_size: base_page_size(),
            huge: false,
        })
    }
    // macOS/iOS: sizes can't change after the first ftruncate. Elsewhere
    // nothing can be checked (see the module docs).
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    {
        let _ = raw;
        Ok(PageInfo {
            page_size: base_page_size(),
            huge: false,
        })
    }
}

/// Size of the shared memory object `raw` (`InvalidInput` if not open).
pub(crate) fn object_len(raw: RawFd) -> io::Result<usize> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat writes a stat through a valid pointer; bad values fail.
    if unsafe { libc::fstat(raw, st.as_mut_ptr()) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared memory descriptor is not open",
        ));
    }
    // SAFETY: fstat succeeded. No file-type check: macOS reports POSIX
    // shared memory objects without S_IFREG; callers check contents.
    let size = unsafe { st.assume_init() }.st_size;
    usize::try_from(size).map_err(|_| super::invalid_data())
}

/// # Safety
/// `ptr`/`len` must be a view created by `map`, not used afterwards.
pub(super) unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    // SAFETY: forwarded to the caller.
    unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
}

/// This process's pid. Cached, and reset in the child after `fork`
/// (where the cached value would be the parent's).
pub(crate) fn current_pid() -> u32 {
    static PID: AtomicU32 = AtomicU32::new(0);
    static ATFORK: Once = Once::new();

    extern "C" fn reset_in_child() {
        PID.store(0, Ordering::Relaxed);
    }

    let pid = PID.load(Ordering::Relaxed);
    if pid != 0 {
        return pid;
    }
    ATFORK.call_once(|| {
        // SAFETY: registers a handler that only does an atomic store,
        // which is safe to run in the child right after fork.
        unsafe { libc::pthread_atfork(None, None, Some(reset_in_child)) };
    });
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() } as u32;
    PID.store(pid, Ordering::Relaxed);
    pid
}

/// False if process `pid` no longer exists, or is our child that has
/// exited but not been reaped yet (`kill(pid, 0)` still succeeds for
/// such zombies).
pub(crate) fn process_alive(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    // SAFETY: an all-zero siginfo_t is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live siginfo_t. WNOWAIT leaves the child
    // waitable, so this never steals an exit status from the application.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r == 0 {
        // Our child: si_pid is set only if it has exited.
        // SAFETY: waitid filled `info` (or left it zeroed for "no change").
        return unsafe { info.si_pid() } == 0;
    }
    // Not our child (ECHILD): ask the kernel whether it exists.
    // SAFETY: signal 0 only checks for existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::super::{HugePages, RegionOptions, SharedRegion};
    use super::*;

    fn opts(huge: HugePages, huge_size: Option<usize>) -> RegionOptions {
        RegionOptions {
            huge,
            huge_size,
            populate: false,
        }
    }

    fn truncate(fd: RawFd, len: usize) -> io::Result<()> {
        // SAFETY: plain ftruncate on a descriptor the test owns.
        cvt(unsafe { libc::ftruncate(fd, len as libc::off_t) }).map(drop)
    }

    /// Objects from `create` can't be resized, either way.
    #[test]
    fn created_objects_cannot_be_resized() {
        let r = SharedRegion::create(3 * base_page_size()).unwrap();
        let fd = r.fd().as_raw_fd();
        for len in [r.len() / 3, r.len() * 2, 0] {
            let err = truncate(fd, len).unwrap_err();
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
            assert_eq!(err.raw_os_error(), Some(libc::EPERM), "len {len}");
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "len {len}");
            let _ = err;
        }
        assert_eq!(object_len(fd).unwrap(), r.len());
        // SAFETY: inside the region, which kept its size.
        unsafe { *r.ptr().as_ptr().add(r.len() - 1) = 1 };
    }

    /// A sized memfd made by hand, with the given seals (`None`: not
    /// sealable at all).
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    fn hand_made(seals: Option<libc::c_int>) -> OwnedFd {
        let flags = if seals.is_some() {
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING
        } else {
            libc::MFD_CLOEXEC
        };
        // SAFETY: static name and flags.
        let fd = cvt(unsafe { libc::memfd_create(c"gk-test".as_ptr(), flags) }).unwrap();
        // SAFETY: a fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        truncate(fd.as_raw_fd(), 4096).unwrap();
        if let Some(s) = seals.filter(|&s| s != 0) {
            // SAFETY: plain fcntl on our descriptor.
            cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, s) }).unwrap();
        }
        fd
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn unsealed_objects_are_refused() {
        let cases = [
            None,
            Some(0),
            Some(libc::F_SEAL_SHRINK),
            Some(libc::F_SEAL_GROW | libc::F_SEAL_SEAL),
        ];
        for seals in cases {
            let fd = hand_made(seals);
            let err = inspect(fd.as_raw_fd()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{seals:?}");
            let err = SharedRegion::from_owned(fd, 1, |_, _| true).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{seals:?}");
        }
        // Both size seals: accepted (SEAL_SEAL is not required).
        let fd = hand_made(Some(SIZE_SEALS));
        let info = inspect(fd.as_raw_fd()).unwrap();
        assert!(!info.huge);
        let r = SharedRegion::from_owned(fd, 1, |_, _| true).unwrap();
        assert_eq!(r.len(), 4096);
    }

    /// A plain POSIX shm object (a tmpfs file on Linux) and a regular file
    /// are refused too.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn other_files_are_refused() {
        let name = std::ffi::CString::new(format!("/gk-test-{}", current_pid())).unwrap();
        // SAFETY: valid C string; the name is unlinked right away.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::mode_t,
            )
        };
        if fd != -1 {
            // SAFETY: fresh descriptor we own; removes our own name.
            let fd = unsafe {
                libc::shm_unlink(name.as_ptr());
                OwnedFd::from_raw_fd(fd)
            };
            truncate(fd.as_raw_fd(), 4096).unwrap();
            let err = inspect(fd.as_raw_fd()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let err = inspect(file.as_raw_fd()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn prefer_always_succeeds() {
        for size in [None, Some(2 << 20)] {
            let r = SharedRegion::create_with(5 << 20, &opts(HugePages::Prefer, size)).unwrap();
            assert!(r.len() >= 5 << 20);
            assert_eq!(r.len() % r.page_size(), 0);
            // SAFETY: inside the region.
            unsafe { *r.ptr().as_ptr().add(r.len() - 1) = 1 };
        }
    }

    #[test]
    fn huge_size_is_ignored_when_off() {
        let r = SharedRegion::create_with(10, &opts(HugePages::Off, Some(3))).unwrap();
        assert!(!r.is_huge());
        assert_eq!(r.page_size(), base_page_size());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn bad_huge_sizes_are_invalid_input() {
        for size in [3 << 20, base_page_size(), 1024, 0] {
            for huge in [HugePages::Prefer, HugePages::Require] {
                let err = SharedRegion::create_with(10, &opts(huge, Some(size))).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{size} {huge:?}");
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    #[test]
    fn require_is_unsupported() {
        let err = SharedRegion::create_with(10, &opts(HugePages::Require, None)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    /// With `GK_TEST_HUGEPAGES=1` (huge pages reserved), `Require` must
    /// work; otherwise it may also fail for lack of huge pages.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn require_gives_huge_pages_or_a_clear_error() {
        let must = std::env::var("GK_TEST_HUGEPAGES").as_deref() == Ok("1");
        for size in [None, Some(2 << 20)] {
            let o = RegionOptions {
                populate: true,
                ..opts(HugePages::Require, size)
            };
            let r = match SharedRegion::create_with(3 << 20, &o) {
                Ok(r) => r,
                Err(e) if !must => {
                    assert!(
                        matches!(
                            e.kind(),
                            io::ErrorKind::OutOfMemory
                                | io::ErrorKind::Unsupported
                                | io::ErrorKind::PermissionDenied
                        ),
                        "{e:?}"
                    );
                    continue;
                }
                Err(e) => panic!("huge pages required: {e:?}"),
            };
            assert!(r.is_huge());
            assert!(r.page_size() >= 2 << 20);
            if let Some(s) = size {
                assert_eq!(r.page_size(), s);
            }
            assert_eq!(r.len() % r.page_size(), 0);
            assert!(r.len() >= 3 << 20);
            // SAFETY: inside the region.
            unsafe { *r.ptr().as_ptr().add(r.len() - 1) = 7 };
            // Sealed like any other.
            let fd = r.fd().as_raw_fd();
            assert_eq!(
                truncate(fd, r.len() - r.page_size())
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EPERM)
            );
            assert_eq!(
                truncate(fd, r.len() + r.page_size())
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EPERM)
            );
            // A received copy is recognised as huge.
            let info = inspect(fd).unwrap();
            assert!(info.huge);
            assert_eq!(info.page_size, r.page_size());
            let copy = r.fd().try_clone_to_owned().unwrap();
            let len = r.len();
            let b = SharedRegion::from_owned(copy, len, |p, l| {
                // SAFETY: the view is `l` bytes long.
                l == len && unsafe { *p.as_ptr().add(l - 1) } == 7
            })
            .unwrap();
            assert!(b.is_huge());
            assert_eq!(b.page_size(), r.page_size());
            assert_eq!(b.len(), r.len());
        }
        // A size no x86/arm system has: unsupported (or no pool).
        let err = SharedRegion::create_with(10, &opts(HugePages::Require, Some(1 << 40)));
        if let Err(e) = err {
            assert!(
                matches!(
                    e.kind(),
                    io::ErrorKind::OutOfMemory | io::ErrorKind::Unsupported
                ),
                "{e:?}"
            );
        }
    }

    /// More huge pages than the pool has: an error at creation (never
    /// SIGBUS later), and `Prefer` falls back to normal pages.
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        target_pointer_width = "64"
    ))]
    #[test]
    fn exhausted_pool_is_out_of_memory() {
        const LEN: usize = 16 << 30;
        for size in [None, Some(1 << 30)] {
            match SharedRegion::create_with(LEN, &opts(HugePages::Require, size)) {
                // A machine with 16 GiB of huge pages reserved.
                Ok(r) => assert!(r.is_huge()),
                Err(e) => assert!(
                    matches!(
                        e.kind(),
                        io::ErrorKind::OutOfMemory | io::ErrorKind::Unsupported
                    ),
                    "{e:?}"
                ),
            }
            let r = SharedRegion::create_with(LEN, &opts(HugePages::Prefer, size)).unwrap();
            assert_eq!(r.len(), LEN);
        }
    }

    /// `Prefer` with huge pages reserved gives huge pages.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn prefer_uses_reserved_huge_pages() {
        if std::env::var("GK_TEST_HUGEPAGES").as_deref() != Ok("1") {
            return;
        }
        let r = SharedRegion::create_with(1, &opts(HugePages::Prefer, None)).unwrap();
        assert!(r.is_huge());
        assert_eq!(r.len(), r.page_size());
    }
}

//! unix side of [`SharedRegion`](super::SharedRegion): unnamed shared memory
//! objects (`memfd_create`, `SHM_ANON`, or `shm_open` + `shm_unlink`),
//! mapping, and the process helpers.

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

/// An unnamed, close-on-exec shared memory object.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn anon_shm() -> io::Result<OwnedFd> {
    // SAFETY: a static C string name (debugging only) and flags.
    let fd = cvt(unsafe { libc::memfd_create(c"gk-eventfd".as_ptr(), libc::MFD_CLOEXEC) })?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// An unnamed, close-on-exec shared memory object.
#[cfg(target_os = "freebsd")]
fn anon_shm() -> io::Result<OwnedFd> {
    // SAFETY: SHM_ANON creates an object that never has a name.
    let fd = cvt(unsafe { libc::shm_open(libc::SHM_ANON, libc::O_RDWR | libc::O_CLOEXEC, 0o600) })?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
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

/// Creates and maps a zero-filled object of at least `len` bytes.
///
/// Huge pages: not implemented yet on any unix (`Prefer` falls back to
/// normal pages, `Require` fails with `Unsupported`).
pub(crate) fn create(len: usize, opts: &RegionOptions) -> io::Result<Created<OwnedFd>> {
    if opts.huge == HugePages::Require {
        return Err(super::huge_unsupported());
    }
    let page = base_page_size();
    let len = super::round_up(len, page)?;
    let fd = anon_shm()?;
    let size = libc::off_t::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "region too large"))?;
    // SAFETY: sizes our fresh object.
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), size) })?;
    let ptr = map(fd.as_raw_fd(), len)?;
    Ok(Created {
        ptr,
        len,
        page_size: page,
        huge: false,
        handle: fd,
    })
}

/// Checks a received object before it is mapped (e.g. that its size is
/// sealed) and reports its page size and whether it uses huge pages.
/// `InvalidData` if it must be refused.
pub(crate) fn inspect(raw: RawFd) -> io::Result<PageInfo> {
    let _ = raw;
    Ok(PageInfo {
        page_size: base_page_size(),
        huge: false,
    })
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

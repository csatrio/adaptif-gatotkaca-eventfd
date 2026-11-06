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

/// A writable shared mapping of `len` bytes.
#[derive(Debug)]
pub(crate) struct SharedRegion {
    ptr: NonNull<u8>,
    len: usize,
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
    /// A fresh, zero-filled region of `len` bytes.
    pub(crate) fn create(len: usize) -> io::Result<Self> {
        let (ptr, handle) = os::create(len)?;
        Ok(Self { ptr, len, handle })
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
        let len = os::object_len(raw)?;
        if len < min_len {
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
        Ok(Self { ptr, len, handle })
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
        let (ptr, len) = os::map_whole(raw)?;
        if len < min_len || !check(ptr, len) {
            // SAFETY: unmaps the view we just created.
            unsafe { os::unmap(ptr, len) };
            return Err(invalid_data());
        }
        // SAFETY: validated; the caller guarantees we may own it.
        let handle = unsafe { std::os::windows::io::FromRawHandle::from_raw_handle(raw) };
        Ok(Self { ptr, len, handle })
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

// ---------------------------------------------------------------------------
// unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod os {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::ptr::NonNull;
    use std::sync::Once;
    use std::sync::atomic::{AtomicU32, Ordering};

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
        let fd =
            cvt(unsafe { libc::shm_open(libc::SHM_ANON, libc::O_RDWR | libc::O_CLOEXEC, 0o600) })?;
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
    pub(super) fn map(fd: RawFd, len: usize) -> io::Result<NonNull<u8>> {
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

    pub(super) fn create(len: usize) -> io::Result<(NonNull<u8>, OwnedFd)> {
        let fd = anon_shm()?;
        let size = libc::off_t::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "region too large"))?;
        // SAFETY: sizes our fresh object.
        cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), size) })?;
        let ptr = map(fd.as_raw_fd(), len)?;
        Ok((ptr, fd))
    }

    /// Size of the shared memory object `raw` (`InvalidInput` if not open).
    pub(super) fn object_len(raw: RawFd) -> io::Result<usize> {
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
    pub(super) fn current_pid() -> u32 {
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
    pub(super) fn process_alive(pid: u32) -> bool {
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
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod os {
    use std::io;
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr::NonNull;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION,
        MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };

    /// Maps `len` bytes of `section` (0: the whole section).
    fn map(section: RawHandle, len: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: plain call; a bogus handle makes it fail.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        NonNull::new(view.Value.cast::<u8>()).ok_or_else(io::Error::last_os_error)
    }

    pub(super) fn create(len: usize) -> io::Result<(NonNull<u8>, OwnedHandle)> {
        let len64 = len as u64;
        // SAFETY: an unnamed, pagefile-backed section with default (not
        // inheritable) security; no other process can open it by name.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                (len64 >> 32) as u32,
                len64 as u32,
                std::ptr::null(),
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle that nothing else owns.
        let section = unsafe { OwnedHandle::from_raw_handle(raw) };
        let ptr = map(raw, len)?;
        Ok((ptr, section))
    }

    /// Maps a whole section and returns the view and its (page-rounded)
    /// length. `InvalidInput` if `raw` is not a mappable section.
    pub(super) fn map_whole(raw: RawHandle) -> io::Result<(NonNull<u8>, usize)> {
        let ptr = map(raw, 0).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "handle is not a shared section",
            )
        })?;
        // SAFETY: an all-zero MEMORY_BASIC_INFORMATION is a valid value.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: queries our own view; `info` is a valid out buffer.
        let n = unsafe {
            VirtualQuery(
                ptr.as_ptr().cast(),
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 {
            let err = io::Error::last_os_error();
            // SAFETY: unmaps the view we just created.
            unsafe { unmap(ptr, 0) };
            return Err(err);
        }
        Ok((ptr, info.RegionSize))
    }

    /// # Safety
    /// `ptr` must be a view created by `create`/`map_whole`, not used
    /// afterwards.
    pub(super) unsafe fn unmap(ptr: NonNull<u8>, _len: usize) {
        // SAFETY: forwarded to the caller. The section handle itself is
        // closed by the region's `OwnedHandle`.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: ptr.as_ptr().cast(),
            })
        };
    }

    pub(super) fn current_pid() -> u32 {
        // SAFETY: no preconditions.
        unsafe { GetCurrentProcessId() }
    }

    /// False if process `pid` has exited or does not exist. An exited
    /// process's object stays signalled while handles to it are open, so
    /// this is exact for processes that still have a handle somewhere.
    pub(super) fn process_alive(pid: u32) -> bool {
        // SAFETY: plain call; the handle is closed below.
        let h = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if h.is_null() {
            // No such process (ERROR_INVALID_PARAMETER); any other error
            // (e.g. access denied) means it exists.
            // SAFETY: no preconditions.
            return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
        }
        // SAFETY: `h` is a valid process handle we own.
        let exited = unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0;
        // SAFETY: as above.
        unsafe { CloseHandle(h) };
        !exited
    }
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
    fn this_process_is_alive() {
        assert!(process_alive(current_pid()));
    }
}

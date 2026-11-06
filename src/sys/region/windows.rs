//! Windows side of [`SharedRegion`](super::SharedRegion): unnamed
//! pagefile-backed sections, mapping, and the process helpers.
//!
//! Large pages (`HugePages::Prefer`/`Require`) use a `SEC_LARGE_PAGES`
//! section. Windows has a single large page size for this
//! (`GetLargePageMinimum`, 2 MiB on x64), so `huge_size` must be `None` or
//! exactly that size. Creating one needs `SeLockMemoryPrivilege` ("Lock
//! pages in memory"); this module enables it in the process token the
//! first time large pages are asked for (it stays enabled). Large page
//! memory is committed in full when the section is created and is locked:
//! it is never paged out.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use super::{Created, HugePages, PageInfo, RegionOptions};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, ERROR_INVALID_PARAMETER, ERROR_NO_SYSTEM_RESOURCES,
    ERROR_NOT_ENOUGH_MEMORY, ERROR_OUTOFMEMORY, ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, LUID, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_LOCK_MEMORY_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP, FILE_MAP_ALL_ACCESS, FILE_MAP_LARGE_PAGES, GetLargePageMinimum,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_PROTECTION_FLAGS,
    PAGE_READWRITE, SEC_COMMIT, SEC_LARGE_PAGES, UnmapViewOfFile, VirtualQuery,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_BLOCK, PSAPI_WORKING_SET_EX_INFORMATION,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// Maps `len` bytes of `section` (0: the whole section) with `access`.
fn map_with(section: RawHandle, len: usize, access: FILE_MAP) -> io::Result<NonNull<u8>> {
    // SAFETY: plain call; a bogus handle makes it fail.
    let view = unsafe { MapViewOfFile(section, access, 0, 0, len) };
    NonNull::new(view.Value.cast::<u8>()).ok_or_else(io::Error::last_os_error)
}

/// Maps `len` bytes of `section` (0: the whole section) normally.
fn map(section: RawHandle, len: usize) -> io::Result<NonNull<u8>> {
    map_with(section, len, FILE_MAP_ALL_ACCESS)
}

/// Maps `len` bytes (0: all) of a `SEC_LARGE_PAGES` section with
/// `FILE_MAP_LARGE_PAGES` (Windows 10 1703+). If Windows asks for
/// `SeLockMemoryPrivilege`, enables it and tries once more.
fn map_large(section: RawHandle, len: usize) -> io::Result<NonNull<u8>> {
    let access = FILE_MAP_ALL_ACCESS | FILE_MAP_LARGE_PAGES;
    match map_with(section, len, access) {
        Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32) => {
            if lock_memory_privilege() {
                map_with(section, len, access)
            } else {
                Err(e)
            }
        }
        r => r,
    }
}

/// The system's normal page size.
pub(crate) fn base_page_size() -> usize {
    // SAFETY: an all-zero SYSTEM_INFO is a valid out buffer.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: fills `info`.
    unsafe { GetSystemInfo(&mut info) };
    (info.dwPageSize as usize).max(4096)
}

/// The large page size (`GetLargePageMinimum`), `None` if the system has
/// no large page support.
fn large_page_size() -> Option<usize> {
    // SAFETY: no preconditions.
    let n = unsafe { GetLargePageMinimum() };
    Some(n).filter(|n| n.is_power_of_two() && *n > base_page_size())
}

/// Whether `SeLockMemoryPrivilege` is enabled in the process token. The
/// first call tries to enable it; the answer is cached for the process
/// (a newly granted right only reaches processes started after signing in
/// again anyway).
fn lock_memory_privilege() -> bool {
    static HELD: OnceLock<bool> = OnceLock::new();
    *HELD.get_or_init(enable_lock_memory_privilege)
}

fn enable_lock_memory_privilege() -> bool {
    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo handle of this process; `raw` is a valid out
    // pointer.
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut raw,
        )
    };
    if ok == 0 {
        return false;
    }
    // SAFETY: a fresh token handle that nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: local system, a static privilege name, a valid out pointer.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_LOCK_MEMORY_NAME, &mut luid) } == 0 {
        return false;
    }
    let privs = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: our token, opened with TOKEN_ADJUST_PRIVILEGES; `privs` is a
    // valid one-entry TOKEN_PRIVILEGES; no previous state wanted.
    let ok = unsafe {
        AdjustTokenPrivileges(
            token.as_raw_handle(),
            0,
            &privs,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // It also "succeeds" when the account lacks the right, with
    // ERROR_NOT_ALL_ASSIGNED; only ERROR_SUCCESS means it is enabled.
    // SAFETY: no preconditions; read right after the call.
    ok != 0 && unsafe { GetLastError() } == ERROR_SUCCESS
}

fn privilege_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "large pages need the \"Lock pages in memory\" right (SeLockMemoryPrivilege): grant it \
         to the account (secpol.msc > Local Policies > User Rights Assignment) and sign in \
         again; depending on policy the process may also have to run elevated",
    )
}

/// Maps a failure of the large page path to the documented error kinds.
fn large_error(e: io::Error) -> io::Error {
    match e.raw_os_error().map(|c| c as u32) {
        Some(ERROR_PRIVILEGE_NOT_HELD) => privilege_error(),
        Some(
            ERROR_NO_SYSTEM_RESOURCES
            | ERROR_NOT_ENOUGH_MEMORY
            | ERROR_OUTOFMEMORY
            | ERROR_COMMITMENT_LIMIT,
        ) => io::Error::new(
            io::ErrorKind::OutOfMemory,
            format!("no large pages available now (memory exhausted or too fragmented): {e}"),
        ),
        _ => e,
    }
}

/// Creates and maps a zero-filled section of at least `len` bytes.
///
/// With `HugePages::Prefer`/`Require` the section uses large pages (see the
/// module docs): `len` is rounded up to the large page size, and all of it
/// is committed and locked in memory at once. `Prefer` falls back to
/// normal pages if large pages are unsupported, the privilege is missing
/// or no large pages are free; `Require` fails with `Unsupported`,
/// `PermissionDenied` or `OutOfMemory` instead. A `huge_size` other than
/// the large page size is `InvalidInput` (when large pages exist at all).
pub(crate) fn create(len: usize, opts: &RegionOptions) -> io::Result<Created<OwnedHandle>> {
    if opts.huge == HugePages::Off {
        return create_normal(len);
    }
    let Some(large) = large_page_size() else {
        return match opts.huge {
            HugePages::Require => Err(super::huge_unsupported()),
            _ => create_normal(len),
        };
    };
    if let Some(size) = opts.huge_size {
        if size != large {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Windows only offers {large}-byte large pages, not {size}-byte ones"),
            ));
        }
    }
    match create_large(len, large) {
        Ok(c) => Ok(c),
        Err(_) if opts.huge == HugePages::Prefer => create_normal(len),
        Err(e) => Err(e),
    }
}

fn create_normal(len: usize) -> io::Result<Created<OwnedHandle>> {
    let page = base_page_size();
    let len = super::round_up(len, page)?;
    let section = new_section(len, PAGE_READWRITE)?;
    let ptr = map(section.as_raw_handle(), len)?;
    Ok(Created {
        ptr,
        len,
        page_size: page,
        huge: false,
        handle: section,
    })
}

fn create_large(len: usize, large: usize) -> io::Result<Created<OwnedHandle>> {
    let len = super::round_up(len, large)?;
    if !lock_memory_privilege() {
        return Err(privilege_error());
    }
    // SEC_LARGE_PAGES needs SEC_COMMIT and a size that is a multiple of
    // the large page size.
    let section =
        new_section(len, PAGE_READWRITE | SEC_COMMIT | SEC_LARGE_PAGES).map_err(large_error)?;
    let raw = section.as_raw_handle();
    let ptr = match map_large(raw, len) {
        Ok(p) => p,
        // Before Windows 10 1703 FILE_MAP_LARGE_PAGES does not exist, and a
        // plain view was how a large page section got mapped.
        Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
            map(raw, len).map_err(large_error)?
        }
        Err(e) => return Err(large_error(e)),
    };
    Ok(Created {
        ptr,
        len,
        page_size: large,
        huge: true,
        handle: section,
    })
}

/// An unnamed, pagefile-backed section of `len` bytes.
fn new_section(len: usize, protect: PAGE_PROTECTION_FLAGS) -> io::Result<OwnedHandle> {
    let len64 = len as u64;
    // SAFETY: an unnamed, pagefile-backed section with default (not
    // inheritable) security; no other process can open it by name.
    let raw = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            protect,
            (len64 >> 32) as u32,
            len64 as u32,
            std::ptr::null(),
        )
    };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Length of our view at `ptr`; unmaps it on failure.
fn view_len(ptr: NonNull<u8>) -> io::Result<usize> {
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
    Ok(info.RegionSize)
}

/// Whether the first page of our view at `ptr` is a large page, from its
/// working set entry (the `LargePage` bit is only meaningful while the
/// page is resident; large pages are locked, so once touched they stay).
fn view_is_large(ptr: NonNull<u8>) -> bool {
    // Fault the page in. Atomic, as another process may be writing it.
    // SAFETY: `ptr` is the start of a live, readable and writable view,
    // aligned for a byte.
    unsafe { AtomicU8::from_ptr(ptr.as_ptr()) }.load(Ordering::Relaxed);
    let mut info = PSAPI_WORKING_SET_EX_INFORMATION {
        VirtualAddress: ptr.as_ptr().cast(),
        VirtualAttributes: PSAPI_WORKING_SET_EX_BLOCK { Flags: 0 },
    };
    // SAFETY: our own process; `info` is one valid entry of the given size.
    let ok = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            (&raw mut info).cast(),
            size_of::<PSAPI_WORKING_SET_EX_INFORMATION>() as u32,
        )
    };
    // SAFETY: both union fields are plain integers.
    let flags = unsafe { info.VirtualAttributes.Flags };
    const VALID: usize = 1;
    const LARGE_PAGE: usize = 1 << 23;
    ok != 0 && flags & VALID != 0 && flags & LARGE_PAGE != 0
}

/// Maps a whole section and returns the view, its (page-rounded) length and
/// its pages. `InvalidInput` if `raw` is not a mappable section.
///
/// Large page sections are recognised: a plain view is tried first. If its
/// first page is not a large page but the length is a multiple of the large
/// page size, a `FILE_MAP_LARGE_PAGES` view is tried, which Windows only
/// grants for `SEC_LARGE_PAGES` sections, and kept if the working set
/// reports large pages for it. That probe may enable
/// `SeLockMemoryPrivilege` if Windows asks for it.
pub(crate) fn map_whole(raw: RawHandle) -> io::Result<(NonNull<u8>, usize, PageInfo)> {
    let not_section = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "handle is not a shared section",
        )
    };
    let large = large_page_size();
    let normal = PageInfo {
        page_size: base_page_size(),
        huge: false,
    };
    let ptr = match map(raw, 0) {
        Ok(p) => p,
        // In case some Windows only maps a large page section with the
        // large page flag.
        Err(_) => {
            let large = large.ok_or_else(not_section)?;
            let ptr = map_large(raw, 0).map_err(|_| not_section())?;
            let len = view_len(ptr)?;
            let info = PageInfo {
                page_size: large,
                huge: true,
            };
            return Ok((ptr, len, info));
        }
    };
    let len = view_len(ptr)?;
    let Some(large) = large else {
        return Ok((ptr, len, normal));
    };
    let huge = PageInfo {
        page_size: large,
        huge: true,
    };
    if view_is_large(ptr) {
        return Ok((ptr, len, huge));
    }
    if len % large != 0 {
        return Ok((ptr, len, normal));
    }
    let Ok(lp) = map_large(raw, 0) else {
        return Ok((ptr, len, normal));
    };
    if !view_is_large(lp) {
        // SAFETY: `lp` is ours and not used afterwards.
        unsafe { unmap(lp, 0) };
        return Ok((ptr, len, normal));
    }
    match view_len(lp) {
        Ok(llen) if llen == len => {
            // SAFETY: the plain view is ours; `lp` replaces it.
            unsafe { unmap(ptr, len) };
            Ok((lp, llen, huge))
        }
        Ok(llen) => {
            // SAFETY: `lp` is ours and not used afterwards.
            unsafe { unmap(lp, llen) };
            Ok((ptr, len, normal))
        }
        // `view_len` already unmapped `lp`.
        Err(_) => Ok((ptr, len, normal)),
    }
}

/// # Safety
/// `ptr` must be a view created by `create`/`map_whole`, not used
/// afterwards.
pub(crate) unsafe fn unmap(ptr: NonNull<u8>, _len: usize) {
    // SAFETY: forwarded to the caller. The section handle itself is
    // closed by the region's `OwnedHandle`.
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
            Value: ptr.as_ptr().cast(),
        })
    };
}

pub(crate) fn current_pid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetCurrentProcessId() }
}

/// False if process `pid` has exited or does not exist. An exited
/// process's object stays signalled while handles to it are open, so
/// this is exact for processes that still have a handle somewhere.
pub(crate) fn process_alive(pid: u32) -> bool {
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

#[cfg(test)]
mod tests {
    use super::super::{HugePages, RegionOptions, SharedRegion};
    use super::{base_page_size, large_page_size};
    use std::io;

    fn opts(huge: HugePages, huge_size: Option<usize>) -> RegionOptions {
        RegionOptions {
            huge,
            huge_size,
            populate: false,
        }
    }

    /// `GK_TEST_HUGEPAGES=1`: the machine is set up for large pages, so
    /// `Require` must work.
    fn must_have_large_pages() -> bool {
        std::env::var("GK_TEST_HUGEPAGES").is_ok_and(|v| v == "1")
    }

    /// A `Require` region, or `None` if this machine can't give one (and
    /// isn't expected to).
    fn require(len: usize) -> Option<SharedRegion> {
        match SharedRegion::create_with(len, &opts(HugePages::Require, None)) {
            Ok(r) => Some(r),
            Err(e) => {
                assert!(
                    matches!(
                        e.kind(),
                        io::ErrorKind::PermissionDenied
                            | io::ErrorKind::OutOfMemory
                            | io::ErrorKind::Unsupported
                    ),
                    "unexpected error: {e}"
                );
                assert!(!must_have_large_pages(), "large pages required: {e}");
                eprintln!("large pages not available here: {e}");
                None
            }
        }
    }

    #[test]
    fn prefer_always_works() {
        let r = SharedRegion::create_with(1, &opts(HugePages::Prefer, None)).unwrap();
        if r.is_huge() {
            assert_eq!(Some(r.page_size()), large_page_size());
        } else {
            assert_eq!(r.page_size(), base_page_size());
        }
        assert_eq!(r.len(), r.page_size());
        // SAFETY: inside the region.
        unsafe {
            let p = r.ptr().as_ptr();
            assert_eq!(*p.add(r.len() - 1), 0);
            *p.add(r.len() - 1) = 5;
            assert_eq!(*p.add(r.len() - 1), 5);
        }
    }

    #[test]
    fn require_gives_large_pages_or_a_clear_error() {
        let Some(r) = require(3 << 20) else { return };
        let large = large_page_size().unwrap();
        assert!(r.is_huge());
        assert_eq!(r.page_size(), large);
        assert!(r.len() >= 3 << 20);
        assert_eq!(r.len() % large, 0);
        // SAFETY: inside the region.
        unsafe {
            let p = r.ptr().as_ptr();
            assert_eq!(*p, 0);
            assert_eq!(*p.add(r.len() - 1), 0);
            *p.add(r.len() - 1) = 3;
            assert_eq!(*p.add(r.len() - 1), 3);
        }
    }

    /// A large page section adopted through a duplicated handle (as a peer
    /// process would) maps the same memory and reports large pages.
    #[test]
    fn large_page_section_adopted_through_a_dup() {
        let Some(a) = require(1) else { return };
        // SAFETY: inside the region.
        unsafe { *a.ptr().as_ptr().add(100) = 42 };
        let copy = a.section().try_clone_to_owned().unwrap();
        let b = SharedRegion::from_owned(copy, 1, |_, _| true).unwrap();
        assert!(b.is_huge());
        assert_eq!(b.page_size(), a.page_size());
        assert_eq!(b.len(), a.len());
        // SAFETY: inside both views.
        unsafe {
            assert_eq!(*b.ptr().as_ptr().add(100), 42);
            *b.ptr().as_ptr().add(a.len() - 1) = 9;
            assert_eq!(*a.ptr().as_ptr().add(a.len() - 1), 9);
        }
    }

    /// A normal section whose length is a multiple of the large page size
    /// is not mistaken for a large page one.
    #[test]
    fn normal_section_adopted_reports_normal_pages() {
        let len = large_page_size().unwrap_or(2 << 20) * 2;
        let a = SharedRegion::create(len).unwrap();
        assert!(!a.is_huge());
        let copy = a.section().try_clone_to_owned().unwrap();
        let b = SharedRegion::from_owned(copy, len, |_, _| true).unwrap();
        assert!(!b.is_huge());
        assert_eq!(b.page_size(), base_page_size());
        assert_eq!(b.len(), len);
    }

    #[test]
    fn huge_size_must_be_the_large_page_size() {
        let Some(large) = large_page_size() else {
            let err = SharedRegion::create_with(1, &opts(HugePages::Require, None)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
            return;
        };
        let r = SharedRegion::create_with(1, &opts(HugePages::Prefer, Some(large))).unwrap();
        assert!(r.len() >= 1);
        for bad in [base_page_size(), large * 2, large + 1] {
            for huge in [HugePages::Prefer, HugePages::Require] {
                let err = SharedRegion::create_with(1, &opts(huge, Some(bad))).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{huge:?} {bad}");
            }
        }
        // Ignored with normal pages.
        let r = SharedRegion::create_with(1, &opts(HugePages::Off, Some(12345))).unwrap();
        assert!(!r.is_huge());
    }
}

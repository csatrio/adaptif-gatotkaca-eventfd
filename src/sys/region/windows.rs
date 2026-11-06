//! Windows side of [`SharedRegion`](super::SharedRegion): unnamed
//! pagefile-backed sections, mapping, and the process helpers.

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::NonNull;

use super::{Created, HugePages, PageInfo, RegionOptions};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
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

/// The system's normal page size.
pub(crate) fn base_page_size() -> usize {
    // SAFETY: an all-zero SYSTEM_INFO is a valid out buffer.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: fills `info`.
    unsafe { GetSystemInfo(&mut info) };
    (info.dwPageSize as usize).max(4096)
}

/// Creates and maps a zero-filled section of at least `len` bytes.
///
/// Large pages: not implemented yet (`Prefer` falls back to normal pages,
/// `Require` fails with `Unsupported`).
pub(crate) fn create(len: usize, opts: &RegionOptions) -> io::Result<Created<OwnedHandle>> {
    if opts.huge == HugePages::Require {
        return Err(super::huge_unsupported());
    }
    let page = base_page_size();
    let len = super::round_up(len, page)?;
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
    Ok(Created {
        ptr,
        len,
        page_size: page,
        huge: false,
        handle: section,
    })
}

/// Maps a whole section and returns the view, its (page-rounded) length and
/// its pages. `InvalidInput` if `raw` is not a mappable section.
pub(crate) fn map_whole(raw: RawHandle) -> io::Result<(NonNull<u8>, usize, PageInfo)> {
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
    Ok((
        ptr,
        info.RegionSize,
        PageInfo {
            page_size: base_page_size(),
            huge: false,
        },
    ))
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

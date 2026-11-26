//! unix: is another process a zombie (exited, not yet reaped by its parent)?
//!
//! `kill(pid, 0)` succeeds for a zombie, so without this check a process
//! that died holding a shim lock, or a channel peer that died, stays
//! "alive" until its parent reaps it, which may be never. Our own children
//! are recognised earlier (`waitid(WNOWAIT)`); this covers everyone else.
//!
//! | OS | How | Zombie when |
//! |---|---|---|
//! | Linux, Android | read `/proc/<pid>/stat` | state `Z` with at most one thread, or `X` / `x` (dead) |
//! | macOS, iOS | `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)` | the call fails with `ESRCH` although `kill` found the process (XNU's `proc_pidinfo` does not see zombies or exiting processes), or reports `SZOMB` |
//! | FreeBSD | `sysctl(KERN_PROC_PID)` | `ki_stat == SZOMB` |
//! | NetBSD | `sysctl(KERN_PROC2, KERN_PROC_PID)` | `p_stat == SZOMB` (5) |
//! | OpenBSD, DragonFly, illumos, others | none | never (a zombie stays alive, as before 0.3.2) |
//!
//! Each check answers "zombie" only on evidence (a zombie state, or on
//! Apple the specific `ESRCH` above, measured on macOS 15: `kill(pid, 0)`
//! succeeds for another parent's zombie while `proc_pidinfo` fails with
//! `ESRCH`, for both BSD info flavors); any other error (no `/proc`,
//! permission, sandbox, unexpected size) answers "not a zombie", which is
//! the pre-0.3.2 behaviour. Callers ask only after `kill(pid, 0)` found the
//! process. Nothing here allocates or
//! takes a lock: it runs on the shim's lock path, also in a `fork`ed child.
//!
//! Linux detail: when only the main thread of a process has exited
//! (`pthread_exit` in `main`), `/proc/<pid>/stat` shows `Z` for it while
//! the other threads run on; the thread count (field 20, which still counts
//! the exited leader) tells the two apart.

/// True if `pid` names a zombie (see the module docs).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn is_zombie(pid: libc::pid_t) -> bool {
    let mut buf = [0u8; 1024];
    read_proc_stat(pid, &mut buf).is_some_and(stat_says_zombie)
}

/// Reads `/proc/<pid>/stat` into `buf` (one `read`: procfs returns the
/// whole line, about 300 bytes), without allocating.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_proc_stat(pid: libc::pid_t, buf: &mut [u8; 1024]) -> Option<&[u8]> {
    let mut path = [0u8; 32];
    let path = proc_stat_path(pid, &mut path)?;
    // SAFETY: `path` is NUL-terminated; a plain read-only open.
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: reads at most `buf.len()` bytes into the caller's buffer.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: closes the descriptor opened above, once.
    unsafe { libc::close(fd) };
    let n = usize::try_from(n).ok()?;
    Some(&buf[..n])
}

/// Writes `/proc/<pid>/stat\0` into `buf` without allocating.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_stat_path(pid: libc::pid_t, buf: &mut [u8; 32]) -> Option<&[u8]> {
    let pid = u32::try_from(pid).ok().filter(|&p| p != 0)?;
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    let mut v = pid;
    while v > 0 {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let mut len = 0;
    for part in [&b"/proc/"[..], &digits[i..], &b"/stat\0"[..]] {
        buf[len..len + part.len()].copy_from_slice(part);
        len += part.len();
    }
    Some(&buf[..len])
}

/// Parses the contents of `/proc/<pid>/stat`: `pid (comm) S ppid ...`.
/// `comm` may contain spaces and parentheses, so fields start after the
/// last `)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn stat_says_zombie(stat: &[u8]) -> bool {
    let Some(end) = stat.iter().rposition(|&b| b == b')') else { return false };
    let mut fields = stat[end + 1..].split(|&b| b == b' ' || b == b'\n').filter(|f| !f.is_empty());
    match fields.next() {
        Some(b"X" | b"x") => true,
        // Field 3 was the state; field 20 is the thread count.
        Some(b"Z") => fields.nth(20 - 4).is_some_and(|t| t == b"0" || t == b"1"),
        _ => false,
    }
}

/// True if `pid` names a zombie (see the module docs).
#[cfg(target_vendor = "apple")]
pub(super) fn is_zombie(pid: libc::pid_t) -> bool {
    #[cfg(not(target_os = "macos"))]
    use apple::{PROC_PIDT_SHORTBSDINFO, ProcBsdShortInfo, proc_pidinfo};
    #[cfg(target_os = "macos")]
    use libc::{PROC_PIDT_SHORTBSDINFO, proc_bsdshortinfo as ProcBsdShortInfo, proc_pidinfo};

    let mut info = std::mem::MaybeUninit::<ProcBsdShortInfo>::zeroed();
    let size = size_of::<ProcBsdShortInfo>() as libc::c_int;
    // SAFETY: the buffer is `size` writable bytes; the short BSD info flavor
    // fills at most that many and needs no privilege.
    let n = unsafe { proc_pidinfo(pid, PROC_PIDT_SHORTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if n == size {
        // SAFETY: zero-initialised plain C data, filled by the call.
        return unsafe { info.assume_init() }.pbsi_status == libc::SZOMB;
    }
    // proc_pidinfo only finds live processes: ESRCH for a pid that kill(2)
    // just found means it has exited and not been reaped (or has been reaped
    // since: dead too). Other errors (EPERM in a sandbox) tell nothing.
    n <= 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// `<sys/proc_info.h>` / `<libproc.h>` for Apple targets other than macOS,
/// where the `libc` crate has no bindings (macOS uses `libc`'s, and a test
/// checks these against them).
#[cfg(all(target_vendor = "apple", any(test, not(target_os = "macos"))))]
mod apple {
    /// `PROC_PIDT_SHORTBSDINFO`.
    pub(super) const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;

    /// `struct proc_bsdshortinfo` (64 bytes).
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct ProcBsdShortInfo {
        pub(super) pbsi_pid: u32,
        pub(super) pbsi_ppid: u32,
        pub(super) pbsi_pgid: u32,
        pub(super) pbsi_status: u32,
        pub(super) pbsi_comm: [libc::c_char; 16],
        pub(super) pbsi_flags: u32,
        pub(super) pbsi_uid: libc::uid_t,
        pub(super) pbsi_gid: libc::gid_t,
        pub(super) pbsi_ruid: libc::uid_t,
        pub(super) pbsi_rgid: libc::gid_t,
        pub(super) pbsi_svuid: libc::uid_t,
        pub(super) pbsi_svgid: libc::gid_t,
        pub(super) pbsi_rfu: u32,
    }

    unsafe extern "C" {
        /// `int proc_pidinfo(int pid, int flavor, uint64_t arg, void *buffer, int buffersize);`
        #[cfg_attr(test, allow(dead_code))]
        pub(super) fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            size: libc::c_int,
        ) -> libc::c_int;
    }
}

/// True if `pid` names a zombie (see the module docs).
#[cfg(target_os = "freebsd")]
pub(super) fn is_zombie(pid: libc::pid_t) -> bool {
    let mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut kp = std::mem::MaybeUninit::<libc::kinfo_proc>::zeroed();
    let mut len = size_of::<libc::kinfo_proc>();
    // SAFETY: `kp` has room for `len` bytes; the kernel writes at most that
    // many and updates `len`. Read-only query (no new value).
    let r = unsafe { libc::sysctl(mib.as_ptr(), mib.len() as libc::c_uint, kp.as_mut_ptr().cast(), &mut len, std::ptr::null(), 0) };
    if r != 0 || len != size_of::<libc::kinfo_proc>() {
        return false;
    }
    // SAFETY: zero-initialised plain C data, filled by the kernel.
    let kp = unsafe { kp.assume_init() };
    kp.ki_structsize as usize == size_of::<libc::kinfo_proc>() && kp.ki_stat == libc::SZOMB
}

/// True if `pid` names a zombie (see the module docs).
#[cfg(target_os = "netbsd")]
pub(super) fn is_zombie(pid: libc::pid_t) -> bool {
    /// `SZOMB` from `<sys/proc.h>` (also `LSZOMB` for an LWP).
    const SZOMB: i8 = 5;
    let size = size_of::<libc::kinfo_proc2>();
    let mib = [libc::CTL_KERN, libc::KERN_PROC2, libc::KERN_PROC_PID, pid, size as libc::c_int, 1];
    let mut kp = std::mem::MaybeUninit::<libc::kinfo_proc2>::zeroed();
    let mut len = size;
    // SAFETY: `kp` has room for `len` bytes (one element of `size` bytes, as
    // the name asks); the kernel writes at most that many.
    let r = unsafe { libc::sysctl(mib.as_ptr(), mib.len() as libc::c_uint, kp.as_mut_ptr().cast(), &mut len, std::ptr::null(), 0) };
    if r != 0 || len != size {
        return false;
    }
    // SAFETY: zero-initialised plain C data, filled by the kernel.
    unsafe { kp.assume_init() }.p_stat == SZOMB
}

/// No cheap way to ask (OpenBSD, DragonFly, illumos, others): a zombie
/// counts as alive, as before 0.3.2.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "freebsd", target_os = "netbsd")))]
pub(super) fn is_zombie(_pid: libc::pid_t) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parses_proc_stat() {
        let line = |state: &str, threads: u32| {
            format!("123 (we ird) n)ame) {state} 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 {threads} 0 4567 1000 200\n")
        };
        assert!(!stat_says_zombie(line("S", 1).as_bytes()));
        assert!(!stat_says_zombie(line("R", 4).as_bytes()));
        assert!(stat_says_zombie(line("Z", 1).as_bytes()));
        // The main thread exited, others still run: alive.
        assert!(!stat_says_zombie(line("Z", 3).as_bytes()));
        assert!(stat_says_zombie(line("X", 1).as_bytes()));
        assert!(stat_says_zombie(line("x", 1).as_bytes()));
        // Malformed or truncated: not a zombie.
        assert!(!stat_says_zombie(b""));
        assert!(!stat_says_zombie(b"123 (x) Z 1 2"));
        let mut buf = [0u8; 32];
        assert_eq!(proc_stat_path(4_294_967, &mut buf), Some(&b"/proc/4294967/stat\0"[..]));
        assert_eq!(proc_stat_path(0, &mut buf), None);
        assert_eq!(proc_stat_path(-1, &mut buf), None);
    }

    /// The local `<sys/proc_info.h>` bindings used on iOS & co. match the
    /// `libc` crate's macOS ones.
    #[cfg(target_os = "macos")]
    #[test]
    fn apple_local_bindings_match_libc() {
        assert_eq!(apple::PROC_PIDT_SHORTBSDINFO, libc::PROC_PIDT_SHORTBSDINFO);
        assert_eq!(size_of::<apple::ProcBsdShortInfo>(), size_of::<libc::proc_bsdshortinfo>());
        assert_eq!(size_of::<apple::ProcBsdShortInfo>(), 64);
        assert_eq!(std::mem::offset_of!(apple::ProcBsdShortInfo, pbsi_status), std::mem::offset_of!(libc::proc_bsdshortinfo, pbsi_status));
    }

    /// Waits (up to 5 s) until our child `pid` has exited, without reaping
    /// it.
    fn wait_exited(pid: libc::pid_t) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: an all-zero siginfo_t is a valid value.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: asks about our own child; WNOWAIT keeps it waitable.
            let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
            // SAFETY: filled by waitid (or still zeroed: no change yet).
            if r == 0 && unsafe { info.si_pid() } != 0 {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "child {pid} did not exit");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// An exited, unreaped process is a zombie where the OS can tell.
    #[test]
    fn exited_unreaped_child_is_a_zombie() {
        // SAFETY: the child only calls _exit.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: async-signal-safe exit in the forked child.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0);
        wait_exited(child);
        let expected =
            cfg!(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "freebsd", target_os = "netbsd"));
        assert_eq!(is_zombie(child), expected);
        // SAFETY: reaps our own child.
        assert_eq!(unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) }, child);
    }

    /// Linux: a process whose main thread exited (like `pthread_exit`) while
    /// another thread runs shows `Z` in `/proc/<pid>/stat` but is alive; once
    /// killed it is a zombie.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn exited_main_thread_with_live_threads_is_not_a_zombie() {
        extern "C" fn park(_: *mut libc::c_void) -> *mut libc::c_void {
            loop {
                // SAFETY: waits for a signal (SIGKILL ends the process).
                unsafe { libc::pause() };
            }
        }
        // SAFETY: the child starts one thread and ends its main thread; it
        // never returns into the test harness.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: an all-zero pthread_t is a valid placeholder.
            let mut t: libc::pthread_t = unsafe { std::mem::zeroed() };
            // SAFETY: `park` never returns; default attributes.
            if unsafe { libc::pthread_create(&mut t, std::ptr::null(), park, std::ptr::null_mut()) } != 0 {
                // SAFETY: async-signal-safe exit in the forked child.
                unsafe { libc::_exit(1) };
            }
            // Ends only the main thread (the raw `exit` syscall, not
            // `exit_group`; `pthread_exit` would unwind through Rust frames
            // and abort the process). `park` keeps the process alive.
            // SAFETY: the thread ends here and never touches its stack again.
            unsafe { libc::syscall(libc::SYS_exit, 0) };
            // SAFETY: not reached; async-signal-safe exit just in case.
            unsafe { libc::_exit(2) };
        }
        assert!(child > 0);
        struct Reap(libc::pid_t);
        impl Drop for Reap {
            fn drop(&mut self) {
                // SAFETY: kills and reaps our own child.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
        let reap = Reap(child);
        let state = |pid| {
            let mut buf = [0u8; 1024];
            let stat = read_proc_stat(pid, &mut buf).unwrap_or_default();
            let end = stat.iter().rposition(|&b| b == b')').map_or(stat.len(), |e| e + 2);
            stat.get(end).copied()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state(child) != Some(b'Z') {
            assert!(std::time::Instant::now() < deadline, "main thread did not exit");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!is_zombie(child), "a process with a live thread is not a zombie");
        // SAFETY: signals our own child.
        unsafe { libc::kill(child, libc::SIGKILL) };
        wait_exited(child);
        assert!(is_zombie(child));
        drop(reap);
    }

    #[test]
    fn this_process_is_not_a_zombie() {
        // SAFETY: getpid has no preconditions.
        assert!(!is_zombie(unsafe { libc::getpid() }));
    }
}

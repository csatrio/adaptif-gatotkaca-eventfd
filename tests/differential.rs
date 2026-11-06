//! Differential test: the portable unix shim (forced on Linux with the
//! `force-shim` feature) against the real kernel `eventfd(2)`.
//!
//! Random operation sequences run against both a [`GkEventFd`] and a raw
//! `libc::eventfd`. After every operation the results (values and errno) and
//! the `POLLIN` readiness of every descriptor must be identical.
//!
//! Sequences also duplicate and close descriptors: `try_clone` against
//! `dup(2)` for `GkEventFd`, and the `compat` C-style API (`eventfd_dup`,
//! `eventfd_dup2`, raw `dup(2)` adoption, `eventfd_close`) against the same
//! calls on the kernel eventfd.
//!
//! Run with: `cargo test --features force-shim --test differential`
//! (`PROPTEST_CASES=N` raises the number of cases.)

#![cfg(all(target_os = "linux", feature = "force-shim"))]

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use adaptif_gatotkaca_eventfd::{EFD_MAX, EfdFlags, GkEventFd, compat};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    /// `GkEventFd::write` vs `write(2)` of 8 bytes.
    Write(u64),
    /// `GkEventFd::read` vs `read(2)` of 8 bytes.
    Read,
    /// `io::Write` with a buffer of `len` bytes (value in the first 8).
    WriteBuf(u64, usize),
    /// `io::Read` with a buffer of `len` bytes.
    ReadBuf(usize),
}

/// A step of a sequence: an I/O op on handle `index % handles`, or a change
/// to the set of handles.
#[derive(Debug, Clone)]
enum Step {
    Io(usize, Op),
    /// Duplicate handle `index % handles`.
    Dup(usize),
    /// Close handle `index % handles` (skipped if it is the last one).
    Close(usize),
}

/// Result of one op: the value read (0 for writes) or the errno.
type Outcome = Result<u64, i32>;

fn errno_of(e: &io::Error) -> i32 {
    e.raw_os_error()
        .unwrap_or_else(|| panic!("shim error without errno: {e:?}"))
}

fn kernel_eventfd(initval: u32, semaphore: bool) -> OwnedFd {
    let flags =
        libc::EFD_NONBLOCK | libc::EFD_CLOEXEC | if semaphore { libc::EFD_SEMAPHORE } else { 0 };
    // SAFETY: plain syscall.
    let fd = unsafe { libc::eventfd(initval, flags) };
    assert!(fd >= 0, "eventfd: {}", io::Error::last_os_error());
    // SAFETY: fresh descriptor we own.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn last_errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap()
}

fn kernel_read(fd: RawFd, len: usize) -> Outcome {
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is a live buffer of `len` bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), len) };
    if n < 0 {
        return Err(last_errno());
    }
    assert_eq!(n, 8, "kernel eventfd short read");
    Ok(u64::from_ne_bytes(buf[..8].try_into().unwrap()))
}

fn kernel_write(fd: RawFd, value: u64, len: usize) -> Outcome {
    let mut buf = vec![0u8; len];
    let bytes = value.to_ne_bytes();
    let k = len.min(8);
    buf[..k].copy_from_slice(&bytes[..k]);
    // SAFETY: `buf` is a live buffer of `len` bytes.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), len) };
    if n < 0 {
        return Err(last_errno());
    }
    assert_eq!(n, 8, "kernel eventfd short write");
    Ok(0)
}

fn shim_op(efd: &GkEventFd, op: &Op) -> Outcome {
    let res = match *op {
        Op::Write(v) => efd.write(v).map(|()| 0),
        Op::Read => efd.read(),
        Op::WriteBuf(v, len) => {
            let mut buf = vec![0u8; len];
            let bytes = v.to_ne_bytes();
            let k = len.min(8);
            buf[..k].copy_from_slice(&bytes[..k]);
            Write::write(&mut &*efd, &buf).map(|n| {
                assert_eq!(n, 8);
                0
            })
        }
        Op::ReadBuf(len) => {
            let mut buf = vec![0u8; len];
            Read::read(&mut &*efd, &mut buf).map(|n| {
                assert_eq!(n, 8);
                u64::from_ne_bytes(buf[..8].try_into().unwrap())
            })
        }
    };
    res.map_err(|e| errno_of(&e))
}

fn kernel_op(fd: RawFd, op: &Op) -> Outcome {
    match *op {
        Op::Write(v) => kernel_write(fd, v, 8),
        Op::Read => kernel_read(fd, 8),
        Op::WriteBuf(v, len) => kernel_write(fd, v, len),
        Op::ReadBuf(len) => kernel_read(fd, len),
    }
}

/// `POLLIN` readiness, with a zero timeout.
fn pollin(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert!(n >= 0, "poll: {}", io::Error::last_os_error());
    pfd.revents & libc::POLLIN != 0
}

fn value() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => Just(0u64),
        6 => Just(1u64),
        2 => Just(2u64),
        2 => Just(EFD_MAX),
        2 => Just(EFD_MAX - 1),
        1 => Just(EFD_MAX - 2),
        2 => Just(u64::MAX),
        1 => Just(1u64 << 63),
        1 => Just(u64::from(u32::MAX)),
        3 => 0u64..16,
        3 => any::<u64>(),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => value().prop_map(Op::Write),
        5 => Just(Op::Read),
        1 => (value(), 0usize..=16).prop_map(|(v, len)| Op::WriteBuf(v, len)),
        1 => (0usize..=16).prop_map(Op::ReadBuf),
    ]
}

fn initval() -> impl Strategy<Value = u32> {
    prop_oneof![Just(0u32), Just(1u32), Just(u32::MAX), any::<u32>()]
}

fn flags(semaphore: bool) -> EfdFlags {
    if semaphore {
        EfdFlags::NONBLOCK | EfdFlags::SEMAPHORE
    } else {
        EfdFlags::NONBLOCK
    }
}

fn run(initval: u32, semaphore: bool, ops: &[Op]) -> Result<(), TestCaseError> {
    let steps: Vec<Step> = ops.iter().cloned().map(|op| Step::Io(0, op)).collect();
    run_steps(initval, semaphore, &steps)
}

/// `GkEventFd` handles (`try_clone`) against kernel descriptors (`dup`).
fn run_steps(initval: u32, semaphore: bool, steps: &[Step]) -> Result<(), TestCaseError> {
    let mut shim = vec![GkEventFd::new(initval, flags(semaphore)).unwrap()];
    let mut kernel = vec![kernel_eventfd(initval, semaphore)];

    let check_ready = |shim: &[GkEventFd], kernel: &[OwnedFd], what: &str| {
        for (i, (s, k)) in shim.iter().zip(kernel).enumerate() {
            prop_assert_eq!(
                pollin(s.as_raw_fd()),
                pollin(k.as_raw_fd()),
                "readiness of handle {} after {}",
                i,
                what
            );
        }
        Ok(())
    };
    check_ready(&shim, &kernel, "create")?;
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Io(h, op) => {
                let h = h % shim.len();
                let s = shim_op(&shim[h], op);
                let k = kernel_op(kernel[h].as_raw_fd(), op);
                prop_assert_eq!(s, k, "step #{} {:?}: shim vs kernel", i, step);
            }
            Step::Dup(h) => {
                let h = h % shim.len();
                shim.push(shim[h].try_clone().unwrap());
                kernel.push(kernel[h].try_clone().unwrap());
            }
            Step::Close(h) => {
                if shim.len() > 1 {
                    let h = h % shim.len();
                    drop(shim.remove(h));
                    drop(kernel.remove(h));
                }
            }
        }
        check_ready(&shim, &kernel, &format!("step #{i} {step:?}"))?;
    }
    // Final state: drain both and compare what is left.
    let s = shim_op(&shim[0], &Op::Read);
    let k = kernel_op(kernel[0].as_raw_fd(), &Op::Read);
    prop_assert_eq!(s, k, "final read");
    Ok(())
}

/// Steps of the compat (C-style) differential.
#[derive(Debug, Clone)]
enum CStep {
    Write(usize, u64),
    Read(usize),
    /// `eventfd_dup` vs `dup(2)`.
    Dup(usize),
    /// Raw `dup(2)` on both, then a no-op write through the copy: the shim
    /// adopts a raw dup the first time it is passed to compat (a raw dup that
    /// is never used before every known descriptor is closed is a documented
    /// limitation, see `compat`).
    RawDup(usize),
    /// `eventfd_dup2(a, b)` vs `dup2(2)` between two live handles.
    Dup2(usize, usize),
    /// `eventfd_close` vs `close(2)` (skipped for the last handle).
    Close(usize),
}

fn compat_read(fd: RawFd) -> Outcome {
    let mut v = 0;
    // SAFETY: `v` is a valid eventfd_t.
    match unsafe { compat::eventfd_read(fd, &mut v) } {
        0 => Ok(v),
        _ => Err(last_errno()),
    }
}

fn compat_write(fd: RawFd, v: u64) -> Outcome {
    match compat::eventfd_write(fd, v) {
        0 => Ok(0),
        _ => Err(last_errno()),
    }
}

/// The compat registry (shim) against raw syscalls on the kernel eventfd.
/// Both sides use raw descriptor numbers that this function owns.
fn run_compat(initval: u32, semaphore: bool, steps: &[CStep]) -> Result<(), TestCaseError> {
    let bits = flags(semaphore).bits();
    let first = compat::eventfd(initval, bits);
    prop_assert!(first >= 0);
    let mut shim = vec![first];
    let mut kernel = vec![kernel_eventfd(initval, semaphore).into_raw_fd()];

    let result = (|| {
        for (i, step) in steps.iter().enumerate() {
            let n = shim.len();
            match *step {
                CStep::Write(h, v) => {
                    let (s, k) = (
                        compat_write(shim[h % n], v),
                        kernel_write(kernel[h % n], v, 8),
                    );
                    prop_assert_eq!(s, k, "step #{} {:?}", i, step);
                }
                CStep::Read(h) => {
                    let (s, k) = (compat_read(shim[h % n]), kernel_read(kernel[h % n], 8));
                    prop_assert_eq!(s, k, "step #{} {:?}", i, step);
                }
                CStep::Dup(h) => {
                    let s = compat::eventfd_dup(shim[h % n]);
                    // SAFETY: plain dup of a descriptor we own.
                    let k = unsafe { libc::dup(kernel[h % n]) };
                    prop_assert!(s >= 0 && k >= 0, "step #{} {:?}", i, step);
                    shim.push(s);
                    kernel.push(k);
                }
                CStep::RawDup(h) => {
                    // SAFETY: plain dups of descriptors we own.
                    let (s, k) = unsafe { (libc::dup(shim[h % n]), libc::dup(kernel[h % n])) };
                    prop_assert!(s >= 0 && k >= 0, "step #{} {:?}", i, step);
                    let (ws, wk) = (compat_write(s, 0), kernel_write(k, 0, 8));
                    prop_assert_eq!(ws, wk, "first use of raw dup, step #{} {:?}", i, step);
                    shim.push(s);
                    kernel.push(k);
                }
                CStep::Dup2(a, b) => {
                    let (a, b) = (a % n, b % n);
                    // SAFETY: both targets are descriptors we own.
                    let (s, k) = unsafe {
                        (
                            compat::eventfd_dup2(shim[a], shim[b]),
                            libc::dup2(kernel[a], kernel[b]),
                        )
                    };
                    prop_assert_eq!(s, shim[b], "step #{} {:?}", i, step);
                    prop_assert_eq!(k, kernel[b], "step #{} {:?}", i, step);
                }
                CStep::Close(h) => {
                    if n > 1 {
                        let h = h % n;
                        prop_assert_eq!(compat::eventfd_close(shim.remove(h)), 0);
                        // SAFETY: we own it.
                        prop_assert_eq!(unsafe { libc::close(kernel.remove(h)) }, 0);
                    }
                }
            }
            for (j, (&s, &k)) in shim.iter().zip(&kernel).enumerate() {
                prop_assert_eq!(
                    pollin(s),
                    pollin(k),
                    "readiness of {} after step #{} {:?}",
                    j,
                    i,
                    step
                );
            }
        }
        // Every handle must still see the same final counter.
        let (s, k) = (compat_read(shim[0]), kernel_read(kernel[0], 8));
        prop_assert_eq!(s, k, "final read");
        Ok(())
    })();

    for fd in shim {
        compat::eventfd_close(fd);
    }
    for fd in kernel {
        // SAFETY: we own it.
        unsafe { libc::close(fd) };
    }
    result
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => (0usize..8, op()).prop_map(|(h, op)| Step::Io(h, op)),
        2 => (0usize..8).prop_map(Step::Dup),
        1 => (0usize..8).prop_map(Step::Close),
    ]
}

fn cstep() -> impl Strategy<Value = CStep> {
    prop_oneof![
        5 => (0usize..8, value()).prop_map(|(h, v)| CStep::Write(h, v)),
        5 => (0usize..8).prop_map(CStep::Read),
        1 => (0usize..8).prop_map(CStep::Dup),
        1 => (0usize..8).prop_map(CStep::RawDup),
        1 => (0usize..8, 0usize..8).prop_map(|(a, b)| CStep::Dup2(a, b)),
        1 => (0usize..8).prop_map(CStep::Close),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2048,
        ..ProptestConfig::default()
    })]

    #[test]
    fn shim_matches_kernel(
        initval in initval(),
        semaphore in any::<bool>(),
        ops in prop::collection::vec(op(), 0..64),
    ) {
        run(initval, semaphore, &ops)?;
    }

    #[test]
    fn shim_handles_match_kernel_dups(
        initval in initval(),
        semaphore in any::<bool>(),
        steps in prop::collection::vec(step(), 0..64),
    ) {
        run_steps(initval, semaphore, &steps)?;
    }

    #[test]
    fn compat_dups_match_kernel(
        initval in initval(),
        semaphore in any::<bool>(),
        steps in prop::collection::vec(cstep(), 0..48),
    ) {
        // The compat registry is process-wide; keep its cases sequential.
        let _g = compat_lock();
        run_compat(initval, semaphore, &steps)?;
    }
}

/// Serialises the compat cases (proptest may run tests on several threads).
fn compat_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hand-picked edge cases that random generation may hit only rarely.
#[test]
fn edge_cases_match_kernel() {
    use Op::*;
    let cases: &[(u32, bool, &[Op])] = &[
        (0, false, &[Write(EFD_MAX), Write(1), Write(0), Read, Read]),
        (
            0,
            true,
            &[Write(EFD_MAX), Write(1), Read, Write(1), Write(1)],
        ),
        (
            u32::MAX,
            false,
            &[Write(EFD_MAX - u64::from(u32::MAX)), Write(1), Read],
        ),
        (0, false, &[Write(u64::MAX), Read, Write(0), Read]),
        (3, true, &[Read, Read, Read, Read, Write(0), Read]),
        (
            1,
            false,
            &[ReadBuf(7), ReadBuf(8), WriteBuf(5, 4), WriteBuf(5, 9), Read],
        ),
        (
            0,
            false,
            &[WriteBuf(u64::MAX, 8), WriteBuf(u64::MAX, 16), Read],
        ),
    ];
    for (initval, semaphore, ops) in cases {
        run(*initval, *semaphore, ops)
            .unwrap_or_else(|e| panic!("init {initval} sem {semaphore} {ops:?}: {e}"));
    }
}

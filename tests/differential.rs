//! Differential test: the portable unix shim (forced on Linux with the
//! `force-shim` feature) against the real kernel `eventfd(2)`.
//!
//! Random operation sequences run against both a [`GkEventFd`] and a raw
//! `libc::eventfd`. After every operation the results (values and errno) and
//! the `POLLIN` readiness of both descriptors must be identical.
//!
//! Run with: `cargo test --features force-shim --test differential`
//! (`PROPTEST_CASES=N` raises the number of cases.)

#![cfg(all(target_os = "linux", feature = "force-shim"))]

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use adaptif_gatotkaca_eventfd::{EFD_MAX, EfdFlags, GkEventFd};
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

fn run(initval: u32, semaphore: bool, ops: &[Op]) -> Result<(), TestCaseError> {
    let mut flags = EfdFlags::NONBLOCK;
    if semaphore {
        flags |= EfdFlags::SEMAPHORE;
    }
    let shim = GkEventFd::new(initval, flags).unwrap();
    let kernel = kernel_eventfd(initval, semaphore);
    let kfd = kernel.as_raw_fd();

    prop_assert_eq!(
        pollin(shim.as_raw_fd()),
        pollin(kfd),
        "readiness after create"
    );
    for (i, op) in ops.iter().enumerate() {
        let s = shim_op(&shim, op);
        let k = kernel_op(kfd, op);
        prop_assert_eq!(s, k, "op #{} {:?}: shim vs kernel", i, op);
        prop_assert_eq!(
            pollin(shim.as_raw_fd()),
            pollin(kfd),
            "readiness after op #{} {:?}",
            i,
            op
        );
    }
    // Final state: drain both and compare what is left.
    let s = shim_op(&shim, &Op::Read);
    let k = kernel_op(kfd, &Op::Read);
    prop_assert_eq!(s, k, "final read");
    Ok(())
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

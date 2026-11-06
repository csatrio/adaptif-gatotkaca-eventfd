# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Planned as 0.2.0.

### Added

- `GkChannel` (module `channel`): a one-way channel between threads or
  processes through shared memory, with `GkSender` / `GkReceiver` ends and
  two independent lanes (not ordered relative to each other):
  - messages: atomic, up to `max_message` bytes, zero-length allowed,
    in-place `reserve`/`commit` (`SendSlot`) and `recv_with`;
  - bulk transfers of any size, sliced into chunks and reassembled:
    `send_bulk` / `recv_bulk_vec`, or streamed with `BulkWriter`
    (`io::Write`, declared or unknown length, `finish`, `pause` +
    `resume_bulk`) and `BulkReader` (`io::Read`, `for_each_chunk`,
    `pause`). Each transfer has an id; lost/out-of-order chunks, length
    mismatches and abandoned transfers (`ConnectionAborted`) are detected.
  - a 16-bit kind on every message and transfer, chosen by the application
    to say what the payload is: `send_kind`, `reserve_kind`, `recv_kind`,
    `recv_vec_kind`, `recv_with_kind`, `next_kind`, `send_bulk_kind`,
    `bulk_writer_kind`, `recv_bulk_vec_kind`, `BulkReader::kind`. The plain
    calls send `KIND_UNSPECIFIED` (0).
  `ChannelOptions` defaults: message ring 1 MiB, bulk ring 4 MiB, 64 KiB
  messages and chunks (Windows: 256 KiB and 1 MiB rings). Blocking and
  non-blocking modes, polling (`AsFd`/`AsSocket`, mio) with one doorbell
  for both lanes, closing with `BrokenPipe`, dead-peer detection, and
  ownership across `fork` (`claim`, `detach`). Lock-free single-producer /
  single-consumer rings with this crate's eventfds as doorbells; blocking
  calls spin briefly before sleeping.
- `GkProcess::send` takes any `Shareable` object (`GkEventFd`, `GkSender`,
  `GkReceiver`), and `GkProcess::recv_as::<T>()` receives one; `recv()`
  still returns a `GkEventFd`. Unix: `GkSender`/`GkReceiver::send_to` /
  `recv_from` over a `UnixStream`.
- C: `gk_channel_options` + `gk_channel_options_default`,
  `gk_channel_new`, `gk_sender_send`, `gk_receiver_recv`,
  `gk_receiver_next_len`, bulk `gk_sender_send_bulk`,
  `gk_sender_bulk_begin`/`_write`/`_finish`/`_abort`,
  `gk_receiver_bulk_begin`/`_read`/`_end`, kind variants
  `gk_sender_send_kind`, `gk_receiver_recv_kind`, `gk_receiver_next_kind`,
  `gk_sender_send_bulk_kind`, `gk_sender_bulk_begin_kind`,
  `gk_receiver_bulk_begin_kind` (`GK_KIND_UNSPECIFIED`),
  `gk_sender_/gk_receiver_set_nonblocking`,
  `_claim`, `_detach`, `_free`, `gk_sender_fd`/`gk_receiver_fd` (unix),
  `gk_sender_socket`/`gk_receiver_socket` (Windows),
  `gk_process_send_sender`/`_receiver`, `gk_process_recv_sender`/`_receiver`.
  Example: `examples/c/channel.c`.
- `examples/channel_bench.rs`: messages, bulk throughput and message
  latency during a transfer, against a pipe and a unix socket.
- `GkSharedMemory` (module `shared_memory`): zero-filled shared memory with
  the same API on every OS (`new`, `with_options`, `as_ptr`, `len`,
  `page_size`, `is_huge`, `unsafe as_slice` / `as_mut_slice`, `AsFd` on
  unix, `AsHandle` on Windows). Shared with `GkProcess::send` /
  `recv_as::<GkSharedMemory>()` on every OS, `send_to` / `recv_from` over a
  `UnixStream` on unix, and across `fork()`; both processes keep using it.
- `SharedMemoryOptions` and `HugePages` (`Off`, `Prefer`, `Require`): huge
  (large) pages where the OS has them (Linux hugetlb or transparent huge
  pages, FreeBSD large pages, Windows large pages), and `populate`.
- C: `gk_shm` with `gk_shm_options` (`GK_HUGE_PAGES_*`),
  `gk_shm_options_default`, `gk_shm_new`, `gk_shm_new_ex`, `gk_shm_ptr`,
  `gk_shm_len`, `gk_shm_page_size`, `gk_shm_is_huge`, `gk_shm_fd` (unix) /
  `gk_shm_handle` (Windows), `gk_shm_free`, `gk_process_send_shm`,
  `gk_process_recv_shm`. errno now also maps `Unsupported` to `ENOTSUP`,
  and on Windows a missing privilege to `EPERM`. Example:
  `examples/c/shared_memory.c`.

### Fixed

- Windows: `GkProcess::recv_as::<GkSender>` / `<GkReceiver>` failed with
  "eventfd message too long". A channel end's offer is about 5 KB (two
  eventfds, each with two socket protocol infos, plus a section handle), over
  the pipe transport's 4096-byte receive cap. The cap is now 16 KiB, still a
  bound on the allocation.
- Windows: `GkProcess::recv` reports `InvalidData` (not `InvalidInput`) when
  something other than an eventfd was sent, as on unix.

## [0.1.0] - 2026-11-06

First release.

### Added

- `GkEventFd`: a cross-platform eventfd with Linux `eventfd(2)` semantics
  (`new`, `with_raw_flags`, `read`, `write`, `try_clone`, `set_nonblocking`,
  `is_nonblocking`, and 8-byte `io::Read`/`io::Write`), plus `EfdFlags`, the
  `EFD_SEMAPHORE`, `EFD_CLOEXEC`, `EFD_NONBLOCK`, `EFD_MAX` constants and the
  `eventfd_t` type.
- Native backend for Linux, Android, FreeBSD 13+, NetBSD 10+ and illumos: the
  kernel eventfd itself, with `AsFd`/`AsRawFd` and `OwnedFd`/`RawFd`
  conversions.
- macOS/iOS/OpenBSD/DragonFly backend: the counter lives in an unnamed shared
  memory object (`memfd_create`, `SHM_ANON`, or `shm_open` + immediate
  `shm_unlink`), guarded by a cross-process spinlock (`crossbeam-utils`
  backoff, takeover from a holder that died), with a pipe that is readable
  exactly while the counter is non-zero. Shared across `fork()`.
- Windows backend: the counter lives in an unnamed shared memory section (same
  cross-process lock), with a loopback socket pair whose read socket
  (`AsSocket`) is readable exactly while the counter is non-zero.
- Sharing with other processes on every OS, modelled on Linux:
  - spawned children: `GkEventFd::export_inheritable` (`InheritableExport`) +
    `GkEventFd::import`; on native platforms the token is the inherited
    eventfd's number;
  - unrelated processes: `GkEventFd::send_to` / `GkEventFd::recv_from` over a
    `UnixStream` on unix (`SCM_RIGHTS`), `GkEventFd::export_to(pid)` +
    `import` on Windows;
  - one API for all of them: `GkProcess` and `GkListener` (module `process`):
    `GkProcess::spawn` / `from_parent`, `GkListener::bind` / `accept`,
    `GkProcess::connect`, then `send` / `recv`. Unix sockets underneath on
    unix; named pipes on Windows that record the peer's pid, so the receiver
    pulls the handles into itself and `recv` is safe. A name already in use
    fails with `ErrorKind::AddrInUse` on every OS.
- `compat` module with `<sys/eventfd.h>`-style functions and C semantics (`-1`
  and `errno`): `eventfd`, `eventfd_read`, `eventfd_write`, `eventfd_close`,
  `eventfd_read_buf`/`eventfd_write_buf`, `eventfd_dup`/`eventfd_dup2`/
  `eventfd_dup3` (unix), `eventfd_export_inheritable`/`eventfd_export_release`/
  `eventfd_import`, `eventfd_send`/`eventfd_recv` (unix),
  `eventfd_export_to`/`eventfd_socket` (Windows), and
  `eventfd_process_send`/`eventfd_process_recv`. On macOS/BSD raw `dup()`
  copies are recognised by their pipe identity.
- `ffi` feature: C ABI with `gk_`-prefixed symbols for all of the above, the
  process API as opaque `gk_process`/`gk_listener` handles, and the
  `include/adaptif_gatotkaca_eventfd.h` header. On shim platforms the header
  also provides the `<sys/eventfd.h>` names and an opt-in
  `GK_EVENTFD_WRAP_IO` redirect of raw `read`/`write`/`close`/`dup`/`dup2`/
  `dup3`. On Windows, forwarded CRT calls return `-1`/`errno` for invalid
  descriptors instead of aborting the process.
- `mio` feature: `mio::event::Source` implementation.
- `force-shim` feature: use the unix shim on native platforms, for testing
  the shim against the kernel eventfd.

[Unreleased]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/releases/tag/v0.1.0

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.3] - 2026-11-26

### Added

- `process_alive(pid: u32) -> bool` (`adaptif_gatotkaca_eventfd::process_alive`,
  also `process::process_alive`): the crate's own liveness check, the one
  behind `peer_gone()`, blocking channel calls noticing a dead peer and the
  shims' lock takeover, now public. `false` only on evidence that the
  process has exited or does not exist; `true` while it runs and whenever
  the OS can't tell (e.g. `EPERM` for another user's process whose state
  the OS won't show, Windows access denied). An unreaped zombie counts as
  exited as in 0.3.2: own children via `waitid(WNOWAIT)` (never reaped by
  the check), others on Linux, Android, macOS, iOS, FreeBSD and NetBSD;
  Windows `OpenProcess` + a zero-timeout wait (exited even while handles
  keep the process). Pid 0, and on unix a pid above `i32::MAX`, is not
  looked up (`true`).
- C: `int gk_process_alive(uint32_t pid)` (1 or 0). `examples/c/process.c`
  uses it.

### Changed

- The internal liveness check no longer looks up pid 0 or (unix) a pid
  above `i32::MAX`, which cast to `pid_t` named a process group for
  `kill(2)`; it answers `true` for them. No internal caller passed such a
  pid.

## [0.3.2] - 2026-11-26

### Added

- `GkListener::bind_with(name, ListenerAccess)` and the `ListenerAccess`
  enum (`Default`, `CurrentUser`; non-exhaustive). `CurrentUser` on
  Windows creates every named-pipe instance (the first and each later one)
  with the protected DACL `D:P(A;;FA;;;<user SID>)`, so only the current
  user can connect, with no window of default security; on unix it makes
  the socket file `0600` and `accept` closes connections whose peer's
  effective uid differs. `GkListener::bind` is unchanged
  (`ListenerAccess::Default`).
- C: `gk_listener_bind_ex(name, access)` with `GK_LISTENER_ACCESS_DEFAULT`
  and `GK_LISTENER_ACCESS_CURRENT_USER` (other values `EINVAL`).
  `examples/c/process.c` uses it.

### Fixed

- A process that has exited but not been reaped by its parent (a zombie)
  now counts as dead wherever the crate checks another process's liveness:
  `peer_gone()` on channel ends, blocking channel calls noticing a dead
  peer, and the shims' takeover of a cross-process lock whose holder died.
  Before, `kill(pid, 0)` kept a zombie alive, so a lock held by a killed,
  unreaped process (e.g. a sibling whose parent had not called `wait` yet)
  made the other processes spin until it was reaped. Detected cheaply and
  without allocating: Linux/Android `/proc/<pid>/stat` (state `Z` with one
  thread, or `X`), macOS/iOS `proc_pidinfo` (which no longer finds an
  exited process), FreeBSD `kinfo_proc.ki_stat == SZOMB`, NetBSD
  `kinfo_proc2.p_stat == SZOMB`. Own children were already handled
  (`waitid(WNOWAIT)`); OpenBSD, DragonFly and illumos are unchanged (a
  zombie counts as alive until reaped); Windows was already right.

## [0.3.1] - 2026-11-26

### Added

- `GkProcess::send_bytes` / `GkProcess::recv_bytes`: small framed byte
  messages (0 to `process::BYTES_MAX` = 64 KiB; more is `InvalidInput`) on
  the same connection as objects, in order, for handshakes and bootstraps.
  Receiving the wrong kind (bytes where an object is expected, or the
  reverse) is `InvalidData` and consumes the message, so the connection
  stays in sync.
- `GkProcess::peer_pid()` on every OS: `SO_PEERCRED` (Linux, Android,
  OpenBSD), `LOCAL_PEERPID` (macOS, iOS), `LOCAL_PEERCRED` (FreeBSD 13+),
  `LOCAL_PEEREID` (NetBSD), `getpeerucred` (illumos), the named pipe's
  client/server pid (Windows); `None` where unknown (DragonFly, or not
  connected yet). Read when the connection is made.
- Unix: `AsFd` and `AsRawFd` for `GkProcess`, so the connection can be
  polled: readable on a message, and at EOF once the peer exits (a
  lifeline).
- C: `gk_process_send_bytes`, `gk_process_recv_bytes` (`EMSGSIZE` keeps a
  message that doesn't fit), `GK_PROCESS_BYTES_MAX`, `gk_process_peer_pid`,
  `gk_process_fd` (unix). `examples/c/process.c` shows them.

### Changed

- Windows: `GkProcess::pid()` is now the same as `peer_pid()` (kept for
  compatibility).

## [0.3.0] - 2026-11-26

### Added

- Multi-producer channels: `GkChannel::new_mpsc()` /
  `mpsc_with_options()` return a `GkMpSender` (`Clone + Send + Sync`) and a
  plain `GkReceiver`. Messages are lock-free (`send`, `send_kind`,
  `try_send`, `try_send_kind`, `reserve`/`try_reserve` + `MpSendSlot`);
  each thread's messages keep their order. Bulk transfers through
  `GkMpSender::bulk()` (`MpBulk`), one at a time. All clones belong to one
  process; the end moves between processes with `GkProcess` /
  `send_to`/`recv_from` and survives `fork` (`claim`, `detach`).
- `GkReceiver::recv_batch(max, f)`: receive up to `max` messages in place,
  freeing the batch's space and waking the sender once.
- `peer_gone()` and `peer_pid()` on `GkSender`, `GkReceiver` and
  `GkMpSender`, for non-blocking users that must notice a dead peer.
- Zero-copy pools (module `pool`): `GkShmPool` (allocating side, buddy
  allocator over `granule`-sized blocks, `ShmPoolOptions`, `stats`,
  `reclaim_all`), `ShmBlock`, `ShmLease` (16 bytes), `GkShmPoolView`
  (borrowing side, shareable with `GkProcess`/`send_to`/`fork`) and
  `ShmBuf` (read in place; dropping it gives the block back, lock-free).
  Stale and duplicate returns are refused by generation.
- C: `gk_channel_new_mpsc`, `gk_mp_sender_*`,
  `gk_process_send_mp_sender` / `gk_process_recv_mp_sender`,
  `gk_receiver_recv_batch` (`gk_recv_fn`), `gk_sender_peer_gone`,
  `gk_receiver_peer_gone`, `gk_shm_pool_*`, `gk_shm_block_*`,
  `gk_shm_buf_*`, `gk_shm_pool_view_*`, `gk_process_send_shm_pool_view` /
  `gk_process_recv_shm_pool_view`. Example: `examples/c/mpsc_pool.c`.
- `examples/channel_bench.rs`: `recv_batch`, multi-producer and pool
  numbers.
- `rustfmt.toml` (140 columns) and crate lints (`unsafe_op_in_unsafe_fn`,
  `missing_docs`, `undocumented_unsafe_blocks`, `missing_safety_doc`).

### Changed

- Channel readiness is guaranteed for level-triggered pollers: a fresh
  channel starts with the receiver waiting (the first message rings it),
  only `WouldBlock` clears readiness, and data left in the other lane rings
  the doorbell again when one lane returns `WouldBlock`.
- Channel shared-memory layout version 4 (a mode word, the receiver's
  initial waiting flags). 0.2 and 0.3 processes refuse each other's channel
  ends (`InvalidData`).
- An end of the wrong mode (multi-producer received as `GkSender`, or the
  reverse) is refused with `InvalidData` and closed, so the peer gets
  `BrokenPipe`.

## [0.2.0] - 2026-11-07

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
  `ChannelOptions::huge_pages` puts the rings on huge pages (`HugePages`,
  as for `GkSharedMemory`; C: `gk_channel_options.huge_pages`).
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

### Changed

- Shared memory internals (eventfd shim counters, channel rings) now round
  their size up to the page size. FreeBSD creates them with
  `memfd_create` instead of `shm_open(SHM_ANON)`.

### Security

- Linux, Android and FreeBSD: shared memory objects are created with their
  size sealed (`F_SEAL_SHRINK | F_SEAL_GROW`), and objects received from
  another process are refused (`InvalidData`) unless sealed. Before, a peer
  could shrink a shared object and crash this process with `SIGBUS` on its
  next access. macOS objects can't be resized after creation. NetBSD,
  illumos, OpenBSD and DragonFly have no sealing: there a peer you share
  with can still do this.

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

[Unreleased]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/csatrio/adaptif-gatotkaca-eventfd/releases/tag/v0.1.0

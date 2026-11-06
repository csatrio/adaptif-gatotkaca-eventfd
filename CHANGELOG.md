# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

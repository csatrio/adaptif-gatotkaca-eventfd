# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- One way to share an eventfd with other processes on every OS, modelled on
  Linux: `GkEventFd::export_inheritable` + `GkEventFd::import` for spawned
  children (all OS; on native platforms the token is the inherited eventfd's
  number), and `GkEventFd::send_to` / `GkEventFd::recv_from` over a
  `UnixStream` for unrelated processes on unix (`SCM_RIGHTS`); Windows keeps
  `export_to(pid)`. C-style `compat::eventfd_export_inheritable`,
  `eventfd_export_release`, `eventfd_import` (all OS), `eventfd_send`,
  `eventfd_recv` (unix) and the matching `gk_eventfd_*` exports.
- `GkProcess` and `GkListener` (module `process`): one API on every OS to
  hand eventfds to other processes. `GkProcess::spawn` / `from_parent` for
  children, `GkListener::bind` / `accept` and `GkProcess::connect` for
  unrelated processes, then `send` / `recv`. Unix sockets underneath on
  unix; named pipes on Windows, recording the peer's pid so the receiver
  pulls the handles into itself (safe `recv`, no token import).

### Changed

- Unix shims (macOS, iOS, OpenBSD, DragonFly): the counter page is an
  unnamed shared memory object (`memfd_create`, `SHM_ANON`, or `shm_open` +
  immediate `shm_unlink`) instead of an anonymous mapping, so it can be
  inherited across `exec` and passed over unix sockets. Each eventfd uses one
  more descriptor.

### Fixed

- Windows: `gk_read`, `gk_write`, `gk_close`, `gk_dup` and `gk_dup2` no longer
  abort the process (`STATUS_STACK_BUFFER_OVERRUN`) when forwarding an invalid
  descriptor such as `-1` to the CRT. The forwarded call now runs with a
  thread-local no-op invalid-parameter handler (restored afterwards), so it
  returns `-1` with `errno` set, as on POSIX. Eventfd fast paths are unaffected.
- Windows tests: `recovers_when_child_is_killed_holding_the_lock` never saw the
  child's `locked` line because libtest prefixes it under `--nocapture`; it now
  matches the end of the line.
- Windows tests: `clones_with_distinct_tokens_both_fire` drained through one
  clone and expected another clone's mio registration to re-arm. It now drains
  the registered clone to `WouldBlock`, matching the documented Windows mio
  limitation.

## [0.1.0] - Unreleased

### Added

- `GkEventFd`: a cross-platform eventfd with Linux `eventfd(2)` semantics
  (`new`, `with_raw_flags`, `read`, `write`, `try_clone`, `set_nonblocking`,
  `is_nonblocking`, and 8-byte `io::Read`/`io::Write`).
- Native backend for Linux, Android, FreeBSD 13+, NetBSD 10+ and illumos: the kernel eventfd, with `AsFd`/`AsRawFd` and
  `OwnedFd`/`RawFd` conversions.
- macOS/iOS/OpenBSD/DragonFly backend: counter in anonymous shared memory
  (shared across `fork()`), guarded by a cross-process spinlock with
  `crossbeam-utils` backoff and dead-holder takeover, plus a pipe that is
  readable while the counter is non-zero.
- Windows backend: counter in an unnamed shared memory section (same
  cross-process lock as the unix shim) plus a loopback socket pair whose read
  socket (`AsSocket`) is readable while the counter is non-zero.
- Windows sharing with other processes: `GkEventFd::export_inheritable`
  (`InheritableExport`), `GkEventFd::export_to(pid)` and
  `GkEventFd::import(token)`; C-style `compat::eventfd_export_inheritable`,
  `eventfd_export_release`, `eventfd_export_to`, `eventfd_import` and the
  `gk_eventfd_export_*`/`gk_eventfd_import` exports.
- `EfdFlags` and the `EFD_SEMAPHORE`, `EFD_CLOEXEC`, `EFD_NONBLOCK`, `EFD_MAX`
  constants, plus the `eventfd_t` type.
- `compat` module with `<sys/eventfd.h>`-style `eventfd`, `eventfd_read`,
  `eventfd_write` and `eventfd_close`, plus `eventfd_read_buf`/
  `eventfd_write_buf` (`read(2)`/`write(2)` with eventfd semantics),
  `eventfd_dup`/`eventfd_dup2`/`eventfd_dup3` (unix) and, on Windows,
  `eventfd_socket`. On macOS/BSD raw `dup()` copies are recognised by their
  pipe identity. C semantics: `-1` and `errno` on failure.
- `ffi` feature: C ABI (`gk_eventfd`, `gk_eventfd_read`, `gk_eventfd_write`,
  `gk_eventfd_close`, `gk_eventfd_read_buf`, `gk_eventfd_write_buf`,
  `gk_read`, `gk_write`, `gk_close`, `gk_dup`, `gk_dup2`, `gk_dup3` on unix,
  and `gk_eventfd_socket` on Windows) and
  the `include/adaptif_gatotkaca_eventfd.h` header, which also provides the
  `<sys/eventfd.h>` names on shim platforms and an opt-in
  `GK_EVENTFD_WRAP_IO` redirect of raw `read`/`write`/`close`/`dup`/`dup2`/`dup3`.
- `mio` feature: `mio::event::Source` implementation.
- `force-shim` feature: use the Unix shim on Linux for differential testing.

[Unreleased]: https://crates.io/crates/adaptif-gatotkaca-eventfd
[0.1.0]: https://crates.io/crates/adaptif-gatotkaca-eventfd/0.1.0

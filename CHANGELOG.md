# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - Unreleased

### Added

- `GkEventFd`: a cross-platform eventfd with Linux `eventfd(2)` semantics
  (`new`, `with_raw_flags`, `read`, `write`, `try_clone`, `set_nonblocking`,
  `is_nonblocking`, and 8-byte `io::Read`/`io::Write`).
- Native backend for Linux, Android, FreeBSD 13+, NetBSD 10+ and illumos: the kernel eventfd, with `AsFd`/`AsRawFd` and
  `OwnedFd`/`RawFd` conversions.
- macOS/iOS/OpenBSD/DragonFly backend: user-space counter plus a pipe that is readable while
  the counter is non-zero.
- Windows backend: user-space counter plus a loopback socket pair whose read
  socket (`AsSocket`) is readable while the counter is non-zero.
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

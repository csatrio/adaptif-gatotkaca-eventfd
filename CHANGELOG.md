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
- Linux/Android backend: the kernel eventfd, with `AsFd`/`AsRawFd` and
  `OwnedFd`/`RawFd` conversions.
- macOS/iOS/BSD backend: user-space counter plus a pipe that is readable while
  the counter is non-zero.
- Windows backend: user-space counter plus a loopback socket pair whose read
  socket (`AsSocket`) is readable while the counter is non-zero.
- `EfdFlags` and the `EFD_SEMAPHORE`, `EFD_CLOEXEC`, `EFD_NONBLOCK`, `EFD_MAX`
  constants, plus the `eventfd_t` type.
- `compat` module with `<sys/eventfd.h>`-style `eventfd`, `eventfd_read`,
  `eventfd_write` and `eventfd_close`.
- `ffi` feature: C ABI (`gk_eventfd*`) and the
  `include/adaptif_gatotkaca_eventfd.h` header.
- `mio` feature: `mio::event::Source` implementation.
- `force-shim` feature: use the Unix shim on Linux for differential testing.

[Unreleased]: https://crates.io/crates/adaptif-gatotkaca-eventfd
[0.1.0]: https://crates.io/crates/adaptif-gatotkaca-eventfd/0.1.0

# adaptif-gatotkaca-eventfd

[![crates.io](https://img.shields.io/crates/v/adaptif-gatotkaca-eventfd.svg)](https://crates.io/crates/adaptif-gatotkaca-eventfd)
[![docs.rs](https://docs.rs/adaptif-gatotkaca-eventfd/badge.svg)](https://docs.rs/adaptif-gatotkaca-eventfd)
![MSRV 1.85](https://img.shields.io/badge/MSRV-1.85-blue)

**A cross-platform `eventfd(2)` with full Linux semantics on every OS.**

`GkEventFd` is a 64-bit kernel-style event counter you can poll. On Linux and
Android it *is* the kernel eventfd. On macOS, iOS, the BSDs and Windows it is a
faithful shim: a user-space counter paired with a pollable descriptor that is
readable exactly while the counter is non-zero. So it plugs into `poll`,
`kqueue`, `select`, `WSAPoll`, mio and tokio.

It also ships a `<sys/eventfd.h>`-style `compat` module and an optional C ABI.
C code written for Linux eventfd can be ported with almost no changes.

## Platforms and backends

| Platform | Backend | Pollable object | Raw-fd conversions |
|---|---|---|---|
| Linux, Android | the kernel `eventfd(2)` (no shim) | the eventfd itself (`AsFd`, `AsRawFd`) | `From<OwnedFd>`, `Into<OwnedFd>`, `IntoRawFd`, `FromRawFd` |
| macOS, iOS, FreeBSD/NetBSD/OpenBSD/DragonFly | counter (`Mutex` + `Condvar`) + non-blocking pipe | read end of the pipe (`AsFd`, `AsRawFd`) | — |
| Windows | counter (`Mutex` + `Condvar`) + loopback TCP socket pair | read socket (`AsSocket`, `AsRawSocket`) | — |

On the shim backends the pipe or socket holds exactly one byte iff the counter
is greater than zero. That byte only signals readiness. The value itself lives
in the counter, so always read and write through this crate (see
[Limitations](#limitations)).

## Quick start

```toml
[dependencies]
adaptif-gatotkaca-eventfd = "0.1"
```

```rust
use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};

fn main() -> std::io::Result<()> {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;

    efd.write(2)?;
    efd.write(3)?;
    assert_eq!(efd.read()?, 5); // read returns the sum and resets to 0
    assert_eq!(efd.read().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);

    // Semaphore mode: each read returns 1 and decrements.
    let sem = GkEventFd::new(2, EfdFlags::SEMAPHORE | EfdFlags::NONBLOCK)?;
    assert_eq!(sem.read()?, 1);
    assert_eq!(sem.read()?, 1);
    assert!(sem.read().is_err());

    // Handles are Send + Sync; try_clone() works like dup(2).
    let other = efd.try_clone()?;
    std::thread::spawn(move || other.write(1).unwrap()).join().unwrap();
    assert_eq!(efd.read()?, 1);
    Ok(())
}
```

Other API:

- `GkEventFd::with_raw_flags(initval, EFD_NONBLOCK | EFD_CLOEXEC)` takes raw
  C `EFD_*` bits. Unknown bits fail with `EINVAL`.
- `set_nonblocking(bool)` / `is_nonblocking()`. Like `O_NONBLOCK`, the mode is
  shared by all clones.
- `io::Read` and `io::Write` are implemented for `GkEventFd` and
  `&GkEventFd`. They use 8-byte native-endian buffers, as with `read(2)` and
  `write(2)` on a real eventfd.

## mio

Enable the `mio` feature. `GkEventFd` then implements `mio::event::Source`.

```rust
use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use mio::{Events, Interest, Poll, Token};
use std::io::ErrorKind;

fn main() -> std::io::Result<()> {
    let mut poll = Poll::new()?;
    let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
    poll.registry().register(&mut efd, Token(0), Interest::READABLE)?;

    let notifier = efd.try_clone()?;
    std::thread::spawn(move || notifier.write(42).unwrap());

    let mut events = Events::with_capacity(8);
    loop {
        poll.poll(&mut events, None)?;
        for event in &events {
            if event.token() == Token(0) {
                match efd.read() {
                    Ok(n) => {
                        println!("woken with {n}");
                        return Ok(());
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
}
```

mio is edge-triggered, so keep reading until `WouldBlock`. In normal mode a
single read drains the counter. In semaphore mode, loop.

## tokio (Unix)

On Unix, wrap the eventfd in `tokio::io::unix::AsyncFd`. This needs tokio's
`net` feature.

```rust
use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let efd = Arc::new(AsyncFd::new(GkEventFd::new(0, EfdFlags::NONBLOCK)?)?);

    let notifier = Arc::clone(&efd);
    std::thread::spawn(move || notifier.get_ref().write(7).unwrap());

    loop {
        let mut guard = efd.readable().await?;
        match guard.try_io(|inner| inner.get_ref().read()) {
            Ok(value) => {
                println!("got {}", value?);
                return Ok(());
            }
            Err(_would_block) => continue, // readiness cleared, wait again
        }
    }
}
```

On Windows, use the `mio` feature with your own mio-based reactor, or poll the
socket returned by `AsSocket`.

## C usage

Build with the `ffi` feature to export a C ABI that matches
`<sys/eventfd.h>`. The header is
[`include/adaptif_gatotkaca_eventfd.h`](include/adaptif_gatotkaca_eventfd.h).
The exported symbols are prefixed with `gk_` (`gk_eventfd`,
`gk_eventfd_read`, `gk_eventfd_write`, `gk_eventfd_close`) so they never clash
with libc. On non-Linux platforms the header also maps the standard
`eventfd.h` names (`eventfd`, `eventfd_read`, `eventfd_write`, `eventfd_t`,
`EFD_*`) onto them. On Linux, the `gk_` functions pass straight through to
libc.

```c
#include <stdio.h>
#include "adaptif_gatotkaca_eventfd.h"

int main(void) {
    int fd = gk_eventfd(0, EFD_NONBLOCK);
    if (fd < 0) { perror("eventfd"); return 1; }

    gk_eventfd_write(fd, 3);
    gk_eventfd_write(fd, 4);

    eventfd_t value = 0;
    if (gk_eventfd_read(fd, &value) != 0) { perror("eventfd_read"); return 1; }
    printf("value = %llu\n", (unsigned long long)value); /* 7 */

    gk_eventfd_close(fd); /* not close(): see Limitations */
    return 0;
}
```

Build the static library and link it:

```sh
cargo rustc --release --features ffi --crate-type staticlib
# Print the system libraries the static lib needs on your platform:
cargo rustc --release --features ffi --crate-type staticlib -- --print native-static-libs

# Linux / macOS
cc -Iinclude main.c target/release/libadaptif_gatotkaca_eventfd.a -lpthread -lm -ldl -o main

# Windows (MSVC developer prompt)
cl /Iinclude main.c target\release\adaptif_gatotkaca_eventfd.lib ws2_32.lib userenv.lib ntdll.lib bcrypt.lib advapi32.lib kernel32.lib
```

The functions follow C conventions: they return `-1` and set `errno` on
failure. See [`examples/c/smoke.c`](examples/c/smoke.c) for a fuller example,
which CI runs on Linux, macOS and Windows.

From Rust, the same functions are available without the `ffi` feature in
`adaptif_gatotkaca_eventfd::compat`: `eventfd(initval, flags) -> c_int`,
`eventfd_read(fd, *mut eventfd_t)`, `eventfd_write(fd, eventfd_t)` and
`eventfd_close(fd)`.

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `mio` | off | `impl mio::event::Source for GkEventFd` |
| `ffi` | off | Export the C ABI (`gk_eventfd*`), matching `include/adaptif_gatotkaca_eventfd.h` |
| `force-shim` | off | Use the portable Unix shim even on Linux/Android. Meant for differential testing against the kernel, not production. Has no effect on other platforms. |

## Semantics

These match Linux `eventfd(2)` on every backend:

| Operation | Behavior |
|---|---|
| `write(n)` | Adds `n` to the counter |
| `write(u64::MAX)` | Fails with `EINVAL` (`ErrorKind::InvalidInput`) |
| `write` that would push the counter past `EFD_MAX` (`u64::MAX - 1`) | Blocks until a read makes room, or `EAGAIN` (`WouldBlock`) if non-blocking |
| `read()`, normal mode | Returns the counter and resets it to 0 |
| `read()`, `EFD_SEMAPHORE` | Returns 1 and decrements the counter |
| `read()` when the counter is 0 | Blocks, or `EAGAIN` (`WouldBlock`) if non-blocking |
| `io::Read` / `io::Write` with a buffer under 8 bytes | Fails with `EINVAL` (`InvalidInput`) |
| Readiness | Readable iff counter > 0 |

Flags use the Linux numeric values on all platforms: `EFD_SEMAPHORE = 1`,
`EFD_NONBLOCK = 0o4000`, `EFD_CLOEXEC = 0o2000000`. On Linux/Android they
come from `libc`, so they always match the target's kernel ABI. On Windows,
`EFD_CLOEXEC` makes the handle non-inheritable. The counter type is
`eventfd_t = u64`.

## Limitations

None of these apply on Linux/Android, where the descriptor is the kernel
eventfd. They apply to the shim backends (macOS, iOS, BSDs, Windows, and Linux
with `force-shim`):

- **Raw I/O has no eventfd semantics.** `read(2)`, `write(2)` or `recv` on
  the shim descriptor reach the internal signalling pipe or socket, not the
  counter. Use `GkEventFd::read`/`write` or the `compat` functions.
- **Close with `eventfd_close`.** A descriptor obtained from
  `compat::eventfd` (or `gk_eventfd` in C) is registered with the compat
  layer. Close it with `eventfd_close`, not a raw `close`/`closesocket`, or the
  counter leaks and the descriptor number may later map to a stale entry.
- **No sharing across `fork()`.** The counter lives in process memory. After
  `fork()`, parent and child have separate counters.
- **No write readiness.** `POLLOUT` and writability are not reported in a
  meaningful way. Only readability (counter > 0) is supported.
- **`dup()` is not tracked.** A plain `dup()`/`dup2()` of a compat descriptor
  is not registered with the compat layer. Use `GkEventFd::try_clone` in Rust.

## MSRV

The minimum supported Rust version is **1.85** (edition 2024). CI tests
against it. Raising the MSRV counts as a minor version bump.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

Copyright © 2026 Adaptif.

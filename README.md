# adaptif-gatotkaca-eventfd

[![crates.io](https://img.shields.io/crates/v/adaptif-gatotkaca-eventfd.svg)](https://crates.io/crates/adaptif-gatotkaca-eventfd)
[![docs.rs](https://docs.rs/adaptif-gatotkaca-eventfd/badge.svg)](https://docs.rs/adaptif-gatotkaca-eventfd)
![MSRV 1.85](https://img.shields.io/badge/MSRV-1.85-blue)

**A cross-platform `eventfd(2)` with full Linux semantics on every OS.**

`GkEventFd` is a 64-bit kernel-style event counter you can poll. On Linux, Android, FreeBSD 13+, NetBSD 10+ and illumos
it *is* the kernel eventfd. On macOS, iOS, OpenBSD, DragonFly and Windows it is a
faithful shim: a user-space counter paired with a pollable descriptor that is
readable exactly while the counter is non-zero. So it plugs into `poll`,
`kqueue`, `select`, `WSAPoll`, mio and tokio.

It also ships a `<sys/eventfd.h>`-style `compat` module and an optional C ABI.
C code written for Linux eventfd can be ported with almost no changes.

## Platforms and backends

| Platform | Backend | Pollable object | Raw-fd conversions |
|---|---|---|---|
| Linux, Android, FreeBSD 13+, NetBSD 10+, illumos | the kernel `eventfd(2)` (no shim) | the eventfd itself (`AsFd`, `AsRawFd`) | `From<OwnedFd>`, `Into<OwnedFd>`, `IntoRawFd`, `FromRawFd` |
| macOS, iOS, OpenBSD, DragonFly | counter in fork-shared memory (cross-process spinlock) + non-blocking pipe | read end of the pipe (`AsFd`, `AsRawFd`) | — |
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
`net` feature and tokio 1.53.3+ (or 1.51.6+ LTS) for `AsyncFd::register*`;
on older tokio use the now-deprecated `AsyncFd::new`.

```rust
use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use std::sync::Arc;
use tokio::io::{Interest, unix::AsyncFd};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
    // SAFETY: GkEventFd owns its fd until dropped and always returns the same one.
    let efd = Arc::new(unsafe { AsyncFd::register_with_interest(efd, Interest::READABLE) }?);

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
All exported symbols are prefixed with `gk_` on every platform, so they never
clash with libc:

| C function | Behavior |
|---|---|
| `int gk_eventfd(unsigned int initval, int flags)` | `eventfd(2)` |
| `int gk_eventfd_read(int fd, eventfd_t *value)` | `eventfd_read(3)` |
| `int gk_eventfd_write(int fd, eventfd_t value)` | `eventfd_write(3)` |
| `int gk_eventfd_close(int fd)` | `close(2)` of an eventfd |
| `intptr_t gk_eventfd_read_buf(int fd, void *buf, size_t count)` | `read(2)` of an eventfd: 8 or -1 (`EINVAL` if `count < 8`) |
| `intptr_t gk_eventfd_write_buf(int fd, const void *buf, size_t count)` | `write(2)` of an eventfd: 8 or -1 |
| `ssize_t gk_read(int fd, void *buf, size_t count)` | `read(2)` of *any* fd: eventfd semantics for ours, else the real `read` (`int`/`_read` on Windows) |
| `ssize_t gk_write(int fd, const void *buf, size_t count)` | `write(2)` of any fd (`int`/`_write` on Windows) |
| `int gk_close(int fd)` | `close(2)` of any fd (`_close` on Windows) |
| `int gk_dup(int fd)` | `dup(2)` of any fd: a new descriptor on the same counter for ours, else the real `dup` (`_dup` on Windows) |
| `int gk_dup2(int oldfd, int newfd)` | `dup2(2)` of any fd (`_dup2` on Windows: returns 0, and `newfd` must be `>= 65536` for eventfds) |
| `int gk_dup3(int oldfd, int newfd, int flags)` | Unix only: `dup3(2)` of any fd, `flags` is `0` or `O_CLOEXEC` (emulated on macOS) |
| `uintptr_t gk_eventfd_socket(int fd)` | Windows only: the `SOCKET` to `WSAPoll`/`select` on |

They follow C conventions: `-1` and `errno` on failure. Flags are
`GK_EFD_SEMAPHORE`, `GK_EFD_NONBLOCK` and `GK_EFD_CLOEXEC`.

The header computes `GK_EVENTFD_NATIVE` in one place: 1 on Linux, Android,
FreeBSD 13+, NetBSD 10+ and illumos, where it includes the system
`<sys/eventfd.h>` (so `EFD_*` have the OS's own values) and the `gk_`
functions pass straight through to libc. Elsewhere (macOS, iOS, OpenBSD,
DragonFly, Windows) it also defines the standard names `eventfd`,
`eventfd_read`, `eventfd_write` (as `static inline` wrappers), `eventfd_t`
and `EFD_*`, unless you define `GK_EVENTFD_NO_COMPAT_NAMES`.

```c
#include <stdio.h>
#include "adaptif_gatotkaca_eventfd.h"

int main(void) {
    int fd = gk_eventfd(0, GK_EFD_NONBLOCK);
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

To poll: on unix, `poll`/`select`/`kqueue` the returned fd itself (it is
readable iff the counter is non-zero). On Windows the fd is a synthetic
number (>= 65536, never a CRT fd); `WSAPoll`/`select` the socket from
`gk_eventfd_socket(fd)` and call `WSAStartup` yourself before using Winsock.

### Unchanged Linux code: `GK_EVENTFD_WRAP_IO`

Code that does raw `read(efd, &v, 8)` / `write(efd, &v, 8)` / `close(efd)` /
`dup(efd)` can be compiled unchanged on the shim platforms by defining
`GK_EVENTFD_WRAP_IO` before including the header. It then redirects
`read`/`write`/`close`/`dup`/`dup2` (plus `dup3` on unix, and on Windows also
`_read`/`_write`/`_close`/`_dup`/`_dup2`) to the matching `gk_*` functions, which give our eventfds eventfd semantics
and forward every other descriptor to the real function. It is a no-op where
`GK_EVENTFD_NATIVE` is 1. The redirects are function-like macros, so they only
affect calls in code compiled with this header (not prebuilt libraries, not
Rust), don't affect `&read` or function pointers, and do rewrite member calls
such as `obj->read(a, b, c)` (`#undef` them in such files). See
[`examples/c/wrap_io.c`](examples/c/wrap_io.c).

### Building

```sh
# Build the static library and print the system libraries it needs:
cargo rustc --release --features ffi --crate-type staticlib -- --print native-static-libs

# Linux / macOS (use the libraries printed above)
cc -std=c11 -Iinclude main.c target/release/libadaptif_gatotkaca_eventfd.a -lpthread -lm -ldl -o main

# Windows (MSVC developer prompt)
cl /MD /Iinclude main.c target\release\adaptif_gatotkaca_eventfd.lib ws2_32.lib userenv.lib ntdll.lib bcrypt.lib advapi32.lib kernel32.lib
```

On Windows, link the C program against the same dynamic CRT as Rust (`/MD`)
so both sides see the same `errno`.

[`examples/c/smoke.c`](examples/c/smoke.c) and
[`examples/c/wrap_io.c`](examples/c/wrap_io.c) are fuller examples. They
build warning-free with `-std=c11 -Wall -Wextra -Werror` (gcc, clang) and
`/W3 /WX` (MSVC), and exit 0 when every check passes.

From Rust, the same functions are available without the `ffi` feature in
`adaptif_gatotkaca_eventfd::compat`: `eventfd`, `eventfd_read`,
`eventfd_write`, `eventfd_close`, `eventfd_read_buf`, `eventfd_write_buf`,
`eventfd_dup`, `eventfd_dup2`, `eventfd_dup3` (unix) and (Windows)
`eventfd_socket`.

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `mio` | off | `impl mio::event::Source for GkEventFd` |
| `ffi` | off | Export the C ABI (`gk_eventfd*`), matching `include/adaptif_gatotkaca_eventfd.h` |
| `force-shim` | off | Use the portable Unix shim even where a native eventfd exists. Meant for differential testing against the kernel, not production. Has no effect on other platforms. |

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

On the native platforms (Linux, Android, FreeBSD 13+, NetBSD 10+ and illumos) the flag values come
from `libc`, so they always match that kernel's ABI (FreeBSD's `EFD_NONBLOCK`
is `0x4`, for example). Always use the `EFD_*` constants or `EfdFlags`, never
hard-coded numbers. On the shim platforms they use the Linux x86/arm values:
`EFD_SEMAPHORE = 1`, `EFD_NONBLOCK = 0o4000`, `EFD_CLOEXEC = 0o2000000`. On
Windows the sockets are always non-inheritable, with or without `EFD_CLOEXEC`. The counter type is
`eventfd_t = u64`.

## Limitations

None of these apply on Linux, Android, FreeBSD 13+, NetBSD 10+ and illumos, where the descriptor is
the kernel eventfd. They apply to the shim backends (macOS, iOS, OpenBSD,
DragonFly, Windows, and any platform built with `force-shim`):

- **Raw I/O has no eventfd semantics.** `read(2)`, `write(2)` or `recv` on
  the shim descriptor reach the internal signalling pipe or socket, not the
  counter. Use `GkEventFd::read`/`write`, the `compat` functions, or in C
  `gk_read`/`gk_write` (or `GK_EVENTFD_WRAP_IO`).
- **Close with `eventfd_close`.** A descriptor obtained from
  `compat::eventfd` (or `gk_eventfd` in C) is registered with the compat
  layer. Close it with `eventfd_close` (or `gk_close`), not a raw
  `close`/`closesocket`. (On unix the registry notices a number that was
  closed behind its back and reused, and forgets it without closing anything.)
- **`fork()` yes, `exec()` no (unix shims).** The counter lives in an
  anonymous shared memory page, so parent and child keep sharing it after
  `fork()`, including blocking reads woken from the other process. It is not
  carried across `exec()` or to unrelated processes. Windows has no `fork()`;
  its counter is per process.
- **Not async-signal-safe.** Operations take a short cross-process lock, so
  calling into an eventfd from a signal handler that interrupted an
  operation on the same eventfd deadlocks. (A process that dies holding the
  lock is detected and the lock is taken over.)
- **No write readiness.** `POLLOUT` and writability are not reported in a
  meaningful way. Only readability (counter > 0) is supported.
- **Duplicate through the crate.** Use `GkEventFd::try_clone` in Rust and
  `eventfd_dup`/`eventfd_dup2`/`eventfd_dup3` (C: `gk_dup*`, or plain
  `dup`/`dup2`/`dup3` under `GK_EVENTFD_WRAP_IO`). On macOS/BSD a raw
  `dup()`/`fcntl(F_DUPFD)` copy also works: it is recognised by its pipe the
  first time it is passed to the crate. But the eventfd ends when every
  descriptor the crate knows about is closed, so a raw copy that was never
  passed to the crate stops working once the original is closed.
- **Windows descriptors are synthetic.** `gk_dup` returns another synthetic
  number (`>= 65536`); `gk_dup2` onto a CRT descriptor number fails with
  `EBADF`. The CRT's own `_dup` cannot copy them (use `gk_dup`).

## MSRV

The minimum supported Rust version is **1.85** (edition 2024). Raising the MSRV counts as a minor version bump.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

Copyright © 2026 Adaptif.

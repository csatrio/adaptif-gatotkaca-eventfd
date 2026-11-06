# AGENTS.md: using `adaptif-gatotkaca-eventfd` on every platform

Instructions for coding agents (and humans) writing code that uses this crate.
The crate name is `adaptif-gatotkaca-eventfd`; the Rust import is
`adaptif_gatotkaca_eventfd`; the main type is `GkEventFd`.

## 1. What you get on each platform

`GkEventFd` has Linux `eventfd(2)` semantics everywhere. What differs is the
object behind it, and that decides which OS calls are safe to use on it.

| Platform | Backend | Pollable object | Raw `read`/`write` on it |
|---|---|---|---|
| Linux, Android, FreeBSD 13+, NetBSD 10+, illumos | **native** kernel eventfd | the eventfd fd (`AsFd`, `AsRawFd`) | OK, it is a real eventfd |
| macOS, iOS, OpenBSD, DragonFly | **shim**: counter + pipe | pipe read end (`AsFd`, `AsRawFd`) | **Not OK**: use the crate API |
| Windows | **shim**: counter + loopback TCP socket pair | read socket (`AsSocket`, `AsRawSocket`) | **Not OK**: use the crate API |

On a shim the pollable object is readable exactly while the counter is
non-zero. Never write code that reads or writes it directly.

Check at compile time which one you have with `cfg` on `target_os`. The crate
itself uses an internal `cfg(gk_native)` from its `build.rs`; it is not
visible to your crate.

## 2. Golden rules (all platforms)

1. **Read and write only through the crate**: `GkEventFd::read`/`write`,
   `io::Read`/`io::Write` on `GkEventFd`, the `compat` functions, or the C
   `gk_*` functions. This is portable; raw syscalls are not.
2. **Use the `EFD_*` constants or `EfdFlags`, never numbers.** Flag values
   differ per OS (FreeBSD's `EFD_NONBLOCK` is `0x4`, Linux's is `0o4000`).
3. **Share with `Arc<GkEventFd>` or `try_clone()`.** Don't `dup()` the raw fd
   on a shim: the dup is pollable but isn't known to the crate.
4. **`fork()` does not share a shim counter** with the child. Use the native
   platforms, or another IPC mechanism, if you need a cross-process eventfd.
5. **Close by dropping** the `GkEventFd` (or `eventfd_close`/`gk_eventfd_close`
   in the C-style APIs). Never `close(2)` a shim fd yourself.
6. Write-readiness (`POLLOUT`) is not reported on shims. Register for
   **readable** only.

## 3. Semantics (identical everywhere)

| Operation | Result |
|---|---|
| `write(n)` | adds `n` to the counter |
| `write(u64::MAX)` | `ErrorKind::InvalidInput` (`EINVAL`) |
| `write` past `EFD_MAX` (`u64::MAX - 1`) | blocks, or `WouldBlock` (`EAGAIN`) if non-blocking |
| `write(0)` | no-op, does not make it readable |
| `read()` normal mode | returns the counter, resets it to 0 |
| `read()` with `EFD_SEMAPHORE` | returns 1, decrements by 1 |
| `read()` when counter is 0 | blocks, or `WouldBlock` (`EAGAIN`) if non-blocking |
| `io::Read`/`io::Write` buffer < 8 bytes | `InvalidInput` (`EINVAL`) |
| `set_nonblocking` | shared by all clones, like `O_NONBLOCK` |

## 4. Rust recipes

```toml
[dependencies]
adaptif-gatotkaca-eventfd = "0.1"                       # sync API
# adaptif-gatotkaca-eventfd = { version = "0.1", features = ["mio"] }
```

### Blocking wakeup between threads (all platforms)

```rust
use adaptif_gatotkaca_eventfd::{EfdFlags, GkEventFd};
use std::sync::Arc;

let efd = Arc::new(GkEventFd::new(0, EfdFlags::NONE)?);
let tx = Arc::clone(&efd);
std::thread::spawn(move || tx.write(1).unwrap());
let n = efd.read()?; // blocks until the write
```

### Non-blocking, drain-until-WouldBlock (all platforms)

```rust
let efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
loop {
    match efd.read() {
        Ok(n) => handle(n),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
        Err(e) => return Err(e),
    }
}
```

### Semaphore (work-queue tokens)

```rust
let sem = GkEventFd::new(0, EfdFlags::SEMAPHORE)?;
sem.write(3)?;          // three tokens
assert_eq!(sem.read()?, 1); // each read takes one
```

### mio event loop (feature `mio`, all platforms)

```rust
let mut efd = GkEventFd::new(0, EfdFlags::NONBLOCK)?;
poll.registry().register(&mut efd, Token(0), Interest::READABLE)?;
// on event: read until WouldBlock (mio is edge-triggered)
```

- Always create mio-registered eventfds with `EfdFlags::NONBLOCK`.
- **Windows:** read through the *same* `GkEventFd` handle you registered.
  The Windows shim re-arms mio's readiness only for the handle that did the
  read. Give other threads a `try_clone()` for **writing**.
- Clones get their own descriptor, so registering several clones in one
  `Poll` with different tokens is fine.

### tokio

tokio is not a dependency of this crate; use it from your own crate.

- **Unix (native and shim):** wrap in `tokio::io::unix::AsyncFd` with
  `Interest::READABLE`, await `readable()`, then
  `guard.try_io(|inner| inner.get_ref().read())`. See `README.md` and
  `examples/tokio_async.rs`.
- **Windows:** tokio cannot register foreign sockets. Do the blocking `read()`
  inside `tokio::task::spawn_blocking`, or drive it with mio yourself.

### Platform-specific handles

```rust
#[cfg(unix)]
let fd = std::os::fd::AsRawFd::as_raw_fd(&efd);         // poll/kqueue/epoll: readable only
#[cfg(windows)]
let s = std::os::windows::io::AsRawSocket::as_raw_socket(&efd); // WSAPoll/select: readable only
```

`From<OwnedFd>`, `Into<OwnedFd>`, `IntoRawFd` and `FromRawFd` exist **only on
native platforms**. Don't use them in portable code.

## 5. C and C-style APIs

### From Rust: `adaptif_gatotkaca_eventfd::compat`

`<sys/eventfd.h>`-style functions that return `-1` and set `errno`:
`eventfd(initval, flags)`, `eventfd_read(fd, *mut eventfd_t)`,
`eventfd_write(fd, value)`, `eventfd_close(fd)`. On native platforms they pass
straight through to libc. On shims the returned `int` is a crate-managed
descriptor (on Windows a synthetic number, not a SOCKET). Use them only when
porting C-shaped code; prefer `GkEventFd` otherwise.

### From C: feature `ffi` + `include/adaptif_gatotkaca_eventfd.h`

Build the static library:

```sh
cargo rustc --release --features ffi --crate-type staticlib
cargo rustc --release --features ffi --crate-type staticlib -- --print native-static-libs  # link flags
```

Then `#include "adaptif_gatotkaca_eventfd.h"` (compile with `-Iinclude`) and
link `target/release/libadaptif_gatotkaca_eventfd.a`
(`adaptif_gatotkaca_eventfd.lib` on MSVC) plus the libraries printed above.

- The header gives you the `<sys/eventfd.h>` interface: `eventfd()`,
  `eventfd_read()`, `eventfd_write()`, `eventfd_t`, `EFD_*`. On native
  platforms it includes the system `<sys/eventfd.h>`; elsewhere it maps those
  names to the exported `gk_*` symbols. Define `GK_EVENTFD_NO_COMPAT_NAMES` to
  get only the `gk_` names.
- Exported symbols are always `gk_`-prefixed (`gk_eventfd`,
  `gk_eventfd_read`, `gk_eventfd_write`, `gk_eventfd_close`, ...), so they
  never clash with libc.
- **Raw `read`/`write`/`close` in ported Linux C code:** define
  `GK_EVENTFD_WRAP_IO` before including the header. On shim platforms
  `read`, `write` and `close` (and `_read`/`_write`/`_close` on Windows) are
  redirected to `gk_read`/`gk_write`/`gk_close`. Those apply eventfd rules to
  crate eventfds and forward every other descriptor to the real function. On
  native platforms the macro does nothing. It only affects code compiled with
  the header, not prebuilt libraries.
- **Polling from C:** on unix `poll()` the returned fd for `POLLIN`. On
  Windows get the socket with `gk_eventfd_socket(fd)` and use `WSAPoll`.
- Close with `eventfd_close`/`gk_eventfd_close` (or `close` under
  `GK_EVENTFD_WRAP_IO`).

`examples/c/smoke.c` is a working example of all of the above.

## 6. Per-platform checklists

**Linux / Android / FreeBSD 13+ / NetBSD 10+ / illumos (native)**
- Everything works, including raw syscalls, `epoll`/`kqueue`, `fork()`
  sharing, and passing the fd to other processes.
- FreeBSD 12 and NetBSD 9 lack `eventfd`; the crate does not support them.

**macOS / iOS / OpenBSD / DragonFly (pipe shim)**
- Poll the fd with `kqueue` (`EVFILT_READ`), `poll`, `select` or mio.
- Each `GkEventFd` uses 3 fds, plus 1 per clone. Size `RLIMIT_NOFILE` accordingly.
- Without `EFD_CLOEXEC` the fds are inherited across `exec`, but the counter is not.

**Windows (socket-pair shim)**
- Uses a loopback TCP connection per eventfd. Each eventfd costs 2 sockets
  plus 1 per clone, and closed pairs sit in `TIME_WAIT`. Don't create and
  destroy thousands per second; reuse them.
- Sockets are always non-inheritable, with or without `EFD_CLOEXEC`.
- Use `WSAPoll`/`select`/mio for readiness. tokio needs `spawn_blocking`.
- Third-party firewalls and antivirus that filter loopback traffic can delay
  or break readiness.

## 7. Verifying changes to this crate (no CI; run manually)

```sh
cargo fmt --check
cargo clippy --all-targets --features mio -- -D warnings
cargo test --features mio                      # macOS: exercises the pipe shim
cargo test --features ffi --lib

# Type-check other targets (rustup target add <triple> first)
for t in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc x86_64-unknown-freebsd \
         x86_64-unknown-netbsd x86_64-unknown-illumos; do
  cargo clippy --target $t --all-targets --features mio,ffi -- -D warnings
done

# Real Linux run, native and shim vs kernel (Docker, e.g. via `colima start`)
docker run --rm -v "$(pwd)":/w -w /w -v cargo-cache:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/w/target/docker rust:latest \
  bash -c 'cargo test --features mio && cargo test --features force-shim,mio'
```

- `force-shim` makes Linux use the pipe shim. `tests/differential.rs` then
  compares it against the kernel eventfd with random operation sequences.
  Run this after any change to `src/counter.rs` or `src/sys/unix_shim.rs`.
- Windows tests only run on a Windows machine:
  `cargo test --features mio,ffi`.
- Commit messages: no `Co-Authored-By` lines.

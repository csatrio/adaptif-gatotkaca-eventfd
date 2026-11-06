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
3. **Share with `Arc<GkEventFd>` or `try_clone()`.** In C-style code duplicate
   with `eventfd_dup`/`eventfd_dup2`/`eventfd_dup3` (`gk_dup*`). See section 5
   for raw `dup()` on shims.
4. **Share with other processes the Linux way** (section 6): `fork()` shares
   it automatically on unix; spawned children get it via
   `export_inheritable()` + `GkEventFd::import(token)`; unrelated processes
   via `send_to`/`recv_from` over a `UnixStream` (unix) or `export_to(pid)` +
   `import` (Windows). On shims, plain fd inheritance without a token does
   not carry the counter.
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
`eventfd_write(fd, value)`, `eventfd_close(fd)`, plus `eventfd_dup(fd)`,
`eventfd_dup2(old, new)` and (unix) `eventfd_dup3(old, new, flags)`, which
behave like `dup(2)`/`dup2(2)`/`dup3(2)` (new descriptor, same counter;
`dup`/`dup2` clear close-on-exec). On native platforms they pass
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
- **Raw `read`/`write`/`close`/`dup` in ported Linux C code:** define
  `GK_EVENTFD_WRAP_IO` before including the header. On shim platforms
  `read`, `write`, `close`, `dup`, `dup2` and `dup3` (on Windows `_read`,
  `_write`, `_close`, `_dup`, `_dup2` and their POSIX aliases; no `dup3`) are
  redirected to `gk_read`/`gk_write`/`gk_close`/`gk_dup`/`gk_dup2`/`gk_dup3`.
  Those apply eventfd rules to crate eventfds and forward every other
  descriptor to the real function. On native platforms the macro does
  nothing. It only affects code compiled with the header, not prebuilt
  libraries.
- **Duplicating from C:** `gk_dup`, `gk_dup2`, `gk_dup3` (unix) give a new
  descriptor on the same counter; close each copy separately.
  - macOS/BSD shim: a raw `dup()`/`fcntl(F_DUPFD)` copy is recognised by its
    pipe the first time it reaches a `gk_*`/`compat` call. The eventfd ends
    when every descriptor the crate knows is closed, so pass raw copies to
    the crate (or use `gk_dup`) before closing the original.
  - Windows: descriptors are synthetic numbers `>= 65536`. `gk_dup2` only
    accepts targets in that range and returns `0` on success (like `_dup2`);
    the CRT's own `_dup` cannot copy them.
- **Polling from C:** on unix `poll()` the returned fd for `POLLIN`. On
  Windows get the socket with `gk_eventfd_socket(fd)` and use `WSAPoll`.
- Close with `eventfd_close`/`gk_eventfd_close` (or `close` under
  `GK_EVENTFD_WRAP_IO`).

`examples/c/smoke.c` is a working example of all of the above.

## 6. Sharing with other processes (all OS, modelled on Linux)

| Linux way | Call | Notes |
|---|---|---|
| `fork()` | nothing | unix only; Windows has no `fork` |
| child inherits the fd across `exec` / spawn | parent: `let export = efd.export_inheritable()?;` pass `export.token()` (env var), spawn, `drop(export)`. Child: `unsafe { GkEventFd::import(&token) }` | every child spawned while `export` lives inherits the copies |
| fd passed to an unrelated process (`SCM_RIGHTS`) | unix: `efd.send_to(&stream)` / `GkEventFd::recv_from(&stream)` on a connected `UnixStream` | don't interleave other writes on that stream with it |
| (Windows equivalent of the above) | `let token = efd.export_to(pid)?;` send the text, `unsafe { GkEventFd::import(&token) }` there | needs `PROCESS_DUP_HANDLE` on the target |

- **Uniform API (recommended):** `GkProcess` does the above with the same
  code on every OS. Parent: `let (child, mut peer) = GkProcess::spawn(&mut cmd)?;
  peer.send(&efd)?;` Child: `let efd = GkProcess::from_parent()?.recv()?;`
  Unrelated processes: `GkListener::bind(name)?.accept()?` /
  `GkProcess::connect(name)?`, then `send`/`recv`. Unix: unix socket
  (`$TMPDIR/gkefd-<name>.sock`); Windows: named pipe `\\.\pipe\gkefd-<name>`
  with the peer's pid recorded. `recv` is safe (no `unsafe import`). On
  Windows keep the sending `GkProcess` alive until the peer has received.
- After sharing, both processes use one counter: writes, blocking reads,
  readiness and `set_nonblocking` work across them. Data written before
  `efd.write()` is visible to the process whose `efd.read()` sees that write,
  so the eventfd can be a doorbell for shared memory.
- On native platforms the token is `gkefd1:n:<fd>`: an inheritable copy of
  the kernel eventfd, usable directly by a child that doesn't use this crate.
  On shims the token names several descriptors and only `import` can use it.
- `import` is `unsafe`: it takes ownership of what the token names. One import
  per token, in the process it was made for. Bad tokens fail
  (`InvalidInput`/`InvalidData`) without closing anything. Imported and
  received descriptors are close-on-exec again.
- `recv_from` rejects (and closes) a message from a different backend kind,
  e.g. a native Linux sender with a `force-shim` receiver.
- C: `gk_eventfd_export_inheritable` (then `gk_eventfd_export_release` after
  spawning), `gk_eventfd_import`, `gk_eventfd_send`/`gk_eventfd_recv` (unix),
  `gk_eventfd_export_to` (Windows); token buffers of `GK_EVENTFD_TOKEN_MAX`.
- C process API (opaque `gk_process *` / `gk_listener *`, NULL or -1 +
  errno on failure): `gk_listener_bind`/`accept`/`free`,
  `gk_process_connect`, `gk_process_from_parent`, `gk_process_send(p, fd)`,
  `gk_process_recv(p)` -> fd, `gk_process_free`. To spawn:
  `gk_process_for_child(buf, len)` gives the value for the child's
  `GK_EVENTFD_PARENT` (`GK_EVENTFD_PARENT_ENV`); spawn it yourself, then
  optionally `gk_process_expect_pid(p, pid)` and `gk_process_send`. See
  `examples/c/process.c`.
- mio on Windows: each process should read (drain) its own registered handle;
  use one eventfd per waiting side.

## 6a. Channels (`GkChannel`)

- `let (mut tx, mut rx) = GkChannel::new()?;` or
  `GkChannel::with_options(ChannelOptions { capacity, max_message, bulk_capacity, chunk })`.
  Defaults: message ring 1 MiB, bulk ring 4 MiB, 64 KiB messages and chunks
  (Windows: 256 KiB / 1 MiB rings). Rings: powers of two >= 64;
  `max_message <= capacity / 2 - 8`; `chunk <= bulk_capacity / 2 - 32`.
- Two independent lanes, NOT ordered relative to each other:
  - messages: `tx.send(&[u8])`, `tx.reserve(len)?` + `slot.commit(n)`;
    `rx.recv(&mut buf)` (`InvalidInput` if `buf` too small, message stays;
    `rx.next_len()` peeks), `rx.recv_vec`, `rx.recv_with(|msg| ..)`.
  - bulk (any size): `tx.send_bulk(&data)? -> id` (blocks until queued),
    `tx.bulk_writer(Some(len) | None)?` (`io::Write`, `finish()`),
    `rx.recv_bulk_vec(&mut vec, limit)? -> id`, `rx.bulk_reader()?`
    (`io::Read`, `id()`, `len()`, `for_each_chunk`). Link lanes by sending
    the transfer id as a message.
  - kinds: every message and transfer has a `u16` kind chosen by the
    application (`0` = `KIND_UNSPECIFIED`, what the plain calls send).
    `tx.send_kind(k, msg)`, `tx.reserve_kind(k, len)`,
    `rx.recv_kind(&mut buf) -> (k, len)`, `rx.recv_vec_kind`,
    `rx.recv_with_kind(|k, msg| ..)`, `rx.next_kind() -> Option<(k, len)>`;
    bulk: `tx.send_bulk_kind(k, &data)`, `tx.bulk_writer_kind(k, len)`,
    `rx.recv_bulk_vec_kind(..) -> (k, id)`, `BulkReader::kind()`.
- One sender, one receiver; ends are not `Clone`.
- Errors: `WouldBlock` (non-blocking; bulk writes may be partial),
  `BrokenPipe` (other end gone, or its process died; after draining),
  `ConnectionAborted` (the sender abandoned a transfer: unfinished
  `BulkWriter` dropped), `InvalidData` (length mismatch or corruption),
  `InvalidInput` (sizes, limit), `PermissionDenied` (end owned by another
  process).
- Event loops: register the end (mio `Source`, `AsFd`/`AsSocket`, tokio
  `AsyncFd` on unix) for READABLE; drain every lane you use until
  `WouldBlock`, then wait. To serve messages mid-transfer, `pause()` the
  `BulkReader` (resume with `rx.bulk_reader()`) or `BulkWriter` (resume with
  `tx.resume_bulk()`); dropping them instead discards / abandons the
  transfer.
- Across processes: `GkProcess::send(&end)` + `recv_as::<GkReceiver>()` /
  `recv_as::<GkSender>()` (all OS); unix also `end.send_to(&stream)` /
  `GkReceiver::recv_from(&stream)`. The sending process's copy is detached.
  After `fork()`: the keeping process calls `claim()`, the other
  `detach()`es its copy. Not shared via `export_inheritable`/`import`.
- C: `gk_channel_options_default`, `gk_channel_new(&opts | NULL, &tx, &rx)`,
  `gk_sender_send`, `gk_receiver_recv`, `gk_receiver_next_len`,
  `gk_sender_send_bulk`, `gk_sender_bulk_begin`/`_write`/`_finish`/`_abort`,
  `gk_receiver_bulk_begin`/`_read`/`_end`, kind variants
  `gk_sender_send_kind`, `gk_receiver_recv_kind(rx, &kind, ..)`,
  `gk_receiver_next_kind`, `gk_sender_send_bulk_kind`,
  `gk_sender_bulk_begin_kind`, `gk_receiver_bulk_begin_kind`
  (`GK_KIND_UNSPECIFIED` = 0), `gk_*_set_nonblocking`,
  `gk_*_claim`, `gk_*_detach`, `gk_*_free`, `gk_sender_fd`/`gk_receiver_fd`
  (unix), `gk_*_socket` (Windows), `gk_process_send_sender`/`_receiver`,
  `gk_process_recv_sender`/`_receiver`; errno `EAGAIN`, `EPIPE`, `EMSGSIZE`,
  `EACCES`, `ECONNABORTED`. See `examples/c/channel.c`.

## 6b. Shared memory (`GkSharedMemory`)

- `let shm = GkSharedMemory::new(len)?;` (zero-filled, `len` rounded up to
  the page size; `len == 0` is `InvalidInput`) or
  `GkSharedMemory::with_options(len, SharedMemoryOptions { huge_pages, huge_page_size, populate })`.
- Access: `as_ptr()` (page-aligned), `len()`, `page_size()`, `is_huge()`,
  `unsafe as_slice()` / `unsafe as_mut_slice()` (`&self`: other threads and
  processes may write at any time; you guarantee no overlapping writer).
  Synchronise with atomics in the memory or an eventfd doorbell (writes
  before `efd.write()` are visible after the matching `efd.read()`).
- Share: `GkProcess::send(&shm)` + `recv_as::<GkSharedMemory>()` (all OS);
  unix also `shm.send_to(&stream)` / `GkSharedMemory::recv_from(&stream)`;
  `fork()` shares it with no code. Nothing is detached: both processes keep
  using it; the memory is freed when the last one drops it. Wrong kind of
  object received: `InvalidData`. Windows: keep the sending `GkProcess`
  alive until the peer has received.
- Huge pages (`HugePages::Off` default, `Prefer`, `Require`): Linux hugetlb
  needs `vm.nr_hugepages` reserved (`Prefer` otherwise asks for transparent
  huge pages); FreeBSD large pages; Windows needs the "Lock pages in memory"
  right; macOS/iOS and other unix have none. `Prefer` never fails because of
  huge pages; `Require` fails with `Unsupported`, `PermissionDenied` or
  `OutOfMemory`. Check `is_huge()`.
- `AsFd` (unix) / `AsHandle` (Windows) expose the object; don't resize it.
- C: `gk_shm_options_default`, `gk_shm_new`, `gk_shm_new_ex(len, &opts | NULL)`
  (`GK_HUGE_PAGES_OFF/PREFER/REQUIRE`, `huge_page_size` 0 = default),
  `gk_shm_ptr`, `gk_shm_len`, `gk_shm_page_size`, `gk_shm_is_huge`,
  `gk_shm_fd` (unix) / `gk_shm_handle` (Windows), `gk_shm_free`,
  `gk_process_send_shm`, `gk_process_recv_shm`; errno `EINVAL`, `ENOTSUP`,
  `ENOMEM`, `EPERM`/`EACCES`. See `examples/c/shared_memory.c`.

## 7. Per-platform checklists

**Linux / Android / FreeBSD 13+ / NetBSD 10+ / illumos (native)**
- Everything works, including raw syscalls, `epoll`/`kqueue`, `fork()`
  sharing, and passing the fd to other processes (section 6's calls are thin
  wrappers over fd inheritance and `SCM_RIGHTS`).
- FreeBSD 12 and NetBSD 9 lack `eventfd`; the crate does not support them.

**macOS / iOS / OpenBSD / DragonFly (pipe shim)**
- Poll the fd with `kqueue` (`EVFILT_READ`), `poll`, `select` or mio.
- Each `GkEventFd` uses 4 fds (shared memory object, pipe read and write
  ends, its own pollable dup), plus 1 per clone. Size `RLIMIT_NOFILE`
  accordingly.
- `fork()` shares the counter: it lives in an unnamed shared memory object,
  guarded by a cross-process spinlock. Parent and child see each other's
  writes, blocking reads wake across processes, and `set_nonblocking` is
  shared (as `O_NONBLOCK` is on Linux).
- If a process dies while holding that lock (only possible mid-operation, e.g.
  `SIGKILL`), the next caller takes it over and repairs readiness.
- After `exec` or for unrelated processes, share explicitly (section 6):
  plain inherited fds without a token don't carry the counter.
- Not async-signal-safe: don't call into an eventfd from a signal handler
  that may have interrupted an operation on the same eventfd.
- After `fork()` in a multithreaded program, the usual rule applies: the child
  should only use the eventfd itself (its operations don't allocate), not
  the compat registry, until it calls `exec`.

**Windows (socket-pair shim)**
- Uses a loopback TCP connection per eventfd. Each eventfd costs 2 sockets
  plus 1 per clone, and closed pairs sit in `TIME_WAIT`. Don't create and
  destroy thousands per second; reuse them.
- The eventfd's own handles are always non-inheritable, with or without
  `EFD_CLOEXEC`. Share with other processes explicitly (section 6); the same
  cross-process lock and dead-holder takeover as on the unix shim apply.
- Use `WSAPoll`/`select`/mio for readiness. tokio needs `spawn_blocking`.
- Third-party firewalls and antivirus that filter loopback traffic can delay
  or break readiness.

## 8. Verifying changes to this crate (no CI; run manually)

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
  Run this after any change to `src/sys/shm.rs` (shared by the unix and
  Windows shims) or `src/sys/unix_shim.rs`.
- Channel lane logic under Miri (nightly):
  `cargo +nightly miri test --lib channel::lane`.
- Channel benchmark: `cargo run --release --example channel_bench`.
- Windows tests only run on a Windows machine:
  `cargo test --features mio,ffi`. `tests/windows_share.rs` and the
  sharing tests in `src/sys/windows.rs` re-run the test binary as a child
  process.
- Commit messages: no `Co-Authored-By` lines.

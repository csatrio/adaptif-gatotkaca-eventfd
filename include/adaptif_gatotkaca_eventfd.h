/*
 * adaptif_gatotkaca_eventfd.h - C interface of the adaptif-gatotkaca-eventfd
 * Rust crate (build it with `--features ffi`).
 *
 * A cross-platform eventfd(2) with Linux semantics: a 64-bit counter whose
 * descriptor is readable exactly while the counter is non-zero.
 *
 *   - Native platforms (GK_EVENTFD_NATIVE == 1: Linux, Android, FreeBSD 13+,
 *     NetBSD 10+, illumos): this header includes <sys/eventfd.h> (so EFD_*
 *     have the OS's own values); the gk_* functions pass straight through
 *     to libc and the descriptor is the kernel eventfd.
 *   - macOS, iOS, OpenBSD, DragonFly: the descriptor is the read end of an
 *     internal pipe,
 *     pollable with poll/select/kqueue. Raw read/write/close on it do NOT have
 *     eventfd semantics: use the gk_* functions, or GK_EVENTFD_WRAP_IO below.
 *     Duplicate it with gk_dup/gk_dup2/gk_dup3; a raw dup()/fcntl(F_DUPFD)
 *     copy is also recognised the first time it is passed to a gk_* function.
 *     The eventfd lives until every descriptor known to the crate is closed.
 *   - Windows: the descriptor is a synthetic int >= 65536 (never a CRT fd).
 *     Use gk_eventfd_socket() to get a SOCKET for WSAPoll/select. gk_dup
 *     returns another synthetic number; gk_dup2 only accepts targets >= 65536.
 *
 * Every function returns -1 and sets errno on failure.
 *
 * Configuration macros (define them before including this header):
 *
 *   GK_EVENTFD_NO_COMPAT_NAMES
 *       On non-native platforms this header provides the <sys/eventfd.h>
 *       names (eventfd, eventfd_read, eventfd_write, eventfd_t, EFD_*) as
 *       static inline wrappers around gk_*, so code written for Linux
 *       compiles unchanged. Define this to get only the gk_* / GK_EFD_*
 *       names, e.g. to avoid clashing with a system <sys/eventfd.h>.
 *
 *   GK_EVENTFD_WRAP_IO
 *       On non-native platforms, redirect raw I/O so Linux code that does
 *       read(efd, &v, 8) / write(efd, &v, 8) / close(efd) / dup(efd) works
 *       unchanged:
 *         unix:    read, write, close, dup, dup2, dup3
 *                    -> gk_read, gk_write, gk_close, gk_dup, gk_dup2, gk_dup3
 *         Windows: _read, _write, _close, _dup, _dup2 and the POSIX aliases
 *                  read, write, close, dup, dup2
 *                    -> gk_read, gk_write, gk_close, gk_dup, gk_dup2
 *       gk_* give eventfd semantics to descriptors from gk_eventfd and
 *       forward every other descriptor to the real function. No-op on native
 *       platforms. Limitations: these are function-like macros, so they only
 *       affect code compiled with this header (not prebuilt libraries, not
 *       Rust code), they do not affect uses that are not calls (taking
 *       &read or storing `read` in a function pointer bypasses the
 *       redirect), and they DO rewrite calls through members named like
 *       them (obj->read(a, b, c) becomes obj->gk_read(a, b, c)); #undef the
 *       macro in such files. This header includes <unistd.h> (unix) or
 *       <io.h> (Windows) first so the system prototypes are not rewritten.
 *
 *   GK_EVENTFD_NATIVE
 *       1 where the platform has a kernel eventfd and the crate passes
 *       through to it, else 0. Computed below; only override it to match a
 *       library built with the `force-shim` testing feature (-DGK_EVENTFD_NATIVE=0).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 * Copyright (c) 2026 PT Adaptif Sinergi Dinamis (https://adaptif.co.id/).
 */

#ifndef ADAPTIF_GATOTKACA_EVENTFD_H
#define ADAPTIF_GATOTKACA_EVENTFD_H

/* The one place that decides which platforms use the kernel eventfd
 * (must match `cfg(gk_native)` in the crate's build.rs). */
#ifndef GK_EVENTFD_NATIVE
#if defined(__linux__) || defined(__ANDROID__) || defined(__illumos__)
#define GK_EVENTFD_NATIVE 1
#elif defined(__FreeBSD__) || defined(__NetBSD__)
#include <sys/param.h> /* __FreeBSD_version, __NetBSD_Version__ */
#if (defined(__FreeBSD_version) && __FreeBSD_version >= 1300000) || \
    (defined(__NetBSD_Version__) && __NetBSD_Version__ >= 1000000000)
#define GK_EVENTFD_NATIVE 1
#else
#define GK_EVENTFD_NATIVE 0
#endif
#else
#define GK_EVENTFD_NATIVE 0
#endif
#endif

#include <stddef.h>
#include <stdint.h>

#ifdef _WIN32
#if defined(GK_EVENTFD_WRAP_IO) && !GK_EVENTFD_NATIVE
#include <io.h> /* before the redirect macros below */
#endif
#else
#include <sys/types.h> /* ssize_t */
#if defined(GK_EVENTFD_WRAP_IO) && !GK_EVENTFD_NATIVE
#include <unistd.h> /* before the redirect macros below */
#endif
#endif

#if GK_EVENTFD_NATIVE

#include <sys/eventfd.h>

#define GK_EFD_SEMAPHORE EFD_SEMAPHORE
#define GK_EFD_NONBLOCK EFD_NONBLOCK
#define GK_EFD_CLOEXEC EFD_CLOEXEC

#else

/* Flag values: the Linux x86/arm values, as in src/flags.rs. */
#define GK_EFD_SEMAPHORE 01
#define GK_EFD_NONBLOCK 04000
#define GK_EFD_CLOEXEC 02000000

/* The eventfd counter type. */
typedef uint64_t eventfd_t;

#ifndef GK_EVENTFD_NO_COMPAT_NAMES
#define EFD_SEMAPHORE GK_EFD_SEMAPHORE
#define EFD_NONBLOCK GK_EFD_NONBLOCK
#define EFD_CLOEXEC GK_EFD_CLOEXEC
#endif

#endif /* GK_EVENTFD_NATIVE */

/* Return type of gk_read/gk_write: matches read/write (CRT _read/_write). */
#ifdef _WIN32
typedef int gk_ssize_t;
#else
typedef ssize_t gk_ssize_t;
#endif

#if defined(_MSC_VER) && !defined(__cplusplus)
#define GK_EVENTFD_INLINE static __inline
#else
#define GK_EVENTFD_INLINE static inline
#endif

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Creates an eventfd with counter `initval`, like eventfd(2).
 * `flags` is a bitwise OR of GK_EFD_SEMAPHORE, GK_EFD_NONBLOCK and
 * GK_EFD_CLOEXEC (non-inheritable handles on Windows).
 * Returns a descriptor >= 0, or -1 with errno set (EINVAL for unknown flags,
 * EMFILE/ENFILE/ENOMEM on resource exhaustion).
 * Close it with gk_eventfd_close() (or gk_close()).
 */
int gk_eventfd(unsigned int initval, int flags);

/*
 * Reads the counter into *value, like eventfd_read(3).
 * Normal mode: stores the counter and resets it to 0.
 * GK_EFD_SEMAPHORE: stores 1 and decrements the counter.
 * If the counter is 0 it blocks, or fails with EAGAIN when non-blocking.
 * Returns 0, or -1 with errno (EAGAIN, EBADF, EFAULT, ...).
 */
int gk_eventfd_read(int fd, eventfd_t *value);

/*
 * Adds `value` to the counter, like eventfd_write(3).
 * Fails with EINVAL for UINT64_MAX. If the counter would exceed
 * UINT64_MAX - 1 it blocks, or fails with EAGAIN when non-blocking.
 * Returns 0, or -1 with errno.
 */
int gk_eventfd_write(int fd, eventfd_t value);

/*
 * Closes an eventfd created by gk_eventfd(). On non-native platforms never
 * use a raw close()/closesocket() on it (unless GK_EVENTFD_WRAP_IO redirects
 * close). Returns 0, or -1 with errno (EBADF).
 */
int gk_eventfd_close(int fd);

/*
 * read(2) with eventfd semantics: stores the counter value (as for
 * gk_eventfd_read) as a native-endian uint64_t in the first 8 bytes of buf.
 * Returns 8, or -1 with errno: EINVAL if count < 8, EAGAIN if non-blocking
 * and the counter is 0, EBADF, EFAULT.
 */
intptr_t gk_eventfd_read_buf(int fd, void *buf, size_t count);

/*
 * write(2) with eventfd semantics: adds the native-endian uint64_t in the
 * first 8 bytes of buf to the counter (as for gk_eventfd_write).
 * Returns 8, or -1 with errno: EINVAL if count < 8 or the value is
 * UINT64_MAX, EAGAIN if non-blocking and the counter would overflow, EBADF,
 * EFAULT.
 */
intptr_t gk_eventfd_write_buf(int fd, const void *buf, size_t count);

/*
 * read(2) for any descriptor: eventfds from gk_eventfd() behave like
 * gk_eventfd_read_buf(); every other descriptor is forwarded to the real
 * read() (CRT _read() on Windows, with count clamped to INT_MAX).
 */
gk_ssize_t gk_read(int fd, void *buf, size_t count);

/*
 * write(2) for any descriptor: eventfds from gk_eventfd() behave like
 * gk_eventfd_write_buf(); every other descriptor is forwarded to the real
 * write() (CRT _write() on Windows, with count clamped to INT_MAX).
 */
gk_ssize_t gk_write(int fd, const void *buf, size_t count);

/*
 * close(2) for any descriptor: eventfds from gk_eventfd() are closed as by
 * gk_eventfd_close(); every other descriptor is forwarded to the real
 * close() (CRT _close() on Windows).
 */
int gk_close(int fd);

/*
 * dup(2) for any descriptor: an eventfd gets a new descriptor on the same
 * counter (lowest free number on unix, close-on-exec cleared; a new
 * synthetic number on Windows); every other descriptor is forwarded to the
 * real dup() (CRT _dup() on Windows). Close every copy separately.
 */
int gk_dup(int fd);

/*
 * dup2(2) for any descriptor. If oldfd is an eventfd, newfd becomes another
 * descriptor of it (whatever newfd was is closed first, atomically on unix;
 * on Windows newfd must be >= 65536, else EBADF). Otherwise forwarded to the
 * real dup2() (CRT _dup2()). Returns newfd on unix and 0 on Windows (as
 * dup2/_dup2 do there), or -1 with errno.
 */
int gk_dup2(int oldfd, int newfd);

#ifndef _WIN32
/*
 * dup3(2) for any descriptor (unix only): like gk_dup2 but flags is 0 or
 * O_CLOEXEC and oldfd == newfd fails with EINVAL. Works on macOS too, which
 * has no dup3 (close-on-exec is then set right after the dup2).
 */
int gk_dup3(int oldfd, int newfd, int flags);
#endif

#ifdef _WIN32
/* Returned by gk_eventfd_socket() on error; equal to INVALID_SOCKET. */
#define GK_EVENTFD_INVALID_SOCKET (~(uintptr_t)0)

/*
 * Windows only: returns the SOCKET (cast it to SOCKET) that is readable
 * exactly while the counter is non-zero, for WSAPoll/select. The socket is
 * owned by the eventfd: never recv() from or close it, and do not use it
 * after gk_eventfd_close(). Returns GK_EVENTFD_INVALID_SOCKET with
 * errno = EBADF for an unknown descriptor.
 */
uintptr_t gk_eventfd_socket(int fd);

/*
 * Windows counterpart of gk_eventfd_send (Windows can't pass handles over a
 * socket): duplicates fd's handles into the running process `pid` (needs
 * PROCESS_DUP_HANDLE access) and writes a NUL-terminated token for it into
 * buf. Send the token to that process (stdin, a pipe, a socket, ...) and
 * call gk_eventfd_import there. Nothing becomes inheritable.
 * Returns the token length, or -1 with errno.
 */
int gk_eventfd_export_to(int fd, unsigned long pid, char *buf, size_t len);
#endif

/*
 * Sharing with other processes, following Linux:
 *   - fork():  the eventfd is shared automatically (unix).
 *   - spawn/exec a child: gk_eventfd_export_inheritable + gk_eventfd_import
 *     (on Linux and other native platforms the token is "gkefd1:n:<fd>",
 *     an inheritable copy of the kernel eventfd).
 *   - unrelated process: gk_eventfd_send/gk_eventfd_recv over a unix socket
 *     (SCM_RIGHTS), or gk_eventfd_export_to on Windows.
 * Afterwards both processes share the counter: writes, blocking reads and
 * readiness work across them.
 */

/* Buffer size that always fits an export token, including the NUL. */
#define GK_EVENTFD_TOKEN_MAX 4096

/*
 * For a child spawned next: makes inheritable copies of fd's descriptors
 * (handles on Windows) and writes a NUL-terminated token naming them into
 * buf (len bytes; GK_EVENTFD_TOKEN_MAX is enough). Pass the token to the
 * child (e.g. in an environment variable) and spawn it (fork+exec,
 * posix_spawn, CreateProcess with bInheritHandles=TRUE). Then call
 * gk_eventfd_export_release(token) to close this process's copies. Any
 * other child spawned in between inherits them too.
 * Returns the token length, or -1 with errno (EBADF, ERANGE, EFAULT).
 */
int gk_eventfd_export_inheritable(int fd, char *buf, size_t len);

/*
 * Closes this process's copies made by gk_eventfd_export_inheritable.
 * Returns 0, or -1 with errno = EINVAL for an unknown token.
 */
int gk_eventfd_export_release(const char *token);

/*
 * Opens an eventfd from a token made for this process by one of the export
 * functions. Returns a new descriptor (close it with gk_eventfd_close), or
 * -1 with errno = EINVAL for a bad token (every descriptor it names is left
 * open). Import each token only once.
 */
int gk_eventfd_import(const char *token);

#ifndef _WIN32
/*
 * Sends eventfd fd over the connected unix socket `sock` (SCM_RIGHTS), the
 * way Linux passes an eventfd to an unrelated process. The receiver calls
 * gk_eventfd_recv. Returns 0, or -1 with errno.
 */
int gk_eventfd_send(int fd, int sock);

/*
 * Receives an eventfd sent with gk_eventfd_send on `sock`. Returns a new
 * descriptor sharing the sender's counter, or -1 with errno (EINVAL if the
 * message is not an eventfd).
 */
int gk_eventfd_recv(int sock);
#endif

/*
 * Process API: one way to hand eventfds to other processes on every OS
 * (the C form of the Rust GkProcess / GkListener). Underneath it is a unix
 * socket ($TMPDIR/gkefd-<name>.sock, descriptors passed with SCM_RIGHTS)
 * or, on Windows, a named pipe (\\.\pipe\gkefd-<name>) that records the
 * peer's process id so the receiver pulls the handles into itself.
 *
 * Handles are opaque. Functions returning a handle return NULL with errno
 * set on failure; the others return -1 with errno. A gk_process is not
 * thread-safe; a gk_listener may be accepted from several threads.
 */
typedef struct gk_process gk_process;
typedef struct gk_listener gk_listener;

/* Environment variable gk_process_for_child's value goes in. */
#define GK_EVENTFD_PARENT_ENV "GK_EVENTFD_PARENT"

/*
 * Listens under `name` (a socket path if it contains '/', else
 * $TMPDIR/gkefd-<name>.sock; on Windows \\.\pipe\gkefd-<name> unless it
 * starts with "\\"). Fails with EADDRINUSE if someone already listens.
 */
gk_listener *gk_listener_bind(const char *name);

/* Waits for the next process to gk_process_connect. */
gk_process *gk_listener_accept(gk_listener *listener);

/* Closes the listener (removes the socket file on unix). NULL is ignored. */
void gk_listener_free(gk_listener *listener);

/* Connects to the process listening under `name`. */
gk_process *gk_process_connect(const char *name);

/*
 * In a child spawned via gk_process_for_child: connects to the parent.
 * Fails with ENOENT if GK_EVENTFD_PARENT is not set.
 */
gk_process *gk_process_from_parent(void);

/*
 * Parent side for a child you spawn yourself (fork+exec, posix_spawn,
 * CreateProcess, ...): returns a connection waiting for that child and
 * writes the value for the child's GK_EVENTFD_PARENT into env_value (len
 * bytes; GK_EVENTFD_TOKEN_MAX is enough, ERANGE otherwise). Put
 * GK_EVENTFD_PARENT=<env_value> in the child's environment, spawn it, and
 * optionally call gk_process_expect_pid. The connection completes on the
 * first gk_process_send/gk_process_recv, which waits for the child.
 */
gk_process *gk_process_for_child(char *env_value, size_t len);

/*
 * Windows: only accept the child with this process id (no-op on unix,
 * where the private socket path protects the connection). Returns 0.
 */
int gk_process_expect_pid(gk_process *process, unsigned long pid);

/*
 * Hands eventfd `fd` to the other process, which calls gk_process_recv;
 * both then share the counter. Returns 0, or -1 with errno. On Windows the
 * peer pulls the handles out of this process: keep `process` until the peer
 * has received.
 */
int gk_process_send(gk_process *process, int fd);

/*
 * Receives an eventfd sent with gk_process_send. Returns a new descriptor
 * sharing the sender's counter (close it with gk_eventfd_close), or -1 with
 * errno (EINVAL if the message is not an eventfd).
 */
int gk_process_recv(gk_process *process);

/* Closes the connection. NULL is ignored. */
void gk_process_free(gk_process *process);

/*
 * Byte messages on the same connection (since 0.3.1), e.g. a handshake.
 * A message holds 0 to GK_PROCESS_BYTES_MAX bytes; byte messages and
 * objects (gk_process_send*, gk_process_recv*) may be mixed and arrive in
 * order, but the receiver must ask for the right kind each time: the wrong
 * kind fails with EINVAL (an object is closed, a byte message dropped) and
 * the connection stays usable. Don't send on one gk_process from several
 * threads at once.
 */
#define GK_PROCESS_BYTES_MAX 65536

/*
 * Sends `len` bytes from `buf` (NULL allowed when len is 0) as one byte
 * message. Blocks until written (the peer may have to read first).
 * Returns 0, or -1 with errno (EMSGSIZE if len > GK_PROCESS_BYTES_MAX:
 * nothing is sent).
 */
int gk_process_send_bytes(gk_process *process, const void *buf, size_t len);

/*
 * Receives a byte message into `buf` (`cap` bytes). Returns its length, or
 * -1 with errno: EMSGSIZE if it doesn't fit (the message is kept for the
 * next call; a GK_PROCESS_BYTES_MAX buffer always fits), EINVAL if an
 * object was sent instead, EIO once the peer closed the connection.
 */
intptr_t gk_process_recv_bytes(gk_process *process, void *buf, size_t cap);

/*
 * The id of the process at the other end, 0 if unknown (not connected yet,
 * or the OS can't tell: DragonFly), or -1 with errno = EINVAL for NULL.
 * Read when the connection is made, so it stays known after the peer exits.
 */
int64_t gk_process_peer_pid(const gk_process *process);

#ifndef _WIN32
/*
 * The connection's unix socket, to poll for readable: a message arrived,
 * or EOF once the peer closed the connection or exited (a lifeline).
 * Completes a gk_process_for_child connection first (waits for the child).
 * Owned by `process`: don't close it, and don't read or write it yourself.
 * Returns -1 with errno on failure.
 */
int gk_process_fd(gk_process *process);
#endif

/*
 * Channels: one-way data passing between threads or processes through
 * shared memory (the C form of the Rust GkChannel / GkSender / GkReceiver).
 * A channel has two independent lanes, not ordered relative to each other:
 *   - messages: atomic, up to max_message bytes (may be empty), in order;
 *   - bulk transfers: any size, sliced into chunks and reassembled, in
 *     order, each with an id (gk_sender_send_bulk / gk_sender_bulk_*,
 *     gk_receiver_bulk_*).
 * The eventfd doorbells make each end pollable: the receiver's descriptor is
 * readable when data may be waiting in a lane it drained to EAGAIN, the
 * sender's when space may have been freed (read/write until EAGAIN, then
 * poll).
 *
 * Every message and transfer carries a 16-bit kind chosen by the
 * application to say what the payload is (the *_kind functions); the plain
 * functions send GK_KIND_UNSPECIFIED (0) and ignore it on receipt.
 *
 * Errors: -1 (or NULL) with errno EAGAIN (non-blocking and empty/full),
 * EPIPE (the other end is gone; the receiver gets it after draining),
 * EMSGSIZE (message larger than max_message, or a receive buffer too small:
 * the message stays queued, see gk_receiver_next_len), EACCES (the end
 * belongs to another process; after fork call gk_*_claim in the process
 * that keeps it, and gk_*_detach in the other).
 */
typedef struct gk_sender gk_sender;
typedef struct gk_receiver gk_receiver;

/* The kind of messages and transfers sent without one. */
#define GK_KIND_UNSPECIFIED 0

/*
 * Channel sizes. Defaults (gk_channel_options_default): message ring 1 MiB,
 * bulk ring 4 MiB, 64 KiB messages and chunks; on Windows a 256 KiB message
 * ring and a 1 MiB bulk ring. Rings are powers of two >= 64; max_message is
 * at most capacity / 2 - 8; chunk at most bulk_capacity / 2 - 32.
 */
typedef struct gk_channel_options {
    size_t capacity;      /* message ring, bytes */
    size_t max_message;   /* largest message, bytes */
    size_t bulk_capacity; /* bulk ring, bytes */
    size_t chunk;         /* largest bulk chunk, bytes (transfers: any size) */
    int huge_pages;       /* GK_HUGE_PAGES_* (see gk_shm); default OFF */
} gk_channel_options;

/* Fills *opts with the defaults. */
void gk_channel_options_default(gk_channel_options *opts);

/*
 * Creates a channel (opts NULL: the defaults). Returns 0 and the two ends,
 * or -1 with errno = EINVAL for invalid sizes.
 */
int gk_channel_new(const gk_channel_options *opts, gk_sender **tx,
                   gk_receiver **rx);

/* Sends one message of len bytes (blocks while full unless non-blocking). */
int gk_sender_send(gk_sender *tx, const void *buf, size_t len);

/* gk_sender_send with a kind. */
int gk_sender_send_kind(gk_sender *tx, uint16_t kind, const void *buf,
                        size_t len);

/*
 * Receives one message into buf (cap bytes) and returns its length, which
 * may be 0. Blocks while empty unless non-blocking.
 */
intptr_t gk_receiver_recv(gk_receiver *rx, void *buf, size_t cap);

/* gk_receiver_recv that also stores the message's kind in *kind (if not
 * NULL). */
intptr_t gk_receiver_recv_kind(gk_receiver *rx, uint16_t *kind, void *buf,
                               size_t cap);

/* Length of the next message without waiting; -1 + EAGAIN if empty. */
intptr_t gk_receiver_next_len(gk_receiver *rx);

/* gk_receiver_next_len that also stores the next message's kind in *kind
 * (if not NULL), e.g. to pick a handler before receiving. */
intptr_t gk_receiver_next_kind(gk_receiver *rx, uint16_t *kind);

/*
 * Bulk transfers. Errors as above, plus ECONNABORTED (the sender abandoned
 * the transfer) and EINVAL (more or less data than a declared length, or no
 * transfer in progress).
 */

/* Sends len bytes (any size) as one transfer; blocks until all of it is in
 * the ring (even in non-blocking mode). *id (if not NULL) gets its id. */
int gk_sender_send_bulk(gk_sender *tx, const void *buf, size_t len,
                        uint64_t *id);
int gk_sender_send_bulk_kind(gk_sender *tx, uint16_t kind, const void *buf,
                             size_t len, uint64_t *id);

/* Starts a streamed transfer; len < 0 means unknown (ended by finish). */
int gk_sender_bulk_begin(gk_sender *tx, int64_t len, uint64_t *id);
int gk_sender_bulk_begin_kind(gk_sender *tx, uint16_t kind, int64_t len,
                              uint64_t *id);

/* Writes data of the transfer; returns the bytes taken (may be fewer than
 * len in non-blocking mode), or -1 with errno (EAGAIN when full). */
intptr_t gk_sender_bulk_write(gk_sender *tx, const void *buf, size_t len);

/* Ends the transfer (EINVAL if shorter than declared: then abandoned). */
int gk_sender_bulk_finish(gk_sender *tx);

/* Abandons the transfer: the receiver gets ECONNABORTED. */
int gk_sender_bulk_abort(gk_sender *tx);

/* Waits for the next transfer (or continues an unfinished one): *len gets
 * its declared length or -1, *id its id (either pointer may be NULL). */
int gk_receiver_bulk_begin(gk_receiver *rx, int64_t *len, uint64_t *id);
/* gk_receiver_bulk_begin that also stores the transfer's kind in *kind. */
int gk_receiver_bulk_begin_kind(gk_receiver *rx, uint16_t *kind,
                                int64_t *len, uint64_t *id);

/* Reads data of the transfer into buf; returns the bytes read, 0 at its
 * end, or -1 with errno (EAGAIN, ECONNABORTED, EPIPE, ...). */
intptr_t gk_receiver_bulk_read(gk_receiver *rx, void *buf, size_t cap);

/* Stops reading the transfer; the unread rest is discarded before the next
 * transfer. Returns 0. */
int gk_receiver_bulk_end(gk_receiver *rx);

/* Non-zero: fail with EAGAIN instead of blocking. Returns 0. */
int gk_sender_set_nonblocking(gk_sender *tx, int nonblocking);
int gk_receiver_set_nonblocking(gk_receiver *rx, int nonblocking);

/* After fork: make this process the owner of the end. Returns 0. */
int gk_sender_claim(gk_sender *tx);
int gk_receiver_claim(gk_receiver *rx);

#ifdef _WIN32
/* The SOCKET to WSAPoll/select on (readable as described above). */
uintptr_t gk_sender_socket(gk_sender *tx);
uintptr_t gk_receiver_socket(gk_receiver *rx);
#else
/* The descriptor to poll/kqueue on (readable as described above). */
int gk_sender_fd(gk_sender *tx);
int gk_receiver_fd(gk_receiver *rx);
#endif

/* Closes the end (the other side then gets EPIPE). NULL is ignored. */
void gk_sender_free(gk_sender *tx);
void gk_receiver_free(gk_receiver *rx);

/* Frees this process's copy without closing the channel (after fork). */
void gk_sender_detach(gk_sender *tx);
void gk_receiver_detach(gk_receiver *rx);

/*
 * Hands a channel end to the process at the other end of `process`, which
 * receives it with gk_process_recv_sender / gk_process_recv_receiver. This
 * process's copy is detached (free it with gk_*_free; it won't close the
 * channel). On Windows keep `process` until the peer has received.
 */
int gk_process_send_sender(gk_process *process, gk_sender *tx);
int gk_process_send_receiver(gk_process *process, gk_receiver *rx);
gk_sender *gk_process_recv_sender(gk_process *process);
gk_receiver *gk_process_recv_receiver(gk_process *process);

/*
 * Readiness: a receiver is readable (poll/kqueue/WSAPoll on its descriptor)
 * while a lane may hold data it has not seen; only a call that fails with
 * EAGAIN clears that. A fresh channel starts that way, so a receiver
 * registered before its first receive still wakes. Stopping before EAGAIN
 * (e.g. an event loop's budget) leaves it readable.
 *
 * 1 once the other end was closed or its owning process has exited, else 0
 * (for non-blocking users; blocking calls notice it by themselves).
 */
int gk_sender_peer_gone(gk_sender *tx);
int gk_receiver_peer_gone(gk_receiver *rx);

/*
 * Receives up to max messages without copying: calls f(ctx, kind, msg, len)
 * for each (msg is valid only during the call; f must not unwind). Waits
 * (or fails with EAGAIN) only for the first; frees the space of the whole
 * batch at once. Returns the number received, or -1 with errno.
 */
typedef void (*gk_recv_fn)(void *ctx, uint16_t kind, const void *msg,
                           size_t len);
intptr_t gk_receiver_recv_batch(gk_receiver *rx, size_t max, gk_recv_fn f,
                                void *ctx);

/*
 * Multi-producer channels: the sender can be cloned and used by many
 * threads of one process at once (thread-safe, const handle). Messages are
 * lock-free; each thread's messages arrive in its own order. Bulk sends are
 * serialised (one transfer at a time). The receiver is a plain gk_receiver.
 * The channel closes when the last clone is freed. Errors as for gk_sender.
 */
typedef struct gk_mp_sender gk_mp_sender;

int gk_channel_new_mpsc(const gk_channel_options *opts, gk_mp_sender **tx,
                        gk_receiver **rx);

/* Another handle on the same sender (free each one). */
gk_mp_sender *gk_mp_sender_clone(const gk_mp_sender *tx);

/* Blocks while the ring is full (unless non-blocking). */
int gk_mp_sender_send(const gk_mp_sender *tx, const void *buf, size_t len);
int gk_mp_sender_send_kind(const gk_mp_sender *tx, uint16_t kind,
                           const void *buf, size_t len);

/* Never blocks: EAGAIN when full; the descriptor then becomes readable when
 * space may have been freed. */
int gk_mp_sender_try_send_kind(const gk_mp_sender *tx, uint16_t kind,
                               const void *buf, size_t len);

/* One bulk transfer (blocks until it is all in the ring). */
int gk_mp_sender_send_bulk_kind(const gk_mp_sender *tx, uint16_t kind,
                                const void *buf, size_t len, uint64_t *id);

/* For every clone. Returns 0. */
int gk_mp_sender_set_nonblocking(const gk_mp_sender *tx, int nonblocking);
int gk_mp_sender_claim(const gk_mp_sender *tx);
int gk_mp_sender_peer_gone(const gk_mp_sender *tx);

#ifdef _WIN32
uintptr_t gk_mp_sender_socket(const gk_mp_sender *tx);
#else
int gk_mp_sender_fd(const gk_mp_sender *tx);
#endif

/* Frees this handle (the last one closes the channel). NULL is ignored. */
void gk_mp_sender_free(gk_mp_sender *tx);

/* Frees this handle; no handle of this process will close the channel
 * (after fork, in the process that doesn't send). */
void gk_mp_sender_detach(gk_mp_sender *tx);

/* Hand the sender to another process (EINVAL there if it expected a plain
 * gk_sender: the end is then closed). */
int gk_process_send_mp_sender(gk_process *process, const gk_mp_sender *tx);
gk_mp_sender *gk_process_recv_mp_sender(gk_process *process);

/*
 * Shared memory: zero-filled memory that can be handed to other processes
 * (the C form of the Rust GkSharedMemory). The length is rounded up to the
 * page size. Sharing gives the other process a view of the same memory;
 * both keep using it, and it is freed when the last view is freed. Nothing
 * in it is synchronised: use atomics, or an eventfd as a doorbell.
 *
 * Errors: NULL (or -1) with errno EINVAL (zero length, NULL handle, bad
 * option), ENOTSUP (huge pages required but not available on this OS or of
 * that size), ENOMEM (no huge pages / memory free), EPERM or EACCES
 * (Windows: large pages need the "Lock pages in memory" right).
 */
typedef struct gk_shm gk_shm;

/* gk_shm_options.huge_pages */
#define GK_HUGE_PAGES_OFF 0     /* normal pages (default) */
#define GK_HUGE_PAGES_PREFER 1  /* huge pages if available now, else normal */
#define GK_HUGE_PAGES_REQUIRE 2 /* huge pages or an error */

typedef struct gk_shm_options {
    int huge_pages;        /* GK_HUGE_PAGES_* */
    size_t huge_page_size; /* bytes; 0: the system default */
    int populate;          /* non-zero: touch every page at creation */
} gk_shm_options;

/* Fills *opts with the defaults (normal pages, no populate). */
void gk_shm_options_default(gk_shm_options *opts);

/* Creates shared memory of at least len bytes. */
gk_shm *gk_shm_new(size_t len);

/* gk_shm_new with options (opts NULL: the defaults). */
gk_shm *gk_shm_new_ex(size_t len, const gk_shm_options *opts);

/* Start of the memory (page-aligned), valid until gk_shm_free. */
void *gk_shm_ptr(const gk_shm *shm);

/* Length in bytes (a multiple of the page size). */
size_t gk_shm_len(const gk_shm *shm);

/* Size of the pages backing it (the huge page size if huge). */
size_t gk_shm_page_size(const gk_shm *shm);

/* 1 if it uses huge (large) pages, 0 if not. */
int gk_shm_is_huge(const gk_shm *shm);

#ifdef _WIN32
/* The section HANDLE, still owned by shm (advanced use). */
void *gk_shm_handle(const gk_shm *shm);
#else
/* The shared memory object's descriptor, still owned by shm (advanced use;
 * don't close or resize it). */
int gk_shm_fd(const gk_shm *shm);
#endif

/* Unmaps this process's view. NULL is ignored. */
void gk_shm_free(gk_shm *shm);

/*
 * Hands shared memory to the process at the other end of `process`, which
 * receives it with gk_process_recv_shm (EINVAL if something else was sent).
 * This process keeps its view. On Windows keep `process` until the peer has
 * received.
 */
int gk_process_send_shm(gk_process *process, const gk_shm *shm);
gk_shm *gk_process_recv_shm(gk_process *process);

/*
 * Shared memory pools: zero-copy buffers lent to another process. Only the
 * creating process allocates (from any thread); a view in another process
 * (or this one) takes leases, reads the data in place, and gives the block
 * back by freeing the gk_shm_buf (lock-free, any thread). The allocator
 * collects returns on its next allocation, and refuses stale or duplicate
 * ones (counted in bad_returns). Send the 16-byte gk_shm_lease to the
 * borrower in a channel message.
 *
 * Errors: NULL (or -1) with errno ENOMEM (no free block large enough now),
 * EINVAL (bad options, NULL handle, a request larger than the largest
 * block, a lease outside the pool, len > capacity).
 */
typedef struct gk_shm_pool gk_shm_pool;
typedef struct gk_shm_pool_view gk_shm_pool_view;
typedef struct gk_shm_block gk_shm_block;
typedef struct gk_shm_buf gk_shm_buf;

typedef struct gk_shm_pool_options {
    size_t size;    /* usable bytes (default 64 MiB; 16 MiB on Windows) */
    size_t granule; /* allocation unit: power of two, 64 B .. 1 GiB (4 KiB) */
    int huge_pages; /* GK_HUGE_PAGES_* */
} gk_shm_pool_options;

typedef struct gk_shm_lease {
    uint32_t index;      /* first granule of the block */
    uint32_t generation; /* the block's generation when lent */
    uint64_t len;        /* bytes of data */
} gk_shm_lease;

typedef struct gk_shm_pool_stats {
    size_t size, granule, free; /* bytes */
    size_t allocated, lent;     /* blocks */
    uint64_t bad_returns;
} gk_shm_pool_stats;

void gk_shm_pool_options_default(gk_shm_pool_options *opts);
gk_shm_pool *gk_shm_pool_new(const gk_shm_pool_options *opts);
int gk_shm_pool_get_stats(const gk_shm_pool *pool, gk_shm_pool_stats *stats);
/* Takes back every lent block (only once no borrower uses them, e.g. the
 * borrowing process died). */
int gk_shm_pool_reclaim_all(const gk_shm_pool *pool);
void gk_shm_pool_free(gk_shm_pool *pool);

/* A block of at least len bytes (a power of two number of granules). */
gk_shm_block *gk_shm_pool_alloc(const gk_shm_pool *pool, size_t len);
void *gk_shm_block_ptr(gk_shm_block *block);
size_t gk_shm_block_capacity(const gk_shm_block *block);
/* Lends len bytes of the block: fills *lease and frees the handle. */
int gk_shm_block_lend(gk_shm_block *block, size_t len, gk_shm_lease *lease);
/* Frees a block that was not lent. */
void gk_shm_block_free(gk_shm_block *block);

gk_shm_pool_view *gk_shm_pool_new_view(const gk_shm_pool *pool);
/* Takes a lease (once): the data stays valid until gk_shm_buf_free. */
gk_shm_buf *gk_shm_pool_view_take(const gk_shm_pool_view *view,
                                  const gk_shm_lease *lease);
void gk_shm_pool_view_free(gk_shm_pool_view *view);
const void *gk_shm_buf_ptr(const gk_shm_buf *buf);
size_t gk_shm_buf_len(const gk_shm_buf *buf);
/* Gives the block back to the allocator. */
void gk_shm_buf_free(gk_shm_buf *buf);

int gk_process_send_shm_pool_view(gk_process *process,
                                  const gk_shm_pool_view *view);
gk_shm_pool_view *gk_process_recv_shm_pool_view(gk_process *process);

#if !GK_EVENTFD_NATIVE && !defined(GK_EVENTFD_NO_COMPAT_NAMES)

/* <sys/eventfd.h> names, mapped onto the gk_* functions. */

/* See gk_eventfd(). */
GK_EVENTFD_INLINE int eventfd(unsigned int initval, int flags) {
    return gk_eventfd(initval, flags);
}

/* See gk_eventfd_read(). */
GK_EVENTFD_INLINE int eventfd_read(int fd, eventfd_t *value) {
    return gk_eventfd_read(fd, value);
}

/* See gk_eventfd_write(). */
GK_EVENTFD_INLINE int eventfd_write(int fd, eventfd_t value) {
    return gk_eventfd_write(fd, value);
}

#endif

#ifdef __cplusplus
} /* extern "C" */
#endif

/* Raw-I/O redirect; must stay after every declaration above. */
#if defined(GK_EVENTFD_WRAP_IO) && !GK_EVENTFD_NATIVE
#undef read
#undef write
#undef close
#define read(fd, buf, count) gk_read((fd), (buf), (count))
#define write(fd, buf, count) gk_write((fd), (buf), (count))
#define close(fd) gk_close(fd)
#undef dup
#undef dup2
#define dup(fd) gk_dup(fd)
#define dup2(oldfd, newfd) gk_dup2((oldfd), (newfd))
#ifndef _WIN32
#undef dup3
#define dup3(oldfd, newfd, flags) gk_dup3((oldfd), (newfd), (flags))
#endif
#ifdef _WIN32
#undef _read
#undef _write
#undef _close
#define _read(fd, buf, count) gk_read((fd), (buf), (count))
#define _write(fd, buf, count) gk_write((fd), (buf), (count))
#define _close(fd) gk_close(fd)
#undef _dup
#undef _dup2
#define _dup(fd) gk_dup(fd)
#define _dup2(oldfd, newfd) gk_dup2((oldfd), (newfd))
#endif
#endif

#endif /* ADAPTIF_GATOTKACA_EVENTFD_H */

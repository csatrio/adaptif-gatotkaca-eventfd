/*
 * Smoke test of the C ABI (build the crate with `--features ffi`).
 * Builds warning-free with `-std=c11 -Wall -Wextra -Werror` (gcc, clang) and
 * `/W3 /WX` (MSVC). Exits 0 on success; otherwise prints the failed check
 * and exits 1.
 *
 * Windows: gk_eventfd_socket() hands out a Winsock SOCKET. Winsock is
 * reference-counted per process and the Rust side initializes it for its own
 * sockets, but a C program that calls Winsock functions (WSAPoll here)
 * should still call WSAStartup itself, as done below.
 */

#if defined(__linux__) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE /* poll() and <sys/eventfd.h> under -std=c11 */
#endif

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#else
#include <poll.h>
#endif

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "adaptif_gatotkaca_eventfd.h"

static int failures = 0;

#define CHECK(cond)                                                                    \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            fprintf(stderr, "%s:%d: check failed: %s (errno %d)\n", __FILE__, __LINE__, \
                    #cond, errno);                                                     \
            failures++;                                                                \
        }                                                                              \
    } while (0)

/* 1 if fd is readable right now, 0 if not, -1 on error. */
static int readable(int fd) {
#ifdef _WIN32
    WSAPOLLFD p;
    uintptr_t s = gk_eventfd_socket(fd);
    if (s == GK_EVENTFD_INVALID_SOCKET) {
        return -1;
    }
    p.fd = (SOCKET)s;
    p.events = POLLRDNORM;
    p.revents = 0;
    if (WSAPoll(&p, 1, 0) < 0) {
        return -1;
    }
    return (p.revents & POLLRDNORM) != 0;
#else
    struct pollfd p;
    p.fd = fd;
    p.events = POLLIN;
    p.revents = 0;
    if (poll(&p, 1, 0) < 0) {
        return -1;
    }
    return (p.revents & POLLIN) != 0;
#endif
}

int main(void) {
    eventfd_t v = 0;
    unsigned char buf[16];
    uint64_t u;
    int fd, sem;

#ifdef _WIN32
    WSADATA wsa;
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        fprintf(stderr, "WSAStartup failed\n");
        return 1;
    }
#endif

    /* create, write, read */
    fd = gk_eventfd(0, GK_EFD_NONBLOCK | GK_EFD_CLOEXEC);
    CHECK(fd >= 0);
    if (fd < 0) {
        return 1;
    }
    CHECK(readable(fd) == 0);
    CHECK(gk_eventfd_write(fd, 3) == 0);
    CHECK(gk_eventfd_write(fd, 4) == 0);
    CHECK(readable(fd) == 1);
    CHECK(gk_eventfd_read(fd, &v) == 0);
    CHECK(v == 7);
    CHECK(readable(fd) == 0);

    /* non-blocking read of an empty counter */
    errno = 0;
    CHECK(gk_eventfd_read(fd, &v) == -1);
    CHECK(errno == EAGAIN);

    /* invalid arguments */
    errno = 0;
    CHECK(gk_eventfd_write(fd, UINT64_MAX) == -1);
    CHECK(errno == EINVAL);
    errno = 0;
    CHECK(gk_eventfd_read_buf(fd, buf, 4) == -1);
    CHECK(errno == EINVAL);
    errno = 0;
    CHECK(gk_eventfd(0, 0x40000000) == -1);
    CHECK(errno == EINVAL);

    /* buffer-level I/O: native-endian u64 */
    u = 42;
    memcpy(buf, &u, sizeof u);
    CHECK(gk_eventfd_write_buf(fd, buf, sizeof buf) == 8);
    memset(buf, 0, sizeof buf);
    CHECK(gk_eventfd_read_buf(fd, buf, sizeof buf) == 8);
    memcpy(&u, buf, sizeof u);
    CHECK(u == 42);

    /* close; the descriptor is gone afterwards */
    CHECK(gk_eventfd_close(fd) == 0);
    errno = 0;
    CHECK(gk_eventfd_close(fd) == -1);
    CHECK(errno == EBADF);

    /* semaphore mode */
    sem = gk_eventfd(2, GK_EFD_SEMAPHORE | GK_EFD_NONBLOCK);
    CHECK(sem >= 0);
    CHECK(gk_eventfd_read(sem, &v) == 0 && v == 1);
    CHECK(gk_eventfd_read(sem, &v) == 0 && v == 1);
    errno = 0;
    CHECK(gk_eventfd_read(sem, &v) == -1);
    CHECK(errno == EAGAIN);
    CHECK(gk_eventfd_close(sem) == 0);

#if !GK_EVENTFD_NATIVE
    /* the <sys/eventfd.h> names provided by the header on shim platforms */
    fd = eventfd(5, EFD_NONBLOCK);
    CHECK(fd >= 0);
    CHECK(eventfd_write(fd, 1) == 0);
    CHECK(eventfd_read(fd, &v) == 0 && v == 6);
    CHECK(gk_eventfd_close(fd) == 0);
#endif

#ifdef _WIN32
    WSACleanup();
#endif

    if (failures != 0) {
        fprintf(stderr, "smoke: %d check(s) failed\n", failures);
        return 1;
    }
    printf("smoke: all checks passed\n");
    return 0;
}

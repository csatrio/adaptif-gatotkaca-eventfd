/*
 * GK_EVENTFD_WRAP_IO example: Linux-style eventfd code that uses raw
 * read()/write()/close()/dup()/dup2() on the eventfd, compiled unchanged on
 * every platform. On native platforms (GK_EVENTFD_NATIVE) the redirect is a
 * no-op (the kernel eventfd handles raw I/O); elsewhere these calls become
 * gk_read/gk_write/gk_close/gk_dup/gk_dup2, which give eventfds eventfd
 * semantics and forward every other descriptor (the pipe below) to the real
 * functions.
 *
 * Exits 0 on success; otherwise prints the failed check and exits 1.
 */

#if defined(__linux__) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE /* pipe() under -std=c11 */
#endif

#define GK_EVENTFD_WRAP_IO
#include "adaptif_gatotkaca_eventfd.h"

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#ifdef _WIN32
#include <fcntl.h> /* _O_BINARY */
#include <io.h>    /* _pipe */
#else
#include <unistd.h> /* pipe */
#endif

static int failures = 0;

#define CHECK(cond)                                                                    \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            fprintf(stderr, "%s:%d: check failed: %s (errno %d)\n", __FILE__, __LINE__, \
                    #cond, errno);                                                     \
            failures++;                                                                \
        }                                                                              \
    } while (0)

int main(void) {
    uint64_t v;
    char text[8];
    int efd, efd2, copy, fds[2], wcopy;

    /* An eventfd driven with plain read/write/close, as on Linux. */
    efd = eventfd(0, EFD_NONBLOCK);
    CHECK(efd >= 0);
    v = 5;
    CHECK(write(efd, &v, sizeof v) == 8);
    CHECK(write(efd, &v, sizeof v) == 8);
    errno = 0;
    CHECK(write(efd, &v, 4) == -1);
    CHECK(errno == EINVAL);
    v = 0;
    CHECK(read(efd, &v, sizeof v) == 8);
    CHECK(v == 10);
    errno = 0;
    CHECK(read(efd, &v, sizeof v) == -1);
    CHECK(errno == EAGAIN);
    CHECK(close(efd) == 0);

    /* dup/dup2 give more descriptors on the same counter. */
    efd = eventfd(0, EFD_NONBLOCK);
    efd2 = eventfd(0, EFD_NONBLOCK);
    CHECK(efd >= 0 && efd2 >= 0);
    copy = dup(efd);
    CHECK(copy >= 0 && copy != efd);
    v = 3;
    CHECK(write(copy, &v, sizeof v) == 8);
    v = 0;
    CHECK(read(efd, &v, sizeof v) == 8);
    CHECK(v == 3);
    CHECK(dup2(efd, efd2) != -1); /* efd2 now refers to efd's counter */
    v = 4;
    CHECK(write(efd, &v, sizeof v) == 8);
    CHECK(close(efd) == 0); /* the copies keep it alive */
    v = 0;
    CHECK(read(efd2, &v, sizeof v) == 8);
    CHECK(v == 4);
    CHECK(close(efd2) == 0);
    CHECK(close(copy) == 0);

    /* Any other descriptor goes to the real read/write/close/dup. */
#ifdef _WIN32
    CHECK(_pipe(fds, 256, _O_BINARY) == 0);
#else
    CHECK(pipe(fds) == 0);
#endif
    wcopy = dup(fds[1]);
    CHECK(wcopy >= 0);
    CHECK(write(wcopy, "hey", 3) == 3); /* no 8-byte rule here */
    CHECK(close(wcopy) == 0);
    memset(text, 0, sizeof text);
    CHECK(read(fds[0], text, sizeof text) == 3);
    CHECK(memcmp(text, "hey", 3) == 0);
    CHECK(close(fds[1]) == 0);
    CHECK(read(fds[0], text, sizeof text) == 0); /* EOF */
    CHECK(close(fds[0]) == 0);

    if (failures != 0) {
        fprintf(stderr, "wrap_io: %d check(s) failed\n", failures);
        return 1;
    }
    printf("wrap_io: all checks passed\n");
    return 0;
}

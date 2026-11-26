/*
 * Process API example: a parent hands an eventfd to a child it spawns, and
 * the two ring each other through it (a cross-process doorbell). The same
 * code runs on Linux, macOS and Windows; only the spawning differs.
 *
 *   parent: gk_process_for_child -> spawn with GK_EVENTFD_PARENT -> send
 *   child:  gk_process_from_parent -> recv
 *
 * Then they exchange byte messages on the same connection (a handshake:
 * "hello" / "welcome"), check each other's process id, and on unix the
 * parent watches the connection's descriptor turn readable (EOF) once the
 * child has exited: the connection doubles as a lifeline. gk_process_alive
 * (since 0.3.3) sees the child exited before the parent reaps it.
 *
 * Finally a listener only this user may connect to (gk_listener_bind_ex,
 * since 0.3.2): connect to it and accept, on one thread.
 *
 * Run it without arguments; it re-runs itself with "child".
 * Exits 0 on success; otherwise prints the failed check and exits 1.
 */

#if defined(__linux__) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE /* setenv, fork under -std=c11 */
#endif

#include "adaptif_gatotkaca_eventfd.h"

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <process.h> /* _spawnl, _cwait */
#else
#include <poll.h>
#include <sys/wait.h>
#include <unistd.h>
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

static int child(void) {
    eventfd_t v = 0;
    gk_process *parent = gk_process_from_parent();
    CHECK(parent != NULL);
    if (parent == NULL) return 1;
    int efd = gk_process_recv(parent);
    CHECK(efd >= 0);
    if (efd >= 0) {
        CHECK(gk_eventfd_read(efd, &v) == 0); /* the parent rang 5 before sending */
        CHECK(v == 5);
        CHECK(gk_eventfd_write(efd, 7) == 0); /* ring back */
        CHECK(gk_eventfd_close(efd) == 0);
    }

    /* Handshake: a byte message each way. */
    static char msg[GK_PROCESS_BYTES_MAX]; /* always large enough */
    intptr_t n = gk_process_recv_bytes(parent, msg, sizeof msg);
    CHECK(n == 5 && memcmp(msg, "hello", 5) == 0);
    int64_t ppid = gk_process_peer_pid(parent);
#ifdef _WIN32
    CHECK(ppid > 0);
#else
    CHECK(ppid == 0 || ppid == (int64_t)getppid()); /* 0: the OS can't tell */
#endif
    CHECK(gk_process_send_bytes(parent, "welcome", 7) == 0);
    gk_process_free(parent);
    return failures ? 1 : 0;
}

int main(int argc, char **argv) {
    char env_value[GK_EVENTFD_TOKEN_MAX];
    eventfd_t v = 0;
    int status = -1;

    if (argc > 1 && strcmp(argv[1], "child") == 0) return child();

    int efd = gk_eventfd(0, 0);
    CHECK(efd >= 0);
    gk_process *kid = gk_process_for_child(env_value, sizeof env_value);
    CHECK(kid != NULL);
    if (efd < 0 || kid == NULL) return 1;

    /* Spawn ourselves with GK_EVENTFD_PARENT in the environment. */
#ifdef _WIN32
    CHECK(_putenv_s(GK_EVENTFD_PARENT_ENV, env_value) == 0);
    intptr_t h = _spawnl(_P_NOWAIT, argv[0], argv[0], "child", NULL);
    CHECK(h != -1);
#else
    CHECK(setenv(GK_EVENTFD_PARENT_ENV, env_value, 1) == 0);
    pid_t pid = fork();
    if (pid == 0) {
        execl(argv[0], argv[0], "child", (char *)NULL);
        _exit(127);
    }
    CHECK(pid > 0);
    CHECK(gk_process_expect_pid(kid, (unsigned long)pid) == 0);
#endif

    CHECK(gk_eventfd_write(efd, 5) == 0);    /* ring */
    CHECK(gk_process_send(kid, efd) == 0);   /* waits for the child to connect */

    /* Handshake on the same connection. */
    CHECK(gk_process_send_bytes(kid, "hello", 5) == 0);
    char small[2];
    CHECK(gk_process_recv_bytes(kid, small, sizeof small) == -1); /* too small: kept */
    CHECK(errno == EMSGSIZE);
    char reply[16];
    CHECK(gk_process_recv_bytes(kid, reply, sizeof reply) == 7);
    CHECK(memcmp(reply, "welcome", 7) == 0);
#ifndef _WIN32
    int64_t peer = gk_process_peer_pid(kid);
    CHECK(peer == 0 || peer == (int64_t)pid); /* 0: the OS can't tell */
    int link = gk_process_fd(kid);
    CHECK(link >= 0);
#else
    CHECK(gk_process_peer_pid(kid) > 0);
#endif

    /* gk_process_alive (since 0.3.3): this process runs; the child counts
     * as exited once it has exited, before anyone reaps it, and the check
     * never reaps it (waitpid below still gets its status). */
#ifdef _WIN32
    CHECK(gk_process_alive((uint32_t)_getpid()) == 1);
    CHECK(_cwait(&status, h, 0) != -1);
#else
    CHECK(gk_process_alive((uint32_t)getpid()) == 1);
    for (int i = 0; i < 5000 && gk_process_alive((uint32_t)pid) == 1; i++) poll(NULL, 0, 1);
    CHECK(gk_process_alive((uint32_t)pid) == 0);
    CHECK(waitpid(pid, &status, 0) == pid);
    status = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    /* The child is gone: its end of the connection reads as EOF. */
    struct pollfd pfd = {link, POLLIN, 0};
    CHECK(poll(&pfd, 1, 5000) == 1);
#endif
    CHECK(status == 0);
    CHECK(gk_process_recv_bytes(kid, reply, sizeof reply) == -1); /* EOF */

    CHECK(gk_eventfd_read(efd, &v) == 0);    /* the child rang back */
    CHECK(v == 7);
    CHECK(gk_eventfd_close(efd) == 0);
    gk_process_free(kid);

    /* A listener for this user only (Windows: DACL D:P(A;;FA;;;<user>) on
     * every pipe instance; unix: socket 0600, other users' connections
     * closed by accept). Connecting before accepting works on one thread. */
    char name[64];
#ifdef _WIN32
    snprintf(name, sizeof name, "process-example-%d", _getpid());
#else
    snprintf(name, sizeof name, "process-example-%d", (int)getpid());
#endif
    CHECK(gk_listener_bind_ex(name, 7) == NULL); /* unknown access value */
    CHECK(errno == EINVAL);
    gk_listener *mine = gk_listener_bind_ex(name, GK_LISTENER_ACCESS_CURRENT_USER);
    CHECK(mine != NULL);
    if (mine != NULL) {
        gk_process *client = gk_process_connect(name);
        CHECK(client != NULL);
        gk_process *server = gk_listener_accept(mine);
        CHECK(server != NULL);
        if (client != NULL && server != NULL) {
            CHECK(gk_process_send_bytes(client, "me", 2) == 0);
            CHECK(gk_process_recv_bytes(server, reply, sizeof reply) == 2);
            CHECK(memcmp(reply, "me", 2) == 0);
        }
        gk_process_free(client);
        gk_process_free(server);
        gk_listener_free(mine);
    }

    if (failures != 0) {
        fprintf(stderr, "process: %d check(s) failed\n", failures);
        return 1;
    }
    printf("process: all checks passed\n");
    return 0;
}

/*
 * Channel example: a parent streams messages to a child it spawns through a
 * shared-memory channel. The same code runs on Linux, macOS and Windows;
 * only the spawning differs.
 *
 *   parent: gk_channel_new -> gk_process_for_child -> spawn ->
 *           gk_process_send_receiver -> gk_sender_send x N -> gk_sender_free
 *   child:  gk_process_from_parent -> gk_process_recv_receiver ->
 *           gk_receiver_recv until EPIPE
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
#include <sys/wait.h>
#include <unistd.h>
#endif

#define MESSAGES 1000
#define MAX_MESSAGE 2048

static int failures = 0;

#define CHECK(cond)                                                                    \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            fprintf(stderr, "%s:%d: check failed: %s (errno %d)\n", __FILE__, __LINE__, \
                    #cond, errno);                                                     \
            failures++;                                                                \
        }                                                                              \
    } while (0)

/* Message i: (i * 7) % MAX_MESSAGE bytes, byte k = (k ^ i) & 0xff. */
static size_t fill(unsigned char *buf, unsigned i) {
    size_t len = (size_t)(i * 7u) % MAX_MESSAGE, k;
    for (k = 0; k < len; k++) buf[k] = (unsigned char)((k ^ i) & 0xff);
    return len;
}

static int child(void) {
    unsigned char got[MAX_MESSAGE], want[MAX_MESSAGE];
    unsigned i;
    gk_process *parent = gk_process_from_parent();
    CHECK(parent != NULL);
    if (parent == NULL) return 1;
    gk_receiver *rx = gk_process_recv_receiver(parent);
    CHECK(rx != NULL);
    if (rx == NULL) return 1;
    for (i = 0; i < MESSAGES; i++) {
        intptr_t n = gk_receiver_recv(rx, got, sizeof got);
        size_t len = fill(want, i);
        CHECK(n == (intptr_t)len);
        if (n != (intptr_t)len || memcmp(got, want, len) != 0) {
            fprintf(stderr, "message %u differs\n", i);
            failures++;
            break;
        }
    }
    errno = 0;
    CHECK(gk_receiver_recv(rx, got, sizeof got) == -1); /* sender closed */
    CHECK(errno == EPIPE);
    gk_receiver_free(rx);
    gk_process_free(parent);
    return failures ? 1 : 0;
}

int main(int argc, char **argv) {
    char env_value[GK_EVENTFD_TOKEN_MAX];
    unsigned char msg[MAX_MESSAGE];
    unsigned i;
    int status = -1;
    gk_sender *tx = NULL;
    gk_receiver *rx = NULL;

    if (argc > 1 && strcmp(argv[1], "child") == 0) return child();

    CHECK(gk_channel_new(1u << 16, MAX_MESSAGE, &tx, &rx) == 0);
    gk_process *kid = gk_process_for_child(env_value, sizeof env_value);
    CHECK(kid != NULL);
    if (tx == NULL || rx == NULL || kid == NULL) return 1;

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

    /* Hand the receiving end to the child, then stream messages. */
    CHECK(gk_process_send_receiver(kid, rx) == 0);
    gk_receiver_free(rx); /* our copy was detached by the send: no close */
    errno = 0;
    CHECK(gk_sender_send(tx, msg, MAX_MESSAGE + 1) == -1);
    CHECK(errno == EMSGSIZE);
    for (i = 0; i < MESSAGES; i++) {
        size_t len = fill(msg, i);
        CHECK(gk_sender_send(tx, msg, len) == 0);
    }
    gk_sender_free(tx); /* the child sees EPIPE after the last message */

#ifdef _WIN32
    CHECK(_cwait(&status, h, 0) != -1);
#else
    CHECK(waitpid(pid, &status, 0) == pid);
    status = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
#endif
    CHECK(status == 0);
    gk_process_free(kid);

    if (failures != 0) {
        fprintf(stderr, "channel: %d check(s) failed\n", failures);
        return 1;
    }
    printf("channel: all checks passed\n");
    return 0;
}

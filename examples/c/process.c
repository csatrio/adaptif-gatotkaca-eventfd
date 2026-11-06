/*
 * Process API example: a parent hands an eventfd to a child it spawns, and
 * the two ring each other through it (a cross-process doorbell). The same
 * code runs on Linux, macOS and Windows; only the spawning differs.
 *
 *   parent: gk_process_for_child -> spawn with GK_EVENTFD_PARENT -> send
 *   child:  gk_process_from_parent -> recv
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

#ifdef _WIN32
    CHECK(_cwait(&status, h, 0) != -1);
#else
    CHECK(waitpid(pid, &status, 0) == pid);
    status = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
#endif
    CHECK(status == 0);

    CHECK(gk_eventfd_read(efd, &v) == 0);    /* the child rang back */
    CHECK(v == 7);
    CHECK(gk_eventfd_close(efd) == 0);
    gk_process_free(kid);

    if (failures != 0) {
        fprintf(stderr, "process: %d check(s) failed\n", failures);
        return 1;
    }
    printf("process: all checks passed\n");
    return 0;
}

/*
 * Shared memory example: a parent hands a shared memory region and two
 * eventfds (doorbells) to a child it spawns. The parent writes a pattern and
 * rings "to_child"; the child checks it, writes a reply into the second half
 * and rings "to_parent"; the parent checks the reply. The same code runs on
 * Linux, macOS and Windows; only the spawning differs.
 *
 *   parent: gk_shm_new -> gk_process_for_child -> spawn ->
 *           gk_process_send_shm + gk_process_send x 2 -> write pattern ->
 *           gk_eventfd_write(to_child) -> gk_eventfd_read(to_parent) -> check
 *   child:  gk_process_from_parent -> gk_process_recv_shm +
 *           gk_process_recv x 2 -> gk_eventfd_read(to_child) -> check ->
 *           write reply -> gk_eventfd_write(to_parent)
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

#define LEN (4u << 20) /* 4 MiB */

static int failures = 0;

#define CHECK(cond)                                                                    \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            fprintf(stderr, "%s:%d: check failed: %s (errno %d)\n", __FILE__, __LINE__, \
                    #cond, errno);                                                     \
            failures++;                                                                \
        }                                                                              \
    } while (0)

static unsigned char pattern(size_t i) {
    return (unsigned char)((i * 2654435761u) >> 11);
}

/* Checks bytes [from, to) of mem against pattern(i) ^ x. */
static void verify(const unsigned char *mem, size_t from, size_t to, unsigned char x,
                   const char *what) {
    size_t i;
    for (i = from; i < to; i++) {
        if (mem[i] != (unsigned char)(pattern(i) ^ x)) {
            fprintf(stderr, "%s: byte %zu differs\n", what, i);
            failures++;
            return;
        }
    }
}

static int child(void) {
    eventfd_t v = 0;
    gk_process *parent = gk_process_from_parent();
    CHECK(parent != NULL);
    if (parent == NULL) return 1;
    gk_shm *shm = gk_process_recv_shm(parent);
    int to_child = gk_process_recv(parent);
    int to_parent = gk_process_recv(parent);
    CHECK(shm != NULL);
    CHECK(to_child >= 0 && to_parent >= 0);
    if (shm == NULL || to_child < 0 || to_parent < 0) return 1;
    CHECK(gk_shm_len(shm) == LEN);

    unsigned char *mem = gk_shm_ptr(shm);
    size_t i, half = LEN / 2;
    CHECK(gk_eventfd_read(to_child, &v) == 0); /* wait for the pattern */
    CHECK(v == 1);
    verify(mem, 0, half, 0, "child: pattern");
    for (i = half; i < LEN; i++) mem[i] = (unsigned char)(pattern(i) ^ 0xa5);
    CHECK(gk_eventfd_write(to_parent, 1) == 0); /* the reply is ready */

    CHECK(gk_eventfd_close(to_child) == 0);
    CHECK(gk_eventfd_close(to_parent) == 0);
    gk_shm_free(shm);
    gk_process_free(parent);
    return failures ? 1 : 0;
}

int main(int argc, char **argv) {
    char env_value[GK_EVENTFD_TOKEN_MAX];
    eventfd_t v = 0;
    int status = -1;
    size_t i, half = LEN / 2;

    if (argc > 1 && strcmp(argv[1], "child") == 0) return child();

    gk_shm_options opts;
    gk_shm_options_default(&opts);
    opts.huge_pages = GK_HUGE_PAGES_PREFER; /* huge pages if the system has them */
    gk_shm *shm = gk_shm_new_ex(LEN, &opts);
    int to_child = gk_eventfd(0, 0);
    int to_parent = gk_eventfd(0, 0);
    gk_process *kid = gk_process_for_child(env_value, sizeof env_value);
    CHECK(shm != NULL);
    CHECK(to_child >= 0 && to_parent >= 0);
    CHECK(kid != NULL);
    if (shm == NULL || to_child < 0 || to_parent < 0 || kid == NULL) return 1;
    CHECK(gk_shm_len(shm) == LEN);
    printf("shared_memory: %zu bytes, %zu-byte pages%s\n", gk_shm_len(shm),
           gk_shm_page_size(shm), gk_shm_is_huge(shm) == 1 ? " (huge)" : "");

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

    /* Hand over the memory and both doorbells; we keep using all three. */
    CHECK(gk_process_send_shm(kid, shm) == 0);
    CHECK(gk_process_send(kid, to_child) == 0);
    CHECK(gk_process_send(kid, to_parent) == 0);

    unsigned char *mem = gk_shm_ptr(shm);
    for (i = 0; i < half; i++) mem[i] = pattern(i);
    CHECK(gk_eventfd_write(to_child, 1) == 0); /* the pattern is ready */
    CHECK(gk_eventfd_read(to_parent, &v) == 0); /* wait for the reply */
    CHECK(v == 1);
    verify(mem, half, LEN, 0xa5, "parent: reply");
    verify(mem, 0, half, 0, "parent: pattern");

#ifdef _WIN32
    CHECK(_cwait(&status, h, 0) != -1);
#else
    CHECK(waitpid(pid, &status, 0) == pid);
    status = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
#endif
    CHECK(status == 0);
    gk_process_free(kid);
    CHECK(gk_eventfd_close(to_child) == 0);
    CHECK(gk_eventfd_close(to_parent) == 0);
    gk_shm_free(shm);

    if (failures != 0) {
        fprintf(stderr, "shared_memory: %d check(s) failed\n", failures);
        return 1;
    }
    printf("shared_memory: all checks passed\n");
    return 0;
}

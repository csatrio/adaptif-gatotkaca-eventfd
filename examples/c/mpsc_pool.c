/*
 * Multi-producer channel + shared memory pool example: several threads of a
 * parent lend zero-copy buffers to a child through one channel. The child
 * reads them in place (gk_receiver_recv_batch, gk_shm_pool_view_take) and
 * gives each block back by freeing it; the parent's pool gets them all back.
 *
 *   parent: gk_shm_pool_new + gk_channel_new_mpsc -> spawn ->
 *           gk_process_send_shm_pool_view + gk_process_send_receiver ->
 *           THREADS x (gk_mp_sender_clone, gk_shm_pool_alloc, write,
 *           gk_shm_block_lend, gk_mp_sender_send_kind(lease)) ->
 *           free the senders -> wait -> gk_shm_pool_get_stats: nothing lent
 *   child:  gk_process_recv_shm_pool_view + gk_process_recv_receiver ->
 *           gk_receiver_recv_batch until EPIPE -> gk_shm_pool_view_take,
 *           check, gk_shm_buf_free
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
#include <process.h> /* _spawnl, _cwait, _beginthreadex */
#include <windows.h>
#else
#include <pthread.h>
#include <sys/wait.h>
#include <unistd.h>
#endif

#define THREADS 4
#define PER_THREAD 2000
#define MAX_LEN 20000

static int failures = 0;

#define CHECK(cond)                                                                    \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            fprintf(stderr, "%s:%d: check failed: %s (errno %d)\n", __FILE__, __LINE__, \
                    #cond, errno);                                                     \
            failures++;                                                                \
        }                                                                              \
    } while (0)

/* Buffer i of thread t: len(t, i) bytes, byte k = (k * 31 + i + t) & 0xff. */
static size_t len_of(unsigned t, unsigned i) {
    return 1 + ((size_t)i * 7919u + t * 13u) % MAX_LEN;
}

static unsigned char byte_of(unsigned t, unsigned i, size_t k) {
    return (unsigned char)((k * 31u + i + t) & 0xff);
}

/* --- child -------------------------------------------------------------- */

struct child_state {
    gk_shm_pool_view *view;
    unsigned next[THREADS];
    unsigned received;
};

static void on_message(void *ctx, uint16_t kind, const void *msg, size_t len) {
    struct child_state *st = ctx;
    gk_shm_lease lease;
    unsigned t = kind, i;
    size_t k, n;
    if (t >= THREADS || len != sizeof lease) {
        failures++;
        return;
    }
    memcpy(&lease, msg, sizeof lease);
    i = st->next[t]++;
    gk_shm_buf *buf = gk_shm_pool_view_take(st->view, &lease);
    CHECK(buf != NULL);
    if (buf == NULL) return;
    n = gk_shm_buf_len(buf);
    CHECK(n == len_of(t, i));
    {
        const unsigned char *p = gk_shm_buf_ptr(buf);
        for (k = 0; k < n; k++) {
            if (p[k] != byte_of(t, i, k)) {
                fprintf(stderr, "thread %u buffer %u byte %zu differs\n", t, i, k);
                failures++;
                break;
            }
        }
    }
    gk_shm_buf_free(buf); /* gives the block back to the parent's pool */
    st->received++;
}

static int child(void) {
    struct child_state st;
    intptr_t n;
    memset(&st, 0, sizeof st);
    gk_process *parent = gk_process_from_parent();
    CHECK(parent != NULL);
    if (parent == NULL) return 1;
    st.view = gk_process_recv_shm_pool_view(parent);
    gk_receiver *rx = gk_process_recv_receiver(parent);
    CHECK(st.view != NULL && rx != NULL);
    if (st.view == NULL || rx == NULL) return 1;
    errno = 0;
    while ((n = gk_receiver_recv_batch(rx, 64, on_message, &st)) > 0) {
    }
    CHECK(n == -1 && errno == EPIPE); /* every sender clone was freed */
    CHECK(st.received == THREADS * PER_THREAD);
    gk_receiver_free(rx);
    gk_shm_pool_view_free(st.view);
    gk_process_free(parent);
    return failures ? 1 : 0;
}

/* --- parent ------------------------------------------------------------- */

struct producer {
    gk_mp_sender *tx; /* this thread's clone */
    const gk_shm_pool *pool;
    unsigned t;
};

#ifdef _WIN32
static unsigned __stdcall produce(void *arg) {
#else
static void *produce(void *arg) {
#endif
    struct producer *p = arg;
    unsigned i;
    size_t k;
    for (i = 0; i < PER_THREAD; i++) {
        size_t len = len_of(p->t, i);
        gk_shm_block *b;
        gk_shm_lease lease;
        /* The child gives blocks back as it reads them. */
        while ((b = gk_shm_pool_alloc(p->pool, len)) == NULL && errno == ENOMEM) {
        }
        CHECK(b != NULL);
        if (b == NULL) break;
        {
            unsigned char *dst = gk_shm_block_ptr(b);
            for (k = 0; k < len; k++) dst[k] = byte_of(p->t, i, k);
        }
        CHECK(gk_shm_block_lend(b, len, &lease) == 0);
        CHECK(gk_mp_sender_send_kind(p->tx, (uint16_t)p->t, &lease, sizeof lease) == 0);
    }
    gk_mp_sender_free(p->tx);
    return 0;
}

int main(int argc, char **argv) {
    char env_value[GK_EVENTFD_TOKEN_MAX];
    struct producer producers[THREADS];
    gk_mp_sender *tx = NULL;
    gk_receiver *rx = NULL;
    gk_shm_pool_options popts;
    gk_shm_pool_stats stats;
    unsigned t;
    int status = -1;

    if (argc > 1 && strcmp(argv[1], "child") == 0) return child();

    gk_shm_pool_options_default(&popts);
    popts.size = 4u << 20; /* smaller than what flows through it */
    gk_shm_pool *pool = gk_shm_pool_new(&popts);
    gk_shm_pool_view *view = pool ? gk_shm_pool_new_view(pool) : NULL;
    CHECK(gk_channel_new_mpsc(NULL, &tx, &rx) == 0);
    gk_process *kid = gk_process_for_child(env_value, sizeof env_value);
    CHECK(pool != NULL && view != NULL && kid != NULL);
    if (pool == NULL || view == NULL || tx == NULL || rx == NULL || kid == NULL) return 1;

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

    CHECK(gk_process_send_shm_pool_view(kid, view) == 0);
    CHECK(gk_process_send_receiver(kid, rx) == 0);
    gk_receiver_free(rx); /* detached by the send: no close */
    gk_shm_pool_view_free(view);

    for (t = 0; t < THREADS; t++) {
        producers[t].tx = gk_mp_sender_clone(tx);
        producers[t].pool = pool;
        producers[t].t = t;
        CHECK(producers[t].tx != NULL);
    }
    gk_mp_sender_free(tx); /* the clones keep the channel open */
    {
#ifdef _WIN32
        HANDLE threads[THREADS];
        for (t = 0; t < THREADS; t++)
            threads[t] = (HANDLE)_beginthreadex(NULL, 0, produce, &producers[t], 0, NULL);
        WaitForMultipleObjects(THREADS, threads, TRUE, INFINITE);
        for (t = 0; t < THREADS; t++) CloseHandle(threads[t]);
#else
        pthread_t threads[THREADS];
        for (t = 0; t < THREADS; t++) CHECK(pthread_create(&threads[t], NULL, produce, &producers[t]) == 0);
        for (t = 0; t < THREADS; t++) pthread_join(threads[t], NULL);
#endif
    }

#ifdef _WIN32
    CHECK(_cwait(&status, h, 0) != -1);
#else
    CHECK(waitpid(pid, &status, 0) == pid);
    status = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
#endif
    CHECK(status == 0);
    CHECK(gk_shm_pool_get_stats(pool, &stats) == 0);
    CHECK(stats.lent == 0 && stats.allocated == 0 && stats.bad_returns == 0);
    CHECK(stats.free == stats.size);
    gk_shm_pool_free(pool);
    gk_process_free(kid);

    if (failures != 0) {
        fprintf(stderr, "mpsc_pool: %d check(s) failed\n", failures);
        return 1;
    }
    printf("mpsc_pool: all checks passed\n");
    return 0;
}

/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_vmpause.c -- the menu's Pause / Resume runner (nb_vmpause.h) with
 * real commands: the state flips only on exit 0, a failure leaves it and
 * says why, one command at a time, and polling never blocks.
 */
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "nb_vmpause.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

static int settle_upscaler(struct nb_upscaler *u)
{
    struct timespec ts = { 0, 10 * 1000 * 1000 };
    int r = NB_VMPAUSE_BUSY;
    int i;

    for (i = 0; i < 500 && r == NB_VMPAUSE_BUSY; i++) {
        r = nb_upscaler_poll(u);
        if (r == NB_VMPAUSE_BUSY) {
            nanosleep(&ts, NULL);
        }
    }
    return r;
}

/* The Upscaler row's runner: `CMD WORD`, the mode taken on exit 0 only. */
static void test_upscaler(void)
{
    struct nb_upscaler u;

    nb_upscaler_init(&u);
    CHECK(u.mode == -1 && u.pid == 0, "nothing set at start");
    CHECK(nb_upscaler_start(&u, NULL, "dlss", 1) < 0, "no command");
    CHECK(nb_upscaler_start(&u, "", "dlss", 1) < 0, "empty command");

    /* the word is the command's last argument */
    CHECK(nb_upscaler_start(&u, "f() { [ \"$1\" = dlss ]; }; f", "dlss", 1) == 0,
          "starts");
    CHECK(nb_upscaler_start(&u, "exit 0", "fsr", 2) < 0, "one at a time");
    CHECK(settle_upscaler(&u) == NB_VMPAUSE_DONE && u.mode == 1,
          "dlss set: mode %d", u.mode);

    /* a failure keeps the last mode */
    CHECK(nb_upscaler_start(&u, "f() { exit 5; }; f", "xess", 3) == 0, "starts");
    CHECK(settle_upscaler(&u) == NB_VMPAUSE_FAILED && u.mode == 1 &&
          u.status == 5, "xess failed: mode %d status %d", u.mode, u.status);
}

/* Poll like the session's tick does, up to ~5 s. */
static int settle(struct nb_vmpause *s)
{
    struct timespec ts = { 0, 10 * 1000 * 1000 };
    int r = NB_VMPAUSE_BUSY;
    int i;

    for (i = 0; i < 500 && r == NB_VMPAUSE_BUSY; i++) {
        r = nb_vmpause_poll(s);
        if (r == NB_VMPAUSE_BUSY) {
            nanosleep(&ts, NULL);
        }
    }
    return r;
}

int main(void)
{
    struct nb_vmpause s;

    memset(&s, 0, sizeof(s));
    CHECK(nb_vmpause_poll(&s) == NB_VMPAUSE_IDLE, "idle at start");
    CHECK(nb_vmpause_start(&s, NULL, true) < 0, "no command");
    CHECK(nb_vmpause_start(&s, "", true) < 0, "empty command");

    /* pause succeeds */
    CHECK(nb_vmpause_start(&s, "exit 0", true) == 0, "pause starts");
    CHECK(nb_vmpause_busy(&s), "busy while it runs");
    CHECK(settle(&s) == NB_VMPAUSE_DONE, "pause done");
    CHECK(s.paused && !nb_vmpause_busy(&s), "paused");

    /* resume fails: still paused, the exit code kept */
    CHECK(nb_vmpause_start(&s, "exit 3", false) == 0, "resume starts");
    CHECK(settle(&s) == NB_VMPAUSE_FAILED, "resume failed");
    CHECK(s.paused && s.status == 3, "still paused, status %d", s.status);

    /* one at a time; polling a running command does not block */
    CHECK(nb_vmpause_start(&s, "sleep 0.3", false) == 0, "slow resume");
    CHECK(nb_vmpause_start(&s, "exit 0", true) < 0, "second refused");
    CHECK(nb_vmpause_poll(&s) == NB_VMPAUSE_BUSY, "poll returns at once");
    CHECK(settle(&s) == NB_VMPAUSE_DONE && !s.paused, "resumed");

    /* a command that cannot run is a failure, not a flip */
    CHECK(nb_vmpause_start(&s, "/nonexistent/conduit pause vm 2>/dev/null", true) == 0,
          "starts the shell");
    CHECK(settle(&s) == NB_VMPAUSE_FAILED && !s.paused && s.status == 127,
          "missing program: status %d", s.status);

    /* killed by a signal */
    CHECK(nb_vmpause_start(&s, "kill -TERM $$", true) == 0, "self-kill");
    CHECK(settle(&s) == NB_VMPAUSE_FAILED && !s.paused &&
          s.status == 128 + 15, "signal: status %d", s.status);

    /* stdin is /dev/null: a command that reads it does not hang */
    CHECK(nb_vmpause_start(&s, "read x || exit 4; exit 0", true) == 0,
          "reads stdin");
    CHECK(settle(&s) == NB_VMPAUSE_FAILED && s.status == 4,
          "EOF, not a hang: status %d", s.status);

    test_upscaler();

    printf("test_vmpause: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}

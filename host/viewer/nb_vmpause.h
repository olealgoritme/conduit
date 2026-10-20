/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_vmpause.h -- the menu's Pause / Resume.
 *
 * --vm-pause-cmd and --vm-resume-cmd (`conduit pause NAME`, `conduit resume
 * NAME`) are run through /bin/sh WITHOUT waiting: resuming can take seconds
 * (the GPU backend confirms it kept the guest's GPU state), and the viewer's
 * thread also draws the window and dispatches input.  The child is reaped
 * with waitpid(WNOHANG) from the session's UI tick, and the state the menu
 * shows flips only when the command exits 0.
 *
 * The viewer gets no pause signal from anywhere else (no frames is not one:
 * an idle guest sends none either), so `paused` is what its own commands
 * did.  Both commands are idempotent -- pausing a paused VM or resuming a
 * running one succeeds -- so a VM paused or resumed behind the viewer's back
 * puts the label right with one click.
 *
 * Display-free: test/test_vmpause.c drives it with real commands.
 */
#ifndef NB_VMPAUSE_H
#define NB_VMPAUSE_H

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdbool.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

struct nb_vmpause {
    pid_t pid;                  /* command in flight, 0 none                 */
    bool  pausing;              /* what it was asked: true = pause           */
    bool  paused;               /* the VM is paused (by our last command)    */
    int   status;               /* exit code of the last failed command      */
};

/* What nb_vmpause_poll() saw. */
enum {
    NB_VMPAUSE_IDLE = 0,        /* nothing in flight                         */
    NB_VMPAUSE_BUSY,            /* still running                             */
    NB_VMPAUSE_DONE,            /* exited 0: `paused` flipped                */
    NB_VMPAUSE_FAILED,          /* non-zero exit or killed; `status` says    */
};

/* How often the session's tick polls while a command runs, ms. */
#define NB_VMPAUSE_POLL_MS 100

/*
 * Start `cmd` to pause (pause = true) or resume the VM.  0 when it runs;
 * -1 when there is no command, one is already in flight, or fork failed.
 */
static inline int nb_vmpause_start(struct nb_vmpause *s, const char *cmd,
                                   bool pause)
{
    pid_t p;

    if (!cmd || !*cmd || s->pid > 0) {
        return -1;
    }
    p = fork();
    if (p < 0) {
        return -1;
    }
    if (p == 0) {
        sigset_t none;
        int fd;

        sigemptyset(&none);
        sigprocmask(SIG_SETMASK, &none, NULL);
        signal(SIGPIPE, SIG_DFL);
        fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
        if (fd >= 0) {
            dup2(fd, STDIN_FILENO);
        }
        execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        _exit(127);
    }
    s->pid = p;
    s->pausing = pause;
    return 0;
}

/* Reap the command if it ended (never blocks). */
static inline int nb_vmpause_poll(struct nb_vmpause *s)
{
    int st = 0;
    pid_t r;

    if (s->pid <= 0) {
        return NB_VMPAUSE_IDLE;
    }
    do {
        r = waitpid(s->pid, &st, WNOHANG);
    } while (r < 0 && errno == EINTR);
    if (r == 0) {
        return NB_VMPAUSE_BUSY;
    }
    s->pid = 0;
    if (r > 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0) {
        s->paused = s->pausing;
        s->status = 0;
        return NB_VMPAUSE_DONE;
    }
    s->status = r < 0 ? -1 : WIFEXITED(st) ? WEXITSTATUS(st)
                                           : 128 + WTERMSIG(st);
    return NB_VMPAUSE_FAILED;
}

/* The menu's view of it. */
static inline bool nb_vmpause_busy(const struct nb_vmpause *s)
{
    return s->pid > 0;
}

#endif /* NB_VMPAUSE_H */

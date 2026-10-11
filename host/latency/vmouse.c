// vmouse.c -- a virtual mouse at a fixed report rate, to measure what reaches
// the guest (guest/windows/tools/rawcount.c counts the reports there).
//
// Creates a uinput mouse and keyboard, presses Ctrl+Alt+G (the viewer's
// grab), moves the pointer in circles of one turn per second at HZ reports a
// second for SECONDS, one SYN_REPORT per report as a real HZ mouse sends, and
// presses Ctrl+Alt+G again to release the grab. The viewer window must have
// the focus. Each second it prints the reports sent and their |dx| and |dy|
// sums, the numbers rawcount prints in the guest.
//
//   cc -O2 -Wall -o vmouse host/latency/vmouse.c -lm
//   ./vmouse [SECONDS [HZ]]        (default 6 s at 1000 Hz)
//
// Needs write access to /dev/uinput.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/uinput.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

static int fd;

static void ev(int type, int code, int value)
{
    struct input_event e;

    memset(&e, 0, sizeof e);
    e.type = type;
    e.code = code;
    e.value = value;
    if (write(fd, &e, sizeof e) != sizeof e) {
        perror("vmouse: write");
    }
}

static void syn(void) { ev(EV_SYN, SYN_REPORT, 0); }

static void key(int code, int value)
{
    ev(EV_KEY, code, value);
    syn();
}

static void ms(int n) { usleep(n * 1000); }

// Ctrl+Alt+G: the viewer toggles its grab.
static void chord(void)
{
    key(KEY_LEFTCTRL, 1);
    key(KEY_LEFTALT, 1);
    ms(30);
    key(KEY_G, 1);
    ms(30);
    key(KEY_G, 0);
    key(KEY_LEFTALT, 0);
    key(KEY_LEFTCTRL, 0);
}

static double secs_of(const struct timespec *t) { return t->tv_sec + t->tv_nsec * 1e-9; }

static void add_ns(struct timespec *t, long ns)
{
    t->tv_nsec += ns;
    while (t->tv_nsec >= 1000000000L) {
        t->tv_nsec -= 1000000000L;
        t->tv_sec++;
    }
}

int main(int argc, char **argv)
{
    double secs = argc > 1 ? atof(argv[1]) : 6;
    int hz = argc > 2 ? atoi(argv[2]) : 1000;
    static const int keys[] = {BTN_LEFT, BTN_RIGHT, KEY_LEFTCTRL, KEY_LEFTALT, KEY_G};
    struct uinput_setup us;
    struct timespec start, next, now;
    long n = 0, sec_n = 0, sec_dx = 0, sec_dy = 0, period_ns;
    double fx = 0, fy = 0;
    int sec = 1;

    if (secs <= 0 || hz <= 0 || hz > 8000) {
        fprintf(stderr, "usage: vmouse [SECONDS [HZ]]  (HZ 1..8000)\n");
        return 2;
    }
    period_ns = 1000000000L / hz;
    fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0) {
        perror("vmouse: /dev/uinput");
        return 1;
    }
    ioctl(fd, UI_SET_EVBIT, EV_KEY);
    ioctl(fd, UI_SET_EVBIT, EV_REL);
    ioctl(fd, UI_SET_EVBIT, EV_SYN);
    ioctl(fd, UI_SET_RELBIT, REL_X);
    ioctl(fd, UI_SET_RELBIT, REL_Y);
    for (unsigned i = 0; i < sizeof keys / sizeof keys[0]; i++) {
        ioctl(fd, UI_SET_KEYBIT, keys[i]);
    }
    memset(&us, 0, sizeof us);
    us.id.bustype = BUS_VIRTUAL;
    us.id.vendor = 0x1234;
    us.id.product = 0x5678;
    snprintf(us.name, sizeof us.name, "conduit-vmouse");
    if (ioctl(fd, UI_DEV_SETUP, &us) < 0 || ioctl(fd, UI_DEV_CREATE) < 0) {
        perror("vmouse: uinput setup");
        return 1;
    }
    sleep(1); // the compositor opens the new device
    chord();
    ms(500);

    clock_gettime(CLOCK_MONOTONIC, &start);
    next = start;
    for (;;) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        double t = secs_of(&now) - secs_of(&start);
        if (t >= secs) {
            break;
        }
        // One turn a second, 3 counts per report: every report moves (one
        // axis is always at least 2), so each tick is one report. At 1000 Hz
        // that is about 1910 a second in |dx| and in |dy|.
        double a = t * 2 * M_PI;
        fx += 3 * cos(a);
        fy += 3 * sin(a);
        int dx = (int)fx, dy = (int)fy;
        fx -= dx;
        fy -= dy;
        if (dx || dy) {
            if (dx) {
                ev(EV_REL, REL_X, dx);
            }
            if (dy) {
                ev(EV_REL, REL_Y, dy);
            }
            syn();
            n++;
            sec_n++;
            sec_dx += labs(dx);
            sec_dy += labs(dy);
        }
        if (t >= sec) {
            printf("%3d s: %5ld reports/s  |dx| %6ld  |dy| %6ld\n", sec, sec_n, sec_dx, sec_dy);
            fflush(stdout);
            sec_n = sec_dx = sec_dy = 0;
            sec++;
        }
        add_ns(&next, period_ns);
        while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL) == EINTR) {
        }
    }
    clock_gettime(CLOCK_MONOTONIC, &now);
    double took = secs_of(&now) - secs_of(&start);
    printf("sent %ld reports in %.1f s (%.0f/s)\n", n, took, n / took);
    ms(300);
    chord();
    ms(300);
    ioctl(fd, UI_DEV_DESTROY);
    close(fd);
    return 0;
}

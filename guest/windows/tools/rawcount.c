/* rawcount.c -- count the mouse reports Windows delivers, per second.
 *
 * Registers for raw mouse input with RIDEV_INPUTSINK, so it counts while
 * another window (a game, the desktop) has the focus. Each second it prints
 * the reports received, the sum of |dx| and |dy| over them, and the longest
 * gap between two reports; then one column per mouse device (the PS/2 mouse,
 * vioinput's Conduit Mouse and Tablet, ...), numbered in the order they
 * first report. Device names go to stderr when first seen.
 *
 *   rawcount.exe [SECONDS]        (default 10)
 *
 * With host/latency/vmouse.c moving the host's pointer at N Hz through the
 * viewer, reports/s should be N and |dx| should match what vmouse printed.
 *
 * Build (mingw-w64, on the host):
 *   x86_64-w64-mingw32-gcc -O2 -Wall -Wextra -o rawcount.exe \
 *       guest/windows/tools/rawcount.c -luser32
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

#define MAX_DEVS 8

static HANDLE devs[MAX_DEVS];
static int ndevs;
static long per_dev[MAX_DEVS];
static long count, sum_dx, sum_dy;
static LONGLONG last_qpc, max_gap_qpc;

static int dev_index(HANDLE h)
{
    char name[512];
    UINT len = sizeof name;
    int i;

    for (i = 0; i < ndevs; i++) {
        if (devs[i] == h) {
            return i;
        }
    }
    if (ndevs == MAX_DEVS) {
        return MAX_DEVS - 1; /* the last column takes the rest */
    }
    devs[ndevs] = h;
    if (h == NULL) {
        snprintf(name, sizeof name, "(no device: injected input)");
    } else if (GetRawInputDeviceInfoA(h, RIDI_DEVICENAME, name, &len) == (UINT)-1) {
        snprintf(name, sizeof name, "(name unavailable)");
    }
    fprintf(stderr, "device %d: %s\n", ndevs, name);
    return ndevs++;
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_INPUT) {
        RAWINPUT ri;
        UINT sz = sizeof ri;

        if (GetRawInputData((HRAWINPUT)l, RID_INPUT, &ri, &sz,
                            sizeof(RAWINPUTHEADER)) != (UINT)-1 &&
            ri.header.dwType == RIM_TYPEMOUSE) {
            LARGE_INTEGER t;

            QueryPerformanceCounter(&t);
            if (last_qpc != 0 && t.QuadPart - last_qpc > max_gap_qpc) {
                max_gap_qpc = t.QuadPart - last_qpc;
            }
            last_qpc = t.QuadPart;
            count++;
            if (!(ri.data.mouse.usFlags & MOUSE_MOVE_ABSOLUTE)) {
                sum_dx += labs(ri.data.mouse.lLastX);
                sum_dy += labs(ri.data.mouse.lLastY);
            }
            per_dev[dev_index(ri.header.hDevice)]++;
        }
    }
    return DefWindowProcW(h, m, w, l);
}

int main(int argc, char **argv)
{
    int secs = argc > 1 ? atoi(argv[1]) : 10;
    WNDCLASSW wc = {0};
    RAWINPUTDEVICE rid;
    LARGE_INTEGER freq;
    ULONGLONG t0, next;
    HWND h;
    MSG msg;

    if (secs <= 0) {
        fprintf(stderr, "usage: rawcount [SECONDS]\n");
        return 2;
    }
    QueryPerformanceFrequency(&freq);
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"rawcount";
    RegisterClassW(&wc);
    h = CreateWindowW(L"rawcount", L"rawcount", 0, 0, 0, 0, 0, HWND_MESSAGE,
                      NULL, wc.hInstance, NULL);
    rid.usUsagePage = 0x01; /* generic desktop */
    rid.usUsage = 0x02;     /* mouse */
    rid.dwFlags = RIDEV_INPUTSINK;
    rid.hwndTarget = h;
    if (!RegisterRawInputDevices(&rid, 1, sizeof rid)) {
        fprintf(stderr, "RegisterRawInputDevices failed: %lu\n", GetLastError());
        return 1;
    }

    t0 = GetTickCount64();
    next = t0 + 1000;
    while (GetTickCount64() - t0 < (ULONGLONG)secs * 1000) {
        ULONGLONG now;

        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
            DispatchMessageW(&msg);
        }
        now = GetTickCount64();
        if (now >= next) {
            int i;

            printf("%3llu s: %5ld reports/s  |dx| %6ld  |dy| %6ld  max gap %6.2f ms  per device:",
                   (now - t0) / 1000, count, sum_dx, sum_dy,
                   1000.0 * (double)max_gap_qpc / (double)freq.QuadPart);
            for (i = 0; i < ndevs; i++) {
                printf(" %ld", per_dev[i]);
                per_dev[i] = 0;
            }
            printf("\n");
            fflush(stdout);
            count = sum_dx = sum_dy = 0;
            max_gap_qpc = 0;
            next += 1000;
        }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 5, QS_ALLINPUT);
    }
    return 0;
}

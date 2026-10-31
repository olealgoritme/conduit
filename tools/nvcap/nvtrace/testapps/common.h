/*
 * Shared helpers for the nvtrace Windows test programs.
 *
 * - every log line is flushed at once (output is usually captured to a file)
 * - a watchdog thread terminates the process with "FAIL: watchdog ..." if a
 *   test runs too long, so a stuck driver call can never hang the caller
 * - an unhandled-exception filter prints "FAIL: unhandled exception ..."
 *   instead of opening a Windows Error Reporting dialog
 */
#ifndef NVTRACE_TESTAPPS_COMMON_H
#define NVTRACE_TESTAPPS_COMMON_H

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#define NV_VENDOR_ID 0x10DEu
#define WATCHDOG_MS 180000u
#define WAIT_TIMEOUT_MS 5000u

static const char *g_test_name = "test";

/* Raw write that takes no CRT lock; used from the watchdog and crash paths. */
static inline void raw_out(const char *s)
{
    HANDLE h = GetStdHandle(STD_OUTPUT_HANDLE);
    DWORD written;
    if (h && h != INVALID_HANDLE_VALUE)
        WriteFile(h, s, (DWORD)strlen(s), &written, NULL);
}

static inline void logmsg(const char *fmt, ...)
{
    va_list ap;
    printf("[%s] ", g_test_name);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    printf("\n");
    fflush(stdout);
}

/* Prints the final FAIL line and returns the process exit code (1). */
static inline int fail(const char *fmt, ...)
{
    va_list ap;
    printf("FAIL: ");
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    printf("\n");
    fflush(stdout);
    return 1;
}

static inline int pass(void)
{
    printf("PASS\n");
    fflush(stdout);
    return 0;
}

static LONG WINAPI crash_filter(EXCEPTION_POINTERS *ep)
{
    char buf[160];
    snprintf(buf, sizeof(buf), "\nFAIL: unhandled exception 0x%08lx at %p\n",
             (unsigned long)ep->ExceptionRecord->ExceptionCode,
             ep->ExceptionRecord->ExceptionAddress);
    raw_out(buf); /* no fflush: the crash may have happened inside stdio */
    TerminateProcess(GetCurrentProcess(), 3);
    return EXCEPTION_EXECUTE_HANDLER;
}

static DWORD WINAPI watchdog_thread(LPVOID arg)
{
    char buf[128];
    Sleep((DWORD)(UINT_PTR)arg);
    snprintf(buf, sizeof(buf), "\nFAIL: watchdog timeout after %lu ms (a call did not return)\n",
             (unsigned long)(UINT_PTR)arg);
    raw_out(buf);
    TerminateProcess(GetCurrentProcess(), 4);
    return 0;
}

static inline void common_init(const char *name)
{
    HANDLE t;
    g_test_name = name;
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    SetUnhandledExceptionFilter(crash_filter);
    t = CreateThread(NULL, 0, watchdog_thread, (LPVOID)(UINT_PTR)WATCHDOG_MS, 0, NULL);
    if (t)
        CloseHandle(t);
    logmsg("start (pid %lu, watchdog %u ms)", (unsigned long)GetCurrentProcessId(), WATCHDOG_MS);
}

/* GetProcAddress without function-pointer cast warnings. */
static inline void *load_sym(HMODULE mod, const char *name)
{
    FARPROC p = mod ? GetProcAddress(mod, name) : NULL;
    void *v;
    memcpy(&v, &p, sizeof(v));
    return v;
}

static inline double now_ms(void)
{
    static LARGE_INTEGER freq;
    LARGE_INTEGER t;
    if (!freq.QuadPart)
        QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1000.0 / (double)freq.QuadPart;
}

static inline void wide_to_utf8(const WCHAR *w, char *out, int out_size)
{
    if (!WideCharToMultiByte(CP_UTF8, 0, w, -1, out, out_size, NULL, NULL))
        snprintf(out, (size_t)out_size, "?");
}

static inline int has_arg(int argc, char **argv, const char *flag)
{
    int i;
    for (i = 1; i < argc; i++)
        if (!strcmp(argv[i], flag))
            return 1;
    return 0;
}

/* "--name N" (decimal) or def; N is clamped to [lo, hi]. */
static inline int arg_int(int argc, char **argv, const char *flag, int def, int lo, int hi)
{
    int i, v = def;
    for (i = 1; i + 1 < argc; i++)
        if (!strcmp(argv[i], flag))
            v = atoi(argv[i + 1]);
    return v < lo ? lo : v > hi ? hi : v;
}

/*
 * Steady-state workload sizes shared by the tests. Defaults are chosen so
 * that each test runs for several seconds (long enough to show the
 * driver's steady state, not only its first-use paths) while staying small
 * and deterministic; --quick restores the short smoke test.
 *   --frames N   presented frames (windowed phase)
 *   --iters N    compute dispatch / kernel launch iterations
 */
#define STEADY_FRAMES 300
#define STEADY_ITERS 200
#define QUICK_FRAMES 30
/*
 * Present phases also stop after this long: with the displays powered off
 * (the capture runs keep the monitor dark) vsync-paced presents can drop to
 * a few frames per second, and the frame count must not run into the
 * watchdog.
 */
#define PRESENT_BUDGET_MS 30000.0
#define QUICK_ITERS 1

/* Allocation sizes swept by every test, in bytes (4 KiB .. 16 MiB; bigger
 * ones only grow the level-1 logs, which snapshot mapped memory). */
static const unsigned alloc_sweep_sizes[] = {
    4096u, 65536u, 262144u, 1u << 20, 4u << 20, 16u << 20,
};
#define ALLOC_SWEEP_N (sizeof(alloc_sweep_sizes) / sizeof(alloc_sweep_sizes[0]))

/* Deterministic fill pattern: word i of sweep buffer s. */
static inline unsigned sweep_word(unsigned s, unsigned i)
{
    return (s + 1u) * 0x9e3779b9u ^ (i * 2654435761u);
}

#endif

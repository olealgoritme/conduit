/* nvinject: start a program suspended, load nvtrace into it, run it.
 *
 *   nvinject [-d dir] [-f] [-t seconds] <exe> [args...]
 *   nvinject -p <pid>
 *
 * The nvtrace DLL (nvtrace64.dll / nvtrace32.dll) must sit next to nvinject.
 * A 32-bit target is handed to nvinject32.exe (and the other way round). */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>

#ifdef _WIN64
#define SELF_MACHINE IMAGE_FILE_MACHINE_AMD64
#define DLL_NAME L"nvtrace64.dll"
#define OTHER_INJECT L"nvinject32.exe"
#else
#define SELF_MACHINE IMAGE_FILE_MACHINE_I386
#define DLL_NAME L"nvtrace32.dll"
#define OTHER_INJECT L"nvinject.exe"
#endif

static wchar_t self_dir[MAX_PATH];

static void usage(void)
{
    fprintf(stderr,
            "usage: nvinject [-d dir] [-f] [-t seconds] <exe> [args...]\n"
            "       nvinject -p <pid>\n"
            "  -d dir   log directory (NVTRACE_DIR, default C:\\nvtrace)\n"
            "  -f       follow child processes (inject them too)\n"
            "  -t sec   kill the program after sec seconds (exit code 124)\n"
            "  -p pid   load nvtrace into an existing process and exit\n"
            "Settings are read by the DLL from NVTRACE_* environment variables.\n");
}

/* Append one argument with the quoting CommandLineToArgvW undoes. */
static void append_arg(wchar_t *out, size_t cap, const wchar_t *a)
{
    size_t n = wcslen(out);
    int quote = !*a || wcspbrk(a, L" \t\"") != NULL;
    if (n && n + 1 < cap)
        out[n++] = L' ';
    if (quote && n + 1 < cap)
        out[n++] = L'"';
    for (const wchar_t *p = a;; p++) {
        size_t bs = 0;
        while (*p == L'\\') {
            bs++;
            p++;
        }
        if (!*p) {
            if (quote)
                bs *= 2;
            while (bs-- && n + 1 < cap)
                out[n++] = L'\\';
            break;
        }
        if (*p == L'"') {
            bs = bs * 2 + 1;
        }
        while (bs-- && n + 1 < cap)
            out[n++] = L'\\';
        if (n + 1 < cap)
            out[n++] = *p;
    }
    if (quote && n + 1 < cap)
        out[n++] = L'"';
    out[n] = 0;
}

static WORD exe_machine(const wchar_t *path)
{
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                           OPEN_EXISTING, 0, NULL);
    if (f == INVALID_HANDLE_VALUE)
        return 0;
    IMAGE_DOS_HEADER dos;
    IMAGE_FILE_HEADER fh;
    DWORD sig = 0, rd = 0;
    WORD m = 0;
    if (ReadFile(f, &dos, sizeof(dos), &rd, NULL) && rd == sizeof(dos) &&
        dos.e_magic == IMAGE_DOS_SIGNATURE &&
        SetFilePointer(f, dos.e_lfanew, NULL, FILE_BEGIN) != INVALID_SET_FILE_POINTER &&
        ReadFile(f, &sig, 4, &rd, NULL) && sig == IMAGE_NT_SIGNATURE &&
        ReadFile(f, &fh, sizeof(fh), &rd, NULL) && rd == sizeof(fh))
        m = fh.Machine;
    CloseHandle(f);
    return m;
}

static int inject(HANDLE proc, const wchar_t *dll)
{
    size_t bytes = (wcslen(dll) + 1) * sizeof(wchar_t);
    void *rp = VirtualAllocEx(proc, NULL, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!rp) {
        fprintf(stderr, "nvinject: VirtualAllocEx failed (%lu)\n", GetLastError());
        return -1;
    }
    if (!WriteProcessMemory(proc, rp, dll, bytes, NULL)) {
        fprintf(stderr, "nvinject: WriteProcessMemory failed (%lu)\n", GetLastError());
        return -1;
    }
    FARPROC ll = GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "LoadLibraryW");
    HANDLE th = CreateRemoteThread(proc, NULL, 0, (LPTHREAD_START_ROUTINE)(void *)ll, rp, 0, NULL);
    if (!th) {
        fprintf(stderr, "nvinject: CreateRemoteThread failed (%lu)\n", GetLastError());
        return -1;
    }
    DWORD w = WaitForSingleObject(th, 60000);
    DWORD code = 0;
    GetExitCodeThread(th, &code);
    CloseHandle(th);
    if (w != WAIT_OBJECT_0) {
        /* LoadLibraryW may still read the path: leave the buffer */
        fprintf(stderr, "nvinject: LoadLibraryW in the target did not finish\n");
        return -1;
    }
    VirtualFreeEx(proc, rp, 0, MEM_RELEASE);
    if (!code) {
        fprintf(stderr, "nvinject: LoadLibraryW(%ls) failed in the target\n", dll);
        return -1;
    }
    return 0;
}

static int run_other(const wchar_t *raw_tail)
{
    wchar_t cmd[32768];
    _snwprintf(cmd, 32767, L"\"%ls\\%ls\" %ls", self_dir, OTHER_INJECT, raw_tail);
    cmd[32767] = 0;
    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    si.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    si.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    if (!CreateProcessW(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
        fprintf(stderr, "nvinject: cannot start %ls (%lu)\n", OTHER_INJECT, GetLastError());
        return 2;
    }
    WaitForSingleObject(pi.hProcess, INFINITE);
    DWORD code = 2;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    return (int)code;
}

/* the raw command line after the program name, for handing over */
static const wchar_t *cmdline_tail(void)
{
    const wchar_t *c = GetCommandLineW();
    if (*c == L'"') {
        c++;
        while (*c && *c != L'"')
            c++;
        if (*c)
            c++;
    } else {
        while (*c && *c != L' ' && *c != L'\t')
            c++;
    }
    while (*c == L' ' || *c == L'\t')
        c++;
    return c;
}

int wmain(int argc0, wchar_t **argv0)
{
    (void)argc0;
    (void)argv0;
    int argc;
    wchar_t **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    wchar_t dll[MAX_PATH], exe[MAX_PATH];
    const wchar_t *dir = NULL;
    int follow = 0, i;
    DWORD timeout = INFINITE;
    unsigned long pid = 0;

    setvbuf(stderr, NULL, _IONBF, 0);
    GetModuleFileNameW(NULL, self_dir, MAX_PATH);
    wchar_t *s = wcsrchr(self_dir, L'\\');
    if (s)
        *s = 0;
    _snwprintf(dll, MAX_PATH - 1, L"%ls\\%ls", self_dir, DLL_NAME);
    dll[MAX_PATH - 1] = 0;

    for (i = 1; i < argc && argv[i][0] == L'-'; i++) {
        if (!wcscmp(argv[i], L"-d") && i + 1 < argc)
            dir = argv[++i];
        else if (!wcscmp(argv[i], L"-f"))
            follow = 1;
        else if (!wcscmp(argv[i], L"-t") && i + 1 < argc)
            timeout = (DWORD)wcstoul(argv[++i], NULL, 10) * 1000;
        else if (!wcscmp(argv[i], L"-p") && i + 1 < argc)
            pid = wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(argv[i], L"--")) {
            i++;
            break;
        } else {
            usage();
            return 2;
        }
    }
    if (GetFileAttributesW(dll) == INVALID_FILE_ATTRIBUTES) {
        fprintf(stderr, "nvinject: %ls not found\n", dll);
        return 2;
    }

    if (pid) {
        HANDLE proc = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION |
                                      PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ,
                                  FALSE, (DWORD)pid);
        if (!proc) {
            fprintf(stderr, "nvinject: OpenProcess(%lu) failed (%lu)\n", pid, GetLastError());
            return 2;
        }
        BOOL wow_target = FALSE, wow_self = FALSE;
        IsWow64Process(proc, &wow_target);
        IsWow64Process(GetCurrentProcess(), &wow_self);
#ifdef _WIN64
        int other = wow_target;
#else
        int other = wow_self && !wow_target;
#endif
        if (other) {
            CloseHandle(proc);
            return run_other(cmdline_tail());
        }
        int rc = inject(proc, dll);
        CloseHandle(proc);
        return rc ? 1 : 0;
    }
    if (i >= argc) {
        usage();
        return 2;
    }

    /* resolve the program the way CreateProcess would (plus .exe) */
    if (!SearchPathW(NULL, argv[i], L".exe", MAX_PATH, exe, NULL)) {
        fprintf(stderr, "nvinject: %ls: not found\n", argv[i]);
        return 2;
    }
    WORD m = exe_machine(exe);
    if (m && m != SELF_MACHINE) {
        if (m != IMAGE_FILE_MACHINE_I386 && m != IMAGE_FILE_MACHINE_AMD64) {
            fprintf(stderr, "nvinject: %ls: unsupported machine 0x%x\n", exe, m);
            return 2;
        }
        return run_other(cmdline_tail());
    }

    if (dir) {
        SetEnvironmentVariableW(L"NVTRACE_DIR", dir);
        CreateDirectoryW(dir, NULL);
    }
    if (follow)
        SetEnvironmentVariableW(L"NVTRACE_FOLLOW", L"1");

    static wchar_t cmd[32768];
    cmd[0] = 0;
    for (int k = i; k < argc; k++)
        append_arg(cmd, 32768, argv[k]);

    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    si.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    si.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    if (!CreateProcessW(exe, cmd, NULL, NULL, TRUE, CREATE_SUSPENDED, NULL, NULL, &si, &pi)) {
        fprintf(stderr, "nvinject: CreateProcess(%ls) failed (%lu)\n", exe, GetLastError());
        return 2;
    }
    if (inject(pi.hProcess, dll)) {
        TerminateProcess(pi.hProcess, 2);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        return 2;
    }
    fprintf(stderr, "nvinject: pid %lu traced\n", pi.dwProcessId);
    ResumeThread(pi.hThread);
    CloseHandle(pi.hThread);
    DWORD w = WaitForSingleObject(pi.hProcess, timeout);
    if (w == WAIT_TIMEOUT) {
        fprintf(stderr, "nvinject: timeout, killing pid %lu\n", pi.dwProcessId);
        TerminateProcess(pi.hProcess, 124);
        WaitForSingleObject(pi.hProcess, 10000);
        CloseHandle(pi.hProcess);
        return 124;
    }
    DWORD code = 1;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hProcess);
    LocalFree(argv);
    return (int)code;
}

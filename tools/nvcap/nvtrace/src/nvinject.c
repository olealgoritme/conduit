/* nvinject: load nvtrace into programs.
 *
 *   nvinject [opts] [-f] [-t sec] <exe> [args...]   start a program with nvtrace
 *   nvinject [opts] -p <pid>[,<pid>...]               load nvtrace into running processes
 *   nvinject [opts] --dwm                             ... into every dwm.exe
 *   nvinject [opts] --watch [--all|name,...] [--dwm]  inject into new processes until stopped
 *   nvinject --install-service [watch opts] / --uninstall-service
 *   nvinject [-d dir] --stop [pid]                    close logs, hooks pass through
 *   nvinject --info <pid>                             what blocks injection into a process
 *   nvinject [-d dir] --selftest                      check hooking and live injection here
 *   opts: -d dir, -c NAME=value (NVTRACE_* setting for the traced processes)
 *
 * The nvtrace DLL (nvtrace64.dll / nvtrace32.dll) must sit next to nvinject.
 * A 32-bit target is handed to nvinject32.exe (and the other way round).
 * Settings reach a process that is already running through
 * <dll dir>\nvtrace-<pid>.cfg, written before and deleted after injection. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#include <sddl.h>
#include <psapi.h>
#include <winternl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#ifdef _WIN64
#define SELF_MACHINE IMAGE_FILE_MACHINE_AMD64
#define DLL_NAME L"nvtrace64.dll"
#define OTHER_INJECT L"nvinject32.exe"
#define ARCH "x64"
#else
#define SELF_MACHINE IMAGE_FILE_MACHINE_I386
#define DLL_NAME L"nvtrace32.dll"
#define OTHER_INJECT L"nvinject.exe"
#define ARCH "x86"
#endif

static wchar_t self_dir[MAX_PATH], self_exe[MAX_PATH], dll_path[MAX_PATH];
static wchar_t log_dir[MAX_PATH] = L"C:\\nvtrace";
static wchar_t extra_cfg[4096];       /* -c NAME=value lines */
static FILE *logf;                     /* service mode: messages go here too */
static volatile LONG stop_flag;

static void msg(const char *fmt, ...)
{
    va_list ap;
    char text[2048];
    va_start(ap, fmt);
    _vsnprintf(text, sizeof(text) - 1, fmt, ap);
    va_end(ap);
    text[sizeof(text) - 1] = 0;
    fprintf(stderr, "%s", text);
    if (logf) {
        SYSTEMTIME st;
        GetLocalTime(&st);
        fprintf(logf, "%02d:%02d:%02d.%03d %s", st.wHour, st.wMinute, st.wSecond, st.wMilliseconds, text);
        fflush(logf);
    }
}

static void usage(void)
{
    fprintf(stderr,
            "usage: nvinject [opts] [-f] [-t seconds] <exe> [args...]\n"
            "       nvinject [opts] -p <pid>[,<pid>...]\n"
            "       nvinject [opts] --dwm\n"
            "       nvinject [opts] --watch [--all | name.exe,...] [--dwm] [--existing]\n"
            "                [--lazy | --full] [--interval ms]\n"
            "       nvinject [opts] --install-service <watch options> | --uninstall-service\n"
            "       nvinject [-d dir] --stop [pid]\n"
            "       nvinject --info <pid>\n"
            "       nvinject [-d dir] --selftest\n"
            "  -d dir        log directory (default C:\\nvtrace)\n"
            "  -c NAME=val   nvtrace setting for the traced processes (NVTRACE_ prefix optional)\n"
            "  -f            follow child processes\n"
            "  -t sec        stop the program after sec seconds (exit code 124)\n"
            "  -p pid        load nvtrace into running processes (needs admin for other users)\n"
            "  --dwm         load nvtrace into every dwm.exe (admin; DWM restarts if it dies)\n"
            "  --watch       inject into new processes: --all (minus a deny list) or the named\n"
            "                ones; --dwm adds dwm.exe; --existing also takes running ones;\n"
            "                --lazy (default with --all) starts tracing only when an NVIDIA\n"
            "                driver DLL loads; stop with Ctrl+C or nvinject --stop-watch\n"
            "  --stop        every traced process (or pid) closes its log; hooks stay, inert\n"
            "  --info pid    architecture, session, mitigations, nvtrace/NVIDIA DLLs loaded\n"
            "  --selftest    hook this process and a busy child live; prints ok/FAIL lines\n");
}

/* ------------------------------------------------------------------ helpers */

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
        if (*p == L'"')
            bs = bs * 2 + 1;
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

static int enable_privilege(const wchar_t *name)
{
    HANDLE tok;
    TOKEN_PRIVILEGES tp;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &tok))
        return 0;
    tp.PrivilegeCount = 1;
    tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    int ok = LookupPrivilegeValueW(NULL, name, &tp.Privileges[0].Luid) &&
             AdjustTokenPrivileges(tok, FALSE, &tp, sizeof(tp), NULL, NULL) &&
             GetLastError() == ERROR_SUCCESS;
    CloseHandle(tok);
    return ok;
}

/* Log directory writable by DWM's virtual account, services, AppContainers
 * and low-integrity processes; the DLLs readable by them. */
static void prepare_access(void)
{
    PSECURITY_DESCRIPTOR sd = NULL;
    /* only a directory nvinject created (now, or earlier: marker file) gets
     * the open ACL; an existing directory keeps its own */
    wchar_t marker[MAX_PATH + 32];
    _snwprintf(marker, MAX_PATH + 31, L"%ls\\.nvtrace-logdir", log_dir);
    marker[MAX_PATH + 31] = 0;
    int created = CreateDirectoryW(log_dir, NULL);
    if (!created && GetFileAttributesW(marker) == INVALID_FILE_ATTRIBUTES) {
        msg("nvinject: %ls exists and was not made by nvinject: ACL left as is; DWM, services and "
            "sandboxed processes may be unable to write logs there (use a new -d directory)\n", log_dir);
    } else {
        HANDLE mf = CreateFileW(marker, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_HIDDEN, NULL);
        if (mf != INVALID_HANDLE_VALUE)
            CloseHandle(mf);
    }
    if ((created || GetFileAttributesW(marker) != INVALID_FILE_ATTRIBUTES) &&
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"D:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;WD)(A;OICI;0x1301bf;;;AC)"
            L"(A;OICI;0x1301bf;;;S-1-15-2-2)S:(ML;OICI;NW;;;LW)",
            SDDL_REVISION_1, &sd, NULL)) {
        if (!SetFileSecurityW(log_dir, DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION, sd) &&
            !SetFileSecurityW(log_dir, DACL_SECURITY_INFORMATION, sd))
            msg("nvinject: could not open up %ls for all processes (%lu)\n", log_dir, GetLastError());
        LocalFree(sd);
    }
    sd = NULL;
    if (ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;WD)(A;;0x1200a9;;;AC)(A;;0x1200a9;;;S-1-15-2-2)",
            SDDL_REVISION_1, &sd, NULL)) {
        const wchar_t *dlls[2] = {L"nvtrace64.dll", L"nvtrace32.dll"};
        for (int i = 0; i < 2; i++) {
            wchar_t p[MAX_PATH];
            _snwprintf(p, MAX_PATH - 1, L"%ls\\%ls", self_dir, dlls[i]);
            p[MAX_PATH - 1] = 0;
            if (GetFileAttributesW(p) != INVALID_FILE_ATTRIBUTES)
                SetFileSecurityW(p, DACL_SECURITY_INFORMATION, sd);
        }
        LocalFree(sd);
    }
}

/* per-process settings file read by the DLL at load */
static int write_cfg(DWORD pid, int lazy)
{
    wchar_t path[MAX_PATH];
    _snwprintf(path, MAX_PATH - 1, L"%ls\\nvtrace-%lu.cfg", self_dir, (unsigned long)pid);
    path[MAX_PATH - 1] = 0;
    FILE *f = _wfopen(path, L"wb");
    if (!f)
        return -1;
    char u8[4 * MAX_PATH];
    WideCharToMultiByte(CP_UTF8, 0, log_dir, -1, u8, sizeof(u8), NULL, NULL);
    fprintf(f, "# written by nvinject for pid %lu; deleted after injection\r\nNVTRACE_DIR=%s\r\n",
            (unsigned long)pid, u8);
    if (lazy >= 0)
        fprintf(f, "NVTRACE_MODE=%s\r\n", lazy ? "lazy" : "full");
    /* this process's NVTRACE_* environment */
    wchar_t *env = GetEnvironmentStringsW();
    for (wchar_t *e = env; e && *e; e += wcslen(e) + 1) {
        if (!_wcsnicmp(e, L"NVTRACE_", 8) && _wcsnicmp(e, L"NVTRACE_DIR=", 12) &&
            (lazy < 0 || _wcsnicmp(e, L"NVTRACE_MODE=", 13))) {
            WideCharToMultiByte(CP_UTF8, 0, e, -1, u8, sizeof(u8), NULL, NULL);
            fprintf(f, "%s\r\n", u8);
        }
    }
    if (env)
        FreeEnvironmentStringsW(env);
    WideCharToMultiByte(CP_UTF8, 0, extra_cfg, -1, u8, sizeof(u8), NULL, NULL);
    fputs(u8, f);
    fclose(f);
    PSECURITY_DESCRIPTOR sd = NULL;
    if (ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;WD)(A;;0x1200a9;;;AC)(A;;0x1200a9;;;S-1-15-2-2)",
            SDDL_REVISION_1, &sd, NULL)) {
        SetFileSecurityW(path, DACL_SECURITY_INFORMATION, sd);
        LocalFree(sd);
    }
    return 0;
}

static void delete_cfg(DWORD pid)
{
    wchar_t path[MAX_PATH];
    _snwprintf(path, MAX_PATH - 1, L"%ls\\nvtrace-%lu.cfg", self_dir, (unsigned long)pid);
    path[MAX_PATH - 1] = 0;
    DeleteFileW(path);
}

static int module_loaded(HANDLE proc, const wchar_t *prefix, wchar_t *found, size_t foundsz)
{
    HMODULE mods[1024];
    DWORD need = 0;
    if (!EnumProcessModulesEx(proc, mods, sizeof(mods), &need, LIST_MODULES_ALL))
        return -1;
    size_t n = need / sizeof(HMODULE);
    if (n > 1024)
        n = 1024;
    size_t pl = wcslen(prefix);
    for (size_t i = 0; i < n; i++) {
        wchar_t name[MAX_PATH];
        if (GetModuleBaseNameW(proc, mods[i], name, MAX_PATH) && !_wcsnicmp(name, prefix, pl)) {
            if (found)
                lstrcpynW(found, name, (int)foundsz);
            return 1;
        }
    }
    return 0;
}

typedef BOOL(WINAPI *GetProcessMitigationPolicy_t)(HANDLE, int, PVOID, SIZE_T);

static DWORD mitigation(HANDLE proc, int policy)
{
    static GetProcessMitigationPolicy_t gpmp;
    DWORD v[2] = {0, 0};
    if (!gpmp)
        gpmp = (GetProcessMitigationPolicy_t)(void *)GetProcAddress(GetModuleHandleW(L"kernel32.dll"),
                                                                     "GetProcessMitigationPolicy");
    if (!gpmp || !gpmp(proc, policy, v, policy == 0 ? 8 : 4))
        return 0;
    return v[0];
}

/* What would make nvtrace fail in this process; NULL if nothing known. */
static const char *injection_blocker(HANDLE proc)
{
    if (mitigation(proc, 8 /* ProcessSignaturePolicy */) & 1)
        return "loads only Microsoft-signed DLLs (signature policy)";
    if (mitigation(proc, 2 /* ProcessDynamicCodePolicy */) & 1)
        return "forbids dynamic code (ACG): hooks cannot be installed";
    return NULL;
}

/* ------------------------------------------------------------------ injection */

/* LoadLibraryW(dll) on a new thread of proc. 0 ok, 1 already there, -1 error */
static int inject_handle(HANDLE proc, DWORD pid, int lazy, int force)
{
    wchar_t found[MAX_PATH];
    if (module_loaded(proc, L"nvtrace", found, MAX_PATH) == 1) {
        msg("nvinject: pid %lu already has %ls\n", (unsigned long)pid, found);
        return 1;
    }
    const char *why = injection_blocker(proc);
    if (why && !force) {
        msg("nvinject: pid %lu %s; not injected (--force tries anyway)\n", (unsigned long)pid, why);
        return -1;
    }
    size_t bytes = (wcslen(dll_path) + 1) * sizeof(wchar_t);
    void *rp = VirtualAllocEx(proc, NULL, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!rp) {
        msg("nvinject: pid %lu: VirtualAllocEx failed (%lu)\n", (unsigned long)pid, GetLastError());
        return -1;
    }
    if (!WriteProcessMemory(proc, rp, dll_path, bytes, NULL)) {
        msg("nvinject: pid %lu: WriteProcessMemory failed (%lu)\n", (unsigned long)pid, GetLastError());
        VirtualFreeEx(proc, rp, 0, MEM_RELEASE);
        return -1;
    }
    int have_cfg = pid && write_cfg(pid, lazy) == 0;
    FARPROC ll = GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "LoadLibraryW");
    HANDLE th = CreateRemoteThread(proc, NULL, 0, (LPTHREAD_START_ROUTINE)(void *)ll, rp, 0, NULL);
    if (!th) {
        msg("nvinject: pid %lu: CreateRemoteThread failed (%lu)\n", (unsigned long)pid, GetLastError());
        if (have_cfg)
            delete_cfg(pid);
        VirtualFreeEx(proc, rp, 0, MEM_RELEASE);
        return -1;
    }
    DWORD w = WaitForSingleObject(th, 30000);
    DWORD code = 0;
    GetExitCodeThread(th, &code);
    CloseHandle(th);
    /* on a timeout LoadLibraryW may still read the settings file: keep it */
    if (have_cfg && w == WAIT_OBJECT_0)
        delete_cfg(pid);
    if (w != WAIT_OBJECT_0) {
        /* the loader lock is held elsewhere; LoadLibraryW still owns the
         * path buffer, so it is left allocated */
        msg("nvinject: pid %lu: LoadLibraryW did not finish within 30 s\n", (unsigned long)pid);
        return -1;
    }
    VirtualFreeEx(proc, rp, 0, MEM_RELEASE);
    if (!code) {
        msg("nvinject: pid %lu: LoadLibraryW(%ls) failed in the target\n", (unsigned long)pid, dll_path);
        return -1;
    }
    return 0;
}

static int run_other(const wchar_t *args, DWORD timeout)
{
    static wchar_t cmd[32768];
    _snwprintf(cmd, 32767, L"\"%ls\\%ls\" %ls", self_dir, OTHER_INJECT, args);
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
        msg("nvinject: cannot start %ls (%lu)\n", OTHER_INJECT, GetLastError());
        return 2;
    }
    if (WaitForSingleObject(pi.hProcess, timeout) == WAIT_TIMEOUT) {
        msg("nvinject: %ls did not finish\n", OTHER_INJECT);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        return 2;
    }
    DWORD code = 2;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    return (int)code;
}

static int is_other_arch(HANDLE proc)
{
    BOOL wow_target = FALSE, wow_self = FALSE;
    IsWow64Process(proc, &wow_target);
    IsWow64Process(GetCurrentProcess(), &wow_self);
#ifdef _WIN64
    return wow_target;
#else
    return wow_self && !wow_target;
#endif
}

/* 0 injected, 1 already traced, -1 failed */
static int inject_pid(DWORD pid, int lazy, int force)
{
    HANDLE proc = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION |
                                  PROCESS_VM_WRITE | PROCESS_VM_READ | SYNCHRONIZE,
                              FALSE, pid);
    if (!proc) {
        DWORD e = GetLastError();
        msg("nvinject: OpenProcess(%lu) failed (%lu)%s\n", (unsigned long)pid, e,
            e == ERROR_ACCESS_DENIED ? ": run as administrator; protected processes cannot be traced" : "");
        return -1;
    }
    if (is_other_arch(proc)) {
        CloseHandle(proc);
        wchar_t a[MAX_PATH + 256];
        a[0] = 0;
        wchar_t num[32];
        append_arg(a, MAX_PATH + 256, L"-d");
        append_arg(a, MAX_PATH + 256, log_dir);
        if (lazy >= 0)
            append_arg(a, MAX_PATH + 256, lazy ? L"--lazy" : L"--full");
        if (force)
            append_arg(a, MAX_PATH + 256, L"--force");
        /* -c settings travel through the environment of the helper */
        for (wchar_t *l = extra_cfg; *l;) {
            wchar_t *e = wcsstr(l, L"\r\n");
            wchar_t line[512];
            size_t n = e ? (size_t)(e - l) : wcslen(l);
            if (n && n < 512) {
                wcsncpy(line, l, n);
                line[n] = 0;
                wchar_t *eq = wcschr(line, L'=');
                if (eq) {
                    *eq = 0;
                    SetEnvironmentVariableW(line, eq + 1);
                }
            }
            if (!e)
                break;
            l = e + 2;
        }
        append_arg(a, MAX_PATH + 256, L"-p");
        _snwprintf(num, 31, L"%lu", (unsigned long)pid);
        num[31] = 0;
        append_arg(a, MAX_PATH + 256, num);
        int rc = run_other(a, 70000);
        return rc == 0 ? 0 : rc == 3 ? 1 : -1;
    }
    int rc = inject_handle(proc, pid, lazy, force);
    CloseHandle(proc);
    if (!rc)
        msg("nvinject: pid %lu traced, log in %ls\n", (unsigned long)pid, log_dir);
    return rc;
}

/* ------------------------------------------------------------------ process list */

typedef LONG(NTAPI *NtQSI_t)(ULONG, PVOID, ULONG, PULONG);

static void *process_list(void)
{
    static NtQSI_t qsi;
    if (!qsi)
        qsi = (NtQSI_t)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtQuerySystemInformation");
    if (!qsi)
        return NULL;
    ULONG size = 1 << 20;
    for (int tries = 0; tries < 6; tries++) {
        void *p = VirtualAlloc(NULL, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
        if (!p)
            return NULL;
        ULONG need = 0;
        LONG st = qsi(5, p, size, &need);
        if (st >= 0)
            return p;
        VirtualFree(p, 0, MEM_RELEASE);
        if (st != (LONG)0xC0000004)
            return NULL;
        size = (need > size ? need : size) * 2;
    }
    return NULL;
}

typedef struct pentry {
    DWORD pid;
    DWORD session;
    uint64_t create;
    wchar_t name[64];
} pentry;

/* fills out[] from a SystemProcessInformation buffer; returns count */
static int parse_processes(void *pl, pentry *out, int max)
{
    uint8_t *p = pl;
    int n = 0;
    for (; n < max;) {
        SYSTEM_PROCESS_INFORMATION *pi = (SYSTEM_PROCESS_INFORMATION *)p;
        pentry *e = &out[n];
        e->pid = (DWORD)(uintptr_t)pi->UniqueProcessId;
        memcpy(&e->create, p + 0x20, 8);   /* CreateTime, inside Reserved1 */
        e->session = 0;
        memcpy(&e->session, (uint8_t *)&pi->HandleCount + 4, 4);
        size_t len = pi->ImageName.Buffer ? pi->ImageName.Length / 2 : 0;
        if (len > 63)
            len = 63;
        if (len)
            memcpy(e->name, pi->ImageName.Buffer, len * 2);
        e->name[len] = 0;
        if (e->pid)
            n++;
        if (!pi->NextEntryOffset)
            break;
        p += pi->NextEntryOffset;
    }
    return n;
}

static int name_in_list(const wchar_t *list, const wchar_t *name)
{
    size_t nl = wcslen(name);
    for (const wchar_t *e = list; e && *e;) {
        while (*e == L',' || *e == L' ')
            e++;
        const wchar_t *c = wcschr(e, L',');
        size_t len = c ? (size_t)(c - e) : wcslen(e);
        if (len && ((len == nl && !_wcsnicmp(e, name, len)) ||
                    (len + 4 == nl && !_wcsnicmp(e, name, len) && !_wcsicmp(name + len, L".exe"))))
            return 1;
        if (!c)
            break;
        e = c + 1;
    }
    return 0;
}

/* never touched, not even when named */
static const wchar_t *hard_deny =
    L"System,Registry,Idle,smss.exe,csrss.exe,wininit.exe,winlogon.exe,services.exe,lsass.exe,"
    L"lsaiso.exe,MemCompression,secure system,fontdrvhost.exe,nvinject.exe,nvinject32.exe,"
    L"LogonUI.exe,consent.exe,userinit.exe,WUDFHost.exe,WerFault.exe,WerFaultSecure.exe,"
    L"CredentialUIBroker.exe,LockApp.exe,smartscreen.exe";
/* skipped by --all unless named: security software, service hosts, and games
 * with anti-cheat (injecting into those can get an account banned) */
static const wchar_t *soft_deny =
    L"svchost.exe,MsMpEng.exe,NisSrv.exe,MsSense.exe,SecurityHealthService.exe,"
    L"SecurityHealthSystray.exe,smartscreen.exe,audiodg.exe,conhost.exe,dllhost.exe,"
    L"sihost.exe,ctfmon.exe,WmiPrvSE.exe,TrustedInstaller.exe,TiWorker.exe,spoolsv.exe,"
    L"cs2.exe,csgo.exe,steam.exe,steamwebhelper.exe,steamservice.exe,gameoverlayui.exe,"
    L"EasyAntiCheat.exe,EasyAntiCheat_EOS.exe,EasyAntiCheat_Setup.exe,BEService.exe,"
    L"BEService_x64.exe,vgc.exe,vgtray.exe,FACEIT.exe,faceitservice.exe,RiotClientServices.exe,"
    L"VALORANT-Win64-Shipping.exe,EpicGamesLauncher.exe,Battle.net.exe,Agent.exe,"
    L"mhyprot2.exe,ACE-Guard-Client.exe,xigncode.exe,nProtect.exe,GameGuard.des";

/* ------------------------------------------------------------------ watch */

typedef struct watch_opts {
    int all, dwm, existing, lazy, force;   /* lazy: -1 default */
    DWORD interval;
    wchar_t names[2048];
} watch_opts;

typedef struct seen {
    DWORD pid;
    uint64_t create;
    HANDLE h;              /* injected: kept to see how it ends */
    ULONGLONG injected_at;
    wchar_t name[64];
} seen;

#define MAX_SEEN 16384
typedef struct crashrec {
    wchar_t name[64];
    int crashes;
    int banned;
} crashrec;

static crashrec crashes[256];
static int ncrashes;

static crashrec *crash_of(const wchar_t *name)
{
    for (int i = 0; i < ncrashes; i++)
        if (!_wcsicmp(crashes[i].name, name))
            return &crashes[i];
    if (ncrashes == 256)
        return NULL;
    lstrcpynW(crashes[ncrashes].name, name, 64);
    return &crashes[ncrashes++];
}

static int watch_wants(const watch_opts *o, const wchar_t *name)
{
    if (!name[0] || name_in_list(hard_deny, name))
        return 0;
    if (name_in_list(o->names, name))
        return 1;
    if (o->dwm && !_wcsicmp(name, L"dwm.exe"))
        return 1;
    if (o->all && !name_in_list(soft_deny, name))
        return 1;
    return 0;
}

static BOOL WINAPI ctrl_handler(DWORD type)
{
    (void)type;
    InterlockedExchange(&stop_flag, 1);
    return TRUE;
}

static int watch_stop_file(void)
{
    wchar_t p[MAX_PATH + 32];
    _snwprintf(p, MAX_PATH + 31, L"%ls\\nvinject-watch.stop", log_dir);
    p[MAX_PATH + 31] = 0;
    if (GetFileAttributesW(p) != INVALID_FILE_ATTRIBUTES) {
        DeleteFileW(p);
        return 1;
    }
    return 0;
}

static int watch_loop(const watch_opts *o)
{
    seen *tab = calloc(MAX_SEEN, sizeof(seen));
    pentry *pe = calloc(MAX_SEEN, sizeof(pentry));
    int ntab = 0, first = 1, injected = 0;
    if (!tab || !pe)
        return 2;
    msg("nvinject: watching (%s%s%ls), interval %lu ms, log %ls\n", o->all ? "all processes" : "",
        o->dwm ? " dwm.exe " : " ", o->names, (unsigned long)o->interval, log_dir);
    while (!stop_flag) {
        void *pl = process_list();
        int np = pl ? parse_processes(pl, pe, MAX_SEEN) : 0;
        if (pl)
            VirtualFree(pl, 0, MEM_RELEASE);
        /* ends of injected processes: crash detection */
        for (int i = 0; i < ntab; i++) {
            if (!tab[i].h || WaitForSingleObject(tab[i].h, 0) != WAIT_OBJECT_0)
                continue;
            DWORD code = 0;
            GetExitCodeProcess(tab[i].h, &code);
            CloseHandle(tab[i].h);
            tab[i].h = NULL;
            ULONGLONG lived = GetTickCount64() - tab[i].injected_at;
            int crashed = code >= 0xC0000000u && code != 0xC000013Au /* Ctrl+C */;
            msg("nvinject: pid %lu %ls ended after %.1f s, exit 0x%lx%s\n", (unsigned long)tab[i].pid,
                tab[i].name, lived / 1000.0, (unsigned long)code, crashed ? " (crash)" : "");
            if (crashed && lived < 120000) {
                crashrec *c = crash_of(tab[i].name);
                if (c && ++c->crashes >= 2 && !c->banned) {
                    c->banned = 1;
                    msg("nvinject: %ls crashed twice soon after injection: no longer injected\n", c->name);
                }
            }
        }
        /* new processes */
        for (int k = 0; k < np && !stop_flag; k++) {
            int known = -1;
            for (int i = 0; i < ntab; i++)
                if (tab[i].pid == pe[k].pid) {
                    known = i;
                    break;
                }
            if (known >= 0 && tab[known].create == pe[k].create)
                continue;
            int slot = known;
            if (slot < 0) {
                if (ntab == MAX_SEEN) {
                    /* drop entries of processes that are gone */
                    int w = 0;
                    for (int i = 0; i < ntab; i++) {
                        int alive = 0;
                        for (int j = 0; j < np; j++)
                            if (pe[j].pid == tab[i].pid) {
                                alive = 1;
                                break;
                            }
                        if (alive || tab[i].h)
                            tab[w++] = tab[i];
                    }
                    ntab = w;
                    if (ntab == MAX_SEEN)
                        continue;
                }
                slot = ntab++;
            } else if (tab[slot].h) {
                CloseHandle(tab[slot].h);
            }
            memset(&tab[slot], 0, sizeof(seen));
            tab[slot].pid = pe[k].pid;
            tab[slot].create = pe[k].create;
            lstrcpynW(tab[slot].name, pe[k].name, 64);
            if (first && !o->existing && !(o->dwm && !_wcsicmp(pe[k].name, L"dwm.exe")))
                continue;
            if (pe[k].pid == GetCurrentProcessId() || !watch_wants(o, pe[k].name))
                continue;
            crashrec *c = crash_of(pe[k].name);
            if (c && c->banned)
                continue;
            int lazy = o->lazy;
            if (lazy < 0)
                lazy = (o->all && !name_in_list(o->names, pe[k].name) && _wcsicmp(pe[k].name, L"dwm.exe")) ? 1 : 0;
            int rc = inject_pid(pe[k].pid, lazy, o->force);
            if (rc == 0) {
                injected++;
                tab[slot].injected_at = GetTickCount64();
                tab[slot].h = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pe[k].pid);
                msg("nvinject: + %ls pid %lu session %lu (%s)\n", pe[k].name, (unsigned long)pe[k].pid,
                    (unsigned long)pe[k].session, lazy ? "lazy" : "full");
            }
        }
        first = 0;
        for (DWORD slept = 0; slept < o->interval && !stop_flag; slept += 10)
            Sleep(10);
        static ULONGLONG next_check;
        if (GetTickCount64() >= next_check) {
            next_check = GetTickCount64() + 1000;
            if (watch_stop_file())
                InterlockedExchange(&stop_flag, 1);
        }
    }
    for (int i = 0; i < ntab; i++)
        if (tab[i].h)
            CloseHandle(tab[i].h);
    free(tab);
    free(pe);
    msg("nvinject: watch stopped, %d processes injected\n", injected);
    return 0;
}

/* ------------------------------------------------------------------ service */

#define SVC_NAME L"nvtracewatch"
static SERVICE_STATUS_HANDLE svc_handle;
static SERVICE_STATUS svc_status;
static watch_opts svc_opts;

static DWORD WINAPI svc_ctrl(DWORD ctl, DWORD type, LPVOID data, LPVOID ctx)
{
    (void)type;
    (void)data;
    (void)ctx;
    if (ctl == SERVICE_CONTROL_STOP || ctl == SERVICE_CONTROL_SHUTDOWN) {
        InterlockedExchange(&stop_flag, 1);
        svc_status.dwCurrentState = SERVICE_STOP_PENDING;
        SetServiceStatus(svc_handle, &svc_status);
    }
    return NO_ERROR;
}

static void WINAPI svc_main(DWORD argc, LPWSTR *argv)
{
    (void)argc;
    (void)argv;
    svc_handle = RegisterServiceCtrlHandlerExW(SVC_NAME, svc_ctrl, NULL);
    if (!svc_handle)
        return;
    memset(&svc_status, 0, sizeof(svc_status));
    svc_status.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
    svc_status.dwCurrentState = SERVICE_RUNNING;
    svc_status.dwControlsAccepted = SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN;
    SetServiceStatus(svc_handle, &svc_status);
    watch_loop(&svc_opts);
    svc_status.dwCurrentState = SERVICE_STOPPED;
    SetServiceStatus(svc_handle, &svc_status);
}

static int install_service(const wchar_t *args)
{
    static wchar_t bin[32768];
    _snwprintf(bin, 32767, L"\"%ls\" --service %ls", self_exe, args);
    bin[32767] = 0;
    SC_HANDLE scm = OpenSCManagerW(NULL, NULL, SC_MANAGER_CREATE_SERVICE);
    if (!scm) {
        msg("nvinject: OpenSCManager failed (%lu): run as administrator\n", GetLastError());
        return 2;
    }
    SC_HANDLE s = CreateServiceW(scm, SVC_NAME, L"nvtrace watcher", SERVICE_ALL_ACCESS,
                                 SERVICE_WIN32_OWN_PROCESS, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL,
                                 bin, NULL, NULL, NULL, NULL, NULL);
    if (!s && GetLastError() == ERROR_SERVICE_EXISTS) {
        s = OpenServiceW(scm, SVC_NAME, SERVICE_ALL_ACCESS);
        if (s)
            ChangeServiceConfigW(s, SERVICE_NO_CHANGE, SERVICE_NO_CHANGE, SERVICE_NO_CHANGE, bin, NULL,
                                 NULL, NULL, NULL, NULL, NULL);
    }
    if (!s) {
        msg("nvinject: CreateService failed (%lu)\n", GetLastError());
        CloseServiceHandle(scm);
        return 2;
    }
    int rc = 0;
    if (!StartServiceW(s, 0, NULL) && GetLastError() != ERROR_SERVICE_ALREADY_RUNNING) {
        msg("nvinject: StartService failed (%lu)\n", GetLastError());
        rc = 2;
    } else {
        msg("nvinject: service %ls running as LocalSystem: %ls\n", SVC_NAME, bin);
        msg("nvinject: messages in %ls\\nvinject-watch.log; stop: nvinject --uninstall-service\n", self_dir);
    }
    CloseServiceHandle(s);
    CloseServiceHandle(scm);
    return rc;
}

static int uninstall_service(void)
{
    SC_HANDLE scm = OpenSCManagerW(NULL, NULL, SC_MANAGER_CONNECT);
    if (!scm) {
        msg("nvinject: OpenSCManager failed (%lu)\n", GetLastError());
        return 2;
    }
    SC_HANDLE s = OpenServiceW(scm, SVC_NAME, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE);
    if (!s) {
        msg("nvinject: service %ls not installed\n", SVC_NAME);
        CloseServiceHandle(scm);
        return 0;
    }
    SERVICE_STATUS st;
    ControlService(s, SERVICE_CONTROL_STOP, &st);
    for (int i = 0; i < 100 && QueryServiceStatus(s, &st) && st.dwCurrentState != SERVICE_STOPPED; i++)
        Sleep(100);
    int rc = DeleteService(s) ? 0 : 2;
    msg("nvinject: service %ls %s\n", SVC_NAME, rc ? "could not be deleted" : "stopped and removed");
    CloseServiceHandle(s);
    CloseServiceHandle(scm);
    return rc;
}

/* ------------------------------------------------------------------ stop / info */

static int write_stop(DWORD pid)
{
    wchar_t p[MAX_PATH + 40];
    if (pid)
        _snwprintf(p, MAX_PATH + 39, L"%ls\\nvtrace-%lu.stop", log_dir, (unsigned long)pid);
    else
        _snwprintf(p, MAX_PATH + 39, L"%ls\\nvtrace.stop", log_dir);
    p[MAX_PATH + 39] = 0;
    HANDLE f = CreateFileW(p, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_DELETE, NULL, CREATE_ALWAYS,
                           FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) {
        msg("nvinject: cannot write %ls (%lu)\n", p, GetLastError());
        return -1;
    }
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    SetFileTime(f, NULL, NULL, &ft);
    CloseHandle(f);
    return 0;
}

static void remove_stop(DWORD pid)
{
    wchar_t p[MAX_PATH + 40];
    if (pid)
        _snwprintf(p, MAX_PATH + 39, L"%ls\\nvtrace-%lu.stop", log_dir, (unsigned long)pid);
    else
        _snwprintf(p, MAX_PATH + 39, L"%ls\\nvtrace.stop", log_dir);
    p[MAX_PATH + 39] = 0;
    DeleteFileW(p);
}

static int do_stop(DWORD pid)
{
    if (write_stop(pid))
        return 2;
    /* the DLLs poll every 250 ms */
    Sleep(1500);
    remove_stop(pid);
    msg("nvinject: stop sent to %s; logs are closed, the hooks stay and pass through\n",
        pid ? "the process" : "every traced process");
    return 0;
}

static int do_info(DWORD pid)
{
    HANDLE proc = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, FALSE, pid);
    if (!proc)
        proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (!proc) {
        printf("pid %lu: OpenProcess failed (%lu): not running, or protected/elevated (run as admin)\n",
               (unsigned long)pid, GetLastError());
        return 2;
    }
    wchar_t path[MAX_PATH];
    DWORD n = MAX_PATH;
    if (!QueryFullProcessImageNameW(proc, 0, path, &n))
        lstrcpyW(path, L"?");
    BOOL wow = FALSE;
    IsWow64Process(proc, &wow);
    DWORD sess = 0;
    ProcessIdToSessionId(pid, &sess);
    printf("pid       %lu\nimage     %ls\narch      %s\nsession   %lu\n", (unsigned long)pid, path,
           wow ? "x86 (WOW64): uses nvinject32 / nvtrace32" : "native", (unsigned long)sess);
    HANDLE tok;
    if (OpenProcessToken(proc, TOKEN_QUERY, &tok)) {
        uint8_t buf[256];
        DWORD len;
        if (GetTokenInformation(tok, TokenIntegrityLevel, buf, sizeof(buf), &len)) {
            TOKEN_MANDATORY_LABEL *tml = (TOKEN_MANDATORY_LABEL *)buf;
            DWORD rid = *GetSidSubAuthority(tml->Label.Sid, *GetSidSubAuthorityCount(tml->Label.Sid) - 1);
            printf("integrity 0x%lx%s\n", (unsigned long)rid,
                   rid >= 0x4000 ? " (system)" : rid >= 0x3000 ? " (high)" : rid >= 0x2000 ? " (medium)" : " (low)");
        }
        DWORD isapp = 0;
        if (GetTokenInformation(tok, TokenIsAppContainer, &isapp, sizeof(isapp), &len) && isapp)
            printf("container AppContainer: needs ALL APPLICATION PACKAGES access to the DLL and log dir\n");
        CloseHandle(tok);
    } else {
        printf("integrity (token not readable: %lu)\n", GetLastError());
    }
    DWORD dep[2] = {0, 0};
    (void)dep;
    printf("mitigations:\n");
    printf("  dynamic code (ACG)        %s\n", (mitigation(proc, 2) & 1) ? "PROHIBITED: hooks cannot be installed" : "allowed");
    printf("  signature policy          %s\n", (mitigation(proc, 8) & 1) ? "MICROSOFT-SIGNED ONLY: nvtrace cannot load" : "any");
    DWORD il = mitigation(proc, 10);
    printf("  image load                %s%s%s\n", il & 1 ? "no-remote-images " : "",
           il & 2 ? "no-low-label-images " : "", il ? "" : "unrestricted");
    printf("  strict handle checks      %s\n", (mitigation(proc, 3) & 1) ? "on" : "off");
    printf("  win32k syscalls           %s\n", (mitigation(proc, 4) & 1) ? "DISABLED (no D3DKMT here)" : "allowed");
    wchar_t found[MAX_PATH];
    int r = module_loaded(proc, L"nvtrace", found, MAX_PATH);
    printf("nvtrace   %s%ls\n", r == 1 ? "loaded: " : r == 0 ? "not loaded" : "module list not readable",
           r == 1 ? found : L"");
    HMODULE mods[1024];
    DWORD need = 0;
    if (EnumProcessModulesEx(proc, mods, sizeof(mods), &need, LIST_MODULES_ALL)) {
        printf("NVIDIA    ");
        int any = 0;
        for (size_t i = 0; i < need / sizeof(HMODULE) && i < 1024; i++) {
            wchar_t name[MAX_PATH];
            if (GetModuleBaseNameW(proc, mods[i], name, MAX_PATH) && !_wcsnicmp(name, L"nv", 2) &&
                _wcsnicmp(name, L"nvtrace", 7)) {
                printf("%s%ls", any ? ", " : "", name);
                any = 1;
            }
        }
        printf("%s\n", any ? "" : "(none loaded)");
    }
    const char *why = injection_blocker(proc);
    printf("verdict   %s%s\n", why ? "cannot trace: " : "can be traced", why ? why : "");
    CloseHandle(proc);
    return why ? 1 : 0;
}

/* ------------------------------------------------------------------ busy child */

typedef LONG(APIENTRY *PFN_KMT)(void *);

static volatile LONG busy_stop;

static DWORD WINAPI busy_thread(LPVOID arg)
{
    (void)arg;
    HMODULE gdi = LoadLibraryW(L"gdi32.dll");
    PFN_KMT enum2 = gdi ? (PFN_KMT)(void *)GetProcAddress(gdi, "D3DKMTEnumAdapters2") : NULL;
    unsigned it = 0;
    while (!busy_stop) {
        HANDLE e = CreateEventW(NULL, FALSE, FALSE, NULL);
        if (e)
            CloseHandle(e);
        if (enum2 && (it & 15) == 0) {
            struct {
                ULONG n;
                void *p;
            } q = {0, NULL};
            enum2(&q);
        }
        if ((it & 255) == 0) {
            HANDLE d = CreateFileW(L"\\\\.\\NvAdminDevice", 0, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                                   OPEN_EXISTING, 0, NULL);
            if (d != INVALID_HANDLE_VALUE)
                CloseHandle(d);
        }
        it++;
    }
    return 0;
}

/* nvinject --busy ms: 8 threads calling hooked functions in a loop; the
 * live-injection target of --selftest. Exit 0 = nothing went wrong. */
static int do_busy(DWORD ms)
{
    HANDLE th[8];
    int n = 0;
    for (int i = 0; i < 8; i++)
        if ((th[n] = CreateThread(NULL, 0, busy_thread, NULL, 0, NULL)))
            n++;
    Sleep(ms);
    InterlockedExchange(&busy_stop, 1);
    WaitForMultipleObjects(n, th, TRUE, 10000);
    return n == 8 ? 0 : 3;
}

/* ------------------------------------------------------------------ selftest */

/* count CALL records in a log (format v1/v2) */
static long count_calls(const wchar_t *path, long *records, int *has_end)
{
    *records = 0;
    *has_end = 0;
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL,
                           OPEN_EXISTING, 0, NULL);
    if (f == INVALID_HANDLE_VALUE)
        return -1;
    LARGE_INTEGER sz;
    GetFileSizeEx(f, &sz);
    if (sz.QuadPart > (1 << 30) || sz.QuadPart < 32) {
        CloseHandle(f);
        return -1;
    }
    uint8_t *d = malloc((size_t)sz.QuadPart);
    DWORD rd = 0;
    long calls = 0;
    if (d && ReadFile(f, d, (DWORD)sz.QuadPart, &rd, NULL) && rd == sz.QuadPart && !memcmp(d, "NVTRACE", 7)) {
        uint32_t hs;
        memcpy(&hs, d + 12, 4);
        for (size_t off = hs; off + 8 <= rd;) {
            uint32_t type, size;
            memcpy(&type, d + off, 4);
            memcpy(&size, d + off + 4, 4);
            if (size < 8 || (size & 7) || off + size > rd)
                break;
            if (type)
                (*records)++;
            if (type == 6)
                calls++;
            if (type == 8)
                *has_end = 1;
            off += size;
        }
    } else {
        calls = -1;
    }
    free(d);
    CloseHandle(f);
    return calls;
}

typedef int(__stdcall *selftest_t)(char *, unsigned);

static int selftest(void)
{
    int fails = 0;
    printf("nvinject selftest (%s), log dir %ls\n", ARCH, log_dir);
    CreateDirectoryW(log_dir, NULL);

    /* 1. live injection into a busy child (8 threads in hooked calls) */
    wchar_t cmd[MAX_PATH + 64];
    _snwprintf(cmd, MAX_PATH + 63, L"\"%ls\" --busy 2500", self_exe);
    cmd[MAX_PATH + 63] = 0;
    STARTUPINFOW si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    printf("live injection into a busy process:\n");
    if (CreateProcessW(NULL, cmd, NULL, NULL, FALSE, CREATE_NO_WINDOW, NULL, NULL, &si, &pi)) {
        Sleep(400);
        int rc = inject_pid(pi.dwProcessId, 0, 0);
        printf("  %s  injected into running pid %lu\n", rc == 0 ? "ok  " : "FAIL", (unsigned long)pi.dwProcessId);
        fails += rc != 0;
        DWORD w = WaitForSingleObject(pi.hProcess, 20000);
        DWORD code = 99;
        GetExitCodeProcess(pi.hProcess, &code);
        printf("  %s  child kept running and exited normally (exit 0x%lx%s)\n",
               w == WAIT_OBJECT_0 && code == 0 ? "ok  " : "FAIL", (unsigned long)code,
               w == WAIT_OBJECT_0 ? "" : ", hung");
        fails += !(w == WAIT_OBJECT_0 && code == 0);
        if (w != WAIT_OBJECT_0)
            TerminateProcess(pi.hProcess, 1);
        wchar_t lp[MAX_PATH + 64];
        _snwprintf(lp, MAX_PATH + 63, L"%ls\\%ls-%lu.ntr", log_dir,
#ifdef _WIN64
                   L"nvinject",
#else
                   L"nvinject32",
#endif
                   (unsigned long)pi.dwProcessId);
        lp[MAX_PATH + 63] = 0;
        long recs = 0;
        int has_end = 0;
        long calls = count_calls(lp, &recs, &has_end);
        printf("  %s  child log %ls: %ld records, %ld calls, %s\n", calls > 0 && has_end ? "ok  " : "FAIL",
               lp, recs, calls, has_end ? "END record present" : "no END record");
        fails += !(calls > 0 && has_end);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    } else {
        printf("  FAIL  cannot start busy child (%lu)\n", GetLastError());
        fails++;
    }

    /* 2. hooks of this process, D3DKMT logging, live patch under load */
    printf("nvtrace in this process:\n");
    SetEnvironmentVariableW(L"NVTRACE_DIR", log_dir);
    SetEnvironmentVariableW(L"NVTRACE_MODE", L"full");
    SetEnvironmentVariableW(L"NVTRACE_SELFTEST", L"1");
    HMODULE m = LoadLibraryW(dll_path);
    selftest_t st = m ? (selftest_t)(void *)GetProcAddress(m, "nvtrace_selftest") : NULL;
    if (!st) {
        printf("  FAIL  cannot load %ls (%lu)\n", dll_path, GetLastError());
        fails++;
    } else {
        static char rep[16384];
        int f = st(rep, sizeof(rep));
        fputs(rep, stdout);
        fails += f;
    }

#ifdef _WIN64
    /* 3. the 32-bit side */
    wchar_t other[MAX_PATH];
    _snwprintf(other, MAX_PATH - 1, L"%ls\\%ls", self_dir, OTHER_INJECT);
    other[MAX_PATH - 1] = 0;
    if (GetFileAttributesW(other) != INVALID_FILE_ATTRIBUTES) {
        fflush(stdout);
        wchar_t a[MAX_PATH + 32] = L"";
        append_arg(a, MAX_PATH + 32, L"-d");
        append_arg(a, MAX_PATH + 32, log_dir);
        append_arg(a, MAX_PATH + 32, L"--selftest");
        int rc = run_other(a, 120000);
        fails += rc != 0;
    } else {
        printf("(nvinject32.exe not found: 32-bit side not tested)\n");
    }
#endif
    printf("%s (%s): %d failed checks\n", fails ? "SELFTEST FAIL" : "SELFTEST PASS", ARCH, fails);
    fflush(stdout);
    return fails ? 1 : 0;
}

/* ------------------------------------------------------------------ main */

static const wchar_t *after_options(int argc, wchar_t **argv, int i)
{
    /* raw tail of the command line from argv[i] on, for the service */
    static wchar_t buf[16384];
    buf[0] = 0;
    for (int k = i; k < argc; k++)
        append_arg(buf, 16384, argv[k]);
    return buf;
}

int wmain(int argc0, wchar_t **argv0)
{
    (void)argc0;
    (void)argv0;
    int argc;
    wchar_t **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    wchar_t exe[MAX_PATH];
    int follow = 0, i, dwm = 0, watch = 0, force = 0, lazy = -1, do_selftest_flag = 0;
    int stop = 0, stop_watch = 0, install = 0, uninstall = 0, service = 0;
    DWORD timeout = INFINITE, info_pid = 0, busy_ms = 0;
    wchar_t pids[1024] = L"";
    watch_opts wo;
    memset(&wo, 0, sizeof(wo));
    wo.lazy = -1;
    wo.interval = 20;

    setvbuf(stderr, NULL, _IONBF, 0);
    GetModuleFileNameW(NULL, self_exe, MAX_PATH);
    self_exe[MAX_PATH - 1] = 0;
    lstrcpynW(self_dir, self_exe, MAX_PATH);
    wchar_t *s = wcsrchr(self_dir, L'\\');
    if (s)
        *s = 0;
    _snwprintf(dll_path, MAX_PATH - 1, L"%ls\\%ls", self_dir, DLL_NAME);
    dll_path[MAX_PATH - 1] = 0;
    wchar_t envdir[MAX_PATH];
    DWORD en = GetEnvironmentVariableW(L"NVTRACE_DIR", envdir, MAX_PATH);
    if (en && en < MAX_PATH)
        lstrcpynW(log_dir, envdir, MAX_PATH);

    int service_args_from = 0;
    for (i = 1; i < argc && argv[i][0] == L'-'; i++) {
        const wchar_t *a = argv[i];
        if (!wcscmp(a, L"-d") && i + 1 < argc) {
            wchar_t full[MAX_PATH];
            if (GetFullPathNameW(argv[++i], MAX_PATH, full, NULL))
                lstrcpynW(log_dir, full, MAX_PATH);
        } else if (!wcscmp(a, L"-c") && i + 1 < argc) {
            const wchar_t *kvp = argv[++i];
            if (!wcschr(kvp, L'=')) {
                usage();
                return 2;
            }
            if (_wcsnicmp(kvp, L"NVTRACE_", 8))
                wcsncat(extra_cfg, L"NVTRACE_", 4095 - wcslen(extra_cfg));
            wcsncat(extra_cfg, kvp, 4095 - wcslen(extra_cfg));
            wcsncat(extra_cfg, L"\r\n", 4095 - wcslen(extra_cfg));
        } else if (!wcscmp(a, L"-f"))
            follow = 1;
        else if (!wcscmp(a, L"-t") && i + 1 < argc)
            timeout = (DWORD)wcstoul(argv[++i], NULL, 10) * 1000;
        else if (!wcscmp(a, L"-p") && i + 1 < argc)
            lstrcpynW(pids, argv[++i], 1024);
        else if (!wcscmp(a, L"--dwm"))
            dwm = wo.dwm = 1;
        else if (!wcscmp(a, L"--watch")) {
            watch = 1;
            if (!service_args_from)
                service_args_from = i;
            if (i + 1 < argc && argv[i + 1][0] != L'-')
                lstrcpynW(wo.names, argv[++i], 2048);
        } else if (!wcscmp(a, L"--all"))
            wo.all = 1;
        else if (!wcscmp(a, L"--existing"))
            wo.existing = 1;
        else if (!wcscmp(a, L"--lazy"))
            lazy = wo.lazy = 1;
        else if (!wcscmp(a, L"--full"))
            lazy = wo.lazy = 0;
        else if (!wcscmp(a, L"--force"))
            force = wo.force = 1;
        else if (!wcscmp(a, L"--interval") && i + 1 < argc)
            wo.interval = (DWORD)wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(a, L"--stop")) {
            stop = 1;
            if (i + 1 < argc && argv[i + 1][0] != L'-')
                info_pid = (DWORD)wcstoul(argv[++i], NULL, 10);
        } else if (!wcscmp(a, L"--stop-watch"))
            stop_watch = 1;
        else if (!wcscmp(a, L"--info") && i + 1 < argc)
            info_pid = (DWORD)wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(a, L"--selftest"))
            do_selftest_flag = 1;
        else if (!wcscmp(a, L"--busy") && i + 1 < argc)
            busy_ms = (DWORD)wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(a, L"--install-service")) {
            install = 1;
            service_args_from = i + 1;
        } else if (!wcscmp(a, L"--uninstall-service"))
            uninstall = 1;
        else if (!wcscmp(a, L"--service"))
            service = watch = 1;
        else if (!wcscmp(a, L"--")) {
            i++;
            break;
        } else {
            usage();
            return 2;
        }
    }
    if (wo.interval < 5)
        wo.interval = 5;

    if (busy_ms)
        return do_busy(busy_ms);
    if (uninstall)
        return uninstall_service();
    if (install) {
        enable_privilege(L"SeDebugPrivilege");
        prepare_access();
        static wchar_t sargs[16384];
        sargs[0] = 0;
        append_arg(sargs, 16384, L"-d");
        append_arg(sargs, 16384, log_dir);
        for (wchar_t *l = extra_cfg; *l;) {
            wchar_t *e = wcsstr(l, L"\r\n");
            wchar_t line[512];
            size_t n = e ? (size_t)(e - l) : wcslen(l);
            if (n && n < 512) {
                wcsncpy(line, l, n);
                line[n] = 0;
                append_arg(sargs, 16384, L"-c");
                append_arg(sargs, 16384, line);
            }
            if (!e)
                break;
            l = e + 2;
        }
        wcsncat(sargs, L" ", 16383 - wcslen(sargs));
        wcsncat(sargs, after_options(argc, argv, service_args_from), 16383 - wcslen(sargs));
        if (!wcsstr(sargs, L"--watch"))
            wcsncat(sargs, L" --watch", 16383 - wcslen(sargs));
        return install_service(sargs);
    }
    if (GetFileAttributesW(dll_path) == INVALID_FILE_ATTRIBUTES && !stop && !stop_watch && !info_pid) {
        msg("nvinject: %ls not found\n", dll_path);
        return 2;
    }
    if (stop_watch) {
        wchar_t p[MAX_PATH + 32];
        _snwprintf(p, MAX_PATH + 31, L"%ls\\nvinject-watch.stop", log_dir);
        p[MAX_PATH + 31] = 0;
        HANDLE f = CreateFileW(p, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, 0, NULL);
        if (f != INVALID_HANDLE_VALUE)
            CloseHandle(f);
        msg("nvinject: watchers stop within a second\n");
        return f == INVALID_HANDLE_VALUE ? 2 : 0;
    }
    if (stop)
        return do_stop(info_pid);
    if (info_pid) {
        enable_privilege(L"SeDebugPrivilege");
        return do_info(info_pid);
    }
    if (do_selftest_flag)
        return selftest();

    if (service) {
        wchar_t lp[MAX_PATH + 32];
        CreateDirectoryW(log_dir, NULL);
        /* next to nvinject (admin-owned), not in the world-writable log dir */
        _snwprintf(lp, MAX_PATH + 31, L"%ls\\nvinject-watch.log", self_dir);
        lp[MAX_PATH + 31] = 0;
        logf = _wfopen(lp, L"a");
        enable_privilege(L"SeDebugPrivilege");
        prepare_access();
        svc_opts = wo;
        SERVICE_TABLE_ENTRYW tab[2] = {{(LPWSTR)SVC_NAME, svc_main}, {NULL, NULL}};
        if (!StartServiceCtrlDispatcherW(tab)) {
            msg("nvinject: --service is for the service manager (%lu); use --watch\n", GetLastError());
            return 2;
        }
        return 0;
    }
    if (watch) {
        if (!wo.all && !wo.dwm && !wo.names[0]) {
            msg("nvinject: --watch needs --all, --dwm or a list of names\n");
            return 2;
        }
        enable_privilege(L"SeDebugPrivilege");
        prepare_access();
        SetConsoleCtrlHandler(ctrl_handler, TRUE);
        return watch_loop(&wo);
    }
    if (dwm) {
        enable_privilege(L"SeDebugPrivilege");
        prepare_access();
        void *pl = process_list();
        pentry *pe = calloc(MAX_SEEN, sizeof(pentry));
        int np = (pl && pe) ? parse_processes(pl, pe, MAX_SEEN) : 0;
        int found = 0, bad = 0;
        for (int k = 0; k < np; k++)
            if (!_wcsicmp(pe[k].name, L"dwm.exe")) {
                found++;
                msg("nvinject: dwm.exe pid %lu session %lu\n", (unsigned long)pe[k].pid,
                    (unsigned long)pe[k].session);
                bad += inject_pid(pe[k].pid, lazy < 0 ? 0 : lazy, force) < 0;
            }
        if (pl)
            VirtualFree(pl, 0, MEM_RELEASE);
        free(pe);
        if (!found)
            msg("nvinject: no dwm.exe running\n");
        return (!found || bad) ? 1 : 0;
    }
    if (pids[0]) {
        enable_privilege(L"SeDebugPrivilege");
        prepare_access();
        int bad = 0, n = 0, already = 0;
        for (wchar_t *p = pids; *p;) {
            DWORD pid = (DWORD)wcstoul(p, &p, 10);
            if (!pid)
                break;
            n++;
            int rc = inject_pid(pid, lazy, force);
            bad += rc < 0;
            already += rc == 1;
            while (*p == L',' || *p == L' ')
                p++;
        }
        if (!n) {
            usage();
            return 2;
        }
        return bad ? 1 : (already == n ? 3 : 0);
    }
    if (i >= argc) {
        usage();
        return 2;
    }

    /* launch: resolve the program the way CreateProcess would (plus .exe) */
    if (!SearchPathW(NULL, argv[i], L".exe", MAX_PATH, exe, NULL)) {
        msg("nvinject: %ls: not found\n", argv[i]);
        return 2;
    }
    WORD mach = exe_machine(exe);
    if (mach && mach != SELF_MACHINE) {
        if (mach != IMAGE_FILE_MACHINE_I386 && mach != IMAGE_FILE_MACHINE_AMD64) {
            msg("nvinject: %ls: unsupported machine 0x%x\n", exe, mach);
            return 2;
        }
        static wchar_t a[32768];
        a[0] = 0;
        append_arg(a, 32768, L"-d");
        append_arg(a, 32768, log_dir);
        if (follow)
            append_arg(a, 32768, L"-f");
        if (timeout != INFINITE) {
            wchar_t t[32];
            _snwprintf(t, 31, L"%lu", (unsigned long)(timeout / 1000));
            t[31] = 0;
            append_arg(a, 32768, L"-t");
            append_arg(a, 32768, t);
        }
        for (wchar_t *l = extra_cfg; *l;) {
            wchar_t *e = wcsstr(l, L"\r\n");
            wchar_t line[512];
            size_t n = e ? (size_t)(e - l) : wcslen(l);
            if (n && n < 512) {
                wcsncpy(line, l, n);
                line[n] = 0;
                append_arg(a, 32768, L"-c");
                append_arg(a, 32768, line);
            }
            if (!e)
                break;
            l = e + 2;
        }
        append_arg(a, 32768, L"--");
        for (int k = i; k < argc; k++)
            append_arg(a, 32768, argv[k]);
        return run_other(a, INFINITE);
    }

    SetEnvironmentVariableW(L"NVTRACE_DIR", log_dir);
    CreateDirectoryW(log_dir, NULL);
    if (follow)
        SetEnvironmentVariableW(L"NVTRACE_FOLLOW", L"1");
    if (lazy >= 0)
        SetEnvironmentVariableW(L"NVTRACE_MODE", lazy ? L"lazy" : L"full");
    for (wchar_t *l = extra_cfg; *l;) {
        wchar_t *e = wcsstr(l, L"\r\n");
        wchar_t line[512];
        size_t n = e ? (size_t)(e - l) : wcslen(l);
        if (n && n < 512) {
            wcsncpy(line, l, n);
            line[n] = 0;
            wchar_t *eq = wcschr(line, L'=');
            if (eq) {
                *eq = 0;
                SetEnvironmentVariableW(line, eq + 1);
            }
        }
        if (!e)
            break;
        l = e + 2;
    }

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
        msg("nvinject: CreateProcess(%ls) failed (%lu)\n", exe, GetLastError());
        return 2;
    }
    if (inject_handle(pi.hProcess, 0, -1, 1)) {
        TerminateProcess(pi.hProcess, 2);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        return 2;
    }
    msg("nvinject: pid %lu traced\n", (unsigned long)pi.dwProcessId);
    ResumeThread(pi.hThread);
    CloseHandle(pi.hThread);
    DWORD w = WaitForSingleObject(pi.hProcess, timeout);
    if (w == WAIT_TIMEOUT) {
        /* ask the DLL to write its END record, then kill; the log is a
         * mapped file, so nothing written before is lost either way */
        msg("nvinject: timeout, stopping pid %lu\n", (unsigned long)pi.dwProcessId);
        if (!write_stop(pi.dwProcessId))
            WaitForSingleObject(pi.hProcess, 600);
        TerminateProcess(pi.hProcess, 124);
        WaitForSingleObject(pi.hProcess, 10000);
        remove_stop(pi.dwProcessId);
        CloseHandle(pi.hProcess);
        return 124;
    }
    DWORD code = 1;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hProcess);
    LocalFree(argv);
    return (int)code;
}

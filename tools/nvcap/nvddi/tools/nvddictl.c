/*
 * nvddictl - control nvddi.sys (run elevated).
 *
 *   nvddictl status
 *   nvddictl start              new log file, capture at the configured level
 *   nvddictl stop               stop capture, flush and close the file
 *   nvddictl flush              write everything recorded so far
 *   nvddictl level 0..3         0 off, 1 calls, 2 + structs, 3 + nested buffers
 *   nvddictl mute NAME|*        stop recording a slot (e.g. DxgkDdiInterruptRoutine)
 *   nvddictl unmute NAME|*
 *   nvddictl mark TEXT          put a marker event into the log
 *   nvddictl reset              clear Parameters\FailCount (re-enable after self-disable)
 *   nvddictl enable | disable   set Parameters\Enabled (takes effect at next boot)
 */
#include <windows.h>
#include <winioctl.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>
#include "nvddi_ioctl.h"

#define PARAMS L"SYSTEM\\CurrentControlSet\\Services\\nvddi\\Parameters"

static HANDLE open_dev(void)
{
    HANDLE h = CreateFileW(NVDDI_CTL_WIN32, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        DWORD e = GetLastError();
        fwprintf(stderr, L"cannot open %ls: error %lu%ls\n", NVDDI_CTL_WIN32, e,
                 e == ERROR_FILE_NOT_FOUND ? L" (driver not loaded?)" :
                 e == ERROR_ACCESS_DENIED ? L" (run elevated)" : L"");
    }
    return h;
}

static int ioctl(DWORD code, void *in, DWORD inl, void *out, DWORD outl)
{
    DWORD got = 0;
    HANDLE h = open_dev();
    BOOL ok;
    if (h == INVALID_HANDLE_VALUE)
        return 2;
    ok = DeviceIoControl(h, code, in, inl, out, outl, &got, NULL);
    CloseHandle(h);
    if (!ok) {
        fwprintf(stderr, L"DeviceIoControl 0x%lx failed: error %lu\n", code, GetLastError());
        return 1;
    }
    return 0;
}

static int set_param(const wchar_t *name, DWORD v)
{
    HKEY k;
    LONG r = RegCreateKeyExW(HKEY_LOCAL_MACHINE, PARAMS, 0, NULL, 0, KEY_SET_VALUE, NULL, &k, NULL);
    if (r != ERROR_SUCCESS) {
        fwprintf(stderr, L"cannot open HKLM\\%ls: error %ld\n", PARAMS, r);
        return 1;
    }
    r = RegSetValueExW(k, name, 0, REG_DWORD, (const BYTE *)&v, sizeof(v));
    RegCloseKey(k);
    if (r != ERROR_SUCCESS) {
        fwprintf(stderr, L"cannot set %ls: error %ld\n", name, r);
        return 1;
    }
    wprintf(L"%ls = %lu\n", name, v);
    return 0;
}

static int status(void)
{
    NVDDI_STATUS s;
    static const wchar_t *lv[] = {L"off", L"calls", L"structs", L"full"};
    memset(&s, 0, sizeof(s));
    if (ioctl(IOCTL_NVDDI_STATUS, NULL, 0, &s, sizeof(s)))
        return 1;
    wprintf(L"nvddi %u.%u.%u\n", s.version >> 24, (s.version >> 16) & 0xFF, s.version & 0xFFFF);
    wprintf(L"  level          %u (%ls)\n", s.level, s.level < 4 ? lv[s.level] : L"?");
    wprintf(L"  capturing      %ls\n", s.capturing ? L"yes" : L"no");
    wprintf(L"  attached       %ls (\\Device\\DxgKrnl filter)\n", s.attached ? L"yes" : L"NO");
    wprintf(L"  registrations  %u (miniports that called DxgkInitialize since load)\n", s.registrations);
    wprintf(L"  hooked         %u %hs\n", s.hooked, s.hooked_driver);
    wprintf(L"  rings          %u x %u KB\n", s.ncpu, s.ring_bytes / 1024);
    wprintf(L"  records        %llu  wrapped calls %llu\n", s.seq, s.calls);
    wprintf(L"  dropped        %llu (rings)  %llu (file)\n", s.dropped, s.dropped_file);
    wprintf(L"  file           %ls\n", s.file_name[0] ? (wchar_t *)s.file_name : L"(none)");
    wprintf(L"  file bytes     %llu\n", s.file_bytes);
    wprintf(L"  fail count     %u at load\n", s.fail_count);
    if (s.last_error)
        wprintf(L"  last error     0x%08x\n", s.last_error);
    if (!s.hooked)
        wprintf(L"\n  nothing hooked yet: restart the NVIDIA adapter now that nvddi is loaded:\n"
                L"    powershell -ExecutionPolicy Bypass -File restart-gpu.ps1\n");
    return 0;
}

int wmain(int argc, wchar_t **argv)
{
    if (argc < 2 || !wcscmp(argv[1], L"status"))
        return status();
    if (!wcscmp(argv[1], L"start"))
        return ioctl(IOCTL_NVDDI_START, NULL, 0, NULL, 0) || status();
    if (!wcscmp(argv[1], L"stop"))
        return ioctl(IOCTL_NVDDI_STOP, NULL, 0, NULL, 0) || status();
    if (!wcscmp(argv[1], L"flush"))
        return ioctl(IOCTL_NVDDI_FLUSH, NULL, 0, NULL, 0) || status();
    if (!wcscmp(argv[1], L"level") && argc > 2) {
        ULONG l = (ULONG)_wtoi(argv[2]);
        return ioctl(IOCTL_NVDDI_SET_LEVEL, &l, sizeof(l), NULL, 0);
    }
    if ((!wcscmp(argv[1], L"mute") || !wcscmp(argv[1], L"unmute")) && argc > 2) {
        NVDDI_MUTE m;
        memset(&m, 0, sizeof(m));
        m.mute = !wcscmp(argv[1], L"mute");
        WideCharToMultiByte(CP_ACP, 0, argv[2], -1, m.name, sizeof(m.name) - 1, NULL, NULL);
        return ioctl(IOCTL_NVDDI_MUTE, &m, sizeof(m), NULL, 0);
    }
    if (!wcscmp(argv[1], L"mark") && argc > 2) {
        char t[200];
        int n = WideCharToMultiByte(CP_ACP, 0, argv[2], -1, t, sizeof(t) - 1, NULL, NULL);
        return ioctl(IOCTL_NVDDI_MARK, t, n > 0 ? n - 1 : 0, NULL, 0);
    }
    if (!wcscmp(argv[1], L"reset"))
        return set_param(L"FailCount", 0);
    if (!wcscmp(argv[1], L"enable"))
        return set_param(L"Enabled", 1) || set_param(L"FailCount", 0);
    if (!wcscmp(argv[1], L"disable"))
        return set_param(L"Enabled", 0);
    fwprintf(stderr, L"usage: nvddictl status|start|stop|flush|level N|mute NAME|unmute NAME|mark TEXT|"
                     L"reset|enable|disable\n");
    return 2;
}

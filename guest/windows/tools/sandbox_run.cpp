// sandbox_run: run a command under a token like a Chromium/Edge GPU process
// (restricted token in the style of the Chromium sandbox's USER_LIMITED level,
// low integrity, a job object), with its stdout/stderr in a file, so driver
// messages that a browser's sandboxed GPU process loses (Mesa, NAK panics,
// DXVK) can be read, and the failure reproduced without a browser.
//
//   sandbox_run.exe OUT_FILE SECONDS LEVEL COMMAND...
//
// LEVEL: none (plain child, the control), low (low integrity only),
//        limited (restricted SIDs Everyone/Users/RESTRICTED/logon + low
//        integrity + all privileges but SeChangeNotify dropped; the GPU
//        sandbox's lockdown token).
// The child gets a job object (kill on close, UI limits off) and is killed
// by handle after SECONDS. Exit code: the child's, or 259 on timeout.
//
// Build: x86_64-w64-mingw32-g++ -std=c++17 -O2 -static sandbox_run.cpp
//          -o sandbox_run.exe -ladvapi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <sddl.h>

#include <cstdio>
#include <cstring>
#include <string>

static void fail(const char *what) {
  std::printf("sandbox_run: %s failed: %lu\n", what, GetLastError());
}

static bool set_low_integrity(HANDLE token) {
  PSID low = nullptr;
  if (!ConvertStringSidToSidA("S-1-16-4096", &low)) return false;
  TOKEN_MANDATORY_LABEL tml = {};
  tml.Label.Attributes = SE_GROUP_INTEGRITY;
  tml.Label.Sid = low;
  const BOOL ok = SetTokenInformation(token, TokenIntegrityLevel, &tml, sizeof(tml) + GetLengthSid(low));
  LocalFree(low);
  return ok != FALSE;
}

static HANDLE make_token(const char *level) {
  HANDLE self = nullptr;
  if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &self)) {
    fail("OpenProcessToken");
    return nullptr;
  }
  HANDLE out = nullptr;
  if (!std::strcmp(level, "low")) {
    if (!DuplicateTokenEx(self, TOKEN_ALL_ACCESS, nullptr, SecurityImpersonation, TokenPrimary, &out)) fail("DuplicateTokenEx");
  } else {
    // Deny-only every group but the logon SID, Everyone and Users; restricted
    // SIDs Everyone, Users, RESTRICTED and the logon SID (USER_LIMITED).
    DWORD len = 0;
    GetTokenInformation(self, TokenGroups, nullptr, 0, &len);
    auto *groups = static_cast<TOKEN_GROUPS *>(std::malloc(len));
    GetTokenInformation(self, TokenGroups, groups, len, &len);
    PSID everyone = nullptr, users = nullptr, restricted = nullptr, logon = nullptr;
    ConvertStringSidToSidA("S-1-1-0", &everyone);
    ConvertStringSidToSidA("S-1-5-32-545", &users);
    ConvertStringSidToSidA("S-1-5-12", &restricted);
    SID_AND_ATTRIBUTES deny[64];
    DWORD ndeny = 0;
    for (DWORD i = 0; i < groups->GroupCount && ndeny < 64; i++) {
      const auto &g = groups->Groups[i];
      if (g.Attributes & SE_GROUP_LOGON_ID) {
        logon = g.Sid;
        continue;
      }
      if (g.Attributes & SE_GROUP_INTEGRITY) continue;
      if (EqualSid(g.Sid, everyone) || EqualSid(g.Sid, users)) continue;
      deny[ndeny].Sid = g.Sid;
      deny[ndeny].Attributes = 0;
      ndeny++;
    }
    SID_AND_ATTRIBUTES rsids[4] = {{everyone, 0}, {users, 0}, {restricted, 0}, {logon, 0}};
    const DWORD nr = logon ? 4 : 3;
    // Every privilege but SeChangeNotifyPrivilege.
    GetTokenInformation(self, TokenPrivileges, nullptr, 0, &len);
    auto *privs = static_cast<TOKEN_PRIVILEGES *>(std::malloc(len));
    GetTokenInformation(self, TokenPrivileges, privs, len, &len);
    LUID change_notify;
    LookupPrivilegeValueA(nullptr, "SeChangeNotifyPrivilege", &change_notify);
    LUID_AND_ATTRIBUTES drop[64];
    DWORD ndrop = 0;
    for (DWORD i = 0; i < privs->PrivilegeCount && ndrop < 64; i++) {
      if (privs->Privileges[i].Luid.LowPart == change_notify.LowPart &&
          privs->Privileges[i].Luid.HighPart == change_notify.HighPart)
        continue;
      drop[ndrop++] = privs->Privileges[i];
    }
    if (!CreateRestrictedToken(self, 0, ndeny, deny, ndrop, drop, nr, rsids, &out)) fail("CreateRestrictedToken");
  }
  CloseHandle(self);
  if (out && !set_low_integrity(out)) fail("SetTokenInformation(low integrity)");
  return out;
}

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  if (argc < 5) {
    std::printf("usage: sandbox_run OUT_FILE SECONDS none|low|limited COMMAND...\n");
    return 2;
  }
  const char *out_path = argv[1];
  const DWORD seconds = DWORD(std::atoi(argv[2]));
  const char *level = argv[3];
  std::string cmd;
  for (int i = 4; i < argc; i++) {
    if (i > 4) cmd += ' ';
    cmd += argv[i];
  }
  SECURITY_ATTRIBUTES sa = {sizeof(sa), nullptr, TRUE};
  HANDLE file = CreateFileA(out_path, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, &sa, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_NORMAL, nullptr);
  if (file == INVALID_HANDLE_VALUE) {
    fail("CreateFile(out)");
    return 2;
  }
  STARTUPINFOA si = {};
  si.cb = sizeof(si);
  si.dwFlags = STARTF_USESTDHANDLES;
  si.hStdOutput = file;
  si.hStdError = file;
  si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
  PROCESS_INFORMATION pi = {};
  HANDLE job = CreateJobObjectA(nullptr, nullptr);
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION lim = {};
  lim.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  SetInformationJobObject(job, JobObjectExtendedLimitInformation, &lim, sizeof(lim));
  BOOL ok;
  std::printf("sandbox_run: level %s: %s\n", level, cmd.c_str());
  if (!std::strcmp(level, "none")) {
    ok = CreateProcessA(nullptr, cmd.data(), nullptr, nullptr, TRUE, CREATE_SUSPENDED, nullptr, nullptr, &si, &pi);
  } else {
    HANDLE token = make_token(level);
    if (!token) return 2;
    ok = CreateProcessAsUserA(token, nullptr, cmd.data(), nullptr, nullptr, TRUE, CREATE_SUSPENDED, nullptr, nullptr,
                              &si, &pi);
    CloseHandle(token);
  }
  if (!ok) {
    fail("CreateProcess");
    return 2;
  }
  AssignProcessToJobObject(job, pi.hProcess);
  ResumeThread(pi.hThread);
  DWORD code = 259;
  if (WaitForSingleObject(pi.hProcess, seconds * 1000) == WAIT_TIMEOUT) {
    std::printf("sandbox_run: timeout after %lu s, terminating the child (pid %lu)\n", seconds, pi.dwProcessId);
    TerminateProcess(pi.hProcess, 259);
    WaitForSingleObject(pi.hProcess, 5000);
  }
  GetExitCodeProcess(pi.hProcess, &code);
  std::printf("sandbox_run: child pid %lu exit %lu (0x%lx)\n", pi.dwProcessId, code, code);
  CloseHandle(file);
  return int(code);
}

// ods_capture: collect OutputDebugString output of this session, including
// sandboxed processes (a browser's GPU process: low integrity, restricted
// token), into a file. The Helios UMD logs through OutputDebugString when it
// may create neither C:\ProgramData\Helios\<module>-<pid>.log nor a file in
// C:\ProgramData\Helios\sandbox (umd_common/src/log.rs).
//
//   ods_capture.exe OUT_FILE [SECONDS [FILTER]]
//
// SECONDS (default 60) bounds the run; FILTER (default "helios") keeps only
// lines containing it ("" keeps everything). Each line: milliseconds since
// start, pid, text. Only one debug-output collector can run per session (the
// DBWIN objects are a single slot); a debugger attached to a process gets that
// process's output instead.
//
// The DBWIN objects are created with an ACL that admits Everyone, restricted
// tokens and AppContainers, and a low integrity label, so sandboxed writers
// can open them.
//
// Build: x86_64-w64-mingw32-g++ -std=c++17 -O2 -static ods_capture.cpp -o ods_capture.exe -ladvapi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <sddl.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>

int main(int argc, char **argv) {
  if (argc < 2) {
    std::fprintf(stderr, "usage: ods_capture OUT_FILE [SECONDS [FILTER]]\n");
    return 2;
  }
  const DWORD seconds = argc > 2 ? DWORD(std::atoi(argv[2])) : 60u;
  const char *filter = argc > 3 ? argv[3] : "helios";
  FILE *out = std::fopen(argv[1], "a");
  if (!out) {
    std::fprintf(stderr, "cannot open %s\n", argv[1]);
    return 1;
  }
  SECURITY_ATTRIBUTES sa = {sizeof(sa), nullptr, FALSE};
  PSECURITY_DESCRIPTOR sd = nullptr;
  if (ConvertStringSecurityDescriptorToSecurityDescriptorW(
          L"D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;WD)(A;;GA;;;RC)(A;;GA;;;AC)(A;;GA;;;S-1-15-2-2)S:(ML;;NW;;;LW)",
          SDDL_REVISION_1, &sd, nullptr))
    sa.lpSecurityDescriptor = sd;
  HANDLE buffer_ready = CreateEventW(&sa, FALSE, FALSE, L"DBWIN_BUFFER_READY");
  HANDLE data_ready = CreateEventW(&sa, FALSE, FALSE, L"DBWIN_DATA_READY");
  HANDLE mapping = CreateFileMappingW(INVALID_HANDLE_VALUE, &sa, PAGE_READWRITE, 0, 4096, L"DBWIN_BUFFER");
  if (!buffer_ready || !data_ready || !mapping) {
    std::fprintf(stderr, "DBWIN objects: error %lu\n", GetLastError());
    return 1;
  }
  if (GetLastError() == ERROR_ALREADY_EXISTS)
    std::fprintf(stderr, "warning: another debug-output collector holds DBWIN_BUFFER\n");
  const auto *view = static_cast<const unsigned char *>(MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 4096));
  if (!view) {
    std::fprintf(stderr, "MapViewOfFile: %lu\n", GetLastError());
    return 1;
  }
  std::fprintf(out, "== ods_capture start, %lu s, filter \"%s\"\n", seconds, filter);
  std::fflush(out);
  const ULONGLONG t0 = GetTickCount64(), end = t0 + ULONGLONG(seconds) * 1000ull;
  unsigned long kept = 0, seen = 0;
  SetEvent(buffer_ready);
  for (;;) {
    const ULONGLONG now = GetTickCount64();
    if (now >= end)
      break;
    const DWORD w = WaitForSingleObject(data_ready, DWORD(end - now > 500 ? 500 : end - now));
    if (w != WAIT_OBJECT_0)
      continue;
    DWORD pid = 0;
    std::memcpy(&pid, view, sizeof(pid));
    char text[4096 - sizeof(DWORD) + 1];
    std::memcpy(text, view + sizeof(DWORD), sizeof(text) - 1);
    text[sizeof(text) - 1] = 0;
    SetEvent(buffer_ready);
    seen++;
    if (*filter && !std::strstr(text, filter))
      continue;
    size_t n = std::strlen(text);
    while (n && (text[n - 1] == '\n' || text[n - 1] == '\r'))
      text[--n] = 0;
    std::fprintf(out, "%8llu %6lu %s\n", GetTickCount64() - t0, pid, text);
    kept++;
    if ((kept & 63) == 0)
      std::fflush(out);
  }
  std::fprintf(out, "== ods_capture end: %lu lines kept of %lu\n", kept, seen);
  std::fclose(out);
  return 0;
}

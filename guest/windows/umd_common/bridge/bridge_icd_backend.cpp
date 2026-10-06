// ICD backend selection and the NVK ICD loader. See `bridge_icd_backend.h`.
//
// Compiled into helios_umd.dll and helios_umd12.dll (S5, with
// HELIOS_ICD_BACKEND_D3D12: the D3D12-only levers in decide()); every copy
// reads the same environment and registry, so copies in one process agree
// except where a D3D12 lever says otherwise.

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <sddl.h>

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <mutex>

#include "bridge_common.h"  // umd_log
#include "bridge_icd_backend.h"
#include "helios_icd_interface.h"
#include "helios_icd_policy.h"  // protocol/include: shared with NVK

extern "C" int32_t helios_kmdmap_c_epoch(void);

namespace helios_bridge {

namespace {

// Executables that stay on Venus unless NvkDenyList replaces this list or
// NvkAllowList takes a name out of it. DWM and the shell compose everything
// else (S6 moves them); the rest open or produce surfaces shared with Venus
// processes (video, browsers, overlays, capture), which an NVK process cannot
// import (dxvk-on-nvk.md 3.7).
// The built-in deny-list: helios_policy_builtin_deny (helios_icd_policy.h).

// Deny-list entries measured to work on NVK under a Venus DWM
// (docs/dwm-on-nvk.md 4.2.1, 22.22.326.1, 2026-10-06): they make an NVK
// device, DWM opens their surfaces (A8 included), their windows compose, no
// crash. With HKLM\SOFTWARE\Helios!NvkDefaults=1 (REG_DWORD, default 0)
// they go to NVK even under Icd=venus and leave the built-in deny-list, as if
// NvkAllowList named them; an explicit NvkDenyList still wins. Off by default
// until the desktop moves to NVK as a whole (DwmIcd).
//  - csrss, winlogon, fontdrvhost, rdpclip: never create a D3D device (no UMD
//    log in 1038 logs), listed for completeness;
//  - taskmgr (NVK device, 3 resource ids), mmc (no D3D device);
//  - startmenuexperiencehost (NVK, 5 resource ids, Start menu drawn);
//  - systemsettings + applicationframehost (Settings: NVK, 3 and 12 resource
//    ids, window drawn).
// Not here, and why: explorer, shellexperiencehost, searchhost, textinputhost
// (a device-wide removal while explorer ran on NVK; ShellExperienceHost
// crashed at its NVK start; SearchHost crash-loops on Venus too), msedge and
// the Chromium family (video frames stall on NVK), video players (VLC's
// D3D11VA output is green on NVK), logonui/consent/lockapp (after the shell).
// The NvkDefaults names: helios_policy_nvk_default_names (helios_icd_policy.h).

void lower_ascii(char* s) {
  for (; *s; s++) {
    if (*s >= 'A' && *s <= 'Z') *s = char(*s - 'A' + 'a');
  }
}

bool reg_sz(const char* name, char* out, DWORD cap) {
  out[0] = 0;
  DWORD size = cap;
  return RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", name,
                      RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, nullptr, out,
                      &size) == ERROR_SUCCESS && out[0];
}

bool reg_dword(const char* name, DWORD* out) {
  DWORD size = sizeof(*out);
  return RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", name,
                      RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, nullptr, out,
                      &size) == ERROR_SUCCESS;
}

bool reg_wsz(const wchar_t* name, wchar_t* out, DWORD cap_chars) {
  out[0] = 0;
  DWORD size = cap_chars * sizeof(wchar_t);
  return RegGetValueW(HKEY_LOCAL_MACHINE, L"SOFTWARE\\Helios", name,
                      RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, nullptr, out,
                      &size) == ERROR_SUCCESS && out[0];
}

bool env_sz(const char* name, char* out, std::size_t cap) {
  out[0] = 0;
  const DWORD n = GetEnvironmentVariableA(name, out, DWORD(cap));
  return n > 0 && n < cap;
}

// `list` holds names separated by ';' (spaces around names ignored).
bool list_has(const char* list, const char* exe) {
  return helios_policy_list_has(list, exe) != 0;
}

// NvkDefaults=1 and `exe` is one of kBuiltinNvkDefault.
bool nvk_default(const char* exe) {
  return helios_policy_nvk_default(exe) != 0;
}

// NvkDenyList (REG_SZ) names `exe`.
bool explicitly_denied(const char* exe) {
  return helios_policy_explicitly_denied(exe) != 0;
}

bool denied(const char* exe, const char** why) {
  return helios_policy_denied(exe, why) != 0;
}

// DwmIcd=nvk crash-loop guard (docs/dwm-on-nvk.md, "Failure safety"). Windows
// restarts a DWM that dies; a DWM that dies on NVK at every start would leave
// the desktop black and, after a few rounds, make Windows give up on the
// display. %ProgramData%\Helios\dwm-nvk-starts.txt keeps the FILETIMEs of the
// recent DWM starts that chose NVK, one per line. When DwmNvkMaxStarts
// (REG_DWORD, default 2, 0 = no guard) of them fall inside the last
// DwmNvkGuardSeconds (default 600) this start goes to Venus and is not
// recorded, so NVK is tried again once the window has passed. A DWM that
// stays up never starts again, so a healthy session never trips it; a test
// that restarts DWM by hand more often than that sees Venus (delete the file
// or raise the knob).
bool dwm_nvk_guard_allows(const char** why) {
  DWORD max_starts = 2;
  reg_dword("DwmNvkMaxStarts", &max_starts);
  if (max_starts == 0)
    return true;
  DWORD window_s = 600;
  reg_dword("DwmNvkGuardSeconds", &window_s);
  char dir[MAX_PATH];
  const DWORD n = GetEnvironmentVariableA("ProgramData", dir, MAX_PATH);
  if (n == 0 || n >= MAX_PATH - 40)
    std::snprintf(dir, sizeof(dir), "C:\\ProgramData");
  char path[MAX_PATH];
  std::snprintf(path, sizeof(path), "%s\\Helios\\dwm-nvk-starts.txt", dir);

  FILETIME ft;
  GetSystemTimeAsFileTime(&ft);
  const unsigned long long now =
      (static_cast<unsigned long long>(ft.dwHighDateTime) << 32) | ft.dwLowDateTime;
  const unsigned long long window = static_cast<unsigned long long>(window_s) * 10000000ull;

  unsigned long long kept[16];
  std::size_t count = 0;
  if (FILE* f = std::fopen(path, "r")) {
    unsigned long long t = 0;
    while (count < 15 && std::fscanf(f, "%llu", &t) == 1) {
      if (t <= now && now - t < window)
        kept[count++] = t;
    }
    std::fclose(f);
  }
  if (count >= max_starts) {
    static char reason[192];
    std::snprintf(reason, sizeof(reason),
                  "DwmIcd=nvk, but %u DWM starts on NVK in the last %u s (crash-loop guard, "
                  "DwmNvkMaxStarts=%u): Venus",
                  unsigned(count), unsigned(window_s), unsigned(max_starts));
    *why = reason;
    return false;
  }
  kept[count++] = now;
  if (FILE* f = std::fopen(path, "w")) {
    for (std::size_t i = 0; i < count; i++)
      std::fprintf(f, "%llu\n", kept[i]);
    std::fclose(f);
  } else {
    umd_log("icd backend: DwmIcd=nvk crash-loop guard cannot write its file; NVK unguarded");
  }
  return true;
}

// "The desktop follows DWM" (docs/dwm-on-nvk.md 4.2): a DWM on NVK cannot
// import surfaces Venus processes made (it shows a blank placeholder for them:
// the gray Start menu, search and notification centre). So while DWM runs on
// NVK, every D3D11 process goes to NVK too: Icd=venus and the built-in
// deny-list no longer apply (the deny-list exists because an NVK process and a
// Venus process cannot share surfaces, which is exactly what it would cause
// now); only an explicit NvkDenyList keeps a process on Venus.
// DesktopFollowsDwm (REG_DWORD, default 1) = 0 turns this off.
//
// The marker is a named event in the session's namespace that the DWM on NVK
// creates when it chooses NVK and closes when NVK fails for it; it dies with
// that DWM. So it names the DWM that runs now (the crash-loop guard sending a
// DWM to Venus leaves no marker), not just the registry value. Sandboxed
// processes may not open it (its DACL admits everyone for SYNCHRONIZE, but a
// restricted token can still be refused): ERROR_ACCESS_DENIED also means it
// exists.
constexpr const char kDwmOnNvkEvent[] = HELIOS_POLICY_DWM_MARKER;
HANDLE g_dwm_marker = nullptr;

void dwm_marker_create() {
  if (g_dwm_marker)
    return;
  // Everyone (and restricted / AppContainer tokens) may wait on it; low label.
  const char* sddl = "D:(A;;0x100001;;;WD)(A;;0x100001;;;RC)(A;;0x100001;;;AC)(A;;GA;;;SY)S:(ML;;NW;;;LW)";
  PSECURITY_DESCRIPTOR sd = nullptr;
  SECURITY_ATTRIBUTES sa = { sizeof(sa), nullptr, FALSE };
  if (ConvertStringSecurityDescriptorToSecurityDescriptorA(sddl, SDDL_REVISION_1, &sd, nullptr))
    sa.lpSecurityDescriptor = sd;
  g_dwm_marker = CreateEventA(&sa, TRUE, TRUE, kDwmOnNvkEvent);
  if (sd)
    LocalFree(sd);
  umd_log(g_dwm_marker ? "icd backend: DWM on NVK: the desktop follows (marker Local\\HeliosDwmOnNvk set)"
                       : "icd backend: DWM on NVK, but its marker event could not be created");
}

void dwm_marker_drop() {
  if (g_dwm_marker) {
    CloseHandle(g_dwm_marker);
    g_dwm_marker = nullptr;
    umd_log("icd backend: DWM left NVK: marker Local\\HeliosDwmOnNvk dropped");
  }
}

// A non-DWM process: does the running DWM compose on NVK (and should this
// process follow it)?
[[maybe_unused]] bool desktop_follows_dwm_on_nvk() {
  return helios_policy_desktop_follows_dwm() != 0;
}

bool file_exists(const wchar_t* path) {
  const DWORD a = GetFileAttributesW(path);
  return a != INVALID_FILE_ATTRIBUTES && !(a & FILE_ATTRIBUTE_DIRECTORY);
}

// The driver package installs NVK next to the UMDs in the driver store
// (helios_kmd_render.inx): vulkan_nouveau.dll + librmclient.dll for AMD64,
// vulkan_nouveau32.dll + librmclient32.dll for WoW64.
bool nvk_next_to_umd(wchar_t* out, std::size_t cap) {
  HMODULE self = nullptr;
  if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                              GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                          reinterpret_cast<LPCWSTR>(&nvk_next_to_umd), &self))
    return false;
  wchar_t dir[MAX_PATH];
  const DWORD len = GetModuleFileNameW(self, dir, MAX_PATH);
  if (len == 0 || len >= MAX_PATH)
    return false;
  wchar_t* slash = std::wcsrchr(dir, L'\\');
  if (!slash)
    return false;
  slash[1] = 0;
#if defined(_WIN64)
  std::swprintf(out, cap, L"%lsvulkan_nouveau.dll", dir);
#else
  std::swprintf(out, cap, L"%lsvulkan_nouveau32.dll", dir);
#endif
  return file_exists(out);
}

bool find_nvk_icd(wchar_t* out, std::size_t cap) {
  out[0] = 0;
  // An override that names no file (a stale test copy, deleted since) falls
  // through to the installed NVK instead of putting the process on Venus:
  // that silently turned "HELIOS_ICD=nvk" runs into Venus runs (2026-10-06,
  // Heaven with HELIOS_NVK_ICD=...\s315\nvk32 after the copy was removed).
  const DWORD n = GetEnvironmentVariableW(L"HELIOS_NVK_ICD", out, DWORD(cap));
  if (n > 0 && n < cap && file_exists(out))
    return true;
#if defined(_WIN64)
  if (reg_wsz(L"NvkIcdPath", out, DWORD(cap)) && file_exists(out))
    return true;
#else
  if (reg_wsz(L"NvkIcdPath32", out, DWORD(cap)) && file_exists(out))
    return true;
#endif
  if (nvk_next_to_umd(out, cap))
    return true;
  // %ProgramFiles% is "Program Files (x86)" in a WoW64 process.
  wchar_t pf[MAX_PATH];
  const DWORD m = GetEnvironmentVariableW(L"ProgramFiles", pf, MAX_PATH);
  if (m == 0 || m >= MAX_PATH)
    return false;
  std::swprintf(out, cap, L"%ls\\Helios\\nvk\\vulkan_nouveau.dll", pf);
  return file_exists(out);
}

IcdBackendChoice decide() {
  IcdBackendChoice c = {};
  c.backend = IcdBackend::Venus;

  char path[MAX_PATH] = {};
  GetModuleFileNameA(nullptr, path, MAX_PATH);
  const char* base = std::strrchr(path, '\\');
  base = base ? base + 1 : path;
  std::snprintf(c.exe, sizeof(c.exe), "%s", base);
  lower_ascii(c.exe);

  char forced[16];
  bool force_nvk = false;
#if defined(HELIOS_ICD_BACKEND_D3D12)
  // D3D12 (helios_umd12.dll, S5) has its own levers in front of the shared
  // ones: HELIOS_ICD12 (environment, venus|nvk) wins over HELIOS_ICD; Nvk12=0
  // (REG_DWORD) keeps D3D12 on Venus everywhere; NvkDenyList12 (REG_SZ, `;`
  // separated executables) keeps titles that need what NVK lacks -- ray
  // tracing (DXR) above all -- on Venus. On NVK D3D12 reports no DXR.
  if (env_sz("HELIOS_ICD12", forced, sizeof(forced))) {
    lower_ascii(forced);
    if (std::strcmp(forced, "venus") == 0) {
      c.reason = "HELIOS_ICD12=venus";
      return c;
    }
    if (std::strcmp(forced, "nvk") == 0) {
      force_nvk = true;
      c.reason = "HELIOS_ICD12=nvk";
    }
  }
  if (!force_nvk) {
    DWORD nvk12 = 1;
    if (reg_dword("Nvk12", &nvk12) && nvk12 == 0) {
      c.reason = "HKLM\\SOFTWARE\\Helios!Nvk12=0 (D3D12 on Venus)";
      return c;
    }
    char deny12[4096];
    if (reg_sz("NvkDenyList12", deny12, sizeof(deny12)) && list_has(deny12, c.exe)) {
      c.reason = "NvkDenyList12 names this executable (D3D12, e.g. DXR)";
      return c;
    }
  }
  if (!force_nvk && env_sz("HELIOS_ICD", forced, sizeof(forced))) {
#else
  if (env_sz("HELIOS_ICD", forced, sizeof(forced))) {
#endif
    lower_ascii(forced);
    if (std::strcmp(forced, "venus") == 0) {
      c.reason = "HELIOS_ICD=venus";
      return c;
    }
    if (std::strcmp(forced, "nvk") == 0)
      force_nvk = true;
  }

  // DWM's own lever (docs/dwm-on-nvk.md): HKLM\SOFTWARE\Helios!DwmIcd = nvk
  // puts dwm.exe on NVK whatever Icd and the deny-lists say; venus or absent
  // (the default) leaves DWM to the rules below, which keep it on Venus. Read
  // only in dwm.exe, and only while the crash-loop guard allows it.
  if (!force_nvk && std::strcmp(c.exe, "dwm.exe") == 0) {
    char dwm[16];
    if (reg_sz("DwmIcd", dwm, sizeof(dwm))) {
      lower_ascii(dwm);
      if (std::strcmp(dwm, "nvk") == 0) {
        const char* why = nullptr;
        if (!dwm_nvk_guard_allows(&why)) {
          c.reason = why;
          return c;
        }
        force_nvk = true;
        c.reason = "HKLM\\SOFTWARE\\Helios!DwmIcd=nvk";
        dwm_marker_create();
      }
    }
  }

  if (!force_nvk) {
#if !defined(HELIOS_ICD_BACKEND_D3D12)
    // D3D11 only. `Icd=venus` is the transition default that keeps DWM and the
    // D3D11 desktop on Venus; it does not reach D3D12: vkd3d on Venus has no
    // VK_EXT_device_generated_commands, so it stops at FL11_0 and every title
    // that needs FL12 fails there, while NVK serves FL12_0 / SM 6.8. D3D12 goes
    // to NVK unless HELIOS_ICD(12)=venus, Nvk12=0, NvkDenyList12 or the
    // deny-list below says otherwise, or NVK fails (2026-10-06).
    // NvkAllowList names executables that go to NVK whatever Icd says: the
    // per-category lever of docs/dwm-on-nvk.md 4.2 (move one category off
    // the Venus defaults at a time, Icd=venus staying for everything else).
    if (std::strcmp(c.exe, "dwm.exe") != 0 && desktop_follows_dwm_on_nvk()) {
      if (explicitly_denied(c.exe)) {
        c.reason = "DWM is on NVK, but NvkDenyList names this executable";
        return c;
      }
      if (!find_nvk_icd(c.nvk_path, sizeof(c.nvk_path) / sizeof(c.nvk_path[0]))) {
        c.reason = "DWM is on NVK, but no NVK ICD was found";
        return c;
      }
      c.backend = IcdBackend::NvkRm;
      c.reason = "DWM is on NVK (DwmIcd=nvk, marker set): the desktop follows, "
                 "Icd and the built-in deny-list do not apply";
      return c;
    }
    char allow[4096];
    const bool allowed = (reg_sz("NvkAllowList", allow, sizeof(allow)) && list_has(allow, c.exe))
      || (nvk_default(c.exe) && !explicitly_denied(c.exe));
    char mode[16];
    if (!allowed && reg_sz("Icd", mode, sizeof(mode))) {
      lower_ascii(mode);
      if (std::strcmp(mode, "venus") == 0) {
        c.reason = "HKLM\\SOFTWARE\\Helios!Icd=venus";
        return c;
      }
    }
    if (allowed)
      c.reason = "NvkAllowList or NvkDefaults names this executable";
#endif
    const char* why = nullptr;
    if (denied(c.exe, &why)) {
      c.reason = why;
      return c;
    }
  }

  if (!find_nvk_icd(c.nvk_path, sizeof(c.nvk_path) / sizeof(c.nvk_path[0]))) {
    c.reason = "no NVK ICD (HELIOS_NVK_ICD, NvkIcdPath, driver store, %ProgramFiles%\\Helios\\nvk)";
    return c;
  }
  c.backend = IcdBackend::NvkRm;
  if (!c.reason) {
#if defined(HELIOS_ICD_BACKEND_D3D12)
    c.reason = force_nvk ? "HELIOS_ICD=nvk" : "D3D12 default NVK (Icd does not apply), not denied";
#else
    c.reason = force_nvk ? "HELIOS_ICD=nvk" : "global NVK, not denied";
#endif
  }
  return c;
}

std::once_flag g_choice_once;
IcdBackendChoice g_choice;
std::atomic<bool> g_nvk_failed{false};
// The process's loss epoch (helios_kmdmap.h) when NVK failed. A failure is a
// latch for the process, except across a driver update or device restart:
// once the epoch moved, the next device creation tries NVK again
// (librmclient reopens the KMD in a new generation), so a long-lived process
// (explorer, the shell hosts, DWM) comes back to NVK instead of staying on
// Venus, whose surfaces an NVK DWM cannot compose.
std::atomic<int32_t> g_nvk_failed_epoch{0};

std::once_flag g_nvk_load_once;
HMODULE g_nvk_module = nullptr;
void* g_nvk_gipa = nullptr;
PFN_helios_icd_interface_v2 g_nvk_iface = nullptr;

using PfnNegotiate = VkResult(VKAPI_PTR*)(uint32_t*);

void load_nvk() {
  const IcdBackendChoice& c = icd_backend_choice();
  if (c.backend != IcdBackend::NvkRm)
    return;
  // NVK's RM backend is opt-in inside the ICD (it would otherwise also look
  // for nouveau); this process asked for it.
  if (!GetEnvironmentVariableA("NVK_RM", nullptr, 0))
    _putenv_s("NVK_RM", "1");
  // Vulkan Video decode is experimental in NVK: without NVK_EXPERIMENTAL=video
  // it shows no video queue, and DXVK's D3D11 video decoder (the UMD's video
  // DDI, `VideoDdi` knob, default on for NVK) has nothing to run on. Added to
  // whatever the environment already asks for.
  DWORD video_ddi = 1;
  reg_dword("VideoDdi", &video_ddi);
  if (video_ddi != 0) {
    char experimental[256] = {};
    const DWORD n = GetEnvironmentVariableA("NVK_EXPERIMENTAL", experimental, sizeof(experimental));
    if (n == 0) {
      _putenv_s("NVK_EXPERIMENTAL", "video");
    } else if (n < sizeof(experimental) - 8 && !std::strstr(experimental, "video")) {
      std::strcat(experimental, ",video");
      _putenv_s("NVK_EXPERIMENTAL", experimental);
    }
  }
  // LOAD_WITH_ALTERED_SEARCH_PATH: librmclient.dll next to the ICD resolves
  // first (the ICD also loads it by its own path).
  HMODULE m = LoadLibraryExW(c.nvk_path, nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
  if (!m) {
    char msg[160];
    std::snprintf(msg, sizeof(msg), "icd backend: NVK ICD did not load (error %lu)",
                  static_cast<unsigned long>(GetLastError()));
    umd_log(msg);
    return;
  }
  auto negotiate = reinterpret_cast<PfnNegotiate>(reinterpret_cast<void*>(
      GetProcAddress(m, "vk_icdNegotiateLoaderICDInterfaceVersion")));
  void* gipa = reinterpret_cast<void*>(GetProcAddress(m, "vk_icdGetInstanceProcAddr"));
  auto iface = reinterpret_cast<PFN_helios_icd_interface_v2>(reinterpret_cast<void*>(
      GetProcAddress(m, HELIOS_ICD_INTERFACE_EXPORT)));
  if (!negotiate || !gipa || !iface) {
    umd_log("icd backend: NVK ICD lacks vk_icd* or helios_icd_interface_v2");
    FreeLibrary(m);
    return;
  }
  uint32_t version = 7;
  if (negotiate(&version) != VK_SUCCESS) {
    umd_log("icd backend: NVK ICD refused interface negotiation");
    FreeLibrary(m);
    return;
  }
  // Held for the process lifetime: DXVK's instance and every VkDeviceMemory
  // name code in it.
  g_nvk_module = m;
  g_nvk_gipa = gipa;
  g_nvk_iface = iface;
  char msg[160];
  std::snprintf(msg, sizeof(msg), "icd backend: NVK ICD loaded, loader interface %u", version);
  umd_log(msg);
}

}  // namespace

const IcdBackendChoice& icd_backend_choice() {
  std::call_once(g_choice_once, [] {
    g_choice = decide();
    char msg[1024];
    std::snprintf(msg, sizeof(msg), "icd backend: %s for %s (%s)%s%ls",
                  g_choice.backend == IcdBackend::NvkRm ? "NVK on RM" : "Venus",
                  g_choice.exe, g_choice.reason,
                  g_choice.backend == IcdBackend::NvkRm ? " -> " : "",
                  g_choice.backend == IcdBackend::NvkRm ? g_choice.nvk_path : L"");
    umd_log(msg);
  });
  return g_choice;
}

bool is_dwm_process() {
  return std::strcmp(icd_backend_choice().exe, "dwm.exe") == 0;
}

IcdBackend effective_icd_backend() {
  const IcdBackendChoice& c = icd_backend_choice();
  if (c.backend == IcdBackend::NvkRm && g_nvk_failed.load(std::memory_order_acquire)) {
    const int32_t now = helios_kmdmap_c_epoch();
    const int32_t then = g_nvk_failed_epoch.load(std::memory_order_acquire);
    if (now != then && g_nvk_failed.exchange(false, std::memory_order_acq_rel)) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
                    "icd backend: the KMD restarted since NVK failed (loss epoch %d -> %d); "
                    "NVK again for this process", int(then), int(now));
      umd_log(msg);
      if (is_dwm_process() && c.reason && std::strstr(c.reason, "DwmIcd"))
        dwm_marker_create();
    }
  }
  if (c.backend == IcdBackend::NvkRm && !g_nvk_failed.load(std::memory_order_acquire))
    return IcdBackend::NvkRm;
  return IcdBackend::Venus;
}

void note_nvk_failed(const char* why) {
  g_nvk_failed_epoch.store(helios_kmdmap_c_epoch(), std::memory_order_release);
  if (!g_nvk_failed.exchange(true, std::memory_order_acq_rel)) {
    dwm_marker_drop();
    char msg[256];
    std::snprintf(msg, sizeof(msg), "icd backend: NVK failed (%s); Venus for this process", why);
    umd_log(msg);
  }
}

void* nvk_icd_get_instance_proc_addr() {
  std::call_once(g_nvk_load_once, load_nvk);
  return g_nvk_gipa;
}

NvkPolicyScope::NvkPolicyScope() {
  if (icd_backend_choice().backend != IcdBackend::NvkRm)
    return;
  const DWORD n = GetEnvironmentVariableA("HELIOS_ICD", saved_, sizeof(saved_));
  had_ = n > 0 && n < sizeof(saved_);
  if (had_ && _stricmp(saved_, "nvk") == 0)
    return;
  if (_putenv_s("HELIOS_ICD", "nvk") != 0)
    return;
  active_ = true;
  umd_log(had_ ? "icd backend: HELIOS_ICD=nvk for NVK's own policy while the device is created "
                 "(was set to another value)"
               : "icd backend: HELIOS_ICD=nvk for NVK's own policy while the device is created");
}

NvkPolicyScope::~NvkPolicyScope() {
  if (!active_)
    return;
  // An empty value removes the variable (_putenv_s semantics).
  _putenv_s("HELIOS_ICD", had_ ? saved_ : "");
}

const helios_icd_api* nvk_icd_api(helios_icd_api* out) {
  std::call_once(g_nvk_load_once, load_nvk);
  if (!g_nvk_iface || !out)
    return nullptr;
  std::memset(out, 0, sizeof(*out));
  out->size = sizeof(*out);
  if (g_nvk_iface(HELIOS_ICD_INTERFACE_VERSION, out) != VK_SUCCESS)
    return nullptr;
  if (out->backend != HELIOS_ICD_BACKEND_NVK_RM)
    return nullptr;
  return out;
}

}  // namespace helios_bridge

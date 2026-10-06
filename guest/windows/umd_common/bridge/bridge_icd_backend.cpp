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

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <mutex>

#include "bridge_common.h"  // umd_log
#include "bridge_icd_backend.h"
#include "helios_icd_interface.h"

namespace helios_bridge {

namespace {

// Executables that stay on Venus unless NvkDenyList replaces this list or
// NvkAllowList takes a name out of it. DWM and the shell compose everything
// else (S6 moves them); the rest open or produce surfaces shared with Venus
// processes (video, browsers, overlays, capture), which an NVK process cannot
// import (dxvk-on-nvk.md 3.7).
constexpr const char* kBuiltinDeny[] = {
  "dwm.exe", "explorer.exe", "csrss.exe", "winlogon.exe", "logonui.exe",
  "consent.exe", "fontdrvhost.exe", "sihost.exe", "dllhost.exe",
  "searchhost.exe", "searchapp.exe", "startmenuexperiencehost.exe",
  "shellexperiencehost.exe", "shellhost.exe", "textinputhost.exe",
  "lockapp.exe", "applicationframehost.exe", "systemsettings.exe",
  "runtimebroker.exe", "widgets.exe", "widgetservice.exe",
  "phoneexperiencehost.exe", "crossdeviceresume.exe", "taskmgr.exe",
  "mmc.exe", "rdpclip.exe", "msedge.exe", "msedgewebview2.exe",
  "chrome.exe", "firefox.exe", "brave.exe", "opera.exe", "teams.exe",
  "ms-teams.exe", "discord.exe", "slack.exe", "spotify.exe", "code.exe",
  "obs64.exe", "obs32.exe", "vlc.exe", "mpc-hc64.exe", "mpc-be64.exe",
  "video.ui.exe", "microsoft.photos.exe", "photos.exe",
  "steamwebhelper.exe", "epicwebhelper.exe",
  "cefsharp.browsersubprocess.exe", "nvidia share.exe",
};

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
  const std::size_t len = std::strlen(exe);
  const char* p = list;
  while (*p) {
    while (*p == ';' || *p == ' ' || *p == '\t') p++;
    const char* start = p;
    while (*p && *p != ';') p++;
    const char* end = p;
    while (end > start && (end[-1] == ' ' || end[-1] == '\t')) end--;
    if (std::size_t(end - start) == len && _strnicmp(start, exe, len) == 0)
      return true;
  }
  return false;
}

bool denied(const char* exe, const char** why) {
  char allow[4096];
  if (reg_sz("NvkAllowList", allow, sizeof(allow)) && list_has(allow, exe))
    return false;
  char deny[4096];
  if (reg_sz("NvkDenyList", deny, sizeof(deny))) {
    *why = "NvkDenyList names this executable";
    return list_has(deny, exe);
  }
  for (const char* name : kBuiltinDeny) {
    if (std::strcmp(name, exe) == 0) {
      *why = "built-in deny-list (compositor, shell or interop-heavy app)";
      return true;
    }
  }
  return false;
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
  const DWORD n = GetEnvironmentVariableW(L"HELIOS_NVK_ICD", out, DWORD(cap));
  if (n > 0 && n < cap)
    return file_exists(out);
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

#if defined(HELIOS_ICD_BACKEND_D3D12)
bool reg_dword(const char* name, DWORD* out) {
  DWORD size = sizeof(*out);
  return RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", name,
                      RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, nullptr, out,
                      &size) == ERROR_SUCCESS;
}
#endif

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

  if (!force_nvk) {
    char mode[16];
    if (reg_sz("Icd", mode, sizeof(mode))) {
      lower_ascii(mode);
      if (std::strcmp(mode, "venus") == 0) {
        c.reason = "HKLM\\SOFTWARE\\Helios!Icd=venus";
        return c;
      }
    }
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
  if (!c.reason)
    c.reason = force_nvk ? "HELIOS_ICD=nvk" : "global NVK, not denied";
  return c;
}

std::once_flag g_choice_once;
IcdBackendChoice g_choice;
std::atomic<bool> g_nvk_failed{false};

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

IcdBackend effective_icd_backend() {
  const IcdBackendChoice& c = icd_backend_choice();
  if (c.backend == IcdBackend::NvkRm && !g_nvk_failed.load(std::memory_order_acquire))
    return IcdBackend::NvkRm;
  return IcdBackend::Venus;
}

void note_nvk_failed(const char* why) {
  if (!g_nvk_failed.exchange(true, std::memory_order_acq_rel)) {
    char msg[256];
    std::snprintf(msg, sizeof(msg), "icd backend: NVK failed (%s); Venus for this process", why);
    umd_log(msg);
  }
}

void* nvk_icd_get_instance_proc_addr() {
  std::call_once(g_nvk_load_once, load_nvk);
  return g_nvk_gipa;
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

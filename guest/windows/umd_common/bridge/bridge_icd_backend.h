// Which Vulkan ICD a Helios UMD runs its D3D engine on, and the backend-neutral
// table it talks to that ICD through (`protocol/include/helios_icd_interface.h`;
// `docs/dxvk-on-nvk.md` on research/dxvk-on-nvk, sections 3.2 and 3.3, stage S3).
//
// # Selection (user decision, 2026-10-06): global NVK with a deny-list
//
// Every process runs on NVK on RM unless it is denied, NVK is not provisioned,
// or NVK cannot serve it; then it runs on Venus exactly as before. Decided once
// per process, in this order:
//
//  1. `HELIOS_ICD` in the process environment: `venus` or `nvk` (tests, and a
//     per-launch override; it skips the deny-list).
//  1b. dwm.exe only: `HKLM\SOFTWARE\Helios!DwmIcd` (REG_SZ) `nvk` puts DWM on
//     NVK past `Icd` and the deny-lists, unless the crash-loop guard says DWM
//     already started on NVK `DwmNvkMaxStarts` (default 2) times in the last
//     `DwmNvkGuardSeconds` (default 600); `venus` or absent changes nothing
//     (docs/dwm-on-nvk.md).
//  2. `HKLM\SOFTWARE\Helios!Icd` (REG_SZ): `venus` turns NVK off everywhere;
//     `nvk` or absent selects the global mode below.
//  3. The deny-list. `HKLM\SOFTWARE\Helios!NvkDenyList` (REG_SZ, executable
//     names separated by `;`, case-insensitive) REPLACES the built-in list when
//     present; `NvkAllowList` (same form) removes names from whichever list is in
//     force. The built-in list holds the compositor and shell (DWM must stay on
//     Venus until S6), browsers, video players and other interop-heavy apps that
//     open surfaces Venus processes made, which an NVK process cannot import.
//  4. The NVK ICD must exist: `HELIOS_NVK_ICD` (environment), else
//     `NvkIcdPath` / `NvkIcdPath32` (REG_SZ, the full path of
//     `vulkan_nouveau.dll`, with `librmclient.dll` next to it) when that file
//     exists, else the driver package's copy next to this UMD in the driver
//     store (`vulkan_nouveau.dll` + `librmclient.dll`; WoW64:
//     `vulkan_nouveau32.dll` + `librmclient32.dll`), else
//     `%ProgramFiles%\Helios\nvk\vulkan_nouveau.dll` (the x86 Program Files in
//     a WoW64 process). Missing file = Venus. NVK itself applies the same
//     policy to Vulkan apps (Mesa patch 0032) and Zink to OpenGL apps (0034).
//
// D3D12 (helios_umd12.dll, stage S5, compiled with HELIOS_ICD_BACKEND_D3D12)
// checks three levers of its own first: `HELIOS_ICD12` (environment, venus|nvk,
// wins over HELIOS_ICD), `Nvk12` (REG_DWORD, 0 = D3D12 on Venus everywhere)
// and `NvkDenyList12` (REG_SZ, executables whose D3D12 stays on Venus -- titles
// that need ray tracing above all: NVK has no DXR and reports none).
// D3D12 ignores `Icd` (2026-10-06): `Icd=venus` is the D3D11 transition default
// that keeps DWM on Venus, but vkd3d on Venus lacks
// VK_EXT_device_generated_commands and stops at FL11_0, so D3D12 runs on NVK
// (FL12_0, SM 6.8, no DXR) unless HELIOS_ICD12 / HELIOS_ICD say venus, Nvk12=0,
// NvkDenyList12 or the deny-list (step 3) names the process, or NVK fails.
//
// After that the device create can still fall back: the DLL does not load, it
// lacks `helios_icd_interface_v2`, NVK enumerates no GPU, or the DXVK device
// fails. The first such failure latches the process to Venus
// (`note_nvk_failed`), so one process never alternates between the two.
#pragma once

#include <cstdint>

struct helios_icd_api;

namespace helios_bridge {

enum class IcdBackend : std::uint32_t {
  Venus = 1,
  NvkRm = 2,
};

struct IcdBackendChoice {
  IcdBackend backend;
  // Static string: why. Logged once.
  const char* reason;
  // The process's executable name (lower case), as matched against the lists.
  char exe[128];
  // NVK only: the ICD to load.
  wchar_t nvk_path[520];
};

/// The process's choice. Thread-safe; computed on first call.
const IcdBackendChoice& icd_backend_choice();

/// This process is dwm.exe (the executable name the choice matched).
bool is_dwm_process();

/// The backend device creation should use now: the choice, unless NVK has
/// failed in this process.
IcdBackend effective_icd_backend();

/// Latch the process to Venus after an NVK failure. Logs `why`.
void note_nvk_failed(const char* why);

/// NVK: `vk_icdGetInstanceProcAddr` of the loaded ICD (as `void*`, so this
/// header needs no Vulkan header), after `vk_icdNegotiateLoaderICDInterfaceVersion`.
/// Loads the DLL on first call; null if it cannot be used.
void* nvk_icd_get_instance_proc_addr();

/// NVK: the ICD's `helios_icd_interface_v2` table, re-read on every call so the
/// caps follow the KMD (IMPORT_RM can open after a host update). Null if the
/// DLL is not loaded or lacks the export. `out` is caller storage.
const helios_icd_api* nvk_icd_api(helios_icd_api* out);

/// NVK applies the Helios ICD policy itself (Mesa patch 0032: `HELIOS_ICD`,
/// `Icd`, `NvkDenyList`, `NvkAllowList`) when it first enumerates devices, and
/// hides its GPU from a process that policy sends to Venus. It does not know
/// the D3D12 levers, so with `HKLM\SOFTWARE\Helios!Icd=venus` and
/// `HELIOS_ICD12=nvk` the UMD loads NVK and NVK enumerates no GPU. The UMD has
/// already decided when it creates a device on the ICD it loaded itself, so for
/// that scope this sets `HELIOS_ICD=nvk` (NVK reads it with
/// GetEnvironmentVariableA) and restores the previous value afterwards. NVK
/// latches its answer once per module, at that first enumeration.
class NvkPolicyScope {
 public:
  NvkPolicyScope();
  ~NvkPolicyScope();
  NvkPolicyScope(const NvkPolicyScope&) = delete;
  NvkPolicyScope& operator=(const NvkPolicyScope&) = delete;

 private:
  bool active_ = false;
  bool had_ = false;
  char saved_[32] = {};
};

}  // namespace helios_bridge

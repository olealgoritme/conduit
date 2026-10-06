/* SPDX-License-Identifier: MIT
 *
 * The Helios ICD process policy: does a process in a Windows guest use NVK on
 * RM, or Venus? One implementation, header-only, shared by the D3D UMDs
 * (guest/windows/umd_common/bridge/bridge_icd_backend.cpp) and by NVK itself
 * (Mesa patch nvk/rm "Helios process policy", which Zink reaches through
 * nvk_helios_process_allowed()). Before this header the two kept their own
 * copies, and NVK's missed the "desktop follows DWM" rule: under a DWM on NVK,
 * Zink (OpenGL) apps stayed on Venus, whose surfaces an NVK DWM shows blank.
 *
 * Registry: HKLM\SOFTWARE\Helios (64-bit view from WoW64 too).
 *   Icd               REG_SZ    "venus": the transition default (D3D11/GL/Vulkan)
 *   NvkAllowList      REG_SZ    exe names (';'-separated) that go to NVK anyway
 *   NvkDenyList       REG_SZ    replaces the built-in deny-list; always wins
 *   NvkDefaults       REG_DWORD 1: the measured-safe names below go to NVK
 *   DwmIcd            REG_SZ    "nvk": DWM composes on NVK (the UMD's lever)
 *   DesktopFollowsDwm REG_DWORD 0: turn the rule below off (default 1)
 * Marker: the named event Local\HeliosDwmOnNvk, held by a DWM that chose NVK
 * (created by the D3D11 UMD in dwm.exe, dies with it).
 *
 * Only the rules every API shares live here. Per-API levers stay with their
 * callers: HELIOS_ICD / HELIOS_ICD12, DwmIcd and its crash-loop guard in
 * dwm.exe, D3D12's Nvk12 / NvkDenyList12 and its NVK-by-default.
 */
#ifndef HELIOS_ICD_POLICY_H
#define HELIOS_ICD_POLICY_H

#ifdef _WIN32

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

#define HELIOS_POLICY_DWM_MARKER "Local\\HeliosDwmOnNvk"

static inline int
helios_policy_reg_sz(const char *name, char *out, DWORD cap)
{
   out[0] = 0;
   DWORD size = cap;
   return RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", name,
                       RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, NULL, out,
                       &size) == ERROR_SUCCESS && out[0];
}

static inline int
helios_policy_reg_dword(const char *name, DWORD *out)
{
   DWORD size = sizeof(*out);
   return RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", name,
                       RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, NULL, out,
                       &size) == ERROR_SUCCESS;
}

/* `list` holds names separated by ';' (spaces around names ignored); the
 * match is case-insensitive. */
static inline int
helios_policy_list_has(const char *list, const char *exe)
{
   const size_t len = strlen(exe);
   const char *p = list;
   while (*p) {
      while (*p == ';' || *p == ' ' || *p == '\t')
         p++;
      const char *start = p;
      while (*p && *p != ';')
         p++;
      const char *end = p;
      while (end > start && (end[-1] == ' ' || end[-1] == '\t'))
         end--;
      if ((size_t)(end - start) == len && _strnicmp(start, exe, len) == 0)
         return 1;
   }
   return 0;
}

/* Executables that stay on Venus unless NvkDenyList replaces this list or
 * NvkAllowList / NvkDefaults takes a name out of it. Every entry is there
 * because of surface sharing between an NVK and a Venus process
 * (docs/dwm-on-nvk.md 4.2.1). Lower case. */
static const char *const helios_policy_builtin_deny[] = {
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
   "cefsharp.browsersubprocess.exe",
};

/* Deny-list entries measured to work on NVK under a Venus DWM
 * (docs/dwm-on-nvk.md 4.2.1a): with NvkDefaults=1 they go to NVK even under
 * Icd=venus, as if NvkAllowList named them; NvkDenyList still wins. */
static const char *const helios_policy_nvk_default_names[] = {
   "csrss.exe", "winlogon.exe", "fontdrvhost.exe", "rdpclip.exe",
   "taskmgr.exe", "mmc.exe", "startmenuexperiencehost.exe",
   "systemsettings.exe", "applicationframehost.exe",
};

static inline int
helios_policy_explicitly_denied(const char *exe)
{
   char deny[4096];
   return helios_policy_reg_sz("NvkDenyList", deny, sizeof(deny)) &&
          helios_policy_list_has(deny, exe);
}

static inline int
helios_policy_nvk_default(const char *exe)
{
   DWORD on = 0;
   if (!helios_policy_reg_dword("NvkDefaults", &on) || on == 0)
      return 0;
   for (size_t i = 0; i < sizeof(helios_policy_nvk_default_names) / sizeof(helios_policy_nvk_default_names[0]); i++) {
      if (_stricmp(helios_policy_nvk_default_names[i], exe) == 0)
         return 1;
   }
   return 0;
}

/* NvkAllowList or NvkDefaults (not explicitly denied) names `exe`. */
static inline int
helios_policy_allowed(const char *exe)
{
   char allow[4096];
   if (helios_policy_reg_sz("NvkAllowList", allow, sizeof(allow)) &&
       helios_policy_list_has(allow, exe))
      return 1;
   return helios_policy_nvk_default(exe) && !helios_policy_explicitly_denied(exe);
}

/* The deny-lists keep `exe` on Venus (NvkAllowList / NvkDefaults first, then
 * NvkDenyList if set, else the built-in list). *why gets a reason. */
static inline int
helios_policy_denied(const char *exe, const char **why)
{
   if (helios_policy_allowed(exe))
      return 0;
   char deny[4096];
   if (helios_policy_reg_sz("NvkDenyList", deny, sizeof(deny))) {
      if (why)
         *why = "NvkDenyList names this executable";
      return helios_policy_list_has(deny, exe);
   }
   for (size_t i = 0; i < sizeof(helios_policy_builtin_deny) / sizeof(helios_policy_builtin_deny[0]); i++) {
      if (_stricmp(helios_policy_builtin_deny[i], exe) == 0) {
         if (why)
            *why = "built-in deny-list (compositor, shell or interop-heavy app)";
         return 1;
      }
   }
   return 0;
}

/* "The desktop follows DWM" (docs/dwm-on-nvk.md 4.2.0): DwmIcd=nvk and the
 * running DWM holds the marker, unless DesktopFollowsDwm=0. A sandboxed
 * process that may not open the marker (ERROR_ACCESS_DENIED) still finds it. */
static inline int
helios_policy_desktop_follows_dwm(void)
{
   DWORD follow = 1;
   helios_policy_reg_dword("DesktopFollowsDwm", &follow);
   if (follow == 0)
      return 0;
   char dwm[16];
   if (!helios_policy_reg_sz("DwmIcd", dwm, sizeof(dwm)) || _stricmp(dwm, "nvk") != 0)
      return 0;
   HANDLE h = OpenEventA(SYNCHRONIZE, FALSE, HELIOS_POLICY_DWM_MARKER);
   if (h) {
      CloseHandle(h);
      return 1;
   }
   return GetLastError() == ERROR_ACCESS_DENIED;
}

/* The shared decision for a process that is not dwm.exe and has no
 * per-API override: 1 = NVK, 0 = Venus. `exe` is the executable's base name.
 *   1. the desktop follows an NVK DWM: NVK unless NvkDenyList names it;
 *   2. Icd=venus keeps it on Venus unless NvkAllowList / NvkDefaults names it;
 *   3. the deny-lists.
 * `icd_applies` = 0 skips step 2 (D3D12, whose default is NVK). */
static inline int
helios_policy_decide(const char *exe, int icd_applies, const char **why)
{
   const char *dummy;
   if (!why)
      why = &dummy;
   *why = NULL;
   if (_stricmp(exe, "dwm.exe") != 0 && helios_policy_desktop_follows_dwm()) {
      if (helios_policy_explicitly_denied(exe)) {
         *why = "DWM is on NVK, but NvkDenyList names this executable";
         return 0;
      }
      *why = "DWM is on NVK (DwmIcd=nvk, marker set): the desktop follows, "
             "Icd and the built-in deny-list do not apply";
      return 1;
   }
   if (icd_applies && !helios_policy_allowed(exe)) {
      char mode[16];
      if (helios_policy_reg_sz("Icd", mode, sizeof(mode)) && _stricmp(mode, "venus") == 0) {
         *why = "HKLM\\SOFTWARE\\Helios!Icd=venus";
         return 0;
      }
   }
   if (helios_policy_denied(exe, why))
      return 0;
   return 1;
}

#ifdef __cplusplus
}
#endif

#endif /* _WIN32 */
#endif /* HELIOS_ICD_POLICY_H */

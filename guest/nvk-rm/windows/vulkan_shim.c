/* SPDX-License-Identifier: MIT */
/*
 * vulkan-1.dll shim: run an unmodified Vulkan application (DXVK in Heaven,
 * say) on NVK-on-RM without registering the ICD with the system.
 *
 * Dropped next to the application's .exe as vulkan-1.dll, it is found before
 * the system loader (application directory first in the DLL search order).
 * It loads the real loader from the system directory and forwards
 * everything to it, except vkCreateInstance, where it chains
 * VK_LUNARG_direct_driver_loading in EXCLUSIVE mode with vulkan_nouveau.dll
 * (taken from next to the shim). So the loader still does WSI surfaces,
 * layers and enumeration, but sees only NVK, even in an elevated process
 * where it ignores VK_DRIVER_FILES / VK_ICD_FILENAMES.
 *
 * Only the entry points DXVK resolves with GetProcAddress are exported
 * (vkGetInstanceProcAddr, plus the global commands for other callers);
 * everything else is reached through vkGetInstanceProcAddr.
 *
 * Environment:
 *   NVK_SHIM_DRIVER  ICD DLL to load instead of <shim dir>\vulkan_nouveau.dll
 *   NVK_SHIM_OFF=1   pass vkCreateInstance through untouched (system drivers)
 *   NVK_SHIM_LOG     file to append a line per vkCreateInstance to
 *   NVK_SHIM_FRAMES  CSV of vkQueuePresentKHR times (see shim_QueuePresentKHR)
 *
 * NVK itself still needs NVK_RM=1 in the environment.
 *
 * Build (x86 for 32-bit apps such as Heaven, or x86_64):
 *   i686-w64-mingw32-gcc -O2 -shared -o vulkan-1.dll vulkan_shim.c \
 *       vulkan_shim.def -I<Vulkan-Headers>/include -static-libgcc
 */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>
#include <vulkan/vulkan.h>

static HMODULE real_loader;
static PFN_vkGetInstanceProcAddr real_gipa;
static PFN_vkGetInstanceProcAddrLUNARG nvk_gipa;
static INIT_ONCE init_once = INIT_ONCE_STATIC_INIT;
static char shim_dir[MAX_PATH];

static void shim_log(const char *fmt, ...)
{
   const char *path = getenv("NVK_SHIM_LOG");
   if (!path || !*path)
      return;
   FILE *f = fopen(path, "a");
   if (!f)
      return;
   va_list ap;
   va_start(ap, fmt);
   vfprintf(f, fmt, ap);
   va_end(ap);
   fputc('\n', f);
   fclose(f);
}

static BOOL CALLBACK shim_init(PINIT_ONCE once, PVOID param, PVOID *ctx)
{
   (void)once; (void)param; (void)ctx;
   char sysdir[MAX_PATH], path[MAX_PATH + 32];

   /* GetSystemDirectory in a WoW64 process is SysWOW64 (file system
    * redirection), i.e. the loader matching our bitness. */
   UINT n = GetSystemDirectoryA(sysdir, sizeof(sysdir));
   if (n == 0 || n >= sizeof(sysdir))
      return TRUE;
   snprintf(path, sizeof(path), "%s\\vulkan-1.dll", sysdir);
   real_loader = LoadLibraryA(path);
   if (real_loader)
      real_gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(real_loader, "vkGetInstanceProcAddr");
   shim_log("nvk-shim: system loader %s -> %p", path, (void *)real_gipa);

   HMODULE self = NULL;
   GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                      (LPCSTR)(void *)shim_init, &self);
   n = GetModuleFileNameA(self, shim_dir, sizeof(shim_dir));
   if (n && n < sizeof(shim_dir)) {
      char *slash = strrchr(shim_dir, '\\');
      if (slash)
         *slash = '\0';
   }

   const char *off = getenv("NVK_SHIM_OFF");
   if (off && *off == '1')
      return TRUE;

   const char *drv = getenv("NVK_SHIM_DRIVER");
   if (drv && *drv)
      snprintf(path, sizeof(path), "%s", drv);
   else
      snprintf(path, sizeof(path), "%s\\vulkan_nouveau.dll", shim_dir);
   /* LOAD_WITH_ALTERED_SEARCH_PATH: the ICD's own imports (librmclient.dll is
    * loaded at runtime by name) resolve from its directory too. */
   HMODULE icd = LoadLibraryExA(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
   if (icd)
      nvk_gipa = (PFN_vkGetInstanceProcAddrLUNARG)(void *)GetProcAddress(icd, "vk_icdGetInstanceProcAddr");
   shim_log("nvk-shim: ICD %s -> %p (error %lu)", path, (void *)nvk_gipa,
            icd ? 0ul : (unsigned long)GetLastError());
   return TRUE;
}

static void ensure_init(void)
{
   InitOnceExecuteOnce(&init_once, shim_init, NULL, NULL);
}

static VKAPI_ATTR VkResult VKAPI_CALL
shim_CreateInstance(const VkInstanceCreateInfo *pCreateInfo,
                    const VkAllocationCallbacks *pAllocator, VkInstance *pInstance)
{
   ensure_init();
   PFN_vkCreateInstance create =
      real_gipa ? (PFN_vkCreateInstance)real_gipa(NULL, "vkCreateInstance") : NULL;
   if (!create)
      return VK_ERROR_INITIALIZATION_FAILED;
   if (!nvk_gipa)
      return create(pCreateInfo, pAllocator, pInstance);

   VkInstanceCreateInfo ici = *pCreateInfo;
   VkDirectDriverLoadingInfoLUNARG info = {
      .sType = VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_INFO_LUNARG,
      .pfnGetInstanceProcAddr = nvk_gipa,
   };
   VkDirectDriverLoadingListLUNARG list = {
      .sType = VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_LIST_LUNARG,
      .pNext = ici.pNext,
      .mode = VK_DIRECT_DRIVER_LOADING_MODE_EXCLUSIVE_LUNARG,
      .driverCount = 1,
      .pDrivers = &info,
   };
   ici.pNext = &list;

   uint32_t n = ici.enabledExtensionCount;
   const char **exts = calloc(n + 1, sizeof(*exts));
   if (!exts)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   for (uint32_t i = 0; i < n; i++)
      exts[i] = ici.ppEnabledExtensionNames[i];
   exts[n] = VK_LUNARG_DIRECT_DRIVER_LOADING_EXTENSION_NAME;
   ici.enabledExtensionCount = n + 1;
   ici.ppEnabledExtensionNames = exts;

   VkResult r = create(&ici, pAllocator, pInstance);
   shim_log("nvk-shim: vkCreateInstance (%u app extensions, direct NVK) -> %d", n, (int)r);
   free(exts);
   return r;
}

/* ---- present timing (NVK_SHIM_FRAMES) ------------------------------------
 * An application-local dxgi.dll (DXVK) emits no DXGI ETW events, so
 * PresentMon cannot see its frames. With NVK_SHIM_FRAMES=file the shim
 * appends one line per vkQueuePresentKHR: milliseconds since the first
 * present (QueryPerformanceCounter, taken before the call) and the call's
 * own duration in ms. One device / one presenting thread is assumed. */
static PFN_vkQueuePresentKHR real_queue_present;
static PFN_vkGetDeviceProcAddr real_gdpa;
static FILE *frames_file;
static LARGE_INTEGER qpc_freq, qpc_first;
static unsigned frames_count;

static VKAPI_ATTR VkResult VKAPI_CALL
shim_QueuePresentKHR(VkQueue queue, const VkPresentInfoKHR *pPresentInfo)
{
   LARGE_INTEGER t0, t1;
   QueryPerformanceCounter(&t0);
   VkResult r = real_queue_present(queue, pPresentInfo);
   if (frames_file) {
      QueryPerformanceCounter(&t1);
      if (!frames_count++)
         qpc_first = t0;
      fprintf(frames_file, "%.3f,%.3f\n",
              (double)(t0.QuadPart - qpc_first.QuadPart) * 1000.0 / (double)qpc_freq.QuadPart,
              (double)(t1.QuadPart - t0.QuadPart) * 1000.0 / (double)qpc_freq.QuadPart);
      fflush(frames_file); /* a killed process keeps every line */
   }
   return r;
}

static PFN_vkVoidFunction wrap_present(PFN_vkVoidFunction fn)
{
   const char *path = getenv("NVK_SHIM_FRAMES");
   if (!fn || !path || !*path)
      return fn;
   if (!frames_file) {
      frames_file = fopen(path, "w");
      QueryPerformanceFrequency(&qpc_freq);
      if (frames_file)
         fprintf(frames_file, "t_ms,present_ms\n");
   }
   real_queue_present = (PFN_vkQueuePresentKHR)fn;
   return (PFN_vkVoidFunction)shim_QueuePresentKHR;
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
shim_GetDeviceProcAddr(VkDevice device, const char *pName)
{
   PFN_vkVoidFunction fn = real_gdpa(device, pName);
   if (pName && !strcmp(pName, "vkQueuePresentKHR"))
      return wrap_present(fn);
   if (pName && !strcmp(pName, "vkGetDeviceProcAddr"))
      return (PFN_vkVoidFunction)shim_GetDeviceProcAddr;
   return fn;
}

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
vkGetInstanceProcAddr(VkInstance instance, const char *pName)
{
   ensure_init();
   if (pName && !strcmp(pName, "vkCreateInstance"))
      return (PFN_vkVoidFunction)shim_CreateInstance;
   if (pName && !strcmp(pName, "vkGetInstanceProcAddr"))
      return (PFN_vkVoidFunction)vkGetInstanceProcAddr;
   PFN_vkVoidFunction fn = real_gipa ? real_gipa(instance, pName) : NULL;
   if (fn && pName && !strcmp(pName, "vkGetDeviceProcAddr")) {
      real_gdpa = (PFN_vkGetDeviceProcAddr)fn;
      return (PFN_vkVoidFunction)shim_GetDeviceProcAddr;
   }
   if (pName && !strcmp(pName, "vkQueuePresentKHR"))
      return wrap_present(fn);
   return fn;
}

VKAPI_ATTR VkResult VKAPI_CALL
vkCreateInstance(const VkInstanceCreateInfo *pCreateInfo,
                 const VkAllocationCallbacks *pAllocator, VkInstance *pInstance)
{
   return shim_CreateInstance(pCreateInfo, pAllocator, pInstance);
}

VKAPI_ATTR VkResult VKAPI_CALL
vkEnumerateInstanceExtensionProperties(const char *pLayerName, uint32_t *pCount,
                                       VkExtensionProperties *pProps)
{
   PFN_vkEnumerateInstanceExtensionProperties f = (PFN_vkEnumerateInstanceExtensionProperties)
      vkGetInstanceProcAddr(NULL, "vkEnumerateInstanceExtensionProperties");
   return f ? f(pLayerName, pCount, pProps) : VK_ERROR_INITIALIZATION_FAILED;
}

VKAPI_ATTR VkResult VKAPI_CALL
vkEnumerateInstanceLayerProperties(uint32_t *pCount, VkLayerProperties *pProps)
{
   PFN_vkEnumerateInstanceLayerProperties f = (PFN_vkEnumerateInstanceLayerProperties)
      vkGetInstanceProcAddr(NULL, "vkEnumerateInstanceLayerProperties");
   return f ? f(pCount, pProps) : VK_ERROR_INITIALIZATION_FAILED;
}

VKAPI_ATTR VkResult VKAPI_CALL
vkEnumerateInstanceVersion(uint32_t *pApiVersion)
{
   PFN_vkEnumerateInstanceVersion f = (PFN_vkEnumerateInstanceVersion)
      vkGetInstanceProcAddr(NULL, "vkEnumerateInstanceVersion");
   if (!f) {
      *pApiVersion = VK_API_VERSION_1_0;
      return VK_SUCCESS;
   }
   return f(pApiVersion);
}

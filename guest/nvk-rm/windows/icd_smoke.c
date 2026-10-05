/*
 * Loads vulkan_nouveau.dll the way the Vulkan loader does (no loader
 * needed), creates an instance and enumerates physical devices.
 *
 *   x86_64-w64-mingw32-gcc -O1 -I<Vulkan headers> icd_smoke.c -o icd_smoke.exe
 *   icd_smoke.exe [path\to\vulkan_nouveau.dll]
 *
 * With librmclient.dll next to the driver and NVK_RM=1, NVK asks RM for
 * GPUs. With the stub Windows transport crm_open fails with -ENOSYS, so the
 * expected result there is "0 physical devices" (set MESA_DEBUG / NVK_DEBUG
 * for logs); a real transport should list the GPU.
 */
#include <stdio.h>
#include <stdlib.h>
#include <windows.h>

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

typedef VkResult (VKAPI_PTR *PFN_negotiate)(uint32_t *version);

int main(int argc, char **argv)
{
    const char *path = argc > 1 ? argv[1] : "vulkan_nouveau.dll";
    HMODULE dll = LoadLibraryA(path);
    if (!dll) {
        printf("LoadLibrary(%s) failed: %lu\n", path, (unsigned long)GetLastError());
        return 1;
    }

    PFN_negotiate negotiate =
        (PFN_negotiate)(void *)GetProcAddress(dll, "vk_icdNegotiateLoaderICDInterfaceVersion");
    PFN_vkGetInstanceProcAddr gipa =
        (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(dll, "vk_icdGetInstanceProcAddr");
    if (!negotiate || !gipa) {
        printf("missing ICD entry points\n");
        return 1;
    }

    uint32_t version = 7;
    VkResult r = negotiate(&version);
    printf("vk_icdNegotiateLoaderICDInterfaceVersion: %d, interface version %u\n", r, version);

    PFN_vkEnumerateInstanceExtensionProperties enum_ext =
        (PFN_vkEnumerateInstanceExtensionProperties)
            gipa(VK_NULL_HANDLE, "vkEnumerateInstanceExtensionProperties");
    uint32_t n_ext = 0;
    enum_ext(NULL, &n_ext, NULL);
    VkExtensionProperties *exts = calloc(n_ext, sizeof(*exts));
    enum_ext(NULL, &n_ext, exts);
    printf("%u instance extensions:", n_ext);
    for (uint32_t i = 0; i < n_ext; i++)
        printf(" %s", exts[i].extensionName);
    printf("\n");
    free(exts);

    PFN_vkCreateInstance create =
        (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "icd_smoke",
        .apiVersion = VK_API_VERSION_1_3,
    };
    VkInstanceCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance inst = VK_NULL_HANDLE;
    r = create(&ci, NULL, &inst);
    printf("vkCreateInstance: %d\n", r);
    if (r != VK_SUCCESS)
        return 1;

    PFN_vkEnumeratePhysicalDevices enum_pd =
        (PFN_vkEnumeratePhysicalDevices)gipa(inst, "vkEnumeratePhysicalDevices");
    PFN_vkGetPhysicalDeviceProperties props =
        (PFN_vkGetPhysicalDeviceProperties)gipa(inst, "vkGetPhysicalDeviceProperties");
    PFN_vkDestroyInstance destroy =
        (PFN_vkDestroyInstance)gipa(inst, "vkDestroyInstance");

    uint32_t count = 0;
    r = enum_pd(inst, &count, NULL);
    printf("vkEnumeratePhysicalDevices: %d, %u physical devices\n", r, count);
    if (r == VK_SUCCESS && count > 0) {
        VkPhysicalDevice *pds = calloc(count, sizeof(*pds));
        enum_pd(inst, &count, pds);
        for (uint32_t i = 0; i < count; i++) {
            VkPhysicalDeviceProperties p;
            props(pds[i], &p);
            printf("  %u: %s (0x%04x:0x%04x)\n", i, p.deviceName, p.vendorID, p.deviceID);
        }
        free(pds);
    }

    destroy(inst, NULL);
    printf("vkDestroyInstance: done\n");
    return 0;
}

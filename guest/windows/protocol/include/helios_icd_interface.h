/* The backend-neutral interface between the Helios D3D UMDs and the Vulkan ICD
 * they run on (guest/windows/docs/dxvk-on-nvk.md, section 3.2, decision D2).
 *
 * One export per ICD:
 *
 *    VkResult helios_icd_interface_v2(uint32_t version, struct helios_icd_api *out);
 *
 * NVK on RM (vulkan_nouveau.dll, guest/nvk-rm/patches-windows) implements it
 * natively. The Venus ICD (vulkan_virtio.dll) does not need to: the UMD builds
 * the same table from the older helios_venus_* exports, so a new UMD keeps
 * working with an older Venus ICD (umd_common/bridge/bridge_icd_backend.cpp).
 *
 * The KMD resource id stays the one buffer name across components (D1): every
 * entry that names a buffer to WDDM, DWM or the scanout does it with a resource
 * id. Venus makes one per exportable VkDeviceMemory; NVK mints one through the
 * KMD's HELIOS_ESCAPE_FOREIGN_RESOURCE IMPORT_RM (protocol/src/foreign.rs) and
 * reports the layout that goes with it, because a Venus importer (DWM) and the
 * scanout cannot learn it anywhere else.
 *
 * Rules:
 *  - `version` in = the version the caller was built against; the ICD fills
 *    at most `out->size` bytes it knows, sets `version`/`size` to what it
 *    filled, and returns VK_ERROR_INCOMPATIBLE_DRIVER for a version it cannot
 *    serve. Entries are only ever appended; a NULL entry is "not supported".
 *  - Every entry takes handles of the ICD that returned the table, created by
 *    the instance/device the caller made through that ICD's
 *    vk_icdGetInstanceProcAddr (no loader trampolines), and is thread-safe.
 *  - Calling convention: the platform's default C convention (cdecl on x86).
 *
 * C header, included by Mesa (a verbatim copy) and by the UMD bridges. */
#ifndef HELIOS_ICD_INTERFACE_H
#define HELIOS_ICD_INTERFACE_H

#include <stdint.h>
#include <vulkan/vulkan_core.h>

#ifdef __cplusplus
extern "C" {
#endif

#define HELIOS_ICD_INTERFACE_EXPORT "helios_icd_interface_v2"
/* The export keeps its name; version 3 only appends entries (and caps 4..6),
 * version 4 appends memory_res_plane1 (and cap 8). */
#define HELIOS_ICD_INTERFACE_VERSION 4u

enum helios_icd_backend {
   HELIOS_ICD_BACKEND_VENUS = 1,
   HELIOS_ICD_BACKEND_NVK_RM = 2,
};

/* helios_icd_api.caps, as of the call that returned the table. */
/* memory_res_id can mint resource ids now (Venus: always; NVK: the KMD serves
 * IMPORT_RM, i.e. FOREIGN_RESOURCE caps bit CAP_RM_IMPORT). */
#define HELIOS_ICD_CAP_RES_ID (1u << 0)
/* The resource ids carry a layout the importer must use (NVK: always). */
#define HELIOS_ICD_CAP_LAYOUT (1u << 1)
/* scanout_present works (NVK: the KMD's foreign scanout source, ops 9..11). */
#define HELIOS_ICD_CAP_SCANOUT (1u << 2)
/* The producer interface (escape 0x13 streams keyed on a Venus timeline)
 * exists. NVK: no; presents are CPU-complete (value 0 markers). */
#define HELIOS_ICD_CAP_PRODUCER (1u << 3)
/* Version 3: queue_rm_fence / rm_fence_wait / rm_fence_close /
 * scanout_present_fenced work (NVK: librmclient with RM fences and a host with
 * DRM fences, guest/windows/docs/rm-fence-marker.md, dxvk-on-nvk S4). */
#define HELIOS_ICD_CAP_RM_FENCE (1u << 4)
/* scanout_present_fenced hands the fence to the KMD, which flips when it fires
 * (QueryCaps HELIOS_NVRM_CAP_SCANOUT_FENCE). Without it a thread in the ICD
 * waits for the fence; the caller returns at once either way. */
#define HELIOS_ICD_CAP_SCANOUT_FENCE_KMD (1u << 5)
/* The KMD takes RM fences in WDDM present markers (HERF/HEPR tail, HE12 v4;
 * QueryCaps HELIOS_NVRM_CAP_PRESENT_FENCE, protocol/include/helios_rm_fence.h). */
#define HELIOS_ICD_CAP_PRESENT_FENCE_KMD (1u << 6)
/* Surfaces another process of this backend made can be opened
 * (guest/windows/docs/shared-surfaces.md): HELIOS_STRUCTURE_TYPE_IMPORT_
 * MEMORY_RESOURCE_INFO is accepted. NVK: librmclient has
 * crm_win_rm_resource_import; whether the KMD and host serve it shows only
 * when it is tried. */
#define HELIOS_ICD_CAP_SHARED_IMPORT (1u << 7)
/* Version 4: memory_res_id mints ids for every format of the shared-format
 * table (guest/windows/docs/shared-formats.md: 8/16/64 bpp, 10:10:10:2, YUYV,
 * NV12/P010/P016 with plane 1), not only 32 bpp RGB (NVK: the KMD advertises
 * HELIOS_FOREIGN_CAP_LAYOUT_FORMATS). A two-plane id's plane 1 comes from
 * memory_res_plane1, and goes into the WDDM trailer (version 2, 144 bytes). */
#define HELIOS_ICD_CAP_LAYOUT_FORMATS (1u << 8)

/* DRM fourcc / modifier values used below (drm_fourcc.h). */
#define HELIOS_DRM_FORMAT_XRGB8888 0x34325258u
#define HELIOS_DRM_FORMAT_ARGB8888 0x34325241u
#define HELIOS_DRM_FORMAT_XBGR8888 0x34324258u
#define HELIOS_DRM_FORMAT_ABGR8888 0x34324241u
#define HELIOS_DRM_FORMAT_MOD_LINEAR 0ull
/* DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, h) on GB20x: | h, h 0..5 */
#define HELIOS_DRM_FORMAT_MOD_NVIDIA_BL_GB20X 0x0300000000606010ull

/* What the memory behind a resource id holds: plane 0 of a 2D image. The same
 * fields as helios_foreign_layout (protocol/include/helios_foreign.h) plus the
 * object size; zero `modifier`/`stride` with HELIOS_ICD_CAP_LAYOUT clear means
 * "the importer infers it" (Venus). */
struct helios_icd_layout {
   uint64_t size;     /* bytes of the exported object (>= the image) */
   uint64_t modifier; /* DRM_FORMAT_MOD_* */
   uint32_t width;
   uint32_t height;
   uint32_t stride;   /* row pitch, bytes */
   uint32_t offset;   /* plane 0 offset, bytes */
   uint32_t fourcc;   /* DRM_FORMAT_*, 0 for formats without one */
   uint32_t memory_type_index;
};

/* Chained into VkMemoryAllocateInfo by the UMD's DXVK for memory that will
 * need a resource id (a dedicated allocation of a WDDM-backed image). NVK then
 * allocates it standalone in VRAM, uncompressed, and records the image's
 * layout for the export. Venus ignores it (its own OPAQUE_FD/DMA_BUF export
 * request does the same job there). The value sits next to Venus' private
 * VkImportMemoryResourceInfoMESA (1000384002). */
#define HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO ((VkStructureType)1000384003)
struct helios_export_memory_resource_info {
   VkStructureType sType; /* HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO */
   const void *pNext;
   uint32_t flags;        /* 0 */
   uint32_t reserved;     /* 0 */
};

/* Chained into VkMemoryAllocateInfo by the opener's DXVK, together with a
 * VkMemoryDedicatedAllocateInfo naming the image that will be bound, when a
 * D3D app on NVK opens a surface another NVK process made (a foreign resource
 * id the KMD let this device open): NVK maps the creator's memory instead of
 * allocating. The resource id is the only name that crosses processes; NVK
 * asks the host to make its memory a GEM object of NVK's own render node
 * (backend RmResourceImport, docs/VENUS.md "RM-export resources in a second
 * process") and imports that into its own RM client
 * (DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY, OS_UNIX_IMPORT_OBJECT_FROM_FD). The
 * image must have the layout the KMD recorded for the resource (the open's
 * HeliosWddmAllocLayout trailer): NVK on Windows has no
 * VK_EXT_image_drm_format_modifier, so it checks instead of building one.
 * The memory takes the resource id as its own (memory_res_id answers it;
 * nothing is released with the memory: the WDDM allocation owns the id). */
#define HELIOS_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO ((VkStructureType)1000384004)
struct helios_import_memory_resource_info {
   VkStructureType sType; /* HELIOS_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO */
   const void *pNext;
   uint32_t resource_id;  /* the opened allocation's foreign resource id */
   uint32_t flags;        /* 0 */
   uint64_t size;         /* the object's size from the open (blob_size), 0: unknown */
   uint64_t modifier;     /* DRM_FORMAT_MOD_* the image must have */
   uint32_t stride;       /* plane 0 row pitch the image must have */
   uint32_t offset;       /* plane 0 offset the image must have */
   /* With HELIOS_IMPORT_MEMORY_RESOURCE_FLAG_PLANE1 in `flags` only (an older
    * caller's struct ends above): plane 1 of a two-plane surface, from the
    * open's version-2 trailer. */
   uint64_t plane1_modifier;
   uint32_t plane1_stride;
   uint32_t plane1_offset;
};
/* helios_import_memory_resource_info.flags: the plane1_* fields are there. */
#define HELIOS_IMPORT_MEMORY_RESOURCE_FLAG_PLANE1 (1u << 0)

/* Plane 1 of a two-plane image behind a resource id (memory_res_plane1). */
struct helios_icd_plane {
   uint64_t modifier; /* DRM_FORMAT_MOD_* of plane 1 */
   uint32_t stride;   /* row pitch, bytes */
   uint32_t offset;   /* bytes from the start of the object */
};

struct helios_icd_api {
   uint32_t version; /* HELIOS_ICD_INTERFACE_VERSION */
   uint32_t size;    /* sizeof(struct helios_icd_api) the ICD filled */
   uint32_t backend; /* enum helios_icd_backend */
   uint32_t caps;    /* HELIOS_ICD_CAP_* */

   /* The Venus context that resource ids made for `instance` are attached to
    * (stamped into WDDM allocation private data as ctx_id). NVK: the holder
    * context it created for IMPORT_RM, 0 before the first import. */
   uint32_t (*ctx_id)(VkInstance instance);

   /* The resource id behind `memory`, made on first use and cached on the
    * memory object for its lifetime. `image` is the image bound to it at
    * offset 0 (NVK needs it for the layout; Venus ignores it). Fills `layout`
    * (may be NULL). VK_ERROR_FEATURE_NOT_PRESENT when ids cannot be made now
    * (NVK: IMPORT_RM not served), VK_ERROR_FORMAT_NOT_SUPPORTED for an image
    * the importers cannot take (NVK: a format outside the shared-format table,
    * or not 32 bpp RGB without HELIOS_ICD_CAP_LAYOUT_FORMATS; 3D tiling,
    * suballocated). */
   VkResult (*memory_res_id)(VkDevice device, VkDeviceMemory memory, VkImage image,
                             uint32_t *res_id, struct helios_icd_layout *layout);

   /* Exact allocation size and memory type of `memory` (Venus: what an
    * importer must repeat; NVK: the RM object size). */
   VkBool32 (*memory_alloc_info)(VkDeviceMemory memory, uint64_t *alloc_size,
                                 uint32_t *memory_type_index);

   /* A WDDM allocation adopted the resource id: the ICD stops releasing it when
    * the memory is freed. Returns the id, 0 if there was none. */
   uint32_t (*transfer_ownership)(VkDeviceMemory memory);

   /* Present `image` (bound to `memory`) on scanout 0 through the KMD's foreign
    * scanout source, after the GPU finished rendering it (the caller waited).
    * NULL or VK_ERROR_FEATURE_NOT_PRESENT without HELIOS_ICD_CAP_SCANOUT. */
   VkResult (*scanout_present)(VkDevice device, VkDeviceMemory memory, VkImage image);
   /* Give scanout 0 back to the desktop. */
   void (*scanout_release)(VkDevice device);

   /* The producer interface (helios_producer_abi.h), or NULL. */
   const void *(*producer)(void);

   /* ---- version 3 (size covers them; NULL = not supported) ---------------
    * RM fences (HELIOS_ICD_CAP_RM_FENCE). A fence is a backend handle of the
    * NVRM escape device the ICD's librmclient opened in this process: it fires
    * once when the GPU has finished the work it stands for (or after the host's
    * 5 s timeout). */

   /* Signal the device's present timeline on `queue` after everything
    * submitted to it so far and make a fence for that point. The caller holds
    * the queue as vkQueueSubmit requires (DXVK: lockSubmission). *fence_handle
    * is the caller's: hand it to the KMD (scanout_present_fenced, a present
    * marker tail) or give it back with rm_fence_close. *value (may be NULL) is
    * the timeline value, diagnostic. VK_ERROR_FEATURE_NOT_PRESENT: no fences
    * here; wait on the CPU as before. */
   VkResult (*queue_rm_fence)(VkDevice device, VkQueue queue,
                              uint32_t *fence_handle, uint64_t *value);
   /* Wait for a fence the caller still owns. VK_SUCCESS fired, VK_TIMEOUT. */
   VkResult (*rm_fence_wait)(VkDevice device, uint32_t fence_handle,
                             uint64_t timeout_ns);
   /* Close a fence the caller still owns (fired or not). */
   void (*rm_fence_close)(VkDevice device, uint32_t fence_handle);
   /* scanout_present, sent when `fence_handle` fires instead of after a CPU
    * wait; returns at once. TAKES the fence in every case (success or not).
    * Image reuse: with N >= 3 images, do not render into the image of present
    * P before the fence of present P+1 fired and P+1's call returned
    * (rm-fence-marker.md). */
   VkResult (*scanout_present_fenced)(VkDevice device, VkDeviceMemory memory,
                                      VkImage image, uint32_t fence_handle);

   /* ---- version 4 (size covers it; NULL = not supported) ----------------
    * Plane 1 of `image` as its resource id records it (call after
    * memory_res_id succeeded for the same memory and image). VK_SUCCESS and
    * *plane1 filled for a two-plane image; VK_ERROR_FORMAT_NOT_SUPPORTED for a
    * single-plane one (nothing to add to the version-1 trailer). */
   VkResult (*memory_res_plane1)(VkDevice device, VkImage image,
                                 struct helios_icd_plane *plane1);
};

typedef VkResult (*PFN_helios_icd_interface_v2)(uint32_t version, struct helios_icd_api *out);

#ifdef __cplusplus
}
#endif

#endif /* HELIOS_ICD_INTERFACE_H */

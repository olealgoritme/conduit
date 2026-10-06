/*
 * The minimal subset of NVIDIA's RM user-mode ABI that librmclient uses,
 * copied (types renamed to <stdint.h>, otherwise unchanged in layout) from
 * NVIDIA's open-gpu-kernel-modules 610.57.04:
 *
 *   kernel-open/common/inc/nv-ioctl-numbers.h       NV_ESC_* frontend escapes
 *   src/nvidia/arch/nvalloc/unix/include/nv_escape.h NV_ESC_RM_* escapes
 *   kernel-open/common/inc/nv-ioctl.h                card info, version, fds, events
 *   src/nvidia/arch/nvalloc/unix/include/nv-unix-nvos-params-wrappers.h
 *   src/common/sdk/nvidia/inc/nvos.h                 NVOS* parameter blocks
 *   src/common/sdk/nvidia/inc/class/cl*.h            class numbers, alloc params
 *   src/common/sdk/nvidia/inc/ctrl/ctrl0000, ctrl2080  control commands
 *
 * SPDX-FileCopyrightText: Copyright (c) 1993-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */
#ifndef CRM_NV_IOCTL_DEFS_H
#define CRM_NV_IOCTL_DEFS_H

#include <stdint.h>
#include <stdalign.h>

/* NvP64 / NvU64 fields carry NV_ALIGN_BYTES(8) in NVIDIA's headers. */
#define NV_A8 alignas(8)

/* ---- Escape numbers ---------------------------------------------------- */

#define NV_IOCTL_MAGIC               'F'
#define NV_IOCTL_BASE                200
#define NV_ESC_CARD_INFO             (NV_IOCTL_BASE + 0)
#define NV_ESC_REGISTER_FD           (NV_IOCTL_BASE + 1)
#define NV_ESC_ALLOC_OS_EVENT        (NV_IOCTL_BASE + 6)
#define NV_ESC_FREE_OS_EVENT         (NV_IOCTL_BASE + 7)
#define NV_ESC_CHECK_VERSION_STR     (NV_IOCTL_BASE + 10)

#define NV_ESC_RM_ALLOC_MEMORY       0x27
#define NV_ESC_RM_FREE               0x29
#define NV_ESC_RM_CONTROL            0x2A
#define NV_ESC_RM_ALLOC              0x2B
#define NV_ESC_RM_MAP_MEMORY         0x4E
#define NV_ESC_RM_GET_EVENT_DATA     0x52
#define NV_ESC_RM_UNMAP_MEMORY       0x4F
#define NV_ESC_RM_MAP_MEMORY_DMA     0x57
#define NV_ESC_RM_UNMAP_MEMORY_DMA   0x58

#define NV_MAX_DEVICES               32

/* ---- nv-ioctl.h ---------------------------------------------------------- */

typedef struct {
    uint32_t domain;
    uint8_t  bus;
    uint8_t  slot;
    uint8_t  function;
    uint16_t vendor_id;
    uint16_t device_id;
} nv_pci_info_t;

typedef struct nv_ioctl_card_info {
    uint8_t       valid;           /* NvBool */
    nv_pci_info_t pci_info;
    uint32_t      gpu_id;
    uint16_t      interrupt_line;
    NV_A8 uint64_t reg_address;
    NV_A8 uint64_t reg_size;
    NV_A8 uint64_t fb_address;
    NV_A8 uint64_t fb_size;
    uint32_t      minor_number;
    uint8_t       dev_name[10];
} nv_ioctl_card_info_t;

typedef struct nv_ioctl_alloc_os_event {
    uint32_t hClient;
    uint32_t hDevice;
    uint32_t fd;
    uint32_t Status;
} nv_ioctl_alloc_os_event_t;

typedef nv_ioctl_alloc_os_event_t nv_ioctl_free_os_event_t;

#define NV_RM_API_VERSION_STRING_LENGTH 64

typedef struct nv_ioctl_rm_api_version {
    uint32_t cmd;
    uint32_t reply;
    char     versionString[NV_RM_API_VERSION_STRING_LENGTH];
} nv_ioctl_rm_api_version_t;

#define NV_RM_API_VERSION_CMD_STRICT          0
#define NV_RM_API_VERSION_CMD_RELAXED         '1'
#define NV_RM_API_VERSION_CMD_QUERY           '2'
#define NV_RM_API_VERSION_REPLY_UNRECOGNIZED  0
#define NV_RM_API_VERSION_REPLY_RECOGNIZED    1

typedef struct nv_ioctl_register_fd {
    int ctl_fd;
} nv_ioctl_register_fd_t;

/* ---- nvos.h -------------------------------------------------------------- */

typedef struct {
    uint32_t hRoot;
    uint32_t hObjectParent;
    uint32_t hObjectOld;
    uint32_t status;
} NVOS00_PARAMETERS;

typedef struct {
    uint32_t hRoot;
    uint32_t hObjectParent;
    uint32_t hObjectNew;
    uint32_t hClass;
    NV_A8 uint64_t pAllocParms;
    uint32_t paramsSize;
    uint32_t status;
} NVOS21_PARAMETERS;

/* nvos.h NVOS02_PARAMETERS; nv-ioctl.h nv_ioctl_nvos02_parameters_with_fd */
typedef struct {
    uint32_t hRoot;
    uint32_t hObjectParent;
    uint32_t hObjectNew;
    uint32_t hClass;
    uint32_t flags;
    NV_A8 uint64_t pMemory;
    NV_A8 uint64_t limit;
    uint32_t status;
} NVOS02_PARAMETERS;

typedef struct {
    NVOS02_PARAMETERS params;
    int fd;
} nv_ioctl_nvos02_parameters_with_fd;

/* NVOS02_FLAGS_* (nvos.h) */
#define NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS  (0x1u << 4)   /* 7:4 */
#define NVOS02_FLAGS_LOCATION_PCI               (0x0u << 8)   /* 11:8 */
#define NVOS02_FLAGS_COHERENCY_CACHED           (0x1u << 12)  /* 15:12 */
#define NVOS02_FLAGS_MAPPING_NO_MAP             (0x1u << 30)  /* 31:30 */

typedef struct {
    uint32_t hRoot;
    uint32_t hObjectParent;
    uint32_t hObjectNew;
    uint32_t hClass;
    NV_A8 uint64_t pAllocParms;
    NV_A8 uint64_t pRightsRequested;
    uint32_t paramsSize;
    uint32_t flags;
    uint32_t status;
} NVOS64_PARAMETERS;

typedef struct {
    uint32_t hClient;
    uint32_t hObject;
    uint32_t cmd;
    uint32_t flags;
    NV_A8 uint64_t params;
    uint32_t paramsSize;
    uint32_t status;
} NVOS54_PARAMETERS;

typedef struct {
    uint32_t hClient;
    uint32_t hDevice;
    uint32_t hMemory;
    NV_A8 uint64_t offset;
    NV_A8 uint64_t length;
    NV_A8 uint64_t pLinearAddress;
    uint32_t status;
    uint32_t flags;
} NVOS33_PARAMETERS;

/* NVOS33_FLAGS_ACCESS 1:0 */
#define NVOS33_FLAGS_ACCESS_MASK        0x3u
#define NVOS33_FLAGS_ACCESS_READ_WRITE  0x0u
#define NVOS33_FLAGS_ACCESS_READ_ONLY   0x1u
#define NVOS33_FLAGS_ACCESS_WRITE_ONLY  0x2u

typedef struct {
    NVOS33_PARAMETERS params;
    int fd;
} nv_ioctl_nvos33_parameters_with_fd;

typedef struct {
    uint32_t hClient;
    uint32_t hDevice;
    uint32_t hMemory;
    NV_A8 uint64_t pLinearAddress;
    uint32_t status;
    uint32_t flags;
} NVOS34_PARAMETERS;

typedef struct {
    uint32_t hClient;
    uint32_t hDevice;
    uint32_t hDma;
    uint32_t hMemory;
    NV_A8 uint64_t offset;
    NV_A8 uint64_t length;
    uint32_t flags;
    uint32_t flags2;
    uint32_t kindOverride;
    NV_A8 uint64_t dmaOffset;
    uint32_t status;
} NVOS46_PARAMETERS;

typedef struct {
    uint32_t hClient;
    uint32_t hDevice;
    uint32_t hDma;
    uint32_t hMemory;
    uint32_t flags;
    NV_A8 uint64_t dmaOffset;
    NV_A8 uint64_t size;
    uint32_t status;
} NVOS47_PARAMETERS;

typedef struct {
    NV_A8 uint64_t pEvent;     /* NvUnixEvent * */
    uint32_t MoreEvents;
    uint32_t status;
} NVOS41_PARAMETERS;

typedef struct {
    uint32_t hObject;
    uint32_t NotifyIndex;
    uint32_t info32;
    uint16_t info16;
} NvUnixEvent;

/* ---- Classes ------------------------------------------------------------- */

#define NV01_ROOT_CLIENT                  0x00000041
#define NV01_DEVICE_0                     0x00000080
#define NV20_SUBDEVICE_0                  0x00002080
#define NV01_MEMORY_SYSTEM                0x0000003E
#define NV01_MEMORY_LOCAL_USER            0x00000040
#define NV01_MEMORY_SYSTEM_OS_DESCRIPTOR  0x00000071
#define NV01_EVENT_OS_EVENT               0x00000079
#define FERMI_VASPACE_A                   0x000090f1
#define NV01_MEMORY_VIRTUAL               0x00000070
#define NV50_MEMORY_VIRTUAL               0x000050a0

typedef struct NV0005_ALLOC_PARAMETERS {
    uint32_t hParentClient;
    uint32_t hSrcResource;
    uint32_t hClass;
    uint32_t notifyIndex;
    NV_A8 uint64_t data;
} NV0005_ALLOC_PARAMETERS;

typedef struct NV0080_ALLOC_PARAMETERS {
    uint32_t deviceId;
    uint32_t hClientShare;
    uint32_t hTargetClient;
    uint32_t hTargetDevice;
    uint32_t flags;
    NV_A8 uint64_t vaSpaceSize;
    NV_A8 uint64_t vaStartInternal;
    NV_A8 uint64_t vaLimitInternal;
    uint32_t vaMode;
} NV0080_ALLOC_PARAMETERS;

typedef struct NV2080_ALLOC_PARAMETERS {
    uint32_t subDeviceId;
} NV2080_ALLOC_PARAMETERS;

typedef struct {
    uint32_t index;
    uint32_t flags;
    NV_A8 uint64_t vaSize;
    NV_A8 uint64_t vaStartInternal;
    NV_A8 uint64_t vaLimitInternal;
    uint32_t bigPageSize;
    NV_A8 uint64_t vaBase;
    uint32_t pasid;
} NV_VASPACE_ALLOCATION_PARAMETERS;

typedef struct {
    uint32_t owner;
    uint32_t type;
    uint32_t flags;
    uint32_t width;
    uint32_t height;
    int32_t  pitch;
    uint32_t attr;
    uint32_t attr2;
    uint32_t format;
    uint32_t comprCovg;
    uint32_t zcullCovg;
    NV_A8 uint64_t rangeLo;
    NV_A8 uint64_t rangeHi;
    NV_A8 uint64_t size;
    NV_A8 uint64_t alignment;
    NV_A8 uint64_t offset;
    NV_A8 uint64_t limit;
    NV_A8 uint64_t address;
    uint32_t ctagOffset;
    uint32_t hVASpace;
    uint32_t internalflags;
    uint32_t tag;
    int32_t  numaNode;
} NV_MEMORY_ALLOCATION_PARAMS;

#define NVOS32_TYPE_IMAGE                         0
#define NVOS32_ATTR_PAGE_SIZE_SHIFT               23   /* 24:23 */
#define NVOS32_ATTR_PAGE_SIZE_DEFAULT             0x0u
#define NVOS32_ATTR_PAGE_SIZE_4KB                 0x1u
#define NVOS32_ATTR_PAGE_SIZE_BIG                 0x2u
#define NVOS32_ATTR_PAGE_SIZE_HUGE                0x3u
#define NVOS32_ATTR_LOCATION_SHIFT                25   /* 26:25 */
#define NVOS32_ATTR_LOCATION_VIDMEM               0x0u
#define NVOS32_ATTR_LOCATION_PCI                  0x1u
#define NVOS32_ATTR_PHYSICALITY_SHIFT             27   /* 28:27 */
#define NVOS32_ATTR_PHYSICALITY_DEFAULT           0x0u
#define NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS     0x1u
#define NVOS32_ATTR_PHYSICALITY_CONTIGUOUS        0x2u
#define NVOS32_ATTR_COHERENCY_SHIFT               29   /* 31:29 */
#define NVOS32_ATTR_COHERENCY_UNCACHED            0x0u
#define NVOS32_ATTR_COHERENCY_CACHED              0x1u
#define NVOS32_ATTR_COHERENCY_WRITE_COMBINE       0x2u
#define NVOS32_ATTR2_GPU_CACHEABLE_SHIFT          2    /* 3:2 */
#define NVOS32_ATTR2_GPU_CACHEABLE_YES            0x1u
#define NVOS32_ATTR2_GPU_CACHEABLE_NO             0x2u
#define NVOS32_ALLOC_FLAGS_IGNORE_BANK_PLACEMENT  0x00000001u
#define NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE 0x00000010u
#define NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE        0x00000100u
#define NVOS32_ALLOC_FLAGS_VIRTUAL                0x00080000u

/* ---- Controls ------------------------------------------------------------ */

#define NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2  0x205u
typedef struct {
    uint32_t gpuId;
    uint32_t gpuFlags;
    uint32_t deviceInstance;
    uint32_t subDeviceInstance;
    uint32_t sliStatus;
    uint32_t boardId;
    uint32_t gpuInstance;
    int32_t  numaId;
} NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS;

/* Export an RM object to a fresh control channel `fd`, which nvidia-drm then
 * imports (DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY). hParent is the device. */
#define NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD 0x3d05u
#define NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM   1u
typedef struct {
    struct {
        uint32_t type;
        union {
            struct {
                uint32_t hDevice;
                uint32_t hParent;
                uint32_t hObject;
            } rmObject;
        } data;
    } object;
    int32_t  fd;
    uint32_t flags;
} NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS;

#define NV2080_CTRL_CMD_GPU_GET_NAME_STRING              0x20800110u
#define NV2080_GPU_MAX_NAME_STRING_LENGTH                0x40
#define NV2080_CTRL_GPU_GET_NAME_STRING_FLAGS_TYPE_ASCII 0
typedef struct {
    uint32_t gpuNameStringFlags;
    struct {
        uint8_t ascii[NV2080_GPU_MAX_NAME_STRING_LENGTH];
    } gpuNameString;
} NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS;

#define NV2080_CTRL_CMD_MC_GET_ARCH_INFO 0x20801701u
typedef struct {
    uint32_t architecture;
    uint32_t implementation;
    uint32_t revision;
    uint8_t  subRevision;
} NV2080_CTRL_MC_GET_ARCH_INFO_PARAMS;

/* ---- Status codes used by the library itself ---------------------------- */

#define NV_OK                          0x00000000u
#define NV_ERR_INVALID_ARGUMENT        0x0000001Fu
#define NV_ERR_INVALID_OBJECT_HANDLE   0x00000033u
#define NV_ERR_OBJECT_NOT_FOUND        0x00000057u
#define NV_ERR_OPERATING_SYSTEM        0x00000059u

/* ---- Layout checks (sizes match the 610.57.04 kernel's sizeof) ---------- */

_Static_assert(sizeof(nv_ioctl_card_info_t) == 72, "card_info");
_Static_assert(sizeof(nv_ioctl_rm_api_version_t) == 72, "rm_api_version");
_Static_assert(sizeof(nv_ioctl_alloc_os_event_t) == 16, "alloc_os_event");
_Static_assert(sizeof(nv_ioctl_register_fd_t) == 4, "register_fd");
_Static_assert(sizeof(NVOS00_PARAMETERS) == 16, "NVOS00");
_Static_assert(sizeof(NVOS21_PARAMETERS) == 32, "NVOS21");
_Static_assert(sizeof(NVOS02_PARAMETERS) == 48, "NVOS02");
_Static_assert(sizeof(nv_ioctl_nvos02_parameters_with_fd) == 56, "NVOS02 with fd");
_Static_assert(sizeof(NVOS64_PARAMETERS) == 48, "NVOS64");
_Static_assert(sizeof(NVOS54_PARAMETERS) == 32, "NVOS54");
_Static_assert(sizeof(NVOS33_PARAMETERS) == 48, "NVOS33");
_Static_assert(sizeof(nv_ioctl_nvos33_parameters_with_fd) == 56, "NVOS33+fd");
_Static_assert(sizeof(NVOS34_PARAMETERS) == 32, "NVOS34");
_Static_assert(sizeof(NVOS46_PARAMETERS) == 64, "NVOS46");
_Static_assert(sizeof(NVOS47_PARAMETERS) == 48, "NVOS47");
_Static_assert(sizeof(NVOS41_PARAMETERS) == 16, "NVOS41");
_Static_assert(sizeof(NvUnixEvent) == 16, "NvUnixEvent");
_Static_assert(sizeof(NV0005_ALLOC_PARAMETERS) == 24, "NV0005 alloc");
_Static_assert(sizeof(NV0080_ALLOC_PARAMETERS) == 56, "NV0080 alloc");
_Static_assert(sizeof(NV_MEMORY_ALLOCATION_PARAMS) == 128, "NV_MEMORY_ALLOCATION_PARAMS");
_Static_assert(sizeof(NV_VASPACE_ALLOCATION_PARAMETERS) == 56, "NV_VASPACE_ALLOCATION_PARAMETERS");
_Static_assert(sizeof(NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS) == 68, "GET_NAME_STRING");
_Static_assert(sizeof(NV2080_CTRL_MC_GET_ARCH_INFO_PARAMS) == 16, "GET_ARCH_INFO");
_Static_assert(sizeof(NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS) == 32, "GET_ID_INFO_V2");
_Static_assert(sizeof(NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS) == 24, "EXPORT_OBJECT_TO_FD");

#endif /* CRM_NV_IOCTL_DEFS_H */

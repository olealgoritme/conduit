/* SPDX-License-Identifier: MIT */
/*
 * librmclient: a small client for NVIDIA's Resource Manager (RM) API.
 *
 * A user-mode driver allocates RM objects, sends controls, frees them and maps
 * memory through this API; the OS-specific escapes (Linux ioctls on
 * /dev/nvidiactl and /dev/nvidiaN, a Windows KMD later) live behind the
 * transport vtable in rmclient_transport.h.
 *
 * Return values: 0 on success, a negative errno for transport/library
 * failures (-ENOMEM, -EINVAL, -ENOENT, ...), or a positive NV_STATUS when RM
 * itself refused the call (e.g. 0x22 NV_ERR_INVALID_CLASS). crm_status_name()
 * turns either into text.
 *
 * Thread safety: every function may be called from several threads on the
 * same client; the client serialises its own bookkeeping, RM calls run
 * concurrently.
 */
#ifndef RMCLIENT_H
#define RMCLIENT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct crm_client crm_client;
struct crm_transport;                       /* OS backend vtable, see rmclient_transport.h */

/* Open a client: allocates NV01_ROOT_CLIENT. transport NULL = the platform default (Linux: /dev/nvidiactl). Returns 0 or -errno. */
int  crm_open(crm_client **out, const struct crm_transport *transport);
void crm_close(crm_client *c);              /* frees the root client and everything under it */
uint32_t crm_root(const crm_client *c);     /* the root client handle */
/* params is in/out: RM's answer (e.g. NV_MEMORY_ALLOCATION_PARAMS.offset/size, VA space size) is written back into it in place. */
/* hObject in/out: 0 = the library picks a fresh handle. Returns 0, -errno for transport failures, or a positive NV_STATUS from RM. */
int crm_alloc(crm_client *c, uint32_t parent, uint32_t *object, uint32_t hclass, void *params, uint32_t params_size);
int crm_control(crm_client *c, uint32_t object, uint32_t cmd, void *params, uint32_t params_size);
int crm_free(crm_client *c, uint32_t parent, uint32_t object);
/* CPU-map an RM memory object (NV_ESC_RM_MAP_MEMORY + mmap on the device fd). device = the NV01_DEVICE_0 (or subdevice) handle. */
/* Each mapping gets its own fresh channel (Linux: /dev/nvidiaN for video/BAR
 * memory, /dev/nvidiactl for system memory), which RM requires; it stays open
 * until crm_unmap_memory. A subdevice handle works as `device` (e.g. for the
 * usermode doorbell object, which lives under the subdevice). */
int crm_map_memory(crm_client *c, uint32_t device, uint32_t memory, uint64_t offset, uint64_t length, uint32_t flags, void **cpu_ptr);
int crm_unmap_memory(crm_client *c, uint32_t device, uint32_t memory, void *cpu_ptr, uint64_t length, uint32_t flags);
/* GPU-VA map (NvRmMapMemoryDma / NV_ESC_RM_MAP_MEMORY_DMA) and unmap. */
/* `dma` may be a VirtualMemory (NV50_MEMORY_VIRTUAL / NV01_MEMORY_VIRTUAL) or
 * a ctxdma, passed to RM unchanged, or a FERMI_VASPACE_A allocated through
 * this client: RM cannot map into a VA space handle directly, so then the
 * library allocates an NV50_MEMORY_VIRTUAL of `length` in that VA space per
 * mapping (at *gpu_va when it is non-zero, else wherever RM places it), maps
 * into it, and frees it again in crm_unmap_dma. */
int crm_map_dma(crm_client *c, uint32_t device, uint32_t dma /* VA space or ctxdma */, uint32_t memory, uint64_t offset, uint64_t length, uint32_t flags, uint64_t *gpu_va /* in/out */);
int crm_unmap_dma(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory, uint32_t flags, uint64_t gpu_va);
/* Number of GPUs and their minor numbers (for /dev/nvidiaN) and device ids, from NV_ESC_CARD_INFO. */
int crm_gpu_count(crm_client *c);
int crm_gpu_info(crm_client *c, int index, uint32_t *gpu_id, uint32_t *minor, uint32_t *pci_device_id);
const char *crm_status_name(int status);    /* NV_STATUS / -errno to text */

/* ------------------------------------------------------------------------
 * Additions beyond the core contract.
 * ------------------------------------------------------------------------ */

/* The RM version string the kernel reported (e.g. "610.57.04"). */
const char *crm_rm_version(const crm_client *c);

/* Version this library's structure layouts were taken from. crm_open does
 * NV_ESC_CHECK_VERSION_STR (strict, as libnvidia does) against this string,
 * or against $CRM_RM_VERSION when set; CRM_RM_VERSION=any accepts whatever
 * the kernel reports. A mismatch makes crm_open fail with -EPROTO. */
#define CRM_RM_BUILD_VERSION "610.57.04"

/* crm_map_dma with NVOS46's flags2 and kindOverride (PTE kind, applied when
 * NVOS46_FLAGS_PAGE_KIND_OVERRIDE is set in flags). */
int crm_map_dma2(crm_client *c, uint32_t device, uint32_t dma, uint32_t memory,
                 uint64_t offset, uint64_t length, uint32_t flags, uint32_t flags2,
                 uint32_t kind_override, uint64_t *gpu_va);

/* NV_STATUS / -errno to a description ("Given class-id not valid", "No such file or
 * directory"); crm_status_name gives the symbolic name. */
const char *crm_status_string(int status);

/* Extended GPU info from NV_ESC_CARD_INFO. Any pointer may be NULL. */
int crm_gpu_pci(crm_client *c, int index, uint32_t *domain, uint8_t *bus,
                uint8_t *slot, uint8_t *function, uint16_t *vendor_id);

/* Like crm_free but tolerant: frees and forgets the handle even if RM
 * reports it already gone (NV_ERR_OBJECT_NOT_FOUND counts as success). */
int crm_free_quiet(crm_client *c, uint32_t parent, uint32_t object);

/* Pick a fresh handle without allocating anything (for callers that build
 * NVOS structures themselves, e.g. to pass handles inside alloc params). The
 * handle is reserved until crm_alloc uses it or crm_release_handle drops it. */
uint32_t crm_new_handle(crm_client *c);
void     crm_release_handle(crm_client *c, uint32_t handle);

/* OS events, as libnvidia sets them up: a fresh event channel (Linux: a new
 * /dev/nvidiactl fd, pollable for POLLIN), NV_ESC_ALLOC_OS_EVENT on it, then
 * an NV01_EVENT_OS_EVENT object under `parent` (NV0005_ALLOC_PARAMETERS:
 * hSrcResource = parent, notifyIndex = notify_index (callers may OR in
 * NV01_EVENT_* flags), data = the channel fd). Returns the event object's
 * handle and the channel fd. crm_event_close frees the object, the OS event
 * and the channel. */
int crm_event_open(crm_client *c, uint32_t parent, uint32_t notify_index,
                   uint32_t *event_handle, int *event_fd);
int crm_event_close(crm_client *c, uint32_t event_handle, int event_fd);

/* One notification read back by crm_event_drain (NvUnixEvent). */
struct crm_event_data {
    uint32_t object;        /* hObject the notification came from */
    uint32_t notify_index;
    uint32_t info32;
    uint16_t info16;
};

/* Consume pending notifications on an event fd (NV_ESC_RM_GET_EVENT_DATA
 * until RM has no more). Stores up to `max` of them in `out` (may be NULL)
 * and returns how many were consumed (>= 0), or -errno. Events allocated
 * with NV01_EVENT_WITHOUT_EVENT_DATA carry no data: poll() wakes, drain
 * returns 0. Never blocks. */
int crm_event_drain(crm_client *c, int event_fd, struct crm_event_data *out, int max);

/* NV01_MEMORY_SYSTEM_OS_DESCRIPTOR over `size` bytes of the caller's own
 * page-aligned memory at `addr` (e.g. an anonymous mmap), parent `device`
 * (NV01_DEVICE_0). RM pins the pages for the object's lifetime; the CPU keeps
 * using `addr`. This is NV_ESC_RM_ALLOC_MEMORY (NVOS02) on the device's GPU
 * channel, the route RM's Unix escape layer serves for user addresses:
 * NV_ESC_RM_ALLOC of the same class with a VIRTUAL_ADDRESS descriptor is
 * refused with NV_ERR_NOT_SUPPORTED. `nvos02_flags` = 0 picks
 * PCI | CACHED | NONCONTIGUOUS | MAPPING_NO_MAP (RM requires PCI and NO_MAP);
 * otherwise NVOS02_FLAGS_* as RM defines them. *object = 0 lets the library
 * pick the handle. */
int crm_alloc_os_descriptor(crm_client *c, uint32_t device, uint32_t *object,
                            void *addr, uint64_t size, uint32_t nvos02_flags);

/* Raw escape on the control channel (fd = -1) or a transport fd, for escapes
 * this API does not wrap. Returns 0 or -errno; RM status stays in *arg. */
int crm_escape(crm_client *c, int fd, uint32_t nr, void *arg, uint32_t size);
int crm_ctl_fd(const crm_client *c);

/* Number of objects / CPU mappings the client tracks (tests, leak checks). */
size_t crm_object_count(const crm_client *c);
size_t crm_mapping_count(const crm_client *c);

#ifdef __cplusplus
}
#endif

#endif /* RMCLIENT_H */

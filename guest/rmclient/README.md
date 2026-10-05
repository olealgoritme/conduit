# guest/rmclient: librmclient

A small C11 library that talks to NVIDIA's Resource Manager (RM): it allocates
objects, sends controls, frees them, and maps memory for the CPU and the GPU.
It is meant for a user-mode driver that runs on RM instead of nouveau, starting
with NVK (Mesa's Vulkan driver) inside Conduit guests. It depends on nothing but
libc. The OS-specific part is a transport vtable: Linux ioctls on
`/dev/nvidiactl` and `/dev/nvidiaN` today, and a Windows transport through a
Conduit KMD later.

Licensed MIT (`LICENSE`), so Mesa can vendor it. `src/nv_ioctl_defs.h` and
`src/nv_status_codes.inc` are copied from NVIDIA's open-gpu-kernel-modules
610.57.04, which is also MIT.

## API (`include/rmclient.h`)

Calls return `0`, a negative errno for library or transport failures, or a
positive `NV_STATUS` when RM refused the call. `crm_status_name()` and
`crm_status_string()` turn any of these into text.

| call | RM escape |
|---|---|
| `crm_open(&c, transport)` / `crm_close(c)` | `NV_ESC_CHECK_VERSION_STR`, `NV_ESC_CARD_INFO`, `NV_ESC_RM_ALLOC(NV01_ROOT_CLIENT)`, `NV_ESC_REGISTER_FD` / `NV_ESC_RM_FREE(root)` |
| `crm_alloc(c, parent, &h, class, params, size)` | `NV_ESC_RM_ALLOC` (NVOS64). `h = 0` lets the library pick a handle. `params` is in/out. |
| `crm_control(c, object, cmd, params, size)` | `NV_ESC_RM_CONTROL` (NVOS54) |
| `crm_free(c, parent, object)` | `NV_ESC_RM_FREE` (NVOS00). `parent = 0` looks the parent up. |
| `crm_map_memory` / `crm_unmap_memory` | `NV_ESC_RM_MAP_MEMORY` (NVOS33 + fd) + `mmap` / `munmap` + `NV_ESC_RM_UNMAP_MEMORY` (NVOS34) |
| `crm_map_dma[2]` / `crm_unmap_dma` | `NV_ESC_RM_MAP_MEMORY_DMA` (NVOS46) / `NV_ESC_RM_UNMAP_MEMORY_DMA` (NVOS47) |
| `crm_gpu_count`, `crm_gpu_info`, `crm_gpu_pci` | the `NV_ESC_CARD_INFO` read at open |
| `crm_alloc_os_descriptor(c, device, &h, addr, size, flags)` | `NV_ESC_RM_ALLOC_MEMORY` (NVOS02 + fd) on the GPU channel: `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over the caller's own pages. RM accepts a user address only on this route; `NV_ESC_RM_ALLOC` of the class answers `NV_ERR_NOT_SUPPORTED`. |
| `crm_event_open` / `crm_event_drain` / `crm_event_close` | a new control fd, `NV_ESC_ALLOC_OS_EVENT`, `NV01_EVENT_OS_EVENT` with `data = fd` / `NV_ESC_RM_GET_EVENT_DATA` / `NV_ESC_FREE_OS_EVENT` |
| `crm_escape(c, fd, nr, arg, size)` | any escape, unwrapped |
| `crm_new_handle`, `crm_release_handle`, `crm_free_quiet`, `crm_object_count`, `crm_mapping_count`, `crm_rm_version`, `crm_ctl_fd` | helpers |

What the library does for the caller:

- **Handles.** The library picks handles from `0x5c000001` upward, unique
  within the client, like libnvidia does. The root client handle comes from RM.
  Every object is tracked with its parent and class. Freeing an object forgets
  its subtree, and freeing a memory object releases its CPU mappings.
- **Version check.** A strict `NV_ESC_CHECK_VERSION_STR` against
  `CRM_RM_BUILD_VERSION` ("610.57.04"), the version the structure layouts come
  from. You can override it with `$CRM_RM_VERSION`. `CRM_RM_VERSION=any`
  accepts whatever the kernel reports. A mismatch fails `crm_open` with
  `-EPROTO`.
- **GPU channels.** `crm_open` opens `/dev/nvidiaN` for every GPU and links it
  with `NV_ESC_REGISTER_FD`. RM needs the process to hold the GPU open before
  it allocates `NV01_DEVICE_0`.
- **CPU mappings.** Each mapping gets its own fresh channel, because RM and
  Conduit's backend allow one mapping context per fd. System memory uses
  `/dev/nvidiactl` and BAR memory uses `/dev/nvidiaN` (registered). The
  channel stays open until `crm_unmap_memory`. The library picks the channel
  from the memory object's class. If RM answers `NV_ERR_INVALID_ARGUMENT` (the
  wrong kind of node), it retries on the other kind. `device` can be a
  subdevice handle, which the usermode doorbell object needs. A sub-page
  `offset` is handled: the returned pointer is `base + (offset % page)`.
- **GPU mappings.** RM cannot map into a `FERMI_VASPACE_A` handle directly. It
  answers `NV_ERR_INVALID_OBJECT_HANDLE`, because only VirtualMemory or a
  ctxdma can be a mapper. When `dma` is a VA space of this client, the library
  does three things. It allocates an `NV50_MEMORY_VIRTUAL` of `length` in that
  VA space for the mapping, at `*gpu_va` if that is non-zero
  (`FIXED_ADDRESS_ALLOCATE`) or else where RM places it. It maps into that
  object. `crm_unmap_dma` frees the object again. Any other `dma` handle is
  passed through unchanged, so a driver that manages its own VA (NVK) can
  allocate one large VirtualMemory and map with `NVOS46_FLAGS_DMA_OFFSET_FIXED`
  itself.
- **Threads.** The client's bookkeeping is protected by a C11 `mtx_t`. RM calls
  run concurrently.

## Transports (`include/rmclient_transport.h`)

`struct crm_transport` holds `open(node)` (`CRM_NODE_CTL` or a GPU minor),
`close`, `ioctl(fd, nr, arg, size)`, `mmap`, `munmap`, a `page_size`, flags
(`NO_VERSION_CHECK`, `NO_REGISTER_FD`) and an optional `destroy`. Transport fds
are also the values the library writes into fd-carrying payloads
(`register_fd.ctl_fd`, `nvos33_with_fd.fd`, `alloc_os_event.fd`,
`NV0005_ALLOC_PARAMETERS.data`). A transport's fd namespace must therefore be
one its kernel side understands.

The library runs Linux's mapping protocol itself, using `open`, `ioctl`,
`mmap` and `close`: open a fresh channel, `MAP_MEMORY` naming it, mmap it at
offset 0, and keep it until unmap. A transport whose OS maps memory another
way, such as a Windows KMD that maps into the process and returns the address,
sets the optional `map_memory` / `unmap_memory` hooks instead, and the library
skips its own protocol.

`crm_linux_transport()` is the Linux one. `crm_open(&c, NULL)` uses
`crm_default_transport()`. `$CRM_DEV_DIR` overrides `/dev` for tests.

## Build and test

```sh
meson setup build && meson test -C build        # unit tests (fake transport, no GPU)
make check                                      # same without meson (build-make/)
make smoke                                      # tests/crm_smoke.c against a real RM
```

`tests/test_unit.c` runs the library against a fake transport that acts as a
small RM. It covers handle allocation and reuse, table growth, NVOS
marshalling, error mapping, free cascades, CPU-mapping bookkeeping (channel per
mapping, fallback, cookies, cleanup on free and close), VA-space GPU mappings,
events, the version check and a custom-map transport. It also passes under
ASan/UBSan.

Run `tests/crm_smoke.c` inside a Conduit guest, not on the host, so a GPU fault
stays contained. It opens a client, reads CARD_INFO, allocates a device and a
subdevice, reads the GPU name and arch, allocates a VA space plus 2 MiB of
video memory and 2 MiB of system memory, CPU-maps the system memory and
writes/reads it, GPU-maps both, then unmaps and frees everything. Extra steps
CPU-map video memory over BAR1, map the usermode doorbell object through the
subdevice, and open/drain/close an OS event. Each step is printed.

## Status

Tested in the `lab` guest (RTX 5090, GB20x, host driver 610.57.04):
`crm_smoke` passes every required step and every extra step. One gap is in
Conduit, not in the library. The host backend refuses `NV_ESC_RM_GET_EVENT_DATA`
(0x52) with `-EINVAL` ("escape 0x52 is not in the ABI profile for host driver
610.57.04"), so `crm_event_drain` fails in Conduit guests. Event allocation
works. Polling the fd for a real notification has not been tested. The backend's
ABI profile needs 0x52, a 16-byte `NVOS41` whose `pEvent` is a nested
16-byte user pointer, before event data can be read.

Not done yet: the Windows transport, `NV_ESC_RM_DUP_OBJECT`, and
export/import of objects by fd.

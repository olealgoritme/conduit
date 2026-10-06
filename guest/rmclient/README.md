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
| `crm_dup_object(c, parent, &h, client_src, object_src, hclass, flags)` | `NV_ESC_RM_DUP_OBJECT` (NVOS55): another client's object as a new handle of this one, tracked like an allocation |
| `crm_map_memory` / `crm_unmap_memory` | `NV_ESC_RM_MAP_MEMORY` (NVOS33 + fd) + `mmap` / `munmap` + `NV_ESC_RM_UNMAP_MEMORY` (NVOS34) |
| `crm_map_dma[2]` / `crm_unmap_dma` | `NV_ESC_RM_MAP_MEMORY_DMA` (NVOS46) / `NV_ESC_RM_UNMAP_MEMORY_DMA` (NVOS47) |
| `crm_gpu_count`, `crm_gpu_info`, `crm_gpu_pci` | the `NV_ESC_CARD_INFO` read at open |
| `crm_alloc_os_descriptor(c, device, &h, addr, size, flags)` | `NV_ESC_RM_ALLOC_MEMORY` (NVOS02 + fd) on the GPU channel: `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over the caller's own pages. RM accepts a user address only on this route; `NV_ESC_RM_ALLOC` of the class answers `NV_ERR_NOT_SUPPORTED`. |
| `crm_event_open` / `crm_event_drain` / `crm_event_close` | a new control fd, `NV_ESC_ALLOC_OS_EVENT`, `NV01_EVENT_OS_EVENT` with `data = fd` / `NV_ESC_RM_GET_EVENT_DATA` / `NV_ESC_FREE_OS_EVENT` |
| `crm_event_wait(c, fd, timeout_ms)` | none: waits for the event channel (Linux: `poll` POLLIN). 1 signalled, 0 timeout, `-ENOSYS` if the transport cannot wait |
| `crm_alloc_pages` / `crm_free_pages` | none: host pages for `crm_alloc_os_descriptor` (Linux: anonymous `mmap` with `MAP_POPULATE`, `MADV_DONTFORK`; Windows: `VirtualAlloc`) |
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
- **Threads.** The client's bookkeeping is protected by a C11 `mtx_t` (an SRW
  lock on Windows, `src/crm_mutex.h`). RM calls run concurrently.
- **OS services.** Everything else OS-specific that a driver on RM needs (event
  waits, pinnable host pages) also goes through the library, so NVK's RM
  backend calls neither `mmap` nor `poll` and builds for Windows unchanged.

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

Transport ABI 2 (`CRM_TRANSPORT_ABI`) adds three optional callbacks at the end
of the struct: `event_wait(fd, timeout_ms)`, `alloc_pages(size)` and
`free_pages`. `crm_open` still accepts ABI 1 transports; their ABI 2 callbacks
count as missing, and the wrappers answer `-ENOSYS`.

`crm_linux_transport()` is the Linux one. `crm_windows_transport()` is the
Windows one (`src/transport_windows.c`): RM escapes through the Conduit KMD as
`HELIOS_ESCAPE_NVRM` calls on `D3DKMTEscape` (the ABI is
`guest/windows/protocol/src/nvrm.rs`, mirrored in `src/helios_nvrm_escape.h`).
It finds the Helios adapter by probing the verb, then does what the Linux guest
module does in its kernel: builds the host's wire messages (`src/win_wire.h`,
unit-tested on any host by `tests/test_win_wire.c`) from each NVIDIA escape and
the blocks its pointers address. The KMD only forwards them and owns handles.
A transport "fd" is the backend handle the host returned from Open, so fds
written into payloads already mean what the host expects.

Implemented: `open`, `close`, `ioctl` (including the parameter block of
`NV_ESC_RM_CONTROL` and `NV_ESC_RM_ALLOC`), `map_memory`/`unmap_memory` (CPU
mapping: Linux's channel-per-mapping protocol, with the KMD doing the final map),
`alloc_pages`, `event_wait` (a manual-reset event per channel, registered with the
KMD's EVENT_REGISTER) and pinning user memory as an OS descriptor (PIN/UNPIN).

Device loss is tracked per generation (`src/win_gen.h`). A generation starts
when init accepts QUERY_CAPS and records the transport's epoch; it ends when a
reply says the transport is gone (`helios_nvrm_reply_is_lost`: TRANSPORT_RESET,
or an epoch other than the init epoch, which with the KMD's per-image salt also
catches a reloaded driver image), when a D3DKMT status says the device is gone,
or when the process's loss table (`helios_kmdmap.h`) moved for any other reason.
The loss table's epoch is the one latch. From then on every call answers
`-ENODEV` without an escape, and the process's TRANSPORT_LOST event wakes every
blocked `event_wait`. The next `open` starts a new generation on the KMD that
came back: the old generation's channels are stale (refused, forgotten at their
close), its CPU views are dropped (never unmapped through the new KMD), and
nothing of it is ever sent again, because ids restart per transport and an old
MUNMAP, UNPIN or EVENT_UNREGISTER could tear down a new object with the same id.
Controls whose parameters hold a pointer of their own are sent without it. The
Windows build needs the vendored WDK headers
(`guest/windows/icd/win-build/wdk-include`), which `meson.build` adds.
`crm_open(&c, NULL)` uses `crm_default_transport()` (Linux or Windows).
`$CRM_DEV_DIR` overrides `/dev` for tests.

## Build and test

```sh
meson setup build && meson test -C build        # unit tests (fake transport, no GPU)
make check                                      # same without meson (build-make/)
make smoke                                      # tests/crm_smoke.c against a real RM
```

Windows (cross-compiled on Linux with MinGW-w64; `guest/nvk-rm/build-windows.sh`
does this as part of the NVK build):

```sh
meson setup build-win --cross-file ../nvk-rm/windows/mingw-x86_64.ini
ninja -C build-win                 # librmclient.dll, librmclient.dll.a, test_unit.exe, ...
wine build-win/test_unit.exe       # unit tests pass under wine (wine64 9.0)
```

`librmclient.dll` depends on system DLLs only (KERNEL32, msvcrt). MinGW
exports every non-static function (there is no `.def` file yet).

`tests/test_unit.c` runs the library against a fake transport that acts as a
small RM. It covers handle allocation and reuse, table growth, NVOS
marshalling, error mapping, free cascades, CPU-mapping bookkeeping (channel per
mapping, fallback, cookies, cleanup on free and close), VA-space GPU mappings,
events, the version check and a custom-map transport. It also passes under
ASan/UBSan.

Run `tests/crm_smoke.c` and `tests/crm_event_smoke.c` inside a Conduit guest,
not on the host, so a GPU fault stays contained. It opens a client, reads CARD_INFO, allocates a device and a
subdevice, reads the GPU name and arch, allocates a VA space plus 2 MiB of
video memory and 2 MiB of system memory, CPU-maps the system memory and
writes/reads it, GPU-maps both, then unmaps and frees everything. Extra steps
CPU-map video memory over BAR1, map the usermode doorbell object through the
subdevice, and open/drain/close an OS event. Each step is printed.
`crm_event_smoke` arms the subdevice's software notifier, triggers it and
reads the events back with `crm_event_drain`.

`tests/crm_pin_smoke.c` registers the caller's own memory with RM: it takes
pages from `crm_alloc_pages`, makes an `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`
over them, GPU-maps and unmaps it, checks the CPU's pattern survived, and
frees it, at 2 MiB and at 512 MiB (past what a direct page-run table holds on
Windows, where the KMD pins the range).

`tests/crm_import_smoke.c` (Windows only; exits 77 elsewhere) exercises the Helios
KMD's `HELIOS_ESCAPE_FOREIGN_RESOURCE` verb, which turns RM memory into a Venus
resource id. It first asks `QUERY_CAPS` and prints SKIP, exit 0, when the KMD has no
such verb or the host does not serve the import yet (`CAP_RM_IMPORT` clear), so it
is safe to run before the host half is installed. Otherwise it allocates pitch-linear
vidmem, exports it into a GEM object on a DRM render node (as `crm_scanout_smoke`),
creates a Venus context, sends `IMPORT_RM` with its layout and expects `ST_OK` and
a resource id, releases it with `RELEASE_BLOB` (a second release must fail), then
sends six deliberate mistakes and checks the status and `out_host_errno` of each
(unknown GEM: `ST_NOT_OWNED`/ENOENT; oversize: `ST_BAD_RANGE`/ERANGE; a non-DRI
channel: `ST_NOT_OWNED`; a foreign context: `ST_BAD_CONTEXT`; no layout flag and a
bad modifier: `ST_BAD_RANGE`). It prints `IMPORT SMOKE PASSED`, or the first failing
step, and exits nonzero on failure. The verbs go out through `crm_win_escape_raw`,
which sends any non-NVRM Helios escape on the same D3DKMT device as the RM handles;
the ABI comes from the KMD's own `guest/windows/protocol/include/helios_foreign.h`.

## Status

Tested in the `lab` guest (RTX 5090, GB20x, host driver 610.57.04):
`crm_smoke` passes every required step and every extra step. Conduit serves
`NV_ESC_RM_GET_EVENT_DATA` (0x52) on a file that holds an OS event (the guest
module carries the 16-byte event buffer, the backend checks it,
`host/backend/device/src/nvidia/os_event.rs`), so `crm_event_drain` works in
Conduit guests; a backend from before that refuses 0x52 with `-EINVAL`. NVK on
RM (`guest/nvk-rm`) is the first user.

Windows, in the `win11` guest with the Helios KMD (22.22.307.0 and later,
same GPU and host driver): `crm_smoke`, `crm_pin_smoke` (OS descriptors through
the KMD's PIN), `crm_event_smoke` (EVENT_REGISTER over the device event queue)
and `crm_scanout_smoke` (zero-copy ScanoutFlip) pass; NVK on RM runs on this
transport (see `guest/nvk-rm/README.md`).

`crm_dup_object` (`NV_ESC_RM_DUP_OBJECT`) works across processes of a
Windows guest (`tests/crm_share_smoke.c`, win11, KMD 22.22.311.0): every RM
client of a VM lives in the one backend process, so RM lets any of them dup
from any other. The sanctioned route for shared surfaces is the resource id
instead (`crm_win_rm_resource_import`, backend `RmResourceImport`,
`guest/windows/docs/shared-surfaces.md`); the KMD is to refuse a dup whose
source client is another process's.

Not done yet: export/import of objects by fd in the library itself (NVK on RM
does that with the RM controls `OS_UNIX_EXPORT_OBJECT_TO_FD` /
`IMPORT_OBJECT_FROM_FD`).

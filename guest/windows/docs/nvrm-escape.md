# HELIOS_ESCAPE_NVRM: the RM forwarding escape (developer reference)

Status: written against `1622662` (KMD 22.22.307.0 line). Every statement below was
checked against the code named in it. Things that could not be verified from the source
alone (nothing here has been run by the author of this document) are marked
**UNVERIFIED** and collected in section 10.

Sources of truth, in order of authority: the KMD code (`kmd_render/src/ddi/escape.rs`,
`virtio/nvrm.rs`, `virtio/gpu/nvrm_tables.rs`, `virtio/gpu/nvrm_events.rs`), then the ABI
(`guest/windows/protocol/src/nvrm.rs`, mirrored by `guest/rmclient/src/helios_nvrm_escape.h`).
Where a comment in the ABI file disagrees with the code, this document follows the code and
section 10.3 lists the disagreement.

## 1. Purpose and architecture

NVK on NVIDIA's Resource Manager (RM) runs in the Windows guest as a user-mode driver.
RM lives on the host. `HELIOS_ESCAPE_NVRM` (verb `0x0016`) is the one door between them:
a user-mode RM client sends the device's NVIDIA-RM forwarding messages through the Helios
KMD, which owns the virtio device.

```
NVK (Mesa, RM backend)
  -> librmclient  (guest/rmclient, Windows transport: src/transport_windows.c)
       builds the host wire messages itself (src/win_wire.h): the per-ioctl pointer
       fix-ups, nested blocks, the GetSysFiles tables
  -> D3DKMTEscape(D3DKMT_ESCAPE_DRIVERPRIVATE, HELIOS_ESCAPE_NVRM buffer)
  -> Helios KMD  DxgkDdiEscape -> escape_nvrm  (a pipe with ownership)
  -> virtio-gpu control queue, raw message (ctrl::raw_roundtrip)
  -> host backend (host/backend/protocol MsgType::{Open,Close,Ioctl,Mmap,Munmap,
       GetProcFiles,GetSysFiles,EventReady,ScanoutFlip})
  -> RM (nvidiactl / nvidiaN / UVM / DRM nodes on the host)
```

The KMD is **a pipe with ownership, not an RM client.** It does not interpret RM. In user
mode stay: the RM structures, the data/nested/deep pointer fix-ups, the device tables from
`GetSysFiles`. What the KMD reads of a forwarded message is only:

- the 16-byte `MsgHeader` (`msg_type`, `handle`);
- for `Ioctl`, the 24-byte `IoctlReq` after it, to enforce the page-run rule and the length
  check (`virtio/nvrm.rs::check_ioctl`);
- for `ScanoutFlip`, `scanout` and `owner_handle` (offsets 16 and 20);
- the reply `MsgHeader` of `Open` and `Close` (handle, status), and, for a registration
  that carries a pin, the RM status word at `rm_status_off`;
- for a plain `Ioctl`, the one RM call it recognises: `NV_ESC_RM_FREE` (section 4.4).

Everything else the KMD adds is bookkeeping that one process must not be able to get wrong
about another: handle ownership, mapping / pin / event lifetime, and teardown.

**Security stance (the one rule that is enforced now).** User mode never supplies or sees a
guest-physical address. Page-run tables are built only by the KMD from pages it locked
(`PIN`); a `FORWARD` that carries a page-run `deep_ptr_offset` is refused. Broader hardening
is deferred (section 10).

**Owner = the D3DKMT device handle**, not the process. Every ownership table is keyed on
the `hDevice` of the escape (`DeviceOwner::new(args.hDevice)`). An escape with no `hDevice`
gets `STATUS_INVALID_PARAMETER`. librmclient's Windows transport keeps one process-global
device, so in practice one process is one owner. Two devices in one process do not share
handles.

## 2. Calling convention

One buffer, passed as `pPrivateDriverData` / `PrivateDriverDataSize` of
`D3DKMT_ESCAPE` (`Type = DRIVERPRIVATE`; librmclient sets `HardwareAccess = 0`).

- Every request is `repr(C)`, padding-free, 8-byte aligned, little-endian, and starts with
  `HeliosNvrmHeader` (40 bytes): the 16-byte `HeliosEscapeHeader`
  (`magic = 0x48454C53 'HELS'`, `cmd_type = 0x0016`, `version = 1`, `size` = whole buffer)
  then `abi_version` (must be `HELIOS_NVRM_ABI_VERSION = 1`) @16, `op` @20, `status` @24
  (out), `reserved` @28 (must be 0), `epoch` @32 (out).
- `hdr.size` must be at least the op's struct and not larger than the buffer the runtime
  gave; for `FORWARD` it must equal the computed total exactly.
- No pointers, no `HANDLE`/`size_t`/`long` in any struct: addresses are `u64`, an event
  `HANDLE` is a `u64` zero-extended from a 32-bit process. WoW64 and 64-bit callers use the
  identical layout and the KMD has no thunk. (A 32-bit process can still fail for lack of
  address space: `MMAP` -> `NO_RESOURCES`.)
- `helios_nvrm_init(&head, op, total)` (C header) fills the common header.
- Any number of threads may issue ops concurrently; the KMD does not serialise them.
- **Capability probe:** send `QUERY_CAPS`. `STATUS_NOT_IMPLEMENTED` means a KMD that does not
  know the verb (the dispatcher's unknown-`cmd_type` arm). Only the Helios KMD answers it, so
  librmclient uses it to identify the adapter.
- Additive changes (new op, new status, new flag bit) do not bump `abi_version`; clients
  discover them through `QUERY_CAPS`.

## 3. Result reporting: three layers, never conflate them

| layer | where | meaning |
|---|---|---|
| 1. NTSTATUS of `D3DKMTEscape` | return value; on failure the buffer is not written | transport verdict for a malformed or unservable request |
| 2. `HeliosNvrmHeader.status` (`HELIOS_NVRM_ST_*`, 0 = OK) | header, written whenever the escape returns `STATUS_SUCCESS` | the KMD's verdict on a well-formed request |
| 3. RM result | inside the forwarded reply bytes: `MsgHeader.status` (signed negative errno from the host) and the RM parameter struct's own `status` | never interpreted by the KMD, except the two cases the KMD must act on (Open/Close reply handle+status; `rm_status_off` word for a pin) |

NTSTATUS values this code returns (all in `escape.rs`):

| NTSTATUS | when |
|---|---|
| `STATUS_NOT_IMPLEMENTED` | unknown `cmd_type` (an older KMD without the verb) |
| `STATUS_INVALID_PARAMETER` | null/too-small buffer for a Helios header, bad magic/version, `hdr.size` > buffer, no `hDevice`, buffer > 1 MiB, `abi_version != 1`, `reserved != 0`, unknown `op`; nonzero `flags` on MUNMAP / PIN / UNPIN / EVENT_*; EVENT `kind == 0` or `event_handle == 0` (register), or an `event_handle` that is not an event the caller may signal |
| `STATUS_BUFFER_TOO_SMALL` | buffer or `hdr.size` shorter than the op's struct |
| `STATUS_DEVICE_NOT_READY` | **only** `EVENT_REGISTER` / `EVENT_UNREGISTER` when there is no virtio transport at all |

Everything else is a header status. `HELIOS_NVRM_ST_*` as this build produces them:

| code | name | produced by |
|---|---|---|
| 0 | OK | |
| 1 | NOT_OWNED | handle / mapping_id / pin_id not the caller's (one code on purpose); also the same condition when there is no transport |
| 2 | MSG_TYPE_REFUSED | FORWARD `msg_type` outside the allow-list |
| 3 | DEVICE_ERROR | transport failed, short/bad reply, host refused an `Mmap` (host errno in `MMAP.flags`) |
| 4 | TRANSPORT_RESET | **`EVENT_REGISTER` only**, on a failed transport |
| 5 | TOO_SCATTERED | PIN: more runs than the indirect table can name |
| 6 | NO_RESOURCES | a quota or table full, `QueueFull`/`OutOfMemory` on the ring, user address space, MMAP bookkeeping |
| 7 | UNSUPPORTED | MMAP with bad `prot`/`cache_request`/nonzero `flags`, or no shared-memory region; EVENT_REGISTER when the event queue is not up or `kind` unknown |
| 8 | TIMEOUT | FORWARD / host Mmap / host Munmap exceeded its time |
| 9 | RESP_TRUNCATED | **defined, never returned by this build** (a too-small `resp_cap` is not detected; size it generously) |
| 10 | BAD_RANGE | size/alignment/length field out of bounds (per op below) |
| 11 | FORBIDDEN | page-run `deep_ptr_offset`; `pin_id` with non-empty deep fields or on a non-Ioctl; `ScanoutFlip` on a non-DRM handle |
| 12 | PIN_IN_USE | UNPIN of a pin a FORWARD has used |

`epoch` (u64, written on every success return including non-zero `status`) is
`VirtioGpu::nvrm_epoch()`, i.e. the transport instance's `wire_fence_base`: it changes at
every StartDevice, and reads **0 when there is no transport**. A client that sees its
remembered epoch change must reopen: every handle, mapping, pin and event of the earlier
transport is gone. A status of `TRANSPORT_RESET` (or `STATUS_DEVICE_NOT_READY` from the event
ops, which is what "no transport" looks like to them) says the same. librmclient on branch
`rmc/transport-loss` (commit `2995935`) reads the epoch of every reply, and `0` means no
transport (section 9); on `feat/nvk-rm-windows-transport` alone it still reads the epoch only
at init.

## 4. The operations

`op` numbers; `QUERY_CAPS.supported_ops` bit `n` is op `n`. This build reports
`0x19E` (ops 1, 2, 3, 4, 7, 8) plus `0x60` (ops 5, 6) only while the event queue is up.

### 4.1 QUERY_CAPS (op 1, 88 bytes)

Touches no device state, so it works with the transport down (then `device_features = 0`,
event ops/kinds absent, `epoch = 0`). Fills: `max_buffer_bytes` (1 MiB),
`default_timeout_ms` (30000), `supported_ops`, `supported_event_kinds` (`0b110` while events
are usable, else 0), `supported_cache_types` (`0b1111`), `device_features` (the virtio config
`features` word read at init), `max_handles` 128, `max_mappings` 256, `max_pins` 256,
`max_pin_pages` 262143, `pin_deep_kinds` (direct | indirect = 3). The `max_*` values are the
per-process limits of section 5.

### 4.2 FORWARD (op 2)

Buffer: `HeliosNvrmForward` (64 bytes) | request (`req_len`) | pad to 8 | response area
(`resp_cap`). `total = 64 + align8(req_len) + resp_cap` must equal `hdr.size`.

```
+0                       HeliosNvrmForward (head 40, req_len @40, resp_cap @44,
                         resp_len @48 (out), timeout_ms @52, pin_id @56, rm_status_off @60)
+64                      request  = host MsgHeader(16) | payload
+64 + align8(req_len)    response area (the device writes the reply here)
```

The request is forwarded **verbatim**; the reply is the device's bytes as-is
(`MsgHeader | payload`, or the header-less stream for `GetProcFiles`/`GetSysFiles`) and
`resp_len` says how many are valid. Size `resp_cap` as the Linux module sizes `resp_max`
(128 KiB for `GetSysFiles`).

Layout checks (all `BAD_RANGE`): arithmetic overflow; `req_len < 16`; `resp_cap < 16`;
`total > buffer length`; `total > 1 MiB`; `total != hdr.size`; `rm_status_off != 0` with
`pin_id == 0`. `timeout_ms == 0` means 30000. The call blocks the thread at PASSIVE on an
event, not a poll. Timeouts and enqueue back-pressure: the ring enqueue retries up to 5 s
(`ENQUEUE_RETRY_MAX_MS`) before `QueueFull` -> `NO_RESOURCES`.

**Message-type allow-list** (`HELIOS_NVRM_FORWARD_MSG_TYPES`, bitmask over the host
`MsgType`): Open 1, Close 2, Ioctl 3, GetProcFiles 6, GetSysFiles 7, ScanoutFlip 20.
Anything else, including `msg_type >= 32`, is `MSG_TYPE_REFUSED`. Mmap (4) and Munmap (5)
are refused because they have their own ops; EventReady (8) and input/clipboard (21-27) are
host -> guest; GpuCmd (30) has its own paths. The `msg_type` check runs first.

Per message type:

| msg | rules |
|---|---|
| Open (1) | A tracking slot is reserved **before** the host is asked: a full table or per-process quota gives `NO_RESOURCES` and the host opens nothing. The reply's `handle` (nonzero) with `MsgHeader.status == 0` is committed as owned by the caller, together with the request's `device_type` (`OpenReq` at offset 16). A failed/zero reply cancels the reservation. A transport timeout leaves a possibly-opened handle on the host that the KMD does not track (until the host side is reset; bounded by quota). |
| Close (2) | See section 6 for the full order. Requires the handle to be the caller's, else `NOT_OWNED`. |
| Ioctl (3) | Handle must be the caller's (`NOT_OWNED`). `check_ioctl`: request >= 40 bytes; `IoctlReq` at offset 16: `data_len` @20, `nested_len` @28, `deep_ptr_offset` @32, `deep_len` @36; `deep_ptr_offset` equal to `0xFFFFFFFE` or `0xFFFFFFFD` is `FORBIDDEN`; `40 + data_len + nested_len + deep_len > req_len` is `BAD_RANGE`. (Extra trailing bytes are accepted; only an overrun is refused.) A handle that is a fence (section 4.6) is `FORBIDDEN`. A `SEMSURF_FENCE_CREATE` on a DRM node is recognised and its reply handle is recorded (section 4.6). |
| GetProcFiles (6) / GetSysFiles (7) | `handle = 0`; no ownership; counted as `NvOther`. |
| ScanoutFlip (20) | Request must be exactly `16 + 64` bytes (`BAD_RANGE`). `scanout` (offset 16) must be 0 (`BAD_RANGE`). `owner_handle` (offset 20) must be a handle the caller opened (`NOT_OWNED`) with `device_type >= 512`, i.e. a DRM node (`FORBIDDEN`). The 64-byte payload is otherwise forwarded as is (it names a host GEM object; zero-copy present, see `zero-copy-present.md`). The `MsgHeader.handle` of a flip is not checked. Counter `NvFlip`. |

**`pin_id` (registration of pinned memory), Ioctl only.** `pin_id != 0` on any other message
type is `FORBIDDEN`. The request must carry `deep_ptr_offset == 0 && deep_len == 0`
(`FORBIDDEN` otherwise). The pin must be the caller's, made on the same `handle`, and not
yet used (`NOT_OWNED` otherwise; a second FORWARD with the same pin_id therefore fails).
`resp_cap` must reach the status word: `16 + 12 + rm_status_off + 4 <= resp_cap`
(`BAD_RANGE`). The KMD truncates the request to `40 + data_len + nested_len`, appends the
pin's page-run table as the deep block, writes `deep_ptr_offset` (`0xFFFFFFFE` direct or
`0xFFFFFFFD` indirect) and `deep_len` itself, and forwards. It then decides whether to keep
the pin (section 4.5).

Failure mapping of a forward: `Timeout` -> `TIMEOUT`; `QueueFull` / `OutOfMemory` ->
`NO_RESOURCES`; any other transport error -> `DEVICE_ERROR`. On `TIMEOUT` the request may
still be in flight and may take effect; the KMD discards the late reply and the response
area holds nothing. Treat the handle as indeterminate and close it.

### 4.3 MMAP / MUNMAP (ops 3, 4)

`MMAP` (88 bytes): in `handle`, `prot` (`READ` 1, `WRITE` 2; at least one, no others;
execute is never granted), `offset` (the RM mmap cookie, passed to the host unchanged),
`size`, `cache_request`, `flags` (0); out `size`, `cache_effective`, `out_user_va`,
`out_mapping_id`, and `flags` = the host's errno (positive) when `status == DEVICE_ERROR`
because the host refused.

Order and results:

1. `flags != 0`, `prot == 0`, unknown `prot` bits, `cache_request > WB` -> `UNSUPPORTED`.
2. Handle not the caller's -> `NOT_OWNED`. `size == 0`, not a page multiple, `> 256 MiB`
   (`MAX_MAP_BYTES`), or `offset` not page-aligned -> `BAD_RANGE`. Per-process mapping
   quota -> `NO_RESOURCES`.
3. Host `Mmap{size, offset, prot}` is sent (prot 3 if writable, else 1; 30 s). A host errno
   -> `DEVICE_ERROR` + `flags = errno`; a timeout -> `TIMEOUT`.
4. The reply is an offset into a shared-memory region, not an address. The region is chosen
   by the handle's `device_type`: 256 (UVM) -> the UVM aperture (shm region 2), everything
   else (UVM tools 257 included) -> the RM window (region 1). No such region (device built
   without it; see `NvWinMb`/`NvAptMb`) -> `UNSUPPORTED`; offset not page-aligned, host size
   < requested, or beyond the region -> `BAD_RANGE`. Every failure after the host `Mmap`
   undoes it with a host `Munmap` (skipped for host id 0 or while another live mapping
   carries the same nonzero host id).
5. Cache: UVM is forced write-back; others default (`CACHE_DEFAULT`) to **write-combined**
   (as the Linux module); an explicit `UC`/`WC`/`WB` is honoured for non-UVM.
   `cache_effective` is never `DEFAULT`. Do not mix attributes across live mappings of the
   same pages.
6. The BAR range is mapped into the caller with a read-only view unless `PROT_WRITE`.
   Failure -> `NO_RESOURCES`.
7. A **KMD-minted** `mapping_id` (unique, nonzero, below `0x7FFFFFF0`) is returned. The
   host's id is not used as the key because the RM path answers 0 for every mapping.
   Commit re-checks that the handle is still the caller's (a concurrent `Close` may have
   taken it).

`MUNMAP` (48 bytes): `mapping_id`, `flags` (0, else `STATUS_INVALID_PARAMETER`). An id the
caller does not hold (including a second MUNMAP) is `NOT_OWNED`. The view is unmapped
first and always; then the host `Munmap` is sent (5 s) and its failure is reported as
`TIMEOUT` / `DEVICE_ERROR` after the view is already gone. For host id 0 nothing is sent
(RM releases the mapping through `NV_ESC_RM_UNMAP_MEMORY`).

A mapping lives until `MUNMAP`, `Close` of its handle, device destroy, or transport loss
(for the view, "transport loss" means the owner's next NVRM escape after a `StopDevice`:
section 6).

### 4.4 PIN / UNPIN (ops 7, 8)

`PIN` (80 bytes) registers memory by CPU address (`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`).
RM reads a user address in the caller's space, which is meaningless to a host, so the KMD
locks the pages and names them to the host as guest-physical runs. In: `handle` (the backend
handle the registration will be made under), `flags` (0, else `STATUS_INVALID_PARAMETER`),
`user_va`, `length` (both page multiples, `length > 0`), `h_root`, `h_object`. Out:
`out_pin_id`, `out_npages`.

- `NOT_OWNED` for a foreign handle; `BAD_RANGE` for misalignment, `length == 0`, more than
  262143 pages, or a range the OS refuses to lock (the lock runs in the caller's process
  under SEH; `NvPinErr`); `NO_RESOURCES` for the per-process quota (checked before locking)
  or a table/table-memory failure; `TOO_SCATTERED` when the range needs more than 262143
  runs.
- Run table: up to 1024 runs go in the message ("direct"). More: the table is kept in
  contiguous non-paged memory by the KMD and the message names the pages that hold it
  ("indirect"). Expect indirect tables beyond a few MiB; Windows user memory scatters.
- The pin is bound to `handle`, which bounds its life.

**Pin lifetime, exactly as coded.** A pin starts *unused*. The `FORWARD` that carries it
makes it *used* ("committed": the GPU may now hold the pages). Then:

| event | effect on the pin |
|---|---|
| `UNPIN` of an unused pin | released |
| `UNPIN` of a used pin | `PIN_IN_USE`, nothing released |
| FORWARD with the pin fails (host status != 0, or the word at `rm_status_off` != 0, or the request never reached the host) | released at once |
| that FORWARD times out | **kept** (indeterminate: the GPU may hold the pages) until Close/exit |
| the reply is too short to contain the status word | **kept** (indeterminate) |
| a *successful* plain-Ioctl `NV_ESC_RM_FREE` (low 16 bits of `cmd == 0x4629`, `data_len == 16`, flat `NVOS00`) whose `hRoot`/`hObjectOld` match | every used pin of the caller with that `h_root` and `h_object == hObjectOld` is released; if `hObjectOld == hRoot` (client free) every used pin with that `h_root` is released. "Successful" = host status 0 **and** the `NVOS00.status` word (reply data + 12) 0 |
| successful `Close` of the pin's `handle` | released (after the host has torn the objects down) |
| process exit / device destroy | released (last step of teardown) |
| transport dropped (`StopDevice`) | released after the device reset, whether or not the owner's device was destroyed first |

`h_root` / `h_object` are the caller's word: the KMD does not verify them against the
registration. They only decide *when* the KMD unlocks. A caller that mis-tags can have its
own pages unlocked while the host still maps them (section 10.1). Pin ids are never reused;
on id exhaustion further pins are refused.

`UNPIN` (48 bytes): `pin_id`, `flags` (0). Not the caller's / not found -> `NOT_OWNED`.
`UNPIN` is for a pin that never reached a `FORWARD` (a failure path of the caller).

librmclient's sequence (`alloc_memory_pinned`): `PIN` with `h_root = hRoot`,
`h_object = hObjectNew` and the page-aligned `pMemory`/`limit + 1` of the `NVOS02` block;
then `NV_ESC_RM_ALLOC_MEMORY` as a `FORWARD` with `pin_id` and
`rm_status_off = offsetof(NVOS02_PARAMETERS, status)`; on a failure that never reached RM,
`UNPIN`.

### 4.5 EVENT_REGISTER / EVENT_UNREGISTER (ops 5, 6, 64 bytes)

Lets a user-mode event be signalled when the host reports that a backend file became
readable (the Linux `poll()` wakeup) or when the transport is lost.

Fields: `handle` (an owned backend handle, or 0 for `TRANSPORT_LOST`), `kind`
(`READY` 1, `TRANSPORT_LOST` 2), `event_handle` (user `HANDLE`, zero-extended; ignored by
UNREGISTER), `flags` (0), out `out_state`.

**Availability.** Events ride the device's second virtqueue (index 1, the event queue),
brought up at transport start with 16 buffers of 256 bytes, posted from StartDevice after the
ISR address is published. No virtio feature bit is involved. In particular the KMD never
acks `NVGPU_CFG_TAKES_INPUT` (bit 12), which would move keyboard and mouse onto
`InputEvent`s the Windows driver cannot use; a compile-time assertion keeps bit 12 out of the
acked set. If the queue could not be brought up: `QUERY_CAPS` reports no event ops and
`supported_event_kinds == 0`, `EVENT_REGISTER` answers `UNSUPPORTED`, `NvEvQ = 0`, and the
client falls back to polling.

**REGISTER**, in this order: `flags != 0 || kind == 0 || event_handle == 0` ->
`STATUS_INVALID_PARAMETER`; no transport -> `STATUS_DEVICE_NOT_READY`; queue not up ->
`UNSUPPORTED`; transport failed -> `TRANSPORT_RESET`; unknown `kind` -> `UNSUPPORTED`; the
handle is resolved with `ObReferenceObjectByHandle(EVENT_MODIFY_STATE, UserMode,
ExEventObjectType)` in the caller (a bogus handle -> `STATUS_INVALID_PARAMETER`, counted in
`NvEvErr`; the KMD holds a reference past closing the handle; auto- and manual-reset events
both work); `READY` on a handle the caller does not own -> `NOT_OWNED`; per-process or
device-wide table full -> `NO_RESOURCES`. Otherwise `OK` with `out_state`:
`REGISTERED` (1), `REPLACED` (2: the same `(device, handle, kind)` already had an event; the
old reference is released), `LATCHED_SIGNALED` (3: see below).

**UNREGISTER**: not gated on event availability. `OK` with `UNREGISTERED` (4) or
`NOT_FOUND` (5). It does not signal. `STATUS_DEVICE_NOT_READY` without a transport.

**Semantics.**

- A registration is keyed by `(owner, handle, kind)`. `TRANSPORT_LOST` registrations are
  keyed with handle 0 whatever `handle` was passed, so there is one per owner.
- **Persistent and level-triggered**, unlike the one-shot fence events: every notification
  does `KeSetEvent`; the registration stays until UNREGISTER, `Close` of its handle, device
  destroy or transport drop. Consumers must drain the RM event source until empty after
  every wake (the `poll()` contract) and reset a manual-reset event themselves (librmclient
  does `ResetEvent` after the wake, before draining). A spurious wake is harmless.
- **No lost wakeups.** An `EventReady` for a handle that has no `READY` registration is
  *latched* on the handle (`ready_latched`: one flag, not a count). The next `REGISTER` for
  that handle consumes the latch and signals at once (`LATCHED_SIGNALED`). An
  `EventReady` for a handle nobody has open is dropped (`NvEvDrop`), except while a
  `SEMSURF_FENCE_CREATE` is in flight (section 4.6). Note `UNREGISTER`
  followed by `REGISTER` can yield an immediate wake if a notification arrived in between.
- `TRANSPORT_LOST` does not latch. On a failed transport every registration of every kind
  is signalled (`NvEvLost` counts them) so blocked waiters give up and see the failure; the
  registrations stay until their owners' Close/exit or the transport's drop (which signals
  once more and releases the references at PASSIVE). A `REGISTER` on an already-failed
  transport is refused with `TRANSPORT_RESET`.
- Delivery path: the interrupt DPC drains the event queue (at most 16 buffers per call),
  reads the 16-byte message, and for `msg_type == 8` (`EventReady`) signals the registered
  events under the virtio lock with `KeSetEvent(Wait = FALSE)`. Each buffer is reposted
  at once. Any other message is counted (`NvEvOther`) and dropped; after 1024 such messages
  the queue is no longer kicked on repost (a host that served queue-1 kicks as requests could
  otherwise ping-pong with the DPC).
- Object references are only ever dropped at PASSIVE outside every lock; removal paths hand
  the event back by value.

### 4.6 Fence handles (`SEMSURF_FENCE_CREATE`)

nvidia-drm's semaphore-surface fences (`docs/SYNC.md`, host `nvidia/fence.rs`) turn an RM
semaphore value into a one-shot backend handle. The guest never `Open`s it: the handle
arrives inside the reply of a forwarded `Ioctl`, in the same handle namespace as `Open`
handles. The KMD records it as owned, so `EVENT_REGISTER` works on it and `Close` is allowed.

**Recognition** (all must hold, `kmd_logic/nvrm_fence.rs::is_fence_create`): the message is an
`Ioctl` on a handle the caller owns with `device_type >= 512` (a DRM node); the low 16 bits of
`IoctlReq.cmd` are `0x6455` (`('d' << 8) | (0x40 + 0x15)`, i.e. `DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE`
whatever its direction/size bits, which is all the host looks at too); `data_len == 24`;
and the device reports `NVGPU_CFG_DRM_FENCES` (config `features` bit 11, in
`QUERY_CAPS.device_features`). Without the bit a host passes the ioctl to the host driver
and the `fd` field would be a descriptor number of the backend process, which must never be
adopted. `0x54` (`FENCE_CTX_CREATE`, returns a GEM handle of the DRM file, not a backend handle)
and `0x56` (`FENCE_WAIT`, names a fence handle but creates none) are not tracked.

**Wire format** (read from the host, `dispatch_fence_create`): request data is
`struct drm_nvidia_semsurf_fence_create_params` (24 bytes: `u32 ctx`, `u32 timeout_ms`,
`u64 wait_value`, `s32 fd` @16, `u32 pad`). The host replies `MsgHeader` (16) | `IoctlResp`
(12, `data_len = 24`) | the same 24 bytes with the new backend handle in `fd`. The handle is
therefore the `u32` at **reply offset 28 + 16 = 44** (offset 16 of the data block). It is
taken only for `MsgHeader.status == 0`, `IoctlResp.data_len == 24`, a reply long enough to
hold all 24 data bytes, and a handle that is neither 0 nor `0xFFFFFFFF`. Anything else is
not recorded. The reply is returned to the caller as the device wrote it.

**Lifetime.** One slot is reserved before the host is asked, exactly as for `Open`: a full
table or the per-process quota (`MAX_NVRM_HANDLES_PER_OWNER`, 128, shared with every other
handle; the host's own cap is 4096 unsignalled fences) gives `NO_RESOURCES` and the host makes
no fence. The handle is recorded under `device_type = 511` (`DEVICE_TYPE_FENCE`): not a value
the host accepts in `Open`, and below 512 so it is never taken for a DRM node
(`ScanoutFlip.owner_handle`, `IMPORT_RM`). The host reports it **once**: one
`EventReady(handle)` when the semaphore reaches the value, or when the host driver gives up
(`hdr.status` is the fence's error, e.g. `-ETIMEDOUT`; the KMD does not read it, so a timed-out
fence wakes the waiter like a signalled one; check the semaphore). The handle then **stays
open on the host until `Close`**, so a client must `Close` every fence, fired or not. A
fired fence is closable. `Close`, device destroy and process exit release it like any handle
(event registrations released, host `Close` sent best effort and idempotently: the table
entry is removed first). A fence takes no message but `Close`: `Ioctl` on it is `FORBIDDEN`,
`MMAP` is `NOT_OWNED`, and the host answers `BadHandle` to anything else.

**No lost fire.** A fence made for an already-reached value fires at once, and the event
(event queue) can be consumed before the thread that waits for the create's reply (control
queue) has recorded the handle, when the ordinary latch has no slot to sit on. While at
least one `SEMSURF_FENCE_CREATE` is in flight (from before it is forwarded until its handle is
recorded or the create fails) the KMD keeps `EventReady`s for unowned handles in a 16-entry
table (`FenceBook`) and, when it records the new handle, takes the matching one in the same
lock hold and latches it. The first `EVENT_REGISTER` then answers `LATCHED_SIGNALED`. A fire
after recording takes the ordinary path (signal if registered, else latch). With no create in
flight nothing is kept, so a stale notification of a closed file cannot be taken for a later
fence that reuses its number. Overflow of the 16-entry table (more than 16 distinct fires
inside one create's round trip) loses a wake and is counted in `NvFenceErr`; a client should
wait with a timeout shorter than the 5 s host timeout and re-check the semaphore anyway.

Counters: `NvFence`, `NvFenceCl`, `NvFenceSig`, `NvFenceEarly`, `NvFenceErr` (section 7).

## 5. Ownership, quotas and limits

All `MAX_*` values are read from the code. "Per process" really means per device handle
(section 1).

| constant | value | where | meaning |
|---|---|---|---|
| `HELIOS_NVRM_MAX_BUFFER` | 1048576 (1 MiB) | protocol `nvrm.rs`; checked in `escape_nvrm_op` and FORWARD | largest escape buffer; larger is `STATUS_INVALID_PARAMETER` |
| `NVRM_DEFAULT_TIMEOUT_MS` | 30000 | `escape.rs` | FORWARD wait when `timeout_ms == 0` |
| host Mmap wait / host Munmap wait / teardown `Close` wait | 30000 / 5000 / 5000 ms | `virtio/nvrm.rs` | hard-coded |
| `ENQUEUE_RETRY_MAX_MS` | 5000 | `virtio/ctrl.rs` | ring-full retry budget before `NO_RESOURCES` |
| `MAX_NVRM_HANDLES` | 1024 | `nvrm_tables.rs` | backend handles, all owners (in-flight Opens count) |
| `MAX_NVRM_HANDLES_PER_OWNER` | 128 | same | per process; `QUERY_CAPS.max_handles` |
| `MAX_NVRM_MAPS` | 1024 | same | live MMAPs, all owners |
| `MAX_NVRM_MAPS_PER_OWNER` | 256 | same | `QUERY_CAPS.max_mappings` |
| `MAX_MAP_BYTES` | 256 MiB | `virtio/nvrm.rs` | one MMAP (bounds the MDL's non-paged cost) |
| `MAX_NVRM_PINS` | 1024 | `nvrm_tables.rs` | live pins, all owners |
| `MAX_NVRM_PINS_PER_OWNER` | 256 | same | `QUERY_CAPS.max_pins` |
| `MAX_NVRM_PIN_PAGES` | 262143 pages (just under 1 GiB) | same | one PIN; equals the indirect table's run capacity, so a fully scattered range still fits |
| `HELIOS_NVRM_PAGE_RUNS_MAX` / `page_runs::DIRECT_MAX_RUNS` | 1024 | protocol / `kmd_logic/page_runs.rs` | runs in a direct table (8 + 1024 x 16 bytes) |
| `page_runs::INDIRECT_MAX_RUNS` | 262143 | `page_runs.rs` | runs an indirect table can hold |
| `MAX_NVRM_EVENTS` | 1024 | `gpu/nvrm_events.rs` | registrations, all owners; storage reserved at init (0 if no event queue) |
| `MAX_NVRM_EVENTS_PER_OWNER` | 129 | same (`MAX_NVRM_HANDLES_PER_OWNER + 1`) | one `READY` per handle plus one `TRANSPORT_LOST` |
| `EVENT_QUEUE_SIZE` / `EVENT_BUF_BYTES` | 16 / 256 | same | event virtqueue (kept small on purpose: the by-value queue slot sits on the boot stack under `VirtioGpu::init`, gated by `tools/kmd-frame-sizes.ps1`) |
| `OTHER_KICK_LIMIT` | 1024 | same | non-`EventReady` messages after which reposts stop kicking |
| mapping id range | `1 .. 0x7FFFFFF0` | `virtio/nvrm.rs::mint_map_id`, `kmd_logic/nvrm_views.rs` | KMD-minted from ONE counter for the life of the driver (never restarted by a new transport, never reused); the table key is `id | 0x80000000` in `AdapterContext::mappings` |
| `HELIOS_NVRM_SCANOUT_FLIP_BYTES` / `MSG_HEADER_BYTES` | 64 / 16 | protocol | |
| `DEVICE_TYPE_DRI_FIRST` | 512 | `virtio/nvrm.rs` | first DRM-node `device_type` (`512 + minor`); 255 control, GPU minors, 256 UVM, 257 UVM tools |

What is owned, and what is checked:

- **Handles**: `nvrm_handles` (owner, handle, device_type, latch); `Open` handles and, as
  `device_type` 511, fence handles (section 4.6). `Ioctl`, `Close`, `MMAP`,
  `PIN`, `EVENT_REGISTER(READY)` and `ScanoutFlip.owner_handle` check it. The check and the
  record are done under one lock hold wherever a concurrent `Close` could otherwise let a
  stale number name another process's file (`push_nvrm_map`, `push_nvrm_pin`,
  `register_nvrm_event`).
- **Mappings**: `nvrm_maps` (owner, handle, kmd_id, host_id) plus the user view in
  `AdapterContext::mappings`.
- **Pins**: `nvrm_pins`; ids never reused.
- **Events**: `nvrm_events` registry in `kmd_logic/nvrm_events.rs` (pure, host-tested).
- Capacity of every table is reserved at init, so nothing allocates under the spinlock.
  Anything whose drop must run at PASSIVE (locked MDL, contiguous buffer, event
  reference) is handed back by value from `take_*` and released after the lock.
- **The KMD itself is a client of this pipe** behind the `KmdRmClient` knob (default off): its
  handles are recorded under the reserved owner `DeviceOwner::KMD_RM`, which no escape can
  present (`DeviceOwner::new` refuses it), so `close_all_for_owner` never touches them, the
  per-owner quotas above apply to it alone, and the transport-wide sweep of section 6 closes
  them with everybody's. Its traffic is counted in `NvOpen`/`NvClose`/`NvIoctl` like any
  client's. Design and counters (`Rm*`): `kmd-rm-client.md`.

## 6. Teardown rules

**`Close` (FORWARD, msg 2)**, in order:

1. The handle is removed from the table first (the host may hand the number to another
   process the moment it closes it). Not the caller's -> `NOT_OWNED`.
2. Every mapping the caller holds on that handle is unmapped (user view first) and its host
   `Munmap` sent (stops sending after the first timeout/device error; still unmaps every
   view). The ABI's order is unmap, then close.
3. The `Close` is forwarded.
4. On `MsgHeader.status == 0`: the handle's event registrations are released and its pins
   unlocked. On a nonzero status or a transport error that never reached the host: the
   handle is restored to the table (note: its mappings are already gone) and events/pins are
   kept. On a **timeout**: the handle stays forgotten, its events are released, its pins stay
   until device destroy (the handle is no longer the caller's, so nothing else can release them).

**Device destroy / process exit** (`DxgkDdiDestroyDevice` -> `nvrm::close_all_for_owner`),
after the device's user mappings were drained in the owning process:

1. Event registrations of the owner (including `TRANSPORT_LOST`): dereferenced, **not**
   signalled.
2. Host `Munmap` for every remaining mapping.
3. Host `Close` (5 s) for every handle left open. After the first timeout/device error it
   stops sending but still clears the tables so nothing outlives the device handle.
4. Pins last: the host no longer holds an alias of the pages, so they are unlocked.

**Transport loss / reset.** When the transport fails, every event registration is signalled
(section 4.5); handles, mappings and pins stay in the tables until their owners close or
exit (`close_all_for_owner` then sends nothing to a transport that has already failed, but
still clears the tables and unlocks the pins).

**Device reset does NOT make the host drop anything.** The host backend keeps its RM files,
registrations (and with them its alias of every OS-descriptor page) across a guest device
reset: QEMU's generic vhost-user device never sends `RESET_DEVICE`, and the backend resets
only at its next feature negotiation, i.e. the next `StartDevice`. Between the reset and
then the host may still hold, and the GPU still write, a page the guest has unlocked. So
the guest never unlocks pinned pages on the strength of a reset; it unlocks them after the
host closed the files that hold them.

**Retiring the transport** (`StopDevice`, and `StartDevice` when it finds a transport no
stop retired; both go through `nvrm::retire_transport`; `RemoveDevice` without a stop runs
the first step too). While the transport is still alive:

1. `nvrm::close_all_on_host`, for ALL owners at once: host `Munmap` for every mapping, host
   `Close` (5 s) for every handle left open (fence handles counted in `NvFenceCl`), then
   every pin is unlocked. Best effort and bounded: it stops sending after the first
   timeout or failed send, or after 10 s in total, and keeps clearing the tables. A
   transport that has already failed (`transport_failed()`) or is absent is not asked, and
   this step does nothing.
2. `set_virtio(None)`: the transport is dropped, the device reset, and `VirtioGpu::drop`
   releases what is left, without sending anything (fallback, see below).
3. The user views of the dropped transport's mappings are marked stale (below).

**Fallback sweep** (`VirtioGpu::drop`, for a transport that failed, was never asked, or was
re-populated by a call racing the sweep). The device is reset first, then:

1. event registrations are signalled once more and dereferenced (PASSIVE);
2. every pin is unlocked (`NvrmPin`'s `Drop` is the unlock, so a pin cannot leave the tables
   any other way than unlocked): the MDLs are user MDLs that are never mapped, and
   `MmUnlockPages` on such an MDL does not need the owning process (the same helper,
   `helios_unlock_system_buffer`, already runs from `close_all_for_owner` in whatever
   context the destroy arrives in). User pages left locked would bugcheck the owning process
   at exit (0x76). Here the host was NOT asked to let go, so this is the one place a page
   can be unlocked while a (wedged or dead) host still holds it; it is accepted because the
   alternative is the bugcheck, and the host side is gone or not answering;
3. the handle and host-mapping records are dropped (fence handles counted in `NvFenceCl`);
   `NvSwept` counts the entries 2-3 found (0 when the live sweep, or dxgkrnl destroying every
   device first, emptied the tables).

All of this is idempotent against `DestroyDevice` -> `close_all_for_owner`: they take
entries out of the same tables under the virtio lock, so whichever runs first releases them
and the others find nothing (after the drop `with_virtio` fails and the per-device path does
nothing).

**User views of a stopped transport.** The views made by `MMAP` live in
`AdapterContext::mappings`, which deliberately outlives the transport (as blob views do): a
user view can only be unmapped inside the process that made it, and `StopDevice` is not that
process. After the drop they point at BAR memory the host no longer backs for them, and the
next transport may hand the same window offsets to another process. So retiring the transport marks
them stale (`MappingTable::mark_nvrm_views_stale`, `NvStale`, taken AFTER the drop so no older
id can still be minted), and the owner's NEXT `HELIOS_ESCAPE_NVRM` call of any op unmaps its
own stale views first, in its own process (`nvrm::reclaim_stale_views`, `NvStaleUn`): a process
that touches one afterwards takes an access violation instead of reading someone else's
memory. A process that never calls again keeps them until its `DestroyDevice` drain, which
takes every view regardless of the transport, as it always did, and does not slow anyone
else down: the "anything to reclaim" test is per owner (`helios_kmd_logic::nvrm_views::
StaleOwners`, up to 16 owners exactly, then conservative until the next locked scan repairs
it), so a process with nothing stale pays a few atomic loads whatever the others hold. A
`MUNMAP` of such an id answers `NOT_OWNED` (the escape-entry sweep already removed the
view). Because the views outlive the transport, mapping ids come from one driver-wide
counter, so a new transport can never mint an id an old view still holds. An `MMAP` caught
between its table push and its view insert while the transport is retired is marked by the
insert itself (`MappingTable::insert_unique` sees an id below the stale line and flags its
owner), so its next call reclaims it.

A new transport starts with empty tables and a new `epoch`.

## 7. Counters (registry)

Written as `REG_DWORD` values (names at most 14 characters) under
`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`. Read them headless, e.g. over
the win11 SSH session:
`reg query HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render` and look at `Nv*`.

**Publication lag.** `publish_nvrm_counters` runs from the NVRM escape when the "shape"
(a hash of `NvOpen NvClose NvMap NvPin NvUnpin NvEvReg NvEvUnreg NvFence NvFenceCl`) changed since the last
escape, or on every 256th NVRM escape call, and also on the present edge. The other
counters can therefore be up to 255 calls stale: do a few extra calls (or a present, or
change a shape counter) before reading, or compare after the process has exited.

| value | what it counts | healthy reading |
|---|---|---|
| `NvOpen` | `Open`s attempted (counted after the slot is reserved, before the host answers; failed host opens included) | moves with each opened RM file |
| `NvClose` | forwarded `Close`s that passed the ownership check (before the host answers) | `NvOpen - NvClose` is the handles still open or left to teardown. Teardown closes (device destroy) are **not** counted here, so a crashed client leaves a permanent difference |
| `NvIoctl` | forwarded `Ioctl`s that passed the checks (pinned registrations included) | moves |
| `NvOther` | forwarded `GetProcFiles`/`GetSysFiles` | at least 1 per process that initialised librmclient (it reads `GetSysFiles` once) |
| `NvRef` | FORWARDs the KMD refused by policy or ownership (msg type, not owned, forbidden, bad range, handle quota) | **0** in a normal run; nonzero only in deliberate negative tests. Transport errors are not counted here |
| `NvMap` | successful `MMAP`s, cumulative (there is no unmap counter) | moves with CPU maps |
| `NvMapErr` | `MMAP` failures after the ownership/range prechecks | **0** |
| `NvFlip` | forwarded `ScanoutFlip`s (counted after validation, before the round trip) | one per zero-copy present |
| `NvPin` | pages locked and a pin made | |
| `NvUnpin` | pins released by any path (UNPIN, failed registration, RM_FREE, Close, teardown) | `NvPin - NvUnpin` is what is locked now; **equal** when no client is running. A difference that only grows is a leak |
| `NvPinErr` | pin failures after the early prechecks (lock refused, table build failed, table push refused) | **0** |
| `NvEvQ` | written once at transport init: 1 = event queue up, 0 = events unsupported | **1** |
| `NvEvReg` | successful `EVENT_REGISTER`s (replacements included) | |
| `NvEvUnreg` | registrations removed by `EVENT_UNREGISTER` | `Close`, exit and teardown removals are not counted, so only the order of magnitude is a check: a count far above anything that removes is a leak |
| `NvEvRef` | `EVENT_REGISTER` refusals (unsupported, lost, not owned, full) | **0** |
| `NvEvSig` | `KeSetEvent`s for an `EventReady`, plus a latched one at registration | moves with host notifications |
| `NvEvLatch` | notifications latched because nothing was registered | small; each should be followed by a `LATCHED_SIGNALED` register |
| `NvEvDrop` | `EventReady` for a handle nobody has open | small (host fences, closed files) |
| `NvEvLost` | registrations woken by a failed or dropped transport | **0** until a transport failure |
| `NvEvOther` | queue messages other than `EventReady` | **0** (nonzero means the host sent e.g. `InputEvent`) |
| `NvEvErr` | event-queue faults: a buffer that would not repost, a bad token, **or a `REGISTER` whose `event_handle` did not resolve** | **0** |
| `NvFence` | fence handles recorded as owned (section 4.6) | moves with `SEMSURF_FENCE_CREATE`s |
| `NvFenceCl` | fence handles released: a successful or timed-out `Close`, plus device-destroy closes and transport sweeps (live or fallback; unlike `NvClose`) | `NvFence - NvFenceCl` is the fences open now; **equal** when no client runs |
| `NvFenceSig` | `EventReady`s for fence handles (one per fire), including the early ones | at most `NvFence` |
| `NvFenceEarly` | fires that arrived before their handle was recorded and were latched at record time (also counted in `NvFenceSig` and `NvEvLatch`) | small; nonzero only when the semaphore was already reached |
| `NvFenceErr` | a create the host answered with status 0 whose reply could not be recorded (short or bad reply, duplicate handle), or a notification lost to a full early table | **0** |
| `NvSwept` | handles, mappings and pins still tracked when the transport was dropped, AFTER the live host-close sweep (so only what a failed transport, a wedged host or a racing call left), cumulative | **0** when dxgkrnl destroys every device first; nonzero means the sweep did the owners' work |
| `NvStale` | user views of a dropped transport that `StopDevice` marked stale, cumulative | usually 0 |
| `NvStaleUn` | of those, how many owners' next NVRM escape unmapped | follows `NvStale`; the rest is reclaimed by `DestroyDevice` |
| `NvWinMb`, `NvAptMb` | size in MiB of shared-memory region 1 (RM window) and 2 (UVM aperture), written at init | nonzero, or `MMAP` answers `UNSUPPORTED` |

`Fg*` counters belong to the foreign-resource verb (`zero-copy-present.md`), not this
escape.

## 8. Test programs and running them in win11

Host-side, no GPU needed:

- `kmd_logic` tests: the event registry (`kmd_logic/src/nvrm_events.rs`), the fence rules
  (`nvrm_fence.rs`: recognition, reply parsing, the early-fire table) and run-table
  builder (`page_runs.rs`): `cargo test` in `guest/windows/kmd_logic`. The KMD crate itself
  cannot host a test harness (`panic = "abort"` cdylib), so logic worth testing lives in
  `kmd_logic`.
- librmclient (branch `feat/nvk-rm-windows-transport`): `tests/test_unit.c` (fake RM
  transport) and `tests/test_win_wire.c` (the wire builder), run by `meson test` or
  `make check`. They never touch the KMD.
- Layout drift: `const` assertions in `nvrm.rs` and `_Static_assert`s in the C header. There
  is **no** automated header-versus-Rust comparison for this ABI (the foreign-resource ABI
  has one); keep both files in step by hand.

On a real guest (all four are on `feat/nvk-rm-windows-transport`, in `guest/rmclient/tests`;
they need the Conduit GPU stack and are built by meson, never run by `meson test`):

| program | exercises | pass criterion |
|---|---|---|
| `crm_smoke` | Open/Close, Ioctl with nested blocks, GetSysFiles, MMAP/MUNMAP (system memory, BAR memory, usermode doorbell via subdevice), GPU map, optional OS event | prints `SMOKE PASSED`; "objects still tracked: 0, CPU mappings: 0" |
| `crm_pin_smoke` | PIN + registration FORWARD (`pin_id`, `rm_status_off`), GPU map, free (RM_FREE releases the pin), at 2 MiB (direct table) and 512 MiB (indirect table) | `PIN SMOKE PASSED`; afterwards `NvPin == NvUnpin` |
| `crm_event_smoke` | EVENT_REGISTER via `crm_event_wait`: arms `NV2080_NOTIFIERS_SW`, triggers it, expects the event to fire and `crm_event_drain` to return the notification(s) | `EVENT SMOKE PASSED`; `NvEvReg >= 1`, `NvEvSig >= 1`, `NvEvOther`, `NvEvErr`, `NvEvLost` all 0 |
| `crm_scanout_smoke [seconds=60] [dri_index=0] [frame_ms=50]` | RM video memory -> export -> `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` on a DRM node -> repeated `ScanoutFlip` (1920x1080 XRGB, three buffers) | no `[FAIL]`; `NvFlip` advances by the number of flips; no viewer is needed for a pass, the counter is the check |

Build (Linux host, MinGW-w64; the `.ini` is `guest/nvk-rm/windows/mingw-x86_64.ini`):

```sh
cd guest/rmclient
meson setup build-win --cross-file ../nvk-rm/windows/mingw-x86_64.ini
ninja -C build-win          # crm_smoke.exe crm_pin_smoke.exe crm_event_smoke.exe
                            # crm_scanout_smoke.exe librmclient.dll test_unit.exe ...
```

Run in win11: win11 is the Windows guest and also the driver build VM; its SSH defaults
are in `guest/windows/ci/vm/win-build.sh` (`WIN_SSH` default `Ole Algoritme@127.0.0.1`,
`WIN_PORT` 2222). Install the KMD package as for any Helios test (restart the guest only
through the project's checked restart script), `scp` the `.exe` files and `librmclient.dll`
next to them into the guest, and run them over `ssh -p 2222`. Check first that
`reg query` shows `NvEvQ = 1` and nonzero `NvWinMb`; then run `crm_smoke`, `crm_pin_smoke`,
`crm_event_smoke`, and finally `crm_scanout_smoke`; read the `Nv*` counters after each
(section 7 for the publication lag). Whether `D3DKMTEscape` works from an SSH (non-console)
session is **UNVERIFIED** in this document; the transport README reports `crm_smoke` and
`crm_pin_smoke` passing in the win11 guest, without saying how they were launched.

## 9. librmclient's use of the ABI (Windows transport)

For orientation, from `src/transport_windows.c` on `feat/nvk-rm-windows-transport`:

- Finds the adapter by name hint (`Helios`/`VIRTIO GPU`), creating a D3DKMT device and
  context and probing with `QUERY_CAPS`; requires `supported_ops` to include `FORWARD`
  and `max_buffer_bytes >= 4096`; remembers `max_buffer` and the init `epoch`.
- A transport "fd" is the backend handle returned by `Open`, so fds written into RM payloads
  already mean what the host expects.
- Maps CPU memory with Linux's channel-per-mapping protocol, the final map being `MMAP`;
  the channel is kept until `NV_ESC_RM_UNMAP_MEMORY`.
- `event_wait` creates a manual-reset event per channel, `EVENT_REGISTER(READY)`, waits,
  `ResetEvent`s, and the caller drains. On `feat/nvk-rm-windows-transport` it never
  registers `TRANSPORT_LOST`; on `rmc/transport-loss` (`2995935`) it does, see the next item.
- Transport loss (`rmc/transport-loss`): init refuses a KMD that reports `epoch == 0` (no
  transport; retried by the next `crm_open`) and registers ONE process-wide `TRANSPORT_LOST`
  event (handle 0, when `QUERY_CAPS` offers the op and the kind). `event_wait` waits on the
  channel event and that event with `WaitForMultipleObjects`; a loss ends it with `-ENODEV`,
  even an infinite wait. The KMD wakes every registration on a loss, so the loss is
  re-checked after a channel wake. Every escape reply is judged too: `TRANSPORT_RESET`, an
  `epoch` other than the init one (including 0), or `STATUS_DEVICE_NOT_READY` /
  `STATUS_DEVICE_REMOVED` latches the loss for the life of the process, and every call then
  answers `-ENODEV` without a round trip, except `MUNMAP`, `UNPIN` and `EVENT_UNREGISTER`,
  which still go to the KMD (an escape from the owner is how the KMD reclaims the CPU views
  a stopped transport left, section 6). The rules are pure inline helpers in
  `helios_nvrm_escape.h`, tested by `tests/test_win_wire.c`; the wait itself has only been
  cross-compiled. A failed-but-not-replaced transport keeps its epoch and answers
  `DEVICE_ERROR` (-> `EIO`) to forwards: only the `TRANSPORT_LOST` event (or a `TRANSPORT_RESET`
  from `EVENT_REGISTER`) reveals it, so a KMD without events gives no loss detection there.
- KMD statuses become errno: NOT_OWNED -> `EBADF`, MSG_TYPE_REFUSED/FORBIDDEN -> `EPERM`,
  TRANSPORT_RESET -> `ENODEV`, NO_RESOURCES -> `EMFILE`, UNSUPPORTED -> `ENOSYS`,
  TIMEOUT -> `ETIMEDOUT`, BAD_RANGE -> `EINVAL`, anything else -> `EIO`; escape NTSTATUS
  NOT_IMPLEMENTED -> `ENOSYS`, INVALID_PARAMETER -> `EINVAL`.
- The file's header comment and the branch's README still say events and PIN are not
  implemented; the code below the comment implements both (section 10.3).

## 10. Known gaps, TODO and what could not be verified

### 10.1 Known gaps (deliberate)

- **Security hardening is deferred.** Only the physical-address rule and handle/pin/map
  ownership are enforced. Not done: per-process resource accounting beyond the counts above,
  in-flight reference counts on handles (a handle closed by one thread while another is
  inside a `FORWARD` on it can still name a recycled number: the commit-time rechecks narrow
  but do not close this), validation of RM structures, rate limiting, auditing which RM
  classes a process may allocate.
- **Pin tags are trusted.** `h_root`/`h_object` on `PIN` are the caller's word; the KMD only
  uses them to decide when `RM_FREE` unlocks a pin. A mis-tagging process can unlock its own
  pages while the host still maps them. Hardening TODO: take the object handle from the
  registration reply itself.
- **A committed pin cannot be released by the user** (`PIN_IN_USE`); a process that registers
  and never frees keeps the pages locked until Close/exit.
- **`TRANSPORT_LOST` reaches librmclient only on `rmc/transport-loss`** (`2995935`, not merged
  into `feat/nvk-rm-windows-transport` here): the earlier Windows transport registers only
  `READY`, so a thread blocked in `event_wait` with an infinite timeout is not woken by a
  transport failure and the epoch is never re-read (section 9 describes the fix).
- **`IMPORT_RM`** (`HELIOS_ESCAPE_FOREIGN_RESOURCE`, verb 0x0018, a different verb) is gated
  off: `RM_IMPORT_SERVED = false` in `virtio/foreign.rs`, `CAP_RM_IMPORT` not advertised,
  answers `ST_UNSUPPORTED` before touching state. The host half and the NVK/UMD half do not
  exist. See `zero-copy-present.md`.
- `HELIOS_NVRM_ST_RESP_TRUNCATED` is never produced; an undersized `resp_cap` is not detected.
- `ScanoutFlip` forwarding is the only path that accepts a non-RM message; `scanout != 0` is
  refused (single scanout).
- Host-side indeterminate cases: a timed-out `Open` or `SEMSURF_FENCE_CREATE` can leave an
  untracked host handle (a fence ends by itself within the host driver's 5 s timeout but its
  handle stays until the host side is reset); a timed-out `Close` is treated as closed.
- Fence handles count against the per-process handle quota (128) with every file. A client
  that keeps hundreds of fences in flight must close fired ones promptly (section 4.6).

### 10.2 UNVERIFIED (could not be checked from source alone)

- That any of this runs as described on the current KMD in a Windows guest: it was read,
  not executed. The transport README's pass reports predate the event/flip work and the
  v307 event queue fixes.
- Whether `StopDevice` is always preceded by `DestroyDevice` for every device. It no longer
  matters for correctness: the transport's drop unlocks pins and clears the tables, and the
  views are reclaimed by their owners (section 6). `NvSwept` / `NvStale` on a real stop will
  say which order dxgkrnl uses.
- That `MmUnlockPages` of a never-mapped user MDL from a foreign process context (the
  `StopDevice` thread) is accepted by the memory manager. It is the documented behaviour of
  that routine (the MDL records the locking process) and the same helper is already relied on
  from `DestroyDevice`, but it was not run.
- Anything about the retire sweep on a real host: the order (Munmap, Close, then unlock), the
  10 s / first-failure bound and the stale-owner set were read and (for the pure set) unit
  tested, not run. Not covered: a call racing the sweep that opens or pins after it has
  passed (the fallback unlocks that pin without a host close).
- That, when a process crashes, `DestroyDevice` runs in a context where unmapping the user
  views is valid; the code comments assume the creating process.
- That `D3DKMTEscape` from a non-console SSH session works for the smoke programs.
- Whether `crm_event_smoke` and `crm_scanout_smoke` pass on win11 with KMD 307 (the README's
  recorded results predate them). `crm_event_smoke` additionally needs the host backend to
  serve `NV_ESC_RM_GET_EVENT_DATA` (0x52), which the README says the backend profile
  refused at the time.
- The registry counter publication rate was derived from the code, not measured.
- Fence handles (section 4.6) were written from the host code and `docs/SYNC.md` and
  checked with host unit tests of the pure logic only: that the reply offset, the one-shot
  `EventReady` and the early-fire path behave this way on a live win11 + backend with
  `NVGPU_CFG_DRM_FENCES` set, that the Windows transport actually sends cmd `0x6455` with
  `data_len 24` on a DRM node handle, and the measured semaphore-release-to-`KeSetEvent`
  latency (the design doc's X3) are all unrun.
- Whether the frame-size gate (`tools/kmd-frame-sizes.ps1`) still passes: `VirtioGpu`
  gained about 80 bytes (`FenceBook`) on the `VirtioGpu::init` frame.

### 10.3 Comments and docs that disagree with the code

The source files cannot be changed by this document; these should be fixed when code is next
touched.

- `nvrm.rs` module docs (SECURITY section) say a committed pin is released only by `Close`,
  exit or reset and that `RM_FREE` release "is not part of ABI v1". The `HeliosNvrmPin` doc
  and the code do release on a successful `RM_FREE` (section 4.4).
- `nvrm.rs` says the escape returns `STATUS_DEVICE_NOT_READY` for "no virtio transport"; only
  the event ops do. Other ops report `NOT_OWNED` (handle lookups fail) or `DEVICE_ERROR` in
  the header.
- `nvrm.rs` describes `TRANSPORT_RESET` as a general status; only `EVENT_REGISTER` produces it.
- `nvrm.rs` `BAD_RANGE` says an `IoctlReq` whose lengths "do not fill the request" is refused;
  the code refuses only lengths that overrun it.
- `nvrm.rs` `HeliosNvrmMmap` says `mapping_id` is the host's id; the KMD returns its own
  minted id (the RM path's host id is 0).
- `nvrm.rs` says `epoch` changes "when the device is reset"; it is the transport instance's
  fence base and changes at StartDevice only (0 with no transport).
- Commit `0185243` says only the UVM aperture is mappable; the code also maps the RM window.
- `transport_windows.c` header comment and the transport-branch README say events and PIN
  answer `-ENOSYS`; both are implemented in that file (corrected on `rmc/transport-loss`).

## 11. How to add an op: checklist

1. **ABI, Rust** (`guest/windows/protocol/src/nvrm.rs`): new `HELIOS_NVRM_OP_*` number,
   a `repr(C)`, `Pod`, padding-free, 8-aligned struct starting with `HeliosNvrmHeader`, a
   `*_BYTES` constant, `offset_of!` / `size_of!` assertions in the `const` block, new
   `HELIOS_NVRM_ST_*` only if no existing code fits. No pointers or `HANDLE`s; reserved
   fields are "zero in, zero out". Additive: do not bump `HELIOS_NVRM_ABI_VERSION`.
2. **ABI, C** (`guest/rmclient/src/helios_nvrm_escape.h`): the same struct, constants and
   `_Static_assert`s. Nothing checks the two files against each other automatically.
3. **Handler** (`kmd_render/src/ddi/escape.rs`): add an `nvrm_<op>` function using
   `EscapeBuf::<YourStruct>::new(buf, hdr)` (short buffer -> `STATUS_BUFFER_TOO_SMALL`),
   reject nonzero reserved fields with `STATUS_INVALID_PARAMETER`, answer with the KMD
   verdict in `head.status` and `head.epoch = epoch`, return `STATUS_SUCCESS` for every
   well-formed request. Add the dispatch arm in `escape_nvrm_op`, and the op's bit in
   `NVRM_OPS_IMPLEMENTED` (or gate it like `NVRM_EVENT_OPS`). New limit visible to clients:
   add a `QUERY_CAPS` field (needs the ABI struct and both asserts) or extend an existing one.
4. **Ownership.** Any handle, mapping id or pin id the request names must be checked against
   the owner (`nvrm_handle_owned`, `take_nvrm_map`, ...) and answer one code (`NOT_OWNED`) for
   "not yours" and "does not exist". Record-and-check under one lock hold when a concurrent
   `Close` could race.
5. **Tables and quotas** (`virtio/gpu/nvrm_tables.rs`, `kmd_logic` for pure logic): add
   `MAX_*` and `MAX_*_PER_OWNER`, reserve capacity at init (no allocation under the spinlock),
   mint ids that are never reused, return anything with a PASSIVE-only drop by value from
   `take_*` and release it after the lock. Put testable logic in `kmd_logic` with host tests.
6. **Teardown.** Release on `Close` of the owning handle (the Close arm in
   `virtio/nvrm.rs::forward`, after the host reply is known), in `close_all_for_owner` (mind
   the order: events, mappings, handles, pins), and on transport drop. Decide what a
   `Close` that times out does. A failed `Close` restores the handle.
7. **IRQL.** Escapes run at PASSIVE; table methods run under the virtio spinlock at
   DISPATCH. No allocation, no wait, no `ObDereferenceObject`/`MmUnlockPages` under the lock.
8. **Counters.** Add `static AtomicU32`s in `virtio/nvrm.rs`, mirror them in
   `ddi/submit_command.rs::publish_nvrm_counters` (value names at most 14 characters),
   add session-shaping ones to the `shape` list in `nvrm_publish_counters_if_due`. Count every
   refusal and failure path, and add the new rows to section 7 with a healthy value.
9. **Transport and tests.** Use it from `src/transport_windows.c`, map the new statuses in
   `kmd_status_to_errno`, add or extend a `crm_*_smoke` and a host unit test where the logic
   can be exercised without a GPU.
10. **Docs and version.** Update this file (sections 4, 5, 7), keep `QUERY_CAPS` truthful, bump
    the KMD driver version (`guest/windows/kmd_render/driver-version.env` is the single source;
    the `tools/win-mcp` helper that `build.rs` mentions for bumping is not in this tree).

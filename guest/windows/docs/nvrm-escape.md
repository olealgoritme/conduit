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

**Security stance (two rules; the second is counted, not yet refused).** User mode never supplies or sees a
guest-physical address. Page-run tables are built only by the KMD from pages it locked
(`PIN`); a `FORWARD` that carries a page-run `deep_ptr_offset` is refused. The second rule
is that a request may only name RM clients and backend handles of the process that sends it:
section 12 (`NvDupHarden`: log-only by default in the first shipped package, refusing with
`NvDupHarden` = 1). Everything else is deferred (section 10).

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
are usable, `0b1110` with the host's buffer-release event acked, else 0), `supported_cache_types` (`0b1111`), `device_features` (the virtio config
`features` word read at init), `max_handles` 4096, `max_mappings` 4096 (128 and 256 under `NvWinPolicy` = 0), `max_pins` 256,
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
| Ioctl (3) | Handle must be the caller's (`NOT_OWNED`). `check_ioctl`: request >= 40 bytes; `IoctlReq` at offset 16: `data_len` @20, `nested_len` @28, `deep_ptr_offset` @32, `deep_len` @36; `deep_ptr_offset` equal to `0xFFFFFFFE` or `0xFFFFFFFD` is `FORBIDDEN`; `40 + data_len + nested_len + deep_len > req_len` is `BAD_RANGE`. (Extra trailing bytes are accepted; only an overrun is refused.) A handle that is a fence (section 4.6) is `FORBIDDEN`. A request whose payload names an RM client or a backend handle that is not the caller's is `NOT_OWNED` (section 12, after the length check). A successful `NV_ESC_RM_ALLOC` of a root class records the client it made as the caller's, a successful free of it forgets it (section 12). A `SEMSURF_FENCE_CREATE` on a DRM node is recognised and its reply handle is recorded (section 4.6). |
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
   (`MAX_MAP_BYTES`), or `offset` not page-aligned -> `BAD_RANGE`. A mapping-table bound or
   the window policy (section 13: window full, the reserve, a map that could never fit; the
   legacy per-device quota under `NvWinPolicy` = 0) -> `NO_RESOURCES`, counted by reason.
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
(`READY` 1, `TRANSPORT_LOST` 2, `SCANOUT_RELEASED` 3: handle 0, only with the release capability,
`foreign-scanout.md` "Buffer release"), `event_handle` (user `HANDLE`, zero-extended; ignored by
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
table or the per-process sanity bound (`MAX_NVRM_HANDLES_PER_OWNER`, 4096 since the tables
grow, 128 under `NvWinPolicy` = 0; shared with every other handle; the host's own cap is 4096
unsignalled fences) gives `NO_RESOURCES` and the host makes no fence. The handle is recorded under `device_type = 511` (`DEVICE_TYPE_FENCE`): not a value
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

### 4.7 WINDOW_INFO (op 13, 88 bytes)

A read-only report of the RM window for NVK's live `VK_EXT_memory_budget` (policy and
accounting: section 13). Advertised by `QUERY_CAPS.supported_ops` bit 13 (the op) and bit 36
(`HELIOS_NVRM_CAP_WINDOW_INFO`; bits 32..35 are `SCANOUT_FENCE`, `PRESENT_FENCE`, `FLUSH_GATE`,
`SCANOUT_RELEASE`). `HeliosNvrmWindowInfo` (`protocol/src/nvrm.rs`, `guest/rmclient/src/
helios_nvrm_escape.h`), little endian:

| offset | field | |
|---|---|---|
| 0..39 | `head` | `op` = 13 |
| 40 | `u64 window_bytes` | the RM window's size as the device reports it (0: no window, or the transport is down) |
| 48 | `u64 window_used_bytes` | mapped now by every process (the UVM aperture is another region and not counted) |
| 56 | `u64 owner_limit_bytes` | the ceiling that applies to the caller. Dynamic policy: a ceiling on `window_used_bytes` (flag bit 2): `window_bytes` for the privileged device (the shell's), `window_bytes - reserve` (default reserve 256 MiB) for everyone else; `NvWinMaxMb` lowers the window first. `NvWinPolicy` = 0: a quarter of the window, a ceiling on `owner_used_bytes` |
| 64 | `u64 owner_used_bytes` | this process's (device's) bytes of the window, all its window `MMAP`s (UVM aperture maps excluded); 0 for a process with none |
| 72 | `u64 generation` | bumps when the window size or the policy may have changed (once per transport start today; 0 before the first) |
| 80 | `u32 flags` | bit 0: `owner_limit_bytes < window_bytes` (this process cannot use the whole window); bit 1: the window can grow (always 0 today); bit 2: the limit is a ceiling on the all-owners total |
| 84 | `u32 reserved` | 0 |

Room left for the caller: `owner_limit_bytes - window_used_bytes` with bit 2 set,
`owner_limit_bytes - owner_used_bytes` with it clear; the host may still refuse a map it cannot place
(`DEVICE_ERROR`, errno in `flags` of `MMAP`; `NvWinRHost`). The status is always `OK`; no side
effect, no host round trip, no registry write (the counters are mirrored by the worker); a few
atomic reads, one read of the scanout state's leaf lock (is the caller the shell's device) and one
short virtio-lock hold over the window account (`O(owners)`), so it is cheap enough for every new
memory chunk; a UMD caches the answer for 10 ms. A buffer shorter than 88 bytes answers `BAD_RANGE`
in the header (when the 40-byte header fits; otherwise `STATUS_BUFFER_TOO_SMALL` like every op) and
is counted in the short-buffer counter (QUERY_STATS `out_escape_short_buffer`). Counter: `NvWinInfo`
(calls). Tests: `protocol` (`window_info_*`: offsets, little-endian bytes, 64-bit fields, op and
capability bits free) and `kmd_logic::rm_window` (`info_*`).

**Phase 2 (design only, not implemented).** A per-process read-only page, mapped like the read
ledger (`HELIOS_ESCAPE_MAP_READ_LEDGER`) and refreshed under a seqlock by the KMD whenever the
window account changes (the table doors already call `mirror`), would let the UMD read the budget
with no escape at all: `{seq, window_bytes, used, owner_limit, owner_used, generation, flags}`
with the usual odd-while-writing sequence word. It is per process because `owner_limit` and
`owner_used` are; one page per device, written for every device on every change would cost
`O(devices)` per map, so the writes would be lazy (marked dirty at the change, refreshed by the
next escape of that device or by the worker). Not needed while the escape costs about a microsecond.

## 5. Ownership, quotas and limits

All `MAX_*` values are read from the code. "Per process" really means per device handle
(section 1).

| constant | value | where | meaning |
|---|---|---|---|
| `HELIOS_NVRM_MAX_BUFFER` | 1048576 (1 MiB) | protocol `nvrm.rs`; checked in `escape_nvrm_op` and FORWARD | largest escape buffer; larger is `STATUS_INVALID_PARAMETER` |
| `NVRM_DEFAULT_TIMEOUT_MS` | 30000 | `escape.rs` | FORWARD wait when `timeout_ms == 0` |
| host Mmap wait / host Munmap wait / teardown `Close` wait | 30000 / 5000 / 5000 ms | `virtio/nvrm.rs` | hard-coded |
| `ENQUEUE_RETRY_MAX_MS` | 5000 | `virtio/ctrl.rs` | ring-full retry budget before `NO_RESOURCES` |
| `MAX_NVRM_HANDLES` | 16384 (was 1024), grows from 1024 | `nvrm_tables.rs` | SANITY bound on backend handles, all owners (in-flight Opens count); `NvWinPolicy` = 0 puts 1024 back |
| `MAX_NVRM_HANDLES_PER_OWNER` | 4096 (was 128) | same | SANITY bound per process; `QUERY_CAPS.max_handles` reports the one in force |
| `MAX_NVRM_MAPS` | 8192 (was 1024), grows from 1024 | same | SANITY bound on live MMAPs, all owners (equals the adapter-wide view table, `mapping.rs`); legacy 1024 |
| `MAX_NVRM_MAPS_PER_OWNER` | 4096 (was 256) | same | `QUERY_CAPS.max_mappings`; legacy 256 |
| `MAX_MAP_BYTES` | 256 MiB | `virtio/nvrm.rs` | one MMAP (bounds the MDL's non-paged cost) |
| `MAX_NVRM_PINS` | 1024 | `nvrm_tables.rs` | live pins, all owners |
| `MAX_NVRM_PINS_PER_OWNER` | 256 | same | `QUERY_CAPS.max_pins` |
| `MAX_NVRM_PIN_PAGES` | 262143 pages (just under 1 GiB) | same | one PIN; equals the indirect table's run capacity, so a fully scattered range still fits |
| `HELIOS_NVRM_PAGE_RUNS_MAX` / `page_runs::DIRECT_MAX_RUNS` | 1024 | protocol / `kmd_logic/page_runs.rs` | runs in a direct table (8 + 1024 x 16 bytes) |
| `page_runs::INDIRECT_MAX_RUNS` | 262143 | `page_runs.rs` | runs an indirect table can hold |
| `rm_limits::EVENTS` (was `MAX_NVRM_EVENTS` 1024 / `MAX_NVRM_EVENTS_PER_OWNER` 130) | 17408 in all, 4098 per process, grows from 1024 | `kmd_logic/rm_limits.rs`, `kmd_logic/nvrm_events.rs`, `gpu/nvrm_events.rs` | registrations, all owners (0 if no event queue). DERIVED from the handle bounds: one `READY` per handle a process can have open (the registration requires the handle to be the caller's) plus its `TRANSPORT_LOST` and `SCANOUT_RELEASED`; in all the handle bound plus 1024. Grown like the handle table (13.8). 1024 / 130 fixed under `NvWinPolicy` = 0 |
| `EVENT_QUEUE_SIZE` / `EVENT_BUF_BYTES` | 16 / 256 | same | event virtqueue (kept small on purpose: the by-value queue slot sits on the boot stack under `VirtioGpu::init`, gated by `tools/kmd-frame-sizes.ps1`) |
| `OTHER_KICK_LIMIT` | 1024 | same | non-`EventReady` messages after which reposts stop kicking |
| mapping id range | `1 .. 0x7FFFFFF0` | `virtio/nvrm.rs::mint_map_id`, `kmd_logic/nvrm_views.rs` | KMD-minted from ONE counter for the life of the driver (never restarted by a new transport, never reused); the table key is `id | 0x80000000` in `AdapterContext::mappings` |
| `HELIOS_NVRM_SCANOUT_FLIP_BYTES` / `MSG_HEADER_BYTES` | 64 / 16 | protocol | |
| `DEVICE_TYPE_DRI_FIRST` | 512 | `virtio/nvrm.rs` | first DRM-node `device_type` (`512 + minor`); 255 control, GPU minors, 256 UVM, 257 UVM tools |

**Nothing assumes a 4 GiB window.** The host sizes region 1 (the RM window) to the GPU's BAR1
(`--window-mib`: 32 GiB with ReBAR, 128 GiB on a 96 GB card). Every byte count, offset and
region length in the KMD is `u64`: the shared-memory capability is read as a 64-bit
`virtio_pci_cap64` (`pci_caps.rs`), the host's `MmapResp` offset and size as two dwords each,
`MMAP`/`MUNMAP` and the per-device quota in `u64`. The two places that turn the host's
`offset` into a physical address (`nvrm_mmap`, the KMD's own client `kernel_map`) go through
`helios_kmd_logic::window_units::place`: page aligned, inside the region, and no intermediate
sum can wrap or leave the signed `PHYSICAL_ADDRESS` (a host that names a wrapping span is
refused `BAD_RANGE`). The registry counters publish MiB through `window_units::mib_u32`
(exact to 4 PiB, saturating beyond: `NvWinMb`, `NvAptMb`, `NvMapMb`). The window is never
mapped into kernel address space as one range (no `MmMapIoSpace` of 32 GiB): each `MMAP`
builds an MDL of PFNs over its own span (`blob_map::map_io_pages_to_user_prot`, 8 bytes of
non-paged pool per 4 KiB page, so 2 KiB per MiB mapped; a full 32 GiB window is 64 MiB of
MDLs, 128 GiB is 256 MiB), and nothing scans or keeps a bitmap of the window (the KMD does
not place mappings in region 1: the host's extent allocator does, `host/.../shm.rs`). The
KMD's own `window: WindowAllocator` belongs to region 3 (Venus host-visible blobs) and is a
`u64` bump allocator plus a coalescing free list of at most 1024 ranges. What the guest OS
must provide is a BAR big enough for all three regions (above-4G decoding in the VM
firmware): the KMD reads the BAR base from config space and does not depend on the resource
list. Tests: `kmd_logic/src/window_units.rs` (32, 64 and 128 GiB windows, offsets past 4 GiB,
wrapping spans).

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
- Capacity of the pin, event and client tables is reserved at init, so nothing allocates under the
  spinlock. The handle and mapping tables start at 1024 slots and GROW (section 13.8): the new
  storage is allocated at PASSIVE outside every lock before the reservation that needs it, and
  only swapped in under the lock.
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
4. On `MsgHeader.status == 0`: the handle's event registrations are released, the RM clients
   made through it are forgotten (section 12) and its pins unlocked. On a nonzero status or a transport error that never reached the host: the
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
| `NvCliRec` | RM clients recorded as a process's own (a successful forwarded `NV_ESC_RM_ALLOC` of a root class; section 12) | moves with `crm_open` |
| `NvCliDrop` | clients forgotten: a successful (or timed-out) free of the client, `Close` of the file it was made through, device destroy, the transport sweep | `NvCliRec - NvCliDrop` is the clients live now; **equal** when no client runs |
| `NvCliFull` | client allocations the table could not take (per-process 32, total 256): refused before the host in mode 1, forwarded untracked in mode 2 | **0** |
| `NvDupCli` | requests whose own client (`hRoot` / `hClient`) is not the caller's, counted in modes 1 and 2 | **0** |
| `NvDupSrc` | requests with another cross-client slot (`hClientSrc`, `hParentClient`, ...) that names a client that is not the caller's | **0** |
| `NvDupFd` | requests with a backend-handle slot (`fd`, `memFd`, `ctl_fd`, event `data`) that names a handle the caller did not open | **0** |
| `NvDupDeny` | of those, refused (mode 1; each is also in `NvRef`) | **0** outside a deliberate negative test |
| `NvDupWould` | of those, only counted because `NvDupHarden` = 2 | **0**; the field of a log-only run, read it before switching to 1 |
| `NvDupDoubt` | requests with a slot the rules could not judge with confidence (a block of an unverified size, a field cut short, an fd control the host does not translate); forwarded in every mode | small; a rise names a workload to look at (section 12.5) |
| `NvDupMode` | the `NvDupHarden` value in force (0, 1 or 2), written once the first forward read it | 2 until the default is flipped, then 1 |
| `NvWinMb`, `NvAptMb` | size in MiB of shared-memory region 1 (RM window) and 2 (UVM aperture), written at init | nonzero, or `MMAP` answers `UNSUPPORTED` |
| `NvMapMb` | bytes mapped through `MMAP` now, all owners, UVM aperture included, in MiB | follows the clients |
| `NvMapQRef` | `MMAP`s refused or failed for want of window: the policy's refusals, a view not made after the host mapped, a host refusal (until the window policy it counted only the per-device quota; under `NvWinPolicy` = 0 it still does, byte for byte). The split is `NvWinR*` (section 13.6). A mapping-table bound is NOT in it: `NvMapTRef` | **0**; nonzero says the window is under pressure |
| `NvWin*`, `NvHdl*`, `NvMapT*`, `NvTblOom`, `NvPinQRef`, `NvSanityRef` | the RM window policy, the handle and mapping tables behind the per-process bounds, pin quota refusals | section 13.6 |

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
are in `guest/windows/ci/vm/win-build.sh` (`WIN_SSH`, the guest account as `user@127.0.0.1`,
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

- **Security hardening is deferred.** Only the physical-address rule, handle/pin/map
  ownership are enforced, and (section 12) cross-client references are counted, or refused
  with `NvDupHarden` = 1. Not done: per-process resource accounting beyond the counts above,
  in-flight reference counts on handles (a handle closed by one thread while another is
  inside a `FORWARD` on it can still name a recycled number: the commit-time rechecks narrow
  but do not close this), validation of RM structures, rate limiting, auditing which RM
  classes a process may allocate.
- **Payload slots that name another client's object** are checked since `NvDupHarden`
  (section 12; counted by default, refused with `NvDupHarden` = 1): `RM_DUP_OBJECT`'s `hClientSrc`, every RM escape's own client, the cross-client
  slots of the allocations and controls the host lets an unprivileged caller reach, and the
  backend-handle slots (`0x3d05` / `0x3d06`, the NVKMS `memFd`, the `NV0005` event `data`,
  `REGISTER_FD`, the `fd` of `ALLOC_MEMORY` / `MAP_MEMORY`, the OS-event `fd`; the fence wait
  `fd` is only counted). What section 12 does NOT cover (UVM, NVKMS on the modeset file, controls
  outside its table, the grant side of `RM_SHARE`) is listed in section 12.6. The cross-process
  route the KMD does offer is `RM_RESOURCE_IMPORT` (`shared-foreign-surfaces.md` section 6),
  which never needs another process's handle.
- **Adoption of a KMD-created resource has no same-device check** (`ctx_id != 0` in place of the
  creating device's context; `shared-foreign-surfaces.md` R5). Level 4 of the KMD's own RM client
  (`kmd-rm-client.md` section 14) is the only creator of such a resource.
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
  at runtime by `rm_import_served` in `virtio/foreign.rs` (host config feature bits 13 + 10;
  `CAP_RM_IMPORT` follows it) and answers `ST_UNSUPPORTED` before touching state when the
  host does not serve it. The NVK/UMD half is still to do. See `zero-copy-present.md`.
- `HELIOS_NVRM_ST_RESP_TRUNCATED` is never produced; an undersized `resp_cap` is not detected.
- `ScanoutFlip` forwarding is the only path that accepts a non-RM message; `scanout != 0` is
  refused (single scanout).
- Host-side indeterminate cases: a timed-out `Open` or `SEMSURF_FENCE_CREATE` can leave an
  untracked host handle (a fence ends by itself within the host driver's 5 s timeout but its
  handle stays until the host side is reset); a timed-out `Close` is treated as closed.
- Fence handles count against the per-process handle bound (4096 now, 128 before the tables grew,
  and again under `NvWinPolicy` = 0) with every file. A client that keeps hundreds of fences in
  flight must close fired ones promptly (section 4.6); the host's own cap is 4096 unsignalled
  fences.

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


## 12. Cross-client hardening (`NvDupHarden`)

Written against the code of this branch (`kmd_logic/src/nvrm_clients.rs`,
`kmd_render/src/virtio/nvrm_harden.rs`, the call sites in `virtio/nvrm.rs`). It was verified
with host tests of the pure rules and a type-check of the KMD sources; it has **not** been run
in a Windows guest (section 12.7).

### 12.1 Threat model

* **Actors.** Two guest processes A and B, each an owner (a D3DKMT device), both using
  `FORWARD`. The host backend serves the whole VM from ONE process, so every RM client and every
  backend file handle in it is "the guest's": RM does not tell A's client from B's, and the
  backend's handle table is one table. `RM_DUP_OBJECT` across guest clients worked through
  `FORWARD` with no check (shown on v311 with `crm_share_smoke`).
* **What A controls.** Every byte of its escape buffer, lengths included. It can guess numbers:
  backend handles are small integers, RM client handles are RM-chosen but sequential and
  unpublished.
* **What A wants.** To act in, read, dup, map or be signalled by something of B: run RM controls
  in B's client; `RM_DUP_OBJECT` B's memory into its own client (`hClientSrc` / `hObjectSrc`);
  allocate an event on B's objects (`NV0005.hParentClient`); import an object from a backend
  file of B's (`0x3d06`, the NVKMS `memFd`); use B's file as its own event or mapping channel
  (`REGISTER_FD`, `ALLOC_MEMORY` / `MAP_MEMORY` `fd`).
* **Out of scope.** A compromised KMD or host; GPU-level isolation between channels; resource
  exhaustion beyond the existing quotas; UVM (section 12.6).
* **The rule.** A forwarded `Ioctl` may only name RM clients and backend handles its own owner
  was given. The one sanctioned cross-process route is the KMD-mediated foreign resource
  (`RM_RESOURCE_IMPORT`), which is another verb and needs no foreign handle: the importer
  exports the GEM handle of ITS OWN DRM file to a control descriptor of its own client. The KMD's
  own RM client (`DeviceOwner::KMD_RM`, `KmdRmClient`) goes through `forward` too and is
  exempt: no escape can present that owner, and its traffic is the KMD's.

### 12.2 Mechanism

* **Who owns which client.** `kmd_logic::nvrm_clients::ClientTable`, a field of the transport
  (so a new transport starts empty): `(owner, hClient, via)`, at most 32 per owner and 256
  total, boxed. It is learned from replies, not from requests: a forwarded `NV_ESC_RM_ALLOC` of
  class `NV01_ROOT` / `NV01_ROOT_NON_PRIV` / `NV01_ROOT_CLIENT` (the host's `ROOT_CLASSES`) in an
  NVOS21 (32-byte) or NVOS64 (48-byte) block, whose reply has host status 0, RM status 0 (word
  28 or 40) and a `hObjectNew` (word 8) that is neither 0 nor `0xFFFFFFFF`, records that client
  for the owner, with the backend file the request went through as `via`.
* **Reservation.** As an `Open` does for handles, a client allocation reserves its table slot
  BEFORE it is forwarded (the reservation is a slot of the table that counts against the table
  AND the owner's quota, so two concurrent allocations one below the quota cannot both reserve): a full table or quota refuses (`NO_RESOURCES`, `NvCliFull`) before the
  host makes a client nobody tracks. A failed or refused allocation gives the slot back.
* **Forgetting.** A successful `NV_ESC_RM_FREE` of the client itself (16-byte NVOS00,
  `hObjectOld == hRoot`, host and RM status 0), or one that TIMED OUT (indeterminate: the entry
  goes, the safe direction); `Close` of the `via` file (success or timeout, not a failed
  `Close`, which restores the handle); `close_all_for_owner`; the transport sweep
  (`close_all_on_host`); and the table dies with the transport. A client number RM mints again
  evicts a stale entry of ANOTHER owner, so one number never has two owners.
* **Judging a request.** `judge` runs in the SAME lock hold that already resolves the handle's
  `device_type`, so the hot path pays no extra lock. It parses `MsgHeader | IoctlReq | data |
  nested` with checked arithmetic (a request that does not parse is `Allow` here, because
  `check_ioctl` refuses it right after), then looks at every slot that names something (12.4).
  UVM files (`device_type` 256 / 257) are not interpreted.
* **Order in `forward`.** handle ownership (`NOT_OWNED`) -> fence handle (`FORBIDDEN`) ->
  `check_ioctl` (`BAD_RANGE`, `FORBIDDEN`) -> the hardening verdict -> client reservation ->
  the round trip -> record / forget from the reply.
* **Verdicts.** `Allow` (every reference is the caller's, or there is none), `Deny(cause)`,
  `Doubt(cause)`. A slot is `Deny`-grade only when the field is known AND the parameter block has
  exactly the size the layout was verified against (or, for a prefix field, any size that holds
  it). A block of another size, a block too short for the slot, and a descriptor control the host
  does not translate are `Doubt`: counted (`NvDupDoubt`), never refused, in every mode. A refusal
  answers `NOT_OWNED` (one code for "not yours" and "does not exist", checklist item 4 of section
  11), counted in `NvRef` as every refusal.
* **Zero and negative.** Client 0 is "none" (RM refuses it itself) and is never checked; a
  backend-handle slot is read as the `i32` the host reads, and a value `<= 0` names no file.

### 12.3 The knob and the counters

`NvDupHarden` (REG_DWORD under the service key, read once per boot): **2** (the default of the
first shipped package) log-only: everything is recorded and judged, what mode 1 would refuse is
counted in `NvDupWould` and forwarded; **1** enforce (a refusal is `NOT_OWNED`, counted in
`NvDupDeny` and `NvRef`); **0** off (nothing is recorded or judged: the behaviour before this
change). A value that is present and is not 0 or 2 enforces, so a typo never turns the checks
off. The counters are in section 7 (`NvCli*`, `NvDup*`; all at most 10 characters). `NvDupMode`
shows what was read.

To set it, in an elevated prompt on the guest, then restart the adapter (or reboot):

```
reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v NvDupHarden /t REG_DWORD /d 1 /f
pnputil /restart-device "<the Helios display adapter's instance id>"
reg query HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v NvDupMode
```

(`NvDupMode` appears once the first forward after the restart read the knob.)

**Flip the default to 1** (change `MODE_LOG` to `MODE_ENFORCE` in `virtio/nvrm_harden.rs`
`read_mode`, and the knob comment in `diag.rs`) after a real NVK run, with the default as
shipped, shows: `NvCliRec` close to `NvOpen` (one client per `crm_open`; a gap means replies the
table could not read), `NvDupWould` 0 (nothing the rules would refuse was sent), and `NvDupDoubt`
small and explained. Read the counters after a few extra escapes (section 7, publication lag).

### 12.4 What is covered

Offsets are bytes into the named block (`data` = the `NVOSxx` struct, `nested` = the block its
pointer names). "Grade" is Deny when the slot's block has the stated size (the sizes are the
host allow-list's, equal in all six releases it carries unless noted), else Doubt.

| request | slot | names | grade |
|---|---|---|---|
| every RM escape in `CLIENT_AT_0` (`0x27 0x28 0x29 0x2A 0x2B 0x32 0x33 0x34 0x35 0x37 0x38 0x39 0x41 0x4A 0x4D 0x4E 0x4F 0x56 0x57 0x58 0x59 0x5E`) | `data` @0 (`hRoot` / `hClient`) | the caller's client | Deny; client 0 passes |
| `NV_ESC_RM_ALLOC` of a root class | none (RM picks the number) | | the request is recognised and its reply recorded |
| `NV_ESC_RM_DUP_OBJECT` (NVOS55, 28 bytes) | `data` @12 `hClientSrc` (`hObjectSrc` @16 follows from it) | a client | Deny at 28 bytes |
| `RM_ALLOC` class `0x79` (`NV0005`, 24 bytes) | `nested` @0 `hParentClient`; @16 `data` (low word, the event file) | a client; a backend handle | Deny at 24; Deny at 24 |
| `RM_ALLOC` class `0x05` (`NV0005`, 24 bytes) | `nested` @0 `hParentClient`; @16 `data` | a client; a number that may be a cookie | Deny at 24; Doubt always (12.5) |
| `RM_ALLOC` class `0x80` (`NV0080`, 56) | `nested` @4 `hClientShare`, @8 `hTargetClient` | clients | Deny at 56 |
| `RM_ALLOC` class `0x83DE` (debugger, 12) | `nested` @4 `hAppClient` | a client | Deny at 12 |
| `RM_ALLOC` class `0xB2CC` (profiler, 8) | `nested` @0 `hClientTarget` | a client | Deny at 8 |
| `RM_CONTROL` `0x3D05` `EXPORT_OBJECT_TO_FD` (24) | `nested` @16 `fd` | a backend handle | Deny at 24 |
| `RM_CONTROL` `0x3D06` `IMPORT_OBJECT_FROM_FD` (20) | `nested` @0 `fd` | a backend handle | Deny at 20 |
| `RM_CONTROL` `0x3D08 0x3D0A 0x3D0B 0x3D0C` | `nested` @0 / 72 / 0 / 0 `fd` | a descriptor the host does not translate | Doubt always |
| `RM_CONTROL` `0x00000D03` `CLIENT_GET_ACCESS_RIGHTS` (12) | `nested` @4 `hClient` | a client | Deny at 12 |
| `RM_CONTROL` `0x20802502` `DMA_INVALIDATE_TLB` (16) | @0 `hClient` | a client | Deny at 16 |
| `RM_CONTROL` `0x2080110B` `FIFO_DISABLE_CHANNELS` (536) | @4 `numChannels` (capped at 64), @24 `hClientList[]` | clients | Deny at 536 |
| `RM_CONTROL` `0x208F0403` `FIFO_GET_CHANNEL_STATE` (16) | @4 `hClient` | a client | Deny at 16 |
| `RM_CONTROL` `0x503C0106` `REGISTER_PID` (4) | @0 `hClient` | a client | Deny at 4 |
| `RM_CONTROL` `0xA0840105` `BIND_FECS_EVTBUF` (16) | @0 `hEventBufferClient` | a client | Deny at 16 |
| `RM_CONTROL` `0x20800122` `GPU_EXEC_REG_OPS` (48) | @0 `hClientTarget` (0 = all) | a client | Deny at 48 |
| `RM_CONTROL` `0x20801209` `GR_CTXSW_PM_BIND` (40), `0x20801208` `ZCULL_BIND` (24) | @0 `hClient` | a client | Deny at the size |
| `RM_CONTROL` `0x20801211` `GR_CTXSW_PREEMPTION_BIND` (104 or 112 by release) | @4 `hClient` | a client | Deny at any size that holds it |
| `RM_CONTROL` `0x20801205` `GR_CTXSW_ZCULL_MODE` (16) | @4 `hShareClient` | a client | Deny at 16 |
| `NV_ESC_RM_ALLOC_MEMORY` `0x27`, `NV_ESC_RM_MAP_MEMORY` `0x4E` (56 with the trailing fd) | `data` @48 `fd` | a backend handle | Deny at 56 |
| `NV_ESC_REGISTER_FD` `0xC9` (4) | `data` @0 `ctl_fd` | a backend handle | Deny at 4 |
| `NV_ESC_ALLOC_OS_EVENT` `0xCE` / `NV_ESC_FREE_OS_EVENT` `0xCF` (16) | `data` @0 `hClient`; @8 `fd` | the caller's client; a backend handle | Deny; Deny at 16 |
| nvidia-drm `GEM_IMPORT_NVKMS_MEMORY` `0x41`, `GEM_EXPORT_NVKMS_MEMORY` `0x49`, `GEM_EXPORT_DMABUF_MEMORY` `0x4D` | `nested` @0 `memFd` | a backend handle | Deny; a missing block is a Doubt |
| nvidia-drm `SEMSURF_FENCE_CTX_CREATE` `0x54` | `nested` @0 `hClient` | a client | Deny |
| nvidia-drm `SEMSURF_FENCE_WAIT` `0x56` (24) | `data` @4 `fd` (a fence handle; 0 = already signalled) | a backend handle | Doubt always (12.5) |

The ioctl type byte decides what the number means: `'F'` (0x46) for the RM escapes, `'d'` (0x64)
for the nvidia-drm ones. Sources of the layouts: the NVIDIA headers (`nvos.h`, `class/cl0005.h`,
`cl0080.h`, `cl83de.h`, `clb2cc.h`, `ctrl/...`) compiled with `offsetof` against the host's
per-release allow-lists (`host/backend/gen/src/rmallow`), and the host's own parsers
(`nested.rs`, `rm_fd.rs`, `fence.rs`) for the descriptor slots.

### 12.5 Single-client traffic and the doubts

A caller that only names its own clients and files is `Allow` by construction. The librmclient
flows were read for this change (`crm_open`: the root allocation on the control file and
`REGISTER_FD` of that file on each GPU channel; `crm_alloc`; `crm_map_memory`: `MAP_MEMORY` with
the per-mapping channel as `fd`; `crm_event_open`: `ALLOC_OS_EVENT` on the event file and the
`NV0005` event with that file as `data`), and `a_single_client_session_is_untouched` walks a
session of that shape through `judge`. The `0x3D05` / `0x3D06` export / import, the NVKMS import
and the fence wait are read from the host's parsers and `shared-foreign-surfaces.md` section 6,
not from a running NVK. Mode 0 restores the old behaviour exactly. Where the
rules are not sure they count instead of refusing:

* **Size drift.** Every Deny needs the verified block size. A release whose struct grew gets a
  `NvDupDoubt` and is forwarded as before until the table is updated (the preemption bind, 104 /
  112, is the one entry whose size differs between the six releases, and asserts none).
* **`NV0005` class `0x05` versus `0x79`.** The host turns `data`'s low word into a descriptor for
  both, but only `NV01_EVENT_OS_EVENT` (`0x79`) is known to take a descriptor there. For class
  `0x05` a `data` that is not a handle of the caller's may be a cookie, so it is counted, not
  refused; the parent client is checked for both.
* **`SEMSURF_FENCE_WAIT`'s `fd`.** Counted, never refused: a process may wait on a fence it did not
  create (one the KMD took over for a present moves to the `KMD_RM` owner, and a fence another
  process shares is a plausible design), and nothing read for this change says that cannot be
  legitimate. Its creator's own waits are `Allow`. If waits on foreign fences turn out never to
  occur (`NvDupDoubt` stays 0 under a real workload), make it a Deny.
* **Descriptor controls the host does not translate** (`0x3D08 0x3D0A 0x3D0B 0x3D0C`): the
  number reaches RM as a descriptor of the backend process, which is not a handle of this table,
  so they are counted, not judged. This is a HOST finding (the values name whichever backend file
  sits at that number); the fix belongs in `host/backend/device/src/nvidia/nested.rs`.
* **Two layout assumptions** read from the host and the current headers, not from a running RM:
  the NVOS64 / NVOS21 sizes and status offsets (48 / 40 and 32 / 28: the host's
  `nvidia/ioctl.rs` `note_clients` and the current `nvos.h` agree; the older SDK snapshot under
  an older SDK copy has shorter structs and was not used for them) and `IoctlResp` carrying the data
  block at reply offset 28. A reply that does not fit them records nothing (the client is then untracked,
  and in mode 1 its later calls are refused: watch `NvCliRec` against `NvOpen` in a first run).

### 12.6 What is NOT covered

* **UVM** (`device_type` 256 / 257). Its calls carry `rmCtrlFd` (a backend handle) and `hClient`
  side by side (`UVM_REGISTER_GPU`, `UVM_MAP_EXTERNAL_ALLOCATION`, `UVM_REGISTER_CHANNEL`, ...). The
  host checks that the client was made on that control file, but accepts any of the VM's files,
  so a UVM caller could name another process's control file and client. The per-release call
  tables are the host's (`abi::uvm`); the KMD has none. NVK does not use UVM.
* **NVKMS on the modeset file** (type `'m'`) and the other `'d'` ioctls not in 12.4: not parsed.
* **`RM_CONTROL` commands outside the table.** The table is the set of allow-listed controls
  (`v615_71_09`: 756) whose parameter structs, in the NVIDIA headers, hold a client handle; the
  controls with a client handle that the host does not allow an unprivileged caller are refused
  there (`GPU_EVICT_CTX`, `GPU_INITIALIZE_CTX`, `GPU_PROMOTE_CTX`, `GR_SET_ZCULL_BIT_WAR`,
  `FIFO_UPDATE_CHANNEL_INFO`, ...). Still open: controls that embed another struct with a client
  handle, notably `NV5080_CTRL_CMD_DEFERRED_API` / `_V2` (the `api_bundle` union holds the GR
  context-switch binds above), and any control a later release adds. Re-run the extraction
  (12.8) when the host's allow-list changes.
* **`NV_ESC_RM_SHARE`** (grant side): its `sharePolicy.target` is not checked. The consuming
  side (dup, event, control in the granted client) is covered, so a grant to a client the
  grantee cannot use is harmless.
* **Objects.** Object handles are never checked on their own; they live inside a client and RM
  resolves them there, so checking the client is the check. A client of the caller's own with
  an object handle the caller did not create is the caller's business.
* **A timed-out client allocation** leaves a client on the host that this table does not know (as
  a timed-out `Open` leaves a file): in mode 1 the process cannot use it, and the host frees it
  with its file. Bounded by the file quota.
* **A free that fails after the host acted** without a timeout (a malformed reply) leaves the
  entry; the next client RM mints under that number evicts it.
* **Races.** A request judged just before a concurrent free of its client, or a `Close` of its
  file, is forwarded (the host then answers for it). The tables are not reference counted, as the
  handle tables are not (section 10.1).
* **Pinned registrations.** A client allocation sent with a `pin_id` is forwarded by
  `forward_pinned`, which does not record its reply (only `ALLOC_MEMORY` registrations are
  meant to carry pins): in mode 1 such a client cannot be used by its creator, which only
  harms the creator.
* **The owner is the D3DKMT device**, as everywhere in this file: two devices of one process are
  strangers to each other; one device shared by two processes is not possible.

### 12.7 Verification

* `cargo test` in `guest/windows/kmd_logic` (`nvrm_clients::tests`, 47 tests): the table (two
  owners with the same number, reuse after a free, quotas, reservations, retire, `Close` of a
  file, `clear`), parsing and replies with every truncation and `u32::MAX` lengths, one test per
  covered slot (own / foreign / zero / negative / wrong size / too short), UVM and other
  namespaces left alone, a seeded 20 000-request random run for panics, a whole two-process
  session, and a single-client session that must stay `Allow`.
* The KMD sources were type-checked as a whole crate against a `wdk-sys` stub (the real target
  does not build here), comparing the errors of the touched files before and after: no new ones.
  `kmd_render` itself was **not** compiled for Windows and nothing was run in a guest.
* **UNVERIFIED:** that NVK on a live backend behaves as the headers say for the slots above
  (`NvDupWould` / `NvDupDoubt` in a log-only run are the check); that the reply layout of
  `RM_ALLOC` is as read from the host (`NvCliRec` must follow `NvOpen`); that `crm_share_smoke`'s
  cross-client dup is now seen (`NvDupSrc` and `NvDupWould` rise by one per attempt in the default
  mode; `NvDupDeny` in mode 1).

### 12.8 Adding or changing a slot

1. Find the struct in the NVIDIA headers and the command in the host allow-list
   (`host/backend/gen/src/rmallow/<release>.rs`); compile an `offsetof` probe against
   `sdk/nvidia/inc` for the offset and the size.
2. Add a `Layout` to `CLASS_LAYOUTS` (alloc classes) or `CONTROL_LAYOUTS` (controls) in
   `kmd_logic/src/nvrm_clients.rs`, or a match arm in `rm_escape` / `drm_ioctl` for an escape. Use
   `untranslated` when the host does not turn the number into a descriptor, a size of 0 only for a
   prefix field.
3. Add a test with an own, a foreign, a zero and a wrong-size value; run it first as a Doubt in
   `NvDupHarden` = 2 on a real workload if the layout was not read from a header.

## 13. The RM window policy and the table limits

Pure rules: `kmd_logic/src/rm_window.rs` (who may map how many bytes of region 1),
`rm_limits.rs` (when a table grows, where it refuses), `window_units.rs` (64-bit placement and
MiB conversion). Driver side: `virtio/nvrm_window.rs` (counters, privilege, publishing),
the doors in `virtio/gpu/nvrm_tables.rs`, `virtio/nvrm.rs::host_mmap`, `ddi/escape.rs::nvrm_mmap`.
Host tests: `cargo test` in `guest/windows/kmd_logic`.

### 13.1 What is accounted

NVK maps RM memory with `MMAP` into the host's RM window (shared-memory region 1, sized by the
backend's `--window-mib`, read by the KMD as `NvWinMb`: the GPU's BAR1, 32 GiB with ReBAR, 128 GiB
on a 96 GB card). The HOST places each mapping there (its own extent allocator, split into caching
zones: `host/backend/device/src/shm.rs`) and the reply names where; the KMD never chooses an
address in region 1 and holds no free list or bitmap of it. So what the KMD can count is BYTES: the
sum of the sizes of the live non-UVM mappings (the UVM aperture, region 2, is exempt as before). That
is an approximation of the real occupancy: the host also holds extents for mappings armed by
`RM_MAP_MEMORY` and not yet `MMAP`ed, and its zones can run out (write-combined is most of it) while
the byte total still has room. A host refusal is therefore its own counted reason (`NvWinRHost`,
last errno `NvWinHErrno`: 12 is ENOMEM, the host's zone is full). The KMD cannot report a "largest
free extent": `NvWinFreeMb` is `cap - in use`, an upper bound.

### 13.2 The policy (`NvWinPolicy` = 1, the default)

* No fixed per-process share. Any device may map until the window is full.
* A reserve (`NvWinReserveMb`, default 256 MiB) is kept for the privileged device: a map by anybody
  else is refused once it would take the in-use total past `cap - reserve`; the privileged device
  may use everything up to `cap`. The reserve is clamped to a quarter of the cap, so a small
  window (a 256 MiB BAR1 without ReBAR keeps 64 MiB) or an `NvWinMaxMb` at or below the reserve
  still gives an ordinary device at least three quarters: never less than the legacy window/4
  (`small_windows_table`).
* `cap` is the window size, or `NvWinMaxMb` when that is set and smaller: an operator bound on the
  non-paged pool (each mapping costs an MDL of 2 KiB per MiB: 64 MiB of pool for a full 32 GiB, 256
  MiB for 128 GiB; `NvWinMaxMb` = 16384 would hold it to 32 MiB).
* One map is never larger than `MAX_MAP_BYTES` (256 MiB, the MDL's contiguous non-paged allocation
  and the `ULONG` length of `IoAllocateMdl`); a map larger than the window could ever give (an empty
  window would refuse it too) is `NvWinRBig`.
* Refusal: `NO_RESOURCES`, what the quota answered, so NVK's patch falls back to system memory
  with no UMD change. Counted by reason (13.6) and in `NvMapQRef`.
* `NvWinPolicy` = 0 is the old rule byte for byte: a quarter of the window per device (8 GiB of 32),
  nothing else, no reserve, and the tables back at their old fixed sizes (1024 handles, 128 per
  process, 1024 maps, 256 per process). The pure function equals the old one over a grid of
  windows and sizes (`legacy_equals_the_old_function`).

All of it is `u64` bytes; the knob products are `u64` (any `u32` MiB is valid, 4 PiB at most).

### 13.3 The privileged device (the reserve is its)

Nothing in the KMD identified DWM before (no image name, no pid). The choice, least fragile first:

1. **The holder of the foreign scanout source** (`SCANOUT_SET`): DWM-on-NVK is the process that sets
   scanout 0 from an NVK-allocated image. Decided at map time by `foreign_scanout_owner_is` (the
   leaf `STATE` lock, read BEFORE the virtio lock is taken) and made sticky: a successful
   `SCANOUT_SET` marks the device privileged in the account until the device is destroyed
   (`close_all_for_owner`), so DWM keeps the reserve between sources (a resolution change, a
   lapse), exactly when it re-creates its swap chain.
2. **The KMD's own RM client** (`DeviceOwner::KMD_RM`), always.

**A restarted DWM** is a new device, ordinary until its first `SCANOUT_SET`, and its first maps
(swap chain, glyph caches) come before that. While others hold the window up to `cap - reserve` it
would be refused: the shell would fall back to system memory for its first buffers. Mitigation, kept
simple (`rm_window::PRIVILEGE_GRACE_100NS`, test `a_restarted_dwm_gets_the_reserve_for_its_first_maps`):
when a device marked privileged is destroyed, the account opens a 30 s grace. A device that appears
during it (no row yet) may use the reserve, and the row it makes keeps that right until the grace
ends (`NvWinGrace` counts them); devices that already held maps stay ordinary, and the right ends
with the 30 s or with the device's own `SCANOUT_SET` (the sticky mark). Cost: any new process
that starts in those 30 s can use the reserve too, which only matters if the window is nearly full,
and the reserve is only 256 MiB. Not chosen: keying on a MISC_PRIMARY/foreign primary allocation
(the KMD sees it at `CreateAllocation`, in another lock domain and before any NVRM device exists)
or on the last privileged process image (no image name is recorded anywhere). If DWM restarts
slower than 30 s after the old one died, its first maps are ordinary until its `SCANOUT_SET`;
`NvWinPriv` 0 with a running desktop says it.

Not used, on purpose: an image-name list (`PsGetProcessImageFileName` for `dwm.exe`) needs an
export the Rust bindings do not carry and a C shim that cannot be built here, and is a name a
renamed binary spoofs; no image name or pid is recorded anywhere in the KMD today. Cost: a device that never sets a scanout
source (a DWM that has not yet drawn) maps as an ordinary device, which only matters while the
window is nearly full. Risk: any process can call `SCANOUT_SET` and claim the reserve (a hostile
process already can take the whole window below the reserve; the reserve only keeps the shell
alive, it is not a security boundary, see the trust notes in section 10.1). The
reserve in use is `NvWinRsvUse`; `NvWinPriv` is the number of privileged devices.

### 13.4 Reclaim (designed, not implemented)

Reclaiming an idle mapping is not safe from the KMD alone: the process holds a live user address
into it, `MmUnmapLockedPages` is only legal in the owner's context, and an access after it faults
the process instead of failing a call. No mapping is provably idle. The safe design is cooperative:
the read-only `WINDOW_INFO` report (section 4.7) tells NVK how much room is left, and a future
"release hint" event (kind 3 or a new one) asks the UMD to `MUNMAP` its least recently used
persistent maps (NVK knows which are idle; its patch falls back to system memory for the next one).
The KMD would count hints sent and honoured. Nothing evicts today; the refusal is the pressure
valve.

### 13.5 Scale: 32 to 128 GiB

| item | state |
|---|---|
| byte counts, offsets, region lengths | `u64` everywhere (13.1, section 5 "Nothing assumes a 4 GiB window") |
| `NvWinMb` parse and publish | `virtio_pci_cap64` length, `mib_u32` (exact to 4 PiB, saturating) |
| mapping the window | never as one range. Per map: an MDL of PFNs (`IoAllocateMdl`, 8 bytes per page) mapped into the caller. `MmMapIoSpace` is only the KMD's own client, one surface at a time |
| non-paged pool | MDLs: 2 KiB per MiB mapped (bound with `NvWinMaxMb`); tables: handle slot ~48 bytes, map slot ~40, so 16384 handles is under 1 MiB |
| per-map work | `O(table)` scans under the lock (the old gauge refold per change is gone; the policy is `O(owners)`): at the 8192/16384 bounds about 10 to 30 microseconds worst case. An index by owner is the next step if the bounds are ever raised |
| DPC per `EventReady` | ONE scan of the handle table under the virtio lock (`nvrm_handle_index`, shared by the fence-fired mark and the latch); it used to be two per event, up to 16 events per drain. The events registry scan (`signal_handle`) is over at most 1024 registrations. Still `O(table)` and not indexed: at the 16384 bound a scan is tens of microseconds, so an index by handle is the next step if tables are ever near their bound. Also `O(table)` at PASSIVE: `fence_claim_index` counts the attached fences (a counter kept at the attach/close sites would make it `O(1)`), and `nvrm_judge` / `nvrm_handle_device_type` per forwarded ioctl |
| BAR | the guest must give the device a BAR for all three regions (above-4G decoding); the KMD reads the base from config space |
| host zones | the host splits the window by caching type; a WC-only workload fills its zone before the byte total (13.1) |
| `VIDMM_VRAM_MAX_MB` (64 GiB) | region 3 (Venus), not region 1; a Venus window above 64 GiB reads `VidVBad` and disables the VidMm override |

### 13.6 Counters and knobs

Knobs (service key, REG_DWORD, read once per transport): `NvWinPolicy` (default 1; 0 = legacy),
`NvWinReserveMb` (default 256), `NvWinMaxMb` (default 0 = the window). Names are at most 14
characters, unique across `kmd_render` and `kmd_logic` (`rm_window::counter_names_fit_...` scans
both trees).

| counter | meaning |
|---|---|
| `NvWinMb` | the window, MiB (existing) |
| `NvWinPol` / `NvWinCapMb` / `NvWinResMb` | policy in force; effective cap; reserve (MiB) |
| `NvWinUseMb` / `NvWinPeakMb` / `NvWinFreeMb` | window bytes mapped now (non-UVM); high-water mark since driver load; `cap - use` |
| `NvWinRsvUse` | MiB in use inside the reserve (only the privileged device gets there) |
| `NvWinMaps` / `NvWinOwn` / `NvWinPriv` | live window mappings; devices with a row; devices marked privileged |
| `NvWinGrace` | devices let use the reserve by the privilege grace (13.3, a restarted DWM or any new device within 30 s of the shell's loss) since the transport started |
| `NvWinRFull` | refused: the window (up to `cap`) has no room |
| `NvWinRRes` | refused: a non-privileged map would eat into the reserve |
| `NvWinRBig` | refused: one map larger than the window could ever give |
| `NvWinRTab` | refused: bookkeeping full (no owner row, or the adapter-wide view table) |
| `NvWinRAddr` | the view could not be made after the host mapped (MDL, user address space) |
| `NvWinRHost` / `NvWinHErrno` | the host refused the `Mmap`; the last errno (12 = its zone is full) |
| `NvWinT1Pid`, `NvWinT1Mb` ... `NvWinT4Pid`, `NvWinT4Mb` | the four owners mapping most (process id of the first map, MiB); 0 = unused rank. Recomputed under the lock at every change in `O(owners)`, published at PASSIVE only |
| `NvMapMb` | all `MMAP` bytes now, UVM included (existing) |
| `NvMapQRef` | the window refusals and failures above (`NvWinRFull`, `NvWinRRes`, `NvWinRBig`, `NvWinRTab`, `NvWinRAddr`, `NvWinRHost`), NOT the mapping-table bounds (`NvMapTRef`). Under `NvWinPolicy` = 0 only the per-device quota refusals, as before: the new reasons are then visible in `NvWinR*` alone |
| `NvHdlLive` / `NvHdlPeak` / `NvHdlCap` / `NvHdlGrow` | live handles (reservations included), high-water mark, table slots, growths |
| `NvHdlORef` / `NvHdlGRef` / `NvHdlFRef` | handle reservations refused: per-process bound, whole-table bound, fairness while scarce |
| `NvMapTCap` / `NvMapTGrow` / `NvMapTRef` | mapping table slots, growths, refusals by its bounds |
| `NvRestLost` | a handle still open on the host that could not be put back after a failed `Close` (nothing was left in the storage kept for restores): untracked until the sweep. Should read 0 |
| `NvTblOom` | a table wanted to grow and the allocator refused, or a reservation found no storage because growth lagged (only a hostile burst gets there) |
| `NvPinQRef` | `PIN`s refused by the per-process pin quota (before this counter nothing in the registry showed it) |
| `NvWinInfo` | `WINDOW_INFO` calls answered (section 4.7) |
| `NvSanityRef` | every refusal by a sanity bound: `NvHdl*Ref` plus `NvMapTRef` |

### 13.7 Reading a submission failure: which counters show KMD-side exhaustion

The FFXIV run: an NVK queue submit failed after about 56,700 frames (`present_frame_gate: command
stream/submission failed`, hr 0x80004005, no host Xid). A refusal anywhere below reaches the UMD as
`NO_RESOURCES` / `STATUS_INSUFFICIENT_RESOURCES` and from there as a generic failure. Every path:

| path | limit | counter TODAY (before this change) | counter now |
|---|---|---|---|
| window byte quota | window/4 per device | `NvMapQRef` (and `NvMapErr`, +1 per refused `MMAP`) | `NvMapQRef`, split `NvWinRFull`/`NvWinRRes`/`NvWinRBig` |
| per-process mapping count | 256 | `NvMapErr` only (no counter of its own) | `NvMapTRef`, `NvSanityRef` (bound 4096) |
| host refused the map (its window or zone full) | host | `NvMapErr` | `NvWinRHost`, `NvWinHErrno` |
| view not made (MDL, address space) | OS | `NvMapErr`; QUERY_STATS `MAP_PAGES_FAILS` | `NvWinRAddr` |
| adapter-wide view table | 8192 | QUERY_STATS `MAPPING_FULL_REJECTS` | `NvWinRTab` as well |
| per-process handle quota (`Open`, fence create) | 128 | `NvRef` only (shared with every policy refusal) | `NvHdlORef` / `NvHdlGRef` / `NvHdlFRef`; `NvHdlLive` shows the level |
| client table (`NvDupHarden`) | 32 per process, 256 | `NvCliFull` | unchanged (not yet dynamic) |
| pin quota | 256 per process, 1024 | nothing (`NvPinErr` counts what failed after the lock) | `NvPinQRef`; `NvPinErr` as before |
| pin leaks | n/a | `NvPinLeak`, `NvPin - NvUnpin` | unchanged |
| event registrations | 130 per process, 1024 | `NvEvRef` | `NvEvRef` (bound now 4098 per process, derived from the handle bound) |
| fence early table | 16 | `NvFenceErr` | unchanged |
| KMD-held (attached) fences | 512 | `RmGRef` (marker refused), `FsFRef` | unchanged |
| RM gates / points | 8 gates, 128 points | `RmGRef` | unchanged |
| scanout fenced queue | 8 | `FsFFull` | unchanged |
| producer completion | 8192 pending, 16384 writers, 64 marks | `PrdFull`, `PrdWrFull`, `PrdMarkFull` (`PrdPend`, `PrdHi` level) | unchanged |
| flush gate table | see `flush-gate.md` | `FlGTblFull` | unchanged |
| control ring full past 5 s | virtqueue | `QfRet` (retries) | unchanged |
| windowed-blt tokens | 64 | loud Present refusal (`PrBndDrop` is a different thing) | unchanged |
| scanout allocation slots | 32 | `ScAlcFul` | unchanged |

**Can a per-frame resource leak and hit a fixed limit in about 56,700 frames?** The leak rate each
table implies, if one entry is lost every N frames: 128 handles (fence handles count against it)
N = 443; 512 attached fences N = 111; 1024 pins, events or handles in the old global table N = 55;
8192 producer entries N = 6.9; 64 windowed-blt tokens N = 886. Where the KMD can lose one:

* **A fence handle** is created per present (one `SEMSURF_FENCE_CREATE`). The KMD closes it when
  the carrier is accepted and the fence fires (`NvFenceCl` counts it). It stays the CALLER's when a
  carrier whose status the UMD sees is refused (the scanout `PRESENT` with a fence, `HE12`): the
  UMD must then `Close` it. A single error path in NVK that forgets that is a leak of one handle
  per occurrence, and at 128 every later create is refused with `NvRef` rising. Read
  `NvFence - NvFenceCl` (live fences; with no leak it stays near the frames in flight), `NvOpen -
  NvClose`, and `NvHdlLive` / `NvHdlPeak`: a value climbing steadily with the frame count and
  stopping at 128 (legacy) or 4096 is this leak; from this change the 128 wall is gone, and a leak
  would show as `NvHdlLive` rising instead of a failed submit.
* **A KMD-held fence** that never fires (the host's `EventReady` lost, the event queue has 16
  buffers): `RmGAtt - RmGFire - RmGCan` grows and `NvFence - NvFenceCl` with it; at 512 the carrier is
  refused (`RmGRef`) and presents fall back to the legacy wait. `NvEvErr`, `NvEvDrop`, `NvFenceErr`
  say whether notifications are being lost.
* **A producer entry** stranded behind an older one that never completes (`PrdPend` rising with
  frames, `PrdHi` at 8192, `PrdFull` > 0, the UMD log "producer: allocation epoch publication
  failed"). The completion rule fix (`producer-completion.md`) closes the known cases; 56,700 / 8192
  is 6.9 frames per entry, so this is the one that needs MOST of the frames to leak and the first to
  check if `PrdPend` is high.
* **Event registrations** (`EVENT_REGISTER` per fence wait): `NvEvReg - NvEvUnreg` is only an order
  of magnitude (Close removes without counting); `NvEvRef` > 0 is the refusal at the per-process
  bound (130 before the registry followed the handle bound; 4098 now).
* **Mappings** do not accumulate per frame in the present path (persistent maps); `NvWinMaps` and
  `NvMap` show it if they do. **Pins** are not in the present path (`NvPin - NvUnpin`, `NvPinQRef`).
* No fixed table in the release book (`scanout_release.rs`, 32 slots) can fail a call: it
  overwrites its oldest entry (counted as evicted), so it cannot be the cause.

So: after a failed submit read, in this order, `NvRef`, `NvHdlLive`/`NvHdlPeak`, `NvFence`/`NvFenceCl`,
`RmGRef`/`RmGAtt`/`RmGFire`, `PrdPend`/`PrdHi`/`PrdFull`, `NvEvRef`, `NvMapQRef`/`NvWinR*`,
`NvPinQRef`, `QfRet`. The registry values are published at most 255 NVRM calls late (section 7).

### 13.8 Table growth and the sanity bounds

The handle and mapping tables used to be fixed arrays reserved at init (so nothing allocates under
the spinlock): 1024 handles with 128 per process, 1024 maps with 256 per process. Those numbers
were guesses; the limits that matter are the host's (it opens real files: 4096 unsignalled fences)
and RM's own. The tables now start at 1024 slots and grow by doubling to a SANITY bound that no
real client reaches (handles 16384 / 4096 per process; maps 8192 / 4096 per process, the size of the
adapter-wide view table they feed), counted when hit (`NvHdl*Ref`, `NvMapTRef`, `NvSanityRef`).

* Growth never allocates under the lock. `grow_nvrm_tables` runs at PASSIVE before the reservation
  that needs room (`Open`, a fence create, `MMAP`): under a short lock hold it asks the pure
  `want_capacity` whether fewer than 16 slots are free, allocates the new storage with no lock, takes
  the lock again and swaps it in (`append`: a copy, no allocation), and frees the old, empty storage
  after the lock. A push still only ever happens into a free slot (the reservation checks
  `live < capacity`), so a lost race refuses (`NvTblOom`) instead of allocating.
* Fairness: while a table is 3/4 full, a process already holding its fair share is refused
  (`NvHdlFRef`), so the last quarter of the table is only for processes below it. The fair share
  is 1/8 of the handle table (2048: it must be BELOW the per-process bound of 4096, or the
  per-process refusal fires first and the rule is dead; the first shape had exactly that bug) and
  1/4 of the mapping table (2048 of 8192). Four hostile processes can take 12288 handles between
  them and no more; a fresh process (DWM) still gets slots (`hostile_owners_cannot_starve_a_fresh_one`,
  and `production_shapes_can_actually_be_fair` holds both shapes to it). Below the scarce point
  only the per-process bound applies.
* **Init frame budget.** `VirtioGpu::init` builds the transport by value on the boot stack, and
  the `StartDevice` + `init` pair was at its ceiling (17936 bytes was the last good nested pair, 18800
  did not boot, `tools/kmd-frame-sizes.ps1`). The window account and both tables' bounds therefore
  live in ONE `Box<NvrmLimits>` built by `new_window_account` (`#[inline(never)]`, one pointer
  returned, its registry reads in its own frame), so `init` gains 8 bytes of struct and no return
  slot. **The script must be run on the build** (`new_window_account` is in its default symbols and
  chains); the numbers were not measured here.
* **Restores cannot grow, so they have their own slots.** A forwarded `Close` takes the entry out
  of the table first (the host may reuse the number at once) and puts it back if the host did not
  take the close; the KMD's own fence close does the same. Both run where nothing may allocate, and
  used to reserve a slot like an `Open`, so a table that filled up in between silently untracked a
  handle still open on the host. The handle table now keeps `restore_slack` = 8 slots of storage
  that no reservation may take (`rm_limits::admit` stops reservations 8 short of the capacity, and
  the PASSIVE pre-grow keeps 16 free, so growth stays ahead); `restore_nvrm_handle` /
  `restore_fence_after_failed_close` use them with no bound and no growth. If even those are gone
  `NvRestLost` counts it (read 0). `NvWinPolicy` = 0 has no slack (the old shape, 0 slots).
* The window account's owner rows (512) are reserved at init; a 513th device mapping at once is
  `NvWinRTab`.
* The event registry grows the same way (`Registry::want_capacity` / `spare` / `install`, run by
  `grow_nvrm_tables` before `EVENT_REGISTER`), to a bound derived from the handle bounds
  (`rm_limits::EVENTS`): a process may register an event on every handle it can hold, and no
  more. Registrations hold an object reference each, bounded by the handle bound.
* Behaviour in the working range is unchanged: refusals only appear past the old numbers.
* `NvWinPolicy` = 0 restores the fixed tables.

### 13.9 Audit of the other fixed limits

"Host/protocol" = a number the host or the ABI defines (keep, derive from it). "Arbitrary" = a guess
(make dynamic). Nothing below was changed except the first two rows; the order is the recommended
order of work. Safety rule for every one: a global sanity bound plus a per-owner share while the
global is scarce, counted, grown at PASSIVE outside the lock (13.8).

| # | limit | value | where | kind | recommendation |
|---|---|---|---|---|---|
| 1 | handle table | 1024 / 128 | `nvrm_tables.rs` | arbitrary | DONE (13.8) |
| 2 | mapping table | 1024 / 256 | same | arbitrary | DONE (13.8) |
| 3 | pins | 1024 / 256 | same | arbitrary (the lock cost is the process's own locked-page quota) | next: same growth (`Vec<NvrmPin>`), the `NvPinQRef` counter exists |
| 4 | event registrations | 1024 / 130 | `nvrm_events.rs`, `kmd_logic::nvrm_events::Registry` | arbitrary | DONE: derived from the handle bound (4098 per process) and grown (13.8); a process past ~128 live fences no longer fails at `EVENT_REGISTER` |
| 5 | RM client table (`NvDupHarden`) | 256 / 32 | `kmd_logic/nvrm_clients.rs` | arbitrary, `[Slot; 256]` in a box | make a growable `Vec`; per process 32 is far above one NVK process (it makes a few roots) |
| 6 | attached (KMD-held) fences | 512 | `nvrm_tables.rs` | KMD budget, below gates x points (8 x 128 = 1024) | derive from gates x points or raise with them |
| 7 | RM gates / points per gate | 8 / 128 | `rm_gates.rs`, `rm_fence_present.rs` | arbitrary (the UMD falls back to a CPU wait) | gates per process count (`MAX_STREAMS` 64 is the adapter-wide stream table, `lib.rs`) |
| 8 | fence early table | 16 | `nvrm_fence.rs` | arbitrary, loses a wake counted `NvFenceErr` | keep (bounded by creates in flight) or make it a `Vec` |
| 9 | scanout fenced queue | 8 | `rm_fence_present.rs`, `HELIOS_NVRM_SCANOUT_FENCE_DEPTH` | protocol (the UMD knows 8) | keep; `QUEUE_FULL` is the contract |
| 10 | scanout release book | 32 | `scanout_release.rs` | arbitrary, SELF-EVICTING (cannot fail a call) | keep |
| 11 | producer completion | 8192 allocations (ABI), 8192 pending, 16384 writers, 64 marks, 1024 waiters, 16384 bindings | `producer_completion.rs`, `adapter/producer.rs` | slots: protocol (`HELIOS_PRODUCER_SLOTS`, the status page); the rest arbitrary, allocated at `StartDevice` | keep the slot count; make pending/writers grow only if `PrdHi` ever nears them (`PrdPend`, `PrdHi`, `PrdFull` exist; 100 000-entry soak test) |
| 12 | windowed-blt tokens | 64 | `gpu/mod.rs` `MAX_WINDOWED_BLT_PENDING` | tied to `HELIOS_READ_LEDGER_SLOTS` = 65 (ABI, the ledger page) | keep with the ABI; derive both from the flip queue depth in a version bump |
| 13 | scanout allocation slots | 32 | `create_allocation.rs` | arbitrary (`ScAlcFul`) | grow (also 512 foreign records, 64 per owner, `foreign_resource.rs`, `rm-backed-standard.md` item 5) |
| 14 | foreign resources | 512 / 64 per owner, 4 GiB per owner, 2048 open rows | `foreign_resource.rs` | arbitrary | grow with 13 |
| 15 | adapter-wide view table | 8192 | `mapping.rs` | arbitrary (Doom lesson: raised once) | growable behind the same PASSIVE pre-grow; it is the real bound of 2 |
| 16 | blobs / resources / contexts | 8192 / 16384 / 1024 | `gpu/mod.rs` | arbitrary | counters exist (`BLOB_FULL_REJECTS`, `RESOURCE_FULL_REJECTS`, `CONTEXT_FULL_DROPS` in QUERY_STATS); grow |
| 17 | in-flight control entries | `CTRL_QUEUE_SIZE` (virtqueue), parked 4x | `gpu/mod.rs` | host-derived (queue size) | already derived |
| 18 | Venus window ranges (region 3) | 1024 free ranges | `WindowAllocator` | arbitrary: past it a freed range is DROPPED (fragmentation leak, `WINDOW_RANGE_DROPS`) | grow the free list or coalesce harder |
| 19 | fence waiters / events | 64 / 256 | `gpu/mod.rs` | arbitrary (`WtTbl`) | grow |
| 20 | event virtqueue | 16 buffers | `nvrm_events.rs` | boot-stack frame budget | keep (events are rare); `NvEvDrop`/`NvEvErr` say if it matters |
| 21 | flip queue depth, S4 FIFO | 1..16, 8 | `query_adapter_info.rs`, `rm_fence_present.rs` | protocol / WDDM caps | keep |
| 22 | pin pages per pin | 262143 | `nvrm_tables.rs` | protocol (the indirect run table) | keep |
| 23 | one map | 256 MiB | `nvrm.rs` | KMD (MDL allocation, `ULONG` length) | keep; the window total is the budget |

### 13.10 Checklist

1. Install; `reg query HKLM\\SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render`: `NvWinMb` is the
   BAR1 size in MiB (32768 for 32 GiB), `NvWinPol` 1, `NvWinCapMb` = `NvWinMb`, `NvWinResMb` 256.
2. Run `crm_smoke` and `crm_pin_smoke`: `NvWinUseMb` rises while mappings are held and returns to 0
   (`NvMapMb` too); `NvWinPeakMb` keeps the peak; `NvWinMaps` follows `NvMap`.
3. Run NVK / the desktop with DWM-on-NVK: `NvWinPriv` is 1 (the device that set the scanout source);
   `NvWinT1Pid` is the process with the most mapped; `NvWinRFull`, `NvWinRRes`, `NvWinRBig` 0.
4. Pressure test (several processes mapping to the cap): refusals count in `NvWinRRes` first (the
   ordinary limit), `NvWinRFull` only when the privileged device itself fills the window;
   `NvWinRsvUse` > 0 only while DWM is in the reserve.
5. Long run: `NvHdlLive` and `NvFence - NvFenceCl` stay flat; `NvHdlGrow` 0 for a normal game (one
   growth appears past about 1000 handles); `NvSanityRef`, `NvTblOom` 0.
6. Fallback: `reg add ... /v NvWinPolicy /t REG_DWORD /d 0`, restart the device: the old quota
   and fixed tables are back (`NvWinPol` 0).
7. `NvWinHErrno` 12 with `NvWinRHost` rising and `NvWinUseMb` well below `NvWinCapMb` means the
   host's caching zone is full, not the KMD's accounting: raise the host window or look at the
   host's write-combine zone.

### 13.11 Verification status

* `cargo test` in `guest/windows/kmd_logic`: `rm_window` (hostile sizes, u64 overflow, 64 GiB windows,
  reserve boundary exact to the page, per-owner accounting, free-all-by-owner, sticky privilege,
  top-N, fragmentation, a 20 000-step interleaving checked against a full recomputation, legacy
  equals the old function), `rm_limits` (growth, bounds, fairness, the grow-then-reserve protocol),
  `window_units`, and the counter-name uniqueness scan of both trees.
* `kmd_render` cannot be built here: it was type-checked as a whole crate against the `wdk-sys` stub
  harness, comparing the errors before and after (no new ones apart from the stubbed
  `PsGetCurrentProcessId`); nothing was run in a Windows guest. NOT VERIFIED: the real build, that
  `wdk_sys::ntddk::PsGetCurrentProcessId` is bound, the grow-and-swap under load, the 32 GiB BAR
  assignment by the guest, DWM's actual `SCANOUT_SET` ordering relative to its first maps.
* Risks: a process can claim the reserve with a `SCANOUT_SET`; the byte total cannot see the host's
  zones (13.1); table scans are `O(n)` under the lock (13.5); the privilege grace (13.3) lets
  any device that appears within 30 s of the shell's loss use the reserve.

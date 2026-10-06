# Shared foreign surfaces: opening an NVK (RM) allocation from another process

Status: KMD half implemented on `kmd/s6-shared-foreign` (plan stage S6, section 3.7 of
`docs/dxvk-on-nvk.md` on `research/dxvk-on-nvk`); the re-export route of section 6 (route R) is
implemented as `RM_RESOURCE_IMPORT` on `kmd/rm-resource-import`. Nothing here has run on a Windows
guest.
The UMD / NVK half (who opens, who imports) is written by another session against the ABI
in section 2. Read `zero-copy-present.md` (sections 3, 10) first: this document starts where
its adoption (S3) ends.

## 1. The question, and the short answer

Process A (an NVK-on-RM game) rendered into RM memory, imported it with
`HELIOS_ESCAPE_FOREIGN_RESOURCE IMPORT_RM`, and adopted the resulting resource id into a WDDM
allocation. Process B (DWM, today on Venus; later another NVK process) opens that allocation
(`D3DKMTOpenResource`, `OpenResourceFromNtHandle`, `DxgkDdiOpenAllocation`). What must the KMD
guarantee, and what must it hand B?

**B gets a resource id, a size and a layout, and nothing else of A's.** Never an RM handle, a GEM
handle, a DRM file or an export fd, and never a CPU view. The KMD guarantees that the resource
is live, adopted and not yet destroyed when the open succeeds; that the host resource stays alive
for as long as B holds the open, whatever order dxgkrnl destroys and closes in; that the layout
and size B reads are the KMD's own records, not what A wrote; and that B's open is the recorded,
sanctioned route to attach the resid to its own Venus context.

## 2. What the opener gets (the ABI)

`DXGK_OPENALLOCATIONINFO.pPrivateDriverData` of a foreign allocation is 128 bytes
(`HELIOS_WDDM_PRIVATE_WITH_LAYOUT_BYTES`; the adoption refuses a smaller buffer). The KMD
rewrites two of its three parts at every open:

| bytes | record | written by | notes |
|---|---|---|---|
| 0..48 | `HeliosWddmOpenIdentity` (`'HIDN'`, version 2) | KMD, every open | `resource_id`; `kind = DEVICE_MEMORY`; `blob_size = venus_alloc_size =` the recorded, host-verified object size; `reserved[0] = HELIOS_WDDM_OPEN_FLAG_FOREIGN`, `reserved[1] = 0`; `ctx_id` is the holder context of A's device (diagnostic: B must not use it); `memory_type_index` is A's and means nothing for a dma-buf import |
| 48..96 | `HeliosWddmAllocMeta` | A, at create | `width`, `height`, `pitch`, `plane_offset` were proven equal to the KMD's record at adoption; `format`, `dxgi_format`, `bind_flags`, `misc_flags` are A's word, not validated (as for any adopted allocation) |
| 96..128 | `HeliosWddmAllocLayout` (`'HFLY'`, version 1) | KMD, create and every open | `modifier`, `fourcc`, `stride`, `plane_offset` from the foreign record |

The resource-level buffer (`args.pPrivateDriverData`) carries the identity (with the flag) but no
layout: **read the layout from the per-allocation buffer of the same `pOpenAllocationInfo2[i]`
entry.**

### 2.1 Additions (all backward compatible; every struct size is unchanged and asserted)

* `HELIOS_WDDM_OPEN_FLAG_FOREIGN = 1` (`protocol/src/wddm.rs`): bit 0 of
  `HeliosWddmOpenIdentity.reserved[0]`, with `reserved[1] == 0`. A DEVICE_MEMORY identity that
  carries a global VidMm tracker has both words nonzero, so the two shapes cannot collide, and
  every existing reader acts on `reserved` only through `global_vidmm_tracker()` (both words
  nonzero), so it ignores a foreign identity's words. The identity version is **not** bumped (an
  old opener would reject version 3). Accessor: `HeliosWddmOpenIdentity::foreign()` (valid,
  version >= 2, kind DEVICE_MEMORY, resource id nonzero, `reserved[1] == 0`, bit 0 set).
* `HeliosWddmAllocLayout::read_open(private: &[u8]) -> Option<Self>`: the trailer of a buffer, or
  `None` if the buffer is shorter than 128 bytes or the record is invalid.
  `HeliosWddmAllocLayout::agrees_with(&HeliosWddmAllocMeta)`: `stride == meta.pitch` and
  `plane_offset == meta.plane_offset`.
* `HELIOS_FOREIGN_CAP_SHARED_OPEN = 1 << 1` in `HeliosForeignQueryCaps.caps_flags` (also in
  `protocol/include/helios_foreign.h`). Set by every KMD that knows the verb, **independent of
  the host gate** (`CAP_RM_IMPORT`): it says "this KMD marks the identity, rewrites the trailer
  and refcounts opens". A producer about to share a foreign allocation should require it (an
  older KMD would open it without the flag and without the lifetime guarantee). The
  `QUERY_CAPS` struct is unchanged (96 bytes).
* Open errors: `STATUS_INVALID_PARAMETER` (the resid is dead, was never adopted, or its
  allocation was already destroyed), `STATUS_INSUFFICIENT_RESOURCES` (open table full),
  `STATUS_DEVICE_NOT_READY` (no transport).

### 2.2 The opener's recipe (UMD, DWM's Venus path, an NVK consumer)

1. Once per process: `QUERY_CAPS`; require `CAP_SHARED_OPEN` (and an identity with `foreign()`),
   else treat the allocation as unsupported.
2. In `pfnOpenResource`: parse the identity of the per-allocation buffer. `foreign()` selects the
   dma-buf-modifier import, not the plain opaque-fd path.
3. `HeliosWddmAllocLayout::read_open(private)`; check `agrees_with(meta)`.
4. `HELIOS_ESCAPE_ATTACH_RESOURCE(own Venus ctx, resource_id)`. This is the sanctioned route
   (section 4): the open recorded B's process.
5. Import: `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`, explicit plane layout
   `{offset = plane_offset, rowPitch = stride}`, `drmFormatModifier = modifier`,
   `VkExternalMemoryImageCreateInfo{DMA_BUF}`, `VkImportMemoryResourceInfoMESA(resource_id)`,
   dedicated allocation, `allocationSize <= blob_size` (host relaxed the exact-size rule, see
   `zero-copy-present.md` 10.5).
6. B must **not** `CreateAllocation` with `adopt_resource_id` for that resid (refused
   `AlreadyAdopted`), must not `RELEASE_BLOB` it (the slot is KMD-owned: harmless no-op) and
   cannot `MAP_BLOB` it (refused for every caller).
7. Closing the shared handle is all B does to end its reference.

## 3. Lifetime: the host resource lives until the LAST of {allocation destroyed, every open closed}

Before this change the host unref ran at the adopting allocation's destroy
(`destroy_allocation_ctx`), unconditionally, and the opens were bookkeeping-free
(`OpenAllocationContext` kept no reference). Whether a destroy can arrive while another process
still has the allocation open is dxgkrnl's contract, and it cannot be verified from this tree:
the in-tree evidence (DWM opening Venus surfaces whose creator exits) only says the existing
path survives it. So the KMD no longer depends on the answer.

`helios_kmd_logic::foreign_resource::ForeignTable` now counts opens per `(resource, process)`
(process = dxgkrnl's `hKmdProcess`, the same token the present-buffer capability uses):

```text
 adopted (destroyed = false)
    | open(p) / close(p): rows change, nothing is released
    | allocation_destroyed
    +- no opens -------------> Release   (the destroyer releases)
    +- opens > 0 -> destroyed = true, Deferred
                      | open(p): refused (the allocation is gone)
                      | close(p) ... the close that drops the last open -> Release
```

* `Release` is returned exactly once per resource: by `allocation_destroyed` when nothing is
  open, else by the `close` that drains the last open of a destroyed allocation. Both
  transitions test and set `destroyed` in the same call (one device-lock hold), so two racing
  destroys, or a destroy racing the last close, cannot both release. A model test runs 12,000
  random open / close / destroy steps against a reference count
  (`release_happens_exactly_once_after_the_last_of_destroy_and_close`).
* Both triggers end in one function, `ctrl::release_allocation_resource`: drop the blob slot
  (and the record, which also drops the open rows), then the first claimant of the live-resource
  entry detaches from the holder context and `RESOURCE_UNREF`s. `take_live_resource` is the
  existing one-shot guard, so a sweep that got there first (StopDevice, DestroyDevice of a
  not-yet-adopted resource) turns the late release into a no-op; the record being gone makes
  the opener's late close `Gone`, never a release.
* The holder context for a deferred detach is read from the foreign record in the same lock
  hold as the decision (the destroyer's `AllocationContext` is gone by then). A context that
  died with its device only makes the detach fail; the unref still happens.
* Under dxgkrnl's documented order (every open closed before the allocation is destroyed)
  `Deferred` never occurs. It is counted (`FgDefer`), so a run shows whether the guard was ever
  needed. The cost of the guard if dxgkrnl never calls CloseAllocation for an open this driver
  counted is a leaked host resource until the transport drops, visible as `FgOpLive` (opens
  counted minus closes) and `FgOrphan` that never return to 0.
* Other release paths are unchanged and cannot race into a double unref: `RELEASE_BLOB` and the
  DestroyDevice sweep of the importing device only reach resources still owned by that device
  (before adoption); the transport sweep takes every KMD slot at once and unrefs under the same
  guard.

What the creator's death does: A's DestroyDevice finds the adopted slot KMD-owned (owner `None`)
and leaves it; the allocation survives through B's handle; A's RM handles close independently
(the host import holds its own dma-buf reference, `zero-copy-present.md` 3.3).

## 4. `ATTACH_RESOURCE` of a foreign resid

Any live resid is still accepted (unchanged), but an attach of a foreign one is now counted and
classified (`ForeignTable::note_attach`):

* **sanctioned**: the escaping device created the resource (before adoption its importing
  device), or the escaping device's process (`hKmdProcess`) holds an open of the allocation
  that adopted it, which is what B's `OpenAllocation` recorded;
* **unsanctioned**: neither (`FgAttUns`).

`virtio/foreign.rs::ATTACH_ENFORCE` (`false`) turns the unsanctioned case into
`STATUS_ACCESS_DENIED`. Flip it when a run with DWM, the bridge and NVK's holder context shows
`FgAttUns == 0` for every legitimate consumer. Known holes this does not close: the context
named by the attach is not checked against the caller (as for every attach today), and an
attacker in a process that does hold an open may attach to a context it does not own.
Everything is counted, not hidden.

## 5. RM handles across processes

RM handles are per D3DKMT device (`DeviceOwner`, `docs/nvrm-escape.md` section 1), not
transferable, and **they must not become shareable**. The rules, as implemented:

* R1. The KMD never copies a handle from one owner's table to another's. The foreign record
  keeps `rm_handle` / `gem_handle` for provenance only; no escape or DDI returns them to anyone
  but the importing device (which supplied them), and the open identity carries neither.
* R2. What a process may do with another process's NVK surface: **open the shared allocation,
  attach the resid to its own Venus context, import it as a modifier image, and (as a WDDM
  allocation) flip or compose it.** That is the whole list. It cannot map it, release it, adopt
  it again, query RM state of it, dup its RM object or learn its handles.
* R3. Closing A's RM handle neither releases nor invalidates the resource for B (independent
  lifetimes, by design).
* R4. Known gap, not fixed here (security last): `FORWARD` does not check handles *inside*
  payloads. From `host/backend/device/src/nvidia/nested.rs`, the guest-visible "handle" in these
  slots is an entry of the host's per-connection handle table, i.e. the same numbering as the
  backend handles the KMD tracks per owner, so another process's number is guessable and the
  host honours it: `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` (0x3d05, nested offset 16),
  `IMPORT_OBJECT_FROM_FD` (0x3d06, nested offset 0), the NV0005 event class `data` (offset 16),
  the NVKMS `memFd` at the ioctl's `nested_fd_offset`, plus RM `DUP_OBJECT`'s `hClientSrc`. A
  hardening pass must check each against `nvrm_handle_owned(owner, ...)` before forwarding.
  Until then cross-process sharing "works by accident" through those slots; nothing in this
  design relies on it, and nothing may.
* R5. Known gap, not fixed here (security last): **adoption of a KMD-created resource is weaker than
  the same-device rule.** A resource the KMD's own RM client made (`KmdRmClient` = 4,
  `kmd-rm-client.md` section 14; creator token `KMD_RM`) has no creating device, so
  `VirtioGpu::adopt_for_allocation` cannot ask "is the holder context still the creating device's":
  for that creator it asks only that the record's context is nonzero (`ctx_id != 0`; the pure table
  still requires the allocation to name the record's context, which is the KMD's own Venus context,
  a small integer a process can try) and that the blob slot is still `KMD_RM`'s. Any process whose
  `D3DKMTCreateAllocation` declares a foreign resource with the right resid, context and exact
  geometry can adopt an unadopted KMD-created resource. Narrow today (level 4 is a validation knob
  and the ring surfaces are not meant to be adopted by anything), and closed by the same fix as
  the user-created case (an adoption cookie from the creator, or the caller's process identity at
  `CreateAllocation`; see `zero-copy-present.md` section 8), where the KMD's own allocation arm
  (`kmd-rm-client.md` 14.2) would not need one.

## 6. Do NVK-to-NVK consumers need a re-export route?

Two consumers exist:

* **Venus consumers** (DWM today, the D3D11/D3D12 UMD bridge): the resid route above is
  complete. No re-export.
* **NVK consumers** (DWM on NVK at S6, an NVK process opening another NVK process's surface):
  they cannot import a Venus resid (there is no Venus-to-RM direction, plan 4.3), so for them the
  resid route ends at "attach", and they need the memory as an RM object in their own client.

Options for the second case:

| route | what | verdict |
|---|---|---|
| **F** fd hand-off | A's exported fd (a backend handle of A's device, an `Open`ed control file bound by 0x3d05) is passed to B, B runs 0x3d06 on it | **Rejected.** It needs a second capability system beside WDDM sharing (a token the KMD mints), breaks the one-owner rule of every table (handles, pins, mappings, events, teardown), and A's `Close` of the fd would pull the memory from B. It needs the host to dup the fd anyway. |
| **R** reverse export | a new FOREIGN op `RM_RESOURCE_IMPORT(resource_id, drm_rm_handle)` returns a GEM handle in the caller's DRM file made by the host from the resource's own dma-buf; the caller then runs `DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY` -> fd -> 0x3d06 and gets its own RM object | **Chosen and implemented** (section 6.1) on top of the host's `RmResourceImport` message (`MsgType` 31, host branch `feat/s6-backend`, `docs/VENUS.md` "RM-export resources in a second process"). |

KMD requirements of route R, each already available after this change:

1. the caller's process holds an open of the allocation that adopted the resource
   (`ForeignTable::process_has_open`), which is exactly dxgkrnl's own access check
   (the caller got the shared handle) recorded by the KMD;
2. the caller owns `drm_rm_handle` and it is a DRM file (`nvrm_handle_device_type >= 512`,
   the `IMPORT_RM` rule);
3. a quota (reuse the per-device count and bytes of `IMPORT_RM`: the new GEM handle pins the
   same host memory) taken by the same reserve/commit protocol;
4. the resulting `(drm file, gem)` is recorded so the caller's DestroyDevice and the transport
   sweep release it with the host's own teardown (the host import holds its own reference, so
   it outlives A);
5. unreachable from `FORWARD` (the verb is a separate escape, like `IMPORT_RM`), so the
   unchecked payload handles of section 5 R4 are not part of its trust.

Items 1, 2 and 5 are implemented as written. Items 3 and 4 were decided the other way, see
6.1 ("What changed from the sketch above").

### 6.1 `RM_RESOURCE_IMPORT` (implemented)

**ABI** (`protocol/src/foreign.rs`, mirrored in `protocol/include/helios_foreign.h`, sizes and
offsets asserted on both sides). A new op on `HELIOS_ESCAPE_FOREIGN_RESOURCE` (0x0018):

| item | value |
|---|---|
| op | `HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT = 3` (bit 3 of `QueryCaps.supported_ops`) |
| cap bit | `HELIOS_FOREIGN_CAP_RM_RESOURCE_IMPORT = 1 << 2` in `QueryCaps.caps_flags`. Set only when `RM_IMPORT` is served (config features bits 13 and 10) AND the host advertises `NVGPU_CFG_RM_RESOURCE_IMPORT` (bit 14). Unlike `CAP_SHARED_OPEN` it needs the host |
| struct | `HeliosForeignRmResourceImport`, 80 bytes: header (40) \| `rm_handle u32` @40 in \| `resource_id u32` @44 in \| `flags u32` @48 in (0) \| `out_gem_handle u32` @52 \| `out_size u64` @56 \| `out_modifier u64` @64 \| `out_flags u32` @72 (`HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER` = bit 0) \| `out_host_errno u32` @76 |
| status | `HeliosForeignHeader.status`: `OK`; `UNSUPPORTED` (gate closed, host `EOPNOTSUPP`, older backend `EPROTO`); `NOT_OWNED` (every KMD refusal below, host `EBADF`/`ENOENT`); `BAD_RANGE` (zero ids, nonzero flags, host `EINVAL`/`ERANGE`); `NO_RESOURCES` (host `ENOMEM`); `DEVICE_ERROR` (anything else: transport failure, short or malformed reply, other errno). `STATUS_DEVICE_NOT_READY` for the escape if there is no transport |

**The caller's recipe** (NVK, user mode): `QUERY_CAPS` requires `CAP_RM_RESOURCE_IMPORT`; open the
shared allocation (the open identity gives `resource_id`); `FOREIGN RM_RESOURCE_IMPORT{rm_handle =
own DRM node, resource_id}`; `GEM_EXPORT_NVKMS_MEMORY(gem)` to a control descriptor of its own;
`NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD` (0x3d06) into its RM client; `DRM_IOCTL_GEM_CLOSE`
(all `FORWARD`ed). The same resource on the same file answers the same handle, so one close undoes
any number of imports. The memory the RM client imported lives independently of the GEM handle, of
the resource and of A.

**What the KMD checks** (`helios_kmd_logic::rm_resource_import::authorize`, evaluated in ONE device
lock hold with the handle table read: `VirtioGpu::rm_resource_import_begin`):

1. `rm_handle` is a backend handle of the caller's DEVICE (`nvrm_handle_device_type(owner, h)`) and
   a DRM node (`>= 512`: not the control file, a GPU minor, UVM, a fence);
2. the resource is a foreign record, not destroyed (a destroyed allocation whose host resource is
   kept alive for opens still draining is "defer-pending": no NEW reference may start on it);
3. the caller's device created it (`IMPORT_RM`, before adoption), OR the caller's process
   (`hKmdProcess`, 0 disables this route) holds an open row of it (the `FgOpen` record);
4. the request is built exactly (`MSG_HDR + 16` bytes, `flags = reserved = 0`);
5. the reply is read only as far as the transport says it was written; a success needs
   `>= MSG_HDR + 24` bytes and a nonzero GEM handle; unknown `flags` bits are dropped and the
   modifier is 0 unless bit 0 says it is valid;
6. after the round trip (no lock held across it), in one hold: the caller still owns `rm_handle`
   in the same transport generation (`nvrm_epoch`). A handle closed during the wait may be reused
   by the host for another process's file, so a GEM handle made in it is withheld (`FgRiStale`).

Every refusal of 1 to 3 is `NOT_OWNED`, one code, so a process learns nothing about another's
handles or resources. After ADOPTION the creating device is no longer "the creator" (the record's
creator is `None`) and holds no open row, so it cannot use this op for its own surface: it already
has the GEM handle it imported from. A second device of the same process as an opener may: the open
row is per process.

**What changed from the sketch above.**

* *Quota (item 3)*: none. The new GEM handle is per (resource, file), the host answers the same
  handle for a repeat, and the files are the caller's own (their count is the existing handle
  quota), so the number of live GEM handles is bounded by resources times the caller's DRM
  files. A per-device byte count would double-count memory the host already holds for the
  resource.
* *Recording the GEM handle (item 4)*: none. The host closes a DRM file's GEM handles when the
  file closes, and the KMD already closes every DRM file a device left open (`DestroyDevice` and
  the transport sweeps), so there is nothing to release that the existing teardown does not, and
  a record would be a second table to keep consistent with `GEM_CLOSE` going through `FORWARD`
  (which the KMD does not parse). The cost: a caller that never `GEM_CLOSE`s keeps the host
  memory pinned until its file closes, bounded by its own handle quota.
* *FORWARD*: message 31 is NOT in `HELIOS_NVRM_FORWARD_MSG_TYPES` (asserted by a `const` in the
  protocol crate). The only route to it is this op, so the gate above cannot be bypassed.

**Counters** (registry, throttled with the rest of this verb): `FgRiOk` (GEM handles returned),
`FgRiRef` (KMD refusals before the wire, incl. nonzero flags), `FgRiErr` (round trips that failed
or that the host refused or answered badly), `FgRiUns` (gate closed), `FgRiStale` (replies
withheld because the handle or the transport changed during the wait). Healthy: `FgRiErr` and
`FgRiStale` 0; `FgRiRef` counts a caller that tried without an open.

**Known gaps.** The process an open is recorded under is dxgkrnl's `hKmdProcess`; the check
compares it with the escaping device's (the same rule as `ATTACH_RESOURCE`). Another process
that learns a resource id still cannot get a handle (it has no open row), but a process that holds
an open may name ANY of its own DRM nodes, which is what it is for. The host's reply `size` and
`modifier` are not cross-checked against the KMD's own record (the dma-buf may be rounded up).
The cross-client slots of `FORWARD` (`RM_DUP_OBJECT` and the fd slots) remain unchecked; the list
is in `nvrm-escape.md` section 10.1.

## 7. Counters (registry, throttled: first and every 16th open/close, every 64th escape)

| name | meaning | healthy |
|---|---|---|
| `FgOpen` / `FgClose` | counted opens / matching closes of adopted foreign allocations | `FgOpen - FgClose == FgOpLive` |
| `FgOpLive` | opens alive now, all processes | returns to 0 when the surfaces are gone |
| `FgOpRf` (+ `FgOpRfC`, `FgOpRfN`) | opens refused (code: 1 not adopted, 2 destroyed, 3 table full) | 0 |
| `FgClsMis` | closes that found no record or no row of that process | 0 (nonzero: a swept record, or a lifecycle bug) |
| `FgDefer`, `FgDeferRl` | destroys that found opens alive and deferred the release / those completed by the last close | 0 under dxgkrnl's documented order |
| `FgOrphan` | destroyed allocations whose release still waits for opens now | 0 at rest |
| `FgDestDef` | resource id of the last deferred (or repeated) destroy | |
| `FgAtt`, `FgAttUns` | attach attempts of foreign resids / of those by a caller with no open and not the creator | `FgAttUns` 0 before `ATTACH_ENFORCE` goes on |
| `FgRiOk`, `FgRiRef`, `FgRiErr`, `FgRiUns`, `FgRiStale` | `RM_RESOURCE_IMPORT`: GEM handles returned / refused by the KMD before the wire / failed or refused by the host / gate closed / withheld after a handle or transport change | `FgRiErr`, `FgRiStale` 0 |

(Existing `FgImp FgRel FgAdo FgLive FgHi FgRef*` are unchanged and now also published from these
paths. All names are <= 14 bytes, asserted by the glue tests.)

## 8. Decision list

| # | decision | why |
|---|---|---|
| S1 | The opener's identity carries resid, recorded size and layout, and the FOREIGN flag; never an RM or GEM handle. | One sanctioned currency (the resid); nothing of A's namespace leaks. |
| S2 | The flag lives in `reserved[0]` bit 0 with `reserved[1] == 0`; the identity version stays 2. | Old openers ignore it; a version bump would reject them. The tracker shape cannot collide. |
| S3 | Size and layout at open come from the KMD's table, rewritten at every open. | The creator's private data is a claim; the record is host-verified and validated. |
| S4 | Opens are counted per `(resource, process)` in the foreign table; an open of a non-adopted or destroyed allocation is refused. | The host resource must outlive its openers whatever dxgkrnl's order is; refusing late opens means no new reference appears on a resource whose release is pending. |
| S5 | One release function, one-shot guard; the last of destroy and closes releases. | Single teardown path; no double unref. |
| S6 | `ATTACH_RESOURCE` of a foreign resid is counted and classified; enforcement is a const, off. | Keeps today's behaviour, makes the sanctioned route measurable. |
| S7 | `CAP_SHARED_OPEN` is KMD-only and always set. | A producer can refuse to share on an old KMD without a host round trip. |
| S8 | RM handles never cross owners; the cross-process route is the resid. | Every ownership table is per device. |
| S9 | Re-export is route R (resid -> GEM in the caller's DRM file), implemented as `FOREIGN RM_RESOURCE_IMPORT` over the host's `RmResourceImport` (msg 31); fd hand-off rejected. | The authorization is the KMD's (open row or creator, DRM node of the caller's device); the host cannot tell processes apart. |
| S11 | `RM_RESOURCE_IMPORT` records nothing and has no quota; message 31 is not forwardable. | The host closes GEM handles with the file and the KMD sweeps files; the handle is per (resource, file). |
| S10 | Payload-handle hardening of `FORWARD` stays deferred, with the exact slots listed. | Security last; nothing here depends on it. |

## 9. Tests, and what is not tested

| what | where | run |
|---|---|---|
| open/close/destroy state machine (14 tests: per-process rows, drain-order, deferred release exactly once, refusals, bounded storage, attach classification, a randomized reference-model test) | `kmd_logic/src/foreign_resource.rs` | host `cargo test` (scratch copy: cargo refuses inside the worktree) |
| ABI: identity flag shapes, layout trailer read from private data, C mirror of the new cap bit | `protocol/src/wddm.rs`, `protocol/src/foreign.rs`, `protocol/include/helios_foreign.h` | host `cargo test` |
| the real `foreign_tables.rs`, `foreign.rs`, `escape_foreign.rs` against a stub of the surrounding crate: open/close/destroy glue with the real table methods, a transport sweep between destroy and close, attach classification, `QUERY_CAPS` bits, every counter name <= 14 | a scratch harness (not in the tree) | `cargo test` in the stub |
| `RM_RESOURCE_IMPORT`: request builder, reply parser (short, malformed, flags bit 0, errno), the gate (creator / open / destroyed / not a DRM node / unknown process), errno mapping incl. `EPROTO` | `kmd_logic/src/rm_resource_import.rs`, `kmd_logic/src/foreign_errno.rs` | host `cargo test` |
| the same through the REAL `virtio/rm_resource_import.rs`, `virtio/gpu/rm_resource_import_tables.rs`, `ddi/escape_foreign_rm_resource.rs` and `escape_foreign.rs` against a stub whose `raw_roundtrip` is scripted and panics if the device lock is held across it: exact 32-byte request, refusals send nothing, gate bits 13/10/14, handle closed or transport restarted during the wait, short/broken replies, `QUERY_CAPS` bits | a scratch harness (not in the tree) | `cargo test` in the stub |
| the C mirror (`helios_foreign_rm_resource_import` sizes and offsets) | `protocol/include/helios_foreign.h` | compile with any C11 compiler; the Rust test pins the `#define`s |

**Not compiled and not run (RM_RESOURCE_IMPORT):** the `escape.rs` dispatch arm (computes
`hKmdProcess` like the attach arm), the `virtio/mod.rs` / `virtio/gpu/mod.rs` / `ddi/mod.rs` module
lines, and the real `ctrl::raw_roundtrip` (scripted in the harness). Never run against a host that
serves message 31.

**Not compiled and not run:** `ddi/create_allocation.rs` (open, close, destroy and unwind edits),
`ddi/escape.rs` (the attach arm), `virtio/ctrl.rs` (`release_allocation_resource`). They were
parsed by rustfmt and reviewed, nothing more (the crate cannot be built here). Nothing has run
on a Windows guest. **Unverified assumptions:** dxgkrnl closes an allocation's opens before it
destroys it (the guard makes this non-load-bearing); `hKmdProcess` is equal for every device of
a process (the open row and the attach check compare it; a mismatch shows as `FgAttUns`);
`DxgkDdiCloseAllocation` runs at PASSIVE_LEVEL (documented; the release round-trips the control
queue there).

## 10. Open questions

* **Present of a foreign allocation by the KMD itself.** `PresentAllocationStorage` has no
  foreign variant: a windowed `Blt` or scanout copy that the KMD's own Venus client does for an
  opened foreign source still imports it as an OPTIMAL opaque-fd image, which the host refuses
  (`zero-copy-present.md` 10.6 item 1, "Gate opened"). B composing in its own Venus context
  (this document's recipe) does not hit it; DWM's WDDM `Present` blit path would. Not touched.
* **The quota hole** (`zero-copy-present.md` finding 1) is unchanged: an adopted resource no
  longer counts against its creator, and an orphan (a destroyed allocation waiting for its last
  opener) holds host memory longer. The global table cap (512) bounds it; a global byte cap is
  the fix.
* Route R: done (6.1). Open: whether the quota-free design is acceptable once a hostile NVK
  process is in scope (a loop of `RM_RESOURCE_IMPORT` costs the host one GEM handle per
  (resource, file), not per call, but the caller's files are its own quota).
* `ATTACH_ENFORCE`: when to turn it on (section 4), and whether the named context should also
  have to belong to the caller.

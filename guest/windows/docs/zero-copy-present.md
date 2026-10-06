# Zero-copy presentation of NVK-on-RM images (Windows guest, KMD side)

Status: KMD half implemented behind a closed gate (`RM_IMPORT_SERVED = false`);
host half and the NVK/UMD half do not exist yet. Written against the code at
commit `0185243` on `kmd/zero-copy-present`.

Background: `docs/research/nvk-rm-windows.md` on branch `research/nvk-rm-windows`
(sections 4.1 and 4.3 are the intended design), `guest/nvk-rm/README.md` on
`nvk-rm/zero-copy` ("Zero-copy presentation": the Linux export route this reuses),
`docs/VENUS.md` and `docs/SCANOUT.md` for what the host does with a scanout blob.

## 1. The question, and the short answer

NVK-on-RM renders into RM memory (`HELIOS_ESCAPE_NVRM` / librmclient). The viewer
must scan it out with no CPU copy. Today every presentable image is a Venus
resource the KMD created, and everything downstream of creation keys off the KMD's
resource tables. So: what must the KMD accept?

**It must accept an RM-exported memory object as a Venus resource that it did not
create through `ALLOC_BLOB`, by minting the resource id itself.** It must not accept
a resource id from anywhere else. Nothing in the present or scanout code needs to
change: once the resource is in the KMD's tables it is indistinguishable, to every
later step, from a blob the KMD made. The missing piece was admission, which is what
`HELIOS_ESCAPE_FOREIGN_RESOURCE` (0x0018, `protocol/src/foreign.rs`) adds.

## 2. How a Venus image reaches the display today (verified)

The KMD's `resources` (a `Vec<u32>` of live ids) and `blobs` (`BlobSlot`: owner
device, context, size, mapping state) tables, both in `virtio/gpu/resource_tables.rs`,
are the authority for a resource id. The KMD owns the id namespace
(`alloc_resource_id`); user mode only ever holds ids the KMD returned.

| step | code | what it requires of a resource id |
|---|---|---|
| create | `ctrl::alloc_blob` (`ALLOC_BLOB` escape): reserve blob slot, `resource_create_blob` (reserve resource slot, mint id, `RESOURCE_CREATE_BLOB`, `CTX_ATTACH_RESOURCE`), commit blob and resource | creates both table entries; owner = the escaping device |
| import into another context (DXVK bridge, DWM) | `ATTACH_RESOURCE` escape -> `ctrl::attach_resource_checked` | `resource_is_live`; no owner check |
| adopt into a WDDM allocation | `D3DKMTCreateAllocation` private data `adopt_resource_id` -> `Backing::AdoptedUmdResource` (`create_allocation.rs`) -> `adopt_blob_for_allocation` / `live_blob_size` | live and has a `blobs` slot; adoption re-owns the slot to the KMD (`owner = None`) so the creator's DestroyDevice sweep cannot free it |
| open in another process | `DxgkDdiOpenAllocation` | `resource_is_live` (C1 gate) |
| scanout bind of a UMD-chosen image (D4b snapshot, "purpose 0") | present private data -> `snapshot_bind::validate` -> `ScanoutTarget::from_snapshot_descriptor` -> `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH` | only layout arithmetic; liveness in the flush executor (`resource_is_live`, `adapter/scanout.rs`) |
| scanout of an app image that is not the VidPn primary | `submit_primary_scanout_copy` -> `prepare_optimal_scanout_copy` (`venus/scanout.rs`): attach to the KMD's own Venus context, import as an OPTIMAL image, GPU copy into the adapter-owned LINEAR scanout image | live; UMD-claimed `alloc_size` and `memory_type_index` |
| host reads the image | `SET_SCANOUT_BLOB` binds the resource, `RESOURCE_FLUSH` makes the host export it as a dma-buf for the viewer, no copy (`host/backend/device/src/venus/scanout.rs`) | host-side resource |
| reuse / completion | flush token retires -> D4a READ LEDGER (resid-keyed slots, `adapter/read_ledger.rs`, `MAP_READ_LEDGER`, `SCANOUT_EVENT`) | any resid the KMD binds |
| teardown | `RELEASE_BLOB`, `release_blobs_for_owner` (DestroyDevice, StopDevice), `forget_allocation_blob` (allocation destroy) | the `blobs` slot |

Every row accepts any id that is in the two tables. None checks how the memory behind
it was made. That is why the design below touches only admission and teardown.

## 3. Design

```
NVK (librmclient)                          KMD                              host backend
RM memory --OS_UNIX_EXPORT_OBJECT_TO_FD--> (NVRM FORWARD, opaque)  ------>  nvidia ctl
DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on DRM file rm_handle  -------------->   GEM object {rm_handle, gem_handle}
HELIOS_ESCAPE_FOREIGN_RESOURCE IMPORT_RM {ctx, rm_handle, gem_handle, size} ->
        check: caller owns rm_handle (DRM file) and ctx; quota; mint resid
        RESOURCE_CREATE_BLOB{blob_mem=RM_EXPORT, blob_id=rm<<32|gem, size, resid} --> import GEM as dma-buf blob
        CTX_ATTACH_RESOURCE(ctx, resid)                                       -->
        blobs slot (owner = device) + resources entry + foreign record
<-- out_resource_id
then, per frame, existing paths (section 2): ATTACH to the bridge's context, adopt, bind
```

### 3.1 How the id reaches the KMD

A new escape verb, not an NVRM op. `IMPORT_RM` carries RM handles that the caller
proves it owns, but it is not RM forwarding: it creates a Venus resource that lives
in the resource tables. A separate verb also avoids op-number and `QUERY_CAPS`
collisions with the NVRM event work, and needs no change to `helios_nvrm_escape.h`.
ABI: 40-byte header (same shape as `HeliosNvrmHeader`), `QUERY_CAPS` (96 bytes) and
`IMPORT_RM` (72 bytes). The canonical C mirror is `protocol/include/helios_foreign.h`;
sizes, offsets and every constant are asserted on both sides (a Rust test parses the
header). `rm_handle` is the backend handle librmclient got from `Open` of a DRM node
(`device_type >= 512`); `gem_handle` is what `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`
returned in that file; `size` is the exported object's byte size.

### 3.2 Validation (all before any wire traffic, in this order)

1. Gate: `QUERY_CAPS.caps_flags & CAP_RM_IMPORT`, else `ST_UNSUPPORTED` and nothing happens.
2. Structure (`validate_request`, pure, host-tested): ctx, rm and gem ids nonzero, `flags == 0`,
   `size` a nonzero page multiple and at most 1 GiB. `ST_BAD_RANGE`.
3. Ownership, one lock hold with the reservation: the calling device opened `rm_handle`
   through `HELIOS_ESCAPE_NVRM` and it is a DRM file (`ST_NOT_OWNED`, one code, so
   another process's handles are not probeable); the calling device created `ctx_id`
   (`ST_BAD_CONTEXT`). The GEM handle is checked by the host against that DRM file.
4. Quota (`ST_NO_RESOURCES`): 512 foreign resources in total, 64 and 4 GiB per
   creating device (reservations count), plus the ordinary `MAX_BLOBS` / `MAX_RESOURCES`.
5. After the create round trip, in one lock hold: the blob slot is still the device's
   (device teardown may have popped it) and the RM handle is still the device's (it may
   have been closed and the number reused by another process; same rule as
   `push_nvrm_map`). Otherwise the reservation is returned and a closed handle also
   tears the new resource down through `release_blob_for_owner`.

### 3.3 Lifetime and ownership

- The blob slot is owned by the importing device. `RELEASE_BLOB(ctx, resid)` releases
  it; DestroyDevice and StopDevice sweeps reclaim it; all existing, unchanged.
- A WDDM allocation that adopts it (`adopt_resource_id`, `DEVICE_MEMORY` kind) takes it
  over exactly as for a Venus blob; the creator's quota is freed
  (`ForeignTable::adopt`) and allocation destroy releases it.
- Every path that pops a `blobs` slot also pops the foreign record (four one-line hooks in
  `resource_tables.rs`), so the record cannot outlive the resource and there is no second
  teardown path.
- **Closing the RM handle does not release the resource.** The host import holds its own
  reference to the memory; the resource is released only by the paths above. The RM
  handle's lifetime and the resource's are independent on purpose (NVK closes and reopens
  RM files freely).
- Reset: the table lives inside `VirtioGpu`, which is replaced with the transport, as the
  NVRM tables are. `epoch` (same value as `HeliosNvrmHeader.epoch`) tells a client that
  its resources are gone.
- **No CPU view**: `MAP_BLOB` and the BAR remap refuse a foreign resource (counted,
  `FgMapRf`). There is no byte of it the guest can touch, so no CPU copy is possible by
  construction.

### 3.4 Present fence / timeline

No new KMD mechanism in v1. The two consumers and what completes them:

- **Windowed (DWM composes).** NVK's Helios WSI hands the resid to the in-process
  `helios_umd` present vehicle; the DXVK-on-Venus bridge attaches it to its context
  (`ATTACH_RESOURCE`), imports it, and GPU-copies it into the DXGI surface; the present
  completes on the Venus ring fence of that copy, as for any Venus image. The KMD sees
  only a present of an ordinary allocation. This is zero CPU copy but one GPU copy, which
  DWM's redirection surface forces anyway.
- **Direct (fullscreen / independent flip, or a snapshot bind).** The image is a WDDM
  allocation that adopted the resid, or a snapshot descriptor names it. The flip completes
  when the display worker's `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH` token retires; the image
  is reusable when the READ LEDGER slot for that resid shows no read in flight
  (`MAP_READ_LEDGER`, event via `SCANOUT_EVENT`). All keyed by resid, so foreign
  resources work unchanged.
- **Producer-side sync.** NVK has no Venus semaphore. In v1 its WSI waits on the CPU for
  the image's RM semaphore before handing it over (`wait_before_present`, Mesa patch 12),
  so the image is complete when the KMD first sees the resid. A later step imports the RM
  semaphore into Venus as an external semaphore (RM `NV_SEMAPHORE_SURFACE` -> nvidia-drm
  sync_file -> host `VkSemaphore`, `docs/SYNC.md`); the `flags` field of `IMPORT_RM` and
  `caps_flags` are reserved for it. Nothing in this ABI forecloses it.

## 4. Decision list

| # | decision | why |
|---|---|---|
| D1 | The KMD mints the resource id; user mode never supplies one and learns it only from `out_resource_id`. | A user-supplied or host-minted id is a number the KMD cannot tell from another process's resource, and it collides with `next_resource_id`. The research doc's "host returns the resid" is rejected for this reason. |
| D2 | New escape verb 0x0018, not an NVRM op. | It creates a Venus resource, not an RM forward; no collision with NVRM op numbering or `helios_nvrm_escape.h`. |
| D3 | The wire is the existing `RESOURCE_CREATE_BLOB` with a vendor `blob_mem` (0x80000001) and `blob_id = (rm_handle << 32) \| gem_handle`, then the ordinary `CTX_ATTACH_RESOURCE`. | No new host message type; the host's blob handler gains one arm. `(rm_handle, gem_handle)` is the whole identity, and the KMD has proven the caller owns the first half. **Proposal, not agreed with the host** (O2). |
| D4 | `size` is a claim the host verifies (refuses if larger than the object); the KMD records it. | Gives the KMD a trustworthy size for the undersize guards, unlike a size a UMD states in allocation private data. |
| D5 | The foreign resource gets an ordinary `blobs` + `resources` entry plus a side record (`kmd_logic::foreign_resource`). | Every existing consumer (attach, adopt, open, flush, teardown) works unchanged; only quotas, provenance and the no-map rule need the record. |
| D6 | Caller must own both the DRM file and the Venus context (strict, same device). | Simple and sound. NVK's WSI must therefore make the escape from the device that holds the RM handles and has a Venus context. Same-process cross-device is deferred (O5). |
| D7 | Quotas: 512 total, 64 and 4 GiB per creating device, 1 GiB per resource; adoption frees the creator's share. | A foreign resource pins host VRAM until released, so tighter than blobs and counted in bytes. |
| D8 | Not mappable. | Guarantees no CPU copy; the memory may be VRAM. |
| D9 | v1 sync: NVK CPU-waits, `fence_value = 0`; reuse via the READ LEDGER. | Smallest thing that works; the RM-semaphore path is a later, additive step. |
| D10 | Counters: `FgImp FgRel FgAdo FgLive FgHi FgRefQ FgRefO FgRefC FgRefR FgRefH FgUns FgMapRf` (registry, throttled). | Project rule: every refused path is counted. `FgImp - FgRel` is what is live; a count that only grows is a leak. |
| D11 | Gate: `virtio/foreign.rs::RM_IMPORT_SERVED = false`, and `CAP_RM_IMPORT` is not advertised. | The host half and the wire shape do not exist. While closed, `IMPORT_RM` returns `ST_UNSUPPORTED` before touching any state. |
| D12 | Rejected: a KMD verb that takes a bare resource id ("register this resid"), and a direct RM-handle-to-scanout message (`ScanoutFlip` with the GEM handle). | The first lets any process claim any id; the second bypasses WDDM's ownership of the VidPn source. |

## 5. What the host must provide

- **H1 Blob type.** `RESOURCE_CREATE_BLOB` with `blob_mem = 0x80000001` and
  `blob_id = (rm_handle << 32) | gem_handle`, `blob_flags = 0`, guest-chosen `resource_id`:
  look up the GEM object `gem_handle` in the DRM file the backend holds for `rm_handle`
  (`device/src/nvidia`), `PRIME_HANDLE_TO_FD`, import the dma-buf into the renderer
  (`virgl_renderer_resource_import_blob`; `host/venus/src/virgl.rs` has the export side
  only) as resource `resource_id`, owned by the guest's context. Today an unknown
  `blob_mem` is refused, which the KMD reports as `ST_DEVICE_ERROR`.
- **H2 Size check.** Refuse (`RESP_ERR_INVALID_PARAMETER`) if `size` exceeds the object.
  The KMD relies on it (D4).
- **H3 Layout.** Remember the object's own layout (pitch or block-linear, log2 GOBs per
  block, from the import parameters nvidia-drm kept) and use it as the modifier at
  `SET_SCANOUT_BLOB` / flush export, instead of the size-based inference in
  `venus/scanout.rs`. The inference is wrong for heights that are a whole number of blocks
  (768, 1024).
- **H4 Lifetime.** Hold its own dma-buf reference, so the resource survives `Close` of the
  DRM file; drop it on `RESOURCE_UNREF`; drop everything on Venus reset.
- **H5 Importability.** The imported resource must be importable by Venus contexts
  (`VkImportMemoryResourceInfoMESA`) as dma-buf-typed memory, for both the bridge
  (windowed path) and the KMD's own copy (`prepare_optimal_scanout_copy`). See O1.
- **H6 Feature bit.** A config `features` bit so the KMD can open the gate only on a host
  that has H1 to H4 (the KMD reads config features once at init; `CONDUIT_CFG_*`).
- **H7 Errno.** If the host can say why it refused, echo the errno so the KMD can fill
  `out_host_errno` (not wired today; the field is reserved).

## 6. What librmclient / NVK / the UMD must do

- **librmclient (Windows transport):** a call for `HELIOS_ESCAPE_FOREIGN_RESOURCE` from the
  same D3DKMT device that holds the RM handles; the C header is
  `protocol/include/helios_foreign.h`. Probe with `QUERY_CAPS`: `STATUS_NOT_IMPLEMENTED` is an
  old KMD, `caps_flags & CAP_RM_IMPORT == 0` is a KMD or host without the path.
- **NVK, Helios WSI mode:** per swapchain image, export the RM memory
  (`OS_UNIX_EXPORT_OBJECT_TO_FD`), `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` on a DRM-node handle,
  `CTX_CREATE` a Venus context on that device (a holder context is enough), `IMPORT_RM`, then
  hand `out_resource_id` to the present vehicle. Release with `RELEASE_BLOB(ctx, resid)` when
  the swapchain image dies, unless a WDDM allocation adopted it.
- **UMD present vehicle (`umd/src/forward/vehicle.rs`):** `set_present_source` refuses
  `fence_value == 0` and `semaphore_handle == 0`, which is what the research doc's "pass 0, already
  complete" needs. It needs a mode for a CPU-complete foreign source. It also needs the image
  create info NVK used (DRM modifier) to import the resource as an image.
- **KMD later, not now:** the consumers that still trust a UMD-claimed size should use the
  host-verified size for a foreign resource (section 8).

## 7. Implemented in this change

| piece | where | tested |
|---|---|---|
| ABI structs, constants, `blob_mem`, C mirror | `protocol/src/foreign.rs`, `protocol/include/helios_foreign.h` | layout asserts (Rust `const`, C `_Static_assert`, gcc -m32/-m64), header-vs-Rust constant test |
| pure table: quotas, reservations, adoption, removal, counters, request validation, blob-id encoding | `kmd_logic/src/foreign_resource.rs` | 19 host tests (`cargo test` in `kmd_logic`) |
| table wiring, ownership checks, atomic commit | `kmd_render/src/virtio/gpu/foreign_tables.rs`, 4 hooks + 2 map refusals in `resource_tables.rs`, 1 field in `gpu/mod.rs` | type-checked and run against stubs (see below) |
| import sequence, gate, counters | `kmd_render/src/virtio/foreign.rs` | same |
| escape handler | `kmd_render/src/ddi/escape_foreign.rs`; one match arm in `ddi/escape.rs` | same |

The KMD cannot be built here, so the new KMD files were compiled and exercised against a
stub of the surrounding crate (11 glue tests: id minting and wire shape, someone else's
handle or context, malformed fields, short and misversioned buffers, quotas, host refusal
returning the reservation, handle closed mid-flight, teardown mid-flight, adopt then destroy,
map refusal, caps). Those stubs are not in the tree. Nothing here has run on a Windows guest.

Not implemented: the host half; the UMD/NVK/librmclient half; opening the gate; the
hardening items below.

## 8. Hardening TODO (security last; none blocks the first light)

- `prepare_optimal_scanout_copy` and `ScanoutTarget::from_direct_primary` /
  `from_snapshot_descriptor` bound reads by a size the UMD claims. For a foreign resource they
  should take `min(claimed, recorded size)` from the foreign record.
- `ATTACH_RESOURCE` and the snapshot descriptor take any live resid from any process. A foreign
  resource is the most valuable thing to name (it is a window into VRAM that holds another
  process's frame). Both should check that the caller created or was handed the resource.
- Stale RM handle: a handle closed by one thread while another uses it can name another
  process's file once the host reuses the number. Narrowed here by the recheck at commit, as in
  `push_nvrm_map`; closing it fully needs an in-flight count on the handle, for `FORWARD` too.
- Cross-process lifetime of an imported foreign resource that two devices of one process use:
  the importing device's DestroyDevice frees it even if the bridge's device still imports it.

## 9. Open questions

- **O1 (the big one) Venus import of a dma-buf-origin resource.** The bridge and the KMD import
  a resource as `VkDeviceMemory` + an OPTIMAL `VkImage`. `prepare_optimal_scanout_copy` uses
  `OptimalImageTransport::OpaqueFd` and creates a plain OPTIMAL image; the vehicle's import
  needs "an exact-size match" (vehicle.rs). A dma-buf-backed resource may need dma-buf handle
  type and an explicit DRM-modifier image (`VK_EXT_image_drm_format_modifier`), and the layout
  NVK chose must equal what that image expects. Needs a host spike (a test pattern through
  `SET_SCANOUT_BLOB` of an imported resid, then a Venus import of the same resid) before NVK work.
- **O2 Wire shape.** `RESOURCE_CREATE_BLOB` with a vendor `blob_mem` (D3) versus a dedicated
  message. Only `virtio/foreign.rs::import_rm` (one `ctrl::alloc_blob` call) changes if the host
  prefers another shape.
- **O3 Who knows the layout.** H3 assumes nvidia-drm keeps the import parameters on the GEM
  object. If not, `IMPORT_RM.flags` / a new field must carry pitch/block height, and the KMD
  must forward it (the 56-byte `RESOURCE_CREATE_BLOB` has no room: it would need a message).
- **O4 Producer sync.** The RM-semaphore-to-Venus-semaphore path is unspecified; v1 is the CPU wait.
- **O5 One device or two.** D6 needs the escape from the device that holds the RM handles and a
  Venus context. If NVK's RM device and the UMD bridge's device differ (likely), the bridge
  `ATTACH_RESOURCE`s the resid into its own context (allowed today) and the lifetime rule above
  applies. If that is awkward, relax step 3 to "same creator process" (needs the process in the
  NVRM handle slot).
- **O6 The vehicle's `PresentSource` for a foreign image** (section 6): who owns the change.

## Review findings, deferred (IMPORT_RM, gated off)

Found by a read-only review of the first import commit. None affects the build
or behaviour while `RM_IMPORT_SERVED` is false; fix before opening the gate.

1. A device can exceed the per-device byte quota by adopting imported resources
   into WDDM allocations and importing again, up to 512 x 1 GiB pinned on the
   host. Add a global byte cap summed over entries and reservations in
   `check_quota`, or keep charging the creator until the record is removed.
2. Between `alloc_blob` and `foreign_commit_import` the new blob is an ordinary
   owner slot, so a second thread of the same device that guesses the next
   resource id can `MAP_BLOB` it (a CPU view of foreign memory). Refuse maps on
   a mid-import blob, or record the foreign marker in the same lock hold as
   the slot.
3. `foreign.remove` runs before the host resource is torn down on the
   `release_blob_for_owner` / `release_blobs_for_owner` paths that deliberately
   leak a host resource on a drain failure, so the quota is freed while the
   host memory stays pinned. Remove the record after teardown succeeds, or
   adopt it on the leak path.
4. `out_host_errno` is documented as the host's errno on `ST_DEVICE_ERROR` but
   is always 0. Reword the field's doc or plumb the errno out.

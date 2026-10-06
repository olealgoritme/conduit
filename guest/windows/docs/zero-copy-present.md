# Zero-copy presentation of NVK-on-RM images (Windows guest, KMD side)

Status: KMD half implemented; the gate opens at runtime when the host advertises the import (v310, see "Gate opened");
host half and the NVK/UMD half do not exist yet. Written against the code at
commit `0185243` on `kmd/zero-copy-present`. **Section 10 (S3) supersedes this
document where it differs**: the layout is mandatory in `IMPORT_RM`, adoption of
a foreign resid by `D3DKMTCreateAllocation` is complete, and a present marker may
carry `value == 0`. Sections 3.1, 3.2, 5 (H3) and 9 (O1, O3) are annotated
accordingly.

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
`IMPORT_RM` (72 bytes; **S3: the request must be the 104-byte `IMPORT_RM` plus
layout, section 10.1**). The canonical C mirror is `protocol/include/helios_foreign.h`;
sizes, offsets and every constant are asserted on both sides (a Rust test parses the
header). `rm_handle` is the backend handle librmclient got from `Open` of a DRM node
(`device_type >= 512`); `gem_handle` is what `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`
returned in that file; `size` is the exported object's byte size.

### 3.2 Validation (all before any wire traffic, in this order)

1. Gate: `QUERY_CAPS.caps_flags & CAP_RM_IMPORT`, else `ST_UNSUPPORTED` and nothing happens.
2. Structure (`validate_request`, pure, host-tested): ctx, rm and gem ids nonzero, `flags`
   only the layout bit, `size` a nonzero page multiple and at most 1 GiB, **and a valid
   layout that fits `size` (S3, section 10.1)**. `ST_BAD_RANGE`.
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
  (768, 1024). **S3: the KMD now holds the layout (10.1); the 56-byte
  `RESOURCE_CREATE_BLOB` still carries none of it, so the host either keeps it from the
  GEM object or a layout-carrying message is added (10.6).**
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
- Adoption by `D3DKMTCreateAllocation` (S3, 10.2) cannot name the creating device (the DDI has no
  device handle); it is bound to the import's holder context, which a hostile process could guess.
  Closing it needs the caller's process identity at CreateAllocation (nothing in the KMD reads the
  current process today) or an adoption cookie returned by `IMPORT_RM`.
- Adoption of a resource the KMD's own RM client made (`KMD_RM` creator, `kmd-rm-client.md` 14.1) is
  checked only by `ctx_id != 0` in place of the creating-device rule (there is none): see
  `shared-foreign-surfaces.md` R5.
- Cross-process lifetime of an imported foreign resource that two devices of one process use:
  the importing device's DestroyDevice frees it even if the bridge's device still imports it.
  **S6 (`shared-foreign-surfaces.md`): after adoption the lifetime is refcounted over the opens
  of the adopting allocation (the host resource lives until the last of the allocation's
  destroy and every open's close); before adoption this item still stands.**

## 9. Open questions

- **O1 (the big one) Venus import of a dma-buf-origin resource. ANSWERED by the host spike
  (`host_import_spike.c`, branch `spike/host-nvk-import`, c6fab91): see 10.5. The text below is
  the question as asked.** The bridge and the KMD import
  a resource as `VkDeviceMemory` + an OPTIMAL `VkImage`. `prepare_optimal_scanout_copy` uses
  `OptimalImageTransport::OpaqueFd` and creates a plain OPTIMAL image; the vehicle's import
  needs "an exact-size match" (vehicle.rs). A dma-buf-backed resource may need dma-buf handle
  type and an explicit DRM-modifier image (`VK_EXT_image_drm_format_modifier`), and the layout
  NVK chose must equal what that image expects. Needs a host spike (a test pattern through
  `SET_SCANOUT_BLOB` of an imported resid, then a Venus import of the same resid) before NVK work.
- **O2 Wire shape.** `RESOURCE_CREATE_BLOB` with a vendor `blob_mem` (D3) versus a dedicated
  message. Only `virtio/foreign.rs::import_rm` (one `ctrl::alloc_blob` call) changes if the host
  prefers another shape.
- **O3 Who knows the layout. DECIDED (S3): the caller of `IMPORT_RM` sends it, mandatory (10.1).
  The question as asked:** H3 assumes nvidia-drm keeps the import parameters on the GEM
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

## 10. S3: adoption of a foreign resid and its layout (implemented, KMD half)

Written against the DXVK-on-NVK plan (`docs/dxvk-on-nvk.md` on `research/dxvk-on-nvk`,
sections 2.3, 2.4, 3.4, 3.5 level 1, 3.6, 7), stage S3. Everything here is KMD-only and still
dead code while `RM_IMPORT_SERVED` is `false`, except two things that are live regardless of
the gate and that are called out in 10.3 and 10.4.

### 10.1 The layout is part of the record (and of the request)

A foreign resource is RM memory the KMD never allocated, so its layout can only come from the
process that made it. The host spike showed that the layout must reach the importer exactly
(OPTIMAL tiling with a dma-buf or opaque fd fails; DRM format modifier with explicit plane layout
imports with 0 wrong pixels, read and write, LINEAR and block-linear h = 5, 4, 0), so the layout is
**not optional**.

`IMPORT_RM` is the 72-byte `HeliosForeignImportRm` followed by a 32-byte `HeliosForeignLayout`,
104 bytes in all (`HeliosForeignImportRmLayout`), with `flags = HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT`
(bit 0). A request without the flag (the old 72-byte form), or with the flag and a short buffer,
or with any other flag bit, or with a nonzero `reserved`, is refused: `ST_BAD_RANGE` (short buffer:
the escape fails `STATUS_BUFFER_TOO_SMALL`). The 72-byte struct is unchanged, so a client builds the
104-byte one by appending. Only the first 72 bytes are written back.

```c
/* Shared formats (shared-formats.md): the fourcc column below is the four 32 bpp RGB
   formats; with HELIOS_FOREIGN_CAP_LAYOUT_FORMATS the record also takes the other formats of that
   document's table, and flags bit 1 (FLAG_PLANE1) appends a 16-byte plane 1 (120 bytes in all). */
struct helios_foreign_layout {          /* 32 bytes, plane 0 only */
   uint32_t width;     /* 1..=16384 */
   uint32_t height;    /* 1..=16384 */
   uint32_t stride;    /* rowPitch: multiple of 4, >= width*4, <= 1 MiB */
   uint32_t offset;    /* plane 0 offset in bytes from the start of the object */
   uint32_t fourcc;    /* DRM_FORMAT_{XRGB,ARGB,XBGR,ABGR}8888 */
   uint32_t reserved;  /* 0 */
   uint64_t modifier;  /* DRM_FORMAT_MOD_LINEAR, or 0x0300000000606010 | h, h = 0..=5 */
};
struct helios_foreign_import_rm_layout { struct helios_foreign_import_rm base;   /* 72 */
                                         struct helios_foreign_layout layout; }; /* +32 = 104 */
```

Validation, pure and host-tested (`helios_kmd_logic::foreign_resource::Layout`), mirroring the
foreign-scanout validator (`kmd/foreign-scanout` 66c714f) with two deliberate differences: the extent
floor is 1 (a foreign resource is any adopted allocation, not only a mode-sized scanout image), and
the modifier set is closed to the family the host was shown to import exactly (the list NVK builds:
`0x0300000000606010` to `...6015`, plus LINEAR).

**Size rule.** `min_bytes() <= size`, never equality: `min_bytes = offset + stride * rows`, `rows`
being `height` (LINEAR) or `height` rounded up to `8 << h` (block-linear). RM rounds allocations to
64 KiB (a 1080p linear image is `0x7e9000` inside a `0x7f0000` object), so a layout-derived size is a
lower bound. The recorded `size` itself is still the host-verified object size.

The record keeps the layout (`foreign_resource::Entry::layout`) for the life of the resource, and the
KMD exposes it:

```rust
VirtioGpu::foreign_layout(resource_id) -> Option<helios_kmd_logic::foreign_resource::Layout>
// Layout { width, height, stride, offset, fourcc, modifier }, plus Layout::block_height_log2()
```

This is the accessor a KMD-driven `ScanoutFlip{stride, fourcc, modifier}` and the scanout-copy
import will read. It is valid while the resource lives; the allocation's destroy drops the record.

### 10.2 Adopting a foreign resid in `D3DKMTCreateAllocation`

`HeliosWddmAllocPrivate` (48 bytes, unchanged) must be:

| field | value |
|---|---|
| `kind` | `HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY` (1). Any other kind is refused: only DEVICE_MEMORY takes the blob's lifetime. |
| `blob_mem` | `HELIOS_BLOB_MEM_RM_EXPORT` (0x80000001). It is a declaration, checked against the record in both directions. Do NOT set `HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER` (that shape reuses `blob_mem` as a cookie; foreign allocations keep the full conservative VidMm charge). |
| `adopt_resource_id` | `out_resource_id` of `IMPORT_RM` |
| `ctx_id` | **the Venus context `IMPORT_RM` was given (the holder context)**; mismatch is refused |
| `blob_id`, `map_cache` | 0 (not used) |
| `size` | the object size (informational; the recorded size is authoritative) |
| `blob_flags` | 0 |

`HeliosWddmAllocMeta` (the 48-byte trailer at byte 48, unchanged) must **repeat the layout**:
`width`, `height`, `pitch` (= `stride`), `plane_offset` (= `offset`) each equal to the record's; a
zero is not "don't care". `venus_alloc_size` is 0 or at most the recorded size (the KMD reports the
recorded size). `dxgi_format` / `format` / `bind_flags` / `misc_flags` as for any adopted texture.

**Per-allocation private-data size must be 128 bytes** (`HELIOS_WDDM_PRIVATE_WITH_LAYOUT_BYTES`):
the new second trailer `HeliosWddmAllocLayout` lives at byte 96.

```c
struct helios_wddm_alloc_layout {       /* 32 bytes, at private data offset 96 */
   uint64_t modifier;      /* == layout.modifier */
   uint32_t magic;         /* 0x594C4648 'HFLY' */
   uint32_t version;       /* 1 */
   uint32_t fourcc;        /* == layout.fourcc */
   uint32_t stride;        /* == meta.pitch */
   uint32_t plane_offset;  /* == meta.plane_offset */
   uint32_t reserved;      /* 0 */
};
```

The creator may fill it (the KMD then requires it to equal the record's layout exactly: fourcc,
modifier, stride, offset) or leave it zero. Either way the KMD **overwrites it at create time** with
the recorded layout, so the creator (after `pfnAllocateCb` returns) and every opener (DWM, through
`OpenAllocation`; the KMD leaves bytes 48.. as written, as it does for the meta) read one
KMD-validated record. A buffer smaller than 128 bytes is refused for a foreign adoption, because an
opener would otherwise have no way to learn the layout. This is backward compatible: the KMD's meta
reader already accepts "48 bytes or more" after the prefix and ignores the excess, so a 96-byte
creator is unaffected for every ordinary allocation, and an older KMD sees a 128-byte buffer as a
96-byte one.

Adoption (`ForeignTable::adopt_for_allocation`, one device-lock hold with the slot re-ownership,
`VirtioGpu::adopt_for_allocation`):

```text
resid has no record, not declared ........ legacy Venus adoption, unchanged
resid has no record, declared ............ refused NotForeign   (dead, or a plain Venus blob)
record, not declared ..................... refused Undeclared
kind does not own the blob ............... refused NotDeviceMemory
already adopted .......................... refused AlreadyAdopted   (it would be released twice)
ctx_id != the import's ctx ............... refused ContextMismatch
that context no longer the creator's ..... refused ContextGone
blob slot no longer the creator's ........ refused SlotNotCreators
private data < 128 bytes ................. refused NoTrailerRoom
width/height/pitch/plane_offset differ ... refused GeometryMismatch
supplied trailer differs from the record . refused LayoutMismatch
venus_alloc_size claim > recorded size ... refused ClaimTooLarge
otherwise: creator quota freed, record KEPT (so MAP refusal and the teardown rules still apply),
           blob slot re-owned to the KMD (owner None), layout written back.
```

A refusal is `STATUS_INVALID_PARAMETER`, changes nothing, and is counted (`FgRefA` in the service
key, mirrored as `refused` in `QUERY_CAPS`; the first and every 64th refusal also records `FgAdRf` =
the refusal code, 1 to 11, `AdoptRefusal::code`).

**"Same device".** `DXGKARG_CREATEALLOCATION` carries no device handle, so the creating device cannot
be named by the DDI. The KMD binds the adoption to the import's **holder context**: the allocation must
name the context the resource was imported on, and that context must still belong to the importing
device. Contexts are device-owned, so presenting one is presenting something only that device was
handed. This is a consistency check, not authentication: context ids are small integers a hostile
process could guess (listed in section 8, hardening).

**Destroy and open release the host resource exactly once.** `DestroyAllocation` of the adopting
allocation (`owns_resource`) pops the blob slot and the record (`forget_allocation_blob`), then
`take_live_resource` lets exactly one caller issue `CTX_DETACH_RESOURCE` + `RESOURCE_UNREF`. A second
allocation cannot adopt the same resid (`AlreadyAdopted`), so there is never a second owner.
`OpenAllocation` creates only an `OpenAllocationContext` and never owns the resource, and the C1 gate
(`resource_is_live`) still fails an open of a dead resid. **S6 supersedes the single release: the
open is counted and the unref moves to the last of destroy and closes, see
`shared-foreign-surfaces.md` section 3.** The creator's `RELEASE_BLOB` after adoption
no longer finds the slot (it is KMD-owned) and fails harmlessly.

### 10.3 The vendor blob type cannot be minted from allocation private data (live, gate or not)

A `D3DKMTCreateAllocation` with `adopt_resource_id == 0` and `blob_mem == HELIOS_BLOB_MEM_RM_EXPORT`
used to classify as a raw HOST3D blob and forward the vendor `blob_mem` to the host verbatim, which
would have bypassed `IMPORT_RM`'s ownership proof and quota. It is now refused
(`STATUS_INVALID_PARAMETER`, counted as `FgAdRf` = 0x100). No shipping UMD sends that value.

### 10.4 Present markers: `value == 0` means "already complete" (live, gate or not)

The marker tail `(ctx_id, value, cookie)` of `HeliosPresentRefreshCmd` / `HeliosPresentPrivateData`
(and the D3D11/D3D12 `publish` of the producer escape) now has four readings
(`helios_kmd_logic::present_stream::classify_tail`):

| ctx_id | value | cookie | reading | effect |
|---|---|---|---|---|
| 0 | 0 | 0 | absent | legacy: capture the current wire watermark (unchanged; an old UMD leaves the tail zero) |
| != 0 | != 0 | != 0 | point | wait for that point on the registered stream (byte-identical to before) |
| != 0 | **0** | != 0 | **complete** | the boundary is `(stream handle, 0)`: ready as soon as the stream is live, waits on no Venus timeline |
| any other mix | | | partial | not a marker; legacy rule |

A complete marker still authenticates exactly like a point: `ctx_id`, `cookie` and the process
association must name a live registered stream (`HELIOS_ESCAPE_PRESENT_STREAM` register), else the
marker is rejected (`PRESENT_STREAM_REJECTS`) and the present falls back to the legacy watermark, which
is correct but waits. The boundary stays in the tagged namespace: it dies with its stream like every
other marker (a dead stream is never success), `scanout_boundary_ready` is true for it (`slot_ready`
with value 0), the bind worker, the fast bind and the WDDM fence all treat it as any other tagged
boundary, and nothing treats the 64-bit boundary as 0 (the tag bit is set). The two parse sites in
`DxgkDdiRender` and `ContextHandleRef::stash_present_stream_marker` no longer drop `value == 0`
(dropping it would silently turn the present into the legacy wait). It is counted: `PsMkCpl`
(`PRESENT_STREAM_MARKER_COMPLETE`). D3D12 ECL records (`HE12`) still require `value != 0`
(`HeliosD3D12SubmitCmd::is_valid`); that is a different contract (DMA completion waits on the exact
worker point) and is unchanged.

The allocation-scoped producer table (escape 0x13, `HELIOS_PRODUCER_PUBLISH`) accepts `value == 0`
as well (`producer_completion::Table::publish`): the epoch is announced and completes at once when
nothing older is pending on that allocation (no pending slot consumed), or queues as an already-done
entry behind older pending epochs (the per-allocation prefix rule: a consumer never sees epoch N+1
before N); it never reads or advances the stream's strictly-increasing writer value. The stream must
still be the calling device's live registration.

Completion of a pending entry is `value <= completed` on the stream, not `==`, and the table keeps a
per-stream watermark so a publish after its retirement completes on arrival; writer slots are freed
with their pair's last entry. See `producer-completion.md`.

What the UMD must change (not done here): `PresentStreamCorrelation::is_complete` requires
`value32 != 0` and the vehicle's `set_present_source` refuses `fence_value == 0`; both need a
"CPU-complete" mode that sends `ctx_id`, `cookie` of the NVK device's registered stream with
`value = 0`, after it has waited on the CPU for the frame's NVK timeline point.

### 10.5 What the host spike settled (host_import_spike.c, c6fab91)

- The host's NVIDIA Vulkan driver imports NVK-on-RM memory exported via nvidia-drm with the exact
  layout, both directions, every pixel checked: LINEAR (rowPitch 7680) and block-linear h = 5, 4, 0.
  Advertised modifiers for B8G8R8A8 / R8G8B8A8: `0x0300000000606015` down to `...6010`, plus LINEAR
  (the list NVK builds); an NVK 1080p swapchain image on GB202 is `0x0300000000606015` (h = 5).
- The opaque route fails (OPTIMAL tiling with a dma-buf or opaque fd: `OUT_OF_DEVICE_MEMORY`), and the
  layout would be wrong anyway. This answers O1: the import must be an explicit-modifier image.
- The host will relax the vehicle's exact-size match to `image size <= resource size`; hence the size
  rule in 10.1.

### 10.6 Follow-ups (not in this change)

1. **Scanout copy import.** `prepare_optimal_scanout_copy` (`venus/scanout.rs`) and the vehicle import
   must create the image for a foreign resource with `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` and
   `VkImageDrmFormatModifierExplicitCreateInfoEXT { drmFormatModifier = layout.modifier,
   drmFormatModifierPlaneCount = 1, pPlaneLayouts = { offset = layout.offset, rowPitch = layout.stride } }`,
   reading `VirtioGpu::foreign_layout`. It currently creates a plain OPTIMAL image and infers layout from
   the size. Not rewritten here. **Done: section 11.**
2. **Layout on the host wire.** The 56-byte `RESOURCE_CREATE_BLOB` has no room for the layout; the host
   learns it from the GEM object or from a new message. Needed before the gate opens.
3. **KMD-driven `ScanoutFlip`** in `program_vidpn_source_inner` (plan 3.6 Option B): read
   `foreign_layout` for `stride`, `fourcc`, `modifier`. **Done, behind the knob `ForeignFlip` (default off): see
   `kmd-rm-client.md` 15.18** (the arbiter's resident source under the importing device, `present_within`, the
   poison of records whose DRM file closed; the wire still names `(owner_handle, GEM)`, so a flip by resource id is a
   host change, 15.18.6).
4. **Hardening**: ownership check of the adopting process (the holder-context binding is a consistency
   check only, 10.2); `ATTACH_RESOURCE` and snapshot descriptors for a foreign resid (section 8).

### 10.7 Opening the gate

`virtio/foreign.rs::RM_IMPORT_SERVED` is the one const. Flip it to `true` (or make it read the host
feature bit) only when its documented preconditions hold: the host serves `blob_mem 0x80000001`
(H1 to H4), advertises it with a feature bit, imports with the recorded layout (10.6 item 2), and the
deferred review findings below are fixed or accepted. Nothing else changes: caps, `IMPORT_RM`,
adoption and the layout record are already written.

### 10.8 Tests

| what | where | how it was run |
|---|---|---|
| layout rules, size lower bound, request validation, adoption state machine (all 11 refusals, once-only, quota freed, record kept, claim, trailer room), refusal codes | `kmd_logic/src/foreign_resource.rs` | host `cargo test` |
| marker tail readings, the "old gate for nonzero values is unchanged" table, boundary readiness for value 0 and dead streams | `kmd_logic/src/lib.rs` (`present_marker_tail_tests`) | host `cargo test` |
| producer publish with value 0 (no pending slot, ordering behind older epochs, no writer slot, atomic failure, terminal) | `kmd_logic/src/producer_completion.rs` | host `cargo test` |
| producer completion rule (skipped values, early completion, writer reuse, 100k soak, wrap, dead stream, occupancy counters) | `kmd_logic/src/producer_completion.rs` | host `cargo test` (`producer-completion.md`) |
| ABI: `HeliosForeignLayout` 32, `HeliosForeignImportRmLayout` 104, `HeliosWddmAllocLayout` 32 at offset 96; C mirror | `protocol/src/foreign.rs`, `protocol/src/wddm.rs`, `protocol/include/helios_foreign.h` | Rust `const` asserts + `cargo test`; `gcc -m32/-m64 -Wall -Wextra -Werror` on the header |
| escape parse of the layout tail, import with layout, `foreign_layout`, `adopt_for_allocation` glue (re-ownership, quota, wrong context, dropped context, declared/record agreement, legacy path) | the real `escape_foreign.rs`, `virtio/foreign.rs`, `gpu/foreign_tables.rs` | compiled and run against a stub of the surrounding crate (not in the tree), gate flipped in the copy only |
| `create_allocation.rs`, `submit_command.rs`, `gpu/mod.rs`, `device.rs` edits | - | rustfmt parse and review only; **never compiled**. Nothing here has run on a Windows guest. |


## Gate opened (v310)

`IMPORT_RM` is served when the device's config `features` word has
`NVGPU_CFG_RM_IMPORT` (bit 13) AND `NVGPU_CFG_VENUS` (bit 10): the host sets bit 13
only when its renderer imports dma-bufs, so an older host is never sent the new blob
type. `virtio/foreign.rs::rm_import_served` reads it per call; `RM_IMPORT_ENABLED` is
the compile-time kill switch. `QUERY_CAPS` reports `CAP_RM_IMPORT` accordingly.

Wire contract (host: docs/VENUS.md "RM-export blobs"):

* `RESOURCE_CREATE_BLOB`: `blob_mem = 0x80000001`, `blob_id = rm_handle << 32 | gem`,
  `blob_flags = 0`, `nr_entries = 0`, `size` nonzero and at most the dma-buf size, the
  KMD-minted resource id, attached to `hdr.ctx_id`. The KMD's own `CTX_ATTACH_RESOURCE`
  is a no-op there. `MAP_BLOB` is always refused for this type (the KMD refuses it too).
* Errors: the usual `RESP_ERR_*`, and for this blob type only the 3 padding bytes of the
  response header carry a Linux errno (24-bit LE, 0 if none). The KMD returns it in
  `out_host_errno` and maps it (`kmd_logic::foreign_errno::classify`):
  `EBADF`/`ENOENT` -> `ST_NOT_OWNED`, `ERANGE`/`EINVAL` -> `ST_BAD_RANGE`,
  `EOPNOTSUPP` -> `ST_UNSUPPORTED`, `ENOMEM` -> `ST_NO_RESOURCES`, anything else
  (`EIO`, none) -> `ST_DEVICE_ERROR`.
* Lifetime: the host and the renderer each hold their own dma-buf, so the resource
  outlives closing the DRM node or the GEM; it is released at `RESOURCE_UNREF` or reset.
* Layout: the host records the modifier from the forwarded `GEM_IMPORT_NVKMS_MEMORY`
  (pitch -> LINEAR, block-linear -> `0x0300000000606010 | h`) and uses it for
  `SET_SCANOUT_BLOB`. The KMD's record still carries the layout NVK passes (mandatory in
  `IMPORT_RM`) for its own use.
* An importing context (the UMD bridge, DWM) must create the image with
  `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` (explicit modifier, one plane, offset 0,
  `rowPitch`), `VkExternalMemoryImageCreateInfo{DMA_BUF}`, `VkImportMemoryResourceInfoMESA`
  and a dedicated allocation. The KMD's own `prepare_optimal_scanout_copy` used to make a
  plain OPTIMAL opaque-fd image and fail for these; it now builds exactly this import for a
  foreign source (section 11). The direct flip (a foreign source scanned out with
  `ScanoutFlip`) does not use it.

## 11. The KMD copy of a foreign resource (Blt model / KMD copy)

A foreign resource that is not scanned out directly still has to reach the screen: the legacy
windowed present (the Blt / GDI / DWM-composed path) copies the source allocation into DWM's
destination, and the primary copy (`SetVidPnSourceAddress` of a non-direct primary) copies it into
the adapter's LINEAR scan-out image. Both import the source into the KMD's own Venus device. For
an ordinary UMD resource that is a plain OPTIMAL opaque-fd image and nothing below changes it. For a
foreign resource the host's NVIDIA driver accepts only the explicit-modifier import (10.5), so the
KMD now builds that.

### 11.1 What is built

For a source that adopted a foreign resource (a layout record exists, 10.1):

| step | Vulkan | from |
|---|---|---|
| image | `vkCreateImage`: `VkExternalMemoryImageCreateInfo{DMA_BUF}` -> `VkImageDrmFormatModifierExplicitCreateInfoEXT{modifier, 1 plane, {offset, rowPitch}}`, `tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`, 2D, 1 mip, 1 layer, `TRANSFER_SRC`, UNDEFINED | record: fourcc (-> format), modifier, stride (-> rowPitch), offset; extent from the layout, which must be the copied extent |
| requirements | `vkGetImageMemoryRequirements` | refused if `required > recorded size` (the undersize guard of the OPTIMAL path) |
| memory type | the first allowed DEVICE_LOCAL type (VRAM first), by the KMD device's own choice | the creator has no `vkAllocateMemory`, so there is no creator type to reuse |
| memory | `vkAllocateMemory`: `VkImportMemoryResourceInfoMESA{resource}` -> `VkMemoryDedicatedAllocateInfo{image}`, `allocationSize = required` | the resource must be attached to the KMD context first (as for any import) |
| bind | `vkBindImageMemory` offset 0 | |

Then the existing recorders run unchanged: acquire from the external family GENERAL -> GENERAL,
`vkCmdCopyImage` (or `vkCmdBlitImage` through the BGRA scratch image when the source is
`DRM_FORMAT_XBGR/ABGR8888`, whose bytes are R8G8B8A8), release. The conversion decision uses the
record's fourcc, never the allocation's DXGI format. `SIMULTANEOUS_USE`, the ring-1 submit, the
wire fence and the teardown drain are the ones the OPTIMAL path has.

Mapping (`helios_kmd_logic::foreign_copy`): `XRGB8888`/`ARGB8888` -> `B8G8R8A8_UNORM` (44),
`XBGR8888`/`ABGR8888` -> `R8G8B8A8_UNORM` (37). The modifier set is the record's: LINEAR and
`0x0300000000606010 | h`, `h <= 5`. The VkSubresourceLayout carries `size = arrayPitch = depthPitch
= 0` as the spec requires for a single-layer 2D image.

One deviation from the host spike (c6fab91) worth knowing: the spike passed the object size in
`VkSubresourceLayout.size` and allocated `mr.size`; the KMD follows the spec for the layout (size 0)
and allocates the image's own requirement, as the spike did. `allocationSize` is therefore not the
recorded size but at most it (a dedicated allocation must equal the image's requirement, and the host
relaxes its check to `image size <= resource size`, 10.5).

### 11.2 Where it plugs in

* `virtio/venus/foreign_copy.rs` (new): `ForeignSource{layout, record_size}`,
  `foreign_preflight` (no host contact), `import_foreign_source`, the counters.
* `virtio/venus/scanout.rs::prepare_optimal_scanout_copy` takes `foreign: Option<ForeignSource>`;
  `None` runs the code it always ran (the OPTIMAL steps moved into an `else` block, statement for
  statement).
* `virtio/venus/present.rs`: `OptimalPresentImageDesc::new_foreign_dma_buf`; its `foreign` field is
  part of the descriptor's identity; `import_optimal_present_image` branches first on it. The
  imported image is an ordinary `ImportedOptimalImage`, so the cache and
  `release_present_blits_for_resource` need nothing new.
* `ddi/create_allocation.rs`: `AllocationContext::foreign` (the adopted layout, for the primary
  copy) and `PresentAllocInfo::foreign` (read at OpenAllocation from the KMD-written layout trailer,
  for the Blt); `ddi/display.rs` chooses the foreign descriptor for the Present source.
* `virtio/gpu/foreign_tables.rs::foreign_record` (layout + size in one lookup).
* pure logic and tests: `kmd_logic/src/foreign_copy.rs`, the two new encoder variants
  (`ImagePNext::ExternalMemoryDrmExplicit`, `MemoryPNext::ImportResourceDedicated`) in `kmd_logic/src/lib.rs`
  (golden bytes written out field by field from `vn_encode_VkImageCreateInfo`).

Trust: the layout travels with the allocation (no lock on the present path), but the import checks it
once against the foreign table (`foreign_record`: same layout, same size) and refuses a mismatch or a
missing record (`FcStale`). A forged trailer on an ordinary allocation therefore cannot turn it into
a foreign import, and a stale one cannot import a recycled resource id.

### 11.3 The device extension, and why it is gated

The image needs `VK_EXT_image_drm_format_modifier` enabled on the KMD's `VkDevice`. The KMD's device
had only the export trio on purpose: a global enable of this extension (with `VK_KHR_image_format_list`)
in the 38th session inflated the memory requirements of ordinary shared OPTIMAL imports (undersized-import
refusals, DWM failures, Xid 31). So the `CreateDevice` ladder gained a tier 0 (export trio + the one extension,
no format list) that is tried first **only** when all three hold:

* the `ForeignCopy` knob is 1 (default 0: OFF, see below);
* the adapter is the display half;
* the host serves `IMPORT_RM` (config features `NVGPU_CFG_RM_IMPORT` and `NVGPU_CFG_VENUS`): otherwise no
  foreign resource can exist and the extension would only add the risk.

Everywhere else the ladder starts where it did (tier 1 = export trio, tier 2 = none) and the device is
byte-for-byte the previous one. A host that refuses the extension steps down to tier 1: bring-up
succeeds, `FcDevX` reads 0, `SdgDevR` holds the refusing VkResult, and a foreign source is refused at
use (`FcNoExt`). `SdgDevX` = 0 now means tier 0.

### 11.4 Knob and counters

`ForeignCopy` (REG_DWORD in the service key, default 0 = OFF, read like every knob at AddAdapter/StartDevice;
the device tier is chosen at device creation, so a change needs a device restart): 0 restores the
previous device and makes a foreign source take the ordinary OPTIMAL import (which the host refuses), for a
same-boot bisect. `FcKnob` mirrors it.

| counter | meaning |
|---|---|
| `FcDevWant` / `FcDevX` | tier 0 attempted / obtained (written at device creation) |
| `FcImp`, `FcScan`, `FcBlt` | complete imports; of which for the primary copy and for the Blt (`FcImp = FcScan + FcBlt`) |
| `FcRefuse`, `FcRefCode` | refusals the KMD decided, and the last code: 1 dimensions, 2 fourcc, 3 stride, 4 modifier, 5 layout larger than the resource, 6 no format, 7 extent differs, 8 size 0, 9 image needs more than the resource, 10 no memory type, 11 plane fault (a plane 1 on a one-plane format) |
| `FcNotRgb32` | of those, records refused as a shared format this 32 bpp one-plane copy cannot carry (code 6; `shared-formats.md`) |
| `FcHostErr` | the host refused a step (image, requirements, memory, bind) |
| `FcNoExt` | foreign source on a device without the extension |
| `FcStale` | the allocation's layout disagreed with the table, or no record |
| `FcOff` | foreign source seen with the knob at 0 |
| `FcImpSt` (stage), `FcImgVr` / `FcMemVr` (raw VkResult), `FcReq`, `FcBit`, `FcMt` | breadcrumbs: last stage, host results, requirement size / memory-type bits / chosen type |

### 11.5 Not tested

Nothing here has run. The KMD cannot be built or run here; the Venus tree was type-checked against a stub
of the surrounding crate (the new KMD files, the touched Venus files, bring-up), the `ddi/` and `adapter/`
edits were reviewed and rustfmt-parsed only, and the pure logic has host tests (`cargo test` in `kmd_logic`).
In particular unverified: that the host accepts `VkImportMemoryResourceInfoMESA` + dedicated
for a dma-buf resource as this image; that NVIDIA accepts the layout with `size = 0`; that the
extension enable does not move ordinary OPTIMAL import sizes on the production device (the reason for the
gate and the knob); that `vkCmdBlitImage` from the modifier image is supported for the R8G8B8A8 source.

### 11.5 Why it ships off (review of the foreign-copy commits)

Tier 0 puts `VK_EXT_image_drm_format_modifier` on the KMD's one production Venus
device, on every host that serves `IMPORT_RM` (which is every host from now on),
and the repo records that this very extension inflated ordinary OPTIMAL shared
import requirements (`memreq_probe.c`: 7,811,520 vs 8,773,632 bytes for a 1896x1030
image; `dxvk_device_info.cpp`: "never enable image_drm_format_modifier on DXVK
devices"). Leaving out `VK_KHR_image_format_list` buys nothing, because the KMD
never chains a format list. Tier 0 is also not spec-valid on a 1.1 device without
the list (VUID-vkCreateDevice-ppEnabledExtensionNames-01387). So `ForeignCopy`
defaults to 0 and is an opt-in for testing; with 0 the device is byte-identical to
before. Before any default-on, A/B the same workload with 0 and 1 and compare `CpReq`
and the Blt refusals (`PBImSt 0xE3`). The right end state is a SECOND Venus device
(created when the first foreign source arrives) that carries the modifier extension
while the production device stays at the export trio; it needs a device scope on the
dozen helpers keyed on the single `device_id`/`queue_id`. Also open: a persistent
refusal retries every frame (a per-resource negative cache is needed), the memory
type is chosen from the image requirement only (the dma-buf fd's own `memoryTypeBits`
are not consulted), and `vkCmdBlitImage` from a modifier image needs BLIT_SRC in
that modifier's tiling features.

## 12. Present never fails on a foreign source

Measured (win11 tester, DWM on NVK): `PBRet` went 0 to `0xC000000D` (`STATUS_INVALID_PARAMETER`) and stayed,
while DWM presented IMPORT_RM-adopted DEVICE_MEMORY swap-chain buffers (5120x1440, `MISC_PRIMARY`)
with `ForeignCopy=0`. A failed `DxgkDdiPresent` is a device error for dxgkrnl, so a surface the KMD's Venus
arms were never written for must not be able to cause one.

### 12.1 Rule

A refusal that a FOREIGN allocation causes is answered with `STATUS_SUCCESS`, counted, and the work is
skipped: a Blt leaves the destination as it was; a DMA flip arms nothing and the display keeps the previous
picture; a flip's format check is simply not enforced. An ordinary (Venus) allocation fails exactly as
before: every added statement is behind a refusal that was already a failure, and the only new work on the
success path is one atomic store at the start of the call and the arm decode.

Foreign means the KMD's own record, never the creator's word: the open identity's FOREIGN flag
(`PresentAllocInfo::foreign_identity`, from the foreign-table hit at `OpenAllocation`), or a
`foreign_record(resource_id)` hit looked up at the refusal (the table lock is taken only then). The layout
trailer is not a fact: a creator can forge it, and a forged one keeps failing. The decision is
`helios_kmd_logic::present_foreign::decide` (host tests); the arms call
`ddi::present_foreign::skip`.

What is never skipped: a null `DXGKARG_PRESENT`, `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (dxgkrnl's retry protocol), `STATUS_NO_MEMORY` and the
rest after a host copy was submitted (the wait, the mirror, the ownership release of a standard-buffer
destination cannot be unwound; they are not foreign-specific), and a foreign source that the existing arms
handle: `ForeignCopy=1` imports it as before, and the level 5 Blt fallback (`sysmem_blt`) keeps taking an RM
primary destination.

A skipped Blt finishes through the same tail as a copied one (patch references, the refresh marker in the DMA
buffer, the stream boundary), after writing a fence-0 marker so the scheduler sees a record that names no
pending work, like the level 5 fallback does.

### 12.2 Counters

All are atomics on the Present path. The first skip and every 64th reach the registry at once; the throttled
mirror (`publish_nvrm_counters`) writes the rest. No registry write per Present.

| counter | meaning |
|---|---|
| `PrFgSkip` | refusals a foreign allocation caused, answered with success |
| `PrFgWhy` | the last one: `arm << 12 \| destination_foreign << 9 \| source_foreign << 8 \| refusal` (arm 1 Blt, 2 MMIO flip, 3 DMA flip); bit 10 is the unresolved ADAPTER of reason 15 |
| `PrFgBlt`, `PrFgFlip` | the same per arm (`PrFgFlip` holds both flip contracts) |
| `PrFgHand` | DMA flips of a foreign allocation armed for `ForeignFlip`'s programming (12.6); not skips |
| `PrUnres` | Blts answered with success because the adapter, source or destination handle resolved to nothing, on EVERY transport (12.7); not counted in `PrFgSkip` (which stays "a foreign allocation caused it") |
| `PrUnrWhy` | the last one's causes: `source \| destination << 4 \| adapter_unresolved << 8`, each side 0 resolved, 1 null list slot, 2 not an open context of ours (`OaBadH`), 3 open of an older transport generation, 4 open that recorded no identity |
| `PrColFill` | `ColorFill` Blts with no source allocation, a no-op (12.7) |
| `PBRetSite` | the site id of the last non-success return of `DxgkDdiPresent` (table 12.4), written when it changes and then every 64th failure; 0 = a status no site names |

Refusal codes (`PrFgWhy`, low byte):

| code | refusal | what it replaces | effect |
|---|---|---|---|
| 1 | Blt destination handle resolves to nothing, source foreign (superseded by 15, which skips every unresolved handle; the code is kept stable and no longer produced) | `PBCpy` 0xE1 | no copy |
| 2 | unresolved DXGI format (source or destination) | `PBCpy` 0xE2 | no copy |
| 3 | source kind is not DEVICE_MEMORY | `PBCpy` 0xE6 | no copy |
| 4 | WindowedBlt snapshot does not match the source | `PBCpy` 0xE7 | no copy |
| 5 | no import descriptor (also a foreign format the import does not take) | `PBCpy` 0xE2 | no copy |
| 6 | source and destination extents differ (a 5120x1440 foreign source into a 1600x900 destination) | `PBCpy` 0xE3 | no copy |
| 7 | snapshot Blt without a stream boundary | `PBCpy` 0xE8 | no copy |
| 8 | the two-phase snapshot Blt could not be queued, or its token merged | `PBCpy` 0xE4 / 0xE5 / 0xE6 | no copy |
| 9 | the destination Present buffer cannot be taken for the write (a foreign STANDARD destination without a level 5 primary) | `PBOwn` 0xE1 | no copy |
| 10 | the host copy was refused or could not be submitted before anything was written (a foreign import the host refused, with `ForeignCopy=0` or 1) | `PBCpy` 0xE4 / 0xE5 | no copy |
| 11 | flip: unresolved DXGI format (a check only) | `PBFlip` 0xE2 | flip proceeds |
| 12 | DMA flip: the resource is not in the direct-scan-out table, `ForeignFlip` off (or registered while it is off) | `PBFlip` 0xE6 | nothing armed, previous picture kept |
| 13 | completion tail: the stream boundary cannot be merged into the DMA private data | tail return | boundary dropped, legacy retirement |
| 14 | DMA flip, `ForeignFlip` on, foreign allocation that is not registered in the table (it was full, or the allocation predates the knob) | `PBFlip` 0xE6 | nothing armed, previous picture kept |
| 15 | Blt: the adapter, source or destination resolves to nothing, unconditionally (12.7); `PrFgWhy` bits 10 / 8 / 9 name the UNRESOLVED adapter / source / destination | `PBCpy` 0xE1 | no copy, destination keeps its bytes |
| 16 | Blt with `ColorFill` and no source allocation (12.7) | `PBCpy` 0xE1 | no copy (a no-op fill) |

Roles: refusals 1, 3, 4, 11, 12 and 14 concern the source; 9 the destination; the rest either (the `source` and
`destination` bits of `PrFgWhy` say which was foreign). A flip has no destination entry.

### 12.3 Where the failing status came from (read the breadcrumbs this way)

* `PBCpy` values 0xE1 to 0xE8 are DECIMAL 225 to 232 in a registry dump: a `PBCpy` of 225 is 0xE1, the Blt
  arm's "adapter, source or destination unresolved" refusal, not a count. The legacy Blt arm writes `PBCpy`
  on every call (1 copied, 2 snapshot queued) and the level 5 arm only when it changes (3 copied, 4 skipped);
  `PBFlip` 1 is SAMPLED while its failure values are unconditional. So a `PBCpy` that "does not move" at 225
  is a failure that repeats, and the `PBs*` / `PBd*` block next to it describes an older Present.
* `PBcall`, `PBflag`, `PBcnt`, `PBalst`, `PBDma`, `PBPatch`, `PBkpsz` and the whole `PBs*` / `PBd*` block are
  SAMPLED (first call, then every 600th, or every call at `DiagLevel >= 1`): they show the last sampled
  Present, which is not the failing one. `PBcnt` is `NumSrcAllocations << 16 | NumDstAllocations` of that
  sample. The allocation list is read by its fixed slots, never by those counts.
* The early returns of `dxgkddi_present_inner` before the arms are the null argument, the MPO refusal
  (`STATUS_NOT_SUPPORTED`) and nothing else: there is no argument, flag or rect validation ahead of the Blt
  and Flip arms. Everything else that can return `STATUS_INVALID_PARAMETER` is in the arms or the tail, and is
  in table 12.4. `PBRetSite` names which one fired, so the dump no longer needs the `PBCpy` / `PBFlip` code to
  be inferred.

### 12.4 `PBRetSite` ids

| id | return |
|---|---|
| 1 | null `DXGKARG_PRESENT` |
| 2 | level 5 Blt arm without an adapter |
| 3 / 4 / 5 | Blt arm: no adapter / the source handle resolves to nothing / the destination handle resolves to nothing |
| 6 / 7 / 8 | Blt arm: unresolved format / source kind not DEVICE_MEMORY / snapshot mismatch |
| 9 / 10 / 11 | Blt arm: no import descriptor / extents differ / snapshot Blt without a boundary |
| 12 | Blt arm: the WindowedBlt token could not be merged |
| 13 / 14 / 15 | flip arm: no adapter / the source handle resolves to nothing / unresolved format |
| 16 / 17 | DMA flip: no allocation-list source / resource not in the direct-scan-out table |
| 18 | level 5 Blt arm: no allocation behind the source handle |
| 19 | completion tail: stream boundary cannot be merged |
| 20 | completion tail: the patch-location capacity or write failed (insufficient, dxgkrnl retries) |
| 21 | `FlipWithMultiPlaneOverlay` (`STATUS_NOT_SUPPORTED`) |
| 22 | Blt arm (legacy or level 5): DMA buffer or its private data too small (insufficient, retried) |
| 23 | Blt arm (legacy or level 5): patch-location capacity (insufficient, retried) |
| 24 / 25 / 26 | Blt arm: snapshot queue failed / destination Present buffer not takeable / host copy refused or not submittable |
| 27 | Blt arm, after the copy was submitted: fence wait, CPU mirror or ownership release failed |
| 28 | Blt arm: fence marker not mergeable into the DMA private data |
| 29 | completion tail: DMA buffer smaller than the refresh marker (insufficient, retried) |
| 30 | DMA flip: the flip record does not fit the DMA private data (insufficient, retried) |

Every non-success return of `dxgkddi_present_inner` and of the level 5 arm names one of these; a status that does not
come from them (none known) would read 0. A return by a callee that already named its own site (the level 5 arm's
`Err`) is passed through unchanged.

Sites 1 and 13 to 14 (null argument, flip without an adapter or source) stay failures by design. Sites 3, 4, 5
(Blt adapter, source, destination) and 18 (level 5 arm source) are skips on every transport since 12.7: they stay in
the table because the code below the skip is the failure a Blt would have returned, and `PBRetSite` still reads them
if that decision is ever wrong (it cannot be reached for a Blt today).

### 12.5 Not verified, risks

Nothing here has run on win11; the KMD cannot be built here. The pure decision has host tests
(`cargo test present_foreign` in `kmd_logic`), and the arms' code shapes (the macro defined after the
let-else bindings, `patch_capacity.take()` in a return, the shared tail as a function) were compiled in a
model crate against the real `present_foreign.rs`; display.rs itself was only rustfmt-parsed.

* A skipped DMA flip arms nothing: dxgkrnl is told the flip happened and nothing is shown. If dxgkrnl keeps the
  flip pending until a CRTC_VSYNC that carries the new address, the flip queue can stall behind it; the
  counters (`PrFgFlip` rising while the screen is frozen) would say so.
* A skipped Blt shows a stale destination. A source that always hits refusal 6 shows nothing, forever, instead
  of failing: that is the intent, but it makes `PrFgSkip` the number to watch.
* Refusal 10 repeats the host's refusal every frame (as before; the persistent-refusal negative cache of
  11.5 is still open).
* The skip of 13 drops a producer boundary: the buffer retires by the legacy rule, which can show a frame the
  producer has not finished.
* `PresentAllocInfo::foreign_identity` is new state read at the refusals; an allocation opened before the
  foreign table recorded it is only caught by the table lookup. It is purely the table's record:
  `OpenAllocation` clears a FOREIGN flag the private data carried when `foreign_open` finds no table entry
  (`NotForeign`), so a previous open's flag or a creator's forgery never counts. A legitimate foreign open
  (`Opened`) sets it, as before; a `Refused` open fails the open, as before.

### 12.6 DMA flips of a foreign allocation: handed to `ForeignFlip`

DWM on NVK flips DEVICE_MEMORY + `MISC_PRIMARY` buffers that are not `MISC_DIRECT_SCANOUT`, so they are not in the
direct-scan-out table that the DMA-buffer flip contract (interval 0) resolves its source from, and the flip used to
fail (`PBFlip` 0xE6) before `arm_dma_flip` could run. Skipping those flips outright would have hidden every
interval-0 flip from `ForeignFlip` (`docs/kmd-rm-client.md` 15.18). So:

* With `ForeignFlip` on, `CreateAllocation` registers an adopted foreign allocation (`ctx.foreign`, the KMD's own
  adoption record, independent of `ForeignCopy`) in the same table as a direct-scan-out one (`register_for_flip`).
  With the knob off, one relaxed load, nothing registered, nothing changes.
* The DMA flip arm routes with `present_foreign::flip_route(knob, in_table, direct_scanout, source facts)`:

| `ForeignFlip` | in the table | foreign (identity / table record) | direct flag | route |
|---|---|---|---|---|
| any | yes | any | yes | arm, as always (byte-identical) |
| any | yes | no | no | arm, as always |
| on | yes | yes | no | arm; counted `PrFgHand`; the deferred programming reaches `ForeignFlip`'s hook in `program_vidpn_source_inner` |
| off | yes | yes | no | skip, reason 12 (a registration from before the knob went off) |
| any | no | no | any | fail `PBFlip` 0xE6, as always (Venus) |
| off | no | yes | any | skip, reason 12 |
| on | no | yes | any | skip, reason 14 |

* What happens after the flip is armed is the MMIO route's, unchanged: `process_deferred_vidpn_source_address` calls
  `program_vidpn_source`, whose `ForeignFlip` hook takes the allocation (`FfProg`) or refuses it with a counted reason
  (`FfRef*`). A refusal there runs the Venus path of the same function (`production_linear_scanout`, the KMD copy of the
  foreign resource, which needs `ForeignCopy=1`; with it off the host refuses the import, counted, and the screen keeps
  the previous picture), exactly as for a refused MMIO flip. The Present does not wait for that decision, so a
  `ForeignFlip` refusal is NOT visible in the Present status or in `PrFg*`; `FfRef` / `FfWhy` carry it.
* With the knob on, the KMD's own level 5 sysmem primary (adopted too) is registered the same way; its DMA flips are
  then armed and answered by the level 5 arm, where they used to be skipped or refused. That is the intended route for
  them, but it is a change to read in a level 5 run.
* The MMIO flip (`pDmaBuffer == NULL`) is unchanged: it returns success and `SetVidPnSourceAddress` follows.

### 12.7 Unresolved handles and ColorFill (`PBCpy` 0xE1, sites 3 / 4 / 5 / 18): unconditional

Evidence. T1 (DWM on NVK): `PBCpy` 225 (= 0xE1) with `PBFlip` and the sampled blocks unmoved. T2 (DWM stayed on Venus,
no foreign allocation anywhere): `PBRet` 0 to `0xC000000D` and `PBCpy` 2 to 225 during a DWM restart, on its first
presents. So this refusal does not need a foreign allocation; the earlier gate (a live foreign record or `ForeignFlip`)
was wrong and is removed. A Blt whose adapter, source or destination does not resolve is a counted success on every
transport (reason 15, `PrUnres`); a `ColorFill` Blt with no source and a resolved destination is reason 16
(`PrColFill`). Both finish through `present_complete` after a fence-0 marker, like every skipped Blt, and the level 5
arm does the same for an unreadable source. Flips (`PBFlip` 0xE1, site 14) are not covered.

Trade-off, stated plainly: a Blt that cannot be resolved loses that frame's picture instead of failing the Present
(which dxgkrnl turns into a device error for DWM). A real handle bug is hidden from dxgkrnl and visible only as
`PrUnres` with `PrUnrWhy` naming the cause; read those two first when a window shows stale content.

When `present_alloc_info(adapter, h)` returns `None` (static reading of `ddi/create_allocation.rs`), in order:

1. `h` is NULL: the list slot is not part of this operation. dxgkrnl encodes an absent source or destination as a NULL
   `hDeviceSpecificAllocation` (`PresentAllocations::from_allocation_list`). The Blt shape with NO source by definition
   is `ColorFill`. This is the one cause that needs no restart or race to occur, and it is a KMD defect, not a
   dxgkrnl one: `docs/kmd-rm-client.md` 15.16 says "ColorFill ... accepted as no-ops", but the Blt arm required
   both entries and answered every `Blt | ColorFill` with 0xE1. A freshly started DWM clears its buffers with fills
   before it has composed anything, which fits "the first present(s) of a fresh DWM" and `PBCpy` 2 to 225. It is
   fixed (reason 16) because it changes only a path that always failed. It is not proven to be T2's cause: `PBcnt`
   is sampled and cannot show it, hence `PrUnrWhy`.
2. `h` is not an `OpenAllocationContext` of ours (misaligned, or the `HOPN` magic does not match; counted `OaBadH`): a
   handle this driver did not mint, or one already closed.
3. The open belongs to an older transport generation (`is_current_generation` false; counted `STALE_ALLOC_REFUSED`).
   `alloc_is_current` is false for serial 0 ("never stamped") and while no transport is up, so every open made across a
   StopDevice/StartDevice (a device restart, a TDR-style reset) is refused for good even though dxgkrnl and DWM still
   hold it: resource ids restart at 1 per generation, so serving it could name another live blob. That is intended
   safety, and a reason the presents that follow a restart can hit this refusal.
4. The open context recorded no identity: `read_alloc_identity` found neither a `HeliosWddmOpenIdentity` nor a
   `HeliosWddmAllocPrivate` with a non-zero adopt id in the open-time private data (null or under 48 bytes, or an
   allocation the KMD created without a Venus backing, or a private-data buffer dxgkrnl did not carry the create-time
   write-back into). `present` is then `None` although the handle is ours.

Causes 2 to 4 are not a KMD bug that can be fixed blind: each is the driver correctly declining to guess. Which one
T2 hit is not known; `PrUnrWhy` records it per side (codes in the counters table) so the next dump names it. Neither the
open-before-present ordering (dxgkrnl cannot reference a `hDeviceSpecificAllocation` before `DxgkDdiOpenAllocation`
returned it) nor a different handle table (the list entries are always the open handles this driver returned) is a
candidate: a handle that was never returned is cause 2.

## 13. Flip completion invariant for foreign primaries

Built, host-tested (`kmd_logic/src/flip_completion.rs`), compiled by nothing that links the WDK, run by nothing.
Branch `kmd/flip-completion`. Read 13.4 (unknowns) before believing any of it.

### 13.1 The problem

DWM on NVK presented about twice and then blocked, where a Venus DWM presents about 157 times. First found by a static
trace, then supported by T3 (320.1, `ForeignFlip` 1, 45 s): four NVK DWM frames reached the screen with no refusal
(`FfProg` 4, `FfFrames` 4, `FfSeq` 4, `FfEdges` 4, every `FfRef*` / `FfFlipFail` / `FfGaveUp` 0), then DWM stalled
behind one more flip: `FfNoRec` 1 (an allocation with no foreign record), `PrFgHand` 0 and `PrFgFlip` 0 (so the flips
were MMIO), `ScUnav` 0 to 2.

The KMD's completion model: dxgkrnl retires a queued flip when a `DXGK_INTERRUPT_CRTC_VSYNC` carries the flip's NEW
`PhysicalAddress`. The KMD's VSync (`adapter/kobj.rs`) sends `AdapterContext::last_primary_address`, and that word was
written only by a programming that BOUND the allocation (`publish_bound_primary`: a host bind, a finished copy, a level 5
or `ForeignFlip` programming). For a foreign primary every one of those can fail or be switched off, and nothing
completed the flip then:

| route | what happened | exit that published |
|---|---|---|
| MMIO flip, `ForeignFlip` 0 (default) | the worker's `program_vidpn_source_inner` reached the Venus copy; the host refuses a plain OPTIMAL import of a foreign resource (`ForeignCopy` 0); `CopyFailed` is retried four times, then `GaveUp` drops the gate | none |
| DMA flip, skipped at the Present (`PrFgWhy` reason 12 / 14) | `present_complete` and no `PresentFlipPrivate`, so `arm_dma_flip` armed nothing | none |
| `ForeignFlip` 1, refused (any `FfRef*`, the presenter `Failing` for five seconds after three failed flips) | fell to the same Venus copy | none |
| an allocation with no foreign record that the copy cannot take (T3's `FfNoRec`; the host-less shared placeholder has no resource id at all) | `NotOurs` (or `BadAlloc`, `ScRid` 0) then the same copy | none |
| extent not the mode's | permanent reject before every arm (`ScBadExt`) | none |
| `ForeignCopy` 1 and the queued copy fails on the host | the ring-1 completion DPC stored the address only on success | none |

### 13.2 The invariant and its rule

The KMD OWNS flip completion toward dxgkrnl. Whether the pixels reached the screen is a different question with its
own counters. A flip whose programming cannot bind a non-Venus allocation completes anyway by publishing its address as
a KEPT picture (`ProgrammedPrimary::kept_picture`, `AdapterContext::publish_kept_primary`): the address moves, the
screen keeps whatever it showed, no refresh or bind is requested. One atomic store, legal at any IRQL.

Which allocations (`flip_completion::classify`, from the allocation context the KMD built at create time). Honest
about provenance: whether an allocation is FOREIGN is the KMD's own adoption record; the other inputs (`width`,
`height`, `direct_scanout`, `venus_alloc_size`, the Venus image id) come from the creator's private-data trailer. A
creator can therefore make ITS OWN allocation look hollow; the only effect is that ITS flips complete as kept pictures
instead of failing, which is self-harm and reaches no other allocation:

* `Foreign`: adopted NVK-on-RM resource (`AllocationContext::foreign`, the KMD's record).
* `Hollow`: not foreign and the Venus path can never show it: no resource id (the host-less shared placeholder), or a
  non-direct allocation with no geometry (`submit_primary_scanout_copy` refuses `ctx.width != width`, and an allocation
  with no geometry is programmed at the mode's extent) or no Venus identity to import. A direct-scanout allocation with
  a resource id is never hollow: the host binds its own resource and its own failure paths stay.
* `Venus`: everything else. NOTHING CHANGES for it, on any route: `decide` answers `None` for every outcome of a Venus
  source except `Programmed` (the existing bound publication), and the host test asserts it for every contract, knob
  and outcome.

Decision table (`flip_completion::decide`; `venus_can_bind` = `ForeignCopy` on for a foreign allocation, or direct
scan-out; never for a hollow one). Foreign and Hollow follow one column; the contract only decides WHERE a kept
publication is made (the worker for MMIO and for an armed DMA flip, the Present and submit for a skipped DMA flip):

| outcome | Venus | Foreign / Hollow, `venus_can_bind` off | `venus_can_bind` on |
|---|---|---|---|
| Programmed (bound by the existing code) | Bound | Bound | Bound |
| NotOurs (`ForeignFlip` off, no record, sysmem) | n/a | Kept | None (the Venus path runs) |
| Refused (`ForeignFlip` on, any `Why`; foreign only) | n/a | Kept | None |
| CopyFailed (retryable, budget left) | None | None (gate held) | None |
| GaveUp (budget spent) | None | Kept | Kept |
| Extent | None | Kept | Kept |
| Rejected (layout, format, producer abandoned, no resource id) | None | Kept | Kept |
| Unresolved (`SetVidPnSourceAddress` handle pairs with nothing) | None | Kept | Kept |
| PresentSkip (DMA Present answered without arming; DMA only) | n/a | Kept | Kept |
| AsyncCopyFailed (ring-1 completion, host error) | None | Kept | Kept |

The T3 rows are the Hollow ones: `NotOurs` with `ForeignFlip` on and off, MMIO and DMA, are `Kept`; a resource id of 0
is `Rejected` at the worker (it never reaches the arm) and `Kept`; the DMA Present of a hollow allocation (which used to
fail `PBFlip` 0xE6) is `PresentSkip`.

### 13.3 What is built, exit by exit

* `adapter/mod.rs`, `adapter/scanout.rs`: `ProgrammedPrimary::kept_picture(address)` (doc comment: why it is legal) and
  `publish_kept_primary`. Deliberately not `publish_bound_primary`, whose lease census (`LsPub`) counts binds.
* `ddi/create_allocation.rs`: `WindowsPrimary::flip_source` and `flip_completion_info(adapter, h)`, which, unlike
  `scanout_alloc_info`, also answers for an allocation with no resource id (the placeholder whose flip must complete).
* `ddi/display.rs`, `program_vidpn_source_inner`, after the `ForeignFlip` arm: a `NotOurs` or `Refused` non-Venus source
  whose Venus path cannot bind is completed as a kept picture and returns `Programmed` (the gate lowers), skipping the
  copy that could only fail. `ForeignFlip`'s shown target is NOT dropped on a kept flip (the screen keeps its picture; the
  arm still withdraws it when its file or device goes, or when it gives up). Venus sources fall through unchanged.
* `ddi/display.rs`, both wrappers' `Err` arms: a foreign or hollow source publishes kept on `GaveUp` and on every
  permanent reject (extent, layout, format, producer abandoned, no resource id); the inline wrapper treats every refusal as
  final. Statuses returned to dxgkrnl are unchanged.
* `dxgkddi_set_vidpn_source_address`: an unpaired handle (stale transport generation, foreign, null) publishes the
  address Windows named (`FkDdi`), still returning `STATUS_INVALID_PARAMETER`.
* DMA lane: `display.rs` writes a keep record (`PresentFlipPrivate::write_keep`, its own magic `HPKP`, so `take` still
  refuses a zero allocation) for a skipped foreign flip and for the `Fail` route of a hollow allocation;
  `submit_command::arm_dma_flip` takes it (`take_keep`, one-shot) and publishes `kept_picture(physical_address)`. Atomics
  only, legal at DISPATCH. A zero physical address publishes nothing. STALE REPLAY: dxgkrnl recycles DMA private
  buffers; if `present_complete` FAILS after the record was written (patch capacity, stream boundary), the slot is
  zeroed (`PresentFlipPrivate::clear_keep`, in `display::present_flip_kept`) so a recycled buffer cannot publish an old
  address as kept for another flip. Not host-testable (raw DMA private memory); it is read, not run. The ordinary flip
  record (`HPFL`) has the same exposure and is not changed here.
* Placeholder flips (`FkPhFlip`). A host-less shared placeholder has no identity, so `present_alloc_info` is `None` and the
  flip arm failed `PBFlip` 0xE1 before any of the above. The open now records `host_less_placeholder` (no identity and
  the placeholder's shape, `shared_placeholder::identityless_open_is_placeholder`, the KMD's own test) and the flip arm
  completes such a flip: MMIO returns success (`SetVidPnSourceAddress` follows with the global handle, resource id 0,
  which the worker rejects and completes as a kept picture) and DMA writes a keep record for the allocation list's
  address. An identity-less allocation that does not have the placeholder's shape still fails 0xE1, as does every
  Venus allocation.
* `virtio/gpu/mod.rs` and the three call layers above it (`submit_prepared_image_copy`, `submit_venus_async_scanout`,
  `scanout_notify`): `ScanoutNotify::keep_on_failure`, set for a non-Venus source. The ring-1 completion DPC then stores
  the address when the copy's GPU completion FAILS (`response_ok` false), where it stored it only on success. The
  transport-latch arm is untouched (an epoch abort).
* `ForeignFlip` and the host round trip. Publication was already decoupled from the host's acknowledgement (`take`
  publishes at programming; the flip is sent later by the worker). What was coupled was the worker: it drains
  `pending_vidpn_allocation` (`hpd.rs`, before `foreign_flip::service`), so a flip waiting on a slow or silent host sat in
  front of every later publication for up to `FLIP_TIMEOUT_MS` = 1 s. The timeout is now
  `flip_completion::WORKER_FLIP_TIMEOUT_MS` = 250 ms (host-tested bounds: at least two 60 Hz frames, at most 250 ms, well
  under a second), with the retry and failure accounting untouched (a timeout is `Failed`, three in a row give up for
  five seconds, `FAIL_UNTIL`). It was first 100 ms; a tester's run showed three 100 ms host stalls withdrawing the
  `ForeignFlip` source in about 0.6 s (three strikes plus the retry pauses), so it is 250 ms. Chosen
  over a second thread (new lifetime and lock-order surface, unverifiable without hardware) and over draining between acts
  (starves the flips under a steady stream of programmings). With every refusal now completing as a kept picture, a
  spurious timeout costs a stale picture for the pause, not a held flip. The level 5 presenter's 1 s is untouched.

### 13.4 Unknowns (read before trusting this)

1. Whether dxgkrnl retires flips STRICTLY by CRTC_VSYNC address match has never been observed. The model is the driver's
   own (`viogpu3d`'s `m_sourceAddress`, `last_primary_address`'s documented contract) and the stall it explains is derived
   from reading the code, then matched to T3's counters, not seen in a trace. If dxgkrnl retires on something else (the
   DMA fence, an interval, another interrupt type), this changes nothing and the stall has another cause. The first run
   answers it: `FkKeep` moving with `SaCnt` and `VpVsN` following, and DWM's present count with it.
2. What a kept picture does to a flip chain: dxgkrnl may reuse the previous buffer of the chain once the new address is
   reported. The screen shows stale content (that is the point), but a stale picture is not a frozen compositor. If DWM
   renders into a buffer the KMD is still showing (`ForeignFlip`, the conservative reuse rule, 15.18.5) the picture may
   tear; unobserved.
3. `last_primary_address` now names an address that is not on the screen. Readers: `same_active_identity` (direct sources
   only, requires `already_bound`), the `SaLo` / `SaHi` diagnostics, the ring-1 DPC (stores its own). A later DIRECT
   Venus source with the very same physical address as a kept foreign one could read `same_active_identity` true; the
   address is a per-allocation segment address and the window is theoretical, but it is not proven impossible.
4. The `Hollow` test is derived from the code (`submit_primary_scanout_copy` refuses what it names), not from a trace. A
   real Venus allocation that is non-direct with a geometry and an identity stays `Venus` and keeps its failure paths: a
   T3-style stall behind such an allocation (a plain Venus flip with `FfNoRec` and a copy that fails) is NOT fixed by
   this, by design (Venus byte-identical). `FfNoRec` alone cannot tell the two apart; the checklist below separates them.
5. An unpaired handle (`FkDdi`) publishes at DIRQL while the DDI still returns `STATUS_INVALID_PARAMETER`. Whether
   dxgkrnl waits for a retire after a failed DDI is unknown; the publication is harmless if it does not.
6. `ScUnav` has two sources. `ScanoutReject::ProducerAbandoned` (completed here when the source is non-Venus) and the HPD
   worker's refresh arm (`ScanoutRefreshQueue::Unavailable`: a dirty edge with no bound scanout, which a `ForeignFlip` or
   kept screen legitimately has). The second is a dropped refresh, not a flip, and has no address to publish.

### 13.5 Counters

New (`Fk`, at most 14 characters, `kmd_logic::flip_completion::COUNTERS`, enforced by host tests against every other
counter name in `kmd_render`; written once a flip was completed this way, the first and every 64th at PASSIVE, the rest
by the periodic mirror):

| name | meaning |
|---|---|
| `FkKeep` | flips completed as a kept picture |
| `FkWorker`, `FkDma`, `FkAsync`, `FkDdi` | by whom: the programming worker; the DMA lane at submit; the ring-1 DPC (copy failed); `SetVidPnSourceAddress` (unpaired) |
| `FkDmaRec` | DMA Presents that wrote a keep record (the Present side of `FkDma`) |
| `FkWhy` | the last reason: 1 NotOurs, 2 Refused, 3 GaveUp, 4 Extent, 5 Rejected, 6 PresentSkip, 7 AsyncCopyFailed, 8 Unresolved |
| `FkKeep01` .. `FkKeep08` | per reason |

### 13.6 Verified, and not

Verified (host tests, `kmd_logic`): the full decision table (2 contracts x 3 sources x ForeignFlip on/off x 10 outcomes,
unreachable rows asserted unreachable); Venus rows publish nothing new for every knob; Kept never for Venus or for a
programming that bound; every terminal foreign dead end completes; the T3 rows; `classify` for each shape; the 100 ms
bound (at most 250 ms); counter names (length, uniqueness, no `Fk` literal elsewhere in `kmd_render`, exact list);
first-and-every-64th.
Type-checked: the whole `kmd_render` against the stub harness (a build script supplies the base NT types), with the
error set of the touched tree IDENTICAL to the base, and a probe confirming the harness reports an injected arity error in
`create_allocation.rs` and an unknown variant in `submit_command.rs`.

NOT verified: anything on hardware; the WDK build; that the stub harness's remaining errors (the display types it lacks)
hide nothing in the functions that use those types; the DISPATCH-level claims (read, not run); the `Hollow` rule against a
live allocation.

### 13.7 Hardware checklist, in order

Run the 1920x1080 low-rate case first, then larger. Read the values from the service key after the run; a counter that
was never written is zero.

1. Is a flip arriving at all? `DXGK_PRESENTFLAGS`: `Blt` = 0x1, `ColorFill` = 0x2, `Flip` = 0x4. A tester's "0x1 then
   0x2" is Blt and ColorFill, with NO flip in it. Check explicitly: `PBflag` (sampled: the last sampled call's flags; bit
   0x4 must appear at least once), `PBFlip` (1 = a sampled flip passed; 0xE1 / 0xE2 / 0xE4 / 0xE5 / 0xE6 are the error
   arms), `PBMmio` (1 = the MMIO contract was met), and the unsampled census in the `scanout_trace` dump: `VpPres`
   (Present calls), `VpBlt`, `VpFlip` (flip presents), `VpMmio`, `VpDmaF` / `VpDmaA` (DMA flips seen / armed), plus
   `PrFgFlip` and `PrFgBlt` (skips per arm). If DWM presents only Blt and ColorFill, no flip ever exists and none of
   this section applies.
2. Is `SetVidPnSourceAddress` called? `SaCnt` (programming entries; also `VpSA`, sampled), `VpEnt` (DDI entries), `VpPrF`
   (handles that paired with nothing), `VpDSt` (the worker's last status; 0 is success), `VpVsN` (VSync ticks), `SaLo` /
   `SaHi` (the address the VSync is reporting now).
3. What did the programming do? `ScSet` (the last programming step: 1 bound, 0xD extent, 0xE3 layout, 0xE host refused,
   0xE1 / 0xE2 / 0xE4 the others), `CpCpy` (1 submitted, 0xE1 .. 0xE4 refused), `ScCpyErr`, `ScRetry`, `ScGaveUp`,
   `ScBadExt`, `ScUnav`, `ScRid` (0 = a handle that did not resolve to a resource).
4. Which knobs? `FcKnob` (ForeignCopy), `FfKnob` (ForeignFlip), `FfNoRec` and `FfRef01`..`FfRef15`, `FfWhy`, `PrFgWhy`.
5. Is the new rule engaged? `FkKeep` > 0; split `FkWorker` / `FkDma` / `FkAsync` / `FkDdi`; `FkWhy` and `FkKeep0N` say
   which exit. Expectations: default knobs with DWM on NVK: `FkKeep` tracks `SaCnt` and `FkWhy` is 1 (MMIO) or 6 (DMA);
   `ForeignFlip` 1: `FkKeep` stays near zero while `FfProg` rises, and a T3-style `FfNoRec` flip is `FkKeep01`.
6. The effect: DWM's present count (`VpPres`, `PBcall`) growing with `VpVsN` advancing and `SaCnt` following; the
   pre-change signature is `SaCnt` stuck at 2 to 4, `ScCpyErr` rising by 4 per flip, `ScGaveUp` rising.
7. If `FkKeep` moves and DWM still blocks, unknown 1 is the answer: dxgkrnl is not retiring on the VSync address. Then
   compare `SaLo` / `SaHi` with the flip's address and look at the `DMA_COMPLETED` fence path (`WfDone`, `WtOut`) before
   suspecting this rule.
8. A Venus-only session should read `FkKeep` 0 (the `Fk*` block is written once per StartDevice as zeros, then only on
   events). A nonzero `FkKeep` there is either an allocation `classify` called hollow (read `SaSeg`, `ScSrc`, `ScWH`,
   `ScDir` for it), or the DIRQL unpaired-handle publication (`FkDdi`, `FkKeep08`): a stale-generation or foreign handle
   of an otherwise ordinary Venus session can bump it. Confirm `VpPrF` (handles that paired with nothing) is 0 on a Venus
   baseline before reading a nonzero `FkDdi` as a defect; `FkDdi` should equal the growth of `VpPrF`.
9. `ForeignFlip` 1: `FfFlipFail` and `FfGaveUp` are the timeout's cost; with the 250 ms bound a loaded host may fail more
   than with 1 s. A rise with `FkKeep02` (refused while failing) is the stale-picture window, expected for five seconds.
10. Every knob mirror in the table of 13.8 is the value in force at this StartDevice, 0 included: read `FfKnob`, `FcKnob`,
    `RmKnob`, `NvDupMode`, `DiagLvl` before trusting any block that depends on them.

### 13.8 Knob read times and their mirrors (the table), and the stale-block rule

Found on hardware: `FfKnob` = 1 stayed in the service key after the registry knob was set to 0 and the device restarted,
and the whole `Ff*` block (written only once an allocation was seen) stayed frozen at a previous run's values. Two
causes: the lazy knob readers wrote their mirror only for a NONZERO value, and event-gated counter blocks write nothing
until their first event. Rules now: (1) a knob mirror is written on EVERY read, 0 included; (2) every cached knob is
read again at StartDevice, so `reg add` + `pnputil /restart-device` applies a change; (3) a block that is written only
on events is zeroed in the service key (and its statics) once per StartDevice, and the `Ff*` and `Fk*` blocks publish a
full zero block once per generation even if nothing is ever seen.

| knob | read at | cached in | mirror (value in force) |
|---|---|---|---|
| `DiagLevel` | StartDevice (`diag::reread_level`), lazily before | static | `DiagLvl` (new) |
| `StopFlush` | each StopDevice | not cached | none (behaviour only) |
| `NvSpinUs` | StartDevice (`ctrl::reread_spin_knob`), lazily before | static (was: once per driver load) | `NvSpinUs` (new) |
| `NvDupHarden` | StartDevice (`nvrm_harden::reread_mode`), lazily before | static (was: once per driver load) | `NvDupMode` (now on every read) |
| `AllocCached`, `BindFlushMode`, `DispatchBind`, `PresentProbe`, `ForeignCopy`, `DisplayHalf`, `DirectFlipCaps`, `CrossAdaptCaps`, `BarSegFlags`, `BarSegBaseMB`, `BarSegMode`, `VidMmVramMB`, `SubSpaceWake` | AddDevice and StartDevice (`AdapterKnobs::read`, `read_at_start`) | `AdapterContext::knobs` | `AlcC`, `BndFM`, `DspBnd`, `PBPrEn`, `FcKnob`, `DspH`, `BarF`, `BarB`, `BarM` (written at every start); the others none |
| `DmaGpuFence`, `PresentWmk`, `WddmHoldMs`, `WddmHeadMs` | transport init (`VirtioGpu::init`, every StartDevice) | `VirtioGpu` fields / statics | `DmaGfEff`, `PrWmkEff`, `WdHoldEff`, `WdHeadEff` (new) |
| `FlGSyncMs` | transport init (`flush_trace::init_from_registry`) | static | `FlGSyncEff` (now written at every init; before, only once a flush-gate Render was seen) |
| `MsiVectors` | transport init (MSI plan) | local | `MsiVec` |
| `VsyncRateMhz` | StartDevice | static | `VsRate` |
| `OutputTech` | each child-capabilities query | not cached | `OutTech` |
| `FlipCapsX`, `FlipQueueN` | each QueryAdapterInfo caps query | not cached | `FlipCapV`, `FlipQueV` |
| `KmdRmClient` | StartDevice, after `retire_transport` (`rm_client::reread_knob_at_start`); `forget` resets it per transport | static | `RmKnob` (now on every read, 0 included) |
| `KmdRmSysCache` | each level 5 primary bring-up | state | `RmSysCache` (level 5 counter block) |
| `KmdRmSysPollMs` | lazily at level 5, per transport generation (`forget` resets it) | static | `RmSysPollMs` (now on every read, 0 included) |
| `ForeignFlip` | StartDevice, after `retire_transport` (via `foreign_flip::publish_counters`), lazily otherwise; `forget` resets it | static | `FfKnob` (now on every read, 0 included) |
| `FlipWdogMs`, `DeferBudget` | each StartDevice (`stall_diag::reread_knobs`, from `reread_cached_knobs`) | statics | `FlWdMsEff`, `DefBudEff` (clamped value in force, 0 included; section 14) |

Event-gated counter blocks, and what resets them: `Ff*` (`foreign_flip::forget` zeroes the counters at every
`retire_transport`, the block is published once as zeros, then on events), `Fk*` (`flip_keep::reset_for_start`, same),
`PrFg*` / `PrUnres*` / `PrColFill` (`present_foreign::reset_for_start`), `ShPh*` and `CrPrivSmall` / `CrApInvalid`
(`shared_placeholder::reset_for_start`). Not changed, and still event-gated, so a value in the service key may predate
this boot until its first event: the `Rm*` presenter and level 5 blocks (`RmKnob` itself is now fresh), `Fc*`
(`FcKnob` is fresh), `FlG*`. The `Nv*`, `Vs*` and `Sa*` counters are written by the periodic mirror without a gate.
`reset_fault_counters` already zeroes the fault set at StartDevice.
The stall-diagnosis block (`HpdLoop*`, `HpdSite*`, `ScLk*`, `Flip*`, `VsPend*`, `StartN`, `StartT`, `FlipWd*`, `FkDefBud`,
`FkVenus`; section 14) is zeroed and written once at every StartDevice (`stall_diag::start_generation`, from
`start_generation_mirrors`; `StartN` itself counts generations, so it is bumped, not zeroed).

## 14. Stall diagnosis and the opt-in flip watchdog (v322)

Built, host-tested for its pure half (`kmd_logic/src/stall_diag.rs`), type-checked against the stub harness, compiled by
nothing that links the WDK, run by nothing. Branch `kmd/stall-watchdog`. Every knob defaults to today's behaviour; with
the knobs at their defaults the only changes are new passive registry counters and a few relaxed atomic stores.

### 14.1 Why

A tester saw an unexplained desktop stall: the Venus DWM stopped presenting after a user NVK scan-out app exited. No TDR,
no crash, and the counters could not name a cause: the registry mirrors are event gated and survive restarts, and the
`Vp*` dump only runs on every 128th HPD worker wake, so a stuck worker shows a STALE dump. A read-only trace of the code
left three candidates:

1. The HPD worker, or the Venus programming, blocked inside the KMD. `retire_scanout_allocation_locked` holds the scanout
   mutex across `ctrl_fifo_barrier` and `set_scanout_blob` (each up to `SYNC_ROUNDTRIP_TIMEOUT_MS` = 30 s), and
   `with_scanout_lifecycle` waits on that mutex forever (`adapter/locks.rs`). Or a Deferred programming
   (`apply_deferred_vidpn_source_address_locked`) re-arming itself with no budget: the vsync DPC wakes the worker every
   tick while `pending_vidpn_allocation` is nonzero.
2. A Venus flip whose completion is withheld: `flip_completion::decide` deliberately answers `None` for a Venus source
   except `Programmed`, including `GaveUp`, `Rejected`, `AsyncCopyFailed`; a `GaveUp` drops the gate without publishing.
3. A host or UMD ring wait the KMD cannot see.

This section is the instrument that tells them apart, and two default-off valves.

### 14.2 What is built

Counters written by `ddi/stall_diag.rs` (at most 14 characters; the list is `kmd_logic::stall_diag::COUNTERS`, enforced by
a host test that also scans every `b"..."` literal of `kmd_render` and every quoted name of `kmd_logic` for collisions and
for truncation onto one of these). Times are interrupt time in milliseconds (wraps at 2^32; subtract with wrapping
arithmetic), the clock of `VpDmpT`, `VpVsT` and `VsCntT`.

| name | meaning |
|---|---|
| `StallT` | the time this block was last written: the "now" of every age below. A `StallT` that does not move between two reads means NOBODY is writing the block (see below) |
| `HpdLoopN`, `HpdLoopT` | HPD worker loops (wakes) and the time of the last wake |
| `HpdSite`, `HpdSiteT` | the step the worker is in or last entered (ids below) and the time it entered it. Age in the step = `StallT - HpdSiteT`. The pair is two stores: a reader may see the id of one step with the time of the next |
| `ScLkN`, `ScLkAcqT`, `ScLkRelT` | acquisitions of the scanout mutex, the time of the last acquisition and of the last release. HELD NOW when `ScLkAcqT` is later than `ScLkRelT`; its age is `StallT - ScLkAcqT`. Every holder is counted (the worker, the DDI threads, `DestroyAllocation`), which is what lets a worker that waits on the mutex be told from one that holds it |
| `FlipIss` | flips dxgkrnl issued: each `SetVidPnSourceAddress` with an argument, each DMA flip record the submit took (a flip record or a keep record) |
| `FlipPub`, `FlipPubT` | publications of a displayed address (`publish_displayed_primary`: bound or kept, any class, plus the ring-1 completion DPC's two direct stores) and the time of the last. A flip can publish more than once, so `FlipPub` can exceed `FlipIss` by a little; coalescing (`VpCoal`: dxgkrnl flipping faster than the worker drains, handles dropped) makes `FlipIss` exceed it. Healthy at quiescence: `FlipIss - FlipPub - VpCoal` about 0 |
| `VsPendN`, `VsPendMax` | consecutive vsync ticks with a pending programming (`pending_vidpn_allocation != 0` or the programming gate raised; the vsync DPC maintains it with atomics only) and the longest run this generation. 0 and a small max is a quiet pipeline |
| `StartN`, `StartT` | StartDevice generations since the image was loaded (a driver reload restarts it at 1) and the time of the last. Bumped at EVERY StartDevice: a `pnputil /restart-device` is visible as `StartN + 1` and a reset of everything below |
| `FlipWd`, `FlipWdT`, `FlipWdBig` | watchdog publications, the time of the last, and flips it could not record (an address above 2^40, never expected) |
| `FkDefBud`, `FkVenus` | `Fk` counters, written by `flip_keep.rs`, in neither `FkKeep` nor `FkWhy`: Deferred programmings that spent `DeferBudget` and published kept; Venus GaveUp / permanent-reject exits that published kept under `FlipWdogMs` |
| `FlWdMsEff`, `DefBudEff` | the knobs in force (clamped, 0 included), written at every StartDevice |

`HpdSite` ids (`kmd_logic::stall_diag::site`; owner-readable ABI, append only):

| id | step |
|---|---|
| 0 | none (the worker has not run since the reset) |
| 1 | `wait`: asleep on the wake event. Healthy, says nothing about a stall |
| 2 | `start_wait`: the prologue wait for StartDevice to return |
| 3 | `indicate_child`: `DxgkCbIndicateChildStatus` |
| 4 | `drain_used`: `drain_used_and_complete` (holds `virtio_lock`) |
| 5 | `foreign_scanout_service` |
| 6 | `foreign_fence_service` |
| 7 | `process_deferred_vidpn_source_address`, WAITING for the scanout mutex |
| 16 | the same function with the scanout mutex HELD: the programming itself (`SET_SCANOUT_BLOB`, the Venus copy, a host round trip) |
| 8 | `service_windowed_blt` |
| 9 | `rm_client::service`: the level 5 service |
| 10 | `foreign_flip::service` |
| 11 | `nvrm_publish_service`: the `Nv*` mirror |
| 12 | `dump_periodic`: the `Vp*` dump (about 120 registry writes) |
| 13 | the one-shot Present probe (a fence wait and a host map round trip) |
| 14 | `queue_active_scanout_refresh`, WAITING for the scanout mutex |
| 17 | the same with the mutex HELD |
| 15 | the worker is terminating |

Where the numbers come from, and why they survive a stuck worker. The worker's own stores (`HpdLoopN`, `HpdSite`, ...) are
atomics, written on entering each step (one clock read per step); nothing about them depends on the worker running
afterwards. They are PUBLISHED from three places. `publish_nvrm_counters` (the escape-driven `Nv*` mirror) and the pacing
snapshot (the existing periodic mirror) both run ON the worker, so a stuck worker stops them. The third does not:
`dxgkddi_escape` calls `stall_diag::publish_from_escape`, which, at most every 500 ms, writes the block on the CALLER's
thread (user mode calls an escape at PASSIVE on its own thread). So a stall dump is fresh as long as SOMETHING calls an
escape: an NVK process (every NVRM message is one), a Venus submit, the tester's own tool. If `StallT` does not move,
nothing is calling one and the worker's mirrors are not running either: every value is as old as `StallT`, and the next
step is to make one call (start any Vulkan app) and read again.

Where the hooks are: `HpdSite` / `HpdLoop*`: `ddi/hpd.rs` (every service and step) and `display.rs` /
`adapter/scanout.rs` (the two mutex-held sites). Scanout mutex: `adapter/locks.rs`, `with_scanout_lifecycle`. `FlipIss`:
the top of `dxgkddi_set_vidpn_source_address` (after the null check) and `submit_command::arm_dma_flip` (both the
flip-record and keep-record branches). `FlipPub`: `AdapterContext::publish_displayed_primary` and the two stores of the
ring-1 completion DPC (`virtio/gpu/mod.rs`). `VsPendN` and the watchdog: `adapter/kobj.rs::service_vsync_tick`, before the
delivery gate (so a disabled `ControlInterrupt` does not blind it). `StartN`: `start_generation_mirrors` (zeroes the
module, writes the zero block once, per the 13.8 rule).

### 14.3 The two knobs

Both are read at every StartDevice (`reg add` + `pnputil /restart-device` applies them, 13.8) and mirrored as `FlWdMsEff` /
`DefBudEff`.

`DeferBudget` (default 0 = unlimited). Caps the Deferred programming loop. A Deferred outcome is "wait for the producer
boundary" or "the publication is busy" or "the host SET timed out": the exact handle is re-armed and the gate stays
raised, and the vsync DPC wakes the worker again (one attempt per tick, more when completions also wake it). With a
budget, the attempt that exceeds it (the same convention as `SCANOUT_RETRY_BUDGET`: `attempts > budget`, a different
handle restarts the count, a programmed or failed primary forgets it) does what the refusal retry's `GaveUp` does:
releases the leases, publishes the flip's address KEPT (any class, Venus included, `FkDefBud`), and lowers the gate
instead of re-arming. Clamped to 16..4 000 000 when nonzero; 240 is about four seconds at 60 Hz.

Why the default is 0: the budget cannot be proven never to cut a working flow. A Deferred wait is legitimate for as long
as a producer boundary takes to retire, which is a GPU time the KMD does not bound (a heavy frame, a host that is slow
but alive). Cutting it publishes an address whose picture may still be bound a moment later, and abandons the
retry-until-ready contract that `apply_deferred_vidpn_source_address_locked` and `program_vidpn_source` rely on (the
`Timeout` arm: "releasing either would allow a newer SET to overtake an unknown host selection"). A budget of 240 is
generous for a working flow, which waits one or two frames, but "generous" is not "proven". Use it on a diagnosis run, as
the A/B that tells a Deferred livelock (the stall clears, `FkDefBud` moves) from everything else.

`FlipWdogMs` (default 0 = off). Clamped to 50..60 000 when nonzero. With it set:

* The vsync DPC keeps the address of the newest pending flip (recorded where the gate is raised:
  `set_vidpn_source_address_dirql` for the MMIO contract, `arm_dma_flip_programming` for the DMA contract) as one packed
  word. When the pending run (`VsPendN`) has gone more than `FlipWdogMs` worth of ticks (`ticks_for_ms`, rounded up) with
  NO publication since (every publication restarts the clock, so a stream of flips that each publish is progress however
  long the gate stays raised), it publishes that address with `publish_kept_primary` (one atomic store, legal at
  DISPATCH), counted `FlipWd` / `FlipWdT`. Class independent, Venus included. Never twice for the same flip (the fired
  word is remembered); a newer flip that is still stuck after the interval fires again; the watchdog's own publication
  restarts the clock. It does not lower the gate or touch the pending slot: the worker still owns the programming.
* The Venus direct exits publish kept too: `GaveUp` (the refusal-retry budget) and permanent rejects of a VENUS flip in
  the deferred wrapper, which by default complete nothing (`FkVenus`). Foreign and hollow flips are untouched (they
  already publish kept). The inline (PASSIVE DDI) wrapper is not changed: its refusal status reaches dxgkrnl directly.

The risk, plainly. A KEPT address names a picture that is NOT on the screen. dxgkrnl retires the flip on seeing it and
issues the next one, so the compositor keeps running and the screen shows a stale picture, until a flip that does program.
If the programming was slow and not stuck, the bind lands later and the screen catches up (a visible hitch, no damage). If
the address model is wrong (13.4 unknown 1: dxgkrnl may not retire strictly on the address), nothing changes. It is a
DIAGNOSTIC AND RECOVERY valve, off by default, and what it hides is the very condition it reports: read `FlipWd` and
`FkVenus` before trusting any run that had it on. The 50 ms floor exists because a flip's programming takes a few ticks by
design.

What the watchdog does NOT cover: a flip whose gate was already lowered without a publication (the pending run is 0, so
there is nothing to count): a Venus `AsyncCopyFailed` (the ring-1 DPC lowers the gate and stores nothing for a Venus
source), a `Superseded` outcome that published nothing. Those read as `VsPendN` 0 with `FlipIss` ahead of `FlipPub`
(14.5, row 2).

### 14.4 What to dump during a stall

Read the service key (`reg query HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`) TWICE, 10 s apart, and note the
uptime in milliseconds at each read (`StallT` is the driver's own clock, but a tester's note of the wall clock detects a
frozen `StallT`). A stalled system is read by what MOVES between the two reads; a value read once says little.

* Clocks and freshness: `StallT`, `VpDmpT`, `VpVsT` (and `VsCntT`), `StartN`, `StartT`.
* Pending state: `VpGate`, `VpPend`, `VpDSt` (the worker's last status, 0 = success), `VpVsEn`, `VpLpa` (and `SaLo` /
  `SaHi`: the address the vsync reports), `SaCnt`, `VpPrgN`, `VpCoal`.
* New: `HpdLoopN`, `HpdLoopT`, `HpdSite`, `HpdSiteT`, `ScLkN`, `ScLkAcqT`, `ScLkRelT`, `FlipIss`, `FlipPub`, `FlipPubT`,
  `VsPendN`, `VsPendMax`, `FlipWd`, `FlipWdT`, `FkDefBud`, `FkVenus`, `FlWdMsEff`, `DefBudEff`.
* Programming outcomes: `ScUnav`, `ScRetry`, `ScGaveUp`, `FkKeep`, `FkWhy`, `PrUnres`.
* Refresh pipeline: `RfCnt`, `RfDone`, `RfFail`, `RfUnb`, `RfWait`.
* Foreign scan-out source: `FsSupp`, `FsFDrop`, `FsPres`, `FsErr`, `FsFErr`, `FnCloseErr`.
* Host releases and rings: `RelRecv`, `RelMatch`, `RelRTimeouts`, `RngSub`, `RngCmp`, `IrqN`, `DpcN`, `NvEvErr`.

(An `RfInfl` was asked for; this tree has no counter of that name. The refresh pipeline's in-flight bit is
`scanout_flush_inflight`, not mirrored under that name; `RfCnt`, `RfDone`, `RfFail` and `RfWait` are what the refresh arm
writes.)

### 14.5 Decision table

"Moves" is between the two reads. `age(X)` is `StallT - X`. A row is a pattern, not a proof: read the counters it names
together. If `StallT` does not move, see 14.2 (nothing is writing the block).

| # | pattern | means | next |
|---|---|---|---|
| 1a | `HpdSite` 7 or 14 (waiting on the mutex), age growing; `ScLkAcqT` later than `ScLkRelT`, age growing, `ScLkN` flat; `VsPendN` growing; `VpVsN` moves | hypothesis 1: ANOTHER thread holds the scanout mutex across a host round trip (`retire_scanout_allocation_locked`: `ctrl_fifo_barrier`, `set_scanout_blob`, up to 30 s each), the worker queues on it | the age is the answer: near 30 s or 60 s is the barrier / SET timeout; `NvEvErr`, `RelRTimeouts`, `RngSub - RngCmp` say whether the host is answering |
| 1b | `HpdSite` 16 or 17 (mutex held by the worker), age growing; `ScLkN` flat; `FlipPub` flat | hypothesis 1: the worker is INSIDE the programming (a Venus copy, `SET_SCANOUT_BLOB`) waiting on the host | `IrqN` / `DpcN` flat = the host is not interrupting; moving = it answers, the wait is on a fence or producer |
| 1c | `HpdLoopN` moves fast (about the vsync rate), `HpdSite` flickers between 1 and 16, `ScLkN` moves, `VpPrgN` moves, `VpPend` nonzero or `VsPendN` growing, `FlipPub` flat, `VpDSt` 0, `ScRetry` / `ScGaveUp` flat | hypothesis 1: a DEFERRED programming retrying forever (no budget; it has no counter of its own) | set `DeferBudget` 240: `FkDefBud` moves and the stall clears = confirmed |
| 1d | `HpdSite` is another step (4, 5, 6, 9, 10, 11, 12, 13), age growing | the worker is stuck in that service, not in the programming | 4: `virtio_lock` / the used ring; 5, 6: `Fs*`, `FnCloseErr`; 9: `Rm*`; 10: `Ff*`; 12: the registry |
| 2 | `HpdSite` 1 (wait), `VsPendN` 0 with `VsPendMax` small, `VpVsN` moves; `FlipIss - FlipPub - VpCoal` at least 1 and constant; `ScGaveUp` or `ScRetry` or `ScUnav` or `VpDSt` (nonzero) changed around the stall | hypothesis 2: a flip's programming gave up, the gate dropped, nothing published, dxgkrnl waits for a retire that never comes. `VpLpa` still names the old address | `FkVenus` with `FlipWdogMs` set confirms (it publishes at the GaveUp / reject exit); `ScSet`, `ScCpyErr`, `FcKnob` say why the programming failed; `FkKeep` / `FkWhy` 0 means a Venus source (`decide` answers `None`) |
| 2b | `VsPendN` growing, `FlipPub` flat, `HpdSite` 1 (the worker is idle), `VpPend` 0, `VpGate` 1 | the gate is raised with nothing pending: a programming handed to a copy completion that never came (`CopyQueued`) or a stale gate (`ScStale`) | `FlipWdogMs` publishes at the interval: `FlipWd` moving and the stall clearing confirms; `AsDone` against `AsSub` |
| 3 | `HpdSite` 1, `HpdLoopN` flat, `VsPendN` 0, `FlipIss - FlipPub - VpCoal` about 0, `FlipIss` flat | the KMD holds nothing: everything handed to it was published and no flip arrives. dxgkrnl has issued none, so the stall is upstream | `VpPres` / `PBcall` (Present calls) flat: DWM is not presenting, blocked in user mode or on the host (a UMD ring wait). Moving with `FlipIss` flat: blocked inside dxgkrnl (a fence: `WfDone`, `WtOut`) |
| 3b | as 3 and `RngSub - RngCmp` growing, `IrqN` / `DpcN` flat | the host stopped answering a ring: the wait is invisible to the KMD by design | host side |
| 4 | `VpVsN` does not move, or `VpVsEn` 0 | the vsync heartbeat is dead or its delivery gate closed: a separate failure from all of the above | `VpVsEn`, `VsMinGap`, the timer / DPC |
| 5 | `FlipWd` moves and the compositor still blocks | `FlipWd` published, dxgkrnl did not retire on the address (13.4 unknown 1): the stall has another cause | the DMA fence path (`WfDone`, `WtOut`); do not read the knob as a fix |
| 6 | `StartN` moved | the device restarted: every block above was zeroed; compare only values written after `StartT` | |
| 7 | `StallT` frozen | nobody called an escape and the worker's mirrors are not running | start any Vulkan app (any NVRM message is an escape) and read again |

### 14.6 Verified, and not

Verified (host tests, `kmd_logic`): the vsync tick bookkeeping (pending run, maximum, saturation, the no-publication clock
restarting on every publication, idle ticks resetting it); the watchdog decision (off never fires; fires after exactly the
interval, never earlier; once per flip word; again for a newer stuck flip; a stream of publishing flips is progress; needs
a recorded flip; idle never fires); the flip word (round trip, never 0, 40-bit limit, 24-bit sequence wrap); ticks from
milliseconds (rounded up, never earlier, zero = off); the Deferred budget (0 unlimited, exactly `budget` attempts, a new
handle restarts it); the knob clamps; the site ids (dense, unique); the counter names (at most 14 characters, unique, no
collision with any other literal in `kmd_render` or quoted name in `kmd_logic`, no 14-character truncation onto one, the
writer file spells exactly the list, histogram and `Vp<hex>` ring stems excluded). Type-checked: the whole `kmd_render`
against the stub harness, the error set IDENTICAL to the base (v321), with five injected errors (one per touched file
group) all reported, so the touched code is checked and not skipped.

NOT verified: anything on hardware; the WDK build; that dxgkrnl retires a flip on the kept address (13.4 unknown 1); the
DISPATCH / DIRQL legality claims (read, not run: the new code is relaxed atomics and, in `publish_displayed_primary`, one
`KeQueryInterruptTimePrecise` per publication); the cost of one clock read per HPD worker step, two per scanout lifecycle
operation and one per publication (a scalar read, assumed small next to what each step does); that the 500 ms registry
write from an escape thread (about 20 values) is not noticeable on a hot escape path; that a Deferred wait never
legitimately exceeds a budget (why it defaults to off).

Risks: the kept address (14.3); `DeferBudget` abandons a programming whose host SET may still land; the watchdog and the
direct exits only exist while the knob is set, and a run with it set is no longer a baseline.

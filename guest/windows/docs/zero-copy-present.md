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
| `VsPowerMode`, `VsWatchdog`, `VsIdleWake`, `VsWdTimer` (15.4, 19.3) | StartDevice (`stall_diag::reread_knobs`) | static | `VsPwrEff`, `VsWdgEff`, `VsIdlEff`, `VsWdTmEff` (every start, 0 included) |
| `OutputTech` | each child-capabilities query | not cached | `OutTech` |
| `FlipCapsX` | AddDevice and StartDevice (`AdapterKnobs::read`, `read_at_start`; section 18) | `AdapterContext::knobs` | `FlipCapsXEff`, `FlipCapsXMsk`, `FlipCapsRep` (written at every start, 0 included), `FlipCapV` (each caps query) |
| `FlipQueueN` | each QueryAdapterInfo caps query | not cached | `FlipQueV` |
| `KmdRmClient` | StartDevice, after `retire_transport` (`rm_client::reread_knob_at_start`); `forget` resets it per transport | static | `RmKnob` (now on every read, 0 included) |
| `KmdRmSysCache` | each level 5 primary bring-up | state | `RmSysCache` (level 5 counter block) |
| `KmdRmSysPollMs` | lazily at level 5, per transport generation (`forget` resets it) | static | `RmSysPollMs` (now on every read, 0 included) |
| `ForeignFlip` | StartDevice, after `retire_transport` (via `foreign_flip::publish_counters`), lazily otherwise; `forget` resets it | static | `FfKnob` (now on every read, 0 included) |
| `RestSeed` (section 20.3a) | each StartDevice (`stall_diag::load_rest_seed`, from `note_start_entry`) | statics | `RestSeedEff` (every start, 0 included), `RestSeedLo`, `RestSeedHi`, `RestSeedUse` (published with `ScRest*`) |
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
| `ScLkN`, `ScLkRelN`, `ScLkAcqT`, `ScLkRelT` | acquisitions and releases of the scanout mutex, and the time of the last acquisition and of the last release. HELD NOW when `ScLkN` and `ScLkRelN` differ (counts, not the millisecond stamps: an acquire and a release in one millisecond cannot be ordered by time); its age is `StallT - ScLkAcqT`. Every holder is counted (the worker, the DDI threads, `DestroyAllocation`), which is what lets a worker that waits on the mutex be told from one that holds it |
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
| 18 | `process_deferred_vidpn_source_address` AFTER the mutex was released (the `VpDSt` registry write) |
| 19 | `queue_active_scanout_refresh` AFTER the mutex was released (the pacing snapshot, about 40 registry writes) |
| 15 | the worker is terminating |

Where the numbers come from, and why they survive a stuck worker. The worker's own stores (`HpdLoopN`, `HpdSite`, ...) are
atomics, written on entering each step (one clock read per step); nothing about them depends on the worker running
afterwards. They are PUBLISHED from three places. `publish_nvrm_counters` (the escape-driven `Nv*` mirror) and the pacing
snapshot (the existing periodic mirror) both run ON the worker, so a stuck worker stops them. The third does not:
`dxgkddi_escape` calls `stall_diag::publish_from_escape`, which writes the block on the CALLER's thread (user mode calls
an escape at PASSIVE on its own thread), but ONLY while the worker LOOKS STUCK, and at most every 500 ms. The block is
about twenty registry writes (each opens the key by path, about half a millisecond), and the `Nv*` mirror was already
moved off the escape path for a similar cost, so a healthy worker costs the escape one clock read, one load and the
loads of the test, and nothing else. "Looks stuck" is `kmd_logic::stall_diag::worker_looks_stuck`: the worker has been
in a step other than the idle wait for more than 1 s (`HpdSite` not 0, 1 or 15, age of `HpdSiteT`), or the scanout mutex
has been held for more than 1 s (`ScLkN` != `ScLkRelN`, age of `ScLkAcqT`), or a programming is pending (slot or gate)
and `HpdLoopT` is older than 2 s. Ages are on the wrapping 32-bit clock; a stamp ahead of now is age 0. So a stall dump
is fresh as long as SOMETHING calls an escape (an NVK process: every NVRM message is one; a Venus submit; the tester's
own tool) after the worker has been stuck for a second or two. If `StallT` does not move and nothing is calling an
escape, every value is as old as `StallT`: make one call (start any Vulkan app) and read again. If `StallT` does not
move although escapes ARE being called, the worker does not look stuck by the rules above (row 7).

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
handle restarts the count, and ANY other outcome of the deferred wrapper forgets it: a programmed or failed primary,
a copy queued, a superseded handle, a retryable refusal whether re-armed or given up; the count is of CONSECUTIVE Deferred
outcomes of one handle, so a later Deferred of the same handle never continues an old count; `kmd_logic::DeferState`) does what the refusal retry's `GaveUp` does:
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

* EVERY flip dxgkrnl issues is recorded as the newest flip, as one packed word of its number (the new `FlipIss`, 24 bits)
  and its address: `note_flip_issued` at the top of `SetVidPnSourceAddress` and in both DMA branches of `arm_dma_flip`,
  before the flip can be paired, raise the gate, or be completed by a direct publisher (an unpaired handle `FkDdi`, a DMA
  keep record, `ForeignFlip`). The last DONE flip number is advanced by EVERY publication (`note_published`, from
  `publish_displayed_primary` and the ring-1 DPC): when the published address is the newest recorded flip's, that flip is
  done. When the pending run (`VsPendN`) has gone more than `FlipWdogMs` worth of ticks (`ticks_for_ms`, rounded up) with
  NO publication since (every publication restarts the clock, so a stream of flips that each publish is progress however
  long the gate stays raised), and the newest recorded flip is NEWER than the last one done (wrapping 24-bit order), it
  publishes that flip's address with `publish_kept_primary` (one atomic store, legal at DISPATCH), counted `FlipWd` /
  `FlipWdT`. Class independent, Venus included. So it never publishes the same flip twice, never the address of an OLDER
  flip than one already done (a flip n stuck behind a gate while a direct publisher completes flip n+1 stays unpublished),
  and a newer flip that is still stuck after the interval fires again; the watchdog's own publication restarts the clock.
  A flip whose address the word cannot carry (zero, 40 bits or more) clears the word instead (`FlipWdBig`), so no older
  address is fired for it. It does not lower the gate or touch the pending slot: the worker still owns the programming.
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
* New: `HpdLoopN`, `HpdLoopT`, `HpdSite`, `HpdSiteT`, `ScLkN`, `ScLkRelN`, `ScLkAcqT`, `ScLkRelT`, `FlipIss`, `FlipPub`, `FlipPubT`,
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
| 1a | `HpdSite` 7 or 14 (waiting on the mutex), age growing; `ScLkN` ahead of `ScLkRelN`, `ScLkAcqT` age growing, `ScLkN` flat; `VsPendN` growing; `VpVsN` moves | hypothesis 1: ANOTHER thread holds the scanout mutex across a host round trip (`retire_scanout_allocation_locked`: `ctrl_fifo_barrier`, `set_scanout_blob`, up to 30 s each), the worker queues on it | the age is the answer: near 30 s or 60 s is the barrier / SET timeout; `NvEvErr`, `RelRTimeouts`, `RngSub - RngCmp` say whether the host is answering |
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
| 7 | `StallT` frozen | either nothing is calling an escape and the worker's mirrors are not running, or escapes are called and the worker looks healthy to `worker_looks_stuck` (asleep in `wait`, mutex free, nothing pending, or in a step for less than a second): a block written only by the escape path stays quiet while the worker is healthy | start any Vulkan app (any NVRM message is an escape), wait two seconds and read again; a stall of a few hundred milliseconds is not visible here |

### 14.6 Verified, and not

Verified (host tests, `kmd_logic`): the vsync tick bookkeeping (pending run, maximum, saturation, the no-publication clock
restarting on every publication, idle ticks resetting it); the watchdog decision (off never fires; fires after exactly the
interval, never earlier; once per flip word; again for a newer stuck flip; a stream of publishing flips is progress; needs
a recorded flip; idle never fires; never a flip older than one already done, across the 24-bit wrap too; a publication completes
the newest recorded flip only when it names its address); the flip word (round trip, never 0, 40-bit limit, 24-bit sequence wrap); ticks from
milliseconds (rounded up, never earlier, zero = off); the Deferred budget (0 unlimited, exactly `budget` attempts, a new
handle restarts it, and an outcome in between that is not a Deferred ends the count; the clear on each non-Deferred arm
is wiring in `display.rs`, read and type-checked, not host-run); the knob clamps; the site ids (dense, unique); the counter names (at most 14 characters, unique, no
collision with any other literal in `kmd_render` or quoted name in `kmd_logic`, no 14-character truncation onto one, the
writer file spells exactly the list, histogram and `Vp<hex>` ring stems excluded). Type-checked: the whole `kmd_render`
against the stub harness, the error set IDENTICAL to the base (v321), with five injected errors (one per touched file
group) all reported, so the touched code is checked and not skipped.

NOT verified: anything on hardware; the WDK build; that dxgkrnl retires a flip on the kept address (13.4 unknown 1); the
DISPATCH / DIRQL legality claims (read, not run: the new code is relaxed atomics and, in `publish_displayed_primary`, one
`KeQueryInterruptTimePrecise` per publication); the cost of one clock read per HPD worker step, two per scanout lifecycle
operation and one per publication (a scalar read, assumed small next to what each step does); the stuck test's thresholds
(1 s, 1 s, 2 s) against real stalls (a stall that is none of its three patterns, such as a worker that loops fast and never
publishes, looks healthy to it; `VsPendN` and `FlipIss - FlipPub` see that, and the worker's own mirrors still write); the
cost of the 500 ms write from an escape thread while the worker looks stuck (about 20 values); that a Deferred wait never
legitimately exceeds a budget (why it defaults to off).

Risks: the kept address (14.3); `DeferBudget` abandons a programming whose host SET may still land; the watchdog and the
direct exits only exist while the knob is set, and a run with it set is no longer a baseline.

## 15. Incident: display mode lost after a device restart (v326, hotfix v327)

### 15.1 Symptom

KMD 326.1, 5120x1440@240. The live install was fine; `pnputil /restart-device` with `ForeignFlip=0` and an NVK spin app
running was fine; the next restart with `ForeignFlip=1` left "Conduit Helios" with NO current mode. Windows' only active
path was a 1280x800 fallback on another adapter LUID, `SetDisplayConfig` with our path failed with error 87, DWM presented
nothing (black), a further restart with `ForeignFlip=0` did not help. The monitor child "Generic Monitor (Conduit)" was
present and OK. A full VM restart brought the mode back (v326, same image): so the failure is in the device-restart path
(StopDevice -> StartDevice on a process-lifetime image whose statics survive), and the Windows side kept the bad topology
until the VM restarted.

Counters after the failing start: `InitStg` 7 (the transport came up; 7 is the last stage), `StVio` 0, `PwrN` 1 / `PwrD3N` 1 /
`PwrUid` 0 (one `DxgkDdiSetPowerState` call since StartDevice: the monitor CHILD, uid 0, going to D3; no D0 and no adapter
call), `HpdN` 1, `HpdLoopN` 0, `VsTickN` 0.

### 15.2 What the v325 -> v326 diff can and cannot have done

v326 touched NOTHING of the mode-set path (`ddi/display.rs`, `ddi/vidpn.rs`, `ddi/child.rs`, the EDID and mode adoption in
`resolve_scanout_mode` are byte-identical to v325), nor `StartDevice` except the new stall-diagnosis block that
`start_generation` zeroes and writes (about 35 more registry values). It changed four things that run around a restart:

1. `DxgkDdiSetPowerState`: only the ADAPTER (uid 0xFFFFFFFF) leaving D0 quiesces the vsync heartbeat; the monitor child's D3
   no longer does (`hpd_wake::power_vsync`, `ddi/lifecycle.rs`). In v325 any non-D0 state of any uid quiesced.
2. A heartbeat watchdog (`AdapterContext::vsync_watch`, called from every HPD worker pass and from every escape) that revives
   an armed-but-silent heartbeat and, new, ARMS one that is disarmed when `ADAPTER_D0 && vsync_enabled` (the "Resume" branch,
   PASSIVE callers, so it can run on the escape thread concurrently with StartDevice and StopDevice).
3. The HPD worker's wait gains a 250 ms idle tick while the heartbeat is armed (an event-only worker became a 4 Hz one).
4. New statics (`ADAPTER_D0`, `VS_REF_AT`, `VS_TICK_AT`, the wake and dump statics, `HELD_AT`, `LAST_FLIP_AT`, ...); all but
   `VS_REF_AT` were already zeroed at StartDevice (`start_generation`, `foreign_flip::forget`, `scanout_trace::reset`).
   `HPD_INDICATE_COUNT` (`HpdN`) and `HPD_START_EDGE_TIMEOUTS` are older and were never reset: `HpdN` is a count since the
   image was loaded, not since this start.

### 15.3 Ranked hypotheses (the root cause is NOT proven; no run of the failing state exists with the v327 breadcrumbs)

1. **Windows' CCD database kept a topology chosen in a bad first mode set, and the restarts replayed it** (most consistent
   with "a full VM restart fixes it, three restarts did not"). The KMD's part would be whatever made that first mode set
   fail. Evidence for: v326 does not alter any DDI that builds or validates the VidPn; error 87 from `SetDisplayConfig`
   with the persisted path means the persisted source/target mode was not in the cofunctional set we enumerate right then,
   or the target was not usable; the monitor child itself is fine. Counters: `PwrN` 1 with uid 0 D3 says Windows powered the
   monitor child down after start (no active path to power). Not supported or refuted: `InitStg` 7 / `StVio` 0 (the
   transport and the host mode were fine).
2. **The heartbeat did not run after the restart, and v326's new behaviour is what left it so** (`VsTickN` 0). With
   `display_half` the heartbeat is armed by `start_vsync` inside StartDevice and the worker's wait then has the 250 ms
   tick, so `HpdLoopN` would be at least 1 within a second; `HpdLoopN` 0 AND `VsTickN` 0 together mean either the heartbeat
   was never armed (or was disarmed before the worker's first wait) or the worker never reached its first wait. Paths in
   v326 that disarm or leave it disarmed: the watchdog's Resume branch racing StartDevice (it can arm from the escape thread
   after `vsync_enabled = 1` and before `arm_vsync`, which is harmless) and the adapter-D3 / child-D3 split (`PwrD3N` 1,
   uid 0: v326 LEAVES the heartbeat running on the child's D3, v325 quiesced it). Honest status: reading the code, a
   deterministic disarm after StartDevice was not found; this hypothesis rests on `HpdLoopN` 0 + `VsTickN` 0 and needs
   `VsArmN`, `VsDisN`, `VsCanN`, `VsEarlyN`, `VsExhN`, `VsRevN`, `StartN`, `HpdSite`, `HpdSiteT` (not in the report).
3. **The worker never completed its first pass** (`HpdN` 1, `HpdLoopN` 0). `HpdN` is incremented AFTER `DxgkCbIndicateChildStatus`
   returns and is NOT reset per generation: with the image kept across restarts, 1 means no worker after the first start has
   returned from an indication (a worker stuck in the first indication, or `dxgkrnl_opt()` None, or the image was reloaded).
   If dxgkrnl blocked the indication (it holds the VidPn / child lock while it commits the topology with the child at D3),
   neither the worker nor the mode would move. v326 did not change the indication, but it did change what the worker does
   around it (the `retire_wanted` swap and `vsync_watch` now run at the top of every pass, before the first drain).
4. **A stale static or knob inherited from the previous generation** (the coordinator's list: `ADAPTER_D0`, `VS_REF_AT`, held
   wake, repeat gate, dump pacing). Read against the code, every one of these is zeroed at StartDevice except `VS_REF_AT`
   (now also zeroed) and `HpdN` (now zeroed). `ADAPTER_D0` is set to 1 in `start_generation`, which runs before
   `start_vsync`, so a D3-at-stop cannot make the Resume branch refuse to arm. No v326 value is written to the service key
   and read back as a knob (the `FfRepeatMs` gate is read from the key but only ever written by the operator). Least likely.
5. **`ForeignFlip=1` at StartDevice.** The only things it changes at start are `read_knob` (window and repeat gate) in
   `start_generation_mirrors` and the arm's registration; `forget()` now also drops a held repeat, which is atomics only. No
   interaction with the mode set was found. It is the trigger the tester had, not a cause that code reading supports.

Which counters support which: 1 `PwrN`/`PwrUid` (child D3), `InitStg`, `StVio`; 2 `VsTickN`, `HpdLoopN`, `PwrD3N`; 3 `HpdN`,
`HpdLoopN`; 4 none (the dump shows them at zero); 5 none.

### 15.4 The hotfix (v327): the v326 behaviour changes behind knobs that default to v325

All read at every StartDevice from the service key (DWORD), mirrored with the value in force:

| knob | default | meaning | mirror |
| --- | --- | --- | --- |
| `VsPowerMode` | 0 | 0 = v325: ANY non-D0 `DxgkDdiSetPowerState` of ANY uid quiesces the heartbeat; 1 = v326: adapter only | `VsPwrEff` |
| `VsWatchdog` | 0 | 0 = off (v325); 1 = revive an armed-but-silent heartbeat; 2 = also re-arm a quiesced one while the adapter is in D0 (v326) | `VsWdgEff` |
| `VsIdleWake` | 0 | 1 = the worker wakes 4 times a second while the heartbeat is armed (v326); needs `VsWatchdog` above 0 | `VsIdlEff` |
| `VsWdTimer` | 1 | 1 = the independent 250 ms watchdog timer (19.3) runs; 0 = off (KMD 328). Re-arms an ARMED silent heartbeat whatever `VsWatchdog` says; never resurrects a quiesced one | `VsWdTmEff` |

With the defaults the vsync/power/wait behaviour is v325's: the HPD worker waits exactly as it did (infinite when nothing is
due), nothing arms the heartbeat but StartDevice and a D0 call, and a child's D3 stops it. Kept from v326 because they are
not on the restart/mode-set path and not implicated: `FfRepeatMs` gating, the dump pacing, the stale held wake fix, the
signal-by-cause counters, the coalesced windowed-Blt signal, the new counters. Statics: `VS_REF_AT` and `HpdN` /
`HpdStTo` are zeroed at StartDevice (`HpdN` is now per generation).

Breadcrumbs added so the next failure names its cause (service key, written at StartDevice and mirrored with the rest):
`EntD0`, `EntRef`, `EntArm`, `EntVsEn`, `EntHpdTh`, `EntHpdN`, `EntVsTk` (what the PREVIOUS generation left in the statics and
on the adapter, taken at StartDevice entry before anything is zeroed); `HpdPhase` / `HpdPhaseT` / `HpdFirstT` (worker phase:
1 thread entered, 2 StartDevice's return seen, 3 first indication returned, 4 first loop reached; time of the first loop);
`ModeStg` / `ModeStgT` / `ModeN` / `ModeSt` (the last mode-set DDI: 1 QueryChildRelations, 2 QueryChildStatus, 3
IsSupportedVidPn, 4 RecommendFunctionalVidPn, 5 EnumVidPnCofuncModality (with its status), 7 CommitVidPn entered, 8
CommitVidPn returned (with its status); how many entered). Existing: `StartStg` (4 = StartDevice returned), `InitStg`.
The pure decisions are `hpd_wake::power_vsync_mode`, `vsync_watch_level`, `idle_watch`, `clamp_*` (host-tested).

### 15.5 Recovery when the mode is lost

1. Full VM restart (the one thing known to work); if the mode is still wrong after it, continue.
2. Windows Display settings / `Win+P`: choose "PC screen only", then "Extend" (or `displayswitch.exe /internal` then
   `/extend`), then pick 5120x1440@240 in Advanced display.
3. Remove the display adapter and rescan (keeps the driver package): `pnputil /remove-device <instance id of Conduit Helios>`
   then `pnputil /scan-devices`; or Device Manager, uninstall the device WITHOUT ticking "delete the driver", then Scan for
   hardware changes. Then reboot.
4. Reset the CCD database: export `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration` and
   `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Connectivity` (`reg export`), then delete the subkeys of
   `Configuration` and of `Connectivity` (keep the keys themselves; the subkeys are named by monitor / adapter IDs, such as
   `CND...` and `...` entries) and reboot. Windows recreates them from the EDID and the driver's mode set at boot. Only the
   Conduit/Helios subkeys need to go if other displays matter (they name the Helios monitor id and the adapter LUID).
5. If the mode is lost again with the v327 breadcrumbs: dump the whole service key before touching anything and read
   `EntD0`..`EntVsTk`, `HpdPhase`, `ModeStg`/`ModeSt`, `VsArmN`/`VsDisN`/`VsEarlyN`, `StartN`, `StartStg`, `DspMd`, `VpCM`.

### 15.6 Regression checklist (the acceptance test of v327)

Restart the device 5 times, alternating `ForeignFlip` 0 / 1 (`reg add` the knob, `pnputil /restart-device`), and after EACH:
the mode is 5120x1440@240 (Display settings and `SetDisplayConfig` query), DWM presents, `StartN` moved by 1, `HpdPhase`
is 4 within a second, `HpdLoopN` rises on a desktop that changes, `VsTickN` rises (the heartbeat runs), `HpdN` is 1 (per
generation now). Run it with the NVK spin app running in at least two of the five, and once with `VsPowerMode` 1 and
`VsWatchdog` 2 to see whether the v326 behaviour is what breaks it (it should be run last, after the defaults pass).

## 16. Adapter-wide device removed (v326.1 incident; instrument added after v327)

### 16.1 Symptom

KMD 326.1 (`VsWatchdog` on). The counters below were read about five minutes after the event. Between about 15:30:50 and 15:31:07 VM time EVERY
live D3D device on the adapter, Venus and NVK processes alike, got `D3DDDIERR_DEVICEREMOVED` (0x88760870). DWM logged
"Evict FAILED" eleven times on its primaries, then DestroyDevice, a failed CreateDevice and its exit (Windows restarted it).
No System 4101, no dump, `StartN` 1 (no device restart), nothing on the host (no Xid, no backend WARN/ERROR; one Venus
context with fence latencies up to 650 ms). Just before: an NVK explorer logged "NVK present: frame wait timed out,
presenting anyway", and another process was restarting explorer (NVK to Venus): process teardown plus device creation at that
moment. Counters at 15:36:17 (uptime 1209 s): `PgSe` 2, `PgSc` 2, `PgTo` 3, `PgFn` 8, `PgUn` 8, `PgMr` 1303, `PgMc` 1, `PgDn`
191, `PgDi` 630, every other `Pg*` failure counter 0; `HpdPassMaxUs` 2 408 319, `HpdBusyUs` 14.5 s, `HpdDumpUs` 1.45 s,
`VsGapMaxMs` 5825, `VsRevN` 4, `VsPendMax` 578, `VsArmN` 1, `ScLkN` = `ScLkRelN` 125693, `HpdSite` 19.

### 16.2 What the code says (read, not measured)

**Statuses.** Every DDI the OS calls at run time answers `STATUS_SUCCESS` by construction. The non-success returns that exist are
argument checks (`STATUS_INVALID_PARAMETER` on a null pointer or a bad escape header) and capability refusals; none of them is
reachable from a running adapter with valid arguments. Specifically:

* `DxgkDdiBuildPagingBuffer` (`ddi/build_paging_buffer.rs`, `build_paging_buffer_inner`, about line 1826): the only non-success
  return is `STATUS_INVALID_PARAMETER` for a null adapter or null args. Every failed content operation (`PagingOpOutcome::Failed`)
  is answered through `paging_failure()` (line 173), which is `STATUS_SUCCESS`, with `PgSkipV` and a per-reason counter; the
  compile-time assert at line 176 ties that to `helios_kmd_logic::paging::is_legal_status`. `PgSkipV` 0 in the event means no
  content operation was skipped, so no eviction was refused.
* `PgSe` is NOT an exception counter. It is `BAR_SYSTEM_BACKING_ERRORS` ("system backing errors"): a failed or refused lease on
  the system pages of an eviction (`remember_system_backing`, line 880; the lease and record steps in `bar_virtual_transfer_inner`,
  about lines 1105 to 1190; `bar_transfer`, about line 1323 and 1404), or a Present mirror that could not take the content mutex
  or copy (`mirror_present_system_backing`, line 908). Every one of those sites is "the copy is complete or skipped, only the
  record that lets Present keep mirroring into the system copy is not" and the DDI still answers success. There is no `__try`
  anywhere in the paging path: the only SEH in the driver is `seh_shim.c` (user-mode blob mapping, `helios_lock_*`), none of it
  reachable from `BuildPagingBuffer`. The KMD panic handler is `KeBugCheck` (`wdk-panic`), so a Rust panic is a bugcheck, not a hang.
  `PgSe` 2 with `PgSc` 2 (captures) and `PgTo` 3 (blob to system copies) says: three evictions of a BAR allocation happened in
  the whole 20 minutes, two of them kept a lease and two lease/record steps failed (one eviction can account for both). It does
  not say an eviction failed. `PgDn` 191 are discards, `PgDi` 630 are paging operations on device-local allocations that are
  host-owned and correctly not ours (`NotOurs`).
* `DxgkDdiSubmitCommand`, `SubmitCommandVirtual` (`ddi/submit_command.rs`, 1420 and 1352 area): `STATUS_SUCCESS` always after the
  null checks. `PreemptCommand` (1592): `STATUS_DEVICE_NOT_READY` only when `adapter.dxgkrnl()` is `Err` (never after start).
  `ResetFromTimeout` (1623), `RestartFromTimeout`, `ResetEngine`, `QueryEngineStatus` (`ddi/scheduler.rs`): success.
* Device/context/process DDIs (`device.rs`): success, except the null-argument checks. `DestroyDevice` (387) ALWAYS returns
  success, but it is where the time goes (see 16.4, H3).
* Power: `DxgkDdiSetPowerState` (`ddi/lifecycle.rs`, 863) answers success for every state. Only the adapter leaving D0
  quiesces the heartbeat under `VsPowerMode` 1 (v326); `VsArmN` 1 says there was no quiesce-and-resume (resume goes through
  `arm_vsync`, which counts) so the adapter did not go D3 and back in this generation.
* Callbacks: the DMA completion notify (`notify_at_dirql`, `ddi/submit_command.rs` 802) can fail; the fence is put back (the
  `DMA_NOTIFY_FAILS` retry path), the DDI still answers success. `DxgkCbIndicateChildStatus` (`ddi/hpd.rs` 43) result was kept
  only as 16 bits in `HpdI`.
* "Fatal" latches: there is no latch that makes DDIs fail. A latched ring (`transport_failed`, the v323 30 s ring budget) makes
  host round trips fail (`VirtioError`), which the DDIs absorb (`PgEm`, `PgTxG`, `RfFail`...); nothing reports device loss to
  dxgkrnl by status, by `DxgkCbIndicateChildStatus` or by an interrupt type. The KMD never calls `DxgkCbSetPowerComponent...`,
  `DxgkCbQueryVidPnInterface` outside the VidPn DDIs, and signals only `DMA_COMPLETED`, `DMA_PREEMPTED` and `CRTC_VSYNC`.

So by construction the KMD cannot tell dxgkrnl "the device is lost". What can make dxgkrnl declare it with no 4101 is therefore
indirect: dxgkrnl or VidMm/VidSch deciding from TIME (a fence, a paging operation or a DDI that did not return in time), from a
status the KMD does not return today (excluded by reading, and now watched by `LostN`), or from the OS (PnP, session, display
power, TDR with logging not reaching the log). The instrument added in this change is aimed at the first.

### 16.3 "Evict FAILED" on DWM's primaries

DWM's eviction is a D3DKMT call (`D3DKMTEvict` / `EvictResources`) on its own allocations. A failure there with every device
already removed is a CONSEQUENCE (the call fails on a lost device), not evidence of the cause; DWM evicts its primaries when
the display is powered down, the session is disconnected or locked, or under memory pressure. The KMD's part in an eviction is
`BuildPagingBuffer` `TRANSFER` (BAR to system) or `VIRTUAL_TRANSFER` `LOCAL_TO_SYSTEM`, and only for allocations that are
`bar_eligible`; a scanout-source primary that is a Venus or foreign (adopted) resource, a host-less placeholder (resource id 0)
or a device-local image is `NotOurs` and answers success at once (`PgDi`). For a BAR allocation whose host resource was retired
(owner death at DestroyDevice before the blob sweep), `paging_alloc_info` refuses a stale or dead handle (`PgStale`, `PgEh`,
`PgFh`) and the arm answers `Failed` through `paging_failure()`: success plus the invalid mark (`PgInv`), never a failure
status. All of `PgStale`, `PgEh`, `PgInv`, `PgSkipV` were 0 in the event. `PgTo` 3 over 20 minutes also says VidMm was not under
eviction pressure.

The one way the paging path can hurt VidMm is TIME. It runs `serialize()` (a sleeping mutex, `adapter.system_backings`, the same
one a Present mirror and a teardown take: line 1949) and, for a BAR allocation, `map_blob_prepare` (a `RESOURCE_MAP_BLOB`
round trip on the control queue, up to `SYNC_ROUNDTRIP_TIMEOUT_MS` 30 s, one retry on timeout; `with_blob_bytes`, line 729).
A paging operation that waited behind a 30 s class stall holds VidMm's paging thread for that long; `PgMtxMaxUs`, `PgLongUs`
and `PgLastUs` now say whether that happened (16.5). With `PgTo` 3 and `PgTi` small, only three operations in the whole run
could have, so H3 below rests on the other DDIs.

### 16.4 Ranked hypotheses

Ranked by what the code and the counters of the event support; none is proven. "Read" says which counters decide it.

1. **H1: a silent stall of the whole guest (host steal, a vCPU parked, a CPU at DISPATCH) of seconds, which dxgkrnl's
   scheduler timeout turned into a TDR-class recovery (preempt, ResetFromTimeout, every device `HUNG`/`REMOVED`), the
   System log not showing 4101.** For: `VsGapMaxMs` 5825 is a silence of the vsync one-shot, and that tick runs at DISPATCH
   (`adapter/kobj.rs` `service_vsync_tick`, 601) taking no PASSIVE lock, so a PASSIVE mutex held across a host round trip
   CANNOT make it late: only a CPU not running timers can (a spinning CPU at DISPATCH on `virtio_lock` / `wddm_notify_lock`,
   an ISR or DPC that does not end, or a vCPU the hypervisor did not schedule); `HpdPassMaxUs` 2.4 s and `VsRevN` 4 (a
   revive means 250 ms of silence, the watchdog cannot tell late from dead) fit one cause; `StartN` 1 and no host error fit a
   guest-side timing event; "frame wait timed out" in an NVK process and 650 ms Venus fence latencies are the same slowness
   seen from above. Against: no 4101 (a logged recovery is the usual trace), no TDR counter existed to confirm.
   Read: `NPreempt`, `NResetTmo`, `NRestartTmo`, `NResetEng` (calls of the TDR DDIs; nonzero is the proof of a
   TDR-class recovery), `TResetTmo`/`TPreempt` (when), `AbnDrop` (fences dropped; `scanout.rs` mirror), `DdiFailN` 0; `VsGapFlg`
   bit 2 set with no mutex held, `VsGapInfl` 0, `HpdLongUs` small while `VsGapMaxMs` is large = the timer/CPU, not the driver.
2. **H2: a DDI or the HPD worker held inside the KMD for seconds under the Venus / scanout / content mutex during the explorer
   teardown, so VidMm/VidSch waited on it.** For: `DestroyDevice` (`device.rs` 387) runs the owner sweeps
   `release_blobs_for_owner` (476), `destroy_contexts_for_owner` (478) and `nvrm::close_all_for_owner` (482), each a host round
   trip of up to 30 s (`ctrl.rs` 100) under `venus_mutex`, whose wait is infinite (`adapter/locks.rs` 239, `KeWaitForSingleObject`
   with a NULL timeout) for every other taker (`with_venus_client`, 319; the worker's `DEFERRED_VIDPN`, `REFRESH` and
   `FOREIGN_FLIP` steps; Present; DestroyAllocation through the scanout mutex); the event was exactly an explorer teardown plus
   creation. A worker pass of 2.4 s (`HpdPassMaxUs`) fits a step waiting on that mutex. Against: it cannot make the vsync tick
   late (see H1), VidMm has no per-DDI watchdog (it times fences, not calls), and `ScLkN` = `ScLkRelN` says the scanout mutex
   was FREE when read; `HpdSite` 19 is `REFRESH_POST` (`stall_diag::site`), which is the step that WRITES this block
   (`pacing_snapshot` calls `publish_counters`, `adapter/scanout.rs` 688), so it is where the worker is when IT prints, not a
   stuck site; if the escape thread wrote the block instead (it only does while the worker looks stuck: a step older than
   1 s), `StallT - HpdSiteT` above 1000 ms would instead mean the worker sat in those 100 odd synchronous registry writes, i.e.
   a registry stall (an explorer restart writes the registry heavily) and not a host round trip: read both before choosing.
   Read: `DdiOldId` / `DdiOldMs` (the DDI inside longest right now), `Dz*` ring and `DdiSlowN`, `DdiLongMs`/`DdiLongId`,
   `VnLkHoldMs` / `VnLkWaitMs` / `VnLkHeldMs`, `ScLkHoldMs`, `HpdLongSite` / `HpdLongUs`, `HpdStep100N`.
3. **H3: the heartbeat/worker race of v326 (the v327 knobs).** `VsWatchdog` on (v326): `vsync_watch` re-arms from the worker AND
   from every escape; `note_vsync_revived` re-bases the reference with one compare-exchange so only one caller sets the one-shot,
   and `set_vsync_one_shot` on an armed Ex timer replaces the expiry. No path that hangs inside `DxgkCbSynchronizeExecution` was
   found: the revive path calls only `ExSetTimer` and atomics (legal at DISPATCH, `kobj.rs` 437 to 520). Weak. It is excluded
   or confirmed by `VsRevN` against `VsGap100N` (a revive per silence = late timer, not a race) and by the default-off v327 build.
4. **H4: a status outside every DDI's legal set that the reading missed** (a wrapped DDI answering `STATUS_GRAPHICS_*`,
   `DEVICE_NOT_READY`, a removed-class status, a scheduler or paging DDI answering anything but success). Excluded by reading
   for the runtime paths; the sticky first-fatal record makes the next occurrence name it. Read: `LostN` (0 = none), then
   `LostDdi`, `LostSt`, `LostT`, `LostThr`, `LostIrql`, `LostHint`, `LostInfL`/`LostInfH`.
5. **H5: the OS side, with a display-state trigger (monitor power, session lock/unlock, topology change)** that explains DWM
   evicting its primaries just before it was removed. KMD evidence only by absence: `PwrN`/`PwrD3N`/`PwrUid`, `HpdN`
   (indications), `ModeN`/`ModeStg`/`ModeSt` against the values before the event; `VsArmN` 1 already says no adapter D3/D0
   cycle in this generation. Keep: ask the tester for the Windows side (`Microsoft-Windows-Kernel-PnP`, `Display`,
   `dxgkrnl` operational log, `HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers` TdrLevel / TdrDelay).

### 16.5 Breadcrumbs: what existed, what is new

Existing, paging (`ddi/build_paging_buffer.rs`, `PAGING_COUNTERS` line 270 on, mirrored every 64th content operation and on a
failure-counter change; atomics are the source of truth):

| name | counts | incremented at |
| --- | --- | --- |
| `PgTi` / `PgTo` | `PgTi` system to blob copies (page-ins), `PgTo` blob to system copies (evictions) | end of `bar_transfer`, `bar_virtual_transfer_inner` |
| `PgTm` | segment to segment moves (no copy) | `bar_transfer`, virtual `LOCAL_TO_LOCAL` |
| `PgFn` / `PgDn` | fills / discards | `bar_fill`, `VirtualFill` arm / `DiscardContent` arm |
| `PgUn` | leaf PTE placements harvested | `bar_harvest_page_table` |
| `PgMr`, `PgMc` | last resource id mapped for a content op, last map cache mode | `with_blob_bytes` |
| `PgSf`, `PgTs`, `PgTd` | last transfer flags / offset / MDL offset | `bar_transfer` |
| `PgEi` | content op arrived above PASSIVE (skipped) | `build_paging_buffer_inner` IRQL gate |
| `PgEm` | blob map or kernel map failed after retries | `with_blob_bytes` |
| `PgTxG` | paging transfer with no transport | transfer arms |
| `PgEb` / `PgEc` / `PgEv` / `PgEx` / `PgEf` | range outside the blob / discontiguous PTEs / unresolved paging VA / MDL map failed / PTE shadow full | the respective checks |
| `PgSkipV` | content ops that did not move data and answered `STATUS_SUCCESS` | `build_paging_buffer_inner` tail, no-guard and shadow-full arms |
| `PgRetry` | extra attempts of a transient failure | `backoff` |
| `PgInv` / `PgInvOvf` / `PgInvSk` / `PgInvClr` | allocations marked "system copy invalid" / overflow / page-ins skipped for it / marks cleared | `note_skipped_eviction`, page-in arms, `note_eviction_done` |
| `PgV64` | virtual transfers with nonzero `Flags` | `bar_virtual_transfer_inner` |
| `PgStale` | refused stale handles | `create_allocation::paging_alloc_info` |
| `PgClamp` | ranges cut to the allocation / blob | clamp sites |
| `PgVp`, `PgVs`, `PgVd` | retained system PTEs, last virtual src / dst | PTE shadow, virtual arm |
| `PgDi` | content ops naming a device-local allocation (`NotOurs`) | transfer / fill arms |
| `PgSc` / `PgSm` / `PgSe` | system-backing leases captured / Present mirrors done / lease or mirror errors (not exceptions) | `remember_system_backing`, `mirror_present_system_backing`, the lease sites |
| `PgEh` / `PgFh` | classic TRANSFER / FILL naming no live allocation | `bar_transfer`, `bar_fill` |
| `PgFv` | VIRTUAL_FILL while the allocation was system-resident | `VirtualFill` arm |

Also readable by symbol (ntoseye), not in the registry: `PAGING_LAST_OP`, `PAGING_CALL_COUNT`, `PAGING_OP_SEEN_MASK` (bit n = operation n
was seen; UPDATE_PAGE_TABLE is 11). What each value would look like: H1 and H5 leave every `Pg*` failure counter at 0
and `PgLongUs` small; H2 shows `PgMtxMaxUs` or `PgLongUs` in the seconds only if a paging operation happened to be in flight;
H4 shows nothing in `Pg*` (the paging DDI returns only success) and the answer in `Lost*`.

New (this change; writer `ddi/device_lost.rs`, the pure half `helios_kmd_logic::device_lost`, wrappers `ddi/traced.rs` which
`lib.rs` wires into the DDI table; the real DDIs are untouched; `DxgkDdiStartDevice` is NOT wrapped, it is the frame-size-gated
nested pair, see `tools/kmd-frame-sizes.ps1`, and its failures are in `StVio` / `InitStg`). Atomics at any IRQL; the registry is
written at PASSIVE only, through ONE function, `device_lost::publish_block(Trigger)`, which is the only caller of the writer
(redirect it and the whole block moves to another thread). Three triggers: `Periodic` (`stall_diag::publish_counters`: the worker's
mirror, the escape thread's stuck-only publisher, which also fires when a suspect/fatal status or a slow call appeared, and the
StartDevice zero write; the block is written only when a ring moved or 30 s passed since the last write, so it does not ride every
`REFRESH_POST`), `Stop` (`StopDevice`, before its first hive flush, always) and `Teardown` (the `DestroyDevice` wrapper: only for a
suspect, fatal or slow event, never because an expected refusal moved a ring, and after the call's duration is taken so the stall is
already in `Dz*`).

* The sticky first-fatal record, `Lost*` (image lifetime, first wins, never overwritten; `LostN` counts all fatal events):
  `LostN`, `LostDdi` (DDI id, 16.7), `LostSt` (status), `LostT` (interrupt ms), `LostThr` (thread id), `LostHint`, `LostIrql`,
  `LostSeq` (the failure sequence number), `LostInfL` / `LostInfH` (DDIs in flight then, bitmask by id, ids 0 to 31 / 32 to 63). A fatal
  verdict is: a non-success from `BuildPagingBuffer` other than `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (so `STATUS_INVALID_PARAMETER`
  from paging is fatal), any non-success from the scheduler DDIs, a removed-class status from any DDI or callback, a `STATUS_GRAPHICS_*`
  or `STATUS_DEVICE_NOT_READY` from a DDI outside its expected set (`device_lost::verdict`, host-tested; the table is `is_expected`).
  The two callbacks (ids 42, 43) are never fatal for anything but a removed-class status: `STATUS_DEVICE_NOT_READY` from the DMA
  notify (no `DxgkCbSynchronizeExecution` at shutdown, or a refused sync) is expected, anything else they say is suspect.
* The DDI failure rings, dynamic names `<stem><kind><two hex digits>`, index 0 newest: stems `Dd` (last 16 non-success returns of any
  wrapped DDI or the two callbacks, except the routine refusals of `QueryAdapterInfo` / `ControlInterrupt`, which would push the
  entries that matter out), `Dx` (last 8 whose verdict was suspect or fatal), `Dz` (last 8 calls that took 250 ms or more,
  5 s for Escape); kinds `S` (status; milliseconds in `Dz`), `D` (DDI id in the top byte, a 24-bit hint below: the handle's low
  bits, the escape code, `uid << 16 | state << 8 | action` for SetPowerState, the interrupt type for ControlInterrupt), `T`
  (interrupt time ms). Totals `DdiFailN`, `DdiSuspN`, `DdiSlowN`; the longest call `DdiLongMs`, `DdiLongId`, `DdiLongT`; who is
  inside a DDI right now `DdiInflL`/`DdiInflH` (bitmask) and `DdiOldId` / `DdiOldMs` (the oldest entry); `DdiPubT` (when this was
  written).
* Calls of the DDIs a TDR or a teardown drives: `NPreempt`, `NResetTmo`, `NRestartTmo`, `NResetEng`, `NCreateDev`, `NDestroyDev`,
  `NCreateCtx`, `NDestroyCtx`, `NCreateProc`, `NDestroyProc`, `NStopDev`, `NSetPower`; `TResetTmo`, `TPreempt` (last call, interrupt ms).
* The last paging operation: `PgLastOp` (the raw `DXGK_BUILDPAGINGBUFFER_OPERATION` value, see `d3dkmddi.h`; 0xFFFFFFFF = none yet), `PgLastRes` (1 executed, 2 not ours, 3 skipped (answered
  success), 4 no content mutex, 5 above PASSIVE, 6 no BAR segment, 7 page-table update, 8 null args), `PgLastAl` (allocation handle,
  low 32 bits), `PgLastSz` (bytes named), `PgLastT`, `PgLastUs` (its duration). Evictions by result: `PgEvTot`, `PgEvOk` (copied),
  `PgEvSkip` (refused, answered success), `PgEvNo` (not ours / no BAR), `PgEvBad` (no mutex or above PASSIVE); page-ins `PgPiOk`,
  `PgPiSkip`. Time: `PgLongUs` / `PgLongOp` / `PgLongT` (the longest `BuildPagingBuffer` call), `PgMtxMaxUs` (the longest wait for
  the content mutex), `PgMtxFail` (could not take it).
* Locks: `VnLkN`, `VnLkWaitMs` (longest wait to get the Venus mutex), `VnLkHoldMs` / `VnLkHoldT` / `VnLkThr` (longest hold, when
  it ended, the holder's thread), `VnLkHeldMs` (age of the current hold, 0 = free), `ScLkHoldMs` (longest hold of the scanout mutex).
* The worker and the heartbeat (`ddi/stall_diag.rs`): `HpdLongSite` (a `stall_diag::site` id: the STEP that held the worker longest
  in one go), `HpdLongUs`, `HpdLongT`, `HpdLongInfl` (DDIs in flight when it ended), `HpdStep100N` (steps of 100 ms or more),
  `HpdPass100N` / `HpdPass500N` (whole passes of 100 ms / 500 ms or more); `VsGap100N` / `VsGap1000N` (silences of the heartbeat of
  100 ms / 1 s or more), and for the longest one (`VsGapMaxMs`): `VsGapT` (when it ended), `VsGapSite` (the worker's `HpdSite`
  then), `VsGapFlg` (bit 0 scanout mutex held, 1 Venus mutex held, 2 worker idle in its wait, 3 programming pending), `VsGapInfl`
  (DDIs in flight, ids 0 to 31).

Names are at most 14 characters and unique across both crates: `helios_kmd_logic::device_lost::COUNTERS` and the new rows of
`stall_diag::COUNTERS` are checked against every `b"..."` literal of `kmd_render` (test
`counters_collide_with_nothing_in_either_tree` and the existing stall_diag scan).

### 16.6 What to read after the next event, in this order

1. `LostN`. Nonzero: read `LostDdi` / `LostSt` / `LostT` / `LostThr` / `LostIrql` / `LostHint` / `LostSeq` / `LostInfL` and stop: the
   KMD answered a status it should not have (H4); `LostSt` 0xC01E.... is a graphics status, 0xC000009A is INSUFFICIENT_RESOURCES.
2. `NPreempt`, `NResetTmo`, `NRestartTmo`, `NResetEng`, `TResetTmo`, `AbnDrop`. Any nonzero = a TDR-class recovery ran (H1); when
   (`T*`) against the event; `AbnDrop` is how many pending fences it dropped.
3. `DdiPubT` (the block is fresh), then `Dx00..Dx07` (suspect and fatal returns, newest first) and `Dd00..Dd0F` (all non-success).
4. `DdiOldId` + `DdiOldMs` (a DDI inside the KMD that long NOW), `DdiSlowN`, `Dz00..Dz07` (DDI id and hint, milliseconds), `DdiLongMs`
   + `DdiLongId`. A `DestroyDevice` (id 6) of seconds, or a `BuildPagingBuffer` (13), is H2.
5. `VnLkHoldMs`, `VnLkWaitMs`, `VnLkThr`, `VnLkHeldMs`, `ScLkHoldMs`: which mutex was held for how long; `PgMtxMaxUs`, `PgLongUs`.
6. `HpdLongSite` + `HpdLongUs`, `HpdStep100N`, `HpdPass500N`; `VsGapMaxMs`, `VsGap1000N`, `VsGapFlg`, `VsGapSite`, `VsGapInfl`.
7. `PgLastOp`, `PgLastRes`, `PgLastAl`, `PgLastT`, `PgEvTot` / `PgEvOk` / `PgEvSkip` / `PgEvNo` / `PgEvBad`, the existing `PgSkipV`, `PgInv*`,
   `PgStale`, `PgEh`, `PgSe`.
8. `PwrN`, `PwrD3N`, `PwrUid`, `HpdN`, `ModeN`, `ModeStg`, `StartN`, `VsArmN`, `VsDisN`, `VsEarlyN`, `VsRevN`.

What each hypothesis predicts: **H1** (guest stall): step 2 nonzero or `AbnDrop` > 0; `LostN` 0; `VsGapMaxMs` large with `VsGapFlg` bit
2 and no other bit, `VsGapInfl` 0, `HpdLongUs` and `DdiLongMs` small. **H2** (blocked DDI): `DdiLongMs` / `Dz*` / `HpdLongUs` in
the seconds with `DdiLongId` 6 (DestroyDevice) or 13, `VnLkHoldMs` of the same size, `VsGapFlg` bits 0 or 1 at a gap that is NOT
accompanied by an idle worker. **H3** (revive race): `VsRevN` greater than `VsGap100N`, no `Dz*`, `HpdLongUs` small. **H4**: step 1.
**H5**: everything quiet, `PwrD3N` or `HpdN` / `ModeN` moved across the event.

### 16.7 DDI ids (`helios_kmd_logic::device_lost::ddi`)

1 StartDevice, 2 StopDevice, 3 RemoveDevice, 4 SetPowerState, 5 CreateDevice, 6 DestroyDevice, 7 CreateContext, 8 DestroyContext, 9
CreateProcess, 10 DestroyProcess, 11 CreateAllocation, 12 DestroyAllocation, 13 BuildPagingBuffer, 14 SubmitCommand, 15
SubmitCommandVirtual, 16 PreemptCommand, 17 ResetFromTimeout, 18 RestartFromTimeout, 19 ResetEngine, 20 QueryEngineStatus, 21 Render,
22 RenderKm, 23 RenderGdi, 24 Present, 25 OpenAllocation, 26 CloseAllocation, 27 MapCpuHostAperture, 28 SetVidPnSourceAddress, 29
CommitVidPn, 30 Escape, 31 QueryAdapterInfo, 32 UnmapCpuHostAperture, 33 Patch, 34 ControlInterrupt, 35 IsSupportedVidPn, 36
UpdateActiveVidPnPresentPath, 37 SetVidPnSourceVisibility, 38 EnumVidPnCofuncModality, 39 RecommendFunctionalVidPn, 40
QueryChildStatus, 41 QueryChildRelations, 42 `cb:IndicateChildStatus` (the HPD worker's hot-plug callback), 43 `cb:NotifyDmaCompleted`
(`DxgkCbSynchronizeExecution` / `DxgkCbNotifyInterrupt` refused a DMA completion or a vsync).

### 16.8 The worker pass and the vsync gap: what is blocking, and the fix (design; NOT implemented here)

The HPD worker's pass (`ddi/hpd.rs`, loop from line 232 to 436) is a sequence of steps, each of which can block; `HpdLongSite` now names the
one that did. Every blocking call reachable from it, with the step id (`stall_diag::site`):

| step | call | what blocks | bound today |
| --- | --- | --- | --- |
| 3 `INDICATE` (299) | `DxgkCbIndicateChildStatus` | dxgkrnl's own VidPn / child locks | none (dxgkrnl) |
| 4 `DRAIN_USED` (292) | `drain_used_and_complete` | `virtio_lock`, `wddm_notify_lock` (spinlocks, short) | none needed |
| 5/6 `FOREIGN_SCANOUT`, `FOREIGN_FENCE` (305, 310) | host `Close` round trips | the control queue | per-call timeouts |
| 7/16/18 `DEFERRED_*` (324) | `with_scanout_lifecycle`: `KeWaitForSingleObject(scanout_mutex, NULL)` (`adapter/locks.rs` 271); inside it `SET_SCANOUT_BLOB` and the Venus copy | the scanout mutex (infinite wait), the host (up to 30 s) | none on the wait; 30 s on the call |
| 8 `WINDOWED_BLT`, 9 `RM_CLIENT`, 10 `FOREIGN_FLIP` | host round trips under `venus_mutex` (infinite wait, `locks.rs` 239) | the Venus mutex, the host | `SweepBudget` on some |
| 11 `NVRM_PUBLISH`, 12 `DUMP`, 19 `REFRESH_POST` | about 60 to 150 synchronous `RtlWriteRegistryValue` per call | the registry / hive lock (shared with every process writing the registry: an explorer restart is one) | none |
| 13 `PROBE` | a 5 s fence wait and a map round trip | the host | 5 s |
| 14/17 `REFRESH` | `with_scanout_lifecycle` as above | the scanout mutex | none on the wait |

The cheapest culprit for a 2.4 s pass in a window with a registry-heavy event (an explorer teardown and start) is the registry
mirror: `HpdDumpUs` 1.45 s over all dumps says each dump is cheap in the mean, so a single 2.4 s pass would be a step blocked,
not a mirror that is always slow; `HpdLongSite` decides. Design of the fix, in order of cost:

1. Revive the heartbeat from the interrupt DPC (`ddi/interrupt.rs` `dxgkddi_dpc_routine`, 517k calls in the run, about 430 per
   second) by calling `vsync_watch(false)` there: it needs no PASSIVE state (`ExSetTimer` and atomics), so a stuck worker no
   longer delays a revive. This changes behaviour only for a heartbeat that has been silent 250 ms.
2. Replace the infinite waits on `scanout_mutex` and `venus_mutex` in the WORKER's steps with a bounded wait (20 ms), and on timeout
   set a "retry" flag the existing wake logic turns into a short timer: the step is requeued, the pass ends, the heartbeat's
   reviver and the other steps run. The DDI threads keep the infinite wait (they have a caller to block).
3. Move the registry mirrors (steps 11, 12, 19: `nvrm_publish_service`, `dump_periodic`, `pacing_snapshot`) to a low-priority
   "diag" system thread that the worker only signals: it removes the largest unbounded blocker from the pass at no change to
   behaviour. Only the stuck-only publisher (escape thread) and `StopDevice` keep writing inline.
4. Host round trips in the worker (steps 5, 7, 8 to 10, 13) run under a `SweepBudget` slice of tens of milliseconds and carry their
   progress in a state machine between passes (the foreign-flip service already does for `FfAsyncWin`). Anything that cannot be sliced
   moves to its own service thread with a bounded queue.

None of these is applied in this change; `HpdLongSite`, `HpdStep100N`, `VsGapFlg` and `Dz*` exist to say which one to build first.

### 16.9 Checklist

* After a v327+ build is installed: `reg query` the service key, confirm `DdiPubT`, `LostN` 0, `DdiFailN` small, `NCreateDev` rising
  with application starts, `PgLastT` moving on a paging operation, `VnLkN` rising.
* Reproduce: restart explorer from another process while NVK and Venus devices are live (the event's conditions); read the block
  within seconds (the escape publisher refreshes it when a suspect or fatal status or a slow call appeared; otherwise the next
  `pacing_snapshot`, about every 10 s of presents).
* If every device is removed again: follow 16.6 in order and record the values; the first fatal record is not overwritten.
* Before pushing a change to this instrument: run `kmd_logic` tests (the collision scan), the protocol crate tests (the v315 lesson),
  and the type check; `traced.rs` is a table of signatures, so a bindgen name that changed shows as a compile error there.
## 18. FlipCapsX and flip flag counters (S-0a)

For the independent-flip probe S-0a of `independent-flip.md` (branch `kmd/independent-flip-design`): no rebuild per matrix
row, and a read of the flip flags the driver ignores. Header facts are from WDK 10.0.26100.0, `d3dkmddi.h` under
`/home/user/.local/share/conduit-dev/wdk-10.0.26100.0/` (not copied into the repo).

### 18.1 FlipCapsX: raw DXGK_FLIPCAPS bits OR'd into the reported word

`FlipCaps` is `DXGK_DRIVERCAPS` offset 60, a `DXGK_FLIPCAPS` union whose `Value` is a UINT; `query_driver_caps` writes that
UINT (`out.set(caps_offset!(FlipCaps), ...)`), not the bit fields, so the knob is a raw bit mask. Bits (`d3dkmddi.h:1967-1992`):

| bit | mask | field | note |
|---|---|---|---|
| 0 | 0x01 | `FlipOnVSyncWithNoWait` | not accepted from the knob |
| 1 | 0x02 | `FlipOnVSyncMmIo` | the driver's own default word; load-mandatory (Code 43 without it) |
| 2 | 0x04 | `FlipInterval` | not accepted from the knob |
| 3 | 0x08 | `FlipImmediateMmIo` | not accepted: deliberately clear, setting it was a measured regression (`query_adapter_info.rs`, defect 0ab) |
| 4 | 0x10 | `FlipIndependent` | accepted. "MMIO flip to redirected surfaces bypassing DWM Present" (:1978, WDDM 1.3+) |
| 5 | 0x20 | `DdiPresentForIFlip` | accepted. "Call `DxgkDdiPresent` when independent flip Present might be issued" (:1980, WDDM 2.0+) |
| 6 | 0x40 | `FlipImmediateOnHSync` | accepted (:1981, WDDM 2.0+) |
| 7+ | | `Reserved` | not accepted |

Semantics (`kmd_logic::flip_flags::resolve_flip_caps`): reported = `FlipOnVSyncMmIo` | (`FlipCapsX` & 0x70). `FlipCapsX`
0 (the default, or absent) reports 0x2, byte-identical to before. `0x10` reports 0x12, `0x30` reports 0x32, `0x70` reports 0x72;
writing the full word (`0x12`, `0x32`) reports the same, since the default's own bit is a no-op. Every other bit is dropped; the
dropped bits are published as `FlipCapsXMsk` (0 when nothing was dropped).

Behaviour change: until now a nonzero `FlipCapsX` REPLACED the whole word (any bit pattern, unfiltered), and the one documented
use, `FlipCapsX=2`, was the default word anyway. A value that cleared `FlipOnVSyncMmIo` or set `FlipImmediateMmIo` could still be
typed; neither is any longer possible. Anything that only used 2, or 0, behaves as before.

Read time: `AdapterKnobs::read` at AddAdapter and again at StartDevice (`read_at_start`), like `DirectFlipCaps`, so the caps the
query reports and the mirrors cannot disagree; before it was re-read on every caps query. Mirrors, written at EVERY StartDevice,
0 included (13.8 rule 1): `FlipCapsXEff` (the accepted bits), `FlipCapsXMsk` (the dropped bits), `FlipCapsRep` (the final reported
word). `FlipCapV` (the word actually written, at each caps query) is unchanged. A quick check after a restart: `FlipCapsRep` equals
`0x2 | (FlipCapsX & 0x70)`; `FlipCapsX=0x10` and `FlipCapsRep` still 2 means the value was not read at this start.

### 18.2 Flip flag counters (read-only)

The driver ignores these flags; the counters only say whether dxgkrnl ever sets them. Atomics only (`SetVidPnSourceAddress` can run
at DIRQL), zeroed at every StartDevice with the rest of `scanout_trace` (`reset`), published at PASSIVE from `scanout_trace::dump`
(the HPD worker's periodic dump, which is also the only call site). The DDIs' behaviour is unchanged.

| value | meaning |
|---|---|
| `IdfSpaTrans` | `SetVidPnSourceAddress` calls with `SharedPrimaryTransition` set (`Flags` & 0x40) |
| `IdfSpaExcl` | ... with `IndependentFlipExclusive` set (`Flags` & 0x80) |
| `IdfSpaMove` | ... with `MoveFlip` set (`Flags` & 0x100) |
| `IdfSpaFlg` | the last full `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.Value` seen |
| `IdfPrRedir` | `DxgkDdiPresent` calls with `RedirectedFlip` set (`Flags` & 0x2000) |
| `IdfPrFlg` | the last full `DXGK_PRESENTFLAGS.Value` seen |

CORRECTION to the bit values quoted in `independent-flip.md` (10.2: `SharedPrimaryTransition` 0x20, `IndependentFlipExclusive` 0x40,
`MoveFlip` 0x80, taken from the comments in the header). Those comments are stale: `DXGK_SETVIDPNSOURCEADDRESS_FLAGS`
(`d3dkmddi.h:6212-6245`) has `ModeChange`, `FlipImmediate`, `FlipOnNextVSync`, `FlipStereo`, `FlipStereoTemporaryMono`,
`FlipStereoPreferRight` (bits 0..5; the two stereo comments both say 0x10), then `SharedPrimaryTransition` (bit 6), 
`IndependentFlipExclusive` (bit 7), `MoveFlip` (bit 8), `Reserved :23` (9 + 23 = 32 bits). The compiler's, and so bindgen's `.Value`,
values are 0x40 / 0x80 / 0x100, which is what is counted. A reader of `IdfSpaFlg` therefore sees 0x40 for a transition, not 0x20
(0x20 is `FlipStereoPreferRight`). `DXGK_PRESENTFLAGS.RedirectedFlip` is 0x2000 as commented (`:167-199`: 13 fields before it);
the existing present-flags histogram (`FlR<n>` / `FlC<n>` / `FlTot`) already holds every present `Flags` word, `IdfPrFlg` holds the latest one.

`IdfSpaFlg` / `IdfPrFlg` are last-writer values (a flip carrying a flag can be followed by one that does not); the counters are
the evidence, the last values are for decoding what an unexpected flag word is.

### 18.3 The S-0a procedure

Matrix: `FlipCapsX` in {0, 0x10, 0x30} x `DirectFlipCaps` in {0, 1}, six rows, baseline first (`FlipCapsX` 0, `DirectFlipCaps` 0).
Lowest mode first (1920x1080 at 60 Hz), then the real mode.

For each row:

1. Set the knobs in the service key (`reg add ... /v FlipCapsX /t REG_DWORD /d <v> /f`, same for `DirectFlipCaps`) and REBOOT the VM.
   The knobs are read at StartDevice, and `pnputil /restart-device` re-reads them, but dxgkrnl derives the user-mode caps answers
   from what the adapter reported when it was created: a reboot is the way to be sure a row's answer is that row's. Check `FlipCapsXEff`, `FlipCapsRep` and `DirectFlipCaps`' mirror
   (`0x01D7` bit 2) match the row before trusting anything below.
2. User-mode probe (a `tools/adapter_type_probe.cpp`-style program): `D3DKMTQueryAdapterInfo` with `KMTQAITYPE_DIRECTFLIP_SUPPORT`
   (19, `D3DKMT_DIRECTFLIP_SUPPORT`) and `KMTQAITYPE_INDEPENDENTFLIP_SUPPORT` (28); the rest of the S-0a list of
   `independent-flip.md` (`INDEPENDENTFLIP_SECONDARY_SUPPORT` 39, `MULTIPLANEOVERLAY_SUPPORT` 20, `MPO3DDI_SUPPORT` 43,
   `SCANOUT_CAPS` 67, `DISPLAY_CAPS` 74) if cheap. Record which `Supported` values changed: that is dxgkrnl's derivation from our
   caps, before any application runs.
3. Then a borderless, flip-model, fullscreen application at exactly the display mode (`d3d11_triangle.cpp` is the existing vehicle),
   with PresentMon running: record `PresentMode` ("Composed: Flip" against "Hardware: Independent Flip" or "Hardware Composed")
   and the flip rate; and read the counters after the run: `IdfSpaTrans`, `IdfSpaExcl`, `IdfSpaMove`, `IdfSpaFlg`, `IdfPrRedir`,
   `IdfPrFlg`, with `VpFlip`, `VpMmio`, `VpDmaF`, `PBFlip`, `FkKeep*`, `FlipCapV`. A promoted flip is `IdfPrRedir` / `IdfSpaExcl`
   rising; a nonzero `IdfPrRedir` with `IdfSpaExcl` 0 says dxgkrnl redirected the Present but never asked for the exclusive primary.
4. Write the six rows into the S-0 result table of `independent-flip.md` (the two docs live on different branches).

Safety. A wrong caps combination can change what DWM does: `FlipIndependent` is "MMIO flip to redirected surfaces bypassing DWM
Present" and may change how dxgkrnl issues DWM's OWN flips, and a promoted application with no pointer and a KMD that does not
program the application's buffers can freeze the screen. Watch `PBFlip`, `FkKeep*`, `VpPrF` and DWM's present count on the first
boot with a bit set, and have the VM snapshot. Recovery: delete (or set to 0) `FlipCapsX` and `DirectFlipCaps` in the service key and
reboot; if the VM cannot be reached, set the same values from the offline registry (the service key under the Helios driver's
`HKLM\SYSTEM\CurrentControlSet\Services`) or revert to the snapshot. The defaults (`FlipCapsX` 0, `DirectFlipCaps` 0) are what ships.

### 18.4 Verified, and not

Verified on the host: the pure decode and mask (`kmd_logic::flip_flags`, host tests: default 0x2 byte-identical, the matrix values,
dropped bits, the bit values above); the bit values against the header text; rustfmt parse of every KMD source; the WDK-less type
check of `kmd_render` with stubs. NOT verified: a WDK build of `kmd_render`; that `FlipCapsRep` etc. appear in a VM service key;
what dxgkrnl does with any non-default row (that is the experiment).

## 17. Incident: HPD worker "frozen at site 11", display asleep, and a guest that would not shut down (v327)

### 17.1 Symptom

KMD 327.1, no device restart since boot. Two registry dumps 10 s apart (uptime 1121046 / 1131578 ms) read
`HpdLoopT` = `HpdSiteT` = `HpdPhaseT` = `ScLkAcqT` = `ScLkRelT` = 824616, `HpdSite` 11 (`nvrm_publish_service`),
`StallT` 824619, `HpdPassMaxUs` 30011, `VpPend` 0 then 2562804832. A later checked restart (`shutdown /s`) left the guest
unresponsive (SSH timed out at the banner, the VM "running" with an idle CPU) until it was powered off from the host.

### 17.2 What the counters actually say (read this before the hypotheses)

1. **The worker was not blocked.** `HpdSite`, `HpdLoopN`, `HpdLoopT`, `ScLk*` are written ONLY by
   `stall_diag::publish_counters`, and that ran last at `StallT` 824619, from the worker itself, three
   milliseconds after it entered site 11 (the `Nv*` mirror calls it). Nobody refreshed it afterwards: the escape
   thread refreshes it only while the worker "looks stuck", and an idle worker (asleep in `WAIT`, nothing pending) never
   does. The `HpdSite` 11 / `HpdLoopT` 824616 pair is a snapshot of a worker that then went to sleep.
   The live values of the same dump say so: `HpdWait` 0 (an infinite wait, `hpd_wake::wait_us`), `HpdWkEvt` 106648 ->
   106772 (the worker woke 124 times in between), `HpdBusyUs` +10 ms, and `VpDmpT` 1129213 (the worker ran the periodic
   dump right after the heartbeat's tick at 1129212). The worker enters `site::WAIT` (1) before every sleep, so a
   sleeping worker never leaves 11 in the LIVE atomics.
2. **`HpdPassMaxUs` 30011 is microseconds**: the longest pass was 30 ms (a pass that ran the ~8 ms `Vp*` dump plus
   work), not 30 s. There is no 30 s timeout in the evidence.
3. **`VpPend` 2562804832 is not garbage.** `scanout_trace::dump` writes `pending_vidpn_allocation as u32`: the
   low 32 bits of the 64-bit allocation handle dxgkrnl passed to `SetVidPnSourceAddress` (the `Vp*D` ring holds
   the same family of values, `PrCreateLo` 2562808192). 0 = nothing deferred (the first read, a dump from 824566, stale);
   nonzero = a deferred programming waiting for the worker (the second read, taken right after the display woke).
   `VpGate` 0 -> 1 is the programming gate raised for it.
4. **Dump 1 was a stale dump.** Its `VpDmpT` is 824566, its `VsTickT` 640239: it is the registry as the last
   dump left it, 300 s old. Dump 2 is the first fresh one (`VpDmpT` 1129213).
5. **The heartbeat stopped at 640 s by a power call.** `VsArmN` 1, `VsDisN` 1, `VsCanN` 1, `PwrN` 1, `PwrUid` 0
   (a child), `PwrD3N` 1, `VpVsEn` 0 (dxgkrnl disabled CRTC_VSYNC): the monitor child went to D3 (most likely the display
   idle timeout, about ten minutes after boot) and `VsPowerMode` 0 (KMD 325 semantics) quiesced the heartbeat for
   it. `FlipPubT` 625804 is the last flip the display retired before that. At 1129 s a D0 call (`PwrN` 2, `VsArmN` 2)
   re-armed it and `VpEnt` / `VpPrgN` / `FlipPub` moved again, DWM with them.

So the DWM "stall" is the display asleep (nothing asks for vsync, DWM presents nothing), and a user NVK scan-out source
(`FsLastP` 824582, `FsEndBy` 5 = process exit at 824796) kept the screen alive through the foreign scanout while
Windows believed the monitor off. It is not a worker deadlock.

### 17.3 Ranked hypotheses, with what each predicts

| rank | hypothesis | predicts | verdict |
| --- | --- | --- | --- |
| 1 | the monitor child's D3 (display idle timeout) with the heartbeat quiesced; the worker idle | `PwrChSt` 0, `VsCiSt` 0, `VsTickT` frozen, `HpdWait` 0, `HpdWkEvt` nearly still, `VpPend` set when the display wakes | matches every live value. Not a KMD defect by itself. |
| 2 | stale snapshot misread (the worker "stuck at 11") | `StallT` older than `VpDmpT` | proved above; fixed by v328 (the block is written by every dump and by the escape thread when 5 s old) |
| 3 | worker blocked in a registry write at site 11 | `HpdSite` 11 AND `StallT` / `HpdWkEvt` frozen, the escape publication firing (it fires when the worker looks stuck) | refuted: `HpdWkEvt` moves, `HpdWait` is 0, no escape publication. Still a latent risk (a registry write can block behind a hive flush), see 17.6 |
| 4 | lock inversion worker / owner-death thread (`DestroyDevice`) | the scanout mutex held (`ScLkN` != `ScLkRelN`) | refuted for this run: `ScLkN` = `ScLkRelN` and the worker woke afterwards |
| 5 | an infinite wait ending at a 30 s timeout | `HpdPassMaxUs` of 30 s | refuted (30 ms) |

### 17.4 The guest that would not shut down

NOT explained by the evidence: no dump exists from the wedge, and no stop-progress counter existed. What the audit
of the paths a graceful shutdown takes (SetPowerState D3, StopDevice, RemoveDevice) found:

* `quiesce_vsync` on a heartbeat that is already disarmed returns at the first `swap` (`kobj.rs` `disarm_vsync`); no wait.
* `ExCancelTimer` does not wait for a callback; `ExDeleteTimer(cancel, wait)` is only in `Drop` (RemoveDevice), and a
  disarmed heartbeat has no callback in flight. The embedded-timer fallback's `KeFlushQueuedDpcs` is used only when
  `ExAllocateTimer` failed.
* `stop_hpd` sets `hpd_stop` and THEN signals `hpd_event` (`kobj.rs` `stop_hpd`), so an idle worker with an infinite
  wait wakes and exits; the joins are bounded (5 s on the exit event, 5 s on the thread), and a leak latch protects
  RemoveDevice.
* The host round trips of the stop are bounded by one `SweepBudget` plus one in-flight command (30 s,
  `SYNC_ROUNDTRIP_TIMEOUT_MS`).
* Three waits are INFINITE by design (they are mutexes): the venus mutex (`acquire_venus_mutex`, also taken by
  `set_venus_client(None)` in StopDevice), the scanout mutex (`with_scanout_lifecycle`, taken by the display DDIs, the
  worker, DestroyAllocation ...) and the content mutex (`PassiveMutex`). They are events, not owner-tracked mutexes: a
  holder that never releases (a thread parked in a host round trip behind a stopped host, a path that forgot to
  release, or a recursive take) blocks every later taker, including a power or stop path, with no timer to end it
  and an idle CPU. These are the candidates for a wedge of this shape.

Nothing found proves a defect there, so v328 makes the next one nameable instead of changing the locking.

### 17.5 v328: what changed (diagnosis only; defaults and behaviour are v327's)

* **No default changes.** `VsPowerMode` stays 0, `VsWatchdog` 0, `VsIdleWake` 0. Reason: in this incident dxgkrnl
  itself had disabled CRTC_VSYNC (`VpVsEn` 0) for the sleeping display, so a heartbeat kept alive by `VsPowerMode` 1
  would tick with the delivery gate closed (`VsOffN`) and deliver nothing; DWM waits for the display, not for the
  heartbeat. `VsPowerMode` 1 is also the v326 behaviour whose restart mode loss (section 15) is unexplained. Test the
  restart with `VsPowerMode` 1 on v328 before making it a default.
* **The stall block is never a stale snapshot for long.** `scanout_trace::dump` now calls `stall_diag::publish_counters`
  (the whole block, the two mirrors it wrote alone included), and the escape thread publishes the block when
  it is 5 s old even if the worker does not look stuck (`escape_publish_due`, host-tested), at most twice a second, and
  also on an unpublished suspect or fatal status (`device_lost::serious_dirty`, section 16). Every write of the block
  still goes through `stall_diag::publish_counters`, which ends in the single `device_lost::publish_block(Periodic)`
  funnel of section 16. Read the age as `VpDmpT - StallT` (`snapshot_is_stale`).
* **The sliced waits and the section 16 instrument**: `wait_logged` only splits the same KeWait into 5 s slices; the
  Venus mutex's wait/hold accounting (`VnLkWaitMs`, `VnLkHoldMs`, `venus_acquired`) brackets the whole wait and starts
  its hold clock after it, and the traced DDI wrappers' in-flight marks bracket the whole DDI, so neither sees the
  slices. `LkWait*` (this section) and `VnLk*` (section 16) are two views of the same wait.
* **The display's power history is visible**: `PwrT`, `PwrAdSt`, `PwrChSt` (1 D0, 0 not D0, 0xFF no call yet),
  `VsCiT`, `VsCiSt` (the last `ControlInterrupt(CRTC_VSYNC)`), and `PwrStg` (1 entered, 3 done: a power call that never
  reached 3 hung inside).
* **The infinite mutex waits are sliced, not bounded** (`sync::wait_logged`): 5 s slices, each expiry counted
  (`LkWaitN`, `LkWaitWh` 1 venus / 2 scanout / 3 content, `LkWaitT`, `LkWaitMs`) and the wait goes on, so mutual exclusion
  is never given up and nothing can proceed unlocked; a holder that never lets go shows in the next dump.
* **Stop progress**: `StopSub` / `StopSubT` (`stall_diag::stop_sub`, 17 steps of StopDevice, 20-22 for RemoveDevice, and 23, written between 21 and 22 before the timer deletion waits: section 19.3),
  written to the registry BEFORE each step. `StopStg` / `StopMs` (stages 1-10) are unchanged. After a wedge the next
  boot's service key holds the last step entered: read `StopSub`, `StopSubT`, `PwrStg`, `PwrChSt`.

### 17.6 Not done, and why

The proposed move of every registry publication off the HPD worker onto a dedicated publisher thread is NOT in
v328: the evidence refutes a blocked worker, and a new system thread with its own start, stop and join (and a
stuck-registry deadlock of its own at StopDevice) cannot be built or run here. The worker's registry writes remain
(`HpdDumpUs` / `HpdDumpN` = 8 ms per dump); `HpdPassMaxUs` is the number to watch (30 ms in this run). If a future
dump shows `HpdSite` 11 or 12 with a LIVE `StallT` (`StallT` within a second of `VpDmpT`) and `HpdLoopT` frozen, the
worker is blocked in a registry write and the publisher thread becomes the fix.

### 17.7 Counters to read after a wedge or a stall

`VpDmpT` and `StallT` (age of the block), `HpdWait` (0 = infinite), `HpdWkEvt` (moving = alive), `PwrChSt` / `PwrAdSt` /
`PwrT`, `VsCiSt` / `VsCiT`, `VsArmN` / `VsDisN`, `VsTickT`, `LkWaitN` / `LkWaitWh` / `LkWaitMs`, `StopSub` / `StopSubT`,
`StopStg` / `StopMs`, `PwrStg`.

Tester: set the display idle timeout to 0 (`powercfg /x monitor-timeout-ac 0` and `-dc 0`) in the test image, or wake the
display with input, before a user NVK run; the sleeping display is the reason DWM "stalled".

## 19. Heartbeat stops after (re)start (v327 device-restart round 1; breadcrumbs and watchdog timer, v329)

### 19.1 The report

KMD 327.1, `VsPowerMode` 1, `VsWatchdog` 1, an NVK `d3d11_spin` running, `pnputil /restart-device`: the mode was kept
(5120x1440@240), DWM stopped presenting, and `VsTickN` read 2135 and stayed 2135 (2135 ticks at 240 Hz is 8.9 s after the
restart). `VsDisN`, `VsCanN`, `VsEarlyN`, `VsExhN`, `VsRevN` all 0, `VsArmN` 1. Restarting DWM moved the counters (`VsTickN`
36380) and they stopped again. A second stopped state (`VsTickN` 46496, `VsTickT` 623181, `StallT` 623184, read at uptime
662609): the one thread with `helios_kmd_render` on its stack was the HPD worker, idle in `KeWaitForSingleObject`; the
system was responsive.

### 19.2 What the code says, ranked

1. **H0, the counters are a lazily written mirror, so "VsTickN did not move" does not mean "no ticks".** Nothing in the
   tick path writes the registry (`service_vsync_tick` and `on_vsync_tick` are atomics only, DISPATCH). `VsTickN`,
   `VsTickT`, `VsRevN`, `VsDisN` ... reach the service key only from `stall_diag::publish_counters` /
   `publish_vsync_ticks`, which run from the HPD worker's periodic dump (`scanout_trace::dump_periodic`, first pass, then
   128 passes AND 1 s) and from an escape (`publish_from_escape`, only when the worker looks stuck or the snapshot is
   older than 5 s). The worker waits with no timeout when nothing is due (`VsIdleWake` 0, `hpd_wake::idle_watch`), and
   the tick wakes it only while a programming is pending (`pending_vidpn_allocation != 0`). A desktop that stopped
   changing therefore leaves the worker asleep and the whole block frozen at its last pass, with every counter in it
   frozen too (`VsRevN` and `VsEarlyN` included: a 0 there means "0 as of the last pass"). Facts that fit H0 and not a
   dead chain: (a) `StallT` - `VsTickT` was 3 ms, less than one 240 Hz period (4.17 ms), which is what ANY mirror taken
   from a running chain reads; a chain that died by itself would have to die within 3 ms of a worker pass; (b)
   `VsTickN` 46496 over `StartT` 428489 to `VsTickT` 623181 (194.7 s) is 238.8 Hz, a chain that ran the whole time with
   `VsGapMaxMs` 95; (c) 34245 ticks between two reads in round 1 is 142.7 s at 240 Hz; (d) "DWM stopped presenting" after a
   device restart is the designed outcome (the UMD reports `device removed (KMD gone: true)`), and DWM on a static desktop
   presents nothing; (e) a LiveKD with the worker as the only Helios thread, idle, and a responsive system says no
   callback is blocked (H1 is out), not that the timer is not queued. A stack cannot show a missing timer.
   Discriminate on hardware by LIVE state, not the mirror: see 19.6.
2. **H1, a blocked `DxgkCbSynchronizeExecution` inside the tick** (the coordinator's list item 1). In the code the
   count is incremented BEFORE the synchronized call (`on_vsync_tick`, `VS_TICKS.fetch_add`, then the call in
   `service_vsync_tick` via `signal_crtc_vsync`) and the one-shot is re-armed BEFORE both (`set_vsync_one_shot`, then the
   `vsync_armed` re-check), so a long sync does NOT end the chain by itself; it would end it only if an Ex timer never
   calls back while its callback runs (not documented either way) AND the sync never returns. The LiveKD refutes a
   stuck callback for the second stop. Not fixed (the condition for the "queue the notify to a DPC" change, a confirmed
   hung callback, is not met); `VsCbIn` / `VsCbOut` / `VsCbSync*` (19.4) settle it on the next run.
3. **H2, a re-arm path that is skipped.** Every return in `service_vsync_tick` was checked. Before the re-arm: the display
   half off or `vsync_armed` 0 (counted `VsEarlyN`), deadline exhaustion (`VsExhN`; `vsync_deadline::next` is `None`
   only on a zero period or a `u64` overflow, and `period_100ns` never returns 0), and the Ex callback's null context.
   After the re-arm: the post-arm `vsync_armed == 0` re-check (cancels, counted `VsCanN`), the closed delivery gate
   (`vsync_enabled == 0`, returns after the re-arm), and `dxgkrnl_opt()` `None` (after the re-arm). A failed or refused
   `DxgkCbSynchronizeExecution` / notify does not return early (the status only skips the timeline note); the pending-
   programming `signal_hpd` and the count follow regardless. `ExSetTimer`'s return value is "the timer was already set",
   not an error, and is rightly ignored. No path was found that leaves `vsync_armed` 1 with no timer queued. Races
   examined: `arm_vsync` returns early when `vsync_armed` is already 1, so a dead chain with a stale flag is not
   re-armed by a D0 resume (it IS by the revive paths below); `disarm_vsync` sets the flag before cancelling and the
   tick re-checks it after re-arming, so StopDevice ends cancelled; a callback of the previous generation that
   re-arms while the new `arm_vsync` sets the one-shot replaces a pending expiry with one a period later, harmless. The
   Ex timer is allocated once per adapter and survives a restart (`pnputil /restart-device` keeps the same
   `AdapterContext`; only RemoveDevice deletes it), so no stale timer object or callback context can exist.
4. **H3, the deadline.** `vsync_deadline::next(anchor, now, period)` with the stored deadline as the anchor skips any
   gap to the first future deadline, so a long gap or a stale `VS_REF_AT` cannot produce a far deadline (`VS_REF_AT` is
   only the watchdog's silence reference, zeroed by `start_generation` and set by the arm, not an input to the deadline);
   `relative_due` is clamped to at least 100 ns and at most `i64::MAX`. `period_100ns(0)` is 60 Hz.
5. **H4, the delivery gate and notify failure.** A closed gate (`ControlInterrupt` disable) only counts `VsOffN`; the
   re-arm precedes it. A notify failure never stops the chain.
6. **H5, the old watchdog is blind exactly when needed** (confirmed from code): `vsync_watch` runs on a worker pass or an
   escape only, and the worker has no timeout by default. That is a real defect whatever H0 turns out to be, and is the
   reason for 19.3.

### 19.3 The independent watchdog timer (default on, `VsWdTimer`)

A second Ex timer (`ExAllocateTimer`, default resolution, its own callback `vsync_wd_callback`), allocated at AddDevice
beside the heartbeat's, armed in `start_vsync` AFTER the heartbeat and stopped in `stop_vsync` (so StopDevice and
RemoveDevice), deleted with cancel+wait at RemoveDevice. A 250 ms one-shot chain that re-arms itself FIRST and checks
`vsync_wd_on` after, like the heartbeat's own cancel rule. It does not depend on the heartbeat chain, the worker or an
escape. Each tick (`AdapterContext::vsync_wd_tick`, DISPATCH, atomics and `ExSetTimer` only), decisions in
`kmd_logic::vsync_wd` (host-tested):

* heartbeat armed, display half up, adapter in D0, and silent for more than `max(250 ms, 16 periods)` (newest of the last
  tick and the reference), and no tick callback in flight: re-arm it (`revive_heartbeat`, shared with the old watchdog;
  counted `VsRevN` and `VsWdFixN`);
* the same silence with a tick callback entered and not returned (`VsCbIn` above `VsCbOut`): count `VsWdHungN` and do not
  re-arm (a re-arm cannot unblock a callback);
* every 8th tick (2 s): ask the worker to write ten live values (`VsLiveT`, `VsTickN`, `VsTickT`, `VsCbIn`, `VsCbOut`,
  `VsWdTkN`, `VsWdTkT`, `VsWdAgeMs`, `VsWdFixN`, `VsWdHungN`; a value that did not change since its last write is
  skipped, `stall_diag::rec_live`); after an action (a fix or a hang): the whole heartbeat block, but at most once per
  2 s however often it acts (`vsync_wd::publish_plan`; a hang does not re-base the reference and acts on every tick, a fix
  repeats about every 500 ms: neither may become 4 wakes and ~190 registry writes a second). The request is
  `request_live_publish` then `signal_hpd`; the worker calls `publish_live_if_wanted` after its watchdog call. This is what
  keeps the mirror from going stale while the worker is otherwise asleep: `VsLiveT` is the time of the write;
* nothing at all while the adapter is not in D0: a heartbeat quiesce (`quiesce_vsync`) stops the timer and a D0 resume
  restarts it, and its callback is a no-op outside D0, so the power-down and shutdown windows see no new activity.

Independence from `VsWatchdog`: with `VsWatchdog` 0 (the default) the timer STILL re-arms an armed, silent heartbeat
(`VsWdTimer` 1). It never resurrects a quiesced one: `vsync_wd::decide` requires `armed`, so the `VsWatchdog` 2 "Resume" of
the old watchdog has no counterpart here. A revive that loses the race with a quiesce bumps nothing
(`revive_heartbeat` checks `vsync_armed` before `VsRevN`).

A healthy chain never meets it: the silence limit is 60 periods at 240 Hz. Cost: one DISPATCH callback and a few atomics
every 250 ms, and on an idle desktop one worker wake and three or four registry writes every 2 s (`VsLiveT`, `VsWdTkN`,
`VsWdTkT` always change; the rest only when they move). The wake shows as `HpdSgOth` and `HpdLoopN` rising about every 2 s:
that is this timer, not a regression. A side effect of the wakes: `dump_periodic` has a 128-pass gate, so on an idle desktop
the ~120-write `Vp*` dump now runs about every 256 s instead of never. It changes the `HpdWait` accounting not at all (the
worker's wait stays infinite; the event is set). `VsWdTimer` 0 turns the timer off (KMD 328 behaviour).

Timer lifetime (review fix): the watchdog timer, unlike the heartbeat, has no KTIMER fallback, so its pointer must never
read 0 while a callback can run. The callback loads it once and returns on 0; `delete_vsync_ex_timer` publishes 0 only AFTER
`ExDeleteTimer(wait)` returned (which cannot return while a callback runs), for both timers. Before `ExDeleteTimer` it writes
`StopSub` 23 (`stop_sub::REMOVE_TIMER`, written between 21 and 22): a heartbeat callback blocked in
`DxgkCbSynchronizeExecution` hangs that wait with no timeout, and `StopSub` 23 with `VsCbIn` above `VsCbOut` would name it.

### 19.4 New breadcrumbs (all in the heartbeat block, written by `publish_vsync_ticks`)

| value | meaning |
|---|---|
| `VsLiveT` | interrupt time (ms) the heartbeat block was written (by the 2 s ten-value write or a full write); EVERY value below and `VsTickN`/`VsTickT`/`VsRevN`/... is as of the last full write, and the ten live ones (`VsLiveT`, `VsTickN`, `VsTickT`, `VsCbIn`, `VsCbOut`, `VsWdTkN`, `VsWdTkT`, `VsWdAgeMs`, `VsWdFixN`, `VsWdHungN`) as of `VsLiveT` |
| `VsCbIn`, `VsCbOut` | tick callbacks entered / returned (either timer source; NEVER zeroed). `VsCbIn` > `VsCbOut` for more than a tick is a blocked callback |
| `VsCbSyncB`, `VsCbSyncOk` | `DxgkCbSynchronizeExecution` calls the tick began / returned (any status); `B` > `Ok` is a hung sync |
| `VsCbSyncSt`, `VsCbSyncT` | status of the last return; interrupt ms the last sync began |
| `VsWdTkN`, `VsWdTkT` | watchdog timer ticks, time of the last |
| `VsWdAgeMs` | the heartbeat's silence the watchdog saw at its last tick (0 = not armed / unknown) |
| `VsWdFixN`, `VsWdHungN`, `VsWdPubN` | re-arms done, blocked callbacks found, worker refreshes asked for |
| `VsWdOn`, `VsWdNoTm`, `VsWdTmEff` | watchdog armed; no timer could be allocated; `VsWdTimer` in force |
| `VsWdSAt`, `VsWdSArm`, `VsWdSRef`, `VsWdSDl`, `VsWdSAge`, `VsWdSCbI`, `VsWdSCbO`, `VsWdSSyT` | what it saw the last time it acted: when, armed flag, reference and pending deadline (ms), silence, callback counts, when the last sync began |

### 19.5 Not done

The tick callback was not changed to hand the synchronized notify to a DPC or work item: hypothesis H1 is not supported
by the code (the re-arm precedes the call) nor by the LiveKD of the second stop. If `VsCbIn` > `VsCbOut` or `VsCbSyncB` >
`VsCbSyncOk` is ever read, that becomes the fix.

### 19.6 What to read next on hardware

1. **Live state, not the mirror.** With LiveKD, twice, a second apart: `dd helios_kmd_render!*VS_TICKS*` (the symbol is
   in `ddi::stall_diag`; `x helios_kmd_render!*VS_TICKS*` finds it) or, from the adapter, `vsync_count` and
   `vsync_last_100ns`. Moving = the heartbeat is alive and H0 is the answer. Also `!timer` and look for the Helios Ex
   timer's expiry; the kernel's high-resolution timer list shows an armed one-shot.
2. With the v329 build: wait 5 s on an idle desktop and read `VsLiveT` against the uptime (it must be under 3 s old),
   then `VsTickT` against `VsLiveT` (within a period = alive), `VsTickN` twice.
3. If `VsTickT` is old against `VsLiveT`: `VsWdFixN`, `VsWdHungN`, `VsWdAgeMs`, `VsWdS*`, `VsCbIn` vs `VsCbOut`,
   `VsCbSyncB` vs `VsCbSyncOk`, `VsCbSyncSt`, `VsDisN`, `VsCanN`, `VsEarlyN`, `VsExhN`, `VsArmN`, `VsWdOn`, `VsWdNoTm`.
   `VsWdFixN` above 0 is a lost one-shot the old code left dead; read `VsWdSArm`, `VsWdSRef` and `VsWdSDl` for the state it
   found.
4. `HpdLoopN` rising about every 2 s with `HpdSgOth` is the watchdog's refresh wake.

## 20. DWM after a device restart (flip retirement; v329 follow-up)

### 20.1 The report

KMD 327.1 (`VsPowerMode` 1, `VsWatchdog` 1, `ForeignFlip` 0), a Venus `d3d11_spin` window composed by an animating DWM,
`pnputil /restart-device`. After the restart the mode is kept (5120x1440@240) and the heartbeat runs (section 19: the
counters had been a lazy mirror; the ticks ran at about 238.8 Hz), but DWM does not compose. The UMD's log of both DWM
processes (1772, then a freshly started 1808): `Evict` returns DEVICE_REMOVED, DestroyDevice / CloseAdapter,
OpenAdapter10_2 and CreateDevice succeed (new Venus contexts on the restarted KMD, same LUID), then about 60 flip presents
(`Present1` flags 0x2, interval 1; the render and present callbacks return S_OK), then DWM sits at 0 CPU with no error.
The fresh DWM does the same after 64. `VpPres` / `VpFlip` and `FlipIss` stop moving. Second capture: `VpGate` 1,
`VpVsEn` 1, `VpPend` nonzero (low 32 bits of an allocation handle), `HpdLoopN` 424, `HpdSite` 1, `FsLive` 0.

So DWM's own recovery works and it is the NEW generation's allocations that are flipped. dxgkrnl accepts the presents
but stops issuing flips (`FlipIss` flat), which, with `MaxQueuedFlipOnVSync` 1 (`FlipQueueN`, default 1), means the
flip it issued LAST is not seen retired and everything queues behind it.

### 20.2 Ranked hypotheses

1. **H1, the restart zeroed the address every CRTC_VSYNC carries (fixed, 20.3).** dxgkrnl retires a queued flip only
   when a CRTC_VSYNC carries ITS address (`signal_crtc_vsync`, `adapter/kobj.rs`, reading
   `AdapterContext::last_primary_address`) and issues the next flip after that (depth 1). It keeps its flip queue and
   VidPn state across a PnP stop/start. `reset_display_publication_state` (`adapter/mod.rs`, called from StopDevice and
   again from StartDevice, `ddi/lifecycle.rs`) stored 0 in `last_primary_address`. A flip dxgkrnl had issued and not yet
   seen retired when the device stopped (near certain with an animating compositor) was therefore never named again:
   the restarted heartbeat reported 0 until a NEW flip had been programmed, and a new flip is not issued while the old
   one is outstanding. This fits every observation: the ticks run, `VpVsEn` 1, `FlipIss` flat, a fresh DWM stalls the
   same way after the same count (dxgkrnl's own present queue, 60 to 64 deep, fills and blocks the compositor), a
   static desktop restarts fine (no flip in flight), nothing failed in the KMD (nothing was ever asked of it).
   Counters: `SaLo` / `SaHi` and `VpLpa` read 0 after the restart; `ScRestAdr0` (the heartbeat's address at StopDevice)
   and `ScRestIss` nonzero while `ScRestAddr` was 0 before the fix and equals `ScRestIss` after it; `FlipIss` frozen.
   NOT proven on hardware: that dxgkrnl waits for exactly the newest issued address (it is the address of the flip it
   issued last, which is what `note_flip_issued` records; if that flip was already retired the report is the harmless
   "this is what is displayed").
2. **H2, a flip that no later step of the KMD can complete (partly fixed, 20.3).** The completion invariant (section 13)
   covered foreign and hollow sources. A handle that no longer resolves at the worker (`scanout_alloc_info` None:
   destroyed, or an allocation of an older transport generation: `ScanoutReject::BadAlloc`) and a Venus
   `ProducerAbandoned` (the host resource is not live, a destroy barrier is up, the exact producer boundary was purged)
   completed nothing, because `flip_completion::decide` answers `None` for a Venus source and `flip_completion_info`
   returns `None` for an unresolved handle. At `SetVidPnSourceAddress` itself an unpaired handle already published
   (`FkDdi`, `KeepWhy::Unresolved`), so only the worker's exits were open. Counters: `FkStale` (new), `FkGen` (new, a
   subset of `FkDdi` for handles of an older generation), `FkKeep05` / `FkKeep08`, `VpPrF`, `PgStale`, `PrUnres`.
3. **H3, the worker's programming Deferred for ever (not a restart defect in itself; unproven, instrument in place).**
   `VpGate` 1 with `VpPend` nonzero is what a Deferred re-arm leaves (`apply_deferred_vidpn_source_address_locked`
   re-stores the handle and keeps the gate), and the second capture had it. Causes: `stage_worker_scanout_bind`
   `Waiting` (`virtio/gpu/mod.rs`: a publication active, a fast owner, the producer boundary not ready) or the linear
   fallback's `publication_active()` (`ddi/display.rs`, `program_vidpn_source_inner`). The mirror is only as fresh as the
   worker's last dump (every 128 passes and 1 s), so a nonzero `VpPend` at a quiet moment is also what one flip issued
   after the last dump looks like. Discriminate with the 14.5 rows 1c and 2b: `HpdLoopN` rising at about the vsync rate
   with `VsPendN` growing and `FlipPub` flat is a Deferred loop; `DeferBudget` 240 as the A/B (`FkDefBud` moves and the
   stall clears). Under H1 nothing new is issued, so `VpPend` is 0 or one handle and `VsPendN` stays small.
4. **H4, a stale fence high-water (examined, refuted by the observations).** `last_completed_fence` (`adapter/mod.rs`) is
   never reset at a start. If dxgkrnl restarted its fence numbering at 1, every `signal_dma_completed` would be skipped
   as stale (`DmStl` counts them: `submit_command.rs`, `fence_is_forward`) and every fence, paging included, would
   stall. But the new generation's contexts were created, made resident, rendered and presented about 60 times, which
   needs completions. `DmStl` stays on the checklist to close it.
5. **H5, resource id collision across generations (examined, refuted).** Ids restart at 1. Every identity that could
   make the new generation's `already_bound` / `same_active_identity` true by accident is cleared by
   `reset_display_publication_state` at StopDevice and StartDevice: `active_scanout_resource`, `active_scanout_wh`,
   `host_bound_scanout_resource`, `dedicated_scanout_*`, `primary_scanout_*` (the generation is bumped), the bind sequence
   pair and `scanout_bind_wire_resource`, the leases and epochs, `frame_watermark_*`, the read ledger; the host-side state
   belongs to the new `VirtioGpu`. `same_active_identity` also requires `already_bound`. An allocation handle of the old
   generation is refused by `resolve_current_alloc` (`is_current_generation`).
6. **H6, CommitVidPn not re-issued (observed, not a flip blocker).** `ModeStg` 5 after the restart: the last mode DDI
   entered was `EnumVidPnCofuncModality`, no `CommitVidPn` (7 / 8). dxgkrnl keeps the active VidPn across a stop/start.
   The only thing the KMD keeps from a commit is `committed_refresh_mhz`, which StartDevice zeroes: the heartbeat then
   follows the host's preferred rate until a commit. The ticks ran at 238.8 Hz, so it was harmless here; a user-chosen
   rate other than the host's would be lost. Not changed.
7. **H7, handles dxgkrnl passes after the restart (refuted).** The DDIs take the adapter context pointer from AddDevice;
   `publish_started` REPLACES the boxed `StartedState` at every StartDevice, so the callback table and `DeviceHandle` that
   the heartbeat and the notifications use are the new ones. The heartbeat's notify status is `VsCbSyncSt`.

### 20.3 What changed

* **The heartbeat's address survives the restart (v329: statics only; image reload found on hardware, then persisted).**
  `ddi::stall_diag::LAST_ISSUED` records the newest address dxgkrnl ever
  issued in a flip (`note_flip_issued`: every `SetVidPnSourceAddress` and every DMA flip record; process-lifetime,
  `start_generation` leaves it alone). The v329 design assumed a process-lifetime static survives
  `pnputil /restart-device`. That was WRONG: the restart RELOADS the driver image (hardware: `StartN` is 1 after each
  restart, `EntHpdN`, `EntHpdTh` and `EntVsTk` are 0 at the start), so every static is zero at the new start, the seed was
  inert and `ScRestAdr0` / `ScRestIss` read 0. The restart passed 5/5 on 328.1 and 330.1 without the seed; the persisted
  seed (20.3a) is robustness work, not a fix of an observed failure. `reset_display_publication_state` stores `restart_flip::seed_address(LAST_ISSUED)` in
  `last_primary_address` instead of 0, at its start and again at its end (the lease teardown can publish a withheld
  old-generation address over it). Only the ADDRESS word is kept: the displayed identity, the binding, the gate, the
  pending slot and every resource-id keyed table are still cleared. A new flip's programming still publishes its own
  address through `publish_bound_primary` / `publish_kept_primary` as before.
* **Dead-source flips complete whatever their class.** `ddi/display.rs::complete_dead_source`, from the worker's
  permanent-reject exit and from the inline wrapper: `BadAlloc` (handle unresolved) and `ProducerAbandoned` publish a kept
  picture (`restart_flip::worker_dead_exit`: the allocation's own address if it still resolves, else the newest issued
  flip's), for Venus, foreign and hollow alike, counted `FkStale` and under `FkKeep08` (unresolved) / `FkKeep05`
  (rejected). The status returned to dxgkrnl is unchanged. `SetVidPnSourceAddress` counts an unpaired handle of an OLDER
  generation as `FkGen` (`create_allocation::alloc_is_stale_generation`) in addition to `FkDdi`.
* **StartDevice / StopDevice breadcrumbs and wake.** `ScRestPend` (bits 0 and 1: a programming handle was pending / the
  gate was raised at StopDevice, after the worker and heartbeat stopped and before the reset; bits 2 and 3 the same at
  StartDevice entry), `ScRestAdr0` (the heartbeat's address at StopDevice, low 32 bits), `ScRestAddr` (at StartDevice
  exit: the seed), `ScRestIss` (the newest issued address), `ScRestHi` (their bits 32..39: stop 16..23, issued 8..15,
  exit 0..7), `ScRestSig` (worker wakes StartDevice owed). The wake is one `signal_hpd` at the end of StartDevice when
  `restart_flip::needs_worker_signal` (the reset clears both, so it fires only for a programming raised while the start
  ran). The `ScRest*` values are never zeroed by `start_generation`: they describe the restart itself.
* **The seed survives an image reload (20.3a).**
* Pure logic and tests: `kmd_logic/src/restart_flip.rs`; the counter lists are `stall_diag::COUNTERS` (`ScRest*`,
  `RestSeed*`; the persisted words are spelled once in `restart_flip`) and `flip_completion::COUNTERS` (`FkGen`, `FkStale`).

### 20.3a The persisted seed (`RestSeed`, default 1)

The newest issued flip address is kept in the SERVICE KEY (the key of the other knobs and counters; a hive value outlives
an image reload, a static does not). `RtlWriteRegistryValue` writes DWORDs here, so the 64-bit address is two DWORDs:

| value | content |
|---|---|
| `RestIssLo`, `RestIssHi` | the address, low and high dword |
| `RestUpS` | interrupt time in whole seconds when it was written (`KeQueryInterruptTimePrecise`; zero at every boot) |
| `RestChk` | check word over the three (`restart_flip::persist_check`), written LAST, so a write torn by a crash reads as damaged |

*Written* (PASSIVE only, never on the flip path; `ddi::stall_diag::persist_rest_seed`): (1) at the top of StopDevice,
before the first hive flush (`StopFlush`), (2) again in `note_stop_entry` (after the worker and heartbeat stopped, before
`reset_display_publication_state`; the later `stop_flush` stages cover it), and (3) from the HPD worker's every pass
(right after `publish_live_if_wanted`), when `LAST_ISSUED` changed and at least 2 s passed since the last write
(`restart_flip::persist_due`; two loads and a compare otherwise). A crash, bugcheck or unclean stop therefore leaves a value at
most about 2 s old (the lazy hive writer decides when it reaches the disk; the stop flushes force it). Only a sane address
is written: nonzero, page aligned, below 2^52 (`sane_address`).

*Read* (`load_rest_seed`, from `note_start_entry`, the first thing StartDevice does with it: before `ScRestIss` is
captured, before the first `reset_display_publication_state` and before the heartbeat starts) with max-of semantics
(`restart_flip::choose_seed`):

| `RestSeedUse` | meaning | seed |
|---|---|---|
| 0 | knob off, or nothing persisted and no static | the static (0 with the knob off after a reload) |
| 1 | the persisted address was used and stored into `LAST_ISSUED` | persisted |
| 2 | rejected: stale. `RestUpS` is later than this boot's uptime, so the value is from an earlier boot (dxgkrnl's flip queue is empty at boot; a stale address is harmless but pointless) | none |
| 3 | rejected: insane (unaligned, bit 52 or above, or `RestChk` does not match: torn or hand-edited) | none |
| 4 | the image was NOT reloaded, `LAST_ISSUED` is nonzero: the static is used, the persisted value was not consulted for the seed | static |

A rejected value (2, 3) is erased (all four words zeroed) so a later, longer boot cannot accept it by its uptime. The boot
test is the monotonic interrupt time, as there is no boot id: a value written in an earlier boot whose uptime was SHORTER
than the new driver start's reads as fresh. That is the harmless case (a stale address names no flip; the heartbeat reports a
picture's address that dxgkrnl is not waiting for), not a stall. Hibernate keeps interrupt time running and a restart is not
a boot, so a restart after hibernation is accepted as it should be.

*Mirrors*, written at EVERY StartDevice, zero included, in the `ScRest*` block (`publish_restart`): `RestSeedEff` (the knob
in force), `RestSeedLo` / `RestSeedHi` (the persisted address as read, 0 with the knob off), `RestSeedUse` (above).
`RestSeed` 0 is exactly the v329 behaviour: nothing is read, written or fed to `LAST_ISSUED`.

### 20.4 State that survives StopDevice / StartDevice: the audit and the decisions

CORRECTION (hardware): the table below was written assuming that process-lifetime statics survive
`pnputil /restart-device`. They do not: the restart reloads the image and every static is zero at the new start
(`StartN` 1, `EntHpdN` / `EntHpdTh` / `EntVsTk` 0). Rows that say KEPT or LEFT for a static therefore mean "kept across a
StopDevice / StartDevice pair of the SAME image" (a stop and start without an unload); after an image reload they are zero,
and the only state that crosses it is the service key (the persisted seed above, the knobs and the counters).

Every process-lifetime static and `AdapterContext` field that holds scanout, flip, present, fence, vsync, retry, epoch,
lease, bind-sequence, producer-stream or generation state was read for what StopDevice / StartDevice does to it
(`kmd_render/src`, line numbers of the tree before this change). Nothing found makes a NEW-generation
`SetVidPnSourceAddress` or worker programming Deferred, Superseded or refused for ever, and no stale static suppresses a
worker wake. Decision column: RESET (already), CHANGED (this section), KEPT (on purpose), LEFT (unreset, judged harmless).

| state | where | at a restart | decision |
|---|---|---|---|
| `last_primary_address` (the CRTC_VSYNC address) | `adapter/mod.rs:706`, zeroed at `:1403`, read `adapter/kobj.rs:898` | was zeroed at Stop and Start | CHANGED: seeded from `LAST_ISSUED` (H1) |
| `FLIP_WORD`, `DONE_SEQ`, `FLIP_ISS`, `FLIP_PUB` | `ddi/stall_diag.rs:271,316-317` | zeroed in `start_generation` (the only record of the newest issued address was lost) | RESET; the address now also lives in `LAST_ISSUED`, which is KEPT |
| `vidpn_programming`, `pending_vidpn_allocation` | `adapter/mod.rs:826,712` | zeroed by `reset_display_publication_state` | RESET; the state at the Stop edge is now recorded (`ScRestPend`), and a surviving programming wakes the worker |
| `active_scanout_*`, `host_bound_scanout_resource`, `dedicated_scanout_*`, `primary_scanout_*` (generation bumped), epochs and leases, bind-sequence trio, `frame_watermark_*`, `scanout_refresh_pending`, `scanout_flush_inflight`, read ledger | `adapter/mod.rs` `:1388-1470`, `adapter/read_ledger.rs:508` | zeroed | RESET (H5) |
| `committed_refresh_mhz` | `adapter/mod.rs:683`, `lifecycle.rs:319` | zeroed at Start | LEFT (H6) |
| `last_completed_fence` | `adapter/mod.rs:553`, read/written `adapter/locks.rs:143-150`, `ddi/submit_command.rs:874-904` | NOT reset | LEFT: if dxgkrnl keeps its VidSch node the carried value is right, and zeroing a value dxgkrnl has seen risks bugcheck 0x119 (`submit_command.rs:1526`); `DmStl` closes it (H4) |
| `ForeignScanout` `STATE` / `FENCES`, scanout-release book | `adapter/foreign_scanout.rs:47,53`, `virtio/scanout_release.rs:38` | `foreign_scanout_reset` | RESET; its `seq` is KEPT (the host must never see it go back) |
| `foreign_flip` statics (`KNOB`, `WINDOW`, `FLYING`, `FAIL_UNTIL`, `REPEAT_GATE`, ...) | `virtio/foreign_flip.rs:90-249` | `forget()` from `retire_transport` | RESET; `drain_blocked()` cannot latch |
| `rm_client`, `rm_present`, `sysmem`, `sysmem_flip` | `virtio/rm_client.rs:92,101` | `rm_client::forget` | RESET |
| `RETRY_HANDLE`, `RETRY_ATTEMPTS` | `ddi/display.rs:2889-2890` | cleared at Start only (`lifecycle.rs:538`) | LEFT: heap-pointer key, budget 4, a collision costs one early GaveUp (which completes kept) |
| `DEFER_HANDLE`, `DEFER_ATTEMPTS`, `DEFER_BUDGET` | `ddi/stall_diag.rs:842-843` | zeroed at Start; budget 0 = unlimited | RESET |
| `ProducerCompletion` table | `adapter/producer.rs` | `start_transport` bumps the generation embedded in every stream key; the table is cleared when the old transport drops | KEPT (pages and generation counter on purpose) |
| `SCANOUT_ALLOCS` (32 slots) | `ddi/create_allocation.rs:1602` | not cleared | KEPT: a new-serial allocation takes over a stale id (`:1620-1627`), a stale destroy withdraws by handle (`:2294`) |
| `TRANSPORT_SERIAL`, `NEXT_WIRE_FENCE_BASE`, `scanout_timeline` ring | `adapter/mod.rs`, `virtio/gpu/mod.rs:1763` | monotonic | KEPT |
| `scanout_retire_wanted` coalescing | `adapter/mod.rs:782`, `ddi/submit_command.rs:1084`, `ddi/hpd.rs:272` | zeroed at Stop | LEFT: a submit during StartDevice can have its single signal eaten by the worker's start-edge wait, which delays only the windowed-Blt retire edge, not flips |
| `hpd_exited`, `config_change_pending`, `pending_refresh_resource`, `frame_watermark_fence`, `PUMP_BUSY` / `PUMP_AGAIN`, `WDDM_HEAD_DEADLINE_100NS` | `adapter/mod.rs`, `adapter/foreign_scanout.rs:56-57`, `virtio/gpu/mod.rs:495` | not reset | LEFT: one extra indication, one refused refresh, one spurious DPC |
| `flip_keep` (incl. `MIRROR_PENDING`), `present_foreign`, `scanout_trace`, `stall_diag` counters | `lifecycle.rs:219,220,541`, `stall_diag::start_generation` | zeroed | RESET; diagnostic only. `ScRest*` are NOT zeroed (they describe the restart) |
| `vsync_enabled`, `VsPowerMode` / `VsWatchdog` / `VsIdleWake` | `adapter/mod.rs:676`, `ddi/stall_diag.rs:342-344` | start forces the gate open; knobs re-read | RESET; `VsPowerMode` 0 quiesces the heartbeat on any non-D0 call (not restart specific) |

The doc comment on `reset_display_publication_state` says `pnputil /restart-device` re-runs AddDevice and allocates a fresh context;
section 19 says the context is kept (the Ex timers and the adapter survive). The two cannot both hold. `StartN` rising while
`EntArm`, `EntHpdTh` and `EntVsTk` show the old generation's heartbeat and worker in the statics, together with a matching `HpdN`, says
the context is reused; the fix is correct either way (`LAST_ISSUED` is a static). Hardware then showed `StartN` 1 and the
`Ent*` counters 0 at the start of every restart: the image IS reloaded, and the audit's conclusions about statics hold
only for a stop and start without an unload (20.3a closes the one that matters, the newest issued address).

### 20.5 Hardware checklist

After `pnputil /restart-device` with a Venus `d3d11_spin` window and DWM running (1920x1080 low rate first, then
5120x1440@240):

1. `VpPres` keeps rising, DWM's CPU time moves, the spin window animates, `FlipIss` and `FlipPub` rise together.
2. Read `ScRestPend`, `ScRestAdr0`, `ScRestAddr`, `ScRestIss`, `ScRestHi`, `ScRestSig`: with an animating compositor
   `ScRestAdr0` and `ScRestIss` are nonzero and `ScRestAddr` equals `ScRestIss` (low 32 bits). On a pre-fix image
   `ScRestAddr` read 0 and that is the H1 signature.
3. `SaLo` / `SaHi` and `VpLpa` just after the restart equal `ScRestIss` until the first new flip is programmed.
   With the persisted seed (`RestSeed` 1, the default), after `pnputil /restart-device` of an image that issued flips
   (the image is reloaded: `StartN` 1): `RestSeedEff` 1, `RestSeedLo` / `RestSeedHi` the persisted address (the same as
   `RestIssLo` / `RestIssHi` in the key), `RestSeedUse` 1, `ScRestIss` NONZERO (it was 0 on 328.1 / 330.1: the seed was
   inert), `ScRestAddr` equal to `ScRestIss` (low 32 bits) and `SaLo` / `SaHi` / `VpLpa` equal to the seed until the first
   new flip. `RestSeedUse` 0 with nonzero `RestIssLo` means the address was not read (knob off); 2 is a boot in between
   (expected after a reboot, erased); 3 an address that is not page aligned or damaged (check `RestIssLo`, `RestChk`);
   4 an image that was not reloaded. A reboot, then a first start: `RestSeedUse` 0 or 2 and `ScRestIss` 0. With
   `RestSeed` 0 the run is the v329 behaviour (`RestSeedUse` 0, `ScRestIss` 0 after a reload).
4. `FkKeep`, `FkStale`, `FkGen`, `FkKeep05`, `FkKeep08`, `PBRetSite`, `VpPend`, `VpGate`, `VpPrF`, `PgStale`: small or 0.
   A nonzero `FkStale` after a restart means H2 was also live.
5. If DWM still stalls: `VsPendN`, `HpdLoopN`, `HpdSite`, `FlipIss - FlipPub - VpCoal` (14.5 rows 1c, 2, 2b), `DeferBudget` 240
   as the A/B (H3), `DmStl` (H4), `VsCbSyncSt` (the heartbeat's notify status) and, before any recovery, `cdb -pv` stacks
   of the stalled DWM plus the v328 rings (`Dx` / `Dd` / `Dz`, `Lost*`, `PgLast*`).
6. Regression: a restart with an idle desktop, with the NVK spin app (`ForeignFlip` 0 and 1) and a plain boot
   (`ScRestAddr` 0 on the first boot, where nothing was ever issued).

### 20.6 Risks and what is not verified

A kept or re-reported address names a picture that is not on the screen; dxgkrnl retires the flip and the screen shows the
previous contents until the next programming (13.4). If the first new flip's address equals the seeded one (the segment
allocator can hand the same address out again) dxgkrnl retires it a moment before it is programmed: a stale frame, not a
stall. If dxgkrnl does not wait for the address of the newest issued flip (H1 wrong), the seed is inert (it reports an
address that matches no flip, exactly as 0 did) and the stall has another cause: H3 and H4 are the next reads. Verified:
host tests of `restart_flip`, the whole `kmd_render` through the stub harness with the error set identical to the base.
NOT verified: anything on hardware, the WDK build, that dxgkrnl keeps its flip queue across the restart.

The persisted seed (20.3a) is verified by host tests of its pure decisions (`restart_flip`: the choice table, the sanity and
torn-write checks, the persist rule) and by the stub type-check of the whole `kmd_render`; NOT on hardware or the WDK. Open
points: the flip addresses must be page aligned for the persisted value to be written (an address that is not never reaches
the key and `RestSeedUse` stays 0: read `RestIssLo` and `ScRestIss`); the registry writes add about half a millisecond each on the
worker's pass (four, at most once per 2 s while flips change the address) and at StopDevice; `RtlWriteRegistryValue` on the
service key may reach the disk late, so a bugcheck within the lazy writer's interval loses the newest value (the stop
flushes cover a clean stop).

## 21. Default flip: `VsPowerMode=1`, `VsWatchdog=1` (v330)

Hardware acceptance on 328.1 (5/5 device restarts, `ForeignFlip` 0,1,0,1,0, `VsPowerMode=1`,
`VsWatchdog=1`, an NVK `d3d11_spin` on scanout across each `pnputil /restart-device`): the mode stayed
5120x1440@240, the same DWM survived with no kill, `VpPres` +230..236 per sample, all breadcrumbs nominal.
On 327.1 (both knobs 0) the same DWM never re-opened the adapter after a monitor-child D3 and
froze. From v330 the defaults are `VsPowerMode=1` (only the ADAPTER leaving D0 quiesces the heartbeat)
and `VsWatchdog=1` (revive an armed but silent heartbeat). `VsIdleWake` stays 0; `VsWdTimer` stays 1.
Both old behaviours remain selectable (`VsPowerMode=0`, `VsWatchdog=0`), re-read at every StartDevice
(prefer a VM reboot when changing them). The knob tables in 13.8 and 15.4 list the old defaults and
are superseded by this section.
## 22. Already-on-scanout present tag

Status: KMD half implemented (`kmd_logic::onscanout`, `kmd_render/src/ddi/onscanout.rs`, hooks in `display.rs`,
`submit_command.rs`, `device.rs`, `virtio/foreign_scanout.rs`); the UMD half is written (branch
`fix/ffxiv-basemark-nvk`, d812a49; NVK patch 0046 adds `scanout_frame()` returning `out_seq` / `out_generation`) and is
not yet run against this KMD on hardware. Written on v326, merged onto v331 (v331 tip: the doc sections 15 to 21 are
other incidents, hence section 22). Wire layout:
`protocol/src/onscanout.rs`, C mirror `protocol/include/helios_onscanout.h`. The pure logic is host-tested; the glue is
type-checked against the stub harness only, never compiled for the WDK and never run on Windows.

### 22.1 The problem, and which arm it is

A producer that shows its frames through the user foreign-scanout source (`SCANOUT_SET` / `SCANOUT_PRESENT`) has
already put the frame on scanout, but a D3D11 `PresentImpl` waits on the frame-latency semaphore that only a per-frame
`pfnPresentCb` releases, so the UMD presents anyway. The arm `DxgkDdiPresent` runs is decided by `DXGK_PRESENTFLAGS`
alone (`display.rs`: bit 2 `Flip` clear is the Blt arm; bit 2 set with no DMA buffer is the MMIO flip, with one the DMA
flip). What dxgkrnl sends for which swap chain is its choice and is not provable from this tree; what the tree
establishes:

* **Legacy blit model, windowed (and a flip-model chain the runtime degrades to a blit): the Blt arm** (`Flags.Blt`,
  `PBflag` bit 0). Source = the app's DXGI surface, destination = the window's redirection surface (DWM's: a
  `PitchedStandardBuffer` or an OPTIMAL image). This is the per-frame cost.
* **Flip-model swap chain composed by DWM: the app's frame is not a Blt.** dxgkrnl redirects a composed flip-model
  present to DWM (the app's buffers are DWM's inputs); the KMD sees DWM's own flips (Flip arm), not a per-frame app Blt.
  A Blt per frame therefore means the chain is on the blit path. **Independent / direct flip** (borderless fullscreen
  promoted by dxgkrnl) reaches the Flip arm, MMIO or DMA (`flip_route`: `Arm` for a direct-scanout or a
  `ForeignFlip`-registered allocation, `Skip` = the counted keep of sections 12 and 13, `Fail` for an ordinary
  unregistered one). A flip copies nothing; its cost is the programming (`arm_dma_flip` / `SetVidPnSourceAddress`) and
  its completion invariant (section 13), which must keep running.

Which one FFXIV hits is read, not assumed: `PBflag` (bit 0 / bit 2), `PrFgBlt` / `PrFgFlip`, and `OsRejWhy` 6 below (a
tag arrived on a flip).

**The full-frame Blt, concretely** (non-snapshot Blt arm with an adopted foreign source and `ForeignCopy` = 1; with it
0 the foreign source is a counted skip, `FcOff`, with no copy but still a failed import attempt per frame): import of
the NVK image into the KMD's own Venus device as an explicit-modifier dma-buf image, `vkCmdCopyImage` (or the BGRA
scratch blit for XBGR) of the WHOLE source into the destination (`SrcRect`, `DstRect` and sub-rects are ignored, the
extents must be equal), then for a standard-buffer destination a CPU wait on the GPU fence (`wait_fence`, up to 5 s),
`mirror_present_system_backing` (a CPU copy of the whole surface into the paged-out MDL pages when the destination has
system backing) and the ownership hand-back, and the copy's wire fence merged into the DMA fence. 5120x1440x4 is 29.5
MB read and 29.5 MB written per frame plus a host round trip. While a user source is live none of it is visible: the
source withholds the desktop's host flush (`foreign_scanout_suppresses`).

### 22.2 The tag, and where it travels

It does NOT travel in `pfnPresentCb`'s `pPrivateDriverData`: dxgkrnl does not forward that to `DxgkDdiPresent` (`PBIdOk`
= "no payload" across three driver generations; the D4b snapshot and the stream marker had to move to the Render command
for the same reason). The carrier is the **`HERF` command the UMD already submits with `pfnRenderCb` immediately before
`pfnPresentCb`, on the same `hContext`** (`MarkerPresent`, `umd/src/forward/present.rs`), extended by a tail.
`DxgkDdiRender` parses it and stashes it on the context; the Present that follows on that context takes it (read and
clear, on every Present, whatever its arm: the same pairing and orphan bound as the stream marker). Every `HERF` Render
replaces the stash, so a tag never reaches a later Present than its own.

All little-endian. `CommandLength` of the Render must be **72** (not 32 or 48):

```text
offset  size  field
  0      32   HeliosPresentRefreshCmd  'HERF' v1, the stream tail as ever (ctx_id, value, cookie)
 32      16   HeliosRmFenceTail        all zero unless an RM fence is attached (rm-fence-marker.md)
 48      24   HeliosOnScanoutTag
   48    u32  magic        0x43534F48 ('HOSC': bytes 48 4F 53 43)
   52    u16  version      1
   54    u16  flags        0 (nonzero is rejected)
   56    u64  sequence     the out_seq SCANOUT_PRESENT returned for THIS frame, nonzero
   64    u32  generation   the out_generation SCANOUT_SET returned for the live source, nonzero
   68    u32  resource_id  the Blt source's Helios resource id, or 0 = not stated
```

```c
struct HeliosOnScanoutTag { uint32_t magic; uint16_t version, flags; uint64_t sequence;
                            uint32_t generation, resource_id; };           /* 24 */
struct HeliosPresentRefreshCmdOnScanout { struct HeliosPresentRefreshCmdFence base; /* 48 */
                                          struct HeliosOnScanoutTag tag; };         /* 72 */
```

The KMD reads the bytes from offset 48 up to `CommandLength`: nothing there, or zero bytes, is "no claim" and costs
nothing; anything nonzero is a claim and is counted. The stream tail and the fence slot compose with the tag unchanged
(a CPU-complete `value == 0` marker, or an RM fence, still decides the present's boundary). An older KMD copies the 72
bytes into the DMA buffer, reads the 32-byte `HERF`, ignores the rest and does the ordinary Blt, so the UMD may always
send it; there is no capability bit (a delta in `OsSkip` is the proof). The DMA buffer must hold the 72 bytes
(`STATUS_BUFFER_TOO_SMALL` from Render is dxgkrnl's retry, as for any command).

UMD rules: take `generation` from the `SCANOUT_SET` reply and `sequence` from the `SCANOUT_PRESENT` reply of the frame
being presented; the presenting context must belong to a device of the SAME PROCESS as the device that issued them
(NVK's librmclient D3DKMT device and the UMD's runtime device are two devices of one process, which is what the check
compares: `hKmdProcess`); send the Render immediately before the Present, on the presenting context, with the usual
allocation list; tag only a whole-surface Blt (no `ColorFill`, no dirty rects) and never together with a windowed-Blt
snapshot.

### 22.3 Verification (a tag is a claim, never a fact)

The skip happens only when the KMD's own state backs every part of it (`kmd_logic::onscanout::verify`, checked in this
order; the first failure is the reason, `OsRejWhy`):

| code | reason | what the KMD checked |
|---|---|---|
| 1 | `BadMagic` | the bytes at 48 are nonzero and not `HOSC` |
| 2 | `Short` | `HOSC` but fewer than 24 bytes before `CommandLength` |
| 3, 4, 5 | `Version`, `Flags`, `Fields` | version 1, flags 0, `sequence` and `generation` nonzero |
| 6 | `NotBlt` | the Present is a flip, or has no allocation list or no Blt flag: nothing to copy, the flip machinery runs as ever |
| 7, 8, 9 | `ColorFill`, `SubRects`, `Snapshot` | a whole-frame Blt only: no fill, no destination sub-rects, no snapshot stash |
| 10 | `NoSource` | a live, unlapsed USER source exists (not the KMD's resident one) and has minted a frame |
| 11 | `Generation` | `generation` is that source's |
| 12 | `Owner` | the presenting context's `hKmdProcess` equals the process of the device that minted the source's frames; an unknown process on either side never matches |
| 13, 14 | `Ahead`, `Stale` | `sequence` is at most the newest `SCANOUT_PRESENT` minted for that generation (`Ahead` = a frame never minted) and at most 256 behind it (`Stale`) |
| 15 | `Resource` | `resource_id`, if nonzero, is the Present's source resource id |
| 16 | `Orphan` | a tag whose Present never came was replaced by the next Render |
| 17 | `Retry` | verified, but the Present's own preconditions refused it (DMA or private buffer, patch capacity); dxgkrnl retries without the tag and the retry is the ordinary Blt |

A rejected tag is the ordinary Blt, byte for byte: nothing of the default path changed (one relaxed load per Render and
per Present on a context that carries no tag). What a lying UMD can do: nothing without the live source, and with it,
the process that owns scanout 0 can already show anything on it; the loss is the update of its own window's
redirection surface for the frames it falsely tagged. Another process, a stale or forged generation, a sequence the
source never minted, an expired source or a flip: all rejected and counted.

### 22.4 What the skipped Present does

`present_blt_onscanout` (`display.rs`) is the legacy Blt arm minus the copy and nothing else: the arm's own
preconditions in its order and at its sites (DMA buffer holds the marker, private record holds the merge, patch
capacity: all before anything is done, so dxgkrnl's retry protocol is unchanged), then `present_blt_skipped`, the
completion a skipped foreign Blt already takes: fence-0 marker merged, patch references written, the KMD's DMA marker,
the stream boundary merged. So the Present completes at the point a Blt would for dxgkrnl (the DMA fence retires with
the packet, behind the producer's own boundary if the marker carries one), the frame-latency semaphore releases, and
the source is no longer read, which is why it can be reused at once. Nothing is begun that needs a release: no
destination `KmdWriter` / CPU-mirror ownership (`begin_present_buffer_write_legacy` is not called), no WindowedBlt token
(a snapshot never reaches this path, so the dead-ready-token class fixed in v323 has no way in), no read-ledger ticket,
no Venus import, no host call. The level 5 RM-primary Blt is skipped too (its copy is what is being avoided).

### 22.5 Counters (`Os`, at most 14 characters, unique across `kmd_render` and `kmd_logic`)

`OsTag` claims seen (a tag, well-formed or not; `OsTag = OsSkip + OsRej` when nothing is in flight), `OsSkip` presents
completed with no copy, `OsRej` claims not honoured, `OsRejWhy` the last reason (the table above), `OsWhyMask` every
reason seen this generation (bit `code - 1`), `OsBytes` MiB of copy avoided (the source's `pitch * height`), `OsLast`
the last honoured sequence (low 32 bits). Registry: the first skip, then every 256th; the first rejection, every new
reason and every 64th; the rest through `publish_nvrm_counters`; zeroed at StartDevice.

### 22.6 Hardware checklist (FFXIV on NVK with the tag; lowest mode first: 1920x1080 at 60 Hz, then bigger)

1. Before the tag (UMD without it): read `PBflag` (bit 0 Blt, bit 2 Flip), `PrFgBlt` / `PrFgFlip`, `FcImp`, and the
   Present's return time (`scanout_timeline` PRESENT_RETURN): the cost this removes.
2. With the tag: `OsTag` and `OsSkip` rise at about the frame rate; `OsRej` stays 0 in steady state; `OsBytes` per
   second is the frame bytes times the rate (5120x1440: about 28 MiB per frame). `OsLast` follows the sequence
   `SCANOUT_PRESENT` returned.
3. `OsRej` rising: read `OsRejWhy` and `OsWhyMask`. 6 = the present is a flip (the Blt hypothesis was wrong: report
   `PBflag`); 10 / 11 = the source is not live or the UMD's generation is stale (`FsLive`, `FsGen`); 12 = the UMD's
   presenting context is in another process than the one that issued the escapes; 13 / 14 = sequence bookkeeping; 15 =
   a wrong `resource_id`; 7 to 9 = the shape (dirty rects, snapshot).
4. No Blt cost: the Present's return time drops to the marker path; `FcImp` / `FcRefuse` stop moving; `PBSyWt` and
   `PBSyCp` do not appear; in WPR / PresentMon the Venus device's copy submissions drop to the desktop's own and
   `msBetweenPresents` follows the producer (about 247 fps in the reference run), not the Blt.
5. Release the source (`SCANOUT_RELEASE`, or kill the app): the tag is rejected with 10 from the next frame, the
   ordinary Blt resumes, and the desktop is restored by the usual flush (`FsRest`).

### 22.7 Verified, and not

Verified (host tests): the parse over every short length, a forged magic, version, flags, zero fields, a zero tail (not
a claim) and random bytes; the verdict for the exact claim, lag at and past the limit, a sequence ahead, a wrong
generation, a wrong or unknown process, no source or no minted frame, flips, ColorFill, sub-rects, a snapshot, a named
resource, and the order of the reasons; the per-source record's generation and monotonic rules; reason codes dense and
stable; counter names (at most 14, unique, nothing else in either crate writes an `Os` literal); the protocol layout
(sizes 24 and 72, offsets, magic bytes) and the C header's constants. Type-checked against the stub harness: the error
set equals the base's (a new error kind would have shown; an injected error in the new file is reported).

Review of the v331 merge: the record of what `SCANOUT_PRESENT` minted (`ddi::onscanout::note_minted`) is written by the
two escape-side callers only (`present`, `present_fenced`). It reads the minting device's object for its
`hKmdProcess`; the first version sat in `mint`, which the `ForeignFlip` worker and the KMD's presenter also reach with a
stored owner token whose device may already be destroyed (a read of freed memory). Those two never mint a user source.

NOT verified: anything on hardware or the WDK build; the arm FFXIV really hits (22.1); that `hKmdProcess` is the same
token for the producer's escape device and the presenting device (the premise of the owner check, as for stream
markers); that dxgkrnl treats the skipped Present's frame-latency semantics exactly as a copied one's (the completion
shape is a skipped foreign Blt's, which has run); that the runtime's command buffer accepts a 72-byte Render command.

Risks: (1) a window whose redirection surface is not updated while the source is live shows stale content when the
source ends, until its next honest Present (the desktop's restore flush covers the primary, not the window); (2) an
owner mismatch makes the optimization silently inert (counted, `OsRejWhy` 12), not wrong; (3) the sequence check is
about frames minted, not frames shown (a fenced flip may still be queued when its Present completes), which is
invisible by construction; (4) a tag on a flip is refused on purpose (a flip copies nothing and its completion
invariant must run), so a flip-path workload needs a different lever; (5) `ForeignFlip`'s resident source (DWM-on-NVK)
is not covered: its presents are flips.

## 23. RM fence carrier wedge (v333 incident; fix v335)

Built, host-tested for its pure half (`kmd_logic/src/wait_bound.rs`, `flip_pend_wd.rs`, `rm_fence_present.rs` `Gate`),
type-checked against the stub harness, compiled by nothing that links the WDK, run by nothing. Branch
`kmd/fence-carrier-wedge`.

### 23.1 The incident

Heaven DX11 fullscreen exclusive 5120x1440, NVK DWM with `ForeignFlip=1`, `HELIOS_NVK_RM_FENCE_PRESENT=1` for Heaven only
(carrier (b) of `docs/rm-fence-marker.md`). About 50 s of fast presents (present gate 105 us, 7680 presents), then the
screen froze. Heaven's `dxvk-queue` thread sat in `NtGdiDdDDIEscape`. After `Stop-Process -Force` Heaven stayed alive, the
screen stayed frozen, explorer logged an app hang, and `pnputil /restart-device` never returned. Only a VM reboot cleared it.

### 23.2 What the dumps actually say (read before the hypotheses)

Files: `kmd-1.txt`, `kmd-2.txt` (uptime 4 760 s and 4 765 s), `kmd-3-stuck-1913.txt` (uptime 4 987 s), `heaven-stacks.txt`,
`dwm-stacks-stuck.txt`, `umd-heaven.log`. `kstacks-stuck.txt` holds only the LiveKd banner: NO kernel stack of any
thread exists, and the user-mode stacks end at `wow64win!NtGdiDdDDIEscape`, so "blocked in the KMD" is an inference, not
a reading.

1. The stall block of `kmd-1` / `kmd-2` is a stale snapshot. `StallT` 4 700 746 against uptime 4 760 375 / 4 765 859:
   every value of the block (`FlipIss`, `FlipPub`, `HpdSite`, `ScLkN` ...) is 60 s old and identical in both reads
   because nothing wrote the block, not because nothing moved. Only the heartbeat block (`VsTick*`, `VsWd*`) moved. The
   block is written on request (worker, dump, escapes while the worker looks stuck); an idle worker and no escapes mean
   nobody requests (14.2). "Frozen in both reads 5 s apart" is therefore no evidence about the flip queue.
2. `FlipIss` 16267 against `FlipPub` 16261 is NOT six parked flips: `VpCoal` is 12 (handles dropped by coalescing; the
   healthy quiescent figure is `FlipIss - FlipPub - VpCoal` near 0, here -6, i.e. more publications than issues, as foreign
   flips publish more than once). `VsPendN` 0, `VpPend` 0, `VpGate` 0, `PrdPend` 0, `FfAsSub` = `FfAsAck` = 12281,
   `FsFQue` 0, `RmGAtt` = `RmGFire` = 7763, `RmGCan` 0, `RmGRef` 0, `NvEvLost` 0, `NvEvDrop` 0: the KMD held nothing.
3. `kmd-3` (block refreshed at 4 955 523, 256 s after the last flip): `FlipIss` 16267, `FlipPub` 16261, `FlipPubT`
   4 699 380 (unchanged: dxgkrnl issued no flip for 256 s), still `VsPendN` 0 and `VpGate` 0, `VpVsN` moving at
   240 Hz (the heartbeat reports the last address every tick). `DdiInflL` 0: NO DDI of ours in flight, so no escape was
   inside `DxgkDdiEscape`. `NStopDev` 0: the `pnputil` stop never reached `DxgkDdiStopDevice`.
   `NPreempt` 2 and `TPreempt` 4 699 382: `DxgkDdiPreemptCommand` was called twice, the last 2 ms after the last
   publication. (`kmd-1` / `kmd-2` carry `DdiInflL` = bit 30 = one Escape in flight at 4 700.7 s: that one, or the one the
   tester's tool made; it was gone by 4 955.)
4. `EscHwA` 0 and `EscNoSy` 0: no escape sets `HardwareAccess`, and NO escape sets `NoAdapterSynchronization`, so
   dxgkrnl serializes every escape against the adapter-level DDI synchronization before our DDI is entered.

### 23.3 Ranked hypotheses

| # | hypothesis | for | against | what would show it |
|---|---|---|---|---|
| H1 | dxgkrnl / VidSch wedge after the preemption at 4 699.382: the scheduler never resumed (no `SubmitCommand`, no flip, no `CreateDevice` for 290 s) and the escape (adapter-synchronized, `EscNoSy` 0) and the device stop queue behind a dxgkrnl lock, not behind ours | `NPreempt` 2 / `TPreempt` 2 ms after `FlipPubT`; `DdiInflL` 0 at 4 955; `NStopDev` 0; nothing pending in the KMD; `NResetTmo` 0 (TDR off or not reached) | the ack looks delivered (`DmaNtfF` 0, `WdSigF` 0, `DdiFailN` 0); the preempt request itself is unexplained | kernel stacks: `!process <Heaven> 7`, the thread `27fc.3194`, `!stacks 2 dxgkrnl`, `!stacks 2 dxgmms2`, `!stacks 2 helios_kmd_render`, `!locks`; the new `Pre*` breadcrumbs (23.6) |
| H2 | the preempt ack semantics: `preempt_flush` drops the whole pending WDDM FIFO (`virtio/gpu/mod.rs` `preempt_flush`), `DMA_PREEMPTED` carries `LastCompletedFenceId = last_completed_fence`; dxgkrnl resubmits the dropped buffers only when it schedules again, and nothing after the preempt was ever submitted | same as H1 | no counter of what was dropped (`AbnDrop` 0 in both reads: written by the PASSIVE flush, may be stale) | `Pre*` breadcrumbs: preemption fence, last completed, dropped count |
| H3 | an escape blocked INSIDE the KMD on a host round trip (`wait_block` 30 s `SYNC_ROUNDTRIP_TIMEOUT_MS`, a loop of them, the Venus ring wait up to minutes under the Venus mutex, the scanout mutex acquire without end) | `kmd-2` shows an Escape in flight at 4 700.7 | `DdiInflL` 0 at 4 955 (a 30 s round trip ended by then; a mutex wait would still be counted in flight) | `DdiInflL` bit 30 and `DdiOldMs` in the next stuck dump; `LkWaitN`, `VnLkHeldMs` |
| H4 | a lost RM fence fire holds a gate point (and the WDDM FIFO head behind it) for ever | the incident's path | `RmGAtt` = `RmGFire`, `RmGCan` 0, nothing pending; the 250 ms head rebase (`WdHeadEff` 250) already bounds a stream head | `RmGAtt - RmGFire` > 0 for more than 6 s |
| H5 | the early-fire note table (16 entries, `nvrm_fence::EARLY_CAP`) dropped a fire that raced a create | possible under many threads | `NvEvLost` 0, `NvFenceEarly` 29, `RmGEarly` 32 | `NvEvLost`, `NvFenceErr` |
| H6 | the flip queue itself parked behind a Deferred programming on a boundary that never completes | the task's premise | `VpPend` 0, `VpGate` 0, `VsPendN` 0 (at 4 700.7 and at 4 955) | `VsPendN` growing with `FlipPub` flat |

Whatever the cause, three facts make the damage worse than it has to be, and v335 fixes those without waiting for the
cause: a thread in a kernel wait cannot be killed, so one blocked escape makes the process unkillable; the device stop
then waits for what that thread holds; and a flip that stops being published is never retired by anything.

### 23.4 What changed

All of it acts only when something is already wrong (a thread being terminated, the device stopping, a flip stuck), and
every knob can restore v334 (0).

1. **The escape scope** (`ddi/escape_wait.rs`, `kmd_logic::wait_bound`). `dxgkddi_escape` registers the calling thread
   (table of 64, keyed by thread id) with a deadline of `EscWaitMs` (default 10000, 250..600000, 0 = no deadline) and
   refuses with `STATUS_DEVICE_NOT_READY` when the device is already stopping. Every wait the escape makes gives up when
   the thread is terminating (`PsIsThreadTerminating`), when the stopping flag is up, or when the deadline is spent. A
   thread that is NOT inside an escape (the HPD worker, a DPC, paging, a DDI that dxgkrnl runs on a terminating thread to
   clean up) is never aborted: `verdict` answers `None` for it.
2. **The waits**, each with its bound before and after:

   | wait | before | after (inside an escape) |
   |---|---|---|
   | `ctrl::wait_block` (every control-queue and NVRM round trip, `wait_fence`) | 1 ms .. 1 s slices to the call's total (30 s; `WAIT_FENCE_MAX_MS` 120 s) | slices <= 100 ms, each ends with the abort check; ends at the escape deadline; abort = the existing timeout path (`VirtioError::Timeout`, abandon, the escape's own `TIMEOUT` status) |
   | every retry loop charging a `Budget` (queue full, map busy, present-buffer write/teardown) | nominal ms, up to ~16x real | `charge_slice` reports spent on an abort |
   | the Venus ring wait (`ring_wait_until`, under the Venus mutex) | 30 000 sleeps of 1 ms, bounded by the real clock | the same, plus the abort check each round, WITHOUT latching the ring fatal (`Timeout`) |
   | Venus mutex acquire (`acquire_venus_mutex`) | infinite (5 s counted slices) | 100 ms slices, abort returns `NotStarted` through `with_venus_client`, the client is not touched |
   | scanout mutex acquire | infinite | `try_with_scanout_lifecycle` (RELEASE_BLOB path, snapshot status) gives up with `None`; `with_scanout_lifecycle` (callers that cannot fail) is unchanged |
   | content mutex (`PassiveMutex::lock`) | infinite | abortable, returns `None` |
   | RM client lease/sysmem waits, worker service | worker only | unchanged (never in an escape) |

   Not covered: waits inside DDIs other than Escape (`DxgkDdiPresent`'s scanout-lifecycle acquire, `Render`'s fence service).
   A thread blocked inside dxgkrnl itself (H1) is not reachable from the KMD at all.
3. **The stopping flag.** Set first thing in `DxgkDdiStopDevice` and `DxgkDdiRemoveDevice`, cleared at StartDevice
   (`escape_wait::reread_knobs`). Every scoped wait polls it at least every 100 ms, so an escape holding the Venus or
   scanout mutex releases it and the teardown that waits for it proceeds.
4. **The generic pending-flip watchdog** (`kmd_logic::flip_pend_wd`, knob `FlipPendWdMs`, default 500, 100..60000,
   0 = off). Every vsync tick: the newest flip dxgkrnl issued is not done, was issued at least that long ago, and no
   address was published for as long, then its address is published kept (`publish_kept_primary`, one atomic store,
   DISPATCH) and the flip marked done: `FlipPendWd`, `FlipPendWdT`, `FpWdMsEff`. Why `FlipWdogMs` would not have caught
   the incident: it counts only while a programming is pending (`VsPendN`), and `VsPendN` was 0; this one needs no
   pending state, only an unretired newest flip. The two share the record of the newest flip, so a flip is never
   published twice.
5. **Lost RM gate fires** (`kmd_logic::rm_fence_present::Gate::take_expired`, knob `RmGateMs`, default 6000, 1000..120000,
   0 = never). The worker (`foreign_fence_service`, every 250 ms at most) declares fired any point whose fence has not
   fired 6 s after it was attached (the host's own fence timeout is 5 s and fires with an error status), queues its handle
   for closing and prompts the completion DPC: a boundary that can never become ready is ready after a bounded time.
   `RmGExp`, `RmGateMsEff`.
6. **Owner death.** A killed owner can now leave its escape, so `DestroyDevice` / `DestroyProcess` run: the RM gate is
   purged (`rm_gate_purge_process_ordered`), the stream slot retires, `discharge_dead_present_stream_waits` ends the
   waits that named it, and a Deferred programming gated on it exits through `WorkerBindDispatch::Abandoned` ->
   `ScanoutReject::ProducerAbandoned` -> `complete_dead_source`, which publishes the flip's address kept (read, not run:
   `stage_worker_scanout_bind`, `display.rs` `complete_dead_source`). A flip nobody completes is retired by item 4 after
   `FlipPendWdMs` whoever owned it.
7. **A stale dump refreshes itself.** The vsync tick asks the mirror thread for the stall block when it is older than 5 s
   (`StallReqN`): the 333 stuck dumps were read 60 s and 290 s after the block they showed.

### 23.5 Counters (at most 14 characters, unique across `kmd_render` and `kmd_logic`)

`EscWaitN` (escapes scoped), `EscWaitMax` (longest, ms), `EscAbortKill`, `EscAbortStop`, `EscTimeout` (waits that gave up:
thread terminating, device stopping, `EscWaitMs` spent), `LkWaitAbort` (mutex acquires that gave up), `EscNoSlot` (table
full), `EscRefStop` (escapes refused while stopping), `EscWaitMsEff`, `PreFence`, `PreLastCmp`, `PreDropped`, `PreStatus`,
`PreT` (the last `DxgkDdiPreemptCommand`), `FlipPendWd`, `FlipPendWdT`, `FpWdMsEff`, `StallReqN`, `RmGExp`, `RmGateMsEff`.
Knobs: `EscWaitMs`, `FlipPendWdMs`, `RmGateMs` (read at every StartDevice).

### 23.6 What to read after the next wedge

`DdiInflL` / `DdiInflH` / `DdiOldId` / `DdiOldMs` (is anything of ours in flight), `NPreempt`, `TPreempt`, `Pre*`, `NStopDev`,
`StallT` against the uptime (is the block fresh), `FlipIss`, `FlipPub`, `FlipPubT`, `VsPendN`, `FlipPendWd`, `RmGAtt`,
`RmGFire`, `RmGExp`, `EscAbort*`, `EscTimeout`, `LkWaitAbort`. And the kernel stacks, which this incident lacked: with
LiveKd, `!process 0 7` for the stuck process and for `dwm.exe`, `!thread` for the escape thread, `!stacks 2 dxgkrnl`,
`!stacks 2 dxgmms2`, `!stacks 2 helios_kmd_render`, `!locks`, `!vm`. The UMD could also set `NoAdapterSynchronization`
on its escapes (none does): that takes dxgkrnl's adapter-level serialization out of the picture for them (not a KMD change).

### 23.7 Verified, and not

Verified (host tests): the wait state machine (unscoped waits unchanged to their own total, the 1 ms -> 1 s ladder,
scoped slices <= 100 ms, kill beats stop beats deadline, a kill noticed within one slice, the deadline exact across the
32-bit clock wrap, never 0, a nested scope keeps the outer deadline and does not release it, a full table registers
nothing); the pending-flip watchdog decision (to the millisecond, once per flip, a newer stuck flip fires again,
the wrap, the incident's own numbers); gate point expiry (in order, out-of-order fires, wrap, unstamped never); knob
clamps; counter names (listed = written, written nowhere else). Type-checked against the stub harness (the error set
equals the base's, apart from one stub-only unknown-field error counted twice).

NOT verified: anything on hardware or the WDK build; that a killed thread's abort status is what the UMD expects from each
escape (an abort is the escape's own timeout/not-ready status); that `PsIsThreadTerminating` is true for a thread under
`Stop-Process -Force` while it waits in KernelMode (it is the documented test; the 100 ms slices make the wait itself
independent of any APC); the cause of H1.

Risks: (1) `EscWaitMs` 10 s cuts a legitimate escape that waits longer in total (a first-time RM init with a user
timeout above it): the UMD sees its `TIMEOUT` status; the knob is 0 or larger on a machine that needs it; (2)
`FlipPendWdMs` 500 publishes a kept address for a producer slower than half a second (the screen shows the previous
picture a moment longer; 14.3); (3) an aborted `release_blob` leaves the host blob to the transport reset; (4) the
preempt breadcrumbs are diagnosis only.

### 23.8 Checklist

1. Heaven 1600x900 windowed 10-minute soak with `HELIOS_NVK_RM_FENCE_PRESENT=1`: `FlipPendWd` 0, `RmGExp` 0, `EscTimeout` 0,
   `EscAbortKill` 0, `LkWaitAbort` 0.
2. Kill the app while it presents: it ends within a second; `EscAbortKill` may count; the desktop continues.
3. `pnputil /restart-device` with an app running: returns; `EscAbortStop` may count.
4. A wedge with `FlipPendWdMs=0`, then 500: `FlipPendWd` moves and the flip queue resumes.
5. `EscWaitMs=0`, `FlipPendWdMs=0`, `RmGateMs=0` restore v334 behaviour (kill and stop exits stay).

## 24. Asynchronous composed present (BltAsync, BltNoMirror)

Status: implemented behind two knobs that default to 0 (the previous behaviour). Nothing here has run: the KMD cannot be
built or run in the authoring environment. The pure logic (`kmd_logic/src/blt_async.rs`, 23 tests) is host-tested; the
render crate was rustfmt-parsed and type-checked against the stub harness (the error set equals the base's, apart from
stub-only unknown-field and arity errors in the new files).

Hardware finding (v337.2): the arm was never entered because `ForeignCopy` was 0; see 24.11 (entry conditions, `BltEntry*` /
`BltNoEntry*` counters, `BltAsyncVenus`).

### 24.1 The measurement

Heaven 1600x900 windowed, composed under DWM, KMD 334.1: the host renders 576 fps, the VM shows 148 fps. PresentMon:
`msBetweenPresents` p50 6.61 ms, `msInPresentAPI` p50 3.55 ms ("Composed: Copy with GPU GDI"); the UMD present gate
averages 2.56 ms in mode 0 (a CPU wait); about 1.1 ms of the rest is the KMD. Host GPU utilization on this path is
55-70 %: each frame runs CPU, then GPU, then CPU, serially.

### 24.2 What the DDI waits for today, and why (read, `ddi/display.rs`, Blt arm, non-snapshot branch)

A Blt whose source is an adopted foreign (NVK-on-RM) allocation (`foreign_source_if_enabled`, `ForeignCopy=1`) and whose
destination is a KMD standard buffer (`PitchedStandardBuffer`, `PresentDestinationDesc::StandardBuffer`: DWM's redirection
surface) runs, on the app thread, in this order:

1. `begin_present_buffer_write_legacy` (`virtio/ctrl.rs`): take the buffer's writer ownership (`PresentBufferAccess`
   `KmdWriter`). It sleeps in 1 ms slices (rounded to the timer quantum, 5 s budget) while a consumer (DWM) has not
   finished reading the previous frame or a writer is on it.
2. `submit_present_blt`: import the NVK image into the KMD's Venus device (cached after the first frame), enqueue the
   reusable copy command on ring 1 (`submit_venus_async_present`). Per frame this also allocates two contiguous DMA
   buffers (`stage_display_submit`), unchanged here.
3. `wait_fence(gpu_fence, 5 s)`: sleep until the copy's ring-1 wire fence completes. THIS IS THE CPU WAIT.
4. `present_buffer_cpu_mirror_ready` (ownership is now `KmdCpuMirror`: the ring completion moved it there) and
   `mirror_present_system_backing`: take the system-backing content mutex, map the destination blob
   (`RESOURCE_MAP_BLOB` + `MmMapIoSpace`) and memcpy the whole frame (5.76 MB at 1600x900) into the system pages VidMm
   gave the allocation, if it keeps a lease on any (`SystemBackingPolicy::PresentLinearBuffer`; none: the call is a
   mutex and a lookup).
5. `complete_present_buffer_cpu_mirror` (ownership back to `ExternalReady`), `merge_fence(gpu_fence)` into the DMA
   private data, return.

Why step 3 exists. It is NOT what makes the Present's DMA fence mean "the copy is done". That is already tied to the copy's
wire fence by the private record (`PresentSubmissionPrivate::gpu_fence_id`, written by step 5's merge): SubmitCommand
(`note_and_maybe_signal` -> `note_wddm_submission`) gates the packet on that exact fence in the GPU-completion domain
(`RetireDomain::IncludingGpu`, `wddm_boundary::select`), and the completion DPC signals `DMA_COMPLETED` when the wire fence
has retired. The wait exists because (a) the mirror (4) reads the blob and must run after the copy, and (b) the hand-back of
the buffer ownership (5) was done by the DDI after the mirror. Remove the mirror, or move it off the app thread, and the
wait has no job.

What orders the copy after the producer today. Nothing in this arm. The Present's stream boundary (the RM fence the UMD
attaches with carrier (b), `NvkRmFencePresent`) is merged into the private record in `present_complete`, so the DMA FENCE
waits for the producer, but the Venus copy is submitted at once, in the DDI, whether the producer has finished or not. Today
that is harmless only because the UMD's present gate (the 2.56 ms CPU wait) makes the producer finish before the Present is
issued. With the UMD wait removed, the copy of the legacy arm would read a frame the NVK queue may still be writing. The host
cannot order a Venus ring-1 command after an RM fence (they are different drivers). The ordering has to be the KMD's:
submit the copy only after the boundary is ready.

Who reads what (allocation classes). The Venus consumer of a dedicated present buffer (identity bit 0
`DEDICATED_PRESENT_BUFFER`, `rm-backed-standard.md` 1.2) imports the buffer's own memory as a linear image: a GPU read of
the blob, after the Present's DMA fence, through the ownership table (`claim_present_buffer_read`, which refuses while the
KMD is a writer). The system backing is read only by CPU views of the allocation (GDI `LockCb` readers; a CPU mapping of a
staging or shadow surface) and is the source of a page-in. The census in `rm-backed-standard.md` found DWM opening no KMD
STANDARD allocation at all in its own runs and reading the UMD-made images instead; for the redirection surfaces this
section is about, the requester states DWM reads the GPU copy. That is the premise of `BltNoMirror` and it is
unverified here (24.8).

### 24.3 Design

`BltAsync` (default 0): for a foreign source into a standard buffer, `DxgkDdiPresent` never waits for the copy. The route is
`helios_kmd_logic::blt_async::decide`:

| situation | route |
|---|---|
| knob 0, source not foreign, snapshot present, destination not a standard buffer | legacy arm, unchanged |
| producer boundary live and NOT ready; or live and the mirror is on; or an older copy for the destination is still queued | DEFERRED |
| producer boundary live and ready, mirror off, nothing queued for the destination, table room | DIRECT |
| no boundary (the UMD waited on the CPU), mirror off, nothing queued | DIRECT |
| no boundary and the mirror is on; dead boundary; full table, with nothing queued for the destination | legacy arm (counted `BltAsyncFall`, reason `BltAsyncWhy`) |
| no boundary or a dead one, an older copy for the destination is queued (whatever the mirror knob says: v337) | wait (bounded) for the queue to drain, then the legacy arm (`BltDrainN`) |
| any of the above where the queue, the token or the submission was refused after the decision | the same drain, then the legacy arm (v337) |

DIRECT (`ddi/blt_async.rs::direct`, `virtio/gpu/blt_async.rs::enqueue_async_submit_blt`). The destination ownership is taken, or
joined, in the same critical section of the transport lock as the ring-1 enqueue, and the copy is recorded in a small fixed
table (`kmd_logic::blt_async::Table`, 8 entries). The DDI then merges the copy's wire fence into the private record exactly as
before and returns. The completion DPC (`blt_async_retire`, next to the other per-fence retirements of `drain_used`) hands the
buffer back when the LAST direct copy of it retires. Several frames for one destination may be in flight at once (an app that
presents faster than the copy completes): ring-1 submissions of one context retire in order, so "the last to retire" is
also the last to write. A writer that is not a direct copy (a deferred copy, a CPU mirror) is never joined (`begin`).

DEFERRED (`queue_async_blt`, then the existing WindowedBlt machinery). The reusable copy is prepared, a request is queued in
the WindowedBlt FIFO under the producer's boundary and its token is merged into the private record (the same two-phase
transaction a DXVK snapshot Blt has): SubmitCommand admits it once the destination's residency is effective, the HPD worker
(`service_windowed_blt`) submits it when BOTH the boundary is ready (`scanout_boundary_ready`: an RM gate fires through the
EventReady DPC, `rm_gate_fire`, and the worker is woken) and the destination can be written (`try_begin_present_buffer_write`;
a completion wakes the worker), and the ring completion terminalizes the token the Present's DMA fence waits for. The two
additions to the request are `async_blt` (timed and counted) and `no_mirror`; the source is ledgered like a snapshot's
(24.10.3; a full ledger leaves it unledgered, it does not refuse the request). With the mirror on, the worker's existing PASSIVE mirror runs after the ring completion and the token
terminalizes after it; with `BltNoMirror` the ring completion hands the buffer back and terminalizes at once.

`BltNoMirror` (default 0, independent of `BltAsync`): for the same class of Blt the CPU mirror is not made. Instead the
destination's system copy is marked invalid (`mark_stale_if_backed`, the "system copy invalid" machinery of
`build_paging_buffer.rs`: `SystemBackingTable::mark_system_copy_invalid`) when, and only when, VidMm holds system pages with a
KMD lease for it. A later SYSTEM_TO_LOCAL page-in of the allocation is then skipped (`PgInvSk`) instead of copying the older
system pages over the blob, and the next whole-allocation eviction (blob to system) revalidates the mark (`PgInvClr`). Not
marking an allocation that has no backing keeps the bounded invalid set (overflow skips every page-in, `PgInvOvf` must stay 0)
out of the per-frame path. The mark is placed immediately before the copy is submitted (the DDI for DIRECT and the legacy arm,
the worker for DEFERRED), so the window in which a page-in can see pages older than the copy in flight is the copy itself.

### 24.4 Invariants

1. The Present's DMA fence retires only after the copy has completed on the host GPU. DIRECT: the private record names the
   copy's wire fence (unchanged mechanism). DEFERRED: the fence waits for the request's terminal token, which exists only after
   the ring completion (and after the mirror, if there is one). A gate that fires with an error, or expires (`RmGateMs`),
   releases the copy; the producer's frame may then be incomplete, the same trade the gate always made.
2. The copy never starts before the producer has finished. DIRECT is chosen only for "no boundary" (the UMD's CPU wait already
   ordered it) or a boundary already ready; otherwise the copy waits in the FIFO until `scanout_boundary_ready`.
3. One destination, one order. A DIRECT copy never overtakes a queued one for the same destination (`dst_deferred_pending`), a
   queued copy cannot start while direct copies hold the buffer, and a legacy Blt that cannot be queued waits for the queue
   to drain first. A deferred request's dispatch is FIFO in admission order.
4. Ownership. The destination is KmdWriter from the enqueue (DIRECT) or the dispatch (DEFERRED) until the copy's completion; no
   consumer claim is accepted meanwhile (`claim_present_buffer_read` is `Busy`), allocation teardown waits
   (`begin_present_buffer_teardown`), and the cached copy command is drained by `release_present_blits_for_resource` as before.
5. A failed copy still completes the Present. The host's error response for a ring-1 command means it touched nothing: the wire
   fence retires regardless (the DMA fence signals), the destination keeps the previous frame, `BltAsyncFail` counts it, and the
   buffer is handed back. (The legacy arm pinned the buffer for ever in this case. A transport latch is different: everything
   is abandoned with the transport generation. Why releasing is safe: 24.10.2.)
6. Nothing sleeps in the DDI except the legacy fallbacks (`BltAsyncBusy`, `BltDrainN`).

### 24.5 Hazards

* Source reuse (write after read). Before, the DDI returned after the copy had read the NVK source, so the app could draw into
  it at once. Now the Present returns with the copy still queued behind the producer (milliseconds). dxgkrnl's allocation
  tracking orders the app's next WDDM submission after the Present's DMA fence, but NVK on RM submits through its own channel,
  which dxgkrnl does not see. Whatever recycles the swap-chain buffer must wait for the Present's fence; the KMD cannot enforce
  it. Symptom if not: a frame with the next frame's pixels in it (tearing, one frame early). First thing to look for in the
  checklist.
* DWM reading before the copy completes: only if the DMA fence signalled early. The fence is tied to the copy (24.4 item 1);
  `PBFnc` shows the fence or token each Present carried.
* `BltNoMirror` and CPU readers. A GDI or CPU reader of the destination (a `LockCb`, a CPU-mapped staging surface) sees the pages
  VidMm holds, which are no longer updated while the allocation is system-resident and are skipped on page-in. If DWM reads the
  system pages rather than the GPU copy, composition shows stale content: `BltMirrorSk` rising with a frozen window is the
  signature. Turn the knob off.
* The eviction race. A whole-allocation eviction that completes while a copy is in flight copies a partial frame into the
  system pages and clears the mark; a later page-in then puts that frame over the blob until the next Present rewrites it
  (one frame). The mark is re-placed on the next Present.
* A failed copy (`BltAsyncFail`): see 24.4 item 5.
* A dead boundary (the producing process was killed): DIRECT is not used, the legacy arm copies at once (unordered, as before
  this change); a queued request whose stream dies is cancelled by the existing teardown paths.
* A deferred request makes the Present's DMA fence depend on the worker. A wedged worker stalls composed presents; the
  existing `WddmHeadMs` rebase bounds it as for snapshot Blts.
* `KmdRmClient` 5: the level 5 frame edge is raised by the asynchronous routes at their own end (24.10.1; the first version
  of this section argued it was never owed and was wrong).

### 24.6 Counters (at most 14 characters, unique across `kmd_render` and `kmd_logic`; `kmd_logic::blt_async::COUNTERS`)

| counter | meaning |
|---|---|
| `BltAsyncKnob`, `BltNoMirKnob` | the knobs in force (written at every StartDevice, 0 included) |
| `BltAsyncN` | asynchronous Blts made; `BltAsyncDir` direct, `BltAsyncDefer` deferred (producer not ready, mirror on, or an older frame queued) |
| `BltAsyncInfl`, `BltAsyncPk` | submitted or queued with the copy not yet complete, now and the most at once |
| `BltAsyncLat0..7` | submission to copy completion: < 250 us, < 500 us, < 1 ms, < 2 ms, < 4 ms, < 8 ms, < 16 ms, more |
| `BltDeferUs` | microseconds deferred Blts waited in the FIFO from Present to submission |
| `BltAsyncFail` | copies the host answered with an error |
| `BltAsyncFall`, `BltAsyncWhy`, `BltAsyncMask` | eligible Blts that took the legacy arm; last reason code and the set of reasons seen (bit `code - 1`: 2 not foreign, 3 not a buffer, 4 no boundary with the mirror on, 5 dead boundary, 6 queue refused, 7 token refused, 8 submit refused, 9 older copy queued and no boundary, 10 table full, 11 destination busy) |
| `BltAsyncBusy`, `BltDrainN` | of those, destination busy; legacy Blts that drained the queue first |
| `BltSrcBusy` | Presents of a source an earlier asynchronous copy was still reading (24.10.3) |
| `BltLookKnob`, `BltLookN` | worker lookahead depth in force; copies dispatched ahead of a front entry that could not go (24.10.4) |
| `BltWaitN`, `BltWaitUs`, `BltWait0..7` | the legacy arm's CPU wait for the copy (same buckets): what `BltAsync` saves |
| `BltMirrorN`, `BltMirrorSk`, `BltMirrorUs` | CPU mirrors done (legacy and worker), skipped by `BltNoMirror`, microseconds spent in them |
| `BltNoMirInv` | destination system copies newly marked invalid (an Already mark is not counted) |

`PBCpy` is 3 for a DIRECT Blt and 4 for a DEFERRED one (`PBFnc`: the wire fence or the token); `PBSyCp` is 3 when the mirror
was skipped. A new transport generation zeroes the counters (`reset_for_start`).

### 24.7 Where the code is

`kmd_logic/src/blt_async.rs` (route table, ownership rule `begin`, in-flight `Table`, buckets, stale-mark rule, counter list and
the name scans); `kmd_render/src/ddi/blt_async.rs` (knobs, counters, `try_async`, `direct`, `deferred`, `drain`);
`kmd_render/src/virtio/gpu/blt_async.rs` (in-flight table in the transport, `enqueue_async_submit_blt`, `blt_async_retire`,
`queue_async_blt`, ownership release); hooks: `ddi/display.rs` (Blt arm, worker), `virtio/ctrl.rs::submit_venus_async_blt`,
`virtio/venus/present.rs::submit_present_blt_direct`, `virtio/gpu/mod.rs` (the retire arm, the request fields, the ring
completion), `adapter/backing.rs::mark_stale_if_backed`, `diag.rs` (knob names), `ddi/lifecycle.rs`, `ddi/submit_command.rs`.

### 24.8 Verified, and not

Verified (host tests): the route table, exhaustively over its inputs (Direct only with its preconditions, never with a producer
that has not finished, never past a queued older copy; Deferred only behind a live boundary); the ownership rule; the in-flight
table (last-writer hand-back, any completion order, bounded, fence ordering); histogram buckets at their edges; the stale-mark
rule; counter names (listed = written by `ddi/blt_async.rs`, written nowhere else, at most 14, unique).

NOT verified: anything on hardware or the WDK build; that the host executes ring-1 copies of the KMD context in submission order
across the destination (assumed from the Venus per-queue order, which the legacy arm already relied on for its own fence);
that a deferred request for a source that is not a snapshot passes every WindowedBlt precondition (admission, terminal
membership, teardown by resource id): the paths are shared but were written for snapshots; that `merge_blt_boundaries` accepts
the RM gate boundary beside a Venus stream boundary in one private record (a refusal is counted `TokenRefused` and falls
back); the premise that DWM reads the GPU copy (24.2).

### 24.9 Hardware checklist (Heaven 1600x900 windowed, composed; lowest mode first: 1920x1080 at 60 Hz, then bigger)

Run each row ten minutes, read the counters after, and record PresentMon `msInPresentAPI`, `msBetweenPresents`, host GPU
utilization (`nvidia-smi dmon -s u` on the host) and fps.

| row | `HELIOS_NVK_RM_FENCE_PRESENT` | `BltAsync` | `BltNoMirror` | expect |
|---|---|---|---|---|
| 1 | 0 | 0 | 0 | the baseline; `BltWaitUs / BltWaitN` is the DDI's wait, `BltMirrorUs / BltMirrorN` the mirror |
| 2 | 0 | 0 | 1 | `BltMirrorSk` = Presents, `BltMirrorN` 0, `BltNoMirInv` > 0 only if VidMm paged the surface; the wait unchanged |
| 3 | 0 | 1 | 1 | no boundary: all DIRECT (`BltAsyncDir`), `BltWaitN` 0, `BltAsyncPk` small, `BltAsyncLat` mostly < 2 ms |
| 4 | 1 | 0 | 0 | the ordering gap of 24.2 shows as torn or old frames if the producer is slower than the DDI |
| 5 | 1 | 1 | 0 | all DEFERRED (mirror on), `BltDeferUs / BltAsyncDefer` is the wait for the producer, `BltMirrorN` from the worker |
| 6 | 1 | 1 | 1 | the target: DEFERRED while the producer runs, DIRECT when it had finished (`BltAsyncDir`); `BltWaitN` 0, `BltMirrorN` 0 |

Pass: `BltAsyncFail`, `BltAsyncBusy`, `BltDrainN` 0 or tiny; `BltAsyncInfl` returns to 0 at idle; `PgInvOvf` 0; no `RmGExp`, no
`WddmHeadMs` rebase; `FlipPendWd` 0; no tearing or one-frame-early content in Heaven's moving scene (the source-reuse hazard);
a GDI app (Notepad, Explorer) beside it stays correct with `BltNoMirror` 1 (the CPU-reader hazard); `pnputil /restart-device`
with the app running returns and the counters restart from 0. Compare rows 1, 3, 5 and 6 on `msInPresentAPI` (the saved
wait), `msBetweenPresents`, fps and host GPU utilization. Recommend the defaults only after rows 3 and 6 are clean; turn
`BltNoMirror` on separately, after the question of who reads the system pages is settled.

### 24.10 Follow-ups after review (v337)

Five findings of the review of v336, and what was done about each. The pure parts are in `kmd_logic/src/blt_async.rs`
(1355 tests in the crate now, nine of them new); the rest is in the same three files as 24.7.

#### 24.10.1 Level 5 (`KmdRmClient` 5) lost its frame edge (fixed)

The legacy arm calls `primary_changed(Edge::PresentBlt)` after a copy that completed, and the worker's mirror stage calls
`primary_changed(Edge::WindowedBlt)`. The asynchronous routes reached neither: the DIRECT route returns before the arm's
tail, and a DEFERRED copy with `BltNoMirror` ends in `complete_windowed_blt_ring`, which has no mirror stage. A destination
that is the shown RM primary would then never be flipped. Now:

* DIRECT: the completion DPC (`blt_async_retire`) raises `Edge::PresentBlt` for the destination when the copy succeeded.
* DEFERRED with `BltNoMirror`: `complete_windowed_blt_ring` raises `Edge::WindowedBlt` at the terminal.
* DEFERRED with the mirror on: unchanged, the worker's mirror stage raises it.
* A failed copy raises nothing (the screen did not change; the legacy arm returned before its edge as well).

The decision is the pure `blt_async::edge_owed(Finish, copy_ok)` (tested for every finish and both outcomes); whether the
destination is the shown RM primary stays `rm_refresh::judge`'s question, asked inside `primary_changed`. That function is
atomics and `KeSetEvent(Wait = FALSE)` only, so it is legal in the DPC; the DPC reaches the adapter through the pointer the
first direct submission stored in the in-flight state (`BltAsyncState::adapter`, with the same lifetime argument as
`WindowedBltPending::adapter`: every entry is retired or forgotten before the transport goes). The earlier text of 24.5
(the edge "is not raised ... never takes this arm") was an argument, not a guarantee, and is superseded by this.

#### 24.10.2 A failed copy releases the destination (decided, documented)

The legacy arm leaves the buffer `KmdWriter`-poisoned after a rejected host response, with this reasoning in the code: "a
retired wire id is not enough: a rejected host response leaves KmdWriter poisoned and must never authorize a CPU read or
external reacquire". The asynchronous routes with `BltNoMirror` hand it back. This is a deliberate change of the invariant,
for these reasons and no wider than these:

* What the poison protected is a CPU read (the mirror) of a destination the copy may have half written. With `BltNoMirror`
  there is no CPU read. The same invariant holds where it still applies: a DEFERRED copy with the mirror on keeps the
  legacy behaviour (its ring failure never reaches the mirror stage, the buffer stays pinned), and so does every
  `BltAsync` 0 Blt.
* What it costs when it is kept: the buffer is never writable again, so every later Present to that destination waits the
  full 5 s budget of `begin_present_buffer_write_legacy` and then fails, and no consumer (DWM) can read it either: the window
  is dead for the session, for a single rejected command.
* What it costs when it is released: if the host started the copy and then failed, an external reader may take one frame that
  is partly the new one. The next Present rewrites the buffer. The host's error answer for a ring-1 SUBMIT_3D is a rejection
  at decode or at fence level, i.e. a command that did not run to completion; this is a belief from the transport code
  (`resp_is_ok`), not something measured, and is why the case is counted, not hidden.
* It is counted: every failed asynchronous copy bumps `BltAsyncFail` (DIRECT in `blt_async_retire`, DEFERRED in
  `complete_windowed_blt_ring`), and the Present's DMA fence retires with the wire fence regardless. A transport latch is a
  different event: all entries are forgotten with their ledger tickets retired as failures, and nothing is handed back.

`BltAsyncFail` should read 0 on a healthy run. A nonzero value with torn frames in the same window is the evidence that the
belief is wrong; the knob that restores the old behaviour is `BltAsync` 0.

#### 24.10.3 Source reuse after the Present returns (highest risk: UMD requirements, plus a KMD-side claim)

Check of what orders the next render. The UMD's RM-fence carrier (b) gates the NEXT PRESENT's DMA fence on the producer, it
does not gate the next RENDER on anything of this Present: NVK on RM submits its work through its own channel, which neither
dxgkrnl nor this KMD sees. Before v336 the DDI returned after the copy had read the source, so nothing could overwrite it
afterwards; with `BltAsync` the Present returns while the copy is still queued behind the producer (and, deferred, until the
worker submits it), so the source's next-frame render is not ordered after this copy by anything in the KMD. The source of
a Blt is a swap-chain back buffer the application may draw into as soon as Present returns (blt-model swap chains), so
the hazard is real and cannot be closed from the KMD alone: the KMD has no way to stop an RM channel.

What the UMD must guarantee (requirement list for the UMD session):

1. Do not let any work that WRITES a swap-chain buffer (or a resource that was the source of a Blt) start until the DMA fence of
   the Present that read it has signalled. The Present's fence is the one `DxgkDdiPresent` returned with: it is the copy's
   wire fence (DIRECT) or the copy's terminal token (DEFERRED), and it signals only after the host GPU finished reading.
2. The wait has to be a GPU-side or a CPU-side wait the UMD owns, because the KMD cannot gate an RM channel: a CPU wait on
   the fence before the next Draw/Clear/Present of that buffer (`D3DKMTWaitForSynchronizationObject`-style on the Present's
   monitored/DMA fence, or the swap chain's frame-latency semaphore / buffer-ready that is signalled from it), or an RM
   semaphore acquire inserted ahead of the next render that the UMD releases from that fence.
3. With composed presents the runtime's present queue and `SetMaximumFrameLatency` bound the number of Presents in flight, and
   the back-buffer rotation depth decides which buffer the next frame renders into. The UMD must verify that depth: if the
   number of buffers is not larger than the latency of (producer + deferred queue + copy), which is milliseconds here, the
   rotation hands the app a buffer an earlier Present is still reading. The safe configuration is a rotation depth of at least
   the frame latency plus one AND requirement 1 on top (the rotation alone is an assumption about timing).
4. The same holds for a buffer that is Present'd twice in a row: the second Present's copy reads it again, and the first one may
   still be in flight (`BltSrcBusy` counts exactly this, below).
5. The RM-fence carrier must keep attaching the fence of the work that WRITES the frame the Present reads (it does today); the
   KMD only orders the copy after it.

Not verified: that the NVK UMD does any of this today. It is not in this tree (the Mesa side), and this change cannot know. Treat
it as the first thing to check on hardware (24.9): tearing or one-frame-early content in Heaven's moving scene with `BltAsync`
1 and the RM fence present on.

KMD-side safety added: the source is CLAIMED in the read ledger for the life of the copy, the same ledger a DXVK snapshot
reader is published in (`HELIOS_ESCAPE_MAP_READ_LEDGER`: a slot per resource id, `issued > retired` while a host read is in
flight). DIRECT takes the ticket in the same critical section as the enqueue and the completion DPC retires it (a failed
copy and a transport latch included); DEFERRED takes it when the request is queued and the existing ring completion and
terminal paths retire it. A full ledger (`RdOvf`) leaves the copy unledgered, loudly, and never refuses it; `RdIss` and `RdRet`
stay balanced. This is a claim a consumer can read, not a wall: nothing in the KMD waits on it, and the NVK UMD does not
read the ledger today. It lets the UMD (or a probe) see the source busy without a KMD change, and it makes the copy's
read lifetime visible in the same place as every other.

`BltSrcBusy` counts Presents whose source an earlier asynchronous copy was still reading when the next Present of it arrived
(the in-flight table and the queued requests are searched in the same critical section as the route decision). Nonzero means
the buffer rotation is shallower than the copy's latency or a buffer is Present'd again before it was released: with the
requirements above violated, this is the Present at which the frame can be torn.

#### 24.10.4 Head-of-line blocking of the deferred queue (fixed, `BltLookahead`)

`take_ready_windowed_blt` looked only at the FRONT of the ready queue. A live producer boundary that has not finished (an NVK
frame in flight) held every later windowed copy of every other window behind it, so one slow producer stalled DWM's other
windows. Now the worker looks at the first `BltLookahead` ready entries (default 1 = the old behaviour, so a plain build is unchanged; set 4 together with `BltAsync`; clamped 1 to 8, read at every StartDevice,
mirrored as `BltLookKnob`; 1 is exactly the old behaviour) and dispatches the first that can go: admitted, its producer's
boundary reached, its destination writable. Per-destination order is kept by construction: an entry never goes ahead of an
earlier LIVE entry of its own destination, whether or not that earlier one could go (`blt_async::pick`; the test enumerates
every window of four entries over three destinations and every ready pattern: the pick is the first entry that could go, is
never preceded by a live entry of its own destination, and nothing is picked only when nothing could go). A stale token in
the window neither dispatches nor blocks (the front is still healed exactly as before, `WbStaleRdy`). `BltLookN` counts
dispatches made ahead of a front entry that could not go. This changes the dispatcher the DXVK snapshot Blts share, which is
why the depth is a knob; with 1 the behaviour is byte for byte the old one.

Order across destinations is not preserved by design: the WDDM fence of each Present waits for its own token's terminal, not for
an earlier token's. Order across entries beyond the window, and of entries not yet admitted, is as before.

#### 24.10.5 Ordering gap with `BltNoMirror` 0, no boundary and queued older copies (fixed)

`decide` tested the mirror knob before the queued-copy test, so a Blt with no boundary, the mirror on and older deferred copies
for the same destination took the plain legacy route and its copy could land BEFORE the older frame. A dead boundary had the
same gap. Now the queued-copy test comes first for both: the route is `LegacyAfterDrain`, whatever the mirror knob says. Beyond
`decide`, every way out to the legacy arm (a refused queue, a refused token, a refused submission, a busy destination) now
drains the destination's queue before the arm runs (`try_async` does it once, at the single exit), so no fallback can overtake
a queued frame either. Tests: an older queued copy is drained for every mirror setting and both boundary states, and an
exhaustive property over the whole input space: a plain `Legacy` route with a queued copy exists only for the Presents this
feature never touches (knob off, not foreign, not a buffer).

#### 24.10.6 Counters added, and what to read

`BltSrcBusy`, `BltLookKnob`, `BltLookN` (24.6 plus these). `BltAsyncFall` / `BltAsyncWhy` bit 9 (`PendingNoBoundary`) and bit 5
(`BoundaryDead`) now also mean "drained first". Checklist additions (24.9): read `BltSrcBusy` (expect 0 with a sound buffer
rotation), `RdIss == RdRet` after a quiescent run, and, with `KmdRmClient` 5 and Heaven windowed onto the RM primary, that the
frame edge `RmSysEdBlt` / `RmSysEdWBlt` (the per-edge counters of `sysmem_flip`) move with `BltAsync` 1 as they do with 0. Two
Heaven windows at once, one of them slowed by a heavy producer, with `BltLookahead` 1 and 4: the other window's frame rate
should stop following the slow one's at 4 (`BltLookN` > 0).

### 24.11 The arm was never entered on hardware: entry conditions, the finding, the counters (v339)

#### 24.11.1 The finding

KMD 337.2, Heaven windowed D3D11 composed under the NVK DWM, `BltAsync=1 BltNoMirror=1 BltLookahead=4` (knobs confirmed in
`BltAsyncKnob` / `BltNoMirKnob` / `BltLookKnob`): `BltAsyncN` 0, `BltAsyncFall` 0 (not even counted as a fallback),
`BltMirrorSk` 0, while `BltWaitN` == `BltMirrorN` rose with every present. The same service-key dump holds `FcKnob=0` and
`FcOff=5354` == `BltWaitN` (5354). `FcOff` counts "a foreign source seen while `ForeignCopy` is 0"
(`foreign_source_if_enabled`, `virtio/venus/foreign_copy.rs`), so every Blt of the run was a foreign (NVK-on-RM) source
with `ForeignCopy` off.

Which condition failed. In `ddi/display.rs` (Blt arm, before this change) the async arm and the mirror skip were gated on
`source_foreign`, which the arm sets only in the `foreign_source_if_enabled(...)` branch of the source-descriptor chain
(the Some branch needs `adapter.knobs().foreign_copy`). With `ForeignCopy=0` that function returns `None`, the chain falls
to the `source.storage` match, the source is imported as an ordinary OPTIMAL image (the plain import the host refuses for a
foreign resource, section 11), `source_foreign` stays false, and
`no_mirror_applies(.., source_foreign, ..)` and the async test were both false: the Blt ran the legacy path
(`begin_present_buffer_write_legacy`, `submit_present_blt`, `wait_fence` = `BltWaitN`, `mirror_present_system_backing` =
`BltMirrorN`) with neither new feature consulted, and nothing counted because the counters sat behind the same gate.
The fix for the tester is `ForeignCopy=1` (a restart-device is enough). Two caveats worth reading from the counters of that
run: the legacy copy of a foreign source with `ForeignCopy` 0 is the import the host refuses, so `BltWaitUs` of those rows
(about 0.98 ms per Blt) measured a refused copy plus a mirror of whatever the destination held, not a real composed
frame; and `FcImp`, `FcBlt`, `FcRefuse`, `FcHostErr` were all 0 (no foreign copy ever ran). Compare the rows again with
`ForeignCopy=1`.

Hypotheses considered, ranked, with what the counters said:

1. `ForeignCopy=0` (confirmed: `FcKnob=0`, `FcOff` == `BltWaitN`; `display.rs`, the `foreign_source_if_enabled` branch).
2. A Venus-native source (UMD-made image, `foreign` None) would also have run the legacy arm and counted `BltWaitN`, but
   `FcOff` would then be 0: refuted by `FcOff`.
3. A destination that is not a standard buffer: refuted, `BltWaitN` and `BltMirrorN` are only counted for one
   (`destination_buffer.is_some()`).
4. A snapshot (WindowedBlt) Blt: refuted, that path never counts `BltWaitN` (`BltMirrorN` there comes from the worker, and
   `SnSub` / `SnFbk` / `BeSmp` are 0).
5. A precondition returning before the decision (patch capacity, format, kind, descriptors, extent): refuted, those return
   an error or a counted skip and never reach `wait_fence`.

#### 24.11.2 Entry conditions, in order (`ddi/display.rs` Blt arm; the pure decision is `blt_async::entry`)

| # | condition | where | on failure |
|---|---|---|---|
| 1 | `present_flags` Blt bit, not the level 5 RM primary, not an on-scanout skip | `dxgkddi_present_inner` | other arms (not counted) |
| 2 | DMA buffer and private data large enough; patch capacity (`validate_patch_capacity`) | top of the arm | error / `BLT_PATCH`, counted `BltNoEntryO` |
| 3 | adapter, source and destination resolve | `let (Some(adapter), ..)` | counted skip or error, `BltNoEntryO` |
| 4 | both DXGI formats resolve; source kind is DEVICE_MEMORY | `BltFormat`, `BltSourceKind` | `BltNoEntryO` |
| 5 | a snapshot, if present, validates | `validate_windowed_blt` | `BltNoEntryO` |
| 6 | source and destination descriptors exist; extents equal | `BltDescriptor`, `BltExtent` | `BltNoEntryO` |
| 7 | the entry decision (`entry`), counted `BltEntryDec`: 7a both knobs 0 (`KnobOff`, `BltNoEntryK`); 7b a snapshot (`Snapshot`, `BltNoEntryM`: its two-phase path is separate); 7c source class: `ForeignCopyOff` (`BltNoEntryFc`), or Venus-native with `BltAsyncVenus` 0 (`NotForeign`, `BltNoEntryF`); 7d destination not a standard buffer (`NotBuffer`, `BltNoEntryS`) | after row 6, before the snapshot / legacy split | the Blt takes the arm it always took |
| 8 | `BltEntryOk`: `async_enter` (`BltAsync` on) goes to `try_async`; `no_mirror` (`BltNoMirror` on) skips the mirror whichever arm copies | the non-snapshot branch | |
| 9 | `try_async`: `decide` (boundary, queue, table room, destination ownership) | `ddi/blt_async.rs` | `BltAsyncFall`, `BltAsyncWhy` (24.6) |

`BltEntrySeen` counts row 1 arrivals; `BltNoEntryO` is derived as `BltEntrySeen - BltEntryDec`, so it cannot drift from the
other counters. The identity at a quiescent point: `BltEntrySeen = BltNoEntryK + M + F + Fc + S + O + BltEntryOk`, and
`BltEntryOk >= BltAsyncN + BltAsyncFall` (the rest are no-mirror-only Blts). `BltEntryWhy` is the last reason code and
`BltEntryMask` has bit `code - 1` for every reason seen (1 knobs, 2 snapshot, 3 source not eligible, 4 foreign with
`ForeignCopy` off, 5 destination, 6 before the decision). Reasons are tried in the order of the table (a snapshot is reported
as the snapshot even though its source is not foreign).

#### 24.11.3 Venus-native sources: `BltAsyncVenus` (default 0)

Decision: a Venus-native source into a standard buffer is eligible for both knobs when `BltAsyncVenus=1`, and not by
default.

Why it can be eligible. Ordering after the producer: the copy is a Venus command on the same ring as the UMD's own commands,
and the producer boundary that travels with the Present (if any) is handled by `decide` exactly as for a foreign source; with
no boundary the route is DIRECT (mirror off) or the legacy arm (mirror on, `NoBoundaryMirror`). The DMA fence retires on the
copy's wire fence (24.4 item 1), the destination ownership, in-flight table and drain rules are keyed by resource id and do not
look at the source's origin, the source's read-ledger claim is taken by resource id (24.10.3), and `BltNoMirror`'s premise (DWM
reads the GPU copy) is a property of the destination. The wait and the mirror cost the same.

Why it is off by default. Nothing here has run for a Venus source: whether the host executes the UMD's last write to the
image before the KMD's copy on ring 1 with no KMD-visible boundary is an assumption the legacy arm hid behind its CPU wait
(the app thread blocked until the copy had retired, so a swap-chain buffer was never rewritten while the KMD read it); with
`BltAsync` the source-reuse hazard of 24.5 applies to Venus images too, and the Venus UMD's own present gate is the only thing
that bounds it. A tester can A/B it: `BltAsyncVenus=1` with `BltAsync` / `BltNoMirror`, read `BltEntryOk`, `BltNoEntryF`
(0 when it is on), `BltSrcBusy`, `BltAsyncFail`, and look for one-frame-early content.

#### 24.11.4 Counters (24.6 plus these; all in `kmd_logic::blt_async::COUNTERS`)

`BltVenusKnob` (knob in force), `BltEntrySeen`, `BltEntryDec`, `BltEntryOk`, `BltEntryWhy`, `BltEntryMask`, `BltNoEntryK`,
`BltNoEntryM`, `BltNoEntryF`, `BltNoEntryFc`, `BltNoEntryS`, `BltNoEntryO`. Read them first on any run where `BltAsyncN` is 0:
`BltNoEntryFc` rising means `ForeignCopy` is 0; `BltNoEntryF` means a Venus-native source; `BltNoEntryK` that the knobs did not
reach the transport (compare `BltAsyncKnob`); `BltNoEntryS` that the destination is not DWM's redirection surface.

Tests (`kmd_logic`): `entry` over its whole input space (2 knobs x venus knob x snapshot x destination x three source classes,
against an independent statement of the rule), the reason order, the foreign-copy-off refusal that this section is about, the
Venus knob, and the reason codes' bits.

## 25. Wrong buffer on scanout after a device restart (v337 incident; identity fix, StopDevice unbind)

### 25.1 The incident

Win11 tester, KMD v337 (`ForeignFlip` 1, `FlipAnnForeign` 1, NVK DWM), after `pnputil /restart-device` with `BltNoMirror` 1 and a plain
windowed Heaven: the viewer showed a wrong buffer, a 512x512-ish block of noise on the left, then a texture atlas of desktop icons
(Outlook, Store, Settings, Xbox) on black. KMD read at the time: `FfProg` 0, `FfNoRec` 0, `FlipIss` 0, `SaLo` 0xC4180000, `VpLpa`
0xC0590000. Earlier in the same session (same restart, no Blt knobs, `HELIOS_NVK_RM_FENCE_PRESENT` 1 on Heaven) Heaven failed to load
(D3D11 out of memory on a 2048x2048 DXT1/ATI2 texture); a later plain Heaven ran. Host backend log, per transport generation: DWM was
the SAME process (pid 13220) across all three restarts. Before the first restart `gpu_cmd` 352613 and `scanout_flip` 260689; in the
generations after it `gpu_cmd` 11 and `scanout_flip` 7 to 9 over minutes, with `ioctl` 9682 then 194739 and `open` 9620 / `close`
9681: the long-lived NVK clients reconnected in a loop and never got a working RM client again. The viewer therefore kept whatever it
had last been given while the guest's new generation reused small ids.

Not read from a dump (none was taken at the moment); everything below is from the code, with the counters that decide it.

### 25.2 What the numbers already say

* `FlipIss` 0 and `FfProg` 0 together: no flip of this generation had been issued or taken by the foreign arm. `SaLo` is also written
  by `pacing_publish` from `last_primary_address` (`adapter/scanout.rs:756`), which after a restart is the RestSeed (section 20.3a):
  the address of the PREVIOUS image's newest flip. So `SaLo` 0xC4180000 is most likely that seed and not a flip of this generation.
  `VpLpa` (`ddi/scanout_trace.rs:915`) is a service-key value and survives an image reload until it is rewritten, so a `VpLpa` that
  differs from `SaLo` is expected and is not by itself evidence of two programmed sources. Read both with `StartN` and `StartT`.
* `gpu_cmd` 11 means the NVK processes were not rendering at all: nothing new was presented, so the viewer's last picture was the
  only content. A wrong picture then needs an old binding or a flip naming a stale object.

### 25.3 Ranked hypotheses

**R1 (certain defect, fixed): the NVRM epoch repeats across an image reload, so surviving NVK clients never learn the device was
lost.** `HeliosNvrmHeader.epoch` is `VirtioGpu::nvrm_epoch()` (`virtio/gpu/nvrm_tables.rs`), which was the bare `wire_fence_base`,
taken from `NEXT_WIRE_FENCE_BASE` (`virtio/gpu/mod.rs:1765`), a STATIC that starts at 1. `pnputil /restart-device` reloads the image
(`StartN` 1), so every image's first transport had epoch 1. The client contract (`helios_nvrm_reply_is_lost`,
`guest/rmclient/src/helios_nvrm_escape.h:488`) is: a reply whose epoch differs from the one QUERY_CAPS gave at init means the device
is lost, latch it, answer `-ENODEV` from then on, and the process reopens once. With equal epochs nothing latched. The processes kept
their device (DWM's librmclient device is a private `D3DKMTCreateDevice` that lives until the process exits), their RM client, file
and GEM numbers of the old generation, and used them against the new generation's empty tables:

* `Ioctl` / `Close` / `ScanoutFlip` of an old handle: header status `NOT_OWNED` (1), librmclient errno `EBADF` (`owned()`,
  `virtio/nvrm.rs:239`; the handle tables are per transport and empty). There is no other status for "an owner of an older
  generation": owners are not registered at CreateDevice, they appear at their first `Open` (`reserve_nvrm_handle_slot`).
* `Open`: succeeds (a fresh slot) and returns a handle number the new generation hands out from the start. Handle numbers, RM
  client ids and GEM numbers therefore RESTART and collide with the stale ones a client still believes it holds. That is the
  `open` 9620 / `close` 9681 / `ioctl` 194739 storm, and it is also how a stale `ScanoutFlip { owner_handle, host_handle }` can name a
  live DRM file of the new generation and a GEM number that is now another object of the same process (DWM's own icon atlas):
  the `MSG_SCANOUT_FLIP` arm (`virtio/nvrm.rs:444`) checks only that `owner_handle` is the caller's and a DRM node.
* There IS a clean "device lost" signal and it is the epoch itself: `TRANSPORT_RESET` (4) is produced only by `EVENT_REGISTER`;
  `STATUS_DEVICE_NOT_READY` only by the event ops with no transport; `STATUS_DEVICE_REMOVED` only when the adapter is removed.
  `helios_nvrm_reply_is_lost` and `helios_nvrm_ntstatus_is_lost` treat a different epoch, `TRANSPORT_RESET`, 0xC00000A3 and 0xC00002B6
  as fatal for the process. The in-tree header carries the rules; the transport that applies them (`transport_windows.c`) is on the
  librmclient branch `rmc/transport-loss` (`nvrm-escape.md` section 9). A librmclient without it still loops.

Fix (next build, `kmd_logic::generation_id`): the epoch is `(image_salt << 24) | generation index`, the salt being the interrupt time in
~0.1 s units read once per image (`adapter::image_salt`), never 0. The first reply of a surviving client now differs from its init
epoch and it takes the loss path once. Predicts: `GenEpoch` (low 32 bits, written at every StartDevice) differs between restarts;
`NvRef` (NOT_OWNED refusals) no longer climbs by thousands after a restart; host `open` / `ioctl` per generation stays small.

**R2 (certain defect, fixed; reachable only if dxgkrnl kept allocation handles across the reload): the allocation serial repeats the
same way.** `TRANSPORT_SERIAL` (`adapter/mod.rs:511`) is a static starting at 0, so each image's first generation had serial 1.
`resolve_current_alloc` (`ddi/create_allocation.rs:768`, `paging::alloc_is_current`) refuses an allocation whose `serial` differs from
the current generation's: after a reload an older image's allocation compared EQUAL. `scanout_alloc_info`
(`ddi/create_allocation.rs:1230`) then returned its `resource_id` as the primary's, and the direct arm
(`ScanoutTarget::from_direct_primary`, `ddi/display.rs:3551`) bound and flushed that id: a resource id of the old generation that names
a different live resource of the new one (ids restart at 1). The host accepts it when its size covers the layout
(`SET_SCANOUT_BLOB` checks in `host/backend/device/src/venus/scanout.rs:138-146`: an icon atlas of 4096x4096 RGBA covers a 5120x1440
stride, and the noise block is the same bytes read with the wrong stride). Fixed by the same salt (`transport_serial`). Predicts: a
stale handle in use after a restart with `FkGen` 0 and `PgStale` 0 (both stay 0 while the serials collide; with the fix they count).
Not provable without a dump of `ScRid` and the allocation's `resource_id`; the defect is certain, its role in this incident is not.

**R3 (certain gap, fixed for the Venus bind): StopDevice never turned the host's scanout binding off.** `dxgkddi_stop_device`
(`ddi/lifecycle.rs`) calls `reset_display_publication_state` (`adapter/mod.rs:1378`), which only zeroes the guest's views
(`host_bound_scanout_resource`, `active_scanout_*`), then drops the transport. The host's device reset (`Venus::reset`,
`host/backend/device/src/venus/mod.rs:469`) calls `release(window, None, tell)`: with `display` None the scanout is forgotten but
`link.disable()` (`venus/mod.rs:443`) is not run, and `teardown_scanout` (`nvidia/scanout.rs:183`) only clears the dma-buf cache; the
viewer window "keeps the last frame" (`display.rs:2015-2019`). So after a restart the viewer shows the old generation's last image
until a flip of the new one arrives, and in the incident none did. New: `stop_unbind_scanout` (`ddi/lifecycle.rs`, before the reset,
inside the StopDevice budget) sends `SET_SCANOUT_BLOB` with resource 0 (`ctrl::disable_scanout_within`) when the guest's host-bound id
or the host's last accepted bind names a resource. Predicts: `StopUnbSt` 1 after every restart that had a Venus-bound desktop.
Limits: a desktop shown by a `ScanoutFlip` (`ForeignFlip`, the KMD RM client) has no Venus binding, so SET_SCANOUT 0 finds nothing to
disable on the host (`venus/scanout.rs:111-117` disables only a scanout that was set). The host does have a `ScanoutDisable`
message (`nvidia/scanout.rs`, `handle_scanout_disable`) that nothing in the guest sends yet: that is the follow-up for ForeignFlip,
and the host side (`Venus::reset` / `teardown_scanout` calling `disable` on a reset) is the other half. Both are outside this tree.

**R4 (open, low): a fresh adapter-owned LINEAR scanout target shown before its first copy.** `production_linear_scanout`
(`ddi/display.rs:69-149`) allocates a new blob in the new generation and nothing clears it; uninitialised device memory showing
earlier contents (noise and an icon atlas are what freed texture memory looks like) would look like the incident. The flush is
supposed to follow the copy's completion (`queue_active_scanout_refresh` docs), and `gpu_cmd` 11 says no copy ran, so it is possible
only if something flushed without the copy. Decided by: `ScRid` equal to `CpRid` (the dedicated target), `CpErr`, and the flush
histogram (`FfR<n>` / `FfC<n>` / `FfTot`, `FLUSH_HISTOGRAM`, `ddi/scanout_trace.rs:310`) naming a resource id that is not the one
`ScRid` bound.

**R5 (refuted by construction): `BltAsync` / `BltNoMirror`.** The deferred queue (`virtio/gpu/blt_async.rs:241`) dies with the
transport and the knobs are read again at every start (`ddi/blt_async.rs:127`); the system-copy invalid marks and system-backing
leases are cleared at stop (`reset_system_backings`, `adapter/mod.rs:1843`, called from StopDevice); the Blt arm only writes the
destination's backing and its RM sysmem image and never issues a SET_SCANOUT or a flush. It cannot make the host show another
resource. `BltNoMirror` 1 was merely a knob set in the run.

**The RestSeed arm (the issue's H1 tail)**: confirmed harmless. `RestSeed` seeds `last_primary_address` only (`adapter/mod.rs:1442`,
`:1510`, `restart_flip::seed_address`); it is read by the heartbeat and by `same_active_identity` (`ddi/display.rs:3575`, which also
requires `already_bound`, false after the reset). No bind or flush takes an id from it: `queue_active_scanout_refresh_locked` flushes
only `active_scanout_resource`, which `reset_display_publication_state` zeroes and only a bind of THIS generation sets.

### 25.4 The Heaven out-of-memory (row A), briefly

Nothing in the KMD returns a D3D "out of memory" for the texture create by itself; the candidates are (a) the 32-bit process's own
address space (x86 Heaven at 1.6 GB private bytes) and (b) a KMD refusal that surfaces as `STATUS_NO_MEMORY` from a CreateAllocation or
as `NO_RESOURCES` / `EMFILE` from an NVRM `MMAP`. Read, in this order, after a repro: `NvWinRFull` (window full), `NvWinRRes` (the
256 MiB reserve), `NvWinRBig` (one map larger than the window), `NvWinRAddr` (user address space: the x86 process limit), `NvWinRTab`
(mapping table), `NvWinRHost` with `NvWinHErrno` (the host refused); `NvMapTRef` (mapping-table bound), `NvHdlLive` / `NvHdlPeak` /
`NvHdlCap` / `NvHdlORef` / `NvHdlFRef` / `NvHdlGRef` (handle table), `CrPrivSmall` (CreateAllocation private data too small: a
UMD/KMD mismatch, not memory), `ShPhFail` (shared physical backing). Row A was also the first load after a restart, so R1 (a surviving
client or shell process looping on stale handles and consuming handle and window quota) is a better explanation than a limit:
`NvWinUseMb` / `NvHdlLive` high with `gpu_cmd` near 0 would say so.

### 25.5 Breadcrumbs for the next occurrence

New: `GenSalt`, `GenSerial`, `GenEpoch` (StartDevice: the image salt, the allocation serial and the NVRM epoch, low 32 bits);
`StopUnbGst` (the guest's host-bound resource at the stop), `StopUnbAct` (its active resource), `StopUnbHst` (the host's last accepted
bind), `StopUnbSt` (0 nothing bound, 1 disabled, 2 refused or failed, 3 no budget left). Existing, to read with them: `StartN` /
`StartT`, `SaLo` / `SaHi` / `SaCnt`, `VpLpa`, `ScRid` / `ScPub` / `ScSrc` / `ScDir`, `CpRid`, `RfRid` / `RfCnt` / `RfUnb`, the flush
histogram (`FfR<n>`), `FlipIss` / `FlipPub`, `FfProg` / `FfNoRec` / `FfMoved` / `FfReowned`, `FkGen`, `PgStale`, `NvOpen` / `NvClose` /
`NvIoctl` / `NvRef` / `NvStale` / `NvStaleUn` / `NvSwept`, `RestSeed*` / `ScRest*`, `PBFlip` / `PBRetSite`. Reading order: `StartN` (1 =
image reloaded), `GenEpoch` against the previous run's (equal = R1 not fixed or not running), `StopUnbSt` / `StopUnbHst` (what the host
was bound to at the stop), `ScRid` / `ScPub` / the histogram (what this generation bound and flushed), `NvRef` / `NvOpen` (the
reconnect loop).

### 25.6 Verified, and not

The pure logic (`generation_id`) is host-tested (the salt, the serial and the epoch differ between two images loaded minutes apart and
increase strictly within one; a stale serial is refused by `paging::alloc_is_current`; the whole kmd_logic and protocol suites pass).
`kmd_render` cannot be built here: the stub type-check harness reports the same distinct error lines before and after. NOT verified:
that a surviving client reacts to the new epoch as the header says (needs the librmclient branch and the Windows transport); that the
StopDevice disable does not lengthen a restart (it is bounded by the StopDevice budget and sent only when something is bound);
anything on hardware. Two statements elsewhere are wrong across an image reload: the doc comment of
`reset_display_publication_state` and the row "TRANSPORT_SERIAL, NEXT_WIRE_FENCE_BASE: monotonic, KEPT" of section 20.4. Both restart at
zero with the image, which the salt now covers.

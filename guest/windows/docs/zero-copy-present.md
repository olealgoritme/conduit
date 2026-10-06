# Zero-copy presentation of NVK-on-RM images (Windows guest, KMD side)

Status: KMD half implemented behind a closed gate (`RM_IMPORT_SERVED = false`);
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
- Cross-process lifetime of an imported foreign resource that two devices of one process use:
  the importing device's DestroyDevice frees it even if the bridge's device still imports it.

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
(`resource_is_live`) still fails an open of a dead resid. The creator's `RELEASE_BLOB` after adoption
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
   the size. Not rewritten here.
2. **Layout on the host wire.** The 56-byte `RESOURCE_CREATE_BLOB` has no room for the layout; the host
   learns it from the GEM object or from a new message. Needed before the gate opens.
3. **KMD-driven `ScanoutFlip`** in `program_vidpn_source_inner` (plan 3.6 Option B): read
   `foreign_layout` for `stride`, `fourcc`, `modifier`.
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
| ABI: `HeliosForeignLayout` 32, `HeliosForeignImportRmLayout` 104, `HeliosWddmAllocLayout` 32 at offset 96; C mirror | `protocol/src/foreign.rs`, `protocol/src/wddm.rs`, `protocol/include/helios_foreign.h` | Rust `const` asserts + `cargo test`; `gcc -m32/-m64 -Wall -Wextra -Werror` on the header |
| escape parse of the layout tail, import with layout, `foreign_layout`, `adopt_for_allocation` glue (re-ownership, quota, wrong context, dropped context, declared/record agreement, legacy path) | the real `escape_foreign.rs`, `virtio/foreign.rs`, `gpu/foreign_tables.rs` | compiled and run against a stub of the surrounding crate (not in the tree), gate flipped in the copy only |
| `create_allocation.rs`, `submit_command.rs`, `gpu/mod.rs`, `device.rs` edits | - | rustfmt parse and review only; **never compiled**. Nothing here has run on a Windows guest. |

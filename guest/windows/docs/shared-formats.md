# Shared formats beyond 32 bpp (NVK to NVK)

An NVK process shares a D3D11 texture with another process by minting a KMD
resource id for its dedicated `VkDeviceMemory` (`IMPORT_RM`, NVK patches 0025-s3
and 0031, `shared-surfaces.md`). Until now the id carried a layout for the four
32 bpp RGB formats only. `docs/dwm-on-nvk.md` 4.2 shows why that keeps the shell
and the browsers on Venus: DWM opens `A8_UNORM` shell surfaces, and browsers and
video apps share NV12/P010 video textures and fp16 / 10-bit HDR surfaces. This
document settles the format set, the layout record for it, the KMD change
(owned by the KMD session), and the guest side.

## 1. Which formats Windows actually shares

| source | formats shared across processes |
|---|---|
| DWM / shell (T1 log, `dwm-on-nvk.md` 1 and 6) | `B8G8R8A8` (87) window and swap-chain surfaces; `A8_UNORM` (65) DirectComposition / XAML mask and glyph atlases (800x704, 704x704, 32x32) |
| DWM advanced colour (HDR / WCG desktops, HDR apps' DComp surfaces) | `R16G16B16A16_FLOAT` (10, scRGB), `R10G10B10A2_UNORM` (24, HDR10 / 10-bit swap chains) |
| Chromium / Edge / WebView2 / Electron / CEF (D3D shared images, GPU process to DComp, video) | `B8G8R8A8`, `R8G8B8A8`, `R8_UNORM` (Alpha8 / Luminance8 resources), `R8G8_UNORM` (RG88), `R16_UNORM` and `R16G16_UNORM` (per-plane P010/P016 views), `B5G6R5_UNORM` (RGB_565 resources), `R10G10B10A2_UNORM`, `R16G16B16A16_FLOAT`; video: `NV12` (103), `P010` (104), `P016` (105) shared decoder / VideoProcessor textures and DComp video overlays |
| Firefox (WebRender, DComp, RDD process) | `B8G8R8A8`, `R8`, `R16`, `NV12`, `P010`, `P016`, `R16G16B16A16_FLOAT` |
| Media Foundation (players, Photos, Teams camera) | `NV12`, `P010`, `YUY2` (107, camera capture) |

Not taken, on purpose: `R11G11B10_FLOAT`, `AYUV`, `Y410`, `Y416`, `Y210`, `Y216`,
`NV11`, `P208`, `V208`, `420_OPAQUE` (no DRM fourcc or no NVK format, and none of
the components above shares them), 3-plane formats, depth, block-compressed,
MSAA, arrays and mip chains (a shared surface is one 2D subresource).

## 2. The set and its codes

The layout record's format code stays the DRM fourcc. `bpp` is bytes per texel
unit of the plane; plane 1 of the 4:2:0 formats is `ceil(w/2) x ceil(h/2)`
chroma pairs.

| fourcc | value | planes | p0 bpp | p1 bpp | DXGI formats (UMD/DXVK side) |
|---|---|---|---|---|---|
| `XRGB8888` | `0x34325258` | 1 | 4 | - | `B8G8R8X8_*` 88, 93 (existing) |
| `ARGB8888` | `0x34325241` | 1 | 4 | - | `B8G8R8A8_*` 87, 90, 91 (existing) |
| `XBGR8888` | `0x34324258` | 1 | 4 | - | (existing) |
| `ABGR8888` | `0x34324241` | 1 | 4 | - | `R8G8B8A8_*` 27..32 (existing) |
| `R8` | `0x20203852` | 1 | 1 | - | `A8_UNORM` 65, `R8_*` 60..64 |
| `GR88` | `0x38385247` | 1 | 2 | - | `R8G8_*` 48..52 |
| `R16` | `0x20363152` | 1 | 2 | - | `R16_*` 53..59 |
| `GR1616` | `0x32335247` | 1 | 4 | - | `R16G16_*` 33..38 |
| `RGB565` | `0x36314752` | 1 | 2 | - | `B5G6R5_UNORM` 85 |
| `ARGB1555` | `0x35315241` | 1 | 2 | - | `B5G5R5A1_UNORM` 86 |
| `ARGB4444` | `0x32315241` | 1 | 2 | - | `B4G4R4A4_UNORM` 115 |
| `ABGR2101010` | `0x30334241` | 1 | 4 | - | `R10G10B10A2_*` 23..25 |
| `ABGR16161616F` | `0x48344241` | 1 | 8 | - | `R16G16B16A16_FLOAT` 10, `_TYPELESS` 9 |
| `ABGR16161616` | `0x38344241` | 1 | 8 | - | `R16G16B16A16_UNORM/UINT/SNORM/SINT` 11..14 |
| `YUYV` | `0x56595559` | 1 | 4 per 2 px | - | `YUY2` 107 (even width) |
| `NV12` | `0x3231564E` | 2 | 1 | 2 | `NV12` 103 (even width and height) |
| `P010` | `0x30313050` | 2 | 2 | 4 | `P010` 104 (even width and height) |
| `P016` | `0x36313050` | 2 | 2 | 4 | `P016` 105 (even width and height) |

The fourcc only says how the bytes lie; the D3D view format (A8 against R8,
UNORM against FLOAT for `R16`) travels in `HeliosWddmAllocMeta::dxgi_format` as
before. The Rust table is `helios_protocol::share_format`, the C mirror the
`HELIOS_DRM_FORMAT_*` list in `helios_foreign.h`.

## 3. Design

**One resource id per image, plane 1 in the layout record.** DXVK puts every
plane of a D3D11 texture in one dedicated `VkDeviceMemory` (it never makes
disjoint images), so one RM object holds both planes and the KMD names objects.
One id per plane would need two WDDM allocations for one D3D resource (D3D11
shares one allocation per texture), two host imports and two lifetimes to keep
in step, for no gain. So the record carries plane 0 as today plus an optional
plane 1 (offset, pitch, its own modifier).

**Plane 1 has its own modifier.** NVK picks the block height per plane from the
plane's extent, so plane 1 of a 1080p NV12 may use a smaller `h` than plane 0.
The PTE kind is the same generic colour kind (`0x06`) for every colour format
on GB20x, so the modifier family stays `0x0300000000606010 | h`. LINEAR planes
stay LINEAR together.

**Single-plane formats need no new bytes.** They fit today's 32-byte layout
and the 128-byte private data unchanged; only the KMD's fourcc check and stride
rule change. Only the two-plane formats need 16 more bytes, in the request and
in the WDDM trailer.

**Linear and block-linear.** Both, per plane, as NVK builds them. NVK exports
an image that would be compressed only uncompressed (the export request turns
compression off), so the modifier never names a compressed kind.

**Fail cleanly.** NVK refuses (`VK_ERROR_FORMAT_NOT_SUPPORTED`, no escape) a
format outside the table, a 3-plane or disjoint image, more than one mip or
layer, MSAA, a plane with a tiling outside the family, and, without the KMD
cap, anything but 32 bpp RGB. The UMD then makes the KMD placeholder allocation
it makes today for an id-less texture, and the opener sees an ordinary
non-foreign resource (refused on NVK, blank placeholder in an NVK DWM). A SHARED
placeholder is created host-less by the KMD (no Venus buffer, no identity;
`shared-foreign-surfaces.md` section 11), so it succeeds without a resource id.

## 4. Protocol (on this branch, `guest/windows/protocol`)

* `HELIOS_FOREIGN_CAP_LAYOUT_FORMATS = 1 << 3` in `QueryCaps.caps_flags`
  (`foreign.rs`, `helios_foreign.h`): the KMD takes the table above, the
  `PLANE1` tail, and writes the version-2 trailer for two-plane records.
* `HELIOS_FOREIGN_IMPORT_FLAG_PLANE1 = 1 << 1`: only with `FLAG_LAYOUT`. The
  request is then `HeliosForeignImportRmPlanes` (120 bytes): the 104-byte
  `HeliosForeignImportRmLayout`, then

  ```rust
  #[repr(C)]
  pub struct HeliosForeignPlane { // 16 bytes, at request offset 104
      pub modifier: u64,          // @0  DRM_FORMAT_MOD_* of plane 1
      pub stride: u32,            // @8  plane 1 row pitch, bytes
      pub offset: u32,            // @12 plane 1 offset from the object start
  }
  ```

  Only the first 72 bytes are written back, as today.
* WDDM trailer (`wddm.rs`): a single-plane record of any format is the version-1
  `HeliosWddmAllocLayout` at private offset 96 (128 bytes of private data), as
  today. A two-plane record is the same 32 bytes with `version = 2`
  (`HELIOS_WDDM_LAYOUT_VERSION_PLANES`) and `reserved = 2` (the plane count),
  then `HeliosWddmAllocPlane { modifier: u64, stride: u32, plane_offset: u32 }`
  at offset 128 (`HELIOS_WDDM_LAYOUT_PLANE1_OFFSET`), 144 bytes of private data
  (`HELIOS_WDDM_PRIVATE_WITH_PLANES_BYTES`). A version-1 reader refuses
  version 2 (`is_valid`), so an older opener falls back instead of misreading;
  `HeliosWddmAllocLayout::read_open_planes` reads the new form. The meta trailer
  is unaffected: `MetaLayout::from_trailer_len(96)` is `Full48`.

## 5. KMD change request (owner: the KMD session)

Implemented on `kmd/shared-formats`; section 9 says what was built, where it differs from this
list (nothing in the wire, a few choices the list left open) and how to check it on hardware.

Files and rules, exactly:

1. `kmd_logic/src/foreign_resource.rs`
   * `Layout` gains `plane1: Option<Plane>` with
     `pub struct Plane { pub stride: u32, pub offset: u32, pub modifier: u64 }`.
   * `Layout::validate` takes the fourcc table from
     `helios_protocol::share_format` (unknown: `LayoutError::Format`). When the
     KMD does not advertise `CAP_LAYOUT_FORMATS` it keeps today's four-format
     check.
   * Per plane `p` (plane 1 extent `ceil(w/2) x ceil(h/2)`):
     `stride_p >= ShareFormat::row_bytes(p, width)`,
     `stride_p % ShareFormat::stride_align(p) == 0` (1, 2 or 4),
     `stride_p <= MAX_STRIDE`, else `LayoutError::Stride`.
   * `modifier_p` is `MOD_LINEAR` or `MOD_NVIDIA_BLOCK_LINEAR_BASE | h`,
     `h <= 5`; plane 1 is LINEAR iff plane 0 is; else `LayoutError::Modifier`.
     `h` may differ between the planes.
   * `min_bytes_p = offset_p + stride_p * rows_p`, rows rounded up to the plane's
     own block (`8 << h_p`) when block-linear.
   * `plane1.is_some() == (planes == 2)`, else a new `LayoutError::Planes`.
   * `plane1.offset >= min_bytes_0` (no overlap, plane 1 after plane 0) and
     `min_bytes_1 <= size`, else `LayoutError::TooLarge` (with
     `min_bytes_0 <= size` as today).
   * `ShareFormat::even_width` / `even_height`: odd extent is
     `LayoutError::Dimensions`.
   * `validate_request`: `flags & !(FLAG_LAYOUT | FLAG_PLANE1) != 0` is
     `RequestError::Flags`; `FLAG_PLANE1` without `FLAG_LAYOUT` is
     `LayoutRequired`; the plane tail goes into `Layout::plane1`.
2. `kmd_render/src/ddi/escape_foreign.rs` (`IMPORT_RM`, the
   `HeliosForeignImportRmLayout` bind near line 183): with `FLAG_PLANE1` bind the
   120-byte `HeliosForeignImportRmPlanes` (a shorter buffer is `BAD_RANGE`) and
   pass `plane1`. `QUERY_CAPS` sets `HELIOS_FOREIGN_CAP_LAYOUT_FORMATS`.
3. `kmd_render/src/ddi/create_allocation.rs`
   * Adoption of a record with `plane1`: require
     `PrivateDriverDataSize >= HELIOS_WDDM_PRIVATE_WITH_PLANES_BYTES` (144), else
     refuse as today's too-short foreign buffer.
   * `write_foreign_layout_trailer` (create and every open): a single-plane
     record writes version 1, `reserved = 0` (unchanged); a two-plane record
     writes version 2, `reserved = 2`, and `HeliosWddmAllocPlane` at 128.
   * `read_layout_trailer` (the creator's optional hint): accept version 2 and
     compare plane 1 as well.
   * The meta's `pitch` / `plane_offset` stay plane 0's.
4. 32 bpp-only consumers must refuse other records, never misread them:
   `foreign_scanout` (its own four-format `Layout`, keep), `foreign_copy`
   (`vk_format_for_fourcc` / `layout_from_open` answer `None` for other fourccs:
   keep it so, and treat a record with `plane1` as not copyable), `rm_blt`
   (`order_for_fourcc`, same), and the K1 `ForeignFlip` arm (32 bpp primaries
   only).
5. Tests: one per rule above in `foreign_resource.rs` (R8 1920x1080 linear and
   block-linear, fp16 stride 8 * w, NV12 1920x1080 with plane 1 at
   `min_bytes_0`, overlap refused, odd height NV12 refused, PLANE1 flag with a
   one-plane fourcc refused, plane-1 LINEAR with plane-0 block-linear refused).

## 6. Host

No change. Msg 31 (`nvidia/rm_resource.rs`, `RmResourceImport`) and the
RM-export blob (`venus/rm.rs`, `create_rm_blob`, `rm_resource`) move one
dma-buf / GEM object and one modifier; they never look at the format or the
planes. The modifier they report is the one the creator's GEM import had
(plane 0's), which is also what the KMD record holds for plane 0, so the
opener's modifier check in NVK (`nvkmd_rm_mem_import_resource`) still holds.
A Venus process importing a non-32 bpp NVK surface (`ForeignImport`, DXVK
patch 0001) is a separate path (Venus explicit-modifier import, one plane) and
is not covered here.

## 7. Guest side

* **librmclient**: `crm_win_import_rm_planes` sends the 120-byte request (the
  104-byte one when there is no plane 1); `crm_win_import_rm` is unchanged.
* **NVK** (patch 0041): `memory_res_id` maps the image's Vulkan format to the
  table, builds plane 0 and plane 1 from the nil layout, and asks for the
  non-32 bpp path only when `QUERY_CAPS` shows `CAP_LAYOUT_FORMATS`; the export
  memory of a two-plane image gets plane 0's PTE kind. The opener's check
  compares plane 1 too when the import info carries it. Interface version 4:
  `HELIOS_ICD_CAP_LAYOUT_FORMATS` (bit 8) and `memory_res_plane1`. Scanout
  stays 32 bpp.
* **UMD**: a two-plane foreign texture gets 144 bytes of private data with the
  version-2 trailer; the opener reads `read_open_planes` and passes plane 1 to
  DXVK (patch 0009) and on to NVK.
* **Test**: `guest/windows/tools/d3d11_share.cpp`, mode `fmt`, see section 8.

## 8. Tests

`d3d11_share.exe fmt <a8|r8|r8g8|r16|r16g16|b5g6r5|r10g10b10a2|rgba16f|nv12|p010|yuy2> [kmt|nt] [w h]`:
A creates a shared texture of the format, fills it with a byte pattern
(`UpdateSubresource`, every plane), starts B; B opens it, reads every byte back
through a staging copy, writes a second pattern, waits for its GPU work; A reads
B's pattern back. Pixel-exact both ways. `HELIOS_ICD=nvk` for both processes.

Results: section 11.

## 9. KMD implementation

What was built, as three layers; everything is in `guest/windows`, nothing in the host.

**Pure logic (`kmd_logic`, host tests).**

* `foreign_resource.rs`: `Layout::plane1: Option<Plane{stride, offset, modifier}>`, the format table
  (`share_format`, a copy of `helios_protocol::share_format`: `kmd_logic` has no dependency edge to
  the protocol crate, so `kmd_render` pins the two with a const assertion over every fourcc of the
  table and its neighbours, `escape_foreign.rs`), `LayoutError::Format` / `Planes` (appended),
  per-plane stride / modifier / extent rules exactly as section 5, `FLAG_PLANE1`.
  `min_bytes` is the larger of the two planes' bounds and **saturates**: an unvalidated layout with
  `u32::MAX` stride and height can no longer wrap `stride * rows`.
* The choices section 5 left open: plane 1 starting inside plane 0 is `LayoutError::TooLarge` (as
  the list says) and is checked in `validate()`, so it does not depend on the object size;
  `FLAG_PLANE1` against the decoded tail (flag without tail, tail without flag) is
  `RequestError::Flags`, while a two-plane fourcc without the flag, or a one-plane fourcc with it, is
  `Layout(Planes)`; `FLAG_PLANE1` without `FLAG_LAYOUT` is `LayoutRequired`; a 120-byte request in a
  shorter buffer is `BAD_RANGE` (counted), not an escape failure. The 32-bit RGB check is unchanged:
  the new code is pinned against a verbatim copy of the old rules over a 215 000 point grid.
* Adoption: `AdoptRequest::plane_room` (private data of 144 bytes or more) and
  `AdoptRefusal::NoPlaneRoom` (code 12, appended; `FgAdRf` shows it). `trailer_bytes(layout)` is
  128 or 144.
* Every status the import answers for a layout fault is `BAD_RANGE`
  (`ImportError::BadRequest`); `foreign_errno.rs` classifies *host* errnos only and needed no
  change.

**Driver (`kmd_render`, type-checked against the stub harness only: see "Not verified").**

* `escape_foreign.rs`: `QUERY_CAPS` sets `HELIOS_FOREIGN_CAP_LAYOUT_FORMATS` on the same gate as
  `CAP_RM_IMPORT` (`rm_import_served`); `IMPORT_RM` binds the 120-byte
  `HeliosForeignImportRmPlanes` when `FLAG_PLANE1` is set; only the 72-byte base is written back.
* `create_allocation.rs`: a two-plane record needs 144 bytes of private data or the adoption is
  refused (counted). `write_foreign_layout_trailer` writes version 2 with `reserved = 2` and plane 1
  at 128 for a two-plane record, at create and at every open, and writes nothing if the room is
  short; one-plane records keep writing version 1. `read_layout_trailer` reads plane 1 of a version-2
  trailer (`read_open_planes`) and the creator's hint is compared with it too. The meta's `pitch` /
  `plane_offset` stay plane 0's. The allocation size is the recorded, host-verified size, which
  `validate_for` has already proven at least `min_bytes()`.
* The 32-bit consumers refuse the new records and count it, none of them reads one as BGRA:

| consumer | decision | refusal | counter |
|---|---|---|---|
| ForeignFlip (`foreign_flip::decide`) | before `flip_layout`, which has no plane 1 | `Why::SharedFormat` (15, appended) | `FfRef15` |
| foreign copy (`foreign_copy`) | `vk_format_for_fourcc` is `None`; a record with a plane 1 never builds an image; `layout_from_open` is `None` | `Refusal::Format` (6), `Layout(Planes)` (11, appended) | `FcRefuse`, `FcRefCode`, `FcNotRgb32` |
| SCANOUT_SET / resident set (`foreign_scanout`) | its own 32-bit validator | `SetError::Layout(Format)` | `FsRef`, `FsFmtRef` |
| level 5 Blt (`rm_blt::order_for_fourcc`) | `None` for every fourcc but the four | `Skip::Layout` | the blt skip counters |
| level 5 primary (`rm_sysmem::layout`) | only DXGI 28, 87, 88 | `LayoutError::Format` | (creation falls back to Venus) |

**No overlay planes, which is why NV12 is never scanned out.** dxgkrnl hands a YUV surface to the
display hardware only through a multi-plane-overlay present. The KMD never advertises one: the
adapter reports WDDM 2.1 + GpuMmu (`wddm_surface.rs`), `DXGK_DRIVERCAPS.SupportMultiPlaneOverlay`
is never written (it lies past the last field `query_adapter_info.rs` writes, `SupportDirectFlip`,
and the field-by-field writer would refuse and count a write there), and the MPO3 KMD interface is
not registered (registering it needs the 3.2 level, where DWM fails with `E_NOTIMPL`:
`wddm_surface.rs` module docs). A present flagged `FlipWithMultiPlaneOverlay` would be refused
(`PresentPayload::MultiPlaneOverlay`, `present_packet.rs`). So an NV12 / P010 / P016 (or any other shared-format)
allocation is only ever opened and sampled, never programmed as a scanout source; if one were, the
flip arm and the copy path would refuse it by the table above. The test
`foreign_resource::shared_format_tests::the_kmd_never_advertises_overlay_planes` scans every
non-comment line of `kmd_render/src` and fails if any names `SupportMultiPlaneOverlay`,
`CheckMultiPlaneOverlay`, `SetVidPnSourceAddressWithMultiPlaneOverlay`, `MaxOverlay` or `MPO3`, or
names a multi-plane overlay anywhere but `present_packet.rs` (where it is the refused arm).

**Counters** (service key values, written by the throttled foreign-resource publish; all at most 13
characters and checked against every other `b"..."` name in `kmd_render` and `kmd_logic`). None is
bumped by a 32-bit RGB record, and each is also included in the older total named in the last
column.

| counter | counts | total it is part of |
|---|---|---|
| `FgImpFmt` | imports of a one-plane record beyond the four 32-bit formats | `FgImp` |
| `FgImp2P` | imports of a two-plane record | `FgImp` |
| `FgAdo2P` | adoptions of a two-plane record | `FgAdo` |
| `FgRefFmt` | requests refused for a fourcc outside the table | `FgRefR` |
| `FgRefPln` | requests refused because the plane tail and the format disagree (also a 120-byte request in a short buffer) | `FgRefR` |
| `FgRefNewG` | requests for a known shared format refused for geometry: odd extent, stride, modifier, overlap, size | `FgRefR` |
| `FgAdoNoPln` | two-plane adoptions refused for private data under 144 bytes | `FgRefA` (and `FgAdRf` = 12) |
| `FgTrl2W` | version-2 trailers written, at create and at every open | |
| `FgTrl2NoRm` | version-2 writes that found the buffer short (wrote nothing) | |
| `FgTrl2Rd` | version-2 trailers read (the creator's hint, the KMD's own record at an open) | |
| `FfRef15` | ForeignFlip refusals: a shared format | `FfRef` |
| `FcNotRgb32` | foreign copy refusals: not a format it can carry | `FcRefuse` |
| `FsFmtRef` | `SCANOUT_SET` refused for a fourcc outside the four | `FsRef` |

**Tests** (`cargo test` in `kmd_logic`, 928, of which 40 are new; `protocol` unchanged, 31):
`foreign_resource::shared_format_tests` has one test per rule of section 5 and the cases around
it: the table row by row, every one-plane format valid at its limits and refused one byte under,
over the cap and off its alignment, extents per format (odd width YUYV, odd NV12 on both axes), plane
1 strides per format, R8 1920x1080 linear and block-linear, fp16 stride `8 * w`, NV12 / P010 / P016
1080p with plane 1 at `min_bytes_0`, every `h0`/`h1` pair, mixed LINEAR / block-linear refused,
overlap refused (and the boundary accepted), `Planes` in both directions, the request flags,
hostile values (u32 / u64 extremes in every format, `stride * rows` overflow, 16384 x 16384 fp16
over the 1 GiB cap, the largest accepted layout), the four 32-bit formats against the old rules,
the counters, the 144-byte room rule, the supplied trailer repeating plane 1. Each consumer has a
table of non-32-bit layouts refused by its pure decision (`foreign_flip`, `foreign_copy`,
`rm_blt`, `foreign_scanout`, `rm_sysmem`). Two assertions of the older tests changed because the
values they used as "unknown" are real now: bit 1 of the import flags is `FLAG_PLANE1`, and RGB565
and the fp16 fourcc are accepted (unknown fourccs in those tests are `BG24` and `XR30`), and one
ForeignFlip table row (NV12) now expects `SharedFormat` instead of `BadLayout`.

**Hardware checklist** (set nothing: there is no knob; the KMD cap follows the host's `IMPORT_RM`):

1. `QUERY_CAPS` shows bit 3 (`CAP_LAYOUT_FORMATS`) together with bit 0 (`CAP_RM_IMPORT`). Without the
   host's import neither is set, and a client mints ids for nothing.
2. Run `d3d11_share.exe fmt a8` (both processes `HELIOS_ICD=nvk`), then read the counters:
   `FgImp`, `FgImpFmt`, `FgAdo` and `FgOpen` each grew by one per shared texture; `FgImp2P`,
   `FgAdo2P` and `FgTrl2W` did not move (A8 is a one-plane record, version-1 trailer);
   `FgRefFmt`, `FgRefPln`, `FgRefNewG`, `FgAdoNoPln` and `FgAdRf` stayed 0.
3. Run `d3d11_share.exe fmt nv12`: `FgImp2P` and `FgAdo2P` grew by one, `FgImpFmt` did not,
   `FgTrl2W` grew by at least two (create and B's open), `FgTrl2Rd` by at least one, `FgTrl2NoRm`
   and `FgAdoNoPln` stayed 0.
4. In both runs `FfRef15`, `FcNotRgb32` and `FsFmtRef` stayed 0 (nothing tried to show the shared
   surface), and `FgOpLive` is back to 0 once both processes exit.
5. A refused request is `BAD_RANGE` and counted: a build with a bad plane (odd height NV12, plane 1
   inside plane 0) moves `FgRefNewG`, a fourcc outside the table `FgRefFmt`.

**Not verified.** `kmd_render` cannot be compiled for the WDK here: the driver edits were
type-checked in a stub harness that copies the real module tree and replaces only the `wdk` crates,
and the set of errors it reports (the missing bindgen types) is identical before and after the
change; the const assertions that pin `kmd_logic` to the protocol are evaluated there too (a
deliberate drift in the table was seen to fail the build). Nothing has run on the guest: the
section 8 runs and the checklist above are still to do. The three commits do not build `kmd_render`
one by one: the `kmd_logic` commit adds struct fields the driver commit fills in.

## 10. LINEAR surfaces with a recorded pitch (KMD-made RM surfaces)

An NVK DWM must also open RM-backed surfaces the KMD makes (GDI redirection,
`dwm-on-nvk.md` 4.2.2), which are LINEAR with a pitch the KMD chose. The
rebuild-and-check import cannot take them: the opener's own LINEAR pitch is
NVK's (`align(width * bpp, 128)`) and DXVK builds shared images OPTIMAL.

* **NVK patch 0042**: `VK_EXT_image_drm_format_modifier` on Windows
  (`has_alloc_tiled` no longer depends on the DRM path; RM applies kinds per
  mapping). nil already takes an explicit LINEAR row pitch for an import, any
  multiple of 32 bytes (below 128 NVK uses its render workaround).
  `NVK_HELIOS_MODIFIERS=0` hides it.
* **DXVK patch 0010**: an NVK device enables the extension; an NVK import whose
  record is LINEAR, single-plane and has a pitch is created with
  `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` and an explicit plane layout
  (offset, rowPitch from the trailer), as 0001 does for Venus. Block-linear
  records (every ordinary OPTIMAL share) and two-plane records are unchanged.
* **Import check** (`nvk_helios_check_import_layout`): unchanged code; the
  explicit image has the recorded layout by construction.
* **KMD**: author 128-byte-aligned pitches for RM surfaces NVK will open
  (NVK's own LINEAR stride, no render workaround); 256-aligned also imports.

## 11. Results

2026-10-06, win11 (22.22.319.x KMD from feat/umd-nvk-combined, no
`CAP_LAYOUT_FORMATS` yet), NVK loaded per process with
`HELIOS_NVK_ICD=W:\fmt\nvk\vulkan_nouveau.dll` (patches 0041, 0042), the
installed UMD otherwise:

| run | result |
|---|---|
| NVK to NVK `bgra8`, kmt 256x128 and nt 1920x1080 (0041, 0041+0042) | byte-exact both ways (32 bpp path unchanged) |
| Venus to Venus `a8`, `r10g10b10a2`, `rgba16f`, `nv12` | byte-exact both ways (validates the tool, planes included) |
| NVK `a8`, `r8g8`, `r10g10b10a2`, `rgba16f`, `nv12` | refused as designed on this KMD (`memory_res_id` -11, no escape); the UMD's KMD placeholder is then refused by `pfnAllocateCb` (E_INVALIDARG) and the runtime reported DEVICE_REMOVED. Pre-existing: the same happens to `bgra8` with `NVK_HELIOS_RESID=0`. The UMD now answers E_OUTOFMEMORY for that one creation (commit e38d158); relayed to the KMD session |
| health | no dumps, no TDR, no app crash, DWM pid unchanged |

Pending: the non-32 bpp formats end to end need the KMD change (section 5);
then `d3d11_share.exe fmt all kmt|nt` on NVK.

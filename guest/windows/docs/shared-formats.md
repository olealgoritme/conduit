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
non-foreign resource (refused on NVK, blank placeholder in an NVK DWM).

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
* **NVK** (patch 0040): `memory_res_id` maps the image's Vulkan format to the
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

Results: section 9.

## 9. Results

(filled in as runs complete)

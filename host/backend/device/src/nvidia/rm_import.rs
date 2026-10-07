//! RM-export blobs, the backend half (docs/VENUS.md "RM-export blobs").
//!
//! NVK on RM exports an image's memory the way NVIDIA's own userspace does:
//! RM memory to a control-file descriptor, then nvidia-drm's
//! `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` on a render node the guest opened,
//! which makes a host GEM object. The guest's KMD then names that object to
//! Venus as `RESOURCE_CREATE_BLOB { blob_mem = BLOB_MEM_RM_EXPORT, blob_id =
//! rm_handle << 32 | gem_handle }`, and `crate::venus` imports it as a dma-buf
//! resource.
//!
//! Two things live here. When the forwarded GEM import succeeds, the layout
//! NVK gave NVKMS (pitch, or block-linear with `2^h` GOBs per block) is kept
//! as a DRM format modifier, keyed by `(file, GEM handle)`, because nothing
//! later can read it back: a dma-buf carries no layout. And [`RmView`] is what
//! Venus asks to turn `(rm_handle, gem_handle)` into a dma-buf: the same
//! checks and PRIME export a scanout flip uses.

use super::*;
use crate::display::prime_export;
use std::collections::HashMap;

/// nvidia-drm's `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` (nr 0x41, type 'd').
const GEM_IMPORT_NVKMS_MEMORY: u64 = 0x41;
/// `struct drm_nvidia_gem_import_nvkms_memory_params`: `mem_size` u64,
/// `nvkms_params_ptr` u64, `nvkms_params_size` u64, `handle` u32, pad.
const GEM_IMPORT_OUTER: usize = 32;
const GEM_IMPORT_HANDLE: usize = 24;

/// `struct NvKmsKapiPrivImportMemoryParams` (nvkms-kapi-private.h), 28 bytes:
/// `int memFd`, then `NvKmsKapiPrivSurfaceParams { enum
/// NvKmsSurfaceMemoryLayout layout; struct { log2GobsPerBlock {x, y, z};
/// pitchInBlocks; NvBool genericMemory; } blockLinear; }`.
const NVKMS_IMPORT_LEN: usize = 28;
const NVKMS_LAYOUT: usize = 4;
const NVKMS_LOG2_GOBS_X: usize = 8;
const NVKMS_LOG2_GOBS_Y: usize = 12;
const NVKMS_LOG2_GOBS_Z: usize = 16;
/// `NvKmsSurfaceMemoryLayout`.
const NVKMS_LAYOUT_BLOCK_LINEAR: u32 = 0;
const NVKMS_LAYOUT_PITCH: u32 = 1;

/// `DRM_FORMAT_MOD_LINEAR`.
pub const MOD_LINEAR: u64 = 0;

/// The largest `log2GobsPerBlock.y` a 2D block-linear modifier names (NIL's
/// and NVIDIA's lists both stop at 5, 32 GOBs).
const MAX_LOG2_GOBS_Y: u32 = 5;

/// Layouts kept at once. A guest that imports more without closing any
/// still works; the newest imports just carry no layout.
const MAX_LAYOUTS: usize = 4096;

/// The tiling words of `DRM_NVIDIA_GET_DEV_INFO` (`generic_page_kind`,
/// `page_kind_generation`, `sector_layout`): what a block-linear modifier
/// encodes besides the block height. GB202 answers 0x06, 2, 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tiling {
    pub kind: u32,
    pub generation: u32,
    pub sector_layout: u32,
}

/// The layout NVK handed NVKMS for one imported memory object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceLayout {
    Pitch,
    /// 2D block-linear, blocks `2^log2_gobs_y` GOBs high.
    BlockLinear {
        log2_gobs_y: u32,
    },
    /// Something no 2D DRM modifier names (3D blocks, an unknown layout).
    Other,
}

/// Read the layout out of an `NvKmsKapiPrivImportMemoryParams` block.
pub fn parse_nvkms_import(block: &[u8]) -> Option<SurfaceLayout> {
    if block.len() < NVKMS_IMPORT_LEN {
        return None;
    }
    let word = |at: usize| u32::from_le_bytes(block[at..at + 4].try_into().unwrap());
    Some(match word(NVKMS_LAYOUT) {
        NVKMS_LAYOUT_PITCH => SurfaceLayout::Pitch,
        NVKMS_LAYOUT_BLOCK_LINEAR
            if word(NVKMS_LOG2_GOBS_X) == 0
                && word(NVKMS_LOG2_GOBS_Z) == 0
                && word(NVKMS_LOG2_GOBS_Y) <= MAX_LOG2_GOBS_Y =>
        {
            SurfaceLayout::BlockLinear {
                log2_gobs_y: word(NVKMS_LOG2_GOBS_Y),
            }
        }
        _ => SurfaceLayout::Other,
    })
}

/// The DRM format modifier of `layout` on a GPU with `tiling`:
/// `DRM_FORMAT_MOD_LINEAR`, or `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c = 0,
/// s, g, k, h)` = `0x03 << 56 | 0x10 | h | k << 12 | g << 20 | s << 22`
/// (drm_fourcc.h; uncompressed, as NVK allocates). On GB202 a 1080p NVK
/// swapchain image (h = 5) is 0x0300000000606015. `None` for a layout no 2D
/// modifier names.
pub fn modifier_for(layout: SurfaceLayout, tiling: Tiling) -> Option<u64> {
    match layout {
        SurfaceLayout::Pitch => Some(MOD_LINEAR),
        SurfaceLayout::BlockLinear { log2_gobs_y: h } => Some(
            (0x03u64 << 56)
                | 0x10
                | u64::from(h & 0xf)
                | (u64::from(tiling.kind & 0xff) << 12)
                | (u64::from(tiling.generation & 0x3) << 20)
                | (u64::from(tiling.sector_layout & 0x1) << 22),
        ),
        SurfaceLayout::Other => None,
    }
}

/// Where an RM memory object lives and how the CPU caches it: the `attr`
/// word RM wrote back into `NV_MEMORY_ALLOCATION_PARAMS` when it allocated
/// the object (location in bits 26:25, CPU coherency in 31:29; RM answers
/// with what it actually did, e.g. 0x2a800000 for cached PCI sysmem).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RmPlacement {
    pub attr: u32,
}

/// `NVOS32_ATTR_LOCATION_PCI`: system memory.
const ATTR_LOCATION_PCI: u32 = 1;

impl RmPlacement {
    /// System memory (`NVOS32_ATTR_LOCATION_PCI`), as opposed to video memory.
    pub fn sysmem(&self) -> bool {
        (self.attr >> 25) & 3 == ATTR_LOCATION_PCI
    }

    /// `NVOS32_ATTR_COHERENCY_*`.
    pub fn coherency(&self) -> u32 {
        self.attr >> 29
    }

    /// The `VIRTIO_GPU_MAP_CACHE_*` a CPU mapping of the object has, or
    /// `None` for memory that is not CPU-mappable through its dma-buf as
    /// guest memory (video memory: behind BAR1). Cached and write-back are
    /// cached (the GPU snoops them: `SYSTEM_COHERENT` PTE aperture);
    /// write-combined is WC; uncached, write-through and write-protect are
    /// reported uncached.
    pub fn map_cache(&self) -> Option<u32> {
        use protocol::venus::{MAP_CACHE_CACHED, MAP_CACHE_UNCACHED, MAP_CACHE_WC};
        if !self.sysmem() {
            return None;
        }
        Some(match self.coherency() {
            1 | 5 => MAP_CACHE_CACHED,
            2 => MAP_CACHE_WC,
            _ => MAP_CACHE_UNCACHED,
        })
    }
}

/// RM memory classes whose `NV_MEMORY_ALLOCATION_PARAMS.attr` is recorded:
/// `NV01_MEMORY_SYSTEM`, `NV01_MEMORY_LOCAL_USER`.
const PLACED_CLASSES: [u32; 2] = [0x3e, 0x40];
/// `NV_MEMORY_ALLOCATION_PARAMS.attr`.
const ALLOC_ATTR: usize = 24;
/// `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` and its parameters:
/// `object.type` (1 = RM object) at 0, `rmObject.hObject` at 12, `fd` at 16.
const CTRL_EXPORT_OBJECT_TO_FD: u32 = 0x3d05;
const EXPORT_TYPE_RM: u32 = 1;
const EXPORT_H_OBJECT: usize = 12;
const EXPORT_FD: usize = 16;
/// Entries kept at once in each map below.
const MAX_PLACEMENTS: usize = 65536;

/// How each RM memory object the guest allocated is placed, followed from
/// the allocation to the export descriptor to the GEM object NVK imports
/// (`RmLayouts::placement`), because nvidia-drm cannot say (a dma-buf of
/// RM memory carries neither location nor caching) and RM answers neither
/// through any control a user client may make on the object: its
/// `GET_SURFACE_INFO` `PHYS_ATTR` is 0 for these, and `RM_MAP_MEMORY` echoes
/// the default caching type it was asked for.
#[derive(Default)]
pub(super) struct RmPlacements {
    /// (client, object) to placement, from successful `RM_ALLOC`s.
    objects: HashMap<(u32, u32), RmPlacement>,
    /// Export descriptor (the guest's handle for it) to the placement of
    /// the object exported into it.
    exports: HashMap<u32, RmPlacement>,
}

impl RmPlacements {
    fn put<K: std::hash::Hash + Eq>(m: &mut HashMap<K, RmPlacement>, k: K, v: RmPlacement) {
        if m.len() >= MAX_PLACEMENTS && !m.contains_key(&k) {
            return;
        }
        m.insert(k, v);
    }

    /// The guest's handle `handle` closed: an export descriptor it was goes.
    pub(super) fn handle_closed(&mut self, handle: u32) {
        self.exports.remove(&handle);
    }

    #[cfg(test)]
    pub(super) fn objects(&self) -> usize {
        self.objects.len()
    }
}

/// What the backend remembers of each GEM object NVK imported: its modifier,
/// or `None` when it has none a 2D image can name. Keyed by (DRM file
/// handle, host GEM handle). Beside it, where the memory behind the object
/// lives ([`RmPlacement`]), when the backend followed it from the
/// allocation.
#[derive(Default)]
pub(super) struct RmLayouts(
    HashMap<(u32, u32), Option<u64>>,
    HashMap<(u32, u32), RmPlacement>,
);

impl RmLayouts {
    pub(super) fn set_placement(&mut self, owner: u32, gem: u32, p: RmPlacement) {
        RmPlacements::put(&mut self.1, (owner, gem), p);
    }

    /// Where the memory behind GEM handle `gem` of file `owner` lives, when
    /// known.
    #[cfg_attr(not(feature = "venus"), allow(dead_code))]
    pub(super) fn placement(&self, owner: u32, gem: u32) -> Option<RmPlacement> {
        self.1.get(&(owner, gem)).copied()
    }

    pub(super) fn insert(&mut self, owner: u32, gem: u32, modifier: Option<u64>) {
        if self.0.len() >= MAX_LAYOUTS && !self.0.contains_key(&(owner, gem)) {
            log::warn!(
                "rm import: {MAX_LAYOUTS} layouts kept already; handle {gem} on file {owner} \
                 gets none"
            );
            return;
        }
        self.0.insert((owner, gem), modifier);
    }

    /// `Some(m)`: the object was imported with modifier `m` (`None` inside
    /// for a layout with no modifier). `None`: no import seen.
    #[cfg_attr(not(feature = "venus"), allow(dead_code))]
    pub(super) fn get(&self, owner: u32, gem: u32) -> Option<Option<u64>> {
        self.0.get(&(owner, gem)).copied()
    }

    pub(super) fn forget(&mut self, owner: u32, gem: u32) {
        self.0.remove(&(owner, gem));
        self.1.remove(&(owner, gem));
    }

    pub(super) fn forget_owner(&mut self, owner: u32) {
        self.0.retain(|(o, _), _| *o != owner);
        self.1.retain(|(o, _), _| *o != owner);
    }

    /// The guest's generation ended: every file it named is gone.
    pub(super) fn clear(&mut self) {
        self.0.clear();
        self.1.clear();
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty() && self.1.is_empty()
    }
}

impl NvidiaBackend {
    /// After the host answered an RM call: follow where memory objects live
    /// (`RmPlacements`). An allocation of a memory class records the `attr`
    /// RM wrote back; a free forgets it (all of a client's, when the client
    /// goes); a duplicate copies it; an export to a descriptor carries it to
    /// that descriptor, from which [`Self::note_gem_import`] takes it.
    pub(super) fn note_rm_placement(&mut self, payload: &[u8], resp: &[u8]) {
        const ESC_RM_FREE: u32 = 0x29;
        const ESC_RM_CONTROL: u32 = 0x2a;
        const ESC_RM_ALLOC: u32 = 0x2b;
        const ESC_RM_DUP_OBJECT: u32 = 0x34;
        if payload.len() < size_of::<IoctlReq>() {
            return;
        }
        let req = read_struct::<IoctlReq>(payload, 0);
        if (req.cmd >> 8) & 0xFF != b'F' as u32 {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let r = read_struct::<IoctlResp>(resp, size_of::<MsgHeader>());
        let (data_len, nested_len) = (r.data_len as usize, r.nested_len as usize);
        let Some(out) = resp.get(head..head + data_len + nested_len) else {
            return;
        };
        let (top, nested) = out.split_at(data_len);
        let w = |b: &[u8], at: usize| {
            b.get(at..at + 4)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        };
        let p = &mut self.rm_placements;
        match req.cmd & 0xFF {
            // NVOS64: hRoot, hObjectParent, hObjectNew, hClass, pAllocParms,
            // pRightsRequested, paramsSize, flags, status at 40.
            ESC_RM_ALLOC => {
                let (Some(client), Some(object), Some(class), Some(status)) =
                    (w(top, 0), w(top, 8), w(top, 12), w(top, 40))
                else {
                    return;
                };
                if status != 0 || !PLACED_CLASSES.contains(&class) {
                    return;
                }
                if let Some(attr) = w(nested, ALLOC_ATTR) {
                    RmPlacements::put(&mut p.objects, (client, object), RmPlacement { attr });
                }
            }
            // NVOS00: hRoot, hObjectParent, hObjectOld, status.
            ESC_RM_FREE => {
                let (Some(client), Some(object), Some(0)) = (w(top, 0), w(top, 8), w(top, 12))
                else {
                    return;
                };
                if client == object {
                    p.objects.retain(|(c, _), _| *c != client);
                } else {
                    p.objects.remove(&(client, object));
                }
            }
            // NVOS55: hClient, hParent, hObject, hClientSrc, hObjectSrc,
            // flags, status.
            ESC_RM_DUP_OBJECT => {
                let (Some(client), Some(object), Some(sc), Some(so), Some(0)) =
                    (w(top, 0), w(top, 8), w(top, 12), w(top, 16), w(top, 24))
                else {
                    return;
                };
                if let Some(pl) = p.objects.get(&(sc, so)).copied() {
                    RmPlacements::put(&mut p.objects, (client, object), pl);
                }
            }
            // NVOS54: hClient, hObject, cmd, flags, params, paramsSize,
            // status at 28.
            ESC_RM_CONTROL => {
                let (Some(client), Some(CTRL_EXPORT_OBJECT_TO_FD), Some(0)) =
                    (w(top, 0), w(top, 8), w(top, 28))
                else {
                    return;
                };
                let (Some(EXPORT_TYPE_RM), Some(object), Some(fd)) = (
                    w(nested, 0),
                    w(nested, EXPORT_H_OBJECT),
                    w(nested, EXPORT_FD),
                ) else {
                    return;
                };
                match p.objects.get(&(client, object)).copied() {
                    Some(pl) => RmPlacements::put(&mut p.exports, fd, pl),
                    None => {
                        p.exports.remove(&fd);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `DRM_IOCTL_NVIDIA_GET_DEV_INFO` on `fd`, through the host driver, in the
/// host release's layout.
fn tiling_of(host: &dyn HostDriver, fd: RawFd, layout: &abi::devinfo::Layout) -> Option<Tiling> {
    let mut p = vec![0u8; layout.size];
    if let Err(e) = host.ioctl(fd, u64::from(layout.ioctl()), &mut p) {
        log::warn!("rm import: GET_DEV_INFO on the render node failed: errno {e}");
        return None;
    }
    let d = layout.decode(&p)?;
    Some(Tiling {
        kind: d.generic_page_kind,
        generation: d.page_kind_generation,
        sector_layout: d.sector_layout,
    })
}

impl NvidiaBackend {
    /// After a forwarded ioctl: a `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` that
    /// succeeded on a render node records the new object's modifier, read
    /// from the NVKMS block the guest sent and the node's tiling.
    pub(super) fn note_gem_import(&mut self, payload: &[u8], resp: &[u8]) {
        let owner = self.current_handle;
        if !matches!(
            self.handle_kinds.get(&(owner as u64)),
            Some(DeviceKind::Dri(_))
        ) {
            return;
        }
        if payload.len() < size_of::<IoctlReq>() {
            return;
        }
        let req = read_struct::<IoctlReq>(payload, 0);
        let request = req.cmd as u64;
        if (request >> 8) & 0xFF != b'd' as u64 || request & 0xFF != GEM_IMPORT_NVKMS_MEMORY {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head + GEM_IMPORT_OUTER || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let out = &resp[head..];
        let gem = u32::from_le_bytes(
            out[GEM_IMPORT_HANDLE..GEM_IMPORT_HANDLE + 4]
                .try_into()
                .unwrap(),
        );
        if gem == 0 {
            return;
        }
        // The NVKMS block is the nested part of the request, after the outer
        // struct; what the host wrote back into it is not needed.
        let body = &payload[size_of::<IoctlReq>()..];
        let start = req.data_len as usize;
        let end = start + req.nested_len as usize;
        // `memFd` (the guest's handle for the export descriptor) opens the
        // NVKMS block: where the memory lives comes with it, when the
        // backend saw the allocation and the export.
        if let Some(pl) = body
            .get(start..start + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .and_then(|fd| self.rm_placements.exports.get(&fd).copied())
        {
            self.rm_layouts.set_placement(owner, gem, pl);
        }
        let layout = body
            .get(start..end)
            .and_then(parse_nvkms_import)
            .unwrap_or(SurfaceLayout::Other);
        let modifier = match layout {
            SurfaceLayout::Pitch => Some(MOD_LINEAR),
            SurfaceLayout::Other => None,
            SurfaceLayout::BlockLinear { .. } => {
                let Ok(fd) = self.handles.get_raw(owner as u64) else {
                    return;
                };
                let Some(devinfo) = self.devinfo else {
                    return;
                };
                tiling_of(&*self.host, fd, &devinfo.layout).and_then(|t| modifier_for(layout, t))
            }
        };
        log::debug!(
            "rm import: GEM handle {gem} on file {owner} is {layout:?}, modifier {}",
            modifier.map_or("none".into(), |m| format!("{m:#018x}"))
        );
        self.rm_layouts.insert(owner, gem, modifier);
    }

    /// The view Venus exports RM objects through, for tests. `GpuCmd` builds
    /// it field by field instead, so Venus can be borrowed mutably beside it.
    #[cfg(test)]
    pub(super) fn rm_view(&self) -> RmView<'_> {
        RmView {
            handles: &self.handles,
            kinds: &self.handle_kinds,
            host: &*self.host,
            layouts: &self.rm_layouts,
        }
    }

    /// Layouts currently remembered, for tests and the teardown report.
    pub fn rm_layouts_kept(&self) -> usize {
        self.rm_layouts.len()
    }
}

/// `(rm_handle, gem_handle)` to a dma-buf, with the same checks as a
/// scanout flip: the handle must be a render node this guest opened, and
/// the GEM handle one that file has (PRIME says so).
#[cfg_attr(not(feature = "venus"), allow(dead_code))]
pub(super) struct RmView<'a> {
    pub(super) handles: &'a HandleTable,
    pub(super) kinds: &'a std::collections::HashMap<u64, DeviceKind>,
    pub(super) host: &'a dyn HostDriver,
    pub(super) layouts: &'a RmLayouts,
}

/// An RM-exported object, exported again as a dma-buf for Venus.
#[derive(Debug)]
pub struct RmObject {
    /// A fresh dma-buf descriptor for the object: the caller's own reference.
    pub dmabuf: OwnedFd,
    /// Its modifier, when the backend saw the GEM import (`None`: not seen,
    /// or a layout with no 2D modifier).
    pub modifier: Option<u64>,
    /// Where its memory lives, when the backend followed it from the RM
    /// allocation (`None`: not seen).
    pub placement: Option<RmPlacement>,
}

#[cfg_attr(not(feature = "venus"), allow(dead_code))]
impl RmView<'_> {
    /// `Err` is an errno: `EBADF` when `rm_handle` is not a render node this
    /// guest opened, otherwise what `PRIME_HANDLE_TO_FD` failed with
    /// (`ENOENT` for a GEM handle the file does not have).
    pub fn export(&self, rm_handle: u32, gem_handle: u32) -> std::result::Result<RmObject, i32> {
        if gem_handle == 0
            || !matches!(
                self.kinds.get(&(rm_handle as u64)),
                Some(DeviceKind::Dri(_))
            )
        {
            return Err(libc::EBADF);
        }
        let fd = self
            .handles
            .get_raw(rm_handle as u64)
            .map_err(|_| libc::EBADF)?;
        let dmabuf = prime_export(fd, gem_handle, |fd, req, arg| self.host.ioctl(fd, req, arg))?;
        Ok(RmObject {
            dmabuf,
            modifier: self.layouts.get(rm_handle, gem_handle).flatten(),
            placement: self.layouts.placement(rm_handle, gem_handle),
        })
    }
}

#[cfg(feature = "venus")]
impl crate::venus::RmExports for RmView<'_> {
    fn export(&self, rm_handle: u32, gem_handle: u32) -> std::result::Result<RmObject, i32> {
        RmView::export(self, rm_handle, gem_handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, Mutex};

    const GB202: Tiling = Tiling {
        kind: 0x06,
        generation: 2,
        sector_layout: 1,
    };

    fn nvkms_block(layout: u32, x: u32, y: u32, z: u32) -> Vec<u8> {
        let mut b = vec![0u8; NVKMS_IMPORT_LEN];
        b[0..4].copy_from_slice(&7i32.to_le_bytes()); // memFd (a handle)
        b[4..8].copy_from_slice(&layout.to_le_bytes());
        b[8..12].copy_from_slice(&x.to_le_bytes());
        b[12..16].copy_from_slice(&y.to_le_bytes());
        b[16..20].copy_from_slice(&z.to_le_bytes());
        b[24] = 1; // genericMemory
        b
    }

    #[test]
    fn modifiers_are_nvidias_block_linear_2d() {
        // What the spike measured NVIDIA advertising on GB202, h = 0..5.
        for h in 0..=5 {
            assert_eq!(
                modifier_for(SurfaceLayout::BlockLinear { log2_gobs_y: h }, GB202),
                Some(0x0300_0000_0060_6010 | u64::from(h))
            );
        }
        assert_eq!(
            modifier_for(SurfaceLayout::BlockLinear { log2_gobs_y: 5 }, GB202),
            Some(0x0300_0000_0060_6015),
            "NVK's 1080p swapchain image"
        );
        assert_eq!(modifier_for(SurfaceLayout::Pitch, GB202), Some(MOD_LINEAR));
        assert_eq!(modifier_for(SurfaceLayout::Other, GB202), None);
        // Turing-era generation 0 / sector layout 1 / kind 0xfe, from the
        // same formula: drm_fourcc.h's own example.
        let turing = Tiling {
            kind: 0xfe,
            generation: 0,
            sector_layout: 1,
        };
        assert_eq!(
            modifier_for(SurfaceLayout::BlockLinear { log2_gobs_y: 4 }, turing),
            Some(0x0300_0000_004f_e014)
        );
    }

    #[test]
    fn the_nvkms_block_is_read_as_nvk_writes_it() {
        assert_eq!(
            parse_nvkms_import(&nvkms_block(NVKMS_LAYOUT_PITCH, 0, 0, 0)),
            Some(SurfaceLayout::Pitch)
        );
        assert_eq!(
            parse_nvkms_import(&nvkms_block(NVKMS_LAYOUT_BLOCK_LINEAR, 0, 5, 0)),
            Some(SurfaceLayout::BlockLinear { log2_gobs_y: 5 })
        );
        // 3D blocks, a wide block, a block height no modifier has, a layout
        // NVKMS does not define: no 2D modifier.
        for b in [
            nvkms_block(NVKMS_LAYOUT_BLOCK_LINEAR, 0, 4, 1),
            nvkms_block(NVKMS_LAYOUT_BLOCK_LINEAR, 1, 4, 0),
            nvkms_block(NVKMS_LAYOUT_BLOCK_LINEAR, 0, 6, 0),
            nvkms_block(2, 0, 0, 0),
        ] {
            assert_eq!(parse_nvkms_import(&b), Some(SurfaceLayout::Other));
        }
        assert_eq!(parse_nvkms_import(&[0u8; 27]), None, "short");
    }

    /// The host nvidia-drm, as far as these paths ask it: GEM imports make
    /// handle 77, GET_DEV_INFO answers GB202, PRIME exports a memfd of
    /// `object_size` bytes. Every request is recorded.
    #[derive(Clone)]
    struct DrmHost {
        object_size: u64,
        calls: Arc<Mutex<Vec<u64>>>,
        /// What the node answers `GET_DEV_INFO` with, in the host release's
        /// own words. The ioctl number it is asked with has to carry their
        /// size, as the real driver's does.
        dev_info: Vec<u32>,
    }

    /// What GB202 answers on 575 and later, nine words.
    const GB202_575: [u32; 9] = [0x100, 0, 0, 1, 6, 2, 1, 1, 1];
    /// What an RTX 4070 SUPER answers on 565.77: eight words, no `mig_device`.
    const AD104_565: [u32; 8] = [0x200, 1, 1, 6, 2, 1, 1, 1];

    /// `_IOWR('d', 0x43, <size>)`, written out so a change to
    /// [`abi::devinfo::Layout::ioctl`] cannot hide behind itself.
    fn get_dev_info_cmd(size: u32) -> u64 {
        0xC000_6443 | u64::from(size) << 16
    }

    pub(crate) const GEM_HANDLE: u32 = 77;
    /// What PRIME_FD_TO_HANDLE answers on any file (RmResourceImport).
    pub(crate) const IMPORTED_HANDLE: u32 = 88;

    impl HostDriver for DrmHost {
        fn ioctl(&self, _fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
            self.calls.lock().unwrap().push(request);
            if (request >> 8) & 0xff == b'd' as u64 && request & 0xff == GEM_IMPORT_NVKMS_MEMORY {
                arg[GEM_IMPORT_HANDLE..GEM_IMPORT_HANDLE + 4]
                    .copy_from_slice(&GEM_HANDLE.to_le_bytes());
                return Ok(());
            }
            if request & 0xffff == 0x6443 {
                // The real driver copies in the number's size, fills its own
                // struct and copies that out: a mismatch is not an error
                // there, it is a short or a long read. Here it is one.
                if request != get_dev_info_cmd(self.dev_info.len() as u32 * 4)
                    || arg.len() != self.dev_info.len() * 4
                {
                    return Err(libc::EINVAL);
                }
                for (i, w) in self.dev_info.iter().enumerate() {
                    arg[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
                }
                return Ok(());
            }
            if request == super::super::rm_resource::DRM_IOCTL_PRIME_FD_TO_HANDLE {
                let fd = i32::from_le_bytes(arg[8..12].try_into().unwrap());
                if fd < 0 {
                    return Err(libc::EBADF);
                }
                arg[0..4].copy_from_slice(&IMPORTED_HANDLE.to_le_bytes());
                return Ok(());
            }
            if request == crate::display::DRM_IOCTL_PRIME_HANDLE_TO_FD {
                let handle = u32::from_le_bytes(arg[0..4].try_into().unwrap());
                if handle != GEM_HANDLE {
                    return Err(libc::ENOENT);
                }
                let fd = conduit_venus::mock::memfd(self.object_size).map_err(|_| libc::EIO)?;
                use std::os::fd::IntoRawFd;
                arg[8..12].copy_from_slice(&fd.into_raw_fd().to_le_bytes());
                return Ok(());
            }
            Ok(())
        }
    }

    /// A backend on [`DrmHost`] with a render node (and a control file whose
    /// handle stands in for NVK's export descriptor) open.
    pub(crate) fn drm_backend(object_size: u64) -> (NvidiaBackend, u64, u64, Arc<Mutex<Vec<u64>>>) {
        drm_backend_on(
            abi::version::DriverVersion::new(615, 71, 9),
            &GB202_575,
            object_size,
        )
    }

    /// [`drm_backend`] on a host release whose nvidia-drm answers `dev_info`.
    fn drm_backend_on(
        release: abi::version::DriverVersion,
        dev_info: &[u32],
        object_size: u64,
    ) -> (NvidiaBackend, u64, u64, Arc<Mutex<Vec<u64>>>) {
        let mut be = NvidiaBackend::for_test();
        be.devinfo = abi::devinfo::select(release);
        let calls = Arc::new(Mutex::new(Vec::new()));
        be.set_host(Box::new(DrmHost {
            object_size,
            calls: calls.clone(),
            dev_info: dev_info.to_vec(),
        }));
        let open = |be: &mut NvidiaBackend, kind| {
            let null = std::fs::File::open("/dev/null").expect("/dev/null");
            let h = be.handles.insert(OwnedFd::from(null));
            be.handle_kinds.insert(h, kind);
            h
        };
        let dri = open(&mut be, DeviceKind::Dri(0));
        let ctl = open(&mut be, DeviceKind::Ctl);
        (be, dri, ctl, calls)
    }

    /// The guest's forwarded GEM import: outer struct, then the NVKMS block
    /// with `mem_fd` (one of our handles) in its first word.
    pub(crate) fn gem_import_msg(file: u64, mem_fd: u64, block: &[u8]) -> Vec<u8> {
        let mut outer = vec![0u8; GEM_IMPORT_OUTER];
        outer[0..8].copy_from_slice(&(1u64 << 20).to_le_bytes());
        outer[8..16].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
        outer[16..24].copy_from_slice(&(block.len() as u64).to_le_bytes());
        let mut block = block.to_vec();
        block[0..4].copy_from_slice(&(mem_fd as i32).to_le_bytes());
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: MsgType::Ioctl as u32,
                handle: file as u32,
                status: 0,
                padding: 0,
            },
        );
        let at = v.len();
        v.resize(at + size_of::<IoctlReq>(), 0);
        write_struct(
            &mut v[at..],
            &IoctlReq {
                // _IOWR('d', 0x41, 32)
                cmd: 0xC020_6441,
                data_len: GEM_IMPORT_OUTER as u32,
                nested_offset: 0,
                nested_len: block.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        v.extend_from_slice(&outer);
        v.extend_from_slice(&block);
        v
    }

    pub(crate) fn import_layout(be: &mut NvidiaBackend, dri: u64, ctl: u64, layout: u32, h: u32) {
        let mut resp = vec![0u8; 512];
        be.dispatch(
            &gem_import_msg(dri, ctl, &nvkms_block(layout, 0, h, 0)),
            &mut resp,
        );
        assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, 0);
    }

    #[test]
    fn a_forwarded_gem_import_records_the_modifier() {
        let (mut be, dri, ctl, calls) = drm_backend(1 << 20);
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_BLOCK_LINEAR, 5);
        assert_eq!(
            be.rm_layouts.get(dri as u32, GEM_HANDLE),
            Some(Some(0x0300_0000_0060_6015))
        );
        assert!(
            calls.lock().unwrap().contains(&get_dev_info_cmd(36)),
            "the node's own tiling was asked, in the size its release has"
        );
        let o = be.rm_view().export(dri as u32, GEM_HANDLE).unwrap();
        assert_eq!(o.modifier, Some(0x0300_0000_0060_6015));

        // Re-imported as pitch: the newer layout wins.
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_PITCH, 0);
        assert_eq!(
            be.rm_layouts.get(dri as u32, GEM_HANDLE),
            Some(Some(MOD_LINEAR))
        );
    }

    /// On 565.77 the struct is eight words. Read as nine, the page kind comes
    /// out as 2 and the generation as 1 and the modifier names a layout the
    /// card does not use; asked with a 36-byte number the real driver reads
    /// 36 bytes of the caller's 32 as well.
    #[test]
    fn a_565_host_is_asked_and_read_in_its_own_layout() {
        let v565 = abi::version::DriverVersion::new(565, 77, 0);
        let (mut be, dri, ctl, calls) = drm_backend_on(v565, &AD104_565, 1 << 20);
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_BLOCK_LINEAR, 5);
        assert_eq!(
            be.rm_layouts.get(dri as u32, GEM_HANDLE),
            Some(Some(0x0300_0000_0060_6015)),
            "kind 6, generation 2, sector layout 1"
        );
        assert!(calls.lock().unwrap().contains(&get_dev_info_cmd(32)));
        assert!(!calls.lock().unwrap().contains(&get_dev_info_cmd(36)));
    }

    /// An answered RM call as `note_rm_placement` sees it: the request
    /// (command, top-level struct, nested block) and the response carrying
    /// what the host wrote back into both.
    fn rm_answer(be: &mut NvidiaBackend, esc: u32, top: &[u8], nested: &[u8]) {
        let mut payload = vec![0u8; size_of::<IoctlReq>()];
        write_struct(
            &mut payload,
            &IoctlReq {
                cmd: 0xC000_4600 | esc,
                data_len: top.len() as u32,
                nested_offset: top.len() as u32,
                nested_len: nested.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        payload.extend_from_slice(top);
        payload.extend_from_slice(nested);
        let mut resp = vec![0u8; size_of::<MsgHeader>() + size_of::<IoctlResp>()];
        write_struct(&mut resp, &MsgHeader::ok(MsgType::Ioctl, 0));
        write_struct(
            &mut resp[size_of::<MsgHeader>()..],
            &IoctlResp {
                data_len: top.len() as u32,
                nested_len: nested.len() as u32,
                deep_len: 0,
            },
        );
        resp.extend_from_slice(top);
        resp.extend_from_slice(nested);
        be.note_rm_placement(&payload, &resp);
    }

    fn words(n: usize, set: &[(usize, u32)]) -> Vec<u8> {
        let mut v = vec![0u8; n];
        for &(at, x) in set {
            v[at..at + 4].copy_from_slice(&x.to_le_bytes());
        }
        v
    }

    const CLIENT: u32 = 0xc1d0_0042;
    const MEM: u32 = 0x5c00_0005;

    /// RM_ALLOC of `class` answered with `attr`.
    fn alloc(be: &mut NvidiaBackend, class: u32, object: u32, attr: u32) {
        rm_answer(
            be,
            0x2b,
            &words(
                48,
                &[(0, CLIENT), (4, 0x5c00_0001), (8, object), (12, class)],
            ),
            &words(128, &[(ALLOC_ATTR, attr)]),
        );
    }

    /// OS_UNIX_EXPORT_OBJECT_TO_FD of `object` into descriptor `fd`.
    fn export(be: &mut NvidiaBackend, object: u32, fd: u32) {
        rm_answer(
            be,
            0x2a,
            &words(
                32,
                &[(0, CLIENT), (4, CLIENT), (8, CTRL_EXPORT_OBJECT_TO_FD)],
            ),
            &words(
                24,
                &[
                    (0, EXPORT_TYPE_RM),
                    (4, 0x5c00_0001),
                    (8, 0x5c00_0001),
                    (EXPORT_H_OBJECT, object),
                    (EXPORT_FD, fd),
                ],
            ),
        );
    }

    /// Where the memory behind a GEM object lives is followed from RM's
    /// answer to the allocation, through the export descriptor, to the GEM
    /// import that names it; a free or a closed descriptor ends the trail.
    #[test]
    fn the_placement_follows_the_allocation_to_the_gem_object() {
        // What RM wrote back on the host (rm_sysmem_flip): cached PCI
        // sysmem, write-combined PCI sysmem, video memory.
        const SYS_CACHED: u32 = 0x2a80_0000;
        const SYS_WC: u32 = 0x4a80_0000;
        const VIDMEM: u32 = 0x1100_0000;
        let p = |attr| RmPlacement { attr };
        assert_eq!(
            p(SYS_CACHED).map_cache(),
            Some(protocol::venus::MAP_CACHE_CACHED)
        );
        assert_eq!(p(SYS_WC).map_cache(), Some(protocol::venus::MAP_CACHE_WC));
        assert_eq!(p(VIDMEM).map_cache(), None);
        assert_eq!(
            p(1 << 25).map_cache(),
            Some(protocol::venus::MAP_CACHE_UNCACHED)
        );

        let (mut be, dri, ctl, _) = drm_backend(1 << 20);
        alloc(&mut be, 0x3e, MEM, SYS_CACHED);
        alloc(&mut be, 0x0071, MEM + 1, SYS_CACHED); // not a placed class
        assert_eq!(be.rm_placements.objects(), 1);
        export(&mut be, MEM, ctl as u32);
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_PITCH, 0);
        assert_eq!(
            be.rm_layouts.placement(dri as u32, GEM_HANDLE),
            Some(p(SYS_CACHED))
        );
        let o = be.rm_view().export(dri as u32, GEM_HANDLE).unwrap();
        assert_eq!(o.placement, Some(p(SYS_CACHED)));

        // Video memory exported into the same descriptor replaces it.
        alloc(&mut be, 0x40, MEM + 2, VIDMEM);
        export(&mut be, MEM + 2, ctl as u32);
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_PITCH, 0);
        assert_eq!(
            be.rm_layouts.placement(dri as u32, GEM_HANDLE),
            Some(p(VIDMEM))
        );

        // A freed object exports nothing known; the GEM close forgets.
        rm_answer(&mut be, 0x29, &words(16, &[(0, CLIENT), (8, MEM)]), &[]);
        export(&mut be, MEM, ctl as u32);
        be.rm_layouts.forget(dri as u32, GEM_HANDLE);
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_PITCH, 0);
        assert_eq!(be.rm_layouts.placement(dri as u32, GEM_HANDLE), None);

        // A client's free takes all of its objects; a dup copies one.
        alloc(&mut be, 0x3e, MEM, SYS_WC);
        rm_answer(
            &mut be,
            0x34,
            &words(28, &[(0, CLIENT), (8, MEM + 9), (12, CLIENT), (16, MEM)]),
            &[],
        );
        assert_eq!(be.rm_placements.objects(), 3);
        rm_answer(&mut be, 0x29, &words(16, &[(0, CLIENT), (8, CLIENT)]), &[]);
        assert_eq!(be.rm_placements.objects(), 0);

        // A failed allocation records nothing.
        rm_answer(
            &mut be,
            0x2b,
            &words(48, &[(0, CLIENT), (8, MEM), (12, 0x3e), (40, 0x51)]),
            &words(128, &[(ALLOC_ATTR, SYS_CACHED)]),
        );
        assert_eq!(be.rm_placements.objects(), 0);
    }

    #[test]
    fn a_gem_import_on_another_kind_of_file_is_not_recorded() {
        let (mut be, _dri, ctl, _) = drm_backend(1 << 20);
        let mut resp = vec![0u8; 512];
        be.dispatch(
            &gem_import_msg(ctl, ctl, &nvkms_block(NVKMS_LAYOUT_PITCH, 0, 0, 0)),
            &mut resp,
        );
        assert_eq!(be.rm_layouts_kept(), 0);
    }

    #[test]
    fn gem_close_and_file_close_forget_the_layout() {
        let (mut be, dri, ctl, _) = drm_backend(1 << 20);
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_PITCH, 0);
        assert_eq!(be.rm_layouts_kept(), 1);
        // DRM_IOCTL_GEM_CLOSE {handle, pad}
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: MsgType::Ioctl as u32,
                handle: dri as u32,
                status: 0,
                padding: 0,
            },
        );
        let at = v.len();
        v.resize(at + size_of::<IoctlReq>(), 0);
        write_struct(
            &mut v[at..],
            &IoctlReq {
                cmd: crate::display::DRM_IOCTL_GEM_CLOSE as u32,
                data_len: 8,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        v.extend_from_slice(&GEM_HANDLE.to_le_bytes());
        v.extend_from_slice(&[0u8; 4]);
        let mut resp = vec![0u8; 256];
        be.dispatch(&v, &mut resp);
        assert_eq!(be.rm_layouts_kept(), 0);

        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_PITCH, 0);
        assert_eq!(be.rm_layouts_kept(), 1);
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: MsgType::Close as u32,
                handle: dri as u32,
                status: 0,
                padding: 0,
            },
        );
        be.dispatch(&v, &mut resp);
        assert_eq!(be.rm_layouts_kept(), 0);
    }

    /// The whole path through the backend: NVK's GEM import on the render
    /// node, then the KMD's RESOURCE_CREATE_BLOB naming it, then the file
    /// closed while the resource lives on.
    #[cfg(feature = "venus")]
    #[test]
    fn the_kmds_rm_blob_reaches_the_renderer_as_the_object() {
        use protocol::venus::*;
        let (mut be, dri, ctl, _) = drm_backend(8 << 20);
        be.set_venus(crate::venus::Venus::new(
            Box::new(conduit_venus::mock::Mock::new()),
            1 << 20,
            None,
        ));
        assert!(be.venus_rm_import());
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_BLOCK_LINEAR, 5);

        let gpu_cmd = |body: &[u8]| {
            let mut v = vec![0u8; size_of::<MsgHeader>()];
            write_struct(&mut v, &MsgHeader::ok(MsgType::GpuCmd, 0));
            v.extend_from_slice(body);
            v
        };
        let mut resp = [0u8; 1024];
        let mut send = |be: &mut NvidiaBackend, body: &[u8]| {
            be.dispatch(&gpu_cmd(body), &mut resp);
            let h = CtrlHdr::from_bytes(&resp[16..]).unwrap();
            (h.ty, h.padding)
        };
        let ctx = CtxCreate {
            hdr: CtrlHdr {
                ty: CMD_CTX_CREATE,
                ctx_id: 1,
                ..Default::default()
            },
            context_init: CAPSET_VENUS,
            ..Default::default()
        };
        assert_eq!(send(&mut be, &ctx.to_bytes()).0, RESP_OK_NODATA);
        let blob = |id: u32, file: u64, size: u64| ResourceCreateBlob {
            hdr: CtrlHdr {
                ty: CMD_RESOURCE_CREATE_BLOB,
                ctx_id: 1,
                ..Default::default()
            },
            resource_id: id,
            blob_mem: BLOB_MEM_RM_EXPORT,
            blob_flags: 0,
            nr_entries: 0,
            blob_id: (file << 32) | u64::from(GEM_HANDLE),
            size,
        };
        // The control file is no render node; the size is the object's.
        assert_eq!(
            send(&mut be, &blob(5, ctl, 4 << 20).to_bytes()),
            (RESP_ERR_INVALID_PARAMETER, errno_padding(libc::EBADF))
        );
        assert_eq!(
            send(&mut be, &blob(5, dri, (8 << 20) + 4096).to_bytes()),
            (RESP_ERR_INVALID_PARAMETER, errno_padding(libc::ERANGE))
        );
        assert_eq!(
            send(&mut be, &blob(5, dri, 8 << 20).to_bytes()).0,
            RESP_OK_NODATA
        );
        assert_eq!(be.venus().unwrap().resources(), 1);

        // The guest closes the render node: the layout record goes, the
        // resource stays.
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: MsgType::Close as u32,
                handle: dri as u32,
                status: 0,
                padding: 0,
            },
        );
        let mut r = vec![0u8; 256];
        be.dispatch(&v, &mut r);
        assert_eq!(be.rm_layouts_kept(), 0);
        assert_eq!(be.venus().unwrap().resources(), 1);
        // And a new import names a file that is gone.
        assert_eq!(
            send(&mut be, &blob(6, dri, 4096).to_bytes()),
            (RESP_ERR_INVALID_PARAMETER, errno_padding(libc::EBADF))
        );
        assert_eq!(
            send(
                &mut be,
                &ResourceCmd {
                    hdr: CtrlHdr {
                        ty: CMD_RESOURCE_UNREF,
                        ..Default::default()
                    },
                    resource_id: 5,
                    padding: 0,
                }
                .to_bytes()
            )
            .0,
            RESP_OK_NODATA
        );
        assert_eq!(be.venus().unwrap().resources(), 0);
    }

    /// RmResourceImport: a second render node of the guest gets a GEM handle
    /// for an RM-export resource, with its layout; what is not a render node,
    /// not a resource, or not an RM-export one is refused.
    #[cfg(feature = "venus")]
    #[test]
    fn an_rm_resource_becomes_a_gem_handle_on_another_render_node() {
        use protocol::messages::{
            RM_RESOURCE_IMPORT_MODIFIER, RmResourceImport, RmResourceImportReply,
        };
        use protocol::venus::*;
        let (mut be, dri, ctl, calls) = drm_backend(8 << 20);
        let ask = |be: &mut NvidiaBackend, owner: u64, res: u32, flags: u32| {
            let mut v = vec![0u8; size_of::<MsgHeader>()];
            write_struct(&mut v, &MsgHeader::ok(MsgType::RmResourceImport, 0));
            v.extend_from_slice(
                &RmResourceImport {
                    owner_handle: owner as u32,
                    resource_id: res,
                    flags,
                    reserved: 0,
                }
                .to_bytes(),
            );
            let mut resp = vec![0u8; 256];
            let n = be.dispatch(&v, &mut resp);
            let h = read_struct::<MsgHeader>(&resp, 0);
            assert_eq!(h.msg_type, MsgType::RmResourceImport as u32);
            (h.status, RmResourceImportReply::from_bytes(&resp[16..n]))
        };
        // No Venus, no RM-export resources.
        assert_eq!(ask(&mut be, dri, 5, 0).0, -libc::EOPNOTSUPP);

        be.set_venus(crate::venus::Venus::new(
            Box::new(conduit_venus::mock::Mock::new()),
            1 << 20,
            None,
        ));
        import_layout(&mut be, dri, ctl, NVKMS_LAYOUT_BLOCK_LINEAR, 5);
        let gpu_cmd = |body: &[u8]| {
            let mut v = vec![0u8; size_of::<MsgHeader>()];
            write_struct(&mut v, &MsgHeader::ok(MsgType::GpuCmd, 0));
            v.extend_from_slice(body);
            v
        };
        let send = |be: &mut NvidiaBackend, body: &[u8]| {
            let mut resp = [0u8; 1024];
            be.dispatch(&gpu_cmd(body), &mut resp);
            CtrlHdr::from_bytes(&resp[16..]).unwrap().ty
        };
        let ctx = CtxCreate {
            hdr: CtrlHdr {
                ty: CMD_CTX_CREATE,
                ctx_id: 1,
                ..Default::default()
            },
            context_init: CAPSET_VENUS,
            ..Default::default()
        };
        assert_eq!(send(&mut be, &ctx.to_bytes()), RESP_OK_NODATA);
        let blob = ResourceCreateBlob {
            hdr: CtrlHdr {
                ty: CMD_RESOURCE_CREATE_BLOB,
                ctx_id: 1,
                ..Default::default()
            },
            resource_id: 5,
            blob_mem: BLOB_MEM_RM_EXPORT,
            blob_flags: 0,
            nr_entries: 0,
            blob_id: (dri << 32) | u64::from(GEM_HANDLE),
            size: 8 << 20,
        };
        assert_eq!(send(&mut be, &blob.to_bytes()), RESP_OK_NODATA);
        // An ordinary (host Vulkan) blob beside it.
        let host = ResourceCreateBlob {
            resource_id: 6,
            blob_mem: BLOB_MEM_HOST3D,
            blob_id: 1,
            size: 4096,
            ..blob
        };
        assert_eq!(send(&mut be, &host.to_bytes()), RESP_OK_NODATA);

        // The second process's render node.
        let null = std::fs::File::open("/dev/null").expect("/dev/null");
        let dri2 = be.handles.insert(OwnedFd::from(null));
        be.handle_kinds.insert(dri2, DeviceKind::Dri(0));

        let (status, reply) = ask(&mut be, dri2, 5, 0);
        assert_eq!(status, 0);
        let reply = reply.expect("a reply body");
        assert_eq!(reply.gem_handle, IMPORTED_HANDLE);
        assert_eq!(reply.size, 8 << 20);
        assert_eq!(reply.flags, RM_RESOURCE_IMPORT_MODIFIER);
        assert_eq!(reply.modifier, 0x0300_0000_0060_6015);
        assert_eq!(
            be.rm_layouts.get(dri2 as u32, IMPORTED_HANDLE),
            Some(Some(0x0300_0000_0060_6015)),
            "the new handle carries the layout, for a later flip or export"
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .contains(&super::super::rm_resource::DRM_IOCTL_PRIME_FD_TO_HANDLE)
        );
        assert_eq!(be.rm_resource_imports(), 1);

        // Refusals: not a render node, no such resource, not an RM-export
        // resource, flags.
        assert_eq!(ask(&mut be, ctl, 5, 0).0, -libc::EBADF);
        assert_eq!(ask(&mut be, 999, 5, 0).0, -libc::EBADF);
        assert_eq!(ask(&mut be, dri2, 7, 0).0, -libc::ENOENT);
        assert_eq!(ask(&mut be, dri2, 6, 0).0, -libc::EINVAL);
        assert_eq!(ask(&mut be, dri2, 5, 1).0, -libc::EINVAL);
        assert_eq!(be.rm_resource_imports(), 1);

        // A Venus blob whose renderer export is a dma-buf (the mock's memfd
        // taken for one) is imported the same way, with no modifier.
        be.venus.as_mut().unwrap().assume_dmabufs(true);
        let (status, reply) = ask(&mut be, dri2, 6, 0);
        assert_eq!(status, 0);
        let reply = reply.expect("a reply body");
        assert_eq!(reply.gem_handle, IMPORTED_HANDLE);
        assert_eq!((reply.flags, reply.modifier), (0, 0), "layout unknown");
        assert_eq!(be.rm_resource_imports(), 2);
        be.venus.as_mut().unwrap().assume_dmabufs(false);

        // The creator's file closes: the resource, and the import, live on.
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: MsgType::Close as u32,
                handle: dri as u32,
                status: 0,
                padding: 0,
            },
        );
        let mut r = vec![0u8; 256];
        be.dispatch(&v, &mut r);
        assert_eq!(ask(&mut be, dri2, 5, 0).0, 0);
    }

    #[test]
    fn export_refuses_what_is_not_a_render_node_object() {
        let (be, dri, ctl, _) = drm_backend(1 << 20);
        let v = be.rm_view();
        assert_eq!(
            v.export(ctl as u32, GEM_HANDLE).unwrap_err(),
            libc::EBADF,
            "not a DRM file"
        );
        assert_eq!(
            v.export(999, GEM_HANDLE).unwrap_err(),
            libc::EBADF,
            "not open"
        );
        assert_eq!(
            v.export(dri as u32, 0).unwrap_err(),
            libc::EBADF,
            "GEM handle 0"
        );
        assert_eq!(
            v.export(dri as u32, 5).unwrap_err(),
            libc::ENOENT,
            "not that file's"
        );
        let o = v.export(dri as u32, GEM_HANDLE).unwrap();
        assert!(o.dmabuf.as_raw_fd() >= 0);
        assert_eq!(o.modifier, None, "no import seen");
    }
}

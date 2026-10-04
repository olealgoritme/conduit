//! SET_SCANOUT_BLOB / RESOURCE_FLUSH: the guest names which blob is the
//! screen, and says when it changed. The renderer exports that blob as a
//! dma-buf once per resource and layout, and each flush hands it to the same
//! display path a Linux guest's flip takes (docs/SCANOUT.md). Nothing copies.
//!
//! The image's layout is the guest's: `SET_SCANOUT_BLOB`'s size, `strides[0]`,
//! `offsets[0]`, and its virtio-gpu format as the `DRM_FORMAT_*` of the same
//! memory layout. A Venus blob carries no layout the host could check it
//! against, and no modifier, so Venus scanout images must be linear for now:
//! a guest driver must allocate its scanout images with linear tiling, or
//! the viewer shows garbage.

use super::*;
use crate::display::FrameGeometry;
use conduit_venus::ScanoutLayout;
use std::os::fd::AsRawFd;

/// Scanout 0, as SET_SCANOUT_BLOB last set it.
pub(super) struct Scanout {
    pub(super) resource_id: u32,
    /// `virtio_gpu_formats`, as the guest gave it.
    pub(super) format: u32,
    /// The image, with `format` as its `DRM_FORMAT_*`.
    pub(super) layout: ScanoutLayout,
}

/// A resource exported for scanout, with one layout.
pub(super) struct Export {
    layout: ScanoutLayout,
    dmabuf: conduit_venus::Dmabuf,
}

impl Venus {
    pub(super) fn set_scanout_blob(&mut self, s: &SetScanoutBlob, env: Env<'_>) -> Answer {
        if s.scanout_id != 0 {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        // Resource 0 turns the scanout off.
        if s.resource_id == 0 {
            if self.scanout.take().is_some()
                && let Some(link) = env.display
            {
                link.disable();
            }
            return Ok(Reply::NoData);
        }
        let Some(r) = self.resources.get(&s.resource_id) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        // A format the viewer has no DRM name for cannot be shown.
        let Some(fourcc) = format::drm_fourcc(s.format) else {
            log::debug!("venus: scanout format {} is not one we can show", s.format);
            return Err(RESP_ERR_INVALID_PARAMETER);
        };
        if s.width == 0 || s.height == 0 {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        // The visible rectangle inside the image, and the image inside the
        // blob, as QEMU checks it.
        let fits = |at: u32, len: u32, max: u32| at.checked_add(len).is_some_and(|e| e <= max);
        let end = u64::from(s.offsets[0]) + u64::from(s.strides[0]) * u64::from(s.height);
        if !fits(s.r.x, s.r.width, s.width)
            || !fits(s.r.y, s.r.height, s.height)
            || u64::from(s.strides[0]) < u64::from(s.width) * 4
            || end > r.size
        {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        self.scanout = Some(Scanout {
            resource_id: s.resource_id,
            format: s.format,
            layout: ScanoutLayout {
                width: s.width,
                height: s.height,
                stride: s.strides[0],
                offset: s.offsets[0],
                fourcc,
            },
        });
        Ok(Reply::NoData)
    }

    /// A flush of the scanout's resource is a frame; of any other resource,
    /// nothing to show.
    pub(super) fn flush(&mut self, f: &ResourceFlush, env: Env<'_>) -> Answer {
        if !self.resources.contains_key(&f.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        let Some(s) = self
            .scanout
            .as_ref()
            .filter(|s| s.resource_id == f.resource_id)
        else {
            return Ok(Reply::NoData);
        };
        let Some(link) = env.display else {
            return Ok(Reply::NoData);
        };
        let (id, layout, format) = (s.resource_id, s.layout, s.format);
        let r = self.resources.get_mut(&id).expect("checked above");
        if r.export.as_ref().is_none_or(|e| e.layout != layout) {
            r.export = None;
            match self.renderer.export_scanout(id, layout) {
                Ok(dmabuf) => {
                    self.resources.get_mut(&id).expect("checked above").export =
                        Some(Export { layout, dmabuf })
                }
                Err(e) => {
                    log::warn!(
                        "venus: exporting resource {id} for scanout ({}x{} format {format} stride {} offset {}): {e}; frame dropped",
                        layout.width,
                        layout.height,
                        layout.stride,
                        layout.offset
                    );
                    return Err(self.renderer_error(&e));
                }
            }
        }
        let e = &self.resources[&id]
            .export
            .as_ref()
            .expect("exported above")
            .dmabuf;
        // The guest's layout, which the renderer echoes into the dma-buf;
        // the modifier is the renderer's (linear).
        let g = FrameGeometry {
            width: layout.width,
            height: layout.height,
            stride: layout.stride,
            offset: layout.offset,
            fourcc: layout.fourcc,
            modifier: e.modifier,
        };
        link.flip_dmabuf(e.fd.as_raw_fd(), &g);
        Ok(Reply::NoData)
    }

    /// What the scanout shows, for tests: (resource, width, height, format,
    /// stride, offset).
    #[cfg(test)]
    pub(super) fn scanout_state(&self) -> Option<(u32, u32, u32, u32, u32, u32)> {
        self.scanout.as_ref().map(|s| {
            let l = &s.layout;
            (
                s.resource_id,
                l.width,
                l.height,
                s.format,
                l.stride,
                l.offset,
            )
        })
    }

    /// The layout the scanout's resource was last exported with, for tests.
    #[cfg(test)]
    pub(super) fn exported_layout(&self, res_id: u32) -> Option<ScanoutLayout> {
        self.resources
            .get(&res_id)?
            .export
            .as_ref()
            .map(|e| e.layout)
    }
}

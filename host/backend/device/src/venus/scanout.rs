//! SET_SCANOUT_BLOB / RESOURCE_FLUSH: the guest names which blob is the
//! screen, and says when it changed. The renderer exports that blob as a
//! dma-buf once per resource and size, and each flush hands it to the same
//! display path a Linux guest's flip takes (docs/SCANOUT.md). Nothing copies.

use super::*;
use crate::display::FrameGeometry;
use std::os::fd::AsRawFd;

/// Scanout 0, as SET_SCANOUT_BLOB last set it.
pub(super) struct Scanout {
    pub(super) resource_id: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    /// `virtio_gpu_formats`.
    pub(super) format: u32,
    pub(super) stride: u32,
    pub(super) offset: u32,
}

/// A resource exported for scanout, at one size.
pub(super) struct Export {
    width: u32,
    height: u32,
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
        if s.width == 0 || s.height == 0 || format::drm_fourcc(s.format).is_none() {
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
            width: s.width,
            height: s.height,
            format: s.format,
            stride: s.strides[0],
            offset: s.offsets[0],
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
        let (id, width, height) = (s.resource_id, s.width, s.height);
        // What the guest said, for a renderer that leaves a field unset.
        let guest = (
            s.stride,
            s.offset,
            format::drm_fourcc(s.format).expect("checked at SET_SCANOUT_BLOB"),
        );
        let r = self.resources.get_mut(&id).expect("checked above");
        let cached = r
            .export
            .as_ref()
            .is_some_and(|e| e.width == width && e.height == height);
        if !cached {
            r.export = None;
            match self.renderer.export_scanout(id, width, height) {
                Ok(dmabuf) => {
                    self.resources.get_mut(&id).expect("checked above").export = Some(Export {
                        width,
                        height,
                        dmabuf,
                    })
                }
                Err(e) => {
                    log::warn!(
                        "venus: exporting resource {id} for scanout ({width}x{height}): {e}; frame dropped"
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
        // The image as the renderer laid it out: its stride, offset, format
        // and modifier describe the dma-buf, which the guest's need not. A
        // stride or format of 0 is one the renderer did not fill in.
        let g = if e.stride != 0 && e.fourcc != 0 {
            FrameGeometry {
                width,
                height,
                stride: e.stride,
                offset: e.offset,
                fourcc: e.fourcc,
                modifier: e.modifier,
            }
        } else {
            FrameGeometry {
                width,
                height,
                stride: guest.0,
                offset: guest.1,
                fourcc: guest.2,
                modifier: e.modifier,
            }
        };
        link.flip_dmabuf(e.fd.as_raw_fd(), &g);
        Ok(Reply::NoData)
    }

    /// What the scanout shows, for tests: (resource, width, height, format,
    /// stride, offset).
    #[cfg(test)]
    pub(super) fn scanout_state(&self) -> Option<(u32, u32, u32, u32, u32, u32)> {
        self.scanout.as_ref().map(|s| {
            (
                s.resource_id,
                s.width,
                s.height,
                s.format,
                s.stride,
                s.offset,
            )
        })
    }
}

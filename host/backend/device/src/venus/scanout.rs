//! SET_SCANOUT_BLOB / RESOURCE_FLUSH: the guest names which blob is the
//! screen, and says when it changed. The renderer exports that blob as a
//! dma-buf once per resource and layout, and each flush hands it to the same
//! display path a Linux guest's flip takes (docs/SCANOUT.md). Nothing copies.
//!
//! The image's layout is the guest's: `SET_SCANOUT_BLOB`'s size, `strides[0]`,
//! `offsets[0]`, and its virtio-gpu format as the `DRM_FORMAT_*` of the same
//! memory layout. A Venus blob carries no modifier, so the backend infers one
//! from the blob's size: a blob exactly as big as the image is linear, and one
//! with room for the rows NVIDIA pads an optimal-tiling image to is
//! block-linear ([`modifier_for`]). `CONDUIT_VENUS_SCANOUT_MODIFIER` overrides
//! the guess, for experiments.

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
    /// `DRM_FORMAT_MOD_*` the image is shown with.
    pub(super) modifier: u64,
}

/// `DRM_FORMAT_MOD_LINEAR`.
const MOD_LINEAR: u64 = conduit_venus::DRM_FORMAT_MOD_LINEAR;

/// `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c=0, s=1, g=2, k=0x06, h)`: an
/// uncompressed block-linear image of a desktop Turing-or-later GPU, with
/// blocks `2^h` GOBs (of 8 rows by 64 bytes) high. The host's NVIDIA driver
/// advertises it for every scanout format with `h` 0..=5 (measured with
/// eglQueryDmaBufModifiersEXT on an RTX 5090, driver 610), and it is what
/// Linux guests already flip.
const fn nvidia_block_linear(h: u32) -> u64 {
    0x0300_0000_0060_6010 | h as u64
}

/// The modifier a scanout image has, from the size of the blob holding it.
///
/// The NVIDIA driver lays a `VK_IMAGE_TILING_OPTIMAL` image out block-linear,
/// with blocks `min(16, next_pow2(ceil(rows / 8)))` GOBs high (16 for any
/// screen-sized image), and its memory holds whole blocks: `roundup(rows,
/// 8 << h)` rows of `stride` bytes, the stride a whole number of 64-byte
/// GOBs. A 1920x1080 XRGB8888 image takes 7680 * 1152 bytes, a linear one
/// 7680 * 1080. A blob with room for the padded rows (and possibly more) is
/// taken as block-linear; one without, as linear. Where there is no padding
/// (a height that is a multiple of the block height: 768, 1024) the two
/// cannot be told apart and linear is assumed.
pub(super) fn modifier_for(layout: &ScanoutLayout, blob_size: u64) -> u64 {
    if !layout.stride.is_multiple_of(64) {
        return MOD_LINEAR;
    }
    let h = layout
        .height
        .div_ceil(8)
        .next_power_of_two()
        .trailing_zeros()
        .min(4);
    let rows = u64::from(layout.height).next_multiple_of(8 << h);
    let need = u64::from(layout.offset) + u64::from(layout.stride) * rows;
    if rows > u64::from(layout.height) && blob_size >= need {
        nvidia_block_linear(h)
    } else {
        MOD_LINEAR
    }
}

/// `CONDUIT_VENUS_SCANOUT_MODIFIER`, read once: `linear` or a hex modifier.
pub(super) fn modifier_override() -> Option<u64> {
    let v = std::env::var("CONDUIT_VENUS_SCANOUT_MODIFIER").ok()?;
    let m = parse_modifier(&v);
    match m {
        Some(m) => log::info!(
            "venus: scanouts are shown with modifier {m:#018x} (CONDUIT_VENUS_SCANOUT_MODIFIER)"
        ),
        None => log::warn!(
            "venus: CONDUIT_VENUS_SCANOUT_MODIFIER={v:?} is neither `linear` nor a hex modifier; ignored"
        ),
    }
    m
}

pub(super) fn parse_modifier(v: &str) -> Option<u64> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("linear") {
        return Some(MOD_LINEAR);
    }
    let hex = v
        .strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .unwrap_or(v);
    u64::from_str_radix(hex, 16).ok()
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
        let layout = ScanoutLayout {
            width: s.width,
            height: s.height,
            stride: s.strides[0],
            offset: s.offsets[0],
            fourcc,
        };
        let modifier = self
            .forced_modifier
            .unwrap_or_else(|| modifier_for(&layout, r.size));
        log::debug!(
            "venus: scanout is resource {} ({} bytes): {}x{} format {} stride {} offset {} modifier {modifier:#018x}",
            s.resource_id,
            r.size,
            s.width,
            s.height,
            s.format,
            s.strides[0],
            s.offsets[0]
        );
        self.scanout = Some(Scanout {
            resource_id: s.resource_id,
            format: s.format,
            layout,
            modifier,
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
        let (id, layout, format, modifier) = (s.resource_id, s.layout, s.format, s.modifier);
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
        // The guest's layout, which the renderer echoes into the dma-buf,
        // and the modifier SET_SCANOUT_BLOB inferred: the renderer's is
        // always linear, as it cannot know better.
        let g = FrameGeometry {
            width: layout.width,
            height: layout.height,
            stride: layout.stride,
            offset: layout.offset,
            fourcc: layout.fourcc,
            modifier,
        };
        link.flip_dmabuf(e.fd.as_raw_fd(), &g);
        Ok(Reply::NoData)
    }

    /// The modifier the scanout is shown with, for tests.
    #[cfg(test)]
    pub(super) fn scanout_modifier(&self) -> Option<u64> {
        self.scanout.as_ref().map(|s| s.modifier)
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

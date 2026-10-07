//! `CMD_SET_CURSOR_BLOB` (a Conduit extension): a Windows guest's hardware
//! cursor (docs/SCANOUT.md "Hardware cursor, Windows guests").
//!
//! The KMD keeps the pointer image in a blob of its own and names a
//! rectangle of it. The blob is exported once, as the scanout's are, and the
//! image goes to the same display path a Linux guest's cursor plane takes
//! ([`DisplayLink::cursor`]): `CMD_CURSOR` to every client that takes one,
//! which shows it as the host pointer's image. The host positions it, so a
//! move never travels; the guest sends this only when the shape or the
//! visibility changes. Nothing copies.

use super::*;
use conduit_venus::ScanoutLayout;
use protocol::messages::{CURSOR_F_VISIBLE, CursorUpdate};
use std::os::fd::AsRawFd;

/// Largest cursor side, as the Linux guest's cursor plane
/// (`display::wire::CURSOR_MAX_DIM`).
pub(super) const CURSOR_MAX: u32 = 256;

impl Venus {
    /// Serve `CMD_SET_CURSOR_BLOB` from now on (the backend sets
    /// `NVGPU_CFG_VENUS_CURSOR` when this says yes). Only with a display.
    pub fn enable_cursor(&mut self) -> bool {
        self.cursor_on = self.display.is_some();
        if self.cursor_on {
            log::info!("venus: the guest's hardware cursor is served (CMD_SET_CURSOR_BLOB)");
        }
        self.cursor_on
    }

    pub(super) fn set_cursor_blob(&mut self, c: &SetCursorBlob, env: Env<'_>) -> Answer {
        if !self.cursor_on {
            return Err(RESP_ERR_UNSPEC);
        }
        if c.scanout_id != 0 {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        if !c.visible() {
            self.hide_cursor(env.display);
            return Ok(Reply::NoData);
        }
        let Some(r) = self.resources.get(&c.resource_id) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        // What the viewer and the Linux cursor plane take: ARGB8888, linear,
        // at most 256 square, the hotspot inside, the rows inside the blob.
        let end = u64::from(c.offset)
            + u64::from(c.stride) * u64::from(c.height.saturating_sub(1))
            + u64::from(c.width) * 4;
        if c.format != format::B8G8R8A8_UNORM
            || c.width == 0
            || c.height == 0
            || c.width > CURSOR_MAX
            || c.height > CURSOR_MAX
            || c.hot_x >= c.width
            || c.hot_y >= c.height
            || u64::from(c.stride) < u64::from(c.width) * 4
            || end > r.size
        {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let fourcc = format::drm_fourcc(c.format).expect("checked above");
        let Some(link) = env.display else {
            self.cursor = Some(c.resource_id);
            return Ok(Reply::NoData);
        };
        let id = c.resource_id;
        let r = self.resources.get_mut(&id).expect("checked above");
        // An RM-export blob is a dma-buf already. Any other is exported once,
        // whatever rectangle it is first named with: the descriptor is the
        // memory, and the rectangle travels with each update. A scanout export
        // of the same resource is reused the same way.
        let fd = if r.rm.is_some() {
            r.fd.as_raw_fd()
        } else {
            if r.export.is_none() {
                let layout = ScanoutLayout {
                    width: c.width,
                    height: c.height,
                    stride: c.stride,
                    offset: c.offset,
                    fourcc,
                };
                match self.renderer.export_scanout(id, layout) {
                    Ok(dmabuf) => {
                        self.resources.get_mut(&id).expect("checked above").export =
                            Some(scanout::Export::new(layout, dmabuf))
                    }
                    Err(e) => {
                        log::warn!("venus: exporting resource {id} for the cursor: {e}");
                        return Err(self.renderer_error(&e));
                    }
                }
            }
            self.resources[&id]
                .export
                .as_ref()
                .expect("exported above")
                .fd()
        };
        self.cursor_seq = self.cursor_seq.wrapping_add(1);
        let u = CursorUpdate {
            scanout: 0,
            width: c.width,
            height: c.height,
            hot_x: c.hot_x,
            hot_y: c.hot_y,
            // A Venus resource, not a GEM pair: informational here.
            owner_handle: 0,
            host_handle: id,
            stride: c.stride,
            offset: c.offset,
            fourcc,
            modifier: conduit_venus::DRM_FORMAT_MOD_LINEAR,
            crtc_x: c.x,
            crtc_y: c.y,
            flags: CURSOR_F_VISIBLE,
            seq: self.cursor_seq,
        };
        self.cursor = Some(id);
        link.cursor(Some(fd), &u);
        Ok(Reply::NoData)
    }

    /// Hide the guest's cursor (asked for, or its resource went).
    pub(super) fn hide_cursor(&mut self, display: Option<&DisplayLink>) {
        self.cursor = None;
        if !self.cursor_on {
            return;
        }
        if let Some(link) = display {
            self.cursor_seq = self.cursor_seq.wrapping_add(1);
            link.cursor(
                None,
                &CursorUpdate {
                    seq: self.cursor_seq,
                    ..Default::default()
                },
            );
        }
    }

    /// The resource the cursor shows, for tests.
    #[cfg(test)]
    pub(super) fn cursor_resource(&self) -> Option<u32> {
        self.cursor
    }
}

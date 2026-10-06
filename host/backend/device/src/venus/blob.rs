//! Blob resources and region 3 (docs/VENUS.md): the renderer allocates a
//! host-visible blob and hands back a descriptor; the guest picks where in
//! region 3 it wants it, and the transport places it there.
//!
//! Region 3 is the guest's to lay out, as virtio-gpu's host-visible region
//! is: the backend checks a placement and keeps the books, but chooses
//! nothing. That makes the checks the whole defence -- an offset that
//! overlapped another placement would have the frontend replace part of one
//! blob's mapping with another's.

use super::*;
use std::os::fd::AsRawFd;

use crate::shm_regions::page_align;

/// What is placed in region 3: offset to (length, resource id).
#[derive(Default)]
pub(super) struct Maps(BTreeMap<u64, (u64, u32)>);

impl Maps {
    /// Whether `[offset, offset + len)` touches a placement.
    pub(super) fn overlaps(&self, offset: u64, len: u64) -> bool {
        let end = offset.saturating_add(len);
        // The last placement starting before `end` is the only one that can
        // reach into the range: placements never overlap each other.
        self.0
            .range(..end)
            .next_back()
            .is_some_and(|(&at, &(l, _))| at.saturating_add(l) > offset)
    }

    pub(super) fn insert(&mut self, offset: u64, len: u64, id: u32) {
        self.0.insert(offset, (len, id));
    }

    pub(super) fn remove(&mut self, offset: u64) -> Option<(u64, u32)> {
        self.0.remove(&offset)
    }

    /// Everything, as (offset, length, resource id).
    pub(super) fn drain(&mut self) -> Vec<(u64, u64, u32)> {
        std::mem::take(&mut self.0)
            .into_iter()
            .map(|(at, (len, id))| (at, len, id))
            .collect()
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

impl Venus {
    /// `blob_mem = HOST3D`, Conduit's `BLOB_MEM_RM_EXPORT` (`rm.rs`), or
    /// `GUEST` (`guest.rs`, only with `--venus-guest-blobs`). `entries` are
    /// the `virtio_gpu_mem_entry`s after the struct, `nr_entries` of them.
    pub(super) fn create_blob(
        &mut self,
        c: &ResourceCreateBlob,
        entries: &[u8],
        env: Env<'_>,
    ) -> Answer {
        if c.blob_mem == BLOB_MEM_RM_EXPORT {
            return self.create_rm_blob(c, env);
        }
        if c.blob_mem == BLOB_MEM_GUEST && self.guest_blobs {
            return self.create_guest_blob(c, entries, env);
        }
        let ctx = c.hdr.ctx_id;
        log::debug!(
            "venus: create blob res {} ctx {}: blob_mem {} flags {:#x} blob_id {} size {} entries {}",
            c.resource_id,
            ctx,
            c.blob_mem,
            c.blob_flags,
            c.blob_id,
            c.size,
            c.nr_entries
        );
        if !self.contexts.contains(&ctx) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        if c.resource_id == 0 || self.resources.contains_key(&c.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        if c.blob_mem != BLOB_MEM_HOST3D || c.nr_entries != 0 {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        // Any nonzero size, as QEMU's virtio-gpu takes it: Mesa's Venus ring
        // and reply buffers are not page multiples (the Windows guest's shared
        // ring is 128 KiB + 196). A mapping covers whole pages (map_blob).
        if c.size == 0 || page_align(c.size) > self.hostmem_len {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        if self.resources.len() >= MAX_RESOURCES {
            log::warn!(
                "venus: resource {} refused: {MAX_RESOURCES} already",
                c.resource_id
            );
            return Err(RESP_ERR_OUT_OF_MEMORY);
        }
        let blob = self
            .renderer
            .create_blob(ctx, c.resource_id, c.blob_id, c.size, c.blob_flags)
            .map_err(|e| self.renderer_error(&e))?;
        // A blob smaller than asked for would have the guest map past its
        // end; the frontend would place it, and the tail would fault.
        if blob.size < c.size {
            log::warn!(
                "venus: resource {}: the renderer made {} bytes of {} asked for",
                c.resource_id,
                blob.size,
                c.size
            );
            self.renderer.unref(c.resource_id);
            return Err(RESP_ERR_UNSPEC);
        }
        self.resources.insert(
            c.resource_id,
            Resource {
                ctx_id: ctx,
                size: c.size,
                flags: c.blob_flags,
                map_info: blob.map_info,
                fd: blob.fd,
                mapped: None,
                // The renderer attaches a blob to the context it was made in.
                attached: HashSet::from([ctx]),
                export: None,
                rm: None,
                guest: None,
            },
        );
        Ok(Reply::NoData)
    }

    /// Place the blob at the guest's offset in region 3.
    pub(super) fn map_blob(&mut self, m: &ResourceMapBlob, env: Env<'_>) -> Answer {
        let Some(r) = self.resources.get(&m.resource_id) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        if r.flags & BLOB_FLAG_USE_MAPPABLE == 0 || r.mapped.is_some() {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let len = page_align(r.size);
        let inside = m
            .offset
            .checked_add(len)
            .is_some_and(|end| end <= self.hostmem_len);
        if !m.offset.is_multiple_of(PAGE) || !inside || self.maps.overlaps(m.offset, len) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let Some(window) = env.window else {
            log::warn!(
                "venus: resource {} cannot be mapped: no frontend channel to place it with",
                m.resource_id
            );
            return Err(RESP_ERR_UNSPEC);
        };
        if let Err(e) = window.place_blob(m.offset, len, r.fd.as_raw_fd()) {
            log::warn!(
                "venus: placing resource {} at {:#x} in region 3: {e}",
                m.resource_id,
                m.offset
            );
            return Err(RESP_ERR_UNSPEC);
        }
        let map_info = r.map_info;
        self.maps.insert(m.offset, len, m.resource_id);
        self.resources
            .get_mut(&m.resource_id)
            .expect("looked up above")
            .mapped = Some(m.offset);
        let info = RespMapInfo {
            map_info,
            ..Default::default()
        };
        Ok(Reply::With(
            RESP_OK_MAP_INFO,
            info.to_bytes()[CTRL_HDR_LEN..].to_vec(),
        ))
    }

    pub(super) fn unmap_blob(&mut self, id: u32, env: Env<'_>) -> Answer {
        let Some(r) = self.resources.get_mut(&id) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        let Some(offset) = r.mapped.take() else {
            return Err(RESP_ERR_INVALID_PARAMETER);
        };
        self.withdraw(offset, id, env);
        Ok(Reply::NoData)
    }

    /// Unmapped if mapped, off the scanout if shown, detached, then freed.
    pub(super) fn unref(&mut self, id: u32, env: Env<'_>) -> Answer {
        let Some(r) = self.resources.remove(&id) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        if let Some(g) = &r.guest {
            self.guest_live.remove(g);
        }
        if let Some(offset) = r.mapped {
            self.withdraw(offset, id, env);
        }
        if self.scanout.as_ref().is_some_and(|s| s.resource_id == id) {
            self.scanout = None;
            if let Some(link) = env.display {
                link.disable();
            }
        }
        // A release still owed for it names an id the guest may reuse.
        if let Some(link) = env.display {
            link.forget_resource(id);
        }
        for &ctx in &r.attached {
            self.renderer.ctx_detach(ctx, id);
        }
        self.renderer.unref(id);
        log::trace!(
            "venus: resource {id} of ctx {} ({} bytes) freed",
            r.ctx_id,
            r.size
        );
        Ok(Reply::NoData)
    }

    fn withdraw(&mut self, offset: u64, id: u32, env: Env<'_>) {
        let Some((len, _)) = self.maps.remove(offset) else {
            return;
        };
        if let Some(w) = env.window
            && let Err(e) = w.withdraw_blob(offset, len)
        {
            log::warn!("venus: withdrawing resource {id} from region 3 at {offset:#x}: {e}");
        }
    }
}

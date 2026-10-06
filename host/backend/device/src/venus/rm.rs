//! RM-export blobs (docs/VENUS.md "RM-export blobs"): memory NVK on RM
//! rendered into, made a Venus resource so Venus contexts (DWM, the D3D
//! bridge) can read it without a copy through the CPU.
//!
//! `RESOURCE_CREATE_BLOB { blob_mem = BLOB_MEM_RM_EXPORT, blob_id =
//! rm_handle << 32 | gem_handle }` names a host GEM object on a render node
//! the guest opened. The RM side of the backend ([`RmExports`]) checks the
//! handle and exports the object as a dma-buf; the renderer imports that as
//! resource `resource_id` (virglrenderer's `import_blob`, fd type dma-buf),
//! which is attached to the creating context. A Venus context then imports
//! it with `VkImportMemoryResourceInfoMESA`, which vkr turns into a
//! `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` import; the image bound
//! to it must be `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` with the modifier
//! NVK used (NVIDIA's driver refuses an OPTIMAL import of this memory).
//!
//! The backend keeps its own dma-buf descriptor as the resource's `fd`, so the
//! memory stays alive until `RESOURCE_UNREF` (or a reset), whatever the guest
//! does with the render node or the GEM handle meanwhile.

use super::*;
use std::os::fd::{AsFd, AsRawFd};

/// The size of the object behind a dma-buf: its end, as virglrenderer's own
/// attach measures it. `None` if the descriptor cannot seek.
pub(super) fn dmabuf_size(fd: BorrowedFd<'_>) -> Option<u64> {
    // SAFETY: lseek on a descriptor we hold; the offset is the dma-buf's own
    // file position, which nothing else here uses.
    let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    // SAFETY: as above, back to the start.
    unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
    (end > 0).then_some(end as u64)
}

/// Whether `fd` is a dma-buf: its file system is dma-buf's
/// (`DMA_BUF_MAGIC`). A renderer export of a Venus blob is either that or an
/// `OPAQUE_FD`, which on NVIDIA is a driver handle, not shareable memory.
pub(super) fn is_dmabuf(fd: BorrowedFd<'_>) -> bool {
    const DMA_BUF_MAGIC: libc::c_long = 0x444d_4142;
    // SAFETY: fstatfs into a zeroed struct on a descriptor we hold.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstatfs(fd.as_raw_fd(), &mut st) } == 0)
        && st.f_type as libc::c_long == DMA_BUF_MAGIC
}

/// A resource's memory as `RmResourceImport` sees it.
pub struct RmResource<'a> {
    /// The backend's own dma-buf of the object, borrowed.
    pub dmabuf: BorrowedFd<'a>,
    /// The object's size (the dma-buf's).
    pub size: u64,
    /// The modifier its GEM import had, when the backend saw it.
    pub modifier: Option<u64>,
}

impl Venus {
    /// Refuse with a `RESP_ERR_*` and the errno the guest is told
    /// (`errno_padding`).
    fn refuse_rm(&mut self, resp: u32, errno: i32, why: &'static str) -> Answer {
        self.count(why);
        self.refusal_errno = Some(errno);
        Err(resp)
    }

    pub(super) fn create_rm_blob(&mut self, c: &ResourceCreateBlob, env: Env<'_>) -> Answer {
        let ctx = c.hdr.ctx_id;
        let (rm_handle, gem_handle) = rm_export_ids(c.blob_id);
        log::debug!(
            "venus: create RM-export blob res {} ctx {}: file {rm_handle} GEM handle {gem_handle} \
             size {} flags {:#x}",
            c.resource_id,
            ctx,
            c.size,
            c.blob_flags
        );
        let Some(rm) = env.rm.filter(|_| self.rm_import) else {
            return self.refuse_rm(
                RESP_ERR_UNSPEC,
                libc::EOPNOTSUPP,
                "refused: rm import unsupported",
            );
        };
        if !self.contexts.contains(&ctx) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        if c.resource_id == 0 || self.resources.contains_key(&c.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        // No guest pages. MAPPABLE only for system memory (checked once the
        // object is known); SHAREABLE changes nothing (the resource id is
        // what is shared); nothing else.
        let known_flags = BLOB_FLAG_USE_MAPPABLE | BLOB_FLAG_USE_SHAREABLE;
        let mappable = c.blob_flags & BLOB_FLAG_USE_MAPPABLE != 0;
        if c.blob_flags & !known_flags != 0
            || c.nr_entries != 0
            || c.size == 0
            || (mappable && crate::shm_regions::page_align(c.size) > self.hostmem_len)
        {
            return self.refuse_rm(
                RESP_ERR_INVALID_PARAMETER,
                libc::EINVAL,
                "refused: rm blob shape",
            );
        }
        if self.resources.len() >= MAX_RESOURCES {
            return self.refuse_rm(
                RESP_ERR_OUT_OF_MEMORY,
                libc::ENOMEM,
                "refused: rm blob resources",
            );
        }
        let obj = match rm.export(rm_handle, gem_handle) {
            Ok(o) => o,
            Err(errno) => {
                log::warn!(
                    "venus: RM-export blob res {}: file {rm_handle} GEM handle {gem_handle}: \
                     errno {errno}",
                    c.resource_id
                );
                // A handle that is not the guest's render node, or a GEM
                // handle that file does not have, is the guest's mistake.
                let resp = if matches!(errno, libc::EBADF | libc::ENOENT | libc::EINVAL) {
                    RESP_ERR_INVALID_PARAMETER
                } else {
                    RESP_ERR_UNSPEC
                };
                return self.refuse_rm(resp, errno, "refused: rm export");
            }
        };
        // A mapping is the dma-buf's own mmap placed in region 3: only system
        // memory is guest memory that way, with the CPU caching RM gave it.
        // Video memory sits behind BAR1 and is refused, as is memory whose
        // allocation the backend did not see.
        let map_cache = obj.placement.and_then(|p| p.map_cache());
        if mappable && map_cache.is_none() {
            log::warn!(
                "venus: RM-export blob res {}: MAPPABLE asked of {} memory; only system memory \
                 maps",
                c.resource_id,
                match obj.placement {
                    Some(_) => "video",
                    None => "unknown (allocation not seen)",
                }
            );
            return self.refuse_rm(
                RESP_ERR_INVALID_PARAMETER,
                libc::EOPNOTSUPP,
                "refused: rm blob not mappable",
            );
        }
        // The guest's size may be smaller than the object (RM rounds to
        // 64 KiB) but never larger: the renderer would let a context bind an
        // image that runs past the end of the memory.
        let Some(object) = dmabuf_size(obj.dmabuf.as_fd()) else {
            return self.refuse_rm(RESP_ERR_UNSPEC, libc::EIO, "refused: rm size unknown");
        };
        if c.size > object {
            log::warn!(
                "venus: RM-export blob res {}: {} bytes asked of an object of {object}",
                c.resource_id,
                c.size
            );
            return self.refuse_rm(RESP_ERR_INVALID_PARAMETER, libc::ERANGE, "refused: rm size");
        }
        if let Err(e) = self
            .renderer
            .import_dmabuf(c.resource_id, obj.dmabuf.as_fd(), c.size)
        {
            log::warn!("venus: importing RM-export blob res {}: {e}", c.resource_id);
            let resp = self.renderer_error(&e);
            return self.refuse_rm(resp, libc::EIO, "refused: rm renderer import");
        }
        // An imported resource belongs to no context until attached; the
        // guest's own CTX_ATTACH_RESOURCE that follows is then a no-op.
        if let Err(e) = self.renderer.ctx_attach(ctx, c.resource_id) {
            log::warn!(
                "venus: attaching RM-export blob res {} to ctx {ctx}: {e}",
                c.resource_id
            );
            self.renderer.unref(c.resource_id);
            let resp = self.renderer_error(&e);
            return self.refuse_rm(resp, libc::EIO, "refused: rm attach");
        }
        log::debug!(
            "venus: RM-export blob res {} is {} of {object} bytes, modifier {}",
            c.resource_id,
            c.size,
            obj.modifier
                .map_or("unknown".into(), |m| format!("{m:#018x}"))
        );
        self.count("rm_export_blob");
        self.resources.insert(
            c.resource_id,
            Resource {
                ctx_id: ctx,
                size: c.size,
                flags: c.blob_flags,
                map_info: map_cache.filter(|_| mappable).unwrap_or(0),
                fd: obj.dmabuf,
                mapped: None,
                attached: HashSet::from([ctx]),
                export: None,
                rm: Some(RmImport {
                    modifier: obj.modifier,
                }),
                guest: None,
            },
        );
        Ok(Reply::NoData)
    }

    /// A resource's memory as a dma-buf, for `RmResourceImport`: the
    /// backend's own reference, the object's size and the modifier the
    /// resource was created with, when known.
    ///
    /// - An RM-export blob: the dma-buf the backend holds for it, with the
    ///   modifier NVK imported it with.
    /// - A Venus blob (`HOST3D`, host Vulkan memory) whose renderer export is
    ///   a dma-buf: that export. nvidia-drm imports memory NVIDIA's Vulkan
    ///   driver exported as `DMA_BUF` into an RM client exactly (spike X4,
    ///   `guest/nvk-rm/tests/vk_dmabuf_to_rm.c`). Its modifier is unknown:
    ///   the image layout is the creating Venus context's, which the memory
    ///   does not carry (the importer takes it from the surface's metadata).
    ///
    /// `Err` is an errno: `ENOENT` for no such resource, `EINVAL` for a
    /// Venus blob exported as an `OPAQUE_FD` (a driver handle nvidia-drm
    /// cannot take), `EIO` when the object's size cannot be read.
    pub fn rm_resource(&self, res_id: u32) -> std::result::Result<RmResource<'_>, i32> {
        let r = self.resources.get(&res_id).ok_or(libc::ENOENT)?;
        let modifier = match r.rm {
            Some(rm) => rm.modifier,
            None if (self.is_dmabuf)(r.fd.as_fd()) => None,
            None => return Err(libc::EINVAL),
        };
        let size = dmabuf_size(r.fd.as_fd()).ok_or(libc::EIO)?;
        Ok(RmResource {
            dmabuf: r.fd.as_fd(),
            size,
            modifier,
        })
    }

    /// The modifier an RM-export blob was imported with, for tests:
    /// `None` for no such resource, `Some(None)` for an unknown layout.
    #[cfg(test)]
    pub(super) fn rm_modifier(&self, res_id: u32) -> Option<Option<u64>> {
        self.resources.get(&res_id)?.rm.map(|r| r.modifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dma_buf_is_taken_for_one() {
        let m = conduit_venus::mock::memfd(4096).unwrap();
        assert!(!is_dmabuf(m.as_fd()), "a memfd");
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!is_dmabuf(null.as_fd()), "a device node");
    }
}

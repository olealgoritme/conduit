//! Guest-memory blobs (docs/VENUS.md "Guest-memory blobs"): a Venus
//! resource whose memory is the guest's own pages, so that a copy into it on
//! the host GPU lands directly in memory the guest reads.
//!
//! This is for the Windows KMD's windowed Present blt. Its destination is a
//! guest allocation whose content dxgkrnl and DWM read through guest system
//! pages. With such a blob over those pages, the GPU copy writes them
//! directly, and the KMD's CPU mirror (and the fence wait before it) goes.
//!
//! `RESOURCE_CREATE_BLOB { blob_mem = GUEST, blob_id = 0, flags = 0 or
//! USE_SHAREABLE }` carries `nr_entries` `virtio_gpu_mem_entry`s: guest
//! physical addresses and lengths, whole pages, the guest's pin holding them
//! in place. Each entry is resolved through the vhost-user memory table to
//! the guest RAM file and an offset in it. The renderer is sent that file and
//! the runs, maps them as one span of its own, and imports the span as a
//! host-pointer resource (`conduit_venus::Renderer::import_guest_pages`). The
//! resource is attached to the creating context. A Venus context imports it
//! with `VkImportMemoryResourceInfoMESA`, which vkr turns into a
//! `VK_EXT_external_memory_host` import.
//!
//! Only served with `--venus-guest-blobs` (config bit
//! `NVGPU_CFG_GUEST_BLOB`). Without it, `GUEST` is refused as it always was.

use super::*;
use std::os::fd::{AsFd, AsRawFd};

use crate::guestmem::GuestRam;
use conduit_venus::PageRun;

/// What one guest-memory blob counts against the live limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GuestPages {
    /// Page runs after merging runs adjacent in the guest RAM file: host
    /// mappings in the renderer.
    runs: usize,
    bytes: u64,
}

/// Totals over every live guest-memory blob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Live {
    pub blobs: usize,
    pub runs: usize,
    pub bytes: u64,
}

impl Live {
    fn fits(&self, g: &GuestPages) -> bool {
        self.blobs < GUEST_BLOB_MAX_LIVE
            && self.runs + g.runs <= GUEST_BLOB_MAX_LIVE_RUNS
            && self.bytes + g.bytes <= GUEST_BLOB_MAX_LIVE_BYTES
    }

    fn add(&mut self, g: &GuestPages) {
        self.blobs += 1;
        self.runs += g.runs;
        self.bytes += g.bytes;
    }

    pub(super) fn remove(&mut self, g: &GuestPages) {
        self.blobs = self.blobs.saturating_sub(1);
        self.runs = self.runs.saturating_sub(g.runs);
        self.bytes = self.bytes.saturating_sub(g.bytes);
    }
}

/// A refusal: `RESP_ERR_*`, the errno echoed, the counter it is noted under.
type Refusal = (u32, i32, &'static str);

/// `(device, inode)`: which file a descriptor is.
fn identity(fd: BorrowedFd<'_>) -> Option<(u64, u64)> {
    // SAFETY: fstat into a zeroed local on a descriptor we hold.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0).then_some((st.st_dev, st.st_ino))
}

/// The `virtio_gpu_mem_entry`s as `(addr, length)`. `bytes` is exactly
/// `nr_entries` of them (the dispatch checked the length).
fn entries(bytes: &[u8]) -> impl Iterator<Item = (u64, u64)> + '_ {
    bytes
        .chunks_exact(ResourceCreateBlob::MEM_ENTRY_LEN)
        .map(|e| {
            let addr = u64::from_le_bytes(e[0..8].try_into().expect("16-byte entry"));
            let len = u32::from_le_bytes(e[8..12].try_into().expect("16-byte entry"));
            (addr, u64::from(len))
        })
}

/// Resolve guest physical entries to runs of one guest RAM file: the file
/// (this side's own handle) and the runs, merged where they are adjacent in
/// it. Checked here, before the renderer sees anything:
///
/// - whole pages, nonzero;
/// - each entry inside one region of guest RAM, so it cannot reach past a
///   region's end into whatever lies next to it in the file;
/// - every entry in the same file. QEMU backs all of a VM's RAM with one
///   memfd, so this holds unless the VM has several memory backends (NUMA).
pub(super) fn resolve(
    ram: &dyn GuestRam,
    bytes: &[u8],
) -> Result<(OwnedFd, Vec<PageRun>), Refusal> {
    let mut file: Option<(OwnedFd, (u64, u64))> = None;
    let mut runs: Vec<PageRun> = Vec::new();
    for (addr, len) in entries(bytes) {
        if len == 0 || !addr.is_multiple_of(PAGE) || !len.is_multiple_of(PAGE) {
            return Err((
                RESP_ERR_INVALID_PARAMETER,
                libc::EINVAL,
                "refused: guest blob entry shape",
            ));
        }
        let Some(b) = ram.backing(addr) else {
            return Err((
                RESP_ERR_INVALID_PARAMETER,
                libc::EFAULT,
                "refused: guest blob entry not RAM",
            ));
        };
        if len > b.len {
            return Err((
                RESP_ERR_INVALID_PARAMETER,
                libc::EFAULT,
                "refused: guest blob entry crosses a region",
            ));
        }
        let Some(id) = identity(b.fd.as_fd()) else {
            return Err((RESP_ERR_UNSPEC, libc::EIO, "refused: guest blob RAM file"));
        };
        match &file {
            None => file = Some((b.fd, id)),
            Some((_, first)) if *first != id => {
                return Err((
                    RESP_ERR_INVALID_PARAMETER,
                    libc::EXDEV,
                    "refused: guest blob spans RAM files",
                ));
            }
            Some(_) => {}
        }
        match runs.last_mut() {
            Some(last) if last.offset + last.len == b.offset => last.len += len,
            _ => runs.push(PageRun {
                offset: b.offset,
                len,
            }),
        }
    }
    let (fd, _) = file.ok_or((
        RESP_ERR_INVALID_PARAMETER,
        libc::EINVAL,
        "refused: guest blob entry shape",
    ))?;
    Ok((fd, runs))
}

impl Venus {
    /// Serve guest-memory blobs if the renderer can (`--venus-guest-blobs`).
    /// Whether they are served now; the transport sets
    /// [`protocol::messages::NVGPU_CFG_GUEST_BLOB`] on it.
    pub fn enable_guest_blobs(&mut self) -> bool {
        self.guest_blobs =
            self.renderer.features() & conduit_venus::FEATURE_IMPORT_GUEST_PAGES != 0;
        if self.guest_blobs {
            log::info!("venus: guest-memory blobs are served");
        } else {
            log::warn!(
                "venus: guest-memory blobs asked for, but the renderer cannot import host memory"
            );
        }
        self.guest_blobs
    }

    /// Guest-memory blobs are served.
    pub fn guest_blobs(&self) -> bool {
        self.guest_blobs
    }

    fn refuse_guest(&mut self, (resp, errno, why): Refusal) -> Answer {
        self.count(why);
        self.refusal_errno = Some(errno);
        Err(resp)
    }

    pub(super) fn create_guest_blob(
        &mut self,
        c: &ResourceCreateBlob,
        bytes: &[u8],
        env: Env<'_>,
    ) -> Answer {
        let ctx = c.hdr.ctx_id;
        log::debug!(
            "venus: create guest blob res {} ctx {}: flags {:#x} size {} entries {}",
            c.resource_id,
            ctx,
            c.blob_flags,
            c.size,
            c.nr_entries
        );
        if !self.contexts.contains(&ctx) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        if c.resource_id == 0 || self.resources.contains_key(&c.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        // No mapping: the guest has the pages already. SHAREABLE changes
        // nothing (the resource id is what is shared).
        let total: u64 = entries(bytes).map(|(_, l)| l).sum();
        if c.blob_flags & !BLOB_FLAG_USE_SHAREABLE != 0
            || c.blob_id != 0
            || c.nr_entries == 0
            || c.nr_entries > GUEST_BLOB_MAX_ENTRIES
            || c.size == 0
            || !c.size.is_multiple_of(PAGE)
            || c.size > GUEST_BLOB_MAX_BYTES
            || total != c.size
        {
            return self.refuse_guest((
                RESP_ERR_INVALID_PARAMETER,
                libc::EINVAL,
                "refused: guest blob shape",
            ));
        }
        let Some(ram) = env.ram else {
            log::warn!(
                "venus: guest blob res {}: guest RAM is not known yet",
                c.resource_id
            );
            return self.refuse_guest((
                RESP_ERR_UNSPEC,
                libc::EOPNOTSUPP,
                "refused: guest blob no RAM",
            ));
        };
        let (fd, runs) = match resolve(ram, bytes) {
            Ok(r) => r,
            Err(r) => {
                log::warn!("venus: guest blob res {}: {}", c.resource_id, r.2);
                return self.refuse_guest(r);
            }
        };
        let pages = GuestPages {
            runs: runs.len(),
            bytes: c.size,
        };
        if self.resources.len() >= MAX_RESOURCES || !self.guest_live.fits(&pages) {
            log::warn!(
                "venus: guest blob res {} ({} runs, {} bytes) refused: {} blobs, {} runs, {} bytes live",
                c.resource_id,
                pages.runs,
                pages.bytes,
                self.guest_live.blobs,
                self.guest_live.runs,
                self.guest_live.bytes
            );
            return self.refuse_guest((
                RESP_ERR_OUT_OF_MEMORY,
                libc::ENOMEM,
                "refused: guest blob limits",
            ));
        }
        if let Err(e) = self
            .renderer
            .import_guest_pages(c.resource_id, fd.as_fd(), &runs)
        {
            log::warn!("venus: importing guest blob res {}: {e}", c.resource_id);
            let resp = self.renderer_error(&e);
            return self.refuse_guest((resp, libc::EIO, "refused: guest blob renderer import"));
        }
        // Imported resources belong to no context until attached; the
        // guest's own CTX_ATTACH_RESOURCE that follows is then a no-op.
        if let Err(e) = self.renderer.ctx_attach(ctx, c.resource_id) {
            log::warn!(
                "venus: attaching guest blob res {} to ctx {ctx}: {e}",
                c.resource_id
            );
            self.renderer.unref(c.resource_id);
            let resp = self.renderer_error(&e);
            return self.refuse_guest((resp, libc::EIO, "refused: guest blob attach"));
        }
        self.count("guest_blob");
        self.guest_live.add(&pages);
        self.resources.insert(
            c.resource_id,
            Resource {
                ctx_id: ctx,
                size: c.size,
                flags: c.blob_flags,
                map_info: 0,
                fd,
                mapped: None,
                attached: HashSet::from([ctx]),
                export: None,
                rm: None,
                guest: Some(pages),
            },
        );
        Ok(Reply::NoData)
    }

    /// What live guest-memory blobs hold, for tests.
    #[cfg(test)]
    pub(super) fn guest_live(&self) -> Live {
        self.guest_live
    }
}

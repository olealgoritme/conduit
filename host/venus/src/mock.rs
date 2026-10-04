//! An in-memory [`Renderer`] for tests: blobs are memfds, fences signal at
//! once, scanouts export a memfd.

use super::*;
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, FromRawFd};

#[derive(Default)]
pub struct Mock {
    pub contexts: HashSet<u32>,
    pub resources: HashMap<u32, u64>,
    pub submitted: Vec<(u32, Vec<u8>)>,
    pending: Vec<Signalled>,
    event: Option<OwnedFd>,
}

impl Mock {
    pub fn new() -> Self {
        // SAFETY: eventfd returns a new descriptor or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(fd >= 0, "eventfd");
        // SAFETY: fd is a fresh descriptor we own.
        Self { event: Some(unsafe { OwnedFd::from_raw_fd(fd) }), ..Default::default() }
    }
}

fn memfd(size: u64) -> Result<OwnedFd> {
    // SAFETY: plain syscalls on a fresh descriptor.
    unsafe {
        let fd = libc::memfd_create(c"conduit-venus-mock".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        use std::os::fd::AsRawFd;
        if libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(fd)
    }
}

impl Renderer for Mock {
    fn capset_info(&mut self, index: u32) -> Result<CapsetInfo> {
        match index {
            0 => Ok(CapsetInfo { id: CAPSET_VENUS, max_version: 0, max_size: 160 }),
            _ => Err(Error::Refused("capset index".into())),
        }
    }
    fn capset(&mut self, id: u32, _version: u32) -> Result<Vec<u8>> {
        if id != CAPSET_VENUS {
            return Err(Error::Refused("capset id".into()));
        }
        Ok(vec![0; 160])
    }
    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, _name: &[u8]) -> Result<()> {
        if capset_id != CAPSET_VENUS {
            return Err(Error::Refused("capset id".into()));
        }
        self.contexts.insert(ctx_id);
        Ok(())
    }
    fn ctx_destroy(&mut self, ctx_id: u32) {
        self.contexts.remove(&ctx_id);
    }
    fn ctx_attach(&mut self, ctx_id: u32, res_id: u32) -> Result<()> {
        if !self.contexts.contains(&ctx_id) {
            return Err(Error::NoContext(ctx_id));
        }
        if !self.resources.contains_key(&res_id) {
            return Err(Error::NoResource(res_id));
        }
        Ok(())
    }
    fn ctx_detach(&mut self, _ctx_id: u32, _res_id: u32) {}
    fn submit(&mut self, ctx_id: u32, commands: &[u8]) -> Result<()> {
        if !self.contexts.contains(&ctx_id) {
            return Err(Error::NoContext(ctx_id));
        }
        self.submitted.push((ctx_id, commands.to_vec()));
        Ok(())
    }
    fn create_blob(&mut self, ctx_id: u32, res_id: u32, _blob_id: u64, size: u64, _flags: u32) -> Result<Blob> {
        if !self.contexts.contains(&ctx_id) {
            return Err(Error::NoContext(ctx_id));
        }
        let fd = memfd(size)?;
        self.resources.insert(res_id, size);
        Ok(Blob { fd, map_info: 0x01 /* VIRTIO_GPU_MAP_CACHE_CACHED */, size })
    }
    fn unref(&mut self, res_id: u32) {
        self.resources.remove(&res_id);
    }
    fn create_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<()> {
        self.pending.push(Signalled { ctx_id, ring_idx, fence_id });
        Ok(())
    }
    fn fence_fd(&self) -> BorrowedFd<'_> {
        self.event.as_ref().expect("Mock::new").as_fd()
    }
    fn signalled(&mut self) -> Result<Vec<Signalled>> {
        Ok(std::mem::take(&mut self.pending))
    }
    /// The image as `layout` says, in a memfd just big enough for it.
    fn export_scanout(&mut self, res_id: u32, layout: ScanoutLayout) -> Result<Dmabuf> {
        if !self.resources.contains_key(&res_id) {
            return Err(Error::NoResource(res_id));
        }
        let size = u64::from(layout.offset) + u64::from(layout.stride) * u64::from(layout.height);
        Ok(Dmabuf {
            fd: memfd(size)?,
            width: layout.width,
            height: layout.height,
            stride: layout.stride,
            offset: layout.offset,
            fourcc: layout.fourcc,
            modifier: DRM_FORMAT_MOD_LINEAR,
        })
    }
}

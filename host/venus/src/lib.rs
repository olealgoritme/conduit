//! The Venus renderer as conduit-backend sees it (docs/VENUS.md).
//!
//! The backend validates a guest's virtio-gpu commands and then calls a
//! [`Renderer`]. In production that is the IPC client talking to the
//! `conduit-venus` process; in tests it is [`mock::Mock`].

use std::os::fd::{BorrowedFd, OwnedFd};

pub mod ipc;
pub mod mock;
pub mod sandbox;
#[cfg(feature = "renderer")]
pub mod virgl;

/// `VIRTIO_GPU_CAPSET_VENUS`.
pub const CAPSET_VENUS: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("renderer refused: {0}")]
    Refused(String),
    #[error("unknown context {0}")]
    NoContext(u32),
    #[error("unknown resource {0}")]
    NoResource(u32),
    #[error("renderer is gone")]
    Disconnected,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapsetInfo {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}

/// A host-visible blob: memory the guest maps through region 3.
#[derive(Debug)]
pub struct Blob {
    /// Mappable descriptor for the blob's memory, offset 0.
    pub fd: OwnedFd,
    /// `VIRTIO_GPU_MAP_CACHE_*` for `RESP_OK_MAP_INFO`.
    pub map_info: u32,
    pub size: u64,
}

/// `DRM_FORMAT_MOD_LINEAR`, the only modifier a Venus scanout has for now.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// A scanout image's layout, as the guest gave it in `SET_SCANOUT_BLOB`
/// (`width`, `height`, `strides[0]`, `offsets[0]`), with the virtio-gpu
/// format already mapped to its `DRM_FORMAT_*` by the backend.
///
/// There is no modifier: a Venus blob carries no image layout the host could
/// read back (virglrenderer's export query knows nothing of it), so the only
/// layout both sides can agree on without one is linear. Venus scanout
/// images must be linear for now: guest drivers must allocate scanout images
/// with `VK_IMAGE_TILING_LINEAR` (or an explicit `DRM_FORMAT_MOD_LINEAR`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanoutLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of plane 0.
    pub stride: u32,
    /// Where plane 0 starts in the blob.
    pub offset: u32,
    /// `DRM_FORMAT_*`.
    pub fourcc: u32,
}

/// A scanout resource exported for the viewer.
#[derive(Debug)]
pub struct Dmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    /// `DRM_FORMAT_*`.
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*`.
    pub modifier: u64,
}

/// A fence the renderer has signalled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signalled {
    pub ctx_id: u32,
    pub ring_idx: u32,
    pub fence_id: u64,
}

/// What the backend asks of the renderer. Ids are the guest's, already
/// checked by the backend (existence, ownership, uniqueness).
pub trait Renderer: Send {
    fn capset_info(&mut self, index: u32) -> Result<CapsetInfo>;
    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>>;

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, debug_name: &[u8]) -> Result<()>;
    fn ctx_destroy(&mut self, ctx_id: u32);
    fn ctx_attach(&mut self, ctx_id: u32, res_id: u32) -> Result<()>;
    fn ctx_detach(&mut self, ctx_id: u32, res_id: u32);

    /// A Venus command stream for `ctx_id`.
    fn submit(&mut self, ctx_id: u32, commands: &[u8]) -> Result<()>;

    /// `RESOURCE_CREATE_BLOB` with `blob_mem = HOST3D`.
    fn create_blob(&mut self, ctx_id: u32, res_id: u32, blob_id: u64, size: u64, flags: u32) -> Result<Blob>;
    fn unref(&mut self, res_id: u32);

    /// Ask for `fence_id` on `(ctx_id, ring_idx)`; it shows up in
    /// [`Renderer::signalled`] once the GPU work before it is done.
    fn create_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<()>;
    /// Readable when [`Renderer::signalled`] has something.
    fn fence_fd(&self) -> BorrowedFd<'_>;
    /// Drain signalled fences. [`Error::Disconnected`] once the renderer is
    /// gone (after any fences it signalled before going): its fences will
    /// never signal, so whoever waits on them must give up.
    fn signalled(&mut self) -> Result<Vec<Signalled>>;

    /// Export `res_id` for scanout. The returned [`Dmabuf`] describes the
    /// image with `layout`'s size, stride, offset and format and
    /// [`DRM_FORMAT_MOD_LINEAR`] (see [`ScanoutLayout`]).
    fn export_scanout(&mut self, res_id: u32, layout: ScanoutLayout) -> Result<Dmabuf>;
}

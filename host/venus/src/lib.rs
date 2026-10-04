//! The Venus renderer as conduit-backend sees it (docs/VENUS.md).
//!
//! The backend validates a guest's virtio-gpu commands and then calls a
//! [`Renderer`]. In production that is the IPC client talking to the
//! `conduit-venus` process; in tests it is [`mock::Mock`].

use std::os::fd::{BorrowedFd, OwnedFd};

pub mod mock;

/// `VIRTIO_GPU_CAPSET_VENUS`.
pub const CAPSET_VENUS: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("renderer refused: {0}")]
    Refused(&'static str),
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
    /// Drain signalled fences.
    fn signalled(&mut self) -> Vec<Signalled>;

    /// Export `res_id` for scanout.
    fn export_scanout(&mut self, res_id: u32, width: u32, height: u32) -> Result<Dmabuf>;
}

//! The Venus renderer as conduit-backend sees it (docs/VENUS.md).
//!
//! The backend validates a guest's virtio-gpu commands and then calls a
//! [`Renderer`]. In production that is the IPC client talking to the
//! `conduit-venus` process; in tests it is [`mock::Mock`].

use std::os::fd::{BorrowedFd, OwnedFd};

pub mod affinity;
pub mod guest_pages;
pub mod ipc;
pub mod latency;
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

/// `DRM_FORMAT_MOD_LINEAR`, the only modifier the renderer reports.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// A scanout image's layout, as the guest gave it in `SET_SCANOUT_BLOB`
/// (`width`, `height`, `strides[0]`, `offsets[0]`), with the virtio-gpu
/// format already mapped to its `DRM_FORMAT_*` by the backend.
///
/// There is no modifier: a Venus blob carries no image layout the host could
/// read back (virglrenderer's export query knows nothing of it), so the
/// renderer reports linear. The backend, which knows the blob's size, infers
/// the modifier the viewer is told (block-linear for a blob padded the way an
/// optimal-tiling image is; docs/VENUS.md "Scanout layout").
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

/// [`Renderer::features`]: the renderer can make a resource from a dma-buf
/// the backend hands it ([`Renderer::import_dmabuf`]).
pub const FEATURE_IMPORT_DMABUF: u32 = 1 << 0;

/// [`Renderer::features`]: the renderer can make a resource of guest pages
/// ([`Renderer::import_guest_pages`]) that Venus contexts import with
/// `VK_EXT_external_memory_host` (docs/VENUS.md "Guest-memory blobs").
pub const FEATURE_IMPORT_GUEST_PAGES: u32 = 1 << 1;

/// [`Renderer::features`]: a submit and the fence after it travel as one
/// call ([`Renderer::submit_fenced`]), one round trip instead of two. The
/// IPC server adds this bit itself, whatever its renderer: any renderer
/// serves the call through the trait's default.
pub const FEATURE_SUBMIT_FENCED: u32 = 1 << 3;

/// Why [`Renderer::submit_fenced`] failed: the submit (no fence was asked
/// for), or the fence after a submit that went through.
#[derive(Debug)]
pub enum FencedError {
    Submit(Error),
    Fence(Error),
}

/// Called by an IPC client's reader thread right after it queued signalled
/// fences ([`Renderer::set_fence_hook`]). Returns whether it took them; on
/// `false` the client wakes [`Renderer::fence_fd`] as it does without a hook.
pub type FenceHook = Box<dyn Fn() -> bool + Send + Sync>;

/// Called by a renderer on the thread that retires a fence, with that fence,
/// instead of queueing it for [`Renderer::signalled`]
/// ([`Renderer::set_fence_sink`]).
pub type FenceSink = Box<dyn Fn(Signalled) + Send + Sync>;

/// One run of guest pages for [`Renderer::import_guest_pages`]: `len` bytes
/// at `offset` of the guest RAM file, both whole pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRun {
    pub offset: u64,
    pub len: u64,
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

    /// [`Renderer::submit`] and then, if it went through,
    /// [`Renderer::create_fence`]: what a fenced `SUBMIT_3D` asks for. The
    /// IPC client sends both as one message when the server has
    /// [`FEATURE_SUBMIT_FENCED`].
    fn submit_fenced(
        &mut self,
        ctx_id: u32,
        commands: &[u8],
        ring_idx: u32,
        fence_id: u64,
    ) -> std::result::Result<(), FencedError> {
        self.submit(ctx_id, commands).map_err(FencedError::Submit)?;
        self.create_fence(ctx_id, ring_idx, fence_id).map_err(FencedError::Fence)
    }

    /// Run `hook` as soon as signalled fences arrive, on whatever thread
    /// receives them, instead of only waking [`Renderer::fence_fd`]. Returns
    /// whether the renderer can (the IPC client can; others say no and keep
    /// the descriptor as the only wakeup).
    fn set_fence_hook(&mut self, hook: FenceHook) -> bool {
        let _ = hook;
        false
    }

    /// Hand every fence signalled from now on to `sink`, on the thread that
    /// retires it, instead of queueing it for [`Renderer::signalled`].
    /// Returns whether the renderer can; one that cannot keeps queueing.
    fn set_fence_sink(&mut self, sink: FenceSink) -> bool {
        let _ = sink;
        false
    }
    /// Readable when [`Renderer::signalled`] has something.
    fn fence_fd(&self) -> BorrowedFd<'_>;
    /// Drain signalled fences. [`Error::Disconnected`] once the renderer is
    /// gone (after any fences it signalled before going): its fences will
    /// never signal, so whoever waits on them must give up.
    fn signalled(&mut self) -> Result<Vec<Signalled>>;

    /// Upkeep between requests (closing a fence latency window on time):
    /// the serve loop calls it before each wait and wakes again within the
    /// returned time, or only for the next request or fence on `None`.
    fn tick(&mut self) -> Option<std::time::Duration> {
        None
    }

    /// Export `res_id` for scanout. The returned [`Dmabuf`] describes the
    /// image with `layout`'s size, stride, offset and format and
    /// [`DRM_FORMAT_MOD_LINEAR`] (see [`ScanoutLayout`]).
    fn export_scanout(&mut self, res_id: u32, layout: ScanoutLayout) -> Result<Dmabuf>;

    /// What this renderer can do beyond the calls every renderer serves:
    /// `FEATURE_*` bits. None by default.
    fn features(&mut self) -> u32 {
        0
    }

    /// Make `res_id` a resource whose memory is the dma-buf `fd`, `size`
    /// bytes of it (docs/VENUS.md "RM-export blobs"). The resource is
    /// attached to no context; the backend attaches it. Contexts import it
    /// as `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` memory through
    /// `VkImportMemoryResourceInfoMESA`. The renderer keeps its own
    /// duplicate of `fd`. Only with [`FEATURE_IMPORT_DMABUF`].
    fn import_dmabuf(&mut self, res_id: u32, fd: BorrowedFd<'_>, size: u64) -> Result<()> {
        let _ = (res_id, fd, size);
        Err(Error::Refused("this renderer cannot import a dma-buf".into()))
    }

    /// Make `res_id` a resource whose memory is `runs` of the guest RAM file
    /// `ram`, in order (docs/VENUS.md "Guest-memory blobs"). The renderer maps
    /// them as one span of its own and keeps that mapping until
    /// [`Renderer::unref`]. The resource is attached to no context; the
    /// backend attaches it. Contexts import it through
    /// `VkImportMemoryResourceInfoMESA`, which vkr turns into a
    /// `VK_EXT_external_memory_host` import of the span. Only with
    /// [`FEATURE_IMPORT_GUEST_PAGES`].
    fn import_guest_pages(&mut self, res_id: u32, ram: BorrowedFd<'_>, runs: &[PageRun]) -> Result<()> {
        let _ = (res_id, ram, runs);
        Err(Error::Refused("this renderer cannot import guest pages".into()))
    }
}

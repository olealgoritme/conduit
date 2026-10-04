//! [`Renderer`] on virglrenderer's Venus (feature `renderer`).
//!
//! virglrenderer runs Venus only behind its "render server" (the `proxy`
//! layer). It is built here with `render-server-mode=thread` (see
//! build-virglrenderer.sh), so that server is a thread of this process: no
//! `virgl_render_server` binary, no fork. conduit-venus is already the
//! separate process the backend sandboxes around, so a second process
//! boundary would buy nothing.
//!
//! virglrenderer keeps its state in globals, so there is at most one
//! [`Virgl`] per process. Its API is not thread-safe except for the fence
//! callback: with `THREAD_SYNC | ASYNC_FENCE_CB` each Venus context gets a
//! sync thread that calls `write_context_fence` as fences retire. That
//! callback only queues into [`FENCES`] and signals an eventfd, so the serving
//! thread never has to poll virglrenderer for fences.

use crate::{Blob, CAPSET_VENUS, CapsetInfo, Dmabuf, Error, Renderer, Result, Signalled};
use std::ffi::{c_char, c_int, c_void};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

mod ffi {
    use super::*;

    pub const VIRGL_RENDERER_THREAD_SYNC: c_int = 1 << 1;
    pub const VIRGL_RENDERER_VENUS: c_int = 1 << 6;
    pub const VIRGL_RENDERER_NO_VIRGL: c_int = 1 << 7;
    pub const VIRGL_RENDERER_ASYNC_FENCE_CB: c_int = 1 << 8;
    pub const VIRGL_RENDERER_RENDER_SERVER: c_int = 1 << 9;

    pub const VIRGL_RENDERER_CONTEXT_FLAG_CAPSET_ID_MASK: u32 = 0xff;

    pub const VIRGL_RENDERER_BLOB_MEM_HOST3D: u32 = 0x0002;
    pub const VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;

    pub const VIRGL_RENDERER_MAP_CACHE_NONE: u32 = 0x00;

    pub const VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF: u32 = 0x0001;
    pub const VIRGL_RENDERER_BLOB_FD_TYPE_OPAQUE: u32 = 0x0002;

    pub const VIRGL_RENDERER_STRUCTURE_TYPE_EXPORT_QUERY: u32 = 1 << 0;

    /// Up to `get_egl_display` (v4). We declare v3 so the v4 field is never
    /// read; it is here only so the layout matches the header's.
    #[repr(C)]
    pub struct Callbacks {
        pub version: c_int,
        pub write_fence: Option<extern "C" fn(*mut c_void, u32)>,
        pub create_gl_context: *const c_void,
        pub destroy_gl_context: *const c_void,
        pub make_current: *const c_void,
        pub get_drm_fd: *const c_void,
        pub write_context_fence: Option<extern "C" fn(*mut c_void, u32, u32, u64)>,
        pub get_server_fd: *const c_void,
        pub get_egl_display: *const c_void,
    }
    // SAFETY: only function pointers and nulls, never mutated.
    unsafe impl Sync for Callbacks {}

    #[repr(C)]
    pub struct CreateBlobArgs {
        pub res_handle: u32,
        pub ctx_id: u32,
        pub blob_mem: u32,
        pub blob_flags: u32,
        pub blob_id: u64,
        pub size: u64,
        pub iovecs: *const libc::iovec,
        pub num_iovs: u32,
    }

    #[repr(C)]
    pub struct Hdr {
        pub stype: u32,
        pub stype_version: u32,
        pub size: u32,
    }

    #[repr(C)]
    pub struct ExportQuery {
        pub hdr: Hdr,
        pub in_resource_id: u32,
        pub out_num_fds: u32,
        pub in_export_fds: u32,
        pub out_fourcc: u32,
        pub pad: u32,
        pub out_fds: [i32; 4],
        pub out_strides: [u32; 4],
        pub out_offsets: [u32; 4],
        pub out_modifier: u64,
    }

    unsafe extern "C" {
        pub fn virgl_renderer_init(cookie: *mut c_void, flags: c_int, cb: *const Callbacks) -> c_int;
        pub fn virgl_renderer_cleanup(cookie: *mut c_void);
        pub fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);
        pub fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);
        pub fn virgl_renderer_context_create_with_flags(
            ctx_id: u32,
            ctx_flags: u32,
            nlen: u32,
            name: *const c_char,
        ) -> c_int;
        pub fn virgl_renderer_context_destroy(handle: u32);
        pub fn virgl_renderer_ctx_attach_resource(ctx_id: c_int, res_handle: c_int);
        pub fn virgl_renderer_ctx_detach_resource(ctx_id: c_int, res_handle: c_int);
        pub fn virgl_renderer_submit_cmd(buffer: *mut c_void, ctx_id: c_int, ndw: c_int) -> c_int;
        pub fn virgl_renderer_resource_create_blob(args: *const CreateBlobArgs) -> c_int;
        pub fn virgl_renderer_resource_export_blob(res_id: u32, fd_type: *mut u32, fd: *mut c_int) -> c_int;
        pub fn virgl_renderer_resource_get_map_info(res_handle: u32, map_info: *mut u32) -> c_int;
        pub fn virgl_renderer_resource_unref(res_handle: u32);
        pub fn virgl_renderer_context_create_fence(ctx_id: u32, flags: u32, ring_idx: u32, fence_id: u64) -> c_int;
        pub fn virgl_renderer_execute(args: *mut c_void, size: u32) -> c_int;
    }
}

const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Fences retired by virglrenderer's sync threads, waiting for
/// [`Renderer::signalled`]. Global because the callback gets only the
/// init-time cookie and virglrenderer is a per-process singleton anyway.
static FENCES: Mutex<Vec<Signalled>> = Mutex::new(Vec::new());
static EVENT: AtomicI32 = AtomicI32::new(-1);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {
    // Context-0 fences belong to vrend, which is not built.
}

extern "C" fn write_context_fence(_cookie: *mut c_void, ctx_id: u32, ring_idx: u32, fence_id: u64) {
    // Runs on a virglrenderer thread: no panics across the FFI boundary, so a
    // poisoned lock is used as is.
    FENCES.lock().unwrap_or_else(|p| p.into_inner()).push(Signalled { ctx_id, ring_idx, fence_id });
    let fd = EVENT.load(Ordering::Acquire);
    if fd >= 0 {
        let one: u64 = 1;
        // SAFETY: an 8-byte write from a local to the eventfd, which lives
        // until after virgl_renderer_cleanup has joined the sync threads.
        unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
    }
}

static CALLBACKS: ffi::Callbacks = ffi::Callbacks {
    version: 3,
    write_fence: Some(write_fence),
    create_gl_context: std::ptr::null(),
    destroy_gl_context: std::ptr::null(),
    make_current: std::ptr::null(),
    get_drm_fd: std::ptr::null(),
    write_context_fence: Some(write_context_fence),
    // Null: the render server is an in-process thread, nothing to hand over.
    get_server_fd: std::ptr::null(),
    get_egl_display: std::ptr::null(),
};

/// virglrenderer returns `-errno` from some calls and a positive `EINVAL`
/// from others; either way nonzero is failure.
fn check(ret: c_int) -> Result<()> {
    if ret == 0 { Ok(()) } else { Err(Error::Io(io::Error::from_raw_os_error(ret.abs()))) }
}

pub struct Virgl {
    event: OwnedFd,
}

impl Virgl {
    /// Initialize virglrenderer for Venus. Fails if one already exists in
    /// this process, or if Venus cannot start (no Vulkan driver, no Venus in
    /// the build).
    pub fn new() -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return Err(Error::Refused("virglrenderer already initialized"));
        }
        // SAFETY: eventfd returns a new descriptor or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            INITIALIZED.store(false, Ordering::Release);
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: fd is fresh and ours.
        let event = unsafe { OwnedFd::from_raw_fd(fd) };
        EVENT.store(event.as_raw_fd(), Ordering::Release);

        // VENUS | NO_VIRGL | RENDER_SERVER is how QEMU and crosvm start a
        // Venus-only renderer. THREAD_SYNC | ASYNC_FENCE_CB: fences arrive by
        // callback from per-context threads (module comment).
        let flags = ffi::VIRGL_RENDERER_VENUS
            | ffi::VIRGL_RENDERER_NO_VIRGL
            | ffi::VIRGL_RENDERER_RENDER_SERVER
            | ffi::VIRGL_RENDERER_THREAD_SYNC
            | ffi::VIRGL_RENDERER_ASYNC_FENCE_CB;
        // SAFETY: CALLBACKS is 'static as virglrenderer keeps the pointer.
        let ret = unsafe { ffi::virgl_renderer_init(std::ptr::null_mut(), flags, &CALLBACKS) };
        if ret != 0 {
            EVENT.store(-1, Ordering::Release);
            INITIALIZED.store(false, Ordering::Release);
            return Err(Error::Io(io::Error::from_raw_os_error(ret.abs())));
        }
        let me = Self { event };
        // Venus registers its capset only when the render server came up and
        // found a Vulkan driver; without it every context would be refused.
        if me.venus_caps().1 == 0 {
            return Err(Error::Refused("virglrenderer has no Venus capset (render server or Vulkan failed)"));
        }
        Ok(me)
    }

    fn venus_caps(&self) -> (u32, u32) {
        let (mut ver, mut size) = (0, 0);
        // SAFETY: two out-params on the stack.
        unsafe { ffi::virgl_renderer_get_cap_set(CAPSET_VENUS, &mut ver, &mut size) };
        (ver, size)
    }

    fn export(&self, res_id: u32) -> Result<(u32, OwnedFd)> {
        let (mut ty, mut fd) = (0u32, -1);
        // SAFETY: out-params on the stack; on success fd is a new descriptor.
        check(unsafe { ffi::virgl_renderer_resource_export_blob(res_id, &mut ty, &mut fd) })?;
        if fd < 0 {
            return Err(Error::Refused("export_blob returned no fd"));
        }
        // SAFETY: virglrenderer handed us ownership of a dup.
        Ok((ty, unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

impl Drop for Virgl {
    fn drop(&mut self) {
        // Destroys every context, which joins their sync threads, so no
        // callback can touch EVENT after this.
        // SAFETY: we are the only user of the global renderer.
        unsafe { ffi::virgl_renderer_cleanup(std::ptr::null_mut()) };
        EVENT.store(-1, Ordering::Release);
        FENCES.lock().unwrap_or_else(|p| p.into_inner()).clear();
        INITIALIZED.store(false, Ordering::Release);
    }
}

/// Whether `fd` can back a guest mapping. NVIDIA's `OPAQUE_FD` exports are
/// driver handles, not mmap-able memory; a mappable blob that came out that
/// way could not be placed in region 3, so it is refused up front instead of
/// failing in the backend.
fn mmappable(fd: BorrowedFd<'_>) -> bool {
    // SAFETY: a one-page read-only probe mapping, unmapped right away.
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return false;
        }
        libc::munmap(p, 4096);
        true
    }
}

impl Renderer for Virgl {
    fn capset_info(&mut self, index: u32) -> Result<CapsetInfo> {
        if index != 0 {
            return Err(Error::Refused("capset index"));
        }
        let (max_version, max_size) = self.venus_caps();
        Ok(CapsetInfo { id: CAPSET_VENUS, max_version, max_size })
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>> {
        if id != CAPSET_VENUS {
            return Err(Error::Refused("capset id"));
        }
        let (max_ver, size) = self.venus_caps();
        if version > max_ver {
            return Err(Error::Refused("capset version"));
        }
        let mut caps = vec![0u8; size as usize];
        // SAFETY: caps is max_size bytes, which is what fill_caps writes.
        unsafe { ffi::virgl_renderer_fill_caps(id, version, caps.as_mut_ptr().cast()) };
        Ok(caps)
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, debug_name: &[u8]) -> Result<()> {
        if capset_id != CAPSET_VENUS {
            return Err(Error::Refused("capset id"));
        }
        // virtio-gpu caps the name at 64 bytes; the backend has checked, but
        // the length goes to C as u32.
        let name = &debug_name[..debug_name.len().min(64)];
        let flags = capset_id & ffi::VIRGL_RENDERER_CONTEXT_FLAG_CAPSET_ID_MASK;
        // SAFETY: name/len describe a live slice; virglrenderer copies it.
        check(unsafe {
            ffi::virgl_renderer_context_create_with_flags(ctx_id, flags, name.len() as u32, name.as_ptr().cast())
        })
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        // SAFETY: unknown ids are ignored by virglrenderer.
        unsafe { ffi::virgl_renderer_context_destroy(ctx_id) };
    }

    fn ctx_attach(&mut self, ctx_id: u32, res_id: u32) -> Result<()> {
        // The C call is void: unknown ids are ignored, which matches the
        // backend having checked them already.
        // SAFETY: plain ids.
        unsafe { ffi::virgl_renderer_ctx_attach_resource(ctx_id as c_int, res_id as c_int) };
        Ok(())
    }

    fn ctx_detach(&mut self, ctx_id: u32, res_id: u32) {
        // SAFETY: plain ids.
        unsafe { ffi::virgl_renderer_ctx_detach_resource(ctx_id as c_int, res_id as c_int) };
    }

    fn submit(&mut self, ctx_id: u32, commands: &[u8]) -> Result<()> {
        if !commands.len().is_multiple_of(4) {
            return Err(Error::Refused("submit length not a multiple of 4"));
        }
        // Copied into u64s: virglrenderer wants 4-byte alignment and copies
        // again internally below 8, and IPC hands us an unaligned slice.
        let mut buf = vec![0u64; commands.len().div_ceil(8)];
        // SAFETY: buf has at least commands.len() bytes.
        unsafe { std::ptr::copy_nonoverlapping(commands.as_ptr(), buf.as_mut_ptr().cast::<u8>(), commands.len()) };
        // SAFETY: buf is aligned and ndw words long; virglrenderer only reads.
        check(unsafe {
            ffi::virgl_renderer_submit_cmd(buf.as_mut_ptr().cast(), ctx_id as c_int, (commands.len() / 4) as c_int)
        })
    }

    fn create_blob(&mut self, ctx_id: u32, res_id: u32, blob_id: u64, size: u64, flags: u32) -> Result<Blob> {
        let args = ffi::CreateBlobArgs {
            res_handle: res_id,
            ctx_id,
            blob_mem: ffi::VIRGL_RENDERER_BLOB_MEM_HOST3D,
            blob_flags: flags,
            blob_id,
            size,
            iovecs: std::ptr::null(),
            num_iovs: 0,
        };
        // SAFETY: args is a valid struct for the call.
        check(unsafe { ffi::virgl_renderer_resource_create_blob(&args) })?;

        let made = (|| {
            let (ty, fd) = self.export(res_id)?;
            let mappable = flags & ffi::VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE != 0;
            let mut map_info = ffi::VIRGL_RENDERER_MAP_CACHE_NONE;
            if mappable {
                if ty == ffi::VIRGL_RENDERER_BLOB_FD_TYPE_OPAQUE && !mmappable(fd.as_fd()) {
                    return Err(Error::Refused("mappable blob exported as an opaque fd that cannot be mmapped"));
                }
                // SAFETY: out-param on the stack.
                check(unsafe { ffi::virgl_renderer_resource_get_map_info(res_id, &mut map_info) })?;
            }
            Ok(Blob { fd, map_info, size })
        })();
        if made.is_err() {
            // SAFETY: the resource we just created.
            unsafe { ffi::virgl_renderer_resource_unref(res_id) };
        }
        made
    }

    fn unref(&mut self, res_id: u32) {
        // SAFETY: unknown ids are ignored.
        unsafe { ffi::virgl_renderer_resource_unref(res_id) };
    }

    fn create_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<()> {
        // Not MERGEABLE: every fenced guest command holds a descriptor chain
        // that only this fence's callback releases.
        // SAFETY: plain values.
        check(unsafe { ffi::virgl_renderer_context_create_fence(ctx_id, 0, ring_idx, fence_id) })
    }

    fn fence_fd(&self) -> BorrowedFd<'_> {
        self.event.as_fd()
    }

    fn signalled(&mut self) -> Vec<Signalled> {
        let mut v: u64 = 0;
        // SAFETY: 8-byte read into a local; EAGAIN when nothing is pending.
        // Reset before taking so a fence pushed after the take re-signals.
        unsafe { libc::read(self.event.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
        // No virgl_renderer_poll here: with ASYNC_FENCE_CB the sync threads
        // own fence retirement, and polling from this thread too would race
        // them (proxy_context_retire_fences asserts against it).
        std::mem::take(&mut *FENCES.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The pinned virglrenderer knows nothing of a Venus blob's image layout:
    /// its export query answers fourcc 0, stride 0, `DRM_FORMAT_MOD_INVALID`
    /// for any untyped (blob) resource. Those are passed through when real,
    /// and otherwise filled with the layout Venus scanouts use in practice
    /// (XRGB8888, `width * 4`, linear), which the backend must overwrite
    /// with what the guest's `SET_SCANOUT_BLOB` said.
    fn export_scanout(&mut self, res_id: u32, width: u32, height: u32) -> Result<Dmabuf> {
        let (ty, fd) = self.export(res_id)?;
        if ty != ffi::VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF {
            return Err(Error::Refused("scanout resource does not export as a dma-buf"));
        }
        let mut q = ffi::ExportQuery {
            hdr: ffi::Hdr {
                stype: ffi::VIRGL_RENDERER_STRUCTURE_TYPE_EXPORT_QUERY,
                stype_version: 0,
                size: size_of::<ffi::ExportQuery>() as u32,
            },
            in_resource_id: res_id,
            out_num_fds: 0,
            in_export_fds: 0,
            out_fourcc: 0,
            pad: 0,
            out_fds: [-1; 4],
            out_strides: [0; 4],
            out_offsets: [0; 4],
            out_modifier: DRM_FORMAT_MOD_INVALID,
        };
        // SAFETY: q is a correctly sized export query; with in_export_fds = 0
        // no fds are created.
        let queried = unsafe { ffi::virgl_renderer_execute((&mut q as *mut ffi::ExportQuery).cast(), q.hdr.size) } == 0;
        let known = |v: u32| queried && v != 0;
        Ok(Dmabuf {
            fd,
            width,
            height,
            stride: if known(q.out_strides[0]) { q.out_strides[0] } else { width * 4 },
            offset: if queried { q.out_offsets[0] } else { 0 },
            fourcc: if known(q.out_fourcc) { q.out_fourcc } else { DRM_FORMAT_XRGB8888 },
            modifier: if queried && q.out_modifier != DRM_FORMAT_MOD_INVALID {
                q.out_modifier
            } else {
                DRM_FORMAT_MOD_LINEAR
            },
        })
    }
}

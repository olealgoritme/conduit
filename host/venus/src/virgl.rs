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

use crate::latency;
use crate::stage::{self, Rec};
use crate::{
    Blob, CAPSET_VENUS, CapsetInfo, DRM_FORMAT_MOD_LINEAR, Dmabuf, Error, Renderer, Result, ScanoutLayout, Signalled,
};
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
    pub struct ImportBlobArgs {
        pub res_handle: u32,
        pub blob_mem: u32,
        pub fd_type: u32,
        pub fd: c_int,
        pub size: u64,
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
        pub fn virgl_renderer_resource_import_blob(args: *const ImportBlobArgs) -> c_int;
        /// patches/0002-vkr-host-pointer-resources.patch
        pub fn virgl_renderer_resource_import_host_ptr(res_handle: u32, ptr: *mut c_void, size: u64) -> c_int;
        pub fn virgl_renderer_resource_get_map_info(res_handle: u32, map_info: *mut u32) -> c_int;
        pub fn virgl_renderer_resource_unref(res_handle: u32);
        pub fn virgl_renderer_context_create_fence(ctx_id: u32, flags: u32, ring_idx: u32, fence_id: u64) -> c_int;
        pub fn virgl_renderer_execute(args: *mut c_void, size: u32) -> c_int;
    }
}

const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Fences retired by virglrenderer's sync threads, waiting for
/// [`Renderer::signalled`]. Global because the callback gets only the
/// init-time cookie and virglrenderer is a per-process singleton anyway.
static FENCES: Mutex<Vec<Signalled>> = Mutex::new(Vec::new());
/// When each pending fence was created, for the create-to-signal latency
/// summary (`fence_latency`), keyed by `(ctx_id, ring_idx, fence_id)`.
static CREATED: Mutex<Option<std::collections::HashMap<(u32, u32, u64), std::time::Instant>>> = Mutex::new(None);
/// Create-to-signal latencies of the current summary window.
static LATENCY: Mutex<latency::Window<()>> = Mutex::new(latency::Window::new(latency::PERIOD));

/// Record one fence's create-to-signal time (on a virglrenderer thread).
fn fence_latency(key: (u32, u32, u64)) {
    let Some(at) = CREATED.lock().unwrap_or_else(|p| p.into_inner()).as_mut().and_then(|m| m.remove(&key)) else {
        return;
    };
    let now = std::time::Instant::now();
    let ended = LATENCY.lock().unwrap_or_else(|p| p.into_inner()).add((), now.saturating_duration_since(at), now);
    if let Some(s) = ended {
        log_latency(&s);
    }
}

/// Close the latency window if it is over (from the serve loop, through
/// [`Renderer::tick`]); how long until the open one is.
fn flush_latency() -> Option<std::time::Duration> {
    let now = std::time::Instant::now();
    let mut l = LATENCY.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = l.flush(now) {
        log_latency(&s);
    }
    l.due(now)
}

/// The count, median, 90th percentile and maximum, to the log (stderr).
fn log_latency(s: &latency::Summary<()>) {
    for (_, st) in &s.by_key {
        eprintln!(
            "conduit-venus: fence create-to-signal: {} fences in {:.1} s, p50 {} us, p90 {} us, max {} us",
            st.count,
            s.span.as_secs_f64(),
            st.p50,
            st.p90,
            st.max
        );
    }
}
static EVENT: AtomicI32 = AtomicI32::new(-1);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {
    // Context-0 fences belong to vrend, which is not built.
}

extern "C" fn write_context_fence(_cookie: *mut c_void, ctx_id: u32, ring_idx: u32, fence_id: u64) {
    // Runs on a virglrenderer thread: no panics across the FFI boundary, so a
    // poisoned lock is used as is.
    if stage::on() {
        stage::stamp(Rec::fence(stage::R_SIGNAL, ctx_id, ring_idx, fence_id, stage::now_ns()));
    }
    FENCES.lock().unwrap_or_else(|p| p.into_inner()).push(Signalled { ctx_id, ring_idx, fence_id });
    fence_latency((ctx_id, ring_idx, fence_id));
    let fd = EVENT.load(Ordering::Acquire);
    if fd >= 0 {
        let one: u64 = 1;
        // SAFETY: an 8-byte write from a local to the eventfd, which lives
        // until after virgl_renderer_cleanup has joined the sync threads.
        unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
    }
}

/// The proxy's seqno of each pending ring fence -> the guest's fence id,
/// keyed `(ctx_id, ring_idx, seqno)`: vkr reports fences by the seqno the
/// proxy gave them (patches/0003-vkr-stage-timing.patch).
type SeqnoMap = std::collections::HashMap<(u32, u32, u32), u64>;
static SEQNOS: Mutex<Option<SeqnoMap>> = Mutex::new(None);

/// vkr's and the proxy's stage reports, on the serving, render server and
/// sync threads: translated to the guest's fence id, then into the ring.
extern "C" fn vkr_stage(stage: u32, ctx_id: u32, ring_idx: u32, fence_id: u64, ts_ns: u64, aux: u64) {
    let mut seqnos = SEQNOS.lock().unwrap_or_else(|p| p.into_inner());
    let map = seqnos.get_or_insert_with(Default::default);
    if stage == u32::from(stage::V_SEQNO) {
        // Ring 0 is retired on the CPU timeline: vkr reports nothing for it.
        if ring_idx != 0 {
            if map.len() >= 1 << 16 {
                map.clear(); // fences that never retired (a lost context)
            }
            map.insert((ctx_id, ring_idx, aux as u32), fence_id);
        }
        return;
    }
    let key = (ctx_id, ring_idx, fence_id as u32);
    let Some(&guest_id) = map.get(&key) else { return };
    if stage == u32::from(stage::V_FENCE_DONE) {
        map.remove(&key);
    }
    drop(seqnos);
    stage::stamp(Rec { aux, ..Rec::fence(stage as u8, ctx_id, ring_idx, guest_id, ts_ns) });
}

type StageHookFn = Option<extern "C" fn(u32, u32, u32, u64, u64, u64)>;

/// Hand vkr the stage hook, or take it back. Looked up at run time so a
/// virglrenderer without the 0003 patch still loads (and stamps no vkr
/// stages). Returns whether vkr has the hook.
fn vkr_stage_hook(on: bool) -> bool {
    // SAFETY: a lookup in the already loaded objects; the symbol, when there,
    // has the signature of virglrenderer.h's virgl_renderer_conduit_stage_hook.
    unsafe {
        let f = libc::dlsym(libc::RTLD_DEFAULT, c"virgl_renderer_conduit_stage_hook".as_ptr());
        if f.is_null() {
            return false;
        }
        let set: extern "C" fn(StageHookFn) = std::mem::transmute(f);
        set(if on { Some(vkr_stage) } else { None });
    }
    true
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
    /// Where guest-memory blobs are mapped; reserved at the first one.
    arena: Option<crate::guest_pages::Arena>,
    /// Guest-memory blobs, by resource id: the span each is mapped at.
    guest: std::collections::HashMap<u32, crate::guest_pages::Span>,
    /// Whether every host Vulkan device imports host pointers; asked once.
    host_ptr: Option<bool>,
    /// Stage timing: per context, when its last `SUBMIT` came in and when
    /// `virgl_renderer_submit_cmd` returned, until a fence names them.
    stage_submits: std::collections::HashMap<u32, (u64, u64)>,
}

impl Virgl {
    /// Initialize virglrenderer for Venus. Fails if one already exists in
    /// this process, or if Venus cannot start (no Vulkan driver, no Venus in
    /// the build).
    pub fn new() -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return Err(Error::Refused("virglrenderer already initialized".into()));
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
        let me =
            Self { event, arena: None, guest: Default::default(), host_ptr: None, stage_submits: Default::default() };
        if stage::on() && !vkr_stage_hook(true) {
            eprintln!("conduit-venus: stage timing: this virglrenderer has no stage hook; no vkr stages");
        }
        // Venus registers its capset only when the render server came up and
        // found a Vulkan driver; without it every context would be refused.
        if me.venus_caps().1 == 0 {
            return Err(Error::Refused("virglrenderer has no Venus capset (render server or Vulkan failed)".into()));
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
            return Err(Error::Refused("export_blob returned no fd".into()));
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
            return Err(Error::Refused("capset index".into()));
        }
        let (max_version, max_size) = self.venus_caps();
        Ok(CapsetInfo { id: CAPSET_VENUS, max_version, max_size })
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>> {
        if id != CAPSET_VENUS {
            return Err(Error::Refused("capset id".into()));
        }
        let (max_ver, size) = self.venus_caps();
        if version > max_ver {
            return Err(Error::Refused("capset version".into()));
        }
        let mut caps = vec![0u8; size as usize];
        // SAFETY: caps is max_size bytes, which is what fill_caps writes.
        unsafe { ffi::virgl_renderer_fill_caps(id, version, caps.as_mut_ptr().cast()) };
        Ok(caps)
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, debug_name: &[u8]) -> Result<()> {
        if capset_id != CAPSET_VENUS {
            return Err(Error::Refused("capset id".into()));
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
            return Err(Error::Refused("submit length not a multiple of 4".into()));
        }
        // Copied into u64s: virglrenderer wants 4-byte alignment and copies
        // again internally below 8, and IPC hands us an unaligned slice.
        let t0 = if stage::on() { stage::now_ns() } else { 0 };
        let mut buf = vec![0u64; commands.len().div_ceil(8)];
        // SAFETY: buf has at least commands.len() bytes.
        unsafe { std::ptr::copy_nonoverlapping(commands.as_ptr(), buf.as_mut_ptr().cast::<u8>(), commands.len()) };
        // SAFETY: buf is aligned and ndw words long; virglrenderer only reads.
        let r = check(unsafe {
            ffi::virgl_renderer_submit_cmd(buf.as_mut_ptr().cast(), ctx_id as c_int, (commands.len() / 4) as c_int)
        });
        if t0 != 0 {
            self.stage_submits.insert(ctx_id, (t0, stage::now_ns()));
        }
        r
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
                    return Err(Error::Refused("mappable blob exported as an opaque fd that cannot be mmapped".into()));
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
        // The render worker may still hold the pointer (guest_pages module
        // comment); the arena keeps the range for guest pages only.
        if let Some(span) = self.guest.remove(&res_id)
            && let Some(a) = self.arena.as_mut()
        {
            a.unmap(span);
        }
    }

    fn create_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<()> {
        // Not MERGEABLE: every fenced guest command holds a descriptor chain
        // that only this fence's callback releases.
        CREATED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(Default::default)
            .insert((ctx_id, ring_idx, fence_id), std::time::Instant::now());
        if stage::on() {
            let now = stage::now_ns();
            if let Some((recv, done)) = self.stage_submits.remove(&ctx_id) {
                stage::stamp(Rec::fence(stage::R_RECV, ctx_id, ring_idx, fence_id, recv));
                stage::stamp(Rec::fence(stage::R_SUBMITTED, ctx_id, ring_idx, fence_id, done));
            }
            stage::stamp(Rec::fence(stage::R_FENCE, ctx_id, ring_idx, fence_id, now));
        }
        // SAFETY: plain values.
        check(unsafe { ffi::virgl_renderer_context_create_fence(ctx_id, 0, ring_idx, fence_id) })
    }

    fn fence_fd(&self) -> BorrowedFd<'_> {
        self.event.as_fd()
    }

    fn signalled(&mut self) -> Result<Vec<Signalled>> {
        let mut v: u64 = 0;
        // SAFETY: 8-byte read into a local; EAGAIN when nothing is pending.
        // Reset before taking so a fence pushed after the take re-signals.
        unsafe { libc::read(self.event.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
        // No virgl_renderer_poll here: with ASYNC_FENCE_CB the sync threads
        // own fence retirement, and polling from this thread too would race
        // them (proxy_context_retire_fences asserts against it).
        // In-process: virglrenderer cannot die without taking this with it.
        Ok(std::mem::take(&mut *FENCES.lock().unwrap_or_else(|p| p.into_inner())))
    }

    fn tick(&mut self) -> Option<std::time::Duration> {
        flush_latency()
    }

    fn features(&mut self) -> u32 {
        let host_ptr = *self.host_ptr.get_or_insert_with(|| {
            let yes = vk_probe::every_device_imports_host_pointers();
            eprintln!(
                "conduit-venus: guest-memory blobs {}",
                if yes {
                    "served (VK_EXT_external_memory_host)"
                } else {
                    "not served: a device lacks VK_EXT_external_memory_host"
                }
            );
            yes
        });
        crate::FEATURE_IMPORT_DMABUF
            | crate::FEATURE_STAGE_TRACE
            | if host_ptr { crate::FEATURE_IMPORT_GUEST_PAGES } else { 0 }
    }

    fn stages(&mut self, on: bool) -> Result<Vec<Rec>> {
        if on != stage::on() {
            stage::set_on(on);
            let hooked = vkr_stage_hook(on);
            eprintln!(
                "conduit-venus: stage timing {}{}",
                if on { "on" } else { "off" },
                if on && !hooked { " (this virglrenderer has no stage hook: no vkr stages)" } else { "" }
            );
            if !on {
                self.stage_submits.clear();
                *SEQNOS.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let (recs, lost) = stage::take();
        if lost > 0 {
            eprintln!("conduit-venus: stage timing: {lost} stamp(s) lost (ring full)");
        }
        Ok(recs)
    }

    /// The runs mapped as one span of the guest-page arena, then
    /// `virgl_renderer_resource_import_host_ptr` on it (the 0002 patch): a
    /// `VIRGL_RESOURCE_HOST_PTR` resource, which the render worker's attach
    /// receives as the pointer and vkr imports with
    /// `VkImportMemoryHostPointerInfoEXT`.
    fn import_guest_pages(&mut self, res_id: u32, ram: BorrowedFd<'_>, runs: &[crate::PageRun]) -> Result<()> {
        if self.features() & crate::FEATURE_IMPORT_GUEST_PAGES == 0 {
            return Err(Error::Refused("no host-pointer import on this host".into()));
        }
        if res_id == 0 || self.guest.contains_key(&res_id) {
            return Err(Error::Refused("import_guest_pages: resource 0 or in use".into()));
        }
        if self.arena.is_none() {
            self.arena = Some(crate::guest_pages::Arena::new(crate::guest_pages::ARENA_BYTES)?);
        }
        let arena = self.arena.as_mut().expect("made above");
        let span = arena.map(ram, runs)?;
        // SAFETY: the span is page-aligned guest memory mapped in this
        // process, and stays mapped (or reserved) for the arena's life.
        let ret =
            unsafe { ffi::virgl_renderer_resource_import_host_ptr(res_id, span.addr as *mut c_void, span.len as u64) };
        if ret != 0 {
            arena.unmap(span);
            return check(ret);
        }
        self.guest.insert(res_id, span);
        Ok(())
    }

    /// `virgl_renderer_resource_import_blob` with a dma-buf. The resource is
    /// `VIRGL_RESOURCE_FD_DMABUF`, which is what the render server's attach
    /// (`proxy_context_attach_resource`) passes on and what vkr turns into
    /// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` for
    /// `VkImportMemoryResourceInfoMESA` (`vkr_get_fd_info_from_resource_info`).
    /// `blob_mem` is HOST3D only because import_blob takes nothing else; the
    /// memory is the dma-buf's.
    fn import_dmabuf(&mut self, res_id: u32, fd: BorrowedFd<'_>, size: u64) -> Result<()> {
        if res_id == 0 || size == 0 {
            return Err(Error::Refused("import_dmabuf: resource 0 or size 0".into()));
        }
        let dup = fd.try_clone_to_owned()?;
        let args = ffi::ImportBlobArgs {
            res_handle: res_id,
            blob_mem: ffi::VIRGL_RENDERER_BLOB_MEM_HOST3D,
            fd_type: ffi::VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF,
            fd: dup.as_raw_fd(),
            size,
        };
        // SAFETY: args is a valid struct for the call.
        let ret = unsafe { ffi::virgl_renderer_resource_import_blob(&args) };
        // Ownership of the descriptor: import_blob refuses its arguments
        // (-EINVAL) before it takes the fd, and from then on owns it, closing
        // it itself if it fails (virgl_resource_create_from_fd).
        if ret == -libc::EINVAL {
            drop(dup);
            return check(ret);
        }
        std::mem::forget(dup);
        check(ret)
    }

    /// The image is described by the guest's layout (see [`ScanoutLayout`]):
    /// a Venus blob carries none the host can read back -- the pinned
    /// virglrenderer's export query answers fourcc 0, stride 0 and
    /// `DRM_FORMAT_MOD_INVALID` for any blob resource. The query is still
    /// made, so that a virglrenderer that does know the layout cannot be
    /// contradicted silently: a layout it reports must be the guest's, and
    /// linear.
    fn export_scanout(&mut self, res_id: u32, layout: ScanoutLayout) -> Result<Dmabuf> {
        let (ty, fd) = self.export(res_id)?;
        if ty != ffi::VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF {
            return Err(Error::Refused("scanout resource does not export as a dma-buf".into()));
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
        if unsafe { ffi::virgl_renderer_execute((&mut q as *mut ffi::ExportQuery).cast(), q.hdr.size) } == 0 {
            if q.out_modifier != DRM_FORMAT_MOD_INVALID && q.out_modifier != DRM_FORMAT_MOD_LINEAR {
                return Err(Error::Refused(format!(
                    "scanout resource {res_id} has modifier {:#x}; Venus scanouts must be linear",
                    q.out_modifier
                )));
            }
            let disagrees = |known: u32, guest: u32| known != 0 && known != guest;
            if disagrees(q.out_strides[0], layout.stride) || disagrees(q.out_fourcc, layout.fourcc) {
                return Err(Error::Refused(format!(
                    "scanout resource {res_id} is stride {} fourcc {:#x}, the guest said stride {} fourcc {:#x}",
                    q.out_strides[0], q.out_fourcc, layout.stride, layout.fourcc
                )));
            }
        }
        Ok(Dmabuf {
            fd,
            width: layout.width,
            height: layout.height,
            stride: layout.stride,
            offset: layout.offset,
            fourcc: layout.fourcc,
            modifier: DRM_FORMAT_MOD_LINEAR,
        })
    }
}

/// Whether the host's Vulkan devices can import host pointers
/// (`VK_EXT_external_memory_host`), which guest-memory blobs need. Asked
/// through the loader directly, as virglrenderer's own instances are out of
/// reach from here.
mod vk_probe {
    use std::ffi::{CStr, c_char, c_void};

    type Handle = *mut c_void;
    type GetInstanceProcAddr = unsafe extern "C" fn(Handle, *const c_char) -> *const c_void;
    type CreateInstance = unsafe extern "C" fn(*const InstanceCreateInfo, *const c_void, *mut Handle) -> i32;
    type DestroyInstance = unsafe extern "C" fn(Handle, *const c_void);
    type EnumeratePhysicalDevices = unsafe extern "C" fn(Handle, *mut u32, *mut Handle) -> i32;
    type EnumerateDeviceExtensionProperties =
        unsafe extern "C" fn(Handle, *const c_char, *mut u32, *mut ExtensionProperties) -> i32;

    #[repr(C)]
    struct ApplicationInfo {
        s_type: u32,
        p_next: *const c_void,
        app_name: *const c_char,
        app_version: u32,
        engine_name: *const c_char,
        engine_version: u32,
        api_version: u32,
    }

    #[repr(C)]
    struct InstanceCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        app: *const ApplicationInfo,
        layer_count: u32,
        layers: *const *const c_char,
        ext_count: u32,
        exts: *const *const c_char,
    }

    #[repr(C)]
    struct ExtensionProperties {
        name: [c_char; 256],
        spec_version: u32,
    }

    pub fn every_device_imports_host_pointers() -> bool {
        // SAFETY: dlopen/dlsym of the Vulkan loader, called through the
        // documented signatures; every out-pointer is a live local, and the
        // instance is destroyed before return.
        unsafe {
            let lib = libc::dlopen(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return false;
            }
            let gipa = libc::dlsym(lib, c"vkGetInstanceProcAddr".as_ptr());
            if gipa.is_null() {
                return false;
            }
            let gipa: GetInstanceProcAddr = std::mem::transmute(gipa);
            let create = gipa(std::ptr::null_mut(), c"vkCreateInstance".as_ptr());
            if create.is_null() {
                return false;
            }
            let create: CreateInstance = std::mem::transmute(create);
            let app = ApplicationInfo {
                s_type: 0,
                p_next: std::ptr::null(),
                app_name: c"conduit-venus probe".as_ptr(),
                app_version: 0,
                engine_name: std::ptr::null(),
                engine_version: 0,
                api_version: (1 << 22) | (1 << 12), // 1.1
            };
            let info = InstanceCreateInfo {
                s_type: 1,
                p_next: std::ptr::null(),
                flags: 0,
                app: &app,
                layer_count: 0,
                layers: std::ptr::null(),
                ext_count: 0,
                exts: std::ptr::null(),
            };
            let mut inst: Handle = std::ptr::null_mut();
            if create(&info, std::ptr::null(), &mut inst) != 0 || inst.is_null() {
                return false;
            }
            let destroy: DestroyInstance = std::mem::transmute(gipa(inst, c"vkDestroyInstance".as_ptr()));
            let enum_pd: EnumeratePhysicalDevices =
                std::mem::transmute(gipa(inst, c"vkEnumeratePhysicalDevices".as_ptr()));
            let enum_ext: EnumerateDeviceExtensionProperties =
                std::mem::transmute(gipa(inst, c"vkEnumerateDeviceExtensionProperties".as_ptr()));
            let mut n = 0u32;
            let mut ok = enum_pd(inst, &mut n, std::ptr::null_mut()) == 0 && n > 0;
            let mut pds = vec![std::ptr::null_mut(); n as usize];
            ok &= ok && enum_pd(inst, &mut n, pds.as_mut_ptr()) == 0;
            pds.truncate(n as usize);
            for pd in &pds {
                if !ok {
                    break;
                }
                let mut m = 0u32;
                if enum_ext(*pd, std::ptr::null(), &mut m, std::ptr::null_mut()) != 0 {
                    ok = false;
                    break;
                }
                let mut exts: Vec<ExtensionProperties> =
                    (0..m).map(|_| ExtensionProperties { name: [0; 256], spec_version: 0 }).collect();
                // VK_INCOMPLETE (5) only if the list grew in between.
                if enum_ext(*pd, std::ptr::null(), &mut m, exts.as_mut_ptr()) != 0 {
                    ok = false;
                    break;
                }
                ok = exts
                    .iter()
                    .take(m as usize)
                    .any(|e| CStr::from_ptr(e.name.as_ptr()) == c"VK_EXT_external_memory_host");
            }
            destroy(inst, std::ptr::null());
            ok
        }
    }
}

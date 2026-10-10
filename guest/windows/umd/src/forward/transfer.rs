//! Copy, resolve, map/unmap, flush, discard, clear-view and UpdateSubresource --
//! everything that moves or invalidates resource CONTENTS.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

// --- Copy / Map / Flush -----------------------------------------------------

pub(crate) unsafe extern "system" fn resource_copy(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    h_src: ddi::D3D10DDI_HRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (load_resource(h_dst), load_resource(h_src)) else {
        if COPY_LOG_COUNT.first_n(256).is_some() {
            log_error!(
                "DDI resource_copy missing resource dst_priv={:p} src_priv={:p}",
                h_dst.pDrvPrivate,
                h_src.pDrvPrivate
            );
        }
        return;
    };
    let dst_alloc = resource_allocation(h_dst);
    let src_alloc = resource_allocation(h_src);
    let n = COPY_LOG_COUNT.next();
    if n < 256 || dst_alloc != 0 || src_alloc != 0 {
        trace_line!(
            "DDI resource_copy dst_alloc=0x{:x} src_alloc=0x{:x}",
            dst_alloc,
            src_alloc
        );
    }
    context.CopyResource(&*dst, &*src);
}

pub(crate) unsafe extern "system" fn resource_copy_region(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let dst = load_resource(h_dst);
    let src = load_resource(h_src);
    let n = COPY_REGION_LOG_COUNT.next();
    if crate::trace_enabled() && (n < 1024 || dst.is_none() || src.is_none()) {
        let dst_summary = resource_summary(h_dst);
        let src_summary = resource_summary(h_src);
        let (dst_rt, dst_km) = resource_parent_handles(h_dst);
        let (src_rt, src_km) = resource_parent_handles(h_src);
        trace_line!(
            "DDI ResourceCopyRegion: #{} dstDrv={:p} dstRT={:p} dstKM=0x{:x} \
             dstAlloc=0x{:x} dst={}x{} fmt={} srcDrv={:p} srcRT={:p} srcKM=0x{:x} \
             srcAlloc=0x{:x} src={}x{} fmt={} dstSub={} xyz={},{},{} srcSub={} box={:p} \
             dstOk={} srcOk={}",
            n,
            h_dst.pDrvPrivate,
            dst_rt,
            dst_km,
            dst_summary.0,
            dst_summary.2,
            dst_summary.3,
            dst_summary.5,
            h_src.pDrvPrivate,
            src_rt,
            src_km,
            src_summary.0,
            src_summary.2,
            src_summary.3,
            src_summary.5,
            dst_subresource,
            dst_x,
            dst_y,
            dst_z,
            src_subresource,
            box_,
            dst.is_some(),
            src.is_some(),
        );
    }
    let (Some(dst), Some(src)) = (dst, src) else {
        return;
    };
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.CopySubresourceRegion(
        &*dst,
        dst_subresource,
        dst_x,
        dst_y,
        dst_z,
        &*src,
        src_subresource,
        bx_ptr,
    );
}

pub(crate) unsafe extern "system" fn resource_copy_region_11_1(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    dst_x: u32,
    dst_y: u32,
    dst_z: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    _copy_flags: u32,
) {
    resource_copy_region(
        h,
        h_dst,
        dst_subresource,
        dst_x,
        dst_y,
        dst_z,
        h_src,
        src_subresource,
        box_,
    );
}

pub(crate) unsafe extern "system" fn resource_resolve_subresource(
    h: Hdevice,
    h_dst: ddi::D3D10DDI_HRESOURCE,
    dst_subresource: u32,
    h_src: ddi::D3D10DDI_HRESOURCE,
    src_subresource: u32,
    format: ddi::DXGI_FORMAT,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let (Some(dst), Some(src)) = (load_resource(h_dst), load_resource(h_src)) else {
        return;
    };
    context.ResolveSubresource(
        &*dst,
        dst_subresource,
        &*src,
        src_subresource,
        DXGI_FORMAT(format as i32),
    );
}

/// Returns 0 = "not busy", unconditionally. That is a semantic CLAIM the
/// runtime acts on, not a no-op: an app polling this to avoid a stalling Map is
/// told the staging resource is always free. Counted, behaviour unchanged. R911.
pub(crate) unsafe extern "system" fn resource_is_staging_busy(
    _h: Hdevice,
    _h_resource: ddi::D3D10DDI_HRESOURCE,
) -> i32 {
    note_ddi_refusal(&DDI_REFUSALS.staging_busy_assumed_free);
    0
}

pub(crate) unsafe extern "system" fn resource_map(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    map_type: ddi::D3D10_DDI_MAP,
    _map_flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource) else {
        return;
    };
    let mut out = D3D11_MAPPED_SUBRESOURCE::default();
    // Video decoder buffers are staging buffers (forward/video.rs), which take
    // no DISCARD/NO_OVERWRITE map. Only checked for those two map types.
    let map_type = if (map_type == 4 || map_type == 5) && is_decoder_buffer(h_resource) {
        decoder_buffer_map_type(map_type as u32) as ddi::D3D10_DDI_MAP
    } else {
        map_type
    };
    // DDI D3D10_DDI_MAP values match D3D11_MAP (READ=1, WRITE=2, ...).
    match context.Map(
        &*res,
        subresource,
        D3D11_MAP(map_type as i32),
        0,
        Some(&mut out),
    ) {
        Ok(()) => {
            let allocation = resource_allocation(h_resource);
            let n = MAP_LOG_COUNT.next();
            if n < 256 || allocation != 0 {
                trace_line!(
                    "DDI resource_map ok alloc=0x{:x} sub={} map={} rowPitch={} depthPitch={} pData={:p}",
                    allocation,
                    subresource,
                    map_type,
                    out.RowPitch,
                    out.DepthPitch,
                    out.pData
                );
            }
            if !mapped.is_null() {
                (*mapped).pData = out.pData;
                (*mapped).RowPitch = out.RowPitch;
                (*mapped).DepthPitch = out.DepthPitch;
            }
        }
        Err(e) => {
            log_error!("DDI resource_map failed: {e:?}");
            if !mapped.is_null() {
                (*mapped).pData = core::ptr::null_mut();
            }
        }
    }
}

/// `pfnDynamicConstantBufferMapNoOverwrite`, which exists only in
/// `D3D11_1DDI_DEVICEFUNCS` and later. Its PFN type is `PFND3D10DDI_RESOURCEMAP`
/// — the same shape as `pfnDynamicIABufferMapNoOverwrite`, which `install()`
/// already points at `resource_map` — and `D3D10_DDI_MAP_WRITE_NOOVERWRITE`
/// equals `D3D11_MAP_WRITE_NO_OVERWRITE` (4), so no-overwrite semantics reach
/// DXVK unchanged.
///
/// The wrapper exists only to make the slot's first use measurable: it spent
/// its life on `ddi_noop_device`, a stub that returns without touching the
/// caller's `D3D10DDI_MAPPED_SUBRESOURCE` even though filling it is this
/// slot's entire job.
pub(crate) unsafe extern "system" fn dynamic_cb_map_no_overwrite(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    map_type: ddi::D3D10_DDI_MAP,
    map_flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    static FIRST_HIT: AtomicUsize = AtomicUsize::new(0);
    if FIRST_HIT.fetch_add(1, Ordering::Relaxed) == 0 {
        trace_line!(
            "DDI DynamicConstantBufferMapNoOverwrite: first hit sub={} map={}",
            subresource,
            map_type
        );
    }
    resource_map(h, h_resource, subresource, map_type, map_flags, mapped);
}

pub(crate) unsafe extern "system" fn resource_unmap(
    h: Hdevice,
    h_resource: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_resource) else {
        return;
    };
    context.Unmap(&*res, subresource);
}

// DXVK tracks read-after-write hazards itself from the barrier state it already
// maintains, so forwarding these adds nothing -- but "we deliberately do nothing
// here" and "nobody ever wired this up" were the same empty body. R911.
pub(crate) unsafe extern "system" fn srv_read_after_write_hazard(
    _h: Hdevice,
    _srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    _resource: ddi::D3D10DDI_HRESOURCE,
) {
    note_ddi_refusal(&DDI_REFUSALS.srv_raw_hazard);
}

pub(crate) unsafe extern "system" fn resource_read_after_write_hazard(
    _h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
) {
    note_ddi_refusal(&DDI_REFUSALS.resource_raw_hazard);
}

pub(crate) unsafe extern "system" fn flush(h: Hdevice) {
    if report_if_removed(h, "Flush") {
        return;
    }
    if let Some(context) = d3d11_context(h) {
        context.Flush();
        super::present::present_timing::ddi_flush_gate(|| flush_gate(h, &context));
    }
}

static FLUSH_GATE_SENT: [AtomicUsize; 3] =
    [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];
static FLUSH_GATE_RENDER_FAILED: AtomicUsize = AtomicUsize::new(0);
static FLUSH_GATE_FALLBACK: AtomicUsize = AtomicUsize::new(0);

/// Cross-process hand-off of shared surfaces at a flush (shared-surfaces.md
/// section 4, flush-gate.md section 9), for a device holding a cross-process
/// shared resource (created or opened `MISC_SHARED`, not `BIND_PRESENT`) and
/// only when work was recorded since the previous hand-off.
///
/// The runtime releases a keyed mutex right after `pfnFlush`; the acquirer's
/// `AcquireSync` returns on the CPU, and its reads go through the Venus ring or
/// NVK's RM channel, which nothing in dxgkrnl holds. So the order must come
/// from the acquirer's side, or from the releaser completing first:
///
/// * Venus: a point of the device's present stream is signalled behind the
///   recorded work and published on the shared allocations this device
///   created (`HELIOS_FLUSH_GATE_PUBLISH=all`: opened ones too). An importer's
///   read of the image waits for the allocation's announced epoch in its own
///   submission worker (DXVK's producer wait), so only the acquirer waits,
///   only for this producer, and nothing heads the adapter's WDDM queue. A
///   device that only opened shared surfaces (DWM) does nothing.
/// * NVK: no acquirer-side wait yet (that needs the releaser's RM semaphore in
///   the acquirer, S4 fences), so the releaser completes its work on the CPU.
///
/// The `HEFL` flush-gate packet (flush-gate.md) is OFF by default: it orders
/// only dxgkrnl's release, which nobody needs once the acquirer waits, and its
/// packet heads the adapter-wide WDDM queue until the point retires (DWM's
/// D3D12 swap-chain buffers made that a 250 ms stall per frame). It stays for
/// experiments behind `HELIOS_FLUSH_GATE_HEFL=1`, on the KMD's capability.
pub(crate) unsafe fn flush_gate(h: Hdevice, context: &ID3D11DeviceContext) {
    use crate::bridge::{FlushGatePoint, FLUSH_GATE_RM_FENCE, FLUSH_GATE_STREAM};
    let Some(dev) = helios_device(h) else {
        return;
    };
    if lock_ignore_poison(&dev.nvk_keyed_resources).is_empty() {
        return;
    }
    let cpu_wait = gate_cpu_wait();
    // The hand-off ledger: publish a point of this device on every shared
    // resource it holds; readers in other processes wait for its completion
    // in their submission worker. Nobody waits here.
    if ledger_enabled() {
        let all = shared_resources(dev, true);
        match dev.dxvk.handoff_publish(&all) {
            0 => return,
            1 => {
                LEDGER_HANDOFFS.fetch_add(1, Ordering::Relaxed);
                if cpu_wait == GateCpuWait::Forced {
                    nvk_keyed_flush_wait(h, context, true);
                }
                return;
            }
            _ => {
                // No ledger (or full): the releaser completes its work.
                FLUSH_GATE_FALLBACK.fetch_add(1, Ordering::Relaxed);
                if cpu_wait != GateCpuWait::Off {
                    nvk_keyed_flush_wait(h, context, true);
                }
                return;
            }
        }
    }
    let hefl = hefl_enabled();
    if dev.dxvk.is_nvk() {
        if hefl && crate::knobs::nvk_rm_fence() && crate::scanout_acquire::nvrm_flush_gate_capable(dev) {
            match dev.dxvk.flush_gate_point(FLUSH_GATE_RM_FENCE, &[]) {
                FlushGatePoint::Nothing => return,
                FlushGatePoint::Ready { fence, fence_value, .. } if fence != 0 => {
                    // The handle is the KMD's from here on, whatever happens.
                    send_flush_gate(
                        dev,
                        helios_protocol::HELIOS_FLUSH_GATE_FLAG_RM_FENCE,
                        (0, 0, 0),
                        Some((fence, fence_value)),
                    );
                }
                _ => {}
            }
        }
        if cpu_wait != GateCpuWait::Off {
            nvk_keyed_flush_wait(h, context, true);
        }
        return;
    }
    // Venus.
    let publish = publish_list(dev);
    if publish.is_empty() {
        // Nothing of ours another process reads through a producer wait.
        if cpu_wait == GateCpuWait::Forced {
            nvk_keyed_flush_wait(h, context, true);
        }
        return;
    }
    match dev.dxvk.flush_gate_point(FLUSH_GATE_STREAM, &publish) {
        FlushGatePoint::Nothing => return,
        FlushGatePoint::Ready { ctx, value, cookie, .. } if ctx != 0 && cookie != 0 => {
            if hefl && crate::scanout_acquire::flush_gate_capable() {
                send_flush_gate(
                    dev,
                    helios_protocol::HELIOS_FLUSH_GATE_FLAG_STREAM,
                    (ctx, value, cookie),
                    None,
                );
            }
            if cpu_wait == GateCpuWait::Forced {
                nvk_keyed_flush_wait(h, context, true);
            }
        }
        _ => {
            // No present stream (old ICD/KMD): nothing published, so the
            // releaser completes its work unless told otherwise.
            FLUSH_GATE_FALLBACK.fetch_add(1, Ordering::Relaxed);
            if cpu_wait != GateCpuWait::Off {
                nvk_keyed_flush_wait(h, context, true);
            }
        }
    }
}

static LEDGER_HANDOFFS: AtomicUsize = AtomicUsize::new(0);

/// `HELIOS_HANDOFF_LEDGER=1` (process environment, in every process sharing
/// the surface): the hand-off ledger. Off by default until it is verified
/// across processes; the default is the previous hand-off handling (Venus
/// producer publication, NVK releaser CPU wait, optional HEFL).
pub(crate) fn ledger_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("HELIOS_HANDOFF_LEDGER").is_ok_and(|v| v == "1"))
}

/// The registered shared resources of `dev` as `ID3D11Resource*`; opened ones
/// too when `opened`.
fn shared_resources(dev: &crate::device_funcs::HeliosDevice, opened: bool) -> Vec<usize> {
    let list = lock_ignore_poison(&dev.nvk_keyed_resources);
    let mut out = Vec::with_capacity(list.len());
    for &(key, created) in list.iter() {
        if !created && !opened {
            continue;
        }
        // SAFETY: `key` is the live pDrvPrivate of a resource of this device
        // (destroy removes it from the list before the slot goes).
        if let Some(res) = unsafe {
            load_resource(ddi::D3D10DDI_HRESOURCE {
                pDrvPrivate: key as *mut c_void,
            })
        } {
            out.push(res.as_raw() as usize);
        }
    }
    out
}

/// `HELIOS_FLUSH_GATE_HEFL=1` (process environment): also send the `HEFL`
/// flush-gate packet (needs the KMD capability). Off by default: see
/// `flush_gate`.
fn hefl_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("HELIOS_FLUSH_GATE_HEFL").is_ok_and(|v| v == "1"))
}

/// `HELIOS_FLUSH_GATE_CPU_WAIT` (process environment): what the releaser
/// does on the CPU at a hand-off. Unset: NVK completes its work (there is no
/// acquirer-side wait on NVK yet), Venus relies on the published point. `1`:
/// every backend completes its work (the diagnosis of flush-gate.md section
/// 9). `0`: no CPU wait anywhere (shows the unordered acquirer).
#[derive(Clone, Copy, PartialEq, Eq)]
enum GateCpuWait {
    Default,
    Forced,
    Off,
}

fn gate_cpu_wait() -> GateCpuWait {
    static MODE: std::sync::OnceLock<GateCpuWait> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("HELIOS_FLUSH_GATE_CPU_WAIT").as_deref() {
        Ok("1") => GateCpuWait::Forced,
        Ok("0") => GateCpuWait::Off,
        _ => GateCpuWait::Default,
    })
}

/// `HELIOS_FLUSH_GATE_PUBLISH` (process environment): which shared resources a
/// Venus flush gate publishes its point on. Unset: the ones this device
/// created (an opener of many surfaces, DWM above all, would otherwise publish
/// on every one at every flush). `all`: opened ones too. `0`: none (shows the
/// unordered acquirer).
fn publish_list(dev: &crate::device_funcs::HeliosDevice) -> Vec<usize> {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    let mode = *MODE.get_or_init(|| match std::env::var("HELIOS_FLUSH_GATE_PUBLISH").as_deref() {
        Ok("0") => 0,
        Ok("all") => 2,
        _ => 1,
    });
    if mode == 0 {
        return Vec::new();
    }
    shared_resources(dev, mode == 2)
}

/// One `HEFL` (`HeliosFlushGateCmd`, 48 bytes) through `pfnRenderCb` on the
/// device's context: no allocations, no patches. Logs a failure; never
/// propagates it.
unsafe fn send_flush_gate(
    dev: &crate::device_funcs::HeliosDevice,
    flags: u32,
    stream: (u32, u32, u64),
    fence: Option<(u32, u64)>,
) {
    let kind = if flags & helios_protocol::HELIOS_FLUSH_GATE_FLAG_RM_FENCE != 0 {
        2
    } else if flags & helios_protocol::HELIOS_FLUSH_GATE_FLAG_STREAM != 0 {
        1
    } else {
        0
    };
    let fail = |why: &str| {
        let n = FLUSH_GATE_RENDER_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 16 || n % 1024 == 0 {
            log_error!("DDI flush gate (kind {kind}) not sent: {why} (x{n})");
        }
    };
    let Some(ctx) = dev.context.as_ref() else {
        return fail("no runtime context");
    };
    if dev.kt_callbacks.is_null() {
        return fail("no callback table");
    }
    let Some(render_cb) = (*dev.kt_callbacks).pfnRenderCb else {
        return fail("pfnRenderCb missing");
    };
    let len = core::mem::size_of::<helios_protocol::HeliosFlushGateCmd>() as u32;
    let Some(window) = ctx.command.get() else {
        return fail("no command buffer");
    };
    if window.capacity < len {
        return fail("command buffer too small");
    }
    let (fence_handle, fence_value) = fence.unwrap_or((0, 0));
    let cmd = helios_protocol::HeliosFlushGateCmd {
        magic: helios_protocol::HELIOS_FLUSH_GATE_MAGIC,
        version: helios_protocol::HELIOS_FLUSH_GATE_VERSION,
        flags,
        ctx_id: stream.0,
        value: stream.1,
        reserved: 0,
        cookie: stream.2,
        fence: helios_protocol::HeliosRmFenceTail {
            rm_fence_handle: fence_handle,
            flags: if fence.is_some() { helios_protocol::HELIOS_RM_FENCE_TAIL_FLAG_FENCE } else { 0 },
            rm_fence_value: fence_value,
        },
    };
    (window.ptr.as_ptr() as *mut helios_protocol::HeliosFlushGateCmd).write_unaligned(cmd);
    let mut render = ddi::D3DDDICB_RENDER::default();
    render.CommandLength = len;
    render.CommandOffset = 0;
    render.NumAllocations = 0;
    render.NumPatchLocations = 0;
    render.hContext = ctx.handle.as_ptr();
    let hr = render_cb(dev.h_rt_device, &mut render);
    if hr < 0 {
        return fail(&format!("pfnRenderCb hr=0x{:08x}", hr as u32));
    }
    if render.NewCommandBufferSize != 0 {
        if let Some(w) = crate::device_funcs::Window::new(render.pNewCommandBuffer, render.NewCommandBufferSize) {
            ctx.command.set(Some(w));
        }
    }
    if render.NewAllocationListSize != 0 {
        if let Some(w) = crate::device_funcs::Window::new(render.pNewAllocationList, render.NewAllocationListSize) {
            ctx.allocations.set(Some(w));
        }
    }
    if render.NewPatchLocationListSize != 0 {
        if let Some(w) =
            crate::device_funcs::Window::new(render.pNewPatchLocationList, render.NewPatchLocationListSize)
        {
            ctx.patches.set(Some(w));
        }
    }
    let n = FLUSH_GATE_SENT[kind].fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 4 || n % 4096 == 0 {
        log_error!(
            "DDI flush gate: {} #{n} (ctx {} value {} fence {}; wire {} stream {} fence {}, fallbacks {}, failed {})",
            ["wire", "stream", "rm-fence"][kind],
            stream.0,
            stream.1,
            fence_handle,
            FLUSH_GATE_SENT[0].load(Ordering::Relaxed),
            FLUSH_GATE_SENT[1].load(Ordering::Relaxed),
            FLUSH_GATE_SENT[2].load(Ordering::Relaxed),
            FLUSH_GATE_FALLBACK.load(Ordering::Relaxed),
            FLUSH_GATE_RENDER_FAILED.load(Ordering::Relaxed),
        );
    }
}

static NVK_KEYED_WAITS: AtomicUsize = AtomicUsize::new(0);

/// `HELIOS_KEYED_FLUSH_WAIT=1` (process environment): the keyed-mutex flush
/// wait on any backend, Venus included. Diagnostic: d3d11_share keyed-load
/// shows Venus devices misorder keyed-mutex hand-offs across processes too
/// (docs/shared-surfaces.md section 4); off by default there because DWM
/// runs on Venus.
pub(crate) fn keyed_flush_wait_forced() -> bool {
    static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| {
        std::env::var("HELIOS_KEYED_FLUSH_WAIT").is_ok_and(|v| v == "1")
    })
}

/// Wait on the CPU until every command `dev` submitted so far has completed on the GPU. The
/// immediate context flushes what is recorded and names the newest submission
/// (`flush_present_copy`); the wait then sleeps on DXVK's submission-fence condition variable,
/// which the queue's completion thread wakes (`wait_present_copy`), bounded at 5 s. No polling:
/// the thread is off the CPU until the GPU is done. `None` when the device has no context or the
/// wait failed (device lost). `what` names the caller in the log.
pub(crate) unsafe fn wait_submitted(
    dev: &crate::device_funcs::HeliosDevice,
    _context: &ID3D11DeviceContext,
    what: &str,
) -> Option<std::time::Instant> {
    const TIMEOUT_US: u32 = 5_000_000;
    let start = std::time::Instant::now();
    let submission = dev.dxvk.flush_present_copy();
    if submission == 0 {
        // Nothing was ever submitted (nothing to wait for), or no context / a failed device
        // (nothing that could still complete).
        return Some(start);
    }
    match dev.dxvk.wait_present_copy(submission, TIMEOUT_US) {
        0 => {}
        1 => log_error!("DDI NVK {what}: GPU not done after 5 s, going on"),
        r => {
            log_error!("DDI NVK {what}: submission wait failed ({r})");
            return None;
        }
    }
    Some(start)
}

/// NVK with a live keyed-mutex shared resource (docs/shared-surfaces.md §4,
/// v1): wait on the CPU until every command this device submitted so far has
/// completed on the GPU. The Microsoft runtime releases a keyed mutex right
/// after `pfnFlush`, and dxgkrnl orders that release against this device's
/// DMA buffers only; NVK submits to RM directly, so without the wait another
/// process could acquire the key and read before our GPU writes landed. The
/// acquirer needs nothing: its `AcquireSync` returns only after this release.
///
/// An event query issued after the flush signals when all earlier work is
/// done (DXVK tracks it with the submission's fence; on NVK an RM semaphore).
/// S4 replaces this with an RM-fence boundary on the DMA buffer, no CPU wait.
pub(crate) unsafe fn nvk_keyed_flush_wait(h: Hdevice, context: &ID3D11DeviceContext, force: bool) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    if (!force && !dev.dxvk.is_nvk() && !keyed_flush_wait_forced())
        || lock_ignore_poison(&dev.nvk_keyed_resources).is_empty()
    {
        return;
    }
    let Some(start) = wait_submitted(dev, context, "keyed-mutex flush wait") else {
        return;
    };
    let n = NVK_KEYED_WAITS.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 4 || n % 1024 == 0 {
        log_error!(
            "DDI NVK keyed-mutex flush wait #{n}: {} us",
            start.elapsed().as_micros()
        );
    }
}

pub(crate) unsafe extern "system" fn flush_11_1(h: Hdevice, _flush_flags: u32) -> ddi::BOOL {
    flush(h);
    1
}

pub(crate) unsafe extern "system" fn discard_11_1(
    h: Hdevice,
    handle_type: ddi::D3D11DDI_HANDLETYPE,
    handle: *mut c_void,
    _rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    if D3D11_1_LOG_COUNT.first_n(64).is_some() {
        trace_line!("DDI D3D11.1 Discard: type={handle_type} rects={num_rects}");
    }

    // A rect-limited discard invalidates ONLY the given sub-rects. D3D11's
    // Discard is a hint, so discarding LESS than requested is always legal —
    // discarding more is not: forwarding these as full-view discards made
    // DXVK reinitialize the whole image, wiping the undamaged 99% of dwm's
    // flip backbuffer every frame (dwm redraws only the dirty region and
    // rect-discards exactly that region on the incoming buffer). Match
    // upstream DXVK's DiscardView1 behaviour: drop partial discards.
    if num_rects != 0 {
        note_ddi_refusal(&DDI_REFUSALS.discard_partial);
        return;
    }

    let Some(context) = d3d11_context1(h) else {
        return;
    };
    match handle_type {
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE => {
            if let Some(res) = load_resource_at(handle) {
                context.DiscardResource(&*res);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_SHADERRESOURCEVIEW => {
            if let Some(view) = load_com_at::<ID3D11ShaderResourceView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW => {
            if let Some(view) = load_rtv_at(handle).and_then(|v| (*v).cast::<ID3D11View>().ok()) {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_DEPTHSTENCILVIEW => {
            if let Some(view) = load_com_at::<ID3D11DepthStencilView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        ddi::D3D11DDI_HANDLETYPE_D3D11DDI_HT_UNORDEREDACCESSVIEW => {
            if let Some(view) = load_com_at::<ID3D11UnorderedAccessView>(handle)
                .and_then(|v| (*v).cast::<ID3D11View>().ok())
            {
                context.DiscardView(&view);
            }
        }
        _ => {}
    }
}

pub(crate) unsafe extern "system" fn check_direct_flip_support_11_1(
    _h: Hdevice,
    resource1: ddi::D3D10DDI_HRESOURCE,
    resource2: ddi::D3D10DDI_HRESOURCE,
    flags: u32,
    supported: *mut ddi::BOOL,
) {
    let mode = crate::knobs::direct_flip_support();
    // The default (0) answers no with no work at all: no resource lookup, no formatting, one
    // bounded log line. DWM may ask on every frame it considers a DirectFlip.
    if mode == 0 {
        if !supported.is_null() {
            *supported = 0;
        }
        if D3D11_1_DIRECT_FLIP_ASKED.fetch_add(1, Ordering::Relaxed) == 0 {
            log_error!(
                "DDI D3D11.1 CheckDirectFlipSupport: DirectFlipSupport=0 -> no (logged once)"
            );
        }
        return;
    }
    let (_, k1, w1, h1, a1, f1) = resource_summary(resource1);
    let (_, k2, w2, h2, a2, f2) = resource_summary(resource2);
    // Size: the KMD has no scaler, so the application's buffer must have DWM's primary's extent
    // (the mode). Format: equal (the 393.1 rule), or with 5 only two 8-bit scan-out formats (an
    // R8G8B8A8 game on the B8G8R8A8 desktop). 5 is opt-in: a promoted R8G8B8A8 CS2 swap chain
    // froze after one frame on 405.8 (docs/independent-flip.md 13.10).
    let same_size = k1 == "tex2d" && k2 == "tex2d" && w1 == w2 && h1 == h2 && w1 != 0;
    let formats_ok = helios_umd_common::format::direct_flip_formats_compatible(f1, f2, mode != 5);
    let same = same_size && formats_ok;
    let (answer, why) = match mode {
        0 => (false, "DirectFlipSupport=0"),
        1 | 3 | 4 | 5 if !kmd_reports_direct_flip() => (false, "dxgkrnl reports no DirectFlip"),
        // 3: resource1 (the app's) must be able to replace resource2 (DWM's primary) on scan-out
        // as is, in a layout the KMD can flip (the stricter rule; it was 1 in driver 388.1, the
        // build whose run never promoted, so 1 is back to the rule measured promoting).
        3 => match scanout_pair_compatible(resource1, resource2) {
            Ok(()) => (true, "same scan-out layout"),
            Err(why) => (false, why),
        },
        // 1 and 4 (11.6's rule as measured: the same format), 5 (scan-out-compatible formats,
        // opt-in) and 2 (the test lever, no dxgkrnl query): same size and a matching format.
        _ if same => (true, "same size, scan-out compatible format"),
        _ if !same_size => (false, "size differs (no scaler: the buffer must cover the mode)"),
        _ => (false, "formats not scan-out compatible"),
    };
    if !supported.is_null() {
        *supported = answer as ddi::BOOL;
    }
    // Evidence for the promotion question (who asks, about what, and what it got): the first 64
    // answers, then one in 4096.
    let n = D3D11_1_DIRECT_FLIP_ASKED.fetch_add(1, Ordering::Relaxed);
    if n < 64 || n % 4096 == 0 {
        log_error!(
            "DDI D3D11.1 CheckDirectFlipSupport #{n}: dwm={} flags=0x{flags:x} mode={mode} \
             app={k1} {w1}x{h1} fmt={f1} slices={a1} dwm_res={k2} {w2}x{h2} fmt={f2} slices={a2} \
             -> {} ({why})",
            crate::knobs::is_dwm_process(),
            if answer { "yes" } else { "no" }
        );
    }
}

static D3D11_1_DIRECT_FLIP_ASKED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// DXGI formats the KMD can put on the scan-out as they are (`ScanoutFormat`): R8G8B8A8_UNORM,
/// B8G8R8A8_UNORM, B8G8R8X8_UNORM. sRGB-typed aliases, 10-bit and fp16 are refused there
/// (`docs/independent-flip.md` 2.4), and a promoted chain the KMD refuses would freeze on its
/// last frame, so the UMD does not offer them.
const SCANOUT_DXGI_FORMATS: [u32; 3] = helios_umd_common::format::SCANOUT_DXGI_FORMATS;

/// Whether the application's swap-chain buffer `app` can replace DWM's `dwm` on the scan-out
/// with no conversion: both single-sample, single-mip, single-slice 2-D textures of the same
/// extent, in 8-bit scan-out formats (equal, or e.g. R8G8B8A8 against B8G8R8A8: the KMD flips
/// each buffer in its own format). DWM's buffer has the display mode's extent, so this is
/// also "the application covers the output at the mode" (there is no scaler: the KMD flips
/// only a buffer of the mode's extent). `Err` names the first mismatch, for the log.
unsafe fn scanout_pair_compatible(
    app: ddi::D3D10DDI_HRESOURCE,
    dwm: ddi::D3D10DDI_HRESOURCE,
) -> Result<(), &'static str> {
    let desc = |h: ddi::D3D10DDI_HRESOURCE| -> Option<D3D11_TEXTURE2D_DESC> {
        let r = load_resource(h)?;
        let t = (*r).cast::<ID3D11Texture2D>().ok()?;
        let mut d = D3D11_TEXTURE2D_DESC::default();
        t.GetDesc(&mut d);
        Some(d)
    };
    let (Some(a), Some(d)) = (desc(app), desc(dwm)) else {
        return Err("not two 2-D textures");
    };
    if a.Width == 0 || a.Width != d.Width || a.Height != d.Height {
        return Err("extent differs");
    }
    if !SCANOUT_DXGI_FORMATS.contains(&(a.Format.0 as u32)) {
        return Err("not a scan-out format");
    }
    if !helios_umd_common::format::direct_flip_formats_compatible(
        a.Format.0 as u32,
        d.Format.0 as u32,
        true,
    ) {
        return Err("format differs");
    }
    if a.SampleDesc.Count != 1 || d.SampleDesc.Count != 1 {
        return Err("multisampled");
    }
    if a.MipLevels > 1 || a.ArraySize != 1 || d.ArraySize != 1 {
        return Err("mips or array slices");
    }
    Ok(())
}

/// Does dxgkrnl report DirectFlip support (KMTQAITYPE_DIRECTFLIP_SUPPORT, from
/// the KMD's SupportDirectFlip cap) for a hardware render adapter? Asked once
/// per process through gdi32's D3DKMT entry points.
fn kmd_reports_direct_flip() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        // SAFETY: gdi32's documented D3DKMT ABI; every buffer is local and
        // sized as the call expects; adapters opened by EnumAdapters2 are closed.
        let r = unsafe { query_adapter_support(KMTQAITYPE_DIRECTFLIP_SUPPORT) };
        log_error!("DDI CheckDirectFlipSupport: dxgkrnl DirectFlip support = {r}");
        r
    })
}

const KMTQAITYPE_DIRECTFLIP_SUPPORT: u32 = 19;
const KMTQAITYPE_INDEPENDENTFLIP_SUPPORT: u32 = 28;

/// Does dxgkrnl report independent-flip support (KMTQAITYPE_INDEPENDENTFLIP_SUPPORT, derived
/// from the KMD's `FlipIndependent` caps, i.e. `IndepFlip`) for a hardware render adapter?
/// Asked once per process. Off by default: the KMD does not advertise it unless `IndepFlip`
/// is set (docs/independent-flip.md section 11).
pub(crate) fn kmd_reports_independent_flip() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        // SAFETY: as `kmd_reports_direct_flip`.
        let r = unsafe { query_adapter_support(KMTQAITYPE_INDEPENDENTFLIP_SUPPORT) };
        log_error!("dxgkrnl IndependentFlip support = {r}");
        r
    })
}

/// Whether any hardware render adapter answers `kind` (a `KMTQAITYPE_*` whose answer starts
/// with a BOOL `Supported`) with yes.
unsafe fn query_adapter_support(kind_query: u32) -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct AdapterInfo {
        h_adapter: u32,
        luid_low: u32,
        luid_high: i32,
        num_sources: u32,
        precise_present_regions: i32,
    }
    #[repr(C)]
    struct EnumAdapters2 {
        num_adapters: u32,
        adapters: *mut AdapterInfo,
    }
    #[repr(C)]
    struct QueryAdapterInfo {
        h_adapter: u32,
        kind: u32,
        data: *mut c_void,
        size: u32,
    }
    const KMTQAITYPE_ADAPTERTYPE: u32 = 15;
    type Enum2 = unsafe extern "system" fn(*mut EnumAdapters2) -> i32;
    type Query = unsafe extern "system" fn(*const QueryAdapterInfo) -> i32;
    type Close = unsafe extern "system" fn(*const u32) -> i32;

    let gdi = LoadLibraryA(c"gdi32.dll".as_ptr().cast());
    if gdi.is_null() {
        return false;
    }
    let (e, q, c) = (
        GetProcAddress(gdi, c"D3DKMTEnumAdapters2".as_ptr().cast()),
        GetProcAddress(gdi, c"D3DKMTQueryAdapterInfo".as_ptr().cast()),
        GetProcAddress(gdi, c"D3DKMTCloseAdapter".as_ptr().cast()),
    );
    if e.is_null() || q.is_null() || c.is_null() {
        return false;
    }
    let (enum2, query, close): (Enum2, Query, Close) =
        (core::mem::transmute(e), core::mem::transmute(q), core::mem::transmute(c));
    let mut adapters = [AdapterInfo::default(); 16];
    let mut arg = EnumAdapters2 { num_adapters: adapters.len() as u32, adapters: adapters.as_mut_ptr() };
    if enum2(&mut arg) < 0 {
        return false;
    }
    let mut supported = false;
    for a in &adapters[..(arg.num_adapters as usize).min(adapters.len())] {
        // D3DKMT_ADAPTERTYPE: bit 0 RenderSupported, bit 2 SoftwareDevice.
        let mut kind: u32 = 0;
        let qa = QueryAdapterInfo {
            h_adapter: a.h_adapter,
            kind: KMTQAITYPE_ADAPTERTYPE,
            data: (&mut kind as *mut u32).cast(),
            size: 4,
        };
        let hardware_render = query(&qa) >= 0 && kind & 1 != 0 && kind & 4 == 0;
        if hardware_render {
            let mut df: i32 = 0;
            let qd = QueryAdapterInfo {
                h_adapter: a.h_adapter,
                kind: kind_query,
                data: (&mut df as *mut i32).cast(),
                size: 4,
            };
            if query(&qd) >= 0 && df & 1 != 0 {
                supported = true;
            }
        }
        close(&a.h_adapter);
    }
    supported
}

pub(crate) unsafe extern "system" fn clear_view_11_1(
    h: Hdevice,
    view_type: ddi::D3D11DDI_HANDLETYPE,
    view: *mut c_void,
    color: *const f32,
    rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    if D3D11_1_LOG_COUNT.first_n(64).is_some() {
        trace_line!("DDI D3D11.1 ClearView: type={view_type} rects={num_rects}");
    }
    if view_type != ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RENDERTARGETVIEW {
        // Already loud -- this arm logs. `bump()` instead of
        // `note_ddi_refusal`, which would add a SECOND line for the same event.
        // R911 is explicit about not doing that here, and `bump` is the named
        // form of exactly that intent (stage S2).
        DDI_REFUSALS.clear_view_unsupported.bump();
        log_error!("DDI D3D11.1 ClearView UNSUPPORTED view type {view_type} — clear dropped");
        return;
    }
    let Some(context) = d3d11_context1(h) else {
        return;
    };
    let Some(rtv) = load_rtv_at(view) else {
        return;
    };
    let Ok(view) = (*rtv).cast::<ID3D11View>() else {
        return;
    };
    let rgba = if color.is_null() {
        [0.0; 4]
    } else {
        [*color, *color.add(1), *color.add(2), *color.add(3)]
    };
    // A rect-limited ClearView clears ONLY the given sub-rects. The previous
    // ClearRenderTargetView forwarding cleared the WHOLE view: dwm's flip
    // composition issues ClearView(dirty-rect) each frame before redrawing
    // just that region, so every frame the accumulated desktop was wiped to
    // the (transparent-black) clear color and only the delta survived — the
    // all-zero presented-frame class. D3D10_DDI_RECT is layout-identical to
    // RECT; DXVK implements ID3D11DeviceContext1::ClearView incl. rects.
    if num_rects != 0 && !rects.is_null() {
        let rects = core::slice::from_raw_parts(
            rects as *const windows::Win32::Foundation::RECT,
            num_rects as usize,
        );
        context.ClearView(&view, &rgba, Some(rects));
    } else {
        context.ClearView(&view, &rgba, None);
    }
}

pub(crate) unsafe extern "system" fn resource_update_subresource(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
) {
    let Some(context) = d3d11_context(h) else {
        return;
    };
    let Some(res) = load_resource(h_res) else {
        if HANDLE_MISS_LOG_COUNT.first_n(256).is_some() {
            log_error!(
                "DDI UpdateSubresource missing resource hpriv={:p} sub={} data={:p}",
                h_res.pDrvPrivate,
                subresource,
                data
            );
        }
        return;
    };
    // Untraced: nothing below the trace gate is needed, so the allocation
    // read (a dependent load into the resource slot) and the two counters
    // are skipped on this per-draw path.
    if crate::trace_enabled() {
        trace_update_subresource(h_res, subresource, box_, data, row_pitch, depth_pitch);
    }
    let bx;
    let bx_ptr = if box_.is_null() {
        None
    } else {
        let b = &*box_;
        bx = D3D11_BOX {
            left: b.left as u32,
            top: b.top as u32,
            front: b.front as u32,
            right: b.right as u32,
            bottom: b.bottom as u32,
            back: b.back as u32,
        };
        Some(&bx as *const D3D11_BOX)
    };
    context.UpdateSubresource(&*res, subresource, bx_ptr, data, row_pitch, depth_pitch);
}

/// The UpdateSubresource trace line (UmdTrace only).
unsafe fn trace_update_subresource(
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
) {
    let alloc = resource_allocation(h_res);
    let n = UPDATE_LOG_COUNT.next();
    // DECLARED diagnostic change: the old gate's `|| alloc != 0` disjunct
    // removed the rate cap entirely for exactly the shared/primary/present
    // resources that update most often, so a steady stream of updates to a
    // WDDM-allocated texture wrote one 21-argument formatted line per call.
    // Allocation-backed updates stay observable without being unbounded, and
    // what is no longer emitted is counted rather than silently dropped.
    let rate_ok = n < 1024 || (alloc != 0 && n % 512 == 0);
    if !rate_ok {
        UPDATE_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
    }
    if crate::trace_enabled() && rate_ok {
        // Three QueryInterface casts and a GetDesc: only for the log line,
        // never on the untraced path (profiled at ~3 % of Heaven's render
        // thread when it ran per call).
        let (_, kind, width, height, depth, fmt) = resource_summary(h_res);
        let (rt_resource, km_resource) = resource_parent_handles(h_res);
        let (box_left, box_top, box_right, box_bottom) = if box_.is_null() {
            (
                0i32,
                0i32,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
            )
        } else {
            let b = &*box_;
            (b.left, b.top, b.right, b.bottom)
        };
        let source_width = u32::try_from(box_right.saturating_sub(box_left)).unwrap_or(0);
        let source_height = u32::try_from(box_bottom.saturating_sub(box_top)).unwrap_or(0);
        let source_samples = if !data.is_null()
            && kind == "tex2d"
            && matches!(fmt, 28 | 87 | 88)
            && source_width != 0
            && source_height != 0
            && row_pitch >= source_width.saturating_mul(4)
        {
            let center_offset = (source_height as usize / 2)
                .saturating_mul(row_pitch as usize)
                .saturating_add((source_width as usize / 2).saturating_mul(4));
            Some((
                core::ptr::read_unaligned(data.cast::<u32>()),
                core::ptr::read_unaligned((data as *const u8).add(center_offset).cast::<u32>()),
            ))
        } else {
            None
        };
        trace_line!(
            "DDI UpdateSubresource #{} hDrv={:p} hRT={:p} hKM=0x{:x} alloc=0x{:x} \
             kind={} dims={}x{}x{} fmt={} sub={} box={},{},{},{} data={:p} \
             row_pitch={} depth_pitch={} sample0={} sample_center={}",
            n,
            h_res.pDrvPrivate,
            rt_resource,
            km_resource,
            alloc,
            kind,
            width,
            height,
            depth,
            fmt,
            subresource,
            box_left,
            box_top,
            box_right,
            box_bottom,
            data,
            row_pitch,
            depth_pitch,
            source_samples
                .map(|samples| format!("0x{:08x}", samples.0))
                .unwrap_or_else(|| "n/a".to_string()),
            source_samples
                .map(|samples| format!("0x{:08x}", samples.1))
                .unwrap_or_else(|| "n/a".to_string()),
        );
        if UPDATE_SUPPRESSED.load(Ordering::Relaxed) != 0 {
            trace_line!(
                "DDI UpdateSubresource: {} update lines suppressed by the rate cap so far",
                UPDATE_SUPPRESSED.load(Ordering::Relaxed)
            );
        }
    }
}

pub(crate) unsafe extern "system" fn resource_update_subresource_11_1(
    h: Hdevice,
    h_res: ddi::D3D10DDI_HRESOURCE,
    subresource: u32,
    box_: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
    _copy_flags: u32,
) {
    resource_update_subresource(h, h_res, subresource, box_, data, row_pitch, depth_pitch);
}

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
    let dst_summary = resource_summary(h_dst);
    let src_summary = resource_summary(h_src);
    let (dst_rt, dst_km) = resource_parent_handles(h_dst);
    let (src_rt, src_km) = resource_parent_handles(h_src);
    let n = COPY_REGION_LOG_COUNT.next();
    if n < 1024 || dst.is_none() || src.is_none() {
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
    if let Some(context) = d3d11_context(h) {
        context.Flush();
        flush_gate(h, &context);
    }
}

static FLUSH_GATE_SENT: [AtomicUsize; 3] =
    [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];
static FLUSH_GATE_RENDER_FAILED: AtomicUsize = AtomicUsize::new(0);
static FLUSH_GATE_FALLBACK: AtomicUsize = AtomicUsize::new(0);

/// The flush gate (docs/flush-gate.md, shared-surfaces.md section 4): on a
/// flush of a device that holds a cross-process shared resource, and only when
/// work was recorded since the previous gate, one `HEFL` render packet on the
/// device's own context whose WDDM fence retires when that work is done on the
/// GPU, so the runtime's keyed-mutex release (right after this `pfnFlush`,
/// ordered by dxgkrnl against our last packet) cannot overtake it.
///
/// Carriers, each gated on its KMD capability: NVK an RM fence for the
/// submitted batch (NVRM caps bit 34); Venus a point of the present stream
/// signalled behind the recorded work (scanout probe bit 5), else the wire
/// rung after the work reached the transport. Without a carrier: the CPU wait
/// (`nvk_keyed_flush_wait`; NVK by default, Venus with
/// `HELIOS_KEYED_FLUSH_WAIT=1`). A failed packet never fails the flush.
pub(crate) unsafe fn flush_gate(h: Hdevice, context: &ID3D11DeviceContext) {
    use crate::bridge::{FlushGatePoint, FLUSH_GATE_RM_FENCE, FLUSH_GATE_STREAM, FLUSH_GATE_WIRE};
    let Some(dev) = helios_device(h) else {
        return;
    };
    if lock_ignore_poison(&dev.nvk_keyed_resources).is_empty() {
        return;
    }
    if dev.dxvk.is_nvk() {
        if crate::knobs::nvk_rm_fence() && crate::scanout_acquire::nvrm_flush_gate_capable(dev) {
            match dev.dxvk.flush_gate_point(FLUSH_GATE_RM_FENCE) {
                FlushGatePoint::Nothing => return,
                FlushGatePoint::Ready { fence, fence_value, .. } if fence != 0 => {
                    // The handle is the KMD's from here on, whatever happens.
                    send_flush_gate(
                        dev,
                        helios_protocol::HELIOS_FLUSH_GATE_FLAG_RM_FENCE,
                        (0, 0, 0),
                        Some((fence, fence_value)),
                    );
                    return;
                }
                _ => {}
            }
        }
        FLUSH_GATE_FALLBACK.fetch_add(1, Ordering::Relaxed);
        nvk_keyed_flush_wait(h, context);
        return;
    }
    if crate::scanout_acquire::flush_gate_capable() {
        match dev.dxvk.flush_gate_point(FLUSH_GATE_STREAM) {
            FlushGatePoint::Nothing => return,
            FlushGatePoint::Ready { ctx, value, cookie, .. } if ctx != 0 && cookie != 0 => {
                send_flush_gate(
                    dev,
                    helios_protocol::HELIOS_FLUSH_GATE_FLAG_STREAM,
                    (ctx, value, cookie),
                    None,
                );
                return;
            }
            _ => {}
        }
        // No stream point: the wire rung once the work reached the transport.
        match dev.dxvk.flush_gate_point(FLUSH_GATE_WIRE) {
            FlushGatePoint::Nothing => return,
            FlushGatePoint::Ready { .. } => {
                send_flush_gate(dev, 0, (0, 0, 0), None);
                return;
            }
            _ => {}
        }
    }
    FLUSH_GATE_FALLBACK.fetch_add(1, Ordering::Relaxed);
    nvk_keyed_flush_wait(h, context);
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
pub(crate) unsafe fn nvk_keyed_flush_wait(h: Hdevice, context: &ID3D11DeviceContext) {
    let Some(dev) = helios_device(h) else {
        return;
    };
    if (!dev.dxvk.is_nvk() && !keyed_flush_wait_forced())
        || lock_ignore_poison(&dev.nvk_keyed_resources).is_empty()
    {
        return;
    }
    let Some(device) = dev.dxvk.d3d11_device() else {
        return;
    };
    let desc = windows::Win32::Graphics::Direct3D11::D3D11_QUERY_DESC {
        Query: windows::Win32::Graphics::Direct3D11::D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query = None;
    if device.CreateQuery(&desc, Some(&mut query)).is_err() {
        return;
    }
    let Some(query) = query else {
        return;
    };
    context.End(&query);
    let start = std::time::Instant::now();
    let mut done: windows::Win32::Foundation::BOOL = Default::default();
    loop {
        // Raw HRESULT: S_FALSE (not yet) must not read as success. Flags 0
        // flushes, so the query itself is submitted.
        let hr = (Interface::vtable(context).GetData)(
            Interface::as_raw(context),
            Interface::as_raw(&query),
            (&mut done as *mut windows::Win32::Foundation::BOOL).cast(),
            core::mem::size_of::<windows::Win32::Foundation::BOOL>() as u32,
            0,
        );
        if hr.0 == 0 && done.as_bool() {
            break;
        }
        if hr.0 < 0 {
            log_error!("DDI NVK keyed-mutex flush wait: GetData 0x{:08x}", hr.0 as u32);
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            log_error!("DDI NVK keyed-mutex flush wait: GPU not done after 5 s, releasing anyway");
            break;
        }
        std::thread::yield_now();
    }
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
    _resource1: ddi::D3D10DDI_HRESOURCE,
    _resource2: ddi::D3D10DDI_HRESOURCE,
    flags: u32,
    supported: *mut ddi::BOOL,
) {
    if !supported.is_null() {
        *supported = 0;
    }
    if D3D11_1_LOG_COUNT.first_n(64).is_some() {
        log_error!("DDI D3D11.1 CheckDirectFlipSupport: flags=0x{flags:x} -> no");
    }
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
    // `alloc` selects the gate below, so the summary read stays out here; every
    // other operand is log-only and now lives inside it. The two
    // `read_unaligned` probes in particular are two dependent cache misses into
    // the CALLER's buffer, and they used to be paid on every BGRA/RGBA tex2d
    // update purely to produce a log field.
    let (alloc, kind, width, height, depth, fmt) = resource_summary(h_res);
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

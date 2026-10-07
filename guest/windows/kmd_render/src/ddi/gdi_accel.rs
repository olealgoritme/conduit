//! GDI hardware acceleration limited to what the KMD executes (`GdiAccel`, lane F fallback A):
//! the I/O half of the caps and of `DxgkDdiRenderKm`. The pure rules are
//! `helios_kmd_logic::gdi_accel`; the executor is `ddi/gdi_exec.rs`; the design, the sources and
//! the hardware procedure are `docs/vram-redirection.md` section 10.
//!
//! KNOB. `GdiAccel` (read with the other caps knobs at AddAdapter and StartDevice, `AdapterKnobs`).
//! Anything but 1: the reported `PresentationCaps` word is 0 exactly as before, `on()` is one
//! relaxed load that answers false, `DxgkDdiRenderKm` keeps its pass-through body, nothing is
//! counted or written.
//!
//! WITH THE KNOB AT 1: the caps word of `gdi_accel::ACCEL_CAPS` is reported, so Windows may move
//! GDI redirection to GDI `TEXTURE` surfaces and send GDI operations as kernel-mode command
//! buffers. Every `DxgkDdiRenderKm` buffer is parsed, each command's surfaces are resolved through
//! the allocation list and classified (VRAM with `RedirVram`, a KMD standard buffer reachable by
//! the CPU, or unreachable), the engine is planned, the sub-rectangles are materialised, and the
//! whole buffer becomes one job of `ddi/gdi_exec.rs`, named in the DMA buffer's private data; the
//! referenced allocations go into the output patch list and a 16-byte marker is the DMA buffer.
//! SubmitCommand admits the job and gates the fence on it; the HPD worker executes it.
//!
//! IRQL. `DxgkDdiRenderKm` is PASSIVE (WDK), so the registry mirrors may be written from it
//! (throttled); nothing here is touched at DISPATCH.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;

use helios_kmd_logic::gdi_accel::{self as ga, Cmd, Rect, Surface, SurfaceClass};

use crate::adapter::AdapterContext;
use crate::ddi::gdi_exec as gx;
use crate::dxgk::*;

// ---- counters (`helios_kmd_logic::gdi_accel::COUNTERS`, written here and in `gdi_exec.rs`) ------

static ON: AtomicU32 = AtomicU32::new(0);
static CAPS: AtomicU32 = AtomicU32::new(0);
static CMD_N: AtomicU32 = AtomicU32::new(0);
static OP_N: AtomicU32 = AtomicU32::new(0);
static BAD: AtomicU32 = AtomicU32::new(0);
static BAD_WHY: AtomicU32 = AtomicU32::new(0);
static OP_MASK: AtomicU32 = AtomicU32::new(0);
/// Entries into `DxgkDdiRenderKm` / `DxgkDdiRenderGdi` with the knob on, before any parsing.
static RK_IN: AtomicU32 = AtomicU32::new(0);
static RG_IN: AtomicU32 = AtomicU32::new(0);
/// `DxgkDdiCreateDevice` with `GdiDevice`, `DxgkDdiCreateContext` with `GdiContext` (the knob on
/// or not: counted always, mirrored with the knob on), and the last GDI context's raw flags.
static DEV_N: AtomicU32 = AtomicU32::new(0);
static CTX_N: AtomicU32 = AtomicU32::new(0);
static CTX_FLAGS: AtomicU32 = AtomicU32::new(0);
/// Submissions on a GDI context (knob on), private records decoded there, jobs claimed by context
/// instead (record missing), and the private sizes seen: RenderGdi/RenderKm's in the low 16 bits,
/// SubmitCommand's in the high 16; SubmitCommand's UMD prefix size.
static SUB_N: AtomicU32 = AtomicU32::new(0);
static PRV_OK: AtomicU32 = AtomicU32::new(0);
static CTX_CLAIM: AtomicU32 = AtomicU32::new(0);
static PRV_SZ: AtomicU32 = AtomicU32::new(0);
static PRV_UMD: AtomicU32 = AtomicU32::new(0);

/// The GDI contexts alive (their `hContext` values), for SubmitCommand's by-context claim.
const GDI_CTX_SLOTS: usize = 32;
static GDI_CTX: [core::sync::atomic::AtomicUsize; GDI_CTX_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; GDI_CTX_SLOTS];

/// Is `h` a live GDI context. Lock-free, any IRQL.
pub(crate) fn is_gdi_context(h: usize) -> bool {
    h != 0 && GDI_CTX.iter().any(|s| s.load(Ordering::Acquire) == h)
}

/// DestroyContext: forget `h`. Atomics only.
pub(crate) fn forget_context(h: usize) {
    for s in GDI_CTX.iter() {
        let _ = s.compare_exchange(h, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// SubmitCommand census (any IRQL; atomics).
pub(crate) fn note_submit(private_size: u32, umd: u32, decoded: bool, claimed: bool) {
    SUB_N.fetch_add(1, Ordering::Relaxed);
    PRV_SZ.store((PRV_SZ.load(Ordering::Relaxed) & 0xffff) | (private_size.min(0xffff) << 16), Ordering::Relaxed);
    PRV_UMD.store(umd, Ordering::Relaxed);
    if decoded {
        PRV_OK.fetch_add(1, Ordering::Relaxed);
    }
    if claimed {
        CTX_CLAIM.fetch_add(1, Ordering::Relaxed);
    }
}
static ROP_MASK: AtomicU32 = AtomicU32::new(0);
pub(crate) static DROP: AtomicU32 = AtomicU32::new(0);
pub(crate) static WHY: AtomicU32 = AtomicU32::new(0);
pub(crate) static MASK: AtomicU32 = AtomicU32::new(0);

/// `GdiAccel` is 1 for this start. One relaxed load.
#[inline]
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed) != 0
}

/// The caps word for a knob snapshot (`query_adapter_info`), pure.
pub(crate) fn reported_caps(knob: u32) -> u32 {
    ga::resolve_caps(knob).reported
}

/// StartDevice (PASSIVE), from `AdapterKnobs::read_at_start`: latch the knob, zero the counters,
/// and with the knob on write the mirrors. With the knob off nothing is written.
pub(crate) fn note_start(knob: u32) {
    let caps = ga::resolve_caps(knob);
    for c in [
        &CMD_N, &OP_N, &BAD, &BAD_WHY, &OP_MASK, &ROP_MASK, &DROP, &WHY, &MASK, &RK_IN, &RG_IN, &SUB_N,
        &PRV_OK, &CTX_CLAIM, &PRV_SZ, &PRV_UMD,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    CAPS.store(caps.reported, Ordering::Relaxed);
    ON.store(u32::from(caps.on), Ordering::Relaxed);
    crate::ddi::gdi_exec::reset_for_start(caps.on);
    if caps.on {
        publish_counters();
    } else if caps.reported != 0 {
        // A one-bit experiment (knob 2 or 3): only the word is mirrored.
        crate::diag::record_named_bytes(b"GdiKnob", knob);
        crate::diag::record_named_bytes(b"GdiCaps", caps.reported);
    }
}

/// Mirror the counters (PASSIVE only). With the knob off: nothing.
pub(crate) fn publish_counters() {
    if !on() {
        return;
    }
    let w = crate::diag::record_named_bytes;
    w(b"GdiKnob", 1);
    w(b"GdiCaps", CAPS.load(Ordering::Relaxed));
    w(b"GdiCmdN", CMD_N.load(Ordering::Relaxed));
    w(b"GdiOpN", OP_N.load(Ordering::Relaxed));
    w(b"GdiBad", BAD.load(Ordering::Relaxed));
    w(b"GdiBadWhy", BAD_WHY.load(Ordering::Relaxed));
    w(b"GdiOpMask", OP_MASK.load(Ordering::Relaxed));
    w(b"GdiRopMask", ROP_MASK.load(Ordering::Relaxed));
    w(b"GdiDrop", DROP.load(Ordering::Relaxed));
    w(b"GdiWhy", WHY.load(Ordering::Relaxed));
    w(b"GdiMask", MASK.load(Ordering::Relaxed));
    w(b"GdiRkIn", RK_IN.load(Ordering::Relaxed));
    w(b"GdiRgIn", RG_IN.load(Ordering::Relaxed));
    w(b"GdiDevN", DEV_N.load(Ordering::Relaxed));
    w(b"GdiCtxN", CTX_N.load(Ordering::Relaxed));
    w(b"GdiCtxFl", CTX_FLAGS.load(Ordering::Relaxed));
    w(b"GdiSubN", SUB_N.load(Ordering::Relaxed));
    w(b"GdiPrvOk", PRV_OK.load(Ordering::Relaxed));
    w(b"GdiCtxClm", CTX_CLAIM.load(Ordering::Relaxed));
    w(b"GdiPrvSz", PRV_SZ.load(Ordering::Relaxed));
    w(b"GdiPrvUmd", PRV_UMD.load(Ordering::Relaxed));
    crate::ddi::gdi_exec::publish_counters();
}

/// `DxgkDdiCreateDevice` (PASSIVE): `flags` is `DXGK_CREATEDEVICEFLAGS.Value`. Bit 1 `GdiDevice`.
/// Counted with the knob off too (an atomic add), mirrored only with it on.
pub(crate) fn note_create_device(flags: u32) {
    if flags & 2 != 0 {
        DEV_N.fetch_add(1, Ordering::Relaxed);
        if on() {
            crate::diag::record_named_bytes(b"GdiDevN", DEV_N.load(Ordering::Relaxed));
        }
    }
}

/// `DxgkDdiCreateContext` (PASSIVE): `flags` is `DXGK_CREATECONTEXTFLAGS.Value`. Bit 1
/// `GdiContext` (bit 2 `VirtualAddressing`: such a context gets `DxgkDdiRenderGdi`, not RenderKm).
pub(crate) fn note_create_context(flags: u32, h: usize) {
    if flags & 2 != 0 {
        CTX_N.fetch_add(1, Ordering::Relaxed);
        CTX_FLAGS.store(flags, Ordering::Relaxed);
        for s in GDI_CTX.iter() {
            if s.compare_exchange(0, h, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                break;
            }
        }
        if on() {
            crate::diag::record_named_bytes(b"GdiCtxN", CTX_N.load(Ordering::Relaxed));
            crate::diag::record_named_bytes(b"GdiCtxFl", flags);
        }
    }
}

/// Record a reason a command is off the copy engine.
pub(crate) fn note_why(why: ga::Why) {
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
}

/// Bytes of the DMA buffer one RenderKm writes: the private record's encoding, as a marker a
/// debugger can recognise (the KMD never reads it back: SubmitCommand reads the private data).
const DMA_MARKER_BYTES: usize = ga::PRIVATE_BYTES;

/// The most distinct allocation indices one buffer may reference that the patch list tracks; a
/// GDI context's lists are `DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT` long.
const MAX_REFS: usize = 256;

/// Resolve one allocation-list index to a [`Surface`].
///
/// # Safety
/// `list` is dxgkrnl's allocation list of `len` entries for this RenderKm call.
unsafe fn surface_at(
    adapter: &AdapterContext,
    list: *const DXGK_ALLOCATIONLIST,
    len: u32,
    index: u32,
) -> Option<Surface> {
    if list.is_null() || index >= len {
        return None;
    }
    // SAFETY: `index < len` entries of dxgkrnl's list.
    let entry = unsafe { &*list.add(index as usize) };
    let h = entry.hDeviceSpecificAllocation;
    if h.is_null() {
        return None;
    }
    // SAFETY: a handle dxgkrnl round-trips from our OpenAllocation, as for a Present.
    let info = unsafe { crate::ddi::create_allocation::present_alloc_info(Some(adapter), h) }?;
    let class = if crate::ddi::gdi_ce_glue::is_vram(info.resource_id) {
        SurfaceClass::Vram
    } else if info.storage == crate::ddi::create_allocation::PresentAllocationStorage::PitchedStandardBuffer {
        // A KMD standard buffer (staging, shadow, lookup table): its authoritative CPU view is
        // reachable whether VidMm holds it in system pages or in the Venus window.
        SurfaceClass::System
    } else {
        SurfaceClass::Unreachable
    };
    Some(Surface {
        resource_id: info.resource_id,
        width: info.width,
        height: info.height,
        pitch: info.pitch,
        class,
    })
}

fn note_opcode(cmd: &Cmd) {
    OP_MASK.fetch_or(1 << cmd.opcode(), Ordering::Relaxed);
    match *cmd {
        Cmd::BitBlt { rop, .. } => {
            ROP_MASK.fetch_or(1 << (rop as u32 & 7), Ordering::Relaxed);
        }
        Cmd::ColorFill { rop, .. } => {
            ROP_MASK.fetch_or(1 << (8 + (rop as u32 & 7)), Ordering::Relaxed);
        }
        _ => {}
    }
}

/// The fields `DxgkDdiRenderKm` (`DXGKARG_RENDER`) and `DxgkDdiRenderGdi` (`DXGKARG_RENDERGDI`)
/// share; the second has no patch lists (GPU virtual addressing: nothing to patch).
struct Call<'a> {
    p_command: *const c_void,
    command_length: u32,
    p_dma_buffer: &'a mut *mut c_void,
    dma_size: u32,
    p_private: *mut c_void,
    private_size: u32,
    p_allocation_list: *const DXGK_ALLOCATIONLIST,
    allocation_list_size: u32,
    /// The output patch list's cursor and room (RenderKm only).
    patch_out: Option<(&'a mut *mut D3DDDI_PATCHLOCATIONLIST, u32)>,
    multipass: &'a mut u32,
}

/// `DxgkDdiRenderKm` with `GdiAccel` = 1 (PASSIVE).
///
/// # Safety
/// `h_context` and `args` are dxgkrnl's for this call; the command buffer, the DMA buffer, the
/// private data and the lists are valid for the sizes it states.
pub(crate) unsafe fn render_km(h_context: HANDLE, args: &mut DXGKARG_RENDER) -> NTSTATUS {
    RK_IN.fetch_add(1, Ordering::Relaxed);
    let room = if args.pPatchLocationListOut.is_null() { 0 } else { args.PatchLocationListOutSize };
    let call = Call {
        p_command: args.pCommand,
        command_length: args.CommandLength,
        p_dma_buffer: &mut args.pDmaBuffer,
        dma_size: args.DmaSize,
        p_private: args.pDmaBufferPrivateData,
        private_size: args.DmaBufferPrivateDataSize,
        p_allocation_list: args.pAllocationList as *const DXGK_ALLOCATIONLIST,
        allocation_list_size: args.AllocationListSize,
        patch_out: Some((&mut args.pPatchLocationListOut, room)),
        multipass: &mut args.MultipassOffset,
    };
    // SAFETY: as the caller's.
    unsafe { translate(h_context, call) }
}

/// `DxgkDdiRenderGdi` with `GdiAccel` = 1 (PASSIVE): the GDI command buffer on a GPU-virtual-
/// addressing adapter. dxgkrnl's `ADAPTER_RENDER::DdiRenderGdi` calls this DDI, not RenderKm,
/// when the adapter reports GpuMmu (this one does): same `DXGK_RENDERKM_COMMAND` stream, no patch
/// lists, the DMA buffer's GPU VA in place of the segment list (WDK `DXGKARG_RENDERGDI`).
///
/// # Safety
/// As [`render_km`].
pub(crate) unsafe fn render_gdi(h_context: HANDLE, args: &mut DXGKARG_RENDERGDI) -> NTSTATUS {
    RG_IN.fetch_add(1, Ordering::Relaxed);
    let call = Call {
        p_command: args.pCommand,
        command_length: args.CommandLength,
        p_dma_buffer: &mut args.pDmaBuffer,
        dma_size: args.DmaSize,
        p_private: args.pDmaBufferPrivateData,
        private_size: args.DmaBufferPrivateDataSize,
        p_allocation_list: args.pAllocationList as *const DXGK_ALLOCATIONLIST,
        allocation_list_size: args.AllocationListSize,
        patch_out: None,
        multipass: &mut args.MultipassOffset,
    };
    // SAFETY: as the caller's.
    unsafe { translate(h_context, call) }
}

/// The translation both entry points share.
///
/// # Safety
/// As [`render_km`].
unsafe fn translate(h_context: HANDLE, args: Call<'_>) -> NTSTATUS {
    let n = CMD_N.fetch_add(1, Ordering::Relaxed) + 1;
    if (args.dma_size as usize) < DMA_MARKER_BYTES {
        return crate::ddi::present_packet::STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
    }
    // SAFETY: a live hContext from our CreateContext.
    let adapter = unsafe { crate::device::ContextHandleRef::from_raw(h_context) }
        .and_then(|c| c.adapter());

    // Distinct allocation indices referenced, for the patch list.
    let mut refs = [0u32; MAX_REFS];
    let mut nrefs = 0usize;
    fn add_ref(i: u32, refs: &mut [u32; MAX_REFS], nrefs: &mut usize) {
        if refs[..*nrefs].contains(&i) || *nrefs >= MAX_REFS {
            return;
        }
        refs[*nrefs] = i;
        *nrefs += 1;
    }

    let mut ops: Vec<gx::Op> = Vec::new();
    let len = args.command_length as usize;
    if len > 0 && !args.p_command.is_null() {
        // SAFETY: dxgkrnl's command buffer, `CommandLength` readable bytes (kernel memory, no
        // try/except needed per the DDI's remarks).
        let bytes = unsafe { core::slice::from_raw_parts(args.p_command as *const u8, len) };
        let mut parser = ga::Parser::new(bytes, args.p_command as u64);
        let list = args.p_allocation_list;
        let list_len = args.allocation_list_size;
        while let Some(next) = parser.next_cmd() {
            let cmd = match next {
                Ok(cmd) => cmd,
                Err(bad) => {
                    BAD.fetch_add(1, Ordering::Relaxed);
                    BAD_WHY.store(bad.code(), Ordering::Relaxed);
                    break;
                }
            };
            OP_N.fetch_add(1, Ordering::Relaxed);
            note_opcode(&cmd);
            let (dst_i, src_i) = cmd.indices();
            for i in [dst_i, src_i[0], src_i[1]].into_iter().flatten() {
                if i < list_len {
                    add_ref(i, &mut refs, &mut nrefs);
                }
            }
            let resolve = |i: Option<u32>| -> Option<Surface> {
                let a = adapter?;
                // SAFETY: dxgkrnl's list of `list_len` entries.
                i.and_then(|i| unsafe { surface_at(a, list, list_len, i) })
            };
            let dst = resolve(dst_i);
            let srcs = [resolve(src_i[0]), resolve(src_i[1])];
            let (engine, why) = ga::plan(&cmd, dst.as_ref(), [srcs[0].as_ref(), srcs[1].as_ref()]);
            if let Some(w) = why {
                note_why(w);
            }
            gx::note_census(&cmd, dst.as_ref(), [srcs[0].as_ref(), srcs[1].as_ref()]);
            if matches!(cmd, Cmd::Escape) {
                continue;
            }
            // The destination sub-rectangles, materialised and clipped.
            let mut subs: Vec<Rect> = Vec::new();
            let dst_rect = cmd.dst_rect();
            let count = cmd.subs().count() as usize;
            if subs.try_reserve_exact(count.max(1)).is_err() {
                DROP.fetch_add(1, Ordering::Relaxed);
                note_why(ga::Why::CpuFailed);
                continue;
            }
            match cmd.subs() {
                ga::SubRects::None => subs.push(gx::clip_sub(&dst_rect, &dst_rect, dst.as_ref())),
                ga::SubRects::Inline { offset, count } => {
                    for i in 0..count {
                        if let Some(r) = ga::inline_rect(bytes, offset, i) {
                            subs.push(gx::clip_sub(&r, &dst_rect, dst.as_ref()));
                        }
                    }
                }
                ga::SubRects::External { ptr, count } => {
                    // SAFETY: dxgkrnl's kernel array of `count` RECTs (bounded by the parser's
                    // MAX_SUB_RECTS); kernel buffers need no try/except (DDI remarks).
                    let raw = unsafe {
                        core::slice::from_raw_parts(ptr as *const u8, count as usize * ga::layout::RECT_BYTES)
                    };
                    for i in 0..count {
                        if let Some(r) = ga::rect_at(raw, i) {
                            subs.push(gx::clip_sub(&r, &dst_rect, dst.as_ref()));
                        }
                    }
                }
            }
            subs.retain(|r| !r.is_empty());
            if ops.try_reserve(1).is_err() {
                DROP.fetch_add(1, Ordering::Relaxed);
                note_why(ga::Why::CpuFailed);
                continue;
            }
            ops.push(gx::Op { cmd, dst, srcs, engine, why, subs });
        }
    }

    // The output patch list: one reference per allocation the buffer uses (dxgkrnl's contract:
    // "insert all the references to allocations into the output patch-location list"). The
    // decorative GpuMmu has nothing to patch (`DxgkDdiPatch` is a no-op), the entries only keep
    // the list honest.
    // RenderGdi (GPU virtual addressing) has no patch list.
    let Call { p_dma_buffer, p_private, private_size, patch_out, multipass, .. } = args;
    if let Some((cursor, room)) = patch_out {
        if nrefs > room as usize || (nrefs > 0 && cursor.is_null()) {
            // Nothing committed yet: dxgkrnl retries with fresh lists and the commands are parsed
            // again.
            return crate::ddi::present_packet::STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
        }
        for (k, &index) in refs[..nrefs].iter().enumerate() {
            // SAFETY: `k < nrefs <= PatchLocationListOutSize` entries of dxgkrnl's output list.
            unsafe {
                let patch = cursor.add(k);
                core::ptr::write_bytes(patch, 0, 1);
                (*patch).AllocationIndex = index;
            }
        }
        if nrefs > 0 {
            // SAFETY: advancing within the list by the entries written.
            *cursor = unsafe { cursor.add(nrefs) };
        }
    }

    // The private data: a RenderKm buffer carries no Present, flip or execution record. dxgkrnl
    // recycles DMA buffers, so the whole private range is cleared first (a stale `HPBL` prefix
    // would gate this buffer's fence on an old copy), then the job record is written. Job 0 (no
    // command to run, or the table refused it) makes SubmitCommand gate nothing.
    let job = if ops.is_empty() {
        0
    } else {
        let n_ops = ops.len() as u32;
        let id = gx::commit(ops, h_context as usize);
        if id == 0 {
            DROP.fetch_add(n_ops, Ordering::Relaxed);
            note_why(ga::Why::CpuFailed);
        }
        id
    };
    let rec = ga::Private { job }.encode();
    PRV_SZ.store((PRV_SZ.load(Ordering::Relaxed) & !0xffff) | private_size.min(0xffff), Ordering::Relaxed);
    if !p_private.is_null() {
        let size = private_size as usize;
        // SAFETY: dxgkrnl's private data of `size` writable bytes.
        unsafe {
            core::ptr::write_bytes(p_private as *mut u8, 0, size);
            if size >= rec.len() {
                core::ptr::copy_nonoverlapping(rec.as_ptr(), p_private as *mut u8, rec.len());
            }
        }
    }
    // SAFETY: `DmaSize >= DMA_MARKER_BYTES` writable bytes at `pDmaBuffer`.
    unsafe {
        core::ptr::copy_nonoverlapping(rec.as_ptr(), *p_dma_buffer as *mut u8, DMA_MARKER_BYTES);
        *p_dma_buffer = (*p_dma_buffer as *mut u8).add(DMA_MARKER_BYTES) as *mut c_void;
    }
    *multipass = 0;

    if n == 1 || n % 64 == 0 {
        publish_counters();
    }
    STATUS_SUCCESS
}

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
    for c in [&CMD_N, &OP_N, &BAD, &BAD_WHY, &OP_MASK, &ROP_MASK, &DROP, &WHY, &MASK] {
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
    crate::ddi::gdi_exec::publish_counters();
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

/// `DxgkDdiRenderKm` with `GdiAccel` = 1 (PASSIVE).
///
/// # Safety
/// `h_context` and `args` are dxgkrnl's for this call; the command buffer, the DMA buffer, the
/// private data and the lists are valid for the sizes it states.
pub(crate) unsafe fn render_km(h_context: HANDLE, args: &mut DXGKARG_RENDER) -> NTSTATUS {
    let n = CMD_N.fetch_add(1, Ordering::Relaxed) + 1;
    if (args.DmaSize as usize) < DMA_MARKER_BYTES {
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
    let len = args.CommandLength as usize;
    if len > 0 && !args.pCommand.is_null() {
        // SAFETY: dxgkrnl's command buffer, `CommandLength` readable bytes (kernel memory, no
        // try/except needed per the DDI's remarks).
        let bytes = unsafe { core::slice::from_raw_parts(args.pCommand as *const u8, len) };
        let mut parser = ga::Parser::new(bytes, args.pCommand as u64);
        let list = args.pAllocationList as *const DXGK_ALLOCATIONLIST;
        let list_len = args.AllocationListSize;
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
    let room = if args.pPatchLocationListOut.is_null() { 0 } else { args.PatchLocationListOutSize as usize };
    if nrefs > room {
        // Nothing committed yet: dxgkrnl retries with fresh lists and the commands are parsed again.
        return crate::ddi::present_packet::STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER;
    }
    for (k, &index) in refs[..nrefs].iter().enumerate() {
        // SAFETY: `k < nrefs <= PatchLocationListOutSize` entries of dxgkrnl's output list.
        unsafe {
            let patch = args.pPatchLocationListOut.add(k);
            core::ptr::write_bytes(patch, 0, 1);
            (*patch).AllocationIndex = index;
        }
    }
    if nrefs > 0 {
        // SAFETY: advancing within the list by the entries written.
        args.pPatchLocationListOut = unsafe { args.pPatchLocationListOut.add(nrefs) };
    }

    // The private data: a RenderKm buffer carries no Present, flip or execution record. dxgkrnl
    // recycles DMA buffers, so the whole private range is cleared first (a stale `HPBL` prefix
    // would gate this buffer's fence on an old copy), then the job record is written. Job 0 (no
    // command to run, or the table refused it) makes SubmitCommand gate nothing.
    let job = if ops.is_empty() {
        0
    } else {
        let n_ops = ops.len() as u32;
        let id = gx::commit(ops);
        if id == 0 {
            DROP.fetch_add(n_ops, Ordering::Relaxed);
            note_why(ga::Why::CpuFailed);
        }
        id
    };
    let rec = ga::Private { job }.encode();
    if !args.pDmaBufferPrivateData.is_null() {
        let size = args.DmaBufferPrivateDataSize as usize;
        // SAFETY: dxgkrnl's private data of `size` writable bytes.
        unsafe {
            core::ptr::write_bytes(args.pDmaBufferPrivateData as *mut u8, 0, size);
            if size >= rec.len() {
                core::ptr::copy_nonoverlapping(rec.as_ptr(), args.pDmaBufferPrivateData as *mut u8, rec.len());
            }
        }
    }
    // SAFETY: `DmaSize >= DMA_MARKER_BYTES` writable bytes at `pDmaBuffer`.
    unsafe {
        core::ptr::copy_nonoverlapping(rec.as_ptr(), args.pDmaBuffer as *mut u8, DMA_MARKER_BYTES);
        args.pDmaBuffer = (args.pDmaBuffer as *mut u8).add(DMA_MARKER_BYTES) as *mut c_void;
    }
    args.MultipassOffset = 0;

    if n == 1 || n % 64 == 0 {
        publish_counters();
    }
    STATUS_SUCCESS
}

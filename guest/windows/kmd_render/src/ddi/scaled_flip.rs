//! Scaled direct flip (`ScaledFlip`, default 0; `docs/independent-flip.md` section 14): a
//! single-plane multi-plane overlay (MPO) surface, so dxgkrnl can flip a full-screen swap chain
//! smaller than the mode straight to scan-out and the host scales it, instead of DWM composing
//! (and stretching) every frame.
//!
//! With the knob on:
//!
//! * the caps report `SupportMultiPlaneOverlay`, `MaxOverlayPlanes` = 1 and, when the flip caps
//!   carry `FlipImmediateMmIo`, `SupportMultiPlaneOverlayImmediateFlip` (`query_adapter_info.rs`);
//! * `DriverEntry` registers the three MPO3 DDIs of the WDDM 2.1 table (below);
//! * [`dxgkddi_check_mpo_support3`] answers with `helios_kmd_logic::scaled_flip::check`: one plane,
//!   the whole buffer as the source, the whole output or the aspect-correct rectangle as the
//!   destination, a 32-bit scan-out format, no rotation, mirror, blending or HDR colour space. A
//!   buffer smaller than the mode only when it is an NVK buffer (`ForeignFlip` shows it at its
//!   own size and the host scales it with its aspect ratio kept);
//! * [`dxgkddi_set_vidpn_source_address_mpo3`] turns a plane-0 flip into the legacy flip's path
//!   (`display::set_vidpn_source_address_common`) with the plane's allocation and address, and
//!   records its `PresentId`;
//! * [`signal_vsync`] retires plane flips the way dxgkrnl expects them to be retired once it uses
//!   the MPO DDIs: `DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2` naming the last completed
//!   `PresentId` of layer 0, looked up from the address the rest of the KMD reports
//!   (`helios_kmd_logic::scaled_flip::PresentBook`);
//! * `DxgkDdiControlInterrupt` takes the MPO vsync interrupt types as the same delivery gate.
//!
//! Nothing here touches the transport: the plane flip is the legacy flip from the KMD's point of
//! view, and the host receives what `ForeignFlip` sends today (the buffer at its own size).
//!
//! With the knob off (the default) dxgkrnl is told there is no overlay plane, "will not call
//! multiplane overlay functions" (`DXGK_DRIVERCAPS.SupportMultiPlaneOverlay`, WDK), the DDIs are
//! not registered, and every function here is one relaxed load.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::scaled_flip::{
    self as sf, Mode, Plane, Post, Rect, Source, VsyncReport, ANY_ADDRESS,
};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// The knob as read in `DriverEntry` (raw).
static KNOB: AtomicU32 = AtomicU32::new(0);
/// `SflPlanes` as read in `DriverEntry` (the count reported, `sf::planes_from_knob`).
static PLANES: AtomicU32 = AtomicU32::new(sf::PLANES_DEFAULT);
/// `SflDdis` as read in `DriverEntry` (raw).
static DDIS: AtomicU32 = AtomicU32::new(0);
/// The DRIVERCAPS replies: how many, the buffer size of the last, and the plane count it carried
/// (0: the knob was off or the field did not fit). Not zeroed at StartDevice: dxgkrnl may ask
/// before it.
static CAP_N: AtomicU32 = AtomicU32::new(0);
static CAP_SZ: AtomicU32 = AtomicU32::new(0);
static CAP_MPO: AtomicU32 = AtomicU32::new(0);
static OLD_CHK: AtomicU32 = AtomicU32::new(0);
/// The `PresentId`s of plane flips, by address.
static BOOK: sf::PresentBook = sf::PresentBook::new();
/// Which vsync interrupt types dxgkrnl has enabled: bit 0 `CRTC_VSYNC`, bit 1 the MPO forms.
static VSYNC_TYPES: AtomicU32 = AtomicU32::new(VS_LEGACY);
const VS_LEGACY: u32 = 1;
const VS_MPO: u32 = 2;

static DIRTY: AtomicU32 = AtomicU32::new(0);
static CHK: AtomicU32 = AtomicU32::new(0);
static CHK_OK: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static FLIP: AtomicU32 = AtomicU32::new(0);
static SCALED: AtomicU32 = AtomicU32::new(0);
static OFF: AtomicU32 = AtomicU32::new(0);
static BAD: AtomicU32 = AtomicU32::new(0);
static VS_MPO_N: AtomicU32 = AtomicU32::new(0);
static VS_BOTH: AtomicU32 = AtomicU32::new(0);
static PRESENT: AtomicU32 = AtomicU32::new(0);
static PR_DMA: AtomicU32 = AtomicU32::new(0);
static LAYER: AtomicU32 = AtomicU32::new(0);
static VS_ERR: AtomicU32 = AtomicU32::new(0);

fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// Read the knob. `DriverEntry` only (PASSIVE), before the DDI table is built.
pub(crate) fn read_knob_at_driver_entry() {
    let v = crate::diag::read_config_dword(crate::diag::knobs::SCALED_FLIP, sf::KNOB_DEFAULT);
    KNOB.store(v, Ordering::Relaxed);
    let planes = crate::diag::read_config_dword(crate::diag::knobs::SFL_PLANES, 0);
    PLANES.store(sf::planes_from_knob(planes), Ordering::Relaxed);
    let ddis = crate::diag::read_config_dword(crate::diag::knobs::SFL_DDIS, 0);
    DDIS.store(ddis, Ordering::Relaxed);
}

/// The plane count the caps report (`SflPlanes`). Any IRQL.
pub(crate) fn planes() -> u32 {
    PLANES.load(Ordering::Relaxed)
}

/// Whether the first- and second-generation check DDIs are registered too (`SflDdis` bit 0).
pub(crate) fn old_checks() -> bool {
    on() && DDIS.load(Ordering::Relaxed) & sf::DDIS_OLD_CHECKS != 0
}

/// A `DXGKQAITYPE_DRIVERCAPS` reply: `size` is the buffer dxgkrnl gave, `planes` the
/// `MaxOverlayPlanes` written into it (0 when not written). PASSIVE; atomics, mirrored by the dump.
pub(crate) fn note_caps(size: u32, planes: u32) {
    CAP_N.fetch_add(1, Ordering::Relaxed);
    CAP_SZ.store(size, Ordering::Relaxed);
    CAP_MPO.store(planes, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// Whether the surface is on for this driver load. Any IRQL.
pub(crate) fn on() -> bool {
    Mode::from_knob(KNOB.load(Ordering::Relaxed)).is_on()
}

/// A new device generation (StartDevice, PASSIVE): `PresentId`s start over, the vsync gate is the
/// legacy one `start_vsync` opens, and the `Sfl*` block is written as zeros (`SflKnob` included).
pub(crate) fn reset_for_start() {
    BOOK.reset();
    VSYNC_TYPES.store(VS_LEGACY, Ordering::Relaxed);
    for c in [
        &CHK, &CHK_OK, &WHY, &FLIP, &SCALED, &OFF, &BAD, &VS_MPO_N, &VS_BOTH, &PRESENT, &PR_DMA,
        &LAYER, &VS_ERR, &OLD_CHK,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    DIRTY.store(0, Ordering::Relaxed);
    write_block();
}

/// Mirror the block if a count moved. PASSIVE; from `scanout_trace::dump`.
pub(crate) fn publish() {
    if DIRTY.swap(0, Ordering::Relaxed) == 0 {
        return;
    }
    write_block();
}

fn write_block() {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"SflKnob", u32::from(on()));
    crate::diag::record_named_bytes(b"SflChk", r(&CHK));
    crate::diag::record_named_bytes(b"SflChkOk", r(&CHK_OK));
    crate::diag::record_named_bytes(b"SflWhy", r(&WHY));
    crate::diag::record_named_bytes(b"SflFlip", r(&FLIP));
    crate::diag::record_named_bytes(b"SflScaled", r(&SCALED));
    crate::diag::record_named_bytes(b"SflOff", r(&OFF));
    crate::diag::record_named_bytes(b"SflBad", r(&BAD));
    crate::diag::record_named_bytes(b"SflVsMpo", r(&VS_MPO_N));
    crate::diag::record_named_bytes(b"SflVsBoth", r(&VS_BOTH));
    crate::diag::record_named_bytes(b"SflPresent", r(&PRESENT));
    crate::diag::record_named_bytes(b"SflPrDma", r(&PR_DMA));
    crate::diag::record_named_bytes(b"SflLayer", r(&LAYER));
    crate::diag::record_named_bytes(b"SflVsErr", r(&VS_ERR));
    crate::diag::record_named_bytes(b"SflPlRep", if on() { planes() } else { 0 });
    crate::diag::record_named_bytes(b"SflDdisV", r(&DDIS));
    crate::diag::record_named_bytes(b"SflOldChk", r(&OLD_CHK));
    crate::diag::record_named_bytes(b"SflCapN", r(&CAP_N));
    crate::diag::record_named_bytes(b"SflCapSz", r(&CAP_SZ));
    crate::diag::record_named_bytes(b"SflCapMpo", r(&CAP_MPO));
}

/// A flip through the legacy `DxgkDdiSetVidPnSourceAddress` (any IRQL; one store).
pub(crate) fn note_legacy_flip() {
    if on() {
        BOOK.legacy_flip();
    }
}

/// `DxgkDdiPresent` with `FlipWithMultiPlaneOverlay`: true when the Present is an MMIO plane flip
/// to accept with nothing generated (the plane DDI follows). `has_dma`: dxgkrnl passed a DMA
/// buffer, which this adapter does not take for a plane flip (it is counted and refused).
pub(crate) fn present_mmio_plane_flip(has_dma: bool) -> bool {
    if !on() {
        return false;
    }
    if has_dma {
        bump(&PR_DMA);
        return false;
    }
    bump(&PRESENT);
    true
}

/// `DxgkDdiControlInterrupt` for a vsync type while the surface is on: the legacy and the MPO
/// forms open and close one delivery gate, open while either is enabled. `None` with the knob
/// off or for any other interrupt type (the legacy handling runs).
pub(crate) fn control_interrupt(
    adapter: &AdapterContext,
    interrupt_type: IN_CONST_DXGK_INTERRUPT_TYPE,
    enable: bool,
) -> Option<NTSTATUS> {
    if !on() {
        return None;
    }
    let bit = if interrupt_type == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC {
        VS_LEGACY
    } else if interrupt_type
        == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY
        || interrupt_type
            == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2
    {
        VS_MPO
    } else {
        return None;
    };
    if !adapter.display_half() {
        return None;
    }
    let types = if enable {
        VSYNC_TYPES.fetch_or(bit, Ordering::AcqRel) | bit
    } else {
        VSYNC_TYPES.fetch_and(!bit, Ordering::AcqRel) & !bit
    };
    adapter
        .vsync_enabled
        .store(u32::from(types != 0), Ordering::Release);
    // `VsCiT`, `VsCiSt`: atomics only (DIRQL).
    crate::ddi::stall_diag::note_control_vsync(types != 0);
    Some(STATUS_SUCCESS)
}

/// The vsync that reports `physical_address`: delivers the MPO form when plane flips are being
/// retired (`Some(status)`: the caller delivers nothing else), or `None` for the caller's
/// `DXGK_INTERRUPT_CRTC_VSYNC`. When dxgkrnl went back to the legacy DDI with plane flips still
/// unreported, those are retired first and the caller still delivers the legacy one.
///
/// # Safety
/// `dxgkrnl` is the live callback interface; callable at <= DISPATCH (the notification raises to
/// DIRQL through `DxgkCbSynchronizeExecution`, as `signal_crtc_vsync`).
pub(crate) unsafe fn signal_vsync(
    dxgkrnl: &DXGKRNL_INTERFACE,
    physical_address: i64,
    target_id: u32,
) -> Option<NTSTATUS> {
    if !on() {
        return None;
    }
    match BOOK.on_vsync(physical_address as u64) {
        VsyncReport::Legacy => None,
        VsyncReport::Mpo(ids) => {
            bump(&VS_MPO_N);
            Some(unsafe { notify_mpo_vsync(dxgkrnl, ids.as_slice(), target_id) })
        }
        VsyncReport::Both(ids) => {
            bump(&VS_BOTH);
            let _ = unsafe { notify_mpo_vsync(dxgkrnl, ids.as_slice(), target_id) };
            None
        }
    }
}

/// `DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2` with one entry per layer of `ids`.
unsafe fn notify_mpo_vsync(
    dxgkrnl: &DXGKRNL_INTERFACE,
    ids: &[(u32, u64)],
    target_id: u32,
) -> NTSTATUS {
    // SAFETY: all-zero is a valid value of both plain-data structs.
    let mut info =
        unsafe { core::mem::zeroed::<[DXGK_MULTIPLANE_OVERLAY_VSYNC_INFO2; sf::MAX_LAYERS]>() };
    let n = ids.len().min(sf::MAX_LAYERS);
    for (slot, &(layer, id)) in info.iter_mut().zip(ids.iter()) {
        slot.LayerIndex = layer;
        slot.PresentId = id;
    }
    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType =
        _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2;
    // SAFETY: CrtcVsyncWithMultiPlaneOverlay2 is the union arm of this interrupt type.
    let vsync = unsafe {
        interrupt
            .__bindgen_anon_1
            .CrtcVsyncWithMultiPlaneOverlay2
            .as_mut()
    };
    vsync.VidPnTargetId = target_id;
    vsync.MultiPlaneOverlayVsyncInfoCount = n as u32;
    vsync.pMultiPlaneOverlayVsyncInfo = info.as_mut_ptr();
    // SAFETY: fully initialised packet; `info` outlives the synchronous notification.
    let status = unsafe { crate::ddi::submit_command::notify_at_dirql(dxgkrnl, &mut interrupt) };
    if status != STATUS_SUCCESS {
        bump(&VS_ERR);
    }
    status
}

/// A `RECT` as the logic's.
fn rect(r: &RECT) -> Rect {
    Rect::new(r.left, r.top, r.right, r.bottom)
}

/// The logic's view of `DXGK_MULTIPLANE_OVERLAY_ATTRIBUTES3`.
fn plane_of(layer: u32, a: &DXGK_MULTIPLANE_OVERLAY_ATTRIBUTES3) -> Plane {
    Plane {
        layer,
        src: rect(&a.SrcRect),
        dst: rect(&a.DstRect),
        clip: rect(&a.ClipRect),
        // SAFETY (all three): POD unions of a bitfield struct and a `UINT` `Value`.
        flags: unsafe { a.Flags.__bindgen_anon_1.Value },
        rotation: a.Rotation as u32,
        blend: unsafe { a.Blend.__bindgen_anon_1.Value },
        color_space: a.ColorSpaceType as u32,
    }
}

/// The most planes one query is looked at with (more is a refusal by count anyway).
const MAX_QUERY_PLANES: usize = 4;

/// `DxgkDdiCheckMultiPlaneOverlaySupport3` (PASSIVE). Always `STATUS_SUCCESS` with an answer in
/// `Supported` (the only documented status); `STATUS_INVALID_PARAMETER` for a null argument.
///
/// # Safety
/// Called by dxgkrnl with its argument struct and plane arrays live for the call.
pub unsafe extern "C" fn dxgkddi_check_mpo_support3(
    h_adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT3,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let a = unsafe { &mut *args };
    a.Supported = 0;
    // SAFETY: all-zero is "no limitation reported".
    a.ReturnInfo = unsafe { core::mem::zeroed() };
    let p = h_adapter as *const AdapterContext;
    if p.is_null() || !on() {
        bump(&OFF);
        return STATUS_SUCCESS;
    }
    let adapter = unsafe { &*p };
    if !adapter.display_half() {
        bump(&OFF);
        return STATUS_SUCCESS;
    }
    bump(&CHK);
    let verdict = unsafe { judge_query(adapter, a) };
    match verdict {
        Ok(_) => {
            a.Supported = 1;
            bump(&CHK_OK);
        }
        Err(why) => {
            WHY.store(why as u32, Ordering::Relaxed);
            DIRTY.store(1, Ordering::Relaxed);
        }
    }
    STATUS_SUCCESS
}

/// Gather a Check3 query into the logic's terms and judge it.
///
/// # Safety
/// As [`dxgkddi_check_mpo_support3`].
unsafe fn judge_query(
    adapter: &AdapterContext,
    a: &DXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT3,
) -> Result<sf::Fit, sf::Why> {
    let count = a.PlaneCount as usize;
    if count > MAX_QUERY_PLANES || (count > 0 && a.ppPlanes.is_null()) {
        return Err(sf::Why::PlaneCount);
    }
    let mut planes: [(Plane, Option<Source>); MAX_QUERY_PLANES] =
        [(plane_of_default(), None); MAX_QUERY_PLANES];
    for (i, slot) in planes.iter_mut().enumerate().take(count) {
        // SAFETY: `ppPlanes` holds `PlaneCount` pointers (WDK `_Field_size_(PlaneCount)`).
        let pp = unsafe { *a.ppPlanes.add(i) };
        if pp.is_null() {
            return Err(sf::Why::PlaneCount);
        }
        let plane = unsafe { &*pp };
        if plane.VidPnSourceId != 0 {
            // This adapter has one source.
            return Err(sf::Why::Layer);
        }
        let source = unsafe {
            crate::ddi::create_allocation::scaled_flip_source(adapter, plane.hAllocation)
        };
        *slot = (plane_of(plane.LayerIndex, &plane.PlaneAttributes), source);
    }
    let post_count = a.PostCompositionCount as usize;
    let post = match post_count {
        0 => None,
        1 if !a.ppPostComposition.is_null() => {
            // SAFETY: one pointer, as counted.
            let pp = unsafe { *a.ppPostComposition };
            if pp.is_null() {
                return Err(sf::Why::PostRect);
            }
            let pc = unsafe { &*pp };
            if pc.VidPnSourceId != 0 {
                return Err(sf::Why::PostRect);
            }
            let t = &pc.PostComposition;
            Some(Post {
                src: rect(&t.SrcRect),
                dst: rect(&t.DstRect),
                // SAFETY: POD union of a bitfield struct and a `UINT` `Value`.
                flags: unsafe { t.Flags.__bindgen_anon_1.Value },
                rotation: t.Rotation as u32,
            })
        }
        _ => return Err(sf::Why::PostRect),
    };
    sf::check(
        adapter.scanout_extent(),
        self::planes(),
        &planes[..count],
        post.as_ref(),
    )
}

const fn plane_of_default() -> Plane {
    Plane {
        layer: 0,
        src: Rect::new(0, 0, 0, 0),
        dst: Rect::new(0, 0, 0, 0),
        clip: Rect::new(0, 0, 0, 0),
        flags: 0,
        rotation: 0,
        blend: 0,
        color_space: 0,
    }
}

/// `DXGK_PRIMARYCONTEXTDATA` on x64: `hContext` (8), `hAllocation` (8), the `SegmentId` / `MmuId`
/// union (2, padded to 8), the `SegmentAddress` / `VirtualAddress` union (8). The address is read
/// at its offset so no bindgen spelling of the anonymous unions is assumed; the size assert below
/// fails the build if the layout is not this one.
const PRIMARY_CONTEXT_SEGMENT_ID: usize = 16;
const PRIMARY_CONTEXT_ADDRESS: usize = 24;
const _: () = assert!(core::mem::size_of::<DXGK_PRIMARYCONTEXTDATA>() == 32);

/// `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3` (up to DIRQL, as the legacy flip DDI;
/// atomics only before the shared body). A plane-0 flip is the legacy flip of the plane's
/// allocation; a disabled plane keeps the picture and retires at the next vsync.
///
/// # Safety
/// Called by dxgkrnl with its argument struct, plane and context arrays live for the call.
pub unsafe extern "C" fn dxgkddi_set_vidpn_source_address_mpo3(
    h_adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY3,
) -> NTSTATUS {
    let p = h_adapter as *const AdapterContext;
    if p.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let adapter = unsafe { &*p };
    if !on() || !adapter.display_half() {
        bump(&OFF);
        return STATUS_NOT_SUPPORTED;
    }
    let a = unsafe { &mut *args };
    // Nothing is asked of dxgkrnl (no PrePresent, no flip-queue drain).
    a.OutputFlags.__bindgen_anon_1.Value = 0;
    if a.VidPnSourceId != 0 {
        bump(&BAD);
        return STATUS_INVALID_PARAMETER;
    }
    if a.PlaneCount > 0 && a.ppPlanes.is_null() {
        bump(&BAD);
        return STATUS_INVALID_PARAMETER;
    }
    let mut status = STATUS_SUCCESS;
    // The host shows one plane: the first enabled plane of the call is the one flipped. The check
    // refused every configuration with two, so a second is unexpected; it is retired, not shown.
    let mut shown = false;
    for i in 0..a.PlaneCount as usize {
        // SAFETY: `ppPlanes` holds `PlaneCount` pointers.
        let pp = unsafe { *a.ppPlanes.add(i) };
        if pp.is_null() {
            continue;
        }
        let plane = unsafe { &mut *pp };
        // No conversion to immediate, no PostPresent wanted.
        plane.OutputFlags.__bindgen_anon_1.Value = 0;
        let layer = plane.LayerIndex;
        // SAFETY: POD union of a bitfield struct and a `UINT` `Value`.
        let input = unsafe { plane.InputFlags.__bindgen_anon_1.Value };
        if input & sf::plane_input::ENABLED == 0 || shown || layer >= planes() {
            if input & sf::plane_input::ENABLED != 0 {
                // A second enabled plane, or a layer beyond the reported planes.
                bump(&BAD);
            }
            BOOK.record(layer, ANY_ADDRESS, plane.PresentId);
            continue;
        }
        if layer != 0 {
            bump(&LAYER);
        }
        if plane.ContextCount == 0 || plane.ppContextData.is_null() {
            bump(&BAD);
            status = STATUS_INVALID_PARAMETER;
            continue;
        }
        // SAFETY: `ppContextData` holds `ContextCount` pointers; the first names the allocation.
        let ctx = unsafe { *plane.ppContextData };
        if ctx.is_null() {
            bump(&BAD);
            status = STATUS_INVALID_PARAMETER;
            continue;
        }
        let h_alloc = unsafe { (*ctx).hAllocation };
        let base = ctx as *const u8;
        // SAFETY: inside the 32-byte struct (size asserted above); unaligned reads of plain data.
        let segment = unsafe {
            base.add(PRIMARY_CONTEXT_SEGMENT_ID)
                .cast::<u16>()
                .read_unaligned()
        };
        let address = unsafe {
            base.add(PRIMARY_CONTEXT_ADDRESS)
                .cast::<u64>()
                .read_unaligned()
        };
        let src = rect(&plane.PlaneAttributes.SrcRect);
        let (mode_w, mode_h) = adapter.scanout_extent();
        if src.width() != mode_w as i64 || src.height() != mode_h as i64 {
            bump(&SCALED);
        }
        bump(&FLIP);
        // Recorded first: an immediate flip may be retired before the shared body returns.
        BOOK.record(layer, address, plane.PresentId);
        shown = true;
        let s = unsafe {
            crate::ddi::display::set_vidpn_source_address_common(
                adapter,
                h_alloc,
                u32::from(segment),
                address,
                sf::legacy_flags_of(input),
            )
        };
        if s != STATUS_SUCCESS {
            bump(&BAD);
            status = s;
        }
    }
    status
}

/// `DxgkDdiPostMultiPlaneOverlayPresent` (PASSIVE). Never requested (no plane output flag asks
/// for it); accepted.
///
/// # Safety
/// Called by dxgkrnl.
pub unsafe extern "C" fn dxgkddi_post_mpo_present(
    _h_adapter: IN_CONST_HANDLE,
    _args: IN_CONST_PDXGKARG_POSTMULTIPLANEOVERLAYPRESENT,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// `DxgkDdiCheckMultiPlaneOverlaySupport` (first generation; registered only with `SflDdis` bit 0).
/// Always "not supported": this adapter's configurations are judged by the third-generation check.
///
/// # Safety
/// Called by dxgkrnl with its argument struct live for the call.
pub unsafe extern "C" fn dxgkddi_check_mpo_support(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let a = unsafe { &mut *args };
    a.Supported = 0;
    // SAFETY: all-zero is "no limitation reported".
    a.ReturnInfo = unsafe { core::mem::zeroed() };
    bump(&OLD_CHK);
    STATUS_SUCCESS
}

/// `DxgkDdiCheckMultiPlaneOverlaySupport2` (second generation; registered only with `SflDdis`
/// bit 0). Always "not supported", as [`dxgkddi_check_mpo_support`].
///
/// # Safety
/// Called by dxgkrnl with its argument struct live for the call.
pub unsafe extern "C" fn dxgkddi_check_mpo_support2(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT2,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let a = unsafe { &mut *args };
    a.Supported = 0;
    // SAFETY: all-zero is "no limitation reported".
    a.ReturnInfo = unsafe { core::mem::zeroed() };
    bump(&OLD_CHK);
    STATUS_SUCCESS
}

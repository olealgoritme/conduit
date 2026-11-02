//! MPO3 for the NVIDIA UMD personality: one plane, the primary.
//!
//! At WDDM 3.2 DWM drives the display as a Display-Core/MPO3 device. NVIDIA's own
//! 610.88 KMD flips only through `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3`
//! (990 calls in the boot capture, no plain `SetVidPnSourceAddress`) and reports
//! `DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY3`. Without these DDIs DWM got
//! legacy flips, rebuilt its DDisplay swap chain every frame (create three
//! primaries, set the mode, flip once, release the source) and composed at ~6 fps.
//!
//! What this registers (NVIDIA personality only, `lib.rs`):
//! * `GetMultiPlaneOverlayCaps`: one plane, shared and immediate flips, no scaling.
//! * `GetPostCompositionCaps`: no scaling.
//! * `CheckMultiPlaneOverlaySupport3`: a single plane is supported.
//! * `SetVidPnSourceAddressWithMultiPlaneOverlay3`: plane 0 is handed to the
//!   existing `SetVidPnSourceAddress` path (the native-scanout arm shows NVIDIA's
//!   memory on the host); a disabled plane completes at once.
//!
//! Completion: every primary publication counts (`note_published`); the vsync
//! (`submit_command::signal_crtc_vsync`) reports layer 0 as flipped when a new
//! publication happened since the last report (`take_completion`).
//!
//! dxgkrnl's adapter start (the DRIVERCAPS check that reads `SupportMultiPlaneOverlay`)
//! refuses the cap unless `MaxOverlayPlanes` is 1..=8 ("doesn't report any overlay
//! planes or panel fitter"), `SupportDirectFlip` is set and an MPO flip DDI exists.
//!
//! Counters: `Mpo3Flip` flips, `Mpo3Off` disabled-plane flips, `Mpo3Ret` the last
//! status the legacy path returned that was not SUCCESS, `Mpo3Chk` checks.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::dxgk::*;

static PUBLISHED: AtomicU64 = AtomicU64::new(0);
static REPORTED: AtomicU64 = AtomicU64::new(0);
static FLIPS: AtomicU32 = AtomicU32::new(0);
static OFF: AtomicU32 = AtomicU32::new(0);
static CHECKS: AtomicU32 = AtomicU32::new(0);

/// `DXGK_MULTIPLANEOVERLAYCAPS`: `Shared` (bit 8) and `Immediate` (bit 9).
const OVERLAY_CAPS_SHARED_IMMEDIATE: u32 = (1 << 8) | (1 << 9);

/// `NvMpo3` (NVIDIA personality only): bit 0 registers the DDIs, bit 1 sets
/// DRIVERCAPS.SupportMultiPlaneOverlay, bit 2 sends the MPO3 vsync, bit 3 sets
/// MaxQueuedMultiPlaneOverlayFlipVSync, bit 4 SupportMultiPlaneOverlayImmediateFlip.
/// Read once, at DriverEntry. (Bit 1 together with bits 0+2 made AddAdapter fail
/// with INVALID_PARAMETER on 405.256.)
static PARTS: AtomicU32 = AtomicU32::new(0);
pub(crate) const PART_DDIS: u32 = 1;
pub(crate) const PART_CAPS: u32 = 2;
pub(crate) const PART_VSYNC: u32 = 4;
pub(crate) const PART_QUEUE: u32 = 8;
pub(crate) const PART_IMMEDIATE: u32 = 16;
/// Bit 5: also the MPO1/MPO2 flip and check DDIs and `PostMultiPlaneOverlayPresent`.
pub(crate) const PART_LEGACY: u32 = 32;
/// Bit 6: the vsync stays on whatever `ControlInterrupt` asks. With MPO3 dxgkrnl turned
/// it off 3.4 s after boot and never on again, and DWM's wait for vblank fell back to
/// an ~80 ms timer (12.7 fps composition).
pub(crate) const PART_VSYNC_ALWAYS: u32 = 64;
/// Off by default. With every part on (0x7f) the adapter starts and DWM stops
/// rebuilding its swap chain, but it never flips again after one disabled-plane flip
/// (no picture reaches the host) and composes at ~12.7 fps; without MPO3 DWM flips
/// NVIDIA's memory to the host at 6-10 fps. NVIDIA's own KMD also implements
/// `ControlInterrupt3` and the WDDM 3.x hardware flip queue (`SetFlipQueueLogBuffer`,
/// `SetInterruptTargetPresentId`), which this does not.
const DEFAULT_PARTS: u32 = 0;

/// Read the knob (DriverEntry, after the personality is chosen).
pub(crate) fn select_at_driver_entry() -> u32 {
    let parts = if crate::ddi::wddm_surface::nvidia_selected() {
        crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvMpo3"), DEFAULT_PARTS)
    } else {
        0
    };
    PARTS.store(parts, Ordering::Release);
    crate::diag::record_named_bytes(b"Mpo3Parts", parts);
    parts
}

pub(crate) fn part(bit: u32) -> bool {
    PARTS.load(Ordering::Acquire) & bit != 0
}

/// Whether the MPO3 vsync form is used.
pub(crate) fn active() -> bool {
    part(PART_VSYNC)
}

/// A primary was published (bound or kept). Any IRQL, one atomic.
pub(crate) fn note_published() {
    PUBLISHED.fetch_add(1, Ordering::AcqRel);
}

/// Whether layer 0 flipped since the last vsync report; consumes it. Any IRQL.
pub(crate) fn take_completion() -> bool {
    let published = PUBLISHED.load(Ordering::Acquire);
    REPORTED.swap(published, Ordering::AcqRel) != published
}

fn count(counter: &AtomicU32, name: &[u8]) {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 256 == 0 {
        crate::diag::record_named_bytes(name, n);
    }
}

pub unsafe extern "C" fn get_multi_plane_overlay_caps(
    _adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_GETMULTIPLANEOVERLAYCAPS,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl passes a writable DXGKARG_GETMULTIPLANEOVERLAYCAPS.
    let args = unsafe { &mut *args };
    args.MaxPlanes = 1;
    args.MaxRGBPlanes = 1;
    args.MaxYUVPlanes = 0;
    args.OverlayCaps.__bindgen_anon_1.Value = OVERLAY_CAPS_SHARED_IMMEDIATE;
    args.MaxStretchFactor = 1.0;
    args.MaxShrinkFactor = 1.0;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn get_post_composition_caps(
    _adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_GETPOSTCOMPOSITIONCAPS,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl passes a writable DXGKARG_GETPOSTCOMPOSITIONCAPS.
    let args = unsafe { &mut *args };
    args.MaxStretchFactor = 1.0;
    args.MaxShrinkFactor = 1.0;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn check_multi_plane_overlay_support3(
    _adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT3,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    count(&CHECKS, b"Mpo3Chk");
    // SAFETY: dxgkrnl passes a writable DXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT3.
    let args = unsafe { &mut *args };
    args.Supported = BOOL::from(args.PlaneCount <= 1);
    STATUS_SUCCESS
}

/// Plane 0 through the legacy entry; any IRQL up to DIRQL, as that one.
pub unsafe extern "C" fn set_vidpn_source_address_with_multi_plane_overlay3(
    adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY3,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl passes a valid DXGKARG_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY3
    // whose ppPlanes holds PlaneCount plane pointers.
    let args = unsafe { &mut *args };
    let mut plane0: Option<&DXGK_MULTIPLANE_OVERLAY_PLANE3> = None;
    for i in 0..args.PlaneCount as usize {
        // SAFETY: i < PlaneCount, per the contract above.
        let plane = unsafe { (*args.ppPlanes.add(i)).as_ref() };
        if let Some(plane) = plane.filter(|p| p.LayerIndex == 0) {
            plane0 = Some(plane);
        }
    }
    let Some(plane) = plane0 else {
        return STATUS_INVALID_PARAMETER;
    };
    // SAFETY: plain bitfield union reads.
    let flags = unsafe { plane.InputFlags.__bindgen_anon_1.__bindgen_anon_1 };
    if flags.Enabled() == 0 || plane.ContextCount == 0 || plane.ppContextData.is_null() {
        // Nothing to scan out on this layer: the flip completes at the next vsync.
        count(&OFF, b"Mpo3Off");
        note_published();
        return STATUS_SUCCESS;
    }
    // SAFETY: ContextCount >= 1 entries, per the plane contract.
    let Some(context) = (unsafe { (*plane.ppContextData).as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    count(&FLIPS, b"Mpo3Flip");
    // SAFETY: an all-zero DXGKARG_SETVIDPNSOURCEADDRESS is a valid value (handles,
    // integers and a plain bitfield union).
    let mut legacy = unsafe { core::mem::zeroed::<DXGKARG_SETVIDPNSOURCEADDRESS>() };
    legacy.VidPnSourceId = args.VidPnSourceId;
    // SAFETY: plain union reads of the context data.
    legacy.PrimarySegment = u32::from(unsafe { context.__bindgen_anon_1.SegmentId });
    legacy.PrimaryAddress = unsafe { *context.__bindgen_anon_2.SegmentAddress.as_ref() };
    legacy.hAllocation = context.hAllocation;
    legacy.ContextCount = 1;
    legacy.Context[0] = context.hContext;
    // SAFETY: plain bitfield union writes.
    let out = unsafe { &mut legacy.Flags.__bindgen_anon_1.__bindgen_anon_1 };
    out.set_FlipImmediate(flags.FlipImmediate());
    out.set_FlipOnNextVSync(flags.FlipOnNextVSync());
    legacy.Duration = args.Duration;
    let status = unsafe { crate::ddi::traced::set_vidpn_source_address(adapter, &mut legacy) };
    if status != STATUS_SUCCESS {
        crate::diag::record_named_bytes(b"Mpo3Ret", status as u32);
    }
    status
}

/// One plane's flip through the legacy entry (MPO1/MPO2 shapes).
#[allow(clippy::too_many_arguments)]
unsafe fn flip_plane(
    adapter: IN_CONST_HANDLE,
    source: D3DDDI_VIDEO_PRESENT_SOURCE_ID,
    enabled: bool,
    segment: UINT,
    address: PHYSICAL_ADDRESS,
    allocation: HANDLE,
    context_count: UINT,
    contexts: &[HANDLE; 65],
    flags: DXGK_SETVIDPNSOURCEADDRESS_FLAGS,
    duration: UINT,
) -> NTSTATUS {
    if !enabled || allocation.is_null() {
        count(&OFF, b"Mpo3Off");
        note_published();
        return STATUS_SUCCESS;
    }
    count(&FLIPS, b"Mpo3Flip");
    // SAFETY: an all-zero DXGKARG_SETVIDPNSOURCEADDRESS is a valid value.
    let mut legacy = unsafe { core::mem::zeroed::<DXGKARG_SETVIDPNSOURCEADDRESS>() };
    legacy.VidPnSourceId = source;
    legacy.PrimarySegment = segment;
    legacy.PrimaryAddress = address;
    legacy.hAllocation = allocation;
    legacy.ContextCount = context_count.min(65);
    legacy.Context = *contexts;
    legacy.Flags = flags;
    legacy.Duration = duration;
    let status = unsafe { crate::ddi::traced::set_vidpn_source_address(adapter, &legacy) };
    if status != STATUS_SUCCESS {
        crate::diag::record_named_bytes(b"Mpo3Ret", status as u32);
    }
    status
}

pub unsafe extern "C" fn set_vidpn_source_address_with_multi_plane_overlay(
    adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a valid argument block with PlaneCount planes.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    for i in 0..args.PlaneCount as usize {
        // SAFETY: i < PlaneCount.
        let plane = unsafe { &*args.pPlanes.add(i) };
        if plane.LayerIndex == 0 {
            return unsafe {
                flip_plane(
                    adapter,
                    args.VidPnSourceId,
                    plane.Enabled != 0,
                    plane.AllocationSegment,
                    plane.AllocationAddress,
                    plane.hAllocation,
                    args.ContextCount,
                    &args.Context,
                    args.Flags,
                    args.Duration,
                )
            };
        }
    }
    STATUS_INVALID_PARAMETER
}

pub unsafe extern "C" fn set_vidpn_source_address_with_multi_plane_overlay2(
    adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY2,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a valid argument block with PlaneCount planes.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    for i in 0..args.PlaneCount as usize {
        // SAFETY: i < PlaneCount.
        let plane = unsafe { &*args.pPlanes.add(i) };
        if plane.LayerIndex == 0 {
            return unsafe {
                flip_plane(
                    adapter,
                    args.VidPnSourceId,
                    plane.Enabled != 0,
                    plane.AllocationSegment,
                    plane.AllocationAddress,
                    plane.hAllocation,
                    args.ContextCount,
                    &args.Context,
                    args.Flags,
                    args.Duration,
                )
            };
        }
    }
    STATUS_INVALID_PARAMETER
}

pub unsafe extern "C" fn check_multi_plane_overlay_support(
    _adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a writable argument block.
    let Some(args) = (unsafe { args.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    count(&CHECKS, b"Mpo3Chk");
    args.Supported = BOOL::from(args.PlaneCount <= 1);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn check_multi_plane_overlay_support2(
    _adapter: IN_CONST_HANDLE,
    args: IN_OUT_PDXGKARG_CHECKMULTIPLANEOVERLAYSUPPORT2,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a writable argument block.
    let Some(args) = (unsafe { args.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    count(&CHECKS, b"Mpo3Chk");
    args.Supported = BOOL::from(args.PlaneCount <= 1);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn post_multi_plane_overlay_present(
    _adapter: IN_CONST_HANDLE,
    _args: IN_CONST_PDXGKARG_POSTMULTIPLANEOVERLAYPRESENT,
) -> NTSTATUS {
    STATUS_SUCCESS
}

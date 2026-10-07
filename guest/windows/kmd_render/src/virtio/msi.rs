//! Message-signalled interrupts for the virtio-gpu device: deciding whether the
//! OS handed the driver messages instead of the INTx line, and programming the
//! device's MSI-X vectors to match. All decisions live in
//! `helios_kmd_logic::msi` (host-tested); this file is the WDK/MMIO glue.
//!
//! Design and the hardware verification list: `docs/msi-interrupts.md`.
//!
//! # Who decides MSI versus INTx
//!
//! Not this driver, for the start in progress. A WDDM miniport does not connect
//! its own interrupt: dxgkrnl connects whatever PnP assigned and calls
//! `DxgkDdiInterruptRoutine` with a message number (0 for a line). PnP assigns
//! messages when the device key's `MSISupported` is 1 (the INF's default) and the
//! device offers them. What this driver can do is change that key for the NEXT
//! start (`apply_key_policy`: `MsiMode`, the latch). By the time `StartDevice`
//! runs the choice is made, and two things say which way it went: the MSI-X
//! Enable bit in the device's PCI capability, and message descriptors in the
//! translated resource list. Either one means messages (`planned_messages`);
//! the driver follows it:
//!
//! * Neither: INTx. Nothing in this file touches the device and the driver
//!   behaves exactly as before (the only addition is two read-only probes).
//! * Messages: the device's vectors MUST be programmed (a device with
//!   `NO_VECTOR` everywhere raises nothing once MSI-X is enabled) and the ISR
//!   must route by message number instead of reading the ISR status register.
//!
//! # Default, fallback, measurement
//!
//! MSI-X is an opt-in in this package (`MsiMode=2`; the INF writes `MSISupported=0`). What happens
//! when it does not work is the second half of this file: the per-vector
//! counters the ISR and DPC feed, the health logic (a "rescue" is a waiter's
//! polling drain finding a completion no interrupt announced), the polling
//! safety net the HPD worker runs while delivery is in doubt, and the latch
//! (`MsiLatch`) that makes the next `AddDevice` ask PnP for INTx. See
//! `docs/msi-interrupts.md`.
//!
//! # IRQL
//!
//! The set-up half is PASSIVE_LEVEL, run from `StartDevice` / `VirtioGpu::init`
//! (and `apply_key_policy` from `AddDevice`). The counting half
//! ([`note_message`], [`note_intx`], [`take_dpc_cause`]) is atomics only and runs
//! at DIRQL / DISPATCH; the registry is written only by [`publish_counters`],
//! [`latch_intx`] and the set-up breadcrumbs, all PASSIVE.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::msi::{self, Health, Mode, Plan};

use super::config::DxgkConfigAccess;
use super::pci_caps::{map_common_cfg, read_msix_message_control};
use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;

/// Offsets into `virtio_pci_common_cfg` (virtio 1.2, 4.1.4.3).
const COMMON_MSIX_CONFIG: usize = 0x10;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_MSIX_VECTOR: usize = 0x1A;

/// How many messages the OS connected for this device, as a lower bound; 0 when
/// the device is on the INTx line (the Enable bit is clear) or has no MSI at all.
///
/// Its own `#[inline(never)]` frame, called from `StartDevice` BEFORE
/// `VirtioGpu::init` rather than from inside it: `DXGK_DEVICE_INFO` is ~150
/// bytes and `init` sits on the 24 KB boot stack (tools/kmd-frame-sizes.ps1).
/// Sequential with `init`, so the frames never overlap.
#[inline(never)]
pub(crate) fn probe_granted(dxgkrnl: &DXGKRNL_INTERFACE) -> u32 {
    let access = DxgkConfigAccess::new(dxgkrnl);
    let msix_ctrl = read_msix_message_control(&access);
    crate::diag::record_named_bytes(b"MsiCap", u32::from(msix_ctrl.unwrap_or(0)));
    // A device with no MSI-X capability cannot do virtio messages: INTx, and the
    // resource-list call is skipped. Otherwise either signal alone is enough (see
    // `planned_messages`), so the list is read even when Enable is still clear.
    if msix_ctrl.is_none() {
        crate::diag::record_named_bytes(b"MsiGrant", 0);
        return 0;
    }
    let listed = listed_messages(dxgkrnl);
    crate::diag::record_named_bytes(b"MsiList", listed);
    let granted = msi::planned_messages(msix_ctrl, listed);
    crate::diag::record_named_bytes(b"MsiGrant", granted);
    granted
}

/// Message-interrupt descriptors in the translated resource list (a lower
/// bound; 0 if the list is unavailable).
#[inline(never)]
fn listed_messages(dxgkrnl: &DXGKRNL_INTERFACE) -> u32 {
    let Some(get_info) = dxgkrnl.DxgkCbGetDeviceInformation else {
        return 0;
    };
    // SAFETY: an all-zero DXGK_DEVICE_INFO (pointers, integers, an enum whose 0
    // is `DockStateUnsupported`) is valid; dxgkrnl fills it.
    let mut info: DXGK_DEVICE_INFO = unsafe { core::mem::zeroed() };
    // SAFETY: documented PASSIVE_LEVEL callback, called from StartDevice with the
    // live device handle and a valid out-structure.
    let status = unsafe { get_info(dxgkrnl.DeviceHandle, &mut info) };
    if status != STATUS_SUCCESS {
        return 0;
    }
    let list = info.TranslatedResourceList as *const u8;
    if list.is_null() {
        return 0;
    }
    msi::granted_messages(|off| {
        // SAFETY: `off` is a 4-aligned offset derived by the parser from the
        // list's own counts (bounded: at most 64 descriptors); the list is the
        // OS-owned allocation, valid for the duration of StartDevice.
        Some(unsafe { core::ptr::read_unaligned(list.add(off).cast::<u32>()) })
    })
}

#[inline]
unsafe fn cfg_write16(va: usize, off: usize, value: u16) {
    // SAFETY: caller guarantees `va` maps a common-cfg region >= 0x1C bytes.
    unsafe { core::ptr::write_volatile((va + off) as *mut u16, value) }
}

#[inline]
unsafe fn cfg_read16(va: usize, off: usize) -> u16 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile((va + off) as *const u16) }
}

/// Write `plan` to the device and verify every vector by reading it back.
/// `queues[i]` is the virtio queue number planned as `plan.queue[i]`.
///
/// # Safety
/// `va` maps the device's common configuration (>= 0x1C bytes). Called at
/// PASSIVE_LEVEL, single-threaded, before DRIVER_OK: nothing else is selecting
/// queues.
unsafe fn write_plan(va: usize, plan: &Plan, queues: &[u16]) -> bool {
    // SAFETY: per the function contract, for every access below.
    unsafe {
        cfg_write16(va, COMMON_MSIX_CONFIG, plan.config);
        let mut ok = msi::vector_accepted(plan.config, cfg_read16(va, COMMON_MSIX_CONFIG));
        for (i, &queue) in queues.iter().enumerate() {
            let want = plan.queue[i];
            cfg_write16(va, COMMON_QUEUE_SELECT, queue);
            cfg_write16(va, COMMON_QUEUE_MSIX_VECTOR, want);
            ok &= msi::vector_accepted(want, cfg_read16(va, COMMON_QUEUE_MSIX_VECTOR));
        }
        ok
    }
}

/// Program the device's vectors for `granted` messages and return the ISR's
/// state word ([`msi::isr_state`]; never 0 on `Ok`).
///
/// Order (`msi::setup_plan`): the plan first; if the device refuses any vector, ONE
/// retry with every queue on message 0. If that is refused too (or the common cfg
/// cannot be mapped a second time), the transport STAYS UP with no vector programmed
/// ([`msi::polling_only_state`]): the OS connected messages and not the line, so the
/// device cannot be heard, but every completion is still found by polling (a waiter's
/// drain after each slice, the worker's safety net, `POLL_ONLY` / `MsiPollOnly`), which
/// keeps the display half instead of failing the transport. INTx is latched for the
/// next start either way. `queues` are the queue numbers that exist, control first.
///
/// Must run before DRIVER_OK: QEMU wires each queue's call eventfd to KVM as an
/// irqfd when the guest notifiers are set up at DRIVER_OK, from the vectors
/// programmed by then.
#[inline(never)]
pub(crate) fn program_vectors(
    passive: PassiveLevel,
    access: &DxgkConfigAccess,
    granted: u32,
    queues: &[u16],
) -> Result<u32, ()> {
    if granted == 0 || queues.is_empty() || queues.len() > msi::MAX_QUEUES {
        return Err(());
    }
    let va = map_common_cfg(access);
    if va == 0 {
        crate::diag::record_named_bytes(b"MsiNoCfg", 1);
        return Ok(give_up_on_vectors(passive, 0, msi::latch_why::NO_CFG, &[]));
    }
    let shared_only = crate::diag::read_config_dword(crate::diag::knobs::MSI_VECTORS, 0) != 0;
    // The plan, then (if the device refuses a vector) every queue on message 0, then give up.
    // `msi::setup_plan` is the table (host tested); a retry that would write the same vectors
    // again is skipped there.
    let mut refusals = 0u32;
    while let Some(plan) = msi::setup_plan(refusals, granted, queues.len(), shared_only) {
        // SAFETY: `va` is the mapped common cfg (>= 0x1C bytes, checked by
        // `map_common_cfg`); PASSIVE, pre-DRIVER_OK.
        if unsafe { write_plan(va, &plan, queues) } {
            crate::diag::record_named_bytes(b"MsiVec", plan.max_vector().map_or(0xFFFF, u32::from));
            return Ok(msi::isr_state(&plan));
        }
        refusals += 1;
        crate::diag::record_named_bytes(b"MsiRefused", refusals);
    }
    Ok(give_up_on_vectors(
        passive,
        va,
        msi::latch_why::REFUSED,
        queues,
    ))
}

/// Out of plans: unassign every vector the device may have taken (`va` 0: the cfg could not
/// be mapped, nothing to write), latch INTx for the next start, and run this one polling-only.
/// Returns the ISR state for a message-mode start with no vector.
#[inline(never)]
fn give_up_on_vectors(passive: PassiveLevel, va: usize, why: u32, queues: &[u16]) -> u32 {
    if va != 0 {
        // SAFETY: `va` is the mapped common cfg checked by the caller; PASSIVE, pre-DRIVER_OK.
        // NO_VECTOR is always accepted; the result is not needed.
        let _ = unsafe { write_plan(va, &Plan::NONE, queues) };
    }
    POLL_ONLY.store(1, Ordering::Release);
    crate::diag::record_named_bytes(b"MsiVec", 0xFFFF);
    latch_intx(passive, why);
    msi::polling_only_state()
}

// ── Policy at AddDevice: what the device key should ask PnP for ──────────────

/// This image's build tag (`msi::build_tag`: the build and revision of `HELIOS_KMD_VERSION`),
/// written next to the breaker's marker (`MsiStartingVer`) and the latch (`MsiLatchVer`) so the
/// next `AddDevice` can tell a fault of THIS build from one of the build a driver update
/// replaced. Evaluated at compile time from the same file the INF `DriverVer` and the image
/// `FILEVERSION` are rendered from; a malformed version fails the build.
const BUILD_TAG: u32 = match msi::build_tag(include_str!("../../driver-version.env")) {
    Some(tag) => tag,
    None => panic!("kmd_render/driver-version.env: no usable HELIOS_KMD_VERSION"),
};

/// `Interrupt Management\MessageSignaledInterruptProperties`, relative to the device key.
const MSI_SUBKEY: &[u8] = b"Interrupt Management\\MessageSignaledInterruptProperties";
static MSI_SUBKEY_W: [u16; MSI_SUBKEY.len()] =
    crate::diag::widen::<{ MSI_SUBKEY.len() }>(MSI_SUBKEY);
const MSI_SUPPORTED: &[u8] = b"MSISupported";
static MSI_SUPPORTED_W: [u16; MSI_SUPPORTED.len()] =
    crate::diag::widen::<{ MSI_SUPPORTED.len() }>(MSI_SUPPORTED);

/// `DxgkDdiAddDevice` (PASSIVE): bring the device key's `MSISupported` in line with the
/// operator's `MsiMode` and the latch, for the start that follows.
///
/// PnP reads `MSISupported` when it builds the device's interrupt requirements, which is
/// after `AddDevice` for a start but is not something Windows documents the order of: so a
/// value written here is guaranteed for the NEXT start, and honoured by this one only if PnP
/// has not read it yet. `MsiWant` records what was asked for, `MsiGrant` (StartDevice) what
/// was granted; a mismatch on the first start after a change means the write came late. The
/// driver follows what PnP granted either way.
///
/// Only LOWERS the value on its own (the latch, the breaker) or obeys the operator (`MsiMode`:
/// 1 lowers it, 2 and 3 raise it, 0 leaves it): the INF is the source of truth otherwise. Best effort: a failure is a breadcrumb (`MsiKeyWr` = the NTSTATUS), never
/// an error, and `MsiKeyWr` = 0xFFFFFFFF means nothing was written.
#[inline(never)]
pub(crate) fn apply_key_policy(passive: PassiveLevel, pdo: PDEVICE_OBJECT) {
    use crate::diag::{knobs, read_config_dword, record_named_bytes as rec};
    let mode = Mode::from_knob(read_config_dword(knobs::MSI_MODE, 0));
    // The latch belongs to the build that wrote it (`MsiLatchVer`; none = the operator's).
    // Another build's latch is stale: this build gets one fresh attempt at MSI-X.
    let latch = read_config_dword(knobs::MSI_LATCH, 0) != 0;
    let latch_build = read_config_dword(knobs::MSI_LATCH_VER, 0);
    let verdict = msi::latch_verdict(latch, latch_build, BUILD_TAG);
    if verdict == msi::LatchVerdict::Stale {
        rec(b"MsiLatch", 0);
        rec(b"MsiLatchWhy", 0);
        rec(b"MsiLatchVer", 0);
        rec(
            b"MsiLatchOld",
            read_config_dword(knobs::MSI_LATCH_OLD, 0).saturating_add(1),
        );
    } else if msi::latch_tag_orphaned(latch, latch_build) {
        // The latch was cleared by writing 0: drop its tag, so a later hand-set 1 is the operator's.
        rec(b"MsiLatchVer", 0);
    }
    let mut latched = verdict.latched();
    // The boot-loop breaker: a message-mode start that never became healthy left its marker.
    // The marker is consumed here whatever the mode or the build that set it; only a marker of
    // THIS build trips the breaker (`msi::marker_verdict`), `MsiMode=3` ignores it.
    let marker = read_config_dword(knobs::MSI_STARTING, 0) != 0;
    let marker_build = read_config_dword(knobs::MSI_STARTING_VER, 0);
    match msi::marker_verdict(mode, marker, marker_build, BUILD_TAG) {
        msi::MarkerVerdict::Absent => {}
        msi::MarkerVerdict::Ignored => rec(b"MsiStarting", 0),
        msi::MarkerVerdict::Stale => {
            rec(b"MsiStarting", 0);
            rec(
                b"MsiMarkerOld",
                read_config_dword(knobs::MSI_MARKER_OLD, 0).saturating_add(1),
            );
        }
        msi::MarkerVerdict::Trip => {
            rec(b"MsiStarting", 0);
            rec(
                b"MsiBreaker",
                read_config_dword(knobs::MSI_BREAKER, 0).saturating_add(1),
            );
            latch_intx(passive, msi::latch_why::BREAKER);
            latched = true;
        }
    }
    let action = msi::key_action(mode, latched);
    rec(b"MsiModeEff", mode.code());
    rec(b"MsiWant", action.mirror());
    let Some(value) = action.value() else {
        rec(b"MsiKeyWr", u32::MAX);
        return;
    };
    let status = crate::diag::write_device_key_dword(
        passive,
        pdo.cast(),
        &MSI_SUBKEY_W,
        &MSI_SUPPORTED_W,
        value,
    );
    rec(b"MsiKeyWr", status as u32);
}

/// Ask for INTx at the next start (PASSIVE): the latch `AddDevice` reads, tagged with this
/// build (`MsiLatchVer`: another build treats it as stale, `msi::latch_verdict`). `why` is one
/// of `msi::latch_why`. FLUSHED to disk: the faults it records are the ones that end in a hang
/// or a bugcheck, and a latch the lazy writer had not written would be lost with them. Written
/// without a guard against repeats: callers are once per start.
///
/// Not while the device is stopping (`msi::latch_allowed`, the flag `ddi::escape_wait` raises at
/// StopDevice / RemoveDevice entry): a run-time verdict then is ignored and counted
/// (`MsiLatchStop`, `MsiLatchStopW`). The breaker is exempt (it runs at AddDevice, where a
/// same-image restart still has the previous RemoveDevice's flag up).
pub(crate) fn latch_intx(passive: PassiveLevel, why: u32) {
    if !msi::latch_allowed(why, crate::ddi::escape_wait::stopping()) {
        crate::diag::record_named_bytes(
            b"MsiLatchStop",
            LATCH_STOP.fetch_add(1, Ordering::Relaxed).wrapping_add(1),
        );
        crate::diag::record_named_bytes(b"MsiLatchStopW", why);
        return;
    }
    // The tag first: a latch is never on disk with another build's tag next to it.
    crate::diag::record_named_bytes(b"MsiLatchVer", BUILD_TAG);
    crate::diag::record_named_bytes(b"MsiLatch", 1);
    crate::diag::record_named_bytes(b"MsiLatchWhy", why);
    crate::diag::flush_service_key(passive);
}

// ── Counting: interrupts and DPCs by vector (DIRQL / DISPATCH, atomics only) ──

/// One counter alone on its cache line. The messages of one device may interrupt on
/// different CPUs at once; counters sharing a line would bounce it between them on every
/// interrupt, which is the cost this change is trying to remove.
#[repr(align(64))]
struct Padded(AtomicU32);

#[allow(clippy::declare_interior_mutable_const)]
const PADDED_ZERO: Padded = Padded(AtomicU32::new(0));

/// Interrupts per message slot (`msi::vector_slot`).
static INTS: [Padded; msi::SLOTS] = [PADDED_ZERO; msi::SLOTS];
/// DPCs run, per message slot that queued them.
static DPCS: [Padded; msi::SLOTS] = [PADDED_ZERO; msi::SLOTS];
/// Bits (`msi::cause_bit`, `msi::CAUSE_INTX`) of what queued the next DPC, taken by it.
static DPC_CAUSE: Padded = PADDED_ZERO;
/// INTx interrupts claimed (status bit set) / not ours (status 0, a shared line).
static INTX_INTS: AtomicU32 = AtomicU32::new(0);
static INTX_MISS: AtomicU32 = AtomicU32::new(0);
/// DPCs the INTx path queued.
static INTX_DPCS: AtomicU32 = AtomicU32::new(0);
/// DPCs with no recorded cause: queued by `DxgkCbNotifyInterrupt` (a vsync, a DMA
/// completion), `request_wddm_completion_dpc` or a rescue.
static DPC_NO_CAUSE: AtomicU32 = AtomicU32::new(0);
/// DPCs a message queued that found nothing on any ring: spurious, or the work was
/// already taken by a waiter's polling drain or an earlier DPC (coalescing).
static MSI_IDLE: AtomicU32 = AtomicU32::new(0);

/// An interrupt arrived on message `message`. DIRQL.
#[inline]
pub(crate) fn note_message(message: u32) {
    INTS[msi::vector_slot(message)]
        .0
        .fetch_add(1, Ordering::Relaxed);
    DPC_CAUSE
        .0
        .fetch_or(msi::cause_bit(message), Ordering::Release);
}

/// An INTx interrupt was ours (ISR status nonzero). DIRQL.
#[inline]
pub(crate) fn note_intx() {
    INTX_INTS.fetch_add(1, Ordering::Relaxed);
    DPC_CAUSE.0.fetch_or(msi::CAUSE_INTX, Ordering::Release);
}

/// An INTx interrupt was not ours (ISR status 0). DIRQL.
#[inline]
pub(crate) fn note_intx_miss() {
    INTX_MISS.fetch_add(1, Ordering::Relaxed);
}

/// The DPC starts: take what queued it and count it per vector. DISPATCH. Returns the cause
/// mask (0 when nothing recorded one).
#[inline]
pub(crate) fn take_dpc_cause() -> u32 {
    let cause = DPC_CAUSE.0.swap(0, Ordering::Acquire);
    if cause == 0 {
        DPC_NO_CAUSE.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    for (slot, count) in DPCS.iter().enumerate() {
        if msi::cause_has_slot(cause, slot) {
            count.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    if cause & msi::CAUSE_INTX != 0 {
        INTX_DPCS.fetch_add(1, Ordering::Relaxed);
    }
    cause
}

/// A DPC a message queued did no ring work. DISPATCH.
#[inline]
pub(crate) fn note_dpc_idle() {
    MSI_IDLE.fetch_add(1, Ordering::Relaxed);
}

/// Message interrupts taken so far, all vectors (wraps).
fn ints_total() -> u32 {
    INTS.iter()
        .fold(0u32, |sum, c| sum.wrapping_add(c.0.load(Ordering::Relaxed)))
}

// ── Health: is message delivery working, and the safety net while it is in doubt ──

/// Rescues: a waiter's polling drain, after its wait slice timed out, found a completion.
/// Counted in both modes (a lost INTx interrupt looks the same).
static RESCUES: AtomicU32 = AtomicU32::new(0);
/// `msi::Health::code` now / of the end-of-start verdict.
static HEALTH: AtomicU32 = AtomicU32::new(0);
static START_VERDICT: AtomicU32 = AtomicU32::new(0);
/// Silent rescues in a row, and the message total at the last rescue (or when armed).
static STREAK: AtomicU32 = AtomicU32::new(0);
static INTS_PREV: AtomicU32 = AtomicU32::new(0);
/// Set when `StartDevice` has finished: rescues before that prove nothing (whether dxgkrnl
/// delivers interrupts to a device that is still starting is not assumed).
static ARMED: AtomicU32 = AtomicU32::new(0);
/// The polling safety net is on (message mode only): the HPD worker drains the rings on a
/// short timer, and a rescue queues the DPC.
static POLL: AtomicU32 = AtomicU32::new(0);
/// Worker wakes that drained under the safety net.
static POLL_N: AtomicU32 = AtomicU32::new(0);
/// This start began with the latch set (an earlier start convicted delivery).
static LATCHED: AtomicU32 = AtomicU32::new(0);
/// The device refused every vector plan (or the cfg could not be mapped) and this start runs
/// polling-only: transport up, no vector programmed (`MsiPollOnly`).
static POLL_ONLY: AtomicU32 = AtomicU32::new(0);
/// The `MsiStarting` marker is set in the registry for this start.
static STARTING: AtomicU32 = AtomicU32::new(0);
/// Latches ignored because the device was stopping (`MsiLatchStop`), since the image loaded.
static LATCH_STOP: AtomicU32 = AtomicU32::new(0);
/// Interrupt time (100 ns) when run-time judging was armed; 0 = not armed.
static ARMED_AT: AtomicU64 = AtomicU64::new(0);
/// The latch was written by the run-time verdict of this start.
static RUNTIME_LATCHED: AtomicU32 = AtomicU32::new(0);
/// Message total and ring pops when the transport went live, for the end-of-start verdict.
static INTS_BASE: AtomicU32 = AtomicU32::new(0);
static POPS_BASE: AtomicU32 = AtomicU32::new(0);

/// A start begins (PASSIVE, `StartDevice`, right after `probe_granted`, before `init`): the
/// counters of this start are zeroed (a `pnputil /restart-device` into the other mode must read
/// as that mode's numbers, not a sum: the image, and these statics, outlive the restart), and a
/// start that got messages sets the boot-loop breaker's `MsiStarting` marker and flushes it to
/// disk BEFORE anything that could hang. `finish_start` / `service` clear it once interrupts
/// arrive. In INTx mode nothing is set.
#[inline(never)]
pub(crate) fn begin_start(passive: PassiveLevel, granted: u32) {
    for c in INTS.iter().chain(DPCS.iter()) {
        c.0.store(0, Ordering::Relaxed);
    }
    for c in [
        &INTX_INTS,
        &INTX_MISS,
        &INTX_DPCS,
        &DPC_NO_CAUSE,
        &MSI_IDLE,
        &RESCUES,
        &POLL_N,
        &POLL_ONLY,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    DPC_CAUSE.0.store(0, Ordering::Relaxed);
    ARMED_AT.store(0, Ordering::Relaxed);
    STARTING.store(0, Ordering::Relaxed);
    if granted != 0 {
        STARTING.store(1, Ordering::Release);
        // The tag first: a marker is never on disk with another build's tag next to it.
        crate::diag::record_named_bytes(b"MsiStartingVer", BUILD_TAG);
        crate::diag::record_named_bytes(b"MsiStarting", 1);
        crate::diag::flush_service_key(passive);
    }
}

/// The transport is built and its ISR state is about to be published (PASSIVE, `StartDevice`,
/// before the interrupt can be claimed): start a new health generation. `messages` is whether
/// the device is in message mode; in INTx mode everything below stays inert.
#[inline(never)]
pub(crate) fn on_transport_up(messages: bool) {
    HEALTH.store(Health::Unknown.code(), Ordering::Relaxed);
    START_VERDICT.store(Health::Unknown.code(), Ordering::Relaxed);
    STREAK.store(0, Ordering::Relaxed);
    ARMED.store(0, Ordering::Relaxed);
    RUNTIME_LATCHED.store(0, Ordering::Relaxed);
    INTS_BASE.store(ints_total(), Ordering::Relaxed);
    POPS_BASE.store(
        crate::virtio::gpu::RING_POPS.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    let latched = messages && crate::diag::read_config_dword(crate::diag::knobs::MSI_LATCH, 0) != 0;
    LATCHED.store(u32::from(latched), Ordering::Relaxed);
    // An earlier start convicted delivery and PnP still handed this one messages (the key
    // write came late, or the operator forced MSI-X), or no vector could be programmed: poll
    // from the first moment.
    let polling_only = messages && POLL_ONLY.load(Ordering::Acquire) != 0;
    POLL.store(
        u32::from(polling_only || (messages && msi::polling_wanted(latched, Health::Unknown))),
        Ordering::Release,
    );
}

/// `StartDevice`'s last step (PASSIVE): the end-of-start verdict, then arm run-time judging.
/// The verdict is never a conviction (`msi::start_verdict`); silence only turns the polling
/// safety net on.
#[inline(never)]
pub(crate) fn finish_start(adapter: &AdapterContext) {
    if adapter.msi_state.load(Ordering::Acquire) == 0 {
        // INTx (or no transport): nothing to judge, but the registry shows THIS start now.
        publish_counters();
        return;
    }
    let ints = ints_total().wrapping_sub(INTS_BASE.load(Ordering::Relaxed));
    let pops = crate::virtio::gpu::RING_POPS
        .load(Ordering::Relaxed)
        .wrapping_sub(POPS_BASE.load(Ordering::Relaxed));
    let verdict = msi::start_verdict(ints, pops);
    START_VERDICT.store(verdict.code(), Ordering::Relaxed);
    HEALTH.store(verdict.code(), Ordering::Relaxed);
    if msi::polling_wanted(LATCHED.load(Ordering::Relaxed) != 0, verdict) {
        POLL.store(1, Ordering::Release);
    }
    INTS_PREV.store(ints_total(), Ordering::Relaxed);
    STREAK.store(0, Ordering::Relaxed);
    ARMED_AT.store(
        crate::adapter::foreign_scanout::now_100ns().max(1),
        Ordering::Relaxed,
    );
    ARMED.store(1, Ordering::Release);
    // Zeros and the verdict into the registry now, so it shows this start immediately rather
    // than the last one's numbers until the first periodic mirror.
    publish_counters();
}

/// Clear the `MsiStarting` marker once the start proved healthy (`msi::marker_may_clear`:
/// armed, not convicted, an interrupt arrived, and 3 s old). PASSIVE; one load when the marker
/// is not set. Called from the HPD worker's pass and the periodic mirror. This clear is lazy
/// (not flushed): the image keeps running, the lazy writer catches up, and losing it to a crash
/// costs one false trip of this build's breaker, which is the safe direction. StopDevice uses
/// [`service_stop`], which flushes.
pub(crate) fn service(stopping: bool) {
    let _ = clear_marker(stopping);
}

/// StopDevice's clear of the marker (PASSIVE): UNCONDITIONAL whenever this start set one
/// (`msi::marker_may_clear` with `stopping`: a clean stop proves the start did not hang or die,
/// whether or not judging was armed, interrupts arrived, 3 s passed or delivery was convicted;
/// a conviction is the run-time latch's business). The service key is then FLUSHED, so the
/// clear is on disk before the image can be
/// unloaded (a driver update), the machine rebooted or powered off. The clear of a clean stop
/// must not depend on the lazy writer, nor on `StopFlush` and the later stop stages (the stage-5
/// flush that would also cover it is skipped with `StopFlush=0`, and a stop that dies before it
/// would lose the clear). One load and no flush when there was no marker to clear (every INTx
/// start). Returns whether it flushed, so StopDevice can credit the time to its budget.
pub(crate) fn service_stop(passive: PassiveLevel) -> bool {
    if clear_marker(true) {
        crate::diag::flush_service_key(passive);
        return true;
    }
    false
}

/// Clear the marker if `msi::marker_may_clear` allows it; true when this call cleared it.
fn clear_marker(stopping: bool) -> bool {
    if STARTING.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let convicted = HEALTH.load(Ordering::Relaxed) == Health::Broken.code();
    if msi::marker_may_clear(
        ARMED_AT.load(Ordering::Relaxed),
        crate::adapter::foreign_scanout::now_100ns(),
        ints_total(),
        convicted,
        stopping,
    ) && STARTING.swap(0, Ordering::AcqRel) != 0
    {
        crate::diag::record_named_bytes(b"MsiStarting", 0);
        return true;
    }
    false
}

/// A waiter's polling drain found a completion after its wait slice timed out (PASSIVE).
///
/// In message mode, after `StartDevice`: judge it (`msi::rescue_step`: enough silent ones in a
/// row convict delivery), turn the polling safety net on at the first doubt, and queue the DPC
/// so the event queue and the fences are drained too (the rescuer drained the control ring
/// only). A conviction latches INTx for the next start. The device keeps working meanwhile.
#[inline(never)]
pub(crate) fn note_rescue(passive: PassiveLevel, adapter: &AdapterContext) {
    RESCUES.fetch_add(1, Ordering::Relaxed);
    let message_mode = adapter.msi_state.load(Ordering::Acquire) != 0;
    let armed = ARMED.load(Ordering::Acquire) != 0;
    if !msi::rescue_judged(message_mode, armed, crate::ddi::escape_wait::stopping()) {
        // A stopping device's waits time out by design: not judged (no streak, no health, no
        // latch). An armed message-mode rescue still queues the DPC, as before, so the event
        // queue and the fences drain.
        if message_mode && armed {
            crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
        }
        return;
    }
    let now = ints_total();
    let prev = INTS_PREV.swap(now, Ordering::Relaxed);
    let step = msi::rescue_step(STREAK.load(Ordering::Relaxed), prev, now);
    STREAK.store(step.streak, Ordering::Relaxed);
    // A conviction is not undone by a later interrupt: `Broken` is sticky until the next start.
    if HEALTH.load(Ordering::Relaxed) != Health::Broken.code() {
        HEALTH.store(step.health.code(), Ordering::Relaxed);
    }
    if msi::polling_wanted(false, step.health) {
        POLL.store(1, Ordering::Release);
    }
    if msi::should_latch(step.health) && RUNTIME_LATCHED.swap(1, Ordering::Relaxed) == 0 {
        latch_intx(passive, msi::latch_why::SILENT);
    }
    crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
}

/// Clear a suspect state once interrupts have flowed since it was raised (`msi::reassess`), and
/// with it the polling safety net unless the latch holds it on. PASSIVE: from the periodic
/// mirror, so an end-of-start "no interrupts yet" does not leave the net on for the whole run.
fn reassess_health() {
    if ARMED.load(Ordering::Acquire) == 0 {
        return;
    }
    let health = Health::from_code(HEALTH.load(Ordering::Relaxed));
    let next = msi::reassess(health, INTS_PREV.load(Ordering::Relaxed), ints_total());
    if next != health {
        HEALTH.store(next.code(), Ordering::Relaxed);
        STREAK.store(0, Ordering::Relaxed);
        if LATCHED.load(Ordering::Relaxed) == 0 {
            POLL.store(0, Ordering::Release);
        }
    }
}

/// Whether the HPD worker should poll the used rings on a short timer (message mode, delivery
/// in doubt). One load.
#[inline]
pub(crate) fn polling() -> bool {
    POLL.load(Ordering::Acquire) != 0
}

/// The worker woke on the safety-net timer and drained.
#[inline]
pub(crate) fn note_poll() {
    POLL_N.fetch_add(1, Ordering::Relaxed);
}

/// Mirror the counters to the service key. PASSIVE only; called from
/// `publish_nvrm_counters` (the HPD worker's rate-limited mirror, the present edge and
/// StopDevice), so the registry is written first-and-periodically, never from the DPC.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    reassess_health();
    service(false);
    let n = |c: &[Padded], i: usize| c[i].0.load(Ordering::Relaxed);
    rec(b"MsiInts", ints_total());
    rec(b"MsiV0", n(&INTS, 0));
    rec(b"MsiV1", n(&INTS, 1));
    rec(b"MsiV2", n(&INTS, 2));
    rec(b"MsiV3", n(&INTS, 3));
    rec(b"MsiVOth", n(&INTS, msi::SLOT_OTHER));
    rec(b"MsiDpc0", n(&DPCS, 0));
    rec(b"MsiDpc1", n(&DPCS, 1));
    rec(b"MsiDpc2", n(&DPCS, 2));
    rec(b"MsiDpc3", n(&DPCS, 3));
    rec(b"MsiDpcOth", n(&DPCS, msi::SLOT_OTHER));
    rec(b"IntxDpc", INTX_DPCS.load(Ordering::Relaxed));
    rec(b"DpcNoCause", DPC_NO_CAUSE.load(Ordering::Relaxed));
    rec(b"IntxInts", INTX_INTS.load(Ordering::Relaxed));
    rec(b"IntxMiss", INTX_MISS.load(Ordering::Relaxed));
    rec(b"MsiIdle", MSI_IDLE.load(Ordering::Relaxed));
    rec(b"IrqRescue", RESCUES.load(Ordering::Relaxed));
    rec(b"MsiSilent", STREAK.load(Ordering::Relaxed));
    rec(b"MsiHealth", HEALTH.load(Ordering::Relaxed));
    rec(b"MsiStart", START_VERDICT.load(Ordering::Relaxed));
    rec(b"MsiPoll", POLL.load(Ordering::Relaxed));
    rec(b"MsiPollN", POLL_N.load(Ordering::Relaxed));
    rec(b"MsiPollOnly", POLL_ONLY.load(Ordering::Relaxed));
}

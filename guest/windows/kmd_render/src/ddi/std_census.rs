//! The census of the KMD's own STANDARD allocations (stage S-A0 of `docs/rm-backed-standard.md`):
//! which standard types and GDI surface types the OS asks `GetStandardAllocationDriverData` for,
//! how large, and which of them another device OPENS (the question: does an NVK DWM composing a
//! windowed legacy-blt app open the KMD standard buffer that is the Blt destination, and which
//! slot is it). Counting only; no decision reads these. The slot and name tables are
//! `helios_kmd_logic::rm_standard` (`hist_slot`, `hist_name`, `hist_open_name`,
//! `hist_slot_from_misc`, `CENSUS_COUNTERS`; host-tested, including the name scan of this file).
//!
//! Counters (service-key REG_DWORDs, names at most 14 characters; PASSIVE only, both entry
//! points are PASSIVE DDIs; zeroed at every StartDevice by [`reset_for_start`]):
//!
//! * `StdN<slot>` (14, `hist_name`): phase-2 requests per slot (`StdNPrimary`, `StdNShadow`,
//!   `StdNStaging`, `StdNGdi0` .. `StdNGdiTexCXa`, `StdNGdiOther`, `StdNOther`). Written at every
//!   request (the rate is that of window and swap-chain creation; the entry already writes four
//!   values per call).
//! * `StdBytesMiB`: cumulative size asked for, MiB (each rounded up); `StdMaxMiB`: the largest.
//! * `StdMkPid`: the process id current at the last request.
//! * `StdO<slot>` (14, `hist_open_name`): successful opens of a KMD-made standard allocation per
//!   slot, the primary included (`StdOPrimary`).
//! * `StdOpenN`: of those, the non-primary ones (the CPU-visible and GDI surfaces);
//!   `StdOpenSlot` the slot of the last one, `StdOpenPid` the opener's process id.
//!
//! The open path has no record of the creating process for these (dxgkrnl makes them on a
//! device's behalf; the identity carries no process), so "opened by another process" is read
//! by comparing `StdOpenPid` with DWM's pid, not counted here.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::rm_standard::{self as rs, HIST_SLOTS};
use helios_protocol::{
    HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK, HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_SHIFT,
    HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK, HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_SHIFT,
};

// `hist_slot_from_misc` spells the protocol's bit ranges as literals.
const _: () = assert!(
    HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_SHIFT == 24
        && HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK == 0xF << 24
        && HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_SHIFT == 20
        && HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK == 0xF << 20
);

// The fixed names, each spelled once (the host test holds this file to `CENSUS_COUNTERS`).
const BYTES_MIB: &[u8] = b"StdBytesMiB";
const MAX_MIB_NAME: &[u8] = b"StdMaxMiB";
const MK_PID: &[u8] = b"StdMkPid";
const OPEN_N_NAME: &[u8] = b"StdOpenN";
const OPEN_SLOT: &[u8] = b"StdOpenSlot";
const OPEN_PID: &[u8] = b"StdOpenPid";

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU32 = AtomicU32::new(0);
static MADE: [AtomicU32; HIST_SLOTS] = [ZERO; HIST_SLOTS];
static OPENED: [AtomicU32; HIST_SLOTS] = [ZERO; HIST_SLOTS];
static TOTAL_MIB: AtomicU32 = AtomicU32::new(0);
static MAX_MIB: AtomicU32 = AtomicU32::new(0);
static OPEN_N: AtomicU32 = AtomicU32::new(0);

/// A new generation (StartDevice): zero the counters and write zeros over their service-key
/// values, so an earlier run's census is never read as this one's. PASSIVE.
pub(crate) fn reset_for_start() {
    for slot in 0..HIST_SLOTS {
        MADE[slot].store(0, Ordering::Relaxed);
        OPENED[slot].store(0, Ordering::Relaxed);
        crate::diag::record_named_bytes(rs::hist_name(slot), 0);
        crate::diag::record_named_bytes(rs::hist_open_name(slot), 0);
    }
    TOTAL_MIB.store(0, Ordering::Relaxed);
    MAX_MIB.store(0, Ordering::Relaxed);
    OPEN_N.store(0, Ordering::Relaxed);
    for name in [BYTES_MIB, MAX_MIB_NAME, MK_PID, OPEN_N_NAME, OPEN_SLOT, OPEN_PID] {
        crate::diag::record_named_bytes(name, 0);
    }
}

/// `GetStandardAllocationDriverData` phase 2 accepted a request of `std_type` / `gdi_type`
/// (0 for a non-GDI type) whose private data says `size` bytes. PASSIVE.
pub(crate) fn note_request(std_type: u32, gdi_type: u32, size: u64) {
    let slot = rs::hist_slot(std_type, gdi_type);
    let n = MADE[slot].fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    crate::diag::record_named_bytes(rs::hist_name(slot), n);

    let mib = rs::census_mib(size);
    let total = TOTAL_MIB.fetch_add(mib, Ordering::Relaxed).wrapping_add(mib);
    crate::diag::record_named_bytes(BYTES_MIB, total);
    if MAX_MIB.fetch_max(mib, Ordering::Relaxed) < mib {
        crate::diag::record_named_bytes(MAX_MIB_NAME, mib);
    }
    crate::diag::record_named_bytes(MK_PID, crate::virtio::nvrm_window::current_pid());
}

/// `OpenAllocation` bound a device to an identified STANDARD allocation whose meta `misc_flags`
/// is `misc_flags` (after the open is registered: a refused open is not counted). An allocation
/// whose standard type is 0 is not one the KMD made through `GetStandardAllocationDriverData`
/// and is ignored. PASSIVE.
pub(crate) fn note_open(misc_flags: u32) {
    let Some(slot) = rs::hist_slot_from_misc(misc_flags) else {
        return;
    };
    let n = OPENED[slot].fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    crate::diag::record_named_bytes(rs::hist_open_name(slot), n);
    if rs::hist_slot_is_primary(slot) {
        return;
    }
    let total = OPEN_N.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    crate::diag::record_named_bytes(OPEN_N_NAME, total);
    crate::diag::record_named_bytes(OPEN_SLOT, slot as u32);
    crate::diag::record_named_bytes(OPEN_PID, crate::virtio::nvrm_window::current_pid());
}

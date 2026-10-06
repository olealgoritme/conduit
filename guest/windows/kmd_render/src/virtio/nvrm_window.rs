//! The RM window policy, driver side: the counters it publishes, who counts as the
//! privileged device, and the numbers the tables' bounds come from. The rules are
//! `helios_kmd_logic::rm_window` (who may map how many bytes of region 1) and
//! `helios_kmd_logic::rm_limits` (when a table grows and when it refuses); the doors that
//! apply them are in `virtio/gpu/nvrm_tables.rs`. Design, knobs, counters and the checklist:
//! `docs/nvrm-escape.md`, "The RM window policy".
//!
//! Everything published here is a driver-wide atomic, so a counter survives a transport
//! generation (the accounts themselves live in the transport and start empty with it).
//! Gauges (`NvWinUseMb`, `NvWinT*`, ...) are refreshed under the virtio lock by the table
//! doors, with atomic stores only; the PASSIVE publisher just reads them.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::rm_limits::Admit;
use helios_kmd_logic::rm_window::{pack_top, Config, Refusal, Snapshot, TOP_N};

use super::gpu::DeviceOwner;
use crate::adapter::AdapterContext;

// The pure crate has no protocol dependency; its flag values are the ABI's.
const _: () = {
    use helios_kmd_logic::rm_window as rw;
    assert!(rw::INFO_OWNER_LIMIT == helios_protocol::HELIOS_NVRM_WINDOW_FLAG_OWNER_LIMIT);
    assert!(rw::INFO_CAN_GROW == helios_protocol::HELIOS_NVRM_WINDOW_FLAG_CAN_GROW);
    assert!(rw::INFO_SHARED_CEILING == helios_protocol::HELIOS_NVRM_WINDOW_FLAG_SHARED_CEILING);
};

/// Window size, effective cap, reserve and policy in force (set once per transport).
static WIN_WINDOW: AtomicU64 = AtomicU64::new(0);
static WIN_CAP: AtomicU64 = AtomicU64::new(0);
static WIN_RESERVE: AtomicU64 = AtomicU64::new(0);
static WIN_POLICY: AtomicU32 = AtomicU32::new(1);
/// Non-UVM window bytes mapped now, the high-water mark since driver load, bytes inside the
/// reserve, live window mappings, owners with a row, owners marked privileged.
static WIN_IN_USE: AtomicU64 = AtomicU64::new(0);
static WIN_PEAK: AtomicU64 = AtomicU64::new(0);
static WIN_RSV_USE: AtomicU64 = AtomicU64::new(0);
static WIN_MAPS: AtomicU32 = AtomicU32::new(0);
static WIN_OWNERS: AtomicU32 = AtomicU32::new(0);
static WIN_PRIV: AtomicU32 = AtomicU32::new(0);
/// The four largest owners: pid in the high half, MiB in the low half (0 = unused rank).
static WIN_TOP: [AtomicU64; TOP_N] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// Refusals by reason, cumulative.
static R_FULL: AtomicU32 = AtomicU32::new(0);
static R_RESERVE: AtomicU32 = AtomicU32::new(0);
static R_BIG: AtomicU32 = AtomicU32::new(0);
static R_TABLE: AtomicU32 = AtomicU32::new(0);
static R_ADDR: AtomicU32 = AtomicU32::new(0);
static R_HOST: AtomicU32 = AtomicU32::new(0);
static HOST_ERRNO: AtomicU32 = AtomicU32::new(0);

/// The per-process bounds QUERY_CAPS reports (the sanity bounds of the tables).
/// Until a transport has configured them: what QUERY_CAPS always said (128 / 256).
static HANDLES_PER_OWNER: AtomicU32 = AtomicU32::new(128);
static MAPS_PER_OWNER: AtomicU32 = AtomicU32::new(256);
/// Handle table: live (handles + reservations), high-water, storage slots, growths.
pub(super) static HDL_LIVE: AtomicU32 = AtomicU32::new(0);
pub(super) static HDL_PEAK: AtomicU32 = AtomicU32::new(0);
pub(super) static HDL_CAP: AtomicU32 = AtomicU32::new(0);
pub(super) static HDL_GROWS: AtomicU32 = AtomicU32::new(0);
/// Handle reservations refused by a sanity bound: per owner, whole table, fairness.
static HDL_REF_OWNER: AtomicU32 = AtomicU32::new(0);
static HDL_REF_GLOBAL: AtomicU32 = AtomicU32::new(0);
static HDL_REF_FAIR: AtomicU32 = AtomicU32::new(0);
/// Mapping table: storage slots, growths, and its sanity refusals (all three kinds).
pub(super) static MAP_CAP: AtomicU32 = AtomicU32::new(0);
pub(super) static MAP_GROWS: AtomicU32 = AtomicU32::new(0);
static MAP_REF: AtomicU32 = AtomicU32::new(0);
/// A table that wanted to grow and could not (the allocator refused), or a reservation that
/// found no storage because growth had not caught up (needs more concurrent reservers than
/// the headroom: only a hostile process gets there).
pub(super) static TBL_OOM: AtomicU32 = AtomicU32::new(0);

/// Count one more in `NvMapQRef`, the all-reasons window refusal total. Under `NvWinPolicy`
/// = 0 it counts only what it always counted (the per-device quota, `Refusal::Quota`), so a
/// legacy run reads byte-identical counters; the new reasons then appear in `NvWinR*` only.
fn count_qref(always: bool) {
    if always || WIN_POLICY.load(Ordering::Relaxed) != 0 {
        super::nvrm::NVRM_MAP_QUOTA_REFUSED.fetch_add(1, Ordering::Relaxed);
    }
}

/// What a window map is refused with: the reason's counter, and `NvMapQRef`.
pub fn count_refusal(why: Refusal) {
    let c = match why {
        Refusal::WindowFull => &R_FULL,
        Refusal::ReserveHit => &R_RESERVE,
        Refusal::TooBig => &R_BIG,
        Refusal::TableFull => &R_TABLE,
        // The legacy quota has no counter of its own: it is in `NvMapQRef`, as before.
        Refusal::Quota => {
            count_qref(true);
            return;
        }
    };
    c.fetch_add(1, Ordering::Relaxed);
    count_qref(false);
}

/// The user view could not be made after the host mapped (no MDL, no address space).
pub fn count_addr_space() {
    R_ADDR.fetch_add(1, Ordering::Relaxed);
    count_qref(false);
}

/// The host refused the `Mmap` with `errno` (12, ENOMEM, is its window zone being full).
pub fn count_host_refused(errno: u32) {
    R_HOST.fetch_add(1, Ordering::Relaxed);
    HOST_ERRNO.store(errno, Ordering::Relaxed);
    count_qref(false);
}

/// `PIN`s refused by the per-process pin quota before any page was locked. Until this
/// counter existed that refusal left no trace in the registry at all (`NvPinErr` counts
/// only what failed after the pages were locked).
static PIN_QUOTA_REF: AtomicU32 = AtomicU32::new(0);

pub fn count_pin_quota_refusal() {
    PIN_QUOTA_REF.fetch_add(1, Ordering::Relaxed);
}

/// A table bound (not the window) refused a map: counted as a sanity refusal.
pub fn count_map_table_refusal() {
    MAP_REF.fetch_add(1, Ordering::Relaxed);
}

/// A handle reservation refused by `why`.
pub fn count_handle_refusal(why: Admit) {
    let c = match why {
        Admit::OwnerBound => &HDL_REF_OWNER,
        Admit::Unfair => &HDL_REF_FAIR,
        Admit::GlobalBound => &HDL_REF_GLOBAL,
        // Growth is the caller's to do; a reservation that still found none is `TBL_OOM`.
        Admit::NeedGrow(_) => &TBL_OOM,
        Admit::Ok => return,
    };
    c.fetch_add(1, Ordering::Relaxed);
}

/// Record the configuration of a new transport's window account and the per-process bounds
/// of its tables. PASSIVE (init), before the transport serves anything.
pub fn configure(cfg: &Config, handles_per_owner: usize, maps_per_owner: usize) {
    WIN_WINDOW.store(cfg.window, Ordering::Relaxed);
    WIN_CAP.store(cfg.cap, Ordering::Relaxed);
    WIN_RESERVE.store(cfg.reserve, Ordering::Relaxed);
    WIN_POLICY.store(cfg.policy.as_u32(), Ordering::Relaxed);
    WIN_GENERATION.fetch_add(1, Ordering::Relaxed);
    HANDLES_PER_OWNER.store(handles_per_owner.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    MAPS_PER_OWNER.store(maps_per_owner.min(u32::MAX as usize) as u32, Ordering::Relaxed);
}

/// `WINDOW_INFO` calls answered (`NvWinInfo`).
pub static INFO_CALLS: AtomicU32 = AtomicU32::new(0);
/// Bumps when the window's size or the policy may have changed: once per transport start.
static WIN_GENERATION: AtomicU64 = AtomicU64::new(0);

/// `WindowInfo.generation`: 0 before the first transport, then 1, 2, ...
pub fn generation() -> u64 {
    WIN_GENERATION.load(Ordering::Relaxed)
}

/// What `QUERY_CAPS.max_handles` reports: the per-process sanity bound in force.
pub fn handles_per_owner() -> u32 {
    HANDLES_PER_OWNER.load(Ordering::Relaxed)
}

/// What `QUERY_CAPS.max_mappings` reports.
pub fn maps_per_owner() -> u32 {
    MAPS_PER_OWNER.load(Ordering::Relaxed)
}

/// Refresh the gauges from the account. Atomic stores only: legal under the virtio lock.
pub fn mirror(s: &Snapshot) {
    WIN_IN_USE.store(s.in_use, Ordering::Relaxed);
    WIN_PEAK.fetch_max(s.in_use, Ordering::Relaxed);
    WIN_RSV_USE.store(s.reserve_use, Ordering::Relaxed);
    WIN_MAPS.store(s.maps.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
    WIN_OWNERS.store(s.owners, Ordering::Relaxed);
    WIN_PRIV.store(s.privileged_owners, Ordering::Relaxed);
    for (slot, top) in WIN_TOP.iter().zip(s.top.iter()) {
        slot.store(pack_top(*top), Ordering::Relaxed);
    }
}

/// The transport is gone: the live gauges go to zero (the high-water mark and every
/// refusal count stay).
pub fn reset_gauges() {
    WIN_IN_USE.store(0, Ordering::Relaxed);
    WIN_RSV_USE.store(0, Ordering::Relaxed);
    WIN_MAPS.store(0, Ordering::Relaxed);
    WIN_OWNERS.store(0, Ordering::Relaxed);
    WIN_PRIV.store(0, Ordering::Relaxed);
    for slot in WIN_TOP.iter() {
        slot.store(0, Ordering::Relaxed);
    }
    HDL_LIVE.store(0, Ordering::Relaxed);
}

/// Whether `owner` is the privileged device RIGHT NOW (the reserve is its to use): the KMD's
/// own RM client, or the device that holds the foreign scanout source (DWM-on-NVK). The
/// sticky half of the rule (a device that once set a scanout source stays privileged until
/// its device is destroyed) lives in the account (`mark_privileged`). Takes the scanout
/// state's leaf lock, so: never call with the virtio lock held.
pub fn live_privileged(adapter: &AdapterContext, owner: DeviceOwner) -> bool {
    owner == DeviceOwner::KMD_RM || adapter.foreign_scanout_owner_is(owner)
}

/// The calling process's id, for the per-process report. PASSIVE, in the caller's context.
pub fn current_pid() -> u32 {
    // SAFETY: `PsGetCurrentProcessId` has no preconditions at or below DISPATCH_LEVEL.
    unsafe { wdk_sys::ntddk::PsGetCurrentProcessId() as usize as u32 }
}

/// MiB for a counter: a u32 holds 4 PiB, and a larger value saturates instead of wrapping.
fn mib(bytes: u64) -> u32 {
    helios_kmd_logic::window_units::mib_u32(bytes)
}

/// Mirror the window, handle-table and map-table counters into the registry. PASSIVE only.
/// Called from `publish_nvrm_counters`.
pub fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    let cap = WIN_CAP.load(Ordering::Relaxed);
    let in_use = WIN_IN_USE.load(Ordering::Relaxed);
    rec(b"NvWinPol", WIN_POLICY.load(Ordering::Relaxed));
    rec(b"NvWinCapMb", mib(cap));
    rec(b"NvWinResMb", mib(WIN_RESERVE.load(Ordering::Relaxed)));
    rec(b"NvWinUseMb", mib(in_use));
    rec(b"NvWinPeakMb", mib(WIN_PEAK.load(Ordering::Relaxed)));
    rec(b"NvWinFreeMb", mib(cap.saturating_sub(in_use)));
    rec(b"NvWinRsvUse", mib(WIN_RSV_USE.load(Ordering::Relaxed)));
    rec(b"NvWinMaps", WIN_MAPS.load(Ordering::Relaxed));
    rec(b"NvWinOwn", WIN_OWNERS.load(Ordering::Relaxed));
    rec(b"NvWinPriv", WIN_PRIV.load(Ordering::Relaxed));
    rec(b"NvWinRFull", R_FULL.load(Ordering::Relaxed));
    rec(b"NvWinRRes", R_RESERVE.load(Ordering::Relaxed));
    rec(b"NvWinRBig", R_BIG.load(Ordering::Relaxed));
    rec(b"NvWinRTab", R_TABLE.load(Ordering::Relaxed));
    rec(b"NvWinRAddr", R_ADDR.load(Ordering::Relaxed));
    rec(b"NvWinRHost", R_HOST.load(Ordering::Relaxed));
    rec(b"NvWinHErrno", HOST_ERRNO.load(Ordering::Relaxed));
    let top = |i: usize| WIN_TOP[i].load(Ordering::Relaxed);
    rec(b"NvWinT1Pid", (top(0) >> 32) as u32);
    rec(b"NvWinT1Mb", top(0) as u32);
    rec(b"NvWinT2Pid", (top(1) >> 32) as u32);
    rec(b"NvWinT2Mb", top(1) as u32);
    rec(b"NvWinT3Pid", (top(2) >> 32) as u32);
    rec(b"NvWinT3Mb", top(2) as u32);
    rec(b"NvWinT4Pid", (top(3) >> 32) as u32);
    rec(b"NvWinT4Mb", top(3) as u32);
    // The tables behind the per-process handle and mapping bounds.
    rec(b"NvHdlLive", HDL_LIVE.load(Ordering::Relaxed));
    rec(b"NvHdlPeak", HDL_PEAK.load(Ordering::Relaxed));
    rec(b"NvHdlCap", HDL_CAP.load(Ordering::Relaxed));
    rec(b"NvHdlGrow", HDL_GROWS.load(Ordering::Relaxed));
    rec(b"NvHdlORef", HDL_REF_OWNER.load(Ordering::Relaxed));
    rec(b"NvHdlGRef", HDL_REF_GLOBAL.load(Ordering::Relaxed));
    rec(b"NvHdlFRef", HDL_REF_FAIR.load(Ordering::Relaxed));
    rec(b"NvMapTCap", MAP_CAP.load(Ordering::Relaxed));
    rec(b"NvMapTGrow", MAP_GROWS.load(Ordering::Relaxed));
    rec(b"NvMapTRef", MAP_REF.load(Ordering::Relaxed));
    rec(b"NvTblOom", TBL_OOM.load(Ordering::Relaxed));
    rec(b"NvPinQRef", PIN_QUOTA_REF.load(Ordering::Relaxed));
    rec(b"NvWinInfo", INFO_CALLS.load(Ordering::Relaxed));
    // Every refusal by a sanity bound, handles and mappings together.
    rec(
        b"NvSanityRef",
        HDL_REF_OWNER
            .load(Ordering::Relaxed)
            .saturating_add(HDL_REF_GLOBAL.load(Ordering::Relaxed))
            .saturating_add(HDL_REF_FAIR.load(Ordering::Relaxed))
            .saturating_add(MAP_REF.load(Ordering::Relaxed)),
    );
}

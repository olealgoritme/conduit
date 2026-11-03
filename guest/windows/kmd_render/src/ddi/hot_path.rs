//! Hot-path cost counters and the `NvHotPath` knob (NVIDIA UMD personality).
//!
//! The hot DDIs only bump the atomics here (legal at any IRQL): escapes,
//! `SubmitCommandToHwQueue`, `BuildPagingBuffer` and its page-table updates, and
//! every host RM round trip (`virtio::ctrl::raw_roundtrip`). The registry mirror
//! thread ([`publish`], from `mirror_thread::run_pass`) turns them into
//! per-second rates over the window since its last pass; [`tick`] asks for that
//! pass every `NvHotPubMs` (default 1000, 0 = never) from the hot paths
//! themselves, so the values stay fresh while a game runs. Nothing here writes
//! the registry outside the mirror pass. With `MirrorThread` 0 nothing is
//! published.
//!
//! Values (service key; rates are per second over the last window `HpWinMs`):
//!
//! | Name | What |
//! |---|---|
//! | `HpEscN` / `HpEscPs` | `DxgkDdiEscape` calls |
//! | `HpNvdaN` / `HpNvdaPs` | NVDA-magic escapes (NVIDIA's UMDs) |
//! | `HpNvLocN` / `HpNvLocPs` | NVDA escapes answered in the KMD (no host) |
//! | `HpNvFwdN` / `HpNvFwdPs` | NVDA RM controls forwarded to the host RM |
//! | `HpSubN` / `HpSubPs` | native HW-queue submissions (`HwsSubmit`) |
//! | `HpSoftPs` | software HW-queue submissions (paging, GDI, present contexts) |
//! | `HpRmN` / `HpRmPs` | host RM round trips, every source |
//! | `HpRmUsPs` | microseconds per second spent waiting in those round trips |
//! | `HpRmPerSub` | RM round trips per native submission in the window, x100 |
//! | `HpBpbN` / `HpBpbPs` | `BuildPagingBuffer` calls |
//! | `HpPtN` / `HpPtPs` | UpdatePageTable operations (every allocation) |
//! | `HpPtNatN` / `HpPtNatPs` | ... native leaf updates (host RM map/unmap) |
//! | `HpPtSysN` | ... VidMm's own (system process) leaf updates of native allocations |
//! | `HpPtUpN` | ... upper-level (non-leaf) updates, answered at once |
//! | `HpPtFgnN` / `HpPtFgnPs` | ... foreign (non-native) leaf updates mirrored into RM |
//! | `HpMemRtPs` | native-memory round trips per second (leaf updates, creates) |
//! | `HpQuiN` / `HpQuiUs` / `HpQuiMaxUs` | idle-channels quiesces before a live replacement, time |
//! | `HpQuiSkip` | quiesces skipped (`NvHotPath` 0x4) |
//! | `HpFgnSkip` | foreign leaf updates skipped as unchanged (`NvHotPath` 0x2) |
//! | `HpFlipFast` / `HpFlipSlow` | native flips without / with the native I/O mutex (0x1) |
//! | `HpFn0`..`HpFn7`, `HpFnN0`..`HpFnN7`, `HpFnOvf` | NVDA function census: function, count |
//! | `HpRc0`..`HpRc7`, `HpRcN0`..`HpRcN7`, `HpRcOvf` | forwarded RM control census: cmd, count |
//! | `Pg*` | the page-table update fast paths (`NvPaging`): batches, OS-descriptor reuse, the per-process index; see `virtio/native_memory.rs` `publish_hot` |
//! | `PgPh*` / `PgPv*` | where system-page / video-memory leaf updates spend their time, per phase (`helios_kmd_logic::pg_phase`); `PgHo*` / `PgNmWait*` native-memory mutex holds per kind and waits; `PgSt*` STATE spinlock waits / holds; `PgCopy*` / `PgView*` CPU copies of `with_cpu_range`; see `virtio/native_memory.rs` `publish_phases` |
//! | `HpKnob` | `NvHotPath` in force |
//! | `HpWinMs` | the window the rates were taken over |

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::hot_path::{self as hp, Census, HotPath, Rate};

const UNREAD: u32 = u32::MAX;

static KNOB: AtomicU32 = AtomicU32::new(UNREAD);
static PERIOD_MS: AtomicU32 = AtomicU32::new(UNREAD);
/// Interrupt time (ms, never 0) the last periodic pass was asked for.
static LAST_REQ_MS: AtomicU32 = AtomicU32::new(0);
/// Interrupt time (ms) of the last publish that took rates.
static LAST_PUB_MS: AtomicU32 = AtomicU32::new(0);
static WINDOW_MS: AtomicU32 = AtomicU32::new(0);

// ── counters (any IRQL) ──────────────────────────────────────────────────────────────────────
pub(crate) static ESCAPES: AtomicU32 = AtomicU32::new(0);
pub(crate) static NVDA: AtomicU32 = AtomicU32::new(0);
pub(crate) static NVDA_LOCAL: AtomicU32 = AtomicU32::new(0);
pub(crate) static NVDA_FORWARD: AtomicU32 = AtomicU32::new(0);
pub(crate) static RM_TRIPS: AtomicU32 = AtomicU32::new(0);
pub(crate) static RM_US: AtomicU32 = AtomicU32::new(0);
pub(crate) static PT_ALL: AtomicU32 = AtomicU32::new(0);
pub(crate) static PT_NATIVE: AtomicU32 = AtomicU32::new(0);
pub(crate) static PT_SYSTEM: AtomicU32 = AtomicU32::new(0);
pub(crate) static PT_UPPER: AtomicU32 = AtomicU32::new(0);
pub(crate) static PT_FOREIGN: AtomicU32 = AtomicU32::new(0);
pub(crate) static QUIESCES: AtomicU32 = AtomicU32::new(0);
pub(crate) static QUIESCE_US: AtomicU32 = AtomicU32::new(0);
pub(crate) static QUIESCE_MAX_US: AtomicU32 = AtomicU32::new(0);
pub(crate) static QUIESCE_SKIPPED: AtomicU32 = AtomicU32::new(0);
pub(crate) static FOREIGN_SKIPPED: AtomicU32 = AtomicU32::new(0);
pub(crate) static FLIP_FAST: AtomicU32 = AtomicU32::new(0);
pub(crate) static FLIP_SLOW: AtomicU32 = AtomicU32::new(0);

/// NVDA functions (every NVDA escape) and forwarded RM control commands.
pub(crate) static FUNCTIONS: Census<8> = Census::new();
pub(crate) static CONTROLS: Census<8> = Census::new();

static R_ESC: Rate = Rate::new();
static R_NVDA: Rate = Rate::new();
static R_LOCAL: Rate = Rate::new();
static R_FORWARD: Rate = Rate::new();
static R_SUB: Rate = Rate::new();
static R_SOFT: Rate = Rate::new();
static R_RM: Rate = Rate::new();
static R_RM_US: Rate = Rate::new();
static R_BPB: Rate = Rate::new();
static R_PT: Rate = Rate::new();
static R_PT_NATIVE: Rate = Rate::new();
static R_PT_FOREIGN: Rate = Rate::new();
static R_MEM_RT: Rate = Rate::new();
/// The last `HpRmPerSub` (kept over a short window).
static RM_PER_SUB: AtomicU32 = AtomicU32::new(0);

#[inline]
pub(crate) fn count(counter: &AtomicU32) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// One host RM round trip that took `ticks` (100 ns). Any IRQL.
#[inline]
pub(crate) fn note_rm_trip(ticks: u64) {
    RM_TRIPS.fetch_add(1, Ordering::Relaxed);
    RM_US.fetch_add(u32::try_from(ticks / 10).unwrap_or(u32::MAX), Ordering::Relaxed);
}

/// One idle-channels quiesce that took `ticks` (100 ns).
pub(crate) fn note_quiesce(ticks: u64) {
    let us = u32::try_from(ticks / 10).unwrap_or(u32::MAX);
    QUIESCES.fetch_add(1, Ordering::Relaxed);
    QUIESCE_US.fetch_add(us, Ordering::Relaxed);
    QUIESCE_MAX_US.fetch_max(us, Ordering::Relaxed);
}

/// `NvHotPath` (PASSIVE: read once, cached). See `helios_kmd_logic::hot_path`.
pub(crate) fn knob(_passive: crate::irql::PassiveLevel) -> HotPath {
    let mut value = KNOB.load(Ordering::Relaxed);
    if value == UNREAD {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvHotPath"), 0);
        if value == UNREAD {
            value = 0;
        }
        KNOB.store(value, Ordering::Relaxed);
    }
    HotPath(value)
}

/// StartDevice: read `NvHotPath` and `NvHotPubMs` again (the statics outlive a
/// `pnputil /restart-device`) and start the census over. PASSIVE.
pub(crate) fn reread_knobs() {
    KNOB.store(UNREAD, Ordering::Relaxed);
    if let Some(passive) = crate::irql::PassiveLevel::try_assume() {
        let _ = knob(passive);
    }
    let period = crate::diag::read_config_dword(
        crate::diag::KnobName::new(b"NvHotPubMs"),
        hp::DEFAULT_PUBLISH_MS,
    );
    PERIOD_MS.store(if period == UNREAD { hp::DEFAULT_PUBLISH_MS } else { period }, Ordering::Relaxed);
    FUNCTIONS.reset();
    CONTROLS.reset();
}

/// [`tick_at`] with a clock read of its own.
#[inline]
pub(crate) fn tick() {
    tick_at(crate::adapter::foreign_scanout::now_100ns());
}

/// From the hot paths, with the interrupt time (100 ns) they already read: ask the
/// mirror thread for a pass every `NvHotPubMs`. Two relaxed loads when none is
/// due; a CAS and `KeSetEvent(Wait = FALSE)` when one is. Any IRQL up to DISPATCH.
#[inline]
pub(crate) fn tick_at(now_100ns: u64) {
    let period = match PERIOD_MS.load(Ordering::Relaxed) {
        // Not read yet (before the first StartDevice): the default.
        UNREAD => hp::DEFAULT_PUBLISH_MS,
        p => p,
    };
    if period == 0 {
        return;
    }
    let now = helios_kmd_logic::vsync_rate::ms_from_100ns(now_100ns).max(1);
    let last = LAST_REQ_MS.load(Ordering::Relaxed);
    if !hp::publish_due(now, last, period) {
        return;
    }
    if LAST_REQ_MS
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
        && crate::ddi::mirror_thread::running()
    {
        crate::ddi::mirror_thread::request();
    }
}

/// The mirror pass (PASSIVE): rates over the window since the last pass, the
/// totals, the census, and the values the hot paths used to write themselves.
pub(crate) fn publish() {
    use crate::diag::record_named_bytes as rec;
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    // NVIDIA personality only: nothing native ran in the default one (whose escapes,
    // round trips and page-table updates are counted too, but not published).
    let (submits, soft) = crate::ddi::hws::submit_counts();
    if r(&NVDA) == 0 && submits == 0 && r(&PT_NATIVE) == 0 && r(&PT_SYSTEM) == 0 {
        return;
    }
    let now = crate::adapter::AdapterContext::interrupt_time_ms().max(1);
    let last = LAST_PUB_MS.load(Ordering::Relaxed);
    let window = if last == 0 { 0 } else { now.wrapping_sub(last) };
    if window >= hp::MIN_WINDOW_MS || last == 0 {
        LAST_PUB_MS.store(now, Ordering::Relaxed);
        if last != 0 {
            WINDOW_MS.store(window, Ordering::Relaxed);
        }
    }
    // The first pass only takes the baselines (`Rate::sample`'s first call).
    let w = if last == 0 { 0 } else { window };

    let esc = R_ESC.sample(r(&ESCAPES), w).1;
    let nvda = R_NVDA.sample(r(&NVDA), w).1;
    let local = R_LOCAL.sample(r(&NVDA_LOCAL), w).1;
    let forward = R_FORWARD.sample(r(&NVDA_FORWARD), w).1;
    let (sub_delta, sub_ps) = R_SUB.sample(submits, w);
    let soft_ps = R_SOFT.sample(soft, w).1;
    let (rm_delta, rm_ps) = R_RM.sample(r(&RM_TRIPS), w);
    let rm_us_ps = R_RM_US.sample(r(&RM_US), w).1;
    let bpb = crate::ddi::build_paging_buffer::paging_call_count();
    let bpb_ps = R_BPB.sample(bpb, w).1;
    let pt_ps = R_PT.sample(r(&PT_ALL), w).1;
    let pt_native_ps = R_PT_NATIVE.sample(r(&PT_NATIVE), w).1;
    let pt_foreign_ps = R_PT_FOREIGN.sample(r(&PT_FOREIGN), w).1;
    let mem_rt_ps = R_MEM_RT.sample(crate::virtio::native_memory::round_trip_count(), w).1;
    if w >= hp::MIN_WINDOW_MS {
        RM_PER_SUB.store(hp::ratio_x100(rm_delta, sub_delta), Ordering::Relaxed);
    }

    rec(b"HpKnob", match KNOB.load(Ordering::Relaxed) {
        UNREAD => 0,
        k => k,
    });
    rec(b"HpWinMs", WINDOW_MS.load(Ordering::Relaxed));
    rec(b"HpEscN", r(&ESCAPES));
    rec(b"HpEscPs", esc);
    rec(b"HpNvdaN", r(&NVDA));
    rec(b"HpNvdaPs", nvda);
    rec(b"HpNvLocN", r(&NVDA_LOCAL));
    rec(b"HpNvLocPs", local);
    rec(b"HpNvFwdN", r(&NVDA_FORWARD));
    rec(b"HpNvFwdPs", forward);
    rec(b"HpSubN", submits);
    rec(b"HpSubPs", sub_ps);
    rec(b"HpSoftPs", soft_ps);
    rec(b"HpRmN", r(&RM_TRIPS));
    rec(b"HpRmPs", rm_ps);
    rec(b"HpRmUsPs", rm_us_ps);
    rec(b"HpRmPerSub", RM_PER_SUB.load(Ordering::Relaxed));
    rec(b"HpBpbN", bpb);
    rec(b"HpBpbPs", bpb_ps);
    rec(b"HpPtN", r(&PT_ALL));
    rec(b"HpPtPs", pt_ps);
    rec(b"HpPtNatN", r(&PT_NATIVE));
    rec(b"HpPtNatPs", pt_native_ps);
    rec(b"HpPtSysN", r(&PT_SYSTEM));
    rec(b"HpPtUpN", r(&PT_UPPER));
    rec(b"HpPtFgnN", r(&PT_FOREIGN));
    rec(b"HpPtFgnPs", pt_foreign_ps);
    rec(b"HpMemRtPs", mem_rt_ps);
    rec(b"HpQuiN", r(&QUIESCES));
    rec(b"HpQuiUs", r(&QUIESCE_US));
    rec(b"HpQuiMaxUs", r(&QUIESCE_MAX_US));
    rec(b"HpQuiSkip", r(&QUIESCE_SKIPPED));
    rec(b"HpFgnSkip", r(&FOREIGN_SKIPPED));
    rec(b"HpFlipFast", r(&FLIP_FAST));
    rec(b"HpFlipSlow", r(&FLIP_SLOW));
    publish_census(&FUNCTIONS, *b"HpFn0", *b"HpFnN0", b"HpFnOvf");
    publish_census(&CONTROLS, *b"HpRc0", *b"HpRcN0", b"HpRcOvf");

    // The values the paging and allocation paths wrote per call before (same names).
    crate::ddi::build_paging_buffer::publish_hot();
    crate::ddi::create_allocation::publish_hot();
    crate::virtio::native_memory::publish_hot();
    crate::virtio::native_scanout::publish_hot();
    crate::ddi::nv_copy::publish_hot();
}

fn publish_census<const N: usize>(census: &Census<N>, key: [u8; 5], count: [u8; 6], overflow: &[u8]) {
    for i in 0..N.min(10) {
        let (k, n) = census.slot(i);
        if k == 0 {
            break;
        }
        let digit = b'0' + i as u8;
        let mut kn = key;
        kn[4] = digit;
        let mut cn = count;
        cn[5] = digit;
        crate::diag::record_named_bytes(&kn, k);
        crate::diag::record_named_bytes(&cn, n);
    }
    crate::diag::record_named_bytes(overflow, census.overflow());
}

//! Flip retirement latency and frame-time consistency: the measurement half of
//! `docs/kmd-rm-client.md` 15.18.15. The model, the bucket edges and the percentile estimate
//! are `helios_kmd_logic::flip_retire` (host-tested); this file is the statics and the mirror.
//!
//! WHAT IS MEASURED (all atomics; the registry is written only by [`publish_counters`], PASSIVE,
//! from the stall-diagnosis mirror that already runs once a second):
//!
//! * `FlipLat0..7`, `FlipMaxUs`, `FlipP50Us`, `FlipP99Us`, `FlipRetN`: for every flip
//!   `SetVidPnSourceAddress` issued, the time from the DDI's entry to the start of the
//!   `CRTC_VSYNC` tick that carried its address (the instant dxgkrnl can retire it). The ring
//!   of issued flips ([`fr::RING`]) is matched by address at every delivered tick
//!   ([`fr::retire_match`]); `FlipSkip` counts older unretired flips a tick passed over (a
//!   newer address carried: the coalescing question), `FlipLive` the unretired ones now.
//!   `FlipMaxT` / `FlipMaxSite` / `FlipMaxFl`: when the longest one retired, which step the HPD
//!   worker was in (`stall_diag::site`), and `pending | gate << 1 | venus mutex held << 2` at that
//!   tick: the site that held the longest flip.
//! * `FlipPrgLat0..7`, `FlipPrgMax`, `FlipPrgP99`: entry to the worker's publication of the
//!   address (the programming finished: the bind accepted by the host, the foreign `take`), i.e.
//!   present-to-programmed on every class.
//! * `FlipHostLat0..7`, `FlipHostMax`, `FlipHostP99`: entry to the `ForeignFlip` host flip
//!   SUBMITTED (the viewer shows it a moment later).
//! * `IfGap0..7`, `IfGapMax` (ms), `IfN`, `IfIdle`, `IfStall8`, `IfP99Us`: the interval between
//!   two retiring ticks in vblank periods; `IfStall8` counts intervals over 8 ms, `IfIdle`
//!   those over 250 ms (idleness, not in the histogram). Read the DELTAS during motion.
//! * `VbTicks`, `VbUsed`, `VbUsedPm`: delivered ticks with a source shown, of which those that
//!   retired a flip, and the share in permille: the "every vblank used" ratio.
//! * `VsLate0..7`, `VsLateMaxUs`: how late the heartbeat's tick ran against its scheduled
//!   deadline (the one-shot high-resolution timer's own jitter).
//! * `FlipPh0..3`, `FlipInDpc`: where in the period the DDI ran (quarters after the last tick)
//!   and how many DDIs ran while this driver's own device DPC was inside
//!   `DxgkCbNotifyDpc` (dxgkrnl issuing the next flip as part of retiring the previous one).
//!
//! `FlipLat` (service key, default 1) = 0 turns all of it off (the hooks are one relaxed load).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::flip_retire::{self as fr, BUCKETS, RING};

/// `FlipLat` in force (default 1).
static ON: AtomicU32 = AtomicU32::new(1);

#[allow(clippy::declare_interior_mutable_const)]
const Z64: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const Z32: AtomicU32 = AtomicU32::new(0);

// The ring of issued flips: address and entry time per slot, the live mask (issued and not yet
// carried by a tick), the announced mask and the "programming time taken" mask.
static ADDR: [AtomicU64; RING] = [Z64; RING];
static TIME: [AtomicU64; RING] = [Z64; RING];
static LIVE: AtomicU32 = AtomicU32::new(0);
static ANNOUNCED: AtomicU32 = AtomicU32::new(0);
static PROGRAMMED: AtomicU32 = AtomicU32::new(0);
static HEAD: AtomicU32 = AtomicU32::new(0);
/// The slot of the newest issue (`mark_announced` names it).
static LAST_SLOT: AtomicU32 = AtomicU32::new(0);

static LAT: [AtomicU32; BUCKETS] = [Z32; BUCKETS];
static LAT_MAX_US: AtomicU32 = AtomicU32::new(0);
static LAT_MAX_T: AtomicU32 = AtomicU32::new(0);
static LAT_MAX_SITE: AtomicU32 = AtomicU32::new(0);
static LAT_MAX_FL: AtomicU32 = AtomicU32::new(0);
static RET_N: AtomicU32 = AtomicU32::new(0);
static SKIPPED: AtomicU32 = AtomicU32::new(0);
static PRG: [AtomicU32; BUCKETS] = [Z32; BUCKETS];
static PRG_MAX_US: AtomicU32 = AtomicU32::new(0);
static HOST: [AtomicU32; BUCKETS] = [Z32; BUCKETS];
static HOST_MAX_US: AtomicU32 = AtomicU32::new(0);
/// The entry time of the flip `ForeignFlip` last programmed, until its host flip is submitted.
static PROG_T: AtomicU64 = AtomicU64::new(0);
static GAP: [AtomicU32; BUCKETS] = [Z32; BUCKETS];
static GAP_MAX_MS: AtomicU32 = AtomicU32::new(0);
static GAP_N: AtomicU32 = AtomicU32::new(0);
static GAP_IDLE: AtomicU32 = AtomicU32::new(0);
static GAP_STALL: AtomicU32 = AtomicU32::new(0);
static LAST_RETIRE: AtomicU64 = AtomicU64::new(0);
static VB_TICKS: AtomicU32 = AtomicU32::new(0);
static VB_USED: AtomicU32 = AtomicU32::new(0);
static LATE: [AtomicU32; BUCKETS] = [Z32; BUCKETS];
static LATE_MAX_US: AtomicU32 = AtomicU32::new(0);
static PHASE: [AtomicU32; 4] = [Z32; 4];
static IN_DPC_N: AtomicU32 = AtomicU32::new(0);
/// Non-zero while this driver's device DPC is inside `DxgkCbNotifyDpc`.
static IN_DPC: AtomicU32 = AtomicU32::new(0);
static LAST_TICK_AT: AtomicU64 = AtomicU64::new(0);
static PERIOD: AtomicU64 = AtomicU64::new(0);
/// The announce counters of `flip_announce` that ride the retire match.
static FA_TICK: AtomicU32 = AtomicU32::new(0);
/// The block owes the service key one full write (zeros included) for this generation.
static MIRROR_PENDING: AtomicU32 = AtomicU32::new(1);

#[inline]
fn on() -> bool {
    ON.load(Ordering::Relaxed) != 0
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// `FlipLat` and the zeroing of a new generation. PASSIVE (StartDevice).
pub(crate) fn start_generation() {
    let v = crate::diag::read_config_dword(crate::diag::knobs::FLIP_LAT, 1);
    ON.store(u32::from(v != 0), Ordering::Relaxed);
    for a in ADDR.iter().chain(TIME.iter()) {
        a.store(0, Ordering::Relaxed);
    }
    for c in [
        &LIVE,
        &ANNOUNCED,
        &PROGRAMMED,
        &HEAD,
        &LAST_SLOT,
        &LAT_MAX_US,
        &LAT_MAX_T,
        &LAT_MAX_SITE,
        &LAT_MAX_FL,
        &RET_N,
        &SKIPPED,
        &PRG_MAX_US,
        &HOST_MAX_US,
        &GAP_MAX_MS,
        &GAP_N,
        &GAP_IDLE,
        &GAP_STALL,
        &VB_TICKS,
        &VB_USED,
        &LATE_MAX_US,
        &IN_DPC_N,
        &IN_DPC,
        &FA_TICK,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for h in LAT
        .iter()
        .chain(PRG.iter())
        .chain(HOST.iter())
        .chain(GAP.iter())
        .chain(LATE.iter())
        .chain(PHASE.iter())
    {
        h.store(0, Ordering::Relaxed);
    }
    PROG_T.store(0, Ordering::Relaxed);
    LAST_RETIRE.store(0, Ordering::Relaxed);
    LAST_TICK_AT.store(0, Ordering::Relaxed);
    PERIOD.store(0, Ordering::Relaxed);
    MIRROR_PENDING.store(1, Ordering::Release);
}

/// `SetVidPnSourceAddress` (or a DMA flip record) issued `address`. Any IRQL, atomics only.
pub(crate) fn note_issue(address: u64) {
    if !on() || address == 0 {
        return;
    }
    let t = now();
    // Where in the period the DDI ran: quarters after the last tick.
    let tick = LAST_TICK_AT.load(Ordering::Relaxed);
    let period = PERIOD.load(Ordering::Relaxed);
    if tick != 0 && period != 0 && t >= tick {
        let q = (((t - tick) as u128 * 4) / period as u128).min(3) as usize;
        PHASE[q].fetch_add(1, Ordering::Relaxed);
    }
    if IN_DPC.load(Ordering::Relaxed) != 0 {
        IN_DPC_N.fetch_add(1, Ordering::Relaxed);
    }
    let n = HEAD.fetch_add(1, Ordering::AcqRel);
    let i = (n as usize) % RING;
    let bit = 1u32 << i;
    LIVE.fetch_and(!bit, Ordering::AcqRel);
    ANNOUNCED.fetch_and(!bit, Ordering::Relaxed);
    PROGRAMMED.fetch_and(!bit, Ordering::Relaxed);
    ADDR[i].store(address, Ordering::Relaxed);
    TIME[i].store(t, Ordering::Relaxed);
    LAST_SLOT.store(i as u32, Ordering::Relaxed);
    LIVE.fetch_or(bit, Ordering::AcqRel);
}

/// The newest issued flip was announced at the DDI (`flip_announce`): its retire is a `FaTick`.
pub(crate) fn mark_announced() {
    if !on() {
        return;
    }
    let i = LAST_SLOT.load(Ordering::Relaxed) as usize % RING;
    ANNOUNCED.fetch_or(1 << i, Ordering::Relaxed);
}

/// The device DPC entered / left `DxgkCbNotifyDpc`.
pub(crate) fn dpc_enter() {
    IN_DPC.store(1, Ordering::Relaxed);
}
pub(crate) fn dpc_leave() {
    IN_DPC.store(0, Ordering::Relaxed);
}

fn load_ring() -> ([u64; RING], [u64; RING]) {
    let mut a = [0u64; RING];
    let mut t = [0u64; RING];
    for i in 0..RING {
        a[i] = ADDR[i].load(Ordering::Relaxed);
        t[i] = TIME[i].load(Ordering::Relaxed);
    }
    (a, t)
}

/// The heartbeat tick ran `now` against its scheduled `deadline` (100 ns). DISPATCH.
pub(crate) fn note_tick_late(now: u64, deadline: u64) {
    if !on() {
        return;
    }
    let d = now.saturating_sub(deadline);
    LATE[fr::late_bucket_100ns(d)].fetch_add(1, Ordering::Relaxed);
    LATE_MAX_US.fetch_max((d / 10).min(u32::MAX as u64) as u32, Ordering::Relaxed);
}

/// A `CRTC_VSYNC` carrying `phys` was delivered by the tick that started at `tick_t`
/// (`period` = the vblank period, 100 ns). DISPATCH, atomics only.
pub(crate) fn on_delivered_tick(adapter: &crate::adapter::AdapterContext, phys: u64, tick_t: u64, period: u64) {
    if !on() {
        return;
    }
    LAST_TICK_AT.store(tick_t, Ordering::Relaxed);
    PERIOD.store(period, Ordering::Relaxed);
    if phys == 0 {
        return;
    }
    VB_TICKS.fetch_add(1, Ordering::Relaxed);
    let (addrs, times) = load_ring();
    let live = LIVE.load(Ordering::Acquire);
    let head = HEAD.load(Ordering::Acquire);
    let Some(hit) = fr::retire_match(&addrs, &times, live, head, phys, tick_t) else {
        return;
    };
    // Claim the slot: a tick races nothing but itself, but the DDI can recycle a slot.
    let bit = 1u32 << hit.idx;
    if LIVE.fetch_and(!bit, Ordering::AcqRel) & bit == 0 {
        return;
    }
    if hit.skipped != 0 {
        LIVE.fetch_and(!hit.skipped, Ordering::AcqRel);
        SKIPPED.fetch_add(hit.skipped.count_ones(), Ordering::Relaxed);
    }
    VB_USED.fetch_add(1, Ordering::Relaxed);
    RET_N.fetch_add(1, Ordering::Relaxed);
    if ANNOUNCED.fetch_and(!bit, Ordering::Relaxed) & bit != 0 {
        FA_TICK.fetch_add(1, Ordering::Relaxed);
    }
    let us = (hit.latency_100ns / 10).min(u32::MAX as u64) as u32;
    LAT[fr::lat_bucket(us as u64)].fetch_add(1, Ordering::Relaxed);
    if LAT_MAX_US.fetch_max(us, Ordering::Relaxed) < us {
        // The longest so far: when, which step the worker was in, and what it owed.
        LAT_MAX_T.store(
            helios_kmd_logic::vsync_rate::ms_from_100ns(tick_t),
            Ordering::Relaxed,
        );
        LAT_MAX_SITE.store(crate::ddi::stall_diag::hpd_site(), Ordering::Relaxed);
        let flags = u32::from(adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0)
            | (u32::from(crate::adapter::gate_active(
                adapter.vidpn_programming.load(Ordering::Acquire),
            )) << 1)
            | (u32::from(crate::ddi::device_lost::venus_held_ms() != 0) << 2);
        LAT_MAX_FL.store(flags, Ordering::Relaxed);
    }
    // The interval between two retiring ticks.
    let previous = LAST_RETIRE.swap(tick_t, Ordering::Relaxed);
    match fr::classify_gap(previous, tick_t, period) {
        fr::Gap::First => {}
        fr::Gap::Idle => {
            GAP_IDLE.fetch_add(1, Ordering::Relaxed);
        }
        fr::Gap::Interval { bucket, stall } => {
            GAP[bucket].fetch_add(1, Ordering::Relaxed);
            GAP_N.fetch_add(1, Ordering::Relaxed);
            if stall {
                GAP_STALL.fetch_add(1, Ordering::Relaxed);
            }
            let ms = ((tick_t - previous) / helios_kmd_logic::vsync_rate::UNITS_PER_MS)
                .min(u32::MAX as u64) as u32;
            GAP_MAX_MS.fetch_max(ms, Ordering::Relaxed);
        }
    }
}

/// The worker (or the DDI, for a kept picture) published `address`: the programming of its
/// newest unmeasured flip is done. Any IRQL, atomics only.
pub(crate) fn note_published(address: u64) {
    if !on() || address == 0 {
        return;
    }
    let t = now();
    let head = HEAD.load(Ordering::Acquire);
    for k in 0..RING {
        let i = (head.wrapping_sub(1 + k as u32) as usize) % RING;
        if ADDR[i].load(Ordering::Relaxed) != address {
            continue;
        }
        let bit = 1u32 << i;
        if PROGRAMMED.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
            return;
        }
        let entry = TIME[i].load(Ordering::Relaxed);
        if t >= entry {
            let us = ((t - entry) / 10).min(u32::MAX as u64) as u32;
            PRG[fr::lat_bucket(us as u64)].fetch_add(1, Ordering::Relaxed);
            PRG_MAX_US.fetch_max(us, Ordering::Relaxed);
        }
        return;
    }
}

/// `ForeignFlip` programmed `address` (PASSIVE worker): remember when its flip entered, for
/// [`note_host_submit`].
pub(crate) fn note_programmed(address: u64) {
    if !on() || address == 0 {
        return;
    }
    let head = HEAD.load(Ordering::Acquire);
    for k in 0..RING {
        let i = (head.wrapping_sub(1 + k as u32) as usize) % RING;
        if ADDR[i].load(Ordering::Relaxed) == address {
            PROG_T.store(TIME[i].load(Ordering::Relaxed), Ordering::Relaxed);
            return;
        }
    }
}

/// The `ForeignFlip` host flip of the picture last programmed was submitted at `at` (100 ns).
pub(crate) fn note_host_submit(at: u64) {
    if !on() {
        return;
    }
    let entry = PROG_T.swap(0, Ordering::Relaxed);
    if entry != 0 && at >= entry {
        let us = ((at - entry) / 10).min(u32::MAX as u64) as u32;
        HOST[fr::lat_bucket(us as u64)].fetch_add(1, Ordering::Relaxed);
        HOST_MAX_US.fetch_max(us, Ordering::Relaxed);
    }
}

fn load_hist(h: &[AtomicU32; BUCKETS]) -> [u32; BUCKETS] {
    let mut out = [0u32; BUCKETS];
    for (o, c) in out.iter_mut().zip(h.iter()) {
        *o = c.load(Ordering::Relaxed);
    }
    out
}

/// `prefix` followed by one decimal digit, for the indexed histograms.
fn rec_hist(prefix: &[u8], counts: &[u32]) {
    let mut name = [0u8; 16];
    name[..prefix.len()].copy_from_slice(prefix);
    for (i, &c) in counts.iter().enumerate() {
        name[prefix.len()] = b'0' + i as u8;
        crate::diag::record_named_bytes(&name[..prefix.len() + 1], c);
    }
}

/// The retire ticks the announce path counted (`FaTick`), for its mirror.
pub(crate) fn announced_ticks() -> u32 {
    FA_TICK.load(Ordering::Relaxed)
}

/// Mirror the block to the service key. PASSIVE only; the stall-diagnosis mirror calls it
/// (once a second from the worker's periodic dump). Nothing is written until a flip was issued
/// or a tick delivered, except once per generation (zeros), so a value an earlier run left is
/// never read as this one's.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    let owed = MIRROR_PENDING.swap(0, Ordering::AcqRel) != 0;
    rec(b"FlipLatOn", ON.load(Ordering::Relaxed));
    if !on() {
        return;
    }
    if !owed && HEAD.load(Ordering::Relaxed) == 0 && VB_TICKS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let lat = load_hist(&LAT);
    let max_us = LAT_MAX_US.load(Ordering::Relaxed);
    rec_hist(b"FlipLat", &lat);
    rec(b"FlipMaxUs", max_us);
    rec(b"FlipMaxT", LAT_MAX_T.load(Ordering::Relaxed));
    rec(b"FlipMaxSite", LAT_MAX_SITE.load(Ordering::Relaxed));
    rec(b"FlipMaxFl", LAT_MAX_FL.load(Ordering::Relaxed));
    rec(b"FlipP50Us", fr::lat_percentile_us(&lat, 500, max_us));
    rec(b"FlipP99Us", fr::lat_percentile_us(&lat, 990, max_us));
    rec(b"FlipRetN", RET_N.load(Ordering::Relaxed));
    rec(b"FlipSkip", SKIPPED.load(Ordering::Relaxed));
    rec(b"FlipLive", LIVE.load(Ordering::Relaxed).count_ones());
    let prg = load_hist(&PRG);
    rec_hist(b"FlipPrgLat", &prg);
    let prg_max = PRG_MAX_US.load(Ordering::Relaxed);
    rec(b"FlipPrgMax", prg_max);
    rec(b"FlipPrgP99", fr::lat_percentile_us(&prg, 990, prg_max));
    let host = load_hist(&HOST);
    rec_hist(b"FlipHostLat", &host);
    let host_max = HOST_MAX_US.load(Ordering::Relaxed);
    rec(b"FlipHostMax", host_max);
    rec(b"FlipHostP99", fr::lat_percentile_us(&host, 990, host_max));
    rec(b"FlipInDpc", IN_DPC_N.load(Ordering::Relaxed));
    let mut phase = [0u32; 4];
    for (p, c) in phase.iter_mut().zip(PHASE.iter()) {
        *p = c.load(Ordering::Relaxed);
    }
    rec_hist(b"FlipPh", &phase);
    let gap = load_hist(&GAP);
    let gap_max = GAP_MAX_MS.load(Ordering::Relaxed);
    rec_hist(b"IfGap", &gap);
    rec(b"IfGapMax", gap_max);
    rec(b"IfN", GAP_N.load(Ordering::Relaxed));
    rec(b"IfIdle", GAP_IDLE.load(Ordering::Relaxed));
    rec(b"IfStall8", GAP_STALL.load(Ordering::Relaxed));
    rec(
        b"IfP99Us",
        fr::gap_percentile_us(
            &gap,
            PERIOD.load(Ordering::Relaxed),
            990,
            gap_max.saturating_mul(1000),
        ),
    );
    let ticks = VB_TICKS.load(Ordering::Relaxed);
    let used = VB_USED.load(Ordering::Relaxed);
    rec(b"VbTicks", ticks);
    rec(b"VbUsed", used);
    rec(b"VbUsedPm", fr::used_permille(used, ticks));
    rec_hist(b"VsLate", &load_hist(&LATE));
    rec(b"VsLateMaxUs", LATE_MAX_US.load(Ordering::Relaxed));
}

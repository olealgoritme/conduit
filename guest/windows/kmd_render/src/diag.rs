//! TEMPORARY post-start bring-up tracer (remove once Code 43 / AddAdapter clears).
//!
//! dxgkrnl's StartAdapter→AddAdapter sequence drives a series of our DDIs and can
//! fail internally (e.g. `STATUS_OBJECT_NAME_NOT_FOUND`) with no NTSTATUS we get
//! to see. To find which DDI dxgkrnl is calling (and which we answer how) right
//! before it gives up, each instrumented PASSIVE-level DDI calls [`record`],
//! which appends a `REG_DWORD` breadcrumb as values `S0`, `S1`, `S2`, … under
//! `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`. After a repro read them in
//! order (`reg query` / `Get-ItemProperty`); the last few before the failure
//! point at the culprit.
//!
//! IRQL: `RtlWriteRegistryValue` requires PASSIVE_LEVEL — only call [`record`]
//! from PASSIVE DDIs (never the DPC/ISR or DISPATCH paging paths).
//!
//! Breadcrumb code encoding (high byte = which DDI, low bytes = detail):
//!   0x01_00_0000 | type     QueryAdapterInfo entry (DXGK_QUERYADAPTERINFOTYPE)
//!   0x02_00_0000 | type     QueryAdapterInfo answered STATUS_NOT_SUPPORTED (type)
//!   0x03_00_0000 | ordinal  GetNodeMetadata entry
//!   0x04_00_0000            QueryInterface entry (followed by the GUID Data1)
//!   0x05_00_0000            GetRootPageTableSize entry
//!   0x06_00_0000            CreateProcess entry
//!   raw value               an interface GUID Data1 logged after a 0x04 marker
//!
//! COLLISIONS FIXED 2026-07-27 (T4b/R722) — these are owner debugging ABI, so
//! the old values are recorded here rather than only in git:
//!   0x0B00_00E7  was BOTH venus-bring-up-failed (ddi/lifecycle.rs) and
//!                HPD-worker-create-failed (adapter.rs), both inside the
//!                StartDevice window. HPD moved to 0x0B00_00EA.
//!   0x0E00_0001  was BOTH DestroyDevice entry (device.rs) and
//!                ExchangePreStartInfo entry (display.rs). The 0x0E00_* block is
//!                the device-teardown family, so ExchangePreStartInfo moved to
//!                0x0E10_0001 (and its success marker 0x0E00_0002 -> 0x0E10_0002).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use wdk_sys::ntddk::RtlWriteRegistryValue;

/// Cached `DiagLevel` service-key knob (u32::MAX = not read yet).
/// Level 0 (default, PSC stage): the `S<idx>` breadcrumb ring is OFF — it is
/// bring-up archaeology, and its steady-state writers (QueryAdapterInfo
/// polling, paging/allocation paths) each cost a synchronous kernel registry
/// write. Level >= 1 restores full breadcrumb tracing. Named counters
/// (`record_named*`) are NOT gated here — each caller decides its own flush
/// cadence; failure counters must stay loud.
static DIAG_LEVEL: AtomicU32 = AtomicU32::new(u32::MAX);

/// Read (once) and cache the `DiagLevel` knob. PASSIVE_LEVEL only — every
/// legal [`record`] caller already is. Benign race on first concurrent calls.
pub fn level() -> u32 {
    let cached = DIAG_LEVEL.load(Ordering::Relaxed);
    if cached != u32::MAX {
        return cached;
    }
    let level = read_config_dword(knobs::DIAG_LEVEL, 0);
    DIAG_LEVEL.store(level, Ordering::Relaxed);
    level
}

/// Forget the cached `DiagLevel` and read it again, mirroring the value in force (`DiagLvl`,
/// written on every read, 0 included). StartDevice: the static outlives a `pnputil
/// /restart-device` (the image is not reloaded), so without this a changed `DiagLevel` needed a
/// reboot. PASSIVE_LEVEL.
pub fn reread_level() -> u32 {
    DIAG_LEVEL.store(u32::MAX, Ordering::Relaxed);
    let level = level();
    record_named_bytes(b"DiagLvl", level);
    level
}

/// `RTL_REGISTRY_SERVICES` — Path is relative to
/// `\Registry\Machine\System\CurrentControlSet\Services`.
const RTL_REGISTRY_SERVICES: u32 = 1;
/// `REG_DWORD`.
const REG_DWORD: u32 = 4;
/// `REG_QWORD`.
const REG_QWORD: u32 = 11;
/// `REG_BINARY`.
const REG_BINARY: u32 = 3;
/// Cap on breadcrumbs so a chatty steady state can't grow the key unbounded.
const MAX_STEPS: u32 = 3000;

static STEP: AtomicU32 = AtomicU32::new(0);

/// `"helios_kmd_render\0"` as UTF-16 — the service subkey under Services.
static SERVICE_NAME: [u16; 18] = [
    b'h' as u16,
    b'e' as u16,
    b'l' as u16,
    b'i' as u16,
    b'o' as u16,
    b's' as u16,
    b'_' as u16,
    b'k' as u16,
    b'm' as u16,
    b'd' as u16,
    b'_' as u16,
    b'r' as u16,
    b'e' as u16,
    b'n' as u16,
    b'd' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
];

/// Write a DWORD breadcrumb to a FIXED value name (not the `S<idx>` ring). The
/// `S*` ring is overwritten within ~1s by steady-state QueryAdapterInfo polling,
/// so it is useless for one-shot tracing of a rare DDI (e.g. Present). A fixed
/// name persists until the next write, so it can be read live from the registry.
/// `name` must be a NUL-terminated UTF-16 value name. PASSIVE_LEVEL only.
///
/// PRIVATE on purpose: [`record_named_bytes`] is the entry point. `display.rs`
/// carried a byte-identical copy of that wrapper (`rec_named`) that called this
/// directly, which is how 79 registry writes accumulated on the Present path
/// outside every policy this module documents. With the raw writer private,
/// a future bypass has to be a deliberate edit to this file.
fn record_named(name: &[u16], mut code: u32) {
    // SAFETY: PASSIVE_LEVEL (see module note). `name` is a caller-provided
    // NUL-terminated UTF-16 value name; ValueData points to a 4-byte DWORD that
    // RtlWriteRegistryValue copies before returning.
    unsafe {
        let _ = RtlWriteRegistryValue(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            name.as_ptr(),
            REG_DWORD,
            (&mut code as *mut u32).cast::<core::ffi::c_void>(),
            4,
        );
    }
}

/// [`record_named`] for a 64-bit value (`REG_QWORD`, 8 bytes): one registry transaction, so a
/// reader sees the whole value or none of it. PASSIVE_LEVEL only.
fn record_named_q(name: &[u16], mut value: u64) {
    // SAFETY: PASSIVE_LEVEL (see module note). `name` is a NUL-terminated UTF-16 value name;
    // ValueData points to an 8-byte QWORD that RtlWriteRegistryValue copies before returning.
    unsafe {
        let _ = RtlWriteRegistryValue(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            name.as_ptr(),
            REG_QWORD,
            (&mut value as *mut u64).cast::<core::ffi::c_void>(),
            8,
        );
    }
}

/// A `REG_BINARY` value of `data.len()` bytes (`StgRing`, `ddi::stage_trace`). Not part of the
/// mirror's changed-only cache. PASSIVE_LEVEL only.
pub fn record_named_binary(name: &[u8], data: &[u8]) {
    let mut buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        buf[i] = name[i] as u16;
        i += 1;
    }
    buf[n] = 0;
    let Ok(len) = u32::try_from(data.len()) else {
        return;
    };
    // SAFETY: PASSIVE_LEVEL (see module note). `buf` is a NUL-terminated UTF-16 value name;
    // ValueData points to `len` readable bytes that RtlWriteRegistryValue copies before
    // returning (it does not write through the pointer).
    unsafe {
        let _ = RtlWriteRegistryValue(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            buf.as_ptr(),
            REG_BINARY,
            data.as_ptr() as *mut core::ffi::c_void,
            len,
        );
    }
}

/// The registry mirror's pass-local write policy (15.18.16): inside a pass of `ddi::mirror_thread`
/// (and only there) a write whose value is what the registry already holds is skipped, and a short
/// rest is taken every few writes so a hundred-write pass does not keep one processor and the
/// registry lock for tens of milliseconds. Every OTHER writer is untouched, except that each write
/// from anywhere updates the cache, so the cache is the registry's content (a name written by a
/// worker one-shot and by the mirror cannot be skipped wrongly).
mod mirror {
    use super::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

    const SLOTS: usize = 2048;
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    /// One entry per name hash: a 32-bit tag of the name in the high half (never 0: an empty slot
    /// matches nothing) and the last value written in the low half.
    static CACHE: [AtomicU64; SLOTS] = [Z; SLOTS];
    /// The thread inside a pass (0 = none), `MirChanged` and `MirYield` in force, writes counted
    /// in a pass since the last rest, writes made / skipped / rests taken (`MirWrN`, `MirSkipN`,
    /// `MirYlds`; owned by `ddi::mirror_thread`).
    static PASS_THREAD: AtomicUsize = AtomicUsize::new(0);
    static CHANGED_ONLY: AtomicU32 = AtomicU32::new(1);
    static YIELD_EVERY: AtomicU32 = AtomicU32::new(0);
    static SINCE_REST: AtomicU32 = AtomicU32::new(0);
    pub(super) static WRITES: AtomicU32 = AtomicU32::new(0);
    pub(super) static SKIPPED: AtomicU32 = AtomicU32::new(0);

    extern "system" {
        /// `PsGetCurrentThread()` (exported; `KeGetCurrentThread` is an inline in wdm.h and is not): any IRQL.
        fn PsGetCurrentThread() -> usize;
    }

    /// FNV-1a over the UTF-16 units of the value name.
    fn hash(name: &[u16]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for c in name {
            h ^= *c as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// `(slot, tag)` of a name.
    pub(super) fn key(name: &[u16]) -> (usize, u32) {
        let h = hash(name);
        (((h ^ (h >> 32)) as usize) & (SLOTS - 1), ((h >> 32) as u32) | 1)
    }

    fn entry(tag: u32, value: u32) -> u64 {
        ((tag as u64) << 32) | value as u64
    }

    /// The calling thread is the one inside a pass.
    pub(super) fn in_pass() -> bool {
        let t = PASS_THREAD.load(Ordering::Relaxed);
        // SAFETY: a scalar read of the current thread pointer, any IRQL.
        t != 0 && t == unsafe { PsGetCurrentThread() }
    }

    /// The pass's write is redundant: the registry already holds `value` under this name.
    pub(super) fn unchanged(k: (usize, u32), value: u32) -> bool {
        CHANGED_ONLY.load(Ordering::Relaxed) != 0
            && CACHE[k.0].load(Ordering::Relaxed) == entry(k.1, value)
    }

    /// The cache holds exactly `value` under this name (regardless of `MirChanged` and of the
    /// calling thread: the per-flip one-value breadcrumbs always use it).
    pub(super) fn cached_equal(k: (usize, u32), value: u32) -> bool {
        CACHE[k.0].load(Ordering::Relaxed) == entry(k.1, value)
    }

    /// `value` was written under this name, by any thread.
    pub(super) fn wrote(k: (usize, u32), value: u32) {
        CACHE[k.0].store(entry(k.1, value), Ordering::Relaxed);
    }

    /// Forget everything (the registry may have been edited by hand: every value is rewritten).
    pub(crate) fn forget_all() {
        for c in &CACHE {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// A pass begins on the calling thread, with the knobs in force.
    pub(crate) fn begin_pass(changed_only: bool, yield_every: u32) {
        CHANGED_ONLY.store(changed_only as u32, Ordering::Relaxed);
        YIELD_EVERY.store(yield_every, Ordering::Relaxed);
        SINCE_REST.store(0, Ordering::Relaxed);
        // SAFETY: as in `in_pass`.
        PASS_THREAD.store(unsafe { PsGetCurrentThread() }, Ordering::Release);
    }

    /// The pass is over.
    pub(crate) fn end_pass() {
        PASS_THREAD.store(0, Ordering::Release);
    }

    /// One write was made inside a pass: count it, and tell whether a rest is due.
    pub(super) fn made_write() -> bool {
        WRITES.fetch_add(1, Ordering::Relaxed);
        let every = YIELD_EVERY.load(Ordering::Relaxed);
        if every == 0 {
            return false;
        }
        let n = SINCE_REST.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if n >= every {
            SINCE_REST.store(0, Ordering::Relaxed);
            return true;
        }
        false
    }
}

pub(crate) use mirror::{begin_pass as mirror_begin_pass, end_pass as mirror_end_pass, forget_all as mirror_forget_all};

/// Registry writes the mirror's passes made and skipped as unchanged (`MirWrN`, `MirSkipN`).
pub(crate) fn mirror_write_counts() -> (u32, u32) {
    (
        mirror::WRITES.load(Ordering::Relaxed),
        mirror::SKIPPED.load(Ordering::Relaxed),
    )
}

/// Zero the two mirror write counters (StartDevice).
pub(crate) fn mirror_reset_counts() {
    mirror::WRITES.store(0, Ordering::Relaxed);
    mirror::SKIPPED.store(0, Ordering::Relaxed);
}

/// A lifecycle failure that must stay visible on a **default** boot.
///
/// [`record`] returns early when `DiagLevel` is 0 (the default), so a refusal
/// reported through it leaves no trace at all in production — the driver starts
/// degraded and silent. The module contract at the top of this file already says
/// failure counters must stay loud; this enum is how that is enforced. A
/// `FaultCounter` cannot be passed to the gated ring, and a raw `u32` breadcrumb
/// cannot be passed to [`fault`], so at every converted site "this failure is
/// reported through the lossy mechanism" is a type error.
///
/// It does not stop a future author from reaching for [`record`] on a *new*
/// failure path; that remains a review rule.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FaultCounter {
    /// `VirtioGpu::init` failed — value is the resulting NTSTATUS. The adapter
    /// starts render-only with no transport.
    StVio,
    /// Venus bring-up failed — value is the resulting NTSTATUS. Transport is up
    /// but there is no page-table window.
    StVnu,
    /// The HPD worker thread could not be created — value is the NTSTATUS.
    StHpd,
    /// `MmAllocateContiguousMemory` for the paging RAM returned null — value is
    /// the requested size in bytes.
    StRam,
    /// The BAR segment size was rejected — value is the rejected size in MiB.
    StBar,
    /// The display half asked the host for its scan-out mode but the transport
    /// was gone — value is the NTSTATUS. The mode falls back to a fabricated
    /// default, so the OS is told about a monitor whose size we invented.
    StTxG,
    /// The transport answered the mode query but reported nothing usable —
    /// value is 1. Same fallback, different cause.
    StMdB,
    /// The display half was demoted to render-only for this start because the
    /// transport is absent — value is the NTSTATUS that killed it, or 1 if the
    /// transport was already gone for another reason. The adapter still binds.
    StNoTx,
    /// `DxgkDdiDispatchIoRequest` was called — value is the IoControlCode. A
    /// WDDM display miniport is effectively never called on this legacy
    /// video-port path, so any movement here is itself the news.
    StVrp,
    /// `DxgkDdiQueryChildStatus` was called with the display half off — value is
    /// the child status Type. Behaviour-neutral in the field
    /// (`NumberOfChildren` is 0 in that configuration), so movement means the
    /// two are out of step.
    StQcs,
    /// `MmMapIoSpace` failed for the virtio ISR-status register — value is the
    /// failure count. NOT a benign degrade on this INTx device: with no ISR ack
    /// the level-triggered line stays asserted and Windows' interrupt-storm
    /// detector Code-43s the adapter.
    StIsr,
    /// A Venus LINEAR scan-out copy was requested with no target image — value
    /// is 0. Previously reported only through the DiagLevel-gated `diag(0x0136)`,
    /// so a default boot saw nothing but `ScCpy=0xE` / `CpCpy=0xE3`.
    CpTgtE,
    /// The virtio control ring latched its corruption failure — value is
    /// `DRAIN_BAD_TOKEN`. Reported from `ResetFromTimeout`, on change only, so a
    /// TDR storm cannot become a registry write storm.
    StRing,
    /// `stop_hpd` could not prove the HPD worker exited — value is the
    /// `ObReferenceObjectByHandle` status (STATUS_SUCCESS means the bounded join
    /// timed out instead). The adapter context is deliberately leaked rather
    /// than freed under a live worker.
    StHpdX,
    /// The virtio device did not clear its status within the reset handshake's
    /// spin bound — value is the spin count. Not reachable on the supported host
    /// (QEMU services the status write synchronously inside `virtio_reset()`),
    /// but every later assumption in `init` rests on that reset.
    StVioR,
    /// A `DXGK_DRIVERCAPS` field did not fit the versioned buffer dxgkrnl
    /// supplied and was SKIPPED — value is the count of skipped fields on the
    /// last `DXGKQAITYPE_DRIVERCAPS` query. The adapter still reports the
    /// maximal valid prefix, so this is a truncated capability surface rather
    /// than a failure, but any movement means the OS is being told less than
    /// this driver believes it said. Expected 0 on 24H2, which passes the full
    /// 592-byte struct.
    CapTrunc,
    /// `BarSegMode` held a value that is no longer a segment topology — value is
    /// that stale number. The adapter binds the production shape anyway. Most
    /// likely a VM left set from one of the deleted Code-43 bisect arms
    /// (1/2/5/11); the only legal values are 0 and 10.
    BarMCo,
    /// The proposed segment table broke the AddAdapter ordering rule and was
    /// REFUSED — value is the `SegmentRuleViolation` code (1 = a cpu-host
    /// segment was not last, 2 = too many segments). The adapter binds with the
    /// aperture-only shape instead of landing in Code 43, which is the whole
    /// point: the rule was previously enforced by nothing and its violation
    /// surfaced only as a dead device.
    SegRule,
    /// The BAR segment was dropped from the REPORTED table while the driver's
    /// own `bar_segment` said otherwise — value is 1. StartDevice clears
    /// `bar_segment` in the same step, so this counts a state divergence that
    /// was PREVENTED. Any movement means the reported topology and the
    /// allocation path disagreed about which segment ids exist.
    SegDiv,
    /// The descriptor pass of the two-call segment protocol disagreed with the
    /// count reported on the descriptor-NULL call — value is the count the
    /// render pass wanted. The write loop is clamped to the reported count, so
    /// this is the counter behind that clamp's SAFETY comment. Not constructible
    /// with an immutable post-StartDevice table; it exists so that stays true.
    SegCntMis,
    /// An escape arrived with `DXGKARG_ESCAPE.Flags.HardwareAccess` set — value
    /// is the running count.
    ///
    /// This must read 0. A HardwareAccess escape makes dxgkrnl take the adapter
    /// core resource EXCLUSIVE, which first runs `FlushAllDevice` against a
    /// kwait-parked queue and wedges the whole graphics stack — the wedge class
    /// the 26th session killed. Until now that contract was enforced entirely in
    /// another codebase (the Mesa ICD's `HELIOS_ESCAPE_HW` kill switch), so an
    /// ICD rebuild or a stale test-VM environment regressed it with no counter
    /// and no breadcrumb: the only symptom was a frozen desktop, indistinguishable
    /// from a venus hang.
    EscHwA,
    /// An escape arrived with `DXGKARG_ESCAPE.Flags.NoAdapterSynchronization`
    /// set — value is the running count. Same class as [`Self::EscHwA`]: a
    /// second env-driven ICD flag riding the same struct, whose own ICD comment
    /// records a cold boot producing black output.
    EscNoSy,
}

impl FaultCounter {
    /// Registry value name. Must stay ≤14 bytes: [`record_named_bytes`]
    /// truncates beyond that, which would silently merge two counters.
    const fn name(self) -> &'static [u8] {
        match self {
            FaultCounter::StVio => b"StVio",
            FaultCounter::StVnu => b"StVnu",
            FaultCounter::StHpd => b"StHpd",
            FaultCounter::StRam => b"StRam",
            FaultCounter::StBar => b"StBar",
            FaultCounter::StTxG => b"StTxG",
            FaultCounter::StMdB => b"StMdB",
            FaultCounter::StNoTx => b"StNoTx",
            FaultCounter::StVrp => b"StVrp",
            FaultCounter::StQcs => b"StQcs",
            FaultCounter::StIsr => b"StIsr",
            FaultCounter::CpTgtE => b"CpTgtE",
            FaultCounter::StRing => b"StRing",
            FaultCounter::StHpdX => b"StHpdX",
            FaultCounter::StVioR => b"StVioR",
            FaultCounter::CapTrunc => b"CapTrunc",
            FaultCounter::BarMCo => b"BarMCo",
            FaultCounter::SegRule => b"SegRule",
            FaultCounter::SegDiv => b"SegDiv",
            FaultCounter::SegCntMis => b"SegCntMis",
            FaultCounter::EscHwA => b"EscHwA",
            FaultCounter::EscNoSy => b"EscNoSy",
        }
    }

    /// Every counter, so StartDevice can zero the whole set in one place.
    const ALL: &'static [FaultCounter] = &[
        FaultCounter::StVio,
        FaultCounter::StVnu,
        FaultCounter::StHpd,
        FaultCounter::StRam,
        FaultCounter::StBar,
        FaultCounter::StTxG,
        FaultCounter::StMdB,
        FaultCounter::StNoTx,
        FaultCounter::StVrp,
        FaultCounter::StQcs,
        FaultCounter::StIsr,
        FaultCounter::CpTgtE,
        FaultCounter::StRing,
        FaultCounter::StHpdX,
        FaultCounter::StVioR,
        FaultCounter::CapTrunc,
        FaultCounter::BarMCo,
        FaultCounter::SegRule,
        FaultCounter::SegDiv,
        FaultCounter::SegCntMis,
        FaultCounter::EscHwA,
        FaultCounter::EscNoSy,
    ];
}

/// Compile-time proof that no fault-counter name is truncated.
///
/// [`record_named_bytes`] silently clamps to [`MAX_CONFIG_NAME`], so two counters
/// whose names share a 14-byte prefix would MERGE into one registry value — a
/// failure counter reading someone else's number. The rule was a doc comment on
/// [`FaultCounter::name`]; this makes it a build failure.
///
/// Scope, stated honestly: this covers the counter set, and [`KnobName`] covers
/// every knob READ. The ~478 remaining raw `record_named_bytes(b"…")` call sites
/// still rely on the review rule — converting them is a mechanical sweep too
/// large to land inside a tranche that has to stay reviewable.
const _: () = {
    let mut i = 0;
    while i < FaultCounter::ALL.len() {
        assert!(
            FaultCounter::ALL[i].name().len() <= MAX_CONFIG_NAME,
            "FaultCounter name exceeds MAX_CONFIG_NAME and would merge with another counter"
        );
        i += 1;
    }
};

/// Report a lifecycle failure through the **ungated** named-counter path.
/// PASSIVE_LEVEL only, like every other writer here.
pub fn fault(counter: FaultCounter, value: u32) {
    record_named_bytes(counter.name(), value);
}

/// Zero every [`FaultCounter`] once, at StartDevice entry.
///
/// Registry values persist across boots, so without this a stale nonzero value
/// from an earlier boot is indistinguishable from a fault that happened on this
/// one. The gate's rule is "verify a counter moved this boot"; this is what
/// makes that rule applicable.
pub fn reset_fault_counters() {
    let mut i = 0;
    while i < FaultCounter::ALL.len() {
        record_named_bytes(FaultCounter::ALL[i].name(), 0);
        i += 1;
    }
}

/// Sampling period for [`sample_tick`] at the default `DiagLevel`.
///
/// 600 is the period this driver already uses for its other throttled
/// telemetry (`create_allocation`'s breadcrumbs, the scanout pacing block), so
/// a sampled value refreshes about every 10 seconds at 60 Hz.
pub const SAMPLE_EVERY: u32 = 600;

/// The THIRD diag channel, alongside [`fault`] (ungated named counter) and
/// [`record`] (DiagLevel-gated ring): throttled named IDENTITY values.
///
/// Returns whether this call is a sampled one, so a whole identity block can be
/// gated on a single tick and therefore stay internally consistent — every
/// value in a sampled block comes from the same operation.
///
/// The problem it exists for: `record_named*` is one synchronous
/// `RtlWriteRegistryValue` per call with no gate, and `dxgkddi_present_inner`
/// performed ~30 of them for a DWM flip and ~55 for an app BLT — almost none of
/// them failure counters, just per-call dumps of geometry, formats, handles and
/// sizes that mattered during bring-up. That is a per-frame kernel registry tax
/// on the exact path the PSC stage is trying to measure.
///
/// Policy, deliberately explicit at every call site:
///   - a FAILURE goes through [`fault`] or an unconditional `record_named_bytes`;
///   - an IDENTITY value goes through this;
///   - bring-up archaeology goes through [`record`].
/// At `DiagLevel >= 1` this returns true every time, restoring the per-call
/// behaviour for a debugging session.
pub fn sample_tick(ticks: &AtomicU32) -> bool {
    let n = ticks.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    level() >= 1 || n == 1 || n % SAMPLE_EVERY == 0
}

/// The last value a [`record_named_on_change`] site wrote; starts as "never".
pub struct NamedLast(core::sync::atomic::AtomicU64);

impl NamedLast {
    pub const fn new() -> Self {
        Self(core::sync::atomic::AtomicU64::new(u64::MAX))
    }
}

/// A fixed-name outcome value ("what the last operation did") written only
/// when it differs from the value this name last got, so the registry still
/// always holds the latest outcome but a steady success costs no synchronous
/// `RtlWriteRegistryValue` per call. EVERY write of `name` (success and
/// failure arms alike) must go through the same `last`, or a skipped write
/// could leave a stale failure code in place. `DiagLevel >= 1` writes always.
pub fn record_named_on_change(name: &[u8], value: u32, last: &NamedLast) {
    let prev = last.0.swap(u64::from(value), Ordering::Relaxed);
    if prev != u64::from(value) || level() >= 1 {
        record_named_bytes(name, value);
    }
}

/// One-shot throttled identity value, for a site with no surrounding block.
/// Same policy as [`sample_tick`].
pub fn sample_named(name: &[u8], value: u32, ticks: &AtomicU32) {
    if sample_tick(ticks) {
        record_named_bytes(name, value);
    }
}

/// One counter in a [`CounterBlock`].
pub struct CounterEntry {
    /// Registry value name (≤14 chars, as [`record_named_bytes`] requires).
    pub name: &'static [u8],
    pub value: CounterRef,
    /// A FAILURE counter: when one of these changes, the block flushes
    /// immediately regardless of the throttle, so a failure always surfaces on
    /// the operation that produced it.
    pub failure: bool,
}

/// The two atomic widths this driver's counter blocks hold. `U64Low` reports the
/// low 32 bits, exactly as the hand-rolled dumps did.
pub enum CounterRef {
    U32(&'static AtomicU32),
    U64Low(&'static core::sync::atomic::AtomicU64),
}

impl CounterRef {
    fn load(&self) -> u32 {
        match self {
            CounterRef::U32(a) => a.load(Ordering::Relaxed),
            CounterRef::U64Low(a) => a.load(Ordering::Relaxed) as u32,
        }
    }
}

/// How often a [`CounterBlock`] mirrors itself into the registry.
pub enum FlushPolicy {
    /// Every call (for blocks that only run at a rate that is already low).
    EveryOp,
    /// The 1st call and every Nth after it, at the default `DiagLevel`.
    EveryNth(u32),
}

/// A named counter block that can only be emitted through a throttled emitter.
///
/// Three modules each implemented the same "flush my counters to fixed registry
/// names" routine, and they had already drifted on throttling: the GDI executor
/// deferred to every 64th batch, while the paging block ran at the tail of EVERY
/// content op (24 synchronous registry writes) and the aperture block on every
/// map, unmap and refusal (16). Under VidMm eviction pressure the paging path is
/// per-allocation, so a storm of N allocations performed 24N registry writes
/// INSIDE BuildPagingBuffer — inflating paging latency and therefore the storm
/// (x-dup-dead-27).
///
/// Values stay cumulative atomics, so flushing less often does not change what a
/// `reg query` reads at rest; only failure latency changes, and the
/// flush-on-failure-change rule bounds that. Every atomic stays a named `static`,
/// so the TDR report and ntoseye symbol reads are unaffected.
pub struct CounterBlock {
    pub entries: &'static [CounterEntry],
    /// Call counter driving [`FlushPolicy::EveryNth`].
    pub ticks: &'static AtomicU32,
    /// Last observed sum of the `failure` entries, for the flush-on-change rule.
    pub failures: &'static AtomicU32,
    pub policy: FlushPolicy,
}

impl CounterBlock {
    /// Mirror the block into the registry if the policy (or a changed failure
    /// counter, or `DiagLevel >= 1`) says so. PASSIVE_LEVEL only.
    pub fn flush(&self) {
        let mut fail_sum: u32 = 0;
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].failure {
                fail_sum = fail_sum.wrapping_add(self.entries[i].value.load());
            }
            i += 1;
        }
        let previous = self.failures.swap(fail_sum, Ordering::Relaxed);
        let n = self.ticks.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let due = match self.policy {
            FlushPolicy::EveryOp => true,
            FlushPolicy::EveryNth(period) => n == 1 || n % period == 0,
        };
        // A changed failure counter always wins over the throttle.
        if !(due || fail_sum != previous || level() >= 1) {
            return;
        }
        let mut i = 0;
        while i < self.entries.len() {
            record_named_bytes(self.entries[i].name, self.entries[i].value.load());
            i += 1;
        }
    }
}

/// The service key as a native registry path, for `ZwOpenKey`.
const SERVICE_KEY_PATH: &[u8] =
    b"\\Registry\\Machine\\System\\CurrentControlSet\\Services\\helios_kmd_render";

pub(crate) const fn widen<const N: usize>(ascii: &[u8]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < N {
        out[i] = ascii[i] as u16;
        i += 1;
    }
    out
}

static SERVICE_KEY_PATH_W: [u16; SERVICE_KEY_PATH.len()] =
    widen::<{ SERVICE_KEY_PATH.len() }>(SERVICE_KEY_PATH);

/// `UNICODE_STRING`, spelled out because `wdk-sys` 0.5 gives no helper to build one
/// from a static (same reason `kobj.rs` declares its own `Ex*` timer imports).
#[repr(C)]
struct NtUnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

/// `OBJECT_ATTRIBUTES` (48 bytes on x64).
#[repr(C)]
struct NtObjectAttributes {
    length: u32,
    root_directory: *mut core::ffi::c_void,
    object_name: *mut NtUnicodeString,
    attributes: u32,
    security_descriptor: *mut core::ffi::c_void,
    security_quality_of_service: *mut core::ffi::c_void,
}

const _: () = assert!(core::mem::size_of::<NtObjectAttributes>() == 48);
const _: () = assert!(core::mem::size_of::<NtUnicodeString>() == 16);

#[link(name = "ntoskrnl")]
extern "system" {
    fn ZwOpenKey(
        key_handle: *mut *mut core::ffi::c_void,
        desired_access: u32,
        object_attributes: *mut NtObjectAttributes,
    ) -> i32;
    fn ZwFlushKey(key_handle: *mut core::ffi::c_void) -> i32;
    fn ZwSetValueKey(
        key_handle: *mut core::ffi::c_void,
        value_name: *mut NtUnicodeString,
        title_index: u32,
        value_type: u32,
        data: *mut core::ffi::c_void,
        data_size: u32,
    ) -> i32;
    fn IoOpenDeviceRegistryKey(
        physical_device_object: *mut core::ffi::c_void,
        device_registry_key_type: u32,
        desired_access: u32,
        device_registry_key: *mut *mut core::ffi::c_void,
    ) -> i32;
}

/// `OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE`.
const OBJ_KEY_ATTRIBUTES: u32 = 0x40 | 0x200;
/// `KEY_QUERY_VALUE`. `ZwFlushKey` needs no particular access to the handle.
const KEY_QUERY_VALUE: u32 = 0x1;

/// `PLUGPLAY_REGKEY_DEVICE`: the device's hardware key (`Enum\...\Device Parameters`),
/// the key `HKR` of an INF `.HW` section writes to and PnP reads `MSISupported` from.
/// (`PLUGPLAY_REGKEY_DRIVER` is 2: the software key under the class.)
const PLUGPLAY_REGKEY_DEVICE: u32 = 1;
/// `KEY_ALL_ACCESS`.
const KEY_ALL_ACCESS: u32 = 0x000F_003F;
/// `KEY_SET_VALUE`.
const KEY_SET_VALUE: u32 = 0x2;

/// Write one `REG_DWORD` under a subkey of a device's hardware key. Returns the NTSTATUS of
/// the step that failed, or 0. `subkey` and `value_name` are UTF-16 WITHOUT a terminator; the
/// subkey must already exist (the INF creates the ones the driver uses).
///
/// For PnP policy values that are read when the device is next started (`Interrupt
/// Management\MessageSignaledInterruptProperties\MSISupported`): writing one here changes
/// the NEXT start, and the current one only if PnP has not read it yet. PASSIVE_LEVEL, from a
/// DDI that is handed the PDO (`DxgkDdiAddDevice`).
pub(crate) fn write_device_key_dword(
    _passive: crate::irql::PassiveLevel,
    pdo: *mut core::ffi::c_void,
    subkey: &[u16],
    value_name: &[u16],
    value: u32,
) -> i32 {
    let mut device_key: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL (the token); `pdo` is the physical device object dxgkrnl handed
    // to AddDevice; the out-handle is a live local. The handle, if returned, is closed below.
    let opened = unsafe {
        IoOpenDeviceRegistryKey(pdo, PLUGPLAY_REGKEY_DEVICE, KEY_ALL_ACCESS, &mut device_key)
    };
    if opened < 0 || device_key.is_null() {
        return if opened < 0 { opened } else { -1 };
    }
    let sub_bytes = (subkey.len() * 2) as u16;
    let mut sub_name = NtUnicodeString {
        length: sub_bytes,
        maximum_length: sub_bytes,
        buffer: subkey.as_ptr() as *mut u16,
    };
    let mut attributes = NtObjectAttributes {
        length: core::mem::size_of::<NtObjectAttributes>() as u32,
        root_directory: device_key,
        object_name: &mut sub_name,
        attributes: OBJ_KEY_ATTRIBUTES,
        security_descriptor: core::ptr::null_mut(),
        security_quality_of_service: core::ptr::null_mut(),
    };
    let mut sub: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: as above; `attributes` and the name it points to outlive the call, and
    // `subkey` is the caller's slice.
    let mut status = unsafe { ZwOpenKey(&mut sub, KEY_SET_VALUE, &mut attributes) };
    if status >= 0 && !sub.is_null() {
        let value_bytes = (value_name.len() * 2) as u16;
        let mut name = NtUnicodeString {
            length: value_bytes,
            maximum_length: value_bytes,
            buffer: value_name.as_ptr() as *mut u16,
        };
        let mut data = value;
        // SAFETY: `sub` is the subkey just opened with KEY_SET_VALUE; `name` and `data`
        // are live locals the call copies before returning.
        status = unsafe {
            ZwSetValueKey(
                sub,
                &mut name,
                0,
                REG_DWORD,
                (&mut data as *mut u32).cast(),
                4,
            )
        };
        // SAFETY: closes the handle opened above.
        let _ = unsafe { wdk_sys::ntddk::ZwClose(sub as wdk_sys::HANDLE) };
    } else if status >= 0 {
        status = -1;
    }
    // SAFETY: closes the handle IoOpenDeviceRegistryKey returned.
    let _ = unsafe { wdk_sys::ntddk::ZwClose(device_key as wdk_sys::HANDLE) };
    status
}

/// Force the service key (and so every breadcrumb written to it) to disk.
///
/// `RtlWriteRegistryValue` only updates the in-memory hive; the lazy writer would
/// lose the last values of a stop that bugchecks. This pays one synchronous hive
/// flush, which is why it is called twice per StopDevice and nowhere else. Best
/// effort: every failure is ignored.
///
/// PASSIVE_LEVEL only (`ZwOpenKey` / `ZwFlushKey` / `ZwClose`).
pub fn flush_service_key(_passive: crate::irql::PassiveLevel) {
    let bytes = (SERVICE_KEY_PATH_W.len() * 2) as u16;
    let mut name = NtUnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: SERVICE_KEY_PATH_W.as_ptr() as *mut u16,
    };
    let mut attributes = NtObjectAttributes {
        length: core::mem::size_of::<NtObjectAttributes>() as u32,
        root_directory: core::ptr::null_mut(),
        object_name: &mut name,
        attributes: OBJ_KEY_ATTRIBUTES,
        security_descriptor: core::ptr::null_mut(),
        security_quality_of_service: core::ptr::null_mut(),
    };
    let mut key: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: PASSIVE_LEVEL (the token); `attributes` and the name it points to
    // outlive the call, and the path buffer is a static. The handle, if opened,
    // is a kernel handle closed below.
    unsafe {
        if ZwOpenKey(&mut key, KEY_QUERY_VALUE, &mut attributes) >= 0 && !key.is_null() {
            let _ = ZwFlushKey(key);
            let _ = wdk_sys::ntddk::ZwClose(key as wdk_sys::HANDLE);
        }
    }
}

/// `record_named` convenience: build the UTF-16 value name from an ASCII byte
/// slice (≤14 chars). PASSIVE_LEVEL only.
pub fn record_named_bytes(name: &[u8], value: u32) {
    let mut buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        buf[i] = name[i] as u16;
        i += 1;
    }
    buf[n] = 0;
    let key = mirror::key(&buf[..n]);
    let in_pass = mirror::in_pass();
    if in_pass && mirror::unchanged(key, value) {
        mirror::SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    record_named(&buf[..=n], value);
    mirror::wrote(key, value);
    if in_pass && mirror::made_write() {
        crate::ddi::mirror_thread::rest();
    }
}

/// [`record_named_bytes`] unless the registry already holds `value` under this name, as far as the
/// writes of this driver tell (every write from every thread updates the same cache). For the
/// one-value breadcrumbs of a per-flip path (`VpDSt`: the status of every programming, almost
/// always the same), which cost the HPD worker a registry transaction of a hundred microseconds or
/// more per flip, between two flips. The value is rewritten at once when it changes, and by the
/// mirror's full refresh at the latest 30 s after it was lost (`MirChanged`). PASSIVE_LEVEL only.
pub fn record_named_changed(name: &[u8], value: u32) {
    let mut buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        buf[i] = name[i] as u16;
        i += 1;
    }
    buf[n] = 0;
    let key = mirror::key(&buf[..n]);
    if mirror::cached_equal(key, value) {
        return;
    }
    record_named(&buf[..=n], value);
    mirror::wrote(key, value);
}

/// Whether the calling thread is inside a pass of the registry mirror thread.
pub(crate) fn mirror_in_pass() -> bool {
    mirror::in_pass()
}

/// [`record_named_bytes`] for a 64-bit value (REG_QWORD): the value is ONE registry transaction,
/// so the two 32-bit halves of a (count, time) pair are never from different writes. Not part of
/// the mirror's changed-only cache (the callers keep their own, `stall_diag::rec_live_q`); inside a
/// mirror pass it counts towards the rest like any write. PASSIVE_LEVEL only.
pub fn record_named_qword(name: &[u8], value: u64) {
    let mut buf = [0u16; 16];
    let n = name.len().min(14);
    let mut i = 0;
    while i < n {
        buf[i] = name[i] as u16;
        i += 1;
    }
    buf[n] = 0;
    record_named_q(&buf[..=n], value);
    if mirror::in_pass() && mirror::made_write() {
        crate::ddi::mirror_thread::rest();
    }
}

/// `RTL_QUERY_REGISTRY_DIRECT` — store the value straight into EntryContext
/// (for REG_DWORD data that fits a ULONG). No callback routine.
const RTL_QUERY_REGISTRY_DIRECT: u32 = 0x20;

/// Read a REG_DWORD config value from the service key (the same key the
/// breadcrumbs live under), or `default` if absent/unreadable. The value name
/// is ASCII (≤14 chars). PASSIVE_LEVEL only. Bring-up experiment knobs: lets
/// AddAdapter-shape experiments iterate via `reg add` + `devcon restart`
/// instead of a rebuild+reboot per variant.
///
/// The value MUST be REG_DWORD (RTL_QUERY_REGISTRY_DIRECT without TYPECHECK
/// interprets string data as a UNICODE_STRING buffer — only this driver's own
/// documented knobs are read here).
/// Longest service-key value name [`read_config_dword`] can look up.
///
/// The lookup builds a UTF-16 name in a fixed 16-word buffer and NUL-terminates
/// it, so anything longer is silently TRUNCATED and the lookup then misses —
/// returning `default` forever with no diagnostic. `ScanoutForceReject` (18)
/// was created that way and read as 0 on every boot, which cost a deploy cycle.
pub const MAX_CONFIG_NAME: usize = 14;

/// A service-key value name PROVEN to survive the lookup buffer.
///
/// The constructor is `const fn` and every [`knobs`] entry is a `const` item, so
/// the length assert is evaluated during `cargo check` — not only at
/// monomorphisation, which is where the previous inline-const-in-a-generic-fn
/// form fired (a build failure but not a check failure, so `cargo check` passed
/// on a knob that could never be read).
#[derive(Clone, Copy)]
pub struct KnobName(&'static [u8]);

impl KnobName {
    /// Fails the BUILD if the name cannot survive the lookup buffer.
    pub const fn new(name: &'static [u8]) -> Self {
        assert!(
            name.len() <= MAX_CONFIG_NAME,
            "knob name exceeds the RtlQueryRegistryValues lookup buffer and would silently read as its default"
        );
        Self(name)
    }
}

/// Every service-key knob the driver reads, in one place.
///
/// A knob that is not here cannot be read, which makes this the inventory rather
/// than a list someone maintains in ROADMAP.md by grepping. Each is a `const`
/// item, so [`KnobName::new`]'s length assert runs on every build of this module.
pub mod knobs {
    use super::KnobName;

    /// Breadcrumb ring level. 0 (default) = the `S<idx>` ring is off.
    pub const DIAG_LEVEL: KnobName = KnobName::new(b"DiagLevel");
    /// `StopFlush` (default 1): flush the service key to disk at StopDevice's
    /// first and last stage, so `StopStg` survives a bugcheck inside the stop.
    /// 0 skips the two `ZwFlushKey` calls.
    pub const STOP_FLUSH: KnobName = KnobName::new(b"StopFlush");
    /// `NvSpinUs` (default 50): how long, in microseconds, a forwarded RM message
    /// polls for its reply before it blocks (`virtio::ctrl::raw_roundtrip`). 0
    /// turns the spin off; above 200 it is clamped. Adaptive on top of this: the
    /// spin backs off by itself while the host answers slower than the budget
    /// (`helios_kmd_logic::nvrm_fastpath::spin`). Read once, at the first forward;
    /// the outcome is mirrored in `NvSpinHit` / `NvSpinMis`.
    pub const NV_SPIN_US: KnobName = KnobName::new(b"NvSpinUs");
    /// `NvDupHarden` (default 2 = log-only, for the first shipped package). Cross-client
    /// hardening of forwarded RM ioctls (`virtio::nvrm_harden`, `docs/nvrm-escape.md`
    /// section 12): every RM client and backend handle a forwarded `Ioctl` names must be
    /// the calling process's own. 2 = log-only (everything is recorded and checked, what
    /// mode 1 would refuse is counted in `NvDupWould` and forwarded), 1 = enforce (the
    /// request is refused, `NvDupDeny` / `NvRef`), 0 = off (nothing is recorded or checked:
    /// the behaviour before the hardening). A value present and other than 0 or 2 enforces.
    /// Read once per boot. Flip the default to 1 after a real NVK run shows `NvCliRec` close
    /// to `NvOpen`, `NvDupWould` 0 and `NvDupDoubt` small.
    pub const NV_DUP_HARDEN: KnobName = KnobName::new(b"NvDupHarden");
    /// `NvWinPolicy` (default 1). The RM window policy (`helios_kmd_logic::rm_window`,
    /// `docs/nvrm-escape.md`, "The RM window policy"): 1 = dynamic (any device may map until
    /// the window is full, minus the reserve kept for the privileged device; the handle and
    /// mapping tables grow to their sanity bounds), 0 = the legacy rule byte for byte (a
    /// quarter of the window per device, fixed tables of 1024 entries and 128 / 256 per
    /// process). Read once per transport (StartDevice); mirrored as `NvWinPol`.
    pub const NV_WIN_POLICY: KnobName = KnobName::new(b"NvWinPolicy");
    /// `NvWinReserveMb` (default 256). MiB of the RM window only the privileged device (the
    /// one that holds the foreign scanout source, DWM-on-NVK; the KMD's own RM client) may
    /// use. Any u32 value is valid (clamped to the window). Read per transport; `NvWinResMb`.
    pub const NV_WIN_RESERVE_MB: KnobName = KnobName::new(b"NvWinReserveMb");
    /// `NvWinMaxMb` (default 0 = the whole window). Caps the bytes mapped through the RM
    /// window below the window's size: an operator bound on the non-paged pool the mapping
    /// MDLs cost (2 KiB per MiB mapped). Read per transport; `NvWinCapMb`.
    pub const NV_WIN_MAX_MB: KnobName = KnobName::new(b"NvWinMaxMb");
    /// `FlipWdogMs` (default 0 = off). The opt-in flip watchdog (`ddi::stall_diag`,
    /// `docs/zero-copy-present.md` "Stall diagnosis"): when a pending flip has gone this many
    /// milliseconds (in vsync ticks) with no address published, the vsync DPC publishes the
    /// flip's address as a KEPT picture (for any class of allocation, Venus included), and the
    /// Venus direct exits (a spent retry budget, a permanent reject) publish kept too. The kept
    /// address names a picture that is not on the screen: a diagnostic and recovery valve, not a
    /// completion. Nonzero values are clamped to 50..60000. Read at every StartDevice; mirrored
    /// as `FlWdMsEff`.
    pub const FLIP_WDOG_MS: KnobName = KnobName::new(b"FlipWdogMs");
    /// `FlipPendWdMs` (default 500, 0 = off). The generic pending-flip watchdog (v334,
    /// `kmd_logic::flip_pend_wd`, `docs/zero-copy-present.md` section 23): the newest flip
    /// dxgkrnl issued that is not done, is this old and was followed by no publication of any
    /// address for as long, is published as a KEPT picture by the vsync tick, whether or not a
    /// programming is pending (`FlipWdogMs` only counts while one is). Nonzero values are clamped
    /// to 100..60000. Read at every StartDevice; mirrored as `FpWdMsEff`; counted `FlipPendWd`.
    pub const FLIP_PEND_WD_MS: KnobName = KnobName::new(b"FlipPendWdMs");
    /// `EscWaitMs` (default 10000, 0 = no deadline; the kill and stop exits stay on). The most one
    /// escape may spend waiting in total (v334, `kmd_logic::wait_bound`, section 23): after it
    /// every wait the escape is in gives up with a clean failure status. Nonzero values are
    /// clamped to 250..600000. Read at every StartDevice; mirrored as `EscWaitMsEff`.
    pub const ESC_WAIT_MS: KnobName = KnobName::new(b"EscWaitMs");
    /// `RmGateMs` (default 6000, 0 = never). An RM fence gate point (`docs/rm-fence-marker.md`
    /// carrier (b)) whose `EventReady` has not come this many milliseconds after the fence was
    /// attached is declared fired by the HPD worker (`RmGExp`): the host's own fence timeout is
    /// 5 s, so a fire later than 6 s was lost, and the present it gates must not wait for it for
    /// ever. Nonzero values are clamped to 1000..120000. Read at every StartDevice; mirrored as
    /// `RmGateMsEff`.
    pub const RM_GATE_MS: KnobName = KnobName::new(b"RmGateMs");
    /// `DeferBudget` (default 0 = unlimited, today's behaviour). The most Deferred programming
    /// attempts of one primary (about one per vsync tick) before the worker publishes the flip's
    /// address kept and lowers the gate instead of retrying again (`FkDefBud`). 240 is about
    /// four seconds at 60 Hz. Nonzero values are clamped to 16..4000000. Read at every
    /// StartDevice; mirrored as `DefBudEff`.
    pub const DEFER_BUDGET: KnobName = KnobName::new(b"DeferBudget");
    /// `VsPowerMode` (default 1 = adapter-only quiesce, KMD 326; 0 = KMD 325, any non-D0 call of any uid): which `DxgkDdiSetPowerState` calls quiesce the vsync
    /// heartbeat. 0: any non-D0 state of any `DeviceUid` (the monitor child's included), 1: only
    /// the adapter leaving D0 (KMD 326). Read at every StartDevice; mirrored as `VsPwrEff`.
    pub const VS_POWER_MODE: KnobName = KnobName::new(b"VsPowerMode");
    /// `VsWatchdog` (default 1 = revive an armed but silent heartbeat; 0 = off, KMD 325): the heartbeat watchdog. 1 revives an armed but
    /// silent heartbeat, 2 also re-arms a quiesced one while the adapter is in D0 (KMD 326). Read
    /// at every StartDevice; mirrored as `VsWdgEff`.
    pub const VS_WATCHDOG: KnobName = KnobName::new(b"VsWatchdog");
    /// `VsIdleWake` (default 0 = off, KMD 325): 1 makes the HPD worker wake 4 times a second
    /// while the heartbeat is armed, to run the watchdog (needs `VsWatchdog` above 0). Read at
    /// every StartDevice; mirrored as `VsIdlEff`.
    pub const VS_IDLE_WAKE: KnobName = KnobName::new(b"VsIdleWake");
    /// `VsWdTimer` (default 1 = on, v329): the independent watchdog timer (a 250 ms Ex timer
    /// started at StartDevice and cancelled at StopDevice) that re-arms a heartbeat that has been
    /// silent for at least max(250 ms, 16 periods) while armed and the adapter is in D0, and asks
    /// the worker to refresh the heartbeat block every 2 s. 0 never arms it. Read at every
    /// StartDevice; mirrored as `VsWdTmEff`.
    pub const VS_WD_TIMER: KnobName = KnobName::new(b"VsWdTimer");
    /// `RestSeed` (default 1 = on): persist the newest flip address dxgkrnl issued in the
    /// service key (`RestIssLo` / `RestIssHi` / `RestUpS` / `RestChk`) and seed the restarted
    /// heartbeat from it after a `pnputil /restart-device` that RELOADED the image (every static
    /// zero). 0 = the v329 behaviour, statics only, nothing written or read. Read at every
    /// StartDevice; mirrored as `RestSeedEff` (docs/zero-copy-present.md section 20).
    pub const REST_SEED: KnobName = KnobName::new(helios_kmd_logic::restart_flip::NAME_KNOB);
    /// The persisted words, read at StartDevice (`restart_flip::Persisted`).
    pub const REST_ISS_LO: KnobName = KnobName::new(helios_kmd_logic::restart_flip::NAME_ISS_LO);
    pub const REST_ISS_HI: KnobName = KnobName::new(helios_kmd_logic::restart_flip::NAME_ISS_HI);
    pub const REST_UPTIME: KnobName = KnobName::new(helios_kmd_logic::restart_flip::NAME_UPTIME);
    pub const REST_CHECK: KnobName = KnobName::new(helios_kmd_logic::restart_flip::NAME_CHECK);
    /// Segment topology. Legal values 0 and 10 only — see `BarSegTopology`.
    pub const BAR_SEG_MODE: KnobName = KnobName::new(b"BarSegMode");
    /// CpuVisible cached-allocation kill switch (default 1 = cached).
    pub const ALLOC_CACHED: KnobName = KnobName::new(b"AllocCached");
    /// Retire ordinary (non-paging) WDDM DMA fences on host GPU COMPLETION
    /// rather than host DECODE (default 1 = GPU completion). 0 restores the
    /// historical decode-only behaviour. The ONE reader (and the contract this
    /// restores) is `crate::virtio::gpu::VirtioGpu::dma_gpu_fence`; the unread
    /// `AdapterKnobs` copy was deleted 2026-08-05.
    pub const DMA_GPU_FENCE: KnobName = KnobName::new(b"DmaGpuFence");
    /// `KmdRmClient` (default 0 = off, nothing is opened and nothing is written).
    /// The KMD's own RM client (`virtio::rm_client`, `docs/kmd-rm-client.md`): 1 =
    /// open an RM client over the forwarding path and allocate, export and import a
    /// video-memory surface of the VidPn primary's size (invisible); 2 = also map it,
    /// paint a test picture and show it once through the KMD's own `ScanoutFlip`;
    /// 3 = a ring of two such surfaces, and the composited desktop (the LINEAR
    /// primary the display worker keeps current) is copied into the one not shown and
    /// flipped, in place of Venus' `RESOURCE_FLUSH`, with Venus as the fallback
    /// (`virtio::rm_present`, `docs/kmd-rm-client.md` section 13).
    /// 4 = 3 plus each ring surface imported as a foreign resource under the KMD's own
    /// owner (the resource id a WDDM allocation adopts, `docs/kmd-rm-client.md` section
    /// 14).
    /// 5 = no ring: the VidPn primary itself is allocated from RM SYSTEM memory (Venus is
    /// the fallback for it), mapped by the host into the window dxgkrnl's CPU aperture
    /// uses, and flipped with the KMD's own `ScanoutFlip` (section 15).
    /// Read once per transport generation. Values above 5 count as 5 (before level 3
    /// existed, 3 and more counted as 2: a service key left at 3 turns the ring on).
    pub const KMD_RM_CLIENT: KnobName = KnobName::new(b"KmdRmClient");
    /// `KmdRmSysCache` (default 0). With `KmdRmClient` = 5: what the RM system-memory
    /// primary is made of (read at the service's bring-up; `docs/kmd-rm-client.md` 15.5).
    /// 0 or 1 = WRITE-COMBINED memory (the default: every view of it agrees with dxgkrnl's
    /// write-combined mapping of the primary, so there is no alias; 1 is the same, spelled
    /// out). 2 = cached memory AND the `Cached` flag on the primary (an opt-in experiment:
    /// dxgkrnl may refuse it). 3 = cached memory under dxgkrnl's write-combined view, a
    /// write-back / write-combined ALIAS of the same pages (an opt-in, counted `RmSysAlias`).
    /// Any other value is the default: an unknown value never picks an alias.
    pub const KMD_RM_SYS_CACHE: KnobName = KnobName::new(b"KmdRmSysCache");
    /// `KmdRmSysPollMs` (default 0 = off). With `KmdRmClient` = 5: a heartbeat for a primary
    /// that is written with no event the KMD can see (a GDI-only session, no DWM: GDI draws
    /// through the CPU aperture mapping and nobody tells the driver). While an RM primary is
    /// shown, it is flipped again at this period (milliseconds, 50 to 5000), also with
    /// nothing reported. Costs a flip (a dup, one message, one compositor commit) per period for
    /// as long as the desktop exists, so it is off by default: with DWM every change comes with
    /// a present, a paging write or a marker, and the short tail after the last of them
    /// (`rm_refresh::TAIL_100NS`, always on) covers what trails it. Read once per transport
    /// generation (`docs/kmd-rm-client.md` 15.16).
    pub const KMD_RM_SYS_POLL_MS: KnobName = KnobName::new(b"KmdRmSysPollMs");
    /// `ForeignFlip` (default 0 = off: the foreign-allocation flip does not exist and every
    /// allocation takes the path it took before). Nonzero: a WDDM allocation that adopted an
    /// RM resource a user-mode device imported (DWM-on-NVK's swap-chain buffers, open identity
    /// FOREIGN) is shown by the KMD's own `ScanoutFlip` of its DRM file and GEM, with the
    /// arbiter's resident source registered under the importing device, instead of
    /// `SET_SCANOUT_BLOB` plus a Venus flush (`virtio::foreign_flip`,
    /// `docs/kmd-rm-client.md` 15.18). Refused, with a counted reason (`FfRef<NN>`), and the
    /// old path runs, when the importer's file is gone, the layout is unusable, the host lacks
    /// the import, or `KmdRmClient` is 3 or 4. Read once per transport generation.
    pub const FOREIGN_FLIP: KnobName = KnobName::new(b"ForeignFlip");
    /// `FfAsyncWin` (default 0 = off: every `ForeignFlip` host flip is a synchronous round trip
    /// on the HPD worker, as it always was). 1 to 4 (larger is 4), only with `ForeignFlip` on:
    /// the host `ScanoutFlip` is SUBMITTED on the control queue without waiting for its reply,
    /// with at most this many in flight; the worker settles the answers when the used-ring drain
    /// wakes it, so programming never waits for the host. Also lets the `SetVidPnSourceAddress`
    /// DDI wake the worker through a DPC at once instead of at the next vsync tick, and the
    /// tick wake it with the vsync delivery gate closed. Read once per transport generation
    /// (`docs/kmd-rm-client.md` 15.18.13).
    pub const FOREIGN_FLIP_WIN: KnobName = KnobName::new(b"FfAsyncWin");
    /// `FfRepeatMs` (default 100, 0 = off, at most 10000), only with `ForeignFlip` on: the least
    /// time between two host flips that only REPEAT the picture the previous flip showed (a
    /// desktop refresh edge, as opposed to a programming dxgkrnl issued). 0 flips on every edge,
    /// as KMD 325 did (155 flips a second of an unchanged picture in the T5 run). Read once per
    /// transport generation (`docs/kmd-rm-client.md` 15.18.14).
    pub const FOREIGN_FLIP_REPEAT: KnobName = KnobName::new(b"FfRepeatMs");
    /// `BindFlushMode` (default 0). Selects when the bind edge tells the host
    /// to READ the freshly bound primary (ROADMAP defect 0ab-B):
    ///   0 = completion-ordered against the boundary this buffer's own present
    ///       marker captured (`AdapterContext::arm_bind_refresh`),
    ///   1 = IMMEDIATE — flush at the bind with no ordering at all.
    ///
    /// 1 is the discriminating A/B, not a shipping mode: it answers "is the
    /// buffer's content already correct when we bind it?" directly, which two
    /// falsified boundary variants could only answer by inference.
    pub const BIND_FLUSH_MODE: KnobName = KnobName::new(b"BindFlushMode");
    /// `DispatchBind` (default 1 = ON). Enqueue the flip's `SET_SCANOUT_BLOB`
    /// from the DISPATCH-level flip arm as well as from the PASSIVE display
    /// worker (ROADMAP defect 0ab-C, D1(ii)) — a pure accelerator: the worker
    /// path is unchanged and still consumes the pending slot, and the earlier
    /// enqueue wins on a FIFO control queue. 0 is the same-boot A/B disable,
    /// which restores the worker-only bind cadence exactly.
    pub const DISPATCH_BIND: KnobName = KnobName::new(b"DispatchBind");
    /// Per-present probe instrumentation (default 0).
    pub const PRESENT_PROBE: KnobName = KnobName::new(b"PresentProbe");
    /// `ForeignCopy` (default 0 = OFF; set 1 to use it). The KMD's explicit-modifier copy of
    /// a foreign (NVK-on-RM) resource into the scan-out image, and the device
    /// extension tier it needs. 0, the default, is the pre-feature device and import;
    /// read at AddAdapter/StartDevice like every knob.
    pub const FOREIGN_COPY: KnobName = KnobName::new(b"ForeignCopy");
    /// `BltAsync` (default 0 = the previous behaviour). 1: a Blt Present of an adopted foreign
    /// (NVK-on-RM) source into a KMD standard buffer returns from `DxgkDdiPresent` without a CPU
    /// wait: the copy is submitted by the DDI, or queued for the HPD worker until the producer's
    /// boundary has been reached, and the Present's DMA fence retires with the copy. Read at every
    /// StartDevice; mirrored as `BltAsyncKnob`. `docs/zero-copy-present.md`, "Asynchronous
    /// composed present (BltAsync, BltNoMirror)".
    pub const BLT_ASYNC: KnobName = KnobName::new(b"BltAsync");
    /// `BltNoMirror` (default 0 = the previous behaviour). 1: such a Blt does not CPU-copy the
    /// frame into the destination's system-memory backing; the backing is marked "system copy
    /// invalid" instead. Independent of `BltAsync`. Read at every StartDevice; mirrored as
    /// `BltNoMirKnob`.
    pub const BLT_NO_MIRROR: KnobName = KnobName::new(b"BltNoMirror");
    /// `BltAsyncVenus` (default 0 = the knobs act on foreign sources only). 1: `BltAsync` and
    /// `BltNoMirror` also act on a Venus-native source (an image the UMD created through Venus)
    /// blitted into a standard buffer. Has no effect with both of those knobs at 0. Read at every
    /// StartDevice; mirrored as `BltVenusKnob`. `docs/zero-copy-present.md` section 24.11.
    pub const BLT_ASYNC_VENUS: KnobName = KnobName::new(b"BltAsyncVenus");
    /// `BltLookahead` (default 1 = the front of the ready queue only, the behaviour before
    /// v337). How many entries of the WindowedBlt ready queue the HPD worker looks at when it
    /// picks the next copy to submit: a request whose producer has not finished, or whose
    /// destination is still being read, no longer holds the requests of unrelated destinations
    /// behind it (per-destination order is kept). Clamped to 1..8. Read at every StartDevice;
    /// mirrored as `BltLookKnob`. `docs/zero-copy-present.md` section 24.10.
    pub const BLT_LOOKAHEAD: KnobName = KnobName::new(b"BltLookahead");
    /// `GuestBlob` (default 0 = the previous behaviour). 1: while VidMm holds a KMD standard
    /// Present buffer in system memory, the Blt copy writes those pages through a virtio-gpu
    /// GUEST blob imported into the KMD's Venus device, and the CPU mirror is skipped for it.
    /// Needs the host's `NVGPU_CFG_GUEST_BLOB` (with `NVGPU_CFG_VENUS`); without it the knob
    /// does nothing. Read at every StartDevice; mirrored as `GbKnob`.
    /// `docs/zero-copy-present.md` section 24.12.
    pub const GUEST_BLOB: KnobName = KnobName::new(b"GuestBlob");
    /// `CopyQueue` (default 0 = the previous behaviour: one queue, family 0). 1: the KMD's Venus
    /// device also gets a queue on a transfer-only family (chosen from the queue family
    /// properties, bound to ring 2), and the windowed Present copies a transfer queue can run (a
    /// plain image-to-buffer copy into a standard buffer or its guest blob, foreign sources
    /// included) go there instead of waiting for graphics-engine timeslices. 2: as 1, and the
    /// family-0 queue (format conversions, image destinations) at high global priority. Read at
    /// every StartDevice (device creation); mirrored as `CqKnob`. `docs/zero-copy-present.md` 24.13.
    pub const COPY_QUEUE: KnobName = KnobName::new(b"CopyQueue");
    /// `RmCopyEngine` (default 0 = nothing happens: no allocation, no RM message). 1: reserved for
    /// the windowed Present copy on the KMD's own copy-engine channel (M3c; nothing yet). 2: the
    /// channel's hardware self-test, once per transport generation, from the HPD worker. 3: the
    /// shadow mode (M3c-1): a sample of real Presents copied again by the channel into a scratch
    /// buffer and compared with the production copy. Any other value is 0. Read at every
    /// StartDevice; mirrored as `CeKnob`.
    /// `docs/rm-copy-engine-present.md` section 11.
    pub const RM_COPY_ENGINE: KnobName = KnobName::new(b"RmCopyEngine");
    /// `RmCeCache` (default 0 = cached, as the copy-engine tool allocates it). 1: the channel's own
    /// RM system memory (control, ring, the self-test's buffers) write-combined, for an A/B. Read at
    /// StartDevice when `RmCopyEngine` is nonzero; mirrored as `CeCache`.
    pub const RM_CE_CACHE: KnobName = KnobName::new(b"RmCeCache");
    /// `CeShadowEvery` (default 0 = 64): with `RmCopyEngine` = 3 (the shadow mode, M3c-1), one in
    /// how many Presents is copied again by the copy-engine channel and compared with the
    /// production copy. Read at StartDevice in shadow mode only; mirrored as `CeShadowEach`.
    /// `docs/rm-copy-engine-present.md` section 14.
    pub const CE_SHADOW_EVERY: KnobName = KnobName::new(b"CeShadowEvery");
    /// `CeRtDirect` (default 0): with `RmCopyEngine` = 1, submit a routed copy at its Present (the
    /// GPU acquire on the record's value waits for the producer) instead of when the HPD worker
    /// sees the producer's boundary ready. Read at StartDevice with the route on; mirrored as
    /// `CeRtDirKnob`. `docs/rm-copy-engine-present.md` section 15.13.
    pub const CE_RT_DIRECT: KnobName = KnobName::new(b"CeRtDirect");
    /// Render+display adapter shape (default 1 = the render+display miniport,
    /// which is the product). 0 restores the boot-era render-only surface.
    pub const DISPLAY_HALF: KnobName = KnobName::new(b"DisplayHalf");
    /// Restore the legacy `SupportDirectFlip` advertisement (default 0 = deny).
    pub const DIRECT_FLIP_CAPS: KnobName = KnobName::new(b"DirectFlipCaps");
    /// Advertise `DXGK_VIDMMCAPS.CrossAdapterResource` (default 0).
    /// Exactly [`super::MAX_CONFIG_NAME`] bytes — the assert's live subject.
    pub const CROSS_ADAPT_CAPS: KnobName = KnobName::new(b"CrossAdaptCaps");
    /// BAR descriptor flag word (default 0x1C).
    pub const BAR_SEG_FLAGS: KnobName = KnobName::new(b"BarSegFlags");
    /// BAR descriptor `BaseAddress` in MiB (default 0).
    pub const BAR_SEG_BASE_MB: KnobName = KnobName::new(b"BarSegBaseMB");
    /// Reported device-memory capacity in MiB. 0 (default) preserves the proven
    /// one-GiB capacity of the existing aperture+BAR topology.
    pub const VIDMM_VRAM_MB: KnobName = KnobName::new(b"VidMmVramMB");
    /// `DXGK_FLIPCAPS` extra bits (default 0 = the driver's own word, `FlipOnVSyncMmIo`).
    /// A raw `DXGK_FLIPCAPS` bit mask OR'd into it: only bit 4 `FlipIndependent` (0x10), bit 5
    /// `DdiPresentForIFlip` (0x20) and bit 6 `FlipImmediateOnHSync` (0x40) are accepted (WDK
    /// 10.0.26100.0 `d3dkmddi.h`); every other bit is dropped and reported in `FlipCapsXMsk`.
    /// `FlipCapsX=0x10` therefore reports 0x12. It used to REPLACE the whole word; the replaced
    /// bits were never survivable except as a no-op (`FlipCapsX=2` equals the default).
    /// Read once per AddAdapter/StartDevice with the other knobs (`AdapterKnobs`), so a change
    /// applies at the next StartDevice (reboot preferred); mirrored as `FlipCapsXEff` and
    /// `FlipCapsRep` at every start.
    pub const FLIP_CAPS_EXTRA: KnobName = KnobName::new(b"FlipCapsX");
    /// `IndepFlip` (default 0): independent flip, stage S-1 (`docs/independent-flip.md` section
    /// 11, `helios_kmd_logic::independent_flip::Mode`). 0 off; 1 advertise `SupportDirectFlip`,
    /// the aperture `DirectFlip` flag and `FlipIndependent | DdiPresentForIFlip` (OR'd into what
    /// `DirectFlipCaps` / `FlipCapsX` ask for) and count every flip's verdict (`Idf*`); 2 as 1,
    /// and a DMA-buffer flip of an unregistered Venus allocation completes as a kept picture
    /// instead of failing (`PBFlip` 0xE6). Read with the other adapter knobs; mirrored as `IdfKnob`.
    pub const INDEP_FLIP: KnobName = KnobName::new(b"IndepFlip");
    /// `DXGK_VIDMMCAPS` extra bits (default 0 = the driver's own word). A raw mask OR'd into
    /// `MemoryManagementCaps`; only bit 9 `NonCpuVisiblePrimary` (0x200) is accepted
    /// (`helios_kmd_logic::vidmm_caps`), the rest is dropped and reported in `VmCapsXMsk`. The
    /// GPU-memory redirection experiment, stage V1 (`docs/vram-redirection.md` 5.2). Read with
    /// the other `AdapterKnobs` at AddAdapter and StartDevice; mirrored as `VmCapsXEff` and
    /// `VmCapsRep` at every start.
    pub const VIDMM_CAPS_EXTRA: KnobName = KnobName::new(b"VidMmCapsX");
    /// `HwCursor` (default 1): the hardware cursor (`docs/independent-flip.md` section 12,
    /// `helios_kmd_logic::hw_cursor`). 1 reports a 256x256 monochrome / color / masked-color
    /// pointer in `DXGK_DRIVERCAPS` when the host serves it (`NVGPU_CFG_VENUS_CURSOR`), and
    /// `SetPointerShape` / `SetPointerPosition` drive the host pointer's image; dxgkrnl then
    /// draws no software cursor, which independent flip needs (DWM, which would draw it, is out
    /// of the path). 0 is the software cursor as before. 2 advertises whatever the host says
    /// (every shape then fails over to the software cursor on a host without it). Read with the
    /// other adapter knobs (a change applies at the next StartDevice); mirrored as `CurKnob`.
    pub const HW_CURSOR: KnobName = KnobName::new(b"HwCursor");
    /// `DXGK_DRIVERCAPS.MaxQueuedFlipOnVSync` — how many flips dxgkrnl may keep
    /// queued and pending on this adapter at once. Default 1 is the historical
    /// advertisement; a Helios flip retires only when its DMA fence completes,
    /// which by design waits on the venus work outstanding at submit, so a
    /// depth of 1 makes present N+1 wait for frame N's host completion. Read at
    /// AddAdapter, so `pnputil /restart-device` applies it without a rebuild.
    /// 0 is coerced to 1 (a zero-depth flip queue is not representable) and the
    /// value actually advertised is mirrored in the `FlipQueV` counter.
    pub const FLIP_QUEUE_DEPTH: KnobName = KnobName::new(b"FlipQueueN");
    /// `FlipAnnounce` (default 2 since the 332.1 hardware rows; 0 = off, the old behaviour): publish a flip's address toward
    /// dxgkrnl AT `SetVidPnSourceAddress` (atomics only, DIRQL) so the very next CRTC_VSYNC tick
    /// retires it (one tick per flip instead of two), while the HPD worker does the real
    /// programming afterwards. 1 = only flips of foreign allocations `ForeignFlip` already
    /// accepted; 2 = every flip, Venus direct and copy paths included. Only when the worker is
    /// idle at the DDI (at most one unprogrammed announced flip); a flip that finds it busy
    /// retires the normal way. Any non-zero value also wakes the worker early (`FlipEarlyWake`).
    /// Read at every StartDevice (`pnputil /restart-device` applies it); mirrored as `FaKnob`.
    /// `docs/kmd-rm-client.md` 15.18.15.
    pub const FLIP_ANNOUNCE: KnobName = KnobName::new(b"FlipAnnounce");
    /// `FlipAnnForeign` (default 1 with `ForeignFlip` on, else 0; an explicit value wins, 0
    /// included): with `FlipAnnounce` 2, also announce flips of foreign or hollow allocations (the
    /// NVK DWM's swap chain). 0 announces the Venus class only, which is the tear-exposure-free
    /// setting (the foreign flip retires when the worker publishes it). `FlipAnnounce` 1 (the
    /// explicit foreign mode) ignores it. Read at every StartDevice; mirrored in `FaKnob` (bit 16).
    /// The name is 14 characters, the lookup buffer's limit. `docs/kmd-rm-client.md` 15.18.16.
    pub const FLIP_ANN_FOREIGN: KnobName = KnobName::new(b"FlipAnnForeign");
    /// `MirrorThread` (default 1, 0 = off): run the registry mirror (`stall_diag::publish_counters`)
    /// on its own thread, one pass a second (`ddi/mirror_thread.rs`). 0 is the kill switch: every
    /// caller publishes inline on the HPD worker, as before v332. Read at every StartDevice and
    /// mirrored as `MirThrEff`; a thread that could not be joined (`MirLeak` 1) turns it off for
    /// the rest of the driver image's life.
    pub const MIRROR_THREAD: KnobName = KnobName::new(b"MirrorThread");
    /// `MirPrio` (default 6, 0 = leave the thread's priority alone): the kernel priority the mirror
    /// thread runs at, below the HPD worker's (8, the default of a system thread), so a registry
    /// pass never delays the flip path. 1 to 15 are taken as given; anything else is the default.
    /// Read at every StartDevice, mirrored as `MirPrioEff`. `docs/kmd-rm-client.md` 15.18.16.
    pub const MIRROR_PRIO: KnobName = KnobName::new(b"MirPrio");
    /// `MirYield` (default 32, 0 = never): the mirror thread rests (a one-millisecond relative wait
    /// that ends at once on a stop; the system timer may round it up) after this many registry
    /// writes of one pass, so a hundred-write pass does not hold one processor for tens of
    /// milliseconds. Mirrored as `MirYldEff`. `docs/kmd-rm-client.md` 15.18.16.
    pub const MIRROR_YIELD: KnobName = KnobName::new(b"MirYield");
    /// `MirChanged` (default 1, 0 = off): inside a mirror pass skip a write whose value is what the
    /// registry already holds (every value is written again at least every 30 s, so a hand-edited
    /// or deleted one comes back). 0 writes every value of every pass, as before. Mirrored as
    /// `MirChgEff`. `docs/kmd-rm-client.md` 15.18.16.
    pub const MIRROR_CHANGED: KnobName = KnobName::new(b"MirChanged");
    /// `VsCatchUp` (default 0): 1 serves ONE missed heartbeat slot with an immediate extra tick
    /// when a tick callback ran between one and 1.5 periods after its own deadline (the missed
    /// slot is otherwise dropped, no burst). Read at every StartDevice, mirrored as `VsCatchEff`.
    /// `docs/kmd-rm-client.md` 15.18.16.
    pub const VS_CATCH_UP: KnobName = KnobName::new(b"VsCatchUp");
    /// `FlipBusyFly` (default 0, at most 4): how many pipelined `ForeignFlip` host flips may be in
    /// flight while the worker still counts as idle for a `FlipAnnounce` (0 = none: strict: the
    /// previous buffer is certainly no longer read when the next flip is announced; 1 lets the
    /// announce run with one host flip in flight, trading a tear exposure of up to one more host
    /// round trip for throughput). Only with `ForeignFlip` and `FfAsyncWin`; read once per
    /// transport generation. `docs/kmd-rm-client.md` 15.18.15.
    pub const FLIP_BUSY_FLY: KnobName = KnobName::new(b"FlipBusyFly");
    /// `FlipEarlyWake` (default 0): the DDI asks for the device DPC that wakes the HPD worker the
    /// moment a flip is pending, instead of the worker waiting for the next vsync tick; without
    /// an announce the retire still waits for the worker's publication (one tick earlier on
    /// average). Read at every StartDevice.
    pub const FLIP_EARLY_WAKE: KnobName = KnobName::new(b"FlipEarlyWake");
    /// `FlipLat` (default 1 = on, 0 = off): the flip retire latency / inter-flip interval /
    /// vblank utilisation measurement (`FlipLat*`, `IfGap*`, `FlipP99Us`, `VbUsed`,
    /// `VsLate*`): atomics in the DDI and the tick, mirrored once a second. Read at every
    /// StartDevice.
    pub const FLIP_LAT: KnobName = KnobName::new(b"FlipLat");
    /// `StageTrace` (default 0 = off): per-frame stage timestamps of the windowed Present copy
    /// and the `ForeignFlip` path into a ring the registry mirror publishes as the REG_BINARY
    /// `StgRing` (`ddi::stage_trace`, `docs/TRACING.md` "Frame stage timing"). Off, every stamp
    /// site is one relaxed load. Read at every StartDevice; mirrored as `StgOn`.
    pub const STAGE_TRACE: KnobName = KnobName::new(b"StageTrace");
    /// `OutputTech` (default 1): the connector type the virtual monitor's child
    /// device reports to Windows. 1 = DisplayPort (external), 2 = HDMI, 3 = DVI,
    /// 4 = internal, 0 = HD15 (analog VGA, the historical value). Anything else
    /// is DisplayPort. A real GPU reports a digital connection; as HD15 Windows
    /// applies analog-monitor frequency rules, which is why modes above 60 Hz
    /// never came up. Read in `QueryChildRelations`, so `pnputil /restart-device`
    /// applies a change without a rebuild; the value in force is mirrored in the
    /// `OutTech` counter.
    pub const OUTPUT_TECH: KnobName = KnobName::new(b"OutputTech");
    /// `VsyncRateMhz` (default 0 = follow the mode): force the retrace rate of the
    /// vsync heartbeat, in millihertz (60000 = 60 Hz), independent of the refresh
    /// rate the mode advertises. A diagnostic for "modes above 60 Hz never flip":
    /// 60000 with a 120 Hz mode tells whether the timer cadence is the cause.
    /// Read at StartDevice and mirrored in the `VsRate` counter.
    pub const VSYNC_RATE_MHZ: KnobName = KnobName::new(b"VsyncRateMhz");
    /// `PresentWmk` (default 1 = ON since 22.22.244.0). Gate a WDDM submission
    /// that carries a LIVE present stream boundary on that exact boundary
    /// alone, rather than additionally on every transport entry enqueued before
    /// it. The superset delays the DMA fence by the whole guest→host pipeline
    /// depth, which is what makes dxgkrnl block the presenting thread at its
    /// 3-deep present queue (ETW `BlockThread` Reason=2). Measured 2026-08-04
    /// on GT1: `DxgkDdiSubmitCommand`→DMA_COMPLETED mean 5.825→4.854 ms
    /// (p50 6.110→3.995), flip packet lifetime 8.594→7.398 ms,
    /// `umd_present_callback` 625-661→359 us, GT1 +3.7…+4.3% paired.
    /// `0` is the same-boot A/B disable and restores the historical superset
    /// exactly. Snapshotted at transport init, so `pnputil /restart-device`
    /// flips it without a reboot.
    pub const PRESENT_EXACT_WATERMARK: KnobName = KnobName::new(b"PresentWmk");

    /// `MsiVectors` (default 0 = per-source vectors when the OS granted enough
    /// messages). 1 forces ONE shared message 0 for every queue even when more
    /// were granted: the same-boot A/B between per-queue and shared vectors.
    ///
    /// This is NOT a switch back to INTx (its default 0 keeps meaning per-source
    /// vectors, as it always did). Whether the OS hands the driver messages or the
    /// INTx line is decided by PnP before `StartDevice` from the device key's
    /// `MSISupported`; the switch for that is `MsiMode` below (see
    /// `docs/msi-interrupts.md`). Snapshotted at transport init, so
    /// `pnputil /restart-device` applies it without a reboot.
    pub const MSI_VECTORS: KnobName = KnobName::new(b"MsiVectors");

    /// `MsiMode` (default 0 = auto). Which interrupt mode the driver asks PnP for
    /// (`docs/msi-interrupts.md`): 0 = follow the INF / the device key as it stands (INTx in
    /// this package; a latch lowers it); 1 = INTx always; 2 = MSI-X (raises the key to 1), but
    /// the boot-loop breaker and the latch still win; 3 = MSI-X with no breaker and no latch
    /// (debugging). Realised by `AddDevice` writing the device key's `MSISupported`, so the
    /// FIRST restart after a change writes the key and a SECOND restart (or a reboot) applies
    /// it; the driver follows whatever PnP actually granted either way. Mirrored as
    /// `MsiModeEff`. Never written by the driver.
    pub const MSI_MODE: KnobName = KnobName::new(b"MsiMode");
    /// `MsiLatch` (default 0). Set to 1 by the driver when message delivery was convicted,
    /// vector set-up failed, or the breaker tripped: the next `AddDevice` then asks PnP for
    /// INTx. An operator clears it (0) to retry MSI-X, or sets it to 1 to rehearse the fallback.
    pub const MSI_LATCH: KnobName = KnobName::new(b"MsiLatch");
    /// `MsiStarting` (default 0). The boot-loop breaker's marker: a start that got messages
    /// sets it (flushed to disk) and clears it once interrupts arrive; `AddDevice` finding it
    /// set means the previous start never became healthy.
    pub const MSI_STARTING: KnobName = KnobName::new(b"MsiStarting");
    /// `MsiBreaker` (default 0). How many times the breaker tripped (a count the driver keeps).
    pub const MSI_BREAKER: KnobName = KnobName::new(b"MsiBreaker");
    /// `MsiStartingVer` (default 0). The build tag (`msi::build_tag`) of the image that set
    /// `MsiStarting`; 0 = an image older than the tag. Only a marker of the running build trips
    /// the breaker: a driver update consumes the old build's marker (`MsiMarkerOld`).
    pub const MSI_STARTING_VER: KnobName = KnobName::new(b"MsiStartingVer");
    /// `MsiLatchVer` (default 0). The build tag of the image that wrote `MsiLatch`; 0 = written
    /// by hand (or by an image older than the tag), honoured. Another build's latch is stale:
    /// cleared at `AddDevice` (`MsiLatchOld`).
    pub const MSI_LATCH_VER: KnobName = KnobName::new(b"MsiLatchVer");
    /// `MsiLatchWhy` (default 0). Why the KMD latched (`msi::latch_why`, 1 to 4); 0 / absent on
    /// an operator's latch. Read at `AddDevice`: an untagged latch WITH a KMD reason was written
    /// by an image older than the build tag and is set aside (`MsiLatchLegacy=1`).
    pub const MSI_LATCH_WHY: KnobName = KnobName::new(b"MsiLatchWhy");
    /// `MsiMarkerOld` (default 0). How many markers of another build `AddDevice` consumed
    /// without tripping the breaker (a count the driver keeps).
    pub const MSI_MARKER_OLD: KnobName = KnobName::new(b"MsiMarkerOld");
    /// `MsiLatchOld` (default 0). How many latches of another build `AddDevice` set aside (a
    /// count the driver keeps).
    pub const MSI_LATCH_OLD: KnobName = KnobName::new(b"MsiLatchOld");

    /// Default-enabled capacity notification for retry of a full Venus transport
    /// queue. 0 preserves historical 1 ms polling; no capacity change.
    pub const SUBMIT_SPACE_WAKE: KnobName = KnobName::new(b"SubSpaceWake");
    /// `SubmitPool` (default 1 = ON). The KMD display submitters (scan-out copy, Present Blt,
    /// `BltAsync` direct, deferred windowed Blt) take their two staged DMA buffers (SUBMIT_3D
    /// meta, Venus stream) from the transport's bounded DMA pool, in the same lock hold as the
    /// reap, instead of two `MmAllocateContiguousMemory` calls per submit. A miss falls back to
    /// a fresh allocation. 0 restores allocate-per-submit exactly: the same-boot A/B. Read at
    /// every StartDevice, mirrored as `SubPoolOn`. `docs/zero-copy-present.md` 24.14.
    pub const SUBMIT_POOL: KnobName = KnobName::new(b"SubmitPool");
    /// `SubStageClk` (default 1 = ON). The stage clock of the display submitters and of the
    /// pipelined foreign flip (`Sub*`, `SubW*`, `SubF*`): 0 reads no interrupt time on any submit
    /// path (four of the reads sit inside the `virtio_lock` hold) and leaves only the counts, the
    /// A/B that tells the clock's own cost from what it measures. Read at every StartDevice,
    /// mirrored as `SubClkOn`. `docs/zero-copy-present.md` 24.14.
    pub const SUBMIT_STAGE_CLOCK: KnobName = KnobName::new(b"SubStageClk");
    /// `SubKickUnlock` (default 1 = ON). The display submitters and the pipelined foreign flip
    /// ring the control queue's doorbell AFTER releasing `virtio_lock` (one MMIO write to the
    /// notify register located at transport init) instead of inside it (`PciTransport::notify`:
    /// three MMIO accesses, each a VM exit, under the lock every other submitter spins on). 0 =
    /// the previous behaviour exactly. Read at every transport init (StartDevice), mirrored as
    /// `SubKickUnl` (1 only when the doorbell was located). The requested name
    /// `SubKickUnlocked` is 15 bytes and would not fit the lookup buffer.
    /// `docs/zero-copy-present.md` 24.14.10.
    pub const SUBMIT_KICK_UNLOCK: KnobName = KnobName::new(b"SubKickUnlock");
    /// `WddmHoldMs` (default 0 = OFF, and OFF is the only shipping value).
    ///
    /// # THE KNOB IS THE EXPERIMENT (UV1, `docs/dx12/KMD_IMPACT.md` §14a.1)
    ///
    /// Hold a D3D12 ECL packet's `DMA_COMPLETED` back by N ms after everything it
    /// really depends on is satisfied, then read the D3D12 probe's own
    /// `WaitForSingleObject signalled in N us` against the measured 0.8–1.1 µs
    /// baseline:
    ///
    /// * **N grows by ~the hold** ⇒ **UV1 ✓**. dxgkrnl DOES release the runtime's
    ///   queued monitored-fence signal behind our DMA packets, so the fence bridge
    ///   is the right lever and everything after it is plumbing.
    /// * **N stays at the baseline** ⇒ **UV1 ✗**. The runtime's fence advance has
    ///   no causal dependency on this context's packets at all, and none of
    ///   K-F3..K-F9 is the answer. Say so loudly and stop.
    ///
    /// ⛔⛔ **PRECONDITION, AND WITHOUT IT THE ✗ ROW IS A TRUSTING-A-ZERO.**
    /// `WfBHold` must have **MOVED** on the run. Three different states produce a
    /// flat N and only the third is UV1 ✗:
    ///   1. **The knob never took.** `WDDM_HOLD_MS` is snapshotted at
    ///      `VirtioGpu::init`, so setting the registry value and then doing
    ///      `pnputil /restart-device` — the project's standard deploy — leaves the
    ///      hold at 0. It needs a **reboot**, exactly like `DiagLevel`.
    ///   2. **The hold never armed.** It only arms when a D3D12 packet reaches the
    ///      FIFO head with its three real dependency arms already satisfied. If
    ///      `Umd12EclSubmit` is off, or no D3D12 packet reached the head at all,
    ///      nothing was ever held.
    ///   3. Genuinely no causal dependency — the only reading that licenses ✗.
    /// ⇒ **read `WfBHold` first. If it is 0, the experiment did not run, and the
    /// flat N says nothing whatever about UV1.**
    ///
    /// ⭐ THIS IS THE ONLY CLEAN UV1 TEST AVAILABLE. The bare submission's reading
    /// is confounded: the venus ring emits NO wire fence at all while it is busier
    /// than 1 ms (`icd/mesa/src/virtio/vulkan/vn_ring.c:673-690` — the doorbell is
    /// rate-limited and only sent when the host ring advertises IDLE), so an
    /// unheld packet can retire immediately for a reason that has nothing to do
    /// with whether dxgkrnl would have ordered anything. A hold is the one
    /// dependency this driver can create unilaterally and time exactly.
    ///
    /// ⛔ SCOPED, and it must stay scoped: only a submission whose private data
    /// carries the D3D12 ECL record is eligible (`present_packet.rs`'s
    /// `mark_d3d12`). `wddm_pending` is adapter-global and strictly head-of-line,
    /// so an unscoped hold stalls DWM.
    /// ⛔ Clamped in code to `WDDM_HOLD_MS_MAX`, because an unbounded hold on that
    /// FIFO is a TDR and an operator typo must not be able to cause one.
    /// ⚠ The release edge is the 60 Hz display heartbeat (`adapter/kobj.rs`), so
    /// the experiment requires the display half armed — which is the configuration
    /// it runs in anyway. `WfBHold` counts the blocked looks.
    pub const WDDM_HOLD_MS: KnobName = KnobName::new(b"WddmHoldMs");
    /// `WddmHeadMs` (default 250 = ON; `0` is the A/B disable).
    ///
    /// CONSUMER-SIDE LIVENESS FOR THE WDDM FIFO HEAD (`KMD_IMPACT.md` §14a.2
    /// K-F2, and `docs/dx12/PENDING.md` §1 A5). How long the head may stay blocked
    /// on a TAGGED-namespace dependency — a present-stream boundary or a
    /// WindowedBlt terminal, both of which can be unsatisfiable by construction —
    /// before that dependency is rebased onto the conservative wire watermark and
    /// counted (`WfBReb`).
    ///
    /// ⚠ A rebase RELEASES a fence whose named producer has not completed, so this
    /// is a bounded last resort, not a policy. The alternative it is traded against
    /// is an adapter-wide TDR (or the 256-entry FIFO overflow, which drops 256
    /// fences at once) — see the read site and `WDDM_HEAD_MS_DEFAULT`.
    /// ⛔ Clamped in code to `[WDDM_HEAD_MS_MIN, WDDM_HEAD_MS_MAX]` when nonzero:
    /// too large reinstates the TDR, too small re-opens the 0ab-B stale-frame class.
    pub const WDDM_HEAD_MS: KnobName = KnobName::new(b"WddmHeadMs");
    /// `FlGSyncMs` (default 0 = OFF; DIAGNOSTIC, `docs/flush-gate.md` section 9).
    ///
    /// A `HEFL` flush-gate Render waits up to N ms (PASSIVE, no lock held, clamped in
    /// code to `flush_trace::SYNC_MS_MAX`) until the boundary its packet carries has
    /// retired, then returns, so the runtime's key release follows the GPU completion on
    /// the CPU. If the keyed-mutex ordering failure goes away with it, the failure is a
    /// CPU-vs-GPU race (the release outruns the work); if it does not, the gate is not
    /// what orders the acquirer. Snapshotted at transport init: `pnputil /restart-device`
    /// applies it. Read `FlGSyncWt` (waits that ran) first: 0 means the experiment did
    /// not run.
    pub const FLG_SYNC_MS: KnobName = KnobName::new(b"FlGSyncMs");
}

/// Read a service-key REG_DWORD knob, or `default` if absent.
pub fn read_config_dword(name: KnobName, default: u32) -> u32 {
    let name = name.0;
    let mut name_buf = [0u16; 16];
    let n = name.len().min(MAX_CONFIG_NAME);
    let mut i = 0;
    while i < n {
        name_buf[i] = name[i] as u16;
        i += 1;
    }
    name_buf[n] = 0;

    let mut value: u32 = default;
    // SAFETY: zeroed RTL_QUERY_REGISTRY_TABLE entries are valid; the second,
    // all-zero entry terminates the table (Name == NULL, QueryRoutine == NULL).
    let mut table: [wdk_sys::RTL_QUERY_REGISTRY_TABLE; 2] = unsafe { core::mem::zeroed() };
    table[0].Flags = RTL_QUERY_REGISTRY_DIRECT;
    table[0].Name = name_buf.as_ptr() as *mut u16;
    table[0].EntryContext = (&mut value as *mut u32).cast();
    // DefaultType/DefaultData stay zero (REG_NONE): an absent value leaves
    // `value` at `default`.
    // SAFETY: PASSIVE_LEVEL; Path is the NUL-terminated service subkey relative
    // to RTL_REGISTRY_SERVICES; the table is NUL-entry-terminated; EntryContext
    // points at a live ULONG for the duration of the call.
    unsafe {
        let _ = wdk_sys::ntddk::RtlQueryRegistryValues(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            table.as_mut_ptr(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
    }
    value
}

/// Append one DWORD breadcrumb. Cheap and lossy by design (best-effort tracing).
/// No-op at `DiagLevel` 0 (the default) — see [`level`].
pub fn record(mut code: u32) {
    if level() == 0 {
        return;
    }
    let idx = STEP.fetch_add(1, Ordering::Relaxed);
    if idx >= MAX_STEPS {
        return;
    }
    // Build the value name "S<idx>\0" as UTF-16. `idx` is a u32 (up to 10 digits);
    // MAX_STEPS lets it exceed 999, so size both buffers for the full u32 range —
    // `digits[d]` previously overflowed `[0u8; 3]` once idx reached 1000, panicking
    // (→ the no_std loop{} handler hangs the thread under dxgkrnl's adapter lock and
    // deadlocks the whole graphics stack). 'S' + up to 10 digits + NUL = 12.
    let mut name = [0u16; 12];
    name[0] = b'S' as u16;
    let mut digits = [0u8; 10];
    let mut n = idx;
    let mut d = 0usize;
    if n == 0 {
        digits[0] = b'0';
        d = 1;
    } else {
        while n > 0 {
            digits[d] = b'0' + (n % 10) as u8;
            n /= 10;
            d += 1;
        }
    }
    let mut i = 0;
    while i < d {
        name[1 + i] = digits[d - 1 - i] as u16;
        i += 1;
    }
    name[1 + d] = 0;

    // SAFETY: PASSIVE_LEVEL (see module note). Path/ValueName are NUL-terminated
    // UTF-16; ValueData points to a 4-byte DWORD. RtlWriteRegistryValue copies the
    // value, so `code`'s lifetime ending after the call is fine.
    unsafe {
        let _ = RtlWriteRegistryValue(
            RTL_REGISTRY_SERVICES,
            SERVICE_NAME.as_ptr(),
            name.as_ptr(),
            REG_DWORD,
            (&mut code as *mut u32).cast::<core::ffi::c_void>(),
            4,
        );
    }
}

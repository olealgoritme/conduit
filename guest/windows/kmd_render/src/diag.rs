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

use core::sync::atomic::{AtomicU32, Ordering};

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

const fn widen<const N: usize>(ascii: &[u8]) -> [u16; N] {
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
}

/// `OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE`.
const OBJ_KEY_ATTRIBUTES: u32 = 0x40 | 0x200;
/// `KEY_QUERY_VALUE`. `ZwFlushKey` needs no particular access to the handle.
const KEY_QUERY_VALUE: u32 = 0x1;

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
    record_named(&buf[..=n], value);
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
    /// `FlipWdogMs` (default 0 = off). The opt-in flip watchdog (`ddi::stall_diag`,
    /// `docs/zero-copy-present.md` "Stall diagnosis"): when a pending flip has gone this many
    /// milliseconds (in vsync ticks) with no address published, the vsync DPC publishes the
    /// flip's address as a KEPT picture (for any class of allocation, Venus included), and the
    /// Venus direct exits (a spent retry budget, a permanent reject) publish kept too. The kept
    /// address names a picture that is not on the screen: a diagnostic and recovery valve, not a
    /// completion. Nonzero values are clamped to 50..60000. Read at every StartDevice; mirrored
    /// as `FlWdMsEff`.
    pub const FLIP_WDOG_MS: KnobName = KnobName::new(b"FlipWdogMs");
    /// `DeferBudget` (default 0 = unlimited, today's behaviour). The most Deferred programming
    /// attempts of one primary (about one per vsync tick) before the worker publishes the flip's
    /// address kept and lowers the gate instead of retrying again (`FkDefBud`). 240 is about
    /// four seconds at 60 Hz. Nonzero values are clamped to 16..4000000. Read at every
    /// StartDevice; mirrored as `DefBudEff`.
    pub const DEFER_BUDGET: KnobName = KnobName::new(b"DeferBudget");
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
    /// `DXGK_FLIPCAPS` OVERRIDE. 0 (default) = the driver's own word
    /// (`FlipOnVSyncMmIo | FlipImmediateMmIo`); nonzero replaces it verbatim,
    /// so `FlipCapsX=2` restores the pre-2026-07-29 advertisement for an A/B.
    /// Bit order (bindgen, WDK 10.0.26100): 0 `FlipOnVSyncWithNoWait`,
    /// 1 `FlipOnVSyncMmIo`, 2 `FlipInterval`, 3 `FlipImmediateMmIo`. Read at
    /// AddAdapter, so `pnputil /restart-device` applies it without a rebuild.
    pub const FLIP_CAPS_EXTRA: KnobName = KnobName::new(b"FlipCapsX");
    /// `DXGK_DRIVERCAPS.MaxQueuedFlipOnVSync` — how many flips dxgkrnl may keep
    /// queued and pending on this adapter at once. Default 1 is the historical
    /// advertisement; a Helios flip retires only when its DMA fence completes,
    /// which by design waits on the venus work outstanding at submit, so a
    /// depth of 1 makes present N+1 wait for frame N's host completion. Read at
    /// AddAdapter, so `pnputil /restart-device` applies it without a rebuild.
    /// 0 is coerced to 1 (a zero-depth flip queue is not representable) and the
    /// value actually advertised is mirrored in the `FlipQueV` counter.
    pub const FLIP_QUEUE_DEPTH: KnobName = KnobName::new(b"FlipQueueN");
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
    /// This is NOT a switch back to INTx. Whether the OS hands the driver
    /// messages or the INTx line is decided by PnP before `StartDevice` from the
    /// device key's `MSISupported`; see `docs/msi-interrupts.md` for the one
    /// `reg add` that forces INTx. Snapshotted at transport init, so
    /// `pnputil /restart-device` applies it without a reboot.
    pub const MSI_VECTORS: KnobName = KnobName::new(b"MsiVectors");

    /// Default-enabled capacity notification for retry of a full Venus transport
    /// queue. 0 preserves historical 1 ms polling; no capacity change.
    pub const SUBMIT_SPACE_WAKE: KnobName = KnobName::new(b"SubSpaceWake");
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

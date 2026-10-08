//! The UMD's `HKLM\SOFTWARE\Helios` registry knobs, with their defaults as data.
//!
//! Four accessors used to be four literal copies of one ~33-line body: the same
//! `advapi32!RegGetValueA` redeclaration, the same `HKEY_LOCAL_MACHINE` /
//! `RRF_RT_REG_DWORD` constants, the same `SOFTWARE\Helios` subkey, the same
//! `OnceLock`, differing only in the value name and in an unlabelled tail
//! expression that decided what "absent" meant. That is policy-in-boilerplate:
//! a knob copy-pasted with the wrong tail is silently the wrong value on every
//! machine that never wrote the value, with no counter and no log.
//!
//! Here the default is a constructor argument, so it is impossible to write a
//! knob without stating what an absent value means, and the FFI call exists at
//! one audited site instead of four.
//!
//! **The registry value names, the hive and the `RRF` flag are the owner's
//! debugging ABI and are unchanged.** So are the remaining defaults:
//!
//! | Value | Type | Absent |
//! |---|---|---|
//! | `UmdTrace` | DWORD | `false` (explicit non-zero enables) |
//! | `UmdTimerRes` | DWORD | `true` (explicit 0 disables our timer request) |
//! | `FeatureLevel11` | DWORD | `1` |
//! | `VehicleFlipGateUs` | DWORD | `32000` |
//! | `ScanoutAcquire` | DWORD | `true` (explicit 0 is the kill switch) |
//! | `ScanoutSnapshot` | DWORD | `true` (explicit 0 is the kill switch) |
//! | `UmdPresentBatchFold` | DWORD | `true` (explicit 0 is the kill switch) |
//! | `UmdAsyncPresentStream` | DWORD | `true` (explicit 0 keeps the old gate) |
//! | `UmdFreeThreaded` | DWORD | `true` (explicit 0 reverts the threading surface) |
//! | `UmdCommandLists` | DWORD | `true` (explicit 0 reverts to emulated lists) |
//! | `UmdDeferredDiagnostics` | DWORD | `false` (diagnostic atomics, opt-in) |
//! | `UmdDdiLevel` | DWORD | `0x10` (D3D11 DDI WDDM1.3; `0x24` adds WDDM2.3) |
//! | `UmdDdiLevelDwm` | DWORD | `0x10` (the same, dwm.exe only) |
//! | `UmdDdiAllowList` | REG_SZ | absent (`;`-separated executables that get WDDM2.3) |
//!
//! ⛔ **`PresentGateUs` and `PresentOrder` were DELETED 2026-07-29 by owner
//! directive and must not come back.** They were the producer-side CPU
//! present gate: `PresentOrder=0` made the app's Present block until its own
//! GPU work finished, and `PresentGateUs` put a timeout on that block. It is a
//! hack in both directions — on expiry it publishes the present with work
//! still outstanding (the very thing it exists to prevent), and when it does
//! hold it removes all CPU/GPU overlap, costing Fire Strike GT1 158 -> 136 fps.
//! It also does not work: with `PresentOrder=0` and a 200 ms bound that cannot
//! expire, the owner still saw black-frame flashes, only less often. Reaching
//! for a producer-side stall to "fix" an ordering defect hides the defect
//! instead of fixing it; the ordering belongs on the GPU timeline
//! (`publish_present_order` + a consumer-side wait), not on a blocked CPU
//! thread. See ROADMAP defect 0ab.
//!
//! Two policies survive, not four: `BoolKnob` ("absent = off, non-zero = on")
//! and `DwordKnob` ("absent = this default, else the stored value"). The
//! absent-means-ON policy (`rc != 0 || value != 0`) belonged to
//! `VehicleKernelFlipWait` and the second bool to `PresentSyncPublish`; both
//! knobs went with T6/R912 when the kwait subsystem was retired.
//!
//! **Not covered here:** the environment-variable knobs, which are process
//! environment rather than registry state and have their own `OnceLock`s —
//! `HELIOS_DXGI_NO_REDIRECTION` (`lib.rs`), and `HELIOS_PRESENT_READBACK` /
//! `HELIOS_PRESENT_FORCE_OPAQUE` / `HELIOS_PRESENT_OPTIMIZE_COMPOSITION` /
//! `HELIOS_PRESENT_DUMP_DIR` (`forward.rs`). They are listed here so the knob
//! inventory is readable in one place even though the reader is not shared.

use helios_umd_common::knobs::{BoolKnob, DwordKnob};

// ⚠ `reg_dword` (the single audited advapi32 FFI site), `DwordKnob` and
// `BoolKnob` moved to `helios_umd_common::knobs` (`DECISIONS.md` D3b, stage S2).
// ⛔ THE KNOB VALUES DID NOT MOVE and must not: D3b says "the knob values stay
// per-crate -- `umd12` declares its own set including `UmdD3D12`". Sharing the
// table would make one driver's A/B lever silently apply to the other, and
// `UserModeDriverName[3]` is meant to be the only coupling between them.

// --- The knob set ----------------------------------------------------------
//
// Every registry knob the UMD reads is declared here. Adding one anywhere else
// is the drift this module exists to stop.

/// Per-frame/per-op DDI chatter (`trace_line!`). Absent = OFF.
pub(crate) static UMD_TRACE: BoolKnob = BoolKnob::new(c"UmdTrace", false);

/// Native DXGI bypasses DXVK's swapchain timer setup. On 2026-09-12, PassMark's
/// Venus polling sleeps lasted about 11 ms; a process-local 1 ms request raised
/// its active DX11 frame rate from about 16 to 31 FPS. Keep 0 for paired A/B runs.
pub(crate) static UMD_TIMER_RESOLUTION: BoolKnob = BoolKnob::new(c"UmdTimerRes", true);

/// Feature-level profile selector. Absent = 1 (the full FL11 profile).
pub(crate) static FEATURE_LEVEL_11: DwordKnob = DwordKnob::new(c"FeatureLevel11", 1);

/// Dcomp-vehicle flip-ordering gate cap, microseconds. Absent = 32 ms.
pub(crate) static VEHICLE_FLIP_GATE_US: DwordKnob = DwordKnob::new(c"VehicleFlipGateUs", 32_000);

/// D4a scanout-read acquire kill switch (FIX-DESIGN-d4a.md §4). Absent = ON;
/// explicit 0 disables without a reboot (a fresh process re-reads it).
///
/// This gates the **GPU-timeline** ordering mechanism the ⛔ note above points
/// at as the correct alternative to the deleted CPU present gate: at submit
/// time the DXVK engine arms a `VkSemaphoreSubmitInfo` TOP_OF_PIPE wait on the
/// command list that re-writes a scan-out buffer, iff the KMD's read ledger
/// says a host readback of THAT buffer is still in flight (`issued > retired`).
/// The wait parks the host GPU queue, never a guest CPU thread — no app,
/// present, CS or submit thread ever blocks, which is what distinguishes it
/// from PresentGateUs/PresentOrder and keeps it on the right side of the
/// owner directive. OFF (or an old KMD failing the probe) reverts to today's
/// unordered behavior with a single cheap flag check on the flush path.
pub(crate) static SCANOUT_ACQUIRE: BoolKnob = BoolKnob::new(c"ScanoutAcquire", true);

/// D4b ordered-snapshot substitution kill switch
/// (FIX-DESIGN-d4b-snapshot.md §3). Absent = ON; explicit 0 disables without
/// a reboot (a fresh process re-reads it).
///
/// Gates the direct-flip present-time snapshot: DXVK records a **GPU-queue-
/// ordered** image copy of the presented primary into a 4-slot ring of
/// ICD-owned OPTIMAL snapshot images, and the present's private data then
/// describes the snapshot (`HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT`) so the KMD
/// binds/flushes an image whose sole writer is that ordered copy — app
/// clears/draws can never touch a scanned-out surface. The copy rides frame
/// N's own command stream at present position; **no CPU stall is introduced
/// anywhere**, which is what keeps it on the right side of the ⛔ note above
/// (the deleted `PresentGateUs`/`PresentOrder` were producer-side CPU gates;
/// this is command-stream ordering the GPU already provides). Substitution
/// additionally requires the KMD to advertise
/// `HELIOS_SCANOUT_CAP_SNAPSHOT_BIND` in the D4a probe reply
/// (`scanout_acquire::scanout_snapshot_capable`). The knob disables optional
/// isolation only: MSAA and sRGB sources still require normalization before
/// the single-sample, encoded-byte consumer may read them. An incapable KMD
/// makes those presents fail explicitly rather than publishing raw memory.
pub(crate) static SCANOUT_SNAPSHOT: BoolKnob = BoolKnob::new(c"ScanoutSnapshot", true);

/// Ordinary-present batch-fold kill switch. Absent = ON; explicit 0 keeps the
/// historical ordering where `publish_present_order` is recorded after the
/// Present flush. When enabled, the ordinary no-debug path records the
/// present-fence signal after its copy/snapshot and before that existing flush,
/// so the signal shares the frame's real submission. Vehicle, force-opaque and
/// readback presents deliberately retain their historical sequencing.
pub(crate) static UMD_PRESENT_BATCH_FOLD: BoolKnob = BoolKnob::new(c"UmdPresentBatchFold", true);

/// Registered monotonic present-stream kill switch. Absent = ON; explicit 0
/// preserves the old frame gate even when an early folded publication has a
/// valid KMD correlation. This gates only the final skip decision: it never
/// changes publication or the existing Flush that dispatches the frame batch.
/// Required normalized WindowedBlt presents refuse without that correlation.
pub(crate) static UMD_ASYNC_PRESENT_STREAM: BoolKnob =
    BoolKnob::new(c"UmdAsyncPresentStream", true);

/// FREETHREADED THREADING-caps kill switch (Phase B of the command-list
/// build, `tmp/handoff-perf-structural/PLAN-commandlists.md`). Absent = ON;
/// explicit 0 reverts the adapter to THREADING caps = 0 without a redeploy
/// (a fresh process re-reads it; the caps answer is per-process).
///
/// ON reports `D3D11DDICAPS_FREETHREADED`: the runtime stops taking its
/// device critical section around create/destroy/calc DDIs, so they arrive
/// concurrently with immediate-context DDIs — the contention this removes
/// was measured at 5.6 % of the render thread (65th session,
/// `RtlpEnterCriticalSectionContended` under `CUseCountedObject::Release`).
/// The state this exposes went thread-safe in Phase A (`ShaderCaches` mutex,
/// `CtxBindings` atomics, `direct_scanout_allocations` mutex); present-path
/// RefCells stay immediate-only and the runtime still serializes those.
/// This knob NEVER enables command-list caps — see
/// `device_funcs::threading_caps`.
pub(crate) static UMD_FREE_THREADED: BoolKnob = BoolKnob::new(c"UmdFreeThreaded", true);

/// Native deferred-context/command-list DDIs (Phase C of the command-list
/// build, `tmp/handoff-perf-structural/PLAN-commandlists.md`).
///
/// **Default ON since 2026-08-05**; explicit 0 reverts to the runtime's
/// emulated path. The bring-up comment used to say the default flips "only
/// after the full Phase C gate set passes" — it has passed: with this on, plus
/// FREETHREADED and the DXVK CL fast/inline/recycle/sampler-retention set,
/// `tmp/handoff-perf-structural/reports/p3-227-recovery-outcome.md` records
/// **GT1 224.16 / GT2 229.17 / Graphics 52,126 / Combined 8,465**, against
/// GT1 ~184 and Graphics ~43.5k on the emulated path. Every accepted score
/// since 2026-08-03 was measured with it on, supplied by the test VM's
/// registry, so leaving the code default OFF meant a fresh install shipped a
/// materially slower driver than the one being measured.
///
/// ON (and only with [`UMD_FREE_THREADED`] also on — COMMANDLISTS requires
/// FREETHREADED) reports `D3D11DDICAPS_COMMANDLISTS_BUILD_2`: the runtime
/// stops emulating command lists (worker-thread SWDC recording + render-
/// thread SWCL replay, the verified #1 render-thread cost) and instead
/// records through our deferred-context DDI onto DXVK's stock
/// `D3D11DeferredContext`, handing finished `ID3D11CommandList`s to
/// `pfnCommandListExecute`. The DDI slots themselves are real and installed
/// unconditionally; this knob only controls whether the caps bit invites the
/// runtime to use them. See `device_funcs::threading_caps`.
pub(crate) static UMD_COMMAND_LISTS: BoolKnob = BoolKnob::new(c"UmdCommandLists", true);

/// Deferred-context/command-list success-path counters and sampled logs.
///
/// The native command-list path finishes and executes work on many worker
/// threads. Its evidence counters used to perform two process-global atomic
/// RMWs per successful finish/execute (the counter and `LogThrottle`), which
/// turns instrumentation into a contended cache line in the benchmarked path.
/// Keep the evidence available for an explicit diagnostic run, but absent =
/// OFF means a timed run does no diagnostic atomic RMW at all.
pub(crate) static UMD_DEFERRED_DIAGNOSTICS: BoolKnob = BoolKnob::new(c"UmdDeferredDiagnostics", false);

/// D3D11 DDI interface the adapter advertises (`ddi_level.rs`). Absent = 0x10
/// (D3DWDDM1_3, what every process negotiated before); 0x24 (D3DWDDM2_3)
/// advertises the WDDM 2.3 D3D11 DDI above it. Never applied to dwm.exe, which
/// has its own `UmdDdiLevelDwm` (same values, absent = 0x10).
pub(crate) static UMD_DDI_LEVEL: DwordKnob = DwordKnob::new(c"UmdDdiLevel", 0x10);

/// `UmdDdiLevel` for dwm.exe only. Absent = 0x10: DWM keeps the WDDM 1.3 D3D11
/// DDI until the 2.3 path has been proven in ordinary processes.
pub(crate) static UMD_DDI_LEVEL_DWM: DwordKnob = DwordKnob::new(c"UmdDdiLevelDwm", 0x10);

/// The knob inventory, so the set is enumerable instead of grep-discoverable.
///
/// Each entry is `(value name, resolved value as text)`. Resolving forces every
/// `OnceLock`, which is why this is not called on any hot path — it exists for
/// a one-shot dump at load, and for anyone asking "what knobs are there".
/// Emit this crate's knob inventory through the shared reader, once per process.
///
/// The thin wrapper D3b's split implies: the READER is shared
/// (`helios_umd_common::log::log_knob_inventory`) because the log format is the
/// evidence contract, while the SET is per-crate. `crate::log_knob_inventory()`
/// keeps resolving at its one call site in `open_adapter_common`, and the
/// emitted lines are byte-identical to before the move — which is `S2-check`.
pub(crate) fn log_knob_inventory() {
    helios_umd_common::log::log_knob_inventory(&resolved_inventory());
}

pub(crate) fn resolved_inventory() -> [(&'static str, u32); 13] {
    [
        ("UmdTrace", UMD_TRACE.get() as u32),
        ("UmdTimerRes", UMD_TIMER_RESOLUTION.get() as u32),
        ("FeatureLevel11", FEATURE_LEVEL_11.get()),
        ("VehicleFlipGateUs", VEHICLE_FLIP_GATE_US.get()),
        ("ScanoutAcquire", SCANOUT_ACQUIRE.get() as u32),
        ("ScanoutSnapshot", SCANOUT_SNAPSHOT.get() as u32),
        ("UmdPresentBatchFold", UMD_PRESENT_BATCH_FOLD.get() as u32),
        ("UmdAsyncPresentStream", UMD_ASYNC_PRESENT_STREAM.get() as u32),
        ("UmdFreeThreaded", UMD_FREE_THREADED.get() as u32),
        ("UmdCommandLists", UMD_COMMAND_LISTS.get() as u32),
        ("UmdDeferredDiagnostics", UMD_DEFERRED_DIAGNOSTICS.get() as u32),
        ("UmdDdiLevel", UMD_DDI_LEVEL.get()),
        ("UmdDdiLevelDwm", UMD_DDI_LEVEL_DWM.get()),
    ]
}

// ── The typed accessors ──────────────────────────────────────────────────────
//
// Moved verbatim out of `lib.rs` by T8/R1106, beside the knobs they read.
// `lib.rs` re-exports all of them, so `crate::trace_enabled()`,
// `crate::feature_level_mode()` and `crate::vehicle_flip_gate_us()` still
// resolve at every call site.

/// Resolve `HKLM\SOFTWARE\Helios!UmdTrace` (REG_DWORD) != 0, forcing its
/// `OnceLock`. Read once per process.
///
/// ⚠ This is the KNOB. The GATE that `trace_line!` consults is
/// `helios_umd_common::log::trace_enabled()`, which caches this answer in a
/// relaxed `AtomicBool` at `log::init` time (stage S2). Two names for what used
/// to be one function, and the split is deliberate: `trace_line!` expands at
/// ~430 sites, many per-op, so the gate must be one relaxed load and not a
/// `OnceLock` walk through a knob table the shared crate cannot see.
/// `crate::trace_enabled()` now re-exports the GATE, so every call site reads
/// the cached answer.
///
/// Errors, one-shots and refusals keep using `log_error!` unconditionally —
/// only known-hot repeat traffic (Present, OMSetRenderTargets,
/// ResolveSharedResource, per-op stamps) sits behind the gate.
pub(crate) fn umd_trace_knob() -> bool {
    UMD_TRACE.get()
}

/// Selects whether the adapter advertises the full D3D11 feature-level profile
/// or the conservative FL10_0 fallback:
/// `HKLM\SOFTWARE\Helios!FeatureLevel11` (REG_DWORD). Absent = full FL11
/// profile; explicit 0 = FL10_0 opt-out. Read once
/// per process, so an already-running dwm keeps the level it created its
/// device at while freshly-launched apps pick up the new value.
///
/// This gate MUST cover the three caps together — the 3DPIPELINESUPPORT
/// pipeline level, `check_format_support`'s multisample bits, and
/// `CheckMultisampleQualityLevels` — because the Microsoft runtime validates
/// them as one coherent feature-level contract during
/// `CDevice::LLOCompleteLayerConstruction`; a partial change is rejected with
/// DXGI_ERROR_UNSUPPORTED. FL11_0 additionally requires real multisample
/// support, which the FL10_0 profile deliberately suppresses.
///
/// 30th/31st-session ETW evidence (Microsoft-Windows-DXGI) showed this is a UMD
/// caps sequence, not a KMD/adapter ceiling: the runtime reaches
/// CreateDevice/venus CTX_CREATE and rejects each bad caps contract with a
/// concrete string. Gates fixed so far: 3DPIPELINESUPPORT is a bitmask,
/// SHADER compute cap is 0x2, and MSAA/format support must match D3D11.3
/// §19.2.5. knob=0 remains the exact FL10_0 baseline opt-out for A/B.
///   absent = full FL11 profile
///   0 = FL10_0 profile
///   1 = full FL11_0 (pipeline 11_0 + real MSAA + unmasked format bits)
///   2 = DIAGNOSTIC: pipeline claims 11_0 but keeps the FL10 MSAA/format caps —
///       isolates pipeline-level validation from the later FL11 caps gates.
pub(crate) fn feature_level_mode() -> u32 {
    FEATURE_LEVEL_11.get()
}

/// Dcomp-vehicle flip-ordering gate cap in microseconds:
/// `HKLM\SOFTWARE\Helios!VehicleFlipGateUs` (REG_DWORD). Read once per
/// process. Absent = 32000; 0 disables (A/B lever). Bounds the worker-side
/// wait for the vehicle frame COPY's host-GPU completion before the flip is
/// minted: a direct/independent-flip present is ordered only on the KMD's
/// DMA fence, which completes at DECODE — without this gate the backbuffer
/// scans out before the venus copy lands and the previous occupant of the
/// buffer pops out (the 24th-session gameplay stutter). Composed presents
/// are protected by dwm's consumer wait either way; direct flip is not.
pub(crate) fn vehicle_flip_gate_us() -> u32 {
    VEHICLE_FLIP_GATE_US.get()
}

/// D4a scanout-read acquire kill switch:
/// `HKLM\SOFTWARE\Helios!ScanoutAcquire` (REG_DWORD). Read once per process.
/// Absent = ON. `false` means `scanout_acquire::init_for_device` does nothing
/// at all — no escapes, no event, no mapping — so the off path is
/// bit-identical to a build without the mechanism. See [`SCANOUT_ACQUIRE`].
pub(crate) fn scanout_acquire_knob() -> bool {
    SCANOUT_ACQUIRE.get()
}

/// D4b ordered-snapshot substitution kill switch:
/// `HKLM\SOFTWARE\Helios!ScanoutSnapshot` (REG_DWORD). Read once per process.
/// Absent = ON. `false` disables optional isolation; MSAA resolve and sRGB
/// encoded-byte normalization still require a snapshot. See [`SCANOUT_SNAPSHOT`].
pub(crate) fn scanout_snapshot_knob() -> bool {
    SCANOUT_SNAPSHOT.get()
}

/// Whether an ordinary Present records its producer timeline signal in the
/// same batch as the frame copy/snapshot. `HKLM\\SOFTWARE\\Helios!UmdPresentBatchFold`
/// (REG_DWORD), read once per process. Absent = ON; explicit 0 restores the
/// post-flush publication order for same-binary A/B.
pub(crate) fn present_batch_fold() -> bool {
    UMD_PRESENT_BATCH_FOLD.get()
}

/// Registered monotonic present-stream kill switch:
/// `HKLM\\SOFTWARE\\Helios!UmdAsyncPresentStream` (REG_DWORD), read once per
/// process. Absent = ON; explicit 0 retains the historical frame gate.
pub(crate) fn umd_async_present_stream() -> bool {
    UMD_ASYNC_PRESENT_STREAM.get()
}

/// FREETHREADED THREADING-caps kill switch:
/// `HKLM\SOFTWARE\Helios!UmdFreeThreaded` (REG_DWORD). Read once per process.
/// Absent = ON; explicit 0 reverts to caps = 0. See [`UMD_FREE_THREADED`].
pub(crate) fn umd_free_threaded() -> bool {
    UMD_FREE_THREADED.get()
}

/// Native command-list enable: `HKLM\SOFTWARE\Helios!UmdCommandLists`
/// (REG_DWORD). Read once per process. Absent = ON; explicit 0 reverts to the
/// runtime's emulated command lists. Forced off when [`umd_free_threaded`] is off —
/// COMMANDLISTS caps require FREETHREADED, so `UmdFreeThreaded=0` remains the
/// one kill switch that reverts the whole threading surface at once. See
/// [`UMD_COMMAND_LISTS`].
pub(crate) fn umd_command_lists() -> bool {
    UMD_COMMAND_LISTS.get() && UMD_FREE_THREADED.get()
}

/// Whether deferred-context/command-list success-path diagnostics are enabled:
/// `HKLM\\SOFTWARE\\Helios!UmdDeferredDiagnostics` (REG_DWORD). Read once per
/// process. Absent = OFF so the timed command-list path performs no diagnostic
/// counter or log-throttle atomic RMWs.
pub(crate) fn umd_deferred_diagnostics() -> bool {
    UMD_DEFERRED_DIAGNOSTICS.get()
}

// --- NVK on RM (dxvk-on-nvk S3) ----------------------------------------------
//
// Which ICD a process runs on is decided in the bridge
// (`umd_common/bridge/bridge_icd_backend.h`: `Icd`, `NvkDenyList`,
// `NvkAllowList`, `NvkIcdPath`, and `ForeignImport` for Venus processes that
// compose NVK surfaces). These two only shape how an NVK device presents.

/// `NvkPresent`: 0 = automatic (compose through DWM when the KMD gave the back
/// buffer a resource id, else show it on scanout 0), 1 = always scanout 0
/// (zero-copy flip of the back buffer; the desktop is hidden while the app
/// presents), 2 = always the WDDM present (DWM composes).
pub(crate) static NVK_PRESENT: DwordKnob = DwordKnob::new(c"NvkPresent", 0);

/// `NvkScanoutComposeEvery`: how often a frame that NVK already showed on
/// scanout 0 also goes through the WDDM present (`pfnPresentCb`) to DWM.
/// 0 (default) = only the first frame; N = the first and every Nth after it;
/// 1 = every frame (the behaviour before this knob).
///
/// For a blt-model swapchain (`DXGI_SWAP_EFFECT_DISCARD`, windowed: Heaven)
/// that present is a Blt into DWM's GDI redirection surface: the KMD copies
/// the frame on the host, waits for the copy on the presenting thread and
/// mirrors it into the surface's system pages, and dxgkrnl copies it again
/// through GDI (`DxgkEngBltViaGDI`). Measured 2026-10-06 (Heaven 1600x900,
/// xperf): about a fifth of the render thread, which made it CPU-bound at
/// ~170-200 fps against 357 for app-local DXVK, which never presents through
/// DXGI. DWM's copy is not shown anyway while the app owns scanout 0.
pub(crate) static NVK_SCANOUT_COMPOSE_EVERY: DwordKnob =
    DwordKnob::new(c"NvkScanoutComposeEvery", 0);

/// `NvkScanoutComposeEvery`, or `HELIOS_NVK_SCANOUT_COMPOSE_EVERY` from the
/// process environment.
pub(crate) fn nvk_scanout_compose_every() -> u32 {
    static CELL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::var("HELIOS_NVK_SCANOUT_COMPOSE_EVERY")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .or_else(|| helios_umd_common::knobs::reg_dword(c"NvkScanoutComposeEvery"))
            .unwrap_or_else(|| {
                // A WDDM 2.x D3D11 device must hand every frame to the runtime:
                // d3d11!NDXGI::CDevice::PresentImpl throttles flip-model presents
                // on its frame-latency semaphore (2 s timeout per Present), which
                // is only released through the per-frame WDDM present. Skipping
                // it after the NVK scanout flip (the 470a978 default) made FFXIV
                // run at ~0.3 fps under HELIOS_UMD_DDI=2.3. Until the KMD can
                // retire a "shown on scanout, nothing to copy" present cheaply,
                // 2.3 processes pay the per-frame present (WDDM 1.3 keeps 0).
                if crate::ddi_level::ddi_level() == crate::ddi_level::DdiLevel::Wddm2_3 {
                    1
                } else {
                    NVK_SCANOUT_COMPOSE_EVERY.get()
                }
            })
    })
}

/// `NvkPlaceholderAllocations`: 1 = never ask NVK for resource ids; every
/// WDDM-backed texture gets a KMD placeholder (A/B and fallback lever).
pub(crate) static NVK_PLACEHOLDER_ALLOCATIONS: BoolKnob =
    BoolKnob::new(c"NvkPlaceholderAllocations", false);

/// This process is dwm.exe. DWM on NVK (`DwmIcd=nvk`, docs/dwm-on-nvk.md)
/// presents only through the WDDM flip: its frames are the desktop, and the
/// KMD flips its swap-chain buffers (their foreign resource ids) itself.
pub(crate) fn is_dwm_process() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("dwm.exe")))
            .unwrap_or(false)
    })
}

/// `NvkPresent`, or `HELIOS_NVK_PRESENT` from the process environment (tests:
/// one process, no registry write).
pub(crate) fn nvk_present_mode() -> u32 {
    static CELL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::var("HELIOS_NVK_PRESENT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| NVK_PRESENT.get())
    })
}

/// `VideoDdi`: which devices get the D3D11.1 video DDI (decoder and video
/// processor, `forward/video.rs`). 0 = none (the behaviour before it existed:
/// no `ID3D11VideoDevice` at all), 1 = NVK devices (default; H.264 decode on
/// Vulkan Video when the NVK build has it), 2 = every device (Venus gets the
/// DXVK video processor and no decoder). The bridge also sets
/// `NVK_EXPERIMENTAL=video` for NVK processes unless this is 0
/// (`umd_common/bridge/bridge_icd_backend.cpp`).
pub(crate) static VIDEO_DDI: DwordKnob = DwordKnob::new(c"VideoDdi", 1);

pub(crate) fn nvk_placeholder_allocations() -> bool {
    NVK_PLACEHOLDER_ALLOCATIONS.get()
}

// --- NVK on RM: RM fences (dxvk-on-nvk S4, docs/rm-fence-marker.md) ---------

/// `NvkRmFence`: 1 (default) = an NVK present hands its flip an RM fence and
/// does not wait on the CPU for the frame (needs NVK with
/// `helios_icd_interface` version 3 and a host with DRM fences; the KMD flips
/// on the fence with capability bit 32, else NVK's flip thread does). 0 = the
/// S3 CPU wait before every NVK present.
pub(crate) static NVK_RM_FENCE: BoolKnob = BoolKnob::new(c"NvkRmFence", true);

/// `NvkRmFencePresent`: 1 (default) = when DWM composes an NVK app's frames, the
/// WDDM present carries the RM fence in its `HEPR`/`HERF` tail and the KMD
/// retires the present on it (needs capability bit 33, the KMD's (b) carrier).
/// Default on since 22.22.339.2, after a 10-minute windowed Heaven soak with no
/// pending-flip, gate or escape timeouts (windowed Heaven 121 -> 213 fps).
/// 0 (registry, or `HELIOS_NVK_RM_FENCE_PRESENT=0`) = the S3 CPU wait for
/// composed frames.
pub(crate) static NVK_RM_FENCE_PRESENT: BoolKnob = BoolKnob::new(c"NvkRmFencePresent", true);

/// `NvkRmCopyRecord`: 1 (default) = a composed NVK frame whose WDDM present
/// carries an RM fence also carries the copy-engine Present record (`'HEF3'`,
/// the 168-byte `HERF` / 192-byte `HEPR`, docs/rm-copy-engine-present.md 12)
/// when the KMD reads it (QueryCaps bit 37) and NVK has `queue_rm_fence_v3`.
/// The record is only an input: whether a frame is copied on the copy engine
/// is the KMD's `RmCopyEngine` knob and its per-frame decision. 0 (registry,
/// or `HELIOS_NVK_RM_COPY_RECORD=0`) = the 48 / 96-byte fence forms as before.
pub(crate) static NVK_RM_COPY_RECORD: BoolKnob = BoolKnob::new(c"NvkRmCopyRecord", true);

/// `NvkSkipBltCopy` (`HKLM\\SOFTWARE\\Helios`, or `HELIOS_NVK_SKIP_BLT_COPY`): 1 =
/// when DXGI hands a windowed NVK present a destination resource, the UMD does
/// NOT copy the frame into it before the WDDM present; the KMD's Blt reads the
/// present's source (`hSrcAllocation`) and writes it into the window's surface
/// at the client offset. 0 = the UMD copies it, as before. Absent: 1 when the
/// KMD's `RedirVram` is 1 (its service key), else 0.
///
/// The UMD's copy goes to (0, 0) of the destination, which with `RedirVram` is
/// the window's redirection texture including its non-client area, while the
/// KMD's copy places the frame at the client offset: two writers, and with the
/// copy-engine route asynchronous the image jumped by the title-bar height at
/// frame rate (403.1, windowed Heaven; smooth with the copy skipped, no fps
/// cost in 386.1).

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().and_then(|v| match v.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    })
}

/// `NvkRmFence`, or `HELIOS_NVK_RM_FENCE` from the process environment.
pub(crate) fn nvk_rm_fence() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| env_bool("HELIOS_NVK_RM_FENCE").unwrap_or_else(|| NVK_RM_FENCE.get()))
}

/// `NvkRmFencePresent`, or `HELIOS_NVK_RM_FENCE_PRESENT` from the environment.
pub(crate) fn nvk_rm_fence_present() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        env_bool("HELIOS_NVK_RM_FENCE_PRESENT").unwrap_or_else(|| NVK_RM_FENCE_PRESENT.get())
    })
}

/// `NvkRmCopyRecord`, or `HELIOS_NVK_RM_COPY_RECORD` from the environment.
pub(crate) fn nvk_rm_copy_record() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        env_bool("HELIOS_NVK_RM_COPY_RECORD").unwrap_or_else(|| NVK_RM_COPY_RECORD.get())
    })
}

/// `DirectFlipSupport` (REG_DWORD), or `HELIOS_DIRECT_FLIP_SUPPORT` from the
/// process environment: what the D3D11.1 `CheckDirectFlipSupport` DDI answers.
/// 0 = never (the opt-out); 1 (default) = yes when dxgkrnl
/// reports DirectFlip support for the Helios adapter (KMTQAITYPE_DIRECTFLIP_SUPPORT,
/// i.e. the KMD's SupportDirectFlip cap) and the two resources have the same
/// size and format; 2 = yes whenever size and format match (test lever); 3 = as 1, and also only
/// for a pair the KMD can scan out as is (one of R8G8B8A8 / B8G8R8A8 / B8G8R8X8 UNORM, one sample,
/// one mip, one slice). 3 was briefly the meaning of 1 (driver 388.1).
/// Windows decides independent flip and the blt-to-flip swap-effect upgrade
/// partly from this answer.
pub(crate) fn direct_flip_support() -> u32 {
    static CELL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::var("HELIOS_DIRECT_FLIP_SUPPORT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .or_else(|| helios_umd_common::knobs::reg_dword(c"DirectFlipSupport"))
            // 1: DWM consults this answer for independent flip (394.1: 0 kept every frame
            // composed). It still says no unless dxgkrnl reports DirectFlip, i.e. unless the KMD's
            // `IndepFlip` (default 1) advertises it.
            .unwrap_or(1)
    })
}

/// `NvkSkipBltCopy`: the environment, then the registry, then the KMD's
/// `RedirVram` (see the note above `env_bool`).
pub(crate) fn nvk_skip_blt_copy() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        env_bool("HELIOS_NVK_SKIP_BLT_COPY")
            .or_else(|| helios_umd_common::knobs::reg_dword(c"NvkSkipBltCopy").map(|v| v != 0))
            .unwrap_or_else(|| {
                helios_umd_common::knobs::kmd_service_dword(c"RedirVram") == Some(1)
            })
    })
}

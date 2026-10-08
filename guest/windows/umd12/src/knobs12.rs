//! The D3D12 UMD's `HKLM\SOFTWARE\Helios` registry knobs.
//!
//! ⛔ **The knob VALUES are per-crate and that is D3b's explicit instruction**
//! (`helios_umd_common::knobs` says the same at its own head): only the
//! *mechanism* — the single audited `advapi32!RegGetValueA` site, [`BoolKnob`]
//! and `DwordKnob` — is shared. Sharing the *table* would mean one driver's A/B
//! lever silently applying to the other, and `UserModeDriverName[3]` is supposed
//! to be the only coupling between `helios_umd.dll` and `helios_umd12.dll`.
//!
//! ⚠ Which is why the trace knob here is `Umd12Trace` and **not** `UmdTrace`.
//! They are different registry values in the *same* hive: the hive is shared on
//! purpose (the owner types these by hand and a second subkey would be a second
//! thing to remember), the value names are not. `UmdTrace=1` must not turn on
//! per-op chatter in the D3D12 driver, because the two logs are read separately
//! (`umd-<pid>.log` vs `umd12-<pid>.log`) and a shared name would make "trace
//! was on" ambiguous across two files.
//!
//! | Value | Type | Absent |
//! |---|---|---|
//! | `Umd12Trace` | DWORD | `false` (explicit non-zero enables) |
//! | `UmdD3D12` | DWORD | `true` — explicit `0` disables D3D12 (D11) |
//! | `Umd12FormatCaps` | DWORD | `0` — `pfnCheckFormatSupport`'s encoding, as an A/B |
//! | `Umd12FenceSignalDelayUs` | DWORD | `0` — **diagnostic**, the F1 delay probe on `pfnSignalFence` |
//! | `Umd12EclDelayUs` | DWORD | `0` — **diagnostic**, the F1 delay probe on `pfnExecuteCommandLists` |
//! | `Umd12PipelineLibrary` | DWORD | `true` — explicit `0` refuses pipeline libraries with `E_NOTIMPL` |
//!
//! ⭐ **`UmdD3D12` lands here at S5, and not one commit earlier.** A kill switch
//! for a driver that cannot be reached kills nothing, so declaring it before
//! slot 3 existed would have put a knob in the inventory line whose value
//! provably had no effect — the `DECISIONS.md` §7.1 / R908 failure mode in
//! miniature. It arrives in the same commit that registers
//! `UserModeDriverName[3]`, deletes `umd`'s duplicate `OpenAdapter12` export and
//! makes `adapter12::OpenAdapter12`'s body reachable.

use helios_umd_common::knobs::{BoolKnob, DwordKnob};

/// ⭐ **`pfnCheckFormatSupport`'s ENCODING, as an A/B rather than an assumption.**
///
/// `d3d12umddi.h` defines a 20-bit `D3D12DDI_FORMAT_SUPPORT` enum immediately
/// beside `PFND3D12DDI_CHECKFORMATSUPPORT`, whose values are byte-for-byte the
/// D3D10 DDI's. That is strong evidence the DDI has its own small encoding — but
/// it is *evidence*, not a measurement, and the D3D11 side of this project holds
/// the opposite result in as many words:
///
/// > "The D3D11 DDI `pfnCheckFormatSupport` returns API-style
/// > `D3D11_FORMAT_SUPPORT` flags (D3D11 harmonized the DDI with the API enum;
/// > the small `D3D10_DDI_FORMAT_SUPPORT` enum is only for the legacy D3D10
/// > DDI). So pass DXVK's value through unchanged -- translating to the D3D10
/// > DDI layout regresses even a plain `D3D11CreateDevice` to
/// > `DXGI_ERROR_UNSUPPORTED`."  -- `umd/src/forward/format_caps.rs:15-19`
///
/// If D3D12 inherited that harmonization, translating is the same mistake one
/// API generation later, and it would present exactly as `D12-G7` does: a
/// device-creation failure whose ETW reason moves every time the answer changes.
/// ⛔ This knob exists so that question is settled by a measurement instead of a
/// third guess, and so the losing arm stays reachable afterwards (AGENTS.md
/// rule 8's other half).
///
/// | value | meaning |
/// |---:|---|
/// | 0 | **`D3D12DDI_FORMAT_SUPPORT`** -- translate the engine's API bits into the DDI enum and narrow them to this driver's caps. The default. |
/// | 1 | **API passthrough** -- hand the engine's `D3D12_FORMAT_SUPPORT1` back unchanged, exactly as the D3D11 driver does with DXVK's. ⛔ **MEASURED AND LOSING** (2026-08-06): it truncates the runtime's format sweep at 12 formats / 271 multisample queries, against 23 / 600 for arm 0. So the D3D12 DDI is **not** harmonized with the API enum the way D3D11's is, and arm 0 is right. Kept reachable as rule 8 requires. |
///
/// ⚠ Two further arms existed briefly (2: no multisample bits anywhere; 3:
/// multisample bits only alongside `RENDERTARGET`) purely to bisect which bit
/// the runtime was rejecting. **They are gone**, because the rule they were
/// hunting turned out to be written down already — `msaa_ineligible` in
/// `umd_common/src/format.rs`, which the D3D11 driver paid for. A diagnostic
/// arm that outlives its question is scaffolding.
///
/// ⚠ The default is 0 and stays 0 until an arm is measured green; flipping it
/// requires the evidence written at the read site in `caps12`.
pub(crate) static UMD12_FORMAT_CAPS: DwordKnob = DwordKnob::new(c"Umd12FormatCaps", 0);

/// The `pfnCheckFormatSupport` encoding mode. See [`UMD12_FORMAT_CAPS`].
pub(crate) fn umd12_format_caps() -> u32 {
    UMD12_FORMAT_CAPS.get()
}

/// Per-op/per-frame DDI chatter (`trace_line!`) for the D3D12 driver.
/// Absent = OFF.
pub(crate) static UMD12_TRACE: BoolKnob = BoolKnob::new(c"Umd12Trace", false);

/// **The D3D12 kill switch** (`DECISIONS.md` D11). Absent = ON; explicit 0 = OFF.
///
/// Read once per process at the top of `adapter12::OpenAdapter12`, above every
/// other check including the null test. Explicit 0 ⇒ `DXGI_ERROR_UNSUPPORTED`, i.e.
/// **bit-identical behaviour to a build with no D3D12 path at all**: nothing is
/// dereferenced, no table is written, and the only trace is the
/// `OpenAdapter12` refusal counter ticking.
///
/// Owner-directed default change, 2026-09-07. The enabled configuration already
/// passed all four native runtime ordering cases on .270 after reboot, with
/// Code 0 and a visible desktop. Completed Time Spy GT1 runs were 118.746094 /
/// 136.251602 FPS; Fire Strike GT1 was 248.231491 FPS. See
/// docs/PERFORMANCE_FEEDBACK.md for exact artifacts and post-boot variability.
/// The owner's shadow acceptance applies to .266; broader ownership, failure
/// and lifecycle gates remain open. This default change does not close them.
///
/// ⚠ Read once per process, deliberately: a running `dwm` keeps whatever
/// behaviour it started with while newly created processes pick the change up.
/// `HKLM\SOFTWARE\Helios` is writable over SSH with the desktop down, so the
/// switch is usable in exactly the situation it exists for.
pub(crate) static UMD_D3D12: BoolKnob = BoolKnob::new(c"UmdD3D12", true);

/// Resolve `HKLM\SOFTWARE\Helios!Umd12Trace` (REG_DWORD) != 0, forcing its
/// `OnceLock`. Read once per process.
///
/// ⚠ This is the KNOB. The GATE that `trace_line!` consults is
/// `helios_umd_common::log::trace_enabled()`, which caches this answer in a
/// relaxed `AtomicBool` when `log::init` runs — see `crate::init_once`. The
/// split is the same one `umd/src/knobs.rs:222-236` documents: the gate must be
/// one relaxed load, not a `OnceLock` walk through a knob table the shared crate
/// cannot see.
pub(crate) fn umd12_trace() -> bool {
    UMD12_TRACE.get()
}

/// Resolve `HKLM\SOFTWARE\Helios!UmdD3D12` (REG_DWORD) != 0, forcing its
/// `OnceLock`. Read once per process, at the top of
/// `adapter12::OpenAdapter12`.
pub(crate) fn umd_d3d12() -> bool {
    UMD_D3D12.get()
}

/// The largest delay either diagnostic arm below will honour, in microseconds.
///
/// ⚠ 2 s, and the number is not arbitrary: it is the `--settle 2000` window that
/// demonstrated the pixels arriving late in `tmp/dx12/gates/G8-r0-settle/`. A
/// value above it is clamped rather than refused, because a mistyped registry
/// DWORD must not be able to hang a DDI for the rest of the boot — the arm is a
/// measurement, and a measurement that wedges the machine produces nothing.
const MAX_DIAGNOSTIC_DELAY_US: u32 = 2_000_000;

/// ⛔⛔ **A DIAGNOSTIC ARM WITH A QUESTION ATTACHED. It is INERT BY DEFAULT and
/// it is NOT a fix.**
///
/// ⛔ **SUPERSEDED INSTRUCTION, recorded so nobody re-derives the old one.** This
/// doc used to end its first line with *"Delete it with the commit that lands the
/// WDDM submission"*, and [`UMD12_ECL_DELAY_US`] inherited that obligation by
/// reference. **The commit that landed the WDDM submission (K-F1) deliberately
/// kept both arms**, because deleting them would have removed the only lever that
/// separates the two readings the submission still has to be attributed against:
///
/// | reading | what it means once K-F1 is in |
/// |---|---|
/// | the app's fence wait grows with **this** delay | the runtime's fence advance is downstream of a DDI *returning* |
/// | it grows only with a delay the **KMD** imposes on the DMA packet | the advance is downstream of our packet *retiring* — which is what K-F1 is for |
///
/// Nothing in K-F1 answers that; it is `KMD_IMPACT.md` §14a.1's UV1, whose
/// settling experiment is the K-F0 scoped hold. ⇒ these arms outlive the
/// submission and retire with UV1, not with the callback.
///
/// # The question: where does the runtime's fence advance become downstream of this driver?
///
/// `D12-G8` rung 0 fails because the application's `ID3D12Fence` completes with
/// no causal dependency on the engine's Vulkan work: the probe's
/// `WaitForSingleObject` returns in 0.8–1.1 µs (against WARP's 561 µs) and the
/// readback surface is 0/65536 exact at T+0 and 65536/65536 exact at +2000 ms
/// through the *same still-live mapping* (`tmp/dx12/gates/G8-r0-settle/`). The
/// work lands; only the ordering is wrong.
///
/// ⭐ **The architecture is decided, and as of K-F1 the UMD half is LANDED**
/// (now the mandatory HE12 v2 packet): a real `pfnRenderCb` WDDM submission on the queue's
/// context during `pfnExecuteCommandLists`, so that the runtime's own kernel fence
/// signal queues *behind* work the KMD already withholds
/// `DXGK_INTERRUPT_DMA_COMPLETED` for. No stopgap, no producer-side stall in the
/// shipping driver.
///
/// ⛔ **But that design has a precondition nobody has measured: that the runtime
/// queues its fence signal on OUR context at all, rather than CPU-signalling it
/// independently of this driver.** If it does, the submission has to be in place
/// before the DDI the runtime gates on returns — and *which* DDI that is decides
/// where the submission goes and what it must already cover. This knob and
/// [`UMD12_ECL_DELAY_US`] are the experiment that reads it, one DDI at a time.
/// ⚠ K-F1 landing does not settle it: a submission that exists is not evidence
/// that dxgkrnl orders anything behind it. That is UV1.
///
/// ⚠ **A fixed delay, never a drain** (`FENCE-BRIDGE-DESIGN.md` §5 step 2): a
/// drain that fixes the pixels is consistent with every mechanism and settles
/// nothing, while a fixed delay isolates the causal link and cannot be mistaken
/// for a fix.
///
/// # What reading says what
///
/// Set this to `50000` (50 ms), run `clear.exe --sentinel --settle 2000` and
/// read one number — the probe's own `WaitForSingleObject signalled in N us`:
///
/// | this knob = 50000 | [`UMD12_ECL_DELAY_US`] = 50000 | what it says about the submission |
/// |---|---|---|
/// | N >= 50 000 µs | — | the advance is downstream of **`pfnSignalFence` returning**; whatever is submitted must be in place before that DDI returns |
/// | N ~ 1 µs | N >= 50 000 µs | it gates on **`pfnExecuteCommandLists` returning** — exactly where the `pfnRenderCb` submission goes. The best case. |
/// | N ~ 1 µs | N ~ 1 µs | ⛔ the runtime advances the fence independently of **both** DDIs, so the precondition is in doubt and must be settled directly — submit a DMA packet the KMD deliberately holds and see whether the app's fence wait grows |
///
/// Read `FenceSignalEntered` alongside it: a zero means the runtime did not
/// enter the native DDI, so this delay cannot attribute that run.
///
/// # Why this is inert by default and stays that way
///
/// Absent = `0` = **no delay**, so a machine with no registry value behaves
/// byte-identically to the build that has never heard of this knob (AGENTS.md
/// rule 8 is satisfied trivially: the shipping default is the measured one,
/// because every accepted measurement was taken with the value absent). The
/// non-zero arm is a producer-side CPU stall of exactly the kind
/// `umd/src/knobs.rs:31-43` forbids as a *fix*; it is legal here only because it
/// is a **measurement**, run deliberately for one probe at a time and never
/// shipped on. ⛔ It is not, and never becomes, the answer to the ordering defect
/// — that is the exact execution bridge's job, and a delay that "fixes" the pixels is
/// consistent with every mechanism and therefore evidence for none.
///
/// Clamped to [`MAX_DIAGNOSTIC_DELAY_US`]; each firing bumps
/// `FenceSignalDelayed`, so an arm that was set and never reached is
/// distinguishable from one that was never set.
pub(crate) static UMD12_FENCE_SIGNAL_DELAY_US: DwordKnob =
    DwordKnob::new(c"Umd12FenceSignalDelayUs", 0);

/// ⛔⛔ **A DIAGNOSTIC ARM WITH A QUESTION ATTACHED. Inert by default, and NOT a
/// fix.** The `pfnExecuteCommandLists` half of the experiment
/// [`UMD12_FENCE_SIGNAL_DELAY_US`] documents — read that doc for the question and
/// the reading table; everything there applies here with `pfnExecuteCommandLists`
/// substituted for `pfnSignalFence`.
///
/// ⛔ **KEPT BY K-F1, and its earlier "delete me" instruction is SUPERSEDED.**
/// This doc used to say *"Delete it with the commit that lands the WDDM
/// submission"*, and the same sentence sat at the read site in
/// `forward12::queue::execute_command_lists`. Both are wrong now, for a reason
/// that only became visible when the submission was written:
///
/// **what this arm measures now** — it delays the DDI's *return*, and the
/// `pfnRenderCb` submission happens *before* that return. So with
/// the exact execution bridge ON, a fence wait that grows with this delay says the
/// runtime's advance is downstream of **the DDI returning**; a fence wait that
/// does *not* grow, while a KMD-imposed hold on the DMA packet *does* move it,
/// says the advance is downstream of **our packet retiring**. Only the second is
/// the ordering K-F1 is built on, and no other instrument in the driver can tell
/// the two apart. Deleting this arm in the same commit would have shipped the
/// submission with no way to attribute its effect — `KMD_IMPACT.md` §14a.1's
/// UV1, unsettled.
///
/// ⇒ it retires with **UV1**, not with the callback. ⚠ And it must never be read
/// as the fix for the ordering: it is a CPU stall, `umd/src/knobs.rs:31-43`
/// forbids that as a fix, and the exact execution bridge is what actually closes the
/// defect.
///
/// ⚠ The two delay arms are run **separately**, never together: their whole
/// purpose is to attribute the runtime's fence advance to one DDI or the other,
/// and a run with both set cannot tell which delay the number came from.
///
/// Absent = `0` = no delay. Clamped to [`MAX_DIAGNOSTIC_DELAY_US`]; each firing
/// bumps `EclDelayed`.
pub(crate) static UMD12_ECL_DELAY_US: DwordKnob = DwordKnob::new(c"Umd12EclDelayUs", 0);

// HE12 v2 has mandatory admission and completion. The old EclSubmit,
// EclDrain and EclFence registry switches are retired; an exact packet cannot
// be disabled or replaced with a sampled prefix. Their inventory positions
// remain below for existing log parsers, followed by ExecutionSyncVersion.

/// The `pfnSignalFence` diagnostic delay in microseconds, clamped. `0` = off.
/// See [`UMD12_FENCE_SIGNAL_DELAY_US`].
pub(crate) fn umd12_fence_signal_delay_us() -> u32 {
    UMD12_FENCE_SIGNAL_DELAY_US
        .get()
        .min(MAX_DIAGNOSTIC_DELAY_US)
}

/// The `pfnExecuteCommandLists` diagnostic delay in microseconds, clamped.
/// `0` = off. See [`UMD12_ECL_DELAY_US`].
pub(crate) fn umd12_ecl_delay_us() -> u32 {
    UMD12_ECL_DELAY_US.get().min(MAX_DIAGNOSTIC_DELAY_US)
}

/// Emit this crate's knob inventory through the shared reader, once per process.
///
/// The thin wrapper D3b's split implies: the READER is shared
/// (`helios_umd_common::log::log_knob_inventory`) because the emitted
/// `UMD knob: name=value` line is the evidence contract that
/// `tools/capture-knob-inventory.ps1` parses, while the SET is per-crate. The
/// module-path line logged just before it is what attributes these values to
/// *this* DLL when two Helios UMDs are loaded in one process.
pub(crate) fn log_knob_inventory() {
    helios_umd_common::log::log_knob_inventory(&resolved_inventory());
}

/// The knob inventory, so the set is enumerable instead of grep-discoverable.
///
/// Resolving forces every `OnceLock`, which is why this is not on any hot path:
/// it exists for the one-shot dump at driver init, and for anyone asking "what
/// knobs does the D3D12 driver have".
///
/// ⚠ **New knobs are APPENDED, never inserted.** The emitted `UMD knob:` lines
/// are the evidence contract `tools/capture-knob-inventory.ps1` parses and that
/// S2 proved the crate split byte-identical against; reordering makes two
/// captures differ for a reason that is not a behaviour change.
pub(crate) fn resolved_inventory() -> [(&'static str, u32); 19] {
    [
        ("Umd12Trace", UMD12_TRACE.get() as u32),
        ("UmdD3D12", UMD_D3D12.get() as u32),
        ("Umd12FormatCaps", UMD12_FORMAT_CAPS.get()),
        // ⚠ The two delay arms report their **clamped** value, through the same
        // accessor the DDI reads, not the raw DWORD. The inventory line is read
        // as "what will this driver do", and a mistyped 50000000 that the read
        // site silently caps at 2 000 000 would otherwise be captured as a
        // configuration the run never had.
        ("Umd12FenceSignalDelayUs", umd12_fence_signal_delay_us()),
        ("Umd12EclDelayUs", umd12_ecl_delay_us()),
        // Retired registry switches: report the fixed behavior while preserving
        // inventory order. ExecutionSyncVersion distinguishes v2 from old arms.
        ("Umd12EclSubmit", 1),
        ("Umd12EclFence", 1),
        ("Umd12EclDrain", 0),
        ("ExecutionSyncVersion", 2),
        // S5 (NVK on RM). Appended.
        ("Nvk12EclSync", nvk12_ecl_sync()),
        ("Nvk12Present", nvk12_present_mode()),
        // Appended.
        ("Umd12PipelineLibrary", UMD12_PIPELINE_LIBRARY.get() as u32),
        // Appended. Clamped, as read by the ECL CPU wait.
        ("Nvk12EclSpinUs", nvk12_ecl_spin_us()),
        // Appended.
        ("Nvk12ScanoutFence", nvk12_scanout_fence() as u32),
        // Appended. Clamped, as the worker reads it.
        ("Nvk12WorkerSpinUs", nvk12_worker_spin_us()),
        // Appended (diagnostic).
        ("Nvk12AdmitAfterPresentOnly", nvk12_admit_after_present_only() as u32),
        // Appended.
        ("Nvk12EclFencePrefetch", nvk12_ecl_fence_prefetch() as u32),
        // Appended.
        ("Umd12ContextNode", umd12_context_node() as u32),
        // Appended (probe).
        ("Umd12FenceVaProbe", umd12_fence_va_probe() as u32),
    ]
}

// --- NVK on RM (dxvk-on-nvk S5) ----------------------------------------------
//
// Which ICD the D3D12 engine runs on is decided in the bridge
// (`umd_common/bridge/bridge_icd_backend.h`: `Icd`, `NvkDenyList`,
// `NvkAllowList`, `NvkIcdPath`, and for D3D12 only `Nvk12` / `NvkDenyList12` /
// `HELIOS_ICD12`). These two shape how an NVK device orders and presents.

/// `Nvk12EclSync`: how the runtime's context is ordered behind NVK work, which
/// the KMD cannot see (no Venus stream; the RM-fence boundary is S4).
///
/// | value | meaning |
/// |---:|---|
/// | 0 | a UMD monitored fence: each ECL makes the context wait for its value, a per-queue worker signals it from the CPU when the engine's execution stream reaches it. Nothing blocks the app thread. Falls back to 1 if the fence cannot be created |
/// | 1 | CPU wait: ExecuteCommandLists returns only after its work completed (2 s cap per call, then it proceeds and counts a timeout) |
/// | 2 | ECL fence: each ECL submits an `HE12` v4 record naming an RM fence the engine signals after the batch; the KMD withholds the packet's DMA completion until it fires. Nothing waits and batches pipeline. Needs NVK helios_icd_interface v7 and a KMD that takes RM fences in `HE12` v4 (`HELIOS_ICD_CAP_PRESENT_FENCE_KMD`); without them, and for a batch whose fence could not be made or whose Render is refused, it acts as 0. The default |
///
/// Default 0 since 2026-10-08. 1 was the default from 2026-10-06, when 0
/// deadlocked Basemark GPU DX12 after its first frame: the worker coalesced
/// handed values and waited for N+1, whose admission sat on the context behind
/// the wait for N (fixed the same day, c5ee89b7: values are waited for and
/// signalled in order). Measured on 393.1 (Basemark DX12, 1920x1080 windowed,
/// 3 loops): 1 = median frame 36.7 ms, CPUBusy 36.6 ms (20 ECLs a frame, each
/// a ~0.85 ms CPU wait); 0 = median 22.65 ms, CPUBusy 17.9 ms, no deadlock.
///
/// Default 2 since 2026-10-08 (399.1, same scene, 876 frames): 0 = frame
/// 24.7 ms with 4.6 ms in Present (the scanout present's CPU wait); 2 = 21.4 ms
/// with 67 us in Present (fenced scanout present). Mode 2 ran clean on 394.1,
/// 395.1, 396.1 and 399.1 (no fallback batches, RmGErr 0). Its fallback is
/// mode 0, so a device without ECL fences behaves as before.
pub(crate) static NVK12_ECL_SYNC: DwordKnob = DwordKnob::new(c"Nvk12EclSync", 2);

/// `Nvk12Present` (or `HELIOS_NVK_PRESENT` in the process environment): 0 =
/// automatic (DWM composes the back buffer from its NVK resource id when the
/// Venus side imports foreign surfaces, `ForeignImport=1`, else scanout 0),
/// 1 = always scanout 0 (zero-copy flip; the desktop is hidden while the app
/// presents), 2 = always the WDDM present (DWM composes).
pub(crate) static NVK12_PRESENT: DwordKnob = DwordKnob::new(c"Nvk12Present", 0);

/// `Nvk12EclSpinUs`: how long, in microseconds, the CPU-wait arm of
/// [`NVK12_ECL_SYNC`] polls the engine's execution stream on the calling
/// thread before it falls back to the blocking engine wait. `0` = no polling
/// (block at once, the behaviour before this knob). Default 2000, clamped to
/// [`MAX_ECL_SPIN_US`].
///
/// Why: the blocking wait is NVK's `vkWaitSemaphores`, whose RM backend
/// yields 64 times and then blocks on the non-stall event or `Sleep()`s;
/// on Windows `os_time_sleep(10 us)` is `Sleep(1)`, i.e. a whole timer tick.
/// Basemark GPU DX12 submits ~21 ECLs per frame, each waited here, so a tick
/// per ECL alone is the measured ~36 ms of CPU per frame. Polling with a zero
/// timeout costs a semaphore read per probe and returns as soon as the GPU
/// is done (the whole frame's GPU time is ~1 ms on this scene).
/// Read once per process.
pub(crate) static NVK12_ECL_SPIN_US: DwordKnob = DwordKnob::new(c"Nvk12EclSpinUs", 2000);

/// Upper bound of [`NVK12_ECL_SPIN_US`]: a mistyped value must not turn every
/// ECL into a long busy loop.
const MAX_ECL_SPIN_US: u32 = 50_000;

/// The ECL CPU-wait poll budget in microseconds, clamped. See [`NVK12_ECL_SPIN_US`].
pub(crate) fn nvk12_ecl_spin_us() -> u32 {
    NVK12_ECL_SPIN_US.get().min(MAX_ECL_SPIN_US)
}

/// `Nvk12ScanoutFence`: with `Nvk12EclSync=2`, a scanout present (scanout 0,
/// `Nvk12Present`) hands the KMD (or a thread in NVK) a flip that waits for the
/// frame's last ECL fence (`scanout_present_fenced`, helios_icd_interface v3)
/// instead of waiting on the CPU for the queue before flipping. Absent = ON;
/// 0 = the CPU wait, as before. Without an ECL fence value on the queue, or if
/// the flip cannot be queued, the present waits on the CPU (counted).
///
/// ⚠ The flip now happens after Present returns, so the KMD may still show
/// image P when DXGI hands it back for rendering (a torn frame at worst with
/// two buffers; the scanout queue rule is three or more images).
/// Read once per process.
pub(crate) static NVK12_SCANOUT_FENCE: BoolKnob = BoolKnob::new(c"Nvk12ScanoutFence", true);

pub(crate) fn nvk12_scanout_fence() -> bool {
    NVK12_SCANOUT_FENCE.get()
}

/// `Nvk12WorkerSpinUs`: with `Nvk12EclSync=0`, how long the per-queue worker
/// polls the engine's execution stream for a handed boundary before blocking
/// (NVK's blocking wait sees completion only after the RM non-stall event is
/// relayed from the host). The boundary it signals gates the admission of the
/// queue's next batch, so this sits on the critical path once per
/// ExecuteCommandLists. Default 0 (block at once, as before) until measured;
/// clamped to [`MAX_ECL_SPIN_US`]. Costs up to that much of one core per
/// boundary while polling. Read once per process.
pub(crate) static NVK12_WORKER_SPIN_US: DwordKnob = DwordKnob::new(c"Nvk12WorkerSpinUs", 0);

pub(crate) fn nvk12_worker_spin_us() -> u32 {
    NVK12_WORKER_SPIN_US.get().min(MAX_ECL_SPIN_US)
}

/// ⛔ DIAGNOSTIC, never a default. `Nvk12AdmitAfterPresentOnly=1`: on NVK,
/// only the first ExecuteCommandLists of each queue after a Present (or after
/// queue creation) waits for the runtime admission event; every other batch
/// is released to the engine at once. The context is still ordered behind
/// every batch (the app's fence signals stay correct), but a batch no longer
/// waits for runtime waits queued before it -- a cross-queue Wait or a
/// flip-model buffer wait that is not right after a Present -- so an app that
/// relies on those can race. What it measures: how much of the frame is the
/// per-batch admission round trip (commit-to-admission ~1-2 ms on 398.1).
/// Counter EclAdmissionSkipped. Read once per process.
pub(crate) static NVK12_ADMIT_AFTER_PRESENT_ONLY: BoolKnob =
    BoolKnob::new(c"Nvk12AdmitAfterPresentOnly", false);

pub(crate) fn nvk12_admit_after_present_only() -> bool {
    NVK12_ADMIT_AFTER_PRESENT_ONLY.get()
}

/// `Nvk12EclFencePrefetch` (`Nvk12EclSync=2`): after an ECL is admitted, make
/// the queue's next ECL fence right away (vkd3d patch 0009,
/// helios_vkd3d_prepare_ecl_fence), so the next ECL's HE12 v4 Render does not
/// wait for the fence-create escape, a host round trip. Absent = ON; 0 = make
/// each fence inline, as before. Read once per process.
pub(crate) static NVK12_ECL_FENCE_PREFETCH: BoolKnob = BoolKnob::new(c"Nvk12EclFencePrefetch", true);

pub(crate) fn nvk12_ecl_fence_prefetch() -> bool {
    NVK12_ECL_FENCE_PREFETCH.get()
}

/// `Umd12ContextNode` (default 0): on NVK, D3D12 queue contexts ask for WDDM
/// node 1, which the KMD reports with `D3d12Node=1` and completes independently
/// of node 0 (DWM, D3D11, presents): a D3D12 batch held for its RM fence no
/// longer waits behind a present, nor the desktop behind it. A refusal (one
/// node) falls back to node 0, counted (QueueContextNodeRefused). Read once per
/// process.
pub(crate) static UMD12_CONTEXT_NODE: BoolKnob = BoolKnob::new(c"Umd12ContextNode", false);

pub(crate) fn umd12_context_node() -> bool {
    UMD12_CONTEXT_NODE.get()
}

/// `Umd12FenceVaProbe` (default 0; probe, no behaviour change): at device
/// creation, create one monitored fence through the runtime's kernel callbacks
/// and log the GPU virtual address dxgkrnl gives it (`nvk12::
/// probe_monitored_fence_va`). Whether that is nonzero decides the first step of
/// driver-backed fences (pfnSignalFence / pfnWaitForFence). Read once per
/// process.
pub(crate) static UMD12_FENCE_VA_PROBE: BoolKnob = BoolKnob::new(c"Umd12FenceVaProbe", false);

pub(crate) fn umd12_fence_va_probe() -> bool {
    UMD12_FENCE_VA_PROBE.get()
}

pub(crate) fn nvk12_ecl_sync() -> u32 {
    NVK12_ECL_SYNC.get().min(2)
}

pub(crate) fn nvk12_present_mode() -> u32 {
    static CELL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::var("HELIOS_NVK_PRESENT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| NVK12_PRESENT.get())
            .min(2)
    })
}

/// `ForeignImport` (shared with the D3D11 driver's Venus side): DWM imports
/// NVK-made surfaces, so a windowed NVK app can be composed.
pub(crate) fn foreign_import() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        helios_umd_common::knobs::reg_dword(c"ForeignImport").is_some_and(|v| v != 0)
    })
}

/// `Umd12PipelineLibrary`: the six pipeline-library DDI slots forward to the
/// engine's `ID3D12PipelineLibrary1` (`forward12::pso`). Absent = ON; explicit
/// 0 = every slot refuses with `E_NOTIMPL`, the behaviour before the
/// passthrough, which applications answer by creating PSOs directly.
///
/// ⚠ Read once per process.
pub(crate) static UMD12_PIPELINE_LIBRARY: BoolKnob = BoolKnob::new(c"Umd12PipelineLibrary", true);

pub(crate) fn umd12_pipeline_library() -> bool {
    UMD12_PIPELINE_LIBRARY.get()
}

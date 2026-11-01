//! WDDM hardware GPU scheduling (HWS) for the NVIDIA personality (`NvUmd` != 0).
//!
//! NVIDIA's UMDs (D3D11 and CUDA) create their contexts with
//! `D3DKMTCreateContextVirtual(HwQueueSupported | DisableGpuTimeout)` and then
//! `D3DKMTCreateHwQueue` on that context, which dxgkrnl accepts only when the
//! adapter has HWS enabled. NVIDIA's own 610.88 KMD gets there by asking
//! `DxgkCbQueryFeatureSupport(DXGK_FEATURE_HWSCH, STABLE)` at StartDevice and by
//! registering the WDDM 2.7 context-based scheduling DDIs (`CreateContext` +
//! `CreateHwQueue`, `SuspendContext`, `SetSchedulingLogBuffer`, ...), not the
//! `CreateHwContext` family.
//!
//! Everything here is argument-only so the arithmetic is host-tested; the DDI
//! glue lives in `kmd_render/src/ddi/hws.rs`.

/// `DXGK_FEATURE_HWSCH` (`d3dukmdt.h`, `DXGK_FEATURE_ID` 0).
pub const FEATURE_HWSCH: i32 = 0;
/// `DXGK_FEATURE_SUPPORT_STABLE`: the support state NVIDIA's KMD reports.
pub const FEATURE_SUPPORT_STABLE: u32 = 2;

/// `DXGK_CREATECONTEXTFLAGS.VirtualAddressing` (bit 2).
pub const CONTEXT_FLAG_VIRTUAL_ADDRESSING: u32 = 1 << 2;
/// `DXGK_CREATECONTEXTFLAGS.HwQueueSupported` (bit 4).
pub const CONTEXT_FLAG_HW_QUEUE_SUPPORTED: u32 = 1 << 4;

/// The `DXGK_CONTEXTINFO` NVIDIA's 610.88 KMD returns from `DxgkDdiCreateContext`
/// for an NVDA-private HW-queue context (captured on an RTX 5090).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvdaContextInfo {
    pub dma_buffer_size: u32,
    pub dma_buffer_segment_set: u32,
    pub dma_buffer_private_data_size: u32,
    /// `DXGK_CONTEXTINFO_CAPS.Value`: `NoPatchingRequired | DriverManagesResidency`.
    pub caps: u32,
}

pub const NVDA_CONTEXT_INFO: NvdaContextInfo = NvdaContextInfo {
    dma_buffer_size: 12_288,
    dma_buffer_segment_set: 0x1,
    dma_buffer_private_data_size: 4_512,
    caps: 0x3,
};

/// Whether the KMD-side `DXGK_CREATECONTEXTFLAGS` of a CreateContext describe a
/// HW-queue context in a GPU-VA process (both bits set; others are allowed).
pub const fn is_hw_queue_context(flags: u32) -> bool {
    let want = CONTEXT_FLAG_VIRTUAL_ADDRESSING | CONTEXT_FLAG_HW_QUEUE_SUPPORTED;
    flags & want == want
}

/// Monitored-fence progress of one HW queue, as the KMD sees it.
///
/// `submitted` is the highest `HwQueueProgressFenceId` handed to
/// `SubmitCommandToHwQueue`; `signalled` is the highest value read back from the
/// progress-fence storage (which the UMD's DMA buffer writes on completion) that
/// dxgkrnl has been told about with `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED`.
/// Fence values are 64-bit and monotonic per queue, so plain comparisons hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub submitted: u64,
    pub signalled: u64,
}

impl Progress {
    /// A queue whose storage currently reads `initial`: nothing is owed for it.
    pub const fn starting_at(initial: u64) -> Self {
        Self {
            submitted: initial,
            signalled: initial,
        }
    }

    /// Record a submission. A smaller id than one already seen is ignored (the
    /// fence never goes backwards).
    pub fn note_submit(&mut self, fence_id: u64) {
        if fence_id > self.submitted {
            self.submitted = fence_id;
        }
    }

    /// Feed one read of the storage. `true` = the value moved past what dxgkrnl
    /// was last told, so a `MONITORED_FENCE_SIGNALED` is owed for this node.
    pub fn observe(&mut self, value: u64) -> bool {
        if value > self.signalled {
            self.signalled = value;
            if value > self.submitted {
                // A value the KMD never saw submitted (a kernel-side signal, or a
                // submit racing this read): progress is progress.
                self.submitted = value;
            }
            true
        } else {
            false
        }
    }

    /// Work handed to the GPU whose completion has not been observed yet.
    pub const fn outstanding(&self) -> bool {
        self.submitted > self.signalled
    }
}

/// The interim progress poll's period, in 100 ns units: 1 ms while it is
/// recently productive.
pub const POLL_PERIOD_100NS: i64 = 10_000;
/// The period once [`POLL_SLOW_AFTER`] consecutive ticks saw outstanding work
/// but no progress (a long dispatch, or a UMD that never writes the fence):
/// 10 ms, so a stuck queue cannot keep a CPU busy at 1 kHz forever.
pub const POLL_SLOW_PERIOD_100NS: i64 = 100_000;
/// One second of fruitless 1 ms ticks.
pub const POLL_SLOW_AFTER: u32 = 1_000;

/// What the poll does after a tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollNext {
    /// Nothing outstanding anywhere: do not re-arm (the next submit arms it).
    Stop,
    /// Re-arm with this relative (negative) due time in 100 ns units.
    Rearm(i64),
}

/// The decision after one tick. `idle_ticks` counts consecutive ticks that
/// found outstanding work and no progress (reset by any progress).
pub const fn poll_next(any_outstanding: bool, idle_ticks: u32) -> PollNext {
    if !any_outstanding {
        PollNext::Stop
    } else if idle_ticks >= POLL_SLOW_AFTER {
        PollNext::Rearm(-POLL_SLOW_PERIOD_100NS)
    } else {
        PollNext::Rearm(-POLL_PERIOD_100NS)
    }
}

/// `DXGKARG_SETSCHEDULINGLOGBUFFER` sanity: a nonzero entry count needs a CPU
/// view to write through. (Zero entries with no buffer is accepted as "no log".)
pub const fn log_buffer_ok(number_of_entries: u32, cpu_va: u64) -> bool {
    number_of_entries == 0 || cpu_va != 0
}

/// A `DXGKARG_SUSPENDCONTEXT.contextSuspendFence` is monotonically increasing
/// per context; `true` when `new` is newer than the last one recorded.
pub const fn suspend_fence_newer(last: u64, new: u64) -> bool {
    new > last
}

/// The `Hws*` registry counter names `kmd_render/src/ddi/hws.rs` writes.
pub const COUNTERS: &[&str] = &[
    "NvHwsEn",
    "NvHwsQSt",
    "HwsCtxNv",
    "HwsCtxNvFail",
    "HwsCtxBad",
    "HwsQueue",
    "HwsQueueFail",
    "HwsSubmit",
    "HwsSubmitFail",
    "HwsMfSig",
    "HwsPollTick",
    "HwsSuspend",
    "HwsResume",
    "HwsSchedProp",
    "HwsPrioChg",
    "HwsBands",
    "HwsLogBuf",
    "HwsEngReset",
    "HwsEngResume",
    "HwsSmf",
    "HwsSmfKind",
    "HwsQFull",
];

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    #[test]
    fn captured_context_info_and_flags() {
        assert_eq!(NVDA_CONTEXT_INFO.dma_buffer_size, 0x3000);
        assert_eq!(NVDA_CONTEXT_INFO.dma_buffer_private_data_size, 0x11a0);
        assert_eq!(NVDA_CONTEXT_INFO.caps, 0x3);
        // The UMD's D3DDDI 0x14 lands as KMD VirtualAddressing|HwQueueSupported.
        assert!(is_hw_queue_context(0x14));
        assert!(is_hw_queue_context(0x15));
        assert!(!is_hw_queue_context(0x10));
        assert!(!is_hw_queue_context(0x04));
        assert!(!is_hw_queue_context(0));
    }

    #[test]
    fn progress_signals_only_forward_moves() {
        let mut p = Progress::starting_at(5);
        assert!(!p.outstanding());
        assert!(!p.observe(5));
        p.note_submit(6);
        p.note_submit(7);
        p.note_submit(3); // backwards id ignored
        assert_eq!(p.submitted, 7);
        assert!(p.outstanding());
        assert!(p.observe(6));
        assert!(p.outstanding());
        assert!(!p.observe(6));
        assert!(!p.observe(4)); // a stale read never re-signals
        assert!(p.observe(7));
        assert!(!p.outstanding());
        // A value beyond anything submitted still counts as progress.
        assert!(p.observe(9));
        assert_eq!(p.submitted, 9);
        assert!(!p.outstanding());
    }

    #[test]
    fn poll_stops_when_idle_and_slows_when_stuck() {
        assert_eq!(poll_next(false, 0), PollNext::Stop);
        assert_eq!(poll_next(false, 5_000), PollNext::Stop);
        assert_eq!(poll_next(true, 0), PollNext::Rearm(-10_000));
        assert_eq!(poll_next(true, POLL_SLOW_AFTER - 1), PollNext::Rearm(-10_000));
        assert_eq!(poll_next(true, POLL_SLOW_AFTER), PollNext::Rearm(-100_000));
        // Never faster than 1 ms.
        for idle in [0, 1, 999, 1000, u32::MAX] {
            if let PollNext::Rearm(due) = poll_next(true, idle) {
                assert!(due <= -POLL_PERIOD_100NS);
            }
        }
    }

    #[test]
    fn log_buffer_and_suspend_fence_rules() {
        assert!(log_buffer_ok(0, 0));
        assert!(log_buffer_ok(64, 0x1000));
        assert!(!log_buffer_ok(64, 0));
        assert!(suspend_fence_newer(0, 1));
        assert!(!suspend_fence_newer(4, 4));
        assert!(!suspend_fence_newer(4, 3));
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
    }

    /// Every listed name is written by `kmd_render`, and `ddi/hws.rs` writes no
    /// `Hws*` name that is not listed.
    #[test]
    fn counter_names_match_kmd_render() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kmd_render/src/ddi/hws.rs");
        let Ok(text) = std::fs::read_to_string(&src) else {
            assert!(
                std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
                "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
                src.display()
            );
            return;
        };
        for n in COUNTERS {
            assert!(text.contains(&std::format!("b\"{n}\"")), "{n} is not written");
        }
        for piece in text.split("record_named_bytes(b\"").skip(1) {
            let name = piece.split('"').next().unwrap_or("");
            assert!(COUNTERS.contains(&name), "{name} is not in COUNTERS");
        }
    }
}

//! `DxgkDdiSuspendContext` / `DxgkDdiResumeContext` for native TSGs (`NvSuspend`, REG_DWORD,
//! default 1): the pure half. The call sites are `dxgkddi_suspend_context` /
//! `dxgkddi_resume_context` in `kmd_render/src/ddi/hws.rs` and `NativeContext::transition` in
//! `kmd_render/src/virtio/native_channel.rs`.
//!
//! # What WDDM requires (windows-driver-docs-ddi `nc-d3dkmddi-dxgkddi_suspendcontext.md`,
//! `ns-d3dkmddi-_dxgkarg_suspendcontext.md`, `user-mode-work-submission.md`, d3dkmddi.h)
//!
//! * Both DDIs are `_IRQL_requires_(PASSIVE_LEVEL)`.
//! * SuspendContext "instructs the GPU to suspend a context ... no grace period, expected to be
//!   honored as soon as possible". It returns STATUS_SUCCESS "if the context is already
//!   suspended at the time of this call", otherwise STATUS_PENDING, and the suspend finishes
//!   when `contextSuspendFence` is reported with `DXGK_INTERRUPT_SUSPEND_CONTEXT_COMPLETED`
//!   (`SuspendContextCompleted { hContext, ContextSuspendFence }`). "Until the fence is
//!   signaled, the OS cannot assume GPU is suspended." No acknowledgement within the TDR
//!   timeout = engine timeout and reset.
//! * "Once a context is suspended, it is assumed that all references to it are gone from the
//!   GPU, and the OS is free to destroy the context or move its memory around." In WDDM 2.4+
//!   HWS mode there is no NULL context switch: SuspendContext is what VidMm relies on before
//!   it evicts, relocates or unmaps allocations a context may use.
//! * Suspend removes all HW queues of the context from the HW scheduler; the doorbells stay
//!   connected and the ring buffers resident, the UMD may keep writing and ringing, the GPU
//!   just does not run it. ResumeContext puts the queues back; a context is created resumed.
//!
//! # What this KMD does
//!
//! The suspend is done synchronously at PASSIVE: the TSG comes off the host runlist
//! (`NVA06C_CTRL_CMD_GPFIFO_SCHEDULE` false: every channel of the TSG, VEID 0 and the
//! compute-only VEIDs alike, since the TSG is the runlist entry) and is preempted with a wait
//! (`NVA06C_CTRL_CMD_PREEMPT { bWait }`: RM disables the TSG, preempts PBDMA and engines and
//! waits for both). Only when both returned is STATUS_SUCCESS reported, which the contract
//! defines as "suspended at the time of this call", so no completion interrupt is owed and
//! VidMm's paging only proceeds once the GPU is off the context. Resume puts the TSG back on
//! the runlist (and rings each queue's doorbell again, for work the UMD queued meanwhile).
//!
//! [`Sched`] is the per-TSG state machine every scheduling change goes through (first
//! channel, suspend, resume, recovery, destroy), so a queue created while its context is
//! suspended does not put the TSG on the runlist, and a recovered TSG is never put back.

/// `NVA06C_CTRL_CMD_PREEMPT` (on the TSG): `{ NvBool bWait; NvBool bManualTimeout; NvU32
/// timeoutUs }`.
pub const CTRL_PREEMPT: u32 = 0xa06c_0105;
/// `sizeof(NVA06C_CTRL_PREEMPT_PARAMS)`: two NvBool, two pad bytes, a u32.
pub const PREEMPT_BYTES: usize = 8;
/// `NVA06C_CTRL_CMD_PREEMPT_MAX_MANUAL_TIMEOUT_US`.
pub const MAX_PREEMPT_TIMEOUT_US: u32 = 1_000_000;

/// `NvSuspend` default: real suspend.
pub const DEFAULT_SUSPEND: u32 = 1;

/// The preempt's parameters: wait for it; `timeout_us` 0 = RM's own timeout, else a manual
/// one clamped to RM's maximum.
pub fn preempt_params(timeout_us: u32) -> [u8; PREEMPT_BYTES] {
    let mut p = [0u8; PREEMPT_BYTES];
    p[0] = 1;
    if timeout_us != 0 {
        p[1] = 1;
        p[4..8].copy_from_slice(&timeout_us.min(MAX_PREEMPT_TIMEOUT_US).to_le_bytes());
    }
    p
}

/// What `NvSuspend` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0: report "already suspended" without touching the TSG (the pre-`NvSuspend` stand-in).
    Report,
    /// 1 (default, and any other value): unschedule + preempt, resume reschedules.
    Real,
}

pub const fn mode(knob: u32) -> Mode {
    if knob == 0 {
        Mode::Report
    } else {
        Mode::Real
    }
}

/// The RM call a [`Sched`] change needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    None,
    /// `GPFIFO_SCHEDULE` true.
    Schedule,
    /// `GPFIFO_SCHEDULE` false (+ the preempt, for a suspend).
    Unschedule,
}

const ON_RUNLIST: u32 = 1 << 0;
const WANTED: u32 = 1 << 1;
const SUSPENDED: u32 = 1 << 2;
const DEAD: u32 = 1 << 3;

/// One TSG's scheduling state, packed into a u32 (the KMD keeps it in an atomic and changes it
/// under the context's lock, together with the RM call the change asks for).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sched {
    /// RM has the TSG on its runlist (the last `GPFIFO_SCHEDULE` that succeeded said so).
    pub on_runlist: bool,
    /// The TSG has a channel and should run (set by its first channel).
    pub wanted: bool,
    /// dxgkrnl suspended the context and has not resumed it.
    pub suspended: bool,
    /// Recovered or being destroyed: never on the runlist again.
    pub dead: bool,
}

impl Sched {
    pub const fn pack(self) -> u32 {
        (if self.on_runlist { ON_RUNLIST } else { 0 })
            | (if self.wanted { WANTED } else { 0 })
            | (if self.suspended { SUSPENDED } else { 0 })
            | (if self.dead { DEAD } else { 0 })
    }

    pub const fn unpack(v: u32) -> Self {
        Self {
            on_runlist: v & ON_RUNLIST != 0,
            wanted: v & WANTED != 0,
            suspended: v & SUSPENDED != 0,
            dead: v & DEAD != 0,
        }
    }

    /// Whether the TSG should be on the runlist now.
    pub const fn should_run(self) -> bool {
        self.wanted && !self.suspended && !self.dead
    }

    /// The RM call that brings the runlist in line with [`Self::should_run`].
    pub const fn step(self) -> Step {
        match (self.should_run(), self.on_runlist) {
            (true, false) => Step::Schedule,
            (false, true) => Step::Unschedule,
            _ => Step::None,
        }
    }

    /// A channel of the TSG is complete (preemption modes set): it may run.
    pub fn channel_ready(&mut self) -> Step {
        self.wanted = true;
        self.step()
    }

    /// `DxgkDdiSuspendContext`.
    pub fn suspend(&mut self) -> Step {
        self.suspended = true;
        self.step()
    }

    /// `DxgkDdiResumeContext`.
    pub fn resume(&mut self) -> Step {
        self.suspended = false;
        self.step()
    }

    /// Recovery (`recover_native`) or DestroyContext: off for good.
    pub fn kill(&mut self) -> Step {
        self.dead = true;
        self.step()
    }

    /// The RM call for `step` returned `ok`. A refused Schedule leaves the TSG off; a refused
    /// Unschedule leaves it counted on (a later kill or destroy tries again), except when the
    /// TSG is dead, where nothing will try again anyway.
    pub fn done(&mut self, step: Step, ok: bool) {
        match step {
            Step::None => {}
            Step::Schedule => self.on_runlist = ok,
            Step::Unschedule => self.on_runlist = !ok && !self.dead,
        }
    }
}

/// Suspend statistics (`HwsSusN`, `HwsSusAvgUs`, `HwsSusMaxUs`, ...): the time from the
/// DDI's entry to the TSG confirmed off the engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SuspendStats {
    /// Suspends that preempted a TSG (timed).
    pub preempts: u32,
    pub sum_us: u64,
    pub max_us: u32,
}

impl SuspendStats {
    pub const fn new() -> Self {
        Self {
            preempts: 0,
            sum_us: 0,
            max_us: 0,
        }
    }

    pub fn add(&mut self, d_100ns: u64) {
        let us = d_100ns / 10;
        self.preempts = self.preempts.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(u32::try_from(us).unwrap_or(u32::MAX));
    }

    pub fn mean_us(&self) -> u32 {
        if self.preempts == 0 {
            0
        } else {
            u32::try_from(self.sum_us / u64::from(self.preempts)).unwrap_or(u32::MAX)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut Sched, step: Step) {
        s.done(step, true);
    }

    #[test]
    fn preempt_params_layout() {
        assert_eq!(preempt_params(0), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(preempt_params(100_000), [1, 1, 0, 0, 0xa0, 0x86, 0x01, 0]);
        assert_eq!(
            preempt_params(5_000_000)[4..8],
            MAX_PREEMPT_TIMEOUT_US.to_le_bytes()
        );
        assert_eq!(CTRL_PREEMPT, 0xa06c_0105);
    }

    #[test]
    fn knob() {
        assert_eq!(mode(0), Mode::Report);
        assert_eq!(mode(1), Mode::Real);
        assert_eq!(mode(DEFAULT_SUSPEND), Mode::Real);
        assert_eq!(mode(7), Mode::Real);
    }

    #[test]
    fn pack_roundtrip() {
        for v in 0..16 {
            assert_eq!(Sched::unpack(v).pack(), v);
        }
        assert_eq!(Sched::unpack(0), Sched::default());
    }

    #[test]
    fn first_channel_schedules_once() {
        let mut s = Sched::default();
        let step = s.channel_ready();
        assert_eq!(step, Step::Schedule);
        run(&mut s, step);
        assert!(s.on_runlist);
        // A second channel (a VEID compute channel) of the same TSG: nothing.
        assert_eq!(s.channel_ready(), Step::None);
    }

    #[test]
    fn suspend_resume_cycle() {
        let mut s = Sched::default();
        let step = s.channel_ready();
        run(&mut s, step);
        let step = s.suspend();
        assert_eq!(step, Step::Unschedule);
        run(&mut s, step);
        assert!(!s.on_runlist && s.suspended);
        // A repeated suspend (newer fence, no resume between): nothing more to do.
        assert_eq!(s.suspend(), Step::None);
        let step = s.resume();
        assert_eq!(step, Step::Schedule);
        run(&mut s, step);
        assert!(s.on_runlist && !s.suspended);
        // A resume without a suspend: nothing.
        assert_eq!(s.resume(), Step::None);
    }

    #[test]
    fn suspend_without_channel_is_immediate() {
        let mut s = Sched::default();
        assert_eq!(s.suspend(), Step::None);
        assert!(s.suspended);
        assert_eq!(s.resume(), Step::None);
        assert!(!s.on_runlist);
    }

    #[test]
    fn channel_made_while_suspended_waits_for_resume() {
        let mut s = Sched::default();
        assert_eq!(s.suspend(), Step::None);
        assert_eq!(s.channel_ready(), Step::None);
        assert!(!s.on_runlist);
        let step = s.resume();
        assert_eq!(step, Step::Schedule);
        run(&mut s, step);
        assert!(s.on_runlist);
    }

    #[test]
    fn dead_tsg_never_returns() {
        let mut s = Sched::default();
        let step = s.channel_ready();
        run(&mut s, step);
        let step = s.kill();
        assert_eq!(step, Step::Unschedule);
        // Recovery's unschedule refused: still counted off (nothing retries a dead TSG).
        s.done(step, false);
        assert!(!s.on_runlist);
        assert_eq!(s.resume(), Step::None);
        assert_eq!(s.channel_ready(), Step::None);
        assert_eq!(s.suspend(), Step::None);
    }

    #[test]
    fn suspended_then_killed_sends_nothing() {
        let mut s = Sched::default();
        let step = s.channel_ready();
        run(&mut s, step);
        let step = s.suspend();
        run(&mut s, step);
        assert_eq!(s.kill(), Step::None);
        assert_eq!(s.resume(), Step::None);
    }

    #[test]
    fn refused_steps() {
        let mut s = Sched::default();
        let step = s.channel_ready();
        s.done(step, false);
        assert!(!s.on_runlist);
        // The next transition that wants it on tries again.
        assert_eq!(s.suspend(), Step::None);
        assert_eq!(s.resume(), Step::Schedule);
        s.done(Step::Schedule, true);
        // A refused suspend unschedule leaves it on: resume sends nothing, kill retries.
        let step = s.suspend();
        s.done(step, false);
        assert!(s.on_runlist);
        assert_eq!(s.resume(), Step::None);
        assert_eq!(s.kill(), Step::Unschedule);
    }

    #[test]
    fn stats() {
        let mut st = SuspendStats::new();
        assert_eq!(st.mean_us(), 0);
        st.add(1_000); // 100 us
        st.add(3_000); // 300 us
        assert_eq!(st.preempts, 2);
        assert_eq!(st.mean_us(), 200);
        assert_eq!(st.max_us, 300);
    }
}

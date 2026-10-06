//! RM gates: the KMD side of carrier (b) of `docs/rm-fence-marker.md`. A WDDM present
//! (`HERF` / `HEPR`) or an `ExecuteCommandLists` batch (`HE12` v4) names an RM fence
//! as its completion boundary; the boundary lives in the existing tagged stream
//! namespace, so every consumer of boundaries (WDDM FIFO head, deferred fast bind,
//! windowed blit, execution waits, dead-stream discharge) works unchanged.
//!
//! One gate per process (`hKmdProcess`). A gate is a slot of the present-stream table
//! with no Venus context (`ctx_id == 0`, `cookie == 0`: nothing that finds streams by
//! context or cookie can name it, and its owner is [`DeviceOwner::KMD_RM`], which no
//! escape can present), whose retired value is advanced by [`Gate::fire`] instead of
//! by used-ring responses. The point rules (prefix retirement, early fire, bounds) are
//! `helios_kmd_logic::rm_fence_present::Gate`, host-tested.
//!
//! Everything here runs under `virtio_lock` at `DISPATCH_LEVEL`: fixed storage reserved
//! at init, no allocation, no wait, no PASSIVE-only call. The host `Close` of a fence
//! that fired is the HPD worker's (`adapter/foreign_scanout.rs::foreign_fence_service`).

use super::nvrm_tables::{FenceClaim, FenceRefusal};
use super::*;
use helios_kmd_logic::nvrm_fence::NVGPU_CFG_DRM_FENCES;
use helios_kmd_logic::rm_fence_present::{Attach, Gate, GateError, GATE_POINTS};

/// Most processes with a live gate at once.
pub(super) const MAX_RM_GATES: usize = 8;

/// Points attached (`RmGAtt`), fired (`RmGFire`), fired with an error status
/// (`RmGErr`), attached already fired (`RmGEarly`), cancelled by teardown
/// (`RmGCan`), carriers refused (`RmGRef`, which includes a gate table or point
/// table with no room).
pub static RMG_ATTACHED: AtomicU32 = AtomicU32::new(0);
pub static RMG_FIRED: AtomicU32 = AtomicU32::new(0);
pub static RMG_ERRORS: AtomicU32 = AtomicU32::new(0);
pub static RMG_EARLY: AtomicU32 = AtomicU32::new(0);
pub static RMG_CANCELLED: AtomicU32 = AtomicU32::new(0);
pub static RMG_REFUSED: AtomicU32 = AtomicU32::new(0);
/// Handles of a `HERF` / `HEPR` tail the KMD took and closed although it attached no
/// marker (`RmGTake`): the UMD cannot know the tail was refused, so the handle is
/// the KMD's whatever became of the marker.
pub static RMG_TAKEN: AtomicU32 = AtomicU32::new(0);
/// Gates open now (a count, not a counter): `DestroyProcess` reads it before it
/// touches the adapter, so a process exit with no gate open costs one load.
static RMG_OPEN: AtomicU32 = AtomicU32::new(0);

/// Whether any process has a gate open. A gate is opened at Render under
/// `virtio_lock`, and a process that opened one cannot exit before its Render
/// returned, so a zero here at `DestroyProcess` means nothing of the process's is
/// left to purge.
pub fn rm_gates_open() -> bool {
    RMG_OPEN.load(Ordering::Acquire) != 0
}

/// Mirror the counters to the registry. PASSIVE only.
pub fn publish_rm_gate_counters() {
    use crate::diag::record_named_bytes as rec;
    rec(b"RmGAtt", RMG_ATTACHED.load(Ordering::Relaxed));
    rec(b"RmGFire", RMG_FIRED.load(Ordering::Relaxed));
    rec(b"RmGErr", RMG_ERRORS.load(Ordering::Relaxed));
    rec(b"RmGEarly", RMG_EARLY.load(Ordering::Relaxed));
    rec(b"RmGCan", RMG_CANCELLED.load(Ordering::Relaxed));
    rec(b"RmGRef", RMG_REFUSED.load(Ordering::Relaxed));
    rec(b"RmGTake", RMG_TAKEN.load(Ordering::Relaxed));
}

/// One gate: the process it belongs to, the present-stream slot that carries its
/// boundary, and its points.
pub(super) struct RmGateSlot {
    in_use: bool,
    process: usize,
    stream_index: usize,
    gate: Gate,
}

impl RmGateSlot {
    fn new() -> Self {
        Self {
            in_use: false,
            process: 0,
            stream_index: 0,
            gate: Gate::new(),
        }
    }
}

/// The gate table, built in its own (popped) frame like the present-stream table: a
/// gate is about 1 KiB, and `VirtioGpu::init` is on the boot stack.
#[inline(never)]
pub(super) fn allocate_rm_gates() -> Result<Vec<RmGateSlot>, VirtioError> {
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(MAX_RM_GATES)
        .map_err(|_| VirtioError::OutOfMemory)?;
    for _ in 0..MAX_RM_GATES {
        slots.push(RmGateSlot::new());
    }
    Ok(slots)
}

/// Why a WDDM carrier's fence was not attached. Every refusal leaves the handle the
/// caller's and the present on the legacy rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRefusal {
    /// The transport is failed, or events / fences are not served.
    Unsupported,
    /// Not a fence of this process (or already the KMD's).
    NotOwned,
    /// The handle is not a fence.
    NotFence,
    /// No gate, no stream slot or no point is free.
    NoRoom,
}

/// What attaching produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateAttached {
    /// The boundary the present names (tagged stream namespace).
    pub boundary: u64,
    /// The fence had fired already (the point is born retired).
    pub early: bool,
    /// The KMD now owes the host a `Close`: the caller wakes the worker.
    pub wake_worker: bool,
}

fn gate_refusal(why: FenceRefusal) -> GateRefusal {
    match why {
        FenceRefusal::NotOwned | FenceRefusal::AlreadyAttached => GateRefusal::NotOwned,
        FenceRefusal::NotFence => GateRefusal::NotFence,
        FenceRefusal::Quota => GateRefusal::NoRoom,
    }
}

impl VirtioGpu {
    /// Whether the WDDM carriers can be honoured now: the event queue is up and the
    /// host serves fences (`NVGPU_CFG_DRM_FENCES`).
    pub fn rm_fence_served(&self) -> bool {
        !self.failed
            && self.nvrm_events_state() == NvrmEventsState::Ready
            && self.cfg_features & NVGPU_CFG_DRM_FENCES != 0
    }

    fn rm_gate_of(&self, process: usize) -> Option<usize> {
        self.rm_gates
            .iter()
            .position(|g| g.in_use && g.process == process)
    }

    /// Open the gate of `process`: a free gate entry and a free stream slot.
    fn rm_gate_open(&mut self, process: usize) -> Option<usize> {
        let gi = self.rm_gates.iter().position(|g| !g.in_use)?;
        let (si, _) = self
            .present_streams
            .iter()
            .enumerate()
            .find(|(_, slot)| !slot.live && slot.generation < PRESENT_STREAM_GENERATION_MAX)?;
        let generation = self.present_streams[si].generation + 1;
        self.present_streams[si] = PresentStreamSlot {
            live: true,
            closing: false,
            owner: Some(DeviceOwner::KMD_RM),
            ctx_id: 0,
            ring_idx: 0,
            generation,
            cookie: 0,
            creator_process: process,
            claimed_value: 0,
            submitted_value: 0,
            progress: helios_kmd_logic::execution_completion::Progress::EMPTY,
        };
        let live = PRESENT_STREAM_LIVE.fetch_add(1, Ordering::Relaxed) as usize + 1;
        bump_high_water(&PRESENT_STREAM_HIGH_WATER, live);
        let g = &mut self.rm_gates[gi];
        g.in_use = true;
        g.process = process;
        g.stream_index = si;
        g.gate.reset();
        RMG_OPEN.fetch_add(1, Ordering::AcqRel);
        Some(gi)
    }

    /// Attach fence `fence` of `process` as the next point of the process's gate and
    /// return the boundary naming it. The ownership rule is `FenceClaim::Process`:
    /// a fence recorded as created in this process, not already the KMD's.
    ///
    /// Nothing changes on a refusal: the claim is probed read-only BEFORE a gate and a
    /// stream slot are taken (a junk handle must not open a gate that lasts until
    /// process exit), and the room is checked before the fence is taken over, so no
    /// rollback exists in the normal flow. The probe and the attach are one
    /// `virtio_lock` hold (this takes `&mut self`), so the attach cannot then refuse;
    /// if it ever did, a gate this call opened is closed again.
    pub fn rm_gate_attach(
        &mut self,
        fence: u32,
        process: usize,
    ) -> Result<GateAttached, GateRefusal> {
        let refuse = |why: GateRefusal| {
            RMG_REFUSED.fetch_add(1, Ordering::Relaxed);
            Err(why)
        };
        if process == 0 || fence == 0 || !self.rm_fence_served() {
            return refuse(GateRefusal::Unsupported);
        }
        if let Err(why) = self.fence_claimable(FenceClaim::Process(process), fence) {
            return refuse(gate_refusal(why));
        }
        let (gi, opened) = match self.rm_gate_of(process) {
            Some(gi) => (gi, false),
            None => match self.rm_gate_open(process) {
                Some(gi) => (gi, true),
                None => return refuse(GateRefusal::NoRoom),
            },
        };
        {
            let gate = &self.rm_gates[gi].gate;
            if gate.pending() as usize >= GATE_POINTS || gate.needs_recycle() {
                if opened {
                    self.rm_gate_cancel(gi);
                }
                return refuse(GateRefusal::NoRoom);
            }
        }
        let early =
            match self.fence_attach(FenceClaim::Process(process), fence, Attach::Gate(gi as u8)) {
                Ok(early) => early,
                Err(why) => {
                    if opened {
                        self.rm_gate_cancel(gi);
                    }
                    return refuse(gate_refusal(why));
                }
            };
        let point = match self.rm_gates[gi].gate.attach(fence, early.is_some()) {
            Ok(p) => p,
            // Checked above; if it ever happens the fence is the KMD's already, so
            // close it rather than strand it.
            Err(GateError::Full) | Err(GateError::Exhausted) => {
                let _ = self.fence_want_close(fence);
                return refuse(GateRefusal::NoRoom);
            }
        };
        RMG_ATTACHED.fetch_add(1, Ordering::Relaxed);
        let mut wake_worker = false;
        if let Some(status) = early {
            RMG_EARLY.fetch_add(1, Ordering::Relaxed);
            RMG_FIRED.fetch_add(1, Ordering::Relaxed);
            if status != 0 {
                RMG_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            wake_worker = self.fence_want_close(fence);
            self.rm_gate_sync(gi);
        }
        let si = self.rm_gates[gi].stream_index;
        let handle = self.present_streams[si].handle(si);
        Ok(GateAttached {
            boundary: encode_present_stream_boundary(handle, point),
            early: early.is_some(),
            wake_worker,
        })
    }

    /// The stream handle of `process`'s gate, if it has one open: what a context that
    /// already bound a boundary must match (`execution_completion::may_bind_stream`).
    pub fn rm_gate_stream_handle(&self, process: usize) -> Option<u32> {
        let gi = self.rm_gate_of(process)?;
        let si = self.rm_gates[gi].stream_index;
        Some(self.present_streams[si].handle(si))
    }

    /// A `HERF` / `HEPR` tail named fence `fence` of `process`, and its marker was not
    /// attached (both markers, a partial stream tail, no room, a refused attach): the
    /// UMD was told nothing (Render returned success), so it cannot close the handle,
    /// and every such handle would leak one of its 128. The KMD takes it and owes the
    /// host its `Close`, whether or not a marker exists. Only a fence recorded as
    /// created in this process (the claim of an attach) is taken: any other handle
    /// is not ours to close. Returns whether the handle was taken (a `Close` is then
    /// owed and the caller wakes the worker). Counted `RmGTake`.
    pub fn rm_fence_take(&mut self, fence: u32, process: usize) -> bool {
        if self.failed || process == 0 || fence == 0 {
            return false;
        }
        match self.fence_discard(FenceClaim::Process(process), fence) {
            Ok(_) => {
                RMG_TAKEN.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }

    /// Publish a gate's retirement into its stream slot and let waits that named it
    /// observe it. The same two steps `complete_present_stream_gpu` takes for a Venus
    /// stream, minus the producer table (a gate has no allocation epochs).
    fn rm_gate_sync(&mut self, gi: usize) {
        let retired = self.rm_gates[gi].gate.retired();
        let si = self.rm_gates[gi].stream_index;
        let slot = &mut self.present_streams[si];
        if !slot.live {
            return;
        }
        slot.progress.advance_to(retired);
        let completed = slot.progress.completed();
        let handle = slot.handle(si);
        for pending in self.wddm_pending.iter_mut() {
            if let Some(wait) = pending.execution.as_mut() {
                wait.observe(handle, completed);
            }
        }
    }

    /// `EventReady` for fence `fence`, attached to gate `gi`: fire its point. Called
    /// from the DPC; the caller wakes the worker (the handle is now owed a `Close`).
    pub(super) fn rm_gate_fire(&mut self, gi: u8, fence: u32, status: i32) {
        let gi = usize::from(gi);
        if gi >= self.rm_gates.len() || !self.rm_gates[gi].in_use {
            // The gate was purged (its fence was queued for closing then).
            return;
        }
        if self.rm_gates[gi].gate.fire(fence).is_some() {
            RMG_FIRED.fetch_add(1, Ordering::Relaxed);
            if status != 0 {
                RMG_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            self.rm_gate_sync(gi);
        }
        self.fence_want_close(fence);
    }

    /// Cancel gate `gi`: queue every unfired fence for closing, count the points it
    /// held, free the gate and retire its stream slot (a dead boundary: the caller
    /// discharges the waits that named it under the notification lock). Returns how
    /// many points were pending.
    fn rm_gate_cancel(&mut self, gi: usize) -> u32 {
        let pending = self.rm_gates[gi].gate.pending();
        loop {
            let Some(fence) = self.rm_gates[gi].gate.take_unfired() else {
                break;
            };
            self.fence_want_close(fence);
        }
        let si = self.rm_gates[gi].stream_index;
        let handle = self.present_streams[si].handle(si);
        self.rm_gates[gi].gate.reset();
        self.rm_gates[gi].in_use = false;
        self.rm_gates[gi].process = 0;
        if self.present_streams[si].live {
            self.retire_present_stream_slot(si);
            // An execution (HE12 v4) wait that named this gate: nobody will ever fire
            // its fences (they were just handed to the closer), and an execution
            // packet is never rebased or discharged by the generic dead-stream rule,
            // so left alone it would pin the head of the adapter-wide WDDM FIFO. It is
            // a cancellation, counted in `RmGCan` with the points, not a fire: latch
            // those waits as ended. A Venus stream's execution waits keep their own
            // rule (`discharge_dead_present_stream_waits`).
            for pending_wddm in self.wddm_pending.iter_mut() {
                if let Some(wait) = pending_wddm.execution.as_mut() {
                    wait.observe(handle, u32::MAX);
                }
            }
        }
        RMG_OPEN.fetch_sub(1, Ordering::AcqRel);
        RMG_CANCELLED.fetch_add(pending, Ordering::Relaxed);
        pending
    }

    /// `DestroyProcess`: the process's gate goes, and every wait that named it is
    /// discharged (a dead boundary is a cancellation, never success: the same rule as
    /// a stream whose owner died). Needs the notification ordering proof, like
    /// `purge_present_streams_for_owner`. Returns the points that were pending; the
    /// caller then prompts the completion DPC and wakes the HPD worker.
    pub fn rm_gate_purge_process_ordered(
        &mut self,
        order: &crate::adapter::NotifyOrdered<'_>,
        process: usize,
    ) -> u32 {
        let Some(gi) = self.rm_gate_of(process) else {
            return 0;
        };
        let pending = self.rm_gate_cancel(gi);
        let _ = self.discharge_dead_present_stream_waits(order);
        self.cancel_dead_undispatched_windowed_blt();
        // A gate with nothing pending can still have had boundaries in submitted
        // DMA buffers: report at least 1 so the caller prompts the DPC.
        pending.max(1)
    }

    /// Transport-wide purge (`purge_all_present_streams`): every gate goes. The slots
    /// themselves are retired by that function's own loop.
    pub(super) fn rm_gates_purge_all(&mut self) {
        for gi in 0..self.rm_gates.len() {
            if self.rm_gates[gi].in_use {
                self.rm_gate_cancel(gi);
            }
        }
    }
}

//! Asynchronous composed present (`BltAsync`): the transport half. The rules are
//! `helios_kmd_logic::blt_async` (host-tested), the knobs and counters `ddi/blt_async.rs`, the
//! design and the invariants `docs/zero-copy-present.md`, "Asynchronous composed present
//! (BltAsync, BltNoMirror)".
//!
//! Two routes live here.
//!
//! * DIRECT: [`VirtioGpu::enqueue_async_submit_blt`] takes (or joins) the destination's KMD writer
//!   ownership, enqueues the copy on ring 1 and records it in the in-flight table, all in ONE
//!   critical section of the transport lock. The completion DPC ([`VirtioGpu::blt_async_retire`],
//!   called from the used-ring drain next to the other per-fence retirements) hands the buffer
//!   back when the LAST direct writer of it retires: the copy of one context's ring 1 retire in
//!   submission order, so several frames for one destination can be in flight at once without
//!   the app thread sleeping in `begin_present_buffer_write_legacy`. The Present's DMA fence is
//!   NOT retired here: its private record names the copy's wire fence and `note_wddm_submission`
//!   gates it on that exact fence (`RetireDomain::IncludingGpu`), which is also what makes the
//!   destination's readers (DWM, after the fence) see the copy.
//! * DEFERRED: [`VirtioGpu::queue_async_blt`] is `queue_windowed_blt` for a request that has no
//!   snapshot reader to ledger (an NVK source), with the two flags a WindowedBlt snapshot does not
//!   carry: `async_blt` (counted, timed) and `no_mirror` (the ring completion hands the buffer
//!   back at once instead of owing the worker a CPU mirror).

use super::*;
use helios_kmd_logic::blt_async as ba;

/// Direct submissions in flight at most. Each retires within a few milliseconds; the table is
/// small on purpose (it lives in the by-value `VirtioGpu::init` frame) and a full table is the
/// legacy arm, not a failure.
pub(super) const MAX_BLT_ASYNC: usize = 8;

/// The in-flight table of the direct submissions (`helios_kmd_logic::blt_async::Table`).
pub(super) struct BltAsyncState {
    table: ba::Table<MAX_BLT_ASYNC>,
}

impl BltAsyncState {
    pub(super) const fn new() -> Self {
        Self {
            table: ba::Table::new(),
        }
    }
}

/// What [`VirtioGpu::enqueue_async_submit_blt`] did with the staged buffers.
pub(crate) enum BltEnq {
    /// Enqueued; the wire fence of the copy.
    Fence(u64),
    /// The destination is owned by a reader, or by a writer that is not a direct submission:
    /// nothing was enqueued and the buffers are handed back (to be dropped at PASSIVE).
    Busy(DmaBuffer, DmaBuffer),
}

impl VirtioGpu {
    /// What the route decision needs from the transport, in one critical section: the producer
    /// boundary's state, whether a queued (WindowedBlt) copy already names `dst`, and whether the
    /// direct table has room.
    pub(crate) fn blt_async_facts(
        &self,
        boundary: Option<u64>,
        dst: u32,
    ) -> (ba::Boundary, bool, bool) {
        let boundary = match boundary {
            None => ba::Boundary::None,
            Some(b) if self.present_stream_boundary_live(b) => ba::Boundary::Live {
                ready: self.scanout_boundary_ready(b),
            },
            Some(_) => ba::Boundary::Dead,
        };
        (
            boundary,
            self.blt_dst_deferred_pending(dst),
            self.blt_async.table.has_room(),
        )
    }

    /// A WindowedBlt request (a snapshot's, or a deferred NVK copy) that has not reached its
    /// terminal still names `dst` as its destination.
    pub(crate) fn blt_dst_deferred_pending(&self, dst: u32) -> bool {
        self.windowed_blt
            .pending
            .iter()
            .any(|request| request.destination_resource_id == dst)
    }

    /// The writer state of `resource_id`'s Present buffer as the ownership rule sees it, or `None`
    /// when no such buffer is tracked.
    fn blt_async_own(&self, resource_id: u32) -> Option<ba::Own> {
        let slot = self
            .present_buffer_syncs
            .iter()
            .find(|slot| slot.resource_id == resource_id)?;
        Some(match slot.access {
            PresentBufferAccess::ExternalReady => ba::Own::Free,
            PresentBufferAccess::Consumer(boundary) if self.scanout_boundary_ready(boundary) => {
                ba::Own::Free
            }
            PresentBufferAccess::KmdWriter => ba::Own::Writer,
            PresentBufferAccess::Empty
            | PresentBufferAccess::Consumer(_)
            | PresentBufferAccess::KmdCpuMirror
            | PresentBufferAccess::Teardown => ba::Own::Blocked,
        })
    }

    /// Enqueue the copy of a DIRECT asynchronous Blt (ring 1, fenced) and record it. See the
    /// module doc. The ownership transition, the enqueue and the table entry are one critical
    /// section, so the completion DPC (which needs this lock) cannot see an enqueued copy that
    /// has no entry, and no other submission can see a half-acquired buffer.
    pub(crate) fn enqueue_async_submit_blt(
        &mut self,
        ctx_id: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        resource_id: u32,
    ) -> Result<BltEnq, (DmaBuffer, DmaBuffer, VirtioError)> {
        if !self.blt_async.table.has_room() {
            return Err((meta, venus, VirtioError::QueueFull));
        }
        let Some(own) = self.blt_async_own(resource_id) else {
            return Err((meta, venus, VirtioError::DeviceError));
        };
        let acquired = match ba::begin(own, self.blt_async.table.writers(resource_id)) {
            ba::Begin::Busy => {
                PRESENT_BUFFER_WRITE_BUSY.fetch_add(1, Ordering::Relaxed);
                return Ok(BltEnq::Busy(meta, venus));
            }
            ba::Begin::Overlap => false,
            ba::Begin::Acquire => {
                if self.try_begin_present_buffer_write(resource_id)
                    != PresentBufferWriteBegin::Acquired
                {
                    return Ok(BltEnq::Busy(meta, venus));
                }
                true
            }
        };
        match self.enqueue_submit_inner(
            ctx_id,
            SCANOUT_RING_IDX,
            meta,
            venus,
            venus_len,
            None,
            None,
            None,
            None,
        ) {
            Ok(fence_id) => {
                let added = self.blt_async.table.add(ba::Entry {
                    fence_id,
                    resource_id,
                    t0: crate::ddi::blt_async::now_100ns(),
                });
                if added {
                    crate::ddi::blt_async::note_infl_add();
                }
                // `has_room` held under this same lock and wire fence ids only grow, so `added`
                // is true. If it ever were not, the buffer stays KMD-owned (fail closed, the
                // same as a host rejection of the legacy copy) rather than being handed back
                // under a copy that is still running.
                debug_assert!(added);
                Ok(BltEnq::Fence(fence_id))
            }
            Err(error) => {
                // Descriptor enqueue failed: no host command exists. A buffer this call
                // acquired goes back; a joined one belongs to the earlier submissions.
                if acquired {
                    self.complete_present_buffer_write(resource_id);
                }
                Err(error)
            }
        }
    }

    /// The used-ring drain retired wire fence `fence_id` (`response_ok` is the host's verdict).
    /// A direct asynchronous copy hands its destination back when it was the last of the buffer's
    /// writers. A failed copy still hands it back: the host rejected a command that touched
    /// nothing, the Present's fence retires with the wire fence either way, and the destination
    /// keeps the previous frame (`BltAsyncFail`). One compare when nothing is in flight.
    pub(super) fn blt_async_retire(&mut self, fence_id: u64, response_ok: bool) {
        if self.blt_async.table.is_empty() {
            return;
        }
        let Some(done) = self.blt_async.table.complete(fence_id) else {
            return;
        };
        crate::ddi::blt_async::note_infl_sub();
        crate::ddi::blt_async::note_copy_done(done.t0, response_ok);
        if done.last_for_resource {
            // KmdWriter -> ExternalReady, and the queued WindowedBlt requests that were only
            // waiting for this buffer are woken.
            self.complete_present_buffer_write(done.resource_id);
        }
    }

    /// A transport failure latched: nothing in the table can complete any more. Forget the
    /// entries (the buffers die with the transport generation).
    pub(super) fn blt_async_forget(&mut self) {
        for _ in 0..self.blt_async.table.len() {
            crate::ddi::blt_async::note_infl_sub();
        }
        self.blt_async.table.clear();
    }

    /// `queue_windowed_blt` for a DEFERRED asynchronous Blt: the same FIFO, token and boundary
    /// rules, with no snapshot reader ledgered (the NVK source is not a DXVK snapshot and nobody
    /// reads the ledger for it) and the two flags `WindowedBltPending` carries for it.
    pub(crate) fn queue_async_blt(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        source: OptimalPresentImageDesc,
        destination: PresentDestinationDesc,
        prepared: PreparedPresentBltSubmission,
        stream_boundary: u64,
        no_mirror: bool,
    ) -> Result<u64, VirtioError> {
        if self.failed
            || !self.present_stream_boundary_live(stream_boundary)
            || source.resource_id() == 0
            || source.resource_id() == destination.resource_id()
            || !matches!(destination, PresentDestinationDesc::StandardBuffer(_))
        {
            return Err(VirtioError::DeviceError);
        }
        let Some(token) = self.windowed_blt.issue_token() else {
            return Err(VirtioError::OutOfMemory);
        };
        self.windowed_blt.pending.push_back(WindowedBltPending {
            adapter: NonNull::from(adapter),
            token,
            stream_boundary,
            source_resource_id: source.resource_id(),
            destination_resource_id: destination.resource_id(),
            source,
            destination,
            prepared,
            ledger_ticket: LedgerTicket::NONE,
            admitted: false,
            dispatched: false,
            ring_complete: false,
            mirror_ready: false,
            // Nothing was ledgered, so there is nothing to retire.
            ledger_retired: true,
            wddm_completion_required: true,
            mirror_claimed: false,
            async_blt: true,
            no_mirror,
            t_queue: crate::ddi::blt_async::now_100ns(),
            t_submit: 0,
        });
        crate::ddi::blt_async::note_infl_add();
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::WINDOWED_BLT_ARM,
            crate::ddi::scanout_timeline::flag::SNAPSHOT,
            0,
            stream_boundary,
            token,
            source.resource_id(),
            destination.resource_id(),
        );
        Ok(token)
    }

    /// The worker dispatched `pending[index]` (it is about to be enqueued on the ring): stamp the
    /// submission time of an asynchronous request and count what it waited in the FIFO.
    pub(super) fn blt_async_dispatched(&mut self, index: usize) {
        let request = &mut self.windowed_blt.pending[index];
        if !request.async_blt {
            return;
        }
        request.t_submit = crate::ddi::blt_async::now_100ns();
        crate::ddi::blt_async::note_defer_wait(request.t_queue, request.t_submit);
    }

    /// Hand `resource_id` back to its readers if the KMD owns it as a writer, and do nothing (no
    /// reject counted) if it does not.
    pub(super) fn blt_async_release_writer(&mut self, resource_id: u32) {
        let owned = self
            .present_buffer_syncs
            .iter()
            .any(|slot| slot.resource_id == resource_id && slot.access == PresentBufferAccess::KmdWriter);
        if owned {
            self.complete_present_buffer_write(resource_id);
        }
    }

    /// A request left `pending` (terminal, cancelled or abandoned).
    pub(super) fn blt_async_gone(&self, request: &WindowedBltPending) {
        if request.async_blt {
            crate::ddi::blt_async::note_infl_sub();
        }
    }
}

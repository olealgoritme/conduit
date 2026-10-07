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
    /// The adapter that owns this transport, set by the first direct submission: the completion
    /// DPC needs it for the source's ledger ticket and the Level 5 frame edge. It names the same
    /// adapter for the life of the transport and every entry is retired or forgotten before the
    /// transport (`StopDevice` drains, the latch forgets), like `WindowedBltPending::adapter`.
    adapter: Option<NonNull<crate::adapter::AdapterContext>>,
}

impl BltAsyncState {
    pub(super) const fn new() -> Self {
        Self {
            table: ba::Table::new(),
            adapter: None,
        }
    }
}

/// What the route decision needs from the transport, in one critical section.
pub(crate) struct BltFacts {
    /// The producer boundary's state.
    pub boundary: ba::Boundary,
    /// A queued (WindowedBlt) copy already names the destination.
    pub deferred_pending: bool,
    /// The direct table has room.
    pub room: bool,
    /// An earlier asynchronous copy is still reading the source.
    pub source_busy: bool,
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
    /// The facts of one Present: the producer boundary's state, whether a queued (WindowedBlt)
    /// copy already names `dst`, whether the direct table has room, and whether an earlier
    /// asynchronous copy still reads `src`.
    pub(crate) fn blt_async_facts(&self, boundary: Option<u64>, dst: u32, src: u32) -> BltFacts {
        let boundary = match boundary {
            None => ba::Boundary::None,
            Some(b) if self.present_stream_boundary_live(b) => ba::Boundary::Live {
                ready: self.scanout_boundary_ready(b),
            },
            Some(_) => ba::Boundary::Dead,
        };
        BltFacts {
            boundary,
            deferred_pending: self.blt_dst_deferred_pending(dst),
            room: self.blt_async.table.has_room(),
            source_busy: self.blt_async.table.readers(src) > 0
                || self
                    .windowed_blt
                    .pending
                    .iter()
                    .any(|request| request.async_blt && request.source_resource_id == src),
        }
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
        adapter: &crate::adapter::AdapterContext,
        ctx_id: u32,
        meta: DmaBuffer,
        venus: DmaBuffer,
        venus_len: usize,
        resource_id: u32,
        source_id: u32,
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
        // A read of the source until the copy retires, published in the read ledger like a
        // WindowedBlt snapshot's: a consumer of the ledger (the UMD) sees the source busy. A
        // full ledger leaves the copy unledgered (loud in `RdOvf`, never a refusal).
        let ticket = adapter.read_ledger.issue(source_id);
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
                self.blt_async.adapter = Some(NonNull::from(adapter));
                let added = self.blt_async.table.add(
                    ba::Entry::new(fence_id, resource_id, crate::ddi::blt_async::now_100ns())
                        .reading(source_id, ticket),
                );
                if added {
                    crate::ddi::blt_async::note_infl_add();
                } else {
                    adapter.read_ledger.retire(ticket, true);
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
                adapter.read_ledger.retire(ticket, true);
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
        if let Some(adapter) = self.blt_async.adapter {
            // SAFETY: set by an enqueue of this transport; see `BltAsyncState::adapter`.
            let adapter = unsafe { adapter.as_ref() };
            // The copy has stopped reading the source (a failed one too).
            adapter.read_ledger.retire(done.ticket, !response_ok);
            // Level 5: the frame is in the destination; if it is the shown RM primary a flip is
            // owed (atomics only, DISPATCH-legal).
            crate::ddi::blt_async::raise_edge(
                adapter,
                ba::Finish::Direct,
                response_ok,
                done.resource_id,
            );
        }
    }

    /// A transport failure latched: nothing in the table can complete any more. Forget the
    /// entries (the buffers die with the transport generation).
    pub(super) fn blt_async_forget(&mut self) {
        while let Some(entry) = self.blt_async.table.pop_oldest() {
            crate::ddi::blt_async::note_infl_sub();
            if let Some(adapter) = self.blt_async.adapter {
                // SAFETY: as `blt_async_retire`.
                unsafe { adapter.as_ref() }.read_ledger.retire(entry.ticket, true);
            }
        }
    }

    /// `queue_windowed_blt` for a DEFERRED asynchronous Blt: the same FIFO, token and boundary
    /// rules, with the two flags `WindowedBltPending` carries for it. The source is ledgered
    /// like a snapshot's, but an overflowing ledger does not refuse the request.
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
            || !(matches!(destination, PresentDestinationDesc::StandardBuffer(_))
                // `RedirVram` (docs/vram-redirection.md 5.3): an image destination only when it is a
                // KMD RM video-memory surface, whose copy the copy-engine route takes. The
                // dispatch takes no Present-buffer ownership for an image, and its ring completion
                // terminalizes at once (`complete_windowed_blt_ring`).
                || (matches!(destination, PresentDestinationDesc::OptimalImage(_))
                    && crate::virtio::rm_client::vidmem::lookup(destination.resource_id()).is_some()))
        {
            return Err(VirtioError::DeviceError);
        }
        let Some(token) = self.windowed_blt.issue_token() else {
            return Err(VirtioError::OutOfMemory);
        };
        // A read of the source until the copy retires (the ring completion or the terminal
        // retires it), as for a snapshot; a full ledger leaves the request unledgered, not
        // refused.
        let ledger_ticket = adapter.read_ledger.issue(source.resource_id());
        self.windowed_blt.pending.push_back(WindowedBltPending {
            adapter: NonNull::from(adapter),
            token,
            stream_boundary,
            source_resource_id: source.resource_id(),
            destination_resource_id: destination.resource_id(),
            source,
            destination,
            prepared,
            ledger_ticket,
            admitted: false,
            dispatched: false,
            ring_complete: false,
            mirror_ready: false,
            ledger_retired: false,
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

    /// `GuestBlob`: the worker re-prepared the dispatched request `(token, stream_boundary)`
    /// because the guest buffer it was prepared for is no longer its destination's copy target
    /// (`VenusClient::retarget_prepared_present_blt`). Its preparation and its mirror flag are
    /// replaced BEFORE it is submitted, so the ring completion owes the worker a CPU mirror
    /// exactly when the copy no longer writes the leased pages itself. `false`: no such
    /// request (the caller then submits nothing). Spinlock-only, no allocation.
    pub(crate) fn retarget_windowed_blt(
        &mut self,
        token: u64,
        stream_boundary: u64,
        prepared: PreparedPresentBltSubmission,
        no_mirror: bool,
    ) -> bool {
        match self
            .windowed_blt
            .pending
            .iter_mut()
            .find(|request| request.token == token && request.stream_boundary == stream_boundary)
        {
            Some(request) => {
                request.prepared = prepared;
                request.no_mirror = no_mirror;
                true
            }
            None => false,
        }
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

    /// Which ready request the worker dispatches next: `(index in pending, position in ready)`.
    /// The first of the first `BltLookahead` ready entries that can go now (admitted, its
    /// producer's boundary reached, its destination writable), unless an earlier live entry names
    /// the same destination (`helios_kmd_logic::blt_async::pick`). `front` is the healed front's
    /// index in `pending`. With a depth of 1 this is the rule before v337.
    pub(super) fn blt_pick_ready(&self) -> Option<(usize, usize)> {
        let depth = crate::ddi::blt_async::lookahead().min(ba::LOOKAHEAD_MAX);
        let mut window = [ba::Cand {
            live: false,
            dst: 0,
            dispatchable: false,
        }; ba::LOOKAHEAD_MAX];
        let mut index = [usize::MAX; ba::LOOKAHEAD_MAX];
        let mut n = 0;
        for token in self.windowed_blt.ready.iter().take(depth) {
            let at = self
                .windowed_blt
                .pending
                .iter()
                .position(|request| request.token == *token);
            if let Some(i) = at {
                let request = &self.windowed_blt.pending[i];
                let live = !request.dispatched;
                let writable = match request.destination {
                    PresentDestinationDesc::StandardBuffer(d) => {
                        self.blt_async_own(d.resource_id()) == Some(ba::Own::Free)
                    }
                    PresentDestinationDesc::OptimalImage(_) => true,
                };
                window[n] = ba::Cand {
                    live,
                    dst: request.destination_resource_id,
                    dispatchable: live
                        && request.admitted
                        && writable
                        && self.scanout_boundary_ready(request.stream_boundary),
                };
                index[n] = i;
            }
            n += 1;
        }
        let pos = ba::pick(&window[..n])?;
        Some((index[pos], pos))
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

    /// The copy-engine Present route (`RmCopyEngine` 1, `ddi/ce_present_route.rs`): whether the
    /// request `(token, stream_boundary)` still exists, and if so whether the worker dispatched
    /// it. Spinlock only.
    pub(crate) fn ce_blt_request(&self, token: u64, stream_boundary: u64) -> Option<bool> {
        self.windowed_blt
            .pending
            .iter()
            .find(|request| request.token == token && request.stream_boundary == stream_boundary)
            .map(|request| request.dispatched)
    }

    /// The copy-engine route's completion of the dispatched request `(token, stream_boundary)`:
    /// the copy engine wrote the destination's own pages (or, `ok` false, the copy was discharged
    /// after its deadline). The request's ring completion with the mirror forced OFF whatever its
    /// Venus fallback would have needed: the destination goes back to its readers at once, the
    /// source's ledger ticket retires, the Level 5 edge is raised, the token terminalizes (the
    /// Present's DMA fence waits for exactly that), and nothing is marked stale. Spinlock only.
    pub(crate) fn complete_ce_blt(
        &mut self,
        adapter: &crate::adapter::AdapterContext,
        token: u64,
        stream_boundary: u64,
        ok: bool,
    ) {
        let Some(request) = self.windowed_blt.pending.iter_mut().find(|request| {
            request.token == token && request.stream_boundary == stream_boundary && request.dispatched
        }) else {
            return;
        };
        request.no_mirror = true;
        self.complete_windowed_blt_ring(adapter, token, stream_boundary, ok);
    }

    /// The copy-engine route (`CeRtDirect`): whether the CPU may consider the producer of
    /// `stream_boundary` finished: its boundary is ready, or dead (a stream that is gone will
    /// never fire: the copy's deadline starts and it is discharged if its acquire never
    /// releases). Spinlock only.
    pub(crate) fn ce_boundary_seen(&self, stream_boundary: u64) -> bool {
        !self.present_stream_boundary_live(stream_boundary)
            || self.scanout_boundary_ready(stream_boundary)
    }

    /// The copy-engine route's DIRECT dispatch (`CeRtDirect`) of the request
    /// `(token, stream_boundary)` at its own Present, before its producer finished: in ONE
    /// critical section of this lock, the request must exist undispatched, be the only pending
    /// request naming `destination` (per-destination order), and the destination must be taken as
    /// `KmdWriter` without waiting (`try_begin_present_buffer_write`, the worker's rule); then
    /// `submit` runs (the route's spinlock and the channel's: plain stores) and, if it submitted,
    /// the request is dispatched and admitted (SubmitCommand's later admission finds it admitted
    /// and skips it), else the writer goes back and the request stays a deferred one. Spinlocks
    /// only. `Err(())`: nothing changed (the caller counts why).
    pub(crate) fn ce_direct_dispatch(
        &mut self,
        token: u64,
        stream_boundary: u64,
        destination: u32,
        submit: impl FnOnce() -> bool,
    ) -> Result<(), bool> {
        let Some(index) = self.windowed_blt.pending.iter().position(|request| {
            request.token == token && request.stream_boundary == stream_boundary
        }) else {
            return Err(false);
        };
        let request = &self.windowed_blt.pending[index];
        let older = self.windowed_blt.pending.iter().any(|r| {
            r.destination_resource_id == destination
                && !(r.token == token && r.stream_boundary == stream_boundary)
        });
        if request.dispatched
            || request.destination_resource_id != destination
            || !matches!(request.destination, PresentDestinationDesc::StandardBuffer(_))
            || older
        {
            return Err(false);
        }
        if self.try_begin_present_buffer_write(destination) != PresentBufferWriteBegin::Acquired {
            return Err(false);
        }
        if !submit() {
            // Nothing reached the GPU: the destination goes back, the request stays deferred.
            self.complete_present_buffer_write(destination);
            return Err(true);
        }
        let was_admitted = self.windowed_blt.pending[index].admitted;
        {
            let request = &mut self.windowed_blt.pending[index];
            request.dispatched = true;
            request.admitted = true;
        }
        if was_admitted {
            self.windowed_blt.ready.retain(|known| *known != token);
        }
        self.blt_async_dispatched(index);
        Ok(())
    }

    /// A request left `pending` (terminal, cancelled or abandoned).
    pub(super) fn blt_async_gone(&self, request: &WindowedBltPending) {
        if request.async_blt {
            crate::ddi::blt_async::note_infl_sub();
        }
    }
}

//! PASSIVE-level control-command orchestration (C3/M3.4).
//!
//! Every virtio-gpu control verb (ctx/blob/map/attach/unref) is a multi-phase
//! flow here: table phase(s) under the device spinlock ([`VirtioGpu`] helpers)
//! interleaved with a host round-trip whose WAIT happens at PASSIVE_LEVEL on a
//! stack [`SyncWaitBlock`] KEVENT — never a DISPATCH spin under the spinlock.
//! The waits use adaptive slices and re-drain the used ring on each slice, so
//! they are interrupt-driven when interrupts flow and degrade to ~ms-latency
//! polling when they do not (bring-up, lost interrupts).
//!
//! Why this exists (2026-07-04 evidence): the host processes the virtio ctrl
//! queue serially, and a venus `RESOURCE_CREATE_BLOB(blob_id)` can legally
//! block host-side waiting for the vkr ring to execute the referenced
//! `vkAllocateMemory` — so ANY control command can take seconds under a
//! validate-slow host. The old model burned a ~1 s DISPATCH spin per waiter
//! under the device spinlock and then poisoned the transport; this model waits
//! properly and, on timeout, abandons only its own in-flight slot.
//!
//! # IRQL
//!
//! Every function in this module runs at PASSIVE_LEVEL, and since R614 that is a
//! signature rather than this comment: every entry point takes a
//! [`crate::irql::PassiveLevel`], which safe code cannot construct. What the
//! token proves, exactly:
//!
//! * **What it does prove.** A caller that holds no token cannot reach any of
//!   these functions at all. The concrete case: the DIRQL half of
//!   `DxgkDdiSetVidPnSourceAddress` (`ddi::display`) holds no token, so
//!   `set_scanout_blob` and `resource_flush` are unreachable from it, and adding
//!   such a call is a compile error instead of a shipped DISPATCH deadlock.
//! * **What it does NOT prove.** The live IRQL. Only `KeGetCurrentIrql` can, and
//!   this module deliberately does not call it per entry point — one check inside
//!   `PassiveLevel::assume` at the DDI boundary is the whole budget. So the
//!   guarantee is about *provenance*: every token in the driver traces to one of
//!   twelve audited mints (`grep -rn 'PassiveLevel::assume()' src/`), four of
//!   which sit below a runtime IRQL gate that already existed, plus one
//!   structural claim about the venus gateway
//!   (`AdapterContext::with_venus_client`).
//!
//! `crate::irql::IRQL_ASSUME_BAD` — the `IrqlBad` breadcrumb — is what turns a
//! wrong audit into evidence. It must read 0.
//!
//! One PASSIVE-only operation is still outside the type system:
//! `DmaBuffer`'s `Drop` (`MmFreeContiguousMemory`). `Drop::drop` has a fixed
//! signature, so the transport parks completed buffers and frees them from
//! [`reap_parked`] instead of letting the DISPATCH drain drop one.
//!
//! ONE function here is deliberately IRQL-free and takes no token:
//! [`fill_set_scanout_blob`], which only writes the fields of a wire command
//! someone else owns. It lives here so the DISPATCH-level fast bind
//! (`VirtioGpu::enqueue_scanout_bind_async`, ROADMAP defect 0ab-C) and the
//! PASSIVE round-trip below cannot encode the same command differently.

use core::cell::Cell;
use core::mem::size_of;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::AtomicU32;

use bytemuck::{bytes_of, Zeroable};
use wdk_sys::ntddk::{KeDelayExecutionThread, KeQueryInterruptTimePrecise, KeWaitForSingleObject};
use wdk_sys::{KEVENT, LARGE_INTEGER, PVOID, STATUS_SUCCESS};

use super::gpu::{
    BlobMapBegin, BlobMapFinish, BlobMapPrep, BlobRemapBegin, DeviceOwner, FenceWaitPrep, InFlight,
    OwnerFilter, SyncOutcome, SyncTicket, SyncWaitBlock, WaitBlockRef, CTRL_TEARDOWN_ABANDONS,
    CTRL_TIMEOUT_COUNT, ESCAPE_SUBMIT_CALLS, ESCAPE_SUBMIT_COUNT, ESCAPE_SUBMIT_RING_COUNT,
    FENCE_WAIT_TABLE_FULL,
    FENCE_WAIT_TIMEOUTS, RING_POPS, SUBMIT_META_BYTES, TRANSPORT_GONE_AT_WAIT,
};
use super::hal::DmaBuffer;
use super::submit_stage::{self, measured};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::error::NotStarted;
use crate::irql::PassiveLevel;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use helios_kmd_logic::submit_stage::{Clock, Path, Stage};
use helios_kmd_logic::sweep_budget::SweepBudget;
use helios_protocol::{
    resp_is_ok, VirtioGpuCtrlHdr, VirtioGpuCtxCreate, VirtioGpuCtxDestroy, VirtioGpuCtxResource,
    VirtioGpuGetCapsetInfo, VirtioGpuRect, VirtioGpuResourceCreateBlob, VirtioGpuResourceFlush,
    VirtioGpuResourceMapBlob, VirtioGpuResourceUnmapBlob, VirtioGpuResourceUnref,
    VirtioGpuRespCapsetInfo, VirtioGpuRespMapInfo, VirtioGpuSetScanoutBlob, HeliosSetCursorBlob,
    HELIOS_CMD_SET_CURSOR_BLOB, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_CREATE, VIRTIO_GPU_CMD_CTX_DESTROY,
    VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE, VIRTIO_GPU_CMD_GET_CAPSET_INFO,
    VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB, VIRTIO_GPU_CMD_RESOURCE_FLUSH,
    VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB, VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB,
    VIRTIO_GPU_CMD_RESOURCE_UNREF, VIRTIO_GPU_CMD_SET_SCANOUT_BLOB, VIRTIO_GPU_MAP_CACHE_MASK,
};

/// `KernelMode` (`KPROCESSOR_MODE`).
const KERNEL_MODE: i8 = 0;
/// `Executive` (`KWAIT_REASON`).
const EXECUTIVE: i32 = 0;

/// Default PASSIVE wait budget for one synchronous control round-trip. Sized
/// for a validate-slow host whose ctrl queue is momentarily blocked behind a
/// wait-for-mem-alloc blob create; beyond this the host is genuinely wedged
/// and the command fails loudly (`VirtioError::Timeout`).
const SYNC_ROUNDTRIP_TIMEOUT_MS: u64 = 30_000;
/// Backpressure retry budget when the control queue / in-flight tables are
/// full. MILLISECONDS, like its three siblings — it used to be a bare retry
/// count that only *happened* to equal 5 s because the sleep is hard-coded to
/// 1 ms.
const ENQUEUE_RETRY_MAX_MS: u64 = 5_000;
/// Hard cap on a single WAIT_FENCE escape (the ICD's own forward-progress
/// deadline fires far earlier; this only bounds kernel-side thread residency).
const WAIT_FENCE_MAX_MS: u64 = 120_000;
/// Bound on waiting out another mapper's in-flight RESOURCE_MAP_BLOB.
/// MILLISECONDS, as above.
const MAP_BUSY_MAX_MS: u64 = 30_000;

/// Largest `GpuCmd` request the backend takes (docs/VENUS.md): 4 MiB including
/// the `MsgHeader` and the SUBMIT_3D header. Larger streams are refused here
/// rather than answered with a transport error after the copy.
const GPU_CMD_MAX: usize = 4 << 20;

/// True when `stream` plus its framing fits one `GpuCmd`.
fn stream_fits(stream: &[u8]) -> bool {
    stream.len()
        <= GPU_CMD_MAX
            - super::hal::MSG_HDR_LEN
            - size_of::<helios_protocol::VirtioGpuCmdSubmit>()
}

/// One PASSIVE retry slice. See [`sleep_ms`] for why this is not really 1 ms.
const RETRY_SLICE_MS: u64 = 1;

/// A retry budget in MILLISECONDS.
///
/// Two of the four sibling constants used to be millisecond budgets and two
/// were bare retry counts that only *happened* to equal 5 s and 30 s because
/// the sleep is hard-coded to 1 ms, and a fifth budget was an unnamed literal.
/// A units mismatch like that makes every reader over-estimate how fast the
/// driver gives up.
///
/// It counts NOMINAL slept milliseconds, not wall clock, which is exactly what
/// the retry counters it replaces did — same numbers, same sleeps, same failure
/// statuses. See [`sleep_ms`] for why nominal and actual differ by up to ~16x.
struct Budget {
    total_ms: u64,
    spent_ms: u64,
}

impl Budget {
    const fn new(total_ms: u64) -> Self {
        Self {
            total_ms,
            spent_ms: 0,
        }
    }

    /// Charge one slice. Returns true once the budget is exhausted, matching
    /// the old `attempts > MAX` test exactly (charge first, then test).
    fn charge_slice(&mut self) -> bool {
        self.spent_ms = self.spent_ms.saturating_add(RETRY_SLICE_MS);
        // Inside an escape (v334, `ddi::escape_wait`): a terminating thread, a stopping device or
        // a spent `EscWaitMs` ends EVERY retry loop that charges a Budget, through the exit the
        // loop already has for a spent budget (`QueueFull` and friends). The nominal budget is
        // up to ~16x its real time (see `sleep_ms`), which is exactly the unbounded kind of wait
        // the escape deadline exists for. Never aborts a thread that is not inside an escape.
        if let Some(why) = crate::ddi::escape_wait::abort_now() {
            crate::ddi::escape_wait::note_abort(why);
            self.spent_ms = self.total_ms.saturating_add(1);
        }
        self.expired()
    }

    fn expired(&self) -> bool {
        self.spent_ms > self.total_ms
    }

    #[allow(dead_code)]
    fn elapsed_ms(&self) -> u64 {
        self.spent_ms
    }
}

/// PASSIVE sleep for ~`ms` milliseconds.
pub(crate) fn sleep_ms(_passive: PassiveLevel, ms: u64) {
    let mut interval: LARGE_INTEGER = unsafe { core::mem::zeroed() };
    interval.QuadPart = -((ms.max(1) as i64) * 10_000);
    // SAFETY: PASSIVE_LEVEL relative-timeout sleep.
    let _ = unsafe { KeDelayExecutionThread(KERNEL_MODE, 0, &mut interval) };
}
// ⚠ `KeDelayExecutionThread` with a small relative timeout rounds UP to the
// system timer granularity — ~15.6 ms by default. A `sleep_ms(1)` therefore
// costs up to ~16 ms of thread residency, so a [`Budget`] of N nominal
// milliseconds can be up to ~16N of real time. Every budget in this module is
// nominal for that reason; do not read one as wall clock.

/// Wait on `block` for up to `total_ms`, in adaptive slices (1 ms → 1 s),
/// opportunistically draining the used ring after each slice so a lost
/// interrupt costs only slice latency. Returns whether the block completed.
///
/// ⚠ THE SIGNAL IS THE ONLY COMPLETION-SIDE EXIT. THE INVARIANT: a waiter may
/// resume — and therefore pop the stack frame this block lives in — only once
/// the signaler has finished touching the block. This loop has exactly two
/// exits, and each one carries that guarantee:
///
///   * `KeWaitForSingleObject` returning STATUS_SUCCESS. The kernel's own
///     stack-event contract: the wait cannot be satisfied before `KeSetEvent`
///     has finished with the dispatcher object.
///   * The timeout, which goes to `abandon_sync` under `virtio_lock` — the same
///     lock the whole `InFlightKind::Sync` arm runs under, so abandon either
///     clears `waiter` before the drain runs (and the drain then signals
///     nothing) or observes `AlreadyCompleted` after the arm finished. The
///     timed-out waiter's frame is alive across its own abandon call, so the
///     block outlives every access on that side too.
///
/// A LOCK-FREE `done` POLL PROVIDES NEITHER, and this loop used to open with
/// one. `done` is stored one instruction BEFORE `KeSetEvent` in the drain's
/// Sync arm (`gpu/mod.rs`, the `InFlightKind::Sync` write site), so a waiter
/// polling it could return, pop its frame, and leave the drain to memcpy and
/// signal a dead stack frame — one ISR or KVM vm-exit inside that one-
/// instruction window is all it takes. That is the 22.22.218.0 `0xA` bugcheck,
/// root-caused from two dumps (ROADMAP defect 0ab-C): `KeSetEvent` walking the
/// waiter list of a "KEVENT" that was the HPD worker's own popped frame. It
/// only became reachable when the sync waits started outliving a wait slice.
///
/// Nothing is lost by removing it: a completion that lands before the first
/// wait leaves the KEVENT SIGNALED, and a KEVENT holds state, so the next
/// `KeWaitForSingleObject` returns immediately. The only cost is one
/// 15.6 ms-granularity slice on the rare poll-hit, and correctness owns that
/// trade.
fn wait_block(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    block: &WaitBlockRef<'_>,
    total_ms: u64,
) -> bool {
    // The slice ladder, the total and the abort rules are `helios_kmd_logic::wait_bound::Bounded`
    // (host-tested). Outside an escape it is the old 1 ms -> 1 s doubling to `total_ms`, byte for
    // byte; inside one (v334, `ddi::escape_wait`) the slices stop at 100 ms and each ends with a
    // check for a terminating thread, a stopping device and the escape's `EscWaitMs`, so a
    // wait that the host never answers is ended by a kill, a stop or the deadline instead of by
    // `total_ms` (30 s per call, minutes for a loop of calls). An abort is a timeout to the
    // caller: it takes the abandon path and fails with `VirtioError::Timeout`.
    let mut bounded = helios_kmd_logic::wait_bound::Bounded::new(total_ms);
    loop {
        let this_slice = match bounded.step(crate::ddi::escape_wait::probe()) {
            helios_kmd_logic::wait_bound::Step::Wait(ms) => ms,
            helios_kmd_logic::wait_bound::Step::Spent => return false,
            helios_kmd_logic::wait_bound::Step::Abort(why) => {
                crate::ddi::escape_wait::note_abort(why);
                return false;
            }
        };
        let mut timeout: LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = -((this_slice.max(1) as i64) * 10_000);
        // SAFETY: the KEVENT was initialized by SyncWaitBlock::init at this
        // address and outlives the wait; PASSIVE_LEVEL.
        let status = unsafe {
            KeWaitForSingleObject(
                core::ptr::addr_of_mut!((*block.as_ptr().as_ptr()).event) as PVOID,
                EXECUTIVE,
                KERNEL_MODE,
                0,
                &mut timeout,
            )
        };
        if status == STATUS_SUCCESS {
            return true;
        }
        bounded.expired(this_slice);
        // Interrupt-loss tolerance: drain whatever completed. A drain that TOOK something is a
        // "rescue": the completion was there and no interrupt (yet) had run the DPC for it. The
        // count is taken inside the lock hold, so a pop by the DPC on another CPU is not
        // mistaken for ours. In message mode a run of rescues with no interrupt between them
        // is how a delivery that does not work is found (`virtio::msi::note_rescue`).
        let rescued = adapter
            .with_virtio(|v| {
                let before = RING_POPS.load(Ordering::Relaxed);
                v.drain_used();
                RING_POPS.load(Ordering::Relaxed) != before
            })
            .unwrap_or(false);
        if rescued {
            super::msi::note_rescue(passive, adapter);
        }
    }
}

/// Reap completed entries at PASSIVE and retain their DMA buffers for reuse.
/// `MmAllocateContiguousMemory` per tiny Venus submission dominated DWM's
/// command rate; recycling page-backed buffers removes that steady-state cost.
pub fn reap_parked(passive: PassiveLevel, adapter: &AdapterContext) {
    let work = adapter.with_virtio(|v| v.begin_parked_reap());
    reap_parked_work(passive, adapter, work);
}

/// The rest of [`reap_parked`] for a caller that took the `begin_parked_reap`
/// result inside a `with_virtio` hold it needed anyway (`raw_roundtrip` does it
/// in the same hold as its pool take, one lock instead of two). Whatever `work`
/// holds MUST be passed here, or `reap_in_progress` stays set and reaping is
/// disabled for good (see `abort_parked_reap`).
fn reap_parked_work(
    _passive: PassiveLevel,
    adapter: &AdapterContext,
    work: Result<Option<(Vec<InFlight>, Vec<DmaBuffer>)>, NotStarted>,
) {
    let Ok(Some((mut dead, mut buffers))) = work else {
        return;
    };
    debug_assert!(buffers.capacity() >= dead.len().saturating_mul(2));
    for entry in dead.drain(..) {
        let (meta, venus) = entry.into_dma_buffers();
        buffers.push(meta);
        if let Some(venus) = venus {
            buffers.push(venus);
        }
    }
    // Moving buffers into the pre-reserved pool is allocation-free under the
    // spinlock. Excess buffers are returned and dropped here at PASSIVE.
    //
    // The `else` arm is the two-phase strand: returning here without
    // finish_parked_reap left reap_in_progress true and both pre-reserved
    // spares taken, permanently disabling reaping and then refusing every
    // enqueue at the PARKED_ENQUEUE_GATE. `dead` is already drained, so the
    // abort restores both vectors intact.
    let excess = adapter.with_virtio(move |v| v.recycle_dma_buffers(buffers));
    let Ok(mut excess) = excess else {
        let _ = adapter.with_virtio(move |v| v.abort_parked_reap(dead, alloc::vec::Vec::new()));
        return;
    };
    // Drop only the retained elements at PASSIVE while preserving the vector's
    // allocation for the next reap.
    excess.clear();
    let _ = adapter.with_virtio(move |v| v.finish_parked_reap(dead, excess));
}

/// A scan-out bind's mint, as passed down to the enqueue: where the minted wire
/// sequence goes, and which resource the command names.
///
/// The two travel together because they are published together, under the one
/// `virtio_lock` hold that enqueues the command — the sequence orders the
/// bookkeeping, the resource is the WIRE view of what is bound
/// (`AdapterContext::scanout_bind_wire_resource`), and neither is meaningful
/// against a different command's lock hold.
#[derive(Clone, Copy)]
struct BindMint<'a> {
    seq_out: &'a Cell<u64>,
    /// The `SET_SCANOUT_BLOB`'s own `resource_id`; 0 is the scan-out disable.
    resource_id: u32,
    /// The full presentation identity carried by a direct synchronous SET. It
    /// survives waiter abandonment in the in-flight tag so a late success can
    /// be applied and arm this request's exact flush from the DPC.
    request: Option<crate::virtio::ScanoutBindRequest>,
    timeline: Option<ScanoutSetTimeline>,
}

/// Caller-owned context for the synchronous `SET_SCANOUT_BLOB` timeline.
/// The wire sequence is still minted by `VirtioGpu::enqueue_sync` under
/// `virtio_lock`; this values-only context lets that exact publish and the
/// PASSIVE caller's eventual return retain the originating epoch/watermark.
#[derive(Clone, Copy)]
pub(crate) struct ScanoutSetTimeline {
    pub request: crate::virtio::ScanoutBindRequest,
    pub present_epoch: u64,
    pub carried_watermark: u64,
    pub flags: u32,
}

/// One synchronous control round-trip: `req` (+ optional second device-read
/// span `extra`) → device → `resp_out`. Blocks at PASSIVE until completion or
/// `timeout_ms`. On timeout the in-flight slot is abandoned (reaped when the
/// completion eventually arrives) — the transport is NOT poisoned.
/// `bind`, when supplied, is minted INSIDE the same `with_virtio` as the
/// successful enqueue (ROADMAP defect 0ab-C). Minting there and nowhere else is
/// what makes the sequence agree with the control queue's FIFO order, and
/// therefore with the order the host applies binds in. `None` for every command
/// that is not a `SET_SCANOUT_BLOB`.
fn ctrl_roundtrip(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    req: &[u8],
    extra: Option<&[u8]>,
    resp_out: &mut [u8],
    timeout_ms: u64,
    bind: Option<BindMint<'_>>,
) -> Result<(), VirtioError> {
    let in0_len = req.len();
    let in1_len = extra.map_or(0, |e| e.len());
    let resp_len = resp_out.len();
    if in0_len == 0 || resp_len == 0 {
        return Err(VirtioError::DeviceError);
    }
    reap_parked(passive, adapter);

    let total = in0_len + in1_len + resp_len;
    let mut meta = DmaBuffer::new(passive, total).ok_or(VirtioError::OutOfMemory)?;
    {
        let m = meta.as_mut_slice();
        m[..in0_len].copy_from_slice(req);
        if let Some(e) = extra {
            m[in0_len..in0_len + in1_len].copy_from_slice(e);
        }
    }

    // The wait block is created, initialised and dropped inside `with`, so it
    // is never nameable here: "registered before init" and "moved after init"
    // are not expressible. The abandon-on-timeout epilogue is the closure's
    // last statement, which is what keeps deregistration paired with the frame.
    SyncWaitBlock::with(|block| {
        // Enqueue, with PASSIVE backpressure while the queue is full.
        //
        // `meta` is carried as a loop value, not round-tripped through an
        // `Option`. The enqueue moves it into the closure and the QueueFull arm
        // reinitialises it before the back edge, which Rust's flow-sensitive move
        // checking accepts. A future retry arm that forgets to hand the buffer back
        // is then a *compile* error, where the take-then-expect this replaces was a
        // `KeBugCheck` inside a DDI on the next iteration.
        let mut budget = Budget::new(ENQUEUE_RETRY_MAX_MS);
        let token: SyncTicket = loop {
            let res = adapter.with_virtio(move |v| {
                v.drain_used();
                let queued = v.enqueue_sync(
                    meta,
                    in0_len,
                    in1_len,
                    resp_len,
                    block.as_ptr(),
                    bind.map(|bind| (bind.resource_id, bind.request)),
                    |resource_id| adapter.mint_scanout_bind_seq(resource_id),
                );
                // The sequence is minted by `enqueue_sync` after its descriptor
                // was accepted and before it is published to the device.  Keep
                // the caller's value in lockstep with the in-flight lifecycle
                // tag, so a late response after waiter abandonment can still
                // update the host-selection ledger.
                match queued {
                    Ok((ticket, seq)) => {
                        if let (Some(bind), Some(seq)) = (bind, seq) {
                            bind.seq_out.set(seq);
                            if let Some(timeline) = bind.timeline {
                                crate::ddi::scanout_timeline::note(
                                    crate::ddi::scanout_timeline::kind::SYNC_SET_PUBLISH,
                                    timeline.flags | crate::ddi::scanout_timeline::flag::SUCCESS,
                                    timeline.present_epoch,
                                    timeline.carried_watermark,
                                    seq,
                                    bind.resource_id,
                                    0,
                                );
                            }
                        }
                        Ok(ticket)
                    }
                    Err(error) => Err(error),
                }
            });
            match res {
                Err(_) => return Err(VirtioError::DeviceError), // transport gone
                Ok(Ok(ticket)) => break ticket,
                Ok(Err((m_back, VirtioError::QueueFull))) => {
                    meta = m_back;
                    if budget.charge_slice() {
                        return Err(VirtioError::QueueFull);
                    }
                    reap_parked(passive, adapter);
                    sleep_ms(passive, RETRY_SLICE_MS);
                }
                Ok(Err((_m, e))) => return Err(e), // dropped here at PASSIVE
            }
        };

        // Kept for the refusal breadcrumb: SyncTicket is move-only, so it is
        // consumed by abandon_sync and cannot be read afterwards.
        let token_value = token.raw();
        if !wait_block(passive, adapter, block, timeout_ms) {
            // Final race check + abandonment under the lock.
            // Three outcomes, not two. `unwrap_or(true)` folded Err(DeviceNotFound)
            // - the transport was torn down under us - into "already completed
            // successfully", which skipped the timeout counter and picked the wrong
            // error class. The fake-success half is masked here because all three
            // callers re-validate resp_is_ok on the returned bytes and a zeroed
            // response fails that, but the missing evidence was real.
            match adapter.with_virtio(|v| {
                v.drain_used();
                v.abandon_sync(token, block.as_ptr())
            }) {
                // The drain already signalled us; the response bytes are valid.
                Ok(SyncOutcome::AlreadyCompleted) => {}
                Ok(SyncOutcome::Abandoned) => {
                    CTRL_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed);
                    return Err(VirtioError::Timeout);
                }
                // NEW population. The token names an entry that is not this
                // waiter's, so `resp` was never written — do NOT copy it out.
                // The old bool folded this into "already completed" and handed
                // the caller a zeroed buffer.
                Ok(SyncOutcome::NotOurs) => {
                    crate::diag::record_named_bytes(b"CtNotOurs", u32::from(token_value));
                    return Err(VirtioError::DeviceError);
                }
                Err(_) => {
                    CTRL_TEARDOWN_ABANDONS.fetch_add(1, Ordering::Relaxed);
                    return Err(VirtioError::DeviceError);
                }
            }
        }
        block.copy_resp(resp_out);
        Ok(())
    })
}

/// Replies up to this many bytes (the capacity the device is told it may write)
/// land in a buffer on the caller's stack instead of the heap. 1 KiB covers the
/// reply of almost every RM call (header, `IoctlResp`, a few hundred bytes of
/// parameters) and is small next to a 24 KiB kernel stack.
const RAW_REPLY_STACK: usize = 1024;

/// `NvSpinUs` as read from the service key (clamped), or `u32::MAX` while unread.
/// Read at StartDevice ([`reread_spin_knob`]) and, if unread, at the first forward (PASSIVE);
/// mirrored as `NvSpinUs` on every read.
static NVRM_SPIN_US: AtomicU32 = AtomicU32::new(u32::MAX);
/// The spin governor's packed state (`helios_kmd_logic::nvrm_fastpath::spin`).
/// Plain load / store, never a read-modify-write: it is a heuristic, a lost
/// update costs nothing, and a locked instruction on a line every forwarding
/// thread shares is exactly what this path is trying to stop paying.
static NVRM_SPIN_STATE: AtomicU32 =
    AtomicU32::new(helios_kmd_logic::nvrm_fastpath::spin::initial());
/// Forwards whose pre-wait spin saw the reply (`NvSpinHit`) and whose spin gave up
/// (`NvSpinMis`). `hit / (hit + miss)` is the governor's input; both flat at 0
/// with forwards running means the spin is off (`NvSpinUs = 0`).
pub static NVRM_SPIN_HITS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_SPIN_MISSES: AtomicU32 = AtomicU32::new(0);
/// `BlbAbandoned`: blob teardowns of a destroyed device that were not sent to the host
/// because an earlier blob of the same device was ambiguous (a blit still in flight, a
/// failed release): the ambiguous one itself and every blob taken out of the table after it
/// (`release_blobs_for_owner_within`). Must read 0 on a healthy session.
pub static BLOB_SWEEP_ABANDONED: AtomicU32 = AtomicU32::new(0);

/// Read `NvSpinUs` (clamped), cache it and mirror it. PASSIVE.
fn read_spin_knob() -> u32 {
    use helios_kmd_logic::nvrm_fastpath::spin;
    let us = crate::diag::read_config_dword(crate::diag::knobs::NV_SPIN_US, spin::BUDGET_US_DEFAULT)
        .min(spin::BUDGET_US_MAX);
    NVRM_SPIN_US.store(us, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvSpinUs", us);
    us
}

/// Forget the cached `NvSpinUs` and read it again (StartDevice, PASSIVE): the static outlives a
/// `pnputil /restart-device`, and this knob used to need a driver reload.
pub(crate) fn reread_spin_knob() -> u32 {
    read_spin_knob()
}

/// The pre-wait spin budget in 100 ns units; 0 = disabled by the knob.
fn nvrm_spin_budget_100ns(_passive: PassiveLevel) -> u64 {
    use helios_kmd_logic::nvrm_fastpath::spin;
    let mut us = NVRM_SPIN_US.load(Ordering::Relaxed);
    if us == u32::MAX {
        us = read_spin_knob();
    }
    spin::budget_100ns(us)
}

/// Poll `block` for up to `budget_100ns`, at PASSIVE with no lock held, and
/// return whether the drain was seen starting to complete it.
///
/// This is a HINT and nothing leaves on it: whatever it returns, the caller goes
/// on to [`wait_block`], whose `KeWaitForSingleObject` is still the only
/// completion-side exit (see its doc for why a lock-free `done` read must not be
/// one). What the spin buys is that a reply landing within a few microseconds
/// finds the waiter running: the wait then returns at once, without arming a
/// timer, switching context, or waking a halted vCPU. The drain is driven by the
/// device interrupt as always; this never touches the transport or its lock, so
/// it cannot hold up the DPC that completes the call.
/// Most clock-read rounds one pre-wait spin may take (each round is
/// `POLLS_PER_CLOCK_READ` polls and one clock read): far above what a 50 us budget
/// needs even on a slow emulated timer, far below an unbounded spin.
const SPIN_MAX_ROUNDS: u32 = 20_000;

fn spin_for_completion(block: &WaitBlockRef<'_>, budget_100ns: u64) -> bool {
    /// Polls between two clock reads: the clock read costs more than a poll.
    const POLLS_PER_CLOCK_READ: u32 = 8;
    let mut qpc = 0u64;
    // SAFETY: a scalar time read, callable at any IRQL; it waits on nothing and
    // fills the valid out-pointer.
    let start = unsafe { KeQueryInterruptTimePrecise(&mut qpc) };
    // A hard cap on top of the clock: should the interrupt clock ever fail to
    // advance, this must not become an unbounded spin on the path of every forward.
    let mut rounds = 0u32;
    loop {
        rounds += 1;
        if rounds > SPIN_MAX_ROUNDS {
            return block.spin_hint_done();
        }
        for _ in 0..POLLS_PER_CLOCK_READ {
            if block.spin_hint_done() {
                return true;
            }
            core::hint::spin_loop();
        }
        // SAFETY: as above.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut qpc) };
        if now.wrapping_sub(start) >= budget_100ns {
            return block.spin_hint_done();
        }
    }
}

/// Forward one RM message VERBATIM and wait for the reply (HELIOS_ESCAPE_NVRM).
///
/// `req` is `MsgHeader | payload` exactly as the caller built it — nothing is
/// stamped or interpreted here — and the device's reply (`MsgHeader | payload`,
/// or the header-less stream of `GetSysFiles`) is written into `resp_out`, whose
/// length is the capacity the device is given. Returns how many reply bytes the
/// device wrote, which is never 0 on success.
///
/// The drain runs at DISPATCH under the device lock, so it cannot be trusted to
/// write `resp_out` (the runtime's copy of the escape's private data, whose
/// paging is not ours to assume): it copies the reply into a driver-owned
/// non-paged buffer instead (see `InFlightKind::Raw`), and the copy into
/// `resp_out` happens here, at PASSIVE, after the wake. That buffer is on this
/// frame for a reply up to [`RAW_REPLY_STACK`] bytes (the same standing the
/// `SyncWaitBlock`'s own response bytes have: the drain writes it only under the
/// lock and only while `waiter` is still set, and `abandon_sync` clears `waiter`
/// under that lock before this frame can pop), and otherwise on the heap. It is
/// never zero-filled: the drain writes exactly the bytes it reports in `used`,
/// and only those are read back.
///
/// A timeout abandons the entry; its eventual completion neither copies nor
/// signals. An RM call that outlives `timeout_ms` may still have taken effect on
/// the host, so the caller must treat the handle as indeterminate.
pub fn raw_roundtrip(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    req: &[u8],
    resp_out: &mut [u8],
    timeout_ms: u64,
) -> Result<usize, VirtioError> {
    const MH: usize = super::hal::MSG_HDR_LEN;
    let req_len = req.len();
    let want = resp_out.len();
    if req_len < MH || want < MH {
        return Err(VirtioError::DeviceError);
    }
    // What the device is told it may write: one byte more than a bare header, so
    // the reply is two non-empty descriptors (see `Chain::Raw`).
    let resp_len = want.max(MH + 1);
    // The reply's landing buffer, declared before the wait block so it outlives
    // the abandon path. Heap storage is reserved fallibly — the size is
    // caller-controlled — and only for a reply that does not fit the stack.
    let mut stack_reply = MaybeUninit::<[u8; RAW_REPLY_STACK]>::uninit();
    let mut heap_reply = Vec::<u8>::new();
    let dest = if resp_len <= RAW_REPLY_STACK {
        stack_reply.as_mut_ptr() as *mut u8
    } else {
        if heap_reply.try_reserve_exact(resp_len).is_err() {
            return Err(VirtioError::OutOfMemory);
        }
        heap_reply.as_mut_ptr()
    };
    let Some(dest) = NonNull::new(dest) else {
        return Err(VirtioError::DeviceError);
    };

    // One lock hold for what used to be two: whether completed entries wait to be
    // reaped (almost never now, see `drain_used`'s Raw arm) and a pooled buffer.
    let total = req_len + resp_len;
    let (work, pooled) =
        match adapter.with_virtio(|v| (v.begin_parked_reap(), v.take_dma_buffer(total))) {
            Ok((work, pooled)) => (Ok(work), pooled),
            Err(e) => (Err(e), None),
        };
    // Finish the reap BEFORE anything below can return, or `reap_in_progress`
    // stays set (see `reap_parked_work`).
    reap_parked_work(passive, adapter, work);
    let mut meta = pooled
        .or_else(|| DmaBuffer::new(passive, total))
        .ok_or(VirtioError::OutOfMemory)?;
    meta.as_mut_slice()[..req_len].copy_from_slice(req);

    let used = SyncWaitBlock::with(|block| {
        // As in `ctrl_roundtrip`: the buffer is a loop value, handed back by every
        // refusal arm, so a retry that forgets it does not compile.
        let mut budget = Budget::new(ENQUEUE_RETRY_MAX_MS);
        let ticket = loop {
            let res = adapter.with_virtio(move |v| {
                v.drain_used();
                v.enqueue_raw(meta, req_len, resp_len, block.as_ptr(), dest)
            });
            match res {
                Err(_) => return Err(VirtioError::DeviceError), // transport gone
                Ok(Ok(ticket)) => break ticket,
                Ok(Err((m_back, VirtioError::QueueFull))) => {
                    meta = m_back;
                    if budget.charge_slice() {
                        return Err(VirtioError::QueueFull);
                    }
                    reap_parked(passive, adapter);
                    sleep_ms(passive, RETRY_SLICE_MS);
                }
                Ok(Err((_m, e))) => return Err(e), // buffer dropped at PASSIVE
            }
        };

        // A short bounded poll before the real wait (see `spin_for_completion`).
        {
            use helios_kmd_logic::nvrm_fastpath::spin;
            let state = NVRM_SPIN_STATE.load(Ordering::Relaxed);
            if spin::should_spin(state) {
                let spin_budget = nvrm_spin_budget_100ns(passive);
                if spin_budget != 0 {
                    let hit = spin_for_completion(block, spin_budget);
                    let fresh = NVRM_SPIN_STATE.load(Ordering::Relaxed);
                    NVRM_SPIN_STATE.store(spin::next(fresh, true, hit), Ordering::Relaxed);
                    let counter = if hit {
                        &NVRM_SPIN_HITS
                    } else {
                        &NVRM_SPIN_MISSES
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                NVRM_SPIN_STATE.store(spin::next(state, false, false), Ordering::Relaxed);
            }
        }

        if !wait_block(passive, adapter, block, timeout_ms) {
            match adapter.with_virtio(|v| {
                v.drain_used();
                v.abandon_sync(ticket, block.as_ptr())
            }) {
                // The drain already completed it; the reply is in `dest`.
                Ok(SyncOutcome::AlreadyCompleted) => {}
                Ok(SyncOutcome::Abandoned) => {
                    CTRL_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed);
                    return Err(VirtioError::Timeout);
                }
                Ok(SyncOutcome::NotOurs) => return Err(VirtioError::DeviceError),
                Err(_) => return Err(VirtioError::DeviceError),
            }
        }
        match block.used() as usize {
            0 => Err(VirtioError::DeviceError),
            n => Ok(n),
        }
    })?;
    // PASSIVE: now the reply may go to the caller's buffer. `used` is what the
    // drain copied into `dest` and is at most `resp_len`, the size of `dest`.
    let n = used.min(want);
    // SAFETY: the drain initialised the first `used >= n` bytes of `dest` (and
    // no other writer exists once the wait was satisfied or the abandon found the
    // entry completed); `dest` and `resp_out` are distinct allocations; `n` is at
    // most both lengths.
    unsafe { core::ptr::copy_nonoverlapping(dest.as_ptr(), resp_out.as_mut_ptr(), n) };
    Ok(n)
}

/// Submit one RM message VERBATIM and do NOT wait for the reply (`ForeignFlip`'s pipelined
/// `ScanoutFlip`, `helios_kmd_logic::flip_pipeline`). `req` is `MsgHeader | payload` as for
/// [`raw_roundtrip`]; the reply is a bare header whose `status` the used-ring drain turns into
/// ONE word, `cell` (tagged `tag`: `flip_pipeline::pack_reply` / `pack_no_reply`), before it
/// signals the HPD worker's event. The caller zeroes `cell` BEFORE the call and settles it
/// later (`virtio/foreign_flip.rs`).
///
/// PASSIVE, one attempt: a full queue is returned as `QueueFull` at once (no sleeping retry
/// loop as the round trip has; the worker keeps the frame owed and comes back), an allocation
/// failure as `OutOfMemory`. On every error return nothing reached the ring and `cell` is
/// untouched. `cell` must be `'static` (it is read by the drain after this returns).
///
/// Timed on `clock` (the flip's `SubF*` table, docs 24.14): `Take` (the reap's begin and the pool
/// take, one lock hold), `Reap`, `Alloc` (a pool miss), `Copy`, then the enqueue hold's `Lock`,
/// `Drain`, `Enq`, `Kick` as for the display submitters.
pub fn raw_submit_async(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    req: &[u8],
    cell: &'static core::sync::atomic::AtomicU64,
    tag: u32,
    clock: &mut Clock,
) -> Result<(), VirtioError> {
    const MH: usize = super::hal::MSG_HDR_LEN;
    let req_len = req.len();
    if req_len < MH {
        return Err(VirtioError::DeviceError);
    }
    // A bare header plus one more descriptor's worth, as `raw_roundtrip` gives a header-only
    // reply (see `Chain::Raw`); the drain reads only the header's `status`.
    let resp_len = 2 * MH;
    let total = req_len + resp_len;
    let (work, pooled) =
        match adapter.with_virtio(|v| (v.begin_parked_reap(), v.take_dma_buffer(total))) {
            Ok((work, pooled)) => (Ok(work), pooled),
            Err(e) => (Err(e), None),
        };
    submit_stage::lap(clock, Stage::Take);
    reap_parked_work(passive, adapter, work);
    submit_stage::lap(clock, Stage::Reap);
    let meta = pooled
        .or_else(|| DmaBuffer::new(passive, total))
        .ok_or(VirtioError::OutOfMemory);
    submit_stage::lap(clock, Stage::Alloc);
    let mut meta = meta?;
    meta.as_mut_slice()[..req_len].copy_from_slice(req);
    submit_stage::lap(clock, Stage::Copy);
    let wake = NonNull::new(adapter.hpd_event.get()).ok_or(VirtioError::DeviceError)?;
    let cell = NonNull::from(cell);
    let queued = display_enqueue(passive, adapter, Path::Flip, clock, move |v| {
        v.enqueue_raw_async(meta, req_len, resp_len, cell, tag, wake)
    });
    match queued {
        Ok(Ok(())) => Ok(()),
        // The buffer comes back here, at PASSIVE, to be dropped.
        Ok(Err((_meta, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

/// Round-trip expecting a bare `VirtioGpuCtrlHdr` response; checks RESP_OK.
fn ctrl_roundtrip_ok(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    req: &[u8],
    extra: Option<&[u8]>,
) -> Result<(), VirtioError> {
    ctrl_roundtrip_ok_seq(passive, adapter, req, extra, None)
}

/// [`ctrl_roundtrip_ok`] plus the scan-out bind mint. Only the
/// `SET_SCANOUT_BLOB` caller passes one; every other command's semantics are
/// unchanged, because `None` skips the mint entirely.
fn ctrl_roundtrip_ok_seq(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    req: &[u8],
    extra: Option<&[u8]>,
    bind: Option<BindMint<'_>>,
) -> Result<(), VirtioError> {
    ctrl_roundtrip_ok_timed(
        passive,
        adapter,
        req,
        extra,
        bind,
        SYNC_ROUNDTRIP_TIMEOUT_MS,
    )
}

/// [`ctrl_roundtrip_ok_seq`] with an explicit wait budget, for the teardown
/// sweeps that must not wait the full 30 s on a host that is not answering.
fn ctrl_roundtrip_ok_timed(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    req: &[u8],
    extra: Option<&[u8]>,
    bind: Option<BindMint<'_>>,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let mut resp = [0u8; size_of::<VirtioGpuCtrlHdr>()];
    ctrl_roundtrip(passive, adapter, req, extra, &mut resp, timeout_ms, bind)?;
    let resp_type = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if resp_is_ok(resp_type) {
        Ok(())
    } else {
        Err(VirtioError::DeviceError)
    }
}

/// Wait until every control descriptor published before this call has reached a
/// terminal host response, without changing device state.
///
/// GET_CAPSET_INFO is a pure query. Its response type is deliberately not
/// validated here: even an error for capset index 0 proves the command reached
/// the head of the FIFO, which is the only property lifecycle callers need.
/// Transport enqueue/wait failure still returns `Err`, because then no ordering
/// proof exists. The small fixed request/response keep this barrier off the
/// already-constrained display-init stack.
#[inline(never)]
pub fn ctrl_fifo_barrier(
    passive: PassiveLevel,
    adapter: &AdapterContext,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuGetCapsetInfo::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_GET_CAPSET_INFO;
    cmd.capset_index = 0;
    let mut response = [0u8; size_of::<VirtioGpuRespCapsetInfo>()];
    ctrl_roundtrip(
        passive,
        adapter,
        bytes_of(&cmd),
        None,
        &mut response,
        SYNC_ROUNDTRIP_TIMEOUT_MS,
        None,
    )
}

// ── Context lifecycle ────────────────────────────────────────────────────────

/// Create a virtio-gpu 3D context bound to `capset_id` (Venus = 4) and return
/// the guest-assigned context id. `owner` is the D3D device handle recorded for
/// `DxgkDdiDestroyDevice` reclamation (0 = KMD-internal).
pub fn ctx_create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    capset_id: u32,
    owner: Option<DeviceOwner>,
) -> Result<u32, VirtioError> {
    let ctx_id = adapter
        .with_virtio(|v| v.alloc_ctx_id())
        .map_err(|_| VirtioError::DeviceError)?;
    // Reserve the tracking slot BEFORE the host round-trip: tracking is
    // mandatory, so a context this driver cannot track must not be created.
    let reserved = adapter
        .with_virtio(|v| v.reserve_context_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        return Err(VirtioError::OutOfMemory);
    }
    let mut cmd = VirtioGpuCtxCreate::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_CREATE;
    cmd.hdr.ctx_id = ctx_id;
    // With VIRTIO_GPU_F_CONTEXT_INIT, context_init carries the capset id.
    cmd.context_init = capset_id;
    // A debug name helps host-side (virglrenderer) logs; purely cosmetic.
    const NAME: &[u8] = b"helios";
    cmd.nlen = NAME.len() as u32;
    cmd.debug_name[..NAME.len()].copy_from_slice(NAME);
    crate::diag::record(0x0D20_0000 | (ctx_id & 0xFFFF));
    if let Err(e) = ctrl_roundtrip_ok(passive, adapter, bytes_of(&cmd), None) {
        let _ = adapter.with_virtio(|v| v.cancel_context_reservation());
        return Err(e);
    }
    crate::diag::record(0x0D21_0000 | (ctx_id & 0xFFFF));
    let _ = adapter.with_virtio(|v| v.commit_context(owner, ctx_id));
    Ok(ctx_id)
}

/// Destroy a context and drop its tracking slot, scoped to its owner.
///
/// The untrack and the ownership test are ONE step under the device lock, so a
/// racing CTX_DESTROY for the same id cannot have both callers pass the check.
/// A guest-supplied id that this owner does not own never reaches the wire:
/// before this, CTX_DESTROY took the raw id straight to the host, so process B
/// (or A after a restart that recycled the id) could destroy process A's Venus
/// context and A's next submit referenced a destroyed host context — CS error,
/// fatal decoder state (k-capsescape-02).
pub fn ctx_destroy(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
) -> Result<(), VirtioError> {
    ctx_destroy_within(passive, adapter, owner, ctx_id, None)
}

/// How long the next command of a teardown sweep may wait: the whole synchronous
/// timeout when there is no budget, the budget's per-call allowance otherwise,
/// and `None` once the budget is spent (send nothing more).
fn sweep_timeout_ms(budget: Option<&SweepBudget>) -> Option<u64> {
    match budget {
        None => Some(SYNC_ROUNDTRIP_TIMEOUT_MS),
        Some(b) => b.call_timeout_ms(crate::adapter::foreign_scanout::now_100ns()),
    }
}

/// [`ctx_destroy`] under an optional [`SweepBudget`]. With the budget spent the
/// context is still untracked (the table entry goes) but no `CTX_DESTROY` is sent;
/// the transport reset that follows a StopDevice sweep reclaims it.
pub fn ctx_destroy_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    budget: Option<&SweepBudget>,
) -> Result<(), VirtioError> {
    let owned = adapter
        .with_wddm_notify_lock(|guard| {
            guard.with_virtio(|order, v| {
                let owned = v.untrack_owned_context(owner, ctx_id);
                if let Some(ctx_id) = owned {
                    let _ = v.purge_present_streams_for_context(order, owner, ctx_id);
                }
                owned
            })
        })
        .map_err(|_| VirtioError::DeviceError)?;
    let Some(ctx_id) = owned else {
        return Err(VirtioError::NotOwned);
    };
    // The stream wait was explicitly discharged under the notify lock above.
    // Wake the normal DPC so a now-ready WDDM head is observed even if no
    // unrelated virtio completion arrives after CTX_DESTROY's roundtrip.
    crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
    let mut cmd = VirtioGpuCtxDestroy::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
    cmd.hdr.ctx_id = ctx_id;
    let result = match sweep_timeout_ms(budget) {
        Some(timeout_ms) => {
            ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
        }
        None => Err(VirtioError::Timeout),
    };
    if result.is_ok() {
        let finalized = adapter.with_wddm_notify_lock(|guard| {
            guard
                .with_virtio(|order, v| {
                    v.finalize_closed_present_streams_for_context(order, ctx_id)
                })
                .unwrap_or(0)
        });
        crate::ddi::dwm_restart::note_streams_finalized(finalized);
        if finalized != 0 {
            crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
            adapter.signal_hpd();
        }
    } else {
        // The host did not confirm: the stream slots this context closed stay `closing`, and the
        // consumer claims they carry stay pinned (nothing retries). `DwCtxFail`, and the census.
        crate::ddi::dwm_restart::note_ctx_destroy_failed();
    }
    result
}

/// Untracked teardown of a context this driver created for itself (the
/// persistent venus context, the virgl diagnostic contexts). Owner-scoped to
/// the KMD.
pub fn ctx_destroy_kmd(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    budget: Option<&SweepBudget>,
) -> Result<(), VirtioError> {
    ctx_destroy_within(passive, adapter, None, ctx_id, budget)
}

/// `CTX_DESTROY` every context still owned by `owner` (device teardown).
pub fn destroy_contexts_for_owner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
) -> u32 {
    let mut destroyed = 0u32;
    loop {
        let taken = adapter.with_wddm_notify_lock(|guard| {
            guard
                .with_virtio(|order, v| {
                    let taken = v.take_context_for_owner(owner);
                    if let Some(ctx_id) = taken {
                        let _ = v.purge_present_streams_for_context(order, owner, ctx_id);
                    }
                    taken
                })
                .unwrap_or(None)
        });
        let Some(ctx_id) = taken else {
            break;
        };
        let mut cmd = VirtioGpuCtxDestroy::zeroed();
        cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DESTROY;
        cmd.hdr.ctx_id = ctx_id;
        if ctrl_roundtrip_ok(passive, adapter, bytes_of(&cmd), None).is_ok() {
            let finalized = adapter.with_wddm_notify_lock(|guard| {
                guard
                    .with_virtio(|order, v| {
                        v.finalize_closed_present_streams_for_context(order, ctx_id)
                    })
                    .unwrap_or(0)
            });
            crate::ddi::dwm_restart::note_streams_finalized(finalized);
            if finalized != 0 {
                crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
                adapter.signal_hpd();
            }
        } else {
            // As in `ctx_destroy_within`: unconfirmed, nothing retries.
            crate::ddi::dwm_restart::note_ctx_destroy_failed();
        }
        destroyed += 1;
    }
    if destroyed != 0 {
        crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
    }
    destroyed
}

// ── Resource / blob lifecycle ────────────────────────────────────────────────

/// Attach a resource to a 3D context (`CTX_ATTACH_RESOURCE`).
pub fn ctx_attach_resource(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    ctx_attach_resource_within(
        passive,
        adapter,
        ctx_id,
        resource_id,
        SYNC_ROUNDTRIP_TIMEOUT_MS,
    )
}

/// [`ctx_attach_resource`] with an explicit wait, for a caller that runs on a thread
/// StopDevice joins (the HPD worker).
fn ctx_attach_resource_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuCtxResource::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
}

/// Detach a resource from a 3D context.
pub fn ctx_detach_resource(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    ctx_detach_resource_within(
        passive,
        adapter,
        ctx_id,
        resource_id,
        SYNC_ROUNDTRIP_TIMEOUT_MS,
    )
}

fn ctx_detach_resource_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuCtxResource::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
}

/// Bind a venus blob `resource_id` to scanout 0 (the QEMU gtk/sdl display) via
/// `SET_SCANOUT_BLOB` — the Phase-7 zero-copy display path (DISPLAY.md §8), now
/// driven from the WDDM VidPn scanout DDI. The blob must be a dmabuf-exportable
/// HOST3D resource (the host's venus render-server exports its `dmabuf_fd`, e.g.
/// via ANV); a non-exportable/wrong-layout resource is rejected host-side and
/// surfaces here as `VirtioError::DeviceError` — that IS the export-gate signal.
/// `stride`/`offset` are plane-0 geometry of the LINEAR image. Device-global
/// (`hdr.ctx_id = 0`). PASSIVE_LEVEL only (control round-trip).
///
/// Returns the WIRE-ORDER SEQUENCE this bind was minted with (ROADMAP defect
/// 0ab-C): the caller's post-response bookkeeping is only allowed to run if no
/// LATER bind has already applied its own — see
/// `AdapterContext::adopt_scanout_bind_seq`.
pub fn set_scanout_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
    offset: u32,
    timeline: Option<ScanoutSetTimeline>,
) -> Result<u64, VirtioError> {
    let mut cmd = VirtioGpuSetScanoutBlob::zeroed();
    fill_set_scanout_blob(&mut cmd, resource_id, width, height, format, stride, offset);
    let seq = Cell::new(0u64);
    // `resource_id` rides down to the mint: it is 0 for the scan-out DISABLE the
    // retire path sends, which is exactly what must land in the wire-resource
    // word — after a disable nothing is bound, so nothing may be skipped as
    // already bound.
    let bind = BindMint {
        seq_out: &seq,
        resource_id,
        request: timeline.map(|timeline| timeline.request),
        timeline,
    };
    let result = ctrl_roundtrip_ok_seq(passive, adapter, bytes_of(&cmd), None, Some(bind));
    if let Some(timeline) = timeline {
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::SYNC_SET_RETURN,
            timeline.flags
                | if result.is_ok() {
                    crate::ddi::scanout_timeline::flag::SUCCESS
                } else {
                    0
                },
            timeline.present_epoch,
            timeline.carried_watermark,
            seq.get(),
            resource_id,
            0,
        );
    }
    result?;
    Ok(seq.get())
}

/// `SET_SCANOUT_BLOB` with resource 0 (scanout 0 off), for StopDevice: the host keeps the scanout
/// binding of a transport it is about to lose only if nobody turns it off, and ids restart at 1 in
/// the next generation (docs/zero-copy-present.md section 25). Waits at most `timeout_ms` (the
/// StopDevice budget's share); it takes no wire-order mint, because the display state it would
/// update is zeroed by `reset_display_publication_state` right after and the transport is dropped.
/// PASSIVE_LEVEL only.
pub fn disable_scanout_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuSetScanoutBlob::zeroed();
    fill_set_scanout_blob(&mut cmd, 0, 0, 0, 0, 0, 0);
    ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
}

/// `HELIOS_CMD_SET_CURSOR_BLOB`: the hardware cursor's image, or its hiding
/// (`ddi::hw_cursor`; docs/SCANOUT.md "Hardware cursor, Windows guests"). `cmd.hdr` is filled
/// here. Waits at most `timeout_ms`: it runs inside `DxgkDdiSetPointerShape` /
/// `SetPointerPosition`, which must not hang the desktop on a wedged host. PASSIVE_LEVEL only.
pub fn set_cursor_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    mut cmd: HeliosSetCursorBlob,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    cmd.hdr = VirtioGpuCtrlHdr::zeroed();
    cmd.hdr.type_ = HELIOS_CMD_SET_CURSOR_BLOB;
    ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
}

/// Encode one `SET_SCANOUT_BLOB` into `cmd`, whoever owns the storage.
///
/// THE ONE ENCODER. Its two callers are the synchronous round-trip above, which
/// stages the command on its PASSIVE stack, and the DISPATCH-level fast bind,
/// which writes it straight into the transport's preallocated DMA buffer — so
/// the two commands are byte-identical by construction rather than by two
/// copies of the same twelve field assignments.
///
/// Every field is written, including the zeros: `cmd` may be a recycled buffer
/// whose previous contents are a different bind, and a stale `strides[1]` would
/// be read by QEMU as a real plane.
///
/// IRQL-free (plain field stores, no allocation, no round-trip), which is why it
/// takes no [`PassiveLevel`] unlike everything else in this module.
pub(crate) fn fill_set_scanout_blob(
    cmd: &mut VirtioGpuSetScanoutBlob,
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
    offset: u32,
) {
    cmd.hdr = VirtioGpuCtrlHdr::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_SET_SCANOUT_BLOB;
    cmd.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    cmd.scanout_id = 0;
    cmd.resource_id = resource_id;
    cmd.width = width;
    cmd.height = height;
    cmd.format = format;
    cmd.padding = 0;
    cmd.strides = [stride, 0, 0, 0];
    cmd.offsets = [offset, 0, 0, 0];
}

/// Queue a RESOURCE_FLUSH without synchronously waiting for its ctrl response.
/// The used-ring drain validates the response, clears `completion`, and wakes
/// `wake_event`.  This is intentionally limited to scanout refresh: unlike
/// lifecycle commands, the caller does not need response data before it can
/// continue, and blocking here previously imposed the observed ~0.41 s/frame
/// cadence when ctrl interrupts were delayed.
///
/// `scanout_flush` is the presentation-ownership token (ROADMAP defect 0ab-B):
/// the epoch this command's host read covers. It is CONSUMED by the used-ring
/// drain. On every error return below the command never reaches the ring, so no
/// host read exists for that epoch — the caller
/// (`queue_active_scanout_refresh_locked`) ends the lease explicitly, which is
/// why the token being dropped here is correct rather than a leak.
pub fn resource_flush_async(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
    completion: NonNull<AtomicU32>,
    completion_errors: NonNull<AtomicU32>,
    wake_event: NonNull<KEVENT>,
    scanout_flush: crate::virtio::ScanoutFlushToken,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceFlush::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_FLUSH;
    cmd.r = VirtioGpuRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    cmd.resource_id = resource_id;

    reap_parked(passive, adapter);
    let request = bytes_of(&cmd);
    let response_len = size_of::<VirtioGpuCtrlHdr>();
    let mut meta =
        DmaBuffer::new(passive, request.len() + response_len).ok_or(VirtioError::OutOfMemory)?;
    meta.as_mut_slice()[..request.len()].copy_from_slice(request);

    let queued = adapter.with_virtio(move |v| {
        v.drain_used();
        v.enqueue_async_control(
            meta,
            request.len(),
            response_len,
            completion,
            completion_errors,
            wake_event,
            None,
            None,
            Some(scanout_flush),
        )
    });
    match queued {
        Ok(Ok(())) => Ok(()),
        Ok(Err((_meta, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

/// Drop the host's reference to a resource.
pub fn resource_unref(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<(), VirtioError> {
    resource_unref_within(passive, adapter, resource_id, SYNC_ROUNDTRIP_TIMEOUT_MS)
}

fn resource_unref_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceUnref::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNREF;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
}

/// Attach an EXISTING live resource id to a context without taking ownership
/// (the DXVK/Mesa shared-resource import path). C1: liveness is validated
/// against the KMD's authoritative table BEFORE sending — the host attach path
/// cannot be trusted to fail (`virgl_renderer_ctx_attach_resource` is void and
/// silently no-ops on an unknown resource; QEMU still replies OK_NODATA).
pub fn attach_resource_checked(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    let live = adapter
        .with_virtio(|v| v.resource_is_live(resource_id))
        .map_err(|_| VirtioError::DeviceError)?;
    if !live {
        crate::diag::record(0x0E09_0000 | (resource_id & 0xFFFF));
        return Err(VirtioError::DeviceError);
    }
    ctx_attach_resource(passive, adapter, ctx_id, resource_id)
}

/// Create a HOST3D virtio-gpu blob resource in venus context `ctx_id`,
/// referencing venus device-memory `blob_id`, and attach it to the context.
/// Returns the guest-assigned resource id. Mirrors the proven System-class
/// `kmd::alloc_blob` sequence (create_blob → ctx_attach_resource); the
/// live-resource table slot is reserved up front so an untracked-but-live
/// resource can never exist.
pub fn resource_create_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
) -> Result<u32, VirtioError> {
    resource_create_blob_errno(
        passive, adapter, ctx_id, blob_mem, blob_flags, blob_id, size, None,
    )
}

/// [`resource_create_blob`], plus the host's errno when it refuses the create:
/// the host reports one only for the RM-export blob type
/// (`foreign_errno::from_resp_hdr`). With `errno_out == None` this is exactly the
/// plain call.
#[allow(clippy::too_many_arguments)]
fn resource_create_blob_errno(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    errno_out: Option<&mut u32>,
) -> Result<u32, VirtioError> {
    resource_create_blob_errno_within(
        passive, adapter, ctx_id, blob_mem, blob_flags, blob_id, size, errno_out, None,
    )
}

/// [`resource_create_blob_errno`] under an optional [`SweepBudget`]: the create, the
/// attach and (on an attach that failed) the unref share it, so the whole call is over
/// when the budget is, and with it spent nothing more is sent (`Timeout`). `None` is
/// the plain call (each command waits the whole synchronous timeout).
#[allow(clippy::too_many_arguments)]
fn resource_create_blob_errno_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    errno_out: Option<&mut u32>,
    budget: Option<&SweepBudget>,
) -> Result<u32, VirtioError> {
    let Some(create_timeout_ms) = sweep_timeout_ms(budget) else {
        return Err(VirtioError::Timeout);
    };
    let reserved = adapter
        .with_virtio(|v| v.reserve_resource_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        return Err(VirtioError::OutOfMemory);
    }
    let resource_id = match adapter.with_virtio(|v| v.alloc_resource_id()) {
        Ok(id) => id,
        Err(_) => {
            let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
            return Err(VirtioError::DeviceError);
        }
    };
    let mut cmd = VirtioGpuResourceCreateBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    cmd.blob_mem = blob_mem;
    cmd.blob_flags = blob_flags;
    cmd.nr_entries = 0;
    cmd.blob_id = blob_id;
    cmd.size = size;
    let mut resp = [0u8; size_of::<VirtioGpuCtrlHdr>()];
    let sent = ctrl_roundtrip(
        passive,
        adapter,
        bytes_of(&cmd),
        None,
        &mut resp,
        create_timeout_ms,
        None,
    );
    let created = match sent {
        Ok(_) => {
            let t = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
            if resp_is_ok(t) {
                Ok(())
            } else {
                if let Some(e) = errno_out {
                    *e = helios_kmd_logic::foreign_errno::from_resp_hdr(&resp);
                }
                Err(VirtioError::DeviceError)
            }
        }
        Err(e) => Err(e),
    };
    if let Err(e) = created {
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        return Err(e);
    }
    let attached = match sweep_timeout_ms(budget) {
        Some(timeout_ms) => {
            ctx_attach_resource_within(passive, adapter, ctx_id, resource_id, timeout_ms)
        }
        None => Err(VirtioError::Timeout),
    };
    if let Err(e) = attached {
        // The resource exists host-side but could not attach: drop it so it
        // does not leak untracked (with the budget spent the transport reset that
        // follows a stop reclaims it).
        if let Some(timeout_ms) = sweep_timeout_ms(budget) {
            let _ = resource_unref_within(passive, adapter, resource_id, timeout_ms);
        }
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        return Err(e);
    }
    let _ = adapter.with_virtio(|v| v.commit_resource(resource_id));
    Ok(resource_id)
}

/// What a refused guest-blob create answered (`create_guest_blob`).
#[derive(Clone, Copy)]
pub(crate) enum GuestBlobCreateError {
    /// The host answered `resp_type` with `errno` in the response header.
    Host { resp_type: u32, errno: u32 },
    /// The command was not sent (no transport, no memory for the request, a full queue).
    Transport,
    /// The command may have reached the host and got no answer in time (a timeout, an
    /// abandoned wait): the host may still create the blob over the pages.
    Unanswered,
    /// The live-resource table is full.
    NoSlot,
}

/// `RESOURCE_CREATE_BLOB` of a GUEST blob over the page runs `entries` (`nr_entries`
/// `virtio_gpu_mem_entry`s, already encoded, following the 56-byte create in the same request),
/// `size` bytes (their sum), in Venus context `ctx_id`. Every field the host defines comes from
/// `helios_kmd_logic::guest_blob::contract`. No `CTX_ATTACH_RESOURCE` follows: the host
/// attaches a guest blob to `hdr.ctx_id` itself. The resource is committed to the live-resource
/// table, so [`release_guest_blob_within`] can claim it exactly once. Waits at most
/// `timeout_ms` for the answer (`helios_kmd_logic::guest_blob::deadline::CREATE_MS`). PASSIVE
/// only.
pub(crate) fn create_guest_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    entries: &[u8],
    nr_entries: u32,
    size: u64,
    timeout_ms: u64,
) -> Result<u32, GuestBlobCreateError> {
    use helios_kmd_logic::guest_blob::contract as gb;
    if entries.is_empty() || size == 0 || ctx_id == 0 {
        return Err(GuestBlobCreateError::Transport);
    }
    let reserved = adapter
        .with_virtio(|v| v.reserve_resource_slot())
        .map_err(|_| GuestBlobCreateError::Transport)?;
    if !reserved {
        return Err(GuestBlobCreateError::NoSlot);
    }
    let resource_id = match adapter.with_virtio(|v| v.alloc_resource_id()) {
        Ok(id) => id,
        Err(_) => {
            let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
            return Err(GuestBlobCreateError::Transport);
        }
    };
    let mut cmd = VirtioGpuResourceCreateBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
    cmd.hdr.ctx_id = ctx_id;
    cmd.resource_id = resource_id;
    cmd.blob_mem = gb::BLOB_MEM;
    cmd.blob_flags = gb::BLOB_FLAGS;
    cmd.nr_entries = nr_entries;
    cmd.blob_id = gb::BLOB_ID;
    cmd.size = size;
    const _: () = assert!(
        size_of::<VirtioGpuResourceCreateBlob>()
            == helios_kmd_logic::guest_blob::contract::CREATE_BYTES
    );
    let mut resp = [0u8; size_of::<VirtioGpuCtrlHdr>()];
    let sent = ctrl_roundtrip(
        passive,
        adapter,
        bytes_of(&cmd),
        Some(entries),
        &mut resp,
        timeout_ms,
        None,
    );
    let outcome = match sent {
        Ok(()) => {
            let t = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
            if resp_is_ok(t) {
                Ok(())
            } else {
                Err(GuestBlobCreateError::Host {
                    resp_type: t,
                    errno: helios_kmd_logic::foreign_errno::from_resp_hdr(&resp),
                })
            }
        }
        // Never enqueued: nothing reached the host.
        Err(VirtioError::QueueFull | VirtioError::OutOfMemory) => {
            Err(GuestBlobCreateError::Transport)
        }
        // A timeout (the wait was abandoned, the command stays queued) or an abandon at
        // teardown: the host may still run it. The caller keeps the pages pinned.
        Err(_) => Err(GuestBlobCreateError::Unanswered),
    };
    if let Err(e) = outcome {
        // A refused create made nothing on the host; an unanswered one is reclaimed by the
        // transport reset that ends the generation (its pages stay pinned until then).
        let _ = adapter.with_virtio(|v| v.cancel_resource_reservation());
        return Err(e);
    }
    let _ = adapter.with_virtio(|v| v.commit_resource(resource_id));
    Ok(resource_id)
}

/// `RESOURCE_UNREF` of a guest blob made by [`create_guest_blob`], once (the live-resource
/// entry is the guard), waiting at most `timeout_ms` for the answer
/// (`helios_kmd_logic::guest_blob::deadline::UNREF_MS`). After a successful return the host
/// holds no mapping or pin of its pages. `Ok` also when another path already claimed it; an
/// unanswered UNREF is NOT retried (the claim is spent): its pages stay pinned until the
/// generation ends. PASSIVE only.
pub(crate) fn release_guest_blob_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let first = adapter
        .with_virtio(|v| v.take_live_resource(resource_id))
        .map_err(|_| VirtioError::DeviceError)?;
    if !first {
        return Ok(());
    }
    resource_unref_within(passive, adapter, resource_id, timeout_ms)
}

/// `HELIOS_ESCAPE_ALLOC_BLOB` — create a HOST3D blob (create + attach) and
/// record it in the blob table. Returns the resource id.
pub fn alloc_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    owner: Option<DeviceOwner>,
) -> Result<u32, VirtioError> {
    alloc_blob_errno(
        passive, adapter, ctx_id, blob_mem, blob_flags, blob_id, size, owner, None,
    )
}

/// [`alloc_blob`] that also reports the host's errno for a refused create (see
/// [`resource_create_blob_errno`]).
#[allow(clippy::too_many_arguments)]
pub fn alloc_blob_errno(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    owner: Option<DeviceOwner>,
    errno_out: Option<&mut u32>,
) -> Result<u32, VirtioError> {
    alloc_blob_errno_within(
        passive, adapter, ctx_id, blob_mem, blob_flags, blob_id, size, owner, errno_out, None,
    )
}

/// [`alloc_blob_errno`] under an optional [`SweepBudget`] shared by every command of
/// the call (create, attach, and the unref of a failed attach): for the KMD's own RM
/// client, which runs on the HPD worker StopDevice joins for a bounded time.
#[allow(clippy::too_many_arguments)]
pub fn alloc_blob_errno_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    owner: Option<DeviceOwner>,
    errno_out: Option<&mut u32>,
    budget: Option<&SweepBudget>,
) -> Result<u32, VirtioError> {
    if size == 0 {
        return Err(VirtioError::DeviceError);
    }
    let reserved = adapter
        .with_virtio(|v| v.reserve_blob_slot())
        .map_err(|_| VirtioError::DeviceError)?;
    if !reserved {
        return Err(VirtioError::OutOfMemory);
    }
    match resource_create_blob_errno_within(
        passive, adapter, ctx_id, blob_mem, blob_flags, blob_id, size, errno_out, budget,
    ) {
        Ok(resource_id) => {
            let _ = adapter.with_virtio(|v| v.commit_blob(owner, ctx_id, resource_id, size));
            Ok(resource_id)
        }
        Err(e) => {
            let _ = adapter.with_virtio(|v| v.cancel_blob_reservation());
            Err(e)
        }
    }
}

/// `RESOURCE_MAP_BLOB` round-trip; returns the host caching nibble.
fn resource_map_blob_roundtrip(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    offset: u64,
    timeout_ms: u64,
) -> Result<u32, VirtioError> {
    let mut cmd = VirtioGpuResourceMapBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB;
    cmd.resource_id = resource_id;
    cmd.offset = offset;
    let mut resp = [0u8; size_of::<VirtioGpuRespMapInfo>()];
    ctrl_roundtrip(
        passive,
        adapter,
        bytes_of(&cmd),
        None,
        &mut resp,
        timeout_ms,
        None,
    )?;
    let resp_type = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if !resp_is_ok(resp_type) {
        return Err(VirtioError::DeviceError);
    }
    // VirtioGpuRespMapInfo = { hdr: VirtioGpuCtrlHdr, map_info: u32, .. }.
    let off = size_of::<VirtioGpuCtrlHdr>();
    let map_info = u32::from_le_bytes([resp[off], resp[off + 1], resp[off + 2], resp[off + 3]]);
    Ok(map_info & VIRTIO_GPU_MAP_CACHE_MASK)
}

/// Tear down a blob's host-visible mapping.
pub fn resource_unmap_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<(), VirtioError> {
    resource_unmap_blob_within(passive, adapter, resource_id, SYNC_ROUNDTRIP_TIMEOUT_MS)
}

/// [`resource_unmap_blob`] under a [`SweepBudget`]: with it spent nothing is sent (`Timeout`).
pub fn resource_unmap_blob_budgeted(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    budget: &SweepBudget,
) -> Result<(), VirtioError> {
    match sweep_timeout_ms(Some(budget)) {
        Some(timeout_ms) => resource_unmap_blob_within(passive, adapter, resource_id, timeout_ms),
        None => Err(VirtioError::Timeout),
    }
}

fn resource_unmap_blob_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    let mut cmd = VirtioGpuResourceUnmapBlob::zeroed();
    cmd.hdr.type_ = VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB;
    cmd.resource_id = resource_id;
    ctrl_roundtrip_ok_timed(passive, adapter, bytes_of(&cmd), None, None, timeout_ms)
}

/// Map a blob into the host-visible window (idempotent — returns the existing
/// mapping if present). `owner = Some(o)` is the owner-scoped escape path;
/// `None` resolves by resource id alone (the GDI executor / kernel path).
pub fn map_blob_prepare(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: OwnerFilter,
    resource_id: u32,
) -> Result<BlobMapPrep, VirtioError> {
    map_blob_prepare_within(passive, adapter, owner, resource_id, None)
}

/// [`map_blob_prepare`] under an optional [`SweepBudget`] (for the KMD's own presenter
/// on the HPD worker): the wait for a busy slot and the `RESOURCE_MAP_BLOB` round trip
/// both end with it (`Timeout`); `None` is the plain call. A mapping that already exists
/// returns at once either way, with no round trip.
pub fn map_blob_prepare_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: OwnerFilter,
    resource_id: u32,
    budget: Option<&SweepBudget>,
) -> Result<BlobMapPrep, VirtioError> {
    let mut busy = Budget::new(MAP_BUSY_MAX_MS);
    loop {
        let begin = adapter
            .with_virtio(|v| v.blob_map_begin(owner, resource_id))
            .map_err(|_| VirtioError::DeviceError)?;
        match begin {
            BlobMapBegin::Mapped(prep) => return Ok(prep),
            BlobMapBegin::Failed(e) => return Err(e),
            BlobMapBegin::Busy => {
                if busy.charge_slice() || sweep_timeout_ms(budget).is_none() {
                    return Err(VirtioError::Timeout);
                }
                sleep_ms(passive, RETRY_SLICE_MS);
            }
            BlobMapBegin::Start { offset, len } => {
                // With the budget spent no host command is sent; the reserved range is
                // given back by `blob_map_finish` (the host rejected it, as for any
                // failed map).
                let cache = match sweep_timeout_ms(budget) {
                    Some(timeout_ms) => resource_map_blob_roundtrip(
                        passive,
                        adapter,
                        resource_id,
                        offset,
                        timeout_ms,
                    ),
                    None => Err(VirtioError::Timeout),
                };
                let cache_ok = cache.as_ref().ok().copied();
                let fin = adapter
                    .with_virtio(|v| v.blob_map_finish(resource_id, offset, len, cache_ok))
                    .map_err(|_| VirtioError::DeviceError)?;
                return match fin {
                    BlobMapFinish::Done(prep) => Ok(prep),
                    BlobMapFinish::HostRejected => {
                        Err(cache.err().unwrap_or(VirtioError::DeviceError))
                    }
                    BlobMapFinish::SlotGone => {
                        // Owner teardown raced the map: undo the host mapping
                        // and return the reserved range.
                        let _ = resource_unmap_blob_within(
                            passive,
                            adapter,
                            resource_id,
                            sweep_timeout_ms(budget).unwrap_or(1),
                        );
                        let _ = adapter.with_virtio(|v| v.free_window_range_pub(offset, len));
                        Err(VirtioError::DeviceError)
                    }
                };
            }
        }
    }
}

/// Map a blob at the FIXED window offset VidMm assigned (the CPU-visible BAR
/// memory segment, `build_paging_buffer.rs`). Inverts the normal order: instead
/// of the KMD allocator picking the offset, the blob is placed where VidMm put
/// the allocation, so CPU raster (through the segment's CpuTranslatedAddress),
/// the GDI executor, and the host all address the same bytes.
///
/// A pre-existing mapping at another offset is torn down first (blob content is
/// intrinsic to the host memory object — a remap is content-preserving), and
/// any STALE other-blob mapping overlapping the target range (an eviction this
/// driver missed) is unmapped so host window subregions never overlap.
/// PASSIVE_LEVEL only (host round-trips).
pub fn map_blob_at(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    window_offset: u64,
) -> Result<BlobMapPrep, VirtioError> {
    // Evict stale overlapping placements before reserving our own slot.
    let (_, blob_size, _) = adapter
        .with_virtio(|v| v.blob_lookup(resource_id))
        .map_err(|_| VirtioError::DeviceError)?
        .ok_or(VirtioError::DeviceError)?;
    let map_len = blob_size.saturating_add(4095) & !4095;
    let mut stale = [0u32; 8];
    // Two swallows used to live on this line. `.unwrap_or(0)` turned a torn-down
    // transport into "no stale placements", skipping the eviction pass entirely
    // instead of failing the map; and the scan itself silently stopped recording
    // once `stale` was full, so a ninth overlapping mapping was neither unmapped
    // nor reported and the RESOURCE_MAP_BLOB below created the overlapping host
    // window subregion this pass exists to prevent. Both are now hard failures:
    // refusing the aperture map / paging op is strictly better than two host
    // resources sharing one window subregion (k-gputransport-04).
    let n = adapter
        .with_virtio(|v| v.blobs_overlapping(window_offset, map_len, resource_id, &mut stale))
        .map_err(|_| VirtioError::DeviceError)?
        .map_err(|_truncated| VirtioError::DeviceError)?;
    for &res in stale[..n].iter() {
        let _ = resource_unmap_blob(passive, adapter, res);
        let _ = adapter.with_virtio(|v| v.blob_note_unmapped(res));
    }

    let mut busy = Budget::new(MAP_BUSY_MAX_MS);
    loop {
        let begin = adapter
            .with_virtio(|v| v.blob_remap_begin(resource_id, window_offset))
            .map_err(|_| VirtioError::DeviceError)?;
        match begin {
            BlobRemapBegin::Mapped(prep) => return Ok(prep),
            BlobRemapBegin::Failed(e) => return Err(e),
            BlobRemapBegin::Busy => {
                if busy.charge_slice() {
                    return Err(VirtioError::Timeout);
                }
                sleep_ms(passive, RETRY_SLICE_MS);
            }
            BlobRemapBegin::Start { old, len } => {
                if let Some((old_offset, old_len)) = old {
                    // Content-preserving move: unmap the previous placement and
                    // (for KMD-partition offsets only — the free guard ignores
                    // VidMm-partition ones) return its range.
                    let _ = resource_unmap_blob(passive, adapter, resource_id);
                    let _ = adapter.with_virtio(|v| v.free_window_range_pub(old_offset, old_len));
                }
                let cache = resource_map_blob_roundtrip(
                    passive,
                    adapter,
                    resource_id,
                    window_offset,
                    SYNC_ROUNDTRIP_TIMEOUT_MS,
                );
                let cache_ok = cache.as_ref().ok().copied();
                let fin = adapter
                    .with_virtio(|v| v.blob_map_finish(resource_id, window_offset, len, cache_ok))
                    .map_err(|_| VirtioError::DeviceError)?;
                return match fin {
                    BlobMapFinish::Done(prep) => Ok(prep),
                    BlobMapFinish::HostRejected => {
                        Err(cache.err().unwrap_or(VirtioError::DeviceError))
                    }
                    BlobMapFinish::SlotGone => {
                        let _ = resource_unmap_blob(passive, adapter, resource_id);
                        Err(VirtioError::DeviceError)
                    }
                };
            }
        }
    }
}

/// `HELIOS_ESCAPE_RELEASE_BLOB` — unmap (if mapped) + detach + unref a blob and
/// drop its tracking slot, returning its window range to the free list.
pub fn release_blob_for_owner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    ctx_id: u32,
    resource_id: u32,
) -> Result<(), VirtioError> {
    release_blob_for_owner_within(passive, adapter, owner, ctx_id, resource_id, None)
}

/// [`release_blob_for_owner`] under an optional [`SweepBudget`] shared by its commands
/// (unmap, detach, unref). With the budget spent the slot is still taken out of the
/// table, but the commands not yet sent are not (the transport reset reclaims the host
/// side) and the answer is `Timeout`. `None` is the plain call.
pub fn release_blob_for_owner_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    ctx_id: u32,
    resource_id: u32,
    budget: Option<&SweepBudget>,
) -> Result<(), VirtioError> {
    let taken = adapter
        .with_virtio(|v| v.take_blob_matching(owner, ctx_id, resource_id))
        .map_err(|_| VirtioError::DeviceError)?;
    let Some((res, mapped, map_offset, map_len)) = taken else {
        crate::virtio::foreign::RELEASE_DUP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return Ok(());
    };
    // A snapshot/DWM resource cannot detach while a deferred WindowedBlt
    // still owns its reader lease or reusable Venus command. Cancellation is
    // exact by resource id; cache release runs before detach/unref.
    // The abortable acquire (v334, `ddi::escape_wait`): this is the RELEASE_BLOB escape's path. A
    // thread that gave up leaves the blob out of the table and on the host (the transport reset
    // reclaims it), the same outcome as a host that never answers, and returns an error.
    let terminal = adapter
        .try_with_scanout_lifecycle(passive, |lock| -> Result<(), VirtioError> {
        lock.with_venus_client(|client| {
            let _ = adapter.with_virtio(|v| v.cancel_windowed_blt_for_resource(adapter, res));
            client.release_present_blits_for_resource(adapter, res)
        })
        .map_err(|_| VirtioError::DeviceError)??;

        // Hold the lifecycle gate until the exact reader terminal has
        // been established.  The Venus guard is released above, before
        // this virtio-only finalization, preserving lock order.
        if adapter
            .with_virtio(|v| v.finish_windowed_blt_teardown_for_resource(adapter, res))
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(VirtioError::DeviceError)
        }
    })
        .unwrap_or(Err(VirtioError::Timeout));
    terminal?;
    if mapped {
        if let Some(timeout_ms) = sweep_timeout_ms(budget) {
            let _ = resource_unmap_blob_within(passive, adapter, res, timeout_ms);
        }
        let _ = adapter.with_virtio(|v| v.free_window_range_pub(map_offset, map_len));
    }
    let first_teardown = adapter
        .with_virtio(|v| v.take_live_resource(res))
        .unwrap_or(false);
    let result = if first_teardown {
        if let Some(timeout_ms) = sweep_timeout_ms(budget) {
            let _ = ctx_detach_resource_within(passive, adapter, ctx_id, res, timeout_ms);
        }
        match sweep_timeout_ms(budget) {
            Some(timeout_ms) => resource_unref_within(passive, adapter, res, timeout_ms),
            None => Err(VirtioError::Timeout),
        }
    } else {
        Ok(())
    };
    // D4b: reclaim this resid's D4a read-ledger slot. Snapshot resids never
    // pass through `retire_scanout_allocation` (they have no WDDM allocation),
    // so without this the 8-slot ledger leaks one slot per snapshot-ring
    // recreate and `RdOvf` climbs across app restarts. Under the scanout
    // lifecycle mutex because `ReadLedger::issue`'s claim discipline is
    // serialized by it (see `note_alloc_retired`'s contract); PASSIVE here, no
    // other lock held, so the acquisition is legal and unordered against
    // nothing. A resid with no ledger slot no-ops.
    let _ = adapter.try_with_scanout_lifecycle(passive, |_lock| {
        adapter.read_ledger.note_alloc_retired(res);
    });
    result
}

/// Reclaim every blob still owned by `owner` (a destroyed D3D device handle):
/// unmap (if mapped), detach, unref, and return the window range. KMD-side
/// safety net for an ICD that crashes or skips RELEASE_BLOB. Returns the count.
pub fn release_blobs_for_owner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
) -> u32 {
    release_blobs_for_owner_within(passive, adapter, owner, None)
}

/// [`release_blobs_for_owner`] under an optional [`SweepBudget`] (StopDevice's
/// KMD-owned sweep). Once the budget is spent the remaining slots are still taken
/// out of the table, but no host command (`UNMAP_BLOB`, detach, unref) is sent for
/// them: the transport reset that follows reclaims the host side.
pub fn release_blobs_for_owner_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    budget: Option<&SweepBudget>,
) -> u32 {
    let mut reclaimed = 0u32;
    // Set once one blob's teardown was ambiguous (see below): the rest of the owner's table
    // entries are still taken out, so a dead owner cannot hold blob slots for ever, but they
    // are not sent to the host (the first ambiguity already says the host or an in-flight
    // blit cannot be trusted to answer), and only the host-free part runs for them.
    let mut abandoned = false;
    loop {
        let taken = adapter
            .with_virtio(|v| v.take_blob_for_owner(owner))
            .unwrap_or(None);
        let Some((ctx_id, res, mapped, map_offset, map_len)) = taken else {
            return reclaimed;
        };
        if abandoned {
            // Undispatched windowed blts that name this resource have no host reader: end
            // them, or each keeps a ledger ticket and one of the 64 token slots for ever.
            // Nothing else: the host objects stay until the Venus teardown, as for the
            // blob that was ambiguous.
            let _ = adapter.with_virtio(|v| v.cancel_windowed_blt_for_resource(adapter, res));
            // And the resid's claim on the 8-slot read ledger: skipping it leaked one slot
            // per abandoned blob, until `queue_windowed_blt` could issue no ticket at all.
            // `note_alloc_retired` pins the claim while a reader is still active and
            // reclaims it when the last ticket retires, so it is safe for any state.
            retire_ledger_claim(passive, adapter, res);
            BLOB_SWEEP_ABANDONED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let terminal = adapter.with_scanout_lifecycle(passive, |lock| {
            let cache_release = lock.with_venus_client(|client| {
                let _ = adapter.with_virtio(|v| v.cancel_windowed_blt_for_resource(adapter, res));
                client.release_present_blits_for_resource(adapter, res)
            });
            match cache_release {
                Ok(Ok(())) => {}
                // The release itself failed: ambiguous drain, retain (below).
                Ok(Err(_)) => return false,
                // `NotStarted`: there is no venus client, so there are no Present
                // blits cached for this resource to release. StopDevice drops the
                // client BEFORE this sweep on purpose (its ring/reply mappings are
                // unmapped first, and the ring blob is itself a KMD blob this sweep
                // frees), so for that caller this arm is the normal one. Returning
                // false here instead abandoned the sweep after the first blob had
                // already been taken out of the table: the rest were never released
                // (`StopBlobs` always read 0). Only the windowed-blt cancel the
                // closure would have done remains to do.
                Err(_) => {
                    let _ =
                        adapter.with_virtio(|v| v.cancel_windowed_blt_for_resource(adapter, res));
                }
            }

            // As in the single-resource path, the worker cannot pass this
            // point until cancellation, cache drain, and reader terminal are
            // one lifecycle transaction.
            adapter
                .with_virtio(|v| v.finish_windowed_blt_teardown_for_resource(adapter, res))
                .unwrap_or(false)
        });
        if !terminal {
            // The blob tracking entry was intentionally taken first. Retaining
            // the host objects on an ambiguous drain leaks safely until Venus
            // teardown; continuing would detach a possibly in-flight resource.
            //
            // Returning here used to leave every REMAINING blob of the owner in the table
            // with a dead owner token (the device is being destroyed: nothing will ever
            // sweep them again) and their windowed blts pending, which fills the blob table
            // and the 64 WindowedBlt token slots across a few killed processes. The rest
            // are drained without host commands (`abandoned`, above).
            abandoned = true;
            // The ambiguous blob's own ledger claim as well (pinned while its reader is
            // active, reclaimed when that reader's ticket retires).
            retire_ledger_claim(passive, adapter, res);
            BLOB_SWEEP_ABANDONED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if mapped {
            if let Some(timeout_ms) = sweep_timeout_ms(budget) {
                let _ = resource_unmap_blob_within(passive, adapter, res, timeout_ms);
            }
            let _ = adapter.with_virtio(|v| v.free_window_range_pub(map_offset, map_len));
        }
        let first_teardown = adapter
            .with_virtio(|v| v.take_live_resource(res))
            .unwrap_or(false);
        if first_teardown {
            if let Some(timeout_ms) = sweep_timeout_ms(budget) {
                let _ = ctx_detach_resource_within(passive, adapter, ctx_id, res, timeout_ms);
            }
            if let Some(timeout_ms) = sweep_timeout_ms(budget) {
                let _ = resource_unref_within(passive, adapter, res, timeout_ms);
            }
        }
        // Same D4b ledger reclaim as `release_blob_for_owner`: this sweep is
        // how a crashed/exited process's snapshot resids reach the ledger at
        // all (`RdOvf` must stay 0 across app restarts). Per-resid acquisition
        // keeps the display worker's lock hold times unchanged during a sweep.
        retire_ledger_claim(passive, adapter, res);
        reclaimed += 1;
    }
}

/// End `res`'s claim on the D4a read ledger (`ReadLedger::note_alloc_retired`) under the
/// scanout lifecycle mutex, which serialises the ledger's claim discipline. Every blob
/// teardown path of `release_blobs_for_owner_within` ends here, the ones that send nothing
/// to the host included.
fn retire_ledger_claim(passive: PassiveLevel, adapter: &AdapterContext, res: u32) {
    adapter.with_scanout_lifecycle(passive, |_lock| {
        adapter.read_ledger.note_alloc_retired(res);
    });
}

/// Drop the KMD-internal (owner-0) blob slot for an allocation at
/// DestroyAllocation time, unmapping the window mapping the GDI executor may
/// have opened. Returns `true` if a live mapping was unmapped here (the caller
/// must not send a second host unmap for the same resource).
pub fn forget_allocation_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> bool {
    let taken = adapter
        .with_virtio(|v| {
            if v.unregister_present_buffer(resource_id) {
                v.forget_allocation_blob(resource_id)
            } else {
                None
            }
        })
        .unwrap_or(None);
    let Some((mapped, map_offset, map_len)) = taken else {
        return false;
    };
    if mapped {
        let _ = resource_unmap_blob(passive, adapter, resource_id);
        let _ = adapter.with_virtio(|v| v.free_window_range_pub(map_offset, map_len));
        return true;
    }
    false
}

/// Release the host resource an allocation owns, exactly once: drop its blob slot
/// (and a foreign record), then, only for the first claimant of the live-resource
/// entry, detach it from `ctx_id` and `RESOURCE_UNREF` it.
///
/// The one place both release triggers meet: `DxgkDdiDestroyAllocation` (nothing
/// open) and the last `DxgkDdiCloseAllocation` of an allocation destroyed while
/// still open (`ForeignTable::allocation_destroyed` / `close`). The old adopted
/// arm unref'd unconditionally, which double-freed resources another path had
/// already reclaimed (QEMU "virgl_cmd_resource_unref: resource does not exist");
/// `take_live_resource` is the guard. Best-effort on the virtio operations:
/// teardown must not get stuck, and a context that is already gone just makes the
/// detach fail. PASSIVE only.
pub fn release_allocation_resource(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    resource_id: u32,
) {
    // `forget_allocation_blob` already OWNS the unmap decision (T6/R915).
    let _ = forget_allocation_blob(passive, adapter, resource_id);
    let first_teardown = adapter
        .with_virtio(|v| v.take_live_resource(resource_id))
        .unwrap_or(false);
    if first_teardown {
        let _ = ctx_detach_resource(passive, adapter, ctx_id, resource_id);
        let _ = resource_unref(passive, adapter, resource_id);
    }
    // `KmdRmClient` = 5: a resource that was the KMD's own RM system memory also owes its
    // GEM and its RM memory (one atomic load when the service holds nothing).
    super::rm_client::sysmem::released(passive, adapter, resource_id);
    super::rm_client::vidmem::released(passive, adapter, resource_id);
}

// ── Venus submission ─────────────────────────────────────────────────────────

/// SYNCHRONOUS venus SUBMIT_3D (in-kernel venus client's direct commands —
/// small ring-bootstrap/notify streams). Blocks at PASSIVE until the device
/// acks the command on the used ring (decode-level; the client's real waits
/// are its ring-head polls). `fence_id` stays 0 (parity with the proven
/// System-class `submit_direct` shape).
pub fn submit_3d_sync(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
) -> Result<(), VirtioError> {
    if stream.is_empty() || !stream_fits(stream) {
        return Err(VirtioError::DeviceError);
    }
    let mut cmd = helios_protocol::VirtioGpuCmdSubmit::zeroed();
    cmd.hdr.type_ = helios_protocol::VIRTIO_GPU_CMD_SUBMIT_3D;
    cmd.hdr.flags = helios_protocol::VIRTIO_GPU_FLAG_FENCE;
    cmd.hdr.ctx_id = ctx_id;
    cmd.size = stream.len() as u32;
    // The stream rides a second device-read descriptor (kept split so the host
    // never mis-parses the submit header as another control command).
    ctrl_roundtrip_ok(passive, adapter, bytes_of(&cmd), Some(stream))
}

pub fn submit_venus_sync(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
) -> Result<(), VirtioError> {
    submit_3d_sync(passive, adapter, ctx_id, stream)
}

/// ASYNC venus SUBMIT_3D (the ICD escape path): stage the stream into DMA
/// buffers, enqueue fenced with a fresh KMD wire fence id, and return that id
/// at QUEUE time. Completion is observed via [`wait_fence`].
pub fn submit_venus_async(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    ring_idx: u32,
    stream: &[u8],
) -> Result<u64, VirtioError> {
    submit_venus_async_inner(passive, adapter, owner, ctx_id, ring_idx, stream, None)
}

/// Tagged async submit for a registered present stream.  `cookie` and `value`
/// are validated again in the exact transport-lock critical section that adds
/// the normal wire-fenced command, so CTX_DESTROY cannot race a successful
/// enqueue.
pub fn submit_venus_async_present_stream(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    ctx_id: u32,
    ring_idx: u32,
    cookie: u64,
    value: u32,
    stream: &[u8],
) -> Result<u64, VirtioError> {
    let result = submit_venus_async_inner(
        passive,
        adapter,
        Some(owner),
        ctx_id,
        ring_idx,
        stream,
        Some((owner, cookie, value)),
    );

    if result.is_err() {
        // The UMD Present marker is deliberately allowed to reach VidSch before
        // Mesa has queued this tagged batch.  Once this function returns an
        // error, however, no tag can ever retire that marker.  Revoke the exact
        // registration and explicitly discharge its scheduler condition while
        // retaining the ordinary wire/GPU watermark; otherwise an allocation or
        // queue failure here can leave DMA_COMPLETED waiting forever for a tag
        // that was never placed on the transport.
        let _ = adapter.with_wddm_notify_lock(|guard| {
            guard.with_virtio(|order, v| {
                // `submit_venus_async_inner` opportunistically drains used
                // descriptors before enqueue. If that drain consumed a host
                // rejection, the stream is already dead and unregister returns
                // false; still run the ordered discharge here so correctness
                // never depends on receiving the interrupt that prompted the
                // opportunistic drain in the first place.
                let _ =
                    v.cancel_present_buffer_read_claims_before_submit(owner, ctx_id, cookie, value);
                let _ = v.unregister_present_stream(order, owner, ctx_id, cookie);
                let _ = v.discharge_dead_present_stream_waits(order);
            })
        });
        // Rare terminal path: request unconditionally. The stream may have
        // been invalidated by the drain above even when exact unregister could
        // no longer find it, and a now-ready WDDM head needs a fresh DPC edge.
        crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
    }

    result
}

fn submit_venus_async_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: Option<DeviceOwner>,
    ctx_id: u32,
    ring_idx: u32,
    stream: &[u8],
    present_stream: Option<(DeviceOwner, u64, u32)>,
) -> Result<u64, VirtioError> {
    if stream.is_empty() {
        return Err(VirtioError::DeviceError);
    }
    if !stream_fits(stream) {
        return Err(VirtioError::DeviceError);
    }
    reap_parked(passive, adapter);
    // Ownership is resolved under the device lock, the same lock the enqueue
    // below takes, so a foreign command stream cannot reach another process's
    // Venus ring. The two staging buffers come out of the pool in the SAME hold
    // (one acquisition instead of three per submit, ~2000 submits/s); a buffer
    // the pool cannot supply is allocated below at PASSIVE, never under the lock.
    let staged = adapter
        .with_virtio(|v| {
            let owned = v.resolve_owned_ctx(owner, ctx_id)?;
            let meta = v.take_dma_buffer(SUBMIT_META_BYTES);
            let venus = v.take_dma_buffer(stream.len());
            Some((owned, meta, venus))
        })
        .map_err(|_| VirtioError::DeviceError)?;
    let Some((owned, pooled_meta, pooled_venus)) = staged else {
        return Err(VirtioError::NotOwned);
    };
    let ctx_id = owned.id();
    let mut meta = pooled_meta
        .or_else(|| DmaBuffer::new(passive, SUBMIT_META_BYTES))
        .ok_or(VirtioError::OutOfMemory)?;
    let mut venus = pooled_venus
        .or_else(|| DmaBuffer::new(passive, stream.len()))
        .ok_or(VirtioError::OutOfMemory)?;
    venus.as_mut_slice()[..stream.len()].copy_from_slice(stream);
    let venus_len = stream.len();

    let space_wake = adapter.knobs().submit_space_wake;
    let mut space_waiter: Option<crate::adapter::ControlSpaceWaiter<'_>> = None;
    let mut retry_qpc = 0;
    // SAFETY: nonpageable monotonic query with valid required QPC output storage.
    let retry_started = if space_wake {
        unsafe { KeQueryInterruptTimePrecise(&mut retry_qpc) }
    } else {
        0
    };

    // Both buffers are carried as loop values for the reason given in
    // `ctrl_roundtrip`: this loop has two of them, so an arm that returns only
    // one is exactly the maintenance mistake the take-then-expect pair used to
    // turn into a bugcheck. As loop values it does not compile.
    let mut budget = Budget::new(ENQUEUE_RETRY_MAX_MS);
    loop {
        let waiter_ref = &space_waiter;
        let res = adapter.with_virtio(move |v| {
            v.drain_used();
            let result = match present_stream {
                Some((stream_owner, cookie, value)) => v.enqueue_async_submit_present_stream(
                    stream_owner,
                    ctx_id,
                    ring_idx,
                    cookie,
                    value,
                    meta,
                    venus,
                    venus_len,
                ),
                None => v.enqueue_async_submit(ctx_id, ring_idx, meta, venus, venus_len),
            };
            if matches!(result, Err((_, _, VirtioError::QueueFull))) {
                if let Some(waiter) = waiter_ref {
                    waiter.reset_after_full(v);
                }
            }
            result
        });
        match res {
            Err(_) => return Err(VirtioError::DeviceError), // transport gone
            Ok(Ok(fence_id)) => {
                // ATTRIBUTION POINT for guest venus traffic (KMD_IMPACT §14a.1
                // UV3). This function is reachable only from the escape
                // (`ddi/escape.rs:1233`, `:1242`), so a count here is
                // GUEST-originated by construction — which the adapter-wide
                // `RING_SUBMIT_COUNT` is not: three internal producers reach
                // ring 1 on their own, one of them (`submit_venus_async_present`
                // below) through the same generic enqueue this path uses.
                // Counted on the accepted arm only, so it compares directly with
                // `ASYNC_SUBMIT_COUNT`.
                //
                // ⛔ The retired text here named "the two readings that matter" as
                // *a zero `EscSubRing` with a nonzero `EscSub`, versus a zero
                // `EscSub`*. **Both of those readings are UNREACHABLE.** This
                // counter is guest-originated but NOT process-scoped, and DWM's own
                // DXVK -> Mesa-venus ICD submits through this same escape: every
                // present signals a win32 external semaphore whose batch carries a
                // ring index that is always >= 1 (ring 0 is reserved for the CPU
                // timeline and never handed out). So on any live desktop BOTH
                // counters are already climbing before a D3D12 client starts, and a
                // zero in either can only mean the driver never ran.
                // ⇒ the grading in `virtio/counters.rs` is a **delta against a
                // control arm** over the same wall-clock window, never an absolute
                // value and never a zero-test. Read it there.
                ESCAPE_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
                if ring_idx != 0 {
                    ESCAPE_SUBMIT_RING_COUNT.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(fence_id);
            }
            Ok(Err((m_back, v_back, VirtioError::QueueFull))) => {
                meta = m_back;
                venus = v_back;
                // Preserve the legacy number of retry opportunities and its
                // no-wake timing. Early signals must not spend that budget in
                // milliseconds; additionally require five seconds of elapsed
                // interrupt time. This is not a new hard five-second deadline.
                let retries_exhausted = budget.charge_slice();
                // SAFETY: valid required QPC output storage, as above.
                let elapsed = if space_wake {
                    unsafe { KeQueryInterruptTimePrecise(&mut retry_qpc) }
                        .saturating_sub(retry_started)
                } else {
                    u64::MAX
                };
                if retries_exhausted && elapsed >= ENQUEUE_RETRY_MAX_MS * 10_000 {
                    return Err(VirtioError::QueueFull);
                }
                if space_wake && space_waiter.is_none() {
                    space_waiter = Some(adapter.control_space_waiter());
                    // Registration may race reclamation: retry under the lock
                    // before the first wait, so that wake cannot be lost.
                    continue;
                }
                reap_parked(passive, adapter);
                if let Some(waiter) = &space_waiter {
                    waiter.wait(passive)?;
                } else {
                    sleep_ms(passive, RETRY_SLICE_MS);
                }
            }
            Ok(Err((_m, _v, e))) => return Err(e), // buffers dropped at PASSIVE
        }
    }
}

/// Count one received submission escape (`EscCalls`); see
/// `ESCAPE_SUBMIT_CALLS`. Counted on receipt, accepted or not.
pub fn count_submit_escape() {
    ESCAPE_SUBMIT_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// The prologue every display submitter shares: refuse an empty stream, reap
/// parked buffers, and stage the meta + venus DMA buffers.
///
/// R1004 made it one function; docs 24.14 (`SubmitPool`, default 1) made it
/// take the two buffers from the transport's DMA POOL, as the escape path
/// (`submit_venus_async`) always has, instead of two `MmAllocateContiguousMemory`
/// calls per frame. `SubmitPool` 0 is the old allocate-per-submit path exactly.
///
/// THE LIFETIME RULE is the pool's own and is not changed here: a buffer enters
/// `dma_pool` only from a completed in-flight entry, i.e. after `drain_used`
/// popped that submit's used-ring element (the host has consumed both
/// descriptors), through the PASSIVE reap (`recycle_dma_buffers`) or the
/// drain's raw-forward push; and a buffer a submit took is owned by its
/// in-flight entry until that same completion. The pool is bounded
/// (`MAX_DMA_POOL` buffers, `MAX_DMA_POOL_BYTES`) and lives in the transport
/// generation (`VirtioGpu`), so StopDevice frees it at PASSIVE with the
/// transport, before anything the allocation depends on goes. Display buffers
/// were already RECYCLED into it by the reap; they were simply never taken.
///
/// The reap's begin and both takes share ONE lock hold (the `raw_roundtrip`
/// pattern); a miss (pool empty, nothing big enough) falls back to a fresh
/// allocation here at PASSIVE, never under the lock and never waiting.
///
/// Every stage is timed on `clock` (docs 24.14, `Sub*` counters): `Take`,
/// `Reap`, `Alloc`, `Copy`.
fn stage_display_submit(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    stream: &[u8],
    clock: &mut Clock,
) -> Result<(DmaBuffer, DmaBuffer, usize), VirtioError> {
    if stream.is_empty() || !stream_fits(stream) {
        return Err(VirtioError::DeviceError);
    }
    let pool_on = adapter.knobs().submit_pool;
    let (pooled_meta, pooled_venus) = if pool_on {
        let taken = adapter.with_virtio(|v| {
            (
                v.begin_parked_reap(),
                v.take_dma_buffer(SUBMIT_META_BYTES),
                v.take_dma_buffer(stream.len()),
            )
        });
        submit_stage::lap(clock, Stage::Take);
        let (work, meta, venus) = match taken {
            Ok((work, meta, venus)) => (Ok(work), meta, venus),
            Err(gone) => (Err(gone), None, None),
        };
        // Whatever `begin_parked_reap` returned MUST reach the reap (see
        // `reap_parked_work`). The buffers it recycles serve the NEXT submit.
        reap_parked_work(passive, adapter, work);
        submit_stage::lap(clock, Stage::Reap);
        (meta, venus)
    } else {
        reap_parked(passive, adapter);
        submit_stage::lap(clock, Stage::Reap);
        (None, None)
    };
    let plan = helios_kmd_logic::submit_stage::plan(
        pool_on,
        pooled_meta.is_some(),
        pooled_venus.is_some(),
    );
    let fresh = helios_kmd_logic::submit_stage::fresh_count(plan);
    submit_stage::note_staging(fresh, 2 - fresh);
    // A failed fresh allocation drops the other (pooled or fresh) buffer here,
    // at PASSIVE, exactly as before.
    let meta = match pooled_meta {
        Some(buf) => Some(buf),
        None => DmaBuffer::new(passive, SUBMIT_META_BYTES),
    }
    .ok_or(VirtioError::OutOfMemory);
    let venus = match (&meta, pooled_venus) {
        (Err(_), _) => None,
        (Ok(_), Some(buf)) => Some(buf),
        (Ok(_), None) => DmaBuffer::new(passive, stream.len()),
    };
    submit_stage::lap(clock, Stage::Alloc);
    let meta = meta?;
    let mut venus = venus.ok_or(VirtioError::OutOfMemory)?;
    // A pooled buffer keeps stale bytes past `stream.len()`; the device reads
    // only `[..venus_len]` (the descriptor length), and `take_dma_buffer`'s
    // `reset` re-stamped the wire tail.
    venus.as_mut_slice()[..stream.len()].copy_from_slice(stream);
    submit_stage::lap(clock, Stage::Copy);
    Ok((meta, venus, stream.len()))
}

/// The display submitters' (and the pipelined flip's) enqueue hold: `drain_used`, then
/// `enqueue` (one attempt), each timed on `clock` (`Lock` from the call to the first instruction
/// under the lock, `Drain`, `Enq`, and `Kick` carved out of `Enq` by the transport's notify
/// timer, armed for this hold only and only with `SubStageClk` on).
///
/// The drain stays unconditional. When the used ring is empty it is one acquire load
/// (`peek_used`), so there is nothing to save; when it is not, what it consumes is exactly
/// what the enqueue may depend on in the same hold: free descriptors (the display paths do not
/// retry a QueueFull), and the destination ownership a retired Blt released (`BltEnq::Busy`
/// otherwise) or the windowed-Blt FIFO state. `SubDrainHit` counts the second case; docs 24.14.
///
/// THE LATE DOORBELL (`SubKickUnlock`, docs 24.14.10) lives here, for all five callers: the
/// hold arms it, the enqueue's `publish_then_notify` decides (after a full fence) whether a
/// notify is owed and takes a pending count instead of ringing, the hold takes the
/// [`super::gpu::KickTicket`], and it is rung only after `with_virtio` has released the lock.
/// `Stage::Kick` is then the unlocked doorbell, timed outside the lock.
fn display_enqueue<R>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    path: Path,
    clock: &mut Clock,
    enqueue: impl FnOnce(&mut super::gpu::VirtioGpu) -> R,
) -> Result<R, NotStarted> {
    let timing = submit_stage::timing();
    let held = &mut *clock;
    let queued = adapter.with_virtio(move |v| {
        submit_stage::lap(held, Stage::Lock);
        if v.used_pending() {
            submit_stage::note_drain_hit(path);
        }
        v.drain_used();
        submit_stage::lap(held, Stage::Drain);
        if timing {
            v.arm_kick_timer();
        }
        v.arm_late_kick();
        let result = enqueue(v);
        // Taken in the SAME hold that armed it: nothing else can see it armed.
        let late = v.take_late_kick();
        submit_stage::lap(held, Stage::Enq);
        if timing {
            if let Some(kick) = v.take_kick_timer() {
                held.carve(Stage::Enq, Stage::Kick, u64::from(kick.ticks));
                if !kick.notified {
                    submit_stage::note_no_kick(path);
                }
            }
        }
        (result, late)
    });
    // `virtio_lock` is released: ring the owed doorbell now.
    queued.map(|(result, late)| {
        if let Some(ticket) = late {
            ticket.ring();
            submit_stage::lap(clock, Stage::Kick);
            publish_kick_counters(passive);
        }
        result
    })
}

/// Last mirror of the late-doorbell counters, 100 ns.
static KICK_MIRROR_AT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Mirror the late-doorbell counters (`helios_kmd_logic::kick_defer::COUNTERS`), at most once a
/// second, from a submitter that just rang one. PASSIVE (registry writes).
fn publish_kick_counters(_passive: PassiveLevel) {
    use super::gpu::{
        KICK_DROPPED, KICK_GAP_TICKS, KICK_LATE, KICK_TEARDOWN_LEAKS, KICK_TEARDOWN_WAITS,
    };
    use crate::diag::record_named_bytes as rec;
    let now = submit_stage::now_100ns();
    let last = KICK_MIRROR_AT.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 10_000_000 {
        return;
    }
    KICK_MIRROR_AT.store(now, Ordering::Relaxed);
    rec(b"SubKickLate", KICK_LATE.load(Ordering::Relaxed));
    rec(b"SubKickDrop", KICK_DROPPED.load(Ordering::Relaxed));
    rec(
        b"SubKickGap",
        helios_kmd_logic::submit_stage::ticks_to_us(KICK_GAP_TICKS.load(Ordering::Relaxed)),
    );
    rec(b"SubKickWait", KICK_TEARDOWN_WAITS.load(Ordering::Relaxed));
    rec(b"SubKickLeak", KICK_TEARDOWN_LEAKS.load(Ordering::Relaxed));
}

/// The outcome mapping both display submitters share: a transport-gone outer
/// error and a per-enqueue inner error are distinct, and the handed-back
/// buffers are dropped at PASSIVE.
///
/// Neither path retries. That is deliberate and unchanged: one enqueue attempt
/// either succeeds or reports QueueFull to the caller, which keeps
/// SetVidPnSourceAddress out of a hidden multi-second retry loop.
fn display_submit_outcome(
    queued: Result<Result<u64, (DmaBuffer, DmaBuffer, VirtioError)>, crate::error::NotStarted>,
) -> Result<u64, VirtioError> {
    match queued {
        Ok(Ok(fence_id)) => Ok(fence_id),
        Ok(Err((_meta, _venus, e))) => Err(e),
        Err(_) => Err(VirtioError::DeviceError),
    }
}

/// Nonblocking KMD scanout-copy submission. `stream` is the already encoded
/// Venus vkQueueSubmit command; the outer virtio SUBMIT_3D is fenced on
/// ring_idx=1, whose used-ring completion represents GPU completion. Only a
/// successful completion marks scanout dirty and wakes the refresh worker.
///
/// Unlike the user escape path above, this per-frame display path never sleeps
/// for queue backpressure: one enqueue attempt either succeeds or reports
/// QueueFull to the caller. That keeps SetVidPnSourceAddress out of a hidden
/// multi-second retry loop.
pub fn submit_venus_async_scanout(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
    primary_address: u64,
    ticket: crate::adapter::ProgrammingTicket,
    keep_on_failure: bool,
) -> Result<u64, VirtioError> {
    measured(Path::Scanout, |clock| {
        let (meta, venus, venus_len) = stage_display_submit(passive, adapter, stream, clock)?;
        // One construction site, on the adapter, so all four pointers necessarily
        // come from the same adapter; and `enqueue_scanout_submit` is the only way
        // to attach it, so it necessarily lands on the ring the drain honours.
        let notify = adapter.scanout_notify(primary_address, ticket, keep_on_failure);

        display_submit_outcome(display_enqueue(passive, adapter, Path::Scanout, clock, move |v| {
            v.enqueue_scanout_submit(ctx_id, meta, venus, venus_len, notify)
        }))
    })
}

/// Nonblocking KMD Present-BLT submission.
///
/// `ring_idx` is the ring of the queue the copy's command buffer is submitted to: 1 (family 0,
/// every copy before `CopyQueue`) or `copy_queue::COPY_RING_IDX` (the transfer queue). The
/// host's wire fence for a ring is an empty submit on that ring's queue, so only the copy's own
/// ring orders the fence after it; retirement is per command, so nothing else changes.
///
/// Like the scanout copy path, ring_idx=1 makes used-ring retirement represent
/// GPU completion. Unlike scanout, an ordinary app/DWM BLT must not mark the
/// physical scanout dirty or wake the display refresh worker. Every tagged
/// standard destination advances only to CPU-mirror ownership at retirement;
/// its caller explicitly releases the buffer after the serialized backing
/// check/mirror returns.
pub fn submit_venus_async_present(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
    present_buffer_write: Option<u32>,
    ring_idx: u32,
) -> Result<u64, VirtioError> {
    if ring_idx == 0 {
        return Err(VirtioError::DeviceError);
    }
    measured(Path::Present, |clock| {
        let (meta, venus, venus_len) = stage_display_submit(passive, adapter, stream, clock)?;

        // Ring 1 WITHOUT a notify, which is the whole difference from the scanout
        // path above: used-ring retirement still represents GPU completion, but an
        // ordinary app/DWM BLT must not mark the physical scanout dirty or wake the
        // display refresh worker.
        display_submit_outcome(display_enqueue(passive, adapter, Path::Present, clock, move |v| {
            match present_buffer_write {
                Some(resource_id) => v.enqueue_async_submit_present_buffer(
                    ctx_id,
                    ring_idx,
                    meta,
                    venus,
                    venus_len,
                    resource_id,
                ),
                None => v.enqueue_async_submit(
                    ctx_id,
                    ring_idx,
                    meta,
                    venus,
                    venus_len,
                ),
            }
        }))
    })
}

/// The outcome of [`submit_venus_async_blt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BltSubmit {
    /// The copy was enqueued on ring 1; its wire fence. The destination's writer ownership is
    /// held by the in-flight table and handed back by the completion DPC.
    Fence(u64),
    /// The destination is owned by a reader, or by a writer that is not one of the direct
    /// submissions: nothing was enqueued.
    DstBusy,
}

/// Nonblocking KMD Present-BLT submission of a DIRECT asynchronous Blt (`BltAsync`,
/// `docs/zero-copy-present.md`, "Asynchronous composed present"). Like
/// [`submit_venus_async_present`] on ring 1 without a notify, but the destination's writer
/// ownership is taken (or joined) in the SAME transport critical section as the enqueue, the
/// copy is recorded in the in-flight table, and the completion DPC (not the caller) hands the
/// buffer back: nothing here, and nothing the caller does afterwards, waits for the host.
pub fn submit_venus_async_blt(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
    resource_id: u32,
    source_id: u32,
    ring_idx: u32,
) -> Result<BltSubmit, VirtioError> {
    if ring_idx == 0 {
        return Err(VirtioError::DeviceError);
    }
    measured(Path::Blt, |clock| {
        let (meta, venus, venus_len) = stage_display_submit(passive, adapter, stream, clock)?;
        let queued = display_enqueue(passive, adapter, Path::Blt, clock, move |v| {
            v.enqueue_async_submit_blt(
                adapter,
                ctx_id,
                meta,
                venus,
                venus_len,
                resource_id,
                source_id,
                ring_idx,
            )
        });
        match queued {
            Ok(Ok(crate::virtio::gpu::BltEnq::Fence(fence_id))) => Ok(BltSubmit::Fence(fence_id)),
            // The staged buffers are dropped here, at PASSIVE.
            Ok(Ok(crate::virtio::gpu::BltEnq::Busy(_meta, _venus))) => Ok(BltSubmit::DstBusy),
            Ok(Err((_meta, _venus, e))) => Err(e),
            Err(_) => Err(VirtioError::DeviceError),
        }
    })
}

/// Legacy inline Present cannot leave the callback and retry asynchronously.
/// Wait outside both the Venus mutex and virtio_lock until the external reader
/// retires, then atomically transition the buffer to KmdWriter. The modern
/// two-phase path never uses this wait; it remains queued and event-driven.
pub fn begin_present_buffer_write_legacy(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<(), VirtioError> {
    // Nominal 320 ms = roughly 5 s at Windows' common 15.6 ms timer quantum.
    let mut budget = Budget::new(320);
    loop {
        let state = adapter
            .with_virtio(|v| {
                v.drain_used();
                v.try_begin_present_buffer_write(resource_id)
            })
            .map_err(|_| VirtioError::DeviceError)?;
        match state {
            crate::virtio::gpu::PresentBufferWriteBegin::Acquired => return Ok(()),
            crate::virtio::gpu::PresentBufferWriteBegin::NotFound => {
                return Err(VirtioError::DeviceError);
            }
            crate::virtio::gpu::PresentBufferWriteBegin::Busy => {
                if budget.charge_slice() {
                    return Err(VirtioError::QueueFull);
                }
                sleep_ms(passive, RETRY_SLICE_MS);
            }
        }
    }
}

/// Revoke new Present-buffer users and wait for every already-admitted KMD or
/// consumer GPU submission to retire. The wait is PASSIVE and never holds the
/// Venus mutex, virtio_lock, or the scanout lifecycle lock across a sleep.
pub fn begin_present_buffer_teardown(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<(), VirtioError> {
    let mut budget = Budget::new(320);
    loop {
        let ready = adapter
            .with_virtio(|v| {
                v.drain_used();
                v.begin_present_buffer_teardown(resource_id)
            })
            .map_err(|_| VirtioError::DeviceError)?;
        if ready {
            return Ok(());
        }
        if budget.charge_slice() {
            return Err(VirtioError::QueueFull);
        }
        sleep_ms(passive, RETRY_SLICE_MS);
    }
}

/// Nonblocking ring-1 submission for an already admitted WindowedBlt token.
/// The used-ring entry carries the token back to the DPC so the reader lease
/// and WDDM completion cannot be released by a different present stream.
pub fn submit_venus_async_windowed_blt(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    ctx_id: u32,
    stream: &[u8],
    token: u64,
    stream_boundary: u64,
    ring_idx: u32,
) -> Result<u64, VirtioError> {
    if ring_idx == 0 {
        return Err(VirtioError::DeviceError);
    }
    measured(Path::Windowed, |clock| {
        let (meta, venus, venus_len) = stage_display_submit(passive, adapter, stream, clock)?;
        display_submit_outcome(display_enqueue(passive, adapter, Path::Windowed, clock, move |v| {
            v.enqueue_async_submit_windowed_blt(
                adapter,
                ctx_id,
                meta,
                venus,
                venus_len,
                token,
                stream_boundary,
                ring_idx,
            )
        }))
    })
}

/// Outcome of a [`wait_fence`] call.
///
/// ⚠ The escape boundary (`ddi/escape.rs`'s `escape_wait_fence`) must keep
/// matching every variant explicitly, with **no wildcard arm**: today it maps
/// three variants to three statuses, so adding a variant is a compile error
/// there instead of a silent collapse into `TimedOut`. That is the whole
/// encoding — `#[non_exhaustive]` is deliberately NOT used, because it only
/// affects downstream crates and would claim a guarantee it cannot provide
/// inside this one.
///
/// A fourth variant also needs a paired ICD change: `escape_wait_fence` reports
/// only `out_completed` 1/0 plus `STATUS_INVALID_PARAMETER`, so a third state
/// has nowhere to go on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitFenceOutcome {
    /// The wire fence has completed (host-visible-complete).
    Complete,
    /// `timeout_ns` elapsed first (or this was a poll and it is still pending).
    TimedOut,
    /// The id was never assigned / the transport is gone.
    Invalid,
}

/// Wait (PASSIVE, KEVENT) until wire fence `fence_id` completes or
/// `timeout_ns` elapses. `timeout_ns == 0` is a poll.
pub fn wait_fence(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    fence_id: u64,
    timeout_ns: u64,
) -> WaitFenceOutcome {
    // Scoped exactly as in `ctrl_roundtrip`. Every `return` below is a return
    // from the closure, and the deregistration pairing is unchanged: the four
    // early exits in the registration loop all happen BEFORE `Registered`, so
    // there is no waiter to cancel, and both post-registration exits call
    // `fence_wait_cancel`.
    SyncWaitBlock::with(|block| {
        let mut full_retries = 0u32;
        loop {
            let prep = adapter.with_virtio(|v| {
                v.drain_used();
                v.fence_wait_prepare(fence_id, block.as_ptr())
            });
            match prep {
                Err(_) => return WaitFenceOutcome::Invalid, // transport gone
                Ok(FenceWaitPrep::Complete) => return WaitFenceOutcome::Complete,
                Ok(FenceWaitPrep::Invalid) => return WaitFenceOutcome::Invalid,
                Ok(FenceWaitPrep::TableFull) => {
                    full_retries += 1;
                    // A `GuestBlob` bounded section (the drain of a retire) gives up at its
                    // deadline instead of after up to ~16 s; nothing else changes (escapes and
                    // unscoped threads never see `bounded_spent`).
                    if full_retries > 1_000 || crate::ddi::escape_wait::bounded_spent() {
                        // NOT FENCE_WAIT_TIMEOUTS: the host may be perfectly
                        // healthy and all MAX_FENCE_WAITERS slots simply occupied.
                        // The outcome stays TimedOut so the ICD is untouched; only
                        // the evidence is split. Note the budget is nominally 1 s
                        // but KeDelayExecutionThread rounds a 1 ms relative timeout
                        // up to the system timer granularity (~15.6 ms), so this is
                        // up to ~16 s of thread residency.
                        FENCE_WAIT_TABLE_FULL.fetch_add(1, Ordering::Relaxed);
                        return WaitFenceOutcome::TimedOut;
                    }
                    sleep_ms(passive, 1);
                }
                Ok(FenceWaitPrep::Registered) => break,
            }
        }

        if timeout_ns == 0 {
            // Poll: deregister immediately; completion may still have raced in.
            return match adapter.with_virtio(|v| v.fence_wait_cancel(block.as_ptr())) {
                Ok(true) => WaitFenceOutcome::Complete,
                Ok(false) => WaitFenceOutcome::TimedOut,
                // Transport gone: the fence did NOT retire. Reporting Complete here
                // made escape_wait_fence write out_completed = 1 and return
                // STATUS_SUCCESS for an unretired wire fence - a direct violation of
                // "never signal a wire fence before host completion". Invalid is
                // already mapped to STATUS_INVALID_PARAMETER and already handled by
                // the ICD.
                Err(_) => {
                    TRANSPORT_GONE_AT_WAIT.fetch_add(1, Ordering::Relaxed);
                    WaitFenceOutcome::Invalid
                }
            };
        }

        let total_ms = (timeout_ns / 1_000_000).max(1).min(WAIT_FENCE_MAX_MS);
        if wait_block(passive, adapter, block, total_ms) {
            return WaitFenceOutcome::Complete;
        }
        match adapter.with_virtio(|v| {
            v.drain_used();
            v.fence_wait_cancel(block.as_ptr())
        }) {
            Ok(true) => WaitFenceOutcome::Complete,
            Ok(false) => {
                FENCE_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                WaitFenceOutcome::TimedOut
            }
            // As in the poll exit above.
            Err(_) => {
                TRANSPORT_GONE_AT_WAIT.fetch_add(1, Ordering::Relaxed);
                WaitFenceOutcome::Invalid
            }
        }
    })
}

//! The escape scope: every wait an escape makes is bounded and killable (v334,
//! `docs/zero-copy-present.md` section 23; the pure rules are `helios_kmd_logic::wait_bound`).
//!
//! `dxgkddi_escape` registers the calling thread here for the length of the call
//! ([`begin`]). The wait primitives it reaches (`ctrl::wait_block`, the mutex acquires, the retry
//! budgets, the Venus ring wait) cannot be handed a context through the forty call sites between,
//! so they ask this module ([`probe`], [`abort_now`]): is this thread inside an escape and, if so,
//! is it terminating, is the device stopping, is the escape's deadline spent. A thread that is not
//! inside an escape (the HPD worker, a DPC, the paging path, a DDI dxgkrnl calls on a terminating
//! thread to clean up) is never aborted by any of it.
//!
//! The one exception is a `GuestBlob` bounded section ([`begin_bounded`]): a thread inside one
//! sees that section's deadline through [`probe`] as if it were an escape's (no kill, no stop),
//! so every "never aborts a thread outside an escape" in the wait primitives reads "outside an
//! escape or a bounded section". No section is ever opened with `GuestBlob` 0 and `RmCopyEngine`
//! 0 (the copy-engine channel's bring-up and self-test open one each, on the HPD worker).
//!
//! The abort is a FAILURE of the wait, delivered through the path the wait already has for a
//! timeout (`wait_block` -> the abandon path -> `VirtioError::Timeout` -> the escape's own timeout
//! status; a mutex acquire -> `None` / `NotStarted`; a retry budget -> spent), so the escape
//! unwinds through its existing error handling and releases what it holds. Nothing here proceeds
//! without a lock it did not get.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::wait_bound as wb;

use crate::adapter::AdapterContext;

#[link(name = "ntoskrnl")]
extern "system" {
    fn PsGetCurrentThreadId() -> *mut c_void;
    /// `PsIsThreadTerminating(PETHREAD)`: PASSIVE..APC.
    fn PsIsThreadTerminating(thread: usize) -> u8;
}

/// Escapes in flight at once that can be tracked. A 65th concurrent escape runs with the old,
/// unscoped waits (`EscNoSlot`).
const SLOTS: usize = 64;
static TABLE: wb::Slots<SLOTS> = wb::Slots::new();

/// The device is stopping or being removed: set at the top of StopDevice / RemoveDevice, cleared
/// by StartDevice. Every scoped wait gives up at its next slice, and no new escape starts.
static STOPPING: AtomicU32 = AtomicU32::new(0);
/// `EscWaitMs` in force (clamped; 0 = no deadline). Read at every StartDevice.
static ESC_WAIT_MS: AtomicU32 = AtomicU32::new(wb::ESC_WAIT_DEFAULT_MS);

static SCOPES: AtomicU32 = AtomicU32::new(0);
static LONGEST_MS: AtomicU32 = AtomicU32::new(0);
static ABORT_KILL: AtomicU32 = AtomicU32::new(0);
static ABORT_STOP: AtomicU32 = AtomicU32::new(0);
static TIMEOUTS: AtomicU32 = AtomicU32::new(0);
static LOCK_ABORTS: AtomicU32 = AtomicU32::new(0);
static NO_SLOT: AtomicU32 = AtomicU32::new(0);
static REFUSED_STOPPING: AtomicU32 = AtomicU32::new(0);
/// The last `DxgkDdiPreemptCommand` (`PreFence`: the preemption fence id it carried, `PreLastCmp`:
/// the last completed fence it reported, `PreDropped`: pending submissions it dropped,
/// `PreStatus`: what it returned, `PreT`: when, interrupt ms).
static PRE_FENCE: AtomicU32 = AtomicU32::new(0);
static PRE_LAST: AtomicU32 = AtomicU32::new(0);
static PRE_DROPPED: AtomicU32 = AtomicU32::new(0);
static PRE_STATUS: AtomicU32 = AtomicU32::new(0);
static PRE_T: AtomicU32 = AtomicU32::new(0);

/// `DxgkDdiPreemptCommand` finished: remember what it did. Atomics only, any IRQL.
pub(crate) fn note_preempt(fence: u32, last_completed: u32, dropped: u32, status: u32) {
    PRE_FENCE.store(fence, Ordering::Relaxed);
    PRE_LAST.store(last_completed, Ordering::Relaxed);
    PRE_DROPPED.store(dropped, Ordering::Relaxed);
    PRE_STATUS.store(status, Ordering::Relaxed);
    PRE_T.store(now_ms().max(1), Ordering::Relaxed);
}

fn thread_id() -> u32 {
    // SAFETY: a scalar read of the current thread's cid; callable at any IRQL.
    (unsafe { PsGetCurrentThreadId() } as usize) as u32
}

fn now_ms() -> u32 {
    AdapterContext::interrupt_time_ms()
}

/// The device is going away (StopDevice / RemoveDevice entry): scoped waits give up from now on.
/// Any IRQL.
pub(crate) fn set_stopping(on: bool) {
    STOPPING.store(u32::from(on), Ordering::Release);
}

/// Whether the stopping flag is up.
pub(crate) fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire) != 0
}

/// Read `EscWaitMs` again (StartDevice, PASSIVE) and mirror it as `EscWaitMsEff`. Also lowers the
/// stopping flag: a new generation begins.
pub(crate) fn reread_knobs() {
    let ms = wb::clamp_esc_wait_ms(crate::diag::read_config_dword(
        crate::diag::knobs::ESC_WAIT_MS,
        wb::ESC_WAIT_DEFAULT_MS,
    ));
    ESC_WAIT_MS.store(ms, Ordering::Relaxed);
    set_stopping(false);
    crate::diag::record_named_bytes(b"EscWaitMsEff", ms);
}

/// Bounded KMD sections (`GuestBlob`: one phase of a guest-blob create or retire,
/// `helios_kmd_logic::guest_blob::deadline`): a thread that is NOT inside an escape registers
/// here for the length of one phase, and every wait primitive that obeys an escape's deadline
/// (the control round trip, the Venus ring wait, the abortable mutex acquires, the enqueue retry
/// budget) obeys this deadline too, through the same [`probe`]. Unlike an escape, a bounded
/// section is never ended by a kill or by the stopping flag (StopDevice retires guest blobs
/// itself, after raising it), and its aborts are not counted as escape aborts: the caller counts
/// its own outcome. Every section runs under the content transaction, so one slot is in use at
/// a time; a full table leaves the phase with the old, unbounded waits.
const BOUNDED_SLOTS: usize = 4;
static BOUNDED: wb::Slots<BOUNDED_SLOTS> = wb::Slots::new();
/// Sections registered now: `probe` scans [`BOUNDED`] only while this is nonzero, so a thread
/// outside every section pays one relaxed load.
static BOUNDED_LIVE: AtomicU32 = AtomicU32::new(0);

/// The calling thread's bounded section: registered by [`begin_bounded`], released on drop.
pub(crate) struct Bounded {
    slot: Option<usize>,
}

/// Enter a bounded section of `limit_ms` (at least 1) from now. A thread already inside an
/// escape or a section keeps the deadline it has (nested: nothing registered). PASSIVE.
pub(crate) fn begin_bounded(limit_ms: u32) -> Bounded {
    let id = thread_id();
    if TABLE.find(id).is_some() {
        return Bounded { slot: None };
    }
    let deadline = wb::deadline_for(now_ms(), limit_ms.max(1));
    // Counted first, so a `probe` of this thread never misses its own slot.
    BOUNDED_LIVE.fetch_add(1, Ordering::AcqRel);
    match BOUNDED.enter(id, deadline) {
        Some(Ok(i)) => Bounded { slot: Some(i) },
        Some(Err(_)) | None => {
            BOUNDED_LIVE.fetch_sub(1, Ordering::AcqRel);
            Bounded { slot: None }
        }
    }
}

impl Drop for Bounded {
    fn drop(&mut self) {
        if let Some(i) = self.slot {
            BOUNDED.leave(i);
            BOUNDED_LIVE.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The deadline of the calling thread's bounded section, if it is in one.
fn bounded_deadline(id: u32) -> Option<u32> {
    if BOUNDED_LIVE.load(Ordering::Acquire) == 0 {
        return None;
    }
    BOUNDED.find(id)
}

/// Milliseconds left of the calling thread's bounded section (`None`: not in one). For the
/// waits that take a timeout of their own (`ctrl::wait_fence`).
pub(crate) fn bounded_left_ms() -> Option<u32> {
    let deadline = bounded_deadline(thread_id())?;
    wb::remaining_ms(now_ms(), deadline)
}

/// Whether the calling thread's bounded section is spent (always `false` outside one, and for
/// an escape: this is for the loops that must not change for escapes).
pub(crate) fn bounded_spent() -> bool {
    bounded_left_ms() == Some(0)
}

/// The calling thread's escape scope: registered by [`begin`], released on drop.
pub(crate) struct Scope {
    /// The table slot this scope owns (`None`: a nested scope, a full table, no thread id).
    slot: Option<usize>,
    started_ms: u32,
}

/// Enter an escape: refuse (`Err`) when the device is already stopping, else register the thread
/// with a deadline of `EscWaitMs` from now. PASSIVE.
pub(crate) fn begin() -> Result<Scope, ()> {
    if stopping() {
        REFUSED_STOPPING.fetch_add(1, Ordering::Relaxed);
        return Err(());
    }
    let now = now_ms();
    let deadline = wb::deadline_for(now, ESC_WAIT_MS.load(Ordering::Relaxed));
    let slot = match TABLE.enter(thread_id(), deadline) {
        Some(Ok(i)) => {
            SCOPES.fetch_add(1, Ordering::Relaxed);
            Some(i)
        }
        // Nested (an escape handler that calls another escape handler): the outer scope stays.
        Some(Err(_)) => None,
        None => {
            NO_SLOT.fetch_add(1, Ordering::Relaxed);
            None
        }
    };
    Ok(Scope { slot, started_ms: now })
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(i) = self.slot {
            TABLE.leave(i);
            let took = now_ms().wrapping_sub(self.started_ms);
            // A wrapped clock reads as a huge value: ignore it rather than record 49 days.
            if took < 0x8000_0000 {
                LONGEST_MS.fetch_max(took, Ordering::Relaxed);
            }
        }
    }
}

/// What the calling thread's waits can see right now.
pub(crate) fn probe() -> wb::Probe {
    let id = thread_id();
    let Some(deadline_ms) = TABLE.find(id) else {
        // Not an escape: a bounded section's deadline, or nothing (one relaxed load when no
        // section is open: always, with `GuestBlob` 0).
        return match bounded_deadline(id) {
            Some(deadline_ms) => wb::Probe {
                scoped: true,
                terminating: false,
                stopping: false,
                now_ms: now_ms(),
                deadline_ms,
            },
            None => wb::Probe::default(),
        };
    };
    // SAFETY: both are scalar reads on the current thread, legal at PASSIVE.
    let terminating = unsafe { PsIsThreadTerminating(crate::ddi::mirror_thread::current_thread()) } != 0;
    wb::Probe {
        scoped: true,
        terminating,
        stopping: stopping(),
        now_ms: now_ms(),
        deadline_ms,
    }
}

/// Whether the calling thread is inside an escape (one table scan).
pub(crate) fn scoped() -> bool {
    TABLE.find(thread_id()).is_some()
}

/// The wait must give up now: why, or `None` (always `None` outside an escape). Does not count;
/// the wait that gives up calls [`note_abort`] once.
pub(crate) fn abort_now() -> Option<wb::Abort> {
    wb::verdict(probe())
}

/// A wait gave up for `why`: count it (an escape's; a bounded section's caller counts its own).
pub(crate) fn note_abort(why: wb::Abort) {
    if BOUNDED_LIVE.load(Ordering::Acquire) != 0 && TABLE.find(thread_id()).is_none() {
        return;
    }
    let counter = match why {
        wb::Abort::Killed => &ABORT_KILL,
        wb::Abort::Stopping => &ABORT_STOP,
        wb::Abort::Deadline => &TIMEOUTS,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A mutex acquire gave up (`LkWaitAbort`), whatever the reason.
pub(crate) fn note_lock_abort(why: wb::Abort) {
    LOCK_ABORTS.fetch_add(1, Ordering::Relaxed);
    note_abort(why);
}

/// Cut a wait's total to what is left of the escape's deadline: the old total outside an escape
/// or with no deadline. A scoped wait whose deadline is spent gets 0 (it gives up at once).
pub(crate) fn bound_total_ms(total_ms: u64) -> u64 {
    let p = probe();
    if !p.scoped {
        return total_ms;
    }
    match wb::remaining_ms(p.now_ms, p.deadline_ms) {
        Some(left) => total_ms.min(left as u64),
        None => total_ms,
    }
}

/// Mirror the counters to the service key. PASSIVE (the mirror thread, via
/// `stall_diag::publish_counters`).
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    rec(b"EscWaitN", SCOPES.load(Ordering::Relaxed));
    rec(b"EscWaitMax", LONGEST_MS.load(Ordering::Relaxed));
    rec(b"EscAbortKill", ABORT_KILL.load(Ordering::Relaxed));
    rec(b"EscAbortStop", ABORT_STOP.load(Ordering::Relaxed));
    rec(b"EscTimeout", TIMEOUTS.load(Ordering::Relaxed));
    rec(b"LkWaitAbort", LOCK_ABORTS.load(Ordering::Relaxed));
    rec(b"EscNoSlot", NO_SLOT.load(Ordering::Relaxed));
    rec(b"EscRefStop", REFUSED_STOPPING.load(Ordering::Relaxed));
    rec(b"PreFence", PRE_FENCE.load(Ordering::Relaxed));
    rec(b"PreLastCmp", PRE_LAST.load(Ordering::Relaxed));
    rec(b"PreDropped", PRE_DROPPED.load(Ordering::Relaxed));
    rec(b"PreStatus", PRE_STATUS.load(Ordering::Relaxed));
    rec(b"PreT", PRE_T.load(Ordering::Relaxed));
}

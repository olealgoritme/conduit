//! The failing-call rings (`helios_kmd_logic::vk_fail`): the I/O half.
//!
//! `VkF` (32 entries): DDIs, NVDA escapes, HW-queue and copy-context
//! submissions, allocations and page-table refusals that did not do what was
//! asked. `VkR` (16 entries): RM statuses (an NVDA RM control answered with RM
//! status != 0, a KMD-forwarded RM ioctl the host or RM refused).
//!
//! Recording ([`note`], [`note_rm`]) is a spinlock hold of a few stores, a clock
//! read and `PsGetCurrentProcessId`: callable at PASSIVE or DISPATCH (never at
//! DIRQL). It asks the registry mirror for a pass (`hot_path::tick`, rate limited
//! by `NvHotPubMs`). The mirror's pass ([`publish`], PASSIVE) writes:
//!
//! * `VkFN` / `VkRN`: events recorded (repeats included); `VkFLen` / `VkRLen`
//!   entries held; `VkNkKK` per kind (both rings, `KK` the kind in hex).
//! * per entry, newest first (`00` = the latest): `VkFiiK` = kind << 24 | pid,
//!   `VkFiiC` code, `VkFiiS` status (NTSTATUS, RM status or host errno),
//!   `VkFiiH` hint, `VkFiiT` interrupt-time ms of the latest occurrence,
//!   `VkFiiR` back-to-back repeats; the same for `VkRii*`.
//! * `VkWait*`: the ICD's wait poll (NVDA 0x01000021, `vf::WaitStreaks`, keyed by context):
//!   `VkWaitN` polls, the streak polled last (`VkWaitTok` token, `VkWaitCurMs`
//!   its length, `VkWaitCurN` its polls, `VkWaitAgeMs` since its last poll), the
//!   longest (`VkWaitMaxMs`, `VkWaitMaxTok`, `VkWaitMaxN`). A streak past 1 s is
//!   also a `LONG_WAIT` entry in `VkF` (again at 2 s, 4 s, ...).
//! * `VkCcRej` copy-context submissions whose work did not reach the GPU,
//!   `VkCcDef` submissions that waited in the copy backlog, `VkSoftNv`
//!   submissions to NVDA queues without a channel (completed, never run).
//!
//! Kinds, codes and hints: `helios_kmd_logic::vk_fail::kind` and its reason
//! modules. Unused entries are written 0 once, so a previous boot's values never
//! read as this one's.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::vk_fail::{self as vf, Entry, Ring, WaitStreaks};

use crate::sync::SpinLock;

const DDI_ENTRIES: usize = 32;
const RM_ENTRIES: usize = 16;

static DDI: SpinLock<Ring<DDI_ENTRIES>> = SpinLock::new(Ring::new());
static RM: SpinLock<Ring<RM_ENTRIES>> = SpinLock::new(Ring::new());
static WAITS: SpinLock<WaitStreaks> = SpinLock::new(WaitStreaks::new());

pub(crate) static COPY_REJECTED: AtomicU32 = AtomicU32::new(0);
pub(crate) static COPY_DEFERRED: AtomicU32 = AtomicU32::new(0);
pub(crate) static SOFT_NVDA: AtomicU32 = AtomicU32::new(0);

/// What the last pass wrote (ring versions), so an unchanged ring is not walked.
static PUBLISHED_DDI: AtomicU32 = AtomicU32::new(u32::MAX);
static PUBLISHED_RM: AtomicU32 = AtomicU32::new(u32::MAX);

fn pid() -> u32 {
    // SAFETY: no preconditions at or below DISPATCH_LEVEL.
    unsafe { wdk_sys::ntddk::PsGetCurrentProcessId() as usize as u32 }
}

fn entry(kind: u8, code: u32, status: u32, hint: u32) -> Entry {
    Entry::new(
        kind,
        code,
        status,
        pid(),
        hint,
        crate::adapter::AdapterContext::interrupt_time_ms(),
    )
}

/// Calls made above DISPATCH (not recorded: the rings are spinlocked), `VkHiIrql`.
static ABOVE_DISPATCH: AtomicU32 = AtomicU32::new(0);

fn recordable() -> bool {
    // SAFETY: callable at any IRQL.
    if unsafe { wdk_sys::ntddk::KeGetCurrentIrql() } > wdk_sys::DISPATCH_LEVEL as u8 {
        ABOVE_DISPATCH.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

/// Record a failing call in `VkF`. `status` is the NTSTATUS answered (0 for the
/// informational kinds). Any IRQL; recorded at PASSIVE or DISPATCH.
pub(crate) fn note(kind: u8, code: u32, status: i32, hint: u32) {
    if !recordable() {
        return;
    }
    let e = entry(kind, code, status as u32, hint);
    DDI.lock().push(e);
    crate::ddi::hot_path::tick();
}

/// Record an RM status in `VkR`. Any IRQL; recorded at PASSIVE or DISPATCH.
pub(crate) fn note_rm(kind: u8, code: u32, status: u32, hint: u32) {
    if !recordable() {
        return;
    }
    let e = entry(kind, code, status, hint);
    RM.lock().push(e);
    crate::ddi::hot_path::tick();
}

/// A KMD-forwarded RM ioctl's answer (`virtio::nvrm::forward`): recorded when the
/// host or RM refused it. PASSIVE.
pub(crate) fn note_rm_ioctl(req: &[u8], resp: &[u8], n: usize) {
    if let Some(f) = vf::rm_ioctl_failure(req, resp, n) {
        note_rm(f.kind, f.code, f.status, f.hint);
    }
}

/// The Vulkan ICD polled its wait escape (0x01000021) under `key`
/// (`vf::wait_key`): a streak past `WAIT_REPORT_MS` (and every doubling) goes into
/// `VkF` as `LONG_WAIT`. Returns the streak's polls so far. PASSIVE.
pub(crate) fn note_wait_poll(key: u32) -> u32 {
    let now = crate::adapter::AdapterContext::interrupt_time_ms();
    let (report, count) = {
        let mut waits = WAITS.lock();
        let report = waits.poll(key, now);
        (report, waits.last().count)
    };
    if let Some(ms) = report {
        note(vf::kind::LONG_WAIT, key, 0, ms);
    }
    count
}

/// A KMD `RmBatch`'s answer (`virtio::nvrm::forward_kmd_batch`): each failed
/// entry. PASSIVE.
pub(crate) fn note_rm_batch(req: &[u8], resp: &[u8], n: usize) {
    vf::rm_batch_failures(req, resp, n, |f| note_rm(f.kind, f.code, f.status, f.hint));
}

fn dump<const N: usize>(ring: &Ring<N>, prefix: [u8; 3], first: bool) {
    use crate::diag::record_named_bytes as rec;
    for i in 0..N {
        let e = ring.newest(i);
        if e.is_none() && !first {
            // Past the held entries: written 0 by the first pass already, and a ring
            // only ever grows until it wraps.
            break;
        }
        let e = e.unwrap_or(Entry::EMPTY);
        for field in vf::FIELDS {
            rec(&vf::entry_name(prefix, i, field), vf::field_value(&e, field));
        }
    }
}

/// The registry mirror's pass (PASSIVE): both rings when they moved, and the
/// counters.
pub(crate) fn publish() {
    use crate::diag::record_named_bytes as rec;
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    let ddi = *DDI.lock();
    let rm = *RM.lock();
    let first = PUBLISHED_DDI.load(Ordering::Relaxed) == u32::MAX;
    if first || ddi.version() != PUBLISHED_DDI.load(Ordering::Relaxed) {
        PUBLISHED_DDI.store(ddi.version(), Ordering::Relaxed);
        rec(b"VkFN", ddi.total());
        rec(b"VkFLen", ddi.len() as u32);
        dump(&ddi, *b"VkF", first);
    }
    if first || rm.version() != PUBLISHED_RM.load(Ordering::Relaxed) {
        PUBLISHED_RM.store(rm.version(), Ordering::Relaxed);
        rec(b"VkRN", rm.total());
        rec(b"VkRLen", rm.len() as u32);
        dump(&rm, *b"VkR", first);
    }
    for kind in 1..vf::kind::MAX as u8 {
        let n = ddi.per_kind(kind).wrapping_add(rm.per_kind(kind));
        if n != 0 || first {
            rec(&vf::kind_counter_name(kind), n);
        }
    }
    let waits = *WAITS.lock();
    if waits.polls != 0 {
        let last = waits.last();
        rec(b"VkWaitN", waits.polls);
        rec(b"VkWaitTok", last.token);
        rec(b"VkWaitCurMs", last.ms());
        rec(b"VkWaitCurN", last.count);
        rec(b"VkWaitAgeMs", crate::adapter::AdapterContext::interrupt_time_ms().wrapping_sub(last.last_ms));
        rec(b"VkWaitMaxMs", waits.max_ms);
        rec(b"VkWaitMaxTok", waits.max_token);
        rec(b"VkWaitMaxN", waits.max_count);
    }
    rec(b"VkCcRej", r(&COPY_REJECTED));
    rec(b"VkCcDef", r(&COPY_DEFERRED));
    rec(b"VkSoftNv", r(&SOFT_NVDA));
    rec(b"VkHiIrql", r(&ABOVE_DISPATCH));
}

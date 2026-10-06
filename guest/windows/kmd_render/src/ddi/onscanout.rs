//! The "already on scanout" present tag (`HOSC`): the I/O half. The decisions and the parse are
//! `helios_kmd_logic::onscanout` (host-tested); this file reads the tag out of the `HERF` Render
//! command, keeps the per-source record of what `SCANOUT_PRESENT` minted, resolves the facts a
//! verdict needs and counts what it decides. Wire layout: `protocol/src/onscanout.rs`. Design,
//! the verification rule and the hardware checklist: `docs/zero-copy-present.md`,
//! "Already-on-scanout present tag".
//!
//! THE PAIRING. dxgkrnl does not forward `pfnPresentCb`'s `pPrivateDriverData` to `DxgkDdiPresent`,
//! so the tag rides the `HERF` command the UMD submits with `pfnRenderCb` right before the Present,
//! on the same context. `DxgkDdiRender` calls [`note_render`] for every `HERF` (a command without a
//! tag clears a stale stash, so a tag only ever reaches the Present that follows its own Render);
//! `DxgkDdiPresent` takes the stash on every Present that resolves its context, whatever its arm.
//!
//! WHAT IS TRUSTED. Nothing the tag says: [`decide`] reads the live user source's generation from
//! the foreign-scanout state machine, the newest sequence and the minting process from
//! [`note_minted`] (called by the two escape-side `SCANOUT_PRESENT` paths, `present` and `present_fenced`), and the presenting
//! process from the context's device. See `helios_kmd_logic::onscanout::verify`.
//!
//! Counters (`Os` prefix, at most 14 characters, the list is
//! `helios_kmd_logic::onscanout::COUNTERS`; atomics on the Present path, the registry only on the
//! first event and every 64th reject / 256th skip, the rest through [`publish_counters`] from
//! `publish_nvrm_counters`): `OsTag` claims seen, `OsSkip` presents completed with no copy,
//! `OsRej` claims not honoured with `OsRejWhy` the last reason and `OsWhyMask` every reason seen,
//! `OsBytes` MiB of copy avoided, `OsLast` the last honoured sequence (low 32 bits).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::onscanout::{self as os, Facts, Parsed, Shown, Tag, Verdict, Why};
use helios_kmd_logic::present_foreign::Arm as PresentArm;

use crate::ddi::create_allocation::PresentAllocInfo;
use crate::device::{ContextHandleRef, DeviceHandleRef};
use crate::sync::SpinLock;
use crate::virtio::gpu::DeviceOwner;

// The wire constants `helios_kmd_logic` spells as literals are the protocol's.
const _: () = assert!(
    os::MAGIC == helios_protocol::HELIOS_ONSCANOUT_MAGIC
        && os::VERSION == helios_protocol::HELIOS_ONSCANOUT_VERSION
        && os::TAG_BYTES == core::mem::size_of::<helios_protocol::HeliosOnScanoutTag>()
        && os::HERF_OFFSET == helios_protocol::HELIOS_ONSCANOUT_HERF_OFFSET
        && os::HERF_BYTES == helios_protocol::HELIOS_ONSCANOUT_HERF_BYTES
        && os::MAX_LAG == helios_protocol::HELIOS_ONSCANOUT_MAX_LAG
);

/// Claims seen (`OsTag`), presents completed with no copy (`OsSkip`), claims refused (`OsRej`).
static TAGS: AtomicU32 = AtomicU32::new(0);
static SKIPS: AtomicU32 = AtomicU32::new(0);
static REJECTS: AtomicU32 = AtomicU32::new(0);
/// The last refusal's reason (`OsRejWhy`) and every reason seen (`OsWhyMask`, bit `code - 1`).
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static WHY_MASK: AtomicU32 = AtomicU32::new(0);
/// Bytes of copy avoided (`OsBytes` publishes MiB) and the last honoured sequence (`OsLast`).
static BYTES: AtomicU64 = AtomicU64::new(0);
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// What `SCANOUT_PRESENT` last minted: the live user source's generation, the process that minted
/// it and the newest sequence. A LEAF spinlock: its holders call only the pure record. Written at
/// PASSIVE by the escape, read at PASSIVE by the Present.
static SHOWN: SpinLock<Shown> = SpinLock::new(Shown::new());

/// A `SCANOUT_PRESENT` of `owner` minted flip `sequence` of source `generation` (`present` and
/// `present_fenced`, the callers that run inside the owner's own escape, call this on success; the
/// workers that mint for a stored owner token, `ForeignFlip` and the KMD's presenter, never do:
/// their owner's device object may already be gone). The KMD's own presenter
/// (`DeviceOwner::KMD_RM`) is not a user source and is ignored. PASSIVE: `owner` is the escaping
/// device, live for this call.
pub(crate) fn note_minted(owner: DeviceOwner, generation: u32, sequence: u64) {
    if owner.raw() == DeviceOwner::KMD_RM.raw() {
        return;
    }
    // SAFETY: `owner` is the `hDevice` of the device whose escape is running, so the
    // `DeviceContext` behind it is live; only its creator-process token is read.
    let process = unsafe { DeviceHandleRef::from_raw(owner.raw() as crate::dxgk::HANDLE) }
        .map_or(0, |device| device.creator_process() as u64);
    SHOWN.lock().minted(generation, process, sequence);
}

/// `DxgkDdiRender` of a valid `HERF` command: read the tag out of its tail and stash it for the
/// Present that follows on `context`. `command` / `cmd_len` are the command's bytes.
///
/// A command with no tag still clears the stash, so a tag whose Present never came cannot pair with
/// a later, untagged Present (counted `OsRej`, reason `Orphan`). The ordinary command (32 or 48
/// bytes) costs one relaxed load.
///
/// # Safety
/// `cmd_len` bytes are readable at `command` (the Render's `CommandLength`).
pub(crate) unsafe fn note_render(
    context: &ContextHandleRef<'_>,
    command: *const u8,
    cmd_len: usize,
) {
    let parsed = if cmd_len > os::HERF_OFFSET {
        let take = (cmd_len - os::HERF_OFFSET).min(os::TAG_BYTES);
        let mut tail = [0u8; os::TAG_BYTES];
        // SAFETY: `take <= cmd_len - HERF_OFFSET`, so the range is inside the command; the local
        // buffer holds `TAG_BYTES >= take`.
        unsafe {
            core::ptr::copy_nonoverlapping(command.add(os::HERF_OFFSET), tail.as_mut_ptr(), take);
        }
        os::parse(&tail[..take])
    } else {
        Parsed::Absent
    };
    let stash = match parsed {
        Parsed::Absent => None,
        Parsed::Tag(tag) => {
            TAGS.fetch_add(1, Ordering::Relaxed);
            Some(tag)
        }
        // Malformed: resolved here, never stashed.
        Parsed::Reject(why) => {
            TAGS.fetch_add(1, Ordering::Relaxed);
            note_reject(why);
            None
        }
    };
    if context.stash_onscanout_tag(stash) {
        // The previous Render's tag was never taken by a Present.
        note_reject(Why::Orphan);
    }
}

/// A Blt Present resolved the tag its Render left: may it be completed with no copy? `true` is
/// provisional: the caller then takes the skip through [`note_skip`] (or [`note_retry`] when its
/// own preconditions refuse it). `false` has been counted.
///
/// `arm` is the Present's contract, `no_allocations` the absence of an allocation list,
/// `color_fill` / `sub_rects` the shape of the Blt, `snapshot` whether a windowed-Blt snapshot
/// accompanies it and `source` the resolved source allocation (`None` if it did not resolve).
#[allow(clippy::too_many_arguments)]
pub(crate) fn decide(
    context: Option<&ContextHandleRef<'_>>,
    tag: Tag,
    arm: PresentArm,
    no_allocations: bool,
    color_fill: bool,
    sub_rects: bool,
    snapshot: bool,
    source: Option<&PresentAllocInfo>,
) -> bool {
    let live = crate::adapter::foreign_scanout::live_user_generation()
        .and_then(|generation| SHOWN.lock().live_for(generation));
    let facts = Facts {
        arm,
        no_allocations,
        color_fill,
        sub_rects,
        snapshot,
        presenter_process: context
            .and_then(ContextHandleRef::creator_process)
            .unwrap_or(0) as u64,
        source_resource_id: source.map_or(0, |s| s.resource_id),
        live,
    };
    match os::verify(&tag, &facts) {
        Verdict::Skip => true,
        Verdict::Reject(why) => {
            note_reject(why);
            false
        }
    }
}

/// The Present is completed with no copy: count it. `source` sizes the copy that did not happen.
pub(crate) fn note_skip(tag: Tag, source: Option<&PresentAllocInfo>) {
    let n = SKIPS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    LAST_SEQ.store(tag.sequence, Ordering::Relaxed);
    if let Some(s) = source {
        BYTES.fetch_add(
            os::extent_bytes(s.width, s.height, s.pitch),
            Ordering::Relaxed,
        );
    }
    // The first skip reaches the registry at once, then every 256th (a registry write is
    // synchronous; this is the frame rate); `publish_counters` mirrors the rest.
    if n == 1 || n % 256 == 0 {
        crate::diag::record_named_bytes(b"OsTag", TAGS.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"OsSkip", n);
        crate::diag::record_named_bytes(b"OsBytes", (BYTES.load(Ordering::Relaxed) >> 20) as u32);
        crate::diag::record_named_bytes(b"OsLast", tag.sequence as u32);
    }
}

/// The skip was verified but the Present could not take it (a DMA or private buffer too small, no
/// patch capacity): dxgkrnl retries it, and the retry carries no tag, so it is the ordinary Blt.
pub(crate) fn note_retry() {
    note_reject(Why::Retry);
}

fn note_reject(why: Why) {
    let n = REJECTS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    let bit = 1u32 << (why.code() - 1);
    let seen = WHY_MASK.fetch_or(bit, Ordering::Relaxed);
    // The first rejection, each new reason and every 64th reach the registry at once.
    if n == 1 || seen & bit == 0 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"OsTag", TAGS.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"OsRej", n);
        crate::diag::record_named_bytes(b"OsRejWhy", why.code());
        crate::diag::record_named_bytes(b"OsWhyMask", seen | bit);
    }
}

/// A new generation (StartDevice): zero the counters and the record, and write zeros over their
/// service-key values. They are mirrored only once an event happened, so without this a block from
/// an earlier run stays readable as this one's. PASSIVE.
pub(crate) fn reset_for_start() {
    for c in [&TAGS, &SKIPS, &REJECTS, &LAST_WHY, &WHY_MASK] {
        c.store(0, Ordering::Relaxed);
    }
    BYTES.store(0, Ordering::Relaxed);
    LAST_SEQ.store(0, Ordering::Relaxed);
    *SHOWN.lock() = Shown::new();
    for name in os::COUNTERS {
        crate::diag::record_named_bytes(name.as_bytes(), 0);
    }
}

/// Mirror the counters to the service key, once an event happened. PASSIVE only; with the NVRM
/// counters.
pub(crate) fn publish_counters() {
    let tags = TAGS.load(Ordering::Relaxed);
    if tags == 0 && REJECTS.load(Ordering::Relaxed) == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"OsTag", tags);
    rec(b"OsSkip", SKIPS.load(Ordering::Relaxed));
    rec(b"OsRej", REJECTS.load(Ordering::Relaxed));
    rec(b"OsRejWhy", LAST_WHY.load(Ordering::Relaxed));
    rec(b"OsWhyMask", WHY_MASK.load(Ordering::Relaxed));
    rec(b"OsBytes", (BYTES.load(Ordering::Relaxed) >> 20) as u32);
    rec(b"OsLast", LAST_SEQ.load(Ordering::Relaxed) as u32);
}

//! `DxgkDdiPresent` never fails because of a foreign (NVK/RM-backed) allocation: the I/O half.
//! The decision is `helios_kmd_logic::present_foreign` (host-tested); this file resolves the
//! facts it needs, counts what it decides, and carries the early-return site breadcrumb.
//! Design: `docs/zero-copy-present.md`, "Present never fails on a foreign source".
//!
//! Counters (names at most 13 characters; atomics on the Present path, no registry write per
//! Present: the first skip and every 64th reach the registry at once, the rest through
//! `publish_counters` from `publish_nvrm_counters`):
//!
//! * `PrFgSkip`: Presents (or checks) a foreign allocation would have failed, answered with success.
//! * `PrFgWhy`: the last skip's reason, `present_foreign::Why::code`
//!   (`arm << 12 | destination << 9 | source << 8 | refusal`).
//! * `PrFgBlt`, `PrFgFlip`: the same, per arm (the flip count holds both contracts).
//! * `PrFgHand`: DMA flips of a foreign allocation armed for `ForeignFlip`'s programming (not
//!   skips: the flip proceeds; `FfProg` / `FfRef*` say what the programming did).
//! * `PBRetSite`: the site id (`present_foreign::site`) of the last non-success return of
//!   `DxgkDdiPresent`; every return of the inner function names one, 0 only for a status that
//!   did not come from it.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::present_foreign::{
    self as pf, AllocFacts, Arm, Effect, FlipRoute, Refusal, Verdict,
};

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::PresentAllocInfo;
use crate::dxgk::*;

static SKIPS: AtomicU32 = AtomicU32::new(0);
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static BLT_SKIPS: AtomicU32 = AtomicU32::new(0);
static FLIP_SKIPS: AtomicU32 = AtomicU32::new(0);
/// DMA flips of a foreign allocation armed for `ForeignFlip`'s programming (`PrFgHand`).
static HANDED: AtomicU32 = AtomicU32::new(0);

/// The site of the current (or last) Present's non-success return; reset at each call.
static CALL_SITE: AtomicU32 = AtomicU32::new(0);
/// The last site written to the registry, and how many failures since.
static RECORDED_SITE: AtomicU32 = AtomicU32::new(0);
static SITE_FAILURES: AtomicU32 = AtomicU32::new(0);

/// What the KMD knows about one entry. The table is consulted only when the lock-free identity
/// flag did not already say foreign, and only because a refusal is about to be decided.
fn facts(adapter: Option<&AdapterContext>, info: Option<&PresentAllocInfo>) -> Option<AllocFacts> {
    let info = info?;
    let mut facts = AllocFacts {
        kind: info.kind,
        identity_foreign: info.foreign_identity,
        table_record: false,
    };
    if !facts.is_foreign() {
        facts.table_record = adapter
            .and_then(|adapter| {
                adapter
                    .with_virtio(|v| v.foreign_record(info.resource_id).is_some())
                    .ok()
            })
            .unwrap_or(false);
    }
    Some(facts)
}

// The kinds `helios_kmd_logic` spells as literals are the protocol's.
const _: () = assert!(
    pf::KIND_DEVICE_MEMORY == helios_protocol::HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY
        && pf::KIND_STANDARD == helios_protocol::HELIOS_WDDM_ALLOC_KIND_STANDARD
);

/// A refusal of `arm` is about to fail the Present. `Some(effect)` if a foreign allocation
/// caused it: the skip is counted and the caller answers with success and does `effect`;
/// `None` keeps the failure (an ordinary allocation, or a refusal that is not a foreign one).
pub(crate) fn skip(
    arm: Arm,
    refusal: Refusal,
    adapter: Option<&AdapterContext>,
    source: Option<&PresentAllocInfo>,
    destination: Option<&PresentAllocInfo>,
) -> Option<Effect> {
    let Verdict::Skip { why, effect } = pf::decide(
        arm,
        refusal,
        facts(adapter, source),
        facts(adapter, destination),
    ) else {
        return None;
    };
    note_skip(why);
    Some(effect)
}

/// Count one skip (`PrFgSkip`, `PrFgWhy`, the per-arm count).
fn note_skip(why: pf::Why) {
    let n = SKIPS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    if why.arm.is_flip() {
        FLIP_SKIPS.fetch_add(1, Ordering::Relaxed);
    } else {
        BLT_SKIPS.fetch_add(1, Ordering::Relaxed);
    }
    // The first and every 64th skip reach the registry at once; `publish_counters` mirrors the rest.
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"PrFgWhy", why.code());
        crate::diag::record_named_bytes(b"PrFgSkip", n);
    }
}

/// A Blt has an unresolved source or destination handle (`PBCpy` 0xE1): `true` if the Present is
/// a counted success instead (the transport holds a live foreign record, or `ForeignFlip` is on;
/// a Venus-only session keeps failing). Only called at the refusal, so the table lock is taken
/// only then.
pub(crate) fn unresolved_skip(
    adapter: &AdapterContext,
    source: Option<&PresentAllocInfo>,
    destination: Option<&PresentAllocInfo>,
) -> bool {
    let foreign_flip = crate::virtio::foreign_flip::enabled();
    let live = foreign_flip
        || adapter
            .with_virtio(|v| v.foreign_live() != 0)
            .unwrap_or(false);
    match pf::decide_unresolved(
        Arm::Blt,
        source.is_some(),
        destination.is_some(),
        live,
        foreign_flip,
    ) {
        Verdict::Skip { why, .. } => {
            note_skip(why);
            true
        }
        Verdict::Proceed => false,
    }
}

/// Route a DMA flip (`pf::flip_route`): `Arm` (the flip is armed as every direct-scan-out flip
/// is; `foreign_flip` says the programming it reaches is `ForeignFlip`'s, counted `PrFgHand`),
/// `Fail` (an ordinary allocation: the failure it always was) or `Skip` (counted here).
/// `in_table`: the source's resource id has a global handle in the direct-scan-out table.
pub(crate) fn flip_route(
    adapter: Option<&AdapterContext>,
    in_table: bool,
    source: &PresentAllocInfo,
) -> FlipRoute {
    // Nothing to ask for the common flip: a direct-scan-out allocation in the table.
    if in_table && source.direct_scanout {
        return FlipRoute::Arm {
            foreign_flip: false,
        };
    }
    let route = pf::flip_route(
        crate::virtio::foreign_flip::enabled(),
        in_table,
        source.direct_scanout,
        facts(adapter, Some(source)),
    );
    match route {
        FlipRoute::Skip { why, .. } => note_skip(why),
        FlipRoute::Arm { foreign_flip: true } => note_handoff(),
        _ => {}
    }
    route
}

fn note_handoff() {
    let n = HANDED.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"PrFgHand", n);
    }
}

/// Mirror the counters to the service key. PASSIVE_LEVEL only; with the NVRM counters.
pub(crate) fn publish_counters() {
    let n = SKIPS.load(Ordering::Relaxed);
    let handed = HANDED.load(Ordering::Relaxed);
    if n == 0 && handed == 0 {
        return;
    }
    crate::diag::record_named_bytes(b"PrFgHand", handed);
    crate::diag::record_named_bytes(b"PrFgSkip", n);
    crate::diag::record_named_bytes(b"PrFgWhy", LAST_WHY.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PrFgBlt", BLT_SKIPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"PrFgFlip", FLIP_SKIPS.load(Ordering::Relaxed));
}

/// Start of one `DxgkDdiPresent` call: no site yet.
#[inline]
pub(crate) fn begin_call() {
    CALL_SITE.store(0, Ordering::Relaxed);
}

/// Name the site of a non-success return, and hand the status back: `return site(SITE, status)`.
#[inline]
pub(crate) fn site(id: u32, status: NTSTATUS) -> NTSTATUS {
    CALL_SITE.store(id, Ordering::Relaxed);
    status
}

/// `STATUS_INVALID_PARAMETER` returned from `id`.
#[inline]
pub(crate) fn invalid(id: u32) -> NTSTATUS {
    site(id, STATUS_INVALID_PARAMETER)
}

/// End of one call that returned a non-success status: `PBRetSite` names the site, written
/// when it changes and then every 64th failure (failures repeat at the frame rate). PASSIVE_LEVEL.
pub(crate) fn note_failure() {
    let id = CALL_SITE.load(Ordering::Relaxed);
    let n = SITE_FAILURES
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    if RECORDED_SITE.swap(id, Ordering::Relaxed) != id || n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"PBRetSite", id);
    }
}

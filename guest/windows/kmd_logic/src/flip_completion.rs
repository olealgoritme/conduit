//! The flip-completion invariant for non-Venus primaries: what the KMD publishes toward dxgkrnl
//! when a flip of a foreign or hollow allocation could not be shown. The pure half: a decision table and
//! its constants. No memory, no transport, no clock; the I/O half is
//! `kmd_render/src/ddi/flip_keep.rs` and the arms in `ddi/display.rs`, `ddi/submit_command.rs`
//! and `virtio/gpu/mod.rs`. Design, unknowns and the hardware checklist:
//! `docs/zero-copy-present.md`, "Flip completion invariant for foreign primaries".
//!
//! THE INVARIANT. dxgkrnl retires a queued flip when a `DXGK_INTERRUPT_CRTC_VSYNC` carries the
//! flip's NEW `PhysicalAddress` (the driver's model, from `viogpu3d`'s `m_sourceAddress`; never
//! observed to be strict, see the doc). The KMD's CRTC_VSYNC carries
//! `AdapterContext::last_primary_address`. Before this module, that word was written only after a
//! programming that BOUND the allocation (a host `SET_SCANOUT_BLOB`, a finished copy, a level 5 or
//! `ForeignFlip` programming). For a foreign primary (DWM-on-NVK's swap chain: adopted
//! DEVICE_MEMORY, not direct scan-out) every one of those can fail or be switched off, and no exit
//! then completed the flip: the address never changed, dxgkrnl held the flip, DWM blocked after a
//! couple of presents.
//!
//! The KMD OWNS flip completion toward dxgkrnl; whether the pixels got to the screen is a
//! different question with its own counters. So a foreign primary that cannot be shown completes
//! its flip anyway by publishing its address as a KEPT picture: the address moves, the screen
//! keeps whatever it showed. [`Publish::Kept`] says that; [`Publish::Bound`] is the existing
//! publication after a real programming; [`Publish::None`] is "nothing new here".
//!
//! WHICH ALLOCATIONS. [`Source::Foreign`] (an adopted NVK-on-RM resource) and [`Source::Hollow`]
//! (an allocation with nothing the Venus path could ever show: no host resource at all, the
//! host-less shared placeholder, or a non-direct allocation the copy provably refuses), told
//! apart from a [`Source::Venus`] one by [`classify`]. The first hardware run of `ForeignFlip`
//! (T3: four frames shown, then a stall behind one flip of an allocation with no foreign record)
//! is why Hollow exists: the invariant is not about the word "foreign" but about "the Venus path
//! cannot bind this".
//!
//! NOTHING CHANGES FOR A VENUS ALLOCATION: [`decide`] answers [`Publish::None`] for every
//! outcome of a [`Source::Venus`] except [`Outcome::Programmed`], where it names the bound
//! publication the existing code already makes. [`Publish::Kept`] is never the answer for one.

/// Whose allocation the flip names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// An ordinary Venus allocation: every path as it was.
    Venus,
    /// An allocation that adopted a foreign (NVK-on-RM) resource: the KMD's own record
    /// (`AllocationContext::foreign`), never the creator's words.
    Foreign,
    /// An allocation with no foreign record and nothing the Venus path could show (see
    /// [`classify`]): the host-less shared placeholder (resource id 0), or one the scan-out copy
    /// refuses by construction. Completes exactly like a foreign one.
    Hollow,
}

/// What the KMD knows about one allocation, from its own create-time record (lock-free).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SourceFacts {
    /// The venus resource behind the allocation; 0 = none (a shared placeholder).
    pub resource_id: u32,
    /// The allocation adopted a foreign resource (`AllocationContext::foreign`).
    pub foreign: bool,
    /// `MISC_DIRECT_SCANOUT`: the Venus path binds the allocation's own resource.
    pub direct_scanout: bool,
    /// The creator's geometry (0 = the allocation carries none).
    pub width: u32,
    pub height: u32,
    /// The allocation has a Venus identity to import or copy from: a linear image id, or a
    /// recorded allocation size.
    pub venus_identity: bool,
}

/// Which rule an allocation's flip follows.
///
/// * a foreign adoption: [`Source::Foreign`];
/// * no resource id: [`Source::Hollow`] (nothing host-side exists);
/// * direct scan-out with a resource id: [`Source::Venus`] (the host binds its own resource);
/// * otherwise the scan-out COPY is the only way to show it, and `submit_primary_scanout_copy`
///   refuses a source whose geometry is not the programmed extent (`ctx.width != width`; an
///   allocation with no geometry is programmed at the mode's extent) and one with no Venus
///   identity to import: such an allocation can never be copied, so it is [`Source::Hollow`];
/// * anything else is [`Source::Venus`], byte-for-byte as before.
pub const fn classify(f: &SourceFacts) -> Source {
    if f.foreign {
        Source::Foreign
    } else if f.resource_id == 0 {
        Source::Hollow
    } else if f.direct_scanout {
        Source::Venus
    } else if f.width == 0 || f.height == 0 || !f.venus_identity {
        Source::Hollow
    } else {
        Source::Venus
    }
}

/// Whether the Venus path that follows a `ForeignFlip` decline can still show the allocation:
/// a foreign one when the `ForeignCopy` knob is on (imported and copied) or it is direct scan-out
/// (bound as its own resource); a hollow one never; a Venus one always (not asked).
pub const fn venus_can_bind(source: Source, foreign_copy: bool, direct_scanout: bool) -> bool {
    match source {
        Source::Venus => true,
        Source::Foreign => foreign_copy || direct_scanout,
        Source::Hollow => false,
    }
}

/// Which Present contract delivered the flip. It decides WHERE a kept publication is made, not
/// whether: the MMIO contract always parks `SetVidPnSourceAddress` for the worker, the DMA contract
/// either arms the same worker or (a foreign allocation the programming path cannot take) is
/// answered at the Present itself ([`Outcome::PresentSkip`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Contract {
    Mmio,
    Dma,
}

/// What the programming of one flip came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The programming bound the allocation (a host bind, a queued copy whose completion
    /// publishes, or `ForeignFlip` taking it): the bound publication is made by the existing code.
    Programmed,
    /// `ForeignFlip` declined without a refusal (knob off, no foreign record, the KMD's own
    /// system memory).
    NotOurs,
    /// `ForeignFlip` refused with a counted reason (any `foreign_flip::Why`: failing, host
    /// capability, file closed, shared format, ...).
    Refused,
    /// A retryable failure of the Venus path with retry budget left (the copy could not be
    /// submitted, the LINEAR target could not be minted, the host refused a bind): the gate is
    /// held and the same handle is programmed again.
    CopyFailed,
    /// The retry budget of [`Outcome::CopyFailed`] is exhausted: the interval is dropped.
    GaveUp,
    /// The primary's extent is not the mode's: a permanent reject, checked before any arm.
    Extent,
    /// Any other permanent reject (layout, format, producer abandoned).
    Rejected,
    /// The handle paired with no allocation of this transport generation (a stale or unknown
    /// handle): not a Venus allocation of the live host, so the address Windows named is
    /// completed. Without an address (the worker's lookup failed) nothing can be.
    Unresolved,
    /// DMA contract only: the Present itself found a foreign allocation the programming path
    /// cannot take and answered it with a counted success, arming nothing.
    PresentSkip,
    /// A queued copy's GPU completion failed (host error), seen by the ring-1 completion DPC.
    AsyncCopyFailed,
}

/// What to publish toward dxgkrnl for the flip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Publish {
    /// The existing publication after a real programming: the screen shows this picture.
    Bound,
    /// The flip's address, with the previous picture kept on the screen. Never for a Venus source.
    Kept,
    /// Nothing new: the existing path decides (continue to the Venus path, hold the gate for a
    /// retry, or a Venus failure that keeps its old behaviour).
    None,
}

/// The decision table. `venus_can_bind` ([`venus_can_bind`]) says the Venus path that follows a
/// `ForeignFlip` decline or refusal can still succeed: the `ForeignCopy` knob is on (the foreign
/// source is imported and copied), or the allocation is direct scan-out (its own resource is
/// bound by `SET_SCANOUT_BLOB`). Then a decline or refusal is not terminal: the path runs, its
/// success publishes the bound address and its failure ends in one of the rows below. With it off
/// that path can only fail (the host refuses a plain OPTIMAL import of a foreign resource, and a
/// hollow allocation has nothing to import), so the flip completes right there.
pub const fn decide(source: Source, venus_can_bind: bool, outcome: Outcome) -> Publish {
    if matches!(outcome, Outcome::Programmed) {
        return Publish::Bound;
    }
    if matches!(source, Source::Venus) {
        return Publish::None;
    }
    match outcome {
        Outcome::Programmed => Publish::Bound,
        Outcome::NotOurs | Outcome::Refused => {
            if venus_can_bind {
                Publish::None
            } else {
                Publish::Kept
            }
        }
        // Retrying: the gate is held, the budget decides, `GaveUp` completes.
        Outcome::CopyFailed => Publish::None,
        Outcome::GaveUp
        | Outcome::Extent
        | Outcome::Rejected
        | Outcome::PresentSkip
        | Outcome::AsyncCopyFailed => Publish::Kept,
        Outcome::Unresolved => Publish::Kept,
    }
}

/// Whether `outcome` can happen at all for these facts (the table's unreachable rows): the
/// Present-level skip belongs to the DMA contract and a non-Venus source, `ForeignFlip` cannot
/// refuse while its knob is off (it answers `NotOurs`) nor an allocation it has no record of
/// (`NotOurs` as well: only a foreign one is refused with a reason), and only a non-Venus
/// allocation is ever declined by it.
pub const fn reachable(
    contract: Contract,
    source: Source,
    foreign_flip: bool,
    outcome: Outcome,
) -> bool {
    match outcome {
        Outcome::PresentSkip => {
            matches!(contract, Contract::Dma) && !matches!(source, Source::Venus)
        }
        Outcome::Refused => foreign_flip && matches!(source, Source::Foreign),
        Outcome::NotOurs => !matches!(source, Source::Venus),
        _ => true,
    }
}

/// Why a flip was completed as a kept picture: the stable code `FkWhy` shows (nonzero) and the
/// per-reason counter `FkKeep<NN>` counts. Append, never renumber.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeepWhy {
    /// `ForeignFlip` declined (knob off, no foreign record) and the Venus path cannot bind.
    NotOurs = 1,
    /// `ForeignFlip` refused and the Venus path cannot bind.
    Refused = 2,
    /// The Venus path failed until its retry budget was spent.
    GaveUp = 3,
    /// The primary's extent is not the mode's.
    Extent = 4,
    /// Another permanent reject (layout, format, producer abandoned).
    Rejected = 5,
    /// The DMA Present answered a flip it could not arm.
    PresentSkip = 6,
    /// The queued copy's GPU completion failed.
    AsyncCopyFailed = 7,
    /// `SetVidPnSourceAddress` named a handle that pairs with no allocation of this transport
    /// generation.
    Unresolved = 8,
}

impl KeepWhy {
    pub const COUNT: usize = 8;

    pub const ALL: [KeepWhy; Self::COUNT] = [
        KeepWhy::NotOurs,
        KeepWhy::Refused,
        KeepWhy::GaveUp,
        KeepWhy::Extent,
        KeepWhy::Rejected,
        KeepWhy::PresentSkip,
        KeepWhy::AsyncCopyFailed,
        KeepWhy::Unresolved,
    ];

    /// Stable nonzero code, 1 to [`Self::COUNT`].
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Index into a counter array (`code() - 1`).
    pub const fn index(self) -> usize {
        self as usize - 1
    }

    /// The reason a [`Publish::Kept`] decision for `outcome` is counted under.
    pub const fn of(outcome: Outcome) -> Option<KeepWhy> {
        match outcome {
            Outcome::NotOurs => Some(KeepWhy::NotOurs),
            Outcome::Refused => Some(KeepWhy::Refused),
            Outcome::GaveUp => Some(KeepWhy::GaveUp),
            Outcome::Extent => Some(KeepWhy::Extent),
            Outcome::Rejected => Some(KeepWhy::Rejected),
            Outcome::PresentSkip => Some(KeepWhy::PresentSkip),
            Outcome::AsyncCopyFailed => Some(KeepWhy::AsyncCopyFailed),
            Outcome::Unresolved => Some(KeepWhy::Unresolved),
            Outcome::Programmed | Outcome::CopyFailed => None,
        }
    }
}

/// The address a kept publication may carry. Zero is `last_primary_address`'s "nothing published
/// yet": a flip whose assigned address is zero would complete nothing, and is not published.
pub const fn keep_address(address: u64) -> Option<u64> {
    if address == 0 {
        None
    } else {
        Some(address)
    }
}

/// Whether counter number `n` (1-based) may be written to the registry now: the first and every
/// 64th. Flips repeat at the frame rate, so a write per kept flip would be the per-frame registry
/// tax the other counters were written to avoid; the rest reach the registry through the periodic
/// mirror.
pub const fn mirror_due(n: u32) -> bool {
    n != 0 && (n == 1 || n % 64 == 0)
}

/// How long the HPD worker lets the host take one `ForeignFlip` (`SCANOUT_FLIP` round trip), in
/// milliseconds. It was one second, on the very worker that drains `pending_vidpn_allocation`
/// (and so publishes every later flip's address): a slow or silent host then delayed every
/// publication behind it by up to a second per attempt. A healthy host answers in milliseconds, so
/// this is a few frame periods; a timeout is the same failure it always was (the presenter's three
/// strikes, the retry pause), and with a kept publication behind every refusal the cost of a
/// spurious one is a stale picture for the pause, not a held flip. 100 ms was first; a tester's
/// run showed three 100 ms host stalls withdrawing the `ForeignFlip` source in about 0.6 s
/// (three strikes plus the retry pauses), so it is 250 ms.
pub const WORKER_FLIP_TIMEOUT_MS: u64 = 250;

/// The service-key counter names this module's I/O half writes (`kmd_render/src/ddi/flip_keep.rs`,
/// nothing else): at most 14 characters (`record_named_bytes` clamps there), all with the `Fk`
/// prefix no other counter uses. `FkKeep` total kept publications, of which `FkWorker` by the
/// programming worker, `FkDma` by the DMA lane at submit, `FkAsync` by the copy-completion DPC,
/// `FkDdi` by `SetVidPnSourceAddress` itself (an unpaired handle), `FkDmaRec` the DMA Presents
/// that wrote a keep record, `FkPhFlip` flips of a host-less placeholder the Present completed;
/// `FkWhy` the last reason ([`KeepWhy::code`]); `FkKeep01`..`FkKeep08`
/// per reason. `FkDefBud` and `FkVenus` are NOT flip completions: they count the two opt-in
/// diagnostic exits of `stall_diag` (a Deferred programming past its `DeferBudget` published
/// kept, and a Venus GaveUp / permanent reject published kept under `FlipWdogMs`); neither is in
/// `FkKeep` or `FkWhy`.
pub const COUNTERS: [&str; 18] = [
    "FkKeep", "FkWhy", "FkWorker", "FkDma", "FkAsync", "FkDdi", "FkDmaRec", "FkPhFlip", "FkKeep01",
    "FkKeep02",
    "FkKeep03", "FkKeep04", "FkKeep05", "FkKeep06", "FkKeep07", "FkKeep08",
    "FkDefBud", "FkVenus",
];

/// Name of the per-reason counter: `FkKeep01` .. `FkKeep08`.
pub const fn why_name(why: KeepWhy) -> [u8; 8] {
    let c = why.code();
    [
        b'F',
        b'k',
        b'K',
        b'e',
        b'e',
        b'p',
        b'0' + (c / 10) as u8,
        b'0' + (c % 10) as u8,
    ]
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const SOURCES: [Source; 3] = [Source::Venus, Source::Foreign, Source::Hollow];
    const CONTRACTS: [Contract; 2] = [Contract::Mmio, Contract::Dma];
    const OUTCOMES: [Outcome; 10] = [
        Outcome::Programmed,
        Outcome::NotOurs,
        Outcome::Refused,
        Outcome::CopyFailed,
        Outcome::GaveUp,
        Outcome::Extent,
        Outcome::Rejected,
        Outcome::Unresolved,
        Outcome::PresentSkip,
        Outcome::AsyncCopyFailed,
    ];

    /// The whole table, written out row by row (not derived from `decide`): contract x source x
    /// ForeignFlip x venus_can_bind (ForeignCopy, or direct scan-out) x outcome. `None` marks a
    /// row that cannot happen.
    ///
    /// Columns after the outcome: expected publication for venus_can_bind off, on.
    fn expected(
        contract: Contract,
        source: Source,
        outcome: Outcome,
    ) -> Option<(Publish, Publish)> {
        use Outcome as O;
        use Publish::{Bound, Kept, None as Nothing};
        // Venus: nothing is ever kept; the bound publication is the existing one.
        if source == Source::Venus {
            return match outcome {
                O::Programmed => Some((Bound, Bound)),
                // The ForeignFlip arm and the Present skip never complete a Venus allocation.
                O::NotOurs | O::Refused | O::PresentSkip => Option::None,
                _ => Some((Nothing, Nothing)),
            };
        }
        // Foreign and hollow follow one rule.
        match outcome {
            O::Programmed => Some((Bound, Bound)),
            // The ForeignFlip arm did not take it: with no Venus path that can bind, the flip
            // completes right here; with one, that path runs (and may still end in a row below).
            O::NotOurs => Some((Kept, Nothing)),
            // Only a foreign allocation (one with a record) is refused with a reason.
            O::Refused => match source {
                Source::Foreign => Some((Kept, Nothing)),
                _ => Option::None,
            },
            // Retrying: the gate is held; the budget's end is the next row.
            O::CopyFailed => Some((Nothing, Nothing)),
            O::GaveUp | O::Extent | O::Rejected | O::AsyncCopyFailed | O::Unresolved => {
                Some((Kept, Kept))
            }
            // The DMA Present's skip exists only on the DMA contract.
            O::PresentSkip => match contract {
                Contract::Dma => Some((Kept, Kept)),
                Contract::Mmio => Option::None,
            },
        }
    }

    #[test]
    fn the_full_table_matches_decide_and_reachable() {
        let mut rows = 0;
        let mut unreachable = 0;
        for contract in CONTRACTS {
            for source in SOURCES {
                for foreign_flip in [false, true] {
                    for outcome in OUTCOMES {
                        let want = expected(contract, source, outcome);
                        let reach = reachable(contract, source, foreign_flip, outcome);
                        match want {
                            // A row the table says cannot happen is never reachable.
                            None => {
                                assert!(
                                    !reach,
                                    "{contract:?} {source:?} ff={foreign_flip} {outcome:?} \
                                     is claimed reachable"
                                );
                                unreachable += 1;
                            }
                            Some((off, on)) => {
                                // ForeignFlip off cannot refuse (it answers NotOurs): that row
                                // is unreachable by the knob, not by the table.
                                if !reach {
                                    assert!(
                                        !foreign_flip && outcome == Outcome::Refused,
                                        "{contract:?} {source:?} ff={foreign_flip} {outcome:?} \
                                         is wrongly unreachable"
                                    );
                                    unreachable += 1;
                                    continue;
                                }
                                assert_eq!(
                                    decide(source, false, outcome),
                                    off,
                                    "{contract:?} {source:?} ff={foreign_flip} \
                                     venus_can_bind=off {outcome:?}"
                                );
                                assert_eq!(
                                    decide(source, true, outcome),
                                    on,
                                    "{contract:?} {source:?} ff={foreign_flip} \
                                     venus_can_bind=on {outcome:?}"
                                );
                                rows += 1;
                            }
                        }
                    }
                }
            }
        }
        // 2 contracts x 3 sources x 2 knobs x 10 outcomes = 120 rows.
        assert_eq!(rows + unreachable, 120);
        assert!(
            rows > 40,
            "the table must cover many reachable rows, got {rows}"
        );
    }

    #[test]
    fn venus_rows_publish_nothing_new_whatever_the_knobs() {
        for contract in CONTRACTS {
            for venus_can_bind in [false, true] {
                for foreign_flip in [false, true] {
                    for outcome in OUTCOMES {
                        if !reachable(contract, Source::Venus, foreign_flip, outcome) {
                            continue;
                        }
                        let p = decide(Source::Venus, venus_can_bind, outcome);
                        match outcome {
                            Outcome::Programmed => assert_eq!(p, Publish::Bound),
                            _ => assert_eq!(
                                p,
                                Publish::None,
                                "a Venus {outcome:?} must publish nothing new"
                            ),
                        }
                        assert_ne!(p, Publish::Kept, "Kept for a Venus allocation");
                    }
                }
            }
        }
    }

    #[test]
    fn kept_is_never_venus_and_never_for_a_programming_that_bound() {
        for source in SOURCES {
            for venus_can_bind in [false, true] {
                for outcome in OUTCOMES {
                    let p = decide(source, venus_can_bind, outcome);
                    if p == Publish::Kept {
                        assert_ne!(source, Source::Venus);
                        assert_ne!(outcome, Outcome::Programmed);
                    }
                    if outcome == Outcome::Programmed {
                        assert_eq!(p, Publish::Bound);
                    }
                }
            }
        }
    }

    #[test]
    fn every_foreign_dead_end_completes_the_flip() {
        // The invariant itself: every terminal exit of the foreign path publishes something.
        // A row that is not terminal (retrying, continuing to the copy, an unresolved handle) is
        // listed so a new outcome has to be placed deliberately.
        for source in [Source::Foreign, Source::Hollow] {
            for venus_can_bind in [false, true] {
                for outcome in OUTCOMES {
                    let p = decide(source, venus_can_bind, outcome);
                    let terminal_dead_end = matches!(
                        outcome,
                        Outcome::GaveUp
                            | Outcome::Extent
                            | Outcome::Rejected
                            | Outcome::PresentSkip
                            | Outcome::AsyncCopyFailed
                            | Outcome::Unresolved
                    );
                    if terminal_dead_end {
                        assert_eq!(
                            p,
                            Publish::Kept,
                            "{source:?} {outcome:?} venus_can_bind={venus_can_bind}"
                        );
                    }
                }
            }
        }
        // And with the Venus copy off there is no later stage to wait for: a decline or a
        // refusal completes at once (the original defect).
        assert_eq!(
            decide(Source::Foreign, false, Outcome::NotOurs),
            Publish::Kept
        );
        assert_eq!(
            decide(Source::Foreign, false, Outcome::Refused),
            Publish::Kept
        );
    }

    #[test]
    fn the_default_configuration_completes_a_foreign_flip_on_every_route() {
        // ForeignFlip off, ForeignCopy off (the defaults, measured to stall): the MMIO flip
        // reaches the worker, which ends in NotOurs; the DMA flip is either skipped at the
        // Present or armed and ends in the same NotOurs.
        for contract in CONTRACTS {
            assert_eq!(
                decide(Source::Foreign, false, Outcome::NotOurs),
                Publish::Kept,
                "{contract:?}"
            );
        }
        assert_eq!(
            decide(Source::Foreign, false, Outcome::PresentSkip),
            Publish::Kept
        );
    }

    #[test]
    fn keep_reasons_are_dense_stable_and_named_for_every_kept_outcome() {
        for (i, w) in KeepWhy::ALL.iter().enumerate() {
            assert_eq!(w.code() as usize, i + 1);
            assert_eq!(w.index(), i);
        }
        assert_eq!(KeepWhy::ALL.len(), KeepWhy::COUNT);
        // The codes shown by `FkWhy` are owner debugging ABI.
        assert_eq!(KeepWhy::NotOurs.code(), 1);
        assert_eq!(KeepWhy::Refused.code(), 2);
        assert_eq!(KeepWhy::GaveUp.code(), 3);
        assert_eq!(KeepWhy::Extent.code(), 4);
        assert_eq!(KeepWhy::Rejected.code(), 5);
        assert_eq!(KeepWhy::PresentSkip.code(), 6);
        assert_eq!(KeepWhy::AsyncCopyFailed.code(), 7);
        assert_eq!(KeepWhy::Unresolved.code(), 8);
        for outcome in OUTCOMES {
            // A reason exists exactly for the outcomes some knob setting keeps.
            let kept_somewhere = [false, true]
                .into_iter()
                .any(|c| decide(Source::Foreign, c, outcome) == Publish::Kept);
            assert_eq!(
                KeepWhy::of(outcome).is_some(),
                kept_somewhere,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn zero_is_never_published() {
        assert_eq!(keep_address(0), None);
        assert_eq!(keep_address(0x1_0000), Some(0x1_0000));
        assert_eq!(keep_address(u64::MAX), Some(u64::MAX));
    }

    #[test]
    fn registry_writes_are_the_first_and_every_64th() {
        let due: Vec<u32> = (0..=200).filter(|n| mirror_due(*n)).collect();
        assert_eq!(due, std::vec![1, 64, 128, 192]);
        // Flips repeat at the frame rate: 60 s of kept flips write a handful of values.
        let writes = (1..=3600u32).filter(|n| mirror_due(*n)).count();
        assert!(writes <= 1 + 3600 / 64);
    }

    #[test]
    fn the_worker_flip_timeout_is_a_few_frames_not_a_second() {
        // At least two 60 Hz frame periods (a loaded host must not fail spuriously), well under
        // the 1 s that held every later publication, and under the presenter's retry pause.
        assert!(WORKER_FLIP_TIMEOUT_MS >= 34);
        assert!(WORKER_FLIP_TIMEOUT_MS <= 250);
        // The one the user-mode `SCANOUT_PRESENT` and the level 5 presenter use is untouched.
        assert!(WORKER_FLIP_TIMEOUT_MS < 1_000);
    }

    // ---- classification, and the T3 rows ---------------------------------------------------

    fn facts() -> SourceFacts {
        SourceFacts {
            resource_id: 7,
            foreign: false,
            direct_scanout: false,
            width: 1920,
            height: 1080,
            venus_identity: true,
        }
    }

    #[test]
    fn classification_keeps_venus_allocations_venus() {
        // The plain Venus shapes: a direct primary, a copyable non-direct allocation.
        let mut f = facts();
        assert_eq!(classify(&f), Source::Venus);
        f.direct_scanout = true;
        assert_eq!(classify(&f), Source::Venus);
        // A direct allocation with no geometry or identity still binds its own resource: the
        // direct arm validates those itself and keeps its failure paths.
        f.width = 0;
        f.height = 0;
        f.venus_identity = false;
        assert_eq!(classify(&f), Source::Venus);
    }

    #[test]
    fn classification_names_what_the_venus_path_can_never_show() {
        // No resource id: the host-less shared placeholder.
        let mut f = facts();
        f.resource_id = 0;
        assert_eq!(classify(&f), Source::Hollow);
        f.direct_scanout = true;
        assert_eq!(
            classify(&f),
            Source::Hollow,
            "no resource, whatever the flags"
        );
        // A non-direct allocation the scan-out copy refuses by construction.
        for (w, h, id) in [
            (0, 1080, true),
            (1920, 0, true),
            (0, 0, true),
            (1920, 1080, false),
        ] {
            let mut f = facts();
            f.width = w;
            f.height = h;
            f.venus_identity = id;
            assert_eq!(classify(&f), Source::Hollow, "{w}x{h} identity={id}");
        }
        // A foreign adoption is foreign before anything else is asked.
        let mut f = facts();
        f.foreign = true;
        assert_eq!(classify(&f), Source::Foreign);
        f.resource_id = 0;
        assert_eq!(classify(&f), Source::Foreign);
    }

    #[test]
    fn venus_can_bind_follows_the_source_and_the_knob() {
        for copy in [false, true] {
            for direct in [false, true] {
                assert!(venus_can_bind(Source::Venus, copy, direct));
                assert_eq!(
                    venus_can_bind(Source::Foreign, copy, direct),
                    copy || direct
                );
                assert!(!venus_can_bind(Source::Hollow, copy, direct));
            }
        }
    }

    /// T3 (320.1, `ForeignFlip` = 1, DWM on NVK, 45 s): four foreign frames shown, then a stall
    /// behind ONE flip of an allocation with no foreign record (`FfNoRec` 1; `PrFgHand` 0, so
    /// the flips were MMIO). The Venus copy of such an allocation fails and nothing published.
    /// Both contracts, the knob on and off, the allocation the placeholder lane makes (no
    /// resource id) and a no-record one that has a resource id but cannot be copied.
    #[test]
    fn the_t3_no_record_flip_completes_on_every_route() {
        let placeholder = SourceFacts {
            resource_id: 0,
            ..facts()
        };
        let no_geometry = SourceFacts {
            width: 0,
            height: 0,
            ..facts()
        };
        for f in [placeholder, no_geometry] {
            let source = classify(&f);
            assert_eq!(source, Source::Hollow);
            for contract in CONTRACTS {
                for foreign_flip in [false, true] {
                    // `ForeignFlip` on says NoRecord (a decline), off says nothing: both NotOurs.
                    // The knobs that let a foreign source reach the copy do not apply.
                    for copy in [false, true] {
                        let can = venus_can_bind(source, copy, f.direct_scanout);
                        assert!(!can);
                        assert_eq!(
                            decide(source, can, Outcome::NotOurs),
                            Publish::Kept,
                            "{contract:?} ff={foreign_flip} copy={copy}"
                        );
                    }
                    // A resource id of 0 never reaches the arm: the worker rejects it, and the
                    // reject completes (ScRid 0).
                    assert_eq!(decide(source, false, Outcome::Rejected), Publish::Kept);
                    // The extent, a spent retry budget, a queue refusal (ScUnav: producer
                    // abandoned) and a failed copy completion all complete too.
                    for o in [
                        Outcome::Extent,
                        Outcome::GaveUp,
                        Outcome::AsyncCopyFailed,
                        Outcome::Unresolved,
                    ] {
                        assert_eq!(decide(source, false, o), Publish::Kept, "{o:?}");
                    }
                }
            }
        }
        // The DMA contract answers the same flip at the Present.
        assert_eq!(
            decide(Source::Hollow, false, Outcome::PresentSkip),
            Publish::Kept
        );
        // And the allocation that WAS shown (a foreign one, taken) is still bound, not kept.
        assert_eq!(
            decide(Source::Foreign, false, Outcome::Programmed),
            Publish::Bound
        );
    }

    // ---- counter names -----------------------------------------------------------------

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        let mut names: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        for w in KeepWhy::ALL {
            let n: std::string::String = std::str::from_utf8(&why_name(w)).unwrap().into();
            assert!(COUNTERS.contains(&n.as_str()), "{n} is not listed");
        }
        for n in &names {
            // `record_named_bytes` clamps to 14 characters; a longer name would be truncated and
            // could merge with another.
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Fk"));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        // No other name in this crate's own lists.
        for other in crate::foreign_flip::COUNTERS {
            assert!(!COUNTERS.contains(&other), "{other} collides");
        }
        // Nothing else in the driver writes a name starting with `Fk` (a literal in another
        // file would merge with ours); only the I/O half of this module may.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if !root.exists() {
            return; // a copy of this crate without its sibling: nothing to scan
        }
        let mut stack = std::vec![root];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    if p.file_name().is_some_and(|n| n == "flip_keep.rs") {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(
                        !text.contains("b\"Fk"),
                        "{} writes a counter named Fk*, the flip completion's prefix",
                        p.display()
                    );
                }
            }
        }
        assert!(checked > 20);
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kmd_render/src/ddi/flip_keep.rs");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut written: Vec<std::string::String> = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("b\"Fk") {
            let tail = &rest[i + 2..];
            let end = tail.find('"').unwrap();
            let name = &tail[..end];
            if !written.iter().any(|w| w == name) {
                written.push(name.into());
            }
            rest = &tail[end..];
        }
        // The per-reason names are built by `why_name`, not spelled; everything else is.
        let mut listed: Vec<std::string::String> = COUNTERS
            .iter()
            .filter(|n| !n.starts_with("FkKeep0"))
            .map(|s| (*s).into())
            .collect();
        written.sort();
        listed.sort();
        assert_eq!(written, listed);
    }
}

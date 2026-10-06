//! `DxgkDdiPresent` never fails because of a foreign (NVK/RM-backed) allocation. The pure half:
//! which allocation counts as foreign, which validation refusals a foreign allocation turns into
//! a counted success, and what the Present then does instead. No memory, no transport, no
//! clock; the I/O half is `kmd_render/src/ddi/present_foreign.rs` and the arms in
//! `kmd_render/src/ddi/display.rs`. Design: `docs/zero-copy-present.md`, "Present never fails on
//! a foreign source".
//!
//! WHY. The Present arms validate their source and destination before any copy (format,
//! kind, extent, descriptor, scan-out table) and return `STATUS_INVALID_PARAMETER` for a
//! surface they cannot handle. For an ordinary Venus allocation that is a real defect worth a
//! loud failure. A foreign allocation is RM memory the KMD adopted for a Vulkan driver it does
//! not control (NVK on RM): its swap-chain buffers have shapes the Venus arms were never written
//! for, and dxgkrnl turns a failed Present into a device error for DWM. So for a foreign
//! allocation the refusal becomes a SKIP: the Present is answered with success, the destination
//! is left as it was (Blt) or the previous picture stays (Flip), and the skip is counted
//! (`PrFgSkip`, last reason `PrFgWhy`, per arm `PrFgBlt` / `PrFgFlip`).
//!
//! WHAT IS FOREIGN. Only the KMD's own records say so, never the creator's words:
//!
//! * the open identity's FOREIGN flag (`HeliosWddmOpenIdentity`, set from the foreign-table hit
//!   at open time), meaningful for the two kinds that define it, DEVICE_MEMORY (the adopted
//!   NVK image) and STANDARD (the KMD's own RM system-memory primary);
//! * a foreign-table record for the resource id (`foreign_record(resource_id)`), checked
//!   lazily at a refusal, so the happy path takes no lock.
//!
//! The layout trailer a creator can write is NOT a fact here: an ordinary allocation can forge
//! it, and a forged one must keep failing as it always did.
//!
//! WHAT IS NEVER SKIPPED: an unresolvable SOURCE handle (nothing to say it is foreign), a null
//! `DXGKARG_PRESENT`, a missing adapter, `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (dxgkrnl's
//! retry protocol) and every failure after a host copy was submitted (the destination's
//! ownership protocol cannot be unwound). Those keep their status.

/// `HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY` (`protocol/src/wddm.rs`); pinned by the KMD build.
pub const KIND_DEVICE_MEMORY: u32 = 1;
/// `HELIOS_WDDM_ALLOC_KIND_STANDARD`.
pub const KIND_STANDARD: u32 = 2;

/// What the KMD knows about one Present allocation-list entry, without a lock except
/// `table_record`, which the caller fills in only after a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocFacts {
    /// `PresentAllocInfo::kind`.
    pub kind: u32,
    /// The open identity said FOREIGN (the KMD's record from the open).
    pub identity_foreign: bool,
    /// `foreign_record(resource_id)` returned a record.
    pub table_record: bool,
}

impl AllocFacts {
    /// Whether this allocation is a foreign resource. The identity flag counts only for the
    /// kinds that define it; a table record counts for any kind.
    pub const fn is_foreign(&self) -> bool {
        self.table_record
            || (self.identity_foreign
                && (self.kind == KIND_DEVICE_MEMORY || self.kind == KIND_STANDARD))
    }
}

/// Which Present contract is running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arm {
    /// `DXGK_PRESENTFLAGS.Blt`.
    Blt,
    /// Flip with no DMA buffer (`FlipOnVSyncMmIo`): completed through SetVidPnSourceAddress.
    FlipMmio,
    /// Flip with a DMA buffer: the KMD programs the display when the buffer executes.
    FlipDma,
}

impl Arm {
    pub const fn is_flip(self) -> bool {
        matches!(self, Arm::FlipMmio | Arm::FlipDma)
    }

    /// 1 = Blt, 2 = MMIO flip, 3 = DMA flip.
    pub const fn code(self) -> u32 {
        match self {
            Arm::Blt => 1,
            Arm::FlipMmio => 2,
            Arm::FlipDma => 3,
        }
    }
}

/// A refusal the Present would return as a failure. The numbers are the reason codes
/// (`PrFgWhy`), stable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// Blt: the source is foreign and the destination handle resolves to nothing (`PBCpy` 0xE1).
    BltNoDestination = 1,
    /// Blt: a source or destination DXGI format is unresolved (`PBCpy` 0xE2).
    BltFormat = 2,
    /// Blt: the source kind is not DEVICE_MEMORY (`PBCpy` 0xE6).
    BltSourceKind = 3,
    /// Blt: a WindowedBlt snapshot does not match the source extent (`PBCpy` 0xE7).
    BltSnapshot = 4,
    /// Blt: no source or destination import descriptor can be built (`PBCpy` 0xE2).
    BltDescriptor = 5,
    /// Blt: source and destination extents differ (`PBCpy` 0xE3).
    BltExtent = 6,
    /// Blt: a snapshot Blt without a stream boundary (`PBCpy` 0xE8).
    BltBoundary = 7,
    /// Blt: the two-phase snapshot Blt could not be queued (`PBCpy` 0xE4 / 0xE5).
    BltQueue = 8,
    /// Blt: the destination Present buffer could not be taken for the write (`PBOwn` 0xE1).
    BltBegin = 9,
    /// Blt: the host copy was refused or could not be submitted, before anything was written
    /// (`PBCpy` 0xE4 / 0xE5). Includes a foreign import the host refused.
    BltSubmit = 10,
    /// Flip: the source's DXGI format is unresolved (`PBFlip` 0xE2). Only a check.
    FlipFormat = 11,
    /// DMA flip: the source's resource is not in the direct-scan-out table (`PBFlip` 0xE6).
    FlipNotScanout = 12,
    /// The completion tail could not merge the stream boundary into the DMA private data.
    TailBoundary = 13,
    /// DMA flip, `ForeignFlip` on: a foreign allocation that is nevertheless not in the
    /// direct-scan-out table (the table was full, or it was created before the knob was read),
    /// so there is no global handle to hand to the programming path.
    FlipUnregistered = 14,
    /// Blt: the adapter, the source or the destination handle resolves to nothing (see
    /// [`decide_unresolved`]). Unconditional: it is not about a foreign allocation.
    Unresolved = 15,
    /// Blt with `ColorFill` and no source allocation: a fill has no source. The driver writes no
    /// content for it (a no-op, as `docs/kmd-rm-client.md` 15.16 has always said).
    ColorFill = 16,
}

impl Refusal {
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Which allocation roles can make this refusal a foreign one.
    const fn roles(self) -> Roles {
        match self {
            Refusal::BltNoDestination
            | Refusal::BltSourceKind
            | Refusal::BltSnapshot
            | Refusal::FlipFormat
            | Refusal::FlipNotScanout => Roles::Source,
            Refusal::BltBegin => Roles::Destination,
            Refusal::FlipUnregistered => Roles::Source,
            Refusal::Unresolved | Refusal::ColorFill => Roles::Either,
            Refusal::BltFormat
            | Refusal::BltDescriptor
            | Refusal::BltExtent
            | Refusal::BltBoundary
            | Refusal::BltQueue
            | Refusal::BltSubmit
            | Refusal::TailBoundary => Roles::Either,
        }
    }

    /// Whether the refusal can occur in `arm` at all. A refusal asked about in an arm that
    /// cannot produce it is never skipped: the caller is wrong and the failure stays visible.
    const fn in_arm(self, arm: Arm) -> bool {
        match self {
            Refusal::FlipFormat => arm.is_flip(),
            Refusal::FlipNotScanout | Refusal::FlipUnregistered => matches!(arm, Arm::FlipDma),
            Refusal::TailBoundary => true,
            // Decided by `decide_unresolved`, which has no foreign facts to read.
            Refusal::Unresolved | Refusal::ColorFill => false,
            _ => matches!(arm, Arm::Blt),
        }
    }

    /// What the Present does instead of failing.
    const fn effect(self) -> Effect {
        match self {
            Refusal::FlipFormat => Effect::IgnoreCheck,
            Refusal::FlipNotScanout | Refusal::FlipUnregistered => Effect::KeepPicture,
            Refusal::TailBoundary => Effect::DropBoundary,
            _ => Effect::LeaveDestination,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Roles {
    Source,
    Destination,
    Either,
}

/// What a skipped Present does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Blt: no copy; the destination keeps its bytes; the Present completes with a marker that
    /// names no pending work.
    LeaveDestination,
    /// Flip: the validation was only a check, the flip proceeds without it.
    IgnoreCheck,
    /// DMA flip: nothing is armed; the display keeps showing the previous picture.
    KeepPicture,
    /// The stream boundary is not carried; the DMA buffer retires by the legacy rule.
    DropBoundary,
}

/// Why a Present was skipped: the value of `PrFgWhy`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Why {
    pub arm: Arm,
    pub refusal: Refusal,
    /// The source was foreign.
    pub source: bool,
    /// The destination was foreign (Blt only).
    pub destination: bool,
    /// [`Refusal::Unresolved`] only: the context's adapter did not resolve. For that refusal
    /// `source` / `destination` mean UNRESOLVED, not foreign.
    pub adapter: bool,
}

impl Why {
    /// `arm << 12 | adapter << 10 | destination << 9 | source << 8 | refusal`.
    pub const fn code(&self) -> u32 {
        (self.arm.code() << 12)
            | ((self.adapter as u32) << 10)
            | ((self.destination as u32) << 9)
            | ((self.source as u32) << 8)
            | self.refusal.code()
    }
}

/// The decision at one refusal site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Keep the failure as it always was.
    Proceed,
    /// Answer with success and do `effect`.
    Skip { why: Why, effect: Effect },
}

/// Decide what a refusal does, given the arm, the refusal and the facts of the source and
/// destination entries (`None` when the handle resolved to no allocation).
///
/// Only a foreign allocation in a role the refusal concerns turns it into a skip. A flip has
/// no destination entry, so `destination` is ignored for the flip arms.
pub const fn decide(
    arm: Arm,
    refusal: Refusal,
    source: Option<AllocFacts>,
    destination: Option<AllocFacts>,
) -> Verdict {
    if !refusal.in_arm(arm) {
        return Verdict::Proceed;
    }
    let src = match source {
        Some(facts) => facts.is_foreign(),
        None => false,
    };
    let dst = match destination {
        Some(facts) => !arm.is_flip() && facts.is_foreign(),
        None => false,
    };
    let involved = match refusal.roles() {
        Roles::Source => src,
        Roles::Destination => dst,
        Roles::Either => src || dst,
    };
    // A Blt whose destination does not resolve is only skipped on a foreign source; with a
    // resolved destination the refusal is not this one.
    if matches!(refusal, Refusal::BltNoDestination) && destination.is_some() {
        return Verdict::Proceed;
    }
    if !involved {
        return Verdict::Proceed;
    }
    Verdict::Skip {
        why: Why {
            arm,
            refusal,
            source: src,
            destination: dst,
            adapter: false,
        },
        effect: refusal.effect(),
    }
}

/// Why a handle resolved to no allocation (`PrUnrWhy`), from `present_alloc_info`'s steps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleCause {
    /// It resolved.
    Resolved = 0,
    /// The list slot holds a NULL `hDeviceSpecificAllocation`: the entry is not part of this
    /// operation (a `ColorFill` has no source; a Blt may name no destination).
    Null = 1,
    /// Not aligned for, or without the magic of, an `OpenAllocationContext` (counted `OaBadH`):
    /// a handle this driver did not mint, or one that was closed.
    NotOurs = 2,
    /// The open belongs to an older transport generation (counted `PgStale` style,
    /// `STALE_ALLOC_REFUSED`): a device restart left it behind.
    StaleGeneration = 3,
    /// An open context of ours that recorded no identity (the private data had none of the two
    /// layouts), so `present` is `None`.
    NoIdentity = 4,
}

/// `PrUnrWhy`: `source << 0 | destination << 4 | adapter_unresolved << 8`.
pub const fn pack_causes(
    adapter_unresolved: bool,
    source: HandleCause,
    destination: HandleCause,
) -> u32 {
    (source as u32) | ((destination as u32) << 4) | ((adapter_unresolved as u32) << 8)
}

/// What a Blt does when it cannot resolve what it is to copy (`PBCpy` 0xE1, `PBRetSite` 3, 4, 5).
///
/// It succeeds without copying, on EVERY transport (Venus-only included): the Present arrives from
/// dxgkrnl with entries this driver cannot name, and failing it is a device error for DWM, while the
/// cost of skipping is one frame's picture. Observed (T2): a Venus-only DWM restart hit this site on
/// its first presents. `Refusal::Unresolved` names which of adapter / source / destination did not
/// resolve; a `ColorFill` Blt (no source by definition) with a resolved destination is
/// `Refusal::ColorFill`, a no-op. A Blt whose three resolve is not this refusal.
pub const fn decide_unresolved(
    arm: Arm,
    adapter_resolved: bool,
    source_resolved: bool,
    destination_resolved: bool,
    color_fill: bool,
) -> Verdict {
    if !matches!(arm, Arm::Blt) || (adapter_resolved && source_resolved && destination_resolved) {
        return Verdict::Proceed;
    }
    let fill_only = color_fill && adapter_resolved && destination_resolved;
    Verdict::Skip {
        why: Why {
            arm,
            refusal: if fill_only {
                Refusal::ColorFill
            } else {
                Refusal::Unresolved
            },
            source: !fill_only && !source_resolved,
            destination: !destination_resolved,
            adapter: !adapter_resolved,
        },
        effect: Effect::LeaveDestination,
    }
}

/// What a DMA flip does with its source, from the direct-scan-out table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlipRoute {
    /// Arm the deferred programming (`PresentFlipPrivate` + `arm_dma_flip`), as every
    /// direct-scan-out flip does. `foreign_flip` is true when the programming it reaches is
    /// `ForeignFlip`'s (a foreign allocation, knob on), false for the existing Venus flips.
    Arm { foreign_flip: bool },
    /// Not a foreign allocation's refusal: the flip fails as it always did.
    Fail,
    /// A foreign allocation the programming path cannot take: counted success, nothing armed.
    Skip { why: Why, effect: Effect },
}

/// Route a DMA flip.
///
/// * `in_table`: the source's resource id has a global handle in the direct-scan-out table.
///   Foreign allocations are registered there at creation iff `ForeignFlip` is on, so a
///   foreign flip can reach `program_vidpn_source_inner` (whose `ForeignFlip` hook programs it,
///   or refuses it and the Venus path runs as for any other primary).
/// * `direct_scanout`: the allocation carries `MISC_DIRECT_SCANOUT` (the table's original
///   population: unchanged behaviour whatever the knob).
/// * `source`: facts of the source entry; only consulted when not (`in_table` and
///   `direct_scanout`), so the common flip needs none.
pub const fn flip_route(
    knob_on: bool,
    in_table: bool,
    direct_scanout: bool,
    source: Option<AllocFacts>,
) -> FlipRoute {
    if in_table && direct_scanout {
        return FlipRoute::Arm {
            foreign_flip: false,
        };
    }
    let foreign = match source {
        Some(facts) => facts.is_foreign(),
        None => false,
    };
    if !foreign {
        return if in_table {
            FlipRoute::Arm {
                foreign_flip: false,
            }
        } else {
            FlipRoute::Fail
        };
    }
    if in_table && knob_on {
        return FlipRoute::Arm { foreign_flip: true };
    }
    // Foreign, and either not registered or registered while the knob is off.
    let refusal = if knob_on {
        Refusal::FlipUnregistered
    } else {
        Refusal::FlipNotScanout
    };
    match decide(Arm::FlipDma, refusal, source, None) {
        Verdict::Skip { why, effect } => FlipRoute::Skip { why, effect },
        Verdict::Proceed => FlipRoute::Fail,
    }
}

/// Early-return site ids for `PBRetSite`: which line of the Present path returned a
/// non-success status, so the registry dump names it. 0 = none (success, or a status no site
/// names). Stable: the dump is read against this table.
pub mod site {
    /// `present` is null.
    pub const NULL_ARGS: u32 = 1;
    /// Level 5 Blt arm without an adapter.
    pub const RM_BLT_NO_ADAPTER: u32 = 2;
    /// Blt arm: no adapter behind the context.
    pub const BLT_NO_ADAPTER: u32 = 3;
    /// Blt arm: the source handle resolved to no allocation (an invalid source handle).
    pub const BLT_NO_SOURCE: u32 = 4;
    /// Blt arm: the destination handle resolved to no allocation.
    pub const BLT_NO_DESTINATION: u32 = 5;
    /// Blt arm: unresolved DXGI format.
    pub const BLT_FORMAT: u32 = 6;
    /// Blt arm: source kind is not DEVICE_MEMORY.
    pub const BLT_SOURCE_KIND: u32 = 7;
    /// Blt arm: snapshot does not match the source.
    pub const BLT_SNAPSHOT: u32 = 8;
    /// Blt arm: no import descriptor.
    pub const BLT_DESCRIPTOR: u32 = 9;
    /// Blt arm: extents differ.
    pub const BLT_EXTENT: u32 = 10;
    /// Blt arm: snapshot Blt without a stream boundary.
    pub const BLT_BOUNDARY: u32 = 11;
    /// Blt arm: the WindowedBlt token could not be merged.
    pub const BLT_TOKEN_MERGE: u32 = 12;
    /// Flip arm: no adapter behind the context.
    pub const FLIP_NO_ADAPTER: u32 = 13;
    /// Flip arm: the source handle resolved to no allocation.
    pub const FLIP_NO_SOURCE: u32 = 14;
    /// Flip arm: unresolved DXGI format.
    pub const FLIP_FORMAT: u32 = 15;
    /// DMA flip: no allocation-list source.
    pub const FLIP_NO_LIST_SOURCE: u32 = 16;
    /// DMA flip: the resource is not in the direct-scan-out table.
    pub const FLIP_NOT_SCANOUT: u32 = 17;
    /// Level 5 Blt arm: no allocation behind the source handle.
    pub const RM_BLT_NO_SOURCE: u32 = 18;
    /// Completion tail: the stream boundary could not be merged.
    pub const TAIL_BOUNDARY: u32 = 19;
    /// Completion tail: the patch-location capacity or write failed.
    pub const TAIL_PATCH: u32 = 20;
    /// `FlipWithMultiPlaneOverlay`: refused with `STATUS_NOT_SUPPORTED`.
    pub const MPO: u32 = 21;
    /// Blt arm (legacy or level 5): the DMA buffer or its private data is too small
    /// (`STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER`, dxgkrnl retries).
    pub const BLT_DMA_SMALL: u32 = 22;
    /// Blt arm (legacy or level 5): the patch-location capacity check failed.
    pub const BLT_PATCH: u32 = 23;
    /// Blt arm: the two-phase snapshot Blt could not be queued.
    pub const BLT_QUEUE: u32 = 24;
    /// Blt arm: the destination Present buffer could not be taken for the write.
    pub const BLT_BEGIN: u32 = 25;
    /// Blt arm: the host copy was refused or could not be submitted.
    pub const BLT_SUBMIT: u32 = 26;
    /// Blt arm, after the copy was submitted: the fence wait, the CPU mirror or the ownership
    /// release failed.
    pub const BLT_WAIT: u32 = 27;
    /// Blt arm: the fence marker could not be merged into the DMA private data.
    pub const BLT_FENCE_MERGE: u32 = 28;
    /// Completion tail: the DMA buffer is smaller than the refresh marker.
    pub const TAIL_DMA_SMALL: u32 = 29;
    /// DMA flip: the flip record could not be written into the DMA private data.
    pub const FLIP_PRIVATE: u32 = 30;
}

#[cfg(test)]
mod tests {
    use super::*;

    const VENUS: AllocFacts = AllocFacts {
        kind: KIND_DEVICE_MEMORY,
        identity_foreign: false,
        table_record: false,
    };
    const FOREIGN_ID: AllocFacts = AllocFacts {
        kind: KIND_DEVICE_MEMORY,
        identity_foreign: true,
        table_record: false,
    };
    const FOREIGN_TABLE: AllocFacts = AllocFacts {
        kind: KIND_DEVICE_MEMORY,
        identity_foreign: false,
        table_record: true,
    };
    const SYSMEM_PRIMARY: AllocFacts = AllocFacts {
        kind: KIND_STANDARD,
        identity_foreign: true,
        table_record: false,
    };
    const ALL: [Refusal; 16] = [
        Refusal::BltNoDestination,
        Refusal::BltFormat,
        Refusal::BltSourceKind,
        Refusal::BltSnapshot,
        Refusal::BltDescriptor,
        Refusal::BltExtent,
        Refusal::BltBoundary,
        Refusal::BltQueue,
        Refusal::BltBegin,
        Refusal::BltSubmit,
        Refusal::FlipFormat,
        Refusal::FlipNotScanout,
        Refusal::TailBoundary,
        Refusal::FlipUnregistered,
        Refusal::Unresolved,
        Refusal::ColorFill,
    ];
    const ARMS: [Arm; 3] = [Arm::Blt, Arm::FlipMmio, Arm::FlipDma];

    fn skipped(v: Verdict) -> bool {
        matches!(v, Verdict::Skip { .. })
    }

    #[test]
    fn foreignness_comes_from_the_kmd_records_only() {
        assert!(!VENUS.is_foreign());
        assert!(FOREIGN_ID.is_foreign());
        assert!(FOREIGN_TABLE.is_foreign());
        assert!(SYSMEM_PRIMARY.is_foreign());
        // The identity flag is defined for kinds 1 and 2 only.
        let odd = AllocFacts {
            kind: 7,
            identity_foreign: true,
            table_record: false,
        };
        assert!(!odd.is_foreign());
        let odd_table = AllocFacts {
            kind: 7,
            identity_foreign: false,
            table_record: true,
        };
        assert!(odd_table.is_foreign());
    }

    #[test]
    fn venus_allocations_never_skip() {
        for arm in ARMS {
            for r in ALL {
                assert_eq!(
                    decide(arm, r, Some(VENUS), Some(VENUS)),
                    Verdict::Proceed,
                    "{arm:?} {r:?}"
                );
                assert_eq!(decide(arm, r, None, None), Verdict::Proceed);
                assert_eq!(decide(arm, r, Some(VENUS), None), Verdict::Proceed);
            }
        }
    }

    #[test]
    fn a_foreign_blt_source_skips_every_blt_refusal_it_causes() {
        for r in [
            Refusal::BltFormat,
            Refusal::BltSourceKind,
            Refusal::BltSnapshot,
            Refusal::BltDescriptor,
            Refusal::BltExtent,
            Refusal::BltBoundary,
            Refusal::BltQueue,
            Refusal::BltSubmit,
            Refusal::TailBoundary,
        ] {
            for f in [FOREIGN_ID, FOREIGN_TABLE, SYSMEM_PRIMARY] {
                match decide(Arm::Blt, r, Some(f), Some(VENUS)) {
                    Verdict::Skip { why, effect } => {
                        assert!(why.source && !why.destination);
                        assert_eq!(why.arm, Arm::Blt);
                        assert_eq!(why.refusal, r);
                        assert_eq!(
                            effect,
                            if r == Refusal::TailBoundary {
                                Effect::DropBoundary
                            } else {
                                Effect::LeaveDestination
                            }
                        );
                    }
                    Verdict::Proceed => panic!("{r:?} {f:?} not skipped"),
                }
            }
        }
    }

    #[test]
    fn a_foreign_blt_destination_skips_the_refusals_it_can_cause() {
        for r in [
            Refusal::BltFormat,
            Refusal::BltDescriptor,
            Refusal::BltExtent,
            Refusal::BltBoundary,
            Refusal::BltQueue,
            Refusal::BltBegin,
            Refusal::BltSubmit,
            Refusal::TailBoundary,
        ] {
            match decide(Arm::Blt, r, Some(VENUS), Some(SYSMEM_PRIMARY)) {
                Verdict::Skip { why, .. } => assert!(!why.source && why.destination),
                Verdict::Proceed => panic!("{r:?} not skipped"),
            }
        }
        // Source-only refusals are not the destination's doing.
        for r in [Refusal::BltSourceKind, Refusal::BltSnapshot] {
            assert_eq!(
                decide(Arm::Blt, r, Some(VENUS), Some(SYSMEM_PRIMARY)),
                Verdict::Proceed
            );
        }
        // The destination's buffer protocol is not a foreign source's doing.
        assert_eq!(
            decide(Arm::Blt, Refusal::BltBegin, Some(FOREIGN_ID), Some(VENUS)),
            Verdict::Proceed
        );
    }

    #[test]
    fn both_foreign_sets_both_bits_in_the_code() {
        let v = decide(
            Arm::Blt,
            Refusal::BltExtent,
            Some(FOREIGN_ID),
            Some(FOREIGN_TABLE),
        );
        let Verdict::Skip { why, .. } = v else {
            panic!("not skipped")
        };
        assert!(why.source && why.destination);
        assert_eq!(why.code(), (1 << 12) | (1 << 9) | (1 << 8) | 6);
    }

    #[test]
    fn blt_without_a_destination_skips_only_for_a_foreign_source() {
        assert!(skipped(decide(
            Arm::Blt,
            Refusal::BltNoDestination,
            Some(FOREIGN_ID),
            None
        )));
        assert!(!skipped(decide(
            Arm::Blt,
            Refusal::BltNoDestination,
            Some(VENUS),
            None
        )));
        // An invalid SOURCE handle is never skipped.
        assert!(!skipped(decide(
            Arm::Blt,
            Refusal::BltNoDestination,
            None,
            Some(FOREIGN_ID)
        )));
        // A resolved destination means this is not the refusal.
        assert!(!skipped(decide(
            Arm::Blt,
            Refusal::BltNoDestination,
            Some(FOREIGN_ID),
            Some(VENUS)
        )));
    }

    #[test]
    fn flips_ignore_the_destination_and_use_their_own_effects() {
        for arm in [Arm::FlipMmio, Arm::FlipDma] {
            // A foreign destination entry is not a flip's concern.
            assert!(!skipped(decide(
                arm,
                Refusal::FlipFormat,
                Some(VENUS),
                Some(FOREIGN_ID)
            )));
            let Verdict::Skip { effect, why } =
                decide(arm, Refusal::FlipFormat, Some(FOREIGN_ID), None)
            else {
                panic!("flip format not skipped")
            };
            assert_eq!(effect, Effect::IgnoreCheck);
            assert_eq!(why.arm, arm);
        }
        // Only the DMA contract looks the resource up in the scan-out table.
        assert!(!skipped(decide(
            Arm::FlipMmio,
            Refusal::FlipNotScanout,
            Some(FOREIGN_ID),
            None
        )));
        let Verdict::Skip { effect, why } = decide(
            Arm::FlipDma,
            Refusal::FlipNotScanout,
            Some(FOREIGN_TABLE),
            None,
        ) else {
            panic!("not skipped")
        };
        assert_eq!(effect, Effect::KeepPicture);
        assert_eq!(why.code(), (3 << 12) | (1 << 8) | 12);
    }

    #[test]
    fn a_refusal_in_an_arm_that_cannot_produce_it_is_never_skipped() {
        for r in ALL {
            for arm in ARMS {
                if r.in_arm(arm) {
                    continue;
                }
                assert_eq!(
                    decide(arm, r, Some(FOREIGN_ID), Some(FOREIGN_ID)),
                    Verdict::Proceed,
                    "{arm:?} {r:?}"
                );
            }
        }
        // Blt refusals are Blt only; the tail is shared.
        assert!(!Refusal::BltExtent.in_arm(Arm::FlipDma));
        assert!(Refusal::TailBoundary.in_arm(Arm::FlipMmio));
    }

    #[test]
    fn reason_codes_are_unique_and_stable() {
        let mut seen = [false; 32];
        for r in ALL {
            let c = r.code() as usize;
            assert!(c >= 1 && c < 32);
            assert!(!seen[c], "duplicate {c}");
            seen[c] = true;
        }
        assert_eq!(Refusal::BltNoDestination.code(), 1);
        assert_eq!(Refusal::BltSubmit.code(), 10);
        assert_eq!(Refusal::TailBoundary.code(), 13);
        assert_eq!(Arm::Blt.code(), 1);
        assert_eq!(Arm::FlipMmio.code(), 2);
        assert_eq!(Arm::FlipDma.code(), 3);
    }

    #[test]
    fn site_ids_are_unique() {
        let ids = [
            site::NULL_ARGS,
            site::RM_BLT_NO_ADAPTER,
            site::BLT_NO_ADAPTER,
            site::BLT_NO_SOURCE,
            site::BLT_NO_DESTINATION,
            site::BLT_FORMAT,
            site::BLT_SOURCE_KIND,
            site::BLT_SNAPSHOT,
            site::BLT_DESCRIPTOR,
            site::BLT_EXTENT,
            site::BLT_BOUNDARY,
            site::BLT_TOKEN_MERGE,
            site::FLIP_NO_ADAPTER,
            site::FLIP_NO_SOURCE,
            site::FLIP_FORMAT,
            site::FLIP_NO_LIST_SOURCE,
            site::FLIP_NOT_SCANOUT,
            site::RM_BLT_NO_SOURCE,
            site::TAIL_BOUNDARY,
            site::TAIL_PATCH,
            site::MPO,
            site::BLT_DMA_SMALL,
            site::BLT_PATCH,
            site::BLT_QUEUE,
            site::BLT_BEGIN,
            site::BLT_SUBMIT,
            site::BLT_WAIT,
            site::BLT_FENCE_MERGE,
            site::TAIL_DMA_SMALL,
            site::FLIP_PRIVATE,
        ];
        for (i, a) in ids.iter().enumerate() {
            assert!(*a != 0);
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// The matrix the coordinator asked for: ForeignFlip knob x in the table x foreign record.
    #[test]
    fn dma_flip_route_matrix() {
        let arm_plain = FlipRoute::Arm {
            foreign_flip: false,
        };
        let arm_ff = FlipRoute::Arm { foreign_flip: true };
        for knob in [false, true] {
            // A direct-scan-out allocation in the table: unchanged, whatever it is.
            for facts in [None, Some(VENUS), Some(FOREIGN_ID), Some(FOREIGN_TABLE)] {
                assert_eq!(flip_route(knob, true, true, facts), arm_plain, "{knob}");
            }
            // Not foreign and not in the table: the failure it always was.
            for facts in [None, Some(VENUS)] {
                assert_eq!(flip_route(knob, false, false, facts), FlipRoute::Fail);
                assert_eq!(flip_route(knob, false, true, facts), FlipRoute::Fail);
            }
            // Not foreign, in the table without the direct flag (not a registration this
            // code makes): armed as before.
            assert_eq!(flip_route(knob, true, false, Some(VENUS)), arm_plain);
        }
        // Foreign, in the table, knob on: handed to ForeignFlip's programming.
        for f in [FOREIGN_ID, FOREIGN_TABLE] {
            assert_eq!(flip_route(true, true, false, Some(f)), arm_ff);
        }
        // Foreign, in the table, knob off (a stale registration): skipped, 12.
        let FlipRoute::Skip { why, effect } = flip_route(false, true, false, Some(FOREIGN_ID))
        else {
            panic!("not skipped")
        };
        assert_eq!(why.refusal, Refusal::FlipNotScanout);
        assert_eq!(why.arm, Arm::FlipDma);
        assert_eq!(effect, Effect::KeepPicture);
        // Foreign, not in the table: knob off -> 12, knob on -> 14.
        for (knob, refusal) in [
            (false, Refusal::FlipNotScanout),
            (true, Refusal::FlipUnregistered),
        ] {
            for f in [FOREIGN_ID, FOREIGN_TABLE, SYSMEM_PRIMARY] {
                let FlipRoute::Skip { why, effect } = flip_route(knob, false, false, Some(f))
                else {
                    panic!("not skipped")
                };
                assert_eq!(why.refusal, refusal);
                assert!(why.source && !why.destination);
                assert_eq!(effect, Effect::KeepPicture);
            }
        }
        // A foreign allocation carrying the direct flag but missing from the table is
        // foreign and unregistered: skipped, never failed.
        assert!(matches!(
            flip_route(false, false, true, Some(FOREIGN_ID)),
            FlipRoute::Skip { .. }
        ));
    }

    #[test]
    fn unresolved_handles_skip_on_every_transport() {
        // All three resolved: not this refusal.
        for fill in [false, true] {
            assert_eq!(
                decide_unresolved(Arm::Blt, true, true, true, fill),
                Verdict::Proceed
            );
        }
        // Any unresolved side skips, with no foreign record or knob involved (T2: a Venus-only
        // DWM restart).
        for (a, s, d) in [
            (true, false, true),
            (true, true, false),
            (true, false, false),
            (false, true, true),
            (false, false, false),
        ] {
            let Verdict::Skip { why, effect } = decide_unresolved(Arm::Blt, a, s, d, false) else {
                panic!("not skipped {a} {s} {d}")
            };
            assert_eq!(why.refusal, Refusal::Unresolved);
            assert_eq!((why.adapter, why.source, why.destination), (!a, !s, !d));
            assert_eq!(effect, Effect::LeaveDestination);
        }
        let Verdict::Skip { why, .. } = decide_unresolved(Arm::Blt, true, false, true, false)
        else {
            panic!()
        };
        assert_eq!(why.code(), (1 << 12) | (1 << 8) | 15);
        let Verdict::Skip { why, .. } = decide_unresolved(Arm::Blt, false, true, true, false)
        else {
            panic!()
        };
        assert_eq!(why.code(), (1 << 12) | (1 << 10) | 15);
        // A fill has no source: a no-op of its own reason, naming nothing unresolved.
        let Verdict::Skip { why, .. } = decide_unresolved(Arm::Blt, true, false, true, true) else {
            panic!()
        };
        assert_eq!(why.refusal, Refusal::ColorFill);
        assert_eq!(why.code(), (1 << 12) | 16);
        // A fill whose destination is also unresolved is an unresolved handle.
        let Verdict::Skip { why, .. } = decide_unresolved(Arm::Blt, true, false, false, true)
        else {
            panic!()
        };
        assert_eq!(why.refusal, Refusal::Unresolved);
        // Flips are not covered.
        for arm in [Arm::FlipMmio, Arm::FlipDma] {
            assert_eq!(
                decide_unresolved(arm, false, false, false, false),
                Verdict::Proceed
            );
        }
    }

    #[test]
    fn causes_pack_into_one_word() {
        use HandleCause::*;
        assert_eq!(pack_causes(false, Resolved, Resolved), 0);
        assert_eq!(pack_causes(false, Null, Resolved), 1);
        assert_eq!(pack_causes(false, Resolved, StaleGeneration), 0x30);
        assert_eq!(pack_causes(true, NoIdentity, NotOurs), 0x124);
    }

    #[test]
    fn the_tester_shape_is_skipped() {
        // 5120x1440 foreign DEVICE_MEMORY source blitted into a 1600x900 STANDARD staging
        // destination: the extent refusal (PBCpy 0xE3) must be a skip, not INVALID_PARAMETER.
        let staging = AllocFacts {
            kind: KIND_STANDARD,
            identity_foreign: false,
            table_record: false,
        };
        let v = decide(
            Arm::Blt,
            Refusal::BltExtent,
            Some(FOREIGN_ID),
            Some(staging),
        );
        assert!(skipped(v));
        // The same shape from a Venus source keeps failing.
        assert!(!skipped(decide(
            Arm::Blt,
            Refusal::BltExtent,
            Some(VENUS),
            Some(staging)
        )));
    }
}

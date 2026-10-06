//! Independent flip (direct flip) of a flip-model swap chain: the pure decision "may this flip
//! source go direct" (`docs/independent-flip.md`, section 6 and the table in 6.2).
//!
//! ⚠ NOT WIRED. Nothing in `kmd_render` calls this module, and the driver's behaviour is
//! unchanged by it. It exists so the decision can be argued, tested and reviewed on the host
//! before any DDI code is written, the way `foreign_flip` and `flip_completion` were.
//!
//! What it decides. When dxgkrnl flips an application's own swap-chain buffer (an independent
//! flip: DWM stops composing that window and the application's buffer is the primary), the
//! KMD sees an ordinary Flip-arm `DxgkDdiPresent` plus `SetVidPnSourceAddress`, exactly as it
//! does for DWM's chain. The KMD already has three ways to answer one: `ForeignFlip` (an
//! adopted NVK-on-RM allocation is shown by a flip of its importer's GEM), the Venus direct
//! bind (`MISC_DIRECT_SCANOUT`: `SET_SCANOUT_BLOB` of the allocation's own blob) and the Venus
//! copy (a GPU copy into the adapter's LINEAR image). This table sits in FRONT of them and
//! answers one question per flip: is it a zero-copy direct source ([`Verdict::Direct`]), a
//! source the existing copy can show ([`Verdict::Copy`]), or a flip that must be COMPLETED as
//! a kept picture because nothing can show it ([`Verdict::Keep`], with the reason)?
//!
//! It never answers "fail the Present": a flip dxgkrnl chose to issue is completed toward
//! dxgkrnl in every case (`docs/zero-copy-present.md` section 13, the flip-completion
//! invariant). A refusal only decides what the screen shows meanwhile.
//!
//! The rows are the doc's. The layout guard is NOT restated: a direct Venus source is checked
//! by `snapshot_bind::validate_layout`, the undersize guard that keeps the host from reading
//! past the blob. A foreign source is judged by `foreign_flip::decide` (its verdict is an input
//! here, [`ForeignOutcome`]), so the two cannot drift apart; [`why_of_foreign`] is the one
//! mapping from its reasons to this table's.
//!
//! Everything is a function of its arguments: no wdk, no clock, no atomics.

use crate::flip_completion::Source;
use crate::foreign_flip::Why as ForeignWhy;
use crate::present_foreign::Arm;
use crate::snapshot_bind::{validate_layout, SnapshotDescriptor, SnapshotReject};
use crate::ScanoutFormat;

/// Service-key knob (REG_DWORD, default 0): the table is consulted. 0 is today's behaviour:
/// every flip is answered by the existing arms alone.
pub const KNOB_ENABLE: &str = "IndepFlip";
/// Service-key knob (default 0): a source the UMD did not create as a primary
/// (`MISC_PRIMARY` clear) is refused. 0 judges the allocation by what it is, not by what the
/// UMD called it; the census (`IdfUntagged`) says whether the 1 is ever needed.
pub const KNOB_NEED_PRIMARY: &str = "IdfNeedPrim";
/// Service-key knob (default 0): hold the displayed-address publication of a flip until the
/// host released the buffer it replaces (`scanout_release`). Design item S-2b; this module
/// does not use it.
pub const KNOB_HOLD_RELEASE: &str = "IdfHoldRel";

/// The knobs, for the collision tests.
pub const KNOBS: [&str; 3] = [KNOB_ENABLE, KNOB_NEED_PRIMARY, KNOB_HOLD_RELEASE];

/// Which kind of allocation the flip names (`flip_completion::classify` plus the direct flag).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// An adopted foreign (NVK-on-RM) allocation: judged by `foreign_flip::decide`.
    Foreign,
    /// A Venus allocation with `MISC_DIRECT_SCANOUT`: the host binds its own blob.
    VenusDirect,
    /// Any other Venus allocation: only the GPU copy can show it.
    VenusOther,
    /// No resource id, or nothing the Venus path can ever show (the host-less placeholder).
    Hollow,
}

impl Class {
    /// From the completion invariant's classification and the allocation's direct flag.
    pub const fn of(source: Source, direct_scanout: bool) -> Class {
        match source {
            Source::Foreign => Class::Foreign,
            Source::Hollow => Class::Hollow,
            Source::Venus => {
                if direct_scanout {
                    Class::VenusDirect
                } else {
                    Class::VenusOther
                }
            }
        }
    }
}

/// What the flip's pixels are, as far as scan-out is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatClass {
    /// One of the three 32-bit formats `SET_SCANOUT_BLOB` carries.
    Rgb32(ScanoutFormat),
    /// 10-bit and 16-bit-float RGB: a real display format, not carried yet (stage S-4).
    Wide,
    /// Everything else (YUV, sRGB-typed aliases, formats with no scan-out meaning).
    Other,
}

impl FormatClass {
    /// DXGI values: 10 `R16G16B16A16_FLOAT`, 11 `R16G16B16A16_UNORM`, 24 `R10G10B10A2_UNORM`,
    /// 89 `R10G10B10_XR_BIAS_A2_UNORM` are the wide ones; 28, 87 and 88 are the carried three.
    pub const fn from_dxgi(dxgi: u32) -> FormatClass {
        match ScanoutFormat::from_dxgi(dxgi) {
            Some(f) => FormatClass::Rgb32(f),
            None => match dxgi {
                10 | 11 | 24 | 89 => FormatClass::Wide,
                _ => FormatClass::Other,
            },
        }
    }
}

/// `foreign_flip::decide`'s answer for this allocation, when the class is [`Class::Foreign`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForeignOutcome {
    /// `Verdict::Take`: `ForeignFlip` shows it.
    Take,
    /// `Verdict::Refuse(why)`.
    Refuse(ForeignWhy),
}

/// Everything [`decide`] reads, gathered by the driver in one pass (nothing here is a pointer).
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// `IndepFlip` is nonzero.
    pub knob: bool,
    /// `SupportDirectFlip` was advertised in this generation (`AdapterKnobs::direct_flip`).
    pub caps: bool,
    /// The display half is on.
    pub display: bool,
    /// Which Present contract is running (`DXGK_PRESENTFLAGS.Flip`, `pDmaBuffer`).
    pub arm: Arm,
    pub class: Class,
    /// The `PrimaryAddress` dxgkrnl paired with the allocation. 0 can never be retired by a
    /// `CRTC_VSYNC` (a zero address publishes nothing), so the flip would hold dxgkrnl.
    pub address: u64,
    /// `MISC_PRIMARY` is set on the allocation (the UMD created it from a `pPrimaryDesc`).
    pub primary_tagged: bool,
    /// `IdfNeedPrim`.
    pub need_primary: bool,
    /// The allocation's resource id is in the direct-scan-out table (`SCANOUT_ALLOCS`): only
    /// the DMA-buffer flip resolves its source through it.
    pub registered: bool,
    /// The committed mode's extent.
    pub mode: (u32, u32),
    /// The allocation's extent (the KMD's record for a foreign one, the creator's for Venus).
    pub extent: (u32, u32),
    /// The allocation's DXGI format.
    pub dxgi_format: u32,
    /// Venus direct only: the creator's layout (the undersize guard's inputs).
    pub pitch: u32,
    pub plane_offset: u64,
    pub alloc_size: u64,
    /// The allocation's creator (Venus) or importer (foreign) is alive and, for a foreign one,
    /// its DRM file is open. A destroyed allocation never reaches a flip; this is the
    /// owner-death side.
    pub owner_live: bool,
    /// A user `SCANOUT_SET` source (an NVK application's own scanout) holds scanout 0.
    pub user_source: bool,
    /// `foreign_flip::failing`: flips are failing and the presenter is backing off.
    pub failing: bool,
    /// `foreign_flip::decide` for this allocation; read only for [`Class::Foreign`].
    pub foreign: Option<ForeignOutcome>,
}

/// Which existing arm shows a direct source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// `ForeignFlip`: a flip of the importer's GEM through the arbiter's resident source.
    Foreign,
    /// The Venus direct bind: `SET_SCANOUT_BLOB` of the allocation's own blob.
    VenusBind,
}

/// Why a flip is completed as a kept picture instead of being shown. Codes are stable and
/// dense (the counter `IdfRef<NN>` and `IdfWhy`); append, never renumber.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The display half is off.
    NoDisplay,
    /// No resource id, or nothing the Venus path can show.
    Hollow,
    /// The importer or creator is gone (closed file, destroyed, stale generation).
    OwnerGone,
    /// `PrimaryAddress` is 0.
    NoAddress,
    /// `IdfNeedPrim` is on and the allocation was not created as a primary.
    NotPrimary,
    /// A DMA-buffer flip whose source is not in the direct-scan-out table.
    NotRegistered,
    /// The allocation's extent is not the mode's. Independent flip needs the exact mode
    /// (the path supports identity and centered scaling only, no stretch).
    Extent,
    /// Not a format `SET_SCANOUT_BLOB` carries.
    Format,
    /// A wide format (10-bit, fp16): stage S-4.
    WideFormat,
    /// The undersize guard (pitch, offset, size).
    Layout,
    /// A user scan-out source holds the screen.
    UserSource,
    /// Flips are failing: the pause is on.
    Failing,
    /// `ForeignFlip` refused for a reason this table does not name (ring level, no transport,
    /// host capability, KMD-owned record, direct flag).
    ForeignOther,
}

impl Why {
    pub const COUNT: usize = 13;

    pub const ALL: [Why; Self::COUNT] = [
        Why::NoDisplay,
        Why::Hollow,
        Why::OwnerGone,
        Why::NoAddress,
        Why::NotPrimary,
        Why::NotRegistered,
        Why::Extent,
        Why::Format,
        Why::WideFormat,
        Why::Layout,
        Why::UserSource,
        Why::Failing,
        Why::ForeignOther,
    ];

    /// Stable nonzero code, 1 to [`Self::COUNT`].
    pub const fn code(self) -> u32 {
        match self {
            Why::NoDisplay => 1,
            Why::Hollow => 2,
            Why::OwnerGone => 3,
            Why::NoAddress => 4,
            Why::NotPrimary => 5,
            Why::NotRegistered => 6,
            Why::Extent => 7,
            Why::Format => 8,
            Why::WideFormat => 9,
            Why::Layout => 10,
            Why::UserSource => 11,
            Why::Failing => 12,
            Why::ForeignOther => 13,
        }
    }

    /// Index into a counter array (`code() - 1`).
    pub const fn index(self) -> usize {
        self.code() as usize - 1
    }
}

/// What [`decide`] says about one flip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// The table is inert (knob off, caps not advertised, or not a flip): the existing arms
    /// answer, byte for byte as today. Not counted.
    Off,
    /// Zero-copy: show it through this existing arm.
    Direct(Route),
    /// Not direct, but the Venus GPU copy can show it (a non-direct Venus allocation of the
    /// mode's extent). This is what such a flip does today.
    Copy,
    /// Complete the flip as a kept picture; the screen keeps what it showed. Counted by
    /// reason.
    Keep(Why),
}

/// `ForeignFlip`'s reason, in this table's terms.
pub const fn why_of_foreign(w: ForeignWhy) -> Why {
    match w {
        ForeignWhy::SharedFormat => Why::WideFormat,
        ForeignWhy::Extent => Why::Extent,
        ForeignWhy::BadLayout => Why::Layout,
        ForeignWhy::NotAdopted
        | ForeignWhy::Destroyed
        | ForeignWhy::FileClosed
        | ForeignWhy::OwnerGone => Why::OwnerGone,
        ForeignWhy::Failing => Why::Failing,
        ForeignWhy::NoDisplay => Why::NoDisplay,
        ForeignWhy::RingLevel
        | ForeignWhy::LevelUnread
        | ForeignWhy::NoTransport
        | ForeignWhy::HostCap
        | ForeignWhy::KmdOwned
        | ForeignWhy::DirectScanout => Why::ForeignOther,
    }
}

/// The decision table. First match wins; the order is the doc's (6.2): inert states, the
/// environment, the source's identity and life, the contract, then what the pixels are.
pub fn decide(f: &Facts) -> Verdict {
    if !f.knob || !f.caps || !f.arm.is_flip() {
        return Verdict::Off;
    }
    if !f.display {
        return Verdict::Keep(Why::NoDisplay);
    }
    if f.class == Class::Hollow {
        return Verdict::Keep(Why::Hollow);
    }
    if !f.owner_live {
        return Verdict::Keep(Why::OwnerGone);
    }
    if f.address == 0 {
        return Verdict::Keep(Why::NoAddress);
    }
    if f.need_primary && !f.primary_tagged {
        return Verdict::Keep(Why::NotPrimary);
    }
    // Only the DMA-buffer flip resolves its source through the table (`PBFlip` 0xE6 today);
    // the MMIO flip names the global handle itself in `SetVidPnSourceAddress`.
    if f.arm == Arm::FlipDma && !f.registered {
        return Verdict::Keep(Why::NotRegistered);
    }
    if f.user_source {
        return Verdict::Keep(Why::UserSource);
    }
    if f.failing {
        return Verdict::Keep(Why::Failing);
    }
    match f.class {
        Class::Hollow => Verdict::Keep(Why::Hollow), // unreachable: answered above
        Class::Foreign => match f.foreign {
            Some(ForeignOutcome::Take) => Verdict::Direct(Route::Foreign),
            Some(ForeignOutcome::Refuse(w)) => Verdict::Keep(why_of_foreign(w)),
            // The caller did not ask `foreign_flip::decide`: nothing can show it.
            None => Verdict::Keep(Why::ForeignOther),
        },
        Class::VenusDirect => {
            match FormatClass::from_dxgi(f.dxgi_format) {
                FormatClass::Wide => return Verdict::Keep(Why::WideFormat),
                FormatClass::Other => return Verdict::Keep(Why::Format),
                FormatClass::Rgb32(_) => {}
            }
            if f.extent != f.mode {
                return Verdict::Keep(Why::Extent);
            }
            let d = SnapshotDescriptor {
                resource_id: 1, // identity is not the guard's business; it only needs nonzero
                width: f.extent.0,
                height: f.extent.1,
                pitch: f.pitch,
                dxgi_format: f.dxgi_format,
                plane_offset: f.plane_offset,
                venus_alloc_size: f.alloc_size,
                memory_type_index: 0,
                purpose: 0,
            };
            match validate_layout(&d) {
                Ok(()) => Verdict::Direct(Route::VenusBind),
                Err(SnapshotReject::Format) => Verdict::Keep(Why::Format),
                Err(_) => Verdict::Keep(Why::Layout),
            }
        }
        Class::VenusOther => {
            if f.extent != f.mode {
                Verdict::Keep(Why::Extent)
            } else {
                Verdict::Copy
            }
        }
    }
}

/// Counter names (service-key values, REG_DWORD, at most 14 characters, none shared with any
/// other counter or knob in either crate). Event-gated like `Ff*` and `Fk*`: a zero block is
/// published once per StartDevice, then values on events (`zero-copy-present.md` 13.8).
///
/// `IdfRef01` .. `IdfRef13` are the per-reason counts ([`ref_name`]); they are not repeated
/// here.
pub const COUNTERS: [&str; 14] = [
    "IdfKnob",     // the knob in force (IndepFlip, IdfNeedPrim, IdfHoldRel as bits 0 to 2)
    "IdfSeen",     // flips the table was asked about
    "IdfDirect",   // verdict Direct
    "IdfDirFor",   // ... through ForeignFlip
    "IdfDirVen",   // ... through the Venus direct bind
    "IdfCopy",     // verdict Copy
    "IdfKeep",     // verdict Keep
    "IdfWhy",      // the last Keep reason's code
    "IdfArmMmio",  // asked on the MMIO contract
    "IdfArmDma",   // asked on the DMA-buffer contract
    "IdfSwitch",   // the shown source changed owner (a promotion or a demotion edge)
    "IdfHold",     // publications held for the host's release of the replaced buffer
    "IdfHoldTmo",  // ... that gave up waiting
    "IdfUntagged", // direct flips of a source with MISC_PRIMARY clear (the UMD's primary-compat)
];

/// Name of the per-reason counter, `IdfRef01` .. `IdfRef13`.
pub const fn ref_name(why: Why) -> [u8; 8] {
    let c = why.code();
    [
        b'I',
        b'd',
        b'f',
        b'R',
        b'e',
        b'f',
        b'0' + (c / 10) as u8,
        b'0' + (c % 10) as u8,
    ]
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const MODE: (u32, u32) = (5120, 1440);

    /// A foreign flip that `ForeignFlip` would take, on the MMIO contract, everything on.
    fn foreign_ok() -> Facts {
        Facts {
            knob: true,
            caps: true,
            display: true,
            arm: Arm::FlipMmio,
            class: Class::Foreign,
            address: 0x1_2000_0000,
            primary_tagged: true,
            need_primary: false,
            registered: true,
            mode: MODE,
            extent: MODE,
            dxgi_format: 87,
            pitch: MODE.0 * 4,
            plane_offset: 0,
            alloc_size: MODE.0 as u64 * MODE.1 as u64 * 4,
            owner_live: true,
            user_source: false,
            failing: false,
            foreign: Some(ForeignOutcome::Take),
        }
    }

    fn venus_direct_ok() -> Facts {
        Facts {
            class: Class::VenusDirect,
            foreign: None,
            ..foreign_ok()
        }
    }

    fn venus_other_ok() -> Facts {
        Facts {
            class: Class::VenusOther,
            foreign: None,
            ..foreign_ok()
        }
    }

    #[test]
    fn the_happy_paths() {
        assert_eq!(decide(&foreign_ok()), Verdict::Direct(Route::Foreign));
        assert_eq!(
            decide(&venus_direct_ok()),
            Verdict::Direct(Route::VenusBind)
        );
        assert_eq!(decide(&venus_other_ok()), Verdict::Copy);
        for arm in [Arm::FlipMmio, Arm::FlipDma] {
            let f = Facts {
                arm,
                ..foreign_ok()
            };
            assert_eq!(decide(&f), Verdict::Direct(Route::Foreign));
        }
    }

    #[test]
    fn knob_caps_and_a_blt_make_the_table_inert_whatever_else_is_true() {
        // Every other fact at its worst: the answer is still `Off`, not a refusal. Inert means
        // inert: no count, no behaviour change.
        let worst = Facts {
            display: false,
            class: Class::Hollow,
            address: 0,
            owner_live: false,
            user_source: true,
            failing: true,
            registered: false,
            extent: (1, 1),
            foreign: None,
            ..foreign_ok()
        };
        assert_eq!(
            decide(&Facts {
                knob: false,
                ..worst
            }),
            Verdict::Off
        );
        assert_eq!(
            decide(&Facts {
                caps: false,
                ..worst
            }),
            Verdict::Off
        );
        assert_eq!(
            decide(&Facts {
                arm: Arm::Blt,
                ..worst
            }),
            Verdict::Off
        );
        // And the worst facts with everything on are refused, so the test is not vacuous.
        assert!(matches!(decide(&worst), Verdict::Keep(_)));
    }

    #[test]
    fn each_reason_has_a_minimal_trigger() {
        let cases: [(Facts, Why); 13] = [
            (
                Facts {
                    display: false,
                    ..foreign_ok()
                },
                Why::NoDisplay,
            ),
            (
                Facts {
                    class: Class::Hollow,
                    ..foreign_ok()
                },
                Why::Hollow,
            ),
            (
                Facts {
                    owner_live: false,
                    ..foreign_ok()
                },
                Why::OwnerGone,
            ),
            (
                Facts {
                    address: 0,
                    ..foreign_ok()
                },
                Why::NoAddress,
            ),
            (
                Facts {
                    need_primary: true,
                    primary_tagged: false,
                    ..foreign_ok()
                },
                Why::NotPrimary,
            ),
            (
                Facts {
                    arm: Arm::FlipDma,
                    registered: false,
                    ..foreign_ok()
                },
                Why::NotRegistered,
            ),
            (
                Facts {
                    extent: (1920, 1080),
                    ..venus_direct_ok()
                },
                Why::Extent,
            ),
            (
                Facts {
                    dxgi_format: 59,
                    ..venus_direct_ok()
                },
                Why::Format,
            ),
            (
                Facts {
                    dxgi_format: 24,
                    ..venus_direct_ok()
                },
                Why::WideFormat,
            ),
            (
                Facts {
                    pitch: MODE.0 * 4 - 4,
                    ..venus_direct_ok()
                },
                Why::Layout,
            ),
            (
                Facts {
                    user_source: true,
                    ..foreign_ok()
                },
                Why::UserSource,
            ),
            (
                Facts {
                    failing: true,
                    ..foreign_ok()
                },
                Why::Failing,
            ),
            (
                Facts {
                    foreign: None,
                    ..foreign_ok()
                },
                Why::ForeignOther,
            ),
        ];
        let mut seen = [false; Why::COUNT];
        for (f, why) in cases {
            assert_eq!(decide(&f), Verdict::Keep(why), "{why:?}");
            seen[why.index()] = true;
        }
        assert!(seen.iter().all(|s| *s), "a reason has no trigger");
    }

    #[test]
    fn the_rows_are_in_the_tables_order() {
        // Start from a flip that fails every row and remove the faults from the top: the
        // reason moves down one row each time.
        let mut f = Facts {
            knob: true,
            caps: true,
            display: false,
            arm: Arm::FlipDma,
            class: Class::Hollow,
            address: 0,
            primary_tagged: false,
            need_primary: true,
            registered: false,
            mode: MODE,
            extent: MODE,
            dxgi_format: 87,
            pitch: MODE.0 * 4,
            plane_offset: 0,
            alloc_size: MODE.0 as u64 * MODE.1 as u64 * 4,
            owner_live: false,
            user_source: true,
            failing: true,
            foreign: Some(ForeignOutcome::Refuse(ForeignWhy::Extent)),
        };
        assert_eq!(decide(&f), Verdict::Keep(Why::NoDisplay));
        f.display = true;
        assert_eq!(decide(&f), Verdict::Keep(Why::Hollow));
        f.class = Class::Foreign;
        assert_eq!(decide(&f), Verdict::Keep(Why::OwnerGone));
        f.owner_live = true;
        assert_eq!(decide(&f), Verdict::Keep(Why::NoAddress));
        f.address = 0x1000;
        assert_eq!(decide(&f), Verdict::Keep(Why::NotPrimary));
        f.primary_tagged = true;
        assert_eq!(decide(&f), Verdict::Keep(Why::NotRegistered));
        f.registered = true;
        assert_eq!(decide(&f), Verdict::Keep(Why::UserSource));
        f.user_source = false;
        assert_eq!(decide(&f), Verdict::Keep(Why::Failing));
        f.failing = false;
        // The foreign arm's own reason is the last word for a foreign source.
        assert_eq!(decide(&f), Verdict::Keep(Why::Extent));
        f.foreign = Some(ForeignOutcome::Take);
        assert_eq!(decide(&f), Verdict::Direct(Route::Foreign));
    }

    #[test]
    fn only_the_dma_contract_needs_the_table() {
        let mmio = Facts {
            arm: Arm::FlipMmio,
            registered: false,
            ..foreign_ok()
        };
        assert_eq!(decide(&mmio), Verdict::Direct(Route::Foreign));
        let dma = Facts {
            arm: Arm::FlipDma,
            registered: false,
            ..foreign_ok()
        };
        assert_eq!(decide(&dma), Verdict::Keep(Why::NotRegistered));
    }

    #[test]
    fn primary_tagging_matters_only_when_the_knob_says_so() {
        let untagged = Facts {
            primary_tagged: false,
            ..foreign_ok()
        };
        assert_eq!(decide(&untagged), Verdict::Direct(Route::Foreign));
        let strict = Facts {
            need_primary: true,
            ..untagged
        };
        assert_eq!(decide(&strict), Verdict::Keep(Why::NotPrimary));
    }

    #[test]
    fn every_foreign_reason_maps_and_the_map_is_pinned() {
        for w in ForeignWhy::ALL {
            let f = Facts {
                foreign: Some(ForeignOutcome::Refuse(w)),
                ..foreign_ok()
            };
            assert_eq!(decide(&f), Verdict::Keep(why_of_foreign(w)), "{w:?}");
        }
        assert_eq!(why_of_foreign(ForeignWhy::SharedFormat), Why::WideFormat);
        assert_eq!(why_of_foreign(ForeignWhy::Extent), Why::Extent);
        assert_eq!(why_of_foreign(ForeignWhy::BadLayout), Why::Layout);
        assert_eq!(why_of_foreign(ForeignWhy::FileClosed), Why::OwnerGone);
        assert_eq!(why_of_foreign(ForeignWhy::OwnerGone), Why::OwnerGone);
        assert_eq!(why_of_foreign(ForeignWhy::Destroyed), Why::OwnerGone);
        assert_eq!(why_of_foreign(ForeignWhy::NotAdopted), Why::OwnerGone);
        assert_eq!(why_of_foreign(ForeignWhy::Failing), Why::Failing);
        assert_eq!(why_of_foreign(ForeignWhy::NoDisplay), Why::NoDisplay);
        assert_eq!(why_of_foreign(ForeignWhy::HostCap), Why::ForeignOther);
        assert_eq!(why_of_foreign(ForeignWhy::DirectScanout), Why::ForeignOther);
    }

    #[test]
    fn the_venus_direct_guard_is_the_shared_undersize_guard() {
        // Exactly at the minimum: allowed. One byte under: refused. This is the guard that
        // keeps the host from reading past the blob; the table must not weaken it.
        let min = MODE.0 as u64 * MODE.1 as u64 * 4;
        let at = Facts {
            alloc_size: min,
            ..venus_direct_ok()
        };
        assert_eq!(decide(&at), Verdict::Direct(Route::VenusBind));
        let under = Facts {
            alloc_size: min - 1,
            ..venus_direct_ok()
        };
        assert_eq!(decide(&under), Verdict::Keep(Why::Layout));
        // A plane offset that pushes the footprint past the blob, and one past u32.
        let off = Facts {
            plane_offset: 4096,
            ..venus_direct_ok()
        };
        assert_eq!(decide(&off), Verdict::Keep(Why::Layout));
        let big = Facts {
            plane_offset: u32::MAX as u64 + 1,
            alloc_size: u64::MAX,
            ..venus_direct_ok()
        };
        assert_eq!(decide(&big), Verdict::Keep(Why::Layout));
        // A pitch that is not a multiple of 4.
        let odd = Facts {
            pitch: MODE.0 * 4 + 2,
            alloc_size: u64::MAX,
            ..venus_direct_ok()
        };
        assert_eq!(decide(&odd), Verdict::Keep(Why::Layout));
        // A larger pitch (the UMD's 256-byte alignment) with enough blob is fine.
        let aligned = Facts {
            pitch: (MODE.0 * 4 + 255) & !255,
            alloc_size: ((MODE.0 * 4 + 255) & !255) as u64 * MODE.1 as u64,
            ..venus_direct_ok()
        };
        assert_eq!(decide(&aligned), Verdict::Direct(Route::VenusBind));
    }

    #[test]
    fn formats_are_classed_and_only_the_three_carried_ones_go_direct() {
        for (dxgi, want) in [
            (28, FormatClass::Rgb32(ScanoutFormat::Rgba8)),
            (87, FormatClass::Rgb32(ScanoutFormat::Bgra8)),
            (88, FormatClass::Rgb32(ScanoutFormat::Bgrx8)),
            (10, FormatClass::Wide),
            (11, FormatClass::Wide),
            (24, FormatClass::Wide),
            (89, FormatClass::Wide),
            (0, FormatClass::Other),
            (29, FormatClass::Other), // R8G8B8A8_UNORM_SRGB
            (91, FormatClass::Other),
            (103, FormatClass::Other), // NV12 is 103
        ] {
            assert_eq!(FormatClass::from_dxgi(dxgi), want, "{dxgi}");
        }
        for dxgi in [10u32, 11, 24, 89] {
            let f = Facts {
                dxgi_format: dxgi,
                ..venus_direct_ok()
            };
            assert_eq!(decide(&f), Verdict::Keep(Why::WideFormat));
        }
        for dxgi in [28u32, 87, 88] {
            let f = Facts {
                dxgi_format: dxgi,
                ..venus_direct_ok()
            };
            assert_eq!(decide(&f), Verdict::Direct(Route::VenusBind));
        }
    }

    #[test]
    fn stretch_and_centering_are_not_direct_for_any_class() {
        for extent in [(1920, 1080), (5120, 1439), (5119, 1440), (2560, 1440)] {
            for f in [venus_direct_ok(), venus_other_ok()] {
                let f = Facts { extent, ..f };
                assert_eq!(decide(&f), Verdict::Keep(Why::Extent), "{extent:?}");
            }
        }
    }

    #[test]
    fn a_hollow_source_is_never_direct_and_a_venus_copy_is_never_zero_copy() {
        assert_eq!(
            decide(&Facts {
                class: Class::Hollow,
                ..foreign_ok()
            }),
            Verdict::Keep(Why::Hollow)
        );
        // The copy route is a verdict of its own, never `Direct`.
        assert_eq!(decide(&venus_other_ok()), Verdict::Copy);
    }

    #[test]
    fn class_follows_the_completion_invariant() {
        assert_eq!(Class::of(Source::Foreign, false), Class::Foreign);
        assert_eq!(Class::of(Source::Foreign, true), Class::Foreign);
        assert_eq!(Class::of(Source::Hollow, true), Class::Hollow);
        assert_eq!(Class::of(Source::Venus, true), Class::VenusDirect);
        assert_eq!(Class::of(Source::Venus, false), Class::VenusOther);
    }

    #[test]
    fn reason_codes_are_dense_and_stable() {
        let codes: Vec<u32> = Why::ALL.iter().map(|w| w.code()).collect();
        assert_eq!(codes, (1..=Why::COUNT as u32).collect::<Vec<_>>());
        for (i, w) in Why::ALL.iter().enumerate() {
            assert_eq!(w.index(), i);
        }
        // Pinned: a renumbering breaks the counters a tester reads.
        assert_eq!(Why::NoDisplay.code(), 1);
        assert_eq!(Why::Extent.code(), 7);
        assert_eq!(Why::WideFormat.code(), 9);
        assert_eq!(Why::ForeignOther.code(), 13);
    }

    // ---- counter and knob names ---------------------------------------------------------

    fn all_counter_names() -> Vec<std::string::String> {
        let mut names: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        for w in Why::ALL {
            names.push(std::str::from_utf8(&ref_name(w)).unwrap().into());
        }
        names
    }

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        let names = all_counter_names();
        for n in &names {
            // `record_named_bytes` clamps to 14 characters: a longer name would be truncated
            // and could merge with another.
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Idf"), "{n}");
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate counter name");
        // None of the other modules' lists holds one of ours.
        for other in crate::flip_completion::COUNTERS
            .iter()
            .chain(crate::foreign_flip::COUNTERS.iter())
            .chain(crate::stall_diag::COUNTERS.iter())
        {
            assert!(!names.iter().any(|n| n == other), "{other} collides");
        }
    }

    #[test]
    fn knob_names_fit_and_are_not_counter_names() {
        let names = all_counter_names();
        for k in KNOBS {
            // The service-key lookup clamps names to 14 characters (`read_config_dword`).
            assert!(k.len() <= 14, "{k} is longer than 14");
            assert!(!names.iter().any(|n| n == k), "{k} is also a counter");
        }
        let mut sorted: Vec<&str> = KNOBS.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), KNOBS.len());
    }

    #[test]
    fn nothing_in_the_driver_spells_one_of_these_names_yet() {
        // The module is not wired. This pins that, and the day it is wired this test is the
        // one to replace with the `the_counters_the_driver_writes_are_exactly_the_ones_listed`
        // pair the other modules carry (`flip_completion`, `foreign_flip`).
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if !root.exists() {
            return; // a copy of this crate without its sibling: nothing to scan
        }
        let names: Vec<std::string::String> = all_counter_names()
            .into_iter()
            .chain(KNOBS.iter().map(|k| (*k).into()))
            .collect();
        let mut stack = std::vec![root];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in &names {
                        let lit = std::format!("b\"{n}\"");
                        assert!(!text.contains(&lit), "{} already spells {n}", p.display());
                    }
                }
            }
        }
        assert!(checked > 20);
    }
}

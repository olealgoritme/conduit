//! Independent flip (direct flip) of a flip-model swap chain: the pure decision "may this flip
//! source go direct" (`docs/independent-flip.md`, section 6 and the table in 6.2), and the
//! stage-1 knob that turns the independent-flip advertisement on (section 11).
//!
//! WIRED AT STAGE S-1 (`IndepFlip`, default 0). With the knob at 0 nothing here is consulted and
//! the driver behaves byte for byte as before. With it at 1 the KMD advertises the
//! independent-flip caps ([`advertise`]) and evaluates [`decide`] on every flip as a census
//! (`ddi/indep_flip.rs`, counters `Idf*`); nothing is enforced. At 2 it also completes the one
//! flip the driver used to FAIL (a DMA-buffer flip of a Venus allocation that is not in the
//! direct-scan-out table, `PBFlip` 0xE6) as a kept picture ([`keeps_unregistered_dma_flip`]).
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

/// Service-key knob (REG_DWORD, default 0), read once per AddAdapter/StartDevice with the other
/// adapter knobs. 0: off, today's behaviour. 1: advertise the independent-flip caps and count
/// the table's verdicts (census). 2: as 1, and enforce the one behaviour change of stage S-2
/// that is safe without a measurement ([`keeps_unregistered_dma_flip`]). See [`Mode`].
pub const KNOB_ENABLE: &str = "IndepFlip";
/// `IndepFlip` when the value is absent: 1 (advertise and count). Promotion measured on 393.1 and
/// 394.1 ("Hardware: Independent Flip", `IdfRedErr` 0), 10-bit and fp16 chains stay composed, and the
/// safety rows of section 13.7 passed. `IndepFlip` = 0 is the opt-out.
pub const KNOB_DEFAULT: u32 = 1;
/// Reserved (not read yet): a source the UMD did not create as a primary (`MISC_PRIMARY` clear)
/// is refused. The census (`IdfUntagged`) says whether it is ever needed.
pub const KNOB_NEED_PRIMARY: &str = "IdfNeedPrim";
/// Reserved (not read yet): hold the displayed-address publication of a flip until the host
/// released the buffer it replaces (`scanout_release`). Design item S-2b.
pub const KNOB_HOLD_RELEASE: &str = "IdfHoldRel";

/// The knobs the driver reads.
/// `IdfRedirSkip` (default 0): a `DxgkDdiPresent` that carries `RedirectedFlip` (0x2000) on the Blt
/// arm (Flip clear: dxgkrnl's independent-flip candidate present, `DdiPresentForIFlip`) completes
/// with no copy, like a Blt the producer already put on scan-out. An experiment for the case where
/// the copy those presents cost (or fail) is what keeps DWM from promoting: `IdfRedOk` /
/// `IdfRedErr` / `IdfRedSt` say whether they fail, `IdfRedSkip` counts the skipped ones.
pub const KNOB_REDIR_SKIP: &str = "IdfRedirSkip";
pub const KNOBS: [&str; 2] = [KNOB_ENABLE, KNOB_REDIR_SKIP];

/// Whether a Present with these `DXGK_PRESENTFLAGS` is an independent-flip candidate on the Blt arm
/// (`RedirectedFlip` set, `Flip` clear).
pub const fn redirected_blt(present_flags: u32) -> bool {
    present_flags & crate::flip_flags::PRESENT_REDIRECTED_FLIP != 0 && present_flags & (1 << 2) == 0
}

/// What the KMD does with a Blt-arm Present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectedBlt {
    /// Not an independent-flip candidate, or `IndepFlip` is off: the ordinary Blt arm.
    Ordinary,
    /// A candidate with no destination allocation (driver 392.1: source 1, destination 0, every one
    /// failed `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` on the Blt arm's DMA-size check). There is
    /// nothing to copy: complete it with no copy, whatever `IdfRedirSkip` says.
    NoDestination,
    /// A candidate with a destination, and `IdfRedirSkip` = 1: complete it with no copy.
    Skip,
}

/// The decision for one Blt-arm Present: `mode_on` is `IndepFlip` (the only setting under which
/// dxgkrnl sends candidates, `DdiPresentForIFlip`), `skip_knob` is `IdfRedirSkip`.
pub const fn redirected_blt_action(
    mode_on: bool,
    skip_knob: bool,
    present_flags: u32,
    destinations: u32,
) -> RedirectedBlt {
    if !mode_on || !redirected_blt(present_flags) {
        RedirectedBlt::Ordinary
    } else if destinations == 0 {
        RedirectedBlt::NoDestination
    } else if skip_knob {
        RedirectedBlt::Skip
    } else {
        RedirectedBlt::Ordinary
    }
}
/// Names reserved for later stages: they collide with nothing, and the driver does not read them.
pub const RESERVED_KNOBS: [&str; 2] = [KNOB_NEED_PRIMARY, KNOB_HOLD_RELEASE];

/// What `IndepFlip` asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 0 (the opt-out; absent is [`KNOB_DEFAULT`]): nothing advertised, nothing evaluated.
    Off,
    /// 1: the caps are advertised and every flip is judged and counted. No behaviour change on
    /// any flip path.
    Census,
    /// 2: as [`Mode::Census`], plus [`keeps_unregistered_dma_flip`].
    Enforce,
}

impl Mode {
    /// The raw service value. 0 is off, 1 census, 2 enforce. Any other value is read as
    /// [`Mode::Census`]: a mistyped value advertises and counts but never changes a flip path.
    pub const fn from_knob(raw: u32) -> Mode {
        match raw {
            0 => Mode::Off,
            2 => Mode::Enforce,
            _ => Mode::Census,
        }
    }

    pub const fn is_on(self) -> bool {
        !matches!(self, Mode::Off)
    }

    /// The value mirrored as `IdfKnob`: 0, 1 or 2 (the mode actually in force, not the raw value).
    pub const fn code(self) -> u32 {
        match self {
            Mode::Off => 0,
            Mode::Census => 1,
            Mode::Enforce => 2,
        }
    }
}

/// The `DXGK_FLIPCAPS` bits the independent-flip advertisement adds: `FlipIndependent` (bit 4,
/// "MMIO flip to redirected surfaces bypassing DWM Present", WDDM 1.3) and `DdiPresentForIFlip`
/// (bit 5, "Call DxgkDdiPresent when independent flip Present might be issued", WDDM 2.0).
/// `FlipImmediateOnHSync` (bit 6) stays out until it is measured (design 2.2).
pub const IFLIP_CAPS: u32 = crate::flip_flags::FLIPCAPS_FLIP_INDEPENDENT
    | crate::flip_flags::FLIPCAPS_DDI_PRESENT_FOR_IFLIP;

/// The caps surface one adapter-knob snapshot reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Advertised {
    /// `DXGK_DRIVERCAPS.SupportDirectFlip` and the aperture segment's `DirectFlip` flag (one
    /// value for both, so the two can never disagree).
    pub direct_flip: bool,
    /// The raw `FlipCapsX` value handed to `flip_flags::resolve_flip_caps` (which still filters
    /// it to the accepted bits).
    pub flip_caps_x: u32,
}

/// Fold `IndepFlip` into the two existing caps knobs: the advertisement is the OR of what each
/// asks for, so `DirectFlipCaps` and `FlipCapsX` keep working exactly as before when the mode is
/// off, and turning the mode on never takes a bit away.
pub const fn advertise(mode: Mode, direct_flip_caps: bool, flip_caps_x: u32) -> Advertised {
    if mode.is_on() {
        Advertised {
            direct_flip: true,
            flip_caps_x: flip_caps_x | IFLIP_CAPS,
        }
    } else {
        Advertised {
            direct_flip: direct_flip_caps,
            flip_caps_x,
        }
    }
}

/// A DMA-buffer flip of a Venus allocation that is not in the direct-scan-out table (and is not
/// hollow) FAILS its Present today (`PBFlip` 0xE6, `STATUS_INVALID_PARAMETER`). Under
/// independent flip that is an application's buffer dxgkrnl chose to flip, and the doc's rule
/// is that such a flip COMPLETES (section 4.1, `Why::NotRegistered`). True: complete it as a
/// kept picture instead. Only [`Mode::Enforce`] changes it.
pub const fn keeps_unregistered_dma_flip(mode: Mode) -> bool {
    matches!(mode, Mode::Enforce)
}

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

/// What the flip worker (`program_vidpn_source_inner`) knows about a source before any arm
/// runs. Every flip that reaches the worker is a resolved allocation (the MMIO flip's
/// `SetVidPnSourceAddress` handle, or an armed DMA flip, which was in the table by construction).
#[derive(Clone, Copy, Debug)]
pub struct WorkerFacts {
    pub caps: bool,
    pub display: bool,
    pub class: Class,
    pub address: u64,
    pub primary_tagged: bool,
    pub mode: (u32, u32),
    pub extent: (u32, u32),
    pub dxgi_format: u32,
    pub pitch: u32,
    pub plane_offset: u64,
    pub alloc_size: u64,
}

/// The census verdict in the worker, before the arms run.
///
/// Owner life, a user scan-out source and the failing pause are judged inside `ForeignFlip` (and
/// show up there as its refusal), so they are taken as fine here. For a foreign source
/// `ForeignFlip`'s own answer is not known yet: the verdict is `Direct(Foreign)` provisionally,
/// and the caller finishes it with [`finish_foreign`] once the arm answered. The worker refuses a
/// source whose extent is not the mode's before any arm, for every class; for a foreign source
/// that is reported as `Extent` (the reason `foreign_flip::decide` would give).
pub fn census_worker(mode: Mode, w: &WorkerFacts) -> Verdict {
    let f = Facts {
        knob: mode.is_on(),
        caps: w.caps,
        display: w.display,
        arm: Arm::FlipMmio,
        class: w.class,
        address: w.address,
        primary_tagged: w.primary_tagged,
        need_primary: false,
        registered: true,
        mode: w.mode,
        extent: w.extent,
        dxgi_format: w.dxgi_format,
        pitch: w.pitch,
        plane_offset: w.plane_offset,
        alloc_size: w.alloc_size,
        owner_live: true,
        user_source: false,
        failing: false,
        foreign: Some(ForeignOutcome::Take),
    };
    match decide(&f) {
        Verdict::Direct(Route::Foreign) if w.extent != w.mode => Verdict::Keep(Why::Extent),
        v => v,
    }
}

/// Finish a provisional [`census_worker`] verdict once `ForeignFlip` answered: `took` is its
/// `Programmed::Ok` (or the level-5 arm's). A refusal's own reason is counted by `ForeignFlip`
/// (`FfRef<NN>`); here it is [`Why::ForeignOther`]. Any other verdict is returned unchanged.
pub const fn finish_foreign(pre: Verdict, took: bool) -> Verdict {
    match pre {
        Verdict::Direct(Route::Foreign) if !took => Verdict::Keep(Why::ForeignOther),
        v => v,
    }
}

/// What the Present DDI knows about a DMA-buffer flip it answers WITHOUT arming the worker (the
/// counted skip of a foreign source, a hollow source, or the 0xE6 row). An armed DMA flip is
/// judged in the worker instead, so every flip is counted once.
#[derive(Clone, Copy, Debug)]
pub struct DmaFacts {
    pub caps: bool,
    pub display: bool,
    pub class: Class,
    pub address: u64,
    pub registered: bool,
    pub mode: (u32, u32),
    pub extent: (u32, u32),
    pub dxgi_format: u32,
}

/// The census verdict of an unarmed DMA-buffer flip. `ForeignFlip` was not asked (it is off, or
/// the source is not registered), so a foreign source that gets past the table row is
/// [`Why::ForeignOther`].
pub fn census_dma_unarmed(mode: Mode, d: &DmaFacts) -> Verdict {
    decide(&Facts {
        knob: mode.is_on(),
        caps: d.caps,
        display: d.display,
        arm: Arm::FlipDma,
        class: d.class,
        address: d.address,
        primary_tagged: false,
        need_primary: false,
        registered: d.registered,
        mode: d.mode,
        extent: d.extent,
        dxgi_format: d.dxgi_format,
        // Never reached for an unarmed flip (the table row or the class answers first), and
        // zero fails the undersize guard if it ever were: refused, never direct.
        pitch: 0,
        plane_offset: 0,
        alloc_size: 0,
        owner_live: true,
        user_source: false,
        failing: false,
        foreign: None,
    })
}

/// Counter names (service-key values, REG_DWORD, at most 14 characters, none shared with any
/// other counter or knob in either crate). Event-gated like `Ff*` and `Fk*`: a zero block is
/// published once per StartDevice, then values on events (`zero-copy-present.md` 13.8).
///
/// `IdfRef01` .. `IdfRef13` are the per-reason counts ([`ref_name`]); they are not repeated
/// here.
pub const COUNTERS: [&str; 21] = [
    "IdfKnob",     // the mode in force: 0 off, 1 census, 2 enforce (Mode::code)
    "IdfSeen",     // flips the table was asked about
    "IdfDirect",   // verdict Direct
    "IdfDirFor",   // ... through ForeignFlip (or the level-5 RM arm)
    "IdfDirVen",   // ... through the Venus direct bind
    "IdfCopy",     // verdict Copy
    "IdfKeep",     // verdict Keep
    "IdfWhy",      // the last Keep reason's code
    "IdfArmMmio",  // flips on the MMIO contract (SetVidPnSourceAddress calls) while on
    "IdfArmDma",   // flips on the DMA-buffer contract while on
    "IdfUntagged", // direct flips of a source with MISC_PRIMARY clear (the UMD's primary-compat)
    "IdfEnfKeep",  // Mode::Enforce: 0xE6 flips completed as kept pictures instead of failed
    "IdfRedOk",    // Presents with RedirectedFlip that returned STATUS_SUCCESS
    "IdfRedErr",   // ... that returned anything else
    "IdfRedSt",    // ... the last failing status
    "IdfRedSD",    // ... the last one's source count << 16 | destination count
    "IdfRedSkip",  // ... completed with no copy (IdfRedirSkip=1)
    "IdfRedNoDst", // ... with no destination, completed with no copy (always, with IndepFlip)
    "IdfRedFlg",   // ... the last one's DXGK_PRESENTFLAGS
    "IdfRedSite",  // ... the last failing one's return site (present_foreign::site)
    "IdfRedDma",   // ... the last one's DmaSize << 16 | DmaBufferPrivateDataSize (saturated)
];

/// Counter names reserved for later stages (design 6.4), written by nothing yet.
pub const RESERVED_COUNTERS: [&str; 3] = [
    "IdfSwitch",  // the shown source changed owner (a promotion or a demotion edge)
    "IdfHold",    // publications held for the host's release of the replaced buffer
    "IdfHoldTmo", // ... that gave up waiting
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

    // ---- stage S-1: the knob, the caps, the census ---------------------------------------

    #[test]
    fn the_knob_values_and_a_typo_never_enforces() {
        assert_eq!(Mode::from_knob(0), Mode::Off);
        assert_eq!(Mode::from_knob(1), Mode::Census);
        assert_eq!(Mode::from_knob(2), Mode::Enforce);
        for raw in [3u32, 7, 0x10, u32::MAX] {
            assert_eq!(Mode::from_knob(raw), Mode::Census, "{raw}");
        }
        assert!(!Mode::Off.is_on());
        assert!(Mode::Census.is_on() && Mode::Enforce.is_on());
        assert_eq!(
            [Mode::Off.code(), Mode::Census.code(), Mode::Enforce.code()],
            [0, 1, 2]
        );
    }

    #[test]
    fn off_leaves_the_caps_knobs_exactly_as_they_were() {
        for dfc in [false, true] {
            for x in [0u32, 0x10, 0x30, 0x70, 0xFFFF_FFFF] {
                assert_eq!(
                    advertise(Mode::Off, dfc, x),
                    Advertised {
                        direct_flip: dfc,
                        flip_caps_x: x
                    }
                );
            }
        }
        // And the default reports the driver's own word, byte for byte.
        let a = advertise(Mode::Off, false, 0);
        assert_eq!(
            crate::flip_flags::resolve_flip_caps(a.flip_caps_x).reported,
            0x2
        );
        assert!(!a.direct_flip);
    }

    #[test]
    fn on_advertises_direct_flip_and_the_two_iflip_bits_and_takes_nothing_away() {
        assert_eq!(IFLIP_CAPS, 0x30);
        for mode in [Mode::Census, Mode::Enforce] {
            let a = advertise(mode, false, 0);
            assert!(a.direct_flip);
            let caps = crate::flip_flags::resolve_flip_caps(a.flip_caps_x);
            assert_eq!(
                caps.reported, 0x32,
                "FlipOnVSyncMmIo | FlipIndependent | DdiPresentForIFlip"
            );
            assert_eq!(caps.dropped, 0);
            // FlipImmediateOnHSync asked for separately is kept; the mode does not add it.
            let a = advertise(mode, false, 0x40);
            assert_eq!(
                crate::flip_flags::resolve_flip_caps(a.flip_caps_x).reported,
                0x72
            );
            // FlipImmediateMmIo stays impossible whatever is OR'd in.
            let a = advertise(mode, true, 0x08);
            assert_eq!(
                crate::flip_flags::resolve_flip_caps(a.flip_caps_x).reported & 0x08,
                0
            );
        }
    }

    #[test]
    fn only_enforce_completes_the_unregistered_dma_flip() {
        assert!(!keeps_unregistered_dma_flip(Mode::Off));
        assert!(!keeps_unregistered_dma_flip(Mode::Census));
        assert!(keeps_unregistered_dma_flip(Mode::Enforce));
    }

    fn worker(class: Class) -> WorkerFacts {
        WorkerFacts {
            caps: true,
            display: true,
            class,
            address: 0x1_0000_0000,
            primary_tagged: true,
            mode: MODE,
            extent: MODE,
            dxgi_format: 87,
            pitch: MODE.0 * 4,
            plane_offset: 0,
            alloc_size: MODE.0 as u64 * MODE.1 as u64 * 4,
        }
    }

    #[test]
    fn the_worker_census_matches_the_table_and_finishes_foreign_sources() {
        assert_eq!(
            census_worker(Mode::Off, &worker(Class::Foreign)),
            Verdict::Off
        );
        assert_eq!(
            census_worker(
                Mode::Census,
                &WorkerFacts {
                    caps: false,
                    ..worker(Class::Foreign)
                }
            ),
            Verdict::Off
        );
        let pre = census_worker(Mode::Census, &worker(Class::Foreign));
        assert_eq!(pre, Verdict::Direct(Route::Foreign));
        assert_eq!(finish_foreign(pre, true), Verdict::Direct(Route::Foreign));
        assert_eq!(finish_foreign(pre, false), Verdict::Keep(Why::ForeignOther));
        assert_eq!(
            census_worker(Mode::Enforce, &worker(Class::VenusDirect)),
            Verdict::Direct(Route::VenusBind)
        );
        assert_eq!(
            census_worker(Mode::Census, &worker(Class::VenusOther)),
            Verdict::Copy
        );
        // finish_foreign leaves every non-provisional verdict alone.
        for v in [
            Verdict::Off,
            Verdict::Copy,
            Verdict::Direct(Route::VenusBind),
            Verdict::Keep(Why::Extent),
        ] {
            assert_eq!(finish_foreign(v, false), v);
            assert_eq!(finish_foreign(v, true), v);
        }
    }

    #[test]
    fn the_worker_refuses_a_wrong_extent_for_every_class_including_foreign() {
        for class in [Class::Foreign, Class::VenusDirect, Class::VenusOther] {
            let w = WorkerFacts {
                extent: (1280, 720),
                ..worker(class)
            };
            assert_eq!(
                census_worker(Mode::Census, &w),
                Verdict::Keep(Why::Extent),
                "{class:?}"
            );
        }
        // A zero address still comes first (row 4 before the pixels).
        let w = WorkerFacts {
            extent: (1280, 720),
            address: 0,
            ..worker(Class::Foreign)
        };
        assert_eq!(
            census_worker(Mode::Census, &w),
            Verdict::Keep(Why::NoAddress)
        );
    }

    #[test]
    fn the_unarmed_dma_census_names_the_0xe6_row_and_never_goes_direct() {
        let d = DmaFacts {
            caps: true,
            display: true,
            class: Class::VenusOther,
            address: 0x2000,
            registered: false,
            mode: MODE,
            extent: MODE,
            dxgi_format: 87,
        };
        assert_eq!(
            census_dma_unarmed(Mode::Census, &d),
            Verdict::Keep(Why::NotRegistered)
        );
        assert_eq!(census_dma_unarmed(Mode::Off, &d), Verdict::Off);
        let hollow = DmaFacts {
            class: Class::Hollow,
            ..d
        };
        assert_eq!(
            census_dma_unarmed(Mode::Census, &hollow),
            Verdict::Keep(Why::Hollow)
        );
        // A foreign source the table holds but ForeignFlip did not take (the knob is off).
        let foreign = DmaFacts {
            class: Class::Foreign,
            registered: true,
            ..d
        };
        assert_eq!(
            census_dma_unarmed(Mode::Census, &foreign),
            Verdict::Keep(Why::ForeignOther)
        );
        // The combinations the Present answers without arming (a registered Venus source is
        // always armed): never counted as direct or copy.
        for (class, registered) in [
            (Class::Foreign, false),
            (Class::Foreign, true),
            (Class::VenusDirect, false),
            (Class::VenusOther, false),
            (Class::Hollow, false),
            (Class::Hollow, true),
        ] {
            let v = census_dma_unarmed(
                Mode::Enforce,
                &DmaFacts {
                    class,
                    registered,
                    ..d
                },
            );
            assert!(
                matches!(v, Verdict::Keep(_)),
                "{class:?} {registered} {v:?}"
            );
        }
    }

    // ---- counter and knob names ---------------------------------------------------------

    fn all_counter_names() -> Vec<std::string::String> {
        let mut names: Vec<std::string::String> = COUNTERS
            .iter()
            .chain(RESERVED_COUNTERS.iter())
            .map(|s| (*s).into())
            .collect();
        for w in Why::ALL {
            names.push(std::str::from_utf8(&ref_name(w)).unwrap().into());
        }
        names
    }

    #[test]
    fn the_default_is_on_and_zero_still_opts_out() {
        assert_eq!(Mode::from_knob(KNOB_DEFAULT), Mode::Census);
        assert!(advertise(Mode::from_knob(KNOB_DEFAULT), false, 0).direct_flip);
        assert_eq!(Mode::from_knob(0), Mode::Off);
        assert!(!advertise(Mode::Off, false, 0).direct_flip);
    }

    #[test]
    fn a_redirected_blt_is_redirectedflip_without_flip() {
        // 11.6's IdfPrFlg words: RedirectedFlip with Blt (bit 0) and no Flip (bit 2).
        assert!(redirected_blt(0x2001));
        assert!(redirected_blt(0x2000));
        assert!(
            !redirected_blt(0x2004),
            "a redirected FLIP is a flip, never skipped"
        );
        assert!(!redirected_blt(0x0001));
    }

    #[test]
    fn a_candidate_with_no_destination_is_always_completed_without_a_copy() {
        use RedirectedBlt::*;
        assert_eq!(redirected_blt_action(true, false, 0x2001, 0), NoDestination);
        assert_eq!(redirected_blt_action(true, true, 0x2001, 0), NoDestination);
        assert_eq!(redirected_blt_action(true, false, 0x2001, 1), Ordinary);
        assert_eq!(redirected_blt_action(true, true, 0x2001, 1), Skip);
        // IndepFlip off: never (dxgkrnl sends no candidates then anyway).
        assert_eq!(redirected_blt_action(false, true, 0x2001, 0), Ordinary);
        // Not a candidate, or a flip.
        assert_eq!(redirected_blt_action(true, true, 0x0001, 0), Ordinary);
        assert_eq!(redirected_blt_action(true, true, 0x2004, 0), Ordinary);
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
        let all: Vec<&str> = KNOBS.iter().chain(RESERVED_KNOBS.iter()).copied().collect();
        for k in &all {
            // The service-key lookup clamps names to 14 characters (`read_config_dword`).
            assert!(k.len() <= 14, "{k} is longer than 14");
            assert!(!names.iter().any(|n| n == k), "{k} is also a counter");
        }
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }

    /// The sibling `kmd_render/src`, or `None` when this copy of the crate has none. With
    /// `HELIOS_REQUIRE_NAME_SCAN=1` an absent sibling FAILS instead of skipping.
    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    fn rust_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = std::vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        out
    }

    /// The census module of the driver.
    const RENDER_FILE: &str = "ddi/indep_flip.rs";

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(root) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(root.join(RENDER_FILE)).unwrap();
        let mut written: Vec<std::string::String> = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("b\"Idf") {
            let tail = &rest[i + 2..];
            let end = tail.find('"').unwrap();
            let name = &tail[..end];
            if !written.iter().any(|w| w == name) {
                written.push(name.into());
            }
            rest = &tail[end..];
        }
        written.sort();
        let mut listed: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        listed.sort();
        assert_eq!(written, listed);
    }

    #[test]
    fn no_other_driver_file_spells_these_names_and_the_knob_is_read_by_this_name() {
        let Some(root) = render_src() else {
            return;
        };
        let ours = root.join(RENDER_FILE);
        let names: Vec<std::string::String> = all_counter_names()
            .into_iter()
            .chain(RESERVED_KNOBS.iter().map(|k| (*k).into()))
            .collect();
        let mut knob_spelled = 0;
        let files = rust_files(&root);
        assert!(files.len() > 20);
        for p in files {
            let text = std::fs::read_to_string(&p).unwrap();
            if text.contains(&std::format!("b\"{KNOB_ENABLE}\"")) {
                knob_spelled += 1;
            }
            if p == ours {
                continue;
            }
            for n in &names {
                let lit = std::format!("b\"{n}\"");
                assert!(!text.contains(&lit), "{} spells {n}", p.display());
            }
        }
        // `diag::knobs::INDEP_FLIP` is the one spelling of the knob.
        assert_eq!(knob_spelled, 1, "the knob literal must exist exactly once");
    }
}

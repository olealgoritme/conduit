//! Option B for any foreign allocation (`ForeignFlip`, `docs/kmd-rm-client.md` 15.18): the pure
//! decisions behind the KMD's own `ScanoutFlip` of a WDDM allocation that adopted an RM
//! resource a user-mode device imported (DWM-on-NVK's swap-chain buffers).
//!
//! The level 5 primary (`rm_sysmem`) is the same idea for memory the KMD made itself. What is
//! different here, and what these functions decide:
//!
//! * the flip names the DRM file and GEM of ANOTHER device (the record's creator), so the
//!   arbiter's resident source is registered under that device's token, not the KMD's, and
//!   it is only good while that file is still that device's ([`Facts::owner_file`], the
//!   record's `file_closed`);
//! * the memory is not CPU-written by the KMD's own surface machinery, and every frame is a
//!   DIFFERENT allocation (a swap chain cycles its buffers), so the source is updated in place
//!   per flipped allocation ([`Book::set`], the arbiter's `resident_set` keeps the generation of
//!   a source of the same owner);
//! * any of a dozen things can make the allocation not flippable by this route, and each one
//!   must hand it back to the Venus path with a reason ([`Why`], [`decide`]).
//!
//! Everything is a function of its arguments: no wdk, no clock, no atomics.

use crate::foreign_resource::{share_format, FlipRecord};
use crate::foreign_scanout::Layout as FlipLayout;
use crate::rm_sysmem::flip_layout;

/// `device_type` from which an `Open` names a DRM node (`virtio/foreign_scanout.rs`).
pub const DEVICE_TYPE_DRI_FIRST: u32 = 512;

/// Why an allocation was NOT taken by the foreign flip, as the stable code `FfWhy` shows
/// (`Why::code`, nonzero) and the per-reason counter `FfRef<NN>` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `KmdRmClient` 3 or 4: the ring presenter owns the resident source.
    RingLevel,
    /// The worker has not read `KmdRmClient` for this transport generation yet.
    LevelUnread,
    /// No transport (or the NVRM epoch is 0).
    NoTransport,
    /// The display half is off.
    NoDisplay,
    /// The host does not serve the RM import (config features bits 13 and 10).
    HostCap,
    /// The record exists but no WDDM allocation owns the resource.
    NotAdopted,
    /// The adopting allocation was destroyed (its release waits for the last close).
    Destroyed,
    /// The importer closed the DRM file (or went away) since the record was made: the pair
    /// `(rm_handle, gem)` may name another file now.
    FileClosed,
    /// The record is the KMD's own (levels 3 to 5): not this arm's.
    KmdOwned,
    /// The recorded layout is not one `ScanoutFlip` carries (extent under 64, format).
    BadLayout,
    /// The recorded extent is not the mode's.
    Extent,
    /// The importer's DRM file is not that device's in this generation (closed, other epoch).
    OwnerGone,
    /// Flips are failing: the presenter gave up (and waits out its restart pause), or a
    /// registration or a flip failed within its retry pause. Taking another allocation now
    /// would leave the screen frozen (no bind, no `SET_SCANOUT_BLOB`), so the Venus path
    /// runs until the pause is over.
    Failing,
    /// The allocation carries `MISC_DIRECT_SCANOUT`: the fast bind of a flip
    /// (`fast_bind_from_flip`) would race the resident source for the same screen.
    DirectScanout,
    /// The record is a shared format beyond the four 32 bpp RGB ones (`R8`, `YUYV`,
    /// `NV12`, fp16, ...: `docs/shared-formats.md`) or carries a plane 1. `ScanoutFlip`
    /// names one 32 bpp plane, so a flip would read the bytes wrong; the Venus path runs.
    /// Appended as code 15 (the codes before it never move).
    SharedFormat,
}

impl Why {
    pub const COUNT: usize = 15;

    pub const ALL: [Why; Self::COUNT] = [
        Why::RingLevel,
        Why::LevelUnread,
        Why::NoTransport,
        Why::NoDisplay,
        Why::HostCap,
        Why::NotAdopted,
        Why::Destroyed,
        Why::FileClosed,
        Why::KmdOwned,
        Why::BadLayout,
        Why::Extent,
        Why::OwnerGone,
        Why::Failing,
        Why::DirectScanout,
        Why::SharedFormat,
    ];

    /// Stable nonzero code, 1 to [`Self::COUNT`].
    pub const fn code(self) -> u32 {
        match self {
            Why::RingLevel => 1,
            Why::LevelUnread => 2,
            Why::NoTransport => 3,
            Why::NoDisplay => 4,
            Why::HostCap => 5,
            Why::NotAdopted => 6,
            Why::Destroyed => 7,
            Why::FileClosed => 8,
            Why::KmdOwned => 9,
            Why::BadLayout => 10,
            Why::Extent => 11,
            Why::OwnerGone => 12,
            Why::Failing => 13,
            Why::DirectScanout => 14,
            Why::SharedFormat => 15,
        }
    }

    /// Index into a counter array (`code() - 1`).
    pub const fn index(self) -> usize {
        self.code() as usize - 1
    }
}

/// Everything [`decide`] reads, gathered by the driver in one pass.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    /// `ForeignFlip` service-key knob is nonzero.
    pub knob: bool,
    /// `KmdRmClient` of this transport generation, `None` before the worker read it.
    pub level: Option<u32>,
    /// The NVRM epoch of the live transport (0: none).
    pub epoch: u64,
    /// The display half is on.
    pub display: bool,
    /// The host serves the RM import.
    pub host_import: bool,
    /// The KMD's own owner token (`DeviceOwner::KMD_RM.raw()`).
    pub kmd_token: u64,
    /// The mode's extent.
    pub mode: (u32, u32),
    /// The allocation's foreign record (`ForeignTable::flip_record`), if it has one.
    pub record: Option<FlipRecord>,
    /// The `device_type` the record's importer holds its `rm_handle` with in this
    /// generation (`nvrm_handle_device_type`), `None` when that handle is not its own.
    pub owner_file: Option<u32>,
    /// [`failing`]: flips are not working right now.
    pub failing: bool,
    /// The allocation's `MISC_DIRECT_SCANOUT` flag.
    pub direct_scanout: bool,
}

/// Whether flips are failing at `now` (100 ns): the presenter has given up and not been reset
/// yet, or waits for its restart (`restart_at`, 0 = not waiting; `rm_sysmem::restart_pause`),
/// or a registration or flip failed and `fail_until` (its retry pause, 0 = none) has not
/// passed. The pause ending is what lets the arm be used again: a "failing" that stuck would
/// refuse for good.
pub fn failing(now: u64, restart_at: u64, gave_up: bool, fail_until: u64) -> bool {
    gave_up || crate::rm_sysmem::restart_pause(restart_at, now).is_some() || now < fail_until
}

/// The source a taken allocation makes: what the flip names, under whose token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub resid: u32,
    /// The importing device's token (`DeviceOwner::raw`), the arbiter's owner.
    pub owner: u64,
    /// The DRM file handle (the arbiter's handle).
    pub drm: u32,
    pub gem: u32,
    pub epoch: u64,
    pub layout: FlipLayout,
}

/// What [`decide`] says about one programmed allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The knob is off: nothing at all (not counted).
    Off,
    /// No foreign record: an ordinary Venus allocation, or a placeholder that never got an
    /// identity (counted `FfNoRec`).
    NotForeign,
    /// The KMD's own RM system memory: the level 5 arm's, not this one's.
    Sysmem,
    /// Show it by a flip of this source.
    Take(Target),
    /// Do not: the Venus path runs, and `Why` is counted.
    Refuse(Why),
}

/// The decision table. Order matters and is the doc's (15.18): the knob, then "is it foreign
/// at all", then the environment (level, transport, display, host), then "are flips failing",
/// then the record's life, then the allocation's flags and the picture, last the importer's
/// file.
pub fn decide(f: &Facts) -> Verdict {
    if !f.knob {
        return Verdict::Off;
    }
    let Some(rec) = f.record else {
        return Verdict::NotForeign;
    };
    if rec.sysmem {
        return Verdict::Sysmem;
    }
    match f.level {
        None => return Verdict::Refuse(Why::LevelUnread),
        Some(3) | Some(4) => return Verdict::Refuse(Why::RingLevel),
        Some(_) => {}
    }
    if f.epoch == 0 {
        return Verdict::Refuse(Why::NoTransport);
    }
    if !f.display {
        return Verdict::Refuse(Why::NoDisplay);
    }
    if !f.host_import {
        return Verdict::Refuse(Why::HostCap);
    }
    if f.failing {
        return Verdict::Refuse(Why::Failing);
    }
    if !rec.adopted {
        return Verdict::Refuse(Why::NotAdopted);
    }
    if rec.destroyed {
        return Verdict::Refuse(Why::Destroyed);
    }
    if rec.file_closed {
        return Verdict::Refuse(Why::FileClosed);
    }
    if rec.origin == f.kmd_token {
        return Verdict::Refuse(Why::KmdOwned);
    }
    if f.direct_scanout {
        return Verdict::Refuse(Why::DirectScanout);
    }
    // A shared format beyond 32 bpp RGB, or a record with a second plane, is a real record
    // this arm cannot show: refused by its own reason before `flip_layout` (which has no
    // plane 1 and would drop it) is asked anything.
    if !rec.layout.is_rgb32() && share_format(rec.layout.fourcc).is_some() {
        return Verdict::Refuse(Why::SharedFormat);
    }
    let layout = flip_layout(&rec.layout);
    if layout.validate().is_err() {
        return Verdict::Refuse(Why::BadLayout);
    }
    if (layout.width, layout.height) != f.mode {
        return Verdict::Refuse(Why::Extent);
    }
    if !f.owner_file.is_some_and(|t| t >= DEVICE_TYPE_DRI_FIRST) {
        return Verdict::Refuse(Why::OwnerGone);
    }
    Verdict::Take(Target {
        resid: 0, // filled by the caller, which knows the resource id
        owner: rec.origin,
        drm: rec.rm_handle,
        gem: rec.gem_handle,
        epoch: f.epoch,
        layout,
    })
}

/// [`decide`] with the resource id the facts were gathered for.
pub fn decide_for(resid: u32, f: &Facts) -> Verdict {
    match decide(f) {
        Verdict::Take(mut t) => {
            t.resid = resid;
            Verdict::Take(t)
        }
        other => other,
    }
}

/// What a [`Book::set`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing was shown by this arm before.
    New,
    /// The same allocation again (a repeated `SetVidPnSourceAddress`).
    Same,
    /// Another allocation of the same device replaced it: the arbiter's resident source is
    /// updated in place and keeps its generation, so a flip in flight stays valid.
    Moved,
    /// An allocation of ANOTHER device replaced it: the arbiter's source gets a new generation.
    Reowned,
}

impl Change {
    /// Whether the arbiter's resident source must be told (when it is registered).
    pub const fn updates_source(self) -> bool {
        !matches!(self, Change::Same)
    }
}

/// The allocation the screen shows through this arm (plain data under a leaf lock).
#[derive(Debug, Clone, Copy)]
pub struct Book {
    cur: Option<Target>,
}

impl Book {
    pub const fn new() -> Self {
        Book { cur: None }
    }

    pub fn current(&self) -> Option<Target> {
        self.cur
    }

    pub fn set(&mut self, t: Target) -> Change {
        let change = match self.cur {
            None => Change::New,
            Some(c) if c == t => Change::Same,
            Some(c) if c.owner == t.owner => Change::Moved,
            Some(_) => Change::Reowned,
        };
        self.cur = Some(t);
        change
    }

    /// The allocation `resid` is being destroyed: if it is the shown one, forget it.
    pub fn gone(&mut self, resid: u32) -> bool {
        match self.cur {
            Some(c) if c.resid == resid => {
                self.cur = None;
                true
            }
            _ => false,
        }
    }

    /// `owner` closed DRM file `drm`: a shown allocation made from it is not flippable any more.
    pub fn file_closed(&mut self, owner: u64, drm: u32) -> bool {
        match self.cur {
            Some(c) if c.owner == owner && c.drm == drm => {
                self.cur = None;
                true
            }
            _ => false,
        }
    }

    /// `owner`'s device is gone.
    pub fn owner_closed(&mut self, owner: u64) -> bool {
        match self.cur {
            Some(c) if c.owner == owner => {
                self.cur = None;
                true
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.cur = None;
    }
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the shown target is usable in transport generation `epoch`.
pub fn target_ready(t: Option<&Target>, epoch: u64) -> bool {
    t.is_some_and(|t| t.epoch == epoch && epoch != 0)
}

/// The service-key counter names this arm writes (`FfRef<NN>` per [`Why`] are built from
/// [`ref_name`]). At most 13 characters each, all with the `Ff` prefix no other counter uses.
/// `FfKnob` (when the knob is read) and `FfGaveUp` (at the event) are also written at their
/// event; everything else only by the throttled mirror.
pub const COUNTERS: [&str; 22] = [
    "FfKnob",
    "FfProg",
    "FfSame",
    "FfMoved",
    "FfReowned",
    "FfNoRec",
    "FfRef",
    "FfWhy",
    "FfRegs",
    "FfWithdrawn",
    "FfGaveUp",
    "FfFrames",
    "FfReflips",
    "FfYielded",
    "FfFlipFail",
    "FfPres",
    "FfSeq",
    "FfStale",
    "FfGone",
    "FfPoison",
    "FfEdges",
    "FfRegFail",
];

/// Name of the per-reason refusal counter: `FfRef01` .. `FfRef15`.
pub const fn ref_name(why: Why) -> [u8; 7] {
    let c = why.code();
    [
        b'F',
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
    use crate::foreign_resource::{Layout as FrLayout, FOURCC_XRGB8888};
    use crate::foreign_scanout::{
        ForeignScanout, PresentError, ReleaseOutcome, ResidentDrop, ResidentKind, SetKind,
    };
    use crate::rm_present::{Act, FlipResult, Presenter};
    use crate::rm_sysmem::flip_inputs;
    use std::vec::Vec;

    const KMD: u64 = 0xFFFF;
    const DWM: u64 = 0xD000;
    const OTHER: u64 = 0xE000;
    const MODE: (u32, u32) = (1920, 1080);
    const MS: u64 = 10_000;

    fn lay() -> FrLayout {
        FrLayout {
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: 0x0300_0000_0060_6015,
            plane1: None,
        }
    }

    fn rec() -> FlipRecord {
        FlipRecord {
            origin: DWM,
            adopted: true,
            destroyed: false,
            sysmem: false,
            file_closed: false,
            rm_handle: 7,
            gem_handle: 21,
            layout: lay(),
            size: 0x80_0000,
        }
    }

    fn facts() -> Facts {
        Facts {
            knob: true,
            level: Some(0),
            epoch: 3,
            display: true,
            host_import: true,
            kmd_token: KMD,
            mode: MODE,
            record: Some(rec()),
            owner_file: Some(512),
            failing: false,
            direct_scanout: false,
        }
    }

    // ---- the decision table ------------------------------------------------------

    #[test]
    fn the_table_of_cases() {
        type Edit = fn(&mut Facts);
        let cases: &[(&str, Edit, Verdict)] = &[
            ("knob off", |f| f.knob = false, Verdict::Off),
            (
                "knob off beats every other fault",
                |f| {
                    f.knob = false;
                    f.record = None;
                    f.owner_file = None;
                },
                Verdict::Off,
            ),
            ("no record", |f| f.record = None, Verdict::NotForeign),
            (
                "kmd sysmem is the level 5 arm's",
                |f| {
                    let mut r = rec();
                    r.sysmem = true;
                    r.origin = KMD;
                    f.record = Some(r);
                },
                Verdict::Sysmem,
            ),
            (
                "level unread",
                |f| f.level = None,
                Verdict::Refuse(Why::LevelUnread),
            ),
            (
                "level 3",
                |f| f.level = Some(3),
                Verdict::Refuse(Why::RingLevel),
            ),
            (
                "level 4",
                |f| f.level = Some(4),
                Verdict::Refuse(Why::RingLevel),
            ),
            (
                "no transport",
                |f| f.epoch = 0,
                Verdict::Refuse(Why::NoTransport),
            ),
            (
                "no display",
                |f| f.display = false,
                Verdict::Refuse(Why::NoDisplay),
            ),
            (
                "host lacks the import",
                |f| f.host_import = false,
                Verdict::Refuse(Why::HostCap),
            ),
            (
                "not adopted",
                |f| {
                    let mut r = rec();
                    r.adopted = false;
                    f.record = Some(r);
                },
                Verdict::Refuse(Why::NotAdopted),
            ),
            (
                "destroyed",
                |f| {
                    let mut r = rec();
                    r.destroyed = true;
                    f.record = Some(r);
                },
                Verdict::Refuse(Why::Destroyed),
            ),
            (
                "importer closed the file",
                |f| {
                    let mut r = rec();
                    r.file_closed = true;
                    f.record = Some(r);
                },
                Verdict::Refuse(Why::FileClosed),
            ),
            (
                "the KMD's own vidmem import (nothing makes one adopted yet)",
                |f| {
                    let mut r = rec();
                    r.origin = KMD;
                    f.record = Some(r);
                },
                Verdict::Refuse(Why::KmdOwned),
            ),
            (
                "layout missing: extent under the flip floor",
                |f| {
                    let mut r = rec();
                    r.layout.width = 32;
                    r.layout.height = 32;
                    r.layout.stride = 128;
                    f.record = Some(r);
                    f.mode = (32, 32);
                },
                Verdict::Refuse(Why::BadLayout),
            ),
            (
                "a shared format the host flip does not carry (NV12)",
                |f| {
                    let mut r = rec();
                    r.layout.fourcc = 0x3231_564e;
                    f.record = Some(r);
                },
                Verdict::Refuse(Why::SharedFormat),
            ),
            (
                "layout missing: a fourcc nobody shares ('BG24')",
                |f| {
                    let mut r = rec();
                    r.layout.fourcc = 0x3432_4742;
                    f.record = Some(r);
                },
                Verdict::Refuse(Why::BadLayout),
            ),
            (
                "record extent is not the mode's",
                |f| f.mode = (2560, 1440),
                Verdict::Refuse(Why::Extent),
            ),
            (
                "owner gone: the handle is nobody's",
                |f| f.owner_file = None,
                Verdict::Refuse(Why::OwnerGone),
            ),
            (
                "owner gone: the handle is not a DRM node any more",
                |f| f.owner_file = Some(4),
                Verdict::Refuse(Why::OwnerGone),
            ),
            (
                "flips are failing",
                |f| f.failing = true,
                Verdict::Refuse(Why::Failing),
            ),
            (
                "an adopted allocation with MISC_DIRECT_SCANOUT",
                |f| f.direct_scanout = true,
                Verdict::Refuse(Why::DirectScanout),
            ),
            (
                "a user owner on a healthy box is taken at level 0",
                |_| {},
                Verdict::Take(Target {
                    resid: 0,
                    owner: DWM,
                    drm: 7,
                    gem: 21,
                    epoch: 3,
                    layout: flip_layout(&lay()),
                }),
            ),
        ];
        for (name, edit, want) in cases {
            let mut f = facts();
            edit(&mut f);
            assert_eq!(decide(&f), *want, "{name}");
        }
    }

    #[test]
    fn refusal_precedence_is_the_tables_order() {
        let mut f = facts();
        let mut r = rec();
        r.adopted = false;
        r.destroyed = true;
        r.file_closed = true;
        r.origin = KMD;
        r.layout.fourcc = 0;
        f.record = Some(r);
        f.direct_scanout = true;
        f.mode = (1, 1);
        f.owner_file = None;
        // Each step removes the fault that was refused, and the next one in the table's order
        // is what is refused then: not adopted, destroyed, file closed, the KMD's own origin,
        // direct scanout, the layout, the extent, the importer's file.
        let order: &[(&str, fn(&mut Facts), Why)] = &[
            ("not adopted", |_| {}, Why::NotAdopted),
            (
                "destroyed",
                |f| f.record.as_mut().unwrap().adopted = true,
                Why::Destroyed,
            ),
            (
                "file closed",
                |f| f.record.as_mut().unwrap().destroyed = false,
                Why::FileClosed,
            ),
            (
                "kmd origin",
                |f| f.record.as_mut().unwrap().file_closed = false,
                Why::KmdOwned,
            ),
            (
                "direct scanout",
                |f| f.record.as_mut().unwrap().origin = DWM,
                Why::DirectScanout,
            ),
            ("bad layout", |f| f.direct_scanout = false, Why::BadLayout),
            (
                "extent",
                |f| f.record.as_mut().unwrap().layout.fourcc = FOURCC_XRGB8888,
                Why::Extent,
            ),
            ("owner gone", |f| f.mode = MODE, Why::OwnerGone),
        ];
        for (name, fix, want) in order {
            fix(&mut f);
            assert_eq!(decide(&f), Verdict::Refuse(*want), "{name}");
        }
        f.owner_file = Some(512);
        assert!(matches!(decide(&f), Verdict::Take(_)));
        // The environment rows, and "failing", come before the record's life.
        let mut g = facts();
        let mut r = rec();
        r.destroyed = true;
        g.record = Some(r);
        g.failing = true;
        assert_eq!(decide(&g), Verdict::Refuse(Why::Failing));
        g.host_import = false;
        assert_eq!(decide(&g), Verdict::Refuse(Why::HostCap));
        g.display = false;
        assert_eq!(decide(&g), Verdict::Refuse(Why::NoDisplay));
        g.epoch = 0;
        assert_eq!(decide(&g), Verdict::Refuse(Why::NoTransport));
        g.level = Some(4);
        assert_eq!(decide(&g), Verdict::Refuse(Why::RingLevel));
        g.level = None;
        assert_eq!(decide(&g), Verdict::Refuse(Why::LevelUnread));
        // The sysmem row and the no-record row precede all of them, the knob precedes those.
        let mut r = rec();
        r.sysmem = true;
        g.record = Some(r);
        assert_eq!(decide(&g), Verdict::Sysmem);
        g.record = None;
        assert_eq!(decide(&g), Verdict::NotForeign);
        g.knob = false;
        assert_eq!(decide(&g), Verdict::Off);
    }

    #[test]
    fn failing_lasts_exactly_as_long_as_the_pause() {
        let now = 1_000 * MS;
        assert!(!failing(now, 0, false, 0));
        // The presenter gave up and waits for its restart: failing until then, not after.
        assert!(failing(now, now + 5_000 * MS, false, 0));
        assert!(!failing(now, now - 1, false, 0));
        assert!(!failing(now + 5_000 * MS, now + 5_000 * MS, false, 0));
        // A presenter that has given up and was not reset yet is failing whatever the clock says.
        assert!(failing(now, 0, true, 0));
        // A registration or flip failure's retry pause.
        assert!(failing(now, 0, false, now + 100 * MS));
        assert!(!failing(now + 100 * MS, 0, false, now + 100 * MS));
        // It never sticks: with every deadline in the past it is clear.
        assert!(!failing(now, now - 5 * MS, false, now - 5 * MS));
    }

    #[test]
    fn levels_that_leave_the_resident_source_alone_are_taken() {
        for level in [0, 1, 2, 5, 9] {
            let mut f = facts();
            f.level = Some(level);
            assert!(matches!(decide(&f), Verdict::Take(_)), "level {level}");
        }
    }

    #[test]
    fn the_target_carries_the_resource_id_and_the_importers_token() {
        let Verdict::Take(t) = decide_for(42, &facts()) else {
            panic!("not taken");
        };
        assert_eq!((t.resid, t.owner, t.drm, t.gem), (42, DWM, 7, 21));
        assert!(target_ready(Some(&t), 3));
        assert!(!target_ready(Some(&t), 4), "another transport generation");
        assert!(!target_ready(Some(&t), 0));
        assert!(!target_ready(None, 3));
    }

    #[test]
    fn why_codes_are_stable_unique_and_nonzero() {
        let mut seen = Vec::new();
        for (i, w) in Why::ALL.iter().enumerate() {
            assert_eq!(w.code() as usize, i + 1);
            assert_eq!(w.index(), i);
            assert!(!seen.contains(&w.code()));
            seen.push(w.code());
        }
        assert_eq!(Why::ALL.len(), Why::COUNT);
        assert_eq!(&ref_name(Why::RingLevel), b"FfRef01");
        assert_eq!(&ref_name(Why::OwnerGone), b"FfRef12");
        assert_eq!(&ref_name(Why::Failing), b"FfRef13");
        assert_eq!(&ref_name(Why::DirectScanout), b"FfRef14");
    }

    // ---- the book -------------------------------------------------------------------

    fn tgt(resid: u32, owner: u64, drm: u32, gem: u32) -> Target {
        Target {
            resid,
            owner,
            drm,
            gem,
            epoch: 3,
            layout: flip_layout(&lay()),
        }
    }

    #[test]
    fn the_book_tells_a_repeat_from_a_move_from_a_new_owner() {
        let mut b = Book::new();
        assert_eq!(b.set(tgt(1, DWM, 7, 21)), Change::New);
        assert_eq!(b.set(tgt(1, DWM, 7, 21)), Change::Same);
        assert_eq!(b.set(tgt(2, DWM, 7, 22)), Change::Moved);
        assert_eq!(
            b.set(tgt(3, DWM, 8, 5)),
            Change::Moved,
            "another file, same device"
        );
        assert_eq!(b.set(tgt(4, OTHER, 9, 5)), Change::Reowned);
        assert!(!Change::Same.updates_source());
        assert!(Change::New.updates_source() && Change::Moved.updates_source());
        assert!(Change::Reowned.updates_source());
    }

    #[test]
    fn the_book_forgets_what_is_destroyed_or_closed() {
        let mut b = Book::new();
        b.set(tgt(1, DWM, 7, 21));
        assert!(!b.gone(2));
        assert!(b.gone(1));
        assert!(b.current().is_none());
        b.set(tgt(1, DWM, 7, 21));
        assert!(!b.file_closed(DWM, 8));
        assert!(!b.file_closed(OTHER, 7));
        assert!(b.file_closed(DWM, 7));
        b.set(tgt(1, DWM, 7, 21));
        assert!(!b.owner_closed(OTHER));
        assert!(b.owner_closed(DWM));
        b.set(tgt(1, DWM, 7, 21));
        b.clear();
        assert!(b.current().is_none());
    }

    // ---- against the REAL arbiter ----------------------------------------------------

    /// The flip service as the driver runs it for this arm: the same calls, against the real
    /// arbiter, with a wire log of `(owner, handle, gem, layout width)`.
    struct Model {
        p: Presenter,
        arb: ForeignScanout,
        book: Book,
        flips: Vec<(u64, u32, u32)>,
        refused: Vec<PresentError>,
        now: u64,
    }

    impl Model {
        fn new() -> Self {
            let mut p = Presenter::new(1);
            p.set_min_interval(16 * MS);
            Model {
                p,
                arb: ForeignScanout::new(),
                book: Book::new(),
                flips: Vec::new(),
                refused: Vec::new(),
                now: 100 * MS,
            }
        }

        /// `program` of an allocation: the book, and the in-place update when registered.
        fn program(&mut self, t: Target) -> Change {
            let change = self.book.set(t);
            if self.p.registered() && change.updates_source() {
                let _ = self
                    .arb
                    .resident_set(t.owner, t.drm, t.epoch, t.layout, self.now);
            }
            change
        }

        fn step(&mut self, frame: bool, resume: bool) -> Act {
            let (has, fg) = self.arb.resident_state_of(KMD, false);
            let tgt = self.book.current();
            let i = flip_inputs(
                self.now,
                target_ready(tgt.as_ref(), 3),
                has,
                fg,
                frame,
                resume,
            );
            let act = self.p.decide(i);
            match act {
                Act::Register => {
                    let t = tgt.unwrap();
                    let ok = self
                        .arb
                        .resident_set(t.owner, t.drm, t.epoch, t.layout, self.now)
                        .is_ok();
                    self.p.registration(ok, self.now);
                }
                Act::Withdraw => {
                    self.arb.resident_drop_of(KMD, false);
                }
                Act::CopyFlip { slot } | Act::Reflip { slot } => {
                    let copied = matches!(act, Act::CopyFlip { .. });
                    let t = tgt.unwrap();
                    // `present_within`: mint against the arbiter's own source.
                    let r = match self.arb.present(t.owner, t.drm, self.now) {
                        Ok(flip) => {
                            self.flips.push((t.owner, t.gem, flip.layout.width));
                            self.arb.extend(flip.generation, self.now);
                            FlipResult::Shown
                        }
                        Err(e) => {
                            self.refused.push(e);
                            FlipResult::Yielded
                        }
                    };
                    self.p.flipped(slot, copied, r, self.now);
                }
                Act::Idle | Act::WaitUntil(_) => {}
            }
            act
        }

        fn run(&mut self, frame: bool, resume: bool) {
            let (mut f, mut r) = (frame, resume);
            for _ in 0..6 {
                let a = self.step(f, r);
                f = false;
                r = false;
                if matches!(a, Act::Idle | Act::WaitUntil(_)) {
                    break;
                }
            }
        }
    }

    fn user_layout() -> crate::foreign_scanout::Layout {
        crate::foreign_scanout::Layout {
            width: 640,
            height: 480,
            stride: 2560,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: 0,
        }
    }

    #[test]
    fn the_first_allocation_registers_under_the_importers_token_and_is_flipped() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        assert!(m.arb.resident_foreground());
        let r = m.arb.resident().unwrap();
        assert_eq!((r.owner, r.handle), (DWM, 7));
        assert_eq!(m.flips, [(DWM, 21, 1920)]);
        assert_eq!(m.arb.resident_state_of(KMD, false), (true, true));
        assert_eq!(
            m.arb.resident_state_of(KMD, true),
            (false, false),
            "the KMD's own presenters do not see a user device's source"
        );
        // The desktop's flush is withheld while it is the foreground source.
        let a = m.arb.suppress_desktop(m.now).unwrap();
        assert!(a.resident && a.owner == DWM);
    }

    #[test]
    fn the_same_allocation_again_keeps_the_generation_and_a_new_one_updates_in_place() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        let gen = m.arb.resident().unwrap().generation;
        // A repeated SetVidPnSourceAddress of the same allocation: nothing to tell the arbiter.
        assert_eq!(m.program(tgt(1, DWM, 7, 21)), Change::Same);
        assert_eq!(m.arb.resident().unwrap().generation, gen);
        // The swap chain moves to its next buffer: same source, new GEM, generation kept.
        assert_eq!(m.program(tgt(2, DWM, 7, 22)), Change::Moved);
        assert_eq!(m.arb.resident().unwrap().generation, gen);
        m.now += 20 * MS;
        m.run(true, false);
        assert_eq!(m.flips, [(DWM, 21, 1920), (DWM, 22, 1920)]);
        // Another file of the same device: still the same source.
        assert_eq!(m.program(tgt(3, DWM, 9, 5)), Change::Moved);
        assert_eq!(m.arb.resident().unwrap().generation, gen);
        assert_eq!(m.arb.resident().unwrap().handle, 9);
    }

    #[test]
    fn a_new_owner_gets_a_new_generation_and_the_old_owners_flip_is_refused() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        let gen = m.arb.resident().unwrap().generation;
        assert_eq!(m.program(tgt(2, OTHER, 9, 5)), Change::Reowned);
        assert_ne!(m.arb.resident().unwrap().generation, gen);
        // A flip minted for the old device after the change finds no source.
        assert_eq!(
            m.arb.present(DWM, 7, m.now).err(),
            Some(PresentError::NoSource)
        );
        m.now += 20 * MS;
        m.run(true, false);
        assert_eq!(m.flips.last(), Some(&(OTHER, 5, 1920)));
    }

    #[test]
    fn frames_are_paced_to_the_interval_and_the_newest_allocation_wins() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        for (i, gem) in [22u32, 23, 24].into_iter().enumerate() {
            m.now += 4 * MS;
            m.program(tgt(2 + i as u32, DWM, 7, gem));
            m.run(true, false);
        }
        assert_eq!(
            m.flips.len(),
            1,
            "inside the pacing interval nothing is flipped"
        );
        m.now += 10 * MS;
        m.run(false, false);
        assert_eq!(m.flips.len(), 2);
        assert_eq!(m.flips[1].1, 24, "the buffers in between were never shown");
    }

    #[test]
    fn a_user_source_preempts_and_its_end_or_lapse_resumes_the_newest_allocation() {
        for end_by_lapse in [false, true] {
            let mut m = Model::new();
            m.program(tgt(1, DWM, 7, 21));
            m.run(true, false);
            let o = m.arb.set(OTHER, 5, 3, user_layout(), 100, m.now).unwrap();
            assert_eq!(o.kind, SetKind::Preempted);
            assert!(m.arb.suppress_desktop(m.now).is_some_and(|a| !a.resident));
            // DWM keeps flipping while the user source holds the screen: no wire flip,
            // the book follows.
            m.now += 20 * MS;
            m.program(tgt(2, DWM, 7, 22));
            m.run(true, false);
            assert_eq!(
                m.flips.len(),
                1,
                "yielded while a user source holds scanout 0"
            );
            m.now += 20 * MS;
            m.program(tgt(3, DWM, 7, 23));
            if end_by_lapse {
                m.now += 200 * MS;
                assert!(matches!(
                    m.arb.poll(m.now),
                    crate::foreign_scanout::Poll::Lapsed { .. }
                ));
            } else {
                assert!(matches!(
                    m.arb.release(OTHER, Some(5)),
                    ReleaseOutcome::Released { .. }
                ));
            }
            // The resume rule: the resident source took the screen back, a re-flip is owed,
            // and no Venus flush (the desktop stays suppressed).
            assert!(m.arb.resident_foreground());
            assert!(m.arb.take_resume_owed());
            assert!(m.arb.suppress_desktop(m.now).is_some());
            m.now += 20 * MS;
            m.run(false, true);
            assert_eq!(
                m.flips.last(),
                Some(&(DWM, 23, 1920)),
                "the newest allocation, not the one flipped last (lapse = {end_by_lapse})"
            );
        }
    }

    #[test]
    fn the_importer_itself_setting_a_user_source_is_an_update_and_its_end_resumes() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        let o = m.arb.set(DWM, 7, 3, user_layout(), 0, m.now).unwrap();
        assert_eq!(o.kind, SetKind::Updated);
        assert!(!m.arb.resident_foreground());
        m.arb.release(DWM, Some(7));
        assert!(m.arb.resident_foreground());
        assert!(m.arb.take_resume_owed());
    }

    #[test]
    fn the_importers_close_or_exit_ends_the_source_and_the_desktop_is_owed_a_flush() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        assert!(m.arb.release_handle(DWM, 7));
        assert!(m.arb.resident().is_none());
        assert!(m.arb.restore_pending());
        assert!(!m.arb.resident_foreground());
        // Parked behind a user source: the exit of the importer forgets it for good.
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        m.arb.set(OTHER, 5, 3, user_layout(), 0, m.now).unwrap();
        assert!(!m.arb.release_owner(DWM));
        assert!(m.arb.resident().is_none());
        // The user source ending now finds no resident to resume.
        m.arb.release(OTHER, Some(5));
        assert!(!m.arb.take_resume_owed());
        assert!(m.arb.restore_pending());
    }

    #[test]
    fn withdrawing_never_drops_the_other_classs_source() {
        let mut arb = ForeignScanout::new();
        let l = flip_layout(&lay());
        arb.resident_set(KMD, 3, 3, l, 0).unwrap();
        assert_eq!(arb.resident_drop_of(KMD, false), ResidentDrop::None);
        assert!(arb.resident().is_some());
        assert_eq!(arb.resident_drop_of(KMD, true), ResidentDrop::Ended);
        arb.resident_set(DWM, 7, 3, l, 0).unwrap();
        assert_eq!(arb.resident_drop_of(KMD, true), ResidentDrop::None);
        assert!(arb.resident().is_some());
        assert_eq!(arb.resident_drop_of(KMD, false), ResidentDrop::Ended);
    }

    #[test]
    fn a_user_device_taking_over_from_the_kmds_resident_is_a_new_source() {
        let mut arb = ForeignScanout::new();
        let l = flip_layout(&lay());
        let k = arb.resident_set(KMD, 3, 3, l, 0).unwrap();
        let u = arb.resident_set(DWM, 7, 3, l, 0).unwrap();
        assert_eq!(u.kind, ResidentKind::Foreground);
        assert_ne!(k.generation, u.generation);
        // The KMD's own presenter now finds no resident of its class (it stands down by its
        // own rule) and the arbiter still has the user device's.
        assert_eq!(arb.resident_state_of(KMD, true), (false, false));
        assert_eq!(arb.resident_state_of(KMD, false), (true, true));
    }

    #[test]
    fn a_target_that_goes_withdraws_the_source() {
        let mut m = Model::new();
        m.program(tgt(1, DWM, 7, 21));
        m.run(true, false);
        assert!(m.book.gone(1));
        m.now += 20 * MS;
        let a = m.step(false, false);
        assert!(matches!(a, Act::Withdraw));
        assert!(m.arb.resident().is_none());
        assert!(m.arb.restore_pending());
        // Nothing more is flipped; the next allocation registers afresh.
        m.program(tgt(2, DWM, 7, 22));
        m.run(true, false);
        assert!(m.arb.resident_foreground());
        assert_eq!(m.flips.last(), Some(&(DWM, 22, 1920)));
    }

    // ---- the foreign table's side of it ----------------------------------------------

    #[test]
    fn a_file_close_poisons_what_the_file_made_and_only_that() {
        use crate::foreign_resource::{AdoptRequest, ForeignTable};
        let mut t = ForeignTable::new();
        let mut ids = Vec::new();
        for (i, (owner, rm)) in [(DWM, 7u32), (DWM, 7), (DWM, 8), (OTHER, 7)]
            .into_iter()
            .enumerate()
        {
            let r = t.reserve(owner, 8 << 20).unwrap();
            let id = 50 + i as u32;
            t.commit(r, id, 1, rm, 20 + i as u32, lay()).unwrap();
            ids.push(id);
        }
        // Adopted: the creator is gone from the record, the origin is not.
        let req = AdoptRequest {
            declares_foreign: true,
            take_ownership: true,
            ctx_id: 1,
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
            plane_offset: 0,
            claimed_alloc_size: 0,
            supplied_layout: Some(lay()),
            trailer_room: true,
            plane_room: true,
        };
        assert!(t.adopt_for_allocation(50, &req, true, true).is_ok());
        let r = t.flip_record(50).unwrap();
        assert!(r.adopted && !r.file_closed && !r.sysmem);
        assert_eq!((r.origin, r.rm_handle, r.gem_handle), (DWM, 7, 20));
        assert!(!t.flip_record(51).unwrap().adopted);
        assert!(t.flip_record(99).is_none());
        assert_eq!(t.file_closed(DWM, 7), 2);
        assert_eq!(t.file_closed(DWM, 7), 0, "once");
        assert!(t.flip_record(50).unwrap().file_closed);
        assert!(t.flip_record(51).unwrap().file_closed);
        assert!(!t.flip_record(52).unwrap().file_closed, "another file");
        assert!(!t.flip_record(53).unwrap().file_closed, "another device");
        // A file number the host reuses makes NEW records, which are not poisoned.
        let r = t.reserve(DWM, 8 << 20).unwrap();
        t.commit(r, 60, 1, 7, 30, lay()).unwrap();
        assert!(!t.flip_record(60).unwrap().file_closed);
        assert_eq!(t.owner_closed(DWM), 2, "the file 8 record and the new one");
        assert!(t.flip_record(52).unwrap().file_closed);
        assert!(!t.flip_record(53).unwrap().file_closed);
        // A poisoned record is refused by the decision.
        let mut f = facts();
        f.record = t.flip_record(50);
        assert_eq!(decide(&f), Verdict::Refuse(Why::FileClosed));
    }

    // ---- counter names -----------------------------------------------------------------

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        let mut names: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        for w in Why::ALL {
            names.push(std::str::from_utf8(&ref_name(w)).unwrap().into());
        }
        for n in &names {
            assert!(n.len() <= 13, "{n} is longer than 13");
            assert!(n.starts_with("Ff"));
        }
        // No two share the 14-byte prefix the registry lookup clamps to (all are shorter, so
        // equality is the test).
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate counter name");
        // Nothing else in the driver writes or reads a name starting with `Ff` (a literal
        // in another file would merge with ours).
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
                    if p.file_name().is_some_and(|n| n == "foreign_flip.rs") {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(
                        !text.contains("b\"Ff"),
                        "{} writes a counter named Ff*, the foreign flip's prefix",
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
            .join("../kmd_render/src/virtio/foreign_flip.rs");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut written: Vec<std::string::String> = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("b\"Ff") {
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

    // ---- shared formats (docs/shared-formats.md) --------------------------------------

    #[test]
    fn every_shared_format_beyond_rgb32_is_refused_with_its_own_reason() {
        use crate::foreign_resource::test_formats::{valid_layout, BEYOND_RGB32};
        for fourcc in BEYOND_RGB32 {
            for modifier in [0, 0x0300_0000_0060_6015] {
                let mut f = facts();
                let mut r = rec();
                r.layout = valid_layout(fourcc, 1920, 1080, modifier);
                f.record = Some(r);
                assert_eq!(
                    decide(&f),
                    Verdict::Refuse(Why::SharedFormat),
                    "{fourcc:#x} modifier {modifier:#x}"
                );
                // The record's life, the environment and the flags come first; the
                // extent comes after (a format the arm cannot show is that, whatever
                // the mode).
                let mut g = f;
                g.mode = (1, 1);
                assert_eq!(decide(&g), Verdict::Refuse(Why::SharedFormat));
                let mut g = f;
                g.failing = true;
                assert_eq!(decide(&g), Verdict::Refuse(Why::Failing));
                let mut g = f;
                g.direct_scanout = true;
                assert_eq!(decide(&g), Verdict::Refuse(Why::DirectScanout));
                let mut g = f;
                g.record.as_mut().unwrap().destroyed = true;
                assert_eq!(decide(&g), Verdict::Refuse(Why::Destroyed));
            }
        }
    }

    #[test]
    fn a_second_plane_is_never_dropped_by_the_flip_layout() {
        // A 32 bpp fourcc with a plane 1 cannot be recorded (the table refuses it), but if
        // one ever were, the flip must not silently reduce it to its plane 0.
        let mut f = facts();
        let mut r = rec();
        r.layout.plane1 = crate::foreign_resource::Plane {
            stride: 1920,
            offset: 0x80_0000,
            modifier: 0,
        }
        .into();
        f.record = Some(r);
        assert_eq!(decide(&f), Verdict::Refuse(Why::SharedFormat));
    }

    #[test]
    fn the_four_32_bpp_formats_are_still_taken() {
        use crate::foreign_resource::{FOURCC_ABGR8888, FOURCC_ARGB8888, FOURCC_XBGR8888};
        for fourcc in [
            FOURCC_XRGB8888,
            FOURCC_ARGB8888,
            FOURCC_XBGR8888,
            FOURCC_ABGR8888,
        ] {
            let mut f = facts();
            let mut r = rec();
            r.layout.fourcc = fourcc;
            f.record = Some(r);
            assert!(matches!(decide(&f), Verdict::Take(_)), "{fourcc:#x}");
        }
    }

    #[test]
    fn the_shared_format_reason_is_appended_not_renumbered() {
        assert_eq!(Why::SharedFormat.code(), 15);
        assert_eq!(Why::COUNT, 15);
        assert_eq!(&ref_name(Why::SharedFormat), b"FfRef15");
        // Every code before it is where it was.
        let before: [(Why, u32); 14] = [
            (Why::RingLevel, 1),
            (Why::LevelUnread, 2),
            (Why::NoTransport, 3),
            (Why::NoDisplay, 4),
            (Why::HostCap, 5),
            (Why::NotAdopted, 6),
            (Why::Destroyed, 7),
            (Why::FileClosed, 8),
            (Why::KmdOwned, 9),
            (Why::BadLayout, 10),
            (Why::Extent, 11),
            (Why::OwnerGone, 12),
            (Why::Failing, 13),
            (Why::DirectScanout, 14),
        ];
        for (w, c) in before {
            assert_eq!(w.code(), c);
        }
    }
}

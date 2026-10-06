//! The "foreign scanout source": the state machine behind
//! `HELIOS_NVRM_OP_SCANOUT_SET` / `SCANOUT_PRESENT` / `SCANOUT_RELEASE`.
//!
//! While an NVK-on-RM (or librmclient) process presents host GEM objects to
//! scanout 0 through the KMD, the desktop's own Venus flips of the same scanout
//! must stop, or the two alternate on screen. This module decides WHO owns
//! scanout 0 and WHEN the desktop's refresh is suppressed. It knows nothing of
//! the transport: the driver feeds it the owner, the DRM-node handle, the time and
//! the device epoch, and acts on what it answers.
//!
//! ```text
//!            set(owner A)                       release / owner exits /
//!  Inactive ───────────────► Active ───────────► handle closed / lapse / invalid
//!     ▲                       │  ▲                          │
//!     │                       │  │ present() pushes         ▼
//!     │                       └──┘ the lapse out       ReleasePending
//!     │                  (set again: Updated)               │
//!     └─────────────── desktop_restored() ──────────────────┘
//!                      (and set() from any state starts a new source)
//! ```
//!
//! * `Active`: one owner holds scanout 0. The desktop's refresh is suppressed
//!   ([`ForeignScanout::suppress_desktop`]) and only the owner's `present` yields a
//!   [`Flip`] to send. Every `present` pushes the lapse deadline out; a source that
//!   stops presenting for `lapse` (default 2 s) stops suppressing, so a hung or
//!   suspended owner cannot freeze the desktop. That is the fallback for a dead
//!   process the teardown paths did not see.
//! * `ReleasePending`: the source is gone but the desktop owes one fresh flush, so
//!   the viewer shows the desktop again and not the app's last frame. The driver
//!   asks for the refresh and calls [`ForeignScanout::desktop_restored`] once the
//!   worker has queued it. A new `set` may begin at any time, also from here.
//! * Sequence numbers are minted here, strictly increasing across every source
//!   this boot (never reset by release or reset), so the host never sees `seq`
//!   go back.
//!
//! # The resident source (the KMD's own, `docs/kmd-rm-client.md` section 13)
//!
//! A user-mode source holds scanout 0 for a lapse and is replaced by the desktop
//! when it ends. The KMD's own RM surface (level 3 of `KmdRmClient`) is different:
//! it is the desktop itself, shown through RM instead of Venus, so it has no
//! lapse and must come back by itself when a user source ends. It is a RESIDENT
//! source, registered with [`ForeignScanout::resident_set`]:
//!
//! ```text
//!  priority:   user source   >   resident (KMD) source   >   Venus desktop flush
//! ```
//!
//! * the resident source is the foreground source when no user source holds scanout
//!   0 (`Active` with `resident = true`: suppresses the desktop, never lapses, and
//!   only its owner's `present` yields a flip);
//! * a user [`ForeignScanout::set`] PREEMPTS it ([`SetKind::Preempted`]), at once and
//!   without waiting for a lapse; the resident registration is kept, parked;
//! * when the user source ends, by any of the ways a source ends, the resident one
//!   takes the screen back and `resume_owed` is raised. The driver answers it with
//!   a re-flip of the resident surface (no Venus flush: the desktop stays
//!   suppressed), where ending the last source asks for a desktop flush instead;
//! * when the resident source itself ends (its file closed, its generation
//!   invalid, withdrawn by the KMD) it is forgotten and the desktop is owed one
//!   flush, exactly as for any source.
//!
//! Pure functions of their arguments: no wdk, no atomics, no clock. Time is `now`
//! in 100 ns units from any monotonic source.

/// Default lapse when the client asks for 0, and the accepted range, in ms.
pub const DEFAULT_LAPSE_MS: u32 = 2_000;
pub const MIN_LAPSE_MS: u32 = 100;
pub const MAX_LAPSE_MS: u32 = 30_000;

/// Smallest and largest extent a source may have (the host's mode range).
pub const MIN_DIM: u32 = 64;
pub const MAX_DIM: u32 = 16_384;
/// Largest pitch accepted.
pub const MAX_STRIDE: u32 = 1 << 20;

/// `DRM_FORMAT_*` accepted: the four 32-bit RGB formats.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;
pub const FOURCC_ARGB8888: u32 = 0x3432_5241;
pub const FOURCC_XBGR8888: u32 = 0x3432_4258;
pub const FOURCC_ABGR8888: u32 = 0x3432_4241;

const NS100_PER_MS: u64 = 10_000;

/// The picture a source shows: everything of a host `ScanoutFlip` except which
/// GEM object and which sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub fourcc: u32,
    pub modifier: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// Width or height outside `MIN_DIM..=MAX_DIM`.
    Dimensions,
    /// A fourcc this KMD does not forward.
    Format,
    /// Stride under `width * 4` or over `MAX_STRIDE`.
    Stride,
}

impl Layout {
    pub fn validate(&self) -> Result<(), LayoutError> {
        if !(MIN_DIM..=MAX_DIM).contains(&self.width) || !(MIN_DIM..=MAX_DIM).contains(&self.height)
        {
            return Err(LayoutError::Dimensions);
        }
        if !matches!(
            self.fourcc,
            FOURCC_XRGB8888 | FOURCC_ARGB8888 | FOURCC_XBGR8888 | FOURCC_ABGR8888
        ) {
            return Err(LayoutError::Format);
        }
        if u64::from(self.stride) < u64::from(self.width) * 4 || self.stride > MAX_STRIDE {
            return Err(LayoutError::Stride);
        }
        Ok(())
    }
}

/// The live source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Active {
    /// The escaping device (`DeviceOwner::raw`), widened.
    pub owner: u64,
    /// Backend handle of the owner's DRM-node file.
    pub handle: u32,
    /// The NVRM epoch the handle belongs to; a different epoch means it is gone.
    pub epoch: u64,
    /// Identifies this source among all of this boot's, nonzero.
    pub generation: u32,
    pub layout: Layout,
    /// The KMD's resident source: no lapse, yields to a user source.
    pub resident: bool,
    lapse: u64,
    deadline: u64,
}

/// The KMD's standing source: what is shown again when a user source lets go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resident {
    pub owner: u64,
    pub handle: u32,
    pub epoch: u64,
    pub generation: u32,
    pub layout: Layout,
}

impl Resident {
    fn foreground(&self) -> Active {
        Active {
            owner: self.owner,
            handle: self.handle,
            epoch: self.epoch,
            generation: self.generation,
            layout: self.layout,
            resident: true,
            lapse: 0,
            deadline: u64::MAX,
        }
    }
}

/// Where a registered resident source stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentKind {
    /// It holds scanout 0: the driver flips its frames.
    Foreground,
    /// A user source holds scanout 0: it waits, and comes back when that one ends.
    Parked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidentOutcome {
    pub kind: ResidentKind,
    pub generation: u32,
}

/// What [`ForeignScanout::resident_drop`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentDrop {
    /// No resident source was registered.
    None,
    /// It was parked behind a user source: nothing changes on screen.
    Parked,
    /// It held scanout 0: the desktop is owed one flush.
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Inactive,
    Active(Active),
    ReleasePending,
}

/// What [`ForeignScanout::set`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetKind {
    /// No source before (or only a pending restore).
    Activated,
    /// The same owner changed its source in place.
    Updated,
    /// A source whose lapse had run out was replaced by another owner's.
    TookOver,
    /// The KMD's resident source was foreground and a user source took scanout 0 from
    /// it, at once. The resident one is parked, not ended.
    Preempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetOutcome {
    pub kind: SetKind,
    pub generation: u32,
    /// The lapse in effect, in ms (the request clamped, or the default).
    pub lapse_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetError {
    /// Another owner holds scanout 0 and has presented within its lapse.
    Busy,
    Layout(LayoutError),
}

/// One flip to send, minted by [`ForeignScanout::present`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flip {
    pub seq: u64,
    pub generation: u32,
    pub handle: u32,
    pub epoch: u64,
    pub layout: Layout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentError {
    /// No source, or not this caller's / this handle's. One answer, so a process
    /// learns nothing of another's source.
    NoSource,
    /// The caller's source had lapsed; it is released now and must `set` again.
    Lapsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    Released {
        generation: u32,
    },
    /// Nothing is active (also: it lapsed or was already released). Not an error.
    NotActive,
    /// Somebody else's source.
    NotOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    Nothing,
    /// The source ran out its lapse and was released; the desktop is owed a flush.
    Lapsed {
        generation: u32,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ForeignScanout {
    state: State,
    resident: Option<Resident>,
    /// A user source just ended and the resident one took the screen back: the driver
    /// owes a re-flip of its surface.
    resume_owed: bool,
    next_seq: u64,
    next_generation: u32,
}

impl Default for ForeignScanout {
    fn default() -> Self {
        Self::new()
    }
}

fn lapse_100ns(lapse_ms: u32) -> (u64, u32) {
    let ms = if lapse_ms == 0 {
        DEFAULT_LAPSE_MS
    } else {
        lapse_ms.clamp(MIN_LAPSE_MS, MAX_LAPSE_MS)
    };
    (u64::from(ms) * NS100_PER_MS, ms)
}

impl ForeignScanout {
    pub const fn new() -> Self {
        Self {
            state: State::Inactive,
            resident: None,
            resume_owed: false,
            next_seq: 1,
            next_generation: 1,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Take (or change) scanout 0 for `owner`.
    pub fn set(
        &mut self,
        owner: u64,
        handle: u32,
        epoch: u64,
        layout: Layout,
        lapse_ms: u32,
        now: u64,
    ) -> Result<SetOutcome, SetError> {
        layout.validate().map_err(SetError::Layout)?;
        let kind = match &self.state {
            State::Inactive | State::ReleasePending => SetKind::Activated,
            State::Active(a) if a.owner == owner => SetKind::Updated,
            // The KMD's resident source yields to any user source, at once.
            State::Active(a) if a.resident => SetKind::Preempted,
            State::Active(a) if now >= a.deadline => SetKind::TookOver,
            State::Active(_) => return Err(SetError::Busy),
        };
        let (lapse, ms) = lapse_100ns(lapse_ms);
        // An in-place update by the same owner keeps the generation: it is the
        // same source with a new picture or file.
        let generation = match (&self.state, kind) {
            (State::Active(a), SetKind::Updated) => a.generation,
            _ => {
                let g = self.next_generation;
                self.next_generation = self.next_generation.checked_add(1).unwrap_or(1);
                g
            }
        };
        self.state = State::Active(Active {
            owner,
            handle,
            epoch,
            generation,
            layout,
            resident: false,
            lapse,
            deadline: now.saturating_add(lapse),
        });
        // A source that was just set is the one on screen: nothing is owed to the
        // resident one until this one ends.
        self.resume_owed = false;
        Ok(SetOutcome {
            kind,
            generation,
            lapse_ms: ms,
        })
    }

    /// Mint the next flip of `owner`'s source on `handle`. Does NOT move the lapse
    /// deadline: a flip that fails, or takes longer than the lapse to be accepted,
    /// must not keep the source alive. The caller extends it with [`Self::extend`]
    /// once the host took the flip.
    pub fn present(&mut self, owner: u64, handle: u32, now: u64) -> Result<Flip, PresentError> {
        let State::Active(a) = &mut self.state else {
            return Err(PresentError::NoSource);
        };
        if a.owner != owner || a.handle != handle {
            return Err(PresentError::NoSource);
        }
        if now >= a.deadline {
            let was_resident = a.resident;
            self.foreground_ended(was_resident);
            return Err(PresentError::Lapsed);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(Flip {
            seq,
            generation: a.generation,
            handle: a.handle,
            epoch: a.epoch,
            layout: a.layout,
        })
    }

    /// The host took the flip of `generation`: push the lapse deadline out from
    /// `now`. `false` (and no change) if that source is no longer the live one.
    pub fn extend(&mut self, generation: u32, now: u64) -> bool {
        match &mut self.state {
            State::Active(a) if a.generation == generation => {
                a.deadline = now.saturating_add(a.lapse).max(a.deadline);
                true
            }
            _ => false,
        }
    }

    /// `owner` gives scanout 0 back. `handle`, when given, must be the source's.
    pub fn release(&mut self, owner: u64, handle: Option<u32>) -> ReleaseOutcome {
        let State::Active(a) = &self.state else {
            return ReleaseOutcome::NotActive;
        };
        if a.owner != owner || handle.is_some_and(|h| h != a.handle) {
            return ReleaseOutcome::NotOwner;
        }
        let generation = a.generation;
        let was_resident = a.resident;
        self.foreground_ended(was_resident);
        ReleaseOutcome::Released { generation }
    }

    /// Device teardown: `owner` is gone. True if it held the source.
    pub fn release_owner(&mut self, owner: u64) -> bool {
        // A parked resident source of a gone owner goes with it.
        if self.resident.is_some_and(|r| r.owner == owner)
            && !matches!(&self.state, State::Active(a) if a.resident)
        {
            self.resident = None;
        }
        match &self.state {
            State::Active(a) if a.owner == owner => {
                let was_resident = a.resident;
                self.foreground_ended(was_resident);
                true
            }
            _ => false,
        }
    }

    /// `owner` closed `handle`. True if that was the source's file.
    pub fn release_handle(&mut self, owner: u64, handle: u32) -> bool {
        // A parked resident source whose file was closed is gone for good.
        if self
            .resident
            .is_some_and(|r| r.owner == owner && r.handle == handle)
            && !matches!(&self.state, State::Active(a) if a.resident)
        {
            self.resident = None;
        }
        match &self.state {
            State::Active(a) if a.owner == owner && a.handle == handle => {
                let was_resident = a.resident;
                self.foreground_ended(was_resident);
                true
            }
            _ => false,
        }
    }

    /// The driver found the source's handle or epoch no longer valid. Releases
    /// exactly that generation (a newer one is left alone).
    pub fn invalidate(&mut self, generation: u32) -> bool {
        match &self.state {
            State::Active(a) if a.generation == generation => {
                let was_resident = a.resident;
                self.foreground_ended(was_resident);
                true
            }
            _ => false,
        }
    }

    /// Transport reset: nothing of the old generation can be shown, and the desktop
    /// state is rebuilt from scratch, so there is nothing to restore either.
    pub fn reset(&mut self) -> bool {
        let was = matches!(self.state, State::Active(_));
        self.state = State::Inactive;
        self.resident = None;
        self.resume_owed = false;
        was
    }

    /// Expire a source whose owner stopped presenting.
    pub fn poll(&mut self, now: u64) -> Poll {
        match &self.state {
            State::Active(a) if now >= a.deadline => {
                let generation = a.generation;
                let was_resident = a.resident;
                self.foreground_ended(was_resident);
                Poll::Lapsed { generation }
            }
            _ => Poll::Nothing,
        }
    }

    /// The live source, if there is one and it has not lapsed. `Some` means the
    /// desktop's refresh must be suppressed (once the driver has checked the
    /// handle is still the owner's).
    pub fn suppress_desktop(&self, now: u64) -> Option<Active> {
        match &self.state {
            State::Active(a) if now < a.deadline => Some(*a),
            _ => None,
        }
    }

    /// The desktop owes a flush.
    pub fn restore_pending(&self) -> bool {
        matches!(self.state, State::ReleasePending)
    }

    /// The desktop's flush was queued (or there is nothing to flush): the restore
    /// is done. False if nothing was owed, or a new source began meanwhile.
    pub fn desktop_restored(&mut self) -> bool {
        if matches!(self.state, State::ReleasePending) {
            self.state = State::Inactive;
            true
        } else {
            false
        }
    }

    /// The foreground source ended. If it was a user source and the KMD has a resident
    /// one registered, that takes scanout 0 back and a re-flip is owed; otherwise the
    /// desktop is owed one flush. A resident source that ends is forgotten.
    fn foreground_ended(&mut self, was_resident: bool) {
        if was_resident {
            self.resident = None;
        }
        match self.resident {
            Some(r) if !was_resident => {
                self.state = State::Active(r.foreground());
                self.resume_owed = true;
            }
            _ => {
                self.state = State::ReleasePending;
                self.resume_owed = false;
            }
        }
    }

    /// Register (or update) the KMD's resident source. With no user source live it
    /// becomes the foreground source at once ([`ResidentKind::Foreground`]: the driver
    /// copies a first frame and flips it); behind a live user source it waits parked.
    /// A resident source of the same owner is updated in place, keeping its
    /// generation (a flip in flight stays valid).
    pub fn resident_set(
        &mut self,
        owner: u64,
        handle: u32,
        epoch: u64,
        layout: Layout,
        now: u64,
    ) -> Result<ResidentOutcome, SetError> {
        layout.validate().map_err(SetError::Layout)?;
        let generation = match self.resident {
            Some(r) if r.owner == owner => r.generation,
            _ => {
                let g = self.next_generation;
                self.next_generation = self.next_generation.checked_add(1).unwrap_or(1);
                g
            }
        };
        let r = Resident {
            owner,
            handle,
            epoch,
            generation,
            layout,
        };
        self.resident = Some(r);
        // A user source that is live (and not lapsed) keeps the screen; anything else
        // is replaced by the resident one.
        let user_live = matches!(&self.state, State::Active(a) if !a.resident && now < a.deadline);
        if user_live {
            return Ok(ResidentOutcome {
                kind: ResidentKind::Parked,
                generation,
            });
        }
        self.state = State::Active(r.foreground());
        // This is not a resume: the driver flips because it registered, not because a
        // user source ended.
        self.resume_owed = false;
        Ok(ResidentOutcome {
            kind: ResidentKind::Foreground,
            generation,
        })
    }

    /// The KMD withdraws its resident source (it is no longer what the screen shows).
    pub fn resident_drop(&mut self) -> ResidentDrop {
        if self.resident.is_none() {
            return ResidentDrop::None;
        }
        if matches!(&self.state, State::Active(a) if a.resident) {
            self.foreground_ended(true);
            return ResidentDrop::Ended;
        }
        self.resident = None;
        self.resume_owed = false;
        ResidentDrop::Parked
    }

    /// `(a resident source of the given class is registered, it is the foreground source)`.
    /// The class is "the KMD's own" (`owner == kmd`: the RM client's ring and primary, levels
    /// 3 to 5) or "a user device's" (`ForeignFlip`: a foreign allocation's flip, whose
    /// source is its creator's); each flip service asks only about its own, so neither
    /// withdraws or counts the other's.
    pub fn resident_state_of(&self, kmd: u64, kmd_class: bool) -> (bool, bool) {
        let mine = self.resident.is_some_and(|r| (r.owner == kmd) == kmd_class);
        (mine, mine && self.resident_foreground())
    }

    /// [`Self::resident_drop`] for one class only (see [`Self::resident_state_of`]); a
    /// resident source of the other class is left alone and the answer is `None`.
    pub fn resident_drop_of(&mut self, kmd: u64, kmd_class: bool) -> ResidentDrop {
        match self.resident {
            Some(r) if (r.owner == kmd) == kmd_class => self.resident_drop(),
            _ => ResidentDrop::None,
        }
    }

    /// The registered resident source, if any.
    pub fn resident(&self) -> Option<Resident> {
        self.resident
    }

    /// Whether the resident source is the foreground source right now.
    pub fn resident_foreground(&self) -> bool {
        matches!(&self.state, State::Active(a) if a.resident)
    }

    /// A user source ended and the resident one took scanout 0 back: true once, then
    /// false until the next time. The driver answers it with a re-flip.
    pub fn take_resume_owed(&mut self) -> bool {
        core::mem::take(&mut self.resume_owed)
    }

    /// When the worker must next look at an `Active` source, for its wait timeout.
    pub fn next_deadline(&self) -> Option<u64> {
        match &self.state {
            // A resident source never lapses: the worker needs no timed wake for it.
            State::Active(a) if !a.resident => Some(a.deadline),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u64 = 0xA000;
    const B: u64 = 0xB000;
    const MS: u64 = NS100_PER_MS;

    fn layout() -> Layout {
        Layout {
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: 0,
        }
    }

    fn active(s: &mut ForeignScanout, owner: u64, now: u64) -> SetOutcome {
        s.set(owner, 7, 3, layout(), 0, now).unwrap()
    }

    #[test]
    fn layout_rules() {
        let mut l = layout();
        assert_eq!(l.validate(), Ok(()));
        l.width = 63;
        assert_eq!(l.validate(), Err(LayoutError::Dimensions));
        l.width = 16_385;
        assert_eq!(l.validate(), Err(LayoutError::Dimensions));
        l = layout();
        l.height = 0;
        assert_eq!(l.validate(), Err(LayoutError::Dimensions));
        l = layout();
        l.fourcc = 0x3231_564e; // NV12
        assert_eq!(l.validate(), Err(LayoutError::Format));
        l = layout();
        l.stride = 1920 * 4 - 1;
        assert_eq!(l.validate(), Err(LayoutError::Stride));
        l.stride = MAX_STRIDE + 4;
        assert_eq!(l.validate(), Err(LayoutError::Stride));
        l = layout();
        l.modifier = 0x0300_0000_0000_0010; // block linear is allowed
        assert_eq!(l.validate(), Ok(()));
        for f in [FOURCC_ARGB8888, FOURCC_XBGR8888, FOURCC_ABGR8888] {
            l.fourcc = f;
            assert_eq!(l.validate(), Ok(()));
        }
    }

    #[test]
    fn activate_present_release_restore() {
        let mut s = ForeignScanout::new();
        assert!(s.suppress_desktop(0).is_none());
        let o = active(&mut s, A, 0);
        assert_eq!(o.kind, SetKind::Activated);
        assert_eq!(o.lapse_ms, DEFAULT_LAPSE_MS);
        assert_eq!(s.suppress_desktop(1).map(|a| a.owner), Some(A));
        assert!(!s.restore_pending());

        let f = s.present(A, 7, 10 * MS).unwrap();
        assert_eq!(
            (f.seq, f.handle, f.epoch, f.generation),
            (1, 7, 3, o.generation)
        );
        assert_eq!(f.layout, layout());

        assert_eq!(
            s.release(A, Some(7)),
            ReleaseOutcome::Released {
                generation: o.generation
            }
        );
        assert!(s.suppress_desktop(11 * MS).is_none());
        assert!(s.restore_pending());
        assert!(s.desktop_restored());
        assert!(!s.restore_pending());
        assert!(!s.desktop_restored());
        assert_eq!(s.release(A, None), ReleaseOutcome::NotActive);
    }

    #[test]
    fn seq_is_strictly_increasing_across_sources() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        let a = s.present(A, 7, 1).unwrap().seq;
        let b = s.present(A, 7, 2).unwrap().seq;
        assert!(b > a);
        s.release(A, None);
        s.desktop_restored();
        active(&mut s, B, 3);
        let c = s.present(B, 7, 4).unwrap().seq;
        assert!(c > b);
    }

    #[test]
    fn only_the_owner_presents_and_releases() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        assert_eq!(s.present(B, 7, 1), Err(PresentError::NoSource));
        assert_eq!(s.present(A, 8, 1), Err(PresentError::NoSource));
        assert_eq!(s.release(B, None), ReleaseOutcome::NotOwner);
        assert_eq!(s.release(A, Some(8)), ReleaseOutcome::NotOwner);
        assert!(s.suppress_desktop(2).is_some());
        // a refused present minted nothing
        assert_eq!(s.present(A, 7, 3).unwrap().seq, 1);
    }

    #[test]
    fn busy_until_the_holder_lapses() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        assert_eq!(s.set(B, 9, 3, layout(), 0, 100 * MS), Err(SetError::Busy));
        // an accepted present pushes the deadline out
        let f = s.present(A, 7, 1_500 * MS).unwrap();
        assert!(s.extend(f.generation, 1_500 * MS));
        assert_eq!(s.set(B, 9, 3, layout(), 0, 2_500 * MS), Err(SetError::Busy));
        let o = s.set(B, 9, 3, layout(), 0, 3_600 * MS).unwrap();
        assert_eq!(o.kind, SetKind::TookOver);
        assert_eq!(s.present(A, 7, 3_601 * MS), Err(PresentError::NoSource));
        assert_eq!(s.present(B, 9, 3_601 * MS).unwrap().handle, 9);
    }

    #[test]
    fn same_owner_set_updates_in_place() {
        let mut s = ForeignScanout::new();
        let a = active(&mut s, A, 0);
        let mut l = layout();
        l.width = 1280;
        l.height = 720;
        l.stride = 1280 * 4;
        let b = s.set(A, 8, 3, l, 500, 10).unwrap();
        assert_eq!(b.kind, SetKind::Updated);
        assert_eq!(b.generation, a.generation);
        assert_eq!(b.lapse_ms, 500);
        let f = s.present(A, 8, 20).unwrap();
        assert_eq!(f.layout.width, 1280);
        assert_eq!(s.present(A, 7, 21), Err(PresentError::NoSource));
    }

    #[test]
    fn bad_layout_changes_nothing() {
        let mut s = ForeignScanout::new();
        let mut l = layout();
        l.fourcc = 0;
        assert_eq!(
            s.set(A, 7, 3, l, 0, 0),
            Err(SetError::Layout(LayoutError::Format))
        );
        assert_eq!(*s.state(), State::Inactive);
        active(&mut s, A, 0);
        assert!(s.set(A, 7, 3, l, 0, 1).is_err());
        assert!(s.suppress_desktop(2).is_some());
    }

    #[test]
    fn lapse_clamps() {
        let mut s = ForeignScanout::new();
        assert_eq!(
            s.set(A, 7, 3, layout(), 1, 0).unwrap().lapse_ms,
            MIN_LAPSE_MS
        );
        assert_eq!(
            s.set(A, 7, 3, layout(), u32::MAX, 0).unwrap().lapse_ms,
            MAX_LAPSE_MS
        );
        assert_eq!(s.set(A, 7, 3, layout(), 750, 0).unwrap().lapse_ms, 750);
    }

    #[test]
    fn a_silent_owner_stops_suppressing_and_the_desktop_is_restored() {
        let mut s = ForeignScanout::new();
        let o = active(&mut s, A, 0);
        assert_eq!(s.next_deadline(), Some(2_000 * MS));
        // suppression is a pure read: it lapses without anyone calling poll
        assert!(s.suppress_desktop(1_999 * MS).is_some());
        assert!(s.suppress_desktop(2_000 * MS).is_none());
        assert_eq!(s.poll(1_999 * MS), Poll::Nothing);
        assert_eq!(
            s.poll(2_000 * MS),
            Poll::Lapsed {
                generation: o.generation
            }
        );
        assert!(s.restore_pending());
        assert_eq!(s.poll(2_001 * MS), Poll::Nothing);
        assert_eq!(s.next_deadline(), None);
        assert!(s.desktop_restored());
    }

    #[test]
    fn present_alone_does_not_extend_the_lapse() {
        let mut s = ForeignScanout::new();
        let a = active(&mut s, A, 0);
        // default lapse is 2 s: minting flips for 5 s without the host taking any
        // must not keep the source alive past the first deadline
        let f = s.present(A, 7, 1_000 * MS).unwrap();
        assert_eq!(f.generation, a.generation);
        assert_eq!(s.next_deadline(), Some(2_000 * MS));
        assert_eq!(s.present(A, 7, 2_100 * MS), Err(PresentError::Lapsed));
    }

    #[test]
    fn extend_only_moves_the_live_generation_forward() {
        let mut s = ForeignScanout::new();
        let a = active(&mut s, A, 0);
        assert!(s.extend(a.generation, 1_000 * MS));
        assert_eq!(s.next_deadline(), Some(3_000 * MS));
        // an earlier acceptance never pulls the deadline back
        assert!(s.extend(a.generation, 500 * MS));
        assert_eq!(s.next_deadline(), Some(3_000 * MS));
        // a stale generation is ignored
        assert!(!s.extend(a.generation + 1, 1_500 * MS));
        assert_eq!(s.next_deadline(), Some(3_000 * MS));
    }

    #[test]
    fn present_after_lapse_is_refused_and_releases() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        assert_eq!(s.present(A, 7, 2_000 * MS), Err(PresentError::Lapsed));
        assert!(s.restore_pending());
        assert_eq!(s.present(A, 7, 2_001 * MS), Err(PresentError::NoSource));
    }

    #[test]
    fn teardown_paths_release() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        assert!(!s.release_owner(B));
        assert!(!s.release_handle(A, 8));
        assert!(!s.release_handle(B, 7));
        assert!(s.suppress_desktop(1).is_some());
        assert!(s.release_handle(A, 7));
        assert!(s.restore_pending());
        assert!(!s.release_owner(A));
        s.desktop_restored();

        active(&mut s, A, 10);
        assert!(s.release_owner(A));
        assert!(s.suppress_desktop(11).is_none());
        assert!(s.restore_pending());
    }

    #[test]
    fn invalidate_names_a_generation() {
        let mut s = ForeignScanout::new();
        let a = active(&mut s, A, 0);
        assert!(!s.invalidate(a.generation + 1));
        assert!(s.suppress_desktop(1).is_some());
        assert!(s.invalidate(a.generation));
        assert!(s.restore_pending());
        // a stale generation cannot end a newer source
        let b = active(&mut s, B, 5);
        assert_ne!(a.generation, b.generation);
        assert!(!s.invalidate(a.generation));
        assert!(s.suppress_desktop(6).is_some());
    }

    #[test]
    fn a_new_source_cancels_the_pending_restore() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        s.release(A, None);
        assert!(s.restore_pending());
        active(&mut s, B, 1);
        assert!(!s.restore_pending());
        assert!(!s.desktop_restored());
        assert!(s.suppress_desktop(2).is_some());
    }

    #[test]
    fn reset_drops_everything_without_a_restore() {
        let mut s = ForeignScanout::new();
        active(&mut s, A, 0);
        assert!(s.reset());
        assert!(!s.restore_pending());
        assert!(s.suppress_desktop(1).is_none());
        assert!(!s.reset());
        // seq survives: the host must never see it go back
        active(&mut s, A, 5);
        assert_eq!(s.present(A, 7, 6).unwrap().seq, 1);
        s.reset();
        active(&mut s, A, 7);
        assert_eq!(s.present(A, 7, 8).unwrap().seq, 2);
    }

    #[test]
    fn deadline_arithmetic_saturates() {
        let mut s = ForeignScanout::new();
        s.set(A, 7, 3, layout(), 0, u64::MAX - 5).unwrap();
        // The deadline saturates at MAX instead of wrapping to a past time.
        assert!(s.present(A, 7, u64::MAX - 5).is_ok());
        assert_eq!(s.next_deadline(), Some(u64::MAX));
    }

    // ---- the resident (KMD) source ----------------------------------------------------

    /// The KMD's own owner token and its DRM file.
    const K: u64 = u64::MAX;
    const KH: u32 = 3;

    fn resident(s: &mut ForeignScanout, now: u64) -> ResidentOutcome {
        s.resident_set(K, KH, 3, layout(), now).unwrap()
    }

    #[test]
    fn a_resident_source_is_foreground_when_nobody_else_holds_scanout() {
        let mut s = ForeignScanout::new();
        let r = resident(&mut s, 0);
        assert_eq!(r.kind, ResidentKind::Foreground);
        let a = s
            .suppress_desktop(1)
            .expect("the desktop flush is withheld");
        assert!(a.resident);
        assert_eq!((a.owner, a.handle, a.generation), (K, KH, r.generation));
        assert!(s.resident_foreground());
        // No lapse, no timed wake, however long it runs.
        assert_eq!(s.next_deadline(), None);
        assert_eq!(s.poll(u64::MAX - 1), Poll::Nothing);
        assert!(s.suppress_desktop(1_000_000 * 3_600 * MS).is_some());
        // Only the KMD presents; each flip is a new, increasing seq.
        assert_eq!(s.present(A, KH, 10), Err(PresentError::NoSource));
        let f1 = s.present(K, KH, 10).unwrap();
        let f2 = s.present(K, KH, 20 * 3_600 * 1_000 * MS).unwrap();
        assert!(f2.seq > f1.seq);
        assert_eq!(f1.generation, r.generation);
        assert_eq!(f1.layout, layout());
        // A host-accepted flip does not move a deadline that does not exist.
        assert!(s.extend(f1.generation, 5));
        assert_eq!(s.next_deadline(), None);
        // Nothing is owed: it started, it did not resume.
        assert!(!s.take_resume_owed());
        assert!(!s.restore_pending());
    }

    #[test]
    fn a_user_source_preempts_the_resident_one_at_once() {
        let mut s = ForeignScanout::new();
        resident(&mut s, 0);
        // `Busy` would be the answer to another USER source holding scanout 0; the
        // resident one has no lapse and yields anyway.
        let o = s.set(A, 7, 3, layout(), 0, 100 * MS).unwrap();
        assert_eq!(o.kind, SetKind::Preempted);
        let a = s.suppress_desktop(101 * MS).unwrap();
        assert_eq!((a.owner, a.resident), (A, false));
        assert!(!s.resident_foreground());
        // The KMD's flips are refused while it is parked, and mint nothing.
        assert_eq!(s.present(K, KH, 101 * MS), Err(PresentError::NoSource));
        assert_eq!(s.present(A, 7, 101 * MS).unwrap().seq, 1);
        // The registration is kept.
        assert_eq!(s.resident().map(|r| r.handle), Some(KH));
        // A second user source is Busy as always, resident or not.
        assert_eq!(s.set(B, 9, 3, layout(), 0, 102 * MS), Err(SetError::Busy));
        // The user source has a lapse; its deadline is what the worker waits for.
        assert_eq!(s.next_deadline(), Some(100 * MS + 2_000 * MS));
    }

    #[test]
    fn when_the_user_source_ends_the_resident_one_resumes_and_the_desktop_stays_off() {
        let mut s = ForeignScanout::new();
        let r = resident(&mut s, 0);
        s.set(A, 7, 3, layout(), 0, 100 * MS).unwrap();
        assert_eq!(s.present(A, 7, 101 * MS).unwrap().seq, 1);
        assert!(
            !s.take_resume_owed(),
            "nothing owed while the user holds scanout"
        );
        assert!(matches!(
            s.release(A, Some(7)),
            ReleaseOutcome::Released { .. }
        ));
        // Not a desktop restore: the resident source is back on screen.
        assert!(!s.restore_pending());
        assert!(!s.desktop_restored());
        assert!(s.resident_foreground());
        assert!(s.suppress_desktop(200 * MS).is_some());
        assert!(s.take_resume_owed());
        assert!(!s.take_resume_owed(), "once");
        // Its flips work again, in the same generation, with a seq above the user's.
        let f = s.present(K, KH, 201 * MS).unwrap();
        assert_eq!(f.generation, r.generation);
        assert_eq!(
            f.seq, 2,
            "above the user's flip: the host never sees seq go back"
        );
    }

    #[test]
    fn every_way_a_user_source_ends_resumes_the_resident_one() {
        type End = fn(&mut ForeignScanout, u32);
        let ends: [(&str, End); 6] = [
            ("release", |s, _| {
                s.release(A, Some(7));
            }),
            ("release_owner", |s, _| {
                s.release_owner(A);
            }),
            ("release_handle", |s, _| {
                s.release_handle(A, 7);
            }),
            ("invalidate", |s, g| {
                s.invalidate(g);
            }),
            ("poll lapse", |s, _| {
                s.poll(10_000 * MS);
            }),
            ("present after lapse", |s, _| {
                let _ = s.present(A, 7, 10_000 * MS);
            }),
        ];
        for (name, end) in ends {
            let mut s = ForeignScanout::new();
            resident(&mut s, 0);
            let o = s.set(A, 7, 3, layout(), 0, 10 * MS).unwrap();
            end(&mut s, o.generation);
            assert!(s.resident_foreground(), "{name}");
            assert!(!s.restore_pending(), "{name}");
            assert!(s.take_resume_owed(), "{name}");
            assert!(s.present(K, KH, 10_001 * MS).is_ok(), "{name}");
        }
    }

    #[test]
    fn a_stale_generation_cannot_end_the_resident_source_through_invalidate() {
        let mut s = ForeignScanout::new();
        let r = resident(&mut s, 0);
        assert!(!s.invalidate(r.generation + 1));
        assert!(s.resident_foreground());
        // Its own generation can: the driver found the file or the epoch dead.
        assert!(s.invalidate(r.generation));
        assert!(s.restore_pending(), "the desktop is owed a flush");
        assert_eq!(s.resident(), None, "an invalid source is forgotten");
        assert!(!s.take_resume_owed());
    }

    #[test]
    fn the_resident_source_ends_for_good_when_its_own_file_closes() {
        let mut s = ForeignScanout::new();
        resident(&mut s, 0);
        assert!(!s.release_handle(K, KH + 1));
        assert!(s.release_handle(K, KH));
        assert!(s.restore_pending());
        assert_eq!(s.resident(), None);
        assert!(s.suppress_desktop(1).is_none());
        assert!(!s.take_resume_owed());
        // ... and a parked one is forgotten without touching what is on screen.
        let mut s = ForeignScanout::new();
        resident(&mut s, 0);
        s.set(A, 7, 3, layout(), 0, 1).unwrap();
        assert!(
            !s.release_handle(K, KH),
            "the foreground source is not its file"
        );
        assert_eq!(s.resident(), None);
        assert!(s.suppress_desktop(2).is_some());
        // So the user's end now owes the DESKTOP, not a resume.
        s.release(A, None);
        assert!(s.restore_pending());
        assert!(!s.take_resume_owed());
    }

    #[test]
    fn the_kmd_can_withdraw_its_resident_source() {
        let mut s = ForeignScanout::new();
        assert_eq!(s.resident_drop(), ResidentDrop::None);
        resident(&mut s, 0);
        assert_eq!(s.resident_drop(), ResidentDrop::Ended);
        assert!(s.restore_pending());
        assert_eq!(s.resident(), None);
        assert!(s.desktop_restored());
        // Parked: the screen belongs to the user and stays so; nothing is owed after.
        resident(&mut s, 1);
        s.set(A, 7, 3, layout(), 0, 2).unwrap();
        assert_eq!(s.resident_drop(), ResidentDrop::Parked);
        assert!(s.suppress_desktop(3).is_some());
        s.release(A, None);
        assert!(s.restore_pending());
        assert!(!s.take_resume_owed());
    }

    #[test]
    fn registering_behind_a_live_user_source_parks_and_a_lapsed_one_does_not_hold() {
        let mut s = ForeignScanout::new();
        s.set(A, 7, 3, layout(), 0, 0).unwrap();
        let r = resident(&mut s, 100 * MS);
        assert_eq!(r.kind, ResidentKind::Parked);
        assert_eq!(s.suppress_desktop(101 * MS).map(|a| a.owner), Some(A));
        assert!(!s.take_resume_owed());
        s.release(A, None);
        assert!(s.take_resume_owed());
        assert!(s.resident_foreground());
        // A user source that has lapsed (the worker has not polled yet) does not hold.
        let mut s = ForeignScanout::new();
        s.set(A, 7, 3, layout(), 0, 0).unwrap();
        let r = resident(&mut s, 3_000 * MS);
        assert_eq!(r.kind, ResidentKind::Foreground);
        assert!(s.resident_foreground());
        assert_eq!(s.present(A, 7, 3_001 * MS), Err(PresentError::NoSource));
    }

    #[test]
    fn re_registering_updates_in_place_and_keeps_the_generation() {
        let mut s = ForeignScanout::new();
        let a = resident(&mut s, 0);
        let mut l = layout();
        l.width = 1280;
        l.height = 720;
        l.stride = 1280 * 4;
        let b = s.resident_set(K, KH + 1, 4, l, 1).unwrap();
        assert_eq!(b.generation, a.generation);
        let f = s.present(K, KH + 1, 2).unwrap();
        assert_eq!((f.layout.width, f.epoch, f.handle), (1280, 4, KH + 1));
        assert_eq!(s.present(K, KH, 2), Err(PresentError::NoSource));
        // A bad layout changes nothing.
        let mut bad = layout();
        bad.fourcc = 0;
        assert_eq!(
            s.resident_set(K, KH, 3, bad, 3),
            Err(SetError::Layout(LayoutError::Format))
        );
        assert_eq!(s.resident().map(|r| r.handle), Some(KH + 1));
    }

    #[test]
    fn a_new_user_source_cancels_a_resume_nobody_answered_yet() {
        let mut s = ForeignScanout::new();
        resident(&mut s, 0);
        s.set(A, 7, 3, layout(), 0, 1).unwrap();
        s.release(A, None);
        // The driver has not looked yet when another user source takes over.
        let o = s.set(B, 9, 3, layout(), 0, 2).unwrap();
        assert_eq!(o.kind, SetKind::Preempted);
        assert!(
            !s.take_resume_owed(),
            "the user is on screen: nothing to resume"
        );
        s.release(B, None);
        assert!(s.take_resume_owed());
    }

    #[test]
    fn reset_forgets_the_resident_source_and_seq_survives() {
        let mut s = ForeignScanout::new();
        resident(&mut s, 0);
        let first = s.present(K, KH, 1).unwrap().seq;
        assert!(s.reset());
        assert_eq!(s.resident(), None);
        assert!(!s.resident_foreground());
        assert!(!s.restore_pending());
        assert!(!s.take_resume_owed());
        resident(&mut s, 2);
        assert!(s.present(K, KH, 3).unwrap().seq > first);
    }

    #[test]
    fn the_probe_style_source_of_the_same_owner_still_works_beside_the_ring() {
        // Level 2 sets a lapse-limited source with the KMD's token; level 3 never does,
        // but nothing about the old source changes.
        let mut s = ForeignScanout::new();
        let o = s.set(K, KH, 3, layout(), 4_000, 0).unwrap();
        assert_eq!(o.kind, SetKind::Activated);
        assert_eq!(s.next_deadline(), Some(4_000 * MS));
        assert_eq!(
            s.poll(4_000 * MS),
            Poll::Lapsed {
                generation: o.generation
            }
        );
        assert!(
            s.restore_pending(),
            "no resident source: the desktop is owed"
        );
    }
}

#[cfg(test)]
mod shared_format_refusals {
    use super::*;
    use crate::foreign_resource::test_formats::BEYOND_RGB32;

    fn layout(fourcc: u32) -> Layout {
        Layout {
            width: 1920,
            height: 1080,
            stride: 1920 * 8,
            offset: 0,
            fourcc,
            modifier: 0,
        }
    }

    #[test]
    fn a_scanout_set_or_resident_set_of_a_shared_format_is_refused_and_changes_nothing() {
        for f in BEYOND_RGB32 {
            let mut s = ForeignScanout::new();
            assert_eq!(
                s.set(0xA, 7, 3, layout(f), 0, 0),
                Err(SetError::Layout(LayoutError::Format)),
                "{f:#x}"
            );
            assert_eq!(*s.state(), State::Inactive);
            assert_eq!(
                s.resident_set(0xB, 8, 3, layout(f), 1),
                Err(SetError::Layout(LayoutError::Format)),
                "{f:#x}"
            );
            assert!(s.resident().is_none());
            assert_eq!(layout(f).validate(), Err(LayoutError::Format));
        }
        // With a live source, a refused set leaves it alone.
        let mut s = ForeignScanout::new();
        s.set(0xA, 7, 3, layout(FOURCC_XRGB8888), 0, 0).unwrap();
        for f in BEYOND_RGB32 {
            assert!(s.set(0xA, 7, 3, layout(f), 0, 1).is_err());
        }
        assert!(s.suppress_desktop(2).is_some());
    }
}

//! The KMD's RM scanout presenter (`KmdRmClient` level 3): the pure half.
//!
//! Level 3 shows the VidPn primary through an RM video-memory ring instead of Venus'
//! `RESOURCE_FLUSH`: whenever the desktop would have been flushed to the host, the
//! KMD copies the primary's pixels into the ring surface that is NOT on screen and
//! flips it with its own `ScanoutFlip`. The decisions live here, tested on the host:
//!
//! * [`source_layout`]: whether the primary that is bound to scanout 0 can be read
//!   as a linear XRGB picture of the ring's extent (it is a Venus blob the KMD maps),
//!   and where its rows are;
//! * [`Ring`] and [`Presenter`]: which surface to write, when a frame is due, when
//!   it is paced, when the source yields to a user-mode source (and what a resume
//!   needs), and when to give up and leave scanout to Venus for the generation;
//! * [`CopyPlan`]: every bound of a frame copy, checked once, so the I/O layer's raw
//!   pointer loop has nothing left to check.
//!
//! Nothing here touches memory, the transport or the clock. Time is `now` in 100 ns
//! units. Design, performance numbers and the open questions:
//! `docs/kmd-rm-client.md` section 13.

use crate::foreign_scanout::{MAX_DIM, MAX_STRIDE, MIN_DIM};

/// Smallest time between two flips of copied frames: 60 Hz. The desktop edge that
/// asks for a frame can come at the display's rate (240 Hz); a whole-frame CPU copy
/// of a 5120x1440 desktop cannot keep that up, so frames coalesce: the newest
/// content is what the next due flip carries. The ring's job is to keep the GDI and
/// fallback desktop alive, not to be a game's present path (that is a user-mode
/// source that flips its own NVK images with no KMD copy at all).
pub const MIN_FRAME_INTERVAL_100NS: u64 = 160_000;
/// A flip, a registration or a source read that fails this many times in a row ends
/// the presenter for the transport generation: the desktop goes back to Venus.
pub const MAX_CONSECUTIVE_FAILS: u8 = 3;
/// Pause before registering again after the arbiter ended our registration under us.
pub const REREGISTER_PAUSE_100NS: u64 = 1_000_000;
/// Pause (100 ms) before the next attempt after a registration, a flip or a source read
/// that FAILED. It is what keeps [`MAX_CONSECUTIVE_FAILS`] strikes apart in time: a
/// transient condition (a dedicated image being replaced, a host that is busy for a
/// moment) must not be able to spend all of them inside one worker pass and end the
/// presenter for the whole generation. The presenter enforces it itself
/// ([`Presenter::decide`] answers [`Act::WaitUntil`]), whatever the caller's pass
/// structure.
pub const RETRY_AFTER_FAIL_100NS: u64 = 1_000_000;

// ---- the source -------------------------------------------------------------------

/// Where the primary's rows are, in the blob the KMD maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLayout {
    /// Bytes from the blob's start to the first row.
    pub offset: u64,
    /// Bytes between rows.
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
}

/// Why the primary cannot be copied from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceRefusal {
    /// Nothing is bound to scanout 0, or no primary identity is published.
    NoSource,
    /// What is bound is not the published LINEAR primary (a UMD's own OPTIMAL image,
    /// for instance): its bytes are not a pitched picture.
    NotThePrimary,
    /// The primary's extent is not the ring's.
    ExtentMismatch,
    /// The published pitch cannot hold a row, or is absurd.
    BadPitch,
    /// The rows do not fit the published allocation size.
    TooSmall,
}

/// Judge the published primary identity (`active_scanout_*` and `primary_scanout_*`,
/// read coherently by the caller) against the ring's extent `(w, h)`.
///
/// `layout_word` is `pitch << 32 | plane_offset`, `wh` is `width << 32 | height`.
pub fn source_layout(
    active_resource: u32,
    primary_resource: u32,
    primary_wh: u64,
    layout_word: u64,
    alloc_size: u64,
    ring: (u32, u32),
) -> Result<SourceLayout, SourceRefusal> {
    if active_resource == 0 || primary_resource == 0 {
        return Err(SourceRefusal::NoSource);
    }
    if active_resource != primary_resource {
        return Err(SourceRefusal::NotThePrimary);
    }
    let (w, h) = ((primary_wh >> 32) as u32, primary_wh as u32);
    if (w, h) != ring || !(MIN_DIM..=MAX_DIM).contains(&w) || !(MIN_DIM..=MAX_DIM).contains(&h) {
        return Err(SourceRefusal::ExtentMismatch);
    }
    let pitch = (layout_word >> 32) as u32;
    let offset = u64::from(layout_word as u32);
    if u64::from(pitch) < u64::from(w) * 4 || pitch > MAX_STRIDE {
        return Err(SourceRefusal::BadPitch);
    }
    // The last row needs only `w * 4` bytes, but ask for the whole pitch of every row
    // but the last: a shorter allocation than that is not this layout.
    let need = offset
        .checked_add(u64::from(pitch) * u64::from(h - 1))
        .and_then(|v| v.checked_add(u64::from(w) * 4))
        .ok_or(SourceRefusal::TooSmall)?;
    if need > alloc_size {
        return Err(SourceRefusal::TooSmall);
    }
    Ok(SourceLayout {
        offset,
        pitch,
        width: w,
        height: h,
    })
}

// ---- the copy ---------------------------------------------------------------------

/// A frame copy with every bound checked: the offsets of row `y` in the source and
/// in the destination, for `y` in `y0..y1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPlan {
    src_offset: u64,
    src_pitch: u64,
    dst_pitch: u64,
    /// Bytes per row actually copied (`width * 4`).
    pub row_bytes: u32,
    pub y0: u32,
    pub y1: u32,
}

impl CopyPlan {
    /// Plan the copy of rows `y0..y1` of a `width`-pixel picture from a source
    /// mapping of `src_len` bytes (rows start at `src.offset`, `src.pitch` apart) to
    /// a destination mapping of `dst_len` bytes (`dst_pitch` apart from byte 0).
    pub fn new(
        src: &SourceLayout,
        src_len: u64,
        dst_pitch: u32,
        dst_len: u64,
        y0: u32,
        y1: u32,
    ) -> Option<CopyPlan> {
        if y0 >= y1 || y1 > src.height {
            return None;
        }
        let row_bytes = src.width.checked_mul(4)?;
        if dst_pitch < row_bytes || src.pitch < row_bytes {
            return None;
        }
        // Last byte read and written.
        let src_end = src
            .offset
            .checked_add(u64::from(src.pitch).checked_mul(u64::from(y1 - 1))?)?
            .checked_add(u64::from(row_bytes))?;
        let dst_end = u64::from(dst_pitch)
            .checked_mul(u64::from(y1 - 1))?
            .checked_add(u64::from(row_bytes))?;
        if src_end > src_len || dst_end > dst_len {
            return None;
        }
        Some(CopyPlan {
            src_offset: src.offset,
            src_pitch: u64::from(src.pitch),
            dst_pitch: u64::from(dst_pitch),
            row_bytes,
            y0,
            y1,
        })
    }

    /// `(source offset, destination offset)` of row `y`. In range by construction for
    /// every `y` in `y0..y1`; `None` outside it.
    pub fn row(&self, y: u32) -> Option<(u64, u64)> {
        if y < self.y0 || y >= self.y1 {
            return None;
        }
        Some((
            self.src_offset + self.src_pitch * u64::from(y),
            self.dst_pitch * u64::from(y),
        ))
    }

    /// Bytes the copy moves.
    pub fn bytes(&self) -> u64 {
        u64::from(self.row_bytes) * u64::from(self.y1 - self.y0)
    }

    /// The whole of the destination rows' span (what a write-combined view must
    /// flush before the host reads it): offset of the first byte and of the last.
    pub fn dst_span(&self) -> (u64, u64) {
        (
            self.dst_pitch * u64::from(self.y0),
            self.dst_pitch * u64::from(self.y1 - 1) + u64::from(self.row_bytes),
        )
    }
}

// ---- the ring ---------------------------------------------------------------------

/// Which surface of the ring is shown, and which gets the next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ring {
    n: u8,
    front: Option<u8>,
}

impl Ring {
    pub const fn new(n: u8) -> Ring {
        Ring { n, front: None }
    }

    /// The surface shown last (flipped and accepted by the host), if any.
    pub fn front(&self) -> Option<u8> {
        self.front
    }

    /// The surface the next frame is written to: never the one shown last, because
    /// the viewer may still be reading it (the host flip has no completion).
    pub fn back(&self) -> u8 {
        match self.front {
            Some(f) if self.n > 1 => (f + 1) % self.n,
            _ => 0,
        }
    }

    /// `slot` was flipped and the host took it.
    pub fn commit(&mut self, slot: u8) {
        if slot < self.n {
            self.front = Some(slot);
        }
    }

    /// Nothing is shown any more (withdrawn, new extent, new generation).
    pub fn clear(&mut self) {
        self.front = None;
    }
}

// ---- the presenter ----------------------------------------------------------------

/// What one worker pass knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub now: u64,
    /// The client's ring is complete and healthy (`Client::presentable`).
    pub ring_ready: bool,
    /// The VidPn primary is bound to scanout 0 and [`source_layout`] accepts it.
    pub source_ok: bool,
    /// The arbiter still has our resident registration.
    pub arbiter_has_resident: bool,
    /// The resident source is the foreground source (no user source holds scanout 0).
    pub foreground: bool,
    /// The desktop wanted a flush since the last pass (the suppression gate dropped one).
    pub frame_edge: bool,
    /// A user source ended and the resident one took the screen back.
    pub resume_edge: bool,
}

/// What to do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Idle,
    /// Register the resident source with the arbiter; answer with
    /// [`Presenter::registered`].
    Register,
    /// Withdraw the resident source from the arbiter (the desktop flush returns).
    Withdraw,
    /// Copy the primary into `slot`, flip it; answer with [`Presenter::flipped`].
    CopyFlip {
        slot: u8,
    },
    /// Flip `slot` again, with no copy (a resume); answer with [`Presenter::flipped`].
    Reflip {
        slot: u8,
    },
    /// Nothing is due before `0` (100 ns, the clock `now` is on).
    WaitUntil(u64),
}

/// How a flip ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipResult {
    /// The host took it.
    Shown,
    /// The source is not ours to show right now (a user source holds scanout 0, or the
    /// registration lapsed): not a failure, the frame stays owed.
    Yielded,
    /// The host or the transport refused the flip.
    Failed,
    /// The primary could not be read (its mapping failed): nothing was flipped.
    SourceFailed,
}

/// The presenter's state: plain data, owned by the HPD worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presenter {
    ring: Ring,
    registered: bool,
    owed_frame: bool,
    owed_resume: bool,
    last_flip: u64,
    retry_at: u64,
    fails: u8,
    gave_up: bool,
}

impl Presenter {
    pub const fn new(ring_slots: u8) -> Presenter {
        Presenter {
            ring: Ring::new(ring_slots),
            registered: false,
            owed_frame: false,
            owed_resume: false,
            last_flip: 0,
            retry_at: 0,
            fails: 0,
            gave_up: false,
        }
    }

    pub fn registered(&self) -> bool {
        self.registered
    }
    pub fn gave_up(&self) -> bool {
        self.gave_up
    }
    pub fn fails(&self) -> u8 {
        self.fails
    }
    pub fn front(&self) -> Option<u8> {
        self.ring.front()
    }
    pub fn frame_owed(&self) -> bool {
        self.owed_frame
    }

    fn fail(&mut self) {
        self.fails = self.fails.saturating_add(1);
        if self.fails >= MAX_CONSECUTIVE_FAILS {
            self.gave_up = true;
        }
    }

    fn stand_down(&mut self) {
        self.registered = false;
        self.owed_frame = false;
        self.owed_resume = false;
        self.ring.clear();
    }

    /// The next thing to do. Edges are folded in; an act that needs an answer clears
    /// what it consumed, and the answer puts back what did not happen.
    pub fn decide(&mut self, i: Inputs) -> Act {
        self.owed_frame |= i.frame_edge;
        self.owed_resume |= i.resume_edge;
        if self.gave_up {
            return if self.registered {
                self.stand_down();
                Act::Withdraw
            } else {
                Act::Idle
            };
        }
        if !(i.ring_ready && i.source_ok) {
            // The screen shows something this ring cannot copy (a UMD's own image, a
            // mode change in flight, a ring being rebuilt): Venus has it again.
            return if self.registered {
                self.stand_down();
                Act::Withdraw
            } else {
                self.owed_frame = false;
                self.owed_resume = false;
                Act::Idle
            };
        }
        if self.registered && !i.arbiter_has_resident {
            // The arbiter ended our registration (our file closed, the generation
            // went invalid): count it, and do not register again at once.
            self.stand_down();
            self.fail();
            self.retry_at = i.now.saturating_add(REREGISTER_PAUSE_100NS);
            return Act::Idle;
        }
        if !self.registered {
            if i.now < self.retry_at {
                return Act::WaitUntil(self.retry_at);
            }
            // The first frame is always owed; whatever was on screen before it is
            // Venus', not ours.
            self.registered = true;
            self.owed_frame = true;
            self.owed_resume = false;
            self.ring.clear();
            return Act::Register;
        }
        if !i.foreground {
            // A user source holds scanout 0. Frames stay owed; on resume the newest
            // content is copied, or the front surface re-flipped if nothing changed.
            // A failure's pause is moot: the screen is not ours until that source
            // ends, and the first act after it is a fresh one (the strikes stay).
            self.retry_at = 0;
            return Act::Idle;
        }
        if self.owed_frame {
            let mut due = if self.last_flip == 0 {
                0
            } else {
                self.last_flip.saturating_add(MIN_FRAME_INTERVAL_100NS)
            };
            // After a failure: not before the pause, even when 16 ms have passed.
            due = due.max(self.retry_at);
            if i.now < due {
                return Act::WaitUntil(due);
            }
            self.owed_frame = false;
            self.owed_resume = false;
            return Act::CopyFlip {
                slot: self.ring.back(),
            };
        }
        if self.owed_resume {
            if i.now < self.retry_at {
                return Act::WaitUntil(self.retry_at);
            }
            self.owed_resume = false;
            return match self.ring.front() {
                Some(slot) => Act::Reflip { slot },
                // Nothing was ever shown: a frame is what is owed.
                None => {
                    self.owed_frame = true;
                    Act::Idle
                }
            };
        }
        Act::Idle
    }

    /// The answer to [`Act::Register`]: the arbiter took (`true`) or refused the
    /// registration, at `now`. A refusal counts as a failure and the next attempt waits
    /// [`RETRY_AFTER_FAIL_100NS`].
    pub fn registration(&mut self, ok: bool, now: u64) {
        if ok {
            self.retry_at = 0;
        } else {
            self.stand_down();
            self.fail();
            self.retry_at = now.saturating_add(RETRY_AFTER_FAIL_100NS);
        }
    }

    /// The answer to [`Act::CopyFlip`] / [`Act::Reflip`]. `copied` is whether it
    /// was a copy (a failed copy owes a frame, a failed re-flip a resume).
    pub fn flipped(&mut self, slot: u8, copied: bool, result: FlipResult, now: u64) {
        match result {
            FlipResult::Shown => {
                self.ring.commit(slot);
                self.last_flip = now.max(1);
                self.fails = 0;
                self.retry_at = 0;
            }
            FlipResult::Yielded => {
                if copied {
                    self.owed_frame = true;
                } else {
                    self.owed_resume = true;
                }
            }
            FlipResult::Failed | FlipResult::SourceFailed => {
                if copied {
                    self.owed_frame = true;
                } else {
                    self.owed_resume = true;
                }
                self.fail();
                // The next attempt is not before `now + 100 ms`, in whatever pass it
                // would otherwise be made (a retry spacing only the caller's wake
                // enforced let two strikes fall in one pass).
                self.retry_at = now.saturating_add(RETRY_AFTER_FAIL_100NS);
            }
        }
    }

    /// The transport generation changed or the client was forgotten: everything of
    /// the old generation is gone. A new generation gets a fresh chance.
    pub fn reset(&mut self) {
        *self = Presenter::new(self.ring.n);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const MS: u64 = 10_000;

    fn wh(w: u32, h: u32) -> u64 {
        (u64::from(w) << 32) | u64::from(h)
    }
    fn lw(pitch: u32, off: u32) -> u64 {
        (u64::from(pitch) << 32) | u64::from(off)
    }

    // ---- the source ---------------------------------------------------------------

    #[test]
    fn the_published_primary_is_accepted_when_it_is_the_bound_linear_picture() {
        let l =
            source_layout(7, 7, wh(1920, 1080), lw(7680, 0), 7680 * 1080, (1920, 1080)).unwrap();
        assert_eq!(
            l,
            SourceLayout {
                offset: 0,
                pitch: 7680,
                width: 1920,
                height: 1080
            }
        );
        // A plane offset and a padded allocation.
        let l = source_layout(7, 7, wh(1896, 1030), lw(7680, 4096), 1 << 24, (1896, 1030)).unwrap();
        assert_eq!((l.offset, l.pitch), (4096, 7680));
    }

    #[test]
    fn a_primary_that_cannot_be_read_as_a_pitched_picture_is_refused() {
        let ok = |a, p, w, l, s, r| source_layout(a, p, w, l, s, r);
        let w = wh(1920, 1080);
        assert_eq!(
            ok(0, 7, w, lw(7680, 0), 1 << 24, (1920, 1080)),
            Err(SourceRefusal::NoSource)
        );
        assert_eq!(
            ok(7, 0, w, lw(7680, 0), 1 << 24, (1920, 1080)),
            Err(SourceRefusal::NoSource)
        );
        assert_eq!(
            ok(7, 8, w, lw(7680, 0), 1 << 24, (1920, 1080)),
            Err(SourceRefusal::NotThePrimary),
            "a UMD's own image is bound"
        );
        assert_eq!(
            ok(7, 7, w, lw(7680, 0), 1 << 24, (1280, 720)),
            Err(SourceRefusal::ExtentMismatch)
        );
        assert_eq!(
            ok(7, 7, wh(63, 1080), lw(7680, 0), 1 << 24, (63, 1080)),
            Err(SourceRefusal::ExtentMismatch),
            "outside the range every flip accepts"
        );
        assert_eq!(
            ok(7, 7, w, lw(1920 * 4 - 4, 0), 1 << 24, (1920, 1080)),
            Err(SourceRefusal::BadPitch)
        );
        assert_eq!(
            ok(7, 7, w, lw((1 << 20) + 4, 0), u64::MAX, (1920, 1080)),
            Err(SourceRefusal::BadPitch)
        );
        assert_eq!(
            ok(7, 7, w, lw(7680, 0), 7680 * 1079, (1920, 1080)),
            Err(SourceRefusal::TooSmall)
        );
        // The last row needs only its pixels, not a whole pitch.
        assert!(ok(7, 7, w, lw(7680, 0), 7680 * 1079 + 1920 * 4, (1920, 1080)).is_ok());
        assert_eq!(
            ok(7, 7, w, lw(7680, 0xffff_ffff), 1 << 24, (1920, 1080)),
            Err(SourceRefusal::TooSmall)
        );
    }

    // ---- the copy -----------------------------------------------------------------

    fn src(w: u32, h: u32, pitch: u32, off: u64) -> SourceLayout {
        SourceLayout {
            offset: off,
            pitch,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_copy_plan_checks_every_bound_once() {
        let s = src(100, 10, 512, 64);
        let src_len = 64 + 512 * 9 + 400;
        let p = CopyPlan::new(&s, src_len, 448, 448 * 9 + 400, 0, 10).unwrap();
        assert_eq!(p.row_bytes, 400);
        assert_eq!(p.row(0), Some((64, 0)));
        assert_eq!(p.row(9), Some((64 + 512 * 9, 448 * 9)));
        assert_eq!(p.row(10), None);
        assert_eq!(p.bytes(), 4000);
        assert_eq!(p.dst_span(), (0, 448 * 9 + 400));
        // One byte short on either side.
        assert!(CopyPlan::new(&s, src_len - 1, 448, 448 * 9 + 400, 0, 10).is_none());
        assert!(CopyPlan::new(&s, src_len, 448, 448 * 9 + 399, 0, 10).is_none());
        // A destination pitch that cannot hold a row, an empty or reversed range, rows
        // beyond the picture.
        assert!(CopyPlan::new(&s, src_len, 399, 1 << 20, 0, 10).is_none());
        assert!(CopyPlan::new(&s, src_len, 448, 1 << 20, 5, 5).is_none());
        assert!(CopyPlan::new(&s, src_len, 448, 1 << 20, 6, 5).is_none());
        assert!(CopyPlan::new(&s, src_len, 448, 1 << 20, 0, 11).is_none());
        // Band copies: the rows asked for, and only those.
        let b = CopyPlan::new(&s, src_len, 448, 448 * 9 + 400, 3, 5).unwrap();
        assert_eq!(b.row(2), None);
        assert_eq!(b.row(3), Some((64 + 512 * 3, 448 * 3)));
        assert_eq!(b.row(4), Some((64 + 512 * 4, 448 * 4)));
        assert_eq!(b.row(5), None);
        assert_eq!(b.dst_span(), (448 * 3, 448 * 4 + 400));
        // Overflow does not wrap.
        let big = src(100, 16_000, u32::MAX, u64::MAX - 10);
        assert!(CopyPlan::new(&big, u64::MAX, 400, u64::MAX, 0, 16_000).is_none());
    }

    #[test]
    fn executing_a_plan_moves_the_picture_and_nothing_else() {
        // Source pitch 24 (padding 0xEE), destination pitch 32 (guard 0xCC), 5 px wide.
        let (w, h) = (5u32, 4u32);
        let mut srcmem = [0xEEu8; 8 + 24 * 4];
        for y in 0..h {
            for x in 0..w {
                let at = 8 + (y * 24 + x * 4) as usize;
                srcmem[at..at + 4].copy_from_slice(&((y << 8) | x).to_le_bytes());
            }
        }
        let mut dst = [0xCCu8; 32 * 4];
        let s = src(w, h, 24, 8);
        let plan = CopyPlan::new(&s, srcmem.len() as u64, 32, dst.len() as u64, 0, h).unwrap();
        for y in plan.y0..plan.y1 {
            let (so, d) = plan.row(y).unwrap();
            let n = plan.row_bytes as usize;
            dst[d as usize..d as usize + n].copy_from_slice(&srcmem[so as usize..so as usize + n]);
        }
        for y in 0..h {
            for x in 0..w {
                let at = (y * 32 + x * 4) as usize;
                assert_eq!(
                    u32::from_le_bytes(dst[at..at + 4].try_into().unwrap()),
                    (y << 8) | x
                );
            }
            // The row's tail was not touched.
            assert!(dst[(y * 32 + 20) as usize..(y * 32 + 32) as usize]
                .iter()
                .all(|b| *b == 0xCC));
        }
    }

    // ---- the ring -----------------------------------------------------------------

    #[test]
    fn the_ring_never_writes_the_surface_shown_last() {
        let mut r = Ring::new(2);
        assert_eq!(r.front(), None);
        assert_eq!(r.back(), 0);
        r.commit(0);
        assert_eq!(r.back(), 1);
        r.commit(1);
        assert_eq!(r.back(), 0);
        r.commit(0);
        assert_eq!(r.back(), 1);
        r.clear();
        assert_eq!(r.back(), 0);
        // Out of range is ignored; one surface can only rewrite itself.
        r.commit(5);
        assert_eq!(r.front(), None);
        let mut one = Ring::new(1);
        one.commit(0);
        assert_eq!(one.back(), 0);
        // Three surfaces rotate.
        let mut three = Ring::new(3);
        let mut seen = Vec::new();
        for _ in 0..6 {
            let b = three.back();
            seen.push(b);
            three.commit(b);
        }
        assert_eq!(seen, [0, 1, 2, 0, 1, 2]);
    }

    // ---- the presenter ------------------------------------------------------------

    fn inp(now: u64) -> Inputs {
        Inputs {
            now,
            ring_ready: true,
            source_ok: true,
            arbiter_has_resident: true,
            foreground: true,
            frame_edge: false,
            resume_edge: false,
        }
    }

    /// A presenter that is registered, foreground and has shown slot 0 at `t`.
    fn shown(t: u64) -> Presenter {
        let mut p = Presenter::new(2);
        assert_eq!(p.decide(inp(t)), Act::Register);
        p.registration(true, t);
        assert_eq!(p.decide(inp(t)), Act::CopyFlip { slot: 0 });
        p.flipped(0, true, FlipResult::Shown, t);
        p
    }

    #[test]
    fn nothing_happens_until_the_ring_and_the_source_are_both_ready() {
        let mut p = Presenter::new(2);
        for (ring, src) in [(false, false), (true, false), (false, true)] {
            let mut i = inp(1);
            i.ring_ready = ring;
            i.source_ok = src;
            i.frame_edge = true;
            assert_eq!(p.decide(i), Act::Idle);
            assert!(!p.registered());
            assert!(
                !p.frame_owed(),
                "an edge with nothing to show is not owed later"
            );
        }
    }

    #[test]
    fn the_first_pass_registers_and_the_first_frame_is_always_owed() {
        let mut p = Presenter::new(2);
        assert_eq!(p.decide(inp(1)), Act::Register);
        assert!(p.registered());
        p.registration(true, 1);
        // No edge at all: the first frame is owed anyway (the screen shows Venus').
        assert_eq!(p.decide(inp(1)), Act::CopyFlip { slot: 0 });
        p.flipped(0, true, FlipResult::Shown, 1);
        assert_eq!(p.front(), Some(0));
        assert_eq!(p.decide(inp(2)), Act::Idle);
    }

    #[test]
    fn frames_alternate_surfaces_and_are_paced() {
        let mut p = shown(10 * MS);
        let mut i = inp(11 * MS);
        i.frame_edge = true;
        // Too soon: the frame stays owed and a wake is asked for at the due time.
        assert_eq!(
            p.decide(i),
            Act::WaitUntil(10 * MS + MIN_FRAME_INTERVAL_100NS)
        );
        assert!(p.frame_owed());
        // Edges while waiting coalesce into the one owed frame.
        let mut i = inp(12 * MS);
        i.frame_edge = true;
        assert!(matches!(p.decide(i), Act::WaitUntil(_)));
        let due = 10 * MS + MIN_FRAME_INTERVAL_100NS;
        assert_eq!(p.decide(inp(due)), Act::CopyFlip { slot: 1 });
        assert!(!p.frame_owed());
        p.flipped(1, true, FlipResult::Shown, due);
        let mut i = inp(due + MIN_FRAME_INTERVAL_100NS);
        i.frame_edge = true;
        assert_eq!(p.decide(i), Act::CopyFlip { slot: 0 });
    }

    #[test]
    fn a_user_source_makes_the_ring_yield_and_resume_shows_the_newest_content() {
        let mut p = shown(10 * MS);
        // A user source takes scanout 0. Desktop edges keep coming and are only owed.
        let mut i = inp(100 * MS);
        i.foreground = false;
        i.frame_edge = true;
        assert_eq!(p.decide(i), Act::Idle);
        assert!(p.frame_owed());
        let mut i = inp(200 * MS);
        i.foreground = false;
        assert_eq!(p.decide(i), Act::Idle);
        // It ends: the resident source is back and the desktop changed meanwhile, so the
        // resume is a COPY (of the newest content), into the surface not shown last.
        let mut i = inp(300 * MS);
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::CopyFlip { slot: 1 });
        p.flipped(1, true, FlipResult::Shown, 300 * MS);
        assert_eq!(
            p.decide(inp(301 * MS)),
            Act::Idle,
            "the resume was answered once"
        );
    }

    #[test]
    fn a_resume_with_nothing_changed_is_a_re_flip_of_the_front_surface() {
        let mut p = shown(10 * MS);
        let mut i = inp(500 * MS);
        i.foreground = false;
        assert_eq!(p.decide(i), Act::Idle);
        let mut i = inp(600 * MS);
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::Reflip { slot: 0 });
        p.flipped(0, false, FlipResult::Shown, 600 * MS);
        assert_eq!(p.front(), Some(0));
        assert_eq!(p.decide(inp(601 * MS)), Act::Idle);
        // A re-flip is not paced: it answers a user source's end, not a desktop edge.
        let mut i = inp(601 * MS + 1);
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::Reflip { slot: 0 });
    }

    #[test]
    fn a_flip_that_found_the_source_yielded_keeps_what_was_owed() {
        let mut p = shown(10 * MS);
        let mut i = inp(100 * MS);
        i.frame_edge = true;
        let Act::CopyFlip { slot } = p.decide(i) else {
            panic!("a frame was due")
        };
        // The user source took scanout 0 between the decision and the flip.
        p.flipped(slot, true, FlipResult::Yielded, 100 * MS);
        assert!(p.frame_owed());
        assert_eq!(p.fails(), 0, "yielding is not a failure");
        assert_eq!(p.front(), Some(0), "nothing new was shown");
        // A re-flip that yielded owes the resume again.
        let mut i = inp(200 * MS);
        i.foreground = true;
        // (the owed frame is copied first)
        assert!(matches!(p.decide(i), Act::CopyFlip { .. }));
        p.flipped(1, true, FlipResult::Shown, 200 * MS);
        let mut i = inp(300 * MS);
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::Reflip { slot: 1 });
        p.flipped(1, false, FlipResult::Yielded, 300 * MS);
        assert_eq!(p.decide(inp(301 * MS)), Act::Reflip { slot: 1 });
    }

    #[test]
    fn three_failures_in_a_row_give_the_desktop_back_to_venus_for_the_generation() {
        let mut p = shown(10 * MS);
        let mut now = 100 * MS;
        for n in 1..=MAX_CONSECUTIVE_FAILS {
            let mut i = inp(now);
            i.frame_edge = true;
            let Act::CopyFlip { slot } = p.decide(i) else {
                panic!("a frame was due at failure {n}")
            };
            p.flipped(slot, true, FlipResult::Failed, now);
            assert_eq!(p.fails(), n);
            now += 100 * MS;
        }
        assert!(p.gave_up());
        // The next pass withdraws the registration (the desktop flush returns) ...
        assert_eq!(p.decide(inp(now)), Act::Withdraw);
        assert!(!p.registered());
        // ... and after that the presenter does nothing, whatever arrives.
        let mut i = inp(now + 1);
        i.frame_edge = true;
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::Idle);
        // A new transport generation starts clean.
        p.reset();
        assert!(!p.gave_up());
        assert_eq!(p.decide(inp(now + 2)), Act::Register);
    }

    #[test]
    fn a_failed_frame_is_not_retried_before_the_pause_whatever_the_pass_structure() {
        let mut p = shown(10 * MS);
        let t = 500 * MS;
        let mut i = inp(t);
        i.frame_edge = true;
        let Act::CopyFlip { slot } = p.decide(i) else {
            panic!("a frame was due")
        };
        p.flipped(slot, true, FlipResult::SourceFailed, t);
        assert_eq!(p.fails(), 1);
        // The second `service` call of the same worker pass (same instant), and later
        // ones inside the pause, are all told when to come back: no second strike.
        let back = t + RETRY_AFTER_FAIL_100NS;
        for dt in [0, 1, 20 * MS, 99 * MS] {
            assert_eq!(p.decide(inp(t + dt)), Act::WaitUntil(back), "dt {dt}");
        }
        assert!(p.frame_owed());
        assert_eq!(p.fails(), 1);
        // At the pause's end the frame is attempted again.
        assert!(matches!(p.decide(inp(back)), Act::CopyFlip { .. }));
    }

    #[test]
    fn three_strikes_cannot_be_spent_in_one_pass() {
        // A pass is "decide, perform, answer" up to three times at one instant, the second
        // `service` call of the pass doing the same again: six acts at `t`.
        let mut p = shown(10 * MS);
        let t = 500 * MS;
        let mut strikes = 0;
        for n in 0..6 {
            let mut i = inp(t);
            i.frame_edge = n == 0;
            if let Act::CopyFlip { slot } = p.decide(i) {
                p.flipped(slot, true, FlipResult::Failed, t);
                strikes += 1;
            }
        }
        assert_eq!(strikes, 1);
        assert_eq!(p.fails(), 1);
        assert!(!p.gave_up());
    }

    #[test]
    fn a_failed_resume_re_flip_waits_too_and_a_refused_registration_waits() {
        let mut p = shown(10 * MS);
        let t = 700 * MS;
        let mut i = inp(t);
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::Reflip { slot: 0 });
        p.flipped(0, false, FlipResult::Failed, t);
        assert_eq!(
            p.decide(inp(t + MS)),
            Act::WaitUntil(t + RETRY_AFTER_FAIL_100NS)
        );
        assert_eq!(
            p.decide(inp(t + RETRY_AFTER_FAIL_100NS)),
            Act::Reflip { slot: 0 }
        );
        // A refused registration: the next attempt is a pause away, not the next act.
        let mut q = Presenter::new(2);
        assert_eq!(q.decide(inp(t)), Act::Register);
        q.registration(false, t);
        assert_eq!(q.decide(inp(t)), Act::WaitUntil(t + RETRY_AFTER_FAIL_100NS));
        assert_eq!(q.fails(), 1);
        assert_eq!(q.decide(inp(t + RETRY_AFTER_FAIL_100NS)), Act::Register);
    }

    #[test]
    fn a_yield_is_not_delayed_and_a_user_source_voids_a_failures_pause() {
        let mut p = shown(10 * MS);
        let t = 500 * MS;
        let mut i = inp(t);
        i.frame_edge = true;
        let Act::CopyFlip { slot } = p.decide(i) else {
            panic!()
        };
        p.flipped(slot, true, FlipResult::Yielded, t);
        assert!(matches!(p.decide(inp(t + 1)), Act::CopyFlip { .. }));
        // A failure, then a user source takes the screen and ends within the pause: the
        // resume is not held back by the pause (the strike stays counted).
        let mut p = shown(10 * MS);
        let mut i = inp(t);
        i.frame_edge = true;
        let Act::CopyFlip { slot } = p.decide(i) else {
            panic!()
        };
        p.flipped(slot, true, FlipResult::Failed, t);
        let mut i = inp(t + 10 * MS);
        i.foreground = false;
        assert_eq!(p.decide(i), Act::Idle);
        let mut i = inp(t + 20 * MS);
        i.resume_edge = true;
        assert!(matches!(p.decide(i), Act::CopyFlip { .. }));
        assert_eq!(p.fails(), 1);
    }

    #[test]
    fn a_shown_frame_forgives_earlier_failures() {
        let mut p = shown(10 * MS);
        let mut i = inp(100 * MS);
        i.frame_edge = true;
        let Act::CopyFlip { slot } = p.decide(i) else {
            panic!()
        };
        p.flipped(slot, true, FlipResult::SourceFailed, 100 * MS);
        assert_eq!(p.fails(), 1);
        assert!(p.frame_owed(), "the frame is still owed");
        let Act::CopyFlip { slot } = p.decide(inp(200 * MS)) else {
            panic!()
        };
        p.flipped(slot, true, FlipResult::Shown, 200 * MS);
        assert_eq!(p.fails(), 0);
    }

    #[test]
    fn losing_the_source_withdraws_and_a_later_return_starts_again_with_a_frame() {
        let mut p = shown(10 * MS);
        // The screen shows a UMD's image now: not the primary.
        let mut i = inp(100 * MS);
        i.source_ok = false;
        assert_eq!(p.decide(i), Act::Withdraw);
        assert!(!p.registered());
        assert_eq!(
            p.front(),
            None,
            "the front surface is not on screen any more"
        );
        i.frame_edge = true;
        assert_eq!(p.decide(i), Act::Idle);
        // The primary is on screen again.
        assert_eq!(p.decide(inp(200 * MS)), Act::Register);
        p.registration(true, 200 * MS);
        assert_eq!(p.decide(inp(200 * MS)), Act::CopyFlip { slot: 0 });
    }

    #[test]
    fn a_mode_change_that_unreadies_the_ring_withdraws_too() {
        let mut p = shown(10 * MS);
        let mut i = inp(100 * MS);
        i.ring_ready = false;
        assert_eq!(p.decide(i), Act::Withdraw);
        assert_eq!(p.decide(i), Act::Idle);
    }

    #[test]
    fn a_registration_the_arbiter_ended_is_counted_and_retried_after_a_pause() {
        let mut p = shown(10 * MS);
        let mut i = inp(100 * MS);
        i.arbiter_has_resident = false;
        assert_eq!(p.decide(i), Act::Idle);
        assert!(!p.registered());
        assert_eq!(p.fails(), 1);
        // Not at once: a wake at the retry time is asked for.
        let i2 = inp(100 * MS + 1);
        assert_eq!(
            p.decide(i2),
            Act::WaitUntil(100 * MS + REREGISTER_PAUSE_100NS)
        );
        assert_eq!(
            p.decide(inp(100 * MS + REREGISTER_PAUSE_100NS)),
            Act::Register
        );
    }

    #[test]
    fn a_refused_registration_counts_and_three_of_them_end_it() {
        let mut p = Presenter::new(2);
        let mut now = MS;
        for _ in 0..MAX_CONSECUTIVE_FAILS {
            assert_eq!(p.decide(inp(now)), Act::Register);
            p.registration(false, now);
            assert!(!p.registered());
            now += RETRY_AFTER_FAIL_100NS;
        }
        assert!(p.gave_up());
        assert_eq!(p.decide(inp(now)), Act::Idle);
    }

    #[test]
    fn registering_behind_a_user_source_waits_without_copying() {
        let mut p = Presenter::new(2);
        let mut i = inp(MS);
        i.foreground = false;
        assert_eq!(p.decide(i), Act::Register);
        p.registration(true, MS);
        assert_eq!(
            p.decide(i),
            Act::Idle,
            "parked: nothing is copied for nobody"
        );
        assert!(p.frame_owed());
        i.foreground = true;
        i.resume_edge = true;
        assert_eq!(p.decide(i), Act::CopyFlip { slot: 0 });
    }

    #[test]
    fn frame_pacing_is_sixty_hertz_and_a_240_hz_edge_stream_coalesces() {
        // 240 edges a second for one second: about 60 flips, never more than due.
        let mut p = shown(MS);
        let period = 10_000_000 / 240;
        let mut flips = 0;
        let mut now = MS;
        for _ in 0..240 {
            now += period;
            let mut i = inp(now);
            i.frame_edge = true;
            if let Act::CopyFlip { slot } = p.decide(i) {
                p.flipped(slot, true, FlipResult::Shown, now);
                flips += 1;
            }
        }
        assert!((50..=61).contains(&flips), "flips {flips}");
    }

    // ---- the presenter against the real arbiter -------------------------------------------

    mod cosim {
        use super::*;
        use crate::foreign_scanout::{ForeignScanout, Layout, PresentError, FOURCC_XRGB8888};

        const K: u64 = u64::MAX;
        const KH: u32 = 3;
        const USER: u64 = 0xA000;

        fn layout() -> Layout {
            Layout {
                width: 1920,
                height: 1080,
                stride: 7680,
                offset: 0,
                fourcc: FOURCC_XRGB8888,
                modifier: 0,
            }
        }

        struct Rig {
            arb: ForeignScanout,
            p: Presenter,
            now: u64,
            flips: Vec<(&'static str, u8, u64)>,
        }

        impl Rig {
            fn new() -> Rig {
                Rig {
                    arb: ForeignScanout::new(),
                    p: Presenter::new(2),
                    now: 100 * MS,
                    flips: Vec::new(),
                }
            }

            /// One worker pass, as `rm_present::service` does it: decide, perform, answer.
            fn pass(&mut self, frame_edge: bool, resume_edge: bool) -> Vec<Act> {
                let mut acts = Vec::new();
                let (mut fe, mut re) = (frame_edge, resume_edge);
                for _ in 0..3 {
                    let i = Inputs {
                        now: self.now,
                        ring_ready: true,
                        source_ok: true,
                        arbiter_has_resident: self.arb.resident().is_some(),
                        foreground: self.arb.resident_foreground(),
                        frame_edge: fe,
                        resume_edge: re,
                    };
                    fe = false;
                    re = false;
                    let act = self.p.decide(i);
                    acts.push(act);
                    match act {
                        Act::Idle | Act::WaitUntil(_) => break,
                        Act::Register => {
                            let ok = self.arb.resident_set(K, KH, 3, layout(), self.now).is_ok();
                            self.p.registration(ok, self.now);
                        }
                        Act::Withdraw => {
                            self.arb.resident_drop();
                            break;
                        }
                        Act::CopyFlip { slot } | Act::Reflip { slot } => {
                            let copied = matches!(act, Act::CopyFlip { .. });
                            let result = match self.arb.present(K, KH, self.now) {
                                Ok(f) => {
                                    self.arb.extend(f.generation, self.now);
                                    self.flips.push((
                                        if copied { "copy" } else { "reflip" },
                                        slot,
                                        f.seq,
                                    ));
                                    FlipResult::Shown
                                }
                                Err(PresentError::NoSource | PresentError::Lapsed) => {
                                    FlipResult::Yielded
                                }
                            };
                            self.p.flipped(slot, copied, result, self.now);
                        }
                    }
                }
                acts
            }

            fn advance(&mut self, ms: u64) {
                self.now += ms * MS;
            }

            /// What the adapter does at the end of a source: ask the driver for a resume.
            fn resume_edge(&mut self) -> bool {
                self.arb.take_resume_owed()
            }
        }

        #[test]
        fn a_whole_session_register_show_yield_resume_and_the_desktop_never_flushes() {
            let mut r = Rig::new();
            assert_eq!(
                r.pass(false, false)[..2],
                [Act::Register, Act::CopyFlip { slot: 0 }]
            );
            assert!(r.arb.suppress_desktop(r.now).is_some_and(|a| a.resident));
            // Desktop edges: paced frames alternating surfaces.
            r.advance(20);
            assert_eq!(r.pass(true, false), [Act::CopyFlip { slot: 1 }, Act::Idle]);
            r.advance(20);
            assert_eq!(r.pass(true, false), [Act::CopyFlip { slot: 0 }, Act::Idle]);
            // A user-mode game takes scanout 0 and flips its own frames.
            r.advance(20);
            r.arb.set(USER, 9, 3, layout(), 0, r.now).unwrap();
            assert_eq!(r.pass(false, false), [Act::Idle]);
            assert!(r.arb.present(USER, 9, r.now).is_ok());
            // The desktop kept changing under it (the gate raised the flag only).
            r.advance(500);
            assert_eq!(r.pass(true, false), [Act::Idle], "yielded, frame owed");
            assert_eq!(
                r.flips.len(),
                3,
                "nothing of the KMD's reached the host meanwhile"
            );
            // The game exits: its source is released; the resident one is back.
            r.arb.release(USER, Some(9));
            assert!(!r.arb.restore_pending(), "no Venus desktop flush is owed");
            let resume = r.resume_edge();
            assert!(resume);
            // The desktop changed during the tenure, so the resume carries NEW content,
            // into the surface that was not shown last.
            let acts = r.pass(false, resume);
            assert_eq!(acts[0], Act::CopyFlip { slot: 1 }, "{acts:?}");
            // seq 4 was the game's flip: the host never sees it go back.
            assert_eq!(*r.flips.last().unwrap(), ("copy", 1, 5));
            assert!(r.arb.resident_foreground());
        }

        #[test]
        fn a_resume_after_a_quiet_tenure_re_flips_the_front_surface_unchanged() {
            let mut r = Rig::new();
            r.pass(false, false);
            r.advance(20);
            r.pass(true, false);
            assert_eq!(r.p.front(), Some(1));
            r.arb.set(USER, 9, 3, layout(), 0, r.now).unwrap();
            r.pass(false, false);
            r.advance(300);
            r.arb.release(USER, None);
            let resume = r.resume_edge();
            let acts = r.pass(false, resume);
            assert_eq!(acts[0], Act::Reflip { slot: 1 }, "{acts:?}");
            assert_eq!(r.flips.last().unwrap().0, "reflip");
            // And nothing is owed after it.
            assert_eq!(r.pass(false, false), [Act::Idle]);
        }

        #[test]
        fn a_user_source_that_goes_silent_hands_the_screen_back_by_its_lapse() {
            let mut r = Rig::new();
            r.pass(false, false);
            r.arb.set(USER, 9, 3, layout(), 0, r.now).unwrap();
            r.pass(false, false);
            // The default lapse passes with no flip from the user.
            r.advance(2_100);
            assert!(matches!(
                r.arb.poll(r.now),
                crate::foreign_scanout::Poll::Lapsed { .. }
            ));
            let resume = r.resume_edge();
            assert!(resume);
            let acts = r.pass(false, resume);
            assert_eq!(acts[0], Act::Reflip { slot: 0 }, "{acts:?}");
        }

        #[test]
        fn the_resident_files_closure_is_counted_and_registration_waits_then_retries() {
            let mut r = Rig::new();
            r.pass(false, false);
            assert!(r.arb.release_handle(K, KH), "its DRM file was closed");
            assert!(
                r.arb.restore_pending(),
                "the desktop is owed one Venus flush"
            );
            // The presenter sees its registration gone: a failure, then a pause.
            assert_eq!(r.pass(false, false), [Act::Idle]);
            assert_eq!(r.p.fails(), 1);
            assert!(matches!(r.pass(false, false)[0], Act::WaitUntil(_)));
            r.advance(200);
            let acts = r.pass(false, false);
            assert_eq!(acts[..2], [Act::Register, Act::CopyFlip { slot: 0 }]);
            assert_eq!(r.p.fails(), 0, "a shown frame forgives it");
            assert!(r.arb.resident_foreground());
        }

        #[test]
        fn withdrawing_gives_the_desktop_back_exactly_once() {
            let mut r = Rig::new();
            r.pass(false, false);
            // The screen now shows a UMD's image: the presenter withdraws.
            let i = Inputs {
                now: r.now,
                ring_ready: true,
                source_ok: false,
                arbiter_has_resident: true,
                foreground: true,
                frame_edge: false,
                resume_edge: false,
            };
            assert_eq!(r.p.decide(i), Act::Withdraw);
            assert_eq!(
                r.arb.resident_drop(),
                crate::foreign_scanout::ResidentDrop::Ended
            );
            assert!(r.arb.restore_pending());
            assert!(r.arb.desktop_restored());
            assert!(
                r.arb.suppress_desktop(r.now).is_none(),
                "Venus flushes again"
            );
            assert_eq!(
                r.arb.resident_drop(),
                crate::foreign_scanout::ResidentDrop::None
            );
        }

        #[test]
        fn seq_never_goes_back_across_the_resident_and_user_sources() {
            let mut r = Rig::new();
            r.pass(false, false);
            r.advance(20);
            r.pass(true, false);
            r.arb.set(USER, 9, 3, layout(), 0, r.now).unwrap();
            let u1 = r.arb.present(USER, 9, r.now).unwrap().seq;
            let u2 = r.arb.present(USER, 9, r.now).unwrap().seq;
            r.arb.release(USER, None);
            let resume = r.resume_edge();
            r.pass(false, resume);
            let mut seqs: Vec<u64> = r.flips.iter().map(|f| f.2).collect();
            seqs.insert(2, u1);
            seqs.insert(3, u2);
            assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        }
    }

    // ---- the arbiter, the fenced-present queue and the presenter, together -------------
    //
    // A model of what `adapter/foreign_scanout.rs` and `virtio/rm_present.rs` do with the
    // three pure state machines (`ForeignScanout`, `ScanoutQueue`, `Presenter`): the same
    // calls in the same order, with a wire that records every flip that reached the host
    // and a fence table that counts closes. The section 13.12 state machine of
    // `docs/kmd-rm-client.md` is what these tests pin.

    mod combined {
        use super::*;
        use crate::foreign_scanout::{ForeignScanout, Layout, Poll, PresentError, ReleaseOutcome};
        use crate::foreign_scanout::{ResidentDrop, FOURCC_XRGB8888};
        use crate::rm_fence_present::{QEntry, ScanoutQueue};
        use std::collections::BTreeMap;

        const K: u64 = u64::MAX;
        const KH: u32 = 3;
        const U1: u64 = 0xA000;
        const U2: u64 = 0xB000;
        const UH: u32 = 9;
        const EPOCH: u64 = 3;

        fn layout() -> Layout {
            Layout {
                width: 1920,
                height: 1080,
                stride: 7680,
                offset: 0,
                fourcc: FOURCC_XRGB8888,
                modifier: 0,
            }
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Who {
            Kmd,
            User,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum End {
            Release,
            CloseFile,
            OwnerExit,
            Lapse,
            Reset,
        }

        struct World {
            arb: ForeignScanout,
            q: ScanoutQueue,
            p: Presenter,
            now: u64,
            /// Every flip that reached the host, in arrival order: (who, kind, seq).
            wire: Vec<(Who, &'static str, u64)>,
            /// Fence handles the KMD took over (attached), and whether each has fired.
            fired: BTreeMap<u32, bool>,
            /// How often the KMD asked for each attached fence to be closed.
            closes: BTreeMap<u32, u32>,
            frame_edge: bool,
            resume_edge: bool,
            refresh_requested: bool,
            /// Desktop flushes that reached Venus.
            venus_flushes: u32,
        }

        impl World {
            fn new() -> World {
                World {
                    arb: ForeignScanout::new(),
                    q: ScanoutQueue::new(),
                    p: Presenter::new(2),
                    now: 100 * MS,
                    wire: Vec::new(),
                    fired: BTreeMap::new(),
                    closes: BTreeMap::new(),
                    frame_edge: false,
                    resume_edge: false,
                    refresh_requested: false,
                    venus_flushes: 0,
                }
            }

            fn advance(&mut self, ms: u64) {
                self.now += ms * MS;
            }

            /// `foreign_scanout_restore_desktop`: a re-flip of the resident surface when it
            /// took the screen back, else one desktop refresh.
            fn restore(&mut self) {
                if self.arb.take_resume_owed() {
                    self.resume_edge = true;
                    return;
                }
                self.refresh_requested = true;
            }

            /// The display worker's refresh arm: the gate asks the arbiter.
            fn refresh(&mut self) {
                if !self.refresh_requested {
                    return;
                }
                self.refresh_requested = false;
                match self.arb.suppress_desktop(self.now) {
                    // Resident on screen: the withheld flush is a frame to copy. A user
                    // source on screen: a flag the presenter folds in for later.
                    Some(_) => self.frame_edge = true,
                    None => {
                        self.venus_flushes += 1;
                        self.arb.desktop_restored();
                    }
                }
            }

            /// The desktop changed (the gate saw a flush it had to withhold).
            fn desktop_changed(&mut self) {
                self.refresh_requested = true;
                self.refresh();
            }

            /// `foreign_scanout_flip_done`.
            fn flip_done(&mut self, generation: u32) {
                self.arb.extend(generation, self.now);
                let live = self
                    .arb
                    .suppress_desktop(self.now)
                    .is_some_and(|a| a.generation == generation);
                if !live {
                    self.restore();
                }
            }

            /// `foreign_fence_pump`, one drain.
            fn drain_once(&mut self) -> Option<QEntry> {
                let live = self
                    .arb
                    .suppress_desktop(self.now)
                    .map(|a| (a.generation, a.epoch));
                let fired = self.fired.clone();
                let d = self
                    .q
                    .drain(live, |f| fired.get(&f).copied().unwrap_or(true));
                for &c in d.closes() {
                    *self.closes.entry(c).or_default() += 1;
                }
                d.send
            }

            fn send_entry(&mut self, e: QEntry) {
                let who = if e.flip.handle == KH {
                    Who::Kmd
                } else {
                    Who::User
                };
                self.wire.push((who, "queued", e.flip.seq));
                self.flip_done(e.flip.generation);
            }

            fn pump(&mut self) {
                while let Some(e) = self.drain_once() {
                    self.send_entry(e);
                }
            }

            fn user_set(&mut self, owner: u64) -> Result<crate::foreign_scanout::SetOutcome, ()> {
                self.arb
                    .set(owner, UH, EPOCH, layout(), 0, self.now)
                    .map_err(|_| ())
            }

            /// `present_fenced`: mint, queue behind `fence`, pump.
            fn user_present_fenced(&mut self, owner: u64, fence: u32) -> Result<u64, PresentError> {
                let flip = match self.arb.present(owner, UH, self.now) {
                    Ok(f) => f,
                    Err(e) => {
                        if e == PresentError::Lapsed {
                            self.restore();
                        }
                        return Err(e);
                    }
                };
                self.fired.entry(fence).or_insert(false);
                self.closes.entry(fence).or_insert(0);
                self.q
                    .push(QEntry {
                        flip,
                        gem: 100 + fence,
                        fence,
                    })
                    .unwrap();
                self.pump();
                Ok(flip.seq)
            }

            fn fire(&mut self, fence: u32) {
                self.fired.insert(fence, true);
            }

            /// `rm_present::service` (one call).
            fn present_pass(&mut self) {
                let (mut fe, mut re) = (
                    core::mem::take(&mut self.frame_edge),
                    core::mem::take(&mut self.resume_edge),
                );
                for _ in 0..3 {
                    let i = Inputs {
                        now: self.now,
                        ring_ready: true,
                        source_ok: true,
                        arbiter_has_resident: self.arb.resident().is_some(),
                        foreground: self.arb.resident_foreground(),
                        frame_edge: fe,
                        resume_edge: re,
                    };
                    fe = false;
                    re = false;
                    match self.p.decide(i) {
                        Act::Idle | Act::WaitUntil(_) => break,
                        Act::Register => {
                            let ok = self
                                .arb
                                .resident_set(K, KH, EPOCH, layout(), self.now)
                                .is_ok();
                            self.p.registration(ok, self.now);
                        }
                        Act::Withdraw => {
                            if self.arb.resident_drop() == ResidentDrop::Ended {
                                self.restore();
                            }
                            break;
                        }
                        act @ (Act::CopyFlip { .. } | Act::Reflip { .. }) => {
                            let (slot, copied) = match act {
                                Act::CopyFlip { slot } => (slot, true),
                                Act::Reflip { slot } => (slot, false),
                                _ => unreachable!(),
                            };
                            let result = match self.arb.present(K, KH, self.now) {
                                Ok(f) => {
                                    self.wire.push((
                                        Who::Kmd,
                                        if copied { "copy" } else { "reflip" },
                                        f.seq,
                                    ));
                                    // `present_within` sends direct, then `flip_done`.
                                    self.flip_done(f.generation);
                                    FlipResult::Shown
                                }
                                Err(_) => FlipResult::Yielded,
                            };
                            self.p.flipped(slot, copied, result, self.now);
                        }
                    }
                }
            }

            /// One HPD worker pass, in the worker's order: the display refresh arm, the
            /// lapse poll, the fenced queue, the RM client's presenter.
            fn pass(&mut self) {
                self.refresh();
                if let Poll::Lapsed { .. } = self.arb.poll(self.now) {
                    self.restore();
                }
                self.pump();
                self.present_pass();
            }

            fn settle(&mut self) {
                for _ in 0..4 {
                    self.advance(20);
                    self.pass();
                }
            }

            fn end(&mut self, how: End) {
                match how {
                    End::Release => {
                        if let ReleaseOutcome::Released { .. } = self.arb.release(U1, Some(UH)) {
                            self.restore();
                        }
                    }
                    End::CloseFile => {
                        if self.arb.release_handle(U1, UH) {
                            self.restore();
                        }
                    }
                    End::OwnerExit => {
                        if self.arb.release_owner(U1) {
                            self.restore();
                        }
                    }
                    End::Lapse => self.advance(2_100),
                    End::Reset => {
                        // `foreign_scanout_reset`, then the transport sweep closes whatever
                        // the queue still held (`close_all_on_host`).
                        self.arb.reset();
                        self.q.clear();
                        self.p.reset();
                        self.frame_edge = false;
                        self.resume_edge = false;
                        self.refresh_requested = false;
                        let open: Vec<u32> = self
                            .closes
                            .iter()
                            .filter(|(_, n)| **n == 0)
                            .map(|(f, _)| *f)
                            .collect();
                        for f in open {
                            *self.closes.get_mut(&f).unwrap() += 1;
                        }
                    }
                }
            }

            /// The resident is registered and shown; a user source with three fenced
            /// presents holds scanout 0 (fence 101 fired and sent, 102 and 103 waiting).
            fn user_tenure() -> World {
                let mut w = World::new();
                w.pass();
                assert_eq!(w.wire, [(Who::Kmd, "copy", 1)]);
                w.advance(20);
                assert_eq!(
                    w.user_set(U1).unwrap().kind,
                    crate::foreign_scanout::SetKind::Preempted
                );
                assert!(w.arb.resident().is_some(), "parked, not ended");
                w.pass();
                assert_eq!(w.wire.len(), 1, "the presenter yields to the user source");
                for f in [101, 102, 103] {
                    w.user_present_fenced(U1, f).unwrap();
                }
                assert_eq!(w.q.len(), 3, "nothing fired yet");
                w.fire(101);
                w.pass();
                assert_eq!(w.q.len(), 2, "101 was sent");
                assert_eq!(w.wire.last().unwrap().0, Who::User);
                // The desktop kept changing under the game.
                w.advance(20);
                w.desktop_changed();
                w
            }

            fn assert_quiet_and_exactly_once(&self) {
                assert!(self.q.is_empty(), "the queue is empty");
                for (f, n) in &self.closes {
                    assert_eq!(*n, 1, "fence {f} was closed {n} times");
                }
                let seqs: Vec<u64> = self.wire.iter().map(|w| w.2).collect();
                assert!(
                    seqs.windows(2).all(|w| w[0] < w[1]),
                    "seq never goes back on the wire: {seqs:?}"
                );
            }
        }

        #[test]
        fn every_end_of_a_user_source_with_fenced_presents_closes_each_fence_once_and_resumes_the_resident_one(
        ) {
            for how in [End::Release, End::CloseFile, End::OwnerExit, End::Lapse] {
                let mut w = World::user_tenure();
                let last_user = w.wire.last().unwrap().2;
                w.end(how);
                w.settle();
                w.assert_quiet_and_exactly_once();
                // The resident source has the screen back, with the newest content: a
                // COPY (the desktop changed during the tenure) after the user's last flip.
                let (who, kind, seq) = *w.wire.last().unwrap();
                assert_eq!((who, kind), (Who::Kmd, "copy"), "{how:?}: {:?}", w.wire);
                assert!(seq > last_user, "{how:?}");
                assert!(w.arb.resident_foreground(), "{how:?}");
                assert!(w.p.registered(), "{how:?}");
                assert_eq!(w.venus_flushes, 0, "{how:?}: no Venus flush while resident");
                // 102 and 103 never reached the host.
                assert_eq!(
                    w.wire.iter().filter(|f| f.0 == Who::User).count(),
                    1,
                    "{how:?}"
                );
            }
        }

        #[test]
        fn a_quiet_tenure_ends_in_a_re_flip_not_a_copy_and_not_a_venus_flush() {
            let mut w = World::new();
            w.pass();
            w.advance(20);
            w.user_set(U1).unwrap();
            w.pass();
            w.user_present_fenced(U1, 101).unwrap();
            w.fire(101);
            w.pass();
            w.end(End::Release);
            w.settle();
            w.assert_quiet_and_exactly_once();
            assert_eq!(
                w.wire.last().map(|f| (f.0, f.1)),
                Some((Who::Kmd, "reflip")),
                "{:?}",
                w.wire
            );
            assert_eq!(w.venus_flushes, 0);
        }

        #[test]
        fn a_transport_reset_ends_everything_and_the_sweep_closes_each_fence_once() {
            let mut w = World::user_tenure();
            w.end(End::Reset);
            assert!(w.q.is_empty());
            for (f, n) in &w.closes {
                assert_eq!(*n, 1, "fence {f}");
            }
            assert!(
                w.arb.resident().is_none(),
                "the registration died with the transport"
            );
            // The new generation starts cold: it registers again, with a first frame.
            let before = w.wire.len();
            w.settle();
            assert!(w.wire.len() > before);
            assert_eq!(w.wire.last().map(|f| f.0), Some(Who::Kmd));
        }

        #[test]
        fn without_a_resident_source_the_end_of_a_user_source_flushes_venus_once() {
            // Level 0 to 2: no presenter, no resident source. The S4 behaviour is unchanged.
            let mut w = World::new();
            w.user_set(U1).unwrap();
            w.user_present_fenced(U1, 101).unwrap();
            w.user_present_fenced(U1, 102).unwrap();
            w.end(End::Release);
            w.pass();
            assert_eq!(w.venus_flushes, 1);
            assert!(w.q.is_empty());
            assert_eq!(w.closes.values().copied().collect::<Vec<_>>(), [1, 1]);
        }

        #[test]
        fn a_flip_taken_off_the_queue_just_before_its_source_ended_lands_late_and_the_resident_answers(
        ) {
            let mut w = World::new();
            w.pass();
            w.advance(20);
            w.user_set(U1).unwrap();
            w.pass();
            w.closes.entry(101).or_insert(0);
            w.fired.insert(101, true);
            // The pump drained a ready entry (its fence is closed) and is about to send ...
            let flip = w.arb.present(U1, UH, w.now).unwrap();
            w.q.push(QEntry {
                flip,
                gem: 7,
                fence: 101,
            })
            .unwrap();
            let late = w.drain_once().expect("a ready entry");
            // ... when the game exits: the resident source takes the screen back and the
            // worker re-flips it.
            w.end(End::OwnerExit);
            w.pass();
            assert_eq!(w.wire.last().map(|f| f.1), Some("reflip"));
            // The late flip is accepted by the host AFTER that. It is what is on screen now,
            // and `flip_done` finds its source gone: the resident source owes a frame again.
            w.send_entry(late);
            w.settle();
            assert_eq!(
                w.wire.last().map(|f| (f.0, f.1)),
                Some((Who::Kmd, "copy")),
                "{:?}",
                w.wire
            );
            assert_eq!(w.venus_flushes, 0);
            assert_eq!(w.closes[&101], 1);
        }

        #[test]
        fn a_resident_flip_in_flight_when_a_user_source_takes_over_costs_nothing() {
            let mut w = World::new();
            w.pass();
            w.advance(20);
            // The presenter minted a flip ...
            let f = w.arb.present(K, KH, w.now).unwrap();
            // ... and the user source set (preempting) before the host answered.
            w.user_set(U1).unwrap();
            w.wire.push((Who::Kmd, "copy", f.seq));
            w.flip_done(f.generation);
            // No resume is owed (the user source is what is live), no Venus flush.
            assert!(!w.arb.take_resume_owed());
            w.refresh();
            assert_eq!(w.venus_flushes, 0);
            // The user's frames flow; the desktop changed meanwhile; the game exits.
            w.user_present_fenced(U1, 101).unwrap();
            w.fire(101);
            w.pass();
            w.desktop_changed();
            w.end(End::Release);
            w.settle();
            w.assert_quiet_and_exactly_once();
            assert_eq!(w.wire.last().map(|f| (f.0, f.1)), Some((Who::Kmd, "copy")));
            assert_eq!(w.venus_flushes, 0);
        }

        #[test]
        fn another_users_source_replaces_a_lapsed_one_and_the_old_ones_queue_is_dropped() {
            let mut w = World::user_tenure();
            // U2 cannot take scanout 0 while U1 is within its lapse.
            assert!(w.user_set(U2).is_err());
            // U1 goes silent past its lapse (its fences 102, 103 never fire); U2 takes over.
            w.advance(2_100);
            assert_eq!(
                w.user_set(U2).unwrap().kind,
                crate::foreign_scanout::SetKind::TookOver
            );
            w.user_present_fenced(U2, 201).unwrap();
            w.pass();
            // U1's entries were dropped by generation (and closed), exactly once; U2's own
            // waits for its fence.
            assert_eq!(w.q.len(), 1);
            assert_eq!(w.closes[&201], 0);
            assert_eq!(w.closes[&102], 1);
            assert_eq!(w.closes[&103], 1);
            assert!(w.arb.resident().is_some(), "still parked behind U2");
            assert!(!w.arb.resident_foreground());
        }
    }
}

//! The host's buffer-release event (`ScanoutReleased`, virtio event queue message 28,
//! device feature `NVGPU_F_SCANOUT_RELEASE`) and the bookkeeping that turns it into
//! "this image can be written again": the pure half. The I/O half is
//! `kmd_render/src/virtio/scanout_release.rs`. Spec: `docs/foreign-scanout.md`
//! ("Buffer release"); host contract: the host's `docs/SCANOUT.md`.
//!
//! # The event
//!
//! `MsgHeader{28, handle 0, status 0}` (16 bytes) then 32 bytes
//! `{scanout, flags, owner_handle, host_handle, seq u64, reserved u64}`. It says: the
//! latest flip (`seq`) of the buffer `(owner_handle, host_handle)` was replaced (or the
//! scanout disabled) and every display client that was sent it is done reading it. A
//! Venus scanout resource is the same event with `RESOURCE` set, `host_handle` the
//! resource id and `owner_handle` / `seq` zero. The buffer on the scanout is never
//! released; a buffer flipped again is released again later; a buffer whose GEM handle
//! or file the guest closed is forgotten with no event.
//!
//! # The book
//!
//! Every flip the KMD itself mints (`SCANOUT_PRESENT`, fenced or not, and the RM ring
//! presenter's) is entered as `(seq, owner, handle, gem)` and moves
//!
//! ```text
//!   Queued ──sent()──► OnHost ──released() / superseded / aged──► Done
//!      └─────────────gone() (skipped, dropped, refused, unsent)──────────┘
//! ```
//!
//! * `Queued`: minted; not on the host (it may wait in the fenced queue, or the send is
//!   in flight). It can still be skipped or dropped, which is `gone`.
//! * `OnHost`: the host took the flip. It is read until a flip of ANOTHER buffer
//!   replaces it and the host's clients are done (the event), or until the same buffer
//!   is flipped again (the newer flip carries the buffer: the older seq is `Done`,
//!   superseded). The seq a client must watch for an image is its LATEST flip's.
//! * `Done`: nothing reads this seq for this flip any more.
//!
//! What an image's user needs is [`ReleaseBook::floor`]: the highest `S` such that every
//! flip of that source with `seq <= S` is `Done`. It is conservative by construction (a
//! slow client holding an old buffer delays the floor, never advances it falsely) and
//! sound for the rule "image whose latest present has seq `P` may be written once
//! `floor >= P`". An entry that stays `OnHost` for [`AGE_OUT_100NS`] after it was
//! replaced counts as `Done` (aged out): the host forces every release after 500 ms, so
//! this only catches the buffers the guest closed (no event) and a lost event, and keeps
//! one stuck entry from holding the floor for ever.
//!
//! The book is a fixed array: nothing here allocates, so the driver can update it from
//! the interrupt DPC under a spinlock. When it is full the oldest entry that is `Done`
//! (else the oldest) is overwritten; the overwritten live one is reported (`evicted`) and
//! then counts as released, like an aged one.
//!
//! Pure functions of their arguments. Time is `now` in 100 ns units.

/// Host `MsgType::ScanoutReleased`.
pub const MSG_SCANOUT_RELEASED: u32 = 28;
/// `MsgHeader` bytes before the body.
pub const HEADER_BYTES: usize = 16;
/// Body bytes (`struct scanout_released`).
pub const BODY_BYTES: usize = 32;
/// The whole message.
pub const MSG_BYTES: usize = HEADER_BYTES + BODY_BYTES;

/// A Venus `SET_SCANOUT_BLOB` resource: `host_handle` is its resource id.
pub const FLAG_RESOURCE: u32 = 1 << 0;
/// No display client was sent the buffer's latest flip.
pub const FLAG_NOT_SHOWN: u32 = 1 << 1;
/// A client did not release it within 500 ms of its replacement; the host overruled.
pub const FLAG_FORCED: u32 = 1 << 2;
/// Every flag this KMD knows; others are ignored, not refused.
pub const FLAGS_KNOWN: u32 = FLAG_RESOURCE | FLAG_NOT_SHOWN | FLAG_FORCED;

/// A parsed `ScanoutReleased`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Released {
    pub scanout: u32,
    pub flags: u32,
    pub owner_handle: u32,
    pub host_handle: u32,
    pub seq: u64,
}

impl Released {
    pub const fn is_resource(&self) -> bool {
        self.flags & FLAG_RESOURCE != 0
    }
    pub const fn not_shown(&self) -> bool {
        self.flags & FLAG_NOT_SHOWN != 0
    }
    pub const fn forced(&self) -> bool {
        self.flags & FLAG_FORCED != 0
    }
}

/// What a filled event buffer held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    Released(Released),
    /// Some other message type (or fewer than 16 bytes).
    NotRelease,
    /// A `ScanoutReleased` shorter than [`MSG_BYTES`]: dropped.
    Short,
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

/// Read one used buffer: `bytes` is the buffer, `len` the length the device reported.
/// The header's `handle` and `status` are not checked (the contract says 0 and 0; a
/// release is a release either way), nor is `reserved`.
pub fn parse(bytes: &[u8], len: usize) -> Parsed {
    let len = len.min(bytes.len());
    if len < HEADER_BYTES || rd32(bytes, 0) != Some(MSG_SCANOUT_RELEASED) {
        return Parsed::NotRelease;
    }
    if len < MSG_BYTES {
        return Parsed::Short;
    }
    let b = HEADER_BYTES;
    match (
        rd32(bytes, b),
        rd32(bytes, b + 4),
        rd32(bytes, b + 8),
        rd32(bytes, b + 12),
        rd64(bytes, b + 16),
    ) {
        (Some(scanout), Some(flags), Some(owner_handle), Some(host_handle), Some(seq)) => {
            Parsed::Released(Released {
                scanout,
                flags,
                owner_handle,
                host_handle,
                seq,
            })
        }
        _ => Parsed::Short,
    }
}

// ---- the book ---------------------------------------------------------------------

/// Flips the book remembers at once: the fenced queue holds 8, the host a couple, and
/// the ring presenter two; 32 is several times what a healthy source has outstanding.
pub const BOOK_SLOTS: usize = 32;
/// How long after a buffer was replaced its entry may stay `OnHost` before it counts as
/// released regardless (2 s: four times the host's forced release).
pub const AGE_OUT_100NS: u64 = 20_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Queued,
    OnHost,
    Done,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// 0 = free.
    seq: u64,
    owner: usize,
    handle: u32,
    gem: u32,
    st: St,
    /// When a newer flip of the source replaced this one on the host (0 = still current).
    replaced_at: u64,
}

const FREE: Entry = Entry {
    seq: 0,
    owner: 0,
    handle: 0,
    gem: 0,
    st: St::Done,
    replaced_at: 0,
};

/// What [`ReleaseBook::released`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// No entry of that buffer at or below `seq`: not one of ours (a forwarded flip, a
    /// buffer already forgotten).
    Unmatched,
    /// `newly` entries became `Done` (0: all were done already). `owner` is the owner
    /// of the newest entry matched, the one to wake.
    Matched { owner: usize, newly: u32 },
}

/// What [`ReleaseBook::sent`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    /// Older flips of the same buffer this one superseded (finished).
    pub superseded: u32,
    /// The owner the flip was minted for.
    pub owner: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Minted {
    /// An entry that was not `Done` had to be overwritten.
    pub evicted_live: bool,
}

/// The flips the KMD minted and what became of them. See the module docs.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseBook {
    e: [Entry; BOOK_SLOTS],
    /// Highest seq ever entered.
    hi: u64,
}

impl Default for ReleaseBook {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseBook {
    pub const fn new() -> Self {
        Self {
            e: [FREE; BOOK_SLOTS],
            hi: 0,
        }
    }

    /// Forget everything (transport reset).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Entries that are not free (for tests and counters).
    pub fn len(&self) -> usize {
        self.e.iter().filter(|e| e.seq != 0).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn find(&self, seq: u64) -> Option<usize> {
        if seq == 0 {
            return None;
        }
        self.e.iter().position(|e| e.seq == seq)
    }

    /// Whether `e` still reads, at `now`: not `Done`, not aged out.
    fn live(e: &Entry, now: u64) -> bool {
        match e.st {
            St::Done => false,
            St::Queued => true,
            St::OnHost => {
                !(e.replaced_at != 0 && now.saturating_sub(e.replaced_at) >= AGE_OUT_100NS)
            }
        }
    }

    /// Enter a flip of `(owner, handle, gem)` that was just minted as `seq` (nonzero).
    pub fn minted(&mut self, seq: u64, owner: usize, handle: u32, gem: u32) -> Minted {
        if seq == 0 {
            return Minted {
                evicted_live: false,
            };
        }
        self.hi = self.hi.max(seq);
        let fresh = Entry {
            seq,
            owner,
            handle,
            gem,
            st: St::Queued,
            replaced_at: 0,
        };
        // The same seq again: the newest word wins.
        if let Some(i) = self.find(seq) {
            self.e[i] = fresh;
            return Minted {
                evicted_live: false,
            };
        }
        if let Some(i) = self.e.iter().position(|e| e.seq == 0) {
            self.e[i] = fresh;
            return Minted {
                evicted_live: false,
            };
        }
        // Full: the oldest Done entry, else the oldest.
        let oldest = |pick: &dyn Fn(&Entry) -> bool| {
            self.e
                .iter()
                .enumerate()
                .filter(|(_, e)| pick(e))
                .min_by_key(|(_, e)| e.seq)
                .map(|(i, _)| i)
        };
        let (i, live) = match oldest(&|e| e.st == St::Done) {
            Some(i) => (i, false),
            None => (oldest(&|_| true).unwrap_or(0), true),
        };
        self.e[i] = fresh;
        Minted { evicted_live: live }
    }

    /// The host took flip `seq` at `now`. Every older flip still on the host is replaced:
    /// one of the SAME buffer is superseded (done), any other starts its age-out clock.
    /// `None` if `seq` was not known; otherwise how many older flips this finished
    /// (superseded), which may move the floor: whoever waits for it is woken, because the
    /// host's release event for a flip this replaced can arrive BEFORE this call (it is
    /// sent before the flip's reply).
    pub fn sent(&mut self, seq: u64, now: u64) -> Option<Sent> {
        let i = self.find(seq)?;
        if self.e[i].st == St::Queued {
            self.e[i].st = St::OnHost;
        }
        let (handle, gem) = (self.e[i].handle, self.e[i].gem);
        let mut superseded = 0;
        for (k, e) in self.e.iter_mut().enumerate() {
            if k == i || e.seq == 0 || e.seq >= seq || e.st != St::OnHost {
                continue;
            }
            if e.handle == handle && e.gem == gem {
                e.st = St::Done;
                superseded += 1;
            } else if e.replaced_at == 0 {
                e.replaced_at = now.max(1);
            }
        }
        Some(Sent {
            superseded,
            owner: self.e[i].owner,
        })
    }

    /// Flip `seq` will never be (or was never) shown: skipped, dropped with its source,
    /// refused by the host, not sent. Returns the owner when this changed anything.
    pub fn gone(&mut self, seq: u64) -> Option<usize> {
        let i = self.find(seq)?;
        if self.e[i].st == St::Done {
            return None;
        }
        self.e[i].st = St::Done;
        Some(self.e[i].owner)
    }

    /// The host released `(owner_handle, host_handle)` up to its latest flip `seq`.
    pub fn released(&mut self, owner_handle: u32, host_handle: u32, seq: u64) -> Release {
        let mut matched = false;
        let mut newly = 0;
        let mut newest: Option<(u64, usize)> = None;
        for e in self.e.iter_mut() {
            if e.seq == 0 || e.handle != owner_handle || e.gem != host_handle || e.seq > seq {
                continue;
            }
            matched = true;
            if newest.is_none_or(|(s, _)| e.seq > s) {
                newest = Some((e.seq, e.owner));
            }
            if e.st != St::Done {
                e.st = St::Done;
                newly += 1;
            }
        }
        match (matched, newest) {
            (true, Some((_, owner))) => Release::Matched { owner, newly },
            _ => Release::Unmatched,
        }
    }

    /// Whether nothing reads flip `seq` any more, at `now`. A seq the book does not
    /// know (never entered, overwritten, forgotten) counts as done: there is nothing to
    /// wait for.
    pub fn is_done(&self, seq: u64, now: u64) -> bool {
        match self.find(seq) {
            None => true,
            Some(i) => !Self::live(&self.e[i], now),
        }
    }

    /// `(floor, last)` for the source with DRM handle `handle`, at `now`: `last` is the
    /// highest seq of that handle in the book, `floor` the highest seq such that every
    /// flip of the handle up to it is done (== `last` when none is live). A handle the
    /// book has nothing of answers the highest seq ever entered for both: all of that
    /// caller's flips are, as far as anyone can tell, done.
    pub fn floor(&self, handle: u32, now: u64) -> (u64, u64) {
        let mut last = 0u64;
        let mut first_live: Option<u64> = None;
        for e in self.e.iter().filter(|e| e.seq != 0 && e.handle == handle) {
            last = last.max(e.seq);
            if Self::live(e, now) {
                first_live = Some(first_live.map_or(e.seq, |s| s.min(e.seq)));
            }
        }
        if last == 0 {
            return (self.hi, self.hi);
        }
        match first_live {
            Some(s) => (s - 1, last),
            None => (last, last),
        }
    }

    /// The file `handle` was closed: the host forgets its buffers with no event.
    pub fn forget_handle(&mut self, handle: u32) {
        for e in self.e.iter_mut().filter(|e| e.handle == handle) {
            *e = FREE;
        }
    }

    /// The device `owner` is gone.
    pub fn forget_owner(&mut self, owner: usize) {
        for e in self.e.iter_mut().filter(|e| e.owner == owner) {
            *e = FREE;
        }
    }
}

// ---- the ring presenter's wait ----------------------------------------------------

/// How long the RM ring presenter waits for the release of the surface it wants to write
/// before it writes anyway (500 ms from the moment the surface was replaced: the host's
/// own forced-release limit).
pub const RING_WAIT_100NS: u64 = 5_000_000;

/// Where a wait for a release stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Released (or nothing to wait for): go.
    Go,
    /// Not released yet and the limit is `until`: wait, and look again when the event
    /// arrives or the time comes.
    Hold { until: u64 },
    /// Not released and the limit passed: go anyway (the caller counts it).
    TimedOut,
}

/// The wait rule: `released` says whether the surface's last flip is done; it was
/// replaced at `replaced_at` (the time of the flip that put another surface on screen).
pub fn ring_wait(released: bool, replaced_at: u64, now: u64) -> Wait {
    if released {
        return Wait::Go;
    }
    let until = replaced_at.saturating_add(RING_WAIT_100NS);
    if now < until {
        Wait::Hold { until }
    } else {
        Wait::TimedOut
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const MS: u64 = 10_000;

    fn msg(ty: u32, flags: u32, owner: u32, host: u32, seq: u64, len: usize) -> Vec<u8> {
        let mut b = std::vec![0u8; 256];
        b[0..4].copy_from_slice(&ty.to_le_bytes());
        b[16..20].copy_from_slice(&0u32.to_le_bytes());
        b[20..24].copy_from_slice(&flags.to_le_bytes());
        b[24..28].copy_from_slice(&owner.to_le_bytes());
        b[28..32].copy_from_slice(&host.to_le_bytes());
        b[32..40].copy_from_slice(&seq.to_le_bytes());
        b.truncate(len.max(1));
        b
    }

    // ---- the message --------------------------------------------------------------

    #[test]
    fn a_release_is_parsed_field_by_field() {
        let b = msg(28, FLAG_NOT_SHOWN | FLAG_FORCED, 7, 9, 0x1_0000_0002, 48);
        let Parsed::Released(r) = parse(&b, 48) else {
            panic!("not parsed");
        };
        assert_eq!(
            r,
            Released {
                scanout: 0,
                flags: FLAG_NOT_SHOWN | FLAG_FORCED,
                owner_handle: 7,
                host_handle: 9,
                seq: 0x1_0000_0002
            }
        );
        assert!(r.not_shown() && r.forced() && !r.is_resource());
        // A Venus resource: owner and seq are zero, the flag says what host_handle is.
        let Parsed::Released(v) = parse(&msg(28, FLAG_RESOURCE, 0, 33, 0, 48), 48) else {
            panic!();
        };
        assert!(v.is_resource() && v.host_handle == 33 && v.seq == 0);
    }

    #[test]
    fn other_messages_and_short_ones_are_not_releases() {
        let ok = msg(28, 0, 1, 2, 3, 256);
        // The device's length governs, not the buffer's.
        assert_eq!(parse(&ok, 47), Parsed::Short);
        assert_eq!(parse(&ok, 16), Parsed::Short);
        assert_eq!(parse(&ok, 15), Parsed::NotRelease);
        assert_eq!(parse(&ok, 0), Parsed::NotRelease);
        assert!(matches!(parse(&ok, 48), Parsed::Released(_)));
        // A length beyond the buffer is clamped, not trusted.
        let short_buf = msg(28, 0, 1, 2, 3, 40);
        assert_eq!(parse(&short_buf, 4096), Parsed::Short);
        // EventReady (8) with a long length is no release.
        assert_eq!(parse(&msg(8, 0, 1, 2, 3, 256), 48), Parsed::NotRelease);
        assert_eq!(parse(&msg(29, 0, 1, 2, 3, 256), 48), Parsed::NotRelease);
        // Unknown flag bits and a nonzero scanout are the caller's business, not a parse error.
        let Parsed::Released(r) = parse(&msg(28, 0xF0, 1, 2, 3, 256), 48) else {
            panic!();
        };
        assert_eq!(r.flags & !FLAGS_KNOWN, 0xF0);
    }

    // ---- the book -----------------------------------------------------------------

    const KMD: usize = usize::MAX;
    const USR: usize = 0x1000;

    /// Mint and send flip `seq` of `(handle, gem)` at `t`.
    fn flip(b: &mut ReleaseBook, seq: u64, owner: usize, handle: u32, gem: u32, t: u64) {
        b.minted(seq, owner, handle, gem);
        assert!(b.sent(seq, t).is_some());
    }

    #[test]
    fn a_flip_is_not_done_until_its_buffer_is_replaced_and_released() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, USR, 5, 100, 10);
        assert!(!b.is_done(1, 11), "on the host, current");
        flip(&mut b, 2, USR, 5, 101, 20);
        // Replaced, but the host has not said it is done with it.
        assert!(!b.is_done(1, 21));
        assert_eq!(b.floor(5, 21), (0, 2));
        assert_eq!(
            b.released(5, 100, 1),
            Release::Matched {
                owner: USR,
                newly: 1
            }
        );
        assert!(b.is_done(1, 22));
        assert!(
            !b.is_done(2, 22),
            "the buffer on the scanout is not released"
        );
        assert_eq!(b.floor(5, 22), (1, 2));
    }

    #[test]
    fn the_floor_is_contiguous_and_never_runs_ahead_of_a_slow_buffer() {
        let mut b = ReleaseBook::new();
        for (s, g) in [(1, 100), (2, 101), (3, 102), (4, 100)] {
            flip(&mut b, s, USR, 5, g, s * 10);
        }
        // Seq 1 is superseded by seq 4 (same buffer): done at once.
        assert!(b.is_done(1, 41));
        // 3 released before 2: the floor must not skip 2.
        b.released(5, 102, 3);
        assert_eq!(b.floor(5, 50), (1, 4));
        b.released(5, 101, 2);
        assert_eq!(b.floor(5, 51), (3, 4));
        b.released(5, 100, 4);
        assert_eq!(b.floor(5, 52), (4, 4));
    }

    #[test]
    fn a_release_covers_every_older_flip_of_the_buffer_and_none_of_the_newer() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, USR, 5, 100, 10);
        flip(&mut b, 2, USR, 5, 101, 20);
        // Seq 1 was queued behind a fence when A's second flip was minted.
        b.minted(3, USR, 5, 100);
        // Event for seq 2 of buffer 101 only touches that buffer.
        assert_eq!(
            b.released(5, 101, 2),
            Release::Matched {
                owner: USR,
                newly: 1
            }
        );
        assert!(!b.is_done(1, 30));
        // The event for A carries its latest flip as the host knew it (1): 3 is newer.
        assert_eq!(
            b.released(5, 100, 1),
            Release::Matched {
                owner: USR,
                newly: 1
            }
        );
        assert!(b.is_done(1, 31));
        assert!(!b.is_done(3, 31));
        // A repeat matches but frees nothing new.
        assert_eq!(
            b.released(5, 100, 1),
            Release::Matched {
                owner: USR,
                newly: 0
            }
        );
    }

    #[test]
    fn events_that_name_no_flip_of_ours_are_unmatched() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 5, USR, 5, 100, 10);
        // Another file, another buffer, an older seq than any entry of that buffer.
        assert_eq!(b.released(6, 100, 9), Release::Unmatched);
        assert_eq!(b.released(5, 101, 9), Release::Unmatched);
        assert_eq!(b.released(5, 100, 4), Release::Unmatched);
        assert!(!b.is_done(5, 11));
    }

    #[test]
    fn skipped_dropped_and_refused_flips_are_done_without_an_event() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, USR, 5, 100, 10);
        b.minted(2, USR, 5, 101);
        b.minted(3, USR, 5, 102);
        assert_eq!(b.floor(5, 11), (0, 3));
        // 2 is skipped for 3; 3 is sent.
        assert_eq!(b.gone(2), Some(USR));
        assert_eq!(b.gone(2), None, "once");
        assert_eq!(b.sent(3, 20).map(|s| s.superseded), Some(0));
        // 1 was replaced by 3; its release comes.
        b.released(5, 100, 1);
        assert_eq!(b.floor(5, 21), (2, 3));
        // An unknown seq is a no-op.
        assert_eq!(b.gone(99), None);
        assert_eq!(b.sent(99, 22), None);
    }

    #[test]
    fn a_buffer_the_host_never_releases_ages_out_after_its_replacement() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, USR, 5, 100, 10 * MS);
        flip(&mut b, 2, USR, 5, 101, 20 * MS);
        flip(&mut b, 3, USR, 5, 102, 30 * MS);
        // 1 and 2 are replaced (clocks from 20 ms and 30 ms); 3 is current.
        let at = |dt: u64| 30 * MS + dt;
        assert!(!b.is_done(1, at(0)));
        assert!(b.is_done(1, 20 * MS + AGE_OUT_100NS));
        assert!(!b.is_done(2, 30 * MS + AGE_OUT_100NS - 1));
        assert!(b.is_done(2, 30 * MS + AGE_OUT_100NS));
        // The current one never ages, however idle the desktop is.
        assert!(!b.is_done(3, u64::MAX / 2));
        assert_eq!(b.floor(5, 30 * MS + AGE_OUT_100NS), (2, 3));
    }

    #[test]
    fn a_fenced_queue_that_skips_frames_makes_their_images_reusable_at_once() {
        use crate::foreign_scanout::{Flip, Layout, FOURCC_XRGB8888};
        use crate::rm_fence_present::{QEntry, ScanoutQueue};
        let layout = Layout {
            width: 64,
            height: 64,
            stride: 256,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: 0,
        };
        let entry = |seq: u64, gem: u32, fence: u32| QEntry {
            flip: Flip {
                seq,
                generation: 1,
                handle: 5,
                epoch: 7,
                layout,
            },
            gem,
            fence,
        };
        let mut book = ReleaseBook::new();
        let mut q = ScanoutQueue::new();
        // Three images A B C = gem 11 12 13, presented A B C A with fences 1..4.
        for (seq, gem) in [(1u64, 11u32), (2, 12), (3, 13), (4, 11)] {
            book.minted(seq, USR, 5, gem);
            q.push(entry(seq, gem, seq as u32)).unwrap();
        }
        assert_eq!(book.floor(5, 0), (0, 4));
        // Fences 1..3 fired: 3 is sent, 1 and 2 never reach the host.
        let d = q.drain(Some((1, 7)), |f| f <= 3);
        for &s in d.gone_seqs() {
            assert_eq!(book.gone(s), Some(USR));
        }
        let sent = d.send.unwrap();
        book.sent(sent.flip.seq, 10 * MS);
        assert_eq!(sent.flip.seq, 3);
        // Images A (seq 1) and B (seq 2) were never shown: their number is done...
        assert!(book.is_done(1, 10 * MS) && book.is_done(2, 10 * MS));
        assert_eq!(book.floor(5, 10 * MS), (2, 4));
        // ...but A's LATEST present (4) is not, and C (3) is on the screen.
        assert!(!book.is_done(4, 10 * MS) && !book.is_done(3, 10 * MS));
        // Fence 4 fires: A is sent behind C. The host releases C when A replaces it.
        let d = q.drain(Some((1, 7)), |_| true);
        assert_eq!(d.send.map(|e| e.flip.seq), Some(4));
        book.sent(4, 20 * MS);
        assert_eq!(book.floor(5, 20 * MS), (2, 4));
        assert_eq!(
            book.released(5, 13, 3),
            Release::Matched {
                owner: USR,
                newly: 1
            }
        );
        assert_eq!(book.floor(5, 21 * MS), (3, 4));
        // The source ends with one more queued: dropped, retired.
        book.minted(5, USR, 5, 12);
        q.push(entry(5, 12, 5)).unwrap();
        let d = q.drain(None, |_| true);
        assert_eq!(d.gone_seqs(), &[5]);
        assert_eq!(book.gone(5), Some(USR));
        assert_eq!(book.floor(5, 22 * MS), (3, 5));
    }

    #[test]
    fn sending_a_flip_reports_the_older_flips_of_its_buffer_it_finished() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, USR, 5, 100, 10);
        flip(&mut b, 2, USR, 5, 101, 20);
        // The host's release of B (seq 2) reaches us while A's second flip is still being
        // sent: the floor is held by seq 1, which only the send of seq 3 finishes.
        b.minted(3, USR, 5, 100);
        assert_eq!(
            b.released(5, 101, 2),
            Release::Matched {
                owner: USR,
                newly: 1
            }
        );
        assert_eq!(b.floor(5, 21), (0, 3));
        assert_eq!(
            b.sent(3, 22),
            Some(Sent {
                superseded: 1,
                owner: USR
            })
        );
        assert_eq!(b.floor(5, 23), (2, 3));
    }

    #[test]
    fn a_source_with_a_second_handle_is_judged_apart() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, KMD, 3, 7, 10);
        flip(&mut b, 2, USR, 5, 100, 20);
        // The user's flip replaced the ring surface: its clock runs, nothing is released.
        assert!(!b.is_done(1, 21));
        assert_eq!(b.floor(3, 21), (0, 1));
        assert_eq!(
            b.floor(5, 21),
            (1, 2),
            "the KMD's older flip is not the user's"
        );
        assert_eq!(
            b.released(3, 7, 1),
            Release::Matched {
                owner: KMD,
                newly: 1
            }
        );
        assert_eq!(b.floor(3, 22), (1, 1));
    }

    #[test]
    fn an_unknown_handle_has_nothing_outstanding() {
        let mut b = ReleaseBook::new();
        assert_eq!(b.floor(9, 0), (0, 0));
        flip(&mut b, 4, USR, 5, 100, 10);
        assert_eq!(b.floor(9, 11), (4, 4));
        assert!(b.is_done(77, 11), "an unknown seq has nothing to wait for");
        assert!(b.is_done(0, 11));
    }

    #[test]
    fn the_book_overwrites_done_entries_first_and_reports_a_live_one() {
        let mut b = ReleaseBook::new();
        for s in 1..=BOOK_SLOTS as u64 {
            flip(&mut b, s, USR, 5, (100 + s) as u32, s);
        }
        assert_eq!(b.len(), BOOK_SLOTS);
        // Entry 3 is released: it is the one overwritten.
        b.released(5, 103, 3);
        assert_eq!(
            b.minted(100, USR, 5, 900),
            Minted {
                evicted_live: false
            }
        );
        assert!(b.find(3).is_none() && b.find(1).is_some());
        // Nothing done now: the oldest (seq 1, live) goes.
        assert_eq!(b.minted(101, USR, 5, 901), Minted { evicted_live: true });
        assert!(b.find(1).is_none());
        // An overwritten flip counts as done: nothing is waited for.
        assert!(b.is_done(1, 50));
        assert_eq!(b.len(), BOOK_SLOTS);
    }

    #[test]
    fn closing_the_file_or_the_device_forgets_without_events() {
        let mut b = ReleaseBook::new();
        flip(&mut b, 1, KMD, 3, 7, 10);
        flip(&mut b, 2, USR, 5, 100, 20);
        flip(&mut b, 3, USR, 6, 100, 30);
        b.forget_handle(5);
        assert!(b.is_done(2, 31) && b.find(2).is_none());
        assert_eq!(b.len(), 2);
        b.forget_owner(USR);
        assert_eq!(b.len(), 1);
        b.reset();
        assert!(b.is_empty());
        assert_eq!(b.floor(3, 0), (0, 0), "reset forgets the highest seq too");
    }

    #[test]
    fn seqs_compare_as_plain_u64s_with_no_wrap() {
        let mut b = ReleaseBook::new();
        let base = u64::MAX - 3;
        flip(&mut b, base, USR, 5, 100, 10);
        flip(&mut b, base + 1, USR, 5, 101, 20);
        assert_eq!(b.floor(5, 21), (base - 1, base + 1));
        b.released(5, 100, base);
        assert_eq!(b.floor(5, 22), (base, base + 1));
    }

    // ---- the ring presenter's wait -------------------------------------------------

    #[test]
    fn the_ring_waits_for_a_release_until_the_forced_limit() {
        let t0 = 1_000 * MS;
        assert_eq!(ring_wait(true, t0, t0), Wait::Go);
        assert_eq!(
            ring_wait(false, t0, t0 + 1),
            Wait::Hold {
                until: t0 + RING_WAIT_100NS
            }
        );
        assert_eq!(
            ring_wait(false, t0, t0 + RING_WAIT_100NS - 1),
            Wait::Hold {
                until: t0 + RING_WAIT_100NS
            }
        );
        assert_eq!(ring_wait(false, t0, t0 + RING_WAIT_100NS), Wait::TimedOut);
        // The release wins over the clock.
        assert_eq!(ring_wait(true, t0, t0 + 10 * RING_WAIT_100NS), Wait::Go);
        // No overflow at the end of time.
        assert_eq!(ring_wait(false, u64::MAX, u64::MAX), Wait::TimedOut);
    }
}

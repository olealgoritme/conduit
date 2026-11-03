//! Hot-path cost measurement and the `NvHotPath` knob (NVIDIA UMD personality).
//!
//! The I/O half is `kmd_render/src/ddi/hot_path.rs`. The hot DDIs (escapes,
//! `SubmitCommandToHwQueue`, `BuildPagingBuffer`) only bump atomics; the
//! registry mirror thread turns the counts into per-second rates once per
//! publish window. Everything here is a function of its arguments.
//!
//! `NvHotPath` (REG_DWORD, default 0) enables the optional shortcuts; with 0
//! every path does what it did before the knob existed:
//!
//! * [`HotPath::FAST_FLIP`] (0x1): a native primary whose GEM is already
//!   imported on this transport's DRM file is flipped without taking the
//!   native-memory I/O mutex (which leaf updates hold across their host round
//!   trips). A first flip still imports under the mutex.
//! * [`HotPath::SKIP_FOREIGN_UNCHANGED`] (0x2): a foreign (non-native) leaf
//!   update that names exactly the system pages already mapped at exactly
//!   that range is answered without the unmap / descriptor / map round trips.
//! * [`HotPath::NO_QUIESCE`] (0x4): a native leaf update that replaces a live
//!   mapping skips RM's idle-channels call (`NV_ESC_RM_IDLE_CHANNELS`, one host
//!   round trip that waits for the client's channels). VidMm already orders a
//!   placement change after the GPU work that uses it.
//! * [`HotPath::ASYNC_FLIP`] (0x8): a native primary's host `ScanoutFlip` is
//!   submitted without waiting for its reply (`present_submit`); the reply is
//!   settled at the next native flip through the flip pipeline
//!   (`crate::flip_pipeline`), with the same bookkeeping as the round trip's
//!   return. A full window (2 in flight) or a full queue sends
//!   that flip the synchronous way.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::native_memory::{self as nm, MappingPiece};

/// The `NvHotPath` knob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HotPath(pub u32);

impl HotPath {
    pub const FAST_FLIP: u32 = 0x1;
    pub const SKIP_FOREIGN_UNCHANGED: u32 = 0x2;
    pub const NO_QUIESCE: u32 = 0x4;
    pub const ASYNC_FLIP: u32 = 0x8;

    const fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
    pub const fn fast_flip(self) -> bool {
        self.has(Self::FAST_FLIP)
    }
    pub const fn skip_foreign_unchanged(self) -> bool {
        self.has(Self::SKIP_FOREIGN_UNCHANGED)
    }
    pub const fn no_quiesce(self) -> bool {
        self.has(Self::NO_QUIESCE)
    }
    pub const fn async_flip(self) -> bool {
        self.has(Self::ASYNC_FLIP)
    }
}

/// The `NvPaging` knob (REG_DWORD, default 0): the cheaper page-table update
/// paths of `kmd_render` `virtio/native_memory.rs`. With 0 every update makes
/// its RM calls one round trip each and frees each OS descriptor at its unmap,
/// as before the knob existed.
///
/// * [`Paging::BATCH`] (0x1): the RM calls of one system-page or foreign leaf
///   update (descriptor, virtual object, map; unmaps and frees) go to the host
///   as one `RmBatch` (`crate::rm_batch`), only when the host's config features
///   carry `NVGPU_CFG_RM_BATCH`; a host that refuses a batch gets single calls.
/// * [`Paging::DESC_CACHE`] (0x2): an OS descriptor whose last mapping went is
///   kept (up to `NvDescCacheMax`, default 256) and reused for the same pages
///   (`crate::desc_cache`). Sharing a mapped descriptor for the same pages is
///   always on: it is the same RM object either way.
/// * [`Paging::ONE_LOCK`] (0x4): a system-page leaf update takes the
///   native-memory mutex once for its unmap and its map half (it took it twice:
///   anything queued meanwhile ran between the two halves, and the update
///   waited for it again).
/// * [`Paging::COPY_UNLOCKED`] (0x8): `with_cpu_range` (VidMm's native
///   fills and transfers, through an uncached BAR view) lets go of the
///   native-memory mutex while it copies; only the RM map / unmap of the
///   temporary view run under it. A copy of many megabytes through an uncached
///   view takes up to seconds, and every leaf update on another thread waited.
/// * [`Paging::COPY_WC`] (0x10): that temporary view is write-combined instead
///   of uncached (writes, i.e. fills and uploads, are combined into bursts),
///   with a store fence after the copy.
/// * [`Paging::COPY_LOCKED`] (0x20): BuildPagingBuffer's own CPU copies keep the
///   mutex too. Without it they copy unlocked whatever 0x8 says: VidMm destroys
///   no allocation while one of its paging operations runs, and the temporary
///   view is the copy's alone ([`Paging::paging_copy_unlocked`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Paging(pub u32);

impl Paging {
    pub const BATCH: u32 = 0x1;
    pub const DESC_CACHE: u32 = 0x2;
    pub const ONE_LOCK: u32 = 0x4;
    pub const COPY_UNLOCKED: u32 = 0x8;
    pub const COPY_WC: u32 = 0x10;
    pub const COPY_LOCKED: u32 = 0x20;

    pub const fn batch(self) -> bool {
        self.0 & Self::BATCH != 0
    }
    pub const fn desc_cache(self) -> bool {
        self.0 & Self::DESC_CACHE != 0
    }
    pub const fn one_lock(self) -> bool {
        self.0 & Self::ONE_LOCK != 0
    }
    pub const fn copy_unlocked(self) -> bool {
        self.0 & Self::COPY_UNLOCKED != 0
    }
    pub const fn copy_wc(self) -> bool {
        self.0 & Self::COPY_WC != 0
    }
    pub const fn copy_locked(self) -> bool {
        self.0 & Self::COPY_LOCKED != 0
    }
    /// Whether a BuildPagingBuffer copy lets go of the mutex while it copies: by default,
    /// unless [`Self::COPY_LOCKED`].
    pub const fn paging_copy_unlocked(self) -> bool {
        self.copy_unlocked() || !self.copy_locked()
    }
}

/// The default period of the mirror pass the hot paths ask for (`NvHotPubMs`).
pub const DEFAULT_PUBLISH_MS: u32 = 1_000;
/// A window shorter than this keeps the previous rates (a pass someone else asked
/// for right after ours would otherwise publish a noisy rate).
pub const MIN_WINDOW_MS: u32 = 250;

/// Whether a periodic publish is due at `now_ms` (interrupt time, never 0) when the
/// last one was asked for at `last_ms` (0 = never). `period_ms` 0 = off.
pub const fn publish_due(now_ms: u32, last_ms: u32, period_ms: u32) -> bool {
    period_ms != 0 && (last_ms == 0 || now_ms.wrapping_sub(last_ms) >= period_ms)
}

/// `delta` events over `window_ms`, per second (rounded down, saturating).
pub fn per_second(delta: u32, window_ms: u32) -> u32 {
    if window_ms == 0 {
        return 0;
    }
    u32::try_from(u64::from(delta) * 1000 / u64::from(window_ms)).unwrap_or(u32::MAX)
}

/// `num / den` in hundredths (`num` per `den`, x100), 0 when `den` is 0.
pub fn ratio_x100(num: u32, den: u32) -> u32 {
    if den == 0 {
        return 0;
    }
    u32::try_from(u64::from(num) * 100 / u64::from(den)).unwrap_or(u32::MAX)
}

/// Microseconds of `delta_us` spent per second of `window_ms` (a load in
/// thousandths: 1000 = one thread busy the whole window).
pub fn us_per_second(delta_us: u32, window_ms: u32) -> u32 {
    per_second(delta_us, window_ms)
}

/// A fixed census of the most frequent keys (an NVDA function, an RM control
/// command). Lock-free: a key claims the first empty slot with a CAS; once every
/// slot holds a key, unknown keys count in [`Census::overflow`]. Key 0 is
/// counted as overflow (it marks an empty slot). Legal at any IRQL.
pub struct Census<const N: usize> {
    keys: [AtomicU32; N],
    counts: [AtomicU32; N],
    overflow: AtomicU32,
}

impl<const N: usize> Census<N> {
    pub const fn new() -> Self {
        Self {
            keys: [const { AtomicU32::new(0) }; N],
            counts: [const { AtomicU32::new(0) }; N],
            overflow: AtomicU32::new(0),
        }
    }

    /// Count one occurrence of `key`.
    pub fn note(&self, key: u32) {
        if key == 0 {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        for i in 0..N {
            let held = self.keys[i].load(Ordering::Acquire);
            if held == key {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
            if held == 0 {
                match self.keys[i].compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        self.counts[i].fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // Another thread claimed it: for this key, or go on.
                    Err(other) if other == key => {
                        self.counts[i].fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Err(_) => {}
                }
            }
        }
        self.overflow.fetch_add(1, Ordering::Relaxed);
    }

    /// `(key, count)` of slot `i` (key 0 = empty).
    pub fn slot(&self, i: usize) -> (u32, u32) {
        match (self.keys.get(i), self.counts.get(i)) {
            (Some(k), Some(c)) => (k.load(Ordering::Acquire), c.load(Ordering::Relaxed)),
            _ => (0, 0),
        }
    }

    /// Occurrences that found no slot.
    pub fn overflow(&self) -> u32 {
        self.overflow.load(Ordering::Relaxed)
    }

    /// Forget every key and count (a new transport generation).
    pub fn reset(&self) {
        for i in 0..N {
            self.keys[i].store(0, Ordering::Relaxed);
            self.counts[i].store(0, Ordering::Relaxed);
        }
        self.overflow.store(0, Ordering::Relaxed);
    }
}

impl<const N: usize> Default for Census<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// One counter's value at the last publish, for its rate over the next window.
/// Used by one thread (the mirror pass); atomics only so it can be a `static`.
pub struct Rate {
    last: AtomicU32,
    rate: AtomicU32,
    primed: AtomicU32,
}

impl Rate {
    pub const fn new() -> Self {
        Self {
            last: AtomicU32::new(0),
            rate: AtomicU32::new(0),
            primed: AtomicU32::new(0),
        }
    }

    /// The counter reads `now` after a window of `window_ms`: the delta since the
    /// last publish (wrapping) and, when the window is long enough, the new rate.
    /// Returns `(delta, rate per second)`. A short window keeps the previous rate
    /// and does not consume the delta. The first sample only takes the baseline
    /// (`(0, 0)`): the counts before it are not one window's.
    pub fn sample(&self, now: u32, window_ms: u32) -> (u32, u32) {
        if self.primed.swap(1, Ordering::Relaxed) == 0 {
            self.last.store(now, Ordering::Relaxed);
            return (0, 0);
        }
        let last = self.last.load(Ordering::Relaxed);
        let delta = now.wrapping_sub(last);
        if window_ms < MIN_WINDOW_MS {
            return (delta, self.rate.load(Ordering::Relaxed));
        }
        self.last.store(now, Ordering::Relaxed);
        let rate = per_second(delta, window_ms);
        self.rate.store(rate, Ordering::Relaxed);
        (delta, rate)
    }
}

impl Default for Rate {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a foreign leaf update over `[first_va, first_va + len)` naming `pfns`
/// would remake exactly what is mapped there already: one live foreign mapping
/// of this process overlaps the range, it covers exactly that range from
/// descriptor offset 0, and its descriptor holds exactly these pages (`None`:
/// pages not kept, cannot tell).
pub fn foreign_update_unchanged(
    overlapping: &[(MappingPiece, Option<&[u64]>)],
    first_va: u64,
    len: u64,
    pfns: &[u64],
) -> bool {
    if pfns.is_empty() || len != pfns.len() as u64 * nm::PAGE_BYTES {
        return false;
    }
    let [(piece, Some(shown))] = overlapping else {
        return false;
    };
    piece.va == first_va && piece.len == len && piece.memory_offset == 0 && *shown == pfns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_flip_bit() {
        assert!(HotPath(0x8).async_flip());
        assert!(!HotPath(0x7).async_flip());
        assert!(!HotPath(0x8).fast_flip() && !HotPath(0x8).no_quiesce());
    }

    #[test]
    fn paging_knob_bits() {
        assert!(!Paging(0).batch() && !Paging(0).desc_cache());
        assert!(Paging(1).batch() && !Paging(1).desc_cache());
        assert!(!Paging(2).batch() && Paging(2).desc_cache());
        assert!(Paging(3).batch() && Paging(3).desc_cache());
        assert!(!Paging(3).one_lock() && !Paging(3).copy_unlocked() && !Paging(3).copy_wc());
        assert!(Paging(4).one_lock() && !Paging(4).batch());
        assert!(Paging(8).copy_unlocked() && !Paging(8).one_lock());
        assert!(Paging(0x10).copy_wc() && !Paging(0x10).copy_unlocked());
        let all = Paging(0x1f);
        assert!(all.batch() && all.desc_cache() && all.one_lock() && all.copy_unlocked() && all.copy_wc());
        // BuildPagingBuffer's copies run unlocked unless 0x20 (and 0x8 still wins).
        assert!(Paging(0).paging_copy_unlocked() && !Paging(0).copy_unlocked());
        assert!(!Paging(0x20).paging_copy_unlocked() && Paging(0x20).copy_locked());
        assert!(Paging(0x28).paging_copy_unlocked());
    }

    #[test]
    fn knob_bits() {
        let none = HotPath(0);
        assert!(!none.fast_flip() && !none.skip_foreign_unchanged() && !none.no_quiesce());
        let all = HotPath(7);
        assert!(all.fast_flip() && all.skip_foreign_unchanged() && all.no_quiesce());
        assert!(HotPath(1).fast_flip() && !HotPath(1).no_quiesce());
        assert!(HotPath(2).skip_foreign_unchanged() && !HotPath(2).fast_flip());
        assert!(HotPath(4).no_quiesce() && !HotPath(4).skip_foreign_unchanged());
    }

    #[test]
    fn publish_cadence() {
        assert!(publish_due(5, 0, 1000));
        assert!(!publish_due(500, 1, 1000));
        assert!(publish_due(1001, 1, 1000));
        assert!(!publish_due(1001, 1, 0));
        // Interrupt-time wrap.
        assert!(publish_due(10, u32::MAX - 1000, 1000));
    }

    #[test]
    fn rates() {
        assert_eq!(per_second(500, 1000), 500);
        assert_eq!(per_second(500, 500), 1000);
        assert_eq!(per_second(1, 3000), 0);
        assert_eq!(per_second(7, 0), 0);
        assert_eq!(per_second(u32::MAX, 1), u32::MAX);
        assert_eq!(ratio_x100(3, 2), 150);
        assert_eq!(ratio_x100(0, 2), 0);
        assert_eq!(ratio_x100(9, 0), 0);
        assert_eq!(us_per_second(250_000, 1000), 250_000);
    }

    #[test]
    fn rate_windows() {
        let r = Rate::new();
        // The first sample is the baseline.
        assert_eq!(r.sample(5_000, 0), (0, 0));
        assert_eq!(r.sample(5_100, 1000), (100, 100));
        // Too short: the previous rate, the delta not consumed.
        assert_eq!(r.sample(5_150, 10), (50, 100));
        assert_eq!(r.sample(5_400, 500), (300, 600));
        // Counter wrap.
        let w = Rate::new();
        w.sample(u32::MAX - 9, 0);
        assert_eq!(w.sample(10, 1000).0, 20);
    }

    #[test]
    fn census_counts_and_overflows() {
        let c: Census<2> = Census::new();
        c.note(7);
        c.note(7);
        c.note(9);
        c.note(11);
        c.note(0);
        assert_eq!(c.slot(0), (7, 2));
        assert_eq!(c.slot(1), (9, 1));
        assert_eq!(c.slot(2), (0, 0));
        assert_eq!(c.overflow(), 2);
        c.reset();
        assert_eq!(c.slot(0), (0, 0));
        assert_eq!(c.overflow(), 0);
        c.note(11);
        assert_eq!(c.slot(0), (11, 1));
    }

    #[test]
    fn foreign_unchanged() {
        let pfns = [10u64, 11, 12];
        let piece = MappingPiece { va: 0x10_000, memory_offset: 0, len: 3 * 4096 };
        let same: [(MappingPiece, Option<&[u64]>); 1] = [(piece, Some(&pfns))];
        assert!(foreign_update_unchanged(&same, 0x10_000, 3 * 4096, &pfns));
        // Other pages.
        assert!(!foreign_update_unchanged(&same, 0x10_000, 3 * 4096, &[10, 11, 13]));
        // Other range.
        assert!(!foreign_update_unchanged(&same, 0x11_000, 3 * 4096, &pfns));
        assert!(!foreign_update_unchanged(&same, 0x10_000, 2 * 4096, &pfns[..2]));
        // Length does not match the pages.
        assert!(!foreign_update_unchanged(&same, 0x10_000, 4 * 4096, &pfns));
        // Pages not kept.
        let unknown: [(MappingPiece, Option<&[u64]>); 1] = [(piece, None)];
        assert!(!foreign_update_unchanged(&unknown, 0x10_000, 3 * 4096, &pfns));
        // Nothing mapped, or two mappings.
        assert!(!foreign_update_unchanged(&[], 0x10_000, 3 * 4096, &pfns));
        let two = [same[0], same[0]];
        assert!(!foreign_update_unchanged(&two, 0x10_000, 3 * 4096, &pfns));
        // An unmap (no pages) is never "unchanged".
        assert!(!foreign_update_unchanged(&same, 0x10_000, 0, &[]));
        // A descriptor offset other than 0.
        let shifted = [(MappingPiece { memory_offset: 4096, ..piece }, Some(&pfns[..]))];
        assert!(!foreign_update_unchanged(&shifted, 0x10_000, 3 * 4096, &pfns));
    }
}

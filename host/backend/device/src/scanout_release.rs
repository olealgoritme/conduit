//! Which scanout buffers a display client may still be reading, and when the
//! guest may have one back (`ScanoutReleased`, docs/SCANOUT.md "Buffer
//! release").
//!
//! A buffer the guest flipped stays in use while it is the one on the scanout
//! and, after a later flip replaced it, until every display client that was
//! sent it is done with it:
//!
//! - a client that declared `CAP_RELEASE_SEQ` says so itself with
//!   `EV_RELEASE` (the buffer's dma-buf inode, and the `seq` of the newest
//!   ATTACH of it the release covers: an older release crossing a newer send
//!   on the socket does not count for the newer one);
//! - any other client (an older viewer or stream host) is taken to be done
//!   with a buffer once it has been sent a different one, as before this
//!   existed.
//!
//! A buffer no client was sent is released as soon as it is replaced, and a
//! client that never answers delays a release by at most the timeout.
//!
//! Pure bookkeeping: [`crate::display::DisplayLink`] feeds it under its lock
//! and delivers what [`ReleaseTracker::take`] hands out.

use protocol::messages::{
    SCANOUT_RELEASED_FORCED, SCANOUT_RELEASED_NOT_SHOWN, SCANOUT_RELEASED_RESOURCE, ScanoutReleased,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A scanout buffer as the guest names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BufKey {
    /// A `ScanoutFlip` buffer: a GEM handle of a drm file the guest opened.
    Gem { owner: u32, handle: u32 },
    /// A Venus resource shown with `SET_SCANOUT_BLOB`.
    Resource(u32),
}

/// One client's hold on a buffer: the last ATTACH of it this client was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hold {
    inode: u64,
    stamp: u32,
    /// The client reports releases itself (`CAP_RELEASE_SEQ`).
    reports: bool,
}

#[derive(Debug)]
struct Tracked {
    key: BufKey,
    /// `ScanoutFlip::seq` of its most recent flip (0 for Venus).
    seq: u64,
    /// When a later flip (or a disable) replaced it; `None` while it is the
    /// buffer on the scanout.
    replaced: Option<Instant>,
    /// Some client was sent its most recent flip.
    shown: bool,
    /// Per client index.
    holds: Vec<Option<Hold>>,
}

impl Tracked {
    fn held(&self) -> bool {
        self.holds.iter().any(Option::is_some)
    }
}

/// At most this many buffers are tracked; past it the oldest replaced one is
/// released by force (a guest flipping through more distinct buffers than any
/// swapchain has, with a client that never releases).
const MAX_TRACKED: usize = 32;
/// Undelivered releases kept for a guest that has no event buffer posted.
const MAX_QUEUED: usize = 64;

pub struct ReleaseTracker {
    enabled: bool,
    timeout: Duration,
    bufs: Vec<Tracked>,
    out: VecDeque<ScanoutReleased>,
    pub forced: u64,
    pub released: u64,
}

impl Default for ReleaseTracker {
    fn default() -> Self {
        Self::new(Duration::from_millis(u64::from(
            protocol::messages::SCANOUT_RELEASE_TIMEOUT_MS,
        )))
    }
}

impl ReleaseTracker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            enabled: false,
            timeout,
            bufs: Vec::new(),
            out: VecDeque::new(),
            forced: 0,
            released: 0,
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The guest acked `NVGPU_F_SCANOUT_RELEASE` (or a new device start did
    /// not). Turning it off forgets everything.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.bufs.clear();
            self.out.clear();
        }
    }

    fn find(&mut self, key: BufKey) -> Option<&mut Tracked> {
        self.bufs.iter_mut().find(|t| t.key == key)
    }

    /// The guest flipped `key` (`None`: something that is not a guest buffer,
    /// the boot console). Whatever was on the scanout is replaced.
    pub fn flipped(&mut self, key: Option<BufKey>, seq: u64, now: Instant) {
        if !self.enabled {
            return;
        }
        for t in self.bufs.iter_mut() {
            if t.replaced.is_none() && Some(t.key) != key {
                t.replaced = Some(now);
            }
        }
        let Some(key) = key else { return };
        match self.find(key) {
            Some(t) => {
                t.seq = seq;
                t.replaced = None;
                t.shown = false;
            }
            None => self.bufs.push(Tracked {
                key,
                seq,
                replaced: None,
                shown: false,
                holds: Vec::new(),
            }),
        }
    }

    /// The scanout was turned off: the buffer on it is replaced by nothing.
    pub fn disabled(&mut self, now: Instant) {
        for t in self.bufs.iter_mut() {
            if t.replaced.is_none() {
                t.replaced = Some(now);
            }
        }
    }

    /// Client `client` was sent `key`'s current flip as the dma-buf with
    /// inode `inode`, in an ATTACH stamped `stamp`.
    pub fn sent(
        &mut self,
        client: usize,
        key: Option<BufKey>,
        inode: u64,
        stamp: u32,
        reports: bool,
    ) {
        if !self.enabled {
            return;
        }
        for t in self.bufs.iter_mut() {
            if Some(t.key) == key {
                if t.holds.len() <= client {
                    t.holds.resize(client + 1, None);
                }
                t.holds[client] = Some(Hold {
                    inode,
                    stamp,
                    reports,
                });
                t.shown = true;
            } else if let Some(h) = t.holds.get_mut(client)
                && h.is_some_and(|h| !h.reports)
            {
                // A client that does not report is done with what it had
                // once it has something else.
                *h = None;
            }
        }
    }

    /// Client `client` released the buffer with inode `inode`, covering its
    /// ATTACHes up to the one stamped `stamp`.
    pub fn released(&mut self, client: usize, inode: u64, stamp: u32) {
        for t in self.bufs.iter_mut() {
            if let Some(h) = t.holds.get_mut(client)
                && h.is_some_and(|h| h.inode == inode && h.stamp == stamp)
            {
                *h = None;
            }
        }
    }

    /// Client `client` disconnected or went idle: it reads nothing any more.
    pub fn client_gone(&mut self, client: usize) {
        for t in self.bufs.iter_mut() {
            if let Some(h) = t.holds.get_mut(client) {
                *h = None;
            }
        }
    }

    /// Stop tracking buffers the guest closed (a GEM handle, a file, a
    /// resource): nobody waits for those, and the name may be reused.
    pub fn forget(&mut self, gone: impl Fn(BufKey) -> bool) {
        self.bufs.retain(|t| !gone(t.key));
        self.out.retain(|r| {
            let key = if r.flags & SCANOUT_RELEASED_RESOURCE != 0 {
                BufKey::Resource(r.host_handle)
            } else {
                BufKey::Gem {
                    owner: r.owner_handle,
                    handle: r.host_handle,
                }
            };
            !gone(key)
        });
    }

    /// Move every buffer that is free now onto the outgoing queue. Returns
    /// whether anything was added.
    pub fn collect(&mut self, now: Instant) -> bool {
        if !self.enabled || self.bufs.is_empty() {
            return false;
        }
        let before = self.out.len();
        let over = self.bufs.len().saturating_sub(MAX_TRACKED);
        let mut forced_left = over;
        let timeout = self.timeout;
        let mut i = 0;
        while i < self.bufs.len() {
            let t = &self.bufs[i];
            let Some(at) = t.replaced else {
                i += 1;
                continue;
            };
            let late = now.saturating_duration_since(at) >= timeout;
            let force = forced_left > 0;
            if t.held() && !late && !force {
                i += 1;
                continue;
            }
            let t = self.bufs.remove(i);
            let mut flags = 0;
            if !t.shown {
                flags |= SCANOUT_RELEASED_NOT_SHOWN;
            }
            if t.held() {
                flags |= SCANOUT_RELEASED_FORCED;
                self.forced += 1;
                if force {
                    forced_left -= 1;
                }
            }
            let (owner_handle, host_handle, seq) = match t.key {
                BufKey::Gem { owner, handle } => (owner, handle, t.seq),
                BufKey::Resource(id) => {
                    flags |= SCANOUT_RELEASED_RESOURCE;
                    (0, id, 0)
                }
            };
            self.released += 1;
            self.push(ScanoutReleased {
                scanout: 0,
                flags,
                owner_handle,
                host_handle,
                seq,
                reserved: 0,
            });
        }
        self.out.len() != before
    }

    fn push(&mut self, r: ScanoutReleased) {
        // A newer release of the same buffer replaces one not yet delivered.
        self.out.retain(|o| {
            !(o.flags & SCANOUT_RELEASED_RESOURCE == r.flags & SCANOUT_RELEASED_RESOURCE
                && o.owner_handle == r.owner_handle
                && o.host_handle == r.host_handle)
        });
        if self.out.len() >= MAX_QUEUED {
            self.out.pop_front();
        }
        self.out.push_back(r);
    }

    /// When the next forced release is due, if any buffer waits for one.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.bufs
            .iter()
            .filter(|t| t.held())
            .filter_map(|t| t.replaced)
            .min()
            .map(|at| at + self.timeout)
    }

    /// Releases waiting for delivery.
    pub fn pending(&self) -> bool {
        !self.out.is_empty()
    }

    pub fn take(&mut self) -> Vec<ScanoutReleased> {
        self.out.drain(..).collect()
    }

    /// Put back what could not be delivered, ahead of anything newer.
    pub fn untake(&mut self, rest: Vec<ScanoutReleased>) {
        for r in rest.into_iter().rev() {
            if self.out.len() >= MAX_QUEUED {
                break;
            }
            self.out.push_front(r);
        }
    }

    /// Buffers tracked, for tests.
    pub fn tracked(&self) -> usize {
        self.bufs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gem(h: u32) -> Option<BufKey> {
        Some(BufKey::Gem {
            owner: 3,
            handle: h,
        })
    }

    fn on() -> ReleaseTracker {
        let mut t = ReleaseTracker::new(Duration::from_millis(500));
        t.set_enabled(true);
        t
    }

    fn handles(t: &mut ReleaseTracker) -> Vec<(u32, u32)> {
        t.take().iter().map(|r| (r.host_handle, r.flags)).collect()
    }

    #[test]
    fn off_tracks_nothing() {
        let mut t = ReleaseTracker::new(Duration::from_millis(500));
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.flipped(gem(2), 2, now);
        assert!(!t.collect(now));
        assert_eq!(t.tracked(), 0);
    }

    #[test]
    fn with_no_client_the_replaced_buffer_goes_at_the_next_flip() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        assert!(
            !t.collect(now),
            "the buffer on the scanout is never released"
        );
        t.flipped(gem(2), 2, now);
        assert!(t.collect(now));
        let r = t.take();
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].owner_handle, r[0].host_handle, r[0].seq), (3, 1, 1));
        assert_eq!(r[0].flags, SCANOUT_RELEASED_NOT_SHOWN);
    }

    #[test]
    fn a_reporting_client_releases_with_the_stamp_of_its_newest_attach() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.sent(0, gem(1), 100, 10, true);
        t.flipped(gem(2), 2, now);
        t.sent(0, gem(2), 200, 11, true);
        assert!(!t.collect(now), "still read by the client");
        // A stale release (an older stamp) does not count.
        t.released(0, 100, 9);
        assert!(!t.collect(now));
        t.released(0, 100, 10);
        assert!(t.collect(now));
        assert_eq!(handles(&mut t), vec![(1, 0)]);
    }

    #[test]
    fn a_release_crossing_a_newer_send_is_ignored() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.sent(0, gem(1), 100, 10, true);
        t.flipped(gem(2), 2, now);
        t.sent(0, gem(2), 200, 11, true);
        t.flipped(gem(1), 3, now);
        t.sent(0, gem(1), 100, 12, true);
        // The client's release of the first send arrives now.
        t.released(0, 100, 10);
        t.flipped(gem(2), 4, now);
        t.sent(0, gem(2), 200, 13, true);
        assert!(
            !t.collect(now),
            "buffer 1 still held by send 12; 2 is current"
        );
        t.released(0, 100, 12);
        assert!(t.collect(now));
        let r = t.take();
        assert_eq!((r[0].host_handle, r[0].seq), (1, 3));
    }

    #[test]
    fn a_silent_client_is_done_once_it_has_another_buffer() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.sent(0, gem(1), 100, 10, false);
        t.flipped(gem(2), 2, now);
        assert!(!t.collect(now), "not sent the next one yet (socket full)");
        t.sent(0, gem(2), 200, 11, false);
        assert!(t.collect(now));
        assert_eq!(handles(&mut t), vec![(1, 0)]);
    }

    #[test]
    fn every_client_must_be_done() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.sent(0, gem(1), 100, 10, true);
        t.sent(1, gem(1), 100, 10, true);
        t.flipped(gem(2), 2, now);
        t.sent(0, gem(2), 200, 11, true);
        t.sent(1, gem(2), 200, 11, true);
        t.released(0, 100, 10);
        assert!(!t.collect(now));
        t.released(1, 100, 10);
        assert!(t.collect(now));
        assert_eq!(handles(&mut t), vec![(1, 0)]);
    }

    #[test]
    fn a_client_that_leaves_holds_nothing() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.sent(1, gem(1), 100, 10, true);
        t.flipped(gem(2), 2, now);
        assert!(!t.collect(now));
        t.client_gone(1);
        assert!(t.collect(now));
        assert_eq!(handles(&mut t), vec![(1, 0)]);
    }

    #[test]
    fn a_stuck_client_is_overruled_after_the_timeout() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.sent(0, gem(1), 100, 10, true);
        t.flipped(gem(2), 2, now);
        assert_eq!(t.next_deadline(), Some(now + Duration::from_millis(500)));
        assert!(!t.collect(now + Duration::from_millis(499)));
        assert!(t.collect(now + Duration::from_millis(500)));
        assert_eq!(handles(&mut t), vec![(1, SCANOUT_RELEASED_FORCED)]);
        assert_eq!(t.forced, 1);
        assert_eq!(t.next_deadline(), None);
    }

    #[test]
    fn disable_and_the_console_replace_the_current_buffer() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.disabled(now);
        assert!(t.collect(now));
        t.flipped(gem(2), 2, now);
        t.flipped(None, 0, now);
        assert!(t.collect(now));
        assert_eq!(
            t.take().iter().map(|r| r.host_handle).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn venus_resources_are_named_by_id() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(Some(BufKey::Resource(77)), 0, now);
        t.flipped(Some(BufKey::Resource(78)), 0, now);
        assert!(t.collect(now));
        let r = t.take();
        assert_eq!(
            (r[0].owner_handle, r[0].host_handle, r[0].flags),
            (
                0,
                77,
                SCANOUT_RELEASED_RESOURCE | SCANOUT_RELEASED_NOT_SHOWN
            )
        );
    }

    #[test]
    fn closed_buffers_are_forgotten_and_undelivered_ones_requeue_in_order() {
        let mut t = on();
        let now = Instant::now();
        t.flipped(gem(1), 1, now);
        t.flipped(gem(2), 2, now);
        t.flipped(gem(3), 3, now);
        t.forget(|k| k == gem(2).unwrap());
        assert!(t.collect(now));
        let r = t.take();
        assert_eq!(r.iter().map(|r| r.host_handle).collect::<Vec<_>>(), vec![1]);
        t.flipped(gem(4), 4, now);
        t.collect(now);
        t.untake(r);
        assert_eq!(
            t.take().iter().map(|r| r.host_handle).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn tracking_is_bounded() {
        let mut t = on();
        let now = Instant::now();
        for h in 0..100 {
            t.flipped(gem(h), h as u64, now);
            t.sent(0, gem(h), h as u64, h, true);
            t.collect(now);
        }
        assert!(t.tracked() <= MAX_TRACKED + 1);
        assert!(t.forced > 0);
    }
}

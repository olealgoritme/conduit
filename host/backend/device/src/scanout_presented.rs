//! Which guest flip a display client's presentation report names, and the
//! `ScanoutPresented` event it becomes (docs/SCANOUT.md "Presentation
//! feedback").
//!
//! Every frame the backend sends a client is an ATTACH stamped with the
//! backend's `CLOCK_MONOTONIC` microseconds, one stamp per client per send. A
//! client that declared `CAP_PRESENTED` answers the commit with `EV_PRESENTED`
//! carrying that stamp once the compositor put the frame on the screen
//! (Wayland `wp_presentation_feedback.presented`). This book remembers, per
//! send, which guest flip the stamp belongs to, so the report can be handed to
//! the guest as the flip it completes.
//!
//! At most one event per guest flip: the first report of any client wins, a
//! later one for the same or an older flip is dropped. A flip no client
//! reports (none connected, or the compositor replaced it before the screen
//! showed it) produces nothing; the guest then completes it on its own timer.
//!
//! Pure bookkeeping, like [`crate::scanout_release`]: [`crate::display::DisplayLink`]
//! feeds it under its lock.

use crate::scanout_release::BufKey;
use protocol::messages::{
    SCANOUT_PRESENTED_RESOURCE, SCANOUT_PRESENTED_TIMED, SCANOUT_PRESENTED_VSYNC,
    SCANOUT_PRESENTED_ZERO_COPY, ScanoutPresented,
};
use std::collections::VecDeque;

/// `wp_presentation_feedback` kind bits as the client passes them in
/// `EV_PRESENTED`'s `y`.
pub const KIND_VSYNC: u32 = 0x1;
pub const KIND_ZERO_COPY: u32 = 0x8;

/// Sends remembered. A report comes back one or two refresh periods after the
/// send; this is many frames of slack for two clients.
const MAX_SENT: usize = 64;

#[derive(Clone, Copy, Debug)]
struct Sent {
    client: usize,
    stamp: u32,
    /// The link's number of the guest flip this send carried.
    frame: u64,
    key: BufKey,
    seq: u64,
}

#[derive(Default)]
pub struct PresentedTracker {
    enabled: bool,
    sent: VecDeque<Sent>,
    /// Number of the guest flip on the scanout now (0: none yet).
    frame: u64,
    /// The guest's name for it (`None`: the console, or nothing).
    key: Option<BufKey>,
    seq: u64,
    /// Highest flip number already reported to the guest.
    reported: u64,
    /// Reports that named no remembered send, or a flip already reported.
    pub stale: u64,
}

impl PresentedTracker {
    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The guest acked `NVGPU_F_SCANOUT_PRESENTED` (or a device start did
    /// not). Turning it off forgets everything.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.sent.clear();
        self.key = None;
        self.seq = 0;
        self.reported = self.frame;
    }

    /// The guest flipped `key` (`None`: not a guest buffer).
    pub fn flipped(&mut self, key: Option<BufKey>, seq: u64) {
        if !self.enabled {
            return;
        }
        self.frame += 1;
        self.key = key;
        self.seq = seq;
    }

    /// The current frame went to client `i` stamped `stamp`.
    pub fn sent(&mut self, i: usize, stamp: u32) {
        if !self.enabled {
            return;
        }
        let Some(key) = self.key else { return };
        if self.sent.len() == MAX_SENT {
            self.sent.pop_front();
        }
        self.sent.push_back(Sent {
            client: i,
            stamp,
            frame: self.frame,
            key,
            seq: self.seq,
        });
    }

    /// Client `i` reports that its frame stamped `stamp` reached the screen
    /// (`kind`: the presentation feedback kind bits; `present_ns`: when, on
    /// `CLOCK_MONOTONIC`, 0 unknown). The event for the guest, if this is
    /// the first report of a flip newer than the last one reported.
    pub fn presented(
        &mut self,
        i: usize,
        stamp: u32,
        kind: u32,
        present_ns: u64,
        now_ns: u64,
    ) -> Option<ScanoutPresented> {
        if !self.enabled {
            return None;
        }
        let Some(at) = self
            .sent
            .iter()
            .rposition(|s| s.client == i && s.stamp == stamp)
        else {
            self.stale += 1;
            return None;
        };
        let s = self.sent[at];
        // This client's older sends can no longer be reported in order.
        self.sent.retain(|o| o.client != i || o.frame > s.frame);
        if s.frame <= self.reported {
            self.stale += 1;
            return None;
        }
        self.reported = s.frame;
        let mut flags = 0;
        if kind & KIND_VSYNC != 0 {
            flags |= SCANOUT_PRESENTED_VSYNC;
        }
        if kind & KIND_ZERO_COPY != 0 {
            flags |= SCANOUT_PRESENTED_ZERO_COPY;
        }
        if present_ns != 0 {
            flags |= SCANOUT_PRESENTED_TIMED;
        }
        let (owner_handle, host_handle, seq) = match s.key {
            BufKey::Gem { owner, handle } => (owner, handle, s.seq),
            BufKey::Resource(id) => {
                flags |= SCANOUT_PRESENTED_RESOURCE;
                (0, id, 0)
            }
        };
        Some(ScanoutPresented {
            scanout: 0,
            flags,
            owner_handle,
            host_handle,
            seq,
            present_ns,
            sent_ns: now_ns,
            reserved: 0,
        })
    }

    /// Client `i` is gone (or idle): its sends will not be reported.
    pub fn client_gone(&mut self, i: usize) {
        self.sent.retain(|s| s.client != i);
    }

    /// Sends remembered (tests).
    pub fn remembered(&self) -> usize {
        self.sent.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: BufKey = BufKey::Gem {
        owner: 3,
        handle: 10,
    };
    const B: BufKey = BufKey::Gem {
        owner: 3,
        handle: 11,
    };

    fn on() -> PresentedTracker {
        let mut t = PresentedTracker::default();
        t.set_enabled(true);
        t
    }

    #[test]
    fn off_it_remembers_and_reports_nothing() {
        let mut t = PresentedTracker::default();
        t.flipped(Some(A), 1);
        t.sent(0, 100);
        assert_eq!(t.remembered(), 0);
        assert_eq!(t.presented(0, 100, KIND_VSYNC, 5, 6), None);
    }

    #[test]
    fn a_report_names_the_flip_its_stamp_carried() {
        let mut t = on();
        t.flipped(Some(A), 7);
        t.sent(0, 100);
        t.flipped(Some(B), 8);
        t.sent(0, 200);
        let p = t
            .presented(0, 100, KIND_VSYNC | KIND_ZERO_COPY, 5_000, 6_000)
            .unwrap();
        assert_eq!((p.owner_handle, p.host_handle, p.seq), (3, 10, 7));
        assert_eq!(
            p.flags,
            SCANOUT_PRESENTED_VSYNC | SCANOUT_PRESENTED_ZERO_COPY | SCANOUT_PRESENTED_TIMED
        );
        assert_eq!((p.present_ns, p.sent_ns), (5_000, 6_000));
        let p = t.presented(0, 200, 0, 0, 7_000).unwrap();
        assert_eq!((p.host_handle, p.seq, p.flags), (11, 8, 0));
    }

    #[test]
    fn one_event_per_flip_whoever_reports_first() {
        let mut t = on();
        t.flipped(Some(A), 7);
        t.sent(0, 100);
        t.sent(1, 101);
        assert!(t.presented(1, 101, KIND_VSYNC, 1, 2).is_some());
        assert_eq!(t.presented(0, 100, KIND_VSYNC, 1, 2), None);
        assert_eq!(t.stale, 1);
    }

    #[test]
    fn an_older_flip_reported_late_is_dropped() {
        let mut t = on();
        t.flipped(Some(A), 7);
        t.sent(0, 100);
        t.sent(1, 101);
        t.flipped(Some(B), 8);
        t.sent(1, 201);
        assert_eq!(t.presented(1, 201, 0, 0, 0).unwrap().seq, 8);
        // Client 0 shows flip 7 only now: the guest already had 8.
        assert_eq!(t.presented(0, 100, 0, 0, 0), None);
    }

    #[test]
    fn a_venus_resource_is_named_by_its_id() {
        let mut t = on();
        t.flipped(Some(BufKey::Resource(42)), 0);
        t.sent(0, 5);
        let p = t.presented(0, 5, 0, 0, 0).unwrap();
        assert_eq!(
            (p.flags, p.owner_handle, p.host_handle, p.seq),
            (SCANOUT_PRESENTED_RESOURCE, 0, 42, 0)
        );
    }

    #[test]
    fn the_console_and_unknown_stamps_report_nothing() {
        let mut t = on();
        t.flipped(None, 0);
        t.sent(0, 5);
        assert_eq!(t.remembered(), 0);
        assert_eq!(t.presented(0, 5, 0, 0, 0), None);
        assert_eq!(t.presented(0, 999, 0, 0, 0), None);
    }

    #[test]
    fn a_gone_client_and_a_restart_forget_their_sends() {
        let mut t = on();
        t.flipped(Some(A), 7);
        t.sent(0, 100);
        t.sent(1, 101);
        t.client_gone(0);
        assert_eq!(t.remembered(), 1);
        assert_eq!(t.presented(0, 100, 0, 0, 0), None);
        t.set_enabled(true);
        assert_eq!(t.remembered(), 0);
        assert_eq!(t.presented(1, 101, 0, 0, 0), None);
        // The next flip after a restart is reported again.
        t.flipped(Some(B), 1);
        t.sent(1, 300);
        assert!(t.presented(1, 300, 0, 0, 0).is_some());
    }

    #[test]
    fn the_book_is_bounded() {
        let mut t = on();
        for n in 0..(MAX_SENT as u32 + 10) {
            t.flipped(Some(A), u64::from(n) + 1);
            t.sent(0, n);
        }
        assert_eq!(t.remembered(), MAX_SENT);
        assert_eq!(t.presented(0, 0, 0, 0, 0), None);
        assert!(t.presented(0, MAX_SENT as u32 + 9, 0, 0, 0).is_some());
    }
}

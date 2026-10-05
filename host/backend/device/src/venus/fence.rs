//! Fenced commands (docs/VENUS.md "Fences"): a command with `FLAG_FENCE`
//! completes only once the renderer has signalled its fence, as QEMU does.
//!
//! Only a command that would answer `RESP_OK_NODATA` waits. One that failed,
//! or that answers with data (`GET_CAPSET`, `RESOURCE_MAP_BLOB`), is
//! answered at once, which is what QEMU does with them too.
//!
//! Fence ids are the guest's, passed through unchanged, and need not
//! increase: 0 after 1000 is fine. The renderer retires the fences of one
//! `(ctx_id, ring)` timeline in submission order and reports every one, so
//! a signal completes that timeline's held commands in submission order up
//! to and including the first one with the signalled id -- never by
//! comparing ids.

use super::*;
use conduit_venus::Signalled;
use std::time::{Duration, Instant};

/// How often the fence latency summary is logged while fences flow.
const LATENCY_REPORT: Duration = Duration::from_secs(2);

/// A response for a chain that was [`Outcome::Held`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub token: u64,
    /// `MsgHeader` and the virtio-gpu response.
    pub resp: Vec<u8>,
}

/// A fenced command waiting for its fence.
struct Held {
    token: u64,
    /// The request's header: the response echoes its fence.
    hdr: CtrlHdr,
    /// When the backend held it: the fence latency is measured from here to
    /// the renderer's signal reaching the backend.
    at: Instant,
}

/// Submit-to-signal latencies seen since the last summary, in microseconds,
/// per `(ctx_id, ring)`.
#[derive(Default)]
struct Latency {
    samples: Vec<(u32, u32, u32)>,
    since: Option<Instant>,
}

impl Latency {
    fn add(&mut self, ctx: u32, ring: u32, held: Duration) {
        let us = held.as_micros().min(u32::MAX as u128) as u32;
        self.samples.push((ctx, ring, us));
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= LATENCY_REPORT {
            self.report();
        }
    }

    /// One info line per timeline: count, median, 90th percentile and
    /// maximum submit-to-signal time.
    fn report(&mut self) {
        let mut by: std::collections::BTreeMap<(u32, u32), Vec<u32>> = Default::default();
        for (c, r, us) in self.samples.drain(..) {
            by.entry((c, r)).or_default().push(us);
        }
        for ((c, r), mut v) in by {
            v.sort_unstable();
            let at = |q: usize| v[(v.len() - 1) * q / 100];
            log::info!(
                "venus: fence latency ctx {c} ring {r}: {} fences, p50 {} us, p90 {} us, max {} us",
                v.len(),
                at(50),
                at(90),
                v[v.len() - 1]
            );
        }
        self.since = None;
    }
}

#[derive(Default)]
pub(super) struct Fences {
    held: Vec<Held>,
    ready: Vec<Completion>,
    next_token: u64,
    latency: Latency,
}

impl Fences {
    /// Hold the command `hdr` begins; the token names its chain.
    pub(super) fn hold(&mut self, hdr: &CtrlHdr) -> u64 {
        self.next_token += 1;
        self.held.push(Held {
            token: self.next_token,
            hdr: *hdr,
            at: Instant::now(),
        });
        self.next_token
    }

    /// The renderer signalled `s`: on its timeline, every held command up to
    /// and including the oldest with its id is answered `RESP_OK_NODATA`.
    /// A signal matching nothing held (a fence dropped by a reset) is ignored.
    pub(super) fn signal(&mut self, s: Signalled) {
        let on = |h: &Held| h.hdr.ctx_id == s.ctx_id && h.hdr.ring() == s.ring_idx;
        let Some(last) = self
            .held
            .iter()
            .position(|h| on(h) && h.hdr.fence_id == s.fence_id)
        else {
            log::debug!(
                "venus: fence {} on ctx {} ring {} signalled, nothing held for it",
                s.fence_id,
                s.ctx_id,
                s.ring_idx
            );
            return;
        };
        let (done, keep) = std::mem::take(&mut self.held)
            .into_iter()
            .enumerate()
            .partition::<Vec<_>, _>(|(i, h)| *i <= last && on(h));
        self.held = keep.into_iter().map(|(_, h)| h).collect();
        for (_, h) in done {
            self.latency.add(h.hdr.ctx_id, h.hdr.ring(), h.at.elapsed());
            self.complete(h, RESP_OK_NODATA);
        }
    }

    /// Every held command answered `RESP_ERR_UNSPEC`: the renderer is gone,
    /// or the device is being reset.
    pub(super) fn fail_all(&mut self) {
        for h in std::mem::take(&mut self.held) {
            self.complete(h, RESP_ERR_UNSPEC);
        }
    }

    fn complete(&mut self, h: Held, ty: u32) {
        let mut resp = vec![0u8; size_of::<MsgHeader>() + CTRL_HDR_LEN];
        let n = reply(&mut resp, &h.hdr.response(ty), &[]);
        debug_assert_eq!(n, resp.len());
        self.ready.push(Completion {
            token: h.token,
            resp,
        });
    }

    pub(super) fn take_ready(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.ready)
    }

    pub(super) fn held(&self) -> usize {
        self.held.len()
    }
}

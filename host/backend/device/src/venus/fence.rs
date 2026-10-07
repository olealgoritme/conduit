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
use conduit_venus::latency::{Summary, Window};
use std::time::Instant;

/// A response for a chain that was [`Outcome::Held`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub token: u64,
    /// `MsgHeader` and the virtio-gpu response.
    pub resp: Vec<u8>,
    /// The fence it waited for, `(ctx_id, ring, fence_id)`: the frame id
    /// stage timing follows it by (`crate::stage`).
    pub fence: (u32, u32, u64),
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

/// One info line per `(ctx_id, ring)` of a closed latency window: count
/// over the window's span, median, 90th percentile and maximum
/// submit-to-signal time.
fn log_latency(s: &Summary<(u32, u32)>) {
    for ((c, r), st) in &s.by_key {
        log::info!(
            "venus: fence latency ctx {c} ring {r}: {} fences in {:.1} s, p50 {} us, p90 {} us, max {} us",
            st.count,
            s.span.as_secs_f64(),
            st.p50,
            st.p90,
            st.max
        );
    }
}

#[derive(Default)]
pub(super) struct Fences {
    held: Vec<Held>,
    ready: Vec<Completion>,
    next_token: u64,
    /// Submit-to-signal latencies per `(ctx_id, ring)`.
    latency: Window<(u32, u32)>,
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
        let now = Instant::now();
        if crate::stage::on() {
            let ns = crate::stage::now_ns();
            for (_, h) in &done {
                crate::stage::stamp(conduit_venus::stage::Rec::fence(
                    conduit_venus::stage::H_SIGNALLED,
                    h.hdr.ctx_id,
                    h.hdr.ring(),
                    h.hdr.fence_id,
                    ns,
                ));
            }
        }
        for (_, h) in done {
            let held = now.saturating_duration_since(h.at);
            if let Some(s) = self.latency.add((h.hdr.ctx_id, h.hdr.ring()), held, now) {
                log_latency(&s);
            }
            self.complete(h, RESP_OK_NODATA);
        }
    }

    /// Log the latency window if it is over. Called whenever the transport
    /// asks for completions (the fence pump does at least every 100 ms), so
    /// the line comes on time when fences stop; nothing while idle.
    pub(super) fn flush_latency(&mut self) {
        if let Some(s) = self.latency.flush(Instant::now()) {
            log_latency(&s);
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
            fence: (h.hdr.ctx_id, h.hdr.ring(), h.hdr.fence_id),
        });
    }

    pub(super) fn take_ready(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.ready)
    }

    pub(super) fn held(&self) -> usize {
        self.held.len()
    }
}

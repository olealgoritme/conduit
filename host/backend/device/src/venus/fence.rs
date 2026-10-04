//! Fenced commands (docs/VENUS.md "Fences"): a command with `FLAG_FENCE`
//! completes only once the renderer has signalled its fence, as QEMU does.
//!
//! Only a command that would answer `RESP_OK_NODATA` waits. One that failed,
//! or that answers with data (`GET_CAPSET`, `RESOURCE_MAP_BLOB`), is
//! answered at once, which is what QEMU does with them too.
//!
//! Fences on one `(ctx_id, ring)` timeline signal in order, so a signalled
//! fence completes every held command on that timeline at or before it --
//! the renderer may report only the newest.

use super::*;
use conduit_venus::Signalled;

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
}

#[derive(Default)]
pub(super) struct Fences {
    held: Vec<Held>,
    ready: Vec<Completion>,
    next_token: u64,
}

impl Fences {
    /// Hold the command `hdr` begins; the token names its chain.
    pub(super) fn hold(&mut self, hdr: &CtrlHdr) -> u64 {
        self.next_token += 1;
        self.held.push(Held {
            token: self.next_token,
            hdr: *hdr,
        });
        self.next_token
    }

    /// The renderer signalled `s`: everything at or before it on its
    /// timeline is answered `RESP_OK_NODATA`.
    pub(super) fn signal(&mut self, s: Signalled) {
        let mut i = 0;
        while i < self.held.len() {
            let h = &self.held[i].hdr;
            if h.ctx_id == s.ctx_id && h.ring() == s.ring_idx && h.fence_id <= s.fence_id {
                let h = self.held.remove(i);
                self.complete(h, RESP_OK_NODATA);
            } else {
                i += 1;
            }
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

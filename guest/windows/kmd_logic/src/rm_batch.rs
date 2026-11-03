//! `RmBatch` (host message 37): several RM `Ioctl`s in one virtio round trip.
//!
//! The guest half of `host/backend/device/src/nvidia/batch.rs`; the layout is
//! `host/backend/protocol/src/messages.rs` (`RmBatchReq`, `RmBatchEntry`,
//! `RmBatchResp`, `RmBatchReply`), and the numbers here are asserted equal to
//! those in this crate's tests and in the host's.
//!
//! Request: `MsgHeader { 37, handle 0 }`, `{ count, flags 0 }`, then per entry
//! `{ req_len, resp_cap, status_at, flags | group << 16 }` and `req_len` bytes
//! of a whole `Ioctl` request (`MsgHeader | IoctlReq | data | nested | deep`),
//! zero-padded to 8. Reply: `MsgHeader`, `{ count, served }`, then per entry
//! `{ len, rm_status, flags, 0 }` and a slot of `resp_cap` bytes padded to 8,
//! whose first `len` bytes are exactly the reply the call alone would get.
//!
//! The host serves the entries in order through its single-call dispatcher, so
//! every check of a single call applies to each. An entry flagged
//! [`SKIP_GROUP_ON_FAIL`] that fails (host status nonzero, or RM's status word
//! at `status_at` nonzero or out of reach) skips the later entries of its
//! group: a map after a refused virtual object never runs.
//!
//! The KMD's paging path builds batches here (`kmd_render`
//! `virtio/native_memory.rs`, `NvPaging` 0x1) only when the host's config
//! features carry `NVGPU_CFG_RM_BATCH`.

extern crate alloc;

use alloc::vec::Vec;

use crate::rm_client::{self as rc, IOCTL_REQ, MSG_HDR, MSG_IOCTL, REPLY_DATA};

/// `MsgType::RmBatch`.
pub const MSG_RM_BATCH: u32 = 37;
/// `RmBatchReq`.
pub const BATCH_REQ: usize = 8;
/// `RmBatchEntry`.
pub const ENTRY_HDR: usize = 16;
/// `RmBatchResp`.
pub const BATCH_RESP: usize = 8;
/// `RmBatchReply`.
pub const REPLY_HDR: usize = 16;
/// `RM_BATCH_MAX_ENTRIES`.
pub const MAX_ENTRIES: usize = 64;
/// `RM_BATCH_MAX_RESP`: the most reply room one entry may ask for.
pub const MAX_RESP: usize = 4096;
/// `RM_BATCH_NO_STATUS`.
pub const NO_STATUS: u32 = u32::MAX;
/// `RM_BATCH_SKIP_GROUP_ON_FAIL`.
pub const SKIP_GROUP_ON_FAIL: u32 = 1 << 0;
/// `RM_BATCH_ABORT_ON_FAIL`.
pub const ABORT_ON_FAIL: u32 = 1 << 1;
/// `RM_BATCH_REPLY_SERVED`.
pub const REPLY_SERVED: u32 = 1 << 0;
/// `RM_BATCH_REPLY_FAILED`.
pub const REPLY_FAILED: u32 = 1 << 1;

/// What this KMD puts in one batch at most (well under the host's limits and
/// the backend's 64-KiB reply buffer): entries, request bytes, reply bytes.
pub const KMD_MAX_ENTRIES: usize = 48;
pub const KMD_MAX_REQUEST: usize = 32 * 1024;
pub const KMD_MAX_REPLY: usize = 48 * 1024;

/// `n` rounded up to a multiple of 8.
pub const fn pad8(n: usize) -> usize {
    (n + 7) & !7
}

/// The reply room an `Ioctl` entry gets: header, lengths, its data and nested
/// blocks back, and slack, at least 256 bytes and at most [`MAX_RESP`].
pub const fn reply_cap(data: usize, nested: usize) -> usize {
    let want = REPLY_DATA + data + nested + 128;
    let want = if want < 256 { 256 } else { want };
    if want > MAX_RESP {
        MAX_RESP
    } else {
        want
    }
}

/// A point in a batch's building ([`Batch::mark`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    buf: usize,
    entries: usize,
    reply: usize,
}

/// Why an entry could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// The batch is at one of the `KMD_MAX_*` limits: send it and start another.
    Full,
    /// No memory for the bytes.
    NoMemory,
    /// The entry itself is malformed (lengths that do not fit a `u32`).
    Invalid,
}

/// A batch being built.
#[derive(Debug, Default)]
pub struct Batch {
    buf: Vec<u8>,
    caps: Vec<u32>,
    reply: usize,
}

impl Batch {
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            caps: Vec::new(),
            reply: MSG_HDR + BATCH_RESP,
        }
    }

    /// Entries so far.
    pub fn len(&self) -> usize {
        self.caps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    /// The reply room the whole batch needs.
    pub fn reply_len(&self) -> usize {
        self.reply
    }

    /// The reply room of each entry, in order (for [`entry_reply`]).
    pub fn caps(&self) -> &[u32] {
        &self.caps
    }

    /// Whether an entry of `req_len` request bytes and `resp_cap` reply bytes fits.
    pub fn fits(&self, req_len: usize, resp_cap: usize) -> bool {
        let request = self.request_len() + ENTRY_HDR + pad8(req_len);
        let reply = self.reply + REPLY_HDR + pad8(resp_cap);
        self.caps.len() < KMD_MAX_ENTRIES && request <= KMD_MAX_REQUEST && reply <= KMD_MAX_REPLY
    }

    fn request_len(&self) -> usize {
        if self.buf.is_empty() {
            MSG_HDR + BATCH_REQ
        } else {
            self.buf.len()
        }
    }

    /// Add an `Ioctl` of `cmd` on backend file `handle` carrying `data`,
    /// `nested` and, when given, a deep block of kind `deep.0` (the page-run
    /// kinds; `IoctlReq.deep_ptr_offset` carries it). `status_at` is where RM's
    /// status sits in the reply's data block. Returns the entry's index.
    #[allow(clippy::too_many_arguments)]
    pub fn push_ioctl(
        &mut self,
        handle: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        deep: Option<(u32, &[u8])>,
        status_at: Option<usize>,
        group: u16,
        flags: u32,
    ) -> Result<usize, PushError> {
        let deep_len = deep.map_or(0, |(_, d)| d.len());
        let req_len = MSG_HDR + IOCTL_REQ + data.len() + nested.len() + deep_len;
        let resp_cap = reply_cap(data.len(), nested.len());
        if u32::try_from(req_len).is_err() || flags & !(SKIP_GROUP_ON_FAIL | ABORT_ON_FAIL) != 0 {
            return Err(PushError::Invalid);
        }
        if !self.fits(req_len, resp_cap) {
            return Err(PushError::Full);
        }
        let status_at = match status_at {
            Some(at) => u32::try_from(at).map_err(|_| PushError::Invalid)?,
            None => NO_STATUS,
        };
        let entry_bytes = ENTRY_HDR + pad8(req_len);
        let start = self.request_len();
        let grow = start + entry_bytes - self.buf.len();
        self.buf.try_reserve(grow).map_err(|_| PushError::NoMemory)?;
        self.caps.try_reserve(1).map_err(|_| PushError::NoMemory)?;
        if self.buf.is_empty() {
            self.buf.resize(MSG_HDR + BATCH_REQ, 0);
            put32(&mut self.buf, 0, MSG_RM_BATCH);
        }
        self.buf.resize(start + entry_bytes, 0);
        let e = &mut self.buf[start..];
        put32(e, 0, req_len as u32);
        put32(e, 4, resp_cap as u32);
        put32(e, 8, status_at);
        put32(e, 12, flags | (u32::from(group) << 16));
        let r = &mut e[ENTRY_HDR..ENTRY_HDR + req_len];
        rc::build_ioctl(r, handle, cmd, data, nested).ok_or(PushError::Invalid)?;
        if let Some((kind, d)) = deep {
            let body = MSG_HDR + IOCTL_REQ + data.len() + nested.len();
            r[body..body + d.len()].copy_from_slice(d);
            put32(r, MSG_HDR + 16, kind);
            put32(r, MSG_HDR + 20, d.len() as u32);
        }
        let index = self.caps.len();
        self.caps.push(resp_cap as u32);
        self.reply += REPLY_HDR + pad8(resp_cap);
        let count = self.caps.len() as u32;
        put32(&mut self.buf, MSG_HDR, count);
        Ok(index)
    }

    /// Where the batch stands now, for [`Self::rollback`].
    pub fn mark(&self) -> Mark {
        Mark {
            buf: self.buf.len(),
            entries: self.caps.len(),
            reply: self.reply,
        }
    }

    /// Drop every entry added since `mark` (a job whose later entries did not fit
    /// must not be sent half).
    pub fn rollback(&mut self, mark: Mark) {
        if mark.entries >= self.caps.len() {
            return;
        }
        self.caps.truncate(mark.entries);
        self.reply = mark.reply;
        if mark.entries == 0 {
            self.buf.clear();
        } else {
            self.buf.truncate(mark.buf);
            put32(&mut self.buf, MSG_HDR, mark.entries as u32);
        }
    }

    /// The request bytes (empty for an empty batch).
    pub fn request(&self) -> &[u8] {
        &self.buf
    }

    /// Start over, keeping the storage.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.caps.clear();
        self.reply = MSG_HDR + BATCH_RESP;
    }
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Why a batch reply could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// Shorter than its header, or than the entries it must hold.
    Short,
    /// The host refused the whole batch (nothing ran): its negative errno.
    Refused(i32),
    /// Not an `RmBatch` reply, or one for another number of entries.
    Mismatch,
}

/// Check a batch reply's header against the `count` entries sent: `Ok(served)`.
pub fn check_reply(resp: &[u8], caps: &[u32]) -> Result<u32, ReplyError> {
    let status = rc::reply_status(resp).ok_or(ReplyError::Short)?;
    if status != 0 {
        return Err(ReplyError::Refused(status));
    }
    if rc::get32(resp, 0) != Some(MSG_RM_BATCH) {
        return Err(ReplyError::Mismatch);
    }
    let count = rc::get32(resp, MSG_HDR).ok_or(ReplyError::Short)?;
    let served = rc::get32(resp, MSG_HDR + 4).ok_or(ReplyError::Short)?;
    if count as usize != caps.len() || served > count {
        return Err(ReplyError::Mismatch);
    }
    let need = caps
        .iter()
        .fold(MSG_HDR + BATCH_RESP, |n, c| n + REPLY_HDR + pad8(*c as usize));
    if resp.len() < need {
        return Err(ReplyError::Short);
    }
    Ok(served)
}

/// One entry's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryReply<'a> {
    /// The host served it (else it was skipped).
    pub served: bool,
    /// It failed: host status, or RM's status word, nonzero (or out of reach).
    pub failed: bool,
    /// RM's status word as the host read it.
    pub rm_status: Option<u32>,
    /// The reply exactly as a single call gets it (empty when skipped).
    pub bytes: &'a [u8],
}

/// Entry `index` of a reply [`check_reply`] accepted.
pub fn entry_reply<'a>(resp: &'a [u8], caps: &[u32], index: usize) -> Option<EntryReply<'a>> {
    if index >= caps.len() {
        return None;
    }
    let at = caps[..index]
        .iter()
        .fold(MSG_HDR + BATCH_RESP, |n, c| n + REPLY_HDR + pad8(*c as usize));
    let len = rc::get32(resp, at)? as usize;
    let rm_status = rc::get32(resp, at + 4)?;
    let flags = rc::get32(resp, at + 8)?;
    if len > caps[index] as usize {
        return None;
    }
    let body = at + REPLY_HDR;
    Some(EntryReply {
        served: flags & REPLY_SERVED != 0,
        failed: flags & REPLY_FAILED != 0,
        rm_status: (rm_status != u32::MAX).then_some(rm_status),
        bytes: resp.get(body..body + len)?,
    })
}

/// The backend file of every entry of a batch request, in order, or `None` when
/// the request is not a well-formed batch of `Ioctl`s. The KMD checks each is
/// one its own RM client opened before sending.
pub fn entry_handles(req: &[u8]) -> Option<Vec<u32>> {
    if rc::get32(req, 0)? != MSG_RM_BATCH {
        return None;
    }
    let count = rc::get32(req, MSG_HDR)? as usize;
    if count == 0 || count > MAX_ENTRIES {
        return None;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(count).ok()?;
    let mut at = MSG_HDR + BATCH_REQ;
    for _ in 0..count {
        let req_len = rc::get32(req, at)? as usize;
        let sub = req.get(at + ENTRY_HDR..at + ENTRY_HDR + req_len)?;
        if req_len < MSG_HDR + IOCTL_REQ || rc::get32(sub, 0)? != MSG_IOCTL {
            return None;
        }
        out.push(rc::get32(sub, 4)?);
        at += ENTRY_HDR + pad8(req_len);
    }
    (at == req.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply as the host writes it (batch.rs), for the given entry answers.
    fn host_reply(caps: &[u32], answers: &[(u32, u32, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MSG_RM_BATCH.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&(caps.len() as u32).to_le_bytes());
        let served = answers.iter().filter(|a| a.2 & REPLY_SERVED != 0).count() as u32;
        out.extend_from_slice(&served.to_le_bytes());
        for (cap, (len, st, flags, bytes)) in caps.iter().zip(answers) {
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&st.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            let start = out.len();
            out.extend_from_slice(bytes);
            out.resize(start + pad8(*cap as usize), 0);
        }
        out
    }

    #[test]
    fn layout_is_the_host_s() {
        assert_eq!(MSG_RM_BATCH, 37);
        assert_eq!((BATCH_REQ, ENTRY_HDR, BATCH_RESP, REPLY_HDR), (8, 16, 8, 16));
        assert_eq!((MAX_ENTRIES, MAX_RESP), (64, 4096));
        assert_eq!(NO_STATUS, u32::MAX);
        assert_eq!((SKIP_GROUP_ON_FAIL, ABORT_ON_FAIL), (1, 2));
        assert_eq!((REPLY_SERVED, REPLY_FAILED), (1, 2));
        // Within what the backend accepts and its 64-KiB reply buffer.
        assert!(KMD_MAX_ENTRIES <= MAX_ENTRIES);
        assert!(KMD_MAX_REPLY + 4096 <= 64 * 1024);
    }

    #[test]
    fn entries_are_whole_ioctls_padded_to_8() {
        let mut b = Batch::new();
        assert!(b.is_empty());
        let i0 = b
            .push_ioctl(7, 0xc030_462b, &[1u8; 48], &[2u8; 20], None, Some(40), 3, SKIP_GROUP_ON_FAIL)
            .unwrap();
        let i1 = b
            .push_ioctl(8, 0xc010_4629, &[3u8; 16], &[], Some((u32::MAX - 1, &[9u8; 24])), None, 4, 0)
            .unwrap();
        assert_eq!((i0, i1), (0, 1));
        let r = b.request();
        assert_eq!(rc::get32(r, 0), Some(37));
        assert_eq!(rc::get32(r, 4), Some(0), "handle 0");
        assert_eq!(rc::get32(r, MSG_HDR), Some(2));
        assert_eq!(rc::get32(r, MSG_HDR + 4), Some(0));
        let e0 = MSG_HDR + BATCH_REQ;
        let len0 = (MSG_HDR + IOCTL_REQ + 68) as u32;
        assert_eq!(rc::get32(r, e0), Some(len0));
        assert_eq!(rc::get32(r, e0 + 4), Some(reply_cap(48, 20) as u32));
        assert_eq!(rc::get32(r, e0 + 8), Some(40));
        assert_eq!(rc::get32(r, e0 + 12), Some(SKIP_GROUP_ON_FAIL | 3 << 16));
        // The request is what build_ioctl makes for a single call.
        let mut single = [0u8; 256];
        let n = rc::build_ioctl(&mut single, 7, 0xc030_462b, &[1u8; 48], &[2u8; 20]).unwrap();
        assert_eq!(&r[e0 + ENTRY_HDR..e0 + ENTRY_HDR + n], &single[..n]);
        let e1 = e0 + ENTRY_HDR + pad8(len0 as usize);
        assert_eq!(e1 % 8, 0);
        let s1 = e1 + ENTRY_HDR;
        assert_eq!(rc::get32(r, s1 + 4), Some(8), "file");
        assert_eq!(rc::get32(r, s1 + MSG_HDR + 16), Some(u32::MAX - 1), "deep kind");
        assert_eq!(rc::get32(r, s1 + MSG_HDR + 20), Some(24), "deep len");
        assert_eq!(&r[s1 + MSG_HDR + IOCTL_REQ + 16..s1 + MSG_HDR + IOCTL_REQ + 40], &[9u8; 24]);
        assert_eq!(rc::get32(r, e1 + 8), Some(NO_STATUS));
        assert_eq!(r.len(), e1 + ENTRY_HDR + pad8(MSG_HDR + IOCTL_REQ + 40));
        assert_eq!(entry_handles(r), Some(alloc::vec![7, 8]));
        assert_eq!(
            b.reply_len(),
            MSG_HDR + BATCH_RESP + 2 * REPLY_HDR + pad8(reply_cap(48, 20)) + pad8(reply_cap(16, 0))
        );
        b.clear();
        assert!(b.is_empty());
        assert!(b.request().is_empty());
    }

    #[test]
    fn rollback_drops_a_half_added_job() {
        let mut b = Batch::new();
        let empty = b.mark();
        b.push_ioctl(1, 2, &[1u8; 16], &[], None, None, 0, 0).unwrap();
        let one = b.request().to_vec();
        let reply_one = b.reply_len();
        let m = b.mark();
        b.push_ioctl(3, 4, &[2u8; 40], &[5u8; 8], None, Some(40), 1, SKIP_GROUP_ON_FAIL).unwrap();
        b.push_ioctl(3, 4, &[2u8; 40], &[], None, None, 1, 0).unwrap();
        b.rollback(m);
        assert_eq!(b.request(), &one[..]);
        assert_eq!(b.len(), 1);
        assert_eq!(b.reply_len(), reply_one);
        assert_eq!(entry_handles(b.request()), Some(alloc::vec![1]));
        b.rollback(m);
        assert_eq!(b.len(), 1, "nothing after the mark: unchanged");
        b.rollback(empty);
        assert!(b.is_empty() && b.request().is_empty());
        assert_eq!(b.reply_len(), Batch::new().reply_len());
    }

    #[test]
    fn a_batch_stops_at_its_limits() {
        let mut b = Batch::new();
        for _ in 0..KMD_MAX_ENTRIES {
            b.push_ioctl(1, 2, &[0u8; 16], &[], None, None, 0, 0).unwrap();
        }
        assert_eq!(b.push_ioctl(1, 2, &[0u8; 16], &[], None, None, 0, 0), Err(PushError::Full));
        let mut b = Batch::new();
        let big = alloc::vec![0u8; 8 * 1024];
        let mut n = 0;
        while b.push_ioctl(1, 2, &[0u8; 16], &[], Some((1, &big)), None, 0, 0).is_ok() {
            n += 1;
        }
        assert_eq!(n, 3, "32 KiB of request");
        assert!(b.request().len() <= KMD_MAX_REQUEST);
        assert_eq!(
            Batch::new().push_ioctl(1, 2, &[], &[], None, None, 0, 0x80),
            Err(PushError::Invalid)
        );
    }

    #[test]
    fn replies_are_read_per_entry() {
        let caps = [256u32, 300, 256];
        let one = [5u8; 40];
        let resp = host_reply(
            &caps,
            &[
                (40, 0, REPLY_SERVED, &one),
                (28, 0x1f, REPLY_SERVED | REPLY_FAILED, &[6u8; 28]),
                (0, u32::MAX, 0, &[]),
            ],
        );
        assert_eq!(check_reply(&resp, &caps), Ok(2));
        let a = entry_reply(&resp, &caps, 0).unwrap();
        assert!(a.served && !a.failed);
        assert_eq!(a.rm_status, Some(0));
        assert_eq!(a.bytes, &one);
        let b = entry_reply(&resp, &caps, 1).unwrap();
        assert!(b.served && b.failed);
        assert_eq!(b.rm_status, Some(0x1f));
        let c = entry_reply(&resp, &caps, 2).unwrap();
        assert!(!c.served && !c.failed && c.bytes.is_empty());
        assert_eq!(c.rm_status, None);
        assert!(entry_reply(&resp, &caps, 3).is_none());
        // Wrong count, short, refused.
        assert_eq!(check_reply(&resp, &caps[..2]), Err(ReplyError::Mismatch));
        assert_eq!(check_reply(&resp[..resp.len() - 8], &caps), Err(ReplyError::Short));
        let mut refused = [0u8; 16];
        refused[0..4].copy_from_slice(&37u32.to_le_bytes());
        refused[8..12].copy_from_slice(&(-22i32).to_le_bytes());
        assert_eq!(check_reply(&refused, &caps), Err(ReplyError::Refused(-22)));
    }

    #[test]
    fn a_malformed_request_has_no_handles() {
        let mut b = Batch::new();
        b.push_ioctl(7, 2, &[0u8; 16], &[], None, None, 0, 0).unwrap();
        let mut r = b.request().to_vec();
        r.push(0);
        assert_eq!(entry_handles(&r), None, "trailing bytes");
        let mut r = b.request().to_vec();
        r[MSG_HDR + BATCH_REQ + ENTRY_HDR..][..4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(entry_handles(&r), None, "an Open inside");
        assert_eq!(entry_handles(&[]), None);
    }
}

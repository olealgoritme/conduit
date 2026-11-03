//! `MsgType::RmBatch`: several `Ioctl` requests in one round trip.
//!
//! The guest KMD's page-table updates make three to six RM calls each (an OS
//! descriptor, a virtual object, a map; an unmap and two frees), every one a
//! virtio round trip of its own. A batch carries them together. Each entry is
//! a whole `Ioctl` request and is served by [`NvidiaBackend::dispatch`] exactly
//! as if it had arrived alone: the ABI check, the RM policy gate
//! (`CONDUIT_RM_POLICY`), the registration translation and every record a
//! single call keeps apply to each entry, in order. Nothing here interprets RM
//! beyond reading the status word the guest points at, to decide whether the
//! entry failed and later entries of its group are skipped.
//!
//! The whole batch is checked before anything is served: an entry that is not
//! an `Ioctl`, a length that does not fit, a reply that could not be given
//! room, or more than [`RM_BATCH_MAX_ENTRIES`] entries refuse the batch with
//! `EINVAL` and no entry runs.

use super::*;
use protocol::messages::{
    MsgType, RM_BATCH_ABORT_ON_FAIL, RM_BATCH_MAX_ENTRIES, RM_BATCH_MAX_RESP, RM_BATCH_NO_STATUS,
    RM_BATCH_REPLY_FAILED, RM_BATCH_REPLY_SERVED, RM_BATCH_SKIP_GROUP_ON_FAIL, RmBatchEntry,
    RmBatchReply, RmBatchReq, RmBatchResp, rm_batch_pad8,
};

/// One checked entry: where its request is in the payload and its slot in the reply.
struct Planned {
    req_at: usize,
    req_len: usize,
    resp_cap: usize,
    status_at: u32,
    flags: u32,
}

impl NvidiaBackend {
    /// `MsgType::RmBatch`: check every entry, then serve them in order.
    pub(super) fn handle_rm_batch(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let refuse = |this: &mut Self, resp_buf: &mut [u8], why: &str| {
            log::warn!("RmBatch refused: {why}");
            this.current_msg = MsgType::RmBatch;
            this.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL)
        };
        if self.current_handle != 0 {
            return refuse(self, resp_buf, "the header names a handle");
        }
        if payload.len() < size_of::<RmBatchReq>() {
            return refuse(self, resp_buf, "too short for its header");
        }
        let req = read_struct::<RmBatchReq>(payload, 0);
        if req.count == 0 || req.count > RM_BATCH_MAX_ENTRIES || req.flags != 0 {
            return refuse(self, resp_buf, "entry count or flags out of range");
        }

        // Check the whole batch first: nothing runs unless all of it can.
        let mut plan = Vec::with_capacity(req.count as usize);
        let mut at = size_of::<RmBatchReq>();
        let mut reply_bytes = size_of::<MsgHeader>() + size_of::<RmBatchResp>();
        for _ in 0..req.count {
            let Some(head) = payload.get(at..at + size_of::<RmBatchEntry>()) else {
                return refuse(self, resp_buf, "an entry runs past the request");
            };
            let entry = read_struct::<RmBatchEntry>(head, 0);
            let req_at = at + size_of::<RmBatchEntry>();
            let req_len = entry.req_len as usize;
            let Some(sub) = payload.get(req_at..req_at.saturating_add(req_len)) else {
                return refuse(self, resp_buf, "an entry's request runs past the batch");
            };
            if req_len < size_of::<MsgHeader>() + size_of::<IoctlReq>()
                || read_struct::<MsgHeader>(sub, 0).msg_type != MsgType::Ioctl as u32
            {
                return refuse(self, resp_buf, "an entry is not an Ioctl");
            }
            if entry.resp_cap < size_of::<MsgHeader>() as u32 || entry.resp_cap > RM_BATCH_MAX_RESP {
                return refuse(self, resp_buf, "an entry's reply room is out of range");
            }
            let known = RM_BATCH_SKIP_GROUP_ON_FAIL | RM_BATCH_ABORT_ON_FAIL;
            if entry.flags & 0xffff & !known != 0 {
                return refuse(self, resp_buf, "an entry has unknown flags");
            }
            reply_bytes += size_of::<RmBatchReply>() + rm_batch_pad8(entry.resp_cap as usize);
            plan.push(Planned {
                req_at,
                req_len,
                resp_cap: entry.resp_cap as usize,
                status_at: entry.status_at,
                flags: entry.flags,
            });
            at = req_at + rm_batch_pad8(req_len);
        }
        if at > payload.len() + 7 {
            return refuse(self, resp_buf, "padding runs past the request");
        }
        if reply_bytes > resp_buf.len() {
            return refuse(self, resp_buf, "the reply room is smaller than the entries ask for");
        }

        // Serve in order. Each entry's reply is written straight into its slot.
        let mut off = size_of::<MsgHeader>() + size_of::<RmBatchResp>();
        let mut served = 0u32;
        let mut aborted = false;
        let mut failed_groups: Vec<u16> = Vec::new();
        for p in &plan {
            let group = (p.flags >> 16) as u16;
            let slot_at = off + size_of::<RmBatchReply>();
            let skip = aborted || failed_groups.contains(&group);
            let mut answer = RmBatchReply {
                len: 0,
                rm_status: u32::MAX,
                flags: 0,
                reserved: 0,
            };
            if !skip {
                let sub = &payload[p.req_at..p.req_at + p.req_len];
                let slot = &mut resp_buf[slot_at..slot_at + p.resp_cap];
                let n = self.dispatch(sub, slot).min(p.resp_cap);
                served += 1;
                answer.len = n as u32;
                answer.flags = RM_BATCH_REPLY_SERVED;
                let host_ok =
                    n >= size_of::<MsgHeader>() && read_struct::<MsgHeader>(slot, 0).status == 0;
                let mut ok = host_ok;
                if p.status_at != RM_BATCH_NO_STATUS {
                    let word = (size_of::<MsgHeader>() + size_of::<IoctlResp>())
                        .checked_add(p.status_at as usize)
                        .filter(|w| w + 4 <= n)
                        .map(|w| u32::from_le_bytes(slot[w..w + 4].try_into().unwrap()));
                    match word {
                        Some(s) if host_ok => {
                            answer.rm_status = s;
                            ok = s == 0;
                        }
                        _ => ok = false,
                    }
                }
                if !ok {
                    answer.flags |= RM_BATCH_REPLY_FAILED;
                    if p.flags & RM_BATCH_ABORT_ON_FAIL != 0 {
                        aborted = true;
                    }
                    if p.flags & RM_BATCH_SKIP_GROUP_ON_FAIL != 0 && !failed_groups.contains(&group)
                    {
                        failed_groups.push(group);
                    }
                }
                // Padding after the reply, so nothing of an earlier use leaks.
                slot[n..].fill(0);
            } else {
                resp_buf[slot_at..slot_at + p.resp_cap].fill(0);
            }
            let pad_end = slot_at + rm_batch_pad8(p.resp_cap);
            resp_buf[slot_at + p.resp_cap..pad_end].fill(0);
            write_struct(&mut resp_buf[off..], &answer);
            off = pad_end;
        }
        self.current_msg = MsgType::RmBatch;
        self.current_handle = 0;
        let head = self.write_hdr(resp_buf, 0, 0);
        write_struct(
            &mut resp_buf[head..],
            &RmBatchResp {
                count: req.count,
                served,
            },
        );
        off
    }
}

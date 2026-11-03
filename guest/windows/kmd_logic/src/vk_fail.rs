//! The failing-call rings (`VkF*`, `VkR*`): the pure half.
//!
//! A Vulkan app on NVIDIA's ICD can end in `VK_ERROR_DEVICE_LOST` with no Xid,
//! no KMD hang or fault and no dxgkrnl device error: the ICD decided it from a
//! call the KMD answered. Most of the DDIs and escapes it uses answer without any
//! trace (the HW-queue DDIs are not wrapped by `device_lost`, an NVDA escape's
//! hint there is its version word, an RM status travels inside a successful
//! escape). These rings keep the last failing calls with what names them:
//!
//! * `VkF` (32 entries): DDIs, NVDA escapes, HW-queue and copy-context
//!   submissions, allocations, page-table refusals.
//! * `VkR` (16 entries): RM statuses: an NVDA RM control answered with a nonzero
//!   status and every KMD-forwarded RM ioctl the host or RM refused. Separate,
//!   so a stream of expected RM refusals cannot push the DDI failures out.
//!
//! An entry is `(kind, code, status, pid, hint, time, repeats)`; the same event
//! again (kind, code, status and process equal) right after itself only counts a
//! repeat and moves the time, so a polled refusal takes one slot.
//!
//! `kmd_render/src/ddi/vk_fail.rs` holds the rings under spinlocks (callers run
//! at PASSIVE or DISPATCH) and publishes them from the registry mirror's pass.

/// What failed (the top byte of `VkF..K`).
pub mod kind {
    /// An NVDA escape answered with an NTSTATUS other than success. code: the
    /// escape function (+0x10), hint: the buffer's bytes.
    pub const ESCAPE: u8 = 1;
    /// An NVDA RM control answered with RM status != 0 (the escape itself
    /// succeeded). code: the control's cmd, status: RM's, hint: hObject.
    pub const ESCAPE_RM: u8 = 2;
    /// A KMD-forwarded RM ioctl whose RM status was nonzero. code: the control
    /// cmd (RM_CONTROL), the class (RM_ALLOC) or 0; hint: [`rm_hint`].
    pub const RM_CALL: u8 = 3;
    /// CreateContext of an NVDA context. code: [`context_reason`], hint: the node.
    pub const CREATE_CONTEXT: u8 = 4;
    /// CreateHwQueue. code: [`queue_reason`], hint: [`queue_hint`].
    pub const CREATE_HW_QUEUE: u8 = 5;
    /// SubmitCommandToHwQueue. code: [`submit_reason`], hint: [`submit_hint`].
    pub const SUBMIT_HW_QUEUE: u8 = 6;
    /// A submission to an NVDA queue without a channel (a node this KMD does not
    /// back): completed at once, its work never ran. status 0, code: the node.
    pub const SOFT_QUEUE: u8 = 7;
    /// The Vulkan ICD's node-3 copy context: a submission whose work did not go
    /// to the channel. code: [`copy_reason`], hint: entries (or the record size).
    pub const COPY_CONTEXT: u8 = 8;
    /// CreateAllocation of an NVDA record. code: [`alloc_code`], hint: the
    /// record's size in 64-KiB units (0 when unparsed).
    pub const CREATE_ALLOCATION: u8 = 9;
    /// A native paging operation skipped (BuildPagingBuffer answers success).
    /// code: 1 leaf update, 2 transfer, 3 fill, 4 virtual fill, 5 virtual
    /// transfer, 6 residency, 7 above PASSIVE; hint: the VA >> 16 (leaf updates).
    pub const PAGING: u8 = 10;
    /// A `device_lost`-wrapped DDI answered non-success. code: its DDI id.
    pub const DDI: u8 = 11;
    /// DestroyHwQueue answered non-success.
    pub const DESTROY_HW_QUEUE: u8 = 12;
    /// A KMD-forwarded RM ioctl the host refused (status: its errno, negative).
    pub const RM_HOST: u8 = 13;
    /// A copy-context submission that waited in the backlog (ring full) and went
    /// to the channel later: informational, status 0. code: the backlog depth.
    pub const COPY_DEFERRED: u8 = 14;
    /// The ICD polled its wait escape (0x01000021) for the same [`super::wait_key`]
    /// for more than [`super::WAIT_REPORT_MS`] (again at every doubling): it is
    /// spinning on GPU work that has not finished. status 0, code: the key, hint:
    /// ms so far.
    pub const LONG_WAIT: u8 = 15;
    /// One past the largest kind; per-kind counters are this long.
    pub const MAX: usize = 16;
}

/// `kind::COPY_CONTEXT` codes.
pub mod copy_reason {
    /// The submission's private data holds no record this KMD understands.
    pub const NO_RECORD: u32 = 1;
    /// The channel's ring was full and the backlog could not take it.
    pub const BUSY: u32 = 2;
    /// No channel for the context and no CPU fallback: the work never ran.
    pub const NO_CHANNEL: u32 = 3;
    /// The channel refused for another reason (transport, dead).
    pub const CHANNEL_ERROR: u32 = 4;
    /// The CPU fallback's job table was full.
    pub const CPU_FULL: u32 = 5;
    /// The record has more entries than the CPU fallback takes.
    pub const CPU_TOO_LONG: u32 = 6;
}

/// `kind::CREATE_CONTEXT` codes.
pub mod context_reason {
    /// The HW-queue private header did not parse: the context exists, its queues
    /// are refused. status: 0; code low byte: the `PrivateError` + 1.
    pub const BAD_PRIVATE: u32 = 0x100;
    /// The native TSG could not be made (status says why).
    pub const NATIVE_FAILED: u32 = 0x200;
    /// The context was made without channel state on a node this KMD does not
    /// back (its queues complete without running).
    pub const UNBACKED_NODE: u32 = 0x300;
    /// The shared context path failed.
    pub const CREATE_FAILED: u32 = 0x400;
}

/// `kind::CREATE_HW_QUEUE` codes (with the error's discriminant + 1 in the low byte).
pub mod queue_reason {
    pub const PRIVATE: u32 = 0x100;
    pub const NO_FENCE: u32 = 0x200;
    pub const RING_BACKING: u32 = 0x300;
    pub const NATIVE: u32 = 0x400;
    pub const WRITEBACK: u32 = 0x500;
    pub const REGISTRY_FULL: u32 = 0x600;
    pub const NOT_NATIVE: u32 = 0x700;
    pub const OTHER: u32 = 0x800;
}

/// `kind::SUBMIT_HW_QUEUE` codes: the native error's discriminant + 1, or these.
pub mod submit_reason {
    pub const BAD_HANDLE: u32 = 0x100;
    pub const NO_PRIVATE: u32 = 0x200;
    /// The record's GP_PUT did not move (a submission of no entries).
    pub const GP_PUT_SAME: u32 = 0x300;
    /// The queue's channel is dead: completed without running (status 0).
    pub const DEAD: u32 = 0x400;
    /// A zero-entry submission accepted (status 0, `NvQZeroAdv`).
    pub const ZERO_ADVANCE: u32 = 0x500;
}

/// `kind::CREATE_ALLOCATION` codes: 0x100 | `PrivateError` + 1 for a record that
/// did not parse, 0x200 | `native_memory::Error` + 1 (and the step << 16) for one
/// the native service refused, 0x300 for any other refusal.
pub mod alloc_code {
    pub const PARSE: u32 = 0x100;
    pub const CREATE: u32 = 0x200;
    pub const OTHER: u32 = 0x300;
}

/// `queue_kind | ring_bytes / 4 KiB << 8 | queue_flags << 24` (`kind::CREATE_HW_QUEUE`).
pub const fn queue_hint(queue_kind: u8, ring_bytes: u32, queue_flags: u32) -> u32 {
    (queue_kind as u32) | ((ring_bytes >> 12) & 0xffff) << 8 | (queue_flags & 0xff) << 24
}

/// `new GP_PUT (16 bits) | private bytes << 16` (`kind::SUBMIT_HW_QUEUE`).
pub const fn submit_hint(new_gp_put: u32, private_bytes: u32) -> u32 {
    (new_gp_put & 0xffff) | (private_bytes & 0xffff) << 16
}

/// `escape nr << 24 | object (low 24 bits)` (`kind::RM_CALL`, `kind::RM_HOST`).
pub const fn rm_hint(nr: u32, object: u32) -> u32 {
    (nr & 0xff) << 24 | (object & 0x00ff_ffff)
}

/// `kind << 24 | pid` (the `K` value of an entry).
pub const fn pack_kind_pid(kind: u8, pid: u32) -> u32 {
    (kind as u32) << 24 | (pid & 0x00ff_ffff)
}

/// One failing call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub kind: u8,
    pub code: u32,
    pub status: u32,
    pub pid: u32,
    pub hint: u32,
    /// Interrupt-time milliseconds of the latest occurrence.
    pub t_ms: u32,
    /// Occurrences after the first, back to back.
    pub repeats: u32,
}

impl Entry {
    pub const EMPTY: Entry = Entry {
        kind: 0,
        code: 0,
        status: 0,
        pid: 0,
        hint: 0,
        t_ms: 0,
        repeats: 0,
    };

    pub const fn new(kind: u8, code: u32, status: u32, pid: u32, hint: u32, t_ms: u32) -> Self {
        Entry {
            kind,
            code,
            status,
            pid,
            hint,
            t_ms,
            repeats: 0,
        }
    }

    /// The same event (what is collapsed into a repeat): the hint and the time
    /// may differ.
    pub const fn same_event(&self, other: &Entry) -> bool {
        self.kind == other.kind
            && self.code == other.code
            && self.status == other.status
            && self.pid == other.pid
    }
}

/// The last `N` distinct events, newest first on read.
#[derive(Clone, Copy, Debug)]
pub struct Ring<const N: usize> {
    slots: [Entry; N],
    /// Where the next distinct event goes.
    next: usize,
    len: usize,
    /// Every event, repeats included.
    total: u32,
    /// Moves on every push, for the publisher's "changed" test.
    version: u32,
    per_kind: [u32; kind::MAX],
}

impl<const N: usize> Ring<N> {
    pub const fn new() -> Self {
        Ring {
            slots: [Entry::EMPTY; N],
            next: 0,
            len: 0,
            total: 0,
            version: 0,
            per_kind: [0; kind::MAX],
        }
    }

    pub fn push(&mut self, entry: Entry) {
        self.total = self.total.wrapping_add(1);
        self.version = self.version.wrapping_add(1);
        if let Some(c) = self.per_kind.get_mut(usize::from(entry.kind)) {
            *c = c.wrapping_add(1);
        }
        if N == 0 {
            return;
        }
        if self.len != 0 {
            let newest = (self.next + N - 1) % N;
            let last = &mut self.slots[newest];
            if last.same_event(&entry) {
                last.repeats = last.repeats.saturating_add(1);
                last.t_ms = entry.t_ms;
                last.hint = entry.hint;
                return;
            }
        }
        self.slots[self.next] = Entry {
            repeats: 0,
            ..entry
        };
        self.next = (self.next + 1) % N;
        self.len = (self.len + 1).min(N);
    }

    /// The `i`-th newest entry (0 = the latest).
    pub fn newest(&self, i: usize) -> Option<Entry> {
        if i >= self.len {
            return None;
        }
        Some(self.slots[(self.next + N - 1 - i) % N])
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn total(&self) -> u32 {
        self.total
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn per_kind(&self, kind: u8) -> u32 {
        self.per_kind.get(usize::from(kind)).copied().unwrap_or(0)
    }
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The registry name of entry `i`'s field `field` (`K` kind | pid, `C` code, `S`
/// status, `H` hint, `T` time, `R` repeats): `prefix` + two hex digits + field,
/// e.g. `VkF00C`.
pub const fn entry_name(prefix: [u8; 3], i: usize, field: u8) -> [u8; 6] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        prefix[0],
        prefix[1],
        prefix[2],
        HEX[(i >> 4) & 15],
        HEX[i & 15],
        field,
    ]
}

/// The fields of an entry, in the order they are published.
pub const FIELDS: [u8; 6] = [b'K', b'C', b'S', b'H', b'T', b'R'];

/// The value of `field` of `e` ([`FIELDS`]).
pub const fn field_value(e: &Entry, field: u8) -> u32 {
    match field {
        b'K' => pack_kind_pid(e.kind, e.pid),
        b'C' => e.code,
        b'S' => e.status,
        b'H' => e.hint,
        b'T' => e.t_ms,
        b'R' => e.repeats,
        _ => 0,
    }
}

/// The name of per-kind counter `kind`: `VkNk` + two hex digits.
pub const fn kind_counter_name(kind: u8) -> [u8; 6] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        b'V',
        b'k',
        b'N',
        b'k',
        HEX[((kind >> 4) & 15) as usize],
        HEX[(kind & 15) as usize],
    ]
}

// ---- the ICD's wait poll -----------------------------------------------------------------

/// A wait streak is reported once it is this long, and again each time it doubles.
pub const WAIT_REPORT_MS: u32 = 1000;

/// Waits followed at once (threads polling different tokens side by side).
pub const WAIT_STREAKS: usize = 4;
/// A token not polled for this long starts a new streak when it comes back.
pub const WAIT_GAP_MS: u32 = 100;

/// One token's polls back to back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streak {
    pub token: u32,
    pub first_ms: u32,
    pub last_ms: u32,
    pub count: u32,
    /// Where the next report is due (ms into the streak); 0 = slot unused.
    next_report: u32,
}

impl Streak {
    pub const EMPTY: Streak = Streak {
        token: 0,
        first_ms: 0,
        last_ms: 0,
        count: 0,
        next_report: 0,
    };

    pub const fn ms(&self) -> u32 {
        self.last_ms.wrapping_sub(self.first_ms)
    }
}

/// The Vulkan ICD's wait poll (NVDA 0x01000021): its device-lost check, which it
/// calls (with `GetDeviceState`) on every turn of its busy-wait loops while it
/// waits for GPU work, about 2 us per poll on NVIDIA's KMD, a few polls per wait
/// there. The word at +0x20 is the ICD's uninitialised stack, so a streak is keyed
/// by [`wait_key`] (the escape's context, else its device). A streak is one key's
/// polls without a gap of [`WAIT_GAP_MS`]; its length says how long the ICD has
/// waited for that work. [`WAIT_STREAKS`] keys are followed at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitStreaks {
    pub polls: u32,
    pub streaks: [Streak; WAIT_STREAKS],
    /// The streak polled last.
    pub current: usize,
    pub max_ms: u32,
    pub max_token: u32,
    pub max_count: u32,
}

impl WaitStreaks {
    pub const fn new() -> Self {
        WaitStreaks {
            polls: 0,
            streaks: [Streak::EMPTY; WAIT_STREAKS],
            current: 0,
            max_ms: 0,
            max_token: 0,
            max_count: 0,
        }
    }

    /// One poll of `token` at `now_ms`: `Some(ms)` when its streak just crossed a
    /// report point ([`WAIT_REPORT_MS`], then every doubling).
    pub fn poll(&mut self, token: u32, now_ms: u32) -> Option<u32> {
        self.polls = self.polls.wrapping_add(1);
        let live = |s: &Streak| s.next_report != 0 && now_ms.wrapping_sub(s.last_ms) <= WAIT_GAP_MS;
        let i = match self.streaks.iter().position(|s| s.token == token && live(s)) {
            Some(i) => i,
            None => {
                // A free slot, else the one polled longest ago.
                let i = self
                    .streaks
                    .iter()
                    .position(|s| s.next_report == 0)
                    .unwrap_or_else(|| {
                        (0..WAIT_STREAKS)
                            .max_by_key(|&i| now_ms.wrapping_sub(self.streaks[i].last_ms))
                            .unwrap_or(0)
                    });
                self.streaks[i] = Streak {
                    token,
                    first_ms: now_ms,
                    last_ms: now_ms,
                    count: 0,
                    next_report: WAIT_REPORT_MS,
                };
                i
            }
        };
        self.current = i;
        let s = &mut self.streaks[i];
        s.count = s.count.saturating_add(1);
        s.last_ms = now_ms;
        let ms = s.ms();
        let report = if ms >= s.next_report {
            s.next_report = s.next_report.saturating_mul(2).max(ms.saturating_add(1));
            Some(ms)
        } else {
            None
        };
        if ms > self.max_ms {
            self.max_ms = ms;
            self.max_token = token;
            self.max_count = s.count;
        }
        report
    }

    /// The streak polled last.
    pub fn last(&self) -> Streak {
        self.streaks[self.current]
    }
}

/// The streak key of a wait poll: the escape's hContext (the ICD's per-context
/// check), else its hDevice (the device-wide check), as the low 32 bits dxgkrnl's
/// handles live in.
pub const fn wait_key(h_device: usize, h_context: usize) -> u32 {
    if h_context != 0 {
        h_context as u32
    } else {
        h_device as u32
    }
}

/// What the KMD does on a wait poll before it answers (`NvGlYield`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollPause {
    /// Answer at once, as NVIDIA's KMD does.
    None,
    /// Give the vCPU to another ready thread (a zero-length delay), then answer.
    Yield,
    /// Sleep this many microseconds (rounded up to the timer resolution), then answer.
    DelayUs(u32),
}

/// Polls of one streak answered at once before [`poll_pause`] slows the ICD
/// down (`NvGlYieldMin` 0): above what one wait takes on NVIDIA's hardware.
pub const POLL_PAUSE_MIN_DEFAULT: u32 = 16;
/// The longest delay `NvGlYield` may ask for (us).
pub const POLL_PAUSE_MAX_US: u32 = 2000;

/// The pause for a poll that is the `streak`-th of its streak. `mode`
/// (`NvGlYield`): 0 none, 1 yield, 2.. a delay of that many microseconds (at most
/// [`POLL_PAUSE_MAX_US`]). `min_polls` (`NvGlYieldMin`, 0 = the default): the
/// first polls of a streak are answered at once, so short waits keep NVIDIA's
/// latency and only a spin that has lasted pays.
pub fn poll_pause(mode: u32, min_polls: u32, streak: u32) -> PollPause {
    let min = if min_polls == 0 { POLL_PAUSE_MIN_DEFAULT } else { min_polls };
    if mode == 0 || streak <= min {
        return PollPause::None;
    }
    match mode {
        1 => PollPause::Yield,
        us => PollPause::DelayUs(us.min(POLL_PAUSE_MAX_US)),
    }
}

// ---- forwarded RM ioctls -----------------------------------------------------------------

/// What a failed KMD-forwarded RM ioctl is recorded as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RmFailure {
    pub kind: u8,
    pub code: u32,
    pub status: u32,
    pub hint: u32,
}

fn get32(buf: &[u8], at: usize) -> Option<u32> {
    let s = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A forwarded `Ioctl` (`req`) and its reply (`resp`, `n` bytes): the failure to
/// record, or `None` when both the host and RM answered success (or the reply is
/// too short to tell RM's status, which the callers' own parsers then refuse).
///
/// Layout (`rm_client`): request `MsgHeader` (16) | `IoctlReq` (24: cmd at +16)
/// | data from +40; reply `MsgHeader` (status at +8) | `IoctlResp` (12) | data
/// from +28. RM's status sits in the data block: NVOS54 (RM_CONTROL) +28,
/// NVOS64 (RM_ALLOC) +40, NVOS00 (RM_FREE) +12, NVOS33 (MAP_MEMORY) +40, NVOS34
/// (UNMAP_MEMORY) +24. Other escapes carry no RM status here.
pub fn rm_ioctl_failure(req: &[u8], resp: &[u8], n: usize) -> Option<RmFailure> {
    use crate::rm_client as rc;
    const REQ_DATA: usize = rc::MSG_HDR + rc::IOCTL_REQ;
    let cmd = get32(req, rc::MSG_HDR)?;
    let nr = cmd & 0xff;
    let resp = resp.get(..n.min(resp.len()))?;
    let (code, object, status_at) = match nr {
        rc::ESC_RM_CONTROL => (
            get32(req, REQ_DATA + 8).unwrap_or(0),
            get32(req, REQ_DATA + 4).unwrap_or(0),
            Some(rc::NVOS54_STATUS_AT),
        ),
        rc::ESC_RM_ALLOC => (
            get32(req, REQ_DATA + 12).unwrap_or(0),
            get32(req, REQ_DATA + 8).unwrap_or(0),
            Some(rc::NVOS64_STATUS_AT),
        ),
        rc::ESC_RM_FREE => (0, get32(req, REQ_DATA + 8).unwrap_or(0), Some(rc::NVOS00_STATUS_AT)),
        rc::ESC_RM_MAP_MEMORY => (0, get32(req, REQ_DATA + 8).unwrap_or(0), Some(rc::NVOS33_STATUS_AT)),
        rc::ESC_RM_UNMAP_MEMORY => (0, get32(req, REQ_DATA + 8).unwrap_or(0), Some(rc::NVOS34_STATUS_AT)),
        _ => (0, 0, None),
    };
    let hint = rm_hint(nr, object);
    let host = get32(resp, 8)?;
    if host != 0 {
        return Some(RmFailure {
            kind: kind::RM_HOST,
            code: if code != 0 { code } else { nr },
            status: host,
            hint,
        });
    }
    let status = get32(resp, rc::REPLY_DATA + status_at?)?;
    if status == 0 {
        return None;
    }
    Some(RmFailure {
        kind: kind::RM_CALL,
        code: if code != 0 { code } else { nr },
        status,
        hint,
    })
}

/// Every failed entry of an `RmBatch` (`crate::rm_batch`) `req` and its reply
/// `resp` (`n` bytes), as [`rm_ioctl_failure`] reads a single call; an entry the
/// host flagged failed without a readable status is reported with RM's status
/// word as the host read it (`NO_STATUS` when out of reach). Skipped entries
/// are not failures. A malformed pair reports nothing.
pub fn rm_batch_failures(req: &[u8], resp: &[u8], n: usize, mut each: impl FnMut(RmFailure)) {
    use crate::rm_batch as rb;
    use crate::rm_client::MSG_HDR;
    let resp = &resp[..n.min(resp.len())];
    if get32(req, 0) != Some(rb::MSG_RM_BATCH) || get32(resp, 0) != Some(rb::MSG_RM_BATCH) {
        return;
    }
    let Some(count) = get32(req, MSG_HDR) else {
        return;
    };
    if count as usize > rb::MAX_ENTRIES || get32(resp, MSG_HDR) != Some(count) {
        return;
    }
    let mut at_req = MSG_HDR + rb::BATCH_REQ;
    let mut at_resp = MSG_HDR + rb::BATCH_RESP;
    for _ in 0..count {
        let (Some(req_len), Some(cap)) = (get32(req, at_req), get32(req, at_req + 4)) else {
            return;
        };
        let (req_len, cap) = (req_len as usize, cap as usize);
        let Some(sub) = req.get(at_req + rb::ENTRY_HDR..at_req + rb::ENTRY_HDR + req_len) else {
            return;
        };
        let (Some(len), Some(rm_status), Some(flags)) =
            (get32(resp, at_resp), get32(resp, at_resp + 4), get32(resp, at_resp + 8))
        else {
            return;
        };
        let body = at_resp + rb::REPLY_HDR;
        if flags & rb::REPLY_SERVED != 0 && flags & rb::REPLY_FAILED != 0 {
            let reply = resp.get(body..body + (len as usize).min(cap)).unwrap_or(&[]);
            match rm_ioctl_failure(sub, reply, reply.len()) {
                Some(f) => each(f),
                None => {
                    let cmd = get32(sub, MSG_HDR).unwrap_or(0);
                    each(RmFailure {
                        kind: kind::RM_CALL,
                        code: cmd & 0xff,
                        status: rm_status,
                        hint: rm_hint(cmd & 0xff, 0),
                    });
                }
            }
        }
        at_req += rb::ENTRY_HDR + rb::pad8(req_len);
        at_resp += rb::REPLY_HDR + rb::pad8(cap);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::rm_client as rc;
    use std::vec;
    use std::vec::Vec;

    fn e(kind: u8, code: u32, status: u32, t: u32) -> Entry {
        Entry::new(kind, code, status, 0x1234, 7, t)
    }

    #[test]
    fn newest_first_and_wraps_at_capacity() {
        let mut r = Ring::<4>::new();
        assert!(r.is_empty());
        assert_eq!(r.newest(0), None);
        for i in 0..6u32 {
            r.push(e(kind::ESCAPE, i, 0xc000_0002, i));
        }
        assert_eq!(r.len(), 4);
        assert_eq!(r.total(), 6);
        let codes: Vec<u32> = (0..4).map(|i| r.newest(i).unwrap().code).collect();
        assert_eq!(codes, vec![5, 4, 3, 2]);
        assert_eq!(r.newest(4), None);
        assert_eq!(r.per_kind(kind::ESCAPE), 6);
        assert_eq!(r.per_kind(kind::DDI), 0);
        assert_eq!(r.per_kind(200), 0);
    }

    #[test]
    fn back_to_back_repeats_take_one_slot_and_keep_the_latest_time_and_hint() {
        let mut r = Ring::<4>::new();
        r.push(e(kind::ESCAPE, 0x0100_0021, 0xc000_0002, 10));
        let mut again = e(kind::ESCAPE, 0x0100_0021, 0xc000_0002, 20);
        again.hint = 99;
        r.push(again);
        r.push(e(kind::ESCAPE, 0x0100_0021, 0xc000_0002, 30));
        assert_eq!(r.len(), 1);
        assert_eq!(r.total(), 3);
        let newest = r.newest(0).unwrap();
        assert_eq!((newest.repeats, newest.t_ms, newest.hint), (2, 30, 7));
        // A different status, process or kind is a new event.
        r.push(e(kind::ESCAPE, 0x0100_0021, 0xc000_000d, 40));
        let mut other_pid = e(kind::ESCAPE, 0x0100_0021, 0xc000_000d, 50);
        other_pid.pid = 8;
        r.push(other_pid);
        r.push(e(kind::DDI, 0x0100_0021, 0xc000_000d, 60));
        assert_eq!(r.len(), 4);
        assert_eq!(r.newest(3).unwrap().repeats, 2);
        // Not back to back: a new slot again.
        r.push(e(kind::ESCAPE, 0x0100_0021, 0xc000_0002, 70));
        assert_eq!(r.newest(0).unwrap().repeats, 0);
        assert_eq!(r.version(), 7);
    }

    #[test]
    fn names_and_values() {
        assert_eq!(&entry_name(*b"VkF", 0, b'C'), b"VkF00C");
        assert_eq!(&entry_name(*b"VkR", 31, b'K'), b"VkR1fK");
        assert_eq!(&kind_counter_name(kind::COPY_CONTEXT), b"VkNk08");
        let x = Entry {
            kind: kind::SUBMIT_HW_QUEUE,
            code: 1,
            status: 2,
            pid: 0x2345_6789,
            hint: 3,
            t_ms: 4,
            repeats: 5,
        };
        let values: Vec<u32> = FIELDS.iter().map(|f| field_value(&x, *f)).collect();
        assert_eq!(values, vec![0x0645_6789, 1, 2, 3, 4, 5]);
        assert_eq!(queue_hint(1, 0x10000, 0x12), 0x1200_1001);
        assert_eq!(submit_hint(0x1_0005, 0x118), 0x0118_0005);
        assert_eq!(rm_hint(0x2a, 0xff03_0000), 0x2a03_0000);
    }

    /// A forwarded ioctl request/reply pair: `nr`, top-level data block `data`
    /// (as sent), reply host status and reply data block.
    fn ioctl(nr: u32, data: &[u8], host: u32, reply_data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut req = vec![0u8; rc::MSG_HDR + rc::IOCTL_REQ + data.len()];
        req[0..4].copy_from_slice(&rc::MSG_IOCTL.to_le_bytes());
        req[16..20].copy_from_slice(&rc::nv_cmd(nr, data.len() as u32).to_le_bytes());
        req[40..].copy_from_slice(data);
        let mut resp = vec![0u8; rc::REPLY_DATA + reply_data.len()];
        resp[8..12].copy_from_slice(&host.to_le_bytes());
        resp[rc::REPLY_DATA..].copy_from_slice(reply_data);
        (req, resp)
    }

    #[test]
    fn rm_control_status_names_the_control_and_object() {
        let data = rc::nvos54(0xc1d0_0001, 0x2080_0001, 0x2080_012f, 8);
        let mut reply = data;
        reply[rc::NVOS54_STATUS_AT..rc::NVOS54_STATUS_AT + 4].copy_from_slice(&0x56u32.to_le_bytes());
        let (req, resp) = ioctl(rc::ESC_RM_CONTROL, &data, 0, &reply);
        assert_eq!(
            rm_ioctl_failure(&req, &resp, resp.len()),
            Some(RmFailure {
                kind: kind::RM_CALL,
                code: 0x2080_012f,
                status: 0x56,
                hint: 0x2a80_0001,
            })
        );
        // Success: nothing.
        let (req, resp) = ioctl(rc::ESC_RM_CONTROL, &data, 0, &data);
        assert_eq!(rm_ioctl_failure(&req, &resp, resp.len()), None);
    }

    #[test]
    fn rm_alloc_free_and_map_status_offsets() {
        let data = rc::nvos64(1, 2, 0xcafe_0003, 0xc86f, 0);
        let mut reply = data;
        reply[rc::NVOS64_STATUS_AT..rc::NVOS64_STATUS_AT + 4].copy_from_slice(&0x51u32.to_le_bytes());
        let (req, resp) = ioctl(rc::ESC_RM_ALLOC, &data, 0, &reply);
        let f = rm_ioctl_failure(&req, &resp, resp.len()).unwrap();
        assert_eq!((f.kind, f.code, f.status, f.hint), (kind::RM_CALL, 0xc86f, 0x51, 0x2bfe_0003));

        let data = rc::nvos00(1, 2, 3);
        let mut reply = data;
        reply[rc::NVOS00_STATUS_AT..rc::NVOS00_STATUS_AT + 4].copy_from_slice(&0x1fu32.to_le_bytes());
        let (req, resp) = ioctl(rc::ESC_RM_FREE, &data, 0, &reply);
        let f = rm_ioctl_failure(&req, &resp, resp.len()).unwrap();
        assert_eq!((f.code, f.status, f.hint), (rc::ESC_RM_FREE, 0x1f, 0x2900_0003));

        let data = [0u8; rc::NVOS33_FD_BYTES];
        let mut reply = data;
        reply[rc::NVOS33_STATUS_AT..rc::NVOS33_STATUS_AT + 4].copy_from_slice(&0x11u32.to_le_bytes());
        let (req, resp) = ioctl(rc::ESC_RM_MAP_MEMORY, &data, 0, &reply);
        assert_eq!(rm_ioctl_failure(&req, &resp, resp.len()).unwrap().status, 0x11);
    }

    #[test]
    fn batch_failures_name_each_failed_entry() {
        use crate::rm_batch as rb;
        // Two controls: the first succeeds, the second RM refuses (0x1f).
        let ok = rc::nvos54(1, 0x10, 0xa06f_0103, 0);
        let bad = rc::nvos54(1, 0x20, 0x0080_0289, 0);
        let (req_a, resp_a) = ioctl(rc::ESC_RM_CONTROL, &ok, 0, &ok);
        let mut bad_reply = bad;
        bad_reply[rc::NVOS54_STATUS_AT..rc::NVOS54_STATUS_AT + 4].copy_from_slice(&0x1fu32.to_le_bytes());
        let (req_b, resp_b) = ioctl(rc::ESC_RM_CONTROL, &bad, 0, &bad_reply);
        let cap = 128u32;
        let mut req = vec![0u8; rc::MSG_HDR + rb::BATCH_REQ];
        req[0..4].copy_from_slice(&rb::MSG_RM_BATCH.to_le_bytes());
        req[16..20].copy_from_slice(&2u32.to_le_bytes());
        let mut resp = vec![0u8; rc::MSG_HDR + rb::BATCH_RESP];
        resp[0..4].copy_from_slice(&rb::MSG_RM_BATCH.to_le_bytes());
        resp[16..20].copy_from_slice(&2u32.to_le_bytes());
        resp[20..24].copy_from_slice(&2u32.to_le_bytes());
        for (sub, reply, flags) in [
            (&req_a, &resp_a, rb::REPLY_SERVED),
            (&req_b, &resp_b, rb::REPLY_SERVED | rb::REPLY_FAILED),
        ] {
            let mut h = [0u8; rb::ENTRY_HDR];
            h[0..4].copy_from_slice(&(sub.len() as u32).to_le_bytes());
            h[4..8].copy_from_slice(&cap.to_le_bytes());
            req.extend_from_slice(&h);
            req.extend_from_slice(sub);
            req.resize(req.len() + rb::pad8(sub.len()) - sub.len(), 0);
            let mut r = [0u8; rb::REPLY_HDR];
            r[0..4].copy_from_slice(&(reply.len() as u32).to_le_bytes());
            r[8..12].copy_from_slice(&flags.to_le_bytes());
            resp.extend_from_slice(&r);
            let mut slot = vec![0u8; rb::pad8(cap as usize)];
            slot[..reply.len()].copy_from_slice(reply);
            resp.extend_from_slice(&slot);
        }
        let mut seen = Vec::new();
        rm_batch_failures(&req, &resp, resp.len(), |f| seen.push(f));
        assert_eq!(
            seen,
            vec![RmFailure {
                kind: kind::RM_CALL,
                code: 0x0080_0289,
                status: 0x1f,
                hint: 0x2a00_0020,
            }]
        );
        // A truncated reply or a single call: nothing, no panic.
        let mut none = 0;
        rm_batch_failures(&req, &resp, 40, |_| none += 1);
        rm_batch_failures(&req_b, &resp_b, resp_b.len(), |_| none += 1);
        assert_eq!(none, 0);
    }

    #[test]
    fn wait_key_prefers_the_context() {
        assert_eq!(wait_key(0x4000_1280, 0x4000_31c0), 0x4000_31c0);
        assert_eq!(wait_key(0x4000_1280, 0), 0x4000_1280);
    }

    #[test]
    fn poll_pause_spares_short_waits() {
        assert_eq!(poll_pause(0, 0, 1000), PollPause::None);
        assert_eq!(poll_pause(1, 0, POLL_PAUSE_MIN_DEFAULT), PollPause::None);
        assert_eq!(poll_pause(1, 0, POLL_PAUSE_MIN_DEFAULT + 1), PollPause::Yield);
        assert_eq!(poll_pause(1, 4, 5), PollPause::Yield);
        assert_eq!(poll_pause(1, 4, 4), PollPause::None);
        assert_eq!(poll_pause(200, 0, 100), PollPause::DelayUs(200));
        assert_eq!(poll_pause(u32::MAX - 1, 0, 100), PollPause::DelayUs(POLL_PAUSE_MAX_US));
    }

    #[test]
    fn wait_streaks_report_long_spins_once_per_doubling() {
        let mut w = WaitStreaks::new();
        assert_eq!(w.poll(0xb87b_3200, 100), None);
        assert_eq!(w.poll(0xb87b_3200, 150), None);
        // A new token starts a new streak.
        assert_eq!(w.poll(0x3cb_bfc2, 200), None);
        let last = w.last();
        assert_eq!((last.token, last.count, last.ms()), (0x3cb_bfc2, 1, 0));
        assert_eq!((w.max_ms, w.max_token), (50, 0xb87b_3200));
        let mut reports = Vec::new();
        for t in (200..=4500).step_by(10) {
            if let Some(ms) = w.poll(0x3cb_bfc2, t) {
                reports.push(ms);
            }
        }
        assert_eq!(reports, vec![1000, 2000, 4000]);
        assert_eq!((w.max_ms, w.max_token, w.max_count), (4300, 0x3cb_bfc2, 432));
        assert_eq!(w.polls, 3 + 431);
        // A gap starts the token over.
        assert_eq!(w.poll(0x3cb_bfc2, 4700), None);
        assert_eq!(w.last().ms(), 0);
        // The clock wrapping does not panic.
        let mut w = WaitStreaks::new();
        w.poll(1, u32::MAX - 5);
        assert_eq!(w.poll(1, 10), None);
        assert_eq!(w.last().ms(), 16);
    }

    #[test]
    fn interleaved_waits_keep_their_own_streaks() {
        let mut w = WaitStreaks::new();
        let mut reports = Vec::new();
        for t in (0..=1200).step_by(5) {
            for token in [10u32, 20] {
                if let Some(ms) = w.poll(token, t) {
                    reports.push((token, ms));
                }
            }
        }
        assert_eq!(reports, vec![(10, 1000), (20, 1000)]);
        // Five tokens: the stalest slot goes.
        let mut w = WaitStreaks::new();
        for (i, token) in [1u32, 2, 3, 4, 5].iter().enumerate() {
            w.poll(*token, i as u32);
        }
        let tokens: Vec<u32> = w.streaks.iter().map(|s| s.token).collect();
        assert_eq!(tokens, vec![5, 2, 3, 4]);
    }

    #[test]
    fn host_refusal_wins_and_short_replies_say_nothing() {
        let data = rc::nvos54(1, 2, 0x906f_0101, 16);
        let (req, resp) = ioctl(rc::ESC_RM_CONTROL, &data, (-22i32) as u32, &[]);
        let f = rm_ioctl_failure(&req, &resp, resp.len()).unwrap();
        assert_eq!((f.kind, f.code, f.status), (kind::RM_HOST, 0x906f_0101, (-22i32) as u32));
        // RM status not in the reply: no verdict.
        let (req, resp) = ioctl(rc::ESC_RM_CONTROL, &data, 0, &[0u8; 8]);
        assert_eq!(rm_ioctl_failure(&req, &resp, resp.len()), None);
        // An escape without an RM status (CARD_INFO) only reports host refusals.
        let (req, resp) = ioctl(rc::ESC_CARD_INFO, &[0u8; 8], 0, &[0xffu8; 64]);
        assert_eq!(rm_ioctl_failure(&req, &resp, resp.len()), None);
        // Garbage never panics.
        for len in 0..64 {
            let junk = vec![0xa5u8; len];
            let _ = rm_ioctl_failure(&junk, &junk, len + 10);
        }
    }
}

//! RM fence handles: the pure rules behind owning a handle that a forwarded
//! `SEMSURF_FENCE_CREATE` (nvidia-drm ioctl 0x55) returns.
//!
//! The host turns an RM semaphore value into a one-shot backend handle (see
//! `docs/SYNC.md`, `host/backend/device/src/nvidia/fence.rs`). The guest never
//! `Open`s it: the handle arrives inside an `Ioctl` reply, in the same handle
//! namespace as `Open` handles, and the host sends one `EventReady` for it when
//! the semaphore reaches the value (or the host driver times it out). The KMD
//! must therefore
//!
//! * recognise the create precisely ([`is_fence_create`]) and read the handle out
//!   of the reply with every length checked ([`fence_handle_from_reply`]);
//! * keep it apart from device files ([`DEVICE_TYPE_FENCE`]), so nothing but
//!   `EVENT_REGISTER` and `Close` ever treats it as a file;
//! * not lose an `EventReady` that races the reply ([`FenceBook`]).
//!
//! # The race `FenceBook` closes
//!
//! A fence made for a semaphore value that is already reached signals at once.
//! The `EventReady` travels on the event queue, the reply on the control queue,
//! and both are consumed by the same DPC, while the reply's waiter is a thread
//! that has to be scheduled before it can record the handle as owned. So the
//! event can be consumed while the handle is still unknown, and the ordinary
//! latch (a flag on an owned handle) has nothing to latch on. `FenceBook` keeps
//! such unowned notifications for exactly as long as some `SEMSURF_FENCE_CREATE`
//! is between "forwarded" and "recorded", and hands the matching one over when
//! the handle is recorded. With no create in flight nothing is kept, so a stale
//! notification for a closed file cannot wait around to be taken for a later
//! fence that reuses its number.
//!
//! Pure functions and a fixed-size table: no allocation, no wdk, safe under the
//! KMD's spinlock.

/// The `device_type` the KMD records a fence handle under. Not a value the host
/// accepts in `Open` (255 control, 0..=254 GPUs, 256 UVM, 257 UVM tools, 258
/// modeset, 512+ DRM nodes), so no `Open` can forge one; and below
/// [`DEVICE_TYPE_DRI_FIRST`], so no check for "a DRM node" accepts it
/// (`ScanoutFlip.owner_handle`, the foreign-resource import).
pub const DEVICE_TYPE_FENCE: u32 = 511;
/// First `device_type` that names a DRM node (`512 + minor`).
pub const DEVICE_TYPE_DRI_FIRST: u32 = 512;

/// Whether `device_type` is a fence handle's.
pub const fn is_fence(device_type: u32) -> bool {
    device_type == DEVICE_TYPE_FENCE
}

/// Whether `device_type` names a DRM node.
pub const fn is_dri_node(device_type: u32) -> bool {
    device_type >= DEVICE_TYPE_DRI_FIRST
}

/// The low 16 bits of the ioctl number the guest sends for
/// `DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE`: `('d' << 8) | (DRM_COMMAND_BASE 0x40 +
/// 0x15)`. The host decodes the same two bytes and ignores the direction/size
/// bits (`dispatch` in `nvidia/ioctl.rs`), so this is as permissive as the host
/// and no more: a request the host treats as a fence create is one the KMD does.
pub const CMD_FENCE_CREATE_LOW16: u32 = 0x6455;
/// `struct drm_nvidia_semsurf_fence_create_params`: u32 ctx, u32 timeout_ms, u64
/// wait_value, s32 fd, u32 pad. The host refuses any other `data_len`.
pub const FENCE_CREATE_DATA_LEN: u32 = 24;
/// Offset of `fd` in that struct (request and reply). The host overwrites it with
/// the new backend handle.
pub const FENCE_FD_OFFSET: usize = 16;
/// Config `features` bit `NVGPU_CFG_DRM_FENCES` (bit 11). Without it a host
/// passes the ioctl through to the host driver and the `fd` field would be a
/// descriptor number of the backend process, not a handle: never adopt it.
pub const NVGPU_CFG_DRM_FENCES: u32 = 1 << 11;

/// `MsgHeader` of an `Ioctl` reply.
const MSG_HDR: usize = 16;
/// Where the reply's data block starts: `MsgHeader` + `IoctlResp { data_len,
/// nested_len, deep_len }`.
pub const REPLY_DATA: usize = MSG_HDR + 12;
/// Offset of the new handle in the whole reply message.
pub const REPLY_HANDLE_AT: usize = REPLY_DATA + FENCE_FD_OFFSET;

/// Whether a forwarded `Ioctl` is a `SEMSURF_FENCE_CREATE` whose reply carries a
/// handle to own: the number, the length and the file kind (an owned DRM node)
/// must all match, and the device must serve fences.
pub const fn is_fence_create(
    cmd: u32,
    data_len: u32,
    handle_device_type: u32,
    cfg_features: u32,
) -> bool {
    cmd & 0xFFFF == CMD_FENCE_CREATE_LOW16
        && data_len == FENCE_CREATE_DATA_LEN
        && is_dri_node(handle_device_type)
        && cfg_features & NVGPU_CFG_DRM_FENCES != 0
}

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}

/// The fence handle in a `SEMSURF_FENCE_CREATE` reply of `n` valid bytes in
/// `resp`, or `None` if the reply is not a success that carries one: the host
/// status must be 0, the reply must be long enough to hold the whole 24-byte
/// data block, `IoctlResp.data_len` must be 24 (the host sizes the split from the
/// request), and the handle must be nonzero and not the `-1` the host puts in a
/// field it never wrote.
pub fn fence_handle_from_reply(resp: &[u8], n: usize) -> Option<u32> {
    let resp = resp.get(..n)?;
    if n < REPLY_DATA + FENCE_CREATE_DATA_LEN as usize {
        return None;
    }
    // MsgHeader { msg_type @0, handle @4, status @8, pad @12 }.
    if rd_u32(resp, 8)? != 0 {
        return None;
    }
    // IoctlResp.data_len @16.
    if rd_u32(resp, MSG_HDR)? != FENCE_CREATE_DATA_LEN {
        return None;
    }
    match rd_u32(resp, REPLY_HANDLE_AT)? {
        0 | u32::MAX => None,
        h => Some(h),
    }
}

/// Unowned `EventReady`s kept while creates are in flight. The window is one
/// escape round trip, so a few threads' worth is plenty. Kept small on purpose:
/// the book sits by value in `VirtioGpu`, which `VirtioGpu::init` builds on the
/// boot stack (see `tools/kmd-frame-sizes.ps1`); 16 entries are 64 bytes.
pub const EARLY_CAP: usize = 16;

/// What [`FenceBook::note_ready`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Noted {
    /// No create is in flight: nothing is kept (the caller drops it).
    NotTracking,
    /// Kept (or already kept) until the create that may own it is recorded.
    Kept,
    /// The table is full: this notification is lost. Cannot happen unless dozens
    /// of fences fire inside one create's round trip; counted by the caller.
    Overflow,
}

/// Creates in flight, and the notifications that arrived for handles nobody owns
/// yet. See the module docs.
pub struct FenceBook {
    inflight: u32,
    len: usize,
    early: [u32; EARLY_CAP],
    /// The `EventReady` status kept with each early handle (0 or the fence's error).
    early_status: [i32; EARLY_CAP],
}

impl Default for FenceBook {
    fn default() -> Self {
        Self::new()
    }
}

impl FenceBook {
    pub const fn new() -> Self {
        Self {
            inflight: 0,
            len: 0,
            early: [0; EARLY_CAP],
            early_status: [0; EARLY_CAP],
        }
    }

    /// Creates between `begin` and `finish`.
    pub fn inflight(&self) -> u32 {
        self.inflight
    }

    /// Notifications kept.
    pub fn kept(&self) -> usize {
        self.len
    }

    /// A `SEMSURF_FENCE_CREATE` is about to be forwarded. Call BEFORE the host can
    /// see it, so no notification for its fence can arrive unobserved.
    pub fn begin(&mut self) {
        self.inflight = self.inflight.saturating_add(1);
    }

    /// `EventReady{handle}` arrived for a handle no process has open.
    pub fn note_ready(&mut self, handle: u32) -> Noted {
        self.note_ready_status(handle, 0)
    }

    /// As [`Self::note_ready`], keeping the fence's status (0 or its error) so an
    /// early error fire is not recorded as a success.
    pub fn note_ready_status(&mut self, handle: u32, status: i32) -> Noted {
        if self.inflight == 0 {
            return Noted::NotTracking;
        }
        if self.early[..self.len].contains(&handle) {
            return Noted::Kept;
        }
        match self.early.get_mut(self.len) {
            Some(slot) => {
                *slot = handle;
                self.early_status[self.len] = status;
                self.len += 1;
                Noted::Kept
            }
            None => Noted::Overflow,
        }
    }

    /// A create is over. `Some(handle)`: the host answered with this handle and it
    /// is being recorded now; the result says whether it had already fired.
    /// `None`: the create failed or answered nothing usable. Either way the
    /// create stops counting as in flight, and once none is left every kept
    /// notification is discarded.
    pub fn finish(&mut self, handle: Option<u32>) -> bool {
        self.finish_status(handle).is_some()
    }

    /// As [`Self::finish`], returning the status the early fire carried.
    pub fn finish_status(&mut self, handle: Option<u32>) -> Option<i32> {
        let mut fired = None;
        if let Some(h) = handle {
            if let Some(i) = self.early[..self.len].iter().position(|&x| x == h) {
                fired = Some(self.early_status[i]);
                self.len -= 1;
                self.early[i] = self.early[self.len];
                self.early_status[i] = self.early_status[self.len];
            }
        }
        self.inflight = self.inflight.saturating_sub(1);
        if self.inflight == 0 {
            self.len = 0;
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRI: u32 = 512;

    /// The reply the host writes for a successful create: `MsgHeader` (type 3,
    /// handle of the DRM file, status 0) | `IoctlResp { 24, 0, 0 }` | the 24-byte
    /// parameter block with the new handle in `fd`.
    fn reply(status: i32, data_len: u32, handle: u32) -> [u8; 52] {
        let mut r = [0u8; 52];
        r[0..4].copy_from_slice(&3u32.to_le_bytes());
        r[4..8].copy_from_slice(&7u32.to_le_bytes());
        r[8..12].copy_from_slice(&status.to_le_bytes());
        r[16..20].copy_from_slice(&data_len.to_le_bytes());
        // ctx 3, timeout 0, wait_value 9 (what the host's own test sends back).
        r[28..32].copy_from_slice(&3u32.to_le_bytes());
        r[36..44].copy_from_slice(&9u64.to_le_bytes());
        r[44..48].copy_from_slice(&handle.to_le_bytes());
        r
    }

    #[test]
    fn layout_constants_match_the_host_structs() {
        assert_eq!(REPLY_DATA, 28);
        // fd @16 of a 24-byte block, behind the 28-byte reply prefix.
        assert_eq!(REPLY_HANDLE_AT, 44);
        assert_eq!(REPLY_DATA + FENCE_CREATE_DATA_LEN as usize, 52);
        // ('d' << 8) | 0x55, as the host decodes `ioc_type` and `escape`.
        assert_eq!(CMD_FENCE_CREATE_LOW16, (u32::from(b'd') << 8) | 0x55);
        assert_eq!(NVGPU_CFG_DRM_FENCES, 1 << 11);
    }

    #[test]
    fn device_types_are_disjoint_and_a_fence_is_no_drm_node() {
        assert!(is_fence(DEVICE_TYPE_FENCE));
        assert!(!is_dri_node(DEVICE_TYPE_FENCE));
        assert!(DEVICE_TYPE_FENCE < DEVICE_TYPE_DRI_FIRST);
        // The values the host knows: none is a fence.
        for t in [0u32, 1, 254, 255, 256, 257, 258, 512, 513, 1024] {
            assert!(!is_fence(t), "{t}");
        }
        assert!(is_dri_node(512) && is_dri_node(600));
        assert!(!is_dri_node(511) && !is_dri_node(258) && !is_dri_node(0));
    }

    #[test]
    fn only_the_create_on_a_drm_node_of_a_fence_serving_device_is_a_create() {
        let f = NVGPU_CFG_DRM_FENCES;
        // DRM_IOWR(0x55, 24) as Linux encodes it, and the same number with other
        // direction/size bits (the host ignores them, so the KMD does too).
        assert!(is_fence_create(0xC018_6455, 24, DRI, f));
        assert!(is_fence_create(0x0000_6455, 24, DRI, f));
        assert!(is_fence_create(0xC018_6455, 24, 600, f | 1));
        // Other nvidia-drm numbers: ctx create (0x54), wait (0x56), attach (0x57).
        for nr in [0x54u32, 0x56, 0x57, 0x45, 0x00, 0xFF] {
            assert!(!is_fence_create(0xC018_6400 | nr, 24, DRI, f), "{nr:#x}");
        }
        // Same low byte, another type: an RM escape 0x55 on 'F' is not ours.
        assert!(!is_fence_create(0xC018_4655, 24, DRI, f));
        // The host refuses any data_len but 24.
        for len in [0u32, 8, 16, 23, 25, 32] {
            assert!(!is_fence_create(0xC018_6455, len, DRI, f), "{len}");
        }
        // Not a DRM node: control, GPU, UVM, a fence itself.
        for t in [0u32, 255, 256, 257, 258, DEVICE_TYPE_FENCE] {
            assert!(!is_fence_create(0xC018_6455, 24, t, f), "{t}");
        }
        // No DRM_FENCES bit: the host would hand back a descriptor number.
        assert!(!is_fence_create(0xC018_6455, 24, DRI, 0));
        assert!(!is_fence_create(0xC018_6455, 24, DRI, !f));
    }

    #[test]
    fn the_handle_is_read_from_the_fd_field_of_a_successful_reply() {
        let r = reply(0, 24, 0x1234);
        assert_eq!(fence_handle_from_reply(&r, r.len()), Some(0x1234));
        // Trailing bytes the buffer holds past `n` are not the reply.
        let mut big = [0xEEu8; 64];
        big[..52].copy_from_slice(&r);
        assert_eq!(fence_handle_from_reply(&big, 52), Some(0x1234));
        assert_eq!(fence_handle_from_reply(&big, 60), Some(0x1234));
    }

    #[test]
    fn a_reply_that_is_not_a_clean_success_yields_no_handle() {
        // Host error (EAGAIN past 4096 fences, EIO, a bad size).
        assert_eq!(fence_handle_from_reply(&reply(-11, 24, 5), 52), None);
        assert_eq!(fence_handle_from_reply(&reply(1, 24, 5), 52), None);
        // The split the host echoed is not the 24-byte block.
        assert_eq!(fence_handle_from_reply(&reply(0, 16, 5), 52), None);
        assert_eq!(fence_handle_from_reply(&reply(0, 0, 5), 52), None);
        // A handle the host never wrote, or 0.
        assert_eq!(fence_handle_from_reply(&reply(0, 24, 0), 52), None);
        assert_eq!(fence_handle_from_reply(&reply(0, 24, u32::MAX), 52), None);
        // Short replies: the handle byte must be inside `n`, whatever the buffer.
        let r = reply(0, 24, 5);
        for n in [0usize, 15, 16, 27, 28, 44, 47, 48, 51] {
            assert_eq!(fence_handle_from_reply(&r, n), None, "n={n}");
        }
        // `n` larger than the buffer is not trusted.
        assert_eq!(fence_handle_from_reply(&r[..40], 52), None);
        assert_eq!(fence_handle_from_reply(&[], 0), None);
    }

    #[test]
    fn nothing_is_kept_while_no_create_is_in_flight() {
        let mut b = FenceBook::new();
        assert_eq!(b.note_ready(5), Noted::NotTracking);
        assert_eq!(b.kept(), 0);
        // So a stale notification cannot wait for a later create to reuse 5.
        b.begin();
        assert!(!b.finish(Some(5)));
    }

    #[test]
    fn a_notification_before_the_handle_is_recorded_is_handed_over_once() {
        let mut b = FenceBook::new();
        b.begin();
        assert_eq!(b.note_ready(5), Noted::Kept);
        assert_eq!(b.note_ready(5), Noted::Kept, "twice is still one");
        assert_eq!(b.kept(), 1);
        assert!(b.finish(Some(5)));
        assert_eq!(b.inflight(), 0);
        assert_eq!(b.kept(), 0);
        // Not again for a fence that reuses the number.
        b.begin();
        assert!(!b.finish(Some(5)));
    }

    #[test]
    fn a_create_takes_only_its_own_handle() {
        let mut b = FenceBook::new();
        b.begin();
        b.begin();
        assert_eq!(b.note_ready(5), Noted::Kept);
        assert_eq!(b.note_ready(6), Noted::Kept);
        assert!(b.finish(Some(6)));
        assert_eq!(b.inflight(), 1);
        // 5 stays for the create still in flight.
        assert_eq!(b.kept(), 1);
        assert!(b.finish(Some(5)));
        assert_eq!(b.kept(), 0);
    }

    #[test]
    fn a_failed_create_discards_what_nobody_can_claim_once_none_is_left() {
        let mut b = FenceBook::new();
        b.begin();
        b.begin();
        assert_eq!(b.note_ready(9), Noted::Kept);
        assert!(!b.finish(None));
        assert_eq!(b.kept(), 1, "the other create may own 9");
        assert!(!b.finish(Some(10)));
        assert_eq!(b.kept(), 0, "no create left: nobody can");
        assert_eq!(b.inflight(), 0);
    }

    #[test]
    fn an_early_fire_keeps_its_status_through_the_hand_over() {
        let mut b = FenceBook::new();
        b.begin();
        b.begin();
        assert_eq!(b.note_ready_status(5, -62), Noted::Kept);
        assert_eq!(b.note_ready_status(6, 0), Noted::Kept);
        assert_eq!(b.note_ready_status(5, 0), Noted::Kept, "twice is still one");
        assert_eq!(b.finish_status(Some(6)), Some(0));
        assert_eq!(b.finish_status(Some(5)), Some(-62));
        assert_eq!(b.finish_status(Some(5)), None);
    }

    #[test]
    fn an_unbalanced_finish_does_not_wrap() {
        let mut b = FenceBook::new();
        assert!(!b.finish(None));
        assert_eq!(b.inflight(), 0);
        b.begin();
        assert_eq!(b.inflight(), 1);
    }

    #[test]
    fn a_full_table_reports_the_loss_and_keeps_what_it_has() {
        let mut b = FenceBook::new();
        b.begin();
        for h in 1..=EARLY_CAP as u32 {
            assert_eq!(b.note_ready(h), Noted::Kept);
        }
        assert_eq!(b.note_ready(1000), Noted::Overflow);
        assert_eq!(b.note_ready(1), Noted::Kept, "already kept is not a loss");
        assert!(b.finish(Some(EARLY_CAP as u32)));
        assert_eq!(b.kept(), 0);
    }

    #[test]
    fn handing_over_one_leaves_the_others_intact_until_the_last_create_ends() {
        let mut b = FenceBook::new();
        b.begin();
        b.begin();
        b.begin();
        for h in [11u32, 12, 13] {
            assert_eq!(b.note_ready(h), Noted::Kept);
        }
        assert!(b.finish(Some(12)));
        assert!(b.finish(Some(13)));
        assert!(b.finish(Some(11)));
        assert_eq!(b.kept(), 0);
    }
}

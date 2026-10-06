//! The KMD's own RM client: the pure half.
//!
//! `HELIOS_ESCAPE_NVRM` makes the KMD a pipe that forwards a user-mode RM client's
//! host messages (`docs/nvrm-escape.md`). This module is what the KMD needs to be
//! an RM client ITSELF over the same pipe, so that allocations the KMD owns (first:
//! the VidPn primary's scanout surface) can come from RM video memory instead of
//! Venus. Design, stages and the failure matrix: `docs/kmd-rm-client.md`.
//!
//! Nothing here touches the transport, the clock or a lock. It holds
//!
//! * the exact host wire messages and RM payloads the client sends, built and
//!   parsed with every length checked ([`wire`] and the `build_*` / `parse_*`
//!   functions). They follow librmclient (`guest/rmclient/src/rmclient.c`,
//!   `win_wire.h`, `nv_ioctl_defs.h`) and the host backend's parsing
//!   (`host/backend/device/src/nvidia`), byte for byte; the tests pin every offset
//!   to the value `offsetof` gives on the C headers;
//! * the surface geometry rules ([`surface_layout`], [`adopt_alloc_reply`]);
//! * the client's state machine ([`Client`]): which step is next, what a failure
//!   does, what must be closed, and when the client starts over. The driver's I/O
//!   layer (`virtio/rm_client.rs`) is a loop that asks [`Client::next`], performs
//!   the step and reports with [`Client::finish`]; every decision lives here.
//!
//! # Failing closed
//!
//! A failure in bring-up or in the surface path makes the client [`Phase::Dead`]
//! until the transport generation changes: the caller then keeps using Venus, which
//! is what it did before this client existed. A failure of the OPTIONAL CPU view or
//! the probe flip ([`Step::OpenMapCh`] and later) does not: the surface is still
//! valid for a flip, and the view is simply given up.

use crate::foreign_scanout::{Layout, FOURCC_XRGB8888, MAX_DIM, MAX_STRIDE, MIN_DIM};

// ---- host wire numbers --------------------------------------------------------

/// `MsgHeader` (`msg_type`, `handle`, `status`, `padding`).
pub const MSG_HDR: usize = 16;
/// `IoctlReq` after the header: `cmd`, `data_len`, `nested_offset`, `nested_len`,
/// `deep_ptr_offset`, `deep_len`.
pub const IOCTL_REQ: usize = 24;
/// `IoctlResp` after a reply header: `data_len`, `nested_len`, `deep_len`.
pub const IOCTL_RESP: usize = 12;
/// Where an `Ioctl` reply's data block starts.
pub const REPLY_DATA: usize = MSG_HDR + IOCTL_RESP;

pub const MSG_OPEN: u32 = 1;
pub const MSG_CLOSE: u32 = 2;
pub const MSG_IOCTL: u32 = 3;
pub const MSG_GET_SYS_FILES: u32 = 7;

/// `Open` `device_type`: the RM control file.
pub const DEV_CTL: u32 = 255;
/// `Open` `device_type` of DRM node `n` is `512 + n` (the host's `GetSysFiles` DRI list).
pub const DEV_DRI_BASE: u32 = 512;
/// Largest `device_type` an `Open` of a GPU channel may carry (the minor).
pub const DEV_GPU_MAX: u32 = 254;
/// `OpenReq.flags`: `O_RDWR`, which is all the host reads.
pub const OPEN_FLAGS_RDWR: u32 = 2;

/// Largest reply a step may ask for besides the file listing: the biggest is
/// `CARD_INFO`'s 32 cards of 72 bytes.
pub const CARD_INFO_BYTES: usize = 32 * 72;
/// What the host's `GetSysFiles` reply is sized at (as librmclient and the Linux
/// module do).
pub const SYS_FILES_CAP: usize = 128 * 1024;

// ---- NVIDIA escapes and DRM ioctls --------------------------------------------

pub const ESC_CARD_INFO: u32 = 200;
pub const ESC_REGISTER_FD: u32 = 201;
pub const ESC_CHECK_VERSION_STR: u32 = 210;
pub const ESC_RM_FREE: u32 = 0x29;
pub const ESC_RM_CONTROL: u32 = 0x2A;
pub const ESC_RM_ALLOC: u32 = 0x2B;
pub const ESC_RM_MAP_MEMORY: u32 = 0x4E;
pub const ESC_RM_UNMAP_MEMORY: u32 = 0x4F;

/// A Linux ioctl number: `_IOC(dir, type, nr, size)`; `dir` 1 = write, 2 = read,
/// 3 = both.
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    ((dir & 3) << 30) | ((size & 0x3fff) << 16) | ((ty & 0xff) << 8) | (nr & 0xff)
}

/// `_IOWR('F', nr, size)`: how the Linux module forwards an NVIDIA escape and how
/// the host dispatches it (on the low byte, with `size` the top-level data length).
pub const fn nv_cmd(nr: u32, size: u32) -> u32 {
    ioc(3, b'F' as u32, nr, size)
}

/// `DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`: `DRM_IOWR(0x40 + 0x01, 32)`.
pub const DRM_IOCTL_GEM_IMPORT_NVKMS: u32 = ioc(3, b'd' as u32, 0x41, 32);
/// `DRM_IOCTL_GEM_CLOSE`: `DRM_IOW(0x09, 8)`.
pub const DRM_IOCTL_GEM_CLOSE: u32 = ioc(1, b'd' as u32, 0x09, 8);

const _: () = assert!(DRM_IOCTL_GEM_IMPORT_NVKMS == 0xC020_6441);
const _: () = assert!(DRM_IOCTL_GEM_CLOSE == 0x4008_6409);

// ---- RM classes and controls ---------------------------------------------------

pub const NV01_ROOT_CLIENT: u32 = 0x41;
pub const NV01_DEVICE_0: u32 = 0x80;
pub const NV20_SUBDEVICE_0: u32 = 0x2080;
pub const NV01_MEMORY_LOCAL_USER: u32 = 0x40;
/// `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD`.
pub const NV0000_CTRL_CMD_EXPORT_OBJECT_TO_FD: u32 = 0x3d05;
pub const EXPORT_OBJECT_TYPE_RM: u32 = 1;

/// The parameter-block sizes the host's allow-list demands of each class
/// (`host/backend/gen/src/rmallow`): a block of any other size is refused before
/// RM sees it.
pub const NV0080_ALLOC_BYTES: usize = 56;
pub const NV2080_ALLOC_BYTES: usize = 4;
pub const MEM_ALLOC_BYTES: usize = 128;
pub const EXPORT_PARAMS_BYTES: usize = 24;

pub const NVOS64_BYTES: usize = 48;
pub const NVOS00_BYTES: usize = 16;
pub const NVOS54_BYTES: usize = 32;
pub const NVOS33_FD_BYTES: usize = 56;
pub const NVOS34_BYTES: usize = 32;
pub const VERSION_BYTES: usize = 72;
pub const VERSION_STR_BYTES: usize = 64;
pub const GEM_IMPORT_BYTES: usize = 32;
pub const NVKMS_IMPORT_BYTES: usize = 28;

/// `status` offsets inside the top-level (data) block of a reply.
pub const NVOS64_STATUS_AT: usize = 40;
pub const NVOS00_STATUS_AT: usize = 12;
pub const NVOS54_STATUS_AT: usize = 28;
pub const NVOS33_STATUS_AT: usize = 40;
pub const NVOS34_STATUS_AT: usize = 24;

// ---- object handles the KMD chooses --------------------------------------------

/// RM object handles of the KMD's client. The root client's own handle is RM's to
/// choose (`hObjectNew = 0`), and librmclient's allocator lives at `0x5c000000`, so
/// these cannot collide with either; they are per client, and a client lives for one
/// transport generation.
pub const H_DEVICE: u32 = 0x4b4d_0001;
pub const H_SUBDEVICE: u32 = 0x4b4d_0002;
const H_MEMORY_BASE: u32 = 0x4b4d_1000;

/// The handle of the `n`th scanout surface of a client (distinct per surface, so a
/// free that did not reach RM can never be confused with the next surface).
pub const fn memory_handle(n: u32) -> u32 {
    H_MEMORY_BASE + (n & 0xff)
}

// ---- little-endian helpers -----------------------------------------------------

fn put32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// A `u32` at `at`, or `None` past the end.
pub fn get32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A `u64` at `at`, or `None` past the end.
pub fn get64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

// ---- host messages -------------------------------------------------------------

/// `MsgHeader { msg_type, handle, status = 0, padding = 0 }`.
fn header(out: &mut [u8], msg_type: u32, handle: u32) -> Option<()> {
    let h = out.get_mut(..MSG_HDR)?;
    h.fill(0);
    put32(h, 0, msg_type);
    put32(h, 4, handle);
    Some(())
}

/// `Open` of `device_type`: `MsgHeader | { device_type, flags }`. Returns the length.
pub fn build_open(out: &mut [u8], device_type: u32) -> Option<usize> {
    let n = MSG_HDR + 8;
    if out.len() < n {
        return None;
    }
    header(out, MSG_OPEN, 0)?;
    put32(out, MSG_HDR, device_type);
    put32(out, MSG_HDR + 4, OPEN_FLAGS_RDWR);
    Some(n)
}

/// `Close` of `handle`: a bare header.
pub fn build_close(out: &mut [u8], handle: u32) -> Option<usize> {
    header(out, MSG_CLOSE, handle)?;
    Some(MSG_HDR)
}

/// `GetSysFiles`: a bare header (the reply is a header-less stream).
pub fn build_get_sys_files(out: &mut [u8]) -> Option<usize> {
    header(out, MSG_GET_SYS_FILES, 0)?;
    Some(MSG_HDR)
}

/// An `Ioctl` of `cmd` on backend handle `handle` carrying `data` and, behind it,
/// `nested` (the block a pointer in `data` addresses). The nested block sits
/// straight after the data block and `nested_offset` is `data.len()` when there is
/// one, as the Linux module and librmclient lay it out. No deep block (the KMD
/// never sends one). Returns the request length.
pub fn build_ioctl(
    out: &mut [u8],
    handle: u32,
    cmd: u32,
    data: &[u8],
    nested: &[u8],
) -> Option<usize> {
    let data_len = u32::try_from(data.len()).ok()?;
    let nested_len = u32::try_from(nested.len()).ok()?;
    let total = MSG_HDR
        .checked_add(IOCTL_REQ)?
        .checked_add(data.len())?
        .checked_add(nested.len())?;
    if out.len() < total {
        return None;
    }
    header(out, MSG_IOCTL, handle)?;
    put32(out, MSG_HDR, cmd);
    put32(out, MSG_HDR + 4, data_len);
    put32(out, MSG_HDR + 8, if nested_len != 0 { data_len } else { 0 });
    put32(out, MSG_HDR + 12, nested_len);
    put32(out, MSG_HDR + 16, 0); // deep_ptr_offset
    put32(out, MSG_HDR + 20, 0); // deep_len
    let body = MSG_HDR + IOCTL_REQ;
    out.get_mut(body..body + data.len())?.copy_from_slice(data);
    out.get_mut(body + data.len()..total)?
        .copy_from_slice(nested);
    Some(total)
}

/// A parsed `Ioctl` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply<'a> {
    /// `MsgHeader.status`: 0, or the host's negative errno.
    pub status: i32,
    pub data: &'a [u8],
    pub nested: &'a [u8],
}

/// Why a reply could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// Shorter than a header, or than what its own lengths claim.
    Short,
    /// The host refused: `MsgHeader.status` is this (nonzero) value.
    Host(i32),
}

/// `MsgHeader.status` of any reply, or `None` if it is shorter than a header.
pub fn reply_status(resp: &[u8]) -> Option<i32> {
    get32(resp, 8).map(|v| v as i32)
}

/// Parse `MsgHeader | IoctlResp | data | nested | deep` (`resp` is the `n` valid
/// bytes). A header-only reply with a nonzero status is the host's refusal
/// ([`ReplyError::Host`]); a short reply that claims success is malformed
/// ([`ReplyError::Short`]), and so is one whose blocks overrun it.
pub fn parse_ioctl_reply(resp: &[u8]) -> Result<Reply<'_>, ReplyError> {
    let status = reply_status(resp).ok_or(ReplyError::Short)?;
    if status != 0 {
        return Err(ReplyError::Host(status));
    }
    let data_len = get32(resp, MSG_HDR).ok_or(ReplyError::Short)? as usize;
    let nested_len = get32(resp, MSG_HDR + 4).ok_or(ReplyError::Short)? as usize;
    let data_end = REPLY_DATA.checked_add(data_len).ok_or(ReplyError::Short)?;
    let nested_end = data_end.checked_add(nested_len).ok_or(ReplyError::Short)?;
    let data = resp.get(REPLY_DATA..data_end).ok_or(ReplyError::Short)?;
    let nested = resp.get(data_end..nested_end).ok_or(ReplyError::Short)?;
    Ok(Reply {
        status,
        data,
        nested,
    })
}

/// Why a step's RM answer is not a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RmError {
    /// The reply could not be read, or did not reach the status word.
    Malformed,
    /// The host refused the message (negative errno, as a positive number here).
    Host(u32),
    /// RM answered with this nonzero `NV_STATUS`.
    Status(u32),
}

/// RM's verdict of an answered call: the host's status was 0 (a [`Reply`] exists)
/// and the `status` word `at` bytes into the data block is 0.
pub fn rm_ok(reply: &Reply<'_>, status_at: usize) -> Result<(), RmError> {
    match get32(reply.data, status_at) {
        Some(0) => Ok(()),
        Some(s) => Err(RmError::Status(s)),
        None => Err(RmError::Malformed),
    }
}

/// [`parse_ioctl_reply`] plus [`rm_ok`], with the host's refusal mapped.
pub fn rm_reply(resp: &[u8], status_at: usize) -> Result<Reply<'_>, RmError> {
    let reply = match parse_ioctl_reply(resp) {
        Ok(r) => r,
        Err(ReplyError::Short) => return Err(RmError::Malformed),
        Err(ReplyError::Host(e)) => return Err(RmError::Host(e.unsigned_abs())),
    };
    rm_ok(&reply, status_at)?;
    Ok(reply)
}

/// The backend handle an `Open` reply carries: nonzero, `MsgHeader.status == 0`,
/// and small enough to be the positive `int` librmclient also insists on.
pub fn parse_open_reply(resp: &[u8]) -> Option<u32> {
    if reply_status(resp)? != 0 {
        return None;
    }
    let h = get32(resp, 4)?;
    (h != 0 && h <= 0x7fff_ffff).then_some(h)
}

// ---- RM payloads ---------------------------------------------------------------

/// `NVOS64_PARAMETERS`, the top-level block of `NV_ESC_RM_ALLOC`. The pointer
/// fields stay 0: the host puts its own pointer at `pAllocParms` and the nested
/// block's length is the message's `nested_len`.
pub fn nvos64(
    h_root: u32,
    h_parent: u32,
    h_new: u32,
    h_class: u32,
    params_size: u32,
) -> [u8; NVOS64_BYTES] {
    let mut a = [0u8; NVOS64_BYTES];
    put32(&mut a, 0, h_root);
    put32(&mut a, 4, h_parent);
    put32(&mut a, 8, h_new);
    put32(&mut a, 12, h_class);
    put32(&mut a, 32, params_size);
    a
}

/// The root client allocation: RM picks the handle (`hObjectNew = 0`), no parent, no
/// parameters.
pub fn alloc_root() -> [u8; NVOS64_BYTES] {
    nvos64(0, 0, 0, NV01_ROOT_CLIENT, 0)
}

/// `hObjectNew` of an answered `NV_ESC_RM_ALLOC` (what RM chose for the root client).
pub fn alloc_new_handle(reply: &Reply<'_>) -> Option<u32> {
    let h = get32(reply.data, 8)?;
    (h != 0).then_some(h)
}

/// `NV0080_ALLOC_PARAMETERS` for device instance 0, all else zero.
pub fn device_params() -> [u8; NV0080_ALLOC_BYTES] {
    [0u8; NV0080_ALLOC_BYTES]
}

/// `NV2080_ALLOC_PARAMETERS { subDeviceId = 0 }`.
pub fn subdevice_params() -> [u8; NV2080_ALLOC_BYTES] {
    [0u8; NV2080_ALLOC_BYTES]
}

/// `NVOS00_PARAMETERS`: the free of object `h_old` under `h_parent`.
pub fn nvos00(h_root: u32, h_parent: u32, h_old: u32) -> [u8; NVOS00_BYTES] {
    let mut a = [0u8; NVOS00_BYTES];
    put32(&mut a, 0, h_root);
    put32(&mut a, 4, h_parent);
    put32(&mut a, 8, h_old);
    a
}

/// `NVOS54_PARAMETERS`: `NV_ESC_RM_CONTROL` of `cmd` on `h_object` with a parameter
/// block of `params_size` bytes (the message's nested block).
pub fn nvos54(h_client: u32, h_object: u32, cmd: u32, params_size: u32) -> [u8; NVOS54_BYTES] {
    let mut a = [0u8; NVOS54_BYTES];
    put32(&mut a, 0, h_client);
    put32(&mut a, 4, h_object);
    put32(&mut a, 8, cmd);
    put32(&mut a, 24, params_size);
    a
}

/// `NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS`: export RM object `h_object`
/// (under device `h_device`) to the control file whose backend handle is `fd`.
pub fn export_params(h_device: u32, h_object: u32, fd: u32) -> [u8; EXPORT_PARAMS_BYTES] {
    let mut a = [0u8; EXPORT_PARAMS_BYTES];
    put32(&mut a, 0, EXPORT_OBJECT_TYPE_RM);
    put32(&mut a, 4, h_device);
    put32(&mut a, 8, h_device); // hParent: the device
    put32(&mut a, 12, h_object);
    put32(&mut a, 16, fd);
    a
}

/// `nv_ioctl_register_fd_t { ctl_fd }`: tie a GPU channel to the control channel.
pub fn register_fd_params(ctl_handle: u32) -> [u8; 4] {
    ctl_handle.to_le_bytes()
}

/// `NV_ESC_CHECK_VERSION_STR` block: `cmd`, `reply`, 64 bytes of version string.
pub const VERSION_CMD_STRICT: u32 = 0;
pub const VERSION_CMD_QUERY: u32 = b'2' as u32;

pub fn version_params(cmd: u32, version: &[u8; VERSION_STR_BYTES]) -> [u8; VERSION_BYTES] {
    let mut a = [0u8; VERSION_BYTES];
    put32(&mut a, 0, cmd);
    a[8..8 + VERSION_STR_BYTES].copy_from_slice(version);
    a
}

/// The version string of a `QUERY` reply (the data block), NUL-padded as RM wrote
/// it; `None` if the block is short or the string empty.
pub fn parse_version_reply(data: &[u8]) -> Option<[u8; VERSION_STR_BYTES]> {
    let s = data.get(8..8 + VERSION_STR_BYTES)?;
    let mut v = [0u8; VERSION_STR_BYTES];
    v.copy_from_slice(s);
    // Whatever follows the first NUL is not part of the string.
    let end = v
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(VERSION_STR_BYTES - 1);
    if end == 0 {
        return None;
    }
    for b in v.iter_mut().skip(end) {
        *b = 0;
    }
    Some(v)
}

/// The version string out of a whole `QUERY` reply (`resp` is its valid bytes), or all
/// zero ("no string") when the host refused it (`ReplyError::Host`), the reply is
/// short, or the string is empty. librmclient ignores a failed `QUERY` the same way:
/// the strict check is then skipped. Only a transport failure or a policy refusal of
/// the forward itself (not a reply) is fatal, and those never reach here.
pub fn version_from_query_reply(resp: &[u8]) -> [u8; VERSION_STR_BYTES] {
    parse_ioctl_reply(resp)
        .ok()
        .and_then(|r| parse_version_reply(r.data))
        .unwrap_or([0u8; VERSION_STR_BYTES])
}

/// What `NV_ESC_CARD_INFO` says of the first valid card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardInfo {
    pub gpu_id: u32,
    /// `minor_number`: the `device_type` an `Open` of the GPU's channel takes.
    pub minor: u32,
}

/// `nv_ioctl_card_info_t` is 72 bytes: `valid` @0, `gpu_id` @16, `minor_number` @56.
pub fn parse_card_info(data: &[u8]) -> Option<CardInfo> {
    for card in data.chunks_exact(72) {
        if card[0] != 0 {
            let gpu_id = get32(card, 16)?;
            let minor = get32(card, 56)?;
            if minor <= DEV_GPU_MAX {
                return Some(CardInfo { gpu_id, minor });
            }
        }
    }
    None
}

/// One render node of the host's `GetSysFiles` DRI section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriNode {
    /// `dev_info[0]`: the NVIDIA gpu id of the node's card.
    pub gpu_id: u32,
    pub slot: u32,
}

/// How many DRI records [`parse_dri_section`] keeps.
pub const MAX_DRI: usize = 8;
const DRI_FIXED: usize = 16 + 4 * 9;

/// The DRI render nodes in a `GetSysFiles` stream: the files section ends with a
/// `(0, 0)` record, then comes a `u32` count and that many records of `{ name_len,
/// major, minor, slot_index, dev_info[9] }` followed by the name
/// (`host/backend/device/src/nvidia/files.rs`). Returns how many were stored (in
/// stream order, so index `i` is the node an `Open` of `512 + i` names).
pub fn parse_dri_section(stream: &[u8], out: &mut [DriNode; MAX_DRI]) -> usize {
    let mut at = 0usize;
    // The files: (path_len, content_len, path, content)* then (0, 0).
    loop {
        let (Some(path_len), Some(content_len)) = (get32(stream, at), get32(stream, at + 4)) else {
            return 0;
        };
        at += 8;
        if path_len == 0 && content_len == 0 {
            break;
        }
        let Some(next) = at
            .checked_add(path_len as usize)
            .and_then(|v| v.checked_add(content_len as usize))
        else {
            return 0;
        };
        if next > stream.len() {
            return 0;
        }
        at = next;
    }
    let Some(count) = get32(stream, at) else {
        return 0;
    };
    at += 4;
    let mut kept = 0usize;
    for _ in 0..count {
        let (Some(name_len), Some(slot), Some(gpu_id)) = (
            get32(stream, at),
            get32(stream, at + 12),
            get32(stream, at + 16),
        ) else {
            return kept;
        };
        let Some(next) = at
            .checked_add(DRI_FIXED)
            .and_then(|v| v.checked_add(name_len as usize))
        else {
            return kept;
        };
        if name_len == 0 || next > stream.len() {
            return kept;
        }
        if let Some(slot_out) = out.get_mut(kept) {
            *slot_out = DriNode { gpu_id, slot };
            kept += 1;
        }
        at = next;
    }
    kept
}

/// Which DRI node to open: the first whose gpu id is the card's, else node 0. `None`
/// with no nodes at all.
pub fn pick_dri(nodes: &[DriNode], gpu_id: u32) -> Option<u32> {
    if nodes.is_empty() {
        return None;
    }
    let i = nodes.iter().position(|n| n.gpu_id == gpu_id).unwrap_or(0);
    Some(i as u32)
}

// ---- the scanout surface -------------------------------------------------------

/// Row pitch is rounded up to this (RM pads pitch-linear surfaces itself; asking for
/// an aligned pitch makes the answer equal the request for every width).
pub const PITCH_ALIGN: u32 = 256;
/// RM video memory is allocated in 64 KiB pages (`NVOS32_ATTR_PAGE_SIZE_BIG`).
pub const SIZE_ALIGN: u64 = 64 * 1024;
/// The largest surface this client allocates.
pub const MAX_SURFACE_BYTES: u64 = 256 << 20;

/// Pixel size of the surface: `XRGB8888`.
pub const BYTES_PER_PIXEL: u32 = 4;

/// The geometry of a pitch-linear `XRGB8888` surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub pitch: u32,
    /// Allocation size in bytes.
    pub size: u64,
}

/// The layout of a `width` x `height` surface, or `None` outside the extents the
/// scanout accepts or above [`MAX_SURFACE_BYTES`].
pub fn surface_layout(width: u32, height: u32) -> Option<SurfaceLayout> {
    if !(MIN_DIM..=MAX_DIM).contains(&width) || !(MIN_DIM..=MAX_DIM).contains(&height) {
        return None;
    }
    let row = u64::from(width) * u64::from(BYTES_PER_PIXEL);
    let pitch = (row + u64::from(PITCH_ALIGN) - 1) & !(u64::from(PITCH_ALIGN) - 1);
    if pitch > u64::from(MAX_STRIDE) {
        return None;
    }
    let size = (pitch * u64::from(height) + SIZE_ALIGN - 1) & !(SIZE_ALIGN - 1);
    if size > MAX_SURFACE_BYTES {
        return None;
    }
    Some(SurfaceLayout {
        width,
        height,
        pitch: pitch as u32,
        size,
    })
}

/// The host flip layout of a surface: `XRGB8888`, linear, no offset.
pub fn flip_layout(s: &SurfaceLayout) -> Layout {
    Layout {
        width: s.width,
        height: s.height,
        stride: s.pitch,
        offset: 0,
        fourcc: FOURCC_XRGB8888,
        modifier: 0,
    }
}

// `NVOS32_*` values of the allocation, as nvk-rm and `crm_scanout_smoke` use them
// for scanout video memory.
const NVOS32_TYPE_IMAGE: u32 = 0;
const NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE: u32 = 0x100;
const ATTR_PAGE_SIZE_SHIFT: u32 = 23;
const ATTR_PAGE_SIZE_BIG: u32 = 2;
const ATTR_LOCATION_SHIFT: u32 = 25;
const ATTR_LOCATION_VIDMEM: u32 = 0;
const ATTR_PHYSICALITY_SHIFT: u32 = 27;
/// `NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS`.
const ATTR_PHYSICALITY_ALLOW_NONCONTIG: u32 = 3;
const ATTR2_ZBC_PREFER_NO_ZBC: u32 = 2;
const ATTR2_GPU_CACHEABLE_SHIFT: u32 = 2;
const ATTR2_GPU_CACHEABLE_YES: u32 = 1;

/// `attr` the surface is allocated with: video memory, 64 KiB pages, not
/// necessarily contiguous.
pub const SURFACE_ATTR: u32 = (ATTR_LOCATION_VIDMEM << ATTR_LOCATION_SHIFT)
    | (ATTR_PAGE_SIZE_BIG << ATTR_PAGE_SIZE_SHIFT)
    | (ATTR_PHYSICALITY_ALLOW_NONCONTIG << ATTR_PHYSICALITY_SHIFT);
/// `attr2` of the surface: no ZBC, GPU cacheable.
pub const SURFACE_ATTR2: u32 =
    ATTR2_ZBC_PREFER_NO_ZBC | (ATTR2_GPU_CACHEABLE_YES << ATTR2_GPU_CACHEABLE_SHIFT);

const _: () = assert!(SURFACE_ATTR == 0x1900_0000);
const _: () = assert!(SURFACE_ATTR2 == 6);

/// `NV_MEMORY_ALLOCATION_PARAMS` of a pitch-linear scanout surface in video memory,
/// the way `crm_scanout_smoke` (and nvk-rm) allocate one.
pub fn mem_alloc_params(root: u32, s: &SurfaceLayout) -> [u8; MEM_ALLOC_BYTES] {
    let mut a = [0u8; MEM_ALLOC_BYTES];
    put32(&mut a, 0, root); // owner
    put32(&mut a, 4, NVOS32_TYPE_IMAGE);
    put32(&mut a, 8, NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE);
    put32(&mut a, 12, s.width);
    put32(&mut a, 16, s.height);
    put32(&mut a, 20, s.pitch);
    put32(&mut a, 24, SURFACE_ATTR);
    put32(&mut a, 28, SURFACE_ATTR2);
    put64(&mut a, 64, s.size);
    put64(&mut a, 72, SIZE_ALIGN); // alignment
    a
}

/// Why RM's answer to the memory allocation cannot be used as a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// The reply carries no parameter block (or a short one).
    Short,
    /// RM changed the pitch to something that cannot hold a row.
    Pitch,
    /// RM's size cannot hold `pitch * height`.
    Size,
}

/// What RM made, read back from the (updated) parameter block of the allocation:
/// RM may round `pitch` and `size` up. An answer that still holds the picture is
/// adopted (the flip names the pitch RM reports); anything else is refused.
pub fn adopt_alloc_reply(
    want: &SurfaceLayout,
    nested: &[u8],
) -> Result<SurfaceLayout, LayoutError> {
    let pitch = get32(nested, 20).ok_or(LayoutError::Short)?;
    let size = get64(nested, 64).ok_or(LayoutError::Short)?;
    let pitch = if pitch == 0 { want.pitch } else { pitch };
    if u64::from(pitch) < u64::from(want.width) * u64::from(BYTES_PER_PIXEL) || pitch > MAX_STRIDE {
        return Err(LayoutError::Pitch);
    }
    let size = if size == 0 { want.size } else { size };
    if size < u64::from(pitch) * u64::from(want.height) || size > MAX_SURFACE_BYTES {
        return Err(LayoutError::Size);
    }
    Ok(SurfaceLayout {
        width: want.width,
        height: want.height,
        pitch,
        size,
    })
}

/// `drm_nvidia_gem_import_nvkms_memory_params` (32 bytes): the byte size, the
/// pointer to the NVKMS block (the host replaces it), that block's size.
pub fn gem_import_params(mem_size: u64) -> [u8; GEM_IMPORT_BYTES] {
    let mut a = [0u8; GEM_IMPORT_BYTES];
    put64(&mut a, 0, mem_size);
    put64(&mut a, 16, NVKMS_IMPORT_BYTES as u64);
    a
}

/// `nvkms_kapi_priv_import_memory_params` (28 bytes) of a pitch-linear surface whose
/// export file has backend handle `mem_fd`: `memFd` first (the host turns the handle
/// into its descriptor), `layout = 1` (pitch), all block geometry zero.
pub fn nvkms_import_params(mem_fd: u32) -> [u8; NVKMS_IMPORT_BYTES] {
    let mut a = [0u8; NVKMS_IMPORT_BYTES];
    put32(&mut a, 0, mem_fd);
    put32(&mut a, 4, 1);
    a
}

/// The GEM handle of an answered `DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` (`handle`
/// is at 24 of the top-level block). Zero is not a handle.
pub fn gem_handle(reply: &Reply<'_>) -> Option<u32> {
    let h = get32(reply.data, 24)?;
    (h != 0).then_some(h)
}

/// `struct drm_gem_close { handle, pad }`.
pub fn gem_close_params(handle: u32) -> [u8; 8] {
    let mut a = [0u8; 8];
    put32(&mut a, 0, handle);
    a
}

/// `nv_ioctl_nvos33_parameters_with_fd` (56 bytes): map `length` bytes at `offset` of
/// memory `h_memory` for CPU access, armed on the channel whose backend handle is
/// `fd`. `flags` 0 = read/write, default caching (librmclient's).
pub fn nvos33_with_fd(
    h_client: u32,
    h_device: u32,
    h_memory: u32,
    offset: u64,
    length: u64,
    fd: u32,
) -> [u8; NVOS33_FD_BYTES] {
    let mut a = [0u8; NVOS33_FD_BYTES];
    put32(&mut a, 0, h_client);
    put32(&mut a, 4, h_device);
    put32(&mut a, 8, h_memory);
    put64(&mut a, 16, offset);
    put64(&mut a, 24, length);
    put32(&mut a, 48, fd);
    a
}

/// `pLinearAddress` of an answered `NV_ESC_RM_MAP_MEMORY`: the cookie its unmap
/// names (the host puts the shared-memory offset of the mapping there).
pub fn map_cookie(reply: &Reply<'_>) -> Option<u64> {
    get64(reply.data, 32)
}

/// `NVOS34_PARAMETERS`: undo the CPU mapping whose cookie is `cookie`.
pub fn nvos34(h_client: u32, h_device: u32, h_memory: u32, cookie: u64) -> [u8; NVOS34_BYTES] {
    let mut a = [0u8; NVOS34_BYTES];
    put32(&mut a, 0, h_client);
    put32(&mut a, 4, h_device);
    put32(&mut a, 8, h_memory);
    put64(&mut a, 16, cookie);
    a
}

// ---- the probe picture ------------------------------------------------------------

/// The picture the probe paints, as `crm_scanout_smoke` does: eight colour bars over
/// the top two thirds, a grey ramp below, and a one-pixel red border (a wrong pitch
/// or offset shows at a glance). `0x00RRGGBB`, the top byte zero (`XRGB`).
pub fn pattern_pixel(x: u32, y: u32, w: u32, h: u32) -> u32 {
    const BARS: [u32; 8] = [
        0xff_ffff, 0xff_ff00, 0x00_ffff, 0x00_ff00, 0xff_00ff, 0xff_0000, 0x00_00ff, 0x00_0000,
    ];
    if w == 0 || h == 0 {
        return 0;
    }
    if x == 0 || y == 0 || x + 1 >= w || y + 1 >= h {
        return 0xff_0000;
    }
    if u64::from(y) < u64::from(h) * 2 / 3 {
        return BARS[((u64::from(x) * 8 / u64::from(w)) as usize).min(7)];
    }
    let g = (u64::from(x) * 255 / u64::from(w - 1)) as u32;
    (g << 16) | (g << 8) | g
}

// ---- the state machine -------------------------------------------------------------

/// One I/O step the driver performs for the client. The numbers are the
/// breadcrumbs (`RmStep`, `RmFail`), so a failure shows where it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Step {
    // Bring-up.
    OpenCtl = 1,
    VersionQuery = 2,
    VersionStrict = 3,
    CardInfo = 4,
    AllocRoot = 5,
    OpenGpu = 6,
    RegisterGpuFd = 7,
    AllocDevice = 8,
    AllocSubdevice = 9,
    SysFiles = 10,
    OpenDrm = 11,
    // The surface.
    AllocMemory = 16,
    OpenExportCh = 17,
    ExportToFd = 18,
    GemImport = 19,
    CloseExportCh = 20,
    /// The export file closed as an UNDO: the surface changes while it is still open
    /// (stages `ExportChOpen`..`Imported`), so it goes before the GEM and the memory
    /// do. Unlike [`Step::CloseExportCh`] it keeps the stage, and like every undo it
    /// always advances (a failed close is counted, never retried).
    CloseExportChUndo = 21,
    GemClose = 24,
    FreeMemory = 25,
    // The optional CPU view.
    OpenMapCh = 32,
    RegisterMapFd = 33,
    RmMapMemory = 34,
    HostMmap = 35,
    KernelMap = 36,
    KernelUnmap = 40,
    HostMunmap = 41,
    RmUnmapMemory = 42,
    CloseMapCh = 43,
    // The probe flip.
    FillPattern = 48,
    ScanoutSet = 49,
    ScanoutPresent = 50,
    // The surface as a foreign (Venus) resource, level 4.
    /// `RESOURCE_CREATE_BLOB` of the surface's GEM as `RM_EXPORT`, under the KMD's own
    /// owner: the resid a WDDM allocation adopts and DWM opens.
    ForeignImport = 51,
    /// Release that resource (host unref); an undo, it always advances.
    ForeignRelease = 52,
    // Pure state moves between the slots of a ring (level 3); no I/O.
    /// Put the finished working slot aside; the next surface is built in a fresh one.
    Park = 56,
    /// Take the last parked slot back as the working slot, to tear it down.
    Unpark = 57,
}

/// The bring-up sequence, in order.
const BRING_UP: [Step; 11] = [
    Step::OpenCtl,
    Step::VersionQuery,
    Step::VersionStrict,
    Step::CardInfo,
    Step::AllocRoot,
    Step::OpenGpu,
    Step::RegisterGpuFd,
    Step::AllocDevice,
    Step::AllocSubdevice,
    Step::SysFiles,
    Step::OpenDrm,
];

/// How a step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FailKind {
    /// The KMD's own forwarding policy or quota refused it (`Refusal`).
    Refused = 1,
    /// The transport failed, timed out or is gone.
    Transport = 2,
    /// The host answered with a negative errno.
    Host = 3,
    /// RM answered with a nonzero `NV_STATUS`.
    Rm = 4,
    /// A reply the client could not read.
    Parse = 5,
    /// RM's answer cannot hold the picture.
    Layout = 6,
    /// The guest OS refused (a kernel mapping, an allocation).
    Os = 7,
    /// Another owner holds scanout 0 (probe only, retried).
    Busy = 8,
}

/// A failed step: what kind, and the status or errno that came with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fail {
    pub kind: FailKind,
    pub code: u32,
}

impl Fail {
    pub const fn new(kind: FailKind, code: u32) -> Self {
        Self { kind, code }
    }
}

impl From<RmError> for Fail {
    fn from(e: RmError) -> Self {
        match e {
            RmError::Malformed => Fail::new(FailKind::Parse, 0),
            RmError::Host(errno) => Fail::new(FailKind::Host, errno),
            RmError::Status(s) => Fail::new(FailKind::Rm, s),
        }
    }
}

/// A recorded failure: which step, how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub step: Step,
    pub fail: Fail,
}

impl Failure {
    /// `step << 24 | kind << 16 | code & 0xffff`: one registry word that says where
    /// the client stopped and why.
    pub fn pack(&self) -> u32 {
        ((self.step as u32) << 24) | ((self.fail.kind as u32) << 16) | (self.fail.code & 0xffff)
    }
}

/// What a finished step produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    Unit,
    /// A backend handle (`Open`), or the root client's RM handle for `AllocRoot`.
    Handle(u32),
    /// The card `CARD_INFO` named, and the string the strict check will use is kept
    /// by the driver (it is not state of the machine).
    Card(CardInfo),
    /// The DRI node index to open.
    Dri(u32),
    /// The surface as RM made it.
    Mem(SurfaceLayout),
    /// The GEM handle on the DRM file.
    Gem(u32),
    /// The mapping cookie RM returned.
    Cookie(u64),
    /// The host's mapping id (0 for RM's own mappings) and the offset of the mapping
    /// inside the shared-memory region.
    HostMapped(u32, u64),
    /// The version string `QUERY` returned (all zero: none).
    Version([u8; VERSION_STR_BYTES]),
    /// The kernel view: address and length.
    Mapped(u64, u64),
    /// The foreign resource id the surface was imported as.
    Resource(u32),
}

/// Where the machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing opened (or everything forgotten).
    Cold,
    /// Bring-up is in progress or done; `up` counts finished steps.
    Up,
    /// Failed: unavailable for this transport generation.
    Dead(Failure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SurfStage {
    Allocated,
    ExportChOpen,
    Exported,
    Imported,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Surface {
    layout: SurfaceLayout,
    memory: u32,
    stage: SurfStage,
    gem: u32,
    /// The foreign resource id the surface was imported as (level 4), 0 = none.
    foreign: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ViewStage {
    None,
    ChanOpen,
    FdRegistered,
    RmMapped,
    HostMapped,
    KernelMapped,
}

/// The probe flip: one picture, shown once per surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Not started (or waiting for the view).
    Idle,
    /// The picture is in the surface.
    Filled,
    /// `ScanoutSet` took scanout 0.
    Set,
    /// Flipped: the source runs out its lapse by itself.
    Shown,
    /// Given up (a failure, or another owner held scanout 0 too often).
    Skipped,
}

/// How many times `ScanoutSet` may find scanout 0 busy before the probe is dropped.
pub const PROBE_MAX_BUSY: u8 = 3;

/// What the caller wants of the client right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want {
    /// 0 off, 1 client and surface, 2 also the CPU view and the probe flip, 3 a ring
    /// of [`RING_SLOTS`] surfaces, each with its CPU view, and no probe (the
    /// presenter, `rm_present`, drives scanout), 4 also each surface imported as a
    /// foreign resource (`docs/kmd-rm-client.md` section 14).
    pub level: u8,
    /// The extent of the VidPn primary, once one is bound.
    pub surface: Option<(u32, u32)>,
}

/// Surfaces in the level 3 ring. The host flip has no completion, so a surface
/// that was flipped may still be read by the viewer for a frame or two: content is
/// always written to the one that was NOT shown last.
pub const RING_SLOTS: usize = 2;
/// Slots that can be parked beside the working one.
pub const MAX_PARKED: usize = RING_SLOTS - 1;

impl Want {
    /// The surfaces wanted: one, or the whole ring at level 3.
    pub fn slots(&self) -> usize {
        if self.level >= 3 {
            RING_SLOTS
        } else {
            1
        }
    }

    /// Whether the CPU view of a surface is wanted.
    fn views(&self) -> bool {
        self.level >= 2
    }

    /// Whether each surface is also imported as a foreign resource (level 4).
    fn shares(&self) -> bool {
        self.level >= 4
    }

    /// Whether the one-picture probe flip runs (levels 2 only: level 3 flips real frames).
    fn probes(&self) -> bool {
        self.level == 2
    }
}

/// The next thing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do right now.
    Idle,
    Step(Step),
    /// The client is unavailable (use Venus).
    Dead,
}

/// Everything that must be closed on the host: backend handles the client opened.
/// Closing the control file frees every RM client made on it, and closing the DRM
/// file drops its GEM handles, so closing these is the whole teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cleanup {
    pub handles: [u32; 3 + 2 * (1 + MAX_PARKED)],
    pub count: usize,
}

impl Cleanup {
    pub fn as_slice(&self) -> &[u32] {
        self.handles.get(..self.count).unwrap_or(&[])
    }
}

/// One surface of the ring and everything that belongs to it: its export file, its
/// CPU view and its probe. The client works on one slot at a time (`Client::cur`);
/// finished ones are parked.
#[derive(Debug, Clone, Copy)]
struct Slot {
    export_ch: u32,
    map_ch: u32,
    surface: Option<Surface>,
    view: ViewStage,
    view_cookie: u64,
    view_host_id: u32,
    view_off: u64,
    view_va: u64,
    view_len: u64,
    view_failed: bool,
    /// The foreign import failed: given up for this surface (no retry), the surface
    /// itself is fine.
    share_failed: bool,
    probe: Probe,
    probe_busy: u8,
}

impl Slot {
    const EMPTY: Slot = Slot {
        export_ch: 0,
        map_ch: 0,
        surface: None,
        view: ViewStage::None,
        view_cookie: 0,
        view_host_id: 0,
        view_off: 0,
        view_va: 0,
        view_len: 0,
        view_failed: false,
        share_failed: false,
        probe: Probe::Idle,
        probe_busy: 0,
    };

    /// Nothing of it is allocated or mapped.
    fn is_empty(&self) -> bool {
        self.surface.is_none() && self.view == ViewStage::None && self.export_ch == 0
    }

    /// The kernel view, if mapped.
    fn view_now(&self) -> Option<(u64, u64)> {
        (self.view == ViewStage::KernelMapped).then_some((self.view_va, self.view_len))
    }

    /// Take the kernel view away, for unmapping (see [`Client::take_view`]).
    fn take_view(&mut self) -> Option<(u64, u64)> {
        let v = self.view_now();
        if v.is_some() {
            self.view = ViewStage::HostMapped;
            self.view_va = 0;
            self.view_len = 0;
        }
        v
    }

    /// The surface is finished and, when `views`, its CPU view is up and, when
    /// `shares`, the foreign import is settled (done, or given up).
    fn is_complete(&self, views: bool, shares: bool) -> bool {
        match self.surface {
            Some(s) if s.stage == SurfStage::Ready => {
                (!views || (self.view == ViewStage::KernelMapped && !self.view_failed))
                    && (!shares || s.foreign != 0 || self.share_failed)
            }
            _ => false,
        }
    }
}

/// What the presenter needs to know about one slot of the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotInfo {
    pub layout: SurfaceLayout,
    /// The GEM handle on the DRM file.
    pub gem: u32,
    /// The kernel view: address and length.
    pub view: (u64, u64),
    /// The foreign resource id the surface was imported as (level 4), 0 = none.
    pub foreign: u32,
}

/// Every kernel view the client holds, for unmapping all of them at once (a retire,
/// a new transport generation, a death).
#[derive(Debug, Clone, Copy, Default)]
pub struct Views {
    pub items: [(u64, u64); 1 + MAX_PARKED],
    pub count: usize,
}

impl Views {
    pub fn as_slice(&self) -> &[(u64, u64)] {
        self.items.get(..self.count).unwrap_or(&[])
    }
}

/// The KMD RM client's state.
#[derive(Debug, Clone, Copy)]
pub struct Client {
    phase: Phase,
    /// The transport generation the handles belong to (0 = none yet).
    epoch: u64,
    /// Bring-up steps finished.
    up: u8,
    ctl: u32,
    gpu: u32,
    drm: u32,
    root: u32,
    gpu_id: u32,
    minor: u32,
    dri_index: u32,
    version: [u8; VERSION_STR_BYTES],
    /// The slot every step works on.
    cur: Slot,
    /// Finished slots set aside by [`Step::Park`], oldest first.
    parked: [Slot; MAX_PARKED],
    parked_n: u8,
    /// Surfaces made so far (handle uniqueness).
    surfaces_made: u32,
    /// Dead and not yet cleaned up.
    cleanup_owed: bool,
    /// Unwinding steps that failed (counted, never retried).
    soft_errors: u32,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Cold,
            epoch: 0,
            up: 0,
            ctl: 0,
            gpu: 0,
            drm: 0,
            root: 0,
            gpu_id: 0,
            minor: 0,
            dri_index: 0,
            version: [0u8; VERSION_STR_BYTES],
            cur: Slot::EMPTY,
            parked: [Slot::EMPTY; MAX_PARKED],
            parked_n: 0,
            surfaces_made: 0,
            cleanup_owed: false,
            soft_errors: 0,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The failure that killed the client, if it is dead.
    pub fn failure(&self) -> Option<Failure> {
        match self.phase {
            Phase::Dead(f) => Some(f),
            _ => None,
        }
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.phase, Phase::Dead(_))
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Bring-up steps finished (0 ..= 11).
    pub fn up(&self) -> u8 {
        self.up
    }

    pub fn bring_up_done(&self) -> bool {
        usize::from(self.up) >= BRING_UP.len()
    }

    // The ids the I/O layer needs (zero until known).
    pub fn ctl(&self) -> u32 {
        self.ctl
    }
    pub fn gpu(&self) -> u32 {
        self.gpu
    }
    pub fn drm(&self) -> u32 {
        self.drm
    }
    pub fn root(&self) -> u32 {
        self.root
    }
    pub fn minor(&self) -> u32 {
        self.minor
    }
    pub fn gpu_id(&self) -> u32 {
        self.gpu_id
    }
    pub fn dri_index(&self) -> u32 {
        self.dri_index
    }
    pub fn export_ch(&self) -> u32 {
        self.cur.export_ch
    }
    pub fn map_ch(&self) -> u32 {
        self.cur.map_ch
    }
    pub fn soft_errors(&self) -> u32 {
        self.soft_errors
    }
    pub fn probe(&self) -> Probe {
        self.cur.probe
    }

    /// The surface being built or ready: its layout and RM memory handle.
    pub fn surface(&self) -> Option<(SurfaceLayout, u32)> {
        self.cur.surface.map(|s| (s.layout, s.memory))
    }

    /// The finished surface: layout, memory handle and GEM handle.
    pub fn ready_surface(&self) -> Option<(SurfaceLayout, u32, u32)> {
        match self.cur.surface {
            Some(s) if s.stage == SurfStage::Ready => Some((s.layout, s.memory, s.gem)),
            _ => None,
        }
    }

    /// The GEM handle of the surface as soon as it exists (before the export file
    /// is closed): for diagnostics.
    pub fn gem(&self) -> u32 {
        self.cur.surface.map_or(0, |s| s.gem)
    }

    /// The foreign resource id of the working slot's surface (0 = none).
    pub fn foreign(&self) -> u32 {
        self.cur.surface.map_or(0, |s| s.foreign)
    }

    /// The cookie of the CPU mapping RM holds, for its unmap.
    pub fn view_cookie(&self) -> u64 {
        self.cur.view_cookie
    }

    /// The host's mapping id and the mapping's offset in the shared-memory region.
    pub fn view_host(&self) -> (u32, u64) {
        (self.cur.view_host_id, self.cur.view_off)
    }

    /// The version string `QUERY` returned (all zero when RM gave none).
    pub fn version(&self) -> [u8; VERSION_STR_BYTES] {
        self.version
    }

    /// The RM handle the next surface will be allocated under.
    pub fn next_memory_handle(&self) -> u32 {
        memory_handle(self.surfaces_made.wrapping_add(1))
    }

    /// The kernel view of the working slot's surface, if mapped.
    pub fn view(&self) -> Option<(u64, u64)> {
        self.cur.view_now()
    }

    /// Slots that hold a surface: the parked ones and the working one.
    pub fn slot_count(&self) -> usize {
        usize::from(self.parked_n) + usize::from(self.cur.surface.is_some())
    }

    /// Slots set aside by [`Step::Park`].
    pub fn parked(&self) -> usize {
        usize::from(self.parked_n)
    }

    /// Slot `i` of the ring (parked ones first, the working one last), when it is
    /// finished and its CPU view is up.
    pub fn slot(&self, i: usize) -> Option<SlotInfo> {
        let n = usize::from(self.parked_n);
        let slot = match i.cmp(&n) {
            core::cmp::Ordering::Less => self.parked.get(i)?,
            core::cmp::Ordering::Equal => &self.cur,
            core::cmp::Ordering::Greater => return None,
        };
        let s = slot.surface.filter(|s| s.stage == SurfStage::Ready)?;
        let view = slot.view_now()?;
        Some(SlotInfo {
            layout: s.layout,
            gem: s.gem,
            view,
            foreign: s.foreign,
        })
    }

    /// The ring is complete for `want` and the client is healthy: every wanted slot
    /// is finished and mapped. This is what the presenter waits for.
    pub fn presentable(&self, want: Want) -> bool {
        if self.is_dead() || !self.bring_up_done() || !want.views() {
            return false;
        }
        let Some((w, h)) = want.surface else {
            return false;
        };
        let n = want.slots();
        if self.slot_count() != n {
            return false;
        }
        // Every slot has the primary's extent (a mode change tears them all down
        // before this is true again).
        (0..n).all(|i| {
            self.slot(i)
                .is_some_and(|s| s.layout.width == w && s.layout.height == h)
        })
    }

    /// One word for the registry: phase, bring-up progress, surface and view stage,
    /// probe.
    pub fn status_word(&self) -> u32 {
        let phase = match self.phase {
            Phase::Cold => 0u32,
            Phase::Up => 1,
            Phase::Dead(_) => 2,
        };
        let surf = self.cur.surface.map_or(0u32, |s| 1 + s.stage as u32);
        let probe = self.cur.probe as u32;
        (phase << 28)
            | (u32::from(self.parked_n) << 24)
            | (u32::from(self.up) << 20)
            | (u32::from(self.foreign() != 0) << 16)
            | (u32::from(
                self.parked
                    .iter()
                    .take(usize::from(self.parked_n))
                    .any(|p| p.surface.is_some_and(|s| s.foreign != 0)),
            ) << 17)
            | (surf << 12)
            | ((self.cur.view as u32) << 8)
            | probe
    }

    /// Reconcile with the transport generation `now` (`nvrm_epoch`, 0 = no
    /// transport). A different generation means every handle of this client is gone:
    /// start over, cold. Returns whether anything was dropped; the caller must have
    /// unmapped the kernel view first (see [`Self::take_view`]).
    pub fn sync_epoch(&mut self, now: u64) -> bool {
        if now == self.epoch {
            return false;
        }
        let had = self.epoch != 0 || !matches!(self.phase, Phase::Cold);
        let keep_made = self.surfaces_made;
        *self = Self::new();
        self.surfaces_made = keep_made;
        self.epoch = now;
        had
    }

    /// Forget everything (the transport is gone or being retired; the host side was
    /// closed by the sweep). Keeps nothing but the surface counter.
    pub fn forget(&mut self) {
        let keep_made = self.surfaces_made;
        *self = Self::new();
        self.surfaces_made = keep_made;
    }

    /// Take the working slot's kernel view away from the machine, for unmapping.
    /// After this the machine no longer believes in a view.
    pub fn take_view(&mut self) -> Option<(u64, u64)> {
        self.cur.take_view()
    }

    /// [`Self::take_view`] for every slot, parked ones included.
    pub fn take_views(&mut self) -> Views {
        let mut out = Views::default();
        let mut put = |v: Option<(u64, u64)>| {
            if let Some(v) = v {
                if let Some(slot) = out.items.get_mut(out.count) {
                    *slot = v;
                    out.count += 1;
                }
            }
        };
        put(self.cur.take_view());
        for p in self.parked.iter_mut() {
            put(p.take_view());
        }
        out
    }

    /// Record that bring-up begins in generation `epoch` (the first step of a cold
    /// client). A client that is already in that generation is left alone.
    pub fn begin(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.sync_epoch(epoch);
        }
        if matches!(self.phase, Phase::Cold) {
            self.phase = Phase::Up;
        }
    }

    /// The next step for `want`.
    pub fn next(&self, want: Want) -> Action {
        if self.is_dead() {
            return Action::Dead;
        }
        if want.level == 0 {
            return Action::Idle;
        }
        // Nothing is opened until a primary exists to give the surface its size.
        if matches!(self.phase, Phase::Cold) && want.surface.is_none() {
            return Action::Idle;
        }
        if let Some(step) = BRING_UP.get(usize::from(self.up)) {
            return Action::Step(*step);
        }

        // The view goes before the surface it maps.
        if let Some(step) = self.view_unwind_step(want) {
            return Action::Step(step);
        }
        if let Some(s) = self.cur.surface {
            let wanted = want.surface;
            // An export file that was closed as an undo (the extent changed mid-surface)
            // leaves the stage where it was: the stages that need that file cannot go
            // on, whatever the extent is now, so the surface goes.
            let export_lost = self.cur.export_ch == 0
                && matches!(s.stage, SurfStage::ExportChOpen | SurfStage::Exported);
            if export_lost
                || (wanted.is_some() && wanted != Some((s.layout.width, s.layout.height)))
            {
                // The foreign resource goes before the GEM and the memory it names.
                if s.foreign != 0 {
                    return Action::Step(Step::ForeignRelease);
                }
                // A different extent: tear the surface down; the next call makes
                // the new one. An export file still open (stages `ExportChOpen` ..
                // `Imported`) goes first: nothing else would close it, and the next
                // `OpenExportCh` would overwrite its handle.
                if self.cur.export_ch != 0 {
                    return Action::Step(Step::CloseExportChUndo);
                }
                return Action::Step(if s.gem != 0 && s.stage >= SurfStage::Imported {
                    Step::GemClose
                } else {
                    Step::FreeMemory
                });
            }
            return match s.stage {
                SurfStage::Allocated => Action::Step(Step::OpenExportCh),
                SurfStage::ExportChOpen => Action::Step(Step::ExportToFd),
                SurfStage::Exported => Action::Step(Step::GemImport),
                SurfStage::Imported => Action::Step(Step::CloseExportCh),
                SurfStage::Ready => match self.share_step(s, want) {
                    Some(step) => Action::Step(step),
                    None => match self.view_and_probe_step(want) {
                        // Finished, and the ring wants more: set it aside and build the
                        // next one. Only a slot that is complete (surface AND, when the
                        // view is wanted, its view) is kept: a given-up view ends the
                        // ring here, and the presenter never starts.
                        Action::Idle
                            if self.cur.is_complete(want.views(), want.shares())
                                && usize::from(self.parked_n) + 1 < want.slots() =>
                        {
                            Action::Step(Step::Park)
                        }
                        other => other,
                    },
                },
            };
        }
        // The working slot is empty. A parked slot of another extent must go before
        // anything new is made: bring it back to tear it down with the same steps.
        if self.parked_n > 0 && self.parked_is_stale(want) {
            return Action::Step(Step::Unpark);
        }
        match want.surface {
            Some(_) if usize::from(self.parked_n) < want.slots() => Action::Step(Step::AllocMemory),
            _ => Action::Idle,
        }
    }

    /// The foreign-resource step of a finished surface: import it when level 4 wants it
    /// shared (once; a failure is not retried), release it when it is no longer wanted.
    fn share_step(&self, s: Surface, want: Want) -> Option<Step> {
        if s.foreign != 0 && !want.shares() {
            return Some(Step::ForeignRelease);
        }
        (want.shares() && s.foreign == 0 && !self.cur.share_failed).then_some(Step::ForeignImport)
    }

    /// Whether a parked slot has an extent other than the one now wanted.
    fn parked_is_stale(&self, want: Want) -> bool {
        let Some(w) = want.surface else {
            return false;
        };
        self.parked
            .iter()
            .take(usize::from(self.parked_n))
            .any(|p| {
                p.surface
                    .is_some_and(|s| (s.layout.width, s.layout.height) != w)
            })
    }

    /// The step that undoes the next stage of the view, when it must go.
    fn view_unwind_step(&self, want: Want) -> Option<Step> {
        if self.cur.view == ViewStage::None {
            return None;
        }
        let surface_changing = match (self.cur.surface, want.surface) {
            (Some(s), Some(w)) => w != (s.layout.width, s.layout.height),
            (None, _) => true,
            _ => false,
        };
        if !(self.cur.view_failed || surface_changing || !want.views()) {
            return None;
        }
        Some(match self.cur.view {
            ViewStage::KernelMapped => Step::KernelUnmap,
            ViewStage::HostMapped => Step::HostMunmap,
            ViewStage::RmMapped => Step::RmUnmapMemory,
            ViewStage::FdRegistered | ViewStage::ChanOpen => Step::CloseMapCh,
            ViewStage::None => return None,
        })
    }

    /// With the surface ready: the CPU view (level 2) and the probe that needs it.
    fn view_and_probe_step(&self, want: Want) -> Action {
        if !want.views() || self.cur.view_failed {
            return Action::Idle;
        }
        match self.cur.view {
            ViewStage::None => return Action::Step(Step::OpenMapCh),
            ViewStage::ChanOpen => return Action::Step(Step::RegisterMapFd),
            ViewStage::FdRegistered => return Action::Step(Step::RmMapMemory),
            ViewStage::RmMapped => return Action::Step(Step::HostMmap),
            ViewStage::HostMapped => return Action::Step(Step::KernelMap),
            ViewStage::KernelMapped => {}
        }
        if !want.probes() {
            return Action::Idle;
        }
        match self.cur.probe {
            Probe::Idle => Action::Step(Step::FillPattern),
            Probe::Filled => Action::Step(Step::ScanoutSet),
            Probe::Set => Action::Step(Step::ScanoutPresent),
            Probe::Shown | Probe::Skipped => Action::Idle,
        }
    }

    fn die(&mut self, step: Step, fail: Fail) {
        self.phase = Phase::Dead(Failure { step, fail });
        self.cleanup_owed = true;
    }

    /// Report a finished step. A failure of bring-up or of the surface path kills the
    /// client; one of the CPU view or the probe only gives those up.
    pub fn finish(&mut self, step: Step, result: Result<Out, Fail>) {
        use Step::*;
        match step {
            ForeignImport => match result {
                Ok(Out::Resource(r)) if r != 0 => {
                    if let Some(s) = self.cur.surface.as_mut() {
                        s.foreign = r;
                    }
                }
                // Given up for this surface; the surface is untouched.
                _ => self.cur.share_failed = true,
            },
            ForeignRelease => {
                // An undo always advances: a failed release is counted, never retried.
                if result.is_err() {
                    self.soft_errors = self.soft_errors.saturating_add(1);
                }
                if let Some(s) = self.cur.surface.as_mut() {
                    s.foreign = 0;
                }
            }
            // Pure state moves: no I/O behind them, so no result to judge.
            Park => {
                let n = usize::from(self.parked_n);
                if self.cur.is_empty() || n >= MAX_PARKED {
                    // Not a thing the machine asks for; refuse it loudly.
                    return self.die(step, Fail::new(FailKind::Parse, 0xfe));
                }
                if let Some(slot) = self.parked.get_mut(n) {
                    *slot = self.cur;
                    self.cur = Slot::EMPTY;
                    self.parked_n += 1;
                }
            }
            Unpark => {
                let n = usize::from(self.parked_n);
                if !self.cur.is_empty() || n == 0 {
                    return self.die(step, Fail::new(FailKind::Parse, 0xfd));
                }
                if let Some(slot) = self.parked.get_mut(n - 1) {
                    self.cur = *slot;
                    *slot = Slot::EMPTY;
                    self.parked_n -= 1;
                }
            }
            KernelUnmap | HostMunmap | RmUnmapMemory | CloseMapCh | CloseExportChUndo => {
                // An undo always advances: a failed one is counted, never retried.
                if result.is_err() {
                    self.soft_errors = self.soft_errors.saturating_add(1);
                }
                match step {
                    KernelUnmap => {
                        self.cur.view = ViewStage::HostMapped;
                        self.cur.view_va = 0;
                        self.cur.view_len = 0;
                    }
                    HostMunmap => {
                        self.cur.view = ViewStage::RmMapped;
                        self.cur.view_host_id = 0;
                        self.cur.view_off = 0;
                    }
                    RmUnmapMemory => {
                        self.cur.view = ViewStage::FdRegistered;
                        self.cur.view_cookie = 0;
                    }
                    // The surface keeps its stage: the GEM and the memory are next.
                    CloseExportChUndo => self.cur.export_ch = 0,
                    _ => {
                        self.cur.view = ViewStage::None;
                        self.cur.map_ch = 0;
                        self.cur.view_cookie = 0;
                        // The view is gone: either it failed (and stays given up) or
                        // the surface changes (and the next one starts clean).
                        if !self.cur.view_failed {
                            self.cur.probe = Probe::Idle;
                            self.cur.probe_busy = 0;
                        }
                    }
                }
            }
            OpenMapCh | RegisterMapFd | RmMapMemory | HostMmap | KernelMap | FillPattern
            | ScanoutSet | ScanoutPresent => self.finish_optional(step, result),
            _ => self.finish_required(step, result),
        }
    }

    fn finish_optional(&mut self, step: Step, result: Result<Out, Fail>) {
        use Step::*;
        match (step, result) {
            (ScanoutSet, Err(f)) if f.kind == FailKind::Busy => {
                self.cur.probe_busy = self.cur.probe_busy.saturating_add(1);
                if self.cur.probe_busy >= PROBE_MAX_BUSY {
                    self.cur.probe = Probe::Skipped;
                }
            }
            (FillPattern | ScanoutSet | ScanoutPresent, Err(_)) => {
                self.cur.probe = Probe::Skipped;
            }
            (_, Err(_)) => {
                // The view is given up; what is half made is undone by the unwind.
                self.cur.view_failed = true;
            }
            (OpenMapCh, Ok(Out::Handle(h))) => {
                self.cur.map_ch = h;
                self.cur.view = ViewStage::ChanOpen;
            }
            (RegisterMapFd, Ok(_)) => self.cur.view = ViewStage::FdRegistered,
            (RmMapMemory, Ok(Out::Cookie(c))) => {
                self.cur.view_cookie = c;
                self.cur.view = ViewStage::RmMapped;
            }
            (HostMmap, Ok(Out::HostMapped(id, off))) => {
                self.cur.view_host_id = id;
                self.cur.view_off = off;
                self.cur.view = ViewStage::HostMapped;
            }
            (KernelMap, Ok(Out::Mapped(va, len))) => {
                self.cur.view_va = va;
                self.cur.view_len = len;
                self.cur.view = ViewStage::KernelMapped;
            }
            (FillPattern, Ok(_)) => self.cur.probe = Probe::Filled,
            (ScanoutSet, Ok(_)) => self.cur.probe = Probe::Set,
            (ScanoutPresent, Ok(_)) => self.cur.probe = Probe::Shown,
            // A success that did not carry what the step must produce is a
            // malformed result of the driver's own: give the view up.
            (_, Ok(_)) => self.cur.view_failed = true,
        }
    }

    fn finish_required(&mut self, step: Step, result: Result<Out, Fail>) {
        use Step::*;
        let out = match result {
            Ok(o) => o,
            Err(f) => return self.die(step, f),
        };
        if matches!(self.phase, Phase::Cold) {
            self.phase = Phase::Up;
        }
        let bad = |s: &mut Self| s.die(step, Fail::new(FailKind::Parse, 0xff));
        match (step, out) {
            (OpenCtl, Out::Handle(h)) => {
                self.ctl = h;
                self.up = 1;
            }
            (VersionQuery, Out::Version(v)) => {
                self.version = v;
                self.up = 2;
            }
            (VersionStrict, _) => self.up = 3,
            (CardInfo, Out::Card(c)) => {
                self.gpu_id = c.gpu_id;
                self.minor = c.minor;
                self.up = 4;
            }
            (AllocRoot, Out::Handle(h)) => {
                self.root = h;
                self.up = 5;
            }
            (OpenGpu, Out::Handle(h)) => {
                self.gpu = h;
                self.up = 6;
            }
            (RegisterGpuFd, _) => self.up = 7,
            (AllocDevice, _) => self.up = 8,
            (AllocSubdevice, _) => self.up = 9,
            (SysFiles, Out::Dri(i)) => {
                self.dri_index = i;
                self.up = 10;
            }
            (OpenDrm, Out::Handle(h)) => {
                self.drm = h;
                self.up = 11;
            }
            (AllocMemory, Out::Mem(layout)) => {
                self.surfaces_made = self.surfaces_made.wrapping_add(1);
                self.cur.surface = Some(Surface {
                    layout,
                    memory: memory_handle(self.surfaces_made),
                    stage: SurfStage::Allocated,
                    gem: 0,
                    foreign: 0,
                });
                // A fresh surface shows its own picture once.
                self.cur.probe = Probe::Idle;
                self.cur.probe_busy = 0;
            }
            (OpenExportCh, Out::Handle(h)) => {
                self.cur.export_ch = h;
                self.set_stage(SurfStage::ExportChOpen);
            }
            (ExportToFd, _) => self.set_stage(SurfStage::Exported),
            (GemImport, Out::Gem(g)) => {
                if let Some(s) = self.cur.surface.as_mut() {
                    s.gem = g;
                }
                self.set_stage(SurfStage::Imported);
            }
            (CloseExportCh, _) => {
                self.cur.export_ch = 0;
                self.set_stage(SurfStage::Ready);
            }
            (GemClose, _) => {
                if let Some(s) = self.cur.surface.as_mut() {
                    s.gem = 0;
                    // The memory is next; `Imported` is the last stage that owns a GEM.
                    s.stage = SurfStage::Allocated;
                }
            }
            (FreeMemory, _) => self.cur.surface = None,
            _ => bad(self),
        }
    }

    fn set_stage(&mut self, stage: SurfStage) {
        if let Some(s) = self.cur.surface.as_mut() {
            s.stage = stage;
        }
    }

    /// Whether a dead client's handles still have to be closed.
    pub fn cleanup_owed(&self) -> bool {
        self.cleanup_owed
    }

    /// The backend handles to close after a failure, once. Closing the control file
    /// frees every RM client made on it and closing the DRM file drops its GEM
    /// handles, so this is the whole teardown. The caller must have unmapped the
    /// kernel view (`take_view`) first.
    pub fn take_cleanup(&mut self) -> Cleanup {
        let mut c = Cleanup::default();
        if !self.cleanup_owed {
            return c;
        }
        self.cleanup_owed = false;
        // Most dependent first: the files that hold mappings and exports, then the
        // GPU channel, last the control file.
        let mut put = |h: u32| {
            if h != 0 {
                if let Some(slot) = c.handles.get_mut(c.count) {
                    *slot = h;
                    c.count += 1;
                }
            }
        };
        for p in self.parked.iter().take(usize::from(self.parked_n)) {
            put(p.map_ch);
            put(p.export_ch);
        }
        put(self.cur.map_ch);
        put(self.cur.export_ch);
        put(self.drm);
        put(self.gpu);
        put(self.ctl);
        self.drm = 0;
        self.gpu = 0;
        self.ctl = 0;
        // The caller took the views first (`take_views`); nothing of any slot is
        // believed in after a death.
        self.cur = Slot::EMPTY;
        self.parked = [Slot::EMPTY; MAX_PARKED];
        self.parked_n = 0;
        c
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    // ---- layout pins: the offsets `offsetof` gives on nv_ioctl_defs.h -----------

    #[test]
    fn nv_escape_numbers_are_the_linux_ones() {
        // _IOWR('F', 0x2b, 48): what the Linux module forwards for RM_ALLOC.
        assert_eq!(nv_cmd(ESC_RM_ALLOC, 48), 0xC030_462B);
        assert_eq!(nv_cmd(ESC_RM_CONTROL, 32), 0xC020_462A);
        assert_eq!(nv_cmd(ESC_RM_FREE, 16), 0xC010_4629);
        assert_eq!(nv_cmd(ESC_CARD_INFO, 2304), 0xC900_46C8);
        assert_eq!(nv_cmd(ESC_REGISTER_FD, 4), 0xC004_46C9);
        assert_eq!(nv_cmd(ESC_CHECK_VERSION_STR, 72), 0xC048_46D2);
        // The host matches RM_FREE on the low 16 bits (`after_ioctl`).
        assert_eq!(nv_cmd(ESC_RM_FREE, 16) & 0xffff, 0x4629);
        assert_eq!(DRM_IOCTL_GEM_IMPORT_NVKMS, 0xC020_6441);
        assert_eq!(DRM_IOCTL_GEM_CLOSE, 0x4008_6409);
    }

    #[test]
    fn nvos_blocks_have_the_c_offsets() {
        let a = nvos64(1, 2, 3, 4, 5);
        assert_eq!(a.len(), 48);
        assert_eq!(get32(&a, 0), Some(1)); // hRoot
        assert_eq!(get32(&a, 4), Some(2)); // hObjectParent
        assert_eq!(get32(&a, 8), Some(3)); // hObjectNew
        assert_eq!(get32(&a, 12), Some(4)); // hClass
        assert_eq!(get64(&a, 16), Some(0)); // pAllocParms
        assert_eq!(get32(&a, 32), Some(5)); // paramsSize
        assert_eq!(get32(&a, NVOS64_STATUS_AT), Some(0));

        let f = nvos00(7, 8, 9);
        assert_eq!(
            (get32(&f, 0), get32(&f, 4), get32(&f, 8)),
            (Some(7), Some(8), Some(9))
        );
        assert_eq!(NVOS00_STATUS_AT, 12);

        let c = nvos54(1, 2, 0x3d05, 24);
        assert_eq!(c.len(), 32);
        assert_eq!(get32(&c, 8), Some(0x3d05));
        assert_eq!(get32(&c, 24), Some(24));
        assert_eq!(NVOS54_STATUS_AT, 28);

        let m = nvos33_with_fd(1, 2, 3, 0x1000, 0x2000, 77);
        assert_eq!(m.len(), 56);
        assert_eq!(get32(&m, 8), Some(3));
        assert_eq!(get64(&m, 16), Some(0x1000));
        assert_eq!(get64(&m, 24), Some(0x2000));
        assert_eq!(get32(&m, 48), Some(77)); // fd
        assert_eq!(get32(&m, 44), Some(0)); // flags
        assert_eq!(NVOS33_STATUS_AT, 40);

        let u = nvos34(1, 2, 3, 0xabc);
        assert_eq!(u.len(), 32);
        assert_eq!(get64(&u, 16), Some(0xabc));
        assert_eq!(NVOS34_STATUS_AT, 24);
    }

    #[test]
    fn export_and_import_blocks_have_the_c_offsets() {
        let e = export_params(0x11, 0x22, 5);
        assert_eq!(e.len(), 24);
        assert_eq!(get32(&e, 0), Some(1)); // type RM
        assert_eq!(get32(&e, 4), Some(0x11)); // hDevice
        assert_eq!(get32(&e, 8), Some(0x11)); // hParent = the device
        assert_eq!(get32(&e, 12), Some(0x22)); // hObject
        assert_eq!(get32(&e, 16), Some(5)); // fd: the host reads it at nested 16
        assert_eq!(get32(&e, 20), Some(0)); // flags

        let g = gem_import_params(0x80_0000);
        assert_eq!(g.len(), 32);
        assert_eq!(get64(&g, 0), Some(0x80_0000)); // mem_size
        assert_eq!(get64(&g, 8), Some(0)); // pointer: the host's
        assert_eq!(get64(&g, 16), Some(28)); // nvkms_params_size
        assert_eq!(get32(&g, 24), Some(0)); // handle (out)
        let n = nvkms_import_params(9);
        assert_eq!(n.len(), 28);
        assert_eq!(get32(&n, 0), Some(9)); // memFd first
        assert_eq!(get32(&n, 4), Some(1)); // pitch layout
        assert!(n[8..].iter().all(|&b| b == 0));
    }

    #[test]
    fn memory_params_have_the_c_offsets_and_the_smoke_attributes() {
        let s = surface_layout(1920, 1080).unwrap();
        let p = mem_alloc_params(0xc1d0_0001, &s);
        assert_eq!(p.len(), 128);
        assert_eq!(get32(&p, 0), Some(0xc1d0_0001)); // owner
        assert_eq!(get32(&p, 4), Some(0)); // type IMAGE
        assert_eq!(get32(&p, 8), Some(0x100)); // ALIGNMENT_FORCE
        assert_eq!(get32(&p, 12), Some(1920));
        assert_eq!(get32(&p, 16), Some(1080));
        assert_eq!(get32(&p, 20), Some(7680)); // pitch
        assert_eq!(get32(&p, 24), Some(0x1900_0000)); // attr
        assert_eq!(get32(&p, 28), Some(6)); // attr2
        assert_eq!(get64(&p, 64), Some(s.size));
        assert_eq!(get64(&p, 72), Some(64 * 1024)); // alignment
    }

    #[test]
    fn version_block_round_trips() {
        let mut v = [0u8; 64];
        v[..9].copy_from_slice(b"610.57.04");
        let q = version_params(VERSION_CMD_QUERY, &v);
        assert_eq!(get32(&q, 0), Some(b'2' as u32));
        assert_eq!(&q[8..17], b"610.57.04");
        // A reply: cmd, reply = recognised, the string, junk after the NUL.
        let mut data = [0u8; 72];
        data[4] = 1;
        data[8..17].copy_from_slice(b"610.57.04");
        data[30] = b'x';
        let got = parse_version_reply(&data).unwrap();
        assert_eq!(&got[..9], b"610.57.04");
        assert!(
            got[9..].iter().all(|&b| b == 0),
            "junk after the NUL is dropped"
        );
        assert_eq!(parse_version_reply(&data[..40]), None);
        assert_eq!(
            parse_version_reply(&[0u8; 72]),
            None,
            "an empty string is no version"
        );
    }

    // ---- messages -----------------------------------------------------------------

    #[test]
    fn open_close_and_sys_files_messages() {
        let mut b = [0xAAu8; 64];
        assert_eq!(build_open(&mut b, 255), Some(24));
        assert_eq!(get32(&b, 0), Some(MSG_OPEN));
        assert_eq!(get32(&b, 4), Some(0));
        assert_eq!(get32(&b, 8), Some(0), "status and padding are zeroed");
        assert_eq!(get32(&b, 12), Some(0));
        assert_eq!(get32(&b, 16), Some(255));
        assert_eq!(get32(&b, 20), Some(2));
        assert_eq!(build_close(&mut b, 0x42), Some(16));
        assert_eq!((get32(&b, 0), get32(&b, 4)), (Some(MSG_CLOSE), Some(0x42)));
        assert_eq!(build_get_sys_files(&mut b), Some(16));
        assert_eq!(get32(&b, 0), Some(MSG_GET_SYS_FILES));
        assert_eq!(build_open(&mut [0u8; 23], 255), None);
    }

    #[test]
    fn ioctl_message_layout_matches_the_wire_builder() {
        // crm_wire_ioctl: header, cmd, data_len, nested_offset = data_len, nested_len,
        // deep 0/0, data, nested.
        let data = [1u8, 2, 3, 4];
        let nested = [9u8, 8];
        let mut b = [0u8; 128];
        let n = build_ioctl(&mut b, 0x31, 0xC030_462B, &data, &nested).unwrap();
        assert_eq!(n, 16 + 24 + 4 + 2);
        assert_eq!(get32(&b, 0), Some(MSG_IOCTL));
        assert_eq!(get32(&b, 4), Some(0x31));
        assert_eq!(get32(&b, 16), Some(0xC030_462B));
        assert_eq!(get32(&b, 20), Some(4));
        assert_eq!(get32(&b, 24), Some(4)); // nested_offset
        assert_eq!(get32(&b, 28), Some(2)); // nested_len
        assert_eq!((get32(&b, 32), get32(&b, 36)), (Some(0), Some(0)));
        assert_eq!(&b[40..44], &data);
        assert_eq!(&b[44..46], &nested);
        // No nested block: the offset is 0, as librmclient writes it.
        let n = build_ioctl(&mut b, 1, 2, &data, &[]).unwrap();
        assert_eq!(n, 16 + 24 + 4);
        assert_eq!(get32(&b, 24), Some(0));
        // A buffer too small for it is refused, never truncated.
        assert_eq!(build_ioctl(&mut [0u8; 43], 1, 2, &data, &[]), None);
    }

    fn reply(status: i32, data: &[u8], nested: &[u8]) -> ([u8; 512], usize) {
        let mut r = [0u8; 512];
        r[8..12].copy_from_slice(&status.to_le_bytes());
        r[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        r[20..24].copy_from_slice(&(nested.len() as u32).to_le_bytes());
        r[28..28 + data.len()].copy_from_slice(data);
        r[28 + data.len()..28 + data.len() + nested.len()].copy_from_slice(nested);
        (r, 28 + data.len() + nested.len())
    }

    #[test]
    fn replies_parse_and_every_short_reply_is_refused() {
        let mut data = [0u8; 48];
        data[8..12].copy_from_slice(&0xc1d0_0007u32.to_le_bytes());
        let (r, n) = reply(0, &data, &[5, 6, 7]);
        let rep = parse_ioctl_reply(&r[..n]).unwrap();
        assert_eq!(rep.data.len(), 48);
        assert_eq!(rep.nested, &[5, 6, 7]);
        assert_eq!(alloc_new_handle(&rep), Some(0xc1d0_0007));
        assert_eq!(rm_ok(&rep, NVOS64_STATUS_AT), Ok(()));
        // Every truncation is an error, none panics.
        for cut in 0..n {
            assert!(parse_ioctl_reply(&r[..cut]).is_err(), "cut {cut}");
        }
        // The host's refusal: a bare header with a negative errno.
        let mut hdr = [0u8; 16];
        hdr[8..12].copy_from_slice(&(-22i32).to_le_bytes());
        assert_eq!(parse_ioctl_reply(&hdr), Err(ReplyError::Host(-22)));
        assert_eq!(rm_reply(&hdr, 0), Err(RmError::Host(22)));
        // RM's own status in the block.
        data[40..44].copy_from_slice(&0x22u32.to_le_bytes());
        let (r, n) = reply(0, &data, &[]);
        assert_eq!(
            rm_reply(&r[..n], NVOS64_STATUS_AT),
            Err(RmError::Status(0x22))
        );
        // A status word the block does not reach.
        let (r, n) = reply(0, &data[..30], &[]);
        assert_eq!(rm_reply(&r[..n], NVOS64_STATUS_AT), Err(RmError::Malformed));
        // A block that claims more than the reply holds.
        let (mut r, n) = reply(0, &data, &[]);
        r[16..20].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse_ioctl_reply(&r[..n]), Err(ReplyError::Short));
    }

    #[test]
    fn open_reply_needs_a_small_nonzero_handle_and_status_zero() {
        let mut r = [0u8; 16];
        r[4..8].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(parse_open_reply(&r), Some(7));
        r[8..12].copy_from_slice(&(-19i32).to_le_bytes());
        assert_eq!(parse_open_reply(&r), None, "host errno");
        r[8..12].fill(0);
        r[4..8].fill(0);
        assert_eq!(parse_open_reply(&r), None, "handle 0");
        r[4..8].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        assert_eq!(parse_open_reply(&r), None, "not a positive int");
        assert_eq!(parse_open_reply(&r[..8]), None);
    }

    // ---- discovery ------------------------------------------------------------------

    fn card(valid: u8, gpu_id: u32, minor: u32) -> [u8; 72] {
        let mut c = [0u8; 72];
        c[0] = valid;
        c[16..20].copy_from_slice(&gpu_id.to_le_bytes());
        c[56..60].copy_from_slice(&minor.to_le_bytes());
        c
    }

    #[test]
    fn card_info_picks_the_first_valid_card() {
        let mut all = [0u8; CARD_INFO_BYTES];
        all[72..144].copy_from_slice(&card(1, 0x100, 1));
        all[216..288].copy_from_slice(&card(1, 0x200, 3));
        assert_eq!(
            parse_card_info(&all),
            Some(CardInfo {
                gpu_id: 0x100,
                minor: 1
            })
        );
        assert_eq!(parse_card_info(&[0u8; CARD_INFO_BYTES]), None);
        // A minor no `Open` could take is not a card.
        let mut bad = [0u8; 72];
        bad.copy_from_slice(&card(1, 5, 300));
        assert_eq!(parse_card_info(&bad), None);
        assert_eq!(parse_card_info(&[]), None);
    }

    fn sys_files(files: &[(&str, &str)], dri: &[(&str, u32, u32)]) -> ([u8; 1024], usize) {
        let mut s = [0u8; 1024];
        let mut at = 0usize;
        let w = |s: &mut [u8; 1024], at: &mut usize, v: u32| {
            s[*at..*at + 4].copy_from_slice(&v.to_le_bytes());
            *at += 4;
        };
        for (p, c) in files {
            w(&mut s, &mut at, p.len() as u32);
            w(&mut s, &mut at, c.len() as u32);
            s[at..at + p.len()].copy_from_slice(p.as_bytes());
            at += p.len();
            s[at..at + c.len()].copy_from_slice(c.as_bytes());
            at += c.len();
        }
        w(&mut s, &mut at, 0);
        w(&mut s, &mut at, 0);
        w(&mut s, &mut at, dri.len() as u32);
        for (name, slot, gpu_id) in dri {
            w(&mut s, &mut at, name.len() as u32);
            w(&mut s, &mut at, 226); // major
            w(&mut s, &mut at, 128); // minor
            w(&mut s, &mut at, *slot);
            w(&mut s, &mut at, *gpu_id); // dev_info[0]
            for _ in 1..9 {
                w(&mut s, &mut at, 0);
            }
            s[at..at + name.len()].copy_from_slice(name.as_bytes());
            at += name.len();
        }
        (s, at)
    }

    #[test]
    fn the_dri_section_is_found_behind_the_files() {
        let (s, n) = sys_files(
            &[("/proc/driver/nvidia/version", "NVRM 610"), ("/x", "")],
            &[("renderD128", 0, 0x100), ("renderD129", 1, 0x200)],
        );
        let mut nodes = [DriNode::default(); MAX_DRI];
        let k = parse_dri_section(&s[..n], &mut nodes);
        assert_eq!(k, 2);
        assert_eq!(
            nodes[1],
            DriNode {
                gpu_id: 0x200,
                slot: 1
            }
        );
        assert_eq!(pick_dri(&nodes[..k], 0x200), Some(1));
        assert_eq!(pick_dri(&nodes[..k], 0x100), Some(0));
        assert_eq!(pick_dri(&nodes[..k], 0x999), Some(0), "no match: node 0");
        assert_eq!(pick_dri(&[], 0x100), None);
        // No files, no nodes.
        let (s, n) = sys_files(&[], &[]);
        assert_eq!(parse_dri_section(&s[..n], &mut nodes), 0);
        // Every truncation parses to at most what was whole, and never panics.
        let (s, n) = sys_files(&[("/a", "b")], &[("renderD128", 0, 7)]);
        for cut in 0..n {
            let _ = parse_dri_section(&s[..cut], &mut nodes);
        }
        assert_eq!(parse_dri_section(&s[..n], &mut nodes), 1);
        assert_eq!(
            parse_dri_section(&s[..n - 1], &mut nodes),
            0,
            "a cut name is not a node"
        );
    }

    #[test]
    fn dri_records_beyond_the_table_are_counted_out_not_overrun() {
        let many: [(&str, u32, u32); 10] = [("r", 0, 1); 10];
        let (s, n) = sys_files(&[], &many);
        let mut nodes = [DriNode::default(); MAX_DRI];
        assert_eq!(parse_dri_section(&s[..n], &mut nodes), MAX_DRI);
    }

    // ---- the surface ------------------------------------------------------------------

    #[test]
    fn surface_layout_aligns_pitch_and_size() {
        let s = surface_layout(1920, 1080).unwrap();
        assert_eq!(s.pitch, 7680, "1920 * 4 is already 256-aligned");
        assert_eq!(s.size, (7680u64 * 1080 + 0xffff) & !0xffff);
        assert_eq!(s.size % SIZE_ALIGN, 0);
        // The 1896-wide mode the KMD has met: 7584 -> 7680 (the shear that bit it).
        let s = surface_layout(1896, 1030).unwrap();
        assert_eq!(s.pitch, 7680);
        assert!(s.size >= 7680 * 1030);
        // Out of range.
        assert_eq!(surface_layout(63, 1080), None);
        assert_eq!(surface_layout(1920, 16385), None);
        assert_eq!(surface_layout(16384, 16384), None, "1 GiB is over the cap");
        assert!(surface_layout(7680, 4320).is_some(), "8K fits");
    }

    #[test]
    fn rm_may_round_the_pitch_and_size_up_but_not_below_the_picture() {
        let want = surface_layout(1920, 1080).unwrap();
        let mut nested = mem_alloc_params(1, &want);
        assert_eq!(adopt_alloc_reply(&want, &nested), Ok(want));
        // RM pads the pitch and size.
        nested[20..24].copy_from_slice(&8192u32.to_le_bytes());
        nested[64..72].copy_from_slice(&((8192u64 * 1080 + 0xffff) & !0xffff).to_le_bytes());
        let got = adopt_alloc_reply(&want, &nested).unwrap();
        assert_eq!(got.pitch, 8192);
        assert_eq!((got.width, got.height), (1920, 1080));
        // A pitch that cannot hold a row.
        nested[20..24].copy_from_slice(&4096u32.to_le_bytes());
        assert_eq!(adopt_alloc_reply(&want, &nested), Err(LayoutError::Pitch));
        // A size that cannot hold the rows.
        nested[20..24].copy_from_slice(&8192u32.to_le_bytes());
        nested[64..72].copy_from_slice(&0x1000u64.to_le_bytes());
        assert_eq!(adopt_alloc_reply(&want, &nested), Err(LayoutError::Size));
        // A reply with no block.
        assert_eq!(
            adopt_alloc_reply(&want, &nested[..40]),
            Err(LayoutError::Short)
        );
        // Zero fields mean "unchanged".
        let zero = [0u8; 128];
        assert_eq!(adopt_alloc_reply(&want, &zero), Ok(want));
    }

    #[test]
    fn the_flip_layout_is_xrgb_linear_at_the_surface_pitch() {
        let s = surface_layout(1920, 1080).unwrap();
        let l = flip_layout(&s);
        assert_eq!(l.fourcc, 0x3432_5258);
        assert_eq!((l.stride, l.offset, l.modifier), (7680, 0, 0));
        assert_eq!(
            l.validate(),
            Ok(()),
            "the foreign scanout accepts it as it is"
        );
    }

    #[test]
    fn gem_handle_and_cookie_come_from_the_right_offsets() {
        let mut data = [0u8; 32];
        data[24..28].copy_from_slice(&5u32.to_le_bytes());
        let (r, n) = reply(0, &data, &[0u8; 28]);
        let rep = parse_ioctl_reply(&r[..n]).unwrap();
        assert_eq!(gem_handle(&rep), Some(5));
        data[24..28].fill(0);
        let (r, n) = reply(0, &data, &[]);
        assert_eq!(gem_handle(&parse_ioctl_reply(&r[..n]).unwrap()), None);
        let mut map = [0u8; 56];
        map[32..40].copy_from_slice(&0x7000u64.to_le_bytes());
        let (r, n) = reply(0, &map, &[]);
        assert_eq!(
            map_cookie(&parse_ioctl_reply(&r[..n]).unwrap()),
            Some(0x7000)
        );
    }

    #[test]
    fn the_probe_picture_has_a_red_border_bars_and_a_ramp() {
        let (w, h) = (1920, 1080);
        assert_eq!(pattern_pixel(0, 500, w, h), 0xff_0000);
        assert_eq!(pattern_pixel(w - 1, 500, w, h), 0xff_0000);
        assert_eq!(pattern_pixel(500, 0, w, h), 0xff_0000);
        assert_eq!(pattern_pixel(500, h - 1, w, h), 0xff_0000);
        assert_eq!(pattern_pixel(1, 1, w, h), 0xff_ffff, "first bar is white");
        assert_eq!(
            pattern_pixel(w - 2, 10, w, h),
            0x00_0000,
            "last bar is black"
        );
        let g = pattern_pixel(960, h - 5, w, h);
        assert_eq!(g & 0xff, (g >> 8) & 0xff);
        assert_eq!(g >> 16, g & 0xff, "the ramp is grey");
        assert_eq!(pattern_pixel(5, 5, 0, 0), 0);
        // XRGB: the top byte is never set.
        for y in (0..h).step_by(97) {
            for x in (0..w).step_by(89) {
                assert_eq!(pattern_pixel(x, y, w, h) >> 24, 0);
            }
        }
    }

    // ---- the state machine ----------------------------------------------------------

    const WANT1: Want = Want {
        level: 1,
        surface: Some((1920, 1080)),
    };
    const WANT2: Want = Want {
        level: 2,
        surface: Some((1920, 1080)),
    };

    /// What a healthy host would produce for `step`.
    fn ok_out(step: Step, c: &Client) -> Out {
        match step {
            Step::OpenCtl => Out::Handle(10),
            Step::OpenGpu => Out::Handle(11),
            Step::OpenDrm => Out::Handle(12),
            Step::AllocRoot => Out::Handle(0xc1d0_0001),
            Step::CardInfo => Out::Card(CardInfo {
                gpu_id: 0x100,
                minor: 0,
            }),
            Step::SysFiles => Out::Dri(0),
            Step::AllocMemory => Out::Mem(surface_layout(1920, 1080).unwrap()),
            Step::OpenExportCh => Out::Handle(20),
            Step::GemImport => Out::Gem(1),
            Step::OpenMapCh => Out::Handle(30),
            Step::RmMapMemory => Out::Cookie(0x5000),
            Step::HostMmap => Out::HostMapped(0, 0x20_0000),
            Step::VersionQuery => Out::Version([b'6'; VERSION_STR_BYTES]),
            Step::KernelMap => Out::Mapped(0xffff_8000_0000_0000, c.surface().unwrap().0.size),
            _ => Out::Unit,
        }
    }

    /// Run the client to quiescence against a healthy host; returns the steps taken.
    fn run(c: &mut Client, want: Want, steps: &mut [Option<Step>; 64]) -> usize {
        let mut n = 0;
        loop {
            if c.epoch() == 0 {
                c.begin(1);
            } else {
                c.begin(c.epoch());
            }
            match c.next(want) {
                Action::Step(s) => {
                    assert!(n < 64, "runaway: {:?}", &steps[..n]);
                    steps[n] = Some(s);
                    n += 1;
                    let out = ok_out(s, c);
                    c.finish(s, Ok(out));
                }
                Action::Idle | Action::Dead => return n,
            }
        }
    }

    fn taken(steps: &[Option<Step>; 64], n: usize) -> Vec<Step> {
        steps[..n].iter().map(|s| s.unwrap()).collect()
    }

    #[test]
    fn level_zero_and_no_primary_do_nothing() {
        let c = Client::new();
        assert_eq!(
            c.next(Want {
                level: 0,
                surface: Some((1920, 1080))
            }),
            Action::Idle
        );
        assert_eq!(
            c.next(Want {
                level: 1,
                surface: None
            }),
            Action::Idle
        );
        assert_eq!(
            c.next(Want {
                level: 2,
                surface: None
            }),
            Action::Idle
        );
    }

    #[test]
    fn level_one_brings_the_client_up_and_makes_one_surface() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        let n = run(&mut c, WANT1, &mut buf);
        assert_eq!(
            taken(&buf, n),
            [
                OpenCtl,
                VersionQuery,
                VersionStrict,
                CardInfo,
                AllocRoot,
                OpenGpu,
                RegisterGpuFd,
                AllocDevice,
                AllocSubdevice,
                SysFiles,
                OpenDrm,
                AllocMemory,
                OpenExportCh,
                ExportToFd,
                GemImport,
                CloseExportCh
            ]
        );
        assert!(c.bring_up_done());
        assert_eq!(c.root(), 0xc1d0_0001);
        assert_eq!((c.ctl(), c.gpu(), c.drm()), (10, 11, 12));
        let (layout, mem, gem) = c.ready_surface().unwrap();
        assert_eq!((layout.width, layout.height), (1920, 1080));
        assert_eq!(mem, memory_handle(1));
        assert_eq!(gem, 1);
        assert_eq!(c.export_ch(), 0, "the export file is closed");
        // Quiescent: asking again does nothing, and level 1 never maps.
        let n = run(&mut c, WANT1, &mut buf);
        assert_eq!(n, 0);
        assert_eq!(c.view(), None);
        assert_eq!(c.probe(), Probe::Idle);
    }

    #[test]
    fn level_two_adds_the_view_the_picture_and_one_flip() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        let n = run(&mut c, WANT2, &mut buf);
        let steps = taken(&buf, n);
        assert_eq!(
            &steps[16..],
            [
                OpenMapCh,
                RegisterMapFd,
                RmMapMemory,
                HostMmap,
                KernelMap,
                FillPattern,
                ScanoutSet,
                ScanoutPresent
            ]
        );
        assert_eq!(c.probe(), Probe::Shown);
        assert_eq!(c.view().map(|v| v.0), Some(0xffff_8000_0000_0000));
        assert_eq!(c.map_ch(), 30);
        assert_eq!(c.view_cookie(), 0x5000);
        // Shown once: nothing more happens for this surface.
        assert_eq!(run(&mut c, WANT2, &mut buf), 0);
    }

    #[test]
    fn every_bring_up_and_surface_failure_kills_the_client_and_closes_what_it_opened() {
        // Inject a failure at each step of the level-1 sequence in turn.
        let mut probe = Client::new();
        let mut buf = [None; 64];
        let n = run(&mut probe, WANT1, &mut buf);
        let sequence = taken(&buf, n);
        for (i, &failing) in sequence.iter().enumerate() {
            let mut c = Client::new();
            c.begin(1);
            let mut opened_before: Vec<u32> = Vec::new();
            for &s in &sequence[..i] {
                let out = ok_out(s, &c);
                if let (
                    Step::OpenCtl | Step::OpenGpu | Step::OpenDrm | Step::OpenExportCh,
                    Out::Handle(h),
                ) = (s, out)
                {
                    opened_before.push(h);
                }
                assert_eq!(c.next(WANT1), Action::Step(s));
                c.finish(s, Ok(out));
            }
            assert_eq!(c.next(WANT1), Action::Step(failing));
            c.finish(failing, Err(Fail::new(FailKind::Rm, 0x1f)));
            assert!(c.is_dead(), "step {failing:?} must fail closed");
            assert_eq!(c.next(WANT1), Action::Dead);
            assert_eq!(c.next(WANT2), Action::Dead);
            let f = c.failure().unwrap();
            assert_eq!(f.step, failing);
            assert_eq!(f.fail, Fail::new(FailKind::Rm, 0x1f));
            assert_eq!(f.pack(), ((failing as u32) << 24) | (4 << 16) | 0x1f);
            // Everything opened so far is closed, exactly once, and nothing else.
            assert!(c.cleanup_owed());
            let cl = c.take_cleanup();
            let mut got: Vec<u32> = cl.as_slice().to_vec();
            got.sort_unstable();
            opened_before.sort_unstable();
            assert_eq!(got, opened_before, "failing at {failing:?}");
            assert!(!c.cleanup_owed());
            assert_eq!(c.take_cleanup().count, 0, "a second cleanup closes nothing");
            assert!(c.ready_surface().is_none());
        }
    }

    #[test]
    fn a_dead_client_stays_dead_until_the_transport_changes() {
        let mut c = Client::new();
        c.begin(7);
        c.finish(Step::OpenCtl, Err(Fail::new(FailKind::Transport, 1)));
        assert!(c.is_dead());
        // The same generation: no retry.
        c.begin(7);
        assert_eq!(c.next(WANT1), Action::Dead);
        // A new generation starts cold and tries again.
        assert!(c.sync_epoch(8));
        assert_eq!(c.phase(), Phase::Cold);
        c.begin(8);
        assert_eq!(c.next(WANT1), Action::Step(Step::OpenCtl));
        // The same generation again is not a change.
        assert!(!c.sync_epoch(8));
    }

    #[test]
    fn a_new_transport_generation_drops_every_handle() {
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        assert!(c.ready_surface().is_some());
        assert!(c.sync_epoch(2));
        assert_eq!((c.ctl(), c.gpu(), c.drm(), c.root()), (0, 0, 0, 0));
        assert!(c.ready_surface().is_none());
        assert_eq!(c.up(), 0);
        // Surface handles keep counting across generations.
        c.begin(2);
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        assert_eq!(c.ready_surface().unwrap().1, memory_handle(2));
    }

    #[test]
    fn forget_resets_to_cold_but_keeps_counting_surfaces() {
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT2, &mut buf);
        let view = c.take_view();
        assert!(view.is_some());
        assert_eq!(c.view(), None, "the machine no longer believes in the view");
        c.forget();
        assert_eq!(c.phase(), Phase::Cold);
        assert_eq!(c.epoch(), 0);
        assert_eq!(c.take_view(), None);
    }

    #[test]
    fn a_new_extent_tears_the_surface_down_and_makes_another() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        let other = Want {
            level: 1,
            surface: Some((1280, 720)),
        };
        let mut buf = [None; 64];
        let mut n = 0;
        // Healthy host again; the new surface comes out at the new size.
        loop {
            match c.next(other) {
                Action::Step(s) => {
                    buf[n] = Some(s);
                    n += 1;
                    let out = match s {
                        AllocMemory => Out::Mem(surface_layout(1280, 720).unwrap()),
                        _ => ok_out(s, &c),
                    };
                    c.finish(s, Ok(out));
                }
                _ => break,
            }
        }
        assert_eq!(
            taken(&buf, n),
            [
                GemClose,
                FreeMemory,
                AllocMemory,
                OpenExportCh,
                ExportToFd,
                GemImport,
                CloseExportCh
            ]
        );
        let (layout, mem, _) = c.ready_surface().unwrap();
        assert_eq!((layout.width, layout.height), (1280, 720));
        assert_eq!(mem, memory_handle(2));
    }

    /// A client brought up on 1920x1080 with the surface path run for `stages` steps
    /// (1 = AllocMemory, 2 = OpenExportCh, 3 = ExportToFd, 4 = GemImport).
    fn client_at(stages: usize) -> Client {
        let mut c = Client::new();
        for _ in 0..11 + stages {
            c.begin(1);
            let Action::Step(s) = c.next(WANT1) else {
                panic!("the client stopped early")
            };
            let out = ok_out(s, &c);
            c.finish(s, Ok(out));
        }
        c
    }

    const OTHER_EXTENT: Want = Want {
        level: 1,
        surface: Some((1280, 720)),
    };

    /// `client_at(stages)`, then the extent changes: the steps taken until idle.
    fn change_extent_after(stages: usize) -> (Client, Vec<Step>) {
        let mut c = client_at(stages);
        let mut log = Vec::new();
        while let Action::Step(s) = c.next(OTHER_EXTENT) {
            assert!(log.len() < 16, "runaway: {log:?}");
            log.push(s);
            let out = match s {
                Step::AllocMemory => Out::Mem(surface_layout(1280, 720).unwrap()),
                _ => ok_out(s, &c),
            };
            // The step that frees the surface must not find an export file open.
            if matches!(s, Step::FreeMemory | Step::GemClose) {
                assert_eq!(c.export_ch(), 0, "export file still open at {s:?}");
            }
            c.finish(s, Ok(out));
        }
        (c, log)
    }

    #[test]
    fn a_new_extent_mid_surface_closes_the_export_file_first() {
        use Step::*;
        // The log the review found: AllocMemory, OpenExportCh, then the extent changes.
        // Before the undo step, FreeMemory ran with `export_ch` set and the next
        // `OpenExportCh` overwrote it.
        let (c, log) = change_extent_after(2);
        assert_eq!(
            log,
            [
                CloseExportChUndo,
                FreeMemory,
                AllocMemory,
                OpenExportCh,
                ExportToFd,
                GemImport,
                CloseExportCh
            ]
        );
        assert_eq!(c.export_ch(), 0);
        assert_eq!(c.soft_errors(), 0);
        let (layout, _, _) = c.ready_surface().unwrap();
        assert_eq!((layout.width, layout.height), (1280, 720));

        // After ExportToFd (export file open, nothing imported): the same.
        let (c, log) = change_extent_after(3);
        assert_eq!(&log[..2], [CloseExportChUndo, FreeMemory]);
        assert_eq!(c.export_ch(), 0);

        // After GemImport (the export file is closed by the next normal step, which the
        // extent change pre-empts): export file, then GEM, then the memory.
        let (c, log) = change_extent_after(4);
        assert_eq!(&log[..3], [CloseExportChUndo, GemClose, FreeMemory]);
        assert_eq!(c.export_ch(), 0);
        assert!(c.ready_surface().is_some());
    }

    #[test]
    fn a_new_extent_with_no_export_file_open_needs_no_undo_step() {
        use Step::*;
        let (_, log) = change_extent_after(1);
        assert_eq!(&log[..2], [FreeMemory, AllocMemory]);
        assert!(!log[..2].contains(&CloseExportChUndo));
    }

    #[test]
    fn a_failed_export_file_undo_still_advances() {
        use Step::*;
        let mut c = client_at(2);
        assert_eq!(c.export_ch(), 20);
        assert_eq!(c.next(OTHER_EXTENT), Action::Step(CloseExportChUndo));
        c.finish(CloseExportChUndo, Err(Fail::new(FailKind::Host, 0xC1)));
        assert!(!c.is_dead());
        assert_eq!(c.soft_errors(), 1);
        assert_eq!(c.export_ch(), 0);
        assert_eq!(c.next(OTHER_EXTENT), Action::Step(FreeMemory));
    }

    #[test]
    fn a_failed_version_query_means_no_string() {
        // A reply the host refused (errno in the header), a bare header claiming
        // success, a truncated one, and an empty string: none has a string.
        let mut refused = [0u8; MSG_HDR];
        put32(&mut refused, 8, (-22i32) as u32);
        assert_eq!(version_from_query_reply(&refused), [0u8; VERSION_STR_BYTES]);
        assert_eq!(version_from_query_reply(&[]), [0u8; VERSION_STR_BYTES]);
        assert_eq!(
            version_from_query_reply(&[0u8; MSG_HDR]),
            [0u8; VERSION_STR_BYTES]
        );
        // And a good one still yields it.
        let mut r = std::vec![0u8; REPLY_DATA + VERSION_BYTES];
        put32(&mut r, MSG_HDR, VERSION_BYTES as u32);
        r[REPLY_DATA + 8..REPLY_DATA + 8 + 6].copy_from_slice(b"595.58");
        let v = version_from_query_reply(&r);
        assert_eq!(&v[..6], b"595.58");
        assert_eq!(v[6], 0);
    }

    #[test]
    fn a_free_that_fails_kills_the_client() {
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        let other = Want {
            level: 1,
            surface: Some((1280, 720)),
        };
        assert_eq!(c.next(other), Action::Step(Step::GemClose));
        c.finish(Step::GemClose, Ok(Out::Unit));
        assert_eq!(c.next(other), Action::Step(Step::FreeMemory));
        c.finish(Step::FreeMemory, Err(Fail::new(FailKind::Rm, 0x57)));
        assert!(c.is_dead());
        assert_eq!(c.take_cleanup().count, 3, "ctl, gpu and drm are closed");
    }

    #[test]
    fn a_failed_view_is_given_up_and_unwound_without_killing_the_client() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        for s in [OpenMapCh, RegisterMapFd, RmMapMemory] {
            assert_eq!(c.next(WANT2), Action::Step(s));
            let out = ok_out(s, &c);
            c.finish(s, Ok(out));
        }
        // The host refuses the shared-memory mapping.
        assert_eq!(c.next(WANT2), Action::Step(HostMmap));
        c.finish(HostMmap, Err(Fail::new(FailKind::Host, 12)));
        assert!(!c.is_dead());
        // The half-made view is undone, in reverse, and then nothing more is tried.
        assert_eq!(c.next(WANT2), Action::Step(RmUnmapMemory));
        c.finish(RmUnmapMemory, Ok(Out::Unit));
        assert_eq!(c.next(WANT2), Action::Step(CloseMapCh));
        c.finish(CloseMapCh, Ok(Out::Unit));
        assert_eq!(c.next(WANT2), Action::Idle);
        assert_eq!(c.map_ch(), 0);
        assert_eq!(c.probe(), Probe::Idle);
        // The surface is still there for a flip.
        assert!(c.ready_surface().is_some());
        assert_eq!(c.failure(), None);
    }

    #[test]
    fn an_unwind_step_that_fails_still_advances() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT2, &mut buf);
        // The surface changes: the view must go first, whatever its undo answers.
        let other = Want {
            level: 2,
            surface: Some((1280, 720)),
        };
        for s in [KernelUnmap, HostMunmap, RmUnmapMemory, CloseMapCh] {
            assert_eq!(c.next(other), Action::Step(s), "unwind order");
            c.finish(s, Err(Fail::new(FailKind::Transport, 9)));
        }
        assert_eq!(c.soft_errors(), 4);
        assert!(!c.is_dead());
        assert_eq!(c.next(other), Action::Step(GemClose));
    }

    #[test]
    fn dropping_to_level_one_unwinds_the_view() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT2, &mut buf);
        assert_eq!(c.next(WANT1), Action::Step(KernelUnmap));
    }

    #[test]
    fn a_probe_that_finds_scanout_busy_is_retried_a_few_times() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        for s in [
            OpenMapCh,
            RegisterMapFd,
            RmMapMemory,
            HostMmap,
            KernelMap,
            FillPattern,
        ] {
            let out = ok_out(s, &c);
            c.finish(s, Ok(out));
        }
        for tries in 1..=PROBE_MAX_BUSY {
            assert_eq!(c.next(WANT2), Action::Step(ScanoutSet));
            c.finish(ScanoutSet, Err(Fail::new(FailKind::Busy, 0)));
            assert!(!c.is_dead());
            if tries < PROBE_MAX_BUSY {
                assert_eq!(c.probe(), Probe::Filled);
            }
        }
        assert_eq!(c.probe(), Probe::Skipped);
        assert_eq!(c.next(WANT2), Action::Idle);
        assert!(
            c.view().is_some(),
            "the view stays; only the flip is given up"
        );
    }

    #[test]
    fn a_failed_flip_skips_the_probe_but_not_the_client() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT2, &mut buf);
        assert_eq!(c.probe(), Probe::Shown);
        // A new surface gets its own probe.
        let other = Want {
            level: 2,
            surface: Some((1280, 720)),
        };
        let mut n = 0;
        loop {
            match c.next(other) {
                Action::Step(s) => {
                    n += 1;
                    assert!(n < 64);
                    let out = match s {
                        AllocMemory => Out::Mem(surface_layout(1280, 720).unwrap()),
                        KernelMap => Out::Mapped(0x1000, 1 << 20),
                        _ => ok_out(s, &c),
                    };
                    if s == ScanoutPresent {
                        c.finish(s, Err(Fail::new(FailKind::Transport, 3)));
                    } else {
                        c.finish(s, Ok(out));
                    }
                }
                _ => break,
            }
        }
        assert_eq!(c.probe(), Probe::Skipped);
        assert!(!c.is_dead());
    }

    #[test]
    fn a_result_that_does_not_carry_what_the_step_makes_is_a_failure() {
        // The driver's own bug must fail closed, not leave a half-known client.
        let mut c = Client::new();
        c.begin(1);
        c.finish(Step::OpenCtl, Ok(Out::Unit));
        assert!(c.is_dead());
        assert_eq!(c.failure().unwrap().fail.kind, FailKind::Parse);
    }

    #[test]
    fn the_first_finished_step_marks_the_client_up() {
        let mut c = Client::new();
        assert_eq!(c.phase(), Phase::Cold);
        c.finish(Step::OpenCtl, Ok(Out::Handle(5)));
        assert_eq!(c.phase(), Phase::Up);
        assert_eq!((c.ctl(), c.up()), (5, 1));
    }

    #[test]
    fn the_version_string_and_the_view_mapping_ids_are_kept_for_later_steps() {
        use Step::*;
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT2, &mut buf);
        assert_eq!(c.version(), [b'6'; VERSION_STR_BYTES]);
        assert_eq!(c.view_host(), (0, 0x20_0000));
        // Undoing the view clears what described it, step by step.
        assert_eq!(
            c.next(Want { level: 1, ..WANT2 }),
            Action::Step(KernelUnmap)
        );
        c.finish(KernelUnmap, Ok(Out::Unit));
        assert_eq!(c.view(), None);
        c.finish(HostMunmap, Ok(Out::Unit));
        assert_eq!(c.view_host(), (0, 0));
        c.finish(RmUnmapMemory, Ok(Out::Unit));
        assert_eq!(c.view_cookie(), 0);
        c.finish(CloseMapCh, Ok(Out::Unit));
        assert_eq!(c.map_ch(), 0);
        // A new surface may be probed again; a failed view stays given up.
        assert_eq!(c.probe(), Probe::Idle);
    }

    #[test]
    fn the_next_surface_handle_is_what_the_allocation_will_get() {
        let mut c = Client::new();
        let h = c.next_memory_handle();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        assert_eq!(c.ready_surface().unwrap().1, h);
        assert_ne!(c.next_memory_handle(), h);
    }

    #[test]
    fn the_status_words_the_doc_quotes_are_what_the_machine_produces() {
        // `docs/kmd-rm-client.md` section 3 gives these as the healthy readings.
        let mut c = Client::new();
        let mut buf = [None; 64];
        assert_eq!(run(&mut c, WANT1, &mut buf), 16);
        assert_eq!(c.status_word(), 0x10b0_5000);
        let mut c = Client::new();
        assert_eq!(run(&mut c, WANT2, &mut buf), 24);
        assert_eq!(c.status_word(), 0x10b0_5503);
    }

    #[test]
    fn status_word_tracks_progress() {
        let mut c = Client::new();
        assert_eq!(c.status_word(), 0);
        let mut buf = [None; 64];
        run(&mut c, WANT2, &mut buf);
        let w = c.status_word();
        assert_eq!(w >> 28, 1, "up");
        assert_eq!((w >> 20) & 0xff, 11, "bring-up finished");
        assert_eq!(w & 0xff, Probe::Shown as u32);
        c.finish(Step::KernelUnmap, Ok(Out::Unit));
        assert_ne!(c.status_word(), w);
    }

    #[test]
    fn memory_handles_are_distinct_per_surface_and_clear_of_librmclients() {
        assert_ne!(memory_handle(1), memory_handle(2));
        for n in 0..300 {
            let h = memory_handle(n);
            assert!(h != H_DEVICE && h != H_SUBDEVICE);
            assert!(h & 0xff00_0000 != 0x5c00_0000, "librmclient's range");
        }
        assert_eq!(H_DEVICE & 0xff00_0000, 0x4b00_0000);
    }

    // ---- level 3: the ring ----------------------------------------------------------

    const WANT3: Want = Want {
        level: 3,
        surface: Some((1920, 1080)),
    };

    /// `ok_out` with a distinct file per surface (the real host never repeats one).
    fn ok_out3(step: Step, c: &Client, files: &mut u32) -> Out {
        match step {
            Step::OpenExportCh | Step::OpenMapCh => {
                *files += 1;
                Out::Handle(100 + *files)
            }
            Step::GemImport => {
                *files += 1;
                Out::Gem(200 + *files)
            }
            Step::KernelMap => {
                *files += 1;
                Out::Mapped(
                    0xffff_8000_0000_0000 + u64::from(*files) * 0x1000_0000,
                    c.surface().unwrap().0.size,
                )
            }
            _ => ok_out(step, c),
        }
    }

    /// Run `want` to quiescence against a healthy host, with unique files.
    fn run3(c: &mut Client, want: Want, files: &mut u32) -> Vec<Step> {
        let mut log = Vec::new();
        loop {
            c.begin(if c.epoch() == 0 { 1 } else { c.epoch() });
            match c.next(want) {
                Action::Step(s) => {
                    assert!(log.len() < 80, "runaway: {log:?}");
                    log.push(s);
                    let out = match s {
                        Step::AllocMemory => {
                            let (w, h) = want.surface.unwrap();
                            Out::Mem(surface_layout(w, h).unwrap())
                        }
                        _ => ok_out3(s, c, files),
                    };
                    c.finish(s, Ok(out));
                }
                Action::Idle | Action::Dead => return log,
            }
        }
    }

    #[test]
    fn want_slots_follow_the_level() {
        assert_eq!(WANT1.slots(), 1);
        assert_eq!(WANT2.slots(), 1);
        assert_eq!(WANT3.slots(), RING_SLOTS);
        assert_eq!(RING_SLOTS, MAX_PARKED + 1);
        assert!(!WANT1.probes() && WANT2.probes() && !WANT3.probes());
        assert!(!WANT1.views() && WANT2.views() && WANT3.views());
    }

    #[test]
    fn level_three_builds_a_ring_of_mapped_surfaces_and_never_probes() {
        use Step::*;
        let mut c = Client::new();
        let mut files = 0;
        let log = run3(&mut c, WANT3, &mut files);
        // 11 bring-up + (5 surface + 5 view) per slot + 1 park.
        assert_eq!(log.len(), 11 + 2 * 10 + 1, "{log:?}");
        assert_eq!(log.iter().filter(|s| **s == Park).count(), 1);
        assert_eq!(
            log.iter().filter(|s| **s == AllocMemory).count(),
            RING_SLOTS
        );
        assert!(!log
            .iter()
            .any(|s| matches!(s, FillPattern | ScanoutSet | ScanoutPresent)));
        // The park comes after the first slot's view and before the second surface.
        let park = log.iter().position(|s| *s == Park).unwrap();
        assert_eq!(log[park - 1], KernelMap);
        assert_eq!(log[park + 1], AllocMemory);
        assert_eq!(c.slot_count(), 2);
        assert_eq!(c.parked(), 1);
        assert!(c.presentable(WANT3));
        let (a, b) = (c.slot(0).unwrap(), c.slot(1).unwrap());
        assert_ne!(a.gem, b.gem);
        assert_ne!(a.view.0, b.view.0, "two views");
        assert_eq!(a.layout, b.layout);
        assert!(c.slot(2).is_none());
        // The same machine is quiescent once complete.
        assert_eq!(c.next(WANT3), Action::Idle);
        // A level that wants one surface and no views is not "presentable".
        assert!(!c.presentable(WANT1));
    }

    #[test]
    fn the_ring_is_not_presentable_before_it_is_complete_or_at_another_extent() {
        let mut c = Client::new();
        let mut files = 0;
        // One slot short: stop after the first surface's view.
        let mut n = 0;
        while n < 11 + 10 {
            c.begin(1);
            let Action::Step(s) = c.next(WANT3) else {
                panic!("stopped early")
            };
            let out = match s {
                Step::AllocMemory => Out::Mem(surface_layout(1920, 1080).unwrap()),
                _ => ok_out3(s, &c, &mut files),
            };
            c.finish(s, Ok(out));
            n += 1;
        }
        assert_eq!(c.slot_count(), 1);
        assert!(!c.presentable(WANT3), "half a ring");
        assert_eq!(c.next(WANT3), Action::Step(Step::Park));
        // A complete ring is for one extent only.
        let mut c = Client::new();
        run3(&mut c, WANT3, &mut files);
        assert!(c.presentable(WANT3));
        assert!(!c.presentable(Want {
            level: 3,
            surface: Some((1280, 720))
        }));
        assert!(!c.presentable(Want {
            level: 3,
            surface: None
        }));
        // A dead client is never presentable.
        c.finish(Step::AllocMemory, Err(Fail::new(FailKind::Rm, 0x51)));
        assert!(!c.presentable(WANT3));
    }

    #[test]
    fn a_new_extent_tears_the_whole_ring_down_then_builds_it_again() {
        use Step::*;
        let mut c = Client::new();
        let mut files = 0;
        run3(&mut c, WANT3, &mut files);
        let other = Want {
            level: 3,
            surface: Some((1280, 720)),
        };
        let log = run3(&mut c, other, &mut files);
        // Old slot (the working one): view down, GEM, memory; then the parked one comes
        // back and goes the same way; only then is anything new made.
        let first_alloc = log.iter().position(|s| *s == AllocMemory).unwrap();
        let before = &log[..first_alloc];
        let count = |s: Step| before.iter().filter(|x| **x == s).count();
        assert_eq!(count(Unpark), 1, "{log:?}");
        assert_eq!(count(GemClose), 2, "{log:?}");
        assert_eq!(count(FreeMemory), 2, "{log:?}");
        assert_eq!(count(KernelUnmap), 2, "{log:?}");
        assert_eq!(count(CloseMapCh), 2, "{log:?}");
        // The new ring is whole at the new extent.
        assert!(c.presentable(other));
        assert_eq!(c.slot(0).unwrap().layout.width, 1280);
        assert_eq!(c.slot(1).unwrap().layout.height, 720);
        assert_eq!(log.iter().filter(|s| **s == AllocMemory).count(), 2);
        assert_eq!(c.soft_errors(), 0);
    }

    #[test]
    fn a_failure_in_the_second_slot_closes_the_first_slots_files_too() {
        use Step::*;
        let mut c = Client::new();
        let mut files = 0;
        // Run until the second slot has opened its export file, and fail its export.
        let mut opened = 0;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 80);
            c.begin(1);
            let Action::Step(s) = c.next(WANT3) else {
                panic!("stopped early")
            };
            if s == OpenExportCh {
                opened += 1;
            }
            if s == ExportToFd && opened == 2 {
                c.finish(s, Err(Fail::new(FailKind::Rm, 0x1f)));
                break;
            }
            let out = match s {
                AllocMemory => Out::Mem(surface_layout(1920, 1080).unwrap()),
                _ => ok_out3(s, &c, &mut files),
            };
            c.finish(s, Ok(out));
        }
        assert!(c.is_dead());
        // The first slot's view is the caller's to unmap, and it takes it first.
        let views = c.take_views();
        assert_eq!(views.as_slice().len(), 1, "the parked slot's view");
        let cl = c.take_cleanup();
        let hs = cl.as_slice();
        // Bring-up files (ctl 10, gpu 11, drm 12), the parked slot's map channel, the
        // working slot's export file: each exactly once.
        assert!(
            hs.contains(&10) && hs.contains(&11) && hs.contains(&12),
            "{hs:?}"
        );
        let mut sorted: Vec<u32> = hs.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), hs.len(), "each handle once: {hs:?}");
        assert_eq!(hs.len(), 3 + 2, "{hs:?}");
        // Closing is once: a second call is empty.
        assert_eq!(c.take_cleanup().as_slice().len(), 0);
        assert_eq!(c.slot_count(), 0);
    }

    #[test]
    fn a_given_up_view_stops_the_ring_without_killing_the_client() {
        use Step::*;
        let mut c = Client::new();
        let mut files = 0;
        // First slot complete and parked; the second slot's host mapping is refused.
        let mut parked = false;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 80);
            c.begin(1);
            let Action::Step(s) = c.next(WANT3) else {
                break;
            };
            if s == Park {
                parked = true;
            }
            if parked && s == HostMmap {
                c.finish(s, Err(Fail::new(FailKind::Host, 22)));
                continue;
            }
            let out = match s {
                AllocMemory => Out::Mem(surface_layout(1920, 1080).unwrap()),
                _ => ok_out3(s, &c, &mut files),
            };
            c.finish(s, Ok(out));
        }
        assert!(!c.is_dead());
        assert!(parked);
        assert!(!c.presentable(WANT3));
        assert_eq!(c.parked(), 1);
        assert_eq!(c.next(WANT3), Action::Idle, "no retry, no third slot");
        // The failed slot has a surface but no view: it is not a usable slot.
        assert!(c.slot(1).is_none());
    }

    #[test]
    fn take_views_gives_every_slots_view_once() {
        let mut c = Client::new();
        let mut files = 0;
        run3(&mut c, WANT3, &mut files);
        let want_views = [c.slot(0).unwrap().view, c.slot(1).unwrap().view];
        let v = c.take_views();
        assert_eq!(v.as_slice().len(), 2);
        for w in want_views {
            assert!(v.as_slice().contains(&w));
        }
        assert_eq!(c.take_views().as_slice().len(), 0);
        // The machine no longer believes in either view.
        assert!(!c.presentable(WANT3));
    }

    #[test]
    fn the_level_three_status_word_counts_the_parked_slot() {
        let mut c = Client::new();
        let mut files = 0;
        run3(&mut c, WANT3, &mut files);
        let w = c.status_word();
        // `docs/kmd-rm-client.md` section 13 quotes this as the healthy level 3 reading.
        assert_eq!(w, 0x11b0_5500);
        assert_eq!((w >> 24) & 0xf, 1, "one parked");
        assert_eq!((w >> 20) & 0xf, 11);
        assert_eq!(w >> 28, 1);
        // Levels 1 and 2 never park, so their words keep the documented values.
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        assert_eq!((c.status_word() >> 24) & 0xf, 0);
    }

    #[test]
    fn park_and_unpark_refuse_what_the_machine_never_asks() {
        let mut c = Client::new();
        c.begin(1);
        // An empty working slot cannot be parked.
        c.finish(Step::Park, Ok(Out::Unit));
        assert!(c.is_dead());
        let mut c = Client::new();
        let mut buf = [None; 64];
        run(&mut c, WANT1, &mut buf);
        // A working slot that holds a surface cannot take another.
        c.finish(Step::Unpark, Ok(Out::Unit));
        assert!(c.is_dead());
    }

    #[test]
    fn a_surface_whose_export_file_was_undone_is_torn_down_even_if_the_extent_comes_back() {
        // Stage `Exported`, then the extent changes and the export file is closed as an
        // undo; before the surface is freed the wanted extent returns to the old one.
        let mut c = client_at(3);
        assert_eq!(c.next(OTHER_EXTENT), Action::Step(Step::CloseExportChUndo));
        c.finish(Step::CloseExportChUndo, Ok(Out::Unit));
        assert_eq!(c.export_ch(), 0);
        let act = c.next(WANT1);
        assert!(
            matches!(act, Action::Step(Step::FreeMemory | Step::GemClose)),
            "{act:?}: GemImport on a closed export file would kill the client"
        );
        // And with no primary at all.
        let none = Want {
            level: 1,
            surface: None,
        };
        assert!(matches!(
            c.next(none),
            Action::Step(Step::FreeMemory | Step::GemClose)
        ));
        // It then makes the surface again, from the start.
        let mut guard = 0;
        while let Action::Step(s) = c.next(WANT1) {
            guard += 1;
            assert!(guard < 16);
            let out = ok_out(s, &c);
            c.finish(s, Ok(out));
        }
        assert!(c.ready_surface().is_some());
        assert!(!c.is_dead());
    }

    // ---- level 4: each surface also a foreign resource -------------------------------

    const WANT4: Want = Want {
        level: 4,
        surface: Some((1920, 1080)),
    };

    /// `run3` for level 4: the import answers a distinct resource id.
    fn run4(c: &mut Client, want: Want, files: &mut u32, resid: &mut u32) -> Vec<Step> {
        let mut log = Vec::new();
        loop {
            c.begin(if c.epoch() == 0 { 1 } else { c.epoch() });
            match c.next(want) {
                Action::Step(s) => {
                    assert!(log.len() < 120, "runaway: {log:?}");
                    log.push(s);
                    let out = match s {
                        Step::AllocMemory => {
                            let (w, h) = want.surface.unwrap();
                            Out::Mem(surface_layout(w, h).unwrap())
                        }
                        Step::ForeignImport => {
                            *resid += 1;
                            Out::Resource(*resid)
                        }
                        _ => ok_out3(s, c, files),
                    };
                    c.finish(s, Ok(out));
                }
                Action::Idle | Action::Dead => return log,
            }
        }
    }

    #[test]
    fn level_four_imports_every_surface_once_and_hands_the_resource_ids_out() {
        use Step::*;
        assert!(WANT4.shares() && !WANT3.shares());
        assert_eq!(WANT4.slots(), RING_SLOTS);
        let (mut c, mut files, mut resid) = (Client::new(), 0, 100);
        let log = run4(&mut c, WANT4, &mut files, &mut resid);
        assert_eq!(
            log.iter().filter(|s| **s == ForeignImport).count(),
            2,
            "{log:?}"
        );
        // The import follows the export file's close of the same surface and precedes
        // its view; the park comes after both.
        let imp = log.iter().position(|s| *s == ForeignImport).unwrap();
        assert_eq!(log[imp - 1], CloseExportCh);
        assert_eq!(log[imp + 1], OpenMapCh);
        let park = log.iter().position(|s| *s == Park).unwrap();
        assert!(park > imp);
        assert_eq!(log.len(), 11 + 2 * 11 + 1);
        assert!(c.presentable(WANT4));
        let (a, b) = (c.slot(0).unwrap(), c.slot(1).unwrap());
        assert_eq!((a.foreign, b.foreign), (101, 102));
        assert_eq!(c.next(WANT4), Action::Idle);
        // The status word shows both.
        let w = c.status_word();
        assert_eq!((w >> 16) & 3, 3, "{w:#x}");
        // Level 3 never imports.
        let mut c3 = Client::new();
        let log3 = run3(&mut c3, WANT3, &mut files);
        assert!(!log3.contains(&ForeignImport));
        assert_eq!(c3.slot(0).unwrap().foreign, 0);
    }

    #[test]
    fn a_new_extent_releases_the_foreign_resources_before_the_gem_and_the_memory() {
        use Step::*;
        let (mut c, mut files, mut resid) = (Client::new(), 0, 100);
        run4(&mut c, WANT4, &mut files, &mut resid);
        let other = Want {
            level: 4,
            surface: Some((1280, 720)),
        };
        let log = run4(&mut c, other, &mut files, &mut resid);
        let first_alloc = log.iter().position(|s| *s == AllocMemory).unwrap();
        let before = &log[..first_alloc];
        assert_eq!(
            before.iter().filter(|s| **s == ForeignRelease).count(),
            2,
            "{log:?}"
        );
        // Per surface: the release comes before its GemClose.
        let mut released = 0;
        for s in before {
            match s {
                ForeignRelease => released += 1,
                GemClose => assert!(released >= 1, "GemClose before any release: {log:?}"),
                _ => {}
            }
        }
        let first_gem = before.iter().position(|s| *s == GemClose).unwrap();
        let first_rel = before.iter().position(|s| *s == ForeignRelease).unwrap();
        assert!(first_rel < first_gem);
        assert!(c.presentable(other));
        assert_eq!(c.slot(0).unwrap().foreign, 103);
        assert_eq!(c.soft_errors(), 0);
    }

    #[test]
    fn a_refused_import_is_given_up_for_that_surface_without_killing_anything() {
        use Step::*;
        let mut c = Client::new();
        let mut files = 0;
        let mut imports = 0;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 120);
            c.begin(1);
            let Action::Step(s) = c.next(WANT4) else {
                break;
            };
            if s == ForeignImport {
                imports += 1;
                c.finish(s, Err(Fail::new(FailKind::Host, 95)));
                continue;
            }
            let out = match s {
                AllocMemory => Out::Mem(surface_layout(1920, 1080).unwrap()),
                _ => ok_out3(s, &c, &mut files),
            };
            c.finish(s, Ok(out));
        }
        // Once per surface, never retried; the ring is whole and showing is unaffected.
        assert_eq!(imports, 2);
        assert!(!c.is_dead());
        assert!(c.presentable(WANT4));
        assert_eq!(c.slot(0).unwrap().foreign, 0);
        // A given-up surface has nothing to release.
        let log = {
            let other = Want {
                level: 4,
                surface: Some((1280, 720)),
            };
            let mut log = Vec::new();
            while let Action::Step(s) = c.next(other) {
                assert!(log.len() < 80);
                log.push(s);
                let out = match s {
                    AllocMemory => Out::Mem(surface_layout(1280, 720).unwrap()),
                    ForeignImport => Out::Resource(7),
                    _ => ok_out3(s, &c, &mut files),
                };
                c.finish(s, Ok(out));
            }
            log
        };
        let first_alloc = log.iter().position(|s| *s == AllocMemory).unwrap();
        assert!(!log[..first_alloc].contains(&ForeignRelease), "{log:?}");
    }

    #[test]
    fn a_release_that_fails_still_advances_and_is_counted() {
        use Step::*;
        let (mut c, mut files, mut resid) = (Client::new(), 0, 100);
        run4(&mut c, WANT4, &mut files, &mut resid);
        let other = Want {
            level: 4,
            surface: Some((1280, 720)),
        };
        let mut failed = 0;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 120);
            c.begin(1);
            let Action::Step(s) = c.next(other) else {
                break;
            };
            if s == ForeignRelease && failed == 0 {
                failed += 1;
                c.finish(s, Err(Fail::new(FailKind::Transport, 1)));
                continue;
            }
            let out = match s {
                AllocMemory => Out::Mem(surface_layout(1280, 720).unwrap()),
                ForeignImport => {
                    resid += 1;
                    Out::Resource(resid)
                }
                _ => ok_out3(s, &c, &mut files),
            };
            c.finish(s, Ok(out));
        }
        assert!(!c.is_dead());
        assert_eq!(c.soft_errors(), 1);
        assert!(c.presentable(other));
    }

    #[test]
    fn dropping_from_level_four_to_three_releases_the_resources() {
        use Step::*;
        let (mut c, mut files, mut resid) = (Client::new(), 0, 100);
        run4(&mut c, WANT4, &mut files, &mut resid);
        let log = run3(&mut c, WANT3, &mut files);
        assert_eq!(
            log.iter().filter(|s| **s == ForeignRelease).count(),
            1,
            "{log:?} (the working slot)"
        );
        assert_eq!(c.foreign(), 0);
    }
}

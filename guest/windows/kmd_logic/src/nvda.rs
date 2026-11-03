//! NVIDIA Windows UMD `NVDA` escapes: the small RM-control subset.
//!
//! The UMD's pseudo handles are translated to objects in the caller's own RM
//! client. Unknown handles are refused here, before a host message can be built;
//! in particular channel and engine pseudo handles are not aliases of the three
//! objects this subset knows about.

use crate::rm_client as rc;

pub const NVDA_MAGIC: u32 = 0x4e56_4441;
pub const NVDA_VERSION: u32 = 0x0001_0002;
pub const NVDA_HEADER_BYTES: usize = 0x44;
/// Bytes through the second common result word. Several local-only NVDA
/// requests are shorter than the RM-control header.
pub const NVDA_COMMON_HEADER_BYTES: usize = 0x30;
pub const NVDA_MAX_BYTES: usize = 1024 * 1024;

pub const NVDA_SIZE_AT: usize = 0x08;
pub const NVDA_TAG_AT: usize = 0x0c;
/// The tag (`**VN`) of CUDA's and NVAPI's escapes and of CUDA allocation records.
pub const NVDA_TAG_NV: u32 = 0x4e56_2a2a;
pub const NVDA_FUNCTION_AT: usize = 0x10;
pub const NVDA_RESULT_AT: usize = 0x14;
pub const NVDA_RESULT2_AT: usize = 0x2c;

pub const RM_CONTROL_FUNCTION: u32 = 0x2b;
pub const RM_FUNCTION_MASK: u32 = 0x00ff_ffff;
pub const RM_H_CLIENT_AT: usize = 0x30;
pub const RM_H_OBJECT_AT: usize = 0x34;
pub const RM_CMD_AT: usize = 0x38;
pub const RM_PARAMS_SIZE_AT: usize = 0x3c;
pub const RM_STATUS_AT: usize = 0x40;
pub const RM_PARAMS_AT: usize = NVDA_HEADER_BYTES;

/// Host object handles in one RM client's namespace.
pub const H_DEVICE: u32 = 0x4e56_0001;
pub const H_SUBDEVICE: u32 = 0x4e56_0002;

pub const PSEUDO_CLIENT_CUDA: u32 = 0xff02_8010;
pub const PSEUDO_DEVICE: u32 = 0xff02_0000;
pub const PSEUDO_DEVICE_CUDA: u32 = 0xff02_0010;
pub const PSEUDO_SUBDEVICE: u32 = 0xff03_0000;
/// The escape context's KernelGraphicsContext (NV0090) in UMD RM controls.
pub const PSEUDO_GRAPHICS_CONTEXT: u32 = 0xff04_0000;
pub const NV0090_CTRL_CMD_SET_LG_SECTOR_PROMOTION: u32 = 0x0090_010b;

/// The validated fixed part of an NVDA escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub tag: u32,
    pub function: u32,
}

impl Header {
    pub const fn is_rm_control(self) -> bool {
        is_rm_control_function(self.function)
    }
}

pub const fn is_rm_control_function(function: u32) -> bool {
    function & RM_FUNCTION_MASK == RM_CONTROL_FUNCTION
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    Short,
    BadMagic,
    BadVersion,
    BadSize,
    TooLarge,
}

/// Validate the complete escape, not merely the prefix. `size` is the exact
/// buffer length and is capped before it can be used for any later arithmetic.
pub fn parse_header(buf: &[u8]) -> Result<Header, HeaderError> {
    if buf.len() < NVDA_COMMON_HEADER_BYTES {
        return Err(HeaderError::Short);
    }
    if rc::get32(buf, 0) != Some(NVDA_MAGIC) {
        return Err(HeaderError::BadMagic);
    }
    if rc::get32(buf, 4) != Some(NVDA_VERSION) {
        return Err(HeaderError::BadVersion);
    }
    let size = rc::get32(buf, NVDA_SIZE_AT).ok_or(HeaderError::Short)? as usize;
    if size > NVDA_MAX_BYTES || buf.len() > NVDA_MAX_BYTES {
        return Err(HeaderError::TooLarge);
    }
    if size < NVDA_COMMON_HEADER_BYTES || size != buf.len() {
        return Err(HeaderError::BadSize);
    }
    Ok(Header {
        tag: rc::get32(buf, NVDA_TAG_AT).ok_or(HeaderError::Short)?,
        function: rc::get32(buf, NVDA_FUNCTION_AT).ok_or(HeaderError::Short)?,
    })
}

/// A validated `RM_CONTROL` and its flat, in/out parameter block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RmControl<'a> {
    pub header: Header,
    pub h_client: u32,
    pub h_object: u32,
    pub cmd: u32,
    pub status: u32,
    pub params: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    Header(HeaderError),
    NotRmControl,
    ParamsSize,
}

impl From<HeaderError> for ControlError {
    fn from(e: HeaderError) -> Self {
        Self::Header(e)
    }
}

pub fn parse_rm_control(buf: &[u8]) -> Result<RmControl<'_>, ControlError> {
    let header = parse_header(buf)?;
    if buf.len() < NVDA_HEADER_BYTES {
        return Err(ControlError::Header(HeaderError::Short));
    }
    if !header.is_rm_control() {
        return Err(ControlError::NotRmControl);
    }
    let params_size = rc::get32(buf, RM_PARAMS_SIZE_AT).ok_or(ControlError::ParamsSize)? as usize;
    let params = buf.get(RM_PARAMS_AT..).ok_or(ControlError::ParamsSize)?;
    if params_size != params.len() {
        return Err(ControlError::ParamsSize);
    }
    Ok(RmControl {
        header,
        h_client: rc::get32(buf, RM_H_CLIENT_AT).ok_or(ControlError::ParamsSize)?,
        h_object: rc::get32(buf, RM_H_OBJECT_AT).ok_or(ControlError::ParamsSize)?,
        cmd: rc::get32(buf, RM_CMD_AT).ok_or(ControlError::ParamsSize)?,
        status: rc::get32(buf, RM_STATUS_AT).ok_or(ControlError::ParamsSize)?,
        params,
    })
}

/// The host objects corresponding to the UMD's root/device/subdevice aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientMap {
    pub root: u32,
    pub device: u32,
    pub subdevice: u32,
    /// Real objects created by the native WDDM path and therefore safe to name
    /// in a later RM control. Zero is the empty slot marker.
    pub objects: [u32; 64],
    pub objects_len: u8,
}

/// Record `object` (a native allocation's RM memory) as nameable in `objects`
/// (the first `*len` entries). `false` when it is 0, already there, or the list
/// is full.
pub fn note_object(objects: &mut [u32; 64], len: &mut u8, object: u32) -> bool {
    let n = usize::from(*len).min(objects.len());
    if object == 0 || objects[..n].contains(&object) || n == objects.len() {
        return false;
    }
    objects[n] = object;
    *len = (n + 1) as u8;
    true
}

/// Remove `object` again (its allocation was destroyed): a freed handle must not
/// stay nameable, and the list must not fill up with them. The last entry takes
/// its place.
pub fn forget_object(objects: &mut [u32; 64], len: &mut u8, object: u32) -> bool {
    let n = usize::from(*len).min(objects.len());
    let Some(i) = objects[..n].iter().position(|&o| o == object && o != 0) else {
        return false;
    };
    objects[i] = objects[n - 1];
    objects[n - 1] = 0;
    *len = (n - 1) as u8;
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    Client(u32),
    Object(u32),
}

/// Translate only the pseudo handles observed for this subset. Client and
/// object errors stay distinct so the KMD can account for them separately.
pub fn translate_handles(
    h_client: u32,
    h_object: u32,
    map: &ClientMap,
) -> Result<(u32, u32), HandleError> {
    let client = match h_client {
        0 | PSEUDO_CLIENT_CUDA => map.root,
        other if other == map.root => map.root,
        other => return Err(HandleError::Client(other)),
    };
    let object = match h_object {
        0 => map.root,
        PSEUDO_DEVICE | PSEUDO_DEVICE_CUDA => map.device,
        PSEUDO_SUBDEVICE => map.subdevice,
        other if other == map.device => map.device,
        other if other == map.subdevice => map.subdevice,
        other if map.objects[..usize::from(map.objects_len)].contains(&other) => other,
        other => return Err(HandleError::Object(other)),
    };
    Ok((client, object))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    Handle(HandleError),
    ParamsTooLarge,
    OutputTooSmall,
}

impl From<HandleError> for BuildError {
    fn from(e: HandleError) -> Self {
        Self::Handle(e)
    }
}

/// Build `MsgHeader | IoctlReq | NVOS54 | flat RM parameters` on `ctl_handle`.
/// Translation is completed before any output byte is written.
pub fn build_host_control(
    out: &mut [u8],
    ctl_handle: u32,
    control: &RmControl<'_>,
    map: &ClientMap,
) -> Result<usize, BuildError> {
    let (client, object) = translate_handles(control.h_client, control.h_object, map)?;
    let params_size =
        u32::try_from(control.params.len()).map_err(|_| BuildError::ParamsTooLarge)?;
    let data = rc::nvos54(client, object, control.cmd, params_size);
    rc::build_ioctl(
        out,
        ctl_handle,
        rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
        &data,
        control.params,
    )
    .ok_or(BuildError::OutputTooSmall)
}

/// The two independent results in a host RM-control answer: transport/host
/// success was already established; `rm_status` is RM's own verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostReply<'a> {
    pub rm_status: u32,
    pub params: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostReplyError {
    Malformed,
    Host(i32),
}

/// Parse a host answer whose data block must be exactly NVOS54 and whose nested
/// block must be exactly the size sent. A nonzero RM status is a valid answer,
/// not a host refusal.
pub fn parse_host_reply(
    resp: &[u8],
    expected_params: usize,
) -> Result<HostReply<'_>, HostReplyError> {
    let reply = match rc::parse_ioctl_reply(resp) {
        Ok(reply) => reply,
        Err(rc::ReplyError::Short) => return Err(HostReplyError::Malformed),
        Err(rc::ReplyError::Host(status)) => return Err(HostReplyError::Host(status)),
    };
    if reply.data.len() != rc::NVOS54_BYTES || reply.nested.len() != expected_params {
        return Err(HostReplyError::Malformed);
    }
    let rm_status = rc::get32(reply.data, rc::NVOS54_STATUS_AT).ok_or(HostReplyError::Malformed)?;
    Ok(HostReply {
        rm_status,
        params: reply.nested,
    })
}

/// [`parse_host_reply`] plus authentication against the exact request. Besides
/// preventing a malformed reply from being applied, the exact total length rules
/// out an unrequested deep block or trailing bytes.
pub fn parse_host_reply_for<'a>(
    resp: &'a [u8],
    ctl_handle: u32,
    control: &RmControl<'_>,
    map: &ClientMap,
) -> Result<HostReply<'a>, HostReplyError> {
    let expected_len = rc::REPLY_DATA + rc::NVOS54_BYTES + control.params.len();
    if resp.len() != expected_len
        || rc::get32(resp, 0) != Some(rc::MSG_IOCTL)
        || rc::get32(resp, 4) != Some(ctl_handle)
        || rc::get32(resp, rc::MSG_HDR + 8) != Some(0)
    {
        return Err(HostReplyError::Malformed);
    }
    let (client, object) = translate_handles(control.h_client, control.h_object, map)
        .map_err(|_| HostReplyError::Malformed)?;
    let reply = parse_host_reply(resp, control.params.len())?;
    let at = rc::REPLY_DATA;
    if rc::get32(resp, at) != Some(client)
        || rc::get32(resp, at + 4) != Some(object)
        || rc::get32(resp, at + 8) != Some(control.cmd)
        || rc::get32(resp, at + 24) != Some(control.params.len() as u32)
    {
        return Err(HostReplyError::Malformed);
    }
    Ok(reply)
}

/// Apply an answered control to its original NVDA buffer. The declared size and
/// every byte other than the two result words, RM status and parameters survive.
pub fn apply_host_reply(escape: &mut [u8], reply: &HostReply<'_>) -> Result<(), ControlError> {
    let control = parse_rm_control(escape)?;
    if control.params.len() != reply.params.len() {
        return Err(ControlError::ParamsSize);
    }
    let params = escape
        .get_mut(RM_PARAMS_AT..)
        .ok_or(ControlError::ParamsSize)?;
    params.copy_from_slice(reply.params);
    put32(escape, NVDA_RESULT_AT, 1);
    put32(escape, NVDA_RESULT2_AT, 1);
    put32(escape, RM_STATUS_AT, reply.rm_status);
    Ok(())
}

/// Result of trying the capture-backed local NVDA subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOutcome {
    /// A complete local reply is already in the original buffer.
    Applied,
    /// This is an RM control and must continue down the real host-RM path.
    ForwardRmControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalError {
    /// The function is outside the implemented local subset and is not RM control.
    Unsupported,
    /// Bad function-specific size or a missing handle required by that function.
    Invalid,
}

/// Apply the pure-local, native 610.88 CUDA/NVAPI replies observed in
/// `cudatest-12300.escapes-v.txt` / `.rm-v.txt`.
///
/// Every function has an exact REQUEST size. The three replies whose declared
/// size becomes 0x30 still occupy and preserve the entire original allocation;
/// in particular this routine never reparses the shortened reply as though its
/// backing slice had shrunk. All bytes not listed in a function's arm survive.
/// `h_device == 0` is valid for the adapter-scoped NVAPI calls.
pub fn apply_local(
    buf: &mut [u8],
    h_device: usize,
    h_context: usize,
) -> Result<LocalOutcome, LocalError> {
    let header = parse_header(buf).map_err(|_| LocalError::Invalid)?;
    fn exact(buf: &[u8], size: usize) -> Result<(), LocalError> {
        if buf.len() == size {
            Ok(())
        } else {
            Err(LocalError::Invalid)
        }
    }
    fn context(device: usize, context: usize) -> Result<(), LocalError> {
        if device == 0 || context == 0 {
            Err(LocalError::Invalid)
        } else {
            Ok(())
        }
    }
    match header.function {
        0x0700_0002 => {
            exact(buf, 0x238)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x34, 1);
            put32(buf, 0x38, 0x100);
            put32(buf, 0x138, 1);
        }
        0x0700_0003 => {
            exact(buf, 0x238)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0700_00ad => {
            exact(buf, 0x3c)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, 0x28, 0);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x34, 0x3200);
            put32(buf, 0x38, 1);
        }
        0x0700_0016 => {
            // Both cudatest captures answer 0x100 at +0x34 (byte +0x35 = 1).
            exact(buf, 0x38)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x34, 0x100);
        }
        0x0100_0113 => {
            exact(buf, 0x45)?;
            // Device-scoped, deliberately without an hContext requirement.
            if h_device == 0 {
                return Err(LocalError::Invalid);
            }
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, 0x30, 0x2000_0001);
            put32(buf, 0x34, 0);
            put32(buf, 0x38, 0x20);
            put32(buf, 0x3c, 0xffff_ff00);
            put8(buf, 0x40, 0xff);
        }
        0x0200_001e => {
            exact(buf, 0x38)?;
            // EXPERIMENTAL: the capture proves this field-level reply shape, not
            // the full semantics of the operation. Require the real WDDM device
            // and context scopes rather than faking success for unscoped calls.
            context(h_device, h_context)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0100_0140 => {
            exact(buf, 0x36)?;
            context(h_device, h_context)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0100_0152 => {
            // Adapter-scoped query D3D's UMD makes first (also from DWM). The
            // 610.88 capture on a real RTX 5090 answers it with result 1,
            // status -23 (0xffffffe9) and a 0x30-byte reply, and the UMD
            // carries on; mirror exactly that.
            exact(buf, 0x3e)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 0xffff_ffe9);
        }
        0x0100_0090 => {
            // Device-scoped query the D3D11 UMD makes after its first HW queue.
            // The 610.88 capture on a real RTX 5090 answers result 1 at both
            // result words with a 0x30-byte reply; the request tail is kept.
            exact(buf, 0x3c)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0100_006b => {
            // Device-scoped 0x268-byte request the D3D11 UMD makes before its
            // first resource allocations (+0x30 = 4, +0x38 varies per run).
            // The 610.88 capture answers it like 0x01000090.
            exact(buf, 0x268)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0200_000c => {
            // 0xac-byte per-head display query of the Vulkan ICD (head index at +0x34,
            // asked for heads 0..3 before it creates a device). The 610.88 capture
            // answers a 0x30-byte reply, result 1; head 0 carries its mode at +0x40:
            // width, height, 32 bpp, refresh Hz, format 0x15 (A8R8G8B8), and 1 at +0x88
            // and +0x9d; the other heads stay zero. The KMD fills head 0's real mode
            // over these defaults (`set_head_mode`).
            exact(buf, 0xac)?;
            let head = rc::get32(buf, 0x34).unwrap_or(u32::MAX);
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            // Heads 1..3 answer -14 in the second result word (capture seq
            // 933-935, 1411-1413); the ICD then skips its 0x01000069 query for them.
            put32(
                buf,
                NVDA_RESULT2_AT,
                if head == 0 { 1 } else { HEAD_ABSENT_RESULT2 },
            );
            for at in (0x40..0xac).step_by(4) {
                put32(buf, at, 0);
            }
            if head == 0 {
                set_head_mode(buf, 1024, 768, 60);
                put32(buf, 0x48, 32);
                put32(buf, 0x50, 0x15);
                put32(buf, 0x88, 1);
                put32(buf, 0x9c, 0x100);
            }
        }
        0x0100_0057 => {
            // 0x38-byte request the Vulkan ICD makes after creating its device. The
            // capture (seq 937, 1808) answers a 0x30-byte reply, result 1, 0 in the
            // second result word and a nonzero 32-bit value at +0x30 that differs
            // per run (an identifier).
            exact(buf, 0x38)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 0);
            put32(buf, 0x30, 0x1c95_a71e ^ (h_device as u32));
            put32(buf, 0x34, 0);
        }
        0x0100_0069 | 0x0100_0107 => {
            // Vulkan ICD queries the 610.88 capture answers with a bare 0x30-byte
            // success (0x0107: also 1 at +0x9c).
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            if header.function == 0x0100_0107 {
                exact(buf, 0xa8)?;
                put32(buf, 0x9c, 1);
            }
        }
        0x0100_00a4 => {
            // Answered by NVIDIA's KMD with result 1 and -26 in the second result word.
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, (-26i32) as u32);
        }
        0x0100_00a7 => {
            // 0x9c0-byte display description: 1 at +0x34, the mode at +0x40/+0x44
            // and again at +0xcc/+0xd0, 1 at +0xb8 and +0xbc (the KMD fills in
            // the real mode, `set_display_info_mode`).
            exact(buf, 0x9c0)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            for at in (0x30..0x9c0).step_by(4) {
                put32(buf, at, 0);
            }
            put32(buf, 0x34, 1);
            put32(buf, 0xb8, 1);
            put32(buf, 0xbc, 1);
            set_display_info_mode(buf, 1024, 768);
        }
        0x0100_0116 => {
            // 0x39-byte request the D3D11 UMD makes while it sets up a swap chain
            // (DWM's primary every retry). The 610.88 capture answers a 0x30-byte
            // reply, result 1 at both result words and 1 at +0x34. Refused
            // (NOT_IMPLEMENTED), DWM went on to a SetDisplayMode that failed.
            exact(buf, 0x39)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            for at in (0x30..0x38).step_by(4) {
                put32(buf, at, 0);
            }
            put32(buf, 0x34, 1);
            if let Some(last) = buf.get_mut(0x38) {
                *last = 0;
            }
        }
        0x0100_0021 | 0x0100_0088 => {
            // 0x0021 is the ICD's device-lost check (nvoglv64 0xa8e610 per context
            // with that hContext, 0xa8e730 device-wide with none), called once a
            // second by a worker thread and on every turn of the ICD's busy-wait
            // loops (GPU progress, pushbuffer space). The ICD declares the device
            // lost when the escape fails, when +0x2c is not 1, when a per-context
            // reply has +0x34 != 0 (device-wide: when +0x34 changes, a reset
            // counter), or when GetDeviceState after it is not ACTIVE. +0x20 is the
            // ICD's uninitialised stack, not a token. NVIDIA's KMD answers in about
            // 2 us without blocking (the capture: 228 polls, median 2.4 us), so the
            // spinning is the ICD's own. 0x0088 has 5 at +0x30. Refused, vkCreateDevice
            // spun on 0x0021 forever. The 610.88 capture answers both with result 1
            // in both result words; 0x0088's reply shrinks to 0x30 bytes, 0x0021's
            // keeps its 0x38 (capture seq 2440, 1992).
            if header.function == 0x0100_0021 {
                exact(buf, 0x38)?;
            } else {
                exact(buf, 0x34)?;
                put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            }
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0100_007c => {
            // Vulkan ICD query (1 at +0x20); the capture answers a 0x30-byte reply
            // with 1 in the 64-bit word at +0x30.
            exact(buf, 0x38)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x30, 1);
            put32(buf, 0x34, 0);
        }
        0x0100_0001 => {
            // 0xc0-byte driver identity query (field mask at +0x20). The 610.88
            // capture answers the driver version as 61088 at +0x30, the branch
            // string at +0x40 and the adapter name at +0x80, size unchanged.
            exact(buf, 0xc0)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            for at in (0x30..0xc0).step_by(4) {
                put32(buf, at, 0);
            }
            put32(buf, 0x30, NVDA_DRIVER_VERSION);
            buf[0x40..0x40 + NVDA_DRIVER_BRANCH.len()].copy_from_slice(NVDA_DRIVER_BRANCH);
            buf[0x80..0x80 + NVDA_ADAPTER_NAME.len()].copy_from_slice(NVDA_ADAPTER_NAME);
        }
        0x0100_010e => {
            // Adapter-scoped 0x34-byte NVAPI query (Escape Flags 8, no hDevice).
            // The 610.88 capture (vktest seq 3789, 3812) keeps the size and answers
            // result 1 in both result words and 0x100 at +0x30.
            exact(buf, 0x34)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x30, 0x100);
        }
        0x0200_001d => {
            // 0x34-byte device- and context-scoped query the D3D12 UMD makes once
            // per present (the Vulkan ICD's swap chain presents through it). The
            // capture answers a 0x30-byte reply, result 1 in both result words and
            // a word at +0x30: 1 for the first five presents (seq 5797...), 0x20
            // for three, then 0x100. This answers the first frames' 1.
            exact(buf, 0x34)?;
            context(h_device, h_context)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x30, 1);
        }
        0x0200_0031 => {
            // 0x34-byte device-scoped request the D3D12 UMD makes at teardown
            // (+0x30 a value from the UMD). The capture (seq 19409) answers a bare
            // 0x30-byte success; the request word stays.
            exact(buf, 0x34)?;
            if h_device == 0 {
                return Err(LocalError::Invalid);
            }
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0100_002d => {
            // Device-scoped 0x3c-byte query the D3D UMD (nvwgf2umx) sends every 32nd
            // time through one of its per-frame paths (DWM too). The UMD reads only
            // +0x34 & +0x38: nonzero sets a per-device flag (byte +0xeb of its state);
            // a failure just clears it. The 610.88 captures answer a 0x30-byte reply,
            // result 1 at both result words and nonzero words at +0x30/+0x34/+0x38
            // (d3d11test 1/1/1, d3d12test 2/3/3, DWM 0x16a/-1/-1). +0x20 is the UMD's
            // uninitialised stack and stays. Refused, DWM flooded the VkF ring.
            exact(buf, 0x3c)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x30, 1);
            put32(buf, 0x34, 1);
            put32(buf, 0x38, 1);
        }
        0x0100_00bb => {
            // 0x390-byte record the D3D UMD pushes (sub-op 3 at +0x30, then
            // counters and doubles: frame-time statistics). NVIDIA's KMD only takes
            // it: the L3 capture (b4, DWM and d3d12test) answers a 0x30-byte reply,
            // result 1 at both result words, every payload byte unchanged.
            exact(buf, 0x390)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0100_00ae => {
            // 0x34-byte adapter query of the OpenGL/Vulkan ICD (nvoglv64 0xa5ea20,
            // `**VN` tag): one 32-bit value at +0x30, stored in its context (+0xbb8).
            // No capture holds it (the captured runs never sent it). The ICD zeroes
            // the value before the call and keeps 0 when the escape fails, so a
            // success with 0 at +0x30 is exactly what it ends up with either way,
            // without the refusal (52 per Steel Nomad run).
            exact(buf, 0x34)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x30, 0);
        }
        0x0700_00f1 => {
            // 0x40-byte adapter-scoped NVAPI query (Flags 8): the display at +0x34,
            // given as an NVAPI display id (0x8006108n) or as a one-bit display mask
            // (0x100, 0x2000), answered with its index k in two forms: +0x38 = 0x110 + k,
            // +0x3c = 0x1100 + k, 0x100 at +0x30, +0x34 kept (all 662 replies in the
            // vktest and L3 captures follow `display_index`). A display it cannot name
            // keeps the request's -23 in the second result word.
            exact(buf, 0x40)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, 0x30, 0x100);
            match display_index(rc::get32(buf, 0x34).unwrap_or(0)) {
                Some(k) => {
                    put32(buf, NVDA_RESULT2_AT, 1);
                    put32(buf, 0x38, 0x110 + k);
                    put32(buf, 0x3c, 0x1100 + k);
                }
                None => {
                    put32(buf, NVDA_RESULT2_AT, DISPLAY_UNKNOWN_RESULT2);
                    put32(buf, 0x38, 0);
                    put32(buf, 0x3c, 0);
                }
            }
        }
        0x0700_010c => {
            // 0x84-byte adapter-scoped NVAPI query: the NVAPI display id at +0x30 to
            // its display mask, size kept: +0x34 0x100, +0x38 and +0x3c the one-bit
            // mask (0x100 << k, the `display_index` rule), +0x40 0xffffffff (all 2501
            // replies of the vktest and L3 captures, ids 0x80061080..87). An id it
            // cannot name keeps the request's -23 in the second result word.
            exact(buf, 0x84)?;
            put32(buf, NVDA_RESULT_AT, 1);
            let id = rc::get32(buf, 0x30).unwrap_or(0);
            match display_index(id).filter(|_| id & !7 == NVAPI_DISPLAY_ID_BASE) {
                Some(k) => {
                    put32(buf, NVDA_RESULT2_AT, 1);
                    put32(buf, 0x34, 0x100);
                    put32(buf, 0x38, 0x100 << k);
                    put32(buf, 0x3c, 0x100 << k);
                    put32(buf, 0x40, 0xffff_ffff);
                }
                None => put32(buf, NVDA_RESULT2_AT, DISPLAY_UNKNOWN_RESULT2),
            }
        }
        0x0100_0041 => {
            // 0xac0-byte NVAPI/UMD settings push (`**VN`; a table of 0x24 entries of
            // 0x40 bytes, ids 1..0x24). The L3 captures (60 pairs) answer a 0x30-byte
            // reply, result 1 and 0 in the second result word.
            exact(buf, 0xac0)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 0);
        }
        0x0100_012b => {
            // 0x38-byte D3D (`NVDX`) request with a 32-bit value at +0x20 (DWM in the
            // L3 capture, 9 pairs): a 0x30-byte reply, result 1, 0 in the second
            // result word, +0x20 kept.
            exact(buf, 0x38)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 0);
        }
        0x0200_0046 => {
            // 0xac-byte "OCA Data" record of the OpenGL/Vulkan ICD (nvoglv64
            // 0x1810768c5, `NVDX` tag; its log line "// OCA Data = ShaderType=%s |
            // ShaderSize=%08d | AppHash=%016llx | UcodeHash=%016llx"): the shader
            // being run, for an Online Crash Analysis dump. The ICD ignores the
            // answer; no capture holds one. Taken as a bare 0x30-byte success.
            exact(buf, 0xac)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        0x0700_0113 => {
            // 0x3c-byte adapter-scoped NVAPI query: the one-bit display mask at +0x34
            // (+0x30 0x100 from the caller) to its NVAPI display id at +0x38 (mask 0x100
            // -> 0x80061083, 0x2000 -> 0x80061086 in all 51 captured replies).
            exact(buf, 0x3c)?;
            put32(buf, NVDA_RESULT_AT, 1);
            let mask = rc::get32(buf, 0x34).unwrap_or(0);
            match display_index(mask).filter(|_| mask.is_power_of_two() && mask < 0x8000_0000) {
                Some(k) => {
                    put32(buf, NVDA_RESULT2_AT, 1);
                    put32(buf, 0x38, display_id_of(k));
                }
                None => {
                    put32(buf, NVDA_RESULT2_AT, DISPLAY_UNKNOWN_RESULT2);
                    put32(buf, 0x38, 0);
                }
            }
        }
        0x0100_000d => {
            // 0xfc0-byte adapter-scoped NVAPI display-timing query (Flags 8, no
            // hDevice; the vktest capture seq 3783 from nvapi64_impl via the ICD).
            // Answered from the capture's reply with the size kept; the KMD fills in
            // the mode this adapter scans out (`set_display_timing`).
            exact(buf, DISPLAY_TIMING_BYTES)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            for at in (0x30..DISPLAY_TIMING_BYTES).step_by(4) {
                put32(buf, at, 0);
            }
            for &(at, value) in DISPLAY_TIMING_FIXED.iter() {
                put32(buf, at, value);
            }
            set_display_timing(buf, 1024, 768, 60_004);
        }
        function
            if is_rm_control_function(function)
                && parse_rm_control(buf).is_ok_and(|c| c.cmd == NV2080_CTRL_CMD_GPU_QUERY_ECC_STATUS) =>
        {
            // NV2080 GPU_QUERY_ECC_STATUS (the Vulkan ICD asks it 7 times while it
            // creates a device). A GeForce has no ECC: NVIDIA's KMD answers RM
            // status NOT_SUPPORTED itself, in a 0x30-byte reply with -19 in the second
            // result word (vktest capture seq 1352...1837). Answered the same here,
            // without a host round trip; the shortened reply no longer parses as an
            // RM control, so the KMD does not count it as a refusal.
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, ECC_RESULT2);
            put32(buf, RM_STATUS_AT, NV_ERR_NOT_SUPPORTED);
        }
        0x0100_0124 => {
            // Device-scoped 0x48-byte request the D3D11 UMD makes when a swap
            // chain is created and destroyed (+0x30 = 1 or 0, two process
            // handles at +0x38/+0x40). The 610.88 capture answers it with a
            // 0x30-byte reply, result 1 at both result words and +0x30..+0x48
            // cleared. Refused, the UMD sent it again around every present.
            exact(buf, 0x48)?;
            put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            for at in (0x30..0x48).step_by(4) {
                put32(buf, at, 0);
            }
        }
        function
            if is_rm_control_function(function)
                && parse_rm_control(buf).is_ok_and(|c| {
                    c.cmd == NV0090_CTRL_CMD_SET_LG_SECTOR_PROMOTION
                        && c.h_object == PSEUDO_GRAPHICS_CONTEXT
                }) =>
        {
            // NV0090 SET_LG_SECTOR_PROMOTION on the escape's context's
            // KernelGraphicsContext (pseudo handle 0xff040000), sent by the D3D11
            // UMD after its first queue. That object is RM-internal to the
            // channel group and has no client handle here; the control is an L2
            // promotion hint. Answered as the 610.88 capture shows (result 1
            // in both result words, params untouched), not forwarded.
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        function
            if is_rm_control_function(function)
                && parse_rm_control(buf).is_ok_and(|c| {
                    c.cmd == NV906F_CTRL_CMD_GET_CLASS_ENGINEID
                        && c.h_object == PSEUDO_GRAPHICS_CONTEXT
                        && c.params.len() == 0x10
                        && class_engine_of(rc::get32(c.params, 0).unwrap_or(0)).is_some()
                }) =>
        {
            // NV906F GET_CLASS_ENGINEID on the escape context's channel (pseudo
            // 0xff040000) for the class objects NVIDIA's KMD puts on every graphics
            // channel (pseudo handles 0xff1fe0xx), asked by the Vulkan ICD before
            // its first queue submission. As the 610.88 capture answers them (seq
            // 1989-1991, 2327-2329): params {hObject, classEngineID = classID,
            // classID, engineID 1 (GRAPHICS)}, RM status 0, result 1 in both result
            // words. Refused, the ICD fell back to NV0090 SET_LG_SECTOR_PROMOTION.
            let p = RM_PARAMS_AT;
            let class = class_engine_of(rc::get32(buf, p).unwrap_or(0)).unwrap_or(0);
            put32(buf, p + 4, class);
            put32(buf, p + 8, class);
            put32(buf, p + 0xc, NV2080_ENGINE_TYPE_GRAPHICS);
            put32(buf, RM_STATUS_AT, 0);
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        function
            if is_rm_control_function(function)
                && parse_rm_control(buf).is_ok_and(|c| c.h_object == PSEUDO_DISPLAY_COMMON) =>
        {
            // NV0073 controls on the display-common object (pseudo handle 0xff0a0000)
            // that NVIDIA's KMD answers itself; the guest's RM client has no such
            // object (the host owns the display). As the 610.88 capture answers them.
            let cmd = rc::get32(buf, RM_CMD_AT).unwrap_or(0);
            let p = RM_PARAMS_AT;
            match cmd {
                NV0073_CTRL_CMD_SYSTEM_GET_CAPS => {
                    put8(buf, p, 0x81);
                    put8(buf, p + 1, 0x2c);
                }
                NV0073_CTRL_CMD_SYSTEM_GET_NUM_HEADS => {
                    // {subDeviceInstance, flags, numHeads}: 4 heads, 1 with flags.
                    let flags = rc::get32(buf, p + 4).unwrap_or(0);
                    put32(buf, p + 8, if flags == 0 { 4 } else { 1 });
                }
                NV0073_CTRL_CMD_SYSTEM_GET_ACTIVE => {
                    // Per head (+4): head 0 is the active one (+0xd).
                    let head = rc::get32(buf, p + 4).unwrap_or(u32::MAX);
                    put8(buf, p + 0xd, u8::from(head == 0));
                }
                NV0073_CTRL_CMD_SYSTEM_GET_RASTER => {
                    // Head 0's raster; the KMD fills in the real mode (`set_raster`).
                    set_raster(buf, 1024, 768, 60);
                }
                _ => {
                    put32(buf, RM_STATUS_AT, NV_ERR_NOT_SUPPORTED);
                }
            }
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
        }
        function if is_rm_control_function(function) => {
            // 0x0500002b and every other RM control remain real host-RM work.
            return Ok(LocalOutcome::ForwardRmControl);
        }
        _ => return Err(LocalError::Unsupported),
    }
    Ok(LocalOutcome::Applied)
}

fn put8(buf: &mut [u8], at: usize, value: u8) {
    if let Some(byte) = buf.get_mut(at) {
        *byte = value;
    }
}

fn put32(buf: &mut [u8], at: usize, value: u32) {
    if let Some(word) = buf.get_mut(at..at.checked_add(4).unwrap_or(usize::MAX)) {
        word.copy_from_slice(&value.to_le_bytes());
    }
}

/// What to do with the NVDA RM-control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Persona {
    Off = 0,
    Census = 1,
    Forward = 2,
}

impl Persona {
    pub const fn from_knob(value: u32) -> Self {
        match value {
            1 => Self::Census,
            2 => Self::Forward,
            _ => Self::Off,
        }
    }
}

impl From<u32> for Persona {
    fn from(value: u32) -> Self {
        Self::from_knob(value)
    }
}

/// NVDA 0x01000077: the address of a block in the caller's address space.
/// Driver identity `0x01000001` reports: the NVIDIA UMD build these replies were
/// captured with (610.88), its branch string and the adapter name.
pub const NVDA_DRIVER_VERSION: u32 = 61088;
pub const NVDA_DRIVER_BRANCH: &[u8] = b"r610_85-151";
pub const NVDA_ADAPTER_NAME: &[u8] = b"NVIDIA GeForce RTX 5090";

pub const NVDA_FN_USER_SHARED: u32 = 0x0100_0077;
pub const NVDA_FN_USER_SHARED_BYTES: usize = 0x3c;

/// The 0x01000077 reply: 0x30 bytes, result 1, the 64-bit address at +0x34.
pub fn answer_user_shared(buf: &mut [u8], address: u64) {
    put32(buf, NVDA_SIZE_AT, NVDA_COMMON_HEADER_BYTES as u32);
    put32(buf, NVDA_RESULT_AT, 1);
    put32(buf, NVDA_RESULT2_AT, 1);
    put32(buf, 0x30, 0);
    put32(buf, 0x34, address as u32);
    put32(buf, 0x38, (address >> 32) as u32);
}

/// The second result word of a `0x0200000c` head query for a head without a display.
pub const HEAD_ABSENT_RESULT2: u32 = 0xffff_fff2;

pub const NV906F_CTRL_CMD_GET_CLASS_ENGINEID: u32 = 0x906f_0101;
pub const NV2080_ENGINE_TYPE_GRAPHICS: u32 = 1;
/// Pseudo handles of the class objects on a graphics channel, as the Vulkan ICD
/// names them in GET_CLASS_ENGINEID, and their classes (610.88 capture, RTX 5090).
pub const CHANNEL_CLASS_OBJECTS: [(u32, u32); 3] = [
    (0xff1f_e026, 0xce97), // BLACKWELL_B (3D)
    (0xff1f_e02c, 0x902d), // FERMI_TWOD_A
    (0xff1f_e01a, 0xcd40), // BLACKWELL_INLINE_TO_MEMORY_A
];

/// The class of a channel class-object pseudo handle, `None` if it is not one.
pub fn class_engine_of(h_object: u32) -> Option<u32> {
    CHANNEL_CLASS_OBJECTS
        .iter()
        .find(|(handle, _)| *handle == h_object)
        .map(|(_, class)| *class)
}

/// NV04_DISPLAY_COMMON (class 0x73) in UMD RM controls.
pub const PSEUDO_DISPLAY_COMMON: u32 = 0xff0a_0000;
pub const NV0073_CTRL_CMD_SYSTEM_GET_CAPS: u32 = 0x0073_0101;
pub const NV0073_CTRL_CMD_SYSTEM_GET_NUM_HEADS: u32 = 0x0073_0102;
pub const NV0073_CTRL_CMD_SYSTEM_GET_ACTIVE: u32 = 0x0073_010c;
pub const NV0073_CTRL_CMD_SYSTEM_GET_RASTER: u32 = 0x0073_011a;
const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
const NV_ERR_INVALID_ADDRESS: u32 = 0x1e;

/// The second result word NVAPI's display queries carry in (-23) and keep when the
/// display is not one the KMD can name.
pub const DISPLAY_UNKNOWN_RESULT2: u32 = 0xffff_ffe9;
/// NVAPI display ids of the RTX 5090's outputs: 0x80061080 | n.
pub const NVAPI_DISPLAY_ID_BASE: u32 = 0x8006_1080;

/// The output index k of `0x070000f1` / `0x07000113` from an NVAPI display id
/// (0x8006108n: k = n ^ 3) or a one-bit display mask (bit 8 + k), as in every
/// captured reply (ids 0x80..0x87 -> k 3,2,1,0,(7: 0x84 not captured),6,5,4; masks 0x100 -> 0,
/// 0x2000 -> 5).
pub fn display_index(display: u32) -> Option<u32> {
    if display & !7 == NVAPI_DISPLAY_ID_BASE {
        return Some((display & 7) ^ 3);
    }
    if display.is_power_of_two() {
        let bit = display.trailing_zeros();
        if (8..16).contains(&bit) {
            return Some(bit - 8);
        }
    }
    None
}

/// The NVAPI display id of output index `k` (the inverse of [`display_index`]).
pub const fn display_id_of(k: u32) -> u32 {
    NVAPI_DISPLAY_ID_BASE | ((k & 7) ^ 3)
}
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1f;

pub const NV2080_CTRL_CMD_GPU_QUERY_ECC_STATUS: u32 = 0x2080_012f;
/// The second result word NVIDIA's KMD answers with the refused ECC query (-19).
pub const ECC_RESULT2: u32 = 0xffff_ffed;

/// NV0080 NVENC_GET_CAPS: `{NvU32 capsTblSize; NvP64 capsTbl}` (0x10 bytes), the
/// table being a pointer into the caller's address space. RM on a Linux host has no
/// such control for a physical GPU (and could not reach a guest pointer anyway: it
/// answered NV_ERR_INVALID_ADDRESS); NVIDIA's KMD answers it itself (vktest capture
/// seq 1395: RM status 0, the 0x54-byte reply size kept).
pub const NV0080_CTRL_CMD_NVENC_GET_CAPS: u32 = 0x0080_1b01;
pub const NVENC_CAPS_TBL_SIZE: u32 = 6;

/// An NVENC_GET_CAPS request: where the caller wants its table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvencCaps {
    pub table: u64,
    pub size: u32,
}

/// `Some` when `buf` is an NVENC_GET_CAPS control with its 0x10-byte parameters.
pub fn nvenc_caps_request(buf: &[u8]) -> Option<NvencCaps> {
    let control = parse_rm_control(buf).ok()?;
    if control.cmd != NV0080_CTRL_CMD_NVENC_GET_CAPS || control.params.len() != 0x10 {
        return None;
    }
    let size = rc::get32(control.params, 0)?;
    let lo = rc::get32(control.params, 8)?;
    let hi = rc::get32(control.params, 12)?;
    Some(NvencCaps {
        table: u64::from(hi) << 32 | u64::from(lo),
        size,
    })
}

/// How the KMD's attempt to fill the caller's NVENC caps table went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvencCapsWrite {
    /// The table was written.
    Written,
    /// `capsTblSize` was not the table's size: nothing written.
    BadSize,
    /// The table pointer was null or not writable.
    BadAddress,
}

/// The table the KMD writes: no NVENC capability or workaround bits. (The capture
/// does not hold NVIDIA's bytes, which live in the caller's memory; the Vulkan ICD
/// reads only bit 0 of bytes 2 and 3.)
pub const NVENC_CAPS_TABLE: [u8; NVENC_CAPS_TBL_SIZE as usize] = [0; 6];

/// Whether the caller's table should be written for this request (size check).
pub fn nvenc_caps_writable(request: NvencCaps) -> Result<(), NvencCapsWrite> {
    if request.size != NVENC_CAPS_TBL_SIZE {
        return Err(NvencCapsWrite::BadSize);
    }
    if request.table == 0 {
        return Err(NvencCapsWrite::BadAddress);
    }
    Ok(())
}

/// The NVENC_GET_CAPS reply: result 1 in both result words, the size kept, RM
/// status 0 when the table was written, else what RM answers for that case.
pub fn answer_nvenc_caps(buf: &mut [u8], outcome: NvencCapsWrite) {
    put32(buf, NVDA_RESULT_AT, 1);
    put32(buf, NVDA_RESULT2_AT, 1);
    let status = match outcome {
        NvencCapsWrite::Written => 0,
        NvencCapsWrite::BadSize => NV_ERR_INVALID_ARGUMENT,
        NvencCapsWrite::BadAddress => NV_ERR_INVALID_ADDRESS,
    };
    put32(buf, RM_STATUS_AT, status);
}

/// `0x0100000d`: the request (and reply) size.
pub const DISPLAY_TIMING_BYTES: usize = 0xfc0;

/// The `0x0100000d` reply words that do not depend on the mode (vktest capture seq
/// 3783, a 1024x768 DMT display on the RTX 5090): +0x40/+0x44 0x100, +0x48 0x61,
/// +0x50 1, +0x5c 32 bpp, +0x68 format 0x15 (A8R8G8B8), four 1000000 words, +0x98 1,
/// +0xe8 0x00020001, +0xf0 0x110, +0xfb0/+0xfb4 1.
pub const DISPLAY_TIMING_FIXED: [(usize, u32); 15] = [
    (0x40, 0x100),
    (0x44, 0x100),
    (0x48, 0x61),
    (0x50, 1),
    (0x5c, 32),
    (0x68, 0x15),
    (0x80, 1_000_000),
    (0x84, 1_000_000),
    (0x90, 1_000_000),
    (0x94, 1_000_000),
    (0x98, 1),
    (0xe8, 0x0002_0001),
    (0xf0, 0x110),
    (0xfb0, 1),
    (0xfb4, 1),
];

/// Whether `buf` is a `0x0100000d` display-timing query (the KMD fills the mode in).
pub fn is_display_timing(buf: &[u8]) -> bool {
    rc::get32(buf, NVDA_FUNCTION_AT) == Some(0x0100_000d) && buf.len() == DISPLAY_TIMING_BYTES
}

/// A reduced-blanking raster for `width` x `height` at `refresh_mhz` (CVT-RB v1
/// porches: 48/32/80 pixels, 3 lines front porch, 6 lines sync, at least 460 us of
/// vertical blanking). The capture's 1024x768 DMT mode used DMT's own timing; NVIDIA
/// reports what the display's EDID gives, which the guest does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raster {
    pub h_front: u32,
    pub h_sync: u32,
    pub h_total: u32,
    pub v_front: u32,
    pub v_sync: u32,
    pub v_total: u32,
    pub pclk_khz: u32,
}

pub fn reduced_blanking(width: u32, height: u32, refresh_mhz: u32) -> Raster {
    let hz_milli = u64::from(refresh_mhz.clamp(1_000, 1_000_000));
    let h_total = width.saturating_add(160);
    // v_total * (1 - 460 us * hz) >= height, in micro-units.
    let keep = 1_000_000u64.saturating_sub(460 * hz_milli / 1000).max(100_000);
    let v_total = ((u64::from(height) * 1_000_000).div_ceil(keep) as u32).max(height + 3 + 6 + 6);
    let pclk_khz = (u64::from(h_total) * u64::from(v_total) * hz_milli / 1_000_000) as u32;
    Raster {
        h_front: 48,
        h_sync: 32,
        h_total,
        v_front: 3,
        v_sync: 6,
        v_total,
        pclk_khz,
    }
}

/// The mode in a `0x0100000d` reply: +0x54/+0x58 size, +0x60 Hz, +0x64 mHz, the
/// raster at +0xb0 (16-bit fields: visible, border, front porch, sync, total,
/// polarity; horizontal then vertical), +0xcc pixel clock in 10 kHz, +0xd0 in kHz,
/// +0xdc Hz, +0xe0 mHz, and the name at +0xf4 (`DMT:1024x768x60Hz` in the capture).
pub fn set_display_timing(buf: &mut [u8], width: u32, height: u32, refresh_mhz: u32) {
    let hz = (refresh_mhz + 500) / 1000;
    let r = reduced_blanking(width, height, refresh_mhz);
    let pair = |lo: u32, hi: u32| (lo & 0xffff) | (hi & 0xffff) << 16;
    put32(buf, 0x54, width);
    put32(buf, 0x58, height);
    put32(buf, 0x60, hz);
    put32(buf, 0x64, refresh_mhz);
    put32(buf, 0xb0, pair(width, 0));
    put32(buf, 0xb4, pair(r.h_front, r.h_sync));
    put32(buf, 0xb8, pair(r.h_total, 1));
    put32(buf, 0xbc, pair(height, 0));
    put32(buf, 0xc0, pair(r.v_front, r.v_sync));
    put32(buf, 0xc4, pair(r.v_total, 1));
    put32(buf, 0xcc, r.pclk_khz / 10);
    put32(buf, 0xd0, r.pclk_khz);
    put32(buf, 0xdc, hz);
    put32(buf, 0xe0, refresh_mhz);
    let mut name = [0u8; 0x20];
    name[..4].copy_from_slice(b"CVT:");
    let mut n = 4;
    for (i, value) in [width, height, hz].into_iter().enumerate() {
        n += write_decimal(&mut name[n..], value);
        let sep: &[u8] = if i < 2 { b"x" } else { b"Hz" };
        name[n..n + sep.len()].copy_from_slice(sep);
        n += sep.len();
    }
    if let Some(dst) = buf.get_mut(0xf4..0xf4 + name.len()) {
        dst.copy_from_slice(&name);
    }
}

/// `value` in decimal at the start of `out` (at most 7 digits); the digits written.
fn write_decimal(out: &mut [u8], value: u32) -> usize {
    let value = value.min(9_999_999);
    let mut digits = [0u8; 7];
    let mut len = 0;
    let mut v = value;
    loop {
        digits[len] = b'0' + (v % 10) as u8;
        len += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in 0..len {
        out[i] = digits[len - 1 - i];
    }
    len
}

/// The raster in an NV0073 0x73011a reply (params at +0x44): display id +0, width +4,
/// height +8, totals +0xc/+0x10 (the capture's 1344x806 for 1024x768), refresh +0x18.
pub fn set_raster(buf: &mut [u8], width: u32, height: u32, refresh_hz: u32) {
    let p = RM_PARAMS_AT;
    put32(buf, p + 0x04, width);
    put32(buf, p + 0x08, height);
    put32(buf, p + 0x0c, width / 16 * 21);
    put32(buf, p + 0x10, height + height / 20);
    put32(buf, p + 0x18, refresh_hz);
}

/// Whether `buf` is an NV0073 raster query on the display-common object.
pub fn is_raster_query(buf: &[u8]) -> bool {
    parse_rm_control(buf).is_ok_and(|c| {
        c.h_object == PSEUDO_DISPLAY_COMMON && c.cmd == NV0073_CTRL_CMD_SYSTEM_GET_RASTER
    })
}

/// The mode in a `0x010000a7` display description.
pub fn set_display_info_mode(buf: &mut [u8], width: u32, height: u32) {
    put32(buf, 0x40, width);
    put32(buf, 0x44, height);
    put32(buf, 0xcc, width);
    put32(buf, 0xd0, height);
}

/// Whether `buf` is a `0x010000a7` display description.
pub fn is_display_info(buf: &[u8]) -> bool {
    rc::get32(buf, NVDA_FUNCTION_AT) == Some(0x0100_00a7)
}

/// Head 0's mode in a `0x0200000c` reply: width +0x40, height +0x44, refresh +0x4c.
pub fn set_head_mode(buf: &mut [u8], width: u32, height: u32, refresh_hz: u32) {
    put32(buf, 0x40, width);
    put32(buf, 0x44, height);
    put32(buf, 0x4c, refresh_hz);
}

/// Whether `buf` is a `0x0200000c` reply for head 0 (the KMD fills the real mode in).
pub fn is_head0_query(buf: &[u8]) -> bool {
    rc::get32(buf, NVDA_FUNCTION_AT) == Some(0x0200_000c) && rc::get32(buf, 0x34) == Some(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn destroyed_memory_is_no_longer_nameable_and_frees_its_slot() {
        let mut objects = [0u32; 64];
        let mut len = 0u8;
        assert!(note_object(&mut objects, &mut len, 0x4e57_1000));
        assert!(note_object(&mut objects, &mut len, 0x4e57_1001));
        assert!(!note_object(&mut objects, &mut len, 0x4e57_1000));
        assert!(!note_object(&mut objects, &mut len, 0));
        assert!(forget_object(&mut objects, &mut len, 0x4e57_1000));
        assert!(!forget_object(&mut objects, &mut len, 0x4e57_1000));
        assert_eq!((len, objects[0], objects[1]), (1, 0x4e57_1001, 0));
        let map = ClientMap {
            root: 1,
            device: 2,
            subdevice: 3,
            objects,
            objects_len: len,
        };
        assert_eq!(translate_handles(0, 0x4e57_1000, &map), Err(HandleError::Object(0x4e57_1000)));
        assert_eq!(translate_handles(0, 0x4e57_1001, &map), Ok((1, 0x4e57_1001)));
        // A full list takes a new object again once one was forgotten.
        for i in 0..63 {
            assert!(note_object(&mut objects, &mut len, 0x100 + i));
        }
        assert!(!note_object(&mut objects, &mut len, 0x999));
        assert!(forget_object(&mut objects, &mut len, 0x100));
        assert!(note_object(&mut objects, &mut len, 0x999));
        assert!(!forget_object(&mut objects, &mut len, 0));
    }

    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    const MAP: ClientMap = ClientMap {
        root: 0xc100_0001,
        device: H_DEVICE,
        subdevice: H_SUBDEVICE,
        objects: [0; 64],
        objects_len: 0,
    };

    fn test_put32(buf: &mut [u8], at: usize, value: u32) {
        buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn escape_with_handles(cmd: u32, client: u32, object: u32, params: &[u8]) -> Vec<u8> {
        let mut buf = vec![0xa5; NVDA_HEADER_BYTES + params.len()];
        test_put32(&mut buf, 0, NVDA_MAGIC);
        test_put32(&mut buf, 4, NVDA_VERSION);
        let size = buf.len() as u32;
        test_put32(&mut buf, NVDA_SIZE_AT, size);
        test_put32(&mut buf, NVDA_TAG_AT, 0x1234_5678);
        test_put32(
            &mut buf,
            NVDA_FUNCTION_AT,
            0x8100_0000 | RM_CONTROL_FUNCTION,
        );
        test_put32(&mut buf, RM_H_CLIENT_AT, client);
        test_put32(&mut buf, RM_H_OBJECT_AT, object);
        test_put32(&mut buf, RM_CMD_AT, cmd);
        test_put32(&mut buf, RM_PARAMS_SIZE_AT, params.len() as u32);
        test_put32(&mut buf, RM_STATUS_AT, 0xdead_beef);
        buf[RM_PARAMS_AT..].copy_from_slice(params);
        buf
    }

    fn escape(cmd: u32, params: &[u8]) -> Vec<u8> {
        escape_with_handles(cmd, 0, PSEUDO_DEVICE, params)
    }

    fn host_reply(rm_status: u32, params: &[u8]) -> Vec<u8> {
        let mut resp = vec![0u8; rc::REPLY_DATA + rc::NVOS54_BYTES + params.len()];
        test_put32(&mut resp, 0, rc::MSG_IOCTL);
        test_put32(&mut resp, 4, 0x1122_3344);
        test_put32(&mut resp, rc::MSG_HDR, rc::NVOS54_BYTES as u32);
        test_put32(&mut resp, rc::MSG_HDR + 4, params.len() as u32);
        let data = rc::REPLY_DATA;
        test_put32(&mut resp, data, MAP.root);
        test_put32(&mut resp, data + 4, MAP.device);
        test_put32(&mut resp, data + 8, 0x906f_0101);
        test_put32(&mut resp, data + 24, params.len() as u32);
        test_put32(&mut resp, data + rc::NVOS54_STATUS_AT, rm_status);
        resp[data + rc::NVOS54_BYTES..].copy_from_slice(params);
        resp
    }

    #[test]
    fn capture_shaped_controls_have_exact_parameter_lengths() {
        let params16 = [0x5a; 16];
        let buf = escape(0x906f_0101, &params16);
        assert_eq!(buf.len(), 84);
        let control = parse_rm_control(&buf).unwrap();
        assert_eq!(control.header.tag, 0x1234_5678);
        assert_eq!(control.cmd, 0x906f_0101);
        assert_eq!(control.params, params16);
        assert_eq!(control.status, 0xdead_beef);

        let params1 = [0x7b];
        let buf = escape(0x0080_1909, &params1);
        assert_eq!(buf.len(), 69);
        let control = parse_rm_control(&buf).unwrap();
        assert_eq!(control.cmd, 0x0080_1909);
        assert_eq!(control.params, params1);
    }

    #[test]
    fn header_validates_magic_version_and_exact_capped_size() {
        let buf = escape(0x906f_0101, &[0; 16]);
        assert!(parse_header(&buf).is_ok());

        let mut bad = buf.clone();
        test_put32(&mut bad, 0, NVDA_MAGIC ^ 1);
        assert_eq!(parse_header(&bad), Err(HeaderError::BadMagic));
        test_put32(&mut bad, 0, NVDA_MAGIC);
        test_put32(&mut bad, 4, NVDA_VERSION + 1);
        assert_eq!(parse_header(&bad), Err(HeaderError::BadVersion));

        for size in [0, 1, NVDA_HEADER_BYTES as u32 - 1, 83, 85, u32::MAX] {
            let mut bad = buf.clone();
            test_put32(&mut bad, NVDA_SIZE_AT, size);
            let expected = if size as usize > NVDA_MAX_BYTES {
                HeaderError::TooLarge
            } else {
                HeaderError::BadSize
            };
            assert_eq!(parse_header(&bad), Err(expected), "size {size:#x}");
        }

        let mut trailing = buf.clone();
        trailing.push(0);
        assert_eq!(parse_header(&trailing), Err(HeaderError::BadSize));

        let mut too_large = vec![0u8; NVDA_MAX_BYTES + 1];
        test_put32(&mut too_large, 0, NVDA_MAGIC);
        test_put32(&mut too_large, 4, NVDA_VERSION);
        test_put32(&mut too_large, NVDA_SIZE_AT, (NVDA_MAX_BYTES + 1) as u32);
        assert_eq!(parse_header(&too_large), Err(HeaderError::TooLarge));
    }

    #[test]
    fn rm_function_uses_only_the_low_24_bits() {
        assert!(is_rm_control_function(0x2b));
        assert!(is_rm_control_function(0xff00_002b));
        assert!(!is_rm_control_function(0x2c));
        assert!(!is_rm_control_function(0x0100_002c));

        let mut buf = escape(0x906f_0101, &[]);
        test_put32(&mut buf, NVDA_FUNCTION_AT, 0x2c);
        assert_eq!(parse_rm_control(&buf), Err(ControlError::NotRmControl));
    }

    #[test]
    fn control_rejects_every_parameter_size_mismatch() {
        let buf = escape(0x906f_0101, &[0; 16]);
        for size in [0, 1, 15, 17, 84, u32::MAX] {
            let mut bad = buf.clone();
            test_put32(&mut bad, RM_PARAMS_SIZE_AT, size);
            assert_eq!(parse_rm_control(&bad), Err(ControlError::ParamsSize));
        }
    }

    #[test]
    fn every_known_pseudo_handle_maps_to_its_own_host_object() {
        assert_eq!(translate_handles(0, 0, &MAP), Ok((MAP.root, MAP.root)));
        assert_eq!(
            translate_handles(PSEUDO_CLIENT_CUDA, PSEUDO_DEVICE, &MAP),
            Ok((MAP.root, MAP.device))
        );
        assert_eq!(
            translate_handles(0, PSEUDO_DEVICE_CUDA, &MAP),
            Ok((MAP.root, MAP.device))
        );
        assert_eq!(
            translate_handles(0, PSEUDO_SUBDEVICE, &MAP),
            Ok((MAP.root, MAP.subdevice))
        );
        assert_eq!(
            translate_handles(PSEUDO_CLIENT_CUDA, 0, &MAP),
            Ok((MAP.root, MAP.root))
        );
        assert_eq!(
            translate_handles(MAP.root, MAP.device, &MAP),
            Ok((MAP.root, MAP.device))
        );
        assert_eq!(
            translate_handles(MAP.root, MAP.subdevice, &MAP),
            Ok((MAP.root, MAP.subdevice))
        );
        let memory = 0x4e57_1234;
        let mut with_memory = MAP;
        with_memory.objects[0] = memory;
        with_memory.objects_len = 1;
        assert_eq!(
            translate_handles(0, memory, &with_memory),
            Ok((MAP.root, memory))
        );
    }

    #[test]
    fn unknown_clients_and_objects_fail_separately() {
        for client in [1, PSEUDO_DEVICE, 0xff02_8000, 0xff04_0000, 0xff1f_0000] {
            assert_eq!(
                translate_handles(client, PSEUDO_DEVICE, &MAP),
                Err(HandleError::Client(client))
            );
        }
        for object in [1, PSEUDO_CLIENT_CUDA, 0xff03_0010, 0xff04_0000, 0xff1f_0000] {
            assert_eq!(
                translate_handles(0, object, &MAP),
                Err(HandleError::Object(object))
            );
        }
        assert_eq!(
            translate_handles(0x1234, 0x5678, &MAP),
            Err(HandleError::Client(0x1234))
        );
    }

    #[test]
    fn builder_emits_nvos54_and_flat_params_with_cuda_aliases() {
        let params: Vec<u8> = (0..16).collect();
        let buf = escape_with_handles(0x906f_0101, PSEUDO_CLIENT_CUDA, PSEUDO_DEVICE_CUDA, &params);
        let control = parse_rm_control(&buf).unwrap();
        let mut out = [0xa5; 128];
        let n = build_host_control(&mut out, 0x1122_3344, &control, &MAP).unwrap();
        assert_eq!(n, rc::MSG_HDR + rc::IOCTL_REQ + rc::NVOS54_BYTES + 16);
        assert_eq!(rc::get32(&out, 0), Some(rc::MSG_IOCTL));
        assert_eq!(rc::get32(&out, 4), Some(0x1122_3344));
        assert_eq!(
            rc::get32(&out, rc::MSG_HDR),
            Some(rc::nv_cmd(rc::ESC_RM_CONTROL, 32))
        );
        assert_eq!(rc::get32(&out, rc::MSG_HDR + 4), Some(32));
        assert_eq!(rc::get32(&out, rc::MSG_HDR + 8), Some(32));
        assert_eq!(rc::get32(&out, rc::MSG_HDR + 12), Some(16));
        assert_eq!(rc::get32(&out, rc::MSG_HDR + 16), Some(0));
        assert_eq!(rc::get32(&out, rc::MSG_HDR + 20), Some(0));
        let data = rc::MSG_HDR + rc::IOCTL_REQ;
        assert_eq!(rc::get32(&out, data), Some(MAP.root));
        assert_eq!(rc::get32(&out, data + 4), Some(MAP.device));
        assert_eq!(rc::get32(&out, data + 8), Some(0x906f_0101));
        assert_eq!(rc::get32(&out, data + 24), Some(16));
        assert_eq!(&out[data + 32..n], params);
        assert!(out[n..].iter().all(|b| *b == 0xa5));
    }

    #[test]
    fn builder_refuses_small_outputs_and_untranslated_handles() {
        let buf = escape_with_handles(0x0080_1909, 0, PSEUDO_SUBDEVICE, &[7]);
        let control = parse_rm_control(&buf).unwrap();
        let needed = rc::MSG_HDR + rc::IOCTL_REQ + rc::NVOS54_BYTES + 1;
        let mut out = [0xa5; 128];
        for n in 0..needed {
            assert_eq!(
                build_host_control(&mut out[..n], 9, &control, &MAP),
                Err(BuildError::OutputTooSmall)
            );
        }

        for (client, object, expected) in [
            (0x1234, PSEUDO_DEVICE, HandleError::Client(0x1234)),
            (0, 0xff04_0000, HandleError::Object(0xff04_0000)),
            (0, 0xff1f_0000, HandleError::Object(0xff1f_0000)),
        ] {
            let bad = escape_with_handles(0x0080_1909, client, object, &[7]);
            let bad = parse_rm_control(&bad).unwrap();
            let mut untouched = [0x6c; 128];
            assert_eq!(
                build_host_control(&mut untouched, 9, &bad, &MAP),
                Err(BuildError::Handle(expected))
            );
            assert!(untouched.iter().all(|b| *b == 0x6c));
        }
    }

    #[test]
    fn reply_preserves_nonzero_rm_status_and_params() {
        let params = [1, 3, 3, 7];
        let resp = host_reply(0x0000_0056, &params);
        let reply = parse_host_reply(&resp, params.len()).unwrap();
        assert_eq!(reply.rm_status, 0x56);
        assert_eq!(reply.params, params);
    }

    #[test]
    fn reply_distinguishes_host_refusal_from_malformed_success() {
        let mut refusal = [0u8; 16];
        test_put32(&mut refusal, 8, (-22i32) as u32);
        assert_eq!(
            parse_host_reply(&refusal, 0),
            Err(HostReplyError::Host(-22))
        );

        assert_eq!(parse_host_reply(&[], 0), Err(HostReplyError::Malformed));
        let valid = host_reply(0, &[1, 2, 3, 4]);
        for n in 0..valid.len() {
            assert!(parse_host_reply(&valid[..n], 4).is_err());
        }
        for size in [0, 1, 3, 5, usize::MAX] {
            assert_eq!(
                parse_host_reply(&valid, size),
                Err(HostReplyError::Malformed)
            );
        }

        for data_len in [0, 31, 33, u32::MAX] {
            let mut bad = valid.clone();
            test_put32(&mut bad, rc::MSG_HDR, data_len);
            assert_eq!(parse_host_reply(&bad, 4), Err(HostReplyError::Malformed));
        }
        let mut bad = valid;
        test_put32(&mut bad, rc::MSG_HDR + 4, u32::MAX);
        assert_eq!(parse_host_reply(&bad, 4), Err(HostReplyError::Malformed));
    }

    #[test]
    fn authenticated_reply_rejects_every_request_mismatch() {
        let control_buf = escape(0x906f_0101, &[0x44; 16]);
        let control = parse_rm_control(&control_buf).unwrap();
        let valid = host_reply(0, &[0x55; 16]);
        assert!(parse_host_reply_for(&valid, 0x1122_3344, &control, &MAP).is_ok());
        for (at, value) in [
            (0, rc::MSG_OPEN),
            (4, 0x1122_3345),
            (rc::MSG_HDR + 8, 1),
            (rc::REPLY_DATA, 1),
            (rc::REPLY_DATA + 4, 1),
            (rc::REPLY_DATA + 8, 1),
            (rc::REPLY_DATA + 24, 15),
        ] {
            let mut bad = valid.clone();
            test_put32(&mut bad, at, value);
            assert_eq!(
                parse_host_reply_for(&bad, 0x1122_3344, &control, &MAP),
                Err(HostReplyError::Malformed),
                "offset {at:#x}"
            );
        }
        for len in [0, valid.len() - 1, valid.len() + 1] {
            let mut bad = valid.clone();
            bad.resize(len, 0);
            assert_eq!(
                parse_host_reply_for(&bad, 0x1122_3344, &control, &MAP),
                Err(HostReplyError::Malformed),
                "length {len}"
            );
        }
    }

    #[test]
    fn apply_changes_only_results_status_and_params() {
        let original = escape(0x906f_0101, &[0x11; 16]);
        let new_params = [0xe7; 16];
        let resp = host_reply(0x1234_5678, &new_params);
        let reply = parse_host_reply(&resp, 16).unwrap();
        let mut actual = original.clone();
        apply_host_reply(&mut actual, &reply).unwrap();

        assert_eq!(rc::get32(&actual, NVDA_RESULT_AT), Some(1));
        assert_eq!(rc::get32(&actual, NVDA_RESULT2_AT), Some(1));
        assert_eq!(rc::get32(&actual, RM_STATUS_AT), Some(0x1234_5678));
        assert_eq!(&actual[RM_PARAMS_AT..], new_params);
        assert_eq!(rc::get32(&actual, NVDA_SIZE_AT), Some(84));
        assert_eq!(actual.len(), original.len());
        for i in 0..original.len() {
            let changed = (NVDA_RESULT_AT..NVDA_RESULT_AT + 4).contains(&i)
                || (NVDA_RESULT2_AT..NVDA_RESULT2_AT + 4).contains(&i)
                || (RM_STATUS_AT..RM_STATUS_AT + 4).contains(&i)
                || i >= RM_PARAMS_AT;
            if !changed {
                assert_eq!(actual[i], original[i], "byte {i}");
            }
        }
    }

    #[test]
    fn apply_validates_before_mutating() {
        let mut bad_size = escape(0x906f_0101, &[0; 16]);
        let before = bad_size.clone();
        test_put32(&mut bad_size, RM_PARAMS_SIZE_AT, 15);
        let reply = HostReply {
            rm_status: 9,
            params: &[1; 16],
        };
        assert_eq!(
            apply_host_reply(&mut bad_size, &reply),
            Err(ControlError::ParamsSize)
        );
        // Only the deliberately damaged size word differs from the original.
        assert_eq!(&bad_size[..RM_PARAMS_SIZE_AT], &before[..RM_PARAMS_SIZE_AT]);
        assert_eq!(&bad_size[RM_PARAMS_AT..], &before[RM_PARAMS_AT..]);

        let mut mismatch = before;
        let snapshot = mismatch.clone();
        let short_reply = HostReply {
            rm_status: 9,
            params: &[1; 15],
        };
        assert_eq!(
            apply_host_reply(&mut mismatch, &short_reply),
            Err(ControlError::ParamsSize)
        );
        assert_eq!(mismatch, snapshot);
    }

    fn local_request(function: u32, len: usize) -> Vec<u8> {
        let mut buf = vec![0xa5; len];
        test_put32(&mut buf, 0, NVDA_MAGIC);
        test_put32(&mut buf, 4, NVDA_VERSION);
        test_put32(&mut buf, NVDA_SIZE_AT, len as u32);
        test_put32(&mut buf, NVDA_TAG_AT, 0x4e56_2a2a);
        test_put32(&mut buf, NVDA_FUNCTION_AT, function);
        buf
    }

    fn check_local_reply(
        function: u32,
        request_len: usize,
        device: usize,
        context: usize,
        changes: &[(usize, u32)],
        byte_changes: &[(usize, u8)],
    ) {
        let original = local_request(function, request_len);
        let mut expected = original.clone();
        for &(at, value) in changes {
            test_put32(&mut expected, at, value);
        }
        for &(at, value) in byte_changes {
            expected[at] = value;
        }
        let mut actual = original;
        assert_eq!(
            apply_local(&mut actual, device, context),
            Ok(LocalOutcome::Applied)
        );
        assert_eq!(actual, expected, "function {function:#010x}");
    }

    #[test]
    fn local_replies_match_every_capture_offset_and_preserve_every_other_byte() {
        check_local_reply(
            0x0700_0002,
            0x238,
            0,
            0,
            &[
                (NVDA_RESULT_AT, 1),
                (NVDA_RESULT2_AT, 1),
                (0x34, 1),
                (0x38, 0x100),
                (0x138, 1),
            ],
            &[],
        );
        check_local_reply(
            0x0700_0003,
            0x238,
            0,
            0,
            &[(NVDA_RESULT_AT, 1), (NVDA_RESULT2_AT, 1)],
            &[],
        );
        check_local_reply(
            0x0700_00ad,
            0x3c,
            0,
            0,
            &[
                (NVDA_RESULT_AT, 1),
                (0x28, 0),
                (NVDA_RESULT2_AT, 1),
                (0x34, 0x3200),
                (0x38, 1),
            ],
            &[],
        );
        check_local_reply(
            0x0700_0016,
            0x38,
            0,
            0,
            &[(NVDA_RESULT_AT, 1), (NVDA_RESULT2_AT, 1), (0x34, 0x100)],
            &[],
        );
        check_local_reply(
            0x0100_0113,
            0x45,
            0x1234,
            0,
            &[
                (NVDA_SIZE_AT, 0x30),
                (NVDA_RESULT_AT, 1),
                (0x30, 0x2000_0001),
                (0x34, 0),
                (0x38, 0x20),
                (0x3c, 0xffff_ff00),
            ],
            &[(0x40, 0xff)],
        );
        check_local_reply(
            0x0200_001e,
            0x38,
            0x1234,
            0x5678,
            &[
                (NVDA_SIZE_AT, 0x30),
                (NVDA_RESULT_AT, 1),
                (NVDA_RESULT2_AT, 1),
            ],
            &[],
        );
        check_local_reply(
            0x0100_0140,
            0x36,
            0x1234,
            0x5678,
            &[
                (NVDA_SIZE_AT, 0x30),
                (NVDA_RESULT_AT, 1),
                (NVDA_RESULT2_AT, 1),
            ],
            &[],
        );
    }

    #[test]
    fn shortened_replies_keep_the_original_allocation_and_trailing_bytes() {
        for (function, len) in [
            (0x0100_0113, 0x45),
            (0x0200_001e, 0x38),
            (0x0100_0140, 0x36),
        ] {
            let mut buf = local_request(function, len);
            let original = buf.clone();
            assert_eq!(apply_local(&mut buf, 1, 1), Ok(LocalOutcome::Applied));
            assert_eq!(buf.len(), len);
            assert_eq!(rc::get32(&buf, NVDA_SIZE_AT), Some(0x30));
            assert!(buf.len() > NVDA_COMMON_HEADER_BYTES);
            for at in 0x30..len {
                let changed = match function {
                    0x0100_0113 => (0x30..=0x40).contains(&at),
                    _ => false,
                };
                if !changed {
                    assert_eq!(
                        buf[at], original[at],
                        "function {function:#x}, byte {at:#x}"
                    );
                }
            }
            // A consumer must use the original slice length. Reparsing the
            // shortened declaration as the allocation length is deliberately
            // invalid and is not part of the local reply path.
            assert_eq!(parse_header(&buf), Err(HeaderError::BadSize));
            let declared_only = &buf[..NVDA_COMMON_HEADER_BYTES];
            assert!(parse_header(declared_only).is_ok());
        }
    }

    #[test]
    fn local_requests_require_exact_sizes_and_required_scopes_without_mutation() {
        for (function, len) in [
            (0x0700_0002, 0x238),
            (0x0700_0003, 0x238),
            (0x0700_00ad, 0x3c),
            (0x0700_0016, 0x38),
            (0x0100_0113, 0x45),
            (0x0200_001e, 0x38),
            (0x0100_0140, 0x36),
        ] {
            for bad_len in [len - 1, len + 1] {
                let mut bad = local_request(function, bad_len);
                let snapshot = bad.clone();
                assert_eq!(apply_local(&mut bad, 1, 1), Err(LocalError::Invalid));
                assert_eq!(bad, snapshot);
            }
        }
        for (function, len) in [(0x0200_001e, 0x38), (0x0100_0140, 0x36)] {
            for (device, context) in [(0, 0), (0, 1), (1, 0)] {
                let mut bad = local_request(function, len);
                let snapshot = bad.clone();
                assert_eq!(
                    apply_local(&mut bad, device, context),
                    Err(LocalError::Invalid)
                );
                assert_eq!(bad, snapshot);
            }
        }
        let mut no_device = local_request(0x0100_0113, 0x45);
        let snapshot = no_device.clone();
        assert_eq!(apply_local(&mut no_device, 0, 1), Err(LocalError::Invalid));
        assert_eq!(no_device, snapshot);
    }

    #[test]
    fn query_0152_matches_the_capture() {
        let mut buf = local_request(0x0100_0152, 0x3e);
        buf[0x20..0x24].copy_from_slice(&0x7e08_f710u32.to_le_bytes());
        let before = buf.clone();
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &0xffff_ffe9u32.to_le_bytes());
        for at in (0..buf.len()).filter(|a| !(0x08..0x0c).contains(a) && !(0x14..0x18).contains(a) && !(0x2c..0x30).contains(a)) {
            assert_eq!(buf[at], before[at], "byte {at:#x}");
        }
        let mut short = local_request(0x0100_0152, 0x3d);
        assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
    }

    #[test]
    fn query_006b_matches_the_capture() {
        let mut buf = local_request(0x0100_006b, 0x268);
        buf[0x30..0x34].copy_from_slice(&4u32.to_le_bytes());
        buf[0x38..0x3c].copy_from_slice(&0x3264u32.to_le_bytes());
        let before = buf.clone();
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        for at in (0..buf.len()).filter(|a| !(0x08..0x0c).contains(a) && !(0x14..0x18).contains(a) && !(0x2c..0x30).contains(a)) {
            assert_eq!(buf[at], before[at], "byte {at:#x}");
        }
        let mut short = local_request(0x0100_006b, 0x267);
        assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
    }

    #[test]
    fn lg_sector_promotion_is_answered_as_captured() {
        // 610.88 D3D11: fn 0x0500002b, hObject 0xff040000, cmd 0x0090010b, 4 bytes = 1.
        let mut buf = local_request(0x0500_002b, NVDA_HEADER_BYTES + 4);
        test_put32(&mut buf, RM_H_OBJECT_AT, PSEUDO_GRAPHICS_CONTEXT);
        test_put32(&mut buf, RM_CMD_AT, NV0090_CTRL_CMD_SET_LG_SECTOR_PROMOTION);
        test_put32(&mut buf, RM_PARAMS_SIZE_AT, 4);
        test_put32(&mut buf, RM_PARAMS_AT, 1);
        let before = buf.clone();
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        for at in (0..buf.len()).filter(|a| !(0x14..0x18).contains(a) && !(0x2c..0x30).contains(a)) {
            assert_eq!(buf[at], before[at], "byte {at:#x}");
        }
        // Any other control on that object is still forwarded (and refused there).
        let mut other = before.clone();
        test_put32(&mut other, RM_CMD_AT, 0x0090_0101);
        assert_eq!(apply_local(&mut other, 0, 0), Ok(LocalOutcome::ForwardRmControl));
    }

    #[test]
    fn swapchain_0124_matches_the_capture() {
        let mut buf = local_request(0x0100_0124, 0x48);
        buf[0x20..0x24].copy_from_slice(&0x190u32.to_le_bytes());
        buf[0x30..0x34].copy_from_slice(&1u32.to_le_bytes());
        buf[0x38..0x3c].copy_from_slice(&0x680u32.to_le_bytes());
        buf[0x40..0x44].copy_from_slice(&0x684u32.to_le_bytes());
        let before = buf.clone();
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        assert!(buf[0x30..0x48].iter().all(|b| *b == 0));
        assert_eq!(&buf[0x20..0x24], &before[0x20..0x24]);
        let mut short = local_request(0x0100_0124, 0x47);
        assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
    }

    fn display_control(cmd: u32, params: usize) -> Vec<u8> {
        let mut buf = vec![0u8; RM_PARAMS_AT + params];
        test_put32(&mut buf, 0, NVDA_MAGIC);
        test_put32(&mut buf, 4, NVDA_VERSION);
        let len = buf.len() as u32;
        test_put32(&mut buf, NVDA_SIZE_AT, len);
        test_put32(&mut buf, NVDA_TAG_AT, 0x4e56_474c);
        test_put32(&mut buf, NVDA_FUNCTION_AT, 0x0500_002b);
        test_put32(&mut buf, RM_H_OBJECT_AT, PSEUDO_DISPLAY_COMMON);
        test_put32(&mut buf, RM_CMD_AT, cmd);
        test_put32(&mut buf, RM_PARAMS_SIZE_AT, params as u32);
        buf
    }

    #[test]
    fn display_common_controls_match_the_capture() {
        let mut caps = display_control(NV0073_CTRL_CMD_SYSTEM_GET_CAPS, 2);
        assert_eq!(apply_local(&mut caps, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&caps[0x44..0x46], &[0x81, 0x2c]);
        let mut heads = display_control(NV0073_CTRL_CMD_SYSTEM_GET_NUM_HEADS, 0xc);
        assert_eq!(apply_local(&mut heads, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&heads[0x4c..0x50], &4u32.to_le_bytes());
        let mut active = display_control(NV0073_CTRL_CMD_SYSTEM_GET_ACTIVE, 0x10);
        assert_eq!(apply_local(&mut active, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(active[0x44 + 0xd], 1);
        let mut raster = display_control(NV0073_CTRL_CMD_SYSTEM_GET_RASTER, 0x1c);
        test_put32(&mut raster, 0x44, 0x100);
        assert_eq!(apply_local(&mut raster, 0, 0), Ok(LocalOutcome::Applied));
        assert!(is_raster_query(&raster));
        assert_eq!(&raster[0x44..0x60], &[
            0, 1, 0, 0, 0, 4, 0, 0, 0, 3, 0, 0, 0x40, 5, 0, 0, 0x26, 3, 0, 0, 0, 0, 0, 0, 60, 0, 0, 0
        ]);
        assert_eq!(&raster[0x2c..0x30], &1u32.to_le_bytes());
    }

    #[test]
    fn head_query_0200000c_matches_the_capture() {
        let mut buf = local_request(0x0200_000c, 0xac);
        test_put32(&mut buf, 0x30, 0);
        test_put32(&mut buf, 0x34, 0);
        test_put32(&mut buf, 0x38, 1);
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x40..0x54], &[0, 4, 0, 0, 0, 3, 0, 0, 32, 0, 0, 0, 60, 0, 0, 0, 0x15, 0, 0, 0]);
        assert_eq!(buf[0x88], 1);
        assert_eq!(buf[0x9d], 1);
        assert!(is_head0_query(&buf));
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        // Capture seq 933-935: heads 1..3 answer result 1 and -14 (0xfffffff2).
        for head in 1..4u32 {
            let mut other = local_request(0x0200_000c, 0xac);
            for at in (0x14..0xac).step_by(4) {
                test_put32(&mut other, at, 0);
            }
            test_put32(&mut other, 0x34, head);
            test_put32(&mut other, 0x38, 1);
            assert_eq!(apply_local(&mut other, 0, 0), Ok(LocalOutcome::Applied));
            assert_eq!(&other[0x08..0x0c], &0x30u32.to_le_bytes());
            assert_eq!(&other[0x14..0x18], &1u32.to_le_bytes());
            assert_eq!(&other[0x2c..0x30], &[0xf2, 0xff, 0xff, 0xff]);
            assert_eq!(&other[0x30..0x3c], &[0, 0, 0, 0, head as u8, 0, 0, 0, 1, 0, 0, 0]);
            assert!(other[0x40..0xac].iter().all(|b| *b == 0));
            assert!(!is_head0_query(&other));
        }
    }

    #[test]
    fn vulkan_device_queries_match_the_capture() {
        for (function, len) in [(0x0100_0021, 0x38), (0x0100_0088, 0x34), (0x0100_007c, 0x38)] {
            let mut buf = local_request(function, len);
            test_put32(&mut buf, 0x20, 0x140c_3200);
            assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
            // Capture seq 2440: 0x0021's reply keeps its 0x38-byte size.
            let size = if function == 0x0100_0021 { 0x38u32 } else { 0x30 };
            assert_eq!(&buf[0x08..0x0c], &size.to_le_bytes());
            assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
            assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
            assert_eq!(&buf[0x20..0x24], &0x140c_3200u32.to_le_bytes());
            if function == 0x0100_007c {
                assert_eq!(&buf[0x30..0x38], &[1, 0, 0, 0, 0, 0, 0, 0]);
            }
            let mut short = local_request(function, len - 1);
            assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
        }
        let mut id = local_request(0x0100_0001, 0xc0);
        assert_eq!(apply_local(&mut id, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&id[0x08..0x0c], &0xc0u32.to_le_bytes());
        assert_eq!(&id[0x30..0x34], &61088u32.to_le_bytes());
        assert_eq!(&id[0x40..0x4c], b"r610_85-151\0");
        assert_eq!(&id[0x80..0x98], b"NVIDIA GeForce RTX 5090\0");
    }

    #[test]
    fn d3d_status_and_statistics_escapes_match_the_capture() {
        // 0x0100002d: d3d11test seq 1817.
        let mut buf = local_request(0x0100_002d, 0x3c);
        test_put32(&mut buf, 0x20, 0x2337_5286);
        assert_eq!(apply_local(&mut buf, 0x4000_0280, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x20..0x24], &0x2337_5286u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x3c], &[1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
        let mut short = local_request(0x0100_002d, 0x3b);
        assert_eq!(apply_local(&mut short, 1, 0), Err(LocalError::Invalid));

        // 0x010000bb: L3 b4 reply, payload untouched.
        let mut buf = local_request(0x0100_00bb, 0x390);
        test_put32(&mut buf, 0x30, 3);
        let before = buf.clone();
        assert_eq!(apply_local(&mut buf, 1, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        assert_eq!(&buf[0x30..], &before[0x30..]);

        // 0x010000ae: success, 0 at +0x30, size kept.
        let mut buf = local_request(0x0100_00ae, 0x34);
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x34u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x34], &[1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn display_timing_matches_the_capture_layout() {
        let mut buf = local_request(0x0100_000d, DISPLAY_TIMING_BYTES);
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert!(is_display_timing(&buf));
        assert_eq!(&buf[0x08..0x0c], &0xfc0u32.to_le_bytes());
        let w = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
        assert_eq!((w(0x14), w(0x2c), w(0x40), w(0x48), w(0x68)), (1, 1, 0x100, 0x61, 0x15));
        assert_eq!((w(0x54), w(0x58), w(0x60), w(0x64)), (1024, 768, 60, 60_004));
        assert_eq!(w(0xb0), 1024);
        assert_eq!(w(0xbc), 768);
        assert_eq!(&buf[0xf4..0xf4 + 18], b"CVT:1024x768x60Hz\0");
        assert_eq!(w(0xfb0), 1);
        let mut big = local_request(0x0100_000d, DISPLAY_TIMING_BYTES);
        assert_eq!(apply_local(&mut big, 0, 0), Ok(LocalOutcome::Applied));
        set_display_timing(&mut big, 5120, 1440, 239_760);
        let w = |at: usize| u32::from_le_bytes(big[at..at + 4].try_into().unwrap());
        assert_eq!((w(0x54), w(0x58), w(0x60), w(0x64)), (5120, 1440, 240, 239_760));
        let r = reduced_blanking(5120, 1440, 239_760);
        assert_eq!(r.h_total, 5280);
        assert!(r.v_total > 1440 + 15 && r.v_total < 1700, "{r:?}");
        assert_eq!(w(0xc4) & 0xffff, r.v_total);
        assert_eq!(w(0xd0), r.pclk_khz);
        assert_eq!(&big[0xf4..0xf4 + 20], b"CVT:5120x1440x240Hz\0");
        let mut short = local_request(0x0100_000d, DISPLAY_TIMING_BYTES - 4);
        assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
    }

    #[test]
    fn nvapi_display_queries_match_the_captures() {
        // (+0x34 in, +0x38 out, +0x3c out) as captured for 0x070000f1.
        for (display, a, b) in [
            (0x100, 0x110, 0x1100),
            (0x2000, 0x115, 0x1105),
            (0x8006_1080, 0x113, 0x1103),
            (0x8006_1081, 0x112, 0x1102),
            (0x8006_1082, 0x111, 0x1101),
            (0x8006_1083, 0x110, 0x1100),
            (0x8006_1085, 0x116, 0x1106),
            (0x8006_1086, 0x115, 0x1105),
            (0x8006_1087, 0x114, 0x1104),
        ] {
            let mut buf = local_request(0x0700_00f1, 0x40);
            test_put32(&mut buf, 0x2c, 0xffff_ffe9);
            test_put32(&mut buf, 0x30, 0);
            test_put32(&mut buf, 0x34, display);
            assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
            let w = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
            assert_eq!(
                (w(0x08), w(0x14), w(0x2c), w(0x30), w(0x34), w(0x38), w(0x3c)),
                (0x40, 1, 1, 0x100, display, a, b),
                "{display:#x}"
            );
        }
        let mut unknown = local_request(0x0700_00f1, 0x40);
        test_put32(&mut unknown, 0x34, 0x1234);
        assert_eq!(apply_local(&mut unknown, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&unknown[0x2c..0x30], &0xffff_ffe9u32.to_le_bytes());
        // 0x07000113: mask -> NVAPI display id.
        for (mask, id) in [(0x100, 0x8006_1083u32), (0x2000, 0x8006_1086)] {
            let mut buf = local_request(0x0700_0113, 0x3c);
            test_put32(&mut buf, 0x30, 0x100);
            test_put32(&mut buf, 0x34, mask);
            assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
            let w = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
            assert_eq!((w(0x08), w(0x14), w(0x2c), w(0x30), w(0x38)), (0x3c, 1, 1, 0x100, id));
        }
        let mut id_in = local_request(0x0700_0113, 0x3c);
        test_put32(&mut id_in, 0x34, 0x8006_1083);
        assert_eq!(apply_local(&mut id_in, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&id_in[0x2c..0x30], &0xffff_ffe9u32.to_le_bytes());
        for k in 0..8 {
            assert_eq!(display_index(display_id_of(k)), Some(k));
            assert_eq!(display_index(0x100 << k), Some(k));
        }
        assert_eq!(apply_local(&mut local_request(0x0700_00f1, 0x3f), 0, 0), Err(LocalError::Invalid));
        assert_eq!(apply_local(&mut local_request(0x0700_0113, 0x40), 0, 0), Err(LocalError::Invalid));
    }

    #[test]
    fn display_mask_settings_and_oca_escapes_match_the_captures() {
        for (id, mask) in [
            (0x8006_1080u32, 0x800u32),
            (0x8006_1081, 0x400),
            (0x8006_1082, 0x200),
            (0x8006_1083, 0x100),
            (0x8006_1085, 0x4000),
            (0x8006_1086, 0x2000),
            (0x8006_1087, 0x1000),
        ] {
            let mut buf = local_request(0x0700_010c, 0x84);
            test_put32(&mut buf, 0x2c, 0xffff_ffe9);
            test_put32(&mut buf, 0x30, id);
            for at in (0x34..0x84).step_by(4) {
                test_put32(&mut buf, at, 0);
            }
            assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
            let w = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
            assert_eq!(
                (w(0x08), w(0x14), w(0x2c), w(0x30), w(0x34), w(0x38), w(0x3c), w(0x40), w(0x44)),
                (0x84, 1, 1, id, 0x100, mask, mask, 0xffff_ffff, 0),
                "{id:#x}"
            );
        }
        let mut unknown = local_request(0x0700_010c, 0x84);
        test_put32(&mut unknown, 0x30, 0x100);
        assert_eq!(apply_local(&mut unknown, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&unknown[0x2c..0x30], &0xffff_ffe9u32.to_le_bytes());

        for (function, len, result2) in [
            (0x0100_0041u32, 0xac0usize, 0u32),
            (0x0100_012b, 0x38, 0),
            (0x0200_0046, 0xac, 1),
        ] {
            let mut buf = local_request(function, len);
            test_put32(&mut buf, 0x20, 0x066f_c9b0);
            assert_eq!(apply_local(&mut buf, 1, 0), Ok(LocalOutcome::Applied));
            assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
            assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
            assert_eq!(&buf[0x20..0x24], &0x066f_c9b0u32.to_le_bytes());
            assert_eq!(&buf[0x2c..0x30], &result2.to_le_bytes());
            let mut bad = local_request(function, len - 1);
            assert_eq!(apply_local(&mut bad, 1, 0), Err(LocalError::Invalid));
        }
    }

    #[test]
    fn ecc_status_is_refused_locally_like_nvidia() {
        let mut buf = escape_with_handles(
            NV2080_CTRL_CMD_GPU_QUERY_ECC_STATUS,
            0,
            PSEUDO_SUBDEVICE,
            &[0; 0x680],
        );
        assert_eq!(apply_local(&mut buf, 1, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &0xffff_ffedu32.to_le_bytes());
        assert_eq!(&buf[0x40..0x44], &0x56u32.to_le_bytes());
        assert!(parse_rm_control(&buf).is_err());
    }

    #[test]
    fn nvenc_caps_request_and_reply() {
        let mut params = [0u8; 0x10];
        params[0..4].copy_from_slice(&6u32.to_le_bytes());
        params[8..16].copy_from_slice(&0xa8_207f_9e90u64.to_le_bytes());
        let mut buf = escape_with_handles(NV0080_CTRL_CMD_NVENC_GET_CAPS, 0, PSEUDO_DEVICE, &params);
        let request = nvenc_caps_request(&buf).unwrap();
        assert_eq!(request, NvencCaps { table: 0xa8_207f_9e90, size: 6 });
        assert_eq!(nvenc_caps_writable(request), Ok(()));
        assert_eq!(
            nvenc_caps_writable(NvencCaps { table: 0, size: 6 }),
            Err(NvencCapsWrite::BadAddress)
        );
        assert_eq!(
            nvenc_caps_writable(NvencCaps { table: 1, size: 5 }),
            Err(NvencCapsWrite::BadSize)
        );
        answer_nvenc_caps(&mut buf, NvencCapsWrite::Written);
        let control = parse_rm_control(&buf).unwrap();
        assert_eq!(control.status, 0);
        assert_eq!(control.params, params);
        assert_eq!(&buf[0x08..0x0c], &0x54u32.to_le_bytes());
        answer_nvenc_caps(&mut buf, NvencCapsWrite::BadAddress);
        assert_eq!(parse_rm_control(&buf).unwrap().status, 0x1e);
        // Another control or another parameter size is not this request.
        let other = escape_with_handles(0x0080_1b02, 0, PSEUDO_DEVICE, &params);
        assert_eq!(nvenc_caps_request(&other), None);
        let short = escape_with_handles(NV0080_CTRL_CMD_NVENC_GET_CAPS, 0, PSEUDO_DEVICE, &[0; 8]);
        assert_eq!(nvenc_caps_request(&short), None);
    }

    #[test]
    fn user_shared_reply_matches_the_capture() {
        let mut buf = local_request(NVDA_FN_USER_SHARED, NVDA_FN_USER_SHARED_BYTES);
        answer_user_shared(&mut buf, 0x1b0_b7f5_9000);
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x30..0x3c], &[0, 0, 0, 0, 0, 0x90, 0xf5, 0xb7, 0xb0, 1, 0, 0]);
    }

    #[test]
    fn id_0100_0057_is_nonzero() {
        let mut buf = local_request(0x0100_0057, 0x38);
        assert_eq!(apply_local(&mut buf, 0x40000500, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        // Capture seq 937/1808: result 1, second result word 0.
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &0u32.to_le_bytes());
        assert_ne!(&buf[0x30..0x34], &[0, 0, 0, 0]);
    }

    #[test]
    fn present_path_queries_match_the_capture() {
        // vktest capture seq 3789: 0x0100010e keeps its size, 0x100 at +0x30.
        let mut nvapi = local_request(0x0100_010e, 0x34);
        let before = nvapi.clone();
        assert_eq!(apply_local(&mut nvapi, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&nvapi[0x08..0x0c], &0x34u32.to_le_bytes());
        assert_eq!(&nvapi[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&nvapi[0x2c..0x34], &[1, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(&nvapi[..0x14], &before[..0x14]);
        assert_eq!(&nvapi[0x18..0x2c], &before[0x18..0x2c]);
        // Seq 5797: 0x0200001d, 0x30-byte reply, 1 at +0x30; needs device and context.
        let mut present = local_request(0x0200_001d, 0x34);
        assert_eq!(apply_local(&mut present, 0x40006500, 0x400077c0), Ok(LocalOutcome::Applied));
        assert_eq!(&present[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&present[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&present[0x2c..0x34], &[1, 0, 0, 0, 1, 0, 0, 0]);
        let mut unscoped = local_request(0x0200_001d, 0x34);
        assert_eq!(apply_local(&mut unscoped, 0x40006500, 0), Err(LocalError::Invalid));
        // Seq 19409: 0x02000031 bare 0x30-byte success, request word +0x30 kept.
        let mut teardown = local_request(0x0200_0031, 0x34);
        test_put32(&mut teardown, 0x30, 0x354c);
        assert_eq!(apply_local(&mut teardown, 0x40006500, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&teardown[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&teardown[0x2c..0x34], &[1, 0, 0, 0, 0x4c, 0x35, 0, 0]);
        for function in [0x0100_010e, 0x0200_001d, 0x0200_0031] {
            let mut short = local_request(function, 0x33);
            assert_eq!(apply_local(&mut short, 1, 1), Err(LocalError::Invalid));
        }
    }

    fn class_engine_request(h_object: u32) -> Vec<u8> {
        // Capture seq 1989 request bytes: NVGL 0x0500002b, 0x54 bytes, hClient 0,
        // hObject 0xff040000, cmd 0x906f0101, 0x10 parameter bytes.
        let mut buf = vec![0u8; 0x54];
        test_put32(&mut buf, 0, NVDA_MAGIC);
        test_put32(&mut buf, 4, NVDA_VERSION);
        test_put32(&mut buf, NVDA_SIZE_AT, 0x54);
        test_put32(&mut buf, NVDA_TAG_AT, 0x4e56_474c);
        test_put32(&mut buf, NVDA_FUNCTION_AT, 0x0500_002b);
        test_put32(&mut buf, RM_H_OBJECT_AT, PSEUDO_GRAPHICS_CONTEXT);
        test_put32(&mut buf, RM_CMD_AT, NV906F_CTRL_CMD_GET_CLASS_ENGINEID);
        test_put32(&mut buf, RM_PARAMS_SIZE_AT, 0x10);
        test_put32(&mut buf, RM_PARAMS_AT, h_object);
        buf
    }

    #[test]
    fn class_engine_ids_match_the_capture() {
        // Capture seq 1989-1991 after bytes +0x40..+0x54.
        for (h_object, after) in [
            (0xff1f_e026u32, [0x26, 0xe0, 0x1f, 0xff, 0x97, 0xce, 0, 0, 0x97, 0xce, 0, 0, 1, 0, 0, 0]),
            (0xff1f_e02c, [0x2c, 0xe0, 0x1f, 0xff, 0x2d, 0x90, 0, 0, 0x2d, 0x90, 0, 0, 1, 0, 0, 0]),
            (0xff1f_e01a, [0x1a, 0xe0, 0x1f, 0xff, 0x40, 0xcd, 0, 0, 0x40, 0xcd, 0, 0, 1, 0, 0, 0]),
        ] {
            let mut buf = class_engine_request(h_object);
            let before = buf.clone();
            assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
            assert_eq!(&buf[0x44..0x54], &after);
            assert_eq!(&buf[0x40..0x44], &[0, 0, 0, 0]);
            assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
            assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
            for at in (0..0x44).filter(|a| !(0x14..0x18).contains(a) && !(0x2c..0x30).contains(a)) {
                assert_eq!(buf[at], before[at], "byte {at:#x}");
            }
        }
        // An unknown object, a short parameter block or another object stays
        // on the forwarding path.
        let mut unknown = class_engine_request(0xff1f_e030);
        assert_eq!(apply_local(&mut unknown, 0, 0), Ok(LocalOutcome::ForwardRmControl));
        let mut channel = class_engine_request(0xff1f_e026);
        test_put32(&mut channel, RM_H_OBJECT_AT, PSEUDO_SUBDEVICE);
        assert_eq!(apply_local(&mut channel, 0, 0), Ok(LocalOutcome::ForwardRmControl));
    }

    #[test]
    fn swapchain_0116_matches_the_capture() {
        let mut buf = local_request(0x0100_0116, 0x39);
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        assert_eq!(&buf[0x30..0x39], &[0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let mut short = local_request(0x0100_0116, 0x38);
        assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
    }

    #[test]
    fn query_0090_matches_the_capture() {
        let mut buf = local_request(0x0100_0090, 0x3c);
        buf[0x30..0x34].copy_from_slice(&2u32.to_le_bytes());
        buf[0x34..0x38].copy_from_slice(&0x3d0u32.to_le_bytes());
        let before = buf.clone();
        assert_eq!(apply_local(&mut buf, 0, 0), Ok(LocalOutcome::Applied));
        assert_eq!(&buf[0x08..0x0c], &0x30u32.to_le_bytes());
        assert_eq!(&buf[0x14..0x18], &1u32.to_le_bytes());
        assert_eq!(&buf[0x2c..0x30], &1u32.to_le_bytes());
        for at in (0..buf.len()).filter(|a| !(0x08..0x0c).contains(a) && !(0x14..0x18).contains(a) && !(0x2c..0x30).contains(a)) {
            assert_eq!(buf[at], before[at], "byte {at:#x}");
        }
        let mut short = local_request(0x0100_0090, 0x3b);
        assert_eq!(apply_local(&mut short, 0, 0), Err(LocalError::Invalid));
    }

    #[test]
    fn local_dispatch_forwards_only_rm_control_and_rejects_unknown_functions() {
        let mut control = local_request(0x0500_002b, NVDA_HEADER_BYTES);
        test_put32(&mut control, RM_PARAMS_SIZE_AT, 0);
        let snapshot = control.clone();
        assert_eq!(
            apply_local(&mut control, 0, 0),
            Ok(LocalOutcome::ForwardRmControl)
        );
        assert_eq!(control, snapshot);
        assert_eq!(parse_rm_control(&control).unwrap().cmd, 0xa5a5_a5a5);

        let mut unknown = local_request(0x0100_9999, NVDA_COMMON_HEADER_BYTES);
        let snapshot = unknown.clone();
        assert_eq!(
            apply_local(&mut unknown, 1, 1),
            Err(LocalError::Unsupported)
        );
        assert_eq!(unknown, snapshot);
    }

    #[test]
    fn common_header_accepts_short_local_requests_but_rm_control_still_needs_0x44() {
        for len in [NVDA_COMMON_HEADER_BYTES, 0x36, 0x38, 0x3c] {
            let buf = local_request(0x0100_0140, len);
            assert!(parse_header(&buf).is_ok());
            assert_eq!(
                parse_rm_control(&buf),
                Err(ControlError::Header(HeaderError::Short))
            );
        }
        for len in 0..NVDA_COMMON_HEADER_BYTES {
            let buf = vec![0xa5; len];
            assert_eq!(parse_header(&buf), Err(HeaderError::Short));
        }
    }

    #[test]
    fn arbitrary_short_buffers_never_panic() {
        for len in 0..84 {
            let buf = vec![0xff; len];
            let _ = parse_header(&buf);
            let _ = parse_rm_control(&buf);
            let _ = parse_host_reply(&buf, usize::MAX);
            let mut local = buf.clone();
            let _ = apply_local(&mut local, 1, 1);
            let mut output = buf;
            let reply = HostReply {
                rm_status: u32::MAX,
                params: &[],
            };
            let _ = apply_host_reply(&mut output, &reply);
        }
    }

    #[test]
    fn persona_knob_defaults_to_off() {
        assert_eq!(Persona::from_knob(0), Persona::Off);
        assert_eq!(Persona::from_knob(1), Persona::Census);
        assert_eq!(Persona::from_knob(2), Persona::Forward);
        for value in [3, 4, 255, u32::MAX] {
            assert_eq!(Persona::from_knob(value), Persona::Off);
            assert_eq!(Persona::from(value), Persona::Off);
        }
    }
}

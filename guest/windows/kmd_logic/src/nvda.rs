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
            exact(buf, 0x38)?;
            put32(buf, NVDA_RESULT_AT, 1);
            put32(buf, NVDA_RESULT2_AT, 1);
            put32(buf, 0x34, 1);
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

#[cfg(test)]
mod tests {
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
            &[(NVDA_RESULT_AT, 1), (NVDA_RESULT2_AT, 1), (0x34, 1)],
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

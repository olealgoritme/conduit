// crates/protocol/src/messages.rs
//
// Wire message types for conduit-gpu.
//
// Descriptor chain layout (one chain per operation):
//
//   [readable: MsgHeader + payload] → [writable: MsgHeader + payload]
//
// The guest driver:
//   1. Fills in the readable buffer (header + request payload).
//   2. Adds the writable buffer to the chain.
//   3. Posts the chain to the virtqueue and waits for a used-ring notification.
//   4. Reads the writable buffer (header + response payload).
//
// The backend:
//   1. Receives the readable buffer.
//   2. Dispatches on `MsgHeader::msg_type`.
//   3. Writes the response into the writable buffer.
//   4. Pushes the chain back to the used ring.
//
// Every layout here mirrors `guest/linux/conduit_gpu.c`. That file is the wire
// format: it is the half compiled into a guest kernel, and it cannot negotiate.
//
// These definitions previously described a different protocol entirely -- a
// header of `{msg_type, pad, cookie}` against the driver's
// `{msg_type, handle, status, padding}`, an open request of `{kind, index}`
// against the driver's flat `device_type`, and no message at all for four of
// the seven the driver sends. Both halves compiled, and a guest reached the
// point of creating its device nodes before anything went wrong. Nothing checks
// this agreement except the tests here and a guest that fails oddly, so treat a
// change on either side as a change to both.

// ---------------------------------------------------------------------------
// Message type discriminants
// ---------------------------------------------------------------------------

/// Identifies the kind of message in a `MsgHeader`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    /// Guest → host: open a `/dev/nvidia*` device.
    Open = 1,
    /// Guest → host: close a previously opened handle.
    Close = 2,
    /// Guest → host: forward a raw ioctl.
    Ioctl = 3,
    /// Guest → host: map device memory into the shared window.
    Mmap = 4,
    /// Guest → host: release a mapping made by `Mmap`.
    Munmap = 5,
    /// Guest → host: the contents of the host's `/proc/driver/nvidia` tree,
    /// which the guest republishes so its own userspace can read them.
    GetProcFiles = 6,
    /// Guest → host: the same for the sysfs attributes the userspace driver
    /// looks for.
    GetSysFiles = 7,
    /// **Host → guest**, on the event queue: the descriptor named by
    /// `MsgHeader::handle` has something to report.
    ///
    /// The only message that travels this way. NVIDIA's user-mode driver waits
    /// for the GPU by polling the descriptor an RM event is delivered on; the
    /// host driver takes the interrupt and makes *its* descriptor readable, and
    /// this carries that edge across so the guest's can do the same. Without
    /// it a guest cannot wait at all: a `file_operations` with no `.poll` is
    /// reported ready by the VFS every time it is asked, and a driver that
    /// meant to sleep spins instead.
    ///
    /// No payload. The handle in the header is the whole message.
    EventReady = 8,
    /// Guest → host, control queue: present a framebuffer. Payload
    /// [`ScanoutFlip`]; the reply is a bare header. See `docs/SCANOUT.md`.
    ScanoutFlip = 20,
    /// Guest → host, control queue: the scanout is off. Payload
    /// [`ScanoutDisable`]; the reply is a bare header.
    ScanoutDisable = 21,
    /// **Host → guest**, on the event queue: a batch of Linux input events,
    /// [`InputEventBatch`] followed by `count` [`InputEventEntry`]s.
    InputEvent = 22,
    /// **Host → guest**, on the event queue: the display's preferred mode
    /// changed (the viewer window was resized, went fullscreen or came back).
    /// Payload [`DisplayModeEvent`]. The guest makes it the connector's
    /// preferred mode and sends a hotplug event; the compositor decides.
    DisplayMode = 23,
    /// Guest → host, control queue, fire-and-forget like `ScanoutFlip`: the
    /// cursor plane's image or hotspot changed. Payload [`CursorUpdate`]; the
    /// reply is a bare header.
    CursorUpdate = 24,
    /// **Host → guest**, on the event queue: one chunk of the host clipboard.
    /// Payload [`ClipboardChunk`] followed by `len` data bytes. Chunks of one
    /// transfer arrive in order; the guest reassembles them by `generation`.
    ClipboardFromHost = 25,
    /// Guest → host, control queue: one chunk of the guest clipboard. Payload
    /// [`ClipboardChunk`] followed by `len` data bytes; the reply is a bare
    /// header, status 0 or a negative errno.
    ClipboardToHost = 26,
    /// Guest → host, control queue, no payload: the guest's clipboard device
    /// is ready (or its reader found it empty); send the current host
    /// clipboard again as `ClipboardFromHost`. Anything sent before the guest
    /// driver was up is otherwise lost. The reply is a bare header, status 0
    /// or `-ENODEV` without a display. Older backends answer it with an
    /// unknown-type error, which the guest ignores.
    ClipboardRequest = 27,
    /// Guest → host, control queue: one virtio-gpu control command for the
    /// Venus renderer (docs/VENUS.md), laid out as `crate::venus` describes.
    /// The reply is a header and the virtio-gpu response. Served only when
    /// config `features` carries [`NVGPU_CFG_VENUS`]; refused otherwise.
    GpuCmd = 30,
}

impl MsgType {
    /// Decode a wire discriminant.
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::Open,
            2 => Self::Close,
            3 => Self::Ioctl,
            4 => Self::Mmap,
            5 => Self::Munmap,
            6 => Self::GetProcFiles,
            7 => Self::GetSysFiles,
            8 => Self::EventReady,
            20 => Self::ScanoutFlip,
            21 => Self::ScanoutDisable,
            22 => Self::InputEvent,
            23 => Self::DisplayMode,
            24 => Self::CursorUpdate,
            25 => Self::ClipboardFromHost,
            26 => Self::ClipboardToHost,
            27 => Self::ClipboardRequest,
            30 => Self::GpuCmd,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Common header, used for both directions
// ---------------------------------------------------------------------------

/// Every message starts with this header, in both directions.
///
/// One type rather than a request and a response type, because the driver uses
/// one: `struct nvgpu_msg_hdr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsgHeader {
    /// One of the `MsgType` values.
    pub msg_type: u32,
    /// On a request, the handle to act on. On an `Open` response, the handle
    /// the guest should use from then on.
    pub handle: u32,
    /// Result, and **signed**: zero on success, negative errno on failure. The
    /// driver tests `(s32)status < 0`, so writing an unsigned error code here
    /// reads as success.
    pub status: i32,
    pub padding: u32,
}

impl MsgHeader {
    /// A success response carrying `handle`.
    pub fn ok(msg_type: MsgType, handle: u32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle,
            status: 0,
            padding: 0,
        }
    }

    /// A failure response. `errno` is given as a positive number and stored
    /// negated, which is the one direction that is easy to get wrong.
    pub fn err(msg_type: MsgType, errno: i32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle: 0,
            status: -errno.abs(),
            padding: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Device identification
// ---------------------------------------------------------------------------

/// `/dev/nvidiactl`.
pub const DEV_CTL: u32 = 255;
/// `/dev/nvidia-uvm`.
pub const DEV_UVM: u32 = 256;
/// `/dev/nvidia-uvm-tools`.
pub const DEV_UVM_TOOLS: u32 = 257;
/// `/dev/nvidia-modeset`.
pub const DEV_MODESET: u32 = 258;
/// Render nodes start here.
pub const DEV_DRI_BASE: u32 = 512;
/// Highest GPU index expressible before the control device's value.
pub const MAX_GPU_INDEX: u32 = 254;

/// Which device an `Open` refers to.
///
/// The wire encoding is one flat `u32`, not a kind and an index: a GPU is its
/// own minor number, and the singleton devices take values above every possible
/// minor. Decoding is therefore a range check, not a table lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// `/dev/nvidiaN`, where N is the guest's minor number.
    Gpu(u32),
    Ctl,
    Uvm,
    UvmTools,
    Modeset,
    /// A DRM render node, by its index in the list the device reported.
    Dri(u32),
}

impl DeviceKind {
    /// Decode the `device_type` field of an `OpenReq`.
    ///
    /// Returns `None` for anything unrecognised.
    ///
    /// Render nodes are openable, and have to be: NVIDIA's Vulkan and EGL
    /// userspace enumerates the GPU through the DRM render node rather than
    /// through `/dev/nvidia*`, which carry compute. Refusing them here is what
    /// a guest sees as a Vulkan loader that finds a driver, loads it, and is
    /// then told there are none.
    pub fn from_device_type(v: u32) -> Option<Self> {
        Some(match v {
            0..=MAX_GPU_INDEX => Self::Gpu(v),
            DEV_CTL => Self::Ctl,
            DEV_UVM => Self::Uvm,
            DEV_UVM_TOOLS => Self::UvmTools,
            DEV_MODESET => Self::Modeset,
            _ if v >= DEV_DRI_BASE => Self::Dri(v - DEV_DRI_BASE),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// OPEN
// ---------------------------------------------------------------------------

/// Request payload for `MsgType::Open`, following a `MsgHeader`.
///
/// The response is a bare `MsgHeader`: the new handle travels in
/// `MsgHeader::handle`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenReq {
    /// See [`DeviceKind::from_device_type`].
    pub device_type: u32,
    /// The guest's `open(2)` flags.
    pub flags: u32,
}

// ---------------------------------------------------------------------------
// IOCTL
// ---------------------------------------------------------------------------

/// Request payload for `MsgType::Ioctl`, following a `MsgHeader`.
///
/// Layout: `MsgHeader` | `IoctlReq` | `data_len` bytes | `nested_len` bytes |
/// `deep_len` bytes.
///
/// The nested block is data an ioctl parameter points at. The guest cannot pass
/// a pointer that means anything on the host, so it sends the pointed-to bytes
/// alongside and says where in the top-level struct the pointer sits.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlReq {
    /// The ioctl request number.
    pub cmd: u32,
    /// Bytes of top-level parameter struct following this header.
    pub data_len: u32,
    /// Where the nested block begins, measured from the start of the payload
    /// -- so in practice always equal to `data_len`, since the driver lays the
    /// nested block immediately after the top-level struct
    /// (`req->nested_offset = cpu_to_le32(sizeof(params))`). Zero when there is
    /// no nested block. It is *not* the offset of the pointer field being
    /// replaced, which is what the name suggests.
    pub nested_offset: u32,
    /// Bytes of nested data following the top-level struct.
    pub nested_len: u32,
    /// Where, inside the nested block, a further pointer sits, when the nested
    /// block carries one. Only meaningful when `deep_len` is non-zero.
    pub deep_ptr_offset: u32,
    /// Bytes of a second-level block following the nested block: what the
    /// pointer at `deep_ptr_offset` points at in the guest.
    ///
    /// Some parameter blocks hold a pointer of their own. The guest cannot
    /// send an address that means anything here, so it sends those bytes too
    /// and the backend gives them a host address before the call. Zero when
    /// the nested block carries no pointer.
    pub deep_len: u32,
}

/// Response payload for `MsgType::Ioctl`, following a `MsgHeader`.
///
/// Layout: `MsgHeader` | `IoctlResp` | `data_len` bytes | `nested_len` bytes |
/// `deep_len` bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlResp {
    pub data_len: u32,
    pub nested_len: u32,
    /// Bytes of second-level block following the nested block, for the guest
    /// to copy back to where its own pointer points.
    pub deep_len: u32,
}

// ---------------------------------------------------------------------------
// MMAP / MUNMAP
// ---------------------------------------------------------------------------

/// Request payload for `MsgType::Mmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MmapReq {
    pub size: u64,
    pub offset: u64,
    pub prot: u32,
    pub padding: u32,
}

/// Response payload for `MsgType::Mmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MmapResp {
    /// Where the guest should map from, in its own physical address space.
    pub guest_phys_addr: u64,
    pub size: u64,
    /// Identifier the guest passes back to `Munmap`.
    pub mapping_id: u32,
    pub padding: u32,
}

/// Request payload for `MsgType::Munmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MunmapReq {
    pub mapping_id: u32,
    pub padding: u32,
}

// ---------------------------------------------------------------------------
// GET_PROC_FILES / GET_SYS_FILES
// ---------------------------------------------------------------------------

/// One file in the response to `GetProcFiles` or `GetSysFiles`.
///
/// The response is a bare stream of these -- **no `MsgHeader`** -- each
/// followed by `path_len` bytes of path and `content_len` bytes of content,
/// terminated by an entry whose `path_len` is zero. The driver reads from the
/// first byte of the response buffer, so prefixing a header shifts everything
/// and it decodes the header as a length.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FileEntry {
    /// Zero marks the end of the stream.
    pub path_len: u32,
    pub content_len: u32,
}

// ---------------------------------------------------------------------------
// SCANOUT_FLIP / SCANOUT_DISABLE / INPUT_EVENT (docs/SCANOUT.md)
// ---------------------------------------------------------------------------

/// Device config `features` bit: the device has a display. When set, the
/// guest reads `display_width`, `display_height` and `display_refresh_hz`
/// (appended after the existing config fields) and brings up its KMS head.
pub const NVGPU_CFG_DISPLAY: u32 = 1 << 8;

/// Device config `features` bit, only together with [`NVGPU_CFG_DISPLAY`]:
/// the host shows a cursor plane (`CursorUpdate`) as its own pointer image,
/// so the guest head offers one. Without it the guest has no cursor plane and
/// its compositor draws the cursor into the frame, as before.
pub const NVGPU_CFG_CURSOR: u32 = 1 << 9;

/// Device config `features` bit: the device serves `GpuCmd` (Venus, for
/// Windows guests; docs/VENUS.md) and has shared memory region 3 for
/// host-visible blobs. Set only when the backend runs with `--venus`.
pub const NVGPU_CFG_VENUS: u32 = 1 << 10;

/// Request payload for `MsgType::ScanoutFlip`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanoutFlip {
    /// 0; one scanout for now.
    pub scanout: u32,
    /// The backend handle of the drm_file owning the host GEM object.
    pub owner_handle: u32,
    /// The GEM handle in that host drm_file.
    pub host_handle: u32,
    pub width: u32,
    pub height: u32,
    /// Plane 0 pitch, bytes.
    pub stride: u32,
    /// Plane 0 offset, bytes.
    pub offset: u32,
    /// `DRM_FORMAT_*`.
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*`; NVIDIA block-linear allowed.
    pub modifier: u64,
    /// Monotonically increasing per flip.
    pub seq: u64,
    pub reserved: [u32; 4],
}

impl ScanoutFlip {
    /// Decode from the bytes after the header. `None` if too short.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < size_of::<Self>() {
            return None;
        }
        let w = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let q = |at: usize| (w(at) as u64) | ((w(at + 4) as u64) << 32);
        Some(Self {
            scanout: w(0),
            owner_handle: w(4),
            host_handle: w(8),
            width: w(12),
            height: w(16),
            stride: w(20),
            offset: w(24),
            fourcc: w(28),
            modifier: q(32),
            seq: q(40),
            reserved: [w(48), w(52), w(56), w(60)],
        })
    }

    /// Encode, as the guest lays it out.
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut o = [0u8; 64];
        let words = [
            self.scanout,
            self.owner_handle,
            self.host_handle,
            self.width,
            self.height,
            self.stride,
            self.offset,
            self.fourcc,
        ];
        for (i, v) in words.iter().enumerate() {
            o[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        o[32..40].copy_from_slice(&self.modifier.to_le_bytes());
        o[40..48].copy_from_slice(&self.seq.to_le_bytes());
        for (i, v) in self.reserved.iter().enumerate() {
            o[48 + i * 4..52 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        o
    }
}

/// Request payload for `MsgType::ScanoutDisable`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanoutDisable {
    pub scanout: u32,
    pub pad: u32,
}

/// Event-queue payload for `MsgType::DisplayMode`, following a `MsgHeader`.
///
/// The display's new preferred mode. `refresh_mhz` is millihertz (the host
/// output's rate as the viewer measured it); 0 means "keep the current rate".
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayModeEvent {
    pub scanout: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl DisplayModeEvent {
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut o = [0u8; 16];
        for (i, v) in [self.scanout, self.width, self.height, self.refresh_mhz]
            .iter()
            .enumerate()
        {
            o[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        o
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 16 {
            return None;
        }
        let w = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        Some(Self {
            scanout: w(0),
            width: w(4),
            height: w(8),
            refresh_mhz: w(12),
        })
    }
}

/// Size of a whole `DisplayMode` event-queue message.
pub const DISPLAY_MODE_MESSAGE_LEN: usize = size_of::<MsgHeader>() + size_of::<DisplayModeEvent>();

/// Encode one `DisplayMode` message (header and payload) into `out`.
pub fn encode_display_mode(m: &DisplayModeEvent, out: &mut [u8]) -> Option<usize> {
    if out.len() < DISPLAY_MODE_MESSAGE_LEN {
        return None;
    }
    let hdr = MsgHeader::ok(MsgType::DisplayMode, 0);
    for (i, v) in [hdr.msg_type, hdr.handle, hdr.status as u32, hdr.padding]
        .iter()
        .enumerate()
    {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out[16..32].copy_from_slice(&m.to_bytes());
    Some(DISPLAY_MODE_MESSAGE_LEN)
}

// ---------------------------------------------------------------------------
// Clipboard (ClipboardFromHost / ClipboardToHost)
// ---------------------------------------------------------------------------

/// Largest clipboard transfer either direction carries, bytes.
pub const CLIPBOARD_MAX_BYTES: u32 = 1 << 20;
/// The one MIME type defined so far.
pub const CLIPBOARD_MIME_TEXT: &str = "text/plain;charset=utf-8";
/// Bytes of [`ClipboardChunk`] before the data.
pub const CLIPBOARD_CHUNK_HEAD: usize = 56;
/// Size of the `mime` field.
pub const CLIPBOARD_MIME_LEN: usize = 32;

/// `mime` field for `m`: NUL-padded ASCII, truncated to fit with a NUL.
pub fn clipboard_mime(m: &str) -> [u8; CLIPBOARD_MIME_LEN] {
    let mut o = [0u8; CLIPBOARD_MIME_LEN];
    let b = m.as_bytes();
    let n = b.len().min(CLIPBOARD_MIME_LEN - 1);
    o[..n].copy_from_slice(&b[..n]);
    o
}

/// One chunk of a clipboard transfer, following a `MsgHeader`; `len` data
/// bytes follow it. Rules (both directions): offsets contiguous from 0, offset
/// 0 starts a new transfer, complete when `offset + len == total_len`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipboardChunk {
    pub generation: u64,
    pub total_len: u32,
    pub offset: u32,
    pub len: u32,
    pub flags: u32,
    pub mime: [u8; CLIPBOARD_MIME_LEN],
}

impl ClipboardChunk {
    pub fn to_bytes(&self) -> [u8; CLIPBOARD_CHUNK_HEAD] {
        let mut o = [0u8; CLIPBOARD_CHUNK_HEAD];
        o[0..8].copy_from_slice(&self.generation.to_le_bytes());
        for (i, v) in [self.total_len, self.offset, self.len, self.flags]
            .iter()
            .enumerate()
        {
            o[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        o[24..56].copy_from_slice(&self.mime);
        o
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < CLIPBOARD_CHUNK_HEAD {
            return None;
        }
        let w = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let mut mime = [0u8; CLIPBOARD_MIME_LEN];
        mime.copy_from_slice(&b[24..56]);
        Some(Self {
            generation: u64::from_le_bytes(b[0..8].try_into().ok()?),
            total_len: w(8),
            offset: w(12),
            len: w(16),
            flags: w(20),
            mime,
        })
    }

    /// The MIME type up to its first NUL, if it is ASCII.
    pub fn mime_str(&self) -> Option<&str> {
        let n = self
            .mime
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(CLIPBOARD_MIME_LEN);
        let s = core::str::from_utf8(&self.mime[..n]).ok()?;
        s.is_ascii().then_some(s)
    }

    /// True when the type is the defined text type.
    pub fn is_text(&self) -> bool {
        self.mime_str() == Some(CLIPBOARD_MIME_TEXT)
    }
}

/// Bytes before the data of a whole clipboard message (header + chunk).
pub const CLIPBOARD_MESSAGE_HEAD: usize = size_of::<MsgHeader>() + CLIPBOARD_CHUNK_HEAD;

/// Encode one clipboard message of type `t` (header, chunk, `data`) into
/// `out`. `c.len` is taken from `data`. Returns bytes written, or `None` if
/// `out` is too small.
pub fn encode_clipboard_chunk(
    t: MsgType,
    c: &ClipboardChunk,
    data: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let need = CLIPBOARD_MESSAGE_HEAD + data.len();
    if out.len() < need {
        return None;
    }
    let hdr = MsgHeader::ok(t, 0);
    for (i, v) in [hdr.msg_type, hdr.handle, hdr.status as u32, hdr.padding]
        .iter()
        .enumerate()
    {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let c = ClipboardChunk {
        len: data.len() as u32,
        ..*c
    };
    out[16..CLIPBOARD_MESSAGE_HEAD].copy_from_slice(&c.to_bytes());
    out[CLIPBOARD_MESSAGE_HEAD..need].copy_from_slice(data);
    Some(need)
}

/// `CursorUpdate::flags`: the cursor plane shows an image. Clear means the
/// cursor is hidden (no framebuffer on the plane); the buffer fields are 0.
pub const CURSOR_F_VISIBLE: u32 = 1 << 0;

/// Largest cursor edge the guest advertises and the host accepts, pixels.
pub const CURSOR_MAX_DIM: u32 = 256;

/// Request payload for `MsgType::CursorUpdate`, following a `MsgHeader`.
///
/// Sent when the cursor plane's framebuffer, hotspot or visibility changes --
/// never for a move: the host pointer drives the cursor position itself.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorUpdate {
    /// 0; one scanout for now.
    pub scanout: u32,
    pub width: u32,
    pub height: u32,
    /// Hotspot, pixels from the image's top-left; less than width/height.
    pub hot_x: u32,
    pub hot_y: u32,
    /// The backend handle of the drm_file owning the host GEM object.
    pub owner_handle: u32,
    /// The GEM handle in that host drm_file.
    pub host_handle: u32,
    pub stride: u32,
    pub offset: u32,
    /// `DRM_FORMAT_ARGB8888`.
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_LINEAR`.
    pub modifier: u64,
    /// Where the guest last put the plane, informational only.
    pub crtc_x: i32,
    pub crtc_y: i32,
    /// `CURSOR_F_*`.
    pub flags: u32,
    /// Per-update counter.
    pub seq: u32,
}

impl CursorUpdate {
    pub fn visible(&self) -> bool {
        self.flags & CURSOR_F_VISIBLE != 0
    }

    /// Decode from the bytes after the header. `None` if too short.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < size_of::<Self>() {
            return None;
        }
        let w = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        Some(Self {
            scanout: w(0),
            width: w(4),
            height: w(8),
            hot_x: w(12),
            hot_y: w(16),
            owner_handle: w(20),
            host_handle: w(24),
            stride: w(28),
            offset: w(32),
            fourcc: w(36),
            modifier: (w(40) as u64) | ((w(44) as u64) << 32),
            crtc_x: w(48) as i32,
            crtc_y: w(52) as i32,
            flags: w(56),
            seq: w(60),
        })
    }

    /// Encode, as the guest lays it out.
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut o = [0u8; 64];
        let words = [
            self.scanout,
            self.width,
            self.height,
            self.hot_x,
            self.hot_y,
            self.owner_handle,
            self.host_handle,
            self.stride,
            self.offset,
            self.fourcc,
        ];
        for (i, v) in words.iter().enumerate() {
            o[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        o[40..48].copy_from_slice(&self.modifier.to_le_bytes());
        o[48..52].copy_from_slice(&self.crtc_x.to_le_bytes());
        o[52..56].copy_from_slice(&self.crtc_y.to_le_bytes());
        o[56..60].copy_from_slice(&self.flags.to_le_bytes());
        o[60..64].copy_from_slice(&self.seq.to_le_bytes());
        o
    }
}

/// Event-queue payload for `MsgType::InputEvent`, following a `MsgHeader`:
/// this, then `count` [`InputEventEntry`]s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEventBatch {
    pub count: u32,
    pub pad: u32,
}

/// One Linux `input_event` triple, `EV_SYN` included.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEventEntry {
    pub ev_type: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEventEntry {
    pub const fn new(ev_type: u16, code: u16, value: i32) -> Self {
        Self {
            ev_type,
            code,
            value,
        }
    }

    pub fn to_bytes(&self) -> [u8; 8] {
        let mut o = [0u8; 8];
        o[0..2].copy_from_slice(&self.ev_type.to_le_bytes());
        o[2..4].copy_from_slice(&self.code.to_le_bytes());
        o[4..8].copy_from_slice(&self.value.to_le_bytes());
        o
    }

    pub fn from_bytes(b: &[u8; 8]) -> Self {
        Self {
            ev_type: u16::from_le_bytes([b[0], b[1]]),
            code: u16::from_le_bytes([b[2], b[3]]),
            value: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        }
    }
}

/// Encode one `InputEvent` message (header, batch, entries) into `out`.
///
/// Returns the bytes written, or `None` if `out` cannot hold every entry.
pub fn encode_input_events(events: &[InputEventEntry], out: &mut [u8]) -> Option<usize> {
    let need = input_event_message_len(events.len());
    if out.len() < need {
        return None;
    }
    let hdr = MsgHeader::ok(MsgType::InputEvent, 0);
    for (i, v) in [
        hdr.msg_type,
        hdr.handle,
        hdr.status as u32,
        hdr.padding,
        events.len() as u32,
        0,
    ]
    .iter()
    .enumerate()
    {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut at = INPUT_EVENT_HEAD;
    for e in events {
        out[at..at + 8].copy_from_slice(&e.to_bytes());
        at += 8;
    }
    Some(need)
}

/// Bytes before the first entry of an `InputEvent` message.
pub const INPUT_EVENT_HEAD: usize = size_of::<MsgHeader>() + size_of::<InputEventBatch>();

/// Size of an `InputEvent` message carrying `n` entries.
pub const fn input_event_message_len(n: usize) -> usize {
    INPUT_EVENT_HEAD + n * size_of::<InputEventEntry>()
}

/// How many entries fit in an event-queue buffer of `buf_len` bytes.
pub const fn input_events_that_fit(buf_len: usize) -> usize {
    if buf_len < INPUT_EVENT_HEAD {
        0
    } else {
        (buf_len - INPUT_EVENT_HEAD) / size_of::<InputEventEntry>()
    }
}

/// Absolute pointer axes are scaled to `0..=INPUT_ABS_MAX` whatever the size
/// of the host window, so the guest's `input_absinfo` is mode independent
/// (the convention of QEMU's virtio/usb tablets).
pub const INPUT_ABS_MAX: i32 = 0x7fff;

/// linux/input-event-codes.h, the subset the backend emits.
pub mod input {
    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_REL: u16 = 0x02;
    pub const EV_ABS: u16 = 0x03;
    pub const SYN_REPORT: u16 = 0;
    pub const REL_X: u16 = 0x00;
    pub const REL_Y: u16 = 0x01;
    pub const REL_HWHEEL: u16 = 0x06;
    pub const REL_WHEEL: u16 = 0x08;
    pub const REL_WHEEL_HI_RES: u16 = 0x0b;
    pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;
    pub const KEY_MAX: u16 = 0x2ff;
}

// ---------------------------------------------------------------------------
// Size assertions (compile-time, no_std compatible)
// ---------------------------------------------------------------------------
//
// These catch accidental padding changes that would break the C header.

const _: () = {
    assert!(size_of::<MsgHeader>() == 16);
    assert!(size_of::<OpenReq>() == 8);
    assert!(size_of::<IoctlReq>() == 24);
    assert!(size_of::<IoctlResp>() == 12);
    assert!(size_of::<MmapReq>() == 24);
    assert!(size_of::<MmapResp>() == 24);
    assert!(size_of::<MunmapReq>() == 8);
    assert!(size_of::<FileEntry>() == 8);
    assert!(size_of::<ScanoutFlip>() == 64);
    assert!(size_of::<ScanoutDisable>() == 8);
    assert!(size_of::<InputEventBatch>() == 8);
    assert!(size_of::<InputEventEntry>() == 8);
    assert!(size_of::<DisplayModeEvent>() == 16);
    assert!(size_of::<CursorUpdate>() == 64);
    assert!(size_of::<ClipboardChunk>() == CLIPBOARD_CHUNK_HEAD);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_types_decode_the_way_the_driver_encodes_them() {
        assert_eq!(DeviceKind::from_device_type(0), Some(DeviceKind::Gpu(0)));
        assert_eq!(DeviceKind::from_device_type(3), Some(DeviceKind::Gpu(3)));
        assert_eq!(DeviceKind::from_device_type(255), Some(DeviceKind::Ctl));
        assert_eq!(DeviceKind::from_device_type(256), Some(DeviceKind::Uvm));
        assert_eq!(
            DeviceKind::from_device_type(257),
            Some(DeviceKind::UvmTools)
        );
        assert_eq!(DeviceKind::from_device_type(258), Some(DeviceKind::Modeset));
    }

    /// A render node is how NVIDIA's Vulkan userspace finds the GPU, so it is
    /// addressable; the gap between the singletons and the render nodes is not.
    #[test]
    fn render_nodes_are_addressable_and_nonsense_is_not() {
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_BASE),
            Some(DeviceKind::Dri(0))
        );
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_BASE + 3),
            Some(DeviceKind::Dri(3))
        );
        assert_eq!(DeviceKind::from_device_type(300), None);
    }

    /// The driver tests `(s32)status < 0`. An unsigned error code stored here
    /// reads back as success and the guest proceeds on a failed call.
    #[test]
    fn an_error_status_is_negative() {
        let h = MsgHeader::err(MsgType::Open, 2);
        assert_eq!(h.status, -2);
        assert!(h.status < 0);
        assert_eq!(MsgHeader::err(MsgType::Open, -2).status, -2);
        assert_eq!(MsgHeader::ok(MsgType::Open, 7).status, 0);
        assert_eq!(MsgHeader::ok(MsgType::Open, 7).handle, 7);
    }

    #[test]
    fn message_types_round_trip() {
        for t in [
            MsgType::Open,
            MsgType::Close,
            MsgType::Ioctl,
            MsgType::Mmap,
            MsgType::Munmap,
            MsgType::GetProcFiles,
            MsgType::GetSysFiles,
            MsgType::EventReady,
            MsgType::ScanoutFlip,
            MsgType::ScanoutDisable,
            MsgType::InputEvent,
            MsgType::DisplayMode,
            MsgType::CursorUpdate,
            MsgType::ClipboardFromHost,
            MsgType::ClipboardToHost,
            MsgType::ClipboardRequest,
            MsgType::GpuCmd,
        ] {
            assert_eq!(MsgType::from_u32(t as u32), Some(t));
        }
        assert_eq!(MsgType::from_u32(0), None);
        assert_eq!(MsgType::from_u32(9), None);
        assert_eq!(MsgType::ScanoutFlip as u32, 20);
        assert_eq!(MsgType::ScanoutDisable as u32, 21);
        assert_eq!(MsgType::InputEvent as u32, 22);
        assert_eq!(MsgType::DisplayMode as u32, 23);
        assert_eq!(MsgType::CursorUpdate as u32, 24);
        assert_eq!(MsgType::ClipboardFromHost as u32, 25);
        assert_eq!(MsgType::ClipboardToHost as u32, 26);
        assert_eq!(MsgType::ClipboardRequest as u32, 27);
        assert_eq!(MsgType::from_u32(28), None);
        assert_eq!(MsgType::from_u32(29), None);
        assert_eq!(MsgType::GpuCmd as u32, 30);
        assert_eq!(MsgType::from_u32(31), None);
        assert_eq!(NVGPU_CFG_VENUS, 1 << 10);
    }

    /// The event the guest's event-queue handler decodes: header, then
    /// {scanout, width, height, refresh_mhz}.
    #[test]
    fn display_mode_encodes_after_a_header() {
        let m = DisplayModeEvent {
            scanout: 0,
            width: 5120,
            height: 1440,
            refresh_mhz: 239_960,
        };
        let mut buf = [0u8; 40];
        assert_eq!(encode_display_mode(&m, &mut buf), Some(32));
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 23);
        assert_eq!(i32::from_le_bytes(buf[8..12].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(buf[20..24].try_into().unwrap()), 5120);
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), 239_960);
        assert_eq!(DisplayModeEvent::from_bytes(&buf[16..32]), Some(m));
        assert_eq!(DisplayModeEvent::from_bytes(&buf[16..31]), None);
        assert_eq!(encode_display_mode(&m, &mut buf[..31]), None);
        let raw: [u8; 16] = unsafe { core::mem::transmute(m) };
        assert_eq!(raw, m.to_bytes());
    }

    /// Field offsets are the C struct's (docs/SCANOUT.md).
    #[test]
    fn cursor_update_matches_the_contract() {
        use core::mem::offset_of;
        assert_eq!(offset_of!(CursorUpdate, width), 4);
        assert_eq!(offset_of!(CursorUpdate, hot_x), 12);
        assert_eq!(offset_of!(CursorUpdate, owner_handle), 20);
        assert_eq!(offset_of!(CursorUpdate, host_handle), 24);
        assert_eq!(offset_of!(CursorUpdate, fourcc), 36);
        assert_eq!(offset_of!(CursorUpdate, modifier), 40);
        assert_eq!(offset_of!(CursorUpdate, crtc_x), 48);
        assert_eq!(offset_of!(CursorUpdate, flags), 56);
        assert_eq!(offset_of!(CursorUpdate, seq), 60);
        let c = CursorUpdate {
            scanout: 0,
            width: 64,
            height: 64,
            hot_x: 3,
            hot_y: 60,
            owner_handle: 9,
            host_handle: 12,
            stride: 256,
            offset: 0,
            fourcc: 0x3432_5241,
            modifier: 0,
            crtc_x: -3,
            crtc_y: 700,
            flags: CURSOR_F_VISIBLE,
            seq: 5,
        };
        let b = c.to_bytes();
        let raw: [u8; 64] = unsafe { core::mem::transmute(c) };
        assert_eq!(b, raw);
        assert_eq!(CursorUpdate::from_bytes(&b), Some(c));
        assert!(c.visible());
        assert!(!CursorUpdate::default().visible());
        assert_eq!(CursorUpdate::from_bytes(&b[..63]), None);
    }

    /// Field offsets are the C struct's (docs/SCANOUT.md).
    #[test]
    fn scanout_flip_matches_the_contract() {
        use core::mem::offset_of;
        assert_eq!(offset_of!(ScanoutFlip, owner_handle), 4);
        assert_eq!(offset_of!(ScanoutFlip, host_handle), 8);
        assert_eq!(offset_of!(ScanoutFlip, fourcc), 28);
        assert_eq!(offset_of!(ScanoutFlip, modifier), 32);
        assert_eq!(offset_of!(ScanoutFlip, seq), 40);
        let f = ScanoutFlip {
            scanout: 0,
            owner_handle: 7,
            host_handle: 3,
            width: 2560,
            height: 1440,
            stride: 10240,
            offset: 0,
            fourcc: 0x3432_5258,
            modifier: 0x0300_0000_0060_6014,
            seq: 0x1_0000_0002,
            reserved: [0; 4],
        };
        let b = f.to_bytes();
        // The struct's in-memory bytes on a little-endian host are the wire.
        let raw: [u8; 64] = unsafe { core::mem::transmute(f) };
        assert_eq!(b, raw);
        assert_eq!(ScanoutFlip::from_bytes(&b), Some(f));
        assert_eq!(ScanoutFlip::from_bytes(&b[..63]), None);
    }

    #[test]
    fn input_events_encode_after_a_header_and_count() {
        let evs = [
            InputEventEntry::new(input::EV_KEY, 30, 1),
            InputEventEntry::new(input::EV_SYN, input::SYN_REPORT, 0),
            InputEventEntry::new(input::EV_REL, input::REL_X, -5),
        ];
        let mut buf = [0u8; 64];
        let n = encode_input_events(&evs, &mut buf).unwrap();
        assert_eq!(n, 16 + 8 + 24);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 22);
        assert_eq!(u32::from_le_bytes(buf[16..20].try_into().unwrap()), 3);
        let third: [u8; 8] = buf[40..48].try_into().unwrap();
        assert_eq!(InputEventEntry::from_bytes(&third), evs[2]);
        assert_eq!(encode_input_events(&evs, &mut buf[..47]), None);
        assert_eq!(input_events_that_fit(16), 0);
        assert_eq!(input_events_that_fit(24), 0);
        assert_eq!(input_events_that_fit(24 + 8 * 5 + 7), 5);
    }

    #[test]
    fn clipboard_chunk_has_the_c_layout_and_round_trips() {
        use core::mem::offset_of;
        assert_eq!(offset_of!(ClipboardChunk, total_len), 8);
        assert_eq!(offset_of!(ClipboardChunk, offset), 12);
        assert_eq!(offset_of!(ClipboardChunk, len), 16);
        assert_eq!(offset_of!(ClipboardChunk, flags), 20);
        assert_eq!(offset_of!(ClipboardChunk, mime), 24);
        let c = ClipboardChunk {
            generation: 0x1122_3344_5566_7788,
            total_len: 10,
            offset: 4,
            len: 6,
            flags: 0,
            mime: clipboard_mime(CLIPBOARD_MIME_TEXT),
        };
        let b = c.to_bytes();
        assert_eq!(&b[0..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(ClipboardChunk::from_bytes(&b), Some(c));
        assert!(c.is_text());
        assert_eq!(ClipboardChunk::from_bytes(&b[..55]), None);
        assert!(!ClipboardChunk::default().is_text());

        let mut out = [0u8; 128];
        let n = encode_clipboard_chunk(MsgType::ClipboardFromHost, &c, b"abc", &mut out).unwrap();
        assert_eq!(n, 16 + 56 + 3);
        assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 25);
        let back = ClipboardChunk::from_bytes(&out[16..]).unwrap();
        assert_eq!(back.len, 3);
        assert_eq!(&out[72..75], b"abc");
        assert!(
            encode_clipboard_chunk(MsgType::ClipboardFromHost, &c, b"abc", &mut out[..74])
                .is_none()
        );
    }
}

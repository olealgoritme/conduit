// crates/protocol/src/venus.rs
//
// The virtio-gpu control commands a `GpuCmd` message carries (docs/VENUS.md).
//
// Layout: `MsgHeader{msg_type = GpuCmd}` | one virtio-gpu command, exactly as
// VIRTIO 1.3 §5.7.6 lays it out. The reply is `MsgHeader` | one virtio-gpu
// response. Only the commands a Venus guest sends (Helios's KMD) are defined
// here; the rest of virtio-gpu (2D resources, transfers, cursor queue) is
// never served. `GET_EDID` is served although no `VIRTIO_GPU_F_EDID` is
// negotiated: the guest sends it and falls back on any error.
//
// Every value is the spec's, checked against Linux's
// include/uapi/linux/virtio_gpu.h, which is what Mesa and the Windows KMD
// were written against. That header and the spec agree on all of these; a
// wrong one here is a guest that gets `RESP_ERR_UNSPEC` for a valid command,
// or worse, a response it decodes as something else.

/// Largest `GpuCmd` request, header included. Venus command streams are
/// batched by the guest, and a larger submit is split there.
pub const GPU_CMD_MAX: usize = 4 << 20;

// ---------------------------------------------------------------------------
// virtio_gpu_ctrl_type (virtio_gpu.h `enum virtio_gpu_ctrl_type`)
// ---------------------------------------------------------------------------

/// 2D commands.
pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub const CMD_GET_CAPSET: u32 = 0x0109;
pub const CMD_GET_EDID: u32 = 0x010a;
pub const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
pub const CMD_SET_SCANOUT_BLOB: u32 = 0x010d;

/// 3D commands.
pub const CMD_CTX_CREATE: u32 = 0x0200;
pub const CMD_CTX_DESTROY: u32 = 0x0201;
pub const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub const CMD_SUBMIT_3D: u32 = 0x0207;
pub const CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
pub const CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;

/// Success responses.
pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const RESP_OK_CAPSET: u32 = 0x1103;
pub const RESP_OK_EDID: u32 = 0x1104;
pub const RESP_OK_MAP_INFO: u32 = 0x1106;

/// Error responses.
pub const RESP_ERR_UNSPEC: u32 = 0x1200;
pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// `virtio_gpu_ctrl_hdr.flags`: complete only once the fence is signalled.
pub const FLAG_FENCE: u32 = 1 << 0;
/// `virtio_gpu_ctrl_hdr.flags`: `ring_idx` is meaningful (`context_init`).
pub const FLAG_INFO_RING_IDX: u32 = 1 << 1;

/// `virtio_gpu_resource_create_blob.blob_mem`.
pub const BLOB_MEM_GUEST: u32 = 0x0001;
pub const BLOB_MEM_HOST3D: u32 = 0x0002;
pub const BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;
/// Conduit's own `blob_mem` (docs/VENUS.md "RM-export blobs"), only with
/// [`crate::messages::NVGPU_CFG_RM_IMPORT`]: the blob is a host GEM object NVK
/// exported through nvidia-drm, named by `blob_id = rm_handle << 32 |
/// gem_handle` (`rm_handle` the backend handle of the render node the guest
/// opened, `gem_handle` what `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` returned on
/// it). `blob_flags` 0, no entries, `size` at most the object's. The resource
/// is imported as a dma-buf and attached to the creating context; it cannot
/// be mapped.
pub const BLOB_MEM_RM_EXPORT: u32 = 0x8000_0001;

/// `blob_id` of a [`BLOB_MEM_RM_EXPORT`] blob: (rm_handle, gem_handle).
pub const fn rm_export_ids(blob_id: u64) -> (u32, u32) {
    ((blob_id >> 32) as u32, blob_id as u32)
}

/// The errno a host refusal of a [`BLOB_MEM_RM_EXPORT`] blob is echoed with,
/// in `padding` of the error response's header (24 bits, little-endian;
/// zero when there is none). Every other response leaves `padding` zero.
pub fn errno_padding(errno: i32) -> [u8; 3] {
    let e = (errno.unsigned_abs() & 0x00ff_ffff).to_le_bytes();
    [e[0], e[1], e[2]]
}

/// `virtio_gpu_resource_create_blob.blob_flags`.
pub const BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;
pub const BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x0004;

/// `virtio_gpu_ctx_create.context_init`: the capset id is the low byte.
pub const CONTEXT_INIT_CAPSET_ID_MASK: u32 = 0x0000_00ff;

/// `VIRTIO_GPU_CAPSET_VENUS`.
pub const CAPSET_VENUS: u32 = 4;

/// `VIRTIO_GPU_MAP_CACHE_*`, the low nibble of `map_info`.
pub const MAP_CACHE_MASK: u32 = 0x0f;
pub const MAP_CACHE_NONE: u32 = 0x00;
pub const MAP_CACHE_CACHED: u32 = 0x01;
pub const MAP_CACHE_UNCACHED: u32 = 0x02;
pub const MAP_CACHE_WC: u32 = 0x03;

/// `VIRTIO_GPU_MAX_SCANOUTS`: `RESP_OK_DISPLAY_INFO` always carries this many.
pub const MAX_SCANOUTS: usize = 16;

/// `enum virtio_gpu_formats`, the scanout formats.
pub mod format {
    pub const B8G8R8A8_UNORM: u32 = 1;
    pub const B8G8R8X8_UNORM: u32 = 2;
    pub const A8R8G8B8_UNORM: u32 = 3;
    pub const X8R8G8B8_UNORM: u32 = 4;
    pub const R8G8B8A8_UNORM: u32 = 67;
    pub const X8B8G8R8_UNORM: u32 = 68;
    pub const A8B8G8R8_UNORM: u32 = 121;
    pub const R8G8B8X8_UNORM: u32 = 134;

    /// The `DRM_FORMAT_*` with the same memory layout, for the viewer. The
    /// virtio names are byte order, DRM's are a little-endian word: virtio
    /// B8G8R8A8 is DRM ARGB8888.
    pub fn drm_fourcc(f: u32) -> Option<u32> {
        let cc = |s: &[u8; 4]| u32::from_le_bytes(*s);
        Some(match f {
            B8G8R8A8_UNORM => cc(b"AR24"),
            B8G8R8X8_UNORM => cc(b"XR24"),
            A8R8G8B8_UNORM => cc(b"BA24"),
            X8R8G8B8_UNORM => cc(b"BX24"),
            R8G8B8A8_UNORM => cc(b"AB24"),
            X8B8G8R8_UNORM => cc(b"RX24"),
            A8B8G8R8_UNORM => cc(b"RA24"),
            R8G8B8X8_UNORM => cc(b"XB24"),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Little-endian field access
// ---------------------------------------------------------------------------

fn w(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn q(b: &[u8], at: usize) -> u64 {
    (w(b, at) as u64) | ((w(b, at + 4) as u64) << 32)
}

fn put_w(o: &mut [u8], at: usize, v: u32) {
    o[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_q(o: &mut [u8], at: usize, v: u64) {
    o[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// virtio_gpu_ctrl_hdr
// ---------------------------------------------------------------------------

/// `struct virtio_gpu_ctrl_hdr`: starts every command and every response.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CtrlHdr {
    /// A `CMD_*` on a request, a `RESP_*` on a response.
    pub ty: u32,
    /// `FLAG_*`.
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    /// Meaningful only with [`FLAG_INFO_RING_IDX`].
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

pub const CTRL_HDR_LEN: usize = 24;

impl CtrlHdr {
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < CTRL_HDR_LEN {
            return None;
        }
        Some(Self {
            ty: w(b, 0),
            flags: w(b, 4),
            fence_id: q(b, 8),
            ctx_id: w(b, 16),
            ring_idx: b[20],
            padding: [b[21], b[22], b[23]],
        })
    }

    pub fn to_bytes(&self) -> [u8; CTRL_HDR_LEN] {
        let mut o = [0u8; CTRL_HDR_LEN];
        put_w(&mut o, 0, self.ty);
        put_w(&mut o, 4, self.flags);
        put_q(&mut o, 8, self.fence_id);
        put_w(&mut o, 16, self.ctx_id);
        o[20] = self.ring_idx;
        o[21..24].copy_from_slice(&self.padding);
        o
    }

    pub fn fenced(&self) -> bool {
        self.flags & FLAG_FENCE != 0
    }

    /// The ring a fence on this command is on: `ring_idx` with
    /// [`FLAG_INFO_RING_IDX`], else ring 0.
    pub fn ring(&self) -> u32 {
        if self.flags & FLAG_INFO_RING_IDX != 0 {
            self.ring_idx as u32
        } else {
            0
        }
    }

    /// The header of the response to `self` with type `ty`. A fenced
    /// command's response echoes its fence, as the spec requires and Linux's
    /// driver checks; `ring_idx` too when the request named one.
    pub fn response(&self, ty: u32) -> Self {
        let mut r = Self {
            ty,
            ..Default::default()
        };
        if self.fenced() {
            r.flags = self.flags & (FLAG_FENCE | FLAG_INFO_RING_IDX);
            r.fence_id = self.fence_id;
            r.ctx_id = self.ctx_id;
            if self.flags & FLAG_INFO_RING_IDX != 0 {
                r.ring_idx = self.ring_idx;
            }
        }
        r
    }
}

/// `struct virtio_gpu_rect`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn read(b: &[u8], at: usize) -> Self {
        Self {
            x: w(b, at),
            y: w(b, at + 4),
            width: w(b, at + 8),
            height: w(b, at + 12),
        }
    }

    fn write(&self, o: &mut [u8], at: usize) {
        for (i, v) in [self.x, self.y, self.width, self.height].iter().enumerate() {
            put_w(o, at + i * 4, *v);
        }
    }
}

// ---------------------------------------------------------------------------
// Requests: every one is a `CtrlHdr` and the body below. `LEN` is the whole
// command, header included; a command of any other length is refused, as
// QEMU does.
// ---------------------------------------------------------------------------

/// A command that is a header and one `resource_id` (and padding):
/// `RESOURCE_UNREF`, `CTX_ATTACH_RESOURCE`, `CTX_DETACH_RESOURCE`,
/// `RESOURCE_UNMAP_BLOB`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceCmd {
    pub hdr: CtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
}

impl ResourceCmd {
    pub const LEN: usize = 32;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            resource_id: w(b, 24),
            padding: w(b, 28),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.resource_id);
        put_w(&mut o, 28, self.padding);
        o
    }
}

/// `struct virtio_gpu_resource_flush`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceFlush {
    pub hdr: CtrlHdr,
    pub r: Rect,
    pub resource_id: u32,
    pub padding: u32,
}

impl ResourceFlush {
    pub const LEN: usize = 48;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            r: Rect::read(b, 24),
            resource_id: w(b, 40),
            padding: w(b, 44),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        self.r.write(&mut o, 24);
        put_w(&mut o, 40, self.resource_id);
        put_w(&mut o, 44, self.padding);
        o
    }
}

/// `struct virtio_gpu_get_capset_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GetCapsetInfo {
    pub hdr: CtrlHdr,
    pub capset_index: u32,
    pub padding: u32,
}

impl GetCapsetInfo {
    pub const LEN: usize = 32;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            capset_index: w(b, 24),
            padding: w(b, 28),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.capset_index);
        put_w(&mut o, 28, self.padding);
        o
    }
}

/// `struct virtio_gpu_get_capset`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GetCapset {
    pub hdr: CtrlHdr,
    pub capset_id: u32,
    pub capset_version: u32,
}

impl GetCapset {
    pub const LEN: usize = 32;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            capset_id: w(b, 24),
            capset_version: w(b, 28),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.capset_id);
        put_w(&mut o, 28, self.capset_version);
        o
    }
}

/// `struct virtio_gpu_ctx_create`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtxCreate {
    pub hdr: CtrlHdr,
    /// Bytes of `debug_name` in use, at most 64.
    pub nlen: u32,
    /// The capset id in the low byte ([`CONTEXT_INIT_CAPSET_ID_MASK`]).
    pub context_init: u32,
    pub debug_name: [u8; 64],
}

impl Default for CtxCreate {
    fn default() -> Self {
        Self {
            hdr: CtrlHdr::default(),
            nlen: 0,
            context_init: 0,
            debug_name: [0; 64],
        }
    }
}

impl CtxCreate {
    pub const LEN: usize = 96;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        let mut debug_name = [0u8; 64];
        debug_name.copy_from_slice(&b[32..96]);
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            nlen: w(b, 24),
            context_init: w(b, 28),
            debug_name,
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.nlen);
        put_w(&mut o, 28, self.context_init);
        o[32..96].copy_from_slice(&self.debug_name);
        o
    }

    pub fn capset_id(&self) -> u32 {
        self.context_init & CONTEXT_INIT_CAPSET_ID_MASK
    }

    /// The name, `nlen` bytes of it, clamped to the field.
    pub fn name(&self) -> &[u8] {
        &self.debug_name[..(self.nlen as usize).min(64)]
    }
}

/// `struct virtio_gpu_cmd_get_edid`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GetEdid {
    pub hdr: CtrlHdr,
    pub scanout: u32,
    pub padding: u32,
}

impl GetEdid {
    pub const LEN: usize = 32;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            scanout: w(b, 24),
            padding: w(b, 28),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.scanout);
        put_w(&mut o, 28, self.padding);
        o
    }
}

/// `CTX_DESTROY` is a bare header.
pub const CTX_DESTROY_LEN: usize = CTRL_HDR_LEN;
/// `GET_DISPLAY_INFO` is a bare header.
pub const GET_DISPLAY_INFO_LEN: usize = CTRL_HDR_LEN;

/// `struct virtio_gpu_cmd_submit`; `size` bytes of command stream follow.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Submit3d {
    pub hdr: CtrlHdr,
    pub size: u32,
    pub padding: u32,
}

impl Submit3d {
    /// Without the command stream.
    pub const LEN: usize = 32;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            size: w(b, 24),
            padding: w(b, 28),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.size);
        put_w(&mut o, 28, self.padding);
        o
    }
}

/// `struct virtio_gpu_resource_create_blob`; `nr_entries` 16-byte
/// `virtio_gpu_mem_entry`s follow (none for `BLOB_MEM_HOST3D`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceCreateBlob {
    pub hdr: CtrlHdr,
    pub resource_id: u32,
    /// `BLOB_MEM_*`; zero is invalid.
    pub blob_mem: u32,
    /// `BLOB_FLAG_*`.
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}

impl ResourceCreateBlob {
    /// Without the memory entries.
    pub const LEN: usize = 56;
    /// One `virtio_gpu_mem_entry`.
    pub const MEM_ENTRY_LEN: usize = 16;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            resource_id: w(b, 24),
            blob_mem: w(b, 28),
            blob_flags: w(b, 32),
            nr_entries: w(b, 36),
            blob_id: q(b, 40),
            size: q(b, 48),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.resource_id);
        put_w(&mut o, 28, self.blob_mem);
        put_w(&mut o, 32, self.blob_flags);
        put_w(&mut o, 36, self.nr_entries);
        put_q(&mut o, 40, self.blob_id);
        put_q(&mut o, 48, self.size);
        o
    }
}

/// `struct virtio_gpu_set_scanout_blob`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetScanoutBlob {
    pub hdr: CtrlHdr,
    pub r: Rect,
    pub scanout_id: u32,
    /// Zero turns the scanout off.
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    /// [`format`].
    pub format: u32,
    pub padding: u32,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
}

impl SetScanoutBlob {
    pub const LEN: usize = 96;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        let four = |at: usize| [w(b, at), w(b, at + 4), w(b, at + 8), w(b, at + 12)];
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            r: Rect::read(b, 24),
            scanout_id: w(b, 40),
            resource_id: w(b, 44),
            width: w(b, 48),
            height: w(b, 52),
            format: w(b, 56),
            padding: w(b, 60),
            strides: four(64),
            offsets: four(80),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        self.r.write(&mut o, 24);
        for (i, v) in [
            self.scanout_id,
            self.resource_id,
            self.width,
            self.height,
            self.format,
            self.padding,
        ]
        .iter()
        .enumerate()
        {
            put_w(&mut o, 40 + i * 4, *v);
        }
        for i in 0..4 {
            put_w(&mut o, 64 + i * 4, self.strides[i]);
            put_w(&mut o, 80 + i * 4, self.offsets[i]);
        }
        o
    }
}

/// `struct virtio_gpu_resource_map_blob`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceMapBlob {
    pub hdr: CtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
    /// Where in the host-visible region (region 3) the guest wants it.
    pub offset: u64,
}

impl ResourceMapBlob {
    pub const LEN: usize = 40;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            resource_id: w(b, 24),
            padding: w(b, 28),
            offset: q(b, 32),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.resource_id);
        put_w(&mut o, 28, self.padding);
        put_q(&mut o, 32, self.offset);
        o
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// One entry of `struct virtio_gpu_resp_display_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayOne {
    pub r: Rect,
    pub enabled: u32,
    pub flags: u32,
}

/// `struct virtio_gpu_resp_display_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RespDisplayInfo {
    pub hdr: CtrlHdr,
    pub pmodes: [DisplayOne; MAX_SCANOUTS],
}

impl RespDisplayInfo {
    pub const LEN: usize = CTRL_HDR_LEN + MAX_SCANOUTS * 24;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        let mut pmodes = [DisplayOne::default(); MAX_SCANOUTS];
        for (i, p) in pmodes.iter_mut().enumerate() {
            let at = CTRL_HDR_LEN + i * 24;
            *p = DisplayOne {
                r: Rect::read(b, at),
                enabled: w(b, at + 16),
                flags: w(b, at + 20),
            };
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            pmodes,
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        for (i, p) in self.pmodes.iter().enumerate() {
            let at = CTRL_HDR_LEN + i * 24;
            p.r.write(&mut o, at);
            put_w(&mut o, at + 16, p.enabled);
            put_w(&mut o, at + 20, p.flags);
        }
        o
    }
}

/// `struct virtio_gpu_resp_capset_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RespCapsetInfo {
    pub hdr: CtrlHdr,
    pub capset_id: u32,
    pub capset_max_version: u32,
    pub capset_max_size: u32,
    pub padding: u32,
}

impl RespCapsetInfo {
    pub const LEN: usize = 40;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            capset_id: w(b, 24),
            capset_max_version: w(b, 28),
            capset_max_size: w(b, 32),
            padding: w(b, 36),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.capset_id);
        put_w(&mut o, 28, self.capset_max_version);
        put_w(&mut o, 32, self.capset_max_size);
        put_w(&mut o, 36, self.padding);
        o
    }
}

/// `struct virtio_gpu_resp_capset` is a header and the capset's bytes.
pub const RESP_CAPSET_HEAD: usize = CTRL_HDR_LEN;

/// `struct virtio_gpu_resp_edid`: `size` bytes of EDID, the rest of the
/// 1024 zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RespEdid {
    pub hdr: CtrlHdr,
    pub size: u32,
    pub padding: u32,
    pub edid: [u8; RespEdid::EDID_MAX],
}

impl Default for RespEdid {
    fn default() -> Self {
        Self {
            hdr: CtrlHdr::default(),
            size: 0,
            padding: 0,
            edid: [0; Self::EDID_MAX],
        }
    }
}

impl RespEdid {
    /// The `edid` array.
    pub const EDID_MAX: usize = 1024;
    pub const LEN: usize = CTRL_HDR_LEN + 8 + Self::EDID_MAX;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        let mut edid = [0u8; Self::EDID_MAX];
        edid.copy_from_slice(&b[32..Self::LEN]);
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            size: w(b, 24),
            padding: w(b, 28),
            edid,
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.size);
        put_w(&mut o, 28, self.padding);
        o[32..].copy_from_slice(&self.edid);
        o
    }
}

/// `struct virtio_gpu_resp_map_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RespMapInfo {
    pub hdr: CtrlHdr,
    /// `MAP_CACHE_*`.
    pub map_info: u32,
    pub padding: u32,
}

impl RespMapInfo {
    pub const LEN: usize = 32;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            hdr: CtrlHdr::from_bytes(b)?,
            map_info: w(b, 24),
            padding: w(b, 28),
        })
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut o = [0u8; Self::LEN];
        o[..CTRL_HDR_LEN].copy_from_slice(&self.hdr.to_bytes());
        put_w(&mut o, 24, self.map_info);
        put_w(&mut o, 28, self.padding);
        o
    }
}

const _: () = {
    assert!(size_of::<CtrlHdr>() == CTRL_HDR_LEN);
    assert!(size_of::<Rect>() == 16);
    assert!(size_of::<ResourceCmd>() == ResourceCmd::LEN);
    assert!(size_of::<ResourceFlush>() == ResourceFlush::LEN);
    assert!(size_of::<GetCapsetInfo>() == GetCapsetInfo::LEN);
    assert!(size_of::<GetCapset>() == GetCapset::LEN);
    assert!(size_of::<CtxCreate>() == CtxCreate::LEN);
    assert!(size_of::<Submit3d>() == Submit3d::LEN);
    assert!(size_of::<ResourceCreateBlob>() == ResourceCreateBlob::LEN);
    assert!(size_of::<SetScanoutBlob>() == SetScanoutBlob::LEN);
    assert!(size_of::<ResourceMapBlob>() == ResourceMapBlob::LEN);
    assert!(size_of::<DisplayOne>() == 24);
    assert!(size_of::<RespDisplayInfo>() == RespDisplayInfo::LEN);
    assert!(size_of::<RespDisplayInfo>() == 408);
    assert!(size_of::<RespCapsetInfo>() == RespCapsetInfo::LEN);
    assert!(size_of::<RespMapInfo>() == RespMapInfo::LEN);
    assert!(size_of::<GetEdid>() == GetEdid::LEN);
    assert!(size_of::<RespEdid>() == RespEdid::LEN);
    assert!(size_of::<RespEdid>() == 1056);
};

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// The values Linux's include/uapi/linux/virtio_gpu.h gives, written out
    /// again rather than derived, so a typo on either side shows.
    #[test]
    fn command_and_response_values_are_the_specs() {
        assert_eq!(CMD_GET_DISPLAY_INFO, 256);
        assert_eq!(CMD_RESOURCE_UNREF, 258);
        assert_eq!(CMD_RESOURCE_FLUSH, 260);
        assert_eq!(CMD_GET_CAPSET_INFO, 264);
        assert_eq!(CMD_GET_CAPSET, 265);
        assert_eq!(CMD_GET_EDID, 266);
        assert_eq!(CMD_RESOURCE_CREATE_BLOB, 268);
        assert_eq!(CMD_SET_SCANOUT_BLOB, 269);
        assert_eq!(CMD_CTX_CREATE, 512);
        assert_eq!(CMD_CTX_DESTROY, 513);
        assert_eq!(CMD_CTX_ATTACH_RESOURCE, 514);
        assert_eq!(CMD_CTX_DETACH_RESOURCE, 515);
        assert_eq!(CMD_SUBMIT_3D, 519);
        assert_eq!(CMD_RESOURCE_MAP_BLOB, 520);
        assert_eq!(CMD_RESOURCE_UNMAP_BLOB, 521);
        assert_eq!(RESP_OK_NODATA, 4352);
        assert_eq!(RESP_OK_DISPLAY_INFO, 4353);
        assert_eq!(RESP_OK_CAPSET_INFO, 4354);
        assert_eq!(RESP_OK_CAPSET, 4355);
        assert_eq!(RESP_OK_EDID, 4356);
        assert_eq!(RESP_OK_MAP_INFO, 4358);
        assert_eq!(RESP_ERR_UNSPEC, 4608);
        assert_eq!(RESP_ERR_INVALID_SCANOUT_ID, 4610);
        assert_eq!(RESP_ERR_INVALID_RESOURCE_ID, 4611);
        assert_eq!(RESP_ERR_INVALID_CONTEXT_ID, 4612);
        assert_eq!(RESP_ERR_INVALID_PARAMETER, 4613);
        assert_eq!((FLAG_FENCE, FLAG_INFO_RING_IDX), (1, 2));
        assert_eq!(BLOB_MEM_HOST3D, 2);
        assert_eq!(CAPSET_VENUS, 4);
    }

    #[test]
    fn ctrl_hdr_has_the_spec_layout() {
        assert_eq!(offset_of!(CtrlHdr, flags), 4);
        assert_eq!(offset_of!(CtrlHdr, fence_id), 8);
        assert_eq!(offset_of!(CtrlHdr, ctx_id), 16);
        assert_eq!(offset_of!(CtrlHdr, ring_idx), 20);
        let h = CtrlHdr {
            ty: CMD_SUBMIT_3D,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: 0x1122_3344_5566_7788,
            ctx_id: 9,
            ring_idx: 3,
            padding: [0; 3],
        };
        let b = h.to_bytes();
        let raw: [u8; 24] = unsafe { core::mem::transmute(h) };
        assert_eq!(b, raw);
        assert_eq!(CtrlHdr::from_bytes(&b), Some(h));
        assert_eq!(CtrlHdr::from_bytes(&b[..23]), None);
        assert_eq!(h.ring(), 3);
        assert_eq!(
            CtrlHdr {
                flags: FLAG_FENCE,
                ..h
            }
            .ring(),
            0
        );
    }

    /// A fenced command's response carries the fence back; an unfenced one's
    /// is a bare type.
    #[test]
    fn responses_echo_the_fence() {
        let h = CtrlHdr {
            ty: CMD_SUBMIT_3D,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: 77,
            ctx_id: 2,
            ring_idx: 1,
            padding: [0; 3],
        };
        let r = h.response(RESP_OK_NODATA);
        assert_eq!(
            (r.ty, r.flags, r.fence_id, r.ctx_id, r.ring_idx),
            (RESP_OK_NODATA, 3, 77, 2, 1)
        );
        let r = CtrlHdr { flags: 0, ..h }.response(RESP_ERR_UNSPEC);
        assert_eq!(
            r,
            CtrlHdr {
                ty: RESP_ERR_UNSPEC,
                ..Default::default()
            }
        );
    }

    /// Every struct encodes to its own in-memory bytes (the wire, on a
    /// little-endian host) and decodes back; one byte short is refused.
    #[test]
    fn commands_round_trip_as_their_c_layout() {
        let hdr = CtrlHdr {
            ty: 1,
            flags: FLAG_FENCE,
            fence_id: 5,
            ctx_id: 6,
            ring_idx: 0,
            padding: [0; 3],
        };
        macro_rules! round_trip {
            ($t:ty, $v:expr) => {{
                let v: $t = $v;
                let b = v.to_bytes();
                let raw: [u8; <$t>::LEN] = unsafe { core::mem::transmute(v) };
                assert_eq!(b, raw, stringify!($t));
                assert_eq!(<$t>::from_bytes(&b), Some(v), stringify!($t));
                assert_eq!(<$t>::from_bytes(&b[..<$t>::LEN - 1]), None);
            }};
        }
        round_trip!(
            ResourceCmd,
            ResourceCmd {
                hdr,
                resource_id: 7,
                padding: 0
            }
        );
        round_trip!(
            ResourceFlush,
            ResourceFlush {
                hdr,
                r: Rect {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4
                },
                resource_id: 7,
                padding: 0
            }
        );
        round_trip!(
            GetCapsetInfo,
            GetCapsetInfo {
                hdr,
                capset_index: 1,
                padding: 0
            }
        );
        round_trip!(
            GetCapset,
            GetCapset {
                hdr,
                capset_id: 4,
                capset_version: 0
            }
        );
        let mut name = [0u8; 64];
        name[..5].copy_from_slice(b"dxvk!");
        round_trip!(
            CtxCreate,
            CtxCreate {
                hdr,
                nlen: 5,
                context_init: CAPSET_VENUS,
                debug_name: name
            }
        );
        round_trip!(
            Submit3d,
            Submit3d {
                hdr,
                size: 4096,
                padding: 0
            }
        );
        round_trip!(
            ResourceCreateBlob,
            ResourceCreateBlob {
                hdr,
                resource_id: 9,
                blob_mem: BLOB_MEM_HOST3D,
                blob_flags: BLOB_FLAG_USE_MAPPABLE,
                nr_entries: 0,
                blob_id: 0xdead_beef_0000_0001,
                size: 1 << 20
            }
        );
        round_trip!(
            SetScanoutBlob,
            SetScanoutBlob {
                hdr,
                r: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080
                },
                scanout_id: 0,
                resource_id: 9,
                width: 1920,
                height: 1080,
                format: format::B8G8R8X8_UNORM,
                padding: 0,
                strides: [7680, 0, 0, 0],
                offsets: [0, 1, 2, 3]
            }
        );
        round_trip!(
            ResourceMapBlob,
            ResourceMapBlob {
                hdr,
                resource_id: 9,
                padding: 0,
                offset: 1 << 21
            }
        );
        round_trip!(
            RespCapsetInfo,
            RespCapsetInfo {
                hdr,
                capset_id: 4,
                capset_max_version: 0,
                capset_max_size: 160,
                padding: 0
            }
        );
        round_trip!(
            RespMapInfo,
            RespMapInfo {
                hdr,
                map_info: MAP_CACHE_CACHED,
                padding: 0
            }
        );
        let mut di = RespDisplayInfo {
            hdr,
            ..Default::default()
        };
        di.pmodes[0] = DisplayOne {
            r: Rect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
            enabled: 1,
            flags: 0,
        };
        di.pmodes[15].flags = 0xabcd;
        round_trip!(RespDisplayInfo, di);
        round_trip!(
            GetEdid,
            GetEdid {
                hdr,
                scanout: 3,
                padding: 0
            }
        );
        let mut e = RespEdid {
            hdr,
            size: 256,
            ..Default::default()
        };
        e.edid[1] = 0xff;
        e.edid[1023] = 0x5a;
        round_trip!(RespEdid, e);
    }

    /// Field offsets the kernel's structs have.
    #[test]
    fn field_offsets_are_the_kernels() {
        assert_eq!(offset_of!(ResourceCreateBlob, blob_mem), 28);
        assert_eq!(offset_of!(ResourceCreateBlob, nr_entries), 36);
        assert_eq!(offset_of!(ResourceCreateBlob, blob_id), 40);
        assert_eq!(offset_of!(ResourceCreateBlob, size), 48);
        assert_eq!(offset_of!(SetScanoutBlob, scanout_id), 40);
        assert_eq!(offset_of!(SetScanoutBlob, format), 56);
        assert_eq!(offset_of!(SetScanoutBlob, strides), 64);
        assert_eq!(offset_of!(SetScanoutBlob, offsets), 80);
        assert_eq!(offset_of!(ResourceMapBlob, offset), 32);
        assert_eq!(offset_of!(CtxCreate, context_init), 28);
        assert_eq!(offset_of!(CtxCreate, debug_name), 32);
        assert_eq!(offset_of!(RespCapsetInfo, capset_max_size), 32);
        assert_eq!(offset_of!(RespMapInfo, map_info), 24);
        assert_eq!(offset_of!(GetEdid, scanout), 24);
        assert_eq!(offset_of!(RespEdid, size), 24);
        assert_eq!(offset_of!(RespEdid, edid), 32);
    }

    #[test]
    fn ctx_create_reads_capset_and_name() {
        let mut c = CtxCreate {
            nlen: 200,
            context_init: 0x0000_0104,
            ..Default::default()
        };
        c.debug_name[..3].copy_from_slice(b"abc");
        assert_eq!(c.capset_id(), CAPSET_VENUS);
        assert_eq!(c.name().len(), 64, "nlen is clamped to the field");
        c.nlen = 3;
        assert_eq!(c.name(), b"abc");
    }

    #[test]
    fn scanout_formats_map_to_drm() {
        let cc = |s: &[u8; 4]| u32::from_le_bytes(*s);
        assert_eq!(
            format::drm_fourcc(format::B8G8R8X8_UNORM),
            Some(cc(b"XR24"))
        );
        assert_eq!(
            format::drm_fourcc(format::B8G8R8A8_UNORM),
            Some(cc(b"AR24"))
        );
        assert_eq!(
            format::drm_fourcc(format::R8G8B8A8_UNORM),
            Some(cc(b"AB24"))
        );
        assert_eq!(format::drm_fourcc(0), None);
        assert_eq!(format::drm_fourcc(5), None);
    }
}

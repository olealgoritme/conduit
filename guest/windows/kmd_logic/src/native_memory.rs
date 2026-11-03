//! Pure contracts for NVIDIA Windows allocation private data and fixed GPU mappings.
//!
//! The I/O half is `kmd_render/src/virtio/native_memory.rs`.  This module does
//! not dereference guest pointers, issue RM requests, or choose a GPU address.
//! In particular a WDDM segment address is only VidMm's placement cookie; it is
//! never reinterpreted as a host physical address.

extern crate alloc;

use alloc::vec::Vec;

pub const ALLOCATION_PRIVATE_BYTES: usize = 586;
pub const ALLOCATION_PRIVATE_VERSION: u32 = 0x0001_0004;
pub const ALLOCATION_UNIT: u64 = 64 * 1024;
pub const PAGE_BYTES: u64 = 4096;
pub const GPU_ADDRESS_BITS: u32 = 44;
pub const GPU_ADDRESS_LIMIT: u64 = 1u64 << GPU_ADDRESS_BITS;
pub const SYSTEM_MEMORY_SEGMENT_ID: u32 = 0;

pub const SYSTEM_MEMORY_AT: usize = 0xc8;
pub const SIZE_UNITS_AT: usize = 0xd0;
/// D3D (NVDX) records leave `SIZE_UNITS_AT` zero and describe the size at
/// 0x110: whole 64-KiB units in the low 16 bits and the remainder in 256-byte
/// steps in the top byte (0xd000_0000 is mapped as 0xd000 bytes, 0x4000_001d as
/// 0x1e units); bits 16..24 are not size (0x000c_0000 got one page from
/// NVIDIA's KMD). 0x114 can carry a larger count. Vulkan records put the
/// integer at `SIZE_UNITS_AT` and a nonzero fraction at 0x114 (or repeat the
/// count there). Checked against every mapped allocation of the 610.88 D3D11,
/// D3D12, Vulkan and CUDA captures (384 of 384).
pub const SIZE_FRACTION_AT: usize = 0x10c;
pub const SIZE_INTEGER_AT: usize = 0x110;
pub const SIZE_COUNT_AT: usize = 0x114;

/// Size in 64-KiB units, rounded up, from an NVDA allocation record.
pub fn allocation_units(bytes: &[u8]) -> Option<u64> {
    if is_nvgl_record(bytes) {
        // Vulkan: 64-KiB units at 0xd0 and a 1/16-unit fraction at 0xcc.
        let (units, _) = nvgl_size(bytes)?;
        return Some(u64::from(units).max(1));
    }
    let units = u64::from(get32(bytes, SIZE_UNITS_AT)?);
    let count = u64::from(get32(bytes, SIZE_COUNT_AT)?);
    if units != 0 {
        // CUDA: count 0. Vulkan: a fraction, or the count repeated.
        return Some(units + u64::from(count != 0 && count != units));
    }
    let encoded = get32(bytes, SIZE_INTEGER_AT)?;
    let integer = u64::from(encoded & 0xffff);
    let remainder = encoded >> 24;
    // D3D: the size is `count` units plus `remainder` x 256 bytes (a 124x228
    // B8G8R8A8 staging texture: count 1, remainder 0xd0 = 118784 bytes; its
    // third mip faulted past a one-unit allocation). The captured KMD's 0xd0
    // output is that rounded up (0x1d + 0x40 -> 0x1e, 0x29 + 0xc0 -> 0x2a).
    let fraction = u64::from(remainder != 0);
    // A record with only flag bits still describes an allocation (one page).
    Some(
        (count + if count != 0 { fraction } else { 0 })
            .max(integer + fraction)
            .max(u64::from(encoded != 0)),
    )
}
pub const KIND_AT: usize = 0x1a0;
pub const TILING_AT: usize = 0x1a4;
pub const COMPRESSION_AT: usize = 0x1a8;

/// The only allocation shape implemented by the native service: linear,
/// uncompressed memory.  These are the values in the 610.88 v0x10004 record.
pub const KIND_LINEAR: u32 = 0;
pub const TILING_LINEAR: u32 = 0;
pub const COMPRESSION_NONE: u32 = 0;

/// Every field the native KMD writes after RM has really created the object.
/// No field is written speculatively before the host allocation succeeds.
pub const WRITEBACK_OFFSETS: [usize; 16] = [
    0x38, 0x3c, 0x40, 0x8c, 0xc0, 0xd4, 0xd8, 0xe0, 0xf4, 0xf8, 0xfc, 0x108, 0x10c, 0x120, 0x1ac,
    0x1b0,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateError {
    Short,
    BadMagic,
    BadVersion,
    BadDeclaredSize,
    SizeZero,
    SizeOverflow,
    SystemMemoryAlignment,
    UnsupportedKind(u32),
    UnsupportedTiling(u32),
    UnsupportedCompression(u32),
}

/// Whether the private buffer belongs to the native NVDA allocation ABI.
/// A matching magic with another version is still NVDA, and must be refused by
/// the native path rather than accidentally interpreted as a Helios blob.
pub fn is_nvda_allocation(bytes: &[u8]) -> bool {
    get32(bytes, 0) == Some(crate::nvda::NVDA_MAGIC)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocationPrivate {
    pub system_memory: u64,
    pub size: u64,
    pub kind: u32,
    pub tiling: u32,
    pub compression: u32,
}

impl AllocationPrivate {
    pub const fn system_backed(self) -> bool {
        self.system_memory != 0
    }
}

pub fn parse_allocation_private(bytes: &[u8]) -> Result<AllocationPrivate, PrivateError> {
    if bytes.len() < ALLOCATION_PRIVATE_BYTES {
        return Err(PrivateError::Short);
    }
    if get32(bytes, 0) != Some(crate::nvda::NVDA_MAGIC) {
        return Err(PrivateError::BadMagic);
    }
    if get32(bytes, 4) != Some(ALLOCATION_PRIVATE_VERSION) {
        return Err(PrivateError::BadVersion);
    }
    // The record carries its byte size in the common NVDA header.  Some UMD
    // builds leave it zero; a nonzero value must describe the captured record.
    if let Some(declared) = get32(bytes, 8).filter(|v| *v != 0) {
        if declared as usize != ALLOCATION_PRIVATE_BYTES {
            return Err(PrivateError::BadDeclaredSize);
        }
    }
    let units = allocation_units(bytes).ok_or(PrivateError::Short)?;
    let size = units
        .checked_mul(ALLOCATION_UNIT)
        .ok_or(PrivateError::SizeOverflow)?;
    if size == 0 {
        return Err(PrivateError::SizeZero);
    }
    // 44 bits is both the advertised WDDM geometry and the native service's
    // hard limit.  Checking it here prevents a later VA + size wrap.
    if size >= GPU_ADDRESS_LIMIT {
        return Err(PrivateError::SizeOverflow);
    }
    // Vulkan (`NVGL`) records use +0xc8/+0xcc for their size (+0xcc the fraction,
    // which read as the high half made a 0x0040000000000000 "system address" the
    // KMD then failed to pin): they carry no system-memory pointer there. Neither
    // do CUDA records: +0xcc is 0x20 in all of them (read as the high half: a
    // 0x20_0000_0000 "system address", so every cuCtxCreate allocation failed
    // INVALID_PARAMETER); CUDA hands its own memory over as pSystemMem.
    let system_memory = if is_nvgl_record(bytes) || is_cuda_record(bytes) {
        0
    } else {
        get64(bytes, SYSTEM_MEMORY_AT).ok_or(PrivateError::Short)?
    };
    if system_memory != 0 && system_memory & (PAGE_BYTES - 1) != 0 {
        return Err(PrivateError::SystemMemoryAlignment);
    }
    let kind = get32(bytes, KIND_AT).ok_or(PrivateError::Short)?;
    let tiling = get32(bytes, TILING_AT).ok_or(PrivateError::Short)?;
    let compression = get32(bytes, COMPRESSION_AT).ok_or(PrivateError::Short)?;
    if kind != KIND_LINEAR {
        return Err(PrivateError::UnsupportedKind(kind));
    }
    if tiling != TILING_LINEAR {
        return Err(PrivateError::UnsupportedTiling(tiling));
    }
    if compression != COMPRESSION_NONE {
        return Err(PrivateError::UnsupportedCompression(compression));
    }
    Ok(AllocationPrivate {
        system_memory,
        size,
        kind,
        tiling,
        compression,
    })
}

/// Values returned by the real creation, in `WRITEBACK_OFFSETS` order.
/// Keeping the values together makes it impossible for the I/O half to update
/// only the easy prefix and leave stale type/size metadata behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivateWriteback {
    pub values: [u32; 16],
}

impl PrivateWriteback {
    /// Capture-backed output layout.  Size fields use the same 64-KiB units as
    /// the request; the input at +0xd0 remains the requested unit count.
    pub fn new(
        memory: u32,
        client: u32,
        device: u32,
        size: u64,
        kind: u32,
        attr: u32,
        attr2: u32,
        va_space: u32,
    ) -> Option<Self> {
        if size == 0 || size % ALLOCATION_UNIT != 0 {
            return None;
        }
        let units = u32::try_from(size / ALLOCATION_UNIT).ok()?;
        Some(Self {
            values: [
                memory,
                client,
                device,
                kind,
                units,
                attr,
                attr2,
                1 << 8, // 4-KiB MAP_MEMORY_DMA pages, never a large-page cap
                units,
                0,
                0,
                ALLOCATION_UNIT as u32,
                1 << 8,
                ALLOCATION_UNIT as u32,
                va_space,
                0, // no NV50_MEMORY_VIRTUAL exists until VidMm supplies a VA
            ],
        })
    }
}

/// Whether the UMD created the allocation shareable (D3DKMT CreateShared):
/// +0x20 bit 15 together with +0x24 bit 1. That holds for all 34 shared and 1
/// of 492 unshared records (a D3D12 texture the UMD never locks) of the 610.88
/// captures and a shared-texture sweep; either bit alone also marks locked
/// unshared D3D12 buffers. dxgkrnl refuses a shareable CpuVisible allocation
/// unless its segment set is aperture-only ("VerifySupportedSegmentSetAndAdjustFlags
/// failed").
pub fn is_shared_record(bytes: &[u8]) -> bool {
    let usage = get32(bytes, SHARED_USAGE_AT).unwrap_or(0);
    let bind = get32(bytes, SHARED_BIND_AT).unwrap_or(0);
    usage & SHARED_USAGE_BIT != 0 && bind & SHARED_BIND_BIT != 0
}
pub const SHARED_USAGE_AT: usize = 0x20;
pub const SHARED_USAGE_BIT: u32 = 1 << 15;
pub const SHARED_BIND_AT: usize = 0x24;
pub const SHARED_BIND_BIT: u32 = 1 << 1;

/// Whether an NVDA allocation record is the D3D11/D3D12 UMDs' (its size is
/// in the fixed-point fields at 0x10c..0x114, not in 64-KiB units at 0xd0).
pub fn is_d3d_record(bytes: &[u8]) -> bool {
    get32(bytes, SIZE_UNITS_AT) == Some(0) && !is_nvgl_record(bytes)
}

/// The Vulkan ICD's records carry the tag `NVGL` ("LGVN" at +0x0c).
pub const RECORD_TAG_AT: usize = 0x0c;
pub const RECORD_TAG_NVGL: u32 = u32::from_le_bytes(*b"LGVN");

pub fn is_nvgl_record(bytes: &[u8]) -> bool {
    get32(bytes, RECORD_TAG_AT) == Some(RECORD_TAG_NVGL)
}

/// A Vulkan record's size as the 610.88 KMD rounds it: (64-KiB units, the fraction
/// at +0xcc rounded up to 1/16 unit; for an image kind (+0x10 bit 3) up to a whole
/// unit); from 2 MiB on, a fraction rounds to a whole unit (0x3f units +
/// 0xc0000000 -> 0x40 units). An untyped record (+0x10 = 0) has no fraction: its
/// +0xcc is not size and stays as the UMD wrote it.
fn nvgl_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let (units, fraction) = nvgl_requested(bytes)?;
    let kind = get32(bytes, 0x10)?;
    if kind == 0 {
        return Some((units, 0));
    }
    let step: u64 = if kind & NVGL_KIND_IMAGE != 0 { 1 << 32 } else { 0x1000_0000 };
    let rounded = u64::from(fraction).div_ceil(step) * step;
    let (mut units, mut fraction) = if rounded > u64::from(u32::MAX) {
        (units.checked_add(1)?, 0)
    } else {
        (units, rounded as u32)
    };
    if units >= 0x20 && fraction != 0 {
        units = units.checked_add(1)?;
        fraction = 0;
    }
    Some((units, fraction))
}

/// +0x116 of an NVGL record: the byte size the ICD asks for (u64, unaligned).
/// In every record of the vktest capture it is exactly +0xd0 x 64 KiB + the
/// +0xcc fraction (images leave it 0 and carry the size in +0x112). For an
/// allocation of 64 MiB and more the ICD backs the memory with 32-MiB chunks
/// (nvoglv64 0x180a7ffd0, DRS chunk size 32 MiB): it sets +0x116 to the chunk's
/// size and leaves the record's +0xd0/+0xcc at the whole allocation, so the KMD
/// that sizes by +0xd0 created one 64-MiB allocation where the ICD expected a
/// 32-MiB chunk, and the ICD's mapping of the second chunk then named no
/// allocation (dxgkrnl: "Invalid hAllocation", VK_ERROR_OUT_OF_DEVICE_MEMORY).
pub const NVGL_BYTE_SIZE_AT: usize = 0x116;

/// Whether an NVGL record is a chunk: its +0x116 byte size is a whole number of
/// 64-KiB units, smaller than what +0xd0/+0xcc describe.
pub fn nvgl_size_from_bytes(bytes: &[u8]) -> bool {
    let Some(bytes_size) = get64(bytes, NVGL_BYTE_SIZE_AT)
        .filter(|b| *b >= ALLOCATION_UNIT && *b % ALLOCATION_UNIT == 0)
    else {
        return false;
    };
    let units = u64::from(get32(bytes, SIZE_UNITS_AT).unwrap_or(0));
    let fraction = u64::from(get32(bytes, 0xcc).unwrap_or(0));
    bytes_size < units * ALLOCATION_UNIT + (fraction * ALLOCATION_UNIT >> 32)
}

/// (64-KiB units, 2^-32-unit fraction) the ICD asks for: +0x116 for a chunk
/// (`nvgl_size_from_bytes`), else +0xd0/+0xcc.
fn nvgl_requested(bytes: &[u8]) -> Option<(u32, u32)> {
    if nvgl_size_from_bytes(bytes) {
        let size = get64(bytes, NVGL_BYTE_SIZE_AT)?;
        let units = u32::try_from(size / ALLOCATION_UNIT).ok()?;
        let fraction = ((size % ALLOCATION_UNIT) << 16) as u32;
        return Some((units, fraction));
    }
    Some((get32(bytes, SIZE_UNITS_AT)?, get32(bytes, 0xcc)?))
}

/// +0x10 bit 3 of an NVGL record: an image (the swap-chain images and the
/// vktest 16x8 image of the capture), with its width at +0x30 and its bytes per
/// pixel in the 16-bit word at +0x6e.
const NVGL_KIND_IMAGE: u32 = 0x8;

pub const NVGL_WRITEBACK_FIELDS: usize = 20;

/// +0xbc bit 20 of an NVGL record (the ICD's first allocation, 0x01100000):
/// NVIDIA's KMD adds 0x80000 to +0x108 and +0x120 for it, and only for it.
const NVGL_BC_ATTR_80000: u32 = 0x0010_0000;

/// Fields of a Vulkan (`NVGL`) record the 610.88 KMD writes, checked byte for
/// byte against all 75 NVGL records of the vktest capture (all 586 bytes): the
/// same frame as a D3D record's, with its own size fields (+0xcc/+0xd0/+0xd4/
/// +0xd8, and the 1/16-unit rounding of a fraction mirrored at +0x114),
/// +0x8c = 0x7810, +0xc0 | 0x4000, the page/placement bits in +0xe0, +0xc8 and
/// +0x10c: 0x10000000 for a typed buffer (+0x10c | 0x10 from 2 MiB of the wide
/// kind), an image's row pitch in the same 2^-32-of-64-KiB fixed point (width x
/// bytes per pixel, << 16), the UMD's own value for an untyped record; +0x108 and
/// +0x120 keep the UMD's value except for +0xbc bit 20; +0x1ac = +0x1b0 =
/// 0x60000. +0xdc is the UMD's input and is left alone.
///
/// A record asking for compression (+0xdc bit 29) is answered uncompressed:
/// NVIDIA's KMD grants it (bit 29 copied to +0x11c, +0x108/+0x120 | 0x20000,
/// +0x1ac = 0x80000, a compression tag line at +0x1bc for images), but the
/// memory behind it here is linear and uncompressed, and the ICD accepts the
/// ungranted answer (the dispatch-loop buffers are allocated and locked).
pub fn nvgl_writeback(input: &[u8]) -> Option<[(usize, u32); NVGL_WRITEBACK_FIELDS]> {
    let kind = get32(input, 0x10)?;
    let bc = get32(input, 0xbc)?;
    let (units, fraction) = nvgl_size(input)?;
    let last = (units + u32::from(fraction != 0)).saturating_sub(1);
    let typed = kind != 0;
    let wide = kind & 0x20_0000 != 0;
    let image = kind & NVGL_KIND_IMAGE != 0;
    // Page bits: 0x280 for the narrow kind; for the wide kind 0x180 from 2 MiB,
    // 0x100 from 8 units, for an image, or with +0xbc bits 30 and 24 both set
    // (0xc10d0000; 0xc00d0000 and 0x01100000 stay 0x80), else 0x80.
    let page = if typed && !wide {
        0x280
    } else if wide && units >= 0x20 {
        0x180
    } else if wide && (units >= 8 || image || bc & 0x4100_0000 == 0x4100_0000) {
        0x100
    } else {
        0x80
    };
    let e0 = get32(input, 0xe0)? | page;
    let c8 = if image {
        let width = get32(input, 0x30)?;
        let bytes_per_pixel = get32(input, 0x6c)? >> 16;
        width.wrapping_mul(bytes_per_pixel) << 16
    } else if typed {
        0x1000_0000
    } else {
        get32(input, 0xc8)?
    };
    let x10c = if image {
        c8
    } else if typed {
        0x1000_0000 | if wide && units >= 0x20 { 0x10 } else { 0 }
    } else {
        get32(input, 0x10c)?
    };
    let attr = if bc & NVGL_BC_ATTR_80000 != 0 { 0x8_0000 } else { 0 };
    let x108 = get32(input, 0x108)? | attr;
    let x120 = get32(input, 0x120)? | attr;
    let input_fraction = get32(input, 0xcc)?;
    let mut x114 = get32(input, 0x114)?;
    if typed && x114 != 0 && x114 == input_fraction {
        // The fraction's mirror rounds to 1/16 unit, without the whole-unit
        // rounding of images or of 2 MiB and up (0xc0000000 stays, 0x02000000
        // of the 16x8 image becomes 0x10000000).
        let rounded = u64::from(x114).div_ceil(0x1000_0000) * 0x1000_0000;
        if let Ok(rounded) = u32::try_from(rounded) {
            x114 = rounded;
        }
    }
    let cc = if typed { fraction } else { input_fraction };
    Some([
        (0x38, 1),
        (0x3c, 1),
        (0x40, 1),
        (0x8c, 0x7810),
        (0xc0, get32(input, 0xc0)? | 0x4000),
        (0xc8, c8),
        (0xcc, cc),
        (0xd0, units),
        (0xd4, fraction.wrapping_sub(0x1_0000)),
        (0xd8, last),
        (0xe0, e0),
        (0xf4, 0xffff_0000),
        (0xf8, 0xffff_ffff),
        (0xfc, 0xffff),
        (0x108, x108),
        (0x10c, x10c),
        (0x114, x114),
        (0x120, x120),
        (0x1ac, 0x6_0000),
        (0x1b0, 0x6_0000),
    ])
}

/// The allocation alignment NVIDIA's 610.88 KMD gives a Vulkan record, from the
/// page bits its writeback reports at +0xe0: 64 KiB for 0x100 (64-KiB pages),
/// 2 MiB for 0x180 (2-MiB pages); `None` (4 KiB) otherwise. In the vktest
/// capture every VA VidMm handed out for a 0x100 record was 64-KiB aligned and
/// every one for a 0x180 record 2-MiB aligned, while 0x80 records landed on
/// 4-KiB boundaries.
pub fn nvgl_alignment(input: &[u8]) -> Option<u32> {
    if !is_nvgl_record(input) {
        return None;
    }
    let fields = nvgl_writeback(input)?;
    let e0 = fields.iter().find(|(at, _)| *at == 0xe0)?.1;
    match e0 & 0x180 {
        0x100 => Some(0x1_0000),
        0x180 => Some(0x20_0000),
        _ => None,
    }
}

pub const CUDA_WRITEBACK_FIELDS: usize = 16;

/// +0x10 of a CUDA record: set on every allocation the CUDA driver backs with
/// its own user memory (D3DKMT `pSystemMem`; 38 of 90 records in the two
/// cudatest captures, all of them and only them), clear (0x800) on video memory.
pub const CUDA_KIND_AT: usize = 0x10;
pub const CUDA_KIND_SYSTEM_MEMORY: u32 = 0x0100_0000;

/// Whether a CUDA record describes memory the UMD supplies (`pSystemMem`): the
/// KMD reports it as existing system memory in an aperture segment.
pub fn is_cuda_system_memory(bytes: &[u8]) -> bool {
    is_cuda_record(bytes)
        && get32(bytes, CUDA_KIND_AT).is_some_and(|kind| kind & CUDA_KIND_SYSTEM_MEMORY != 0)
}

/// Whether an NVDA record is the CUDA driver's (`**VN` tag, size in 64-KiB units
/// at 0xd0): neither a D3D nor a Vulkan record.
pub fn is_cuda_record(bytes: &[u8]) -> bool {
    get32(bytes, RECORD_TAG_AT) == Some(crate::nvda::NVDA_TAG_NV)
        && get32(bytes, SIZE_UNITS_AT).is_some_and(|units| units != 0)
}

/// Fields of a CUDA record the 610.88 KMD writes (all 90 records of the two
/// cudatest captures, 15 input shapes): 1 at +0x38/+0x3c/+0x40, 0x7810 at +0x8c,
/// +0xc0 | 0x4000, the unit count at +0xd0 is kept and +0xd8 = count - 1, the
/// page bits in +0xe0 and +0x10c by kind (+0x10 bit 11: 0x180 and 0x10, else
/// 0x280 and +0x10c unchanged), +0x108/+0x120 kept when set and 0x80000
/// otherwise, 0x60000 at +0x1ac and +0x1b0. Nothing else changes.
pub fn cuda_writeback(input: &[u8]) -> Option<[(usize, u32); CUDA_WRITEBACK_FIELDS]> {
    let kind = get32(input, 0x10)?;
    let units = get32(input, SIZE_UNITS_AT).filter(|units| *units != 0)?;
    let wide = kind & 0x800 != 0;
    let page = if wide { 0x180 } else { 0x280 };
    let x10c = if wide { 0x10 } else { get32(input, 0x10c)? };
    let or_default = |at: usize| get32(input, at).map(|v| if v == 0 { 0x8_0000 } else { v });
    Some([
        (0x38, 1),
        (0x3c, 1),
        (0x40, 1),
        (0x8c, 0x7810),
        (0xc0, get32(input, 0xc0)? | 0x4000),
        (0xd4, 0xffff_0000),
        (0xd8, units - 1),
        (0xe0, get32(input, 0xe0)? | page),
        (0xf4, 0xffff_0000),
        (0xf8, 0xffff_ffff),
        (0xfc, 0xffff),
        (0x108, or_default(0x108)?),
        (0x10c, x10c),
        (0x120, or_default(0x120)?),
        (0x1ac, 0x6_0000),
        (0x1b0, 0x6_0000),
    ])
}

/// Fields of a D3D record the KMD writes, as the 610.88 KMD does (every D3D11
/// and D3D12 record of the 2026-10-31 captures; the inputs at 0x8c and
/// 0x10c..0x120 stay as the UMD wrote them). The UMD sizes the VA range it
/// reserves for the allocation from 0xd0 (64-KiB units): a zero there was a
/// zero-size ReserveGpuVirtualAddress and a removed D3D12 device.
pub const D3D_WRITEBACK_FIELDS: usize = 17;

pub fn d3d_writeback(input: &[u8], units: u32) -> Option<[(usize, u32); D3D_WRITEBACK_FIELDS]> {
    let bytes_size = get32(input, 0x10c)?;
    let integer = get32(input, SIZE_INTEGER_AT)?;
    let count = get32(input, SIZE_COUNT_AT)?;
    let kind = get32(input, 0x120)?;
    // Three record shapes: byte-sized (0x114 = 0), unit-sized (0x10c = 0) and
    // both (a texture: 0x110 = 0, or a fixed-point count in 0x110).
    let texture = bytes_size != 0 && integer == 0 && count != 0;
    let (cc, d0, d4) = if count == 0 {
        (integer, 0, integer.wrapping_sub(0x1_0000))
    } else {
        (integer & 0x3fff_ffff, units, 0xffff_0000)
    };
    let c0 = if bytes_size == 0 {
        0x4000
    } else if texture {
        (bytes_size >> 2) | 0x4000
    } else {
        bytes_size | 0x4000
    };
    let c4 = if texture {
        bytes_size >> 2
    } else if count == 0 {
        if bytes_size == 0x40_0000 { 0x8_0000 } else { 0x1_0000 }
    } else {
        0x1_0000 | (cc & 0xffff)
    };
    Some([
        (0x38, 1),
        (0x3c, 1),
        (0x40, 1),
        (0xb8, 0x2_0000),
        (0xc0, c0),
        (0xc4, c4),
        (0xc8, bytes_size),
        (0xcc, cc),
        (0xd0, d0),
        (0xd4, d4),
        (0xd8, d0.saturating_sub(1)),
        (0xdc, if texture { 0x4_0000 } else { 0 }),
        (0xe0, 0x280),
        (0xf4, 0xffff_0000),
        (0xf8, 0xffff_ffff),
        (0xfc, 0xffff),
        (0x108, kind),
    ])
}

/// Apply [`d3d_writeback`] fields.
pub fn apply_fields(bytes: &mut [u8], fields: &[(usize, u32)]) -> Result<(), PrivateError> {
    if bytes.len() < ALLOCATION_PRIVATE_BYTES {
        return Err(PrivateError::Short);
    }
    for (offset, value) in fields {
        put32(bytes, *offset, *value).ok_or(PrivateError::Short)?;
    }
    Ok(())
}

pub fn writeback_allocation_private(
    bytes: &mut [u8],
    writeback: &PrivateWriteback,
) -> Result<(), PrivateError> {
    if bytes.len() < ALLOCATION_PRIVATE_BYTES {
        return Err(PrivateError::Short);
    }
    for (offset, value) in WRITEBACK_OFFSETS.iter().zip(writeback.values) {
        put32(bytes, *offset, value).ok_or(PrivateError::Short)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingPiece {
    /// Exact VidMm-selected GPU virtual address.  Never supplied by RM.
    pub va: u64,
    /// Byte offset in the allocation's RM memory object.
    pub memory_offset: u64,
    pub len: u64,
}

impl MappingPiece {
    pub fn end(self) -> Option<u64> {
        self.va.checked_add(self.len)
    }

    pub fn memory_end(self) -> Option<u64> {
        self.memory_offset.checked_add(self.len)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressError {
    ZeroLength,
    Unaligned,
    VirtualOverflow,
    VirtualOutsideGeometry,
    MemoryOverflow,
    MemoryOutsideAllocation,
    Overlap,
    NotFound,
}

pub fn validate_piece(piece: MappingPiece, allocation_size: u64) -> Result<(), AddressError> {
    if piece.len == 0 {
        return Err(AddressError::ZeroLength);
    }
    if piece.va & (PAGE_BYTES - 1) != 0
        || piece.memory_offset & (PAGE_BYTES - 1) != 0
        || piece.len & (PAGE_BYTES - 1) != 0
    {
        return Err(AddressError::Unaligned);
    }
    let va_end = piece
        .va
        .checked_add(piece.len)
        .ok_or(AddressError::VirtualOverflow)?;
    if va_end > GPU_ADDRESS_LIMIT {
        return Err(AddressError::VirtualOutsideGeometry);
    }
    let memory_end = piece
        .memory_offset
        .checked_add(piece.len)
        .ok_or(AddressError::MemoryOverflow)?;
    if memory_end > allocation_size {
        return Err(AddressError::MemoryOutsideAllocation);
    }
    Ok(())
}

pub fn ranges_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    a_start < b_start.saturating_add(b_len) && b_start < a_start.saturating_add(a_len)
}

/// Return the sub-range of `piece` left after removing `[va, va + len)`.
/// Zero, one, or two pieces result; the two-piece case is the host-side split
/// used by a partial unbind or replacement.
pub fn subtract_range(
    piece: MappingPiece,
    va: u64,
    len: u64,
) -> Result<Vec<MappingPiece>, AddressError> {
    if len == 0 {
        return Err(AddressError::ZeroLength);
    }
    let end = va.checked_add(len).ok_or(AddressError::VirtualOverflow)?;
    let piece_end = piece.end().ok_or(AddressError::VirtualOverflow)?;
    if !ranges_overlap(piece.va, piece.len, va, len) {
        return Ok(alloc::vec![piece]);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(2)
        .map_err(|_| AddressError::MemoryOverflow)?;
    let left_end = piece.va.max(va).min(piece_end);
    if piece.va < left_end {
        out.push(MappingPiece {
            va: piece.va,
            memory_offset: piece.memory_offset,
            len: left_end - piece.va,
        });
    }
    let right_start = end.max(piece.va).min(piece_end);
    if right_start < piece_end {
        out.push(MappingPiece {
            va: right_start,
            memory_offset: piece.memory_offset + (right_start - piece.va),
            len: piece_end - right_start,
        });
    }
    Ok(out)
}

/// Result of replacing one VA range in an allocation's fixed-map set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub pieces: Vec<MappingPiece>,
    /// True when at least one old mapping with different contents was removed.
    /// The I/O half permits this only after the address space was made idle.
    pub replaces_live_mapping: bool,
}

/// Apply a map or unmap to a sorted, non-overlapping fixed-map set.
/// `mapped == None` is an unmap.  The operation affects exactly the requested
/// VA range and preserves the memory offsets of both split tails.
pub fn replace_range(
    current: &[MappingPiece],
    va: u64,
    len: u64,
    mapped: Option<MappingPiece>,
    allocation_size: u64,
) -> Result<Replacement, AddressError> {
    let update_end = va.checked_add(len).ok_or(AddressError::VirtualOverflow)?;
    if len == 0 || va & (PAGE_BYTES - 1) != 0 || len & (PAGE_BYTES - 1) != 0 {
        return Err(AddressError::Unaligned);
    }
    if update_end > GPU_ADDRESS_LIMIT {
        return Err(AddressError::VirtualOutsideGeometry);
    }
    if let Some(piece) = mapped {
        validate_piece(piece, allocation_size)?;
        if piece.va != va || piece.len != len {
            return Err(AddressError::Unaligned);
        }
    }

    let mut pieces = Vec::new();
    pieces
        .try_reserve_exact(current.len() + 2)
        .map_err(|_| AddressError::MemoryOverflow)?;
    for &piece in current {
        validate_piece(piece, allocation_size)?;
        if ranges_overlap(piece.va, piece.len, va, len) {
            for tail in subtract_range(piece, va, len)? {
                pieces.push(tail);
            }
        } else {
            pieces.push(piece);
        }
    }
    if let Some(piece) = mapped {
        pieces.push(piece);
    }
    pieces.sort_unstable_by_key(|piece| piece.va);
    for pair in pieces.windows(2) {
        if pair[0].end().ok_or(AddressError::VirtualOverflow)? > pair[1].va {
            return Err(AddressError::Overlap);
        }
    }
    let replaces_live_mapping = live_replacement(current, &pieces, va, len);
    Ok(Replacement {
        pieces,
        replaces_live_mapping,
    })
}

/// Whether applying the update changes any previously mapped byte.  Fresh
/// mappings and idempotent updates are not live replacements; unmaps are.
pub fn live_replacement(
    before: &[MappingPiece],
    after: &[MappingPiece],
    va: u64,
    len: u64,
) -> bool {
    let location = |set: &[MappingPiece], address: u64| {
        set.iter().find_map(|piece| {
            if address >= piece.va && address < piece.end()? {
                Some(piece.memory_offset + (address - piece.va))
            } else {
                None
            }
        })
    };
    (0..len / PAGE_BYTES).any(|page| {
        let address = va + page * PAGE_BYTES;
        let old = location(before, address);
        old.is_some() && old != location(after, address)
    })
}

/// Coalesce adjacent mappings of the same memory object.  This is what keeps a
/// sequence of 4-KiB WDDM updates from consuming one RM handle per page.
pub fn coalesce(pieces: &mut Vec<MappingPiece>) {
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable_by_key(|piece| piece.va);
    let mut out = 0;
    for read in 0..pieces.len() {
        let piece = pieces[read];
        if out != 0 {
            let previous = &mut pieces[out - 1];
            if previous.end() == Some(piece.va)
                && previous.memory_end() == Some(piece.memory_offset)
            {
                previous.len += piece.len;
                continue;
            }
        }
        pieces[out] = piece;
        out += 1;
    }
    pieces.truncate(out);
}

pub fn get32(bytes: &[u8], at: usize) -> Option<u32> {
    let s = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn get64(bytes: &[u8], at: usize) -> Option<u64> {
    let s = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn put32(bytes: &mut [u8], at: usize, value: u32) -> Option<()> {
    bytes
        .get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// `NVOS46_PARAMETERS` with a nonzero memory offset.  The stock copy-engine
/// helper always maps offset zero; native partial mappings need this field.
pub fn nvos46_range(
    root: u32,
    device: u32,
    dma: u32,
    memory: u32,
    memory_offset: u64,
    len: u64,
    flags: u32,
) -> [u8; crate::rm_ce_channel::NVOS46_BYTES] {
    let mut out = crate::rm_ce_channel::nvos46(&crate::rm_ce_channel::DmaMap {
        root,
        h_device: device,
        h_dma: dma,
        h_memory: memory,
        length: len,
        flags,
    });
    out[16..24].copy_from_slice(&memory_offset.to_le_bytes());
    out
}

/// `NVOS47_PARAMETERS` for a whole `NV50_MEMORY_VIRTUAL` mapping.
pub fn nvos47_unmap(
    root: u32,
    device: u32,
    dma: u32,
    memory: u32,
    va: u64,
) -> [u8; crate::rm_ce_channel::NVOS47_BYTES] {
    crate::rm_ce_channel::nvos47(
        &crate::rm_ce_channel::DmaMap {
            root,
            h_device: device,
            h_dma: dma,
            h_memory: memory,
            length: 0,
            flags: 0,
        },
        va,
    )
}

/// The picture a D3D allocation record describes, as the 610.88 UMD lays it
/// out: `+0x30` width and `+0x34` height in texels, and for a 2D block-linear
/// surface `+0x12a` log2 GOBs per block in y, `+0x132` the row pitch in 64-byte
/// GOBs and `+0x136` the height in blocks. DWM's 5120x1440 primary answers 4,
/// 320 (20480 bytes) and 12, a 50x50 texture 2, 4 and 2: always enough blocks
/// for the height, and 480 x 64 KiB, exactly what the record sizes, for the
/// primary. A record with no GOB pitch is not a block-linear 2D surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct D3dSurface {
    pub width: u32,
    pub height: u32,
    pub log2_gobs_y: u32,
    /// Row pitch in bytes.
    pub pitch: u32,
}

pub const SURFACE_WIDTH_AT: usize = 0x30;
pub const SURFACE_HEIGHT_AT: usize = 0x34;
pub const SURFACE_LOG2_Y_AT: usize = 0x128;
pub const SURFACE_PITCH_GOBS_AT: usize = 0x130;
pub const SURFACE_BLOCKS_Y_AT: usize = 0x134;
const GOB_BYTES: u32 = 64;
const GOB_ROWS: u32 = 8;
/// The deepest 2D block a DRM modifier names (32 GOBs).
const MAX_LOG2_GOBS_Y: u32 = 5;

pub fn d3d_surface(bytes: &[u8]) -> Option<D3dSurface> {
    if !is_d3d_record(bytes) {
        return None;
    }
    let width = get32(bytes, SURFACE_WIDTH_AT)?;
    let height = get32(bytes, SURFACE_HEIGHT_AT)?;
    let log2_gobs_y = get32(bytes, SURFACE_LOG2_Y_AT)? >> 16;
    let pitch_gobs = get32(bytes, SURFACE_PITCH_GOBS_AT)? >> 16;
    let blocks_y = get32(bytes, SURFACE_BLOCKS_Y_AT)? >> 16;
    if width == 0 || height == 0 || pitch_gobs == 0 || blocks_y == 0 {
        return None;
    }
    if log2_gobs_y > MAX_LOG2_GOBS_Y {
        return None;
    }
    let pitch = pitch_gobs.checked_mul(GOB_BYTES)?;
    let rows = blocks_y.checked_mul(GOB_ROWS << log2_gobs_y)?;
    if rows < height {
        return None;
    }
    Some(D3dSurface {
        width,
        height,
        log2_gobs_y,
        pitch,
    })
}

/// Bytes a block-linear [`D3dSurface`] spans: pitch x height rounded up to whole blocks.
pub fn d3d_surface_bytes(s: &D3dSurface) -> u64 {
    let block_rows = u64::from(GOB_ROWS << s.log2_gobs_y);
    let rows = u64::from(s.height).div_ceil(block_rows) * block_rows;
    rows * u64::from(s.pitch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private(size_units: u32, system_memory: u64) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = [0u8; ALLOCATION_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&crate::nvda::NVDA_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&ALLOCATION_PRIVATE_VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&(ALLOCATION_PRIVATE_BYTES as u32).to_le_bytes());
        b[SYSTEM_MEMORY_AT..SYSTEM_MEMORY_AT + 8].copy_from_slice(&system_memory.to_le_bytes());
        b[SIZE_UNITS_AT..SIZE_UNITS_AT + 4].copy_from_slice(&size_units.to_le_bytes());
        b
    }

    fn d3d_record(at: &[(usize, u32)]) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = [0u8; ALLOCATION_PRIVATE_BYTES];
        for (o, v) in at {
            b[*o..*o + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn field(f: &[(usize, u32)], at: usize) -> u32 {
        f.iter().find(|(o, _)| *o == at).map(|(_, v)| *v).unwrap()
    }

    fn nvgl_record(kind: u32, units: u32, fraction: u32, bytes_size: u64) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = d3d_record(&[(RECORD_TAG_AT, RECORD_TAG_NVGL), (0x10, kind), (SIZE_UNITS_AT, units), (0xcc, fraction)]);
        b[NVGL_BYTE_SIZE_AT..NVGL_BYTE_SIZE_AT + 8].copy_from_slice(&bytes_size.to_le_bytes());
        b
    }

    #[test]
    fn nvgl_chunk_records_are_sized_by_their_byte_size() {
        // Captured shapes agree: +0x116 = units x 64 KiB + the fraction's bytes.
        for (units, fraction, bytes_size) in [(0x11, 0x22c0_0000, 0x11_22c0u64), (0x40, 0, 0x40_0000), (0x3, 0x8000_0000, 0x3_8000)] {
            let b = nvgl_record(0x220_0800, units, fraction, bytes_size);
            assert!(!nvgl_size_from_bytes(&b));
        }
        // Images leave +0x116 zero.
        assert!(!nvgl_size_from_bytes(&nvgl_record(0x220_0808, 0x14, 0, 0)));
        // A 32-MiB chunk of a 64-MiB allocation: +0xd0 says 0x400 units, +0x116 32 MiB.
        let chunk = nvgl_record(0x220_0800, 0x400, 0, 32 << 20);
        assert!(nvgl_size_from_bytes(&chunk));
        assert_eq!(allocation_units(&chunk), Some(0x200));
        let f = nvgl_writeback(&chunk).unwrap();
        assert_eq!(f.iter().find(|(at, _)| *at == 0xd0).unwrap().1, 0x200);
        // Without the chunk the whole size stays.
        assert_eq!(allocation_units(&nvgl_record(0x220_0800, 0x400, 0, 64 << 20)), Some(0x400));
    }

    #[test]
    fn shared_records_are_recognised() {
        // Shared texture (sweep), DWM primary.
        assert!(is_shared_record(&d3d_record(&[(0x20, 0x8011), (0x24, 0x23)])));
        assert!(is_shared_record(&d3d_record(&[(0x20, 0x8015), (0x24, 0x2_0063)])));
        // The same texture unshared, and a locked D3D12 buffer with bit 15.
        assert!(!is_shared_record(&d3d_record(&[(0x20, 0x11), (0x24, 0)])));
        assert!(!is_shared_record(&d3d_record(&[(0x20, 0x200_8010), (0x24, 0x800_0021)])));
        assert!(!is_shared_record(&d3d_record(&[])));
    }

    #[test]
    fn d3d_writeback_matches_the_captured_kmd() {
        // D3D12 texture record (capture seq 2079): 256 KiB, reserved as 0x40000.
        let b = d3d_record(&[(0x10c, 0x400_0000), (0x114, 4), (0x120, 0x4_0002)]);
        assert!(is_d3d_record(&b));
        let f = d3d_writeback(&b, 4).unwrap();
        for (at, want) in [(0xc0, 0x100_4000), (0xc4, 0x100_0000), (0xc8, 0x400_0000), (0xcc, 0),
            (0xd0, 4), (0xd4, 0xffff_0000), (0xd8, 3), (0xdc, 0x4_0000), (0x108, 0x4_0002)] {
            assert_eq!(field(&f, at), want, "texture +{at:#x}");
        }
        // Unit-sized: 0x110 = 0x114 = 2.
        let b = d3d_record(&[(0x110, 2), (0x114, 2), (0x120, 0x9_0000)]);
        let f = d3d_writeback(&b, 2).unwrap();
        for (at, want) in [(0xc0, 0x4000), (0xc4, 0x1_0002), (0xc8, 0), (0xcc, 2), (0xd0, 2),
            (0xd4, 0xffff_0000), (0xd8, 1), (0x108, 0x9_0000)] {
            assert_eq!(field(&f, at), want, "units +{at:#x}");
        }
        // Byte-sized: 0x10c = 0x110 = 1 GiB.
        let b = d3d_record(&[(0x10c, 0x4000_0000), (0x110, 0x4000_0000), (0x120, 0x9_0000)]);
        let f = d3d_writeback(&b, 0x4000).unwrap();
        for (at, want) in [(0xc0, 0x4000_4000), (0xc4, 0x1_0000), (0xc8, 0x4000_0000),
            (0xcc, 0x4000_0000), (0xd0, 0), (0xd4, 0x3fff_0000), (0xd8, 0)] {
            assert_eq!(field(&f, at), want, "bytes +{at:#x}");
        }
        // Fixed point: 0x110 = 0x4000001d (0x1d units and a fraction).
        let b = d3d_record(&[(0x10c, 0x4000_0000), (0x110, 0x4000_001d), (0x114, 0x1d)]);
        let f = d3d_writeback(&b, 0x1e).unwrap();
        for (at, want) in [(0xc4, 0x1_001d), (0xcc, 0x1d), (0xd0, 0x1e), (0xd8, 0x1d)] {
            assert_eq!(field(&f, at), want, "fixed +{at:#x}");
        }
        // The inputs are not among the written fields.
        for at in [0x8c, 0x10c, 0x110, 0x114, 0x120] {
            assert!(f.iter().all(|(o, _)| *o != at));
        }
        // A CUDA record is not D3D.
        assert!(!is_d3d_record(&d3d_record(&[(SIZE_UNITS_AT, 1)])));
    }

    #[test]
    fn parses_586_byte_v10004_and_64k_units() {
        let local = parse_allocation_private(&private(3, 0)).unwrap();
        assert_eq!(local.size, 3 * ALLOCATION_UNIT);
        assert!(!local.system_backed());
        let system = parse_allocation_private(&private(1, 0x12000)).unwrap();
        assert!(system.system_backed());
        assert_eq!(system.system_memory, 0x12000);
        assert!(matches!(
            parse_allocation_private(&private(0, 0)),
            Err(PrivateError::SizeZero)
        ));
        assert!(matches!(
            parse_allocation_private(&private(1, 0x12001)),
            Err(PrivateError::SystemMemoryAlignment)
        ));
    }

    #[test]
    fn rejects_unsupported_shape_before_service_can_allocate() {
        for (at, expected) in [
            (KIND_AT, PrivateError::UnsupportedKind(1)),
            (TILING_AT, PrivateError::UnsupportedTiling(1)),
            (COMPRESSION_AT, PrivateError::UnsupportedCompression(1)),
        ] {
            let mut b = private(1, 0);
            b[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
            assert_eq!(parse_allocation_private(&b), Err(expected));
        }
    }

    #[test]
    fn writes_every_capture_backed_output_offset() {
        let wb =
            PrivateWriteback::new(0x111, 0x222, 0x333, 0x40000, 0, 0x1900_0000, 6, 0x444).unwrap();
        let mut b = private(4, 0);
        writeback_allocation_private(&mut b, &wb).unwrap();
        for (offset, expected) in WRITEBACK_OFFSETS.iter().zip(wb.values) {
            assert_eq!(get32(&b, *offset), Some(expected), "offset {offset:#x}");
        }
        assert_eq!(get32(&b, SIZE_UNITS_AT), Some(4));
        assert_eq!(get32(&b, 0x1b0), Some(0));
        assert!(PrivateWriteback::new(1, 2, 3, 0, 0, 0, 0, 4).is_none());
        assert!(PrivateWriteback::new(1, 2, 3, 8u64 << 32, 0, 0, 0, 4).is_some());
    }

    #[test]
    fn fixed_mapping_arithmetic_has_no_rm_chosen_address() {
        let p = MappingPiece {
            va: 0x8000_0000,
            memory_offset: 0x20000,
            len: 0x30000,
        };
        assert_eq!(validate_piece(p, 0x80000), Ok(()));
        assert_eq!(p.end(), Some(0x8003_0000));
        assert_eq!(p.memory_end(), Some(0x50000));
        assert_eq!(
            validate_piece(
                MappingPiece {
                    va: GPU_ADDRESS_LIMIT - 0x1000,
                    memory_offset: 0,
                    len: 0x2000
                },
                0x2000
            ),
            Err(AddressError::VirtualOutsideGeometry)
        );
        assert_eq!(
            validate_piece(
                MappingPiece {
                    va: 0,
                    memory_offset: 0x70000,
                    len: 0x2000
                },
                0x80000
            ),
            Ok(())
        );
        assert_eq!(
            validate_piece(
                MappingPiece {
                    va: 0,
                    memory_offset: 0x70001,
                    len: 0x1000
                },
                0x80000
            ),
            Err(AddressError::Unaligned)
        );
    }

    #[test]
    fn partial_unbind_preserves_both_split_tails() {
        let whole = MappingPiece {
            va: 0x10000,
            memory_offset: 0x40000,
            len: 0x50000,
        };
        let r = replace_range(&[whole], 0x20000, 0x10000, None, 0x90000).unwrap();
        assert!(r.replaces_live_mapping);
        assert_eq!(
            r.pieces,
            alloc::vec![
                MappingPiece {
                    va: 0x10000,
                    memory_offset: 0x40000,
                    len: 0x10000
                },
                MappingPiece {
                    va: 0x30000,
                    memory_offset: 0x60000,
                    len: 0x30000
                },
            ]
        );
    }

    #[test]
    fn remap_requires_idle_only_when_live_contents_change() {
        let old = MappingPiece {
            va: 0x20000,
            memory_offset: 0,
            len: 0x2000,
        };
        let same = replace_range(
            &[old],
            0x20000,
            0x1000,
            Some(MappingPiece {
                va: 0x20000,
                memory_offset: 0,
                len: 0x1000,
            }),
            0x4000,
        )
        .unwrap();
        assert!(!same.replaces_live_mapping);
        let changed = replace_range(
            &[old],
            0x20000,
            0x1000,
            Some(MappingPiece {
                va: 0x20000,
                memory_offset: 0x1000,
                len: 0x1000,
            }),
            0x4000,
        )
        .unwrap();
        assert!(changed.replaces_live_mapping);
        assert_eq!(
            changed.pieces,
            alloc::vec![
                MappingPiece {
                    va: 0x20000,
                    memory_offset: 0x1000,
                    len: 0x1000
                },
                MappingPiece {
                    va: 0x21000,
                    memory_offset: 0x1000,
                    len: 0x1000
                },
            ]
        );
    }

    #[test]
    fn coalesces_only_contiguous_va_and_memory() {
        let mut p = alloc::vec![
            MappingPiece {
                va: 0x3000,
                memory_offset: 0x2000,
                len: 0x1000
            },
            MappingPiece {
                va: 0x1000,
                memory_offset: 0,
                len: 0x1000
            },
            MappingPiece {
                va: 0x2000,
                memory_offset: 0x1000,
                len: 0x1000
            },
            MappingPiece {
                va: 0x4000,
                memory_offset: 0x8000,
                len: 0x1000
            },
        ];
        coalesce(&mut p);
        assert_eq!(
            p,
            alloc::vec![
                MappingPiece {
                    va: 0x1000,
                    memory_offset: 0,
                    len: 0x3000
                },
                MappingPiece {
                    va: 0x4000,
                    memory_offset: 0x8000,
                    len: 0x1000
                },
            ]
        );
    }

    #[test]
    fn range_parameter_blocks_use_real_offsets_and_exact_va() {
        let map = nvos46_range(1, 2, 3, 4, 0x12000, 0x3000, 0x100);
        assert_eq!(get64(&map, 16), Some(0x12000));
        assert_eq!(get64(&map, 24), Some(0x3000));
        let unmap = nvos47_unmap(1, 2, 3, 4, 0x8001_2000);
        assert_eq!(get64(&unmap, 24), Some(0x8001_2000));
    }
}


#[cfg(test)]
mod size_rule_tests {
    use super::*;

    fn record() -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = [0u8; ALLOCATION_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&crate::nvda::NVDA_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&ALLOCATION_PRIVATE_VERSION.to_le_bytes());
        b
    }
    fn set(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn cuda_units_at_d0() {
        let mut b = record();
        set(&mut b, SIZE_UNITS_AT, 0x40);
        assert_eq!(allocation_units(&b), Some(0x40));
    }

    #[test]
    fn vulkan_fraction_or_repeated_count() {
        let mut b = record();
        set(&mut b, SIZE_UNITS_AT, 0x11);
        set(&mut b, SIZE_COUNT_AT, 0x22c0_0000);
        assert_eq!(allocation_units(&b), Some(0x12));
        set(&mut b, SIZE_COUNT_AT, 0x11);
        assert_eq!(allocation_units(&b), Some(0x11));
    }

    #[test]
    fn d3d_fixed_point_from_the_capture() {
        // 0x1d4000 bytes: integer 0x1d (flag bits above), fraction .25, count 0x1d.
        let mut b = record();
        set(&mut b, SIZE_FRACTION_AT, 0x4000_0000);
        set(&mut b, SIZE_INTEGER_AT, 0x4000_001d);
        set(&mut b, SIZE_COUNT_AT, 0x1d);
        assert_eq!(allocation_units(&b), Some(0x1e));
        // The first D3D11 allocation (GPFIFO): one unit.
        let mut g = record();
        set(&mut g, SIZE_INTEGER_AT, 1);
        set(&mut g, SIZE_COUNT_AT, 1);
        assert_eq!(allocation_units(&g), Some(1));
        assert_eq!(parse_allocation_private(&g).map(|p| p.size), Ok(ALLOCATION_UNIT));
    }

    #[test]
    fn d3d_sizes_mapped_in_the_capture() {
        // (0x10c, 0x110, 0x114) -> 64-KiB units covering the mapped size.
        for (fraction, encoded, count, units) in [
            (0xd000_0000, 0xd000_0000, 0, 1),     // mapped 0xd000
            (0x0040_0000, 0xb000_0000, 0, 1),     // mapped 0xb000
            (0x8100_0000, 0xc000_0029, 0x29, 0x2a), // mapped 0x2a0000
            (0xc000_0000, 0xc000_003f, 0x3f, 0x40), // mapped 0x400000
            (0xf200_0000, 0x0000_000f, 0x10, 0x10), // mapped 0x100000
            (0x0a00_0000, 0, 0x14, 0x14),           // mapped 0x140000
            (0x0040_0000, 0, 1, 1),                 // mapped 0x10000
            (0, 0x100, 0x100, 0x100),               // mapped 0x1000000
            // Flag bits only: NVIDIA's KMD answered one 4-KiB page; reading
            // them as units asked RM for 48 GiB.
            (0x000c_0000, 0x000c_0000, 0, 1),
        ] {
            let mut b = record();
            set(&mut b, SIZE_FRACTION_AT, fraction);
            set(&mut b, SIZE_INTEGER_AT, encoded);
            set(&mut b, SIZE_COUNT_AT, count);
            assert_eq!(allocation_units(&b), Some(units), "{encoded:#x}");
        }
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;

    fn record(w: u32, h: u32, log2y: u32, pitch_gobs: u32, blocks_y: u32) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = [0u8; ALLOCATION_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&crate::nvda::NVDA_MAGIC.to_le_bytes());
        put32(&mut b, SURFACE_WIDTH_AT, w).unwrap();
        put32(&mut b, SURFACE_HEIGHT_AT, h).unwrap();
        put32(&mut b, SURFACE_LOG2_Y_AT, log2y << 16).unwrap();
        put32(&mut b, SURFACE_PITCH_GOBS_AT, pitch_gobs << 16).unwrap();
        put32(&mut b, SURFACE_BLOCKS_Y_AT, blocks_y << 16).unwrap();
        b
    }

    #[test]
    fn dwm_primary_is_block_linear_16_gobs() {
        // 610.88 DWM primary: +0x128 0x40000, +0x130 0x1400000, +0x134 0xc0000, 480 units.
        let s = d3d_surface(&record(5120, 1440, 4, 320, 12)).unwrap();
        assert_eq!(s, D3dSurface { width: 5120, height: 1440, log2_gobs_y: 4, pitch: 20480 });
        assert_eq!(d3d_surface_bytes(&s), 480 * ALLOCATION_UNIT);
    }

    #[test]
    fn small_texture_uses_shallow_blocks() {
        // 50x50 B8G8R8A8: +0x128 0x20000, +0x130 0x40000, +0x134 0x20000.
        let s = d3d_surface(&record(50, 50, 2, 4, 2)).unwrap();
        assert_eq!((s.log2_gobs_y, s.pitch), (2, 256));
    }

    #[test]
    fn pitch_records_and_short_blocks_are_not_surfaces() {
        assert_eq!(d3d_surface(&record(4096, 1, 0, 0, 0)), None);
        // 12 blocks of 8 GOBs (768 rows) cannot hold 1440 rows.
        assert_eq!(d3d_surface(&record(5120, 1440, 3, 320, 12)), None);
        assert_eq!(d3d_surface(&record(5120, 1440, 6, 320, 12)), None);
        // A CUDA/Vulkan record (0xd0 units) is not D3D.
        let mut b = record(5120, 1440, 4, 320, 12);
        put32(&mut b, SIZE_UNITS_AT, 480).unwrap();
        assert_eq!(d3d_surface(&b), None);
    }
}

#[cfg(test)]
mod nvgl_tests {
    use super::*;

    fn record(fields: &[(usize, u32)]) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = [0u8; ALLOCATION_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&crate::nvda::NVDA_MAGIC.to_le_bytes());
        put32(&mut b, RECORD_TAG_AT, RECORD_TAG_NVGL).unwrap();
        for (o, v) in fields {
            put32(&mut b, *o, *v).unwrap();
        }
        b
    }

    fn get(fields: &[(usize, u32)], at: usize) -> u32 {
        fields.iter().find(|(o, _)| *o == at).map(|(_, v)| *v).unwrap()
    }

    #[test]
    fn first_vulkan_allocation_matches_the_capture() {
        let r = record(&[(0x10, 0x0220_0000), (0x14, 0x4_0000), (0x38, 1), (0xbc, 0x110_0000), (0xc0, 0x208), (0xd0, 1), (0xec, 0x1_0000), (0x108, 0x10_0000), (0x118, 1), (0x120, 0x10_0000)]);
        assert!(is_nvgl_record(&r) && !is_d3d_record(&r));
        assert_eq!(allocation_units(&r), Some(1));
        let w = nvgl_writeback(&r).unwrap();
        for (at, want) in [(0x3c, 1), (0x8c, 0x7810), (0xc0, 0x4208), (0xc8, 0x1000_0000), (0xd0, 1), (0xd4, 0xffff_0000), (0xd8, 0), (0xe0, 0x80), (0x108, 0x18_0000), (0x10c, 0x1000_0000), (0x120, 0x18_0000), (0x1ac, 0x6_0000)] {
            assert_eq!(get(&w, at), want, "+{at:#x}");
        }
    }

    #[test]
    fn vulkan_sizes_round_like_the_capture() {
        for (cc, d0, cc_out, d0_out, d4, d8) in [
            (0x0040_0000u32, 0u32, 0x1000_0000u32, 0u32, 0x0fff_0000u32, 0u32),
            (0x22c0_0000, 0x11, 0x3000_0000, 0x11, 0x2fff_0000, 0x11),
            (0x8000_0000, 3, 0x8000_0000, 3, 0x7fff_0000, 3),
            (0xc000_0000, 0x3f, 0, 0x40, 0xffff_0000, 0x3f),
            (0, 8, 0, 8, 0xffff_0000, 7),
        ] {
            let r = record(&[(0x10, 0x0220_0800), (0xcc, cc), (0xd0, d0)]);
            let w = nvgl_writeback(&r).unwrap();
            assert_eq!((get(&w, 0xcc), get(&w, 0xd0), get(&w, 0xd4), get(&w, 0xd8)), (cc_out, d0_out, d4, d8));
        }
    }

    #[test]
    fn vulkan_fraction_is_not_a_system_address() {
        let mut r = record(&[(0x10, 0x0220_0800), (0xcc, 0x40_0000)]);
        r[4..8].copy_from_slice(&ALLOCATION_PRIVATE_VERSION.to_le_bytes());
        r[8..12].copy_from_slice(&(ALLOCATION_PRIVATE_BYTES as u32).to_le_bytes());
        let p = parse_allocation_private(&r).unwrap();
        assert_eq!(p.system_memory, 0);
        assert_eq!(p.size, ALLOCATION_UNIT);
    }

    /// Writeback of `rec` over its input bytes, as the UMD reads it back.
    fn after(rec: &[u8; ALLOCATION_PRIVATE_BYTES]) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut out = *rec;
        for (at, value) in nvgl_writeback(rec).unwrap() {
            put32(&mut out, at, value).unwrap();
        }
        out
    }

    #[test]
    fn vulkan_image_records_match_the_capture() {
        // vktest seq 5130: 640x480 swap-chain image, kind 0x02200808, 0x14 units.
        let swap = record(&[(0x10, 0x0220_0808), (0x18, 8), (0x20, 0x8211), (0x24, 3), (0x28, 7), (0x30, 0x280), (0x34, 0x1e0), (0x38, 1), (0x68, 0x01e0_0000), (0x6c, 0x4_0000), (0x70, 0x3_0000), (0x74, 0x3_0000), (0x78, 0x1_0000), (0x90, 0x1_0000), (0xbc, 0xc000_0000), (0xc0, 0x0280_0200), (0xc4, 0x01e0_0000), (0xd0, 0x14), (0xdc, 0x2004_0000), (0xe0, 2), (0xec, 0x1_0000)]);
        let a = after(&swap);
        for (at, want) in [(0xc0, 0x0280_4200), (0xc8, 0x0a00_0000), (0xcc, 0), (0xd0, 0x14), (0xd4, 0xffff_0000), (0xd8, 0x13), (0xdc, 0x2004_0000), (0xe0, 0x102)] {
            assert_eq!(get32(&a, at), Some(want), "swap +{at:#x}");
        }
        // Seq 2457: 16x8 image, kind 0x00200808, 512 bytes (0x02000000 at +0xcc)
        // rounded up to one whole unit.
        let small = record(&[(0x10, 0x0020_0808), (0x14, 0x4_0000), (0x20, 0x201), (0x28, 0x5a), (0x30, 0x10), (0x34, 8), (0x68, 0x8_0000), (0x6c, 0x4_0000), (0x70, 0x3_0000), (0x74, 0x3_0000), (0x78, 0x1_0000), (0x90, 0x1_0000), (0xb8, 0x2_0000), (0xbc, 0xc000_0000), (0xc0, 0x10_0200), (0xc4, 0x8_0000), (0xcc, 0x0200_0000), (0xdc, 0x4_0000), (0xe0, 2), (0xec, 0x1_0000)]);
        let a = after(&small);
        for (at, want) in [(0xc0, 0x10_4200), (0xc8, 0x40_0000), (0xcc, 0), (0xd0, 1), (0xd4, 0xffff_0000), (0xd8, 0), (0xdc, 0x4_0000), (0xe0, 0x102)] {
            assert_eq!(get32(&a, at), Some(want), "small +{at:#x}");
        }
    }

    #[test]
    fn untyped_vulkan_record_keeps_its_cc_word() {
        // Seq 2153: kind 0, 1 unit, +0xcc = 1 is not a size fraction.
        let r = record(&[(0x14, 0x4_0000), (0x20, 0x200), (0x30, 0x1_0000), (0x34, 1), (0x68, 0x1_0000), (0x6c, 0x1_0000), (0xbc, 0xc000_0000), (0xc0, 0x200), (0xc4, 0x1_0001), (0xcc, 1), (0xd0, 1), (0xe0, 0x800), (0xec, 0x1_0000)]);
        let a = after(&r);
        for (at, want) in [(0xc0, 0x4200), (0xc8, 0), (0xcc, 1), (0xd0, 1), (0xd4, 0xffff_0000), (0xd8, 0), (0xe0, 0x880)] {
            assert_eq!(get32(&a, at), Some(want), "+{at:#x}");
        }
    }

    #[test]
    fn vulkan_dc_word_is_the_umds() {
        // Seq 3509: kind 0x02200800, 1 unit, +0xdc = 0x20000000 survives.
        let r = record(&[(0x10, 0x0220_0800), (0xbc, 0xc00d_0000), (0xd0, 1), (0xdc, 0x2000_0000)]);
        assert_eq!(get32(&after(&r), 0xdc), Some(0x2000_0000));
        assert!(nvgl_writeback(&r).unwrap().iter().all(|(at, _)| *at != 0xdc));
    }

    #[test]
    fn vulkan_page_bits_follow_bc_bits_30_and_24() {
        // Capture: 0xc00d0000 with 1 unit -> 0x80 (3 records), 0xc10d0000 with
        // 1/4/0x10 units -> 0x100 (9 records), 0x01100000 (kind 0x02200000) -> 0x80.
        let plain = record(&[(0x10, 0x0220_0800), (0xbc, 0xc00d_0000), (0xd0, 1)]);
        assert_eq!(get32(&after(&plain), 0xe0), Some(0x80));
        let plain_fraction = record(&[(0x10, 0x0220_0800), (0xbc, 0xc00d_0000), (0xcc, 0x40_0000)]);
        assert_eq!(get32(&after(&plain_fraction), 0xe0), Some(0x80));
        for units in [1, 4, 0x10] {
            let big_pages = record(&[(0x10, 0x0220_0800), (0xbc, 0xc10d_0000), (0xd0, units)]);
            assert_eq!(get32(&after(&big_pages), 0xe0), Some(0x100), "{units} units");
        }
        let first = record(&[(0x10, 0x0220_0000), (0xbc, 0x110_0000), (0xd0, 1)]);
        assert_eq!(get32(&after(&first), 0xe0), Some(0x80));
    }

    #[test]
    fn two_mib_staging_record_matches_the_capture_byte_for_byte() {
        // vktest capture seq 2468 (vkAllocateMemory of 2 MiB, kind 0x02200000,
        // +0xbc 0x400d0000): every written field, beyond 0x100 too. +0x108 and
        // +0x120 stay 0x140000; +0x1b0 becomes 0x60000.
        let r = record(&[(0x10, 0x0220_0000), (0x14, 0x4_0004), (0x20, 0x200), (0x28, 0x33), (0x38, 1), (0x6c, 0x1_0000), (0x70, 0x3_0000), (0x74, 0x3_0000), (0x78, 0x1_0000), (0x90, 0x1_0000), (0xa0, 0xffff_0000), (0xa4, 0xffff_ffff), (0xa8, 0xffff), (0xbc, 0x400d_0000), (0xc0, 0x200), (0xd0, 0x20), (0xec, 0x1_0000), (0x100, 0xa71e_0000), (0x104, 0x1c95), (0x108, 0x14_0000), (0x118, 0x20), (0x120, 0x14_0000), (0x130, 0x1_0000), (0x134, 0x1_0000), (0x138, 0x1_0000)]);
        let a = after(&r);
        let mut want = r;
        for (at, value) in [(0x3c, 1), (0x40, 1), (0x8c, 0x7810), (0xc0, 0x4200), (0xc8, 0x1000_0000), (0xd4, 0xffff_0000), (0xd8, 0x1f), (0xe0, 0x180), (0xf4, 0xffff_0000), (0xf8, 0xffff_ffff), (0xfc, 0xffff), (0x10c, 0x1000_0010), (0x1ac, 0x6_0000), (0x1b0, 0x6_0000)] {
            put32(&mut want, at, value).unwrap();
        }
        for at in (0..ALLOCATION_PRIVATE_BYTES - 3).step_by(4) {
            assert_eq!(get32(&a, at), get32(&want, at), "+{at:#x}");
        }
    }

    #[test]
    fn attribute_bit_80000_follows_bc_bit_20() {
        // Seq 1387: +0xbc 0x01100000 -> +0x108/+0x120 0x100000 -> 0x180000; seq
        // 2468 (+0xbc 0x400d0000, same kind) keeps 0x140000.
        let first = record(&[(0x10, 0x0220_0000), (0xbc, 0x110_0000), (0xd0, 1), (0x108, 0x10_0000), (0x120, 0x10_0000)]);
        let a = after(&first);
        assert_eq!((get32(&a, 0x108), get32(&a, 0x120)), (Some(0x18_0000), Some(0x18_0000)));
        let other = record(&[(0x10, 0x0220_0800), (0xbc, 0xc00d_0000), (0xd0, 1), (0x108, 0x14_0000), (0x120, 0x14_0080)]);
        let a = after(&other);
        assert_eq!((get32(&a, 0x108), get32(&a, 0x120)), (Some(0x14_0000), Some(0x14_0080)));
    }

    #[test]
    fn compression_requests_are_answered_uncompressed() {
        // Seq 3509 asks for compression (+0xdc bit 29). This KMD's memory is
        // linear and uncompressed: +0x11c stays 0, +0x108/+0x120 keep the UMD's
        // 0x140000, +0x1ac/+0x1b0 are the uncompressed 0x60000.
        let r = record(&[(0x10, 0x0220_0800), (0xbc, 0xc10d_0000), (0xd0, 1), (0xdc, 0x2000_0000), (0x108, 0x14_0000), (0x120, 0x14_0000)]);
        let a = after(&r);
        for (at, want) in [(0xdc, 0x2000_0000), (0x108, 0x14_0000), (0x11c, 0), (0x120, 0x14_0000), (0x1ac, 0x6_0000), (0x1b0, 0x6_0000), (0xe0, 0x100)] {
            assert_eq!(get32(&a, at), Some(want), "+{at:#x}");
        }
    }

    #[test]
    fn image_pitch_and_fraction_mirror_match_the_capture() {
        // Seq 2457 (16x8): +0x10c = pitch 64 << 16, +0x114 0x02000000 ->
        // 0x10000000 while +0xcc rounds to a whole unit. Seq 5130 (640x480):
        // +0x10c = 2560 << 16, +0x114 = 0x14 (the count) stays.
        let small = record(&[(0x10, 0x0020_0808), (0x30, 0x10), (0x34, 8), (0x6c, 0x4_0000), (0xbc, 0xc000_0000), (0xcc, 0x0200_0000), (0x108, 0x14_0000), (0x114, 0x0200_0000), (0x11c, 0x4_0000), (0x120, 0x14_0002)]);
        let a = after(&small);
        for (at, want) in [(0xcc, 0), (0xd0, 1), (0x10c, 0x40_0000), (0x114, 0x1000_0000), (0x108, 0x14_0000), (0x11c, 0x4_0000), (0x120, 0x14_0002)] {
            assert_eq!(get32(&a, at), Some(want), "small +{at:#x}");
        }
        let swap = record(&[(0x10, 0x0220_0808), (0x30, 0x280), (0x34, 0x1e0), (0x6c, 0x4_0000), (0xbc, 0xc000_0000), (0xd0, 0x14), (0x114, 0x14), (0x120, 0x14_0002)]);
        let a = after(&swap);
        assert_eq!((get32(&a, 0x10c), get32(&a, 0x114)), (Some(0x0a00_0000), Some(0x14)));
        // Seq 2184: 0x3f units + 0xc0000000 -> 0x40 units, +0x114 keeps 0xc0000000.
        let big = record(&[(0x10, 0x0220_0800), (0xbc, 0xc00d_0000), (0xcc, 0xc000_0000), (0xd0, 0x3f), (0x114, 0xc000_0000)]);
        let a = after(&big);
        assert_eq!((get32(&a, 0xcc), get32(&a, 0xd0), get32(&a, 0x114)), (Some(0), Some(0x40), Some(0xc000_0000)));
    }

    #[test]
    fn vulkan_alignment_follows_the_page_bits() {
        // Capture: 0xc10d0000 records (0x100) at 64-KiB VAs, 0x20-unit records
        // (0x180) at 2-MiB VAs, 0xc00d0000 one-unit records (0x80) at 4-KiB VAs.
        let big = record(&[(0x10, 0x0220_0800), (0xbc, 0xc10d_0000), (0xd0, 4)]);
        assert_eq!(nvgl_alignment(&big), Some(0x1_0000));
        let huge = record(&[(0x10, 0x0220_0000), (0xbc, 0x400d_0000), (0xd0, 0x20)]);
        assert_eq!(nvgl_alignment(&huge), Some(0x20_0000));
        let small = record(&[(0x10, 0x0220_0800), (0xbc, 0xc00d_0000), (0xd0, 1)]);
        assert_eq!(nvgl_alignment(&small), None);
        // The narrow kind's 0x280 is not a large-page record.
        let narrow = record(&[(0x10, 0x0200_0000), (0xd0, 0x40)]);
        assert_eq!(nvgl_alignment(&narrow), None);
        let mut d3d = big;
        put32(&mut d3d, RECORD_TAG_AT, 0x4e56_4458).unwrap();
        assert_eq!(nvgl_alignment(&d3d), None);
    }

    #[test]
    fn vulkan_page_bits_match_the_capture() {
        let narrow = record(&[(0x10, 0x0200_0000), (0xd0, 8), (0xe0, 0x4000)]);
        assert_eq!(get(&nvgl_writeback(&narrow).unwrap(), 0xe0), 0x4280);
        let big = record(&[(0x10, 0x0220_0800), (0xd0, 0x20)]);
        let w = nvgl_writeback(&big).unwrap();
        assert_eq!((get(&w, 0xe0), get(&w, 0x10c)), (0x180, 0x1000_0010));
        let mid = record(&[(0x10, 0x0220_0800), (0xd0, 8)]);
        assert_eq!(get(&nvgl_writeback(&mid).unwrap(), 0xe0), 0x100);
    }

    fn cuda_record(fields: &[(usize, u32)]) -> [u8; ALLOCATION_PRIVATE_BYTES] {
        let mut b = [0u8; ALLOCATION_PRIVATE_BYTES];
        put32(&mut b, 0, crate::nvda::NVDA_MAGIC).unwrap();
        put32(&mut b, 4, ALLOCATION_PRIVATE_VERSION).unwrap();
        put32(&mut b, 8, ALLOCATION_PRIVATE_BYTES as u32).unwrap();
        put32(&mut b, RECORD_TAG_AT, crate::nvda::NVDA_TAG_NV).unwrap();
        for (o, v) in fields {
            put32(&mut b, *o, *v).unwrap();
        }
        b
    }

    /// Every distinct input shape of the 610.88 cudatest captures (12300 and
    /// 11624) with exactly the words the KMD changed in it.
    #[test]
    fn cuda_records_match_the_capture() {
        let shapes: &[(&[(usize, u32)], &[(usize, u32)])] = &[
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x40), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x40), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x3f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0x9c, 0x500), (0xa0, 0xffff0000), (0xa4, 0x51f), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0x9c, 0x700), (0xa0, 0xffff0000), (0xa4, 0x71f), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x1000000), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x400000), (0x74, 0x400000), (0x78, 0x400000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x60), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x60), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x5f), (0xe0, 0x280), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x1000000), (0x14, 0x40100), (0x20, 0x20000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x20000), (0x74, 0x20000), (0x78, 0x20000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x280), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x1000000), (0x14, 0x40000), (0x20, 0x10000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x20000), (0x74, 0x20000), (0x78, 0x20000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x220), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x220), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x21f), (0xe0, 0x280), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x1000000), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x400000), (0x74, 0x400000), (0x78, 0x400000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x108, 0x40000), (0x110, 0x20), (0x118, 0x20), (0x120, 0x40000), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x280), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x1000000), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x400000), (0x74, 0x400000), (0x78, 0x400000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x280), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x1000000), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x20000), (0x74, 0x20000), (0x78, 0x20000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x280), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x20, 0x800000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x400), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x400), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x3ff), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x10000), (0x74, 0x10000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x20), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x20), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x1f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x30000), (0x74, 0x30000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x100), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x100), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0xff), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
            (&[(0x10, 0x800), (0x14, 0x40000), (0x28, 0x42), (0x6c, 0x10000), (0x70, 0x10000), (0x74, 0x10000), (0x78, 0x10000), (0x90, 0x10000), (0xb8, 0x20000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x40), (0xec, 0x10000), (0x110, 0x20), (0x118, 0x40), (0x130, 0x10000), (0x134, 0x10000), (0x138, 0x10000), (0x1cc, 0x20)], &[(0x38, 0x1), (0x3c, 0x1), (0x40, 0x1), (0x8c, 0x7810), (0xc0, 0x4200), (0xd4, 0xffff0000), (0xd8, 0x3f), (0xe0, 0x180), (0xf4, 0xffff0000), (0xf8, 0xffffffff), (0xfc, 0xffff), (0x108, 0x80000), (0x10c, 0x10), (0x120, 0x80000), (0x1ac, 0x60000), (0x1b0, 0x60000)]),
        ];
        for (input, changed) in shapes {
            let before = cuda_record(input);
            assert!(is_cuda_record(&before) && !is_d3d_record(&before) && !is_nvgl_record(&before));
            let p = parse_allocation_private(&before).unwrap();
            assert_eq!(p.size, u64::from(get32(&before, SIZE_UNITS_AT).unwrap()) * ALLOCATION_UNIT);
            let mut after = before;
            apply_fields(&mut after, &cuda_writeback(&before).unwrap()).unwrap();
            let mut want = before;
            for (o, v) in *changed {
                put32(&mut want, *o, *v).unwrap();
            }
            assert_eq!(after, want, "input {input:x?}");
        }
    }

    #[test]
    fn cuda_records_carry_no_system_pointer() {
        // Our cudatest (405.273) seq 595, byte-identical to capture 12300 seq 597:
        // the first allocation after the context, 0x40 units of video memory.
        let r = cuda_record(&[(0x10, 0x800), (0x14, 0x4_0000), (0x28, 0x42), (0x6c, 0x1_0000), (0x70, 0x3_0000), (0x74, 0x3_0000), (0x78, 0x1_0000), (0x90, 0x1_0000), (0xb8, 0x2_0000), (0xc0, 0x200), (0xcc, 0x20), (0xd0, 0x40), (0xec, 0x1_0000), (0x110, 0x20), (0x118, 0x40), (0x130, 0x1_0000), (0x134, 0x1_0000), (0x138, 0x1_0000), (0x1cc, 0x20)]);
        let p = parse_allocation_private(&r).unwrap();
        assert_eq!(p.system_memory, 0);
        assert!(!p.system_backed());
        assert_eq!(p.size, 0x40 * ALLOCATION_UNIT);
        // The pSystemMem kind is not system-backed in this sense either.
        let mut s = r;
        put32(&mut s, 0x10, CUDA_KIND_SYSTEM_MEMORY).unwrap();
        assert_eq!(parse_allocation_private(&s).unwrap().system_memory, 0);
    }

    #[test]
    fn cuda_system_memory_follows_the_kind_word() {
        // 610.88 cudatest: kind 0x01000000 with pSystemMem, kind 0x800 without.
        assert!(is_cuda_system_memory(&cuda_record(&[(0x10, 0x0100_0000), (0x14, 0x4_0100), (0xd0, 0x20)])));
        assert!(!is_cuda_system_memory(&cuda_record(&[(0x10, 0x800), (0x14, 0x4_0000), (0xd0, 0x400)])));
        // Not a CUDA record: a D3D record (no units at 0xd0) and a Vulkan one.
        assert!(!is_cuda_system_memory(&cuda_record(&[(0x10, 0x0100_0000)])));
        let mut vk = cuda_record(&[(0x10, 0x0100_0000), (0xd0, 1)]);
        put32(&mut vk, RECORD_TAG_AT, RECORD_TAG_NVGL).unwrap();
        assert!(!is_cuda_system_memory(&vk));
    }

    #[test]
    fn cuda_writeback_needs_a_unit_count() {
        let r = cuda_record(&[(0x10, 0x800)]);
        assert!(!is_cuda_record(&r));
        assert!(cuda_writeback(&r).is_none());
    }
}

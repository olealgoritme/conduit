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
/// D3D (NVDX) records leave `SIZE_UNITS_AT` zero and describe the size in
/// 64-KiB units as a fixed-point value: fraction at 0x10c, integer in the low
/// 24 bits of 0x110, and the rounded-up count at 0x114. Vulkan records put the
/// integer at `SIZE_UNITS_AT` and a nonzero fraction at 0x114 (or repeat the
/// count there). Checked against every mapped allocation of the 610.88 D3D11,
/// D3D12, Vulkan and CUDA captures (384 of 384).
pub const SIZE_FRACTION_AT: usize = 0x10c;
pub const SIZE_INTEGER_AT: usize = 0x110;
pub const SIZE_COUNT_AT: usize = 0x114;

/// Size in 64-KiB units, rounded up, from an NVDA allocation record.
pub fn allocation_units(bytes: &[u8]) -> Option<u64> {
    let units = u64::from(get32(bytes, SIZE_UNITS_AT)?);
    let count = u64::from(get32(bytes, SIZE_COUNT_AT)?);
    if units != 0 {
        // CUDA: count 0. Vulkan: a fraction, or the count repeated.
        return Some(units + u64::from(count != 0 && count != units));
    }
    let fraction = get32(bytes, SIZE_FRACTION_AT)?;
    let integer = u64::from(get32(bytes, SIZE_INTEGER_AT)? & 0x00ff_ffff);
    Some(count.max(integer + u64::from(fraction != 0)))
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
    let system_memory = get64(bytes, SYSTEM_MEMORY_AT).ok_or(PrivateError::Short)?;
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
}

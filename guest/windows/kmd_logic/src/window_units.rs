//! The units arithmetic of the shared-memory regions (the RM window, region 1; the UVM
//! aperture, region 2): nothing here may assume the window fits 32 bits.
//!
//! The host sizes the RM window to the GPU's BAR1 (32 GiB with ReBAR, 128 GiB on a 96 GB
//! card), so a byte count is a `u64` everywhere, a MiB figure for a registry counter is a
//! saturating `u32` (4 PiB wide), and an offset the host names is checked against the region
//! in `u64` arithmetic that cannot wrap, before it becomes a physical address.
//!
//! What the driver does with the window, for the record (`docs/nvrm-escape.md`, "The RM
//! window policy", limits): the window is NEVER mapped into kernel address space as one
//! range (no `MmMapIoSpace` of 32 GiB); each `MMAP` builds an MDL of PFNs over its own
//! page-aligned span and maps that into the caller (`blob_map::map_io_pages_to_user_prot`),
//! and the KMD's own client maps one surface at a time. Nothing scans the window per page.

/// Bytes in a MiB.
pub const MIB: u64 = 1 << 20;
/// The page every offset and size is a multiple of.
pub const PAGE: u64 = 4096;

/// `bytes` in MiB for a `u32` registry counter: exact up to 4 PiB, saturating beyond (never
/// wrapping to a small number).
pub fn mib_u32(bytes: u64) -> u32 {
    (bytes / MIB).min(u64::from(u32::MAX)) as u32
}

/// Where the host's placement `[offset, offset + size)` of a region that starts at guest
/// physical address `base` and is `len` bytes long is, as a physical address: `None` unless
/// the offset is page aligned, `size` is nonzero, the span lies inside the region and no
/// intermediate sum wraps (a hostile or broken host can send any 64-bit pair), and the
/// result fits the signed 64-bit `PHYSICAL_ADDRESS`.
pub fn place(base: u64, len: u64, offset: u64, size: u64) -> Option<u64> {
    if size == 0 || offset % PAGE != 0 {
        return None;
    }
    let end = offset.checked_add(size)?;
    if end > len {
        return None;
    }
    // The last byte's address must not wrap either.
    let top = base.checked_add(end)?;
    if top > i64::MAX as u64 {
        return None;
    }
    base.checked_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn mib_of_big_windows() {
        assert_eq!(mib_u32(0), 0);
        assert_eq!(mib_u32(4 * GIB), 4096);
        assert_eq!(mib_u32(32 * GIB), 32 * 1024);
        assert_eq!(mib_u32(64 * GIB), 64 * 1024);
        assert_eq!(mib_u32(128 * GIB), 128 * 1024);
        // 4 GiB is where a u32 BYTE count wraps to 0: the MiB figure does not.
        assert_ne!(mib_u32(4 * GIB), 0);
        assert_eq!(mib_u32((1 << 32) + (3 << 20)), 4096 + 3);
    }

    #[test]
    fn mib_saturates_instead_of_wrapping() {
        assert_eq!(mib_u32(u64::from(u32::MAX) * MIB), u32::MAX);
        assert_eq!(mib_u32(u64::from(u32::MAX) * MIB + MIB), u32::MAX);
        assert_eq!(mib_u32(u64::MAX), u32::MAX);
    }

    #[test]
    fn place_inside_a_128_gib_window() {
        // A BAR above 4 GiB, as the host-visible BAR is.
        let base = 0x20_0000_0000u64;
        let len = 128 * GIB;
        assert_eq!(place(base, len, 0, 4096), Some(base));
        assert_eq!(place(base, len, 100 * GIB, 256 * MIB), Some(base + 100 * GIB));
        // The very end is allowed, one page past it is not.
        assert_eq!(place(base, len, len - 4096, 4096), Some(base + len - 4096));
        assert_eq!(place(base, len, len - 4096, 8192), None);
        assert_eq!(place(base, len, len, 4096), None);
    }

    #[test]
    fn place_offsets_past_4_gib_keep_their_high_bits() {
        let base = 0x10_0000_0000u64;
        let off = (5 * GIB) + 0x3000;
        assert_eq!(place(base, 16 * GIB, off, 4096), Some(base + off));
        assert_eq!(place(base, 16 * GIB, off, 4096).unwrap() >> 32, (base + off) >> 32);
    }

    #[test]
    fn place_refuses_what_wraps() {
        // offset + size overflows u64.
        assert_eq!(place(0, u64::MAX, u64::MAX & !4095, 1 << 63), None);
        assert_eq!(place(0, u64::MAX, 1 << 63, 1 << 63), None);
        assert_eq!(place(0x1000, 1 << 40, u64::MAX & !4095, 4096), None);
        assert_eq!(place(0x1000, 1 << 40, 0, u64::MAX), None);
        // base + end overflows u64 although the span is inside the region.
        assert_eq!(place(u64::MAX - 4095, 1 << 40, 0, 8192), None);
        // A physical address past the signed range.
        assert_eq!(place(i64::MAX as u64 - 4095, 1 << 40, 0, 8192), None);
        assert_eq!(place(i64::MAX as u64 - 8191, 1 << 40, 0, 4096), Some(i64::MAX as u64 - 8191));
    }

    #[test]
    fn place_wants_a_page_aligned_nonempty_span() {
        assert_eq!(place(0x1000, GIB, 1, 4096), None);
        assert_eq!(place(0x1000, GIB, 4095, 4096), None);
        assert_eq!(place(0x1000, GIB, 0, 0), None);
        assert_eq!(place(0x1000, GIB, 4096, 4096), Some(0x2000));
        // A size that is not a page multiple is the caller's `BadRange`; the span is still
        // checked exactly.
        assert_eq!(place(0x1000, 8192, 4096, 4097), None);
        assert_eq!(place(0x1000, 8192, 4096, 4096), Some(0x2000));
    }

    #[test]
    fn empty_region_places_nothing() {
        assert_eq!(place(0x1000, 0, 0, 4096), None);
    }
}

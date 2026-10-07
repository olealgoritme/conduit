//! The `VidMmCapsX` mask: extra `DXGK_VIDMMCAPS` bits for the GPU-memory redirection
//! experiment (stage V1, `docs/vram-redirection.md` 5.2).
//!
//! Same shape as `flip_flags::resolve_flip_caps`: a raw bit mask from the service key, OR'd into
//! the word the driver builds itself, restricted to an accepted set. Bit positions are the C
//! bitfield order of `DXGK_VIDMMCAPS` in `d3dkmddi.h` (WDK 10.0.26100.0): `OutOfOrderLock` 0,
//! `DedicatedPagingEngine` 1, `PagingEngineCanSwizzle` 2, `SectionBackedPrimary` 3,
//! `CrossAdapterResource` 4, `VirtualAddressingSupported` 5, `GpuMmuSupported` 6,
//! `IoMmuSupported` 7, `ReplicateGdiContent` 8, `NonCpuVisiblePrimary` 9, ...
//! (<https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/ns-d3dkmddi-_dxgk_vidmmcaps>).

/// `DXGK_VIDMMCAPS.SectionBackedPrimary` (bit 3): always set by the driver.
pub const VIDMMCAPS_SECTION_BACKED_PRIMARY: u32 = 1 << 3;
/// `DXGK_VIDMMCAPS.CrossAdapterResource` (bit 4): the driver sets it from `CrossAdaptCaps`.
pub const VIDMMCAPS_CROSS_ADAPTER_RESOURCE: u32 = 1 << 4;
/// `DXGK_VIDMMCAPS.VirtualAddressingSupported` (bit 5): the driver sets it with GpuMmu.
pub const VIDMMCAPS_VIRTUAL_ADDRESSING_SUPPORTED: u32 = 1 << 5;
/// `DXGK_VIDMMCAPS.GpuMmuSupported` (bit 6): the driver sets it with GpuMmu.
pub const VIDMMCAPS_GPU_MMU_SUPPORTED: u32 = 1 << 6;
/// `DXGK_VIDMMCAPS.NonCpuVisiblePrimary` (bit 9, WDDM 2.0+): "GDI allocations are not required
/// to be CPU visible". The one bit stage V1 tests.
pub const VIDMMCAPS_NON_CPU_VISIBLE_PRIMARY: u32 = 1 << 9;

/// The only `VidMmCapsX` bits accepted as extras. Everything the driver decides itself (bits 3
/// to 6), the memory-model bits it must not mix (`IoMmuSupported`), the reserved ones and the
/// WDDM 2.2+ bits this WDDM 2.1 adapter cannot honour are dropped and reported.
pub const VIDMMCAPS_X_ACCEPTED: u32 = VIDMMCAPS_NON_CPU_VISIBLE_PRIMARY;

/// The outcome of applying a raw `VidMmCapsX` value to the driver's own word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VidMmCaps {
    /// `knob & VIDMMCAPS_X_ACCEPTED`, mirrored as `VmCapsXEff`.
    pub effective: u32,
    /// The knob bits that were refused, mirrored as `VmCapsXMsk`. Bits the driver's own word
    /// already carries are a silent no-op, not a refusal.
    pub dropped: u32,
    /// The word handed to dxgkrnl as `DXGK_DRIVERCAPS.MemoryManagementCaps.Value`, mirrored as
    /// `VmCapsRep`.
    pub reported: u32,
}

/// Apply `VidMmCapsX` to `base`, the word the driver builds from its other knobs. `knob == 0`
/// (the default) reports exactly `base`.
pub const fn resolve_vidmm_caps(base: u32, knob: u32) -> VidMmCaps {
    let effective = knob & VIDMMCAPS_X_ACCEPTED;
    VidMmCaps {
        effective,
        dropped: knob & !(VIDMMCAPS_X_ACCEPTED | base),
        reported: base | effective,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCTION: u32 = VIDMMCAPS_SECTION_BACKED_PRIMARY
        | VIDMMCAPS_VIRTUAL_ADDRESSING_SUPPORTED
        | VIDMMCAPS_GPU_MMU_SUPPORTED;

    #[test]
    fn header_bit_values() {
        assert_eq!(VIDMMCAPS_SECTION_BACKED_PRIMARY, 0x8);
        assert_eq!(VIDMMCAPS_CROSS_ADAPTER_RESOURCE, 0x10);
        assert_eq!(VIDMMCAPS_VIRTUAL_ADDRESSING_SUPPORTED, 0x20);
        assert_eq!(VIDMMCAPS_GPU_MMU_SUPPORTED, 0x40);
        assert_eq!(VIDMMCAPS_NON_CPU_VISIBLE_PRIMARY, 0x200);
        assert_eq!(VIDMMCAPS_X_ACCEPTED, 0x200);
    }

    #[test]
    fn default_reports_the_base_word_unchanged() {
        for base in [0, PRODUCTION, PRODUCTION | VIDMMCAPS_CROSS_ADAPTER_RESOURCE] {
            let c = resolve_vidmm_caps(base, 0);
            assert_eq!(c, VidMmCaps { effective: 0, dropped: 0, reported: base });
        }
    }

    #[test]
    fn non_cpu_visible_primary_is_or_ed_in() {
        let c = resolve_vidmm_caps(PRODUCTION, 0x200);
        assert_eq!(c.reported, PRODUCTION | 0x200);
        assert_eq!(c.effective, 0x200);
        assert_eq!(c.dropped, 0);
    }

    #[test]
    fn driver_owned_bits_cannot_be_forced_on_and_are_reported() {
        // CrossAdapterResource without CrossAdaptCaps, IoMmuSupported, OutOfOrderLock: dropped.
        let c = resolve_vidmm_caps(PRODUCTION, 0x10 | 0x80 | 0x1 | 0x200);
        assert_eq!(c.reported, PRODUCTION | 0x200);
        assert_eq!(c.dropped, 0x10 | 0x80 | 0x1);
        // Writing the full word that is already reported is a silent no-op.
        let c = resolve_vidmm_caps(PRODUCTION, PRODUCTION | 0x200);
        assert_eq!(c.dropped, 0);
        assert_eq!(c.reported, PRODUCTION | 0x200);
        let c = resolve_vidmm_caps(PRODUCTION, u32::MAX);
        assert_eq!(c.reported, PRODUCTION | 0x200);
        assert_eq!(c.dropped, !(PRODUCTION | 0x200));
    }

    #[test]
    fn effective_and_dropped_partition_the_input() {
        let mut v = 0u32;
        while v < 0x2000 {
            let c = resolve_vidmm_caps(PRODUCTION, v);
            assert_eq!(c.effective | c.dropped | (v & PRODUCTION), v);
            assert_eq!(c.effective & c.dropped, 0);
            assert_eq!(c.reported & PRODUCTION, PRODUCTION);
            assert_eq!(c.reported & !PRODUCTION, c.effective);
            v += 1;
        }
    }
}

//! The device-memory capacity the adapter reports to VidMm (the "dedicated video memory" that
//! `dxdiag`, DXGI's `DedicatedVideoMemory` and the process budgets are sized from).
//!
//! The reported segment is accounting, not storage: VidMm charges the full declared size of every
//! device-local tracking allocation (`HELIOS_WDDM_ALLOC_KIND_TRACKING`, one page of KMD state each)
//! against it, and the bytes themselves live on the host GPU (Venus device memory, or RM video
//! memory under NVK). The KMD maps only a separately capped CPU aperture of it
//! (`BarSegment::size`). So the right capacity is what the host GPU can hold, not the size of the
//! virtio shared-memory window the host happens to offer for host-visible blobs (the Venus
//! `hostmem`, 8 GiB by default, an address-space size that has nothing to do with the card).
//!
//! The host GPU's capacity is read from the Venus device the KMD already creates at StartDevice:
//! `vkGetPhysicalDeviceMemoryProperties` lists the card's heaps, and the largest
//! `DEVICE_LOCAL` one is its video memory. (A second `DEVICE_LOCAL` heap, the BAR-visible slice
//! some drivers list, overlaps the first and is never added to it.)

/// `VK_MEMORY_HEAP_DEVICE_LOCAL_BIT`.
pub const MEMORY_HEAP_DEVICE_LOCAL: u32 = 0x1;
/// `VK_MAX_MEMORY_HEAPS`: the length of the encoded heap array.
pub const MAX_MEMORY_HEAPS: usize = 16;

/// `VidMmVramMB` value meaning "absent: derive it" (never a valid size).
pub const AUTO: u32 = u32::MAX;
/// Smallest capacity reported as a local segment, MiB. Below it the legacy topology is used.
pub const MIN_MB: u32 = 256;
/// Largest capacity reported, MiB. Segment ranges start at GPU address 4 GiB inside a 40-bit
/// virtual address space; 64 GiB is the largest size the segment path has been sized for, so a
/// bigger card is reported as this.
pub const MAX_MB: u32 = 64 * 1024;

const MIB: u64 = 1 << 20;

/// The video memory of the host GPU from the heaps `vkGetPhysicalDeviceMemoryProperties` reported:
/// the largest `DEVICE_LOCAL` heap among the first `count`, in bytes. `None` when there is none
/// (or every one is empty).
pub fn device_local_heap_bytes(heaps: &[(u64, u32)], count: u32) -> Option<u64> {
    let n = (count as usize).min(MAX_MEMORY_HEAPS).min(heaps.len());
    let mut best = 0u64;
    for &(size, flags) in &heaps[..n] {
        if flags & MEMORY_HEAP_DEVICE_LOCAL != 0 && size > best {
            best = size;
        }
    }
    (best != 0).then_some(best)
}

/// Where the reported capacity came from, mirrored as `VidVSrc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// An explicit `VidMmVramMB` value (0 included).
    Registry,
    /// The host GPU's largest device-local heap.
    DeviceLocalHeap,
    /// The virtio host-visible window length: the fallback when the Venus device could not tell.
    /// It is address space for blobs, not the card's memory.
    HostVisibleWindow,
    /// Nothing usable: the legacy one-GiB aperture+BAR topology.
    None,
}

impl Source {
    /// The value of the `VidVSrc` mirror.
    pub const fn code(self) -> u32 {
        match self {
            Self::Registry => 1,
            Self::DeviceLocalHeap => 2,
            Self::HostVisibleWindow => 3,
            Self::None => 0,
        }
    }
}

/// The resolved capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// MiB; 0 selects the legacy topology. An explicit registry value is passed through unchanged
    /// (an out-of-range one is refused, and counted, where the segment is built).
    pub mb: u32,
    pub source: Source,
    /// The host GPU was bigger than [`MAX_MB`] and the value is the cap.
    pub clamped: bool,
    /// An automatic source existed but its size was unusable (`VidVBad` carries it, in MiB).
    pub bad_mb: Option<u32>,
}

/// Resolve `VidMmVramMB`. `knob` is the registry value ([`AUTO`] when absent);
/// `device_local_heap` the host GPU's video memory from [`device_local_heap_bytes`];
/// `host_visible` the exact length the virtio host-visible shared-memory capability reports.
pub fn resolve(knob: u32, device_local_heap: Option<u64>, host_visible: Option<u64>) -> Resolved {
    if knob != AUTO {
        return Resolved {
            mb: knob,
            source: Source::Registry,
            clamped: false,
            bad_mb: None,
        };
    }
    if let Some(bytes) = device_local_heap {
        let mb = bytes / MIB;
        if mb >= u64::from(MIN_MB) {
            let clamped = mb > u64::from(MAX_MB);
            return Resolved {
                mb: mb.min(u64::from(MAX_MB)) as u32,
                source: Source::DeviceLocalHeap,
                clamped,
                bad_mb: None,
            };
        }
    }
    match host_visible {
        None => Resolved {
            mb: 0,
            source: Source::None,
            clamped: false,
            bad_mb: None,
        },
        Some(bytes) => {
            let mb = bytes / MIB;
            if bytes % MIB == 0 && (u64::from(MIN_MB)..=u64::from(MAX_MB)).contains(&mb) {
                Resolved {
                    mb: mb as u32,
                    source: Source::HostVisibleWindow,
                    clamped: false,
                    bad_mb: None,
                }
            } else {
                Resolved {
                    mb: 0,
                    source: Source::None,
                    clamped: false,
                    bad_mb: Some(mb.min(u64::from(u32::MAX)) as u32),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const DL: u32 = MEMORY_HEAP_DEVICE_LOCAL;

    /// An RTX 5090 as the proprietary driver lists it through Venus: the card's memory, the system
    /// memory heap, and the small BAR-visible device-local slice.
    const RTX_5090: [(u64, u32); 3] = [(32_109 * MIB, DL), (48 * GIB, 0), (256 * MIB, DL)];

    #[test]
    fn heap_is_the_largest_device_local_one() {
        assert_eq!(device_local_heap_bytes(&RTX_5090, 3), Some(32_109 * MIB));
    }

    #[test]
    fn heap_order_does_not_matter() {
        let heaps = [(256 * MIB, DL), (48 * GIB, 0), (24 * GIB, DL)];
        assert_eq!(device_local_heap_bytes(&heaps, 3), Some(24 * GIB));
    }

    #[test]
    fn the_bar_slice_is_not_added_to_the_card() {
        // The two device-local heaps overlap physically.
        let sum = 32_109 * MIB + 256 * MIB;
        assert_ne!(device_local_heap_bytes(&RTX_5090, 3), Some(sum));
    }

    #[test]
    fn heaps_past_the_count_are_ignored() {
        let heaps = [(8 * GIB, DL), (64 * GIB, DL)];
        assert_eq!(device_local_heap_bytes(&heaps, 1), Some(8 * GIB));
        assert_eq!(device_local_heap_bytes(&heaps, 0), None);
    }

    #[test]
    fn a_count_past_the_array_is_bounded() {
        let heaps = [(8 * GIB, DL)];
        assert_eq!(device_local_heap_bytes(&heaps, 16), Some(8 * GIB));
        assert_eq!(device_local_heap_bytes(&heaps, u32::MAX), Some(8 * GIB));
    }

    #[test]
    fn no_device_local_heap_is_none() {
        let heaps = [(48 * GIB, 0)];
        assert_eq!(device_local_heap_bytes(&heaps, 1), None);
        assert_eq!(device_local_heap_bytes(&[(0, DL)], 1), None);
        assert_eq!(device_local_heap_bytes(&[], 0), None);
    }

    #[test]
    fn auto_reports_the_card_not_the_window() {
        // The defect: an 8 GiB Venus window made a 32 GiB card report 8 GiB.
        let r = resolve(AUTO, Some(32_109 * MIB), Some(8 * GIB));
        assert_eq!(r.mb, 32_109);
        assert_eq!(r.source, Source::DeviceLocalHeap);
        assert!(!r.clamped);
        assert_eq!(r.bad_mb, None);
    }

    #[test]
    fn auto_does_not_depend_on_the_window_size() {
        for window in [
            None,
            Some(4 * GIB),
            Some(8 * GIB),
            Some(64 * GIB),
            Some(GIB + 1),
        ] {
            assert_eq!(resolve(AUTO, Some(24 * GIB), window).mb, 24 * 1024);
        }
    }

    #[test]
    fn auto_keeps_a_card_smaller_than_the_window_at_its_own_size() {
        let r = resolve(AUTO, Some(8 * GIB - 100 * MIB), Some(16 * GIB));
        assert_eq!(r.mb, 8 * 1024 - 100);
        assert_eq!(r.source, Source::DeviceLocalHeap);
    }

    #[test]
    fn a_bigger_card_is_capped() {
        let r = resolve(AUTO, Some(96 * GIB), Some(8 * GIB));
        assert_eq!(r.mb, MAX_MB);
        assert!(r.clamped);
        assert_eq!(r.source, Source::DeviceLocalHeap);
        let r = resolve(AUTO, Some(u64::MAX), None);
        assert_eq!(r.mb, MAX_MB);
        assert!(r.clamped);
    }

    #[test]
    fn the_cap_itself_is_not_clamped() {
        let r = resolve(AUTO, Some(64 * GIB), None);
        assert_eq!(r.mb, MAX_MB);
        assert!(!r.clamped);
    }

    #[test]
    fn a_tiny_heap_falls_through_to_the_window() {
        let r = resolve(AUTO, Some(128 * MIB), Some(8 * GIB));
        assert_eq!(r.mb, 8 * 1024);
        assert_eq!(r.source, Source::HostVisibleWindow);
    }

    #[test]
    fn minimum_heap_is_accepted() {
        let r = resolve(AUTO, Some(256 * MIB), None);
        assert_eq!(r.mb, MIN_MB);
        assert_eq!(r.source, Source::DeviceLocalHeap);
    }

    #[test]
    fn without_a_heap_auto_is_the_window_as_before() {
        let r = resolve(AUTO, None, Some(6 * GIB));
        assert_eq!(r.mb, 6 * 1024);
        assert_eq!(r.source, Source::HostVisibleWindow);
        assert_eq!(r.bad_mb, None);
    }

    #[test]
    fn without_either_auto_is_the_legacy_topology() {
        let r = resolve(AUTO, None, None);
        assert_eq!(r.mb, 0);
        assert_eq!(r.source, Source::None);
        assert_eq!(r.bad_mb, None);
    }

    #[test]
    fn an_unusable_window_is_reported_bad() {
        // Not a whole number of MiB.
        let r = resolve(AUTO, None, Some(GIB + 1));
        assert_eq!(r.mb, 0);
        assert_eq!(r.bad_mb, Some(1024));
        // Below the minimum and above the maximum.
        assert_eq!(resolve(AUTO, None, Some(128 * MIB)).mb, 0);
        assert_eq!(resolve(AUTO, None, Some(128 * MIB)).bad_mb, Some(128));
        assert_eq!(resolve(AUTO, None, Some(128 * GIB)).mb, 0);
        assert_eq!(resolve(AUTO, None, Some(u64::MAX)).bad_mb, Some(u32::MAX));
    }

    #[test]
    fn an_explicit_value_always_wins() {
        for knob in [0, 256, 4096, 24_576, 65_536, 70_000] {
            let r = resolve(knob, Some(32_109 * MIB), Some(8 * GIB));
            assert_eq!(r.mb, knob);
            assert_eq!(r.source, Source::Registry);
            assert!(!r.clamped);
            assert_eq!(r.bad_mb, None);
        }
    }

    #[test]
    fn explicit_zero_is_the_legacy_topology_even_with_a_heap() {
        assert_eq!(resolve(0, Some(32 * GIB), Some(8 * GIB)).mb, 0);
    }

    #[test]
    fn source_codes_are_distinct() {
        let codes = [
            Source::None.code(),
            Source::Registry.code(),
            Source::DeviceLocalHeap.code(),
            Source::HostVisibleWindow.code(),
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}

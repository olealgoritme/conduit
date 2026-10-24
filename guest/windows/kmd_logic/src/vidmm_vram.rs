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
//! A usable backend VRAM limit bounds the reported capacity even when a larger
//! registry override or host heap is available.

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

/// Read the low/high words of the optional backend limit under one config generation.
/// Missing fields or a continuously changing generation leave the limit unavailable.
pub fn read_backend_limit(
    generation: impl Fn() -> u32,
    word: impl Fn(usize) -> Option<u32>,
) -> Option<u64> {
    for _ in 0..8 {
        let before = generation();
        let lo = word(0)?;
        let hi = word(1)?;
        if before == generation() {
            return Some(u64::from(lo) | (u64::from(hi) << 32));
        }
    }
    None
}

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
    /// The VRAM limit enforced by the backend.
    BackendLimit,
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
            Self::BackendLimit => 4,
            Self::None => 0,
        }
    }
}

/// The resolved capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// MiB; 0 selects the legacy topology. An invalid registry value is passed
    /// through for the segment builder to reject.
    pub mb: u32,
    pub source: Source,
    /// The selected capacity exceeded [`MAX_MB`] and the value is the cap.
    pub clamped: bool,
    /// An automatic source existed but its size was unusable (`VidVBad` carries it, in MiB).
    pub bad_mb: Option<u32>,
}

/// Resolve `VidMmVramMB`. `knob` is the registry value ([`AUTO`] when absent);
/// `device_local_heap` the host GPU's video memory from [`device_local_heap_bytes`];
/// `host_visible` the exact length the virtio host-visible shared-memory capability reports;
/// `backend_limit_mb` the enforced VRAM limit, or 0 when unlimited or unavailable.
pub fn resolve(
    knob: u32,
    device_local_heap: Option<u64>,
    host_visible: Option<u64>,
    backend_limit_mb: u64,
) -> Resolved {
    let mut resolved = resolve_capacity(knob, device_local_heap, host_visible);
    if backend_limit_mb != 0 && backend_limit_mb < u64::from(MIN_MB) {
        resolved.bad_mb = Some(backend_limit_mb as u32);
        return resolved;
    }
    if backend_limit_mb == 0
        || resolved.mb == 0
        || (knob != AUTO && !(MIN_MB..=MAX_MB).contains(&knob))
    {
        return resolved;
    }
    if u64::from(resolved.mb) > backend_limit_mb {
        resolved.mb = backend_limit_mb.min(u64::from(MAX_MB)) as u32;
        resolved.source = Source::BackendLimit;
        resolved.clamped = backend_limit_mb > u64::from(MAX_MB);
    }
    resolved
}

fn resolve_capacity(
    knob: u32,
    device_local_heap: Option<u64>,
    host_visible: Option<u64>,
) -> Resolved {
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
        let r = resolve(AUTO, Some(32_109 * MIB), Some(8 * GIB), 0);
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
            assert_eq!(resolve(AUTO, Some(24 * GIB), window, 0).mb, 24 * 1024);
        }
    }

    #[test]
    fn auto_keeps_a_card_smaller_than_the_window_at_its_own_size() {
        let r = resolve(AUTO, Some(8 * GIB - 100 * MIB), Some(16 * GIB), 0);
        assert_eq!(r.mb, 8 * 1024 - 100);
        assert_eq!(r.source, Source::DeviceLocalHeap);
    }

    #[test]
    fn a_bigger_card_is_capped() {
        let r = resolve(AUTO, Some(96 * GIB), Some(8 * GIB), 0);
        assert_eq!(r.mb, MAX_MB);
        assert!(r.clamped);
        assert_eq!(r.source, Source::DeviceLocalHeap);
        let r = resolve(AUTO, Some(u64::MAX), None, 0);
        assert_eq!(r.mb, MAX_MB);
        assert!(r.clamped);
    }

    #[test]
    fn the_cap_itself_is_not_clamped() {
        let r = resolve(AUTO, Some(64 * GIB), None, 0);
        assert_eq!(r.mb, MAX_MB);
        assert!(!r.clamped);
    }

    #[test]
    fn a_tiny_heap_falls_through_to_the_window() {
        let r = resolve(AUTO, Some(128 * MIB), Some(8 * GIB), 0);
        assert_eq!(r.mb, 8 * 1024);
        assert_eq!(r.source, Source::HostVisibleWindow);
    }

    #[test]
    fn minimum_heap_is_accepted() {
        let r = resolve(AUTO, Some(256 * MIB), None, 0);
        assert_eq!(r.mb, MIN_MB);
        assert_eq!(r.source, Source::DeviceLocalHeap);
    }

    #[test]
    fn without_a_heap_auto_is_the_window_as_before() {
        let r = resolve(AUTO, None, Some(6 * GIB), 0);
        assert_eq!(r.mb, 6 * 1024);
        assert_eq!(r.source, Source::HostVisibleWindow);
        assert_eq!(r.bad_mb, None);
    }

    #[test]
    fn without_either_auto_is_the_legacy_topology() {
        let r = resolve(AUTO, None, None, 0);
        assert_eq!(r.mb, 0);
        assert_eq!(r.source, Source::None);
        assert_eq!(r.bad_mb, None);
    }

    #[test]
    fn an_unusable_window_is_reported_bad() {
        // Not a whole number of MiB.
        let r = resolve(AUTO, None, Some(GIB + 1), 0);
        assert_eq!(r.mb, 0);
        assert_eq!(r.bad_mb, Some(1024));
        // Below the minimum and above the maximum.
        assert_eq!(resolve(AUTO, None, Some(128 * MIB), 0).mb, 0);
        assert_eq!(resolve(AUTO, None, Some(128 * MIB), 0).bad_mb, Some(128));
        assert_eq!(resolve(AUTO, None, Some(128 * GIB), 0).mb, 0);
        assert_eq!(
            resolve(AUTO, None, Some(u64::MAX), 0).bad_mb,
            Some(u32::MAX)
        );
    }

    #[test]
    fn an_explicit_value_wins_without_a_backend_limit() {
        for knob in [0, 256, 4096, 24_576, 65_536, 70_000] {
            let r = resolve(knob, Some(32_109 * MIB), Some(8 * GIB), 0);
            assert_eq!(r.mb, knob);
            assert_eq!(r.source, Source::Registry);
            assert!(!r.clamped);
            assert_eq!(r.bad_mb, None);
        }
    }

    #[test]
    fn explicit_zero_is_the_legacy_topology_even_with_a_heap() {
        assert_eq!(resolve(0, Some(32 * GIB), Some(8 * GIB), 0).mb, 0);
    }

    #[test]
    fn source_codes_are_distinct() {
        let codes = [
            Source::None.code(),
            Source::Registry.code(),
            Source::DeviceLocalHeap.code(),
            Source::HostVisibleWindow.code(),
            Source::BackendLimit.code(),
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn backend_limit_caps_the_host_heap() {
        let r = resolve(AUTO, Some(12 * GIB), Some(8 * GIB), 6144);
        assert_eq!(r.mb, 6144);
        assert_eq!(r.source, Source::BackendLimit);
        assert!(!r.clamped);
    }

    #[test]
    fn backend_limit_caps_the_window_fallback() {
        let r = resolve(AUTO, None, Some(8 * GIB), 6144);
        assert_eq!(r.mb, 6144);
        assert_eq!(r.source, Source::BackendLimit);
    }

    #[test]
    fn backend_limit_preserves_legacy_topology_without_usable_capacity() {
        for limit in [0, 6144, u64::MAX] {
            for window in [None, Some(128 * MIB), Some(GIB + 1)] {
                assert_eq!(
                    resolve(AUTO, None, window, limit),
                    resolve_capacity(AUTO, None, window),
                );
            }
        }
    }

    #[test]
    fn backend_limit_never_enlarges_a_smaller_heap() {
        let r = resolve(AUTO, Some(4 * GIB), Some(8 * GIB), 6144);
        assert_eq!(r.mb, 4096);
        assert_eq!(r.source, Source::DeviceLocalHeap);
    }

    #[test]
    fn registry_override_cannot_exceed_the_backend_limit() {
        let r = resolve(12 * 1024, Some(12 * GIB), Some(8 * GIB), 6144);
        assert_eq!(r.mb, 6144);
        assert_eq!(r.source, Source::BackendLimit);
        let r = resolve(4096, Some(12 * GIB), Some(8 * GIB), 6144);
        assert_eq!(r.mb, 4096);
        assert_eq!(r.source, Source::Registry);
    }

    #[test]
    fn backend_limit_preserves_legacy_and_invalid_overrides() {
        for knob in [0, 128, 70_000] {
            let r = resolve(knob, Some(12 * GIB), Some(8 * GIB), 6144);
            assert_eq!(r.mb, knob);
            assert_eq!(r.source, Source::Registry);
        }
    }

    #[test]
    fn unlimited_or_large_backend_limits_do_not_wrap_or_change_capacity() {
        for limit in [0, 12 * 1024, u64::MAX] {
            let r = resolve(AUTO, Some(12 * GIB), Some(8 * GIB), limit);
            assert_eq!(r.mb, 12 * 1024);
            assert_eq!(r.source, Source::DeviceLocalHeap);
        }
    }

    #[test]
    fn unusable_backend_limits_preserve_capacity_and_record_the_limit() {
        for limit in 1..u64::from(MIN_MB) {
            for (knob, heap, window) in [
                (AUTO, Some(12 * GIB), Some(8 * GIB)),
                (AUTO, None, Some(8 * GIB)),
                (AUTO, None, None),
                (4096, Some(12 * GIB), None),
                (0, Some(12 * GIB), None),
            ] {
                let mut expected = resolve_capacity(knob, heap, window);
                expected.bad_mb = Some(limit as u32);
                assert_eq!(resolve(knob, heap, window, limit), expected);
            }
        }
    }

    #[test]
    fn minimum_backend_limit_is_usable() {
        let r = resolve(AUTO, Some(12 * GIB), Some(8 * GIB), u64::from(MIN_MB));
        assert_eq!(r.mb, MIN_MB);
        assert_eq!(r.source, Source::BackendLimit);
        assert_eq!(r.bad_mb, None);
    }

    #[test]
    fn backend_limit_reads_both_words() {
        assert_eq!(
            read_backend_limit(|| 7, |i| Some([0x89ab_cdef, 0x0123_4567][i])),
            Some(0x0123_4567_89ab_cdef),
        );
        assert_eq!(read_backend_limit(|| 0, |_| Some(0)), Some(0));
    }

    #[test]
    fn backend_limit_retries_a_torn_read() {
        let reads = core::cell::Cell::new(0);
        let result = read_backend_limit(
            || {
                let n = reads.get();
                reads.set(n + 1);
                [0, 1, 1, 1][n]
            },
            |i| {
                Some(if reads.get() == 1 {
                    u32::MAX
                } else {
                    [6144, 0][i]
                })
            },
        );
        assert_eq!(result, Some(6144));
        assert_eq!(reads.get(), 4);
    }

    #[test]
    fn backend_limit_gives_up_on_continuous_config_changes() {
        let reads = core::cell::Cell::new(0);
        assert_eq!(
            read_backend_limit(
                || {
                    let n = reads.get();
                    reads.set(n + 1);
                    n
                },
                |_| Some(6144),
            ),
            None,
        );
        assert_eq!(reads.get(), 16);
    }

    #[test]
    fn backend_limit_is_unavailable_when_either_word_is_missing() {
        for missing in [0, 1] {
            assert_eq!(
                read_backend_limit(|| 0, |i| (i != missing).then_some(6144)),
                None,
            );
        }
    }
}

//! The device's VIRTIO Shared Memory Regions, as a vhost-user frontend
//! learns them.
//!
//! Two regions, found by the guest driver through
//! `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities (`virtio_get_shm_region`),
//! never by BAR number, and a third with `--venus`:
//!
//! | shmid | what | size |
//! | --- | --- | --- |
//! | 1 | the window: device memory the backend places (`shm.rs`) | `--window-mib`, 4 GiB |
//! | 2 | the UVM aperture: CUDA semaphore pools and managed memory (`nvidia/aperture.rs`) | 32 GiB |
//! | 3 | Venus host-visible blobs, at offsets the guest picks (docs/VENUS.md) | `--venus-hostmem-mib`, 8 GiB |
//!
//! conduit-vmm takes the window's size from its config (`gpu-forward.window-mib`,
//! the same 4 GiB default) and hard-codes the aperture (BAR 2 and BAR 4). QEMU >= 11.1 asks instead, with
//! `VHOST_USER_GET_SHMEM_CONFIG`, and lays the regions out itself -- one BAR
//! (BAR 4) holding them back to back in shmid order. The answer is built here,
//! where it can be tested without the vhost crates.
//!
//! The reply is indexed by shmid: `sizes[i]` is region `i`, zero where there
//! is none. Region 0 is unused because the driver predates this and looks the
//! window up as 1.

/// The window. Must match `NVGPU_SHM_ID` in `guest/linux/conduit_gpu.c`.
pub const SHM_ID_WINDOW: u8 = 1;
/// The UVM aperture. Must match `NVGPU_SHM_ID_APERTURE`.
pub const SHM_ID_APERTURE: u8 = 2;

/// `--window-mib` when not given: the window every guest CPU mapping of RM
/// memory goes through (VRAM through BAR1, host-allocated system memory).
///
/// 4 GiB because several NVK-on-RM processes each keep up to 256 MiB of
/// host-visible VRAM mapped for as long as it is allocated (guest/nvk-rm,
/// `NVK_RM_BAR_MB`), and 1 GiB ran out at four. It costs nothing until used:
/// the backend's memfd is sparse, QEMU reserves the range `MAP_NORESERVE`
/// (patch 0008) and conduit-vmm `PROT_NONE`, and the allocator keeps a free
/// list, not a page table. Nor does it move the BAR: QEMU rounds window +
/// aperture (+ Venus) up to a power of two, and 4 + 32 + 8 GiB is 64 GiB,
/// as 1 + 32 + 8 was. conduit-vmm's default (`gpu-forward.window-mib`) is the
/// same number and must stay so.
pub const WINDOW_MIB_DEFAULT: u64 = 4096;

/// The smallest window `--window-mib` takes: each of the allocator's three
/// zones must still be whole pages, and the smallest (uncached, 1/32) at
/// least 1 MiB.
pub const WINDOW_MIB_MIN: u64 = 32;
/// The largest. 64 GiB is QEMU's whole default shared-memory BAR; above it the
/// BAR doubles to 128 GiB or more, which firmware without the host's physical
/// address width will not place (docs/VENUS.md "Windows/OVMF guests").
pub const WINDOW_MIB_MAX: u64 = 65536;

/// The size of region 1 for `--window-mib`: a power of two (conduit-vmm makes
/// it a BAR of its own, and the allocator's zones are fractions of it) from
/// [`WINDOW_MIB_MIN`] to [`WINDOW_MIB_MAX`].
pub fn window_len(mib: u64) -> Result<u64, String> {
    if !mib.is_power_of_two() || !(WINDOW_MIB_MIN..=WINDOW_MIB_MAX).contains(&mib) {
        return Err(format!(
            "--window-mib {mib}: must be a power of two from {WINDOW_MIB_MIN} to {WINDOW_MIB_MAX}"
        ));
    }
    Ok(mib << 20)
}

/// Venus host-visible blobs (docs/VENUS.md). Advertised only with `--venus`,
/// and only to a frontend that asks (QEMU): conduit-vmm's BARs are fixed.
pub const SHM_ID_VENUS: u8 = 3;

/// `--venus-hostmem-mib` when not given (docs/VENUS.md). Like the aperture
/// it is address space, not memory: a blob costs only once it is mapped.
pub const VENUS_HOSTMEM_MIB_DEFAULT: u64 = 8192;

/// The size of region 3 for `--venus-hostmem-mib`, refused unless it is a
/// power of two (a BAR is) of at least a page.
pub fn venus_hostmem_len(mib: u64) -> Result<u64, String> {
    if mib == 0 || !mib.is_power_of_two() {
        return Err(format!("--venus-hostmem-mib {mib}: must be a power of two"));
    }
    mib.checked_mul(1 << 20)
        .ok_or_else(|| format!("--venus-hostmem-mib {mib}: too large"))
}

/// Size of the UVM aperture. conduit-vmm's `APERTURE_SIZE` is the same number, and
/// QEMU takes it from the reply built here.
///
/// It is guest-physical address space and nothing else: a pool gets a memory
/// slot only when it is placed, and the guest maps it with `remap_pfn_range`,
/// so an empty aperture costs neither memory nor `struct page`s. It bounds the
/// managed memory (`cuMemAllocManaged`) one VM can have at once, so it is the
/// size of the largest GPU memory there is. A power of two, because a BAR is.
pub const APERTURE_LEN: u64 = 32 << 30;

/// `VhostUserMMap.flags` bit asking the frontend to map the descriptor at host
/// address `fd_offset` (shared, read-write). Not in the vhost-user spec: a
/// Conduit extension carried by conduit/host/qemu/patches/0003, needed because
/// nvidia-uvm only accepts a mapping whose address equals its file offset, and
/// a spec frontend picks the address itself.
///
/// Sent only to a frontend that asked for `GET_SHMEM_CONFIG` (QEMU); conduit-vmm
/// never asks, maps pools at that address already, and refuses unknown bits.
/// A stock QEMU ignores the bit; its mapping of a UVM file then fails.
pub const MAP_FLAG_FIXED_VA: u64 = 1 << 1;

/// The flags for placing a UVM pool in the aperture.
pub fn pool_map_flags(writable_bit: u64, frontend_is_spec: bool) -> u64 {
    if frontend_is_spec {
        writable_bit | MAP_FLAG_FIXED_VA
    } else {
        writable_bit
    }
}

/// How many entries a `VHOST_USER_GET_SHMEM_CONFIG` reply carries.
pub const MAX_SHM_REGIONS: usize = 256;

const PAGE: u64 = 4096;

/// The region table a frontend is given: `(count of non-empty regions, size
/// by shmid)`.
///
/// Sizes are rounded up to a page, which the protocol requires and QEMU
/// enforces by refusing the device; a zero size leaves the region out.
pub fn region_sizes(window_len: u64, aperture_len: u64) -> (u32, [u64; MAX_SHM_REGIONS]) {
    region_sizes_with_venus(window_len, aperture_len, 0)
}

/// [`region_sizes`] with region 3 as well; `venus_len` zero leaves it out,
/// which is the device without `--venus`.
pub fn region_sizes_with_venus(
    window_len: u64,
    aperture_len: u64,
    venus_len: u64,
) -> (u32, [u64; MAX_SHM_REGIONS]) {
    let mut sizes = [0u64; MAX_SHM_REGIONS];
    sizes[SHM_ID_WINDOW as usize] = page_align(window_len);
    sizes[SHM_ID_APERTURE as usize] = page_align(aperture_len);
    sizes[SHM_ID_VENUS as usize] = page_align(venus_len);
    let n = sizes.iter().filter(|&&s| s != 0).count() as u32;
    (n, sizes)
}

/// Round a placement length up to a whole page.
///
/// A frontend that maps the descriptor itself (QEMU) builds a memory region
/// of exactly this length; KVM can only give a guest whole pages of it, so a
/// ragged tail would be emulated as a hole. It is also the length the
/// matching unmap has to repeat, so placement and withdrawal both go through
/// here. conduit-vmm mmaps, which rounds the same way, so nothing changes for it.
pub fn page_align(len: u64) -> u64 {
    len.checked_add(PAGE - 1)
        .map_or(u64::MAX & !(PAGE - 1), |v| v & !(PAGE - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::ZoneConfig;

    #[test]
    fn ids_are_the_drivers() {
        // guest/linux/conduit_gpu.c: NVGPU_SHM_ID 1, NVGPU_SHM_ID_APERTURE 2.
        assert_eq!(SHM_ID_WINDOW, 1);
        assert_eq!(SHM_ID_APERTURE, 2);
        // docs/VENUS.md: region 3.
        assert_eq!(SHM_ID_VENUS, 3);
    }

    #[test]
    fn the_default_device_has_a_window_and_an_aperture() {
        let window = ZoneConfig::default_1gib().total();
        let (n, sizes) = region_sizes(window, APERTURE_LEN);
        assert_eq!(n, 2);
        assert_eq!(sizes[0], 0, "region 0 is unused");
        assert_eq!(sizes[1], 1 << 30, "window, as conduit-vmm's SHM_SIZE");
        assert_eq!(
            sizes[2],
            32 << 30,
            "aperture, as conduit-vmm's APERTURE_SIZE"
        );
        assert!(sizes[3..].iter().all(|&s| s == 0));
    }

    #[test]
    fn an_absent_aperture_is_left_out() {
        let (n, sizes) = region_sizes(1 << 30, 0);
        assert_eq!(n, 1);
        assert_eq!(sizes[SHM_ID_APERTURE as usize], 0);
    }

    /// Region 3 is there only when asked for, after the other two.
    #[test]
    fn the_venus_region_is_advertised_only_with_venus() {
        let window = ZoneConfig::default_1gib().total();
        assert_eq!(
            region_sizes_with_venus(window, APERTURE_LEN, 0),
            region_sizes(window, APERTURE_LEN)
        );
        let len = venus_hostmem_len(VENUS_HOSTMEM_MIB_DEFAULT).unwrap();
        let (n, sizes) = region_sizes_with_venus(window, APERTURE_LEN, len);
        assert_eq!(n, 3);
        assert_eq!(sizes[SHM_ID_VENUS as usize], 8 << 30);
        assert_eq!(sizes[1], 1 << 30);
        assert_eq!(sizes[2], 32 << 30);
        assert!(sizes[4..].iter().all(|&s| s == 0));
    }

    #[test]
    fn window_size_is_checked() {
        assert_eq!(window_len(WINDOW_MIB_DEFAULT), Ok(4 << 30));
        assert_eq!(window_len(1024), Ok(1 << 30));
        assert_eq!(window_len(WINDOW_MIB_MIN), Ok(32 << 20));
        assert_eq!(window_len(WINDOW_MIB_MAX), Ok(64 << 30));
        for bad in [0, 16, 3000, 4095, 131072, 1 << 62] {
            assert!(window_len(bad).is_err(), "{bad}");
        }
    }

    /// The default window does not change QEMU's BAR (patch 0006 rounds the
    /// regions' total up to a power of two): 64 GiB with Venus or without,
    /// as with the old 1 GiB window.
    #[test]
    fn the_default_window_keeps_the_bar_at_64_gib() {
        let bar = |window: u64, venus: u64| {
            let (_, sizes) = region_sizes_with_venus(window, APERTURE_LEN, venus);
            sizes.iter().sum::<u64>().next_power_of_two()
        };
        let venus = venus_hostmem_len(VENUS_HOSTMEM_MIB_DEFAULT).unwrap();
        let window = window_len(WINDOW_MIB_DEFAULT).unwrap();
        assert_eq!(bar(1 << 30, venus), 64 << 30);
        assert_eq!(bar(window, venus), 64 << 30);
        assert_eq!(bar(window, 0), 64 << 30);
        // 16 GiB still fits; the 64 GiB maximum doubles it.
        assert_eq!(bar(16 << 30, venus), 64 << 30);
        assert_eq!(bar(64 << 30, venus), 128 << 30);
    }

    /// The allocator's zones cover exactly the window, for every size taken.
    #[test]
    fn every_window_size_splits_into_whole_page_zones() {
        let mut mib = WINDOW_MIB_MIN;
        while mib <= WINDOW_MIB_MAX {
            let len = window_len(mib).unwrap();
            let z = ZoneConfig::for_window(len);
            assert_eq!(z.total(), len, "{mib} MiB");
            for zone in [z.uc_size, z.wc_size, z.wb_size] {
                assert!(zone >= 1 << 20 && zone % 4096 == 0, "{mib} MiB");
            }
            mib *= 2;
        }
    }

    #[test]
    fn venus_hostmem_must_be_a_power_of_two() {
        assert_eq!(venus_hostmem_len(1), Ok(1 << 20));
        assert_eq!(venus_hostmem_len(8192), Ok(8 << 30));
        assert!(venus_hostmem_len(0).is_err());
        assert!(venus_hostmem_len(3000).is_err());
        assert!(venus_hostmem_len(1 << 63).is_err(), "overflows bytes");
    }

    #[test]
    fn sizes_are_whole_pages() {
        let (_, sizes) = region_sizes(4097, 1);
        assert_eq!(sizes[1], 8192);
        assert_eq!(sizes[2], 4096);
    }

    #[test]
    fn page_align_rounds_up_and_never_wraps() {
        assert_eq!(page_align(0), 0);
        assert_eq!(page_align(1), 4096);
        assert_eq!(page_align(4096), 4096);
        assert_eq!(page_align(4097), 8192);
        assert_eq!(page_align(u64::MAX), u64::MAX & !4095);
    }

    #[test]
    fn only_a_spec_frontend_is_asked_for_a_fixed_address() {
        // conduit-vmm validates flags against WRITABLE alone.
        assert_eq!(pool_map_flags(1, false), 1);
        assert_eq!(pool_map_flags(1, true), 1 | MAP_FLAG_FIXED_VA);
        // Bit 0 is the spec's read-write bit; the extension must not reuse it.
        assert_eq!(MAP_FLAG_FIXED_VA & 1, 0);
    }

    /// The reply's wire form: u32 count, u32 padding, 256 u64 sizes. The
    /// vhost crate's `VhostUserShMemConfig` and QEMU's both have this shape.
    #[test]
    fn the_table_fits_the_wire_message() {
        let (_, sizes) = region_sizes(1 << 30, 1 << 30);
        assert_eq!(4 + 4 + std::mem::size_of_val(&sizes), 2056);
    }
}

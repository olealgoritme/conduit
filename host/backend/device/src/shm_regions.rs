//! The device's VIRTIO Shared Memory Regions, as a vhost-user frontend
//! learns them.
//!
//! Two regions, found by the guest driver through
//! `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities (`virtio_get_shm_region`),
//! never by BAR number:
//!
//! | shmid | what | size |
//! | --- | --- | --- |
//! | 1 | the window: device memory the backend places (`shm.rs`) | the allocator's total, 1 GiB |
//! | 2 | the UVM aperture: CUDA semaphore pools and managed memory (`nvidia/aperture.rs`) | 32 GiB |
//!
//! conduit-vmm hard-codes both (BAR 2 and BAR 4). QEMU >= 11.1 asks instead, with
//! `VHOST_USER_GET_SHMEM_CONFIG`, and lays the regions out itself -- one BAR
//! (BAR 4) holding them back to back in shmid order. The answer is built here,
//! where it can be tested without the vhost crates.
//!
//! The reply is indexed by shmid: `sizes[i]` is region `i`, zero where there
//! is none. Region 0 is unused because the driver predates this and looks the
//! window up as 1.

/// The window. Must match `NVGPU_SHM_ID` in `driver/virtio_gpu_nv.c`.
pub const SHM_ID_WINDOW: u8 = 1;
/// The UVM aperture. Must match `NVGPU_SHM_ID_APERTURE`.
pub const SHM_ID_APERTURE: u8 = 2;

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
    let mut sizes = [0u64; MAX_SHM_REGIONS];
    sizes[SHM_ID_WINDOW as usize] = page_align(window_len);
    sizes[SHM_ID_APERTURE as usize] = page_align(aperture_len);
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
        // driver/virtio_gpu_nv.c: NVGPU_SHM_ID 1, NVGPU_SHM_ID_APERTURE 2.
        assert_eq!(SHM_ID_WINDOW, 1);
        assert_eq!(SHM_ID_APERTURE, 2);
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

//! The device's VIRTIO Shared Memory Regions, as a vhost-user frontend
//! learns them.
//!
//! Two regions, found by the guest driver through
//! `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities (`virtio_get_shm_region`),
//! never by BAR number, and a third with `--venus`:
//!
//! | shmid | what | size |
//! | --- | --- | --- |
//! | 1 | the window: device memory the backend places (`shm.rs`) | `--window-mib`, the host GPU's BAR1 (`auto`) |
//! | 2 | the UVM aperture: CUDA semaphore pools and managed memory (`nvidia/aperture.rs`) | 32 GiB |
//! | 3 | Venus host-visible blobs, at offsets the guest picks (docs/VENUS.md) | `--venus-hostmem-mib`, 8 GiB |
//!
//! conduit-vmm takes the window's size from its config (`gpu-forward.window-mib`,
//! `auto` by default, the same rule as the backend's) and hard-codes the aperture (BAR 2 and BAR 4). QEMU >= 11.1 asks instead, with
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

/// The window when `--window-mib auto` has nothing to size it by (no NVIDIA
/// GPU in sysfs), and the smallest window `auto` picks. The window is where
/// every guest CPU mapping of RM memory goes (VRAM through BAR1,
/// host-allocated system memory).
///
/// 4 GiB because several NVK-on-RM processes each keep up to 256 MiB of
/// host-visible VRAM mapped for as long as it is allocated (guest/nvk-rm,
/// `NVK_RM_BAR_MB`), and 1 GiB ran out at four. conduit-vmm's fallback is
/// the same number.
///
/// What a window costs, at any size: the backend's memfd is sparse (memfds
/// are `VM_NORESERVE`: only pages written are charged), QEMU
/// reserves the range `MAP_PRIVATE | MAP_NORESERVE` (patch 0008) and
/// conduit-vmm `PROT_NONE`, so it is address space, not memory; the
/// allocator keeps a free list, not a page table. The one cost that scales:
/// under QEMU the window is one KVM memory slot, and if KVM ever needs its
/// shadow MMU (the guest runs Hyper-V: Windows VBS/HVCI, WSL2) it gives every
/// slot a reverse map of 8 bytes per 4 KiB page, 2 MiB of host kernel memory
/// per GiB of window (64 MiB at 32 GiB, 256 MiB at 128 GiB). With the TDP MMU
/// alone (the default, no nested guest) it is never allocated.
pub const WINDOW_MIB_DEFAULT: u64 = 4096;

/// The smallest window `--window-mib` takes: each of the allocator's three
/// zones must still be whole pages, and the smallest (uncached, 1/32) at
/// least 1 MiB.
pub const WINDOW_MIB_MIN: u64 = 32;

/// The largest window `--window-mib` takes on any host: 4 TiB. Under QEMU the
/// window is one KVM memory slot (patch 0008), and KVM refuses a slot of 2^31
/// pages or more (`KVM_MEM_MAX_NR_PAGES`, 8 TiB less a page on x86), so 4 TiB
/// is the largest power of two that can be one. The guest's 64-bit MMIO
/// window binds first on today's hosts: see [`window_mib_limit`].
pub const WINDOW_MIB_MAX: u64 = 4 << 20;

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

/// `--window-mib`: `auto` (the default) or a number of MiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMib {
    /// The host GPU's BAR1, as Resizable BAR gives a bare-metal driver
    /// ([`auto_window_mib`]).
    Auto,
    Mib(u64),
}

impl std::str::FromStr for WindowMib {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            n => n
                .parse::<u64>()
                .map(Self::Mib)
                .map_err(|_| format!("{n}: give auto or a number of MiB")),
        }
    }
}

impl std::fmt::Display for WindowMib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Mib(n) => write!(f, "{n}"),
        }
    }
}

/// OVMF gives a guest without 5-level paging at most 46 physical address bits
/// (edk2 `PlatformAddressWidthFromCpuid`). Seen in `win11`: a 48-bit host,
/// and OVMF's 64-bit MMIO window ends at 2^46.
pub const OVMF_PHYS_BITS_MAX: u8 = 46;

/// Physical address bits assumed when the CPU does not say: 39, what many
/// desktop Intel parts report and the narrowest Conduit supports.
pub const PHYS_BITS_UNKNOWN: u8 = 39;

/// The guest's 64-bit PCI MMIO window on a host with `phys_bits` physical
/// address bits, which the guest sees as its own (QEMU `host-phys-bits=on`,
/// libvirt `<maxphysaddr mode='passthrough'/>`, which `conduit up` and
/// `conduit attach` set).
///
/// OVMF (`PlatformDynamicMmioWindow`) takes the top eighth of the address
/// space it allows itself, `min(phys_bits, 46)`: 8 TiB at 46 bits and up
/// (`win11`: 0x3800_0000_0000..0x4000_0000_0000), 64 GiB at 39. conduit-vmm's
/// window (`layout::mmio64_window`) is at least as large.
pub fn guest_mmio64_len(phys_bits: Option<u8>) -> u64 {
    let bits = phys_bits
        .unwrap_or(PHYS_BITS_UNKNOWN)
        .clamp(36, OVMF_PHYS_BITS_MAX);
    1u64 << (bits - 3)
}

/// The shared-memory BAR QEMU makes (patch 0006): every region back to back,
/// rounded up to a power of two.
pub fn qemu_bar_len(window_len: u64, venus_len: u64) -> u64 {
    let (_, sizes) = region_sizes_with_venus(window_len, APERTURE_LEN, venus_len);
    sizes.iter().sum::<u64>().next_power_of_two()
}

/// The largest window, in MiB, a host with `phys_bits` can give a guest whose
/// device also has `venus_len` bytes of region 3 (0 without `--venus`): the
/// largest power of two whose BAR ([`qemu_bar_len`]) takes at most half the
/// guest's 64-bit MMIO window ([`guest_mmio64_len`]), and at most
/// [`WINDOW_MIB_MAX`] (one KVM slot). The other half is for every other
/// 64-bit BAR and the PCIe root ports' prefetchable reserves (OVMF gives each
/// hotplug port 32 GiB of it). Never below [`WINDOW_MIB_DEFAULT`], what every
/// host was given before.
///
/// | host bits | guest 64-bit MMIO | BAR at most | window at most |
/// | --- | --- | --- | --- |
/// | 46 and up | 8 TiB | 4 TiB | 2 TiB |
/// | 43 | 1 TiB | 512 GiB | 256 GiB |
/// | 41 | 256 GiB | 128 GiB | 64 GiB |
/// | 39 | 64 GiB | 32 GiB | 4 GiB (the floor; its BAR is 64 GiB) |
pub fn window_mib_limit(phys_bits: Option<u8>, venus_len: u64) -> u64 {
    let room = guest_mmio64_len(phys_bits) / 2;
    let mut mib = WINDOW_MIB_MAX;
    while mib > WINDOW_MIB_DEFAULT && qemu_bar_len(mib << 20, venus_len) > room {
        mib /= 2;
    }
    mib
}

/// `--window-mib auto`: the host GPU's BAR1 (`bar1_len`, bytes) rounded up to
/// a power of two, from [`WINDOW_MIB_DEFAULT`] to [`window_mib_limit`];
/// [`WINDOW_MIB_DEFAULT`] when there is no BAR1 to go by. With Resizable BAR
/// a bare-metal driver can map all of VRAM through BAR1, and BAR1 is what the
/// card offers (32 GiB on an RTX 5090, 128 GiB on an RTX PRO 6000), so the
/// guest gets as much CPU-mappable GPU memory as the host has.
pub fn auto_window_mib(bar1_len: Option<u64>, phys_bits: Option<u8>, venus_len: u64) -> u64 {
    let Some(bar1) = bar1_len.filter(|&b| b > 0) else {
        return WINDOW_MIB_DEFAULT;
    };
    let mib = bar1
        .div_ceil(1 << 20)
        .checked_next_power_of_two()
        .unwrap_or(WINDOW_MIB_MAX);
    mib.clamp(WINDOW_MIB_DEFAULT, window_mib_limit(phys_bits, venus_len))
}

/// The largest BAR1 among the NVIDIA GPUs bound to the `nvidia` driver, from
/// `<pci_devices>/<addr>/resource` (its second line is BAR1). `None` when
/// there is none.
pub fn host_bar1_len(pci_devices: &std::path::Path) -> Option<u64> {
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).ok();
    let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    std::fs::read_dir(pci_devices)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let d = e.path();
            let nvidia = hex(&read(d.join("vendor"))?)? == 0x10de;
            let display = hex(&read(d.join("class"))?)? >> 16 == 0x03;
            let driver = std::fs::read_link(d.join("driver")).ok()?;
            if !nvidia || !display || driver.file_name()? != "nvidia" {
                return None;
            }
            let res = read(d.join("resource"))?;
            let mut f = res.lines().nth(1)?.split_whitespace();
            let (start, end) = (hex(f.next()?)?, hex(f.next()?)?);
            (end > start).then(|| end - start + 1)
        })
        .max()
}

/// This CPU's physical address bits (CPUID 0x8000_0008, EAX[7:0]).
pub fn host_phys_bits() -> Option<u8> {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        #[allow(unused_unsafe)]
        let max = unsafe { __cpuid(0x8000_0000) }.eax;
        if max < 0x8000_0008 {
            return None;
        }
        #[allow(unused_unsafe)]
        let bits = (unsafe { __cpuid(0x8000_0008) }.eax & 0xff) as u8;
        (bits != 0).then_some(bits)
    }
    #[cfg(not(target_arch = "x86_64"))]
    None
}

/// The host's own figures for `auto`: `(BAR1 bytes, physical address bits)`.
pub fn host_window_inputs() -> (Option<u64>, Option<u8>) {
    (
        host_bar1_len(std::path::Path::new("/sys/bus/pci/devices")),
        host_phys_bits(),
    )
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
        assert_eq!(window_len(65536), Ok(64 << 30));
        assert_eq!(window_len(131072), Ok(128 << 30));
        assert_eq!(window_len(WINDOW_MIB_MAX), Ok(4 << 40));
        for bad in [0, 16, 3000, 4095, 32769, 8 << 20, 1 << 62] {
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
        // 16 GiB still fits; 32 GiB (an RTX 5090's BAR1) doubles it, and a
        // window of 64 GiB or more makes a BAR of twice the window.
        assert_eq!(bar(16 << 30, venus), 64 << 30);
        assert_eq!(bar(32 << 30, venus), 128 << 30);
        assert_eq!(bar(32 << 30, 0), 64 << 30);
        assert_eq!(bar(64 << 30, venus), 128 << 30);
        assert_eq!(bar(128 << 30, venus), 256 << 30);
        assert_eq!(qemu_bar_len(32 << 30, venus), 128 << 30);
    }

    #[test]
    fn window_mib_parses_auto_and_numbers() {
        assert_eq!("auto".parse(), Ok(WindowMib::Auto));
        assert_eq!("32768".parse(), Ok(WindowMib::Mib(32768)));
        assert!("".parse::<WindowMib>().is_err());
        assert!("32g".parse::<WindowMib>().is_err());
        assert_eq!(WindowMib::Auto.to_string(), "auto");
        assert_eq!(WindowMib::Mib(8192).to_string(), "8192");
    }

    /// OVMF's 64-bit window: the top eighth of at most 46 bits.
    #[test]
    fn the_guest_mmio64_window_follows_the_phys_bits() {
        assert_eq!(guest_mmio64_len(Some(48)), 8 << 40, "win11's host");
        assert_eq!(guest_mmio64_len(Some(46)), 8 << 40);
        assert_eq!(guest_mmio64_len(Some(52)), 8 << 40);
        assert_eq!(guest_mmio64_len(Some(43)), 1 << 40);
        assert_eq!(guest_mmio64_len(Some(39)), 64 << 30);
        assert_eq!(guest_mmio64_len(None), 64 << 30, "unknown: 39 bits");
    }

    /// The table on `window_mib_limit`.
    #[test]
    fn the_window_limit_leaves_half_the_mmio64_window() {
        let venus = venus_hostmem_len(VENUS_HOSTMEM_MIB_DEFAULT).unwrap();
        for v in [0, venus] {
            assert_eq!(window_mib_limit(Some(48), v), 2 << 20, "2 TiB");
            assert_eq!(window_mib_limit(Some(46), v), 2 << 20);
            assert_eq!(window_mib_limit(Some(43), v), 256 << 10);
            assert_eq!(window_mib_limit(Some(41), v), 64 << 10);
            assert_eq!(window_mib_limit(Some(39), v), WINDOW_MIB_DEFAULT);
            assert_eq!(window_mib_limit(None, v), WINDOW_MIB_DEFAULT);
        }
        // A large region 3 takes room from the window.
        assert_eq!(window_mib_limit(Some(43), 256 << 30), 128 << 10);
        for bits in [39u8, 41, 43, 46, 48, 52] {
            let mib = window_mib_limit(Some(bits), venus);
            assert!(window_len(mib).is_ok(), "{bits} bits: {mib}");
            if mib > WINDOW_MIB_DEFAULT {
                assert!(
                    qemu_bar_len(mib << 20, venus) <= guest_mmio64_len(Some(bits)) / 2,
                    "{bits} bits"
                );
                assert!(
                    qemu_bar_len(mib << 21, venus) > guest_mmio64_len(Some(bits)) / 2,
                    "{bits} bits: the next size up would fit too"
                );
            }
        }
    }

    #[test]
    fn auto_is_the_bar1_rounded_up_and_clamped() {
        let venus = venus_hostmem_len(VENUS_HOSTMEM_MIB_DEFAULT).unwrap();
        let auto = |bar1: Option<u64>, bits| auto_window_mib(bar1, bits, venus);
        // RTX 5090 on win11's 48-bit host.
        assert_eq!(auto(Some(32 << 30), Some(48)), 32768);
        // RTX PRO 6000: 128 GiB BAR1, not clamped to 64 GiB.
        assert_eq!(auto(Some(128 << 30), Some(48)), 131072);
        assert_eq!(auto(Some(128 << 30), Some(46)), 131072);
        // Not a power of two: rounded up.
        assert_eq!(auto(Some(24 << 30), Some(48)), 32768);
        assert_eq!(auto(Some((32 << 30) + 1), Some(48)), 65536);
        // A small BAR1 (no Resizable BAR: 256 MiB) still gets the floor.
        assert_eq!(auto(Some(256 << 20), Some(48)), WINDOW_MIB_DEFAULT);
        // No GPU, or a BAR1 of nothing: the fallback.
        assert_eq!(auto(None, Some(48)), WINDOW_MIB_DEFAULT);
        assert_eq!(auto(Some(0), Some(48)), WINDOW_MIB_DEFAULT);
        // Clamped to what the address space holds.
        assert_eq!(auto(Some(128 << 30), Some(41)), 65536);
        assert_eq!(auto(Some(32 << 30), Some(39)), WINDOW_MIB_DEFAULT);
        assert_eq!(auto(Some(32 << 30), None), WINDOW_MIB_DEFAULT);
        assert_eq!(auto(Some(16 << 40), Some(48)), 2 << 20, "2 TiB at most");
        assert_eq!(auto(Some(u64::MAX), Some(52)), 2 << 20);
        // Always a window the backend takes.
        for bar1 in [1u64, 256 << 20, 32 << 30, 96 << 30, 1 << 50, u64::MAX] {
            for bits in [None, Some(36), Some(39), Some(46), Some(48), Some(57)] {
                let mib = auto(Some(bar1), bits);
                assert!(window_len(mib).is_ok(), "{bar1:#x} {bits:?}: {mib}");
            }
        }
    }

    /// A fixture sysfs: two GPUs on `nvidia`, one on `vfio-pci`, a non-GPU.
    #[test]
    fn bar1_is_read_from_sysfs() {
        let root = std::env::temp_dir().join(format!("conduit-bar1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = |addr: &str, vendor: &str, class: &str, driver: &str, bar1: (u64, u64)| {
            let d = root.join("devices").join(addr);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(d.join("class"), format!("{class}\n")).unwrap();
            let drv = root.join("drivers").join(driver);
            std::fs::create_dir_all(&drv).unwrap();
            std::os::unix::fs::symlink(&drv, d.join("driver")).unwrap();
            let line = |s: u64, e: u64| format!("0x{s:016x} 0x{e:016x} 0x000000000014220c\n");
            let res = line(0xd800_0000, 0xdbff_ffff) + &line(bar1.0, bar1.1) + &line(0, 0);
            std::fs::write(d.join("resource"), res).unwrap();
        };
        let devices = root.join("devices");
        assert_eq!(host_bar1_len(&devices), None, "no tree");
        dev(
            "0000:00:02.0",
            "0x8086",
            "0x030000",
            "i915",
            (0x40_0000_0000, 0x7f_ffff_ffff),
        );
        assert_eq!(host_bar1_len(&devices), None, "not NVIDIA");
        dev(
            "0000:01:00.0",
            "0x10de",
            "0x030000",
            "nvidia",
            (0x10_0000_0000, 0x17_ffff_ffff),
        );
        assert_eq!(host_bar1_len(&devices), Some(32 << 30), "win11's RTX 5090");
        dev(
            "0000:02:00.0",
            "0x10de",
            "0x030000",
            "vfio-pci",
            (0x80_0000_0000, 0xff_ffff_ffff),
        );
        assert_eq!(
            host_bar1_len(&devices),
            Some(32 << 30),
            "passed through: not ours"
        );
        dev(
            "0000:03:00.0",
            "0x10de",
            "0x030200",
            "nvidia",
            (0x200_0000_0000, 0x21f_ffff_ffff),
        );
        assert_eq!(host_bar1_len(&devices), Some(128 << 30), "the largest");
        dev(
            "0000:04:00.1",
            "0x10de",
            "0x040300",
            "nvidia",
            (0x300_0000_0000, 0x3ff_ffff_ffff),
        );
        assert_eq!(host_bar1_len(&devices), Some(128 << 30), "audio function");
        let _ = std::fs::remove_dir_all(&root);
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

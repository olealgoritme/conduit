//! Display names for the numbers a trace records.
//!
//! RM classes and control commands, UVM commands, DRM ioctls and NVKMS
//! commands, read out of NVIDIA's headers (and the kernel's `drm.h`) by
//! `gen/names_extract.py`. These are for people reading a trace; nothing in
//! the backend decides anything by a name.

mod table;

use crate::version::DriverVersion;

fn find(table: &'static [(u32, &'static str)], key: u32) -> Option<&'static str> {
    table
        .binary_search_by_key(&key, |&(k, _)| k)
        .ok()
        .map(|i| table[i].1)
}

/// An RM class, e.g. `0xc86f` -> `HOPPER_CHANNEL_GPFIFO_A`.
pub fn class(id: u32) -> Option<&'static str> {
    find(table::CLASSES, id)
}

/// An RM control command, e.g. `0x20800102` -> `NV2080_CTRL_CMD_GPU_GET_INFO_V2`.
pub fn control(cmd: u32) -> Option<&'static str> {
    find(table::CONTROLS, cmd)
}

/// A UVM command, by its whole ioctl number.
pub fn uvm(cmd: u32) -> Option<&'static str> {
    find(table::UVM, cmd)
}

/// A DRM ioctl by its nr (the low byte), core or nvidia-drm.
pub fn drm(nr: u32) -> Option<&'static str> {
    find(table::DRM, nr)
}

/// An NVKMS command by its index, for the host release.
///
/// The index is a position in an enum that changes between releases, so the
/// list used is the newest one not newer than `v`. With no release known,
/// nothing is named.
pub fn nvkms(v: Option<DriverVersion>, cmd: u32) -> Option<&'static str> {
    let v = v?;
    let (_, list) = table::NVKMS.iter().rev().find(|(r, _)| *r <= v)?;
    list.get(cmd as usize).copied()
}

/// An `NV_ESC_*` escape (the low byte of an ioctl on an NVIDIA node).
pub fn escape(nr: u32) -> Option<&'static str> {
    use crate::ioctl::*;
    Some(match nr {
        NV_ESC_CARD_INFO => "CARD_INFO",
        NV_ESC_REGISTER_FD => "REGISTER_FD",
        NV_ESC_ALLOC_OS_EVENT => "ALLOC_OS_EVENT",
        NV_ESC_FREE_OS_EVENT => "FREE_OS_EVENT",
        NV_ESC_STATUS_CODE => "STATUS_CODE",
        NV_ESC_CHECK_VERSION_STR => "CHECK_VERSION_STR",
        NV_ESC_IOCTL_XFER_CMD => "IOCTL_XFER_CMD",
        NV_ESC_ATTACH_GPUS_TO_FD => "ATTACH_GPUS_TO_FD",
        NV_ESC_QUERY_DEVICE_INTR => "QUERY_DEVICE_INTR",
        NV_ESC_SYS_PARAMS => "SYS_PARAMS",
        NV_ESC_NUMA_INFO => "NUMA_INFO",
        NV_ESC_EXPORT_TO_DMABUF_FD => "EXPORT_TO_DMABUF_FD",
        NV_ESC_WAIT_OPEN_COMPLETE => "WAIT_OPEN_COMPLETE",
        NV_ESC_RM_ALLOC_MEMORY => "RM_ALLOC_MEMORY",
        NV_ESC_RM_ALLOC_OBJECT => "RM_ALLOC_OBJECT",
        NV_ESC_RM_FREE => "RM_FREE",
        NV_ESC_RM_CONTROL => "RM_CONTROL",
        NV_ESC_RM_ALLOC => "RM_ALLOC",
        NV_ESC_RM_DUP_OBJECT => "RM_DUP_OBJECT",
        NV_ESC_RM_SHARE => "RM_SHARE",
        NV_ESC_RM_I2C_ACCESS => "RM_I2C_ACCESS",
        NV_ESC_RM_IDLE_CHANNELS => "RM_IDLE_CHANNELS",
        NV_ESC_RM_VID_HEAP_CONTROL => "RM_VID_HEAP_CONTROL",
        NV_ESC_RM_ACCESS_REGISTRY => "RM_ACCESS_REGISTRY",
        NV_ESC_RM_MAP_MEMORY => "RM_MAP_MEMORY",
        NV_ESC_RM_UNMAP_MEMORY => "RM_UNMAP_MEMORY",
        NV_ESC_RM_GET_EVENT_DATA => "RM_GET_EVENT_DATA",
        NV_ESC_RM_ALLOC_CONTEXT_DMA2 => "RM_ALLOC_CONTEXT_DMA2",
        NV_ESC_RM_ADD_VBLANK_CALLBACK => "RM_ADD_VBLANK_CALLBACK",
        NV_ESC_RM_MAP_MEMORY_DMA => "RM_MAP_MEMORY_DMA",
        NV_ESC_RM_UNMAP_MEMORY_DMA => "RM_UNMAP_MEMORY_DMA",
        NV_ESC_RM_BIND_CONTEXT_DMA => "RM_BIND_CONTEXT_DMA",
        NV_ESC_RM_EXPORT_OBJECT_TO_FD => "RM_EXPORT_OBJECT_TO_FD",
        NV_ESC_RM_IMPORT_OBJECT_FROM_FD => "RM_IMPORT_OBJECT_FROM_FD",
        NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => "RM_UPDATE_DEVICE_MAPPING_INFO",
        NV_ESC_RM_LOCKLESS_DIAGNOSTIC => "RM_LOCKLESS_DIAGNOSTIC",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(t: &[(u32, &str)]) -> bool {
        t.windows(2).all(|w| w[0].0 < w[1].0)
    }

    #[test]
    fn tables_are_sorted_for_binary_search() {
        assert!(sorted(table::CLASSES));
        assert!(sorted(table::CONTROLS));
        assert!(sorted(table::UVM));
        assert!(sorted(table::DRM));
        assert!(table::NVKMS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn well_known_numbers_have_their_names() {
        assert_eq!(class(0x41), Some("NV01_ROOT_CLIENT"));
        assert_eq!(control(0x20800102), Some("NV2080_CTRL_CMD_GPU_GET_INFO_V2"));
        assert_eq!(uvm(0x30000001), Some("UVM_INITIALIZE"));
        assert_eq!(drm(0x41), Some("NVIDIA_GEM_IMPORT_NVKMS_MEMORY"));
        assert_eq!(drm(0x00), Some("VERSION"));
        assert_eq!(escape(0x2a), Some("RM_CONTROL"));
    }

    /// REGISTER_SURFACE moves: 16 in 535, 17 from 580 through 610, 16 in 615.
    #[test]
    fn nvkms_follows_the_release() {
        let reg = "NVKMS_IOCTL_REGISTER_SURFACE";
        assert_eq!(nvkms(Some(DriverVersion::new(535, 129, 3)), 16), Some(reg));
        assert_eq!(nvkms(Some(DriverVersion::new(580, 178, 4)), 17), Some(reg));
        assert_eq!(nvkms(Some(DriverVersion::new(610, 57, 4)), 17), Some(reg));
        assert_eq!(nvkms(Some(DriverVersion::new(615, 71, 9)), 16), Some(reg));
        assert_eq!(nvkms(None, 0), None);
    }
}

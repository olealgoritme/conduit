//! `virtio_drivers::transport::pci::bus::ConfigurationAccess` backed by the
//! Dxgkrnl `DxgkCbReadDeviceSpace`/`DxgkCbWriteDeviceSpace` callbacks.
//!
//! A WDDM miniport does not own the PCI bus, so it cannot poke CAM/ECAM
//! directly; instead Dxgkrnl exposes our device's config space through these
//! callbacks (already scoped to our device by the `DeviceHandle`). We therefore
//! ignore the `DeviceFunction` argument and always access our own device.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction};

use crate::dxgk::*;

/// PCI config-space accessor over Dxgkrnl. `Copy` (handle + two callback
/// pointers) so `unsafe_clone` and `PciRoot`'s cloning needs are trivial.
#[derive(Clone, Copy)]
pub struct DxgkConfigAccess {
    handle: HANDLE,
    read: DXGKCB_READ_DEVICE_SPACE,
    write: DXGKCB_WRITE_DEVICE_SPACE,
}

impl DxgkConfigAccess {
    /// Capture the device handle + config-space callbacks saved in StartDevice.
    pub fn new(dxgkrnl: &DXGKRNL_INTERFACE) -> Self {
        Self {
            handle: dxgkrnl.DeviceHandle,
            read: dxgkrnl.DxgkCbReadDeviceSpace,
            write: dxgkrnl.DxgkCbWriteDeviceSpace,
        }
    }
}

/// virtio-drivers 0.13 knows device types up to 25 and `PciTransport::new`
/// refuses any other PCI device id, so Conduit's device (virtio id 45, PCI
/// `0x106D`; id 41, `0x1069`, under some QEMU setups; or the RTX 5090
/// personality's `0x2B85`) never got a transport and `StartDevice` silently
/// left the device off. The transport only reads the vendor/device word at
/// offset 0 and keeps the type for nothing we use, so report the virtio-gpu id
/// (`0x1050`) for those IDs and leave every other read, including the KMD's own
/// capability scans, untouched.
///
/// The transport also refuses any vendor but `0x1AF4`, so the RTX 5090
/// personality (`10DE:2B85`) reports the virtio vendor too.
/// Whether the device is the RTX 5090 personality (`10DE:2B85`), as the transport's read of
/// the vendor/device word saw it (set before the transport exists, never cleared: the PCI
/// identity of a device does not change while the driver is loaded).
static NVIDIA_PERSONALITY: AtomicBool = AtomicBool::new(false);

pub(crate) fn nvidia_personality() -> bool {
    NVIDIA_PERSONALITY.load(Ordering::Relaxed)
}

fn known_device_id(vendor_device: u32) -> u32 {
    const VIRTIO_VENDOR: u32 = 0x1AF4;
    const NVIDIA_VENDOR: u32 = 0x10DE;
    const VIRTIO_GPU_ID: u32 = 0x1050;
    let vendor = vendor_device & 0xFFFF;
    match (vendor, vendor_device >> 16) {
        (VIRTIO_VENDOR, 0x106D | 0x1069) | (NVIDIA_VENDOR, 0x2B85) => {
            (VIRTIO_GPU_ID << 16) | VIRTIO_VENDOR
        }
        _ => vendor_device,
    }
}

impl ConfigurationAccess for DxgkConfigAccess {
    fn read_word(&self, _device_function: DeviceFunction, register_offset: u8) -> u32 {
        let mut val: u32 = 0;
        let mut bytes_read: ULONG = 0;
        if let Some(read) = self.read {
            // SAFETY: reads 4 bytes of our device's PCI config space at
            // `register_offset`; `val` is a valid, sufficiently-sized buffer.
            unsafe {
                read(
                    self.handle,
                    DXGK_WHICHSPACE_CONFIG,
                    (&mut val as *mut u32).cast::<c_void>(),
                    register_offset as ULONG,
                    4,
                    &mut bytes_read,
                );
            }
        }
        if register_offset == 0 {
            if val == 0x2B85_10DE {
                NVIDIA_PERSONALITY.store(true, Ordering::Relaxed);
            }
            val = known_device_id(val);
        }
        val
    }

    fn write_word(&mut self, _device_function: DeviceFunction, register_offset: u8, data: u32) {
        let mut data = data;
        let mut bytes_written: ULONG = 0;
        if let Some(write) = self.write {
            // SAFETY: writes 4 bytes to our device's PCI config space.
            unsafe {
                write(
                    self.handle,
                    DXGK_WHICHSPACE_CONFIG,
                    (&mut data as *mut u32).cast::<c_void>(),
                    register_offset as ULONG,
                    4,
                    &mut bytes_written,
                );
            }
        }
    }

    unsafe fn unsafe_clone(&self) -> Self {
        *self
    }
}

//! Native NVIDIA `NvAdminDevice` compatibility control device.
//!
//! This is a singleton WDM device, separate from the display-miniport PDO/FDO.
//! It exists only for the opt-in NVIDIA personality and implements just the one
//! ioctl observed in the native 610.88 CUDA capture.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::dxgk::*;
use wdk_sys::ntddk::{
    IoCreateDevice, IoCreateSymbolicLink, IoDeleteDevice, IoDeleteSymbolicLink, IofCompleteRequest,
    KeInitializeEvent, KeSetEvent, KeWaitForSingleObject,
};

const IOCTL_NV_ADMIN: u32 = 0x08de_0004;
const ADMIN_BYTES: usize = 0x88;
const METHOD_BUFFERED: u32 = 0;
const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const DO_BUFFERED_IO: u32 = 0x0000_0004;
const DO_DEVICE_INITIALIZING: u32 = 0x0000_0080;
const IRP_MJ_CREATE: usize = 0x00;
const IRP_MJ_CLOSE: usize = 0x02;
const IRP_MJ_DEVICE_CONTROL: usize = 0x0e;

const DEVICE_NAME: &[u16] = &[
    92, 68, 101, 118, 105, 99, 101, 92, 78, 118, 65, 100, 109, 105, 110, 68, 101, 118, 105, 99,
    101, 0,
];
const DOS_NAME: &[u16] = &[
    92, 63, 63, 92, 78, 118, 65, 100, 109, 105, 110, 68, 101, 118, 105, 99, 101, 0,
];

static DRIVER: AtomicPtr<DRIVER_OBJECT> = AtomicPtr::new(core::ptr::null_mut());
type IoDispatch = unsafe extern "C" fn(PDEVICE_OBJECT, PIRP) -> NTSTATUS;
static PREV_CREATE: AtomicPtr<IoDispatch> = AtomicPtr::new(core::ptr::null_mut());
static PREV_CLOSE: AtomicPtr<IoDispatch> = AtomicPtr::new(core::ptr::null_mut());
static PREV_CONTROL: AtomicPtr<IoDispatch> = AtomicPtr::new(core::ptr::null_mut());
static DEVICE: AtomicPtr<DEVICE_OBJECT> = AtomicPtr::new(core::ptr::null_mut());
static ADAPTERS: AtomicU32 = AtomicU32::new(0);
static STOPPING: AtomicU32 = AtomicU32::new(0);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

struct Event(UnsafeCell<KEVENT>);
// SAFETY: a KEVENT is a dispatcher object, shared by design. It is initialized
// before DEVICE is published and remains at a stable static address thereafter.
unsafe impl Sync for Event {}

/// Last-active-dispatch latch. Static storage is deliberate: even a dispatch
/// rejected after stopping began can safely drop its accounting after the WDM
/// device object has been deleted.
static DRAINED: Event = Event(UnsafeCell::new(unsafe { core::mem::zeroed() }));

fn unicode(name: &'static [u16]) -> UNICODE_STRING {
    let content = name.len().saturating_sub(1);
    UNICODE_STRING {
        Length: (content * 2) as u16,
        MaximumLength: (name.len() * 2) as u16,
        Buffer: name.as_ptr() as *mut u16,
    }
}

/// Install only our control-device slots, and only after DxgkInitialize has
/// succeeded. No paging-queue or synchronization-object callbacks belong here.
pub unsafe fn install_dispatch(driver: PDRIVER_OBJECT) {
    if driver.is_null() {
        return;
    }
    DRIVER.store(driver, Ordering::Release);
    // SAFETY: DriverEntry supplies the live DRIVER_OBJECT. The three indices are
    // WDM's fixed MajorFunction slots and the table is at least 0x1c entries.
    unsafe {
        PREV_CREATE.store((*driver).MajorFunction[IRP_MJ_CREATE], Ordering::Release);
        PREV_CLOSE.store((*driver).MajorFunction[IRP_MJ_CLOSE], Ordering::Release);
        PREV_CONTROL.store(
            (*driver).MajorFunction[IRP_MJ_DEVICE_CONTROL],
            Ordering::Release,
        );
        (*driver).MajorFunction[IRP_MJ_CREATE] = Some(dispatch_create);
        (*driver).MajorFunction[IRP_MJ_CLOSE] = Some(dispatch_close);
        (*driver).MajorFunction[IRP_MJ_DEVICE_CONTROL] = Some(dispatch_device_control);
    }
}

/// Add one adapter reference, creating the singleton on the first NVIDIA-
/// personality AddDevice. PASSIVE_LEVEL; PnP serializes AddDevice/RemoveDevice.
pub unsafe fn add_device() -> NTSTATUS {
    if !crate::virtio::nvda::enabled() {
        return STATUS_SUCCESS;
    }
    if !DEVICE.load(Ordering::Acquire).is_null() {
        ADAPTERS.fetch_add(1, Ordering::AcqRel);
        return STATUS_SUCCESS;
    }
    let driver = DRIVER.load(Ordering::Acquire);
    if driver.is_null() {
        return STATUS_DEVICE_NOT_READY;
    }

    STOPPING.store(0, Ordering::Release);
    ACTIVE.store(0, Ordering::Release);
    // NotificationEvent, initially unsignaled. Recreated adapter generations
    // reset it before the new device becomes visible.
    // SAFETY: static, final address, no dispatch can use it before publication.
    unsafe { KeInitializeEvent(DRAINED.0.get(), 0, 0) };

    let mut device_name = unicode(DEVICE_NAME);
    let mut device: PDEVICE_OBJECT = core::ptr::null_mut();
    // SAFETY: PASSIVE AddDevice; driver/name/out-pointer are valid for the call.
    // No extension is needed: all teardown state has static lifetime.
    let status = unsafe {
        IoCreateDevice(
            driver,
            0,
            &mut device_name,
            FILE_DEVICE_UNKNOWN,
            0,
            0,
            &mut device,
        )
    };
    if status < 0 || device.is_null() {
        return if status < 0 {
            status
        } else {
            STATUS_UNSUCCESSFUL
        };
    }

    let mut dos_name = unicode(DOS_NAME);
    // SAFETY: both names live for the call and the device was just created.
    let link_status = unsafe { IoCreateSymbolicLink(&mut dos_name, &mut device_name) };
    if link_status < 0 {
        // SAFETY: not published and no IRP can reference this device yet.
        unsafe { IoDeleteDevice(device) };
        return link_status;
    }

    // SAFETY: this thread owns the not-yet-published device. Buffered I/O gives
    // DeviceIoControl one AssociatedIrp.SystemBuffer for input and output.
    unsafe {
        (*device).Flags |= DO_BUFFERED_IO;
    }
    DEVICE.store(device, Ordering::Release);
    ADAPTERS.store(1, Ordering::Release);
    // SAFETY: publication precedes clearing DO_DEVICE_INITIALIZING, so every IRP
    // sees initialized lifecycle state and a live singleton pointer.
    unsafe {
        (*device).Flags &= !DO_DEVICE_INITIALIZING;
    }
    STATUS_SUCCESS
}

/// Drop one adapter reference and tear down only after the last removal. Never
/// called from StopDevice: stop/start retains both the name and open handles.
pub unsafe fn remove_device() {
    let references = ADAPTERS.load(Ordering::Acquire);
    if references == 0 {
        return;
    }
    if references > 1 {
        ADAPTERS.store(references - 1, Ordering::Release);
        return;
    }

    let device = DEVICE.swap(core::ptr::null_mut(), Ordering::AcqRel);
    ADAPTERS.store(0, Ordering::Release);
    if device.is_null() {
        return;
    }
    STOPPING.store(1, Ordering::Release);

    let mut dos_name = unicode(DOS_NAME);
    // SAFETY: this is the link created with the singleton. Best effort during
    // removal; stopping already prevents any new admin operation from running.
    let _ = unsafe { IoDeleteSymbolicLink(&mut dos_name) };

    if ACTIVE.load(Ordering::Acquire) != 0 {
        // SAFETY: RemoveDevice is PASSIVE. DRAINED is a permanent static event;
        // the final accepted dispatch signals it after leaving the device body.
        let _ = unsafe {
            KeWaitForSingleObject(DRAINED.0.get() as PVOID, 0, 0, 0, core::ptr::null_mut())
        };
    }
    // SAFETY: stopping is latched and every accepted dispatch has drained, so no
    // running handler can still depend on this device object being allocated.
    unsafe { IoDeleteDevice(device) };
}

fn leave() {
    if ACTIVE.fetch_sub(1, Ordering::AcqRel) == 1 && STOPPING.load(Ordering::Acquire) != 0 {
        // SAFETY: DRAINED has static lifetime and was initialized before any
        // dispatch. KeSetEvent is valid at the IRQLs at which IRPs can arrive.
        unsafe { KeSetEvent(DRAINED.0.get(), 0, 0) };
    }
}

fn enter() -> bool {
    if STOPPING.load(Ordering::Acquire) != 0 {
        return false;
    }
    ACTIVE.fetch_add(1, Ordering::AcqRel);
    if STOPPING.load(Ordering::Acquire) != 0 {
        leave();
        return false;
    }
    true
}

unsafe extern "C" fn dispatch_create(device: PDEVICE_OBJECT, irp: PIRP) -> NTSTATUS {
    if device != DEVICE.load(Ordering::Acquire) {
        let previous = PREV_CREATE.load(Ordering::Acquire);
        return if let Some(previous) = previous {
            unsafe { previous(device, irp) }
        } else {
            STATUS_INVALID_DEVICE_REQUEST
        };
    }
    if irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    if !enter() {
        return complete(irp, STATUS_DEVICE_NOT_READY, 0);
    }
    let status = if device.is_null() {
        STATUS_INVALID_DEVICE_REQUEST
    } else {
        STATUS_SUCCESS
    };
    leave();
    complete(irp, status, 0)
}

unsafe extern "C" fn dispatch_close(device: PDEVICE_OBJECT, irp: PIRP) -> NTSTATUS {
    if device != DEVICE.load(Ordering::Acquire) {
        let previous = PREV_CLOSE.load(Ordering::Acquire);
        return if let Some(previous) = previous {
            unsafe { previous(device, irp) }
        } else {
            STATUS_INVALID_DEVICE_REQUEST
        };
    }
    if irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    if !enter() {
        return complete(irp, STATUS_DEVICE_NOT_READY, 0);
    }
    let status = if device.is_null() {
        STATUS_INVALID_DEVICE_REQUEST
    } else {
        STATUS_SUCCESS
    };
    leave();
    complete(irp, status, 0)
}

unsafe extern "C" fn dispatch_device_control(device: PDEVICE_OBJECT, irp: PIRP) -> NTSTATUS {
    if device != DEVICE.load(Ordering::Acquire) {
        let previous = PREV_CONTROL.load(Ordering::Acquire);
        return if let Some(previous) = previous {
            unsafe { previous(device, irp) }
        } else {
            STATUS_INVALID_DEVICE_REQUEST
        };
    }
    if irp.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    if !enter() {
        return complete(irp, STATUS_DEVICE_NOT_READY, 0);
    }
    let mut information = 0;
    let status = if device.is_null() {
        STATUS_INVALID_DEVICE_REQUEST
    } else {
        // SAFETY: the I/O manager supplies a live IRP for this call. Reading the
        // anonymous union fields is the bindgen equivalent of the WDM
        // IoGetCurrentIrpStackLocation inline accessor.
        let stack = unsafe {
            (*irp)
                .Tail
                .Overlay
                .__bindgen_anon_2
                .__bindgen_anon_1
                .CurrentStackLocation
        };
        if stack.is_null() {
            STATUS_INVALID_DEVICE_REQUEST
        } else {
            // SAFETY: CurrentStackLocation points at this IRP's current stack.
            let args = unsafe { &(*stack).Parameters.DeviceIoControl };
            let buffer = unsafe { (*irp).AssociatedIrp.SystemBuffer as *mut u8 };
            if args.IoControlCode != IOCTL_NV_ADMIN
                || args.IoControlCode & 3 != METHOD_BUFFERED
                || args.InputBufferLength as usize != ADMIN_BYTES
                || (args.OutputBufferLength as usize) < ADMIN_BYTES
                || buffer.is_null()
            {
                STATUS_INVALID_DEVICE_REQUEST
            } else {
                // SAFETY: METHOD_BUFFERED and the checked output length make at
                // least 0x88 bytes writable at SystemBuffer. The capture reply is
                // exactly 136 bytes: dword 2 followed by zeroes.
                unsafe {
                    core::ptr::write_bytes(buffer, 0, ADMIN_BYTES);
                    buffer.cast::<u32>().write_unaligned(2);
                }
                information = ADMIN_BYTES;
                STATUS_SUCCESS
            }
        }
    };
    leave();
    complete(irp, status, information)
}

unsafe fn complete(irp: PIRP, status: NTSTATUS, information: usize) -> NTSTATUS {
    // SAFETY: the caller established a live IRP. Set both IoStatus fields before
    // completing it, and return the same status to the I/O manager.
    unsafe {
        (*irp).IoStatus.__bindgen_anon_1.Status = status;
        (*irp).IoStatus.Information = information;
        IofCompleteRequest(irp, 0);
    }
    status
}

//! Message-signalled interrupts for the virtio-gpu device: deciding whether the
//! OS handed the driver messages instead of the INTx line, and programming the
//! device's MSI-X vectors to match. All decisions live in
//! `helios_kmd_logic::msi` (host-tested); this file is the WDK/MMIO glue.
//!
//! Design and the hardware verification list: `docs/msi-interrupts.md`.
//!
//! # Who decides MSI versus INTx
//!
//! Not this driver. A WDDM miniport does not connect its own interrupt: dxgkrnl
//! connects whatever PnP assigned and calls `DxgkDdiInterruptRoutine` with a
//! message number (0 for a line). PnP assigns messages when the device key's
//! `MSISupported` is 1 and the device offers them. By the time `StartDevice`
//! runs the choice is made, and two things say which way it went: the MSI-X
//! Enable bit in the device's PCI capability, and message descriptors in the
//! translated resource list. Either one means messages (`planned_messages`);
//! the driver follows it:
//!
//! * Neither: INTx. Nothing in this file touches the device and the driver
//!   behaves exactly as before (the only addition is two read-only probes).
//! * Messages: the device's vectors MUST be programmed (a device with
//!   `NO_VECTOR` everywhere raises nothing once MSI-X is enabled) and the ISR
//!   must route by message number instead of reading the ISR status register.
//!
//! # IRQL
//!
//! Everything here is PASSIVE_LEVEL, run from `StartDevice` / `VirtioGpu::init`.
//! The ISR half is in `ddi/interrupt.rs` and touches only an atomic.

use helios_kmd_logic::msi::{self, Plan};

use super::config::DxgkConfigAccess;
use super::pci_caps::{map_common_cfg, read_msix_message_control};
use crate::dxgk::*;

/// Offsets into `virtio_pci_common_cfg` (virtio 1.2, 4.1.4.3).
const COMMON_MSIX_CONFIG: usize = 0x10;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_MSIX_VECTOR: usize = 0x1A;

/// How many messages the OS connected for this device, as a lower bound; 0 when
/// the device is on the INTx line (the Enable bit is clear) or has no MSI at all.
///
/// Its own `#[inline(never)]` frame, called from `StartDevice` BEFORE
/// `VirtioGpu::init` rather than from inside it: `DXGK_DEVICE_INFO` is ~150
/// bytes and `init` sits on the 24 KB boot stack (tools/kmd-frame-sizes.ps1).
/// Sequential with `init`, so the frames never overlap.
#[inline(never)]
pub(crate) fn probe_granted(dxgkrnl: &DXGKRNL_INTERFACE) -> u32 {
    let access = DxgkConfigAccess::new(dxgkrnl);
    let msix_ctrl = read_msix_message_control(&access);
    crate::diag::record_named_bytes(b"MsiCap", u32::from(msix_ctrl.unwrap_or(0)));
    // A device with no MSI-X capability cannot do virtio messages: INTx, and the
    // resource-list call is skipped. Otherwise either signal alone is enough (see
    // `planned_messages`), so the list is read even when Enable is still clear.
    if msix_ctrl.is_none() {
        crate::diag::record_named_bytes(b"MsiGrant", 0);
        return 0;
    }
    let listed = listed_messages(dxgkrnl);
    crate::diag::record_named_bytes(b"MsiList", listed);
    let granted = msi::planned_messages(msix_ctrl, listed);
    crate::diag::record_named_bytes(b"MsiGrant", granted);
    granted
}

/// Message-interrupt descriptors in the translated resource list (a lower
/// bound; 0 if the list is unavailable).
#[inline(never)]
fn listed_messages(dxgkrnl: &DXGKRNL_INTERFACE) -> u32 {
    let Some(get_info) = dxgkrnl.DxgkCbGetDeviceInformation else {
        return 0;
    };
    // SAFETY: an all-zero DXGK_DEVICE_INFO (pointers, integers, an enum whose 0
    // is `DockStateUnsupported`) is valid; dxgkrnl fills it.
    let mut info: DXGK_DEVICE_INFO = unsafe { core::mem::zeroed() };
    // SAFETY: documented PASSIVE_LEVEL callback, called from StartDevice with the
    // live device handle and a valid out-structure.
    let status = unsafe { get_info(dxgkrnl.DeviceHandle, &mut info) };
    if status != STATUS_SUCCESS {
        return 0;
    }
    let list = info.TranslatedResourceList as *const u8;
    if list.is_null() {
        return 0;
    }
    msi::granted_messages(|off| {
        // SAFETY: `off` is a 4-aligned offset derived by the parser from the
        // list's own counts (bounded: at most 64 descriptors); the list is the
        // OS-owned allocation, valid for the duration of StartDevice.
        Some(unsafe { core::ptr::read_unaligned(list.add(off).cast::<u32>()) })
    })
}

#[inline]
unsafe fn cfg_write16(va: usize, off: usize, value: u16) {
    // SAFETY: caller guarantees `va` maps a common-cfg region >= 0x1C bytes.
    unsafe { core::ptr::write_volatile((va + off) as *mut u16, value) }
}

#[inline]
unsafe fn cfg_read16(va: usize, off: usize) -> u16 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile((va + off) as *const u16) }
}

/// Write `plan` to the device and verify every vector by reading it back.
/// `queues[i]` is the virtio queue number planned as `plan.queue[i]`.
///
/// # Safety
/// `va` maps the device's common configuration (>= 0x1C bytes). Called at
/// PASSIVE_LEVEL, single-threaded, before DRIVER_OK: nothing else is selecting
/// queues.
unsafe fn write_plan(va: usize, plan: &Plan, queues: &[u16]) -> bool {
    // SAFETY: per the function contract, for every access below.
    unsafe {
        cfg_write16(va, COMMON_MSIX_CONFIG, plan.config);
        let mut ok = msi::vector_accepted(plan.config, cfg_read16(va, COMMON_MSIX_CONFIG));
        for (i, &queue) in queues.iter().enumerate() {
            let want = plan.queue[i];
            cfg_write16(va, COMMON_QUEUE_SELECT, queue);
            cfg_write16(va, COMMON_QUEUE_MSIX_VECTOR, want);
            ok &= msi::vector_accepted(want, cfg_read16(va, COMMON_QUEUE_MSIX_VECTOR));
        }
        ok
    }
}

/// Program the device's vectors for `granted` messages and return the ISR's
/// state word ([`msi::isr_state`]; never 0 on `Ok`).
///
/// Order: the plan first; if the device refuses any vector, ONE retry with every
/// queue on message 0; if that is refused too, `Err` and the caller fails the
/// transport (a device on messages that cannot be given one would never
/// interrupt: failing `StartDevice`'s transport is the safe outcome, a hang is
/// not). `queues` are the queue numbers that exist, control first.
///
/// Must run before DRIVER_OK: QEMU wires each queue's call eventfd to KVM as an
/// irqfd when the guest notifiers are set up at DRIVER_OK, from the vectors
/// programmed by then.
#[inline(never)]
pub(crate) fn program_vectors(
    access: &DxgkConfigAccess,
    granted: u32,
    queues: &[u16],
) -> Result<u32, ()> {
    if granted == 0 || queues.is_empty() || queues.len() > msi::MAX_QUEUES {
        return Err(());
    }
    let va = map_common_cfg(access);
    if va == 0 {
        crate::diag::record_named_bytes(b"MsiNoCfg", 1);
        return Err(());
    }
    let shared_only = crate::diag::read_config_dword(crate::diag::knobs::MSI_VECTORS, 0) != 0;
    let plan = msi::plan(granted, queues.len(), shared_only);
    // SAFETY: `va` is the mapped common cfg (>= 0x1C bytes, checked by
    // `map_common_cfg`); PASSIVE, pre-DRIVER_OK.
    if unsafe { write_plan(va, &plan, queues) } {
        crate::diag::record_named_bytes(b"MsiVec", plan.max_vector().map_or(0xFFFF, u32::from));
        return Ok(msi::isr_state(&plan));
    }
    crate::diag::record_named_bytes(b"MsiRefused", 1);
    let shared = Plan::all_shared(queues.len());
    // SAFETY: as above.
    if unsafe { write_plan(va, &shared, queues) } {
        crate::diag::record_named_bytes(b"MsiVec", 0);
        return Ok(msi::isr_state(&shared));
    }
    crate::diag::record_named_bytes(b"MsiRefused", 2);
    Err(())
}

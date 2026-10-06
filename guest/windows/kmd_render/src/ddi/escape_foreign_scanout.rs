//! `HELIOS_NVRM_OP_SCANOUT_SET` / `SCANOUT_PRESENT` / `SCANOUT_RELEASE`
//! (`helios_protocol::nvrm_scanout`): own scanout 0 and show host GEM objects on
//! it through the KMD. State machine and rules: `helios_kmd_logic::foreign_scanout`;
//! the desktop suppression: `adapter/foreign_scanout.rs`; the flip:
//! `virtio/foreign_scanout.rs`.
//!
//! Kept out of `escape.rs` so that file only gains the dispatch arm. The result
//! layers are those of the rest of the verb: the escape's NTSTATUS for a malformed
//! request, `HeliosNvrmHeader.status` for the KMD's verdict on a well-formed one.

use core::mem::size_of;
use core::sync::atomic::Ordering;

use bytemuck::{bytes_of, pod_read_unaligned, Pod};
use helios_kmd_logic::foreign_scanout::{Layout, ReleaseOutcome, SetError};
use helios_protocol::{
    HeliosEscapeHeader, HeliosNvrmHeader, HeliosNvrmScanoutPresent, HeliosNvrmScanoutRelease,
    HeliosNvrmScanoutSet, HELIOS_NVRM_OP_SCANOUT_PRESENT, HELIOS_NVRM_OP_SCANOUT_RELEASE,
    HELIOS_NVRM_OP_SCANOUT_SET, HELIOS_NVRM_ST_BAD_RANGE, HELIOS_NVRM_ST_DEVICE_ERROR,
    HELIOS_NVRM_ST_FORBIDDEN, HELIOS_NVRM_ST_NOT_OWNED, HELIOS_NVRM_ST_NO_SOURCE,
    HELIOS_NVRM_ST_OK, HELIOS_NVRM_ST_SCANOUT_BUSY,
};

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::foreign_scanout::{self as flip, PresentRefusal};
use crate::virtio::gpu::DeviceOwner;

/// `device_type` from which an `Open` names a DRM node.
const DEVICE_TYPE_DRI_FIRST: u32 = 512;

/// Bind a caller buffer to wire struct `T`: both the declared and the supplied
/// length must cover it. Returns the request, read unaligned.
fn bind<T: Pod>(buf: &[u8], hdr: &HeliosEscapeHeader) -> Result<T, NTSTATUS> {
    let n = size_of::<T>();
    match buf.get(..n) {
        Some(head) if hdr.size as usize >= n => Ok(pod_read_unaligned(head)),
        _ => {
            super::escape::ESCAPE_SHORT_BUFFER.fetch_add(1, Ordering::Relaxed);
            Err(STATUS_BUFFER_TOO_SMALL)
        }
    }
}

/// Write `value` over the start of `buf` (already length-checked by [`bind`]).
fn write_back<T: Pod>(buf: &mut [u8], value: &T) -> NTSTATUS {
    match buf.get_mut(..size_of::<T>()) {
        Some(dst) => {
            dst.copy_from_slice(bytes_of(value));
            STATUS_SUCCESS
        }
        None => STATUS_BUFFER_TOO_SMALL,
    }
}

fn finish(head: &mut HeliosNvrmHeader, status: i32, epoch: u64) {
    head.status = status;
    head.epoch = epoch;
}

/// The dispatch target for the three ops. `epoch` is the NVRM epoch (0 with no
/// transport), already read by the caller.
pub(super) fn escape_scanout_op(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    op: u32,
    epoch: u64,
) -> NTSTATUS {
    match op {
        HELIOS_NVRM_OP_SCANOUT_SET => scanout_set(adapter, buf, hdr, owner, epoch),
        HELIOS_NVRM_OP_SCANOUT_PRESENT => scanout_present(passive, adapter, buf, hdr, owner, epoch),
        HELIOS_NVRM_OP_SCANOUT_RELEASE => scanout_release(adapter, buf, hdr, owner, epoch),
        _ => STATUS_INVALID_PARAMETER,
    }
}

fn scanout_set(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    epoch: u64,
) -> NTSTATUS {
    let mut req: HeliosNvrmScanoutSet = match bind(buf, hdr) {
        Ok(r) => r,
        Err(st) => return st,
    };
    req.out_generation = 0;
    let status = 'verdict: {
        if req.flags != 0 || req.reserved != 0 {
            break 'verdict HELIOS_NVRM_ST_BAD_RANGE;
        }
        // The caller's own DRM-node handle, as for a forwarded ScanoutFlip.
        let device_type =
            match adapter.with_virtio(|v| v.nvrm_handle_device_type(owner, req.handle)) {
                Ok(t) => t,
                // No transport: nothing could be shown anyway.
                Err(_) => return STATUS_DEVICE_NOT_READY,
            };
        match device_type {
            None => break 'verdict HELIOS_NVRM_ST_NOT_OWNED,
            Some(t) if t < DEVICE_TYPE_DRI_FIRST => break 'verdict HELIOS_NVRM_ST_FORBIDDEN,
            Some(_) => {}
        }
        let layout = Layout {
            width: req.width,
            height: req.height,
            stride: req.stride,
            offset: req.offset,
            fourcc: req.fourcc,
            modifier: req.modifier,
        };
        match adapter.foreign_scanout_set(owner, req.handle, epoch, layout, req.lapse_ms) {
            Ok(o) => {
                req.out_generation = o.generation;
                req.lapse_ms = o.lapse_ms;
                HELIOS_NVRM_ST_OK
            }
            Err(SetError::Busy) => HELIOS_NVRM_ST_SCANOUT_BUSY,
            Err(SetError::Layout(_)) => HELIOS_NVRM_ST_BAD_RANGE,
        }
    };
    finish(&mut req.head, status, epoch);
    let st = write_back(buf, &req);
    // Rare, and the registry write is slow: not on the PRESENT path.
    crate::adapter::foreign_scanout::publish_counters();
    st
}

fn scanout_present(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    epoch: u64,
) -> NTSTATUS {
    let mut req: HeliosNvrmScanoutPresent = match bind(buf, hdr) {
        Ok(r) => r,
        Err(st) => return st,
    };
    req.out_seq = 0;
    let status = if req.flags != 0 || req.rm_fence_handle != 0 || req.gem == 0 {
        HELIOS_NVRM_ST_BAD_RANGE
    } else {
        match flip::present(passive, adapter, owner, req.handle, req.gem) {
            Ok(seq) => {
                req.out_seq = seq;
                HELIOS_NVRM_ST_OK
            }
            Err(PresentRefusal::NoTransport) => return STATUS_DEVICE_NOT_READY,
            Err(PresentRefusal::NotOwned) => HELIOS_NVRM_ST_NOT_OWNED,
            Err(PresentRefusal::Forbidden) => HELIOS_NVRM_ST_FORBIDDEN,
            Err(PresentRefusal::NoSource) => HELIOS_NVRM_ST_NO_SOURCE,
            Err(PresentRefusal::Device(_)) => HELIOS_NVRM_ST_DEVICE_ERROR,
        }
    };
    finish(&mut req.head, status, epoch);
    write_back(buf, &req)
}

fn scanout_release(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    epoch: u64,
) -> NTSTATUS {
    let mut req: HeliosNvrmScanoutRelease = match bind(buf, hdr) {
        Ok(r) => r,
        Err(st) => return st,
    };
    let status = if req.flags != 0 {
        HELIOS_NVRM_ST_BAD_RANGE
    } else {
        let handle = (req.handle != 0).then_some(req.handle);
        match adapter.foreign_scanout_release(owner, handle) {
            ReleaseOutcome::Released { .. } | ReleaseOutcome::NotActive => HELIOS_NVRM_ST_OK,
            ReleaseOutcome::NotOwner => HELIOS_NVRM_ST_NOT_OWNED,
        }
    };
    finish(&mut req.head, status, epoch);
    let st = write_back(buf, &req);
    crate::adapter::foreign_scanout::publish_counters();
    st
}

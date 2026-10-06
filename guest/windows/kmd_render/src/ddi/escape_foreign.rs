//! `HELIOS_ESCAPE_FOREIGN_RESOURCE` (`helios_protocol::foreign`): admit a
//! host-exported RM memory object to the resource tables so an NVK-on-RM swapchain
//! image can be presented without a CPU copy. The rules are in
//! `guest/windows/docs/zero-copy-present.md`; the work is in `virtio/foreign.rs`.
//!
//! Kept out of `escape.rs` so that file only gains the dispatch arm. The buffer
//! handling mirrors its `EscapeBuf`: the struct is the only layout authority, and
//! both what the caller DECLARED (`hdr.size`) and what the runtime supplied
//! (`buf.len()`) must cover it.

use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::{bytes_of, pod_read_unaligned, Pod};
use helios_kmd_logic::foreign_resource::{
    self as fr, MAX_FOREIGN_BYTES_PER_OWNER, MAX_FOREIGN_PER_OWNER, MAX_FOREIGN_RESOURCE_BYTES,
    MAX_FOREIGN_TOTAL,
};
use helios_protocol::{
    HeliosEscapeHeader, HeliosForeignHeader, HeliosForeignImportRm, HeliosForeignImportRmLayout,
    HeliosForeignLayout, HeliosForeignQueryCaps, HELIOS_FOREIGN_ABI_VERSION,
    HELIOS_FOREIGN_CAP_RM_IMPORT, HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT, HELIOS_FOREIGN_OP_IMPORT_RM,
    HELIOS_FOREIGN_OP_QUERY_CAPS, HELIOS_FOREIGN_ST_BAD_CONTEXT, HELIOS_FOREIGN_ST_BAD_RANGE,
    HELIOS_FOREIGN_ST_DEVICE_ERROR, HELIOS_FOREIGN_ST_NOT_OWNED, HELIOS_FOREIGN_ST_NO_RESOURCES,
    HELIOS_FOREIGN_ST_OK, HELIOS_FOREIGN_ST_UNSUPPORTED,
};

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::foreign::{self, ImportError};
use crate::virtio::gpu::DeviceOwner;

// kmd_logic has no dependency edge to helios_protocol; this pins its copy of the
// layout flag to the wire's.
const _: () = assert!(fr::FLAG_LAYOUT == HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT);

/// Escapes of this verb, for the registry-write throttle.
static CALLS: AtomicU32 = AtomicU32::new(0);

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

/// The dispatch target. `owner` is the escaping device (proven non-null by the
/// dispatcher); every resource this creates is tagged with it.
pub(super) fn escape_foreign_resource(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
) -> NTSTATUS {
    let head: HeliosForeignHeader = match bind(buf, hdr) {
        Ok(h) => h,
        Err(st) => return st,
    };
    if head.abi_version != HELIOS_FOREIGN_ABI_VERSION || head.reserved != 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // 0 when the transport is down: QUERY_CAPS still answers.
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    let calls = CALLS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);

    let st = match head.op {
        HELIOS_FOREIGN_OP_QUERY_CAPS => query_caps(adapter, buf, hdr, owner, epoch),
        HELIOS_FOREIGN_OP_IMPORT_RM => import_rm(passive, adapter, buf, hdr, owner, epoch),
        _ => STATUS_INVALID_PARAMETER,
    };
    // Registry writes are slow and an escape is not the place to storm them:
    // first call, then every 64th.
    if calls == 1 || calls % 64 == 0 {
        foreign::publish_counters(adapter, owner);
    }
    st
}

fn query_caps(
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    epoch: u64,
) -> NTSTATUS {
    let mut caps: HeliosForeignQueryCaps = match bind(buf, hdr) {
        Ok(c) => c,
        Err(st) => return st,
    };
    caps.supported_ops =
        (1u64 << HELIOS_FOREIGN_OP_QUERY_CAPS) | (1u64 << HELIOS_FOREIGN_OP_IMPORT_RM);
    caps.caps_flags = if foreign::rm_import_served(adapter) {
        HELIOS_FOREIGN_CAP_RM_IMPORT
    } else {
        0
    };
    // The limits are constants; the occupancy is a table read. With no
    // transport the occupancy reads zero, as the NVRM caps do.
    caps.max_per_owner = MAX_FOREIGN_PER_OWNER as u32;
    caps.max_total = MAX_FOREIGN_TOTAL as u32;
    caps.reserved0 = 0;
    caps.max_bytes_per_resource = MAX_FOREIGN_RESOURCE_BYTES;
    caps.max_bytes_per_owner = MAX_FOREIGN_BYTES_PER_OWNER;
    let (live_total, live_owner, imported, refused) = adapter
        .with_virtio(|v| {
            let s = v.foreign_snapshot(owner);
            (
                s.live_total,
                s.live_owner,
                s.counters.imported,
                s.counters.refused(),
            )
        })
        .unwrap_or((0, 0, 0, 0));
    caps.live_total = live_total;
    caps.live_owner = live_owner;
    caps.imported = imported;
    caps.refused = refused;
    caps.head.status = HELIOS_FOREIGN_ST_OK;
    caps.head.epoch = epoch;
    write_back(buf, &caps)
}

fn import_rm(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    epoch: u64,
) -> NTSTATUS {
    let mut req: HeliosForeignImportRm = match bind(buf, hdr) {
        Ok(r) => r,
        Err(st) => return st,
    };
    req.head.epoch = epoch;
    req.out_resource_id = 0;
    req.out_host_errno = 0;
    // The layout tail is part of the request when the flag says so: both the
    // declared and the supplied length must cover all 104 bytes, or the escape
    // fails as any short buffer does. A request without the flag has no layout
    // and is refused `BAD_RANGE` by `validate_request` (the layout is mandatory;
    // the refusal is counted there). Only the 72-byte base is ever written back.
    let layout = if req.flags & HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT != 0 {
        let ext: HeliosForeignImportRmLayout = match bind(buf, hdr) {
            Ok(e) => e,
            Err(st) => return st,
        };
        layout_from_wire(&ext.layout)
    } else {
        None
    };
    let result = foreign::import_rm(
        passive,
        adapter,
        owner,
        req.ctx_id,
        req.rm_handle,
        req.gem_handle,
        req.flags,
        req.size,
        layout,
    );
    req.head.status = match result {
        Ok(resource_id) => {
            req.out_resource_id = resource_id;
            HELIOS_FOREIGN_ST_OK
        }
        Err(ImportError::Unsupported) => HELIOS_FOREIGN_ST_UNSUPPORTED,
        Err(ImportError::BadRequest) => HELIOS_FOREIGN_ST_BAD_RANGE,
        Err(ImportError::NotOwned) => HELIOS_FOREIGN_ST_NOT_OWNED,
        Err(ImportError::BadContext) => HELIOS_FOREIGN_ST_BAD_CONTEXT,
        Err(ImportError::NoResources) => HELIOS_FOREIGN_ST_NO_RESOURCES,
        // No transport is the transport verdict, not the KMD's: fail the escape
        // as the other transport-bound verbs do.
        Err(ImportError::NoTransport) => return STATUS_DEVICE_NOT_READY,
        Err(ImportError::Device(_, errno)) => {
            req.out_host_errno = errno;
            HELIOS_FOREIGN_ST_DEVICE_ERROR
        }
        // The host's own verdicts, with its errno kept for the caller.
        Err(ImportError::HostNotOwned(errno)) => {
            req.out_host_errno = errno;
            HELIOS_FOREIGN_ST_NOT_OWNED
        }
        Err(ImportError::HostBadRange(errno)) => {
            req.out_host_errno = errno;
            HELIOS_FOREIGN_ST_BAD_RANGE
        }
        Err(ImportError::HostUnsupported(errno)) => {
            req.out_host_errno = errno;
            HELIOS_FOREIGN_ST_UNSUPPORTED
        }
    };
    write_back(buf, &req)
}

/// The wire layout as the pure type. `None` when `reserved` is not zero (a field
/// this KMD does not know): the request then reads as having no layout and is
/// refused `BAD_RANGE`, like any unknown bit.
fn layout_from_wire(w: &HeliosForeignLayout) -> Option<fr::Layout> {
    if w.reserved != 0 {
        return None;
    }
    Some(fr::Layout {
        width: w.width,
        height: w.height,
        stride: w.stride,
        offset: w.offset,
        fourcc: w.fourcc,
        modifier: w.modifier,
    })
}

//! `HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT` (`helios_protocol::foreign`): a GEM
//! handle, in the caller's own host DRM file, of an RM-export resource the caller
//! created or opened. The rules and the host message are in
//! `helios_kmd_logic::rm_resource_import`; the work is in
//! `virtio/rm_resource_import.rs`; the design in
//! `guest/windows/docs/shared-foreign-surfaces.md` section 6.
//!
//! Kept out of `escape_foreign.rs` so that file only gains the dispatch arm. The
//! result layers are those of the rest of the verb: the escape's NTSTATUS for a
//! malformed request, `HeliosForeignHeader.status` for the KMD's verdict on a
//! well-formed one. The host's message is NEVER reachable through `FORWARD`
//! (`HELIOS_NVRM_FORWARD_MSG_TYPES` has no bit 31, asserted in the protocol crate).

use core::mem::size_of;
use core::sync::atomic::Ordering;

use bytemuck::{bytes_of, pod_read_unaligned, Pod};
use helios_kmd_logic::foreign_errno::Verdict;
use helios_protocol::{
    HeliosEscapeHeader, HeliosForeignRmResourceImport, HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER,
    HELIOS_FOREIGN_ST_BAD_RANGE, HELIOS_FOREIGN_ST_DEVICE_ERROR, HELIOS_FOREIGN_ST_NOT_OWNED,
    HELIOS_FOREIGN_ST_NO_RESOURCES, HELIOS_FOREIGN_ST_OK, HELIOS_FOREIGN_ST_UNSUPPORTED,
};

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::gpu::DeviceOwner;
use crate::virtio::rm_resource_import::{self as ri, RiError};

// The pure crate and the wire struct agree on the one flag bit.
const _: () = assert!(
    helios_kmd_logic::rm_resource_import::REPLY_FLAG_MODIFIER
        == HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER
);

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

/// The verdict as the wire status.
fn status_for(v: Verdict) -> i32 {
    match v {
        Verdict::NotOwned => HELIOS_FOREIGN_ST_NOT_OWNED,
        Verdict::BadRange => HELIOS_FOREIGN_ST_BAD_RANGE,
        Verdict::Unsupported => HELIOS_FOREIGN_ST_UNSUPPORTED,
        Verdict::NoResources => HELIOS_FOREIGN_ST_NO_RESOURCES,
        Verdict::Device => HELIOS_FOREIGN_ST_DEVICE_ERROR,
    }
}

/// The dispatch target (`escape_foreign.rs` has already checked the header).
/// `process` is the escaping device's `hKmdProcess`.
pub(super) fn rm_resource_import(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    buf: &mut [u8],
    hdr: &HeliosEscapeHeader,
    owner: DeviceOwner,
    process: usize,
    epoch: u64,
) -> NTSTATUS {
    let mut req: HeliosForeignRmResourceImport = match bind(buf, hdr) {
        Ok(r) => r,
        Err(st) => return st,
    };
    req.head.epoch = epoch;
    req.out_gem_handle = 0;
    req.out_size = 0;
    req.out_modifier = 0;
    req.out_flags = 0;
    req.out_host_errno = 0;

    req.head.status = if req.flags != 0 {
        // A bit this KMD does not know: refuse rather than guess (the host
        // refuses nonzero request flags as well).
        ri::REFUSED.fetch_add(1, Ordering::Relaxed);
        HELIOS_FOREIGN_ST_BAD_RANGE
    } else {
        match ri::rm_resource_import(
            passive,
            adapter,
            owner,
            process,
            req.rm_handle,
            req.resource_id,
        ) {
            Ok(done) => {
                req.out_gem_handle = done.gem_handle;
                req.out_size = done.size;
                req.out_modifier = done.modifier;
                req.out_flags = done.flags;
                HELIOS_FOREIGN_ST_OK
            }
            // No transport is the transport verdict, not the KMD's: fail the
            // escape as the other transport-bound verbs do.
            Err(RiError::NoTransport) => return STATUS_DEVICE_NOT_READY,
            Err(RiError::Local(v)) => status_for(v),
            Err(RiError::Host(v, errno)) => {
                req.out_host_errno = errno;
                status_for(v)
            }
        }
    };
    match buf.get_mut(..size_of::<HeliosForeignRmResourceImport>()) {
        Some(dst) => {
            dst.copy_from_slice(bytes_of(&req));
            STATUS_SUCCESS
        }
        None => STATUS_BUFFER_TOO_SMALL,
    }
}

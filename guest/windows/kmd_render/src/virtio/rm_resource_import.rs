//! `RM_RESOURCE_IMPORT` (`HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT`): ask the host for
//! a GEM handle, in the caller's own DRM file, of an RM-export resource the
//! caller created or opened. The host message is `RmResourceImport` (`MsgType`
//! 31); the pure rules (request, reply, gate, errno) are in
//! `helios_kmd_logic::rm_resource_import`, the verb in
//! `ddi/escape_foreign_rm_resource.rs`, the design in
//! `guest/windows/docs/shared-foreign-surfaces.md` section 6.
//!
//! The sequence, and why each step is where it is:
//!
//! 1. **Gate.** Unless the host advertises `NVGPU_CFG_RM_RESOURCE_IMPORT` (config
//!    features bit 14, which it sets together with bits 13 and 10), nothing is
//!    touched: no table read, no wire traffic. An older backend that lacks the
//!    bit is never sent a message type it would answer `-EPROTO`.
//! 2. **Authorize, one lock hold.** The caller's DRM node and the foreign record
//!    (existence, destroyed, creator or open row of the caller's process) are
//!    read together (`VirtioGpu::rm_resource_import_begin`).
//! 3. **Round trip, no lock held.** `ctrl::raw_roundtrip` at PASSIVE; the request
//!    is exactly `MSG_HDR + 16` bytes and the reply is read only as far as the
//!    transport says it was written.
//! 4. **Re-check, one lock hold.** The caller still owns the DRM node in the same
//!    transport generation; a handle closed during the wait may name another
//!    process's file by now, and a GEM handle made in it must not be reported.
//!
//! What this does NOT do: record the GEM handle. The host closes a DRM file's GEM
//! handles when the file closes, and the KMD already closes every DRM file a
//! device left open (`DestroyDevice`, `StopDevice` sweeps), so there is nothing
//! to release that the existing teardown does not. The same resource on the same
//! file answers the same handle, so the count of live GEM handles is bounded by
//! (resources x the caller's own DRM files), both bounded elsewhere.

use core::sync::atomic::{AtomicU32, Ordering};

use super::ctrl;
use super::foreign::rm_import_served;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::virtio::gpu::DeviceOwner;
use helios_kmd_logic::foreign_errno::Verdict;
use helios_kmd_logic::rm_resource_import::{
    self as ri, ReplyError, CFG_RM_RESOURCE_IMPORT, REPLY_BYTES,
};

/// How long the host gets. `PRIME_FD_TO_HANDLE` of a dma-buf the backend already
/// holds is a syscall, but the backend may be draining other work first.
const TIMEOUT_MS: u64 = 5_000;

/// Requests answered `Unsupported` because the gate is closed (`FgRiUns`).
pub static UNSUPPORTED: AtomicU32 = AtomicU32::new(0);
/// Requests the KMD turned away before the wire (`FgRiRef`).
pub static REFUSED: AtomicU32 = AtomicU32::new(0);
/// GEM handles returned (`FgRiOk`).
pub static OK: AtomicU32 = AtomicU32::new(0);
/// Round trips that failed or that the host refused (`FgRiErr`).
pub static ERRORS: AtomicU32 = AtomicU32::new(0);
/// Successful host replies withheld because the caller's DRM node was closed or
/// the transport restarted during the wait (`FgRiStale`).
pub static STALE: AtomicU32 = AtomicU32::new(0);

/// What a successful import tells the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    pub gem_handle: u32,
    /// `REPLY_FLAG_MODIFIER` or 0.
    pub flags: u32,
    pub size: u64,
    pub modifier: u64,
}

/// Why no GEM handle was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiError {
    /// There is no transport.
    NoTransport,
    /// The KMD's own verdict, nothing from the host: the gate, a refusal, or a
    /// stale handle.
    Local(Verdict),
    /// The host's or the transport's verdict, with the host's errno (0 if none).
    Host(Verdict, u32),
}

/// Whether this device serves `RM_RESOURCE_IMPORT`: `IMPORT_RM` is served (config
/// features bits 13 and 10) and the host also advertises bit 14.
pub fn rm_resource_import_served(adapter: &AdapterContext) -> bool {
    rm_import_served(adapter)
        && adapter
            .with_virtio(|v| v.nvrm_device_features() & CFG_RM_RESOURCE_IMPORT != 0)
            .unwrap_or(false)
}

/// Make a GEM handle of `resource_id` in the caller's DRM file `rm_handle`.
/// `process` is the escaping device's `hKmdProcess` (0 if unknown: only the
/// creator route is then open).
///
/// PASSIVE only: it waits on the control queue, holding no lock.
pub fn rm_resource_import(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    process: usize,
    rm_handle: u32,
    resource_id: u32,
) -> Result<Imported, RiError> {
    if !rm_resource_import_served(adapter) {
        UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
        return Err(RiError::Local(Verdict::Unsupported));
    }
    let begin = adapter
        .with_virtio(|v| v.rm_resource_import_begin(owner, process, rm_handle, resource_id))
        .map_err(|_| RiError::NoTransport)?;
    let epoch = match begin {
        Ok(epoch) => epoch,
        Err(refusal) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            return Err(RiError::Local(refusal.verdict()));
        }
    };

    let req = ri::build_request(rm_handle, resource_id);
    let mut resp = [0u8; REPLY_BYTES];
    let n = match ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, TIMEOUT_MS) {
        Ok(n) => n,
        Err(_) => {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            return Err(RiError::Host(Verdict::Device, 0));
        }
    };
    let reply = match ri::parse_reply(&resp, n) {
        Ok(r) => r,
        Err(ReplyError::HostErrno(errno)) => {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            return Err(RiError::Host(ri::verdict_for_errno(errno), errno));
        }
        Err(ReplyError::Short | ReplyError::Malformed) => {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            return Err(RiError::Host(Verdict::Device, 0));
        }
    };

    let valid = adapter
        .with_virtio(|v| v.rm_resource_import_still_valid(owner, rm_handle, epoch))
        .unwrap_or(false);
    if !valid {
        STALE.fetch_add(1, Ordering::Relaxed);
        return Err(RiError::Local(Verdict::NotOwned));
    }
    OK.fetch_add(1, Ordering::Relaxed);
    Ok(Imported {
        gem_handle: reply.gem_handle,
        flags: reply.flags,
        size: reply.size,
        modifier: reply.modifier,
    })
}

/// Mirror the counters to the service key. PASSIVE only; the escape layer
/// throttles the calls.
pub fn publish_counters() {
    crate::diag::record_named_bytes(b"FgRiOk", OK.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FgRiRef", REFUSED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FgRiErr", ERRORS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FgRiUns", UNSUPPORTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FgRiStale", STALE.load(Ordering::Relaxed));
}

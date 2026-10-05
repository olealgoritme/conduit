//! Forwarding of NVIDIA-RM messages for `HELIOS_ESCAPE_NVRM` (the verb itself is
//! in `ddi/escape.rs`; the ABI is `helios_protocol::nvrm`).
//!
//! The KMD is a pipe with ownership, not an RM client: a request is forwarded
//! VERBATIM (`ctrl::raw_roundtrip`) and its reply returned as the device wrote
//! it. What this module adds is exactly the part a process must not be able to
//! get wrong about another:
//!
//! * which `msg_type`s may be forwarded at all;
//! * handle ownership — `Open` records the new handle against the caller,
//!   `Ioctl` and `Close` require it, a successful `Close` forgets it, and device
//!   teardown closes what a process left open;
//! * for an `Ioctl`, the 24-byte `IoctlReq` is read to refuse a page-run deep
//!   block (user mode never supplies a physical address) and a request whose
//!   declared lengths overrun it.
//!
//! Everything else in an RM message is opaque here.

use super::ctrl;
use super::gpu::DeviceOwner;
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_protocol::{
    HELIOS_NVRM_DEEP_PAGE_RUNS, HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT, HELIOS_NVRM_FORWARD_MSG_TYPES,
};

/// Host `MsgType` values that need KMD attention (see `helios_protocol::nvrm`).
const MSG_OPEN: u32 = 1;
const MSG_CLOSE: u32 = 2;
const MSG_IOCTL: u32 = 3;
const MSG_MMAP: u32 = 4;
const MSG_MUNMAP: u32 = 5;

/// Key of an NVRM mapping in `AdapterContext::mappings`, which is shared with
/// blob mappings (keyed by a small KMD-assigned resource id). The host's mapping
/// id is device-wide unique; the high bit keeps the two namespaces apart.
pub fn map_key(mapping_id: u32) -> u32 {
    mapping_id | 0x8000_0000
}

const PAGE: u64 = 4096;
/// Largest single mapping. `IoAllocateMdl` takes a ULONG length; this is well
/// inside it and far above anything an RM client maps in one piece.
pub const MAX_MAP_BYTES: u64 = 1 << 30;

const MSG_HDR: usize = super::hal::MSG_HDR_LEN;
/// `MsgHeader` plus the `IoctlReq` that follows it on an `Ioctl`.
const IOCTL_HDR: usize = MSG_HDR + 24;

/// Forwarded messages by kind, and refusals. Published as `NvOpen`, `NvClose`,
/// `NvIoctl`, `NvOther` and `NvRef`.
pub static NVRM_OPENS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_CLOSES: AtomicU32 = AtomicU32::new(0);
pub static NVRM_IOCTLS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_OTHER: AtomicU32 = AtomicU32::new(0);
pub static NVRM_REFUSED: AtomicU32 = AtomicU32::new(0);
/// Live mappings made, and their failures. Published as `NvMap`, `NvMapErr`.
pub static NVRM_MAPS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_MAP_ERRORS: AtomicU32 = AtomicU32::new(0);

/// Why a forward did not reach (or come back from) the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `msg_type` is not one `FORWARD` carries.
    MsgType,
    /// The handle is not one the caller opened.
    NotOwned,
    /// The handle table or the caller's quota is full.
    NoResources,
    /// The request carries a page-run deep block.
    Forbidden,
    /// A length in the request does not fit it.
    BadRange,
    /// The transport failed, timed out or is gone.
    Transport(VirtioError),
}

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    let a: [u8; 4] = s.try_into().ok()?;
    Some(u32::from_le_bytes(a))
}

fn rd_i32(b: &[u8], at: usize) -> Option<i32> {
    rd_u32(b, at).map(|v| v as i32)
}

fn owned(adapter: &AdapterContext, owner: DeviceOwner, handle: u32) -> bool {
    adapter
        .with_virtio(|v| v.nvrm_handle_owned(owner, handle))
        .unwrap_or(false)
}

/// Forward `req` (`MsgHeader | payload`) for `owner` and return how many reply
/// bytes were written to `resp`.
pub fn forward(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    req: &[u8],
    resp: &mut [u8],
    timeout_ms: u64,
) -> Result<usize, Refusal> {
    let refused = |r: Refusal| {
        NVRM_REFUSED.fetch_add(1, Ordering::Relaxed);
        r
    };
    let (Some(msg), Some(handle)) = (rd_u32(req, 0), rd_u32(req, 4)) else {
        return Err(refused(Refusal::BadRange));
    };
    // `msg >= 32` first: the mask is a u32 bitmap over the value.
    if msg >= 32 || (HELIOS_NVRM_FORWARD_MSG_TYPES >> msg) & 1 == 0 {
        return Err(refused(Refusal::MsgType));
    }
    match msg {
        MSG_OPEN => open(passive, adapter, owner, req, resp, timeout_ms),
        MSG_CLOSE => {
            // Take the entry off the table FIRST. The host may hand this number
            // to another process the moment it closes it, and a stale entry here
            // would let the old owner (or a second concurrent Close) name it.
            let device_type = adapter
                .with_virtio(|v| v.nvrm_handle_device_type(owner, handle))
                .ok()
                .flatten();
            let had = adapter
                .with_virtio(|v| v.take_nvrm_handle(owner, handle))
                .unwrap_or(false);
            let (true, Some(device_type)) = (had, device_type) else {
                return Err(refused(Refusal::NotOwned));
            };
            NVRM_CLOSES.fetch_add(1, Ordering::Relaxed);
            // Mappings go first (the ABI's teardown order: unmap, close): no user
            // address may outlive the host mapping it points at.
            release_maps_for_handle(passive, adapter, owner, handle);
            let restore = || {
                // The host did not close it, so it is still ours.
                let _ = adapter.with_virtio(|v| {
                    if v.reserve_nvrm_handle_slot(owner) {
                        v.commit_nvrm_handle(owner, handle, device_type);
                    }
                });
            };
            match ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms) {
                Ok(n) => {
                    if !(n >= MSG_HDR && rd_i32(resp, 8) == Some(0)) {
                        restore();
                    }
                    Ok(n)
                }
                // A timeout is indeterminate (it may have closed): stay forgotten.
                Err(VirtioError::Timeout) => Err(Refusal::Transport(VirtioError::Timeout)),
                // Anything else never reached the host.
                Err(e) => {
                    restore();
                    Err(Refusal::Transport(e))
                }
            }
        }
        MSG_IOCTL => {
            if !owned(adapter, owner, handle) {
                return Err(refused(Refusal::NotOwned));
            }
            if let Err(r) = check_ioctl(req) {
                return Err(refused(r));
            }
            NVRM_IOCTLS.fetch_add(1, Ordering::Relaxed);
            ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms).map_err(Refusal::Transport)
        }
        // GetProcFiles / GetSysFiles: no handle, nothing to track.
        _ => {
            NVRM_OTHER.fetch_add(1, Ordering::Relaxed);
            ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms).map_err(Refusal::Transport)
        }
    }
}

/// `Open`: reserve the slot first, so a full table refuses before the host opens
/// anything, then record the handle the host returned.
fn open(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    req: &[u8],
    resp: &mut [u8],
    timeout_ms: u64,
) -> Result<usize, Refusal> {
    let reserved = adapter
        .with_virtio(|v| v.reserve_nvrm_handle_slot(owner))
        .map_err(|_| Refusal::Transport(VirtioError::DeviceError))?;
    if !reserved {
        NVRM_REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(Refusal::NoResources);
    }
    NVRM_OPENS.fetch_add(1, Ordering::Relaxed);
    match ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms) {
        Ok(n) => {
            // The reply header carries the new handle and a signed status.
            let handle = rd_u32(resp, 4).unwrap_or(0);
            let ok = n >= MSG_HDR && handle != 0 && rd_i32(resp, 8) == Some(0);
            // The request's OpenReq: { device_type, flags } after the header.
            let device_type = rd_u32(req, MSG_HDR).unwrap_or(0);
            let _ = adapter.with_virtio(|v| {
                if ok {
                    v.commit_nvrm_handle(owner, handle, device_type);
                } else {
                    v.cancel_nvrm_reservation();
                }
            });
            Ok(n)
        }
        Err(e) => {
            // A timeout may still have opened it on the host; that handle is then
            // untracked until the next device reset. Rare, and bounded by quota.
            let _ = adapter.with_virtio(|v| v.cancel_nvrm_reservation());
            Err(Refusal::Transport(e))
        }
    }
}

/// The two checks an `Ioctl` gets (see the module docs): no page-run deep block,
/// and the declared data/nested/deep lengths must fit the request.
fn check_ioctl(req: &[u8]) -> Result<(), Refusal> {
    if req.len() < IOCTL_HDR {
        return Err(Refusal::BadRange);
    }
    // IoctlReq: cmd@16 data_len@20 nested_offset@24 nested_len@28
    // deep_ptr_offset@32 deep_len@36.
    let (Some(data_len), Some(nested_len), Some(deep_ptr), Some(deep_len)) = (
        rd_u32(req, 20),
        rd_u32(req, 28),
        rd_u32(req, 32),
        rd_u32(req, 36),
    ) else {
        return Err(Refusal::BadRange);
    };
    // Only the KMD ever writes these sentinels, and only from pages it locked
    // itself: a table from user mode would name guest-physical memory the host
    // then maps.
    if deep_ptr == HELIOS_NVRM_DEEP_PAGE_RUNS || deep_ptr == HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT {
        return Err(Refusal::Forbidden);
    }
    let declared = IOCTL_HDR as u64 + u64::from(data_len) + u64::from(nested_len) + u64::from(deep_len);
    if declared > req.len() as u64 {
        return Err(Refusal::BadRange);
    }
    Ok(())
}

/// The result of a host `Mmap`.
pub struct HostMapping {
    /// Offset of the mapping inside the region (`guest_phys_addr` in the wire
    /// struct: an offset, not an address).
    pub offset: u64,
    pub size: u64,
    pub mapping_id: u32,
    /// The `device_type` of the handle, which selects the region.
    pub device_type: u32,
}

/// Why a map could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapRefusal {
    NotOwned,
    BadRange,
    NoResources,
    Unsupported,
    /// The host refused: its errno, positive.
    Host(i32),
    Transport(VirtioError),
}

/// The shared-memory region an `Mmap` on a handle of `device_type` points into.
pub fn region_for(
    adapter: &AdapterContext,
    device_type: u32,
) -> Option<super::pci_caps::HostVisibleWindow> {
    adapter
        .with_virtio(|v| v.nvrm_region(device_type))
        .ok()
        .flatten()
}

/// Ask the host to `Mmap` `size` bytes at `offset` of the file `handle`. Nothing
/// is mapped here; the caller maps `region[offset..]` into the process and, on
/// ANY later failure, must call [`host_munmap`] so the host does not keep the
/// mapping.
pub fn host_mmap(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    write: bool,
    offset: u64,
    size: u64,
) -> Result<HostMapping, MapRefusal> {
    let Some(device_type) = adapter
        .with_virtio(|v| v.nvrm_handle_device_type(owner, handle))
        .ok()
        .flatten()
    else {
        return Err(MapRefusal::NotOwned);
    };
    if size == 0 || size % PAGE != 0 || size > MAX_MAP_BYTES || offset % PAGE != 0 {
        return Err(MapRefusal::BadRange);
    }
    let quota_ok = adapter
        .with_virtio(|v| v.nvrm_map_count(owner) < super::gpu::MAX_NVRM_MAPS_PER_OWNER)
        .unwrap_or(false);
    if !quota_ok {
        return Err(MapRefusal::NoResources);
    }

    // MsgHeader{Mmap, handle} | MmapReq { size u64, offset u64, prot u32, pad u32 }.
    // `prot` as the guest module sends it: 3 for a writable mapping, 1 otherwise.
    let mut req = [0u8; MSG_HDR + 24];
    req[..4].copy_from_slice(&MSG_MMAP.to_le_bytes());
    req[4..8].copy_from_slice(&handle.to_le_bytes());
    req[16..24].copy_from_slice(&size.to_le_bytes());
    req[24..32].copy_from_slice(&offset.to_le_bytes());
    req[32..36].copy_from_slice(&(if write { 3u32 } else { 1u32 }).to_le_bytes());
    let mut resp = [0u8; 64];
    let n = ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, 30_000)
        .map_err(MapRefusal::Transport)?;
    if let Some(status) = rd_i32(&resp, 8) {
        if status < 0 {
            return Err(MapRefusal::Host(status.wrapping_neg().max(1)));
        }
    }
    // MsgHeader | MmapResp { guest_phys_addr u64, size u64, mapping_id u32, pad u32 }.
    if n < MSG_HDR + 24 {
        return Err(MapRefusal::Transport(VirtioError::DeviceError));
    }
    let (Some(lo), Some(hi), Some(slo), Some(shi), Some(mapping_id)) = (
        rd_u32(&resp, 16),
        rd_u32(&resp, 20),
        rd_u32(&resp, 24),
        rd_u32(&resp, 28),
        rd_u32(&resp, 32),
    ) else {
        return Err(MapRefusal::Transport(VirtioError::DeviceError));
    };
    Ok(HostMapping {
        offset: u64::from(lo) | (u64::from(hi) << 32),
        size: u64::from(slo) | (u64::from(shi) << 32),
        mapping_id,
        device_type,
    })
}

/// Tell the host to drop mapping `mapping_id` of `handle`. Best effort.
pub fn host_munmap(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    handle: u32,
    mapping_id: u32,
) -> Result<(), VirtioError> {
    // MsgHeader{Munmap, handle} | MunmapReq { mapping_id u32, pad u32 }.
    let mut req = [0u8; MSG_HDR + 8];
    req[..4].copy_from_slice(&MSG_MUNMAP.to_le_bytes());
    req[4..8].copy_from_slice(&handle.to_le_bytes());
    req[16..20].copy_from_slice(&mapping_id.to_le_bytes());
    let mut resp = [0u8; MSG_HDR];
    ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, 5_000).map(|_| ())
}

/// `Close` of `handle`: unmap each view this process holds on it, then tell the
/// host. Runs in the owning process (a `Close` escape), as the unmap requires.
fn release_maps_for_handle(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
) {
    loop {
        let id = adapter
            .with_virtio(|v| v.take_nvrm_map_for_handle(owner, handle))
            .ok()
            .flatten();
        let Some(id) = id else {
            break;
        };
        if let Some((va, mdl)) = adapter.mappings.take_for_resource(owner.raw(), map_key(id)) {
            // SAFETY: PASSIVE, in the process that mapped it; the pair came from
            // `map_io_pages_to_user` and was removed from the table just now.
            unsafe { crate::ddi::unmap_io_pages_from_user(va, mdl as *mut wdk_sys::MDL) };
        }
        let _ = host_munmap(passive, adapter, handle, id);
    }
}

/// Device teardown: close, on the host, every handle `owner` left open. Returns
/// how many. A close that fails is dropped — the device may be going away, and
/// the table entry is already gone, so nothing is retried.
pub fn close_all_for_owner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
) -> u32 {
    let mut closed = 0u32;
    // After the first transport failure the host is not answering: stop sending
    // (a wedged host would cost seconds per handle) but keep clearing the table,
    // so no entry outlives the device handle it names.
    let mut sending = true;
    // Mappings first. Their user views were already unmapped by the device
    // teardown's drain of `AdapterContext::mappings`; what is left is telling
    // the host, before the handles they hang off are closed.
    loop {
        let map = adapter
            .with_virtio(|v| v.take_nvrm_map_for_owner(owner))
            .ok()
            .flatten();
        let Some((handle, id)) = map else {
            break;
        };
        if sending {
            match host_munmap(passive, adapter, handle, id) {
                Err(VirtioError::Timeout) | Err(VirtioError::DeviceError) => sending = false,
                _ => {}
            }
        }
    }
    loop {
        let taken = adapter
            .with_virtio(|v| v.take_nvrm_handle_for_owner(owner))
            .ok()
            .flatten();
        let Some(handle) = taken else {
            break;
        };
        if sending {
            let mut req = [0u8; MSG_HDR];
            req[..4].copy_from_slice(&MSG_CLOSE.to_le_bytes());
            req[4..8].copy_from_slice(&handle.to_le_bytes());
            let mut resp = [0u8; MSG_HDR];
            match ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, 5_000) {
                Err(VirtioError::Timeout) | Err(VirtioError::DeviceError) => sending = false,
                _ => {}
            }
        }
        closed += 1;
    }
    closed
}

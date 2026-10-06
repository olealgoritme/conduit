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
//!   declared lengths overrun it;
//! * pinned user pages: the KMD locks them, builds the page-run table itself and
//!   splices it into the registration `Ioctl` (`forward_pinned`), then keeps the
//!   lock exactly as long as the GPU may use the pages (see `helios_protocol::nvrm`
//!   `HeliosNvrmPin`). Of RM it recognises one call, `NV_ESC_RM_FREE`.
//!
//! * usermode events: `register_event` / `unregister_event` tie a process's
//!   `KEVENT` to a backend handle (or to the loss of the transport); `Close` and
//!   `close_all_for_owner` drop what a handle or a process registered, at PASSIVE
//!   and outside every lock (the DPC side is `virtio::gpu::nvrm_events`).
//!
//! Everything else in an RM message is opaque here.

use super::ctrl;
use super::gpu::{
    release_nvrm_event, DeviceOwner, NvrmEventRefusal, NvrmPin, MAX_NVRM_PINS_PER_OWNER,
    MAX_NVRM_PIN_PAGES,
};
use super::hal::DmaBuffer;
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::page_runs;
use helios_protocol::{
    HELIOS_NVRM_DEEP_PAGE_RUNS, HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT, HELIOS_NVRM_FORWARD_MSG_TYPES,
    HELIOS_NVRM_SCANOUT_FLIP_BYTES,
};
use wdk_sys::{KEVENT, MDL, PMDL};

extern "C" {
    /// `MmProbeAndLockPages` for USER memory raises on a bad range; the C shim
    /// (`src/seh_shim.c`) turns that into NULL. PASSIVE, in the owning process.
    fn helios_lock_user_pages_seh(virtual_address: *mut c_void, length: u32) -> PMDL;
    /// `MmUnlockPages` + `IoFreeMdl`; callable from any process context.
    fn helios_unlock_system_buffer(mdl: PMDL);
}

/// Host `MsgType` values that need KMD attention (see `helios_protocol::nvrm`).
const MSG_OPEN: u32 = 1;
const MSG_CLOSE: u32 = 2;
const MSG_IOCTL: u32 = 3;
const MSG_MMAP: u32 = 4;
const MSG_MUNMAP: u32 = 5;
const MSG_SCANOUT_FLIP: u32 = 20;
/// `device_type` from which an `Open` names a DRM node (`512 + minor`).
const DEVICE_TYPE_DRI_FIRST: u32 = 512;

/// `NV_ESC_RM_FREE` as the low 16 bits of the Linux ioctl number the guest sends:
/// `('F' << 8) | 0x29`.
const CMD_RM_FREE_LOW16: u32 = 0x4629;

/// Key of an NVRM mapping in `AdapterContext::mappings`, which is shared with
/// blob mappings (keyed by a small KMD-assigned resource id). The KMD-assigned
/// mapping ids stay below `0x7FFF_FFF0`; the high bit keeps the two namespaces
/// apart.
pub fn map_key(kmd_id: u32) -> u32 {
    kmd_id | 0x8000_0000
}

const PAGE: u64 = 4096;
/// Largest single mapping. Its MDL is 2 KiB per MiB of non-paged pool, so this
/// bounds one mapping's pool cost to about half a megabyte.
pub const MAX_MAP_BYTES: u64 = 256 << 20;

const MSG_HDR: usize = super::hal::MSG_HDR_LEN;
/// `MsgHeader` plus the `IoctlReq` that follows it on an `Ioctl`.
const IOCTL_HDR: usize = MSG_HDR + 24;
/// `MsgHeader` plus the 12-byte `IoctlResp`: where an Ioctl reply's data begins.
const REPLY_DATA: usize = MSG_HDR + 12;

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
/// Pins made, pins released, pin failures. `NvPin - NvUnpin` is what is locked
/// now; a count that only grows is a leak. Published as `NvPin`, `NvUnpin`,
/// `NvPinErr`.
/// Forwarded `ScanoutFlip`s (zero-copy presents).
pub static NVRM_FLIPS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_PINS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_UNPINS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_PIN_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Event registrations made, removed by `EVENT_UNREGISTER`, and refused. Published
/// as `NvEvReg`, `NvEvUnreg`, `NvEvRef`. `Close`, process exit and reset remove
/// registrations too and are not counted separately, so only the order of
/// magnitude is a check: a count far above anything that removes is a leak.
pub static NVRM_EV_REGS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_EV_UNREGS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_EV_REFUSED: AtomicU32 = AtomicU32::new(0);
/// `KeSetEvent`s for an `EventReady` (or a latched one at registration), and the
/// notifications latched / dropped because nothing was registered. `NvEvSig`,
/// `NvEvLatch`, `NvEvDrop`.
pub static NVRM_EV_SIGNALS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_EV_LATCHED: AtomicU32 = AtomicU32::new(0);
pub static NVRM_EV_DROPS: AtomicU32 = AtomicU32::new(0);
/// Registrations woken because the transport was lost (`NvEvLost`); event-queue
/// messages other than `EventReady` (`NvEvOther`: nonzero means the host sent
/// something this driver does not consume, e.g. `InputEvent`); and faults of the
/// event queue itself (`NvEvErr`: a buffer that would not repost, a bad token,
/// a handle that would not resolve). `NvEvErr` should read 0.
pub static NVRM_EV_LOST: AtomicU32 = AtomicU32::new(0);
pub static NVRM_EV_OTHER: AtomicU32 = AtomicU32::new(0);
pub static NVRM_EV_ERRORS: AtomicU32 = AtomicU32::new(0);

/// Why a forward did not reach (or come back from) the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `msg_type` is not one `FORWARD` carries.
    MsgType,
    /// The handle is not one the caller opened.
    NotOwned,
    /// The handle table or the caller's quota is full.
    NoResources,
    /// The request carries something user mode may not supply.
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

// ---- forwarding ------------------------------------------------------------------

/// Forward `req` (`MsgHeader | payload`) for `owner` and return how many reply
/// bytes were written to `resp`.
///
/// `pin_id != 0` (an `Ioctl` only) splices the KMD-built page-run table of that
/// pin into the request; `rm_status_off` says where RM's status sits in the reply
/// so the pin is kept only if the registration worked.
#[allow(clippy::too_many_arguments)]
pub fn forward(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    req: &[u8],
    resp: &mut [u8],
    timeout_ms: u64,
    pin_id: u32,
    rm_status_off: u32,
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
    if pin_id != 0 && msg != MSG_IOCTL {
        return Err(refused(Refusal::Forbidden));
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
                    if n >= MSG_HDR && rd_i32(resp, 8) == Some(0) {
                        // The handle is closed: its event registrations go (a
                        // wake for a handle nobody holds is dropped by the router
                        // already, so the gap is harmless). A failed Close keeps
                        // them, because the handle stays open.
                        release_events_for_handle(adapter, owner, handle);
                        // The host's objects, and its alias of the pinned pages,
                        // are gone: the pins hang off this handle and unlock now.
                        release_pins_for_handle(adapter, owner, handle);
                        // A foreign scanout source on this file ends with it.
                        adapter.foreign_scanout_release_handle(owner, handle);
                    } else {
                        restore();
                    }
                    Ok(n)
                }
                // A timeout is indeterminate (it may have closed): stay forgotten.
                Err(VirtioError::Timeout) => {
                    release_events_for_handle(adapter, owner, handle);
                    adapter.foreign_scanout_release_handle(owner, handle);
                    Err(Refusal::Transport(VirtioError::Timeout))
                }
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
            if let Err(r) = check_ioctl(req, pin_id != 0) {
                return Err(refused(r));
            }
            if pin_id != 0 {
                return forward_pinned(
                    passive,
                    adapter,
                    owner,
                    handle,
                    req,
                    resp,
                    timeout_ms,
                    pin_id,
                    rm_status_off,
                )
                .map_err(refused);
            }
            NVRM_IOCTLS.fetch_add(1, Ordering::Relaxed);
            let n = ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms)
                .map_err(Refusal::Transport)?;
            after_ioctl(adapter, owner, req, resp, n);
            Ok(n)
        }
        MSG_SCANOUT_FLIP => {
            // ScanoutFlip { scanout, owner_handle, host_handle, .. } (64 bytes):
            // the host exports the GEM object in `owner_handle`'s DRM file and
            // hands it to the display. The only things worth checking here: it is
            // the one scanout, and the file is the caller's own DRM node.
            if req.len() != MSG_HDR + HELIOS_NVRM_SCANOUT_FLIP_BYTES {
                return Err(refused(Refusal::BadRange));
            }
            let (Some(scanout), Some(owner_handle)) =
                (rd_u32(req, MSG_HDR), rd_u32(req, MSG_HDR + 4))
            else {
                return Err(refused(Refusal::BadRange));
            };
            if scanout != 0 {
                return Err(refused(Refusal::BadRange));
            }
            let device_type = adapter
                .with_virtio(|v| v.nvrm_handle_device_type(owner, owner_handle))
                .ok()
                .flatten();
            match device_type {
                None => return Err(refused(Refusal::NotOwned)),
                Some(t) if t < DEVICE_TYPE_DRI_FIRST => return Err(refused(Refusal::Forbidden)),
                Some(_) => {}
            }
            // Another device holds scanout 0 as a foreign scanout source: its
            // frames must not alternate with this one's.
            if adapter.foreign_scanout_blocks_flip(owner) {
                return Err(refused(Refusal::Forbidden));
            }
            NVRM_FLIPS.fetch_add(1, Ordering::Relaxed);
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

/// The checks an `Ioctl` gets (see the module docs): the declared data / nested /
/// deep lengths must fit the request, and the deep block must not be a page-run
/// table. With `pinned`, the KMD writes the deep fields itself, so the caller's
/// must be empty.
fn check_ioctl(req: &[u8], pinned: bool) -> Result<(), Refusal> {
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
    if pinned && (deep_ptr != 0 || deep_len != 0) {
        return Err(Refusal::Forbidden);
    }
    let declared = IOCTL_HDR as u64 + u64::from(data_len) + u64::from(nested_len) + u64::from(deep_len);
    if declared > req.len() as u64 {
        return Err(Refusal::BadRange);
    }
    Ok(())
}

/// After a forwarded `Ioctl` that was not a registration: the one RM call the KMD
/// recognises is `NV_ESC_RM_FREE`, and when the host confirms one, the pins it
/// freed are unlocked. The flat 16-byte `NVOS00 { hRoot, hObjectParent,
/// hObjectOld, status }` starts at the data block.
fn after_ioctl(adapter: &AdapterContext, owner: DeviceOwner, req: &[u8], resp: &[u8], n: usize) {
    let (Some(cmd), Some(data_len)) = (rd_u32(req, 16), rd_u32(req, 20)) else {
        return;
    };
    if cmd & 0xFFFF != CMD_RM_FREE_LOW16 || data_len != 16 {
        return;
    }
    let (Some(h_root), Some(h_old)) = (rd_u32(req, IOCTL_HDR), rd_u32(req, IOCTL_HDR + 8)) else {
        return;
    };
    // The host's verdict and RM's own `status` (data + 12) must both be success.
    let ok = n >= REPLY_DATA + 16
        && rd_i32(resp, 8) == Some(0)
        && rd_u32(resp, REPLY_DATA + 12) == Some(0);
    if !ok {
        return;
    }
    loop {
        let pin = adapter
            .with_virtio(|v| v.take_nvrm_pin_for_free(owner, h_root, h_old))
            .ok()
            .flatten();
        let Some(pin) = pin else {
            break;
        };
        release_pin(pin);
    }
}

// ---- pins ----------------------------------------------------------------------------

/// Why a pin could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinRefusal {
    NotOwned,
    BadRange,
    NoResources,
    TooScattered,
}

/// A made pin.
pub struct PinOut {
    pub id: u32,
    pub npages: u32,
}

/// Unlock a pin's pages and free its tables. PASSIVE, outside every lock: the
/// contiguous table buffer cannot be freed above PASSIVE, and the pin was just
/// removed from the table, so this is its only owner.
pub fn release_pin(pin: NvrmPin) {
    // SAFETY: `pin.mdl` is the locked MDL `helios_lock_user_pages_seh` returned,
    // released exactly once here.
    unsafe { helios_unlock_system_buffer(pin.mdl as PMDL) };
    NVRM_UNPINS.fetch_add(1, Ordering::Relaxed);
    drop(pin);
}

fn discard_pin(adapter: &AdapterContext, owner: DeviceOwner, id: u32) {
    let pin = adapter
        .with_virtio(|v| v.take_nvrm_pin(owner, id))
        .ok()
        .flatten();
    if let Some(pin) = pin {
        release_pin(pin);
    }
}

fn release_pins_for_handle(adapter: &AdapterContext, owner: DeviceOwner, handle: u32) {
    loop {
        let pin = adapter
            .with_virtio(|v| v.take_nvrm_pin_for_handle(owner, handle))
            .ok()
            .flatten();
        let Some(pin) = pin else {
            break;
        };
        release_pin(pin);
    }
}

/// The deep block for a locked MDL: the run table itself when it fits a message,
/// else a one-run table pointing at a big one the KMD keeps in contiguous memory.
fn build_table(
    passive: PassiveLevel,
    mdl: PMDL,
    pages: usize,
) -> Result<(u32, Box<[u8]>, Option<DmaBuffer>), PinRefusal> {
    // SAFETY: a locked MDL's page-frame array follows its header and holds one
    // entry per page (the range is page aligned), valid while the pages stay locked.
    let pfns = unsafe {
        core::slice::from_raw_parts(
            (mdl as *const u8).add(core::mem::size_of::<MDL>()) as *const u64,
            pages,
        )
    };
    let runs = page_runs::count_runs(pfns);
    if runs == 0 {
        return Err(PinRefusal::BadRange);
    }
    if runs <= page_runs::DIRECT_MAX_RUNS {
        let bytes = page_runs::table_bytes(runs);
        let mut table = Vec::<u8>::new();
        if table.try_reserve_exact(bytes).is_err() {
            return Err(PinRefusal::NoResources);
        }
        table.resize(bytes, 0);
        let written = page_runs::encode(pfns, &mut table).ok_or(PinRefusal::BadRange)?;
        table.truncate(written);
        return Ok((HELIOS_NVRM_DEEP_PAGE_RUNS, table.into_boxed_slice(), None));
    }
    if runs > page_runs::INDIRECT_MAX_RUNS {
        return Err(PinRefusal::TooScattered);
    }
    // Too scattered for a message: the table stays in guest memory and the
    // message names the pages that hold it. Contiguous, so ONE run.
    let bytes = page_runs::table_bytes(runs);
    let mut big = DmaBuffer::new(passive, bytes).ok_or(PinRefusal::NoResources)?;
    page_runs::encode(pfns, big.as_mut_slice()).ok_or(PinRefusal::BadRange)?;
    let span = (bytes as u64 + PAGE - 1) & !(PAGE - 1);
    let mut head = Vec::<u8>::new();
    let head_bytes = page_runs::table_bytes(1);
    if head.try_reserve_exact(head_bytes).is_err() {
        return Err(PinRefusal::NoResources);
    }
    head.resize(head_bytes, 0);
    page_runs::encode_single_run(big.physical_address(), span, &mut head)
        .ok_or(PinRefusal::BadRange)?;
    Ok((
        HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT,
        head.into_boxed_slice(),
        Some(big),
    ))
}

/// `HELIOS_NVRM_OP_PIN`: lock `[user_va, user_va + length)` of the CALLING process
/// and remember it, with the page-run table the registration will carry. Must run
/// at PASSIVE in the owning process (an escape from its device handle).
#[allow(clippy::too_many_arguments)]
pub fn pin_pages(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    user_va: u64,
    length: u64,
    h_root: u32,
    h_object: u32,
) -> Result<PinOut, PinRefusal> {
    if !owned(adapter, owner, handle) {
        return Err(PinRefusal::NotOwned);
    }
    if length == 0 || user_va % PAGE != 0 || length % PAGE != 0 {
        return Err(PinRefusal::BadRange);
    }
    let pages = length / PAGE;
    if pages > MAX_NVRM_PIN_PAGES as u64 {
        return Err(PinRefusal::BadRange);
    }
    let quota_ok = adapter
        .with_virtio(|v| v.nvrm_pin_count(owner) < MAX_NVRM_PINS_PER_OWNER)
        .unwrap_or(false);
    if !quota_ok {
        return Err(PinRefusal::NoResources);
    }

    // SAFETY: PASSIVE_LEVEL in the owning process; `length` is at most
    // `MAX_NVRM_PIN_PAGES` pages, so it fits a ULONG. A bad range comes back NULL.
    let mdl = unsafe { helios_lock_user_pages_seh(user_va as *mut c_void, length as u32) };
    if mdl.is_null() {
        NVRM_PIN_ERRORS.fetch_add(1, Ordering::Relaxed);
        return Err(PinRefusal::BadRange);
    }
    let (deep_kind, deep, big) = match build_table(passive, mdl, pages as usize) {
        Ok(t) => t,
        Err(r) => {
            // SAFETY: just locked above, not yet recorded anywhere.
            unsafe { helios_unlock_system_buffer(mdl) };
            NVRM_PIN_ERRORS.fetch_add(1, Ordering::Relaxed);
            return Err(r);
        }
    };
    NVRM_PINS.fetch_add(1, Ordering::Relaxed);
    let pin = NvrmPin::new(
        owner,
        handle,
        h_root,
        h_object,
        mdl as usize,
        deep_kind,
        deep,
        big,
        pages as u32,
    );
    // The pin must come back out if the closure never runs (the transport is
    // gone) as well as if the table refuses it, or its pages stay locked.
    let mut slot = Some(pin);
    let pushed = adapter.with_virtio(|v| slot.take().map(|p| v.push_nvrm_pin(p)));
    match pushed {
        Ok(Some(Ok(id))) => Ok(PinOut {
            id,
            npages: pages as u32,
        }),
        Ok(Some(Err(pin))) => {
            release_pin(pin);
            NVRM_PIN_ERRORS.fetch_add(1, Ordering::Relaxed);
            Err(PinRefusal::NoResources)
        }
        _ => {
            if let Some(pin) = slot.take() {
                release_pin(pin);
            }
            NVRM_PIN_ERRORS.fetch_add(1, Ordering::Relaxed);
            Err(PinRefusal::NoResources)
        }
    }
}

/// RM's own status word in an Ioctl reply of `n` bytes, `off` bytes into the data
/// block; `None` when the reply does not reach it.
fn rm_status_word(resp: &[u8], n: usize, off: u32) -> Option<u32> {
    let at = REPLY_DATA.checked_add(off as usize)?;
    if at.checked_add(4)? > n {
        return None;
    }
    rd_u32(resp, at)
}

/// The registration `Ioctl` of a pin: send the caller's request with the pin's
/// table appended as its deep block (the KMD, not the caller, says where it is
/// and what it holds), and keep the pin only if the registration succeeded.
#[allow(clippy::too_many_arguments)]
fn forward_pinned(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    req: &[u8],
    resp: &mut [u8],
    timeout_ms: u64,
    pin_id: u32,
    rm_status_off: u32,
) -> Result<usize, Refusal> {
    // The status word must be inside the reply the caller made room for, or the
    // outcome could never be judged.
    if (REPLY_DATA as u64) + u64::from(rm_status_off) + 4 > resp.len() as u64 {
        return Err(Refusal::BadRange);
    }
    // The host takes the deep block directly after the declared data and nested
    // blocks, so the table goes there: bytes the caller left past them must not
    // end up in its place.
    let (Some(data_len), Some(nested_len)) = (rd_u32(req, 20), rd_u32(req, 28)) else {
        return Err(Refusal::BadRange);
    };
    let declared = IOCTL_HDR as u64 + u64::from(data_len) + u64::from(nested_len);
    let req = req.get(..declared as usize).ok_or(Refusal::BadRange)?;
    let deep_len = adapter
        .with_virtio(|v| v.nvrm_pin_deep_len(owner, handle, pin_id))
        .ok()
        .flatten()
        .ok_or(Refusal::NotOwned)?;
    let total = req.len().checked_add(deep_len).ok_or(Refusal::BadRange)?;
    let mut buf = Vec::<u8>::new();
    if buf.try_reserve_exact(total).is_err() {
        return Err(Refusal::Transport(VirtioError::OutOfMemory));
    }
    buf.extend_from_slice(req);
    buf.resize(total, 0);
    let kind = {
        let tail = buf.get_mut(req.len()..).ok_or(Refusal::BadRange)?;
        adapter
            .with_virtio(|v| v.claim_nvrm_pin_deep(owner, handle, pin_id, tail))
            .ok()
            .flatten()
            .ok_or(Refusal::NotOwned)?
    };
    // IoctlReq.deep_ptr_offset@32 deep_len@36 (the caller's were checked empty).
    if let Some(d) = buf.get_mut(32..36) {
        d.copy_from_slice(&kind.to_le_bytes());
    }
    if let Some(d) = buf.get_mut(36..40) {
        d.copy_from_slice(&(deep_len as u32).to_le_bytes());
    }
    NVRM_IOCTLS.fetch_add(1, Ordering::Relaxed);
    let result = ctrl::raw_roundtrip(passive, adapter, &buf, resp, timeout_ms);
    let keep = match &result {
        Ok(n) => {
            *n >= MSG_HDR
                && rd_i32(resp, 8) == Some(0)
                // A status the reply does not reach is indeterminate, as a timeout
                // is: the host succeeded and may hold the pages, so keep the pin.
                && rm_status_word(resp, *n, rm_status_off).is_none_or(|w| w == 0)
        }
        // A timeout is indeterminate: the GPU may hold the pages. Keep the pin;
        // `Close`, process exit or reset releases it.
        Err(VirtioError::Timeout) => true,
        Err(_) => false,
    };
    if !keep {
        discard_pin(adapter, owner, pin_id);
    }
    result.map_err(Refusal::Transport)
}

// ---- mappings -----------------------------------------------------------------------

/// The result of a host `Mmap`.
pub struct HostMapping {
    /// Offset of the mapping inside the region (`guest_phys_addr` in the wire
    /// struct: an offset, not an address).
    pub offset: u64,
    pub size: u64,
    /// The host's mapping id. NOT unique and not necessarily nonzero (the RM path
    /// answers 0 for every mapping): the KMD mints its own.
    pub host_id: u32,
    /// The `device_type` of the handle, which selects the region.
    pub device_type: u32,
}

/// Why a map could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapRefusal {
    NotOwned,
    BadRange,
    NoResources,
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
/// ANY later failure, must call [`release_host_map`] so the host does not keep
/// the mapping.
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
    let (Some(lo), Some(hi), Some(slo), Some(shi), Some(host_id)) = (
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
        host_id,
        device_type,
    })
}

/// Tell the host to drop mapping `host_id` of `handle`. Best effort.
pub fn host_munmap(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    handle: u32,
    host_id: u32,
) -> Result<(), VirtioError> {
    // MsgHeader{Munmap, handle} | MunmapReq { mapping_id u32, pad u32 }.
    let mut req = [0u8; MSG_HDR + 8];
    req[..4].copy_from_slice(&MSG_MUNMAP.to_le_bytes());
    req[4..8].copy_from_slice(&handle.to_le_bytes());
    req[16..20].copy_from_slice(&host_id.to_le_bytes());
    let mut resp = [0u8; MSG_HDR];
    ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, 5_000).map(|_| ())
}

/// Release the HOST side of a mapping whose table slot is already gone (or was
/// never made). Skipped for id 0 — the RM path's mappings are released by RM
/// itself (`NV_ESC_RM_UNMAP_MEMORY`), and the host ignores a zero id — and while
/// another live mapping still carries the same nonzero id, which a repeat mapping
/// of one DRM object can: sending it would free the window under the survivor.
pub fn release_host_map(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    handle: u32,
    host_id: u32,
) -> Result<(), VirtioError> {
    if host_id == 0 {
        return Ok(());
    }
    let still_used = adapter
        .with_virtio(|v| v.nvrm_host_map_tracked(handle, host_id))
        .unwrap_or(false);
    if still_used {
        return Ok(());
    }
    host_munmap(passive, adapter, handle, host_id)
}

/// `Close` of `handle`: unmap each view this process holds on it, then tell the
/// host. Runs in the owning process (a `Close` escape), as the unmap requires.
/// Stops sending after the first failure (a wedged host would cost seconds per
/// mapping) but still unmaps every view.
fn release_maps_for_handle(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
) {
    let mut sending = true;
    loop {
        let slot = adapter
            .with_virtio(|v| v.take_nvrm_map_for_handle(owner, handle))
            .ok()
            .flatten();
        let Some((kmd_id, host_id)) = slot else {
            break;
        };
        if let Some((va, mdl)) = adapter
            .mappings
            .take_for_resource(owner.raw(), map_key(kmd_id))
        {
            // SAFETY: PASSIVE, in the process that mapped it; the pair came from
            // `map_io_pages_to_user` and was removed from the table just now.
            unsafe { crate::ddi::unmap_io_pages_from_user(va, mdl as *mut wdk_sys::MDL) };
        }
        if sending {
            if let Err(VirtioError::Timeout) | Err(VirtioError::DeviceError) =
                release_host_map(passive, adapter, handle, host_id)
            {
                sending = false;
            }
        }
    }
}

// ---- events -------------------------------------------------------------------------

/// What a successful `REGISTER` did, as `HeliosNvrmEvent.out_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventState {
    Registered,
    Replaced,
    /// A notification had been latched: the event was signalled at once.
    LatchedSignaled,
}

/// Why a `REGISTER` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventRefusal {
    /// The event queue is not up (see `gpu::nvrm_events`).
    Unsupported,
    TransportLost,
    NotOwned,
    NoResources,
    /// There is no transport at all.
    NoTransport,
}

/// `EVENT_REGISTER`: record `event` for `(owner, handle, kind)`. PASSIVE, in the
/// caller's process (the reference was just taken there). TAKES OVER the caller's
/// object reference in every case: on success the table owns it (and the one a
/// replacement displaced is released here), on refusal it is released here.
pub fn register_event(
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    kind: u32,
    event: NonNull<KEVENT>,
) -> Result<EventState, EventRefusal> {
    // `event` is Copy, so the closure takes a copy and the original is still ours
    // to release if the closure never runs (the transport is gone).
    let result = adapter.with_virtio(|v| v.register_nvrm_event(owner, handle, kind, event));
    let refused = |r: EventRefusal| {
        NVRM_EV_REFUSED.fetch_add(1, Ordering::Relaxed);
        release_nvrm_event(event);
        r
    };
    match result {
        Err(_) => Err(refused(EventRefusal::NoTransport)),
        Ok(Err(NvrmEventRefusal::Unavailable)) => Err(refused(EventRefusal::Unsupported)),
        Ok(Err(NvrmEventRefusal::TransportLost)) => Err(refused(EventRefusal::TransportLost)),
        Ok(Err(NvrmEventRefusal::NotOwned)) => Err(refused(EventRefusal::NotOwned)),
        Ok(Err(NvrmEventRefusal::NoResources)) => Err(refused(EventRefusal::NoResources)),
        Ok(Ok(done)) => {
            NVRM_EV_REGS.fetch_add(1, Ordering::Relaxed);
            let replaced = done.replaced.is_some();
            if let Some(old) = done.replaced {
                release_nvrm_event(old);
            }
            Ok(if done.latched {
                EventState::LatchedSignaled
            } else if replaced {
                EventState::Replaced
            } else {
                EventState::Registered
            })
        }
    }
}

/// `EVENT_UNREGISTER`: `Some(true)` if it removed one, `Some(false)` if there was
/// none, `None` if there is no transport. PASSIVE.
pub fn unregister_event(
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    kind: u32,
) -> Option<bool> {
    match adapter.with_virtio(|v| v.unregister_nvrm_event(owner, handle, kind)) {
        Ok(Some(old)) => {
            NVRM_EV_UNREGS.fetch_add(1, Ordering::Relaxed);
            release_nvrm_event(old);
            Some(true)
        }
        Ok(None) => Some(false),
        Err(_) => None,
    }
}

/// `Close` of `handle`: drop what `owner` registered on it.
fn release_events_for_handle(adapter: &AdapterContext, owner: DeviceOwner, handle: u32) {
    loop {
        let event = adapter
            .with_virtio(|v| v.take_nvrm_event_for_handle(owner, handle))
            .ok()
            .flatten();
        let Some(event) = event else {
            break;
        };
        release_nvrm_event(event);
    }
}

/// Device teardown: drop everything `owner` registered, `TRANSPORT_LOST`
/// registrations included.
fn release_events_for_owner(adapter: &AdapterContext, owner: DeviceOwner) {
    loop {
        let event = adapter
            .with_virtio(|v| v.take_nvrm_event_for_owner(owner))
            .ok()
            .flatten();
        let Some(event) = event else {
            break;
        };
        release_nvrm_event(event);
    }
}

// ---- teardown ----------------------------------------------------------------------

/// Device teardown: release, on the host, everything `owner` left behind — its
/// mappings, then its handles — and unlock its pins. Returns how many handles
/// were closed. A close that fails is dropped — the device may be going away, and
/// the table entry is already gone, so nothing is retried.
pub fn close_all_for_owner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
) -> u32 {
    let mut closed = 0u32;
    // Events first: nothing of this owner's may be signalled from here on, and the
    // references are PASSIVE-only to drop.
    release_events_for_owner(adapter, owner);
    // A foreign scanout source of this owner ends first: the desktop gets scanout
    // 0 back whatever the host does with the rest of the teardown.
    adapter.foreign_scanout_release_owner(owner);
    // After the first transport failure the host is not answering: stop sending
    // (a wedged host would cost seconds per handle) but keep clearing the tables,
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
        let Some((handle, _kmd_id, host_id)) = map else {
            break;
        };
        if sending {
            if let Err(VirtioError::Timeout) | Err(VirtioError::DeviceError) =
                release_host_map(passive, adapter, handle, host_id)
            {
                sending = false;
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
    // Last: the host no longer holds an alias of the pinned pages (or is not
    // answering and the VM is going away), so they may be unlocked.
    loop {
        let pin = adapter
            .with_virtio(|v| v.take_nvrm_pin_for_owner(owner))
            .ok()
            .flatten();
        let Some(pin) = pin else {
            break;
        };
        release_pin(pin);
    }
    closed
}

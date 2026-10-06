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
//! * cross-client references: an `Ioctl` whose payload names an RM client or a backend
//!   handle that is not the caller's is refused (`nvrm_harden`, the pure rules in
//!   `helios_kmd_logic::nvrm_clients`; `docs/nvrm-escape.md` section 12). The KMD learns
//!   a process's clients from the reply of its `NV_ESC_RM_ALLOC` of a root class and
//!   forgets them on the free, on `Close` of the file and on teardown.
//!
//! * RM fence handles: a successful forwarded `SEMSURF_FENCE_CREATE` on an owned DRM
//!   node returns a backend handle that is not `Open`ed; `forward_fence_create`
//!   records it as the caller's under `DEVICE_TYPE_FENCE`, so `EVENT_REGISTER`,
//!   `Close` and device teardown treat it like any owned handle (and nothing else
//!   may name it). The rules are in `helios_kmd_logic::nvrm_fence`.
//!
//! * usermode events: `register_event` / `unregister_event` tie a process's
//!   `KEVENT` to a backend handle (or to the loss of the transport); `Close` and
//!   `close_all_for_owner` drop what a handle or a process registered, at PASSIVE
//!   and outside every lock (the DPC side is `virtio::gpu::nvrm_events`).
//!
//! Everything else in an RM message is opaque here.

use super::ctrl;
use super::gpu::{
    release_nvrm_event, DeviceOwner, FenceCommit, NvrmEventRefusal, NvrmPin,
    MAX_NVRM_PINS_PER_OWNER, MAX_NVRM_PIN_PAGES,
};
use super::hal::DmaBuffer;
use super::nvrm_harden as harden;
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::nvrm_clients::Verdict;
use helios_kmd_logic::nvrm_fence;
use helios_kmd_logic::page_runs;
use helios_kmd_logic::sweep_budget::{CloseTally, PinAction, PinFate, SweepBudget};
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
/// How long the KMD's own fence `Close` waits for the host. At most half of what
/// `stop_hpd` gives the worker to exit (5 s), like the RM client's steps.
const FENCE_CLOSE_TIMEOUT_MS: u64 = 2_500;
const MSG_IOCTL: u32 = 3;
const MSG_MMAP: u32 = 4;
const MSG_MUNMAP: u32 = 5;
const MSG_SCANOUT_FLIP: u32 = 20;

/// `NV_ESC_RM_FREE` as the low 16 bits of the Linux ioctl number the guest sends:
/// `('F' << 8) | 0x29`.
const CMD_RM_FREE_LOW16: u32 = 0x4629;

/// Key of an NVRM mapping in `AdapterContext::mappings`, which is shared with
/// blob mappings (keyed by a small KMD-assigned resource id). The KMD-assigned
/// mapping ids stay below `0x7FFF_FFF0`; the high bit keeps the two namespaces
/// apart.
pub fn map_key(kmd_id: u32) -> u32 {
    helios_kmd_logic::nvrm_views::key(kmd_id)
}

/// The next mapping id to mint. ONE counter for the life of the driver, not one
/// per transport: the user views these ids name live in `AdapterContext::mappings`,
/// which outlives every transport, so a generation that restarted at 1 would put
/// its first id on a key an older view of a surviving device handle still holds.
static NVRM_NEXT_MAP_ID: AtomicU32 = AtomicU32::new(1);

/// Mint a mapping id (unique and nonzero for the life of the driver); `None` once
/// the id space is used up.
pub(crate) fn mint_map_id() -> Option<u32> {
    NVRM_NEXT_MAP_ID
        .fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            helios_kmd_logic::nvrm_views::successor,
        )
        .ok()
}

/// The id the next mapping will get: every id below it belongs to the transport
/// generations that are gone once the current one is dropped.
pub(crate) fn next_map_id() -> u32 {
    NVRM_NEXT_MAP_ID.load(Ordering::Relaxed)
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
// The pure fence rules read the same reply.
const _: () = assert!(nvrm_fence::REPLY_DATA == REPLY_DATA);

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
/// Bytes currently mapped through MMAP, all owners (`NvMapMb`, in MiB). Refreshed
/// under the table lock at every change.
pub static NVRM_MAP_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// MMAPs refused by the per-device byte quota (`NvMapQRef`).
pub static NVRM_MAP_QUOTA_REFUSED: AtomicU32 = AtomicU32::new(0);
/// Pins made, pins released, pin failures. `NvPin - NvUnpin` is what is locked
/// now; a count that only grows is a leak. Published as `NvPin`, `NvUnpin`,
/// `NvPinErr`.
/// Forwarded `ScanoutFlip`s (zero-copy presents).
pub static NVRM_FLIPS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_PINS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_UNPINS: AtomicU32 = AtomicU32::new(0);
/// Pins deliberately left locked because the host never confirmed closing what
/// aliased them (`NvPinLeak`). Nonzero means a teardown outran the host.
pub static NVRM_PIN_LEAKS: AtomicU32 = AtomicU32::new(0);
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
/// What a transport found still tracked when it was dropped (handles, mappings and
/// pins nobody had closed: dxgkrnl normally destroys every device first, so this
/// is 0), the user views left behind by it (`NvStale`), and how many of those the
/// owners' next calls unmapped (`NvStaleUn`). Published as `NvSwept`, `NvStale`,
/// `NvStaleUn`.
pub static NVRM_SWEPT: AtomicU32 = AtomicU32::new(0);
pub static NVRM_STALE_VIEWS: AtomicU32 = AtomicU32::new(0);
pub static NVRM_STALE_UNMAPPED: AtomicU32 = AtomicU32::new(0);

/// RM fence handles (`SEMSURF_FENCE_CREATE`): recorded as owned (`NvFence`),
/// released by a `Close` or by device teardown (`NvFenceCl`; `NvFence -
/// NvFenceCl` is what is live now, 0 when no client runs), `EventReady`s seen for
/// one (`NvFenceSig`, one per fire), fires that beat the recording of their
/// handle (`NvFenceEarly`, also counted in `NvFenceSig`), and creates whose reply
/// was unusable or lost a notification (`NvFenceErr`, should read 0).
pub static NVRM_FENCES: AtomicU32 = AtomicU32::new(0);
pub static NVRM_FENCES_CLOSED: AtomicU32 = AtomicU32::new(0);
pub static NVRM_FENCE_FIRED: AtomicU32 = AtomicU32::new(0);
pub static NVRM_FENCE_EARLY: AtomicU32 = AtomicU32::new(0);
pub static NVRM_FENCE_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Set whenever the KMD owes the host a `Close` of a fence handle it took over
/// (`docs/rm-fence-marker.md`); the HPD worker swaps it to 0 and closes them.
pub static FENCE_CLOSE_OWED: AtomicU32 = AtomicU32::new(0);
/// Closes of KMD-owned fence handles the host did not take (`FnCloseErr`).
pub static FENCE_CLOSE_ERRORS: AtomicU32 = AtomicU32::new(0);

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
///
/// `epoch` is set to the device generation as of the first transport lock this
/// call took (an `Ioctl` folds the read into its ownership check, so the hot path
/// pays no lock for it); it stays `None` when the call was refused before any.
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
    epoch: &mut Option<u64>,
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
    if msg != MSG_IOCTL {
        // Open / Close / ScanoutFlip / the file listings are rare next to Ioctl:
        // one extra lock hold, sampled before the message goes out as ever.
        *epoch = Some(adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0));
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
                        if nvrm_fence::is_fence(device_type) {
                            NVRM_FENCES_CLOSED.fetch_add(1, Ordering::Relaxed);
                        }
                        // The handle is closed: its event registrations go (a
                        // wake for a handle nobody holds is dropped by the router
                        // already, so the gap is harmless). A failed Close keeps
                        // them, because the handle stays open.
                        release_events_for_handle(adapter, owner, handle);
                        // The RM clients made through this file went with it.
                        harden::forget_via(adapter, owner, handle);
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
                    if nvrm_fence::is_fence(device_type) {
                        NVRM_FENCES_CLOSED.fetch_add(1, Ordering::Relaxed);
                    }
                    release_events_for_handle(adapter, owner, handle);
                    harden::forget_via(adapter, owner, handle);
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
            // The knob (one atomic load once read; never judged for the KMD's own client).
            let hmode = harden::mode_for(passive, owner);
            // Ownership, the handle's kind, the device generation and (with hardening on)
            // the verdict on every client / handle the request names, in ONE lock hold.
            // Transport down: not owned, generation 0.
            let (device_type, generation, verdict) = adapter
                .with_virtio(|v| {
                    let device_type = v.nvrm_handle_device_type(owner, handle);
                    let verdict = match device_type {
                        Some(t) if hmode != harden::MODE_OFF => v.nvrm_judge(owner, t, req),
                        _ => Verdict::Allow,
                    };
                    (device_type, v.nvrm_epoch(), verdict)
                })
                .unwrap_or((None, 0, Verdict::Allow));
            *epoch = Some(generation);
            let Some(device_type) = device_type else {
                return Err(refused(Refusal::NotOwned));
            };
            // A fence handle takes no message but `Close` (the host answers
            // BadHandle to anything else); say so here and spare the round trip.
            if nvrm_fence::is_fence(device_type) {
                return Err(refused(Refusal::Forbidden));
            }
            if let Err(r) = check_ioctl(req, pin_id != 0) {
                return Err(refused(r));
            }
            // A request that names a client or a file that is not the caller's
            // (`docs/nvrm-escape.md` section 12). After the layout check, so a request
            // the lengths already refuse is refused for that.
            if let Err(r) = harden::apply(hmode, verdict) {
                return Err(refused(r));
            }
            if let (Some(cmd), Some(data_len)) = (rd_u32(req, 16), rd_u32(req, 20)) {
                let features = adapter
                    .with_virtio(|v| v.nvrm_device_features())
                    .unwrap_or(0);
                if nvrm_fence::is_fence_create(cmd, data_len, device_type, features) {
                    // A fence create has no pin to carry: the handle in its reply
                    // would go untracked.
                    if pin_id != 0 {
                        return Err(refused(Refusal::Forbidden));
                    }
                    // The reply must be able to hold the whole data block, or the
                    // host would create the fence, fail to write its handle back
                    // (BufferTooSmall) and leave one the guest can never close:
                    // refuse before anything is reserved or sent.
                    if resp.len() < nvrm_fence::REPLY_DATA + nvrm_fence::FENCE_CREATE_DATA_LEN as usize {
                        return Err(refused(Refusal::BadRange));
                    }
                    return forward_fence_create(passive, adapter, owner, req, resp, timeout_ms);
                }
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
            // A client allocation reserves its slot in the client table first: a full
            // table refuses before the host makes a client nobody tracks.
            let reserved = harden::begin(adapter, owner, hmode, req).map_err(refused)?;
            NVRM_IOCTLS.fetch_add(1, Ordering::Relaxed);
            let n = match ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms) {
                Ok(n) => n,
                Err(e) => {
                    harden::after_failed(
                        adapter,
                        owner,
                        hmode,
                        reserved,
                        req,
                        matches!(e, VirtioError::Timeout),
                    );
                    return Err(Refusal::Transport(e));
                }
            };
            harden::after_reply(adapter, owner, handle, hmode, reserved, req, resp, n);
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
                Some(t) if !nvrm_fence::is_dri_node(t) => return Err(refused(Refusal::Forbidden)),
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

/// A forwarded `SEMSURF_FENCE_CREATE` on an owned DRM node: reserve a slot first
/// (a full table or quota refuses before the host makes a fence), forward it
/// verbatim, and on a clean success record the handle in the reply's `fd` field as
/// the caller's. The reply is returned to the caller either way, as the device
/// wrote it.
///
/// The record and the take of an `EventReady` that beat it are one lock hold
/// (`commit_nvrm_fence`), and the notification keeper is armed before the host
/// sees the request (`begin_nvrm_fence_create`), so a fence that fires at once is
/// not lost: its first `EVENT_REGISTER` answers `LATCHED_SIGNALED`.
///
/// A timed-out create may still have made a fence on the host that this table
/// does not know; it is bounded by the host's own limit and its 5 s timeout, as a
/// timed-out `Open` is bounded by quota.
fn forward_fence_create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    req: &[u8],
    resp: &mut [u8],
    timeout_ms: u64,
) -> Result<usize, Refusal> {
    let begun = adapter
        .with_virtio(|v| v.begin_nvrm_fence_create(owner))
        .map_err(|_| Refusal::Transport(VirtioError::DeviceError))?;
    if !begun {
        NVRM_REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(Refusal::NoResources);
    }
    NVRM_IOCTLS.fetch_add(1, Ordering::Relaxed);
    // The process the creating device belongs to, recorded with the fence: a WDDM
    // carrier (`docs/rm-fence-marker.md`) checks it against the presenting
    // context's process, because the presenting device is not this one. Read now,
    // while this escape keeps `owner`'s device alive; 0 (never matches) if the
    // handle cannot be resolved.
    // `KMD_RM` is the KMD's own RM client, a token that is not a device handle
    // (`usize::MAX`): never resolve it. Unreachable today (no escape names it, and
    // the RM client does not forward a fence create), and guarded so a future caller
    // cannot make it a wild dereference.
    // SAFETY: `owner` is the `hDevice` of the escape being served, which dxgkrnl
    // keeps alive until the escape returns.
    let process = if owner == DeviceOwner::KMD_RM {
        0
    } else {
        unsafe { crate::device::DeviceHandleRef::from_raw(owner.raw() as *mut core::ffi::c_void) }
            .map_or(0, |d| d.creator_process())
    };
    match ctrl::raw_roundtrip(passive, adapter, req, resp, timeout_ms) {
        Ok(n) => {
            let handle = nvrm_fence::fence_handle_from_reply(resp, n);
            // A host status of 0 with no handle to own is a fence we cannot see.
            let host_ok = n >= MSG_HDR && rd_i32(resp, 8) == Some(0);
            let committed = adapter.with_virtio(|v| match handle {
                Some(h) => Some(v.commit_nvrm_fence(owner, h, process)),
                None => {
                    v.cancel_nvrm_fence_create();
                    None
                }
            });
            match committed {
                Ok(Some(FenceCommit::Recorded { fired })) => {
                    NVRM_FENCES.fetch_add(1, Ordering::Relaxed);
                    if fired {
                        // Its notification was taken before it had an owner (and
                        // so was not counted as a fire of a known fence).
                        NVRM_FENCE_FIRED.fetch_add(1, Ordering::Relaxed);
                        NVRM_FENCE_EARLY.fetch_add(1, Ordering::Relaxed);
                        NVRM_EV_LATCHED.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Some(FenceCommit::Duplicate)) => {
                    NVRM_FENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) if host_ok => {
                    NVRM_FENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
                // A host refusal (no fence was made), or the transport is gone.
                _ => {}
            }
            Ok(n)
        }
        Err(e) => {
            let _ = adapter.with_virtio(|v| v.cancel_nvrm_fence_create());
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
/// contiguous table buffer cannot be freed above PASSIVE. The unlock itself is
/// `NvrmPin`'s `Drop`, so a pin that goes away by ANY path (this one, a transport
/// dropped with pins in its table) is unlocked exactly once; this names the
/// intent at the call sites that hand a pin back by value.
pub fn release_pin(pin: NvrmPin) {
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
    host_mmap_within(
        passive,
        adapter,
        owner,
        handle,
        write,
        offset,
        size,
        HOST_MMAP_TIMEOUT_MS,
    )
}

/// How long the host gets to answer an `Mmap` for a user-mode caller.
const HOST_MMAP_TIMEOUT_MS: u64 = 30_000;

/// [`host_mmap`] with the caller's bound on the host's answer, for a caller that runs on
/// a thread StopDevice joins (the KMD's own RM client, on the HPD worker). A timed-out
/// `Mmap` may still have been served host-side: the mapping id is lost with the reply,
/// and the host's own sweep (the file's `Close`, or the transport's) reclaims it.
#[allow(clippy::too_many_arguments)]
pub fn host_mmap_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    write: bool,
    offset: u64,
    size: u64,
    timeout_ms: u64,
) -> Result<HostMapping, MapRefusal> {
    let Some(device_type) = adapter
        .with_virtio(|v| v.nvrm_handle_device_type(owner, handle))
        .ok()
        .flatten()
    else {
        return Err(MapRefusal::NotOwned);
    };
    // A fence is a host sync_file, not a mappable file (the host would refuse).
    if nvrm_fence::is_fence(device_type) {
        return Err(MapRefusal::NotOwned);
    }
    if size == 0 || size % PAGE != 0 || size > MAX_MAP_BYTES || offset % PAGE != 0 {
        return Err(MapRefusal::BadRange);
    }
    let quota_ok = adapter
        .with_virtio(|v| v.nvrm_map_count(owner) < super::gpu::MAX_NVRM_MAPS_PER_OWNER)
        .unwrap_or(false);
    if !quota_ok {
        return Err(MapRefusal::NoResources);
    }
    // The byte quota (a quarter of the RM window per device), before the host is
    // asked to map anything. The UVM aperture is exempt.
    let bytes_ok = adapter
        .with_virtio(|v| v.nvrm_map_bytes_room(owner, device_type == 256, size))
        .unwrap_or(false);
    if !bytes_ok {
        NVRM_MAP_QUOTA_REFUSED.fetch_add(1, Ordering::Relaxed);
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
    let n = ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, timeout_ms)
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
    host_munmap_within(passive, adapter, handle, host_id, 5_000)
}

/// [`host_munmap`] waiting at most `timeout_ms`.
fn host_munmap_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    handle: u32,
    host_id: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    // MsgHeader{Munmap, handle} | MunmapReq { mapping_id u32, pad u32 }.
    let mut req = [0u8; MSG_HDR + 8];
    req[..4].copy_from_slice(&MSG_MUNMAP.to_le_bytes());
    req[4..8].copy_from_slice(&handle.to_le_bytes());
    req[16..20].copy_from_slice(&host_id.to_le_bytes());
    let mut resp = [0u8; MSG_HDR];
    ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, timeout_ms).map(|_| ())
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
    release_host_map_within(passive, adapter, handle, host_id, 5_000)
}

/// [`release_host_map`] waiting at most `timeout_ms` for the host.
pub fn release_host_map_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    handle: u32,
    host_id: u32,
    timeout_ms: u64,
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
    host_munmap_within(passive, adapter, handle, host_id, timeout_ms)
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

/// Unmap the user views `owner` still holds of transports that no longer exist.
///
/// The views live in `AdapterContext::mappings`, which survives `StopDevice`, and a
/// view can only be unmapped inside the process that made it, which `StopDevice`
/// is not. So `StopDevice` marks them stale (`MappingTable::mark_nvrm_views_stale`)
/// and the OWNER's next call into the KMD comes here, in its own process: the
/// host mapping behind each view died with the old transport, and a later
/// generation may hand the same BAR window offsets to another process, so the view
/// must not stay readable. A process that touches one afterwards takes an access
/// violation, which is the honest outcome of a mapping that no longer exists.
///
/// PASSIVE, in the owning process (an escape from its device handle). One atomic
/// load when nothing is stale.
pub fn reclaim_stale_views(_passive: PassiveLevel, adapter: &AdapterContext, owner: DeviceOwner) {
    const BATCH: usize = 16;
    let mut batch = [(0u64, 0usize); BATCH];
    loop {
        let n = adapter
            .mappings
            .drain_stale_nvrm_for(owner.raw(), &mut batch);
        for &(va, mdl) in batch.iter().take(n) {
            // SAFETY: PASSIVE, in the process that mapped it; the pair came from
            // `map_io_pages_to_user_prot` and was removed from the table just now.
            unsafe { crate::ddi::unmap_io_pages_from_user(va, mdl as *mut wdk_sys::MDL) };
            NVRM_STALE_UNMAPPED.fetch_add(1, Ordering::Relaxed);
        }
        if n < BATCH {
            break;
        }
    }
}

/// Close, on the host, the fence handles the KMD took over and now owes a `Close`
/// (they fired, or their present was dropped). Take-then-send, one at a time, as a
/// user `Close` does: the host may hand the number to someone else the moment it
/// closes it. PASSIVE. A transport that has failed is not asked: the sweep that
/// retires it closes every handle.
///
/// Bounded, because the HPD worker runs it and `stop_hpd` joins the worker for 5 s
/// (twice): each call waits at most [`FENCE_CLOSE_TIMEOUT_MS`] (the KMD's own RM
/// client caps its steps at the same 2.5 s for this reason), and the loop ends at
/// the first timeout (the host is not answering; each further handle would cost
/// another full wait) and as soon as `hpd_stop` is set (the sweep of the transport
/// retires what is left). What is left stays owed, and the next call goes on.
pub fn close_owed_fences(passive: PassiveLevel, adapter: &AdapterContext) {
    if FENCE_CLOSE_OWED.swap(0, Ordering::AcqRel) == 0 {
        return;
    }
    let alive = adapter
        .with_virtio(|v| !v.transport_failed())
        .unwrap_or(false);
    if !alive {
        return;
    }
    loop {
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            rearm_fence_close_debt(adapter);
            break;
        }
        let taken = adapter
            .with_virtio(|v| v.take_fence_to_close())
            .ok()
            .flatten();
        let Some(handle) = taken else {
            break;
        };
        NVRM_FENCES_CLOSED.fetch_add(1, Ordering::Relaxed);
        let mut req = [0u8; MSG_HDR];
        req[..4].copy_from_slice(&MSG_CLOSE.to_le_bytes());
        req[4..8].copy_from_slice(&handle.to_le_bytes());
        let mut resp = [0u8; MSG_HDR];
        match ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, FENCE_CLOSE_TIMEOUT_MS) {
            Ok(n) if n >= MSG_HDR && rd_i32(&resp, 8) == Some(0) => {}
            // The host answered no (it no longer knows the number): nothing to keep.
            Ok(_) => {
                FENCE_CLOSE_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            // Indeterminate (it may have closed): stay forgotten, as `Close` does.
            // The host is not answering in time, so stop here.
            Err(VirtioError::Timeout) => {
                FENCE_CLOSE_ERRORS.fetch_add(1, Ordering::Relaxed);
                rearm_fence_close_debt(adapter);
                break;
            }
            // Never reached the host: still the KMD's, for the transport sweep.
            Err(_) => {
                FENCE_CLOSE_ERRORS.fetch_add(1, Ordering::Relaxed);
                let _ = adapter.with_virtio(|v| v.restore_fence_after_failed_close(handle));
            }
        }
    }
}

/// A `close_owed_fences` that stopped early leaves what it did not reach owed:
/// raise the flag again so the next call (the worker's next pass, or an escape's)
/// goes on. The flag was taken by that call's `swap(0)`.
fn rearm_fence_close_debt(adapter: &AdapterContext) {
    if adapter
        .with_virtio(|v| v.fences_owing_close() != 0)
        .unwrap_or(false)
    {
        FENCE_CLOSE_OWED.store(1, Ordering::Release);
    }
}

/// The KMD took fence `handle` over for a present: drop what its creator had
/// registered on it (`EVENT_REGISTER`, any owner). A user `Close` does this for
/// the handle it closes (`release_events_for_handle`); the KMD's own close does
/// not, so without this the registrations, and the event references they hold,
/// would outlive the handle until the creator's device is destroyed. After the
/// attach no one can register again (the handle is no longer theirs). PASSIVE.
pub fn release_events_of_taken_fence(adapter: &AdapterContext, handle: u32) {
    loop {
        let event = adapter
            .with_virtio(|v| v.take_nvrm_event_for_handle_any(handle))
            .ok()
            .flatten();
        let Some(event) = event else {
            break;
        };
        release_nvrm_event(event);
    }
}

/// With no HPD worker (render-only `DisplayHalf=0`, or its creation failed) nothing
/// else would send the fenced flips that fired or close the fence handles the KMD
/// owes the host, so the callers that are PASSIVE anyway do it on their own thread:
/// every NVRM escape, and the `Render` that takes a fence. One atomic load (and a
/// short lock when there is a queue) when nothing is owed. A no-op with a worker.
pub fn service_fences_without_worker(passive: PassiveLevel, adapter: &AdapterContext) {
    if !adapter.hpd_running() {
        adapter.foreign_fence_service(passive);
    }
}

/// Device teardown: release, on the host, everything `owner` left behind — its
/// mappings, then its handles — and unlock its pins. Returns how many handles
/// were closed. A close that fails is dropped — the device may be going away, and
/// the table entry is already gone, so nothing is retried.
///
/// Idempotent against the transport's own teardown (`VirtioGpu::drop`, which sweeps
/// the same tables when `StopDevice` gets there first): both take entries out of
/// the one set of tables under the virtio lock, so whichever runs first releases
/// them and the other finds nothing. After the transport is gone `with_virtio`
/// fails and this does nothing at all.
pub fn close_all_for_owner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
) -> u32 {
    let mut closed = 0u32;
    // Its RM clients: whatever the host does with them below, none may be named again
    // (and the host may mint the numbers anew).
    harden::forget_owner(adapter, owner);
    // Events first: nothing of this owner's may be signalled from here on, and the
    // references are PASSIVE-only to drop.
    release_events_for_owner(adapter, owner);
    // A foreign scanout source of this owner ends first: the desktop gets scanout
    // 0 back whatever the host does with the rest of the teardown.
    adapter.foreign_scanout_release_owner(owner);
    // After the first transport failure the host is not answering: stop sending
    // (a wedged host would cost seconds per handle) but keep clearing the tables,
    // so no entry outlives the device handle it names. A transport that has
    // ALREADY failed is not asked at all.
    let mut sending = adapter
        .with_virtio(|v| !v.transport_failed())
        .unwrap_or(false);
    // Which of the entries taken below the host actually confirmed closed; the pins
    // are unlocked only if it was all of them (see `release_or_leak_pin`).
    let mut tally = CloseTally::new();
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
            match release_host_map(passive, adapter, handle, host_id) {
                Ok(()) => tally.confirmed(),
                Err(VirtioError::Timeout) | Err(VirtioError::DeviceError) => {
                    sending = false;
                    tally.failed();
                }
                Err(_) => tally.failed(),
            }
        } else {
            tally.unsent();
        }
    }
    loop {
        let taken = adapter
            .with_virtio(|v| v.take_nvrm_handle_for_owner(owner))
            .ok()
            .flatten();
        let Some((handle, device_type)) = taken else {
            break;
        };
        if nvrm_fence::is_fence(device_type) {
            NVRM_FENCES_CLOSED.fetch_add(1, Ordering::Relaxed);
        }
        if sending {
            let mut req = [0u8; MSG_HDR];
            req[..4].copy_from_slice(&MSG_CLOSE.to_le_bytes());
            req[4..8].copy_from_slice(&handle.to_le_bytes());
            let mut resp = [0u8; MSG_HDR];
            match ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, 5_000) {
                Ok(_) => tally.confirmed(),
                Err(VirtioError::Timeout) | Err(VirtioError::DeviceError) => {
                    sending = false;
                    tally.failed();
                }
                Err(_) => tally.failed(),
            }
        } else {
            tally.unsent();
        }
        closed += 1;
    }
    // Last: every handle the host held an alias through was closed (or it was not
    // and the pages stay locked): see `release_or_leak_pin`.
    let fate = tally.pin_fate();
    loop {
        let pin = adapter
            .with_virtio(|v| v.take_nvrm_pin_for_owner(owner))
            .ok()
            .flatten();
        let Some(pin) = pin else {
            break;
        };
        release_or_leak_pin(pin, fate);
    }
    closed
}

/// Hand a pin back according to `fate`: unlock it, or — when the host never
/// confirmed closing what aliased it — keep it locked for good.
///
/// Only a pin a `FORWARD` claimed (`host_may_alias`) can be aliased by the host;
/// an unclaimed one is unlocked either way. A leaked pin costs its locked pages,
/// its MDL, its table buffer and its owner's EPROCESS reference until the next boot,
/// and is counted (`NvPinLeak`). Unlocking pages the GPU may still write is not a
/// leak but a corruption: the guest would reuse that RAM underneath the host. The
/// pin is removed from the transport's table first (the caller took it), so
/// `VirtioGpu::drop`'s fallback sweep cannot unlock it either.
///
/// Locked user pages bugcheck the owning process when its address space goes
/// (0x76 `PROCESS_HAS_LOCKED_PAGES`). The leaked pin's process reference
/// (`NvrmPin::leak`) is meant to keep the process object, and with it that check,
/// from running; that is UNVERIFIED (see `NvrmPin`). If it does not hold, the
/// price of a host that never confirmed its closes is a bugcheck at the owner's
/// exit rather than DMA into reused RAM.
fn release_or_leak_pin(pin: NvrmPin, fate: PinFate) {
    match fate.action(pin.host_may_alias()) {
        PinAction::Leak => pin.leak(),
        // Confirmed closed, or never described to the host (no FORWARD claimed it).
        PinAction::Unlock => release_pin(pin),
    }
}

/// Retire the live transport's NVRM state while it can still be asked: send the
/// host a `Munmap` for every mapping and a `Close` for every handle ANY owner left
/// open, then unlock the pins.
///
/// The order is the point. The host does not drop its RM files when the guest
/// resets the device (QEMU's generic vhost-user device never sends
/// `RESET_DEVICE`; the backend resets at its next feature negotiation, i.e. the
/// next `StartDevice`), and a registered OS descriptor makes the GPU alias the
/// guest pages until its file is closed. `VirtioGpu::drop`'s sweep can only unlock;
/// unlocking pages the host still holds lets the guest reuse memory the GPU may
/// still write. So the host is told FIRST, as `close_all_for_owner` does per owner.
///
/// Best effort and bounded: it stops sending after the first timeout or failed
/// send (a wedged host costs seconds per call) or when `budget` is spent (each
/// `Close`/`Munmap` waits at most the budget's per-call allowance, never more than
/// what is left of it), but keeps clearing the tables, so nothing is left for the fallback in
/// `VirtioGpu::drop` except what a concurrent call re-populated. A transport that
/// has already failed (or is absent) is not asked at all and nothing is touched
/// here: the fallback handles it.
///
/// Event registrations stay: `VirtioGpu::drop` wakes them (their owners must see
/// the loss) and releases them. The user views of the mappings cannot be unmapped
/// from here (see `reclaim_stale_views`).
///
/// PASSIVE, no lock held (each table access is its own short `with_virtio`; every
/// wire call and every pin unlock is outside it). Returns how many handles were
/// closed or dropped.
pub fn close_all_on_host(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    budget: &SweepBudget,
) -> u32 {
    // The KMD's own RM client keeps a kernel view of an RM mapping: it must be
    // unmapped BEFORE the host closes the file that holds the mapping (and whether
    // or not the host is still being asked). Its handles are in the tables below
    // under `DeviceOwner::KMD_RM`, so the sweep closes them like any other owner's.
    // Idempotent: StopDevice sweeps explicitly and again through `retire_transport`.
    super::rm_client::retire_begin(passive);
    let alive = adapter
        .with_virtio(|v| !v.transport_failed())
        .unwrap_or(false);
    if !alive {
        return 0;
    }
    let mut sending = true;
    // What the host confirmed closed. Entries dropped from the tables unsent (the
    // budget spent, or an earlier command failed) and failed sends both count
    // against `all_closed`, and then the pins are NOT unlocked: the host still
    // holds those handles, and the doc below says it does not drop them on reset.
    let mut tally = CloseTally::new();
    // Asked before each send: how long the next command may wait, or `None` when
    // a timeout or error already ended sending or the budget is spent. A `Close`
    // may wait longer than the other commands (`close_timeout_ms`): freeing a
    // device handle's VRAM is slow, and an unconfirmed Close costs the pins.
    let next_timeout_ms = |sending: &mut bool, close: bool| -> Option<u64> {
        if !*sending {
            return None;
        }
        let now = crate::adapter::foreign_scanout::now_100ns();
        let timeout = if close {
            budget.close_timeout_ms(now)
        } else {
            budget.call_timeout_ms(now)
        };
        if timeout.is_none() {
            *sending = false;
        }
        timeout
    };
    // Mappings first (the ABI's order: unmap, then close).
    loop {
        let map = adapter
            .with_virtio(|v| v.take_nvrm_map_any())
            .ok()
            .flatten();
        let Some((handle, host_id)) = map else {
            break;
        };
        if let Some(timeout_ms) = next_timeout_ms(&mut sending, false) {
            if release_host_map_within(passive, adapter, handle, host_id, timeout_ms).is_err() {
                sending = false;
                tally.failed();
            } else {
                tally.confirmed();
            }
        } else {
            tally.unsent();
        }
    }
    let mut closed = 0u32;
    loop {
        let taken = adapter
            .with_virtio(|v| v.take_nvrm_handle_any())
            .ok()
            .flatten();
        let Some((owner, handle, device_type)) = taken else {
            break;
        };
        if nvrm_fence::is_fence(device_type) {
            NVRM_FENCES_CLOSED.fetch_add(1, Ordering::Relaxed);
        }
        // A foreign scanout source on this file ends with it (a no-op after
        // `StopDevice` already reset the display state).
        adapter.foreign_scanout_release_handle(owner, handle);
        if let Some(timeout_ms) = next_timeout_ms(&mut sending, true) {
            let mut req = [0u8; MSG_HDR];
            req[..4].copy_from_slice(&MSG_CLOSE.to_le_bytes());
            req[4..8].copy_from_slice(&handle.to_le_bytes());
            let mut resp = [0u8; MSG_HDR];
            if ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, timeout_ms).is_err() {
                sending = false;
                tally.failed();
            } else {
                tally.confirmed();
            }
        } else {
            tally.unsent();
        }
        closed += 1;
    }
    // Every file is closed (or forgotten), and the host frees a client with its file.
    harden::forget_all(adapter);
    // Last: the pins. Unlocked only if the host confirmed every close above; if
    // sending stopped early they stay locked (leaked, `NvPinLeak`), because the
    // host keeps its RM files across a device reset and the GPU may still write
    // pages it registered. Leaked locked pages are safe; DMA into reused RAM is not.
    let fate = tally.pin_fate();
    loop {
        let pin = adapter
            .with_virtio(|v| v.take_nvrm_pin_any())
            .ok()
            .flatten();
        let Some(pin) = pin else {
            break;
        };
        release_or_leak_pin(pin, fate);
    }
    closed
}

/// Drop the live transport (if there is one) the safe way: tell the host to let go
/// of everything first ([`close_all_on_host`]), then `set_virtio(None)` (whose
/// `VirtioGpu::drop` resets the device and runs the fallback sweep), then mark the
/// user views of the dropped transport stale. The one place every path that
/// replaces or ends a live transport goes through: `StopDevice`, and `StartDevice`
/// when no stop came before it.
///
/// Returns whether a transport was dropped. PASSIVE.
pub fn retire_transport(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    budget: &SweepBudget,
) -> bool {
    let had = adapter.with_virtio(|_| ()).is_ok();
    if had {
        close_all_on_host(passive, adapter, budget);
    }
    adapter.set_virtio(None);
    // Its handles were closed by the sweep (or died with the transport): forget them.
    super::rm_client::forget();
    // The mapping table died with the transport; the gauge is only refreshed by a
    // table change, so without this it kept the last total until the next push.
    NVRM_MAP_BYTES.store(0, Ordering::Relaxed);
    if had {
        mark_views_stale(adapter);
    }
    had
}

/// The transport that made every NVRM view below the next id is gone: mark them
/// stale, so each owner's next NVRM call unmaps its own (`reclaim_stale_views`).
/// After the drop, so no older id can be minted any more; an `MMAP` caught between
/// its id mint and its view insert is marked by the insert itself
/// (`MappingTable::insert_unique`). Returns how many views were marked.
fn mark_views_stale(adapter: &AdapterContext) -> u32 {
    let stale = adapter.mappings.mark_nvrm_views_stale(next_map_id());
    NVRM_STALE_VIEWS.fetch_add(stale, Ordering::Relaxed);
    stale
}

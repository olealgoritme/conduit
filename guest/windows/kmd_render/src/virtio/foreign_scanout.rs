//! `SCANOUT_PRESENT`: the KMD builds the host `ScanoutFlip` (msg 20) itself from a
//! source it validated at `SCANOUT_SET`, so a caller supplies only which GEM
//! object to show. Contract: `helios_protocol::nvrm_scanout`; state and desktop
//! suppression: `adapter/foreign_scanout.rs`.
//!
//! What the KMD checks that a forwarded `ScanoutFlip` leaves to the host: the
//! layout (once, at SET), that the caller holds the live source and that its file
//! is still a DRM node it owns in this transport generation, and the sequence
//! number, which it mints. The GEM handle is the host's to check against the DRM
//! file, as for a forwarded flip.

use super::ctrl;
use super::hal::MSG_HDR_LEN;
use super::scanout_release;
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::virtio::gpu::DeviceOwner;
use helios_kmd_logic::foreign_scanout::{Flip, PresentError};
use helios_kmd_logic::nvrm_fence::is_fence;
use helios_protocol::HELIOS_NVRM_SCANOUT_FLIP_BYTES;

/// Host `MsgType::ScanoutFlip`.
const MSG_SCANOUT_FLIP: u32 = 20;
/// `device_type` from which an `Open` names a DRM node.
const DEVICE_TYPE_DRI_FIRST: u32 = 512;
/// How long the host gets to take one flip. A flip is a header-only reply; the
/// backend acks it without waiting for the viewer.
const FLIP_TIMEOUT_MS: u64 = 5_000;
/// The same for a flip sent from the queue (`send_queued`), which runs on the HPD
/// worker as often as on an escape: at most half of what `stop_hpd` gives the worker
/// to exit (5 s, twice), like the RM client's steps and the fence `Close`.
const QUEUED_FLIP_TIMEOUT_MS: u64 = 2_500;

/// Why a present did not reach the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentRefusal {
    /// No transport.
    NoTransport,
    /// `handle` is not a backend handle of the caller.
    NotOwned,
    /// `handle` is the caller's but not a DRM node (cannot be: `SET` refuses it;
    /// the file was reopened as something else).
    Forbidden,
    /// No live source of the caller's on that handle (also: lapsed, or the
    /// transport generation changed).
    NoSource,
    /// The host or the transport did not take the flip.
    Device(VirtioError),
    /// A fenced present, and this KMD / host does not serve fences.
    Unsupported,
    /// The fence handle is the caller's but not a fence.
    NotFence,
    /// The fence handle is already attached to a present.
    AlreadyAttached,
    /// `HELIOS_NVRM_SCANOUT_FENCE_DEPTH` presents already wait.
    QueueFull,
}

fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Build and send one `ScanoutFlip` and wait for the host's header-only answer.
/// PASSIVE: a control-queue round trip.
fn send(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    flip: &Flip,
    gem: u32,
    timeout_ms: u64,
) -> Result<(), VirtioError> {
    // MsgHeader { msg_type, handle = 0, status = 0, padding = 0 } | ScanoutFlip.
    let mut req = [0u8; MSG_HDR_LEN + HELIOS_NVRM_SCANOUT_FLIP_BYTES];
    wr32(&mut req, 0, MSG_SCANOUT_FLIP);
    let p = MSG_HDR_LEN;
    wr32(&mut req, p, 0); // scanout
    wr32(&mut req, p + 4, flip.handle); // owner_handle
    wr32(&mut req, p + 8, gem); // host_handle
    wr32(&mut req, p + 12, flip.layout.width);
    wr32(&mut req, p + 16, flip.layout.height);
    wr32(&mut req, p + 20, flip.layout.stride);
    wr32(&mut req, p + 24, flip.layout.offset);
    wr32(&mut req, p + 28, flip.layout.fourcc);
    wr64(&mut req, p + 32, flip.layout.modifier);
    wr64(&mut req, p + 40, flip.seq);
    // reserved[4] at p + 48 stays zero.

    let mut resp = [0u8; 2 * MSG_HDR_LEN];
    let sent = match ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, timeout_ms) {
        // MsgHeader.status (offset 8) is a signed errno; 0 is success.
        Ok(n) if n >= MSG_HDR_LEN => {
            let status = i32::from_le_bytes([resp[8], resp[9], resp[10], resp[11]]);
            if status == 0 {
                Ok(())
            } else {
                Err(VirtioError::DeviceError)
            }
        }
        Ok(_) => Err(VirtioError::DeviceError),
        Err(e) => Err(e),
    };
    note_flip_sent(adapter, flip.seq, &sent);
    sent
}

/// Tell the release book what became of flip `seq`: the host took it (or, on a TIMEOUT,
/// may yet: it is assumed to have, which keeps the image reserved), or it never will. A
/// no-op when the host's releases are not tracked. Wakes whoever waits when that moved
/// the floor.
fn note_flip_sent(adapter: &AdapterContext, seq: u64, sent: &Result<(), VirtioError>) {
    let owner = match sent {
        Ok(()) | Err(VirtioError::Timeout) => {
            scanout_release::sent(seq, crate::adapter::foreign_scanout::now_100ns())
        }
        Err(_) => scanout_release::gone(seq),
    };
    if let Some(owner) = owner {
        scanout_release::wake(adapter, owner);
    }
}

/// Send a flip that was queued behind a fence (the pump's), and do the same
/// bookkeeping a direct present does. A failure is counted (`FsErr`); the caller of
/// the original `PRESENT` is long gone and the frame is lost.
///
/// Returns `false` when the host did not answer in time: the caller stops sending
/// for this pass (every further flip would cost another full wait, on a worker that
/// `stop_hpd` joins).
pub fn send_queued(passive: PassiveLevel, adapter: &AdapterContext, flip: Flip, gem: u32) -> bool {
    let sent = send(passive, adapter, &flip, gem, QUEUED_FLIP_TIMEOUT_MS);
    adapter.foreign_scanout_flip_done(flip.generation, sent.is_ok());
    !matches!(sent, Err(VirtioError::Timeout))
}

/// Ownership of the source handle and the minted flip, in a transport generation:
/// the first half of every `PRESENT`.
fn mint(
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    served_needed: bool,
) -> Result<Flip, PresentRefusal> {
    // Ownership first, in this transport generation: a lapsed or foreign handle
    // must not mint a sequence number.
    let (epoch, device_type, served) = adapter
        .with_virtio(|v| {
            (
                v.nvrm_epoch(),
                v.nvrm_handle_device_type(owner, handle),
                v.rm_fence_served(),
            )
        })
        .map_err(|_| PresentRefusal::NoTransport)?;
    if served_needed && !served {
        return Err(PresentRefusal::Unsupported);
    }
    match device_type {
        None => return Err(PresentRefusal::NotOwned),
        Some(t) if t < DEVICE_TYPE_DRI_FIRST => return Err(PresentRefusal::Forbidden),
        Some(_) => {}
    }
    let flip = adapter
        .foreign_scanout_mint_flip(owner, handle)
        .map_err(|_: PresentError| PresentRefusal::NoSource)?;
    if flip.epoch != epoch {
        // The source was registered in an earlier transport generation.
        return Err(PresentRefusal::NoSource);
    }
    Ok(flip)
}

/// Show GEM object `gem` of the caller's DRM file `handle` on scanout 0. Returns
/// the `seq` the flip carried.
///
/// Normally the flip is sent before this returns. If fenced presents are waiting
/// (or being sent), this one queues behind them as an already-ready entry instead,
/// so flips never go out of order, and the call returns at once.
pub fn present(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    gem: u32,
) -> Result<u64, PresentRefusal> {
    let flip = mint(adapter, owner, handle, false)?;
    scanout_release::minted(flip.seq, owner.raw(), flip.handle, gem);
    if adapter.foreign_fence_queue_busy() {
        if let Err(e) = adapter.foreign_fence_enqueue(owner, flip, gem, 0) {
            scanout_release::gone(flip.seq);
            return Err(queue_refusal(e));
        }
        adapter.foreign_fence_pump(passive);
        return Ok(flip.seq);
    }
    let sent = send(passive, adapter, &flip, gem, FLIP_TIMEOUT_MS);
    adapter.foreign_scanout_flip_done(flip.generation, sent.is_ok());
    match sent {
        Ok(()) => Ok(flip.seq),
        Err(e) => Err(PresentRefusal::Device(e)),
    }
}

/// Show GEM object `gem` of the KMD's own DRM file `handle` (`owner` =
/// `DeviceOwner::KMD_RM`): the RM client's presenter (`rm_present.rs`), which runs on
/// the HPD worker once per frame. Same ownership and mint as [`present`], with the
/// caller's bound on how long the host gets to take the flip (a worker that
/// `stop_hpd` joins must not wait the full [`FLIP_TIMEOUT_MS`]), and two differences:
///
/// * it NEVER queues behind fenced presents. A resident source is foreground only when
///   no user source is, and the queue holds only user sources' entries (the pump drops
///   any whose generation is not the live one, closing their fences), so it cannot hold
///   anything this flip must stay behind. A flip of the resident source is always the
///   screen's newest; the pump's one late flip of a source that has just ended is
///   answered by [`AdapterContext::foreign_scanout_flip_done`] (see the state machine
///   in `docs/kmd-rm-client.md`, section 13.12);
/// * the answer for a source that is no longer the live one is `NoSource` (the
///   presenter reads it as "yielded", not as a failure).
pub fn present_within(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    gem: u32,
    timeout_ms: u64,
) -> Result<u64, PresentRefusal> {
    let flip = mint(adapter, owner, handle, false)?;
    scanout_release::minted(flip.seq, owner.raw(), flip.handle, gem);
    let sent = send(passive, adapter, &flip, gem, timeout_ms);
    adapter.foreign_scanout_flip_done(flip.generation, sent.is_ok());
    match sent {
        Ok(()) => Ok(flip.seq),
        Err(e) => Err(PresentRefusal::Device(e)),
    }
}

/// `SCANOUT_PRESENT` with `RM_FENCE`: queue the flip behind `fence`, a fence of the
/// caller's that the KMD takes over, and return at once with the `seq`. The flip is
/// sent when the fence fires (or now, if it already had and nothing waits ahead).
pub fn present_fenced(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    gem: u32,
    fence: u32,
) -> Result<u64, PresentRefusal> {
    // Cheap refusals first, before a sequence number is minted and the lapse is
    // pushed out: the capability, the source, and the fence's kind and owner.
    let fence_type = adapter
        .with_virtio(|v| v.nvrm_handle_device_type(owner, fence))
        .map_err(|_| PresentRefusal::NoTransport)?;
    match fence_type {
        None => return Err(PresentRefusal::NotOwned),
        Some(t) if !is_fence(t) => return Err(PresentRefusal::NotFence),
        Some(_) => {}
    }
    let flip = mint(adapter, owner, handle, true)?;
    scanout_release::minted(flip.seq, owner.raw(), flip.handle, gem);
    if let Err(e) = adapter.foreign_fence_enqueue(owner, flip, gem, fence) {
        // Refused: it never reaches the host, so nothing is waited for.
        scanout_release::gone(flip.seq);
        return Err(queue_refusal(e));
    }
    // Already fired and nothing ahead: this sends it before returning.
    adapter.foreign_fence_pump(passive);
    Ok(flip.seq)
}

fn queue_refusal(e: crate::adapter::foreign_scanout::EnqueueRefusal) -> PresentRefusal {
    use crate::adapter::foreign_scanout::EnqueueRefusal as E;
    match e {
        E::NoTransport => PresentRefusal::NoTransport,
        E::Full => PresentRefusal::QueueFull,
        E::NotOwned => PresentRefusal::NotOwned,
        E::NotFence => PresentRefusal::NotFence,
        E::AlreadyAttached => PresentRefusal::AlreadyAttached,
    }
}

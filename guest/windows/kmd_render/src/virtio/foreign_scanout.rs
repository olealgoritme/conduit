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
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::virtio::gpu::DeviceOwner;
use helios_kmd_logic::foreign_scanout::PresentError;
use helios_protocol::HELIOS_NVRM_SCANOUT_FLIP_BYTES;

/// Host `MsgType::ScanoutFlip`.
const MSG_SCANOUT_FLIP: u32 = 20;
/// `device_type` from which an `Open` names a DRM node.
const DEVICE_TYPE_DRI_FIRST: u32 = 512;
/// How long the host gets to take one flip. A flip is a header-only reply; the
/// backend acks it without waiting for the viewer.
const FLIP_TIMEOUT_MS: u64 = 5_000;

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
}

fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Show GEM object `gem` of the caller's DRM file `handle` on scanout 0. Returns
/// the `seq` the flip carried.
pub fn present(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    gem: u32,
) -> Result<u64, PresentRefusal> {
    // Ownership first, in this transport generation: a lapsed or foreign handle
    // must not mint a sequence number.
    let (epoch, device_type) = adapter
        .with_virtio(|v| (v.nvrm_epoch(), v.nvrm_handle_device_type(owner, handle)))
        .map_err(|_| PresentRefusal::NoTransport)?;
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
    let sent = match ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, FLIP_TIMEOUT_MS) {
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
    adapter.foreign_scanout_flip_done(flip.generation, sent.is_ok());
    match sent {
        Ok(()) => Ok(flip.seq),
        Err(e) => Err(PresentRefusal::Device(e)),
    }
}

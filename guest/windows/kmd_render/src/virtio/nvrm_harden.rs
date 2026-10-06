//! Cross-client hardening of forwarded RM ioctls: the glue between
//! `FORWARD` (`virtio/nvrm.rs`) and the pure rules in
//! `helios_kmd_logic::nvrm_clients`. Design, threat model and what is not covered:
//! `docs/nvrm-escape.md` section 12.
//!
//! What runs where:
//!
//! * BEFORE a forwarded `Ioctl` is sent, one lock hold judges it (`VirtioGpu::nvrm_judge`,
//!   folded into the hold that already resolves the handle's `device_type`): every field
//!   that names an RM client or a backend handle must be the caller's. [`apply`] turns the
//!   verdict into a refusal (mode 1), a count (mode 2) or nothing (mode 0).
//! * A client allocation ([`begin`]) reserves its table slot first, so a full table
//!   refuses BEFORE the host makes a client nobody tracks (as `Open` does for handles).
//! * AFTER the reply, [`after_reply`] records the client RM made, or forgets one a
//!   successful `NV_ESC_RM_FREE` freed; [`after_failed`] cancels a reservation and treats a
//!   timed-out free of a client as done (the safe direction: the entry goes).
//! * `Close` of a file ([`forget_via`]), an owner's teardown ([`forget_owner`]) and the
//!   transport's retirement ([`forget_all`]) drop what they own. The table itself is a
//!   field of the transport, so a new transport starts empty whatever happens here.
//!
//! The KMD's own RM client (`DeviceOwner::KMD_RM`) is never judged and never recorded: no
//! escape can present that owner, and its traffic is the KMD's own.
//!
//! The knob is `NvDupHarden` (service key): 2 (default for the first shipped package)
//! log-only (everything is recorded and judged, and what mode 1 would refuse is counted
//! in `NvDupWould` and forwarded), 1 enforce, 0 off (nothing is recorded or judged: the
//! pre-hardening behaviour exactly). Read once per boot (`reg add` + restart the device
//! to change it). An unknown value enforces. Flip the default to 1 after a real NVK run
//! shows `NvCliRec` close to `NvOpen`, `NvDupWould` 0 and `NvDupDoubt` small
//! (`docs/nvrm-escape.md` section 12.3).

use super::gpu::DeviceOwner;
use super::nvrm::Refusal;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::nvrm_clients::{self, Cause, Commit, Verdict};

/// `NvDupHarden` = 0: no tracking, no checks.
pub const MODE_OFF: u32 = 0;
/// `NvDupHarden` = 1: refuse.
pub const MODE_ENFORCE: u32 = 1;
/// `NvDupHarden` = 2 (the default for now): count what would be refused, forward
/// everything.
pub const MODE_LOG: u32 = 2;

const MODE_UNREAD: u32 = u32::MAX;
static MODE: AtomicU32 = AtomicU32::new(MODE_UNREAD);

/// Client handles recorded from a successful client allocation (`NvCliRec`), forgotten
/// by a free, a `Close`, a teardown or a sweep (`NvCliDrop`; `Rec - Drop` is the
/// clients live now, 0 when no process runs), and allocations whose client could not be
/// recorded or reserved because the table or the owner's quota was full (`NvCliFull`).
pub static NVRM_CLIENTS_RECORDED: AtomicU32 = AtomicU32::new(0);
pub static NVRM_CLIENTS_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static NVRM_CLIENTS_FULL: AtomicU32 = AtomicU32::new(0);
/// Requests that named a client or a backend handle that is not the caller's, by what
/// they named, counted in modes 1 and 2: the request's own client (`NvDupCli`), another
/// cross-client slot such as `hClientSrc` (`NvDupSrc`), a backend handle slot (`NvDupFd`).
pub static NVRM_DUP_CALLER: AtomicU32 = AtomicU32::new(0);
pub static NVRM_DUP_CLIENT: AtomicU32 = AtomicU32::new(0);
pub static NVRM_DUP_HANDLE: AtomicU32 = AtomicU32::new(0);
/// Of those, refused (`NvDupDeny`, mode 1; each is also in `NvRef`) or only counted
/// because the knob says log-only (`NvDupWould`, mode 2).
pub static NVRM_DUP_DENIED: AtomicU32 = AtomicU32::new(0);
pub static NVRM_DUP_WOULD: AtomicU32 = AtomicU32::new(0);
/// Requests with a slot the rules could not judge with confidence (a block of a size the
/// layout was not verified against, a field that is too short, a descriptor the host does
/// not translate): forwarded in every mode, counted here (`NvDupDoubt`).
pub static NVRM_DUP_DOUBT: AtomicU32 = AtomicU32::new(0);

/// The mode in force, once the knob was read (`None` before the first forward).
pub fn mode_if_read() -> Option<u32> {
    match MODE.load(Ordering::Relaxed) {
        MODE_UNREAD => None,
        m => Some(m),
    }
}

/// Forget the cached mode and read the knob again, mirroring it (`NvDupMode`). StartDevice: the
/// static outlives a `pnputil /restart-device`. PASSIVE.
pub(crate) fn reread_mode() -> u32 {
    read_mode()
}

/// The knob, read once per StartDevice (and at the first forward after one). PASSIVE: the
/// registry read is not callable above it.
fn mode(_passive: PassiveLevel) -> u32 {
    let m = MODE.load(Ordering::Relaxed);
    if m != MODE_UNREAD {
        return m;
    }
    read_mode()
}

#[inline(never)]
fn read_mode() -> u32 {
    let v = crate::diag::read_config_dword(crate::diag::knobs::NV_DUP_HARDEN, MODE_LOG);
    // An absent value is the default (log-only); a present one that is not 0 or 2
    // enforces, so a typo never turns the checks off.
    let m = match v {
        0 => MODE_OFF,
        2 => MODE_LOG,
        _ => MODE_ENFORCE,
    };
    MODE.store(m, Ordering::Relaxed);
    // Mirrored on EVERY read, so the value in force is the registry's and never a previous run's.
    crate::diag::record_named_bytes(b"NvDupMode", m);
    m
}

/// The mode that applies to a call of `owner`: the KMD's own client is never judged.
pub fn mode_for(passive: PassiveLevel, owner: DeviceOwner) -> u32 {
    if owner == DeviceOwner::KMD_RM {
        MODE_OFF
    } else {
        mode(passive)
    }
}

/// Turn a verdict into the forward's answer. `mode` is [`mode_for`]'s.
pub fn apply(mode: u32, verdict: Verdict) -> Result<(), Refusal> {
    match verdict {
        Verdict::Allow => Ok(()),
        Verdict::Doubt(_) => {
            NVRM_DUP_DOUBT.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Verdict::Deny(cause) => {
            let counter = match cause {
                Cause::CallerClient => &NVRM_DUP_CALLER,
                Cause::ClientRef => &NVRM_DUP_CLIENT,
                Cause::HandleRef | Cause::Malformed => &NVRM_DUP_HANDLE,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            if mode == MODE_ENFORCE {
                NVRM_DUP_DENIED.fetch_add(1, Ordering::Relaxed);
                // One code for "not yours" and "does not exist" (docs 11, item 4).
                Err(match cause {
                    Cause::Malformed => Refusal::BadRange,
                    _ => Refusal::NotOwned,
                })
            } else {
                NVRM_DUP_WOULD.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

/// Before forwarding `req`: a client allocation reserves its slot. `Ok(true)`: reserved,
/// and exactly one of [`after_reply`] / [`after_failed`] must follow. `Err`: the table or
/// the owner's quota is full (mode 1 only; mode 2 forwards and does not record).
pub fn begin(
    adapter: &AdapterContext,
    owner: DeviceOwner,
    mode: u32,
    req: &[u8],
) -> Result<bool, Refusal> {
    if mode == MODE_OFF || !nvrm_clients::root_alloc(req) {
        return Ok(false);
    }
    match adapter.with_virtio(|v| v.reserve_nvrm_client(owner)) {
        Ok(true) => Ok(true),
        // No transport: the round trip reports it.
        Err(_) => Ok(false),
        Ok(false) => {
            NVRM_CLIENTS_FULL.fetch_add(1, Ordering::Relaxed);
            if mode == MODE_ENFORCE {
                Err(Refusal::NoResources)
            } else {
                Ok(false)
            }
        }
    }
}

/// The host answered `req` with `n` bytes in `resp`. `handle` is the file the request
/// went through. `reserved` is what [`begin`] returned.
#[allow(clippy::too_many_arguments)]
pub fn after_reply(
    adapter: &AdapterContext,
    owner: DeviceOwner,
    handle: u32,
    mode: u32,
    reserved: bool,
    req: &[u8],
    resp: &[u8],
    n: usize,
) {
    if mode == MODE_OFF {
        return;
    }
    if nvrm_clients::root_alloc(req) {
        let client = nvrm_clients::client_from_reply(req, resp, n);
        let outcome = adapter.with_virtio(|v| match client {
            Some(c) => Some(v.commit_nvrm_client(owner, handle, c, reserved)),
            None => {
                if reserved {
                    v.cancel_nvrm_client(owner);
                }
                None
            }
        });
        match outcome {
            Ok(Some(Commit::Recorded)) | Ok(Some(Commit::Evicted)) => {
                NVRM_CLIENTS_RECORDED.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(Commit::Refused)) => {
                NVRM_CLIENTS_FULL.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    } else if let Some(client) = nvrm_clients::client_free(req) {
        if nvrm_clients::free_reply_ok(resp, n) {
            forget_client(adapter, owner, client);
        }
    }
}

/// The round trip of `req` failed. `timed_out`: the host may still have acted.
pub fn after_failed(
    adapter: &AdapterContext,
    owner: DeviceOwner,
    mode: u32,
    reserved: bool,
    req: &[u8],
    timed_out: bool,
) {
    if mode == MODE_OFF {
        return;
    }
    if reserved {
        let _ = adapter.with_virtio(|v| v.cancel_nvrm_client(owner));
    }
    // A free that timed out may have freed the client, and a number the host mints again
    // must not find the old owner still holding it: the entry goes (the owner loses a
    // client that may still be live, which it can free by closing its file).
    if timed_out {
        if let Some(client) = nvrm_clients::client_free(req) {
            forget_client(adapter, owner, client);
        }
    }
}

fn forget_client(adapter: &AdapterContext, owner: DeviceOwner, client: u32) {
    if adapter
        .with_virtio(|v| v.forget_nvrm_client(owner, client))
        .unwrap_or(false)
    {
        NVRM_CLIENTS_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// `Close` of backend file `handle` of `owner`: the host closes the clients made through it.
pub fn forget_via(adapter: &AdapterContext, owner: DeviceOwner, handle: u32) {
    let n = adapter
        .with_virtio(|v| v.forget_nvrm_clients_via(owner, handle))
        .unwrap_or(0);
    if n != 0 {
        NVRM_CLIENTS_DROPPED.fetch_add(n, Ordering::Relaxed);
    }
}

/// `owner`'s device is being destroyed: every client it was given goes.
pub fn forget_owner(adapter: &AdapterContext, owner: DeviceOwner) {
    let n = adapter
        .with_virtio(|v| v.forget_nvrm_clients_for_owner(owner))
        .unwrap_or(0);
    if n != 0 {
        NVRM_CLIENTS_DROPPED.fetch_add(n, Ordering::Relaxed);
    }
}

/// The transport's handles were all closed (or are being dropped): no client survives.
pub fn forget_all(adapter: &AdapterContext) {
    let n = adapter.with_virtio(|v| v.clear_nvrm_clients()).unwrap_or(0);
    if n != 0 {
        NVRM_CLIENTS_DROPPED.fetch_add(n, Ordering::Relaxed);
    }
}

/// Mirror the counters into the registry (`NvCli*`, `NvDup*`); PASSIVE.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    rec(b"NvCliRec", NVRM_CLIENTS_RECORDED.load(Ordering::Relaxed));
    rec(b"NvCliDrop", NVRM_CLIENTS_DROPPED.load(Ordering::Relaxed));
    rec(b"NvCliFull", NVRM_CLIENTS_FULL.load(Ordering::Relaxed));
    rec(b"NvDupCli", NVRM_DUP_CALLER.load(Ordering::Relaxed));
    rec(b"NvDupSrc", NVRM_DUP_CLIENT.load(Ordering::Relaxed));
    rec(b"NvDupFd", NVRM_DUP_HANDLE.load(Ordering::Relaxed));
    rec(b"NvDupDeny", NVRM_DUP_DENIED.load(Ordering::Relaxed));
    rec(b"NvDupWould", NVRM_DUP_WOULD.load(Ordering::Relaxed));
    rec(b"NvDupDoubt", NVRM_DUP_DOUBT.load(Ordering::Relaxed));
    // The mode in force, once the first forward read the knob.
    if let Some(m) = mode_if_read() {
        rec(b"NvDupMode", m);
    }
}

//! `RM_RESOURCE_IMPORT`: the KMD's side of the host's `RmResourceImport` message
//! (`MsgType` 31, `docs/VENUS.md` "RM-export resources in a second process").
//!
//! A second process (B) that OPENED an adopted foreign allocation of process A
//! has the resource id and nothing else. To map the same RM memory in its own RM
//! client it needs a GEM handle in its own host DRM file; the host makes one from
//! the resource's dma-buf (`PRIME_FD_TO_HANDLE`) when the KMD asks. NVK then does
//! `GEM_EXPORT_NVKMS_MEMORY` and `OS_UNIX_IMPORT_OBJECT_FROM_FD` in user mode,
//! through `FORWARD` as for any dma-buf.
//!
//! What the host cannot check, and this module decides:
//!
//! * `owner_handle` is a backend handle the CALLER's device opened, and a DRM
//!   node (the host only knows it is a render node of this guest, not whose);
//! * `resource_id` is a foreign resource that the caller's device created
//!   (`IMPORT_RM`, before adoption) or whose adopting allocation the caller's
//!   process holds an open of (`DxgkDdiOpenAllocation`, recorded in the foreign
//!   table), and is not destroyed (a destroyed allocation with opens still alive
//!   is "defer-pending": its host resource lives for the opens' sake, but no new
//!   reference may start on it);
//! * the request is sized exactly and the reply is read only as far as the
//!   transport says it was written.
//!
//! Everything here is a pure function of its arguments.

use crate::foreign_errno::Verdict;
use crate::foreign_resource::ForeignTable;
use crate::nvrm_fence::is_dri_node;

/// Host `MsgType::RmResourceImport`. NOT a member of
/// `HELIOS_NVRM_FORWARD_MSG_TYPES`: the KMD sends it itself, after the checks
/// below, and `FORWARD` keeps refusing it.
pub const MSG_RM_RESOURCE_IMPORT: u32 = 31;
/// Bytes of the host `MsgHeader` at the start of every request and reply.
pub const MSG_HDR: usize = 16;
/// The request payload: `owner_handle`, `resource_id`, `flags`, `reserved`.
pub const REQUEST_PAYLOAD: usize = 16;
/// Exact length of the request the host accepts.
pub const REQUEST_BYTES: usize = MSG_HDR + REQUEST_PAYLOAD;
/// The reply payload: `gem_handle u32`, `flags u32`, `size u64`, `modifier u64`.
pub const REPLY_PAYLOAD: usize = 24;
/// Smallest length of a successful reply.
pub const REPLY_BYTES: usize = MSG_HDR + REPLY_PAYLOAD;
/// `MsgHeader.status` offset (a signed errno, 0 on success).
const STATUS_AT: usize = 8;

/// Reply `flags` bit 0: `modifier` is known.
pub const REPLY_FLAG_MODIFIER: u32 = 1 << 0;
/// Config `features` bit `NVGPU_CFG_RM_RESOURCE_IMPORT`. The host sets it together
/// with `NVGPU_CFG_RM_IMPORT` (bit 13) and `NVGPU_CFG_VENUS` (bit 10).
pub const CFG_RM_RESOURCE_IMPORT: u32 = 1 << 14;

fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn rd64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The request: `MsgHeader{type 31, handle 0, status 0, pad 0}` then
/// `{owner_handle, resource_id, flags = 0, reserved = 0}`.
pub fn build_request(owner_handle: u32, resource_id: u32) -> [u8; REQUEST_BYTES] {
    let mut req = [0u8; REQUEST_BYTES];
    wr32(&mut req, 0, MSG_RM_RESOURCE_IMPORT);
    wr32(&mut req, MSG_HDR, owner_handle);
    wr32(&mut req, MSG_HDR + 4, resource_id);
    req
}

/// A successful reply, reduced to what the caller is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// GEM handle in `owner_handle`'s host DRM file. Nonzero.
    pub gem_handle: u32,
    /// Only the bits this KMD knows ([`REPLY_FLAG_MODIFIER`]).
    pub flags: u32,
    /// The object's size, bytes.
    pub size: u64,
    /// The modifier the resource was created with; 0 unless `flags` says it is
    /// valid (never the host's raw word when it said the field is unknown).
    pub modifier: u64,
}

/// Why a reply is not a [`Reply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// Fewer bytes than the header, or than a success reply, were written.
    Short,
    /// The header carried `-errno`; the value is the errno (positive).
    HostErrno(u32),
    /// A status the protocol does not define (positive), or a success whose
    /// content cannot be right (a zero GEM handle).
    Malformed,
}

/// Parse a reply. `resp` is the landing buffer and `n` the byte count the
/// transport reported written; bytes past `n` (or past `resp`) are never read.
pub fn parse_reply(resp: &[u8], n: usize) -> Result<Reply, ReplyError> {
    let resp = &resp[..n.min(resp.len())];
    if resp.len() < MSG_HDR {
        return Err(ReplyError::Short);
    }
    let status = rd32(resp, STATUS_AT).ok_or(ReplyError::Short)? as i32;
    if status < 0 {
        return Err(ReplyError::HostErrno(status.unsigned_abs()));
    }
    if status > 0 {
        return Err(ReplyError::Malformed);
    }
    if resp.len() < REPLY_BYTES {
        return Err(ReplyError::Short);
    }
    let (Some(gem), Some(flags), Some(size), Some(modifier)) = (
        rd32(resp, MSG_HDR),
        rd32(resp, MSG_HDR + 4),
        rd64(resp, MSG_HDR + 8),
        rd64(resp, MSG_HDR + 16),
    ) else {
        return Err(ReplyError::Short);
    };
    if gem == 0 {
        return Err(ReplyError::Malformed);
    }
    let flags = flags & REPLY_FLAG_MODIFIER;
    Ok(Reply {
        gem_handle: gem,
        flags,
        size,
        modifier: if flags & REPLY_FLAG_MODIFIER != 0 {
            modifier
        } else {
            0
        },
    })
}

/// The verdict for a host errno, as `IMPORT_RM`'s: `EBADF`, `ENOENT` not owned;
/// `EINVAL`, `ERANGE` bad range; `EOPNOTSUPP` and `EPROTO` (an older backend)
/// unsupported; `ENOMEM` no resources; anything else a device error.
pub fn verdict_for_errno(errno: u32) -> Verdict {
    crate::foreign_errno::classify(errno)
}

/// Why the KMD turned the request away before the wire. All of them except
/// [`Refusal::BadRequest`] reach the caller as one answer, so a process learns
/// nothing about another's handles or resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A zero handle, resource id or device token, or nonzero request flags.
    BadRequest,
    /// `rm_handle` is not a backend handle the caller's device opened, or is not
    /// a DRM node.
    HandleNotOwned,
    /// No foreign resource has this id.
    NoSuchResource,
    /// The adopting allocation was destroyed (its host resource may live on for
    /// the opens still alive, but nothing new may start on it).
    Destroyed,
    /// The caller neither created the resource nor holds an open of it.
    NotPermitted,
}

impl Refusal {
    pub const fn verdict(self) -> Verdict {
        match self {
            Self::BadRequest => Verdict::BadRange,
            _ => Verdict::NotOwned,
        }
    }
}

/// The gating rule, evaluated in ONE lock hold by the driver (it reads
/// `handle_device_type` and `table` under the same device spinlock).
///
/// * `handle_device_type`: the `device_type` the caller's device opened
///   `rm_handle` with, or `None` if it is not the caller's;
/// * `owner`: the escaping device's token; `process`: dxgkrnl's `hKmdProcess`
///   of the escaping device (0 if unknown, which disables the open route).
///
/// Allowed iff the handle is the caller's DRM node AND the resource exists, is
/// not destroyed, and (the caller's device created it, or the caller's process
/// holds an open of the allocation that adopted it).
pub fn authorize(
    table: &ForeignTable,
    handle_device_type: Option<u32>,
    owner: u64,
    process: u64,
    rm_handle: u32,
    resource_id: u32,
) -> Result<(), Refusal> {
    if owner == 0 || rm_handle == 0 || resource_id == 0 {
        return Err(Refusal::BadRequest);
    }
    match handle_device_type {
        Some(t) if is_dri_node(t) => {}
        _ => return Err(Refusal::HandleNotOwned),
    }
    let Some(e) = table.get(resource_id) else {
        return Err(Refusal::NoSuchResource);
    };
    if e.destroyed {
        return Err(Refusal::Destroyed);
    }
    let by_creator = e.creator == Some(owner);
    let by_open = process != 0 && table.process_has_open(resource_id, process);
    if by_creator || by_open {
        Ok(())
    } else {
        Err(Refusal::NotPermitted)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::foreign_errno::*;
    use crate::foreign_resource::{Layout, FOURCC_XRGB8888};
    use std::vec::Vec;

    const A: u64 = 0xA0;
    const B: u64 = 0xB0;
    const PA: u64 = 0x1000;
    const PB: u64 = 0x2000;
    const RES: u32 = 100;

    fn layout() -> Layout {
        Layout {
            width: 1920,
            height: 1080,
            stride: 7680,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: 0x0300_0000_0060_6015,
            plane1: None,
        }
    }

    /// Resource 100 imported by device A: the creator's state.
    fn imported() -> ForeignTable {
        let mut t = ForeignTable::new();
        let r = t.reserve(A, 1 << 20).unwrap();
        t.commit(r, RES, 1, 5, 77, layout()).unwrap();
        t
    }

    fn adopted() -> ForeignTable {
        let mut t = imported();
        assert!(t.adopt(RES));
        t
    }

    const DRM: Option<u32> = Some(512);

    // ---- request ----------------------------------------------------------

    #[test]
    fn the_request_is_a_header_and_four_words() {
        let r = build_request(0x1122_3344, 0x5566_7788);
        assert_eq!(r.len(), 32);
        assert_eq!(&r[0..4], &31u32.to_le_bytes());
        // handle, status, padding of the MsgHeader are zero.
        assert_eq!(&r[4..16], &[0u8; 12]);
        assert_eq!(&r[16..20], &0x1122_3344u32.to_le_bytes());
        assert_eq!(&r[20..24], &0x5566_7788u32.to_le_bytes());
        // flags and reserved are zero, or the host answers EINVAL.
        assert_eq!(&r[24..32], &[0u8; 8]);
    }

    #[test]
    fn the_constants_match_the_wire_shape() {
        assert_eq!(MSG_RM_RESOURCE_IMPORT, 31);
        assert_eq!(REQUEST_BYTES, 32);
        assert_eq!(REPLY_BYTES, 40);
        assert_eq!(CFG_RM_RESOURCE_IMPORT, 1 << 14);
    }

    // ---- reply ------------------------------------------------------------

    fn ok_reply(gem: u32, flags: u32, size: u64, modifier: u64) -> [u8; REPLY_BYTES] {
        let mut r = [0u8; REPLY_BYTES];
        r[0..4].copy_from_slice(&31u32.to_le_bytes());
        r[16..20].copy_from_slice(&gem.to_le_bytes());
        r[20..24].copy_from_slice(&flags.to_le_bytes());
        r[24..32].copy_from_slice(&size.to_le_bytes());
        r[32..40].copy_from_slice(&modifier.to_le_bytes());
        r
    }

    fn err_reply(errno: i32) -> [u8; MSG_HDR] {
        let mut r = [0u8; MSG_HDR];
        r[0..4].copy_from_slice(&31u32.to_le_bytes());
        r[8..12].copy_from_slice(&(-errno).to_le_bytes());
        r
    }

    #[test]
    fn a_full_reply_parses() {
        let r = ok_reply(9, 1, 0x80_0000, 0x0300_0000_0060_6015);
        assert_eq!(
            parse_reply(&r, r.len()),
            Ok(Reply {
                gem_handle: 9,
                flags: 1,
                size: 0x80_0000,
                modifier: 0x0300_0000_0060_6015
            })
        );
    }

    #[test]
    fn the_modifier_is_zero_unless_the_host_says_it_is_valid() {
        let r = ok_reply(9, 0, 4096, 0xdead_beef);
        let p = parse_reply(&r, r.len()).unwrap();
        assert_eq!((p.flags, p.modifier), (0, 0));
    }

    #[test]
    fn unknown_flag_bits_are_not_passed_on() {
        let r = ok_reply(9, 0xffff_fffe, 4096, 7);
        let p = parse_reply(&r, r.len()).unwrap();
        assert_eq!((p.flags, p.modifier), (0, 0));
        let r = ok_reply(9, 0xffff_ffff, 4096, 7);
        let p = parse_reply(&r, r.len()).unwrap();
        assert_eq!((p.flags, p.modifier), (1, 7));
    }

    #[test]
    fn a_short_success_reply_is_refused_not_read() {
        let r = ok_reply(9, 1, 4096, 7);
        for n in 0..REPLY_BYTES {
            assert_eq!(parse_reply(&r, n), Err(ReplyError::Short), "n = {n}");
        }
        // A count above the buffer is clamped, not trusted.
        assert_eq!(parse_reply(&r[..30], 4096), Err(ReplyError::Short));
        assert!(parse_reply(&r, 4096).is_ok());
        // Bytes past `n` are never read even when the buffer holds a full reply.
        assert_eq!(parse_reply(&r, 39), Err(ReplyError::Short));
    }

    #[test]
    fn a_bare_header_error_is_an_errno() {
        for e in [1, 2, 9, 22, 71, 95] {
            let r = err_reply(e);
            assert_eq!(
                parse_reply(&r, r.len()),
                Err(ReplyError::HostErrno(e as u32))
            );
        }
        // An error reply longer than a header (a landing buffer that was not
        // zeroed past the header) is still an error.
        let mut long = [0xAAu8; REPLY_BYTES];
        long[8..12].copy_from_slice(&(-2i32).to_le_bytes());
        assert_eq!(
            parse_reply(&long, long.len()),
            Err(ReplyError::HostErrno(2))
        );
    }

    #[test]
    fn an_error_status_wins_over_a_short_length() {
        // The header is all an error has; it must not read as `Short`.
        let r = err_reply(71);
        assert_eq!(parse_reply(&r, MSG_HDR), Err(ReplyError::HostErrno(71)));
        assert_eq!(parse_reply(&r, 15), Err(ReplyError::Short));
    }

    #[test]
    fn nonsense_is_malformed() {
        let mut r = ok_reply(9, 1, 4096, 7);
        r[8..12].copy_from_slice(&5i32.to_le_bytes());
        assert_eq!(parse_reply(&r, r.len()), Err(ReplyError::Malformed));
        let r = ok_reply(0, 1, 4096, 7);
        assert_eq!(parse_reply(&r, r.len()), Err(ReplyError::Malformed));
        let mut r = err_reply(1);
        r[8..12].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(
            parse_reply(&r, r.len()),
            Err(ReplyError::HostErrno(0x8000_0000))
        );
    }

    // ---- errno ------------------------------------------------------------

    #[test]
    fn host_errnos_map_as_the_import_does() {
        assert_eq!(verdict_for_errno(EBADF), Verdict::NotOwned);
        assert_eq!(verdict_for_errno(ENOENT), Verdict::NotOwned);
        assert_eq!(verdict_for_errno(EINVAL), Verdict::BadRange);
        assert_eq!(verdict_for_errno(ERANGE), Verdict::BadRange);
        assert_eq!(verdict_for_errno(EOPNOTSUPP), Verdict::Unsupported);
        // An older backend: "no such message".
        assert_eq!(verdict_for_errno(EPROTO), Verdict::Unsupported);
        assert_eq!(verdict_for_errno(ENOMEM), Verdict::NoResources);
        assert_eq!(verdict_for_errno(EIO), Verdict::Device);
        assert_eq!(verdict_for_errno(13), Verdict::Device); // EACCES from PRIME
        assert_eq!(verdict_for_errno(0), Verdict::Device);
    }

    // ---- gate -------------------------------------------------------------

    #[test]
    fn the_creating_device_may_import_before_adoption() {
        let t = imported();
        assert_eq!(authorize(&t, DRM, A, PA, 5, RES), Ok(()));
        // Not even another device of the same process.
        assert_eq!(
            authorize(&t, DRM, B, PA, 5, RES),
            Err(Refusal::NotPermitted)
        );
    }

    #[test]
    fn an_opener_may_import_and_a_stranger_may_not() {
        let mut t = adopted();
        // Adoption makes the resource KMD-owned: the creator's token no longer
        // names it, and a process with no open has no route either.
        assert_eq!(
            authorize(&t, DRM, B, PB, 6, RES),
            Err(Refusal::NotPermitted)
        );
        assert_eq!(
            authorize(&t, DRM, A, PA, 5, RES),
            Err(Refusal::NotPermitted)
        );
        t.open(RES, PB);
        assert_eq!(authorize(&t, DRM, B, PB, 6, RES), Ok(()));
        // A second device of B's process shares the process's open.
        assert_eq!(authorize(&t, DRM, 0xB1, PB, 6, RES), Ok(()));
        // Another process still has none.
        assert_eq!(
            authorize(&t, DRM, 0xC0, 0x3000, 7, RES),
            Err(Refusal::NotPermitted)
        );
        // The close ends the route.
        t.close(RES, PB);
        assert_eq!(
            authorize(&t, DRM, B, PB, 6, RES),
            Err(Refusal::NotPermitted)
        );
    }

    #[test]
    fn an_unknown_process_has_no_open_route() {
        let mut t = adopted();
        t.open(RES, 0);
        assert_eq!(authorize(&t, DRM, B, 0, 6, RES), Err(Refusal::NotPermitted));
    }

    #[test]
    fn the_handle_must_be_the_callers_drm_node() {
        let mut t = adopted();
        t.open(RES, PB);
        assert_eq!(
            authorize(&t, None, B, PB, 6, RES),
            Err(Refusal::HandleNotOwned)
        );
        for dt in [0u32, 255, 256, 257, 258, 511] {
            assert_eq!(
                authorize(&t, Some(dt), B, PB, 6, RES),
                Err(Refusal::HandleNotOwned),
                "device_type {dt}"
            );
        }
        assert_eq!(authorize(&t, Some(512), B, PB, 6, RES), Ok(()));
        assert_eq!(authorize(&t, Some(640), B, PB, 6, RES), Ok(()));
    }

    #[test]
    fn a_missing_or_destroyed_resource_is_refused() {
        let mut t = adopted();
        t.open(RES, PB);
        assert_eq!(
            authorize(&t, DRM, B, PB, 6, 999),
            Err(Refusal::NoSuchResource)
        );
        // The adopting allocation is destroyed with B's open alive: deferred.
        assert!(matches!(
            t.allocation_destroyed(RES),
            crate::foreign_resource::DestroyOutcome::Deferred { opens: 1 }
        ));
        assert_eq!(authorize(&t, DRM, B, PB, 6, RES), Err(Refusal::Destroyed));
        // The last close releases and removes the record.
        t.close(RES, PB);
        t.remove(RES);
        assert_eq!(
            authorize(&t, DRM, B, PB, 6, RES),
            Err(Refusal::NoSuchResource)
        );
    }

    #[test]
    fn zeroes_are_a_bad_request_and_answer_before_anything_else() {
        let t = adopted();
        for (o, h, r) in [(0, 6, RES), (B, 0, RES), (B, 6, 0)] {
            assert_eq!(
                authorize(&t, None, o, PB, h, r),
                Err(Refusal::BadRequest),
                "{o} {h} {r}"
            );
        }
    }

    #[test]
    fn refusals_reach_the_caller_as_one_answer_but_for_a_bad_request() {
        let all = [
            Refusal::HandleNotOwned,
            Refusal::NoSuchResource,
            Refusal::Destroyed,
            Refusal::NotPermitted,
        ];
        let v: Vec<_> = all.iter().map(|r| r.verdict()).collect();
        assert!(v.iter().all(|v| *v == Verdict::NotOwned));
        assert_eq!(Refusal::BadRequest.verdict(), Verdict::BadRange);
    }
}

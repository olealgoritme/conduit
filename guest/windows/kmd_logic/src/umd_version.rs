//! `UMD_VERSION`: the KMD's side of the host's `UmdVersion` message (`MsgType` 36).
//!
//! In the NVIDIA personality NVIDIA's own UMDs talk to the host's RM through
//! this KMD, with the parameter layouts of the release they were built with.
//! Nothing on that path checks the release (the KMD answers the UMDs'
//! version questions itself), so at every StartDevice the KMD tells the host
//! which release it was installed with and the host decides whether its RM
//! speaks that ABI.
//!
//! The release comes from the service's `Parameters\NvUmdVersion` DWORD, which
//! the installer writes when it stages the UMDs: NVIDIA's own number with the
//! dot taken out, `61088` for `610.88` (file version `32.0.16.1088`). Two
//! decimal digits of minor, as NVIDIA numbers its Windows releases.
//!
//! What the start does with the answer:
//!
//! | reply                               | start   |
//! |-------------------------------------|---------|
//! | match, branch-only, unchecked       | goes on |
//! | mismatch                            | fails   |
//! | `-EPROTO` (host predates the check) | goes on |
//! | no release recorded in the registry | goes on |
//! | transport error, malformed reply    | goes on |
//!
//! Only a host that says "mismatch" stops the start: the host refuses that
//! guest's RM calls from then on anyway, and failing the start says so where
//! Device Manager shows it. Everything else is recorded and the start goes on,
//! so a host or a package that predates the check keeps working.
//!
//! Everything here is a pure function of its arguments.

/// Host `MsgType::UmdVersion`. Sent by the KMD itself, never forwarded.
pub const MSG_UMD_VERSION: u32 = 36;
/// Bytes of the host `MsgHeader` at the start of every request and reply.
pub const MSG_HDR: usize = 16;
/// The request payload: `kind`, `major`, `minor`, `reserved`.
pub const REQUEST_BYTES: usize = MSG_HDR + 16;
/// The reply payload: `verdict`, host `major`, `minor`, `patch`.
pub const REPLY_BYTES: usize = MSG_HDR + 16;
/// `UmdVersion::kind`: NVIDIA's Windows UMDs.
pub const KIND_NVIDIA_WINDOWS: u32 = 1;

pub const VERDICT_MATCH: u32 = 0;
pub const VERDICT_MISMATCH: u32 = 1;
pub const VERDICT_UNCHECKED: u32 = 2;
pub const VERDICT_BRANCH: u32 = 3;

/// `EPROTO`: what a backend without the message answers.
pub const EPROTO: u32 = 71;

const STATUS_AT: usize = 8;

fn wr32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// A Windows NVIDIA release, `610.88`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub major: u32,
    pub minor: u32,
}

impl Release {
    /// From the `NvUmdVersion` DWORD (`61088`). 0 and anything without a
    /// three-digit major are "not recorded".
    pub fn from_dword(v: u32) -> Option<Self> {
        let major = v / 100;
        (100..1000).contains(&major).then_some(Self {
            major,
            minor: v % 100,
        })
    }

    /// Back to the DWORD form, for diagnostics.
    pub const fn dword(self) -> u32 {
        self.major * 100 + self.minor
    }
}

/// `MsgHeader{type 36, handle 0, status 0, pad 0}` then
/// `{kind = 1, major, minor, reserved = 0}`.
pub fn build_request(r: Release) -> [u8; REQUEST_BYTES] {
    let mut req = [0u8; REQUEST_BYTES];
    wr32(&mut req, 0, MSG_UMD_VERSION);
    wr32(&mut req, MSG_HDR, KIND_NVIDIA_WINDOWS);
    wr32(&mut req, MSG_HDR + 4, r.major);
    wr32(&mut req, MSG_HDR + 8, r.minor);
    req
}

/// The host's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Match {
        host: u32,
    },
    Mismatch {
        host: u32,
    },
    Unchecked {
        host: u32,
    },
    Branch {
        host: u32,
    },
    /// The host does not know the message (`-EPROTO`).
    NotServed,
    /// Any other errno (positive).
    HostErrno(u32),
    /// Too short, or a verdict the protocol does not define.
    Malformed,
}

impl Outcome {
    /// Whether the start may go on.
    pub const fn start_allowed(self) -> bool {
        !matches!(self, Self::Mismatch { .. })
    }

    /// One diagnostic word: verdict in the low byte (0..3 as on the wire,
    /// 0x10 not served, 0x11 host errno, 0x12 malformed).
    pub const fn code(self) -> u32 {
        match self {
            Self::Match { .. } => VERDICT_MATCH,
            Self::Mismatch { .. } => VERDICT_MISMATCH,
            Self::Unchecked { .. } => VERDICT_UNCHECKED,
            Self::Branch { .. } => VERDICT_BRANCH,
            Self::NotServed => 0x10,
            Self::HostErrno(_) => 0x11,
            Self::Malformed => 0x12,
        }
    }

    /// The host RM release as `major * 10000 + minor * 100 + patch`
    /// (`6105704` for `610.57.04`); 0 when the reply carried none.
    pub const fn host(self) -> u32 {
        match self {
            Self::Match { host }
            | Self::Mismatch { host }
            | Self::Unchecked { host }
            | Self::Branch { host } => host,
            _ => 0,
        }
    }
}

/// Read a reply. `resp` is the landing buffer and `n` the byte count the
/// transport reported written; bytes past `n` are never read.
pub fn parse_reply(resp: &[u8], n: usize) -> Outcome {
    let resp = &resp[..n.min(resp.len())];
    let Some(status) = rd32(resp, STATUS_AT).map(|s| s as i32) else {
        return Outcome::Malformed;
    };
    if status < 0 {
        return match status.unsigned_abs() {
            EPROTO => Outcome::NotServed,
            e => Outcome::HostErrno(e),
        };
    }
    if status > 0 || resp.len() < REPLY_BYTES {
        return Outcome::Malformed;
    }
    let (Some(verdict), Some(major), Some(minor), Some(patch)) = (
        rd32(resp, MSG_HDR),
        rd32(resp, MSG_HDR + 4),
        rd32(resp, MSG_HDR + 8),
        rd32(resp, MSG_HDR + 12),
    ) else {
        return Outcome::Malformed;
    };
    let host = major
        .saturating_mul(10_000)
        .saturating_add(minor.min(99) * 100)
        .saturating_add(patch.min(99));
    match verdict {
        VERDICT_MATCH => Outcome::Match { host },
        VERDICT_MISMATCH => Outcome::Mismatch { host },
        VERDICT_UNCHECKED => Outcome::Unchecked { host },
        VERDICT_BRANCH => Outcome::Branch { host },
        _ => Outcome::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: i32, words: [u32; 4]) -> [u8; REPLY_BYTES] {
        let mut r = [0u8; REPLY_BYTES];
        wr32(&mut r, 0, MSG_UMD_VERSION);
        wr32(&mut r, STATUS_AT, status as u32);
        for (i, w) in words.into_iter().enumerate() {
            wr32(&mut r, MSG_HDR + 4 * i, w);
        }
        r
    }

    #[test]
    fn the_registry_dword_is_nvidias_number_without_the_dot() {
        assert_eq!(
            Release::from_dword(61088),
            Some(Release {
                major: 610,
                minor: 88
            })
        );
        assert_eq!(Release::from_dword(58108).map(|r| r.minor), Some(8));
        assert_eq!(Release::from_dword(0), None);
        assert_eq!(Release::from_dword(9999), None);
        assert_eq!(Release::from_dword(u32::MAX), None);
        assert_eq!(Release::from_dword(61088).unwrap().dword(), 61088);
    }

    #[test]
    fn the_request_is_header_then_kind_and_release() {
        let req = build_request(Release {
            major: 610,
            minor: 88,
        });
        assert_eq!(rd32(&req, 0), Some(MSG_UMD_VERSION));
        assert_eq!(rd32(&req, 4), Some(0));
        assert_eq!(rd32(&req, MSG_HDR), Some(KIND_NVIDIA_WINDOWS));
        assert_eq!(rd32(&req, MSG_HDR + 4), Some(610));
        assert_eq!(rd32(&req, MSG_HDR + 8), Some(88));
        assert_eq!(rd32(&req, MSG_HDR + 12), Some(0));
    }

    #[test]
    fn only_a_mismatch_stops_the_start() {
        let m = parse_reply(&reply(0, [VERDICT_MISMATCH, 615, 78, 8]), REPLY_BYTES);
        assert_eq!(m, Outcome::Mismatch { host: 6_157_808 });
        assert!(!m.start_allowed());
        for v in [VERDICT_MATCH, VERDICT_UNCHECKED, VERDICT_BRANCH] {
            let o = parse_reply(&reply(0, [v, 610, 57, 4]), REPLY_BYTES);
            assert!(o.start_allowed(), "{o:?}");
            assert_eq!(o.host(), 6_105_704);
            assert_eq!(o.code(), v);
        }
    }

    #[test]
    fn an_older_host_is_not_a_mismatch() {
        let o = parse_reply(&reply(-(EPROTO as i32), [0; 4]), MSG_HDR);
        assert_eq!(o, Outcome::NotServed);
        assert!(o.start_allowed());
        assert_eq!(
            parse_reply(&reply(-22, [0; 4]), MSG_HDR),
            Outcome::HostErrno(22)
        );
    }

    #[test]
    fn short_or_unknown_replies_are_malformed_and_not_fatal() {
        let full = reply(0, [VERDICT_MISMATCH, 1, 2, 3]);
        assert_eq!(parse_reply(&full, MSG_HDR + 4), Outcome::Malformed);
        assert_eq!(parse_reply(&full, 4), Outcome::Malformed);
        let odd = reply(0, [9, 1, 2, 3]);
        assert_eq!(parse_reply(&odd, REPLY_BYTES), Outcome::Malformed);
        assert!(Outcome::Malformed.start_allowed());
    }
}

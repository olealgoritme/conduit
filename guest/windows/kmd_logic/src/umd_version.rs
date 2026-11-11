//! `UMD_VERSION`: the KMD's side of the host's `UmdVersion` message (`MsgType` 36).
//!
//! In the NVIDIA personality NVIDIA's own UMDs talk to the host's RM through
//! this KMD, with the parameter layouts of the release they were built with.
//! Nothing on that path checks the release (the KMD answers the UMDs'
//! version questions itself), so at every StartDevice the KMD tells the host
//! which release it was installed with and the host decides whether its RM
//! speaks that ABI.
//!
//! The release is the REG_SZ [`VALUE_NAME`] (`NvUmdVersion`), the one value the
//! installer checks a package against before it stages NVIDIA's UMDs
//! (`packaging/windows/Helios-PackageCommon.ps1`): in the service key
//! (`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`), an
//! administrator's override, else in its `Parameters` subkey, where the Helios
//! INF writes the release this KMD speaks (`610.88`). NVIDIA's short form, two
//! decimal digits of minor as NVIDIA numbers its Windows releases, or a driver
//! file version (`32.0.16.1088`), the two forms the installer accepts.
//!
//! What the start does with the answer:
//!
//! | reply                               | start   |
//! |-------------------------------------|---------|
//! | match, branch-only, unchecked       | goes on |
//! | mismatch                            | fails   |
//! | `-EPROTO` (host predates the check) | goes on |
//! | no release recorded in the registry | goes on |
//! | a value that is not a release       | goes on |
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

/// The registry value the release is read from, in the service key and in its
/// `Parameters` subkey.
pub const VALUE_NAME: &str = "NvUmdVersion";
/// `REG_SZ`, the value's one type.
pub const REG_SZ: u32 = 1;
/// Longest value data looked at, in bytes (UTF-16, terminator included).
pub const MAX_VALUE_BYTES: usize = 64;

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

/// Digits only, 1..=`max` of them.
fn number(s: &[u8], max: usize) -> Option<u32> {
    if s.is_empty() || s.len() > max || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
}

impl Release {
    /// From the text of `NvUmdVersion`, in either form the installer accepts:
    /// NVIDIA's short one, `610.88` (a three-digit major, two digits of minor),
    /// or a driver file version, `32.0.16.1088` (the last digit of the third
    /// field and the four of the fourth: `610.88`). Anything else, a Linux
    /// release such as `610.57.04` among it, is not a Windows release.
    pub fn parse(text: &[u8]) -> Option<Self> {
        let mut parts = [&text[..0]; 4];
        let mut count = 0;
        for part in text.split(|&b| b == b'.') {
            *parts.get_mut(count)? = part;
            count += 1;
        }
        let release = match count {
            2 if parts[0].len() == 3 && parts[1].len() == 2 => Self {
                major: number(parts[0], 3)?,
                minor: number(parts[1], 2)?,
            },
            4 => {
                for part in &parts[..2] {
                    number(part, 5)?;
                }
                let branch = number(parts[2], 5)? % 10;
                let build = number(parts[3], 4)?;
                Self {
                    major: branch * 100 + build / 100,
                    minor: build % 100,
                }
            }
            _ => return None,
        };
        (100..1000).contains(&release.major).then_some(release)
    }

    /// NVIDIA's number with the dot taken out (`61088`), for diagnostics.
    pub const fn dword(self) -> u32 {
        self.major * 100 + self.minor
    }
}

/// One `NvUmdVersion` value, as the registry holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// No value, or an empty string.
    Absent,
    Release(Release),
    /// A value that is not a REG_SZ holding a release: another type (a DWORD
    /// among them), too long, not ASCII, or not a version.
    Unusable,
}

impl Value {
    /// From a value's registry type and data (UTF-16LE, a terminating NUL and
    /// anything after it ignored). Surrounding blanks are ignored, as the
    /// installer ignores them.
    pub fn from_registry(value_type: u32, data: &[u8]) -> Self {
        if value_type != REG_SZ || data.len() > MAX_VALUE_BYTES {
            return Self::Unusable;
        }
        let mut text = [0u8; MAX_VALUE_BYTES / 2];
        let mut len = 0;
        for unit in data.chunks_exact(2) {
            let unit = u16::from_le_bytes([unit[0], unit[1]]);
            if unit == 0 {
                break;
            }
            let Ok(byte) = u8::try_from(unit) else {
                return Self::Unusable;
            };
            if !byte.is_ascii() {
                return Self::Unusable;
            }
            text[len] = byte;
            len += 1;
        }
        let text = text[..len].trim_ascii();
        if text.is_empty() {
            return Self::Absent;
        }
        Release::parse(text).map_or(Self::Unusable, Self::Release)
    }
}

/// The value a start reports: the service key's (an administrator's override)
/// when it holds anything, else the `Parameters` one the INF writes. An
/// override that is not a release is not passed over for the INF's, as the
/// installer does not pass over it either.
pub fn configured(service: Value, parameters: impl FnOnce() -> Value) -> Value {
    match service {
        Value::Absent => parameters(),
        held => held,
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

    const R61088: Release = Release {
        major: 610,
        minor: 88,
    };

    /// `text` as REG_SZ data: UTF-16LE with its terminator.
    fn sz(text: &str) -> ([u8; 2 * MAX_VALUE_BYTES], usize) {
        let mut out = [0u8; 2 * MAX_VALUE_BYTES];
        let mut n = 0;
        for unit in text.encode_utf16().chain([0]) {
            out[n..n + 2].copy_from_slice(&unit.to_le_bytes());
            n += 2;
        }
        (out, n)
    }

    fn value(text: &str) -> Value {
        let (data, n) = sz(text);
        Value::from_registry(REG_SZ, &data[..n])
    }

    #[test]
    fn the_short_form_is_nvidias_windows_release() {
        assert_eq!(Release::parse(b"610.88"), Some(R61088));
        assert_eq!(
            Release::parse(b"581.08"),
            Some(Release {
                major: 581,
                minor: 8
            })
        );
        assert_eq!(Release::parse(b"610.88").unwrap().dword(), 61088);
        for bad in [
            &b"610"[..],
            b"610.8",
            b"610.088",
            b"61.88",
            b"6100.88",
            b"061.88",
            b"610.57.04",
            b"610.88.",
            b".610.88",
            b"610,88",
            b"610.x8",
            b"+10.88",
            b"",
            b"1.2.3.4.5",
        ] {
            assert_eq!(Release::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_file_version_reads_as_the_installer_reads_it() {
        assert_eq!(Release::parse(b"32.0.16.1088"), Some(R61088));
        assert_eq!(
            Release::parse(b"32.0.15.6094"),
            Some(Release {
                major: 560,
                minor: 94
            })
        );
        assert_eq!(
            Release::parse(b"32.0.15.8108"),
            Some(Release {
                major: 581,
                minor: 8
            })
        );
        // No three-digit major (the branch digit 0), a fourth field past four
        // digits, empty fields.
        assert_eq!(Release::parse(b"32.0.10.0988"), None);
        assert_eq!(Release::parse(b"32.0.16.10880"), None);
        assert_eq!(Release::parse(b"32..16.1088"), None);
    }

    #[test]
    fn the_registry_value_is_a_reg_sz() {
        assert_eq!(value("610.88"), Value::Release(R61088));
        assert_eq!(value("  610.88 "), Value::Release(R61088));
        assert_eq!(value("32.0.16.1088"), Value::Release(R61088));
        assert_eq!(value(""), Value::Absent);
        assert_eq!(value("   "), Value::Absent);
        assert_eq!(Value::from_registry(REG_SZ, &[]), Value::Absent);
        assert_eq!(value("610.57.04"), Value::Unusable);
        assert_eq!(value("latest"), Value::Unusable);
        assert_eq!(value("610.88\u{e9}"), Value::Unusable);
        assert_eq!(value("\u{3610}.88"), Value::Unusable);
        // Without a terminator, and with data after it.
        let (data, n) = sz("610.88");
        assert_eq!(
            Value::from_registry(REG_SZ, &data[..n - 2]),
            Value::Release(R61088)
        );
        let mut tail = data;
        tail[n..n + 2].copy_from_slice(&u16::from(b'9').to_le_bytes());
        assert_eq!(
            Value::from_registry(REG_SZ, &tail[..n + 2]),
            Value::Release(R61088)
        );
    }

    #[test]
    fn another_type_or_an_oversized_value_is_unusable() {
        // A DWORD 61088, what an earlier package wrote.
        assert_eq!(
            Value::from_registry(4, &61088u32.to_le_bytes()),
            Value::Unusable
        );
        let (data, n) = sz("610.88");
        for other in [0, 2, 3, 7, 11] {
            assert_eq!(Value::from_registry(other, &data[..n]), Value::Unusable);
        }
        let (long, n) = sz("                                610.88");
        assert!(n > MAX_VALUE_BYTES);
        assert_eq!(Value::from_registry(REG_SZ, &long[..n]), Value::Unusable);
        let (fits, n) = sz("                         610.88");
        assert_eq!(n, MAX_VALUE_BYTES);
        assert_eq!(
            Value::from_registry(REG_SZ, &fits[..n]),
            Value::Release(R61088)
        );
    }

    #[test]
    fn the_service_key_overrides_the_inf_value() {
        let inf = || value("610.88");
        let other = Release {
            major: 615,
            minor: 71,
        };
        assert_eq!(configured(Value::Absent, inf), Value::Release(R61088));
        assert_eq!(
            configured(Value::Release(other), inf),
            Value::Release(other)
        );
        assert_eq!(configured(Value::Unusable, inf), Value::Unusable);
        assert_eq!(configured(Value::Absent, || Value::Absent), Value::Absent);
        // The INF's value is not read when the override holds one.
        assert_eq!(
            configured(Value::Release(other), || unreachable!()),
            Value::Release(other)
        );
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

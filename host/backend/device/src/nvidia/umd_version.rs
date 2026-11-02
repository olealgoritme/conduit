//! NVIDIA's guest UMDs against the host's RM: one ABI, one release.
//!
//! In the NVIDIA UMD personality the guest runs NVIDIA's own Windows
//! user-mode drivers, and their RM controls reach the host's RM with the
//! parameter layouts of the release they were built with. RM checks no
//! version on that path (the guest KMD answers `NV_ESC_CHECK_VERSION_STR` with
//! the host's own string), so a mismatched pair does not fail at start: it
//! fails later, as a control RM reads with the wrong layout.
//!
//! So the guest KMD reports, at every StartDevice, the release of the UMDs it
//! was installed with (`MsgType::UmdVersion`), and the backend compares:
//!
//! 1. `CONDUIT_NVIDIA_UMD_VERSION` pins it: `610.88` accepts exactly that
//!    release, `any` turns the check off.
//! 2. Otherwise the host release must be in [`PAIRS`], and the UMDs must be
//!    the release paired with it. Windows and Linux packages of one branch
//!    are numbered differently (Windows `610.88` and Linux `610.57.04` are one
//!    RM ABI), so the pairing is a list of releases run together, not a rule.
//! 3. A host release with no pair is accepted for UMDs of its branch (`610`),
//!    with a warning that the release itself is not checked.
//!
//! A mismatch is one error line on the host, a verdict the guest KMD fails its
//! start with, and from then on every RM call from this guest refused, so a
//! guest that ignores the verdict is not served either.

use abi::version::DriverVersion;

/// The environment variable that pins the accepted UMD release.
pub const ENV_PIN: &str = "CONDUIT_NVIDIA_UMD_VERSION";

/// A Windows NVIDIA driver release, `610.88`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WinVersion {
    pub major: u32,
    pub minor: u32,
}

impl WinVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// `610.88`.
    pub fn parse(s: &str) -> Option<Self> {
        let (a, b) = s.trim().split_once('.')?;
        let num = |p: &str| {
            (!p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        };
        Some(Self::new(num(a)?, num(b)?))
    }
}

impl std::fmt::Display for WinVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

/// What `CONDUIT_NVIDIA_UMD_VERSION` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    Exact(WinVersion),
    Any,
}

impl Pin {
    pub fn parse(s: &str) -> Option<Self> {
        if s.trim().eq_ignore_ascii_case("any") {
            return Some(Self::Any);
        }
        WinVersion::parse(s).map(Self::Exact)
    }
}

/// Host RM releases and the Windows UMD release run on each.
pub static PAIRS: &[(DriverVersion, WinVersion)] =
    &[(DriverVersion::new(610, 57, 4), WinVersion::new(610, 88))];

/// The comparison's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Match,
    /// Not the release the host accepts. `want` is the one it does, when
    /// there is exactly one.
    Mismatch {
        want: Option<WinVersion>,
    },
    /// Nothing to compare with, or the check is off.
    Unchecked,
    /// No pair for the host release; the branch matches.
    Branch,
}

impl Verdict {
    pub const fn wire(self) -> u32 {
        use protocol::messages::*;
        match self {
            Self::Match => UMD_VERDICT_MATCH,
            Self::Mismatch { .. } => UMD_VERDICT_MISMATCH,
            Self::Unchecked => UMD_VERDICT_UNCHECKED,
            Self::Branch => UMD_VERDICT_BRANCH,
        }
    }
}

pub fn judge(host: Option<DriverVersion>, pin: Option<Pin>, guest: WinVersion) -> Verdict {
    match pin {
        Some(Pin::Any) => return Verdict::Unchecked,
        Some(Pin::Exact(want)) if want == guest => return Verdict::Match,
        Some(Pin::Exact(want)) => return Verdict::Mismatch { want: Some(want) },
        None => {}
    }
    let Some(host) = host else {
        return Verdict::Unchecked;
    };
    if let Some((_, want)) = PAIRS.iter().find(|(h, _)| *h == host) {
        return if *want == guest {
            Verdict::Match
        } else {
            Verdict::Mismatch { want: Some(*want) }
        };
    }
    if guest.major == host.major {
        Verdict::Branch
    } else {
        Verdict::Mismatch { want: None }
    }
}

/// What the guest reported and what was decided, for this boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub guest: WinVersion,
    pub verdict: Verdict,
}

impl Report {
    /// Whether RM calls are refused because of it.
    pub fn refuses(&self) -> bool {
        matches!(self.verdict, Verdict::Mismatch { .. })
    }
}

impl super::NvidiaBackend {
    /// Pin the accepted UMD release (`CONDUIT_NVIDIA_UMD_VERSION`).
    pub fn set_umd_pin(&mut self, pin: Option<Pin>) {
        self.start.umd_pin = pin;
    }

    /// `MsgType::UmdVersion`: compare, remember, answer.
    pub(super) fn handle_umd_version(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        use super::Status;
        use protocol::messages::*;
        let Some(req) = UmdVersion::from_bytes(payload) else {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, libc::EINVAL);
        };
        if req.kind != UMD_KIND_NVIDIA_WINDOWS || req.reserved != 0 {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, 0, libc::EINVAL);
        }
        let guest = WinVersion::new(req.major, req.minor);
        let host = self.start.driver;
        let verdict = judge(host, self.start.umd_pin, guest);
        let host_text = host.map_or_else(|| "unknown".to_string(), |v| v.to_string());
        // Logged once per change: every adapter start reports again.
        let same = self.umd_report == Some(Report { guest, verdict });
        match verdict {
            _ if same => log::debug!(
                "NVIDIA UMD release check: guest UMDs {guest}, host RM {host_text}, unchanged"
            ),
            Verdict::Match => {
                log::info!("NVIDIA UMD release check: guest UMDs {guest} match host RM {host_text}")
            }
            Verdict::Branch => log::warn!(
                "NVIDIA UMD release check: no UMD release is paired with host RM {host_text}; \
                 guest UMDs {guest} are from its branch, the release itself is not checked \
                 (set {ENV_PIN}=<release> to pin one)"
            ),
            Verdict::Unchecked => log::warn!(
                "NVIDIA UMD release check: not checked (host RM {host_text}, guest UMDs {guest}{})",
                if self.start.umd_pin == Some(Pin::Any) {
                    format!(", {ENV_PIN}=any")
                } else {
                    String::new()
                }
            ),
            Verdict::Mismatch { want } => log::error!(
                "NVIDIA UMD release MISMATCH: the guest's NVIDIA UMDs are {guest}, host RM {host_text} \
                 needs {}; the guest's adapter fails to start and every RM call from it is \
                 refused until it reports a matching release",
                want.map_or_else(
                    || format!("UMDs of the {} branch", host.map_or(0, |v| v.major)),
                    |w| w.to_string()
                )
            ),
        }
        self.umd_report = Some(Report { guest, verdict });
        let reply = UmdVersionReply {
            verdict: verdict.wire(),
            host_major: host.map_or(0, |v| v.major),
            host_minor: host.map_or(0, |v| v.minor),
            host_patch: host.map_or(0, |v| v.patch),
        };
        let n = self.write_hdr(resp_buf, 0, 0);
        let body = reply.to_bytes();
        if resp_buf.len() < n + body.len() {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        resp_buf[n..n + body.len()].copy_from_slice(&body);
        n + body.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: DriverVersion = DriverVersion::new(610, 57, 4);

    #[test]
    fn the_paired_release_matches_and_no_other() {
        assert_eq!(
            judge(Some(HOST), None, WinVersion::new(610, 88)),
            Verdict::Match
        );
        assert_eq!(
            judge(Some(HOST), None, WinVersion::new(610, 47)),
            Verdict::Mismatch {
                want: Some(WinVersion::new(610, 88))
            }
        );
        assert!(matches!(
            judge(Some(HOST), None, WinVersion::new(615, 88)),
            Verdict::Mismatch { .. }
        ));
    }

    #[test]
    fn an_unpaired_host_checks_the_branch() {
        let host = DriverVersion::new(615, 78, 8);
        assert_eq!(
            judge(Some(host), None, WinVersion::new(615, 12)),
            Verdict::Branch
        );
        assert_eq!(
            judge(Some(host), None, WinVersion::new(610, 88)),
            Verdict::Mismatch { want: None }
        );
    }

    #[test]
    fn a_pin_overrides_the_pairs() {
        let pin = Pin::parse("610.47");
        assert_eq!(pin, Some(Pin::Exact(WinVersion::new(610, 47))));
        assert_eq!(
            judge(Some(HOST), pin, WinVersion::new(610, 47)),
            Verdict::Match
        );
        assert!(matches!(
            judge(Some(HOST), pin, WinVersion::new(610, 88)),
            Verdict::Mismatch { .. }
        ));
        assert_eq!(
            judge(Some(HOST), Pin::parse("ANY"), WinVersion::new(1, 1)),
            Verdict::Unchecked
        );
        assert_eq!(Pin::parse("610"), None);
        assert_eq!(Pin::parse("610.x"), None);
    }

    #[test]
    fn without_a_host_release_nothing_is_decided() {
        assert_eq!(
            judge(None, None, WinVersion::new(610, 88)),
            Verdict::Unchecked
        );
    }

    #[test]
    fn releases_print_as_nvidia_numbers_them() {
        assert_eq!(WinVersion::new(610, 88).to_string(), "610.88");
        assert_eq!(WinVersion::new(581, 8).to_string(), "581.08");
        assert_eq!(WinVersion::parse("581.08"), Some(WinVersion::new(581, 8)));
    }
}

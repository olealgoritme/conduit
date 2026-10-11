// SPDX-License-Identifier: Apache-2.0
//
// How the guest's GPU work is scheduled against the host's own.
//
// The guest's channel groups (TSGs, `KEPLER_CHANNEL_GROUP_A`) are created by
// this process on the host's RM, so they share the GPU's runlist with the
// host's display path: the compositor's composite and the viewer's scale
// pass. RM schedules TSGs on a runlist by interleave level and timeslice
// (`ctrla06c.h`):
//
// * Interleave level. Every TSG starts at MEDIUM (`kchangrpapiConstruct`).
//   A LOW TSG appears once per runlist pass; each MEDIUM TSG appears once per
//   LOW TSG. With the guest's TSGs at LOW, a host TSG with work waits behind
//   one guest timeslice instead of one per guest TSG. RM marks
//   `NVA06C_CTRL_CMD_SET_INTERLEAVE_LEVEL` as needing the NICE access right,
//   which the Linux driver grants on `capable(CAP_SYS_NICE)`
//   (`os_check_access`), in either direction.
// * Timeslice. 1024 us by default (`kfifoChannelGroupGetDefaultTimeslice`,
//   128 << 3). `NVA06C_CTRL_CMD_SET_TIMESLICE` is non-privileged and takes
//   effect immediately; a shorter guest timeslice bounds how long a host TSG
//   with work waits for the GPU, at the cost of more context switches when
//   both have work.
//
// The guest may call SET_TIMESLICE on its own TSGs (it is on the RM
// allowlist). With a timeslice policy set, a guest request is clamped to it,
// so the guest cannot undo the policy by asking for a longer one.
//
//   CONDUIT_GUEST_TSG_INTERLEAVE  low (default) | medium | off
//   CONDUIT_GUEST_TSG_TIMESLICE_US  off (default) | 50..=1024

/// `KEPLER_CHANNEL_GROUP_A`: the TSG class on every generation RM supports.
pub const TSG_CLASS: u32 = 0xa06c;
pub const CMD_SET_TIMESLICE: u32 = 0xa06c_0103;
pub const CMD_GET_TIMESLICE: u32 = 0xa06c_0104;
pub const CMD_SET_INTERLEAVE_LEVEL: u32 = 0xa06c_0107;
pub const CMD_GET_INTERLEAVE_LEVEL: u32 = 0xa06c_0108;
pub const INTERLEAVE_LOW: u32 = 0;
pub const INTERLEAVE_MEDIUM: u32 = 1;

/// Bounds on a timeslice policy. RM rounds down to what the hardware takes;
/// below ~50 us the context-switch cost dominates, and above RM's default the
/// policy would raise the guest's share rather than lower it.
pub const TIMESLICE_MIN_US: u64 = 50;
pub const TIMESLICE_MAX_US: u64 = 1024;

/// The policy for the guest's TSGs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestSched {
    /// Interleave level to set on each guest TSG, `None` to leave RM's.
    pub interleave: Option<u32>,
    /// Timeslice to set on each guest TSG and to clamp guest requests to,
    /// `None` to leave RM's and pass guest requests through.
    pub timeslice_us: Option<u64>,
}

impl Default for GuestSched {
    fn default() -> Self {
        Self {
            interleave: Some(INTERLEAVE_LOW),
            timeslice_us: None,
        }
    }
}

impl GuestSched {
    /// No changes at all: RM's defaults, guest requests passed through.
    pub const NONE: GuestSched = GuestSched {
        interleave: None,
        timeslice_us: None,
    };

    /// From the two variables' values. Returns the policy and a warning for
    /// each value that was not understood (which then takes its default).
    pub fn parse(interleave: Option<&str>, timeslice: Option<&str>) -> (Self, Vec<String>) {
        let mut s = Self::default();
        let mut warn = Vec::new();
        match interleave.map(str::trim) {
            None | Some("") => {}
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "low" | "0" => s.interleave = Some(INTERLEAVE_LOW),
                "medium" | "1" => s.interleave = Some(INTERLEAVE_MEDIUM),
                "off" | "none" | "default" => s.interleave = None,
                _ => warn.push(format!(
                    "CONDUIT_GUEST_TSG_INTERLEAVE={v:?} is not low|medium|off; using low"
                )),
            },
        }
        match timeslice.map(str::trim) {
            None | Some("") => {}
            Some(v) if v.eq_ignore_ascii_case("off") || v == "0" => s.timeslice_us = None,
            Some(v) => match v.parse::<u64>() {
                Ok(us) if (TIMESLICE_MIN_US..=TIMESLICE_MAX_US).contains(&us) => {
                    s.timeslice_us = Some(us)
                }
                _ => warn.push(format!(
                    "CONDUIT_GUEST_TSG_TIMESLICE_US={v:?} is not off or \
                     {TIMESLICE_MIN_US}..={TIMESLICE_MAX_US}; leaving RM's timeslice"
                )),
            },
        }
        (s, warn)
    }

    /// From the environment, read once per process (warnings logged then).
    pub fn from_env() -> Self {
        static ONCE: std::sync::OnceLock<GuestSched> = std::sync::OnceLock::new();
        *ONCE.get_or_init(|| {
            let il = std::env::var("CONDUIT_GUEST_TSG_INTERLEAVE").ok();
            let ts = std::env::var("CONDUIT_GUEST_TSG_TIMESLICE_US").ok();
            let (s, warn) = Self::parse(il.as_deref(), ts.as_deref());
            for w in warn {
                log::warn!("{w}");
            }
            log::info!("guest TSG scheduling: {}", s.describe());
            s
        })
    }

    /// Whether the policy needs CAP_SYS_NICE at the moment it is applied.
    pub fn wants_nice(&self) -> bool {
        self.interleave.is_some()
    }

    /// A guest's SET_TIMESLICE request, as it should reach RM.
    pub fn clamp_guest_timeslice(&self, requested_us: u64) -> u64 {
        match self.timeslice_us {
            Some(max) => requested_us.min(max),
            None => requested_us,
        }
    }

    pub fn describe(&self) -> String {
        let il = match self.interleave {
            Some(INTERLEAVE_LOW) => "LOW".to_string(),
            Some(INTERLEAVE_MEDIUM) => "MEDIUM".to_string(),
            Some(n) => format!("{n}"),
            None => "RM default".to_string(),
        };
        let ts = match self.timeslice_us {
            Some(us) => format!("{us} us (guest requests clamped to it)"),
            None => "RM default".to_string(),
        };
        format!("interleave {il}, timeslice {ts}")
    }
}

/// CAP_SYS_NICE's bit (linux/capability.h).
pub const CAP_SYS_NICE: u32 = 23;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const CAP_V3: u32 = 0x2008_0522;

fn capget() -> Option<[CapData; 2]> {
    let mut header = CapHeader {
        version: CAP_V3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: capget writes one header and two data words, all live here.
    let r = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut CapHeader,
            data.as_mut_ptr(),
        )
    };
    (r == 0).then_some(data)
}

fn capset(data: &[CapData; 2]) -> bool {
    let header = CapHeader {
        version: CAP_V3,
        pid: 0,
    };
    // SAFETY: capset reads one header and two data words, all live here; pid
    // 0 is this thread.
    unsafe { libc::syscall(libc::SYS_capset, &header as *const CapHeader, data.as_ptr()) == 0 }
}

/// Whether this thread holds CAP_SYS_NICE in its permitted set (and so can
/// raise it for a call).
pub fn nice_permitted() -> bool {
    capget().is_some_and(|d| d[0].permitted & (1 << CAP_SYS_NICE) != 0)
}

/// CAP_SYS_NICE raised in this thread's effective set for as long as the
/// guard lives, when it is in the permitted set. The backend keeps it
/// permitted but not effective (see `posture`), so the guest's own ioctls,
/// which run with the effective set empty, never carry it; only the
/// backend's own scheduling calls do.
pub struct NiceRaised {
    raised: bool,
}

impl NiceRaised {
    pub fn raise() -> Self {
        let Some(mut d) = capget() else {
            return Self { raised: false };
        };
        let bit = 1u32 << CAP_SYS_NICE;
        if d[0].permitted & bit == 0 || d[0].effective & bit != 0 {
            return Self { raised: false };
        }
        d[0].effective |= bit;
        Self { raised: capset(&d) }
    }

    pub fn raised(&self) -> bool {
        self.raised
    }
}

impl Drop for NiceRaised {
    fn drop(&mut self) {
        if !self.raised {
            return;
        }
        if let Some(mut d) = capget() {
            d[0].effective &= !(1u32 << CAP_SYS_NICE);
            if !capset(&d) {
                log::error!("could not lower CAP_SYS_NICE after a TSG scheduling call");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_low_interleave_and_rm_timeslice() {
        let (s, w) = GuestSched::parse(None, None);
        assert!(w.is_empty());
        assert_eq!(s.interleave, Some(INTERLEAVE_LOW));
        assert_eq!(s.timeslice_us, None);
        assert!(s.wants_nice());
    }

    #[test]
    fn values_parse() {
        let (s, w) = GuestSched::parse(Some("off"), Some("250"));
        assert!(w.is_empty());
        assert_eq!(
            s,
            GuestSched {
                interleave: None,
                timeslice_us: Some(250)
            }
        );
        assert!(!s.wants_nice());
        let (s, _) = GuestSched::parse(Some("MEDIUM"), Some("off"));
        assert_eq!(s.interleave, Some(INTERLEAVE_MEDIUM));
        assert_eq!(s.timeslice_us, None);
    }

    #[test]
    fn bad_values_warn_and_keep_defaults() {
        let (s, w) = GuestSched::parse(Some("high"), Some("5000"));
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(s, GuestSched::default());
        let (s, w) = GuestSched::parse(None, Some("10"));
        assert_eq!(w.len(), 1);
        assert_eq!(s.timeslice_us, None);
    }

    #[test]
    fn guest_timeslice_is_clamped_only_under_a_policy() {
        let (s, _) = GuestSched::parse(None, Some("250"));
        assert_eq!(s.clamp_guest_timeslice(100_000), 250);
        assert_eq!(s.clamp_guest_timeslice(100), 100);
        assert_eq!(
            GuestSched::default().clamp_guest_timeslice(100_000),
            100_000
        );
    }
}

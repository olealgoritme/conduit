// SPDX-License-Identifier: Apache-2.0
//
// How long the guest's GPU work may hold the GPU before the host's own gets a
// turn.
//
// The guest's channel groups (TSGs, `KEPLER_CHANNEL_GROUP_A`) are created by
// this process on the host's RM, so they share the GPU's runlist with the
// host's display path: the compositor's composite and the viewer's scale
// pass. RM gives each TSG on a runlist a timeslice, 1024 us by default
// (`kfifoChannelGroupGetDefaultTimeslice`, 128 << 3). A host TSG with work
// waits for the busy guest TSG ahead of it to use up its slice, so with a
// game keeping the GPU busy, every host frame can sit up to a full slice per
// guest TSG before it is drawn.
//
// The backend shortens the guest's slice: after each guest TSG is allocated
// it calls `NVA06C_CTRL_CMD_SET_TIMESLICE` (`ctrla06c.h`; non-privileged,
// takes effect immediately) on it, as its own control on the same client.
// When only the guest has work it keeps the GPU slice after slice, so its
// throughput is unchanged; when both have work, the host's waits at most one
// short slice, at the cost of more context switches.
//
// The guest may call SET_TIMESLICE on its own TSGs (it is on the RM
// allowlist). Under a policy its request is clamped to the policy, so the
// guest cannot undo it by asking for a longer slice; a shorter one passes.
//
// The setting, in order of precedence:
//   CONDUIT_GUEST_TSG_TIMESLICE_US   off | 0 | 50..=1024 (an override)
//   --gpu-timeslice-us               off | 0 | 50..=1024 (default 100;
//                                    `conduit config backend.gpu_timeslice_us`)
// `off` and `0` leave RM's timeslice and pass guest requests through.
// docs/HOST-TUNING.md, "GPU time".

/// `KEPLER_CHANNEL_GROUP_A`: the TSG class on every generation RM supports.
pub const TSG_CLASS: u32 = 0xa06c;
pub const CMD_SET_TIMESLICE: u32 = 0xa06c_0103;
pub const CMD_GET_TIMESLICE: u32 = 0xa06c_0104;

/// Bounds on a timeslice policy. RM rounds down to what the hardware takes;
/// below ~50 us the context-switch cost dominates, and above RM's default the
/// policy would raise the guest's share rather than lower it.
pub const TIMESLICE_MIN_US: u64 = 50;
pub const TIMESLICE_MAX_US: u64 = 1024;
/// The default policy.
pub const TIMESLICE_DEFAULT_US: u64 = 100;

/// The variable that overrides `--gpu-timeslice-us`.
pub const ENV: &str = "CONDUIT_GUEST_TSG_TIMESLICE_US";

/// The policy for the guest's TSGs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestSched {
    /// Timeslice to set on each guest TSG and to clamp guest requests to,
    /// `None` to leave RM's and pass guest requests through.
    pub timeslice_us: Option<u64>,
}

impl Default for GuestSched {
    fn default() -> Self {
        Self {
            timeslice_us: Some(TIMESLICE_DEFAULT_US),
        }
    }
}

impl std::str::FromStr for GuestSched {
    type Err = String;

    /// `off`, `0` or `default` (RM's timeslice), or 50..=1024 microseconds.
    fn from_str(v: &str) -> Result<Self, String> {
        let v = v.trim();
        if v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("default") || v == "0" {
            return Ok(Self::NONE);
        }
        match v.parse::<u64>() {
            Ok(us) if (TIMESLICE_MIN_US..=TIMESLICE_MAX_US).contains(&us) => Ok(Self {
                timeslice_us: Some(us),
            }),
            _ => Err(format!(
                "{v:?} is not off, 0 or {TIMESLICE_MIN_US}..={TIMESLICE_MAX_US} (microseconds)"
            )),
        }
    }
}

impl GuestSched {
    /// No changes at all: RM's timeslice, guest requests passed through.
    pub const NONE: GuestSched = GuestSched { timeslice_us: None };

    /// The effective policy: `env` (the override variable's value) when it
    /// is set and parses, otherwise `arg`. Returns where it came from, and a
    /// warning when `env` was set but not understood.
    pub fn resolve(arg: Self, env: Option<&str>) -> (Self, &'static str, Option<String>) {
        match env.map(str::trim).filter(|v| !v.is_empty()) {
            None => (arg, "--gpu-timeslice-us", None),
            Some(v) => match v.parse::<Self>() {
                Ok(s) => (s, ENV, None),
                Err(e) => (
                    arg,
                    "--gpu-timeslice-us",
                    Some(format!("{ENV}: {e}; ignored")),
                ),
            },
        }
    }

    /// [`resolve`](Self::resolve) against the environment, with its one log
    /// line (and the warning, if any).
    pub fn effective(arg: Self) -> Self {
        let env = std::env::var(ENV).ok();
        let (s, from, warn) = Self::resolve(arg, env.as_deref());
        if let Some(w) = warn {
            log::warn!("{w}");
        }
        log::info!("guest GPU timeslice: {} (from {from})", s.describe());
        s
    }

    /// A guest's SET_TIMESLICE request, as it should reach RM.
    pub fn clamp_guest_timeslice(&self, requested_us: u64) -> u64 {
        match self.timeslice_us {
            Some(max) => requested_us.min(max),
            None => requested_us,
        }
    }

    pub fn describe(&self) -> String {
        match self.timeslice_us {
            Some(us) => format!("{us} us per guest TSG, guest requests clamped to it"),
            None => "RM default, guest requests passed through".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_100_us() {
        assert_eq!(GuestSched::default().timeslice_us, Some(100));
    }

    #[test]
    fn values_parse() {
        for off in ["off", "OFF", "0", "default", " off "] {
            assert_eq!(off.parse::<GuestSched>(), Ok(GuestSched::NONE), "{off}");
        }
        for (v, us) in [("50", 50), ("100", 100), ("250", 250), ("1024", 1024)] {
            assert_eq!(v.parse::<GuestSched>().unwrap().timeslice_us, Some(us));
        }
        for bad in ["", "10", "49", "1025", "5000", "fast", "-1"] {
            assert!(bad.parse::<GuestSched>().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_variable_overrides_the_argument() {
        let arg = GuestSched::default();
        assert_eq!(
            GuestSched::resolve(arg, None),
            (arg, "--gpu-timeslice-us", None)
        );
        assert_eq!(
            GuestSched::resolve(arg, Some("")),
            (arg, "--gpu-timeslice-us", None)
        );
        let (s, from, w) = GuestSched::resolve(arg, Some("250"));
        assert_eq!((s.timeslice_us, from, w), (Some(250), ENV, None));
        let (s, from, w) = GuestSched::resolve(arg, Some("off"));
        assert_eq!((s, from, w), (GuestSched::NONE, ENV, None));
        // Not understood: the argument stands, with a warning.
        let (s, from, w) = GuestSched::resolve(GuestSched::NONE, Some("5000"));
        assert_eq!((s, from), (GuestSched::NONE, "--gpu-timeslice-us"));
        assert!(w.is_some_and(|w| w.contains(ENV)));
    }

    #[test]
    fn guest_timeslice_is_clamped_only_under_a_policy() {
        let s: GuestSched = "250".parse().unwrap();
        assert_eq!(s.clamp_guest_timeslice(100_000), 250);
        assert_eq!(s.clamp_guest_timeslice(100), 100);
        assert_eq!(GuestSched::NONE.clamp_guest_timeslice(100_000), 100_000);
    }
}

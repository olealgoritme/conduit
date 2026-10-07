//! Host-feedback display pacing: the pure half. DESIGN SCAFFOLDING, wired into nothing.
//!
//! Spec: `docs/host-vblank-pacing.md`. Today the KMD makes `DXGK_INTERRUPT_CRTC_VSYNC` from a
//! one-shot timer (`vsync_deadline`, `adapter/kobj.rs` `service_vsync_tick`) that knows nothing
//! about when the host's viewer really presents, so the guest's vblank beats against the host's
//! scanout. The host can tell the guest when it presented (a new event on virtio queue 1, message
//! [`MSG_HOST_VBLANK`], feature [`FEATURE`]); this module is everything the KMD would do with that
//! message that is arithmetic: parse it, turn it into phase observations, filter them
//! ([`Pll`]), decide whether the host is to be believed (the [`Mode`] state machine) and compute
//! the next timer deadline from the filtered lattice ([`Pll::next_deadline`]) under the same
//! invariants as `vsync_deadline::next`.
//!
//! Nothing in `kmd_render` calls anything here. Pure functions of their arguments; no allocation,
//! no 128-bit arithmetic (the kernel build has none today), no floating point.
//!
//! # Units and clocks
//!
//! * Guest time is the interrupt time, `u64` in 100 ns units (what `KeQueryInterruptTimePrecise`
//!   returns and what `vsync_deadline` uses). Every comparison is a wrapping difference read as
//!   `i64` ([`diff`]), so a timeline that wraps `u64` still orders correctly; nothing here
//!   compares two guest times with `<`.
//! * Host times are `u64` nanoseconds of one host clock (the viewer's presentation clock converted
//!   to `CLOCK_MONOTONIC`). The guest NEVER relates that clock to its own: an event carries both
//!   the presentation instant and the instant the event was built (`sent_ns`), so
//!   `age = sent_ns - present_ns` is a host-clock difference with no offset in it, and the guest
//!   places the presentation at `arrive - age`. What is left (host send to guest DPC) is a
//!   positive, jittery delay, which the filter treats as one-sided. Its minimum is an unknowable
//!   constant that folds into the phase (the `lead` knob), so no host-to-guest clock offset is ever
//!   estimated; see the design doc, section 4.3.
//! * The period is carried as `u64` 100 ns units shifted left by [`Q_SHIFT`] (16 fractional bits)
//!   so that 240 Hz (41666.67 units) does not carry the 8 ppm rounding error of `period_100ns`.

use crate::vsync_deadline;

// ---- the wire message ---------------------------------------------------------------

/// Proposed host `MsgType::HostVblank`: event queue (virtqueue 1), host to guest. 29 is skipped in
/// the host's enum with no recorded owner, so the next unambiguous value after
/// `RmResourceImport` (31) is used. The host session may renumber; the guest constant is the only
/// other place.
pub const MSG_HOST_VBLANK: u32 = 32;
/// Proposed device feature bit (`NVGPU_F_HOST_VBLANK`, virtio feature bit 17): the next free bit
/// after `NVGPU_F_SCANOUT_RELEASE` (15). Bit 12 is `TAKES_INPUT` (never acked by this driver); the
/// config `features` word is a different space (its bits 8..14 are used, 15 is skipped).
pub const FEATURE_BIT: u32 = 17;
/// [`FEATURE_BIT`] as a mask over the 64-bit virtio feature word.
pub const FEATURE: u64 = 1 << FEATURE_BIT;
/// The wire layout's version. A different version is dropped and counted, not guessed at.
pub const WIRE_VERSION: u16 = 1;
/// `MsgHeader` bytes before the body (`{type, handle, status, reserved}`).
pub const HEADER_BYTES: usize = 16;
/// Fixed body bytes, before the samples.
pub const BODY_FIXED_BYTES: usize = 48;
/// One sample: `{u64 present_ns, u64 msc}`.
pub const SAMPLE_BYTES: usize = 16;
/// Most samples one event carries (a posted buffer is 256 bytes: 16 + 48 + 8 * 16 = 192).
pub const MAX_SAMPLES: usize = 8;
/// The longest valid message.
pub const MAX_MSG_BYTES: usize = HEADER_BYTES + BODY_FIXED_BYTES + MAX_SAMPLES * SAMPLE_BYTES;

/// Every sample is a hardware-vblank-synchronised presentation (`wp_presentation` `VSYNC`, X11
/// Present `COMPLETE_MODE_FLIP`/`COPY` with a vblank msc). Without it a sample says nothing about
/// the host's retrace (a tearing `ASYNC` present) and is not used for phase.
pub const F_VSYNC: u32 = 1 << 0;
/// The timestamps come from the display hardware's vblank clock (`HW_CLOCK`). Informational.
pub const F_HW_CLOCK: u32 = 1 << 1;
/// The presentation completed in hardware (`HW_COMPLETION`). Informational.
pub const F_HW_COMPLETION: u32 = 1 << 2;
/// The frame was scanned out directly (`ZERO_COPY`). Informational.
pub const F_ZERO_COPY: u32 = 1 << 3;
/// First event after the host (re)started feedback: a mode or output change, a viewer that
/// returned from idle, a gap of more than 100 ms. The guest restarts its filter.
pub const F_FIRST: u32 = 1 << 4;
/// The host stops sending until further notice (window minimised, occluded, viewer gone): no
/// samples (`count` 0); the guest drops to the timer at once instead of waiting out the holdover.
pub const F_IDLE: u32 = 1 << 5;
/// More presentations happened than `count` samples carry (`lost` says how many).
pub const F_COALESCED: u32 = 1 << 6;
/// Every flag this KMD knows; others are ignored, not refused.
pub const FLAGS_KNOWN: u32 =
    F_VSYNC | F_HW_CLOCK | F_HW_COMPLETION | F_ZERO_COPY | F_FIRST | F_IDLE | F_COALESCED;
/// A presentation older than this when the event was built is not a phase observation.
pub const MAX_AGE_NS: u64 = 1_000_000_000;

/// One presentation as the host reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawSample {
    /// The host clock's instant the frame reached the screen.
    pub present_ns: u64,
    /// The compositor's vblank counter for it, 0 when unknown.
    pub msc: u64,
}

/// A parsed `HostVblank`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub scanout: u32,
    pub flags: u32,
    /// The output's nominal refresh in millihertz as the compositor reports it, 0 unknown. A
    /// hint for the sanity check; the period is measured, not taken from here.
    pub refresh_mhz: u32,
    /// The host clock when the event was built.
    pub sent_ns: u64,
    /// `ScanoutFlip.seq` of the flip the newest sample presented, 0 when none or unknown.
    pub seq: u64,
    /// Presentations since the previous event that no sample carries.
    pub lost: u32,
    count: usize,
    samples: [RawSample; MAX_SAMPLES],
}

/// What [`parse`] made of a used buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    Event(Event),
    /// Some other message type (or fewer than 16 bytes).
    NotHostVblank,
    /// A `HostVblank` too short for its own header, fixed body or samples.
    Short,
    /// A version this KMD does not speak.
    Version(u16),
    /// `count` above [`MAX_SAMPLES`], or 0 without [`F_IDLE`].
    BadCount(u16),
}

impl Event {
    /// Samples in the event.
    pub const fn count(&self) -> usize {
        self.count
    }
    pub const fn is_idle(&self) -> bool {
        self.flags & F_IDLE != 0
    }
    pub const fn is_first(&self) -> bool {
        self.flags & F_FIRST != 0
    }
    /// Sample `i` as a phase observation for an event consumed at guest time `arrive`.
    /// `Err(Reject::Old)` for a presentation that is in the event's future (a host clock
    /// conversion error) or older than [`MAX_AGE_NS`].
    pub fn sample(&self, i: usize, arrive: u64) -> Result<Sample, Reject> {
        let raw = *self
            .samples
            .get(i)
            .filter(|_| i < self.count)
            .ok_or(Reject::Old)?;
        let age_ns = self.sent_ns.wrapping_sub(raw.present_ns) as i64;
        if age_ns < 0 || age_ns as u64 > MAX_AGE_NS {
            return Err(Reject::Old);
        }
        Ok(Sample {
            arrive,
            age: age_ns as u64 / 100,
            msc: raw.msc,
            vsync: self.flags & F_VSYNC != 0,
        })
    }
}

fn rd16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

/// Read one used buffer: `bytes` is the buffer, `len` the length the device reported. The
/// header's `handle` and `status` are not checked (the contract says 0 and 0), nor is `reserved`.
pub fn parse(bytes: &[u8], len: usize) -> Parsed {
    let len = len.min(bytes.len());
    if len < HEADER_BYTES || rd32(bytes, 0) != Some(MSG_HOST_VBLANK) {
        return Parsed::NotHostVblank;
    }
    let b = HEADER_BYTES;
    if len < b + BODY_FIXED_BYTES {
        return Parsed::Short;
    }
    let (Some(version), Some(count), Some(scanout), Some(flags), Some(refresh_mhz)) = (
        rd16(bytes, b),
        rd16(bytes, b + 2),
        rd32(bytes, b + 4),
        rd32(bytes, b + 8),
        rd32(bytes, b + 12),
    ) else {
        return Parsed::Short;
    };
    let (Some(sent_ns), Some(seq), Some(lost)) = (
        rd64(bytes, b + 16),
        rd64(bytes, b + 24),
        rd32(bytes, b + 32),
    ) else {
        return Parsed::Short;
    };
    if version != WIRE_VERSION {
        return Parsed::Version(version);
    }
    let n = count as usize;
    if n > MAX_SAMPLES || (n == 0 && flags & F_IDLE == 0) {
        return Parsed::BadCount(count);
    }
    let mut samples = [RawSample::default(); MAX_SAMPLES];
    for (i, slot) in samples.iter_mut().enumerate().take(n) {
        let at = b + BODY_FIXED_BYTES + i * SAMPLE_BYTES;
        match (rd64(bytes, at), rd64(bytes, at + 8)) {
            (Some(present_ns), Some(msc)) if at + SAMPLE_BYTES <= len => {
                *slot = RawSample { present_ns, msc };
            }
            _ => return Parsed::Short,
        }
    }
    Parsed::Event(Event {
        scanout,
        flags,
        refresh_mhz,
        sent_ns,
        seq,
        lost,
        count: n,
        samples,
    })
}

// ---- time and fixed-point helpers ----------------------------------------------------

/// Fractional bits of the carried period.
pub const Q_SHIFT: u32 = 16;

/// `a - b` as a signed distance, correct across a wrap of the `u64` timeline.
pub const fn diff(a: u64, b: u64) -> i64 {
    a.wrapping_sub(b) as i64
}

/// `a + d`, wrapping.
pub const fn add(a: u64, d: i64) -> u64 {
    a.wrapping_add(d as u64)
}

/// `a / b` rounded down, `b > 0`.
const fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// `a / b` rounded to nearest (ties up), `b > 0`.
const fn round_div(a: i64, b: i64) -> i64 {
    floor_div(a + b / 2, b)
}

/// `x` reduced modulo `m` into `[-m/2, m/2)`, `m > 0`.
const fn wrap_half(x: i64, m: i64) -> i64 {
    let r = x - floor_div(x, m) * m;
    if r >= m / 2 {
        r - m
    } else {
        r
    }
}

/// The largest distance (in 100 ns, 2^46 units, about 80 days) whose shifted product stays in `i64`.
const MAX_SPAN: i64 = 1 << 46;

// ---- the filter ------------------------------------------------------------------------

/// What the KMD is following.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The free-running timer of today (`vsync_deadline::next`, nominal period). No usable host
    /// information, or not enough of it yet.
    Timer,
    /// Phase and period follow the host's presentations.
    Locked,
    /// The host went quiet: the lattice keeps running on the last phase and period (for a while),
    /// ready to relock on the first sample.
    Holdover,
}

impl Mode {
    /// The value of the `HvMode` counter.
    pub const fn code(self) -> u32 {
        match self {
            Mode::Timer => 0,
            Mode::Locked => 1,
            Mode::Holdover => 2,
        }
    }
}

/// One phase observation: the guest-time instant a host presentation is believed to have
/// happened is `arrive - age`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Guest time the event was consumed (the DPC read it), 100 ns.
    pub arrive: u64,
    /// `sent_ns - present_ns` of the host, in 100 ns.
    pub age: u64,
    /// The compositor's vblank counter, 0 unknown.
    pub msc: u64,
    /// The presentation was vblank-synchronised ([`F_VSYNC`]).
    pub vsync: bool,
}

/// Why a sample was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Not vblank-synchronised (a tearing present): no phase information.
    NotVsync,
    /// The same vblank again, or two samples less than half a period apart.
    Duplicate,
    /// Earlier than the previous sample by more than half a period (a clock stepped back, or
    /// events out of order). Three in a row reseed the filter.
    NonMonotonic,
    /// More than a quarter of a period from the lattice. Three in a row are a phase step: the
    /// lattice moves to the new phase (slewed by [`Pll::next_deadline`]).
    Outlier,
    /// Presentation in the event's future or older than [`MAX_AGE_NS`].
    Old,
}

/// What [`Pll::observe`] did with a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The first sample after a start, a restart or a reseed: the lattice was placed on it.
    Seeded,
    /// Used; the phase error it showed, 100 ns (negative: it came earlier than predicted).
    Accepted(i64),
    /// A phase step was recognised and the lattice moved to the new phase.
    Stepped,
    Rejected(Reject),
}

/// Tuning, all in 100 ns except where said. Build with [`Config::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The committed refresh's period (`vsync_deadline::period_100ns`).
    pub nominal: u64,
    /// The timer fires this long BEFORE the host presentation lattice point. `0` models a real
    /// display (vsync is the scanout); a positive lead is the knob that trades latency for the
    /// risk of a flip reaching the host after its compositor's deadline. Less than a period.
    pub lead: u64,
    /// The measured period must stay within this many parts per thousand of `nominal`, else it is
    /// clamped and the lock is refused.
    pub tol_permille: u32,
    /// Consecutive good samples from `Timer` to `Locked`.
    pub lock_samples: u32,
    /// Consecutive good samples from `Holdover` to `Locked`.
    pub relock_samples: u32,
    /// Without an accepted sample for this long `Locked` becomes `Holdover`.
    pub holdover_after: u64,
    /// Without an accepted sample for this long `Holdover` becomes `Timer`, and the next sample
    /// reseeds the phase.
    pub holdover_max: u64,
    /// Largest correction of the timer grid per tick: every gap stays within `period +- slew_max`.
    pub slew_max: u64,
}

impl Config {
    /// Defaults for a refresh period of `nominal` units: lock after 6 good samples, relock after
    /// 2, holdover after 8 periods (at least 30 ms, at most 150 ms) of silence, timer after 5 s,
    /// slew at most 1/16 period per tick, 3% rate tolerance.
    pub const fn new(nominal: u64, lead: u64) -> Config {
        let after = nominal.saturating_mul(8);
        let after = if after < 300_000 {
            300_000
        } else if after > 1_500_000 {
            1_500_000
        } else {
            after
        };
        // A lead of a whole period or more is the same instant a period earlier.
        let lead = if nominal == 0 { 0 } else { lead % nominal };
        Config {
            nominal,
            lead,
            tol_permille: 30,
            lock_samples: 6,
            relock_samples: 2,
            holdover_after: after,
            holdover_max: 50_000_000,
            slew_max: nominal / 16,
        }
    }

    /// Phase error beyond which a sample is an outlier.
    const fn outlier(&self) -> i64 {
        (self.nominal / 4) as i64
    }

    /// Phase error within which a sample counts toward the lock.
    const fn lock_window(&self) -> i64 {
        (self.nominal / 8) as i64
    }

    const fn nominal_q(&self) -> u64 {
        self.nominal << Q_SHIFT
    }
}

/// Consecutive outliers that agree with each other (within a sixteenth of a period) and are
/// therefore a phase step of the host rather than delay spikes: the lattice moves to the new phase.
/// Five is 21 ms at 240 Hz; a genuine step (an output change, a compositor restart) is rare and can
/// wait, while a run of five equal delay spikes is far rarer than a run of three.
pub const STEP_STRIKES: u32 = 5;
/// Consecutive samples earlier than their predecessor by more than half a period (the guest's
/// clock stepped back) after which the filter reseeds.
pub const BACK_STRIKES: u32 = 3;
/// Consecutive samples whose period had to be clamped after which `Locked` gives up.
pub const CLAMP_RUN_MAX: u32 = 8;
/// Phase loop gain 1/4 for an early sample (it can only be early by the delay's minimum: trusted),
/// 1/16 for a late one (it may be a delay spike).
const KP_EARLY_DIV: i64 = 4;
const KP_LATE_DIV: i64 = 16;
/// Frequency loop gain 1/64 (early), 1/256 (late), per sample, divided by the periods it spans.
const KI_EARLY_DIV: i64 = 64;
const KI_LATE_DIV: i64 = 256;

/// Counts the KMD would publish (`HvXxx`, see [`COUNTERS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub accepted: u32,
    pub not_vsync: u32,
    pub duplicate: u32,
    pub non_monotonic: u32,
    pub outlier: u32,
    pub old: u32,
    pub steps: u32,
    pub reseeds: u32,
    pub msc_mismatch: u32,
    pub clamped: u32,
    /// Entries into each mode.
    pub locks: u32,
    pub holdovers: u32,
    pub timeouts: u32,
}

/// The filter: a second-order phase-locked loop over the host's presentation lattice, with
/// one-sided (delay) noise handling, outlier rejection and the [`Mode`] state machine.
///
/// Single owner: the vsync tick runs it (events reach it through a small mailbox the DPC fills).
#[derive(Debug, Clone, Copy)]
pub struct Pll {
    mode: Mode,
    seeded: bool,
    /// A host presentation instant in guest time (the lattice's origin, rebased at every sample).
    anchor: u64,
    period_q: u64,
    last_obs: u64,
    last_msc: u64,
    last_accept: u64,
    last_err: i64,
    good: u32,
    strikes: u32,
    /// The error of the first outlier of the current run: later ones count only if they agree.
    strike_err: i64,
    back_strikes: u32,
    clamp_run: u32,
    pub stats: Stats,
}

impl Pll {
    pub const fn new(cfg: &Config) -> Pll {
        Pll {
            mode: Mode::Timer,
            seeded: false,
            anchor: 0,
            period_q: cfg.nominal_q(),
            last_obs: 0,
            last_msc: 0,
            last_accept: 0,
            last_err: 0,
            good: 0,
            strikes: 0,
            strike_err: 0,
            back_strikes: 0,
            clamp_run: 0,
            stats: Stats {
                accepted: 0,
                not_vsync: 0,
                duplicate: 0,
                non_monotonic: 0,
                outlier: 0,
                old: 0,
                steps: 0,
                reseeds: 0,
                msc_mismatch: 0,
                clamped: 0,
                locks: 0,
                holdovers: 0,
                timeouts: 0,
            },
        }
    }

    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// The filtered period in 100 ns units (rounded).
    pub const fn period(&self) -> u64 {
        (self.period_q + (1 << (Q_SHIFT - 1))) >> Q_SHIFT
    }

    /// The filtered period in 100 ns units shifted left by [`Q_SHIFT`].
    pub const fn period_q(&self) -> u64 {
        self.period_q
    }

    /// The phase error of the last accepted sample, 100 ns.
    pub const fn last_error(&self) -> i64 {
        self.last_err
    }

    /// Whether a lattice exists to follow (`Locked` or `Holdover`).
    pub const fn has_lattice(&self) -> bool {
        self.seeded && !matches!(self.mode, Mode::Timer)
    }

    /// Forget everything (a transport reset, a display mode change): `Timer`, unseeded, the
    /// nominal period.
    pub fn reset(&mut self, cfg: &Config) {
        let stats = self.stats;
        *self = Pll::new(cfg);
        self.stats = stats;
    }

    /// The nominal period changed (a mode set): restart the filter on the new rate.
    pub fn retune(&mut self, cfg: &Config) {
        self.reset(cfg);
    }

    /// The host said [`F_IDLE`]: follow the timer at once. The tick grid is not touched, so the
    /// switch is seamless.
    pub fn note_idle(&mut self) {
        if self.mode != Mode::Timer {
            self.stats.timeouts = self.stats.timeouts.wrapping_add(1);
        }
        self.mode = Mode::Timer;
        self.good = 0;
        self.seeded = false;
    }

    /// The host said [`F_FIRST`]: its feedback restarted, so the old phase is stale. The lattice
    /// keeps driving the timer (as `Holdover`) until the next sample reseeds it.
    pub fn note_first(&mut self) {
        self.good = 0;
        self.strikes = 0;
        self.back_strikes = 0;
        self.seeded = false;
        if self.mode == Mode::Locked {
            self.mode = Mode::Holdover;
            self.stats.holdovers = self.stats.holdovers.wrapping_add(1);
        }
    }

    fn seed(&mut self, cfg: &Config, obs: u64, s: &Sample, keep_period: bool) {
        self.anchor = obs;
        if !keep_period {
            self.period_q = cfg.nominal_q();
        }
        self.last_obs = obs;
        self.last_msc = s.msc;
        self.last_accept = s.arrive;
        self.last_err = 0;
        self.good = 1;
        self.strikes = 0;
        self.back_strikes = 0;
        self.clamp_run = 0;
        self.seeded = true;
    }

    /// Update the mode from the passage of time alone. Called by [`Self::next_deadline`] at every
    /// tick, and by [`Self::observe`] before it uses a sample.
    pub fn update_mode(&mut self, cfg: &Config, now: u64) -> Mode {
        if self.seeded {
            let silent = diff(now, self.last_accept);
            if self.mode == Mode::Locked && silent > cfg.holdover_after as i64 {
                self.mode = Mode::Holdover;
                // The way back is `relock_samples` good samples counted from now.
                self.good = 0;
                self.stats.holdovers = self.stats.holdovers.wrapping_add(1);
            }
            if self.mode == Mode::Holdover && silent > cfg.holdover_max as i64 {
                self.mode = Mode::Timer;
                self.good = 0;
                self.stats.timeouts = self.stats.timeouts.wrapping_add(1);
            }
        }
        self.mode
    }

    fn clamp_period(&mut self, cfg: &Config) -> bool {
        let nq = cfg.nominal_q();
        let slack = nq / 1000 * cfg.tol_permille as u64;
        let (lo, hi) = (nq - slack, nq + slack);
        if self.period_q < lo {
            self.period_q = lo;
            true
        } else if self.period_q > hi {
            self.period_q = hi;
            true
        } else {
            false
        }
    }

    /// The `n`-th lattice instant after the anchor (`n` may be negative), in guest time.
    fn point(&self, n: i64) -> Option<u64> {
        let span = n.checked_mul(self.period_q as i64)? >> Q_SHIFT;
        Some(add(self.anchor, span))
    }

    /// Feed one phase observation.
    pub fn observe(&mut self, cfg: &Config, s: Sample) -> Verdict {
        if !s.vsync {
            self.stats.not_vsync = self.stats.not_vsync.wrapping_add(1);
            return Verdict::Rejected(Reject::NotVsync);
        }
        self.update_mode(cfg, s.arrive);
        let obs = s.arrive.wrapping_sub(s.age);
        if !self.seeded {
            self.seed(cfg, obs, &s, true);
            self.clamp_period(cfg);
            return Verdict::Seeded;
        }
        // Silence for longer than the holdover: the phase memory is worthless, start over (the
        // period estimate is kept: a host does not change refresh rate by itself).
        if diff(s.arrive, self.last_accept) > cfg.holdover_max as i64 {
            self.stats.reseeds = self.stats.reseeds.wrapping_add(1);
            self.seed(cfg, obs, &s, true);
            self.mode = Mode::Timer;
            return Verdict::Seeded;
        }
        let pu = self.period() as i64;
        let dt = diff(obs, self.last_obs);
        if dt < -(pu / 2) {
            self.back_strikes += 1;
            if self.back_strikes >= BACK_STRIKES {
                self.stats.reseeds = self.stats.reseeds.wrapping_add(1);
                self.seed(cfg, obs, &s, true);
                self.mode = Mode::Timer;
                return Verdict::Seeded;
            }
            self.stats.non_monotonic = self.stats.non_monotonic.wrapping_add(1);
            self.good = 0;
            return Verdict::Rejected(Reject::NonMonotonic);
        }
        self.back_strikes = 0;
        if dt >= MAX_SPAN {
            self.stats.reseeds = self.stats.reseeds.wrapping_add(1);
            self.seed(cfg, obs, &s, true);
            self.mode = Mode::Timer;
            return Verdict::Seeded;
        }

        // How many lattice periods since the last sample: the compositor's counter when it is
        // consistent with the time between the two, else the time rounded to the period.
        let p = self.period_q as i64;
        let n_round = round_div(dt << Q_SHIFT, p);
        let n = if s.msc != 0 && self.last_msc != 0 {
            if s.msc == self.last_msc {
                self.stats.duplicate = self.stats.duplicate.wrapping_add(1);
                return Verdict::Rejected(Reject::Duplicate);
            }
            if s.msc > self.last_msc && s.msc - self.last_msc < (1 << 31) {
                let n_msc = (s.msc - self.last_msc) as i64;
                let expected = (n_msc.saturating_mul(p)) >> Q_SHIFT;
                let slack = pu / 2 + expected / 100;
                if (dt - expected).abs() <= slack {
                    n_msc
                } else {
                    self.stats.msc_mismatch = self.stats.msc_mismatch.wrapping_add(1);
                    n_round
                }
            } else {
                self.stats.msc_mismatch = self.stats.msc_mismatch.wrapping_add(1);
                n_round
            }
        } else {
            n_round
        };
        if n < 1 {
            self.stats.duplicate = self.stats.duplicate.wrapping_add(1);
            return Verdict::Rejected(Reject::Duplicate);
        }
        let Some(pred) = self.point(n) else {
            return self.step_to(cfg, obs, &s);
        };
        let e = diff(obs, pred);
        if e.abs() > cfg.outlier() {
            if self.strikes > 0 && (e - self.strike_err).abs() <= (cfg.nominal / 16) as i64 {
                self.strikes += 1;
            } else {
                self.strikes = 1;
                self.strike_err = e;
            }
            if self.strikes >= STEP_STRIKES {
                return self.step_to(cfg, obs, &s);
            }
            self.stats.outlier = self.stats.outlier.wrapping_add(1);
            // A lock is `lock_samples` GOOD samples in a row: an outlier between them (a host
            // whose rate is a ratio of ours lands on our lattice every few samples) starts the
            // count over. A mode already `Locked` is not unlocked by it.
            self.good = 0;
            return Verdict::Rejected(Reject::Outlier);
        }
        self.strikes = 0;

        // Phase: move the lattice a fraction of the error. Frequency: nudge the period by the
        // error spread over the periods it accumulated across. An early sample is trusted (the
        // delay cannot be below its minimum), a late one may be a delay spike.
        let (kp, ki) = if e < 0 {
            (KP_EARLY_DIV, KI_EARLY_DIV)
        } else {
            (KP_LATE_DIV, KI_LATE_DIV)
        };
        self.anchor = add(pred, e / kp);
        let dq = ((e << Q_SHIFT) / ki) / n;
        self.period_q = (self.period_q as i64 + dq) as u64;
        let clamped = self.clamp_period(cfg);
        if clamped {
            self.stats.clamped = self.stats.clamped.wrapping_add(1);
            self.clamp_run += 1;
        } else {
            self.clamp_run = 0;
        }
        self.last_obs = obs;
        if s.msc != 0 {
            self.last_msc = s.msc;
        }
        self.last_accept = s.arrive;
        self.last_err = e;
        self.stats.accepted = self.stats.accepted.wrapping_add(1);

        if !clamped && e.abs() <= cfg.lock_window() {
            self.good = self.good.saturating_add(1);
        } else {
            self.good = 0;
        }
        match self.mode {
            Mode::Timer if self.good >= cfg.lock_samples => {
                self.mode = Mode::Locked;
                self.stats.locks = self.stats.locks.wrapping_add(1);
            }
            Mode::Holdover if self.good >= cfg.relock_samples => {
                self.mode = Mode::Locked;
                self.stats.locks = self.stats.locks.wrapping_add(1);
            }
            Mode::Locked if self.clamp_run >= CLAMP_RUN_MAX => {
                // The host's rate is not the display's: stop following it.
                self.mode = Mode::Holdover;
                self.good = 0;
                self.stats.holdovers = self.stats.holdovers.wrapping_add(1);
            }
            _ => {}
        }
        Verdict::Accepted(e)
    }

    /// Re-anchor on a sample that fits nowhere (a phase step). Mode is kept, the lock count
    /// restarts.
    fn step_to(&mut self, cfg: &Config, obs: u64, s: &Sample) -> Verdict {
        let _ = cfg;
        self.anchor = obs;
        self.last_obs = obs;
        if s.msc != 0 {
            self.last_msc = s.msc;
        }
        self.last_accept = s.arrive;
        self.last_err = 0;
        self.strikes = 0;
        self.good = 0;
        self.stats.steps = self.stats.steps.wrapping_add(1);
        Verdict::Stepped
    }

    /// The first lattice instant strictly after `now`, minus the lead: where the timer would fire
    /// if it could jump straight to the host's phase. `None` when the lattice is not usable
    /// (unseeded, or the arithmetic would overflow).
    pub fn target_after(&self, cfg: &Config, now: u64) -> Option<u64> {
        if !self.seeded {
            return None;
        }
        let lead = cfg.lead as i64;
        let d = diff(add(now, lead), self.anchor);
        if d.abs() >= MAX_SPAN {
            return None;
        }
        let mut n = floor_div(d << Q_SHIFT, self.period_q as i64) + 1;
        let mut t = add(self.point(n)?, -lead);
        // Rounding of the fixed-point product can leave the point at or before `now`.
        while diff(t, now) <= 0 {
            n += 1;
            t = add(self.point(n)?, -lead);
        }
        Some(t)
    }

    /// The next one-shot deadline, always strictly after `now` and never closer than half a period
    /// to the previous delivered tick.
    ///
    /// * `prev`: the deadline that just fired, or 0 when arming (the same convention as the
    ///   `anchor` of `service_vsync_tick`).
    /// * `last_fire`: the interrupt time the previous tick actually ran at, 0 if none.
    ///
    /// In `Timer` this IS `vsync_deadline::next(prev or now, now, cfg.nominal)`, so the shipping
    /// behaviour is the unchanged default. In `Locked` and `Holdover` the deadline continues the
    /// grid one filtered period after `prev`, moved by at most `slew_max` toward the host's
    /// lattice (so no gap leaves `period +- slew_max`, no gap or burst appears when the lock is
    /// acquired or lost), and a tick that ran a whole period late jumps to the first lattice point
    /// after `now` (one deadline, never a catch-up burst). `None` is the same terminal "arithmetic
    /// exhausted" as `vsync_deadline::next`: leave the timer unarmed.
    pub fn next_deadline(
        &mut self,
        cfg: &Config,
        now: u64,
        prev: u64,
        last_fire: u64,
    ) -> Option<u64> {
        let mode = self.update_mode(cfg, now);
        let anchor = if prev == 0 { now } else { prev };
        let timer = || vsync_deadline::next(anchor, now, cfg.nominal);
        if mode == Mode::Timer || !self.seeded {
            return timer();
        }
        let Some(target) = self.target_after(cfg, now) else {
            return timer();
        };
        let pu = self.period() as i64;
        if pu <= 0 {
            return timer();
        }
        let mut cand = if prev == 0 {
            target
        } else {
            let natural = add(prev, pu);
            let off = wrap_half(diff(target, natural), pu);
            let step = off.clamp(-(cfg.slew_max as i64), cfg.slew_max as i64);
            add(natural, step)
        };
        // A tick that ran a whole period late: the grid point is already gone.
        if diff(cand, now) <= 0 {
            cand = target;
        }
        // Never two ticks closer than half a period, whatever the lattice says.
        if last_fire != 0 {
            let min_gap = pu / 2;
            while diff(cand, last_fire) < min_gap {
                cand = add(cand, pu);
            }
        }
        Some(cand)
    }
}

// ---- GetScanLine ------------------------------------------------------------------------

/// What `DxgkDdiGetScanLine` would answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanPos {
    pub in_vblank: bool,
    /// The active line, 0 while in vblank (what the shipping DDI reports).
    pub line: u32,
}

/// Where the virtual beam is at `now`, given the instant `vblank_start` of the last delivered
/// `CRTC_VSYNC` (the start of vertical blank), the filtered `period`, and the mode's `vactive`
/// and `vtotal` lines. Blank first, then the active lines, so the line counter is monotonic
/// within a period and 0 exactly at the vsync. A clock that reads before `vblank_start`, or a
/// degenerate geometry, answers "in vblank" (the shipping constant).
pub const fn scan_position(
    now: u64,
    vblank_start: u64,
    period: u64,
    vactive: u32,
    vtotal: u32,
) -> ScanPos {
    let blank = ScanPos {
        in_vblank: true,
        line: 0,
    };
    if period == 0 || vtotal == 0 || vactive == 0 || vactive >= vtotal {
        return blank;
    }
    let since = diff(now, vblank_start);
    if since < 0 {
        return blank;
    }
    let phase = since as u64 % period;
    // Line position within the whole frame, vtotal lines per period: phase * vtotal < 2^24 * 2^14.
    let pos = phase * vtotal as u64 / period;
    let blank_lines = (vtotal - vactive) as u64;
    if pos < blank_lines {
        blank
    } else {
        ScanPos {
            in_vblank: false,
            line: (pos - blank_lines) as u32,
        }
    }
}

// ---- knobs and counters --------------------------------------------------------------

/// Registry knobs (service key, REG_DWORD; read at StartDevice at PASSIVE, mirrored on every read,
/// 0 included, as `docs/zero-copy-present.md` 13.8 requires).
pub const KNOBS: [&str; 3] = ["HostVblank", "HostVblLeadUs", "HostVblHoldMs"];

/// Default `HostVblLeadUs`.
pub const LEAD_US_DEFAULT: u32 = 0;
/// Default `HostVblHoldMs`: how long `Holdover` lasts.
pub const HOLD_MS_DEFAULT: u32 = 5_000;

/// `HostVblLeadUs` in 100 ns, below `nominal` (a lead of a period is no lead).
pub const fn lead_from_knob(lead_us: u32, nominal: u64) -> u64 {
    let units = lead_us as u64 * 10;
    if nominal == 0 || units >= nominal {
        0
    } else {
        units
    }
}

/// `HostVblHoldMs` in 100 ns, clamped to 100 ms .. 60 s.
pub const fn hold_from_knob(hold_ms: u32) -> u64 {
    let ms = if hold_ms < 100 {
        100
    } else if hold_ms > 60_000 {
        60_000
    } else {
        hold_ms
    };
    ms as u64 * 10_000
}

/// The service-key values the wiring writes (at most 14 characters: `record_named_bytes` clamps
/// there; no name is shared with any other list in this crate or any literal in `kmd_render`).
/// `HvKnob`/`HvLeadEff`/`HvHoldEff` mirror the knobs, written on every read.
pub const COUNTERS: [&str; 28] = [
    "HvKnob",
    "HvAck",
    "HvNoQ",
    "HvLeadEff",
    "HvHoldEff",
    "HvMode",
    "HvRecv",
    "HvBad",
    "HvSamp",
    "HvAcc",
    "HvRejNoVs",
    "HvRejDup",
    "HvRejBack",
    "HvRejOut",
    "HvRejOld",
    "HvStep",
    "HvReseed",
    "HvMscBad",
    "HvClamp",
    "HvLock",
    "HvHold",
    "HvTimer",
    "HvTickLock",
    "HvTickHold",
    "HvTickTim",
    "HvPerNs",
    "HvErrUs",
    "HvMboxDrop",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const P240: u64 = 41_667; // vsync_deadline::period_100ns(240_000)

    fn cfg240() -> Config {
        Config::new(P240, 0)
    }

    // ---- a deterministic noise source --------------------------------------------------

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A host display whose true retrace lattice, in guest time, is `t0 + k * period` (fixed
    /// point: period in 100 ns << 16).
    struct Host {
        t0: u64,
        period_q: u64,
    }
    impl Host {
        fn at(&self, k: u64) -> u64 {
            self.t0.wrapping_add((k * self.period_q) >> Q_SHIFT)
        }
    }

    fn sample(arrive: u64, age: u64, msc: u64) -> Sample {
        Sample {
            arrive,
            age,
            msc,
            vsync: true,
        }
    }

    /// Feed `k_from..k_to` presentations, every `stride`-th one, with a delay of `dmin` plus
    /// `jitter` (uniform below it) and a spike of `spike` on 1 in `spike_every` samples. Returns
    /// the arrival time of the last sample fed.
    #[allow(clippy::too_many_arguments)]
    fn feed(
        pll: &mut Pll,
        cfg: &Config,
        host: &Host,
        rng: &mut Rng,
        ks: core::ops::Range<u64>,
        stride: u64,
        dmin: u64,
        jitter: u64,
        spike: u64,
        spike_every: u64,
        with_msc: bool,
    ) -> u64 {
        let mut last = 0;
        for k in ks.step_by(stride as usize) {
            let mut d = dmin + if jitter > 0 { rng.below(jitter) } else { 0 };
            if spike_every > 0 && rng.below(spike_every) == 0 {
                d += spike;
            }
            let arrive = host.at(k).wrapping_add(d);
            pll.observe(cfg, sample(arrive, 0, if with_msc { 1_000 + k } else { 0 }));
            last = arrive;
        }
        last
    }

    /// Error of the filter's lattice against the host's, both shifted by the delay floor `dmin`:
    /// the lattice point nearest `host.at(k) + dmin`, minus that.
    fn lattice_error(pll: &Pll, host: &Host, k: u64, dmin: u64) -> i64 {
        let truth = host.at(k).wrapping_add(dmin);
        let p = pll.period_q as i64;
        let d = diff(truth, pll.anchor);
        let n = round_div(d << Q_SHIFT, p);
        let pt = pll.point(n).unwrap();
        diff(pt, truth)
    }

    // ---- the wire message ------------------------------------------------------------

    fn msg(version: u16, count: u16, flags: u32, sent: u64, samples: &[(u64, u64)]) -> Vec<u8> {
        let mut b = std::vec![0u8; HEADER_BYTES];
        b[0..4].copy_from_slice(&MSG_HOST_VBLANK.to_le_bytes());
        b.extend_from_slice(&version.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // scanout
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&240_000u32.to_le_bytes()); // refresh_mhz
        b.extend_from_slice(&sent.to_le_bytes());
        b.extend_from_slice(&77u64.to_le_bytes()); // seq
        b.extend_from_slice(&3u32.to_le_bytes()); // lost
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(b.len(), HEADER_BYTES + BODY_FIXED_BYTES);
        for (p, m) in samples {
            b.extend_from_slice(&p.to_le_bytes());
            b.extend_from_slice(&m.to_le_bytes());
        }
        b
    }

    #[test]
    fn the_layout_constants_are_the_documented_ones() {
        assert_eq!(HEADER_BYTES + BODY_FIXED_BYTES, 64);
        assert_eq!(MAX_MSG_BYTES, 192);
        assert!(MAX_MSG_BYTES <= 256, "a posted event buffer is 256 bytes");
        assert_eq!(FEATURE, 1 << 17);
        assert_eq!(MSG_HOST_VBLANK, 32);
    }

    #[test]
    fn a_valid_event_parses_with_its_samples() {
        let b = msg(
            1,
            2,
            F_VSYNC | F_HW_CLOCK | 0x8000_0000,
            10_000_000,
            &[(9_000_000, 41), (9_041_667, 42)],
        );
        let Parsed::Event(e) = parse(&b, b.len()) else {
            panic!("did not parse")
        };
        assert_eq!(e.count(), 2);
        assert_eq!(e.refresh_mhz, 240_000);
        assert_eq!(e.seq, 77);
        assert_eq!(e.lost, 3);
        assert_eq!(e.sent_ns, 10_000_000);
        // Unknown flag bits are ignored, known ones read.
        assert!(e.flags & F_VSYNC != 0 && !e.is_idle() && !e.is_first());
        // 1 ms and 0.958 ms old (in 100 ns): the age is the host-clock difference.
        let s0 = e.sample(0, 5_000_000).unwrap();
        assert_eq!(
            (s0.arrive, s0.age, s0.msc, s0.vsync),
            (5_000_000, 10_000, 41, true)
        );
        assert_eq!(e.sample(1, 5_000_000).unwrap().age, 9_583);
        assert_eq!(e.sample(2, 0), Err(Reject::Old), "past the count");
    }

    #[test]
    fn bad_messages_are_classified_not_guessed() {
        let good = msg(1, 1, F_VSYNC, 10, &[(5, 0)]);
        assert_eq!(parse(&good[..8], 8), Parsed::NotHostVblank);
        let mut other = good.clone();
        other[0..4].copy_from_slice(&28u32.to_le_bytes());
        assert_eq!(parse(&other, other.len()), Parsed::NotHostVblank);
        assert_eq!(parse(&good, HEADER_BYTES + 20), Parsed::Short);
        // The device-reported length decides, not the buffer's: a sample cut off.
        assert_eq!(parse(&good, good.len() - 1), Parsed::Short);
        assert_eq!(
            parse(&msg(2, 1, F_VSYNC, 10, &[(5, 0)]), 200),
            Parsed::Version(2)
        );
        assert_eq!(
            parse(&msg(1, 9, F_VSYNC, 10, &[(5, 0); 9]), 400),
            Parsed::BadCount(9)
        );
        assert_eq!(
            parse(&msg(1, 0, F_VSYNC, 10, &[]), 200),
            Parsed::BadCount(0)
        );
        // An idle event has no samples.
        let idle = msg(1, 0, F_IDLE, 10, &[]);
        let Parsed::Event(e) = parse(&idle, idle.len()) else {
            panic!()
        };
        assert!(e.is_idle() && e.count() == 0);
        // Eight samples are the most, and fit a 256-byte buffer.
        let eight = msg(1, 8, F_VSYNC, 100, &[(1, 1); 8]);
        assert!(eight.len() <= 256);
        assert!(matches!(parse(&eight, eight.len()), Parsed::Event(_)));
    }

    #[test]
    fn age_is_a_host_clock_difference_and_nonsense_is_old() {
        let b = msg(
            1,
            3,
            F_VSYNC,
            1_000,
            &[(1_001, 0), (1_000, 0), (1_000 - 999, 0)],
        );
        let Parsed::Event(e) = parse(&b, b.len()) else {
            panic!()
        };
        assert_eq!(
            e.sample(0, 0),
            Err(Reject::Old),
            "presented after the event was built"
        );
        assert_eq!(e.sample(1, 0).unwrap().age, 0);
        assert_eq!(e.sample(2, 0).unwrap().age, 9);
        // Older than a second.
        let b = msg(1, 1, F_VSYNC, 5_000_000_000, &[(3_000_000_000, 0)]);
        let Parsed::Event(e) = parse(&b, b.len()) else {
            panic!()
        };
        assert_eq!(e.sample(0, 0), Err(Reject::Old));
        // A host clock that wrapped between the two reads is still a small positive age.
        let b = msg(1, 1, F_VSYNC, 500, &[(u64::MAX - 499, 0)]);
        let Parsed::Event(e) = parse(&b, b.len()) else {
            panic!()
        };
        assert_eq!(e.sample(0, 0).unwrap().age, 10);
    }

    #[test]
    fn the_feature_bit_collides_with_no_protocol_feature() {
        assert_eq!(FEATURE_BIT, 17);
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/src/features.rs");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut seen = 0;
        for line in text.lines() {
            // `pub const NAME: u64 = 1 << N;`
            if let Some(rest) = line.trim().strip_prefix("pub const ") {
                if let Some((_, shift)) = rest.split_once("= 1 << ") {
                    let n: u32 = shift.trim_end_matches(';').trim().parse().unwrap();
                    assert_ne!(n, FEATURE_BIT, "{line} already uses bit {FEATURE_BIT}");
                    seen += 1;
                }
            }
        }
        assert!(seen >= 8, "found only {seen} feature bits");
    }

    // ---- locking -----------------------------------------------------------------------

    #[test]
    fn a_clean_host_locks_after_six_good_samples_and_not_before() {
        let cfg = cfg240();
        let host = Host {
            t0: 10_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(1);
        for k in 0..5 {
            feed(
                &mut pll,
                &cfg,
                &host,
                &mut rng,
                k..k + 1,
                1,
                200,
                0,
                0,
                0,
                true,
            );
            assert_eq!(pll.mode(), Mode::Timer, "after {} samples", k + 1);
        }
        feed(&mut pll, &cfg, &host, &mut rng, 5..6, 1, 200, 0, 0, 0, true);
        assert_eq!(pll.mode(), Mode::Locked);
        assert_eq!(pll.stats.locks, 1);
        assert_eq!(lattice_error(&pll, &host, 5, 200), 0);
        assert_eq!(pll.period(), P240);
    }

    #[test]
    fn jitter_and_delay_spikes_do_not_unlock_or_move_the_lattice_much() {
        let cfg = cfg240();
        let host = Host {
            t0: 77_000_000,
            period_q: (P240 << Q_SHIFT) - 21_845,
        }; // 41666.67
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(0xDEAD_BEEF);
        // 1.5 ms spikes (a quarter period is 10416 units, a spike is 15000: rejected as outliers
        // or weighed at a sixteenth) on one sample in ten, 150 us uniform jitter on the rest.
        let mut worst = 0i64;
        let (mut sum, mut n) = (0i64, 0i64);
        let mut unlocked_after_lock = false;
        for k in 0..2_000u64 {
            feed(
                &mut pll,
                &cfg,
                &host,
                &mut rng,
                k..k + 1,
                1,
                300,
                1_500,
                15_000,
                10,
                true,
            );
            if k > 100 {
                let e = lattice_error(&pll, &host, k, 300);
                worst = worst.max(e.abs());
                sum += e;
                n += 1;
                unlocked_after_lock |= pll.mode() != Mode::Locked;
            }
        }
        assert!(!unlocked_after_lock);
        assert_eq!(pll.mode(), Mode::Locked);
        // The lattice settles near the early edge of the delay, not at its middle (750) nor at
        // a spike: 150 us of jitter is 1500 units, the mean sits about 50 us above the floor and
        // never strays 150 us from it.
        assert!(worst < 1_500, "worst lattice error {worst} (100 ns)");
        let mean = sum / n;
        assert!((0..800).contains(&mean), "mean lattice error {mean}");
        let ppm = ((pll.period_q as i64 - host.period_q as i64) * 1_000_000) / host.period_q as i64;
        assert!(ppm.abs() < 500, "period off by {ppm} ppm");
    }

    #[test]
    fn a_host_a_tenth_of_a_percent_off_the_mode_is_followed() {
        // The real display runs at 239.76 Hz (59.94-style) while the committed mode says 240.
        let cfg = cfg240();
        let host_q = ((P240 << Q_SHIFT) as u128 * 240_000 / 239_760) as u64;
        let host = Host {
            t0: 5_000_000,
            period_q: host_q,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(7);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..1_200,
            1,
            250,
            400,
            0,
            0,
            true,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        let ppm = ((pll.period_q as i64 - host_q as i64) * 1_000_000) / host_q as i64;
        assert!(ppm.abs() < 100, "period error {ppm} ppm");
        for k in 1_100..1_200 {
            assert!(lattice_error(&pll, &host, k, 250).abs() < 600);
        }
    }

    #[test]
    fn a_host_at_another_rate_is_clamped_and_never_locks() {
        let cfg = cfg240();
        let mut rng = Rng(3);
        // 144 Hz host against a 240 Hz mode (6944 units, ratio 1.667): the samples fit no
        // lattice of the nominal period, with or without the compositor's counter.
        for with_msc in [false, true] {
            let host = Host {
                t0: 1_000_000,
                period_q: (69_444u64) << Q_SHIFT,
            };
            let mut pll = Pll::new(&cfg);
            feed(
                &mut pll,
                &cfg,
                &host,
                &mut rng,
                0..600,
                1,
                200,
                100,
                0,
                0,
                with_msc,
            );
            assert_ne!(pll.mode(), Mode::Locked, "msc {with_msc}");
        }
        // 5% slow host: the period saturates at the 3% band.
        let host = Host {
            t0: 1_000_000,
            period_q: (P240 * 1050 / 1000) << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..600,
            1,
            200,
            100,
            0,
            0,
            true,
        );
        assert_ne!(pll.mode(), Mode::Locked);
        assert!(pll.stats.clamped > 0);
        assert!(pll.period_q <= cfg.nominal_q() + cfg.nominal_q() / 1000 * 30);
    }

    #[test]
    fn a_host_at_a_divisor_of_the_rate_is_followed_on_every_kth_tick_which_is_benign() {
        // 60 Hz host against a 240 Hz mode: its presentations are every fourth point of the
        // 240 Hz lattice, indistinguishable from sparse events of a 240 Hz host, and the guest's
        // ticks that coincide with them are exactly on the host's phase. Nothing to refuse.
        let cfg = cfg240();
        let host = Host {
            t0: 1_000_000,
            period_q: (4 * P240) << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(3);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..300,
            1,
            200,
            100,
            0,
            0,
            false,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        assert!((pll.period() as i64 - P240 as i64).abs() <= 2);
    }

    #[test]
    fn a_tearing_present_is_no_phase_information() {
        let cfg = cfg240();
        let mut pll = Pll::new(&cfg);
        let s = Sample {
            arrive: 1_000_000,
            age: 0,
            msc: 5,
            vsync: false,
        };
        assert_eq!(pll.observe(&cfg, s), Verdict::Rejected(Reject::NotVsync));
        assert_eq!(pll.stats.not_vsync, 1);
        assert_eq!(pll.mode(), Mode::Timer);
        assert!(!pll.has_lattice());
    }

    #[test]
    fn sparse_events_every_fourth_vblank_lock_with_or_without_the_counter() {
        for with_msc in [true, false] {
            let cfg = cfg240();
            let host = Host {
                t0: 9_000_000,
                period_q: (P240 << Q_SHIFT) + 3_000,
            };
            let mut pll = Pll::new(&cfg);
            let mut rng = Rng(11);
            feed(
                &mut pll,
                &cfg,
                &host,
                &mut rng,
                0..1_000,
                4,
                250,
                600,
                0,
                0,
                with_msc,
            );
            assert_eq!(pll.mode(), Mode::Locked, "msc {with_msc}");
            assert!(
                lattice_error(&pll, &host, 996, 250).abs() < 400,
                "msc {with_msc}"
            );
        }
    }

    #[test]
    fn a_long_gap_is_counted_by_the_compositor_even_where_rounding_would_be_ambiguous() {
        // Period known to 0.3% only, a 3 s gap (720 vblanks): the rounded count would be off by
        // two; the counter says exactly how many.
        let cfg = cfg240();
        let host = Host {
            t0: 1_000_000,
            period_q: (P240 << Q_SHIFT) + (P240 << Q_SHIFT) / 400,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(5);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..40,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        let before = pll.stats.accepted;
        // One sample 720 periods later, still inside the 5 s holdover (the filter is Holdover).
        let k = 760;
        let arrive = host.at(k) + 200;
        let v = pll.observe(&cfg, sample(arrive, 0, 1_000 + k));
        assert!(matches!(v, Verdict::Accepted(_)), "{v:?}");
        assert_eq!(pll.stats.accepted, before + 1);
        assert_eq!(pll.stats.msc_mismatch, 0);
    }

    #[test]
    fn a_duplicate_vblank_is_dropped() {
        let cfg = cfg240();
        let host = Host {
            t0: 1_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(5);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..10,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        // The same msc again, and a second sample 1/10 period later with no counter.
        let a = host.at(9) + 200;
        assert_eq!(
            pll.observe(&cfg, sample(a + 50, 0, 1_009)),
            Verdict::Rejected(Reject::Duplicate)
        );
        assert_eq!(
            pll.observe(&cfg, sample(a + 4_000, 0, 0)),
            Verdict::Rejected(Reject::Duplicate)
        );
        assert_eq!(pll.stats.duplicate, 2);
    }

    // ---- hysteresis ------------------------------------------------------------------

    #[test]
    fn one_outlier_or_one_missed_event_does_not_unlock() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(9);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..50,
            1,
            200,
            100,
            0,
            0,
            true,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        // One wild sample (0.4 period late).
        let k = 50;
        let v = pll.observe(&cfg, sample(host.at(k) + 200 + 16_700, 0, 1_000 + k));
        assert_eq!(v, Verdict::Rejected(Reject::Outlier));
        assert_eq!(pll.mode(), Mode::Locked);
        // The next on-phase sample is accepted at its own phase, so the outlier cost nothing.
        let k = 51;
        let v = pll.observe(&cfg, sample(host.at(k) + 200, 0, 1_000 + k));
        assert!(matches!(v, Verdict::Accepted(e) if e.abs() < 200), "{v:?}");
        // Seven missed vblanks (28 ms at 240 Hz, under the 33 ms holdover threshold).
        let k = 58;
        let now = host.at(k) + 200;
        assert_eq!(pll.update_mode(&cfg, now - 1_000), Mode::Locked);
        pll.observe(&cfg, sample(now, 0, 1_000 + k));
        assert_eq!(pll.mode(), Mode::Locked);
    }

    #[test]
    fn two_samples_do_not_lock_a_timer_but_do_relock_a_holdover() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(&mut pll, &cfg, &host, &mut rng, 0..2, 1, 200, 0, 0, 0, true);
        assert_eq!(pll.mode(), Mode::Timer);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            2..40,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        // Silence past the holdover threshold (8 periods = 333336 -> 33 ms): Holdover.
        let quiet = host.at(39) + 200 + cfg.holdover_after + 1;
        assert_eq!(pll.update_mode(&cfg, quiet), Mode::Holdover);
        // Back on the lattice: the second good sample relocks (the first only seeds `good`).
        let k = 60;
        pll.observe(&cfg, sample(host.at(k) + 200, 0, 1_000 + k));
        assert_eq!(pll.mode(), Mode::Holdover);
        let k = 61;
        pll.observe(&cfg, sample(host.at(k) + 200, 0, 1_000 + k));
        assert_eq!(pll.mode(), Mode::Locked);
        assert_eq!(pll.stats.locks, 2);
    }

    #[test]
    fn silence_walks_locked_to_holdover_to_timer_and_idle_goes_straight_to_timer() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..60,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        let t = host.at(59) + 200;
        assert_eq!(
            pll.update_mode(&cfg, t + cfg.holdover_after as u64),
            Mode::Locked
        );
        assert_eq!(
            pll.update_mode(&cfg, t + cfg.holdover_after as u64 + 1),
            Mode::Holdover
        );
        assert_eq!(pll.update_mode(&cfg, t + cfg.holdover_max), Mode::Holdover);
        assert_eq!(pll.update_mode(&cfg, t + cfg.holdover_max + 1), Mode::Timer);
        assert_eq!((pll.stats.holdovers, pll.stats.timeouts), (1, 1));
        // An IDLE event skips the wait.
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..60,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        pll.note_idle();
        assert_eq!(pll.mode(), Mode::Timer);
        assert!(!pll.has_lattice());
    }

    #[test]
    fn a_sample_after_a_long_silence_reseeds_the_phase_and_keeps_the_period() {
        let cfg = cfg240();
        let hp = (P240 << Q_SHIFT) + 5_000;
        let host = Host {
            t0: 3_000_000,
            period_q: hp,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..300,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        let period = pll.period_q;
        // 30 s later, no counter (it restarted), phase unrelated to the old lattice.
        let arrive = host.at(300) + 300_000_000 + 7_001;
        assert_eq!(pll.observe(&cfg, sample(arrive, 0, 0)), Verdict::Seeded);
        assert_eq!(pll.mode(), Mode::Timer);
        assert_eq!(pll.period_q, period);
        assert_eq!(pll.stats.reseeds, 1);
    }

    #[test]
    fn a_first_event_restarts_the_phase_but_keeps_the_timer_driven() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..60,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        pll.note_first();
        assert_eq!(pll.mode(), Mode::Holdover);
        assert!(!pll.has_lattice(), "unseeded until the next sample");
        let v = pll.observe(&cfg, sample(host.at(70) + 200, 0, 5)); // a fresh counter
        assert_eq!(v, Verdict::Seeded);
        let v = pll.observe(&cfg, sample(host.at(71) + 200, 0, 6));
        assert!(matches!(v, Verdict::Accepted(_)));
        assert_eq!(pll.mode(), Mode::Locked);
    }

    // ---- clock steps ------------------------------------------------------------------

    #[test]
    fn a_phase_step_of_the_host_is_followed_after_five_agreeing_strikes() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..60,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        // The compositor moves the lattice by 0.4 period (an output change, a long stall).
        let step = P240 * 4 / 10;
        let mut stepped = false;
        for k in 60..70u64 {
            let v = pll.observe(&cfg, sample(host.at(k) + 200 + step, 0, 1_000 + k));
            if v == Verdict::Stepped {
                stepped = true;
                assert_eq!(k, 64, "the fifth strike");
            }
        }
        assert!(stepped);
        assert_eq!(pll.stats.steps, 1);
        assert_eq!(
            pll.mode(),
            Mode::Locked,
            "a phase step is not a loss of the host"
        );
        assert!(
            lattice_error(
                &pll,
                &Host {
                    t0: host.t0 + step,
                    period_q: host.period_q
                },
                69,
                200
            )
            .abs()
                < 100
        );
    }

    #[test]
    fn outliers_that_disagree_are_spikes_not_a_step() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..60,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        // Six outliers in a row (25 ms, inside the 33 ms holdover threshold), alternating 0.3
        // and 0.45 period late and one early: no two neighbours agree within a sixteenth of a
        // period, so the lattice never moves.
        for (i, late) in [12_500i64, 18_700, 12_500, 18_700, -13_000, 12_500]
            .iter()
            .enumerate()
        {
            let k = 60 + i as u64;
            let at = (host.at(k) as i64 + 200 + late) as u64;
            assert_eq!(
                pll.observe(&cfg, sample(at, 0, 1_000 + k)),
                Verdict::Rejected(Reject::Outlier)
            );
        }
        assert_eq!(pll.stats.steps, 0);
        assert!(lattice_error(&pll, &host, 66, 200).abs() < 5);
        assert_eq!(pll.mode(), Mode::Locked);
        // Seven more would be the silence the holdover is for.
        let at = host.at(67) + 200 + 12_500;
        pll.observe(&cfg, sample(at, 0, 1_067));
        assert_eq!(pll.update_mode(&cfg, at + 1), Mode::Holdover);
    }

    #[test]
    fn a_guest_clock_that_steps_back_reseeds_after_three() {
        let cfg = cfg240();
        let host = Host {
            t0: 3_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut rng = Rng(9);
        let mut pll = Pll::new(&cfg);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..60,
            1,
            200,
            0,
            0,
            0,
            false,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        // The interrupt-time clock reads five periods earlier from now on (never happens; a
        // hypervisor clock fix-up might): each of the next samples is earlier than its predecessor
        // by more than half a period.
        let back = 5 * P240 + 1_000;
        let a = host.at(60) + 200 - back;
        assert_eq!(
            pll.observe(&cfg, sample(a, 0, 0)),
            Verdict::Rejected(Reject::NonMonotonic)
        );
        assert_eq!(
            pll.observe(&cfg, sample(a + P240, 0, 0)),
            Verdict::Rejected(Reject::NonMonotonic)
        );
        assert_eq!(
            pll.observe(&cfg, sample(a + 2 * P240, 0, 0)),
            Verdict::Seeded
        );
        assert_eq!(pll.mode(), Mode::Timer);
        assert_eq!((pll.stats.reseeds, pll.stats.non_monotonic), (1, 2));
        // And it locks again on the new timeline.
        let host2 = Host {
            t0: host.t0 - back,
            period_q: host.period_q,
        };
        feed(
            &mut pll,
            &cfg,
            &host2,
            &mut rng,
            63..100,
            1,
            200,
            0,
            0,
            0,
            false,
        );
        assert_eq!(pll.mode(), Mode::Locked);
    }

    // ---- the next deadline ----------------------------------------------------------

    /// Run the one-shot timer: each tick fires `latency` after its deadline (a DPC), events up to
    /// the firing instant are delivered first. Returns every firing time.
    #[allow(clippy::too_many_arguments)]
    fn run_ticks(
        pll: &mut Pll,
        cfg: &Config,
        host: &Host,
        rng: &mut Rng,
        start: u64,
        end: u64,
        feed_until: u64,
        stride: u64,
        latency_max: u64,
    ) -> Vec<u64> {
        let mut fired = Vec::new();
        let mut now = start;
        let mut prev = 0u64;
        let mut last_fire = 0u64;
        // The host's presentations, as events arriving a fixed 200 units after the instant.
        let mut k = 0u64;
        while diff(host.at(k) + 200, start) < 0 {
            k += 1;
        }
        let mut next_event = k;
        loop {
            let dl = pll
                .next_deadline(cfg, now, prev, last_fire)
                .expect("armable");
            assert!(diff(dl, now) > 0, "deadline {dl} not after now {now}");
            now = dl + rng.below(latency_max + 1);
            if diff(now, end) > 0 {
                break;
            }
            while diff(host.at(next_event) + 200, now) <= 0 {
                if diff(host.at(next_event), feed_until) < 0 && next_event % stride == 0 {
                    let a = host.at(next_event) + 200;
                    pll.observe(cfg, sample(a, 0, 1_000 + next_event));
                }
                next_event += 1;
            }
            fired.push(now);
            prev = dl;
            last_fire = now;
        }
        fired
    }

    fn gaps(f: &[u64]) -> Vec<i64> {
        f.windows(2).map(|w| diff(w[1], w[0])).collect()
    }

    #[test]
    fn in_timer_mode_the_deadline_is_exactly_the_shipping_one() {
        let cfg = cfg240();
        let mut pll = Pll::new(&cfg);
        for (prev, now) in [
            (0, 10_000_000u64),
            (10_000_000, 10_000_050),
            (1_166_667, 1_166_667 + P240 * 4 + 7),
        ] {
            let anchor = if prev == 0 { now } else { prev };
            assert_eq!(
                pll.next_deadline(&cfg, now, prev, 0),
                vsync_deadline::next(anchor, now, P240)
            );
        }
        assert_eq!(pll.next_deadline(&Config::new(0, 0), 5, 0, 0), None);
    }

    #[test]
    fn locking_moves_the_grid_by_at_most_the_slew_per_tick_without_a_gap_or_a_burst() {
        let cfg = cfg240();
        // The free-running grid starts a quarter period off the host's.
        let host = Host {
            t0: 20_000_000 + P240 / 4 + P240 * 3 / 4,
            period_q: P240 << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(21);
        let fired = run_ticks(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            20_000_000,
            20_000_000 + 2_000 * P240,
            u64::MAX / 2,
            1,
            600,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        let slew = cfg.slew_max as i64;
        let lat = 600i64;
        for (i, g) in gaps(&fired).iter().enumerate() {
            let lo = P240 as i64 - slew - lat;
            let hi = P240 as i64 + slew + lat;
            assert!((lo..=hi).contains(g), "gap {i} = {g}");
        }
        // And the ticks end up on the host lattice (within the DPC latency plus the delay floor).
        let tail = &fired[fired.len() - 200..];
        let mut worst = 0i64;
        for &t in tail {
            let n = round_div(diff(t, host.t0) << Q_SHIFT, host.period_q as i64);
            worst = worst.max(diff(t, host.at(n as u64)).abs());
        }
        assert!(
            worst < 1_200,
            "ticks sit {worst} units from the host lattice"
        );
    }

    #[test]
    fn a_lead_puts_the_tick_before_the_presentation() {
        let lead = 15_000; // 1.5 ms
        let cfg = Config::new(P240, lead);
        let host = Host {
            t0: 20_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(21);
        let fired = run_ticks(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            20_000_000 + 7_000,
            20_000_000 + 1_500 * P240,
            u64::MAX / 2,
            1,
            0,
        );
        let mut ahead = Vec::new();
        for &t in &fired[fired.len() - 100..] {
            let n = round_div(diff(t, host.t0) + lead as i64, P240 as i64);
            ahead.push(diff(host.at(n as u64), t));
        }
        for a in ahead {
            // The filter's lattice is the host's shifted by the delay floor (200, the events here
            // arrive 200 units after the presentation), and the tick is `lead` before THAT: the
            // knob is measured from presentation plus the transport minimum, a constant the lead
            // absorbs.
            assert!(
                (lead as i64 - 200 - 100..=lead as i64 - 200 + 100).contains(&a),
                "{a}"
            );
        }
        // A lead of a period or more is no lead.
        assert_eq!(Config::new(P240, P240 + 5).lead, 5);
        assert_eq!(lead_from_knob(0, P240), 0);
        assert_eq!(lead_from_knob(1_500, P240), 15_000);
        assert_eq!(lead_from_knob(4_167, P240), 0);
    }

    #[test]
    fn the_host_stopping_and_starting_again_never_makes_a_gap_or_a_burst() {
        let cfg = cfg240();
        let host = Host {
            t0: 40_000_000,
            period_q: (P240 << Q_SHIFT) + 1_500,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(33);
        let mut modes = Vec::new();
        // Phase 1: locked on a host for one second.
        let start = 40_000_000;
        let end = start + 12 * 10_000_000; // 12 s
                                           // The host feeds events for 1 s only (the viewer is occluded afterwards).
        let fired = run_ticks(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            start,
            end,
            start + 10_000_000,
            1,
            400,
        );
        modes.push(pll.mode());
        // Silence ran past the 5 s holdover: back to the free-running timer.
        assert_eq!(pll.mode(), Mode::Timer);
        let slew = cfg.slew_max as i64;
        for (i, g) in gaps(&fired).iter().enumerate() {
            assert!(
                (P240 as i64 - slew - 400..=P240 as i64 + slew + 400).contains(g),
                "gap {i} = {g}"
            );
        }
        // The tick count is the time over the period: nothing lost, nothing doubled.
        let span = diff(*fired.last().unwrap(), fired[0]);
        let expect = span / P240 as i64;
        assert!(
            (fired.len() as i64 - 1 - expect).abs() <= 1,
            "{} ticks for {expect}",
            fired.len()
        );
        // Phase 2: the host comes back (new phase), the filter relocks, still no gap.
        let host2 = Host {
            t0: host.t0 + P240 / 3,
            period_q: host.period_q,
        };
        let t2 = *fired.last().unwrap();
        let fired2 = run_ticks(
            &mut pll,
            &cfg,
            &host2,
            &mut rng,
            t2,
            t2 + 4 * 10_000_000,
            u64::MAX / 2,
            1,
            400,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        for (i, g) in gaps(&fired2).iter().enumerate() {
            assert!(
                (P240 as i64 - slew - 400..=P240 as i64 + slew + 400).contains(g),
                "gap {i} = {g}"
            );
        }
        let _ = modes;
    }

    #[test]
    fn a_tick_that_runs_late_skips_to_one_future_deadline() {
        let cfg = cfg240();
        let host = Host {
            t0: 20_000_000,
            period_q: P240 << Q_SHIFT,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(21);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..200,
            1,
            200,
            0,
            0,
            0,
            true,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        let prev = host.at(199);
        // The DPC ran 3.4 periods late (the guest paused): the answer is one lattice point in the
        // future, not a catch-up.
        let now = prev + P240 * 3 + P240 * 4 / 10;
        let d = pll.next_deadline(&cfg, now, prev, 0).unwrap();
        assert!(diff(d, now) > 0 && diff(d, now) <= P240 as i64);
        let n = round_div(diff(d, host.t0), P240 as i64);
        assert!(diff(d, host.at(n as u64)).abs() < 400, "on the lattice");
        // And two ticks are never closer than half a period: this one fired at `now`, and the
        // lattice point 100 units later is refused for the one after it.
        let now2 = host.at(250) - 100;
        let d2 = pll.next_deadline(&cfg, now2, 0, now2).unwrap();
        assert!(diff(d2, now2) >= (P240 / 2) as i64, "{}", diff(d2, now2));
    }

    #[test]
    fn deadlines_are_strictly_future_and_never_burst_under_random_lateness() {
        let cfg = cfg240();
        let host = Host {
            t0: 20_000_000,
            period_q: (P240 << Q_SHIFT) + 700,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(77);
        feed(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            0..100,
            1,
            200,
            100,
            0,
            0,
            true,
        );
        let mut now = host.at(99) + 1_000;
        let mut prev = 0u64;
        let mut last_fire = 0u64;
        let mut fired = Vec::new();
        for i in 0..5_000 {
            // Keep feeding events as time goes by.
            let k = 100 + i;
            if diff(host.at(k) + 200, now) <= 0 {
                pll.observe(&cfg, sample(host.at(k) + 200, 0, 1_000 + k));
            }
            let Some(dl) = pll.next_deadline(&cfg, now, prev, last_fire) else {
                panic!("not armable at {i}");
            };
            assert!(diff(dl, now) > 0);
            if last_fire != 0 {
                assert!(diff(dl, last_fire) >= (P240 / 2) as i64, "burst at {i}");
            }
            // 1 tick in 40 runs up to 5 periods late, the rest within 300 units.
            let late = if rng.below(40) == 0 {
                rng.below(5 * P240)
            } else {
                rng.below(300)
            };
            now = dl + late;
            fired.push(now);
            prev = dl;
            last_fire = now;
        }
        // No two firings closer than half a period; an on-time tick is within slew of a period.
        for (i, g) in gaps(&fired).iter().enumerate() {
            assert!(*g >= (P240 / 2) as i64 - 300, "gap {i} = {g}");
        }
    }

    #[test]
    fn the_filter_survives_a_timeline_that_wraps_u64() {
        let cfg = cfg240();
        let base = u64::MAX - 5 * 10_000_000; // wraps five seconds in
        let host = Host {
            t0: base,
            period_q: (P240 << Q_SHIFT) + 900,
        };
        let mut pll = Pll::new(&cfg);
        let mut rng = Rng(5);
        // Starting near the top of the u64 range the Timer path (`vsync_deadline`, checked) hits
        // its terminal None, by design, so enter the lattice first, then cross the wrap.
        let end = base.wrapping_add(10 * 10_000_000);
        let fired = run_ticks(
            &mut pll,
            &cfg,
            &host,
            &mut rng,
            base.wrapping_add(10_000),
            end,
            end,
            1,
            300,
        );
        assert_eq!(pll.mode(), Mode::Locked);
        let slew = cfg.slew_max as i64;
        assert!(
            fired.windows(2).any(|w| w[1] < w[0]),
            "the run must cross the wrap"
        );
        for (i, g) in gaps(&fired).iter().enumerate() {
            assert!(
                (P240 as i64 - slew - 300..=P240 as i64 + slew + 300).contains(g),
                "gap {i} = {g}"
            );
        }
    }

    #[test]
    fn a_degenerate_period_falls_back_instead_of_storming() {
        let cfg = Config::new(0, 0);
        let mut pll = Pll::new(&cfg);
        assert_eq!(pll.next_deadline(&cfg, 100, 0, 0), None);
        let cfg = cfg240();
        let mut pll = Pll::new(&cfg);
        // Unseeded: the shipping function, even if asked in Locked-looking state.
        assert_eq!(pll.target_after(&cfg, 1_000), None);
        assert_eq!(pll.next_deadline(&cfg, 1_000, 0, 0), Some(1_000 + P240));
    }

    // ---- GetScanLine ---------------------------------------------------------------

    #[test]
    fn the_beam_is_in_vblank_first_then_counts_active_lines() {
        // 1080 active of 1125 total at 240 Hz.
        let p = P240;
        let start = 1_000_000;
        assert_eq!(
            scan_position(start, start, p, 1080, 1125),
            ScanPos {
                in_vblank: true,
                line: 0
            }
        );
        // 45 blank lines are 4% of the period: 1666 units.
        assert!(scan_position(start + 1_600, start, p, 1080, 1125).in_vblank);
        let s = scan_position(start + 1_700, start, p, 1080, 1125);
        assert!(!s.in_vblank);
        assert!(s.line <= 2, "{s:?}");
        let end = scan_position(start + p - 1, start, p, 1080, 1125);
        assert!(
            !end.in_vblank && (1070..1080).contains(&end.line),
            "{end:?}"
        );
        // The next period starts in vblank again.
        assert!(scan_position(start + p + 10, start, p, 1080, 1125).in_vblank);
        // A clock before the vsync, or a geometry with no blank, is the shipping constant.
        assert!(scan_position(start - 1, start, p, 1080, 1125).in_vblank);
        assert!(scan_position(start + 5_000, start, p, 1125, 1125).in_vblank);
        assert!(scan_position(start + 5_000, start, 0, 1080, 1125).in_vblank);
    }

    // ---- knobs and counters --------------------------------------------------------

    #[test]
    fn knob_values_are_clamped() {
        assert_eq!(hold_from_knob(0), 1_000_000);
        assert_eq!(hold_from_knob(5_000), 50_000_000);
        assert_eq!(hold_from_knob(10_000_000), 600_000_000);
        assert_eq!(HOLD_MS_DEFAULT, 5_000);
        assert_eq!(LEAD_US_DEFAULT, 0);
        assert_eq!(
            Config::new(P240, 0).holdover_max,
            hold_from_knob(HOLD_MS_DEFAULT)
        );
        // 8 periods, clamped to 30..150 ms: 240 Hz is 33 ms, 60 Hz 133 ms, 30 Hz 150 ms, 1 kHz 30 ms.
        assert_eq!(Config::new(41_667, 0).holdover_after, 333_336);
        assert_eq!(Config::new(166_667, 0).holdover_after, 1_333_336);
        assert_eq!(Config::new(333_333, 0).holdover_after, 1_500_000);
        assert_eq!(Config::new(10_000, 0).holdover_after, 300_000);
    }

    #[test]
    fn the_design_doc_names_every_counter_knob_and_constant() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/host-vblank-pacing.md");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return; // a copy of this crate without its sibling docs: nothing to check
        };
        for n in COUNTERS.iter().chain(KNOBS.iter()) {
            assert!(
                text.contains(&std::format!("`{n}`")),
                "{n} is not in docs/host-vblank-pacing.md"
            );
        }
        // The documented defaults are the code's.
        assert!(text.contains("proposed value 32"));
        assert!(text.contains("virtio feature bit 17"));
        assert!(text.contains(&std::format!("(default {HOLD_MS_DEFAULT},")));
    }

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        let mut names: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
            assert!(n.starts_with("Hv"), "{n}");
        }
        for k in KNOBS {
            // A knob and a mirror are different names (the knob is a service-key value too).
            assert!(!COUNTERS.contains(&k), "{k} is also a counter");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        for other in crate::foreign_flip::COUNTERS {
            assert!(!COUNTERS.contains(&other), "{other} collides");
        }
        for other in crate::flip_completion::COUNTERS {
            assert!(!COUNTERS.contains(&other), "{other} collides");
        }
        for other in crate::stall_diag::COUNTERS {
            assert!(!COUNTERS.contains(&other), "{other} collides");
        }
        // Nothing in the driver writes or reads a name starting with `Hv` or a knob of ours until
        // the wiring adds its writer (`host_vblank.rs`, which then owns the prefix): a literal in
        // any other file would merge with ours, and so would the histogram and ring dumps' stems.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for root in [manifest.join("../kmd_render/src"), manifest.join("src")] {
            if !root.exists() {
                continue;
            }
            let mut stack = std::vec![root];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir).unwrap() {
                    let p = e.unwrap().path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().is_some_and(|x| x == "rs") {
                        if p.file_name().is_some_and(|n| n == "host_vblank.rs") {
                            continue;
                        }
                        checked += 1;
                        let text = std::fs::read_to_string(&p).unwrap();
                        assert!(
                            !text.contains("b\"Hv") && !text.contains("\"Hv"),
                            "{} uses a name starting with Hv",
                            p.display()
                        );
                        // The knob table (`kmd_render/src/diag.rs`, `knobs::*`) is where knob names
                        // are spelled; anywhere else a literal of ours would be a second reader.
                        if p.file_name().is_some_and(|n| n == "diag.rs") {
                            continue;
                        }
                        for k in KNOBS {
                            assert!(
                                !text.contains(&std::format!("\"{k}\"")),
                                "{} spells the knob {k}",
                                p.display()
                            );
                        }
                    }
                }
            }
        }
        assert!(checked > 20);
    }
}

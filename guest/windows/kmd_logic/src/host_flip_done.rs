//! Flip completion from the host's presentation feedback (`FlipDoneHost`), the pure half.
//! The I/O half is `kmd_render/src/ddi/host_flip_done.rs`; the design is
//! `docs/independent-flip.md` section 13.
//!
//! # What it changes
//!
//! A flip retires toward dxgkrnl when a `DXGK_INTERRUPT_CRTC_VSYNC` carries its address. Today the
//! vsync heartbeat (a guest timer at the mode's rate) reports `last_primary_address`, which the
//! KMD publishes when it PROGRAMS a flip, before the host has shown anything: the flip retires on
//! the next tick whether or not the host display ever latched it, and the tick's phase is unrelated
//! to the host's vblank.
//!
//! With the knob on and a host that offers `NVGPU_F_SCANOUT_PRESENTED`, the host's display client
//! reports each frame it put on the screen (`ScanoutPresented`, event queue message 33). Then:
//!
//! * the tick reports the address the host CONFIRMED ([`tick_address`]): a newer published address
//!   is held back until its report arrives, at most [`FALLBACK_PERIODS`] periods (the timer
//!   fallback: a report the compositor never sends, a discarded frame);
//! * in [`Mode::On`] the report itself delivers a CRTC_VSYNC at once ([`host_delivers`]) and moves
//!   the timer's phase half a period after it ([`rephase`]), so the timer only fills in the
//!   periods the host presents nothing in, and a tick that falls right after a host vsync does not
//!   deliver a second one ([`tick_delivers`]);
//! * without reports for [`HOLDOVER_100NS`] (a static desktop, a minimised viewer, no presenting
//!   client) everything is as before: the published address, every tick.
//!
//! [`Mode::HoldOnly`] is the A/B: the hold without the early vsync and the re-phase.
//!
//! # Which address a report confirms
//!
//! A `ScanoutFlip` report names the flip by `seq` (minted by the KMD, strictly increasing across
//! sources). When an address is published, the KMD notes the highest `seq` minted so far; the
//! flip that carries the new picture is minted after it, so a report with `seq` above that floor
//! shows the new address (or a newer picture of the same chain). A Venus report names the
//! resource (`RESOURCE`, no seq): it confirms the published address when that resource is the
//! active scanout resource. Anything else is stale and ignored ([`latch`]).
//!
//! Pure functions; time in 100 ns units of the interrupt clock.

/// Host `MsgType::ScanoutPresented`.
pub const MSG_SCANOUT_PRESENTED: u32 = 33;
/// `MsgHeader` bytes before the body.
pub const HEADER_BYTES: usize = 16;
/// Body bytes (`struct ScanoutPresented`).
pub const BODY_BYTES: usize = 48;
/// The whole message.
pub const MSG_BYTES: usize = HEADER_BYTES + BODY_BYTES;

/// The virtio feature bit (`NVGPU_F_SCANOUT_PRESENTED`).
pub const FEATURE_BIT: u32 = 19;
/// [`FEATURE_BIT`] as a mask over the 64-bit virtio feature word.
pub const FEATURE: u64 = 1 << FEATURE_BIT;

/// A Venus resource: `host_handle` is its resource id, `seq` 0.
pub const FLAG_RESOURCE: u32 = 1 << 0;
/// Synchronised to the host display's vblank.
pub const FLAG_VSYNC: u32 = 1 << 1;
/// The guest's buffer was scanned out with no copy.
pub const FLAG_ZERO_COPY: u32 = 1 << 2;
/// `present_ns` is known.
pub const FLAG_TIMED: u32 = 1 << 3;

/// Feedback counts as live this long after the last report (100 ms): about 6 periods at 60 Hz,
/// 24 at 240 Hz. Past it the tick reports the published address at once, as without the knob.
pub const HOLDOVER_100NS: u64 = 1_000_000;
/// A published address waits for its report at most this many periods, then the tick reports it
/// anyway (`FdhTmo`). A compositor presents a commit at its next vblank, one period plus the
/// transport later; three periods is slack for one late frame.
pub const FALLBACK_PERIODS: u64 = 3;

/// The knob (`FlipDoneHost`, REG_DWORD, read at StartDevice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0 / absent / unknown values: the feature is not acked, nothing changes.
    Off,
    /// 1: hold, early vsync from the report, timer re-phase.
    On,
    /// 2: hold only; the timer alone delivers, on its own phase.
    HoldOnly,
}

impl Mode {
    pub const fn from_knob(v: u32) -> Self {
        match v {
            1 => Self::On,
            2 => Self::HoldOnly,
            _ => Self::Off,
        }
    }
    pub const fn code(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::HoldOnly => 2,
        }
    }
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Whether StartDevice asks for the feature: the knob is on and the display half is up (the
/// consumer lives there).
pub const fn wants_feature(mode: Mode, display_half: bool) -> bool {
    mode.is_on() && display_half
}

/// A parsed `ScanoutPresented`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presented {
    pub scanout: u32,
    pub flags: u32,
    pub owner_handle: u32,
    pub host_handle: u32,
    pub seq: u64,
    pub present_ns: u64,
    pub sent_ns: u64,
}

impl Presented {
    pub const fn is_resource(&self) -> bool {
        self.flags & FLAG_RESOURCE != 0
    }
    /// Host-side delay from the screen to the event's send, in microseconds (saturated), when the
    /// presentation time is known.
    pub const fn host_age_us(&self) -> Option<u32> {
        if self.flags & FLAG_TIMED == 0 || self.sent_ns < self.present_ns {
            return None;
        }
        let us = (self.sent_ns - self.present_ns) / 1000;
        Some(if us > u32::MAX as u64 {
            u32::MAX
        } else {
            us as u32
        })
    }
}

/// What a filled event buffer held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    Presented(Presented),
    /// Some other message type (or fewer than 16 bytes).
    NotPresented,
    /// A `ScanoutPresented` shorter than [`MSG_BYTES`]: dropped.
    Short,
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(rd32(b, at)?) | (u64::from(rd32(b, at + 4)?) << 32))
}

/// Read one used buffer: `bytes` is the buffer, `len` the length the device reported. The
/// header's `handle` and `status` are not checked (0 and 0 by contract), nor is `reserved`.
pub fn parse(bytes: &[u8], len: usize) -> Parsed {
    let len = len.min(bytes.len());
    if len < HEADER_BYTES || rd32(bytes, 0) != Some(MSG_SCANOUT_PRESENTED) {
        return Parsed::NotPresented;
    }
    if len < MSG_BYTES {
        return Parsed::Short;
    }
    let b = HEADER_BYTES;
    match (
        rd32(bytes, b),
        rd32(bytes, b + 4),
        rd32(bytes, b + 8),
        rd32(bytes, b + 12),
        rd64(bytes, b + 16),
        rd64(bytes, b + 24),
        rd64(bytes, b + 32),
    ) {
        (
            Some(scanout),
            Some(flags),
            Some(owner_handle),
            Some(host_handle),
            Some(seq),
            Some(present_ns),
            Some(sent_ns),
        ) => Parsed::Presented(Presented {
            scanout,
            flags,
            owner_handle,
            host_handle,
            seq,
            present_ns,
            sent_ns,
        }),
        _ => Parsed::Short,
    }
}

/// The address the vsync would report without the knob, and what is known about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publication {
    /// `last_primary_address`.
    pub address: u64,
    /// When it was published (0: never, or before this generation).
    pub at: u64,
    /// One above the highest flip `seq` minted when it was published.
    pub seq_floor: u64,
    /// Published as a kept picture: the host is never told about it, no report will come.
    pub kept: bool,
}

/// What a report means for the published address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latch {
    /// The host shows this address now.
    Address(u64),
    /// About an older picture, another scanout or another resource: ignored.
    Stale,
}

/// Which address the report `p` confirms, given the current publication and, for a Venus report,
/// the resources that can carry the published picture: the active scanout resource (the direct
/// bind of the application's or DWM's own blob) and the one the host is bound to (the KMD's
/// LINEAR scan-out image the copy path draws into).
pub const fn latch(p: &Presented, publ: &Publication, resources: [u32; 2]) -> Latch {
    if p.scanout != 0 || publ.address == 0 {
        return Latch::Stale;
    }
    if p.is_resource() {
        if p.host_handle != 0 && (p.host_handle == resources[0] || p.host_handle == resources[1]) {
            Latch::Address(publ.address)
        } else {
            Latch::Stale
        }
    } else if publ.seq_floor != 0 && p.seq >= publ.seq_floor {
        Latch::Address(publ.address)
    } else {
        Latch::Stale
    }
}

/// Why a tick reported what it reported (one counter each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickWhy {
    /// The knob is off.
    Off,
    /// No report within [`HOLDOVER_100NS`]: the published address, as without the knob.
    Inactive,
    /// The host confirmed the published address.
    Latched,
    /// A kept picture: the host is never told, so it is reported at once.
    Kept,
    /// The published address waited [`FALLBACK_PERIODS`] periods: reported anyway.
    Timeout,
    /// The published address is not confirmed yet: the confirmed one is reported.
    Held,
}

/// The address a vsync reports now.
///
/// `active_until`: feedback is live before it (last report + [`HOLDOVER_100NS`]; 0 never).
/// `latched`: the last confirmed address (0 none).
pub const fn tick_address(
    mode: Mode,
    now: u64,
    period: u64,
    active_until: u64,
    publ: &Publication,
    latched: u64,
) -> (u64, TickWhy) {
    if !mode.is_on() {
        return (publ.address, TickWhy::Off);
    }
    if now >= active_until || latched == 0 {
        return (publ.address, TickWhy::Inactive);
    }
    if publ.address == latched {
        return (publ.address, TickWhy::Latched);
    }
    if publ.kept {
        return (publ.address, TickWhy::Kept);
    }
    if now.saturating_sub(publ.at) >= FALLBACK_PERIODS.saturating_mul(period) {
        return (publ.address, TickWhy::Timeout);
    }
    (latched, TickWhy::Held)
}

/// Whether the timer tick delivers its CRTC_VSYNC: not in [`Mode::On`] when a report arrived less
/// than three quarters of a period ago (that report was this period's vblank, delivered by itself
/// or, if it came too close to an earlier vsync, by the next report). The timer only fills in the
/// periods the host is silent in.
pub const fn tick_delivers(mode: Mode, now: u64, last_report: u64, period: u64) -> bool {
    if !matches!(mode, Mode::On) || last_report == 0 || now < last_report {
        return true;
    }
    now - last_report >= period * 3 / 4
}

/// Whether a report delivers a CRTC_VSYNC at once ([`Mode::On`]): unless any vsync went out less
/// than half a period ago (then the next tick reports the confirmed address).
pub const fn host_delivers(mode: Mode, now: u64, last_vsync: u64, period: u64) -> bool {
    if !matches!(mode, Mode::On) {
        return false;
    }
    if last_vsync == 0 || now < last_vsync {
        return true;
    }
    now - last_vsync >= period / 2
}

/// The timer deadline a report at `at` asks for ([`Mode::On`]): half a period after it, so the
/// timer's ticks fall between the host's vblanks. `None` when the armed `deadline` already has
/// that phase to within an eighth of a period (the steady state: no re-arm), or the timer is not
/// armed (`deadline` 0).
pub const fn rephase(mode: Mode, at: u64, deadline: u64, period: u64) -> Option<u64> {
    if !matches!(mode, Mode::On) || deadline == 0 || period == 0 {
        return None;
    }
    let target = at.saturating_add(period / 2);
    // Phase distance on the lattice of period `period`.
    let d = if deadline >= target {
        (deadline - target) % period
    } else {
        (target - deadline) % period
    };
    let dist = if d > period - d { period - d } else { d };
    if dist <= period / 8 {
        None
    } else {
        Some(target)
    }
}

/// Microseconds from 100 ns units, saturated to `u32`.
pub const fn us_from_100ns(t: u64) -> u32 {
    let us = t / 10;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

/// The knob (REG_DWORD in the service key).
pub const KNOB: &str = "FlipDoneHost";

/// Counter names (REG_DWORD, at most 14 characters, `Fdh` prefix, none shared with any other
/// counter or knob). Event-gated: a zero block at every StartDevice, then values from the
/// periodic dump when one moved.
pub const COUNTERS: [&str; 19] = [
    "FdhKnob",    // Mode::code in force
    "FdhAck",     // 1: NVGPU_F_SCANOUT_PRESENTED acked this generation
    "FdhEvN",     // ScanoutPresented events received
    "FdhBad",     // short ones (dropped)
    "FdhUnask",   // events without the ack (a host bug; dropped)
    "FdhLatch",   // events that confirmed the published address
    "FdhStale",   // events that confirmed nothing new
    "FdhVsync",   // CRTC_VSYNCs delivered by a report (Mode::On)
    "FdhCoal",    // reports that left the vsync to the tick (gap or gate)
    "FdhRephase", // timer re-phases
    "FdhTkSkip",  // ticks that delivered nothing: a report's vsync was this period's
    "FdhHeld",    // ticks that reported the confirmed address over a newer published one
    "FdhTmo",     // ticks that reported an unconfirmed address after the fallback
    "FdhKept",    // ticks that reported a kept picture at once
    "FdhInact",   // ticks with the knob on and no live feedback
    "FdhLatUs",   // last publish -> report latency, microseconds (guest clock)
    "FdhLatMax",  // its maximum
    "FdhLatAvg",  // its mean over FdhLatch
    "FdhAgeUs",   // last host-side screen -> event-sent delay, microseconds
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const P: u64 = 41_667; // 240 Hz

    fn msg(p: &Presented) -> [u8; 80] {
        let mut b = [0u8; 80];
        b[0..4].copy_from_slice(&MSG_SCANOUT_PRESENTED.to_le_bytes());
        let w =
            |b: &mut [u8; 80], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        w(&mut b, 16, p.scanout);
        w(&mut b, 20, p.flags);
        w(&mut b, 24, p.owner_handle);
        w(&mut b, 28, p.host_handle);
        b[32..40].copy_from_slice(&p.seq.to_le_bytes());
        b[40..48].copy_from_slice(&p.present_ns.to_le_bytes());
        b[48..56].copy_from_slice(&p.sent_ns.to_le_bytes());
        b
    }

    fn ev(seq: u64) -> Presented {
        Presented {
            scanout: 0,
            flags: FLAG_VSYNC | FLAG_TIMED,
            owner_handle: 3,
            host_handle: 10,
            seq,
            present_ns: 1_000_000,
            sent_ns: 1_250_000,
        }
    }

    fn publ(address: u64, at: u64, seq_floor: u64) -> Publication {
        Publication {
            address,
            at,
            seq_floor,
            kept: false,
        }
    }

    #[test]
    fn the_wire_layout_is_the_hosts() {
        // host/backend/protocol: header {33, 0, 0, 0}, then {scanout, flags, owner_handle,
        // host_handle, seq u64, present_ns u64, sent_ns u64, reserved u64}.
        assert_eq!(MSG_BYTES, 64);
        let p = ev(0x1_0000_0002);
        let b = msg(&p);
        assert_eq!(parse(&b, 64), Parsed::Presented(p));
        assert_eq!(parse(&b, 63), Parsed::Short);
        assert_eq!(parse(&b, 15), Parsed::NotPresented);
        let mut other = b;
        other[0] = 28;
        assert_eq!(parse(&other, 64), Parsed::NotPresented);
        assert_eq!(p.host_age_us(), Some(250));
        assert_eq!(Presented { flags: 0, ..p }.host_age_us(), None);
    }

    #[test]
    fn the_feature_bit_is_free_and_matches_the_protocol_crate() {
        assert_eq!(FEATURE, 1 << 19);
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/src/features.rs"),
        )
        .unwrap();
        assert!(text.contains("pub const NVGPU_F_SCANOUT_PRESENTED: u64 = 1 << 19;"));
        // No other constant there takes bit 19.
        let n = text.matches("1 << 19").count();
        assert_eq!(n, 1, "{n} spellings of bit 19");
        // The host vblank proposal keeps 17.
        assert_ne!(FEATURE_BIT, crate::host_vblank::FEATURE_BIT);
    }

    #[test]
    fn modes() {
        assert_eq!(Mode::from_knob(0), Mode::Off);
        assert_eq!(Mode::from_knob(1), Mode::On);
        assert_eq!(Mode::from_knob(2), Mode::HoldOnly);
        assert_eq!(Mode::from_knob(7), Mode::Off, "a typo never turns it on");
        for m in [Mode::Off, Mode::On, Mode::HoldOnly] {
            assert_eq!(Mode::from_knob(m.code()), m);
        }
        assert!(!wants_feature(Mode::On, false));
        assert!(!wants_feature(Mode::Off, true));
        assert!(wants_feature(Mode::HoldOnly, true));
    }

    #[test]
    fn a_flip_report_confirms_the_address_published_before_it_was_minted() {
        // Published when seq 41 was the newest minted: floor 42.
        let pb = publ(0xA000, 100, 42);
        assert_eq!(
            latch(&ev(41), &pb, [0, 0]),
            Latch::Stale,
            "the previous picture"
        );
        assert_eq!(latch(&ev(42), &pb, [0, 0]), Latch::Address(0xA000));
        assert_eq!(latch(&ev(50), &pb, [0, 0]), Latch::Address(0xA000));
        // Nothing published, another scanout, no floor: stale.
        assert_eq!(latch(&ev(50), &publ(0, 100, 42), [0, 0]), Latch::Stale);
        assert_eq!(
            latch(
                &Presented {
                    scanout: 1,
                    ..ev(50)
                },
                &pb,
                [0, 0]
            ),
            Latch::Stale
        );
        assert_eq!(latch(&ev(50), &publ(0xA000, 100, 0), [0, 0]), Latch::Stale);
    }

    #[test]
    fn a_venus_report_confirms_the_active_resource() {
        let pb = publ(0xB000, 100, 0);
        let v = Presented {
            flags: FLAG_RESOURCE,
            owner_handle: 0,
            host_handle: 77,
            seq: 0,
            ..ev(0)
        };
        assert_eq!(latch(&v, &pb, [77, 5]), Latch::Address(0xB000));
        // The copy path: the host shows the KMD's scan-out image it is bound to.
        assert_eq!(latch(&v, &pb, [12, 77]), Latch::Address(0xB000));
        assert_eq!(latch(&v, &pb, [78, 5]), Latch::Stale);
        assert_eq!(
            latch(
                &Presented {
                    host_handle: 0,
                    ..v
                },
                &pb,
                [0, 0]
            ),
            Latch::Stale
        );
    }

    #[test]
    fn the_tick_holds_an_unconfirmed_address_until_the_fallback() {
        let now = 10_000_000;
        let live = now + HOLDOVER_100NS;
        let pb = publ(0xA000, now - P, 42);
        // Off: the published address, always.
        assert_eq!(
            tick_address(Mode::Off, now, P, live, &pb, 0x9000),
            (0xA000, TickWhy::Off)
        );
        // On, live, not confirmed yet: the confirmed one.
        assert_eq!(
            tick_address(Mode::On, now, P, live, &pb, 0x9000),
            (0x9000, TickWhy::Held)
        );
        assert_eq!(
            tick_address(Mode::HoldOnly, now, P, live, &pb, 0x9000),
            (0x9000, TickWhy::Held)
        );
        // Confirmed.
        assert_eq!(
            tick_address(Mode::On, now, P, live, &pb, 0xA000),
            (0xA000, TickWhy::Latched)
        );
        // Waited three periods: reported anyway.
        let old = publ(0xA000, now - 3 * P, 42);
        assert_eq!(
            tick_address(Mode::On, now, P, live, &old, 0x9000),
            (0xA000, TickWhy::Timeout)
        );
        // A kept picture is never reported by the host.
        let kept = Publication { kept: true, ..pb };
        assert_eq!(
            tick_address(Mode::On, now, P, live, &kept, 0x9000),
            (0xA000, TickWhy::Kept)
        );
        // No live feedback (the holdover ran out, or none ever came).
        assert_eq!(
            tick_address(Mode::On, now, P, now, &pb, 0x9000),
            (0xA000, TickWhy::Inactive)
        );
        assert_eq!(
            tick_address(Mode::On, now, P, live, &pb, 0),
            (0xA000, TickWhy::Inactive)
        );
    }

    #[test]
    fn a_report_delivers_unless_a_vsync_just_went_out() {
        assert!(host_delivers(Mode::On, 1000, 0, P));
        assert!(host_delivers(Mode::On, 100 + P / 2, 100, P));
        assert!(!host_delivers(Mode::On, 100 + P / 2 - 1, 100, P));
        assert!(!host_delivers(Mode::HoldOnly, 10 * P, 100, P));
        assert!(!host_delivers(Mode::Off, 10 * P, 100, P));
    }

    #[test]
    fn a_tick_right_after_a_reports_vsync_delivers_nothing() {
        assert!(tick_delivers(Mode::On, 1000, 0, P));
        assert!(!tick_delivers(Mode::On, 100 + P / 2, 100, P));
        assert!(tick_delivers(Mode::On, 100 + P * 3 / 4, 100, P));
        assert!(tick_delivers(Mode::HoldOnly, 101, 100, P));
        assert!(tick_delivers(Mode::Off, 101, 100, P));
    }

    #[test]
    fn rephase_puts_the_timer_between_the_hosts_vblanks_and_then_leaves_it() {
        let at = 1_000_000;
        // A timer in phase with the host: moved half a period later.
        assert_eq!(rephase(Mode::On, at, at + P, P), Some(at + P / 2));
        // Already there (or a whole number of periods off): left alone.
        assert_eq!(rephase(Mode::On, at, at + P / 2, P), None);
        assert_eq!(rephase(Mode::On, at, at + P / 2 + 2 * P, P), None);
        assert_eq!(rephase(Mode::On, at, at + P / 2 + P / 10, P), None);
        assert_eq!(rephase(Mode::On, at, at - P / 2 + P / 10, P), None);
        // Not armed, or not this mode.
        assert_eq!(rephase(Mode::On, at, 0, P), None);
        assert_eq!(rephase(Mode::HoldOnly, at, at + P, P), None);
    }

    /// The whole loop over a few hundred periods: the host presents every period with jitter, the
    /// timer runs, reports confirm. Every period carries exactly one delivered vsync once locked,
    /// and every flip retires on its own report's vsync.
    #[test]
    fn a_simulated_240_hz_run_delivers_one_vsync_per_period() {
        let mut deadline = 37 * P / 10; // an arbitrary initial phase
        let mut last_vsync = 0u64;
        let mut last_host = 0u64;
        let mut delivered: Vec<(u64, bool)> = Vec::new(); // (time, from the host)
        let mut host_t = 5 * P;
        let end = 400 * P;
        let mut jit = 0u64;
        while host_t < end || deadline < end {
            if deadline <= host_t {
                let now = deadline;
                if tick_delivers(Mode::On, now, last_host, P) {
                    delivered.push((now, false));
                    last_vsync = now;
                }
                deadline += P;
            } else {
                let now = host_t;
                if host_delivers(Mode::On, now, last_vsync, P) {
                    delivered.push((now, true));
                    last_vsync = now;
                }
                last_host = now;
                if let Some(d) = rephase(Mode::On, now, deadline, P) {
                    deadline = d;
                }
                // +-10% jitter, deterministic.
                jit = (jit * 7 + 3) % 21;
                host_t += P - P / 10 + jit * P / 100;
            }
        }
        let locked: Vec<&(u64, bool)> = delivered.iter().filter(|d| d.0 > 20 * P).collect();
        let from_host = locked.iter().filter(|d| d.1).count();
        // Every host vblank delivered, the timer filled in nothing while they flow.
        assert!(
            from_host * 100 >= locked.len() * 99,
            "{from_host} of {}",
            locked.len()
        );
        for w in locked.windows(2) {
            let gap = w[1].0 - w[0].0;
            assert!(gap >= P / 2, "two vsyncs {gap} apart");
        }
    }

    #[test]
    fn when_the_host_stops_the_timer_takes_over_within_two_periods() {
        // The last report at t; the timer was re-phased to t + P/2.
        let t = 100 * P;
        let mut deadline = rephase(Mode::On, t, t + P, P).unwrap();
        let mut ticks = Vec::new();
        for _ in 0..4 {
            if tick_delivers(Mode::On, deadline, t, P) {
                ticks.push(deadline - t);
            }
            deadline += P;
        }
        assert_eq!(ticks[0], P / 2 + P, "the first fill-in vsync");
        assert_eq!(ticks.len(), 3);
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut v: Vec<&str> = COUNTERS.to_vec();
        for n in &v {
            assert!(n.len() <= 14 && n.starts_with("Fdh"), "{n}");
        }
        v.sort();
        v.dedup();
        assert_eq!(v.len(), COUNTERS.len());
        assert!(KNOB.len() <= 14);
        assert!(!COUNTERS.contains(&KNOB));
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
            render.display()
        );
        None
    }

    fn rust_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = std::vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        out
    }

    const RENDER_FILE: &str = "ddi/host_flip_done.rs";

    #[test]
    fn the_driver_writes_exactly_these_counters_in_its_one_file() {
        let Some(root) = render_src() else {
            return;
        };
        let ours = root.join(RENDER_FILE);
        let text = std::fs::read_to_string(&ours).unwrap();
        let mut written: Vec<std::string::String> = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("b\"Fdh") {
            let tail = &rest[i + 2..];
            let end = tail.find('"').unwrap();
            let name = &tail[..end];
            if !written.iter().any(|w| w == name) {
                written.push(name.into());
            }
            rest = &tail[end..];
        }
        written.sort();
        let mut listed: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        listed.sort();
        assert_eq!(written, listed);
        let mut knob_spelled = 0;
        for p in rust_files(&root) {
            let text = std::fs::read_to_string(&p).unwrap();
            if text.contains(&std::format!("b\"{KNOB}\"")) {
                knob_spelled += 1;
            }
            if p == ours {
                continue;
            }
            assert!(
                !text.contains("b\"Fdh"),
                "{} spells an Fdh counter",
                p.display()
            );
        }
        assert_eq!(knob_spelled, 1, "the knob literal must exist exactly once");
    }
}

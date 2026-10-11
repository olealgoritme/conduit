//! The vblank pacer: one `HostVblank` event per host display vblank for a
//! guest that acked `NVGPU_F_HOST_VBLANK` (docs/SCANOUT.md "Host vblank").
//!
//! Each display client that reports presentations (`EV_PRESENTED`) gets its
//! own [`VblankClock`]. The pacer follows one of them, the source:
//! a fullscreen client whose clock is live first, else the live client that
//! reported most recently (a change of source restarts the event stream with
//! `FIRST`). Its thread sleeps until the source's next vblank minus the lead
//! (`clock_nanosleep` on `CLOCK_MONOTONIC`, absolute, with a 1 ns timer
//! slack) and hands the event to the sink, which puts it in a posted guest
//! event buffer or drops it. No source (no viewer, a stream host only, a
//! viewer whose compositor reports no vsync'd presentations, reports older
//! than the hold): no events, the guest keeps its own timer. When the stream
//! stops one `IDLE` event says so.
//!
//! The thread never blocks on anything but its own condition variable and
//! its sleep, every wait is bounded, and it holds no lock across the sink.

use crate::vblank_clock::{Sample, VblankClock};
use protocol::messages::{HOST_VBLANK_FIRST, HOST_VBLANK_IDLE, HOST_VBLANK_VSYNC, HostVblank};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Where `HostVblank` events go (the event queue, in the vhost-user binary).
/// Must not block; `false`: dropped (no buffer posted).
pub trait HostVblankSink: Send + Sync {
    fn vblank(&self, v: &HostVblank) -> bool;
}

/// Never schedule a vblank closer than this to now: the event would leave
/// late. The next one is taken instead.
const GUARD_NS: u64 = 300_000;
/// The longest the thread sleeps in one go (a stale plan is re-made).
const MAX_SLEEP_NS: u64 = 100_000_000;
/// How long the thread waits for something to change while it has nothing
/// to send.
const IDLE_WAIT: Duration = Duration::from_millis(100);

#[derive(Default)]
struct ClientClock {
    clock: VblankClock,
    fullscreen: bool,
}

#[derive(Default)]
struct State {
    clients: Vec<ClientClock>,
    /// The guest acked the feature (and a sink exists).
    enabled: bool,
    /// The client followed by the last event, if the stream is running.
    source: Option<usize>,
    seq: u64,
    lost: u32,
    /// Counters.
    sent: u64,
    dropped: u64,
    stop: bool,
}

/// The pacer: shared by the display link (reports in) and its thread.
pub struct VblankPacer {
    state: Mutex<State>,
    wake: Condvar,
    lead_ns: u64,
}

/// Which client the events follow: a fullscreen client with a live clock,
/// else the live client with the newest report. Pure.
fn pick_source(clients: &[ClientClock], now_ns: u64) -> Option<usize> {
    let live = |c: &ClientClock| c.clock.is_live(now_ns);
    if let Some(i) = clients.iter().position(|c| c.fullscreen && live(c)) {
        return Some(i);
    }
    clients
        .iter()
        .enumerate()
        .filter(|(_, c)| live(c))
        .max_by_key(|(_, c)| c.clock.last_report_ns())
        .map(|(i, _)| i)
}

/// `CLOCK_MONOTONIC` in nanoseconds (the clock of the presentation reports).
pub fn mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn sleep_until_ns(t: u64) {
    let ts = libc::timespec {
        tv_sec: (t / 1_000_000_000) as libc::time_t,
        tv_nsec: (t % 1_000_000_000) as libc::c_long,
    };
    // SAFETY: a valid timespec; EINTR just ends the sleep early (the loop
    // re-checks the time).
    unsafe {
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &ts,
            std::ptr::null_mut(),
        )
    };
}

impl VblankPacer {
    pub fn new(lead: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            lead_ns: lead.as_nanos().min(4_000_000) as u64,
        })
    }

    fn client(st: &mut State, i: usize) -> &mut ClientClock {
        if st.clients.len() <= i {
            st.clients.resize_with(i + 1, ClientClock::default);
        }
        &mut st.clients[i]
    }

    /// Client `i` reports a presentation at `t_ns` with these kind bits.
    pub fn presented(&self, i: usize, kind: u32, t_ns: u64) {
        let mut st = self.state.lock().unwrap();
        let was_live = st.enabled && st.source.is_none();
        let r = Self::client(&mut st, i).clock.sample(t_ns, kind);
        if was_live && r == Sample::Accepted {
            // Maybe the first lock: let the thread look.
            self.wake.notify_one();
        }
    }

    /// Client `i`'s window: fullscreen or not, and its output's refresh in
    /// mHz (0 unknown).
    pub fn surface(&self, i: usize, fullscreen: bool, refresh_mhz: u32) {
        let mut st = self.state.lock().unwrap();
        let c = Self::client(&mut st, i);
        c.fullscreen = fullscreen;
        if refresh_mhz != 0 {
            c.clock.set_refresh_mhz(refresh_mhz);
        }
        self.wake.notify_one();
    }

    /// Client `i` disconnected (or reconnected: a new connection starts over).
    pub fn client_gone(&self, i: usize) {
        let mut st = self.state.lock().unwrap();
        if let Some(c) = st.clients.get_mut(i) {
            c.clock.reset();
            c.fullscreen = false;
        }
        self.wake.notify_one();
    }

    /// The guest acked `NVGPU_F_HOST_VBLANK` (`true`) or a new guest
    /// generation started without it (`false`).
    pub fn set_enabled(&self, on: bool) {
        let mut st = self.state.lock().unwrap();
        st.enabled = on;
        st.source = None;
        self.wake.notify_one();
    }

    /// `(sent, dropped)` events so far.
    pub fn counts(&self) -> (u64, u64) {
        let st = self.state.lock().unwrap();
        (st.sent, st.dropped)
    }

    /// Stop the thread (tests; the backend's runs for the process).
    pub fn stop(&self) {
        self.state.lock().unwrap().stop = true;
        self.wake.notify_one();
    }

    /// Start the thread that sends to `sink`.
    pub fn start(self: &Arc<Self>, sink: Arc<dyn HostVblankSink>) -> std::io::Result<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("nvgpu-vblank".into())
            .spawn(move || {
                // SAFETY: prctl on the calling thread; a 1 ns slack keeps the
                // kernel from coalescing this thread's wakes by up to 50 us.
                unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong) };
                me.run(&*sink);
            })?;
        Ok(())
    }

    /// What to do next, decided under the lock.
    fn plan(&self, st: &mut State, now: u64) -> Plan {
        if st.stop {
            return Plan::Stop;
        }
        if !st.enabled {
            return Plan::Wait;
        }
        let Some(src) = pick_source(&st.clients, now) else {
            return if st.source.take().is_some() {
                Plan::Idle
            } else {
                Plan::Wait
            };
        };
        let first = st.source != Some(src);
        if first {
            st.seq = 0;
            st.lost = 0;
        }
        st.source = Some(src);
        let c = &st.clients[src].clock;
        let Some(vblank) = c.next_after(now + GUARD_NS + self.lead_ns) else {
            return Plan::Wait;
        };
        let refresh_mhz = if c.period_ns() > 0.0 {
            (1e12 / c.period_ns()).round() as u32
        } else {
            0
        };
        Plan::Send {
            vblank,
            at: vblank.saturating_sub(self.lead_ns),
            first,
            refresh_mhz,
        }
    }

    fn run(&self, sink: &dyn HostVblankSink) {
        loop {
            let now = mono_ns();
            let plan = {
                let mut st = self.state.lock().unwrap();
                self.plan(&mut st, now)
            };
            match plan {
                Plan::Stop => return,
                Plan::Wait => {
                    let st = self.state.lock().unwrap();
                    let _ = self.wake.wait_timeout(st, IDLE_WAIT).unwrap();
                }
                Plan::Idle => {
                    let v = HostVblank {
                        flags: HOST_VBLANK_IDLE,
                        sent_ns: mono_ns(),
                        ..Default::default()
                    };
                    let took = sink.vblank(&v);
                    self.count(took);
                }
                Plan::Send {
                    vblank,
                    at,
                    first,
                    refresh_mhz,
                } => {
                    if at.saturating_sub(now) > MAX_SLEEP_NS {
                        sleep_until_ns(now + MAX_SLEEP_NS);
                        continue;
                    }
                    sleep_until_ns(at);
                    let (seq, lost) = {
                        let mut st = self.state.lock().unwrap();
                        if !st.enabled || st.stop {
                            continue;
                        }
                        st.seq += 1;
                        (st.seq, std::mem::take(&mut st.lost))
                    };
                    let v = HostVblank {
                        flags: HOST_VBLANK_VSYNC | if first { HOST_VBLANK_FIRST } else { 0 },
                        refresh_mhz,
                        sent_ns: mono_ns(),
                        seq,
                        lost,
                        vblank_ns: vblank,
                    };
                    let took = sink.vblank(&v);
                    self.count(took);
                    // Do not plan the same vblank again.
                    let after = vblank.saturating_sub(self.lead_ns) + 1;
                    let now = mono_ns();
                    if now < after {
                        sleep_until_ns(after);
                    }
                }
            }
        }
    }

    fn count(&self, took: bool) {
        let mut st = self.state.lock().unwrap();
        if took {
            st.sent += 1;
        } else {
            st.dropped += 1;
            st.lost = st.lost.saturating_add(1);
        }
    }
}

enum Plan {
    Stop,
    Wait,
    Idle,
    Send {
        vblank: u64,
        at: u64,
        first: bool,
        refresh_mhz: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vblank_clock::KIND_VSYNC;

    fn fitted(phase: u64, period: u64, fullscreen: bool, upto: u64) -> ClientClock {
        let mut c = ClientClock {
            fullscreen,
            ..Default::default()
        };
        c.clock.set_refresh_mhz((1e12 / period as f64) as u32);
        let mut t = phase;
        while t <= upto {
            c.clock.sample(t, KIND_VSYNC);
            t += period;
        }
        c
    }

    #[test]
    fn the_fullscreen_client_wins_else_the_newest_report() {
        let now = 2_000_000_000;
        let a = fitted(1_000_000_000, 4_166_667, false, now);
        let b = fitted(1_000_100_000, 6_944_444, false, now - 50_000_000);
        assert_eq!(pick_source(&[a, b], now), Some(0), "newest report");
        let a = fitted(1_000_000_000, 4_166_667, false, now);
        let b = fitted(1_000_100_000, 6_944_444, true, now - 50_000_000);
        assert_eq!(pick_source(&[a, b], now), Some(1), "fullscreen");
        assert_eq!(
            pick_source(&[ClientClock::default()], now),
            None,
            "nothing fitted"
        );
    }

    struct Rec(Mutex<Vec<(u64, HostVblank)>>);
    impl HostVblankSink for Rec {
        fn vblank(&self, v: &HostVblank) -> bool {
            self.0.lock().unwrap().push((mono_ns(), *v));
            true
        }
    }

    /// Real time: a fitted client at 250 Hz gets events at its vblanks,
    /// FIRST once, one per vblank, never early.
    #[test]
    fn events_follow_the_lattice_in_real_time() {
        let pacer = VblankPacer::new(Duration::ZERO);
        let period = 4_000_000u64;
        let start = mono_ns();
        let phase = start - 40 * period + 1_234_567;
        pacer.surface(0, true, 250_000);
        for k in 0..40 {
            pacer.presented(0, KIND_VSYNC, phase + k * period);
        }
        pacer.set_enabled(true);
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        pacer.start(rec.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        pacer.stop();
        std::thread::sleep(Duration::from_millis(10));
        let ev = rec.0.lock().unwrap().clone();
        assert!(ev.len() >= 8, "{} events", ev.len());
        assert_eq!(ev[0].1.flags, HOST_VBLANK_VSYNC | HOST_VBLANK_FIRST);
        assert!(ev[1..].iter().all(|(_, v)| v.flags == HOST_VBLANK_VSYNC));
        for w in ev.windows(2) {
            assert_eq!(
                w[1].1.vblank_ns - w[0].1.vblank_ns,
                period,
                "one per vblank"
            );
            assert_eq!(w[1].1.seq, w[0].1.seq + 1);
        }
        for (sent, v) in &ev {
            assert!(*sent >= v.vblank_ns, "never before its vblank");
            assert_eq!((v.vblank_ns - phase) % period, 0, "on the lattice");
        }
        assert_eq!(ev[0].1.refresh_mhz, 250_000);
    }

    #[test]
    fn no_events_without_the_ack_and_idle_when_the_source_goes() {
        let pacer = VblankPacer::new(Duration::ZERO);
        let period = 4_000_000u64;
        let start = mono_ns();
        let phase = start - 40 * period;
        for k in 0..40 {
            pacer.presented(0, KIND_VSYNC, phase + k * period);
        }
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        pacer.start(rec.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(rec.0.lock().unwrap().is_empty(), "not acked: nothing");
        pacer.set_enabled(true);
        std::thread::sleep(Duration::from_millis(30));
        pacer.client_gone(0);
        std::thread::sleep(Duration::from_millis(30));
        pacer.stop();
        std::thread::sleep(Duration::from_millis(10));
        let ev = rec.0.lock().unwrap().clone();
        assert!(ev.len() >= 3);
        assert_eq!(
            ev.last().unwrap().1.flags,
            HOST_VBLANK_IDLE,
            "one IDLE at the end"
        );
        assert_eq!(
            ev.iter()
                .filter(|(_, v)| v.flags & HOST_VBLANK_IDLE != 0)
                .count(),
            1
        );
    }
}

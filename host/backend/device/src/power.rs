// SPDX-License-Identifier: Apache-2.0
//
// Host suspend: the backend's half of `conduit _sleep` (docs/SUSPEND.md).
//
// Before the host sleeps, `conduit _sleep` pauses the VM's vCPUs (QMP `stop`)
// and then asks the backend, on its power socket (`--power-socket`), to
// `hold`: stop serving guest requests and wait, for a bounded time, until
// every host fence handed to the guest has signalled and every fenced Venus
// command has been returned. Requests stay on the virtqueue untouched while
// the hold lasts. After the host is back it says `release`: the backend asks
// RM about every guest channel group it knows (a harmless control, so a GPU
// that did not come back answers "lost" instead of hanging), serves requests
// again and reports. Then the VM is continued (QMP `cont`).
//
// QEMU stops a vhost-user device when the VM pauses and starts it again on
// `cont`. The backend took every such restart that came after served
// requests for a guest reboot and released the guest's GPU objects. `hold`
// and `keep` arm [`Keep`]: the next restart keeps them instead.
//
// The protocol is one line per connection and one line of answer:
//
//   hold MS    stop serving, wait up to MS ms for in-flight GPU work
//              -> "held idle fences=0 chains=0 waited_ms=3"
//              -> "held busy fences=2 chains=0 waited_ms=2000"
//              -> "busy request in flight; not held" (a request did not
//                 finish in time; the restart is still kept)
//   release    check the GPU, serve requests again
//              -> "released healthy tsgs=3"
//              -> "released lost tsgs=3 lost=3 status=0xf"
//   keep       keep the guest's GPU objects across the next restart
//              (`conduit pause`) -> "kept"
//   status     -> "running keep=off fences=0 tsgs=2" / "held for_ms=812 ..."
//
// A hold that nobody releases ends by itself after [`LEASE`] of monotonic
// time (which does not advance while the host sleeps), so a sleep hook that
// died cannot leave a VM's GPU frozen.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// How long a hold lasts without a `release`, in monotonic time.
pub const LEASE: Duration = Duration::from_secs(120);
/// After `release`, how long the next device restart is still kept: QMP
/// `cont` follows within a second or two, but a resume that waits for the
/// GPU's own resume service may take longer.
pub const RESTART_GRACE: Duration = Duration::from_secs(60);
/// The longest wait a `hold` may ask for.
pub const MAX_WAIT: Duration = Duration::from_secs(10);
/// How often the drain is checked while waiting.
pub const DRAIN_STEP: Duration = Duration::from_millis(2);

/// Host fences the event pump is watching for the guest and has not yet
/// reported (`event_pump` publishes it once per pass).
static PENDING_FENCES: AtomicUsize = AtomicUsize::new(0);
/// Set by `release` when the GPU is lost: the event pump reports every
/// pending fence with [`LOST_STATUS`] and forgets it, so guest waits end in
/// an error instead of never.
static FAIL_FENCES: AtomicBool = AtomicBool::new(false);

/// The status a fence is reported with when the GPU did not come back.
pub const LOST_STATUS: i32 = -libc::ENODEV;

pub fn set_pending_fences(n: usize) {
    PENDING_FENCES.store(n, Relaxed);
}

pub fn pending_fences() -> usize {
    PENDING_FENCES.load(Relaxed)
}

pub fn request_fail_fences() {
    FAIL_FENCES.store(true, Relaxed);
}

/// The event pump's check, once per pass: one relaxed load while nothing is asked.
#[inline]
pub fn take_fail_fences() -> bool {
    FAIL_FENCES.load(Relaxed) && FAIL_FENCES.swap(false, Relaxed)
}

/// A command on the power socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Hold { wait: Duration },
    Release,
    Keep,
    Status,
}

impl Command {
    pub fn parse(line: &str) -> Result<Command, String> {
        let mut w = line.split_whitespace();
        let cmd = w.next().unwrap_or("");
        let arg = w.next();
        if w.next().is_some() {
            return Err(format!("too many words in {:?}", line.trim()));
        }
        match (cmd, arg) {
            ("hold", Some(ms)) => {
                let ms: u64 = ms
                    .parse()
                    .map_err(|_| format!("hold: {ms:?} is not a number of milliseconds"))?;
                Ok(Command::Hold {
                    wait: Duration::from_millis(ms).min(MAX_WAIT),
                })
            }
            ("hold", None) => Ok(Command::Hold {
                wait: Duration::from_secs(2),
            }),
            ("release", None) => Ok(Command::Release),
            ("keep", None) => Ok(Command::Keep),
            ("status", None) => Ok(Command::Status),
            ("release" | "keep" | "status", Some(_)) => Err(format!("{cmd} takes no argument")),
            _ => Err(format!(
                "unknown command {:?} (hold MS, release, keep, status)",
                line.trim()
            )),
        }
    }
}

/// Whether the next device restart keeps the guest's GPU objects.
///
/// Armed by `hold` and `keep` with no end; `release` gives it
/// [`RESTART_GRACE`] to be used; the restart uses it up. Without it a restart
/// after served requests is a guest reboot, as before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keep {
    armed: bool,
    until: Option<Instant>,
}

impl Keep {
    pub fn arm(&mut self) {
        self.armed = true;
        self.until = None;
    }

    /// `release`: an armed keep now ends at `now + RESTART_GRACE`.
    pub fn released(&mut self, now: Instant) {
        if self.armed {
            self.until = Some(now + RESTART_GRACE);
        }
    }

    pub fn is_armed(&self, now: Instant) -> bool {
        self.armed && self.until.is_none_or(|u| now < u)
    }

    /// A device restart: true when it keeps the guest's objects. Either way
    /// the keep is spent.
    pub fn consume(&mut self, now: Instant) -> bool {
        let keep = self.is_armed(now);
        *self = Keep::default();
        keep
    }
}

/// How a `hold` found the GPU work the guest had in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drain {
    pub idle: bool,
    pub fences: usize,
    pub chains: usize,
    pub waited: Duration,
}

/// Wait until `poll` (pending fences, held chains) says nothing is in flight,
/// or `wait` has passed.
pub fn drain(wait: Duration, mut poll: impl FnMut() -> (usize, usize)) -> Drain {
    let t = Instant::now();
    loop {
        let (fences, chains) = poll();
        let idle = fences == 0 && chains == 0;
        let waited = t.elapsed();
        if idle || waited >= wait {
            return Drain {
                idle,
                fences,
                chains,
                waited,
            };
        }
        std::thread::sleep(DRAIN_STEP.min(wait - waited));
    }
}

impl Drain {
    pub fn reply(&self) -> String {
        format!(
            "held {} fences={} chains={} waited_ms={}",
            if self.idle { "idle" } else { "busy" },
            self.fences,
            self.chains,
            self.waited.as_millis()
        )
    }
}

// RM statuses (nvstatuscodes.h) the health probe tells apart.
const NV_OK: u32 = 0x00;
const NV_ERR_GPU_IS_LOST: u32 = 0x0F;
const NV_ERR_GPU_IN_FULLCHIP_RESET: u32 = 0x10;
const NV_ERR_GPU_NOT_FULL_POWER: u32 = 0x11;
const NV_ERR_INVALID_CLIENT: u32 = 0x23;
const NV_ERR_INVALID_OBJECT_HANDLE: u32 = 0x33;
const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;
const NV_ERR_RESET_REQUIRED: u32 = 0x62;
const NV_ERR_RESOURCE_LOST: u32 = 0x6E;

/// What one probe of a guest channel group said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// RM answered.
    Alive,
    /// The object or its file is gone (the guest freed it): forget it.
    Stale,
    /// The GPU, or this object's state on it, did not survive.
    Lost(u32),
}

/// The RM status of a probe, or the ioctl's errno as `Err`.
pub fn classify(r: Result<u32, i32>) -> Probe {
    match r {
        Ok(NV_OK) => Probe::Alive,
        Ok(NV_ERR_INVALID_CLIENT | NV_ERR_INVALID_OBJECT_HANDLE | NV_ERR_OBJECT_NOT_FOUND) => {
            Probe::Stale
        }
        Ok(
            s @ (NV_ERR_GPU_IS_LOST
            | NV_ERR_GPU_IN_FULLCHIP_RESET
            | NV_ERR_GPU_NOT_FULL_POWER
            | NV_ERR_RESET_REQUIRED
            | NV_ERR_RESOURCE_LOST),
        ) => Probe::Lost(s),
        // Any other answer came from a GPU that is there.
        Ok(_) => Probe::Alive,
        Err(e) if e == libc::EBADF => Probe::Stale,
        // The device node failing outright: the GPU is gone (EIO, ENODEV).
        Err(e) => Probe::Lost(0x8000_0000 | e as u32),
    }
}

/// The GPU after the host's resume, as the guest's channel groups show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Every live channel group answered (none at all is healthy too).
    Healthy { checked: usize },
    /// `lost` of `checked` did not; `status` is the first lost answer.
    Lost {
        checked: usize,
        lost: usize,
        status: u32,
    },
}

pub fn judge(probes: &[Probe]) -> Health {
    let checked = probes.iter().filter(|p| **p != Probe::Stale).count();
    let mut lost = probes.iter().filter_map(|p| match p {
        Probe::Lost(s) => Some(*s),
        _ => None,
    });
    match lost.next() {
        None => Health::Healthy { checked },
        Some(status) => Health::Lost {
            checked,
            lost: 1 + lost.count(),
            status,
        },
    }
}

impl Health {
    pub fn reply(&self) -> String {
        match self {
            Health::Healthy { checked } => format!("released healthy tsgs={checked}"),
            Health::Lost {
                checked,
                lost,
                status,
            } => format!("released lost tsgs={checked} lost={lost} status={status:#x}"),
        }
    }
}

/// Where a hold stands, for its lease and `status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hold {
    since: Option<Instant>,
}

impl Hold {
    pub fn start(&mut self, now: Instant) {
        self.since = Some(now);
    }

    pub fn end(&mut self) {
        self.since = None;
    }

    pub fn is_held(&self) -> bool {
        self.since.is_some()
    }

    /// How long until the lease runs out; `None` when not held.
    pub fn left(&self, now: Instant) -> Option<Duration> {
        self.since
            .map(|s| (s + LEASE).saturating_duration_since(now))
    }

    pub fn status(&self, now: Instant, keep: &Keep, fences: usize, tsgs: usize) -> String {
        let head = match self.since {
            Some(s) => format!(
                "held for_ms={}",
                now.saturating_duration_since(s).as_millis()
            ),
            None => "running".to_string(),
        };
        format!(
            "{head} keep={} fences={fences} tsgs={tsgs}",
            if keep.is_armed(now) { "armed" } else { "off" }
        )
    }
}

/// What [`serve`] works on: the backend, or a fake in tests.
pub trait Target {
    /// Held while no guest request may be served.
    type Gate<'a>
    where
        Self: 'a;
    /// The gate, if no request is being served right now.
    fn try_gate(&self) -> Option<Self::Gate<'_>>;
    /// In flight: host fences the guest waits on, fenced Venus commands.
    fn in_flight(&self) -> (usize, usize);
    /// Ask RM about the guest's channel groups.
    fn probe(&self) -> Health;
    fn tsgs(&self) -> usize;
    fn keep(&self) -> &std::sync::Mutex<Keep>;
}

/// The gate, waiting for a request being served to finish until `deadline`.
fn gate<T: Target>(t: &T, deadline: Instant) -> Option<T::Gate<'_>> {
    loop {
        if let Some(g) = t.try_gate() {
            return Some(g);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn keep_of<T: Target>(t: &T) -> std::sync::MutexGuard<'_, Keep> {
    t.keep().lock().unwrap_or_else(|p| p.into_inner())
}

/// The power socket's thread (`--power-socket`). Holding is holding the
/// request gate on this thread until `release`, or until [`LEASE`] runs
/// out, so no request is served meanwhile.
pub fn serve<T: Target>(listener: std::os::unix::net::UnixListener, t: T) {
    use std::io::{BufRead, Write};
    use std::os::fd::AsRawFd;
    let mut hold = Hold::default();
    let mut gated: Option<T::Gate<'_>> = None;
    loop {
        let timeout = hold
            .left(Instant::now())
            .map_or(-1, |d| d.as_millis().clamp(1, i32::MAX as u128) as i32);
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd on the listener this function owns.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("power: poll: {e}; the power socket is closed");
            return;
        }
        if n == 0 {
            if hold.left(Instant::now()) == Some(Duration::ZERO) {
                gated = None;
                hold.end();
                log::warn!(
                    "power: hold not released within {} s; serving guest requests again",
                    LEASE.as_secs()
                );
            }
            continue;
        }
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        if std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .is_err()
        {
            continue;
        }
        let reply = match Command::parse(&line) {
            Err(e) => format!("error {e}"),
            Ok(Command::Status) => {
                let keep = *keep_of(&t);
                hold.status(Instant::now(), &keep, pending_fences(), t.tsgs())
            }
            Ok(Command::Keep) => {
                keep_of(&t).arm();
                log::info!("power: keeping the guest's GPU objects across the next restart");
                "kept".to_string()
            }
            Ok(Command::Hold { wait }) => {
                let start = Instant::now();
                keep_of(&t).arm();
                if gated.is_none() {
                    gated = gate(&t, start + wait);
                }
                if gated.is_some() {
                    if !hold.is_held() {
                        hold.start(start);
                    }
                    let mut d = drain(wait.saturating_sub(start.elapsed()), || t.in_flight());
                    d.waited = start.elapsed();
                    log::info!(
                        "power: guest requests held for a host suspend: {}",
                        d.reply()
                    );
                    d.reply()
                } else {
                    log::warn!(
                        "power: a guest request was still being served after {} ms; not held",
                        wait.as_millis()
                    );
                    "busy request in flight; not held".to_string()
                }
            }
            Ok(Command::Release) => {
                let health = t.probe();
                if let Health::Lost { .. } = health {
                    request_fail_fences();
                    log::error!(
                        "power: the GPU did not come back from the host suspend ({}); the \
                         guest's pending fences end in an error",
                        health.reply()
                    );
                } else {
                    log::info!("power: after the host resume: {}", health.reply());
                }
                keep_of(&t).released(Instant::now());
                if hold.is_held() {
                    gated = None;
                    hold.end();
                    log::info!("power: serving guest requests again");
                }
                health.reply()
            }
        };
        let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(
            Command::parse("hold 1500\n"),
            Ok(Command::Hold {
                wait: Duration::from_millis(1500)
            })
        );
        assert_eq!(
            Command::parse("hold 999999"),
            Ok(Command::Hold { wait: MAX_WAIT }),
            "capped"
        );
        assert_eq!(
            Command::parse("hold"),
            Ok(Command::Hold {
                wait: Duration::from_secs(2)
            })
        );
        assert_eq!(Command::parse(" release "), Ok(Command::Release));
        assert_eq!(Command::parse("keep"), Ok(Command::Keep));
        assert_eq!(Command::parse("status"), Ok(Command::Status));
        for bad in ["", "hold x", "hold 1 2", "release now", "reboot"] {
            assert!(Command::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn keep_is_used_once_and_expires_after_release() {
        let t = Instant::now();
        let mut k = Keep::default();
        assert!(!k.consume(t), "unarmed: a restart is a reboot");
        k.arm();
        assert!(
            k.is_armed(t + Duration::from_secs(3600)),
            "no end before release"
        );
        assert!(k.consume(t));
        assert!(!k.consume(t), "spent");

        k.arm();
        k.released(t);
        assert!(k.is_armed(t + RESTART_GRACE - Duration::from_millis(1)));
        assert!(!k.is_armed(t + RESTART_GRACE));
        assert!(!k.consume(t + RESTART_GRACE + Duration::from_secs(1)));

        // A release with nothing armed arms nothing.
        let mut k = Keep::default();
        k.released(t);
        assert!(!k.is_armed(t));
    }

    #[test]
    fn drain_stops_when_idle_or_at_the_deadline() {
        let mut n = 3;
        let d = drain(Duration::from_secs(5), || {
            n -= 1;
            (n, 0)
        });
        assert!(d.idle);
        assert_eq!((d.fences, d.chains), (0, 0));
        assert!(d.waited < Duration::from_secs(5));
        assert!(d.reply().starts_with("held idle fences=0 chains=0"));

        let d = drain(Duration::from_millis(20), || (2, 1));
        assert!(!d.idle);
        assert!(d.waited >= Duration::from_millis(20));
        assert!(d.reply().starts_with("held busy fences=2 chains=1"));

        let d = drain(Duration::ZERO, || (0, 0));
        assert!(d.idle);
    }

    #[test]
    fn probes_are_classified() {
        assert_eq!(classify(Ok(0)), Probe::Alive);
        assert_eq!(classify(Ok(0x57)), Probe::Stale);
        assert_eq!(classify(Ok(0x33)), Probe::Stale);
        assert_eq!(classify(Err(libc::EBADF)), Probe::Stale);
        assert_eq!(classify(Ok(0x0F)), Probe::Lost(0x0F));
        assert_eq!(classify(Ok(0x62)), Probe::Lost(0x62));
        assert_eq!(
            classify(Ok(0x1F)),
            Probe::Alive,
            "another error: the GPU answered"
        );
        assert_eq!(
            classify(Err(libc::EIO)),
            Probe::Lost(0x8000_0000 | libc::EIO as u32)
        );
    }

    #[test]
    fn health_is_judged_from_live_probes() {
        assert_eq!(judge(&[]), Health::Healthy { checked: 0 });
        assert_eq!(
            judge(&[Probe::Alive, Probe::Stale, Probe::Alive]),
            Health::Healthy { checked: 2 }
        );
        let h = judge(&[
            Probe::Alive,
            Probe::Lost(0x0F),
            Probe::Lost(0x10),
            Probe::Stale,
        ]);
        assert_eq!(
            h,
            Health::Lost {
                checked: 3,
                lost: 2,
                status: 0x0F
            }
        );
        assert_eq!(h.reply(), "released lost tsgs=3 lost=2 status=0xf");
        assert_eq!(
            Health::Healthy { checked: 2 }.reply(),
            "released healthy tsgs=2"
        );
    }

    #[test]
    fn hold_lease_and_status() {
        let t = Instant::now();
        let mut h = Hold::default();
        assert_eq!(h.left(t), None);
        let k = Keep::default();
        assert_eq!(h.status(t, &k, 0, 2), "running keep=off fences=0 tsgs=2");
        h.start(t);
        assert!(h.is_held());
        assert_eq!(
            h.left(t + Duration::from_secs(20)),
            Some(LEASE - Duration::from_secs(20))
        );
        assert_eq!(
            h.left(t + LEASE + Duration::from_secs(1)),
            Some(Duration::ZERO)
        );
        let mut k = Keep::default();
        k.arm();
        assert_eq!(
            h.status(t + Duration::from_millis(812), &k, 1, 3),
            "held for_ms=812 keep=armed fences=1 tsgs=3"
        );
        h.end();
        assert!(!h.is_held());
    }

    /// The fail-fences flag is process-wide: its tests take turns.
    static FLAG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn fail_fences_is_taken_once() {
        let _turn = FLAG.lock().unwrap_or_else(|p| p.into_inner());
        assert!(!take_fail_fences());
        request_fail_fences();
        assert!(take_fail_fences());
        assert!(!take_fail_fences());
    }

    #[derive(Default)]
    struct Fake {
        gate: std::sync::Mutex<()>,
        flight: std::sync::Mutex<(usize, usize)>,
        lost: AtomicBool,
        keep: std::sync::Mutex<Keep>,
    }

    impl Target for std::sync::Arc<Fake> {
        type Gate<'a> = std::sync::MutexGuard<'a, ()>;
        fn try_gate(&self) -> Option<Self::Gate<'_>> {
            self.gate.try_lock().ok()
        }
        fn in_flight(&self) -> (usize, usize) {
            *self.flight.lock().unwrap()
        }
        fn probe(&self) -> Health {
            if self.lost.load(Relaxed) {
                judge(&[Probe::Lost(0x0F)])
            } else {
                judge(&[Probe::Alive])
            }
        }
        fn tsgs(&self) -> usize {
            1
        }
        fn keep(&self) -> &std::sync::Mutex<Keep> {
            &self.keep
        }
    }

    fn ask(sock: &std::path::Path, cmd: &str) -> String {
        use std::io::{Read, Write};
        let mut s = std::os::unix::net::UnixStream::connect(sock).unwrap();
        writeln!(s, "{cmd}").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out.trim_end().to_string()
    }

    /// The socket end to end: hold gates requests and reports the drain,
    /// release probes and ungates, a request in flight is never waited on
    /// past the deadline, and a lost GPU fails the guest's fences.
    #[test]
    fn the_power_socket_holds_and_releases() {
        let _turn = FLAG.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("conduit-power-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("power.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fake = std::sync::Arc::new(Fake::default());
        let t = fake.clone();
        std::thread::spawn(move || serve(listener, t));

        assert!(ask(&sock, "status").starts_with("running keep=off"));
        assert!(ask(&sock, "launch").starts_with("error unknown command"));

        *fake.flight.lock().unwrap() = (1, 0);
        let r = ask(&sock, "hold 30");
        assert!(r.starts_with("held busy fences=1 chains=0"), "{r}");
        assert!(fake.gate.try_lock().is_err(), "requests are gated");
        assert!(ask(&sock, "status").contains("keep=armed"));
        assert!(ask(&sock, "status").starts_with("held for_ms="));
        *fake.flight.lock().unwrap() = (0, 0);
        let r = ask(&sock, "hold 30");
        assert!(
            r.starts_with("held idle fences=0 chains=0"),
            "a second hold: {r}"
        );

        assert_eq!(ask(&sock, "release"), "released healthy tsgs=1");
        assert!(fake.gate.try_lock().is_ok(), "served again");
        assert!(
            fake.keep.lock().unwrap().is_armed(Instant::now()),
            "for the restart"
        );
        assert!(fake.keep.lock().unwrap().consume(Instant::now()));

        // A request being served when the hold comes: not waited on for ever.
        {
            let _busy = fake.gate.lock().unwrap();
            assert_eq!(ask(&sock, "hold 20"), "busy request in flight; not held");
        }
        assert!(
            fake.keep.lock().unwrap().is_armed(Instant::now()),
            "the restart is kept anyway"
        );
        assert_eq!(ask(&sock, "keep"), "kept");

        fake.lost.store(true, Relaxed);
        assert!(!take_fail_fences());
        assert_eq!(
            ask(&sock, "release"),
            "released lost tsgs=1 lost=1 status=0xf"
        );
        assert!(take_fail_fences(), "the guest's fences end in an error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

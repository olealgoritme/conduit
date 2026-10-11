// SPDX-License-Identifier: Apache-2.0
//
// virtio-input devices served by this backend (vhost-user-input-pci): a
// relative mouse, a keyboard and an absolute tablet that the guest's own
// virtio-input driver reads (Linux `virtio_input`, Windows `vioinput`).
//
// The display link writes the viewer's input straight into the devices'
// event queues and interrupts the guest through KVM (the call eventfd):
// nothing on the way runs in QEMU, no JSON is parsed, no PS/2 byte is read
// through an emulated port, and no event waits for another. Each SYN the
// viewer sends is one report in the guest.
//
// This file holds what needs no vhost-user crate: the virtio-input config
// space of each device (`Kind`, `Config`) and the routing (`Router`) that
// decides, event by event, whether a device takes it or the boot console
// (QEMU's emulated PS/2 keyboard and mouse and its USB tablet, over VNC and
// QMP) does. The vhost-user side is in `bin/vinput.rs`.
//
// A device takes input only while its guest driver runs it (`Port::ready`):
// the event queue is live and the driver has posted buffers. Before that --
// firmware, boot loader, a Windows without vioinput -- everything goes to
// the console as before.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::messages::{INPUT_ABS_MAX, InputEventEntry, input};

/// `VIRTIO_ID_INPUT`.
pub const VIRTIO_ID_INPUT: u32 = 18;
/// The event queue (device to driver) and the status queue (LEDs, driver to device).
pub const EVENT_QUEUE: usize = 0;
pub const STATUS_QUEUE: usize = 1;
pub const NUM_QUEUES: usize = 2;
/// The largest queue a frontend may ask for. QEMU's vhost-user-input asks for 4.
pub const MAX_QUEUE_SIZE: usize = 256;

/// Size of `struct virtio_input_config`.
pub const CONFIG_SIZE: usize = 136;

const CFG_UNSET: u8 = 0x00;
const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

const EV_LED: u16 = 0x11;
const EV_REP: u16 = 0x14;
const LED_NUML: u16 = 0;
const LED_CAPSL: u16 = 1;
const LED_SCROLLL: u16 = 2;
/// `BUS_VIRTUAL`.
const BUS_VIRTUAL: u16 = 0x06;

/// Linux button codes (input-event-codes.h).
pub const BTN_MISC: u16 = 0x100;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
const KEY_OK: u16 = 0x160;
/// The keyboard's keys: every code below the buttons that a PC keyboard or
/// its media keys can send (KEY_ESC .. KEY_MICMUTE), as QEMU's
/// virtio-keyboard offers.
const KEY_FIRST: u16 = 1;
const KEY_LAST: u16 = 248;
/// A HID mouse report carries one byte per relative axis (vioinput's
/// descriptor: -127..=127); a larger step is sent as several reports.
pub const REL_STEP_MAX: i32 = 127;

/// The console-only event the display link adds when the viewer's grab
/// changes (`crate::console::EV_GRAB_MARK`).
use crate::console::EV_GRAB_MARK;

/// Which device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Relative motion, buttons and wheel: what a game reads under a grab.
    Mouse = 0,
    Keyboard = 1,
    /// Absolute positions in `0..=INPUT_ABS_MAX`, buttons and wheel: the
    /// desktop pointer without a grab.
    Tablet = 2,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Mouse, Kind::Keyboard, Kind::Tablet];

    /// The socket file name a frontend connects to, in the VM's run folder.
    pub fn socket_name(self) -> &'static str {
        match self {
            Kind::Mouse => "input-mouse.sock",
            Kind::Keyboard => "input-keyboard.sock",
            Kind::Tablet => "input-tablet.sock",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Mouse => "mouse",
            Kind::Keyboard => "keyboard",
            Kind::Tablet => "tablet",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    /// The kind whose socket this is, by file name.
    pub fn from_socket_path(p: &std::path::Path) -> Option<Kind> {
        let n = p.file_name()?.to_str()?;
        Kind::ALL.into_iter().find(|k| k.socket_name() == n)
    }

    fn device_name(self) -> &'static str {
        match self {
            Kind::Mouse => "Conduit Mouse",
            Kind::Keyboard => "Conduit Keyboard",
            Kind::Tablet => "Conduit Tablet",
        }
    }

    /// Whether this device reports key or button `code`.
    pub fn has_key(self, code: u16) -> bool {
        match self {
            Kind::Keyboard => (KEY_FIRST..=KEY_LAST).contains(&code),
            Kind::Mouse | Kind::Tablet => (BTN_LEFT..=BTN_EXTRA).contains(&code),
        }
    }

    /// The codes this device reports for event type `ev`.
    fn codes(self, ev: u16) -> Vec<u16> {
        match (self, ev) {
            (k, input::EV_KEY) => (0..KEY_OK).filter(|&c| k.has_key(c)).collect(),
            (Kind::Mouse, input::EV_REL) => vec![
                input::REL_X,
                input::REL_Y,
                input::REL_HWHEEL,
                input::REL_WHEEL,
            ],
            (Kind::Tablet, input::EV_REL) => vec![input::REL_HWHEEL, input::REL_WHEEL],
            (Kind::Tablet, input::EV_ABS) => vec![input::ABS_X, input::ABS_Y],
            (Kind::Keyboard, EV_LED) => vec![LED_NUML, LED_CAPSL, LED_SCROLLL],
            (Kind::Keyboard, EV_REP) => vec![0],
            _ => Vec::new(),
        }
    }

    fn product(self) -> u16 {
        self as u16 + 1
    }
}

/// One device's config space (`struct virtio_input_config`): the driver
/// writes `select` and `subsel` and reads what they name.
#[derive(Clone, Debug)]
pub struct Config {
    kind: Kind,
    select: u8,
    subsel: u8,
}

impl Config {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            select: CFG_UNSET,
            subsel: 0,
        }
    }

    /// The payload `select`/`subsel` name now.
    fn payload(&self) -> Vec<u8> {
        let k = self.kind;
        match self.select {
            CFG_ID_NAME => k.device_name().as_bytes().to_vec(),
            CFG_ID_SERIAL => b"0".to_vec(),
            CFG_ID_DEVIDS => {
                let mut v = Vec::with_capacity(8);
                v.extend_from_slice(&BUS_VIRTUAL.to_le_bytes());
                // QEMU's vendor id, as its own virtio-input devices use.
                v.extend_from_slice(&0x0627u16.to_le_bytes());
                v.extend_from_slice(&k.product().to_le_bytes());
                v.extend_from_slice(&1u16.to_le_bytes());
                v
            }
            CFG_PROP_BITS => Vec::new(),
            CFG_EV_BITS => {
                let codes = k.codes(u16::from(self.subsel));
                let Some(&max) = codes.iter().max() else {
                    return Vec::new();
                };
                let mut bits = vec![0u8; usize::from(max) / 8 + 1];
                for c in codes {
                    bits[usize::from(c) / 8] |= 1 << (c % 8);
                }
                bits
            }
            CFG_ABS_INFO if k == Kind::Tablet && u16::from(self.subsel) <= input::ABS_Y => {
                // min, max, fuzz, flat, res
                let mut v = Vec::with_capacity(20);
                for x in [0, INPUT_ABS_MAX, 0, 0, 0] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            _ => Vec::new(),
        }
    }

    /// The whole config space as the driver reads it now.
    pub fn bytes(&self) -> [u8; CONFIG_SIZE] {
        let mut b = [0u8; CONFIG_SIZE];
        let p = self.payload();
        let n = p.len().min(128);
        b[0] = self.select;
        b[1] = self.subsel;
        b[2] = n as u8;
        b[8..8 + n].copy_from_slice(&p[..n]);
        b
    }

    pub fn read(&self, offset: u32, size: u32) -> Vec<u8> {
        let b = self.bytes();
        let start = (offset as usize).min(CONFIG_SIZE);
        let end = (offset as usize)
            .saturating_add(size as usize)
            .min(CONFIG_SIZE);
        b[start..end].to_vec()
    }

    /// A driver write; only `select` (offset 0) and `subsel` (1) are writable.
    pub fn write(&mut self, offset: u32, data: &[u8]) {
        for (i, &v) in data.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = v,
                1 => self.subsel = v,
                _ => {}
            }
        }
    }
}

/// A device's event queue, as the router sees it.
pub trait Port: Send + Sync {
    /// The guest driver runs the device: its event queue is live and has
    /// had buffers posted. Cheap; asked once per batch of input.
    fn ready(&self) -> bool;
    /// Queue these events for the guest and interrupt it. Never blocks: what
    /// finds no buffer waits for the driver's next one.
    fn send(&self, events: &[InputEventEntry]);
}

/// Where an event went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum To {
    Dev(Kind),
    Console,
}

/// Counts for the periodic log line.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterStats {
    pub mouse_reports: u64,
    /// Mouse reports added because a step was over [`REL_STEP_MAX`].
    pub mouse_split: u64,
    pub keys: u64,
    pub tablet_reports: u64,
    pub to_console: u64,
}

/// Splits the link's console-bound input between the virtio devices and the
/// boot console. Keeps what each device holds pressed, so a release always
/// goes where its press went, and a device that stops (guest reset, driver
/// unloaded) is forgotten -- its driver's reset let go of everything.
pub struct Router {
    ports: [Option<Arc<dyn Port>>; 3],
    ready: [bool; 3],
    grab: bool,
    /// Keys and buttons pressed on a device, by code.
    held: HashMap<u16, Kind>,
    /// Keys and buttons pressed on the console: their repeats and releases
    /// stay there, even once a device is ready.
    console_held: HashSet<u16>,
    /// The report being built per device, until its SYN.
    report: [Vec<InputEventEntry>; 3],
    /// Relative motion of the mouse report being built.
    dx: i64,
    dy: i64,
    /// Finished reports per device, sent at the end of the batch.
    out: [Vec<InputEventEntry>; 3],
    /// The console got something since its last SYN.
    console_dirty: bool,
    stats: RouterStats,
    logged: RouterStats,
    log_at: Instant,
}

impl Router {
    pub fn new(ports: [Option<Arc<dyn Port>>; 3]) -> Self {
        Self {
            ports,
            ready: [false; 3],
            grab: false,
            held: HashMap::new(),
            console_held: HashSet::new(),
            report: Default::default(),
            dx: 0,
            dy: 0,
            out: Default::default(),
            console_dirty: false,
            stats: RouterStats::default(),
            logged: RouterStats::default(),
            log_at: Instant::now(),
        }
    }

    pub fn stats(&self) -> RouterStats {
        self.stats
    }

    fn refresh(&mut self) {
        for k in Kind::ALL {
            let r = self.ports[k as usize].as_ref().is_some_and(|p| p.ready());
            let was = self.ready[k as usize];
            if r != was {
                log::info!(
                    "virtio-input: {} {}",
                    k.name(),
                    if r {
                        "driver ready: its input goes straight to the guest's event queue"
                    } else {
                        "stopped: its input goes to the boot console (QEMU's emulated devices)"
                    }
                );
                if !r {
                    self.held.retain(|_, d| *d != k);
                    self.report[k as usize].clear();
                    if k == Kind::Mouse {
                        self.dx = 0;
                        self.dy = 0;
                    }
                }
                self.ready[k as usize] = r;
            }
        }
    }

    fn ok(&self, k: Kind) -> bool {
        self.ready[k as usize]
    }

    /// Where a press of `code` goes.
    fn press_target(&self, code: u16) -> To {
        let k = if (BTN_MISC..KEY_OK).contains(&code) {
            if self.grab { Kind::Mouse } else { Kind::Tablet }
        } else {
            Kind::Keyboard
        };
        if self.ok(k) && k.has_key(code) {
            To::Dev(k)
        } else {
            To::Console
        }
    }

    /// Take the events the devices take out of `events`; what stays is the
    /// console's, in order. Sends to the devices before returning.
    pub fn route(&mut self, events: &mut Vec<InputEventEntry>) {
        self.refresh();
        let mut keep = Vec::with_capacity(events.len());
        for e in events.drain(..) {
            match self.event(&e) {
                To::Console => {
                    if e.ev_type == input::EV_SYN {
                        if std::mem::take(&mut self.console_dirty) {
                            keep.push(e);
                        }
                    } else {
                        if e.ev_type != EV_GRAB_MARK {
                            self.console_dirty = true;
                            self.stats.to_console += 1;
                        }
                        keep.push(e);
                    }
                }
                To::Dev(_) => {}
            }
        }
        *events = keep;
        for k in Kind::ALL {
            let out = std::mem::take(&mut self.out[k as usize]);
            if out.is_empty() {
                continue;
            }
            if let Some(p) = self.ports[k as usize].as_ref() {
                p.send(&out);
            }
            self.out[k as usize] = out;
            self.out[k as usize].clear();
        }
        self.maybe_log();
    }

    /// One event: where it goes. Device events are added to their report.
    fn event(&mut self, e: &InputEventEntry) -> To {
        match e.ev_type {
            EV_GRAB_MARK => {
                let on = e.value != 0;
                if self.grab && !on {
                    self.release_on(Kind::Mouse);
                }
                self.grab = on;
                To::Console // the console's encoder follows the grab too
            }
            input::EV_KEY => {
                if e.value == 0 {
                    return match self.held.remove(&e.code) {
                        Some(k) => {
                            self.report[k as usize].push(*e);
                            To::Dev(k)
                        }
                        None => {
                            self.console_held.remove(&e.code);
                            To::Console
                        }
                    };
                }
                if let Some(&k) = self.held.get(&e.code) {
                    // Autorepeat: the guest repeats a held key itself.
                    return To::Dev(k);
                }
                if self.console_held.contains(&e.code) {
                    // A repeat of a key the console holds: a device that
                    // became ready since must not take it, or the console
                    // never sees its release.
                    return To::Console;
                }
                match self.press_target(e.code) {
                    To::Dev(k) => {
                        self.held.insert(e.code, k);
                        self.report[k as usize].push(InputEventEntry::new(
                            input::EV_KEY,
                            e.code,
                            1,
                        ));
                        To::Dev(k)
                    }
                    To::Console => {
                        self.console_held.insert(e.code);
                        To::Console
                    }
                }
            }
            input::EV_REL => match e.code {
                input::REL_X | input::REL_Y => {
                    // Relative motion comes only under a grab: one that began
                    // before this backend counts too.
                    self.grab = true;
                    if !self.ok(Kind::Mouse) {
                        return To::Console;
                    }
                    if e.code == input::REL_X {
                        self.dx = self.dx.saturating_add(i64::from(e.value));
                    } else {
                        self.dy = self.dy.saturating_add(i64::from(e.value));
                    }
                    To::Dev(Kind::Mouse)
                }
                input::REL_WHEEL | input::REL_HWHEEL => {
                    let k = if self.grab { Kind::Mouse } else { Kind::Tablet };
                    if !self.ok(k) {
                        return To::Console;
                    }
                    self.report[k as usize].push(*e);
                    To::Dev(k)
                }
                input::REL_WHEEL_HI_RES | input::REL_HWHEEL_HI_RES => {
                    // The devices offer whole detents only, which the
                    // low-resolution twin carries.
                    let k = if self.grab { Kind::Mouse } else { Kind::Tablet };
                    if self.ok(k) { To::Dev(k) } else { To::Console }
                }
                _ => To::Console,
            },
            input::EV_ABS => {
                self.grab = false;
                if !matches!(e.code, input::ABS_X | input::ABS_Y) || !self.ok(Kind::Tablet) {
                    return To::Console;
                }
                self.report[Kind::Tablet as usize].push(InputEventEntry::new(
                    input::EV_ABS,
                    e.code,
                    e.value.clamp(0, INPUT_ABS_MAX),
                ));
                To::Dev(Kind::Tablet)
            }
            input::EV_SYN => {
                for k in Kind::ALL {
                    self.finish(k);
                }
                To::Console
            }
            _ => To::Console,
        }
    }

    /// Release what is held on device `k`, as a report of its own.
    fn release_on(&mut self, k: Kind) {
        let codes: Vec<u16> = self
            .held
            .iter()
            .filter(|(_, d)| **d == k)
            .map(|(c, _)| *c)
            .collect();
        if codes.is_empty() {
            return;
        }
        for c in codes {
            self.held.remove(&c);
            self.report[k as usize].push(InputEventEntry::new(input::EV_KEY, c, 0));
        }
        self.finish(k);
    }

    /// Close the report being built on `k` (its SYN), splitting motion over
    /// [`REL_STEP_MAX`] into as many reports as it takes, evenly.
    fn finish(&mut self, k: Kind) {
        let syn = InputEventEntry::new(input::EV_SYN, input::SYN_REPORT, 0);
        let i = k as usize;
        if k == Kind::Mouse && (self.dx != 0 || self.dy != 0) {
            let (dx, dy) = (std::mem::take(&mut self.dx), std::mem::take(&mut self.dy));
            let step = i64::from(REL_STEP_MAX);
            let n = (dx.unsigned_abs().max(dy.unsigned_abs())).div_ceil(step as u64) as i64;
            let n = n.max(1);
            self.stats.mouse_reports += n as u64;
            self.stats.mouse_split += (n - 1) as u64;
            let mut sent = (0i64, 0i64);
            for j in 1..=n {
                // Cumulative rounding: the parts add up to exactly (dx, dy).
                let (tx, ty) = (dx * j / n, dy * j / n);
                let (px, py) = (tx - sent.0, ty - sent.1);
                sent = (tx, ty);
                if j == 1 {
                    let r = std::mem::take(&mut self.report[i]);
                    self.out[i].extend(r);
                }
                if px != 0 {
                    self.out[i].push(InputEventEntry::new(input::EV_REL, input::REL_X, px as i32));
                }
                if py != 0 {
                    self.out[i].push(InputEventEntry::new(input::EV_REL, input::REL_Y, py as i32));
                }
                self.out[i].push(syn);
            }
            return;
        }
        if self.report[i].is_empty() {
            return;
        }
        match k {
            Kind::Mouse => self.stats.mouse_reports += 1,
            Kind::Tablet => self.stats.tablet_reports += 1,
            Kind::Keyboard => {
                self.stats.keys += self.report[i]
                    .iter()
                    .filter(|e| e.ev_type == input::EV_KEY && e.value != 0)
                    .count() as u64
            }
        }
        let r = std::mem::take(&mut self.report[i]);
        self.out[i].extend(r);
        self.out[i].push(syn);
    }

    fn maybe_log(&mut self) {
        const EVERY: Duration = Duration::from_secs(10);
        let now = Instant::now();
        if now.saturating_duration_since(self.log_at) < EVERY {
            return;
        }
        let s = self.stats;
        let l = self.logged;
        if s != l {
            log::info!(
                "virtio-input, last {} s: {} mouse reports ({} split over {REL_STEP_MAX}), {} keys, \
                 {} tablet reports; {} events to the boot console",
                now.saturating_duration_since(self.log_at).as_secs(),
                s.mouse_reports - l.mouse_reports,
                s.mouse_split - l.mouse_split,
                s.keys - l.keys,
                s.tablet_reports - l.tablet_reports,
                s.to_console - l.to_console,
            );
        }
        self.logged = s;
        self.log_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct FakePort {
        ready: AtomicBool,
        got: Mutex<Vec<Vec<InputEventEntry>>>,
    }
    impl Port for FakePort {
        fn ready(&self) -> bool {
            self.ready.load(Ordering::Relaxed)
        }
        fn send(&self, events: &[InputEventEntry]) {
            self.got.lock().unwrap().push(events.to_vec());
        }
    }
    impl FakePort {
        fn on() -> Arc<Self> {
            let p = Arc::new(Self::default());
            p.ready.store(true, Ordering::Relaxed);
            p
        }
        fn flat(&self) -> Vec<(u16, u16, i32)> {
            self.got
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .map(|e| (e.ev_type, e.code, e.value))
                .collect()
        }
        fn sends(&self) -> usize {
            self.got.lock().unwrap().len()
        }
    }

    fn ev(t: u16, c: u16, v: i32) -> InputEventEntry {
        InputEventEntry::new(t, c, v)
    }
    fn syn() -> InputEventEntry {
        ev(input::EV_SYN, input::SYN_REPORT, 0)
    }
    const S: (u16, u16, i32) = (input::EV_SYN, input::SYN_REPORT, 0);

    fn setup() -> (Router, Arc<FakePort>, Arc<FakePort>, Arc<FakePort>) {
        let (m, k, t) = (FakePort::on(), FakePort::on(), FakePort::on());
        let r = Router::new([
            Some(m.clone() as Arc<dyn Port>),
            Some(k.clone() as Arc<dyn Port>),
            Some(t.clone() as Arc<dyn Port>),
        ]);
        (r, m, k, t)
    }

    #[test]
    fn config_space_describes_each_device_as_qemu_would() {
        let mut c = Config::new(Kind::Mouse);
        c.write(0, &[CFG_ID_NAME, 0]);
        let b = c.bytes();
        assert_eq!(&b[8..8 + b[2] as usize], b"Conduit Mouse");
        c.write(0, &[CFG_EV_BITS, input::EV_REL as u8]);
        let b = c.bytes();
        assert_eq!(b[2], 2);
        assert_eq!(b[8], 0b0100_0011, "REL_X, REL_Y, REL_HWHEEL");
        assert_eq!(b[9], 0b0000_0001, "REL_WHEEL");
        c.write(0, &[CFG_EV_BITS, input::EV_KEY as u8]);
        let b = c.bytes();
        assert_eq!(b[2] as usize, 0x114 / 8 + 1);
        assert_eq!(b[8 + 0x110 / 8], 0x1f, "left, right, middle, side, extra");
        c.write(0, &[CFG_EV_BITS, input::EV_ABS as u8]);
        assert_eq!(c.bytes()[2], 0, "a mouse has no absolute axes");

        let mut t = Config::new(Kind::Tablet);
        t.write(0, &[CFG_ABS_INFO, input::ABS_Y as u8]);
        let b = t.bytes();
        assert_eq!(b[2], 20);
        assert_eq!(
            i32::from_le_bytes(b[12..16].try_into().unwrap()),
            INPUT_ABS_MAX
        );

        let mut k = Config::new(Kind::Keyboard);
        k.write(0, &[CFG_EV_BITS, input::EV_KEY as u8]);
        let b = k.bytes();
        assert_eq!(b[2] as usize, KEY_LAST as usize / 8 + 1);
        assert_eq!(b[8] & 1, 0, "KEY_RESERVED is not a key");
        assert_eq!(b[8] & 2, 2, "KEY_ESC");
        k.write(0, &[CFG_EV_BITS, EV_LED as u8]);
        assert_eq!(k.bytes()[8], 0b111);
        k.write(0, &[CFG_ID_DEVIDS, 0]);
        let b = k.bytes();
        assert_eq!(&b[8..16], &[6, 0, 0x27, 6, 2, 0, 1, 0]);
        // Partial reads, and writes past subsel ignored.
        k.write(2, &[99]);
        assert_eq!(k.read(0, 3), vec![CFG_ID_DEVIDS, 0, 8]);
        assert_eq!(k.read(130, 100).len(), 6);
    }

    #[test]
    fn grabbed_motion_goes_to_the_mouse_one_report_per_syn() {
        let (mut r, m, _, _) = setup();
        let mut b = vec![
            ev(EV_GRAB_MARK, 0, 1),
            ev(input::EV_REL, input::REL_X, 3),
            ev(input::EV_REL, input::REL_Y, -2),
            syn(),
            ev(input::EV_REL, input::REL_X, 1),
            syn(),
        ];
        r.route(&mut b);
        // The console sees the grab mark only.
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].ev_type, EV_GRAB_MARK);
        assert_eq!(
            m.flat(),
            vec![
                (input::EV_REL, input::REL_X, 3),
                (input::EV_REL, input::REL_Y, -2),
                S,
                (input::EV_REL, input::REL_X, 1),
                S
            ]
        );
        assert_eq!(m.sends(), 1, "one interrupt per batch");
    }

    /// A 1000 Hz mouse whose reports reach the link in bursts (a scheduling
    /// hiccup): every host report stays a report of its own in the guest,
    /// none merged with its neighbour, the motion of each intact.
    #[test]
    fn a_burst_of_host_reports_is_not_coalesced() {
        let (mut r, m, _, _) = setup();
        let mut b = vec![ev(EV_GRAB_MARK, 0, 1)];
        for i in 0..64 {
            b.push(ev(input::EV_REL, input::REL_X, 1 + i % 3));
            b.push(ev(input::EV_REL, input::REL_Y, -(i % 2)));
            b.push(syn());
        }
        r.route(&mut b);
        let got = m.flat();
        assert_eq!(got.iter().filter(|e| **e == S).count(), 64);
        let mut report = Vec::new();
        let mut n = 0;
        for e in got {
            if e == S {
                let want_x = 1 + n % 3;
                let want_y = -(n % 2);
                let mut x = vec![(input::EV_REL, input::REL_X, want_x)];
                if want_y != 0 {
                    x.push((input::EV_REL, input::REL_Y, want_y));
                }
                assert_eq!(report, x, "report {n}");
                report.clear();
                n += 1;
            } else {
                report.push(e);
            }
        }
        assert_eq!(r.stats().mouse_reports, 64);
        assert_eq!(r.stats().mouse_split, 0);
    }

    /// vioinput keeps only the last value of an axis before its SYN, so
    /// motion within one host report goes out as one value per axis.
    #[test]
    fn motion_within_one_report_is_one_value_per_axis() {
        let (mut r, m, _, _) = setup();
        let mut b = vec![
            ev(EV_GRAB_MARK, 0, 1),
            ev(input::EV_REL, input::REL_X, 2),
            ev(input::EV_REL, input::REL_X, 3),
            ev(input::EV_REL, input::REL_Y, 1),
        ];
        r.route(&mut b);
        assert!(m.flat().is_empty(), "nothing goes out before the SYN");
        let mut b = vec![ev(input::EV_REL, input::REL_Y, 1), syn()];
        r.route(&mut b);
        assert_eq!(
            m.flat(),
            vec![
                (input::EV_REL, input::REL_X, 5),
                (input::EV_REL, input::REL_Y, 2),
                S
            ]
        );
    }

    #[test]
    fn a_large_step_is_split_into_reports_that_add_up() {
        let (mut r, m, _, _) = setup();
        let mut b = vec![
            ev(EV_GRAB_MARK, 0, 1),
            ev(input::EV_KEY, BTN_LEFT, 1),
            ev(input::EV_REL, input::REL_X, 300),
            ev(input::EV_REL, input::REL_Y, -50),
            syn(),
        ];
        r.route(&mut b);
        let got = m.flat();
        assert_eq!(
            got[0],
            (input::EV_KEY, BTN_LEFT, 1),
            "the button rides the first report"
        );
        let reports = got.iter().filter(|e| **e == S).count();
        assert_eq!(reports, 3);
        let (mut sx, mut sy) = (0, 0);
        for &(t, c, v) in &got {
            if t == input::EV_REL {
                assert!(v.abs() <= REL_STEP_MAX);
                if c == input::REL_X { sx += v } else { sy += v }
            }
        }
        assert_eq!((sx, sy), (300, -50));
        assert_eq!(r.stats().mouse_split, 2);
    }

    #[test]
    fn keys_go_to_the_keyboard_and_releases_follow_their_press() {
        let (mut r, _, k, _) = setup();
        let mut b = vec![ev(input::EV_KEY, 30, 1), syn()];
        r.route(&mut b);
        assert!(b.is_empty());
        // Autorepeat is the guest's: not forwarded, and not the console's.
        let mut b = vec![ev(input::EV_KEY, 30, 2), syn()];
        r.route(&mut b);
        assert!(b.is_empty());
        let mut b = vec![ev(input::EV_KEY, 30, 0), syn()];
        r.route(&mut b);
        assert_eq!(
            k.flat(),
            vec![(input::EV_KEY, 30, 1), S, (input::EV_KEY, 30, 0), S]
        );
    }

    #[test]
    fn a_key_pressed_on_the_console_is_released_there() {
        let (m, k, t) = (
            FakePort::on(),
            Arc::new(FakePort::default()),
            FakePort::on(),
        );
        let mut r = Router::new([
            Some(m as Arc<dyn Port>),
            Some(k.clone() as Arc<dyn Port>),
            Some(t as Arc<dyn Port>),
        ]);
        let mut b = vec![ev(input::EV_KEY, 29, 1), syn()];
        r.route(&mut b);
        assert_eq!(b.len(), 2, "keyboard not ready: the console types");
        k.ready.store(true, Ordering::Relaxed);
        let mut b = vec![
            ev(input::EV_KEY, 29, 0),
            syn(),
            ev(input::EV_KEY, 56, 1),
            syn(),
        ];
        r.route(&mut b);
        assert_eq!(
            b.iter()
                .map(|e| (e.ev_type, e.code, e.value))
                .collect::<Vec<_>>(),
            vec![(input::EV_KEY, 29, 0), S]
        );
        assert_eq!(k.flat(), vec![(input::EV_KEY, 56, 1), S]);
    }

    /// A key held on the console while the keyboard's driver comes up: its
    /// repeats and its release stay on the console.
    #[test]
    fn a_key_held_on_the_console_stays_there_when_the_keyboard_comes_up() {
        let (m, k, t) = (
            FakePort::on(),
            Arc::new(FakePort::default()),
            FakePort::on(),
        );
        let mut r = Router::new([
            Some(m as Arc<dyn Port>),
            Some(k.clone() as Arc<dyn Port>),
            Some(t as Arc<dyn Port>),
        ]);
        let mut b = vec![ev(input::EV_KEY, 42, 1), syn()];
        r.route(&mut b);
        assert_eq!(b.len(), 2);
        k.ready.store(true, Ordering::Relaxed);
        // The viewer's repeat is another press: still the console's.
        let mut b = vec![
            ev(input::EV_KEY, 42, 1),
            syn(),
            ev(input::EV_KEY, 42, 0),
            syn(),
        ];
        r.route(&mut b);
        assert_eq!(b.len(), 4);
        assert!(k.flat().is_empty());
        // Pressed again after its release: the keyboard's now.
        let mut b = vec![ev(input::EV_KEY, 42, 1), syn()];
        r.route(&mut b);
        assert!(b.is_empty());
        assert_eq!(k.flat(), vec![(input::EV_KEY, 42, 1), S]);
    }

    #[test]
    fn ungrab_releases_mouse_buttons_and_buttons_follow_the_grab() {
        let (mut r, m, _, t) = setup();
        let mut b = vec![
            ev(EV_GRAB_MARK, 0, 1),
            ev(input::EV_KEY, BTN_RIGHT, 1),
            syn(),
        ];
        r.route(&mut b);
        let mut b = vec![ev(EV_GRAB_MARK, 0, 0)];
        r.route(&mut b);
        assert_eq!(
            m.flat(),
            vec![
                (input::EV_KEY, BTN_RIGHT, 1),
                S,
                (input::EV_KEY, BTN_RIGHT, 0),
                S
            ]
        );
        // The viewer's own release afterwards finds nothing held: console.
        let mut b = vec![ev(input::EV_KEY, BTN_RIGHT, 0), syn()];
        r.route(&mut b);
        assert_eq!(b.len(), 2);
        // Without a grab, buttons, wheel and positions go to the tablet.
        let mut b = vec![
            ev(input::EV_ABS, input::ABS_X, 40000),
            ev(input::EV_ABS, input::ABS_Y, 100),
            ev(input::EV_KEY, BTN_LEFT, 1),
            ev(input::EV_REL, input::REL_WHEEL, -1),
            ev(input::EV_REL, input::REL_WHEEL_HI_RES, -120),
            syn(),
        ];
        r.route(&mut b);
        assert!(b.is_empty());
        assert_eq!(
            t.flat(),
            vec![
                (input::EV_ABS, input::ABS_X, INPUT_ABS_MAX),
                (input::EV_ABS, input::ABS_Y, 100),
                (input::EV_KEY, BTN_LEFT, 1),
                (input::EV_REL, input::REL_WHEEL, -1),
                S
            ]
        );
    }

    #[test]
    fn a_device_that_stops_hands_everything_back_to_the_console() {
        let (mut r, m, k, _) = setup();
        let mut b = vec![ev(input::EV_KEY, 17, 1), syn()];
        r.route(&mut b);
        k.ready.store(false, Ordering::Relaxed);
        m.ready.store(false, Ordering::Relaxed);
        let mut b = vec![
            ev(input::EV_KEY, 17, 0),
            syn(),
            ev(input::EV_REL, input::REL_X, 5),
            syn(),
        ];
        r.route(&mut b);
        // The release of a key the reset already let go of goes to the
        // console, which holds nothing of it and drops it itself.
        assert_eq!(b.len(), 4);
        assert_eq!(k.flat(), vec![(input::EV_KEY, 17, 1), S]);
    }

    #[test]
    fn no_ports_means_everything_stays_on_the_console() {
        let mut r = Router::new([None, None, None]);
        let mut b = vec![
            ev(input::EV_KEY, 30, 1),
            ev(input::EV_REL, input::REL_X, 1),
            syn(),
        ];
        r.route(&mut b);
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn socket_names_round_trip() {
        for k in Kind::ALL {
            let p = std::path::Path::new("/run/x").join(k.socket_name());
            assert_eq!(Kind::from_socket_path(&p), Some(k));
            assert_eq!(Kind::parse(k.name()), Some(k));
        }
        assert_eq!(
            Kind::from_socket_path(std::path::Path::new("/run/x/gpu-libvirt.sock")),
            None
        );
    }
}

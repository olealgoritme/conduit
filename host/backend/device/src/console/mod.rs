// SPDX-License-Identifier: Apache-2.0
//
// The boot console (`--console-vnc PATH`): the VM's emulated screen --
// firmware setup, the boot menu, a disk password prompt, early kernel output
// -- shown in the viewer whenever the guest's Conduit driver is not, with the
// keyboard and pointer routed to it (always, for a guest without Conduit
// input; see below).
//
// QEMU serves that screen over VNC on a unix socket (`-vnc unix:PATH`); this
// is a minimal RFB 3.8 client for it (`rfb`): security None, raw pixels in
// DRM XRGB8888, DesktopSize, and QEMU's Extended Key Event so keys travel as
// key numbers (`keymap`), not as keysyms a layout would reinterpret.
//
// Who owns the picture is the display link's call
// (`DisplayLink::console_poll`): the console from start and after a device
// reset, the guest from its first flip, the console again when the guest
// turns its scanout off. While the console is not shown it asks QEMU for
// nothing and publishes nothing; while it is, it asks for incremental
// updates at most every `FRAME_EVERY` and publishes each finished update as
// a shared-memory frame (`DisplayLink::flip_console`), from a sealed memfd,
// double-buffered.
//
// Input follows the picture for a guest that takes Conduit input (Linux:
// its driver acks NVGPU_CFG_TAKES_INPUT and posts event-queue buffers at
// probe; `display::guest_takes_input`). A guest that takes none (Windows,
// whose driver never acks the bit, event queue or not) keeps its input here
// -- QEMU's emulated keyboard and tablet -- even while its own frames are
// shown (`DisplayLink::input_to_console`); the pointer is then placed by the
// guest's frame, not by the hidden console screen (`InputEncoder`).
//
// It runs on a thread of its own, polling the socket and an eventfd the link
// kicks; nothing here can stall the control queue or the link thread. QEMU
// may start after the backend and restarts with the VM: a missing or dropped
// socket is retried with backoff, for as long as the backend runs.

pub mod keymap;
pub mod rfb;

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::messages::{INPUT_ABS_MAX, InputEventEntry, input};

use crate::display::{ConsoleMode, ConsoleSink, DisplayLink, FlipOutcome, FrameGeometry};

/// `DRM_FORMAT_XRGB8888`, what [`rfb::set_pixel_format`] asks for.
pub const XRGB8888: u32 = 0x3432_5258;
/// `DRM_FORMAT_MOD_LINEAR`.
const MOD_LINEAR: u64 = 0;

/// The update request pace while shown: about 60 Hz.
pub const FRAME_EVERY: Duration = Duration::from_millis(16);
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(2);
/// How long the handshake may take before the server counts as gone.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the guest probe is asked while the guest owns the picture.
const PROBE_EVERY: Duration = Duration::from_millis(500);
/// Output the server has not read beyond this ends the connection.
const OUT_MAX: usize = 1 << 20;
/// Bytes read per wakeup at most, so input is not starved by a huge update.
const READ_BUDGET: usize = 8 << 20;

/// Linux button codes (input-event-codes.h).
const BTN_MISC: u16 = 0x100;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const KEY_OK: u16 = 0x160;

/// RFB pointer button mask bits.
pub const MASK_LEFT: u8 = 1 << 0;
pub const MASK_MIDDLE: u8 = 1 << 1;
pub const MASK_RIGHT: u8 = 1 << 2;
pub const MASK_WHEEL_UP: u8 = 1 << 3;
pub const MASK_WHEEL_DOWN: u8 = 1 << 4;
pub const MASK_WHEEL_LEFT: u8 = 1 << 5;
pub const MASK_WHEEL_RIGHT: u8 = 1 << 6;

/// Says whether the guest is still driving the device; asked every
/// `PROBE_EVERY` while the guest owns the picture. `false` hands the picture
/// back to the console: a guest that rebooted without a device reset stops
/// its queues long before its driver flips again, and the firmware screen in
/// between is the one the user needs.
pub type GuestProbe = Box<dyn FnMut() -> bool + Send>;

// ---------------------------------------------------------------------------
// Input: Linux events -> RFB
// ---------------------------------------------------------------------------

/// Turns the display link's Linux input events into RFB messages: keys as
/// QEMU Extended Key Events (or plain keysyms for a server without the
/// extension), the pointer as absolute positions in the framebuffer with a
/// button mask. Remembers what is held, so the console can let go of it.
///
/// The pointer is kept as a position in the picture the viewer shows, in
/// `0..=INPUT_ABS_MAX` on each axis, and placed at the same fraction of the
/// VNC framebuffer: QEMU scales a VNC position by its framebuffer's size onto
/// the emulated tablet's range, and the guest maps that range onto its
/// screen. So the pointer lands where the viewer shows it whether the
/// picture is the console's or, for a guest that takes no Conduit input
/// ([`crate::display::InputSink::takes_input`]), the guest's own frames at
/// any size. Relative motion is in pixels of the picture shown.
#[derive(Default, Debug)]
pub struct InputEncoder {
    held: BTreeSet<u16>,
    buttons: u8,
    /// The position, in `0..=INPUT_ABS_MAX` of the picture shown.
    ax: i32,
    ay: i32,
    /// The framebuffer the last pointer message was encoded for.
    fb: (u16, u16),
    moved: bool,
}

/// The sizes one event is encoded against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    /// The VNC server's framebuffer: what pointer positions are in.
    pub fb: (u16, u16),
    /// The picture the viewer shows: what relative motion is in. The
    /// framebuffer while the console is shown; the guest's frame size while
    /// the guest's frames are.
    pub view: (u32, u32),
}

impl Sizes {
    /// The console shown: the picture is the framebuffer.
    pub const fn console(fb: (u16, u16)) -> Self {
        Self {
            fb,
            view: (fb.0 as u32, fb.1 as u32),
        }
    }
}

impl InputEncoder {
    /// `v` in `0..=INPUT_ABS_MAX` -> a pixel in `0..dim`.
    pub fn scale(v: i32, dim: u16) -> u16 {
        if dim <= 1 {
            return 0;
        }
        let max = INPUT_ABS_MAX as i64;
        let v = (v as i64).clamp(0, max);
        ((v * (dim as i64 - 1) + max / 2) / max) as u16
    }

    /// `d` pixels of a `dim`-pixel axis added to `a` in `0..=INPUT_ABS_MAX`.
    fn nudge(a: i32, d: i32, dim: u32) -> i32 {
        let max = INPUT_ABS_MAX as i64;
        let span = (dim.max(2) - 1) as i64;
        let step = (d as i64 * max + d.signum() as i64 * span / 2) / span;
        (a as i64 + step).clamp(0, max) as i32
    }

    fn key(code: u16, down: bool, ext_key: bool, out: &mut Vec<u8>) {
        let sym = keymap::keysym(code);
        if ext_key {
            if let Some(q) = keymap::qnum(code) {
                out.extend_from_slice(&rfb::qemu_key_event(down, sym, q as u32));
            }
        } else if sym != 0 {
            out.extend_from_slice(&rfb::key_event(down, sym));
        }
    }

    /// The pointer position in the framebuffer.
    pub fn position(&self) -> (u16, u16) {
        (
            Self::scale(self.ax, self.fb.0),
            Self::scale(self.ay, self.fb.1),
        )
    }

    fn pointer(&self, mask: u8, out: &mut Vec<u8>) {
        let (x, y) = self.position();
        out.extend_from_slice(&rfb::pointer_event(mask, x, y));
    }

    /// One event; `ext_key` is whether the server takes Extended Key Events.
    pub fn event(&mut self, e: &InputEventEntry, sizes: Sizes, ext_key: bool, out: &mut Vec<u8>) {
        self.fb = sizes.fb;
        let (vw, vh) = sizes.view;
        match e.ev_type {
            input::EV_KEY => {
                let down = e.value != 0;
                let bit = match e.code {
                    BTN_LEFT => MASK_LEFT,
                    BTN_MIDDLE => MASK_MIDDLE,
                    BTN_RIGHT => MASK_RIGHT,
                    // Other buttons (side, extra, joystick, tablet): nothing
                    // a firmware screen takes.
                    BTN_MISC..KEY_OK => return,
                    _ => 0,
                };
                if bit != 0 {
                    if down {
                        self.buttons |= bit;
                    } else {
                        self.buttons &= !bit;
                    }
                    self.pointer(self.buttons, out);
                    self.moved = false;
                    return;
                }
                if down {
                    self.held.insert(e.code);
                } else if !self.held.remove(&e.code) {
                    // Pressed while the guest had the input: the guest got
                    // the release already.
                    return;
                }
                Self::key(e.code, down, ext_key, out);
            }
            input::EV_ABS => {
                match e.code {
                    input::ABS_X => self.ax = e.value.clamp(0, INPUT_ABS_MAX),
                    input::ABS_Y => self.ay = e.value.clamp(0, INPUT_ABS_MAX),
                    _ => return,
                }
                self.moved = true;
            }
            input::EV_REL => match e.code {
                input::REL_X => {
                    self.ax = Self::nudge(self.ax, e.value, vw);
                    self.moved = true;
                }
                input::REL_Y => {
                    self.ay = Self::nudge(self.ay, e.value, vh);
                    self.moved = true;
                }
                // A wheel detent is a press and release of a wheel button.
                input::REL_WHEEL | input::REL_HWHEEL if e.value != 0 => {
                    let bit = match (e.code, e.value > 0) {
                        (input::REL_WHEEL, true) => MASK_WHEEL_UP,
                        (input::REL_WHEEL, false) => MASK_WHEEL_DOWN,
                        (_, true) => MASK_WHEEL_RIGHT,
                        (_, false) => MASK_WHEEL_LEFT,
                    };
                    for _ in 0..e.value.unsigned_abs().min(8) {
                        self.pointer(self.buttons | bit, out);
                        self.pointer(self.buttons, out);
                    }
                    self.moved = false;
                }
                _ => {}
            },
            input::EV_SYN if self.moved => {
                self.pointer(self.buttons, out);
                self.moved = false;
            }
            _ => {}
        }
    }

    /// Let go of every key and button held.
    pub fn release_all(&mut self, ext_key: bool, out: &mut Vec<u8>) {
        for code in std::mem::take(&mut self.held) {
            Self::key(code, false, ext_key, out);
        }
        if self.buttons != 0 {
            self.buttons = 0;
            self.pointer(0, out);
        }
    }

    pub fn held(&self) -> usize {
        self.held.len()
    }
}

// ---------------------------------------------------------------------------
// Frames: a shadow framebuffer and sealed memfds
// ---------------------------------------------------------------------------

/// One shared-memory frame: a memfd sealed against shrinking and growing
/// (the viewer requires `F_SEAL_SHRINK`), mapped for writing here.
pub struct ShmFrame {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
    pub width: u32,
    pub height: u32,
}

// SAFETY: the mapping is owned by this value and only written through it.
unsafe impl Send for ShmFrame {}

impl ShmFrame {
    pub fn new(width: u32, height: u32) -> io::Result<Self> {
        let len = width as usize * height as usize * rfb::BPP;
        if len == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // SAFETY: plain memfd_create with a NUL-terminated name.
        let fd = unsafe {
            libc::memfd_create(
                c"conduit-console".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: integer arguments on a descriptor we hold.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: integer arguments on a descriptor we hold.
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a shared read-write mapping of exactly the file's size.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            ptr: ptr.cast(),
            len,
            width,
            height,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn stride(&self) -> u32 {
        self.width * rfb::BPP as u32
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: a live mapping of `len` bytes owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn geometry(&self) -> FrameGeometry {
        FrameGeometry {
            width: self.width,
            height: self.height,
            stride: self.stride(),
            offset: 0,
            fourcc: XRGB8888,
            modifier: MOD_LINEAR,
        }
    }
}

impl Drop for ShmFrame {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping this value made.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// The server's framebuffer as we know it, and the two memfds it is
/// published through in turn.
#[derive(Default)]
struct Frames {
    width: u16,
    height: u16,
    shadow: Vec<u8>,
    bufs: [Option<ShmFrame>; 2],
    next: usize,
    published: u64,
}

impl Frames {
    fn resize(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
        self.shadow = vec![0u8; w as usize * h as usize * rfb::BPP];
    }

    fn blit(&mut self, r: &rfb::Rect, data: &[u8]) -> io::Result<()> {
        if r.x as u32 + r.w as u32 > self.width as u32
            || r.y as u32 + r.h as u32 > self.height as u32
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "rectangle {}x{}+{}+{} outside the {}x{} framebuffer",
                    r.w, r.h, r.x, r.y, self.width, self.height
                ),
            ));
        }
        let row = r.w as usize * rfb::BPP;
        let stride = self.width as usize * rfb::BPP;
        for j in 0..r.h as usize {
            let at = (r.y as usize + j) * stride + r.x as usize * rfb::BPP;
            self.shadow[at..at + row].copy_from_slice(&data[j * row..(j + 1) * row]);
        }
        Ok(())
    }

    /// Copy the shadow into the next memfd and hand it to the link.
    fn publish(&mut self, link: &DisplayLink) -> FlipOutcome {
        if self.width == 0 || self.height == 0 {
            return FlipOutcome::NoBroker;
        }
        let (w, h) = (self.width as u32, self.height as u32);
        let slot = &mut self.bufs[self.next];
        if slot.as_ref().is_none_or(|b| (b.width, b.height) != (w, h)) {
            *slot = None;
            match ShmFrame::new(w, h) {
                Ok(b) => *slot = Some(b),
                Err(e) => {
                    log::warn!("console: a {w}x{h} shared-memory frame: {e}; frame dropped");
                    return FlipOutcome::Busy;
                }
            }
        }
        let buf = slot.as_mut().expect("made above");
        buf.bytes_mut().copy_from_slice(&self.shadow);
        let out = link.flip_console(buf.fd(), &buf.geometry());
        if self.published == 0 {
            log::info!("console: first frame, {w}x{h}: {out:?}");
        }
        self.published += 1;
        self.next ^= 1;
        out
    }
}

// ---------------------------------------------------------------------------
// The link's side: input and wakeups
// ---------------------------------------------------------------------------

/// What the display link holds of the console.
struct Shared {
    /// eventfd the console thread polls.
    wake: OwnedFd,
    input: Mutex<Vec<InputEventEntry>>,
}

impl Shared {
    fn new() -> io::Result<Self> {
        // SAFETY: plain eventfd.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: a fresh descriptor nothing else owns.
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            input: Mutex::new(Vec::new()),
        })
    }

    fn kick(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: an 8-byte write to an eventfd we hold; a full counter
        // (EAGAIN) means a wakeup is pending anyway.
        unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), 8) };
    }

    fn drain(&self) {
        let mut b = [0u8; 8];
        // SAFETY: an 8-byte read from a non-blocking eventfd we hold.
        unsafe { libc::read(self.wake.as_raw_fd(), b.as_mut_ptr().cast(), 8) };
    }
}

impl ConsoleSink for Shared {
    fn input(&self, events: &[InputEventEntry]) {
        let mut q = self.input.lock().unwrap();
        // A console that is not reading (no server) must not grow this
        // without bound: motion is what goes, as on the guest side.
        if q.len() > 4096 {
            q.retain(|e| e.ev_type == input::EV_KEY || e.ev_type == input::EV_SYN);
        }
        q.extend_from_slice(events);
        drop(q);
        self.kick();
    }

    fn wake(&self) {
        self.kick();
    }
}

/// A running console thread.
pub struct Console {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Console {
    /// Attach a console reading QEMU's VNC socket at `path` to `link`, and
    /// start its thread. The console is shown from now until the guest's
    /// first flip.
    pub fn start(
        link: Arc<DisplayLink>,
        path: PathBuf,
        probe: Option<GuestProbe>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::new()?);
        let stop = Arc::new(AtomicBool::new(false));
        link.attach_console(shared.clone());
        let mut w = Worker {
            link,
            path,
            probe,
            shared: shared.clone(),
            stop: stop.clone(),
            frames: Frames::default(),
            enc: InputEncoder::default(),
            last_probe: Instant::now(),
            out: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("nvgpu-console".into())
            .spawn(move || w.run())?;
        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the thread and wait for it.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.shared.kick();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.halt();
    }
}

// ---------------------------------------------------------------------------
// The console thread
// ---------------------------------------------------------------------------

struct Worker {
    link: Arc<DisplayLink>,
    path: PathBuf,
    probe: Option<GuestProbe>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    frames: Frames,
    enc: InputEncoder,
    last_probe: Instant,
    out: Vec<u8>,
}

/// One connection's state.
struct Session {
    sock: UnixStream,
    inbuf: Vec<u8>,
    ext_key: bool,
    /// An update request is out and unanswered.
    outstanding: bool,
    next_req: Instant,
    /// The next request is a full one (connect, resize, shown again).
    need_full: bool,
    shown: bool,
}

fn connect(path: &Path) -> io::Result<(UnixStream, rfb::ServerInit)> {
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    s.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let init = rfb::handshake(&mut s)?;
    s.set_nonblocking(true)?;
    Ok((s, init))
}

impl Worker {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn run(&mut self) {
        let mut retry = RETRY_MIN;
        let mut warned = false;
        while !self.stopped() {
            match connect(&self.path) {
                Ok((sock, init)) => {
                    log::info!(
                        "console: connected to {} ({:?}, {}x{})",
                        self.path.display(),
                        init.name,
                        init.width,
                        init.height
                    );
                    retry = RETRY_MIN;
                    warned = false;
                    match self.session(sock, &init) {
                        Ok(()) => return,
                        Err(e) => log::info!("console: connection to the VNC server lost: {e}"),
                    }
                    // Whatever was held went with the connection.
                    self.enc = InputEncoder::default();
                }
                Err(e) => {
                    if !warned {
                        log::info!(
                            "console: no VNC server at {} ({e}); waiting for one",
                            self.path.display()
                        );
                        warned = true;
                    }
                }
            }
            // Wait out the backoff; input meanwhile has nowhere to go.
            let until = Instant::now() + retry;
            while !self.stopped() {
                let now = Instant::now();
                if now >= until {
                    break;
                }
                self.tick();
                self.wait(&[], (until - now).min(PROBE_EVERY));
                self.shared.input.lock().unwrap().clear();
            }
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /// Housekeeping every loop: the guest probe, and a due takeover.
    fn tick(&mut self) -> (bool, Option<Instant>) {
        if let Some(p) = self.probe.as_mut()
            && self.last_probe.elapsed() >= PROBE_EVERY
        {
            self.last_probe = Instant::now();
            if self.link.console_mode() == Some(ConsoleMode::Guest) && !p() {
                self.link
                    .console_reset("the guest stopped the device's queues (reboot?)");
            }
        }
        self.link.console_poll()
    }

    /// Poll the wakeup eventfd and `extra`; returns the extra descriptors'
    /// revents.
    fn wait(&self, extra: &[(RawFd, i16)], timeout: Duration) -> Vec<i16> {
        let mut pfds = vec![libc::pollfd {
            fd: self.shared.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        pfds.extend(extra.iter().map(|&(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        }));
        let ms = timeout.as_millis().clamp(0, i32::MAX as u128) as i32;
        // SAFETY: a live pollfd array of the stated length.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
        if n > 0 && pfds[0].revents & libc::POLLIN != 0 {
            self.shared.drain();
        }
        pfds[1..]
            .iter()
            .map(|p| if n > 0 { p.revents } else { 0 })
            .collect()
    }

    fn session(&mut self, sock: UnixStream, init: &rfb::ServerInit) -> io::Result<()> {
        check_size(init.width, init.height)?;
        self.frames.resize(init.width, init.height);
        self.out.clear();
        let mut s = Session {
            sock,
            inbuf: Vec::with_capacity(1 << 16),
            ext_key: false,
            outstanding: false,
            next_req: Instant::now(),
            need_full: true,
            shown: false,
        };
        loop {
            if self.stopped() {
                return Ok(());
            }
            let (shown, takeover) = self.tick();
            if shown && !s.shown {
                // What we hold of the screen is stale: ask for all of it.
                s.need_full = true;
            }
            s.shown = shown;

            // Input, encoded against the current framebuffer and the picture
            // the viewer shows: the console's, or the guest's frames for a
            // guest that takes no Conduit input (`DisplayLink::input_to_console`).
            let events = std::mem::take(&mut *self.shared.input.lock().unwrap());
            if !events.is_empty() {
                let fb = (self.frames.width, self.frames.height);
                let mut sizes = Sizes::console(fb);
                if !shown && let Some(view) = self.link.guest_picture_size() {
                    sizes.view = view;
                }
                for e in &events {
                    self.enc.event(e, sizes, s.ext_key, &mut self.out);
                }
            }

            // Ask for the next frame, paced, and only while shown.
            let now = Instant::now();
            if shown && (s.need_full || (!s.outstanding && now >= s.next_req)) {
                self.out.extend_from_slice(&rfb::update_request(
                    !s.need_full,
                    self.frames.width,
                    self.frames.height,
                ));
                s.outstanding = true;
                s.need_full = false;
                s.next_req = now + FRAME_EVERY;
            }
            self.flush(&mut s.sock)?;

            let mut timeout = PROBE_EVERY;
            if shown && !s.outstanding {
                timeout = timeout.min(s.next_req.saturating_duration_since(now));
            }
            if let Some(at) = takeover {
                timeout = timeout.min(at.saturating_duration_since(now) + Duration::from_millis(1));
            }
            let mut events = libc::POLLIN;
            if !self.out.is_empty() {
                events |= libc::POLLOUT;
            }
            let rev = self.wait(&[(s.sock.as_raw_fd(), events)], timeout);
            if rev[0] & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                self.read(&mut s)?;
            }
        }
    }

    fn flush(&mut self, sock: &mut UnixStream) -> io::Result<()> {
        while !self.out.is_empty() {
            match sock.write(&self.out) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if self.out.len() > OUT_MAX {
            return Err(io::Error::other("the VNC server stopped reading"));
        }
        Ok(())
    }

    fn read(&mut self, s: &mut Session) -> io::Result<()> {
        let mut got = 0usize;
        let mut chunk = vec![0u8; 256 << 10];
        while got < READ_BUDGET {
            match s.sock.read(&mut chunk) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(n) => {
                    s.inbuf.extend_from_slice(&chunk[..n]);
                    got += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut at = 0;
        while let Some((msg, n)) = rfb::parse(&s.inbuf[at..])? {
            if let rfb::ServerMsg::Update(rects) = msg {
                self.update(s, at, &rects)?;
            }
            at += n;
        }
        s.inbuf.drain(..at);
        Ok(())
    }

    /// A whole FramebufferUpdate whose rectangles' data sit at `base` in the
    /// session's input buffer.
    fn update(&mut self, s: &mut Session, base: usize, rects: &[rfb::Rect]) -> io::Result<()> {
        s.outstanding = false;
        let mut changed = false;
        for r in rects {
            match r.enc {
                rfb::ENC_RAW => {
                    let d = &s.inbuf[base + r.data.start..base + r.data.end];
                    self.frames.blit(r, d)?;
                    changed = true;
                }
                rfb::ENC_DESKTOP_SIZE => {
                    check_size(r.w, r.h)?;
                    log::info!("console: the VM's screen is now {}x{}", r.w, r.h);
                    self.frames.resize(r.w, r.h);
                    // Shown with the pixels the full update brings, not as
                    // a black frame now.
                    s.need_full = true;
                }
                rfb::ENC_QEMU_EXT_KEY => {
                    if !s.ext_key {
                        log::debug!("console: the server takes QEMU extended key events");
                    }
                    s.ext_key = true;
                }
                _ => {} // LED state: nothing to show
            }
        }
        if changed && s.shown {
            self.frames.publish(&self.link);
        }
        Ok(())
    }
}

fn check_size(w: u16, h: u16) -> io::Result<()> {
    if w > rfb::MAX_DIM || h > rfb::MAX_DIM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a {w}x{h} framebuffer is over the viewer's {0}x{0}",
                rfb::MAX_DIM
            ),
        ));
    }
    Ok(())
}

/// Connect to the VNC server at `path`, ask for one full frame and return it
/// as (width, height, XRGB8888 pixels). For tools and manual checks.
pub fn grab(path: &Path, timeout: Duration) -> io::Result<(u16, u16, Vec<u8>)> {
    let (mut sock, init) = connect(path)?;
    check_size(init.width, init.height)?;
    let mut f = Frames::default();
    f.resize(init.width, init.height);
    sock.set_nonblocking(false)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.write_all(&rfb::update_request(false, f.width, f.height))?;
    let mut inbuf = Vec::new();
    let mut chunk = vec![0u8; 256 << 10];
    let until = Instant::now() + timeout;
    loop {
        if Instant::now() >= until {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        inbuf.extend_from_slice(&chunk[..n]);
        let mut at = 0;
        while let Some((msg, n)) = rfb::parse(&inbuf[at..])? {
            if let rfb::ServerMsg::Update(rects) = msg {
                let mut pixels = false;
                for r in &rects {
                    match r.enc {
                        rfb::ENC_RAW => {
                            f.blit(r, &inbuf[at + r.data.start..at + r.data.end])?;
                            pixels = true;
                        }
                        rfb::ENC_DESKTOP_SIZE => {
                            check_size(r.w, r.h)?;
                            f.resize(r.w, r.h);
                        }
                        _ => {}
                    }
                }
                if pixels {
                    return Ok((f.width, f.height, f.shadow));
                }
                sock.write_all(&rfb::update_request(false, f.width, f.height))?;
            }
            at += n;
        }
        inbuf.drain(..at);
    }
}

#[cfg(test)]
mod tests;

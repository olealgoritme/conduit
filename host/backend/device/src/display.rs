//! Zero-copy scanout: the backend half (docs/SCANOUT.md).
//!
//! The guest flips a framebuffer that already lives in host VRAM as a host GEM
//! object. The backend exports that object once as a dma-buf (on the owning
//! drm_file's host descriptor, cached per `(owner_handle, host_handle)`) and
//! hands it to the viewer -- the nvkvm display broker -- over a unix socket in
//! the broker's wire protocol (`host/broker/common/nvkvm_broker_proto.h`).
//! Input comes back over the same socket and is turned into Linux input
//! events for the guest's event queue.
//!
//! Latency rules, in order of importance:
//!
//! * A guest RPC never waits on the broker. A flip is one `sendmsg` with
//!   `MSG_DONTWAIT` of two 40-byte records (ATTACH carrying the fd, COMMIT);
//!   a socket that will not take it costs that frame and nothing else --
//!   latest frame wins, the next flip supersedes it. No broker at all: the
//!   flip is acked and dropped.
//! * No CPU copy, no readback. If the broker refuses a buffer it says so on
//!   its own stderr; the backend asks once per format (`QUERY_FORMAT`) and
//!   logs the verdict so a black window has a reason in this log too.
//! * Input is forwarded as soon as it is read: one reader thread blocks in
//!   `poll` on the socket and pushes everything one `recv` returned as one
//!   event-queue message.
//!
//! Two more things travel the same link (docs/SCANOUT.md):
//!
//! * Mode hints. The broker says which mode the guest should be in
//!   (`EV_MODE_HINT`: the output's mode when fullscreen, "your configured
//!   mode" when windowed, or the window's size with `--resize=guest`);
//!   [`ModePolicy`] turns that into a `DisplayMode` event for the guest, sent
//!   only when it changes.
//! * The guest's cursor. A `CursorUpdate` from the guest becomes `CMD_CURSOR`
//!   with the cursor plane's dma-buf; the broker makes it the host pointer's
//!   image. The last one is kept and re-sent to a broker that (re)connects.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::messages::{
    CursorUpdate, DisplayModeEvent, INPUT_ABS_MAX, InputEventEntry, ScanoutFlip, input,
};

/// CLOCK_MONOTONIC in microseconds: the broker's clock too, so a frame stamped
/// with it (`CLIENT_SEQ_USEC`) lets the broker measure flip -> screen.
fn mono_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

// ---------------------------------------------------------------------------
// Preferred mode, from --display WxH@HZ
// ---------------------------------------------------------------------------

/// The mode announced in device config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl DisplayMode {
    pub const DEFAULT: Self = Self {
        width: 2560,
        height: 1440,
        refresh_hz: 240,
    };

    /// `WxH@HZ`, or `WxH` for the default refresh.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (wh, hz) = match s.split_once('@') {
            Some((wh, hz)) => (wh, Some(hz)),
            None => (s, None),
        };
        let (w, h) = wh
            .split_once(['x', 'X'])
            .ok_or_else(|| format!("{s:?}: expected WxH@HZ, e.g. 2560x1440@240"))?;
        let num = |v: &str, what: &str, max: u32| -> Result<u32, String> {
            let n: u32 = v
                .trim()
                .parse()
                .map_err(|_| format!("{s:?}: {what} {v:?} is not a number"))?;
            if n == 0 || n > max {
                return Err(format!("{s:?}: {what} must be 1..={max}"));
            }
            Ok(n)
        };
        // 8192 is the broker's own NVKVM_BROKER_MAX_DIM.
        Ok(Self {
            width: num(w, "width", 8192)?,
            height: num(h, "height", 8192)?,
            refresh_hz: match hz {
                Some(hz) => num(hz, "refresh", 1000)?,
                None => Self::DEFAULT.refresh_hz,
            },
        })
    }
}

impl std::fmt::Display for DisplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.refresh_hz)
    }
}

// ---------------------------------------------------------------------------
// dma-buf cache
// ---------------------------------------------------------------------------

/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`: `_IOWR('d', 0x2d, struct drm_prime_handle)`,
/// a 12-byte `{u32 handle; u32 flags; s32 fd}`.
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u64 = 0xC00C_642D;
/// `DRM_IOCTL_GEM_CLOSE`: `_IOW('d', 0x09, struct drm_gem_close)`.
pub const DRM_IOCTL_GEM_CLOSE: u64 = 0x4008_6409;

/// One dma-buf per `(owner_handle, host_handle)`, exported once.
///
/// Holding the fd keeps the host object alive, so an entry must go when the
/// guest closes the GEM handle (the host may reissue the number) or the file.
/// Bounded: a guest that flips many distinct buffers evicts the oldest, which
/// costs a re-export, never an fd leak.
pub struct DmabufCache {
    map: HashMap<(u32, u32), (u64, OwnedFd)>,
    tick: u64,
    cap: usize,
    exports: u64,
}

impl Default for DmabufCache {
    fn default() -> Self {
        Self::with_capacity(64)
    }
}

impl DmabufCache {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            cap: cap.max(1),
            exports: 0,
        }
    }

    /// The cached dma-buf for this object, exporting it with `export` on a
    /// miss. `Err` is the errno `export` failed with.
    pub fn get_or_export(
        &mut self,
        owner: u32,
        host_handle: u32,
        export: impl FnOnce() -> Result<OwnedFd, i32>,
    ) -> Result<RawFd, i32> {
        self.tick += 1;
        let tick = self.tick;
        if let Some(e) = self.map.get_mut(&(owner, host_handle)) {
            e.0 = tick;
            return Ok(e.1.as_raw_fd());
        }
        let fd = export()?;
        self.exports += 1;
        if self.map.len() >= self.cap
            && let Some(oldest) = self.map.iter().min_by_key(|(_, v)| v.0).map(|(k, _)| *k)
        {
            self.map.remove(&oldest);
        }
        let raw = fd.as_raw_fd();
        self.map.insert((owner, host_handle), (tick, fd));
        Ok(raw)
    }

    /// The guest closed this GEM handle.
    pub fn forget(&mut self, owner: u32, host_handle: u32) -> bool {
        self.map.remove(&(owner, host_handle)).is_some()
    }

    /// The guest closed the whole drm_file.
    pub fn forget_owner(&mut self, owner: u32) -> usize {
        let before = self.map.len();
        self.map.retain(|(o, _), _| *o != owner);
        before - self.map.len()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Exports performed since creation (misses).
    pub fn exports(&self) -> u64 {
        self.exports
    }
}

/// Export `host_handle` on the drm fd `drm_fd` as a dma-buf, through `ioctl`
/// (the backend's `HostDriver`, so tests need no GPU).
pub fn prime_export(
    drm_fd: RawFd,
    host_handle: u32,
    ioctl: impl FnOnce(RawFd, u64, &mut [u8]) -> Result<(), i32>,
) -> Result<OwnedFd, i32> {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&host_handle.to_le_bytes());
    p[4..8].copy_from_slice(&((libc::O_CLOEXEC | libc::O_RDWR) as u32).to_le_bytes());
    p[8..12].copy_from_slice(&(-1i32).to_le_bytes());
    ioctl(drm_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut p)?;
    let fd = i32::from_le_bytes(p[8..12].try_into().unwrap());
    if fd < 0 {
        return Err(libc::EIO);
    }
    // SAFETY: the kernel installed this descriptor for us and nothing else
    // owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// ---------------------------------------------------------------------------
// Broker wire protocol (nvkvm_broker_proto.h, version 2)
// ---------------------------------------------------------------------------

pub mod wire {
    pub const PROTO_VERSION: u32 = 2;
    pub const CMD_SIZE: usize = 40;
    pub const PKT_SIZE: usize = 24;

    pub const CMD_ATTACH: u16 = 1;
    pub const CMD_COMMIT: u16 = 2;
    pub const CMD_WINDOW: u16 = 3;
    pub const CMD_CAPS: u16 = 5;
    pub const CMD_QUERY_FORMAT: u16 = 6;
    /// virtio-nvgpu: the guest cursor image; only to a broker with
    /// [`CAP_CURSOR`] (an unknown command ends the connection).
    pub const CMD_CURSOR: u16 = 7;

    pub const EV_HELLO: u16 = 1;
    pub const EV_SURFACE: u16 = 2;
    pub const EV_FRAME: u16 = 3;
    pub const EV_RELEASE: u16 = 4;
    pub const EV_KEY: u16 = 5;
    pub const EV_BTN: u16 = 6;
    pub const EV_ABS: u16 = 7;
    pub const EV_REL: u16 = 8;
    pub const EV_WHEEL: u16 = 9;
    pub const EV_GRAB: u16 = 10;
    pub const EV_FOCUS: u16 = 11;
    pub const EV_POINTER: u16 = 12;
    pub const EV_BYE: u16 = 13;
    pub const EV_CLOSE: u16 = 14;
    pub const EV_FORMAT: u16 = 16;
    /// virtio-nvgpu: x,y = mode in buffer pixels (0,0 = the configured
    /// mode), w0 = refresh mHz (0 = configured), w1 = reason.
    pub const EV_MODE_HINT: u16 = 17;

    pub const F_FULLSCREEN: u16 = 1 << 2;

    /// HELLO capability bits this backend cares about.
    pub const CAP_MODE_HINTS: u32 = 1 << 10;
    pub const CAP_CURSOR: u32 = 1 << 11;
    /// CMD_CAPS bit: ATTACH/COMMIT `seq` is our CLOCK_MONOTONIC microseconds.
    pub const CLIENT_SEQ_USEC: u32 = 1 << 1;
    pub const CURSOR_MAX_DIM: u32 = 256;

    /// CMD_CURSOR `seq`: the hotspot, x low 16 bits, y high.
    pub const fn cursor_hot(x: u32, y: u32) -> u32 {
        (x & 0xffff) | ((y & 0xffff) << 16)
    }

    /// `struct nvkvm_broker_cmd`.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Cmd {
        pub ty: u16,
        pub flags: u16,
        pub width: u32,
        pub height: u32,
        pub stride: u32,
        pub offset: u32,
        pub fourcc: u32,
        pub modifier: u64,
        pub seq: u32,
    }

    impl Cmd {
        pub fn encode(&self) -> [u8; CMD_SIZE] {
            let mut o = [0u8; CMD_SIZE];
            o[0..2].copy_from_slice(&self.ty.to_le_bytes());
            o[2..4].copy_from_slice(&self.flags.to_le_bytes());
            o[4..8].copy_from_slice(&self.width.to_le_bytes());
            o[8..12].copy_from_slice(&self.height.to_le_bytes());
            o[12..16].copy_from_slice(&self.stride.to_le_bytes());
            o[16..20].copy_from_slice(&self.offset.to_le_bytes());
            o[20..24].copy_from_slice(&self.fourcc.to_le_bytes());
            o[24..32].copy_from_slice(&self.modifier.to_le_bytes());
            o[32..36].copy_from_slice(&self.seq.to_le_bytes());
            // 36..40 reserved1, zero.
            o
        }

        pub fn decode(b: &[u8; CMD_SIZE]) -> Self {
            let w = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
            Self {
                ty: u16::from_le_bytes([b[0], b[1]]),
                flags: u16::from_le_bytes([b[2], b[3]]),
                width: w(4),
                height: w(8),
                stride: w(12),
                offset: w(16),
                fourcc: w(20),
                modifier: (w(24) as u64) | ((w(28) as u64) << 32),
                seq: w(32),
            }
        }
    }

    /// `struct nvkvm_broker_pkt`.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Pkt {
        pub ty: u16,
        pub flags: u16,
        pub seq: u32,
        pub x: i32,
        pub y: i32,
        pub w0: u32,
        pub w1: u32,
    }

    impl Pkt {
        pub fn decode(b: &[u8; PKT_SIZE]) -> Self {
            let w = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
            Self {
                ty: u16::from_le_bytes([b[0], b[1]]),
                flags: u16::from_le_bytes([b[2], b[3]]),
                seq: w(4),
                x: w(8) as i32,
                y: w(12) as i32,
                w0: w(16),
                w1: w(20),
            }
        }

        pub fn encode(&self) -> [u8; PKT_SIZE] {
            let mut o = [0u8; PKT_SIZE];
            o[0..2].copy_from_slice(&self.ty.to_le_bytes());
            o[2..4].copy_from_slice(&self.flags.to_le_bytes());
            o[4..8].copy_from_slice(&self.seq.to_le_bytes());
            o[8..12].copy_from_slice(&self.x.to_le_bytes());
            o[12..16].copy_from_slice(&self.y.to_le_bytes());
            o[16..20].copy_from_slice(&self.w0.to_le_bytes());
            o[20..24].copy_from_slice(&self.w1.to_le_bytes());
            o
        }
    }

    /// Fixed-size record reassembly. Bytes in, whole packets out; a partial
    /// tail is kept for the next read. No resync: the framing has none.
    #[derive(Default)]
    pub struct PktReader {
        buf: [u8; PKT_SIZE],
        len: usize,
    }

    impl PktReader {
        pub fn feed(&mut self, mut bytes: &[u8], mut each: impl FnMut(Pkt)) {
            while !bytes.is_empty() {
                let take = (PKT_SIZE - self.len).min(bytes.len());
                self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
                self.len += take;
                bytes = &bytes[take..];
                if self.len == PKT_SIZE {
                    each(Pkt::decode(&self.buf));
                    self.len = 0;
                }
            }
        }

        pub fn reset(&mut self) {
            self.len = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Input translation: broker packets → Linux input events
// ---------------------------------------------------------------------------

/// Turns broker input packets into `input_event` triples, and remembers what
/// is held down so a focus loss, grab change or disconnect can release it --
/// a press whose release never arrives is a stuck modifier in the guest.
#[derive(Default)]
pub struct InputTranslator {
    held: BTreeSet<u16>,
    /// The broker's window, from EV_SURFACE; logged, not acted on: a windowed
    /// resize is scaled by the host and must not re-mode the guest.
    pub surface: (i32, i32),
}

impl InputTranslator {
    fn syn(out: &mut Vec<InputEventEntry>) {
        out.push(InputEventEntry::new(input::EV_SYN, input::SYN_REPORT, 0));
    }

    /// Absolute position `v` in `0..range` → `0..=INPUT_ABS_MAX`.
    pub fn scale_abs(v: i32, range: u32) -> i32 {
        let max = INPUT_ABS_MAX as i64;
        if range <= 1 {
            return (v as i64).clamp(0, max) as i32;
        }
        let v = (v as i64).clamp(0, range as i64 - 1);
        ((v * max + (range as i64 - 1) / 2) / (range as i64 - 1)).clamp(0, max) as i32
    }

    /// Append the events for one packet to `out`.
    pub fn packet(&mut self, p: &wire::Pkt, out: &mut Vec<InputEventEntry>) {
        use wire::*;
        match p.ty {
            EV_KEY | EV_BTN => {
                let code = match u16::try_from(p.x) {
                    Ok(c) if c <= input::KEY_MAX => c,
                    _ => return,
                };
                let down = p.y != 0;
                if down {
                    self.held.insert(code);
                } else if !self.held.remove(&code) {
                    // A release for something the guest never saw pressed
                    // (pressed before focus) is harmless; pass it on anyway.
                }
                out.push(InputEventEntry::new(input::EV_KEY, code, down as i32));
                Self::syn(out);
            }
            EV_ABS => {
                out.push(InputEventEntry::new(
                    input::EV_ABS,
                    input::ABS_X,
                    Self::scale_abs(p.x, p.w0),
                ));
                out.push(InputEventEntry::new(
                    input::EV_ABS,
                    input::ABS_Y,
                    Self::scale_abs(p.y, p.w1),
                ));
                Self::syn(out);
            }
            EV_REL => {
                if p.x == 0 && p.y == 0 {
                    return;
                }
                if p.x != 0 {
                    out.push(InputEventEntry::new(input::EV_REL, input::REL_X, p.x));
                }
                if p.y != 0 {
                    out.push(InputEventEntry::new(input::EV_REL, input::REL_Y, p.y));
                }
                Self::syn(out);
            }
            EV_WHEEL => {
                if p.x == 0 && p.y == 0 {
                    return;
                }
                // Both the detent and the hi-res code (120 per detent): the
                // guest's input core drops whichever its device did not
                // advertise, and libinput prefers hi-res when it is there.
                if p.x != 0 {
                    out.push(InputEventEntry::new(input::EV_REL, input::REL_WHEEL, p.x));
                    out.push(InputEventEntry::new(
                        input::EV_REL,
                        input::REL_WHEEL_HI_RES,
                        p.x.saturating_mul(120),
                    ));
                }
                if p.y != 0 {
                    out.push(InputEventEntry::new(input::EV_REL, input::REL_HWHEEL, p.y));
                    out.push(InputEventEntry::new(
                        input::EV_REL,
                        input::REL_HWHEEL_HI_RES,
                        p.y.saturating_mul(120),
                    ));
                }
                Self::syn(out);
            }
            EV_FOCUS | EV_GRAB if p.x == 0 => self.release_all(out),
            EV_FOCUS | EV_GRAB => {}
            EV_BYE => self.release_all(out),
            EV_SURFACE => self.surface = (p.x, p.y),
            _ => {}
        }
    }

    /// Release everything held, for focus loss and disconnects.
    pub fn release_all(&mut self, out: &mut Vec<InputEventEntry>) {
        if self.held.is_empty() {
            return;
        }
        for code in std::mem::take(&mut self.held) {
            out.push(InputEventEntry::new(input::EV_KEY, code, 0));
        }
        Self::syn(out);
    }

    pub fn held(&self) -> usize {
        self.held.len()
    }
}

// ---------------------------------------------------------------------------
// Mode hints: broker packets -> DisplayMode events for the guest
// ---------------------------------------------------------------------------

/// Decides the guest's display mode from what the broker reports, and says
/// only when it changes.
///
/// A broker with `CAP_MODE_HINTS` says what it wants (`EV_MODE_HINT`); one
/// without gets the old contract: an `EV_SURFACE` flagged fullscreen is the
/// output's mode, and a windowed one means "back to the configured mode".
/// Starts from the configured mode, which is what the guest booted with.
#[derive(Clone, Copy, Debug)]
pub struct ModePolicy {
    configured: DisplayMode,
    hints: bool,
    last: (u32, u32, u32),
}

impl ModePolicy {
    pub fn new(configured: DisplayMode) -> Self {
        Self {
            configured,
            hints: false,
            last: (
                configured.width,
                configured.height,
                configured.refresh_hz * 1000,
            ),
        }
    }

    fn configured(&self) -> (u32, u32, u32) {
        (
            self.configured.width,
            self.configured.height,
            self.configured.refresh_hz * 1000,
        )
    }

    fn resolve(&self, x: i32, y: i32, mhz: u32) -> (u32, u32, u32) {
        if x <= 0 || y <= 0 {
            return self.configured();
        }
        // The guest clamps too; these are the broker's own limits.
        let w = (x as u32).clamp(64, 8192);
        let h = (y as u32).clamp(64, 8192);
        let mhz = if mhz == 0 {
            self.configured.refresh_hz * 1000
        } else {
            mhz.clamp(1000, 1_000_000)
        };
        (w, h, mhz)
    }

    /// The mode the guest should switch to after this packet, if it changed.
    pub fn packet(&mut self, p: &wire::Pkt) -> Option<DisplayModeEvent> {
        use wire::*;
        let want = match p.ty {
            EV_HELLO => {
                self.hints = p.w1 & CAP_MODE_HINTS != 0;
                return None;
            }
            EV_MODE_HINT if self.hints => self.resolve(p.x, p.y, p.w0),
            EV_SURFACE if !self.hints && p.x > 0 && p.y > 0 => {
                if p.flags & F_FULLSCREEN != 0 {
                    self.resolve(p.x, p.y, p.w0)
                } else {
                    self.configured()
                }
            }
            _ => return None,
        };
        if want == self.last {
            return None;
        }
        self.last = want;
        Some(DisplayModeEvent {
            scanout: 0,
            width: want.0,
            height: want.1,
            refresh_mhz: want.2,
        })
    }

    pub fn current(&self) -> (u32, u32, u32) {
        self.last
    }
}

// ---------------------------------------------------------------------------
// The broker link
// ---------------------------------------------------------------------------

/// Where input goes: the event queue, in the vhost-user binary.
pub trait InputSink: Send {
    /// Deliver as many of `events` as fit in the buffers the guest posted, in
    /// order. Returns how many were consumed (delivered, or deliberately
    /// dropped); the rest are retried shortly.
    fn push(&mut self, events: &[InputEventEntry]) -> usize;

    /// Deliver a `DisplayMode` event. `false` if no buffer was posted; it is
    /// retried shortly (only the newest pending mode is kept).
    fn mode(&mut self, m: &DisplayModeEvent) -> bool {
        let _ = m;
        true
    }
}

/// What became of one flip. None of these is an error to the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipOutcome {
    Sent,
    /// The socket was full: this frame dropped, latest wins.
    Busy,
    /// No broker connected: dropped.
    NoBroker,
    /// The connection broke on this send; dropped, reconnect pending.
    Broken,
    /// The broker cannot take this (no `CAP_CURSOR`); kept for a broker that
    /// can, and not sent.
    Unsupported,
}

/// The last cursor the guest showed, kept so a broker that connects later --
/// or a send that found the socket full -- still gets it.
struct SentCursor {
    /// A dup of the exported dma-buf; `None` when the cursor is hidden.
    fd: Option<Arc<OwnedFd>>,
    c: CursorUpdate,
}

#[derive(Default)]
struct LinkState {
    sock: Option<Arc<OwnedFd>>,
    /// Size last asked of the broker with WINDOW, per connection.
    last_size: Option<(u32, u32)>,
    /// Formats already asked about on this connection.
    queried: HashSet<(u32, u64)>,
    /// HELLO's capability bits on this connection (0 until it arrives).
    broker_caps: u32,
    /// The guest's cursor, and whether this connection still needs it.
    cursor: Option<SentCursor>,
    cursor_dirty: bool,
}

/// Counters, for the teardown log.
#[derive(Default, Debug)]
pub struct LinkStats {
    pub sent: AtomicU64,
    pub busy: AtomicU64,
    pub no_broker: AtomicU64,
    pub broken: AtomicU64,
    pub input_events: AtomicU64,
    pub input_dropped: AtomicU64,
    pub connects: AtomicU64,
    pub cursors: AtomicU64,
    pub modes: AtomicU64,
}

/// The backend's connection to the display broker. Shared between the
/// thread serving guest RPCs (which sends) and the link's own thread (which
/// connects, reads input, and reconnects).
pub struct DisplayLink {
    path: Option<PathBuf>,
    /// The mode the guest boots with (device config); what "restore" means.
    configured: DisplayMode,
    state: Mutex<LinkState>,
    pub stats: LinkStats,
}

impl DisplayLink {
    /// A link to the broker at `path`; `None` acks and drops every flip.
    pub fn new(path: Option<PathBuf>) -> Arc<Self> {
        Self::with_mode(path, DisplayMode::DEFAULT)
    }

    /// A link whose "configured mode" (restore target) is `mode`.
    pub fn with_mode(path: Option<PathBuf>, mode: DisplayMode) -> Arc<Self> {
        Arc::new(Self {
            path,
            configured: mode,
            state: Mutex::new(LinkState::default()),
            stats: LinkStats::default(),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn connected(&self) -> bool {
        self.state.lock().unwrap().sock.is_some()
    }

    /// Adopt an already-connected socket (tests, or a VMM that passes one).
    pub fn adopt(&self, sock: OwnedFd) {
        set_nonblocking(sock.as_raw_fd());
        let mut st = self.state.lock().unwrap();
        // The guest's cursor outlives a connection: the next broker gets it
        // once it has said HELLO (and whether it takes cursors at all).
        let cursor = st.cursor.take();
        *st = LinkState {
            sock: Some(Arc::new(sock)),
            cursor,
            ..Default::default()
        };
        self.stats.connects.fetch_add(1, Ordering::Relaxed);
    }

    fn current(&self) -> Option<Arc<OwnedFd>> {
        self.state.lock().unwrap().sock.clone()
    }

    /// Drop `sock` if it is still the current connection.
    fn drop_conn(&self, sock: &Arc<OwnedFd>) {
        let mut st = self.state.lock().unwrap();
        if st.sock.as_ref().is_some_and(|s| Arc::ptr_eq(s, sock)) {
            // SAFETY: shutdown on a live descriptor; wakes the reader's poll.
            unsafe { libc::shutdown(sock.as_raw_fd(), libc::SHUT_RDWR) };
            let cursor = st.cursor.take();
            *st = LinkState {
                cursor,
                ..Default::default()
            };
        }
    }

    /// Present `dmabuf` as described by `f`. Never blocks.
    pub fn flip(&self, dmabuf: RawFd, f: &ScanoutFlip) -> FlipOutcome {
        let mut st = self.state.lock().unwrap();
        let Some(sock) = st.sock.clone() else {
            self.stats.no_broker.fetch_add(1, Ordering::Relaxed);
            return FlipOutcome::NoBroker;
        };
        let fd = sock.as_raw_fd();

        // Control records first, in their own sendmsg: the fd must ride on
        // the first byte of the ATTACH record, and SCM_RIGHTS attaches to the
        // first byte of whatever one sendmsg carries.
        let mut ctl: Vec<u8> = Vec::new();
        if st.last_size != Some((f.width, f.height)) {
            ctl.extend_from_slice(
                &wire::Cmd {
                    ty: wire::CMD_WINDOW,
                    width: f.width,
                    height: f.height,
                    ..Default::default()
                }
                .encode(),
            );
        }
        let fmt = (f.fourcc, f.modifier);
        let ask = !st.queried.contains(&fmt);
        if ask {
            ctl.extend_from_slice(
                &wire::Cmd {
                    ty: wire::CMD_QUERY_FORMAT,
                    fourcc: f.fourcc,
                    modifier: f.modifier,
                    ..Default::default()
                }
                .encode(),
            );
        }
        if !ctl.is_empty() {
            match send_records(fd, &ctl, None) {
                Ok(()) => {
                    st.last_size = Some((f.width, f.height));
                    if ask {
                        st.queried.insert(fmt);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.stats.busy.fetch_add(1, Ordering::Relaxed);
                    return FlipOutcome::Busy;
                }
                Err(e) => {
                    drop(st);
                    log::warn!("display: broker send failed: {e}; reconnecting");
                    self.drop_conn(&sock);
                    self.stats.broken.fetch_add(1, Ordering::Relaxed);
                    return FlipOutcome::Broken;
                }
            }
        }

        // seq is our clock at the flip, not a counter: after CMD_CAPS
        // (CLIENT_SEQ_USEC) the broker measures flip -> screen with it. It is
        // advisory to a broker that was not told.
        let stamp = mono_us() as u32;
        let mut frame = [0u8; 2 * wire::CMD_SIZE];
        frame[..wire::CMD_SIZE].copy_from_slice(
            &wire::Cmd {
                ty: wire::CMD_ATTACH,
                width: f.width,
                height: f.height,
                stride: f.stride,
                offset: f.offset,
                fourcc: f.fourcc,
                modifier: f.modifier,
                seq: stamp,
                ..Default::default()
            }
            .encode(),
        );
        frame[wire::CMD_SIZE..].copy_from_slice(
            &wire::Cmd {
                ty: wire::CMD_COMMIT,
                seq: stamp,
                ..Default::default()
            }
            .encode(),
        );
        match send_records(fd, &frame, Some(dmabuf)) {
            Ok(()) => {
                self.stats.sent.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Sent
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.stats.busy.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Busy
            }
            Err(e) => {
                drop(st);
                log::warn!("display: broker send failed: {e}; reconnecting");
                self.drop_conn(&sock);
                self.stats.broken.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Broken
            }
        }
    }

    /// The guest's cursor changed: `dmabuf` is the exported cursor plane
    /// buffer, or `None` when the guest hid it. Never blocks. Kept and re-sent
    /// to a broker that connects later or whose socket was full.
    pub fn cursor(&self, dmabuf: Option<RawFd>, c: &CursorUpdate) -> FlipOutcome {
        let fd = match dmabuf {
            // SAFETY: a borrowed descriptor that is live for this call; the
            // dup is ours.
            Some(raw) => {
                match unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) }.try_clone_to_owned() {
                    Ok(o) => Some(Arc::new(o)),
                    Err(e) => {
                        log::warn!("display: cursor: dup: {e}; cursor update dropped");
                        return FlipOutcome::Busy;
                    }
                }
            }
            None => None,
        };
        let mut st = self.state.lock().unwrap();
        st.cursor = Some(SentCursor { fd, c: *c });
        st.cursor_dirty = true;
        self.send_cursor_locked(&mut st)
    }

    /// Send the kept cursor if this connection still needs it.
    fn send_cursor_locked(&self, st: &mut LinkState) -> FlipOutcome {
        let Some(sock) = st.sock.clone() else {
            return FlipOutcome::NoBroker;
        };
        if !st.cursor_dirty {
            return FlipOutcome::Sent;
        }
        if st.broker_caps & wire::CAP_CURSOR == 0 {
            // Before HELLO the caps are unknown; HELLO re-arms the send.
            return FlipOutcome::Unsupported;
        }
        let Some(cur) = st.cursor.as_ref() else {
            st.cursor_dirty = false;
            return FlipOutcome::Sent;
        };
        let (cmd, fd) = match &cur.fd {
            Some(fd) if cur.c.visible() => (
                wire::Cmd {
                    ty: wire::CMD_CURSOR,
                    width: cur.c.width,
                    height: cur.c.height,
                    stride: cur.c.stride,
                    offset: cur.c.offset,
                    fourcc: cur.c.fourcc,
                    modifier: cur.c.modifier,
                    seq: wire::cursor_hot(cur.c.hot_x, cur.c.hot_y),
                    ..Default::default()
                },
                Some(fd.as_raw_fd()),
            ),
            _ => (
                wire::Cmd {
                    ty: wire::CMD_CURSOR,
                    ..Default::default()
                },
                None,
            ),
        };
        match send_records(sock.as_raw_fd(), &cmd.encode(), fd) {
            Ok(()) => {
                st.cursor_dirty = false;
                self.stats.cursors.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Sent
            }
            // Stays dirty: the link thread retries within a poll period.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => FlipOutcome::Busy,
            Err(e) => {
                log::warn!("display: broker send failed: {e}; reconnecting");
                // SAFETY: shutdown on a live descriptor; the link thread
                // notices and reconnects.
                unsafe { libc::shutdown(sock.as_raw_fd(), libc::SHUT_RDWR) };
                FlipOutcome::Broken
            }
        }
    }

    /// A cursor send is owed (socket was full, or a new broker said HELLO).
    fn cursor_owed(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.cursor_dirty && st.sock.is_some() && st.broker_caps & wire::CAP_CURSOR != 0
    }

    fn retry_cursor(&self) {
        let mut st = self.state.lock().unwrap();
        let _ = self.send_cursor_locked(&mut st);
    }

    /// HELLO: remember what this broker takes, tell it our frames are
    /// timestamped, and owe it the guest's cursor.
    fn hello(&self, caps: u32) {
        let mut st = self.state.lock().unwrap();
        st.broker_caps = caps;
        st.cursor_dirty = st.cursor.is_some();
        if let Some(sock) = st.sock.clone() {
            let caps_cmd = wire::Cmd {
                ty: wire::CMD_CAPS,
                width: wire::CLIENT_SEQ_USEC,
                ..Default::default()
            };
            if let Err(e) = send_records(sock.as_raw_fd(), &caps_cmd.encode(), None) {
                log::debug!("display: CAPS not sent: {e}");
            }
        }
    }

    /// The guest turned the scanout off. The broker protocol has no detach;
    /// the window keeps the last frame. Forget the requested size so the next
    /// enable asks again.
    pub fn disable(&self) {
        self.state.lock().unwrap().last_size = None;
    }

    /// Try once to connect. `Ok(false)` when there is no path to connect to.
    pub fn try_connect(&self) -> io::Result<bool> {
        let Some(path) = self.path.as_ref() else {
            return Ok(false);
        };
        let sock = connect_unix(path)?;
        self.adopt(sock);
        log::info!("display: connected to broker at {}", path.display());
        Ok(true)
    }

    /// The link's own thread: connect (with backoff), read broker packets,
    /// translate input and push it to `sink`, reconnect when the broker goes.
    /// Returns only when `stop` is set.
    pub fn run(
        self: &Arc<Self>,
        mut sink: Box<dyn InputSink>,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        const RETRY_MIN: Duration = Duration::from_millis(100);
        const RETRY_MAX: Duration = Duration::from_millis(1000);
        let mut retry = RETRY_MIN;
        let mut tr = InputTranslator::default();
        let mut reader = wire::PktReader::default();
        let mut pending: Vec<InputEventEntry> = Vec::new();
        let mut policy = ModePolicy::new(self.configured);
        let mut pending_mode: Option<DisplayModeEvent> = None;
        let mut warned_absent = false;

        while !stop.load(Ordering::Relaxed) {
            let Some(sock) = self.current() else {
                // A disconnect releases whatever was held.
                tr.release_all(&mut pending);
                self.deliver(&mut *sink, &mut pending);
                reader.reset();
                match self.try_connect() {
                    Ok(true) => {
                        retry = RETRY_MIN;
                        warned_absent = false;
                        continue;
                    }
                    Ok(false) => {
                        // No path: nothing to read, only adopted sockets.
                        std::thread::sleep(RETRY_MAX);
                        continue;
                    }
                    Err(e) => {
                        if !warned_absent {
                            log::info!(
                                "display: no broker at {} ({e}); flips are dropped until one appears",
                                self.path.as_deref().unwrap_or(Path::new("?")).display()
                            );
                            warned_absent = true;
                        }
                        std::thread::sleep(retry);
                        retry = (retry * 2).min(RETRY_MAX);
                        continue;
                    }
                }
            };

            // Undeliverable input (no buffer posted) is retried soon rather
            // than on the next packet: a lost key release is a stuck key.
            let timeout = if pending.is_empty() && pending_mode.is_none() {
                if self.cursor_owed() { 20 } else { 500 }
            } else {
                2
            };
            let mut pfd = libc::pollfd {
                fd: sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd, valid for the call.
            let n = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    log::warn!("display: poll: {e}");
                    self.drop_conn(&sock);
                }
                continue;
            }
            if n > 0 {
                let mut buf = [0u8; 64 * wire::PKT_SIZE];
                // SAFETY: a live buffer of the stated length.
                let r = unsafe {
                    libc::recv(
                        sock.as_raw_fd(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if r == 0 {
                    log::info!("display: broker closed the connection");
                    self.drop_conn(&sock);
                    continue;
                }
                if r < 0 {
                    let e = io::Error::last_os_error();
                    if !matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        log::warn!("display: recv: {e}");
                        self.drop_conn(&sock);
                        continue;
                    }
                } else {
                    let mut bye = false;
                    reader.feed(&buf[..r as usize], |p| {
                        self.note_packet(&p);
                        if p.ty == wire::EV_BYE {
                            bye = true;
                        }
                        if let Some(m) = policy.packet(&p) {
                            pending_mode = Some(m);
                        }
                        tr.packet(&p, &mut pending);
                    });
                    if bye {
                        self.drop_conn(&sock);
                    }
                }
            }
            self.deliver(&mut *sink, &mut pending);
            if let Some(m) = pending_mode
                && sink.mode(&m)
            {
                pending_mode = None;
                self.stats.modes.fetch_add(1, Ordering::Relaxed);
                log::info!(
                    "display: guest mode -> {}x{}@{}.{:03}",
                    m.width,
                    m.height,
                    m.refresh_mhz / 1000,
                    m.refresh_mhz % 1000
                );
            }
            if self.cursor_owed() {
                self.retry_cursor();
            }
        }
    }

    fn deliver(&self, sink: &mut dyn InputSink, pending: &mut Vec<InputEventEntry>) {
        if pending.is_empty() {
            return;
        }
        let n = sink.push(pending).min(pending.len());
        self.stats
            .input_events
            .fetch_add(n as u64, Ordering::Relaxed);
        pending.drain(..n);
        // Bound the backlog: a guest that never posts buffers must not grow
        // this without limit. Motion is what goes; it is superseded anyway.
        if pending.len() > 4096 {
            let before = pending.len();
            let keep: Vec<_> = pending
                .iter()
                .copied()
                .filter(|e| e.ev_type == input::EV_KEY || e.ev_type == input::EV_SYN)
                .collect();
            *pending = keep;
            self.stats
                .input_dropped
                .fetch_add((before - pending.len()) as u64, Ordering::Relaxed);
        }
    }

    fn note_packet(&self, p: &wire::Pkt) {
        use wire::*;
        match p.ty {
            EV_HELLO => {
                if p.w0 != PROTO_VERSION {
                    log::warn!(
                        "display: broker speaks protocol {}, this backend {PROTO_VERSION}",
                        p.w0
                    );
                }
                log::info!("display: broker hello, version {}, caps {:#x}", p.w0, p.w1);
                self.hello(p.w1);
            }
            EV_FORMAT => {
                let m = (p.w0 as u64) | ((p.w1 as u64) << 32);
                if p.x == 1 {
                    log::info!(
                        "display: broker can show fourcc {:#010x} modifier {m:#018x}",
                        p.y as u32
                    );
                } else {
                    log::warn!(
                        "display: broker CANNOT show fourcc {:#010x} modifier {m:#018x}; \
                         frames in it will be dropped (no copy fallback by design)",
                        p.y as u32
                    );
                }
            }
            EV_SURFACE => log::debug!(
                "display: broker window {}x{}{}",
                p.x,
                p.y,
                if p.flags & F_FULLSCREEN != 0 {
                    " (fullscreen)"
                } else {
                    ""
                }
            ),
            EV_MODE_HINT => log::debug!(
                "display: broker mode hint {}x{} @{} mHz (reason {})",
                p.x,
                p.y,
                p.w0,
                p.w1
            ),
            EV_CLOSE => log::info!("display: the user closed the viewer window"),
            EV_BYE => log::info!("display: broker says goodbye (reason {})", p.x),
            _ => {}
        }
    }
}

/// One non-blocking `sendmsg` of whole records, optionally carrying `fd`.
/// A short write would desynchronise the broker's fixed framing, so it is an
/// error, never a partial success.
fn send_records(sock: RawFd, bytes: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: zeroed msghdr is a valid empty one.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let mut cbuf = [0u64; 4]; // CMSG_SPACE(4) == 24 on x86-64, aligned
    if let Some(fd) = fd {
        // SAFETY: CMSG macros over a buffer large enough for one int.
        unsafe {
            let space = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize;
            debug_assert!(space <= std::mem::size_of_val(&cbuf));
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = space;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
        }
    }
    loop {
        // SAFETY: msg and everything it points at live across the call.
        let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n as usize != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("short write {n}/{}", bytes.len()),
            ));
        }
        return Ok(());
    }
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: fcntl with integer arguments on a descriptor we hold.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

/// Connect a non-blocking AF_UNIX stream socket to `path`. A unix connect to
/// a listener completes at once or fails at once; it never waits.
fn connect_unix(path: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zeroed sockaddr_un is valid.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= sa.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path empty or too long",
        ));
    }
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in sa.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: plain socket(2).
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor, owned from here.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: sa is a valid sockaddr_un of at least `len` bytes.
    let r = unsafe { libc::connect(fd, (&sa as *const libc::sockaddr_un).cast(), len) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn modes_parse_and_nonsense_does_not() {
        assert_eq!(
            DisplayMode::parse("2560x1440@240").unwrap(),
            DisplayMode::DEFAULT
        );
        assert_eq!(
            DisplayMode::parse("1920x1080").unwrap(),
            DisplayMode {
                width: 1920,
                height: 1080,
                refresh_hz: 240
            }
        );
        for bad in [
            "",
            "2560",
            "0x1440@60",
            "2560x1440@0",
            "99999x10@60",
            "axb@c",
        ] {
            assert!(DisplayMode::parse(bad).is_err(), "{bad} parsed");
        }
        assert_eq!(DisplayMode::DEFAULT.to_string(), "2560x1440@240");
    }

    fn memfd() -> OwnedFd {
        // SAFETY: plain memfd_create.
        let fd = unsafe { libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn a_buffer_is_exported_once_and_dropped_on_close() {
        let mut c = DmabufCache::default();
        let mut exports = 0;
        let a = c
            .get_or_export(5, 1, || {
                exports += 1;
                Ok(memfd())
            })
            .unwrap();
        let b = c.get_or_export(5, 1, || panic!("exported twice")).unwrap();
        assert_eq!(a, b);
        assert_eq!(exports, 1);
        c.get_or_export(5, 2, || Ok(memfd())).unwrap();
        c.get_or_export(6, 1, || Ok(memfd())).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c.exports(), 3);

        // GEM close: the handle number may be reissued, so the next flip of
        // it must export again.
        assert!(c.forget(5, 1));
        assert!(!c.forget(5, 1));
        let mut again = false;
        c.get_or_export(5, 1, || {
            again = true;
            Ok(memfd())
        })
        .unwrap();
        assert!(again);

        // File close drops only that file's entries.
        assert_eq!(c.forget_owner(5), 2);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c.get_or_export(7, 7, || Err(libc::ENOENT)),
            Err(libc::ENOENT)
        );
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_the_least_recent() {
        let mut c = DmabufCache::with_capacity(2);
        c.get_or_export(1, 1, || Ok(memfd())).unwrap();
        c.get_or_export(1, 2, || Ok(memfd())).unwrap();
        c.get_or_export(1, 1, || panic!()).unwrap(); // 1,1 now most recent
        c.get_or_export(1, 3, || Ok(memfd())).unwrap(); // evicts 1,2
        assert_eq!(c.len(), 2);
        c.get_or_export(1, 1, || panic!("1,1 was evicted")).unwrap();
        let mut re = false;
        c.get_or_export(1, 2, || {
            re = true;
            Ok(memfd())
        })
        .unwrap();
        assert!(re);
    }

    #[test]
    fn prime_export_asks_for_the_right_handle_and_flags() {
        let fd = prime_export(42, 9, |drm, req, arg| {
            assert_eq!(drm, 42);
            assert_eq!(req, DRM_IOCTL_PRIME_HANDLE_TO_FD);
            assert_eq!(u32::from_le_bytes(arg[0..4].try_into().unwrap()), 9);
            assert_eq!(
                u32::from_le_bytes(arg[4..8].try_into().unwrap()),
                (libc::O_CLOEXEC | libc::O_RDWR) as u32
            );
            let m = memfd();
            arg[8..12].copy_from_slice(&std::os::fd::IntoRawFd::into_raw_fd(m).to_le_bytes());
            Ok(())
        })
        .unwrap();
        assert!(fd.as_raw_fd() >= 0);
        assert_eq!(
            prime_export(1, 1, |_, _, _| Err(libc::EINVAL)).err(),
            Some(libc::EINVAL)
        );
    }

    #[test]
    fn wire_records_have_the_c_layout() {
        let c = wire::Cmd {
            ty: wire::CMD_ATTACH,
            flags: 0,
            width: 2560,
            height: 1440,
            stride: 10240,
            offset: 64,
            fourcc: 0x3432_5258,
            modifier: 0x0300_0000_0060_6014,
            seq: 77,
        };
        let b = c.encode();
        assert_eq!(&b[0..2], &1u16.to_le_bytes());
        assert_eq!(&b[4..8], &2560u32.to_le_bytes());
        assert_eq!(&b[24..32], &0x0300_0000_0060_6014u64.to_le_bytes());
        assert_eq!(&b[32..36], &77u32.to_le_bytes());
        assert_eq!(&b[36..40], &[0; 4]);
        assert_eq!(wire::Cmd::decode(&b), c);

        let p = wire::Pkt {
            ty: wire::EV_ABS,
            flags: 2,
            seq: 9,
            x: -1,
            y: 5,
            w0: 100,
            w1: 200,
        };
        assert_eq!(wire::Pkt::decode(&p.encode()), p);
    }

    #[test]
    fn packets_reassemble_across_reads() {
        let a = wire::Pkt {
            ty: wire::EV_KEY,
            x: 30,
            y: 1,
            ..Default::default()
        }
        .encode();
        let b = wire::Pkt {
            ty: wire::EV_KEY,
            x: 30,
            y: 0,
            ..Default::default()
        }
        .encode();
        let mut all = a.to_vec();
        all.extend_from_slice(&b);
        let mut r = wire::PktReader::default();
        let mut got = Vec::new();
        r.feed(&all[..7], |p| got.push(p));
        assert!(got.is_empty());
        r.feed(&all[7..30], |p| got.push(p));
        assert_eq!(got.len(), 1);
        r.feed(&all[30..], |p| got.push(p));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].y, 0);
    }

    fn ev(t: u16, c: u16, v: i32) -> InputEventEntry {
        InputEventEntry::new(t, c, v)
    }

    #[test]
    fn input_translates_to_linux_events() {
        use input::*;
        let mut t = InputTranslator::default();
        let mut out = Vec::new();
        let pkt = |ty, x, y, w0, w1| wire::Pkt {
            ty,
            x,
            y,
            w0,
            w1,
            ..Default::default()
        };
        t.packet(&pkt(wire::EV_KEY, 30, 1, 0, 0), &mut out);
        t.packet(&pkt(wire::EV_BTN, 0x110, 1, 0, 0), &mut out);
        t.packet(&pkt(wire::EV_ABS, 1279, 0, 1280, 720), &mut out);
        t.packet(&pkt(wire::EV_REL, -3, 0, 0, 0), &mut out);
        t.packet(&pkt(wire::EV_WHEEL, 1, 0, 0, 0), &mut out);
        assert_eq!(
            out,
            vec![
                ev(EV_KEY, 30, 1),
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_KEY, 0x110, 1),
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_ABS, ABS_X, INPUT_ABS_MAX),
                ev(EV_ABS, ABS_Y, 0),
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_REL, REL_X, -3),
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_REL, REL_WHEEL, 1),
                ev(EV_REL, REL_WHEEL_HI_RES, 120),
                ev(EV_SYN, SYN_REPORT, 0),
            ]
        );
        assert_eq!(t.held(), 2);

        // Focus loss releases what is held, so no key sticks in the guest.
        out.clear();
        t.packet(&pkt(wire::EV_FOCUS, 0, 0, 0, 0), &mut out);
        assert_eq!(
            out,
            vec![
                ev(EV_KEY, 30, 0),
                ev(EV_KEY, 0x110, 0),
                ev(EV_SYN, SYN_REPORT, 0)
            ]
        );
        assert_eq!(t.held(), 0);

        // Out-of-range key codes are dropped, not forwarded.
        out.clear();
        t.packet(&pkt(wire::EV_KEY, 0x1000, 1, 0, 0), &mut out);
        t.packet(&pkt(wire::EV_KEY, -1, 1, 0, 0), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn absolute_positions_scale_to_the_fixed_range() {
        assert_eq!(InputTranslator::scale_abs(0, 2560), 0);
        assert_eq!(InputTranslator::scale_abs(2559, 2560), INPUT_ABS_MAX);
        assert_eq!(InputTranslator::scale_abs(5000, 2560), INPUT_ABS_MAX);
        assert_eq!(InputTranslator::scale_abs(-4, 2560), 0);
        let mid = InputTranslator::scale_abs(1280, 2561);
        assert!((mid - INPUT_ABS_MAX / 2).abs() <= 1, "{mid}");
        assert_eq!(InputTranslator::scale_abs(7, 0), 7);
    }

    fn socketpair() -> (OwnedFd, OwnedFd) {
        let mut sv = [0i32; 2];
        // SAFETY: plain socketpair.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) }
    }

    /// Read one 40-byte record as the broker does, with any fd it carries.
    fn broker_recv(fd: RawFd) -> (wire::Cmd, Option<OwnedFd>) {
        let mut buf = [0u8; wire::CMD_SIZE];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut cbuf = [0u64; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&cbuf);
        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        assert_eq!(n, wire::CMD_SIZE as isize, "a whole record per read");
        let mut got = None;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            if !c.is_null() && (*c).cmsg_type == libc::SCM_RIGHTS {
                assert_eq!((*c).cmsg_len, libc::CMSG_LEN(4) as usize, "exactly one fd");
                let f = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd);
                got = Some(OwnedFd::from_raw_fd(f));
            }
        }
        (wire::Cmd::decode(&buf), got)
    }

    fn inode(fd: RawFd) -> u64 {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd, &mut st) }, 0);
        st.st_ino
    }

    fn flip(seq: u64, w: u32) -> ScanoutFlip {
        ScanoutFlip {
            owner_handle: 3,
            host_handle: 4,
            width: w,
            height: 1440,
            stride: w * 4,
            fourcc: 0x3432_5258,
            modifier: 0,
            seq,
            ..Default::default()
        }
    }

    #[test]
    fn a_flip_reaches_a_fake_broker_as_attach_with_fd_then_commit() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        assert_eq!(link.flip(0, &flip(1, 2560)), FlipOutcome::NoBroker);
        link.adopt(ours);
        let buf = memfd();

        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(1, 2560)),
            FlipOutcome::Sent
        );
        // First flip of a size/format: WINDOW and QUERY_FORMAT, no fd.
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!((c.ty, c.width, c.height), (wire::CMD_WINDOW, 2560, 1440));
        assert!(f.is_none());
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!(c.ty, wire::CMD_QUERY_FORMAT);
        assert!(f.is_none());
        // Then the frame: the fd rides on ATTACH and only on ATTACH.
        let before = mono_us() as u32;
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!(c.ty, wire::CMD_ATTACH);
        assert_eq!((c.width, c.stride, c.fourcc), (2560, 10240, 0x3432_5258));
        // seq is the flip's CLOCK_MONOTONIC microseconds (CLIENT_SEQ_USEC).
        assert!(
            before.wrapping_sub(c.seq) < 5_000_000,
            "seq {} vs now {before}",
            c.seq
        );
        let f = f.expect("ATTACH carries the dma-buf");
        assert_eq!(inode(f.as_raw_fd()), inode(buf.as_raw_fd()));
        let attach_seq = c.seq;
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!(
            c,
            wire::Cmd {
                ty: wire::CMD_COMMIT,
                seq: attach_seq,
                ..Default::default()
            }
        );
        assert!(f.is_none());

        // Steady state: exactly ATTACH+COMMIT per flip.
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(2, 2560)),
            FlipOutcome::Sent
        );
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_ATTACH);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_COMMIT);

        // A mode change asks for a new window first.
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(3, 1920)),
            FlipOutcome::Sent
        );
        let (c, _) = broker_recv(broker.as_raw_fd());
        assert_eq!((c.ty, c.width), (wire::CMD_WINDOW, 1920));
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_ATTACH);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_COMMIT);
        assert_eq!(link.stats.sent.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_full_socket_drops_the_frame_instead_of_blocking() {
        let (ours, _broker) = socketpair();
        let link = DisplayLink::new(None);
        link.adopt(ours);
        let buf = memfd();
        let mut busy = 0;
        let start = std::time::Instant::now();
        // The broker never reads; the socket fills and flips start dropping.
        for i in 0..100_000u64 {
            if link.flip(buf.as_raw_fd(), &flip(i, 2560)) == FlipOutcome::Busy {
                busy += 1;
                if busy > 10 {
                    break;
                }
            }
        }
        assert!(busy > 10, "the socket never filled");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(link.connected(), "a busy broker is not a dead one");
    }

    #[test]
    fn a_dead_broker_is_dropped_and_flips_are_still_acked() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        link.adopt(ours);
        drop(broker);
        let buf = memfd();
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(1, 2560)),
            FlipOutcome::Broken
        );
        assert!(!link.connected());
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(2, 2560)),
            FlipOutcome::NoBroker
        );
    }

    struct VecSink(Arc<Mutex<Vec<InputEventEntry>>>);
    impl InputSink for VecSink {
        fn push(&mut self, events: &[InputEventEntry]) -> usize {
            self.0.lock().unwrap().extend_from_slice(events);
            events.len()
        }
    }

    fn pkt(ty: u16, flags: u16, x: i32, y: i32, w0: u32, w1: u32) -> wire::Pkt {
        wire::Pkt {
            ty,
            flags,
            x,
            y,
            w0,
            w1,
            ..Default::default()
        }
    }

    /// A broker with mode hints: fullscreen asks for the output, restore goes
    /// back to the configured mode, nothing is said twice.
    #[test]
    fn mode_hints_become_display_mode_events_once() {
        let cfg = DisplayMode {
            width: 2560,
            height: 1440,
            refresh_hz: 240,
        };
        let mut p = ModePolicy::new(cfg);
        assert_eq!(
            p.packet(&pkt(wire::EV_HELLO, 0, 0, 0, 2, wire::CAP_MODE_HINTS)),
            None
        );
        // The handshake's EV_SURFACE is not a mode decision any more.
        assert_eq!(p.packet(&pkt(wire::EV_SURFACE, 0, 1920, 1080, 0, 0)), None);
        // Restore while already configured: nothing to do.
        assert_eq!(p.packet(&pkt(wire::EV_MODE_HINT, 0, 0, 0, 0, 0)), None);
        let fs = p
            .packet(&pkt(
                wire::EV_MODE_HINT,
                wire::F_FULLSCREEN,
                5120,
                1440,
                239_960,
                1,
            ))
            .unwrap();
        assert_eq!((fs.width, fs.height, fs.refresh_mhz), (5120, 1440, 239_960));
        assert_eq!(
            p.packet(&pkt(
                wire::EV_MODE_HINT,
                wire::F_FULLSCREEN,
                5120,
                1440,
                239_960,
                1
            )),
            None,
            "the same hint twice is one re-mode"
        );
        let back = p.packet(&pkt(wire::EV_MODE_HINT, 0, 0, 0, 0, 0)).unwrap();
        assert_eq!(
            (back.width, back.height, back.refresh_mhz),
            (2560, 1440, 240_000)
        );
        // --resize=guest: the window's size; unknown refresh is the configured.
        let win = p
            .packet(&pkt(wire::EV_MODE_HINT, 0, 1277, 1413, 0, 2))
            .unwrap();
        assert_eq!(
            (win.width, win.height, win.refresh_mhz),
            (1277, 1413, 240_000)
        );
        // Nonsense is clamped, not passed on.
        let tiny = p
            .packet(&pkt(wire::EV_MODE_HINT, 0, 3, 99_999, 7, 2))
            .unwrap();
        assert_eq!(
            (tiny.width, tiny.height, tiny.refresh_mhz),
            (64, 8192, 1000)
        );
    }

    /// A broker without the capability: only a fullscreen EV_SURFACE re-modes,
    /// and a windowed one goes back to the configured mode.
    #[test]
    fn a_legacy_broker_re_modes_only_for_fullscreen() {
        let mut p = ModePolicy::new(DisplayMode::DEFAULT);
        p.packet(&pkt(wire::EV_HELLO, 0, 0, 0, 2, 0));
        assert_eq!(p.packet(&pkt(wire::EV_MODE_HINT, 0, 800, 600, 0, 2)), None);
        assert_eq!(
            p.packet(&pkt(wire::EV_SURFACE, 0, 1280, 720, 60_000, 0)),
            None
        );
        let fs = p
            .packet(&pkt(
                wire::EV_SURFACE,
                wire::F_FULLSCREEN,
                3840,
                2160,
                60_000,
                0,
            ))
            .unwrap();
        assert_eq!((fs.width, fs.height, fs.refresh_mhz), (3840, 2160, 60_000));
        let back = p
            .packet(&pkt(wire::EV_SURFACE, 0, 1280, 720, 60_000, 0))
            .unwrap();
        assert_eq!((back.width, back.height), (2560, 1440));
    }

    fn cursor(handle: u32, hx: u32, hy: u32) -> CursorUpdate {
        CursorUpdate {
            width: 64,
            height: 64,
            hot_x: hx,
            hot_y: hy,
            owner_handle: 3,
            host_handle: handle,
            stride: 256,
            fourcc: 0x3432_5241,
            flags: protocol::messages::CURSOR_F_VISIBLE,
            ..Default::default()
        }
    }

    /// The cursor waits for a broker that can take it, then goes as CMD_CURSOR
    /// with the dma-buf and the hotspot packed into seq; a hide carries nothing.
    #[test]
    fn the_cursor_reaches_a_capable_broker_and_survives_reconnects() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        link.adopt(ours);
        let buf = memfd();
        // No HELLO yet: kept, not sent.
        assert_eq!(
            link.cursor(Some(buf.as_raw_fd()), &cursor(9, 3, 60)),
            FlipOutcome::Unsupported
        );
        // A broker without CAP_CURSOR never sees one (it would be a violation).
        link.hello(0);
        let (c, _) = broker_recv(broker.as_raw_fd());
        assert_eq!((c.ty, c.width), (wire::CMD_CAPS, wire::CLIENT_SEQ_USEC));
        assert!(!link.cursor_owed());
        // A capable one gets the kept cursor.
        link.hello(wire::CAP_CURSOR);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_CAPS);
        assert!(link.cursor_owed());
        link.retry_cursor();
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!(c.ty, wire::CMD_CURSOR);
        assert_eq!(
            (c.width, c.height, c.stride, c.fourcc),
            (64, 64, 256, 0x3432_5241)
        );
        assert_eq!(c.seq, 3 | (60 << 16));
        assert_eq!(
            inode(f.expect("the cursor dma-buf").as_raw_fd()),
            inode(buf.as_raw_fd())
        );
        // Hidden: a bare CMD_CURSOR.
        let hidden = CursorUpdate::default();
        assert_eq!(link.cursor(None, &hidden), FlipOutcome::Sent);
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!(
            c,
            wire::Cmd {
                ty: wire::CMD_CURSOR,
                ..Default::default()
            }
        );
        assert!(f.is_none());
        // Shown again, then the broker goes and a new one comes: it is re-sent.
        assert_eq!(
            link.cursor(Some(buf.as_raw_fd()), &cursor(9, 1, 2)),
            FlipOutcome::Sent
        );
        broker_recv(broker.as_raw_fd());
        drop(broker);
        let (ours2, broker2) = socketpair();
        link.adopt(ours2);
        link.hello(wire::CAP_CURSOR);
        assert_eq!(broker_recv(broker2.as_raw_fd()).0.ty, wire::CMD_CAPS);
        link.retry_cursor();
        let (c, f) = broker_recv(broker2.as_raw_fd());
        assert_eq!((c.ty, c.seq), (wire::CMD_CURSOR, 1 | (2 << 16)));
        assert!(f.is_some());
        assert_eq!(link.stats.cursors.load(Ordering::Relaxed), 4);
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < until {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// End to end with a listening fake broker at a path: the link connects
    /// on its own, input comes back as events, and a broker restart is
    /// reconnected to with held keys released in between.
    #[test]
    fn the_link_connects_reads_input_and_reconnects() {
        use std::io::Write;
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("nvgpu-display-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broker.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let link = DisplayLink::new(Some(path.clone()));
        let got = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let th = {
            let (link, got, stop) = (link.clone(), got.clone(), stop.clone());
            std::thread::spawn(move || link.run(Box::new(VecSink(got)), &stop))
        };

        let (mut conn, _) = listener.accept().unwrap();
        let hello = wire::Pkt {
            ty: wire::EV_HELLO,
            w0: 2,
            ..Default::default()
        };
        let key = wire::Pkt {
            ty: wire::EV_KEY,
            x: 42,
            y: 1,
            ..Default::default()
        };
        conn.write_all(&hello.encode()).unwrap();
        conn.write_all(&key.encode()).unwrap();
        assert!(wait_for(|| got.lock().unwrap().len() == 2));
        assert!(link.connected());

        // The broker dies with the key held: the guest gets the release.
        drop(conn);
        assert!(wait_for(|| got.lock().unwrap().len() == 4));
        assert_eq!(got.lock().unwrap()[2], ev(input::EV_KEY, 42, 0));

        // And it comes back.
        let (mut conn, _) = listener.accept().unwrap();
        let abs = wire::Pkt {
            ty: wire::EV_ABS,
            x: 0,
            y: 0,
            w0: 100,
            w1: 100,
            ..Default::default()
        };
        conn.write_all(&abs.encode()).unwrap();
        assert!(wait_for(|| got.lock().unwrap().len() == 7));
        assert!(wait_for(|| link.connected()));

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        th.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

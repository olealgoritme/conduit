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
//! * The clipboard (docs/CLIPBOARD.md). `EV_CLIPBOARD` transfers from the
//!   broker are reassembled ([`ClipAssembler`]) and handed to the sink, which
//!   chunks them onto the event queue as `ClipboardFromHost`; the guest's
//!   `ClipboardToHost` text goes back as paced `CMD_CLIPBOARD` records
//!   ([`ClipOut`]). The viewer decides which directions are allowed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::messages::{
    CursorUpdate, DisplayModeEvent, INPUT_ABS_MAX, InputEventEntry, ScanoutFlip, ScanoutReleased,
    input,
};

pub use crate::scanout_release::BufKey;
use crate::scanout_release::ReleaseTracker;

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
    /// One fixed-size chunk of guest clipboard text (`struct
    /// nvkvm_broker_clip_cmd`): chunk index u32 at 4, info at 12, 27 data bytes.
    pub const CMD_CLIPBOARD: u16 = 4;
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
    /// One fixed-size chunk of host clipboard text (`struct
    /// nvkvm_broker_clip_pkt`): info at byte 8, 15 data bytes after it.
    pub const EV_CLIPBOARD: u16 = 15;
    pub const EV_FORMAT: u16 = 16;
    /// virtio-nvgpu: x,y = mode in buffer pixels (0,0 = the configured
    /// mode), w0 = refresh mHz (0 = configured), w1 = reason.
    pub const EV_MODE_HINT: u16 = 17;
    /// Conduit (stream host): one gamepad event. x = evdev code, y = value,
    /// w0 = pad << 16 | evdev type (EV_KEY, EV_ABS, EV_SYN); pad 0..3.
    /// Sent only to a client that declared [`CLIENT_GAMEPAD`].
    pub const EV_PAD: u16 = 18;
    /// Conduit: x = 1 the client wants frames from now on, 0 it does not
    /// (any more). Only from a client with [`CAP_IDLE`], which starts idle.
    pub const EV_ACTIVE: u16 = 19;

    pub const F_FULLSCREEN: u16 = 1 << 2;
    /// ATTACH flags: the fd is a sealed memfd to present from shared memory
    /// (`NVKVM_BROKER_CMD_F_SHM`), not a dma-buf.
    pub const CMD_F_SHM: u16 = 1 << 0;

    /// HELLO capability bits this backend cares about.
    pub const CAP_MODE_HINTS: u32 = 1 << 10;
    pub const CAP_CURSOR: u32 = 1 << 11;
    /// The broker takes and sends clipboard transfers up to
    /// [`CLIP_LARGE_MAX`] with a client that declared [`CLIENT_CLIP_LARGE`].
    pub const CAP_CLIP_LARGE: u32 = 1 << 12;
    /// The broker may send [`EV_PAD`] (a stream host).
    pub const CAP_GAMEPAD: u32 = 1 << 13;
    /// The broker starts idle and says [`EV_ACTIVE`] when it wants frames
    /// (a stream host with no client attached wants none). Also makes it a
    /// session client for the mode policy ([`super::ModeArbiter`]).
    pub const CAP_IDLE: u32 = 1 << 14;
    /// Conduit: the broker's [`EV_RELEASE`] carries, in x, the `seq` of the
    /// newest ATTACH of that buffer it covers, and every ATTACH is released
    /// eventually (one the display refused or dropped at once). Without it a
    /// buffer counts as done for that broker once it was sent another one.
    pub const CAP_RELEASE_SEQ: u32 = 1 << 15;
    /// CMD_CAPS bit: a clipboard agent is behind this client.
    pub const CLIENT_CLIPBOARD: u32 = 1 << 0;
    /// CMD_CAPS bit: ATTACH/COMMIT `seq` is our CLOCK_MONOTONIC microseconds.
    pub const CLIENT_SEQ_USEC: u32 = 1 << 1;
    /// CMD_CAPS bit: we take and send large clipboard transfers.
    pub const CLIENT_CLIP_LARGE: u32 = 1 << 2;
    /// CMD_CAPS bit: we carry [`EV_PAD`] to the guest's gamepads.
    pub const CLIENT_GAMEPAD: u32 = 1 << 3;
    /// CMD_CAPS bit: we honour [`EV_ACTIVE`] (send nothing to an idle
    /// broker) and arbitrate the mode between several clients, so a session
    /// that ends may just go idle instead of asking for a restore.
    pub const CLIENT_IDLE: u32 = 1 << 4;
    /// Gamepads the guest driver offers (nvgpu_pad.h).
    pub const MAX_PADS: u32 = 4;

    /// Clipboard framing: payload bytes per chunk, each direction.
    pub const CLIP_PKT_BYTES: usize = 15;
    pub const CLIP_CMD_BYTES: usize = 27;
    /// `info`: low 5 bits = meaningful bytes, this bit = last chunk.
    pub const CLIP_NBYTES_MASK: u8 = 0x1f;
    pub const CLIP_LAST: u8 = 0x20;
    /// Transfer caps: a broker without [`CAP_CLIP_LARGE`], and one with it.
    pub const CLIP_LEGACY_MAX: usize = 7168;
    pub const CLIP_LARGE_MAX: usize = 1 << 20;

    /// The clipboard cap a broker with HELLO capabilities `caps` accepts.
    pub const fn clip_cap(caps: u32) -> usize {
        if caps & CAP_CLIP_LARGE != 0 {
            CLIP_LARGE_MAX
        } else {
            CLIP_LEGACY_MAX
        }
    }

    /// One CMD_CLIPBOARD record: chunk `chunk` carrying `data` (at most
    /// [`CLIP_CMD_BYTES`]), flagged last if `last`.
    pub fn clip_cmd(chunk: u32, data: &[u8], last: bool) -> [u8; CMD_SIZE] {
        assert!(data.len() <= CLIP_CMD_BYTES);
        let mut o = [0u8; CMD_SIZE];
        o[0..2].copy_from_slice(&CMD_CLIPBOARD.to_le_bytes());
        // 2..4 flags = 0, 8..12 reserved1 = 0.
        o[4..8].copy_from_slice(&chunk.to_le_bytes());
        o[12] = data.len() as u8 | if last { CLIP_LAST } else { 0 };
        o[13..13 + data.len()].copy_from_slice(data);
        o
    }
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

        /// An EV_CLIPBOARD packet's `info` byte and data bytes (the meaningful
        /// prefix, bounded by the array: a lying `info` cannot reach past it).
        /// `None` when nbytes claims more than the packet holds.
        pub fn clip_payload(&self) -> Option<(bool, Vec<u8>)> {
            let b = self.encode();
            let info = b[8];
            let n = (info & CLIP_NBYTES_MASK) as usize;
            if n > CLIP_PKT_BYTES {
                return None;
            }
            Some((info & CLIP_LAST != 0, b[9..9 + n].to_vec()))
        }

        /// An EV_CLIPBOARD packet (tests and fakes).
        pub fn clip(data: &[u8], last: bool) -> Self {
            assert!(data.len() <= CLIP_PKT_BYTES);
            let mut b = [0u8; PKT_SIZE];
            b[0..2].copy_from_slice(&EV_CLIPBOARD.to_le_bytes());
            b[8] = data.len() as u8 | if last { CLIP_LAST } else { 0 };
            b[9..9 + data.len()].copy_from_slice(data);
            Self::decode(&b)
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
            EV_PAD => {
                // To the guest as an ordinary event whose type carries the
                // pad: (pad + 1) << 8 | type. Guests without gamepads drop it.
                let (pad, ty) = (p.w0 >> 16, (p.w0 & 0xffff) as u16);
                let ok_ty = matches!(ty, input::EV_KEY | input::EV_ABS | input::EV_SYN);
                let Ok(code) = u16::try_from(p.x) else { return };
                if pad >= MAX_PADS || !ok_ty || (ty == input::EV_KEY && code > input::KEY_MAX) {
                    return;
                }
                out.push(InputEventEntry::new(
                    ((pad as u16 + 1) << 8) | ty,
                    code,
                    p.y,
                ));
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

/// One display client's say in the guest's mode.
#[derive(Clone, Copy, Debug)]
struct ModeSlot {
    /// The client sends `EV_MODE_HINT` (`CAP_MODE_HINTS`).
    hints: bool,
    /// A session client (`CAP_IDLE`, a stream host): while active, its
    /// request outranks a plain viewer's.
    session: bool,
    /// Frames are wanted. A session client starts idle.
    active: bool,
    /// The mode this client last asked for, resolved.
    want: Option<(u32, u32, u32)>,
    /// When it asked (arbiter ticks), for "the most recent request wins".
    at: u64,
}

impl ModeSlot {
    /// Before HELLO: a legacy client, active, asking nothing yet.
    const NEW: Self = Self {
        hints: false,
        session: false,
        active: true,
        want: None,
        at: 0,
    };
}

/// Decides the guest's display mode from what the display clients report, and
/// says only when it changes.
///
/// Per client: one with `CAP_MODE_HINTS` says what it wants (`EV_MODE_HINT`);
/// one without gets the old contract: an `EV_SURFACE` flagged fullscreen is
/// the output's mode, and a windowed one means "back to the configured mode".
///
/// Between clients (docs/SCANOUT.md, "Several display clients"):
///
/// * an active session client (a stream with a client attached) wins over
///   the others; among equals the most recent request wins;
/// * the other clients' requests are remembered, not applied, so they never
///   fight over the mode: a viewer scales the stream's picture meanwhile;
/// * a session client going idle withdraws its request: the remaining
///   clients' most recent request applies, or the configured mode if none
///   asked for anything;
/// * a client disconnecting withdraws its request too, but if nobody else
///   asked for anything the guest keeps its mode (as with one client).
///
/// Starts from the configured mode, which is what the guest booted with.
#[derive(Clone, Debug)]
pub struct ModeArbiter {
    configured: DisplayMode,
    slots: Vec<ModeSlot>,
    last: (u32, u32, u32),
    tick: u64,
}

impl ModeArbiter {
    pub fn new(configured: DisplayMode) -> Self {
        Self {
            configured,
            slots: Vec::new(),
            last: (
                configured.width,
                configured.height,
                configured.refresh_hz * 1000,
            ),
            tick: 0,
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

    fn slot(&mut self, i: usize) -> &mut ModeSlot {
        if self.slots.len() <= i {
            self.slots.resize(i + 1, ModeSlot::NEW);
        }
        &mut self.slots[i]
    }

    /// The mode the guest should switch to after client `i` sent this
    /// packet, if it changed.
    pub fn packet(&mut self, i: usize, p: &wire::Pkt) -> Option<DisplayModeEvent> {
        use wire::*;
        self.tick += 1;
        let tick = self.tick;
        let resolved = self.resolve(p.x, p.y, p.w0);
        let configured = self.configured();
        let s = self.slot(i);
        let mut withdrew = false;
        match p.ty {
            EV_HELLO => {
                let session = p.w1 & CAP_IDLE != 0;
                *s = ModeSlot {
                    hints: p.w1 & CAP_MODE_HINTS != 0,
                    session,
                    active: !session,
                    want: None,
                    at: 0,
                };
                return None;
            }
            EV_ACTIVE => {
                let on = p.x != 0;
                if s.active == on {
                    return None;
                }
                s.active = on;
                if on {
                    // Nothing asked yet; its hint follows.
                    return None;
                }
                withdrew = s.want.take().is_some();
                if !withdrew {
                    return None;
                }
            }
            EV_MODE_HINT if s.hints => {
                s.want = Some(resolved);
                s.at = tick;
            }
            EV_SURFACE if !s.hints && p.x > 0 && p.y > 0 => {
                s.want = Some(if p.flags & F_FULLSCREEN != 0 {
                    resolved
                } else {
                    configured
                });
                s.at = tick;
            }
            _ => return None,
        }
        self.decide(withdrew)
    }

    /// Client `i` went away.
    pub fn disconnect(&mut self, i: usize) -> Option<DisplayModeEvent> {
        let had = self.slots.get(i).is_some_and(|s| s.want.is_some());
        if i < self.slots.len() {
            self.slots[i] = ModeSlot::NEW;
        }
        if !had {
            return None;
        }
        self.decide(false)
    }

    /// The request that rules now. `restore`: with no request left, go back
    /// to the configured mode (rather than keep the current one).
    fn decide(&mut self, restore: bool) -> Option<DisplayModeEvent> {
        let best = self
            .slots
            .iter()
            .filter(|s| s.active)
            .filter_map(|s| s.want.map(|w| ((s.session, s.at), w)))
            .max_by_key(|(k, _)| *k)
            .map(|(_, w)| w);
        let want = match best {
            Some(w) => w,
            None if restore => self.configured(),
            None => return None,
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

/// The policy for a single display client: [`ModeArbiter`] with one slot.
#[derive(Clone, Debug)]
pub struct ModePolicy(ModeArbiter);

impl ModePolicy {
    pub fn new(configured: DisplayMode) -> Self {
        Self(ModeArbiter::new(configured))
    }

    /// The mode the guest should switch to after this packet, if it changed.
    pub fn packet(&mut self, p: &wire::Pkt) -> Option<DisplayModeEvent> {
        self.0.packet(0, p)
    }

    pub fn current(&self) -> (u32, u32, u32) {
        self.0.current()
    }
}

// ---------------------------------------------------------------------------
// Clipboard framing
// ---------------------------------------------------------------------------

/// Host -> guest: reassembles EV_CLIPBOARD chunks into one transfer. The cap
/// is a size we keep ourselves; a transfer over it, or a malformed chunk, is
/// abandoned up to its LAST and the next one starts clean.
pub struct ClipAssembler {
    buf: Vec<u8>,
    cap: usize,
    bad: bool,
}

impl Default for ClipAssembler {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            cap: wire::CLIP_LEGACY_MAX,
            bad: false,
        }
    }
}

impl ClipAssembler {
    /// The cap for transfers from now on (from the broker's HELLO).
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Forget any partial transfer (a new connection).
    pub fn reset(&mut self) {
        self.buf = Vec::new();
        self.bad = false;
    }

    /// Feed one packet. Returns the whole text when `p` completed a good,
    /// non-empty transfer. Non-clipboard packets are ignored.
    pub fn feed(&mut self, p: &wire::Pkt) -> Option<Vec<u8>> {
        if p.ty != wire::EV_CLIPBOARD {
            return None;
        }
        let (last, data) = match p.clip_payload() {
            Some(v) => v,
            None => {
                if !self.bad {
                    log::warn!(
                        "display: malformed clipboard chunk from the broker; transfer dropped"
                    );
                }
                self.bad = true;
                // No LAST can be trusted from a malformed chunk; the info
                // byte still says whether it was meant as the end.
                (p.encode()[8] & wire::CLIP_LAST != 0, Vec::new())
            }
        };
        if !self.bad {
            if self.buf.len() + data.len() > self.cap {
                log::warn!(
                    "display: host clipboard is over the {}-byte cap; not sent to the guest",
                    self.cap
                );
                self.bad = true;
                self.buf = Vec::new();
            } else {
                self.buf.extend_from_slice(&data);
            }
        }
        if !last {
            return None;
        }
        let bad = std::mem::take(&mut self.bad);
        let text = std::mem::take(&mut self.buf);
        (!bad && !text.is_empty()).then_some(text)
    }
}

/// Guest -> host: one transfer being sent as CMD_CLIPBOARD records.
#[derive(Debug, Default)]
pub struct ClipOut {
    text: Vec<u8>,
    next: u32,
}

impl ClipOut {
    pub fn new(text: Vec<u8>) -> Self {
        Self { text, next: 0 }
    }

    /// Total records the transfer takes.
    pub fn chunks(&self) -> u32 {
        self.text.len().div_ceil(wire::CLIP_CMD_BYTES).max(1) as u32
    }

    pub fn done(&self) -> bool {
        self.next >= self.chunks()
    }

    /// Up to `max` next records, concatenated, and how many they are. Nothing
    /// is consumed until [`ClipOut::commit`].
    pub fn records(&self, max: u32) -> (Vec<u8>, u32) {
        let total = self.chunks();
        let end = total.min(self.next.saturating_add(max));
        let mut out = Vec::with_capacity((end - self.next) as usize * wire::CMD_SIZE);
        for i in self.next..end {
            let at = i as usize * wire::CLIP_CMD_BYTES;
            let stop = (at + wire::CLIP_CMD_BYTES).min(self.text.len());
            out.extend_from_slice(&wire::clip_cmd(i, &self.text[at..stop], i + 1 == total));
        }
        (out, end - self.next)
    }

    pub fn commit(&mut self, n: u32) {
        self.next += n;
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

    /// Deliver host clipboard transfer `generation` (`data`, the whole text)
    /// from byte `offset` on, as far as posted buffers allow. Returns the new
    /// offset; `data.len()` when done (or deliberately dropped). The rest is
    /// retried shortly; a newer transfer replaces this one.
    fn clipboard(&mut self, generation: u64, data: &[u8], offset: usize) -> usize {
        let _ = (generation, offset);
        data.len()
    }

    /// Whether the guest takes Conduit input at all ([`guest_takes_input`]):
    /// its driver says it consumes `InputEvent`s (it acked
    /// `NVGPU_CFG_TAKES_INPUT`, or it is a Linux guest module from before the
    /// bit) and has its event queue up with buffers posted. A guest that
    /// does not (Windows: the Helios KMD never acks the bit, whether or not
    /// it runs the event queue) has no use for `InputEvent`s; with a boot
    /// console attached, its input goes to the VM's emulated keyboard and
    /// tablet instead, whoever shows the picture. Asked by the link thread
    /// at every turn; must be cheap and must not block.
    fn takes_input(&mut self) -> bool {
        true
    }
}

/// Where `ScanoutReleased` events go: the event queue, in the vhost-user
/// binary. Called from the thread serving guest requests (a flip that
/// replaced a buffer no client holds) and from the link thread (a client's
/// release); must not block and must not call back into the link.
pub trait ReleaseSink: Send + Sync {
    /// Deliver as many of `r` as there are event buffers posted, in order.
    /// Returns how many were consumed; the rest are retried shortly.
    fn released(&self, r: &[ScanoutReleased]) -> usize;
}

/// What the guest's driver has said about taking Conduit input since the
/// device last started, shared between the request path (which learns it)
/// and the input sink (which asks, [`InputSink::takes_input`]). Cleared on
/// every device start or reset: the next driver says it again.
#[derive(Debug, Default)]
pub struct GuestInputClaims {
    /// The guest acked the virtio feature `NVGPU_CFG_TAKES_INPUT`.
    acked: AtomicBool,
    /// The guest asked for `GetSysFiles` or `GetProcFiles` before anything
    /// else that identifies a driver: the Linux guest module does, at probe,
    /// in every version, so this is a Linux guest even when its module
    /// predates the feature bit.
    linux: AtomicBool,
    /// An `Open`, `Ioctl`, `ScanoutFlip` or `GpuCmd` came first. The Windows
    /// KMD has always sent scanout or Venus traffic by the time an
    /// application's NVK forwards its own `GetSysFiles` (librmclient asks for
    /// it at `crm_open`), so a later one says nothing about the driver.
    other_first: AtomicBool,
}

impl GuestInputClaims {
    /// The device (re)started with these acked driver features. Whatever an
    /// earlier driver said is gone with it.
    pub fn device_started(&self, acked_features: u64) {
        let bit = u64::from(protocol::messages::NVGPU_CFG_TAKES_INPUT);
        self.acked
            .store(acked_features & bit != 0, Ordering::Release);
        self.linux.store(false, Ordering::Release);
        self.other_first.store(false, Ordering::Release);
    }

    /// The device was reset: nothing is known about the next driver.
    pub fn reset(&self) {
        self.device_started(0);
    }

    /// A request of this `MsgType` value was served.
    #[inline]
    pub fn saw_request(&self, msg_type: u32) {
        use protocol::messages::MsgType;
        if self.linux.load(Ordering::Relaxed) || self.other_first.load(Ordering::Relaxed) {
            return;
        }
        if msg_type == MsgType::GetSysFiles as u32 || msg_type == MsgType::GetProcFiles as u32 {
            self.linux.store(true, Ordering::Release);
        } else if msg_type == MsgType::Open as u32
            || msg_type == MsgType::Ioctl as u32
            || msg_type == MsgType::ScanoutFlip as u32
            || msg_type == MsgType::GpuCmd as u32
        {
            self.other_first.store(true, Ordering::Release);
        }
    }

    /// The guest acked `NVGPU_CFG_TAKES_INPUT`.
    pub fn acked(&self) -> bool {
        self.acked.load(Ordering::Acquire)
    }

    /// The guest identified itself as the Linux guest module.
    pub fn linux(&self) -> bool {
        self.linux.load(Ordering::Acquire)
    }

    /// [`guest_takes_input`] with what is known so far.
    pub fn takes_input(&self, event_queue_live: bool) -> bool {
        guest_takes_input(event_queue_live, self.acked(), self.linux())
    }
}

/// The routing rule (docs/SCANOUT.md "Input"): the guest takes Conduit input
/// when its event queue is live (started, buffers posted) **and** it says it
/// consumes `InputEvent`s -- it acked `NVGPU_CFG_TAKES_INPUT`, or, for a
/// Linux guest module from before that bit, it is the Linux module
/// (`linux_guest`). A queue alone is not enough: the Windows KMD runs one to
/// receive `EventReady`, never acks the bit, never asks for `GetSysFiles`
/// before its own scanout or Venus traffic (NVK on RM forwards one later,
/// from an application), and its keyboard and mouse are QEMU's emulated
/// devices.
pub fn guest_takes_input(event_queue_live: bool, acked_bit: bool, linux_guest: bool) -> bool {
    event_queue_live && (acked_bit || linux_guest)
}

/// The boot console (`crate::console`): where input goes while it is shown,
/// and how it is told that the mode may have changed.
pub trait ConsoleSink: Send + Sync {
    /// Input for the console, already translated to Linux events (the same
    /// triples the guest would have had). Must not block.
    fn input(&self, events: &[InputEventEntry]);
    /// The console mode changed or may have: look at
    /// [`DisplayLink::console_poll`]. Must not block.
    fn wake(&self);
}

/// Who owns the picture when a console is attached
/// ([`DisplayLink::attach_console`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleMode {
    /// The guest's driver is showing frames; the console is quiet.
    Guest,
    /// The guest turned its scanout off; the console takes over at this
    /// instant unless the guest flips first (a mode set disables and
    /// re-enables, and should not flash the firmware screen).
    Pending(Instant),
    /// The console is shown and gets the input.
    Shown,
}

/// How long a disabled scanout waits before the console takes over.
pub const CONSOLE_GRACE: Duration = Duration::from_millis(250);

/// Plane 0 of a frame handed over as a dma-buf (see
/// [`DisplayLink::flip_dmabuf`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameGeometry {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    /// `DRM_FORMAT_*`.
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*`.
    pub modifier: u64,
}

/// What became of one flip. None of these is an error to the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipOutcome {
    Sent,
    /// The socket was full: this frame dropped, latest wins.
    Busy,
    /// No broker connected (or none wants frames): dropped.
    NoBroker,
    /// The connection broke on this send; dropped, reconnect pending.
    Broken,
    /// The broker cannot take this (no `CAP_CURSOR`); kept for a broker that
    /// can, and not sent.
    Unsupported,
}

impl FlipOutcome {
    /// Several clients: the best thing that happened to any of them.
    fn merge(self, o: FlipOutcome) -> FlipOutcome {
        use FlipOutcome::*;
        let rank = |x: FlipOutcome| match x {
            Sent => 4,
            Busy => 3,
            Broken => 2,
            Unsupported => 1,
            NoBroker => 0,
        };
        if rank(o) > rank(self) { o } else { self }
    }
}

/// The last cursor the guest showed, kept so a broker that connects later --
/// or a send that found the socket full -- still gets it.
struct SentCursor {
    /// A dup of the exported dma-buf; `None` when the cursor is hidden.
    fd: Option<Arc<OwnedFd>>,
    c: CursorUpdate,
}

/// A buffer the guest showed while no client wanted it: not exported, only
/// remembered by its GEM identity, and exported when a client becomes active.
struct Parked<T> {
    /// A dup of the owning drm file's host descriptor, so the handle still
    /// means the same object when the export happens on the link thread.
    drm: Arc<OwnedFd>,
    owner: u32,
    host_handle: u32,
    what: T,
}

/// Exports `host_handle` on a drm descriptor; the real one is PRIME.
pub type Exporter = dyn Fn(RawFd, u32) -> Result<OwnedFd, i32> + Send + Sync;

/// One display client: a socket path the backend connects to, and what this
/// connection has been told.
#[derive(Default)]
struct Client {
    path: Option<PathBuf>,
    sock: Option<Arc<OwnedFd>>,
    /// Size last asked of the broker with WINDOW, per connection.
    last_size: Option<(u32, u32)>,
    /// Formats already asked about on this connection.
    queried: HashSet<(u32, u64)>,
    /// HELLO's capability bits on this connection (0 until it arrives).
    broker_caps: u32,
    /// HELLO arrived on this connection (CAPS sent, caps known).
    hello: bool,
    /// The client wants frames: from connect on, unless it said `CAP_IDLE`
    /// and has not said `EV_ACTIVE` yet.
    active: bool,
    /// The guest's cursor is owed to this connection.
    cursor_dirty: bool,
    /// The newest frame did not fit in the socket: re-sent when it drains.
    frame_owed: bool,
    /// The guest's clipboard on its way to this client.
    clip_out: Option<ClipOut>,
    /// When the last batch of it went, for pacing.
    clip_sent_at: Option<Instant>,
}

impl Client {
    fn wants_frames(&self) -> bool {
        self.sock.is_some() && self.active
    }

    /// Forget the connection, keep the path.
    fn reset(&mut self, sock: Option<Arc<OwnedFd>>) {
        *self = Client {
            path: self.path.take(),
            active: sock.is_some(),
            sock,
            ..Default::default()
        };
    }
}

#[derive(Default)]
struct LinkState {
    clients: Vec<Client>,
    /// The guest's cursor, kept across connections.
    cursor: Option<SentCursor>,
    /// The last frame the guest flipped (a dup of its dma-buf), kept across
    /// connections so a viewer that (re)attaches shows it at once instead of
    /// a black window until the guest's next flip.
    /// The third field is the ATTACH flags ([`wire::CMD_F_SHM`] for the
    /// console's shared-memory frames).
    frame: Option<(Arc<OwnedFd>, ScanoutFlip, u16)>,
    /// Which guest buffer `frame` is (`None` for the console's).
    frame_key: Option<BufKey>,
    /// Which buffers clients still read (docs/SCANOUT.md "Buffer release").
    release: ReleaseTracker,
    /// The last frame / cursor while nobody wanted them (see [`Parked`]).
    parked_frame: Option<Parked<ScanoutFlip>>,
    parked_cursor: Option<Parked<CursorUpdate>>,
    /// The boot console, when one is attached, and who owns the picture.
    console: Option<Arc<dyn ConsoleSink>>,
    console_mode: Option<ConsoleMode>,
    /// The size of the guest's last frame: what the viewer shows while the
    /// guest owns the picture.
    guest_size: Option<(u32, u32)>,
}

/// Guest -> broker clipboard pacing: at most this many records per
/// [`CLIP_BATCH_EVERY`] (32k records/s, under the broker's limit).
const CLIP_BATCH: u32 = 128;
const CLIP_BATCH_EVERY: Duration = Duration::from_millis(4);

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
    pub clip_to_guest: AtomicU64,
    pub clip_to_host: AtomicU64,
    /// Frames re-sent to a client whose socket had been full.
    pub resent: AtomicU64,
    /// `ScanoutReleased` events delivered to the guest.
    pub released: AtomicU64,
}

/// The backend's connections to its display clients (brokers): the local
/// viewer, a stream host, or both. Shared between the thread serving guest
/// RPCs (which sends) and the link's own thread (which connects, reads input,
/// and reconnects).
///
/// Every client gets every frame through its own non-blocking socket: one
/// that is slow, stuck or gone costs only its own frames (latest frame wins,
/// per client). A client that wants no frames (none connected, or a stream
/// host with no session) costs nothing at all: the guest's flip is not even
/// exported (see [`DisplayLink::wants_frames`]).
pub struct DisplayLink {
    /// The mode the guest boots with (device config); what "restore" means.
    configured: DisplayMode,
    state: Mutex<LinkState>,
    /// Clients that want frames now; read without the lock on every flip.
    active: AtomicUsize,
    /// A frame or cursor is parked (lock-free check for GEM closes).
    parked: AtomicBool,
    /// The guest asked for the host clipboard again (ClipboardRequest).
    clip_resend: AtomicBool,
    exporter: Mutex<Arc<Exporter>>,
    /// The console is shown (input goes to it); read without the lock for
    /// every input packet.
    console_shown: AtomicBool,
    /// A console is attached ([`DisplayLink::attach_console`]).
    console_attached: AtomicBool,
    /// The guest wants `ScanoutReleased` (lock-free check on every flip).
    release_on: AtomicBool,
    release_sink: Mutex<Option<Arc<dyn ReleaseSink>>>,
    pub stats: LinkStats,
}

/// The guest's name for a `ScanoutFlip` buffer.
fn gem_key(f: &ScanoutFlip) -> BufKey {
    BufKey::Gem {
        owner: f.owner_handle,
        handle: f.host_handle,
    }
}

/// A dma-buf's inode: its identity to a display client (`EV_RELEASE`).
fn fd_inode(fd: RawFd) -> u64 {
    // SAFETY: fstat into a zeroed struct on a descriptor live for the call.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return 0;
    }
    st.st_ino
}

fn real_export(drm: RawFd, host_handle: u32) -> Result<OwnedFd, i32> {
    prime_export(drm, host_handle, |fd, req, arg| {
        // SAFETY: `arg` is a live 12-byte drm_prime_handle for the call.
        let rc = unsafe { libc::ioctl(fd, req as libc::Ioctl, arg.as_mut_ptr()) };
        if rc < 0 {
            Err(io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        } else {
            Ok(())
        }
    })
}

impl DisplayLink {
    /// A link to the broker at `path`; `None` acks and drops every flip.
    pub fn new(path: Option<PathBuf>) -> Arc<Self> {
        Self::with_mode(path, DisplayMode::DEFAULT)
    }

    /// A link whose "configured mode" (restore target) is `mode`.
    pub fn with_mode(path: Option<PathBuf>, mode: DisplayMode) -> Arc<Self> {
        Self::with_paths(path.into_iter().collect(), mode)
    }

    /// A link to every broker in `paths`, each connected and reconnected on
    /// its own.
    pub fn with_paths(paths: Vec<PathBuf>, mode: DisplayMode) -> Arc<Self> {
        let mut clients: Vec<Client> = paths
            .into_iter()
            .map(|p| Client {
                path: Some(p),
                ..Default::default()
            })
            .collect();
        if clients.is_empty() {
            clients.push(Client::default());
        }
        Arc::new(Self {
            configured: mode,
            state: Mutex::new(LinkState {
                clients,
                ..Default::default()
            }),
            active: AtomicUsize::new(0),
            parked: AtomicBool::new(false),
            clip_resend: AtomicBool::new(false),
            exporter: Mutex::new(Arc::new(real_export)),
            console_shown: AtomicBool::new(false),
            console_attached: AtomicBool::new(false),
            release_on: AtomicBool::new(false),
            release_sink: Mutex::new(None),
            stats: LinkStats::default(),
        })
    }

    /// Replace the PRIME export used for parked buffers (tests).
    pub fn set_exporter(&self, f: Arc<Exporter>) {
        *self.exporter.lock().unwrap() = f;
    }

    /// Where `ScanoutReleased` events go.
    pub fn set_release_sink(&self, sink: Arc<dyn ReleaseSink>) {
        *self.release_sink.lock().unwrap() = Some(sink);
    }

    /// The guest acked `NVGPU_F_SCANOUT_RELEASE` at device start (`true`), or
    /// the device started or reset without it (`false`, which also forgets
    /// every tracked buffer).
    pub fn set_release_enabled(&self, on: bool) {
        let mut st = self.state.lock().unwrap();
        st.release.set_enabled(on);
        self.release_on.store(on, Ordering::Release);
    }

    /// The guest wants `ScanoutReleased` events.
    #[inline]
    pub fn release_enabled(&self) -> bool {
        self.release_on.load(Ordering::Acquire)
    }

    /// The guest flipped `key` (`None`: the console): note it, and queue
    /// whatever that frees.
    fn flipped_locked(&self, st: &mut LinkState, key: Option<BufKey>, seq: u64) {
        if !self.release_enabled() {
            return;
        }
        let now = Instant::now();
        st.release.flipped(key, seq, now);
        st.release.collect(now);
    }

    /// Hand queued releases to the sink. Called without the state lock.
    fn deliver_releases(&self) {
        if !self.release_enabled() {
            return;
        }
        let batch = {
            let mut st = self.state.lock().unwrap();
            if !st.release.pending() {
                return;
            }
            st.release.take()
        };
        let sink = self.release_sink.lock().unwrap().clone();
        let n = match sink {
            Some(s) => s.released(&batch).min(batch.len()),
            // Nobody to tell: dropped.
            None => batch.len(),
        };
        self.stats.released.fetch_add(n as u64, Ordering::Relaxed);
        if n < batch.len() {
            self.state
                .lock()
                .unwrap()
                .release
                .untake(batch[n..].to_vec());
        }
    }

    /// The link thread: forced releases that came due, and releases a guest
    /// with no event buffer posted could not take before. Returns when to
    /// look again, if anything waits.
    fn tick_releases(&self) -> Option<Duration> {
        if !self.release_enabled() {
            return None;
        }
        {
            let mut st = self.state.lock().unwrap();
            st.release.collect(Instant::now());
        }
        self.deliver_releases();
        let st = self.state.lock().unwrap();
        if st.release.pending() {
            return Some(Duration::from_millis(2));
        }
        st.release
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Client `i` released the buffer with inode `inode` up to its ATTACH
    /// stamped `stamp` (`EV_RELEASE` from a `CAP_RELEASE_SEQ` client).
    fn client_released(&self, i: usize, inode: u64, stamp: u32) {
        if !self.release_enabled() {
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            if st
                .clients
                .get(i)
                .is_none_or(|c| c.broker_caps & wire::CAP_RELEASE_SEQ == 0)
            {
                return;
            }
            st.release.released(i, inode, stamp);
            st.release.collect(Instant::now());
        }
        self.deliver_releases();
    }

    /// Client `i` reads nothing any more (gone, or idle).
    fn client_gone_locked(&self, st: &mut LinkState, i: usize) {
        if st.release.enabled() {
            st.release.client_gone(i);
            st.release.collect(Instant::now());
        }
    }

    /// The guest destroyed Venus resource `id`: it is no longer tracked.
    pub fn forget_resource(&self, id: u32) {
        if !self.release_enabled() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        st.release.forget(|k| k == BufKey::Resource(id));
        if st.frame_key == Some(BufKey::Resource(id)) {
            st.frame_key = None;
        }
    }

    /// The guest wants the current host clipboard (again): its driver just
    /// came up, so anything sent earlier -- typically the viewer's push on
    /// connect, which races the guest's boot -- never reached it. The link
    /// thread re-sends the newest host clipboard it has, if any.
    pub fn request_host_clipboard(&self) {
        self.clip_resend.store(true, Ordering::Relaxed);
    }

    /// The first client's path.
    pub fn path(&self) -> Option<PathBuf> {
        self.paths().into_iter().next()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        let st = self.state.lock().unwrap();
        st.clients.iter().filter_map(|c| c.path.clone()).collect()
    }

    /// Any client connected.
    pub fn connected(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.clients.iter().any(|c| c.sock.is_some())
    }

    /// Some client wants frames. When not, the guest's flips and cursor
    /// changes should be [`DisplayLink::park`]ed instead of exported: one
    /// atomic load, the whole cost of a flip nobody looks at.
    #[inline]
    pub fn wants_frames(&self) -> bool {
        self.active.load(Ordering::Acquire) != 0
    }

    /// Something is parked (see [`DisplayLink::park`]).
    #[inline]
    pub fn has_parked(&self) -> bool {
        self.parked.load(Ordering::Relaxed)
    }

    fn note_parked(&self, st: &LinkState) {
        self.parked.store(
            st.parked_frame.is_some() || st.parked_cursor.is_some(),
            Ordering::Relaxed,
        );
    }

    fn recount(&self, st: &LinkState) {
        let n = st.clients.iter().filter(|c| c.wants_frames()).count();
        self.active.store(n, Ordering::Release);
    }

    /// Adopt an already-connected socket as the first client (tests, or a
    /// VMM that passes one).
    pub fn adopt(&self, sock: OwnedFd) {
        self.adopt_at(0, sock);
    }

    /// Adopt a connected socket as client `i`.
    pub fn adopt_at(&self, i: usize, sock: OwnedFd) {
        set_nonblocking(sock.as_raw_fd());
        let mut st = self.state.lock().unwrap();
        if st.clients.len() <= i {
            st.clients.resize_with(i + 1, Client::default);
        }
        if let Some(old) = st.clients[i].sock.as_ref() {
            // SAFETY: shutdown on a live descriptor; wakes the reader's poll.
            unsafe { libc::shutdown(old.as_raw_fd(), libc::SHUT_RDWR) };
        }
        // The guest's cursor and last frame outlive a connection: the next
        // broker gets them once it has said HELLO.
        st.clients[i].reset(Some(Arc::new(sock)));
        self.client_gone_locked(&mut st, i);
        self.recount(&st);
        drop(st);
        self.deliver_releases();
        self.stats.connects.fetch_add(1, Ordering::Relaxed);
    }

    fn sockets(&self) -> Vec<Option<Arc<OwnedFd>>> {
        let st = self.state.lock().unwrap();
        st.clients.iter().map(|c| c.sock.clone()).collect()
    }

    /// Drop client `i`'s connection if `sock` is still it.
    fn drop_conn(&self, i: usize, sock: &Arc<OwnedFd>) {
        let mut st = self.state.lock().unwrap();
        self.drop_conn_locked(&mut st, i, sock);
        drop(st);
        self.deliver_releases();
    }

    fn drop_conn_locked(&self, st: &mut LinkState, i: usize, sock: &Arc<OwnedFd>) {
        let Some(c) = st.clients.get_mut(i) else {
            return;
        };
        if c.sock.as_ref().is_some_and(|s| Arc::ptr_eq(s, sock)) {
            // SAFETY: shutdown on a live descriptor; wakes the reader's poll.
            unsafe { libc::shutdown(sock.as_raw_fd(), libc::SHUT_RDWR) };
            c.reset(None);
            self.client_gone_locked(st, i);
            self.recount(st);
        }
    }

    /// The guest flipped while nobody wants frames: remember the buffer by
    /// its GEM identity without exporting it. `drm` is the owning file's host
    /// descriptor (dup'd only when the owner changes). Returns `false` when a
    /// client became active meanwhile: then export and [`DisplayLink::flip`].
    pub fn park(&self, drm: RawFd, f: &ScanoutFlip) -> bool {
        let mut st = self.state.lock().unwrap();
        self.guest_shows_locked(&mut st, f);
        if self.wants_frames() {
            return false;
        }
        self.flipped_locked(&mut st, Some(gem_key(f)), f.seq);
        if let Some(drm) = Self::drm_for(&st, drm, f.owner_handle) {
            st.frame = None;
            st.frame_key = None;
            st.parked_frame = Some(Parked {
                drm,
                owner: f.owner_handle,
                host_handle: f.host_handle,
                what: *f,
            });
            self.note_parked(&st);
            self.stats.no_broker.fetch_add(1, Ordering::Relaxed);
        }
        drop(st);
        self.deliver_releases();
        true
    }

    /// As [`DisplayLink::park`], for a visible cursor.
    pub fn park_cursor(&self, drm: RawFd, c: &CursorUpdate) -> bool {
        let mut st = self.state.lock().unwrap();
        if self.wants_frames() {
            return false;
        }
        let Some(drm) = Self::drm_for(&st, drm, c.owner_handle) else {
            return true;
        };
        st.cursor = None;
        st.parked_cursor = Some(Parked {
            drm,
            owner: c.owner_handle,
            host_handle: c.host_handle,
            what: *c,
        });
        self.note_parked(&st);
        true
    }

    /// A descriptor for `owner`'s drm file: one already parked, or a dup.
    fn drm_for(st: &LinkState, drm: RawFd, owner: u32) -> Option<Arc<OwnedFd>> {
        let have = [
            st.parked_frame.as_ref().map(|p| (p.owner, &p.drm)),
            st.parked_cursor.as_ref().map(|p| (p.owner, &p.drm)),
        ];
        if let Some((_, d)) = have.into_iter().flatten().find(|(o, _)| *o == owner) {
            return Some(d.clone());
        }
        // SAFETY: dup of a descriptor the caller keeps open for this call.
        let kept = unsafe { libc::fcntl(drm, libc::F_DUPFD_CLOEXEC, 0) };
        if kept < 0 {
            log::warn!(
                "display: dup of drm file {owner}: {}",
                io::Error::last_os_error()
            );
            return None;
        }
        // SAFETY: a fresh descriptor owned by nobody else.
        Some(Arc::new(unsafe { OwnedFd::from_raw_fd(kept) }))
    }

    /// The guest closed GEM handle `host_handle` of `owner`: a parked buffer
    /// naming it is gone (the number may be reissued).
    pub fn forget(&self, owner: u32, host_handle: u32) {
        let mut st = self.state.lock().unwrap();
        if st
            .parked_frame
            .as_ref()
            .is_some_and(|p| p.owner == owner && p.host_handle == host_handle)
        {
            st.parked_frame = None;
        }
        if st
            .parked_cursor
            .as_ref()
            .is_some_and(|p| p.owner == owner && p.host_handle == host_handle)
        {
            st.parked_cursor = None;
        }
        self.note_parked(&st);
        if st.release.enabled() {
            let gone = BufKey::Gem {
                owner,
                handle: host_handle,
            };
            st.release.forget(|k| k == gone);
            if st.frame_key == Some(gone) {
                st.frame_key = None;
            }
        }
    }

    /// The guest closed drm file `owner`: drop parked buffers (and the
    /// descriptor dup that would keep the file alive).
    pub fn forget_owner(&self, owner: u32) {
        let mut st = self.state.lock().unwrap();
        if st.parked_frame.as_ref().is_some_and(|p| p.owner == owner) {
            st.parked_frame = None;
        }
        if st.parked_cursor.as_ref().is_some_and(|p| p.owner == owner) {
            st.parked_cursor = None;
        }
        self.note_parked(&st);
        if st.release.enabled() {
            let gone = |k: BufKey| matches!(k, BufKey::Gem { owner: o, .. } if o == owner);
            st.release.forget(gone);
            if st.frame_key.is_some_and(gone) {
                st.frame_key = None;
            }
        }
    }

    /// A client became active: export what was parked while nobody looked.
    fn unpark_locked(&self, st: &mut LinkState) {
        if st.parked_frame.is_none() && st.parked_cursor.is_none() {
            return;
        }
        let export = self.exporter.lock().unwrap().clone();
        if let Some(p) = st.parked_frame.take() {
            match export(p.drm.as_raw_fd(), p.host_handle) {
                Ok(fd) => {
                    st.frame = Some((Arc::new(fd), p.what, 0));
                    st.frame_key = Some(gem_key(&p.what));
                }
                Err(e) => log::warn!(
                    "display: export of the parked frame (handle {} on file {}): errno {e}",
                    p.host_handle,
                    p.owner
                ),
            }
        }
        if let Some(p) = st.parked_cursor.take() {
            match export(p.drm.as_raw_fd(), p.host_handle) {
                Ok(fd) => {
                    st.cursor = Some(SentCursor {
                        fd: Some(Arc::new(fd)),
                        c: p.what,
                    })
                }
                Err(e) => log::warn!(
                    "display: export of the parked cursor (handle {} on file {}): errno {e}",
                    p.host_handle,
                    p.owner
                ),
            }
        }
        self.note_parked(st);
    }

    /// Present `dmabuf` as described by `f` to every client that wants
    /// frames. Never blocks.
    pub fn flip(&self, dmabuf: RawFd, f: &ScanoutFlip) -> FlipOutcome {
        self.flip_keyed(dmabuf, f, Some(gem_key(f)))
    }

    fn flip_keyed(&self, dmabuf: RawFd, f: &ScanoutFlip, key: Option<BufKey>) -> FlipOutcome {
        let mut st = self.state.lock().unwrap();
        self.guest_shows_locked(&mut st, f);
        self.flipped_locked(&mut st, key, f.seq);
        let out = if !self.wants_frames() {
            self.stats.no_broker.fetch_add(1, Ordering::Relaxed);
            FlipOutcome::NoBroker
        } else {
            self.present_locked(&mut st, dmabuf, f, 0, key)
        };
        drop(st);
        self.deliver_releases();
        out
    }

    /// Keep `fd` as the current frame and send it to every client that
    /// wants frames.
    fn present_locked(
        &self,
        st: &mut LinkState,
        fd: RawFd,
        f: &ScanoutFlip,
        flags: u16,
        key: Option<BufKey>,
    ) -> FlipOutcome {
        st.frame = Self::keep(fd).map(|k| (k, *f, flags));
        st.frame_key = key;
        if st.parked_frame.take().is_some() {
            self.note_parked(st);
        }
        let mut out = FlipOutcome::NoBroker;
        for i in 0..st.clients.len() {
            if st.clients[i].wants_frames() {
                out = out.merge(self.send_frame_locked(st, i, fd, f, flags));
            }
        }
        out
    }

    /// A dup of `fd`, kept as the frame a client that attaches later is shown.
    fn keep(fd: RawFd) -> Option<Arc<OwnedFd>> {
        // SAFETY: dup of a descriptor the caller keeps open for this call.
        let kept = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        // SAFETY: `kept` is a fresh descriptor owned by nobody else.
        (kept >= 0).then(|| Arc::new(unsafe { OwnedFd::from_raw_fd(kept) }))
    }

    /// Present a frame that is already a dma-buf, with no GEM object behind
    /// it: Venus (docs/VENUS.md), where the renderer exports the scanout
    /// image. The same send path as [`DisplayLink::flip`]; the difference is
    /// what happens with nobody looking. A GEM buffer is parked unexported,
    /// but this one is exported already (once per resource), so it is kept
    /// as the frame a client that attaches later is shown first.
    ///
    /// `resource` is the Venus resource shown, which `ScanoutReleased` names.
    pub fn flip_dmabuf(
        &self,
        dmabuf: RawFd,
        g: &FrameGeometry,
        resource: Option<u32>,
    ) -> FlipOutcome {
        let key = resource.map(BufKey::Resource);
        let f = ScanoutFlip {
            width: g.width,
            height: g.height,
            stride: g.stride,
            offset: g.offset,
            fourcc: g.fourcc,
            modifier: g.modifier,
            ..Default::default()
        };
        if !self.wants_frames() {
            let mut st = self.state.lock().unwrap();
            self.guest_shows_locked(&mut st, &f);
            if !self.wants_frames() {
                self.flipped_locked(&mut st, key, 0);
                st.frame = Self::keep(dmabuf).map(|k| (k, f, 0));
                st.frame_key = key;
                if st.parked_frame.take().is_some() {
                    self.note_parked(&st);
                }
                self.stats.no_broker.fetch_add(1, Ordering::Relaxed);
                drop(st);
                self.deliver_releases();
                return FlipOutcome::NoBroker;
            }
        }
        self.flip_keyed(dmabuf, &f, key)
    }

    /// One frame to client `i`. A full socket owes it the newest frame,
    /// which the link thread sends when the socket drains.
    fn send_frame_locked(
        &self,
        st: &mut LinkState,
        i: usize,
        dmabuf: RawFd,
        f: &ScanoutFlip,
        flags: u16,
    ) -> FlipOutcome {
        let c = &mut st.clients[i];
        let Some(sock) = c.sock.clone() else {
            return FlipOutcome::NoBroker;
        };
        let fd = sock.as_raw_fd();

        // Control records first, in their own sendmsg: the fd must ride on
        // the first byte of the ATTACH record, and SCM_RIGHTS attaches to the
        // first byte of whatever one sendmsg carries.
        let mut ctl: Vec<u8> = Vec::new();
        if c.last_size != Some((f.width, f.height)) {
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
        // Shared memory is not a dma-buf format question: the broker checks
        // it against its shm formats, not the display's modifiers.
        let ask = flags & wire::CMD_F_SHM == 0 && !c.queried.contains(&fmt);
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
                    c.last_size = Some((f.width, f.height));
                    if ask {
                        c.queried.insert(fmt);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    c.frame_owed = true;
                    self.stats.busy.fetch_add(1, Ordering::Relaxed);
                    return FlipOutcome::Busy;
                }
                Err(e) => {
                    log::warn!("display: send to client {i} failed: {e}; reconnecting");
                    self.drop_conn_locked(st, i, &sock);
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
                flags,
                width: f.width,
                height: f.height,
                stride: f.stride,
                offset: f.offset,
                fourcc: f.fourcc,
                modifier: f.modifier,
                seq: stamp,
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
                c.frame_owed = false;
                let reports = c.broker_caps & wire::CAP_RELEASE_SEQ != 0;
                if st.release.enabled() {
                    let key = st.frame_key;
                    st.release.sent(i, key, fd_inode(dmabuf), stamp, reports);
                }
                self.stats.sent.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Sent
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                c.frame_owed = true;
                self.stats.busy.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Busy
            }
            Err(e) => {
                log::warn!("display: send to client {i} failed: {e}; reconnecting");
                self.drop_conn_locked(st, i, &sock);
                self.stats.broken.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Broken
            }
        }
    }

    /// Send the kept frame to client `i` (a new or drained connection).
    fn resend_frame_locked(&self, st: &mut LinkState, i: usize) -> Option<FlipOutcome> {
        if !st.clients.get(i).is_some_and(Client::wants_frames) {
            return None;
        }
        let (fd, f, flags) = st.frame.clone()?;
        Some(self.send_frame_locked(st, i, fd.as_raw_fd(), &f, flags))
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
        if st.parked_cursor.take().is_some() {
            self.note_parked(&st);
        }
        let mut out = FlipOutcome::NoBroker;
        for i in 0..st.clients.len() {
            st.clients[i].cursor_dirty = true;
            if st.clients[i].sock.is_some() {
                out = out.merge(self.send_cursor_locked(&mut st, i));
            }
        }
        out
    }

    /// Send the kept cursor to client `i` if it still needs it.
    fn send_cursor_locked(&self, st: &mut LinkState, i: usize) -> FlipOutcome {
        let LinkState {
            clients, cursor, ..
        } = st;
        let cl = &mut clients[i];
        let Some(sock) = cl.sock.clone() else {
            return FlipOutcome::NoBroker;
        };
        if !cl.cursor_dirty {
            return FlipOutcome::Sent;
        }
        if cl.broker_caps & wire::CAP_CURSOR == 0 {
            // Before HELLO the caps are unknown; HELLO re-arms the send.
            return FlipOutcome::Unsupported;
        }
        if !cl.active {
            // EV_ACTIVE re-arms it.
            return FlipOutcome::NoBroker;
        }
        let Some(cur) = cursor.as_ref() else {
            cl.cursor_dirty = false;
            return FlipOutcome::Sent;
        };
        // Over the boot console the guest's pointer image means nothing.
        let console = self.console_shown.load(Ordering::Relaxed);
        let (cmd, fd) = match &cur.fd {
            Some(fd) if cur.c.visible() && !console => (
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
                cl.cursor_dirty = false;
                self.stats.cursors.fetch_add(1, Ordering::Relaxed);
                FlipOutcome::Sent
            }
            // Stays dirty: the link thread retries within a poll period.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => FlipOutcome::Busy,
            Err(e) => {
                log::warn!("display: send to client {i} failed: {e}; reconnecting");
                // SAFETY: shutdown on a live descriptor; the link thread
                // notices and reconnects.
                unsafe { libc::shutdown(sock.as_raw_fd(), libc::SHUT_RDWR) };
                FlipOutcome::Broken
            }
        }
    }

    /// A cursor send is owed to some client (socket was full, or a new
    /// broker said HELLO).
    fn cursor_owed(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.clients
            .iter()
            .any(|c| c.cursor_dirty && c.wants_frames() && c.broker_caps & wire::CAP_CURSOR != 0)
    }

    fn retry_cursor(&self) {
        let mut st = self.state.lock().unwrap();
        for i in 0..st.clients.len() {
            if st.clients[i].sock.is_some() {
                let _ = self.send_cursor_locked(&mut st, i);
            }
        }
    }

    /// Client `i` said HELLO: remember what it takes, tell it our frames are
    /// timestamped, and owe it the guest's cursor and current frame (unless
    /// it starts idle).
    fn hello_at(&self, i: usize, caps: u32) {
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clients.get_mut(i) else {
            return;
        };
        c.broker_caps = caps;
        c.hello = true;
        c.active = caps & wire::CAP_IDLE == 0;
        c.cursor_dirty = true;
        if let Some(sock) = c.sock.clone() {
            let caps_cmd = wire::Cmd {
                ty: wire::CMD_CAPS,
                width: wire::CLIENT_SEQ_USEC
                    | wire::CLIENT_CLIPBOARD
                    | wire::CLIENT_CLIP_LARGE
                    | wire::CLIENT_GAMEPAD
                    | wire::CLIENT_IDLE,
                ..Default::default()
            };
            if let Err(e) = send_records(sock.as_raw_fd(), &caps_cmd.encode(), None) {
                log::debug!("display: CAPS not sent: {e}");
            }
        }
        self.recount(&st);
        if !st.clients[i].active {
            log::info!("display: client {i} is idle until it asks for frames");
            return;
        }
        self.became_active_locked(&mut st, i);
    }

    /// Client `i` now wants frames: export anything parked, then give it
    /// the current picture now, not at the guest's next flip (an idle
    /// desktop may not flip for a long time). Its cursor follows from the
    /// link thread.
    fn became_active_locked(&self, st: &mut LinkState, i: usize) {
        self.unpark_locked(st);
        st.clients[i].cursor_dirty = true;
        if let Some(r) = self.resend_frame_locked(st, i) {
            log::info!("display: current frame re-sent to client {i}: {r:?}");
        }
    }

    /// EV_ACTIVE from client `i`.
    fn set_active(&self, i: usize, on: bool) {
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clients.get_mut(i) else {
            return;
        };
        if c.sock.is_none() || c.active == on {
            return;
        }
        c.active = on;
        c.frame_owed = false;
        if !on {
            self.client_gone_locked(&mut st, i);
        }
        self.recount(&st);
        log::info!(
            "display: client {i} {}",
            if on { "wants frames" } else { "is idle" }
        );
        if on {
            self.became_active_locked(&mut st, i);
        }
        drop(st);
        self.deliver_releases();
    }

    /// The link thread: frames owed to clients whose sockets drained.
    fn retry_frames(&self, writable: &[usize]) {
        let mut st = self.state.lock().unwrap();
        for &i in writable {
            if st.clients.get(i).is_some_and(|c| c.frame_owed)
                && self.resend_frame_locked(&mut st, i) == Some(FlipOutcome::Sent)
            {
                self.stats.resent.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The guest copied `text` (validated UTF-8): send it to every connected
    /// client, paced, from the link thread. A newer copy replaces one still
    /// being sent (chunk 0 restarts a transfer in the broker's framing).
    /// Dropped for a client whose cap it is over, and when none is connected.
    pub fn clipboard_to_host(&self, text: Vec<u8>) {
        if text.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        let mut any = false;
        for i in 0..st.clients.len() {
            let c = &mut st.clients[i];
            if c.sock.is_none() {
                continue;
            }
            any = true;
            let cap = wire::clip_cap(c.broker_caps);
            if text.len() > cap {
                log::warn!(
                    "display: guest clipboard is {} bytes, over client {i}'s {cap}-byte cap; dropped",
                    text.len()
                );
                continue;
            }
            c.clip_out = Some(ClipOut::new(text.clone()));
            c.clip_sent_at = None;
            self.send_clip_locked(&mut st, i);
        }
        if !any {
            log::debug!(
                "display: guest clipboard ({} bytes) dropped: no viewer",
                text.len()
            );
        }
    }

    /// Send the next paced batch of the guest's clipboard to client `i`, if
    /// one is due.
    fn send_clip_locked(&self, st: &mut LinkState, i: usize) {
        let c = &mut st.clients[i];
        if !c.hello {
            return; // CAPS first; HELLO re-arms it.
        }
        let Some(sock) = c.sock.clone() else {
            c.clip_out = None;
            return;
        };
        if c.clip_sent_at
            .is_some_and(|t| t.elapsed() < CLIP_BATCH_EVERY)
        {
            return;
        }
        let Some(out) = c.clip_out.as_mut() else {
            return;
        };
        let (bytes, n) = out.records(CLIP_BATCH);
        match send_records(sock.as_raw_fd(), &bytes, None) {
            Ok(()) => {
                out.commit(n);
                c.clip_sent_at = Some(Instant::now());
                if out.done() {
                    let len = out.text.len();
                    c.clip_out = None;
                    self.stats.clip_to_host.fetch_add(1, Ordering::Relaxed);
                    log::info!("display: guest clipboard sent to client {i} ({len} bytes)");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::warn!("display: send to client {i} failed: {e}; reconnecting");
                c.clip_out = None;
                // SAFETY: shutdown on a live descriptor; the link thread
                // notices and reconnects.
                unsafe { libc::shutdown(sock.as_raw_fd(), libc::SHUT_RDWR) };
            }
        }
    }

    /// HELLO on the first client (tests).
    #[cfg(test)]
    pub(crate) fn hello(&self, caps: u32) {
        self.hello_at(0, caps);
    }

    #[cfg(test)]
    pub(crate) fn hello_for_test(&self, caps: u32) {
        self.hello(caps);
    }

    #[cfg(test)]
    pub(crate) fn retry_clip_for_test(&self) {
        self.retry_clip();
    }

    /// Guest clipboard still being sent to some client.
    fn clip_owed(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.clients.iter().any(|c| c.clip_out.is_some() && c.hello)
    }

    fn retry_clip(&self) {
        let mut st = self.state.lock().unwrap();
        for i in 0..st.clients.len() {
            if st.clients[i].clip_out.is_some() {
                self.send_clip_locked(&mut st, i);
            }
        }
    }

    /// The guest turned the scanout off. The broker protocol has no detach;
    /// the window keeps the last frame. Forget the requested size so the next
    /// enable asks again.
    ///
    /// With a boot console attached, the console takes over after
    /// [`CONSOLE_GRACE`] unless the guest flips again first.
    ///
    /// For a guest that takes `ScanoutReleased`, the buffer that was shown is
    /// released once the clients are done with it, so it is no longer kept to
    /// be re-sent to a client that attaches later: the guest may draw into it.
    pub fn disable(&self) {
        let mut st = self.state.lock().unwrap();
        for c in st.clients.iter_mut() {
            c.last_size = None;
        }
        if st.console_mode == Some(ConsoleMode::Guest) {
            self.set_console_locked(
                &mut st,
                ConsoleMode::Pending(Instant::now() + CONSOLE_GRACE),
            );
        }
        if st.release.enabled() {
            let now = Instant::now();
            st.release.disabled(now);
            st.release.collect(now);
            if st.frame_key.is_some() {
                st.frame = None;
                st.frame_key = None;
            }
            if st.parked_frame.take().is_some() {
                self.note_parked(&st);
            }
        }
        drop(st);
        self.deliver_releases();
    }

    // -- The boot console (crate::console) ---------------------------------

    /// Attach the boot console: it is shown from now (backend start) until
    /// the guest's first flip.
    pub fn attach_console(&self, sink: Arc<dyn ConsoleSink>) {
        let mut st = self.state.lock().unwrap();
        st.console = Some(sink);
        self.console_attached.store(true, Ordering::Release);
        self.set_console_locked(&mut st, ConsoleMode::Shown);
    }

    /// The console is shown. It gets the input then, and also while the
    /// guest takes no Conduit input ([`InputSink::takes_input`]).
    #[inline]
    pub fn console_shown(&self) -> bool {
        self.console_shown.load(Ordering::Acquire)
    }

    /// Where viewer input goes, given whether the guest takes Conduit input:
    /// `true` for the console. With no console attached, always the guest
    /// (as before there was one); with one, the console while it is shown,
    /// and for a guest that takes no Conduit input whoever shows the picture
    /// -- the VM's emulated keyboard and tablet are all such a guest has.
    pub fn input_to_console(&self, guest_takes_input: bool) -> bool {
        self.console_attached.load(Ordering::Acquire)
            && (self.console_shown() || !guest_takes_input)
    }

    /// What the viewer shows: the size of the guest's last frame while the
    /// guest owns the picture, `None` while the console does (its own
    /// framebuffer then) or before the guest's first frame.
    pub fn guest_picture_size(&self) -> Option<(u32, u32)> {
        let st = self.state.lock().unwrap();
        match st.console_mode {
            Some(ConsoleMode::Shown) => None,
            _ => st.guest_size.filter(|&(w, h)| w > 0 && h > 0),
        }
    }

    /// The console's own view: promote a pending takeover that is due, and
    /// say whether the console is shown and, if a takeover is pending, when
    /// to ask again.
    pub fn console_poll(&self) -> (bool, Option<Instant>) {
        let mut st = self.state.lock().unwrap();
        match st.console_mode {
            Some(ConsoleMode::Pending(at)) if Instant::now() >= at => {
                log::info!("display: the guest's scanout stayed off; boot console shown");
                self.set_console_locked(&mut st, ConsoleMode::Shown);
                (true, None)
            }
            Some(ConsoleMode::Pending(at)) => (false, Some(at)),
            Some(ConsoleMode::Shown) => (true, None),
            Some(ConsoleMode::Guest) | None => (false, None),
        }
    }

    /// The guest is gone (device reset, a reboot, or its queues stopped):
    /// the console takes over at once.
    pub fn console_reset(&self, why: &str) {
        let mut st = self.state.lock().unwrap();
        if st.console_mode.is_some_and(|m| m != ConsoleMode::Shown) {
            log::info!("display: {why}; boot console shown");
            self.set_console_locked(&mut st, ConsoleMode::Shown);
        }
    }

    /// Where the console stands (tests, logs).
    pub fn console_mode(&self) -> Option<ConsoleMode> {
        self.state.lock().unwrap().console_mode
    }

    /// A guest frame: the guest owns the picture from now.
    fn guest_shows_locked(&self, st: &mut LinkState, f: &ScanoutFlip) {
        st.guest_size = Some((f.width, f.height));
        if st.console_mode.is_some_and(|m| m != ConsoleMode::Guest) {
            log::info!("display: the guest's driver is showing frames; boot console hidden");
            self.set_console_locked(st, ConsoleMode::Guest);
        }
    }

    fn set_console_locked(&self, st: &mut LinkState, mode: ConsoleMode) {
        let Some(sink) = st.console.clone() else {
            return;
        };
        let was = self.console_shown.load(Ordering::Relaxed);
        let shown = mode == ConsoleMode::Shown;
        st.console_mode = Some(mode);
        self.console_shown.store(shown, Ordering::Release);
        if was != shown {
            // The guest's pointer image is hidden over the console and comes
            // back with the guest.
            for i in 0..st.clients.len() {
                st.clients[i].cursor_dirty = true;
                if st.clients[i].sock.is_some() {
                    let _ = self.send_cursor_locked(st, i);
                }
            }
        }
        sink.wake();
    }

    /// Present one boot-console frame: `memfd` is a memfd sealed against
    /// shrinking, linear, described by `g`, sent with
    /// [`wire::CMD_F_SHM`]. Dropped (`NoBroker`) unless the console is shown:
    /// checked under the lock a guest flip takes, so a console frame can
    /// never land after the guest's first one. Kept for a client that
    /// attaches later, as a guest frame is.
    pub fn flip_console(&self, memfd: RawFd, g: &FrameGeometry) -> FlipOutcome {
        let f = ScanoutFlip {
            width: g.width,
            height: g.height,
            stride: g.stride,
            offset: g.offset,
            fourcc: g.fourcc,
            modifier: g.modifier,
            ..Default::default()
        };
        let mut st = self.state.lock().unwrap();
        if st.console_mode != Some(ConsoleMode::Shown) {
            return FlipOutcome::NoBroker;
        }
        self.flipped_locked(&mut st, None, 0);
        let out = if !self.wants_frames() {
            st.frame = Self::keep(memfd).map(|k| (k, f, wire::CMD_F_SHM));
            st.frame_key = None;
            if st.parked_frame.take().is_some() {
                self.note_parked(&st);
            }
            FlipOutcome::NoBroker
        } else {
            self.present_locked(&mut st, memfd, &f, wire::CMD_F_SHM, None)
        };
        drop(st);
        self.deliver_releases();
        out
    }

    /// Try once to connect client `i`. `Ok(false)` when it has no path.
    fn try_connect_at(&self, i: usize) -> io::Result<bool> {
        let path = {
            let st = self.state.lock().unwrap();
            match st.clients.get(i).and_then(|c| c.path.clone()) {
                Some(p) => p,
                None => return Ok(false),
            }
        };
        let sock = connect_unix(&path)?;
        self.adopt_at(i, sock);
        log::info!("display: connected to broker at {}", path.display());
        Ok(true)
    }

    /// Try once to connect the first client. `Ok(false)` when there is no
    /// path to connect to.
    pub fn try_connect(&self) -> io::Result<bool> {
        self.try_connect_at(0)
    }

    /// The link's own thread: connect every client (each with its own
    /// backoff), read their packets, translate input and push it to `sink`,
    /// reconnect when one goes. Returns only when `stop` is set.
    pub fn run(
        self: &Arc<Self>,
        mut sink: Box<dyn InputSink>,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        const RETRY_MIN: Duration = Duration::from_millis(100);
        const RETRY_MAX: Duration = Duration::from_millis(1000);

        /// What the link thread keeps per client.
        struct Reader {
            seen: Option<Arc<OwnedFd>>,
            tr: InputTranslator,
            reader: wire::PktReader,
            clip_in: ClipAssembler,
            retry: Duration,
            next_try: Instant,
            warned_absent: bool,
        }
        let new_reader = || Reader {
            seen: None,
            tr: InputTranslator::default(),
            reader: wire::PktReader::default(),
            clip_in: ClipAssembler::default(),
            retry: RETRY_MIN,
            next_try: Instant::now(),
            warned_absent: false,
        };
        let mut rd: Vec<Reader> = Vec::new();
        let mut pending: Vec<InputEventEntry> = Vec::new();
        let mut policy = ModeArbiter::new(self.configured);
        let mut pending_mode: Option<DisplayModeEvent> = None;
        // Host clipboard on its way to the guest: generation, text, offset.
        let mut pending_clip: Option<(u64, Vec<u8>, usize)> = None;
        let mut clip_gen: u64 = 0;
        // The newest host clipboard, kept across transfers and reconnects so
        // a guest that comes up later (ClipboardRequest) still gets it.
        let mut latest_clip: Option<Vec<u8>> = None;
        // The sink took nothing last time (no guest yet): retry slowly.
        let mut clip_stalled = false;
        // Input for the boot console (see `input_to_console`), and where
        // input went last: on a change, what the old side holds is released
        // there.
        let mut to_console: Vec<InputEventEntry> = Vec::new();
        let mut routed_console = false;
        // Whether the guest takes Conduit input, as last asked (`None` until
        // first asked), and whether input stays on the console behind the
        // guest's frames (logged once per stretch).
        let mut guest_input: Option<bool> = None;
        let mut kept_on_console = false;
        // Guest-bound input dropped since the guest last took input, for
        // one debug line per stretch.
        let mut dropped_note = false;
        let console_switch = |rd: &mut Vec<Reader>,
                              routed: &mut bool,
                              pending: &mut Vec<InputEventEntry>,
                              to_console: &mut Vec<InputEventEntry>,
                              guest_input: &mut Option<bool>,
                              kept: &mut bool,
                              sink: &mut dyn InputSink| {
            let takes = sink.takes_input();
            let attached = self.console_attached.load(Ordering::Relaxed);
            if attached && takes && *guest_input == Some(false) {
                log::info!(
                    "display: the guest takes Conduit input events (event queue live, input declared)"
                );
            }
            *guest_input = Some(takes);
            let console = self.input_to_console(takes);
            let now_kept = console && !self.console_shown();
            if now_kept && !*kept {
                log::info!(
                    "display: the guest's frames are shown, but it takes no Conduit input \
                     (no NVGPU_CFG_TAKES_INPUT, or no event-queue buffers); input stays on the \
                     VM's emulated keyboard and tablet"
                );
            }
            *kept = now_kept;
            if console == *routed {
                return;
            }
            let old = if *routed {
                &mut *to_console
            } else {
                &mut *pending
            };
            for r in rd.iter_mut() {
                r.tr.release_all(old);
            }
            *routed = console;
        };

        while !stop.load(Ordering::Relaxed) {
            console_switch(
                &mut rd,
                &mut routed_console,
                &mut pending,
                &mut to_console,
                &mut guest_input,
                &mut kept_on_console,
                &mut *sink,
            );
            // Connections that changed under us (a send broke one, a test
            // adopted one): the old one's held input is released.
            let socks = self.sockets();
            while rd.len() < socks.len() {
                rd.push(new_reader());
            }
            let paths = self.paths_by_slot();
            let now = Instant::now();
            for (i, s) in socks.iter().enumerate() {
                let r = &mut rd[i];
                let same = match (&r.seen, s) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    if r.seen.is_some() {
                        r.tr.release_all(if routed_console {
                            &mut to_console
                        } else {
                            &mut pending
                        });
                        if let Some(m) = policy.disconnect(i) {
                            pending_mode = Some(m);
                        }
                    }
                    r.reader.reset();
                    r.clip_in.reset();
                    r.clip_in.set_cap(wire::CLIP_LEGACY_MAX);
                    r.seen = s.clone();
                }
                if s.is_none()
                    && let Some(Some(path)) = paths.get(i)
                    && now >= r.next_try
                {
                    match self.try_connect_at(i) {
                        Ok(_) => {
                            r.retry = RETRY_MIN;
                            r.warned_absent = false;
                        }
                        Err(e) => {
                            if !r.warned_absent {
                                log::info!(
                                    "display: no broker at {} ({e}); waiting for one",
                                    path.display()
                                );
                                r.warned_absent = true;
                            }
                            r.next_try = now + r.retry;
                            r.retry = (r.retry * 2).min(RETRY_MAX);
                        }
                    }
                }
            }
            self.deliver_console(&mut to_console);
            self.deliver(&mut *sink, &mut pending, guest_input, &mut dropped_note);
            let socks = self.sockets();
            // Pick up a connection made just now on the next round.
            if socks
                .iter()
                .zip(rd.iter())
                .any(|(s, r)| s.as_ref().map(Arc::as_ptr) != r.seen.as_ref().map(Arc::as_ptr))
            {
                continue;
            }

            // Undeliverable input (no buffer posted) is retried soon rather
            // than on the next packet: a lost key release is a stuck key.
            let clip_waiting = pending_clip.is_some() && !clip_stalled;
            let mut timeout = if pending.is_empty() && pending_mode.is_none() && !clip_waiting {
                if self.clip_owed() {
                    CLIP_BATCH_EVERY.as_millis() as i32
                } else if self.cursor_owed() || pending_clip.is_some() {
                    20
                } else {
                    500
                }
            } else {
                2
            };
            // Wake for releases that are due or undelivered.
            if let Some(d) = self.tick_releases() {
                timeout = timeout.min((d.as_millis() as i32).max(1));
            }
            // Wake for the next reconnect attempt.
            for (i, s) in socks.iter().enumerate() {
                if s.is_none() && paths.get(i).is_some_and(Option::is_some) {
                    let ms = rd[i]
                        .next_try
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(RETRY_MAX.as_millis()) as i32;
                    timeout = timeout.min(ms.max(1));
                }
            }
            let owed = self.frames_owed();
            let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(socks.len());
            let mut who: Vec<usize> = Vec::with_capacity(socks.len());
            for (i, s) in socks.iter().enumerate() {
                if let Some(s) = s {
                    pfds.push(libc::pollfd {
                        fd: s.as_raw_fd(),
                        events: libc::POLLIN
                            | if owed.get(i).copied().unwrap_or(false) {
                                libc::POLLOUT
                            } else {
                                0
                            },
                        revents: 0,
                    });
                    who.push(i);
                }
            }
            // SAFETY: a live pollfd array of the stated length.
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    log::warn!("display: poll: {e}");
                    std::thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
            let mut writable = Vec::new();
            for (k, pfd) in pfds.iter().enumerate() {
                let i = who[k];
                let sock = socks[i].clone().unwrap();
                if pfd.revents & libc::POLLOUT != 0 {
                    writable.push(i);
                }
                if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                    continue;
                }
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
                    log::info!("display: client {i} closed the connection");
                    self.drop_conn(i, &sock);
                    continue;
                }
                if r < 0 {
                    let e = io::Error::last_os_error();
                    if !matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        log::warn!("display: recv from client {i}: {e}");
                        self.drop_conn(i, &sock);
                    }
                    continue;
                }
                let mut bye = false;
                console_switch(
                    &mut rd,
                    &mut routed_console,
                    &mut pending,
                    &mut to_console,
                    &mut guest_input,
                    &mut kept_on_console,
                    &mut *sink,
                );
                let Reader {
                    tr,
                    reader,
                    clip_in,
                    ..
                } = &mut rd[i];
                reader.feed(&buf[..r as usize], |p| {
                    self.note_packet(i, &p);
                    if p.ty == wire::EV_BYE {
                        bye = true;
                    }
                    if p.ty == wire::EV_HELLO {
                        clip_in.set_cap(wire::clip_cap(p.w1));
                    }
                    if let Some(text) = clip_in.feed(&p) {
                        clip_gen += 1;
                        log::info!(
                            "display: host clipboard ({} bytes) from client {i} -> guest, generation {clip_gen}",
                            text.len()
                        );
                        latest_clip = Some(text.clone());
                        pending_clip = Some((clip_gen, text, 0));
                        clip_stalled = false;
                    }
                    if let Some(m) = policy.packet(i, &p) {
                        pending_mode = Some(m);
                    }
                    // Gamepads are the guest's whatever is shown: the
                    // console has no gamepad to give them to.
                    if routed_console && p.ty != wire::EV_PAD {
                        tr.packet(&p, &mut to_console);
                    } else {
                        tr.packet(&p, &mut pending);
                    }
                });
                if bye {
                    self.drop_conn(i, &sock);
                }
            }
            if !writable.is_empty() {
                self.retry_frames(&writable);
            }
            self.deliver_console(&mut to_console);
            self.deliver(&mut *sink, &mut pending, guest_input, &mut dropped_note);
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
            if self.clip_resend.swap(false, Ordering::Relaxed) {
                match latest_clip.as_ref() {
                    Some(text) => {
                        clip_gen += 1;
                        log::info!(
                            "display: the guest asked for the host clipboard: {} bytes -> guest again, generation {clip_gen}",
                            text.len()
                        );
                        pending_clip = Some((clip_gen, text.clone(), 0));
                        clip_stalled = false;
                    }
                    None => log::debug!(
                        "display: the guest asked for the host clipboard; the viewer has sent none yet"
                    ),
                }
            }
            // Input first: clipboard chunks only take buffers input left.
            if pending.is_empty()
                && let Some((generation, text, off)) = pending_clip.as_mut()
            {
                let before = *off;
                *off = sink.clipboard(*generation, text, *off);
                clip_stalled = *off == before;
                if *off >= text.len() {
                    pending_clip = None;
                    self.stats.clip_to_guest.fetch_add(1, Ordering::Relaxed);
                }
            }
            if self.cursor_owed() {
                self.retry_cursor();
            }
            if self.clip_owed() {
                self.retry_clip();
            }
        }
    }

    fn paths_by_slot(&self) -> Vec<Option<PathBuf>> {
        let st = self.state.lock().unwrap();
        st.clients.iter().map(|c| c.path.clone()).collect()
    }

    fn frames_owed(&self) -> Vec<bool> {
        let st = self.state.lock().unwrap();
        st.clients
            .iter()
            .map(|c| c.frame_owed && c.wants_frames())
            .collect()
    }

    /// Input routed to the boot console (it queues; never blocks).
    fn deliver_console(&self, events: &mut Vec<InputEventEntry>) {
        if events.is_empty() {
            return;
        }
        let console = self.state.lock().unwrap().console.clone();
        if let Some(c) = console {
            c.input(events);
            self.stats
                .input_events
                .fetch_add(events.len() as u64, Ordering::Relaxed);
        }
        events.clear();
    }

    /// Input for the guest, as far as its posted buffers take it. With a
    /// console attached and a guest that takes no Conduit input, what is
    /// left for it (gamepads, which have nowhere else to go) is dropped:
    /// nothing would ever take it.
    fn deliver(
        &self,
        sink: &mut dyn InputSink,
        pending: &mut Vec<InputEventEntry>,
        guest_input: Option<bool>,
        dropped_note: &mut bool,
    ) {
        if guest_input == Some(true) {
            *dropped_note = false;
        }
        if pending.is_empty() {
            return;
        }
        if guest_input == Some(false) && self.console_attached.load(Ordering::Relaxed) {
            if !*dropped_note {
                log::debug!(
                    "display: {} input events for a guest that takes none (gamepad?) dropped; \
                     more are dropped silently until it does",
                    pending.len()
                );
                *dropped_note = true;
            }
            self.stats
                .input_dropped
                .fetch_add(pending.len() as u64, Ordering::Relaxed);
            pending.clear();
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

    fn note_packet(&self, i: usize, p: &wire::Pkt) {
        use wire::*;
        match p.ty {
            EV_HELLO => {
                if p.w0 != PROTO_VERSION {
                    log::warn!(
                        "display: client {i} speaks protocol {}, this backend {PROTO_VERSION}",
                        p.w0
                    );
                }
                log::info!(
                    "display: client {i} hello, version {}, caps {:#x}",
                    p.w0,
                    p.w1
                );
                self.hello_at(i, p.w1);
            }
            EV_ACTIVE => self.set_active(i, p.x != 0),
            EV_RELEASE => {
                self.client_released(i, (p.w0 as u64) | ((p.w1 as u64) << 32), p.x as u32)
            }
            EV_FORMAT => {
                let m = (p.w0 as u64) | ((p.w1 as u64) << 32);
                if p.x == 1 {
                    log::info!(
                        "display: client {i} can show fourcc {:#010x} modifier {m:#018x}",
                        p.y as u32
                    );
                } else {
                    log::warn!(
                        "display: client {i} CANNOT show fourcc {:#010x} modifier {m:#018x}; \
                         frames in it will be dropped (no copy fallback by design)",
                        p.y as u32
                    );
                }
            }
            EV_SURFACE => log::debug!(
                "display: client {i} window {}x{}{}",
                p.x,
                p.y,
                if p.flags & F_FULLSCREEN != 0 {
                    " (fullscreen)"
                } else {
                    ""
                }
            ),
            EV_MODE_HINT => log::debug!(
                "display: client {i} mode hint {}x{} @{} mHz (reason {})",
                p.x,
                p.y,
                p.w0,
                p.w1
            ),
            EV_CLOSE => log::info!("display: the user closed the viewer window"),
            EV_BYE => log::info!("display: client {i} says goodbye (reason {})", p.x),
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
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
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

    /// The routing rule for the three guests there are: a Linux guest whose
    /// module acks `NVGPU_CFG_TAKES_INPUT`, a Linux guest whose module
    /// predates it, and the Windows KMD, which runs the event queue (for
    /// `EventReady`) but never acks the bit. None takes input before its
    /// event queue is live.
    #[test]
    fn input_goes_to_guests_that_declare_it_and_run_the_event_queue() {
        use protocol::messages::{MsgType, NVGPU_CFG_TAKES_INPUT};
        const VERSION_1: u64 = 1 << 32;
        let claims = GuestInputClaims::default();
        assert!(!claims.takes_input(false));
        assert!(!claims.takes_input(true), "nothing declared yet");

        // Linux, current module: acks the bit (and asks for sys files).
        claims.device_started(VERSION_1 | u64::from(NVGPU_CFG_TAKES_INPUT));
        assert!(!claims.takes_input(false), "the queue is still required");
        assert!(claims.takes_input(true));
        claims.saw_request(MsgType::GetSysFiles as u32);
        assert!(claims.takes_input(true));

        // Linux, a module from before the bit: acks only VERSION_1, but asks
        // for sys or proc files at probe, before anything else.
        claims.device_started(VERSION_1);
        assert!(!claims.takes_input(true), "not yet identified");
        claims.saw_request(MsgType::GetSysFiles as u32);
        assert!(claims.takes_input(true));
        claims.saw_request(MsgType::Open as u32);
        assert!(claims.takes_input(true), "later traffic does not undo it");
        assert!(!claims.takes_input(false));
        claims.device_started(VERSION_1);
        claims.saw_request(MsgType::GetProcFiles as u32);
        assert!(claims.takes_input(true));

        // Windows: VERSION_1 only, event queue live, RM and Venus traffic,
        // never sys files. Input stays on the console.
        claims.device_started(VERSION_1);
        for t in [
            MsgType::Open,
            MsgType::Ioctl,
            MsgType::ScanoutFlip,
            MsgType::GpuCmd,
            MsgType::Close,
        ] {
            claims.saw_request(t as u32);
        }
        assert!(!claims.takes_input(true));
        // ...and then NVK on RM in an application, whose librmclient
        // forwards GetSysFiles through the KMD: still not the Linux module.
        claims.saw_request(MsgType::GetSysFiles as u32);
        claims.saw_request(MsgType::GetProcFiles as u32);
        assert!(
            !claims.takes_input(true),
            "Windows NVK's GetSysFiles came late"
        );

        // A Linux guest reboots into Windows: the restart forgets it.
        claims.device_started(VERSION_1 | u64::from(NVGPU_CFG_TAKES_INPUT));
        claims.saw_request(MsgType::GetSysFiles as u32);
        assert!(claims.takes_input(true));
        claims.reset();
        assert!(!claims.takes_input(true));
        claims.device_started(VERSION_1);
        assert!(!claims.takes_input(true));

        // The rule itself.
        for (live, acked, linux, want) in [
            (false, false, false, false),
            (false, true, true, false),
            (true, false, false, false),
            (true, true, false, true),
            (true, false, true, true),
            (true, true, true, true),
        ] {
            assert_eq!(guest_takes_input(live, acked, linux), want);
        }
    }

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

        // Gamepads: the pad rides in the type's high byte; bad ones are dropped.
        let mut pads = Vec::new();
        t.packet(
            &pkt(wire::EV_PAD, 0x130, 1, (1 << 16) | EV_KEY as u32, 0),
            &mut pads,
        );
        t.packet(
            &pkt(wire::EV_PAD, ABS_X as i32, -500, EV_ABS as u32, 0),
            &mut pads,
        );
        t.packet(
            &pkt(wire::EV_PAD, 0, 0, (1 << 16) | EV_SYN as u32, 0),
            &mut pads,
        );
        t.packet(
            &pkt(wire::EV_PAD, 0x130, 1, (9 << 16) | EV_KEY as u32, 0),
            &mut pads,
        );
        t.packet(
            &pkt(wire::EV_PAD, 0, 1, (1 << 16) | EV_REL as u32, 0),
            &mut pads,
        );
        assert_eq!(
            pads,
            vec![
                ev(0x200 | EV_KEY, 0x130, 1),
                ev(0x100 | EV_ABS, ABS_X, -500),
                ev(0x200 | EV_SYN, SYN_REPORT, 0),
            ]
        );

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
    /// An exporter for parked buffers that hands out dups of `buf`.
    fn fake_exporter(link: &DisplayLink, buf: &OwnedFd) -> Arc<Mutex<u32>> {
        let calls = Arc::new(Mutex::new(0u32));
        let (c, b) = (calls.clone(), buf.try_clone().unwrap());
        link.set_exporter(Arc::new(move |_drm, _h| {
            *c.lock().unwrap() += 1;
            b.try_clone().map_err(|_| libc::EIO)
        }));
        calls
    }

    #[test]
    fn a_new_viewer_gets_the_current_frame_at_hello() {
        let link = DisplayLink::new(None);
        let buf = memfd();
        let exports = fake_exporter(&link, &buf);
        let drm = memfd();
        // The guest flips while no viewer is attached: nothing is exported,
        // the buffer is only remembered.
        assert!(!link.wants_frames());
        assert!(link.park(drm.as_raw_fd(), &flip(1, 1920)));
        assert_eq!(*exports.lock().unwrap(), 0);
        let (ours, broker) = socketpair();
        link.adopt(ours);
        link.hello(0);
        assert_eq!(*exports.lock().unwrap(), 1, "exported when someone looks");
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_CAPS);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_WINDOW);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_QUERY_FORMAT);
        let (c, f) = broker_recv(broker.as_raw_fd());
        assert_eq!((c.ty, c.width), (wire::CMD_ATTACH, 1920));
        assert_eq!(
            inode(f.expect("the frame").as_raw_fd()),
            inode(buf.as_raw_fd())
        );
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_COMMIT);
        // The viewer closes and another attaches: it gets the frame too.
        drop(broker);
        let (ours2, broker2) = socketpair();
        link.adopt(ours2);
        link.hello(0);
        assert_eq!(broker_recv(broker2.as_raw_fd()).0.ty, wire::CMD_CAPS);
        assert_eq!(broker_recv(broker2.as_raw_fd()).0.ty, wire::CMD_WINDOW);
        assert_eq!(
            broker_recv(broker2.as_raw_fd()).0.ty,
            wire::CMD_QUERY_FORMAT
        );
        assert_eq!(broker_recv(broker2.as_raw_fd()).0.ty, wire::CMD_ATTACH);
        assert_eq!(*exports.lock().unwrap(), 1, "kept, not exported again");
    }

    /// No client: a flip is one atomic load for the caller, and the link
    /// itself neither dups nor sends anything.
    #[test]
    fn with_no_client_a_flip_costs_nothing() {
        let link = DisplayLink::new(None);
        let buf = memfd();
        assert!(!link.wants_frames());
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(1, 1920)),
            FlipOutcome::NoBroker
        );
        assert!(link.state.lock().unwrap().frame.is_none(), "no dup kept");
        // The same drm file parks without a second dup.
        let drm = memfd();
        assert!(link.park(drm.as_raw_fd(), &flip(1, 1920)));
        let first = link
            .state
            .lock()
            .unwrap()
            .parked_frame
            .as_ref()
            .unwrap()
            .drm
            .clone();
        assert!(link.park(drm.as_raw_fd(), &flip(2, 1920)));
        let again = link
            .state
            .lock()
            .unwrap()
            .parked_frame
            .as_ref()
            .unwrap()
            .drm
            .clone();
        assert!(Arc::ptr_eq(&first, &again));
        assert!(link.has_parked());
        // The guest closing the handle or the file forgets it.
        link.forget(3, 4);
        assert!(!link.has_parked());
        assert!(link.park(drm.as_raw_fd(), &flip(3, 1920)));
        link.forget_owner(3);
        assert!(!link.has_parked());
    }

    /// Two clients (two paths): connected one by one.
    fn two_clients() -> (Arc<DisplayLink>, OwnedFd, OwnedFd) {
        let link = DisplayLink::with_paths(
            vec!["/nonexistent/a".into(), "/nonexistent/b".into()],
            DisplayMode::DEFAULT,
        );
        let (a, broker_a) = socketpair();
        let (b, broker_b) = socketpair();
        link.adopt_at(0, a);
        link.adopt_at(1, b);
        (link, broker_a, broker_b)
    }

    /// Read records until the next ATTACH; its fd and the COMMIT after it.
    fn next_frame(fd: RawFd) -> (wire::Cmd, OwnedFd) {
        loop {
            let (c, f) = broker_recv(fd);
            if c.ty == wire::CMD_ATTACH {
                assert_eq!(broker_recv(fd).0.ty, wire::CMD_COMMIT);
                return (c, f.expect("ATTACH carries the dma-buf"));
            }
        }
    }

    fn pending_bytes(fd: RawFd) -> usize {
        let mut n: libc::c_int = 0;
        assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) }, 0);
        n as usize
    }

    #[test]
    fn two_clients_get_the_same_frames() {
        let (link, a, b) = two_clients();
        link.hello_at(0, 0);
        link.hello_at(1, 0);
        let buf = memfd();
        for seq in 1..=3 {
            assert_eq!(
                link.flip(buf.as_raw_fd(), &flip(seq, 2560)),
                FlipOutcome::Sent
            );
        }
        for broker in [&a, &b] {
            assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_CAPS);
            for _ in 0..3 {
                let (c, f) = next_frame(broker.as_raw_fd());
                assert_eq!(c.width, 2560);
                assert_eq!(inode(f.as_raw_fd()), inode(buf.as_raw_fd()));
            }
            assert_eq!(pending_bytes(broker.as_raw_fd()), 0);
        }
        assert_eq!(link.stats.sent.load(Ordering::Relaxed), 6);
    }

    /// A client that never reads fills its socket and loses frames; the
    /// other gets every one, and the stuck one gets the newest frame once it
    /// drains.
    #[test]
    fn a_stuck_client_does_not_stall_the_other() {
        let (link, stuck, fine) = two_clients();
        let buf = memfd();
        let start = Instant::now();
        let mut busy = 0;
        for seq in 0..20_000u64 {
            link.flip(buf.as_raw_fd(), &flip(seq, 2560));
            // The healthy client keeps up.
            let (c, _) = next_frame(fine.as_raw_fd());
            assert_eq!(c.width, 2560);
            if link.frames_owed()[0] {
                busy += 1;
                if busy > 10 {
                    break;
                }
            }
        }
        assert!(busy > 10, "the stuck socket never filled");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(link.wants_frames());
        // It drains: the link thread re-sends the newest frame.
        let mut sink = [0u8; 4096];
        while unsafe {
            libc::recv(
                stuck.as_raw_fd(),
                sink.as_mut_ptr().cast(),
                sink.len(),
                libc::MSG_DONTWAIT,
            )
        } > 0
        {}
        link.retry_frames(&[0]);
        assert!(!link.frames_owed()[0]);
        assert_eq!(link.stats.resent.load(Ordering::Relaxed), 1);
        let (c, _) = next_frame(stuck.as_raw_fd());
        assert_eq!(c.width, 2560);
    }

    #[test]
    fn either_client_can_leave_and_come_back() {
        let (link, a, b) = two_clients();
        let buf = memfd();
        drop(a);
        // The dead one breaks on the send; the other still gets the frame.
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(1, 2560)),
            FlipOutcome::Sent
        );
        assert!(link.sockets()[0].is_none());
        next_frame(b.as_raw_fd());
        // It comes back and gets the current frame at HELLO.
        let (a2, broker_a2) = socketpair();
        link.adopt_at(0, a2);
        link.hello_at(0, 0);
        assert_eq!(broker_recv(broker_a2.as_raw_fd()).0.ty, wire::CMD_CAPS);
        next_frame(broker_a2.as_raw_fd());
        // Now the other goes; the first is untouched.
        drop(b);
        assert_eq!(
            link.flip(buf.as_raw_fd(), &flip(2, 2560)),
            FlipOutcome::Sent
        );
        next_frame(broker_a2.as_raw_fd());
        assert!(link.sockets()[1].is_none());
        assert_eq!(pending_bytes(broker_a2.as_raw_fd()), 0);
    }

    /// A client with CAP_IDLE gets nothing until it says EV_ACTIVE, and
    /// nothing again after it goes idle.
    #[test]
    fn an_idle_client_gets_no_frames() {
        let (link, viewer, stream) = two_clients();
        link.hello_at(0, 0);
        link.hello_at(1, wire::CAP_IDLE | wire::CAP_CURSOR);
        let buf = memfd();
        link.flip(buf.as_raw_fd(), &flip(1, 2560));
        link.cursor(Some(buf.as_raw_fd()), &cursor(9, 1, 1));
        next_frame(viewer.as_raw_fd());
        let (c, _) = broker_recv(stream.as_raw_fd());
        assert_eq!(c.ty, wire::CMD_CAPS);
        assert!(c.width & wire::CLIENT_IDLE != 0);
        assert_eq!(pending_bytes(stream.as_raw_fd()), 0, "idle: nothing");
        // Active: the current frame at once, then the cursor and every flip.
        link.set_active(1, true);
        next_frame(stream.as_raw_fd());
        link.retry_cursor();
        assert_eq!(broker_recv(stream.as_raw_fd()).0.ty, wire::CMD_CURSOR);
        link.flip(buf.as_raw_fd(), &flip(2, 2560));
        next_frame(stream.as_raw_fd());
        // Idle again: nothing more, the viewer carries on.
        link.set_active(1, false);
        link.flip(buf.as_raw_fd(), &flip(3, 2560));
        assert_eq!(pending_bytes(stream.as_raw_fd()), 0);
        // Only the viewer idle-capable? No: with both idle nothing is exported.
        link.hello_at(0, wire::CAP_IDLE);
        assert!(!link.wants_frames());
    }

    fn hint(w: i32, h: i32) -> wire::Pkt {
        pkt(wire::EV_MODE_HINT, 0, w, h, 0, 2)
    }

    fn mode_of(m: Option<DisplayModeEvent>) -> Option<(u32, u32)> {
        m.map(|m| (m.width, m.height))
    }

    /// Viewer (slot 0) and stream (slot 1): an active stream's request wins,
    /// the viewer's is remembered, not applied, and comes back when the
    /// stream goes idle.
    #[test]
    fn the_mode_follows_an_active_stream_then_the_viewer_again() {
        let mut a = ModeArbiter::new(DisplayMode::DEFAULT);
        let hello = |caps| pkt(wire::EV_HELLO, 0, 0, 0, 2, caps);
        let active = |on| pkt(wire::EV_ACTIVE, 0, on, 0, 0, 0);
        assert_eq!(a.packet(0, &hello(wire::CAP_MODE_HINTS)), None);
        assert_eq!(mode_of(a.packet(0, &hint(1600, 900))), Some((1600, 900)));
        // The stream connects idle: nothing changes.
        assert_eq!(
            a.packet(1, &hello(wire::CAP_MODE_HINTS | wire::CAP_IDLE)),
            None
        );
        // A session starts: active, then its size.
        assert_eq!(a.packet(1, &active(1)), None);
        assert_eq!(mode_of(a.packet(1, &hint(1920, 1080))), Some((1920, 1080)));
        // The viewer resizes meanwhile: remembered, not applied (no ping-pong).
        assert_eq!(a.packet(0, &hint(1700, 950)), None);
        assert_eq!(a.packet(0, &hint(1800, 1000)), None);
        assert_eq!(a.current(), (1920, 1080, 240_000));
        // The session ends: the viewer's latest request applies again.
        assert_eq!(mode_of(a.packet(1, &active(0))), Some((1800, 1000)));
        // A second session: the stream again; then it crashes (disconnect).
        a.packet(1, &active(1));
        assert_eq!(mode_of(a.packet(1, &hint(3840, 2160))), Some((3840, 2160)));
        assert_eq!(mode_of(a.disconnect(1)), Some((1800, 1000)));
    }

    #[test]
    fn the_mode_without_a_viewer_and_between_viewers() {
        let mut a = ModeArbiter::new(DisplayMode::DEFAULT);
        let hello = |caps| pkt(wire::EV_HELLO, 0, 0, 0, 2, caps);
        let active = |on| pkt(wire::EV_ACTIVE, 0, on, 0, 0, 0);
        // A stream alone: its session, then idle restores the configured mode.
        a.packet(1, &hello(wire::CAP_MODE_HINTS | wire::CAP_IDLE));
        a.packet(1, &active(1));
        assert_eq!(mode_of(a.packet(1, &hint(1280, 720))), Some((1280, 720)));
        assert_eq!(mode_of(a.packet(1, &active(0))), Some((2560, 1440)));
        // Idle twice, or an idle client's hint: nothing.
        assert_eq!(a.packet(1, &active(0)), None);
        assert_eq!(a.packet(1, &hint(640, 480)), None);
        // Two plain clients: the most recent request wins.
        a.packet(0, &hello(wire::CAP_MODE_HINTS));
        a.packet(2, &hello(wire::CAP_MODE_HINTS));
        assert_eq!(mode_of(a.packet(0, &hint(1000, 800))), Some((1000, 800)));
        assert_eq!(mode_of(a.packet(2, &hint(1200, 800))), Some((1200, 800)));
        assert_eq!(mode_of(a.packet(0, &hint(1000, 800))), Some((1000, 800)));
        // The last one leaving keeps the mode, as with a single viewer.
        assert_eq!(mode_of(a.disconnect(0)), Some((1200, 800)));
        assert_eq!(a.disconnect(2), None);
        assert_eq!(a.current(), (1200, 800, 240_000));
        // A legacy client (no hints) next to them still works by EV_SURFACE.
        a.packet(3, &hello(0));
        let fs = a.packet(
            3,
            &pkt(wire::EV_SURFACE, wire::F_FULLSCREEN, 3840, 2160, 60_000, 0),
        );
        assert_eq!(mode_of(fs), Some((3840, 2160)));
    }

    /// The single-client flip path, for comparing the per-frame cost before
    /// and after a change: `cargo test -p device --release --lib
    /// flip_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn flip_cost() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        link.adopt(ours);
        let buf = memfd();
        let stop = Arc::new(AtomicBool::new(false));
        let drain = {
            let stop = stop.clone();
            let fd = broker.as_raw_fd();
            std::thread::spawn(move || {
                let mut b = [0u8; 65536];
                let mut cbuf = [0u64; 64];
                while !stop.load(Ordering::Relaxed) {
                    let mut iov = libc::iovec {
                        iov_base: b.as_mut_ptr().cast(),
                        iov_len: b.len(),
                    };
                    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                    msg.msg_iov = &mut iov;
                    msg.msg_iovlen = 1;
                    msg.msg_control = cbuf.as_mut_ptr().cast();
                    msg.msg_controllen = std::mem::size_of_val(&cbuf);
                    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT) };
                    if n <= 0 {
                        std::thread::yield_now();
                        continue;
                    }
                    unsafe {
                        let mut c = libc::CMSG_FIRSTHDR(&msg);
                        while !c.is_null() {
                            let p = libc::CMSG_DATA(c) as *const RawFd;
                            let k = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                            for j in 0..k {
                                libc::close(*p.add(j));
                            }
                            c = libc::CMSG_NXTHDR(&msg, c);
                        }
                    }
                }
            })
        };
        let n = 200_000u64;
        for round in 0..3 {
            let t = Instant::now();
            for i in 0..n {
                let _ = link.flip(buf.as_raw_fd(), &flip(i, 2560));
            }
            let ns = t.elapsed().as_nanos() as f64 / n as f64;
            eprintln!(
                "round {round}: {ns:.0} ns per flip (sent {}, busy {})",
                link.stats.sent.load(Ordering::Relaxed),
                link.stats.busy.load(Ordering::Relaxed)
            );
        }
        stop.store(true, Ordering::Relaxed);
        drain.join().unwrap();
    }

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
        assert_eq!(
            (c.ty, c.width),
            (
                wire::CMD_CAPS,
                wire::CLIENT_SEQ_USEC
                    | wire::CLIENT_CLIPBOARD
                    | wire::CLIENT_CLIP_LARGE
                    | wire::CLIENT_GAMEPAD
                    | wire::CLIENT_IDLE
            )
        );
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

    #[test]
    fn clipboard_chunks_reassemble_with_a_cap() {
        let mut a = ClipAssembler::default();
        assert_eq!(a.cap(), wire::CLIP_LEGACY_MAX);
        assert_eq!(a.feed(&wire::Pkt::clip(b"hello, ", false)), None);
        // Other packets in between are not clipboard and change nothing.
        assert_eq!(a.feed(&pkt(wire::EV_KEY, 0, 30, 1, 0, 0)), None);
        assert_eq!(
            a.feed(&wire::Pkt::clip(b"world", true)),
            Some(b"hello, world".to_vec())
        );
        // Empty transfers carry nothing.
        assert_eq!(a.feed(&wire::Pkt::clip(b"", true)), None);
        // Over the cap: abandoned up to LAST, the next one is clean.
        a.set_cap(20);
        for _ in 0..2 {
            assert_eq!(a.feed(&wire::Pkt::clip(&[b'x'; 15], false)), None);
        }
        assert_eq!(a.feed(&wire::Pkt::clip(b"y", true)), None);
        assert_eq!(a.feed(&wire::Pkt::clip(b"ok", true)), Some(b"ok".to_vec()));
        // A lying nbytes is malformed, not an overread.
        let mut bad = wire::Pkt::clip(b"abc", false).encode();
        bad[8] = 0x1f;
        assert_eq!(a.feed(&wire::Pkt::decode(&bad)), None);
        assert_eq!(a.feed(&wire::Pkt::clip(b"z", true)), None);
        assert_eq!(a.feed(&wire::Pkt::clip(b"z", true)), Some(b"z".to_vec()));
        // Reset drops a partial transfer.
        a.feed(&wire::Pkt::clip(b"partial", false));
        a.reset();
        assert_eq!(
            a.feed(&wire::Pkt::clip(b"new", true)),
            Some(b"new".to_vec())
        );
    }

    #[test]
    fn clipboard_records_have_the_c_layout() {
        let text: Vec<u8> = (0..60u8).collect();
        let mut out = ClipOut::new(text.clone());
        assert_eq!(out.chunks(), 3);
        let (bytes, n) = out.records(2);
        assert_eq!((bytes.len(), n), (80, 2));
        let r0 = &bytes[..40];
        assert_eq!(u16::from_le_bytes([r0[0], r0[1]]), wire::CMD_CLIPBOARD);
        assert_eq!(&r0[2..4], &[0, 0]);
        assert_eq!(u32::from_le_bytes(r0[4..8].try_into().unwrap()), 0);
        assert_eq!(&r0[8..12], &[0; 4]);
        assert_eq!(r0[12], 27);
        assert_eq!(&r0[13..40], &text[..27]);
        assert_eq!(u32::from_le_bytes(bytes[44..48].try_into().unwrap()), 1);
        out.commit(n);
        assert!(!out.done());
        let (bytes, n) = out.records(128);
        assert_eq!(n, 1);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
        assert_eq!(bytes[12], 6 | wire::CLIP_LAST);
        assert_eq!(&bytes[13..19], &text[54..]);
        out.commit(n);
        assert!(out.done());
    }

    /// Read CMD_CLIPBOARD records until LAST (skipping others); the text and
    /// how many records it took.
    fn broker_clip(fd: RawFd) -> (Vec<u8>, u32) {
        let mut text = Vec::new();
        let mut next = 0u32;
        loop {
            let mut rec = [0u8; wire::CMD_SIZE];
            let n =
                unsafe { libc::recv(fd, rec.as_mut_ptr().cast(), rec.len(), libc::MSG_WAITALL) };
            assert_eq!(n, wire::CMD_SIZE as isize);
            if u16::from_le_bytes([rec[0], rec[1]]) != wire::CMD_CLIPBOARD {
                continue;
            }
            assert_eq!(u32::from_le_bytes(rec[4..8].try_into().unwrap()), next);
            next += 1;
            text.extend_from_slice(&rec[13..13 + (rec[12] & 0x1f) as usize]);
            if rec[12] & wire::CLIP_LAST != 0 {
                return (text, next);
            }
        }
    }

    #[test]
    fn guest_clipboard_goes_paced_and_capped() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        // No broker: dropped.
        link.clipboard_to_host(b"lost".to_vec());
        link.adopt(ours);
        // A legacy broker takes at most 7168 bytes.
        link.hello(0);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_CAPS);
        link.clipboard_to_host(vec![b'a'; wire::CLIP_LEGACY_MAX + 1]);
        assert!(!link.clip_owed());
        // A large one gets a big copy, in paced batches.
        link.hello(wire::CAP_CLIP_LARGE);
        assert_eq!(broker_recv(broker.as_raw_fd()).0.ty, wire::CMD_CAPS);
        let text: Vec<u8> = (0..20_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        link.clipboard_to_host(text.clone());
        assert!(link.clip_owed(), "more than one batch");
        let reader = {
            let fd = broker.as_raw_fd();
            std::thread::spawn(move || broker_clip(fd))
        };
        assert!(wait_for(|| {
            link.retry_clip();
            !link.clip_owed()
        }));
        let (got, recs) = reader.join().unwrap();
        assert_eq!(got, text);
        assert_eq!(recs as usize, text.len().div_ceil(wire::CLIP_CMD_BYTES));
        assert_eq!(link.stats.clip_to_host.load(Ordering::Relaxed), 1);
        // A reconnect forgets a transfer in progress.
        link.clipboard_to_host(text.clone());
        assert!(link.clip_owed());
        let (ours2, _broker2) = socketpair();
        link.adopt(ours2);
        assert!(!link.clip_owed());
    }

    /// Records the clipboard the link hands over, a few bytes per call.
    struct ClipSink(Arc<Mutex<Vec<(u64, Vec<u8>)>>>);
    impl InputSink for ClipSink {
        fn push(&mut self, events: &[InputEventEntry]) -> usize {
            events.len()
        }
        fn clipboard(&mut self, generation: u64, data: &[u8], offset: usize) -> usize {
            let end = (offset + 5).min(data.len());
            let mut g = self.0.lock().unwrap();
            if offset == 0 {
                g.push((generation, Vec::new()));
            }
            g.last_mut()
                .unwrap()
                .1
                .extend_from_slice(&data[offset..end]);
            end
        }
    }

    #[test]
    fn host_clipboard_reaches_the_sink_and_reconnect_resets() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        link.adopt(ours);
        let got = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let th = {
            let (link, got, stop) = (link.clone(), got.clone(), stop.clone());
            std::thread::spawn(move || link.run(Box::new(ClipSink(got)), &stop))
        };
        let send = |p: wire::Pkt| {
            let b = p.encode();
            let n = unsafe { libc::send(broker.as_raw_fd(), b.as_ptr().cast(), b.len(), 0) };
            assert_eq!(n, b.len() as isize);
        };
        send(wire::Pkt {
            ty: wire::EV_HELLO,
            w0: 2,
            w1: wire::CAP_CLIP_LARGE,
            ..Default::default()
        });
        let text: Vec<u8> = (0..40u8).map(|i| b'A' + i % 26).collect();
        for (i, c) in text.chunks(wire::CLIP_PKT_BYTES).enumerate() {
            send(wire::Pkt::clip(
                c,
                (i + 1) * wire::CLIP_PKT_BYTES >= text.len(),
            ));
        }
        assert!(wait_for(|| got
            .lock()
            .unwrap()
            .first()
            .is_some_and(|t| t.1 == text)));
        assert_eq!(got.lock().unwrap()[0].0, 1);
        // The guest driver came up late and asks: the same text again.
        link.request_host_clipboard();
        assert!(wait_for(|| got
            .lock()
            .unwrap()
            .get(1)
            .is_some_and(|t| t.1 == text)));
        assert_eq!(got.lock().unwrap()[1].0, 2);
        // A partial transfer, then the broker goes: nothing half-delivered.
        send(wire::Pkt::clip(b"half", false));
        stop.store(true, Ordering::Relaxed);
        drop(broker);
        th.join().unwrap();
        assert_eq!(got.lock().unwrap().len(), 2);
        assert!(wait_for(|| link
            .stats
            .clip_to_guest
            .load(Ordering::Relaxed)
            == 2));
    }

    // -- Buffer release (docs/SCANOUT.md "Buffer release") -----------------

    /// What reached the guest's event queue, with room for `room` more.
    struct Releases {
        got: Mutex<Vec<ScanoutReleased>>,
        room: Mutex<usize>,
    }

    impl ReleaseSink for Releases {
        fn released(&self, r: &[ScanoutReleased]) -> usize {
            let mut room = self.room.lock().unwrap();
            let n = r.len().min(*room);
            *room -= n;
            self.got.lock().unwrap().extend_from_slice(&r[..n]);
            n
        }
    }

    fn releasing(link: &DisplayLink) -> Arc<Releases> {
        let sink = Arc::new(Releases {
            got: Mutex::new(Vec::new()),
            room: Mutex::new(usize::MAX),
        });
        link.set_release_sink(sink.clone());
        link.set_release_enabled(true);
        sink
    }

    fn gflip(seq: u64, handle: u32) -> ScanoutFlip {
        ScanoutFlip {
            host_handle: handle,
            ..flip(seq, 1920)
        }
    }

    fn released(sink: &Releases) -> Vec<(u32, u64, u32)> {
        std::mem::take(&mut *sink.got.lock().unwrap())
            .iter()
            .map(|r| (r.host_handle, r.seq, r.flags))
            .collect()
    }

    const NOT_SHOWN: u32 = protocol::messages::SCANOUT_RELEASED_NOT_SHOWN;

    #[test]
    fn a_guest_that_did_not_ask_gets_no_release() {
        let link = DisplayLink::new(None);
        let sink = releasing(&link);
        link.set_release_enabled(false);
        let (a, b) = (memfd(), memfd());
        link.flip(a.as_raw_fd(), &gflip(1, 10));
        link.flip(b.as_raw_fd(), &gflip(2, 11));
        assert!(released(&sink).is_empty());
    }

    #[test]
    fn with_nobody_watching_the_replaced_buffer_is_released_at_once() {
        let link = DisplayLink::new(None);
        let sink = releasing(&link);
        let drm = memfd();
        // Parked (GEM, never exported) and kept (a Venus dma-buf) alike.
        assert!(link.park(drm.as_raw_fd(), &gflip(1, 10)));
        assert!(released(&sink).is_empty(), "the shown buffer is in use");
        assert!(link.park(drm.as_raw_fd(), &gflip(2, 11)));
        assert_eq!(released(&sink), vec![(10, 1, NOT_SHOWN)]);
        let v = memfd();
        let g = FrameGeometry {
            width: 64,
            height: 64,
            stride: 256,
            ..Default::default()
        };
        link.flip_dmabuf(v.as_raw_fd(), &g, Some(77));
        assert_eq!(released(&sink), vec![(11, 2, NOT_SHOWN)]);
        link.flip_dmabuf(v.as_raw_fd(), &g, Some(78));
        assert_eq!(
            released(&sink),
            vec![(
                77,
                0,
                NOT_SHOWN | protocol::messages::SCANOUT_RELEASED_RESOURCE
            )]
        );
        // Turning the scanout off releases what it showed.
        link.disable();
        assert_eq!(released(&sink).len(), 1);
    }

    #[test]
    fn a_reporting_client_holds_a_buffer_until_it_releases_it() {
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        let sink = releasing(&link);
        link.adopt(ours);
        link.hello_for_test(wire::CAP_RELEASE_SEQ);
        let (a, b) = (memfd(), memfd());
        assert_eq!(link.flip(a.as_raw_fd(), &gflip(1, 10)), FlipOutcome::Sent);
        let (ca, fa) = next_frame(broker.as_raw_fd());
        assert_eq!(link.flip(b.as_raw_fd(), &gflip(2, 11)), FlipOutcome::Sent);
        let (cb, fb) = next_frame(broker.as_raw_fd());
        assert!(released(&sink).is_empty(), "replaced, but still read");
        // A release with the wrong stamp, or of the buffer on screen, frees
        // nothing.
        link.client_released(0, inode(fa.as_raw_fd()), ca.seq.wrapping_sub(1));
        link.client_released(0, inode(fb.as_raw_fd()), cb.seq);
        assert!(released(&sink).is_empty());
        link.client_released(0, inode(fa.as_raw_fd()), ca.seq);
        assert_eq!(released(&sink), vec![(10, 1, 0)]);
        // Buffer 11 was released while on screen: it goes as soon as it is
        // replaced.
        assert_eq!(link.flip(a.as_raw_fd(), &gflip(3, 10)), FlipOutcome::Sent);
        assert_eq!(released(&sink), vec![(11, 2, 0)]);
    }

    #[test]
    fn an_older_client_is_done_once_sent_the_next_buffer() {
        let (link, a, b) = two_clients();
        let sink = releasing(&link);
        link.hello_at(0, wire::CAP_RELEASE_SEQ);
        link.hello_at(1, 0); // a client from before CAP_RELEASE_SEQ
        let (x, y) = (memfd(), memfd());
        link.flip(x.as_raw_fd(), &gflip(1, 10));
        let (cx, fx) = next_frame(a.as_raw_fd());
        let _ = next_frame(b.as_raw_fd());
        link.flip(y.as_raw_fd(), &gflip(2, 11));
        let _ = next_frame(a.as_raw_fd());
        let _ = next_frame(b.as_raw_fd());
        assert!(released(&sink).is_empty(), "client 0 still reads it");
        // The older client's EV_RELEASE (no stamp semantics) means nothing.
        link.client_released(1, inode(fx.as_raw_fd()), cx.seq);
        assert!(released(&sink).is_empty());
        link.client_released(0, inode(fx.as_raw_fd()), cx.seq);
        assert_eq!(released(&sink), vec![(10, 1, 0)]);
    }

    #[test]
    fn a_client_that_leaves_or_idles_releases_what_it_held() {
        let (link, a, b) = two_clients();
        let sink = releasing(&link);
        link.hello_at(0, wire::CAP_RELEASE_SEQ);
        link.hello_at(1, wire::CAP_RELEASE_SEQ | wire::CAP_IDLE);
        link.set_active(1, true);
        let (x, y) = (memfd(), memfd());
        link.flip(x.as_raw_fd(), &gflip(1, 10));
        link.flip(y.as_raw_fd(), &gflip(2, 11));
        let _ = (next_frame(a.as_raw_fd()), next_frame(b.as_raw_fd()));
        let _ = (next_frame(a.as_raw_fd()), next_frame(b.as_raw_fd()));
        link.set_active(1, false);
        assert!(released(&sink).is_empty(), "client 0 still holds it");
        // Client 0 goes away.
        link.drop_conn(0, &link.sockets()[0].clone().unwrap());
        assert_eq!(released(&sink), vec![(10, 1, 0)]);
    }

    #[test]
    fn releases_wait_for_event_buffers_and_closed_buffers_are_forgotten() {
        let link = DisplayLink::new(None);
        let sink = releasing(&link);
        *sink.room.lock().unwrap() = 0;
        let drm = memfd();
        link.park(drm.as_raw_fd(), &gflip(1, 10));
        link.park(drm.as_raw_fd(), &gflip(2, 11));
        link.park(drm.as_raw_fd(), &gflip(3, 12));
        assert!(released(&sink).is_empty());
        // The guest closed handle 11 meanwhile: nobody waits for it.
        link.forget(3, 11);
        *sink.room.lock().unwrap() = usize::MAX;
        assert_eq!(link.tick_releases(), None);
        assert_eq!(released(&sink), vec![(10, 1, NOT_SHOWN)]);
        assert_eq!(link.stats.released.load(Ordering::Relaxed), 1);
    }
}

//! Moonlight input → Linux evdev, as broker events for the backend.
//!
//! Packets: NV_INPUT_HEADER { u32 size (big-endian, excluding itself), u32
//! magic (little-endian) } and a body whose field byte order is per type (the
//! GameStream protocol mixes both). Every read is bounds-checked; anything
//! unknown or short is ignored.

use crate::broker::{self, Pkt};
use std::collections::HashSet;

const KEY_DOWN: u32 = 0x03;
const KEY_UP: u32 = 0x04;
const MOUSE_ABS: u32 = 0x05;
const MOUSE_REL_GEN5: u32 = 0x07;
const MOUSE_REL: u32 = 0x06;
const MOUSE_BTN_DOWN: u32 = 0x08;
const MOUSE_BTN_UP: u32 = 0x09;
const SCROLL: u32 = 0x0A;
const SCROLL_OLD: u32 = 0x09; // gen <5; never sent by 7.x clients (and it collides with BTN_UP)
const MULTI_CONTROLLER: u32 = 0x0C;
const MULTI_CONTROLLER_NEW: u32 = 0x0D;
const UTF8_TEXT: u32 = 0x17;
const SS_HSCROLL: u32 = 0x5500_0001;
const SS_CONTROLLER_ARRIVAL: u32 = 0x5500_0004;
const SS_TOUCH: u32 = 0x5500_0002;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_ABS: u16 = 3;
pub const SYN_REPORT: u16 = 0;

pub const MAX_PADS: usize = 4;

/// Where the guest picture sits inside the stream, to map absolute input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Stream size.
    pub sw: u32,
    pub sh: u32,
    /// Guest picture size (0 = unknown: assume the stream size).
    pub gw: u32,
    pub gh: u32,
}

impl Geometry {
    /// The picture rect inside the stream (aspect-fit, centred), as gpu.c draws it.
    pub fn rect(&self) -> (f64, f64, f64, f64) {
        let (gw, gh) = self.guest();
        if gw == self.sw && gh == self.sh {
            return (0.0, 0.0, self.sw as f64, self.sh as f64);
        }
        let s = (self.sw as f64 / gw as f64).min(self.sh as f64 / gh as f64);
        let (rw, rh) = (gw as f64 * s, gh as f64 * s);
        (
            (self.sw as f64 - rw) / 2.0,
            (self.sh as f64 - rh) / 2.0,
            rw,
            rh,
        )
    }
    pub fn guest(&self) -> (u32, u32) {
        if self.gw == 0 || self.gh == 0 {
            (self.sw.max(1), self.sh.max(1))
        } else {
            (self.gw, self.gh)
        }
    }
    /// Stream pixel → guest pixel, clamped into the picture.
    pub fn map_to_guest(&self, x: f64, y: f64) -> (i32, i32) {
        let (rx, ry, rw, rh) = self.rect();
        let (gw, gh) = self.guest();
        let gx = ((x - rx) * gw as f64 / rw)
            .floor()
            .clamp(0.0, gw as f64 - 1.0);
        let gy = ((y - ry) * gh as f64 / rh)
            .floor()
            .clamp(0.0, gh as f64 - 1.0);
        (gx as i32, gy as i32)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u32,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

#[derive(Debug)]
pub struct InputState {
    pub geo: Geometry,
    keys: HashSet<u16>,
    buttons: HashSet<u16>,
    wheel_acc: i32,
    hwheel_acc: i32,
    /// The guest pointer position as we last set it (guest pixels).
    pub cursor: (i32, i32),
    /// The guest shows a cursor (desktop): integrate relative motion into
    /// absolute positions so the composited cursor is exactly where the guest
    /// thinks the pointer is. Hidden (games): pass relative motion through.
    pub cursor_visible: bool,
    pads: [Option<PadState>; MAX_PADS],
    pub pads_enabled: bool,
}

impl Default for InputState {
    fn default() -> Self {
        InputState {
            geo: Geometry {
                sw: 1920,
                sh: 1080,
                gw: 0,
                gh: 0,
            },
            keys: HashSet::new(),
            buttons: HashSet::new(),
            wheel_acc: 0,
            hwheel_acc: 0,
            cursor: (0, 0),
            cursor_visible: false,
            pads: [None; MAX_PADS],
            pads_enabled: true,
        }
    }
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn le32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn be16(b: &[u8], o: usize) -> Option<i16> {
    Some(i16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn le16(b: &[u8], o: usize) -> Option<i16> {
    Some(i16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

/// Windows virtual-key code → Linux KEY_*.
pub fn vk_to_evdev(vk: u8) -> Option<u16> {
    Some(match vk {
        0x08 => 14,  // BACK
        0x09 => 15,  // TAB
        0x0C => 111, // CLEAR → DELETE (numpad 5 without numlock is 76; CLEAR is rare)
        0x0D => 28,  // RETURN
        0x10 => 42,  // SHIFT
        0x11 => 29,  // CONTROL
        0x12 => 56,  // MENU (Alt)
        0x13 => 119, // PAUSE
        0x14 => 58,  // CAPITAL
        0x1B => 1,   // ESCAPE
        0x20 => 57,  // SPACE
        0x21 => 104, // PRIOR
        0x22 => 109, // NEXT
        0x23 => 107, // END
        0x24 => 102, // HOME
        0x25 => 105, // LEFT
        0x26 => 103, // UP
        0x27 => 106, // RIGHT
        0x28 => 108, // DOWN
        0x2C => 99,  // SNAPSHOT → SYSRQ
        0x2D => 110, // INSERT
        0x2E => 111, // DELETE
        0x2F => 138, // HELP
        0x30 => 11,
        0x31..=0x39 => 2 + (vk - 0x31) as u16,
        0x41..=0x5A => [
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47,
            17, 45, 21, 44,
        ][(vk - 0x41) as usize],
        0x5B => 125, // LWIN
        0x5C => 126, // RWIN
        0x5D => 127, // APPS → COMPOSE
        0x5F => 142, // SLEEP
        0x60 => 82,
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73,
        0x6A => 55,                              // MULTIPLY
        0x6B => 78,                              // ADD
        0x6C => 121,                             // SEPARATOR → KPCOMMA
        0x6D => 74,                              // SUBTRACT
        0x6E => 83,                              // DECIMAL
        0x6F => 98,                              // DIVIDE
        0x70..=0x79 => 59 + (vk - 0x70) as u16,  // F1..F10
        0x7A => 87,                              // F11
        0x7B => 88,                              // F12
        0x7C..=0x87 => 183 + (vk - 0x7C) as u16, // F13..F24
        0x90 => 69,                              // NUMLOCK
        0x91 => 70,                              // SCROLL
        0xA0 => 42,                              // LSHIFT
        0xA1 => 54,                              // RSHIFT
        0xA2 => 29,                              // LCONTROL
        0xA3 => 97,                              // RCONTROL
        0xA4 => 56,                              // LMENU
        0xA5 => 100,                             // RMENU (AltGr)
        0xA6 => 158,                             // BROWSER_BACK
        0xA7 => 159,                             // BROWSER_FORWARD
        0xA8 => 173,                             // BROWSER_REFRESH
        0xA9 => 128,                             // BROWSER_STOP
        0xAA => 217,                             // BROWSER_SEARCH
        0xAB => 156,                             // BROWSER_FAVORITES
        0xAC => 172,                             // BROWSER_HOME
        0xAD => 113,                             // VOLUME_MUTE
        0xAE => 114,                             // VOLUME_DOWN
        0xAF => 115,                             // VOLUME_UP
        0xB0 => 163,                             // MEDIA_NEXT_TRACK
        0xB1 => 165,                             // MEDIA_PREV_TRACK
        0xB2 => 166,                             // MEDIA_STOP
        0xB3 => 164,                             // MEDIA_PLAY_PAUSE
        0xBA => 39,                              // OEM_1 ;:
        0xBB => 13,                              // OEM_PLUS =+
        0xBC => 51,                              // OEM_COMMA
        0xBD => 12,                              // OEM_MINUS
        0xBE => 52,                              // OEM_PERIOD
        0xBF => 53,                              // OEM_2 /?
        0xC0 => 41,                              // OEM_3 `~
        0xDB => 26,                              // OEM_4 [{
        0xDC => 43,                              // OEM_5 \|
        0xDD => 27,                              // OEM_6 ]}
        0xDE => 40,                              // OEM_7 '"
        0xE2 => 86,                              // OEM_102 <>
        _ => return None,
    })
}

// Moonlight (XInput layout) button flags.
const PAD_BUTTONS: &[(u32, u16)] = &[
    (0x1000, 0x130),   // A → BTN_SOUTH
    (0x2000, 0x131),   // B → BTN_EAST
    (0x4000, 0x133),   // X → BTN_X (= BTN_NORTH, as xpad reports it)
    (0x8000, 0x134),   // Y → BTN_Y (= BTN_WEST)
    (0x0100, 0x136),   // LB → BTN_TL
    (0x0200, 0x137),   // RB → BTN_TR
    (0x0020, 0x13a),   // BACK → BTN_SELECT
    (0x0010, 0x13b),   // PLAY → BTN_START
    (0x0400, 0x13c),   // SPECIAL (guide) → BTN_MODE
    (0x0040, 0x13d),   // LS_CLK → BTN_THUMBL
    (0x0080, 0x13e),   // RS_CLK → BTN_THUMBR
    (0x010000, 0x2c0), // PADDLE1 → BTN_TRIGGER_HAPPY1
    (0x020000, 0x2c1),
    (0x040000, 0x2c2),
    (0x080000, 0x2c3),
    (0x100000, 0x2c4), // TOUCHPAD
    (0x200000, 0x2c5), // MISC (share)
];
const DPAD_UP: u32 = 0x1;
const DPAD_DOWN: u32 = 0x2;
const DPAD_LEFT: u32 = 0x4;
const DPAD_RIGHT: u32 = 0x8;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

fn pad_ev(pad: usize, ty: u16, code: u16, value: i32) -> Pkt {
    Pkt::new(
        broker::EV_PAD,
        code as i32,
        value,
        ((pad as u32) << 16) | ty as u32,
        0,
    )
}

fn hat(neg: bool, pos: bool) -> i32 {
    match (neg, pos) {
        (true, false) => -1,
        (false, true) => 1,
        _ => 0,
    }
}

fn inv(v: i16) -> i32 {
    // XInput: +Y is up; evdev: +Y is down.
    -(v as i32).max(-32767)
}

impl InputState {
    pub fn set_geometry(&mut self, g: Geometry) {
        self.geo = g;
    }

    fn key(&mut self, code: u16, down: bool, out: &mut Vec<Pkt>) {
        if down {
            self.keys.insert(code);
        } else if !self.keys.remove(&code) {
            return; // never pressed here: nothing to release
        }
        out.push(Pkt::new(broker::EV_KEY, code as i32, down as i32, 0, 0));
    }

    fn button(&mut self, code: u16, down: bool, out: &mut Vec<Pkt>) {
        if down {
            self.buttons.insert(code);
        } else {
            self.buttons.remove(&code);
        }
        out.push(Pkt::new(broker::EV_BTN, code as i32, down as i32, 0, 0));
    }

    fn abs_guest(&mut self, gx: i32, gy: i32, out: &mut Vec<Pkt>) {
        let (gw, gh) = self.geo.guest();
        self.cursor = (gx, gy);
        out.push(Pkt::new(broker::EV_ABS, gx, gy, gw, gh));
    }

    /// Release everything held (session end, focus loss).
    pub fn release_all(&mut self) -> Vec<Pkt> {
        let mut out = Vec::new();
        for k in std::mem::take(&mut self.keys) {
            out.push(Pkt::new(broker::EV_KEY, k as i32, 0, 0, 0));
        }
        for b in std::mem::take(&mut self.buttons) {
            out.push(Pkt::new(broker::EV_BTN, b as i32, 0, 0, 0));
        }
        for i in 0..MAX_PADS {
            if self.pads[i].is_some() {
                self.pad_update(i, PadState::default(), &mut out);
                self.pads[i] = None;
            }
        }
        out
    }

    fn pad_update(&mut self, i: usize, n: PadState, out: &mut Vec<Pkt>) {
        let o = self.pads[i].unwrap_or_default();
        let start = out.len();
        for &(flag, code) in PAD_BUTTONS {
            if (o.buttons ^ n.buttons) & flag != 0 {
                out.push(pad_ev(i, EV_KEY, code, (n.buttons & flag != 0) as i32));
            }
        }
        let hx = |b: u32| hat(b & DPAD_LEFT != 0, b & DPAD_RIGHT != 0);
        let hy = |b: u32| hat(b & DPAD_UP != 0, b & DPAD_DOWN != 0);
        if hx(o.buttons) != hx(n.buttons) {
            out.push(pad_ev(i, EV_ABS, ABS_HAT0X, hx(n.buttons)));
        }
        if hy(o.buttons) != hy(n.buttons) {
            out.push(pad_ev(i, EV_ABS, ABS_HAT0Y, hy(n.buttons)));
        }
        if o.lt != n.lt {
            out.push(pad_ev(i, EV_ABS, ABS_Z, n.lt as i32));
        }
        if o.rt != n.rt {
            out.push(pad_ev(i, EV_ABS, ABS_RZ, n.rt as i32));
        }
        if o.lx != n.lx {
            out.push(pad_ev(i, EV_ABS, ABS_X, n.lx as i32));
        }
        if o.ly != n.ly {
            out.push(pad_ev(i, EV_ABS, ABS_Y, inv(n.ly)));
        }
        if o.rx != n.rx {
            out.push(pad_ev(i, EV_ABS, ABS_RX, n.rx as i32));
        }
        if o.ry != n.ry {
            out.push(pad_ev(i, EV_ABS, ABS_RY, inv(n.ry)));
        }
        if out.len() > start {
            out.push(pad_ev(i, EV_SYN, SYN_REPORT, 0));
        }
        self.pads[i] = Some(n);
    }

    /// Translate one input packet (starting at its NV_INPUT_HEADER).
    pub fn packet(&mut self, p: &[u8]) -> Vec<Pkt> {
        let mut out = Vec::new();
        let (Some(size), Some(magic)) = (be32(p, 0), le32(p, 4)) else {
            return out;
        };
        if (size as usize) + 4 > p.len() || size < 4 {
            return out;
        }
        let b = &p[8..4 + size as usize];
        match magic {
            KEY_DOWN | KEY_UP => {
                // flags u8, keyCode i16 LE (0x80xx), modifiers u8, zero i16
                let (Some(&_flags), Some(code)) = (b.first(), le16(b, 1)) else {
                    return out;
                };
                if let Some(k) = vk_to_evdev((code as u16 & 0xff) as u8) {
                    self.key(k, magic == KEY_DOWN, &mut out);
                }
            }
            MOUSE_ABS => {
                let (Some(x), Some(y), Some(w), Some(h)) =
                    (be16(b, 0), be16(b, 2), be16(b, 6), be16(b, 8))
                else {
                    return out;
                };
                // width/height are the client's reference size minus one.
                let (w, h) = (
                    (w as i32).max(1) as f64 + 1.0,
                    (h as i32).max(1) as f64 + 1.0,
                );
                let sx = (x as f64 + 0.5) * self.geo.sw as f64 / w;
                let sy = (y as f64 + 0.5) * self.geo.sh as f64 / h;
                let (gx, gy) = self.geo.map_to_guest(sx, sy);
                self.abs_guest(gx, gy, &mut out);
            }
            MOUSE_REL | MOUSE_REL_GEN5 => {
                let (Some(dx), Some(dy)) = (be16(b, 0), be16(b, 2)) else {
                    return out;
                };
                if self.cursor_visible {
                    let (gw, gh) = self.geo.guest();
                    let gx = (self.cursor.0 + dx as i32).clamp(0, gw as i32 - 1);
                    let gy = (self.cursor.1 + dy as i32).clamp(0, gh as i32 - 1);
                    self.abs_guest(gx, gy, &mut out);
                } else {
                    out.push(Pkt::new(broker::EV_REL, dx as i32, dy as i32, 0, 0));
                }
            }
            MOUSE_BTN_DOWN | MOUSE_BTN_UP if b.len() == 1 => {
                let code = match b[0] {
                    1 => 0x110, // BTN_LEFT
                    2 => 0x112, // BTN_MIDDLE
                    3 => 0x111, // BTN_RIGHT
                    4 => 0x113, // BTN_SIDE (X1)
                    5 => 0x114, // BTN_EXTRA (X2)
                    _ => return out,
                };
                self.button(code, magic == MOUSE_BTN_DOWN, &mut out);
            }
            SCROLL if b.len() >= 6 => {
                let Some(amt) = be16(b, 0) else { return out };
                self.wheel_acc += amt as i32;
                let n = self.wheel_acc / 120;
                if n != 0 {
                    self.wheel_acc -= n * 120;
                    out.push(Pkt::new(broker::EV_WHEEL, n, 0, 0, 0));
                }
            }
            SS_HSCROLL => {
                let Some(amt) = be16(b, 0) else { return out };
                self.hwheel_acc += amt as i32;
                let n = self.hwheel_acc / 120;
                if n != 0 {
                    self.hwheel_acc -= n * 120;
                    out.push(Pkt::new(broker::EV_WHEEL, 0, n, 0, 0));
                }
            }
            MULTI_CONTROLLER | MULTI_CONTROLLER_NEW if self.pads_enabled => {
                // headerB, controllerNumber, activeGamepadMask, midB, buttonFlags (LE i16)
                // lt, rt (u8), lx, ly, rx, ry (LE i16), tailA, buttonFlags2, tailB
                let (Some(num), Some(mask), Some(bf), Some(lx), Some(ly), Some(rx), Some(ry)) = (
                    le16(b, 2),
                    le16(b, 4),
                    le16(b, 8),
                    le16(b, 12),
                    le16(b, 14),
                    le16(b, 16),
                    le16(b, 18),
                ) else {
                    return out;
                };
                let (Some(&lt), Some(&rt)) = (b.get(10), b.get(11)) else {
                    return out;
                };
                let bf2 = le16(b, 22).unwrap_or(0);
                let i = num as usize & 0xf;
                if i >= MAX_PADS {
                    return out;
                }
                // Pads that left (not in the mask) are released.
                for j in 0..MAX_PADS {
                    if mask as u16 & (1 << j) == 0 && self.pads[j].is_some() {
                        self.pad_update(j, PadState::default(), &mut out);
                        self.pads[j] = None;
                    }
                }
                if mask as u16 & (1 << i) == 0 {
                    return out;
                }
                let n = PadState {
                    buttons: (bf as u16 as u32) | ((bf2 as u16 as u32) << 16),
                    lt,
                    rt,
                    lx,
                    ly,
                    rx,
                    ry,
                };
                self.pad_update(i, n, &mut out);
            }
            SS_CONTROLLER_ARRIVAL => {
                if let Some(&n) = b.first() {
                    log::info!("input: controller {n} connected (type {:?})", b.get(1));
                }
            }
            UTF8_TEXT | SS_TOUCH | SCROLL_OLD => {}
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(&magic.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn st() -> InputState {
        let mut s = InputState::default();
        s.set_geometry(Geometry {
            sw: 1920,
            sh: 1080,
            gw: 1920,
            gh: 1080,
        });
        s
    }

    #[test]
    fn keyboard_maps_vk_to_evdev_and_tracks_held_keys() {
        let mut s = st();
        let ev = s.packet(&pkt(KEY_DOWN, &[0, 0x41, 0x80, 0, 0, 0])); // 'A'
        assert_eq!(ev, vec![Pkt::new(broker::EV_KEY, 30, 1, 0, 0)]);
        let ev = s.packet(&pkt(KEY_DOWN, &[0, 0xA2, 0x80, 0, 0, 0])); // LCTRL
        assert_eq!(ev[0].x, 29);
        let rel = s.release_all();
        assert_eq!(rel.len(), 2);
        assert!(rel.iter().all(|p| p.y == 0));
        // an UP for a key never pressed is dropped
        assert!(s.packet(&pkt(KEY_UP, &[0, 0x41, 0x80, 0, 0, 0])).is_empty());
        assert_eq!(vk_to_evdev(0x5A), Some(44)); // Z
        assert_eq!(vk_to_evdev(0x7B), Some(88)); // F12
        assert_eq!(vk_to_evdev(0x30), Some(11)); // 0
        assert_eq!(vk_to_evdev(0x39), Some(10)); // 9
    }

    #[test]
    fn absolute_mouse_scales_into_the_letterboxed_picture() {
        let mut s = InputState::default();
        // 2560x1440 stream showing a 1280x1024 guest: pillarboxed
        s.set_geometry(Geometry {
            sw: 2560,
            sh: 1440,
            gw: 1280,
            gh: 1024,
        });
        let mut b = Vec::new();
        b.extend_from_slice(&1279i16.to_be_bytes()); // x (client 1280x720 reference)
        b.extend_from_slice(&0i16.to_be_bytes());
        b.extend_from_slice(&0i16.to_be_bytes());
        b.extend_from_slice(&1279i16.to_be_bytes());
        b.extend_from_slice(&719i16.to_be_bytes());
        let ev = s.packet(&pkt(MOUSE_ABS, &b));
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (ev[0].ty, ev[0].x, ev[0].y, ev[0].w0, ev[0].w1),
            (broker::EV_ABS, 1279, 0, 1280, 1024)
        );
    }

    #[test]
    fn relative_motion_is_absolute_while_the_guest_shows_a_cursor() {
        let mut s = st();
        let mut b = (5i16).to_be_bytes().to_vec();
        b.extend_from_slice(&(-3i16).to_be_bytes());
        let ev = s.packet(&pkt(MOUSE_REL_GEN5, &b));
        assert_eq!(ev[0].ty, broker::EV_REL);
        s.cursor_visible = true;
        s.cursor = (100, 100);
        let ev = s.packet(&pkt(MOUSE_REL_GEN5, &b));
        assert_eq!((ev[0].ty, ev[0].x, ev[0].y), (broker::EV_ABS, 105, 97));
        assert_eq!(s.cursor, (105, 97));
    }

    #[test]
    fn buttons_and_high_resolution_wheel() {
        let mut s = st();
        assert_eq!(s.packet(&pkt(MOUSE_BTN_DOWN, &[3]))[0].x, 0x111);
        assert_eq!(s.packet(&pkt(MOUSE_BTN_UP, &[3]))[0].y, 0);
        let half = [&60i16.to_be_bytes()[..], &[0, 0, 0, 0]].concat();
        assert!(s.packet(&pkt(SCROLL, &half)).is_empty());
        assert_eq!(
            s.packet(&pkt(SCROLL, &half))[0],
            Pkt::new(broker::EV_WHEEL, 1, 0, 0, 0)
        );
        let down = [&(-240i16).to_be_bytes()[..], &[0, 0, 0, 0]].concat();
        assert_eq!(s.packet(&pkt(SCROLL, &down))[0].x, -2);
    }

    fn pad_body(num: i16, mask: i16, buttons: u32, lt: u8, lx: i16, ly: i16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0x1Ai16.to_le_bytes());
        b.extend_from_slice(&num.to_le_bytes());
        b.extend_from_slice(&mask.to_le_bytes());
        b.extend_from_slice(&0x14i16.to_le_bytes());
        b.extend_from_slice(&(buttons as u16).to_le_bytes());
        b.push(lt);
        b.push(0);
        b.extend_from_slice(&lx.to_le_bytes());
        b.extend_from_slice(&ly.to_le_bytes());
        b.extend_from_slice(&0i16.to_le_bytes());
        b.extend_from_slice(&0i16.to_le_bytes());
        b.extend_from_slice(&0x9Ci16.to_le_bytes());
        b.extend_from_slice(&((buttons >> 16) as u16).to_le_bytes());
        b.extend_from_slice(&0x55i16.to_le_bytes());
        b
    }

    #[test]
    fn gamepad_state_becomes_evdev_deltas() {
        let mut s = st();
        let ev = s.packet(&pkt(
            MULTI_CONTROLLER,
            &pad_body(0, 1, 0x1000 | DPAD_UP, 255, 1000, 32767),
        ));
        let want = vec![
            pad_ev(0, EV_KEY, 0x130, 1),
            pad_ev(0, EV_ABS, ABS_HAT0Y, -1),
            pad_ev(0, EV_ABS, ABS_Z, 255),
            pad_ev(0, EV_ABS, ABS_X, 1000),
            pad_ev(0, EV_ABS, ABS_Y, -32767),
            pad_ev(0, EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(ev, want);
        // nothing changed: nothing sent
        assert!(s
            .packet(&pkt(
                MULTI_CONTROLLER,
                &pad_body(0, 1, 0x1000 | DPAD_UP, 255, 1000, 32767)
            ))
            .is_empty());
        // pad 0 leaves: everything released
        let ev = s.packet(&pkt(MULTI_CONTROLLER, &pad_body(1, 2, 0, 0, 0, 0)));
        assert!(ev.contains(&pad_ev(0, EV_KEY, 0x130, 0)));
        assert!(ev.contains(&pad_ev(0, EV_ABS, ABS_Z, 0)));
    }

    #[test]
    fn short_or_lying_packets_are_ignored() {
        let mut s = st();
        assert!(s.packet(&[0, 0, 0]).is_empty());
        let mut p = pkt(KEY_DOWN, &[0, 0x41, 0x80, 0, 0, 0]);
        p[3] = 200; // size beyond the buffer
        assert!(s.packet(&p).is_empty());
        assert!(s.packet(&pkt(MULTI_CONTROLLER, &[1, 2, 3])).is_empty());
        assert!(s.packet(&pkt(MOUSE_ABS, &[1])).is_empty());
    }
}

// SPDX-License-Identifier: Apache-2.0
//
// The few RFB 3.8 messages the boot console speaks (RFC 6143), and QEMU's
// Extended Key Event and LED State extensions. Encoding is plain byte
// layout; decoding works on a growing buffer and says how much it needs,
// so the console thread never blocks on a half-arrived message.

use std::io;
use std::ops::Range;

pub const VERSION: &[u8; 12] = b"RFB 003.008\n";

/// Security type "None".
pub const SEC_NONE: u8 = 1;

pub const ENC_RAW: i32 = 0;
/// Pseudo-encoding: the server may change the framebuffer size.
pub const ENC_DESKTOP_SIZE: i32 = -223;
/// Pseudo-encoding: QEMU Extended Key Event (raw key numbers).
pub const ENC_QEMU_EXT_KEY: i32 = -258;
/// Pseudo-encoding: QEMU LED State. Advertising it turns QEMU's lock-key
/// "sync" off, which otherwise presses Caps/Num Lock on the guest when a
/// keysym's case or keypad-ness disagrees with its own idea of the state.
pub const ENC_LED_STATE: i32 = -261;

/// What the console asks for, in this order.
pub const ENCODINGS: [i32; 4] = [ENC_RAW, ENC_DESKTOP_SIZE, ENC_QEMU_EXT_KEY, ENC_LED_STATE];

/// Bytes per pixel of the format [`set_pixel_format`] asks for.
pub const BPP: usize = 4;

/// The largest framebuffer accepted (the viewer's own bound).
pub const MAX_DIM: u16 = 8192;

/// Server -> client message types.
const S_UPDATE: u8 = 0;
const S_COLOURMAP: u8 = 1;
const S_BELL: u8 = 2;
const S_CUT_TEXT: u8 = 3;

/// Server cut text beyond this ends the connection rather than being buffered.
const CUT_TEXT_MAX: u32 = 1 << 24;

/// SetPixelFormat: 32 bpp, depth 24, little-endian true colour with red at
/// bit 16, green at 8, blue at 0 -- in memory B, G, R, X: DRM XRGB8888.
pub fn set_pixel_format() -> [u8; 20] {
    let mut m = [0u8; 20];
    m[0] = 0; // type; 1..4 padding
    m[4] = 32; // bits per pixel
    m[5] = 24; // depth
    m[6] = 0; // big-endian: no
    m[7] = 1; // true colour
    m[8..10].copy_from_slice(&255u16.to_be_bytes());
    m[10..12].copy_from_slice(&255u16.to_be_bytes());
    m[12..14].copy_from_slice(&255u16.to_be_bytes());
    m[14] = 16; // red shift
    m[15] = 8; // green shift
    m[16] = 0; // blue shift
    m
}

pub fn set_encodings(encs: &[i32]) -> Vec<u8> {
    let mut m = vec![2u8, 0];
    m.extend_from_slice(&(encs.len() as u16).to_be_bytes());
    for e in encs {
        m.extend_from_slice(&e.to_be_bytes());
    }
    m
}

pub fn update_request(incremental: bool, w: u16, h: u16) -> [u8; 10] {
    let mut m = [0u8; 10];
    m[0] = 3;
    m[1] = incremental as u8;
    // x, y = 0
    m[6..8].copy_from_slice(&w.to_be_bytes());
    m[8..10].copy_from_slice(&h.to_be_bytes());
    m
}

/// Plain KeyEvent, by keysym: only for a server without the QEMU extension.
pub fn key_event(down: bool, keysym: u32) -> [u8; 8] {
    let mut m = [0u8; 8];
    m[0] = 4;
    m[1] = down as u8;
    m[4..8].copy_from_slice(&keysym.to_be_bytes());
    m
}

/// QEMU Extended Key Event: message 255, submessage 0, the down flag, a
/// keysym (advisory) and the QEMU "qnum" key number.
pub fn qemu_key_event(down: bool, keysym: u32, qnum: u32) -> [u8; 12] {
    let mut m = [0u8; 12];
    m[0] = 255;
    m[1] = 0;
    m[2..4].copy_from_slice(&(down as u16).to_be_bytes());
    m[4..8].copy_from_slice(&keysym.to_be_bytes());
    m[8..12].copy_from_slice(&qnum.to_be_bytes());
    m
}

pub fn pointer_event(mask: u8, x: u16, y: u16) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[0] = 5;
    m[1] = mask;
    m[2..4].copy_from_slice(&x.to_be_bytes());
    m[4..6].copy_from_slice(&y.to_be_bytes());
    m
}

/// One rectangle of a FramebufferUpdate. For Raw, `data` is its pixels in
/// the buffer that was parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub enc: i32,
    pub data: Range<usize>,
}

/// One server message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMsg {
    Update(Vec<Rect>),
    /// Colour map, bell, cut text: nothing to do.
    Ignored,
}

fn proto(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

/// Parse one server message from the front of `b`. `Ok(None)`: not all of
/// it is here yet. `Ok(Some((msg, n)))`: it took the first `n` bytes.
/// An encoding we did not ask for, or nonsense, is an error: the stream has
/// no way to resynchronise.
pub fn parse(b: &[u8]) -> io::Result<Option<(ServerMsg, usize)>> {
    let Some(&ty) = b.first() else {
        return Ok(None);
    };
    match ty {
        S_UPDATE => {
            if b.len() < 4 {
                return Ok(None);
            }
            let n = be16(b, 2) as usize;
            let mut at = 4;
            let mut rects = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                if b.len() < at + 12 {
                    return Ok(None);
                }
                let (x, y, w, h) = (
                    be16(b, at),
                    be16(b, at + 2),
                    be16(b, at + 4),
                    be16(b, at + 6),
                );
                let enc = be32(b, at + 8) as i32;
                at += 12;
                let len = match enc {
                    ENC_RAW => w as usize * h as usize * BPP,
                    ENC_DESKTOP_SIZE | ENC_QEMU_EXT_KEY => 0,
                    ENC_LED_STATE => 1,
                    e => return Err(proto(format!("rectangle in encoding {e}, never asked for"))),
                };
                if b.len() < at + len {
                    return Ok(None);
                }
                rects.push(Rect {
                    x,
                    y,
                    w,
                    h,
                    enc,
                    data: at..at + len,
                });
                at += len;
            }
            Ok(Some((ServerMsg::Update(rects), at)))
        }
        S_COLOURMAP => {
            if b.len() < 6 {
                return Ok(None);
            }
            let len = 6 + 6 * be16(b, 4) as usize;
            Ok((b.len() >= len).then_some((ServerMsg::Ignored, len)))
        }
        S_BELL => Ok(Some((ServerMsg::Ignored, 1))),
        S_CUT_TEXT => {
            if b.len() < 8 {
                return Ok(None);
            }
            let n = be32(b, 4);
            if n > CUT_TEXT_MAX {
                return Err(proto(format!("server cut text of {n} bytes")));
            }
            let len = 8 + n as usize;
            Ok((b.len() >= len).then_some((ServerMsg::Ignored, len)))
        }
        t => Err(proto(format!("server message type {t}"))),
    }
}

/// ServerInit's size and name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub name: String,
}

/// The client side of the handshake on a blocking stream: version, security
/// None, ClientInit (shared), ServerInit, then our pixel format and
/// encodings.
pub fn handshake<S: io::Read + io::Write>(s: &mut S) -> io::Result<ServerInit> {
    let mut v = [0u8; 12];
    s.read_exact(&mut v)?;
    if &v[..4] != b"RFB " {
        return Err(proto("not an RFB server"));
    }
    // 3.8 or later speaks 3.8 with us; QEMU says 3.8.
    s.write_all(VERSION)?;
    let mut n = [0u8; 1];
    s.read_exact(&mut n)?;
    if n[0] == 0 {
        return Err(proto(format!("server refused: {}", read_reason(s)?)));
    }
    let mut types = vec![0u8; n[0] as usize];
    s.read_exact(&mut types)?;
    if !types.contains(&SEC_NONE) {
        return Err(proto(format!(
            "server wants authentication (security types {types:?}); the console speaks only None"
        )));
    }
    s.write_all(&[SEC_NONE])?;
    let mut res = [0u8; 4];
    s.read_exact(&mut res)?;
    if u32::from_be_bytes(res) != 0 {
        return Err(proto(format!("security failed: {}", read_reason(s)?)));
    }
    s.write_all(&[1])?; // ClientInit: shared
    let mut init = [0u8; 24];
    s.read_exact(&mut init)?;
    let (width, height) = (be16(&init, 0), be16(&init, 2));
    let name_len = be32(&init, 20);
    if name_len > 4096 {
        return Err(proto(format!("desktop name of {name_len} bytes")));
    }
    let mut name = vec![0u8; name_len as usize];
    s.read_exact(&mut name)?;
    let mut hello = set_pixel_format().to_vec();
    hello.extend_from_slice(&set_encodings(&ENCODINGS));
    s.write_all(&hello)?;
    Ok(ServerInit {
        width,
        height,
        name: String::from_utf8_lossy(&name).into_owned(),
    })
}

fn read_reason<S: io::Read>(s: &mut S) -> io::Result<String> {
    let mut l = [0u8; 4];
    s.read_exact(&mut l)?;
    let n = u32::from_be_bytes(l).min(4096) as usize;
    let mut r = vec![0u8; n];
    s.read_exact(&mut r)?;
    Ok(String::from_utf8_lossy(&r).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_have_the_rfb_layout() {
        let pf = set_pixel_format();
        assert_eq!(&pf[4..8], &[32, 24, 0, 1]);
        assert_eq!(&pf[14..17], &[16, 8, 0]);
        assert_eq!(
            set_encodings(&ENCODINGS),
            [
                2, 0, 0, 4, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x21, 0xff, 0xff, 0xfe, 0xfe, 0xff, 0xff,
                0xfe, 0xfb
            ]
        );
        assert_eq!(
            update_request(true, 640, 480),
            [3, 1, 0, 0, 0, 0, 2, 128, 1, 224]
        );
        assert_eq!(
            qemu_key_event(true, 0x61, 0x1e),
            [255, 0, 0, 1, 0, 0, 0, 0x61, 0, 0, 0, 0x1e]
        );
        assert_eq!(pointer_event(5, 1, 2), [5, 5, 0, 1, 0, 2]);
        assert_eq!(key_event(false, 0xff0d), [4, 0, 0, 0, 0, 0, 0xff, 0x0d]);
    }

    fn rect(x: u16, y: u16, w: u16, h: u16, enc: i32) -> Vec<u8> {
        let mut v = Vec::new();
        for f in [x, y, w, h] {
            v.extend_from_slice(&f.to_be_bytes());
        }
        v.extend_from_slice(&enc.to_be_bytes());
        v
    }

    #[test]
    fn updates_parse_only_when_whole() {
        let mut m = vec![0, 0, 0, 3];
        m.extend(rect(0, 0, 640, 480, ENC_DESKTOP_SIZE));
        m.extend(rect(1, 2, 2, 1, ENC_RAW));
        m.extend([1, 2, 3, 4, 5, 6, 7, 8]);
        m.extend(rect(0, 0, 0, 0, ENC_LED_STATE));
        m.push(4);
        for cut in 0..m.len() {
            assert_eq!(parse(&m[..cut]).unwrap(), None, "{cut} bytes");
        }
        let (msg, n) = parse(&m).unwrap().unwrap();
        assert_eq!(n, m.len());
        let ServerMsg::Update(r) = msg else { panic!() };
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].w, r[0].h, r[0].enc), (640, 480, ENC_DESKTOP_SIZE));
        assert_eq!(&m[r[1].data.clone()], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(m[r[2].data.clone()], [4]);

        let mut bad = vec![0, 0, 0, 1];
        bad.extend(rect(0, 0, 1, 1, 7)); // tight, never asked for
        assert!(parse(&bad).is_err());
        assert_eq!(parse(&[2, 9]).unwrap(), Some((ServerMsg::Ignored, 1)));
        assert_eq!(
            parse(&[3, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']).unwrap(),
            Some((ServerMsg::Ignored, 10))
        );
        assert!(parse(&[9]).is_err());
    }
}

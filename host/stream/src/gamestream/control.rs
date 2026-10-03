//! The control channel: ENet over UDP (Moonlight's ENet fork). With
//! `useReliableUdp = 13` every message is wrapped as
//!
//!   0x0001 LE16 | length LE16 | seq LE32 | tag(16) | AES-GCM(type LE16 | len LE16 | payload)
//!   IV = LE32(seq) ‖ 0 ‖ 'C'/'H' ‖ 'C'
//!
//! Input, IDR and reference-frame-invalidation requests, pings and loss
//! reports arrive here. ENet is not thread-safe: only this thread touches it;
//! others queue messages with `queue()`.

use crate::host::Host;
use crate::session::{self, Session};
use anyhow::{bail, Result};
use openssl::symm::{decrypt_aead, encrypt_aead, Cipher};
use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const T_ENCRYPTED: u16 = 0x0001;
pub const T_PING: u16 = 0x0200;
pub const T_LOSS_STATS: u16 = 0x0201;
pub const T_FRAME_STATS: u16 = 0x0204;
pub const T_INPUT: u16 = 0x0206;
pub const T_RFI: u16 = 0x0301;
pub const T_IDR: u16 = 0x0302;
pub const T_START_A: u16 = 0x0305;
pub const T_START_B: u16 = 0x0307;
pub const T_TERMINATION: u16 = 0x0109;
pub const T_RUMBLE: u16 = 0x010b;
pub const T_HDR: u16 = 0x010e;
pub const T_FEC_STATUS: u16 = 0x5502;
pub const T_LTR_ACK: u16 = 0x0350;

#[repr(C)]
struct EnetEvent {
    ty: i32,
    peer: *mut c_void,
    data: u32,
    channel: u8,
    packet: *const u8,
    len: usize,
    pkt: *mut c_void,
    addr: [c_char; 64],
    port: u16,
    local: [c_char; 64],
}

extern "C" {
    fn cs_enet_host(port: u16, max_peers: usize) -> *mut c_void;
    fn cs_enet_service(host: *mut c_void, timeout_ms: u32, ev: *mut EnetEvent) -> i32;
    fn cs_enet_packet_free(pkt: *mut c_void);
    fn cs_enet_send(
        peer: *mut c_void,
        channel: u8,
        data: *const u8,
        len: usize,
        reliable: i32,
    ) -> i32;
    fn cs_enet_flush(host: *mut c_void);
    fn cs_enet_disconnect(peer: *mut c_void, now: i32);
}

fn iv(seq: u32, who: u8) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = who;
    iv[11] = b'C';
    iv
}

/// Seal one control message (host → client).
pub fn seal(key: &[u8; 16], seq: u32, ty: u16, payload: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(4 + payload.len());
    plain.extend_from_slice(&ty.to_le_bytes());
    plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    plain.extend_from_slice(payload);
    let mut tag = [0u8; 16];
    let ct = encrypt_aead(
        Cipher::aes_128_gcm(),
        key,
        Some(&iv(seq, b'H')),
        &[],
        &plain,
        &mut tag,
    )
    .expect("AES-GCM");
    let mut o = Vec::with_capacity(8 + 16 + ct.len());
    o.extend_from_slice(&T_ENCRYPTED.to_le_bytes());
    o.extend_from_slice(&((4 + 16 + ct.len()) as u16).to_le_bytes());
    o.extend_from_slice(&seq.to_le_bytes());
    o.extend_from_slice(&tag);
    o.extend_from_slice(&ct);
    o
}

/// Open one encrypted control message; returns (type, payload).
pub fn open(key: &[u8; 16], msg: &[u8]) -> Result<(u16, Vec<u8>)> {
    if msg.len() < 8 + 16 + 4 {
        bail!("runt control message");
    }
    let ty = u16::from_le_bytes([msg[0], msg[1]]);
    let len = u16::from_le_bytes([msg[2], msg[3]]) as usize;
    if ty != T_ENCRYPTED || len + 4 != msg.len() {
        bail!("bad encrypted control header");
    }
    let seq = u32::from_le_bytes(msg[4..8].try_into().unwrap());
    let tag = &msg[8..24];
    let ct = &msg[24..];
    let p = decrypt_aead(
        Cipher::aes_128_gcm(),
        key,
        Some(&iv(seq, b'C')),
        &[],
        ct,
        tag,
    )
    .map_err(|_| anyhow::anyhow!("control message failed authentication"))?;
    if p.len() < 4 {
        bail!("short inner control message");
    }
    let ity = u16::from_le_bytes([p[0], p[1]]);
    if ity == T_ENCRYPTED {
        bail!("nested encrypted control message");
    }
    Ok((ity, p[4..].to_vec()))
}

/// Messages other threads want sent: (session id, type, payload).
#[derive(Default)]
pub struct Outbox {
    pub msgs: Mutex<Vec<(u32, u16, Vec<u8>)>>,
}

impl Outbox {
    pub fn queue(&self, session: u32, ty: u16, payload: Vec<u8>) {
        self.msgs.lock().unwrap().push((session, ty, payload));
    }
}

struct Peer {
    peer: *mut c_void,
    session: Arc<Session>,
    seq: u32,
}

fn dispatch(host: &Host, s: &Session, ty: u16, p: &[u8]) {
    s.touch();
    match ty {
        T_INPUT => {
            let evs = s.input.lock().unwrap().packet(p);
            if !evs.is_empty() {
                host.broker.send_all(&evs);
            }
        }
        T_IDR => {
            log::debug!("control: IDR requested");
            s.request_idr();
            host.broker.kick();
        }
        T_RFI if p.len() >= 16 => {
            let first = i64::from_le_bytes(p[0..8].try_into().unwrap());
            let last = i64::from_le_bytes(p[8..16].try_into().unwrap());
            log::debug!("control: frames {first}..{last} lost (RFI)");
            s.losses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if first >= 0 && last >= first {
                s.requests.lock().unwrap().rfi = Some((first as u64, last as u64));
            } else {
                s.request_idr();
            }
            host.broker.kick();
        }
        T_LOSS_STATS | T_FRAME_STATS | T_PING | T_START_A | T_START_B | T_FEC_STATUS
        | T_LTR_ACK => {}
        other => log::debug!("control: message {other:#06x} ({} bytes)", p.len()),
    }
}

pub fn serve(host: Arc<Host>, outbox: Arc<Outbox>) -> Result<()> {
    // SAFETY: creates an ENet host owned by this thread for its lifetime.
    let h = unsafe { cs_enet_host(host.ports.control, 8) };
    if h.is_null() {
        bail!("cannot listen on UDP port {} (control)", host.ports.control);
    }
    let mut peers: Vec<Peer> = Vec::new();
    let mut last_reap = Instant::now();
    loop {
        let mut ev: EnetEvent = unsafe { std::mem::zeroed() };
        // SAFETY: h is live; ev is a plain out-struct.
        let r = unsafe { cs_enet_service(h, 5, &mut ev) };
        if r > 0 {
            let addr = unsafe { std::ffi::CStr::from_ptr(ev.addr.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            match ev.ty {
                1 => {
                    let cur = host.current_session();
                    let ok = cur.as_ref().filter(|s| {
                        s.alive()
                            && (s.cfg.ml_flags & session::ML_FF_SESSION_ID_V1 == 0
                                || s.launch.connect_data == ev.data)
                    });
                    match ok {
                        Some(s) => {
                            log::info!("control: client {addr} connected (session {})", s.id);
                            peers.retain(|p| p.session.id != s.id || p.peer == ev.peer);
                            peers.push(Peer {
                                peer: ev.peer,
                                session: s.clone(),
                                seq: 0,
                            });
                            s.touch();
                        }
                        None => {
                            log::warn!("control: refused {addr}: no matching session");
                            unsafe { cs_enet_disconnect(ev.peer, 1) };
                        }
                    }
                }
                2 => {
                    if let Some(i) = peers.iter().position(|p| p.peer == ev.peer) {
                        let p = peers.remove(i);
                        log::info!("control: client {addr} disconnected");
                        session::end(&host, &p.session, "client disconnected");
                    }
                }
                3 => {
                    // SAFETY: packet/len describe the ENet packet until freed below.
                    let data = unsafe { std::slice::from_raw_parts(ev.packet, ev.len) }.to_vec();
                    unsafe { cs_enet_packet_free(ev.pkt) };
                    if let Some(p) = peers.iter().find(|p| p.peer == ev.peer) {
                        let s = &p.session;
                        if data.len() < 2 {
                            continue;
                        }
                        if s.cfg.control_v2 {
                            match open(&s.launch.rikey, &data) {
                                Ok((ty, payload)) => dispatch(&host, s, ty, &payload),
                                Err(e) => {
                                    log::warn!("control: {e}; dropping the client");
                                    session::end(&host, s, "control channel authentication failed");
                                }
                            }
                        } else {
                            // Legacy clients: plain type + payload (their input
                            // is separately encrypted; not supported).
                            let ty = u16::from_le_bytes([data[0], data[1]]);
                            if ty != T_INPUT {
                                dispatch(&host, s, ty, &data[2..]);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Outgoing, and peers whose session is over.
        let msgs: Vec<_> = std::mem::take(&mut *outbox.msgs.lock().unwrap());
        for (sid, ty, payload) in msgs {
            if let Some(p) = peers.iter_mut().find(|p| p.session.id == sid) {
                let m = if p.session.cfg.control_v2 {
                    p.seq += 1;
                    seal(&p.session.launch.rikey, p.seq, ty, &payload)
                } else {
                    let mut m = ty.to_le_bytes().to_vec();
                    m.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                    m.extend_from_slice(&payload);
                    m
                };
                // SAFETY: live peer of this host.
                unsafe { cs_enet_send(p.peer, 0, m.as_ptr(), m.len(), 1) };
            }
        }
        let before = peers.len();
        peers.retain(|p| {
            if p.session.alive() {
                return true;
            }
            // Ended by us (quit, a new session): tell the client, then let go.
            let mut payload = 0x8003_0023u32.to_be_bytes().to_vec(); // graceful
            payload.truncate(4);
            let m = if p.session.cfg.control_v2 {
                seal(&p.session.launch.rikey, p.seq + 1, T_TERMINATION, &payload)
            } else {
                payload
            };
            unsafe {
                cs_enet_send(p.peer, 0, m.as_ptr(), m.len(), 1);
                cs_enet_disconnect(p.peer, 0);
            }
            false
        });
        if before != peers.len() || r > 0 {
            unsafe { cs_enet_flush(h) };
        }
        if last_reap.elapsed() > Duration::from_secs(1) {
            last_reap = Instant::now();
            session::reap(&host, Duration::from_secs(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_open_and_ours_seal() {
        let key = [0x42u8; 16];
        // Build a client message the way moonlight-common-c does.
        let mut plain = T_IDR.to_le_bytes().to_vec();
        plain.extend_from_slice(&2u16.to_le_bytes());
        plain.extend_from_slice(&[0, 0]);
        let mut tag = [0u8; 16];
        let ct = encrypt_aead(
            Cipher::aes_128_gcm(),
            &key,
            Some(&iv(7, b'C')),
            &[],
            &plain,
            &mut tag,
        )
        .unwrap();
        let mut m = T_ENCRYPTED.to_le_bytes().to_vec();
        m.extend_from_slice(&((4 + 16 + ct.len()) as u16).to_le_bytes());
        m.extend_from_slice(&7u32.to_le_bytes());
        m.extend_from_slice(&tag);
        m.extend_from_slice(&ct);
        let (ty, p) = open(&key, &m).unwrap();
        assert_eq!((ty, p), (T_IDR, vec![0, 0]));
        // wrong direction (our own 'H' message) must not open as a client one
        let ours = seal(&key, 7, T_IDR, &[0, 0]);
        assert!(open(&key, &ours).is_err());
        // length lies are rejected
        let mut bad = m.clone();
        bad[2] = bad[2].wrapping_add(1);
        assert!(open(&key, &bad).is_err());
    }
}

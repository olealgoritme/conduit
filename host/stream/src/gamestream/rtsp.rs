//! RTSP over TCP, one request per connection (OPTIONS, DESCRIBE, SETUP x3,
//! ANNOUNCE, PLAY). With `rtspenc://` (client corever >= 1) every message is
//! AES-128-GCM with the launch key:
//!
//!   u32 BE (0x80000000 | length) | u32 BE sequence | 16 tag | ciphertext
//!   IV = LE32(sequence) ‖ 0 ‖ 'C'/'H' ‖ 'R'   (client / host originated)

use super::http::{self, Request};
use crate::host::{Host, Launch};
use crate::session::{self, StreamConfig, SS_ENC_AUDIO, SS_ENC_CONTROL_V2, SS_ENC_VIDEO};
use anyhow::{bail, Result};
use openssl::symm::{decrypt_aead, encrypt_aead, Cipher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ENC_BIT: u32 = 0x8000_0000;
static HOST_SEQ: AtomicU32 = AtomicU32::new(0);

pub fn serve(host: Arc<Host>, l: TcpListener) {
    for s in l.incoming().flatten() {
        let host = host.clone();
        std::thread::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = s.set_nodelay(true);
            if let Err(e) = conn(&host, s) {
                log::debug!("rtsp: {e:#}");
            }
        });
    }
}

fn iv(seq: u32, who: u8) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = who;
    iv[11] = b'R';
    iv
}

pub fn seal(key: &[u8; 16], seq: u32, plain: &[u8]) -> Vec<u8> {
    let mut tag = [0u8; 16];
    let ct = encrypt_aead(
        Cipher::aes_128_gcm(),
        key,
        Some(&iv(seq, b'H')),
        &[],
        plain,
        &mut tag,
    )
    .expect("AES-GCM");
    let mut o = Vec::with_capacity(24 + ct.len());
    o.extend_from_slice(&(ENC_BIT | ct.len() as u32).to_be_bytes());
    o.extend_from_slice(&seq.to_be_bytes());
    o.extend_from_slice(&tag);
    o.extend_from_slice(&ct);
    o
}

pub fn open(key: &[u8; 16], hdr: &[u8; 24], ct: &[u8]) -> Result<Vec<u8>> {
    let seq = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
    match decrypt_aead(
        Cipher::aes_128_gcm(),
        key,
        Some(&iv(seq, b'C')),
        &[],
        ct,
        &hdr[8..24],
    ) {
        Ok(p) => Ok(p),
        Err(_) => bail!("RTSP message failed authentication"),
    }
}

fn conn(host: &Arc<Host>, mut s: TcpStream) -> Result<()> {
    let launch = host.launch.lock().unwrap().clone();
    let mut first = [0u8; 1];
    s.peek(&mut first)?;
    let encrypted = first[0] & 0x80 != 0;
    let req = if encrypted {
        let Some(l) = &launch else {
            bail!("encrypted RTSP without a launch")
        };
        let mut hdr = [0u8; 24];
        s.read_exact(&mut hdr)?;
        let len = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) & !ENC_BIT;
        if len as usize > http::MAX_HEAD + http::MAX_BODY {
            bail!("RTSP message too large");
        }
        let mut ct = vec![0u8; len as usize];
        s.read_exact(&mut ct)?;
        let plain = open(&l.rikey, &hdr, &ct)?;
        let Some((r, _)) = http::parse(&plain) else {
            bail!("malformed RTSP")
        };
        r
    } else {
        if launch.as_ref().is_some_and(|l| l.encrypted_rtsp) {
            bail!("plaintext RTSP refused: the launch asked for encryption");
        }
        http::read_request(&mut s)?
    };
    log::debug!("rtsp {} {}", req.method, req.target);
    let (code, reason, headers, body) = match launch {
        None => (454, "Session Not Found", vec![], String::new()),
        Some(l) => handle(host, &l, &req, &s),
    };
    let cseq = req.header("cseq").unwrap_or("0").to_string();
    let mut msg = format!("RTSP/1.0 {code} {reason}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        msg.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        msg.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    msg.push_str("\r\n");
    msg.push_str(&body);
    let launch = host.launch.lock().unwrap().clone();
    if encrypted {
        let key = launch.map(|l| l.rikey).unwrap_or([0; 16]);
        let seq = HOST_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        s.write_all(&seal(&key, seq, msg.as_bytes()))?;
    } else {
        s.write_all(msg.as_bytes())?;
    }
    s.flush()?;
    Ok(())
}

/// The DESCRIBE body: what we can do.
pub fn describe(host: &Host, l: &Launch) -> String {
    use crate::gpu::Codec;
    let mut d = String::new();
    d.push_str("a=x-ss-general.featureFlags:0\n");
    let mut supported = SS_ENC_CONTROL_V2 | SS_ENC_AUDIO;
    if host.allow_video_encryption {
        supported |= SS_ENC_VIDEO;
    }
    d.push_str(&format!("a=x-ss-general.encryptionSupported:{supported}\n"));
    d.push_str(&format!(
        "a=x-ss-general.encryptionRequested:{SS_ENC_CONTROL_V2}\n"
    ));
    let rfi = [Codec::H264, Codec::Hevc, Codec::Av1]
        .iter()
        .all(|&c| !host.caps(c).supported || host.caps(c).rfi);
    if rfi {
        d.push_str("a=x-nv-video[0].refPicInvalidation:1\n");
    }
    if host.caps(Codec::Hevc).supported {
        d.push_str("sprop-parameter-sets=AAAAAU\n");
    }
    if host.caps(Codec::Av1).supported {
        d.push_str("a=rtpmap:98 AV1/90000\n");
    }
    if !l.surround_params.is_empty() {
        for _ in 0..2 {
            d.push_str(&format!(
                "a=fmtp:97 surround-params={}\n",
                l.surround_params
            ));
        }
    }
    // Opus layouts (channels, streams, coupled, mapping): stereo, 5.1, 7.1 in
    // normal and high quality, the way GameStream hosts list them.
    for p in [
        "21101",
        "21101",
        "642012453",
        "660012345",
        "85301245367",
        "88001234567",
    ] {
        d.push_str(&format!("a=fmtp:97 surround-params={p}\n"));
    }
    d
}

fn handle(
    host: &Arc<Host>,
    l: &Launch,
    req: &Request,
    s: &TcpStream,
) -> (u16, &'static str, Vec<(&'static str, String)>, String) {
    match req.method.as_str() {
        "OPTIONS" => (200, "OK", vec![], String::new()),
        "DESCRIBE" => (200, "OK", vec![], describe(host, l)),
        "SETUP" => {
            let t = req.target.split("streamid=").nth(1).unwrap_or("");
            let kind = t.split('/').next().unwrap_or("");
            let port = match kind {
                "audio" => host.ports.audio,
                "video" => host.ports.video,
                "control" => host.ports.control,
                _ => return (404, "Not Found", vec![], String::new()),
            };
            let mut h = vec![
                ("Session", "DEADBEEFCAFE;timeout = 90".to_string()),
                ("Transport", format!("server_port={port}")),
            ];
            if kind == "control" {
                h.push(("X-SS-Connect-Data", l.connect_data.to_string()));
            } else {
                h.push(("X-SS-Ping-Payload", l.ping_payload.clone()));
            }
            (200, "OK", h, String::new())
        }
        "ANNOUNCE" => {
            let body = String::from_utf8_lossy(&req.body);
            match StreamConfig::from_sdp(&body, host.fec_percent) {
                Ok(cfg) => {
                    if !host.caps(cfg.codec).supported {
                        log::warn!(
                            "rtsp: client asked for {} which NVENC lacks",
                            cfg.codec.name()
                        );
                        return (400, "Bad Request", vec![], String::new());
                    }
                    if cfg.encryption & SS_ENC_VIDEO != 0 && !host.allow_video_encryption {
                        return (403, "Forbidden", vec![], String::new());
                    }
                    let _ = s;
                    session::start(host, l.clone(), cfg);
                    (200, "OK", vec![], String::new())
                }
                Err(e) => {
                    log::warn!("rtsp: bad ANNOUNCE: {e}");
                    (400, "Bad Request", vec![], String::new())
                }
            }
        }
        "PLAY" => (200, "OK", vec![], String::new()),
        _ => (404, "Not Found", vec![], String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_messages_open_with_the_client_direction() {
        let key = [3u8; 16];
        // a client-originated message: same layout, 'C' in the IV
        let plain = b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let mut tag = [0u8; 16];
        let ct = encrypt_aead(
            Cipher::aes_128_gcm(),
            &key,
            Some(&iv(5, b'C')),
            &[],
            plain,
            &mut tag,
        )
        .unwrap();
        let mut hdr = [0u8; 24];
        hdr[0..4].copy_from_slice(&(ENC_BIT | ct.len() as u32).to_be_bytes());
        hdr[4..8].copy_from_slice(&5u32.to_be_bytes());
        hdr[8..].copy_from_slice(&tag);
        assert_eq!(open(&key, &hdr, &ct).unwrap(), plain);
        // tampering fails
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(open(&key, &hdr, &bad).is_err());
        // our replies use 'H'
        let sealed = seal(&key, 9, b"RTSP/1.0 200 OK\r\n\r\n");
        assert_eq!(
            u32::from_be_bytes(sealed[0..4].try_into().unwrap()) & ENC_BIT,
            ENC_BIT
        );
        let pt = decrypt_aead(
            Cipher::aes_128_gcm(),
            &key,
            Some(&iv(9, b'H')),
            &[],
            &sealed[24..],
            &sealed[8..24],
        )
        .unwrap();
        assert_eq!(pt, b"RTSP/1.0 200 OK\r\n\r\n");
    }
}

//! The conduit link: Conduit's own viewer over the network.
//!
//! Two TLS connections to the host's link port, both opened with a JSON hello
//! line carrying the link token:
//!
//!   video  host → client: [VideoHeader 32 B][bitstream]...   (one per frame)
//!   input  client → host: 24-byte broker event records        (the viewer's own)
//!
//! TCP rather than UDP + FEC: the link is for a LAN (10 GbE for lossless), and
//! a stream socket never delivers a damaged lossless frame. The client
//! (`conduit-stream connect`) decodes with NVDEC into GPU buffers and feeds the
//! unchanged local viewer through the same broker socket the backend uses, so
//! fullscreen, mode hints and input behave as they do locally.

use crate::broker::{self, Cmd, Pkt, PKT_SIZE};
use crate::gpu::{Codec, DecParams, Gpu};
use crate::host::{Host, Launch};
use crate::session::{self, Job, Sink, StreamConfig};
use anyhow::{anyhow, bail, Context, Result};
use openssl::hash::MessageDigest;
use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslStream, SslVerifyMode};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 48100;
const MAGIC: u32 = u32::from_le_bytes(*b"CSVF");
const HEADER: usize = 32;
const MAX_FRAME: usize = 64 << 20;
/// Client → host only: ask for an IDR (not forwarded to the VM).
pub const EV_LINK_IDR: u16 = 0x100;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub v: u32,
    pub token: String,
    /// "video" or "input"
    pub role: String,
    #[serde(default)]
    pub session: u32,
    #[serde(default)]
    pub codec: Option<Codec>,
    #[serde(default)]
    pub lossless: bool,
    #[serde(default)]
    pub chroma444: bool,
    #[serde(default)]
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub fps: u32,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelloReply {
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub session: u32,
    #[serde(default)]
    pub codec: Option<Codec>,
    #[serde(default)]
    pub lossless: bool,
    #[serde(default)]
    pub chroma444: bool,
    #[serde(default)]
    pub colorspace: u32,
    #[serde(default)]
    pub full_range: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoHeader {
    pub idr: bool,
    pub frame_index: u64,
    pub width: u32,
    pub height: u32,
    pub len: u32,
}

impl VideoHeader {
    pub fn encode(&self) -> [u8; HEADER] {
        let mut b = [0u8; HEADER];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&(self.idr as u32).to_le_bytes());
        b[8..16].copy_from_slice(&self.frame_index.to_le_bytes());
        b[16..20].copy_from_slice(&self.width.to_le_bytes());
        b[20..24].copy_from_slice(&self.height.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8; HEADER]) -> Result<VideoHeader> {
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        if u(0) != MAGIC {
            bail!("bad frame header");
        }
        let h = VideoHeader {
            idr: u(4) & 1 != 0,
            frame_index: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            width: u(16),
            height: u(20),
            len: u(24),
        };
        if h.len as usize > MAX_FRAME || h.width > 16384 || h.height > 16384 {
            bail!("frame header out of range");
        }
        Ok(h)
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && openssl::memcmp::eq(a, b)
}

/// The input events a remote viewer may send on to the VM.
pub fn allowed(p: &Pkt) -> bool {
    matches!(
        p.ty,
        broker::EV_KEY
            | broker::EV_BTN
            | broker::EV_ABS
            | broker::EV_REL
            | broker::EV_WHEEL
            | broker::EV_PAD
            | broker::EV_MODE_HINT
    )
}

fn read_line<S: Read>(s: &mut S) -> Result<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 4096 {
        s.read_exact(&mut b)?;
        if b[0] == b'\n' {
            return Ok(String::from_utf8(line)?);
        }
        line.push(b[0]);
    }
    bail!("hello too long")
}

// ------------------------------------------------------------------ host side

pub fn serve(host: Arc<Host>, port: u16, acc: Arc<SslAcceptor>) -> Result<()> {
    let l = crate::gamestream::nvhttp::listen(port)
        .with_context(|| format!("TCP port {port} (link)"))?;
    log::info!("conduit link on TCP port {port}");
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let host = host.clone();
            let acc = acc.clone();
            std::thread::spawn(move || {
                let peer = s.peer_addr().ok();
                let _ = s.set_nodelay(true);
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                match acc.accept(s) {
                    Ok(tls) => {
                        if let Err(e) = conn(&host, tls) {
                            log::info!("link {peer:?}: {e:#}");
                        }
                    }
                    Err(e) => log::debug!("link {peer:?}: TLS: {e}"),
                }
            });
        }
    });
    Ok(())
}

fn conn(host: &Arc<Host>, mut s: SslStream<TcpStream>) -> Result<()> {
    let hello: Hello = serde_json::from_str(&read_line(&mut s)?).context("hello")?;
    let reply = |s: &mut SslStream<TcpStream>, r: &HelloReply| -> Result<()> {
        s.write_all(serde_json::to_string(r)?.as_bytes())?;
        s.write_all(b"\n")?;
        s.flush()?;
        Ok(())
    };
    if !ct_eq(hello.token.as_bytes(), host.state.link_token.as_bytes()) {
        reply(
            &mut s,
            &HelloReply {
                ok: false,
                error: "wrong link token (see `conduit stream token` on the host)".into(),
                session: 0,
                codec: None,
                lossless: false,
                chroma444: false,
                colorspace: 0,
                full_range: false,
            },
        )?;
        bail!("wrong token");
    }
    match hello.role.as_str() {
        "video" => video_conn(host, s, hello, reply),
        "input" => input_conn(host, s, hello, reply),
        r => bail!("unknown role {r:?}"),
    }
}

fn video_conn(
    host: &Arc<Host>,
    mut s: SslStream<TcpStream>,
    hello: Hello,
    reply: impl Fn(&mut SslStream<TcpStream>, &HelloReply) -> Result<()>,
) -> Result<()> {
    let codec = hello.codec.unwrap_or(if hello.lossless {
        Codec::Hevc
    } else {
        host.preset.codec
    });
    let caps = host.caps(codec);
    let mut err = String::new();
    if !caps.supported {
        err = format!("this host's GPU cannot encode {}", codec.name());
    } else if hello.lossless && !caps.lossless {
        err = format!(
            "{} lossless is not available here (use --codec hevc)",
            codec.name()
        );
    }
    if !err.is_empty() {
        reply(
            &mut s,
            &HelloReply {
                ok: false,
                error: err.clone(),
                session: 0,
                codec: None,
                lossless: false,
                chroma444: false,
                colorspace: 0,
                full_range: false,
            },
        )?;
        bail!(err);
    }
    let fps = if hello.fps > 0 {
        hello.fps.min(1000)
    } else {
        host.preset.fps
    };
    let cfg = StreamConfig {
        width: 1920,
        height: 1080,
        fps,
        bitrate_kbps: if hello.bitrate_kbps > 0 {
            hello.bitrate_kbps
        } else {
            host.preset.bitrate_kbps
        },
        packet_size: 1392,
        codec,
        chroma444: hello.chroma444 || hello.lossless,
        colorspace: 1,
        full_range: true,
        min_fec_packets: 0,
        encryption: 0,
        ml_flags: 0,
        slices: 1,
        max_ref_frames: 0,
        audio_channels: 2,
        audio_packet_ms: 5,
        control_v2: false,
        hdr: false,
        lossless: hello.lossless,
        follow_guest: true,
    };
    let launch = Launch {
        id: host.new_id(),
        rikey: [0; 16],
        rikeyid: 0,
        width: 0,
        height: 0,
        fps,
        client_name: hello.name.clone(),
        ping_payload: String::new(),
        connect_data: 0,
        encrypted_rtsp: false,
        surround_params: String::new(),
        created: Instant::now(),
    };
    let (tx, rx) = sync_channel::<Job>(8);
    let r = HelloReply {
        ok: true,
        error: String::new(),
        session: launch.id,
        codec: Some(codec),
        lossless: cfg.lossless,
        chroma444: cfg.chroma444,
        colorspace: cfg.colorspace,
        full_range: cfg.full_range,
    };
    reply(&mut s, &r)?;
    log::info!(
        "link: {:?} connected: {} {}{} {} fps",
        hello.name,
        codec.name(),
        if cfg.lossless { "lossless" } else { "" },
        if cfg.chroma444 && !cfg.lossless {
            "4:4:4"
        } else {
            ""
        },
        fps
    );
    let sess = session::start_with(host, launch, cfg, Sink::Link(tx));
    let _ = s.get_ref().set_read_timeout(None);
    let res = (|| -> Result<()> {
        let mut t = Instant::now();
        let (mut n, mut bytes) = (0u64, 0u64);
        while sess.alive() {
            let j = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(j) => j,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            let h = VideoHeader {
                idr: j.idr,
                frame_index: j.frame_index as u64,
                width: j.width,
                height: j.height,
                len: j.data.len() as u32,
            };
            s.write_all(&h.encode())?;
            s.write_all(&j.data)?;
            sess.touch();
            n += 1;
            bytes += j.data.len() as u64;
            if t.elapsed() >= Duration::from_secs(5) {
                let el = t.elapsed().as_secs_f64();
                log::info!(
                    "link: {:.1} fps, {:.1} Mbit/s",
                    n as f64 / el,
                    bytes as f64 * 8.0 / el / 1e6
                );
                t = Instant::now();
                n = 0;
                bytes = 0;
            }
        }
        Ok(())
    })();
    session::end(host, &sess, "link viewer disconnected");
    res
}

fn input_conn(
    host: &Arc<Host>,
    mut s: SslStream<TcpStream>,
    hello: Hello,
    reply: impl Fn(&mut SslStream<TcpStream>, &HelloReply) -> Result<()>,
) -> Result<()> {
    let sess = host
        .current_session()
        .filter(|x| x.id == hello.session && matches!(x.sink, Sink::Link(_)))
        .ok_or_else(|| anyhow!("input for an unknown session"))?;
    reply(
        &mut s,
        &HelloReply {
            ok: true,
            error: String::new(),
            session: sess.id,
            codec: None,
            lossless: false,
            chroma444: false,
            colorspace: 0,
            full_range: false,
        },
    )?;
    let _ = s.get_ref().set_read_timeout(Some(Duration::from_secs(1)));
    let mut buf = [0u8; PKT_SIZE];
    while sess.alive() {
        match s.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        }
        let p = Pkt::decode(&buf);
        if p.ty == EV_LINK_IDR {
            sess.request_idr();
            host.broker.kick();
        } else if allowed(&p) {
            host.broker.send_all(&[p]);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ client side

fn known_path() -> PathBuf {
    crate::state::default_dir()
        .parent()
        .map(|p| p.join("known_streams"))
        .unwrap_or_else(|| PathBuf::from("known_streams"))
}

/// Trust on first use: pin the host's certificate per address.
fn check_known(addr: &str, fp: &str) -> Result<()> {
    let p = known_path();
    let s = std::fs::read_to_string(&p).unwrap_or_default();
    for line in s.lines() {
        if let Some((a, f)) = line.split_once(' ') {
            if a == addr {
                if f.trim() == fp {
                    return Ok(());
                }
                bail!(
                    "{addr} presented a different certificate than before ({fp}).\n\
                     If the host was reinstalled, remove its line from {} and connect again.",
                    p.display()
                );
            }
        }
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)?;
    writeln!(f, "{addr} {fp}")?;
    log::info!("link: trusting {addr} (certificate sha256 {fp}) from now on");
    Ok(())
}

fn connect_tls(addr: &str) -> Result<SslStream<TcpStream>> {
    let mut b = SslConnector::builder(SslMethod::tls_client())?;
    // The host's certificate is self-signed: it is pinned below instead.
    b.set_verify(SslVerifyMode::NONE);
    let c = b.build();
    let tcp = TcpStream::connect(addr).with_context(|| format!("connecting to {addr}"))?;
    tcp.set_nodelay(true)?;
    let mut cfg = c.configure()?;
    cfg.set_verify_hostname(false);
    cfg.set_use_server_name_indication(false);
    let s = cfg
        .connect("conduit", tcp)
        .map_err(|e| anyhow!("TLS with {addr}: {e}"))?;
    let cert = s
        .ssl()
        .peer_certificate()
        .ok_or_else(|| anyhow!("{addr} sent no certificate"))?;
    let fp = crate::state::hex(&cert.digest(MessageDigest::sha256())?).to_ascii_lowercase();
    check_known(addr, &fp)?;
    Ok(s)
}

fn hello(s: &mut SslStream<TcpStream>, h: &Hello) -> Result<HelloReply> {
    s.write_all(serde_json::to_string(h)?.as_bytes())?;
    s.write_all(b"\n")?;
    s.flush()?;
    let r: HelloReply = serde_json::from_str(&read_line(s)?).context("host reply")?;
    if !r.ok {
        bail!("{}", r.error);
    }
    Ok(r)
}

/// The local viewer, seen from the client side of the broker protocol.
struct Viewer {
    s: UnixStream,
}

fn send_with_fd(sock: RawFd, bytes: &[u8], fd: Option<RawFd>) -> std::io::Result<()> {
    let mut cbuf = [0u8; 64];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // SAFETY: msghdr points at live locals; one fd in SCM_RIGHTS when given.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
            *(libc::CMSG_DATA(c) as *mut RawFd) = fd;
        }
        let n = libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n as usize != bytes.len() {
            return Err(std::io::Error::other("short send to the viewer"));
        }
    }
    Ok(())
}

impl Viewer {
    fn connect(p: &Path) -> Result<Viewer> {
        let s = UnixStream::connect(p).with_context(|| format!("the viewer at {}", p.display()))?;
        Ok(Viewer { s })
    }
    fn cmd(&self, c: &Cmd, fd: Option<RawFd>) -> std::io::Result<()> {
        send_with_fd(self.s.as_raw_fd(), &c.encode(), fd)
    }
}

pub struct ConnectOpts {
    pub addr: String,
    pub token: String,
    pub socket: PathBuf,
    pub codec: Option<Codec>,
    pub lossless: bool,
    pub chroma444: bool,
    pub bitrate_kbps: u32,
    pub fps: u32,
}

/// Viewer events the decode thread cares about.
enum FromViewer {
    Format(u32, u64, bool),
    Gone,
}

pub fn connect(o: ConnectOpts) -> Result<()> {
    let name = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "conduit viewer".into());
    let g = Gpu::open(None)?;

    let mut video = connect_tls(&o.addr)?;
    let r = hello(
        &mut video,
        &Hello {
            v: 1,
            token: o.token.clone(),
            role: "video".into(),
            session: 0,
            codec: o.codec,
            lossless: o.lossless,
            chroma444: o.chroma444,
            bitrate_kbps: o.bitrate_kbps,
            fps: o.fps,
            name,
        },
    )?;
    let codec = r.codec.unwrap_or(Codec::Hevc);
    log::info!(
        "link: connected to {} ({}{})",
        o.addr,
        codec.name(),
        if r.lossless { ", lossless" } else { "" }
    );
    let mut input = connect_tls(&o.addr)?;
    hello(
        &mut input,
        &Hello {
            v: 1,
            token: o.token.clone(),
            role: "input".into(),
            session: r.session,
            codec: None,
            lossless: false,
            chroma444: false,
            bitrate_kbps: 0,
            fps: 0,
            name: String::new(),
        },
    )?;

    // The viewer: we are its client, exactly like the backend.
    let viewer = Viewer::connect(&o.socket)?;
    viewer.cmd(
        &Cmd {
            ty: broker::CMD_CAPS,
            ..Default::default()
        },
        None,
    )?;
    let (tx, rx) = channel::<FromViewer>();
    let input = Arc::new(Mutex::new(input));
    {
        let mut rd = viewer.s.try_clone()?;
        let input = input.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; PKT_SIZE];
            while rd.read_exact(&mut b).is_ok() {
                let p = Pkt::decode(&b);
                match p.ty {
                    broker::EV_FORMAT => {
                        let m = p.w0 as u64 | (p.w1 as u64) << 32;
                        let _ = tx.send(FromViewer::Format(p.y as u32, m, p.x == 1));
                    }
                    broker::EV_CLOSE => break,
                    _ if allowed(&p) => {
                        let mut i = input.lock().unwrap();
                        if i.write_all(&p.encode()).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let _ = tx.send(FromViewer::Gone);
        });
    }
    // Which output modifiers the viewer (its compositor) takes.
    let fourcc = crate::pipeline::FOURCCS[0];
    let candidates = g.modifiers(fourcc);
    for &m in &candidates {
        viewer.cmd(
            &Cmd {
                ty: broker::CMD_QUERY_FORMAT,
                fourcc,
                modifier: m,
                ..Default::default()
            },
            None,
        )?;
    }
    let mut ok_mods = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut answers = 0;
    while answers < candidates.len() && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(FromViewer::Format(_, m, ok)) => {
                answers += 1;
                if ok {
                    ok_mods.push(m);
                }
            }
            Ok(FromViewer::Gone) => bail!("the viewer closed"),
            Err(_) => {}
        }
    }
    log::info!(
        "link: viewer takes {} of {} buffer layouts",
        ok_mods.len(),
        candidates.len()
    );

    let mut dec = g.decoder(
        &DecParams {
            codec: codec as u32,
            width: 0,
            height: 0,
            chroma444: r.chroma444 as u32,
            lossless: r.lossless as u32,
            colorspace: r.colorspace,
            full_range: r.full_range as u32,
        },
        &ok_mods,
    )?;
    let _ = video.get_ref().set_read_timeout(None);
    let mut size = (0u32, 0u32);
    let mut stats_t = Instant::now();
    let (mut frames, mut bytes, mut dec_us) = (0u64, 0u64, 0u64);
    let mut buf = Vec::new();
    loop {
        if let Ok(FromViewer::Gone) = rx.try_recv() {
            log::info!("link: the viewer closed");
            return Ok(());
        }
        let mut hb = [0u8; HEADER];
        video
            .read_exact(&mut hb)
            .context("the host closed the link")?;
        let h = VideoHeader::decode(&hb)?;
        buf.resize(h.len as usize, 0);
        video.read_exact(&mut buf)?;
        let t0 = Instant::now();
        let f = match dec.decode(&buf) {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("{e:#}; asking for an IDR");
                let _ = input
                    .lock()
                    .unwrap()
                    .write_all(&Pkt::new(EV_LINK_IDR, 0, 0, 0, 0).encode());
                continue;
            }
        };
        dec_us += t0.elapsed().as_micros() as u64;
        if (f.width, f.height) != size {
            size = (f.width, f.height);
            viewer.cmd(
                &Cmd {
                    ty: broker::CMD_WINDOW,
                    width: f.width,
                    height: f.height,
                    ..Default::default()
                },
                None,
            )?;
        }
        let mut rec = Cmd {
            ty: broker::CMD_ATTACH,
            width: f.width,
            height: f.height,
            stride: f.stride,
            offset: f.offset,
            fourcc: f.fourcc,
            modifier: f.modifier,
            seq: h.frame_index as u32,
            ..Default::default()
        }
        .encode()
        .to_vec();
        rec.extend_from_slice(
            &Cmd {
                ty: broker::CMD_COMMIT,
                seq: h.frame_index as u32,
                ..Default::default()
            }
            .encode(),
        );
        if let Err(e) = send_with_fd(viewer.s.as_raw_fd(), &rec, Some(f.fd)) {
            bail!("the viewer: {e}");
        }
        frames += 1;
        bytes += h.len as u64;
        let el = stats_t.elapsed();
        if el >= Duration::from_secs(5) {
            log::info!(
                "link: {}x{} {:.1} fps, {:.1} Mbit/s, decode+convert {:.2} ms",
                size.0,
                size.1,
                frames as f64 / el.as_secs_f64(),
                bytes as f64 * 8.0 / el.as_secs_f64() / 1e6,
                dec_us as f64 / frames.max(1) as f64 / 1000.0
            );
            stats_t = Instant::now();
            frames = 0;
            bytes = 0;
            dec_us = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_header_round_trip_and_bounds() {
        let h = VideoHeader {
            idr: true,
            frame_index: 42,
            width: 5120,
            height: 1440,
            len: 123456,
        };
        assert_eq!(VideoHeader::decode(&h.encode()).unwrap(), h);
        let mut bad = h.encode();
        bad[0] ^= 1;
        assert!(VideoHeader::decode(&bad).is_err());
        let big = VideoHeader {
            len: (MAX_FRAME + 1) as u32,
            ..h
        };
        assert!(VideoHeader::decode(&big.encode()).is_err());
    }

    #[test]
    fn only_input_events_pass_to_the_vm() {
        assert!(allowed(&Pkt::new(broker::EV_KEY, 30, 1, 0, 0)));
        assert!(allowed(&Pkt::new(broker::EV_MODE_HINT, 1920, 1080, 0, 1)));
        assert!(!allowed(&Pkt::new(broker::EV_HELLO, 0, 0, 0, 0)));
        assert!(!allowed(&Pkt::new(broker::EV_FORMAT, 1, 0, 0, 0)));
        assert!(!allowed(&Pkt::new(EV_LINK_IDR, 0, 0, 0, 0)));
    }

    #[test]
    fn hello_is_json() {
        let h: Hello = serde_json::from_str(
            r#"{"v":1,"token":"t","role":"video","codec":"av1","lossless":true}"#,
        )
        .unwrap();
        assert_eq!(h.codec, Some(Codec::Av1));
        assert!(h.lossless);
    }
}

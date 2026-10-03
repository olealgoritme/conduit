//! The pipeline thread: owns the GPU (EGL context, CUDA, NVENC), turns guest
//! flips into encoded frames for the current session, and hands them to that
//! session's sender thread.
//!
//! Encoding is driven by the guest: a flip is encoded as soon as it arrives
//! (rate-limited to the client's fps). Without flips, the last picture is
//! re-encoded every REPEAT so lost packets heal and the client sees life, and
//! at once when the client asks for an IDR or the composited cursor moved.

use crate::broker::{self, Frame};
use crate::gamestream::input::Geometry;
use crate::gamestream::{udp, video};
use crate::gpu::{self, Codec, CodecCaps, EncParams, FrameOpts, Gpu};
use crate::host::Host;
use crate::session::{self, Job, Session, Sink, SS_ENC_VIDEO};
use std::collections::HashSet;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPEAT: Duration = Duration::from_millis(100);

/// What the pipeline learns at start-up, before the Host exists.
pub struct GpuInfo {
    pub codecs: [CodecCaps; 3],
    pub modifiers: HashSet<(u32, u64)>,
}

pub const FOURCCS: [u32; 4] = [
    0x34325258, // XR24
    0x34325241, // AR24
    0x34324258, // XB24
    0x34324241, // AB24
];

struct Sender {
    session: u32,
    tx: SyncSender<Job>,
}

fn sender_thread(host: Arc<Host>, s: Arc<Session>, sock: Arc<UdpSocket>, rx: Receiver<Job>) {
    let key = (s.cfg.encryption & SS_ENC_VIDEO != 0).then_some(s.launch.rikey);
    let mut pk = video::Packetizer::new(
        s.cfg.packet_size as usize,
        host.fec_percent as usize,
        s.cfg.min_fec_packets as usize,
        key,
    );
    let mut stat_t = Instant::now();
    let (mut frames, mut bytes, mut lat) = (0u64, 0u64, 0u64);
    while let Ok(j) = rx.recv() {
        if !s.alive() {
            break;
        }
        let Some(to) = udp::video_peer(&s) else {
            continue;
        };
        let ftype = if j.idr {
            video::FrameType::Idr
        } else if j.after_rfi {
            video::FrameType::AfterRfi
        } else {
            video::FrameType::P
        };
        let dgrams = pk.frame(&j.data, j.frame_index, ftype, j.latency_tenth_ms, j.rtp_ts);
        let n: usize = dgrams.iter().map(Vec::len).sum();
        udp::send_paced(&sock, to, &dgrams, host.link_mbps);
        frames += 1;
        bytes += n as u64;
        lat += j.latency_tenth_ms as u64;
        s.frames_sent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        s.bytes_sent
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        let el = stat_t.elapsed();
        if el >= Duration::from_secs(5) {
            log::info!(
                "session {}: {:.1} fps, {:.1} Mbit/s, host latency {:.2} ms (flip -> sent)",
                s.id,
                frames as f64 / el.as_secs_f64(),
                bytes as f64 * 8.0 / el.as_secs_f64() / 1e6,
                lat as f64 / frames.max(1) as f64 / 10.0
            );
            stat_t = Instant::now();
            frames = 0;
            bytes = 0;
            lat = 0;
        }
    }
}

/// `picture`: the guest picture's size, for sessions that follow it.
fn enc_params(host: &Host, s: &Session, picture: (u32, u32)) -> EncParams {
    let c = &s.cfg;
    let caps = host.caps(c.codec);
    let (width, height) = if c.follow_guest && picture.0 >= 64 && picture.1 >= 64 {
        (picture.0 & !1, picture.1 & !1)
    } else {
        (c.width, c.height)
    };
    EncParams {
        codec: c.codec as u32,
        width,
        height,
        fps: c.fps,
        bitrate_kbps: c.bitrate_kbps,
        chroma444: ((c.chroma444 || c.lossless) && caps.yuv444) as u32,
        lossless: (c.lossless && caps.lossless) as u32,
        colorspace: c.colorspace,
        full_range: c.full_range as u32,
        preset: 0,
        max_ref_frames: c.max_ref_frames,
        slices: c.slices,
        intra_refresh: 0,
    }
}

/// Open the GPU and report what it can do; then run the pipeline forever
/// once the Host arrives on `host_rx`.
pub fn run(
    render_node: Option<String>,
    info_tx: SyncSender<anyhow::Result<GpuInfo>>,
    host_rx: Receiver<(Arc<Host>, Arc<UdpSocket>)>,
) {
    let g = match Gpu::open(render_node.as_deref()) {
        Ok(g) => g,
        Err(e) => {
            let _ = info_tx.send(Err(e));
            return;
        }
    };
    let codecs = [Codec::H264, Codec::Hevc, Codec::Av1].map(|c| g.codec_caps(c));
    let mut modifiers = HashSet::new();
    for f in FOURCCS {
        for m in g.modifiers(f) {
            modifiers.insert((f, m));
        }
    }
    let _ = info_tx.send(Ok(GpuInfo { codecs, modifiers }));
    let Ok((host, sock)) = host_rx.recv() else {
        return;
    };
    main_loop(&g, &host, &sock);
}

fn main_loop(g: &Gpu, host: &Arc<Host>, sock: &Arc<UdpSocket>) {
    let sh = host.broker.clone();
    let mut enc: Option<(u32, gpu::Encoder<'_>)> = None;
    let mut sender: Option<Sender> = None;
    let mut pending: Option<Frame> = None;
    let mut last_encode = Instant::now() - REPEAT;
    let mut last_cursor: Option<(i32, i32)> = None;
    let mut seen_kick = 0u64;
    let mut stream_start = Instant::now();
    let mut cursor_visible = false;
    // Frame budget: tokens accrue at the stream's fps (at most 1 banked), a
    // frame costs one and may come half an interval early (tokens >= 0.5), so
    // a guest flipping at the stream rate is encoded the moment it flips,
    // whatever its phase, and a faster guest is thinned to the stream rate.
    let mut tokens = 1.0f64;
    let mut token_t = Instant::now();
    let mut fps = 60.0f64;
    let mut held_back = false;
    loop {
        // Wait for something to do, at most until the next due time.
        let cursor = {
            let mut ib = sh.inbox.lock().unwrap();
            let timeout = if enc.is_none() {
                Duration::from_millis(100)
            } else {
                let repeat = (last_encode + REPEAT).saturating_duration_since(Instant::now());
                if held_back {
                    let need = ((0.5 - tokens) / (fps * 1.02)).max(0.0);
                    repeat
                        .min(Duration::from_secs_f64(need))
                        .max(Duration::from_micros(200))
                } else {
                    repeat.max(Duration::from_micros(200))
                }
            };
            if ib.frame.is_none() && ib.cursor.is_none() && ib.kick == seen_kick {
                ib = sh.cv.wait_timeout(ib, timeout).unwrap().0;
            }
            if let Some(f) = ib.frame.take() {
                pending = Some(f);
            }
            seen_kick = ib.kick;
            ib.cursor.take()
        };
        if let Some(c) = cursor {
            let r = match &c {
                Some(img) => {
                    g.set_cursor(Some((img.fd.as_raw_fd(), img.desc, img.hot_x, img.hot_y)))
                }
                None => g.set_cursor(None),
            };
            if let Err(e) = r {
                log::warn!("cursor: {e:#}");
            }
            cursor_visible = c.is_some();
            last_cursor = None; // force a redraw
        }

        let Some(s) = host.current_session().filter(|s| s.alive()) else {
            if enc.take().is_some() {
                log::info!("encoder closed");
            }
            sender = None;
            // Keep the newest picture current for the next session.
            if let Some(f) = pending.take() {
                if let Err(e) = g.set_frame(f.fd.as_raw_fd(), &f.desc) {
                    log::warn!("frame: {e:#}");
                }
            }
            continue;
        };

        // A session that follows the guest's size reopens the encoder when it changes.
        let picture = match &pending {
            Some(f) => (f.desc.width, f.desc.height),
            None => g.frame_size(),
        };
        let resized = s.cfg.follow_guest
            && enc.as_ref().is_some_and(|(_, e)| {
                let want = enc_params(host, &s, picture);
                (want.width, want.height) != (e.params.width, e.params.height)
            });
        if enc.as_ref().map(|e| e.0) != Some(s.id) || resized {
            enc = None;
            let p = enc_params(host, &s, picture);
            match g.encoder(&p) {
                Ok(e) => {
                    log::info!(
                        "encoder: {} {}x{}@{} {} kbit/s {}",
                        Codec::from_u32(p.codec).map(Codec::name).unwrap_or("?"),
                        p.width,
                        p.height,
                        p.fps,
                        p.bitrate_kbps,
                        if p.chroma444 != 0 { "4:4:4" } else { "4:2:0" }
                    );
                    enc = Some((s.id, e));
                }
                Err(e) => {
                    log::error!("{e:#}");
                    session::end(host, &s, "the encoder could not start");
                    continue;
                }
            }
            match &s.sink {
                Sink::GameStream => {
                    if resized && sender.as_ref().is_some_and(|x| x.session == s.id) {
                        // keep the sender (and its packet sequence)
                    } else {
                        let (tx, rx) = sync_channel::<Job>(16);
                        let (h2, s2, k2) = (host.clone(), s.clone(), sock.clone());
                        std::thread::Builder::new()
                            .name("video-send".into())
                            .spawn(move || sender_thread(h2, s2, k2, rx))
                            .expect("thread");
                        sender = Some(Sender { session: s.id, tx });
                    }
                }
                Sink::Link(tx) => {
                    sender = Some(Sender {
                        session: s.id,
                        tx: tx.clone(),
                    });
                }
            }
            if !resized {
                stream_start = Instant::now();
            }
            last_encode = Instant::now() - REPEAT;
            last_cursor = None;
        }
        let Some((_, e)) = enc.as_mut() else { continue };

        // Input mapping follows the picture.
        let (gw, gh) = match &pending {
            Some(f) => (f.desc.width, f.desc.height),
            None => g.frame_size(),
        };
        let cursor_pos = {
            let mut inp = s.input.lock().unwrap();
            inp.set_geometry(Geometry {
                sw: s.cfg.width,
                sh: s.cfg.height,
                gw,
                gh,
            });
            inp.cursor_visible = cursor_visible;
            inp.cursor
        };

        let (want_idr, rfi) = {
            let mut r = s.requests.lock().unwrap();
            (std::mem::take(&mut r.idr), r.rfi.take())
        };
        let now = Instant::now();
        fps = s.cfg.fps.max(1) as f64;
        // Accrue 2% fast: at exactly the stream rate the bucket then sits full
        // instead of on the edge (where every frame would wait for a token).
        tokens = (tokens + now.duration_since(token_t).as_secs_f64() * fps * 1.02).min(1.0);
        token_t = now;
        let cursor_moved = cursor_visible && last_cursor != Some(cursor_pos);
        let new_picture = pending.is_some() || cursor_moved;
        let can = tokens >= 0.5;
        held_back = new_picture && !can;
        if !(want_idr || rfi.is_some() || (new_picture && can) || now >= last_encode + REPEAT) {
            continue;
        }
        tokens -= 1.0;
        let mut received = now;
        if let Some(f) = pending.take() {
            received = f.received;
            if let Err(err) = g.set_frame(f.fd.as_raw_fd(), &f.desc) {
                log::warn!(
                    "frame {}x{} {} {:#x}: {err:#}",
                    f.desc.width,
                    f.desc.height,
                    broker::fourcc_str(f.desc.fourcc),
                    f.desc.modifier
                );
            }
        }
        let mut force_idr = want_idr;
        let mut after_rfi = false;
        if let Some((first, last)) = rfi {
            if !force_idr {
                if e.invalidate(first, last) {
                    after_rfi = true;
                } else {
                    force_idr = true;
                }
            }
        }
        let opts = FrameOpts {
            render: new_picture as i32 | (force_idr as i32),
            force_idr: force_idr as i32,
            cursor_on: cursor_visible as i32,
            cursor_x: cursor_pos.0,
            cursor_y: cursor_pos.1,
        };
        last_cursor = Some(cursor_pos);
        let pk = match e.encode(&opts) {
            Ok(p) => p,
            Err(err) => {
                log::error!("encode: {err:#}");
                session::end(host, &s, "encoding failed");
                enc = None;
                continue;
            }
        };
        last_encode = Instant::now();
        log::trace!(
            "frame {}: waited {} us, encode {} us, {} bytes{}",
            pk.frame_index,
            now.duration_since(received).as_micros(),
            pk.encode_us,
            pk.data.len(),
            if pk.idr { " IDR" } else { "" }
        );
        let latency = last_encode.duration_since(received).as_micros() / 100;
        let job = Job {
            data: pk.data.to_vec(),
            frame_index: pk.frame_index as u32,
            idr: pk.idr,
            after_rfi,
            width: e.params.width,
            height: e.params.height,
            latency_tenth_ms: latency.min(u16::MAX as u128) as u16,
            rtp_ts: (stream_start.elapsed().as_micros() * 9 / 100) as u32,
        };
        if let Some(snd) = &sender {
            debug_assert_eq!(snd.session, s.id);
            match snd.tx.try_send(job) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    // The network is behind: drop this frame and start over
                    // from an IDR so the client never decodes a broken chain.
                    log::warn!("session {}: sender behind, frame dropped", s.id);
                    s.request_idr();
                }
                Err(TrySendError::Disconnected(_)) => sender = None,
            }
        }
    }
}

/// The broker's answer to QUERY_FORMAT, from what EGL imports.
pub fn format_check(mods: HashSet<(u32, u64)>) -> broker::FormatCheck {
    Box::new(move |fourcc, modifier| {
        FOURCCS.contains(&fourcc)
            && (modifier == 0x00ff_ffff_ffff_ffff || mods.contains(&(fourcc, modifier)))
    })
}

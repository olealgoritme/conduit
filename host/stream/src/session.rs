//! One streaming session: what the client asked for in RTSP ANNOUNCE, where
//! its UDP and control traffic go, and the requests (IDR, RFI) that flow from
//! the control channel to the encoder.

use crate::broker::{self, Pkt};
use crate::gpu::Codec;
use crate::host::{Host, Launch};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SS_ENC_CONTROL_V2: u32 = 0x01;
pub const SS_ENC_VIDEO: u32 = 0x02;
pub const SS_ENC_AUDIO: u32 = 0x04;
pub const ML_FF_FEC_STATUS: u32 = 0x01;
pub const ML_FF_SESSION_ID_V1: u32 = 0x02;

/// The client's stream request, from the ANNOUNCE body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub packet_size: u32,
    pub codec: Codec,
    pub chroma444: bool,
    pub colorspace: u32,
    pub full_range: bool,
    pub min_fec_packets: u32,
    pub encryption: u32,
    pub ml_flags: u32,
    pub slices: u32,
    pub max_ref_frames: u32,
    pub audio_channels: u32,
    pub audio_packet_ms: u32,
    /// x-nv-general.useReliableUdp == 13: the encrypted control protocol.
    pub control_v2: bool,
    pub hdr: bool,
    /// Lossless (conduit link): RGB as G/B/R planes, HEVC 4:4:4.
    pub lossless: bool,
    /// The stream is always the guest picture's own size (conduit link):
    /// the encoder follows it instead of scaling into a fixed size.
    pub follow_guest: bool,
}

/// Parse `a=name:value` lines.
pub fn sdp_attrs(body: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in body.split(['\r', '\n']) {
        if let Some(rest) = line.strip_prefix("a=") {
            if let Some((k, v)) = rest.split_once(':') {
                m.insert(k.to_string(), v.trim_end().to_string());
            }
        }
    }
    m
}

impl StreamConfig {
    /// `fec_percent`, `audio_hq` feed the bitrate adjustment Moonlight expects
    /// the host to make when it sends x-ml-video.configuredBitrateKbps.
    pub fn from_sdp(body: &str, fec_percent: u32) -> Result<StreamConfig, String> {
        let a = sdp_attrs(body);
        let get = |k: &str| a.get(k).map(String::as_str);
        let num =
            |k: &str, d: i64| -> i64 { get(k).and_then(|v| v.trim().parse().ok()).unwrap_or(d) };
        let need = |k: &str| -> Result<i64, String> {
            get(k)
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| format!("missing {k}"))
        };
        let width = need("x-nv-video[0].clientViewportWd")?;
        let height = need("x-nv-video[0].clientViewportHt")?;
        let fps = need("x-nv-video[0].maxFPS")?;
        let packet_size = need("x-nv-video[0].packetSize")?;
        let mut bitrate = need("x-nv-vqos[0].bw.maximumBitrateKbps")?;
        let codec = match num("x-nv-vqos[0].bitStreamFormat", 0) {
            1 => Codec::Hevc,
            2 => Codec::Av1,
            _ => Codec::H264,
        };
        let audio_channels = num("x-nv-audio.surround.numChannels", 2).clamp(1, 8);
        let configured = num("x-ml-video.configuredBitrateKbps", 0);
        if configured > 0 {
            // Moonlight's own rule: leave room for FEC, audio and overhead.
            let mut b = configured;
            if fec_percent <= 80 {
                b = b * (100 - fec_percent as i64) / 100;
            }
            b -= (96 * audio_channels).min(b / 5);
            b -= 500.min(b / 10);
            bitrate = b;
        }
        let csc = num("x-nv-video[0].encoderCscMode", 0);
        let cfg = StreamConfig {
            width: width.clamp(64, 8192) as u32 & !1,
            height: height.clamp(64, 8192) as u32 & !1,
            fps: fps.clamp(1, 1000) as u32,
            bitrate_kbps: bitrate.clamp(500, 10_000_000) as u32,
            packet_size: packet_size.clamp(256, 9000) as u32,
            codec,
            chroma444: num("x-ss-video[0].chromaSamplingType", 0) == 1,
            colorspace: ((csc >> 1) & 3) as u32,
            full_range: csc & 1 == 1,
            min_fec_packets: num("x-nv-vqos[0].fec.minRequiredFecPackets", 0).clamp(0, 64) as u32,
            encryption: num("x-ss-general.encryptionEnabled", 0) as u32,
            ml_flags: num("x-ml-general.featureFlags", 0) as u32,
            slices: num("x-nv-video[0].videoEncoderSlicesPerFrame", 1).clamp(1, 32) as u32,
            max_ref_frames: num("x-nv-video[0].maxNumReferenceFrames", 0).clamp(0, 16) as u32,
            audio_channels: audio_channels as u32,
            audio_packet_ms: num("x-nv-aqos.packetDuration", 5).clamp(1, 20) as u32,
            control_v2: num("x-nv-general.useReliableUdp", 1) == 13,
            hdr: num("x-nv-video[0].dynamicRangeMode", 0) == 1,
            lossless: false,
            follow_guest: false,
        };
        Ok(cfg)
    }
}

#[derive(Default)]
pub struct Requests {
    pub idr: bool,
    /// Reference-frame invalidation: frames [first, last] were lost.
    pub rfi: Option<(u64, u64)>,
}

/// One encoded frame for a session's sender.
pub struct Job {
    pub data: Vec<u8>,
    pub frame_index: u32,
    pub idr: bool,
    pub after_rfi: bool,
    pub latency_tenth_ms: u16,
    pub rtp_ts: u32,
    pub width: u32,
    pub height: u32,
}

/// Where a session's frames go.
pub enum Sink {
    /// Moonlight: the pipeline starts a UDP sender thread (packetizer, FEC).
    GameStream,
    /// The conduit link: its connection thread takes the frames.
    Link(std::sync::mpsc::SyncSender<Job>),
}

pub struct Session {
    pub id: u32,
    pub sink: Sink,
    pub cfg: StreamConfig,
    pub launch: Launch,
    pub started: Instant,
    pub video_peer: Mutex<Option<SocketAddr>>,
    pub audio_peer: Mutex<Option<SocketAddr>>,
    pub requests: Mutex<Requests>,
    pub stop: AtomicBool,
    pub last_seen: Mutex<Instant>,
    pub input: Mutex<crate::gamestream::input::InputState>,
    pub frames_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// Frame index the client last reported lost (for logs).
    pub losses: AtomicU64,
}

impl Session {
    pub fn alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
    pub fn touch(&self) {
        *self.last_seen.lock().unwrap() = Instant::now();
    }
    pub fn request_idr(&self) {
        self.requests.lock().unwrap().idr = true;
    }
}

static APP_RUNNING: AtomicBool = AtomicBool::new(false);

pub fn app_running(_host: &Host) -> bool {
    APP_RUNNING.load(Ordering::Relaxed)
}

pub fn set_app_running(_host: &Host, v: bool) {
    APP_RUNNING.store(v, Ordering::Relaxed);
}

/// The VM's display should be the stream's size and rate now.
pub fn mode_hint(host: &Host, w: u32, h: u32, fps: u32) {
    host.broker.send(Pkt::new(
        broker::EV_MODE_HINT,
        w as i32,
        h as i32,
        fps.saturating_mul(1000),
        broker::HINT_FULLSCREEN,
    ));
}

pub fn mode_restore(host: &Host) {
    host.broker.send(Pkt::new(
        broker::EV_MODE_HINT,
        0,
        0,
        0,
        broker::HINT_RESTORE,
    ));
}

/// ANNOUNCE accepted: replace any running session with this one.
pub fn start(host: &Arc<Host>, launch: Launch, cfg: StreamConfig) -> Arc<Session> {
    start_with(host, launch, cfg, Sink::GameStream)
}

pub fn start_with(host: &Arc<Host>, launch: Launch, cfg: StreamConfig, sink: Sink) -> Arc<Session> {
    stop_current(host, "a new session starts");
    let s = Arc::new(Session {
        id: launch.id,
        sink,
        cfg: cfg.clone(),
        launch,
        started: Instant::now(),
        video_peer: Mutex::new(None),
        audio_peer: Mutex::new(None),
        requests: Mutex::new(Requests {
            idr: true,
            rfi: None,
        }),
        stop: AtomicBool::new(false),
        last_seen: Mutex::new(Instant::now()),
        input: Mutex::new(Default::default()),
        frames_sent: AtomicU64::new(0),
        bytes_sent: AtomicU64::new(0),
        losses: AtomicU64::new(0),
    });
    log::info!(
        "session {}: {}x{}@{} {} {}{} {} kbit/s, packets {} B, encryption {:#x}",
        s.id,
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.codec.name(),
        if cfg.chroma444 { "4:4:4" } else { "4:2:0" },
        if cfg.full_range { " full" } else { "" },
        cfg.bitrate_kbps,
        cfg.packet_size,
        cfg.encryption
    );
    *host.session.lock().unwrap() = Some(s.clone());
    if !cfg.follow_guest {
        mode_hint(host, cfg.width, cfg.height, cfg.fps);
    }
    host.broker.kick();
    s
}

fn stop_current(host: &Host, why: &str) -> Option<Arc<Session>> {
    let old = host.session.lock().unwrap().take();
    if let Some(o) = &old {
        log::info!("session {}: ended ({why})", o.id);
        o.stop.store(true, Ordering::Relaxed);
        let mut inp = o.input.lock().unwrap();
        let evs = inp.release_all();
        drop(inp);
        host.broker.send_all(&evs);
    }
    host.broker.kick();
    old
}

/// The client disconnected; the app stays "running" so Moonlight offers Resume.
pub fn end(host: &Host, s: &Session, why: &str) {
    let mut cur = host.session.lock().unwrap();
    if cur.as_ref().is_some_and(|c| c.id == s.id) {
        *cur = None;
        drop(cur);
        log::info!("session {}: ended ({why})", s.id);
        s.stop.store(true, Ordering::Relaxed);
        let evs = s.input.lock().unwrap().release_all();
        host.broker.send_all(&evs);
        mode_restore(host);
        host.broker.kick();
    }
}

/// The client chose Quit: end the session and forget the launch.
pub fn quit(host: &Host, why: &str) {
    if stop_current(host, why).is_some() {
        mode_restore(host);
    }
    *host.launch.lock().unwrap() = None;
    set_app_running(host, false);
}

/// Housekeeping: a session whose client went silent ends.
pub fn reap(host: &Host, timeout: Duration) {
    if let Some(s) = host.current_session() {
        if s.last_seen.lock().unwrap().elapsed() > timeout {
            end(host, &s, "client timed out");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANNOUNCE: &str = "v=0\r\no=android 0 14 IN IPv4 0.0.0.0\r\ns=NVIDIA Streaming Client\r\n\
a=x-ml-general.featureFlags:3 \r\na=x-ss-general.encryptionEnabled:1 \r\n\
a=x-ss-video[0].chromaSamplingType:0 \r\na=x-nv-video[0].clientViewportWd:5120 \r\n\
a=x-nv-video[0].clientViewportHt:1440 \r\na=x-nv-video[0].maxFPS:240 \r\n\
a=x-nv-video[0].packetSize:1392 \r\na=x-nv-video[0].rateControlMode:4 \r\n\
a=x-nv-vqos[0].bw.maximumBitrateKbps:200000 \r\na=x-ml-video.configuredBitrateKbps:200000 \r\n\
a=x-nv-general.useReliableUdp:13 \r\na=x-nv-vqos[0].fec.minRequiredFecPackets:2 \r\n\
a=x-nv-video[0].videoEncoderSlicesPerFrame:1 \r\na=x-nv-vqos[0].bitStreamFormat:2 \r\n\
a=x-nv-video[0].dynamicRangeMode:0 \r\na=x-nv-video[0].maxNumReferenceFrames:0 \r\n\
a=x-nv-audio.surround.numChannels:2 \r\na=x-nv-aqos.packetDuration:5 \r\n\
a=x-nv-video[0].encoderCscMode:3 \r\nt=0 0\r\nm=video 47998  \r\n";

    #[test]
    fn announce_of_the_top_preset() {
        let c = StreamConfig::from_sdp(ANNOUNCE, 20).unwrap();
        assert_eq!((c.width, c.height, c.fps), (5120, 1440, 240));
        assert_eq!(c.codec, Codec::Av1);
        assert_eq!(c.packet_size, 1392);
        assert!(c.control_v2);
        assert_eq!(c.encryption, SS_ENC_CONTROL_V2);
        assert_eq!((c.colorspace, c.full_range), (1, true)); // csc 3 = BT.709 full
        assert_eq!(c.min_fec_packets, 2);
        // 200000 - 20% FEC = 160000, - 192 audio = 159808, - 500 = 159308
        assert_eq!(c.bitrate_kbps, 159_308);
    }

    #[test]
    fn announce_without_the_essentials_is_refused() {
        assert!(StreamConfig::from_sdp("a=x-nv-video[0].maxFPS:60\r\n", 20).is_err());
    }
}

//! The stream host: what every server thread shares.

use crate::broker;
use crate::gpu::{Codec, CodecCaps};
use crate::state::State;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// GameStream ports, all derived from the HTTP port like Moonlight expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub http: u16,
    pub https: u16,
    pub rtsp: u16,
    pub video: u16,
    pub control: u16,
    pub audio: u16,
}

impl Ports {
    pub fn from_http(http: u16) -> Ports {
        Ports {
            http,
            https: http - 5,
            rtsp: http + 21,
            video: http + 9,
            control: http + 10,
            audio: http + 11,
        }
    }
}

/// Defaults a preset gives; the client's own request (resolution, fps,
/// bitrate, codec) still wins where it says something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub codec: Codec,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "top",
        codec: Codec::Av1,
        fps: 240,
        bitrate_kbps: 200_000,
    },
    Preset {
        name: "balanced",
        codec: Codec::Hevc,
        fps: 120,
        bitrate_kbps: 80_000,
    },
    Preset {
        name: "compat",
        codec: Codec::H264,
        fps: 60,
        bitrate_kbps: 30_000,
    },
];

pub fn preset(name: &str) -> Option<Preset> {
    PRESETS.iter().copied().find(|p| p.name == name)
}

/// What the HTTPS `launch`/`resume` set up for the RTSP handshake.
#[derive(Clone, Debug)]
pub struct Launch {
    pub id: u32,
    pub rikey: [u8; 16],
    /// The audio encryption IV's first word (for the audio path, audio.rs).
    #[allow(dead_code)]
    pub rikeyid: u32,
    pub client_name: String,
    /// 16 characters echoed in the clients' UDP pings.
    pub ping_payload: String,
    pub connect_data: u32,
    pub encrypted_rtsp: bool,
    pub surround_params: String,
}

/// A pairing in progress, keyed by the client's uniqueid.
pub struct PendingPair {
    pub pairing: crate::gamestream::pairing::Pairing,
    pub pin: Option<String>,
    pub cancelled: bool,
    pub since: Instant,
}

pub struct Host {
    pub state: State,
    /// Shown in Moonlight's host list.
    pub hostname: String,
    /// The one app: the VM's desktop.
    pub app_name: String,
    pub ports: Ports,
    pub preset: Preset,
    pub codecs: [CodecCaps; 3],
    pub broker: Arc<broker::Shared>,
    pub pairs: Mutex<HashMap<String, PendingPair>>,
    pub pairs_cv: Condvar,
    pub launch: Mutex<Option<Launch>>,
    pub session: Mutex<Option<Arc<crate::session::Session>>>,
    pub next_id: AtomicU32,
    /// Video send-rate cap (Mbit/s) for pacing bursts.
    pub link_mbps: u32,
    /// FEC percentage for video.
    pub fec_percent: u32,
    /// Video encryption offered to clients that want it.
    pub allow_video_encryption: bool,
}

pub const APP_ID: u32 = 1;

impl Host {
    pub fn caps(&self, c: Codec) -> CodecCaps {
        self.codecs[c as usize]
    }

    pub fn new_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current_session(&self) -> Option<Arc<crate::session::Session>> {
        self.session.lock().unwrap().clone()
    }

    /// ServerCodecModeSupport bits (Sunshine's SCM_* extension).
    pub fn codec_mode_support(&self) -> u32 {
        const SCM_H264: u32 = 0x1;
        const SCM_HEVC: u32 = 0x100;
        const SCM_AV1_MAIN8: u32 = 0x10000;
        const SCM_H264_HIGH8_444: u32 = 0x40000;
        const SCM_HEVC_REXT8_444: u32 = 0x80000;
        const SCM_AV1_HIGH8_444: u32 = 0x200000;
        let mut m = 0;
        let h = self.caps(Codec::H264);
        if h.supported {
            m |= SCM_H264;
            if h.yuv444 {
                m |= SCM_H264_HIGH8_444;
            }
        }
        let h = self.caps(Codec::Hevc);
        if h.supported {
            m |= SCM_HEVC;
            if h.yuv444 {
                m |= SCM_HEVC_REXT8_444;
            }
        }
        let a = self.caps(Codec::Av1);
        if a.supported {
            m |= SCM_AV1_MAIN8;
            if a.yuv444 {
                m |= SCM_AV1_HIGH8_444;
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_follow_the_gamestream_layout() {
        let p = Ports::from_http(47989);
        assert_eq!(
            (p.https, p.rtsp, p.video, p.control, p.audio),
            (47984, 48010, 47998, 47999, 48000)
        );
    }

    #[test]
    fn the_top_preset_is_av1_240_200m() {
        let t = preset("top").unwrap();
        assert_eq!((t.codec, t.fps, t.bitrate_kbps), (Codec::Av1, 240, 200_000));
        assert!(preset("nope").is_none());
    }
}

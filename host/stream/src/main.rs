//! conduit-stream: stream a Conduit VM to Moonlight clients (GameStream
//! protocol) and to Conduit viewers (the conduit link), encoding with NVENC.
//! Design: docs/STREAMING.md.

mod broker;
mod ctl;
mod gamestream;
mod gpu;
mod host;
mod logging;
mod pipeline;
mod session;
mod state;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use gamestream::{control, nvhttp, rtsp, udp};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "conduit-stream",
    version,
    about = "Stream a Conduit VM to Moonlight and Conduit viewers (NVENC)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the stream host for one VM (the `conduit stream` command starts this)
    Serve {
        /// The VM's name (shown in Moonlight as the app)
        #[arg(long)]
        name: String,
        /// The VM's display socket to listen on (the backend connects to it)
        #[arg(long)]
        socket: PathBuf,
        /// GameStream base port (HTTP); the others follow it. Moonlight: add the host as IP:PORT if not 47989
        #[arg(long, default_value_t = 47989)]
        port: u16,
        /// Defaults for what the client does not ask for: top, balanced, compat
        #[arg(long, default_value = "top")]
        preset: String,
        /// GPU render node (default: the first NVIDIA one)
        #[arg(long)]
        render_node: Option<String>,
        /// Video FEC percentage
        #[arg(long, default_value_t = 20)]
        fec: u32,
        /// Pace video bursts to this many Mbit/s (0 = no pacing)
        #[arg(long, default_value_t = 1000)]
        link_mbps: u32,
        /// Offer video encryption to clients that ask for it
        #[arg(long)]
        video_encryption: bool,
        /// Where identity and paired clients live
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Enter the PIN a Moonlight client shows, for whichever stream host it is pairing with
    Pair {
        pin: String,
        /// Seconds to wait for a client to ask
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    /// List paired clients
    Clients {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Forget a paired client ("*" = all)
    Unpair {
        name: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Show running stream hosts and their sessions
    Status,
    /// (development) take frames on a display socket and write the encoded stream to a file
    EncodeTest {
        /// Display socket to listen on (the backend or nvgpu-scanout-test connects)
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, default_value = "hevc")]
        codec: gpu::Codec,
        #[arg(long, default_value = "1920x1080")]
        size: String,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 20000)]
        bitrate_kbps: u32,
        #[arg(long)]
        yuv444: bool,
        #[arg(long)]
        lossless: bool,
        #[arg(long, default_value_t = 120)]
        frames: u32,
        #[arg(long)]
        out: PathBuf,
    },
}

pub fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once('x')
        .with_context(|| format!("size {s:?} is not WxH"))?;
    let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
    if !(64..=8192).contains(&w) || !(64..=8192).contains(&h) {
        bail!("size {s} out of range");
    }
    Ok((w, h))
}

fn main() {
    logging::init();
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Serve {
            name,
            socket,
            port,
            preset,
            render_node,
            fec,
            link_mbps,
            video_encryption,
            state_dir,
        } => serve(ServeOpts {
            name,
            socket,
            port,
            preset,
            render_node,
            fec,
            link_mbps,
            video_encryption,
            state_dir,
        }),
        Cmd::Pair { pin, wait } => ctl::pair(&pin, Duration::from_secs(wait)).map(|d| {
            println!("paired: {d}");
        }),
        Cmd::Clients { state_dir } => (|| {
            let st = state::State::open(&state_dir.unwrap_or_else(state::default_dir))?;
            let c = st.clients();
            if c.is_empty() {
                println!("No paired clients.");
            }
            for c in c {
                println!("{}", c.name);
            }
            Ok(())
        })(),
        Cmd::Unpair { name, state_dir } => (|| {
            let st = state::State::open(&state_dir.unwrap_or_else(state::default_dir))?;
            let n = st.remove_client(&name)?;
            if n == 0 {
                bail!("no paired client named {name:?} (see `clients`)");
            }
            println!("removed {n} client(s)");
            Ok(())
        })(),
        Cmd::Status => ctl::status().map(|v| {
            if v.is_empty() {
                println!("No stream host is running.");
            }
            for l in v {
                println!("{l}");
            }
        }),
        Cmd::EncodeTest {
            socket,
            codec,
            size,
            fps,
            bitrate_kbps,
            yuv444,
            lossless,
            frames,
            out,
        } => (|| {
            let (w, h) = parse_size(&size)?;
            encode_test(
                &socket,
                gpu::EncParams {
                    codec: codec as u32,
                    width: w,
                    height: h,
                    fps,
                    bitrate_kbps,
                    chroma444: yuv444 as u32,
                    lossless: lossless as u32,
                    colorspace: 1,
                    ..Default::default()
                },
                frames,
                &out,
            )
        })(),
    };
    if let Err(e) = r {
        eprintln!("conduit-stream: {e:#}");
        std::process::exit(1);
    }
}

struct ServeOpts {
    name: String,
    socket: PathBuf,
    port: u16,
    preset: String,
    render_node: Option<String>,
    fec: u32,
    link_mbps: u32,
    video_encryption: bool,
    state_dir: Option<PathBuf>,
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect("thread");
}

fn serve(o: ServeOpts) -> Result<()> {
    let preset = host::preset(&o.preset)
        .with_context(|| format!("unknown preset {:?} (top, balanced, compat)", o.preset))?;
    if !(1024..=65000).contains(&o.port) {
        bail!("port {} out of range", o.port);
    }
    let st = state::State::open(&o.state_dir.unwrap_or_else(state::default_dir))?;

    // The GPU lives on the pipeline thread; it reports what it can do first.
    let (info_tx, info_rx) = std::sync::mpsc::sync_channel(1);
    let (host_tx, host_rx) = std::sync::mpsc::sync_channel(1);
    let rn = o.render_node.clone();
    spawn("pipeline", move || pipeline::run(rn, info_tx, host_rx));
    let info = info_rx.recv().context("pipeline thread")??;
    for (c, caps) in [gpu::Codec::H264, gpu::Codec::Hevc, gpu::Codec::Av1]
        .iter()
        .zip(info.codecs)
    {
        log::info!(
            "NVENC {}: {}{}{}",
            c.name(),
            if caps.supported { "yes" } else { "no" },
            if caps.yuv444 { ", 4:4:4" } else { "" },
            if caps.lossless { ", lossless" } else { "" }
        );
    }
    if !info.codecs.iter().any(|c| c.supported) {
        bail!("NVENC offers no codec on this GPU");
    }

    let caps = broker::CAP_KEYBOARD
        | broker::CAP_ABS_POINTER
        | broker::CAP_REL_POINTER
        | broker::CAP_FOCUS_EVENTS
        | broker::CAP_DMABUF
        | broker::CAP_MODIFIERS
        | broker::CAP_MODE_HINTS
        | broker::CAP_CURSOR
        | broker::CAP_GAMEPAD;
    let sh = broker::Shared::new(caps);
    *sh.formats.lock().unwrap() = Some(pipeline::format_check(info.modifiers));

    let ports = host::Ports::from_http(o.port);
    let host = Arc::new(host::Host {
        state: st,
        hostname: format!("conduit-{}", o.name),
        app_name: o.name.clone(),
        ports,
        preset,
        codecs: info.codecs,
        broker: sh.clone(),
        pairs: Mutex::new(HashMap::new()),
        pairs_cv: Condvar::new(),
        launch: Mutex::new(None),
        session: Mutex::new(None),
        next_id: AtomicU32::new(0),
        link_mbps: o.link_mbps,
        fec_percent: o.fec.min(100),
        allow_video_encryption: o.video_encryption,
    });

    // Sockets first, so a busy port fails loudly before anything runs.
    let http_l = nvhttp::listen(ports.http).with_context(|| format!("TCP port {}", ports.http))?;
    let https_l =
        nvhttp::listen(ports.https).with_context(|| format!("TCP port {}", ports.https))?;
    let rtsp_l = nvhttp::listen(ports.rtsp).with_context(|| format!("TCP port {}", ports.rtsp))?;
    let video =
        Arc::new(udp::bind(ports.video).with_context(|| format!("UDP port {}", ports.video))?);
    let audio =
        Arc::new(udp::bind(ports.audio).with_context(|| format!("UDP port {}", ports.audio))?);
    let acc = Arc::new(nvhttp::tls_acceptor(&host)?);

    host_tx.send((host.clone(), video.clone())).ok();
    {
        let h = host.clone();
        spawn("http", move || nvhttp::serve_http(h, http_l));
        let h = host.clone();
        spawn("https", move || nvhttp::serve_https(h, https_l, acc));
        let h = host.clone();
        spawn("rtsp", move || rtsp::serve(h, rtsp_l));
        let h = host.clone();
        let v = video.clone();
        spawn("video-ping", move || udp::ping_loop(h, v, udp::Kind::Video));
        let h = host.clone();
        spawn("audio-ping", move || {
            udp::ping_loop(h, audio, udp::Kind::Audio)
        });
        let h = host.clone();
        let outbox = Arc::new(control::Outbox::default());
        spawn("control", move || {
            if let Err(e) = control::serve(h, outbox) {
                log::error!("control: {e:#}");
                std::process::exit(1);
            }
        });
        let s2 = sh.clone();
        let sock = o.socket.clone();
        let h = host.clone();
        spawn("display", move || {
            let on_connect = move || {
                // A backend (re)connected: put the guest at the stream's mode again.
                if let Some(s) = h.current_session() {
                    session::mode_hint(&h, s.cfg.width, s.cfg.height, s.cfg.fps);
                }
            };
            if let Err(e) = broker::serve(&sock, s2, on_connect) {
                log::error!("display socket: {e:#}");
                std::process::exit(1);
            }
        });
    }
    ctl::serve(host.clone(), &o.name)?;
    log::info!(
        "streaming {:?}: Moonlight → add this computer{} and pair (`conduit stream pair PIN`); preset {} ({} {} fps {} Mbit/s)",
        o.name,
        if o.port == 47989 { String::new() } else { format!(" as IP:{}", o.port) },
        preset.name,
        preset.codec.name(),
        preset.fps,
        preset.bitrate_kbps / 1000
    );
    log::info!("certificate sha256 {}", host.state.fingerprint());
    loop {
        std::thread::park();
    }
}

fn encode_test(
    sock: &std::path::Path,
    p: gpu::EncParams,
    n: u32,
    out: &std::path::Path,
) -> Result<()> {
    let g = gpu::Gpu::open(None)?;
    for c in [gpu::Codec::H264, gpu::Codec::Hevc, gpu::Codec::Av1] {
        log::info!("{}: {:?}", c.name(), g.codec_caps(c));
    }
    let sh = broker::Shared::new(
        broker::CAP_DMABUF | broker::CAP_MODIFIERS | broker::CAP_KEYBOARD | broker::CAP_ABS_POINTER,
    );
    let sock = sock.to_path_buf();
    let sh2 = sh.clone();
    std::thread::spawn(move || broker::serve(&sock, sh2, || {}));
    let mut enc = g.encoder(&p)?;
    let mut f = std::fs::File::create(out)?;
    let mut done = 0;
    let mut total_us = 0u64;
    let mut bytes = 0usize;
    let t0 = Instant::now();
    while done < n {
        let frame = {
            let mut ib = sh.inbox.lock().unwrap();
            loop {
                if let Some(fr) = ib.frame.take() {
                    break Some(fr);
                }
                let (g2, to) = sh.cv.wait_timeout(ib, Duration::from_secs(10)).unwrap();
                ib = g2;
                if to.timed_out() {
                    break None;
                }
            }
        };
        let Some(fr) = frame else {
            bail!("no frames for 10 s")
        };
        use std::os::fd::AsRawFd;
        g.set_frame(fr.fd.as_raw_fd(), &fr.desc)?;
        let pk = enc.encode(&gpu::FrameOpts {
            render: 1,
            ..Default::default()
        })?;
        total_us += pk.encode_us as u64;
        bytes += pk.data.len();
        f.write_all(pk.data)?;
        done += 1;
    }
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "{done} frames in {secs:.2}s ({:.1} fps), encode avg {:.2} ms, {:.1} Mbit/s",
        done as f64 / secs,
        total_us as f64 / done as f64 / 1000.0,
        bytes as f64 * 8.0 / secs / 1e6
    );
    Ok(())
}

//! GameStream's HTTP side: plain HTTP on the base port (server info and the
//! start of pairing) and HTTPS with client certificates (everything else).
//! Only a paired certificate gets past the TLS check.

use super::http::{self, Request};
use super::pairing::{Pairing, Phase};
use crate::host::{Host, Launch, PendingPair, APP_ID};
use crate::state::{hex, random_hex, unhex};
use anyhow::Result;
use openssl::ssl::{SslAcceptor, SslMethod, SslVerifyMode};
use openssl::x509::X509;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// "7.1.431.-1": GameStream 7.1.431 with a negative build number, which is how
/// Moonlight recognises a Sunshine-compatible host (AV1, extended packets...).
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";
const PIN_WAIT: Duration = Duration::from_secs(300);
const MAX_CONNS: usize = 64;

static CONNS: AtomicUsize = AtomicUsize::new(0);

pub fn listen(port: u16) -> Result<TcpListener> {
    // Dual stack: [::] also takes IPv4 (v4-mapped) on Linux by default.
    let l = TcpListener::bind(SocketAddr::new(IpAddr::from([0u16; 8]), port))
        .or_else(|_| TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))))?;
    Ok(l)
}

pub fn serve_http(host: Arc<Host>, l: TcpListener) {
    for s in l.incoming().flatten() {
        if CONNS.load(Ordering::Relaxed) >= MAX_CONNS {
            continue;
        }
        let host = host.clone();
        CONNS.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = s.set_nodelay(true);
            let local = s.local_addr().ok();
            let peer = s.peer_addr().ok();
            let mut s = s;
            if let Ok(req) = http::read_request(&mut s) {
                handle(&host, &req, &mut s, None, local, peer);
            }
            CONNS.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

pub fn tls_acceptor(host: &Host) -> Result<SslAcceptor> {
    let mut b = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
    b.set_private_key(&host.state.key)?;
    b.set_certificate(&host.state.cert)?;
    b.check_private_key()?;
    // Ask for the client's certificate and accept any here: whether it is
    // paired is decided per request (an unpaired one gets a 401, which is what
    // tells Moonlight to pair).
    b.set_verify_callback(SslVerifyMode::PEER, |_, _| true);
    Ok(b.build())
}

pub fn serve_https(host: Arc<Host>, l: TcpListener, acc: Arc<SslAcceptor>) {
    for s in l.incoming().flatten() {
        if CONNS.load(Ordering::Relaxed) >= MAX_CONNS {
            continue;
        }
        let host = host.clone();
        let acc = acc.clone();
        CONNS.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = s.set_nodelay(true);
            let local = s.local_addr().ok();
            let peer = s.peer_addr().ok();
            if let Ok(mut tls) = acc.accept(s) {
                let cert = tls.ssl().peer_certificate();
                if let Ok(req) = http::read_request(&mut tls) {
                    handle(&host, &req, &mut tls, Some(cert), local, peer);
                }
                let _ = tls.shutdown();
            }
            CONNS.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

fn reply(s: &mut impl Write, body: String) {
    let _ = http::respond(s, 200, "OK", "application/xml", body.as_bytes());
}

fn err(s: &mut impl Write, code: u16, msg: &str) {
    reply(s, http::xml(code, Some(msg), &[]));
}

/// `tls`: None = plain HTTP; Some(cert) = HTTPS with that client certificate.
fn handle<S: Read + Write>(
    host: &Host,
    req: &Request,
    s: &mut S,
    tls: Option<Option<X509>>,
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
) {
    let paired = match &tls {
        None => None,
        Some(Some(c)) => host.state.client_for(c),
        Some(None) => None,
    };
    log::debug!(
        "http{} {} {} from {:?}{}",
        if tls.is_some() { "s" } else { "" },
        req.method,
        req.path,
        peer,
        paired
            .as_ref()
            .map(|c| format!(" ({})", c.name))
            .unwrap_or_default()
    );
    if req.method != "GET" {
        err(s, 405, "Only GET");
        return;
    }
    if tls.is_some() && paired.is_none() {
        // Unpaired certificate on HTTPS: 401 is the "please pair" answer.
        reply(
            s,
            http::xml(
                401,
                Some("The client is not authorized. Certificate verification failed."),
                &[],
            ),
        );
        return;
    }
    let https = tls.is_some();
    match req.path.as_str() {
        "/serverinfo" => serverinfo(host, req, s, https, local),
        "/pair" => pair(host, req, s, tls.and_then(|c| c)),
        "/applist" if https => applist(host, s),
        "/appasset" if https => {
            let _ = http::respond(s, 404, "Not Found", "image/png", b"");
        }
        "/launch" if https => launch(host, req, s, local, false, paired.map(|c| c.name)),
        "/resume" if https => launch(host, req, s, local, true, paired.map(|c| c.name)),
        "/cancel" if https => {
            crate::session::quit(host, "client quit the app");
            reply(s, http::xml(200, None, &[("cancel", "1".into())]));
        }
        _ => {
            let _ = http::respond(
                s,
                404,
                "Not Found",
                "application/xml",
                http::xml(404, None, &[]).as_bytes(),
            );
        }
    }
}

fn local_ip_str(local: Option<SocketAddr>) -> String {
    match local.map(|a| a.ip()) {
        Some(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => "127.0.0.1".into(), // Moonlight keeps LocalIP for IPv4 only
        },
        Some(IpAddr::V4(v4)) => v4.to_string(),
        None => "127.0.0.1".into(),
    }
}

fn url_host(local: Option<SocketAddr>) -> String {
    match local.map(|a| a.ip()) {
        Some(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => format!("[{v6}]"),
        },
        Some(IpAddr::V4(v4)) => v4.to_string(),
        None => "127.0.0.1".into(),
    }
}

fn serverinfo(
    host: &Host,
    req: &Request,
    s: &mut impl Write,
    https: bool,
    local: Option<SocketAddr>,
) {
    let running = crate::session::app_running(host);
    let pair_status = if https && req.q("uniqueid").is_some() {
        "1"
    } else {
        "0"
    };
    let hevc = host.caps(crate::gpu::Codec::Hevc).supported;
    let body = http::xml(
        200,
        None,
        &[
            ("hostname", host.hostname.clone()),
            ("appversion", APP_VERSION.into()),
            ("GfeVersion", GFE_VERSION.into()),
            ("uniqueid", host.state.uniqueid.clone()),
            ("HttpsPort", host.ports.https.to_string()),
            ("ExternalPort", host.ports.http.to_string()),
            (
                "MaxLumaPixelsHEVC",
                if hevc { "1869449984" } else { "0" }.into(),
            ),
            ("mac", "00:00:00:00:00:00".into()),
            ("LocalIP", local_ip_str(local)),
            (
                "ServerCodecModeSupport",
                host.codec_mode_support().to_string(),
            ),
            ("PairStatus", pair_status.into()),
            ("currentgame", if running { APP_ID } else { 0 }.to_string()),
            (
                "state",
                if running {
                    "SUNSHINE_SERVER_BUSY"
                } else {
                    "SUNSHINE_SERVER_FREE"
                }
                .into(),
            ),
        ],
    );
    reply(s, body);
}

fn applist(host: &Host, s: &mut impl Write) {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><App><IsHdrSupported>0</IsHdrSupported><AppTitle>{}</AppTitle><ID>{}</ID></App></root>",
        http::xml_escape(&host.app_name),
        APP_ID
    );
    reply(s, body);
}

fn pair_fail(s: &mut impl Write, msg: &str) {
    reply(s, http::xml(400, Some(msg), &[("paired", "0".into())]));
}

fn pair(host: &Host, req: &Request, s: &mut impl Write, _cert: Option<X509>) {
    let Some(uid) = req.q("uniqueid").map(str::to_string) else {
        err(s, 400, "Missing uniqueid parameter");
        return;
    };
    match req.q("phrase") {
        Some("getservercert") => return getservercert(host, req, s, uid),
        Some("pairchallenge") => {
            reply(s, http::xml(200, None, &[("paired", "1".into())]));
            return;
        }
        _ => {}
    }
    let mut pairs = host.pairs.lock().unwrap();
    let Some(p) = pairs.get_mut(&uid) else {
        err(s, 400, "Invalid uniqueid");
        return;
    };
    let arg = |k: &str| req.q(k).and_then(unhex);
    let result: Result<(Vec<(&str, String)>, bool)> = if let Some(v) = arg("clientchallenge") {
        p.pairing.client_challenge(&v, &host.state.cert).map(|r| {
            (
                vec![("paired", "1".into()), ("challengeresponse", hex(&r))],
                false,
            )
        })
    } else if let Some(v) = arg("serverchallengeresp") {
        p.pairing
            .server_challenge_resp(&v, &host.state.key)
            .map(|r| {
                (
                    vec![("paired", "1".into()), ("pairingsecret", hex(&r))],
                    false,
                )
            })
    } else if let Some(v) = arg("clientpairingsecret") {
        match p.pairing.client_pairing_secret(&v) {
            Ok(true) => {
                let name = p.pairing.device.clone();
                match host.state.add_client(&name, &p.pairing.client_cert_pem) {
                    Ok(()) => {
                        log::info!("pairing: {name:?} is now paired");
                        Ok((vec![("paired", "1".into())], true))
                    }
                    Err(e) => Err(e),
                }
            }
            Ok(false) => {
                log::warn!("pairing: {:?} failed (wrong PIN?)", p.pairing.device);
                Ok((vec![("paired", "0".into())], true))
            }
            Err(e) => Err(e),
        }
    } else {
        Err(anyhow::anyhow!("Invalid pairing request"))
    };
    match result {
        Ok((children, done)) => {
            if done {
                pairs.remove(&uid);
            }
            reply(s, http::xml(200, None, &children));
        }
        Err(e) => {
            pairs.remove(&uid);
            log::warn!("pairing: {e:#}");
            pair_fail(s, &format!("{e:#}"));
        }
    }
}

fn getservercert(host: &Host, req: &Request, s: &mut impl Write, uid: String) {
    let salt = req.q("salt").and_then(unhex);
    let cert = req
        .q("clientcert")
        .and_then(unhex)
        .and_then(|b| String::from_utf8(b).ok());
    let (Some(salt), Some(cert)) = (salt, cert) else {
        pair_fail(s, "Missing salt or clientcert");
        return;
    };
    let Ok(salt) = <[u8; 16]>::try_from(salt.as_slice()) else {
        pair_fail(s, "Salt too short");
        return;
    };
    if X509::from_pem(cert.as_bytes()).is_err() {
        pair_fail(s, "Invalid client certificate");
        return;
    }
    let device = req
        .q("devicename")
        .unwrap_or("roth")
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>();
    {
        let mut pairs = host.pairs.lock().unwrap();
        // Drop stale requests; a new request from the same client replaces its old one.
        pairs.retain(|_, p| p.since.elapsed() < PIN_WAIT);
        if let Some(old) = pairs.get_mut(&uid) {
            old.cancelled = true;
        }
        if pairs.len() >= 8 {
            drop(pairs);
            reply(
                s,
                http::xml(
                    503,
                    Some("Too many pending pairing requests"),
                    &[("paired", "0".into())],
                ),
            );
            return;
        }
        pairs.insert(
            uid.clone(),
            PendingPair {
                pairing: Pairing::new(salt, cert, device.clone()),
                pin: None,
                cancelled: false,
                since: Instant::now(),
            },
        );
    }
    host.pairs_cv.notify_all();
    log::info!(
        "pairing: {device:?} asks to pair; enter the PIN Moonlight shows with `conduit stream pair PIN`"
    );
    let deadline = Instant::now() + PIN_WAIT;
    let mut pairs = host.pairs.lock().unwrap();
    loop {
        let Some(p) = pairs.get_mut(&uid) else {
            drop(pairs);
            pair_fail(s, "Pairing request cancelled");
            return;
        };
        if p.cancelled {
            // replaced by a newer request from the same client: leave the new one alone
            drop(pairs);
            pair_fail(s, "Pairing request replaced");
            return;
        }
        if let Some(pin) = p.pin.take() {
            p.pairing.set_pin(&pin);
            debug_assert_eq!(p.pairing.phase, Phase::GotCert);
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            pairs.remove(&uid);
            drop(pairs);
            pair_fail(s, "Pairing timed out waiting for the PIN");
            return;
        }
        pairs = host.pairs_cv.wait_timeout(pairs, deadline - now).unwrap().0;
    }
    drop(pairs);
    reply(
        s,
        http::xml(
            200,
            None,
            &[
                ("paired", "1".into()),
                ("plaincert", hex(host.state.cert_pem.as_bytes())),
            ],
        ),
    );
}

/// Enter a PIN for the pending pairing (the most recent one). Returns the
/// device name, or None if no client is waiting.
pub fn enter_pin(host: &Host, pin: &str) -> Option<String> {
    let mut pairs = host.pairs.lock().unwrap();
    let p = pairs
        .values_mut()
        .filter(|p| !p.cancelled && p.pairing.phase == Phase::WaitPin && p.pin.is_none())
        .max_by_key(|p| p.since)?;
    p.pin = Some(pin.to_string());
    let name = p.pairing.device.clone();
    drop(pairs);
    host.pairs_cv.notify_all();
    Some(name)
}

pub fn pending_pairs(host: &Host) -> Vec<String> {
    host.pairs
        .lock()
        .unwrap()
        .values()
        .filter(|p| !p.cancelled && p.pairing.phase == Phase::WaitPin)
        .map(|p| p.pairing.device.clone())
        .collect()
}

fn launch(
    host: &Host,
    req: &Request,
    s: &mut impl Write,
    local: Option<SocketAddr>,
    resume: bool,
    client: Option<String>,
) {
    let rikey = req.q("rikey").and_then(unhex);
    let rikeyid = req.q("rikeyid").and_then(|v| v.parse::<i64>().ok());
    let (Some(rikey), Some(rikeyid)) = (rikey, rikeyid) else {
        reply(
            s,
            http::xml(
                400,
                Some("Missing a required launch parameter"),
                &[("resume", "0".into())],
            ),
        );
        return;
    };
    let Ok(rikey) = <[u8; 16]>::try_from(rikey.as_slice()) else {
        reply(
            s,
            http::xml(400, Some("Bad rikey"), &[("resume", "0".into())]),
        );
        return;
    };
    if !resume && req.q("appid").and_then(|v| v.parse::<u32>().ok()) != Some(APP_ID) {
        reply(
            s,
            http::xml(404, Some("No such app"), &[("gamesession", "0".into())]),
        );
        return;
    }
    let mut mode = req
        .q("mode")
        .unwrap_or("0x0x0")
        .split('x')
        .map(|v| v.parse::<u32>().unwrap_or(0));
    let (w, h, fps) = (
        mode.next().unwrap_or(0),
        mode.next().unwrap_or(0),
        mode.next().unwrap_or(0),
    );
    let corever = req
        .q("corever")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let mut cd = [0u8; 4];
    openssl::rand::rand_bytes(&mut cd).expect("RNG");
    let l = Launch {
        id: host.new_id(),
        rikey,
        rikeyid: rikeyid as u32,
        width: w,
        height: h,
        fps,
        client_name: client.unwrap_or_default(),
        ping_payload: random_hex(8),
        connect_data: u32::from_le_bytes(cd),
        encrypted_rtsp: corever >= 1,
        surround_params: req.q("surroundparams").unwrap_or("").to_string(),
        created: Instant::now(),
    };
    log::info!(
        "{} by {:?}: {}x{}@{}{}",
        if resume { "resume" } else { "launch" },
        l.client_name,
        w,
        h,
        fps,
        if l.encrypted_rtsp {
            " (encrypted RTSP)"
        } else {
            ""
        }
    );
    let scheme = if l.encrypted_rtsp { "rtspenc" } else { "rtsp" };
    *host.launch.lock().unwrap() = Some(l);
    crate::session::set_app_running(host, true);
    let url = format!("{scheme}://{}:{}", url_host(local), host.ports.rtsp);
    let mut children = vec![("sessionUrl0", url)];
    children.push(if resume {
        ("resume", "1".into())
    } else {
        ("gamesession", "1".into())
    });
    reply(s, http::xml(200, None, &children));
}

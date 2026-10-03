//! The video and audio UDP ports: clients ping them (SS_PING: the 16-byte
//! payload from RTSP SETUP + a sequence number) so we learn where to send,
//! and the video sender pushes datagrams with sendmmsg, paced to the link.

use crate::host::Host;
use crate::session::Session;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn bind(port: u16) -> std::io::Result<UdpSocket> {
    let s = UdpSocket::bind(SocketAddr::new(IpAddr::from([0u16; 8]), port))
        .or_else(|_| UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))))?;
    // Room for a big IDR in flight.
    let sz: libc::c_int = 8 << 20;
    // SAFETY: setsockopt with a properly sized int.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&sz as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as u32,
        );
        // Video is latency-sensitive: DSCP AF41 (like other game streamers).
        let tos: libc::c_int = 0x88;
        libc::setsockopt(
            s.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_TCLASS,
            (&tos as *const libc::c_int).cast(),
            4,
        );
        libc::setsockopt(
            s.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            (&tos as *const libc::c_int).cast(),
            4,
        );
    }
    Ok(s)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

/// Receive pings forever; record the sender as the session's peer.
pub fn ping_loop(host: Arc<Host>, sock: Arc<UdpSocket>, kind: Kind) {
    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let Some(s) = host.current_session() else {
            continue;
        };
        let ok = if n >= 20 {
            buf[..16] == *s.launch.ping_payload.as_bytes()
        } else {
            n == 4 && &buf[..4] == b"PING"
        };
        if !ok {
            continue;
        }
        let slot = match kind {
            Kind::Video => &s.video_peer,
            Kind::Audio => &s.audio_peer,
        };
        let mut p = slot.lock().unwrap();
        if *p != Some(from) {
            log::info!(
                "session {}: {} goes to {from}",
                s.id,
                if kind == Kind::Video {
                    "video"
                } else {
                    "audio"
                }
            );
            *p = Some(from);
            if kind == Kind::Video {
                drop(p);
                s.request_idr();
                host.broker.kick();
            }
        }
    }
}

fn sockaddr(a: SocketAddr) -> (libc::sockaddr_in6, libc::socklen_t) {
    // The socket is IPv6 (dual stack): IPv4 peers go as v4-mapped addresses.
    let ip6 = match a.ip() {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    };
    // SAFETY: zeroed plain struct.
    let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    sa.sin6_family = libc::AF_INET6 as u16;
    sa.sin6_port = a.port().to_be();
    sa.sin6_addr.s6_addr = ip6.octets();
    (sa, std::mem::size_of::<libc::sockaddr_in6>() as u32)
}

/// Send `dgrams` to `to`, at most `mbps` megabits per second averaged over
/// each batch (bursts into a slower link are what loses packets).
pub fn send_paced(sock: &UdpSocket, to: SocketAddr, dgrams: &[Vec<u8>], mbps: u32) -> usize {
    const BATCH: usize = 64;
    let v4 = sock.local_addr().map(|a| a.is_ipv4()).unwrap_or(false);
    let start = Instant::now();
    let mut sent_bytes = 0usize;
    let mut sent = 0usize;
    for chunk in dgrams.chunks(BATCH) {
        let mut iov: Vec<libc::iovec> = chunk
            .iter()
            .map(|d| libc::iovec {
                iov_base: d.as_ptr() as *mut _,
                iov_len: d.len(),
            })
            .collect();
        let mut sa6 = sockaddr(to);
        let mut sa4: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        if let (true, IpAddr::V4(ip)) = (v4, to.ip()) {
            sa4.sin_family = libc::AF_INET as u16;
            sa4.sin_port = to.port().to_be();
            sa4.sin_addr.s_addr = u32::from_ne_bytes(ip.octets());
        }
        let mut hdrs: Vec<libc::mmsghdr> = iov
            .iter_mut()
            .map(|io| {
                // SAFETY: zeroed plain struct, pointers to live locals.
                let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
                if v4 {
                    h.msg_hdr.msg_name = (&mut sa4 as *mut libc::sockaddr_in).cast();
                    h.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
                } else {
                    h.msg_hdr.msg_name = (&mut sa6.0 as *mut libc::sockaddr_in6).cast();
                    h.msg_hdr.msg_namelen = sa6.1;
                }
                h.msg_hdr.msg_iov = io;
                h.msg_hdr.msg_iovlen = 1;
                h
            })
            .collect();
        let mut off = 0;
        while off < hdrs.len() {
            // SAFETY: hdrs/iov/addresses are live for the call.
            let n = unsafe {
                libc::sendmmsg(
                    sock.as_raw_fd(),
                    hdrs[off..].as_mut_ptr(),
                    (hdrs.len() - off) as u32,
                    0,
                )
            };
            if n <= 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.raw_os_error() == Some(libc::ENOBUFS)
                {
                    std::thread::sleep(Duration::from_micros(200));
                    continue;
                }
                return sent;
            }
            off += n as usize;
        }
        sent += chunk.len();
        sent_bytes += chunk.iter().map(Vec::len).sum::<usize>();
        if mbps > 0 {
            let due = Duration::from_micros((sent_bytes as u64 * 8) / mbps as u64);
            let el = start.elapsed();
            if due > el {
                std::thread::sleep(due - el);
            }
        }
    }
    sent
}

/// The session's video peer, once its first ping arrived.
pub fn video_peer(s: &Session) -> Option<SocketAddr> {
    *s.video_peer.lock().unwrap()
}

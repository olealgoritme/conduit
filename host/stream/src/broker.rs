//! The display side of the broker protocol (host/viewer/common/nvkvm_broker_proto.h).
//!
//! The backend connects to the VM's display socket and sends each guest flip
//! as ATTACH (with a dma-buf) + COMMIT; we answer with HELLO, format answers,
//! mode hints and input. This is exactly what the local viewer does, so the
//! backend cannot tell a stream from a window.
//!
//! The backend is untrusted input here as everywhere: records are fixed size,
//! fds are only taken where the protocol says one rides, sizes are bounded.

use crate::gpu::BufDesc;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const PROTO_VERSION: u32 = 2;
pub const CMD_SIZE: usize = 40;
pub const PKT_SIZE: usize = 24;

pub const CMD_ATTACH: u16 = 1;
pub const CMD_COMMIT: u16 = 2;
pub const CMD_WINDOW: u16 = 3;
pub const CMD_CLIPBOARD: u16 = 4;
pub const CMD_CAPS: u16 = 5;
pub const CMD_QUERY_FORMAT: u16 = 6;
pub const CMD_CURSOR: u16 = 7;

pub const EV_HELLO: u16 = 1;
pub const EV_SURFACE: u16 = 2;
pub const EV_FRAME: u16 = 3;
pub const EV_KEY: u16 = 5;
pub const EV_BTN: u16 = 6;
pub const EV_ABS: u16 = 7;
pub const EV_REL: u16 = 8;
pub const EV_WHEEL: u16 = 9;
pub const EV_GRAB: u16 = 10;
pub const EV_FOCUS: u16 = 11;
pub const EV_POINTER: u16 = 12;
pub const EV_BYE: u16 = 13;
pub const EV_CLOSE: u16 = 14;
pub const EV_FORMAT: u16 = 16;
pub const EV_MODE_HINT: u16 = 17;
/// Gamepad (Conduit addition; only to a backend that announced
/// CLIENT_GAMEPAD): x = evdev code, y = value, w0 = pad index << 16 | evdev type.
pub const EV_PAD: u16 = 18;

pub const F_GRABBED: u16 = 1 << 0;
pub const F_FOCUSED: u16 = 1 << 1;
pub const F_FULLSCREEN: u16 = 1 << 2;

pub const CAP_KEYBOARD: u32 = 1 << 0;
pub const CAP_ABS_POINTER: u32 = 1 << 1;
pub const CAP_REL_POINTER: u32 = 1 << 2;
pub const CAP_FOCUS_EVENTS: u32 = 1 << 5;
pub const CAP_FULLSCREEN: u32 = 1 << 6;
pub const CAP_DMABUF: u32 = 1 << 7;
pub const CAP_MODIFIERS: u32 = 1 << 8;
pub const CAP_MODE_HINTS: u32 = 1 << 10;
pub const CAP_CURSOR: u32 = 1 << 11;
pub const CAP_GAMEPAD: u32 = 1 << 12;

pub const HINT_RESTORE: u32 = 0;
pub const HINT_FULLSCREEN: u32 = 1;
pub const HINT_FIXED: u32 = 3;

pub const CLIENT_SEQ_USEC: u32 = 1 << 1;
/// CMD_CAPS bit (Conduit addition): the backend carries EV_PAD to the guest.
pub const CLIENT_GAMEPAD: u32 = 1 << 2;
pub const MAX_DIM: u32 = 8192;
pub const CURSOR_MAX_DIM: u32 = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cmd {
    pub ty: u16,
    pub flags: u16,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub seq: u32,
    pub reserved1: u32,
}

impl Cmd {
    pub fn decode(b: &[u8; CMD_SIZE]) -> Cmd {
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Cmd {
            ty: u16_at(0),
            flags: u16_at(2),
            width: u32_at(4),
            height: u32_at(8),
            stride: u32_at(12),
            offset: u32_at(16),
            fourcc: u32_at(20),
            modifier: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            seq: u32_at(32),
            reserved1: u32_at(36),
        }
    }
    pub fn encode(&self) -> [u8; CMD_SIZE] {
        let mut b = [0u8; CMD_SIZE];
        b[0..2].copy_from_slice(&self.ty.to_le_bytes());
        b[2..4].copy_from_slice(&self.flags.to_le_bytes());
        for (i, v) in [
            self.width,
            self.height,
            self.stride,
            self.offset,
            self.fourcc,
        ]
        .iter()
        .enumerate()
        {
            b[4 + i * 4..8 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b[24..32].copy_from_slice(&self.modifier.to_le_bytes());
        b[32..36].copy_from_slice(&self.seq.to_le_bytes());
        b[36..40].copy_from_slice(&self.reserved1.to_le_bytes());
        b
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pkt {
    pub ty: u16,
    pub flags: u16,
    pub seq: u32,
    pub x: i32,
    pub y: i32,
    pub w0: u32,
    pub w1: u32,
}

impl Pkt {
    pub fn new(ty: u16, x: i32, y: i32, w0: u32, w1: u32) -> Pkt {
        Pkt {
            ty,
            x,
            y,
            w0,
            w1,
            ..Default::default()
        }
    }
    pub fn encode(&self) -> [u8; PKT_SIZE] {
        let mut b = [0u8; PKT_SIZE];
        b[0..2].copy_from_slice(&self.ty.to_le_bytes());
        b[2..4].copy_from_slice(&self.flags.to_le_bytes());
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..12].copy_from_slice(&self.x.to_le_bytes());
        b[12..16].copy_from_slice(&self.y.to_le_bytes());
        b[16..20].copy_from_slice(&self.w0.to_le_bytes());
        b[20..24].copy_from_slice(&self.w1.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8; PKT_SIZE]) -> Pkt {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Pkt {
            ty: u16::from_le_bytes([b[0], b[1]]),
            flags: u16::from_le_bytes([b[2], b[3]]),
            seq: u32_at(4),
            x: u32_at(8) as i32,
            y: u32_at(12) as i32,
            w0: u32_at(16),
            w1: u32_at(20),
        }
    }
}

/// One guest frame: the dma-buf and how to read it.
pub struct Frame {
    pub fd: OwnedFd,
    pub desc: BufDesc,
    /// Backend's CLOCK_MONOTONIC µs at the flip (low 32 bits), if announced.
    pub flip_us: Option<u32>,
    pub received: Instant,
}

pub struct CursorImg {
    pub fd: OwnedFd,
    pub desc: BufDesc,
    pub hot_x: u32,
    pub hot_y: u32,
}

/// What the broker hands the pipeline. Latest frame wins; a cursor change is
/// kept until taken.
#[derive(Default)]
pub struct Inbox {
    pub frame: Option<Frame>,
    /// Some(None) = cursor hidden, Some(Some) = new image.
    pub cursor: Option<Option<CursorImg>>,
    pub frames_in: u64,
    pub superseded: u64,
    pub connected: bool,
    /// Bumped by anyone who wants the pipeline to look (new session, IDR...).
    pub kick: u64,
}

pub struct Shared {
    pub inbox: Mutex<Inbox>,
    pub cv: Condvar,
    out: Mutex<Option<UnixStream>>,
    seq: Mutex<u32>,
    /// Formats the GPU imports, filled once by the pipeline thread.
    pub formats: Mutex<Option<FormatCheck>>,
    pub caps: u32,
    pub client_caps: Mutex<u32>,
}

pub type FormatCheck = Box<dyn Fn(u32, u64) -> bool + Send>;

impl Shared {
    pub fn new(caps: u32) -> Arc<Shared> {
        Arc::new(Shared {
            inbox: Mutex::new(Inbox::default()),
            cv: Condvar::new(),
            out: Mutex::new(None),
            seq: Mutex::new(0),
            formats: Mutex::new(None),
            caps,
            client_caps: Mutex::new(0),
        })
    }

    /// Send one event to the backend (no-op when none is connected).
    pub fn send(&self, mut p: Pkt) -> bool {
        let mut out = self.out.lock().unwrap();
        let Some(s) = out.as_mut() else { return false };
        {
            let mut seq = self.seq.lock().unwrap();
            *seq = seq.wrapping_add(1);
            p.seq = *seq;
        }
        p.flags |= F_FOCUSED;
        use std::io::Write;
        if let Err(e) = s.write_all(&p.encode()) {
            log::warn!("display: send to the backend failed: {e}");
            if let Ok(()) = s.shutdown(std::net::Shutdown::Both) {}
            *out = None;
            return false;
        }
        true
    }

    pub fn send_all(&self, ps: &[Pkt]) {
        let pads = *self.client_caps.lock().unwrap() & CLIENT_GAMEPAD != 0;
        for p in ps {
            if p.ty == EV_PAD && !pads {
                continue;
            }
            if !self.send(*p) {
                break;
            }
        }
    }

    pub fn connected(&self) -> bool {
        self.out.lock().unwrap().is_some()
    }

    pub fn kick(&self) {
        self.inbox.lock().unwrap().kick += 1;
        self.cv.notify_all();
    }
}

/// Inode of a dma-buf: the buffer's identity across separate fds.
pub fn fd_inode(fd: RawFd) -> u64 {
    // SAFETY: fstat into a zeroed struct on a live descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return 0;
    }
    st.st_ino
}

fn check_desc(c: &Cmd, max: u32) -> bool {
    c.width > 0
        && c.height > 0
        && c.width <= max
        && c.height <= max
        && c.stride >= c.width.saturating_mul(4)
        && c.stride <= 65536
        && c.offset < (1 << 30)
}

/// Listen on `path` and serve backends one at a time, forever.
pub fn serve(path: &Path, sh: Arc<Shared>, on_connect: impl Fn() + Send + 'static) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    // Owner only, like the viewer's socket.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    log::info!("display socket {}", path.display());
    for conn in l.incoming() {
        let s = match conn {
            Ok(s) => s,
            Err(e) => {
                log::warn!("display: accept: {e}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if !peer_is_us(&s) {
            log::warn!("display: refused a connection from another user");
            continue;
        }
        log::info!("display: backend connected");
        if let Err(e) = session(s, &sh, &on_connect) {
            log::info!("display: backend gone ({e:#})");
        } else {
            log::info!("display: backend disconnected");
        }
        *sh.out.lock().unwrap() = None;
        {
            let mut ib = sh.inbox.lock().unwrap();
            ib.connected = false;
            ib.frame = None;
        }
        sh.cv.notify_all();
    }
    Ok(())
}

fn peer_is_us(s: &UnixStream) -> bool {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a properly sized struct.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    // SAFETY: getuid cannot fail.
    r == 0 && (cred.uid == unsafe { libc::getuid() } || cred.uid == 0)
}

/// recvmsg: bytes into `buf`, any passed fds onto `fds`. 0 = EOF.
fn recv_with_fds(fd: RawFd, buf: &mut [u8], fds: &mut VecDeque<OwnedFd>) -> io::Result<usize> {
    let mut cbuf = [0u8; 256];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: msghdr points at live local buffers for the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len();
    let n = loop {
        // SAFETY: as above.
        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        break n as usize;
    };
    // SAFETY: walking the control buffer the kernel filled, with the libc macros.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push_back(OwnedFd::from_raw_fd(*data.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("control data truncated"));
    }
    Ok(n)
}

fn session(s: UnixStream, sh: &Arc<Shared>, on_connect: &dyn Fn()) -> Result<()> {
    s.set_write_timeout(Some(Duration::from_millis(500)))?;
    *sh.out.lock().unwrap() = Some(s.try_clone()?);
    *sh.seq.lock().unwrap() = 0;
    sh.send(Pkt::new(EV_HELLO, 0, 0, PROTO_VERSION, sh.caps));
    sh.send(Pkt::new(EV_FOCUS, 1, 0, 0, 0));
    {
        sh.inbox.lock().unwrap().connected = true;
    }
    on_connect();

    let fd = s.as_raw_fd();
    let mut buf = vec![0u8; CMD_SIZE * 64];
    let mut have = 0usize;
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();
    let mut attach: Option<(OwnedFd, Cmd)> = None;
    let mut seq_usec = false;
    loop {
        let n = recv_with_fds(fd, &mut buf[have..], &mut fds)?;
        if n == 0 {
            return Ok(());
        }
        have += n;
        let mut at = 0;
        while have - at >= CMD_SIZE {
            let c = Cmd::decode(buf[at..at + CMD_SIZE].try_into().unwrap());
            at += CMD_SIZE;
            match c.ty {
                CMD_ATTACH => {
                    let Some(f) = fds.pop_front() else {
                        anyhow::bail!("ATTACH without a dma-buf");
                    };
                    if !check_desc(&c, MAX_DIM) {
                        log::warn!(
                            "display: bad ATTACH {}x{} stride {}",
                            c.width,
                            c.height,
                            c.stride
                        );
                        attach = None;
                        continue;
                    }
                    attach = Some((f, c));
                }
                CMD_COMMIT => {
                    let Some((f, c)) = attach.take() else {
                        continue;
                    };
                    let desc = BufDesc {
                        id: fd_inode(f.as_raw_fd()),
                        width: c.width,
                        height: c.height,
                        stride: c.stride,
                        offset: c.offset,
                        fourcc: c.fourcc,
                        modifier: c.modifier,
                    };
                    let frame = Frame {
                        fd: f,
                        desc,
                        flip_us: seq_usec.then_some(c.seq),
                        received: Instant::now(),
                    };
                    let mut ib = sh.inbox.lock().unwrap();
                    if ib.frame.replace(frame).is_some() {
                        ib.superseded += 1;
                    }
                    ib.frames_in += 1;
                    drop(ib);
                    sh.cv.notify_all();
                }
                CMD_CURSOR => {
                    let img = if c.width == 0 {
                        None
                    } else {
                        let Some(f) = fds.pop_front() else {
                            anyhow::bail!("CURSOR image without a dma-buf");
                        };
                        if !check_desc(&c, CURSOR_MAX_DIM) {
                            log::warn!("display: bad cursor {}x{}", c.width, c.height);
                            continue;
                        }
                        Some(CursorImg {
                            desc: BufDesc {
                                id: fd_inode(f.as_raw_fd()),
                                width: c.width,
                                height: c.height,
                                stride: c.stride,
                                offset: c.offset,
                                fourcc: c.fourcc,
                                modifier: c.modifier,
                            },
                            fd: f,
                            hot_x: c.seq & 0xffff,
                            hot_y: c.seq >> 16,
                        })
                    };
                    sh.inbox.lock().unwrap().cursor = Some(img);
                    sh.cv.notify_all();
                }
                CMD_QUERY_FORMAT => {
                    let ok = sh
                        .formats
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|f| f(c.fourcc, c.modifier))
                        .unwrap_or(true);
                    log::info!(
                        "display: format {:?} modifier {:#x}: {}",
                        fourcc_str(c.fourcc),
                        c.modifier,
                        if ok { "yes" } else { "no" }
                    );
                    sh.send(Pkt::new(
                        EV_FORMAT,
                        ok as i32,
                        c.fourcc as i32,
                        c.modifier as u32,
                        (c.modifier >> 32) as u32,
                    ));
                }
                CMD_CAPS => {
                    seq_usec = c.width & CLIENT_SEQ_USEC != 0;
                    *sh.client_caps.lock().unwrap() = c.width;
                }
                CMD_WINDOW => {
                    // The guest's size; the window it would ask for does not
                    // exist here. Confirm it so the backend's bookkeeping settles.
                    sh.send(Pkt::new(EV_SURFACE, c.width as i32, c.height as i32, 0, 0));
                }
                CMD_CLIPBOARD => {}
                other => log::debug!("display: ignoring command {other}"),
            }
        }
        buf.copy_within(at..have, 0);
        have -= at;
        // Never hold on to fds no record claimed.
        while fds.len() > 4 {
            fds.pop_front();
        }
    }
}

pub fn fourcc_str(f: u32) -> String {
    f.to_le_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_have_the_c_layout() {
        let c = Cmd {
            ty: CMD_ATTACH,
            flags: 0,
            width: 2560,
            height: 1440,
            stride: 10240,
            offset: 0,
            fourcc: 0x34325258,
            modifier: 0x0300000000606014,
            seq: 7,
            reserved1: 0,
        };
        let b = c.encode();
        assert_eq!(b.len(), 40);
        assert_eq!(&b[0..2], &1u16.to_le_bytes());
        assert_eq!(&b[24..32], &0x0300000000606014u64.to_le_bytes());
        assert_eq!(Cmd::decode(&b), c);
        let p = Pkt::new(EV_ABS, -5, 9, 1920, 1080);
        assert_eq!(Pkt::decode(&p.encode()), p);
        assert_eq!(p.encode().len(), 24);
    }

    #[test]
    fn frames_and_cursors_arrive_with_their_fds() {
        use std::io::Write;
        let (a, mut b) = UnixStream::pair().unwrap();
        let sh = Shared::new(0);
        let sh2 = sh.clone();
        let t = std::thread::spawn(move || session(a, &sh2, &|| {}));
        // a memfd stands in for the dma-buf
        let mfd = unsafe { libc::memfd_create(c"x".as_ptr(), 0) };
        assert!(mfd >= 0);
        let attach = Cmd {
            ty: CMD_ATTACH,
            width: 64,
            height: 32,
            stride: 256,
            fourcc: 0x34325258,
            ..Default::default()
        };
        let commit = Cmd {
            ty: CMD_COMMIT,
            seq: 5,
            ..Default::default()
        };
        let mut rec = attach.encode().to_vec();
        rec.extend_from_slice(&commit.encode());
        send_fd(&b, &rec, mfd);
        // hello + focus arrive first
        let mut hello = [0u8; 24];
        use std::io::Read;
        b.read_exact(&mut hello).unwrap();
        assert_eq!(Pkt::decode(&hello).ty, EV_HELLO);
        b.read_exact(&mut hello).unwrap();
        for _ in 0..100 {
            if sh.inbox.lock().unwrap().frame.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let f = sh.inbox.lock().unwrap().frame.take().expect("frame");
        assert_eq!((f.desc.width, f.desc.height, f.desc.stride), (64, 32, 256));
        assert_eq!(f.desc.id, fd_inode(mfd));
        // a query is answered (no GPU: "yes")
        let q = Cmd {
            ty: CMD_QUERY_FORMAT,
            fourcc: 0x34325258,
            modifier: 1 << 56,
            ..Default::default()
        };
        b.write_all(&q.encode()).unwrap();
        let mut ans = [0u8; 24];
        b.read_exact(&mut ans).unwrap();
        let a = Pkt::decode(&ans);
        assert_eq!((a.ty, a.x, a.w1), (EV_FORMAT, 1, 1 << 24));
        drop(b);
        t.join().unwrap().unwrap();
        unsafe { libc::close(mfd) };
    }

    fn send_fd(s: &UnixStream, bytes: &[u8], fd: RawFd) {
        let mut cbuf = [0u8; 64];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
            *(libc::CMSG_DATA(c) as *mut RawFd) = fd;
            assert_eq!(libc::sendmsg(s.as_raw_fd(), &msg, 0), bytes.len() as isize);
        }
    }
}

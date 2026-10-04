//! Backend ↔ `conduit-venus` over a Unix `SOCK_SEQPACKET` socket.
//!
//! One request message per [`Renderer`] call, one reply per request, in
//! order; the backend never has more than one request outstanding (every
//! trait method takes `&mut self`). Calls that return nothing (`ctx_destroy`,
//! `ctx_detach`, `unref`) get no reply: SEQPACKET keeps order, so the next
//! request still sees their effect, and the backend does not wait a round trip
//! to tear a resource down. Blob and dma-buf fds ride on the reply with
//! `SCM_RIGHTS`.
//!
//! The server also sends `FENCES` messages on its own, whenever the renderer
//! signals. On the client a reader thread owns the receive side: it hands
//! replies to the calling thread over a channel and queues fences, signalling
//! an eventfd that is the client's [`Renderer::fence_fd`]. A thread rather than
//! nonblocking reads from the caller because fences must wake the backend's
//! poll loop while no call is in progress, and because a single reader means a
//! fence arriving between a request and its reply can never be consumed by the
//! wrong party. The caller is the only sender, so sends need no lock.
//!
//! Framing. SEQPACKET keeps message boundaries but one message must fit the
//! socket's send buffer (`wmem_default`, typically 208 KiB), and a submit can
//! be 4 MiB (docs/VENUS.md). So a logical message is sent as fragments of at
//! most [`FRAG`] bytes, each `[kind u32][flags u32][payload]`, `MORE` set on
//! all but the last; fds go with the first fragment. Each side sends from one
//! thread, so a message's fragments are never interleaved with another's.
//! All integers are little-endian.

use crate::{Blob, CapsetInfo, Dmabuf, Error, Renderer, Result, Signalled};
use std::collections::HashSet;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Payload bytes per fragment. Well under the default send buffer, which
/// also has to hold the skb overhead and the fragment header.
const FRAG: usize = 64 << 10;
const HDR: usize = 8;
const MORE: u32 = 1;
/// A reassembled message larger than this is a protocol error: the backend
/// caps a submit at 4 MiB, and nothing else comes close.
const MAX_MSG: usize = 8 << 20;
/// No message carries more than one fd; room for a few so that a confused
/// peer's extra fds are received (and closed) rather than truncated.
const MAX_FDS: usize = 4;

mod op {
    pub const CAPSET_INFO: u32 = 1;
    pub const CAPSET: u32 = 2;
    pub const CTX_CREATE: u32 = 3;
    pub const CTX_DESTROY: u32 = 4;
    pub const CTX_ATTACH: u32 = 5;
    pub const CTX_DETACH: u32 = 6;
    pub const SUBMIT: u32 = 7;
    pub const CREATE_BLOB: u32 = 8;
    pub const UNREF: u32 = 9;
    pub const CREATE_FENCE: u32 = 10;
    pub const EXPORT_SCANOUT: u32 = 11;

    pub const OK: u32 = 0x100;
    pub const ERR: u32 = 0x101;
    pub const FENCES: u32 = 0x200;
}

mod err {
    pub const REFUSED: u32 = 1;
    pub const NO_CONTEXT: u32 = 2;
    pub const NO_RESOURCE: u32 = 3;
    pub const DISCONNECTED: u32 = 4;
    pub const IO: u32 = 5;
}

struct Msg {
    kind: u32,
    body: Vec<u8>,
    fds: Vec<OwnedFd>,
}

fn proto(what: &'static str) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidData, what))
}

// ---------------------------------------------------------------- socket I/O

fn send_msg(sock: BorrowedFd<'_>, kind: u32, body: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let mut off = 0;
    loop {
        let end = (off + FRAG).min(body.len());
        let flags = if end < body.len() { MORE } else { 0 };
        let mut hdr = [0u8; HDR];
        hdr[..4].copy_from_slice(&kind.to_le_bytes());
        hdr[4..].copy_from_slice(&flags.to_le_bytes());
        let fd = if off == 0 { fd } else { None };
        send_frag(sock, &hdr, &body[off..end], fd)?;
        if flags == 0 {
            return Ok(());
        }
        off = end;
    }
}

fn send_frag(sock: BorrowedFd<'_>, hdr: &[u8], payload: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let mut iov = [
        libc::iovec { iov_base: hdr.as_ptr() as *mut _, iov_len: hdr.len() },
        libc::iovec { iov_base: payload.as_ptr() as *mut _, iov_len: payload.len() },
    ];
    // u64 storage keeps the cmsg buffer aligned for cmsghdr.
    let mut cbuf = [0u64; 4];
    // SAFETY: msghdr is plain data; every pointer in it refers to locals that
    // outlive the sendmsg call, and the CMSG macros stay inside cbuf because
    // CMSG_SPACE(sizeof(int)) is far below its 32 bytes.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = iov.len();
        if let Some(fd) = fd {
            let space = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = space;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd.as_raw_fd());
        }
        loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
            if n >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// One fragment. `Ok(None)` is an orderly close by the peer.
fn recv_frag(sock: BorrowedFd<'_>, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<Option<usize>> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: as in send_frag, all pointers are to live locals. Each fd taken
    // from the control message is new to this process (SCM_RIGHTS installs a
    // fresh descriptor), so wrapping it in OwnedFd is sound and closes it if
    // nobody wants it.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as u32) as usize;
        debug_assert!(msg.msg_controllen <= size_of::<[u64; 8]>());
        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
            if n >= 0 {
                break n as usize;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        };
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let len = (*c).cmsg_len - libc::CMSG_LEN(0) as usize;
                for i in 0..len / size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "venus ipc: truncated message"));
        }
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(n))
    }
}

/// One reassembled message. `Ok(None)` is an orderly close.
fn recv_msg(sock: BorrowedFd<'_>, buf: &mut Vec<u8>) -> io::Result<Option<Msg>> {
    buf.resize(HDR + FRAG, 0);
    let mut out: Option<Msg> = None;
    loop {
        let mut fds = Vec::new();
        let Some(n) = recv_frag(sock, buf, &mut fds)? else {
            return if out.is_none() {
                Ok(None)
            } else {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "venus ipc: closed mid-message"))
            };
        };
        if n < HDR {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "venus ipc: short fragment"));
        }
        let kind = u32::from_le_bytes(buf[..4].try_into().unwrap());
        let flags = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let m = out.get_or_insert_with(|| Msg { kind, body: Vec::new(), fds: Vec::new() });
        if m.kind != kind || m.body.len() + (n - HDR) > MAX_MSG {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "venus ipc: bad fragment"));
        }
        m.body.extend_from_slice(&buf[HDR..n]);
        m.fds.append(&mut fds);
        if flags & MORE == 0 {
            return Ok(out);
        }
    }
}

// ------------------------------------------------------------------ encoding

#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(proto("venus ipc: short body"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
}

fn encode_err(e: &Error) -> Vec<u8> {
    let (code, arg, errno, text): (u32, u32, i32, &str) = match e {
        Error::Refused(s) => (err::REFUSED, 0, 0, s),
        Error::NoContext(id) => (err::NO_CONTEXT, *id, 0, ""),
        Error::NoResource(id) => (err::NO_RESOURCE, *id, 0, ""),
        Error::Disconnected => (err::DISCONNECTED, 0, 0, ""),
        Error::Io(io) => (err::IO, 0, io.raw_os_error().unwrap_or(libc::EIO), ""),
    };
    W::default().u32(code).u32(arg).u32(errno as u32).bytes(text.as_bytes()).0
}

fn decode_err(body: &[u8]) -> Error {
    let mut r = R(body);
    let (Ok(code), Ok(arg), Ok(errno)) = (r.u32(), r.u32(), r.i32()) else {
        return proto("venus ipc: bad error reply");
    };
    match code {
        err::REFUSED => Error::Refused(intern(r.rest())),
        err::NO_CONTEXT => Error::NoContext(arg),
        err::NO_RESOURCE => Error::NoResource(arg),
        err::DISCONNECTED => Error::Disconnected,
        _ => Error::Io(io::Error::from_raw_os_error(errno)),
    }
}

/// `Error::Refused` holds a `&'static str`, so a reason that crossed the
/// socket has to be leaked to be returned. Reasons are a small fixed set of
/// literals in the renderer, so each is leaked once; past a bound (a
/// misbehaving renderer) they collapse into one generic reason instead of
/// growing without limit.
fn intern(text: &[u8]) -> &'static str {
    static SEEN: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    const LIMIT: usize = 64;
    let Ok(text) = std::str::from_utf8(text) else {
        return "renderer refused";
    };
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    let seen = seen.get_or_insert_with(HashSet::new);
    if let Some(s) = seen.get(text) {
        return s;
    }
    if seen.len() >= LIMIT || text.len() > 128 {
        return "renderer refused";
    }
    let s: &'static str = Box::leak(text.to_owned().into_boxed_str());
    seen.insert(s);
    s
}

fn encode_fences(f: &[Signalled]) -> Vec<u8> {
    let mut w = W::default();
    for s in f {
        w = w.u32(s.ctx_id).u32(s.ring_idx).u64(s.fence_id);
    }
    w.0
}

fn decode_fences(body: &[u8]) -> Result<Vec<Signalled>> {
    if !body.len().is_multiple_of(16) {
        return Err(proto("venus ipc: bad fence list"));
    }
    let mut r = R(body);
    let mut out = Vec::with_capacity(body.len() / 16);
    while !r.0.is_empty() {
        out.push(Signalled { ctx_id: r.u32()?, ring_idx: r.u32()?, fence_id: r.u64()? });
    }
    Ok(out)
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd returns a new descriptor or -1.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn eventfd_signal(fd: BorrowedFd<'_>) {
    let one: u64 = 1;
    // SAFETY: writes 8 bytes from a local. Failure means the counter is at its
    // maximum, which is still readable, so it is ignored.
    unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
}

fn eventfd_reset(fd: BorrowedFd<'_>) {
    let mut v: u64 = 0;
    // SAFETY: reads 8 bytes into a local; EAGAIN (not signalled) is fine.
    unsafe { libc::read(fd.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
}

/// A connected `SOCK_SEQPACKET` pair, for tests and for a backend that forks
/// the renderer itself.
pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair fills two new descriptors on success.
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both are fresh descriptors we own.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain data.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let p = path.as_os_str().as_bytes();
    if p.len() >= addr.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(p) {
        *d = *s as libc::c_char;
    }
    let len = (size_of::<libc::sa_family_t>() + p.len() + 1) as libc::socklen_t;
    Ok((addr, len))
}

fn seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: socket returns a new descriptor or -1.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Listen on `path` (replacing a stale socket file) and accept one peer: the
/// renderer process serves exactly one backend for its whole life.
pub fn listen_accept_one(path: &Path) -> io::Result<OwnedFd> {
    let _ = std::fs::remove_file(path);
    let sock = seqpacket()?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: addr/len describe a valid sockaddr_un.
    unsafe {
        if libc::bind(sock.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::listen(sock.as_raw_fd(), 1) != 0 {
            return Err(io::Error::last_os_error());
        }
        loop {
            let fd = libc::accept4(sock.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC);
            if fd >= 0 {
                let _ = std::fs::remove_file(path);
                return Ok(OwnedFd::from_raw_fd(fd));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

// -------------------------------------------------------------------- client

type Reply = std::result::Result<Msg, ()>;

struct Shared {
    fences: Mutex<Vec<Signalled>>,
    event: OwnedFd,
    dead: AtomicBool,
}

/// The backend's [`Renderer`]: forwards every call to `conduit-venus`.
pub struct IpcClient {
    sock: Arc<OwnedFd>,
    replies: Receiver<Reply>,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

impl IpcClient {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let sock = seqpacket()?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: addr/len describe a valid sockaddr_un.
        if unsafe { libc::connect(sock.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Self::new(sock)
    }

    /// Wrap a connected `SOCK_SEQPACKET` socket.
    pub fn new(sock: OwnedFd) -> io::Result<Self> {
        let sock = Arc::new(sock);
        let shared =
            Arc::new(Shared { fences: Mutex::new(Vec::new()), event: eventfd()?, dead: AtomicBool::new(false) });
        let (tx, replies) = channel();
        let reader = {
            let (sock, shared) = (sock.clone(), shared.clone());
            std::thread::Builder::new().name("venus-ipc".into()).spawn(move || read_loop(&sock, &shared, tx))?
        };
        Ok(Self { sock, replies, shared, reader: Some(reader) })
    }

    /// The renderer went away (closed, crashed, or broke protocol). The trait
    /// has no way to say so outside a call, so the backend can check this
    /// when [`Renderer::fence_fd`] wakes it with nothing signalled.
    pub fn is_disconnected(&self) -> bool {
        self.shared.dead.load(Ordering::Acquire)
    }

    fn send(&self, kind: u32, body: &[u8]) -> Result<()> {
        if self.is_disconnected() {
            return Err(Error::Disconnected);
        }
        send_msg(self.sock.as_fd(), kind, body, None).map_err(|_| Error::Disconnected)
    }

    fn call(&mut self, kind: u32, body: &[u8]) -> Result<Msg> {
        self.send(kind, body)?;
        let m = self.replies.recv().map_err(|_| Error::Disconnected)?.map_err(|_| Error::Disconnected)?;
        match m.kind {
            op::OK => Ok(m),
            op::ERR => Err(decode_err(&m.body)),
            _ => Err(proto("venus ipc: unexpected reply")),
        }
    }
}

fn read_loop(sock: &OwnedFd, shared: &Shared, tx: Sender<Reply>) {
    let mut buf = Vec::new();
    loop {
        let m = match recv_msg(sock.as_fd(), &mut buf) {
            Ok(Some(m)) => m,
            Ok(None) | Err(_) => break,
        };
        if m.kind == op::FENCES {
            let Ok(f) = decode_fences(&m.body) else { break };
            shared.fences.lock().unwrap_or_else(|p| p.into_inner()).extend(f);
            eventfd_signal(shared.event.as_fd());
        } else if tx.send(Ok(m)).is_err() {
            break;
        }
    }
    // Mark dead before waking anyone so a caller woken by the channel closing
    // or by the eventfd sees it.
    shared.dead.store(true, Ordering::Release);
    let _ = tx.send(Err(()));
    eventfd_signal(shared.event.as_fd());
}

impl Drop for IpcClient {
    fn drop(&mut self) {
        // Wakes the reader out of recvmsg; it then sees EOF and exits.
        // SAFETY: shutdown on a descriptor we still own.
        unsafe { libc::shutdown(self.sock.as_raw_fd(), libc::SHUT_RDWR) };
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
    }
}

fn expect_fd(m: &mut Msg) -> Result<OwnedFd> {
    if m.fds.len() != 1 {
        return Err(proto("venus ipc: reply without its fd"));
    }
    Ok(m.fds.pop().unwrap())
}

impl Renderer for IpcClient {
    fn capset_info(&mut self, index: u32) -> Result<CapsetInfo> {
        let m = self.call(op::CAPSET_INFO, &W::default().u32(index).0)?;
        let mut r = R(&m.body);
        Ok(CapsetInfo { id: r.u32()?, max_version: r.u32()?, max_size: r.u32()? })
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>> {
        Ok(self.call(op::CAPSET, &W::default().u32(id).u32(version).0)?.body)
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, debug_name: &[u8]) -> Result<()> {
        self.call(op::CTX_CREATE, &W::default().u32(ctx_id).u32(capset_id).bytes(debug_name).0)?;
        Ok(())
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        let _ = self.send(op::CTX_DESTROY, &W::default().u32(ctx_id).0);
    }

    fn ctx_attach(&mut self, ctx_id: u32, res_id: u32) -> Result<()> {
        self.call(op::CTX_ATTACH, &W::default().u32(ctx_id).u32(res_id).0)?;
        Ok(())
    }

    fn ctx_detach(&mut self, ctx_id: u32, res_id: u32) {
        let _ = self.send(op::CTX_DETACH, &W::default().u32(ctx_id).u32(res_id).0);
    }

    fn submit(&mut self, ctx_id: u32, commands: &[u8]) -> Result<()> {
        // One copy to prepend the header; a 4 MiB submit is rare and this
        // keeps the fragmenting writer simple.
        self.call(op::SUBMIT, &W::default().u32(ctx_id).bytes(commands).0)?;
        Ok(())
    }

    fn create_blob(&mut self, ctx_id: u32, res_id: u32, blob_id: u64, size: u64, flags: u32) -> Result<Blob> {
        let body = W::default().u32(ctx_id).u32(res_id).u64(blob_id).u64(size).u32(flags).0;
        let mut m = self.call(op::CREATE_BLOB, &body)?;
        let fd = expect_fd(&mut m)?;
        let mut r = R(&m.body);
        Ok(Blob { fd, map_info: r.u32()?, size: r.u64()? })
    }

    fn unref(&mut self, res_id: u32) {
        let _ = self.send(op::UNREF, &W::default().u32(res_id).0);
    }

    fn create_fence(&mut self, ctx_id: u32, ring_idx: u32, fence_id: u64) -> Result<()> {
        self.call(op::CREATE_FENCE, &W::default().u32(ctx_id).u32(ring_idx).u64(fence_id).0)?;
        Ok(())
    }

    fn fence_fd(&self) -> BorrowedFd<'_> {
        self.shared.event.as_fd()
    }

    fn signalled(&mut self) -> Vec<Signalled> {
        // Reset before taking: a fence queued after the take signals the
        // eventfd again, so none is left without a wakeup. The opposite order
        // could swallow one.
        // After the renderer dies this still resets: the death wakeup fires
        // once rather than leaving the fd readable for a poll loop to spin on.
        eventfd_reset(self.shared.event.as_fd());
        std::mem::take(&mut *self.shared.fences.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn export_scanout(&mut self, res_id: u32, width: u32, height: u32) -> Result<Dmabuf> {
        let mut m = self.call(op::EXPORT_SCANOUT, &W::default().u32(res_id).u32(width).u32(height).0)?;
        let fd = expect_fd(&mut m)?;
        let mut r = R(&m.body);
        Ok(Dmabuf {
            fd,
            width: r.u32()?,
            height: r.u32()?,
            stride: r.u32()?,
            offset: r.u32()?,
            fourcc: r.u32()?,
            modifier: r.u64()?,
        })
    }
}

// -------------------------------------------------------------------- server

/// The renderer side: serves one backend connection with any [`Renderer`].
pub struct IpcServer {
    sock: OwnedFd,
    renderer: Box<dyn Renderer>,
}

impl IpcServer {
    pub fn new(sock: OwnedFd, renderer: Box<dyn Renderer>) -> Self {
        Self { sock, renderer }
    }

    /// Serve until the backend hangs up (`Ok`) or breaks the protocol.
    ///
    /// Single-threaded: requests and fence forwarding share one poll loop, so
    /// replies and `FENCES` messages are never interleaved mid-fragment.
    pub fn serve(mut self) -> io::Result<Self> {
        let mut buf = Vec::new();
        loop {
            let mut pfd = [
                libc::pollfd { fd: self.sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.renderer.fence_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: polls two descriptors we hold for the call's duration.
            let n = unsafe { libc::poll(pfd.as_mut_ptr(), 2, -1) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if pfd[0].revents != 0 {
                let Some(m) = recv_msg(self.sock.as_fd(), &mut buf)? else {
                    return Ok(self);
                };
                self.dispatch(m)?;
            }
            // Checked after every request too, not only on fence_fd: a
            // renderer may signal synchronously inside create_fence (the
            // Mock does) without making its fd readable.
            self.forward_fences()?;
        }
    }

    fn forward_fences(&mut self) -> io::Result<()> {
        let f = self.renderer.signalled();
        if f.is_empty() {
            return Ok(());
        }
        send_msg(self.sock.as_fd(), op::FENCES, &encode_fences(&f), None)
    }

    fn reply(&self, r: Result<Vec<u8>>, fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
        match r {
            Ok(body) => send_msg(self.sock.as_fd(), op::OK, &body, fd),
            Err(e) => send_msg(self.sock.as_fd(), op::ERR, &encode_err(&e), None),
        }
    }

    fn dispatch(&mut self, m: Msg) -> io::Result<()> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "venus ipc: bad request");
        let mut r = R(&m.body);
        let rd = &mut self.renderer;
        match m.kind {
            op::CAPSET_INFO => {
                let index = r.u32().map_err(|_| bad())?;
                let res = rd.capset_info(index).map(|c| W::default().u32(c.id).u32(c.max_version).u32(c.max_size).0);
                self.reply(res, None)
            }
            op::CAPSET => {
                let (id, version) = (r.u32().map_err(|_| bad())?, r.u32().map_err(|_| bad())?);
                let res = rd.capset(id, version);
                self.reply(res, None)
            }
            op::CTX_CREATE => {
                let (ctx, capset) = (r.u32().map_err(|_| bad())?, r.u32().map_err(|_| bad())?);
                let res = rd.ctx_create(ctx, capset, r.rest()).map(|()| Vec::new());
                self.reply(res, None)
            }
            op::CTX_DESTROY => {
                rd.ctx_destroy(r.u32().map_err(|_| bad())?);
                Ok(())
            }
            op::CTX_ATTACH => {
                let (ctx, res) = (r.u32().map_err(|_| bad())?, r.u32().map_err(|_| bad())?);
                let res = rd.ctx_attach(ctx, res).map(|()| Vec::new());
                self.reply(res, None)
            }
            op::CTX_DETACH => {
                let (ctx, res) = (r.u32().map_err(|_| bad())?, r.u32().map_err(|_| bad())?);
                rd.ctx_detach(ctx, res);
                Ok(())
            }
            op::SUBMIT => {
                let ctx = r.u32().map_err(|_| bad())?;
                let res = rd.submit(ctx, r.rest()).map(|()| Vec::new());
                self.reply(res, None)
            }
            op::CREATE_BLOB => {
                let mut f = || -> Result<_> { Ok((r.u32()?, r.u32()?, r.u64()?, r.u64()?, r.u32()?)) };
                let (ctx, res, blob_id, size, flags) = f().map_err(|_| bad())?;
                match rd.create_blob(ctx, res, blob_id, size, flags) {
                    Ok(b) => self.reply(Ok(W::default().u32(b.map_info).u64(b.size).0), Some(b.fd.as_fd())),
                    Err(e) => self.reply(Err(e), None),
                }
            }
            op::UNREF => {
                rd.unref(r.u32().map_err(|_| bad())?);
                Ok(())
            }
            op::CREATE_FENCE => {
                let mut f = || -> Result<_> { Ok((r.u32()?, r.u32()?, r.u64()?)) };
                let (ctx, ring, id) = f().map_err(|_| bad())?;
                let res = rd.create_fence(ctx, ring, id).map(|()| Vec::new());
                self.reply(res, None)
            }
            op::EXPORT_SCANOUT => {
                let mut f = || -> Result<_> { Ok((r.u32()?, r.u32()?, r.u32()?)) };
                let (res, w, h) = f().map_err(|_| bad())?;
                match rd.export_scanout(res, w, h) {
                    Ok(d) => {
                        let body = W::default()
                            .u32(d.width)
                            .u32(d.height)
                            .u32(d.stride)
                            .u32(d.offset)
                            .u32(d.fourcc)
                            .u64(d.modifier)
                            .0;
                        self.reply(Ok(body), Some(d.fd.as_fd()))
                    }
                    Err(e) => self.reply(Err(e), None),
                }
            }
            _ => Err(bad()),
        }
    }

    /// The renderer, for inspection after [`IpcServer::serve`] returns.
    pub fn into_renderer(self) -> Box<dyn Renderer> {
        self.renderer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CAPSET_VENUS;
    use crate::mock::Mock;

    /// A Mock behind a server thread; the thread returns the Mock so tests
    /// can check what reached it.
    fn pair() -> (IpcClient, JoinHandle<Box<dyn Renderer>>) {
        let (a, b) = socketpair().unwrap();
        let server = std::thread::spawn(move || {
            IpcServer::new(b, Box::new(Mock::new())).serve().expect("serve").into_renderer()
        });
        (IpcClient::new(a).unwrap(), server)
    }

    fn file_size(fd: BorrowedFd<'_>) -> u64 {
        // SAFETY: fstat into a zeroed local.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd.as_raw_fd(), &mut st), 0);
            st.st_size as u64
        }
    }

    fn readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> bool {
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd on a live descriptor.
        unsafe { libc::poll(&mut p, 1, timeout_ms) == 1 }
    }

    #[test]
    fn capsets() {
        let (mut c, _s) = pair();
        assert_eq!(c.capset_info(0).unwrap(), CapsetInfo { id: CAPSET_VENUS, max_version: 0, max_size: 160 });
        assert!(matches!(c.capset_info(1), Err(Error::Refused("capset index"))));
        assert_eq!(c.capset(CAPSET_VENUS, 0).unwrap(), vec![0; 160]);
        assert!(matches!(c.capset(1, 0), Err(Error::Refused("capset id"))));
    }

    #[test]
    fn contexts_resources_and_errors() {
        let (mut c, s) = pair();
        assert!(matches!(c.ctx_create(1, 1, b"x"), Err(Error::Refused(_))));
        c.ctx_create(1, CAPSET_VENUS, b"game.exe").unwrap();
        assert!(matches!(c.ctx_attach(1, 7), Err(Error::NoResource(7))));
        assert!(matches!(c.ctx_attach(2, 7), Err(Error::NoContext(2))));

        let b = c.create_blob(1, 7, 42, 1 << 20, 1).unwrap();
        assert_eq!((b.size, b.map_info), (1 << 20, 1));
        assert_eq!(file_size(b.fd.as_fd()), 1 << 20);
        assert!(matches!(c.create_blob(9, 8, 0, 4096, 1), Err(Error::NoContext(9))));
        c.ctx_attach(1, 7).unwrap();
        c.ctx_detach(1, 7);

        let d = c.export_scanout(7, 1920, 1080).unwrap();
        assert_eq!((d.width, d.height, d.stride, d.offset), (1920, 1080, 1920 * 4, 0));
        assert_eq!((d.fourcc, d.modifier), (u32::from_le_bytes(*b"XR24"), 0));
        assert_eq!(file_size(d.fd.as_fd()), 1920 * 1080 * 4);

        c.unref(7);
        // Ordered after the fire-and-forget unref.
        assert!(matches!(c.export_scanout(7, 1, 1), Err(Error::NoResource(7))));
        c.ctx_destroy(1);
        assert!(matches!(c.submit(1, &[0; 4]), Err(Error::NoContext(1))));

        // The server returns cleanly once the client hangs up.
        drop(c);
        s.join().unwrap();
    }

    #[test]
    fn blob_memory_is_shared() {
        let (mut c, _s) = pair();
        c.ctx_create(1, CAPSET_VENUS, b"").unwrap();
        let b = c.create_blob(1, 1, 0, 4096, 1).unwrap();
        // The fd that crossed the socket is a working, mappable descriptor.
        // SAFETY: plain pwrite/pread on the received descriptor.
        unsafe {
            assert_eq!(libc::pwrite(b.fd.as_raw_fd(), b"venus".as_ptr().cast(), 5, 100), 5);
            let mut got = [0u8; 5];
            assert_eq!(libc::pread(b.fd.as_raw_fd(), got.as_mut_ptr().cast(), 5, 100), 5);
            assert_eq!(&got, b"venus");
            let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, b.fd.as_raw_fd(), 0);
            assert_ne!(p, libc::MAP_FAILED);
            assert_eq!(std::slice::from_raw_parts(p.cast::<u8>().add(100), 5), b"venus");
            libc::munmap(p, 4096);
        }
    }

    #[test]
    fn large_submit_reaches_renderer_intact() {
        // Same as above but with a renderer that records into shared state,
        // since the Mock behind Box<dyn Renderer> cannot be downcast.
        type Log = Arc<Mutex<Vec<(u32, Vec<u8>)>>>;
        struct Rec(Mock, Log);
        impl Renderer for Rec {
            fn capset_info(&mut self, i: u32) -> Result<CapsetInfo> {
                self.0.capset_info(i)
            }
            fn capset(&mut self, i: u32, v: u32) -> Result<Vec<u8>> {
                self.0.capset(i, v)
            }
            fn ctx_create(&mut self, c: u32, s: u32, n: &[u8]) -> Result<()> {
                self.0.ctx_create(c, s, n)
            }
            fn ctx_destroy(&mut self, c: u32) {
                self.0.ctx_destroy(c)
            }
            fn ctx_attach(&mut self, c: u32, r: u32) -> Result<()> {
                self.0.ctx_attach(c, r)
            }
            fn ctx_detach(&mut self, c: u32, r: u32) {
                self.0.ctx_detach(c, r)
            }
            fn submit(&mut self, c: u32, cmd: &[u8]) -> Result<()> {
                self.0.submit(c, cmd)?;
                self.1.lock().unwrap().push((c, cmd.to_vec()));
                Ok(())
            }
            fn create_blob(&mut self, c: u32, r: u32, b: u64, s: u64, f: u32) -> Result<Blob> {
                self.0.create_blob(c, r, b, s, f)
            }
            fn unref(&mut self, r: u32) {
                self.0.unref(r)
            }
            fn create_fence(&mut self, c: u32, r: u32, f: u64) -> Result<()> {
                self.0.create_fence(c, r, f)
            }
            fn fence_fd(&self) -> BorrowedFd<'_> {
                self.0.fence_fd()
            }
            fn signalled(&mut self) -> Vec<Signalled> {
                self.0.signalled()
            }
            fn export_scanout(&mut self, r: u32, w: u32, h: u32) -> Result<Dmabuf> {
                self.0.export_scanout(r, w, h)
            }
        }
        let log = Arc::new(Mutex::new(Vec::new()));
        let (a, b) = socketpair().unwrap();
        let rec = Rec(Mock::new(), log.clone());
        let server = std::thread::spawn(move || IpcServer::new(b, Box::new(rec)).serve().unwrap());
        let mut c = IpcClient::new(a).unwrap();
        c.ctx_create(3, CAPSET_VENUS, b"").unwrap();
        let big: Vec<u8> = (0..(4u32 << 20) + 12).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        c.submit(3, &big).unwrap();
        c.submit(3, &[1, 2, 3, 4]).unwrap();
        drop(c);
        server.join().unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], (3, big));
        assert_eq!(log[1], (3, vec![1, 2, 3, 4]));
    }

    #[test]
    fn fences_wake_fence_fd() {
        let (mut c, _s) = pair();
        c.ctx_create(1, CAPSET_VENUS, b"").unwrap();
        assert!(!readable(c.fence_fd(), 0));
        assert!(c.signalled().is_empty());

        c.create_fence(1, 0, 10).unwrap();
        c.create_fence(1, 2, 11).unwrap();
        assert!(readable(c.fence_fd(), 5000));
        // Both may have arrived as one or two FENCES messages; drain until
        // both are seen.
        let mut got = Vec::new();
        while got.len() < 2 {
            assert!(readable(c.fence_fd(), 5000));
            got.extend(c.signalled());
        }
        assert_eq!(
            got,
            vec![
                Signalled { ctx_id: 1, ring_idx: 0, fence_id: 10 },
                Signalled { ctx_id: 1, ring_idx: 2, fence_id: 11 },
            ]
        );
        assert!(!readable(c.fence_fd(), 50));
    }

    #[test]
    fn renderer_death_is_disconnected() {
        let (a, b) = socketpair().unwrap();
        let mut c = IpcClient::new(a).unwrap();
        drop(b);
        // The reader notices and wakes fence_fd so a poll loop finds out.
        assert!(readable(c.fence_fd(), 5000));
        assert!(c.signalled().is_empty());
        assert!(c.is_disconnected());
        assert!(!readable(c.fence_fd(), 0));
        assert!(matches!(c.capset_info(0), Err(Error::Disconnected)));
        c.ctx_destroy(1);
        c.unref(1);
    }

    #[test]
    fn server_rejects_garbage() {
        let (a, b) = socketpair().unwrap();
        let server = std::thread::spawn(move || IpcServer::new(b, Box::new(Mock::new())).serve().map(|_| ()));
        send_msg(a.as_fd(), 999, b"", None).unwrap();
        assert!(server.join().unwrap().is_err());

        let (a, b) = socketpair().unwrap();
        let server = std::thread::spawn(move || IpcServer::new(b, Box::new(Mock::new())).serve().map(|_| ()));
        send_msg(a.as_fd(), op::CREATE_BLOB, &[0; 3], None).unwrap();
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn unexpected_fds_are_closed() {
        // A request carrying an fd it should not: the server must not keep it.
        let (a, b) = socketpair().unwrap();
        let (x, y) = socketpair().unwrap();
        let server = std::thread::spawn(move || IpcServer::new(b, Box::new(Mock::new())).serve().map(|_| ()));
        send_msg(a.as_fd(), op::CAPSET_INFO, &0u32.to_le_bytes(), Some(y.as_fd())).unwrap();
        drop(y);
        let mut buf = Vec::new();
        let m = recv_msg(a.as_fd(), &mut buf).unwrap().unwrap();
        assert_eq!(m.kind, op::OK);
        // Only x's peer copy in the server held it open; once dropped, x
        // sees EOF.
        let mut fds = Vec::new();
        let mut one = [0u8; 16];
        assert!(matches!(recv_frag(x.as_fd(), &mut one, &mut fds), Ok(None)));
        drop(a);
        server.join().unwrap().unwrap();
    }
}

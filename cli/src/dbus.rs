//! A small D-Bus client for the system bus: what the sleep hook needs from
//! logind (or elogind, which has the same names), and nothing more. SASL
//! EXTERNAL, `Hello`, method calls whose arguments are strings, replies and
//! signals whose bodies are basic types (a variant of one included), and
//! unix fd passing (the inhibitor lock is a file descriptor). No library:
//! the wire format is small, and this keeps the CLI free of an async runtime.
//!
//! Only little-endian messages are written; both byte orders are read.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub const LOGIN1: &str = "org.freedesktop.login1";
pub const LOGIN1_PATH: &str = "/org/freedesktop/login1";
pub const LOGIN1_MANAGER: &str = "org.freedesktop.login1.Manager";
const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";

/// The longest message accepted (the spec's limit is 128 MiB; nothing here
/// comes near a megabyte).
const MAX_MESSAGE: usize = 1 << 20;

/// Message types.
pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

// Header field codes.
const F_PATH: u8 = 1;
const F_INTERFACE: u8 = 2;
const F_MEMBER: u8 = 3;
const F_ERROR_NAME: u8 = 4;
const F_REPLY_SERIAL: u8 = 5;
const F_DESTINATION: u8 = 6;
const F_SENDER: u8 = 7;
const F_SIGNATURE: u8 = 8;
const F_UNIX_FDS: u8 = 9;

/// A value of a basic type, as read from a body.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Double(f64),
    Str(String),
    /// An index into the message's file descriptors.
    Fd(u32),
}

/// One message as received.
#[derive(Debug, Default)]
pub struct Message {
    pub kind: u8,
    pub serial: u32,
    pub reply_serial: Option<u32>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub sender: Option<String>,
    pub signature: String,
    pub body: Vec<Value>,
    /// The descriptors sent with it, in order.
    pub fds: Vec<OwnedFd>,
}

impl Message {
    /// The error text of an error reply.
    pub fn error_text(&self) -> String {
        let name = self.error_name.as_deref().unwrap_or("error");
        match self.body.first() {
            Some(Value::Str(s)) => format!("{name}: {s}"),
            _ => name.to_string(),
        }
    }
}

// ---------------------------------------------------------------- writing

fn pad_to(buf: &mut Vec<u8>, align: usize) {
    while !buf.len().is_multiple_of(align) {
        buf.push(0);
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    pad_to(buf, 4);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn put_sig(buf: &mut Vec<u8>, s: &str) {
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// A header field: a struct (byte code, variant).
fn put_field(buf: &mut Vec<u8>, code: u8, sig: &str, val: &str) {
    pad_to(buf, 8);
    buf.push(code);
    put_sig(buf, sig);
    match sig {
        "g" => put_sig(buf, val),
        _ => put_str(buf, val),
    }
}

/// A method call whose arguments are all strings.
pub fn method_call(
    serial: u32,
    dest: &str,
    path: &str,
    iface: &str,
    member: &str,
    args: &[&str],
) -> Vec<u8> {
    let mut body = Vec::new();
    for a in args {
        put_str(&mut body, a);
    }
    let sig = "s".repeat(args.len());
    let mut m = vec![b'l', METHOD_CALL, 0, 1];
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(&serial.to_le_bytes());
    // The field array: its length, then (aligned to 8) the fields.
    m.extend_from_slice(&[0; 4]);
    pad_to(&mut m, 8);
    let start = m.len();
    put_field(&mut m, F_PATH, "o", path);
    put_field(&mut m, F_DESTINATION, "s", dest);
    put_field(&mut m, F_INTERFACE, "s", iface);
    put_field(&mut m, F_MEMBER, "s", member);
    if !sig.is_empty() {
        put_field(&mut m, F_SIGNATURE, "g", &sig);
    }
    let len = (m.len() - start) as u32;
    m[12..16].copy_from_slice(&len.to_le_bytes());
    pad_to(&mut m, 8);
    m.extend_from_slice(&body);
    m
}

// ---------------------------------------------------------------- reading

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    big: bool,
}

impl<'a> Reader<'a> {
    fn align(&mut self, n: usize) -> Result<()> {
        let to = self.pos.div_ceil(n) * n;
        if to > self.b.len() {
            bail!("message ends inside padding");
        }
        self.pos = to;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .context("message too short")?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.align(N)?;
        let mut a: [u8; N] = self.take(N)?.try_into().expect("N bytes");
        if self.big {
            a.reverse();
        }
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.fixed::<4>()?))
    }

    fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        if self.u8()? != 0 {
            bail!("string without its NUL");
        }
        String::from_utf8(s.to_vec()).context("string is not UTF-8")
    }

    fn signature(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        if self.u8()? != 0 {
            bail!("signature without its NUL");
        }
        String::from_utf8(s.to_vec()).context("signature is not UTF-8")
    }

    /// One value of the basic type `t`; a variant holding one is unwrapped.
    fn value(&mut self, t: u8) -> Result<Value> {
        Ok(match t {
            b'y' => Value::Byte(self.u8()?),
            b'b' => Value::Bool(self.u32()? != 0),
            b'n' => Value::I16(i16::from_le_bytes(self.fixed::<2>()?)),
            b'q' => Value::U16(u16::from_le_bytes(self.fixed::<2>()?)),
            b'i' => Value::I32(i32::from_le_bytes(self.fixed::<4>()?)),
            b'u' => Value::U32(self.u32()?),
            b'h' => Value::Fd(self.u32()?),
            b'x' => Value::I64(i64::from_le_bytes(self.fixed::<8>()?)),
            b't' => Value::U64(u64::from_le_bytes(self.fixed::<8>()?)),
            b'd' => Value::Double(f64::from_le_bytes(self.fixed::<8>()?)),
            b's' | b'o' => Value::Str(self.string()?),
            b'g' => Value::Str(self.signature()?),
            b'v' => {
                let sig = self.signature()?;
                match sig.as_bytes() {
                    [t] => self.value(*t)?,
                    _ => bail!("variant of {sig:?}: only basic types are read"),
                }
            }
            other => bail!("type {:?} is not read here", other as char),
        })
    }
}

/// The bytes a whole message takes, from its first 16 (`None`: not yet
/// known, fewer than 16 bytes).
pub fn message_len(head: &[u8]) -> Result<Option<usize>> {
    if head.len() < 16 {
        return Ok(None);
    }
    let big = match head[0] {
        b'l' => false,
        b'B' => true,
        e => bail!("bad byte order mark {e:#x}"),
    };
    let word = |o: usize| {
        let a: [u8; 4] = head[o..o + 4].try_into().expect("4 bytes");
        if big {
            u32::from_be_bytes(a)
        } else {
            u32::from_le_bytes(a)
        }
    };
    let body = word(4) as usize;
    let fields = word(12) as usize;
    let total = (16 + fields).div_ceil(8) * 8 + body;
    if total > MAX_MESSAGE {
        bail!("message of {total} bytes is too long");
    }
    Ok(Some(total))
}

/// Parse one whole message (no descriptors attached yet).
pub fn parse(b: &[u8]) -> Result<Message> {
    let total = message_len(b)?.context("message too short")?;
    if b.len() < total {
        bail!("message too short");
    }
    let mut r = Reader {
        b: &b[..total],
        pos: 0,
        big: b[0] == b'B',
    };
    r.take(1)?;
    let mut m = Message {
        kind: r.u8()?,
        ..Message::default()
    };
    r.u8()?; // flags
    if r.u8()? != 1 {
        bail!("not protocol version 1");
    }
    let body_len = r.u32()? as usize;
    m.serial = r.u32()?;
    let fields_len = r.u32()? as usize;
    r.align(8)?;
    let fields_end = r.pos + fields_len;
    while r.pos < fields_end {
        r.align(8)?;
        if r.pos >= fields_end {
            break;
        }
        let code = r.u8()?;
        let sig = r.signature()?;
        let [t] = sig.as_bytes() else {
            bail!("header field {code} of type {sig:?}");
        };
        let v = r.value(*t)?;
        let s = || match &v {
            Value::Str(s) => Some(s.clone()),
            _ => None,
        };
        match code {
            F_PATH => m.path = s(),
            F_INTERFACE => m.interface = s(),
            F_MEMBER => m.member = s(),
            F_ERROR_NAME => m.error_name = s(),
            F_SENDER => m.sender = s(),
            F_SIGNATURE => m.signature = s().unwrap_or_default(),
            F_REPLY_SERIAL | F_UNIX_FDS => {
                let Value::U32(n) = v else {
                    bail!("header field {code} is not a u32");
                };
                if code == F_REPLY_SERIAL {
                    m.reply_serial = Some(n);
                }
            }
            _ => {}
        }
    }
    r.align(8)?;
    if r.b.len() - r.pos != body_len {
        bail!("body length does not match the header");
    }
    for t in m.signature.clone().bytes() {
        m.body.push(r.value(t)?);
    }
    Ok(m)
}

/// The number of descriptors a message says it carries.
fn unix_fds(b: &[u8]) -> usize {
    // Cheap re-parse of the one field; a message `parse` accepted has it right.
    let Ok(Some(total)) = message_len(b) else {
        return 0;
    };
    let mut r = Reader {
        b: &b[..total.min(b.len())],
        pos: 12,
        big: b[0] == b'B',
    };
    let Ok(fields_len) = r.u32().map(|n| n as usize) else {
        return 0;
    };
    if r.align(8).is_err() {
        return 0;
    }
    let end = r.pos + fields_len;
    while r.pos < end {
        if r.align(8).is_err() || r.pos >= end {
            return 0;
        }
        let (Ok(code), Ok(sig)) = (r.u8(), r.signature()) else {
            return 0;
        };
        let [t] = sig.as_bytes() else { return 0 };
        match r.value(*t) {
            Ok(Value::U32(n)) if code == F_UNIX_FDS => return n as usize,
            Ok(_) => {}
            Err(_) => return 0,
        }
    }
    0
}

/// `PrepareForSleep(bool)` from logind: `Some(true)` before sleep, `Some(false)` after.
pub fn prepare_for_sleep(m: &Message) -> Option<bool> {
    if m.kind != SIGNAL
        || m.interface.as_deref() != Some(LOGIN1_MANAGER)
        || m.member.as_deref() != Some("PrepareForSleep")
        || m.path.as_deref() != Some(LOGIN1_PATH)
    {
        return None;
    }
    match m.body.as_slice() {
        [Value::Bool(b)] => Some(*b),
        _ => None,
    }
}

/// The match rule for logind's sleep signal.
pub fn sleep_match_rule() -> String {
    format!(
        "type='signal',sender='{LOGIN1}',interface='{LOGIN1_MANAGER}',member='PrepareForSleep',path='{LOGIN1_PATH}'"
    )
}

/// Where the system bus is: `DBUS_SYSTEM_BUS_ADDRESS`'s first `unix:path=`
/// (or `unix:abstract=`), else the standard socket.
pub fn system_bus_address(env: Option<&str>) -> (String, bool) {
    if let Some(a) = env {
        for part in a.split(';') {
            let Some(rest) = part.strip_prefix("unix:") else {
                continue;
            };
            for kv in rest.split(',') {
                if let Some(p) = kv.strip_prefix("path=") {
                    return (unescape(p), false);
                }
                if let Some(p) = kv.strip_prefix("abstract=") {
                    return (unescape(p), true);
                }
            }
        }
    }
    ("/run/dbus/system_bus_socket".into(), false)
}

/// D-Bus address values escape bytes as %XX.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `AUTH EXTERNAL` takes the uid as hex-coded ASCII digits.
pub fn auth_line(uid: u32) -> String {
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("AUTH EXTERNAL {hex}\r\n")
}

// ---------------------------------------------------------------- the connection

/// A connection to the system bus.
pub struct Bus {
    s: UnixStream,
    serial: u32,
    rbuf: Vec<u8>,
    rfds: VecDeque<OwnedFd>,
    /// Signals that arrived while a call waited for its reply.
    queued: VecDeque<Message>,
    pub fds_ok: bool,
}

impl Bus {
    pub fn system() -> Result<Bus> {
        let env = std::env::var("DBUS_SYSTEM_BUS_ADDRESS").ok();
        let (addr, abstract_ns) = system_bus_address(env.as_deref());
        let s = if abstract_ns {
            use std::os::linux::net::SocketAddrExt;
            let sa = std::os::unix::net::SocketAddr::from_abstract_name(addr.as_bytes())?;
            UnixStream::connect_addr(&sa)
        } else {
            UnixStream::connect(&addr)
        }
        .with_context(|| format!("connecting to the system bus at {addr}"))?;
        let mut b = Bus {
            s,
            serial: 0,
            rbuf: Vec::new(),
            rfds: VecDeque::new(),
            queued: VecDeque::new(),
            fds_ok: false,
        };
        b.auth()?;
        let hello = b.call(DBUS, DBUS_PATH, DBUS, "Hello", &[], Duration::from_secs(5))?;
        if hello.kind == ERROR {
            bail!("Hello: {}", hello.error_text());
        }
        Ok(b)
    }

    fn auth(&mut self) -> Result<()> {
        self.s.set_read_timeout(Some(Duration::from_secs(5)))?;
        self.s.set_write_timeout(Some(Duration::from_secs(5)))?;
        self.s.write_all(b"\0")?;
        self.s
            .write_all(auth_line(crate::paths::uid()).as_bytes())?;
        let ok = self.auth_reply()?;
        if !ok.starts_with("OK ") {
            bail!("the system bus refused EXTERNAL authentication: {ok}");
        }
        self.s.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        self.fds_ok = self.auth_reply()?.starts_with("AGREE_UNIX_FD");
        self.s.write_all(b"BEGIN\r\n")?;
        Ok(())
    }

    /// One line of the authentication dialogue (read byte by byte: nothing
    /// after it may be consumed).
    fn auth_reply(&mut self) -> Result<String> {
        let mut line = Vec::new();
        let mut c = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            if self.s.read(&mut c)? == 0 {
                bail!("the system bus closed the connection while authenticating");
            }
            line.push(c[0]);
            if line.len() > 512 {
                bail!("authentication reply too long");
            }
        }
        Ok(String::from_utf8_lossy(&line).trim_end().to_string())
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// Call a method and wait up to `timeout` for its reply (or error).
    /// Signals that arrive meanwhile are kept for [`Bus::next_signal`].
    pub fn call(
        &mut self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Message> {
        let serial = self.next_serial();
        let msg = method_call(serial, dest, path, iface, member, args);
        self.s.set_write_timeout(Some(timeout))?;
        self.s.write_all(&msg)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("{iface}.{member}: no reply within {} s", timeout.as_secs());
            }
            let Some(m) = self.read_message(left)? else {
                continue;
            };
            match m.kind {
                METHOD_RETURN | ERROR if m.reply_serial == Some(serial) => return Ok(m),
                SIGNAL => self.queued.push_back(m),
                _ => {}
            }
        }
    }

    /// Ask the bus to deliver signals matching `rule`.
    pub fn add_match(&mut self, rule: &str) -> Result<()> {
        let r = self.call(
            DBUS,
            DBUS_PATH,
            DBUS,
            "AddMatch",
            &[rule],
            Duration::from_secs(5),
        )?;
        if r.kind == ERROR {
            bail!("AddMatch: {}", r.error_text());
        }
        Ok(())
    }

    /// The unique name owning `name`, `None` when nobody does.
    pub fn name_owner(&mut self, name: &str) -> Result<Option<String>> {
        let r = self.call(
            DBUS,
            DBUS_PATH,
            DBUS,
            "GetNameOwner",
            &[name],
            Duration::from_secs(5),
        )?;
        Ok(match (r.kind, r.body.first()) {
            (METHOD_RETURN, Some(Value::Str(s))) => Some(s.clone()),
            _ => None,
        })
    }

    /// The next signal, waiting up to `timeout`; `None` when none came.
    pub fn next_signal(&mut self, timeout: Duration) -> Result<Option<Message>> {
        if let Some(m) = self.queued.pop_front() {
            return Ok(Some(m));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.read_message(left)? {
                Some(m) if m.kind == SIGNAL => return Ok(Some(m)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// One message, or `None` when `timeout` passes first.
    fn read_message(&mut self, timeout: Duration) -> Result<Option<Message>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(total) = message_len(&self.rbuf)? {
                if self.rbuf.len() >= total {
                    let bytes: Vec<u8> = self.rbuf.drain(..total).collect();
                    let mut m = parse(&bytes)?;
                    let n = unix_fds(&bytes);
                    for _ in 0..n {
                        match self.rfds.pop_front() {
                            Some(fd) => m.fds.push(fd),
                            None => break,
                        }
                    }
                    return Ok(Some(m));
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            if !self.fill(left)? {
                return Ok(None);
            }
        }
    }

    /// Read what is there (with any descriptors); false on timeout.
    fn fill(&mut self, timeout: Duration) -> Result<bool> {
        self.s
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut data = [0u8; 4096];
        // Room for 16 descriptors.
        let mut cbuf = [0u64; 16];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        // SAFETY: a zeroed msghdr pointing at live buffers for the call.
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cbuf.as_mut_ptr().cast();
        hdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;
        // SAFETY: recvmsg on our socket with the buffers set up above.
        let n = unsafe { libc::recvmsg(self.s.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return match e.kind() {
                std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted => Ok(false),
                _ => Err(e).context("reading from the system bus"),
            };
        }
        if n == 0 {
            bail!("the system bus closed the connection");
        }
        // SAFETY: walking the control messages recvmsg filled in, with the
        // libc helpers, within msg_controllen.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&hdr);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const i32;
                    let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    for i in 0..len / 4 {
                        let fd = std::ptr::read_unaligned(data.add(i));
                        self.rfds.push_back(OwnedFd::from_raw_fd(fd));
                    }
                }
                c = libc::CMSG_NXTHDR(&hdr, c);
            }
        }
        self.rbuf.extend_from_slice(&data[..n as usize]);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_method_call_is_laid_out_as_the_spec_says() {
        let m = method_call(
            7,
            LOGIN1,
            LOGIN1_PATH,
            LOGIN1_MANAGER,
            "Inhibit",
            &["sleep", "x"],
        );
        assert_eq!(&m[..4], &[b'l', 1, 0, 1]);
        assert_eq!(u32::from_le_bytes(m[8..12].try_into().unwrap()), 7);
        // Body: "sleep" (4 + 5 + 1 = 10 bytes), padded to 12, then "x" (4 + 1 + 1).
        assert_eq!(u32::from_le_bytes(m[4..8].try_into().unwrap()), 18);
        assert_eq!(message_len(&m).unwrap(), Some(m.len()));
        // It reads back.
        let p = parse(&m).unwrap();
        assert_eq!(p.kind, METHOD_CALL);
        assert_eq!(p.serial, 7);
        assert_eq!(p.path.as_deref(), Some(LOGIN1_PATH));
        assert_eq!(p.interface.as_deref(), Some(LOGIN1_MANAGER));
        assert_eq!(p.member.as_deref(), Some("Inhibit"));
        assert_eq!(p.signature, "ss");
        assert_eq!(
            p.body,
            vec![Value::Str("sleep".into()), Value::Str("x".into())]
        );
        // No arguments: no signature field, no body.
        let h = method_call(1, DBUS, DBUS_PATH, DBUS, "Hello", &[]);
        let p = parse(&h).unwrap();
        assert!(p.signature.is_empty() && p.body.is_empty());
        assert_eq!(h.len() % 8, 0);
    }

    /// `PrepareForSleep(true)` as logind sends it, written out by hand from
    /// the specification (little endian): fields PATH, INTERFACE, MEMBER,
    /// SIGNATURE "b", SENDER, then the body, one boolean.
    fn prepare_for_sleep_bytes(on: bool, big: bool) -> Vec<u8> {
        let mut f: Vec<u8> = Vec::new();
        let mut field = |code: u8, sig: u8, val: &str| {
            while !f.len().is_multiple_of(8) {
                f.push(0);
            }
            f.extend_from_slice(&[code, 1, sig, 0]);
            if sig == b'g' {
                f.push(val.len() as u8);
            } else {
                let n = val.len() as u32;
                f.extend_from_slice(&if big {
                    n.to_be_bytes()
                } else {
                    n.to_le_bytes()
                });
            }
            f.extend_from_slice(val.as_bytes());
            f.push(0);
        };
        field(1, b'o', "/org/freedesktop/login1");
        field(2, b's', "org.freedesktop.login1.Manager");
        field(3, b's', "PrepareForSleep");
        field(8, b'g', "b");
        field(7, b's', ":1.4");
        let w = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut m = vec![if big { b'B' } else { b'l' }, 4, 1, 1];
        m.extend_from_slice(&w(4));
        m.extend_from_slice(&w(1234));
        m.extend_from_slice(&w(f.len() as u32));
        m.extend_from_slice(&f);
        while !m.len().is_multiple_of(8) {
            m.push(0);
        }
        m.extend_from_slice(&w(on as u32));
        m
    }

    #[test]
    fn logind_sleep_signals_are_recognised_in_both_byte_orders() {
        for big in [false, true] {
            for on in [true, false] {
                let b = prepare_for_sleep_bytes(on, big);
                assert_eq!(message_len(&b).unwrap(), Some(b.len()));
                let m = parse(&b).unwrap();
                assert_eq!(m.kind, SIGNAL);
                assert_eq!(m.serial, 1234);
                assert_eq!(m.sender.as_deref(), Some(":1.4"));
                assert_eq!(prepare_for_sleep(&m), Some(on), "big={big}");
            }
        }
        // Another member, or a body that is not one boolean: not it.
        let mut m = parse(&prepare_for_sleep_bytes(true, false)).unwrap();
        m.member = Some("PrepareForShutdown".into());
        assert_eq!(prepare_for_sleep(&m), None);
        let mut m = parse(&prepare_for_sleep_bytes(true, false)).unwrap();
        m.body = vec![Value::U32(1)];
        assert_eq!(prepare_for_sleep(&m), None);
    }

    #[test]
    fn short_or_broken_messages_are_refused() {
        let b = prepare_for_sleep_bytes(true, false);
        assert_eq!(message_len(&b[..10]).unwrap(), None);
        assert!(parse(&b[..b.len() - 1]).is_err());
        let mut bad = b.clone();
        bad[0] = b'x';
        assert!(message_len(&bad).is_err());
        let mut huge = b.clone();
        huge[4..8].copy_from_slice(&(64u32 << 20).to_le_bytes());
        assert!(message_len(&huge).is_err());
        let mut v2 = b;
        v2[3] = 2;
        assert!(parse(&v2).is_err());
    }

    #[test]
    fn replies_with_a_descriptor_or_a_variant() {
        // An Inhibit reply: signature "h", body index 0, UNIX_FDS 1.
        let mut f: Vec<u8> = Vec::new();
        f.extend_from_slice(&[5, 1, b'u', 0]);
        f.extend_from_slice(&7u32.to_le_bytes());
        f.extend_from_slice(&[8, 1, b'g', 0, 1, b'h', 0, 0]);
        f.extend_from_slice(&[9, 1, b'u', 0]);
        f.extend_from_slice(&1u32.to_le_bytes());
        let mut m = vec![b'l', 2, 1, 1];
        m.extend_from_slice(&4u32.to_le_bytes());
        m.extend_from_slice(&9u32.to_le_bytes());
        m.extend_from_slice(&(f.len() as u32).to_le_bytes());
        m.extend_from_slice(&f);
        while !m.len().is_multiple_of(8) {
            m.push(0);
        }
        m.extend_from_slice(&0u32.to_le_bytes());
        let p = parse(&m).unwrap();
        assert_eq!(p.kind, METHOD_RETURN);
        assert_eq!(p.reply_serial, Some(7));
        assert_eq!(p.body, vec![Value::Fd(0)]);
        assert_eq!(unix_fds(&m), 1);

        // A Properties.Get reply: a variant holding a boolean. The field
        // array's length leaves out the padding after its last field.
        let mut f: Vec<u8> = Vec::new();
        f.extend_from_slice(&[8, 1, b'g', 0, 1, b'v', 0, 0]);
        f.extend_from_slice(&[5, 1, b'u', 0]);
        f.extend_from_slice(&3u32.to_le_bytes());
        let mut m = vec![b'l', 2, 1, 1];
        m.extend_from_slice(&8u32.to_le_bytes());
        m.extend_from_slice(&10u32.to_le_bytes());
        m.extend_from_slice(&(f.len() as u32).to_le_bytes());
        m.extend_from_slice(&f);
        while !m.len().is_multiple_of(8) {
            m.push(0);
        }
        m.extend_from_slice(&[1, b'b', 0, 0]);
        m.extend_from_slice(&1u32.to_le_bytes());
        let p = parse(&m).unwrap();
        assert_eq!(p.body, vec![Value::Bool(true)]);
        assert_eq!(unix_fds(&m), 0);
    }

    #[test]
    fn bus_address_and_auth() {
        assert_eq!(
            system_bus_address(None),
            ("/run/dbus/system_bus_socket".into(), false)
        );
        assert_eq!(
            system_bus_address(Some("unix:path=/var/run/dbus/system_bus_socket")),
            ("/var/run/dbus/system_bus_socket".into(), false)
        );
        assert_eq!(
            system_bus_address(Some("tcp:host=x;unix:abstract=/tmp/dbus-X,guid=1")),
            ("/tmp/dbus-X".into(), true)
        );
        assert_eq!(
            system_bus_address(Some("unix:path=/run/a%20b")),
            ("/run/a b".into(), false)
        );
        assert_eq!(auth_line(1000), "AUTH EXTERNAL 31303030\r\n");
        assert_eq!(auth_line(0), "AUTH EXTERNAL 30\r\n");
        assert!(sleep_match_rule().contains("member='PrepareForSleep'"));
        assert!(sleep_match_rule().contains("sender='org.freedesktop.login1'"));
    }
}

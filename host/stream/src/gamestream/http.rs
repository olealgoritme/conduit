//! Just enough HTTP/1.1 (and RTSP, which shares the shape) for GameStream:
//! one request per connection, GET with a query string, small bodies.
//! Everything here parses bytes from the network, so it is bounded and strict.

use std::collections::HashMap;
use std::io::{self, Read, Write};

pub const MAX_HEAD: usize = 16 * 1024;
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Default, Clone)]
pub struct Request {
    pub method: String,
    /// Path without the query, e.g. "/serverinfo".
    pub path: String,
    /// The whole target as sent (RTSP needs it).
    pub target: String,
    pub query: HashMap<String, String>,
    /// Header names lower-cased.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn q(&self, k: &str) -> Option<&str> {
        self.query.get(k).map(String::as_str)
    }
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .get(&k.to_ascii_lowercase())
            .map(String::as_str)
    }
}

fn pct_decode(s: &str) -> String {
    let hexv = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hexv(b[i + 1]), hexv(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (pct_decode(k).to_ascii_lowercase(), pct_decode(v)),
            None => (pct_decode(kv).to_ascii_lowercase(), String::new()),
        })
        .collect()
}

/// Parse a request head + body from `buf` (complete message). None = incomplete
/// or malformed.
pub fn parse(buf: &[u8]) -> Option<(Request, usize)> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split(' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let _protocol = parts.next();
    if method.is_empty() || target.is_empty() || parts.next().is_some() {
        return None;
    }
    let mut headers = HashMap::new();
    for l in lines {
        let (k, v) = l.split_once(':')?;
        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let len: usize = headers
        .get("content-length")
        .map(|v| v.parse().ok())
        .unwrap_or(Some(0))?;
    if len > MAX_BODY {
        return None;
    }
    let total = end + 4 + len;
    if buf.len() < total {
        return None;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.clone(), HashMap::new()),
    };
    Some((
        Request {
            method,
            path,
            target,
            query,
            headers,
            body: buf[end + 4..total].to_vec(),
        },
        total,
    ))
}

/// Read one request (head + Content-Length body).
pub fn read_request(s: &mut impl Read) -> io::Result<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some((r, _)) = parse(&buf) {
            return Ok(r);
        }
        if buf.len() > MAX_HEAD + MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
    }
}

pub fn respond(
    s: &mut impl Write,
    code: u16,
    reason: &str,
    ctype: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut all = head.into_bytes();
    all.extend_from_slice(body);
    s.write_all(&all)?;
    s.flush()
}

pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            c => o.push(c),
        }
    }
    o
}

/// `<root status_code="..">` + children, as GameStream clients expect.
pub fn xml(status: u16, message: Option<&str>, children: &[(&str, String)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"");
    s.push_str(&status.to_string());
    s.push('"');
    if let Some(m) = message {
        s.push_str(" status_message=\"");
        s.push_str(&xml_escape(m));
        s.push('"');
    }
    s.push('>');
    for (k, v) in children {
        s.push('<');
        s.push_str(k);
        s.push('>');
        s.push_str(&xml_escape(v));
        s.push_str("</");
        s.push_str(k);
        s.push('>');
    }
    s.push_str("</root>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gamestream_requests() {
        let raw = b"GET /pair?uniqueid=0123456789ABCDEF&devicename=roth&updateState=1&phrase=getservercert&salt=AB&clientcert=2D2D HTTP/1.1\r\nHost: 10.0.0.2:47989\r\n\r\n";
        let (r, n) = parse(raw).unwrap();
        assert_eq!(n, raw.len());
        assert_eq!(r.path, "/pair");
        assert_eq!(r.q("phrase"), Some("getservercert"));
        assert_eq!(r.q("devicename"), Some("roth"));
        assert_eq!(r.header("HOST"), Some("10.0.0.2:47989"));
    }

    #[test]
    fn rtsp_with_body() {
        let raw = b"ANNOUNCE streamid=control/13/0 RTSP/1.0\r\nCSeq: 6\r\nContent-length: 5\r\n\r\nv=0\r\nEXTRA";
        let (r, n) = parse(raw).unwrap();
        assert_eq!(r.method, "ANNOUNCE");
        assert_eq!(r.body, b"v=0\r\n");
        assert_eq!(n, raw.len() - 5);
        assert!(parse(b"ANNOUNCE x RTSP/1.0\r\nContent-Length: 10\r\n\r\nshort").is_none());
        assert!(parse(b"GET / HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n").is_none());
    }

    #[test]
    fn percent_and_xml() {
        let q = parse_query("devicename=my%20phone&x=a+b&bad=%zz");
        assert_eq!(q["devicename"], "my phone");
        assert_eq!(q["x"], "a b");
        assert_eq!(q["bad"], "%zz");
        assert_eq!(
            xml(200, None, &[("hostname", "a<b".into())]),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><hostname>a&lt;b</hostname></root>"
        );
    }
}

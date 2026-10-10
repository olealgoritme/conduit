//! The host-to-guest control channel, carried over the VM's `org.conduit.ctl.0`
//! virtio-serial port (no network, no ssh). The host (`conduit run`, `apps`,
//! `cp`, the dashboard) sends one JSON object per line, the guest agent (the
//! Windows tray app, the Linux guest agent) answers each with one line.
//!
//! ```text
//! {"v":1,"id":7,"op":"run","cmd":"notepad.exe","args":[],"cwd":"C:\\"}
//! {"id":7,"ok":true,"pid":4242}
//! {"id":8,"ok":false,"error":"no such file"}
//! ```
//!
//! A request carries the protocol version `v`, a caller-chosen `id` that the
//! response repeats, and `op` plus that op's fields (see [`Op`]). A response
//! is `ok` with the op's fields, or not `ok` with an `error` text. Unknown
//! fields are ignored by both sides. Lines are at most [`MAX_LINE`] bytes.
//!
//! File transfer ([`Op::Put`], [`Op::Get`]) moves a file in [`CHUNK`]-byte
//! pieces, base64 coded, each acknowledged; the last piece carries the
//! file's SHA-256 ([`sha256_hex`]), which the receiver checks.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod b64;
mod sha256;

pub use b64::{decode as b64_decode, encode as b64_encode};
pub use sha256::{sha256_hex, Sha256};

/// The virtio-serial port name; the guest opens `\\.\Global\org.conduit.ctl.0`
/// on Windows and `/dev/virtio-ports/org.conduit.ctl.0` on Linux.
pub const CHANNEL: &str = "org.conduit.ctl.0";

/// The protocol version this crate speaks.
pub const VERSION: u32 = 1;

/// The longest line either side accepts; a longer one is dropped.
pub const MAX_LINE: usize = 4 << 20;

/// Raw bytes per `put`/`get` piece (about 350 KB once base64 coded).
pub const CHUNK: usize = 256 << 10;

/// The largest file `put`/`get` will move. Bigger files belong in a shared folder.
pub const MAX_FILE: u64 = 8 << 30;

/// Longest `icon` image the guest returns, in bytes (before base64).
pub const MAX_ICON: usize = 512 << 10;

/// A request: the common header plus the op.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

impl Request {
    pub fn new(id: u64, op: Op) -> Request {
        Request { v: VERSION, id, op }
    }

    /// One line, newline included.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("a Request always serializes");
        s.push('\n');
        s
    }

    /// Parse one line. `Err` carries the id (when the line has one) and the
    /// text to answer with.
    pub fn parse(line: &str) -> Result<Request, (Option<u64>, String)> {
        let val: Value =
            serde_json::from_str(line.trim()).map_err(|e| (None, format!("bad json: {e}")))?;
        let id = val.get("id").and_then(Value::as_u64);
        match val.get("v").and_then(Value::as_u64) {
            Some(v) if v >= 1 && v <= VERSION as u64 => {}
            Some(v) => return Err((id, format!("unsupported protocol version {v}"))),
            None => return Err((id, "missing protocol version".into())),
        }
        if id.is_none() {
            return Err((None, "missing id".into()));
        }
        if val.get("op").is_none() {
            return Err((id, "missing op".into()));
        }
        serde_json::from_value(val).map_err(|e| (id, format!("bad request: {e}")))
    }
}

/// What a request asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    /// Is the agent there? Answers [`Pong`].
    Ping,
    /// Start a program in the user's desktop session. Answers [`Started`].
    Run(RunArgs),
    /// End a program [`Op::Run`] started (and its children). Answers no fields.
    Stop { pid: u32 },
    /// The installed applications. Answers [`Apps`].
    Apps,
    /// The icon of an app, by its `icon` key. Answers [`Icon`].
    Icon { key: String },
    /// Write one piece of a file to the guest. Answers [`Written`].
    Put(PutArgs),
    /// Read one piece of a file from the guest. Answers [`Chunk`].
    Get(GetArgs),
    /// List a directory. Answers [`Listing`].
    Ls { path: String },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunArgs {
    /// A program, a document, a `.lnk` or `.desktop` file, or a URL such as
    /// `steam://rungameid/730`: whatever the guest's shell can open.
    pub cmd: String,
    pub args: Vec<String>,
    /// Working directory; empty for the program's own.
    pub cwd: String,
    /// Extra environment variables.
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PutArgs {
    /// Absolute guest path; a bare file name goes to the user's Downloads folder.
    pub path: String,
    /// Position of this piece. 0 starts the file over (into a temporary
    /// file next to the target).
    pub offset: u64,
    /// This piece, base64 coded.
    pub data: String,
    /// The whole file's size; sent with the first piece.
    pub size: Option<u64>,
    /// The last piece: the guest checks `size` and `sha256`, then moves the
    /// temporary file to `path`.
    pub done: bool,
    pub sha256: Option<String>,
    /// Replace an existing file. Without it, a target that exists is refused
    /// (with the first piece, so nothing is sent for nothing).
    pub force: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetArgs {
    pub path: String,
    pub offset: u64,
    /// Bytes wanted, at most [`CHUNK`].
    pub len: usize,
}

/// `ping`: what the guest agent is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pong {
    /// The protocol version the agent speaks.
    pub proto: u32,
    pub agent: String,
    /// `windows` or `linux`.
    pub os: String,
    /// The desktop user the agent runs programs for.
    pub user: String,
    /// A desktop session is there to run programs in.
    pub session: bool,
    /// The user's Downloads folder (where a bare `put` name lands).
    pub downloads: String,
}

/// `run`: the started process. `pid` is 0 when the shell handled a URL or
/// document and there is no process of ours to stop.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Started {
    pub pid: u32,
}

/// One installed application.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct App {
    pub name: String,
    /// What `run` takes as `cmd`: a `.lnk` / `.desktop` path or a `steam://` URL.
    pub target: String,
    pub args: Vec<String>,
    /// What `icon` takes as `key`; empty when the app has none.
    pub icon: String,
    /// Where it was found: `startmenu`, `steam`, `desktop`, `flatpak`, `snap`.
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Apps {
    pub apps: Vec<App>,
}

/// `icon`: an image, base64 coded; about 64 px square when it is a bitmap.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Icon {
    /// `png`, or `svg` when the guest has no way to rasterize one.
    pub format: String,
    pub data: String,
}

/// `put`: how far the file has got.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Written {
    /// The file's bytes so far.
    pub size: u64,
    /// The final path, once `done`.
    pub path: String,
    /// The SHA-256 the guest computed, once `done`.
    pub sha256: String,
}

/// `get`: one piece.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Chunk {
    pub data: String,
    /// The whole file's size.
    pub size: u64,
    /// This piece reaches the end of the file.
    pub eof: bool,
    /// The whole file's SHA-256, with the last piece.
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Unix time, seconds; 0 when unknown.
    pub mtime: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listing {
    /// The directory listed, as the guest spells it.
    pub path: String,
    pub entries: Vec<Entry>,
}

/// A response: `ok` with the op's fields, or not with `error`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub body: Map<String, Value>,
}

impl Response {
    pub fn ok<T: Serialize>(id: u64, body: &T) -> Response {
        let body = match serde_json::to_value(body) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        Response {
            id,
            ok: true,
            error: None,
            body,
        }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Response {
        Response {
            id,
            ok: false,
            error: Some(error.into()),
            body: Map::new(),
        }
    }

    /// One line, newline included.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("a Response always serializes");
        s.push('\n');
        s
    }

    pub fn parse(line: &str) -> Option<Response> {
        serde_json::from_str(line.trim()).ok()
    }

    /// The op's fields, or the guest's error text.
    pub fn into_body<T: for<'a> Deserialize<'a>>(self) -> Result<T, String> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| "the guest refused".into()));
        }
        serde_json::from_value(Value::Object(self.body)).map_err(|e| format!("bad response: {e}"))
    }
}

/// Splits a byte stream into lines, dropping any longer than [`MAX_LINE`].
#[derive(Default)]
pub struct LineReader {
    buf: Vec<u8>,
    skipping: bool,
}

impl LineReader {
    /// Feed bytes; get every complete line in them (without the newline).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if !self.skipping && !self.buf.is_empty() {
                    if let Ok(s) = String::from_utf8(std::mem::take(&mut self.buf)) {
                        out.push(s);
                    }
                }
                self.buf.clear();
                self.skipping = false;
            } else if !self.skipping {
                if self.buf.len() >= MAX_LINE {
                    self.buf.clear();
                    self.skipping = true;
                } else {
                    self.buf.push(b);
                }
            }
        }
        out
    }

    /// Forget a partly received line (the connection broke).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.skipping = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_round_trips() {
        let ops = [
            Op::Ping,
            Op::Run(RunArgs {
                cmd: "notepad.exe".into(),
                args: vec!["a b".into()],
                cwd: "C:\\".into(),
                env: [("K".to_string(), "V".to_string())].into(),
            }),
            Op::Stop { pid: 5 },
            Op::Apps,
            Op::Icon { key: "k".into() },
            Op::Put(PutArgs {
                path: "/tmp/x".into(),
                offset: 3,
                data: "AAAA".into(),
                size: Some(9),
                done: true,
                sha256: Some("ab".into()),
                force: true,
            }),
            Op::Get(GetArgs {
                path: "p".into(),
                offset: 1,
                len: 2,
            }),
            Op::Ls { path: "/".into() },
        ];
        for (i, op) in ops.into_iter().enumerate() {
            let r = Request::new(i as u64, op);
            let line = r.to_line();
            assert_eq!(line.matches('\n').count(), 1);
            assert_eq!(Request::parse(&line), Ok(r));
        }
    }

    #[test]
    fn unicode_and_spaces_survive() {
        let r = Request::new(
            1,
            Op::Get(GetArgs {
                path: "C:\\Users\\Ære Ø\\Mine Filer\\日本語 \"q\".txt".into(),
                ..GetArgs::default()
            }),
        );
        assert_eq!(Request::parse(&r.to_line()), Ok(r));
    }

    #[test]
    fn wire_shape_is_flat() {
        let r = Request::new(7, Op::Stop { pid: 9 });
        assert_eq!(
            r.to_line(),
            "{\"v\":1,\"id\":7,\"op\":\"stop\",\"pid\":9}\n"
        );
        let r = Request::parse(r#"{"v":1,"id":2,"op":"run","cmd":"x"}"#).unwrap();
        assert_eq!(
            r.op,
            Op::Run(RunArgs {
                cmd: "x".into(),
                ..RunArgs::default()
            })
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let r = Request::parse(r#"{"v":1,"id":1,"op":"ping","future":[1]}"#).unwrap();
        assert_eq!(r.op, Op::Ping);
    }

    #[test]
    fn bad_requests_say_why_and_keep_the_id() {
        let e = Request::parse(r#"{"v":1,"id":4,"op":"frobnicate"}"#).unwrap_err();
        assert_eq!(e.0, Some(4));
        assert!(e.1.contains("bad request"), "{}", e.1);
        assert_eq!(
            Request::parse(r#"{"v":9,"id":4,"op":"ping"}"#)
                .unwrap_err()
                .0,
            Some(4)
        );
        assert!(Request::parse(r#"{"id":4,"op":"ping"}"#)
            .unwrap_err()
            .1
            .contains("version"));
        assert!(Request::parse(r#"{"v":1,"op":"ping"}"#)
            .unwrap_err()
            .1
            .contains("id"));
        assert!(Request::parse(r#"{"v":1,"id":3}"#)
            .unwrap_err()
            .1
            .contains("op"));
        // Wrong field types and non-objects are errors, not panics.
        assert!(Request::parse(r#"{"v":1,"id":3,"op":"stop","pid":"x"}"#).is_err());
        assert!(Request::parse("[1,2]").is_err());
        assert!(Request::parse("null").is_err());
        assert_eq!(Request::parse("nope").unwrap_err().0, None);
    }

    #[test]
    fn responses() {
        let p = Pong {
            proto: 1,
            agent: "t".into(),
            os: "windows".into(),
            user: "u".into(),
            session: true,
            downloads: "C:\\Users\\u\\Downloads".into(),
        };
        let line = Response::ok(3, &p).to_line();
        assert!(line.starts_with("{\"id\":3,\"ok\":true,"), "{line}");
        let back = Response::parse(&line).unwrap();
        assert_eq!(back.id, 3);
        assert_eq!(back.into_body::<Pong>(), Ok(p));

        let e = Response::err(4, "nope");
        assert_eq!(e.to_line(), "{\"id\":4,\"ok\":false,\"error\":\"nope\"}\n");
        assert_eq!(
            Response::parse(&e.to_line()).unwrap().into_body::<Pong>(),
            Err("nope".into())
        );
        assert_eq!(Response::parse("junk"), None);
    }

    #[test]
    fn apps_and_listing_bodies() {
        let a = Apps {
            apps: vec![App {
                name: "Notepad".into(),
                target: "C:\\x.lnk".into(),
                source: "startmenu".into(),
                ..App::default()
            }],
        };
        let r = Response::ok(1, &a);
        assert_eq!(r.into_body::<Apps>(), Ok(a));
        let none = Response::ok(1, &Started::default());
        assert_eq!(none.into_body::<Started>(), Ok(Started { pid: 0 }));
        // A body of the wrong shape is an error, not a default.
        let wrong = Response::parse(r#"{"id":1,"ok":true,"apps":5}"#).unwrap();
        assert!(wrong.into_body::<Apps>().is_err());
    }

    #[test]
    fn line_reader_splits_and_drops_overlong_lines() {
        let mut r = LineReader::default();
        assert_eq!(r.push(b"ab"), Vec::<String>::new());
        assert_eq!(r.push(b"c\nde\n\nf"), ["abc", "de"]);
        assert_eq!(r.push(b"\n"), ["f"]);
        let long = vec![b'x'; MAX_LINE + 10];
        assert!(r.push(&long).is_empty());
        assert_eq!(r.push(b"tail\nok\n"), ["ok"]);
        // Invalid UTF-8 drops that line only.
        assert_eq!(r.push(b"\xff\xfe\nfine\n"), ["fine"]);
        r.push(b"half");
        r.reset();
        assert_eq!(r.push(b"x\n"), ["x"]);
    }
}

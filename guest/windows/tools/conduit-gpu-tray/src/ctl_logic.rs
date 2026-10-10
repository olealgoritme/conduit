//! The control channel's request handling, independent of Windows: parse a
//! line, run the op, answer with one line that fits `MAX_LINE`. Files are
//! served here with `std::fs`; what needs the OS (starting programs, icons,
//! drives) comes through [`Backend`].
//!
//! The tray runs elevated, so every file op (`put`, `get`, `ls`) runs inside
//! [`Backend::as_user`]: on Windows with the desktop user's normal token, so
//! the host can reach exactly what the user can, and a junction or link a
//! non-elevated process plants is followed with the user's rights only. A
//! transfer's temporary file has a fresh name per transfer, is created with
//! `create_new` (never an existing file or link) and only that file is ever
//! deleted.

use conduit_ctl::{
    b64_decode, b64_encode, App, Apps, Chunk, Entry, GetArgs, Icon, Listing, Op, Pong, PutArgs,
    Request, Response, RunArgs, Sha256, Started, Written, CHUNK, MAX_FILE, MAX_ICON, MAX_LINE,
    VERSION,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

/// Most directory entries one `ls` returns.
pub const MAX_ENTRIES: usize = 5000;
/// Transfers in flight that are remembered; the oldest idle one is dropped.
const MAX_SESSIONS: usize = 64;

/// What the agent asks of the operating system.
pub trait Backend: Send + Sync {
    fn user(&self) -> String;
    /// A desktop session exists to run programs in.
    fn session(&self) -> bool;
    fn downloads(&self) -> Option<PathBuf>;
    /// Drive roots such as `C:\`.
    fn drives(&self) -> Vec<String>;
    fn run(&self, a: &RunArgs) -> Result<u32, String>;
    fn stop(&self, pid: u32) -> Result<(), String>;
    fn apps(&self) -> Result<Vec<App>, String>;
    /// A PNG, about 64 px square.
    fn icon(&self, key: &str) -> Result<Vec<u8>, String>;
    /// Runs `f` (file I/O for the host) with the desktop user's normal
    /// rights; an error when that is not possible, and `f` does not run.
    fn as_user<R>(&self, f: impl FnOnce() -> R) -> Result<R, String>
    where
        Self: Sized;
}

pub struct Agent<B> {
    backend: B,
    agent: String,
    files: Files,
}

impl<B: Backend> Agent<B> {
    pub fn new(backend: B, agent: impl Into<String>) -> Self {
        Agent {
            backend,
            agent: agent.into(),
            files: Files::default(),
        }
    }

    /// The reply line (newline included) for one request line. Never longer
    /// than `MAX_LINE`.
    pub fn handle_line(&self, line: &str) -> String {
        let resp = match Request::parse(line) {
            Ok(req) => self.execute(req),
            Err((id, why)) => Response::err(id.unwrap_or(0), why),
        };
        fit(resp)
    }

    fn execute(&self, req: Request) -> Response {
        let id = req.id;
        let r: Result<Response, String> = match req.op {
            Op::Ping => Ok(Response::ok(
                id,
                &Pong {
                    proto: VERSION,
                    agent: self.agent.clone(),
                    os: "windows".into(),
                    user: self.backend.user(),
                    session: self.backend.session(),
                    downloads: self
                        .backend
                        .downloads()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                },
            )),
            Op::Run(a) => {
                if a.cmd.trim().is_empty() {
                    Err("empty command".into())
                } else {
                    self.backend
                        .run(&a)
                        .map(|pid| Response::ok(id, &Started { pid }))
                }
            }
            Op::Stop { pid } => self.backend.stop(pid).map(|()| Response::ok(id, &())),
            Op::Apps => self
                .backend
                .apps()
                .map(|apps| Response::ok(id, &Apps { apps })),
            Op::Icon { key } => self.backend.icon(&key).and_then(|png| {
                if png.len() > MAX_ICON {
                    Err("icon too large".into())
                } else {
                    Ok(Response::ok(
                        id,
                        &Icon {
                            format: "png".into(),
                            data: b64_encode(&png),
                        },
                    ))
                }
            }),
            Op::Put(a) => {
                let dl = self.backend.downloads();
                self.backend
                    .as_user(|| self.files.put(&a, dl.as_deref()))
                    .and_then(|r| r)
                    .map(|w| Response::ok(id, &w))
            }
            Op::Get(a) => {
                let dl = self.backend.downloads();
                self.backend
                    .as_user(|| self.files.get(&a, dl.as_deref()))
                    .and_then(|r| r)
                    .map(|c| Response::ok(id, &c))
            }
            Op::Ls { path } => {
                let drives = self.backend.drives();
                self.backend
                    .as_user(|| self.files.ls(&path, &drives))
                    .and_then(|r| r)
                    .map(|l| Response::ok(id, &l))
            }
        };
        r.unwrap_or_else(|e| Response::err(id, e))
    }
}

/// The reply as a line, or an error reply when it would not fit.
fn fit(resp: Response) -> String {
    let line = resp.to_line();
    if line.len() > MAX_LINE {
        return Response::err(resp.id, "reply too large").to_line();
    }
    line
}

// ------------------------------------------------------------------- paths

fn native_for(p: &str, windows: bool) -> String {
    let mut s = if windows {
        p.replace('/', "\\")
    } else {
        p.to_string()
    };
    // "C:" alone means the drive's current directory; the root is meant.
    if s.len() == 2 && s.as_bytes()[0].is_ascii_alphabetic() && s.ends_with(':') {
        s.push('\\');
    }
    s
}

/// The path a request names. A bare file name (no separator, no drive)
/// goes to `bare_in` when given; everything else must be absolute.
pub fn resolve(p: &str, bare_in: Option<&Path>) -> Result<PathBuf, String> {
    if p.is_empty() {
        return Err("empty path".into());
    }
    if p.contains('\0') {
        return Err("bad path".into());
    }
    if !p.contains(['/', '\\', ':']) {
        if p == "." || p == ".." {
            return Err(format!("bad file name: {p}"));
        }
        return match bare_in {
            Some(d) => Ok(d.join(p)),
            None => Err(format!("not an absolute path: {p}")),
        };
    }
    let pb = PathBuf::from(native_for(p, cfg!(windows)));
    if pb.is_absolute() {
        Ok(pb)
    } else {
        Err(format!("not an absolute path: {p}"))
    }
}

/// The temporary file of one transfer: `<target>.<nonce>.conduit-part`.
fn part_name(target: &Path, nonce: &str) -> PathBuf {
    let mut s: OsString = target.as_os_str().to_owned();
    s.push(format!(".{nonce}{PART_SUFFIX}"));
    PathBuf::from(s)
}

/// What every temporary file's name ends with.
pub const PART_SUFFIX: &str = ".conduit-part";

fn hash_file(p: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    let mut h = Sha256::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        if n == 0 {
            return Ok(h.finish_hex());
        }
        h.update(&buf[..n]);
    }
}

// ------------------------------------------------------------------- files

struct PutSession {
    /// This transfer's own temporary file, open since it was created.
    tmp: PathBuf,
    file: std::fs::File,
    written: u64,
    size: Option<u64>,
    force: bool,
    last: Instant,
}

struct GetSession {
    pos: u64,
    hasher: Sha256,
}

/// The file ops and the transfers they have under way.
#[derive(Default)]
pub struct Files {
    puts: Mutex<HashMap<PathBuf, PutSession>>,
    gets: Mutex<HashMap<PathBuf, GetSession>>,
    /// Numbers the temporary files.
    next: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn decode_piece(data: &str) -> Result<Vec<u8>, String> {
    let d = b64_decode(data).ok_or("bad base64 data")?;
    if d.len() > CHUNK {
        return Err(format!(
            "piece of {} bytes is over the {CHUNK} limit",
            d.len()
        ));
    }
    Ok(d)
}

impl Files {
    pub fn put(&self, a: &PutArgs, downloads: Option<&Path>) -> Result<Written, String> {
        let target = resolve(&a.path, downloads)?;
        let session = lock(&self.puts).remove(&target);
        match self.put_piece(a, &target, session) {
            Ok((s, w)) => {
                if let Some(s) = s {
                    let mut puts = lock(&self.puts);
                    if puts.len() >= MAX_SESSIONS {
                        let oldest = puts
                            .iter()
                            .min_by_key(|(_, s)| s.last)
                            .map(|(k, _)| k.clone());
                        if let Some(old) = oldest.and_then(|k| puts.remove(&k)) {
                            drop(old.file);
                            let _ = std::fs::remove_file(old.tmp);
                        }
                    }
                    puts.insert(target, s);
                }
                Ok(w)
            }
            Err((e, ours)) => {
                // Only the temporary file of this transfer goes.
                if let Some(tmp) = ours {
                    let _ = std::fs::remove_file(tmp);
                }
                Err(e)
            }
        }
    }

    /// A new temporary file next to `target`, never an existing name.
    fn create_part(&self, target: &Path) -> Result<(PathBuf, std::fs::File), String> {
        let mut last = None;
        for _ in 0..8 {
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            let tmp = part_name(target, &format!("{}-{n}", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(f) => return Ok((tmp, f)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(format!("cannot write {}: {e}", target.display())),
            }
        }
        Err(format!(
            "cannot write {}: {}",
            target.display(),
            last.map_or_else(String::new, |e| e.to_string())
        ))
    }

    /// One piece. An error carries the temporary file to remove: this
    /// transfer's own, never one it did not create.
    fn put_piece(
        &self,
        a: &PutArgs,
        target: &Path,
        session: Option<PutSession>,
    ) -> Result<(Option<PutSession>, Written), (String, Option<PathBuf>)> {
        let ours = session.as_ref().map(|s| s.tmp.clone());
        let data = decode_piece(&a.data).map_err(|e| (e, ours.clone()))?;
        let s = if a.offset == 0 {
            // A restart: the earlier attempt's file is ours to drop.
            if let Some(old) = session {
                drop(old.file);
                let _ = std::fs::remove_file(&old.tmp);
            }
            self.start_put(a, target).map_err(|e| (e, None))?
        } else {
            let s = session.ok_or_else(|| {
                (
                    format!(
                        "no transfer of {} in progress (offset {})",
                        target.display(),
                        a.offset
                    ),
                    None,
                )
            })?;
            if a.offset != s.written {
                return Err((
                    format!("unexpected offset {}, expected {}", a.offset, s.written),
                    Some(s.tmp),
                ));
            }
            s
        };
        let tmp = s.tmp.clone();
        self.write_piece(a, target, s, &data)
            .map_err(|e| (e, Some(tmp)))
    }

    fn start_put(&self, a: &PutArgs, target: &Path) -> Result<PutSession, String> {
        if a.size.is_some_and(|s| s > MAX_FILE) {
            return Err(format!("file is over the {MAX_FILE} byte limit"));
        }
        match target.parent() {
            Some(p) if p.is_dir() => {}
            Some(p) => return Err(format!("folder does not exist: {}", p.display())),
            None => return Err(format!("bad path: {}", target.display())),
        }
        if target.is_dir() {
            return Err(format!("{} is a folder", target.display()));
        }
        if target.exists() && !a.force {
            return Err(format!("already exists: {}", target.display()));
        }
        let (tmp, file) = self.create_part(target)?;
        Ok(PutSession {
            tmp,
            file,
            written: 0,
            size: a.size,
            force: a.force,
            last: Instant::now(),
        })
    }

    fn write_piece(
        &self,
        a: &PutArgs,
        target: &Path,
        mut s: PutSession,
        data: &[u8],
    ) -> Result<(Option<PutSession>, Written), String> {
        if s.written + data.len() as u64 > MAX_FILE {
            return Err(format!("file is over the {MAX_FILE} byte limit"));
        }
        if !data.is_empty() {
            s.file
                .write_all(data)
                .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        }
        s.written += data.len() as u64;
        s.last = Instant::now();
        if !a.done {
            let w = Written {
                size: s.written,
                ..Written::default()
            };
            return Ok((Some(s), w));
        }
        // The last piece: check, then move into place.
        if let Some(want) = a.size.or(s.size) {
            if s.written != want {
                return Err(format!(
                    "size mismatch: received {} bytes, expected {want}",
                    s.written
                ));
            }
        }
        s.file
            .flush()
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        let PutSession {
            tmp,
            file,
            written,
            force,
            ..
        } = s;
        drop(file);
        let tmp = tmp.as_path();
        let on_disk = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
        if on_disk != written {
            return Err(format!(
                "temporary file has {on_disk} bytes, expected {written}"
            ));
        }
        let sum = hash_file(tmp)?;
        if let Some(want) = &a.sha256 {
            if !want.trim().eq_ignore_ascii_case(&sum) {
                return Err(format!("sha256 mismatch: got {sum}, expected {want}"));
            }
        }
        if target.exists() && !(a.force || force) {
            return Err(format!("already exists: {}", target.display()));
        }
        std::fs::rename(tmp, target)
            .map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
        Ok((
            None,
            Written {
                size: written,
                path: target.to_string_lossy().into_owned(),
                sha256: sum,
            },
        ))
    }

    pub fn get(&self, a: &GetArgs, downloads: Option<&Path>) -> Result<Chunk, String> {
        let path = resolve(&a.path, downloads)?;
        let md =
            std::fs::metadata(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if !md.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let size = md.len();
        if size > MAX_FILE {
            return Err(format!("file is over the {MAX_FILE} byte limit"));
        }
        if a.offset > size {
            return Err(format!("offset {} is past the end ({size})", a.offset));
        }
        let want = if a.len == 0 { CHUNK } else { a.len.min(CHUNK) };
        let mut f = std::fs::File::open(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        f.seek(SeekFrom::Start(a.offset))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut data = Vec::with_capacity(want);
        f.take(want as u64)
            .read_to_end(&mut data)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let end = a.offset + data.len() as u64;
        let eof = end >= size;

        // Hash as the pieces go by; a transfer that is not sequential is
        // hashed in one go at the end.
        let mut s = lock(&self.gets).remove(&path);
        if a.offset == 0 {
            s = Some(GetSession {
                pos: 0,
                hasher: Sha256::default(),
            });
        }
        let s = s.filter(|s| s.pos == a.offset).map(|mut s| {
            s.hasher.update(&data);
            s.pos = end;
            s
        });
        let sha256 = if eof {
            match s {
                Some(s) if s.pos == size => Some(s.hasher.finish_hex()),
                _ => Some(hash_file(&path)?),
            }
        } else {
            if let Some(s) = s {
                let mut gets = lock(&self.gets);
                if gets.len() >= 16 {
                    gets.clear();
                }
                gets.insert(path, s);
            }
            None
        };
        Ok(Chunk {
            data: b64_encode(&data),
            size,
            eof,
            sha256,
        })
    }

    pub fn ls(&self, path: &str, drives: &[String]) -> Result<Listing, String> {
        if path.is_empty() {
            return Ok(Listing {
                path: String::new(),
                entries: drives
                    .iter()
                    .map(|d| Entry {
                        name: d.clone(),
                        dir: true,
                        ..Entry::default()
                    })
                    .collect(),
            });
        }
        let dir = resolve(path, None)?;
        let rd =
            std::fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let mut entries = Vec::new();
        for e in rd.flatten() {
            let md = match e.file_type() {
                Ok(t) if t.is_symlink() => std::fs::metadata(e.path()).or_else(|_| e.metadata()),
                _ => e.metadata(),
            };
            let (dir, size, mtime) = match md {
                Ok(m) => (
                    m.is_dir(),
                    if m.is_dir() { 0 } else { m.len() },
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs()),
                ),
                Err(_) => (false, 0, 0),
            };
            entries.push(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                dir,
                size,
                mtime,
            });
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        entries.sort_by_key(|e| (!e.dir, e.name.to_lowercase()));
        Ok(Listing {
            path: dir.to_string_lossy().into_owned(),
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use conduit_ctl::sha256_hex;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct Fake {
        downloads: Mutex<Option<PathBuf>>,
        stopped: AtomicU32,
        big_apps: bool,
        /// File ops run through `as_user`, and how many.
        as_user: AtomicU32,
        refuse_user: bool,
    }

    impl Backend for Fake {
        fn user(&self) -> String {
            "tester".into()
        }
        fn session(&self) -> bool {
            true
        }
        fn downloads(&self) -> Option<PathBuf> {
            lock(&self.downloads).clone()
        }
        fn drives(&self) -> Vec<String> {
            vec!["C:\\".into(), "D:\\".into()]
        }
        fn run(&self, a: &RunArgs) -> Result<u32, String> {
            if a.cmd == "missing" {
                Err("not found: missing".into())
            } else {
                Ok(4242)
            }
        }
        fn stop(&self, pid: u32) -> Result<(), String> {
            if pid == 1 {
                self.stopped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            } else {
                Err(format!("pid {pid} was not started by conduit"))
            }
        }
        fn apps(&self) -> Result<Vec<App>, String> {
            let n = if self.big_apps { 60_000 } else { 2 };
            Ok((0..n)
                .map(|i| App {
                    name: format!("App number {i} with a long enough name to matter"),
                    target: format!("C:\\Users\\x\\Start Menu\\Programs\\App number {i}.lnk"),
                    icon: "k".repeat(40),
                    source: "startmenu".into(),
                    args: vec![],
                })
                .collect())
        }
        fn icon(&self, key: &str) -> Result<Vec<u8>, String> {
            match key {
                "big" => Ok(vec![0; MAX_ICON + 1]),
                "ok" => Ok(vec![1, 2, 3]),
                _ => Err("no icon".into()),
            }
        }
        fn as_user<R>(&self, f: impl FnOnce() -> R) -> Result<R, String> {
            if self.refuse_user {
                return Err("no desktop user".into());
            }
            self.as_user.fetch_add(1, Ordering::Relaxed);
            Ok(f())
        }
    }

    /// The temporary files in `dir`.
    fn parts(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(PART_SUFFIX))
            .collect();
        v.sort();
        v
    }

    fn agent(dl: Option<&Path>) -> Agent<Fake> {
        let f = Fake::default();
        *lock(&f.downloads) = dl.map(Path::to_path_buf);
        Agent::new(f, "conduit-tray test")
    }

    fn call(a: &Agent<Fake>, op: Op) -> Response {
        let line = a.handle_line(&Request::new(7, op).to_line());
        assert!(line.ends_with('\n') && line.len() <= MAX_LINE);
        let r = Response::parse(&line).unwrap();
        assert_eq!(r.id, 7);
        r
    }

    fn put(path: &Path, offset: u64, data: &[u8]) -> PutArgs {
        PutArgs {
            path: path.to_string_lossy().into_owned(),
            offset,
            data: b64_encode(data),
            ..PutArgs::default()
        }
    }

    fn err_of(r: Result<Written, String>) -> String {
        r.expect_err("should fail")
    }

    #[test]
    fn simple_ops() {
        let a = agent(Some(Path::new("/dl")));
        let p: Pong = call(&a, Op::Ping).into_body().unwrap();
        assert_eq!(p.agent, "conduit-tray test");
        assert_eq!(
            (p.os.as_str(), p.user.as_str(), p.session),
            ("windows", "tester", true)
        );
        assert_eq!((p.proto, p.downloads.as_str()), (VERSION, "/dl"));

        let s: Started = call(
            &a,
            Op::Run(RunArgs {
                cmd: "x.exe".into(),
                ..RunArgs::default()
            }),
        )
        .into_body()
        .unwrap();
        assert_eq!(s.pid, 4242);
        let r = call(
            &a,
            Op::Run(RunArgs {
                cmd: "missing".into(),
                ..RunArgs::default()
            }),
        );
        assert_eq!(r.error.as_deref(), Some("not found: missing"));
        let r = call(
            &a,
            Op::Run(RunArgs {
                cmd: "  ".into(),
                ..RunArgs::default()
            }),
        );
        assert!(!r.ok);

        assert!(call(&a, Op::Stop { pid: 1 }).ok);
        assert!(!call(&a, Op::Stop { pid: 2 }).ok);
        let apps: Apps = call(&a, Op::Apps).into_body().unwrap();
        assert_eq!(apps.apps.len(), 2);
        let i: Icon = call(&a, Op::Icon { key: "ok".into() }).into_body().unwrap();
        assert_eq!(
            (i.format.as_str(), b64_decode(&i.data)),
            ("png", Some(vec![1, 2, 3]))
        );
        assert!(!call(&a, Op::Icon { key: "big".into() }).ok);
        assert!(!call(&a, Op::Icon { key: "none".into() }).ok);
    }

    #[test]
    fn bad_lines_get_error_replies_with_the_id() {
        let a = agent(None);
        let r = Response::parse(&a.handle_line(r#"{"v":1,"id":9,"op":"frobnicate"}"#)).unwrap();
        assert_eq!((r.id, r.ok), (9, false));
        assert!(r.error.unwrap().contains("bad request"));
        let r = Response::parse(&a.handle_line(r#"{"v":1,"id":9,"op":"stop","pid":"x"}"#)).unwrap();
        assert_eq!((r.id, r.ok), (9, false));
        let r = Response::parse(&a.handle_line("not json")).unwrap();
        assert!(!r.ok);
        let r = Response::parse(&a.handle_line(r#"{"v":7,"id":3,"op":"ping"}"#)).unwrap();
        assert_eq!(r.id, 3);
    }

    #[test]
    fn oversized_replies_become_errors() {
        let f = Fake {
            big_apps: true,
            ..Fake::default()
        };
        let a = Agent::new(f, "t");
        let r = call(&a, Op::Apps);
        assert_eq!(r.error.as_deref(), Some("reply too large"));
    }

    #[test]
    fn path_resolution() {
        let dl = Path::new("/dl");
        assert_eq!(
            resolve("a b.txt", Some(dl)).unwrap(),
            Path::new("/dl/a b.txt")
        );
        assert!(resolve("a.txt", None).is_err());
        assert!(resolve("", Some(dl)).is_err());
        assert!(resolve("..", Some(dl)).is_err());
        assert!(resolve("sub/a.txt", Some(dl)).is_err());
        assert!(resolve("a:b", Some(dl)).is_err());
        assert!(resolve("/x\0y", Some(dl)).is_err());
        assert_eq!(
            resolve("/tmp/\u{e6} \u{3042}.txt", None).unwrap(),
            Path::new("/tmp/\u{e6} \u{3042}.txt")
        );
        assert_eq!(native_for("C:/a/b", true), "C:\\a\\b");
        assert_eq!(native_for("C:", true), "C:\\");
        assert_eq!(native_for("c:", false), "c:\\");
        assert_eq!(native_for("D:\\x/y", false), "D:\\x/y");
    }

    #[test]
    fn put_streams_checks_and_renames() {
        let t = TempDir::new("put");
        let f = Files::default();
        let target = t.path().join("Mine Filer \u{c6}\u{3042}.bin");
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 253) as u8).collect();
        let sum = sha256_hex(&data);

        let w = f.put(&put(&target, 0, &data[..400]), None).unwrap();
        assert_eq!(w.size, 400);
        assert!(w.path.is_empty() && w.sha256.is_empty());
        assert_eq!(parts(t.path()).len(), 1);
        assert!(!target.exists());
        f.put(&put(&target, 400, &data[400..900]), None).unwrap();
        let mut last = put(&target, 900, &data[900..]);
        last.done = true;
        last.size = Some(1000);
        last.sha256 = Some(sum.to_uppercase());
        let w = f.put(&last, None).unwrap();
        assert_eq!((w.size, w.sha256.as_str()), (1000, sum.as_str()));
        assert_eq!(w.path, target.to_string_lossy());
        assert_eq!(std::fs::read(&target).unwrap(), data);
        assert!(parts(t.path()).is_empty());
        assert!(lock(&f.puts).is_empty());
    }

    #[test]
    fn put_empty_file_in_one_piece() {
        let t = TempDir::new("put0");
        let f = Files::default();
        let target = t.path().join("empty");
        let mut a = put(&target, 0, b"");
        a.done = true;
        a.size = Some(0);
        a.sha256 = Some(sha256_hex(b""));
        f.put(&a, None).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"");
    }

    #[test]
    fn put_failure_paths_clean_up() {
        let t = TempDir::new("putfail");
        let f = Files::default();
        let target = t.path().join("f.bin");
        let tmp_gone = || parts(t.path()).is_empty();

        // Wrong offset (a lost chunk): refused, temp file removed.
        f.put(&put(&target, 0, b"aaaa"), None).unwrap();
        let e = err_of(f.put(&put(&target, 8, b"bbbb"), None));
        assert_eq!(e, "unexpected offset 8, expected 4");
        assert!(tmp_gone() && !target.exists());
        // ...and the transfer is gone: continuing is refused too.
        assert!(err_of(f.put(&put(&target, 4, b"bbbb"), None)).contains("no transfer"));

        // Bad sha.
        f.put(&put(&target, 0, b"aaaa"), None).unwrap();
        let mut a = put(&target, 4, b"bbbb");
        a.done = true;
        a.sha256 = Some("00".repeat(32));
        assert!(err_of(f.put(&a, None)).contains("sha256 mismatch"));
        assert!(tmp_gone() && !target.exists());

        // Size mismatch (size sent with the first piece).
        let mut a = put(&target, 0, b"aaaa");
        a.size = Some(10);
        f.put(&a, None).unwrap();
        let mut a = put(&target, 4, b"");
        a.done = true;
        assert!(err_of(f.put(&a, None)).contains("size mismatch"));
        assert!(tmp_gone() && !target.exists());

        // Bad base64 and oversized pieces.
        let mut a = put(&target, 0, b"");
        a.data = "!!!".into();
        assert!(err_of(f.put(&a, None)).contains("base64"));
        let a = put(&target, 0, &vec![0u8; CHUNK + 1]);
        assert!(err_of(f.put(&a, None)).contains("limit"));
        let mut a = put(&target, 0, b"x");
        a.size = Some(MAX_FILE + 1);
        assert!(err_of(f.put(&a, None)).contains("limit"));
        assert!(tmp_gone());
    }

    #[test]
    fn put_refuses_existing_unless_forced_and_missing_parents() {
        let t = TempDir::new("putforce");
        let f = Files::default();
        let target = t.path().join("exists.txt");
        std::fs::write(&target, b"old").unwrap();
        let e = err_of(f.put(&put(&target, 0, b"new"), None));
        assert!(e.contains("already exists"), "{e}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(parts(t.path()).is_empty());

        let mut a = put(&target, 0, b"new");
        a.force = true;
        a.done = true;
        f.put(&a, None).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        let e = err_of(f.put(&put(&t.path().join("nodir").join("x"), 0, b"x"), None));
        assert!(e.contains("folder does not exist"), "{e}");
        assert!(!t.path().join("nodir").exists());
        let e = err_of(f.put(&put(t.path(), 0, b"x"), None));
        assert!(e.contains("is a folder"), "{e}");
        assert!(err_of(f.put(&put(Path::new("rel/x"), 0, b"x"), None)).contains("absolute"));
    }

    #[test]
    fn abandoned_transfer_is_restarted_by_the_next_offset_zero() {
        let t = TempDir::new("abandon");
        let f = Files::default();
        let target = t.path().join("f.bin");
        f.put(&put(&target, 0, b"stale stale stale"), None).unwrap();
        // A "new process" would have no session; the stale temp file stays.
        let g = Files::default();
        assert!(err_of(g.put(&put(&target, 17, b"x"), None)).contains("no transfer"));
        let mut a = put(&target, 0, b"fresh");
        a.done = true;
        g.put(&a, None).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"fresh");
        // The earlier transfer's file is not this one's to delete.
        assert_eq!(parts(t.path()).len(), 1);
    }

    #[test]
    fn put_never_writes_into_or_deletes_a_planted_temp_file() {
        let t = TempDir::new("plant");
        let f = Files::default();
        let target = t.path().join("f.bin");
        // Whatever sits at the name the next transfer would pick (a file,
        // or on Windows a link a lower-privileged process made) is left alone.
        let planted = part_name(&target, &format!("{}-0", std::process::id()));
        std::fs::write(&planted, b"planted").unwrap();
        let mut a = put(&target, 0, b"data");
        a.done = true;
        f.put(&a, None).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"data");
        assert_eq!(std::fs::read(&planted).unwrap(), b"planted");

        // A refused first piece creates nothing, so it deletes nothing.
        let e = err_of(f.put(&put(&target, 0, b"x"), None));
        assert!(e.contains("already exists"), "{e}");
        assert_eq!(std::fs::read(&planted).unwrap(), b"planted");
        // Neither does a stray continuation.
        assert!(err_of(f.put(&put(&target, 9, b"x"), None)).contains("no transfer"));
        assert_eq!(parts(t.path()), vec![planted]);
    }

    #[test]
    fn file_ops_run_as_the_user() {
        let t = TempDir::new("asuser");
        std::fs::write(t.path().join("a.txt"), b"hi").unwrap();
        let a = agent(Some(t.path()));
        let get = Op::Get(GetArgs {
            path: "a.txt".into(),
            offset: 0,
            len: 10,
        });
        assert!(call(&a, get.clone()).ok);
        assert!(
            call(
                &a,
                Op::Ls {
                    path: t.path().to_string_lossy().into_owned()
                }
            )
            .ok
        );
        let p = PutArgs {
            path: "b.txt".into(),
            data: b64_encode(b"x"),
            done: true,
            ..PutArgs::default()
        };
        assert!(call(&a, Op::Put(p.clone())).ok);
        assert_eq!(a.backend.as_user.load(Ordering::Relaxed), 3);
        assert!(call(&a, Op::Ping).ok);
        assert_eq!(a.backend.as_user.load(Ordering::Relaxed), 3);

        // Without the user's token nothing is read, listed or written.
        let f = Fake {
            refuse_user: true,
            ..Fake::default()
        };
        *lock(&f.downloads) = Some(t.path().to_path_buf());
        let a = Agent::new(f, "t");
        let r = call(&a, get);
        assert_eq!(r.error.as_deref(), Some("no desktop user"));
        assert!(
            !call(
                &a,
                Op::Ls {
                    path: t.path().to_string_lossy().into_owned()
                }
            )
            .ok
        );
        let p = PutArgs {
            path: "c.txt".into(),
            ..p
        };
        assert!(!call(&a, Op::Put(p)).ok);
        assert!(!t.path().join("c.txt").exists());
    }

    #[test]
    fn bare_names_land_in_downloads() {
        let t = TempDir::new("dl");
        let a = agent(Some(t.path()));
        let mut p = PutArgs {
            path: "note \u{e6}.txt".into(),
            data: b64_encode(b"hi"),
            done: true,
            ..PutArgs::default()
        };
        let r = call(&a, Op::Put(p.clone()));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            std::fs::read(t.path().join("note \u{e6}.txt")).unwrap(),
            b"hi"
        );
        // A second one is refused without force.
        p.offset = 0;
        assert!(!call(&a, Op::Put(p)).ok);
        let c: Chunk = call(
            &a,
            Op::Get(GetArgs {
                path: "note \u{e6}.txt".into(),
                offset: 0,
                len: 100,
            }),
        )
        .into_body()
        .unwrap();
        assert_eq!(b64_decode(&c.data).unwrap(), b"hi");
        assert!(c.eof);
    }

    #[test]
    fn get_pieces_with_hash_and_edge_cases() {
        let t = TempDir::new("get");
        let f = Files::default();
        let path = t.path().join("a b").join("data \u{3042}.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data: Vec<u8> = (0..(CHUNK * 2 + 100) as u32)
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();
        let ga = |offset: u64, len: usize| GetArgs {
            path: path.to_string_lossy().into_owned(),
            offset,
            len,
        };
        let mut got = Vec::new();
        let mut off = 0u64;
        loop {
            let c = f.get(&ga(off, CHUNK * 4), None).unwrap(); // clamped to CHUNK
            let d = b64_decode(&c.data).unwrap();
            assert!(d.len() <= CHUNK);
            assert_eq!(c.size, data.len() as u64);
            off += d.len() as u64;
            got.extend(d);
            if c.eof {
                assert_eq!(c.sha256.as_deref(), Some(sha256_hex(&data).as_str()));
                break;
            }
            assert!(c.sha256.is_none());
        }
        assert_eq!(got, data);
        assert!(lock(&f.gets).is_empty());

        // Skipping around still yields the right whole-file hash at the end.
        let c = f.get(&ga(5, 10), None).unwrap();
        assert!(!c.eof);
        let c = f.get(&ga(data.len() as u64 - 3, 10), None).unwrap();
        assert!(c.eof);
        assert_eq!(c.sha256.as_deref(), Some(sha256_hex(&data).as_str()));
        // Exactly at the end: empty piece, eof.
        let c = f.get(&ga(data.len() as u64, 10), None).unwrap();
        assert!(c.eof && c.data.is_empty() && c.sha256.is_some());
        // Past the end, a directory, a missing file.
        assert!(f
            .get(&ga(data.len() as u64 + 1, 10), None)
            .unwrap_err()
            .contains("past the end"));
        let dir = GetArgs {
            path: t.path().to_string_lossy().into_owned(),
            ..GetArgs::default()
        };
        assert!(f.get(&dir, None).unwrap_err().contains("not a file"));
        let miss = GetArgs {
            path: t.path().join("nope").to_string_lossy().into_owned(),
            ..GetArgs::default()
        };
        assert!(f.get(&miss, None).unwrap_err().contains("cannot read"));
        // len 0 means a full piece; an empty file hashes at offset 0.
        let e = t.path().join("e");
        std::fs::write(&e, b"").unwrap();
        let c = f
            .get(
                &GetArgs {
                    path: e.to_string_lossy().into_owned(),
                    offset: 0,
                    len: 0,
                },
                None,
            )
            .unwrap();
        assert!(c.eof);
        assert_eq!(c.sha256.as_deref(), Some(sha256_hex(b"").as_str()));
    }

    #[test]
    fn ls_lists_drives_and_directories() {
        let t = TempDir::new("ls");
        let f = Files::default();
        let drives = vec!["C:\\".to_string()];
        let l = f.ls("", &drives).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert!(l.entries[0].dir && l.entries[0].name == "C:\\");

        std::fs::create_dir(t.path().join("Zdir")).unwrap();
        std::fs::create_dir(t.path().join("adir \u{e6}")).unwrap();
        std::fs::write(t.path().join("b file.txt"), b"12345").unwrap();
        std::fs::write(t.path().join("A.txt"), b"1").unwrap();
        let l = f.ls(&t.path().to_string_lossy(), &drives).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["adir \u{e6}", "Zdir", "A.txt", "b file.txt"]);
        assert!(l.entries[0].dir && !l.entries[2].dir);
        assert_eq!(l.entries[3].size, 5);
        assert!(l.entries[3].mtime > 1_600_000_000);
        assert_eq!(l.path, t.path().to_string_lossy());
        assert!(f.ls("rel", &drives).is_err());
        assert!(f
            .ls(&t.path().join("nope").to_string_lossy(), &drives)
            .is_err());
    }

    #[test]
    fn ls_caps_entries() {
        let t = TempDir::new("lscap");
        for i in 0..(MAX_ENTRIES + 20) {
            std::fs::write(t.path().join(format!("f{i}")), b"").unwrap();
        }
        let l = Files::default()
            .ls(&t.path().to_string_lossy(), &[])
            .unwrap();
        assert_eq!(l.entries.len(), MAX_ENTRIES);
    }
}

use super::*;
use conduit_ctl::{sha256_hex, Entry};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};

/// What goes wrong in the fake guest.
#[derive(Clone, Default)]
struct Faults {
    /// Never answer.
    silent: bool,
    /// Close the channel after this many requests.
    close_after: Option<usize>,
    /// Answer every `put` with a wrong checksum.
    bad_sum: bool,
    /// Send garbage and a stale response before the real one.
    noisy: bool,
    /// Cut `get` pieces short without saying so (a lost piece).
    short_get: bool,
    /// Report a different file size after the first `get` piece.
    grow: bool,
    /// Announce a 10-byte file in `get` but send the whole (bigger) one.
    overflow: bool,
    /// Answer with control characters, escapes and bidi overrides in text.
    hostile: bool,
}

#[derive(Default)]
struct Guest {
    files: HashMap<String, Vec<u8>>,
    temp: HashMap<String, Vec<u8>>,
    ran: Vec<RunArgs>,
}

fn apps() -> Vec<App> {
    let app = |n: &str, t: &str, s: &str| App {
        name: n.into(),
        target: t.into(),
        source: s.into(),
        icon: format!("icon:{n}"),
        ..App::default()
    };
    vec![
        app("Notepad", "C:\\ProgramData\\Notepad.lnk", "startmenu"),
        app("Notepad++", "C:\\ProgramData\\Notepad++.lnk", "startmenu"),
        app("Counter-Strike 2", "steam://rungameid/730", "steam"),
        app(
            "Counter-Strike: GO Server",
            "steam://rungameid/740",
            "steam",
        ),
    ]
}

/// A guest that follows the protocol, like the real agents.
fn serve(s: UnixStream, g: Arc<Mutex<Guest>>, f: Faults) {
    let mut rd = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    let mut n = 0;
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        n += 1;
        if f.close_after.is_some_and(|c| n > c) {
            return;
        }
        if f.silent {
            continue;
        }
        let resp = match Request::parse(&line) {
            Err((id, e)) => Response::err(id.unwrap_or(0), e),
            Ok(r) => handle(&mut g.lock().unwrap(), r, &f),
        };
        if f.noisy {
            let _ = w.write_all(b"not json at all\n{\"id\":9999,\"ok\":true}\n");
        }
        let _ = w.write_all(resp.to_line().as_bytes());
    }
}

fn handle(g: &mut Guest, r: Request, f: &Faults) -> Response {
    let id = r.id;
    match r.op {
        Op::Ping => Response::ok(
            id,
            &Pong {
                proto: 1,
                agent: "fake".into(),
                os: "windows".into(),
                user: "u".into(),
                session: true,
                downloads: "C:\\Users\\u\\Downloads".into(),
            },
        ),
        Op::Run(a) => {
            if f.hostile {
                return Response::err(id, "no\x1b]0;pwned\x07 such\n\u{202e}file");
            }
            if a.cmd.contains("missing") {
                return Response::err(id, format!("not found: {}", a.cmd));
            }
            g.ran.push(a);
            Response::ok(id, &Started { pid: 4242 })
        }
        Op::Stop { .. } => Response::ok(id, &serde_json::json!({})),
        Op::Apps if f.hostile => Response::ok(
            id,
            &Apps {
                apps: vec![App {
                    name: format!("Evil\x1b[2J\r\n\u{2028}\u{202e}gpj.exe{}", "x".repeat(5000)),
                    target: "C:\\a\x07.lnk".into(),
                    args: vec!["--x\x1b[31m".into(); 1000],
                    icon: "i\u{0085}".into(),
                    source: "startmenu\x1b".into(),
                }],
            },
        ),
        Op::Apps => Response::ok(id, &Apps { apps: apps() }),
        Op::Icon { key } => Response::ok(
            id,
            &Icon {
                format: "png".into(),
                data: b64_encode(key.as_bytes()),
            },
        ),
        Op::Ls { path } => {
            if path == "C:\\Temp" {
                Response::ok(
                    id,
                    &Listing {
                        path,
                        entries: vec![Entry {
                            name: "x".into(),
                            ..Entry::default()
                        }],
                    },
                )
            } else {
                Response::err(id, "no such directory")
            }
        }
        Op::Put(p) => {
            let path = if p.path.contains(['/', '\\']) {
                p.path.clone()
            } else {
                format!("C:\\Users\\u\\Downloads\\{}", p.path)
            };
            if p.offset == 0 {
                if g.files.contains_key(&path) && !p.force {
                    return Response::err(id, format!("{path} exists"));
                }
                g.temp.insert(path.clone(), Vec::new());
            }
            let Some(t) = g.temp.get_mut(&path) else {
                return Response::err(id, "no transfer in progress");
            };
            if p.offset != t.len() as u64 {
                return Response::err(
                    id,
                    format!("unexpected offset {}, expected {}", p.offset, t.len()),
                );
            }
            t.extend(b64_decode(&p.data).unwrap());
            let mut w = Written {
                size: t.len() as u64,
                ..Written::default()
            };
            if p.done {
                let t = g.temp.remove(&path).unwrap();
                if p.size.is_some_and(|s| s != t.len() as u64) {
                    return Response::err(id, "size mismatch");
                }
                w.sha256 = sha256_hex(&t);
                if Some(&w.sha256) != p.sha256.as_ref() {
                    return Response::err(id, "checksum mismatch");
                }
                if f.bad_sum {
                    w.sha256 = "0".repeat(64);
                }
                w.path = path.clone();
                g.files.insert(path, t);
            }
            Response::ok(id, &w)
        }
        Op::Get(a) => {
            let Some(d) = g.files.get(&a.path) else {
                return Response::err(id, format!("not found: {}", a.path));
            };
            let from = (a.offset as usize).min(d.len());
            let mut to = (from + a.len.min(CHUNK)).min(d.len());
            if f.short_get && to > from + 1 {
                to = from + 1;
            }
            let eof = to == d.len();
            let size = if f.overflow {
                10
            } else if f.grow && a.offset > 0 {
                d.len() as u64 + 1
            } else {
                d.len() as u64
            };
            Response::ok(
                id,
                &Chunk {
                    data: b64_encode(&d[from..to]),
                    size,
                    eof,
                    sha256: eof.then(|| sha256_hex(d)),
                },
            )
        }
    }
}

fn pair(f: Faults) -> (Client, Arc<Mutex<Guest>>) {
    let (a, b) = UnixStream::pair().unwrap();
    let g = Arc::new(Mutex::new(Guest::default()));
    let g2 = g.clone();
    std::thread::spawn(move || serve(b, g2, f));
    (
        Client::from_stream(a).with_cap(Duration::from_millis(600)),
        g,
    )
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("conduit-ctl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn no_progress() -> impl FnMut(u64, u64) {
    |_, _| {}
}

#[test]
fn ping_and_apps_and_run_end_to_end() {
    let (mut c, g) = pair(Faults::default());
    let p = c.ping().unwrap();
    assert_eq!((p.os.as_str(), p.session), ("windows", true));
    let list = c.apps().unwrap();
    assert_eq!(list.len(), 4);
    // `run --app steam` style resolution, then run.
    let a = pick_app(&list, "counter-strike 2").unwrap();
    let s = c
        .run(RunArgs {
            cmd: a.target.clone(),
            ..RunArgs::default()
        })
        .unwrap();
    assert_eq!(s.pid, 4242);
    assert_eq!(g.lock().unwrap().ran[0].cmd, "steam://rungameid/730");
    // A direct command with arguments, cwd and env.
    c.run(RunArgs {
        cmd: "notepad.exe".into(),
        args: vec!["a b.txt".into()],
        cwd: "C:\\Temp".into(),
        env: [("A".to_string(), "1".to_string())].into(),
    })
    .unwrap();
    let ran = &g.lock().unwrap().ran[1];
    assert_eq!(ran.args, ["a b.txt"]);
    assert_eq!((ran.cwd.as_str(), ran.env["A"].as_str()), ("C:\\Temp", "1"));
}

#[test]
fn run_of_a_missing_target_is_the_guests_readable_error() {
    let (mut c, _) = pair(Faults::default());
    let e = c
        .run(RunArgs {
            cmd: "missing.exe".into(),
            ..RunArgs::default()
        })
        .unwrap_err();
    assert_eq!(e, CtlError::Guest("not found: missing.exe".into()));
    // The channel is still good.
    assert!(c.ping().is_ok());
}

#[test]
fn cp_round_trip_with_odd_names() {
    let d = tmpdir("rt");
    let (mut c, g) = pair(Faults::default());
    let data: Vec<u8> = (0..=255u8).cycle().take(CHUNK * 2 + 123).collect();
    let src = d.join("Mine Filer ærø 日本.bin");
    std::fs::write(&src, &data).unwrap();
    let mut seen = Vec::new();
    let w = c
        .put_file(&src, "C:\\Users\\Ære Ø\\a b.bin", false, &mut |n, t| {
            seen.push((n, t))
        })
        .unwrap();
    assert_eq!(w.path, "C:\\Users\\Ære Ø\\a b.bin");
    assert_eq!(w.size, data.len() as u64);
    assert_eq!(seen.len(), 3);
    assert_eq!(seen.last(), Some(&(data.len() as u64, data.len() as u64)));
    assert_eq!(g.lock().unwrap().files[&w.path], data);

    let dst = d.join("back ✓.bin");
    let n = c
        .get_file(&w.path, &dst, false, &mut no_progress())
        .unwrap();
    assert_eq!(n, data.len() as u64);
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    assert!(!d.join("back ✓.bin.conduit-part").exists());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn cp_of_an_empty_file_and_a_bare_name() {
    let d = tmpdir("empty");
    let (mut c, g) = pair(Faults::default());
    let src = d.join("e.txt");
    std::fs::write(&src, b"").unwrap();
    let w = c
        .put_file(&src, "e.txt", false, &mut no_progress())
        .unwrap();
    assert_eq!(w.path, "C:\\Users\\u\\Downloads\\e.txt");
    assert!(g.lock().unwrap().files[&w.path].is_empty());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn put_refuses_to_overwrite_without_force() {
    let d = tmpdir("force");
    let (mut c, g) = pair(Faults::default());
    let src = d.join("f");
    std::fs::write(&src, b"one").unwrap();
    c.put_file(&src, "C:\\f", false, &mut no_progress())
        .unwrap();
    let e = c
        .put_file(&src, "C:\\f", false, &mut no_progress())
        .unwrap_err();
    assert!(matches!(e, CtlError::Guest(m) if m.contains("exists")));
    std::fs::write(&src, b"two").unwrap();
    c.put_file(&src, "C:\\f", true, &mut no_progress()).unwrap();
    assert_eq!(g.lock().unwrap().files["C:\\f"], b"two");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn get_refuses_to_overwrite_without_force_and_keeps_the_file() {
    let d = tmpdir("getforce");
    let (mut c, g) = pair(Faults::default());
    g.lock()
        .unwrap()
        .files
        .insert("C:\\f".into(), b"guest".to_vec());
    let dst = d.join("f");
    std::fs::write(&dst, b"mine").unwrap();
    let e = c
        .get_file("C:\\f", &dst, false, &mut no_progress())
        .unwrap_err();
    assert!(
        matches!(&e, CtlError::Failed(m) if m.contains("--force")),
        "{e}"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"mine");
    c.get_file("C:\\f", &dst, true, &mut no_progress()).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"guest");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn get_of_a_missing_file_leaves_nothing_behind() {
    let d = tmpdir("getmissing");
    let (mut c, _) = pair(Faults::default());
    let dst = d.join("x");
    let e = c
        .get_file("C:\\nope", &dst, false, &mut no_progress())
        .unwrap_err();
    assert!(matches!(e, CtlError::Guest(m) if m.contains("not found")));
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_guest_checksum_that_differs_fails_the_put() {
    let d = tmpdir("badsum");
    let (mut c, _) = pair(Faults {
        bad_sum: true,
        ..Faults::default()
    });
    let src = d.join("f");
    std::fs::write(&src, b"hello").unwrap();
    let e = c
        .put_file(&src, "C:\\f", false, &mut no_progress())
        .unwrap_err();
    assert!(matches!(e, CtlError::Failed(m) if m.contains("checksum")));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_lost_get_piece_is_caught_by_size_and_checksum() {
    let d = tmpdir("short");
    let (mut c, g) = pair(Faults {
        short_get: true,
        ..Faults::default()
    });
    g.lock()
        .unwrap()
        .files
        .insert("C:\\f".into(), b"abcdef".to_vec());
    // The fake sends one byte per piece but keeps `offset` honest: the copy
    // still arrives whole.
    let dst = d.join("f");
    c.get_file("C:\\f", &dst, false, &mut no_progress())
        .unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"abcdef");
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn the_guest_going_away_mid_transfer_is_an_error_and_cleans_up() {
    let d = tmpdir("gone");
    let (mut c, _) = pair(Faults {
        close_after: Some(1),
        ..Faults::default()
    });
    let src = d.join("big");
    std::fs::write(&src, vec![7u8; CHUNK * 3]).unwrap();
    let e = c
        .put_file(&src, "C:\\big", false, &mut no_progress())
        .unwrap_err();
    assert_eq!(e, CtlError::Closed);

    let (mut c, g) = pair(Faults {
        close_after: Some(1),
        ..Faults::default()
    });
    g.lock()
        .unwrap()
        .files
        .insert("C:\\big".into(), vec![1u8; CHUNK * 3]);
    let dst = d.join("got");
    let e = c
        .get_file("C:\\big", &dst, false, &mut no_progress())
        .unwrap_err();
    assert_eq!(e, CtlError::Closed);
    assert!(!dst.exists());
    assert!(!d.join("got.conduit-part").exists());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_silent_guest_times_out_with_a_clear_error() {
    let (mut c, _) = pair(Faults {
        silent: true,
        ..Faults::default()
    });
    let t = Instant::now();
    let e = c.ping().unwrap_err();
    assert!(matches!(e, CtlError::Timeout(_)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(3));
    let msg = format!("{:#}", friendly("win11", e));
    assert!(
        msg.contains("win11") && msg.contains("did not answer"),
        "{msg}"
    );
}

#[test]
fn garbage_and_stale_responses_are_skipped() {
    let (mut c, _) = pair(Faults {
        noisy: true,
        ..Faults::default()
    });
    assert!(c.ping().is_ok());
    assert_eq!(c.apps().unwrap().len(), 4);
}

#[test]
fn the_guest_answering_a_bad_body_is_a_failure_not_a_panic() {
    let (a, b) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let mut rd = BufReader::new(b.try_clone().unwrap());
        let mut w = b;
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        w.write_all(b"{\"id\":1,\"ok\":true,\"apps\":7}\n").unwrap();
    });
    let mut c = Client::from_stream(a).with_cap(Duration::from_millis(500));
    assert!(matches!(c.apps(), Err(CtlError::Failed(_))));
}

#[test]
fn folders_and_oversize_are_refused_before_anything_is_sent() {
    let d = tmpdir("folder");
    let (mut c, _) = pair(Faults::default());
    let e = c
        .put_file(&d, "C:\\x", false, &mut no_progress())
        .unwrap_err();
    assert!(matches!(e, CtlError::Failed(m) if m.contains("folder")));
    let e = c
        .put_file(&d.join("missing"), "C:\\x", false, &mut no_progress())
        .unwrap_err();
    assert!(matches!(e, CtlError::Failed(_)));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn line_reader_reassembles_split_responses() {
    // The channel may hand the client half a line at a time.
    let (a, b) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let mut rd = BufReader::new(b.try_clone().unwrap());
        let mut w = b;
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        let r = Response::ok(1, &Started { pid: 5 }).to_line();
        let (x, y) = r.split_at(7);
        w.write_all(x.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        w.write_all(y.as_bytes()).unwrap();
    });
    let mut c = Client::from_stream(a).with_cap(Duration::from_secs(2));
    assert_eq!(
        c.run(RunArgs {
            cmd: "x".into(),
            ..RunArgs::default()
        })
        .unwrap()
        .pid,
        5
    );
}

#[test]
fn app_names_resolve_exact_prefix_then_substring() {
    let list = apps();
    assert_eq!(pick_app(&list, "notepad").unwrap().name, "Notepad");
    assert_eq!(pick_app(&list, "NOTEPAD++").unwrap().name, "Notepad++");
    assert_eq!(
        pick_app(&list, "counter-strike 2").unwrap().target,
        "steam://rungameid/730"
    );
    assert_eq!(
        pick_app(&list, "go server").unwrap().name,
        "Counter-Strike: GO Server"
    );
    let e = format!("{}", pick_app(&list, "counter").unwrap_err());
    assert!(
        e.contains("several") && e.contains("Counter-Strike 2"),
        "{e}"
    );
    let e = format!("{}", pick_app(&list, "photoshop").unwrap_err());
    assert!(e.contains("no app called"), "{e}");
    // The same app twice is one.
    let mut dup = list.clone();
    dup.push(App {
        name: "NOTEPAD".into(),
        source: "desktop".into(),
        ..App::default()
    });
    assert!(pick_app(&dup, "notepad").is_ok());
}

#[test]
fn endpoints() {
    assert_eq!(
        Endpoint::parse("win11:C:\\Users\\me\\a.txt"),
        Endpoint::Remote {
            vm: "win11".into(),
            path: "C:\\Users\\me\\a.txt".into()
        }
    );
    assert_eq!(
        Endpoint::parse("win11:"),
        Endpoint::Remote {
            vm: "win11".into(),
            path: "".into()
        }
    );
    assert_eq!(
        Endpoint::parse("C:\\x.txt"),
        Endpoint::Local("C:\\x.txt".into())
    );
    assert_eq!(Endpoint::parse("./a:b"), Endpoint::Local("./a:b".into()));
    assert_eq!(
        Endpoint::parse("/tmp/a:b"),
        Endpoint::Local("/tmp/a:b".into())
    );
    assert_eq!(
        Endpoint::parse("a b.txt"),
        Endpoint::Local("a b.txt".into())
    );
    assert_eq!(Endpoint::parse("日本:x"), Endpoint::Local("日本:x".into()));
}

#[test]
fn target_paths() {
    let src = Path::new("/home/me/My File.txt");
    assert_eq!(remote_target("", src), "My File.txt");
    assert_eq!(remote_target("C:\\Temp\\", src), "C:\\Temp\\My File.txt");
    assert_eq!(remote_target("/tmp/", src), "/tmp/My File.txt");
    assert_eq!(remote_target("C:\\Temp\\x.txt", src), "C:\\Temp\\x.txt");
    assert_eq!(remote_basename("C:\\a\\b c.txt"), "b c.txt");
    assert_eq!(remote_basename("/a/b/"), "b");
    let d = tmpdir("lt");
    assert_eq!(local_target(&d, "C:\\a\\f.txt"), d.join("f.txt"));
    assert_eq!(local_target(&d.join("new"), "C:\\a\\f.txt"), d.join("new"));
    let slash = PathBuf::from(format!("{}/", d.join("nodir").display()));
    assert_eq!(local_target(&slash, "/x/f"), d.join("nodir/f"));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn shortcut_files() {
    assert_eq!(slug("Counter-Strike 2"), "counter-strike-2");
    assert_eq!(slug("  A / B  "), "a-b");
    assert_eq!(slug("日本語 ゲーム"), "日本語-ゲーム");
    assert_eq!(slug("///"), "app");
    assert_eq!(exec_quote("win11"), "win11");
    assert_eq!(exec_quote("Notepad++"), "Notepad++");
    assert_eq!(exec_quote("My \"App\" 100%"), "\"My \\\"App\\\" 100%%\"");
    assert_eq!(exec_quote("$x"), "\"\\$x\"");
    let e = desktop_entry(
        Path::new("/usr/bin/conduit"),
        "win11",
        "Counter-Strike 2",
        Some(Path::new("/h/.local/share/icons/conduit/i.png")),
    );
    assert!(
        e.contains("Exec=/usr/bin/conduit run win11 --start --app \"Counter-Strike 2\"\n"),
        "{e}"
    );
    assert!(e.contains("Icon=/h/.local/share/icons/conduit/i.png\n"));
    assert!(e.contains("X-Conduit-VM=win11\nX-Conduit-App=Counter-Strike 2\n"));
    assert!(!desktop_entry(Path::new("/c"), "v", "n", None).contains("Icon="));

    let d = tmpdir("sc");
    std::fs::write(desktop_file(&d, "win11", "Counter-Strike 2"), &e).unwrap();
    std::fs::write(
        desktop_file(&d, "win11", "Notepad"),
        desktop_entry(Path::new("/c"), "win11", "Notepad", None),
    )
    .unwrap();
    std::fs::write(
        desktop_file(&d, "other", "Notepad"),
        desktop_entry(Path::new("/c"), "other", "Notepad", None),
    )
    .unwrap();
    std::fs::write(d.join("unrelated.desktop"), "[Desktop Entry]\n").unwrap();
    let l = shortcuts(&d, "win11");
    assert_eq!(
        l.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["Counter-Strike 2", "Notepad"]
    );
    assert!(shortcuts(&d.join("nope"), "win11").is_empty());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn error_texts_name_the_fix() {
    let m = format!("{:#}", friendly("w", CtlError::Closed));
    assert!(m.contains("closed"), "{m}");
    assert!(AGENT_HINT.contains("conduit attach") && AGENT_HINT.contains("tray"));
}

// ------------------------------------------------------------ get hardening

fn guest_file(f: Faults, data: &[u8]) -> Client {
    let (c, g) = pair(f);
    g.lock()
        .unwrap()
        .files
        .insert("C:\\f".into(), data.to_vec());
    c
}

fn names(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn get_refuses_a_size_that_changes_mid_copy() {
    let d = tmpdir("grow");
    let mut c = guest_file(
        Faults {
            grow: true,
            ..Faults::default()
        },
        &vec![3u8; CHUNK * 2 + 5],
    );
    let e = c
        .get_file("C:\\f", &d.join("f"), false, &mut no_progress())
        .unwrap_err();
    assert!(
        matches!(&e, CtlError::Failed(m) if m.contains("changed")),
        "{e:?}"
    );
    assert!(names(&d).is_empty());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn get_stops_once_the_guest_sends_more_than_it_announced() {
    let d = tmpdir("overflow");
    let mut c = guest_file(
        Faults {
            overflow: true,
            ..Faults::default()
        },
        &vec![3u8; CHUNK * 3],
    );
    let mut most = 0u64;
    let e = c
        .get_file("C:\\f", &d.join("f"), false, &mut |n, t| {
            assert!(n <= t, "{n} of {t} bytes written");
            most = most.max(n);
        })
        .unwrap_err();
    assert!(
        matches!(&e, CtlError::Failed(m) if m.contains("more than")),
        "{e:?}"
    );
    assert_eq!(most, 0);
    assert!(names(&d).is_empty());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_trickling_guest_hits_the_whole_copy_time_limit() {
    let d = tmpdir("trickle");
    let mut c = guest_file(
        Faults {
            short_get: true,
            ..Faults::default()
        },
        &vec![1u8; 50_000],
    );
    c.budget = Some(Duration::from_millis(30));
    let t = Instant::now();
    let e = c
        .get_file("C:\\f", &d.join("f"), false, &mut no_progress())
        .unwrap_err();
    assert!(
        matches!(&e, CtlError::Failed(m) if m.contains("too slow")),
        "{e:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(names(&d).is_empty());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn the_default_time_limit_grows_with_the_size() {
    let (c, _) = pair(Faults::default());
    assert_eq!(c.budget(0), Duration::from_secs(60));
    assert_eq!(c.budget(MAX_FILE), Duration::from_secs(60 + 8192));
}

/// Both ways of holding a received file: `O_TMPFILE` and a named part file.
fn both(tag: &str, f: impl Fn(&Path, bool)) {
    for named in [false, true] {
        let d = tmpdir(&format!("{tag}-{named}"));
        f(&d, named);
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn a_file_of_the_part_name_that_was_there_is_never_touched() {
    both("foreign", |d, named| {
        let foreign = d.join("f.conduit-part");
        std::fs::write(&foreign, b"mine").unwrap();
        // A failed copy...
        let mut c = guest_file(
            Faults {
                grow: true,
                ..Faults::default()
            },
            &vec![3u8; CHUNK * 2],
        );
        c.no_tmpfile = named;
        assert!(c
            .get_file("C:\\f", &d.join("f"), false, &mut no_progress())
            .is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"mine");
        // ...and a good one.
        let mut c = guest_file(Faults::default(), b"data");
        c.no_tmpfile = named;
        c.get_file("C:\\f", &d.join("f"), false, &mut no_progress())
            .unwrap();
        assert_eq!(std::fs::read(&foreign).unwrap(), b"mine");
        assert_eq!(names(d), ["f", "f.conduit-part"]);
    });
}

#[test]
fn a_part_file_stays_out_of_sight_and_never_follows_a_symlink() {
    // O_TMPFILE: nothing has a name until the copy is complete.
    let d = tmpdir("hidden");
    let mut c = guest_file(Faults::default(), &vec![1u8; CHUNK * 2 + 1]);
    let dd = d.clone();
    c.get_file("C:\\f", &d.join("f"), false, &mut |_, _| {
        assert!(names(&dd).is_empty())
    })
    .unwrap();
    assert_eq!(names(&d), ["f"]);
    let _ = std::fs::remove_dir_all(&d);
    // A named part file is created fresh: a symlink planted at any likely
    // name is not followed and not removed.
    let d = tmpdir("nofollow");
    let victim = d.join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    for n in 0..3 {
        std::os::unix::fs::symlink(
            &victim,
            d.join(format!("f.{}-{n}.conduit-part", std::process::id())),
        )
        .unwrap();
    }
    let mut c = guest_file(Faults::default(), b"data");
    c.no_tmpfile = true;
    c.get_file("C:\\f", &d.join("f"), false, &mut no_progress())
        .unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_eq!(std::fs::read(d.join("f")).unwrap(), b"data");
    assert_eq!(names(&d).len(), 5);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_file_that_appears_during_the_copy_is_not_replaced_without_force() {
    both("race", |d, named| {
        let dst = d.join("f");
        let mut c = guest_file(Faults::default(), &vec![1u8; CHUNK + 1]);
        c.no_tmpfile = named;
        let e = c
            .get_file("C:\\f", &dst, false, &mut |n, t| {
                if n == t {
                    std::fs::write(&dst, b"theirs").unwrap();
                }
            })
            .unwrap_err();
        assert!(
            matches!(&e, CtlError::Failed(m) if m.contains("exists")),
            "{e:?}"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"theirs");
        assert_eq!(names(d), ["f"]);
        // --force replaces it.
        let mut c = guest_file(Faults::default(), b"new");
        c.no_tmpfile = named;
        c.get_file("C:\\f", &dst, true, &mut no_progress()).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert_eq!(names(d), ["f"]);
    });
}

#[test]
fn a_dangling_symlink_at_the_target_is_not_followed() {
    both("dangle", |d, named| {
        let dst = d.join("f");
        let away = d.join("away");
        std::os::unix::fs::symlink(&away, &dst).unwrap();
        let mut c = guest_file(Faults::default(), b"data");
        c.no_tmpfile = named;
        assert!(c
            .get_file("C:\\f", &dst, false, &mut no_progress())
            .is_err());
        assert!(!away.exists());
        c.get_file("C:\\f", &dst, true, &mut no_progress()).unwrap();
        assert!(!away.exists());
        assert!(!dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
    });
}

// ------------------------------------------------------------ guest text

#[test]
fn guest_text_is_cleaned_where_it_enters() {
    assert_eq!(
        clean("a\x1b[2Jb\r\nc\u{85}d\u{2028}e\u{202e}f\u{2066}g", 100),
        "a[2Jbcdefg"
    );
    assert_eq!(clean("Ærø 日本 ✓", 100), "Ærø 日本 ✓");
    assert_eq!(clean("abcdef", 3), "abc");

    let (mut c, _) = pair(Faults {
        hostile: true,
        ..Faults::default()
    });
    let a = &c.apps().unwrap()[0];
    assert!(a.name.starts_with("Evil[2Jgpj.exe"), "{}", a.name);
    assert_eq!(a.name.chars().count(), MAX_NAME);
    assert_eq!(a.target, "C:\\a.lnk");
    assert_eq!(a.args.len(), MAX_ARGS);
    assert_eq!(a.args[0], "--x[31m");
    assert_eq!((a.icon.as_str(), a.source.as_str()), ("i", "startmenu"));

    let e = c.run(RunArgs::default()).unwrap_err();
    assert_eq!(e, CtlError::Guest("no]0;pwned suchfile".into()));
    // Even a Guest error made elsewhere prints clean.
    let raw = CtlError::Guest("x\x1b[31my\n".into());
    assert_eq!(raw.to_string(), "x[31my");
    let m = format!("{:#}", friendly("w", raw));
    assert!(!m.chars().any(unsafe_char), "{m:?}");
}

#[test]
fn notification_text_is_escaped_markup() {
    let a = notify_args("a <b>bold</b> & \x1b[1m\"q\"");
    assert_eq!(a[4..6], ["--", "conduit"]);
    assert_eq!(a[6], "a &lt;b&gt;bold&lt;/b&gt; &amp; [1m&quot;q&quot;");
}

// ------------------------------------------------------------ icons

fn png(w: u32, h: u32, extra: usize) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b.extend(vec![0u8; extra]);
    b
}

fn icon(format: &str, b: &[u8]) -> Icon {
    Icon {
        format: format.into(),
        data: b64_encode(b),
    }
}

#[test]
fn only_small_pngs_become_host_icons() {
    let ok = png(64, 64, 100);
    assert_eq!(png_size(&ok), Some((64, 64)));
    assert_eq!(check_icon(&icon("png", &ok)).unwrap(), ok);
    assert!(check_icon(&icon("png", &png(256, 256, 0))).is_ok());
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>";
    assert!(check_icon(&icon("svg", svg))
        .unwrap_err()
        .contains("only PNG"));
    // An SVG (or anything) labelled png is still refused.
    assert!(check_icon(&icon("png", svg))
        .unwrap_err()
        .contains("not a PNG"));
    assert!(check_icon(&icon("png", &png(257, 16, 0)))
        .unwrap_err()
        .contains("257x16"));
    assert!(check_icon(&icon("png", &png(0, 16, 0))).is_err());
    assert!(check_icon(&icon("png", &png(64, 64, MAX_ICON)))
        .unwrap_err()
        .contains("too large"));
    assert!(check_icon(&icon("png", b"")).unwrap_err().contains("empty"));
    let mut bad_ihdr = ok.clone();
    bad_ihdr[12..16].copy_from_slice(b"IDAT");
    assert!(check_icon(&icon("png", &bad_ihdr)).is_err());
    let damaged = Icon {
        format: "png".into(),
        data: "!!!".into(),
    };
    assert!(check_icon(&damaged).unwrap_err().contains("damaged"));
}

// ------------------------------------------------------------ desktop entries

/// A Desktop Entry `Exec=` value as a launcher reads it: key-file unescape,
/// then the Exec quoting rules.
fn exec_args(value: &str) -> Vec<String> {
    let v = keyfile_unescape(value);
    let (mut out, mut cur, mut it) = (Vec::new(), String::new(), v.chars());
    let mut quoted = false;
    let mut has = false;
    while let Some(c) = it.next() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            '\\' if quoted => cur.push(it.next().unwrap()),
            ' ' if !quoted => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                has = false;
            }
            '%' => {
                assert_eq!(it.next(), Some('%'));
                cur.push('%');
            }
            _ => cur.push(c),
        }
    }
    assert!(!quoted);
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[test]
fn desktop_entries_escape_then_quote_like_a_launcher_reads_them() {
    let name = "C:\\Games\\\"Odd\" $HOME `x` 100% \x1b[2J\nName=Evil";
    let e = desktop_entry(
        Path::new("/opt/my apps/conduit"),
        "win11",
        name,
        Some(Path::new("/h/ic\\on.png")),
    );
    // One line per key: the newline in the guest's name did not add a key.
    assert_eq!(e.lines().filter(|l| l.starts_with("Name=")).count(), 1);
    let field = |k: &str| {
        e.lines()
            .find_map(|l| l.strip_prefix(k))
            .unwrap_or_else(|| panic!("{k} in {e}"))
    };
    let shown = clean(name, MAX_NAME);
    assert_eq!(keyfile_unescape(field("Name=")), shown);
    assert_eq!(keyfile_unescape(field("Icon=")), "/h/ic\\on.png");
    assert_eq!(
        exec_args(field("Exec=")),
        [
            "/opt/my apps/conduit",
            "run",
            "win11",
            "--start",
            "--app",
            shown.as_str()
        ]
    );
    // The literal backslash inside the quoted argument takes four.
    assert!(field("Exec=").contains("C:\\\\\\\\Games"), "{e}");
    // `conduit app list` reads the name back as it was.
    let d = tmpdir("kf");
    std::fs::write(desktop_file(&d, "win11", name), &e).unwrap();
    assert_eq!(shortcuts(&d, "win11")[0].0, shown);
    let _ = std::fs::remove_dir_all(d);
    assert_eq!(keyfile_escape(" a\tb\\"), "\\sa\\tb\\\\");
    assert_eq!(keyfile_unescape("\\sa\\tb\\\\"), " a\tb\\");
}

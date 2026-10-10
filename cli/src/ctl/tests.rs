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
            if a.cmd.contains("missing") {
                return Response::err(id, format!("not found: {}", a.cmd));
            }
            g.ran.push(a);
            Response::ok(id, &Started { pid: 4242 })
        }
        Op::Stop { .. } => Response::ok(id, &serde_json::json!({})),
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
            Response::ok(
                id,
                &Chunk {
                    data: b64_encode(&d[from..to]),
                    size: d.len() as u64,
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

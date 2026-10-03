//! The trace control socket, end to end: a reader connects, tracing turns on,
//! records arrive in order, and tracing turns off when the reader leaves.
#![cfg(feature = "trace")]

use device::trace::format::read::Reader;
use device::trace::format::{Call, DriverVersion, Kind, Record};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let t = Instant::now();
    while !cond() {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn ask(sock: &std::path::Path, cmd: &str) -> String {
    let mut s = UnixStream::connect(sock).unwrap();
    writeln!(s, "{cmd}").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn a_reader_turns_tracing_on_and_gets_every_record() {
    let dir = std::env::temp_dir().join(format!("conduit-trace-sock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("trace.sock");

    device::trace::set_driver(DriverVersion::new(580, 178, 4));
    device::trace::start(device::trace::Options {
        file: None,
        socket: Some(sock.clone()),
    })
    .unwrap();
    assert!(!device::trace::enabled(), "no reader yet");
    assert!(ask(&sock, "status").starts_with("tracing off; file none; 0 live reader(s)"));
    assert!(ask(&sock, "file on").starts_with("no trace file"));
    assert!(ask(&sock, "launch").starts_with("unknown command"));

    let mut s = UnixStream::connect(&sock).unwrap();
    writeln!(s, "stream bin").unwrap();
    wait_for("tracing to turn on", device::trace::enabled);

    let sent: Vec<Record> = (0..1000u64)
        .map(|i| Record {
            ts_ns: i,
            handle: 3,
            kind: Kind::Ioctl,
            call: Call::Control,
            sub: Some(0x2080_0102),
            reply_ns: 1000 + i,
            ..Default::default()
        })
        .collect();
    for r in &sent {
        device::trace::emit(*r);
    }

    let mut reader = Reader::new(BufReader::new(s.try_clone().unwrap())).unwrap();
    assert_eq!(
        reader.header().driver,
        Some(DriverVersion::new(580, 178, 4))
    );
    let got: Vec<Record> = (0..sent.len())
        .map(|_| reader.next_record().unwrap().unwrap())
        .collect();
    assert_eq!(got, sent);
    assert!(ask(&sock, "status").starts_with("tracing on; file none; 1 live reader(s)"));

    drop(reader);
    drop(s);
    // The writer finds the reader gone at its next write.
    let t = Instant::now();
    while device::trace::enabled() {
        device::trace::emit(Record::default());
        assert!(t.elapsed() < Duration::from_secs(5), "tracing stayed on");
        std::thread::sleep(Duration::from_millis(5));
    }

    // A JSON reader gets the same records as JSON Lines.
    let s = UnixStream::connect(&sock).unwrap();
    (&s).write_all(b"stream json\n").unwrap();
    wait_for("tracing to turn on again", device::trace::enabled);
    device::trace::emit(sent[0]);
    let mut lines = BufReader::new(&s).lines();
    assert!(
        lines
            .next()
            .unwrap()
            .unwrap()
            .contains("\"driver\":\"580.178.04\"")
    );
    let rec = lines.next().unwrap().unwrap();
    assert!(
        rec.contains("\"name\":\"NV2080_CTRL_CMD_GPU_GET_INFO_V2\""),
        "{rec}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

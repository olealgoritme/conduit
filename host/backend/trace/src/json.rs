//! JSON Lines: one object per line, flat, greppable.
//!
//! The first line of a file is a header object, `{"conduit_trace":1,...}`.
//! Every other line is a record or a `dropped` marker. Numbers that are
//! identifiers (ioctl numbers, classes, commands, RM status) are hex strings,
//! so `grep 0x20800102` finds them; times are microseconds with three
//! decimals, which is exact to the nanosecond the binary format holds.

use crate::{Call, DriverVersion, Header, Kind, Record, Refusal, names};
use std::fmt::Write as _;

/// The header line, without its newline.
pub fn header_line(h: &Header) -> String {
    match h.driver {
        Some(v) => format!("{{\"conduit_trace\":1,\"driver\":\"{v}\"}}"),
        None => "{\"conduit_trace\":1,\"driver\":null}".to_string(),
    }
}

fn us(out: &mut String, key: &str, ns: u64) {
    let _ = write!(out, ",\"{key}\":{}.{:03}", ns / 1000, ns % 1000);
}

/// Append one record as a line, newline included.
pub fn write_record(out: &mut String, r: &Record, driver: Option<DriverVersion>) {
    if r.kind == Kind::Dropped {
        let _ = writeln!(
            out,
            "{{\"ts_ns\":{},\"kind\":\"dropped\",\"count\":{}}}",
            r.ts_ns,
            r.sub.unwrap_or(0)
        );
        return;
    }
    let n = names(r, driver);
    let _ = write!(
        out,
        "{{\"ts_ns\":{},\"handle\":{},\"kind\":\"{}\",\"call\":\"{}\",\"nr\":\"{:#x}\"",
        r.ts_ns, r.handle, r.kind, r.call, r.nr
    );
    if let Some(op) = &n.op {
        let _ = write!(out, ",\"op\":\"{op}\"");
    }
    if let Some(s) = r.sub {
        let _ = write!(out, ",\"sub\":\"{s:#x}\"");
    }
    if let Some(name) = n.sub {
        let _ = write!(out, ",\"name\":\"{name}\"");
    }
    let _ = write!(
        out,
        ",\"in\":{},\"out\":{},\"errno\":{}",
        r.size_in, r.size_out, r.errno
    );
    if let Some(s) = r.nv_status {
        let _ = write!(out, ",\"nv_status\":\"{s:#x}\"");
    }
    if r.refusal != Refusal::None {
        let _ = write!(out, ",\"refusal\":\"{}\"", r.refusal);
    }
    let _ = write!(out, ",\"host_calls\":{}", r.host_calls);
    if let (Some(q), Some(h), Some(a)) = (r.queue_ns(), r.host_time_ns(), r.after_host_ns()) {
        us(out, "queue_us", q);
        us(out, "host_us", h);
        us(out, "after_us", a);
    }
    us(out, "total_us", r.total_ns());
    out.push_str("}\n");
}

/// A value in one of our flat objects. Numbers are kept as written, because
/// a `u64` timestamp does not survive a trip through `f64`.
#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    Num(String),
    Null,
    Bool(bool),
}

/// Parse a flat JSON object: string keys, scalar values, no nesting.
fn parse_object(line: &str) -> Result<Vec<(String, Value)>, String> {
    let b = line.trim().as_bytes();
    let mut i = 0;
    let ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    let string = |i: &mut usize| -> Result<String, String> {
        if b.get(*i) != Some(&b'"') {
            return Err(format!("expected a string at {i}"));
        }
        *i += 1;
        let mut s = String::new();
        while *i < b.len() {
            match b[*i] {
                b'"' => {
                    *i += 1;
                    return Ok(s);
                }
                b'\\' => {
                    *i += 1;
                    match b.get(*i) {
                        Some(b'n') => s.push('\n'),
                        Some(b't') => s.push('\t'),
                        Some(&c) => s.push(c as char),
                        None => break,
                    }
                    *i += 1;
                }
                _ => {
                    // Copy a whole UTF-8 sequence at once.
                    let start = *i;
                    *i += 1;
                    while *i < b.len() && (b[*i] & 0xc0) == 0x80 {
                        *i += 1;
                    }
                    s.push_str(std::str::from_utf8(&b[start..*i]).map_err(|e| e.to_string())?);
                }
            }
        }
        Err("unterminated string".into())
    };

    ws(&mut i);
    if b.get(i) != Some(&b'{') {
        return Err("not a JSON object".into());
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        ws(&mut i);
        if b.get(i) == Some(&b'}') {
            break;
        }
        let k = string(&mut i)?;
        ws(&mut i);
        if b.get(i) != Some(&b':') {
            return Err(format!("expected ':' after \"{k}\""));
        }
        i += 1;
        ws(&mut i);
        let v = match b.get(i) {
            Some(b'"') => Value::Str(string(&mut i)?),
            Some(b'n') if b[i..].starts_with(b"null") => {
                i += 4;
                Value::Null
            }
            Some(b't') if b[i..].starts_with(b"true") => {
                i += 4;
                Value::Bool(true)
            }
            Some(b'f') if b[i..].starts_with(b"false") => {
                i += 5;
                Value::Bool(false)
            }
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_digit() || b"-+.eE".contains(&b[i])) {
                    i += 1;
                }
                Value::Num(String::from_utf8_lossy(&b[start..i]).into_owned())
            }
            _ => return Err(format!("unexpected value for \"{k}\"")),
        };
        out.push((k, v));
        ws(&mut i);
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => break,
            _ => return Err("expected ',' or '}'".into()),
        }
    }
    Ok(out)
}

/// `"17.345"` microseconds as nanoseconds, exactly.
fn us_to_ns(s: &str) -> Option<u64> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut f = frac.to_string();
    f.truncate(3);
    while f.len() < 3 {
        f.push('0');
    }
    Some(int.parse::<u64>().ok()? * 1000 + f.parse::<u64>().ok()?)
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

/// One line of a JSON trace.
#[derive(Debug, PartialEq)]
pub enum Line {
    Header(Header),
    Record(Record),
}

/// Read one line back. Names are not read: they are derived from the numbers.
pub fn parse_line(line: &str) -> Result<Line, String> {
    let obj = parse_object(line)?;
    let get = |k: &str| obj.iter().find(|(key, _)| key == k).map(|(_, v)| v);
    let num = |k: &str| match get(k) {
        Some(Value::Num(n)) => Some(n.as_str()),
        _ => None,
    };
    let text = |k: &str| match get(k) {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    };

    if get("conduit_trace").is_some() {
        let driver = text("driver").and_then(DriverVersion::parse);
        return Ok(Line::Header(Header { driver }));
    }
    let ts_ns = num("ts_ns")
        .and_then(|n| n.parse().ok())
        .ok_or("no ts_ns")?;
    let kind = text("kind").and_then(Kind::parse).ok_or("no kind")?;
    if kind == Kind::Dropped {
        let count = num("count").and_then(|n| n.parse().ok()).unwrap_or(0);
        return Ok(Line::Record(Record::dropped(ts_ns, count)));
    }
    let int = |k: &str| num(k).and_then(|n| n.parse::<i64>().ok());
    let total = num("total_us").and_then(us_to_ns).ok_or("no total_us")?;
    let host_ns = match (
        num("queue_us").and_then(us_to_ns),
        num("host_us").and_then(us_to_ns),
    ) {
        (Some(q), Some(h)) => Some((q, q + h)),
        _ => None,
    };
    Ok(Line::Record(Record {
        ts_ns,
        handle: int("handle").unwrap_or(0) as u32,
        kind,
        call: text("call").and_then(Call::parse).unwrap_or(Call::Other),
        refusal: text("refusal").and_then(Refusal::parse).unwrap_or_default(),
        nr: text("nr").and_then(hex).unwrap_or(0),
        sub: text("sub").and_then(hex),
        size_in: int("in").unwrap_or(0) as u32,
        size_out: int("out").unwrap_or(0) as u32,
        errno: int("errno").unwrap_or(0) as i32,
        nv_status: text("nv_status").and_then(hex),
        host_calls: int("host_calls").unwrap_or(0) as u16,
        host_ns,
        reply_ns: total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::sample;

    #[test]
    fn a_record_survives_json() {
        let v = Some(DriverVersion::new(580, 178, 4));
        for r in [
            sample(),
            Record {
                sub: None,
                nv_status: None,
                host_ns: None,
                errno: 1,
                refusal: Refusal::HostDisplay,
                call: Call::Nvkms,
                ..sample()
            },
            Record::dropped(99, 1234),
        ] {
            let mut s = String::new();
            write_record(&mut s, &r, v);
            assert!(s.ends_with('\n') && s.matches('\n').count() == 1, "{s}");
            assert_eq!(parse_line(&s), Ok(Line::Record(r)), "{s}");
        }
    }

    #[test]
    fn a_json_record_names_what_it_can() {
        let mut s = String::new();
        write_record(&mut s, &sample(), None);
        assert!(s.contains("\"op\":\"RM_CONTROL\""), "{s}");
        assert!(
            s.contains("\"name\":\"NV2080_CTRL_CMD_GPU_GET_INFO_V2\""),
            "{s}"
        );
        assert!(s.contains("\"sub\":\"0x20800102\""), "{s}");
        assert!(s.contains("\"total_us\":17.345"), "{s}");
        assert!(s.contains("\"host_us\":14.700"), "{s}");
    }

    #[test]
    fn the_header_line_round_trips() {
        for h in [
            Header::default(),
            Header {
                driver: Some(DriverVersion::new(615, 71, 9)),
            },
        ] {
            assert_eq!(parse_line(&header_line(&h)), Ok(Line::Header(h)));
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for bad in [
            "",
            "[]",
            "{",
            "{\"ts_ns\":}",
            "{\"kind\":\"ioctl\"}",
            "{\"a\":\"\\",
        ] {
            assert!(parse_line(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn microseconds_convert_exactly() {
        assert_eq!(us_to_ns("17.345"), Some(17_345));
        assert_eq!(us_to_ns("0.001"), Some(1));
        assert_eq!(us_to_ns("12"), Some(12_000));
        assert_eq!(us_to_ns("1.5"), Some(1_500));
    }
}

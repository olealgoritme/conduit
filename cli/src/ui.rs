//! Messages for people: short, plain words, and a "what to do" line when we know one.

use std::fmt;

/// An error the user can act on. `hint` is printed under the message.
#[derive(Debug)]
pub struct Friendly {
    pub msg: String,
    pub hint: Option<String>,
}

impl fmt::Display for Friendly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Friendly {}

/// Build a friendly error: `oops("No VM called x", "Run `conduit list` to see your VMs")`.
pub fn oops(msg: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    let hint = hint.into();
    anyhow::Error::new(Friendly {
        msg: msg.into(),
        hint: if hint.is_empty() { None } else { Some(hint) },
    })
}

pub fn info(msg: impl AsRef<str>) {
    eprintln!("conduit: {}", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    eprintln!("conduit: warning: {}", msg.as_ref());
}

/// Print an error and its causes; a `Friendly` hint goes last.
pub fn report(err: &anyhow::Error) {
    eprintln!("conduit: error: {err}");
    let mut hint = None;
    for cause in err.chain() {
        if let Some(f) = cause.downcast_ref::<Friendly>() {
            if hint.is_none() {
                hint = f.hint.clone();
            }
        }
    }
    for cause in err.chain().skip(1) {
        eprintln!("  because: {cause}");
    }
    if let Some(h) = hint {
        for line in h.lines() {
            eprintln!("  -> {line}");
        }
    }
}

/// Quote a string for /bin/sh.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Human size, e.g. 4.2 GB.
pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Parse "64G", "8GiB", "512M", "4096" (MiB when no unit is given for `default_unit` = M).
pub fn parse_size(s: &str, default_unit: char) -> anyhow::Result<u64> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| {
        oops(
            format!("\"{s}\" is not a size"),
            "Write sizes like 8G, 512M or 64G",
        )
    })?;
    let unit = unit.trim().to_ascii_uppercase();
    let unit = unit.trim_end_matches("IB").trim_end_matches('B');
    let unit = if unit.is_empty() { default_unit.to_string() } else { unit.to_string() };
    let mult: u64 = match unit.as_str() {
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => {
            return Err(oops(
                format!("unknown size unit in \"{s}\""),
                "Use K, M, G or T, e.g. 8G",
            ))
        }
    };
    let bytes = (n * mult as f64) as u64;
    if bytes == 0 {
        return Err(oops(format!("size \"{s}\" is zero"), ""));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("64G", 'G').unwrap(), 64 << 30);
        assert_eq!(parse_size("8GiB", 'G').unwrap(), 8 << 30);
        assert_eq!(parse_size("8gb", 'G').unwrap(), 8 << 30);
        assert_eq!(parse_size("512M", 'G').unwrap(), 512 << 20);
        assert_eq!(parse_size("4096", 'M').unwrap(), 4096 << 20);
        assert_eq!(parse_size("1.5G", 'G').unwrap(), 3 << 29);
        assert!(parse_size("lots", 'G').is_err());
        assert!(parse_size("8X", 'G').is_err());
        assert!(parse_size("0G", 'G').is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/opt/conduit/bin/conduit"), "/opt/conduit/bin/conduit");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}

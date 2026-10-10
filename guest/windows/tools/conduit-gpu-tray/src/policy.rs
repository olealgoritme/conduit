//! The decisions and string handling behind the Windows shell's
//! privilege-sensitive paths (hand-off to an elevated copy, the logon task,
//! the `--send-list` file, the share drive check, the package URI), kept
//! free of Windows calls so they are unit tested.

/// `TOKEN_ELEVATION_TYPE` of this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    /// No split token: a standard user, or UAC is off.
    Default,
    /// The elevated half of a split (administrator) token.
    Full,
    /// The filtered half of a split token: an administrator running
    /// without elevation.
    Limited,
    Unknown,
}

impl Elevation {
    pub fn from_raw(v: i32) -> Elevation {
        match v {
            1 => Elevation::Default,
            2 => Elevation::Full,
            3 => Elevation::Limited,
            _ => Elevation::Unknown,
        }
    }
}

/// What to do when Windows denies the (admin-only) stats port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnDenied {
    /// Start an elevated copy and leave once its window shows up.
    HandOff,
    /// Stay up and say that administrator rights are needed.
    NeedsAdmin,
}

/// Only an administrator running unelevated can get an elevated copy of
/// itself, and only one attempt is made per run (no prompt loop).
pub fn on_denied(e: Elevation, tried: bool) -> OnDenied {
    if e == Elevation::Limited && !tried {
        OnDenied::HandOff
    } else {
        OnDenied::NeedsAdmin
    }
}

/// Lower-case, backslashes, no `\\?\` prefix, no trailing separator.
fn norm(p: &str) -> String {
    let p = p.replace('/', "\\");
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p);
    p.trim_end_matches('\\').to_lowercase()
}

/// Is `path` strictly inside `dir`? Windows comparison (case-insensitive,
/// either separator); a path with `.` or `..` components is never inside.
pub fn is_under(path: &str, dir: &str) -> bool {
    let (p, d) = (norm(path), norm(dir));
    if d.is_empty() || p.split('\\').any(|c| c == "." || c == "..") {
        return false;
    }
    p.strip_prefix(&d)
        .and_then(|r| r.strip_prefix('\\'))
        .is_some_and(|r| !r.is_empty())
}

/// The install directory the logon task may point at.
pub fn install_dir(program_files: &str) -> String {
    format!("{}\\Conduit", program_files.trim_end_matches(['\\', '/']))
}

/// Text for an XML element or attribute value.
pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// A single-quoted PowerShell string literal (`'` doubled).
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The logon task (`schtasks /create /xml`): `exe` with the highest rights
/// each member of Users has, at every logon, one instance per user session
/// (`Parallel`: a second user's logon must not be ignored while the first
/// user's copy runs), no time limit.
pub fn task_xml(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Conduit GPU tray app</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <GroupId>S-1-5-32-545</GroupId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        xml_escape(exe)
    )
}

/// The `--send-list` file's name prefix (`%TEMP%\conduit-send-*.txt`).
pub const SEND_LIST_PREFIX: &str = "conduit-send-";

/// May `--send-list` delete `file`? Only a list file the shell command or a
/// drop wrote: directly in `temp_dir`, named `conduit-send-*.txt`.
pub fn send_list_deletable(file: &str, temp_dir: &str) -> bool {
    let f = norm(file);
    let Some(cut) = f.rfind('\\') else {
        return false;
    };
    let (dir, name) = (&f[..cut], &f[cut + 1..]);
    dir == norm(temp_dir)
        && !temp_dir.trim().is_empty()
        && name.len() > SEND_LIST_PREFIX.len() + ".txt".len()
        && name.starts_with(SEND_LIST_PREFIX)
        && name.ends_with(".txt")
        && !name.contains([':', '*', '?'])
}

/// The paths in a list file: UTF-16LE (a leading BOM is skipped), one per
/// `\n`; a `\r` before it is dropped, nothing else is trimmed (a name may
/// start with a space). Lone surrogates are kept as they are.
pub fn parse_send_list(bytes: &[u8]) -> Vec<Vec<u16>> {
    let mut units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    if units.first() == Some(&0xFEFF) {
        units.remove(0);
    }
    units
        .split(|&u| u == b'\n' as u16)
        .map(|l| l.strip_suffix(&[b'\r' as u16]).unwrap_or(l))
        .filter(|l| !l.is_empty() && !l.contains(&0))
        .map(<[u16]>::to_vec)
        .collect()
}

/// A list file's contents (see [`parse_send_list`]).
pub fn send_list_bytes(paths: &[Vec<u16>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            out.extend((b'\n' as u16).to_le_bytes());
        }
        for u in p {
            out.extend(u.to_le_bytes());
        }
    }
    out
}

/// The file system name virtiofs.exe gives its volumes.
pub const VIRTIOFS_FS: &str = "VirtIO-FS";
/// Longest volume label Windows reports (WinFsp truncates longer tags).
const MAX_LABEL: usize = 31;

/// Is the volume on a share's drive letter really that share? virtiofs.exe
/// labels the volume with the device tag.
pub fn share_volume_matches(tag: &str, label: &str, fs: &str) -> bool {
    if !fs.eq_ignore_ascii_case(VIRTIOFS_FS) || label.is_empty() {
        return false;
    }
    let (t, l) = (tag.to_lowercase(), label.to_lowercase());
    t == l || (l.chars().count() >= MAX_LABEL && t.starts_with(&l))
}

/// A `file:` URI for a local or UNC Windows path (the package manager takes
/// URIs): UTF-8, percent-encoded outside the unreserved set.
pub fn file_uri(path: &str) -> String {
    let p = path.replace('\\', "/");
    let (prefix, rest) = match p.strip_prefix("//") {
        Some(unc) => ("file://", unc.to_string()),
        None => ("file:///", p),
    };
    let mut o = String::from(prefix);
    for (i, b) in rest.bytes().enumerate() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/')
            || (b == b':' && i == 1);
        if keep {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_off_only_from_a_limited_admin_token_once() {
        assert_eq!(on_denied(Elevation::Limited, false), OnDenied::HandOff);
        assert_eq!(on_denied(Elevation::Limited, true), OnDenied::NeedsAdmin);
        for e in [Elevation::Default, Elevation::Full, Elevation::Unknown] {
            assert_eq!(on_denied(e, false), OnDenied::NeedsAdmin, "{e:?}");
        }
        assert_eq!(Elevation::from_raw(1), Elevation::Default);
        assert_eq!(Elevation::from_raw(2), Elevation::Full);
        assert_eq!(Elevation::from_raw(3), Elevation::Limited);
        assert_eq!(Elevation::from_raw(0), Elevation::Unknown);
    }

    #[test]
    fn install_dir_check() {
        let d = install_dir(r"C:\Program Files\");
        assert_eq!(d, r"C:\Program Files\Conduit");
        assert!(is_under(
            r"C:\Program Files\Conduit\conduit-gpu-tray.exe",
            &d
        ));
        assert!(is_under(r"c:/program files/conduit/x.exe", &d));
        assert!(is_under(r"\\?\C:\Program Files\Conduit\x.exe", &d));
        assert!(!is_under(r"C:\Program Files\ConduitEvil\x.exe", &d));
        assert!(!is_under(r"C:\Program Files\Conduit", &d));
        assert!(!is_under(r"C:\Program Files\Conduit\..\x.exe", &d));
        assert!(!is_under(r"C:\Users\a\Downloads\conduit-gpu-tray.exe", &d));
        assert!(!is_under(r"C:\x.exe", ""));
    }

    #[test]
    fn quoting() {
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(
            xml_escape(r#"a&b<c>"d"'e"#),
            "a&amp;b&lt;c&gt;&quot;d&quot;&apos;e"
        );
        let x = task_xml(r"C:\Program Files\Conduit\a&b.exe");
        assert!(x.contains(r"<Command>C:\Program Files\Conduit\a&amp;b.exe</Command>"));
        assert!(x.contains("<MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>"));
        assert!(x.contains("<GroupId>S-1-5-32-545</GroupId>"));
        assert!(x.contains("<RunLevel>HighestAvailable</RunLevel>"));
    }

    #[test]
    fn send_list_deletes_only_its_own_files() {
        let t = r"C:\Users\a\AppData\Local\Temp";
        assert!(send_list_deletable(
            &format!(r"{t}\conduit-send-1-2.txt"),
            t
        ));
        assert!(send_list_deletable(
            &format!(r"{t}\CONDUIT-SEND-1.TXT"),
            &format!("{t}\\")
        ));
        assert!(!send_list_deletable(&format!(r"{t}\conduit-send-.txt"), t));
        assert!(!send_list_deletable(&format!(r"{t}\other.txt"), t));
        assert!(!send_list_deletable(
            &format!(r"{t}\conduit-send-1.txt.exe"),
            t
        ));
        assert!(!send_list_deletable(
            &format!(r"{t}\sub\conduit-send-1.txt"),
            t
        ));
        assert!(!send_list_deletable(r"C:\Windows\conduit-send-1.txt", t));
        assert!(!send_list_deletable(
            &format!(r"{t}\conduit-send-1.txt:ads.txt"),
            t
        ));
        assert!(!send_list_deletable(r"conduit-send-1.txt", t));
        assert!(!send_list_deletable(r"C:\conduit-send-1.txt", ""));
    }

    #[test]
    fn send_list_trims_only_cr() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let mut bytes = vec![0xFF, 0xFE]; // BOM
        bytes.extend(send_list_bytes(&[
            w(" lead.txt"),
            w("trail \r"),
            w(""),
            w("x"),
        ]));
        let got = parse_send_list(&bytes);
        assert_eq!(got, vec![w(" lead.txt"), w("trail "), w("x")]);
        let lone = vec![b'a' as u16, 0xDC00];
        assert_eq!(
            parse_send_list(&send_list_bytes(std::slice::from_ref(&lone))),
            vec![lone]
        );
        assert!(parse_send_list(&[]).is_empty());
        assert!(parse_send_list(&[b'a', 0, 0, 0]).is_empty(), "embedded NUL");
    }

    #[test]
    fn share_volume_check() {
        assert!(share_volume_matches(
            "conduit-Conduit",
            "conduit-Conduit",
            "VirtIO-FS"
        ));
        assert!(share_volume_matches(
            "conduit-Conduit",
            "CONDUIT-CONDUIT",
            "virtio-fs"
        ));
        assert!(!share_volume_matches(
            "conduit-Conduit",
            "conduit-Conduit",
            "NTFS"
        ));
        assert!(!share_volume_matches(
            "conduit-Conduit",
            "Games",
            "VirtIO-FS"
        ));
        assert!(!share_volume_matches("conduit-Conduit", "", "VirtIO-FS"));
        assert!(!share_volume_matches(
            "conduit-Conduit",
            "conduit-Con",
            "VirtIO-FS"
        ));
        let long = "conduit-a-very-long-share-name-that-is-cut";
        assert!(share_volume_matches(long, &long[..31], "VirtIO-FS"));
    }

    #[test]
    fn file_uris() {
        assert_eq!(
            file_uri(r"C:\Program Files\Conduit\ConduitShellMenu.msix"),
            "file:///C:/Program%20Files/Conduit/ConduitShellMenu.msix"
        );
        assert_eq!(
            file_uri(r"\\srv\share\a#b.msix"),
            "file://srv/share/a%23b.msix"
        );
        assert_eq!(file_uri(r"C:\Pr\ø:x"), "file:///C:/Pr/%C3%B8%3Ax");
    }
}

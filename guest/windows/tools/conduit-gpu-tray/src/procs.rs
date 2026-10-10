//! Launch bookkeeping that needs no Windows: command lines, environment
//! blocks, what a target is, process trees and the table of what we started.

use std::collections::{BTreeMap, HashSet, VecDeque};

/// How a `run` target is started.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// A program: started directly, so there is a pid.
    Exe,
    /// A URL (`steam://...`, `https://...`): the shell handles it.
    Url,
    /// A shortcut, document or script: the shell handles it.
    Shell,
}

pub fn classify(cmd: &str) -> Target {
    let c = cmd.trim();
    if let Some((scheme, _)) = c.split_once(':') {
        let ok = scheme.len() >= 2
            && scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'));
        if ok {
            return Target::Url;
        }
    }
    let ext = c
        .rsplit(['\\', '/'])
        .next()
        .and_then(|f| f.rsplit_once('.'));
    match ext {
        Some((_, e)) if e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("com") => {
            Target::Exe
        }
        _ => Target::Shell,
    }
}

/// A short name for the menu: the file name without extension, or the URL.
pub fn display_name(cmd: &str) -> String {
    let c = cmd.trim();
    if classify(c) == Target::Url {
        return c.to_string();
    }
    let f = c.rsplit(['\\', '/']).next().unwrap_or(c);
    match f.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => f.to_string(),
    }
}

/// One argument quoted the way `CommandLineToArgvW` reads it back.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', slashes));
                slashes = 0;
                out.push(c);
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// `exe` and its arguments as one command line.
pub fn command_line(exe: &str, args: &[String]) -> String {
    let mut s = quote_arg(exe);
    for a in args {
        s.push(' ');
        s.push_str(&quote_arg(a));
    }
    s
}

/// Splits a UTF-16 environment block (`K=V\0K=V\0\0`) into pairs. A leading
/// `=` belongs to the name (`=C:=C:\` drive entries).
pub fn parse_env_block(block: &[u16]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in block.split(|&u| u == 0) {
        if entry.is_empty() {
            break;
        }
        let s = String::from_utf16_lossy(entry);
        let skip = s.chars().next().map_or(0, char::len_utf8);
        if let Some(i) = s[skip..].find('=') {
            let (k, v) = s.split_at(skip + i);
            out.push((k.to_string(), v[1..].to_string()));
        }
    }
    out
}

/// A UTF-16 environment block of `base` with `over` applied (names compare
/// without case; a name that is empty or has `=` or NUL is ignored),
/// sorted the way Windows wants it.
pub fn env_block(base: Vec<(String, String)>, over: &BTreeMap<String, String>) -> Vec<u16> {
    let mut vars = base;
    for (k, v) in over {
        if k.is_empty() || k.contains(['=', '\0']) || v.contains('\0') {
            continue;
        }
        vars.retain(|(b, _)| !b.eq_ignore_ascii_case(k));
        vars.push((k.clone(), v.clone()));
    }
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut out = Vec::new();
    for (k, v) in vars {
        out.extend(k.encode_utf16());
        out.push(b'=' as u16);
        out.extend(v.encode_utf16());
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    out
}

/// `root` and every process below it, from `(pid, parent pid)` pairs.
pub fn descendants(root: u32, procs: &[(u32, u32)]) -> Vec<u32> {
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        for &(pid, ppid) in procs {
            if ppid == parent && pid != 0 && seen.insert(pid) {
                out.push(pid);
            }
        }
        i += 1;
    }
    out
}

/// One program we started.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch<H> {
    pub name: String,
    pub pid: u32,
    /// Unix seconds.
    pub started: u64,
    pub extra: H,
}

/// The programs we started, newest last, bounded.
pub struct LaunchTable<H> {
    items: VecDeque<Launch<H>>,
    cap: usize,
}

impl<H> LaunchTable<H> {
    pub const fn new(cap: usize) -> Self {
        LaunchTable {
            items: VecDeque::new(),
            cap,
        }
    }

    /// Adds a launch; returns the `extra` of any launch that fell off.
    pub fn record(&mut self, name: String, pid: u32, started: u64, extra: H) -> Vec<H> {
        let mut gone = Vec::new();
        if let Some(i) = self.items.iter().position(|l| l.pid == pid) {
            gone.extend(self.items.remove(i).map(|l| l.extra));
        }
        self.items.push_back(Launch {
            name,
            pid,
            started,
            extra,
        });
        while self.items.len() > self.cap {
            gone.extend(self.items.pop_front().map(|l| l.extra));
        }
        gone
    }

    pub fn get(&self, pid: u32) -> Option<&Launch<H>> {
        self.items.iter().find(|l| l.pid == pid)
    }

    pub fn remove(&mut self, pid: u32) -> Option<Launch<H>> {
        let i = self.items.iter().position(|l| l.pid == pid)?;
        self.items.remove(i)
    }

    /// The last `n`, newest first.
    pub fn recent(&self, n: usize) -> Vec<&Launch<H>> {
        self.items.iter().rev().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_targets() {
        assert_eq!(classify("steam://rungameid/730"), Target::Url);
        assert_eq!(classify("https://example.com/a.exe"), Target::Url);
        assert_eq!(classify("ms-settings:display"), Target::Url);
        assert_eq!(classify(r"C:\Windows\notepad.exe"), Target::Exe);
        assert_eq!(classify("notepad"), Target::Shell);
        assert_eq!(classify("NOTEPAD.EXE"), Target::Exe);
        assert_eq!(classify("D:/Games/x.Com"), Target::Exe);
        assert_eq!(classify(r"C:\Users\a\Menu\App.lnk"), Target::Shell);
        assert_eq!(classify(r"C:\a b\doc.pdf"), Target::Shell);
        assert_eq!(classify(r"C:\dir.exe\file.txt"), Target::Shell);
        assert_eq!(classify(""), Target::Shell);
    }

    #[test]
    fn names() {
        assert_eq!(display_name(r"C:\x\Foo Bar.lnk"), "Foo Bar");
        assert_eq!(display_name("steam://rungameid/1"), "steam://rungameid/1");
        assert_eq!(display_name("notepad"), "notepad");
        assert_eq!(display_name("/a/.hidden"), ".hidden");
    }

    #[test]
    fn quoting_round_trips_the_documented_cases() {
        assert_eq!(quote_arg("abc"), "abc");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir\"), r"C:\dir\");
        assert_eq!(quote_arg(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(quote_arg(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(
            command_line(r"C:\Program Files\x.exe", &["-a".into(), "b c".into()]),
            r#""C:\Program Files\x.exe" -a "b c""#
        );
    }

    #[test]
    fn env_blocks() {
        let base = vec![
            ("Path".to_string(), r"C:\Windows".to_string()),
            ("=C:".to_string(), r"C:\".to_string()),
            ("zeta".to_string(), "1".to_string()),
        ];
        let over: BTreeMap<_, _> = [
            ("PATH".to_string(), "x".to_string()),
            ("Alpha".to_string(), "\u{e6}".to_string()),
            ("bad=name".to_string(), "v".to_string()),
            (String::new(), "v".to_string()),
        ]
        .into();
        let b = env_block(base, &over);
        assert_eq!(b.last(), Some(&0));
        assert_eq!(b[b.len() - 2], 0);
        let back = parse_env_block(&b);
        assert_eq!(
            back,
            [
                ("=C:".to_string(), r"C:\".to_string()),
                ("Alpha".to_string(), "\u{e6}".to_string()),
                ("PATH".to_string(), "x".to_string()),
                ("zeta".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(env_block(Vec::new(), &BTreeMap::new()), [0, 0]);
        assert!(parse_env_block(&[0, 0]).is_empty());
        assert!(parse_env_block(&[]).is_empty());
        // A value may hold '='.
        let w: Vec<u16> = "A=b=c\0\0".encode_utf16().collect();
        assert_eq!(parse_env_block(&w), [("A".to_string(), "b=c".to_string())]);
    }

    #[test]
    fn process_trees() {
        let procs = [(2, 1), (3, 2), (4, 2), (5, 3), (6, 99), (7, 7), (1, 5)];
        let mut d = descendants(2, &procs);
        d.sort();
        // 1 is a "child" of 5 here: a cycle must not loop.
        assert_eq!(d, [1, 2, 3, 4, 5]);
        assert_eq!(descendants(42, &procs), [42]);
        assert_eq!(descendants(7, &procs), [7]);
    }

    #[test]
    fn launch_table() {
        let mut t: LaunchTable<u32> = LaunchTable::new(3);
        assert!(t.record("a".into(), 1, 10, 100).is_empty());
        t.record("b".into(), 2, 11, 200);
        t.record("c".into(), 3, 12, 300);
        assert_eq!(t.record("d".into(), 4, 13, 400), [100]);
        assert!(t.get(1).is_none());
        assert_eq!(t.get(2).unwrap().name, "b");
        let names: Vec<_> = t.recent(2).iter().map(|l| l.name.clone()).collect();
        assert_eq!(names, ["d", "c"]);
        // The same pid again replaces the old entry.
        assert_eq!(t.record("c2".into(), 3, 14, 301), [300]);
        assert_eq!(t.recent(8).len(), 3);
        assert_eq!(t.remove(3).unwrap().extra, 301);
        assert!(t.remove(3).is_none());
    }
}

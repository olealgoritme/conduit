//! The platform-independent half of the Conduit GPU tray: reading the host's
//! line feed, the 60-second history, colours and texts. The Windows shell
//! (tray icon, popup, channel I/O) is in `main.rs` and its modules.

use conduit_stats::Reading;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub use conduit_stats::CHANNEL;

/// Samples kept per graph: one a second, a minute.
pub const HISTORY: usize = 60;

/// No line for this long: the host feed is gone (VM migrated, helper stopped).
pub const STALE: Duration = Duration::from_secs(5);

/// Longest line accepted; anything longer is dropped, not buffered forever.
const MAX_LINE: usize = 64 * 1024;

/// Splits the channel's byte stream into readings.
#[derive(Default)]
pub struct LineReader {
    buf: Vec<u8>,
    skipping: bool,
}

impl LineReader {
    /// Feed bytes; get every complete, valid reading in them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Reading> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if !self.skipping {
                    if let Some(r) = std::str::from_utf8(&self.buf).ok().and_then(Reading::parse) {
                        out.push(r);
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
}

/// A fixed-length ring of the last [`HISTORY`] values.
#[derive(Clone, Debug, Default)]
pub struct Series(VecDeque<f32>);

impl Series {
    pub fn push(&mut self, v: f32) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(v);
    }
    pub fn values(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.0.iter().copied()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What the tray knows: the latest reading and the graphs.
#[derive(Default)]
pub struct Model {
    pub latest: Option<Reading>,
    pub received: Option<Instant>,
    pub load: Series,
    pub power_w: Series,
    pub temp_c: Series,
    pub vram_pct: Series,
}

impl Model {
    pub fn ingest(&mut self, r: Reading, now: Instant) {
        self.load.push(r.util_gpu.unwrap_or(0) as f32);
        self.power_w.push(r.power_mw.unwrap_or(0) as f32 / 1000.0);
        self.temp_c.push(r.temp_c.unwrap_or(0) as f32);
        self.vram_pct.push(vram_fraction(&r).unwrap_or(0.0) * 100.0);
        self.latest = Some(r);
        self.received = Some(now);
    }

    /// The latest reading, unless the feed went quiet.
    pub fn live(&self, now: Instant) -> Option<&Reading> {
        match (&self.latest, self.received) {
            (Some(r), Some(t)) if now.saturating_duration_since(t) <= STALE => Some(r),
            _ => None,
        }
    }
}

pub fn vram_fraction(r: &Reading) -> Option<f32> {
    match (r.vram_used, r.vram_total) {
        (Some(u), Some(t)) if t > 0 => Some((u as f64 / t as f64).clamp(0.0, 1.0) as f32),
        _ => None,
    }
}

/// Green (cool/idle) through amber to red, for `v` between `lo` and `hi`.
pub fn heat(v: f32, lo: f32, hi: f32) -> (u8, u8, u8) {
    const STOPS: [(f32, [f32; 3]); 3] = [
        (0.0, [118.0, 185.0, 0.0]),   // NVIDIA green
        (0.55, [255.0, 176.0, 32.0]), // amber
        (1.0, [239.0, 68.0, 68.0]),   // red
    ];
    let t = if hi > lo {
        ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (a, b) = if t <= STOPS[1].0 {
        (STOPS[0], STOPS[1])
    } else {
        (STOPS[1], STOPS[2])
    };
    let k = (t - a.0) / (b.0 - a.0);
    let c = |i: usize| (a.1[i] + (b.1[i] - a.1[i]) * k).round() as u8;
    (c(0), c(1), c(2))
}

/// Temperature colour: green to 50 C, red from 90 C.
pub fn temp_color(c: u32) -> (u8, u8, u8) {
    heat(c as f32, 45.0, 90.0)
}

/// Load colour: green idle, red flat out.
pub fn load_color(pct: u32) -> (u8, u8, u8) {
    heat(pct as f32, 0.0, 100.0)
}

/// What the tray icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconMetric {
    Temp,
    Load,
}

/// The number and colour on the tray icon; `None` while there is no reading.
pub fn icon_face(r: &Reading, m: IconMetric) -> Option<(String, (u8, u8, u8))> {
    match m {
        IconMetric::Temp => r.temp_c.map(|t| (t.to_string(), temp_color(t))),
        IconMetric::Load => r.util_gpu.map(|u| (u.to_string(), load_color(u))),
    }
}

pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

/// The tray tooltip (Windows keeps 127 characters).
pub fn tooltip(r: Option<&Reading>) -> String {
    let Some(r) = r else {
        return "Conduit GPU: waiting for the host".into();
    };
    let mut parts = Vec::new();
    if let Some(t) = r.temp_c {
        parts.push(format!("{t}\u{b0}C"));
    }
    if let Some(u) = r.util_gpu {
        parts.push(format!("{u}% load"));
    }
    if let Some(p) = r.power_mw {
        parts.push(format!("{:.0} W", p as f64 / 1000.0));
    }
    if let (Some(u), Some(t)) = (r.vram_used, r.vram_total) {
        parts.push(format!("{:.1}/{:.0} GiB", gib(u), gib(t)));
    }
    let s = format!("{}\n{}", r.gpu, parts.join("  "));
    s.chars().take(127).collect()
}

/// A host shared folder mounted in this VM as a drive (`conduit-NAME` tag).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub name: String,
    /// "Z:"
    pub drive: String,
}

impl Share {
    /// The folder's root, "Z:\\".
    pub fn root(&self) -> String {
        format!("{}\\", self.drive)
    }
}

/// The name Conduit gives the folder a VM always has.
pub const DEFAULT_SHARE: &str = "Conduit";

/// One entry of the mount script's drive map (tag -> "Z:").
pub fn parse_share(tag: &str, drive: &str) -> Option<Share> {
    let name = tag.strip_prefix("conduit-").filter(|n| !n.is_empty())?;
    let mut c = drive.trim().chars();
    let letter = c.next().filter(char::is_ascii_alphabetic)?;
    (c.as_str() == ":" || c.as_str().is_empty()).then(|| Share {
        name: name.to_string(),
        drive: format!("{}:", letter.to_ascii_uppercase()),
    })
}

/// The share "Open default share" and drops go to: `Conduit`, else the first.
pub fn default_share(shares: &[Share]) -> Option<&Share> {
    shares
        .iter()
        .find(|s| s.name == DEFAULT_SHARE)
        .or_else(|| shares.first())
}

/// A double-null-terminated path list, as SHFileOperationW takes it.
pub fn path_list(paths: &[String]) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::new();
    for p in paths {
        v.extend(p.encode_utf16());
        v.push(0);
    }
    v.push(0);
    v
}

/// "Copied 3 items to Conduit".
pub fn copied_text(n: usize, share: &str) -> String {
    format!(
        "Copied {n} item{} to {share}",
        if n == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(temp: u32, util: u32) -> String {
        let r = Reading {
            v: 1,
            gpu: "RTX".into(),
            temp_c: Some(temp),
            util_gpu: Some(util),
            vram_used: Some(1 << 30),
            vram_total: Some(4 << 30),
            power_mw: Some(150_000),
            ..Reading::default()
        };
        r.to_line()
    }

    #[test]
    fn lines_split_across_reads() {
        let l = line(50, 10);
        let (a, b) = l.as_bytes().split_at(17);
        let mut lr = LineReader::default();
        assert!(lr.push(a).is_empty());
        let got = lr.push(b);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].temp_c, Some(50));
    }

    #[test]
    fn two_lines_and_garbage_in_one_read() {
        let s = format!("{}junk\n{}", line(40, 1), line(41, 2));
        let got = LineReader::default().push(s.as_bytes());
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].util_gpu, Some(2));
    }

    #[test]
    fn overlong_line_is_dropped_then_recovers() {
        let mut lr = LineReader::default();
        let mut big = vec![b'x'; MAX_LINE + 10];
        big.push(b'\n');
        assert!(lr.push(&big).is_empty());
        assert_eq!(lr.push(line(1, 1).as_bytes()).len(), 1);
    }

    #[test]
    fn history_keeps_a_minute() {
        let mut m = Model::default();
        let now = Instant::now();
        for i in 0..100 {
            m.ingest(Reading::parse(&line(i, i)).unwrap(), now);
        }
        assert_eq!(m.temp_c.len(), HISTORY);
        assert_eq!(m.temp_c.values().last(), Some(99.0));
        assert_eq!(m.temp_c.values().next(), Some(40.0));
        assert_eq!(m.vram_pct.values().last(), Some(25.0));
        assert_eq!(m.power_w.values().last(), Some(150.0));
    }

    #[test]
    fn goes_stale() {
        let mut m = Model::default();
        let t0 = Instant::now();
        assert!(m.live(t0).is_none());
        m.ingest(Reading::parse(&line(1, 1)).unwrap(), t0);
        assert!(m.live(t0 + Duration::from_secs(2)).is_some());
        assert!(m.live(t0 + Duration::from_secs(6)).is_none());
    }

    #[test]
    fn heat_runs_green_amber_red() {
        assert_eq!(heat(0.0, 0.0, 100.0), (118, 185, 0));
        assert_eq!(heat(55.0, 0.0, 100.0), (255, 176, 32));
        assert_eq!(heat(100.0, 0.0, 100.0), (239, 68, 68));
        assert_eq!(heat(500.0, 0.0, 100.0), (239, 68, 68));
        assert_eq!(heat(5.0, 10.0, 10.0), (118, 185, 0));
    }

    #[test]
    fn icon_and_tooltip() {
        let r = Reading::parse(&line(61, 33)).unwrap();
        assert_eq!(icon_face(&r, IconMetric::Temp).unwrap().0, "61");
        assert_eq!(icon_face(&r, IconMetric::Load).unwrap().0, "33");
        let t = tooltip(Some(&r));
        assert!(t.contains("61\u{b0}C") && t.contains("150 W") && t.contains("1.0/4 GiB"));
        assert!(tooltip(None).contains("waiting"));
        let empty = Reading {
            v: 1,
            ..Reading::default()
        };
        assert!(icon_face(&empty, IconMetric::Temp).is_none());
    }

    #[test]
    fn shares_from_the_drive_map() {
        let s = parse_share("conduit-Docs", "z:").unwrap();
        assert_eq!((s.name.as_str(), s.drive.as_str()), ("Docs", "Z:"));
        assert_eq!(s.root(), "Z:\\");
        assert!(parse_share("nvidia", "Z:").is_none());
        assert!(parse_share("conduit-", "Z:").is_none());
        assert!(parse_share("conduit-A", "").is_none());
        assert!(parse_share("conduit-A", "ZZ:").is_none());
    }

    #[test]
    fn default_share_prefers_conduit() {
        let a = parse_share("conduit-Games", "Y:").unwrap();
        let b = parse_share("conduit-Conduit", "Z:").unwrap();
        assert_eq!(default_share(&[a.clone(), b.clone()]), Some(&b));
        assert_eq!(default_share(std::slice::from_ref(&a)), Some(&a));
        assert_eq!(default_share(&[]), None);
    }

    #[test]
    fn drop_list_and_text() {
        let v = path_list(&["C:\\a".into(), "C:\\b c".into()]);
        assert_eq!(&v[v.len() - 2..], &[0, 0]);
        assert_eq!(v.iter().filter(|&&c| c == 0).count(), 3);
        assert_eq!(copied_text(1, "Conduit"), "Copied 1 item to Conduit");
        assert_eq!(copied_text(3, "Docs"), "Copied 3 items to Docs");
    }
}

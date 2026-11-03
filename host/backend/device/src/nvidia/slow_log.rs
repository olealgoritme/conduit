//! Host driver calls that take long: one warn line each, rate limited.
//!
//! Every ioctl the backend issues on a guest's behalf goes through
//! [`super::host::RealHost`]. A call that takes [`DEFAULT_THRESHOLD_MS`] or more
//! (`CONDUIT_RM_SLOW_MS` overrides it, 0 turns the log off) is logged at warn level with
//! what it was: the RM escape, the handles and class or control command read from its
//! parameter block, how long it took, RM's status and the errno. The guest sees such a
//! call only as one long round trip (the KMD's `PgPhRtMx`); this names it.
//!
//! At most [`LINES_PER_WINDOW`] lines per [`WINDOW_MS`]; a window that dropped lines
//! says how many when the next line is written.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Calls this long or longer are logged.
pub const DEFAULT_THRESHOLD_MS: u64 = 50;
/// The rate limit: lines per window.
pub const LINES_PER_WINDOW: u32 = 20;
pub const WINDOW_MS: u64 = 10_000;

const NV_IOCTL_MAGIC: u64 = b'F' as u64;
const NV_ESC_IOCTL_XFER_CMD: u32 = 0xd3;

/// The threshold in force: `CONDUIT_RM_SLOW_MS`, else [`DEFAULT_THRESHOLD_MS`]; `None`: off.
pub fn threshold() -> Option<Duration> {
    static MS: OnceLock<u64> = OnceLock::new();
    let ms = *MS.get_or_init(|| {
        std::env::var("CONDUIT_RM_SLOW_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_THRESHOLD_MS)
    });
    (ms != 0).then(|| Duration::from_millis(ms))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// `(type, nr)` of an ioctl request number.
const fn split(request: u64) -> (u64, u32) {
    ((request >> 8) & 0xff, (request & 0xff) as u32)
}

fn hex(b: &[u8], at: usize) -> String {
    match u32_at(b, at) {
        Some(v) => format!("{v:#x}"),
        None => "?".into(),
    }
}

/// What the call was, from its request number and parameter block (after the call, so
/// RM's status is in it).
pub fn describe(request: u64, arg: &[u8]) -> String {
    let (ty, nr) = split(request);
    if ty != NV_IOCTL_MAGIC {
        return format!("ioctl {request:#x}");
    }
    let h = |at: usize| hex(arg, at);
    match nr {
        0x2a => format!(
            "RM_CONTROL client={} object={} cmd={} -> status {}",
            h(0),
            h(4),
            h(8),
            h(28)
        ),
        0x2b => {
            // NVOS64 (48 bytes) has the status at 40; the older NVOS21 (32) at 24.
            let status = if arg.len() >= 44 { h(40) } else { h(24) };
            format!(
                "RM_ALLOC client={} parent={} new={} class={} -> status {status}",
                h(0),
                h(4),
                h(8),
                h(12)
            )
        }
        0x29 => format!(
            "RM_FREE client={} parent={} object={} -> status {}",
            h(0),
            h(4),
            h(8),
            h(12)
        ),
        0x27 => format!(
            "RM_ALLOC_MEMORY client={} parent={} new={} class={} flags={} limit={:#x} -> status {}",
            h(0),
            h(4),
            h(8),
            h(12),
            h(16),
            u64_at(arg, 32).unwrap_or(0),
            h(40)
        ),
        0x4e => format!(
            "RM_MAP_MEMORY client={} device={} memory={} offset={:#x} length={:#x} -> status {}",
            h(0),
            h(4),
            h(8),
            u64_at(arg, 16).unwrap_or(0),
            u64_at(arg, 24).unwrap_or(0),
            h(40)
        ),
        0x4f => format!(
            "RM_UNMAP_MEMORY client={} device={} memory={} -> status {}",
            h(0),
            h(4),
            h(8),
            h(24)
        ),
        0x57 => format!(
            "RM_MAP_MEMORY_DMA client={} device={} dma={} memory={} offset={:#x} length={:#x} flags={} dmaOffset={:#x} -> status {}",
            h(0),
            h(4),
            h(8),
            h(12),
            u64_at(arg, 16).unwrap_or(0),
            u64_at(arg, 24).unwrap_or(0),
            h(32),
            u64_at(arg, 48).unwrap_or(0),
            h(56)
        ),
        0x58 => format!(
            "RM_UNMAP_MEMORY_DMA client={} device={} dma={} memory={} flags={} dmaOffset={:#x} -> status {}",
            h(0),
            h(4),
            h(8),
            h(12),
            h(16),
            u64_at(arg, 24).unwrap_or(0),
            h(40)
        ),
        0x34 => format!(
            "RM_DUP_OBJECT client={} parent={} new={} srcClient={} srcObject={} -> status {}",
            h(0),
            h(4),
            h(8),
            h(12),
            h(16),
            h(24)
        ),
        0x41 => format!(
            "RM_IDLE_CHANNELS client={} device={} channel={} count={} flags={} timeout={} -> status {}",
            h(0),
            h(4),
            h(8),
            h(12),
            h(40),
            h(44),
            h(48)
        ),
        NV_ESC_IOCTL_XFER_CMD => format!(
            "RM escape {:#x} (XFER, {} bytes)",
            u32_at(arg, 0).unwrap_or(0),
            u32_at(arg, 4).unwrap_or(0)
        ),
        _ => format!("RM escape {nr:#x} ({} bytes)", arg.len()),
    }
}

/// The rate limiter: [`LINES_PER_WINDOW`] lines per [`WINDOW_MS`].
pub struct Limiter {
    window_start_ms: AtomicU64,
    lines: AtomicU32,
    dropped: AtomicU32,
}

impl Limiter {
    pub const fn new() -> Self {
        Self {
            window_start_ms: AtomicU64::new(0),
            lines: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        }
    }

    /// Whether a line may be written at `now_ms`, and how many were dropped since the
    /// last one written (0 unless this is the first line of a new window).
    pub fn admit(&self, now_ms: u64) -> Option<u32> {
        let start = self.window_start_ms.load(Ordering::Relaxed);
        if start == 0 || now_ms.saturating_sub(start) >= WINDOW_MS {
            // A new window (a race may start it twice; a line more or less is fine).
            self.window_start_ms.store(now_ms.max(1), Ordering::Relaxed);
            self.lines.store(1, Ordering::Relaxed);
            return Some(self.dropped.swap(0, Ordering::Relaxed));
        }
        if self.lines.fetch_add(1, Ordering::Relaxed) < LINES_PER_WINDOW {
            Some(0)
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

static LIMITER: Limiter = Limiter::new();

fn now_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

/// After a host call on `fd`: log it when it took `elapsed` >= the threshold.
pub fn note(fd: i32, request: u64, arg: &[u8], elapsed: Duration, result: Result<(), i32>) {
    let Some(limit) = threshold() else {
        return;
    };
    if elapsed < limit {
        return;
    }
    let Some(dropped) = LIMITER.admit(now_ms()) else {
        return;
    };
    let errno = match result {
        Ok(()) => 0,
        Err(e) => e,
    };
    let more = if dropped != 0 {
        format!(" ({dropped} slow calls not logged before this one)")
    } else {
        String::new()
    };
    log::warn!(
        "RM slow call: {} took {} ms (fd {fd}, errno {errno}){more}",
        describe(request, arg),
        elapsed.as_millis()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn iowr(nr: u64, size: u64) -> u64 {
        (3 << 30) | (size << 16) | (NV_IOCTL_MAGIC << 8) | nr
    }

    fn block(len: usize, fields: &[(usize, u32)]) -> Vec<u8> {
        let mut b = vec![0u8; len];
        for &(at, v) in fields {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn a_control_names_its_command_handles_and_status() {
        let b = block(
            32,
            &[
                (0, 0xc1d0_0001),
                (4, 0x4e57_0002),
                (8, 0x2080_0170),
                (28, 0x1f),
            ],
        );
        let s = describe(iowr(0x2a, 32), &b);
        assert_eq!(
            s,
            "RM_CONTROL client=0xc1d00001 object=0x4e570002 cmd=0x20800170 -> status 0x1f"
        );
    }

    #[test]
    fn an_alloc_names_its_class_and_reads_the_nvos64_status() {
        let b = block(48, &[(0, 1), (4, 2), (8, 3), (12, 0xcab5), (40, 0x51)]);
        assert_eq!(
            describe(iowr(0x2b, 48), &b),
            "RM_ALLOC client=0x1 parent=0x2 new=0x3 class=0xcab5 -> status 0x51"
        );
        let old = block(32, &[(12, 0x90f1), (24, 7)]);
        assert!(describe(iowr(0x2b, 32), &old).ends_with("class=0x90f1 -> status 0x7"));
    }

    #[test]
    fn a_dma_map_names_its_offsets() {
        let mut b = block(64, &[(8, 0x50a0), (12, 0x71), (32, 0x110), (56, 0)]);
        b[24..32].copy_from_slice(&0x10_0000u64.to_le_bytes());
        b[48..56].copy_from_slice(&0x40_0000_0000u64.to_le_bytes());
        let s = describe(iowr(0x57, 64), &b);
        assert!(s.starts_with("RM_MAP_MEMORY_DMA"), "{s}");
        assert!(s.contains("length=0x100000"), "{s}");
        assert!(s.contains("dmaOffset=0x4000000000"), "{s}");
        assert!(s.ends_with("-> status 0x0"), "{s}");
    }

    #[test]
    fn a_short_block_is_described_without_panicking() {
        for nr in [
            0x27u64, 0x29, 0x2a, 0x2b, 0x34, 0x41, 0x4e, 0x4f, 0x57, 0x58, 0xd3, 0x99,
        ] {
            let s = describe(iowr(nr, 4), &[1, 2, 3]);
            assert!(!s.is_empty());
        }
        assert_eq!(describe(0x1234, &[]), "ioctl 0x1234");
    }

    #[test]
    fn the_limiter_admits_a_window_then_counts_the_dropped() {
        let l = Limiter::new();
        assert_eq!(l.admit(100), Some(0));
        for _ in 1..LINES_PER_WINDOW {
            assert_eq!(l.admit(200), Some(0));
        }
        assert_eq!(l.admit(300), None);
        assert_eq!(l.admit(400), None);
        // The next window reports the two lines it dropped.
        assert_eq!(l.admit(100 + WINDOW_MS), Some(2));
        assert_eq!(l.admit(100 + WINDOW_MS + 1), Some(0));
    }
}

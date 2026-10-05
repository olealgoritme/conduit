//! Fence latency summaries (docs/VENUS.md "Fence latency"), for both sides:
//! the backend keys its samples by `(ctx_id, ring)`, `conduit-venus` has one
//! timeline. Each summary gives, per key, the count, median, 90th percentile
//! and maximum, and the span of the window it covers.
//!
//! A window opens with its first sample and closes [`PERIOD`] later, at the
//! first [`Window::add`] or [`Window::flush`] after that. Callers flush from
//! a loop they already run, so the line comes on time while fences flow; a
//! sample after an idle spell opens a new window instead of joining the old
//! one. The span runs from the window's first sample to its last, so a
//! burst that stopped early says so. No samples, no window, no line: an idle
//! guest logs nothing.

use std::time::{Duration, Instant};

/// How long a window gathers samples.
pub const PERIOD: Duration = Duration::from_secs(2);

/// One key's latencies over a window, in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub count: usize,
    pub p50: u32,
    pub p90: u32,
    pub max: u32,
}

/// A closed window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary<K> {
    /// From the window's first sample to its last.
    pub span: Duration,
    /// Sorted by key.
    pub by_key: Vec<(K, Stats)>,
}

pub struct Window<K> {
    period: Duration,
    /// The open window's first and latest sample.
    open: Option<(Instant, Instant)>,
    samples: Vec<(K, u32)>,
}

impl<K> Window<K> {
    pub const fn new(period: Duration) -> Self {
        Self { period, open: None, samples: Vec::new() }
    }
}

impl<K> Default for Window<K> {
    fn default() -> Self {
        Self::new(PERIOD)
    }
}

impl<K: Ord + Copy> Window<K> {
    /// Record one latency seen at `now`. Returns the window that ended
    /// before it, if one did; the sample then opens the next.
    pub fn add(&mut self, key: K, latency: Duration, now: Instant) -> Option<Summary<K>> {
        let ended = self.flush(now);
        let first = self.open.map_or(now, |(first, _)| first);
        self.open = Some((first, now));
        self.samples.push((key, latency.as_micros().min(u32::MAX as u128) as u32));
        ended
    }

    /// The open window, if its period is over at `now`. None while idle.
    pub fn flush(&mut self, now: Instant) -> Option<Summary<K>> {
        let (first, last) = self.open?;
        if now.saturating_duration_since(first) < self.period {
            return None;
        }
        self.open = None;
        let mut samples = std::mem::take(&mut self.samples);
        samples.sort_unstable();
        let by_key = samples
            .chunk_by(|a, b| a.0 == b.0)
            .map(|run| {
                // Sorted by key, then latency.
                let at = |q: usize| run[(run.len() - 1) * q / 100].1;
                (run[0].0, Stats { count: run.len(), p50: at(50), p90: at(90), max: run[run.len() - 1].1 })
            })
            .collect();
        Some(Summary { span: last.saturating_duration_since(first), by_key })
    }

    /// How long until the open window closes; None while none is open.
    pub fn due(&self, now: Instant) -> Option<Duration> {
        self.open.map(|(first, _)| (first + self.period).saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn closes_on_flush_once_the_period_is_over() {
        let t0 = Instant::now();
        let mut w = Window::new(PERIOD);
        assert_eq!(w.flush(t0 + 10 * PERIOD), None, "idle: nothing");
        assert_eq!(w.due(t0), None);
        for (i, us) in [300, 100, 200, 400, 1000].into_iter().enumerate() {
            let at = t0 + i as u32 * 100 * MS;
            assert_eq!(w.add((1, 0), Duration::from_micros(us), at), None);
        }
        assert_eq!(w.due(t0 + 500 * MS), Some(1500 * MS));
        assert_eq!(w.flush(t0 + 1999 * MS), None, "still open");
        let s = w.flush(t0 + PERIOD).expect("closed on time, with no new fence");
        assert_eq!(s.span, 400 * MS, "first to last sample");
        assert_eq!(s.by_key, vec![((1, 0), Stats { count: 5, p50: 300, p90: 400, max: 1000 })]);
        assert_eq!(w.flush(t0 + 3 * PERIOD), None, "reported once");
        assert_eq!(w.due(t0 + 3 * PERIOD), None);
    }

    #[test]
    fn a_fence_after_an_idle_spell_opens_a_new_window() {
        let t0 = Instant::now();
        let mut w = Window::new(PERIOD);
        w.add((), 50 * MS, t0);
        w.add((), 70 * MS, t0 + 300 * MS);
        // No flush in between: the next fence comes a minute later.
        let late = t0 + Duration::from_secs(60);
        let s = w.add((), 9 * MS, late).expect("the old window ends first");
        assert_eq!(s.span, 300 * MS);
        assert_eq!(s.by_key, vec![((), Stats { count: 2, p50: 50_000, p90: 50_000, max: 70_000 })]);
        assert_eq!(w.due(late), Some(PERIOD), "the late fence opened a window of its own");
        let s = w.flush(late + PERIOD).unwrap();
        assert_eq!((s.span, s.by_key[0].1.count, s.by_key[0].1.max), (Duration::ZERO, 1, 9000));
    }

    #[test]
    fn keys_are_summarized_apart() {
        let t0 = Instant::now();
        let mut w = Window::default();
        for i in 0..10u32 {
            w.add((2, 1), Duration::from_micros(u64::from(i)), t0);
            w.add((1, 0), Duration::from_micros(100), t0);
        }
        let s = w.flush(t0 + PERIOD).unwrap();
        assert_eq!(s.by_key.len(), 2);
        assert_eq!(s.by_key[0], ((1, 0), Stats { count: 10, p50: 100, p90: 100, max: 100 }));
        assert_eq!(s.by_key[1], ((2, 1), Stats { count: 10, p50: 4, p90: 8, max: 9 }));
    }
}

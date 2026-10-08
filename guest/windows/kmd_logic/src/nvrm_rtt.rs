//! Round-trip statistics for forwarded NVRM calls: count, minimum, mean, maximum
//! and a coarse histogram, kept in atomics so the writer needs no lock.
//!
//! The question they answer is "what does one RM control cost from the guest",
//! and in particular whether MSI-X is faster than INTx per call (the INTx path
//! measured about 55 us; `docs/msi-interrupts.md`). A sample is the time the
//! calling thread spent from just before the request was queued to just after
//! the reply was in hand, on the interrupt-time clock (100 ns units): the
//! submit, the pre-wait spin, the interrupt, the DPC, the event signal and the
//! wake of the waiter. Only calls that got a reply are counted.
//!
//! Writers are the PASSIVE threads that make the calls: [`Stats::note`] is a
//! handful of relaxed atomic operations and takes no lock, so it is also legal at
//! any IRQL. The registry mirror reads a [`Snapshot`] at PASSIVE.
//!
//! Units: samples and the kept sum are in 100 ns; the published values are in
//! microseconds ([`Snapshot::mean_us`] rounds to nearest).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Histogram buckets: seven bounds and the open one above.
pub const BUCKETS: usize = 8;

/// Upper bounds (exclusive) of the first seven buckets, in microseconds. Chosen
/// around what a call costs: the INTx path (about 55 us) and the spin budget
/// (50 us) fall in the middle, a host that is busy lands above 250.
pub const BOUNDS_US: [u32; BUCKETS - 1] = [15, 25, 40, 60, 100, 250, 1000];

/// Samples of 100 ns per microsecond.
pub const TICKS_PER_US: u32 = 10;

/// Elapsed 100 ns from `at` to `now`, saturated to `u32` (about 7 minutes); 0 if
/// the clock went backwards.
pub const fn elapsed_ticks(at: u64, now: u64) -> u32 {
    let d = now.saturating_sub(at);
    if d > u32::MAX as u64 {
        u32::MAX
    } else {
        d as u32
    }
}

/// The histogram bucket of a sample of `ticks` (100 ns).
pub const fn bucket_of(ticks: u32) -> usize {
    let mut i = 0;
    while i < BUCKETS - 1 {
        // `bound * 10` cannot overflow: the largest bound is 1000.
        if ticks < BOUNDS_US[i] * TICKS_PER_US {
            return i;
        }
        i += 1;
    }
    BUCKETS - 1
}

/// One class of calls.
pub struct Stats {
    n: AtomicU32,
    sum: AtomicU64,
    min: AtomicU32,
    max: AtomicU32,
    buckets: [AtomicU32; BUCKETS],
}

impl Stats {
    pub const fn new() -> Stats {
        Stats {
            n: AtomicU32::new(0),
            sum: AtomicU64::new(0),
            min: AtomicU32::new(u32::MAX),
            max: AtomicU32::new(0),
            buckets: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
        }
    }

    /// Count one round trip of `ticks` (100 ns).
    pub fn note(&self, ticks: u32) {
        self.n.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(u64::from(ticks), Ordering::Relaxed);
        self.min.fetch_min(ticks, Ordering::Relaxed);
        self.max.fetch_max(ticks, Ordering::Relaxed);
        self.buckets[bucket_of(ticks)].fetch_add(1, Ordering::Relaxed);
    }

    /// Forget everything (a new transport generation, or a test).
    pub fn reset(&self) {
        self.n.store(0, Ordering::Relaxed);
        self.sum.store(0, Ordering::Relaxed);
        self.min.store(u32::MAX, Ordering::Relaxed);
        self.max.store(0, Ordering::Relaxed);
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
    }

    /// The counts now. Not an atomic cut across the fields: a sample noted
    /// during the read may be in some and not in others, which a mean over
    /// thousands of calls does not see.
    pub fn snapshot(&self) -> Snapshot {
        let mut buckets = [0u32; BUCKETS];
        for (out, b) in buckets.iter_mut().zip(&self.buckets) {
            *out = b.load(Ordering::Relaxed);
        }
        Snapshot {
            n: self.n.load(Ordering::Relaxed),
            sum_ticks: self.sum.load(Ordering::Relaxed),
            min_ticks: self.min.load(Ordering::Relaxed),
            max_ticks: self.max.load(Ordering::Relaxed),
            buckets,
        }
    }
}

impl Default for Stats {
    fn default() -> Self {
        Stats::new()
    }
}

/// A read of [`Stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub n: u32,
    /// Sum of the samples, 100 ns.
    pub sum_ticks: u64,
    /// Smallest sample, 100 ns; `u32::MAX` when none.
    pub min_ticks: u32,
    pub max_ticks: u32,
    pub buckets: [u32; BUCKETS],
}

impl Snapshot {
    /// Microseconds, rounded to nearest, of a count of 100 ns ticks.
    const fn us(ticks: u64) -> u32 {
        let us = (ticks + (TICKS_PER_US as u64 / 2)) / TICKS_PER_US as u64;
        if us > u32::MAX as u64 {
            u32::MAX
        } else {
            us as u32
        }
    }

    /// Mean round trip in microseconds; 0 with no samples.
    pub const fn mean_us(&self) -> u32 {
        if self.n == 0 {
            return 0;
        }
        Self::us(self.sum_ticks / self.n as u64)
    }

    /// Smallest round trip in microseconds; 0 with no samples.
    pub const fn min_us(&self) -> u32 {
        if self.n == 0 || self.min_ticks == u32::MAX {
            return 0;
        }
        Self::us(self.min_ticks as u64)
    }

    /// Largest round trip in microseconds; 0 with no samples.
    pub const fn max_us(&self) -> u32 {
        Self::us(self.max_ticks as u64)
    }
}

/// Service-key value names of the round-trip counters, written by
/// `kmd_render/src/virtio/nvrm.rs` only. `NvRtt*` is the `Ioctl` class (the RM
/// control calls), `NvRttO*` every other forwarded message (open, close, scan-out
/// flip, the pinned registration, listings). At most 14 characters each.
pub const COUNTERS: [&str; 16] = [
    "NvRttN",
    "NvRttMinUs",
    "NvRttMeanUs",
    "NvRttMaxUs",
    "NvRttB0",
    "NvRttB1",
    "NvRttB2",
    "NvRttB3",
    "NvRttB4",
    "NvRttB5",
    "NvRttB6",
    "NvRttB7",
    "NvRttON",
    "NvRttOMinUs",
    "NvRttOMeanUs",
    "NvRttOMaxUs",
];

/// The `NvRttB<i>` name of bucket `i` (the `NvRttOB` classes do not exist).
pub const fn bucket_name(i: usize) -> [u8; 7] {
    [b'N', b'v', b'R', b't', b't', b'B', b'0' + (i % 10) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::string::String;
    use std::vec::Vec;

    #[test]
    fn no_samples_reads_as_zero() {
        let s = Stats::new().snapshot();
        assert_eq!(s.n, 0);
        assert_eq!(s.mean_us(), 0);
        assert_eq!(s.min_us(), 0);
        assert_eq!(s.max_us(), 0);
        assert_eq!(s.buckets, [0; BUCKETS]);
    }

    #[test]
    fn min_mean_max_of_a_few_calls() {
        let st = Stats::new();
        // 40 us, 60 us, 80 us, in 100 ns.
        for t in [400, 600, 800] {
            st.note(t);
        }
        let s = st.snapshot();
        assert_eq!(s.n, 3);
        assert_eq!(s.min_us(), 40);
        assert_eq!(s.mean_us(), 60);
        assert_eq!(s.max_us(), 80);
    }

    #[test]
    fn one_sample_is_its_own_min_mean_max() {
        let st = Stats::new();
        st.note(555);
        let s = st.snapshot();
        assert_eq!((s.min_us(), s.mean_us(), s.max_us()), (56, 56, 56));
    }

    #[test]
    fn mean_rounds_to_nearest_microsecond() {
        let st = Stats::new();
        st.note(104); // 10.4 us
        assert_eq!(st.snapshot().mean_us(), 10);
        let st = Stats::new();
        st.note(105); // 10.5 us
        assert_eq!(st.snapshot().mean_us(), 11);
    }

    #[test]
    fn sum_does_not_wrap_where_a_u32_would() {
        // 3000 calls of 2 s each: 6000 s in 100 ns is 6e10 > u32::MAX.
        let st = Stats::new();
        for _ in 0..3000 {
            st.note(20_000_000);
        }
        let s = st.snapshot();
        assert!(s.sum_ticks > u64::from(u32::MAX));
        assert_eq!(s.mean_us(), 2_000_000);
    }

    #[test]
    fn bucket_edges() {
        // Bounds are exclusive upper edges, in us.
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(149), 0);
        assert_eq!(bucket_of(150), 1);
        assert_eq!(bucket_of(249), 1);
        assert_eq!(bucket_of(250), 2);
        assert_eq!(bucket_of(599), 3);
        assert_eq!(bucket_of(600), 4);
        assert_eq!(bucket_of(999), 4);
        assert_eq!(bucket_of(1_000), 5);
        assert_eq!(bucket_of(2_499), 5);
        assert_eq!(bucket_of(2_500), 6);
        assert_eq!(bucket_of(9_999), 6);
        assert_eq!(bucket_of(10_000), 7);
        assert_eq!(bucket_of(u32::MAX), BUCKETS - 1);
        // Monotonic.
        let mut last = 0;
        for t in (0..20_000u32).step_by(7) {
            let b = bucket_of(t);
            assert!(b >= last);
            last = b;
        }
    }

    #[test]
    fn histogram_adds_up_to_the_count() {
        let st = Stats::new();
        for t in (0..50_000u32).step_by(13) {
            st.note(t);
        }
        let s = st.snapshot();
        assert_eq!(
            s.buckets.iter().map(|&b| u64::from(b)).sum::<u64>(),
            u64::from(s.n)
        );
    }

    #[test]
    fn an_intx_like_call_lands_beside_a_msix_like_one() {
        // 55 us (the measured INTx cost) and 25 us land in different buckets.
        assert_ne!(bucket_of(550), bucket_of(250));
        assert_eq!(bucket_of(550), 3); // 40..60 us
        assert_eq!(bucket_of(300), 2); // 25..40 us
    }

    #[test]
    fn elapsed_is_saturated_and_never_negative() {
        assert_eq!(elapsed_ticks(100, 150), 50);
        assert_eq!(elapsed_ticks(150, 100), 0);
        assert_eq!(elapsed_ticks(0, u64::MAX), u32::MAX);
        assert_eq!(elapsed_ticks(5, 5), 0);
    }

    #[test]
    fn reset_forgets_everything() {
        let st = Stats::new();
        st.note(123);
        st.reset();
        let s = st.snapshot();
        assert_eq!(s.n, 0);
        assert_eq!(s.min_us(), 0);
        assert_eq!(s.buckets, [0; BUCKETS]);
        st.note(900);
        assert_eq!(st.snapshot().min_us(), 90);
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        use std::sync::Arc;
        let st = Arc::new(Stats::new());
        let mut hs = Vec::new();
        for k in 0..4u32 {
            let st = Arc::clone(&st);
            hs.push(std::thread::spawn(move || {
                for i in 0..10_000u32 {
                    st.note(100 + k * 1000 + (i % 7));
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let s = st.snapshot();
        assert_eq!(s.n, 40_000);
        assert_eq!(s.buckets.iter().map(|&b| u64::from(b)).sum::<u64>(), 40_000);
        assert_eq!(s.min_ticks, 100);
        assert_eq!(s.max_ticks, 3_106);
    }

    #[test]
    fn counter_names_are_the_ones_the_helpers_build() {
        let mut built: Vec<String> = (0..BUCKETS)
            .map(|i| String::from_utf8(bucket_name(i).to_vec()).unwrap())
            .collect();
        built.sort();
        let mut listed: Vec<String> = COUNTERS
            .iter()
            .filter(|n| n.starts_with("NvRttB"))
            .map(|n| (*n).into())
            .collect();
        listed.sort();
        assert_eq!(built, listed);
        assert_eq!(COUNTERS.len(), 4 + BUCKETS + 4);
    }
}

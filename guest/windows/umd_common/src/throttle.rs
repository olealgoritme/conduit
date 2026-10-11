//! Occurrence counting for log sites whose budget is a call argument rather
//! than a constant baked into the static.
//!
//! Moved verbatim out of `umd/src/forward.rs` by the `umd_common` extraction
//! (`DECISIONS.md` D3b). The only change is visibility: the type and its
//! methods were `pub(super)` inside one crate and are `pub` here, because two
//! cdylibs now consume them. All accesses are `Ordering::Relaxed`.
//!
//! The budget stays a call argument deliberately: baking it into the static
//! would make every site's rate a property of this file instead of a property
//! of the site, which is what made the pre-R8xx logging impossible to reason
//! about.

use core::sync::atomic::{AtomicUsize, Ordering};

/// One cache line per counter. These counters are bumped on per-draw DDI
/// paths (DrawIndexed, IASetVertexBuffers, ...); packed next to counters that
/// other threads bump, every `fetch_add` was a cross-core cache-line transfer
/// (~1 us per UpdateSubresource in a Heaven profile on 339.4, the single
/// hottest instruction of the UMD).
#[repr(align(64))]
pub struct LogThrottle {
    count: AtomicUsize,
}

impl LogThrottle {
    pub const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    /// Bump and return the occurrence ordinal with no rate decision, for sites
    /// whose gate carries an extra escape clause (`|| alloc != 0`) or a shape
    /// of its own.
    pub fn next(&self) -> usize {
        self.count.fetch_add(1, Ordering::Relaxed)
    }

    /// Read the ordinal WITHOUT bumping it — one site logs a "pre" line under
    /// the same budget as the "post" line that follows it.
    pub fn peek(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The first `first` occurrences.
    ///
    /// Once the budget is spent this is a plain load: these sites sit on
    /// per-draw and per-map DDI paths, and a locked add per call for the
    /// life of the process only counted lines nobody would print. The count
    /// therefore stops growing at the largest budget its callers pass (only
    /// [`Self::next`] keeps counting).
    pub fn first_n(&self, first: usize) -> Option<usize> {
        if self.count.load(Ordering::Relaxed) >= first {
            return None;
        }
        let n = self.next();
        (n < first).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from zero.
    pub fn first_n_then_every(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || n % every == 0).then_some(n)
    }

    /// The first `first`, then every `every`-th counting from one. Distinct
    /// from [`Self::first_n_then_every`]: it fires at n = every-1, 2*every-1,
    /// not at n = 0, every, 2*every.
    pub fn first_n_then_every_from_one(&self, first: usize, every: usize) -> Option<usize> {
        let n = self.next();
        (n < first || (n + 1) % every == 0).then_some(n)
    }
}

impl Default for LogThrottle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::LogThrottle;

    #[test]
    fn first_n_hands_out_each_ordinal_once_then_stops_counting() {
        let t = LogThrottle::new();
        let got: Vec<_> = (0..10).filter_map(|_| t.first_n(3)).collect();
        assert_eq!(got, [0, 1, 2]);
        // Spent: no further bumps.
        assert_eq!(t.peek(), 3);
        // A larger budget at another site of the same counter still gets its share.
        assert_eq!(t.first_n(5), Some(3));
        assert_eq!(t.first_n(5), Some(4));
        assert_eq!(t.first_n(5), None);
        assert_eq!(t.peek(), 5);
        // `next` keeps counting.
        assert_eq!(t.next(), 5);
    }

    #[test]
    fn the_every_nth_forms_still_count_every_call() {
        let t = LogThrottle::new();
        let got: Vec<_> = (0..10).filter_map(|_| t.first_n_then_every(2, 4)).collect();
        assert_eq!(got, [0, 1, 4, 8]);
        assert_eq!(t.peek(), 10);
    }
}

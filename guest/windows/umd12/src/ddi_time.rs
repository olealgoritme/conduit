//! Per-DDI CPU time (`Umd12DdiTimes`, default off).
//!
//! Every forwarding DDI starts with `ddi_time!("Name")`: when the knob is on,
//! its wall time and call count are added to a per-DDI static, registered in a
//! lock-free list on first use. The frame-time line then names the DDIs that
//! took the most time per frame (`top`), so the CPU cost of command-list
//! recording splits by DDI without a profiler. Off: one relaxed load per call.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::time::Instant;

pub(crate) struct DdiStat {
    name: &'static str,
    ns: AtomicU64,
    calls: AtomicU64,
    registered: AtomicBool,
    next: AtomicPtr<DdiStat>,
}

static HEAD: AtomicPtr<DdiStat> = AtomicPtr::new(core::ptr::null_mut());

pub(crate) struct DdiTimer {
    stat: Option<(&'static DdiStat, Instant)>,
}

impl Drop for DdiTimer {
    fn drop(&mut self) {
        if let Some((stat, started)) = self.stat {
            let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            stat.ns.fetch_add(ns, Ordering::Relaxed);
            stat.calls.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl DdiStat {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            name,
            ns: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            registered: AtomicBool::new(false),
            next: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    #[inline]
    pub(crate) fn start(&'static self) -> DdiTimer {
        if !crate::knobs12::umd12_ddi_times() {
            return DdiTimer { stat: None };
        }
        if !self.registered.load(Ordering::Acquire) && !self.registered.swap(true, Ordering::AcqRel) {
            let me = self as *const DdiStat as *mut DdiStat;
            let mut head = HEAD.load(Ordering::Acquire);
            loop {
                self.next.store(head, Ordering::Relaxed);
                match HEAD.compare_exchange_weak(head, me, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(h) => head = h,
                }
            }
        }
        DdiTimer { stat: Some((self, Instant::now())) }
    }
}

/// The `n` DDIs with the most time since the last call, as (name, ns, calls),
/// most first; every DDI's window is reset.
pub(crate) fn take_top(n: usize) -> Vec<(&'static str, u64, u64)> {
    let mut all = Vec::new();
    let mut p = HEAD.load(Ordering::Acquire);
    while !p.is_null() {
        // SAFETY: only `'static` DdiStats are linked.
        let stat = unsafe { &*p };
        let ns = stat.ns.swap(0, Ordering::Relaxed);
        let calls = stat.calls.swap(0, Ordering::Relaxed);
        if calls != 0 {
            all.push((stat.name, ns, calls));
        }
        p = stat.next.load(Ordering::Acquire);
    }
    all.sort_by(|a, b| b.1.cmp(&a.1));
    all.truncate(n);
    all
}

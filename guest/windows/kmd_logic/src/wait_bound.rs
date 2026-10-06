//! Bounded and killable waits for everything an escape can reach (v334, `docs/zero-copy-present.md`
//! section 23).
//!
//! The incident: a user process stuck in a `D3DKMTEscape` cannot be terminated (a thread in a
//! non-alertable kernel wait never sees the kill), the device stop then waits for what that thread
//! holds, and only a reboot clears it. So every wait an escape can make has three exits besides
//! its own completion, all decided here from plain numbers:
//!
//! * the thread is being terminated (`PsIsThreadTerminating`): [`Abort::Killed`];
//! * the device is stopping or being removed: [`Abort::Stopping`];
//! * the escape has run for longer than `EscWaitMs`: [`Abort::Deadline`].
//!
//! A wait that is not made inside an escape (the HPD worker, a DPC, the paging path, a DDI that
//! dxgkrnl calls on a terminating thread to clean up) is NEVER aborted by any of these: [`verdict`]
//! answers `None` for it. That is the rule that keeps the change small: nothing outside an escape
//! can behave differently.
//!
//! Escapes are registered in a small table keyed by the thread id ([`Slots`]): the wait
//! primitives (`wait_block`, the mutex acquires, the retry budgets) cannot be handed a context
//! through forty call sites, so they ask "is this thread inside an escape, and until when".

use core::sync::atomic::{AtomicU32, Ordering};

/// The longest a wait slice may be inside an escape (ms). Each slice ends with an abort check, so a
/// kill or a stop is noticed within this plus the scheduler's quantum.
pub const SLICE_MS: u32 = 100;

/// The slice cap of a wait that is not inside an escape: the old behaviour (1 s).
pub const UNSCOPED_SLICE_MS: u32 = 1_000;

/// `EscWaitMs` default: the most one escape may spend waiting in total.
pub const ESC_WAIT_DEFAULT_MS: u32 = 10_000;
/// Smallest nonzero `EscWaitMs`.
pub const ESC_WAIT_MIN_MS: u32 = 250;
/// Largest `EscWaitMs` (ten minutes).
pub const ESC_WAIT_MAX_MS: u32 = 600_000;

/// `EscWaitMs` as the driver uses it: 0 stays 0 (no deadline; the kill and stop exits stay on),
/// anything else is clamped into `[ESC_WAIT_MIN_MS, ESC_WAIT_MAX_MS]`.
pub const fn clamp_esc_wait_ms(raw: u32) -> u32 {
    if raw == 0 {
        0
    } else if raw < ESC_WAIT_MIN_MS {
        ESC_WAIT_MIN_MS
    } else if raw > ESC_WAIT_MAX_MS {
        ESC_WAIT_MAX_MS
    } else {
        raw
    }
}

/// Why a wait must give up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abort {
    /// The calling thread is terminating.
    Killed,
    /// The device is being stopped or removed.
    Stopping,
    /// The escape's total wait budget is spent.
    Deadline,
}

/// What a wait can see, all from atomics and one clock read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// The calling thread is inside an escape (it has a registered scope).
    pub scoped: bool,
    /// `PsIsThreadTerminating(PsGetCurrentThread())`.
    pub terminating: bool,
    /// The stopping flag.
    pub stopping: bool,
    /// Interrupt time now, ms (wrapping 32 bit).
    pub now_ms: u32,
    /// The escape's deadline on the same clock; 0 = none.
    pub deadline_ms: u32,
}

/// Whether `deadline` (never 0) is reached at `now`, on the wrapping 32-bit clock. A deadline up
/// to about 24 days ahead reads as not reached.
pub const fn deadline_reached(now_ms: u32, deadline_ms: u32) -> bool {
    (now_ms.wrapping_sub(deadline_ms) as i32) >= 0
}

/// The deadline of an escape that begins at `now_ms` with `limit_ms` (0 = none): never 0 for a
/// real deadline, because 0 means "none" in [`Probe`] and in the table.
pub const fn deadline_for(now_ms: u32, limit_ms: u32) -> u32 {
    if limit_ms == 0 {
        return 0;
    }
    let d = now_ms.wrapping_add(limit_ms);
    if d == 0 {
        1
    } else {
        d
    }
}

/// Milliseconds left of `deadline_ms` at `now_ms`; `None` for no deadline, `Some(0)` when spent.
pub const fn remaining_ms(now_ms: u32, deadline_ms: u32) -> Option<u32> {
    if deadline_ms == 0 {
        return None;
    }
    if deadline_reached(now_ms, deadline_ms) {
        Some(0)
    } else {
        Some(deadline_ms.wrapping_sub(now_ms))
    }
}

/// Whether the wait must stop now. `None` outside an escape, always. Inside one the order is the
/// order of severity: the thread being killed, the device stopping, the deadline.
pub const fn verdict(p: Probe) -> Option<Abort> {
    if !p.scoped {
        return None;
    }
    if p.terminating {
        return Some(Abort::Killed);
    }
    if p.stopping {
        return Some(Abort::Stopping);
    }
    if p.deadline_ms != 0 && deadline_reached(p.now_ms, p.deadline_ms) {
        return Some(Abort::Deadline);
    }
    None
}

/// The next slice of a doubling wait: `prev` doubled, capped at [`SLICE_MS`] inside an escape and
/// at [`UNSCOPED_SLICE_MS`] outside (the old 1 ms -> 1 s ladder, unchanged for everyone else).
pub const fn next_slice_ms(prev: u64, scoped: bool) -> u64 {
    let cap = if scoped { SLICE_MS as u64 } else { UNSCOPED_SLICE_MS as u64 };
    let doubled = prev.saturating_mul(2);
    if doubled > cap {
        cap
    } else if doubled == 0 {
        1
    } else {
        doubled
    }
}

/// The slice to wait now: the wanted one, cut to what is left of the total and, inside an escape,
/// to what is left of the deadline. At least 1.
pub const fn slice_to_wait(wanted_ms: u64, total_left_ms: u64, deadline_left_ms: Option<u32>) -> u64 {
    let mut s = wanted_ms;
    if total_left_ms < s {
        s = total_left_ms;
    }
    if let Some(d) = deadline_left_ms {
        if (d as u64) < s {
            s = d as u64;
        }
    }
    if s == 0 {
        1
    } else {
        s
    }
}

/// What one turn of a bounded wait did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Wait this many ms, then ask again.
    Wait(u64),
    /// The wait's own total is spent (its caller's timeout, as before).
    Spent,
    /// Give up.
    Abort(Abort),
}

/// The state of one bounded wait: its total, what it has spent and the slice it is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounded {
    total_ms: u64,
    waited_ms: u64,
    slice_ms: u64,
}

impl Bounded {
    pub const fn new(total_ms: u64) -> Self {
        Self { total_ms, waited_ms: 0, slice_ms: 1 }
    }

    /// What to do now, given the probe of this instant. An abort is checked BEFORE the total, so a
    /// killed thread leaves at once even if its own budget is also spent.
    pub fn step(&mut self, p: Probe) -> Step {
        if let Some(why) = verdict(p) {
            return Step::Abort(why);
        }
        if self.waited_ms >= self.total_ms {
            return Step::Spent;
        }
        let left = self.total_ms - self.waited_ms;
        let slice = slice_to_wait(self.slice_ms, left, remaining_ms(p.now_ms, p.deadline_ms).filter(|_| p.scoped));
        self.slice_ms = next_slice_ms(self.slice_ms, p.scoped);
        Step::Wait(slice)
    }

    /// The slice that was waited expired without the event: charge it.
    pub fn expired(&mut self, slice_ms: u64) {
        self.waited_ms = self.waited_ms.saturating_add(slice_ms);
    }

    pub const fn waited_ms(&self) -> u64 {
        self.waited_ms
    }
}

// ---- the table of escapes in flight --------------------------------------------------------

/// A fixed table of `(thread id, deadline)` pairs: the escapes in flight. A slot is claimed by
/// compare-and-swap on its id (0 = free) and released by the thread that claimed it; other threads
/// only read. Thread ids are never 0.
pub struct Slots<const N: usize> {
    ids: [AtomicU32; N],
    deadlines: [AtomicU32; N],
}

impl<const N: usize> Slots<N> {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU32 = AtomicU32::new(0);

    pub const fn new() -> Self {
        Self { ids: [Self::ZERO; N], deadlines: [Self::ZERO; N] }
    }

    /// Register `id` with `deadline_ms` (0 = none): the slot index, or `None` when the thread id
    /// is 0 or no slot is free. A thread that is already registered (a nested scope) gets its
    /// existing slot back as `Err(slot)`: the outer scope keeps its deadline, and the inner
    /// one must not release it.
    pub fn enter(&self, id: u32, deadline_ms: u32) -> Option<Result<usize, usize>> {
        if id == 0 {
            return None;
        }
        if let Some(i) = self.slot_of(id) {
            return Some(Err(i));
        }
        for i in 0..N {
            if self.ids[i]
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.deadlines[i].store(deadline_ms, Ordering::Release);
                return Some(Ok(i));
            }
        }
        None
    }

    /// Release slot `i` (the index `enter` returned in `Ok`).
    pub fn leave(&self, i: usize) {
        if i < N {
            self.deadlines[i].store(0, Ordering::Release);
            self.ids[i].store(0, Ordering::Release);
        }
    }

    fn slot_of(&self, id: u32) -> Option<usize> {
        (0..N).find(|&i| self.ids[i].load(Ordering::Acquire) == id)
    }

    /// The deadline of thread `id`'s scope: `None` when the thread is not inside an escape,
    /// `Some(0)` when it is and has no deadline.
    pub fn find(&self, id: u32) -> Option<u32> {
        if id == 0 {
            return None;
        }
        let i = self.slot_of(id)?;
        Some(self.deadlines[i].load(Ordering::Acquire))
    }

    /// Escapes in flight now.
    pub fn count(&self) -> usize {
        (0..N).filter(|&i| self.ids[i].load(Ordering::Relaxed) != 0).count()
    }
}

impl<const N: usize> Default for Slots<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The service-key counter names `ddi/escape_wait.rs` writes, nothing else does (at most 14
/// characters, none equal to another counter of `kmd_render` / `kmd_logic`; host-tested by
/// scanning both trees).
///
/// * `EscWaitN`: escapes that ran inside a scope; `EscWaitMax`: the longest, in ms.
/// * `EscAbortKill`: waits that gave up because the thread was terminating; `EscAbortStop`:
///   because the device was stopping; `EscTimeout`: because the escape's `EscWaitMs` was spent.
/// * `LkWaitAbort`: mutex acquires (venus, scanout, content) that gave up, each returning a
///   failure instead of proceeding without the lock.
/// * `EscNoSlot`: escapes that found the table full (they run with the old unbounded waits).
/// * `EscWaitMsEff`: `EscWaitMs` in force (clamped, 0 = no deadline).
/// * `EscRefStop`: escapes refused at entry because the device was already stopping.
pub const COUNTERS: [&str; 8] = [
    "EscWaitN",
    "EscWaitMax",
    "EscAbortKill",
    "EscAbortStop",
    "EscTimeout",
    "LkWaitAbort",
    "EscNoSlot",
    "EscWaitMsEff",
];

/// `EscRefStop` is separate only so the list above stays the ones written by the escape scope; it
/// is written by the same file.
pub const COUNTERS_EXTRA: [&str; 1] = ["EscRefStop"];

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Probe {
        Probe { scoped: true, terminating: false, stopping: false, now_ms: 1_000, deadline_ms: 0 }
    }

    #[test]
    fn an_unscoped_wait_is_never_aborted() {
        let p = Probe { scoped: false, terminating: true, stopping: true, now_ms: 9, deadline_ms: 1, ..probe() };
        assert_eq!(verdict(p), None);
    }

    #[test]
    fn kill_beats_stop_beats_deadline() {
        let all = Probe { terminating: true, stopping: true, deadline_ms: 500, ..probe() };
        assert_eq!(verdict(all), Some(Abort::Killed));
        let no_kill = Probe { terminating: false, ..all };
        assert_eq!(verdict(no_kill), Some(Abort::Stopping));
        let only_deadline = Probe { stopping: false, ..no_kill };
        assert_eq!(verdict(only_deadline), Some(Abort::Deadline));
        let none = Probe { deadline_ms: 1_001, ..only_deadline };
        assert_eq!(verdict(none), None);
    }

    #[test]
    fn no_deadline_never_expires() {
        let p = Probe { now_ms: u32::MAX - 3, deadline_ms: 0, ..probe() };
        assert_eq!(verdict(p), None);
        assert_eq!(remaining_ms(5, 0), None);
    }

    #[test]
    fn the_deadline_is_exact_and_survives_the_clock_wrap() {
        let d = deadline_for(u32::MAX - 100, 10_000);
        assert_eq!(d, 9_899);
        assert!(!deadline_reached(u32::MAX - 100, d));
        assert!(!deadline_reached(9_898, d));
        assert!(deadline_reached(9_899, d));
        assert!(deadline_reached(20_000, d));
        assert_eq!(remaining_ms(u32::MAX - 100, d), Some(10_000));
        assert_eq!(remaining_ms(9_900, d), Some(0));
    }

    #[test]
    fn a_deadline_is_never_zero() {
        // now + limit lands exactly on 0: 0 would read as "no deadline".
        assert_eq!(deadline_for(u32::MAX - 9, 10), 1);
        assert_eq!(deadline_for(77, 0), 0);
    }

    #[test]
    fn the_knob_clamps() {
        assert_eq!(clamp_esc_wait_ms(0), 0);
        assert_eq!(clamp_esc_wait_ms(1), ESC_WAIT_MIN_MS);
        assert_eq!(clamp_esc_wait_ms(10_000), 10_000);
        assert_eq!(clamp_esc_wait_ms(u32::MAX), ESC_WAIT_MAX_MS);
    }

    #[test]
    fn slices_double_to_the_cap_of_their_scope() {
        let mut s = 1u64;
        let mut seen = std::vec::Vec::new();
        for _ in 0..10 {
            s = next_slice_ms(s, true);
            seen.push(s);
        }
        assert_eq!(seen, [2, 4, 8, 16, 32, 64, 100, 100, 100, 100]);
        let mut s = 1u64;
        for _ in 0..12 {
            s = next_slice_ms(s, false);
        }
        assert_eq!(s, UNSCOPED_SLICE_MS as u64);
    }

    #[test]
    fn a_slice_never_overshoots_the_total_or_the_deadline() {
        assert_eq!(slice_to_wait(100, 30, None), 30);
        assert_eq!(slice_to_wait(100, 1_000, Some(40)), 40);
        assert_eq!(slice_to_wait(100, 1_000, Some(0)), 1);
        assert_eq!(slice_to_wait(0, 5, None), 1);
    }

    /// Drive a bounded wait whose event never comes, with a clock that advances by what was
    /// waited: the unscoped wait ends `Spent` at its own total, as before.
    #[test]
    fn an_unscoped_wait_runs_to_its_own_total() {
        let mut w = Bounded::new(30_000);
        let mut now = 5u32;
        let mut waited = 0u64;
        loop {
            let p = Probe { scoped: false, now_ms: now, ..Probe::default() };
            match w.step(p) {
                Step::Wait(ms) => {
                    w.expired(ms);
                    waited += ms;
                    now = now.wrapping_add(ms as u32);
                }
                Step::Spent => break,
                Step::Abort(_) => panic!("unscoped"),
            }
        }
        assert_eq!(waited, 30_000);
    }

    #[test]
    fn a_scoped_wait_ends_at_the_escape_deadline_not_its_30_seconds() {
        let start = 1_000u32;
        let deadline = deadline_for(start, 10_000);
        let mut w = Bounded::new(30_000);
        let mut now = start;
        let mut slices = 0u32;
        let end = loop {
            let p = Probe { scoped: true, now_ms: now, deadline_ms: deadline, ..Probe::default() };
            match w.step(p) {
                Step::Wait(ms) => {
                    assert!(ms <= SLICE_MS as u64);
                    w.expired(ms);
                    now = now.wrapping_add(ms as u32);
                    slices += 1;
                }
                Step::Spent => panic!("the escape deadline comes first"),
                Step::Abort(why) => break why,
            }
        };
        assert_eq!(end, Abort::Deadline);
        assert_eq!(now, deadline);
        assert!(slices >= 100, "{slices}");
    }

    #[test]
    fn a_kill_is_noticed_within_one_slice() {
        let mut w = Bounded::new(30_000);
        let mut now = 0u32;
        let mut killed_at = None;
        for _ in 0..1_000 {
            let terminating = now >= 2_000;
            let p = Probe { scoped: true, terminating, now_ms: now, deadline_ms: 0, ..Probe::default() };
            match w.step(p) {
                Step::Wait(ms) => {
                    w.expired(ms);
                    now += ms as u32;
                }
                Step::Abort(Abort::Killed) => {
                    killed_at = Some(now);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        let t = killed_at.expect("the kill must end the wait");
        assert!((2_000..2_000 + SLICE_MS).contains(&t), "{t}");
    }

    #[test]
    fn the_stopping_flag_ends_a_scoped_wait_and_no_other() {
        let mut w = Bounded::new(30_000);
        let p = Probe { scoped: true, stopping: true, now_ms: 3, ..Probe::default() };
        assert_eq!(w.step(p), Step::Abort(Abort::Stopping));
        let mut w = Bounded::new(30_000);
        let p = Probe { scoped: false, stopping: true, now_ms: 3, ..Probe::default() };
        assert!(matches!(w.step(p), Step::Wait(_)));
    }

    #[test]
    fn a_spent_total_is_spent_even_with_no_abort() {
        let mut w = Bounded::new(0);
        assert_eq!(w.step(Probe { scoped: true, now_ms: 1, ..Probe::default() }), Step::Spent);
        // but an abort wins over a spent total
        assert_eq!(
            w.step(Probe { scoped: true, terminating: true, now_ms: 1, ..Probe::default() }),
            Step::Abort(Abort::Killed)
        );
    }

    #[test]
    fn slots_register_find_and_release() {
        let t: Slots<4> = Slots::new();
        assert_eq!(t.find(7), None);
        assert_eq!(t.enter(7, 500), Some(Ok(0)));
        assert_eq!(t.find(7), Some(500));
        assert_eq!(t.enter(8, 0), Some(Ok(1)));
        assert_eq!(t.find(8), Some(0));
        assert_eq!(t.count(), 2);
        t.leave(0);
        assert_eq!(t.find(7), None);
        assert_eq!(t.count(), 1);
        // the freed slot is reused
        assert_eq!(t.enter(9, 1), Some(Ok(0)));
    }

    #[test]
    fn a_nested_scope_keeps_the_outer_deadline_and_does_not_release_it() {
        let t: Slots<4> = Slots::new();
        assert_eq!(t.enter(7, 500), Some(Ok(0)));
        // the inner `enter` is told it is nested: its guard must not `leave`
        assert_eq!(t.enter(7, 900), Some(Err(0)));
        assert_eq!(t.find(7), Some(500));
    }

    #[test]
    fn a_full_table_and_thread_zero_register_nothing() {
        let t: Slots<2> = Slots::new();
        assert_eq!(t.enter(0, 5), None);
        assert_eq!(t.enter(1, 5), Some(Ok(0)));
        assert_eq!(t.enter(2, 5), Some(Ok(1)));
        assert_eq!(t.enter(3, 5), None);
        assert_eq!(t.find(3), None);
        assert_eq!(t.find(0), None);
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: std::vec::Vec<&str> = COUNTERS.iter().chain(COUNTERS_EXTRA.iter()).copied().collect();
        for n in &names {
            assert!(!n.is_empty() && n.len() <= 14, "{n}");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    fn literals_of(text: &str) -> std::vec::Vec<std::string::String> {
        let mut out = std::vec::Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else { break };
            let name = &tail[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric()) {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    #[test]
    fn the_io_file_writes_exactly_these_counters_and_nobody_else_does() {
        let Some(render) = render_src() else { return };
        let io = std::fs::read_to_string(render.join("ddi/escape_wait.rs")).unwrap();
        let lits = literals_of(&io);
        for n in COUNTERS.iter().chain(COUNTERS_EXTRA.iter()) {
            assert!(lits.iter().any(|l| l == n), "{n} is listed but not written");
        }
        for l in &lits {
            assert!(
                COUNTERS.contains(&l.as_str()) || COUNTERS_EXTRA.contains(&l.as_str()),
                "{l} is written by escape_wait.rs but not listed"
            );
        }
        let mut stack = std::vec![render];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let name = p.file_name().unwrap().to_string_lossy().into_owned();
                    if name == "escape_wait.rs" {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in COUNTERS.iter().chain(COUNTERS_EXTRA.iter()) {
                        let lit = std::format!("b\"{n}\"");
                        assert!(!text.contains(&lit), "{} spells {n}", p.display());
                    }
                }
            }
        }
        assert!(checked > 20);
    }
}

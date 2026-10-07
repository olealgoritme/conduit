//! The control-queue doorbell rung AFTER `virtio_lock` is released (`SubKickUnlock`,
//! `docs/zero-copy-present.md` 24.14.10): the pure half. The I/O half is `VirtioGpu`'s
//! `publish_then_notify`, `KickState`, `KickTicket` and its `Drop` (`kmd_render/src/virtio/
//! gpu/mod.rs`) and the counter publication in `kmd_render/src/virtio/ctrl.rs`.
//!
//! # The invariant
//!
//! For one submit that rings late, the order is:
//!
//! 1. descriptors written, avail ring slot written (under `virtio_lock`);
//! 2. avail index stored with Release (virtio-drivers `add`, after its own SeqCst fence);
//! 3. a FULL fence, then the suppression check (`should_notify`: `VIRTQ_USED_F_NO_NOTIFY` or the
//!    event index), still under the lock (virtio 1.x 2.7.13.3; Linux `virtqueue_kick_prepare`).
//!    Without the full fence the check could be satisfied from before the index store, read a
//!    stale "do not notify" and lose the wake of a device that has just re-enabled
//!    notifications and found the old index;
//! 4. if a notify is owed: [`begin`] on the guard word (pending + 1), under the lock;
//! 5. `virtio_lock` released;
//! 6. [`may_ring`] on the guard word, the doorbell write (or a drop if the transport is closing);
//! 7. [`end`] (pending - 1), strictly AFTER the doorbell write.
//!
//! Teardown ([`close`] then wait for [`drained`]) can therefore never reset the queue or free
//! the guard between 4 and 7: a pending count is held across the whole window. A kick that
//! finds the guard closed is dropped (`SubKickDrop`): the transport is going away, the device
//! is reset next, and nothing that was published will be answered anyway.
//!
//! Notifications are idempotent and carry no payload for a split ring: a doorbell after the
//! LAST publish covers every earlier one. So two submitters A then B that both owe a ring may
//! ring in either order; whichever rings last is after both publishes, and the first ring is
//! already after its own. A submitter that publishes twice in one hold owes one ring.
//!
//! The doorbell is the queue's own notify register, located at transport init (one MMIO
//! write), not `PciTransport::notify`, which also writes `queue_select` and reads
//! `queue_notify_off` in the shared common configuration and may therefore only run under
//! the lock.

/// The guard word: bit 31 = closed (teardown started), bits 0..31 = kicks owed but not rung.
pub const CLOSED: u32 = 1 << 31;
/// The pending-count bits of the guard word.
pub const PENDING_MASK: u32 = CLOSED - 1;

/// Take one pending kick (under the lock, at publish). `None` when the guard is closed (the
/// caller then rings under the lock, as with the knob off) or the count is saturated.
pub const fn begin(word: u32) -> Option<u32> {
    if word & CLOSED != 0 || word & PENDING_MASK == PENDING_MASK {
        None
    } else {
        Some(word + 1)
    }
}

/// Release one pending kick (after the doorbell write or the drop). Keeps the closed bit.
pub const fn end(word: u32) -> u32 {
    if word & PENDING_MASK == 0 {
        word
    } else {
        word - 1
    }
}

/// Start teardown: no new pending kick is taken from here on.
pub const fn close(word: u32) -> u32 {
    word | CLOSED
}

/// Whether a late kick may write the doorbell (the transport is not closing).
pub const fn may_ring(word: u32) -> bool {
    word & CLOSED == 0
}

/// Whether no kick is owed (teardown may reset the queue and free the guard).
pub const fn drained(word: u32) -> bool {
    word & PENDING_MASK == 0
}

/// What teardown does with the guard after its bounded wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Teardown {
    /// Drained: free it.
    Free,
    /// A kicker still holds it (stalled past the budget): leak it, so the late kicker's [`end`]
    /// touches live memory; that kicker sees the closed bit and drops its ring. Counted.
    Leak,
}

/// [`Teardown`] for the guard word after the wait.
pub const fn teardown(word: u32) -> Teardown {
    if drained(word) {
        Teardown::Free
    } else {
        Teardown::Leak
    }
}

/// How one publish notifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    /// The device suppressed it, or a ring this hold already owes covers it.
    None,
    /// Ring now, under the lock (knob off, a caller that did not arm the late ring, no
    /// doorbell, or a closed guard).
    Locked,
    /// Owe one ring after the lock is released (take [`begin`]).
    Late,
}

/// The decision of one publish. `unlocked`: the knob; `armed`: the caller will ring after the
/// release (only the display submitters and the pipelined flip arm it); `doorbell`: the
/// notify register was located; `wants`: the suppression check (evaluated after the fence);
/// `owed`: this hold already owes a late ring; `open`: [`begin`] would succeed.
pub const fn decide(
    unlocked: bool,
    armed: bool,
    doorbell: bool,
    wants: bool,
    owed: bool,
    open: bool,
) -> Notify {
    if owed {
        // The owed ring happens after the release, which is after this publish.
        return Notify::None;
    }
    if !wants {
        return Notify::None;
    }
    if unlocked && armed && doorbell && open {
        Notify::Late
    } else {
        Notify::Locked
    }
}

/// The knob (REG_DWORD, default 1 = ring after the release; 0 = ring under the lock, the
/// previous behaviour exactly). 13 characters: the requested `SubKickUnlocked` is 15 and would
/// not survive the 14-byte lookup buffer.
pub const KNOB: &str = "SubKickUnlock";

/// The StartDevice mirror of the knob in force (1 only with a located doorbell).
pub const KNOB_MIRROR: &str = "SubKickUnl";

/// The counters (at most 14 characters), written by `kmd_render/src/virtio/ctrl.rs`.
pub const COUNTERS: [&str; 5] = [
    // Doorbells rung after the release.
    "SubKickLate",
    // Late kicks dropped because the transport was closing.
    "SubKickDrop",
    // Microseconds in all from the publish (under the lock) to the late doorbell write.
    "SubKickGap",
    // Teardowns that found a kick pending and waited / gave up waiting and leaked the guard.
    "SubKickWait",
    "SubKickLeak",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn the_guard_counts_closes_and_drains() {
        let w = 0;
        let w = begin(w).unwrap();
        let w = begin(w).unwrap();
        assert_eq!(w & PENDING_MASK, 2);
        assert!(may_ring(w));
        assert!(!drained(w));
        let w = close(w);
        assert!(!may_ring(w), "a kick after close is dropped");
        assert_eq!(begin(w), None, "no new pending kick after close");
        let w = end(w);
        assert!(!drained(w));
        assert_eq!(teardown(w), Teardown::Leak);
        let w = end(w);
        assert!(drained(w));
        assert_eq!(teardown(w), Teardown::Free);
        assert_eq!(end(w), w, "end never underflows");
        assert_eq!(begin(PENDING_MASK), None, "saturated");
    }

    #[test]
    fn the_decision_table() {
        use Notify::*;
        // Suppressed, or covered by a ring this hold owes.
        assert_eq!(decide(true, true, true, false, false, true), None);
        assert_eq!(decide(true, true, true, true, true, true), None);
        // Late only with the knob, an armed caller, a doorbell and an open guard.
        assert_eq!(decide(true, true, true, true, false, true), Late);
        assert_eq!(decide(false, true, true, true, false, true), Locked);
        assert_eq!(decide(true, false, true, true, false, true), Locked);
        assert_eq!(decide(true, true, false, true, false, true), Locked);
        assert_eq!(decide(true, true, true, true, false, false), Locked);
        // Knob off: exactly the old rule (ring iff the device wants it).
        for armed in [false, true] {
            for bell in [false, true] {
                assert_eq!(decide(false, armed, bell, true, false, true), Locked);
                assert_eq!(decide(false, armed, bell, false, false, true), None);
            }
        }
    }

    // ---- a model of the ring, the suppression check and late doorbells ---------------------
    //
    // Drivers run scripts of atomic steps; the device runs the virtio "enable notifications,
    // then re-check the avail index" loop. Every interleaving is explored. The property: when
    // nothing can move any more, the device has seen every published descriptor (no lost wake).

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Step {
        /// Publish one descriptor (store the avail index).
        Publish,
        /// The suppression check after the fence: owe a ring if the device wants one.
        Check,
        /// The late doorbell (after the lock): rings if owed.
        Ring,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Dev {
        /// Processing: consumes everything published, then arms.
        Process,
        /// Notifications enabled; next it re-checks the index.
        Recheck,
        /// Asleep until a doorbell.
        Sleep,
    }

    #[derive(Clone)]
    struct World {
        avail: u32,
        seen: u32,
        /// The device wants a notification (flags clear / event index reached).
        wants: bool,
        /// A doorbell the device has not acted on yet.
        kicked: bool,
        dev: Dev,
        pc: Vec<usize>,
        owes: Vec<bool>,
    }

    fn explore(w: &World, scripts: &[&[Step]], depth: u32, bad: &mut u32) {
        assert!(depth < 200, "model did not terminate");
        let mut moved = false;
        // Driver steps.
        for d in 0..scripts.len() {
            let Some(step) = scripts[d].get(w.pc[d]) else {
                continue;
            };
            let mut n = w.clone();
            n.pc[d] += 1;
            match step {
                Step::Publish => n.avail += 1,
                Step::Check => {
                    // `decide`: a ring this driver already owes covers it.
                    if n.wants && !n.owes[d] {
                        n.owes[d] = true;
                    }
                }
                Step::Ring => {
                    if n.owes[d] {
                        n.owes[d] = false;
                        n.kicked = true;
                    }
                }
            }
            moved = true;
            explore(&n, scripts, depth + 1, bad);
        }
        // Device steps.
        let mut n = w.clone();
        let dev_moved = match w.dev {
            Dev::Process => {
                n.seen = n.avail;
                n.kicked = false;
                n.wants = true;
                n.dev = Dev::Recheck;
                true
            }
            Dev::Recheck => {
                if n.avail != n.seen {
                    n.wants = false;
                    n.dev = Dev::Process;
                } else {
                    n.dev = Dev::Sleep;
                }
                true
            }
            Dev::Sleep => {
                if n.kicked {
                    n.wants = false;
                    n.dev = Dev::Process;
                    true
                } else {
                    false
                }
            }
        };
        if dev_moved {
            moved = true;
            explore(&n, scripts, depth + 1, bad);
        }
        if !moved && w.seen != w.avail {
            *bad += 1;
        }
    }

    fn run(scripts: &[&[Step]]) -> u32 {
        let w = World {
            avail: 0,
            seen: 0,
            wants: false,
            kicked: false,
            dev: Dev::Process,
            pc: std::vec![0; scripts.len()],
            owes: std::vec![false; scripts.len()],
        };
        let mut bad = 0;
        explore(&w, scripts, 0, &mut bad);
        bad
    }

    use Step::*;

    #[test]
    fn two_submitters_with_late_rings_in_any_order_lose_no_wake() {
        // A and B each publish, check after the fence, and ring after the release; every
        // interleaving includes B ringing before A and both rings after both publishes.
        assert_eq!(run(&[&[Publish, Check, Ring], &[Publish, Check, Ring]]), 0);
    }

    #[test]
    fn two_publishes_in_one_hold_owe_one_ring() {
        assert_eq!(run(&[&[Publish, Check, Publish, Check, Ring], &[Publish, Check, Ring]]), 0);
    }

    #[test]
    fn the_locked_ring_is_the_same_model_with_the_ring_right_after_the_check() {
        assert_eq!(run(&[&[Publish, Check, Ring]]), 0);
    }

    #[test]
    fn a_check_before_the_index_store_loses_a_wake() {
        // What a missing full fence permits: the suppression read satisfied from before the
        // avail index store. The model must find the lost wake, or it proves nothing above.
        assert!(run(&[&[Check, Publish, Ring]]) > 0);
    }

    #[test]
    fn a_dropped_ring_is_only_ever_a_closed_transport() {
        // Teardown racing a pending kick: publish (begin), close, the kicker sees closed and
        // drops, ends; only then is the guard drained and freeable.
        let mut w = 0u32;
        w = begin(w).unwrap(); // publish under the lock
        w = close(w); // StopDevice starts
        assert_eq!(teardown(w), Teardown::Leak, "teardown must not free while owed");
        let rang = may_ring(w);
        assert!(!rang, "the late kick is dropped, the dead queue is not touched");
        w = end(w);
        assert_eq!(teardown(w), Teardown::Free);
        // The other order: the kicker rings before the close; teardown then finds it drained.
        let mut v = begin(0).unwrap();
        assert!(may_ring(v));
        v = end(v);
        v = close(v);
        assert_eq!(teardown(v), Teardown::Free);
    }

    #[test]
    fn names_fit_are_unique_and_differ_from_every_other_list() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        names.push(KNOB_MIRROR);
        for n in &names {
            assert!(n.len() <= 14, "{n}");
            assert!(n.starts_with("SubKick"), "{n}");
        }
        assert!(KNOB.len() <= 14);
        assert!(!names.contains(&KNOB));
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        let mut others: Vec<&str> = crate::submit_stage::COUNTERS.to_vec();
        for row in crate::submit_stage::TABLE_NAMES.iter() {
            others.extend_from_slice(row);
        }
        others.extend_from_slice(&crate::submit_stage::KNOB_MIRRORS);
        others.push(crate::submit_stage::KNOB);
        others.push(crate::submit_stage::KNOB_TIMING);
        for n in &names {
            assert!(!others.contains(n), "{n} collides with submit_stage");
        }
    }

    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric()) {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    /// Exact list: `virtio/ctrl.rs` writes every counter, `virtio/gpu/mod.rs` the mirror,
    /// `diag.rs` declares the knob, and no other `kmd_render` file spells any of them.
    #[test]
    fn the_driver_writes_exactly_these_names_in_exactly_these_files() {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if !render.exists() {
            assert!(
                std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
                "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
                render.display()
            );
            return;
        }
        let ctrl = literals(&std::fs::read_to_string(render.join("virtio/ctrl.rs")).unwrap());
        for n in COUNTERS {
            assert_eq!(ctrl.iter().filter(|l| *l == n).count(), 1, "{n} in ctrl.rs");
        }
        for l in ctrl.iter().filter(|l| l.starts_with("SubKick") && l.len() > 7) {
            assert!(COUNTERS.contains(&l.as_str()), "{l} written by ctrl.rs but not listed");
        }
        let gpu = literals(&std::fs::read_to_string(render.join("virtio/gpu/mod.rs")).unwrap());
        assert_eq!(gpu.iter().filter(|l| *l == KNOB_MIRROR).count(), 1);
        assert!(!gpu.iter().any(|l| COUNTERS.contains(&l.as_str())));
        let diag = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        assert!(diag.contains("KnobName::new(b\"SubKickUnlock\")"));
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let s = p.to_string_lossy().into_owned();
                if p.extension().is_none_or(|x| x != "rs")
                    || s.ends_with("virtio/ctrl.rs")
                    || s.ends_with("virtio/gpu/mod.rs")
                    || s.ends_with("/diag.rs")
                {
                    continue;
                }
                checked += 1;
                // Exact names (the stage counter `SubKick` shares the prefix and is not ours).
                for l in literals(&std::fs::read_to_string(&p).unwrap()) {
                    let ours = COUNTERS.contains(&l.as_str()) || l == KNOB_MIRROR || l == KNOB;
                    assert!(!ours, "{s} spells {l}");
                    if l.len() > 14 {
                        assert!(!COUNTERS.contains(&&l[..14]), "{l} in {s} clamps onto ours");
                    }
                }
            }
        }
        assert!(checked > 20);
    }
}

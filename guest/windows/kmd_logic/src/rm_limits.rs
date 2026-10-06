//! Growth and admission for the KMD's per-process tables (handles, mappings): the pure half.
//! The I/O half is `kmd_render/src/virtio/gpu/nvrm_tables.rs` (the table doors) and
//! `virtio/nvrm_window.rs` (the PASSIVE pre-grow step). Documented in
//! `docs/nvrm-escape.md`, section 5.
//!
//! # Why this exists
//!
//! The tables used to be fixed arrays sized by constants (128 handles per process, 1024 in
//! all), reserved once at init so that nothing allocates under the virtio spinlock. A limit
//! like that was a guess, and a leak or a busy game reached it as a bare `NO_RESOURCES`.
//! The limits that matter belong to the host (it opens real files) and to RM; the KMD's own
//! bound is only a SANITY bound: far above anything a real client reaches, there so that a
//! hostile process cannot take the non-paged pool, and counted when it is hit.
//!
//! # The rules
//!
//! * A table starts at `initial` slots (what it always had) and GROWS by doubling when fewer
//!   than `headroom` are free, up to `global_max` slots.
//! * Growth allocates, so it happens at PASSIVE, outside every lock, BEFORE the reservation
//!   that needs it (`want_capacity`); under the lock the new storage is only swapped in
//!   (`kmd_render`), and a slot is only ever pushed when `live < capacity` (`admit`).
//! * One owner may hold at most `per_owner_max`.
//! * Fairness when the table is scarce: once `scarce_num / scarce_den` of `global_max` is
//!   live, an owner already holding `fair_num / fair_den` of `global_max` or more is refused,
//!   so one process cannot take the last of the table from the others.
//!
//! Pure arithmetic on `usize`; every product saturates.

/// The sizes of one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Slots allocated at init.
    pub initial: usize,
    /// Grow when fewer than this many slots are free.
    pub headroom: usize,
    /// The sanity bound over every owner.
    pub global_max: usize,
    /// The sanity bound for one owner.
    pub per_owner_max: usize,
    /// Scarce when `live * scarce_den >= global_max * scarce_num`.
    pub scarce_num: usize,
    pub scarce_den: usize,
    /// While scarce, an owner holding `owner_live * fair_den >= global_max * fair_num` is
    /// refused.
    pub fair_num: usize,
    pub fair_den: usize,
    /// Slots of storage no RESERVATION may take: kept for restores, the entries that must go
    /// back into the table after a host call that failed (a `Close` the host did not take: the
    /// handle is still open there). A restore cannot grow the table (it may run where nothing
    /// may allocate), so without these a full table silently untracked a handle that was
    /// still open on the host. Keep it at or below `headroom`, so growth keeps ahead of it.
    pub restore_slack: usize,
}

impl Bounds {
    /// A table that never grows past `max` and has no per-owner bound but its own `max`
    /// (the shape the fixed tables had).
    pub const fn fixed(initial: usize, global_max: usize, per_owner_max: usize) -> Self {
        Bounds {
            initial,
            headroom: 0,
            global_max,
            per_owner_max,
            // 4/4 of the table, 4/4 held: effectively no fairness rule (live == global_max is
            // refused by the bound itself first).
            scarce_num: 4,
            scarce_den: 4,
            fair_num: 4,
            fair_den: 4,
            restore_slack: 0,
        }
    }

    /// The same shape with `n` slots held back from reservations, for restores.
    pub const fn with_restore_slack(mut self, n: usize) -> Self {
        self.restore_slack = n;
        self
    }

    /// The growing shape: scarce at 3/4 of the table, fair share 1/4 of it.
    pub const fn growing(
        initial: usize,
        headroom: usize,
        global_max: usize,
        per_owner_max: usize,
    ) -> Self {
        Self::growing_fair(initial, headroom, global_max, per_owner_max, 4)
    }

    /// The growing shape with a fair share of `1 / fair_den` of the table.
    pub const fn growing_fair(
        initial: usize,
        headroom: usize,
        global_max: usize,
        per_owner_max: usize,
        fair_den: usize,
    ) -> Self {
        Bounds {
            initial,
            headroom,
            global_max,
            per_owner_max,
            scarce_num: 3,
            scarce_den: 4,
            fair_num: 1,
            fair_den,
            restore_slack: 0,
        }
    }

    /// Whether the fairness rule can ever fire before the per-process bound does: an owner
    /// must be able to reach its fair share. When this is false the rule is dead code (the
    /// bug the first handle shape had: fair share 1/4 of 16384 = the per-process bound 4096).
    pub const fn fairness_reachable(&self) -> bool {
        self.per_owner_max.saturating_mul(self.fair_den) > self.global_max.saturating_mul(self.fair_num)
    }
}

/// The handle table's shape (`virtio/gpu/nvrm_tables.rs`): starts at 1024, grows to 16384, at
/// most 4096 per process. Fair share 1/8 (2048): once the table is 3/4 full (12288), a process
/// holding 2048 or more is refused, so the last quarter (4096 slots) is only for processes
/// below that. Four hostile processes at 4096 cannot fill it and starve the shell.
pub const HANDLES: Bounds = Bounds::growing_fair(1024, 16, 16_384, 4_096, 8).with_restore_slack(8);
/// The mapping table's shape: starts at 1024, grows to 8192 (the adapter-wide view table),
/// at most 4096 per process, fair share 1/4 (2048).
pub const MAPS: Bounds = Bounds::growing(1024, 16, 8_192, 4_096);
/// The event registry's shape, DERIVED from the handle table's: one `READY` registration per
/// handle a process can have open (the registration checks the handle is the caller's), plus
/// its `TRANSPORT_LOST` and `SCANOUT_RELEASED` ones, so per process the handle bound + 2 and in
/// all the handle bound plus two per device (1024 devices of headroom). Starts at 1024, grows.
pub const EVENTS: Bounds = Bounds::growing(
    1024,
    16,
    HANDLES.global_max + 1024,
    HANDLES.per_owner_max + 2,
);
/// `NvWinPolicy` = 0: 1024 registrations, 130 per process, never grown.
pub const EVENTS_LEGACY: Bounds = Bounds::fixed(1024, 1024, 130);
/// `NvWinPolicy` = 0: the old fixed tables (1024 handles, 128 per process; 1024 maps, 256).
pub const HANDLES_LEGACY: Bounds = Bounds::fixed(1024, 1024, 128);
pub const MAPS_LEGACY: Bounds = Bounds::fixed(1024, 1024, 256);

/// What a reservation may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// A slot is free: reserve it.
    Ok,
    /// Under every bound, but no slot is free right now: grow to this many slots (at
    /// PASSIVE, outside the lock) and ask again.
    NeedGrow(usize),
    /// The table is at `global_max`.
    GlobalBound,
    /// This owner is at `per_owner_max`.
    OwnerBound,
    /// The table is scarce and this owner already holds a fair share or more.
    Unfair,
}

/// The capacity to grow to when `live` slots are in use (reservations included) and the
/// table has `capacity`: double, at least enough for `live + headroom + 1`, never past
/// `global_max`. `None`: nothing to do (enough room, or already at the bound).
pub fn want_capacity(b: &Bounds, capacity: usize, live: usize) -> Option<usize> {
    if capacity >= b.global_max {
        return None;
    }
    if capacity.saturating_sub(live) > b.headroom {
        return None;
    }
    let need = live.saturating_add(b.headroom).saturating_add(1);
    let doubled = capacity.saturating_mul(2).max(b.initial.max(1));
    Some(doubled.max(need).min(b.global_max))
}

/// Whether a restore (an entry going back after a failed host call) finds a free slot: pure
/// storage, no bound and no fairness (the entry was admitted when it was made), and never a
/// growth. `live` counts reservations too.
pub fn restore_room(capacity: usize, live: usize) -> bool {
    live < capacity
}

/// May one more slot be reserved for an owner that holds `owner_live` of the `live` in use,
/// in a table of `capacity` slots?
pub fn admit(b: &Bounds, capacity: usize, live: usize, owner_live: usize) -> Admit {
    if live >= b.global_max {
        return Admit::GlobalBound;
    }
    // Storage a reservation may use: the capacity less the slots kept for restores.
    let usable = capacity.saturating_sub(b.restore_slack);
    if owner_live >= b.per_owner_max {
        return Admit::OwnerBound;
    }
    let scarce = live.saturating_mul(b.scarce_den) >= b.global_max.saturating_mul(b.scarce_num);
    if scarce && owner_live.saturating_mul(b.fair_den) >= b.global_max.saturating_mul(b.fair_num) {
        return Admit::Unfair;
    }
    if live >= usable {
        return match want_capacity(b, capacity, live) {
            Some(n) if n > live.saturating_add(b.restore_slack) => Admit::NeedGrow(n),
            // At the bound with no slot: the bound is what refuses.
            _ => Admit::GlobalBound,
        };
    }
    Admit::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: Bounds = Bounds::growing(1024, 16, 16384, 4096);

    #[test]
    fn nothing_to_grow_while_there_is_headroom() {
        assert_eq!(want_capacity(&H, 1024, 0), None);
        assert_eq!(want_capacity(&H, 1024, 1007), None); // 17 free
        assert_eq!(want_capacity(&H, 1024, 1008), Some(2048)); // 16 free: grow now
        assert_eq!(want_capacity(&H, 1024, 1024), Some(2048));
    }

    #[test]
    fn growth_doubles_and_stops_at_the_bound() {
        let mut cap = 1024;
        let mut steps = 0;
        loop {
            match want_capacity(&H, cap, cap) {
                Some(n) => {
                    assert!(n > cap);
                    assert!(n <= H.global_max);
                    cap = n;
                    steps += 1;
                }
                None => break,
            }
        }
        assert_eq!(cap, 16384);
        assert_eq!(steps, 4); // 2048, 4096, 8192, 16384
        assert_eq!(want_capacity(&H, 16384, 16384), None);
    }

    #[test]
    fn growth_that_would_pass_the_bound_is_clamped() {
        let b = Bounds::growing(100, 4, 150, 150);
        assert_eq!(want_capacity(&b, 100, 100), Some(150));
        assert_eq!(want_capacity(&b, 150, 150), None);
    }

    #[test]
    fn growth_covers_a_jump_larger_than_a_doubling() {
        let b = Bounds::growing(8, 4, 1_000_000, 1_000_000);
        // 8 slots, but 100 are already live: one step reaches 105 or more.
        let n = want_capacity(&b, 8, 100).unwrap();
        assert!(n >= 105);
    }

    #[test]
    fn hostile_numbers_do_not_overflow() {
        let b = Bounds::growing(usize::MAX, usize::MAX, usize::MAX, usize::MAX);
        let _ = want_capacity(&b, usize::MAX - 1, usize::MAX);
        let _ = admit(&b, 1, usize::MAX - 1, usize::MAX - 1);
        let b = Bounds::growing(1, 1, usize::MAX, usize::MAX);
        assert_eq!(want_capacity(&b, usize::MAX, usize::MAX), None);
        assert_eq!(admit(&b, usize::MAX, usize::MAX, 0), Admit::GlobalBound);
    }

    #[test]
    fn admit_ok_inside_capacity() {
        assert_eq!(admit(&H, 1024, 10, 3), Admit::Ok);
        assert_eq!(admit(&H, 1024, 1023, 3), Admit::Ok);
    }

    #[test]
    fn admit_asks_for_growth_when_full() {
        assert_eq!(admit(&H, 1024, 1024, 3), Admit::NeedGrow(2048));
        // After growing it is fine.
        assert_eq!(admit(&H, 2048, 1024, 3), Admit::Ok);
    }

    #[test]
    fn per_owner_bound() {
        assert_eq!(admit(&H, 16384, 5000, 4095), Admit::Ok);
        assert_eq!(admit(&H, 16384, 5000, 4096), Admit::OwnerBound);
        assert_eq!(admit(&H, 16384, 5000, 100_000), Admit::OwnerBound);
    }

    #[test]
    fn global_bound() {
        assert_eq!(admit(&H, 16384, 16383, 0), Admit::Ok);
        assert_eq!(admit(&H, 16384, 16384, 0), Admit::GlobalBound);
        assert_eq!(admit(&H, 16384, 99_999, 0), Admit::GlobalBound);
    }

    #[test]
    fn global_bound_beats_owner_bound_and_fairness() {
        assert_eq!(admit(&H, 16384, 16384, 5000), Admit::GlobalBound);
    }

    #[test]
    fn fairness_when_scarce() {
        // Scarce from 12288 live (3/4 of 16384). Fair share: 4096 (1/4), which here equals the
        // per-owner bound, so make the per-owner bound looser to see the rule alone.
        let b = Bounds::growing(1024, 16, 16384, 16384);
        assert_eq!(admit(&b, 16384, 12287, 4096), Admit::Ok, "not scarce yet");
        assert_eq!(admit(&b, 16384, 12288, 4095), Admit::Ok, "scarce but below the share");
        assert_eq!(admit(&b, 16384, 12288, 4096), Admit::Unfair);
        assert_eq!(admit(&b, 16384, 16000, 10_000), Admit::Unfair);
        // A small owner still gets the last slots.
        assert_eq!(admit(&b, 16384, 16383, 3), Admit::Ok);
    }

    /// The production shapes: fairness must be reachable, or it is dead code (the first handle
    /// shape had fair share 1/4 of 16384 = the per-process bound 4096, so the per-process
    /// refusal always fired first and `NvHdlFRef` could never move).
    /// A process that can hold N handles can register N events: the event bound is the
    /// handle bound (plus the handle-less kinds), never below it (the old fixed 130 against
    /// 4096 handles failed `EVENT_REGISTER` past ~128 live fences).
    #[test]
    fn events_follow_handles() {
        assert!(EVENTS.per_owner_max >= HANDLES.per_owner_max + 2);
        assert!(EVENTS.global_max >= HANDLES.global_max + 1024);
        assert_eq!(EVENTS_LEGACY.per_owner_max, HANDLES_LEGACY.per_owner_max + 2);
        assert_eq!(EVENTS.restore_slack, 0);
    }

    #[test]
    fn production_shapes_can_actually_be_fair() {
        // Growth keeps ahead of the restore slack.
        assert!(HANDLES.restore_slack > 0 && HANDLES.restore_slack <= HANDLES.headroom);
        assert_eq!(MAPS.restore_slack, 0);
        assert!(HANDLES.fairness_reachable());
        assert!(MAPS.fairness_reachable());
        // The fixed legacy shapes have no fairness rule and need none.
        assert!(HANDLES.per_owner_max < HANDLES.global_max);
        assert!(MAPS.per_owner_max <= MAPS.global_max);
        // The old shape is the one that was dead.
        assert!(!Bounds::growing(1024, 16, 16_384, 4_096).fairness_reachable());
    }

    /// Four hostile processes (each trying to take everything) cannot fill the handle table:
    /// a fresh process (the shell) still gets slots, and keeps getting them up to its own
    /// bound.
    #[test]
    fn hostile_owners_cannot_starve_a_fresh_one() {
        let b = HANDLES;
        let mut cap = b.initial;
        let mut live = 0usize;
        let mut hostile = [0usize; 4];
        let mut refused = [0usize; 3]; // owner bound, global, unfair
        // Round-robin greedy: every hostile process asks for a slot again and again.
        for _ in 0..40_000 {
            for h in 0..4 {
                if let Some(n) = want_capacity(&b, cap, live) {
                    cap = n;
                }
                match admit(&b, cap, live, hostile[h]) {
                    Admit::Ok => {
                        hostile[h] += 1;
                        live += 1;
                    }
                    Admit::OwnerBound => refused[0] += 1,
                    Admit::GlobalBound => refused[1] += 1,
                    Admit::Unfair => refused[2] += 1,
                    Admit::NeedGrow(n) => cap = n,
                }
            }
        }
        // The fairness rule did the refusing, and it left the last quarter alone.
        assert!(refused[2] > 0, "NvHdlFRef must be reachable");
        assert!(live <= b.global_max * 3 / 4 + 4, "hostile took {live}");
        assert!(hostile.iter().all(|&n| n <= b.per_owner_max));
        // A fresh owner (nothing held) gets slots, many of them.
        let mut mine = 0usize;
        for _ in 0..1000 {
            if let Some(n) = want_capacity(&b, cap, live) {
                cap = n;
            }
            match admit(&b, cap, live, mine) {
                Admit::Ok => {
                    mine += 1;
                    live += 1;
                }
                other => panic!("the fresh owner was refused at {mine}: {other:?}"),
            }
        }
        // ... and up to just below the fair share, whatever the others hold.
        while mine < b.global_max / 8 - 1 {
            match admit(&b, cap, live, mine) {
                Admit::Ok => {
                    mine += 1;
                    live += 1;
                }
                other => panic!("the fresh owner was refused at {mine}: {other:?}"),
            }
        }
        assert!(live <= b.global_max);
    }

    /// Reservations stop `restore_slack` short of the storage, so a restore always finds room.
    #[test]
    fn restore_slack_is_never_taken_by_a_reservation() {
        let b = HANDLES;
        // Storage 1024, 1016 live: the next reservation would eat the slack: grow first.
        assert_eq!(admit(&b, 1024, 1015, 3), Admit::Ok);
        assert_eq!(admit(&b, 1024, 1016, 3), Admit::NeedGrow(2048));
        // ... and the restore path can still use it.
        assert!(restore_room(1024, 1016));
        assert!(restore_room(1024, 1023));
        assert!(!restore_room(1024, 1024));
        // At the global bound the slack stays: reservations are refused 8 short of it, as the
        // bound says (never NeedGrow: there is nowhere to grow to).
        let top = b.global_max;
        assert_eq!(admit(&b, top, top - b.restore_slack - 1, 3), Admit::Ok);
        assert_eq!(admit(&b, top, top - b.restore_slack, 3), Admit::GlobalBound);
        assert!(restore_room(top, top - 1));
    }

    /// With the PASSIVE pre-grow run before each reservation, a sequential client never sees
    /// NeedGrow, and the slack is never touched.
    #[test]
    fn pre_grow_keeps_sequential_reservations_ahead_of_the_slack() {
        let b = HANDLES;
        let mut cap = b.initial;
        let mut live = 0;
        for _ in 0..2_000 {
            if let Some(n) = want_capacity(&b, cap, live) {
                cap = n;
            }
            match admit(&b, cap, live, live.min(100)) {
                Admit::Ok => live += 1,
                other => panic!("{other:?} at {live}"),
            }
            assert!(cap - live >= b.restore_slack);
        }
    }

    #[test]
    fn fixed_shape_is_the_old_behaviour() {
        // The legacy tables: 1024 in all, 128 per owner, never grown.
        let b = Bounds::fixed(1024, 1024, 128);
        for live in 0..1024 {
            assert_eq!(admit(&b, 1024, live, 127), Admit::Ok);
            assert_eq!(admit(&b, 1024, live, 128), Admit::OwnerBound);
        }
        assert_eq!(admit(&b, 1024, 1024, 0), Admit::GlobalBound);
        assert_eq!(want_capacity(&b, 1024, 1023), None);
    }

    /// Simulate a table filling from several owners through the grow-then-reserve protocol.
    #[test]
    fn protocol_fill_and_drain() {
        let b = Bounds::growing(4, 2, 64, 40);
        let mut cap = b.initial;
        let mut owners = [0usize; 3];
        let mut live = 0usize;
        let mut refused_owner = 0;
        let mut refused_global = 0;
        let mut refused_unfair = 0;
        for i in 0..400 {
            let o = i % 3;
            // PASSIVE pre-grow, then the reservation under the lock.
            if let Some(n) = want_capacity(&b, cap, live) {
                cap = n;
            }
            match admit(&b, cap, live, owners[o]) {
                Admit::Ok => {
                    owners[o] += 1;
                    live += 1;
                    assert!(live <= cap);
                }
                Admit::NeedGrow(n) => {
                    cap = n;
                    assert_eq!(admit(&b, cap, live, owners[o]), Admit::Ok);
                    owners[o] += 1;
                    live += 1;
                }
                Admit::OwnerBound => refused_owner += 1,
                Admit::GlobalBound => refused_global += 1,
                Admit::Unfair => refused_unfair += 1,
            }
            assert!(cap <= b.global_max);
        }
        assert!(live <= 64);
        assert!(owners.iter().all(|&n| n <= 40));
        assert!(refused_owner + refused_global + refused_unfair > 0);
        // Drain one owner entirely: its slots are available again.
        live -= owners[0];
        owners[0] = 0;
        assert_eq!(admit(&b, cap, live, owners[0]), Admit::Ok);
    }

    #[test]
    fn real_game_numbers_never_hit_the_sanity_bounds() {
        // A game that leaks one fence per 400 frames for 56 700 frames holds ~142 handles.
        let live = 142;
        assert_eq!(want_capacity(&H, 1024, live), None);
        assert_eq!(admit(&H, 1024, live, live), Admit::Ok);
        // ... where the old fixed bound refused at 128.
        let old = Bounds::fixed(1024, 1024, 128);
        assert_eq!(admit(&old, 1024, live, live), Admit::OwnerBound);
    }
}

//! The generic pending-flip watchdog (`FlipPendWdMs`, default 500; v334).
//!
//! `FlipWdogMs` (`stall_diag::pend_step`, default off) only counts while a PROGRAMMING is pending
//! (a slot or the gate): a flip whose gate was lowered without a publication, or that never
//! raised one, reads as `VsPendN` 0 and the watchdog has nothing to count, however long dxgkrnl
//! waits for the retire (`docs/zero-copy-present.md` 14.3, "what the watchdog does not cover").
//!
//! This one asks only: is the NEWEST flip dxgkrnl issued still not done, and has it been
//! neither published nor followed by any publication for `limit_ms`? Then its address is
//! published kept (a retire, not a picture), whatever the worker is doing or whether anything is
//! pending. It uses the same record of the newest flip (`pack_flip` / `flip_done_by`) as the
//! other watchdog, so the two never publish the same flip twice and never publish an address older
//! than one already done.
//!
//! The risk is the one of 14.3: a kept address names a picture that is not on the screen. With a
//! genuinely slow producer (a boundary that retires after `limit_ms`) the screen shows the
//! previous picture a moment longer and the real bind lands later; nothing is damaged.

use crate::stall_diag::{age_ms, flip_address, flip_seq, seq_newer};

/// `FlipPendWdMs` default (ms): about 120 frames at 240 Hz, far above a healthy flip's life (a
/// few ticks) and far below anything a person would call a hang.
pub const PEND_WD_DEFAULT_MS: u32 = 500;
/// Smallest nonzero `FlipPendWdMs`.
pub const PEND_WD_MIN_MS: u32 = 100;
/// Largest `FlipPendWdMs`.
pub const PEND_WD_MAX_MS: u32 = 60_000;

/// `FlipPendWdMs` as the driver uses it: 0 stays 0 (off), anything else is clamped into
/// `[PEND_WD_MIN_MS, PEND_WD_MAX_MS]`.
pub const fn clamp_pend_wd_ms(raw: u32) -> u32 {
    if raw == 0 {
        0
    } else if raw < PEND_WD_MIN_MS {
        PEND_WD_MIN_MS
    } else if raw > PEND_WD_MAX_MS {
        PEND_WD_MAX_MS
    } else {
        raw
    }
}

/// What one vsync tick can see.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendWdInput {
    /// Interrupt time now, ms.
    pub now_ms: u32,
    /// `FlipPendWdMs` in force, 0 = off.
    pub limit_ms: u32,
    /// The newest recorded flip (`pack_flip`), 0 for none.
    pub flip_word: u64,
    /// The number of the newest flip that is done (published by anyone).
    pub done_seq: u32,
    /// When the newest recorded flip was issued (ms); 0 = unknown (never fires).
    pub issue_t_ms: u32,
    /// When any address was last published (`FlipPubT`, ms); 0 = never.
    pub pub_t_ms: u32,
}

/// What the tick must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendWdAction {
    None,
    /// Publish this address kept and mark flip number [`flip_seq`] of the word done.
    Publish(u64),
}

/// The decision. Fires when it is on, a flip is recorded and newer than the last one done, the
/// flip is at least `limit_ms` old and no publication happened within the last `limit_ms`. Once
/// per flip (the caller marks it done); a newer flip that sticks fires again `limit_ms` after its
/// own issue.
pub const fn pend_wd_step(i: PendWdInput) -> PendWdAction {
    if i.limit_ms == 0 || i.flip_word == 0 || i.issue_t_ms == 0 {
        return PendWdAction::None;
    }
    if !seq_newer(flip_seq(i.flip_word), i.done_seq) {
        return PendWdAction::None;
    }
    if age_ms(i.now_ms, i.issue_t_ms) < i.limit_ms {
        return PendWdAction::None;
    }
    if i.pub_t_ms != 0 && age_ms(i.now_ms, i.pub_t_ms) < i.limit_ms {
        return PendWdAction::None;
    }
    let address = flip_address(i.flip_word);
    if address == 0 {
        return PendWdAction::None;
    }
    PendWdAction::Publish(address)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::stall_diag::pack_flip;

    fn input() -> PendWdInput {
        PendWdInput {
            now_ms: 10_000,
            limit_ms: 500,
            flip_word: pack_flip(7, 0xC000_0000).unwrap(),
            done_seq: 6,
            issue_t_ms: 9_000,
            pub_t_ms: 8_900,
        }
    }

    #[test]
    fn a_stuck_newest_flip_is_published_after_the_limit() {
        assert_eq!(pend_wd_step(input()), PendWdAction::Publish(0xC000_0000));
    }

    #[test]
    fn off_never_fires() {
        assert_eq!(pend_wd_step(PendWdInput { limit_ms: 0, ..input() }), PendWdAction::None);
    }

    #[test]
    fn a_young_flip_is_left_alone_to_the_millisecond() {
        let i = input();
        assert_eq!(pend_wd_step(PendWdInput { issue_t_ms: 9_501, ..i }), PendWdAction::None);
        assert_eq!(
            pend_wd_step(PendWdInput { issue_t_ms: 9_500, pub_t_ms: 0, ..i }),
            PendWdAction::Publish(0xC000_0000)
        );
    }

    #[test]
    fn a_recent_publication_of_anything_is_progress() {
        let i = input();
        assert_eq!(pend_wd_step(PendWdInput { pub_t_ms: 9_600, ..i }), PendWdAction::None);
        // a publication exactly `limit` ago is not recent
        assert_eq!(pend_wd_step(PendWdInput { pub_t_ms: 9_500, ..i }), PendWdAction::Publish(0xC000_0000));
        // never published at all: no progress to wait for
        assert_eq!(pend_wd_step(PendWdInput { pub_t_ms: 0, ..i }), PendWdAction::Publish(0xC000_0000));
    }

    #[test]
    fn a_done_flip_never_fires_and_neither_does_an_older_one() {
        let i = input();
        assert_eq!(pend_wd_step(PendWdInput { done_seq: 7, ..i }), PendWdAction::None);
        assert_eq!(pend_wd_step(PendWdInput { done_seq: 9, ..i }), PendWdAction::None);
    }

    #[test]
    fn nothing_recorded_or_unstamped_never_fires() {
        let i = input();
        assert_eq!(pend_wd_step(PendWdInput { flip_word: 0, ..i }), PendWdAction::None);
        assert_eq!(pend_wd_step(PendWdInput { issue_t_ms: 0, ..i }), PendWdAction::None);
    }

    #[test]
    fn the_clock_wrap_does_not_fire_early_or_late() {
        // issued 100 ms before the wrap, now 600 ms after it: 700 ms old
        let i = PendWdInput {
            now_ms: 600,
            issue_t_ms: u32::MAX - 99,
            pub_t_ms: u32::MAX - 99,
            ..input()
        };
        assert_eq!(pend_wd_step(i), PendWdAction::Publish(0xC000_0000));
        // 300 ms old across the wrap: not yet
        let young = PendWdInput { now_ms: 200, ..i };
        assert_eq!(pend_wd_step(young), PendWdAction::None);
        // a stamp ahead of now reads as age 0, never as 49 days
        let ahead = PendWdInput { issue_t_ms: 10_050, pub_t_ms: 0, ..input() };
        assert_eq!(pend_wd_step(ahead), PendWdAction::None);
    }

    #[test]
    fn it_fires_once_per_flip_and_again_for_a_newer_stuck_one() {
        let mut i = input();
        let PendWdAction::Publish(_) = pend_wd_step(i) else { panic!() };
        // the caller marked flip 7 done and the publication stamped FlipPubT
        i.done_seq = 7;
        i.pub_t_ms = 10_000;
        assert_eq!(pend_wd_step(i), PendWdAction::None);
        // flip 8 issued at 10_100 and stuck: due at 10_600 (and 500 ms after the publication)
        i.flip_word = pack_flip(8, 0xC100_0000).unwrap();
        i.issue_t_ms = 10_100;
        i.now_ms = 10_599;
        assert_eq!(pend_wd_step(i), PendWdAction::None);
        i.now_ms = 10_600;
        assert_eq!(pend_wd_step(i), PendWdAction::Publish(0xC100_0000));
    }

    #[test]
    fn the_knob_clamps() {
        assert_eq!(clamp_pend_wd_ms(0), 0);
        assert_eq!(clamp_pend_wd_ms(1), PEND_WD_MIN_MS);
        assert_eq!(clamp_pend_wd_ms(500), 500);
        assert_eq!(clamp_pend_wd_ms(u32::MAX), PEND_WD_MAX_MS);
    }

    /// The incident's numbers: six flips issued, none published, the heartbeat alive, nothing
    /// pending (`VsPendN` 0): `FlipWdogMs` could not have fired, this does.
    #[test]
    fn the_wedge_with_nothing_pending_is_caught() {
        let i = PendWdInput {
            now_ms: 4_700_000,
            limit_ms: PEND_WD_DEFAULT_MS,
            flip_word: pack_flip(16_267, 0xC0DE_0000).unwrap(),
            done_seq: 16_261,
            issue_t_ms: 4_699_370,
            pub_t_ms: 4_699_380,
        };
        assert_eq!(pend_wd_step(i), PendWdAction::Publish(0xC0DE_0000));
    }
}

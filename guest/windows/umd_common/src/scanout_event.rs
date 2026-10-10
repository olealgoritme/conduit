//! The scanout-acquire retirement event's registration policy (D4a, umd
//! `scanout_acquire.rs`): what a device does with its event after REGISTER
//! answered, and when it asks again after a refusal.
//!
//! The DXVK signaler has two modes. With an event it sleeps on the event and
//! re-reads the read ledger on every wake (plus a 10 ms safety timeout). Without
//! one it polls the ledger every 1 ms while a gate is armed. An event the KMD
//! refused is never signaled, so handing it to DXVK would put the signaler on
//! its 10 ms timeout: the refused event is closed and DXVK gets none (1 ms
//! polling) until a later REGISTER succeeds.
//!
//! Platform-free so the rules run on the Linux host.

/// What REGISTER answered: the escape's `out_state`, or the escape's failure
/// HRESULT.
pub type RegisterReply = Result<u32, i32>;

/// What the device does with the event it registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    /// The KMD parked it: keep it and hand it to the DXVK signaler.
    Deliver(usize),
    /// Refused (table full, owner cap, any other state, escape failure):
    /// close it; DXVK keeps polling and the device retries later.
    Close(usize),
}

impl Handoff {
    /// The handle to hand to DXVK (0 = none: DXVK polls the ledger at 1 ms).
    pub const fn delivered(self) -> usize {
        match self {
            Handoff::Deliver(event) => event,
            Handoff::Close(_) => 0,
        }
    }

    /// The handle to close now, if any.
    pub const fn to_close(self) -> Option<usize> {
        match self {
            Handoff::Deliver(_) => None,
            Handoff::Close(event) => Some(event),
        }
    }
}

/// Decide the fate of `event` from REGISTER's reply; `ok_state` is
/// `HELIOS_SCANOUT_ACQ_OK`. Only an explicit OK keeps it.
pub fn handoff(reply: RegisterReply, ok_state: u32, event: usize) -> Handoff {
    match reply {
        Ok(state) if state == ok_state && event != 0 => Handoff::Deliver(event),
        _ => Handoff::Close(event),
    }
}

/// When a refused device asks again. The first retry comes soon (a table
/// slot frees whenever any device in the system is destroyed); each further
/// refusal doubles the wait up to [`RetrySchedule::MAX_MS`], so a full table
/// costs at most one escape per device every few seconds. Retries happen on
/// the device's own present and flush calls, never on a timer, so an idle
/// device costs nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetrySchedule {
    due_ms: u64,
    backoff_ms: u64,
}

impl RetrySchedule {
    pub const FIRST_MS: u64 = 250;
    pub const MAX_MS: u64 = 4000;

    /// A device refused at `now_ms`.
    pub const fn refused_at(now_ms: u64) -> Self {
        Self {
            due_ms: now_ms.saturating_add(Self::FIRST_MS),
            backoff_ms: Self::FIRST_MS,
        }
    }

    pub const fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.due_ms
    }

    /// When the next attempt is due.
    pub const fn due_ms(&self) -> u64 {
        self.due_ms
    }

    /// Another refusal at `now_ms`.
    pub fn refused_again(&mut self, now_ms: u64) {
        self.backoff_ms = self.backoff_ms.saturating_mul(2).min(Self::MAX_MS);
        self.due_ms = now_ms.saturating_add(self.backoff_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: u32 = 0;
    const TABLE_FULL: u32 = 3;

    #[test]
    fn only_ok_delivers() {
        assert_eq!(handoff(Ok(OK), OK, 0x44), Handoff::Deliver(0x44));
        assert_eq!(handoff(Ok(OK), OK, 0x44).delivered(), 0x44);
        assert_eq!(handoff(Ok(OK), OK, 0x44).to_close(), None);
    }

    #[test]
    fn refused_register_closes_and_delivers_nothing() {
        for reply in [Ok(TABLE_FULL), Ok(2), Ok(!0), Err(-2147024809), Err(i32::MIN)] {
            let h = handoff(reply, OK, 0x44);
            assert_eq!(h, Handoff::Close(0x44), "{reply:?}");
            assert_eq!(h.delivered(), 0, "DXVK must poll, not wait on a dead event");
            assert_eq!(h.to_close(), Some(0x44));
        }
    }

    #[test]
    fn no_event_delivers_nothing() {
        assert_eq!(handoff(Ok(OK), OK, 0).delivered(), 0);
    }

    #[test]
    fn retry_backs_off_and_caps() {
        let mut r = RetrySchedule::refused_at(1000);
        assert!(!r.due(1000));
        assert!(!r.due(1000 + RetrySchedule::FIRST_MS - 1));
        assert!(r.due(1000 + RetrySchedule::FIRST_MS));
        let mut now = 1000 + RetrySchedule::FIRST_MS;
        let mut last_gap = RetrySchedule::FIRST_MS;
        for _ in 0..10 {
            r.refused_again(now);
            let mut gap = 0;
            while !r.due(now + gap) {
                gap += 1;
            }
            assert!(gap >= last_gap && gap <= RetrySchedule::MAX_MS);
            last_gap = gap;
            now += gap;
        }
        assert_eq!(last_gap, RetrySchedule::MAX_MS);
    }
}

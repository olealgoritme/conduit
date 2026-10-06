//! Registrations of usermode events against RM backend handles
//! (`HELIOS_NVRM_OP_EVENT_REGISTER` / `EVENT_UNREGISTER`).
//!
//! The KMD turns a host `EventReady{handle}` into `KeSetEvent` on the event the
//! owning process registered for that handle, and a lost transport into a
//! `KeSetEvent` on every registration. This is the bookkeeping, generic over what
//! an "event" is (`NonNull<KEVENT>` in the driver, a plain integer in the tests):
//!
//! * a registration is keyed by `(owner, handle, kind)`; registering the same key
//!   again REPLACES the event and hands the old one back;
//! * the table is bounded twice, in total and per owner, and its storage is
//!   reserved once ([`Registry::try_new`], PASSIVE) so that nothing after that
//!   allocates: the driver calls [`Registry::add`] and the `signal_*` methods
//!   under a spinlock, at DISPATCH_LEVEL;
//! * nothing here releases an event. Every method that removes one HANDS IT BACK
//!   by value, because dropping the object reference it stands for may only
//!   happen at PASSIVE, outside every lock.
//!
//! Pure functions of their arguments: no wdk, no atomics.

extern crate alloc;
use alloc::vec::Vec;

/// The host sent `EventReady` for the registered handle.
pub const KIND_READY: u32 = 1;
/// The device was reset or the transport replaced: every registration wakes.
pub const KIND_LOST: u32 = 2;
/// A buffer of the caller's scanout source was released by the host
/// (`ScanoutReleased`, `docs/foreign-scanout.md`), or a flip that was waiting for it
/// was found never to have reached the host. Handle-less, like `KIND_LOST`: the
/// registration is the process's, keyed with handle 0. Offered only on a device that
/// negotiated `NVGPU_F_SCANOUT_RELEASE`.
pub const KIND_SCANOUT_RELEASED: u32 = 3;
/// Bitmask over the kinds that exist on every device (bit `n` <=> kind `n`), as
/// `QUERY_CAPS` reports.
pub const KINDS_ALL: u32 = (1 << KIND_READY) | (1 << KIND_LOST);
/// The kinds that exist only with the release feature: ORed into the `QUERY_CAPS` word
/// by the driver when the host's release events are on.
pub const KINDS_SCANOUT_RELEASE: u32 = 1 << KIND_SCANOUT_RELEASED;

/// Whether `kind` is one this table knows (whether the DEVICE can serve it is the
/// driver's to say: [`KINDS_ALL`] always, [`KINDS_SCANOUT_RELEASE`] with the feature).
pub const fn kind_known(kind: u32) -> bool {
    kind < 32 && ((KINDS_ALL | KINDS_SCANOUT_RELEASE) >> kind) & 1 != 0
}

/// Whether a registration of `kind` names no backend handle (the key uses handle 0).
pub const fn kind_has_no_handle(kind: u32) -> bool {
    kind == KIND_LOST || kind == KIND_SCANOUT_RELEASED
}

/// What [`Registry::add`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added<E> {
    /// A new registration.
    New,
    /// The key was registered already: the new event is in, and this is the old
    /// one, which the caller releases.
    Replaced(E),
    /// The table as a whole is full. Nothing changed.
    TotalFull,
    /// This owner's quota is full. Nothing changed.
    OwnerFull,
}

struct Reg<E> {
    owner: usize,
    handle: u32,
    kind: u32,
    event: E,
}

/// The registrations of every process, bounded.
pub struct Registry<E: Copy + PartialEq> {
    regs: Vec<Reg<E>>,
    max_total: usize,
    max_per_owner: usize,
}

impl<E: Copy + PartialEq> Registry<E> {
    /// A table for up to `max_total` registrations, `max_per_owner` of them one
    /// owner's. Reserves the storage now (`None` if the allocator refuses); a
    /// `max_total` of 0 reserves nothing and refuses every `add`. PASSIVE.
    pub fn try_new(max_total: usize, max_per_owner: usize) -> Option<Self> {
        let mut regs = Vec::new();
        regs.try_reserve_exact(max_total).ok()?;
        Some(Self {
            regs,
            max_total,
            max_per_owner,
        })
    }

    /// How many registrations are live.
    pub fn len(&self) -> usize {
        self.regs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regs.is_empty()
    }

    /// How many registrations `owner` holds.
    pub fn count_for_owner(&self, owner: usize) -> usize {
        self.regs.iter().filter(|r| r.owner == owner).count()
    }

    /// Register `event` for `(owner, handle, kind)`. Never allocates.
    pub fn add(&mut self, owner: usize, handle: u32, kind: u32, event: E) -> Added<E> {
        if let Some(r) = self
            .regs
            .iter_mut()
            .find(|r| r.owner == owner && r.handle == handle && r.kind == kind)
        {
            let old = core::mem::replace(&mut r.event, event);
            return Added::Replaced(old);
        }
        // `len < max_total <= capacity`, so the push cannot reallocate.
        if self.regs.len() >= self.max_total || self.regs.len() >= self.regs.capacity() {
            return Added::TotalFull;
        }
        if self.count_for_owner(owner) >= self.max_per_owner {
            return Added::OwnerFull;
        }
        self.regs.push(Reg {
            owner,
            handle,
            kind,
            event,
        });
        Added::New
    }

    /// Remove the registration `(owner, handle, kind)`, handing its event back.
    pub fn remove(&mut self, owner: usize, handle: u32, kind: u32) -> Option<E> {
        let idx = self
            .regs
            .iter()
            .position(|r| r.owner == owner && r.handle == handle && r.kind == kind)?;
        Some(self.regs.swap_remove(idx).event)
    }

    /// Pop one registration `owner` holds on `handle` (any kind): `Close` of the
    /// handle, called until it returns `None`.
    pub fn take_for_handle(&mut self, owner: usize, handle: u32) -> Option<E> {
        let idx = self
            .regs
            .iter()
            .position(|r| r.owner == owner && r.handle == handle)?;
        Some(self.regs.swap_remove(idx).event)
    }

    /// Pop one registration anyone holds on `handle` (any kind): the KMD took the
    /// handle over (an RM fence attached to a present), so what its creator
    /// registered on it must not outlive the creator's ownership. Called until it
    /// returns `None`. A `TRANSPORT_LOST` registration is keyed 0 and never matches
    /// a (nonzero) handle.
    pub fn take_any_for_handle(&mut self, handle: u32) -> Option<E> {
        if handle == 0 {
            return None;
        }
        let idx = self.regs.iter().position(|r| r.handle == handle)?;
        Some(self.regs.swap_remove(idx).event)
    }

    /// Pop one registration `owner` holds: the owner's device teardown.
    pub fn take_for_owner(&mut self, owner: usize) -> Option<E> {
        let idx = self.regs.iter().position(|r| r.owner == owner)?;
        Some(self.regs.swap_remove(idx).event)
    }

    /// Pop any registration: the transport's teardown.
    pub fn take_any(&mut self) -> Option<E> {
        self.regs.pop().map(|r| r.event)
    }

    /// Call `f` with the event of every registration of `kind` on `handle`
    /// (whoever owns it: a backend handle belongs to one process). Returns how
    /// many. Allocation-free, so it may run under the spinlock; `f` must be
    /// callable at DISPATCH_LEVEL.
    pub fn signal_handle(&self, handle: u32, kind: u32, mut f: impl FnMut(E)) -> usize {
        let mut n = 0;
        for r in self
            .regs
            .iter()
            .filter(|r| r.handle == handle && r.kind == kind)
        {
            f(r.event);
            n += 1;
        }
        n
    }

    /// Call `f` with the event of every registration of `kind` that `owner` holds.
    /// Returns how many. Allocation-free (see [`Self::signal_handle`]).
    pub fn signal_owner_kind(&self, owner: usize, kind: u32, mut f: impl FnMut(E)) -> usize {
        let mut n = 0;
        for r in self
            .regs
            .iter()
            .filter(|r| r.owner == owner && r.kind == kind)
        {
            f(r.event);
            n += 1;
        }
        n
    }

    /// Call `f` with the event of every registration, whatever its kind. Returns
    /// how many.
    pub fn signal_all(&self, mut f: impl FnMut(E)) -> usize {
        for r in &self.regs {
            f(r.event);
        }
        self.regs.len()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec as StdVec;

    fn reg(total: usize, per_owner: usize) -> Registry<u32> {
        Registry::try_new(total, per_owner).expect("reserve")
    }

    #[test]
    fn kinds() {
        assert!(kind_known(KIND_READY));
        assert!(kind_known(KIND_LOST));
        assert!(!kind_known(0));
        assert!(kind_known(KIND_SCANOUT_RELEASED));
        assert!(!kind_known(4));
        assert!(!kind_known(32));
        assert!(!kind_known(u32::MAX));
        assert_eq!(KINDS_ALL, 0b110);
        assert_eq!(KINDS_SCANOUT_RELEASE, 0b1000);
        assert!(kind_has_no_handle(KIND_LOST) && kind_has_no_handle(KIND_SCANOUT_RELEASED));
        assert!(!kind_has_no_handle(KIND_READY));
    }

    #[test]
    fn add_then_replace_returns_the_old_event() {
        let mut r = reg(4, 4);
        assert_eq!(r.add(1, 10, KIND_READY, 100), Added::New);
        assert_eq!(r.add(1, 10, KIND_READY, 101), Added::Replaced(100));
        assert_eq!(r.len(), 1);
        // The same event again is still a replacement: the caller drops one of
        // the two references it now holds.
        assert_eq!(r.add(1, 10, KIND_READY, 101), Added::Replaced(101));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn the_key_is_owner_handle_and_kind() {
        let mut r = reg(8, 8);
        assert_eq!(r.add(1, 10, KIND_READY, 1), Added::New);
        assert_eq!(r.add(2, 10, KIND_READY, 2), Added::New);
        assert_eq!(r.add(1, 11, KIND_READY, 3), Added::New);
        assert_eq!(r.add(1, 10, KIND_LOST, 4), Added::New);
        assert_eq!(r.len(), 4);
        assert_eq!(r.remove(1, 10, KIND_READY), Some(1));
        assert_eq!(r.remove(1, 10, KIND_READY), None);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn total_and_owner_quotas_hold_and_replacement_needs_no_room() {
        let mut r = reg(3, 2);
        assert_eq!(r.add(1, 1, KIND_READY, 1), Added::New);
        assert_eq!(r.add(1, 2, KIND_READY, 2), Added::New);
        assert_eq!(r.add(1, 3, KIND_READY, 3), Added::OwnerFull);
        assert_eq!(r.add(2, 3, KIND_READY, 3), Added::New);
        assert_eq!(r.add(3, 4, KIND_READY, 4), Added::TotalFull);
        // Full table, owner at quota: replacing still works.
        assert_eq!(r.add(1, 1, KIND_READY, 9), Added::Replaced(1));
        assert_eq!(r.len(), 3);
        // A removal makes room again.
        assert_eq!(r.remove(2, 3, KIND_READY), Some(3));
        assert_eq!(r.add(3, 4, KIND_READY, 4), Added::New);
    }

    #[test]
    fn a_zero_sized_table_refuses_everything() {
        let mut r = reg(0, 0);
        assert_eq!(r.add(1, 1, KIND_READY, 1), Added::TotalFull);
        assert!(r.is_empty());
        assert_eq!(r.take_any(), None);
    }

    #[test]
    fn storage_is_never_reallocated() {
        let mut r = reg(16, 16);
        let before = r.regs.as_ptr();
        for i in 0..16 {
            assert_eq!(r.add(1, i, KIND_READY, i), Added::New);
        }
        assert_eq!(r.add(1, 99, KIND_READY, 99), Added::TotalFull);
        assert_eq!(r.regs.as_ptr(), before);
    }

    #[test]
    fn ready_signals_only_the_handle_and_kind_asked() {
        let mut r = reg(8, 8);
        r.add(1, 10, KIND_READY, 1);
        r.add(1, 11, KIND_READY, 2);
        r.add(1, 10, KIND_LOST, 3);
        r.add(2, 20, KIND_READY, 4);
        let mut hit = StdVec::new();
        assert_eq!(r.signal_handle(10, KIND_READY, |e| hit.push(e)), 1);
        assert_eq!(hit, [1]);
        hit.clear();
        assert_eq!(r.signal_handle(99, KIND_READY, |e| hit.push(e)), 0);
        assert!(hit.is_empty());
        // The lost registration is on handle 10 but of the other kind.
        assert_eq!(r.signal_handle(10, KIND_LOST, |e| hit.push(e)), 1);
        assert_eq!(hit, [3]);
    }

    #[test]
    fn a_scanout_release_wakes_only_the_owner_that_registered_for_it() {
        let mut r = reg(8, 8);
        r.add(1, 0, KIND_SCANOUT_RELEASED, 1);
        r.add(2, 0, KIND_SCANOUT_RELEASED, 2);
        r.add(1, 0, KIND_LOST, 3);
        r.add(1, 10, KIND_READY, 4);
        let mut hit = StdVec::new();
        assert_eq!(
            r.signal_owner_kind(1, KIND_SCANOUT_RELEASED, |e| hit.push(e)),
            1
        );
        assert_eq!(hit, [1]);
        hit.clear();
        assert_eq!(
            r.signal_owner_kind(3, KIND_SCANOUT_RELEASED, |e| hit.push(e)),
            0
        );
        assert!(hit.is_empty());
        // The loss of the transport still wakes it, with every other kind.
        assert_eq!(r.signal_all(|e| hit.push(e)), 4);
        // Re-registering replaces (same owner, handle 0, kind).
        assert_eq!(r.add(1, 0, KIND_SCANOUT_RELEASED, 9), Added::Replaced(1));
        // A user Close of an unrelated handle takes none of the handle-less ones.
        assert_eq!(r.take_for_handle(1, 10), Some(4));
        assert_eq!(r.take_for_handle(1, 10), None);
    }

    #[test]
    fn lost_signals_every_registration_of_every_kind() {
        let mut r = reg(8, 8);
        r.add(1, 10, KIND_READY, 1);
        r.add(2, 20, KIND_READY, 2);
        r.add(2, 0, KIND_LOST, 3);
        let mut hit = StdVec::new();
        assert_eq!(r.signal_all(|e| hit.push(e)), 3);
        hit.sort();
        assert_eq!(hit, [1, 2, 3]);
    }

    #[test]
    fn close_takes_every_kind_on_that_handle_for_that_owner_only() {
        let mut r = reg(8, 8);
        r.add(1, 10, KIND_READY, 1);
        r.add(1, 10, KIND_LOST, 2);
        r.add(2, 10, KIND_READY, 3);
        r.add(1, 11, KIND_READY, 4);
        let mut got = StdVec::new();
        while let Some(e) = r.take_for_handle(1, 10) {
            got.push(e);
        }
        got.sort();
        assert_eq!(got, [1, 2]);
        assert_eq!(r.len(), 2);
        assert_eq!(r.take_for_handle(1, 10), None);
    }

    #[test]
    fn a_taken_over_handle_loses_every_owners_registrations_and_nothing_else() {
        let mut r = reg(8, 8);
        r.add(1, 10, KIND_READY, 1);
        r.add(2, 10, KIND_READY, 3);
        r.add(1, 11, KIND_READY, 4);
        r.add(1, 0, KIND_LOST, 5);
        let mut got = StdVec::new();
        while let Some(e) = r.take_any_for_handle(10) {
            got.push(e);
        }
        got.sort();
        assert_eq!(got, [1, 3]);
        assert_eq!(r.len(), 2);
        assert_eq!(r.take_any_for_handle(10), None);
        assert_eq!(r.take_any_for_handle(0), None, "handle 0 is the LOST key");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn owner_teardown_takes_exactly_that_owners_events() {
        let mut r = reg(8, 8);
        r.add(1, 10, KIND_READY, 1);
        r.add(1, 11, KIND_READY, 2);
        r.add(2, 20, KIND_READY, 3);
        let mut got = StdVec::new();
        while let Some(e) = r.take_for_owner(1) {
            got.push(e);
        }
        got.sort();
        assert_eq!(got, [1, 2]);
        assert_eq!(r.count_for_owner(1), 0);
        assert_eq!(r.count_for_owner(2), 1);
        // Transport teardown takes the rest.
        assert_eq!(r.take_any(), Some(3));
        assert_eq!(r.take_any(), None);
    }

    #[test]
    fn every_event_added_comes_back_exactly_once() {
        // The leak check: whatever mix of replace / remove / take, the events
        // handed back plus the ones left account for every reference added.
        let mut r = reg(32, 32);
        let mut refs_in = 0usize;
        let mut refs_out = 0usize;
        for i in 0..20u32 {
            match r.add(1 + (i % 3) as usize, i % 5, 1 + (i % 2), i) {
                Added::New => refs_in += 1,
                Added::Replaced(_) => {
                    refs_in += 1;
                    refs_out += 1;
                }
                _ => {}
            }
        }
        for h in 0..5 {
            while r.take_for_handle(1, h).is_some() {
                refs_out += 1;
            }
        }
        while r.take_for_owner(2).is_some() {
            refs_out += 1;
        }
        while r.take_any().is_some() {
            refs_out += 1;
        }
        assert_eq!(refs_in, refs_out);
        assert!(r.is_empty());
    }
}

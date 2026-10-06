//! Flip retirement across a device restart (docs/zero-copy-present.md, "DWM after a device
//! restart"): the pure decisions.
//!
//! dxgkrnl retires a queued flip when a `DXGK_INTERRUPT_CRTC_VSYNC` carries the flip's NEW
//! `PhysicalAddress`, and (flip queue depth 1, `MaxQueuedFlipOnVSync`) issues the next flip only
//! after that. A `pnputil /restart-device` is StopDevice then StartDevice on the same adapter
//! object and the same loaded image, while dxgkrnl keeps its own flip and VidPn state: a flip it
//! had issued and not yet seen retired when the device stopped is still the one it waits for. The
//! KMD used to zero `last_primary_address` (the address every CRTC_VSYNC carries) at both
//! StopDevice and StartDevice, so after the restart the heartbeat ran and reported 0 and nothing
//! could ever name that flip's address: DWM's flips queued behind it, `FlipIss` froze, the
//! compositor stopped at 0 CPU.
//!
//! Four rules, all argument-only:
//!
//! * [`seed_address`]: after a restart the heartbeat reports the NEWEST address dxgkrnl issued (a
//!   flip it may still wait for; if that flip was already retired the address is simply the one
//!   dxgkrnl believes is displayed), never zero.
//! * [`worker_dead_exit`]: the programming worker's refusals that name no live source complete
//!   the flip as a kept picture, whatever the allocation class (a handle that no longer resolves,
//!   a producer that was abandoned), because nothing can ever bind them.
//! * [`needs_worker_signal`]: a programming already pending when StartDevice ends wakes the
//!   worker once; its own wait does not know about it.
//! * [`choose_seed`]: `pnputil /restart-device` RELOADS the driver image (hardware: `StartN` 1
//!   after each restart, every static zero at the new start), so the newest issued address of the
//!   old image lives only in the service key (`RestIssLo` / `RestIssHi`, with the uptime of the
//!   write and a check word). The new image takes it as the seed unless the knob is off, a static
//!   already holds a newer one, the machine rebooted since, or the stored words are damaged.

/// The address the heartbeat reports right after a restart: the newest address dxgkrnl issued in
/// any earlier generation (`SetVidPnSourceAddress`, or a DMA flip record), 0 if it never issued
/// one. An address names a segment location, not a transport object, so it stays meaningful
/// across a restart; the displayed IDENTITY (`active_scanout_resource`, the host binding) is a
/// different matter and is still cleared.
pub const fn seed_address(newest_issued: u64) -> u64 {
    newest_issued
}

// ---- the seed that survives an image reload (service key) --------------------------------------

/// The knob (`RestSeed`, default 1): 0 = the v329 behaviour (statics only), anything else = on.
pub const NAME_KNOB: &[u8] = b"RestSeed";
/// The persisted words: the newest issued address (low and high dword), the uptime in whole
/// seconds when they were written, and the check word over all three. Written in this order
/// with the check LAST, so a write torn by a crash reads as damaged (`USE_INSANE`).
pub const NAME_ISS_LO: &[u8] = b"RestIssLo";
pub const NAME_ISS_HI: &[u8] = b"RestIssHi";
pub const NAME_UPTIME: &[u8] = b"RestUpS";
pub const NAME_CHECK: &[u8] = b"RestChk";

/// `RestSeedUse`: nothing was persisted and no static holds an address, or the knob is off.
pub const USE_NONE: u32 = 0;
/// The persisted address was used as the seed (and fed to `LAST_ISSUED`).
pub const USE_PERSISTED: u32 = 1;
/// Rejected: written in an earlier boot (its uptime is later than this boot's now).
pub const USE_STALE: u32 = 2;
/// Rejected: unaligned, not a physical address (bit 52 and above), or the check word does not
/// match (a torn or hand-edited value).
pub const USE_INSANE: u32 = 3;
/// A static of this image already holds an address (the image was NOT reloaded): the persisted
/// value was not consulted for the seed.
pub const USE_STATIC: u32 = 4;

/// A physical address is below 2^52 on every x64 CPU the guest can run on.
pub const ADDRESS_BITS: u32 = 52;

/// An address a flip can name: nonzero, page aligned, a physical address.
pub const fn sane_address(address: u64) -> bool {
    address != 0 && address & 0xFFF == 0 && address >> ADDRESS_BITS == 0
}

/// The knob value in force: 0 off, any other value on.
pub const fn clamp_knob(raw: u32) -> u32 {
    (raw != 0) as u32
}

/// Whole seconds of interrupt time (100 ns units), saturating. Interrupt time restarts from zero
/// at every boot, which is what tells a reboot from a driver restart.
pub const fn uptime_seconds(now_100ns: u64) -> u32 {
    let s = now_100ns / 10_000_000;
    if s > u32::MAX as u64 {
        u32::MAX
    } else {
        s as u32
    }
}

/// The check word of a persisted pair.
pub const fn persist_check(address: u64, uptime_s: u32) -> u32 {
    (address as u32)
        ^ ((address >> 32) as u32).rotate_left(13)
        ^ uptime_s.rotate_left(27)
        ^ 0xC0FF_EE11
}

/// The four words as stored: low, high, uptime seconds, check.
pub const fn persist_words(address: u64, uptime_s: u32) -> [u32; 4] {
    [
        address as u32,
        (address >> 32) as u32,
        uptime_s,
        persist_check(address, uptime_s),
    ]
}

/// What was read back from the service key (absent values read as 0).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Persisted {
    pub address: u64,
    pub uptime_s: u32,
    pub check: u32,
}

impl Persisted {
    pub const NONE: Persisted = Persisted {
        address: 0,
        uptime_s: 0,
        check: 0,
    };

    pub const fn from_words(lo: u32, hi: u32, uptime_s: u32, check: u32) -> Self {
        Persisted {
            address: (lo as u64) | ((hi as u64) << 32),
            uptime_s,
            check,
        }
    }
}

/// The seed choice and why (`RestSeedUse`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SeedChoice {
    /// The newest issued address the new generation starts from (0 = none).
    pub address: u64,
    /// One of the `USE_*` codes.
    pub reason: u32,
}

/// The newest issued address at StartDevice: `static_issued` (this image's `LAST_ISSUED`) when it
/// is nonzero (max-of: the image was not reloaded, nothing newer can exist), else the persisted
/// one when it is sane, checks out, and was written in THIS boot (`persisted.uptime_s` not later
/// than `now_uptime_s`: a boot resets the clock, and a value of an earlier boot names a flip
/// queue that no longer exists). With the knob off the answer is the static alone (v329).
///
/// An empty persisted value (nothing was ever written) is `USE_NONE`, not an error. A rejected
/// one (`USE_STALE`, `USE_INSANE`) is the caller's to erase, so that a LATER, longer boot cannot
/// accept it by its uptime.
pub const fn choose_seed(
    knob_on: bool,
    static_issued: u64,
    persisted: Persisted,
    now_uptime_s: u32,
) -> SeedChoice {
    if !knob_on {
        return SeedChoice {
            address: seed_address(static_issued),
            reason: USE_NONE,
        };
    }
    if static_issued != 0 {
        return SeedChoice {
            address: seed_address(static_issued),
            reason: USE_STATIC,
        };
    }
    if persisted.address == 0 {
        return SeedChoice {
            address: 0,
            reason: USE_NONE,
        };
    }
    if !sane_address(persisted.address)
        || persisted.check != persist_check(persisted.address, persisted.uptime_s)
    {
        return SeedChoice {
            address: 0,
            reason: USE_INSANE,
        };
    }
    if persisted.uptime_s > now_uptime_s {
        return SeedChoice {
            address: 0,
            reason: USE_STALE,
        };
    }
    SeedChoice {
        address: persisted.address,
        reason: USE_PERSISTED,
    }
}

/// At most one persisted write per this long outside StopDevice (100 ns units, 2 s).
pub const PERSIST_MIN_INTERVAL_100NS: u64 = 20_000_000;

/// Whether the newest issued address must be written to the service key now: it is sane, differs
/// from what was last written (or read back and used), and `force` (StopDevice) or the minimum
/// interval since the last write has passed. The worker asks on every pass, so the common answer
/// costs two compares.
pub const fn persist_due(
    current: u64,
    persisted: u64,
    now_100ns: u64,
    last_write_100ns: u64,
    force: bool,
) -> bool {
    if !sane_address(current) || current == persisted {
        return false;
    }
    force || now_100ns.saturating_sub(last_write_100ns) >= PERSIST_MIN_INTERVAL_100NS
}

/// Bit 0: a worker programming handle was pending. Bit 1: the programming gate was raised.
pub const fn pending_flags(pending_handle: bool, gate_raised: bool) -> u32 {
    (pending_handle as u32) | ((gate_raised as u32) << 1)
}

/// `ScRestPend`: the flags found at StopDevice entry (bits 0 and 1) and at StartDevice entry
/// (bits 2 and 3), so one value says whether a programming survived either edge.
pub const fn pending_breadcrumb(at_stop: u32, at_start: u32) -> u32 {
    (at_stop & 3) | ((at_start & 3) << 2)
}

/// Whether StartDevice must wake the worker before it returns to dxgkrnl: a handle is pending, or
/// the gate is raised (a raised gate with an empty slot is a copy completion's, which wakes
/// itself, but one extra pass is free and the worker drops the stale case itself).
pub const fn needs_worker_signal(pending_handle: bool, gate_raised: bool) -> bool {
    pending_handle || gate_raised
}

/// Why the worker's programming had no live source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeadKind {
    /// The handle resolves to no allocation of this transport generation (destroyed, stale
    /// generation, never ours): `scanout_alloc_info` answered `None`.
    NoSuchAllocation,
    /// The producer or the host resource is gone (`ScanoutReject::ProducerAbandoned`): the
    /// resource is not live, a destroy barrier is up, or the exact producer boundary was purged.
    ProducerAbandoned,
}

/// The address to publish as a kept picture for a flip that can never bind, whatever its class.
/// `allocation_address`: the address Windows paired with the handle, when the handle still
/// resolves (an allocation context of the current or an older generation that is still alive).
/// `newest_issued`: the newest flip address dxgkrnl issued, for a handle that no longer resolves
/// (nothing else names the flip; with queue depth 1 the flip being retired is the newest one).
/// `None`: nothing to publish (zero is "nothing assigned").
pub const fn worker_dead_exit(
    kind: DeadKind,
    allocation_address: Option<u64>,
    newest_issued: u64,
) -> Option<u64> {
    let address = match allocation_address {
        Some(a) if a != 0 => a,
        _ => match kind {
            DeadKind::NoSuchAllocation => newest_issued,
            // A resolvable allocation with a zero address was never flipped to: nothing owed.
            DeadKind::ProducerAbandoned => 0,
        },
    };
    if address == 0 {
        None
    } else {
        Some(address)
    }
}

/// `ScRestHi`: the bits 32..39 of three addresses packed in one value, for addresses above 4 GiB
/// (the counters carry only the low 32 bits): stop-entry address in bits 16..23, newest issued
/// in bits 8..15, start-exit address in bits 0..7.
pub const fn high_bytes(at_stop: u64, newest_issued: u64, at_start_exit: u64) -> u32 {
    ((((at_stop >> 32) & 0xFF) as u32) << 16)
        | ((((newest_issued >> 32) & 0xFF) as u32) << 8)
        | (((at_start_exit >> 32) & 0xFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seed_is_the_newest_issued_address_and_never_invented() {
        assert_eq!(seed_address(0), 0);
        assert_eq!(seed_address(0x1_2345_6000), 0x1_2345_6000);
        // The one thing the old code did: zero, whatever dxgkrnl had issued.
        assert_ne!(seed_address(0x4000), 0);
    }

    #[test]
    fn pending_flags_and_breadcrumb_pack_each_edge_separately() {
        assert_eq!(pending_flags(false, false), 0);
        assert_eq!(pending_flags(true, false), 1);
        assert_eq!(pending_flags(false, true), 2);
        assert_eq!(pending_flags(true, true), 3);
        assert_eq!(pending_breadcrumb(0, 0), 0);
        assert_eq!(pending_breadcrumb(3, 0), 3);
        assert_eq!(pending_breadcrumb(0, 3), 12);
        assert_eq!(pending_breadcrumb(1, 2), 1 | (2 << 2));
        // Out-of-range bits never leak into the other edge.
        assert_eq!(pending_breadcrumb(0xFF, 0), 3);
        assert_eq!(pending_breadcrumb(0, 0xFF), 12);
    }

    #[test]
    fn the_worker_is_signalled_for_any_programming_state_that_survived() {
        assert!(!needs_worker_signal(false, false));
        assert!(needs_worker_signal(true, false));
        assert!(needs_worker_signal(false, true));
        assert!(needs_worker_signal(true, true));
    }

    #[test]
    fn a_dead_source_completes_with_its_own_address_when_it_has_one() {
        for kind in [DeadKind::NoSuchAllocation, DeadKind::ProducerAbandoned] {
            assert_eq!(worker_dead_exit(kind, Some(0x7000), 0x9000), Some(0x7000));
        }
    }

    #[test]
    fn a_handle_that_no_longer_resolves_completes_the_newest_issued_flip() {
        let kind = DeadKind::NoSuchAllocation;
        assert_eq!(worker_dead_exit(kind, None, 0x9000), Some(0x9000));
        // An allocation that resolves but carries a zero address falls to the same newest flip.
        assert_eq!(worker_dead_exit(kind, Some(0), 0x9000), Some(0x9000));
    }

    #[test]
    fn nothing_is_published_for_an_address_that_was_never_assigned() {
        assert_eq!(worker_dead_exit(DeadKind::NoSuchAllocation, None, 0), None);
        assert_eq!(worker_dead_exit(DeadKind::ProducerAbandoned, None, 0x9000), None);
        assert_eq!(worker_dead_exit(DeadKind::ProducerAbandoned, Some(0), 0x9000), None);
    }

    #[test]
    fn every_dead_flip_of_every_class_completes_when_it_has_any_address() {
        // The invariant: the completion does not look at the allocation class at all (the
        // function has no class parameter), so a Venus, foreign or hollow flip is treated alike.
        for (kind, alloc, newest) in [
            (DeadKind::NoSuchAllocation, None, 5u64),
            (DeadKind::NoSuchAllocation, Some(6), 5),
            (DeadKind::ProducerAbandoned, Some(6), 0),
        ] {
            assert!(worker_dead_exit(kind, alloc, newest).is_some());
        }
    }

    #[test]
    fn high_bytes_keep_the_three_addresses_apart() {
        assert_eq!(high_bytes(0, 0, 0), 0);
        assert_eq!(high_bytes(0x1_0000_0000, 0, 0), 1 << 16);
        assert_eq!(high_bytes(0, 0x2_0000_0000, 0), 2 << 8);
        assert_eq!(high_bytes(0, 0, 0x3_0000_0000), 3);
        assert_eq!(
            high_bytes(0xAB_0000_0000, 0xCD_0000_0000, 0xEF_0000_0000),
            0x00AB_CDEF
        );
        // Bits above 40 are dropped, not smeared into the neighbour.
        assert_eq!(high_bytes(0x1FF_0000_0000, 0, 0), 0xFF << 16);
    }

    const GOOD: u64 = 0x1_2345_6000;

    fn stored(address: u64, uptime_s: u32) -> Persisted {
        let w = persist_words(address, uptime_s);
        Persisted::from_words(w[0], w[1], w[2], w[3])
    }

    #[test]
    fn the_seed_choice_table() {
        let ok = stored(GOOD, 100);
        let damaged = Persisted {
            check: ok.check ^ 1,
            ..ok
        };
        // (knob, static, persisted, now seconds, address, reason)
        let rows: [(bool, u64, Persisted, u32, u64, u32); 12] = [
            // The image was not reloaded: the static wins, whatever is persisted.
            (true, 0x7000, ok, 200, 0x7000, USE_STATIC),
            (true, 0x7000, Persisted::NONE, 200, 0x7000, USE_STATIC),
            // Reloaded, persisted written in this boot: used (also in the same second).
            (true, 0, ok, 200, GOOD, USE_PERSISTED),
            (true, 0, ok, 100, GOOD, USE_PERSISTED),
            // Written later than now: the machine rebooted since.
            (true, 0, ok, 99, 0, USE_STALE),
            (true, 0, ok, 0, 0, USE_STALE),
            // Never persisted.
            (true, 0, Persisted::NONE, 200, 0, USE_NONE),
            // Insane: unaligned, bit 52, damaged check.
            (true, 0, stored(GOOD + 8, 100), 200, 0, USE_INSANE),
            (true, 0, stored(1 << 52, 100), 200, 0, USE_INSANE),
            (true, 0, damaged, 200, 0, USE_INSANE),
            // The knob off is v329: the static alone, never the persisted value.
            (false, 0, ok, 200, 0, USE_NONE),
            (false, 0x7000, ok, 200, 0x7000, USE_NONE),
        ];
        for (i, (knob, st, p, now, address, reason)) in rows.into_iter().enumerate() {
            assert_eq!(
                choose_seed(knob, st, p, now),
                SeedChoice { address, reason },
                "row {i}"
            );
        }
    }

    #[test]
    fn a_damaged_value_is_not_stale_and_a_stale_one_is_not_damaged() {
        // The check comes first: a damaged value of an earlier boot reads as damaged.
        assert_eq!(
            choose_seed(true, 0, stored(GOOD + 1, 900), 5).reason,
            USE_INSANE
        );
        // A good value of an earlier boot is merely stale.
        assert_eq!(choose_seed(true, 0, stored(GOOD, 900), 5).reason, USE_STALE);
    }

    #[test]
    fn sane_addresses() {
        assert!(!sane_address(0));
        assert!(sane_address(0x1000));
        assert!(!sane_address(0x1001));
        assert!(!sane_address(0x800));
        assert!(sane_address(0x000F_FFFF_FFFF_F000));
        assert!(!sane_address(1 << 52));
        assert!(!sane_address(0xFFFF_FFFF_FFFF_F000));
        assert!(!sane_address(0x0010_0000_0000_1000));
    }

    #[test]
    fn the_words_round_trip_and_a_torn_write_reads_as_damaged() {
        let w = persist_words(GOOD, 321);
        assert_eq!(w[0], 0x2345_6000);
        assert_eq!(w[1], 1);
        assert_eq!(w[2], 321);
        let p = Persisted::from_words(w[0], w[1], w[2], w[3]);
        assert_eq!(p.address, GOOD);
        assert_eq!(choose_seed(true, 0, p, 321).address, GOOD);
        // The writer stores low, high, uptime, check in this order: a crash after any prefix
        // leaves the OLD check word against new words, which must not pass.
        let old = persist_words(0x9000, 50);
        let torn = [
            Persisted::from_words(w[0], old[1], old[2], old[3]),
            Persisted::from_words(w[0], w[1], old[2], old[3]),
            Persisted::from_words(w[0], w[1], w[2], old[3]),
        ];
        for (i, t) in torn.into_iter().enumerate() {
            assert_eq!(
                choose_seed(true, 0, t, 1000).reason,
                USE_INSANE,
                "prefix {i}"
            );
        }
    }

    #[test]
    fn uptime_is_whole_seconds_and_saturates() {
        assert_eq!(uptime_seconds(0), 0);
        assert_eq!(uptime_seconds(9_999_999), 0);
        assert_eq!(uptime_seconds(10_000_000), 1);
        assert_eq!(uptime_seconds(u64::MAX), u32::MAX);
        assert_eq!(clamp_knob(0), 0);
        assert_eq!(clamp_knob(1), 1);
        assert_eq!(clamp_knob(7), 1);
    }

    #[test]
    fn the_persist_decision() {
        let t0 = 1_000_000_000u64;
        let ivl = PERSIST_MIN_INTERVAL_100NS;
        // Changed and the interval passed.
        assert!(persist_due(GOOD, 0x9000, t0 + ivl, t0, false));
        assert!(persist_due(GOOD, 0, t0 + ivl + 1, t0, false));
        // Changed but too soon: not outside a stop, yes in a stop.
        assert!(!persist_due(GOOD, 0x9000, t0 + ivl - 1, t0, false));
        assert!(persist_due(GOOD, 0x9000, t0 + 1, t0, true));
        // Unchanged: never, even forced (the registry already has it).
        assert!(!persist_due(GOOD, GOOD, t0 + 10 * ivl, t0, false));
        assert!(!persist_due(GOOD, GOOD, t0 + 10 * ivl, t0, true));
        // Nothing issued, or an address a flip cannot name: never written.
        assert!(!persist_due(0, GOOD, t0 + 10 * ivl, t0, true));
        assert!(!persist_due(GOOD + 4, 0, t0 + 10 * ivl, t0, true));
        assert!(!persist_due(1 << 60, 0, t0 + 10 * ivl, t0, true));
        // A clock that went backwards does not underflow.
        assert!(!persist_due(GOOD, 0, 5, t0, false));
    }

    #[test]
    fn the_names_fit_the_lookup_buffer_and_do_not_collide() {
        let names = [NAME_KNOB, NAME_ISS_LO, NAME_ISS_HI, NAME_UPTIME, NAME_CHECK];
        for (i, a) in names.iter().enumerate() {
            assert!(a.len() <= 14);
            // State and knob names are not counters: the counter lists must not repeat them.
            let a = core::str::from_utf8(a).unwrap();
            assert!(!crate::stall_diag::COUNTERS.contains(&a), "{a}");
            for b in &names[i + 1..] {
                assert_ne!(a.as_bytes(), *b);
            }
        }
    }
}

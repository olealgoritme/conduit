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
//! Three rules, all argument-only:
//!
//! * [`seed_address`]: after a restart the heartbeat reports the NEWEST address dxgkrnl issued (a
//!   flip it may still wait for; if that flip was already retired the address is simply the one
//!   dxgkrnl believes is displayed), never zero.
//! * [`worker_dead_exit`]: the programming worker's refusals that name no live source complete
//!   the flip as a kept picture, whatever the allocation class (a handle that no longer resolves,
//!   a producer that was abandoned), because nothing can ever bind them.
//! * [`needs_worker_signal`]: a programming already pending when StartDevice ends wakes the
//!   worker once; its own wait does not know about it.

/// The address the heartbeat reports right after a restart: the newest address dxgkrnl issued in
/// any earlier generation (`SetVidPnSourceAddress`, or a DMA flip record), 0 if it never issued
/// one. An address names a segment location, not a transport object, so it stays meaningful
/// across a restart; the displayed IDENTITY (`active_scanout_resource`, the host binding) is a
/// different matter and is still cleared.
pub const fn seed_address(newest_issued: u64) -> u64 {
    newest_issued
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
}

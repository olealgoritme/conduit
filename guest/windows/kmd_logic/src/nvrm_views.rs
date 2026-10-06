//! Keys and ids of the user views `HELIOS_NVRM_OP_MMAP` makes, in the mapping table
//! the KMD shares with blob views.
//!
//! A view of host BAR memory lives in `AdapterContext::mappings`, which outlives
//! the virtio transport (a view can only be unmapped inside the process that made
//! it, so a `StopDevice` cannot touch it). The ids that name the views are minted
//! by the transport, and a transport that is replaced starts a new generation:
//!
//! * ids must therefore be MONOTONIC across generations. If a new transport
//!   restarted at 1, a surviving device handle's old view would sit on the very key
//!   the new generation's first `MMAP` wants (`insert_unique` would refuse it), and
//!   a stale `MUNMAP` could name a live mapping of the new generation;
//! * a view whose id is below the first id of the current generation belongs to a
//!   transport that no longer exists ("stale"): the host mapping it pointed at is
//!   gone, and its owner unmaps it the next time it calls into the KMD.
//!
//! The rules are pure functions of their arguments so the host tests can pin them.

/// NVRM views are keyed `id | KEY_BIT`: the high bit keeps them apart from blob
/// views, which are keyed by a small resource id.
pub const KEY_BIT: u32 = 0x8000_0000;

/// Minted ids stay below this. Above it live the fixed keys other view kinds use
/// in the same table (`READ_LEDGER_MAPPING_ID` = `u32::MAX`, `PRODUCER_MAPPING_ID`
/// = `u32::MAX - 1`), whose low 31 bits are `0x7FFF_FFFF` and `0x7FFF_FFFE`.
pub const ID_LIMIT: u32 = 0x7FFF_FFF0;

/// The key of view `id` in the shared table.
pub const fn key(id: u32) -> u32 {
    id | KEY_BIT
}

/// The id of an NVRM view key, or `None` for any other kind of key (a blob's
/// resource id, or one of the fixed pseudo ids).
pub const fn id_of_key(key: u32) -> Option<u32> {
    let id = key & !KEY_BIT;
    if key & KEY_BIT != 0 && id != 0 && id < ID_LIMIT {
        Some(id)
    } else {
        None
    }
}

/// Whether `key` names an NVRM view minted before `below` (the first id of the
/// current transport generation): a view of a transport that is gone. Never true
/// for a key that is not an NVRM view's.
pub const fn is_stale(key: u32, below: u32) -> bool {
    match id_of_key(key) {
        Some(id) => id < below,
        None => false,
    }
}

/// The counter value after minting `current`, or `None` when `current` is not a
/// mintable id (0, or the id space is used up). A monotonic counter minted with
/// `fetch_update(|n| successor(n))` hands out `1, 2, 3, ...` once each, never wraps
/// into the key bit and never yields 0.
pub const fn successor(current: u32) -> Option<u32> {
    if current == 0 || current >= ID_LIMIT {
        None
    } else {
        Some(current + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_stay_apart_from_blob_ids() {
        for id in [1u32, 2, 77, ID_LIMIT - 1] {
            assert_eq!(id_of_key(key(id)), Some(id));
        }
        // A blob's resource id has no key bit: never an NVRM view.
        for blob in [1u32, 5, 0x7FFF_FFF0, 0x7FFF_FFFF] {
            assert_eq!(id_of_key(blob), None);
            assert!(!is_stale(blob, u32::MAX));
        }
    }

    #[test]
    fn the_fixed_pseudo_ids_are_not_nvrm_views() {
        // `mapping.rs`: READ_LEDGER_MAPPING_ID and PRODUCER_MAPPING_ID.
        for fixed in [u32::MAX, u32::MAX - 1] {
            assert_eq!(id_of_key(fixed), None);
            assert!(!is_stale(fixed, u32::MAX));
        }
        // Id 0 under the key bit is not a view either (0 is "none").
        assert_eq!(id_of_key(KEY_BIT), None);
        // The first key above the mintable range is not one.
        assert_eq!(id_of_key(key(ID_LIMIT)), None);
    }

    #[test]
    fn stale_means_minted_before_the_current_generation() {
        let base = 10;
        assert!(is_stale(key(1), base));
        assert!(is_stale(key(9), base));
        assert!(!is_stale(key(10), base));
        assert!(!is_stale(key(11), base));
        // A zero base (no generation boundary recorded) marks nothing stale.
        assert!(!is_stale(key(1), 0));
    }

    #[test]
    fn minting_is_monotonic_and_bounded() {
        let mut next = 1u32;
        let mut last = 0u32;
        let mut count = 0u32;
        // Walk the whole space in big steps by jumping near the end.
        for _ in 0..1000 {
            let minted = next;
            next = successor(next).expect("mintable");
            assert!(minted > last);
            assert_eq!(id_of_key(key(minted)), Some(minted));
            last = minted;
            count += 1;
        }
        assert_eq!(count, 1000);
        // The last mintable id is ID_LIMIT - 1; the counter then refuses.
        assert_eq!(successor(ID_LIMIT - 1), Some(ID_LIMIT));
        assert_eq!(successor(ID_LIMIT), None);
        assert_eq!(successor(u32::MAX), None);
        assert_eq!(successor(0), None);
    }

    #[test]
    fn a_new_generation_never_reuses_an_old_key() {
        // Generation 1 mints 1..=3 and goes away; the counter carries on, so
        // generation 2's first id is above every stale one.
        let mut next = 1u32;
        let mut gen1 = [0u32; 3];
        for slot in gen1.iter_mut() {
            *slot = next;
            next = successor(next).unwrap();
        }
        let base = next; // recorded when generation 1 is dropped
        let first_of_gen2 = next;
        assert!(gen1.iter().all(|&id| is_stale(key(id), base)));
        assert!(!is_stale(key(first_of_gen2), base));
        assert!(gen1.iter().all(|&id| key(id) != key(first_of_gen2)));
    }
}

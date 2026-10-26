//! Portable policy for the idle bound-buffer fast path: keep an idle requested
//! buffer only if it is not older than the last selected/composed DWM frame.
//! `None` means no prior selection; pending buffers are rejected. This helper
//! sees only the selected frame, not host presentation feedback.
//!
//! Chain identity: `chain == 0` is the legacy unknown token and matches only
//! another zero, so known and unknown producers never share bound history.

/// Return whether the idle requested bound buffer may be kept.
/// Accepts when there is no prior selection or `completed >= last_selected` for
/// monotonic sequence numbers. A pending buffer is always rejected.
pub fn keep_bound(idle: bool, completed: u64, last_selected: Option<u64>) -> bool {
    if !idle {
        return false;
    }

    match last_selected {
        None => true,
        Some(last) => completed >= last,
    }
}

/// Return whether a candidate belongs to the same producer chain as the bound
/// buffer. Owner and chain token must both match; zero is legacy unknown and
/// matches only zero, never a known token.
pub fn same_chain(
    bound_owner: u32,
    bound_chain: u32,
    candidate_owner: u32,
    candidate_chain: u32,
) -> bool {
    bound_owner == candidate_owner && bound_chain == candidate_chain
}

/// Stable key for per-chain history. Legacy `chain == 0` keys by owner alone;
/// a known token sets bit 32, so no legacy owner can alias known history.
pub fn history_key(owner: u32, chain: u32) -> u64 {
    if chain == 0 {
        u64::from(owner)
    } else {
        (1u64 << 32) | u64::from(chain)
    }
}

#[cfg(test)]
mod tests {
    use super::{history_key, keep_bound, same_chain};

    // Recorded DWM regression: selected 225, then idle bound 224 arrived.
    // Keeping 224 regresses the sequence, so it must be rejected; repeating 225
    // and advancing to 226 remain safe.
    #[test]
    fn idle_bound_older_than_selected_frame_is_rejected() {
        let selected = 225;
        assert!(!keep_bound(true, 224, Some(selected)));
        assert!(keep_bound(true, 225, Some(selected)));
        assert!(keep_bound(true, 226, Some(selected)));
    }

    /// A valid requested idle frame is preserved, not skipped for a newer one.
    #[test]
    fn valid_requested_idle_frame_is_preserved() {
        let requested = 224;
        assert!(keep_bound(true, requested, Some(223)));
    }

    /// Pending buffers are rejected even with a high completion value; `None`
    /// means no prior selection only for idle buffers.
    #[test]
    fn pending_bound_is_rejected_regardless_of_completed_value() {
        assert!(!keep_bound(false, 999, Some(223)));
        assert!(!keep_bound(false, 999, None));
        assert!(keep_bound(true, 999, None));
    }

    // Same process and geometry, different chain tokens: an owner-only check
    // mixed these and replayed the wrong frame history.
    #[test]
    fn different_chain_tokens_do_not_mix() {
        assert!(!same_chain(42, 100, 42, 101));
        assert!(same_chain(42, 100, 42, 100));
    }

    /// Known and legacy-unknown chains are isolated; a different owner never
    /// matches, even with the same token.
    #[test]
    fn legacy_and_known_chains_are_isolated() {
        assert!(!same_chain(42, 0, 42, 100));
        assert!(!same_chain(42, 100, 42, 0));
        assert!(!same_chain(42, 0, 43, 0));
        assert!(!same_chain(42, 100, 43, 100));
        assert!(same_chain(42, 0, 42, 0));
    }

    /// Known keys occupy a separate u64 space; a legacy owner equal to a token
    /// (including u32::MAX) cannot alias that token's history.
    #[test]
    fn history_keys_do_not_alias_across_legacy_and_known() {
        let token = 42;
        assert_ne!(history_key(token, 0), history_key(token, token));
        assert_ne!(history_key(u32::MAX, 0), history_key(u32::MAX, u32::MAX));
        assert_ne!(history_key(token, u32::MAX), history_key(u32::MAX, 0));
        assert_eq!(history_key(token, 0), u64::from(token));
    }
}

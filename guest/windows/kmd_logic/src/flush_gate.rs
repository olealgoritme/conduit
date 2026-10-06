//! The pure rules of the flush gate record (`HEFL`, `guest/windows/docs/flush-gate.md`).
//!
//! `DxgkDdiRender` parses the record and asks [`plan`] what the packet's WDDM fence is
//! to retire on and what becomes of the RM fence handle in the tail. The function is
//! argument-only: the caller (kmd_render) turns `flags` into the booleans below with
//! the constants of `helios_protocol`, which this crate does not depend on.
//!
//! The rules are those of the other advisory carriers (`HERF` / `HEPR`), because the
//! caller is a D3D11 `pfnFlush`, which must not be failed over bookkeeping:
//!
//! * a record is never refused; what the KMD cannot honour degrades to
//!   [`Carrier::Wire`], the legacy wire-prefix rule, and is told apart in [`Degrade`]
//!   so it can be counted;
//! * the two markers are exclusive: with both, a complete stream point wins;
//! * a parsed fence tail that does not become the carrier is still the KMD's
//!   ([`Plan::take_tail`]): the UMD cannot learn a refusal, so it must not be left
//!   owing a handle it can no longer close.

/// What the packet's WDDM fence retires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    /// Nothing of its own: the legacy rule (every transport entry enqueued before
    /// `SubmitCommand`, GPU completion included).
    Wire,
    /// A point of the process's registered Venus producer stream.
    Stream {
        ctx_id: u32,
        value: u32,
        cookie: u64,
    },
    /// The RM fence of the tail, to be attached as a point of the process's gate.
    Fence,
}

/// Why a request did not become the carrier it asked for (counted by the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degrade {
    /// It did become what it asked for (or asked for the wire rung).
    None,
    /// A flag bit this version does not define.
    UnknownFlags,
    /// `STREAM` without a complete `(ctx_id, cookie)`.
    StreamIncomplete,
    /// `RM_FENCE` with no handle in the tail.
    FenceMissing,
    /// Both markers were asked for; the stream point was honoured.
    BothMarkers,
}

/// The parsed record, flags already split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub want_stream: bool,
    pub want_fence: bool,
    /// The record carries a flag bit outside `HELIOS_FLUSH_GATE_FLAGS_ALL`.
    pub unknown_flags: bool,
    pub ctx_id: u32,
    pub value: u32,
    pub cookie: u64,
    pub tail_handle: u32,
    pub tail_flags: u32,
}

/// The decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub carrier: Carrier,
    /// Take the tail's handle (when it is a fence of this process) and close it
    /// without attaching it: it was parsed and is not the carrier. Never set for
    /// [`Carrier::Fence`], whose attach routine takes the handle itself on a refusal.
    pub take_tail: bool,
    pub degraded: Degrade,
}

/// A complete stream tail: a context and its registration cookie. `value` may be 0
/// (the stream must be live, nothing is waited for), exactly as for a present marker.
#[inline]
pub const fn stream_complete(ctx_id: u32, cookie: u64) -> bool {
    ctx_id != 0 && cookie != 0
}

/// Decide what one record asks of the KMD. See the module doc for the rules.
pub const fn plan(r: Request) -> Plan {
    // A tail is "parsed" when it says anything (the refusal rule of HERF / HEPR).
    let tail_present = r.tail_handle != 0 || r.tail_flags != 0;
    if r.unknown_flags {
        return Plan {
            carrier: Carrier::Wire,
            take_tail: tail_present,
            degraded: Degrade::UnknownFlags,
        };
    }
    match (r.want_stream, r.want_fence) {
        (true, both) => {
            if stream_complete(r.ctx_id, r.cookie) {
                Plan {
                    carrier: Carrier::Stream {
                        ctx_id: r.ctx_id,
                        value: r.value,
                        cookie: r.cookie,
                    },
                    take_tail: tail_present,
                    degraded: if both {
                        Degrade::BothMarkers
                    } else {
                        Degrade::None
                    },
                }
            } else {
                Plan {
                    carrier: Carrier::Wire,
                    take_tail: tail_present,
                    degraded: Degrade::StreamIncomplete,
                }
            }
        }
        (false, true) => {
            if r.tail_handle != 0 {
                Plan {
                    carrier: Carrier::Fence,
                    take_tail: false,
                    degraded: Degrade::None,
                }
            } else {
                Plan {
                    carrier: Carrier::Wire,
                    take_tail: tail_present,
                    degraded: Degrade::FenceMissing,
                }
            }
        }
        (false, false) => Plan {
            carrier: Carrier::Wire,
            take_tail: tail_present,
            degraded: Degrade::None,
        },
    }
}

/// The wire-fence id a flush packet is stamped with when it has no boundary of its own
/// (`Carrier::Wire`, a degrade, a merge error, a boundary the buffer did not keep):
/// the last fence this transport generation has issued, `next_wire_fence - 1`, so the
/// packet's watermark (`id + 1`, an exclusive prefix) is every transport entry enqueued
/// before the Render.
///
/// Why it is stamped at all: dxgkrnl recycles the DMA buffer's private data and
/// `SubmitCommand` only peeks at the Present prefix, so a record left by an earlier
/// Present of the context would otherwise be inherited by the packet. A stale
/// `gpu_fence_id` becomes the watermark (waits only up to that old id), a stale live
/// same-stream boundary selects the exact-present-watermark arm (watermark 0, no wire
/// wait). Naming a fence of its own wins over both: `note_wddm_submission` evaluates
/// the `gpu_completion_fence` arm before the stream relaxation.
///
/// `None` when this generation has issued nothing (`next_wire_fence <= wire_fence_base`):
/// there is no fence to name and nothing to wait for; an id of a previous generation is
/// clamped to the full prefix, which is empty here (`wddm_boundary::select`).
pub const fn wire_floor(wire_fence_base: u64, next_wire_fence: u64) -> Option<u64> {
    if next_wire_fence <= wire_fence_base || next_wire_fence < 2 {
        return None;
    }
    Some(next_wire_fence - 1)
}

/// Whether the boundary the packet asked for is the one its private record holds after
/// the merge (`requested` is what was merged in, `merged` what the record carries now).
///
/// `false` means the packet must be stamped with [`wire_floor`]: the buffer kept an
/// older record's boundary and dropped this flush's wait (a different handle with both
/// waiting, `PrBndDrop`), or the requested boundary is unusable. Same handle: the
/// record holds the larger value, which only waits longer. A requested value of 0
/// ("already complete") waits for nothing, so whatever the buffer kept satisfies it.
pub fn boundary_kept(requested: u64, merged: u64) -> bool {
    use crate::present_stream::decode_boundary;
    let Some((rh, rv)) = decode_boundary(requested) else {
        return false;
    };
    if rv == 0 {
        return true;
    }
    matches!(decode_boundary(merged), Some((mh, mv)) if mh == rh && mv >= rv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request {
            want_stream: false,
            want_fence: false,
            unknown_flags: false,
            ctx_id: 0,
            value: 0,
            cookie: 0,
            tail_handle: 0,
            tail_flags: 0,
        }
    }

    #[test]
    fn no_flags_is_the_wire_rung_and_takes_nothing() {
        let p = plan(req());
        assert_eq!(p.carrier, Carrier::Wire);
        assert!(!p.take_tail);
        assert_eq!(p.degraded, Degrade::None);
    }

    #[test]
    fn stream_point_is_carried_verbatim() {
        let p = plan(Request {
            want_stream: true,
            ctx_id: 3,
            value: 41,
            cookie: 0xABCD,
            ..req()
        });
        assert_eq!(
            p.carrier,
            Carrier::Stream {
                ctx_id: 3,
                value: 41,
                cookie: 0xABCD
            }
        );
        assert!(!p.take_tail);
        assert_eq!(p.degraded, Degrade::None);
    }

    #[test]
    fn value_zero_is_a_complete_stream_tail() {
        // "Already complete": the stream must be live, nothing is waited for.
        assert!(stream_complete(1, 1));
        let p = plan(Request {
            want_stream: true,
            ctx_id: 1,
            value: 0,
            cookie: 1,
            ..req()
        });
        assert!(matches!(p.carrier, Carrier::Stream { value: 0, .. }));
    }

    #[test]
    fn incomplete_stream_tails_degrade_to_wire() {
        for (ctx_id, cookie) in [(0, 5), (5, 0), (0, 0)] {
            let p = plan(Request {
                want_stream: true,
                ctx_id,
                cookie,
                value: 9,
                ..req()
            });
            assert_eq!(p.carrier, Carrier::Wire);
            assert_eq!(p.degraded, Degrade::StreamIncomplete);
            assert!(!stream_complete(ctx_id, cookie));
        }
    }

    #[test]
    fn fence_variant_asks_the_caller_to_attach() {
        let p = plan(Request {
            want_fence: true,
            tail_handle: 7,
            tail_flags: 1,
            ..req()
        });
        assert_eq!(p.carrier, Carrier::Fence);
        // The attach routine takes the handle itself on a refusal; planning it too
        // would take it twice.
        assert!(!p.take_tail);
        assert_eq!(p.degraded, Degrade::None);
    }

    #[test]
    fn fence_variant_without_a_handle_degrades() {
        let p = plan(Request {
            want_fence: true,
            tail_flags: 1,
            ..req()
        });
        assert_eq!(p.carrier, Carrier::Wire);
        assert_eq!(p.degraded, Degrade::FenceMissing);
        // Flags alone name no handle, but the tail said something: the caller's
        // claim check decides, and a zero handle is never a fence of the process.
        assert!(p.take_tail);
    }

    #[test]
    fn both_markers_honour_the_stream_and_take_the_fence() {
        let p = plan(Request {
            want_stream: true,
            want_fence: true,
            ctx_id: 2,
            value: 5,
            cookie: 6,
            tail_handle: 7,
            tail_flags: 1,
            ..req()
        });
        assert!(matches!(p.carrier, Carrier::Stream { value: 5, .. }));
        assert!(p.take_tail);
        assert_eq!(p.degraded, Degrade::BothMarkers);
    }

    #[test]
    fn both_markers_with_a_partial_stream_take_the_fence_and_go_wire() {
        let p = plan(Request {
            want_stream: true,
            want_fence: true,
            ctx_id: 2,
            cookie: 0,
            tail_handle: 7,
            tail_flags: 1,
            ..req()
        });
        assert_eq!(p.carrier, Carrier::Wire);
        assert!(p.take_tail);
        assert_eq!(p.degraded, Degrade::StreamIncomplete);
    }

    #[test]
    fn unknown_flags_mean_no_boundary_but_the_handle_is_still_taken() {
        let p = plan(Request {
            want_stream: true,
            unknown_flags: true,
            ctx_id: 2,
            cookie: 3,
            value: 4,
            tail_handle: 7,
            ..req()
        });
        assert_eq!(p.carrier, Carrier::Wire);
        assert!(p.take_tail);
        assert_eq!(p.degraded, Degrade::UnknownFlags);
    }

    #[test]
    fn a_tail_without_its_flag_is_taken_not_attached() {
        // The UMD wrote a handle but not RM_FENCE: the KMD must not leave the UMD
        // holding something it believes it gave away, nor attach what was not asked.
        let p = plan(Request {
            tail_handle: 7,
            tail_flags: 1,
            ..req()
        });
        assert_eq!(p.carrier, Carrier::Wire);
        assert!(p.take_tail);
        assert_eq!(p.degraded, Degrade::None);
    }

    #[test]
    fn stream_fields_are_ignored_without_the_stream_flag() {
        let p = plan(Request {
            ctx_id: 2,
            value: 5,
            cookie: 6,
            ..req()
        });
        assert_eq!(p.carrier, Carrier::Wire);
        assert!(!p.take_tail);
    }
    fn enc(handle: u32, value: u32) -> u64 {
        crate::present_stream::encode_boundary(handle, value)
    }

    #[test]
    fn wire_floor_names_the_last_issued_fence_of_this_generation() {
        let base = 1 + (3u64 << 32);
        assert_eq!(wire_floor(base, base), None);
        assert_eq!(wire_floor(base, base + 1), Some(base));
        assert_eq!(wire_floor(base, base + 40), Some(base + 39));
        // A degenerate base: never name fence 0, the "no dependency" id.
        assert_eq!(wire_floor(0, 0), None);
        assert_eq!(wire_floor(0, 1), None);
        assert_eq!(wire_floor(1, 2), Some(1));
        // next below base cannot happen; it must not underflow into a bogus id.
        assert_eq!(wire_floor(base, base - 1), None);
    }

    #[test]
    fn wire_floor_is_accepted_by_the_boundary_table_as_a_prefix_of_everything_issued() {
        use crate::wddm_boundary::{select, Kind, Rejection};
        let (base, next) = (1 + (3u64 << 32), 1 + (3u64 << 32) + 40);
        let id = wire_floor(base, next).unwrap();
        let s = select(id, base, next, false);
        assert_eq!(s.rejection, Rejection::Accepted);
        assert_eq!(s.kind, Kind::Prefix);
        // Exactly "every transport entry enqueued before": the legacy watermark.
        assert_eq!(s.watermark, next);
    }

    #[test]
    fn a_stale_id_would_have_weakened_the_watermark_and_the_floor_does_not() {
        use crate::wddm_boundary::select;
        let (base, next) = (1 + (3u64 << 32), 1 + (3u64 << 32) + 40);
        // What a recycled prefix carried before the floor existed.
        let stale = select(base + 2, base, next, false);
        assert_eq!(stale.watermark, base + 3);
        // The merge keeps the larger gpu_fence_id, so the floor wins over any stale id
        // of this generation (ids are monotonic).
        assert!(wire_floor(base, next).unwrap() + 1 > stale.watermark);
    }

    #[test]
    fn a_merged_boundary_is_kept_when_it_is_the_requested_one_or_a_larger_of_its_handle() {
        assert!(boundary_kept(enc(5, 9), enc(5, 9)));
        assert!(boundary_kept(enc(5, 9), enc(5, 12)));
        // Gate handles share the namespace.
        assert!(boundary_kept(enc(0x82, 1), enc(0x82, 1)));
    }

    #[test]
    fn a_boundary_the_buffer_replaced_with_another_handles_wait_is_not_kept() {
        // Old record of a different handle with a real wait: the merge keeps the old
        // one and drops this flush's (`PrBndDrop`).
        assert!(!boundary_kept(enc(5, 9), enc(7, 3)));
        // The same handle at a lower value cannot come out of a merge, but if it did
        // the flush's point is not covered.
        assert!(!boundary_kept(enc(5, 9), enc(5, 8)));
        // Record emptied or untagged.
        assert!(!boundary_kept(enc(5, 9), 0));
        assert!(!boundary_kept(enc(5, 9), 12345));
    }

    #[test]
    fn value_zero_waits_for_nothing_so_whatever_the_buffer_kept_satisfies_it() {
        assert!(boundary_kept(enc(5, 0), enc(5, 0)));
        assert!(boundary_kept(enc(5, 0), enc(7, 3)));
        assert!(boundary_kept(enc(5, 0), 0));
    }

    #[test]
    fn an_unusable_requested_boundary_is_never_kept() {
        assert!(!boundary_kept(0, enc(5, 1)));
        assert!(!boundary_kept(12345, enc(5, 1)));
    }
}

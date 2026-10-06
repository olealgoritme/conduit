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
}

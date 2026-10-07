//! DWM restart and a stale Explorer: the pure half of the `Dw*` breadcrumb block and the one
//! decision the investigation proved wrong. The I/O half is `kmd_render/src/ddi/dwm_restart.rs`;
//! the analysis, the counters and the hardware checklist are in `docs/zero-copy-present.md`,
//! "DWM restart and stale Explorer".
//!
//! WHY. After `dwm.exe` is killed and restarted (the DWM device is destroyed and a new one
//! created) Explorer's taskbar clock stays frozen and its desktop icons are missing until
//! `explorer.exe` is restarted. The registry counters that existed could not say WHICH of the
//! candidate mechanisms ran: `PrUnrWhy` keeps only the last unresolved Blt, the present-buffer
//! busy counters were never mirrored, and nothing recorded what `DestroyDevice` left behind. The
//! block below is the instrument; this file holds its names (one table, so a name can neither
//! exceed the 14 characters the service key lookup keeps nor collide with another), the census
//! verdicts and a few pure helpers, all functions of their arguments.
//!
//! The one DECISION is [`importer_retire_id`]: destroying a WDDM allocation that only IMPORTS the
//! adapter-owned LINEAR scanout target (a DWM generation that is gone) must not retire that
//! resource from the host scanout. It used to, which unbound the screen of a live desktop.

/// Longest service-key value name the driver keeps (`diag::record_named_bytes` clamps to it).
pub const MAX_NAME: usize = 14;

macro_rules! counters {
    ($($variant:ident => $name:literal,)+) => {
        /// One word of the block. The variant is the index into the render crate's atomics.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(usize)]
        pub enum Ctr {
            $($variant,)+
        }

        /// The registry names, in [`Ctr`] order.
        pub const NAMES: &[&str] = &[$($name,)+];

        /// Every word, in [`Ctr`] order (so a loop over the block never needs a cast from a
        /// number).
        pub const ALL: [Ctr; NAMES.len()] = [$(Ctr::$variant,)+];
    };
}

// Kinds, in the comment of each group: K = cumulative since StartDevice; W = a WINDOW, zeroed at
// every `DestroyDevice` (read right after the DWM restart, with no other GPU process coming or
// going, or `DwDevDel` says another device died); L = last value written.
counters! {
    // ---- the device lifecycle (K unless noted) ----
    DevNew => "DwDevNew",       // CreateDevice calls
    DevDel => "DwDevDel",       // DestroyDevice calls
    Reuse => "DwReuse",         // CreateDevice returned the address of one of the last 4 destroyed devices
    DelMs => "DwDelMs",         // L: the last DestroyDevice, milliseconds
    DelMsMax => "DwDelMsMax",   // the longest DestroyDevice, milliseconds
    DelBlobs => "DwDelBlob",    // L: blobs the last DestroyDevice reclaimed
    DelCtxs => "DwDelCtx",      // L: Venus contexts it reclaimed
    DelStrms => "DwDelStrm",    // L: present streams it purged
    CtxFail => "DwCtxFail",     // CTX_DESTROY round trips that did not succeed (the stream slots stay closing)
    CtxFin => "DwCtxFin",       // closing stream slots finalized by a successful CTX_DESTROY
    ImpDest => "DwImpDest",     // importers of the adapter-owned scanout destroyed: host unbind skipped
    // ---- the census (L): what the dead device left behind ----
    PbExt => "DwPbExt",         // present buffers owned by EXTERNAL (idle, writable)
    PbCons => "DwPbCons",       // ... claimed by a consumer (a read in flight or a dead one)
    PbConsDd => "DwPbConsDd",   // ... of those, claimed through a stream that is NOT live
    PbWr => "DwPbWr",           // ... held by the KMD writer or its CPU mirror
    PsLive => "DwPsLive",       // live present-stream slots
    PsClose => "DwPsClose",     // closing present-stream slots (waiting for a CTX_DESTROY)
    WbPend => "DwWbPend",       // windowed Blt requests pending
    WbReady => "DwWbReady",     // windowed Blt READY queue length
    WbHead => "DwWbHead",       // why the READY head is not dispatched: `HeadBlock::code`
    Wedge => "DwWedge",         // the verdict of the last four censuses, 4 bits each (`Wedge::code`)
    CenN => "DwCenN",           // censuses taken since the last DestroyDevice
    // ---- the Present window (W) ----
    PrN => "DwPrN",             // DxgkDdiPresent calls
    PrFail => "DwPrFail",       // ... that returned a failure
    PrOk => "DwPrOk",           // Blts copied or queued
    PrSkip => "DwPrSkip",       // Blts completed without a copy, any reason
    PrUnr => "DwPrUnr",         // ... of those, an unresolved handle (`PrUnres`)
    PrCol => "DwPrCol",         // ... of those, a ColorFill with no source (`PrColFill`)
    OkMs => "DwOkMs",           // milliseconds from the destroy to the first copied Blt, plus 1 (0 = none yet)
    UnrMs => "DwUnrMs",         // ... to the first unresolved Blt, plus 1
    RunNow => "DwRunNow",       // Blts skipped in a row right now (no copy since)
    RunMax => "DwRunMax",       // the longest such run
    UnrSrc => "DwUnrSrc",       // unresolved SOURCE causes 1..4 (null, not ours, old generation, no identity), a byte each
    UnrDst => "DwUnrDst",       // the same for the DESTINATION
    UnrAdp => "DwUnrAdp",       // Blts whose adapter did not resolve
    // ---- flips (W) ----
    FlipN => "DwFlipN",         // SetVidPnSourceAddress calls
    FlipBad => "DwFlipBad",     // ... that returned STATUS_INVALID_PARAMETER (the handle pairs with nothing)
    FlipMs => "DwFlipMs",       // milliseconds from the destroy to the first one, plus 1
    // ---- opens (W) ----
    OpFail => "DwOpFail",       // OpenAllocation refused by the liveness gate (the resource died)
    OpFailId => "DwOpFailId",   // L: the resource id it named
    OpNoId => "DwOpNoId",       // opens that recorded no identity
    OpNoIdSz => "DwOpNoIdSz",   // L: the private data sizes of the last one, `info | call << 16`
    // ---- the screen (L, taken at the end of the destroy, and derived at publish) ----
    ActRes => "DwActRes",       // the resource bound to the host scanout at the end of the destroy
    RfPost => "DwRfPost",       // refreshes queued since the destroy (derived)
    UnavPost => "DwUnavPost",   // refreshes dropped for an unbound scanout since the destroy (derived)
    // ---- present-buffer counters that were never mirrored (K) ----
    WrBusy => "DwWrBusy",       // PRESENT_BUFFER_WRITE_BUSY
    RdBusy => "DwRdBusy",       // PRESENT_BUFFER_READ_BUSY
    SyncRej => "DwSyncRej",     // PRESENT_BUFFER_SYNC_REJECTS
    RdClaim => "DwRdClaim",     // PRESENT_BUFFER_READ_CLAIMS
}

/// How many words the block has.
pub const COUNT: usize = NAMES.len();

impl Ctr {
    /// The registry name of this word.
    pub const fn name(self) -> &'static str {
        NAMES[self as usize]
    }
}

/// The words zeroed at every `DestroyDevice` (the WINDOW, see the table above).
pub const WINDOW: &[Ctr] = &[
    Ctr::PrN,
    Ctr::PrFail,
    Ctr::PrOk,
    Ctr::PrSkip,
    Ctr::PrUnr,
    Ctr::PrCol,
    Ctr::OkMs,
    Ctr::UnrMs,
    Ctr::RunNow,
    Ctr::RunMax,
    Ctr::UnrSrc,
    Ctr::UnrDst,
    Ctr::UnrAdp,
    Ctr::FlipN,
    Ctr::FlipBad,
    Ctr::FlipMs,
    Ctr::OpFail,
    Ctr::OpFailId,
    Ctr::OpNoId,
    Ctr::OpNoIdSz,
    Ctr::CenN,
    Ctr::Wedge,
];

/// A small ring of the addresses of the last destroyed devices. `DeviceOwner` is the address of
/// the `DeviceContext` box, so a new DWM device can get the address of the one that just died:
/// anything keyed by the token and left behind would then be matched by the new device. `reuse`
/// says how often that happened; it is evidence only (every table keyed by the token is cleared
/// by `DestroyDevice`), and `0` is never recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestroyedRing {
    addrs: [usize; Self::LEN],
    next: usize,
}

impl DestroyedRing {
    /// How many destroyed addresses are remembered.
    pub const LEN: usize = 4;

    pub const fn new() -> Self {
        Self {
            addrs: [0; Self::LEN],
            next: 0,
        }
    }

    /// A device at `addr` was destroyed.
    pub fn note_destroyed(&mut self, addr: usize) {
        if addr == 0 {
            return;
        }
        self.addrs[self.next] = addr;
        self.next = (self.next + 1) % Self::LEN;
    }

    /// A device was created at `addr`: whether that is one of the last destroyed addresses. The
    /// entry is consumed (a reuse is counted once per destroy).
    pub fn take_reuse(&mut self, addr: usize) -> bool {
        if addr == 0 {
            return false;
        }
        for slot in self.addrs.iter_mut() {
            if *slot == addr {
                *slot = 0;
                return true;
            }
        }
        false
    }
}

impl Default for DestroyedRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Milliseconds from `then` to `now`, both interrupt time in 100 ns units, saturating at 0 and at
/// `u32::MAX`. `then == 0` ("never") reads 0.
pub const fn ms_between(now_100ns: u64, then_100ns: u64) -> u32 {
    if then_100ns == 0 || now_100ns <= then_100ns {
        return 0;
    }
    let ms = (now_100ns - then_100ns) / 10_000;
    if ms > u32::MAX as u64 {
        u32::MAX
    } else {
        ms as u32
    }
}

/// `ms_between` plus one, so that a value of 0 can mean "did not happen yet" and the first event
/// within the same millisecond as the destroy still reads nonzero.
pub const fn ms_marker(now_100ns: u64, then_100ns: u64) -> u32 {
    ms_between(now_100ns, then_100ns).saturating_add(1)
}

/// Add one to byte `index` (0..4) of a packed saturating histogram.
pub const fn hist_bump(packed: u32, index: usize) -> u32 {
    if index >= 4 {
        return packed;
    }
    let shift = (index * 8) as u32;
    let byte = (packed >> shift) & 0xFF;
    if byte == 0xFF {
        packed
    } else {
        packed + (1 << shift)
    }
}

/// The histogram slot of an unresolved-handle cause (`present_foreign::HandleCause` as its number:
/// 1 null, 2 not ours, 3 old generation, 4 no identity). `Resolved` (0) and unknown causes have no
/// slot.
pub const fn cause_slot(cause: u32) -> Option<usize> {
    match cause {
        1..=4 => Some(cause as usize - 1),
        _ => None,
    }
}

/// The destination of a destroyed WDDM allocation's host retirement.
///
/// `resource_id` is the id the allocation carries; `dedicated` the adapter-owned LINEAR scanout
/// target's. An allocation that carries the dedicated id is an IMPORTER of it (the KMD made the
/// resource itself; a DWM generation imported it to blit into): its destroy must not retire the
/// resource from the host scanout, whose `SET_SCANOUT_BLOB(0)` blanked a live desktop and left no
/// bind until the next `SetVidPnSourceAddress`. The retirement of the exact allocation HANDLE
/// (a deferred flip that names it) is unaffected: this returns the resource id to pass on, `0`
/// meaning "handle only".
pub const fn importer_retire_id(resource_id: u32, dedicated: u32) -> u32 {
    if resource_id != 0 && resource_id == dedicated {
        0
    } else {
        resource_id
    }
}

/// Why the head of the windowed Blt READY queue is not being dispatched, as `DwWbHead`. The order
/// is `take_ready_windowed_blt`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadBlock {
    /// The READY queue is empty.
    NoHead,
    /// The head token names no pending request (dropped by the next worker pass).
    StaleToken,
    /// The request is not admitted yet: SubmitCommand has not seen its DMA buffer.
    NotAdmitted,
    /// Its present stream has not retired the boundary yet and is live (a producer still running).
    BoundaryPending,
    /// Its present stream is gone: only the discharge sweeps ever move it.
    BoundaryDead,
    /// The destination buffer is not acquirable (a consumer, a writer or a mirror holds it).
    DestinationBusy,
    /// Nothing holds it: the next pass dispatches it.
    Ready,
}

impl HeadBlock {
    pub const fn code(self) -> u32 {
        match self {
            Self::NoHead => 0,
            Self::StaleToken => 1,
            Self::NotAdmitted => 2,
            Self::BoundaryPending => 3,
            Self::BoundaryDead => 4,
            Self::DestinationBusy => 5,
            Self::Ready => 6,
        }
    }
}

/// What the worker would find at the head, from the facts `take_ready_windowed_blt` reads.
/// `entry` is `None` when `pending` has no request with the head's token, else its
/// `(admitted, dispatched)`.
pub fn head_blocker(
    head: Option<u64>,
    entry: Option<(bool, bool)>,
    boundary_ready: bool,
    boundary_live: bool,
    destination_busy: bool,
) -> HeadBlock {
    let Some(_) = head else {
        return HeadBlock::NoHead;
    };
    let Some((admitted, dispatched)) = entry else {
        return HeadBlock::StaleToken;
    };
    if dispatched {
        return HeadBlock::StaleToken;
    }
    if !admitted {
        return HeadBlock::NotAdmitted;
    }
    if !boundary_ready {
        return if boundary_live {
            HeadBlock::BoundaryPending
        } else {
            HeadBlock::BoundaryDead
        };
    }
    if destination_busy {
        return HeadBlock::DestinationBusy;
    }
    HeadBlock::Ready
}

/// What the present machinery holds at one moment (the census), counted under the transport lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    pub buffers_external: u32,
    pub buffers_consumer: u32,
    /// Of `buffers_consumer`, claimed through a stream slot that is not live.
    pub buffers_consumer_dead: u32,
    /// Held by the KMD writer or its CPU mirror.
    pub buffers_kmd: u32,
    pub streams_live: u32,
    /// Live slots that are `closing`: revoked, waiting for the host to confirm a CTX_DESTROY.
    pub streams_closing: u32,
    pub blt_pending: u32,
    pub blt_ready: u32,
    pub head: Option<HeadBlock>,
}

/// The verdict of one census, as a hint. The states it names are also the NORMAL transient states
/// of a busy desktop (a writer in flight for a millisecond), so it means something only when the
/// follow-up censuses (about 2 s apart) agree: `DwWedge` keeps the last four.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wedge {
    /// Nothing is pinned.
    Clear,
    /// A buffer is claimed through a stream that no longer exists: it never becomes writable.
    DeadConsumer,
    /// Stream slots wait for a CTX_DESTROY that did not confirm; their consumer claims stay pinned.
    ClosingStreams,
    /// The READY head waits on a buffer that is busy, or on a stream boundary that cannot retire.
    HeadBlocked,
    /// A buffer is held by the KMD writer or mirror.
    KmdHolds,
}

impl Wedge {
    pub const fn code(self) -> u32 {
        match self {
            Self::Clear => 0,
            Self::DeadConsumer => 1,
            Self::ClosingStreams => 2,
            Self::HeadBlocked => 3,
            Self::KmdHolds => 4,
        }
    }
}

/// Classify a census. The order is the order of certainty: a dead consumer is a wedge by itself,
/// closing slots are its precursor, a blocked head is where the user sees it, and a KMD holder is
/// the weakest hint.
pub fn wedge(c: &Census) -> Wedge {
    if c.buffers_consumer_dead != 0 {
        return Wedge::DeadConsumer;
    }
    if c.streams_closing != 0 {
        return Wedge::ClosingStreams;
    }
    if matches!(
        c.head,
        Some(HeadBlock::DestinationBusy | HeadBlock::BoundaryDead | HeadBlock::BoundaryPending)
    ) {
        return Wedge::HeadBlocked;
    }
    if c.buffers_kmd != 0 {
        return Wedge::KmdHolds;
    }
    Wedge::Clear
}

/// `DwWedge`: the last four verdicts, 4 bits each, the newest in the low nibble.
pub const fn push_verdict(packed: u32, verdict: u32) -> u32 {
    ((packed << 4) | (verdict & 0xF)) & 0xFFFF
}

/// When the follow-up censuses run, after a `DestroyDevice` (100 ns units): at once (taken by
/// the destroy itself), then three more. The worker takes them; `next_followup` is the time of
/// the next one or `None` when all ran.
pub const FOLLOWUP_GAP_100NS: u64 = 20_000_000;
/// How many follow-ups after the census the destroy itself takes.
pub const FOLLOWUPS: u32 = 3;

pub const fn next_followup(left: u32, last_100ns: u64) -> Option<u64> {
    if left == 0 {
        None
    } else {
        Some(last_100ns.saturating_add(FOLLOWUP_GAP_100NS))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_name_fits_the_service_key_lookup_and_is_unique() {
        let mut seen = HashSet::new();
        for name in NAMES {
            assert!(
                !name.is_empty() && name.len() <= MAX_NAME,
                "{name} is {} characters",
                name.len()
            );
            assert!(
                name.bytes().all(|b| b.is_ascii_alphanumeric()),
                "{name} has a character the registry value name should not"
            );
            assert!(name.starts_with("Dw"), "{name} is outside the Dw prefix");
            assert!(
                seen.insert(name.to_ascii_lowercase()),
                "{name} appears twice (case-insensitively)"
            );
        }
        assert_eq!(NAMES.len(), COUNT);
        assert_eq!(ALL.len(), COUNT);
        for (i, c) in ALL.iter().enumerate() {
            assert_eq!(*c as usize, i, "{} is out of place in ALL", c.name());
        }
    }

    #[test]
    fn variants_index_their_own_name() {
        assert_eq!(Ctr::DevNew.name(), "DwDevNew");
        assert_eq!(Ctr::RdClaim.name(), "DwRdClaim");
        assert_eq!(Ctr::RdClaim as usize, COUNT - 1);
        assert_eq!(Ctr::DevNew as usize, 0);
    }

    #[test]
    fn window_names_are_in_the_table_once() {
        let mut seen = HashSet::new();
        for c in WINDOW {
            assert!(seen.insert(*c as usize), "{} twice in WINDOW", c.name());
        }
        // The cumulative lifecycle words and the derived screen words are not windowed.
        for c in [
            Ctr::DevNew,
            Ctr::DevDel,
            Ctr::Reuse,
            Ctr::CtxFail,
            Ctr::ImpDest,
            Ctr::WrBusy,
        ] {
            assert!(!seen.contains(&(c as usize)), "{} must not be windowed", c.name());
        }
    }

    #[test]
    fn a_new_device_at_a_destroyed_address_is_a_reuse_once() {
        let mut ring = DestroyedRing::new();
        assert!(!ring.take_reuse(0x1000));
        ring.note_destroyed(0x1000);
        ring.note_destroyed(0);
        assert!(ring.take_reuse(0x1000));
        assert!(!ring.take_reuse(0x1000), "consumed");
        assert!(!ring.take_reuse(0));
    }

    #[test]
    fn the_ring_forgets_the_oldest_after_four_destroys() {
        let mut ring = DestroyedRing::new();
        for a in [0x10usize, 0x20, 0x30, 0x40, 0x50] {
            ring.note_destroyed(a);
        }
        assert!(!ring.take_reuse(0x10), "the oldest was overwritten");
        for a in [0x20usize, 0x30, 0x40, 0x50] {
            assert!(ring.take_reuse(a));
        }
    }

    #[test]
    fn millisecond_helpers_saturate_and_mark_zero_as_none() {
        assert_eq!(ms_between(10_000_000, 0), 0, "never");
        assert_eq!(ms_between(5, 10), 0, "clock behind");
        assert_eq!(ms_between(20_000_000, 10_000_000), 1000);
        assert_eq!(ms_between(u64::MAX, 1), u32::MAX);
        assert_eq!(ms_marker(10_000_000, 10_000_000), 1, "same instant is 1, not none");
        assert_eq!(ms_marker(20_000_000, 10_000_000), 1001);
        assert_eq!(ms_marker(u64::MAX, 1), u32::MAX);
    }

    #[test]
    fn the_histogram_counts_each_cause_and_saturates_a_byte() {
        let mut h = 0u32;
        h = hist_bump(h, 0);
        h = hist_bump(h, 3);
        h = hist_bump(h, 3);
        assert_eq!(h, 0x0200_0001);
        for _ in 0..300 {
            h = hist_bump(h, 0);
        }
        assert_eq!(h & 0xFF, 0xFF);
        assert_eq!(h >> 8, 0x0002_0000, "the others are untouched");
        assert_eq!(hist_bump(h, 4), h, "out of range changes nothing");
        assert_eq!(cause_slot(0), None);
        assert_eq!(cause_slot(1), Some(0));
        assert_eq!(cause_slot(4), Some(3));
        assert_eq!(cause_slot(5), None);
    }

    #[test]
    fn an_importer_of_the_scanout_target_retires_by_handle_only() {
        assert_eq!(importer_retire_id(0x121, 0x121), 0);
        assert_eq!(importer_retire_id(0x122, 0x121), 0x122);
        // No dedicated target yet, or an allocation with no resource: unchanged.
        assert_eq!(importer_retire_id(0x122, 0), 0x122);
        assert_eq!(importer_retire_id(0, 0), 0);
        assert_eq!(importer_retire_id(0, 0x121), 0);
    }

    #[test]
    fn the_head_blocker_follows_the_dispatch_order() {
        use HeadBlock::*;
        assert_eq!(head_blocker(None, None, true, true, false), NoHead);
        assert_eq!(head_blocker(Some(7), None, true, true, false), StaleToken);
        assert_eq!(
            head_blocker(Some(7), Some((true, true)), true, true, false),
            StaleToken
        );
        assert_eq!(
            head_blocker(Some(7), Some((false, false)), true, true, false),
            NotAdmitted
        );
        assert_eq!(
            head_blocker(Some(7), Some((true, false)), false, true, false),
            BoundaryPending
        );
        assert_eq!(
            head_blocker(Some(7), Some((true, false)), false, false, false),
            BoundaryDead
        );
        assert_eq!(
            head_blocker(Some(7), Some((true, false)), true, true, true),
            DestinationBusy
        );
        assert_eq!(
            head_blocker(Some(7), Some((true, false)), true, true, false),
            Ready
        );
        // The codes are owner-readable ABI: pin them.
        assert_eq!(
            [NoHead, StaleToken, NotAdmitted, BoundaryPending, BoundaryDead, DestinationBusy, Ready]
                .map(HeadBlock::code),
            [0, 1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn the_verdict_names_the_strongest_pinned_state() {
        let mut c = Census::default();
        assert_eq!(wedge(&c), Wedge::Clear);
        c.buffers_kmd = 1;
        assert_eq!(wedge(&c), Wedge::KmdHolds);
        c.head = Some(HeadBlock::DestinationBusy);
        assert_eq!(wedge(&c), Wedge::HeadBlocked);
        c.streams_closing = 2;
        assert_eq!(wedge(&c), Wedge::ClosingStreams);
        c.buffers_consumer = 1;
        c.buffers_consumer_dead = 1;
        assert_eq!(wedge(&c), Wedge::DeadConsumer);
        // A head that is only waiting for the next worker pass is not a wedge.
        let idle = Census {
            head: Some(HeadBlock::Ready),
            buffers_consumer: 1,
            ..Census::default()
        };
        assert_eq!(wedge(&idle), Wedge::Clear);
        assert_eq!(
            [Wedge::Clear, Wedge::DeadConsumer, Wedge::ClosingStreams, Wedge::HeadBlocked, Wedge::KmdHolds]
                .map(Wedge::code),
            [0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn the_verdict_history_keeps_the_last_four_nibbles() {
        let mut h = 0;
        for v in [1u32, 2, 3, 4, 1] {
            h = push_verdict(h, v);
        }
        assert_eq!(h, 0x2341);
        assert_eq!(push_verdict(0xFFFF, 0x1F), 0xFFFF);
    }

    #[test]
    fn followups_run_three_times_two_seconds_apart() {
        assert_eq!(next_followup(0, 5), None);
        assert_eq!(next_followup(FOLLOWUPS, 100), Some(100 + FOLLOWUP_GAP_100NS));
        assert_eq!(FOLLOWUP_GAP_100NS, 2 * 10_000_000);
        assert_eq!(next_followup(1, u64::MAX), Some(u64::MAX));
    }
}

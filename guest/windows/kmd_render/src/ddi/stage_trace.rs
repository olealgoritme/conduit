//! Per-frame stage timestamps (`StageTrace`, default 0): the statics, the stamp sites' entry
//! points and the publish. The layout and the pure rules are `helios_kmd_logic::stage_trace`
//! (host-tested); the host side, the collector and how to read its table are `docs/TRACING.md`,
//! "Frame stage timing"; which stage is stamped where is in `docs/kmd-handoff-2026-10.md`.
//!
//! Every stamp site calls [`on`] first: one relaxed load, and nothing else while the knob is 0
//! (no clock read, no store). With the knob on a stamp is a `fetch_add` and four atomic stores
//! into a ring of [`st::SLOTS`] slots, legal at any IRQL (no lock, no allocation).
//!
//! The ring is published as the REG_BINARY value `StgRing` (the whole ring and a header) by the
//! registry mirror's pass ([`publish`], PASSIVE), at most twice a second, only while the knob is
//! on. `StgOn` is the knob in force, `StgHead` the low 32 bits of the records ever written,
//! `StgPubN` the snapshots written.

use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::stage_trace as st;

#[allow(clippy::declare_interior_mutable_const)]
const Z64: AtomicU64 = AtomicU64::new(0);

/// `StageTrace` in force.
static ON: AtomicU32 = AtomicU32::new(0);
/// Records ever written; the next record's index.
static HEAD: AtomicU64 = AtomicU64::new(0);
static SEQ: [AtomicU64; st::SLOTS] = [Z64; st::SLOTS];
static TIME: [AtomicU64; st::SLOTS] = [Z64; st::SLOTS];
static WORD: [AtomicU64; st::SLOTS] = [Z64; st::SLOTS];
/// The interrupt time of the last ISR this driver claimed (written only while on).
static LAST_ISR: AtomicU64 = AtomicU64::new(0);
/// Set by the ForeignFlip worker just before a synchronous host flip: the programming's DDI entry
/// time, taken by [`take_flip_ddi`] in the send that carries the flip's `seq`.
static FLIP_DDI_T: AtomicU64 = AtomicU64::new(0);
/// Pipelined flips in flight: the submission time per flip tag (`flip_pipeline::tag_of`, slot
/// `tag % FLIP_SLOTS`), for the G_FLIP_ISR of an answer the used-ring drain finds (it knows only
/// the tag).
const FLIP_SLOTS: usize = 16;
#[allow(clippy::declare_interior_mutable_const)]
const Z32: AtomicU32 = AtomicU32::new(0);
static FLIP_TAG: [AtomicU32; FLIP_SLOTS] = [Z32; FLIP_SLOTS];
static FLIP_SUB_T: [AtomicU64; FLIP_SLOTS] = [Z64; FLIP_SLOTS];
/// Interrupt time (ms) of the last snapshot, and the snapshots written.
static LAST_PUB_MS: AtomicU32 = AtomicU32::new(0);
static PUB_N: AtomicU32 = AtomicU32::new(0);

/// The shortest interval between two snapshots, ms.
const PUBLISH_EVERY_MS: u32 = 500;

/// Whether stages are stamped. The one check a stamp site makes.
#[inline(always)]
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed) != 0
}

/// Interrupt time (100 ns) when on, else 0 (and no clock read). A stamp of time 0 is dropped.
#[inline]
pub(crate) fn now_if_on() -> u64 {
    if on() {
        crate::ddi::blt_async::now_100ns()
    } else {
        0
    }
}

/// Record `stage` of `id` (a wire fence id or a flip `seq`) at interrupt time `t`. Any IRQL,
/// atomics only. Nothing when off or when `t` is 0 (a time taken while off).
#[inline]
pub(crate) fn stamp(stage: u8, kind: u8, id: u64, t: u64) {
    if !on() || t == 0 {
        return;
    }
    let idx = HEAD.fetch_add(1, Ordering::Relaxed);
    let i = st::slot(idx);
    SEQ[i].store(0, Ordering::Relaxed);
    fence(Ordering::Release);
    TIME[i].store(t, Ordering::Relaxed);
    WORD[i].store(st::pack(st::id32(id), stage, kind, 0), Ordering::Relaxed);
    SEQ[i].store(idx + 1, Ordering::Release);
}

/// [`stamp`] at the current interrupt time.
#[inline]
pub(crate) fn stamp_now(stage: u8, kind: u8, id: u64) {
    if on() {
        stamp(stage, kind, id, crate::ddi::blt_async::now_100ns());
    }
}

/// The ISR claimed an interrupt. DIRQL, one relaxed load while off.
#[inline]
pub(crate) fn note_isr() {
    if on() {
        LAST_ISR.store(crate::ddi::blt_async::now_100ns(), Ordering::Relaxed);
    }
}

/// A completion of something submitted at `submitted` was found now: stamp `done_stage`, and
/// `isr_stage` at the last interrupt if it lies in between (`st::isr_for`). Any IRQL.
#[inline]
pub(crate) fn completed(isr_stage: u8, done_stage: u8, kind: u8, id: u64, submitted: u64) {
    if !on() {
        return;
    }
    let found = crate::ddi::blt_async::now_100ns();
    if let Some(t) = st::isr_for(submitted, LAST_ISR.load(Ordering::Relaxed), found) {
        stamp(isr_stage, kind, id, t);
    }
    stamp(done_stage, kind, id, found);
}

/// A pipelined flip of tag `tag` was submitted at `t` (stamped G_FLIP_SUBMIT under `seq`).
/// PASSIVE or DISPATCH, atomics only.
#[inline]
pub(crate) fn flip_submitted(seq: u64, tag: u32, t: u64) {
    if !on() || t == 0 {
        return;
    }
    let i = tag as usize % FLIP_SLOTS;
    FLIP_SUB_T[i].store(t, Ordering::Relaxed);
    FLIP_TAG[i].store(tag, Ordering::Relaxed);
    stamp(st::G_FLIP_SUBMIT, st::KIND_FLIP, seq, t);
}

/// The used-ring drain found the answer of the pipelined flip of tag `tag`: G_FLIP_ACK (and
/// G_FLIP_ISR) under the tag, which is the low 30 bits of the flip's `seq`. Any IRQL.
#[inline]
pub(crate) fn flip_answered(tag: u32) {
    if !on() {
        return;
    }
    let i = tag as usize % FLIP_SLOTS;
    let submitted = if FLIP_TAG[i].load(Ordering::Relaxed) == tag {
        FLIP_SUB_T[i].load(Ordering::Relaxed)
    } else {
        0
    };
    completed(
        st::G_FLIP_ISR,
        st::G_FLIP_ACK,
        st::KIND_FLIP,
        u64::from(tag),
        submitted,
    );
}

/// The ForeignFlip worker is about to send a synchronous flip of the programming that entered
/// the DDI at `ddi_t` (0: unknown).
#[inline]
pub(crate) fn set_flip_ddi(ddi_t: u64) {
    if on() {
        FLIP_DDI_T.store(ddi_t, Ordering::Relaxed);
    }
}

/// The DDI entry time [`set_flip_ddi`] left for the flip now being sent, once.
#[inline]
pub(crate) fn take_flip_ddi() -> u64 {
    if on() {
        FLIP_DDI_T.swap(0, Ordering::Relaxed)
    } else {
        0
    }
}

/// The knob, read at StartDevice and mirrored with the value in force. PASSIVE. The ring is not
/// cleared: its indices keep growing across generations, so a reader's position stays valid.
pub(crate) fn start_generation() {
    let v = crate::diag::read_config_dword(crate::diag::knobs::STAGE_TRACE, 0);
    ON.store(u32::from(v != 0), Ordering::Relaxed);
    LAST_ISR.store(0, Ordering::Relaxed);
    FLIP_DDI_T.store(0, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"StgOn", u32::from(v != 0));
}

/// Write the ring as `StgRing`, at most every [`PUBLISH_EVERY_MS`], only while on. PASSIVE (the
/// registry mirror's pass). A snapshot buffer that cannot be had skips this pass.
pub(crate) fn publish() {
    if !on() {
        return;
    }
    let now = crate::adapter::AdapterContext::interrupt_time_ms().max(1);
    let last = LAST_PUB_MS.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < PUBLISH_EVERY_MS {
        return;
    }
    LAST_PUB_MS.store(now, Ordering::Relaxed);
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if buf.try_reserve_exact(st::SNAPSHOT_LEN).is_err() {
        return;
    }
    buf.resize(st::SNAPSHOT_LEN, 0);
    let head = HEAD.load(Ordering::Acquire);
    st::encode_header(&mut buf, head, true);
    for i in 0..st::SLOTS {
        let seq = SEQ[i].load(Ordering::Acquire);
        let t = TIME[i].load(Ordering::Relaxed);
        let w = WORD[i].load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        // A slot a writer is in (or overwrote meanwhile) is published as never written.
        let seq = if SEQ[i].load(Ordering::Relaxed) == seq {
            seq
        } else {
            0
        };
        st::encode_slot(&mut buf, i, seq, t, w);
    }
    crate::diag::record_named_binary(b"StgRing", &buf);
    PUB_N.fetch_add(1, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"StgHead", head as u32);
    crate::diag::record_named_bytes(b"StgPubN", PUB_N.load(Ordering::Relaxed));
}

//! Per-frame stage timestamps (docs/TRACING.md "Frame stage timing").
//!
//! Every windowed-Present copy (a fenced `SUBMIT_3D`) and every flip
//! (`ScanoutFlip`) is followed through the host by the id the guest already
//! put on the wire: the copy by its fence, `(ctx_id, ring_idx, fence_id)`,
//! the flip by `ScanoutFlip::seq`. Each place the request passes stamps
//! `(stage, id, CLOCK_MONOTONIC ns)` into a lock-free ring; `conduit trace
//! NAME stages` drains it, joins it with the guest driver's own ring by the
//! same id and prints where the frame time goes.
//!
//! Off unless asked for: [`on`] is one relaxed load, and a stamp site does
//! nothing else while it is false (no clock read, no store). The ring is
//! allocated the first time tracing is turned on. `CONDUIT_STAGE_TRACE=1` in
//! the environment turns it on at start ([`init_from_env`]); `stages on` on
//! the backend's trace socket at run time.
//!
//! This module is shared by the backend, `conduit-venus` and the CLI, so the
//! stage numbers, the record layout and the dump format have one definition.
//! The guest driver's numbers are the same table (`G_*`); its ring snapshot
//! format is decoded by [`guest`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use std::sync::{Mutex, OnceLock};

/// What an id names.
pub const KIND_FENCE: u8 = 1;
/// A `ScanoutFlip`, by its `seq`.
pub const KIND_FLIP: u8 = 2;

// Guest driver (KMD), interrupt time. Windowed copy:
/// `DxgkDdiPresent` entered (Blt arm).
pub const G_PRESENT: u8 = 1;
/// The copy was queued for the worker: the producer had not finished.
pub const G_DEFER: u8 = 2;
/// About to put the copy on the ring (taken before the descriptor is
/// visible to the host, so the host can never see it earlier).
pub const G_SUBMIT: u8 = 3;
/// The last interrupt the driver took before it found the completion.
pub const G_ISR: u8 = 4;
/// The completion found in the used ring (DPC, or a PASSIVE drain).
pub const G_DONE: u8 = 5;
/// The Present's DMA completion reported to dxgkrnl.
pub const G_NOTIFY: u8 = 6;
// Guest driver, flip:
/// `SetVidPnSourceAddress` entered for the flipped allocation.
pub const G_FLIP_DDI: u8 = 16;
/// About to put the `ScanoutFlip` on the ring (as `G_SUBMIT`).
pub const G_FLIP_SUBMIT: u8 = 17;
/// The last interrupt before the flip's answer was found.
pub const G_FLIP_ISR: u8 = 18;
/// The flip's answer found in the used ring.
pub const G_FLIP_ACK: u8 = 19;
/// The vsync tick that carried the flipped address reported to dxgkrnl.
pub const G_FLIP_RETIRE: u8 = 20;

// Backend (`conduit-backend`), CLOCK_MONOTONIC:
/// The control queue's kick handled (the vhost-user kick eventfd fired).
pub const H_KICK: u8 = 32;
/// The command taken off the ring and decoded.
pub const H_DECODED: u8 = 33;
/// The Venus command stream handed to the renderer (its `submit` returned).
pub const H_SUBMITTED: u8 = 34;
/// The renderer asked for the fence (`create_fence` returned).
pub const H_FENCE_ASKED: u8 = 35;
/// The renderer's signal reached the backend.
pub const H_SIGNALLED: u8 = 36;
/// The chain put on the used ring.
pub const H_USED: u8 = 37;
/// The guest notified (the call eventfd written: interrupt injected).
pub const H_IRQ: u8 = 38;
/// A flip handed to the display (viewer link).
pub const H_DISPLAY: u8 = 39;

// Renderer (`conduit-venus` and its virglrenderer), CLOCK_MONOTONIC:
/// The `SUBMIT` request received over the IPC socket.
pub const R_RECV: u8 = 48;
/// `virgl_renderer_submit_cmd` returned (queued to the render server).
pub const R_SUBMITTED: u8 = 49;
/// `CREATE_FENCE` received.
pub const R_FENCE: u8 = 50;
/// vkr called `vkQueueSubmit` for the stream's last submit.
pub const V_SUBMIT: u8 = 51;
/// That `vkQueueSubmit` returned.
pub const V_SUBMIT_DONE: u8 = 52;
/// vkr submitted the ring's sync fence.
pub const V_FENCE: u8 = 53;
/// vkr's sync thread saw the sync fence signalled.
pub const V_FENCE_DONE: u8 = 54;
/// virglrenderer's fence callback ran (`write_context_fence`).
pub const R_SIGNAL: u8 = 55;
/// The signal sent to the backend.
pub const R_PUSH: u8 = 56;
/// The GPU work of the stream's last submit, from timestamp queries: `aux`
/// is its duration in ns (end minus start timestamp), `ts_ns` is when the
/// sync thread read it (placement on the timeline is the collector's).
pub const V_GPU: u8 = 57;

/// Internal to `conduit-venus`, never stored: virglrenderer's proxy pairs
/// the seqno it gives a ring fence (`aux`) with the guest's fence id.
pub const V_SEQNO: u8 = 240;

/// A stage's short name, as the tool prints it.
pub fn name(stage: u8) -> &'static str {
    match stage {
        G_PRESENT => "kmd present",
        G_DEFER => "kmd defer",
        G_SUBMIT => "kmd submit",
        G_ISR => "kmd isr",
        G_DONE => "kmd done",
        G_NOTIFY => "kmd notify",
        G_FLIP_DDI => "kmd flip ddi",
        G_FLIP_SUBMIT => "kmd flip submit",
        G_FLIP_ISR => "kmd flip isr",
        G_FLIP_ACK => "kmd flip ack",
        G_FLIP_RETIRE => "kmd flip retire",
        H_KICK => "backend kick",
        H_DECODED => "backend decoded",
        H_SUBMITTED => "backend submitted",
        H_FENCE_ASKED => "backend fence",
        H_SIGNALLED => "backend signalled",
        H_USED => "backend used",
        H_IRQ => "backend irq",
        H_DISPLAY => "backend display",
        R_RECV => "venus recv",
        R_SUBMITTED => "venus submitted",
        R_FENCE => "venus fence",
        V_SUBMIT => "vkr submit",
        V_SUBMIT_DONE => "vkr submit done",
        V_FENCE => "vkr fence",
        V_FENCE_DONE => "vkr fence done",
        R_SIGNAL => "venus signal",
        R_PUSH => "venus push",
        V_GPU => "gpu",
        _ => "?",
    }
}

/// Which side stamped a stage: 0 guest driver, 1 backend, 2 renderer.
pub fn side(stage: u8) -> u8 {
    match stage {
        0..32 => 0,
        32..48 => 1,
        _ => 2,
    }
}

/// One stamp.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rec {
    /// `CLOCK_MONOTONIC` ns.
    pub ts_ns: u64,
    /// The fence id, or the flip's `seq`.
    pub id: u64,
    /// The Venus context; 0 for a flip.
    pub ctx: u32,
    /// The fence's ring; 0 for a flip.
    pub ring: u8,
    pub stage: u8,
    /// [`KIND_FENCE`] or [`KIND_FLIP`].
    pub kind: u8,
    /// Stage-specific ([`V_GPU`]: the duration in ns).
    pub aux: u64,
}

/// Bytes per record in a dump.
pub const REC_LEN: usize = 32;

impl Rec {
    pub fn fence(stage: u8, ctx: u32, ring: u32, id: u64, ts_ns: u64) -> Self {
        Self { ts_ns, id, ctx, ring: ring.min(255) as u8, stage, kind: KIND_FENCE, aux: 0 }
    }

    pub fn flip(stage: u8, seq: u64, ts_ns: u64) -> Self {
        Self { ts_ns, id: seq, ctx: 0, ring: 0, stage, kind: KIND_FLIP, aux: 0 }
    }

    /// `ts_ns u64 | id u64 | ctx u32 | ring u8 | stage u8 | kind u8 | 0 u8 | aux u64`, little-endian.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ts_ns.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.ctx.to_le_bytes());
        out.extend_from_slice(&[self.ring, self.stage, self.kind, 0]);
        out.extend_from_slice(&self.aux.to_le_bytes());
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < REC_LEN {
            return None;
        }
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Some(Self {
            ts_ns: u64_at(0),
            id: u64_at(8),
            ctx: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            ring: b[20],
            stage: b[21],
            kind: b[22],
            aux: u64_at(24),
        })
    }

    fn pack(&self) -> u64 {
        u64::from(self.ctx) | u64::from(self.ring) << 32 | u64::from(self.stage) << 40 | u64::from(self.kind) << 48
    }

    fn unpack(ts_ns: u64, id: u64, w: u64, aux: u64) -> Self {
        Self { ts_ns, id, ctx: w as u32, ring: (w >> 32) as u8, stage: (w >> 40) as u8, kind: (w >> 48) as u8, aux }
    }
}

/// A dump's magic: `stages dump` on the trace socket, the renderer's reply.
pub const DUMP_MAGIC: &[u8; 8] = b"CDTSTGH1";
/// Header: magic, record count u32, record length u32, records lost u64.
pub const DUMP_HDR: usize = 24;

/// Encode `recs` as a dump, `lost` the records the ring dropped since the
/// last one (overwritten before they were read).
pub fn encode_dump(recs: &[Rec], lost: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(DUMP_HDR + recs.len() * REC_LEN);
    out.extend_from_slice(DUMP_MAGIC);
    out.extend_from_slice(&(recs.len() as u32).to_le_bytes());
    out.extend_from_slice(&(REC_LEN as u32).to_le_bytes());
    out.extend_from_slice(&lost.to_le_bytes());
    for r in recs {
        r.encode(&mut out);
    }
    out
}

/// The records and the lost count of a dump; `None` if it is not one.
pub fn decode_dump(b: &[u8]) -> Option<(Vec<Rec>, u64)> {
    if b.len() < DUMP_HDR || &b[..8] != DUMP_MAGIC {
        return None;
    }
    let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    let len = u32::from_le_bytes(b[12..16].try_into().unwrap()) as usize;
    let lost = u64::from_le_bytes(b[16..24].try_into().unwrap());
    if len < REC_LEN || b.len() < DUMP_HDR + n.checked_mul(len)? {
        return None;
    }
    let recs = (0..n).filter_map(|i| Rec::decode(&b[DUMP_HDR + i * len..])).collect();
    Some((recs, lost))
}

// ---------------------------------------------------------------- the ring

/// Records the ring holds: about 18 s of a 240 Hz frame stream with every
/// stage stamped (two paths, about fifteen stamps per frame).
pub const SLOTS: usize = 1 << 16;

/// A seqlock per slot: `seq` is the record's index + 1 once written, 0
/// while a writer is in it.
#[derive(Default)]
struct Slot {
    seq: AtomicU64,
    ts: AtomicU64,
    id: AtomicU64,
    w: AtomicU64,
    aux: AtomicU64,
}

pub struct Ring {
    head: AtomicU64,
    slots: Box<[Slot]>,
    /// The next record to read, and the records lost so far.
    tail: Mutex<(u64, u64)>,
}

impl Ring {
    pub fn new(slots: usize) -> Self {
        assert!(slots.is_power_of_two());
        Self { head: AtomicU64::new(0), slots: (0..slots).map(|_| Slot::default()).collect(), tail: Mutex::new((0, 0)) }
    }

    /// Lock-free, any thread.
    pub fn push(&self, r: Rec) {
        let idx = self.head.fetch_add(1, Ordering::Relaxed);
        let s = &self.slots[idx as usize & (self.slots.len() - 1)];
        s.seq.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        s.ts.store(r.ts_ns, Ordering::Relaxed);
        s.id.store(r.id, Ordering::Relaxed);
        s.w.store(r.pack(), Ordering::Relaxed);
        s.aux.store(r.aux, Ordering::Relaxed);
        s.seq.store(idx + 1, Ordering::Release);
    }

    /// Append every record written since the last drain to `out`. Returns
    /// the records lost since the last drain: overwritten before they were
    /// read, or torn by a writer that lapped the reader. A record still being
    /// written ends the drain; the next one picks it up.
    pub fn drain(&self, out: &mut Vec<Rec>) -> u64 {
        let mut t = self.tail.lock().unwrap_or_else(|p| p.into_inner());
        let (mut tail, lost0) = *t;
        let mut lost = 0;
        let head = self.head.load(Ordering::Acquire);
        let n = self.slots.len() as u64;
        if head.saturating_sub(tail) > n {
            lost += head - n - tail;
            tail = head - n;
        }
        while tail < head {
            let s = &self.slots[tail as usize & (self.slots.len() - 1)];
            let seq = s.seq.load(Ordering::Acquire);
            if seq != tail + 1 {
                if seq > tail + 1 {
                    // Lapped: this slot holds a newer record already.
                    lost += 1;
                    tail += 1;
                    continue;
                }
                break; // being written
            }
            let r = Rec::unpack(
                s.ts.load(Ordering::Relaxed),
                s.id.load(Ordering::Relaxed),
                s.w.load(Ordering::Relaxed),
                s.aux.load(Ordering::Relaxed),
            );
            fence(Ordering::Acquire);
            if s.seq.load(Ordering::Relaxed) == tail + 1 {
                out.push(r);
            } else {
                lost += 1;
            }
            tail += 1;
        }
        *t = (tail, lost0 + lost);
        lost
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static RING: OnceLock<Ring> = OnceLock::new();
/// When the control queue's kick was last handled, for [`H_KICK`].
static KICK_NS: AtomicU64 = AtomicU64::new(0);

/// Whether stages are being stamped. The one check a stamp site makes.
#[inline(always)]
pub fn on() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn stamping on or off. The ring is allocated on the first `true`;
/// records already in it stay until drained.
pub fn set_on(yes: bool) {
    if yes {
        RING.get_or_init(|| Ring::new(SLOTS));
    }
    ENABLED.store(yes, Ordering::Relaxed);
}

/// `CONDUIT_STAGE_TRACE=1` (or any value but `0` and empty) turns stamping
/// on at start. Returns whether it did.
pub fn init_from_env() -> bool {
    let yes = std::env::var_os("CONDUIT_STAGE_TRACE").is_some_and(|v| !v.is_empty() && v != "0");
    if yes {
        set_on(true);
    }
    yes
}

/// `CLOCK_MONOTONIC` in nanoseconds (vDSO, no syscall).
#[inline]
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a live timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Record `r` if stamping is on.
#[inline]
pub fn stamp(r: Rec) {
    if on()
        && let Some(ring) = RING.get()
    {
        ring.push(r);
    }
}

/// Note that the control queue's kick is being handled now.
#[inline]
pub fn kick() {
    if on() {
        KICK_NS.store(now_ns(), Ordering::Relaxed);
    }
}

/// When [`kick`] last ran (0 if never).
pub fn last_kick() -> u64 {
    KICK_NS.load(Ordering::Relaxed)
}

/// Everything stamped since the last call, as a dump.
pub fn dump() -> Vec<u8> {
    let (recs, lost) = take();
    encode_dump(&recs, lost)
}

/// Everything stamped since the last call (and the records lost meanwhile).
pub fn take() -> (Vec<Rec>, u64) {
    let mut recs = Vec::new();
    let lost = RING.get().map_or(0, |r| r.drain(&mut recs));
    (recs, lost)
}

/// Records from elsewhere (the renderer's ring) into this process's.
pub fn absorb(recs: &[Rec]) {
    if let Some(ring) = RING.get() {
        for r in recs {
            ring.push(*r);
        }
    }
}

// ---------------------------------------------------------- the guest's ring

/// The guest driver's ring, as it publishes it: the REG_BINARY value
/// `StgRing` in its service key (guest/windows/docs/kmd-handoff-2026-10.md).
///
/// ```text
/// header 32 bytes: magic "CDTSTG01" | head u64 (records ever written) |
///                  slots u32 | record length u32 (24) | clock u32 (1) | flags u32 (bit 0: on)
/// slot 24 bytes:   seq u64 (index + 1, 0 never written) | t u64 (interrupt time, 100 ns) |
///                  w u64 = id u32 | stage u8 << 32 | kind u8 << 40 | aux u16 << 48
/// ```
///
/// `id` is the low 32 bits of the wire fence id (a copy) or of the flip's
/// `seq`. Clock 1 is `KeQueryInterruptTimePrecise`, 100 ns units.
pub mod guest {
    pub const MAGIC: &[u8; 8] = b"CDTSTG01";
    pub const HDR: usize = 32;
    pub const REC: usize = 24;
    /// Interrupt time, 100 ns units.
    pub const CLOCK_INTERRUPT_100NS: u32 = 1;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct GRec {
        /// Index in the guest's ring (absolute, from 0).
        pub index: u64,
        /// 100 ns units.
        pub t: u64,
        pub id: u32,
        pub stage: u8,
        pub kind: u8,
        pub aux: u16,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Snapshot {
        pub head: u64,
        pub slots: u32,
        pub clock: u32,
        pub on: bool,
        /// Written slots, oldest first.
        pub recs: Vec<GRec>,
    }

    /// Decode a snapshot; `None` if it is not one.
    pub fn parse(b: &[u8]) -> Option<Snapshot> {
        if b.len() < HDR || &b[..8] != MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let head = u64_at(8);
        let slots = u32_at(16);
        let rec = u32_at(20) as usize;
        if rec < REC {
            return None;
        }
        let mut recs = Vec::new();
        for i in 0..slots as usize {
            let at = HDR + i * rec;
            if at + REC > b.len() {
                break;
            }
            let seq = u64_at(at);
            if seq == 0 || seq > head {
                continue;
            }
            let w = u64_at(at + 16);
            recs.push(GRec {
                index: seq - 1,
                t: u64_at(at + 8),
                id: w as u32,
                stage: (w >> 32) as u8,
                kind: (w >> 40) as u8,
                aux: (w >> 48) as u16,
            });
        }
        recs.sort_by_key(|r| r.index);
        Some(Snapshot { head, slots, clock: u32_at(24), on: u32_at(28) & 1 != 0, recs })
    }

    /// The value from `reg query` output (`StgRing REG_BINARY 4344...`), or
    /// a bare hex string; whitespace is ignored.
    pub fn from_reg_query(text: &str) -> Option<Vec<u8>> {
        let hex = match text.find("REG_BINARY") {
            Some(i) => &text[i + "REG_BINARY".len()..],
            None => text,
        };
        let digits: Vec<u8> = hex.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
        if !digits.len().is_multiple_of(2) || digits.is_empty() {
            return None;
        }
        digits.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_a_dump() {
        let a = Rec { aux: 1234, ..Rec::fence(H_KICK, 7, 1, 0x1_0000_0005, 99) };
        let b = Rec::flip(H_DISPLAY, 42, 100);
        let d = encode_dump(&[a, b], 3);
        assert_eq!(d.len(), DUMP_HDR + 2 * REC_LEN);
        assert_eq!(decode_dump(&d), Some((vec![a, b], 3)));
        assert_eq!(decode_dump(&d[..DUMP_HDR + REC_LEN]), None, "short");
        assert_eq!(decode_dump(b"CDTSTGH0xxxxxxxxxxxxxxxx"), None);
    }

    #[test]
    fn the_ring_drains_in_order_and_counts_what_it_lost() {
        let ring = Ring::new(8);
        for i in 0..5 {
            ring.push(Rec::flip(H_KICK, i, i * 10));
        }
        let mut out = Vec::new();
        assert_eq!(ring.drain(&mut out), 0);
        assert_eq!(out.iter().map(|r| r.id).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        out.clear();
        assert_eq!(ring.drain(&mut out), 0);
        assert!(out.is_empty(), "drained once");
        for i in 5..25 {
            ring.push(Rec::flip(H_USED, i, i));
        }
        assert_eq!(ring.drain(&mut out), 12, "20 written into 8 slots");
        assert_eq!(out.iter().map(|r| r.id).collect::<Vec<_>>(), (17..25).collect::<Vec<_>>());
        assert!(out.iter().all(|r| r.stage == H_USED && r.kind == KIND_FLIP));
    }

    #[test]
    fn concurrent_writers_lose_nothing_while_the_ring_has_room() {
        let ring = std::sync::Arc::new(Ring::new(1 << 14));
        let threads: Vec<_> = (0..4u64)
            .map(|t| {
                let ring = ring.clone();
                std::thread::spawn(move || {
                    for i in 0..2000u64 {
                        ring.push(Rec { aux: t, ..Rec::fence(V_GPU, t as u32, 1, i, i) });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut out = Vec::new();
        assert_eq!(ring.drain(&mut out), 0);
        assert_eq!(out.len(), 8000);
        for t in 0..4 {
            let ids: Vec<u64> = out.iter().filter(|r| r.ctx == t).map(|r| r.id).collect();
            assert_eq!(ids, (0..2000).collect::<Vec<_>>(), "one writer's records keep their order");
        }
    }

    #[test]
    fn stages_have_names_and_sides() {
        for s in [G_PRESENT, G_FLIP_RETIRE, H_KICK, H_DISPLAY, R_RECV, V_GPU] {
            assert_ne!(name(s), "?", "{s}");
        }
        assert_eq!((side(G_NOTIFY), side(H_IRQ), side(V_FENCE_DONE)), (0, 1, 2));
    }

    fn guest_bytes(head: u64, slots: &[(u64, u64, u32, u8, u8, u16)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(guest::MAGIC);
        b.extend_from_slice(&head.to_le_bytes());
        b.extend_from_slice(&(slots.len() as u32).to_le_bytes());
        b.extend_from_slice(&24u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        for &(seq, t, id, stage, kind, aux) in slots {
            b.extend_from_slice(&seq.to_le_bytes());
            b.extend_from_slice(&t.to_le_bytes());
            let w = u64::from(id) | u64::from(stage) << 32 | u64::from(kind) << 40 | u64::from(aux) << 48;
            b.extend_from_slice(&w.to_le_bytes());
        }
        b
    }

    #[test]
    fn a_guest_snapshot_decodes_oldest_first_and_skips_empty_slots() {
        // Four slots, six records written: slots hold indices 4, 5, 2, 3.
        let b = guest_bytes(
            6,
            &[
                (5, 500, 0xdead_beef, G_DONE, KIND_FENCE, 0),
                (6, 600, 9, G_FLIP_ACK, KIND_FLIP, 7),
                (3, 300, 0xdead_beef, G_PRESENT, KIND_FENCE, 0),
                (4, 400, 0xdead_beef, G_SUBMIT, KIND_FENCE, 0),
            ],
        );
        let s = guest::parse(&b).unwrap();
        assert_eq!((s.head, s.slots, s.clock, s.on), (6, 4, 1, true));
        assert_eq!(s.recs.iter().map(|r| r.index).collect::<Vec<_>>(), [2, 3, 4, 5]);
        assert_eq!(s.recs[3], guest::GRec { index: 5, t: 600, id: 9, stage: G_FLIP_ACK, kind: KIND_FLIP, aux: 7 });
        let empty = guest_bytes(0, &[(0, 0, 0, 0, 0, 0)]);
        assert!(guest::parse(&empty).unwrap().recs.is_empty());
        assert_eq!(guest::parse(&b[..31]), None);
    }

    #[test]
    fn reg_query_output_is_read() {
        let text = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render\r\n    StgRing    REG_BINARY    4344545354473031\r\n\r\n";
        assert_eq!(guest::from_reg_query(text).unwrap(), b"CDTSTG01");
        assert_eq!(guest::from_reg_query("43 44"), Some(vec![0x43, 0x44]));
        assert_eq!(guest::from_reg_query("434"), None);
        assert_eq!(guest::from_reg_query("zz"), None);
    }
}

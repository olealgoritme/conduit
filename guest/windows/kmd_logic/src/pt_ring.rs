//! Page-table operation ring (`NvPtRing`, default 0): the pure half. Which GPU VA a native
//! page-table operation touched, when, and how it ended, so that an MMU fault the host reports
//! (`Xid 31 ... faulted @ 0x0_15a9f000 ... FAULT_PTE`) can be matched with the last operation
//! that left that VA. The statics, the record sites and the publish are
//! `kmd_render/src/ddi/pt_ring.rs`; the layout is here.
//!
//! Two snapshots, both REG_BINARY values in the service key:
//!
//! * the ring (`NvPtRing`, and `NvPtRingF` taken at the first pass after a native channel fault
//!   was seen): the last [`SLOTS`] operations;
//! * the last-touch table (`NvPtLast`, `NvPtLastF`): per 64-KiB VA chunk (hashed, [`TOUCH`]
//!   entries) the last operation that changed what is mapped there. It survives a ring that
//!   wrapped. It stops changing when a fault is seen until the fault snapshot is written, so the
//!   teardown that follows a fault does not overwrite it.
//!
//! ```text
//! header 32 bytes: magic "CDTPTR01" | head u64 (records ever written) |
//!                  slots u32 | record length u32 (48) | touch entries u32 | flags u32
//!                  (bit 0 on, bit 1 fault snapshot, bit 2 table frozen)
//! slot 48 bytes:   seq u64 (index + 1, 0 = never written) | t u64 (interrupt time, 100 ns) |
//!                  va u64 | len u64 (bytes) | w u64 | process u64
//!                  w = kind u8 | flags u8 << 8 | detail u16 << 16 | aux u32 << 32
//! touch 24 bytes:  key u64 (64-KiB chunk number + 1, 0 = empty) | t u64 | idx u64
//!                  (the ring record's seq | kind << 56)
//! ```
//!
//! Kinds 1..=11 are the `native_memory::watch` operation numbers (one event stream feeds both):
//! 2/3 foreign map ok/failed, 4 foreign drop, 5/6 native video-memory map ok/failed, 7/8
//! system-page map ok/failed, 10 RM unmap, 11 a native leaf update ended (detail = its
//! `leaf_stage`: 0xfe mapped, 0xff nothing to change, else the stage it failed at).

/// Slots in the driver's ring.
pub const SLOTS: usize = 4096;
/// Bytes per published slot.
pub const REC_LEN: usize = 48;
/// Bytes of the published header.
pub const HDR_LEN: usize = 32;
/// Bytes of a ring snapshot.
pub const SNAPSHOT_LEN: usize = HDR_LEN + SLOTS * REC_LEN;
pub const MAGIC: &[u8; 8] = b"CDTPTR01";

/// Entries of the last-touch table.
pub const TOUCH: usize = 4096;
/// Bytes per published touch entry.
pub const TOUCH_LEN: usize = 24;
/// Bytes of a table snapshot (same header as the ring's).
pub const TOUCH_SNAPSHOT_LEN: usize = HDR_LEN + TOUCH * TOUCH_LEN;
/// The table's granularity: 64 KiB.
pub const CHUNK_SHIFT: u32 = 16;
/// Chunks one operation updates in the table at most; a longer range updates its first and
/// last [`TOUCH_SPAN_MAX`] / 2 chunks (process teardown, whole-window drops).
pub const TOUCH_SPAN_MAX: u64 = 512;

// Operation kinds (`w` bits 0..8).
/// `native_memory::watch` op 2: system pages of a foreign (non-native) allocation mapped.
pub const K_FOREIGN_MAP: u8 = 2;
pub const K_FOREIGN_FAIL: u8 = 3;
pub const K_FOREIGN_DROP: u8 = 4;
/// A native allocation's RM video memory mapped at the VA (one RM map).
pub const K_MAP_VID: u8 = 5;
pub const K_MAP_VID_FAIL: u8 = 6;
/// VidMm's system pages of a native allocation mapped (one OS-descriptor chunk).
pub const K_MAP_SYS: u8 = 7;
pub const K_MAP_SYS_FAIL: u8 = 8;
/// One RM unmap (detail bit 0: by `evict_range`).
pub const K_UNMAP: u8 = 10;
/// A native leaf update ended (detail = the leaf stage).
pub const K_LEAF_DONE: u8 = 11;
/// VidMm's `UpdatePageTable` as it arrived (flags: `UPT_*`; detail: the first PTE's segment;
/// aux: the native allocation id, 0 for a foreign one).
pub const K_UPT_IN: u8 = 20;
/// The `UpdatePageTable` answered (detail 0 executed, 1 refused / skipped; aux: the last
/// `NvPgLeafWhy`, the reason of the refusal, when refused).
pub const K_UPT_OUT: u8 = 21;
/// `evict_range` dropped another allocation's (or a foreign range's) mapping here (aux: the
/// victim's allocation id, 0 = foreign).
pub const K_EVICT: u8 = 22;
/// `evict_range` showed the part of a dropped mapping outside the range again (detail 0 ok,
/// 1 failed: that part stays unmapped).
pub const K_EVICT_REST: u8 = 23;
/// An explicit TLB invalidate of the space.
pub const K_TLB_INV: u8 = 24;
/// The pre-replacement quiesce (detail 0 ok, 1 refused, 2 skipped by `NvHotPath`).
pub const K_QUIESCE: u8 = 25;
/// A native channel fault or hang was reported (`HwsNatFault` / `HwsNatHang`; detail = the
/// node, aux = the `native_hang::Cause` code).
pub const K_FAULT: u8 = 26;

// `K_UPT_IN` flags.
pub const UPT_NATIVE: u8 = 1 << 0;
/// A non-leaf level (RM owns those: no host operation).
pub const UPT_UPPER: u8 = 1 << 1;
pub const UPT_ANY_VALID: u8 = 1 << 2;
pub const UPT_ANY_INVALID: u8 = 1 << 3;
/// A PTE with the `Zero` (sparse) bit.
pub const UPT_ZERO: u8 = 1 << 4;
pub const UPT_REPEAT: u8 = 1 << 5;
/// VidMm's own (system / paging) process.
pub const UPT_SYSTEM: u8 = 1 << 6;
pub const UPT_NOT_PASSIVE: u8 = 1 << 7;

/// Header flags.
pub const HDR_ON: u32 = 1;
pub const HDR_FAULT: u32 = 2;
pub const HDR_FROZEN: u32 = 4;

/// Whether `kind` changes what the GPU sees at the VA (the kinds the last-touch table keeps).
pub const fn touches(kind: u8) -> bool {
    matches!(
        kind,
        K_FOREIGN_MAP
            | K_FOREIGN_DROP
            | K_MAP_VID
            | K_MAP_VID_FAIL
            | K_MAP_SYS
            | K_MAP_SYS_FAIL
            | K_UNMAP
            | K_UPT_IN
            | K_EVICT
            | K_EVICT_REST
    )
}

/// The slot record `index` lives in.
pub const fn slot(index: u64) -> usize {
    (index % SLOTS as u64) as usize
}

/// One record's `w` word.
pub const fn pack(kind: u8, flags: u8, detail: u16, aux: u32) -> u64 {
    kind as u64 | (flags as u64) << 8 | (detail as u64) << 16 | (aux as u64) << 32
}

/// `(kind, flags, detail, aux)` of a `w` word.
pub const fn unpack(w: u64) -> (u8, u8, u16, u32) {
    (w as u8, (w >> 8) as u8, (w >> 16) as u16, (w >> 32) as u32)
}

/// The table entry of 64-KiB chunk `chunk` (a multiplicative hash: neighbouring chunks spread).
pub const fn touch_index(chunk: u64) -> usize {
    ((chunk.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) % TOUCH as u64) as usize
}

/// The 64-KiB chunks an operation on `[va, va + len)` updates in the table: `(first, count)`
/// of the leading run and `(first, count)` of the trailing run (count 0 = none). A range of at
/// most [`TOUCH_SPAN_MAX`] chunks is one run.
pub const fn touch_runs(va: u64, len: u64) -> ((u64, u64), (u64, u64)) {
    let first = va >> CHUNK_SHIFT;
    let end = va.saturating_add(if len == 0 { 1 } else { len });
    let last = (end - 1) >> CHUNK_SHIFT;
    let n = last - first + 1;
    if n <= TOUCH_SPAN_MAX {
        ((first, n), (0, 0))
    } else {
        let half = TOUCH_SPAN_MAX / 2;
        ((first, half), (last + 1 - half, half))
    }
}

/// A touch entry's `idx` word: the ring record's seq and its kind.
pub const fn touch_idx(seq: u64, kind: u8) -> u64 {
    (seq & ((1 << 56) - 1)) | (kind as u64) << 56
}

/// Write the header into `out[..HDR_LEN]`.
pub fn encode_header(out: &mut [u8], head: u64, flags: u32) {
    out[..8].copy_from_slice(MAGIC);
    out[8..16].copy_from_slice(&head.to_le_bytes());
    out[16..20].copy_from_slice(&(SLOTS as u32).to_le_bytes());
    out[20..24].copy_from_slice(&(REC_LEN as u32).to_le_bytes());
    out[24..28].copy_from_slice(&(TOUCH as u32).to_le_bytes());
    out[28..32].copy_from_slice(&flags.to_le_bytes());
}

/// One ring record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub t: u64,
    pub va: u64,
    pub len: u64,
    pub w: u64,
    pub process: u64,
}

impl Record {
    pub const fn kind(&self) -> u8 {
        self.w as u8
    }

    /// Whether the record's range covers `va`.
    pub const fn covers(&self, va: u64) -> bool {
        va >= self.va && va - self.va < if self.len == 0 { 1 } else { self.len }
    }
}

/// Write slot `i` into a ring snapshot.
pub fn encode_slot(out: &mut [u8], i: usize, r: &Record) {
    let at = HDR_LEN + i * REC_LEN;
    for (k, v) in [r.seq, r.t, r.va, r.len, r.w, r.process].into_iter().enumerate() {
        out[at + 8 * k..at + 8 * k + 8].copy_from_slice(&v.to_le_bytes());
    }
}

fn get64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// Slot `i` of a ring snapshot.
pub fn decode_slot(b: &[u8], i: usize) -> Record {
    let at = HDR_LEN + i * REC_LEN;
    Record {
        seq: get64(b, at),
        t: get64(b, at + 8),
        va: get64(b, at + 16),
        len: get64(b, at + 24),
        w: get64(b, at + 32),
        process: get64(b, at + 40),
    }
}

/// Write touch entry `i` into a table snapshot.
pub fn encode_touch(out: &mut [u8], i: usize, key: u64, t: u64, idx: u64) {
    let at = HDR_LEN + i * TOUCH_LEN;
    out[at..at + 8].copy_from_slice(&key.to_le_bytes());
    out[at + 8..at + 16].copy_from_slice(&t.to_le_bytes());
    out[at + 16..at + 24].copy_from_slice(&idx.to_le_bytes());
}

/// The latest record of a ring snapshot that changed the mapping at `va` (a kind
/// [`touches`] names), if the ring still holds one.
pub fn last_touch(snapshot: &[u8], va: u64) -> Option<Record> {
    (0..SLOTS)
        .map(|i| decode_slot(snapshot, i))
        .filter(|r| r.seq != 0 && touches(r.kind()) && r.covers(va))
        .max_by_key(|r| r.seq)
}

/// `NvPtSparse` (REG_DWORD, default 0): the RM virtual windows of a per-process space are
/// allocated SPARSE (`NVOS32_ALLOC_FLAGS_SPARSE`): a VA nothing is mapped at reads as zero and
/// drops writes instead of an MMU fault (`FAULT_PTE`), as a D3D12 NULL tile on NVIDIA. RM's
/// unmap inside a sparse range restores the sparse state (`gvaspaceUnmap`). Bit 0: the low
/// window [64 KiB, 4 GiB); bit 1: the high window [8 GiB, 64 GiB). A diagnosis and a
/// mitigation: it hides the faults, not their cause.
pub const NVOS32_ALLOC_FLAGS_SPARSE: u32 = 0x0400_0000;

/// The extra `NVOS32` allocation flags of a window under `NvPtSparse` = `knob`.
pub const fn window_flags(knob: u32, high: bool) -> u32 {
    let bit = if high { 2 } else { 1 };
    if knob != u32::MAX && knob & bit != 0 {
        NVOS32_ALLOC_FLAGS_SPARSE
    } else {
        0
    }
}

/// The counters `ddi/pt_ring.rs` writes besides the snapshots (at most 14 characters).
pub const COUNTERS: &[&str] = &["NvPtOn", "NvPtHead", "NvPtPubN", "NvPtFaultN", "NvPtFaultT"];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    #[test]
    fn the_word_round_trips() {
        let w = pack(K_UPT_IN, UPT_NATIVE | UPT_ANY_INVALID, 0x1234, 0xdead_beef);
        assert_eq!(unpack(w), (K_UPT_IN, UPT_NATIVE | UPT_ANY_INVALID, 0x1234, 0xdead_beef));
        assert_eq!(w as u8, 20);
    }

    #[test]
    fn slots_wrap() {
        assert_eq!(slot(0), 0);
        assert_eq!(slot(SLOTS as u64), 0);
        assert_eq!(slot(SLOTS as u64 + 7), 7);
    }

    #[test]
    fn short_ranges_are_one_run_and_long_ones_keep_both_ends() {
        assert_eq!(touch_runs(0x15a9f000, 0x1000), ((0x15a9, 1), (0, 0)));
        // Straddles a chunk boundary.
        assert_eq!(touch_runs(0xf000, 0x2000), ((0, 2), (0, 0)));
        // A zero-length record still names its chunk.
        assert_eq!(touch_runs(0x2_0000, 0), ((2, 1), (0, 0)));
        // 2 MiB = 32 chunks.
        assert_eq!(touch_runs(0x20_0000, 0x20_0000), ((0x20, 32), (0, 0)));
        // 1 GiB: both ends.
        let ((a, n), (b, m)) = touch_runs(0x4000_0000, 0x4000_0000);
        assert_eq!((a, n), (0x4000, TOUCH_SPAN_MAX / 2));
        assert_eq!(b + m - 1, 0x7fff);
        assert_eq!(m, TOUCH_SPAN_MAX / 2);
    }

    #[test]
    fn touch_index_is_in_range_and_spreads_neighbours() {
        let mut seen = std::collections::BTreeSet::new();
        for c in 0x1000..0x1000 + 64u64 {
            let i = touch_index(c);
            assert!(i < TOUCH);
            seen.insert(i);
        }
        assert!(seen.len() >= 60, "neighbouring chunks mostly land apart: {}", seen.len());
    }

    #[test]
    fn a_snapshot_finds_the_last_operation_at_a_va() {
        let mut b = vec![0u8; SNAPSHOT_LEN];
        encode_header(&mut b, 4, HDR_ON | HDR_FAULT);
        let rec = |seq, va, len, kind| Record {
            seq,
            t: seq * 10,
            va,
            len,
            w: pack(kind, 0, 0, 7),
            process: 0xffff_8000_1234_0000,
        };
        encode_slot(&mut b, 0, &rec(1, 0x1500_0000, 0x100_0000, K_MAP_VID));
        encode_slot(&mut b, 1, &rec(2, 0x15a0_0000, 0x10_0000, K_UNMAP));
        // A TLB invalidate does not change the mapping: not a touch.
        encode_slot(&mut b, 2, &rec(3, 0x15a0_0000, 0x10_0000, K_TLB_INV));
        encode_slot(&mut b, 3, &rec(4, 0x4000_0000, 0x1000, K_MAP_VID));
        let hit = last_touch(&b, 0x15a9_f000).unwrap();
        assert_eq!((hit.seq, hit.kind()), (2, K_UNMAP));
        assert_eq!(last_touch(&b, 0x1400_0000), None);
        assert_eq!(last_touch(&b, 0x1510_0000).unwrap().kind(), K_MAP_VID);
        assert_eq!(&b[..8], MAGIC);
        assert_eq!(decode_slot(&b, 1), rec(2, 0x15a0_0000, 0x10_0000, K_UNMAP));
        assert_eq!(SNAPSHOT_LEN, 32 + 4096 * 48);
    }

    #[test]
    fn touch_entries_land_where_the_host_reads_them() {
        let mut b = vec![0u8; TOUCH_SNAPSHOT_LEN];
        encode_touch(&mut b, 3, 0x15a9 + 1, 99, touch_idx(5, K_UNMAP));
        let at = HDR_LEN + 3 * TOUCH_LEN;
        assert_eq!(get64(&b, at), 0x15aa);
        assert_eq!(get64(&b, at + 8), 99);
        assert_eq!(get64(&b, at + 16) >> 56, u64::from(K_UNMAP));
        assert_eq!(get64(&b, at + 16) & 0xff_ffff, 5);
    }

    #[test]
    fn sparse_windows_follow_their_bits() {
        assert_eq!(window_flags(0, false), 0);
        assert_eq!(window_flags(u32::MAX, false), 0, "an unread knob is off");
        assert_eq!(window_flags(1, false), NVOS32_ALLOC_FLAGS_SPARSE);
        assert_eq!(window_flags(1, true), 0);
        assert_eq!(window_flags(2, true), NVOS32_ALLOC_FLAGS_SPARSE);
        assert_eq!(window_flags(3, false) | window_flags(3, true), NVOS32_ALLOC_FLAGS_SPARSE);
    }

    #[test]
    fn touch_kinds_are_the_mapping_changes() {
        for k in [K_MAP_VID, K_UNMAP, K_MAP_SYS, K_FOREIGN_DROP, K_EVICT, K_UPT_IN] {
            assert!(touches(k), "{k}");
        }
        for k in [K_LEAF_DONE, K_UPT_OUT, K_TLB_INV, K_QUIESCE, K_FAULT] {
            assert!(!touches(k), "{k}");
        }
    }

    #[test]
    fn the_counters_the_driver_writes_are_listed_and_fit() {
        for n in COUNTERS
            .iter()
            .chain(["NvPtRing", "NvPtRingF", "NvPtLast", "NvPtLastF"].iter())
        {
            assert!(n.len() <= 14, "{n}");
        }
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        let Ok(text) = std::fs::read_to_string(render.join("ddi/pt_ring.rs")) else {
            return;
        };
        for n in COUNTERS
            .iter()
            .chain(["NvPtRing", "NvPtRingF", "NvPtLast", "NvPtLastF"].iter())
        {
            assert!(text.contains(&std::format!("b\"{n}\"")), "{n} not written by ddi/pt_ring.rs");
        }
    }
}

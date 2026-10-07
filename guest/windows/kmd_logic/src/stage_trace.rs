//! Per-frame stage timestamps (`StageTrace`): the pure half. The ring's slot math, the packing of
//! a record and the snapshot the driver publishes as the REG_BINARY value `StgRing`. The statics,
//! the stamp sites and the publish are `kmd_render/src/ddi/stage_trace.rs`.
//!
//! The host joins these records with its own by the id the wire already carries: a windowed
//! Present copy by its WIRE fence id (the ring-1 `SUBMIT_3D`'s `ctrl_hdr.fence_id`), a
//! `ForeignFlip` by its `ScanoutFlip::seq`; the low 32 bits of either. The stage numbers and the
//! snapshot layout are shared with the host (`host/venus/src/stage.rs`, module `guest`), which
//! decodes it; `docs/TRACING.md` "Frame stage timing" says how to run the collector.
//!
//! ```text
//! header 32 bytes: magic "CDTSTG01" | head u64 (records ever written) |
//!                  slots u32 | record length u32 (24) | clock u32 (1) | flags u32 (bit 0: on)
//! slot 24 bytes:   seq u64 (index + 1, 0 never written) | t u64 (interrupt time, 100 ns) |
//!                  w u64 = id u32 | stage u8 << 32 | kind u8 << 40 | aux u16 << 48
//! ```

/// Slots in the driver's ring: about three seconds of a 240 Hz desktop with a windowed app
/// (eleven stamps a frame at most), more than the publish interval.
pub const SLOTS: usize = 8192;
/// Bytes per published slot.
pub const REC_LEN: usize = 24;
/// Bytes of the published header.
pub const HDR_LEN: usize = 32;
/// Bytes of a whole snapshot.
pub const SNAPSHOT_LEN: usize = HDR_LEN + SLOTS * REC_LEN;
pub const MAGIC: &[u8; 8] = b"CDTSTG01";
/// `KeQueryInterruptTimePrecise`, 100 ns units.
pub const CLOCK_INTERRUPT_100NS: u32 = 1;

/// A windowed Present copy, by its wire fence id.
pub const KIND_FENCE: u8 = 1;
/// A `ScanoutFlip`, by its `seq`.
pub const KIND_FLIP: u8 = 2;

/// `DxgkDdiPresent` entered (Blt arm).
pub const G_PRESENT: u8 = 1;
/// The copy was queued for the worker (the producer had not finished).
pub const G_DEFER: u8 = 2;
/// Taken just before the copy was handed to the ring (the host cannot see it earlier).
pub const G_SUBMIT: u8 = 3;
/// The last interrupt before the completion was found.
pub const G_ISR: u8 = 4;
/// The completion found in the used ring.
pub const G_DONE: u8 = 5;
/// The Present's DMA completion reported to dxgkrnl (not stamped yet, see the handoff doc).
pub const G_NOTIFY: u8 = 6;
/// `SetVidPnSourceAddress` entered for the flipped allocation.
pub const G_FLIP_DDI: u8 = 16;
/// Taken just before the `ScanoutFlip` was handed to the ring.
pub const G_FLIP_SUBMIT: u8 = 17;
/// The last interrupt before the flip's answer was found.
pub const G_FLIP_ISR: u8 = 18;
/// The flip's answer found.
pub const G_FLIP_ACK: u8 = 19;
/// The vsync tick that carried the flipped address (not stamped yet, see the handoff doc).
pub const G_FLIP_RETIRE: u8 = 20;

/// The counters `ddi/stage_trace.rs` writes besides `StgRing` (at most 14 characters).
pub const COUNTERS: &[&str] = &["StgOn", "StgHead", "StgPubN"];

/// The slot record `index` lives in.
pub const fn slot(index: u64) -> usize {
    (index % SLOTS as u64) as usize
}

/// One record's `w` word.
pub const fn pack(id: u32, stage: u8, kind: u8, aux: u16) -> u64 {
    id as u64 | (stage as u64) << 32 | (kind as u64) << 40 | (aux as u64) << 48
}

/// The id a record carries: the low 32 bits of a wire fence id or a flip's `seq`.
pub const fn id32(id: u64) -> u32 {
    id as u32
}

/// Write the header for `head` records ever written into `out[..HDR_LEN]`.
pub fn encode_header(out: &mut [u8], head: u64, on: bool) {
    out[..8].copy_from_slice(MAGIC);
    out[8..16].copy_from_slice(&head.to_le_bytes());
    out[16..20].copy_from_slice(&(SLOTS as u32).to_le_bytes());
    out[20..24].copy_from_slice(&(REC_LEN as u32).to_le_bytes());
    out[24..28].copy_from_slice(&CLOCK_INTERRUPT_100NS.to_le_bytes());
    out[28..32].copy_from_slice(&u32::from(on).to_le_bytes());
}

/// Write slot `i`'s `(seq, t, w)` into its place in a snapshot buffer.
pub fn encode_slot(out: &mut [u8], i: usize, seq: u64, t: u64, w: u64) {
    let at = HDR_LEN + i * REC_LEN;
    out[at..at + 8].copy_from_slice(&seq.to_le_bytes());
    out[at + 8..at + 16].copy_from_slice(&t.to_le_bytes());
    out[at + 16..at + 24].copy_from_slice(&w.to_le_bytes());
}

/// The interrupt that preceded a completion found at `found`: `last_isr` when it lies between
/// the submission and the find, else none (the completion was found by a poll, or the last
/// interrupt was for something older). An unknown submission time (0) has none: any older
/// interrupt would pass for it.
pub const fn isr_for(submitted: u64, last_isr: u64, found: u64) -> Option<u64> {
    if submitted != 0 && last_isr != 0 && last_isr >= submitted && last_isr <= found {
        Some(last_isr)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn the_word_is_laid_out_as_the_host_reads_it() {
        let w = pack(0xdead_beef, G_FLIP_ACK, KIND_FLIP, 7);
        assert_eq!(w as u32, 0xdead_beef);
        assert_eq!((w >> 32) as u8, G_FLIP_ACK);
        assert_eq!((w >> 40) as u8, KIND_FLIP);
        assert_eq!((w >> 48) as u16, 7);
        assert_eq!(id32(0x1_0000_0005), 5);
    }

    #[test]
    fn slots_wrap() {
        assert_eq!(slot(0), 0);
        assert_eq!(slot(SLOTS as u64 - 1), SLOTS - 1);
        assert_eq!(slot(SLOTS as u64), 0);
        assert_eq!(slot(3 * SLOTS as u64 + 5), 5);
    }

    /// The bytes the host's decoder (`host/venus/src/stage.rs`, `guest::parse`) is tested with:
    /// the same header and slot layout, byte for byte.
    #[test]
    fn a_snapshot_matches_the_host_layout() {
        let mut b = vec![0u8; SNAPSHOT_LEN];
        encode_header(&mut b, 6, true);
        encode_slot(&mut b, 0, 5, 500, pack(0xdead_beef, G_DONE, KIND_FENCE, 0));
        encode_slot(&mut b, 1, 6, 600, pack(9, G_FLIP_ACK, KIND_FLIP, 7));
        let mut want: Vec<u8> = Vec::new();
        want.extend_from_slice(b"CDTSTG01");
        want.extend_from_slice(&6u64.to_le_bytes());
        want.extend_from_slice(&8192u32.to_le_bytes());
        want.extend_from_slice(&24u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        for (seq, t, id, stage, kind, aux) in [
            (5u64, 500u64, 0xdead_beefu32, 5u8, 1u8, 0u16),
            (6, 600, 9, 19, 2, 7),
        ] {
            want.extend_from_slice(&seq.to_le_bytes());
            want.extend_from_slice(&t.to_le_bytes());
            let w = u64::from(id)
                | u64::from(stage) << 32
                | u64::from(kind) << 40
                | u64::from(aux) << 48;
            want.extend_from_slice(&w.to_le_bytes());
        }
        assert_eq!(&b[..want.len()], &want[..]);
        assert!(
            b[want.len()..].iter().all(|&x| x == 0),
            "unwritten slots stay zero (seq 0)"
        );
        assert_eq!(SNAPSHOT_LEN, 196_640);
    }

    #[test]
    fn an_interrupt_counts_only_between_submission_and_find() {
        assert_eq!(isr_for(100, 150, 200), Some(150));
        assert_eq!(isr_for(100, 100, 200), Some(100));
        assert_eq!(isr_for(100, 90, 200), None, "older than the submission");
        assert_eq!(
            isr_for(100, 250, 200),
            None,
            "after the find: not its interrupt"
        );
        assert_eq!(isr_for(100, 0, 200), None, "none taken");
        assert_eq!(isr_for(0, 150, 200), None, "submission time unknown");
    }

    #[test]
    fn stage_numbers_are_the_shared_table() {
        assert_eq!(
            [G_PRESENT, G_DEFER, G_SUBMIT, G_ISR, G_DONE, G_NOTIFY],
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            [
                G_FLIP_DDI,
                G_FLIP_SUBMIT,
                G_FLIP_ISR,
                G_FLIP_ACK,
                G_FLIP_RETIRE
            ],
            [16, 17, 18, 19, 20]
        );
        assert_eq!((KIND_FENCE, KIND_FLIP), (1, 2));
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
            render.display()
        );
        None
    }

    #[test]
    fn the_counters_the_driver_writes_are_listed_and_fit() {
        for n in COUNTERS {
            assert!(n.len() <= 14, "{n}");
        }
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join("ddi/stage_trace.rs")).unwrap();
        for n in COUNTERS.iter().chain(["StgRing"].iter()) {
            assert!(
                text.contains(&std::format!("b\"{n}\"")),
                "{n} is listed but not written by ddi/stage_trace.rs"
            );
        }
    }
}

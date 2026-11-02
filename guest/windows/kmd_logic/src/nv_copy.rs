//! The 610.88 Vulkan ICD's copy-engine work on its node-3 copy context.
//!
//! The ICD creates one context on node 3 (a copy engine, no HW queues, NVDA
//! private header version 0x00010004) and submits to it with
//! `D3DKMTSubmitCommand(Commands = 1, CommandLength = n)`: the DMA buffer is a
//! dummy, the work is `n` GPFIFO entries in the submission's private data
//! (from +0x354, the offset the record declares at +0x0c; `n` again at +0x18).
//! NVIDIA's KMD runs them on its own copy channel in the process's VA space.
//! They name pushbuffer segments of BLACKWELL_DMA_COPY_B methods on
//! subchannel 4: pitch copies (staging -> descriptor heaps, the shader code
//! heap) and semaphore releases the ICD waits for.
//!
//! Everything here is argument-only: the submission record and the method
//! stream are parsed into [`GpEntry`]s and [`Op`]s; `kmd_render`'s
//! `ddi::nv_copy` reads and writes the process's memory.

use crate::nvda::NVDA_MAGIC;

/// The record's version word (+0x04).
pub const SUBMIT_VERSION: u32 = 0x0001_0004;
/// +0x0c: the offset of the GPFIFO entries (0x354 in every captured record).
pub const ENTRIES_OFFSET_AT: usize = 0x0c;
/// +0x18: the entry count (`CommandLength`).
pub const ENTRY_COUNT_AT: usize = 0x18;
/// The offset every captured record declares.
pub const ENTRIES_AT: usize = 0x354;
/// Entries accepted per submission (the captures carry 1 or 5).
pub const MAX_ENTRIES: usize = 16;
/// Longest pushbuffer segment accepted (dwords).
pub const MAX_SEGMENT_DWORDS: u32 = 0x4000;

fn get32(buf: &[u8], at: usize) -> Option<u32> {
    let s = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// One GPFIFO entry: a pushbuffer segment of `dwords` method dwords at `va`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpEntry {
    pub va: u64,
    pub dwords: u32,
}

impl GpEntry {
    /// Decode a Volta+ GPFIFO entry (two dwords).
    pub const fn decode(lo: u32, hi: u32) -> Self {
        Self {
            va: ((hi as u64 & 0xff) << 32) | (lo & !3) as u64,
            dwords: (hi >> 10) & 0x1f_ffff,
        }
    }
}

/// The entries of one copy-context submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Submission {
    pub entries: [GpEntry; MAX_ENTRIES],
    pub count: usize,
}

impl Submission {
    pub fn entries(&self) -> &[GpEntry] {
        &self.entries[..self.count]
    }
}

/// Parse the ICD's copy-context record at the start of `buf` (the UMD private
/// data of the submission). `None` unless it is one: NVDA magic, version
/// 0x00010004, an entry offset inside the buffer and `command_length` entries
/// (1..=[`MAX_ENTRIES`]) agreeing with +0x18. Entries of length 0 (GPFIFO
/// control entries) are skipped; one past [`MAX_SEGMENT_DWORDS`] refuses the
/// record.
pub fn parse_submission(buf: &[u8], command_length: u32) -> Option<Submission> {
    if get32(buf, 0)? != NVDA_MAGIC || get32(buf, 4)? != SUBMIT_VERSION {
        return None;
    }
    let offset = get32(buf, ENTRIES_OFFSET_AT)? as usize;
    let count = get32(buf, ENTRY_COUNT_AT)?;
    if count != command_length || count == 0 || count as usize > MAX_ENTRIES {
        return None;
    }
    let mut out = Submission {
        entries: [GpEntry::default(); MAX_ENTRIES],
        count: 0,
    };
    for i in 0..count as usize {
        let at = offset.checked_add(i * 8)?;
        let entry = GpEntry::decode(get32(buf, at)?, get32(buf, at + 4)?);
        if entry.dwords == 0 {
            continue;
        }
        if entry.dwords > MAX_SEGMENT_DWORDS || entry.va == 0 {
            return None;
        }
        out.entries[out.count] = entry;
        out.count += 1;
    }
    Some(out)
}

/// Where in `buf` (the whole submission private data) the ICD's record
/// starts: offset 0 or any 4-byte-aligned offset holding the magic, the
/// version and the captured entry offset.
pub fn find_submission(buf: &[u8], command_length: u32) -> Option<Submission> {
    let last = buf.len().checked_sub(ENTRIES_AT + 8)?;
    (0..=last)
        .step_by(4)
        .filter(|at| {
            get32(buf, *at) == Some(NVDA_MAGIC)
                && get32(buf, at + 4) == Some(SUBMIT_VERSION)
                && get32(buf, at + ENTRIES_OFFSET_AT) == Some(ENTRIES_AT as u32)
        })
        .find_map(|at| parse_submission(&buf[at..], command_length))
}

/// One operation of a copy-engine method stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `line_count` lines of `line_len` bytes, `pitch_in` / `pitch_out` apart
    /// (one line unless MULTI_LINE_ENABLE).
    Copy {
        src: u64,
        dst: u64,
        line_len: u32,
        line_count: u32,
        pitch_in: u32,
        pitch_out: u32,
    },
    /// A semaphore release: the payload (8 bytes when `wide`, else 4) at `va`;
    /// a four-word release also writes a 64-bit timestamp at `va + 8` (the
    /// 32-bit payload's upper half is zero).
    Release {
        va: u64,
        payload: u64,
        wide: bool,
        four_word: bool,
    },
    /// A LAUNCH_DMA this executor does not implement (block-linear layouts,
    /// remapping, physical addressing, scrubbing, reductions), with its value.
    Unsupported(u32),
}

/// BLACKWELL_DMA_COPY_B (`clcab5.h`) method offsets used by the ICD.
pub mod method {
    pub const SET_SEMAPHORE_A: u32 = 0x240;
    pub const SET_SEMAPHORE_B: u32 = 0x244;
    pub const SET_SEMAPHORE_PAYLOAD: u32 = 0x248;
    pub const SET_SEMAPHORE_PAYLOAD_UPPER: u32 = 0x24c;
    pub const LAUNCH_DMA: u32 = 0x300;
    pub const OFFSET_IN_UPPER: u32 = 0x400;
    pub const OFFSET_IN_LOWER: u32 = 0x404;
    pub const OFFSET_OUT_UPPER: u32 = 0x408;
    pub const OFFSET_OUT_LOWER: u32 = 0x40c;
    pub const PITCH_IN: u32 = 0x410;
    pub const PITCH_OUT: u32 = 0x414;
    pub const LINE_LENGTH_IN: u32 = 0x418;
    pub const LINE_COUNT: u32 = 0x41c;
}

/// The subchannel the ICD drives the copy class on.
pub const COPY_SUBCHANNEL: u32 = 4;

/// What [`decode`] saw besides the operations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeStats {
    /// Methods on another subchannel or of an unknown offset (ignored).
    pub ignored: u32,
    /// The stream ended inside a method's data, or on a header this decoder
    /// does not know (the rest was not decoded).
    pub truncated: bool,
}

#[derive(Default)]
struct State {
    in_hi: u32,
    in_lo: u32,
    out_hi: u32,
    out_lo: u32,
    pitch_in: u32,
    pitch_out: u32,
    line_len: u32,
    line_count: u32,
    sem_hi: u32,
    sem_lo: u32,
    payload: u32,
    payload_hi: u32,
}

impl State {
    fn method(
        &mut self,
        subchannel: u32,
        m: u32,
        v: u32,
        stats: &mut DecodeStats,
        emit: &mut impl FnMut(Op),
    ) {
        if m < 0x100 {
            // Host (GPFIFO class) methods: NOPs and the like, nothing to run.
            return;
        }
        if subchannel != COPY_SUBCHANNEL {
            stats.ignored += 1;
            return;
        }
        match m {
            method::SET_SEMAPHORE_A => self.sem_hi = v,
            method::SET_SEMAPHORE_B => self.sem_lo = v,
            method::SET_SEMAPHORE_PAYLOAD => self.payload = v,
            method::SET_SEMAPHORE_PAYLOAD_UPPER => self.payload_hi = v,
            method::OFFSET_IN_UPPER => self.in_hi = v,
            method::OFFSET_IN_LOWER => self.in_lo = v,
            method::OFFSET_OUT_UPPER => self.out_hi = v,
            method::OFFSET_OUT_LOWER => self.out_lo = v,
            method::PITCH_IN => self.pitch_in = v,
            method::PITCH_OUT => self.pitch_out = v,
            method::LINE_LENGTH_IN => self.line_len = v,
            method::LINE_COUNT => self.line_count = v,
            method::LAUNCH_DMA => self.launch(v, emit),
            _ => stats.ignored += 1,
        }
    }

    fn launch(&mut self, v: u32, emit: &mut impl FnMut(Op)) {
        let transfer = v & 3;
        let semaphore = (v >> 3) & 3;
        let src_pitch = v & (1 << 7) != 0;
        let dst_pitch = v & (1 << 8) != 0;
        let multi_line = v & (1 << 9) != 0;
        // REMAP_ENABLE (10), SRC_TYPE/DST_TYPE physical (12, 13), the
        // semaphore reduction (19), MEMORY_SCRUB_ENABLE (23).
        let unsupported = v & ((1 << 10) | (1 << 12) | (1 << 13) | (1 << 19) | (1 << 23)) != 0;
        if transfer != 0 {
            if unsupported || !src_pitch || !dst_pitch {
                emit(Op::Unsupported(v));
            } else {
                emit(Op::Copy {
                    src: (u64::from(self.in_hi & 0x1ff_ffff) << 32) | u64::from(self.in_lo),
                    dst: (u64::from(self.out_hi & 0x1ff_ffff) << 32) | u64::from(self.out_lo),
                    line_len: self.line_len,
                    line_count: if multi_line { self.line_count } else { 1 },
                    pitch_in: self.pitch_in,
                    pitch_out: self.pitch_out,
                });
            }
        }
        if semaphore == 1 || semaphore == 2 {
            if v & (1 << 19) != 0 {
                emit(Op::Unsupported(v));
                return;
            }
            let wide = v & (1 << 27) != 0;
            emit(Op::Release {
                va: (u64::from(self.sem_hi & 0x1ff_ffff) << 32) | u64::from(self.sem_lo),
                payload: if wide {
                    (u64::from(self.payload_hi) << 32) | u64::from(self.payload)
                } else {
                    u64::from(self.payload)
                },
                wide,
                four_word: semaphore == 2,
            });
        }
    }
}

/// Decode a pushbuffer segment (Volta+ method headers) into the copy-engine
/// operations it asks for, in order.
pub fn decode(dwords: &[u32], mut emit: impl FnMut(Op)) -> DecodeStats {
    let mut stats = DecodeStats::default();
    let mut state = State::default();
    let mut i = 0usize;
    while i < dwords.len() {
        let h = dwords[i];
        i += 1;
        if h == 0 {
            continue;
        }
        let op = h >> 29;
        let address = (h & 0xfff) << 2;
        let subchannel = (h >> 13) & 7;
        let count = ((h >> 16) & 0x1fff) as usize;
        match op {
            // INC / NON_INC / ONE_INC: `count` data dwords follow.
            1 | 3 | 5 => {
                for k in 0..count {
                    let Some(&value) = dwords.get(i) else {
                        stats.truncated = true;
                        return stats;
                    };
                    i += 1;
                    let m = match op {
                        1 => address + 4 * k as u32,
                        3 => address,
                        _ if k == 0 => address,
                        _ => address + 4,
                    };
                    state.method(subchannel, m, value, &mut stats, &mut emit);
                }
            }
            // IMMD: the data is the count field.
            4 => state.method(subchannel, address, count as u32, &mut stats, &mut emit),
            // SET_SUBDEVICE_MASK and the other tertiary operations: no data.
            0 => {}
            // END_PB_SEGMENT.
            7 => return stats,
            _ => {
                stats.truncated = true;
                return stats;
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    /// vktest capture, seq 3606 (vk8 trace): the copy-context submission that
    /// uploads the compute shaders, 892 bytes; the five entries at +0x354.
    fn code_upload_record() -> Vec<u8> {
        let mut b = std::vec![0u8; 892];
        b[..0x10].copy_from_slice(&[
            0x41, 0x44, 0x56, 0x4e, 0x04, 0x00, 0x01, 0x00, 0, 0, 0, 0, 0x54, 0x03, 0, 0,
        ]);
        b[0x18] = 5;
        b[0x354..0x37c].copy_from_slice(&[
            0x00, 0x11, 0x06, 0x04, 0x00, 0x11, 0x00, 0x00, 0x80, 0x11, 0x06, 0x04, 0x00, 0x05,
            0x00, 0x00, 0x00, 0x12, 0x06, 0x04, 0x00, 0xa1, 0x05, 0x00, 0x00, 0x18, 0x06, 0x04,
            0x00, 0x05, 0x00, 0x00, 0x80, 0x18, 0x06, 0x04, 0x00, 0x9d, 0x04, 0x00,
        ]);
        b
    }

    #[test]
    fn the_code_upload_record_names_five_segments() {
        let record = code_upload_record();
        let s = parse_submission(&record, 5).unwrap();
        assert_eq!(
            s.entries(),
            &[
                GpEntry {
                    va: 0x406_1100,
                    dwords: 4
                },
                GpEntry {
                    va: 0x406_1180,
                    dwords: 1
                },
                GpEntry {
                    va: 0x406_1200,
                    dwords: 360
                },
                GpEntry {
                    va: 0x406_1800,
                    dwords: 1
                },
                GpEntry {
                    va: 0x406_1880,
                    dwords: 295
                },
            ]
        );
        // CommandLength must agree with +0x18.
        assert_eq!(parse_submission(&record, 4), None);
        // Found at the start of a larger private-data block, or further in.
        let mut whole = std::vec![0u8; 4512];
        whole[..892].copy_from_slice(&record);
        assert_eq!(find_submission(&whole, 5), Some(s));
        let mut later = std::vec![0u8; 4512];
        later[4512 - 892..].copy_from_slice(&record);
        assert_eq!(find_submission(&later, 5), Some(s));
        assert_eq!(find_submission(&std::vec![0u8; 4512], 5), None);
    }

    #[test]
    fn the_single_entry_record_of_device_creation() {
        // Seq 2811: one entry, 0x4060000, 26 dwords; 860 bytes.
        let mut b = std::vec![0u8; 860];
        b[..0x10].copy_from_slice(&[
            0x41, 0x44, 0x56, 0x4e, 0x04, 0x00, 0x01, 0x00, 0, 0, 0, 0, 0x54, 0x03, 0, 0,
        ]);
        b[0x18] = 1;
        b[0x354..0x35c].copy_from_slice(&[0x00, 0x00, 0x06, 0x04, 0x00, 0x69, 0x00, 0x00]);
        let s = parse_submission(&b, 1).unwrap();
        assert_eq!(
            s.entries(),
            &[GpEntry {
                va: 0x406_0000,
                dwords: 26
            }]
        );
    }

    fn ops(dwords: &[u32]) -> (Vec<Op>, DecodeStats) {
        let mut out = Vec::new();
        let stats = decode(dwords, |op| out.push(op));
        (out, stats)
    }

    #[test]
    fn device_creation_segment_copies_and_releases() {
        // The 26-dword segment at 0x4060000 (seq 2811): two pitch copies from
        // the staging heap and a one-word release of 5 at 0x4124bb0.
        let seg = [
            0x2002_8100,
            0x0000_0000,
            0x0423_a000,
            0x2002_8102,
            0x0000_0000,
            0x0440_e000,
            0x2001_8106,
            0x0000_0100,
            0x2001_80c0,
            0x0000_0186,
            0x2002_8100,
            0x0000_0000,
            0x0423_a110,
            0x2002_8102,
            0x0000_0000,
            0x0440_e100,
            0x2001_8106,
            0x0000_09f0,
            0x2001_80c0,
            0x0000_0186,
            0x2003_8090,
            0x0000_0000,
            0x0412_4bb0,
            0x0000_0005,
            0x2001_80c0,
            0x0000_000c,
        ];
        let (got, stats) = ops(&seg);
        assert_eq!(stats, DecodeStats::default());
        assert_eq!(
            got,
            std::vec![
                Op::Copy {
                    src: 0x423_a000,
                    dst: 0x440_e000,
                    line_len: 0x100,
                    line_count: 1,
                    pitch_in: 0,
                    pitch_out: 0
                },
                Op::Copy {
                    src: 0x423_a110,
                    dst: 0x440_e100,
                    line_len: 0x9f0,
                    line_count: 1,
                    pitch_in: 0,
                    pitch_out: 0
                },
                Op::Release {
                    va: 0x412_4bb0,
                    payload: 5,
                    wide: false,
                    four_word: false
                },
            ]
        );
    }

    #[test]
    fn code_upload_lands_in_the_code_heap_alias() {
        // The start of the 295-dword segment at 0x4061880 (seq 3606): the
        // first copy writes 0x380 bytes to 0x2_0000_0000.
        let seg = [
            0x0001_fff0,
            0x2002_8100,
            0x0000_0000,
            0x0424_5cb0,
            0x2002_8102,
            0x0000_0002,
            0x0000_0000,
            0x2001_8106,
            0x0000_0380,
            0x2001_80c0,
            0x0000_0186,
        ];
        let (got, _) = ops(&seg);
        assert_eq!(
            got,
            std::vec![Op::Copy {
                src: 0x424_5cb0,
                dst: 0x2_0000_0000,
                line_len: 0x380,
                line_count: 1,
                pitch_in: 0,
                pitch_out: 0
            }]
        );
    }

    #[test]
    fn four_word_release_and_immediate_methods() {
        // Seq 2825's 0x4060800 segment: a four-word release of 1 at 0x4034070.
        let seg = [
            0x0001_fff0,
            0x2003_8090,
            0x0000_0000,
            0x0403_4070,
            0x0000_0001,
            0x2001_80c0,
            0x0000_0014,
        ];
        let (got, _) = ops(&seg);
        assert_eq!(
            got,
            std::vec![Op::Release {
                va: 0x403_4070,
                payload: 1,
                wide: false,
                four_word: true
            }]
        );
        // IMMD LAUNCH_DMA (data in the count field): a release with no copy.
        let (got, _) = ops(&[0x2003_8090, 0, 0x1000, 7, 0x800c_80c0]);
        assert_eq!(
            got,
            std::vec![Op::Release {
                va: 0x1000,
                payload: 7,
                wide: false,
                four_word: false
            }]
        );
    }

    #[test]
    fn multi_line_and_unsupported_launches() {
        let seg = [
            0x2008_8100,
            0,
            0x1000,
            0,
            0x2000,
            0x40,
            0x80,
            0x20,
            3,
            0x2001_80c0,
            0x0000_0386,
        ];
        let (got, _) = ops(&seg);
        assert_eq!(
            got,
            std::vec![Op::Copy {
                src: 0x1000,
                dst: 0x2000,
                line_len: 0x20,
                line_count: 3,
                pitch_in: 0x40,
                pitch_out: 0x80
            }]
        );
        // Block-linear destination (bit 8 clear) and remapping are not run.
        let (got, _) = ops(&[0x2001_80c0, 0x0000_0086]);
        assert_eq!(got, std::vec![Op::Unsupported(0x86)]);
        let (got, _) = ops(&[0x2001_80c0, 0x0000_0586]);
        assert_eq!(got, std::vec![Op::Unsupported(0x586)]);
    }

    #[test]
    fn other_subchannels_and_truncation_are_reported() {
        // A compute-class method on subchannel 1, then a header whose data is cut.
        let (got, stats) = ops(&[0x2001_2144, 0x42_46a8, 0x2002_8100, 0]);
        assert!(got.is_empty());
        assert_eq!(
            stats,
            DecodeStats {
                ignored: 1,
                truncated: true
            }
        );
    }
}

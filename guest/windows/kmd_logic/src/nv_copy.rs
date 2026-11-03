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

/// The node the ICD creates its copy context on.
pub const COPY_CONTEXT_NODE: u32 = 3;
/// The context header's tag at +0x0c: `NVGL`.
const NVGL_TAG: u32 = 0x4e56_474c;

/// Whether a CreateContext's private data is the ICD's copy context: node 3,
/// an NVDA header of version 0x00010004 with the NVGL tag at +0x0c and its own
/// size declared at +0x08 (0xdf1 bytes in the capture).
pub fn is_copy_context(buf: &[u8], node_ordinal: u32) -> bool {
    node_ordinal == COPY_CONTEXT_NODE
        && get32(buf, 0) == Some(NVDA_MAGIC)
        && get32(buf, 4) == Some(SUBMIT_VERSION)
        && get32(buf, 8) == u32::try_from(buf.len()).ok()
        && get32(buf, 0x0c) == Some(NVGL_TAG)
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

/// The most entries a record may declare on the channel path: one channel
/// submission takes them and its release ([`ring::MAX_SUBMIT_ENTRIES`]). The
/// largest record NVIDIA's private-data size allows (4512 bytes, entries from
/// +0x354) has 457.
pub const MAX_RECORD_ENTRIES: usize = ring::MAX_SUBMIT_ENTRIES as usize - 1;

/// Why a submission's private data held no record the channel path takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// No NVDA / version 0x00010004 / +0x354 header anywhere in the data.
    NotFound,
    /// The header's count (+0x18) is not the submission's `CommandLength`.
    CountMismatch,
    /// More than [`MAX_RECORD_ENTRIES`] entries, or none.
    Count,
    /// The entries run past the private data.
    OutOfBounds,
    /// A segment of nonzero length at GPU VA 0.
    NullVa,
}

impl RecordError {
    /// 1-based code for diagnostics.
    pub const fn code(self) -> u32 {
        match self {
            Self::NotFound => 1,
            Self::CountMismatch => 2,
            Self::Count => 3,
            Self::OutOfBounds => 4,
            Self::NullVa => 5,
        }
    }
}

/// The ICD's copy record inside a submission's private data, validated for the
/// channel path: the GPU fetches the segments itself, so unlike
/// [`parse_submission`] (the CPU fallback's 16-entry copy) neither the entry
/// count up to [`MAX_RECORD_ENTRIES`] nor a segment's length is limited here
/// (the GPFIFO entry's own 21-bit length field is the bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Where the entries start in the private data.
    pub entries_at: usize,
    /// The declared entry count (control entries of length 0 included).
    pub count: usize,
}

impl Record {
    /// The segments, GPFIFO control entries (length 0) skipped.
    pub fn entries<'a>(&self, buf: &'a [u8]) -> impl Iterator<Item = GpEntry> + 'a {
        let at = self.entries_at;
        (0..self.count).filter_map(move |i| {
            let e = at + i * 8;
            let entry = GpEntry::decode(get32(buf, e)?, get32(buf, e + 4)?);
            (entry.dwords != 0).then_some(entry)
        })
    }

    /// How many segments [`Record::entries`] yields.
    pub fn segments(&self, buf: &[u8]) -> usize {
        self.entries(buf).count()
    }
}

fn record_at(buf: &[u8], at: usize, command_length: u32) -> Result<Record, RecordError> {
    let header = buf.get(at..).ok_or(RecordError::NotFound)?;
    let offset = get32(header, ENTRIES_OFFSET_AT).ok_or(RecordError::NotFound)? as usize;
    let count = get32(header, ENTRY_COUNT_AT).ok_or(RecordError::NotFound)?;
    if count != command_length {
        return Err(RecordError::CountMismatch);
    }
    if count == 0 || count as usize > MAX_RECORD_ENTRIES {
        return Err(RecordError::Count);
    }
    let entries_at = at.checked_add(offset).ok_or(RecordError::OutOfBounds)?;
    let end = entries_at
        .checked_add(count as usize * 8)
        .ok_or(RecordError::OutOfBounds)?;
    if end > buf.len() {
        return Err(RecordError::OutOfBounds);
    }
    let record = Record {
        entries_at,
        count: count as usize,
    };
    if record.entries(buf).any(|entry| entry.va == 0) {
        return Err(RecordError::NullVa);
    }
    Ok(record)
}

/// [`find_submission`] for the channel path: the first 4-byte-aligned header
/// (magic, version, entry offset 0x354) in `buf`, validated as a [`Record`].
/// When several headers match, the first valid one wins, else the first
/// header's error is returned.
pub fn find_record(buf: &[u8], command_length: u32) -> Result<Record, RecordError> {
    let Some(last) = buf.len().checked_sub(ENTRIES_AT + 8) else {
        return Err(RecordError::NotFound);
    };
    let mut first_error = RecordError::NotFound;
    for at in (0..=last).step_by(4) {
        if get32(buf, at) != Some(NVDA_MAGIC)
            || get32(buf, at + 4) != Some(SUBMIT_VERSION)
            || get32(buf, at + ENTRIES_OFFSET_AT) != Some(ENTRIES_AT as u32)
        {
            continue;
        }
        match record_at(buf, at, command_length) {
            Ok(record) => return Ok(record),
            Err(error) if first_error == RecordError::NotFound => first_error = error,
            Err(_) => {}
        }
    }
    Err(first_error)
}

impl GpEntry {
    /// The two GPFIFO dwords for this segment (main level, no sync).
    pub const fn encode(self) -> (u32, u32) {
        (
            (self.va as u32) & !3,
            ((self.va >> 32) as u32 & 0xff) | (self.dwords << 10),
        )
    }
}

/// The KMD's own copy channel for the copy context (`kmd_render`'s
/// `CopyChannel`): one system-memory block, mapped for the GPU in the
/// process's VA space and for the CPU in the kernel, holds the GPFIFO ring, a
/// release segment per in-flight submission and the progress semaphore.
pub mod ring {
    /// The block.
    pub const BYTES: u64 = 0x1_0000;
    /// GPFIFO entries at offset 0.
    pub const ENTRIES: u32 = 1024;
    pub const GPFIFO_BYTES: u32 = ENTRIES * 8;
    /// Release segments, one slot per submission (by sequence).
    pub const RELEASE_AT: u64 = 0x2000;
    pub const RELEASE_SLOT_BYTES: u64 = 32;
    pub const RELEASE_SLOTS: u64 = 512;
    /// The progress semaphore (64 bits): the last sequence the channel finished.
    pub const SEMAPHORE_AT: u64 = RELEASE_AT + RELEASE_SLOT_BYTES * RELEASE_SLOTS;
    /// Entries one submission may use: its segments and its release. Anything
    /// the ring can hold at once (one entry always stays empty): the channel
    /// path takes whole records ([`super::MAX_RECORD_ENTRIES`]), not only the
    /// CPU fallback's 16 entries.
    pub const MAX_SUBMIT_ENTRIES: u32 = ENTRIES - 1;
    const _: () = assert!(SEMAPHORE_AT + 8 <= BYTES);
    const _: () = assert!(GPFIFO_BYTES as u64 <= RELEASE_AT);
    // Every submission takes at least two entries (a segment and its release),
    // so fewer than RELEASE_SLOTS are in flight: a slot is free again before its
    // sequence comes round.
    const _: () = assert!(((ENTRIES - 1) as u64) / 2 < RELEASE_SLOTS);

    /// The async-CE channel's init segment (`SET_OBJECT` of the copy class on
    /// subchannel 4, [`super::set_object_segment`]): its first GPFIFO entry.
    pub const INIT_AT: u64 = SEMAPHORE_AT + 0x100;
    pub const INIT_DWORDS: u32 = 2;
    const _: () = assert!(INIT_AT + 4 * INIT_DWORDS as u64 <= BYTES);

    /// The release slot's offset in the block for sequence `seq`.
    pub const fn release_offset(seq: u64) -> u64 {
        RELEASE_AT + (seq % RELEASE_SLOTS) * RELEASE_SLOT_BYTES
    }

    /// Free GPFIFO entries with the channel at `get` and the KMD at `put` (one
    /// entry always stays empty: put == get means an empty ring).
    pub const fn room(put: u32, get: u32) -> u32 {
        let used = (put + ENTRIES - get % ENTRIES) % ENTRIES;
        ENTRIES - 1 - used
    }
}

/// Where the KMD's copy channel is in its ring, without USERD `GP_GET` (GB20x under GSP never
/// writes it back: read from USERD it stays 0, so a ring that once took ~1023 entries looked
/// full for good and every later submission waited in the backlog forever). The channel runs
/// its ring in order and every submission ends with the KMD's release of its sequence, so the
/// progress semaphore names how far the ring is consumed: the GP_PUT after that sequence's
/// release entry, kept per sequence here (fewer than [`ring::RELEASE_SLOTS`] are ever in
/// flight: each takes two entries or more of a 1023-entry ring).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingTrack {
    put: u32,
    /// GP_PUT after each sequence's release entry, by `seq % RELEASE_SLOTS`.
    ends: [u16; ring::RELEASE_SLOTS as usize],
    /// The newest sequence on the ring (0 = none yet).
    last: u64,
}

impl Default for RingTrack {
    fn default() -> Self {
        Self::new()
    }
}

impl RingTrack {
    pub const fn new() -> Self {
        Self {
            put: 0,
            ends: [0; ring::RELEASE_SLOTS as usize],
            last: 0,
        }
    }

    /// The KMD's GP_PUT.
    pub const fn put(&self) -> u32 {
        self.put
    }

    /// The newest sequence on the ring.
    pub const fn last(&self) -> u64 {
        self.last
    }

    /// The ring index the channel is done with, given its progress semaphore `completed`.
    /// Before the first sequence completes, 0 (entries written before it, such as the init
    /// segment, count as not consumed). A `completed` older than the per-sequence window
    /// cannot be placed: the answer is the one that leaves no room.
    pub fn consumed(&self, completed: u64) -> u32 {
        if completed == 0 || self.last == 0 {
            return 0;
        }
        let c = if completed > self.last { self.last } else { completed };
        if self.last - c >= ring::RELEASE_SLOTS {
            return (self.put + 1) % ring::ENTRIES;
        }
        u32::from(self.ends[(c % ring::RELEASE_SLOTS) as usize])
    }

    /// Free entries with the channel at `completed`.
    pub fn room(&self, completed: u64) -> u32 {
        ring::room(self.put, self.consumed(completed))
    }

    /// Entries pending on the ring (written, not yet consumed) at `completed`.
    pub fn pending(&self, completed: u64) -> u32 {
        ring::ENTRIES - 1 - self.room(completed)
    }

    /// `n` entries written ahead of any sequence (the init segment); the new GP_PUT.
    pub fn advance_init(&mut self, n: u32) -> u32 {
        self.put = (self.put + n) % ring::ENTRIES;
        self.put
    }

    /// `n` entries written for `seq` (its segments and its release, the last of them); the
    /// new GP_PUT. Sequences arrive in order.
    pub fn advance(&mut self, seq: u64, n: u32) -> u32 {
        self.put = (self.put + n) % ring::ENTRIES;
        self.ends[(seq % ring::RELEASE_SLOTS) as usize] = self.put as u16;
        self.last = seq;
        self.put
    }
}

/// The init segment of an async copy-engine channel: `SET_OBJECT(copy_class)` on subchannel 4,
/// where the ICD's segments drive the copy class without binding it (a graphics channel's
/// subchannel 4 is the GRCE object RM binds; an async CE channel needs it bound).
pub const fn set_object_segment(copy_class: u32) -> [u32; ring::INIT_DWORDS as usize] {
    // INC, one dword, subchannel 4, method 0 (SET_OBJECT).
    [0x2001_8000, copy_class]
}

/// `NvCcxEng`: which engine the KMD's copy channel runs on.
pub mod engine {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Mode {
        /// An asynchronous copy engine (its own runlist, its own TSG), as NVIDIA's node 3
        /// (`DXGK_ENGINE_TYPE_COPY`) is; the graphics channel when that cannot be built.
        Async,
        /// The asynchronous copy engine or nothing (the CPU fallback / rejection).
        AsyncOnly,
        /// A graphics channel (GRCE on subchannel 4) in a TSG of its own on the graphics
        /// runlist: it runs only when the GR engine switches to it, so an app TSG that holds
        /// GR waiting for a value this channel releases deadlocks both.
        Graphics,
    }

    /// The knob's value (unset / 0 = [`Mode::Async`]).
    pub const fn mode(knob: u32) -> Mode {
        match knob {
            1 => Mode::Graphics,
            2 => Mode::AsyncOnly,
            _ => Mode::Async,
        }
    }

    /// `NvCcxEngUsed`: what a built channel runs on.
    pub const USED_GRAPHICS: u32 = 1;
    pub const USED_ASYNC: u32 = 2;
}

/// LAUNCH_DMA for the KMD's release: a one-word semaphore release
/// (SEMAPHORE_TYPE 1) with a 64-bit payload (SEMAPHORE_PAYLOAD_SIZE), flushed.
pub const LAUNCH_RELEASE_64: u32 = (1 << 27) | (1 << 3) | (1 << 2);
pub const RELEASE_DWORDS: usize = 7;

/// The KMD's segment after a submission's own: the copy engine writes `value`
/// (64 bits) to `semaphore_va` once everything before it on the channel is done.
pub const fn release_segment(semaphore_va: u64, value: u64) -> [u32; RELEASE_DWORDS] {
    [
        // SET_SEMAPHORE_A..PAYLOAD_UPPER (0x240..0x24c) on subchannel 4.
        0x2004_8090,
        (semaphore_va >> 32) as u32,
        semaphore_va as u32,
        value as u32,
        (value >> 32) as u32,
        // LAUNCH_DMA (0x300) on subchannel 4.
        0x2001_80c0,
        LAUNCH_RELEASE_64,
    ]
}

/// A copy-context job's gate in a WDDM pending entry (`kmd_render` puts it in
/// the GDI gate field): bit 63 marks it, bits 48..56 name the channel slot, the
/// low 48 bits the sequence.
pub const GATE_TAG: u64 = 1 << 63;
pub const GATE_CPU_SLOT: usize = 0xff;

pub const fn gate(slot: usize, seq: u64) -> u64 {
    GATE_TAG | ((slot as u64 & 0xff) << 48) | (seq & 0xffff_ffff_ffff)
}

/// `(slot, seq)` of a tagged gate, `None` for any other gate value.
pub const fn gate_parts(gate: u64) -> Option<(usize, u64)> {
    if gate & GATE_TAG == 0 {
        None
    } else {
        Some((((gate >> 48) & 0xff) as usize, gate & 0xffff_ffff_ffff))
    }
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
    fn the_captured_copy_context_header_is_recognised() {
        // L3 capture seq 123474: NodeOrdinal 3, 3569 bytes.
        let mut b = std::vec![0u8; 0xdf1];
        b[..0x10].copy_from_slice(&[
            0x41, 0x44, 0x56, 0x4e, 0x04, 0x00, 0x01, 0x00, 0xf1, 0x0d, 0x00, 0x00, 0x4c, 0x47,
            0x56, 0x4e,
        ]);
        assert!(is_copy_context(&b, 3));
        assert!(!is_copy_context(&b, 4));
        assert!(!is_copy_context(&b[..0xdf0], 3));
        let mut d3d = b.clone();
        d3d[0x0c..0x10].copy_from_slice(&0x4e56_4458u32.to_le_bytes());
        assert!(!is_copy_context(&d3d, 3));
    }

    #[test]
    fn entries_encode_back_to_the_captured_dwords() {
        // Seq 3606's last entry: 80 18 06 04 / 00 9d 04 00.
        let entry = GpEntry::decode(0x0406_1880, 0x0004_9d00);
        assert_eq!(
            entry,
            GpEntry {
                va: 0x406_1880,
                dwords: 295
            }
        );
        assert_eq!(entry.encode(), (0x0406_1880, 0x0004_9c00));
        assert_eq!(GpEntry::decode(entry.encode().0, entry.encode().1), entry);
        let high = GpEntry {
            va: 0x2_0000_1000,
            dwords: 7,
        };
        assert_eq!(GpEntry::decode(high.encode().0, high.encode().1), high);
    }

    #[test]
    fn the_kmd_release_decodes_as_a_64_bit_release() {
        let seg = release_segment(0x1_2345_6000, 0x1_0000_0002);
        let (got, stats) = ops(&seg);
        assert_eq!(stats, DecodeStats::default());
        assert_eq!(
            got,
            std::vec![Op::Release {
                va: 0x1_2345_6000,
                payload: 0x1_0000_0002,
                wide: true,
                four_word: false
            }]
        );
    }

    #[test]
    fn ring_room_and_release_slots() {
        assert_eq!(ring::room(0, 0), ring::ENTRIES - 1);
        assert_eq!(ring::room(10, 0), ring::ENTRIES - 11);
        assert_eq!(ring::room(5, 6), 0);
        assert_eq!(ring::room(0, 1), 0);
        assert_eq!(ring::room(3, 1000), ring::ENTRIES - 1 - 27);
        assert_eq!(ring::release_offset(0), ring::RELEASE_AT);
        assert_eq!(
            ring::release_offset(513),
            ring::RELEASE_AT + ring::RELEASE_SLOT_BYTES
        );
        assert!(ring::release_offset(511) + ring::RELEASE_SLOT_BYTES <= ring::SEMAPHORE_AT);
    }

    #[test]
    fn the_ring_is_consumed_by_completed_sequence_not_gp_get() {
        let mut t = RingTrack::new();
        assert_eq!(t.room(0), ring::ENTRIES - 1);
        // GB20x never writes USERD GP_GET back: with GP_GET pinned at 0 the old room check
        // refused everything once ~1023 entries had gone through, whatever had completed.
        let mut seq = 0u64;
        let mut completed = 0u64;
        for _ in 0..10_000 {
            seq += 1;
            let need = 1 + (seq % 300) as u32;
            if t.room(completed) < need {
                // The channel catches up.
                completed = t.last();
                assert_eq!(t.room(completed), ring::ENTRIES - 1);
            }
            assert!(t.room(completed) >= need);
            t.advance(seq, need);
        }
        assert!(t.put() < ring::ENTRIES);
        // The old check, `room(put, GP_GET)` with GP_GET read as 0: once put is past
        // 1023 - need it refuses with nothing outstanding, and nothing ever changes it.
        let mut t = RingTrack::new();
        t.advance(1, 1000);
        assert_eq!(t.room(1), ring::ENTRIES - 1);
        assert!(ring::room(t.put(), 0) < 30);
    }

    #[test]
    fn ring_track_partial_progress_and_stale_values() {
        let mut t = RingTrack::new();
        // The async channel's init segment: not consumed until a sequence completes.
        assert_eq!(t.advance_init(1), 1);
        assert_eq!(t.room(0), ring::ENTRIES - 2);
        t.advance(1, 3); // put 4
        t.advance(2, 5); // put 9
        t.advance(3, 2); // put 11
        assert_eq!(t.consumed(0), 0);
        assert_eq!(t.consumed(1), 4);
        assert_eq!(t.consumed(2), 9);
        assert_eq!(t.consumed(3), 11);
        // A value past the newest sequence (cannot happen) clamps.
        assert_eq!(t.consumed(9), 11);
        assert_eq!(t.pending(1), 7);
        assert_eq!(t.pending(3), 0);
        // Wrap: the ends wrap with the ring.
        let mut t = RingTrack::new();
        t.advance(1, 1000);
        t.advance(2, 100);
        assert_eq!(t.put(), 76);
        assert_eq!(t.consumed(1), 1000);
        assert_eq!(t.room(1), ring::ENTRIES - 1 - 100);
        // A completed value older than the window leaves no room.
        let mut t = RingTrack::new();
        for s in 1..=600u64 {
            t.advance(s, 1);
        }
        assert_eq!(t.room(600 - ring::RELEASE_SLOTS), 0);
        assert_eq!(t.room(600 - ring::RELEASE_SLOTS + 1), ring::ENTRIES - 1 - (ring::RELEASE_SLOTS as u32 - 1));
    }

    #[test]
    fn set_object_segment_binds_subchannel_four() {
        let seg = set_object_segment(0xcab5);
        // The host method decoder ignores it (method 0 < 0x100), nothing runs.
        let (got, stats) = ops(&seg);
        assert!(got.is_empty());
        assert_eq!(stats, DecodeStats::default());
        // Header fields: INC, count 1, subchannel 4, method 0.
        assert_eq!(seg[0] >> 29, 1);
        assert_eq!((seg[0] >> 16) & 0x1fff, 1);
        assert_eq!((seg[0] >> 13) & 7, COPY_SUBCHANNEL);
        assert_eq!(seg[0] & 0xfff, 0);
        assert_eq!(seg[1], 0xcab5);
        assert!(ring::INIT_AT >= ring::SEMAPHORE_AT + 8);
    }

    #[test]
    fn engine_knob() {
        assert_eq!(engine::mode(0), engine::Mode::Async);
        assert_eq!(engine::mode(u32::MAX), engine::Mode::Async);
        assert_eq!(engine::mode(1), engine::Mode::Graphics);
        assert_eq!(engine::mode(2), engine::Mode::AsyncOnly);
    }

    #[test]
    fn gates_carry_slot_and_sequence() {
        let g = gate(3, 0x1234);
        assert_eq!(gate_parts(g), Some((3, 0x1234)));
        assert_eq!(gate_parts(gate(GATE_CPU_SLOT, 9)), Some((GATE_CPU_SLOT, 9)));
        // A GDI executor sequence is not a copy gate.
        assert_eq!(gate_parts(17), None);
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

    /// A record of `n` entries (segments of 4 dwords at 0x4000_0000 + i * 0x100)
    /// in a 4512-byte private-data block.
    fn long_record(n: usize) -> Vec<u8> {
        let mut b = std::vec![0u8; 4512];
        b[..0x10].copy_from_slice(&[
            0x41, 0x44, 0x56, 0x4e, 0x04, 0x00, 0x01, 0x00, 0, 0, 0, 0, 0x54, 0x03, 0, 0,
        ]);
        b[0x18..0x1c].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            let (lo, hi) = GpEntry {
                va: 0x4000_0000 + i as u64 * 0x100,
                dwords: 4,
            }
            .encode();
            let at = ENTRIES_AT + i * 8;
            if at + 8 > b.len() {
                break;
            }
            b[at..at + 4].copy_from_slice(&lo.to_le_bytes());
            b[at + 4..at + 8].copy_from_slice(&hi.to_le_bytes());
        }
        b
    }

    #[test]
    fn the_channel_path_takes_the_captured_records_too() {
        let record = code_upload_record();
        let r = find_record(&record, 5).unwrap();
        let channel: Vec<GpEntry> = r.entries(&record).collect();
        assert_eq!(channel, parse_submission(&record, 5).unwrap().entries());
        assert_eq!(r.segments(&record), 5);
        assert_eq!(find_record(&record, 4), Err(RecordError::CountMismatch));
        assert_eq!(find_record(&std::vec![0u8; 4512], 1), Err(RecordError::NotFound));
        assert_eq!(find_record(&[], 1), Err(RecordError::NotFound));
    }

    #[test]
    fn records_past_sixteen_entries_and_long_segments_reach_the_channel() {
        // 17 and 200 entries: the CPU fallback's copy refuses them (`NvCcNoRec`, the
        // work silently skipped before), the channel path takes all of them.
        for n in [17usize, 200, 457] {
            let b = long_record(n);
            assert_eq!(parse_submission(&b, n as u32), None);
            let r = find_record(&b, n as u32).unwrap();
            assert_eq!(r.segments(&b), n);
            let last = r.entries(&b).last().unwrap();
            assert_eq!(last.va, 0x4000_0000 + (n as u64 - 1) * 0x100);
        }
        // 458 entries do not fit in 4512 bytes.
        assert_eq!(find_record(&long_record(458), 458), Err(RecordError::OutOfBounds));
        // A segment past the CPU fallback's 0x4000 dwords.
        let mut b = long_record(1);
        let (lo, hi) = GpEntry {
            va: 0x5000_0000,
            dwords: 0x8000,
        }
        .encode();
        b[ENTRIES_AT..ENTRIES_AT + 4].copy_from_slice(&lo.to_le_bytes());
        b[ENTRIES_AT + 4..ENTRIES_AT + 8].copy_from_slice(&hi.to_le_bytes());
        assert_eq!(parse_submission(&b, 1), None);
        let r = find_record(&b, 1).unwrap();
        assert_eq!(
            r.entries(&b).collect::<Vec<_>>(),
            std::vec![GpEntry {
                va: 0x5000_0000,
                dwords: 0x8000
            }]
        );
        // One whole record always fits one channel submission with its release.
        assert!(MAX_RECORD_ENTRIES + 1 <= ring::MAX_SUBMIT_ENTRIES as usize);
        assert!(ring::MAX_SUBMIT_ENTRIES < ring::ENTRIES);
        assert!(457 <= MAX_RECORD_ENTRIES);
    }

    #[test]
    fn record_refusals() {
        // Control entries (length 0) are skipped, a null segment refuses.
        let mut b = long_record(3);
        b[ENTRIES_AT + 8..ENTRIES_AT + 16].copy_from_slice(&[0u8; 8]);
        let r = find_record(&b, 3).unwrap();
        assert_eq!(r.segments(&b), 2);
        let (_, hi) = GpEntry { va: 0, dwords: 4 }.encode();
        b[ENTRIES_AT + 12..ENTRIES_AT + 16].copy_from_slice(&hi.to_le_bytes());
        assert_eq!(find_record(&b, 3), Err(RecordError::NullVa));
        // Zero entries, and more than a submission can take.
        let mut b = long_record(1);
        b[0x18..0x1c].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(find_record(&b, 0), Err(RecordError::Count));
        let big = (MAX_RECORD_ENTRIES + 1) as u32;
        b[0x18..0x1c].copy_from_slice(&big.to_le_bytes());
        assert_eq!(find_record(&b, big), Err(RecordError::Count));
        // A valid record after a broken header is still found.
        let mut b = std::vec![0u8; 4512];
        let good = long_record(2);
        b[..0x10].copy_from_slice(&good[..0x10]);
        b[0x18] = 9;
        b[0x400..0x400 + 0x354 + 16].copy_from_slice(&good[..0x354 + 16]);
        let r = find_record(&b, 2).unwrap();
        assert_eq!(r.entries_at, 0x400 + ENTRIES_AT);
        assert_eq!(RecordError::NullVa.code(), 5);
        // Arbitrary bytes never panic.
        for len in [0usize, 4, 0x35c, 0x360, 4512] {
            for fill in [0u8, 0xff, 0x41] {
                let _ = find_record(&std::vec![fill; len], 1);
            }
        }
    }
}

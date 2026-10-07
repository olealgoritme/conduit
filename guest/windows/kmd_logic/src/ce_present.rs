//! The windowed Present copy on the KMD's own RM copy-engine channel (`RmCopyEngine`): the pure
//! half. Design: `docs/rm-copy-engine-present.md` (sections 2 to 4 the mechanism, 10 the producer
//! record, 11 the KMD integration). Nothing here does I/O; the I/O half (the channel in the KMD's
//! RM client, the Present arm) is milestones M3b and M3c.
//!
//! What is here:
//!
//! * [`Gen`], [`Push`] and the emitters: the per-Present push buffer. Host-class semaphore ACQUIRE
//!   of the producer's value, the copy engine's copy (pitch-linear or block-linear source,
//!   pitch-linear destination), host-class RELEASE of the KMD's completion value, optional
//!   `NON_STALL_INTERRUPT`. The pitch-linear words are the ones `crm_ce_copy_smoke` PASSed with on
//!   a GB202 (tests `*_reproduces_the_tool_*`); the block-linear words are host-tested only.
//! * [`gp_entry`], [`Ring`], [`Token`], the USERD and doorbell offsets: submission.
//! * [`source_plan`]: is the producer's image a source this route copies (reusing the foreign
//!   layout rules of [`crate::foreign_resource::Layout`], the KMD's one modifier decoder).
//! * [`Route`], [`decide`], [`Retire`]: the per-destination state machine, the strike rule, the
//!   poison rule, and when the Present's DMA fence may retire.
//! * [`COUNTERS`], [`KNOB`]: names (`Ce*`, at most 14 characters).
//!
//! # Encodings, with the headers they come from
//!
//! | what | value | headers that agree | UNVERIFIED |
//! |---|---|---|---|
//! | method header | `1 << 29 \| count << 16 \| subc << 13 \| method >> 2` (`DMA_SEC_OP_INC_METHOD`) | `clc56f.h` (Mesa and 610.57.04) | |
//! | host `SET_OBJECT`, `NON_STALL_INTERRUPT`, `SEM_ADDR_LO..SEM_EXECUTE` | 0x0, 0x20, 0x5c..0x6c | `clc56f.h` both; `clca6f.h` both for 0x0 and 0x5c..0x6c | `NON_STALL_INTERRUPT` is not listed in `clca6f.h`; the tool's GB202 PASS used it |
//! | `SEM_EXECUTE` `OPERATION` 2:0, `ACQUIRE_SWITCH_TSG` 12, `RELEASE_WFI` 20, `PAYLOAD_SIZE` 24 | | `clc56f.h` both, `clca6f.h` both (2:0, 20, 24) | |
//! | `SEM_EXECUTE.RELEASE_TIMESTAMP` 25 | | `clc56f.h` both | not in `clca6f.h`: unverified on 0xca6f, never used by the tool |
//! | CE `SET_SEMAPHORE_A/B/PAYLOAD` 0x240.., `LAUNCH_DMA` 0x300, `OFFSET_IN_UPPER..LINE_COUNT` 0x400..0x41c | | `clc7b5.h` (Mesa and 610.57.04), Mesa `clcab5.h` | 610.57.04 `clcab5.h` carries only `LAUNCH_DMA` |
//! | `LAUNCH_DMA` `DATA_TRANSFER_TYPE` 1:0, `FLUSH_ENABLE` 2, `SEMAPHORE_TYPE` 4:3, `SRC/DST_MEMORY_LAYOUT` 7/8 (`BLOCKLINEAR` 0, `PITCH` 1), `MULTI_LINE_ENABLE` 9 | | the same three | |
//! | CE `SET_SRC_BLOCK_SIZE` 0x728 (`WIDTH` 3:0, `HEIGHT` 7:4, `DEPTH` 11:8, `GOB_HEIGHT` 15:12 = `FERMI_8` 1), `SET_SRC_WIDTH/HEIGHT/DEPTH/LAYER` 0x72c..0x738, `SRC_ORIGIN_X/Y` 0x744/0x748 | | `clc7b5.h` both, Mesa `clcab5.h` | 0xcab5 against 610.57.04 (its `clcab5.h` omits them) |
//! | `SET_SRC_BLOCK_SIZE.KIND_BPP` 17:16 (`BL_32` 0, `BL_8` 1, `BL_16` 2) | | Mesa `clcab5.h` only | 610.57.04; 0xc7b5 has no such field |
//! | GPFIFO entry `GET` 31:2, `GET_HI` 7:0, `LENGTH` 30:10 | | `clc56f.h` both, `clca6f.h` both | |
//! | USERD `GP_GET` 0x88, `GP_PUT` 0x8c; doorbell `NOTIFY_CHANNEL_PENDING` 0x90 | | nvk-rm patch 0002, `clc361.h` | |
//! | work-submit token: runlist 22:16, channel id 11:0 | | 610.57.04 `dev_vm.h` (GB202), `dev_ctrl.h` (GA100) | |
//!
//! The block-linear source sequence is the one NVK's `nvk_cmd_copy.c` (`nouveau_copy_rect`)
//! emits: `SET_SRC_BLOCK_SIZE` with `width = 0` (one GOB), `height = h`, `depth = 0`,
//! `gob_height = FERMI_8` and on 0xcab5 `kind_bpp`; `SET_SRC_WIDTH = pitch` in bytes (the copy
//! hardware has no tile width), `SET_SRC_HEIGHT = image height`, `DEPTH = 1`, `LAYER = 0`;
//! `SRC_ORIGIN_X` in bytes, `SRC_ORIGIN_Y` in rows; `PITCH_IN = pitch`; `LINE_LENGTH_IN` the copied
//! row bytes and `LINE_COUNT` the copied rows; `LAUNCH_DMA.SRC_MEMORY_LAYOUT = BLOCKLINEAR`.
//!
//! **Block-linear is not claimed to work.** Beyond the words, the source must be mapped in the
//! KMD's VA space with the page kind its modifier names ([`SourcePlan::page_kind`], 0x06): a
//! mapping of another kind reads scrambled pixels with no error (`rm-copy-engine-present.md` 10.3,
//! M1b and M3c).

use crate::foreign_resource::{Layout, MOD_LINEAR};

// ── classes ──────────────────────────────────────────────────────────────────────────────────

/// The GPU generation's class set (`crm_ce_copy_smoke --gen`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gen {
    /// GB20x: `BLACKWELL_CHANNEL_GPFIFO_B`, `BLACKWELL_DMA_COPY_B`, `BLACKWELL_USERMODE_A`.
    Gb202,
    /// AD10x: `AMPERE_CHANNEL_GPFIFO_A`, `AMPERE_DMA_COPY_B`, `AMPERE_USERMODE_A`.
    Ada,
}

/// The three classes a copy-engine channel needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classes {
    pub gpfifo: u32,
    pub copy: u32,
    pub usermode: u32,
}

impl Gen {
    pub const fn classes(self) -> Classes {
        match self {
            Gen::Gb202 => Classes { gpfifo: 0xca6f, copy: 0xcab5, usermode: 0xc761 },
            Gen::Ada => Classes { gpfifo: 0xc56f, copy: 0xc7b5, usermode: 0xc561 },
        }
    }

    /// The generation of a channel/copy class pair, `None` for any other pair.
    pub const fn from_classes(gpfifo: u32, copy: u32) -> Option<Gen> {
        match (gpfifo, copy) {
            (0xca6f, 0xcab5) => Some(Gen::Gb202),
            (0xc56f, 0xc7b5) => Some(Gen::Ada),
            _ => None,
        }
    }

    /// `SET_SRC_BLOCK_SIZE.KIND_BPP` exists (0xcab5 only).
    pub const fn has_kind_bpp(self) -> bool {
        matches!(self, Gen::Gb202)
    }

    /// The GB20x block-linear modifier families are this generation's GOB layout.
    pub const fn gb20x_modifiers(self) -> bool {
        matches!(self, Gen::Gb202)
    }
}

// ── methods ──────────────────────────────────────────────────────────────────────────────────

/// Subchannel of host methods (any; the host class ignores it, nvk-rm patch 0006).
pub const SUBC_HOST: u32 = 0;
/// Subchannel the copy object is bound to (NVK's copy subchannel).
pub const SUBC_CE: u32 = 4;

pub const HOST_SET_OBJECT: u32 = 0x0000;
pub const HOST_NON_STALL_INTERRUPT: u32 = 0x0020;
pub const HOST_SEM_ADDR_LO: u32 = 0x005c;

pub const SEM_OPERATION_RELEASE: u32 = 1;
pub const SEM_OPERATION_ACQ_STRICT_GEQ: u32 = 2;
pub const SEM_ACQUIRE_SWITCH_TSG_EN: u32 = 1 << 12;
pub const SEM_RELEASE_WFI_EN: u32 = 1 << 20;
pub const SEM_PAYLOAD_SIZE_64BIT: u32 = 1 << 24;
/// `clc56f.h`; not in `clca6f.h` (UNVERIFIED on 0xca6f).
pub const SEM_RELEASE_TIMESTAMP_EN: u32 = 1 << 25;

pub const CE_SET_SEMAPHORE_A: u32 = 0x0240;
pub const CE_LAUNCH_DMA: u32 = 0x0300;
pub const CE_OFFSET_IN_UPPER: u32 = 0x0400;
pub const CE_SET_SRC_BLOCK_SIZE: u32 = 0x0728;
pub const CE_SRC_ORIGIN_X: u32 = 0x0744;

pub const LAUNCH_TRANSFER_NONE: u32 = 0;
pub const LAUNCH_TRANSFER_NON_PIPELINED: u32 = 2;
pub const LAUNCH_FLUSH_ENABLE: u32 = 1 << 2;
pub const LAUNCH_SEMAPHORE_WITH_TIMESTAMP: u32 = 2 << 3;
/// `SRC_MEMORY_LAYOUT_PITCH`; 0 is `BLOCKLINEAR`.
pub const LAUNCH_SRC_PITCH: u32 = 1 << 7;
/// `DST_MEMORY_LAYOUT_PITCH`; 0 is `BLOCKLINEAR`.
pub const LAUNCH_DST_PITCH: u32 = 1 << 8;
pub const LAUNCH_MULTI_LINE: u32 = 1 << 9;

/// `SET_SRC_BLOCK_SIZE.GOB_HEIGHT_FERMI_8` at 15:12.
pub const BLOCK_SIZE_GOB_HEIGHT_FERMI_8: u32 = 1 << 12;

/// Every GPU VA this route writes is below 2^40: `GP_ENTRY1_GET_HI` is 8 bits, and below it the
/// class-specific widths of the upper-address fields (0xc7b5 17 bits, 0xcab5 25 bits, `clc56f.h`
/// `SEM_ADDR_HI` 8 bits) all agree, so the words are the same on both generations.
pub const MAX_VA: u64 = 1 << 40;
/// Most dwords one method header may announce (`DMA_INCR_COUNT` 28:16).
pub const MAX_METHOD_COUNT: u32 = 0x1fff;

/// An incrementing method header.
pub const fn method(subc: u32, mthd: u32, count: u32) -> u32 {
    (1 << 29) | (count << 16) | (subc << 13) | (mthd >> 2)
}

/// Why a push could not be built. Nothing is half-written that matters: the caller drops the
/// slot and takes the Venus copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The slot is too small for the words.
    Full,
    /// A VA at or above [`MAX_VA`].
    Va,
    /// A copy shape the route does not take (zero lines, a row past the pitch, a block height over
    /// 5, a block-linear pitch that is not whole GOBs, an origin past the image, an element size
    /// the generation has no block kind for).
    Shape,
}

/// A push buffer being written into one slot.
pub struct Push<'a> {
    buf: &'a mut [u32],
    len: usize,
}

impl<'a> Push<'a> {
    pub fn new(buf: &'a mut [u32]) -> Self {
        Self { buf, len: 0 }
    }

    /// The dwords written.
    pub fn words(&self) -> &[u32] {
        &self.buf[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One method header and its data, all or nothing.
    pub fn method(&mut self, subc: u32, mthd: u32, data: &[u32]) -> Result<(), PushError> {
        if self.len + 1 + data.len() > self.buf.len() {
            return Err(PushError::Full);
        }
        self.buf[self.len] = method(subc, mthd, data.len() as u32);
        self.buf[self.len + 1..self.len + 1 + data.len()].copy_from_slice(data);
        self.len += 1 + data.len();
        Ok(())
    }
}

fn va_ok(va: u64) -> Result<(), PushError> {
    if va < MAX_VA {
        Ok(())
    } else {
        Err(PushError::Va)
    }
}

const fn hi(va: u64) -> u32 {
    (va >> 32) as u32
}

const fn lo(va: u64) -> u32 {
    va as u32
}

/// `SET_OBJECT` of the copy class on [`SUBC_CE`] (once, as the channel's first push).
pub fn set_object(p: &mut Push<'_>, gen: Gen) -> Result<(), PushError> {
    p.method(SUBC_CE, HOST_SET_OBJECT, &[gen.classes().copy])
}

/// A host semaphore operation on the 64-bit value at `va`.
pub fn host_semaphore(p: &mut Push<'_>, va: u64, value: u64, execute: u32) -> Result<(), PushError> {
    va_ok(va)?;
    p.method(
        SUBC_HOST,
        HOST_SEM_ADDR_LO,
        &[lo(va), hi(va), lo(value), hi(value), execute],
    )
}

/// The producer's semaphore: the copy waits until `*va >= value` (unsigned 64-bit, not circular).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acquire {
    pub va: u64,
    pub value: u64,
}

/// `ACQ_STRICT_GEQ | ACQUIRE_SWITCH_TSG | PAYLOAD_SIZE_64BIT`: the runlist schedules other work
/// while the acquire is pending.
pub const ACQUIRE_EXECUTE: u32 =
    SEM_OPERATION_ACQ_STRICT_GEQ | SEM_ACQUIRE_SWITCH_TSG_EN | SEM_PAYLOAD_SIZE_64BIT;

pub fn acquire(p: &mut Push<'_>, a: Acquire) -> Result<(), PushError> {
    host_semaphore(p, a.va, a.value, ACQUIRE_EXECUTE)
}

/// The KMD's completion value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Release {
    pub va: u64,
    pub value: u64,
    /// Wait for the engine to be idle first (always, after a copy).
    pub wfi: bool,
    /// A 16-byte release with the GPU time in the second half (UNVERIFIED on 0xca6f).
    pub timestamp: bool,
    /// Follow with `NON_STALL_INTERRUPT` (the completion event, option B of section 4.3).
    pub interrupt: bool,
}

pub const fn release_execute(r: &Release) -> u32 {
    SEM_OPERATION_RELEASE
        | SEM_PAYLOAD_SIZE_64BIT
        | if r.wfi { SEM_RELEASE_WFI_EN } else { 0 }
        | if r.timestamp { SEM_RELEASE_TIMESTAMP_EN } else { 0 }
}

pub fn release(p: &mut Push<'_>, r: Release) -> Result<(), PushError> {
    host_semaphore(p, r.va, r.value, release_execute(&r))?;
    if r.interrupt {
        p.method(SUBC_HOST, HOST_NON_STALL_INTERRUPT, &[0])?;
    }
    Ok(())
}

/// A copy-engine semaphore release with timestamp (the tool's measurement stamps): 16 bytes,
/// the 32-bit payload then the GPU time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CeStamp {
    pub va: u64,
    pub payload: u32,
}

fn ce_semaphore_address(p: &mut Push<'_>, s: CeStamp) -> Result<(), PushError> {
    va_ok(s.va)?;
    p.method(SUBC_CE, CE_SET_SEMAPHORE_A, &[hi(s.va), lo(s.va), s.payload])
}

/// A semaphore-only launch: the CE writes `payload` and the GPU time when it reaches it (the tool's
/// T0, "the CE starts").
pub fn ce_stamp(p: &mut Push<'_>, s: CeStamp) -> Result<(), PushError> {
    ce_semaphore_address(p, s)?;
    p.method(
        SUBC_CE,
        CE_LAUNCH_DMA,
        &[LAUNCH_TRANSFER_NONE | LAUNCH_FLUSH_ENABLE | LAUNCH_SEMAPHORE_WITH_TIMESTAMP],
    )
}

/// How the copy reads its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceLayout {
    /// Pitch-linear rows `pitch` bytes apart.
    Pitch,
    /// 2D block-linear: GOBs of 64 bytes x 8 rows, blocks of `1 << block_height_log2` GOBs.
    BlockLinear {
        /// `h` of the modifier, 0..=5.
        block_height_log2: u32,
        /// Bytes of one element (4 for the 32 bpp formats); picks `KIND_BPP` on 0xcab5.
        element_bytes: u32,
        /// Rows of the whole image (`SET_SRC_HEIGHT`).
        image_height: u32,
        /// First copied column, in bytes (`SRC_ORIGIN_X`).
        origin_x_bytes: u32,
        /// First copied row (`SRC_ORIGIN_Y`).
        origin_y: u32,
    },
}

/// One rectangle into a pitch-linear destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyRect {
    /// Pitch-linear: the first copied byte. Block-linear: the image's base (the origin selects
    /// the rectangle).
    pub src_va: u64,
    /// The first destination byte.
    pub dst_va: u64,
    /// Source row pitch in bytes (`PITCH_IN`; for block-linear also `SET_SRC_WIDTH`).
    pub src_pitch: u32,
    /// Destination row pitch in bytes (`PITCH_OUT`).
    pub dst_pitch: u32,
    /// Bytes per copied row (`LINE_LENGTH_IN`).
    pub line_bytes: u32,
    /// Copied rows (`LINE_COUNT`).
    pub lines: u32,
    pub layout: SourceLayout,
    /// The copy's own semaphore release with timestamp (the tool's T1); `None` in production.
    pub stamp: Option<CeStamp>,
}

const fn kind_bpp(element_bytes: u32) -> Option<u32> {
    match element_bytes {
        4 => Some(0),
        1 => Some(1),
        2 => Some(2),
        _ => None,
    }
}

/// The `SET_SRC_BLOCK_SIZE` word.
pub const fn src_block_size(gen: Gen, block_height_log2: u32, element_bytes: u32) -> Option<u32> {
    if block_height_log2 > 5 {
        return None;
    }
    let base = (block_height_log2 << 4) | BLOCK_SIZE_GOB_HEIGHT_FERMI_8;
    if gen.has_kind_bpp() {
        match kind_bpp(element_bytes) {
            Some(k) => Some(base | (k << 16)),
            None => None,
        }
    } else if element_bytes == 4 {
        Some(base)
    } else {
        None
    }
}

fn check_copy(gen: Gen, c: &CopyRect) -> Result<(), PushError> {
    va_ok(c.src_va)?;
    va_ok(c.dst_va)?;
    if c.lines == 0 || c.line_bytes == 0 || c.line_bytes > c.dst_pitch {
        return Err(PushError::Shape);
    }
    match c.layout {
        SourceLayout::Pitch => {
            if c.line_bytes > c.src_pitch {
                return Err(PushError::Shape);
            }
        }
        SourceLayout::BlockLinear {
            block_height_log2,
            element_bytes,
            image_height,
            origin_x_bytes,
            origin_y,
        } => {
            if src_block_size(gen, block_height_log2, element_bytes).is_none()
                || c.src_pitch % 64 != 0
                || origin_x_bytes as u64 + c.line_bytes as u64 > c.src_pitch as u64
                || origin_y as u64 + c.lines as u64 > image_height as u64
            {
                return Err(PushError::Shape);
            }
        }
    }
    Ok(())
}

/// The copy: offsets, pitches and extent, the block-linear source state when it applies, and
/// `LAUNCH_DMA` (non-pipelined, flushed, virtual addresses, multi-line).
pub fn copy(p: &mut Push<'_>, gen: Gen, c: &CopyRect) -> Result<(), PushError> {
    check_copy(gen, c)?;
    if let Some(s) = c.stamp {
        ce_semaphore_address(p, s)?;
    }
    p.method(
        SUBC_CE,
        CE_OFFSET_IN_UPPER,
        &[
            hi(c.src_va),
            lo(c.src_va),
            hi(c.dst_va),
            lo(c.dst_va),
            c.src_pitch,
            c.dst_pitch,
            c.line_bytes,
            c.lines,
        ],
    )?;
    let src_layout = match c.layout {
        SourceLayout::Pitch => LAUNCH_SRC_PITCH,
        SourceLayout::BlockLinear {
            block_height_log2,
            element_bytes,
            image_height,
            origin_x_bytes,
            origin_y,
        } => {
            let block = src_block_size(gen, block_height_log2, element_bytes).ok_or(PushError::Shape)?;
            p.method(
                SUBC_CE,
                CE_SET_SRC_BLOCK_SIZE,
                &[block, c.src_pitch, image_height, 1, 0],
            )?;
            p.method(SUBC_CE, CE_SRC_ORIGIN_X, &[origin_x_bytes, origin_y])?;
            0
        }
    };
    let semaphore = if c.stamp.is_some() { LAUNCH_SEMAPHORE_WITH_TIMESTAMP } else { 0 };
    p.method(
        SUBC_CE,
        CE_LAUNCH_DMA,
        &[LAUNCH_TRANSFER_NON_PIPELINED
            | LAUNCH_FLUSH_ENABLE
            | semaphore
            | src_layout
            | LAUNCH_DST_PITCH
            | LAUNCH_MULTI_LINE],
    )
}

/// The production push of one Present: acquire the producer's value, copy, release the KMD's
/// completion value with WFI (the WFI waits for the copy engine), `NON_STALL_INTERRUPT` when
/// the completion is evented.
pub fn present_push(
    p: &mut Push<'_>,
    gen: Gen,
    producer: Acquire,
    c: &CopyRect,
    done: Release,
) -> Result<(), PushError> {
    if !done.wfi {
        // Without the WFI the release may land before the copy finished.
        return Err(PushError::Shape);
    }
    acquire(p, producer)?;
    copy(p, gen, c)?;
    release(p, done)
}

/// The dwords of the largest push [`present_push`] writes (block-linear, interrupt): 6 + 9 + 6 + 3
/// + 2 + 6 + 2. A 512-byte slot (128 dwords) holds it with room.
pub const PRESENT_PUSH_MAX_DWORDS: usize = 34;

// ── submission ───────────────────────────────────────────────────────────────────────────────

/// Bytes of one GPFIFO entry.
pub const GP_ENTRY_BYTES: u64 = 8;
/// Most dwords one entry may name (`GP_ENTRY1_LENGTH` 30:10).
pub const GP_MAX_DWORDS: u32 = 0x1f_ffff;
/// USERD (`Nvc36fControl`..`Nvca6fControl`): `GP_GET` (never written back under GSP on GB202,
/// nvk-rm patch 0009) and `GP_PUT`.
pub const USERD_GP_GET: u32 = 0x88;
pub const USERD_GP_PUT: u32 = 0x8c;
/// Usermode doorbell: the work-submit token goes here (`clc361.h` `NOTIFY_CHANNEL_PENDING`).
pub const DOORBELL_OFFSET: u32 = 0x90;
/// Bytes of the usermode object's mapping.
pub const USERMODE_BYTES: u64 = 0x1_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpError {
    /// The push VA is not 4-aligned or not below 2^40.
    Va,
    /// Zero dwords, or more than one entry can name.
    Length,
}

/// The 64-bit GPFIFO entry for a push of `dwords` at `va`: `GET` 31:2 in the low word, `GET_HI`
/// 7:0 and `LENGTH` 30:10 in the high word (the tool's `kick`).
pub const fn gp_entry(va: u64, dwords: u32) -> Result<u64, GpError> {
    if va % 4 != 0 || va >= MAX_VA {
        return Err(GpError::Va);
    }
    if dwords == 0 || dwords > GP_MAX_DWORDS {
        return Err(GpError::Length);
    }
    let high = ((va >> 32) as u32 & 0xff) | (dwords << 10);
    Ok(((high as u64) << 32) | (va as u32 & !3) as u64)
}

/// A work-submit token (`GET_WORK_SUBMIT_TOKEN`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token(pub u32);

impl Token {
    /// Runlist id, bits 22:16 (the tool prints `(tok >> 16) & 0x7f`).
    pub const fn runlist(self) -> u32 {
        (self.0 >> 16) & 0x7f
    }

    /// Channel id, bits 11:0.
    pub const fn channel(self) -> u32 {
        self.0 & 0xfff
    }
}

/// The ring of GPFIFO entries and push slots, one slot per entry. Progress is the completion
/// semaphore, since USERD `GP_GET` is not written back: every push ends with a release of its
/// sequence number, the channel executes in order, so `completed` is a watermark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ring {
    entries: u32,
    put: u32,
    /// The sequence of the last submission (the completion value its push releases).
    submitted: u64,
    /// The highest completion value seen.
    completed: u64,
}

/// One accepted submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// GPFIFO entry index, and the push slot index.
    pub index: u32,
    /// The completion value the push releases.
    pub value: u64,
    /// `GP_PUT` to store in USERD after the entry is written.
    pub put: u32,
}

impl Ring {
    /// `entries` in `2..=65536`. `first_value` is the last completion value already used by the
    /// channel (its setup push), so submissions continue above it.
    pub const fn new(entries: u32, first_value: u64) -> Option<Ring> {
        if entries < 2 || entries > 65536 {
            return None;
        }
        Some(Ring { entries, put: 0, submitted: first_value, completed: first_value })
    }

    pub const fn entries(&self) -> u32 {
        self.entries
    }

    pub const fn put(&self) -> u32 {
        self.put
    }

    pub const fn submitted(&self) -> u64 {
        self.submitted
    }

    pub const fn completed(&self) -> u64 {
        self.completed
    }

    pub const fn in_flight(&self) -> u64 {
        self.submitted - self.completed
    }

    /// One entry stays free, so `GP_PUT` never laps an entry the GPU has not consumed (and the
    /// slot it names is not rewritten while the GPU may read it).
    pub const fn is_full(&self) -> bool {
        self.in_flight() >= self.entries as u64 - 1
    }

    /// Take the next slot; `None` when full.
    pub fn submit(&mut self) -> Option<Slot> {
        if self.is_full() {
            return None;
        }
        let index = self.put;
        self.put = (self.put + 1) % self.entries;
        self.submitted += 1;
        Some(Slot { index, value: self.submitted, put: self.put })
    }

    /// The completion semaphore read `value`. A value that went backwards or past what was
    /// submitted is ignored (stale or foreign); returns how many submissions retired.
    pub fn observe(&mut self, value: u64) -> u64 {
        if value <= self.completed || value > self.submitted {
            return 0;
        }
        let n = value - self.completed;
        self.completed = value;
        n
    }

    /// GPU VA of slot `index`'s push buffer.
    pub const fn slot_va(push_base_va: u64, index: u32, slot_bytes: u32) -> u64 {
        push_base_va + index as u64 * slot_bytes as u64
    }

    /// Byte offset of entry `index` in the GPFIFO.
    pub const fn entry_offset(index: u32) -> u64 {
        index as u64 * GP_ENTRY_BYTES
    }
}

/// Every store of one kick, in order: the entry, a full barrier, `GP_PUT`, a full barrier, the
/// token to the doorbell (the tool's `kick`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kick {
    pub entry_offset: u64,
    pub entry: u64,
    pub userd_offset: u32,
    pub put: u32,
    pub doorbell_offset: u32,
    pub token: u32,
}

pub fn kick(slot: Slot, push_va: u64, dwords: u32, token: Token) -> Result<Kick, GpError> {
    Ok(Kick {
        entry_offset: Ring::entry_offset(slot.index),
        entry: gp_entry(push_va, dwords)?,
        userd_offset: USERD_GP_PUT,
        put: slot.put,
        doorbell_offset: DOORBELL_OFFSET,
        token: token.0,
    })
}

// ── the source ───────────────────────────────────────────────────────────────────────────────

/// The producer's image as the fence tail v3 record names it (`protocol::HeliosRmCopySource`),
/// after the protocol parser accepted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceDesc {
    pub offset: u64,
    pub size: u64,
    pub modifier: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub compressed: bool,
}

/// What [`source_plan`] decided about a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePlan {
    pub layout: SourceLayout,
    /// The page kind the KMD must map the source with: the modifier's `k` field for block-linear
    /// (0x06 for every family NVK emits), `None` for LINEAR (the default kind of the mapping).
    pub page_kind: Option<u32>,
    /// Bytes per copied row (`width * 4`).
    pub line_bytes: u32,
    /// Bytes from the start of the memory the copy reads for pitch-linear (offset of row 0), or
    /// the image base for block-linear.
    pub offset: u64,
}

/// The modifier's page-kind field, `k` at 19:12 of `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D`.
pub const fn modifier_page_kind(modifier: u64) -> u32 {
    ((modifier >> 12) & 0xff) as u32
}

/// Is the image a source this route copies, and how. The layout rules are the foreign layout's
/// ([`Layout::validate_for`], the KMD's one modifier decoder); on top: the four 32-bit RGB
/// formats only (what a Blt destination holds), not compressed, an offset that fits the layout
/// record, and block-linear only on a generation whose GOB layout the GB20x modifiers name.
pub fn source_plan(gen: Gen, s: &SourceDesc) -> Result<SourcePlan, Why> {
    if s.compressed || s.offset > u32::MAX as u64 {
        return Err(Why::SourceUnsupported);
    }
    let layout = Layout {
        width: s.width,
        height: s.height,
        stride: s.pitch,
        offset: s.offset as u32,
        fourcc: s.fourcc,
        modifier: s.modifier,
        plane1: None,
    };
    if layout.validate_for(s.size).is_err() {
        return Err(Why::SourceUnsupported);
    }
    if !layout.is_rgb32() {
        return Err(Why::SourceUnsupported);
    }
    let line_bytes = s.width * 4;
    if s.modifier == MOD_LINEAR {
        return Ok(SourcePlan {
            layout: SourceLayout::Pitch,
            page_kind: None,
            line_bytes,
            offset: s.offset,
        });
    }
    let Some(h) = layout.block_height_log2() else {
        return Err(Why::SourceUnsupported);
    };
    if !gen.gb20x_modifiers() || s.pitch % 64 != 0 {
        return Err(Why::SourceUnsupported);
    }
    Ok(SourcePlan {
        layout: SourceLayout::BlockLinear {
            block_height_log2: h,
            element_bytes: 4,
            image_height: s.height,
            origin_x_bytes: 0,
            origin_y: 0,
        },
        page_kind: Some(modifier_page_kind(s.modifier)),
        line_bytes,
        offset: s.offset,
    })
}

// ── decisions and the per-destination route ──────────────────────────────────────────────────

/// Why a Present takes the Venus copy (or a destination stops using this route). `code` is what
/// `CeWhy` holds, `bit` what `CeMask` collects. Codes 1 to 6 and 10 are decisions; 7 to 9 are
/// failures, each a strike ([`Why::strikes`]). New codes are appended, never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// `RmCopyEngine` is 0.
    FeatureOff = 1,
    /// The Present carries no fence tail v3 record (or the producer sent none).
    NoTailV3 = 2,
    /// A record that the KMD refused: malformed, a value that is not the fence's, or a client
    /// that is not the presenting process's.
    TailRefused = 3,
    /// The source's format, layout or compression is not one this route copies.
    SourceUnsupported = 4,
    /// The destination's pages are not all covered by leases (or not a KMD standard buffer).
    DestinationUncovered = 5,
    /// The KMD's copy channel does not exist, is being torn down, or its error notifier is set.
    ChannelDead = 6,
    /// A submission did not complete within [`TIMEOUT_AFTER_PRODUCER_MS`] of the producer's fence.
    Timeout = 7,
    /// An RM call of the route failed (dup, map, descriptor).
    RmError = 8,
    /// The channel failed while this destination's copy was in flight.
    ChannelFailed = 9,
    /// No free GPFIFO entry.
    RingFull = 10,
    /// Three strikes: disabled for this destination until it is destroyed.
    StruckOut = 11,
    /// A timed-out copy may still write the destination: no new copy until it completes.
    Poisoned = 12,
}

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The `CeMask` bit: `1 << (code - 1)`.
    pub const fn bit(self) -> u32 {
        1u32 << (self.code() - 1)
    }

    /// A failure of the route itself (a strike), not a decision.
    pub const fn strikes(self) -> bool {
        matches!(self, Why::Timeout | Why::RmError | Why::ChannelFailed)
    }
}

/// Strikes after which a destination never uses the route again.
pub const MAX_STRIKES: u8 = 3;
/// How long a copy may take after the producer's fence fired (policy). The copy of a 1600x900
/// frame is about 0.2 ms; anything near this bound is a hung or starved channel. The clock starts
/// at the fence, not the submission: a slow producer is not the route's failure.
pub const TIMEOUT_AFTER_PRODUCER_MS: u64 = 100;

/// The per-destination record, created lazily with the destination's other state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// The highest completion value of a copy into this destination.
    submitted: u64,
    /// When the producer of the OLDEST outstanding copy fired (ms), if it did.
    producer_fired_ms: Option<u64>,
    /// The completion value the poison waits for; 0 = not poisoned.
    poison: u64,
    strikes: u8,
    last_why: Option<Why>,
}

impl Default for Route {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`Route::poll`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    /// Nothing outstanding.
    Idle,
    /// Copies outstanding, within bounds.
    Pending,
    /// Every copy up to `completed` is done.
    Done,
    /// The oldest outstanding copy ran out of time: the destination is now poisoned and struck.
    TimedOut,
}

/// What happens to the destination's pages when it is destroyed or evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Teardown {
    /// No copy can still write them: free the descriptor, then unlock.
    Unlock,
    /// A copy may still write them: wait (bounded) for `value`, then unlock.
    WaitFor(u64),
    /// Poisoned and never completed: keep them pinned for good (counted `CeLeak`).
    Leak,
}

impl Route {
    pub const fn new() -> Self {
        Self { submitted: 0, producer_fired_ms: None, poison: 0, strikes: 0, last_why: None }
    }

    pub const fn strikes(&self) -> u8 {
        self.strikes
    }

    pub const fn last_why(&self) -> Option<Why> {
        self.last_why
    }

    pub const fn submitted(&self) -> u64 {
        self.submitted
    }

    pub const fn is_poisoned(&self) -> bool {
        self.poison != 0
    }

    pub const fn is_disabled(&self) -> bool {
        self.strikes >= MAX_STRIKES
    }

    /// May a new copy go to this destination at all.
    pub const fn admits(&self) -> Result<(), Why> {
        if self.is_disabled() {
            Err(Why::StruckOut)
        } else if self.is_poisoned() {
            Err(Why::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Copies into this destination still outstanding at `completed`.
    pub const fn outstanding(&self, completed: u64) -> bool {
        self.submitted > completed
    }

    /// A copy with completion `value` was submitted (values grow; the channel is in order).
    pub fn on_submit(&mut self, value: u64, completed: u64) {
        if !self.outstanding(completed) {
            self.producer_fired_ms = None;
        }
        if value > self.submitted {
            self.submitted = value;
        }
    }

    /// The producer's fence of an outstanding copy fired at `now_ms`. Only the first one counts:
    /// the clock measures the oldest outstanding copy.
    pub fn on_producer_fired(&mut self, now_ms: u64, completed: u64) {
        if self.outstanding(completed) && self.producer_fired_ms.is_none() {
            self.producer_fired_ms = Some(now_ms);
        }
    }

    /// A failure outside a submission (an RM call, the channel): a strike when it is one.
    pub fn on_failure(&mut self, why: Why) {
        self.last_why = Some(why);
        if why.strikes() {
            self.strikes = self.strikes.saturating_add(1);
        }
    }

    /// The channel failed: every outstanding copy into this destination may or may not have
    /// written; treat it as a poison (pages stay pinned until a late completion), and a strike.
    pub fn on_channel_failed(&mut self, completed: u64) {
        if self.outstanding(completed) {
            self.poison = self.submitted;
        }
        self.on_failure(Why::ChannelFailed);
    }

    /// Advance with the completion value `completed` at `now_ms`.
    pub fn poll(&mut self, completed: u64, now_ms: u64) -> Poll {
        if self.poison != 0 && completed >= self.poison {
            // The late completion arrived: nothing can write the pages any more.
            self.poison = 0;
        }
        if !self.outstanding(completed) {
            self.producer_fired_ms = None;
            return if self.submitted == 0 { Poll::Idle } else { Poll::Done };
        }
        match self.producer_fired_ms {
            Some(t) if self.poison == 0 && now_ms.saturating_sub(t) > TIMEOUT_AFTER_PRODUCER_MS => {
                self.poison = self.submitted;
                self.on_failure(Why::Timeout);
                Poll::TimedOut
            }
            _ => Poll::Pending,
        }
    }

    /// May the destination's pages be unlocked (eviction, destroy)? Only when no copy can still
    /// write them: nothing outstanding at `completed`.
    pub const fn may_unlock(&self, completed: u64) -> bool {
        !self.outstanding(completed)
    }

    /// The order of retirement (`rm-copy-engine-present.md` 3): stop new copies (the caller), wait
    /// for the last submitted value, free the mapping and the descriptor, unlock.
    pub const fn teardown(&self, completed: u64) -> Teardown {
        if !self.outstanding(completed) {
            Teardown::Unlock
        } else if self.is_poisoned() {
            Teardown::Leak
        } else {
            Teardown::WaitFor(self.submitted)
        }
    }
}

/// Everything [`decide`] reads, gathered by the Present arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// `RmCopyEngine` is 1.
    pub knob_on: bool,
    /// The KMD's copy channel exists and its error notifier is clear.
    pub channel_alive: bool,
    /// The record: `None` absent, `Some(Err(_))` refused (`TailRefused`), `Some(Ok(()))` accepted.
    pub tail: Option<Result<(), ()>>,
    /// [`source_plan`]'s verdict.
    pub source: Result<(), Why>,
    /// The destination's leases cover the surface.
    pub destination_covered: bool,
    /// The ring has a free entry.
    pub ring_room: bool,
}

/// The per-Present decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Submit the copy-engine push.
    CopyEngine,
    /// Take the Venus copy. `after`: copies into the same destination are still outstanding, so
    /// the Venus copy must wait until the completion value reaches it (a Venus frame must never
    /// be overwritten by an older copy-engine frame). A poisoned destination has no such bound
    /// (`after = None` and the poison stands): its late copy can still land on a Venus frame, a
    /// one-frame glitch, never a memory fault, because the pages stay pinned.
    Venus { why: Why, after: Option<u64> },
}

/// Order: knob, channel, record, source, the destination's own state, coverage, ring room.
pub fn decide(f: &Facts, route: &Route, completed: u64) -> Decision {
    let after = if route.outstanding(completed) && !route.is_poisoned() {
        Some(route.submitted())
    } else {
        None
    };
    let venus = |why| Decision::Venus { why, after };
    if !f.knob_on {
        return venus(Why::FeatureOff);
    }
    if !f.channel_alive {
        return venus(Why::ChannelDead);
    }
    match f.tail {
        None => return venus(Why::NoTailV3),
        Some(Err(())) => return venus(Why::TailRefused),
        Some(Ok(())) => {}
    }
    if let Err(why) = f.source {
        return venus(why);
    }
    if let Err(why) = route.admits() {
        return venus(why);
    }
    if !f.destination_covered {
        return venus(Why::DestinationUncovered);
    }
    if !f.ring_room {
        return venus(Why::RingFull);
    }
    Decision::CopyEngine
}

/// When the DMA fence of a copy-engine Present may retire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retire {
    /// Not yet: the completion value has not reached the Present's.
    Wait,
    /// The GPU's completion semaphore reached the Present's value: the copy is in the pages.
    Retire,
    /// The copy timed out (the destination is poisoned): the Present is discharged without its
    /// copy, counted, like the `WddmHeadMs` rebase.
    Discharge,
}

/// The ONLY rule that retires a copy-engine Present: `completed >= value`, the value the GPU's
/// release wrote after the copy's WFI. Neither the producer's fence nor the doorbell nor a CPU
/// observation of anything else retires it. `timed_out` is the destination's poll having
/// returned [`Poll::TimedOut`] for this or an older copy.
pub const fn retire(value: u64, completed: u64, timed_out: bool) -> Retire {
    if completed >= value {
        Retire::Retire
    } else if timed_out {
        Retire::Discharge
    } else {
        Retire::Wait
    }
}

// ── names ────────────────────────────────────────────────────────────────────────────────────

/// The knob (`HKR\Parameters`, `diag.rs` `KnobName`): 0 off (default), 1 on.
pub const KNOB: &str = "RmCopyEngine";
pub const KNOB_DEFAULT: u32 = 0;

/// The counters the route will write (M3b/M3c, `kmd_render`). At most 14 characters, prefix `Ce`,
/// unique across `kmd_render` and `kmd_logic`. No I/O file writes them yet, so the tests below
/// check that NONE of them is spelled in `kmd_render` today; the M3c commit that adds the writer
/// replaces that test with the exact-list check `guest_blob.rs` has.
pub const COUNTERS: &[&str] = &[
    // The knob in force; the channel's state (0 none, 1 alive, 2 dead) and its runlist.
    "CeKnob",
    "CeChan",
    "CeRunlist",
    // Records seen, accepted, refused (and the last refusal's parser code).
    "CeTail",
    "CeTailOk",
    "CeTailBad",
    "CeTailWhy",
    // Presents copied by the copy engine, completed, retired; Venus fallbacks with the last
    // reason and every reason seen; fallbacks that had to wait for an outstanding copy.
    "CeHit",
    "CeDone",
    "CeFallback",
    "CeWhy",
    "CeMask",
    "CeAfter",
    // Failures: strikes, destinations disabled, timeouts, poisoned destinations, pages leaked at
    // destroy, RM errors, channel failures, full ring.
    "CeStrike",
    "CeDisabled",
    "CeTimeout",
    "CePoison",
    "CeLeak",
    "CeRmErr",
    "CeChanFail",
    "CeRingFull",
    // Latency: doorbell to completion seen (sum, max, us), producer fired to completion seen.
    "CeLatUs",
    "CeLatMax",
    "CeAcqUs",
    // Source and destination descriptors live, and the source layout of the last copy (0 pitch,
    // 1 block-linear).
    "CeSrcLive",
    "CeDstLive",
    "CeSrcBl",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    // ── the tool's words (crm_ce_copy_smoke.c, default parameters) ───────────────────────────
    //
    // How the expected words below were derived, by reading the tool's code:
    // * `mthd(subc, m, count)` = `(1 << 29) | (count << 16) | (subc << 13) | (m >> 2)`;
    //   SUBC_HOST 0, SUBC_CE 4.
    // * `emit_host_sem`: header `mthd(0, 0x5c, 5)` = 0x20050017, then VA lo, VA hi, value lo,
    //   value hi, SEM_EXECUTE. Acquire `ACQ_STRICT_GEQ | SWITCH_TSG | 64BIT` = 0x01001002,
    //   release WFI 64-bit = 0x01100001.
    // * `emit_ce_sem_addr`: `mthd(4, 0x240, 3)` = 0x20038090, VA hi (masked 24:0), VA lo, payload.
    // * `emit_ce_copy`: sem addr T0, `mthd(4, 0x300, 1)` = 0x200180c0 + `NONE | FLUSH |
    //   WITH_TIMESTAMP` = 0x14; sem addr T1; `mthd(4, 0x400, 8)` = 0x20088100 + src hi, src lo,
    //   dst hi, dst lo, pitch, pitch, pitch, lines; `mthd(4, 0x300, 1)` + `NON_PIPELINED | FLUSH |
    //   WITH_TIMESTAMP | SRC_PITCH | DST_PITCH | MULTI_LINE` = 0x396.
    // * `emit_release(.., wfi = 1, irq = 1)`: the release, then `mthd(0, 0x20, 1)` = 0x20010008, 0.
    // * The first push: `mthd(4, 0, 1)` = 0x20018000, the class, then a WFI release of seq 1.
    // * VAs: `gpu_map` maps at `next_va` from 0x20_0000_0000 and advances it by the mapping
    //   rounded up to 2 MiB plus 2 MiB. In the tool's order (ring 128 KiB, producer timeline 4 KiB,
    //   source 6 MiB, destination 5767168 B, stamps 64 KiB, completion 4 KiB) the defaults are
    //   ring 0x20_0000_0000, timeline 0x20_0040_0000, source 0x20_0080_0000, destination
    //   0x20_0100_0000, stamps 0x20_0180_0000, completion 0x20_01c0_0000 (when RM takes the fixed
    //   VAs, as it did in the PASS run). `tool_vas` recomputes them the same way.
    // * Round 1 ("ready"): seq 2 (seq 1 is the first push), payload 1, push slot 1:
    //   push VA = ring + 4096 + 1 * 512.

    struct ToolVas {
        ring: u64,
        timeline: u64,
        source: u64,
        destination: u64,
        stamps: u64,
        completion: u64,
    }

    fn tool_vas() -> ToolVas {
        let mib = 1024 * 1024u64;
        let align = |v: u64, a: u64| v.div_ceil(a) * a;
        let copy_bytes = 1600 * 4 * 900u64;
        let mut next = 0x20_0000_0000u64;
        let mut map = |size: u64| {
            let va = next;
            next += align(size, 2 * mib) + 2 * mib;
            va
        };
        ToolVas {
            ring: map(128 * 1024),
            timeline: map(4096),
            source: map(align(copy_bytes, 2 * mib)),
            destination: map(align(copy_bytes, 64 * 1024)),
            stamps: map(65536),
            completion: map(4096),
        }
    }

    #[test]
    fn the_tool_vas_are_the_documented_ones() {
        let v = tool_vas();
        assert_eq!(v.ring, 0x20_0000_0000);
        assert_eq!(v.timeline, 0x20_0040_0000);
        assert_eq!(v.source, 0x20_0080_0000);
        assert_eq!(v.destination, 0x20_0100_0000);
        assert_eq!(v.stamps, 0x20_0180_0000);
        assert_eq!(v.completion, 0x20_01c0_0000);
    }

    fn build(f: impl FnOnce(&mut Push<'_>) -> Result<(), PushError>) -> Vec<u32> {
        let mut buf = [0u32; 128];
        let mut p = Push::new(&mut buf);
        f(&mut p).unwrap();
        p.words().to_vec()
    }

    #[test]
    fn the_first_push_reproduces_the_tool_words() {
        let v = tool_vas();
        for gen in [Gen::Gb202, Gen::Ada] {
            let words = build(|p| {
                set_object(p, gen)?;
                release(
                    p,
                    Release { va: v.stamps + 32, value: 1, wfi: true, timestamp: false, interrupt: false },
                )
            });
            let class = if gen == Gen::Gb202 { 0xcab5 } else { 0xc7b5 };
            assert_eq!(
                words,
                [
                    0x2001_8000,
                    class,
                    0x2005_0017,
                    0x0180_0020,
                    0x0000_0020,
                    1,
                    0,
                    0x0110_0001,
                ]
            );
            // Slot 0 at ring + 4096: GET = 0x1000, GET_HI = 0x20, LENGTH = 8.
            assert_eq!(gp_entry(v.ring + 4096, words.len() as u32), Ok(0x0000_2020_0000_1000));
        }
    }

    #[test]
    fn the_ready_round_reproduces_the_tool_words() {
        let v = tool_vas();
        let words = build(|p| {
            acquire(p, Acquire { va: v.timeline, value: 2 })?;
            ce_stamp(p, CeStamp { va: v.stamps, payload: 1 })?;
            copy(
                p,
                Gen::Gb202,
                &CopyRect {
                    src_va: v.source,
                    dst_va: v.destination,
                    src_pitch: 6400,
                    dst_pitch: 6400,
                    line_bytes: 6400,
                    lines: 900,
                    layout: SourceLayout::Pitch,
                    stamp: Some(CeStamp { va: v.stamps + 16, payload: 1 }),
                },
            )?;
            release(
                p,
                Release { va: v.completion, value: 2, wfi: true, timestamp: false, interrupt: true },
            )
        });
        let expected: [u32; 35] = [
            // acquire the producer timeline >= 2
            0x2005_0017, 0x0040_0000, 0x0000_0020, 2, 0, 0x0100_1002,
            // T0: CE semaphore address, payload 1; semaphore-only launch with timestamp
            0x2003_8090, 0x0000_0020, 0x0180_0000, 1,
            0x2001_80c0, 0x0000_0014,
            // T1: the copy's own semaphore address
            0x2003_8090, 0x0000_0020, 0x0180_0010, 1,
            // OFFSET_IN_UPPER..LINE_COUNT
            0x2008_8100, 0x0000_0020, 0x0080_0000, 0x0000_0020, 0x0100_0000, 6400, 6400, 6400, 900,
            // LAUNCH_DMA: non-pipelined, flush, timestamp semaphore, pitch/pitch, multi-line
            0x2001_80c0, 0x0000_0396,
            // release the completion value 2 with WFI, 64-bit
            0x2005_0017, 0x01c0_0000, 0x0000_0020, 2, 0, 0x0110_0001,
            // NON_STALL_INTERRUPT
            0x2001_0008, 0,
        ];
        assert_eq!(words, expected);
        // Slot 1 at ring + 4096 + 512, 35 dwords.
        assert_eq!(
            gp_entry(v.ring + 4096 + 512, 35),
            Ok((((35u64 << 10) | 0x20) << 32) | 0x1200)
        );
    }

    #[test]
    fn the_probe_push_reproduces_the_tool_words() {
        // `emit_release(p, 0, stamp + ST_PROBE, v, 0, 0)`: no WFI, no interrupt.
        let v = tool_vas();
        let words = build(|p| {
            release(
                p,
                Release { va: v.stamps + 32, value: 4, wfi: false, timestamp: false, interrupt: false },
            )
        });
        assert_eq!(words, [0x2005_0017, 0x0180_0020, 0x20, 4, 0, 0x0100_0001]);
    }

    #[test]
    fn production_pitch_push_has_no_stamps() {
        let v = tool_vas();
        let mut buf = [0u32; PRESENT_PUSH_MAX_DWORDS];
        let mut p = Push::new(&mut buf);
        present_push(
            &mut p,
            Gen::Gb202,
            Acquire { va: v.timeline + 64, value: 0x1_0000_0002 },
            &CopyRect {
                src_va: v.source,
                dst_va: v.destination,
                src_pitch: 6400,
                dst_pitch: 6400,
                line_bytes: 6400,
                lines: 900,
                layout: SourceLayout::Pitch,
                stamp: None,
            },
            Release { va: v.completion, value: 7, wfi: true, timestamp: false, interrupt: true },
        )
        .unwrap();
        assert_eq!(
            p.words(),
            [
                0x2005_0017, 0x0040_0040, 0x20, 2, 1, 0x0100_1002,
                0x2008_8100, 0x20, 0x0080_0000, 0x20, 0x0100_0000, 6400, 6400, 6400, 900,
                0x2001_80c0, 0x386,
                0x2005_0017, 0x01c0_0000, 0x20, 7, 0, 0x0110_0001,
                0x2001_0008, 0,
            ]
        );
    }

    /// The measured windowed source: 1600x900 AB24, block-linear h = 4, pitch 6400, into a
    /// 1600x900 pitch-linear destination of pitch 6400.
    fn heaven_bl_copy() -> CopyRect {
        let v = tool_vas();
        CopyRect {
            src_va: v.source,
            dst_va: v.destination,
            src_pitch: 6400,
            dst_pitch: 6400,
            line_bytes: 6400,
            lines: 900,
            layout: SourceLayout::BlockLinear {
                block_height_log2: 4,
                element_bytes: 4,
                image_height: 900,
                origin_x_bytes: 0,
                origin_y: 0,
            },
            stamp: None,
        }
    }

    #[test]
    fn production_block_linear_push_follows_nvk() {
        let v = tool_vas();
        for gen in [Gen::Gb202, Gen::Ada] {
            let mut buf = [0u32; PRESENT_PUSH_MAX_DWORDS];
            let mut p = Push::new(&mut buf);
            present_push(
                &mut p,
                gen,
                Acquire { va: v.timeline, value: 5 },
                &heaven_bl_copy(),
                Release { va: v.completion, value: 9, wfi: true, timestamp: false, interrupt: true },
            )
            .unwrap();
            assert_eq!(p.len(), PRESENT_PUSH_MAX_DWORDS);
            assert_eq!(
                p.words(),
                [
                    0x2005_0017, 0x0040_0000, 0x20, 5, 0, 0x0100_1002,
                    0x2008_8100, 0x20, 0x0080_0000, 0x20, 0x0100_0000, 6400, 6400, 6400, 900,
                    // SET_SRC_BLOCK_SIZE (h = 4, FERMI_8 GOB, BL_32 = 0), WIDTH = pitch bytes,
                    // HEIGHT, DEPTH 1, LAYER 0
                    0x2005_81ca, 0x1040, 6400, 900, 1, 0,
                    // SRC_ORIGIN_X (bytes), SRC_ORIGIN_Y
                    0x2002_81d1, 0, 0,
                    // LAUNCH_DMA: SRC_MEMORY_LAYOUT BLOCKLINEAR (bit 7 clear), DST PITCH
                    0x2001_80c0, 0x306,
                    0x2005_0017, 0x01c0_0000, 0x20, 9, 0, 0x0110_0001,
                    0x2001_0008, 0,
                ]
            );
        }
    }

    #[test]
    fn block_size_words() {
        assert_eq!(src_block_size(Gen::Gb202, 4, 4), Some(0x1040));
        assert_eq!(src_block_size(Gen::Ada, 4, 4), Some(0x1040));
        assert_eq!(src_block_size(Gen::Gb202, 0, 1), Some(0x1_1000));
        assert_eq!(src_block_size(Gen::Gb202, 5, 2), Some(0x2_1050));
        assert_eq!(src_block_size(Gen::Ada, 0, 1), None);
        assert_eq!(src_block_size(Gen::Gb202, 6, 4), None);
        assert_eq!(src_block_size(Gen::Gb202, 0, 8), None);
    }

    #[test]
    fn a_sub_rectangle_of_a_block_linear_source() {
        let mut c = heaven_bl_copy();
        c.layout = SourceLayout::BlockLinear {
            block_height_log2: 4,
            element_bytes: 4,
            image_height: 900,
            origin_x_bytes: 400,
            origin_y: 100,
        };
        c.line_bytes = 800;
        c.lines = 200;
        let words = build(|p| copy(p, Gen::Gb202, &c));
        assert_eq!(&words[0..9], &[0x2008_8100, 0x20, 0x0080_0000, 0x20, 0x0100_0000, 6400, 6400, 800, 200]);
        assert_eq!(&words[15..18], &[0x2002_81d1, 400, 100]);
        // Past the image: refused.
        c.lines = 801;
        assert_eq!(copy(&mut Push::new(&mut [0; 64]), Gen::Gb202, &c), Err(PushError::Shape));
        c.lines = 200;
        c.layout = SourceLayout::BlockLinear {
            block_height_log2: 4,
            element_bytes: 4,
            image_height: 900,
            origin_x_bytes: 5601,
            origin_y: 100,
        };
        assert_eq!(copy(&mut Push::new(&mut [0; 64]), Gen::Gb202, &c), Err(PushError::Shape));
    }

    #[test]
    fn copy_shapes_and_vas_are_checked_before_anything_is_written() {
        let base = heaven_bl_copy();
        let mut buf = [0xdead_beefu32; 64];
        let mut p = Push::new(&mut buf);
        let mut c = base;
        c.src_va = MAX_VA;
        assert_eq!(copy(&mut p, Gen::Gb202, &c), Err(PushError::Va));
        let mut c = base;
        c.dst_va = MAX_VA + 4;
        assert_eq!(copy(&mut p, Gen::Gb202, &c), Err(PushError::Va));
        let mut c = base;
        c.lines = 0;
        assert_eq!(copy(&mut p, Gen::Gb202, &c), Err(PushError::Shape));
        let mut c = base;
        c.line_bytes = 6404; // over the destination pitch
        assert_eq!(copy(&mut p, Gen::Gb202, &c), Err(PushError::Shape));
        let mut c = base;
        c.src_pitch = 6400 + 32; // not whole GOBs
        assert_eq!(copy(&mut p, Gen::Gb202, &c), Err(PushError::Shape));
        let mut c = base;
        c.layout = SourceLayout::Pitch;
        c.src_pitch = 6396;
        assert_eq!(copy(&mut p, Gen::Gb202, &c), Err(PushError::Shape));
        assert!(p.is_empty());
        // A release without WFI after a copy is refused.
        let v = tool_vas();
        assert_eq!(
            present_push(
                &mut p,
                Gen::Gb202,
                Acquire { va: v.timeline, value: 1 },
                &base,
                Release { va: v.completion, value: 1, wfi: false, timestamp: false, interrupt: false },
            ),
            Err(PushError::Shape)
        );
        assert!(p.is_empty());
        assert!(buf.iter().all(|w| *w == 0xdead_beef));
    }

    #[test]
    fn a_full_slot_is_refused_whole() {
        let mut buf = [0u32; 5];
        let mut p = Push::new(&mut buf);
        assert_eq!(acquire(&mut p, Acquire { va: 0x1000, value: 1 }), Err(PushError::Full));
        assert!(p.is_empty());
        let mut buf = [0u32; 6];
        let mut p = Push::new(&mut buf);
        assert_eq!(acquire(&mut p, Acquire { va: 0x1000, value: 1 }), Ok(()));
        assert_eq!(p.len(), 6);
    }

    #[test]
    fn the_release_variants() {
        let r = |wfi, timestamp| Release { va: 0, value: 0, wfi, timestamp, interrupt: false };
        assert_eq!(release_execute(&r(false, false)), 0x0100_0001);
        assert_eq!(release_execute(&r(true, false)), 0x0110_0001);
        assert_eq!(release_execute(&r(true, true)), 0x0310_0001);
        assert_eq!(release_execute(&r(false, true)), 0x0300_0001);
    }

    #[test]
    fn method_headers() {
        assert_eq!(method(0, 0x5c, 5), 0x2005_0017);
        assert_eq!(method(4, 0x728, 5), 0x2005_81ca);
        assert_eq!(method(4, 0x744, 2), 0x2002_81d1);
        assert_eq!(method(4, 0x400, 8), 0x2008_8100);
        assert_eq!(method(7, 0xffc, MAX_METHOD_COUNT), 0x3fff_e3ff);
    }

    #[test]
    fn classes_round_trip() {
        for gen in [Gen::Gb202, Gen::Ada] {
            let c = gen.classes();
            assert_eq!(Gen::from_classes(c.gpfifo, c.copy), Some(gen));
        }
        assert_eq!(Gen::from_classes(0xca6f, 0xc7b5), None);
        assert_eq!(Gen::Gb202.classes().usermode, 0xc761);
        assert_eq!(Gen::Ada.classes().usermode, 0xc561);
    }

    // ── GPFIFO, ring, token ──────────────────────────────────────────────────────────────────

    #[test]
    fn gp_entries() {
        assert_eq!(gp_entry(0x1000, 1), Ok((1u64 << 10 << 32) | 0x1000));
        assert_eq!(gp_entry(0xff_ffff_fffc, GP_MAX_DWORDS), Ok(((0xffu64 | (0x1f_ffffu64 << 10)) << 32) | 0xffff_fffc));
        assert_eq!(gp_entry(0x1002, 1), Err(GpError::Va));
        assert_eq!(gp_entry(MAX_VA, 1), Err(GpError::Va));
        assert_eq!(gp_entry(0x1000, 0), Err(GpError::Length));
        assert_eq!(gp_entry(0x1000, GP_MAX_DWORDS + 1), Err(GpError::Length));
    }

    #[test]
    fn the_ring_wraps_and_keeps_one_entry_free() {
        let mut r = Ring::new(4, 1).unwrap();
        let a = r.submit().unwrap();
        let b = r.submit().unwrap();
        let c = r.submit().unwrap();
        assert_eq!((a.index, a.value, a.put), (0, 2, 1));
        assert_eq!((b.index, b.value, b.put), (1, 3, 2));
        assert_eq!((c.index, c.value, c.put), (2, 4, 3));
        assert!(r.is_full());
        assert_eq!(r.submit(), None);
        assert_eq!(r.in_flight(), 3);
        // Completion of the first frees one entry.
        assert_eq!(r.observe(2), 1);
        let d = r.submit().unwrap();
        assert_eq!((d.index, d.value, d.put), (3, 5, 0));
        assert_eq!(r.observe(5), 3);
        let e = r.submit().unwrap();
        assert_eq!((e.index, e.put), (0, 1));
        assert_eq!(r.in_flight(), 1);
    }

    #[test]
    fn the_ring_ignores_stale_and_impossible_values() {
        let mut r = Ring::new(8, 10).unwrap();
        r.submit().unwrap();
        r.submit().unwrap();
        assert_eq!(r.observe(10), 0);
        assert_eq!(r.observe(9), 0);
        assert_eq!(r.observe(13), 0); // never submitted
        assert_eq!(r.observe(11), 1);
        assert_eq!(r.observe(11), 0);
        assert_eq!(r.completed(), 11);
        assert_eq!(Ring::new(1, 0), None);
        assert_eq!(Ring::new(65537, 0), None);
        assert!(Ring::new(65536, 0).is_some());
    }

    #[test]
    fn a_kick_is_the_tool_sequence() {
        let mut r = Ring::new(128, 1).unwrap();
        r.submit().unwrap(); // slot 0: the first round
        let s = r.submit().unwrap();
        let push_va = Ring::slot_va(0x20_0000_1000, s.index, 512);
        assert_eq!(push_va, 0x20_0000_1200);
        let k = kick(s, push_va, 37, Token(0x0005_0003)).unwrap();
        assert_eq!(k.entry_offset, 8);
        assert_eq!(k.userd_offset, 0x8c);
        assert_eq!(k.put, 2);
        assert_eq!(k.doorbell_offset, 0x90);
        assert_eq!(k.token, 0x0005_0003);
        assert_eq!(Token(0x0005_0003).runlist(), 5);
        assert_eq!(Token(0x0005_0003).channel(), 3);
        assert_eq!(Token(0xff7f_ffff).runlist(), 0x7f);
        assert_eq!(Token(0xff7f_ffff).channel(), 0xfff);
    }

    // ── source plans ─────────────────────────────────────────────────────────────────────────

    fn heaven_source() -> SourceDesc {
        SourceDesc {
            offset: 0,
            size: 6_553_600,
            modifier: 0x0300_0000_0060_6014,
            pitch: 6400,
            width: 1600,
            height: 900,
            fourcc: crate::foreign_resource::FOURCC_ABGR8888,
            compressed: false,
        }
    }

    #[test]
    fn the_measured_source_is_block_linear_kind_6() {
        let plan = source_plan(Gen::Gb202, &heaven_source()).unwrap();
        assert_eq!(
            plan.layout,
            SourceLayout::BlockLinear {
                block_height_log2: 4,
                element_bytes: 4,
                image_height: 900,
                origin_x_bytes: 0,
                origin_y: 0,
            }
        );
        assert_eq!(plan.page_kind, Some(0x06));
        assert_eq!(plan.line_bytes, 6400);
        // Not on Ada: the GB20x modifiers name another generation's GOB layout.
        assert_eq!(source_plan(Gen::Ada, &heaven_source()), Err(Why::SourceUnsupported));
    }

    #[test]
    fn pitch_sources_and_refusals() {
        let mut s = heaven_source();
        s.modifier = 0;
        s.size = 5_760_000;
        let plan = source_plan(Gen::Ada, &s).unwrap();
        assert_eq!(plan.layout, SourceLayout::Pitch);
        assert_eq!(plan.page_kind, None);
        // Block-linear needs the whole blocks.
        let mut s = heaven_source();
        s.size = 6_553_599;
        assert_eq!(source_plan(Gen::Gb202, &s), Err(Why::SourceUnsupported));
        // Compressed, a non-RGB format, an offset past u32, a foreign modifier.
        let mut s = heaven_source();
        s.compressed = true;
        assert_eq!(source_plan(Gen::Gb202, &s), Err(Why::SourceUnsupported));
        let mut s = heaven_source();
        s.fourcc = crate::foreign_resource::FOURCC_R8;
        s.modifier = 0;
        assert_eq!(source_plan(Gen::Gb202, &s), Err(Why::SourceUnsupported));
        let mut s = heaven_source();
        s.offset = 1 << 32;
        s.size = u64::MAX;
        assert_eq!(source_plan(Gen::Gb202, &s), Err(Why::SourceUnsupported));
        let mut s = heaven_source();
        s.modifier = 0x0300_0000_0060_6016 | (1 << 23);
        assert_eq!(source_plan(Gen::Gb202, &s), Err(Why::SourceUnsupported));
        assert_eq!(modifier_page_kind(0x0300_0000_0060_6014), 6);
    }

    #[test]
    fn a_plan_feeds_the_builder() {
        let plan = source_plan(Gen::Gb202, &heaven_source()).unwrap();
        let c = CopyRect {
            src_va: 0x20_0080_0000 + plan.offset,
            dst_va: 0x20_0100_0000,
            src_pitch: 6400,
            dst_pitch: 6400,
            line_bytes: plan.line_bytes,
            lines: 900,
            layout: plan.layout,
            stamp: None,
        };
        assert_eq!(c, heaven_bl_copy());
    }

    // ── decisions, the route, retirement ─────────────────────────────────────────────────────

    fn facts() -> Facts {
        Facts {
            knob_on: true,
            channel_alive: true,
            tail: Some(Ok(())),
            source: Ok(()),
            destination_covered: true,
            ring_room: true,
        }
    }

    fn venus(why: Why) -> Decision {
        Decision::Venus { why, after: None }
    }

    #[test]
    fn the_decision_order() {
        let r = Route::new();
        assert_eq!(decide(&facts(), &r, 0), Decision::CopyEngine);
        let mut f = facts();
        f.knob_on = false;
        f.channel_alive = false;
        assert_eq!(decide(&f, &r, 0), venus(Why::FeatureOff));
        f.knob_on = true;
        assert_eq!(decide(&f, &r, 0), venus(Why::ChannelDead));
        f.channel_alive = true;
        f.tail = None;
        f.source = Err(Why::SourceUnsupported);
        assert_eq!(decide(&f, &r, 0), venus(Why::NoTailV3));
        f.tail = Some(Err(()));
        assert_eq!(decide(&f, &r, 0), venus(Why::TailRefused));
        f.tail = Some(Ok(()));
        assert_eq!(decide(&f, &r, 0), venus(Why::SourceUnsupported));
        f.source = Ok(());
        f.destination_covered = false;
        f.ring_room = false;
        assert_eq!(decide(&f, &r, 0), venus(Why::DestinationUncovered));
        f.destination_covered = true;
        assert_eq!(decide(&f, &r, 0), venus(Why::RingFull));
    }

    #[test]
    fn a_fallback_waits_for_an_outstanding_copy_into_the_same_destination() {
        let mut r = Route::new();
        r.on_submit(5, 4);
        let mut f = facts();
        f.ring_room = false;
        assert_eq!(decide(&f, &r, 4), Decision::Venus { why: Why::RingFull, after: Some(5) });
        assert_eq!(decide(&f, &r, 5), venus(Why::RingFull));
        // Several outstanding: the bound is the newest.
        r.on_submit(6, 4);
        assert_eq!(decide(&f, &r, 5), Decision::Venus { why: Why::RingFull, after: Some(6) });
    }

    #[test]
    fn a_completed_copy_retires_its_present_and_nothing_else_does() {
        let mut r = Route::new();
        assert_eq!(r.poll(0, 0), Poll::Idle);
        r.on_submit(3, 2);
        assert_eq!(retire(3, 2, false), Retire::Wait);
        // The producer firing is not completion.
        r.on_producer_fired(10, 2);
        assert_eq!(r.poll(2, 50), Poll::Pending);
        assert_eq!(retire(3, 2, false), Retire::Wait);
        assert_eq!(r.poll(3, 51), Poll::Done);
        assert_eq!(retire(3, 3, false), Retire::Retire);
        assert_eq!(retire(3, 9, true), Retire::Retire);
        assert!(r.may_unlock(3));
        assert_eq!(r.teardown(3), Teardown::Unlock);
    }

    #[test]
    fn a_slow_producer_is_not_a_timeout() {
        let mut r = Route::new();
        r.on_submit(3, 2);
        // No producer fence yet: any time passes.
        assert_eq!(r.poll(2, 1_000_000), Poll::Pending);
        assert!(!r.may_unlock(2));
        assert_eq!(r.teardown(2), Teardown::WaitFor(3));
        r.on_producer_fired(1_000_000, 2);
        assert_eq!(r.poll(2, 1_000_000 + TIMEOUT_AFTER_PRODUCER_MS), Poll::Pending);
        assert_eq!(r.poll(2, 1_000_001 + TIMEOUT_AFTER_PRODUCER_MS), Poll::TimedOut);
    }

    #[test]
    fn a_timeout_poisons_strikes_and_pins_until_the_late_completion() {
        let mut r = Route::new();
        r.on_submit(3, 2);
        r.on_producer_fired(0, 2);
        assert_eq!(r.poll(2, 101), Poll::TimedOut);
        assert!(r.is_poisoned());
        assert_eq!(r.strikes(), 1);
        assert_eq!(r.last_why(), Some(Why::Timeout));
        assert_eq!(retire(3, 2, true), Retire::Discharge);
        // Poisoned: no new copy, the fallback has no bound to wait for, the pages stay pinned.
        assert_eq!(decide(&facts(), &r, 2), venus(Why::Poisoned));
        assert!(!r.may_unlock(2));
        assert_eq!(r.teardown(2), Teardown::Leak);
        // A second poll does not strike again.
        assert_eq!(r.poll(2, 500), Poll::Pending);
        assert_eq!(r.strikes(), 1);
        // The late completion lifts the poison; the strike stays.
        assert_eq!(r.poll(3, 600), Poll::Done);
        assert!(!r.is_poisoned());
        assert!(r.may_unlock(3));
        assert_eq!(r.strikes(), 1);
        assert_eq!(decide(&facts(), &r, 3), Decision::CopyEngine);
    }

    #[test]
    fn three_strikes_disable_until_destroyed() {
        let mut r = Route::new();
        r.on_failure(Why::RmError);
        r.on_failure(Why::RingFull); // a decision, no strike
        r.on_failure(Why::RmError);
        assert_eq!(r.strikes(), 2);
        assert_eq!(decide(&facts(), &r, 0), Decision::CopyEngine);
        r.on_channel_failed(0);
        assert_eq!(r.strikes(), MAX_STRIKES);
        assert!(r.is_disabled());
        assert_eq!(decide(&facts(), &r, 0), venus(Why::StruckOut));
        // Only a new record (the destination destroyed and recreated) starts over.
        assert_eq!(decide(&facts(), &Route::new(), 0), Decision::CopyEngine);
    }

    #[test]
    fn a_channel_failure_poisons_only_with_copies_in_flight() {
        let mut r = Route::new();
        r.on_submit(4, 3);
        r.on_channel_failed(3);
        assert!(r.is_poisoned());
        assert_eq!(r.teardown(3), Teardown::Leak);
        let mut q = Route::new();
        q.on_submit(4, 3);
        q.on_channel_failed(4);
        assert!(!q.is_poisoned());
        assert_eq!(q.teardown(4), Teardown::Unlock);
        assert_eq!(q.last_why(), Some(Why::ChannelFailed));
    }

    #[test]
    fn the_timeout_clock_follows_the_oldest_outstanding_copy() {
        let mut r = Route::new();
        r.on_submit(3, 2);
        r.on_producer_fired(10, 2);
        r.on_submit(4, 2);
        r.on_producer_fired(90, 2); // ignored: the clock is the oldest copy's
        assert_eq!(r.poll(2, 111), Poll::TimedOut);
        // After everything completed, a new copy starts a new clock.
        let mut r = Route::new();
        r.on_submit(3, 2);
        r.on_producer_fired(10, 2);
        assert_eq!(r.poll(3, 20), Poll::Done);
        r.on_submit(4, 3);
        assert_eq!(r.poll(3, 500), Poll::Pending);
        r.on_producer_fired(500, 3);
        assert_eq!(r.poll(3, 600), Poll::Pending);
        assert_eq!(r.poll(3, 601), Poll::TimedOut);
    }

    #[test]
    fn why_codes_are_stable() {
        let all = [
            Why::FeatureOff,
            Why::NoTailV3,
            Why::TailRefused,
            Why::SourceUnsupported,
            Why::DestinationUncovered,
            Why::ChannelDead,
            Why::Timeout,
            Why::RmError,
            Why::ChannelFailed,
            Why::RingFull,
            Why::StruckOut,
            Why::Poisoned,
        ];
        let mut mask = 0u32;
        for (i, w) in all.iter().enumerate() {
            assert_eq!(w.code(), i as u32 + 1);
            assert_eq!(mask & w.bit(), 0);
            mask |= w.bit();
        }
        let strikes: Vec<Why> = all.iter().copied().filter(|w| w.strikes()).collect();
        assert_eq!(strikes, [Why::Timeout, Why::RmError, Why::ChannelFailed]);
    }

    // ── names ────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Ce"), "{n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        for n in COUNTERS {
            assert!(!crate::blt_async::COUNTERS.contains(n));
            assert!(!crate::guest_blob::COUNTERS.contains(n));
        }
        assert!(KNOB.len() <= 14);
        assert_eq!(KNOB_DEFAULT, 0);
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    /// No I/O writes these yet (M3a). Until the M3c writer exists, `kmd_render` must not spell any
    /// of them, nor the knob, so the names are still free when it does; the M3c commit replaces
    /// this with the exact-list check of `guest_blob.rs` against the writer file.
    #[test]
    fn the_names_are_free_in_kmd_render() {
        let Some(render) = render_src() else {
            return;
        };
        let mut stack = std::vec![render];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in COUNTERS {
                        let lit = std::format!("b\"{n}\"");
                        assert!(!text.contains(&lit), "{} already spells {n}", p.display());
                    }
                    assert!(!text.contains("b\"RmCopyEngine\""), "{} spells the knob", p.display());
                }
            }
        }
        assert!(checked > 20);
    }
}

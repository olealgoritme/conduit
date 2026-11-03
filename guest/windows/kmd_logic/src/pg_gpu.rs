//! GPU paging (`NvPgGpu`): VidMm's paging FILL / TRANSFER operations on native allocations,
//! run by a copy engine instead of the CPU.
//!
//! The CPU path (`native_memory::with_cpu_range`) copies through a temporary uncached BAR view of
//! the video memory: about 290 MB/s, with seconds-long holds of the native-memory mutex during
//! loading. NVIDIA's own KMD does these operations on a copy engine. So does this path: a
//! KMD-owned RM channel on an asynchronous copy engine (its own TSG, in a VA space of its own:
//! `kmd_render/src/virtio/native_channel/pg_channel.rs`), and per operation two windows of that
//! VA space: the allocation's range of video memory ([`VID_WINDOW_VA`]) and the system pages
//! through an RM OS descriptor ([`SYS_WINDOW_VA`]) (`native_memory::gpu_paging`). The push is
//! one `LAUNCH_DMA` (a pitch copy, or a fill through the remap constant) and a host semaphore
//! release with WFI; the KMD waits for the release before BuildPagingBuffer returns, so VidMm's
//! paging fence completes exactly as for the CPU path (the operation is done when the DDI
//! returns; the paging DMA buffer stays empty).
//!
//! What is here: the knob ([`Knob`]), the push words ([`copy`], [`fill`], [`push`]), the channel
//! block's layout ([`ring`]), the window and page arithmetic ([`vid_map`], [`sys_pages`],
//! [`chunk_len`]) and the wait budget ([`deadline_ms`], [`spin_100ns`]). Nothing here does I/O.
//!
//! # Method words
//!
//! `BLACKWELL_DMA_COPY_B` (0xcab5, `clcab5.h`), the way UVM drives it
//! (`uvm_hopper_ce.c`: `memcopy`, `memset_4`): `OFFSET_IN_UPPER` (0x400) .. `LINE_COUNT`
//! (0x41c) in one incrementing method, `LAUNCH_DMA` (0x300) with `DATA_TRANSFER_TYPE`
//! `NON_PIPELINED`, `FLUSH_ENABLE` (`FLUSH_TYPE` SYS), both layouts `PITCH`, virtual addresses
//! (Blackwell refuses physical writes to video memory), `DISABLE_PLC`. A fill sets
//! `SET_REMAP_CONST_B` (0x704) and `SET_REMAP_COMPONENTS` (0x708: `DST_X` = `CONST_B`,
//! `COMPONENT_SIZE` FOUR, one destination component) and launches with `REMAP_ENABLE`; its line
//! length is in 4-byte elements.

use crate::ce_present::{self as cp, Gen, Push, PushError};

// ── the knob ─────────────────────────────────────────────────────────────────────────────────

/// `NvPgGpu` (REG_DWORD, default [`DEFAULT_KNOB`]): which paging operations of native
/// allocations go to the copy engine. An operation the copy engine cannot take (no channel, a
/// mapping RM refused, a shape it does not do) runs on the CPU as before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Knob(pub u32);

impl Knob {
    /// `DXGK_OPERATION_TRANSFER` between a native allocation and an MDL's system pages.
    pub const TRANSFER: u32 = 0x1;
    /// `DXGK_OPERATION_FILL` of a native allocation.
    pub const FILL: u32 = 0x2;
    /// `DXGK_OPERATION_VIRTUAL_FILL`.
    pub const VIRTUAL_FILL: u32 = 0x4;
    /// `DXGK_OPERATION_VIRTUAL_TRANSFER` (system pages of VidMm's paging process).
    pub const VIRTUAL_TRANSFER: u32 = 0x8;
    /// Every operation above.
    pub const ALL: u32 = 0xf;
    /// The native-memory mutex stays held while the copy engine runs (only the RM map / unmap
    /// calls need it; off by default).
    pub const HOLD_LOCK: u32 = 0x100;

    pub const fn takes(self, op: Op) -> bool {
        self.0 & op.bit() != 0
    }
    pub const fn hold_lock(self) -> bool {
        self.0 & Self::HOLD_LOCK != 0
    }
}

/// Off: the copy engine path is new and is A/B'd before it becomes the default.
pub const DEFAULT_KNOB: u32 = 0;
/// `NvPgGpuMin` (REG_DWORD, bytes, default 0): smaller operations stay on the CPU.
pub const DEFAULT_MIN_BYTES: u32 = 0;

/// A paging operation the copy engine can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Transfer,
    Fill,
    VirtualFill,
    VirtualTransfer,
}

impl Op {
    pub const fn bit(self) -> u32 {
        match self {
            Op::Transfer => Knob::TRANSFER,
            Op::Fill => Knob::FILL,
            Op::VirtualFill => Knob::VIRTUAL_FILL,
            Op::VirtualTransfer => Knob::VIRTUAL_TRANSFER,
        }
    }

    /// Counter slot of the kind (`PgGpuN<k>`).
    pub const fn index(self) -> usize {
        match self {
            Op::Transfer => 0,
            Op::Fill => 1,
            Op::VirtualFill => 2,
            Op::VirtualTransfer => 3,
        }
    }
}

/// Whether an operation of `bytes` goes to the copy engine.
pub const fn wants(knob: Knob, op: Op, bytes: u64, min_bytes: u32) -> bool {
    knob.takes(op) && bytes != 0 && bytes >= min_bytes as u64
}

/// What one operation does with the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// System pages into video memory (a page-in).
    ToVideo,
    /// Video memory into system pages (an eviction).
    FromVideo,
    /// Video memory filled with a 32-bit pattern.
    Fill(u32),
}

// ── methods ──────────────────────────────────────────────────────────────────────────────────

pub const CE_SET_REMAP_CONST_B: u32 = 0x0704;
pub const CE_OFFSET_OUT_UPPER: u32 = 0x0408;
pub const CE_LINE_LENGTH_IN: u32 = 0x0418;

/// `LAUNCH_DMA_DISABLE_PLC` (26:26).
pub const LAUNCH_DISABLE_PLC: u32 = 1 << 26;

/// `SET_REMAP_COMPONENTS`: `DST_X` = `CONST_B` (5), `COMPONENT_SIZE` FOUR (3 at 17:16), one source
/// and one destination component (0 at 21:20 and 25:24).
pub const REMAP_FILL_U32: u32 = 5 | (3 << 16);

/// `LAUNCH_DMA` of a copy: non-pipelined, flushed (SYS), pitch to pitch, virtual, single line,
/// no PLC.
pub const LAUNCH_COPY: u32 = cp::LAUNCH_TRANSFER_NON_PIPELINED
    | cp::LAUNCH_FLUSH_ENABLE
    | cp::LAUNCH_SRC_PITCH
    | cp::LAUNCH_DST_PITCH
    | LAUNCH_DISABLE_PLC;

/// `LAUNCH_DMA` of a fill: a copy's, through the remap unit.
pub const LAUNCH_FILL: u32 = LAUNCH_COPY | cp::LAUNCH_REMAP_ENABLE;

/// Longest single launch: `LINE_LENGTH_IN` is 32 bits (bytes for a copy, elements for a fill);
/// one launch per chunk stays well below it.
pub const MAX_LAUNCH_BYTES: u64 = 1 << 31;

const fn hi(va: u64) -> u32 {
    (va >> 32) as u32
}

const fn lo(va: u64) -> u32 {
    va as u32
}

fn va_ok(va: u64, len: u64) -> Result<(), PushError> {
    match va.checked_add(len) {
        Some(end) if end <= cp::MAX_VA => Ok(()),
        _ => Err(PushError::Va),
    }
}

/// `len` bytes from `src` to `dst` (both virtual, pitch).
pub fn copy(p: &mut Push<'_>, src: u64, dst: u64, len: u64) -> Result<(), PushError> {
    if len == 0 || len > MAX_LAUNCH_BYTES {
        return Err(PushError::Shape);
    }
    va_ok(src, len)?;
    va_ok(dst, len)?;
    p.method(
        cp::SUBC_CE,
        cp::CE_OFFSET_IN_UPPER,
        &[hi(src), lo(src), hi(dst), lo(dst), 0, 0, len as u32, 1],
    )?;
    p.method(cp::SUBC_CE, cp::CE_LAUNCH_DMA, &[LAUNCH_COPY])
}

/// `len` bytes at `dst` set to the 32-bit `pattern` (`len` a multiple of 4).
pub fn fill(p: &mut Push<'_>, dst: u64, len: u64, pattern: u32) -> Result<(), PushError> {
    if len == 0 || len % 4 != 0 || len > MAX_LAUNCH_BYTES {
        return Err(PushError::Shape);
    }
    va_ok(dst, len)?;
    p.method(
        cp::SUBC_CE,
        CE_SET_REMAP_CONST_B,
        &[pattern, REMAP_FILL_U32],
    )?;
    p.method(cp::SUBC_CE, CE_OFFSET_OUT_UPPER, &[hi(dst), lo(dst)])?;
    p.method(cp::SUBC_CE, CE_LINE_LENGTH_IN, &[(len / 4) as u32, 1])?;
    p.method(cp::SUBC_CE, cp::CE_LAUNCH_DMA, &[LAUNCH_FILL])
}

/// One chunk's push: `SET_OBJECT` on the channel's first push, the work between the video
/// window at `vid_va` and the system window at `sys_va`, then the release of `seq` at
/// `semaphore_va` with WFI (the release waits for the copy engine to be idle).
#[allow(clippy::too_many_arguments)]
pub fn push(
    p: &mut Push<'_>,
    gen: Gen,
    first: bool,
    work: Work,
    vid_va: u64,
    sys_va: u64,
    len: u64,
    semaphore_va: u64,
    seq: u64,
) -> Result<(), PushError> {
    if first {
        cp::set_object(p, gen)?;
    }
    match work {
        Work::ToVideo => copy(p, sys_va, vid_va, len)?,
        Work::FromVideo => copy(p, vid_va, sys_va, len)?,
        Work::Fill(pattern) => fill(p, vid_va, len, pattern)?,
    }
    release(p, semaphore_va, seq)
}

/// A push of only the release of `seq` (the channel's first push: `SET_OBJECT` with it).
pub fn release_push(
    p: &mut Push<'_>,
    gen: Gen,
    first: bool,
    semaphore_va: u64,
    seq: u64,
) -> Result<(), PushError> {
    if first {
        cp::set_object(p, gen)?;
    }
    release(p, semaphore_va, seq)
}

fn release(p: &mut Push<'_>, semaphore_va: u64, seq: u64) -> Result<(), PushError> {
    cp::release(
        p,
        cp::Release {
            va: semaphore_va,
            value: seq,
            wfi: true,
            timestamp: false,
            interrupt: false,
        },
    )
}

/// The dwords of the largest push: `SET_OBJECT` 2, a fill 3 + 3 + 3 + 2, the release 6.
pub const PUSH_MAX_DWORDS: usize = 19;

// ── the channel's block ──────────────────────────────────────────────────────────────────────

/// The channel's system-memory block: the GPFIFO ring, the semaphore, the push slots. One
/// push is in flight at a time (each chunk is waited for), so a slot is free again long
/// before its sequence comes round.
pub mod ring {
    pub const BYTES: u64 = 0x1_0000;
    pub const GPFIFO_AT: u64 = 0;
    pub const ENTRIES: u32 = 256;
    pub const GPFIFO_BYTES: u64 = ENTRIES as u64 * 8;
    /// The semaphore the releases write (64 bits): the last sequence done.
    pub const SEMAPHORE_AT: u64 = 0x1000;
    pub const PUSH_AT: u64 = 0x2000;
    pub const PUSH_SLOT_BYTES: u64 = 0x200;
    pub const PUSH_SLOTS: u64 = (BYTES - PUSH_AT) / PUSH_SLOT_BYTES;
    const _: () = assert!(GPFIFO_AT + GPFIFO_BYTES <= SEMAPHORE_AT);
    const _: () = assert!(super::PUSH_MAX_DWORDS as u64 * 4 <= PUSH_SLOT_BYTES);

    /// The push slot of sequence `seq`.
    pub const fn push_offset(seq: u64) -> u64 {
        PUSH_AT + (seq % PUSH_SLOTS) * PUSH_SLOT_BYTES
    }

    /// The GPFIFO entry after `put`.
    pub const fn next(put: u32) -> u32 {
        (put + 1) % ENTRIES
    }
}

// ── windows and pages ────────────────────────────────────────────────────────────────────────

/// The video-memory window of the paging VA space: one chunk's range of the allocation.
pub const VID_WINDOW_VA: u64 = 0x40_0000_0000;
pub const VID_WINDOW_BYTES: u64 = 512 << 20;
/// The system-page window: one chunk's pages.
pub const SYS_WINDOW_VA: u64 = 0x48_0000_0000;
pub const SYS_WINDOW_BYTES: u64 = 512 << 20;
/// Bytes per chunk (each chunk is mapped, pushed, waited for and unmapped).
pub const CHUNK_BYTES: u64 = 256 << 20;
const _: () = assert!(CHUNK_BYTES + 2 * BIG_PAGE <= VID_WINDOW_BYTES);
const _: () = assert!(CHUNK_BYTES + PAGE <= SYS_WINDOW_BYTES);
const _: () = assert!(SYS_WINDOW_VA + SYS_WINDOW_BYTES <= cp::MAX_VA);
const _: () = assert!(VID_WINDOW_VA + VID_WINDOW_BYTES <= SYS_WINDOW_VA);

pub const PAGE: u64 = 0x1000;
/// The video memory's big page (the device's 64 KiB big pages).
pub const BIG_PAGE: u64 = 0x1_0000;

/// The bytes of the next chunk when `done` of `total` are done.
pub const fn chunk_len(total: u64, done: u64) -> u64 {
    let left = total.saturating_sub(done);
    if left < CHUNK_BYTES {
        left
    } else {
        CHUNK_BYTES
    }
}

/// How a chunk of video memory is mapped into the video window: the RM memory's
/// `[map_offset, map_offset + map_len)` at [`VID_WINDOW_VA`], with big pages when that range
/// is big-page aligned inside the memory; the chunk's first byte is at
/// `VID_WINDOW_VA + delta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VidMap {
    pub map_offset: u64,
    pub map_len: u64,
    pub delta: u64,
    pub big: bool,
}

/// The mapping of `[offset, offset + len)` of a memory of `memory_size` bytes, `None` when the
/// range leaves the memory or the window.
pub fn vid_map(offset: u64, len: u64, memory_size: u64) -> Option<VidMap> {
    let end = offset.checked_add(len)?;
    if len == 0 || end > memory_size {
        return None;
    }
    let round = |align: u64| -> Option<(u64, u64)> {
        let first = offset & !(align - 1);
        let last = end.checked_add(align - 1)? & !(align - 1);
        Some((first, last))
    };
    let (first, last, big) = match round(BIG_PAGE) {
        Some((first, last)) if last <= memory_size => (first, last, true),
        _ => {
            let (first, last) = round(PAGE)?;
            if last > memory_size {
                return None;
            }
            (first, last, false)
        }
    };
    let map_len = last - first;
    if map_len > VID_WINDOW_BYTES {
        return None;
    }
    Some(VidMap {
        map_offset: first,
        map_len,
        delta: offset - first,
        big,
    })
}

/// The system pages of a chunk: `count` page frames from `first` of the operation's page list,
/// the chunk's first byte `intra` bytes into the first one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysPages {
    pub first: usize,
    pub count: usize,
    pub intra: u64,
}

/// The pages of the chunk `[at, at + len)`, `at` counted from the start of the operation's first
/// page (the operation's own byte offset into that page included). `None` past `pages` pages.
pub fn sys_pages(at: u64, len: u64, pages: usize) -> Option<SysPages> {
    if len == 0 {
        return None;
    }
    let last = at.checked_add(len - 1)?;
    let first = usize::try_from(at / PAGE).ok()?;
    let end = usize::try_from(last / PAGE).ok()?.checked_add(1)?;
    if end > pages {
        return None;
    }
    let count = end - first;
    if count as u64 * PAGE > SYS_WINDOW_BYTES {
        return None;
    }
    Some(SysPages {
        first,
        count,
        intra: at % PAGE,
    })
}

/// The byte where an MDL's paging range starts, counted from the start of its first page frame:
/// DXGK's `MdlOffset` is in pages of the PFN array, except that page 0 starts at the MDL's
/// `ByteOffset` (`build_paging_buffer::MdlWindow::slice_at_page`).
pub const fn mdl_start(mdl_page: u32, byte_offset: u32) -> u64 {
    if mdl_page == 0 {
        byte_offset as u64
    } else {
        mdl_page as u64 * PAGE
    }
}

/// The page frames an MDL describes (`ADDRESS_AND_SIZE_TO_SPAN_PAGES`).
pub const fn mdl_pages(byte_offset: u32, byte_count: u32) -> u64 {
    let span = (byte_offset & 0xfff) as u64 + byte_count as u64;
    span.div_ceil(PAGE)
}

// ── waiting ──────────────────────────────────────────────────────────────────────────────────

/// The first push (`SET_OBJECT` and a release) must land within this.
pub const FIRST_PUSH_MS: u64 = 250;

/// How long a chunk of `bytes` may take before the channel is declared hung: two seconds, plus
/// a second per 256 MiB (an engine far slower than any copy engine).
pub const fn deadline_ms(bytes: u64) -> u64 {
    2_000 + bytes / (256 << 10)
}

/// How long the waiter spins (100-ns units) before it sleeps (a sleep is a timer tick, up to
/// 15.6 ms): twice the time at 8 GB/s plus 200 us, at most 5 ms.
pub const fn spin_100ns(bytes: u64) -> u64 {
    // 8 GB/s = 800 bytes per 100 ns.
    let expected = bytes / 800;
    let spin = expected * 2 + 2_000;
    if spin > 50_000 {
        50_000
    } else {
        spin
    }
}

/// The counters the I/O publishes (at most 14 characters), checked by the tests.
pub const COUNTERS: [&str; 22] = [
    "PgGpuKnob",
    "PgGpuN0",
    "PgGpuN1",
    "PgGpuN2",
    "PgGpuN3",
    "PgGpuKiB",
    "PgGpuUs",
    "PgGpuMaxUs",
    "PgGpuWaitUs",
    "PgGpuMapUs",
    "PgGpuCpuN",
    "PgGpuCpuKiB",
    "PgGpuFallN",
    "PgGpuFallWhy",
    "PgGpuChState",
    "PgGpuChErr",
    "PgGpuEngine",
    "PgGpuToken",
    "PgGpuHung",
    "PgGpuChunks",
    "PgGpuNoGo",
    "PgGpuNoGoWhy",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn words(
        f: impl FnOnce(&mut Push<'_>) -> Result<(), PushError>,
    ) -> Result<alloc_free::Words, PushError> {
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        f(&mut p)?;
        Ok(alloc_free::Words::from(p.words()))
    }

    mod alloc_free {
        #[derive(Debug, PartialEq, Eq)]
        pub struct Words {
            pub w: [u32; 64],
            pub n: usize,
        }
        impl From<&[u32]> for Words {
            fn from(s: &[u32]) -> Self {
                let mut w = [0u32; 64];
                w[..s.len()].copy_from_slice(s);
                Words { w, n: s.len() }
            }
        }
        impl Words {
            pub fn get(&self) -> &[u32] {
                &self.w[..self.n]
            }
        }
    }

    #[test]
    fn the_knob_selects_kinds_and_size() {
        let k = Knob(Knob::TRANSFER | Knob::VIRTUAL_TRANSFER);
        assert!(wants(k, Op::Transfer, 4096, 0));
        assert!(!wants(k, Op::Fill, 4096, 0));
        assert!(wants(k, Op::VirtualTransfer, 4096, 0));
        assert!(!wants(k, Op::Transfer, 0, 0));
        assert!(!wants(k, Op::Transfer, 4095, 4096));
        assert!(!wants(Knob(DEFAULT_KNOB), Op::Transfer, 1 << 20, 0));
        assert!(!k.hold_lock());
        assert!(Knob(Knob::HOLD_LOCK).hold_lock());
        assert_eq!(
            Knob::ALL,
            Op::Transfer.bit() | Op::Fill.bit() | Op::VirtualFill.bit() | Op::VirtualTransfer.bit()
        );
    }

    #[test]
    fn a_copy_is_one_offset_block_and_a_launch() {
        let w = words(|p| copy(p, 0x48_0000_1000, 0x40_0002_0000, 0x10_0000)).unwrap();
        assert_eq!(
            w.get(),
            &[
                // OFFSET_IN_UPPER (0x400 >> 2 = 0x100), 8 dwords, subchannel 4.
                0x2008_8100,
                0x48,
                0x0000_1000,
                0x40,
                0x0002_0000,
                0,
                0,
                0x10_0000,
                1,
                // LAUNCH_DMA (0x300 >> 2 = 0xc0), 1 dword.
                0x2001_80c0,
                // NON_PIPELINED 2 | FLUSH 1<<2 | SRC_PITCH 1<<7 | DST_PITCH 1<<8 | DISABLE_PLC 1<<26.
                0x0400_0186,
            ]
        );
    }

    #[test]
    fn a_fill_goes_through_the_remap_constant() {
        let w = words(|p| fill(p, 0x40_0000_0000, 0x2000, 0xdead_beef)).unwrap();
        assert_eq!(
            w.get(),
            &[
                // SET_REMAP_CONST_B (0x704 >> 2 = 0x1c1), 2 dwords: the pattern, the components.
                0x2002_81c1,
                0xdead_beef,
                0x0003_0005,
                // OFFSET_OUT_UPPER (0x408 >> 2 = 0x102), 2 dwords.
                0x2002_8102,
                0x40,
                0,
                // LINE_LENGTH_IN (0x418 >> 2 = 0x106): 0x800 elements, LINE_COUNT 1.
                0x2002_8106,
                0x800,
                1,
                0x2001_80c0,
                // The copy's launch with REMAP_ENABLE (1 << 10).
                0x0400_0586,
            ]
        );
        assert_eq!(
            fill(&mut Push::new(&mut [0u32; 64]), 0x1000, 6, 0),
            Err(PushError::Shape)
        );
    }

    #[test]
    fn launches_refuse_empty_long_and_high_ranges() {
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        assert_eq!(copy(&mut p, 0, 0x1000, 0), Err(PushError::Shape));
        assert_eq!(
            copy(&mut p, 0, 0x1000, MAX_LAUNCH_BYTES + 1),
            Err(PushError::Shape)
        );
        assert_eq!(
            copy(&mut p, cp::MAX_VA - 0x1000, 0, 0x2000),
            Err(PushError::Va)
        );
        assert!(p.is_empty());
    }

    #[test]
    fn a_push_copies_in_the_right_direction_and_releases_with_wfi() {
        let into = words(|p| {
            push(
                p,
                Gen::Gb202,
                true,
                Work::ToVideo,
                0x40_0000_0000,
                0x48_0000_0000,
                0x1000,
                0x2_1000,
                7,
            )
        })
        .unwrap();
        let w = into.get();
        // SET_OBJECT of 0xcab5 on subchannel 4.
        assert_eq!(&w[..2], &[0x2001_8000, 0xcab5]);
        // Source = system window, destination = video window.
        assert_eq!(&w[3..7], &[0x48, 0, 0x40, 0]);
        // The release: host SEM_ADDR_LO (0x5c >> 2 = 0x17) on subchannel 0, 5 dwords, 64-bit
        // payload with WFI.
        let r = &w[w.len() - 6..];
        assert_eq!(r[0], 0x2005_0017);
        assert_eq!(&r[1..5], &[0x2_1000, 0, 7, 0]);
        assert_eq!(
            r[5],
            cp::SEM_OPERATION_RELEASE | cp::SEM_PAYLOAD_SIZE_64BIT | cp::SEM_RELEASE_WFI_EN
        );
        let out = words(|p| {
            push(
                p,
                Gen::Gb202,
                false,
                Work::FromVideo,
                0x40_0000_0000,
                0x48_0000_0000,
                0x1000,
                0x2_1000,
                8,
            )
        })
        .unwrap();
        assert_eq!(&out.get()[1..5], &[0x40, 0, 0x48, 0]);
        let fill = words(|p| {
            push(
                p,
                Gen::Gb202,
                true,
                Work::Fill(0),
                0x40_0000_0000,
                0,
                0x1000,
                0x2_1000,
                9,
            )
        })
        .unwrap();
        assert!(fill.get().len() <= PUSH_MAX_DWORDS);
    }

    #[test]
    fn the_ring_holds_its_parts() {
        assert_eq!(ring::push_offset(0), ring::PUSH_AT);
        assert_eq!(ring::push_offset(ring::PUSH_SLOTS), ring::PUSH_AT);
        assert!(ring::push_offset(ring::PUSH_SLOTS - 1) + ring::PUSH_SLOT_BYTES <= ring::BYTES);
        assert_eq!(ring::next(ring::ENTRIES - 1), 0);
        assert_eq!(ring::SEMAPHORE_AT % 8, 0);
    }

    #[test]
    fn video_ranges_map_with_big_pages_when_aligned() {
        let m = vid_map(0x1_3000, 0x2000, 0x10_0000).unwrap();
        assert_eq!(
            m,
            VidMap {
                map_offset: 0x1_0000,
                map_len: 0x1_0000,
                delta: 0x3000,
                big: true
            }
        );
        // The big-page round-up leaves the memory: 4 KiB pages.
        let m = vid_map(0xf_e000, 0x1000, 0xf_f000).unwrap();
        assert_eq!(
            m,
            VidMap {
                map_offset: 0xf_e000,
                map_len: 0x1000,
                delta: 0,
                big: false
            }
        );
        // Unaligned bytes inside a page.
        let m = vid_map(0x10, 0x20, 0x1000).unwrap();
        assert_eq!(
            m,
            VidMap {
                map_offset: 0,
                map_len: 0x1000,
                delta: 0x10,
                big: false
            }
        );
        assert_eq!(vid_map(0, 0x2000, 0x1000), None);
        assert_eq!(vid_map(0, 0, 0x1000), None);
        assert_eq!(vid_map(u64::MAX, 2, u64::MAX), None);
        // A whole chunk fits the window even unaligned.
        let m = vid_map(0x8000, CHUNK_BYTES, 1 << 40).unwrap();
        assert!(m.map_len <= VID_WINDOW_BYTES && m.big);
    }

    #[test]
    fn system_pages_of_a_chunk() {
        assert_eq!(
            sys_pages(0, 0x1000, 1),
            Some(SysPages {
                first: 0,
                count: 1,
                intra: 0
            })
        );
        assert_eq!(
            sys_pages(0x800, 0x1000, 2),
            Some(SysPages {
                first: 0,
                count: 2,
                intra: 0x800
            })
        );
        assert_eq!(
            sys_pages(0x3000, 0x2000, 5),
            Some(SysPages {
                first: 3,
                count: 2,
                intra: 0
            })
        );
        assert_eq!(sys_pages(0x3000, 0x2001, 5), None);
        assert_eq!(sys_pages(0, 0, 5), None);
        assert_eq!(mdl_start(0, 0x200), 0x200);
        assert_eq!(mdl_start(3, 0x200), 0x3000);
        assert_eq!(mdl_pages(0, 0x2000), 2);
        assert_eq!(mdl_pages(0x200, 0x2000), 3);
        assert_eq!(mdl_pages(0, 0), 0);
    }

    #[test]
    fn chunks_cover_the_operation() {
        let total = 3 * CHUNK_BYTES + 0x1000;
        let mut done = 0;
        let mut n = 0;
        while done < total {
            let len = chunk_len(total, done);
            assert!(len > 0 && len <= CHUNK_BYTES);
            done += len;
            n += 1;
        }
        assert_eq!((done, n), (total, 4));
        assert_eq!(chunk_len(10, 10), 0);
    }

    #[test]
    fn waits_scale_with_the_bytes() {
        assert_eq!(deadline_ms(0), 2_000);
        assert_eq!(deadline_ms(CHUNK_BYTES), 3_024);
        assert_eq!(spin_100ns(0), 2_000);
        assert_eq!(spin_100ns(CHUNK_BYTES), 50_000);
    }

    #[test]
    fn counter_names_fit_the_registry_mirror() {
        for name in COUNTERS {
            assert!(name.len() <= 14, "{name}");
            assert!(name.starts_with("PgGpu"), "{name}");
        }
        for (i, a) in COUNTERS.iter().enumerate() {
            for b in &COUNTERS[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}

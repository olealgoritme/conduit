//! The KMD's own copy-engine channel (`RmCopyEngine`): the pure half of milestone M3b. Design:
//! `docs/rm-copy-engine-present.md` section 11 (what is built, and how it differs from the tool:
//! 11.9). The I/O half is `kmd_render/src/virtio/rm_client/ce_channel.rs` (bring-up, submit, poll,
//! teardown) and `ce_selftest.rs` (the hardware self-test of `RmCopyEngine` = 2). Nothing here does
//! I/O, reads a clock or takes a lock.
//!
//! What is here:
//!
//! * The RM parameter blocks the channel's bring-up sends, byte for byte what
//!   `guest/rmclient/tests/crm_ce_copy_smoke.c` (the M1 tool that PASSed on a GB202) sends: the
//!   device, the VA space, the channel group (TSG), the subcontext, the GPFIFO channel with its
//!   engine type, the copy-engine object, the usermode doorbell, and the controls (engine query,
//!   CE caps, BIND, the work-submit token and its notifier index, GPFIFO schedule). Plus the two
//!   messages librmclient's `crm_map_dma2` / `crm_unmap_dma` send for a GPU mapping
//!   (`NV50_MEMORY_VIRTUAL`, `NV_ESC_RM_MAP_MEMORY_DMA`, `NV_ESC_RM_UNMAP_MEMORY_DMA`) and the class
//!   list query NVK uses to pick the generation (nvk-rm patch 0003).
//! * The channel's memory layout (the ring allocation: GPFIFO, completion and producer semaphores,
//!   push slots; the control allocation: error notifier and USERD) and its GPU VAs.
//! * The engine pick (`GET_ENGINES_V2` + `CE_GET_CAPS_V2` replies, section 7.2 of the design).
//! * The bring-up step machine ([`BringUp`]): the stages in the tool's order, what each one made,
//!   and the reverse-order undo ([`next_undo`]) that a failed bring-up and a teardown both run.
//! * The service state ([`Svc`]): cold -> bringing up -> ready, a failure is a strike and a
//!   cool-down, [`MAX_STRIKES`] disable the subsystem for the transport generation (the
//!   `rm_sysmem::Svc` philosophy), with the deadline constants.
//! * The self-test's rules ([`selftest`]): the pattern, the two copies, the verdict words.
//! * [`COUNTERS`]: the `Ce*` names the I/O writes (M3b's), checked against the I/O file by the
//!   tests below.
//!
//! The push words, the GPFIFO entry, the ring of slots and the completion watermark are NOT here:
//! they are `crate::ce_present` ([`cp::Push`], [`cp::present_push`], [`cp::gp_entry`],
//! [`cp::Ring`], [`cp::kick`]), reused as they are.
//!
//! # How the byte layouts were checked
//!
//! The struct definitions of `crm_ce_copy_smoke.c` (lines 186 to 250) and of
//! `guest/rmclient/src/nv_ioctl_defs.h` were compiled with the C compiler on the host, together
//! with the tool's own fill code (`chan_create`, the copier's device and VA space, the usermode
//! object, librmclient's `crm_map_dma2` / `rm_map_dma` / `rm_unmap_dma`) for fixed sample inputs,
//! and every nonzero byte was printed as `offset:value`, together with `sizeof` and `offsetof` of
//! every field this file writes. The tests below (`*_is_the_tools_block`) pin exactly those bytes
//! and require every other byte to be zero; the sizes are also the 610.57.04 allowlist's
//! (`host/backend/gen/src/rmallow/v610_57_04.rs`, rows cited at each constant).

use crate::ce_present::{self as cp, Gen};
use crate::rm_client::{Fail, FailKind, MEM_ALLOC_BYTES, NV0080_ALLOC_BYTES};

// ── classes, controls, escapes ───────────────────────────────────────────────────────────────

/// `FERMI_VASPACE_A`, 56-byte parameters (allowlist 849).
pub const FERMI_VASPACE_A: u32 = 0x90f1;
/// `FERMI_CONTEXT_SHARE_A`, 12 bytes (838).
pub const FERMI_CONTEXT_SHARE_A: u32 = 0x9067;
/// `KEPLER_CHANNEL_GROUP_A`, 20 bytes (850).
pub const KEPLER_CHANNEL_GROUP_A: u32 = 0xa06c;
/// `NV50_MEMORY_VIRTUAL`, 128 bytes (833).
pub const NV50_MEMORY_VIRTUAL: u32 = 0x50a0;
/// `NV01_MEMORY_SYSTEM`, 128 bytes (798).
pub const NV01_MEMORY_SYSTEM: u32 = crate::rm_sysmem::NV01_MEMORY_SYSTEM;

/// `NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2`, 804 bytes (143): `{numClasses, classList[200]}`.
pub const CTRL_GET_CLASSLIST_V2: u32 = 0x0080_0292;
/// `NV2080_CTRL_CMD_GPU_GET_ENGINES_V2`, 340 bytes (268): `{engineCount, engineList[0x54]}`.
pub const CTRL_GET_ENGINES_V2: u32 = 0x2080_0170;
/// `NV2080_CTRL_CMD_CE_GET_CAPS_V2`, 8 bytes (470): `{ceEngineType, capsTbl[2]}`.
pub const CTRL_CE_GET_CAPS_V2: u32 = 0x2080_2a03;
/// `NVA06C_CTRL_CMD_GPFIFO_SCHEDULE`, 3 bytes (638): `{bEnable, bSkipSubmit, bSkipEnable}`.
pub const CTRL_GPFIFO_SCHEDULE: u32 = 0xa06c_0101;
/// `NVA06F_CTRL_CMD_BIND`, 4 bytes (646): `{engineType}`.
pub const CTRL_BIND: u32 = 0xa06f_0104;
/// `NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN`, 4 bytes (730).
pub const CTRL_GET_WORK_SUBMIT_TOKEN: u32 = 0xc36f_0108;
/// `NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`, 4 bytes (731).
pub const CTRL_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX: u32 = 0xc36f_010a;

/// `NV_ESC_RM_MAP_MEMORY_DMA` / `NV_ESC_RM_UNMAP_MEMORY_DMA` (`nv_ioctl_defs.h`): not
/// allowlist-gated on the host (plain passthrough).
pub const ESC_RM_MAP_MEMORY_DMA: u32 = 0x57;
pub const ESC_RM_UNMAP_MEMORY_DMA: u32 = 0x58;

pub const CLASSLIST_BYTES: usize = 804;
pub const CLASSLIST_MAX: usize = 200;
pub const ENGINES_BYTES: usize = 340;
pub const ENGINES_MAX: usize = 0x54;
pub const CE_CAPS_BYTES: usize = 8;
pub const SCHEDULE_BYTES: usize = 3;
/// `BIND`, the token and its notifier index: one `u32`.
pub const U32_PARAM_BYTES: usize = 4;
pub const VASPACE_BYTES: usize = 56;
pub const TSG_BYTES: usize = 20;
pub const CTXSHARE_BYTES: usize = 12;
pub const CHANNEL_BYTES: usize = 376;
pub const CE_OBJECT_BYTES: usize = 8;
/// `NV_HOPPER_USERMODE_A_PARAMS` (from 0xc661 on; the older usermode classes take none).
pub const USERMODE_PARAM_BYTES: usize = 2;
pub const NVOS46_BYTES: usize = 64;
pub const NVOS46_DMA_OFFSET_AT: usize = 48;
pub const NVOS46_STATUS_AT: usize = 56;
pub const NVOS47_BYTES: usize = 48;
pub const NVOS47_STATUS_AT: usize = 40;

const _: () = assert!(NV0080_ALLOC_BYTES == 56 && MEM_ALLOC_BYTES == 128);

// ── engines ──────────────────────────────────────────────────────────────────────────────────

/// `NV2080_ENGINE_TYPE_GRAPHICS`.
pub const ENGINE_GRAPHICS: u32 = 0x01;
const ENGINE_COPY0: u32 = 0x09;
const ENGINE_COPY10: u32 = 0x34;
/// `COPY0..COPY19`.
pub const MAX_COPY_ENGINES: u32 = 20;

/// `NV2080_ENGINE_TYPE_COPY(i)` (`cl2080_notification.h`): `COPY0..9` are 0x09..0x12,
/// `COPY10..19` are 0x34..0x3d.
pub const fn copy_engine(i: u32) -> Option<u32> {
    if i < 10 {
        Some(ENGINE_COPY0 + i)
    } else if i < MAX_COPY_ENGINES {
        Some(ENGINE_COPY10 + i - 10)
    } else {
        None
    }
}

/// The `n` of `COPYn` for an engine type, `None` for any other engine.
pub const fn copy_index(engine_type: u32) -> Option<u32> {
    if engine_type >= ENGINE_COPY0 && engine_type < ENGINE_COPY0 + 10 {
        Some(engine_type - ENGINE_COPY0)
    } else if engine_type >= ENGINE_COPY10 && engine_type < ENGINE_COPY10 + 10 {
        Some(engine_type - ENGINE_COPY10 + 10)
    } else {
        None
    }
}

/// `CE_GET_CAPS_V2` byte 0 (`ctrl2080ce.h`).
pub const CAPS_GRCE: u8 = 0x01;
pub const CAPS_SHARED: u8 = 0x02;
pub const CAPS_SYSMEM_WRITE: u8 = 0x08;

/// One copy engine's answer to `CE_GET_CAPS_V2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CeCaps {
    pub engine_type: u32,
    pub caps: [u8; 2],
}

impl CeCaps {
    pub const fn is_grce(&self) -> bool {
        self.caps[0] & CAPS_GRCE != 0
    }

    /// `CeCaps` registry word: `caps[1] << 8 | caps[0]`.
    pub const fn word(&self) -> u32 {
        ((self.caps[1] as u32) << 8) | self.caps[0] as u32
    }
}

/// The engine the channel runs on (section 7.2): the first async copy engine (not a GRCE) that
/// can write system memory and is not shared; else the first async one that can write system
/// memory; else the tool's own rule (the first async one, whatever its caps: the rule the M1 PASS
/// ran with). `None`: only GRCEs (the route needs an async CE; it never shares GR's runlist).
pub fn pick_engine(caps: &[CeCaps]) -> Option<CeCaps> {
    let usable = |c: &&CeCaps| !c.is_grce() && copy_index(c.engine_type).is_some();
    let sysmem = |c: &&CeCaps| c.caps[0] & CAPS_SYSMEM_WRITE != 0;
    let shared = |c: &&CeCaps| c.caps[0] & CAPS_SHARED != 0;
    caps.iter()
        .filter(usable)
        .find(|c| sysmem(c) && !shared(c))
        .or_else(|| caps.iter().filter(usable).find(sysmem))
        .or_else(|| caps.iter().find(usable))
        .copied()
}

/// How many `CE_GET_CAPS_V2` queries one bring-up makes at most (every `COPYn` the GPU lists).
pub const MAX_CAPS_QUERIES: usize = MAX_COPY_ENGINES as usize;

/// The copy engines of a `GET_ENGINES_V2` reply (`engineCount` clamped to the list), in order.
pub fn copy_engines(reply: &[u8], out: &mut [u32]) -> usize {
    let count = (get32(reply, 0).unwrap_or(0) as usize).min(ENGINES_MAX);
    let mut n = 0;
    for i in 0..count {
        let Some(t) = get32(reply, 4 + 4 * i) else {
            break;
        };
        if copy_index(t).is_some() && n < out.len() && !out[..n].contains(&t) {
            out[n] = t;
            n += 1;
        }
    }
    n
}

/// A `CE_GET_CAPS_V2` reply for `engine_type` (RM echoes the type; a reply naming another engine
/// is not one).
pub fn parse_ce_caps(reply: &[u8], engine_type: u32) -> Option<CeCaps> {
    if get32(reply, 0)? != engine_type || reply.len() < CE_CAPS_BYTES {
        return None;
    }
    Some(CeCaps { engine_type, caps: [reply[4], reply[5]] })
}

/// The generation of a class list (`GET_CLASSLIST_V2` reply): GB20x when the device lists the
/// Blackwell channel, copy and usermode classes, else Ada (AD10x) with the Ampere ones. A GB20x
/// lists the older classes too, so Blackwell is asked first (nvk-rm 0003 picks the highest).
pub fn gen_from_class_list(reply: &[u8]) -> Option<Gen> {
    let count = (get32(reply, 0)? as usize).min(CLASSLIST_MAX);
    let has = |class: u32| (0..count).any(|i| get32(reply, 4 + 4 * i) == Some(class));
    for gen in [Gen::Gb202, Gen::Ada] {
        let c = gen.classes();
        if has(c.gpfifo) && has(c.copy) && has(c.usermode) {
            return Some(gen);
        }
    }
    None
}

/// `NV_HOPPER_USERMODE_A_PARAMS` goes with the usermode classes from 0xc661 on; the older ones
/// (Ada's 0xc561) take no parameters (`crm_ce_copy_smoke`, allowlist rows 880 and 895).
pub const fn usermode_takes_params(class: u32) -> bool {
    class >= 0xc661
}

// ── little-endian helpers ────────────────────────────────────────────────────────────────────

fn put32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// A `u32` at `at`, or `None` past the end.
pub fn get32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A `u64` at `at`, or `None` past the end.
pub fn get64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

// ── parameter blocks ─────────────────────────────────────────────────────────────────────────

/// `NV_DEVICE_ALLOCATION_FLAGS_VASPACE_BIG_PAGE_SIZE_64k`.
pub const DEVICE_FLAGS_BIG_PAGE_64K: u32 = 0x200;
/// `NV_DEVICE_ALLOCATION_VAMODE_OPTIONAL_MULTIPLE_VASPACES`.
pub const VAMODE_OPTIONAL_MULTIPLE_VASPACES: u32 = 0;

/// `NV0080_ALLOC_PARAMETERS` of the copier's device: `hClientShare = root`, 64 KiB big pages,
/// `vaMode = OPTIONAL_MULTIPLE_VASPACES` (the tool's; the ring client's device is all zero).
pub fn device_params(root: u32) -> [u8; NV0080_ALLOC_BYTES] {
    let mut a = [0u8; NV0080_ALLOC_BYTES];
    put32(&mut a, 4, root);
    put32(&mut a, 16, DEVICE_FLAGS_BIG_PAGE_64K);
    put32(&mut a, 48, VAMODE_OPTIONAL_MULTIPLE_VASPACES);
    a
}

/// `NV_VASPACE_ALLOCATION_INDEX_GPU_DEVICE`: the device's own VA space (the tool's default; a new
/// one left GR context buffers nowhere under GSP, nvk-rm 0008).
pub const VASPACE_INDEX_GPU_DEVICE: u32 = 3;

/// `NV_VASPACE_ALLOCATION_PARAMETERS { index = GPU_DEVICE }`, all else zero.
pub fn vaspace_params() -> [u8; VASPACE_BYTES] {
    let mut a = [0u8; VASPACE_BYTES];
    put32(&mut a, 0, VASPACE_INDEX_GPU_DEVICE);
    a
}

/// `NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS { hVASpace, engineType }`.
pub fn channel_group_params(h_vaspace: u32, engine_type: u32) -> [u8; TSG_BYTES] {
    let mut a = [0u8; TSG_BYTES];
    put32(&mut a, 8, h_vaspace);
    put32(&mut a, 12, engine_type);
    a
}

/// `NV_CTXSHARE_ALLOCATION_FLAGS_SUBCONTEXT_SYNC` (VEID 0).
pub const CTXSHARE_FLAGS_SYNC: u32 = 0;

/// `NV_CTXSHARE_ALLOCATION_PARAMETERS { hVASpace, flags = SYNC }`.
pub fn ctxshare_params(h_vaspace: u32) -> [u8; CTXSHARE_BYTES] {
    let mut a = [0u8; CTXSHARE_BYTES];
    put32(&mut a, 0, h_vaspace);
    put32(&mut a, 4, CTXSHARE_FLAGS_SYNC);
    a
}

/// What the GPFIFO channel is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelDesc {
    /// The memory holding the error notifier (at offset 0) and USERD.
    pub h_ctl: u32,
    /// GPU VA of the GPFIFO.
    pub gpfifo_va: u64,
    pub entries: u32,
    pub h_ctxshare: u32,
    pub userd_offset: u64,
    pub engine_type: u32,
}

/// `NV_CHANNEL_ALLOC_PARAMS` (376 bytes): `hObjectError` (the error notifier lives at offset 0
/// of that memory: the "error notifier setup" of the tool), `gpFifoOffset`, `gpFifoEntries`,
/// `hContextShare`, `hUserdMemory[0]` and `userdOffset[0]` (USERD in the same memory), and
/// `engineType`. Offsets 0, 8, 16, 24, 36, 72 and 136 (`offsetof` on the tool's struct).
pub fn channel_params(d: &ChannelDesc) -> [u8; CHANNEL_BYTES] {
    let mut a = [0u8; CHANNEL_BYTES];
    put32(&mut a, 0, d.h_ctl);
    put64(&mut a, 8, d.gpfifo_va);
    put32(&mut a, 16, d.entries);
    put32(&mut a, 24, d.h_ctxshare);
    put32(&mut a, 36, d.h_ctl);
    put64(&mut a, 72, d.userd_offset);
    put32(&mut a, 136, d.engine_type);
    a
}

/// `NVB0B5_ALLOCATION_PARAMETERS_VERSION_1`: `engineType` is an `NV2080_ENGINE_TYPE`
/// (`clb0b5sw.h`; version 0 would read it as a CE instance).
pub const CE_PARAMS_VERSION_1: u32 = 1;

/// `NVB0B5_ALLOCATION_PARAMETERS { VERSION_1, engineType }`.
pub fn ce_object_params(engine_type: u32) -> [u8; CE_OBJECT_BYTES] {
    let mut a = [0u8; CE_OBJECT_BYTES];
    put32(&mut a, 0, CE_PARAMS_VERSION_1);
    put32(&mut a, 4, engine_type);
    a
}

/// `NV_HOPPER_USERMODE_A_PARAMS { bBar1Mapping = 1, bPriv = 0 }`.
pub fn usermode_params() -> [u8; USERMODE_PARAM_BYTES] {
    [1, 0]
}

/// `NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS { bEnable }`.
pub fn schedule_params(enable: bool) -> [u8; SCHEDULE_BYTES] {
    [enable as u8, 0, 0]
}

/// `NVA06F_CTRL_BIND_PARAMS { engineType }`.
pub fn bind_params(engine_type: u32) -> [u8; U32_PARAM_BYTES] {
    engine_type.to_le_bytes()
}

/// `NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1`: the notifier index the tool (and NVK) gives the
/// work-submit token, one past the channel's own notifiers.
pub const TOKEN_NOTIF_INDEX: u32 = 3;

pub fn token_notif_index_params() -> [u8; U32_PARAM_BYTES] {
    TOKEN_NOTIF_INDEX.to_le_bytes()
}

/// `GET_WORK_SUBMIT_TOKEN`'s block, zero on the way in.
pub fn token_params() -> [u8; U32_PARAM_BYTES] {
    [0; U32_PARAM_BYTES]
}

/// The token of a `GET_WORK_SUBMIT_TOKEN` reply.
pub fn parse_token(reply: &[u8]) -> Option<cp::Token> {
    get32(reply, 0).map(cp::Token)
}

/// `CE_GET_CAPS_V2 { ceEngineType }`.
pub fn ce_caps_params(engine_type: u32) -> [u8; CE_CAPS_BYTES] {
    let mut a = [0u8; CE_CAPS_BYTES];
    put32(&mut a, 0, engine_type);
    a
}

// `NV_MEMORY_ALLOCATION_PARAMS` fields (`nv_ioctl_defs.h`).
const NVOS32_TYPE_IMAGE: u32 = 0;
const NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE: u32 = 0x0000_0010;
const NVOS32_ALLOC_FLAGS_VIRTUAL: u32 = 0x0008_0000;

/// `NV_MEMORY_ALLOCATION_PARAMS` of the `NV50_MEMORY_VIRTUAL` that `crm_map_dma2` carves out of
/// the VA space for one mapping: owner, type IMAGE, `VIRTUAL`, the size, `hVASpace`, and with a
/// fixed address `FIXED_ADDRESS_ALLOCATE` plus `offset`.
pub fn virtual_params(root: u32, h_vaspace: u32, va: Option<u64>, size: u64) -> [u8; MEM_ALLOC_BYTES] {
    let mut a = [0u8; MEM_ALLOC_BYTES];
    put32(&mut a, 0, root);
    put32(&mut a, 4, NVOS32_TYPE_IMAGE);
    let mut flags = NVOS32_ALLOC_FLAGS_VIRTUAL;
    if let Some(v) = va {
        flags |= NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE;
        put64(&mut a, 80, v);
    }
    put32(&mut a, 8, flags);
    put64(&mut a, 64, size);
    put32(&mut a, 108, h_vaspace);
    a
}

/// `NVOS46_FLAGS_CACHE_SNOOP_ENABLE` (4:4) and `NVOS46_FLAGS_PAGE_SIZE_4KB` (11:8 = 1): how NVK
/// and the tool map system memory.
pub const MAP_FLAGS_SYSMEM: u32 = (1 << 4) | (1 << 8);

/// One GPU mapping (`NVOS46_PARAMETERS`): `h_memory` into the virtual allocation `h_dma`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaMap {
    pub root: u32,
    pub h_device: u32,
    pub h_dma: u32,
    pub h_memory: u32,
    pub length: u64,
    pub flags: u32,
}

/// `NVOS46_PARAMETERS` as `rm_map_dma` fills it: offset 0, `flags2 = 0`, no kind, `dmaOffset = 0`
/// (relative to the virtual allocation; RM answers the GPU VA there).
pub fn nvos46(m: &DmaMap) -> [u8; NVOS46_BYTES] {
    let mut a = [0u8; NVOS46_BYTES];
    put32(&mut a, 0, m.root);
    put32(&mut a, 4, m.h_device);
    put32(&mut a, 8, m.h_dma);
    put32(&mut a, 12, m.h_memory);
    put64(&mut a, 24, m.length);
    put32(&mut a, 32, m.flags);
    a
}

/// The GPU VA of an answered `MAP_MEMORY_DMA` (`dmaOffset`).
pub fn map_dma_va(reply_data: &[u8]) -> Option<u64> {
    get64(reply_data, NVOS46_DMA_OFFSET_AT)
}

/// `NVOS47_PARAMETERS` as `rm_unmap_dma` fills it: the whole mapping at `va`.
pub fn nvos47(m: &DmaMap, va: u64) -> [u8; NVOS47_BYTES] {
    let mut a = [0u8; NVOS47_BYTES];
    put32(&mut a, 0, m.root);
    put32(&mut a, 4, m.h_device);
    put32(&mut a, 8, m.h_dma);
    put32(&mut a, 12, m.h_memory);
    put64(&mut a, 24, va);
    a
}

/// `NvNotification` (16 bytes): `status` at 14, `info32` at 8. The channel's error notifier is the
/// first one of the control memory; nonzero `status` is an RC error.
pub const NOTIFIER_STATUS_AT: u64 = 14;
pub const NOTIFIER_INFO32_AT: u64 = 8;

// ── the channel's memory ─────────────────────────────────────────────────────────────────────

/// GPFIFO entries (the tool's 128) and one 512-byte push slot per entry.
pub const GPFIFO_ENTRIES: u32 = 128;
pub const SLOT_BYTES: u32 = 512;
pub const SLOT_DWORDS: usize = (SLOT_BYTES / 4) as usize;
/// The ring allocation (RM system memory, CPU-mapped through the RM window, GPU-mapped snooped):
/// the GPFIFO at 0, the semaphore page at 4096 (the completion value, the self-test's producer
/// value), the push slots from 8192.
pub const RING_BYTES: u64 = 128 * 1024;
pub const GPFIFO_OFFSET: u64 = 0;
pub const COMPLETION_OFFSET: u64 = 4096;
/// The self-test's stand-in for a producer's semaphore (M3c acquires the producer's own).
pub const PRODUCER_OFFSET: u64 = 4096 + 64;
pub const PUSH_OFFSET: u64 = 8192;
/// The control allocation (RM system memory, CPU-mapped): the error notifier at 0, USERD at 4096
/// (`GP_PUT` at +0x8c), as the tool's.
pub const CTL_BYTES: u64 = 8192;
pub const NOTIFIER_OFFSET: u64 = 0;
pub const USERD_OFFSET: u64 = 4096;

const _: () = assert!(GPFIFO_OFFSET + GPFIFO_ENTRIES as u64 * cp::GP_ENTRY_BYTES <= COMPLETION_OFFSET);
const _: () = assert!(PRODUCER_OFFSET + 8 <= PUSH_OFFSET);
const _: () = assert!(PUSH_OFFSET + GPFIFO_ENTRIES as u64 * SLOT_BYTES as u64 <= RING_BYTES);
const _: () = assert!(SLOT_DWORDS >= cp::PRESENT_PUSH_MAX_DWORDS);
const _: () = assert!(USERD_OFFSET + cp::USERD_GP_PUT as u64 + 4 <= CTL_BYTES);

/// The fixed GPU VAs (the tool's base, 128 GiB: below 2^40 with room). Each mapping gets a
/// 64 MiB window, the ring first; the self-test's source and destination the next two.
pub const VA_BASE: u64 = 0x20_0000_0000;
pub const VA_WINDOW: u64 = 64 << 20;
pub const VA_RING: u64 = VA_BASE;
pub const VA_SELF_SRC: u64 = VA_BASE + VA_WINDOW;
pub const VA_SELF_DST: u64 = VA_BASE + 2 * VA_WINDOW;

const _: () = assert!(VA_SELF_DST + VA_WINDOW <= cp::MAX_VA);

/// GPU VA of push slot `index` of a ring mapped at `ring_va`.
pub const fn slot_va(ring_va: u64, index: u32) -> u64 {
    cp::Ring::slot_va(ring_va + PUSH_OFFSET, index, SLOT_BYTES)
}

/// Byte offset of push slot `index` in the ring allocation.
pub const fn slot_offset(index: u32) -> u64 {
    PUSH_OFFSET + index as u64 * SLOT_BYTES as u64
}

/// The RM handles of the channel's objects (its own client: no other namespace to avoid but the
/// device and subdevice it shares the numbering of `rm_client`).
pub const H_BASE: u32 = 0x4b4d_3000;
pub const H_VASPACE: u32 = H_BASE + 0x01;
pub const H_USERMODE: u32 = H_BASE + 0x02;
pub const H_CTL: u32 = H_BASE + 0x03;
pub const H_RING: u32 = H_BASE + 0x04;
pub const H_RING_VIRT: u32 = H_BASE + 0x05;
pub const H_TSG: u32 = H_BASE + 0x06;
pub const H_CTXSHARE: u32 = H_BASE + 0x07;
pub const H_CHANNEL: u32 = H_BASE + 0x08;
pub const H_CE: u32 = H_BASE + 0x09;
pub const H_SELF_SRC: u32 = H_BASE + 0x10;
pub const H_SELF_SRC_VIRT: u32 = H_BASE + 0x11;
pub const H_SELF_DST: u32 = H_BASE + 0x12;
pub const H_SELF_DST_VIRT: u32 = H_BASE + 0x13;

// ── the bring-up ─────────────────────────────────────────────────────────────────────────────

/// One stage of the bring-up, in the tool's order. The numbers are the `CeChStage` breadcrumb and
/// the top byte of `CeChFail`; they never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// The RM client: control file, version, card, root, GPU file, the device (the tool's
    /// parameters), subdevice, DRI listing, DRM file (the ring client's machine, `rm_client`).
    Client = 1,
    /// `GET_CLASSLIST_V2`: which generation's classes.
    ClassList = 2,
    /// `FERMI_VASPACE_A`, index `GPU_DEVICE`.
    VaSpace = 3,
    /// `GET_ENGINES_V2`, `CE_GET_CAPS_V2` per copy engine, [`pick_engine`].
    Engines = 4,
    /// `*_USERMODE_A` under the subdevice.
    Usermode = 5,
    /// Its CPU view (the doorbell).
    UsermodeMap = 6,
    /// 8 KiB RM system memory: error notifier, USERD.
    Ctl = 7,
    CtlMap = 8,
    /// 128 KiB RM system memory: GPFIFO, semaphores, push slots.
    Ring = 9,
    RingMap = 10,
    /// `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` of the ring at [`VA_RING`].
    RingGpuMap = 11,
    /// `KEPLER_CHANNEL_GROUP_A { engineType = COPY(n) }`.
    Group = 12,
    /// `FERMI_CONTEXT_SHARE_A` (SYNC).
    Subcontext = 13,
    /// The GPFIFO channel.
    Channel = 14,
    /// `BIND { COPY(n) }` before any engine object.
    Bind = 15,
    /// The copy object `{VERSION_1, COPY(n)}`.
    CeObject = 16,
    /// `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`.
    NotifIndex = 17,
    /// `GET_WORK_SUBMIT_TOKEN`.
    Token = 18,
    /// `GPFIFO_SCHEDULE { enable }` on the TSG.
    Schedule = 19,
    /// `SET_OBJECT` + a release of value 1, kicked, polled for [`FIRST_PUSH_MS`], and the error
    /// notifier 0: the channel is alive.
    FirstPush = 20,
}

/// The stages in order.
pub const STAGES: [Stage; 20] = [
    Stage::Client,
    Stage::ClassList,
    Stage::VaSpace,
    Stage::Engines,
    Stage::Usermode,
    Stage::UsermodeMap,
    Stage::Ctl,
    Stage::CtlMap,
    Stage::Ring,
    Stage::RingMap,
    Stage::RingGpuMap,
    Stage::Group,
    Stage::Subcontext,
    Stage::Channel,
    Stage::Bind,
    Stage::CeObject,
    Stage::NotifIndex,
    Stage::Token,
    Stage::Schedule,
    Stage::FirstPush,
];

/// What a bring-up has made so far: what its undo (or a teardown) must give back. One bit per
/// thing that needs its own undo; the channel group's children (subcontext, channel, copy
/// object) go with it, as in the tool (`chan_destroy`: "freeing the TSG frees the subcontext,
/// channel and CE object").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Made(u16);

impl Made {
    pub const NONE: Made = Made(0);
    pub const CLIENT: Made = Made(1 << 0);
    pub const VASPACE: Made = Made(1 << 1);
    pub const USERMODE: Made = Made(1 << 2);
    pub const USERMODE_MAP: Made = Made(1 << 3);
    pub const CTL: Made = Made(1 << 4);
    pub const CTL_MAP: Made = Made(1 << 5);
    pub const RING: Made = Made(1 << 6);
    pub const RING_MAP: Made = Made(1 << 7);
    pub const RING_GPU_MAP: Made = Made(1 << 8);
    pub const TSG: Made = Made(1 << 9);
    pub const SCHEDULED: Made = Made(1 << 10);

    pub const fn bits(self) -> u16 {
        self.0
    }
    pub const fn contains(self, m: Made) -> bool {
        self.0 & m.0 == m.0 && m.0 != 0
    }
    pub const fn with(self, m: Made) -> Made {
        Made(self.0 | m.0)
    }
    pub const fn without(self, m: Made) -> Made {
        Made(self.0 & !m.0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl Stage {
    /// What a successful stage made ([`Made::NONE`] for a query or a child of the TSG).
    pub const fn makes(self) -> Made {
        match self {
            Stage::Client => Made::CLIENT,
            Stage::VaSpace => Made::VASPACE,
            Stage::Usermode => Made::USERMODE,
            Stage::UsermodeMap => Made::USERMODE_MAP,
            Stage::Ctl => Made::CTL,
            Stage::CtlMap => Made::CTL_MAP,
            Stage::Ring => Made::RING,
            Stage::RingMap => Made::RING_MAP,
            Stage::RingGpuMap => Made::RING_GPU_MAP,
            Stage::Group => Made::TSG,
            Stage::Schedule => Made::SCHEDULED,
            Stage::ClassList
            | Stage::Engines
            | Stage::Subcontext
            | Stage::Channel
            | Stage::Bind
            | Stage::CeObject
            | Stage::NotifIndex
            | Stage::Token
            | Stage::FirstPush => Made::NONE,
        }
    }
}

/// One undo step, in the order [`next_undo`] hands them out: the tool's teardown order (schedule
/// off, free the TSG with its children, then the memory the channel referenced, the doorbell, the
/// VA space), and last the client (closing its control file frees whatever RM still holds of it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Undo {
    ScheduleOff = 1,
    FreeTsg = 2,
    /// `UNMAP_MEMORY_DMA` and the free of the `NV50_MEMORY_VIRTUAL`.
    UnmapRingGpu = 3,
    /// The kernel view, the host map, `RM_UNMAP_MEMORY`, the map channel.
    UnmapRingCpu = 4,
    FreeRing = 5,
    UnmapCtl = 6,
    FreeCtl = 7,
    UnmapUsermode = 8,
    FreeUsermode = 9,
    FreeVaSpace = 10,
    /// Close the client's files (the control file's close frees the RM client).
    CloseClient = 11,
}

impl Undo {
    pub const fn undoes(self) -> Made {
        match self {
            Undo::ScheduleOff => Made::SCHEDULED,
            Undo::FreeTsg => Made::TSG,
            Undo::UnmapRingGpu => Made::RING_GPU_MAP,
            Undo::UnmapRingCpu => Made::RING_MAP,
            Undo::FreeRing => Made::RING,
            Undo::UnmapCtl => Made::CTL_MAP,
            Undo::FreeCtl => Made::CTL,
            Undo::UnmapUsermode => Made::USERMODE_MAP,
            Undo::FreeUsermode => Made::USERMODE,
            Undo::FreeVaSpace => Made::VASPACE,
            Undo::CloseClient => Made::CLIENT,
        }
    }
}

const UNDO_ORDER: [Undo; 11] = [
    Undo::ScheduleOff,
    Undo::FreeTsg,
    Undo::UnmapRingGpu,
    Undo::UnmapRingCpu,
    Undo::FreeRing,
    Undo::UnmapCtl,
    Undo::FreeCtl,
    Undo::UnmapUsermode,
    Undo::FreeUsermode,
    Undo::FreeVaSpace,
    Undo::CloseClient,
];

/// The next thing to give back of `made`, or `None` when nothing is left. The caller clears the
/// bit whatever the step's outcome ([`Made::without`]): an undo is never retried; one that failed
/// is counted (`CeChSoft`), and what RM may still hold goes with the client's close (or with the
/// transport sweep, if even that fails).
pub fn next_undo(made: Made) -> Option<Undo> {
    UNDO_ORDER.iter().copied().find(|u| made.contains(u.undoes()))
}

/// A bring-up in progress: which stage is next, what is made, how it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BringUp {
    next: usize,
    made: Made,
    failed: Option<(Stage, Fail)>,
}

impl Default for BringUp {
    fn default() -> Self {
        Self::new()
    }
}

impl BringUp {
    pub const fn new() -> Self {
        BringUp { next: 0, made: Made::NONE, failed: None }
    }

    /// The stage to perform, `None` once every stage succeeded or one failed.
    pub fn next_stage(&self) -> Option<Stage> {
        if self.failed.is_some() {
            return None;
        }
        STAGES.get(self.next).copied()
    }

    /// Report `stage`; a report of any other stage than the next one is ignored.
    pub fn finish(&mut self, stage: Stage, result: Result<(), Fail>) {
        if self.next_stage() != Some(stage) {
            return;
        }
        match result {
            Ok(()) => {
                self.made = self.made.with(stage.makes());
                self.next += 1;
            }
            Err(f) => self.failed = Some((stage, f)),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.failed.is_none() && self.next == STAGES.len()
    }

    pub fn failure(&self) -> Option<(Stage, Fail)> {
        self.failed
    }

    pub fn made(&self) -> Made {
        self.made
    }
}

/// `stage << 24 | kind << 16 | code & 0xffff`: the `CeChFail` word (as `RmFail` / `RmSysFail`).
pub fn pack_failure(stage: u8, f: Fail) -> u32 {
    ((stage as u32) << 24) | ((f.kind as u32) << 16) | (f.code & 0xffff)
}

// ── time ─────────────────────────────────────────────────────────────────────────────────────

/// The whole bring-up, every message and the first push's poll included: one deadline (the
/// sysmem service's creation budget; each message waits at most what is left, none is sent once
/// it is spent).
pub const BRING_UP_BUDGET_MS: u64 = crate::rm_sysmem::CREATE_BUDGET_MS;
/// The undo of a failed bring-up, and a teardown outside StopDevice, on their own allowance.
pub const UNDO_BUDGET_MS: u64 = crate::rm_sysmem::UNDO_BUDGET_MS;
/// How long the first push may take to land (a release with nothing before it: microseconds).
pub const FIRST_PUSH_MS: u64 = 250;
/// How long a teardown waits for the GPU to finish what was submitted before it frees the memory
/// the GPU may still read or write (after releasing every acquire the channel could wait on).
pub const IDLE_WAIT_MS: u64 = 250;
/// After a failed bring-up, no new one for this long (a busy host is not hammered).
pub const RETRY_AFTER_MS: u64 = 2_000;
/// Failed bring-ups (or channel failures) in a row after which the subsystem is off for the
/// transport generation.
pub const MAX_STRIKES: u8 = 3;

// ── the service ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing made.
    Cold,
    /// One thread is bringing the channel up.
    BringingUp,
    /// Up: submissions go.
    Ready,
    /// The error notifier was set (or a submission never completed): no submission goes; the
    /// worker tears it down.
    Broken,
    /// A teardown is running.
    TearingDown,
    /// A bring-up failed or a broken channel was torn down: none for [`RETRY_AFTER_MS`].
    CoolDown,
    /// [`MAX_STRIKES`] in a row: off for the generation.
    Disabled,
}

impl Phase {
    pub const fn code(self) -> u32 {
        match self {
            Phase::Cold => 0,
            Phase::BringingUp => 1,
            Phase::Ready => 2,
            Phase::Broken => 3,
            Phase::TearingDown => 4,
            Phase::CoolDown => 5,
            Phase::Disabled => 6,
        }
    }
}

/// Why the channel is not there for a caller. The codes never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Why {
    /// Another thread is bringing it up or tearing it down.
    Busy = 1,
    /// A failure is cooling down.
    CoolDown = 2,
    /// Struck out for the generation.
    Disabled = 3,
    /// The error notifier is set; a teardown is due.
    Broken = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// The caller brings it up and reports with [`Svc::bring_up_done`].
    BringUp,
    /// It is up.
    Ready,
    Refuse(Why),
}

/// The service: plain data under one leaf spinlock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Svc {
    phase: Phase,
    epoch: u64,
    strikes: u8,
    since_ms: u64,
    /// The teardown in progress follows a failure (a strike was counted when it broke).
    broken: bool,
}

impl Default for Svc {
    fn default() -> Self {
        Self::new()
    }
}

impl Svc {
    pub const fn new() -> Self {
        Svc { phase: Phase::Cold, epoch: 0, strikes: 0, since_ms: 0, broken: false }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn strikes(&self) -> u8 {
        self.strikes
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// `phase << 28 | strikes << 24`: the high byte of `CeChState`.
    pub fn state_word(&self) -> u32 {
        (self.phase.code() << 28) | (u32::from(self.strikes.min(15)) << 24)
    }

    /// Forget everything (the transport generation ended: the sweep closed the host side).
    pub fn reset(&mut self) {
        *self = Svc::new();
    }

    /// Reconcile with transport generation `epoch`: another one starts from cold.
    pub fn sync_epoch(&mut self, epoch: u64) {
        if epoch != self.epoch {
            self.reset();
            self.epoch = epoch;
        }
    }

    /// A user of the channel asks for it at `now_ms`.
    pub fn admit(&mut self, epoch: u64, now_ms: u64) -> Admit {
        self.sync_epoch(epoch);
        match self.phase {
            Phase::Ready => Admit::Ready,
            Phase::Cold => {
                self.phase = Phase::BringingUp;
                Admit::BringUp
            }
            Phase::CoolDown if now_ms.saturating_sub(self.since_ms) >= RETRY_AFTER_MS => {
                self.phase = Phase::BringingUp;
                Admit::BringUp
            }
            Phase::CoolDown => Admit::Refuse(Why::CoolDown),
            Phase::BringingUp | Phase::TearingDown => Admit::Refuse(Why::Busy),
            Phase::Broken => Admit::Refuse(Why::Broken),
            Phase::Disabled => Admit::Refuse(Why::Disabled),
        }
    }

    fn strike(&mut self, now_ms: u64) {
        self.strikes = self.strikes.saturating_add(1);
        self.since_ms = now_ms;
        self.phase = if self.strikes >= MAX_STRIKES { Phase::Disabled } else { Phase::CoolDown };
    }

    /// The answer to [`Admit::BringUp`]. A success clears the strikes (as a sysmem creation's
    /// does); a failure (already undone by the caller) is a strike.
    pub fn bring_up_done(&mut self, ok: bool, now_ms: u64) {
        if self.phase != Phase::BringingUp {
            return;
        }
        if ok {
            self.phase = Phase::Ready;
            self.strikes = 0;
        } else {
            self.strike(now_ms);
        }
    }

    /// The channel failed while up (error notifier, a push that never landed): a strike is due
    /// once it is torn down; nothing is submitted until then.
    pub fn on_channel_error(&mut self) {
        if self.phase == Phase::Ready {
            self.phase = Phase::Broken;
            self.broken = true;
        }
    }

    /// Take the teardown duty: `true` for a channel that is up or broken.
    pub fn begin_teardown(&mut self) -> bool {
        match self.phase {
            Phase::Ready | Phase::Broken => {
                self.phase = Phase::TearingDown;
                true
            }
            _ => false,
        }
    }

    /// The teardown is over: cold again after a clean one, a strike after a broken channel's.
    pub fn torn_down(&mut self, now_ms: u64) {
        if self.phase != Phase::TearingDown {
            return;
        }
        if core::mem::take(&mut self.broken) {
            self.strike(now_ms);
        } else {
            self.phase = Phase::Cold;
        }
    }
}

// ── CPU views: which map channel, which cache attribute ──────────────────────────────────────

/// The kind of file a CPU view's `RM_MAP_MEMORY` is armed on (librmclient's `map_node_hint`,
/// `transport_windows.c` `win_map_memory`): system memory maps on a fresh CONTROL file (device
/// type 255, no `REGISTER_FD`), BAR memory (video memory, the usermode doorbell) on a fresh GPU
/// file (the minor, tied to the control file with `REGISTER_FD`). The wrong kind is RM's
/// `NV_ERR_INVALID_ARGUMENT`, after which librmclient tries the other kind once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapNode {
    Ctl,
    Gpu,
}

impl MapNode {
    /// The kind for memory of `class`.
    pub const fn for_class(class: u32) -> MapNode {
        match class {
            NV01_MEMORY_SYSTEM | NV01_MEMORY_SYSTEM_OS_DESCRIPTOR => MapNode::Ctl,
            _ => MapNode::Gpu,
        }
    }

    pub const fn other(self) -> MapNode {
        match self {
            MapNode::Ctl => MapNode::Gpu,
            MapNode::Gpu => MapNode::Ctl,
        }
    }
}

/// `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`.
pub const NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: u32 = 0x71;
/// `NV_ERR_INVALID_ARGUMENT`: what RM answers a map armed on the wrong kind of file.
pub const NV_ERR_INVALID_ARGUMENT: u32 = 0x1f;

/// May a failed `RM_MAP_MEMORY` be retried on the other kind of file (once)?
pub fn retry_other_node(f: Fail, already_retried: bool) -> bool {
    !already_retried && f.kind == FailKind::Rm && f.code == NV_ERR_INVALID_ARGUMENT
}

/// `RmCeCache`: what the channel's own RM system memory (control, ring, the self-test's buffers)
/// is made of. 0 (default, and any other value): cached, as the tool allocates it
/// (`NVOS32_ATTR_COHERENCY_CACHED`); 1: write-combined (M3b's first choice, for an A/B). Private
/// channel memory has no dxgkrnl view, so the level-5 alias concern does not apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    Cached,
    WriteCombine,
}

impl CacheMode {
    pub const fn from_knob(v: u32) -> CacheMode {
        match v {
            1 => CacheMode::WriteCombine,
            _ => CacheMode::Cached,
        }
    }

    /// What RM is asked for.
    pub const fn sysmem(self) -> crate::rm_sysmem::Cache {
        match self {
            CacheMode::Cached => crate::rm_sysmem::Cache::Cached,
            CacheMode::WriteCombine => crate::rm_sysmem::Cache::WriteCombine,
        }
    }

    /// `CeCache`: the value in force.
    pub const fn word(self) -> u32 {
        match self {
            CacheMode::Cached => 0,
            CacheMode::WriteCombine => 1,
        }
    }
}

// ── the failing RM call ──────────────────────────────────────────────────────────────────────

/// `CeRmCall`: the last failing RM call of the channel, `esc << 24 | what & 0xff_ffff`. `esc` is
/// the escape number (`0x2b` ALLOC, `0x2a` CONTROL, `0x29` FREE, `0x4e` MAP_MEMORY, `0x4f`
/// UNMAP_MEMORY, `0x57` / `0x58` MAP / UNMAP_MEMORY_DMA); `what` the class for an ALLOC, the
/// command's low 24 bits for a CONTROL (`0x2080_0170` -> `0x80_0170`), the object handle's low 24
/// bits for the others (`0x4b4d_3003` -> `0x4d_3003`).
pub const fn rm_call_word(esc: u32, what: u32) -> u32 {
    ((esc & 0xff) << 24) | (what & 0x00ff_ffff)
}

/// `CeRmStat` (and `CeSelfWhy`): RM's `NV_STATUS` as it is; any other failure as
/// `0x8000_0000 | kind << 16 | code & 0xffff`.
pub fn fail_word(f: Fail) -> u32 {
    if f.kind == FailKind::Rm {
        f.code
    } else {
        0x8000_0000 | ((f.kind as u32) << 16) | (f.code & 0xffff)
    }
}

/// `CeMapNode` of a failed CPU view: `node << 28 | map_ch & 0x0fff_ffff` (node 1 control file,
/// 2 GPU file), so a dump names the file the map was armed on.
pub const fn map_node_word(node: MapNode, map_ch: u32) -> u32 {
    let n = match node {
        MapNode::Ctl => 1,
        MapNode::Gpu => 2,
    };
    (n << 28) | (map_ch & 0x0fff_ffff)
}

// ── the knob ─────────────────────────────────────────────────────────────────────────────────

/// `RmCopyEngine` (`ce_present::KNOB`).
pub const KNOB: &str = cp::KNOB;

/// What the knob asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0 (default), or any value not listed: nothing happens, nothing is allocated.
    Off,
    /// 1: reserved for the Present route (M3c); in M3b nothing happens.
    Route,
    /// 2: the hardware self-test, once per transport generation, from the HPD worker.
    SelfTest,
}

pub const fn mode(knob: u32) -> Mode {
    match knob {
        1 => Mode::Route,
        2 => Mode::SelfTest,
        _ => Mode::Off,
    }
}

/// The value mirrored as `CeKnob`: the value in force (an unknown one is 0).
pub const fn knob_in_force(knob: u32) -> u32 {
    match mode(knob) {
        Mode::Off => 0,
        Mode::Route => 1,
        Mode::SelfTest => 2,
    }
}

/// `CeChan`: 0 no channel, 1 alive, 2 dead (broken, cooling down or struck out).
pub const fn chan_word(phase: Phase) -> u32 {
    match phase {
        Phase::Ready => 1,
        Phase::Broken | Phase::CoolDown | Phase::Disabled => 2,
        Phase::Cold | Phase::BringingUp | Phase::TearingDown => 0,
    }
}

// ── the self-test ────────────────────────────────────────────────────────────────────────────

/// `RmCopyEngine` = 2: one bounded copy-engine self-test per transport generation, from the HPD
/// worker at PASSIVE, never inside a DDI. Two pitch-linear 1600x900x4 copies from a source to a
/// destination, both in the KMD client's own RM system memory:
///
/// 1. **ready**: the producer value is set BEFORE the kick; the copy is the source word for
///    word. `CeSelfUs` = kick to completion seen.
/// 2. **wait**: the push acquires a value the producer does not have yet; after the kick the
///    worker sleeps [`HOLD_MS`] (a timer tick in practice), checks the completion has NOT landed
///    (the GPU waits), then sets the producer. The copy reads the source one word further on, so
///    its result differs from the first copy's at every word. `CeSelfWaitUs` = producer set to
///    completion seen.
///
/// Each destination is compared word for word with the pattern. The verdict is `CeSelfTest`: 1
/// pass, `0xE0 + stage` on failure ([`Stage`]), with `CeSelfWhy` the RM status of the failing
/// call when there was one ([`why_word`]).
pub mod selftest {
    use super::Fail;
    use crate::ce_present::{CopyRect, Remap, SurfaceLayout};

    pub const WIDTH: u32 = 1600;
    pub const HEIGHT: u32 = 900;
    pub const PITCH: u32 = WIDTH * 4;
    pub const COPY_BYTES: u64 = PITCH as u64 * HEIGHT as u64;
    pub const COPY_WORDS: u32 = (COPY_BYTES / 4) as u32;
    /// The source holds one word more than a copy reads: the wait copy reads from word 1.
    pub const SRC_BYTES: u64 = crate::round_up_page(COPY_BYTES + 4);
    pub const DST_BYTES: u64 = crate::round_up_page(COPY_BYTES);
    /// 4 KiB pages the destination covers (`CeSelfPages`; the tool's `precondition:` line says
    /// 1407 for 1600x900).
    pub const PAGES: u32 = ((COPY_BYTES + 4095) / 4096) as u32;

    /// Wait for the ready copy, and for the wait copy after its release.
    pub const COPY_WAIT_MS: u64 = 250;
    /// The delay before the producer value of the wait copy is set (a `sleep_ms`: a timer tick,
    /// about 15.6 ms, in practice).
    pub const HOLD_MS: u64 = 2;
    /// The whole self-test after the channel is up (allocations, fills, copies, verification,
    /// frees): a stage that would start after it is spent fails with [`Stage::NoTime`].
    pub const BUDGET_MS: u64 = 4_000;

    /// The producer values the two copies acquire (the semaphore starts at 0).
    pub const PRODUCER_READY: u64 = 1;
    pub const PRODUCER_WAIT: u64 = 2;

    /// The verdict codes: `CeSelfTest` is [`PASS`] or `0xE0 + stage`. They never change.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Stage {
        /// The channel's bring-up failed (`CeChFail` says where).
        BringUp = 1,
        /// The source's RM memory, its CPU view or its GPU mapping.
        Source = 2,
        Destination = 3,
        /// The push did not build (a VA or shape refusal; never expected).
        Push = 4,
        /// No free GPFIFO entry (never expected: the ring is idle).
        RingFull = 5,
        /// The ready copy did not complete within [`COPY_WAIT_MS`].
        ReadyTimeout = 6,
        /// The ready copy's destination is not the pattern.
        ReadyBad = 7,
        /// The wait copy completed BEFORE its producer value was set: the acquire did not hold.
        NotHeld = 8,
        WaitTimeout = 9,
        WaitBad = 10,
        /// The channel's error notifier is set.
        ChannelError = 11,
        /// [`BUDGET_MS`] was spent before a stage could start.
        NoTime = 12,
    }

    pub const PASS: u32 = 1;

    pub const fn fail_word(s: Stage) -> u32 {
        0xE0 + s as u32
    }

    /// `CeSelfWhy`: RM's `NV_STATUS` of the failing call as it is; any other failure as
    /// `0x8000_0000 | kind << 16 | code & 0xffff`; 0 for a failure that had no call (a mismatch,
    /// a timeout).
    pub fn why_word(f: Option<Fail>) -> u32 {
        f.map_or(0, super::fail_word)
    }

    /// The source's word `i` (the tool's position-dependent pattern, with a per-run salt so a
    /// stale buffer of an earlier run never passes).
    pub const fn pattern(i: u32, salt: u32) -> u32 {
        i.wrapping_mul(2_654_435_761) ^ salt
    }

    /// Which of the two copies.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Copy {
        Ready,
        Wait,
    }

    impl Copy {
        /// Source words the copy skips.
        pub const fn shift_words(self) -> u32 {
            match self {
                Copy::Ready => 0,
                Copy::Wait => 1,
            }
        }

        /// What destination word `i` must hold after this copy.
        pub const fn expected(self, i: u32, salt: u32) -> u32 {
            pattern(i + self.shift_words(), salt)
        }
    }

    /// The copy of `which` from a source mapped at `src_va` into a destination at `dst_va`.
    pub const fn copy_rect(which: Copy, src_va: u64, dst_va: u64) -> CopyRect {
        CopyRect {
            src_va: src_va + 4 * which.shift_words() as u64,
            dst_va,
            src_pitch: PITCH,
            dst_pitch: PITCH,
            line_bytes: PITCH,
            lines: HEIGHT,
            layout: SurfaceLayout::Pitch,
            dst_layout: SurfaceLayout::Pitch,
            remap: Remap::None,
            stamp: None,
        }
    }

    /// Compare `words` destination words, read by `read(i)`, with what `which` must have
    /// written. `Err((index, got))` of the first mismatch.
    pub fn verify(
        which: Copy,
        salt: u32,
        words: u32,
        mut read: impl FnMut(u32) -> u32,
    ) -> Result<(), (u32, u32)> {
        for i in 0..words {
            let got = read(i);
            if got != which.expected(i, salt) {
                return Err((i, got));
            }
        }
        Ok(())
    }

    /// Microseconds from two 100 ns stamps, saturated to the counter.
    pub const fn us_between(from_100ns: u64, to_100ns: u64) -> u32 {
        let us = to_100ns.saturating_sub(from_100ns) / 10;
        if us > u32::MAX as u64 {
            u32::MAX
        } else {
            us as u32
        }
    }
}

// ── counters ─────────────────────────────────────────────────────────────────────────────────

/// The counters M3b writes, all in `kmd_render/src/virtio/rm_client/ce_channel.rs` (the
/// self-test, `ce_selftest.rs`, stores its results in that file's atomics). At most 14
/// characters, prefix `Ce`, unique across `kmd_render` and `kmd_logic`. The first five are
/// `ce_present::COUNTERS`' channel names (that list is the route's whole set; its other names are
/// M3c's and still unwritten). A test below checks this list against the I/O file, both ways.
pub const COUNTERS: &[&str] = &[
    // From `ce_present::COUNTERS`: the knob in force; the channel's state (0 none, 1 alive,
    // 2 dead) and its runlist; RM calls of the channel that failed; channels that failed while up.
    "CeKnob",
    "CeChan",
    "CeRunlist",
    "CeRmErr",
    "CeChanFail",
    // Bring-ups started / that reached Ready; teardowns done; the stage started last (written
    // BEFORE it runs); the last failure (`stage << 24 | kind << 16 | code`); the service word
    // (`phase << 28 | strikes << 24 | made bits`); undo steps that failed; the last bring-up's ms.
    "CeChTry",
    "CeChUp",
    "CeChDown",
    "CeChStage",
    "CeChFail",
    "CeChState",
    "CeChSoft",
    "CeChMs",
    // The class generation (1 GB20x, 2 Ada), the engine type and its caps word, the token, the
    // error notifier's last nonzero status, pushes submitted.
    "CeGen",
    "CeEngine",
    "CeCaps",
    "CeToken",
    "CeNotify",
    "CeSubmit",
    // The self-test (`RmCopyEngine` = 2): the verdict, the RM status of a failing call, kick to
    // completion (acquire already satisfied), producer set to completion, the destination's
    // pages, the whole self-test's ms.
    "CeSelfTest",
    "CeSelfWhy",
    "CeSelfUs",
    "CeSelfWaitUs",
    "CeSelfPages",
    "CeSelfMs",
    // The channel memory's cache attribute in force (`RmCeCache`: 0 cached, 1 write-combined);
    // the last failing RM call (`rm_call_word`), its status (`fail_word`) and, for a CPU view, the
    // file it was armed on (`map_node_word`).
    "CeCache",
    "CeRmCall",
    "CeRmStat",
    "CeMapNode",
];

/// `CeGen`.
pub const fn gen_word(gen: Gen) -> u32 {
    match gen {
        Gen::Gb202 => 1,
        Gen::Ada => 2,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    // ── the tool's blocks ─────────────────────────────────────────────────────────────────────
    //
    // Every nonzero byte the tool's fill code writes, for these inputs (see the module docs for
    // how they were printed): root 0xc1d00042, VA space 0x4b4d3001, control memory 0x4b4d3003,
    // subcontext 0x4b4d3007, engine COPY2 (0x0b), ring VA 0x20_0000_0000, 128 entries.

    const ROOT: u32 = 0xc1d0_0042;
    const COPY2: u32 = 0x0b;

    fn sparse(block: &[u8]) -> Vec<(usize, u8)> {
        block.iter().enumerate().filter(|(_, b)| **b != 0).map(|(i, b)| (i, *b)).collect()
    }

    #[test]
    fn the_device_is_the_tools_block() {
        let b = device_params(ROOT);
        assert_eq!(b.len(), 56);
        assert_eq!(sparse(&b), [(4, 0x42), (6, 0xd0), (7, 0xc1), (17, 0x02)]);
    }

    #[test]
    fn the_vaspace_is_the_tools_block() {
        let b = vaspace_params();
        assert_eq!(b.len(), 56);
        assert_eq!(sparse(&b), [(0, 0x03)]);
    }

    #[test]
    fn the_channel_group_is_the_tools_block() {
        let b = channel_group_params(0x4b4d_3001, COPY2);
        assert_eq!(b.len(), 20);
        assert_eq!(sparse(&b), [(8, 0x01), (9, 0x30), (10, 0x4d), (11, 0x4b), (12, 0x0b)]);
        assert_eq!(H_VASPACE, 0x4b4d_3001);
    }

    #[test]
    fn the_subcontext_is_the_tools_block() {
        let b = ctxshare_params(0x4b4d_3001);
        assert_eq!(b.len(), 12);
        assert_eq!(sparse(&b), [(0, 0x01), (1, 0x30), (2, 0x4d), (3, 0x4b)]);
    }

    #[test]
    fn the_channel_is_the_tools_block() {
        let b = channel_params(&ChannelDesc {
            h_ctl: 0x4b4d_3003,
            gpfifo_va: 0x20_0000_0000,
            entries: 128,
            h_ctxshare: 0x4b4d_3007,
            userd_offset: 4096,
            engine_type: COPY2,
        });
        assert_eq!(b.len(), 376);
        assert_eq!(
            sparse(&b),
            [
                (0, 0x03), (1, 0x30), (2, 0x4d), (3, 0x4b),
                (12, 0x20),
                (16, 0x80),
                (24, 0x07), (25, 0x30), (26, 0x4d), (27, 0x4b),
                (36, 0x03), (37, 0x30), (38, 0x4d), (39, 0x4b),
                (73, 0x10),
                (136, 0x0b),
            ]
        );
        assert_eq!((H_CTL, H_CTXSHARE), (0x4b4d_3003, 0x4b4d_3007));
    }

    #[test]
    fn the_small_blocks_are_the_tools() {
        assert_eq!(sparse(&bind_params(COPY2)), [(0, 0x0b)]);
        assert_eq!(sparse(&ce_object_params(COPY2)), [(0, 0x01), (4, 0x0b)]);
        assert_eq!(sparse(&token_notif_index_params()), [(0, 0x03)]);
        assert_eq!(schedule_params(true), [1, 0, 0]);
        assert_eq!(schedule_params(false), [0, 0, 0]);
        assert_eq!(usermode_params(), [1, 0]);
        assert_eq!(sparse(&ce_caps_params(COPY2)), [(0, 0x0b)]);
        assert_eq!(ce_caps_params(COPY2).len(), 8);
        assert_eq!(token_params(), [0; 4]);
    }

    #[test]
    fn the_virtual_allocation_is_crm_map_dma2s_block() {
        let b = virtual_params(ROOT, 0x4b4d_3001, Some(0x20_0000_0000), 131072);
        assert_eq!(b.len(), 128);
        assert_eq!(
            sparse(&b),
            [
                (0, 0x42), (2, 0xd0), (3, 0xc1),
                (8, 0x10), (10, 0x08),
                (66, 0x02),
                (84, 0x20),
                (108, 0x01), (109, 0x30), (110, 0x4d), (111, 0x4b),
            ]
        );
        // RM's choice of address: neither the flag nor the offset.
        let free = virtual_params(ROOT, 0x4b4d_3001, None, 131072);
        assert_eq!(get32(&free, 8), Some(0x0008_0000));
        assert_eq!(get64(&free, 80), Some(0));
    }

    fn ring_map() -> DmaMap {
        DmaMap {
            root: ROOT,
            h_device: crate::rm_client::H_DEVICE,
            h_dma: 0x4b4d_3005,
            h_memory: 0x4b4d_3004,
            length: 131072,
            flags: MAP_FLAGS_SYSMEM,
        }
    }

    #[test]
    fn the_dma_map_and_unmap_are_rm_map_dmas_blocks() {
        let m = ring_map();
        assert_eq!((H_RING, H_RING_VIRT), (m.h_memory, m.h_dma));
        let b = nvos46(&m);
        assert_eq!(b.len(), 64);
        assert_eq!(
            sparse(&b),
            [
                (0, 0x42), (2, 0xd0), (3, 0xc1),
                (4, 0x01), (6, 0x4d), (7, 0x4b),
                (8, 0x05), (9, 0x30), (10, 0x4d), (11, 0x4b),
                (12, 0x04), (13, 0x30), (14, 0x4d), (15, 0x4b),
                (26, 0x02),
                (32, 0x10), (33, 0x01),
            ]
        );
        let u = nvos47(&m, 0x20_0000_0000);
        assert_eq!(u.len(), 48);
        assert_eq!(
            sparse(&u),
            [
                (0, 0x42), (2, 0xd0), (3, 0xc1),
                (4, 0x01), (6, 0x4d), (7, 0x4b),
                (8, 0x05), (9, 0x30), (10, 0x4d), (11, 0x4b),
                (12, 0x04), (13, 0x30), (14, 0x4d), (15, 0x4b),
                (28, 0x20),
            ]
        );
        // The reply's `dmaOffset` and `status`.
        let mut reply = [0u8; NVOS46_BYTES];
        reply[48..56].copy_from_slice(&0x20_0000_0000u64.to_le_bytes());
        assert_eq!(map_dma_va(&reply), Some(0x20_0000_0000));
        assert_eq!((NVOS46_STATUS_AT, NVOS47_STATUS_AT), (56, 40));
    }

    #[test]
    fn controls_and_sizes_are_the_allowlists() {
        // `v610_57_04.rs` rows 143, 268, 470, 638, 646, 730, 731 and the class rows.
        assert_eq!((CTRL_GET_CLASSLIST_V2, CLASSLIST_BYTES), (0x0080_0292, 804));
        assert_eq!((CTRL_GET_ENGINES_V2, ENGINES_BYTES), (0x2080_0170, 340));
        assert_eq!((CTRL_CE_GET_CAPS_V2, CE_CAPS_BYTES), (0x2080_2a03, 8));
        assert_eq!((CTRL_GPFIFO_SCHEDULE, SCHEDULE_BYTES), (0xa06c_0101, 3));
        assert_eq!((CTRL_BIND, U32_PARAM_BYTES), (0xa06f_0104, 4));
        assert_eq!(CTRL_GET_WORK_SUBMIT_TOKEN, 0xc36f_0108);
        assert_eq!(CTRL_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX, 0xc36f_010a);
        assert_eq!(4 + 4 * CLASSLIST_MAX, CLASSLIST_BYTES);
        assert_eq!(4 + 4 * ENGINES_MAX, ENGINES_BYTES);
        assert_eq!(
            (VASPACE_BYTES, TSG_BYTES, CTXSHARE_BYTES, CHANNEL_BYTES, CE_OBJECT_BYTES),
            (56, 20, 12, 376, 8)
        );
        assert_eq!(USERMODE_PARAM_BYTES, 2);
        assert!(usermode_takes_params(0xc761));
        assert!(!usermode_takes_params(0xc561));
    }

    // ── engines and classes ──────────────────────────────────────────────────────────────────

    #[test]
    fn copy_engine_types_round_trip() {
        assert_eq!(copy_engine(0), Some(0x09));
        assert_eq!(copy_engine(9), Some(0x12));
        assert_eq!(copy_engine(10), Some(0x34));
        assert_eq!(copy_engine(19), Some(0x3d));
        assert_eq!(copy_engine(20), None);
        for i in 0..MAX_COPY_ENGINES {
            assert_eq!(copy_index(copy_engine(i).unwrap()), Some(i));
        }
        assert_eq!(copy_index(ENGINE_GRAPHICS), None);
        assert_eq!(copy_index(0x13), None);
        assert_eq!(copy_index(0x33), None);
    }

    fn caps(engine_type: u32, b0: u8) -> CeCaps {
        CeCaps { engine_type, caps: [b0, 0] }
    }

    #[test]
    fn the_engine_pick_prefers_an_unshared_async_ce_that_writes_sysmem() {
        let grce = caps(0x09, CAPS_GRCE | CAPS_SYSMEM_WRITE);
        let shared = caps(0x0a, CAPS_SHARED | CAPS_SYSMEM_WRITE);
        let plain = caps(0x0b, CAPS_SYSMEM_WRITE);
        let no_sysmem = caps(0x0c, 0);
        assert_eq!(pick_engine(&[grce, shared, plain, no_sysmem]), Some(plain));
        assert_eq!(pick_engine(&[grce, no_sysmem, shared]), Some(shared));
        // The tool's rule last: the first async one.
        assert_eq!(pick_engine(&[grce, no_sysmem]), Some(no_sysmem));
        assert_eq!(pick_engine(&[grce]), None);
        assert_eq!(pick_engine(&[]), None);
        assert_eq!(caps(0x0b, 0x09).word(), 0x09);
    }

    #[test]
    fn the_engine_list_keeps_the_copy_engines_in_order() {
        let mut reply = [0u8; ENGINES_BYTES];
        let list = [ENGINE_GRAPHICS, 0x0b, 0x09, 0x0b, 0x34, 0x13];
        reply[..4].copy_from_slice(&(list.len() as u32).to_le_bytes());
        for (i, t) in list.iter().enumerate() {
            reply[4 + 4 * i..8 + 4 * i].copy_from_slice(&t.to_le_bytes());
        }
        let mut out = [0u32; MAX_CAPS_QUERIES];
        let n = copy_engines(&reply, &mut out);
        assert_eq!(&out[..n], &[0x0b, 0x09, 0x34]);
        // A count past the list is clamped, a short reply ends it.
        reply[..4].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(copy_engines(&reply, &mut out), 3);
        assert_eq!(copy_engines(&reply[..12], &mut out), 1);
    }

    #[test]
    fn a_caps_reply_must_name_its_engine() {
        let mut r = [0u8; 8];
        r[..4].copy_from_slice(&0x0bu32.to_le_bytes());
        r[4] = 0x08;
        r[5] = 0x01;
        assert_eq!(parse_ce_caps(&r, 0x0b), Some(CeCaps { engine_type: 0x0b, caps: [0x08, 0x01] }));
        assert_eq!(parse_ce_caps(&r, 0x0c), None);
        assert_eq!(parse_ce_caps(&r[..5], 0x0b), None);
    }

    fn class_list(classes: &[u32]) -> Vec<u8> {
        let mut r = std::vec![0u8; CLASSLIST_BYTES];
        r[..4].copy_from_slice(&(classes.len() as u32).to_le_bytes());
        for (i, c) in classes.iter().enumerate() {
            r[4 + 4 * i..8 + 4 * i].copy_from_slice(&c.to_le_bytes());
        }
        r
    }

    #[test]
    fn the_generation_comes_from_the_class_list() {
        let gb202 = class_list(&[0xc56f, 0xc7b5, 0xc561, 0xca6f, 0xcab5, 0xc761]);
        assert_eq!(gen_from_class_list(&gb202), Some(Gen::Gb202));
        let ada = class_list(&[0xc56f, 0xc7b5, 0xc561, 0xc597]);
        assert_eq!(gen_from_class_list(&ada), Some(Gen::Ada));
        // A missing usermode class is no generation.
        assert_eq!(gen_from_class_list(&class_list(&[0xca6f, 0xcab5])), None);
        assert_eq!(gen_from_class_list(&[]), None);
        assert_eq!((gen_word(Gen::Gb202), gen_word(Gen::Ada)), (1, 2));
    }

    #[test]
    fn the_token_names_its_runlist() {
        let t = parse_token(&0x0005_0012u32.to_le_bytes()).unwrap();
        assert_eq!((t.runlist(), t.channel()), (5, 0x12));
        assert_eq!(parse_token(&[1, 2]), None);
    }

    // ── memory layout ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn the_ring_layout_and_the_first_push() {
        assert_eq!(slot_offset(0), 8192);
        assert_eq!(slot_offset(127) + SLOT_BYTES as u64, 8192 + 65536);
        assert_eq!(slot_va(VA_RING, 1), VA_RING + 8192 + 512);
        // The first push (`ce_present`'s words) at slot 0, releasing 1 into the completion.
        let mut buf = [0u32; SLOT_DWORDS];
        let mut p = cp::Push::new(&mut buf);
        cp::set_object(&mut p, Gen::Gb202).unwrap();
        cp::release(
            &mut p,
            cp::Release {
                va: VA_RING + COMPLETION_OFFSET,
                value: 1,
                wfi: true,
                timestamp: false,
                interrupt: false,
            },
        )
        .unwrap();
        assert_eq!(
            p.words(),
            [0x2001_8000, 0xcab5, 0x2005_0017, 0x1000, 0x20, 1, 0, 0x0110_0001]
        );
        assert_eq!(
            cp::gp_entry(slot_va(VA_RING, 0), p.len() as u32),
            Ok((((8u64 << 10) | 0x20) << 32) | 0x2000)
        );
        // Every fixed VA is below 2^40 and the windows do not overlap.
        for va in [VA_RING, VA_SELF_SRC, VA_SELF_DST] {
            assert!(va < cp::MAX_VA);
        }
        assert!(RING_BYTES <= VA_WINDOW && selftest::SRC_BYTES <= VA_WINDOW);
    }

    #[test]
    fn handles_are_distinct_and_outside_the_other_namespaces() {
        let hs = [
            H_VASPACE, H_USERMODE, H_CTL, H_RING, H_RING_VIRT, H_TSG, H_CTXSHARE, H_CHANNEL, H_CE,
            H_SELF_SRC, H_SELF_SRC_VIRT, H_SELF_DST, H_SELF_DST_VIRT,
        ];
        let mut v = hs.to_vec();
        v.sort();
        v.dedup();
        assert_eq!(v.len(), hs.len());
        for h in hs {
            assert!(h != crate::rm_client::H_DEVICE && h != crate::rm_client::H_SUBDEVICE);
            assert!(!(crate::rm_sysmem::H_BASE..crate::rm_sysmem::H_BASE + 0x100).contains(&h));
            assert!(!(0x4b4d_1000..0x4b4d_1100).contains(&h));
        }
    }

    // ── the bring-up and its undo ─────────────────────────────────────────────────────────────

    fn fail() -> Fail {
        Fail::new(FailKind::Rm, 0x1f)
    }

    #[test]
    fn a_clean_bring_up_runs_every_stage_in_the_tools_order() {
        let mut b = BringUp::new();
        let mut seen = Vec::new();
        while let Some(s) = b.next_stage() {
            seen.push(s);
            b.finish(s, Ok(()));
        }
        assert_eq!(seen, STAGES.to_vec());
        assert!(b.is_ready());
        for (i, s) in STAGES.iter().enumerate() {
            assert_eq!(*s as usize, i + 1, "stage numbers are the breadcrumbs");
        }
        // Everything with an undo was made.
        let all = STAGES.iter().fold(Made::NONE, |m, s| m.with(s.makes()));
        assert_eq!(b.made(), all);
        assert_eq!(all.bits(), (1 << 11) - 1);
    }

    fn undo_all(mut made: Made) -> Vec<Undo> {
        let mut out = Vec::new();
        while let Some(u) = next_undo(made) {
            out.push(u);
            made = made.without(u.undoes());
        }
        out
    }

    #[test]
    fn a_teardown_gives_everything_back_in_the_tools_order() {
        let all = STAGES.iter().fold(Made::NONE, |m, s| m.with(s.makes()));
        assert_eq!(undo_all(all), UNDO_ORDER.to_vec());
        // Schedule off, then the TSG (channel and CE object with it), before any memory the
        // channel referenced; the client last.
        assert_eq!(UNDO_ORDER[0], Undo::ScheduleOff);
        assert_eq!(UNDO_ORDER[1], Undo::FreeTsg);
        assert_eq!(UNDO_ORDER[10], Undo::CloseClient);
    }

    #[test]
    fn a_failure_at_any_stage_undoes_exactly_what_was_made_in_reverse() {
        for (k, failing) in STAGES.iter().enumerate() {
            let mut b = BringUp::new();
            while let Some(s) = b.next_stage() {
                b.finish(s, if s == *failing { Err(fail()) } else { Ok(()) });
            }
            assert!(!b.is_ready());
            assert_eq!(b.failure(), Some((*failing, fail())));
            let made_before = STAGES[..k].iter().fold(Made::NONE, |m, s| m.with(s.makes()));
            assert_eq!(b.made(), made_before);
            let undo = undo_all(b.made());
            // Exactly the undos of what was made, in the fixed order.
            let expect: Vec<Undo> =
                UNDO_ORDER.iter().copied().filter(|u| made_before.contains(u.undoes())).collect();
            assert_eq!(undo, expect);
            // The failing stage's own object is not in it (its I/O undoes its own partial work).
            if !failing.makes().is_empty() {
                assert!(!b.made().contains(failing.makes()));
            }
        }
    }

    #[test]
    fn a_stray_report_is_ignored() {
        let mut b = BringUp::new();
        b.finish(Stage::Ring, Ok(()));
        assert_eq!(b.next_stage(), Some(Stage::Client));
        b.finish(Stage::Client, Err(fail()));
        b.finish(Stage::Client, Ok(()));
        assert_eq!(b.next_stage(), None);
        assert_eq!(b.made(), Made::NONE);
        assert_eq!(pack_failure(Stage::Group as u8, fail()), 0x0c04_001f);
    }

    // ── the service ───────────────────────────────────────────────────────────────────────────

    #[test]
    fn one_bring_up_at_a_time_then_ready() {
        let mut s = Svc::new();
        assert_eq!(s.admit(7, 0), Admit::BringUp);
        assert_eq!(s.admit(7, 1), Admit::Refuse(Why::Busy));
        s.bring_up_done(true, 2);
        assert_eq!(s.admit(7, 3), Admit::Ready);
        assert_eq!(chan_word(s.phase()), 1);
    }

    #[test]
    fn three_failed_bring_ups_disable_until_the_next_generation() {
        let mut s = Svc::new();
        let mut now = 0;
        for k in 1..=MAX_STRIKES {
            assert_eq!(s.admit(1, now), Admit::BringUp, "attempt {k}");
            s.bring_up_done(false, now);
            assert_eq!(s.strikes(), k);
            if k < MAX_STRIKES {
                assert_eq!(s.admit(1, now + RETRY_AFTER_MS - 1), Admit::Refuse(Why::CoolDown));
                now += RETRY_AFTER_MS;
            }
        }
        assert_eq!(s.admit(1, now + 10 * RETRY_AFTER_MS), Admit::Refuse(Why::Disabled));
        assert_eq!(chan_word(s.phase()), 2);
        // A new transport generation starts over.
        assert_eq!(s.admit(2, now), Admit::BringUp);
        assert_eq!(s.strikes(), 0);
    }

    #[test]
    fn a_success_clears_the_strikes() {
        let mut s = Svc::new();
        s.admit(1, 0);
        s.bring_up_done(false, 0);
        s.admit(1, RETRY_AFTER_MS);
        s.bring_up_done(true, RETRY_AFTER_MS);
        assert_eq!(s.strikes(), 0);
    }

    #[test]
    fn a_broken_channel_refuses_is_torn_down_and_strikes() {
        let mut s = Svc::new();
        s.admit(1, 0);
        s.bring_up_done(true, 0);
        s.on_channel_error();
        assert_eq!(s.admit(1, 1), Admit::Refuse(Why::Broken));
        assert!(s.begin_teardown());
        assert_eq!(s.admit(1, 2), Admit::Refuse(Why::Busy));
        assert!(!s.begin_teardown(), "one teardown at a time");
        s.torn_down(3);
        assert_eq!(s.strikes(), 1);
        assert_eq!(s.phase(), Phase::CoolDown);
        // A clean teardown is no strike.
        s.admit(1, 3 + RETRY_AFTER_MS);
        s.bring_up_done(true, 3 + RETRY_AFTER_MS);
        assert!(s.begin_teardown());
        s.torn_down(4 + RETRY_AFTER_MS);
        assert_eq!(s.phase(), Phase::Cold);
        assert_eq!(s.strikes(), 0);
        assert!(!s.begin_teardown(), "nothing to tear down when cold");
    }

    #[test]
    fn the_state_word_and_phase_codes() {
        let mut s = Svc::new();
        assert_eq!(s.state_word(), 0);
        s.admit(1, 0);
        s.bring_up_done(false, 0);
        assert_eq!(s.state_word(), (5 << 28) | (1 << 24));
        assert_eq!(BRING_UP_BUDGET_MS, 6_000);
        assert_eq!(UNDO_BUDGET_MS, 3_000);
    }

    // ── CPU views and the failing call ───────────────────────────────────────────────────────

    #[test]
    fn system_memory_maps_on_a_control_file_bar_memory_on_a_gpu_file() {
        assert_eq!(MapNode::for_class(NV01_MEMORY_SYSTEM), MapNode::Ctl);
        assert_eq!(MapNode::for_class(NV01_MEMORY_SYSTEM_OS_DESCRIPTOR), MapNode::Ctl);
        assert_eq!(MapNode::for_class(0xc761), MapNode::Gpu, "the usermode doorbell");
        assert_eq!(MapNode::for_class(crate::rm_client::NV01_MEMORY_LOCAL_USER), MapNode::Gpu);
        assert_eq!(MapNode::Ctl.other(), MapNode::Gpu);
        // Only RM's INVALID_ARGUMENT, and only once, moves to the other kind.
        assert!(retry_other_node(Fail::new(FailKind::Rm, 0x1f), false));
        assert!(!retry_other_node(Fail::new(FailKind::Rm, 0x1f), true));
        assert!(!retry_other_node(Fail::new(FailKind::Rm, 0x56), false));
        assert!(!retry_other_node(Fail::new(FailKind::Transport, 0x1f), false));
    }

    #[test]
    fn the_cache_knob_defaults_to_cached_like_the_tool() {
        assert_eq!(CacheMode::from_knob(0), CacheMode::Cached);
        assert_eq!(CacheMode::from_knob(1), CacheMode::WriteCombine);
        assert_eq!(CacheMode::from_knob(7), CacheMode::Cached);
        assert_eq!(CacheMode::Cached.sysmem(), crate::rm_sysmem::Cache::Cached);
        // PCI, cached, any physicality: the tool's location and coherency.
        assert_eq!(CacheMode::Cached.sysmem().attr(), 0x3a00_0000);
        assert_eq!((CacheMode::Cached.word(), CacheMode::WriteCombine.word()), (0, 1));
    }

    #[test]
    fn the_failing_call_words() {
        // The 348.1 failure: RM_MAP_MEMORY of the control memory, INVALID_ARGUMENT.
        assert_eq!(rm_call_word(0x4e, H_CTL), 0x4e4d_3003);
        assert_eq!(rm_call_word(0x2b, KEPLER_CHANNEL_GROUP_A), 0x2b00_a06c);
        assert_eq!(rm_call_word(0x2a, CTRL_GET_ENGINES_V2), 0x2a80_0170);
        assert_eq!(fail_word(Fail::new(FailKind::Rm, 0x1f)), 0x1f);
        assert_eq!(fail_word(Fail::new(FailKind::Host, 22)), 0x8003_0016);
        assert_eq!(map_node_word(MapNode::Gpu, 0x123), 0x2000_0123);
        assert_eq!(map_node_word(MapNode::Ctl, 0x123), 0x1000_0123);
        assert_eq!(pack_failure(Stage::CtlMap as u8, Fail::new(FailKind::Rm, 0x1f)), 0x0804_001f);
    }

    // ── the knob ──────────────────────────────────────────────────────────────────────────────

    #[test]
    fn the_knob_values() {
        assert_eq!(mode(0), Mode::Off);
        assert_eq!(mode(1), Mode::Route);
        assert_eq!(mode(2), Mode::SelfTest);
        assert_eq!(mode(3), Mode::Off, "an unknown value does nothing");
        assert_eq!(knob_in_force(7), 0);
        assert_eq!(knob_in_force(2), 2);
        assert_eq!(KNOB, "RmCopyEngine");
    }

    // ── the self-test ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn the_self_test_geometry() {
        use selftest::*;
        assert_eq!(COPY_BYTES, 5_760_000);
        assert_eq!(PAGES, 1407);
        assert_eq!(SRC_BYTES % 4096, 0);
        assert!(SRC_BYTES >= COPY_BYTES + 4 && DST_BYTES >= COPY_BYTES);
        assert_eq!(fail_word(Stage::BringUp), 0xE1);
        assert_eq!(fail_word(Stage::NoTime), 0xEC);
    }

    #[test]
    fn the_two_copies_differ_at_every_word() {
        use selftest::*;
        let salt = 0x5a5a_1234;
        for i in 0..100_000 {
            assert_ne!(Copy::Ready.expected(i, salt), Copy::Wait.expected(i, salt));
        }
        // A verification sees a stale destination of the first copy as a failure of the second.
        let first = |i: u32| Copy::Ready.expected(i, salt);
        assert_eq!(verify(Copy::Ready, salt, 1000, first), Ok(()));
        assert_eq!(verify(Copy::Wait, salt, 1000, first), Err((0, first(0))));
        let mut bad = |i: u32| if i == 777 { 0 } else { Copy::Wait.expected(i, salt) };
        assert_eq!(verify(Copy::Wait, salt, 1000, &mut bad), Err((777, 0)));
    }

    #[test]
    fn the_self_test_pushes_are_the_production_push() {
        use selftest::*;
        let producer = cp::Acquire { va: VA_RING + PRODUCER_OFFSET, value: PRODUCER_WAIT };
        let done = cp::Release {
            va: VA_RING + COMPLETION_OFFSET,
            value: 3,
            wfi: true,
            timestamp: false,
            interrupt: false,
        };
        let mut buf = [0u32; SLOT_DWORDS];
        let mut p = cp::Push::new(&mut buf);
        cp::present_push(
            &mut p,
            Gen::Gb202,
            producer,
            &copy_rect(Copy::Wait, VA_SELF_SRC, VA_SELF_DST),
            done,
        )
        .unwrap();
        assert_eq!(
            p.words(),
            [
                0x2005_0017, 0x1040, 0x20, 2, 0, 0x0100_1002,
                0x2008_8100, 0x20, 0x0400_0004, 0x20, 0x0800_0000, 6400, 6400, 6400, 900,
                0x2001_80c0, 0x386,
                0x2005_0017, 0x1000, 0x20, 3, 0, 0x0110_0001,
            ]
        );
        assert!(p.len() <= SLOT_DWORDS);
        // The ready copy reads from the first word.
        assert_eq!(copy_rect(Copy::Ready, VA_SELF_SRC, VA_SELF_DST).src_va, VA_SELF_SRC);
    }

    #[test]
    fn why_words() {
        use selftest::why_word;
        assert_eq!(why_word(None), 0);
        assert_eq!(why_word(Some(Fail::new(FailKind::Rm, 0x56))), 0x56);
        assert_eq!(why_word(Some(Fail::new(FailKind::Transport, 1))), 0x8002_0001);
        assert_eq!(selftest::us_between(10, 25), 1);
        assert_eq!(selftest::us_between(25, 10), 0);
    }

    // ── names ─────────────────────────────────────────────────────────────────────────────────

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
        // The five channel names of the route's list, and nothing else of it (the rest is M3c's).
        let shared: Vec<&str> =
            COUNTERS.iter().copied().filter(|n| crate::ce_present::COUNTERS.contains(n)).collect();
        assert_eq!(shared, ["CeKnob", "CeChan", "CeRunlist", "CeRmErr", "CeChanFail"]);
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

    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    /// Every name is written by the channel's I/O file (the self-test stores its results in that
    /// file's atomics; `ce_selftest.rs` spells no name, which the next test checks).
    const WRITERS: [&str; 1] = ["virtio/rm_client/ce_channel.rs"];

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(render) = render_src() else {
            return;
        };
        let mut written = Vec::new();
        for f in WRITERS {
            let text = std::fs::read_to_string(render.join(f)).unwrap();
            written.extend(literals(&text));
        }
        for n in COUNTERS {
            assert!(written.iter().any(|l| l == n), "{n} is listed but not written by {WRITERS:?}");
        }
        for l in written.iter().filter(|l| l.starts_with("Ce")) {
            assert!(COUNTERS.contains(&l.as_str()), "{l} is written by {WRITERS:?} but not listed");
        }
    }

    #[test]
    fn no_other_file_writes_these_names_and_the_knob_is_in_diag() {
        let Some(render) = render_src() else {
            return;
        };
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().replace('\\', "/");
                    if WRITERS.iter().any(|w| s.ends_with(w)) || s.ends_with("/diag.rs") {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(!text.contains("b\"RmCopyEngine\""), "{s} spells the knob name");
                    // The record's writer (M3c-0) spells only its own list, which
                    // `ce_record`'s exact-list test checks; none of these names.
                    // So does the Present route's writer (M3c-2, `ce_route`'s exact-list test).
                    if crate::ce_record::WRITERS.iter().any(|w| s.ends_with(w))
                        || crate::ce_route::WRITERS.iter().any(|w| s.ends_with(w))
                    {
                        for n in COUNTERS {
                            assert!(!text.contains(&std::format!("b\"{n}\"")), "{s} spells {n}");
                        }
                        continue;
                    }
                    assert!(!text.contains("b\"Ce"), "{s} spells a Ce counter name");
                }
            }
        }
        assert!(checked > 20);
        let diag = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        assert!(diag.contains("KnobName::new(b\"RmCopyEngine\")"));
        assert!(!diag.contains("b\"Ce"), "diag.rs spells a Ce counter name");
    }
}

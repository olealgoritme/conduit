//! Context-switch preemption modes of the native channels (`NvPreempt`, REG_DWORD, default 0):
//! the pure half. The call site is `create_queue_objects_on` in
//! `kmd_render/src/virtio/native_channel.rs`.
//!
//! NVIDIA's drivers set the preemption mode of every channel they make
//! (`NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE` after the engine objects: compute CTA in its
//! Linux Vulkan driver's RM trace; the 610.88 Windows KMD reads `RMFailOnNoGfxP` /
//! `RMFailOnNoCILP` / `RMEnableGfxpModeAsDefault`, so its contexts run GfxP and CILP). This
//! KMD leaves RM's defaults. With wait-for-idle graphics preemption a TSG can only be switched
//! out when its whole graphics pipe has drained, so the host's timeslice switch to another TSG
//! (the desktop's, another queue's) waits on the slowest or stuck draw: `Xid 109 CTX SWITCH
//! TIMEOUT` with `CTXSW_STATE` SWITCH, `STATUS` AWAITING_ACK (FECS never acknowledged the save).
//!
//! # What RM needs (open-gpu-kernel-modules 610.57.04)
//!
//! * `SET_CTXSW_PREEMPTION_MODE` (0x20801210), `CTXSW_PREEMPTION_BIND` (0x20801211) and
//!   `GET_CTXSW_MODES` (0x20801235) are `ROUTE_TO_PHYSICAL`: kernel RM forwards them to GSP-RM
//!   unread (`g_subdevice_nvoc.c` method table, `rmresControl_Prologue`), so every rule is
//!   GSP-RM's and the status is GSP-RM's answer.
//! * The modes belong to the TSG's one GR context (`kgrctxFromKernelChannel`: a channel's
//!   context is its TSG's), so setting them through any channel of the TSG, a compute-only
//!   channel on another VEID included, changes them for the whole TSG.
//! * GfxP needs per-context buffers the client provides: the vGPU guest RM on Unix allocates
//!   them for every 3D object (`vgpu/rpc.c` `_allocateGfxpBuffer`): for each of MAIN (the GfxP
//!   context-switch image), SPILL, PAGEPOOL, BETACB and RTV, the size and alignment of
//!   `NV0080_CTRL_CMD_FIFO_GET_ENGINE_CONTEXT_PROPERTIES` engine `GRAPHICS_PREEMPT + i`
//!   (0x0b..0x0f; size 0 = not needed, pointer 0), `NV01_MEMORY_LOCAL_USER` video memory, an
//!   `NV50_MEMORY_VIRTUAL` in the channel's VA space and a DMA mapping, then
//!   `CTXSW_PREEMPTION_BIND { flags GFXP_SET, hClient, hChannel = the TSG (the channel
//!   when it has none), vMemPtrs[0..5], gfxpPreemptMode GFXP }`.
//! * GFXP_POOL takes RM's global GfxP pool instead. Kernel RM names it in the 3D promote list
//!   (`kgrobjGetPromoteIds_FWCLIENT`) but never allocates nor maps it
//!   (`kgrctxPrepareInitializeCtxBuffer`: "No initialization from kernel RM";
//!   `kgrctxMapGlobalCtxBuffers` maps bundle CB, attribute CB, page pool and RTV CB only), so
//!   under the Linux kernel RM GSP-RM has no pool for these contexts. NVIDIA's Windows KMD
//!   brings its own. On this host GFXP_POOL is expected to be refused.
//!
//! # The knob
//!
//! Bits 0..4 the graphics mode + 1 (0 = leave RM's, 1 WFI, 2 GFXP, 3 GFXP_POOL), bits 4..8
//! the compute mode + 1 (0 = leave RM's, 1 WFI, 2 CTA, 3 CILP), bit 8 = read the modes in
//! force and the TSG's hardware id back (`NV2080_CTRL_CMD_GR_GET_CTXSW_MODES`) and record them
//! even when nothing is set. GFXP goes through [`CTRL_PREEMPTION_BIND`] with buffers the KMD
//! allocates; WFI and GFXP_POOL through [`CTRL_SET_PREEMPTION_MODE`]. 0x32 = GFXP + CILP (the
//! Windows driver's set, with per-context buffers instead of the pool), 0x22 = GFXP + CTA,
//! 0x02 = GFXP alone, 0x33 = GFXP_POOL + CILP.
//!
//! Both or neither: the graphics and compute modes go to RM in one call, on the TSG's first
//! VEID 0 channel (the one with the 3D object), before the TSG is first scheduled. A refusal
//! leaves the TSG at RM's defaults for both. Later channels of the TSG, the compute-only
//! channels on other VEIDs included, never set a mode of their own ([`Gate`]).

/// `NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE` (on the subdevice).
pub const CTRL_SET_PREEMPTION_MODE: u32 = 0x2080_1210;
/// `NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND` (on the subdevice).
pub const CTRL_PREEMPTION_BIND: u32 = 0x2080_1211;
/// `NV2080_CTRL_CMD_GR_GET_CTXSW_MODES` (on the subdevice).
pub const CTRL_GET_CTXSW_MODES: u32 = 0x2080_1235;
/// `NVA06C_CTRL_CMD_GET_INFO` (on the TSG): `{ tsgID u32 }`, the hardware TSG id.
pub const CTRL_TSG_GET_INFO: u32 = 0xa06c_0106;
/// `NV0080_CTRL_CMD_FIFO_GET_ENGINE_CONTEXT_PROPERTIES` (on the device):
/// `{ engineId, alignment, size }`.
pub const CTRL_ENGINE_CONTEXT_PROPERTIES: u32 = 0x0080_1707;
/// `sizeof(NV2080_CTRL_GR_SET_CTXSW_PREEMPTION_MODE_PARAMS)`: flags, hChannel, gfxp, cilp,
/// then the 8-aligned `NV2080_CTRL_GR_ROUTE_INFO { flags u32; route u64 }` (16 bytes).
pub const SET_BYTES: usize = 32;
/// `sizeof(NV2080_CTRL_GR_GET_CTXSW_MODES_PARAMS)`: six u32.
pub const GET_BYTES: usize = 24;
/// `sizeof(NV2080_CTRL_GR_CTXSW_PREEMPTION_BIND_PARAMS)` on 610.57.04: flags, hClient,
/// hChannel, pad, `vMemPtrs[9]` (8-aligned, at 16), gfxp (88), cilp (92), route info (96).
pub const BIND_BYTES: usize = 112;
/// `sizeof(NV0080_CTRL_FIFO_GET_ENGINE_CONTEXT_PROPERTIES_PARAMS)`.
pub const PROPS_BYTES: usize = 12;

const BIND_CLIENT_AT: usize = 4;
const BIND_CHANNEL_AT: usize = 8;
const BIND_PTRS_AT: usize = 16;
const BIND_GFXP_AT: usize = 88;
const BIND_CILP_AT: usize = 92;

/// `NV0080_CTRL_FIFO_GET_ENGINE_CONTEXT_PROPERTIES_ENGINE_ID_GRAPHICS_PREEMPT`; the bind
/// buffers' engine ids are this + their index.
pub const ENGINE_ID_GRAPHICS_PREEMPT: u32 = 0x0b;
/// The buffers a client binds for GfxP: `NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND_BUFFERS_`
/// MAIN, SPILL, PAGEPOOL, BETACB, RTV (the vGPU guest's set: indexes below `CONTEXT_POOL`).
pub const BIND_BUFFERS: usize = 5;
/// The most video memory one TSG's GfxP buffers may take. RM's sizes are a few MiB each; a
/// larger answer is not bound (and the TSG keeps RM's defaults).
pub const MAX_BIND_TOTAL: u64 = 256 << 20;
/// The largest alignment taken from RM.
pub const MAX_BIND_ALIGN: u64 = 2 << 20;

/// `NV2080_CTRL_GR_SET_CTXSW_PREEMPTION_MODE_FLAGS_CILP_SET` (0:0).
pub const FLAGS_CILP_SET: u32 = 1 << 0;
/// `NV2080_CTRL_GR_SET_CTXSW_PREEMPTION_MODE_FLAGS_GFXP_SET` (1:1).
pub const FLAGS_GFXP_SET: u32 = 1 << 1;

pub const GFX_WFI: u32 = 0;
pub const GFX_GFXP: u32 = 1;
pub const GFX_GFXP_POOL: u32 = 2;
pub const COMPUTE_WFI: u32 = 0;
pub const COMPUTE_CTA: u32 = 1;
pub const COMPUTE_CILP: u32 = 2;

/// What `NvPreempt` = `knob` asks of a channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The graphics mode to set, if any.
    pub gfx: Option<u32>,
    /// The compute mode to set, if any.
    pub compute: Option<u32>,
    /// Read the modes in force back after the channel's objects exist.
    pub query: bool,
}

impl Plan {
    /// Whether a set call is to be made.
    pub const fn sets(&self) -> bool {
        self.gfx.is_some() || self.compute.is_some()
    }

    /// How the modes reach RM.
    pub const fn route(&self) -> Route {
        match (self.gfx, self.compute) {
            (Some(GFX_GFXP), _) => Route::Bind,
            (None, None) => Route::Nothing,
            _ => Route::Set,
        }
    }
}

/// How a plan's modes reach RM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Nothing,
    /// `SET_CTXSW_PREEMPTION_MODE`: WFI, GFXP_POOL, or a compute mode alone.
    Set,
    /// `CTXSW_PREEMPTION_BIND` with the KMD's GfxP buffers: GFXP (and the compute mode, if
    /// any, in the same call).
    Bind,
}

/// The plan for a channel: `graphics` = it is on VEID 0 with the 3D object, else a
/// compute-only channel, which sets nothing (its TSG's modes are set through the VEID 0
/// channel, and cover it). An unread or absent knob (`u32::MAX`, 0) asks nothing.
pub const fn plan(knob: u32, graphics: bool) -> Plan {
    if knob == u32::MAX {
        return Plan { gfx: None, compute: None, query: false };
    }
    let g = knob & 0xf;
    let c = (knob >> 4) & 0xf;
    let gfx = if g >= 1 && g <= 3 { Some(g - 1) } else { None };
    let compute = if c >= 1 && c <= 3 { Some(c - 1) } else { None };
    let query = knob & 0x100 != 0 || gfx.is_some() || compute.is_some();
    if graphics {
        Plan { gfx, compute, query }
    } else {
        Plan { gfx: None, compute: None, query }
    }
}

/// The TSG-wide gate (one per WDDM context): its modes are set once, by the first VEID 0
/// channel, and never through a compute-only channel. Held in one `u32`.
pub mod gate {
    /// Nothing tried yet.
    pub const UNTRIED: u32 = 0;
    /// A channel is setting the modes now.
    pub const BUSY: u32 = 1;
    /// Set (both modes asked, as one call).
    pub const SET: u32 = 2;
    /// Refused (or the buffers could not be made): the TSG keeps RM's defaults for both.
    pub const REFUSED: u32 = 3;
}

/// Whether a channel with `plan` and `graphics` sets its TSG's modes, given the TSG's gate.
/// Only the first VEID 0 channel of an untried TSG does; the caller then moves the gate
/// `UNTRIED -> BUSY` (compare-exchange) and to [`gate_after`] once RM answered.
pub const fn claims(plan: &Plan, graphics: bool, gate: u32) -> bool {
    graphics && plan.sets() && gate == gate::UNTRIED
}

/// The gate once the one call for the TSG has been answered.
pub const fn gate_after(accepted: bool) -> u32 {
    if accepted {
        gate::SET
    } else {
        gate::REFUSED
    }
}

fn put32(a: &mut [u8], at: usize, v: u32) {
    a[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(a: &mut [u8], at: usize, v: u64) {
    a[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn get32(a: &[u8], at: usize) -> Option<u32> {
    let s = a.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The flags of a call carrying `plan`'s modes.
const fn flags_of(plan: &Plan) -> u32 {
    let mut flags = 0;
    if plan.gfx.is_some() {
        flags |= FLAGS_GFXP_SET;
    }
    if plan.compute.is_some() {
        flags |= FLAGS_CILP_SET;
    }
    flags
}

/// `NV2080_CTRL_GR_SET_CTXSW_PREEMPTION_MODE_PARAMS` for `channel` (route info zero: the
/// one GR engine), both of `plan`'s modes in the one call. `None` when the plan sets
/// nothing.
pub fn set_params(channel: u32, plan: &Plan) -> Option<[u8; SET_BYTES]> {
    if !plan.sets() {
        return None;
    }
    let mut a = [0u8; SET_BYTES];
    put32(&mut a, 0, flags_of(plan));
    put32(&mut a, 4, channel);
    put32(&mut a, 8, plan.gfx.unwrap_or(0));
    put32(&mut a, 12, plan.compute.unwrap_or(0));
    Some(a)
}

/// `NV2080_CTRL_GR_CTXSW_PREEMPTION_BIND_PARAMS`: GFXP with `vas` (GPU VAs of MAIN, SPILL,
/// PAGEPOOL, BETACB, RTV; 0 = not needed) for `target` (the TSG, else the channel) of
/// `client`, and `plan`'s compute mode in the same call. `None` unless the plan's route is
/// [`Route::Bind`] and the MAIN buffer is there.
pub fn bind_params(
    client: u32,
    target: u32,
    plan: &Plan,
    vas: &[u64; BIND_BUFFERS],
) -> Option<[u8; BIND_BYTES]> {
    if plan.route() != Route::Bind || vas[0] == 0 {
        return None;
    }
    let mut a = [0u8; BIND_BYTES];
    put32(&mut a, 0, flags_of(plan));
    put32(&mut a, BIND_CLIENT_AT, client);
    put32(&mut a, BIND_CHANNEL_AT, target);
    for (i, &va) in vas.iter().enumerate() {
        put64(&mut a, BIND_PTRS_AT + 8 * i, va);
    }
    put32(&mut a, BIND_GFXP_AT, GFX_GFXP);
    put32(&mut a, BIND_CILP_AT, plan.compute.unwrap_or(0));
    Some(a)
}

/// `NV0080_CTRL_FIFO_GET_ENGINE_CONTEXT_PROPERTIES_PARAMS` asking for bind buffer `index`.
pub fn props_params(index: usize) -> [u8; PROPS_BYTES] {
    let mut a = [0u8; PROPS_BYTES];
    put32(&mut a, 0, ENGINE_ID_GRAPHICS_PREEMPT + index as u32);
    a
}

/// One bind buffer as RM sized it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buffer {
    pub size: u64,
    pub align: u64,
}

/// `(alignment, size)` of an answered `GET_ENGINE_CONTEXT_PROPERTIES`, the alignment
/// raised to 4 KiB and the size rounded up to it.
pub fn parse_props(reply: &[u8; PROPS_BYTES]) -> Buffer {
    let align = u64::from(get32(reply, 4).unwrap_or(0)).max(0x1000);
    let size = u64::from(get32(reply, 8).unwrap_or(0));
    if size == 0 {
        return Buffer { size: 0, align };
    }
    Buffer { size: size.div_ceil(align) * align, align }
}

/// Why RM's sizes are not bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// MAIN has no size: RM has no GfxP image for this GPU.
    NoMain,
    /// An alignment that is not a power of two or larger than [`MAX_BIND_ALIGN`].
    Align,
    /// More than [`MAX_BIND_TOTAL`] in all.
    Total,
}

impl LayoutError {
    /// The diagnostic code (`NvPrBufErr`).
    pub const fn code(self) -> u32 {
        match self {
            Self::NoMain => 1,
            Self::Align => 2,
            Self::Total => 3,
        }
    }
}

/// Check RM's sizes for the five buffers before any is allocated. Returns the total.
pub fn check_layout(bufs: &[Buffer; BIND_BUFFERS]) -> Result<u64, LayoutError> {
    if bufs[0].size == 0 {
        return Err(LayoutError::NoMain);
    }
    let mut total = 0u64;
    for b in bufs {
        if !b.align.is_power_of_two() || b.align > MAX_BIND_ALIGN {
            return Err(LayoutError::Align);
        }
        total = total.saturating_add(b.size);
    }
    if total > MAX_BIND_TOTAL {
        return Err(LayoutError::Total);
    }
    Ok(total)
}

/// `sizeof(NV_MEMORY_ALLOCATION_PARAMS)`.
pub const MEM_ALLOC_BYTES: usize = 128;
const NVOS32_TYPE_IMAGE: u32 = 0;
const NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE: u32 = 0x100;
const NVOS32_ALLOC_FLAGS_VIRTUAL: u32 = 0x0008_0000;
/// `NVOS32_ATTR_LOCATION_ANY` (26:25 = 3).
const NVOS32_ATTR_LOCATION_ANY: u32 = 3 << 25;

/// The video memory of one bind buffer, as the vGPU guest allocates it: owner `root`, type
/// IMAGE, `ALIGNMENT_FORCE`, attr `LOCATION_VIDMEM` (0), RM's size and alignment.
pub fn vidmem_params(root: u32, b: &Buffer) -> [u8; MEM_ALLOC_BYTES] {
    let mut a = [0u8; MEM_ALLOC_BYTES];
    put32(&mut a, 0, root);
    put32(&mut a, 4, NVOS32_TYPE_IMAGE);
    put32(&mut a, 8, NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE);
    put64(&mut a, 64, b.size);
    put64(&mut a, 72, b.align);
    a
}

/// Its virtual allocation in the VA space `h_vaspace` (RM places it: first fit above the
/// KMD's low window, inside the range reserved from VidMm): `VIRTUAL | ALIGNMENT_FORCE`,
/// attr `LOCATION_ANY`.
pub fn virtual_params(root: u32, h_vaspace: u32, b: &Buffer) -> [u8; MEM_ALLOC_BYTES] {
    let mut a = [0u8; MEM_ALLOC_BYTES];
    put32(&mut a, 0, root);
    put32(&mut a, 4, NVOS32_TYPE_IMAGE);
    put32(&mut a, 8, NVOS32_ALLOC_FLAGS_VIRTUAL | NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE);
    put32(&mut a, 24, NVOS32_ATTR_LOCATION_ANY);
    put64(&mut a, 64, b.size);
    put64(&mut a, 72, b.align);
    put32(&mut a, 108, h_vaspace);
    a
}

/// Whether a mapped VA is usable for buffer `b`: non-zero and aligned.
pub fn va_ok(va: u64, b: &Buffer) -> bool {
    va != 0 && va % b.align == 0
}

/// `NV2080_CTRL_GR_GET_CTXSW_MODES_PARAMS` asking for `channel`.
pub fn get_params(channel: u32) -> [u8; GET_BYTES] {
    let mut a = [0u8; GET_BYTES];
    put32(&mut a, 0, channel);
    a
}

/// `(gfxp, cilp)` from an answered `GET_CTXSW_MODES`, packed for one registry value:
/// `gfxp << 8 | cilp`.
pub fn modes_word(reply: &[u8; GET_BYTES]) -> u32 {
    let get = |at: usize| u32::from_le_bytes([reply[at], reply[at + 1], reply[at + 2], reply[at + 3]]);
    (get(20) & 0xff) << 8 | (get(16) & 0xff)
}

/// The packed word the modes of `plan` should read back as, if set: `gfxp << 8 | cilp`
/// with RM's defaults (WFI, 0) for a mode not asked.
pub const fn expected_word(plan: &Plan) -> u32 {
    let g = match plan.gfx {
        Some(g) => g,
        None => 0,
    };
    let c = match plan.compute {
        Some(c) => c,
        None => 0,
    };
    (g & 0xff) << 8 | (c & 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_asks_nothing() {
        assert_eq!(plan(0, true), Plan::default());
        assert_eq!(plan(u32::MAX, true), Plan::default());
        assert!(set_params(5, &plan(0, true)).is_none());
        assert_eq!(plan(0, true).route(), Route::Nothing);
    }

    #[test]
    fn gfxp_pool_and_cilp_go_through_set_in_one_call() {
        let p = plan(0x33, true);
        assert_eq!(p, Plan { gfx: Some(GFX_GFXP_POOL), compute: Some(COMPUTE_CILP), query: true });
        assert_eq!(p.route(), Route::Set);
        let a = set_params(0x4e57_2005, &p).unwrap();
        assert_eq!(&a[0..4], &(FLAGS_GFXP_SET | FLAGS_CILP_SET).to_le_bytes());
        assert_eq!(&a[4..8], &0x4e57_2005u32.to_le_bytes());
        assert_eq!(&a[8..12], &GFX_GFXP_POOL.to_le_bytes());
        assert_eq!(&a[12..16], &COMPUTE_CILP.to_le_bytes());
        assert!(a[16..].iter().all(|&b| b == 0), "route info: the one GR engine");
    }

    #[test]
    fn gfxp_goes_through_bind_with_the_compute_mode_in_the_same_call() {
        let p = plan(0x32, true);
        assert_eq!(p, Plan { gfx: Some(GFX_GFXP), compute: Some(COMPUTE_CILP), query: true });
        assert_eq!(p.route(), Route::Bind);
        assert!(set_params(1, &p).is_some(), "set_params is only used for Route::Set");
        let vas = [0x1_2000_0000, 0x1_2040_0000, 0, 0x1_2080_0000, 0x1_20c0_0000];
        let a = bind_params(0xc1d0_0001, 0x4e43_1000, &p, &vas).unwrap();
        assert_eq!(a.len(), 112);
        assert_eq!(&a[0..4], &(FLAGS_GFXP_SET | FLAGS_CILP_SET).to_le_bytes());
        assert_eq!(&a[4..8], &0xc1d0_0001u32.to_le_bytes());
        assert_eq!(&a[8..12], &0x4e43_1000u32.to_le_bytes());
        assert_eq!(&a[12..16], &[0; 4]);
        for (i, va) in vas.iter().enumerate() {
            assert_eq!(&a[16 + 8 * i..24 + 8 * i], &va.to_le_bytes());
        }
        // CONTEXT_POOL, CONTEXT_POOL_CONTROL(_CPU), SETUP: not the client's.
        assert!(a[56..88].iter().all(|&b| b == 0));
        assert_eq!(&a[88..92], &GFX_GFXP.to_le_bytes());
        assert_eq!(&a[92..96], &COMPUTE_CILP.to_le_bytes());
        assert!(a[96..].iter().all(|&b| b == 0), "route info");
        // GFXP alone: no compute flag, compute field zero.
        let a = bind_params(1, 2, &plan(0x02, true), &vas).unwrap();
        assert_eq!(&a[0..4], &FLAGS_GFXP_SET.to_le_bytes());
        assert_eq!(&a[92..96], &[0; 4]);
        // No MAIN buffer, or a plan that does not bind: nothing.
        assert!(bind_params(1, 2, &p, &[0, 1, 1, 1, 1]).is_none());
        assert!(bind_params(1, 2, &plan(0x33, true), &vas).is_none());
    }

    #[test]
    fn a_compute_only_channel_sets_nothing() {
        // Its TSG's modes are set once, through the VEID 0 channel.
        let p = plan(0x33, false);
        assert_eq!(p, Plan { gfx: None, compute: None, query: true });
        assert!(set_params(7, &p).is_none());
        assert!(!claims(&p, false, gate::UNTRIED));
        let p = plan(0x30, false);
        assert!(!p.sets());
        assert!(p.query);
        assert_eq!(plan(0, false), Plan::default());
    }

    #[test]
    fn single_fields_and_out_of_range_values() {
        assert_eq!(plan(0x20, true), Plan { gfx: None, compute: Some(COMPUTE_CTA), query: true });
        assert_eq!(plan(0x20, true).route(), Route::Set);
        assert_eq!(plan(0x02, true), Plan { gfx: Some(GFX_GFXP), compute: None, query: true });
        assert_eq!(plan(0x01, true).gfx, Some(GFX_WFI));
        assert_eq!(plan(0x01, true).route(), Route::Set);
        // 4..15 in a field are not modes.
        assert_eq!(plan(0x44, true), Plan::default());
        assert_eq!(plan(0x100, true), Plan { gfx: None, compute: None, query: true });
    }

    #[test]
    fn the_tsg_gate_sets_both_or_neither_once() {
        let p = plan(0x32, true);
        // Only the first VEID 0 channel of an untried TSG makes the call.
        assert!(claims(&p, true, gate::UNTRIED));
        assert!(!claims(&p, true, gate::BUSY));
        assert!(!claims(&p, true, gate::SET));
        assert!(!claims(&p, true, gate::REFUSED));
        // A refused call leaves the TSG at RM's defaults for both modes: the gate says
        // refused, and no later channel of the TSG, VEID 0 or compute-only, sets anything
        // (the compute half of a refused GFXP + CILP never reaches RM on its own).
        let g = gate_after(false);
        assert_eq!(g, gate::REFUSED);
        assert!(!claims(&p, true, g));
        assert!(!claims(&plan(0x30, true), true, g));
        assert!(!plan(0x32, false).sets());
        assert_eq!(gate_after(true), gate::SET);
        // A plan that sets nothing never claims.
        assert!(!claims(&plan(0x100, true), true, gate::UNTRIED));
    }

    #[test]
    fn engine_properties_and_buffers() {
        for i in 0..BIND_BUFFERS {
            let a = props_params(i);
            assert_eq!(&a[0..4], &(0x0b + i as u32).to_le_bytes());
            assert_eq!(&a[4..], &[0; 8]);
        }
        let mut r = [0u8; PROPS_BYTES];
        r[4..8].copy_from_slice(&0x200u32.to_le_bytes());
        r[8..12].copy_from_slice(&0x1_2345u32.to_le_bytes());
        assert_eq!(parse_props(&r), Buffer { size: 0x1_3000, align: 0x1000 });
        r[4..8].copy_from_slice(&0x1_0000u32.to_le_bytes());
        assert_eq!(parse_props(&r), Buffer { size: 0x2_0000, align: 0x1_0000 });
        r[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_props(&r).size, 0);

        let b = |size: u64, align: u64| Buffer { size, align };
        let ok = [b(0x40_0000, 0x1000), b(0x10_0000, 0x1000), b(0, 0x1000), b(0x80_0000, 0x1_0000), b(0x2000, 0x1000)];
        assert_eq!(check_layout(&ok), Ok(0x40_0000 + 0x10_0000 + 0x80_0000 + 0x2000));
        let mut no_main = ok;
        no_main[0].size = 0;
        assert_eq!(check_layout(&no_main), Err(LayoutError::NoMain));
        let mut odd = ok;
        odd[1].align = 0x3000;
        assert_eq!(check_layout(&odd), Err(LayoutError::Align));
        let mut huge = ok;
        huge[3].size = MAX_BIND_TOTAL;
        assert_eq!(check_layout(&huge), Err(LayoutError::Total));
        assert_eq!(LayoutError::Total.code(), 3);
    }

    #[test]
    fn buffer_allocation_parameters() {
        let b = Buffer { size: 0x80_0000, align: 0x1_0000 };
        let v = vidmem_params(0xc1d0_0001, &b);
        assert_eq!(&v[0..4], &0xc1d0_0001u32.to_le_bytes());
        assert_eq!(&v[4..8], &0u32.to_le_bytes(), "IMAGE");
        assert_eq!(&v[8..12], &0x100u32.to_le_bytes(), "ALIGNMENT_FORCE");
        assert_eq!(&v[24..28], &0u32.to_le_bytes(), "LOCATION_VIDMEM");
        assert_eq!(&v[64..72], &0x80_0000u64.to_le_bytes());
        assert_eq!(&v[72..80], &0x1_0000u64.to_le_bytes());
        assert!(v[108..112].iter().all(|&x| x == 0));
        let a = virtual_params(0xc1d0_0001, 0x4e56_0001, &b);
        assert_eq!(&a[8..12], &0x0008_0100u32.to_le_bytes(), "VIRTUAL | ALIGNMENT_FORCE");
        assert_eq!(&a[24..28], &(3u32 << 25).to_le_bytes(), "LOCATION_ANY");
        assert_eq!(&a[64..72], &0x80_0000u64.to_le_bytes());
        assert_eq!(&a[72..80], &0x1_0000u64.to_le_bytes());
        assert!(a[80..88].iter().all(|&x| x == 0), "RM places it");
        assert_eq!(&a[108..112], &0x4e56_0001u32.to_le_bytes());
        assert!(va_ok(0x1_2001_0000, &b));
        assert!(!va_ok(0x1_2000_1000, &b));
        assert!(!va_ok(0, &b));
    }

    #[test]
    fn the_modes_in_force_pack_into_one_value() {
        let mut r = get_params(9);
        r[16..20].copy_from_slice(&COMPUTE_CTA.to_le_bytes());
        r[20..24].copy_from_slice(&GFX_GFXP_POOL.to_le_bytes());
        assert_eq!(modes_word(&r), 0x0201);
        assert_eq!(&get_params(9)[0..4], &9u32.to_le_bytes());
        assert_eq!(SET_BYTES, 32);
        assert_eq!(GET_BYTES, 24);
        assert_eq!(expected_word(&plan(0x32, true)), 0x0102);
        assert_eq!(expected_word(&plan(0x20, true)), 0x0001);
    }
}

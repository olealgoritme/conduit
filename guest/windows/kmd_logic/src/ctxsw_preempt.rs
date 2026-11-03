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
//! The knob: bits 0..4 the graphics mode + 1 (0 = leave RM's, 1 WFI, 2 GFXP, 3 GFXP_POOL),
//! bits 4..8 the compute mode + 1 (0 = leave RM's, 1 WFI, 2 CTA, 3 CILP), bit 8 = read the
//! modes in force and the TSG's hardware id back (`NV2080_CTRL_CMD_GR_GET_CTXSW_MODES`) and record them even when
//! nothing is set. 0x33 = GFXP_POOL + CILP (the Windows driver's set), 0x23 = GFXP_POOL + CTA.
//! A compute-only channel (no 3D object) gets the compute mode only.

/// `NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE` (on the subdevice).
pub const CTRL_SET_PREEMPTION_MODE: u32 = 0x2080_1210;
/// `NV2080_CTRL_CMD_GR_GET_CTXSW_MODES` (on the subdevice).
pub const CTRL_GET_CTXSW_MODES: u32 = 0x2080_1235;
/// `NVA06C_CTRL_CMD_GET_INFO` (on the TSG): `{ tsgID u32 }`, the hardware TSG id.
pub const CTRL_TSG_GET_INFO: u32 = 0xa06c_0106;
/// `sizeof(NV2080_CTRL_GR_SET_CTXSW_PREEMPTION_MODE_PARAMS)`: flags, hChannel, gfxp, cilp,
/// then the 8-aligned `NV2080_CTRL_GR_ROUTE_INFO { flags u32; route u64 }` (16 bytes).
pub const SET_BYTES: usize = 32;
/// `sizeof(NV2080_CTRL_GR_GET_CTXSW_MODES_PARAMS)`: six u32.
pub const GET_BYTES: usize = 24;

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
}

/// The plan for a channel: `graphics` = it has the 3D object (VEID 0), else a compute-only
/// channel (no graphics mode). An unread or absent knob (`u32::MAX`, 0) asks nothing.
pub const fn plan(knob: u32, graphics: bool) -> Plan {
    if knob == u32::MAX {
        return Plan { gfx: None, compute: None, query: false };
    }
    let g = knob & 0xf;
    let c = (knob >> 4) & 0xf;
    let gfx = if graphics && g >= 1 && g <= 3 { Some(g - 1) } else { None };
    let compute = if c >= 1 && c <= 3 { Some(c - 1) } else { None };
    let query = knob & 0x100 != 0 || gfx.is_some() || compute.is_some();
    Plan { gfx, compute, query }
}

fn put32(a: &mut [u8], at: usize, v: u32) {
    a[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// `NV2080_CTRL_GR_SET_CTXSW_PREEMPTION_MODE_PARAMS` for `channel` (route info zero: the
/// one GR engine). `None` when the plan sets nothing.
pub fn set_params(channel: u32, plan: &Plan) -> Option<[u8; SET_BYTES]> {
    if !plan.sets() {
        return None;
    }
    let mut a = [0u8; SET_BYTES];
    let mut flags = 0;
    if let Some(g) = plan.gfx {
        flags |= FLAGS_GFXP_SET;
        put32(&mut a, 8, g);
    }
    if let Some(c) = plan.compute {
        flags |= FLAGS_CILP_SET;
        put32(&mut a, 12, c);
    }
    put32(&mut a, 0, flags);
    put32(&mut a, 4, channel);
    Some(a)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_asks_nothing() {
        assert_eq!(plan(0, true), Plan::default());
        assert_eq!(plan(u32::MAX, true), Plan::default());
        assert!(set_params(5, &plan(0, true)).is_none());
    }

    #[test]
    fn the_windows_set_is_gfxp_pool_and_cilp() {
        let p = plan(0x33, true);
        assert_eq!(p, Plan { gfx: Some(GFX_GFXP_POOL), compute: Some(COMPUTE_CILP), query: true });
        let a = set_params(0x4e57_2005, &p).unwrap();
        assert_eq!(&a[0..4], &(FLAGS_GFXP_SET | FLAGS_CILP_SET).to_le_bytes());
        assert_eq!(&a[4..8], &0x4e57_2005u32.to_le_bytes());
        assert_eq!(&a[8..12], &GFX_GFXP_POOL.to_le_bytes());
        assert_eq!(&a[12..16], &COMPUTE_CILP.to_le_bytes());
        assert!(a[16..].iter().all(|&b| b == 0), "route info: the one GR engine");
    }

    #[test]
    fn a_compute_only_channel_gets_no_graphics_mode() {
        let p = plan(0x33, false);
        assert_eq!(p.gfx, None);
        assert_eq!(p.compute, Some(COMPUTE_CILP));
        let a = set_params(7, &p).unwrap();
        assert_eq!(&a[0..4], &FLAGS_CILP_SET.to_le_bytes());
        assert_eq!(&a[8..12], &[0; 4]);
        // Graphics only on a compute-only channel: nothing to set, only the query.
        let p = plan(0x103, false);
        assert!(!p.sets());
        assert!(p.query);
    }

    #[test]
    fn single_fields_and_out_of_range_values() {
        assert_eq!(plan(0x20, true), Plan { gfx: None, compute: Some(COMPUTE_CTA), query: true });
        assert_eq!(plan(0x02, true), Plan { gfx: Some(GFX_GFXP), compute: None, query: true });
        assert_eq!(plan(0x01, true).gfx, Some(GFX_WFI));
        // 4..15 in a field are not modes.
        assert_eq!(plan(0x44, true), Plan::default());
        assert_eq!(plan(0x100, true), Plan { gfx: None, compute: None, query: true });
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
    }
}

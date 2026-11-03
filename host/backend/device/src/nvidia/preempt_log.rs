//! RM's answers to the GR preemption controls, one compact log line per call.
//!
//! `NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE`, `NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND`
//! and `NV2080_CTRL_CMD_GR_GET_CTXSW_MODES` are `ROUTE_TO_PHYSICAL`: the host's kernel RM hands
//! them to GSP-RM unread, so the status is GSP-RM's and nothing on the host says why a mode was
//! refused. The guest KMD sets the modes of its TSGs with them (`NvPreempt`) and reads the
//! TSG's hardware id (`NVA06C_CTRL_CMD_GET_INFO`) and the GfxP buffer sizes
//! (`NV0080_CTRL_CMD_FIFO_GET_ENGINE_CONTEXT_PROPERTIES`) next to them. Each call is logged
//! with its handles, its mode values and RM's answer: `info` when RM took it, `warn` when it
//! refused. Always on; the KMD makes a handful per TSG.

/// `NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE`.
pub const SET_PREEMPTION_MODE: u32 = 0x2080_1210;
/// `NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND`.
pub const PREEMPTION_BIND: u32 = 0x2080_1211;
/// `NV2080_CTRL_CMD_GR_GET_CTXSW_MODES`.
pub const GET_CTXSW_MODES: u32 = 0x2080_1235;
/// `NVA06C_CTRL_CMD_GET_INFO`.
pub const TSG_GET_INFO: u32 = 0xa06c_0106;
/// `NV0080_CTRL_CMD_FIFO_GET_ENGINE_CONTEXT_PROPERTIES`.
pub const ENGINE_CONTEXT_PROPERTIES: u32 = 0x0080_1707;

/// Whether `cmd` is one of the logged controls.
pub const fn is_logged(cmd: u32) -> bool {
    matches!(
        cmd,
        SET_PREEMPTION_MODE
            | PREEMPTION_BIND
            | GET_CTXSW_MODES
            | TSG_GET_INFO
            | ENGINE_CONTEXT_PROPERTIES
    )
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn gfx_name(v: u32) -> String {
    match v {
        0 => "WFI".into(),
        1 => "GFXP".into(),
        2 => "GFXP_POOL".into(),
        _ => format!("{v:#x}"),
    }
}

fn compute_name(v: u32) -> String {
    match v {
        0 => "WFI".into(),
        1 => "CTA".into(),
        2 => "CILP".into(),
        _ => format!("{v:#x}"),
    }
}

/// RM's status as a name where it is one of the answers these controls give.
pub fn status_name(status: u32) -> String {
    let name = match status {
        0x00 => "NV_OK",
        0x1a => "NV_ERR_INSUFFICIENT_RESOURCES",
        0x1b => "NV_ERR_INSUFFICIENT_PERMISSIONS",
        0x1e => "NV_ERR_INVALID_ADDRESS",
        0x1f => "NV_ERR_INVALID_ARGUMENT",
        0x21 => "NV_ERR_INVALID_CHANNEL",
        0x23 => "NV_ERR_INVALID_CLIENT",
        0x2e => "NV_ERR_INVALID_LIMIT",
        0x31 => "NV_ERR_INVALID_OBJECT",
        0x33 => "NV_ERR_INVALID_OBJECT_HANDLE",
        0x38 => "NV_ERR_INVALID_OPERATION",
        0x3a => "NV_ERR_INVALID_PARAM_STRUCT",
        0x3f => "NV_ERR_INVALID_REQUEST",
        0x40 => "NV_ERR_INVALID_STATE",
        0x51 => "NV_ERR_NO_MEMORY",
        0x55 => "NV_ERR_NOT_READY",
        0x56 => "NV_ERR_NOT_SUPPORTED",
        0x57 => "NV_ERR_OBJECT_NOT_FOUND",
        _ => return format!("status {status:#x}"),
    };
    format!("{name} ({status:#x})")
}

/// The modes a set or bind asks for, by its flags.
fn asked(flags: u32, gfx: u32, cilp: u32) -> String {
    let mut parts = Vec::new();
    if flags & 2 != 0 {
        parts.push(format!("gfx={}", gfx_name(gfx)));
    }
    if flags & 1 != 0 {
        parts.push(format!("compute={}", compute_name(cilp)));
    }
    if parts.is_empty() {
        parts.push("nothing".into());
    }
    parts.join(" ")
}

/// One line for a call to a logged control, or `None` for any other control (or a block too
/// short to read). `params` is the parameter block as RM left it.
pub fn describe(cmd: u32, client: u32, object: u32, status: u32, params: &[u8]) -> Option<String> {
    let ok = status == 0;
    let what = match cmd {
        SET_PREEMPTION_MODE => {
            let flags = u32_at(params, 0)?;
            format!(
                "GR_SET_CTXSW_PREEMPTION_MODE hChannel={:#x} {}",
                u32_at(params, 4)?,
                asked(flags, u32_at(params, 8)?, u32_at(params, 12)?)
            )
        }
        PREEMPTION_BIND => {
            let flags = u32_at(params, 0)?;
            let ptrs: Vec<String> = (0..5)
                .map(|i| u64_at(params, 16 + 8 * i).map(|v| format!("{v:#x}")))
                .collect::<Option<_>>()?;
            format!(
                "GR_CTXSW_PREEMPTION_BIND hClient={:#x} hChannel={:#x} {} buffers=[{}]",
                u32_at(params, 4)?,
                u32_at(params, 8)?,
                asked(flags, u32_at(params, 88)?, u32_at(params, 92)?),
                ptrs.join(" ")
            )
        }
        GET_CTXSW_MODES => {
            let channel = u32_at(params, 0)?;
            if ok {
                format!(
                    "GR_GET_CTXSW_MODES hChannel={channel:#x}: gfx={} compute={} zcull={} pm={} smpc={}",
                    gfx_name(u32_at(params, 20)?),
                    compute_name(u32_at(params, 16)?),
                    u32_at(params, 4)?,
                    u32_at(params, 8)?,
                    u32_at(params, 12)?
                )
            } else {
                format!("GR_GET_CTXSW_MODES hChannel={channel:#x}")
            }
        }
        TSG_GET_INFO => {
            if ok {
                format!("NVA06C_GET_INFO: tsgID={}", u32_at(params, 0)?)
            } else {
                "NVA06C_GET_INFO".into()
            }
        }
        ENGINE_CONTEXT_PROPERTIES => {
            let engine = u32_at(params, 0)?;
            if ok {
                format!(
                    "FIFO_GET_ENGINE_CONTEXT_PROPERTIES engine={engine:#x}: alignment={:#x} size={:#x}",
                    u32_at(params, 4)?,
                    u32_at(params, 8)?
                )
            } else {
                format!("FIFO_GET_ENGINE_CONTEXT_PROPERTIES engine={engine:#x}")
            }
        }
        _ => return None,
    };
    Some(format!(
        "RM preempt: client={client:#x} object={object:#x} {what} -> {}",
        status_name(status)
    ))
}

/// Log a call to a logged control: `info` when RM took it, `warn` when it refused.
pub fn log(cmd: u32, client: u32, object: u32, status: u32, params: &[u8]) {
    if !is_logged(cmd) {
        return;
    }
    match describe(cmd, client, object, status, params) {
        Some(line) if status == 0 => log::info!("{line}"),
        Some(line) => log::warn!("{line}"),
        None => log::warn!(
            "RM preempt: client={client:#x} object={object:#x} cmd={cmd:#010x} with a {}-byte \
             block -> {}",
            params.len(),
            status_name(status)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn a_refused_set_names_the_modes_and_the_status() {
        let mut p = [0u8; 32];
        put32(&mut p, 0, 3);
        put32(&mut p, 4, 0x4e43_1005);
        put32(&mut p, 8, 2);
        put32(&mut p, 12, 2);
        let line = describe(SET_PREEMPTION_MODE, 0xc1d0_0001, 0x5c00_0002, 0x56, &p).unwrap();
        assert_eq!(
            line,
            "RM preempt: client=0xc1d00001 object=0x5c000002 GR_SET_CTXSW_PREEMPTION_MODE \
             hChannel=0x4e431005 gfx=GFXP_POOL compute=CILP -> NV_ERR_NOT_SUPPORTED (0x56)"
        );
        // Only the flagged modes are named.
        put32(&mut p, 0, 1);
        let line = describe(SET_PREEMPTION_MODE, 1, 2, 0, &p).unwrap();
        assert!(line.contains(" compute=CILP -> NV_OK (0x0)"), "{line}");
        assert!(!line.contains("gfx="), "{line}");
    }

    #[test]
    fn a_bind_names_its_buffers() {
        let mut p = [0u8; 112];
        put32(&mut p, 0, 2);
        put32(&mut p, 4, 0xc1d0_0001);
        put32(&mut p, 8, 0x4e43_1000);
        p[16..24].copy_from_slice(&0x1_2000_0000u64.to_le_bytes());
        put32(&mut p, 88, 1);
        let line = describe(PREEMPTION_BIND, 0xc1d0_0001, 2, 0x1f, &p).unwrap();
        assert!(
            line.contains(
                "GR_CTXSW_PREEMPTION_BIND hClient=0xc1d00001 hChannel=0x4e431000 gfx=GFXP \
                 buffers=[0x120000000 0x0 0x0 0x0 0x0] -> NV_ERR_INVALID_ARGUMENT (0x1f)"
            ),
            "{line}"
        );
        assert!(describe(PREEMPTION_BIND, 1, 2, 0, &p[..80]).is_none());
    }

    #[test]
    fn answers_are_read_only_when_rm_gave_them() {
        let mut g = [0u8; 24];
        put32(&mut g, 0, 7);
        put32(&mut g, 16, 2);
        put32(&mut g, 20, 1);
        let line = describe(GET_CTXSW_MODES, 1, 2, 0, &g).unwrap();
        assert!(
            line.contains("hChannel=0x7: gfx=GFXP compute=CILP zcull=0 pm=0 smpc=0"),
            "{line}"
        );
        let line = describe(GET_CTXSW_MODES, 1, 2, 0x33, &g).unwrap();
        assert!(
            line.ends_with(
                "GR_GET_CTXSW_MODES hChannel=0x7 -> NV_ERR_INVALID_OBJECT_HANDLE (0x33)"
            ),
            "{line}"
        );
        let line = describe(TSG_GET_INFO, 1, 0x4e43_1000, 0, &50u32.to_le_bytes()).unwrap();
        assert!(
            line.contains("NVA06C_GET_INFO: tsgID=50 -> NV_OK"),
            "{line}"
        );
        let mut e = [0u8; 12];
        put32(&mut e, 0, 0x0b);
        put32(&mut e, 4, 0x1000);
        put32(&mut e, 8, 0x40_0000);
        let line = describe(ENGINE_CONTEXT_PROPERTIES, 1, 2, 0, &e).unwrap();
        assert!(
            line.contains("engine=0xb: alignment=0x1000 size=0x400000"),
            "{line}"
        );
    }

    #[test]
    fn other_controls_are_not_logged() {
        assert!(!is_logged(0x2080_0102));
        assert!(describe(0x2080_0102, 1, 2, 0, &[0; 64]).is_none());
        for c in [
            SET_PREEMPTION_MODE,
            PREEMPTION_BIND,
            GET_CTXSW_MODES,
            TSG_GET_INFO,
            ENGINE_CONTEXT_PROPERTIES,
        ] {
            assert!(is_logged(c));
        }
        assert_eq!(status_name(0x1234), "status 0x1234");
    }
}

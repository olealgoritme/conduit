//! The DDI table's entries: one thin wrapper per status-returning DDI that times the call, marks
//! it in flight and hands every non-success status to `ddi::device_lost` (the failure rings and
//! the sticky first-fatal record). `lib.rs` wires THESE into `DRIVER_INITIALIZATION_DATA`; the
//! real DDIs are untouched and keep their names.
//!
//! The wrapper never changes the status, never takes a lock and never writes the registry on the
//! way through: atomics and two clock reads, legal at every IRQL a wrapped DDI can run at
//! (`SetVidPnSourceAddress` and `ControlInterrupt` at DIRQL, the submit DDIs at DISPATCH). The
//! two PASSIVE-only teardown DDIs (`DestroyDevice`, `StopDevice`) also mirror the block to the
//! registry when a ring moved, because those are the calls that run while an adapter is being
//! lost.
//!
//! Hints (the 24 bits next to the DDI id in a ring entry): the handle's low bits, the escape
//! code for `Escape`, `uid << 16 | state << 8 | action` for `SetPowerState`, the interrupt type
//! for `ControlInterrupt`.

use core::ffi::c_void;

use crate::ddi::device_lost as dlost;
use crate::dxgk::*;
use helios_kmd_logic::device_lost::ddi;

/// `$name` calls `$target` with the same arguments and records the outcome under `$id`.
/// `$hint` is an expression over the argument names.
macro_rules! traced {
    ($name:ident, $id:expr, $target:path, ($($arg:ident : $ty:ty),*), $hint:expr) => {
        #[inline(never)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> NTSTATUS {
            let started = dlost::enter($id);
            // SAFETY: the same contract the target DDI documents; the arguments are forwarded
            // unchanged.
            let status = unsafe { $target($($arg),*) };
            dlost::leave($id, started, status, $hint);
            status
        }
    };
    // PASSIVE-only teardown DDIs: mirror the block when a ring moved.
    ($name:ident, $id:expr, $target:path, ($($arg:ident : $ty:ty),*), $hint:expr, publish) => {
        #[inline(never)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> NTSTATUS {
            let started = dlost::enter($id);
            // SAFETY: as above.
            let status = unsafe { $target($($arg),*) };
            dlost::leave($id, started, status, $hint);
            // These two DDIs are documented PASSIVE_LEVEL.
            dlost::publish_if_dirty();
            status
        }
    };
}

/// The escape code of a Helios escape (`HeliosEscapeHeader::cmd_type`, the second word), 0 when
/// the buffer is too short to hold one. dxgkrnl hands the DDI a captured kernel copy.
fn escape_hint(escape: *const DXGKARG_ESCAPE) -> u32 {
    if escape.is_null() {
        return 0;
    }
    // SAFETY: non-null; the args struct is valid for the call and the buffer it names is
    // `PrivateDriverDataSize` bytes, which is checked before the read.
    unsafe {
        let args = &*escape;
        let p = args.pPrivateDriverData as *const u8;
        if p.is_null() || (args.PrivateDriverDataSize as usize) < 8 {
            return 0;
        }
        core::ptr::read_unaligned(p.add(4) as *const u32) & 0x00FF_FFFF
    }
}

fn handle_hint(h: *const c_void) -> u32 {
    (h as usize as u32) & 0x00FF_FFFF
}

// ── PnP / power (StartDevice is deliberately not wrapped: see `lifecycle.rs`) ────────────────────────────────────────────────────────────────────────────
traced!(
    stop_device,
    ddi::STOP_DEVICE,
    crate::ddi::dxgkddi_stop_device,
    (miniport_device_context: *mut c_void),
    0,
    publish
);
traced!(
    remove_device,
    ddi::REMOVE_DEVICE,
    crate::ddi::dxgkddi_remove_device,
    (miniport_device_context: *mut c_void),
    0
);
traced!(
    set_power_state,
    ddi::SET_POWER_STATE,
    crate::ddi::dxgkddi_set_power_state,
    (
        miniport_device_context: *mut c_void,
        device_uid: u32,
        device_power_state: DEVICE_POWER_STATE,
        action_type: POWER_ACTION::Type
    ),
    ((device_uid & 0xFF) << 16)
        | (((device_power_state as u32) & 0xFF) << 8)
        | ((action_type as u32) & 0xFF)
);

// ── devices, contexts, processes ───────────────────────────────────────────────────────────
traced!(
    create_device,
    ddi::CREATE_DEVICE,
    crate::device::dxgkddi_create_device,
    (miniport_device_context: *mut c_void, create_device: *mut DXGKARG_CREATEDEVICE),
    0
);
traced!(
    destroy_device,
    ddi::DESTROY_DEVICE,
    crate::device::dxgkddi_destroy_device,
    (h_device: *mut c_void),
    handle_hint(h_device),
    publish
);
traced!(
    create_context,
    ddi::CREATE_CONTEXT,
    crate::device::dxgkddi_create_context,
    (h_device: *mut c_void, create_context: *mut DXGKARG_CREATECONTEXT),
    handle_hint(h_device)
);
traced!(
    destroy_context,
    ddi::DESTROY_CONTEXT,
    crate::device::dxgkddi_destroy_context,
    (h_context: *mut c_void),
    handle_hint(h_context)
);
traced!(
    create_process,
    ddi::CREATE_PROCESS,
    crate::device::dxgkddi_create_process,
    (miniport_device_context: *mut c_void, args: *mut DXGKARG_CREATEPROCESS),
    0
);
traced!(
    destroy_process,
    ddi::DESTROY_PROCESS,
    crate::device::dxgkddi_destroy_process,
    (miniport_device_context: *mut c_void, h_process: *mut c_void),
    handle_hint(h_process)
);

// ── allocations and paging ─────────────────────────────────────────────────────────────────
traced!(
    create_allocation,
    ddi::CREATE_ALLOCATION,
    crate::ddi::dxgkddi_create_allocation,
    (h_adapter: *mut c_void, create_allocation: *mut DXGKARG_CREATEALLOCATION),
    0
);
traced!(
    destroy_allocation,
    ddi::DESTROY_ALLOCATION,
    crate::ddi::dxgkddi_destroy_allocation,
    (h_adapter: *mut c_void, destroy_allocation: *const DXGKARG_DESTROYALLOCATION),
    0
);
traced!(
    open_allocation,
    ddi::OPEN_ALLOCATION,
    crate::ddi::dxgkddi_open_allocation,
    (h_device: IN_CONST_HANDLE, open_allocation: IN_CONST_PDXGKARG_OPENALLOCATION),
    handle_hint(h_device as *const c_void)
);
traced!(
    close_allocation,
    ddi::CLOSE_ALLOCATION,
    crate::ddi::dxgkddi_close_allocation,
    (h_device: IN_CONST_HANDLE, close_allocation: IN_CONST_PDXGKARG_CLOSEALLOCATION),
    handle_hint(h_device as *const c_void)
);
traced!(
    build_paging_buffer,
    ddi::BUILD_PAGING_BUFFER,
    crate::ddi::dxgkddi_build_paging_buffer,
    (h_adapter: *mut c_void, build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER),
    0
);
traced!(
    map_cpu_host_aperture,
    ddi::MAP_CPU_HOST_APERTURE,
    crate::ddi::dxgkddi_map_cpu_host_aperture,
    (h_adapter: *mut c_void, map: IN_CONST_PDXGKARG_MAPCPUHOSTAPERTURE),
    0
);
traced!(
    unmap_cpu_host_aperture,
    ddi::UNMAP_CPU_HOST_APERTURE,
    crate::ddi::dxgkddi_unmap_cpu_host_aperture,
    (h_adapter: *mut c_void, unmap: IN_CONST_PDXGKARG_UNMAPCPUHOSTAPERTURE),
    0
);

// ── scheduling and TDR ─────────────────────────────────────────────────────────────────────
traced!(
    submit_command,
    ddi::SUBMIT_COMMAND,
    crate::ddi::dxgkddi_submit_command,
    (h_adapter: IN_CONST_HANDLE, submit_command: IN_CONST_PDXGKARG_SUBMITCOMMAND),
    0
);
traced!(
    submit_command_virtual,
    ddi::SUBMIT_COMMAND_VIRTUAL,
    crate::ddi::dxgkddi_submit_command_virtual,
    (h_adapter: *mut c_void, submit_command: *const DXGKARG_SUBMITCOMMANDVIRTUAL),
    0
);
traced!(
    preempt_command,
    ddi::PREEMPT_COMMAND,
    crate::ddi::dxgkddi_preempt_command,
    (h_adapter: *mut c_void, preempt_command: *const DXGKARG_PREEMPTCOMMAND),
    0
);
traced!(
    reset_from_timeout,
    ddi::RESET_FROM_TIMEOUT,
    crate::ddi::dxgkddi_reset_from_timeout,
    (h_adapter: *mut c_void),
    0
);
traced!(
    restart_from_timeout,
    ddi::RESTART_FROM_TIMEOUT,
    crate::ddi::dxgkddi_restart_from_timeout,
    (h_adapter: *mut c_void),
    0
);
traced!(
    reset_engine,
    ddi::RESET_ENGINE,
    crate::ddi::dxgkddi_reset_engine,
    (h_adapter: IN_CONST_HANDLE, reset: INOUT_PDXGKARG_RESETENGINE),
    0
);
traced!(
    query_engine_status,
    ddi::QUERY_ENGINE_STATUS,
    crate::ddi::dxgkddi_query_engine_status,
    (h_adapter: IN_CONST_HANDLE, query: INOUT_PDXGKARG_QUERYENGINESTATUS),
    0
);

// ── render and present ─────────────────────────────────────────────────────────────────────
traced!(
    render,
    ddi::RENDER,
    crate::ddi::dxgkddi_render,
    (h_context: IN_CONST_HANDLE, render: INOUT_PDXGKARG_RENDER),
    handle_hint(h_context as *const c_void)
);
traced!(
    render_km,
    ddi::RENDER_KM,
    crate::ddi::dxgkddi_render_km,
    (h_context: IN_CONST_HANDLE, render: INOUT_PDXGKARG_RENDER),
    handle_hint(h_context as *const c_void)
);
traced!(
    render_gdi,
    ddi::RENDER_GDI,
    crate::ddi::dxgkddi_render_gdi,
    (h_context: IN_CONST_HANDLE, render_gdi: INOUT_PDXGKARG_RENDERGDI),
    handle_hint(h_context as *const c_void)
);
traced!(
    patch,
    ddi::PATCH,
    crate::ddi::dxgkddi_patch,
    (h_adapter: IN_CONST_HANDLE, patch: IN_CONST_PDXGKARG_PATCH),
    0
);
traced!(
    present,
    ddi::PRESENT,
    crate::ddi::dxgkddi_present,
    (h_context: IN_CONST_HANDLE, present: INOUT_PDXGKARG_PRESENT),
    handle_hint(h_context as *const c_void)
);

// ── display ────────────────────────────────────────────────────────────────────────────────
traced!(
    set_vidpn_source_address,
    ddi::SET_VIDPN_SOURCE_ADDRESS,
    crate::ddi::dxgkddi_set_vidpn_source_address,
    (adapter: IN_CONST_HANDLE, address: IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESS),
    0
);
traced!(
    commit_vidpn,
    ddi::COMMIT_VIDPN,
    crate::ddi::dxgkddi_commit_vidpn,
    (adapter: IN_CONST_HANDLE, commit: IN_CONST_PDXGKARG_COMMITVIDPN_CONST),
    0
);
traced!(
    is_supported_vidpn,
    ddi::IS_SUPPORTED_VIDPN,
    crate::ddi::dxgkddi_is_supported_vidpn,
    (adapter: IN_CONST_HANDLE, is_supported: INOUT_PDXGKARG_ISSUPPORTEDVIDPN),
    0
);
traced!(
    update_active_vidpn_present_path,
    ddi::UPDATE_ACTIVE_VIDPN_PATH,
    crate::ddi::dxgkddi_update_active_vidpn_present_path,
    (adapter: IN_CONST_HANDLE, path: IN_CONST_PDXGKARG_UPDATEACTIVEVIDPNPRESENTPATH_CONST),
    0
);
traced!(
    set_vidpn_source_visibility,
    ddi::SET_VIDPN_SOURCE_VISIBILITY,
    crate::ddi::dxgkddi_set_vidpn_source_visibility,
    (adapter: IN_CONST_HANDLE, visibility: IN_CONST_PDXGKARG_SETVIDPNSOURCEVISIBILITY),
    0
);
traced!(
    enum_vidpn_cofunc_modality,
    ddi::ENUM_VIDPN_COFUNC,
    crate::ddi::dxgkddi_enum_vidpn_cofunc_modality,
    (adapter: IN_CONST_HANDLE, enum_modality: IN_CONST_PDXGKARG_ENUMVIDPNCOFUNCMODALITY_CONST),
    0
);
traced!(
    recommend_functional_vidpn,
    ddi::RECOMMEND_FUNCTIONAL_VIDPN,
    crate::ddi::dxgkddi_recommend_functional_vidpn,
    (adapter: IN_CONST_HANDLE, recommend: IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST),
    0
);
traced!(
    query_child_status,
    ddi::QUERY_CHILD_STATUS,
    crate::ddi::dxgkddi_query_child_status,
    (
        miniport_device_context: *mut c_void,
        child_status: *mut DXGK_CHILD_STATUS,
        non_destructive_only: BOOLEAN
    ),
    0
);
traced!(
    query_child_relations,
    ddi::QUERY_CHILD_RELATIONS,
    crate::ddi::dxgkddi_query_child_relations,
    (
        miniport_device_context: *mut c_void,
        child_relations: *mut DXGK_CHILD_DESCRIPTOR,
        child_relations_size: u32
    ),
    0
);

// ── escape, queries, interrupts ────────────────────────────────────────────────────────────
traced!(
    escape,
    ddi::ESCAPE,
    crate::ddi::dxgkddi_escape,
    (h_adapter: *mut c_void, escape: *const DXGKARG_ESCAPE),
    escape_hint(escape)
);
traced!(
    query_adapter_info,
    ddi::QUERY_ADAPTER_INFO,
    crate::ddi::dxgkddi_query_adapter_info,
    (miniport_device_context: *mut c_void, query_adapter_info: *const DXGKARG_QUERYADAPTERINFO),
    0
);
traced!(
    control_interrupt,
    ddi::CONTROL_INTERRUPT,
    crate::ddi::dxgkddi_control_interrupt,
    (
        h_adapter: IN_CONST_HANDLE,
        interrupt_type: IN_CONST_DXGK_INTERRUPT_TYPE,
        enable: IN_BOOLEAN
    ),
    ((interrupt_type as u32) & 0xFF) << 8 | (enable as u32 & 0xFF)
);

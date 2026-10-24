//! The DLSS engine-object channel: two meta-commands, published only with the
//! `Dlss` knob, that hand the engine (vkd3d-proton) objects behind a runtime
//! device or command list to Conduit's dxvk-nvapi build in the same process.
//!
//! NGX's D3D12 path calls `NvAPI_D3D12_CreateCubinComputeShader*`,
//! `NvAPI_D3D12_LaunchCubinShader`, `NvAPI_D3D12_GetCudaTextureObject` and
//! friends on the game's device and command list. dxvk-nvapi implements them
//! with vkd3d-proton's `ID3D12DeviceExt` / `ID3D12GraphicsCommandListExt`
//! (`VK_NVX_binary_import`, `VK_NVX_image_view_handle`), but the game holds
//! Microsoft's runtime objects, whose `QueryInterface` never reaches this
//! driver. Meta-commands are the one IHV channel the runtime forwards:
//!
//! | GUID | stage | parameters (UINT64) | effect |
//! |---|---|---|---|
//! | [`ENGINE_DEVICE`] | creation | `Magic`, `Out` | writes the engine `ID3D12Device*` to `*Out` |
//! | [`ENGINE_COMMAND_LIST`] | creation | `Magic` | — |
//! | [`ENGINE_COMMAND_LIST`] | execution | `Magic`, `Out` | writes the list's engine `ID3D12GraphicsCommandList*` to `*Out`; records nothing |
//!
//! Both pointers are borrowed: the engine objects live as long as the runtime
//! objects they back (`BridgeDevice12`, `CommandListState`), and dxvk-nvapi
//! takes its own reference with `QueryInterface`. `Out` is an address in the
//! calling process, which is this driver's process. Recording forwards every
//! command to the engine list on the calling thread, so a launch dxvk-nvapi
//! records on the engine list lands in order with the game's commands.
//!
//! The other side is `src/shared/helios_d3d12.h` of
//! `third_party/patches/dxvk-nvapi`; keep the GUIDs and layouts in sync.

use core::ffi::c_void;

use windows::core::Interface;

use super::queue;
use crate::{ddi12, device12, knobs12};

/// `{f4c98fb7-a890-49b7-a74d-19896f223e93}`
pub(crate) const ENGINE_DEVICE: ddi12::GUID = ddi12::GUID {
    Data1: 0xf4c9_8fb7,
    Data2: 0xa890,
    Data3: 0x49b7,
    Data4: [0xa7, 0x4d, 0x19, 0x89, 0x6f, 0x22, 0x3e, 0x93],
};

/// `{8c8f8982-ba8b-4eb7-a057-f2336c83d415}`
pub(crate) const ENGINE_COMMAND_LIST: ddi12::GUID = ddi12::GUID {
    Data1: 0x8c8f_8982,
    Data2: 0xba8b,
    Data3: 0x4eb7,
    Data4: [0xa0, 0x57, 0xf2, 0x33, 0x6c, 0x83, 0xd4, 0x15],
};

/// "HELIOS", version 1: the first parameter of every stage that has one.
const MAGIC: u64 = 0x4845_4c49_4f53_0001;

/// What a meta-command slot word holds. 0 is a refused (cleared) slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    EngineDevice = 1,
    EngineCommandList = 2,
}

const fn utf16z<const N: usize>(s: &str) -> [u16; N] {
    let b = s.as_bytes();
    assert!(b.len() < N);
    let mut out = [0u16; N];
    let mut i = 0;
    while i < b.len() {
        out[i] = b[i] as u16;
        i += 1;
    }
    out
}

static NAME_ENGINE_DEVICE: [u16; 19] = utf16z("HeliosEngineDevice");
static NAME_ENGINE_COMMAND_LIST: [u16; 24] = utf16z("HeliosEngineCommandList");
static NAME_MAGIC: [u16; 6] = utf16z("Magic");
static NAME_OUT: [u16; 4] = utf16z("Out");

fn guid_eq(a: &ddi12::GUID, b: &ddi12::GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

/// Is the channel published? The `Dlss` knob, read once per process.
pub(crate) fn enabled() -> bool {
    knobs12::dlss()
}

/// The channel's kind for a meta-command GUID, when the channel is on.
pub(crate) fn kind_of(id: &ddi12::GUID) -> Option<Kind> {
    if !enabled() {
        None
    } else if guid_eq(id, &ENGINE_DEVICE) {
        Some(Kind::EngineDevice)
    } else if guid_eq(id, &ENGINE_COMMAND_LIST) {
        Some(Kind::EngineCommandList)
    } else {
        None
    }
}

/// The kind a created slot holds (its slot word), or `None` for anything else.
///
/// # Safety
/// `p_drv_private`, when non-null, must address the meta-command's private
/// word, which `create_meta_command` wrote (a kind or 0).
pub(crate) unsafe fn kind_of_slot(p_drv_private: *mut c_void) -> Option<Kind> {
    if p_drv_private.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees the word is ours and initialised.
    match unsafe { core::ptr::read_unaligned(p_drv_private.cast::<usize>()) } {
        1 => Some(Kind::EngineDevice),
        2 => Some(Kind::EngineCommandList),
        _ => None,
    }
}

/// `pfnEnumerateMetaCommands` with the channel on: the two descriptors.
///
/// # Safety
/// `p_count` must address a writable `UINT`; `p_descs`, when non-null, an
/// array of `*p_count` writable descriptors.
pub(crate) unsafe fn enumerate(
    p_count: *mut ddi12::UINT,
    p_descs: *mut ddi12::D3D12DDIARG_META_COMMAND_DESC,
) {
    let descs = [
        ddi12::D3D12DDIARG_META_COMMAND_DESC {
            Id: ENGINE_DEVICE,
            Name: NAME_ENGINE_DEVICE.as_ptr() as _,
            InitializationDirtyState: ddi12::D3D12DDI_GRAPHICS_STATES_D3D12DDI_GRAPHICS_STATE_NONE,
            ExecutionDirtyState: ddi12::D3D12DDI_GRAPHICS_STATES_D3D12DDI_GRAPHICS_STATE_NONE,
        },
        ddi12::D3D12DDIARG_META_COMMAND_DESC {
            Id: ENGINE_COMMAND_LIST,
            Name: NAME_ENGINE_COMMAND_LIST.as_ptr() as _,
            InitializationDirtyState: ddi12::D3D12DDI_GRAPHICS_STATES_D3D12DDI_GRAPHICS_STATE_NONE,
            ExecutionDirtyState: ddi12::D3D12DDI_GRAPHICS_STATES_D3D12DDI_GRAPHICS_STATE_NONE,
        },
    ];
    // SAFETY: the caller guarantees both pointers.
    unsafe { write_array(p_count, p_descs, &descs) };
}

/// `pfnEnumerateMetaCommandParameters` for one of the channel's GUIDs.
///
/// # Safety
/// As [`enumerate`], for parameter descriptors.
pub(crate) unsafe fn parameters(
    kind: Kind,
    stage: ddi12::D3D12DDI_META_COMMAND_PARAMETER_STAGE,
    p_count: *mut ddi12::UINT,
    p_descs: *mut ddi12::D3D12DDIARG_META_COMMAND_PARAMETER_DESC,
) {
    let param = |name: &'static [u16]| ddi12::D3D12DDIARG_META_COMMAND_PARAMETER_DESC {
        Name: name.as_ptr() as _,
        Type: ddi12::D3D12DDI_META_COMMAND_PARAMETER_TYPE_D3D12DDI_META_COMMAND_PARAMETER_TYPE_UINT64,
        Flags: ddi12::D3D12DDI_META_COMMAND_PARAMETER_FLAGS_D3D12DDI_META_COMMAND_PARAMETER_FLAG_INPUT,
        RequiredResourceState: 0,
    };
    let magic_out = [param(&NAME_MAGIC), param(&NAME_OUT)];
    let magic = [param(&NAME_MAGIC)];
    let none: [ddi12::D3D12DDIARG_META_COMMAND_PARAMETER_DESC; 0] = [];
    let creation =
        stage == ddi12::D3D12DDI_META_COMMAND_PARAMETER_STAGE_D3D12DDI_META_COMMAND_PARAMETER_STAGE_CREATION;
    let execution =
        stage == ddi12::D3D12DDI_META_COMMAND_PARAMETER_STAGE_D3D12DDI_META_COMMAND_PARAMETER_STAGE_EXECUTION;
    let list: &[ddi12::D3D12DDIARG_META_COMMAND_PARAMETER_DESC] = match (kind, creation, execution) {
        (Kind::EngineDevice, true, _) => &magic_out,
        (Kind::EngineCommandList, true, _) => &magic,
        (Kind::EngineCommandList, _, true) => &magic_out,
        _ => &none,
    };
    // SAFETY: the caller guarantees both pointers.
    unsafe { write_array(p_count, p_descs, list) };
}

/// The enumerate idiom: a null array asks for the count; otherwise `*p_count`
/// is the capacity in and the fill count out.
unsafe fn write_array<T: Copy>(p_count: *mut ddi12::UINT, p_items: *mut T, items: &[T]) {
    let n = if p_items.is_null() {
        items.len()
    } else {
        // SAFETY: the caller guarantees `p_count`.
        let cap = unsafe { core::ptr::read_unaligned(p_count) } as usize;
        let n = cap.min(items.len());
        for (i, item) in items.iter().take(n).enumerate() {
            // SAFETY: `i < cap`, inside the caller's array.
            unsafe { core::ptr::write_unaligned(p_items.add(i), *item) };
        }
        n
    };
    // SAFETY: as above.
    unsafe { core::ptr::write_unaligned(p_count, n as ddi12::UINT) };
}

/// Read `[Magic, Out]` (or `[Magic]` when `want_out` is false) from a parameter
/// blob; `None` if it is short or the magic is wrong.
///
/// # Safety
/// `p`, when non-null, must address `size` readable bytes.
unsafe fn read_params(p: *const c_void, size: usize, want_out: bool) -> Option<u64> {
    let need = if want_out { 16 } else { 8 };
    if p.is_null() || size < need {
        return None;
    }
    let words = p.cast::<u64>();
    // SAFETY: `size >= need` readable bytes.
    if unsafe { core::ptr::read_unaligned(words) } != MAGIC {
        return None;
    }
    if !want_out {
        return Some(0);
    }
    // SAFETY: as above.
    let out = unsafe { core::ptr::read_unaligned(words.add(1)) };
    (out != 0).then_some(out)
}

/// `pfnCreateMetaCommand` for one of the channel's GUIDs. Writes the slot word
/// on success.
///
/// # Safety
/// `h_device` must be live; `p_params` must address `size` readable bytes;
/// `p_drv_private` must address the meta-command's private word.
pub(crate) unsafe fn create(
    kind: Kind,
    h_device: ddi12::D3D12DDI_HDEVICE,
    p_params: *const c_void,
    size: usize,
    p_drv_private: *mut c_void,
) -> ddi12::HRESULT {
    use helios_umd_common::hr::{E_FAIL, E_INVALIDARG, S_OK};
    if p_drv_private.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the caller guarantees the blob.
    let Some(out) = (unsafe { read_params(p_params, size, kind == Kind::EngineDevice) }) else {
        return E_INVALIDARG;
    };
    if kind == Kind::EngineDevice {
        // SAFETY: device-scope DDI; the borrow ends with this call.
        let Some(dev) = (unsafe { device12::device(h_device) }) else {
            return E_FAIL;
        };
        let Some(engine) = dev.engine.d3d12_device() else {
            return E_FAIL;
        };
        // SAFETY: `out` is the caller's own UINT64 in this process (dxvk-nvapi).
        unsafe { core::ptr::write_unaligned(out as *mut u64, engine.as_raw() as u64) };
        static LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::log_error!("DLSS: engine device handed out (HeliosEngineDevice meta-command)");
        }
    }
    // SAFETY: the caller guarantees the word.
    unsafe { core::ptr::write_unaligned(p_drv_private.cast::<usize>(), kind as usize) };
    S_OK
}

/// `pfnExecuteMetaCommand` for a slot of the channel. Records nothing.
///
/// # Safety
/// `h_list` must be a live command list; `p_params` must address `size`
/// readable bytes.
pub(crate) unsafe fn execute(
    kind: Kind,
    h_list: ddi12::D3D12DDI_HCOMMANDLIST,
    p_params: *const c_void,
    size: usize,
) {
    if kind != Kind::EngineCommandList {
        return;
    }
    // SAFETY: the caller guarantees the blob.
    let Some(out) = (unsafe { read_params(p_params, size, true) }) else {
        return;
    };
    // SAFETY: the caller guarantees a live list handle.
    let Some(state) = (unsafe { queue::command_list_state(h_list) }) else {
        return;
    };
    // SAFETY: `out` is the caller's own UINT64 in this process (dxvk-nvapi).
    unsafe { core::ptr::write_unaligned(out as *mut u64, state.engine().as_raw() as u64) };
}

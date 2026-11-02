//! NVIDIA's own primaries on the host display (NVIDIA UMD personality).
//!
//! With NVIDIA's UMDs, DWM's primaries are native allocations: RM video memory
//! the KMD made for an NVDA record (`native_memory`), with no Venus resource.
//! `SetVidPnSourceAddress` of one lands here instead of the Venus arms:
//!
//! 1. The first time an allocation is shown, its RM memory is exported to a
//!    descriptor (`NV0000_CTRL_CMD_EXPORT_OBJECT_TO_FD` into a fresh control
//!    file) and imported on a DRM file the KMD owns
//!    (`DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`), block-linear with the record's
//!    block height and GOB pitch. The GEM handle is kept with the allocation
//!    and closed when it is destroyed.
//! 2. Every flip sets the KMD as scanout 0's source with that layout and sends
//!    a `ScanoutFlip` of the GEM (`foreign_scanout::present_within`): the host
//!    shows NVIDIA's memory as it is, no copy.
//! 3. The flip always completes (`publish_kept_primary`): a host refusal costs
//!    the picture, never DWM's flip queue.
//!
//! Counters: `NvScN` flips seen, `NvScOk` sent, `NvScGem` GEM imports,
//! `NvScFail` failures with `NvScWhy` (the step) and `NvScErr` (its code),
//! `NvScExt` primaries of another size than the mode.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::foreign_resource::MOD_NVIDIA_BLOCK_LINEAR_BASE;
use helios_kmd_logic::foreign_scanout::{Layout, FOURCC_XRGB8888};
use helios_kmd_logic::native_memory as nm;
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_client as rc;

use super::gpu::DeviceOwner;
use super::native_memory::{self, Error, Io};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;

const KMD: DeviceOwner = DeviceOwner::KMD_RM;
const TIMEOUT_MS: u64 = 2_500;
/// How long the source outlives its last flip (an idle desktop flips nothing).
const LAPSE_MS: u32 = 4_000;
const REPLY_MAX: usize = 512;

/// `NvKmsKapiPrivImportMemoryParams`: `memFd`, `layout` (0 block-linear),
/// `log2GobsPerBlock {x, y, z}`, `pitchInBlocks`, `genericMemory` (NvBool).
const NVKMS_LAYOUT_BLOCK_LINEAR: u32 = 0;

/// The KMD's DRM file for native scanout, per transport epoch.
#[derive(Clone, Copy)]
struct Files {
    epoch: u64,
    drm: u32,
}

static FILES: SpinLock<Files> = SpinLock::new(Files { epoch: 0, drm: 0 });

static FLIPS: AtomicU32 = AtomicU32::new(0);
static SENT: AtomicU32 = AtomicU32::new(0);
static GEMS: AtomicU32 = AtomicU32::new(0);
static FAILS: AtomicU32 = AtomicU32::new(0);
static EXTENT: AtomicU32 = AtomicU32::new(0);

/// Which step failed (`NvScWhy`).
#[derive(Clone, Copy)]
enum Step {
    Passive = 1,
    Source = 2,
    SysFiles = 3,
    OpenDrm = 4,
    OpenExport = 5,
    Export = 6,
    Import = 7,
    SetSource = 8,
    Present = 9,
}

fn fail(step: Step, code: u32) {
    let n = FAILS.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvScFail", n);
    crate::diag::record_named_bytes(b"NvScWhy", step as u32);
    crate::diag::record_named_bytes(b"NvScErr", code);
}

/// `SetVidPnSourceAddress` of a native allocation (`native_id` nonzero): show it if
/// the host can, and complete the flip either way. Runs on the scanout worker
/// (PASSIVE) under the scanout lifecycle lock.
pub(crate) fn program(adapter: &AdapterContext, native_id: u32, primary_address: u64, mode: (u32, u32)) {
    let n = FLIPS.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"NvScN", n);
    }
    // Whatever was on screen before is not any more.
    crate::virtio::rm_client::sysmem_flip::other_source(adapter);
    crate::virtio::foreign_flip::other_source(adapter);
    show(adapter, native_id, mode);
    adapter.publish_kept_primary(primary_address);
}

fn show(adapter: &AdapterContext, native_id: u32, mode: (u32, u32)) {
    let Some(passive) = PassiveLevel::try_assume() else {
        fail(Step::Passive, 0);
        return;
    };
    let source = match native_memory::scanout_source(native_id) {
        Ok(source) => source,
        Err(error) => {
            fail(Step::Source, error as u32);
            return;
        }
    };
    let s = source.surface;
    if (s.width, s.height) != mode {
        let n = EXTENT.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvScExt", n);
        crate::diag::record_named_bytes(b"NvScWH", (s.width << 16) | (s.height & 0xffff));
        return;
    }
    let prepared = native_memory::with_io(passive, adapter, |io| {
        let drm = drm_file(io)?;
        // Read again under the I/O mutex: the allocation may have been destroyed
        // (or given a GEM) since `scanout_source` above.
        let current = native_memory::scanout_source(native_id)?;
        let gem = if current.gem != 0 {
            current.gem
        } else {
            let gem = import(io, drm, &current)?;
            if let Err(error) = native_memory::set_scanout_gem(native_id, gem) {
                // Nothing would ever close it.
                close_gem(io, drm, gem);
                return Err(error);
            }
            let g = GEMS.fetch_add(1, Ordering::Relaxed) + 1;
            crate::diag::record_named_bytes(b"NvScGem", g);
            gem
        };
        Ok((drm, gem, io.client.epoch))
    });
    let Ok((drm, gem, epoch)) = prepared else {
        return;
    };
    let layout = Layout {
        width: s.width,
        height: s.height,
        stride: s.pitch,
        offset: 0,
        fourcc: FOURCC_XRGB8888,
        modifier: MOD_NVIDIA_BLOCK_LINEAR_BASE | u64::from(s.log2_gobs_y),
    };
    if let Err(e) = adapter.foreign_scanout_set(KMD, drm, epoch, layout, LAPSE_MS) {
        fail(Step::SetSource, match e {
            helios_kmd_logic::foreign_scanout::SetError::Busy => 1,
            helios_kmd_logic::foreign_scanout::SetError::Layout(_) => 2,
        });
        return;
    }
    match crate::virtio::foreign_scanout::present_within(passive, adapter, KMD, drm, gem, TIMEOUT_MS) {
        Ok(_) => {
            let n = SENT.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 64 == 0 {
                crate::diag::record_named_bytes(b"NvScOk", n);
            }
        }
        Err(_) => fail(Step::Present, 0),
    }
}

/// The KMD's DRM file on the GPU's render node, opened once per epoch.
fn drm_file(io: &Io<'_>) -> Result<u32, Error> {
    let epoch = io.client.epoch;
    {
        let f = FILES.lock();
        if f.epoch == epoch && f.drm != 0 {
            return Ok(f.drm);
        }
    }
    let index = dri_index(io).inspect_err(|e| fail(Step::SysFiles, *e as u32))?;
    let drm = io
        .open_file(rc::DEV_DRI_BASE.saturating_add(index))
        .inspect_err(|e| fail(Step::OpenDrm, *e as u32))?;
    *FILES.lock() = Files { epoch, drm };
    Ok(drm)
}

fn dri_index(io: &Io<'_>) -> Result<u32, Error> {
    let mut req = [0u8; 32];
    let n = rc::build_get_sys_files(&mut req).ok_or(Error::Invalid)?;
    let mut resp = Vec::<u8>::new();
    resp.try_reserve_exact(rc::SYS_FILES_CAP)
        .map_err(|_| Error::NoMemory)?;
    resp.resize(rc::SYS_FILES_CAP, 0);
    let len = io.send(req.get(..n).ok_or(Error::Invalid)?, &mut resp)?;
    let stream = resp.get(..len).ok_or(Error::Host)?;
    let mut nodes = [rc::DriNode::default(); rc::MAX_DRI];
    let k = rc::parse_dri_section(stream, &mut nodes);
    rc::pick_dri(nodes.get(..k).unwrap_or(&[]), io.client.gpu_id).ok_or(Error::Unsupported)
}

/// Export the allocation's RM memory and import it as a GEM object on `drm`.
fn import(io: &Io<'_>, drm: u32, source: &native_memory::ScanoutSource) -> Result<u32, Error> {
    let ch = io
        .open_file(rc::DEV_CTL)
        .inspect_err(|e| fail(Step::OpenExport, *e as u32))?;
    let result = export_and_import(io, drm, ch, source);
    let _ = io.close_file(ch);
    result
}

fn export_and_import(
    io: &Io<'_>,
    drm: u32,
    ch: u32,
    source: &native_memory::ScanoutSource,
) -> Result<u32, Error> {
    let root = io.client.root;
    let params = rc::export_params(nd::H_DEVICE, source.memory, ch);
    let block = rc::nvos54(root, root, rc::NV0000_CTRL_CMD_EXPORT_OBJECT_TO_FD, params.len() as u32);
    let mut resp = [0u8; REPLY_MAX];
    let n = io
        .exchange(io.client.ctl, rc::nv_cmd(rc::ESC_RM_CONTROL, 32), &block, &params, &mut resp)
        .inspect_err(|e| fail(Step::Export, 0x100 | *e as u32))?;
    rc::rm_reply(resp.get(..n).ok_or(Error::Host)?, rc::NVOS54_STATUS_AT).map_err(|e| {
        fail(Step::Export, rm_code(e));
        Error::Host
    })?;
    let data = rc::gem_import_params(source.size);
    let nested = nvkms_block_linear(ch, &source.surface);
    let mut resp = [0u8; REPLY_MAX];
    let n = io
        .exchange(drm, rc::DRM_IOCTL_GEM_IMPORT_NVKMS, &data, &nested, &mut resp)
        .inspect_err(|e| fail(Step::Import, 0x100 | *e as u32))?;
    let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Error::Host)?).map_err(|e| {
        fail(
            Step::Import,
            match e {
                rc::ReplyError::Host(s) => 0x1000_0000 | s.unsigned_abs(),
                rc::ReplyError::Short => 0x3000_0000,
            },
        );
        Error::Host
    })?;
    rc::gem_handle(&reply).ok_or_else(|| {
        fail(Step::Import, 0x2000_0000);
        Error::Host
    })
}

fn rm_code(e: rc::RmError) -> u32 {
    match e {
        rc::RmError::Status(s) => s,
        rc::RmError::Host(errno) => 0x1000_0000 | errno,
        rc::RmError::Malformed => 0x3000_0000,
    }
}

fn nvkms_block_linear(mem_fd: u32, s: &nm::D3dSurface) -> [u8; rc::NVKMS_IMPORT_BYTES] {
    let mut a = [0u8; rc::NVKMS_IMPORT_BYTES];
    a[0..4].copy_from_slice(&mem_fd.to_le_bytes());
    a[4..8].copy_from_slice(&NVKMS_LAYOUT_BLOCK_LINEAR.to_le_bytes());
    // log2GobsPerBlock.x = 0, .y, .z = 0.
    a[12..16].copy_from_slice(&s.log2_gobs_y.to_le_bytes());
    // pitchInBlocks: one GOB (64 bytes) per block across.
    a[20..24].copy_from_slice(&(s.pitch / 64).to_le_bytes());
    a[24] = 1; // genericMemory
    a
}

/// Close a scanout GEM handle (the allocation is being destroyed). Ends the KMD's
/// source first, so no later flip names a closed object.
pub(crate) fn release_gem(io: &Io<'_>, gem: u32) {
    let drm = {
        let f = FILES.lock();
        if f.epoch != io.client.epoch {
            return;
        }
        f.drm
    };
    if drm == 0 {
        return;
    }
    let _ = io.adapter.foreign_scanout_release(KMD, Some(drm));
    close_gem(io, drm, gem);
}

fn close_gem(io: &Io<'_>, drm: u32, gem: u32) {
    let data = rc::gem_close_params(gem);
    let mut resp = [0u8; REPLY_MAX];
    let _ = io.exchange(drm, rc::DRM_IOCTL_GEM_CLOSE, &data, &[], &mut resp);
}

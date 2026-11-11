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
//!
//! With `NvHotPath` 0x8 the `ScanoutFlip` is SUBMITTED (`present_submit`) and the
//! scanout worker goes on; the reply is settled at the next native flip ([`settle`])
//! through a `flip_pipeline::Pipeline` of window 2, with the
//! bookkeeping the round trip's return does (`note_async_result`). A full window
//! or queue sends that flip the synchronous way. `NvScAsN` submitted, `NvScAsAck`
//! taken, `NvScAsFail` refused, `NvScAsTmo` timed out, `NvScAsSync` sent
//! synchronously for a full window or queue.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::flip_pipeline::{self as fp, Ack, Pipeline, Settled};

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

/// The pipelined native flips (`NvHotPath` 0x8): at most this many in flight.
const ASYNC_WINDOW: u8 = 2;
/// How long the host gets to answer one (100 ns), as the round trip's timeout.
const ASYNC_TIMEOUT_100NS: u64 = TIMEOUT_MS * 10_000;
static PIPE: SpinLock<Pipeline> = SpinLock::new(Pipeline::new());
static CELLS: [AtomicU64; fp::MAX_CELLS] = [const { AtomicU64::new(0) }; fp::MAX_CELLS];
static AS_SENT: AtomicU32 = AtomicU32::new(0);
static AS_ACK: AtomicU32 = AtomicU32::new(0);
static AS_FAIL: AtomicU32 = AtomicU32::new(0);
static AS_TMO: AtomicU32 = AtomicU32::new(0);
static AS_SYNC: AtomicU32 = AtomicU32::new(0);

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

/// `NvScN` (flips seen) and `NvScOk` (sent), written by the registry mirror's pass
/// (`ddi::hot_path::publish`) instead of every 64th flip. PASSIVE.
pub(crate) fn publish_hot() {
    let flips = FLIPS.load(Ordering::Relaxed);
    if flips != 0 {
        crate::diag::record_named_bytes(b"NvScN", flips);
        crate::diag::record_named_bytes(b"NvScOk", SENT.load(Ordering::Relaxed));
    }
    let sent = AS_SENT.load(Ordering::Relaxed);
    if sent != 0 || AS_SYNC.load(Ordering::Relaxed) != 0 {
        let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
        crate::diag::record_named_bytes(b"NvScAsN", sent);
        crate::diag::record_named_bytes(b"NvScAsAck", r(&AS_ACK));
        crate::diag::record_named_bytes(b"NvScAsFail", r(&AS_FAIL));
        crate::diag::record_named_bytes(b"NvScAsTmo", r(&AS_TMO));
        crate::diag::record_named_bytes(b"NvScAsSync", r(&AS_SYNC));
    }
}

/// Settle the pipelined native flips whose answers came (or whose time ran out): the
/// release book and the source are told what the round trip's return would have told
/// them. PASSIVE, on the scanout worker only (before each native flip, under the
/// scanout lifecycle lock, where the round trip's return ran: a settled flip can
/// restore the desktop source). The last flip before an idle desktop is settled at the
/// next one; until then the source's lapse runs from its mint, as it does for a flip
/// in flight. One lock check while nothing is in flight.
pub(crate) fn settle(adapter: &AdapterContext) {
    use super::foreign_scanout::{note_async_result, AsyncResult};
    let t = crate::adapter::foreign_scanout::now_100ns();
    let mut words = [0u64; fp::MAX_CELLS];
    for (w, c) in words.iter_mut().zip(CELLS.iter()) {
        *w = c.load(Ordering::Acquire);
    }
    let batch = {
        let mut g = PIPE.lock();
        if g.occupied() == 0 {
            return;
        }
        g.settle_all(&words, t, ASYNC_TIMEOUT_100NS)
    };
    for item in batch.iter() {
        match item.settled {
            Settled::Pending | Settled::Orphaned { .. } => {}
            Settled::Acked { slot, ack } => {
                let r = if ack == Ack::Taken {
                    AS_ACK.fetch_add(1, Ordering::Relaxed);
                    SENT.fetch_add(1, Ordering::Relaxed);
                    AsyncResult::Taken
                } else {
                    AS_FAIL.fetch_add(1, Ordering::Relaxed);
                    fail(Step::Present, 0x100);
                    AsyncResult::Failed
                };
                note_async_result(adapter, slot.seq, slot.generation, r);
            }
            Settled::TimedOut { slot } => {
                AS_TMO.fetch_add(1, Ordering::Relaxed);
                fail(Step::Present, 0x200);
                note_async_result(adapter, slot.seq, slot.generation, AsyncResult::TimedOut);
            }
            Settled::Late { slot, ack } => {
                let r = if ack == Ack::Taken {
                    AsyncResult::LateTaken
                } else {
                    AsyncResult::LateFailed
                };
                note_async_result(adapter, slot.seq, slot.generation, r);
            }
        }
    }
}

/// Submit the flip of `gem` without waiting (`NvHotPath` 0x8). `None`: the window or
/// the queue is full and nothing was sent (send it the synchronous way); `Some(ok)`:
/// submitted, or refused before the ring.
fn present_async(passive: PassiveLevel, adapter: &AdapterContext, drm: u32, gem: u32, epoch: u64) -> Option<bool> {
    use super::foreign_scanout::{present_submit, PresentRefusal};
    settle(adapter);
    let i = {
        let mut g = PIPE.lock();
        if g.window() != ASYNC_WINDOW {
            g.set_window(ASYNC_WINDOW);
        }
        g.free_slot()?
    };
    let at = crate::adapter::foreign_scanout::now_100ns();
    match present_submit(passive, adapter, KMD, drm, gem, &CELLS[i]) {
        Ok((seq, generation)) => {
            // The only submitter is the scanout worker: cell `i` is still free.
            let _ = PIPE.lock().begin_at(i, seq, at, generation, epoch, 0, false);
            AS_SENT.fetch_add(1, Ordering::Relaxed);
            Some(true)
        }
        Err(PresentRefusal::Device(super::VirtioError::QueueFull)) => None,
        Err(_) => Some(false),
    }
}

/// A primary that was on the host's screen is being destroyed (`NvScRel`).
pub(crate) fn note_shown_destroyed() {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvScRel", n);
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
    // `NvScN`: published by the mirror pass (`publish_hot`).
    FLIPS.fetch_add(1, Ordering::Relaxed);
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
    // `NvScDbg*`: the last 8 shown primaries: id, RM memory, where its GPU mappings point.
    {
        static SLOT: AtomicU32 = AtomicU32::new(0);
        let i = SLOT.fetch_add(1, Ordering::Relaxed) & 7;
        if let Some((memory, maps, sys, backed, res)) = native_memory::scanout_debug(native_id) {
            let mut name = *b"NvScDbg0I";
            name[7] = b'0' + i as u8;
            crate::diag::record_named_bytes(&name, native_id);
            name[8] = b'M';
            crate::diag::record_named_bytes(&name, memory);
            name[8] = b'P';
            crate::diag::record_named_bytes(
                &name,
                (maps & 0xfff) | ((sys & 0xfff) << 12) | (u32::from(backed) << 24) | (res << 28),
            );
            name[8] = b'G';
            crate::diag::record_named_bytes(&name, source.gem);
        }
    }
    let s = source.surface;
    if (s.width, s.height) != mode {
        let n = EXTENT.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvScExt", n);
        crate::diag::record_named_bytes(b"NvScWH", (s.width << 16) | (s.height & 0xffff));
        return;
    }
    // `NvHotPath` 0x1: a primary already imported on this epoch's DRM file needs no
    // host call to prepare, so it does not wait for the native I/O mutex, which a
    // leaf update holds across its host round trips (`HpFlipFast` / `HpFlipSlow`).
    // The GEM can be closed by a destroy right after this read exactly as after the
    // mutex is released below; the host then refuses the flip (`NvScFail`).
    let fast = if crate::ddi::hot_path::knob(passive).fast_flip() {
        native_memory::scanout_imported(native_id).and_then(|(gem, epoch)| {
            let f = FILES.lock();
            (f.epoch == epoch && f.drm != 0).then_some((f.drm, gem, epoch))
        })
    } else {
        None
    };
    let prepared = if let Some(ready) = fast {
        crate::ddi::hot_path::count(&crate::ddi::hot_path::FLIP_FAST);
        Ok(ready)
    } else {
        crate::ddi::hot_path::count(&crate::ddi::hot_path::FLIP_SLOW);
        native_memory::with_io(passive, adapter, |io| {
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
        })
    };
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
    // `NvHotPath` 0x8: submitted, settled later (`settle`); `NvScOk` then counts at the answer.
    if crate::ddi::hot_path::knob(passive).async_flip() {
        match present_async(passive, adapter, drm, gem, epoch) {
            Some(true) => return,
            Some(false) => {
                fail(Step::Present, 0x300);
                return;
            }
            None => {
                AS_SYNC.fetch_add(1, Ordering::Relaxed);
            }
        }
    } else if PIPE.lock().occupied() != 0 {
        // The knob went off with flips in flight (a StartDevice re-read): settle them.
        settle(adapter);
    }
    match crate::virtio::foreign_scanout::present_within(passive, adapter, KMD, drm, gem, TIMEOUT_MS) {
        Ok(_) => {
            // `NvScOk`: published by the mirror pass (`publish_hot`).
            SENT.fetch_add(1, Ordering::Relaxed);
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

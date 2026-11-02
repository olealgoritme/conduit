//! Native NVIDIA RM memory service for the 610.88 Windows UMD personality.
//!
//! This path does not create, import, or adopt a GEM/Venus object.  The parent
//! supplies an already brought-up host RM client (`NativeClient`); this module
//! adds a `FERMI_VASPACE_A(GPU_DEVICE)`, real memory objects, and fixed
//! `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` mappings at VidMm's exact VAs.
//!
//! Locking: one PASSIVE mutex serializes every RM operation and every lifetime
//! change.  The leaf spinlock is used only to copy or publish plain state; it is
//! never held over a host round trip, an MDL operation, or an allocation.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::ce_route as cr;
use helios_kmd_logic::native_memory as nm;
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client as rc;
use wdk_sys::ntddk::{
    MmMapIoSpace, MmMapLockedPagesSpecifyCache, MmUnmapIoSpace, MmUnmapLockedPages,
};
use wdk_sys::{MDL, NTSTATUS, PHYSICAL_ADDRESS, PMDL, _MEMORY_CACHING_TYPE};

use super::gpu::DeviceOwner;
use super::nvrm::{self, Refusal};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::{PassiveMutex, PassiveMutexGuard, SpinLock};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;
const TIMEOUT_MS: u64 = 2_500;
const REPLY_MAX: usize = 512;
const H_VASPACE: u32 = 0x4e57_0001;
const FIRST_DYNAMIC_HANDLE: u32 = 0x4e57_1000;
const PIN_CHUNK_BYTES: u64 = u32::MAX as u64 & !(nm::PAGE_BYTES - 1);
const LOCAL_4K_MAP_FLAGS: u32 = 1 << 8;

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// Replacements whose RM idle-channels quiesce was refused (`update_leaf`).
static QUIESCE_REFUSED: AtomicU32 = AtomicU32::new(0);
static IO_MUTEX: SpinLock<Option<&'static PassiveMutex>> = SpinLock::new(None);
static STATE: SpinLock<State> = SpinLock::new(State::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotReady,
    Busy,
    Invalid,
    Unsupported,
    NoMemory,
    Host,
    WrongAddress,
    Overlap,
    Poisoned,
}

impl Error {
    pub const fn ntstatus(self) -> NTSTATUS {
        match self {
            Self::NotReady => wdk_sys::STATUS_DEVICE_NOT_READY,
            Self::Busy => wdk_sys::STATUS_DEVICE_BUSY,
            Self::Invalid | Self::Overlap => wdk_sys::STATUS_INVALID_PARAMETER,
            Self::Unsupported => wdk_sys::STATUS_NOT_SUPPORTED,
            Self::NoMemory => wdk_sys::STATUS_NO_MEMORY,
            Self::Host | Self::WrongAddress | Self::Poisoned => wdk_sys::STATUS_IO_DEVICE_ERROR,
        }
    }
}

/// Base RM objects supplied by the parent's normal root/device/subdevice
/// bring-up.  No handle is guessed or borrowed from a process client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeClient {
    pub epoch: u64,
    pub ctl: u32,
    pub gpu: u32,
    pub root: u32,
    pub minor: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Created {
    pub id: u32,
    pub size: u64,
    pub segment: u32,
    pub system_memory: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuView {
    pub address: u64,
    pub len: u64,
    /// Zero for the direct `pSystemMem` view.  Aperture unmaps use this value
    /// only as a generation check for RM-backed views.
    pub generation: u32,
}

#[derive(Clone, Copy)]
struct HostMapping {
    process: usize,
    piece: nm::MappingPiece,
    virt: u32,
    /// The OS-descriptor memory object of VidMm's system pages this mapping
    /// shows (`update_leaf_system`), 0 when it maps the allocation's RM memory.
    desc: u32,
}

struct Pin {
    mdl: usize,
    pages: usize,
    kernel_va: u64,
}

struct CpuChunk {
    physical: u64,
    map_ch: u32,
    cookie: u64,
    rm_mapped: bool,
    host_id: u32,
}

struct CpuMapping {
    view: CpuView,
    chunks: Vec<CpuChunk>,
    kernel_va: u64,
    aperture_offset: Option<u64>,
}

struct Allocation {
    id: u32,
    epoch: u64,
    memory: u32,
    size: u64,
    system_memory: u64,
    system_kernel_va: u64,
    pins: Vec<Pin>,
    mappings: Vec<HostMapping>,
    cpu: Option<CpuMapping>,
    resident: Option<(u64, bool)>,
    poisoned: bool,
    /// What CreateAllocation wrote into the NVDA record. dxgkrnl hands the UMD
    /// the record the KMD's OpenAllocation writes (create's copy is dropped),
    /// so the same values are written again at open (`writeback_open`).
    writeback: Option<Writeback>,
}

/// The record fields CreateAllocation wrote, re-applied at open.
#[derive(Clone, Copy)]
enum Writeback {
    /// CUDA/Vulkan records: RM handles and 64-KiB-unit sizes.
    Native(nm::PrivateWriteback),
    /// D3D11/D3D12 records: the 610.88 KMD's field set (`nm::d3d_writeback`).
    D3d([(usize, u32); nm::D3D_WRITEBACK_FIELDS]),
}

impl Writeback {
    fn apply(&self, bytes: &mut [u8]) -> Result<(), nm::PrivateError> {
        match self {
            Self::Native(w) => nm::writeback_allocation_private(bytes, w),
            Self::D3d(fields) => nm::apply_fields(bytes, fields),
        }
    }
}

struct State {
    client: Option<NativeClient>,
    allocations: Vec<Allocation>,
    spaces: Vec<(usize, u32)>,
    /// Per-process space -> its low window (see [`LOW_WINDOW_BASE`]).
    windows: Vec<(u32, u32)>,
    /// System pages of NON-native allocations (dxgkrnl's monitored fences and
    /// the like) mapped into a process's space (`update_foreign_leaf`).
    foreign: Vec<HostMapping>,
    /// The system pages behind each `update_leaf_system` descriptor (for CPU
    /// access to what the GPU sees there, `patch_gpu_bytes`).
    desc_pfns: Vec<(u32, Vec<u64>)>,
    orphan_pins: Vec<Pin>,
    idle_spaces: Vec<usize>,
    next_handle: u32,
}

impl State {
    const fn new() -> Self {
        Self {
            client: None,
            allocations: Vec::new(),
            spaces: Vec::new(),
            windows: Vec::new(),
            foreign: Vec::new(),
            desc_pfns: Vec::new(),
            orphan_pins: Vec::new(),
            idle_spaces: Vec::new(),
            next_handle: FIRST_DYNAMIC_HANDLE,
        }
    }

    fn client(&self) -> Result<NativeClient, Error> {
        self.client.ok_or(Error::NotReady)
    }

    fn allocation(&self, id: u32) -> Result<&Allocation, Error> {
        self.allocations
            .iter()
            .find(|a| a.id == id)
            .ok_or(Error::Invalid)
    }

    fn allocation_mut(&mut self, id: u32) -> Result<&mut Allocation, Error> {
        self.allocations
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or(Error::Invalid)
    }

    fn mint(&mut self) -> Result<u32, Error> {
        let out = self.next_handle;
        if out == 0 || out >= 0x4e5f_ffff {
            return Err(Error::NoMemory);
        }
        self.next_handle = out.checked_add(1).ok_or(Error::NoMemory)?;
        Ok(out)
    }
}

fn mutex(passive: PassiveLevel) -> Result<&'static PassiveMutex, Error> {
    if let Some(mutex) = *IO_MUTEX.lock() {
        return Ok(mutex);
    }
    let mutex = Box::leak(Box::new(
        PassiveMutex::try_new(passive).ok_or(Error::NoMemory)?,
    ));
    let mut slot = IO_MUTEX.lock();
    if let Some(existing) = *slot {
        return Ok(existing);
    }
    *slot = Some(mutex);
    Ok(mutex)
}

fn lock(passive: PassiveLevel) -> Result<PassiveMutexGuard<'static>, Error> {
    mutex(passive)?.lock(passive).ok_or(Error::Busy)
}

struct Io<'a> {
    passive: PassiveLevel,
    adapter: &'a AdapterContext,
    client: NativeClient,
}

impl Io<'_> {
    fn send(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, Error> {
        let mut seen = None;
        let n = nvrm::forward(
            self.passive,
            self.adapter,
            KMD,
            0,
            req,
            resp,
            TIMEOUT_MS,
            0,
            0,
            &mut seen,
        )
        .map_err(|_| Error::Host)?;
        if seen.is_some_and(|epoch| epoch != self.client.epoch) {
            return Err(Error::NotReady);
        }
        Ok(n)
    }

    fn exchange(
        &self,
        file: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, Error> {
        let total = rc::MSG_HDR + rc::IOCTL_REQ + data.len() + nested.len();
        let mut request = Vec::new();
        request
            .try_reserve_exact(total)
            .map_err(|_| Error::NoMemory)?;
        request.resize(total, 0);
        let n = rc::build_ioctl(&mut request, file, cmd, data, nested).ok_or(Error::Invalid)?;
        self.send(request.get(..n).ok_or(Error::Invalid)?, resp)
    }

    fn rm_alloc(
        &self,
        parent: u32,
        object: u32,
        class: u32,
        params: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let block = rc::nvos64(self.client.root, parent, object, class, params.len() as u32);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            self.client.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            params,
            &mut response,
        )?;
        let reply = rc::parse_ioctl_reply(response.get(..n).ok_or(Error::Host)?)
            .map_err(|_| Error::Host)?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS64_STATUS_AT).map_err(
            |error| {
                // NvRmAlCls: the refused class; NvRmAlSt: NV_STATUS, 0x8000_0000|errno
                // for a host refusal, 0xffff_ffff for a malformed reply.
                crate::diag::record_named_bytes(b"NvRmAlCls", class);
                crate::diag::record_named_bytes(b"NvRmAlSt", rm_error_code(error));
                Error::Host
            },
        )?;
        Ok(reply.nested.to_vec())
    }

    fn rm_free(&self, parent: u32, object: u32) -> Result<(), Error> {
        let block = rc::nvos00(self.client.root, parent, object);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            self.client.ctl,
            rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
            &block,
            &[],
            &mut response,
        )?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS00_STATUS_AT)
            .map(|_| ())
            .map_err(|_| Error::Host)
    }

    fn open_file(&self, device_type: u32) -> Result<u32, Error> {
        let mut request = [0u8; 32];
        let n = rc::build_open(&mut request, device_type).ok_or(Error::Invalid)?;
        let mut response = [0u8; 64];
        let len = self.send(request.get(..n).ok_or(Error::Invalid)?, &mut response)?;
        rc::parse_open_reply(response.get(..len).ok_or(Error::Host)?).ok_or(Error::Host)
    }

    fn close_file(&self, handle: u32) -> Result<(), Error> {
        if handle == 0 {
            return Ok(());
        }
        let mut request = [0u8; 32];
        let n = rc::build_close(&mut request, handle).ok_or(Error::Invalid)?;
        let mut response = [0u8; 64];
        let len = self.send(request.get(..n).ok_or(Error::Invalid)?, &mut response)?;
        response
            .get(..len)
            .and_then(rc::reply_status)
            .filter(|status| *status == 0)
            .map(|_| ())
            .ok_or(Error::Host)
    }

    fn register_fd(&self, channel: u32) -> Result<(), Error> {
        let params = rc::register_fd_params(self.client.ctl);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            channel,
            rc::nv_cmd(rc::ESC_REGISTER_FD, 4),
            &params,
            &[],
            &mut response,
        )?;
        rc::parse_ioctl_reply(response.get(..n).ok_or(Error::Host)?)
            .map(|_| ())
            .map_err(|_| Error::Host)
    }
}

/// Install the base client and its GPU_DEVICE VA space. Per-process spaces are
/// created on demand; the parent's base RM bring-up owns ctl/gpu/root. All VA
/// spaces are freed by the transport-wide client teardown.
pub fn install_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    client: NativeClient,
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    {
        let state = STATE.lock();
        if state.client.is_some() || !state.allocations.is_empty() {
            return Err(Error::Busy);
        }
    }
    let io = Io {
        passive,
        adapter,
        client,
    };
    io.rm_alloc(
        nd::H_DEVICE,
        H_VASPACE,
        cc::FERMI_VASPACE_A,
        &cc::vaspace_params(),
    )?;
    {
        let mut state = STATE.lock();
        state.spaces.clear();
        state.windows.clear();
        state.foreign.clear();
        state.desc_pfns.clear();
        state
            .spaces
            .try_reserve_exact(1)
            .map_err(|_| Error::NoMemory)?;
    }
    STATE.lock().client = Some(client);
    STATE.lock().spaces.push((0, H_VASPACE));
    Ok(())
}

/// The shared client and VA space used by channel creation. The process token is
/// only an association key; it never selects or borrows a user-mode RM client.
pub fn channel_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
) -> Result<(NativeClient, u32), Error> {
    // Copy the client out first: a guard in an `if let` scrutinee lives through
    // the body (edition 2021), and `ensure_space` takes STATE again on this
    // non-reentrant spinlock (CreateContext spun forever in ensure_space).
    let existing = STATE.lock().client.as_ref().copied();
    if let Some(client) = existing {
        let io = Io {
            passive,
            adapter,
            client,
        };
        let space = ensure_space(&io, process)?;
        return Ok((client, space));
    }
    let client = super::nvda::memory_client(passive, adapter, process)?;
    Ok((client, H_VASPACE))
}

/// Return this process's real VA space, creating it on first use. VidMm's
/// address ranges remain process-scoped; no second process can alias them by
/// sharing a root VA space.
fn ensure_space(io: &Io<'_>, process: usize) -> Result<u32, Error> {
    if let Some((_, space)) = STATE
        .lock()
        .spaces
        .iter()
        .find(|(owner, _)| *owner == process)
    {
        return Ok(*space);
    }
    STATE
        .lock()
        .spaces
        .try_reserve_exact(1)
        .map_err(|_| Error::NoMemory)?;
    let space = STATE.lock().mint()?;
    // NvVaDevice=1 (experiment): the device's own VA space (index GPU_DEVICE,
    // shared by every process, no window) as NVK on RM uses, to tell whether
    // the new per-process spaces are what the graphics copy engine fails on.
    let device_space =
        crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvVaDevice"), 0) == 1;
    let reply = io.rm_alloc(
        nd::H_DEVICE,
        space,
        cc::FERMI_VASPACE_A,
        &if device_space { cc::vaspace_params() } else { cc::vaspace_params_vidmm() },
    )?;
    if device_space {
        STATE.lock().spaces.push((process, space));
        return Ok(space);
    }
    // Diagnosis: the space RM made (vaSize at +8, vaBase at +40 of the
    // returned NV_VASPACE_ALLOCATION_PARAMETERS), in 64-KiB units.
    crate::diag::record_named_bytes(
        b"NvSpaceSz",
        (rc::get64(&reply, 8).unwrap_or(u64::MAX) >> 16) as u32,
    );
    crate::diag::record_named_bytes(
        b"NvSpaceBase",
        (rc::get64(&reply, 40).unwrap_or(u64::MAX) >> 16) as u32,
    );
    // One virtual object over all of VidMm's low VAs, before RM maps anything
    // of its own into the space: RM's first-fit internal mappings (GR context
    // buffers) then land above 4.5 GiB, in the [4 GiB, 8 GiB) range the KMD
    // reserves from VidMm, and every VidMm VA below 4 GiB is mapped inside
    // this window at its exact address.
    let window = match STATE.lock().mint() {
        Ok(window) => window,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, space);
            return Err(error);
        }
    };
    let params = cc::virtual_params(
        io.client.root,
        space,
        Some(LOW_WINDOW_BASE),
        LOW_WINDOW_END - LOW_WINDOW_BASE,
    );
    if let Err(error) = io.rm_alloc(nd::H_DEVICE, window, cc::NV50_MEMORY_VIRTUAL, &params) {
        let _ = io.rm_free(nd::H_DEVICE, space);
        return Err(error);
    }
    {
        let mut state = STATE.lock();
        state.spaces.push((process, space));
        let _ = state.windows.try_reserve(1);
        state.windows.push((space, window));
    }
    crate::diag::record_named_bytes(b"NvSpaceProc", process as u32);
    crate::diag::record_named_bytes(b"NvSpaceH", space);
    // Diagnosis: where RM puts a non-fixed 2-MiB virtual object now (in 64-KiB
    // units; 0xffffffff = refused), i.e. where its own mappings would go.
    if let Ok(probe) = STATE.lock().mint() {
        let params = cc::virtual_params(io.client.root, space, None, 0x20_0000);
        let at = match io.rm_alloc(nd::H_DEVICE, probe, cc::NV50_MEMORY_VIRTUAL, &params) {
            Ok(reply) => {
                let _ = io.rm_free(nd::H_DEVICE, probe);
                (rc::get64(&reply, 80).unwrap_or(u64::MAX) >> 16) as u32
            }
            Err(_) => u32::MAX,
        };
        crate::diag::record_named_bytes(b"NvSpaceFree", at);
    }
    Ok(space)
}

/// The low window of a per-process space: [64 KiB, 4 GiB), VidMm's fences
/// (0x10000) and the UMDs' low VAs included.
const LOW_WINDOW_BASE: u64 = 0x1_0000;
const LOW_WINDOW_END: u64 = 0x1_0000_0000;

fn window_of(space: u32) -> Option<u32> {
    STATE
        .lock()
        .windows
        .iter()
        .find(|(owner, _)| *owner == space)
        .map(|(_, window)| *window)
}

fn is_window(virt: u32) -> bool {
    STATE.lock().windows.iter().any(|(_, window)| *window == virt)
}


/// Validate a UMD GPFIFO allocation before it becomes a hardware queue. The
/// entire ring must be covered by one live native mapping at the exact VA.
pub fn ring_backing(
    id: u32,
    gpfifo_va: u64,
    ring_bytes: u64,
) -> Result<super::native_channel::NativeRingBacking, Error> {
    let state = STATE.lock();
    let allocation = state.allocation(id)?;
    let end = gpfifo_va.checked_add(ring_bytes).ok_or(Error::Invalid)?;
    if ring_bytes == 0 || ring_bytes > allocation.size {
        return Err(Error::Invalid);
    }
    let Some(mapping) = allocation.mappings.iter().find(|mapping| {
        mapping.piece.va <= gpfifo_va
            && mapping
                .piece
                .end()
                .is_some_and(|mapping_end| mapping_end >= end)
    }) else {
        // MakeResident is asynchronous: the UMD creates its queue before VidMm
        // has paged the ring in and updated its PTEs. RM's channel needs only
        // the GPFIFO VA, which VidMm maps before any submission runs; the
        // VA-space match is checked once mapped (va_space 0 = not yet).
        return Ok(super::native_channel::NativeRingBacking {
            memory: allocation.memory,
            mapped_bytes: ring_bytes,
            va_space: 0,
            id,
            offset: 0,
        });
    };
    let va_space = state
        .spaces
        .iter()
        .find(|(process, _)| *process == mapping.process)
        .map(|(_, space)| *space)
        .ok_or(Error::NotReady)?;
    Ok(super::native_channel::NativeRingBacking {
        memory: allocation.memory,
        mapped_bytes: ring_bytes,
        va_space,
        id,
        offset: mapping.piece.memory_offset + (gpfifo_va - mapping.piece.va),
    })
}

fn local_params(root: u32, size: u64) -> [u8; rc::MEM_ALLOC_BYTES] {
    rc::mem_alloc_params(
        root,
        &rc::SurfaceLayout {
            width: 0,
            height: 0,
            pitch: 0,
            size,
        },
    )
}

extern "C" {
    fn helios_lock_user_pages_seh(virtual_address: *mut c_void, length: u32) -> PMDL;
    fn helios_unlock_system_buffer(mdl: PMDL);
}

fn unlock_pins(pins: &mut Vec<Pin>) {
    for pin in pins.drain(..) {
        if pin.kernel_va != 0 {
            // SAFETY: this exact kernel mapping belongs to the pin and is removed
            // before its MDL is unlocked, at PASSIVE.
            unsafe { MmUnmapLockedPages(pin.kernel_va as *mut c_void, pin.mdl as PMDL) };
        }
        if pin.mdl != 0 {
            // SAFETY: each MDL was returned by helios_lock_user_pages_seh and is
            // owned by this pin.  PASSIVE, outside STATE's spinlock.
            unsafe { helios_unlock_system_buffer(pin.mdl as PMDL) };
        }
    }
}

fn preserve_ambiguous_pins(pins: &mut Vec<Pin>) {
    STATE.lock().orphan_pins.append(pins);
}

fn pin_system_memory(system_memory: u64, size: u64) -> Result<(Vec<Pin>, Vec<u64>), Error> {
    let page_count = usize::try_from(size / nm::PAGE_BYTES).map_err(|_| Error::NoMemory)?;
    let mut pins = Vec::new();
    let mut pfns = Vec::new();
    pfns.try_reserve_exact(page_count)
        .map_err(|_| Error::NoMemory)?;
    let mut offset = 0u64;
    while offset < size {
        let len = (size - offset).min(PIN_CHUNK_BYTES);
        let address = system_memory.checked_add(offset).ok_or(Error::Invalid)?;
        let mdl = unsafe { helios_lock_user_pages_seh(address as *mut c_void, len as u32) };
        if mdl.is_null() {
            unlock_pins(&mut pins);
            return Err(Error::Invalid);
        }
        let pages = usize::try_from(len / nm::PAGE_BYTES).map_err(|_| Error::NoMemory)?;
        // SAFETY: a locked MDL's PFN array follows MDL and has `pages` entries.
        let chunk = unsafe {
            core::slice::from_raw_parts(
                (mdl as *const u8).add(core::mem::size_of::<MDL>()) as *const u64,
                pages,
            )
        };
        pfns.extend_from_slice(chunk);
        let kernel_va = if size <= PIN_CHUNK_BYTES {
            unsafe {
                MmMapLockedPagesSpecifyCache(
                    mdl,
                    0, // KernelMode: this view is valid in every process context
                    _MEMORY_CACHING_TYPE::MmCached,
                    core::ptr::null_mut(),
                    0,
                    16, // NormalPagePriority
                ) as u64
            }
        } else {
            0
        };
        pins.push(Pin {
            mdl: mdl as usize,
            pages,
            kernel_va,
        });
        offset += len;
    }
    if pfns.len() != page_count || pins.iter().map(|p| p.pages).sum::<usize>() != page_count {
        unlock_pins(&mut pins);
        return Err(Error::Invalid);
    }
    Ok((pins, pfns))
}

fn alloc_system_descriptor(io: &Io<'_>, memory: u32, pfns: &[u64], size: u64) -> Result<(), Error> {
    let (kind, deep, big) = nvrm::page_run_table(io.passive, pfns).map_err(|_| Error::NoMemory)?;
    let block = cr::nvos02_osdesc(
        io.client.root,
        nd::H_DEVICE,
        memory,
        pfns.first()
            .copied()
            .unwrap_or(0)
            .wrapping_mul(nm::PAGE_BYTES),
        size,
    );
    let mut request = [0u8; rc::MSG_HDR + rc::IOCTL_REQ + cr::NVOS02_FD_BYTES];
    let n = rc::build_ioctl(
        &mut request,
        io.client.gpu,
        rc::nv_cmd(cr::ESC_RM_ALLOC_MEMORY, cr::NVOS02_FD_BYTES as u32),
        &block,
        &[],
    )
    .ok_or(Error::Invalid)?;
    let mut response = [0u8; REPLY_MAX];
    let sent = nvrm::forward_kmd_registration(
        io.passive,
        io.adapter,
        request.get(..n).ok_or(Error::Invalid)?,
        kind,
        &deep,
        &mut response,
        TIMEOUT_MS,
    );
    let reply = match sent {
        Ok(len) => {
            drop(big);
            response.get(..len).ok_or(Error::Host)?
        }
        Err(Refusal::Transport(VirtioError::Timeout)) => {
            // The host may have consumed the table and retained the registration;
            // the MDLs must remain locked until the client/transport is torn down.
            core::mem::forget(big);
            return Err(Error::Host);
        }
        Err(refusal) => {
            // NvSysHost: 0x2000_0000 | refusal kind for a transport/forward refusal.
            crate::diag::record_named_bytes(b"NvSysHost", 0x2000_0000 | refusal_code(&refusal));
            crate::diag::record_named_bytes(b"NvSysPages", pfns.len() as u32);
            return Err(Error::Host);
        }
    };
    rc::rm_reply(reply, cr::NVOS02_STATUS_AT).map(|_| ()).map_err(|error| {
        // NvSysHost: RM's NV_STATUS, 0x1000_0000 | errno for a host refusal,
        // 0x3000_0000 for a malformed reply; NvSysPages: the descriptor's pages.
        let code = match error {
            rc::RmError::Status(status) => status,
            rc::RmError::Host(errno) => 0x1000_0000 | errno,
            rc::RmError::Malformed => 0x3000_0000,
        };
        crate::diag::record_named_bytes(b"NvSysHost", code);
        crate::diag::record_named_bytes(b"NvSysPages", pfns.len() as u32);
        // The refused page frames' range, and how many lie outside guest RAM
        // as Windows reports it below 4 GiB / above (pfn >= 0x80000 and < 0x100000).
        let lo = pfns.iter().copied().min().unwrap_or(0);
        let hi = pfns.iter().copied().max().unwrap_or(0);
        let hole = pfns.iter().filter(|&&p| (0x80000..0x100000).contains(&p)).count();
        crate::diag::record_named_bytes(b"NvSysPfnLo", lo as u32);
        crate::diag::record_named_bytes(b"NvSysPfnHi", hi as u32);
        crate::diag::record_named_bytes(b"NvSysPfnHole", hole as u32);
        crate::diag::record_named_bytes(b"NvSysPfn0", pfns.first().copied().unwrap_or(0) as u32);
        Error::Host
    })
}

fn refusal_code(refusal: &Refusal) -> u32 {
    match refusal {
        Refusal::MsgType => 1,
        Refusal::NotOwned => 2,
        Refusal::NoResources => 3,
        Refusal::Forbidden => 4,
        Refusal::BadRange => 5,
        Refusal::Transport(_) => 6,
        #[allow(unreachable_patterns)]
        _ => 7,
    }
}

/// Create the real local-linear or pSystemMem OS-descriptor backing.  All
/// private-data validation has already completed in `kmd_logic`; no host
/// allocation is attempted for an unsupported shape.
static STEP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
fn step(n: u32) {
    STEP.store(n, Ordering::Relaxed);
}
/// The last checkpoint `create` reached (diagnostic for refused allocations).
pub fn last_step() -> u32 {
    STEP.load(Ordering::Relaxed)
}

pub fn create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    private: &nm::AllocationPrivate,
    private_bytes: &mut [u8],
) -> Result<Created, Error> {
    step(1);
    let _guard = lock(passive)?;
    step(2);
    // As in `channel_client`: release STATE before `channel_client` re-takes it.
    let existing = STATE.lock().client.as_ref().copied();
    let client = match existing {
        Some(client) => client,
        None => channel_client(passive, adapter, 0)?.0,
    };
    step(3);
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    if epoch == 0 || epoch != client.epoch {
        return Err(Error::NotReady);
    }
    let io = Io {
        passive,
        adapter,
        client,
    };
    step(4);
    let (memory, pins) = {
        let mut state = STATE.lock();
        (state.mint()?, Vec::new())
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        return Err(Error::NoMemory);
    }

    let (actual_size, mut pins) = if private.system_backed() {
        let (mut pins, pfns) = pin_system_memory(private.system_memory, private.size)?;
        if let Err(error) = alloc_system_descriptor(&io, memory, &pfns, private.size) {
            // A timeout may have left the descriptor live.  Its memory handle is
            // deliberately leaked with the pins until the transport sweep.
            if matches!(error, Error::Host) {
                preserve_ambiguous_pins(&mut pins);
            } else if io.rm_free(nd::H_DEVICE, memory).is_ok() {
                unlock_pins(&mut pins);
            } else {
                preserve_ambiguous_pins(&mut pins);
            }
            return Err(error);
        }
        (private.size, pins)
    } else {
        step(5);
        let params = local_params(client.root, private.size);
        match io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &params) {
            Ok(reply) => {
                let actual = rc::get64(&reply, 64).unwrap_or(private.size);
                if actual < private.size
                    || actual % nm::PAGE_BYTES != 0
                    || actual >= nm::GPU_ADDRESS_LIMIT
                {
                    let _ = io.rm_free(nd::H_DEVICE, memory);
                    return Err(Error::Host);
                }
                (actual, pins)
            }
            Err(error) => return Err(error),
        }
    };

    step(6);
    let writeback = match nm::PrivateWriteback::new(
        memory,
        client.root,
        nd::H_DEVICE,
        actual_size,
        private.kind,
        rc::SURFACE_ATTR,
        rc::SURFACE_ATTR2,
        ensure_space(&io, 0)?,
    ) {
        Some(writeback) => writeback,
        None => {
            if io.rm_free(nd::H_DEVICE, memory).is_ok() {
                unlock_pins(&mut pins);
            } else {
                preserve_ambiguous_pins(&mut pins);
            }
            return Err(Error::Invalid);
        }
    };
    step(7);
    // D3D records keep their inputs (0x8c, 0x10c..0x120) and get the D3D field
    // set; the size in 64-KiB units is what the UMD reserves VA for.
    let writeback = if nm::is_d3d_record(private_bytes) {
        let units = u32::try_from(private.size / nm::ALLOCATION_UNIT).unwrap_or(u32::MAX);
        match nm::d3d_writeback(private_bytes, units) {
            Some(fields) => Writeback::D3d(fields),
            None => Writeback::Native(writeback),
        }
    } else {
        Writeback::Native(writeback)
    };
    if writeback.apply(private_bytes).is_err() {
        if io.rm_free(nd::H_DEVICE, memory).is_ok() {
            unlock_pins(&mut pins);
        }
        return Err(Error::Invalid);
    }
    let segment = if private.system_backed() {
        nm::SYSTEM_MEMORY_SEGMENT_ID
    } else {
        crate::ddi::gpummu::MEMORY_SEGMENT_ID
    };
    let allocation = Allocation {
        id,
        epoch,
        memory,
        size: actual_size,
        system_memory: private.system_memory,
        system_kernel_va: if pins.len() == 1 {
            pins[0].kernel_va
        } else {
            0
        },
        pins,
        mappings: Vec::new(),
        cpu: None,
        resident: None,
        poisoned: false,
        writeback: Some(writeback),
    };
    {
        let mut state = STATE.lock();
        if state.allocations.try_reserve_exact(1).is_err() {
            drop(state);
            let mut allocation = allocation;
            if io.rm_free(nd::H_DEVICE, memory).is_ok() {
                unlock_pins(&mut allocation.pins);
            } else {
                preserve_ambiguous_pins(&mut allocation.pins);
            }
            return Err(Error::NoMemory);
        }
        state.allocations.push(allocation);
    }
    super::nvda::note_memory(memory);
    Ok(Created {
        id,
        size: actual_size,
        segment,
        system_memory: private.system_memory,
    })
}

fn rm_error_code(error: rc::RmError) -> u32 {
    match error {
        rc::RmError::Status(status) => status,
        rc::RmError::Host(errno) => 0x8000_0000 | errno,
        rc::RmError::Malformed => 0xffff_ffff,
    }
}

fn map_fixed(
    io: &Io<'_>,
    memory: u32,
    allocation_size: u64,
    system_backed: bool,
    space: u32,
    virt: u32,
    piece: nm::MappingPiece,
) -> Result<u32, Error> {
    nm::validate_piece(piece, allocation_size).map_err(|_| Error::Invalid)?;
    let flags = if system_backed {
        cc::MAP_FLAGS_SYSMEM
    } else {
        LOCAL_4K_MAP_FLAGS
    };
    let end = piece.end().ok_or(Error::Invalid)?;
    if let Some(window) = window_of(space).filter(|_| piece.va >= LOW_WINDOW_BASE && end <= LOW_WINDOW_END) {
        return map_in_window(io, memory, window, flags, piece);
    }
    let params = cc::virtual_params(io.client.root, space, Some(piece.va), piece.len);
    io.rm_alloc(nd::H_DEVICE, virt, cc::NV50_MEMORY_VIRTUAL, &params)
        .inspect_err(|_| {
            // The refused fixed VA (4-KiB page number) and length in pages.
            crate::diag::record_named_bytes(b"NvMapVaPg", (piece.va >> 12) as u32);
            crate::diag::record_named_bytes(b"NvMapLenPg", (piece.len >> 12) as u32);
        })?;
    let block = nm::nvos46_range(
        io.client.root,
        nd::H_DEVICE,
        virt,
        memory,
        piece.memory_offset,
        piece.len,
        flags,
    );
    let mut response = [0u8; REPLY_MAX];
    let mapped = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut response,
    );
    let got = match mapped {
        Ok(n) => (|| {
            let reply = response.get(..n).ok_or(Error::Host)?;
            let reply = rc::rm_reply(reply, cc::NVOS46_STATUS_AT).map_err(|error| {
                crate::diag::record_named_bytes(b"NvRmMapSt", rm_error_code(error));
                Error::Host
            })?;
            cc::map_dma_va(reply.data).ok_or(Error::Host)
        })(),
        Err(error) => Err(error),
    };
    let got = match got {
        Ok(got) => got,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, virt);
            return Err(error);
        }
    };
    if got != piece.va {
        // RM must not silently relocate a WDDM mapping.  Tear down the address
        // RM returned, not the address we wished it had returned.
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, virt, memory, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        let _ = io.rm_free(nd::H_DEVICE, virt);
        return Err(Error::WrongAddress);
    }
    Ok(virt)
}

/// `NVOS46_FLAGS_DMA_OFFSET_FIXED_TRUE`: for a virtual object, `dmaOffset` is
/// the absolute VA of the mapping inside it.
const MAP_FLAGS_DMA_OFFSET_FIXED: u32 = 1 << 15;

/// Map `piece` at its exact VA inside the space's low window (no virtual object
/// of its own). Returns the window handle, which the mapping records.
fn map_in_window(
    io: &Io<'_>,
    memory: u32,
    window: u32,
    flags: u32,
    piece: nm::MappingPiece,
) -> Result<u32, Error> {
    let mut block = nm::nvos46_range(
        io.client.root,
        nd::H_DEVICE,
        window,
        memory,
        piece.memory_offset,
        piece.len,
        flags | MAP_FLAGS_DMA_OFFSET_FIXED,
    );
    block[cc::NVOS46_DMA_OFFSET_AT..cc::NVOS46_DMA_OFFSET_AT + 8]
        .copy_from_slice(&piece.va.to_le_bytes());
    let mut response = [0u8; REPLY_MAX];
    let n = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut response,
    )?;
    let reply = response.get(..n).ok_or(Error::Host)?;
    let reply = rc::rm_reply(reply, cc::NVOS46_STATUS_AT).map_err(|error| {
        crate::diag::record_named_bytes(b"NvRmWinSt", rm_error_code(error));
        crate::diag::record_named_bytes(b"NvMapVaPg", (piece.va >> 12) as u32);
        Error::Host
    })?;
    let got = cc::map_dma_va(reply.data).ok_or(Error::Host)?;
    if got != piece.va {
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, window, memory, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        return Err(Error::WrongAddress);
    }
    Ok(window)
}

fn unmap_fixed(io: &Io<'_>, memory: u32, mapping: HostMapping) -> Result<(), Error> {
    let memory = if mapping.desc != 0 { mapping.desc } else { memory };
    let block = nm::nvos47_unmap(
        io.client.root,
        nd::H_DEVICE,
        mapping.virt,
        memory,
        mapping.piece.va,
    );
    let mut response = [0u8; REPLY_MAX];
    let n = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
        &block,
        &[],
        &mut response,
    )?;
    rc::rm_reply(response.get(..n).ok_or(Error::Host)?, cc::NVOS47_STATUS_AT)
        .map_err(|_| Error::Host)?;
    // The space's low window outlives its mappings (`release_process`).
    if !is_window(mapping.virt) {
        io.rm_free(nd::H_DEVICE, mapping.virt)?;
    }
    if mapping.desc != 0 {
        STATE.lock().desc_pfns.retain(|(desc, _)| *desc != mapping.desc);
        io.rm_free(nd::H_DEVICE, mapping.desc)?;
    }
    Ok(())
}

/// Write the record values CreateAllocation produced for native allocation
/// `id` into `bytes` (an NVDA record dxgkrnl passes to OpenAllocation). The
/// UMD reads sizes and handles from it: without them the D3D12 UMD reserved a
/// zero-size VA range (INVALID_PARAMETER) and removed its device.
pub fn writeback_open(id: u32, bytes: &mut [u8]) -> Result<(), Error> {
    let writeback = STATE.lock().allocation(id)?.writeback.ok_or(Error::Invalid)?;
    writeback.apply(bytes).map_err(|_| Error::Invalid)
}

/// How many mappings `evict_range` dropped (NvEvict) and how many of their
/// outside remainders could not be shown again (NvEvictFail).
static EVICTED: AtomicU32 = AtomicU32::new(0);
static EVICT_FAILED: AtomicU32 = AtomicU32::new(0);

/// Drop every mapping of `process` in `[first_va, first_va + len)` that VidMm's
/// page table no longer shows there: foreign ranges and other allocations
/// (`keep` excepted). The parts of a dropped mapping outside the range are
/// mapped again (RM unmaps whole objects); one that cannot be stays unmapped,
/// as if VidMm had not written it yet. Callers hold the RM lock.
fn evict_range(
    io: &Io<'_>,
    space: u32,
    process: usize,
    first_va: u64,
    len: u64,
    keep: Option<u32>,
) -> Result<(), Error> {
    // NvEvictOff=1 (test knob): leave other mappings alone, as before eviction.
    static EVICT_OFF: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut off = EVICT_OFF.load(Ordering::Relaxed);
    if off == u32::MAX {
        off = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvEvictOff"), 0);
        EVICT_OFF.store(off, Ordering::Relaxed);
    }
    if off == 1 {
        return Ok(());
    }
    let end = first_va.checked_add(len).ok_or(Error::Invalid)?;
    let hit = |m: &HostMapping| {
        m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
    };
    struct Victim {
        id: u32,
        memory: u32,
        size: u64,
        system_backed: bool,
        mapping: HostMapping,
        pfns: Option<Vec<u64>>,
    }
    let (foreign, victims) = {
        let mut state = STATE.lock();
        let mut foreign = Vec::new();
        if state.foreign.iter().any(|m| hit(m)) {
            foreign.try_reserve(state.foreign.len()).map_err(|_| Error::NoMemory)?;
            foreign.extend(state.foreign.iter().copied().filter(|m| hit(m)));
            state.foreign.retain(|m| !hit(m));
        }
        let mut found = Vec::new();
        for allocation in state.allocations.iter().filter(|a| Some(a.id) != keep) {
            for mapping in allocation.mappings.iter().copied().filter(|m| hit(m)) {
                found.try_reserve(1).map_err(|_| Error::NoMemory)?;
                found.push((allocation.id, allocation.memory, allocation.size, allocation.system_memory != 0, mapping));
            }
        }
        let mut victims = Vec::new();
        victims.try_reserve(found.len()).map_err(|_| Error::NoMemory)?;
        for (id, memory, size, system_backed, mapping) in found {
            let pfns = if mapping.desc != 0 {
                state
                    .desc_pfns
                    .iter()
                    .find(|(desc, _)| *desc == mapping.desc)
                    .and_then(|(_, pfns)| {
                        let mut copy = Vec::new();
                        copy.try_reserve_exact(pfns.len()).ok()?;
                        copy.extend_from_slice(pfns);
                        Some(copy)
                    })
            } else {
                None
            };
            victims.push(Victim { id, memory, size, system_backed, mapping, pfns });
        }
        for allocation in state.allocations.iter_mut().filter(|a| Some(a.id) != keep) {
            allocation.mappings.retain(|m| !hit(m));
        }
        (foreign, victims)
    };
    if foreign.is_empty() && victims.is_empty() {
        return Ok(());
    }
    let n = EVICTED.fetch_add((foreign.len() + victims.len()) as u32, Ordering::Relaxed)
        + (foreign.len() + victims.len()) as u32;
    crate::diag::record_named_bytes(b"NvEvict", n);
    for mapping in foreign {
        let _ = unmap_fixed(io, 0, mapping);
    }
    let failed = || {
        let n = EVICT_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvEvictFail", n);
    };
    for victim in victims {
        let m = victim.mapping;
        if unmap_fixed(io, victim.memory, m).is_err() {
            if let Ok(allocation) = STATE.lock().allocation_mut(victim.id) {
                allocation.poisoned = true;
            }
            failed();
            continue;
        }
        let Some(m_end) = m.piece.end() else {
            continue;
        };
        let mut rest = [None, None];
        if m.piece.va < first_va {
            rest[0] = Some((m.piece.va, first_va - m.piece.va));
        }
        if m_end > end {
            rest[1] = Some((end, m_end - end));
        }
        for (va, bytes) in rest.into_iter().flatten() {
            let delta = va - m.piece.va;
            let piece = nm::MappingPiece {
                va,
                memory_offset: m.piece.memory_offset + delta,
                len: bytes,
            };
            let shown = (|| -> Result<HostMapping, Error> {
                let virt = STATE.lock().mint()?;
                if m.desc == 0 {
                    let used = map_fixed(io, victim.memory, victim.size, victim.system_backed, space, virt, piece)?;
                    return Ok(HostMapping { process, piece, virt: used, desc: 0 });
                }
                let pfns = victim.pfns.as_ref().ok_or(Error::Invalid)?;
                let first = (delta / nm::PAGE_BYTES) as usize;
                let count = (bytes / nm::PAGE_BYTES) as usize;
                let sub = pfns.get(first..first + count).ok_or(Error::Invalid)?;
                let desc = STATE.lock().mint()?;
                alloc_system_descriptor(io, desc, sub, bytes)?;
                let inner = nm::MappingPiece { va, memory_offset: 0, len: bytes };
                let used = match map_fixed(io, desc, bytes, true, space, virt, inner) {
                    Ok(used) => used,
                    Err(error) => {
                        let _ = io.rm_free(nd::H_DEVICE, desc);
                        return Err(error);
                    }
                };
                let mut kept = Vec::new();
                if kept.try_reserve_exact(sub.len()).is_ok() {
                    kept.extend_from_slice(sub);
                    let mut state = STATE.lock();
                    if state.desc_pfns.try_reserve(1).is_ok() {
                        state.desc_pfns.push((desc, kept));
                    }
                }
                Ok(HostMapping { process, piece, virt: used, desc })
            })();
            let Ok(shown) = shown else {
                failed();
                continue;
            };
            let recorded = {
                let mut state = STATE.lock();
                match state.allocation_mut(victim.id) {
                    Ok(allocation) => {
                        if allocation.mappings.try_reserve(1).is_ok() {
                            allocation.mappings.push(shown);
                            allocation
                                .mappings
                                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
                            true
                        } else {
                            false
                        }
                    }
                    Err(_) => false,
                }
            };
            if !recorded {
                let _ = unmap_fixed(io, victim.memory, shown);
                failed();
            }
        }
    }
    Ok(())
}

/// A leaf update whose PTEs place a device-local native allocation in SYSTEM
/// pages. VidMm keeps CPU-accessed allocations there (the UMD's Lock2 view is
/// those pages), so the GPU must see the same pages, not the allocation's RM
/// video memory: GPFIFO entries and pushbuffers the UMD wrote were otherwise
/// invisible to the GPU. Whatever the process mapped in the range is dropped,
/// the pages (`pfns`, one per 4-KiB PTE) become an RM OS-descriptor object and
/// that is mapped at the exact VA. VidMm's Transfer operations move contents
/// between these pages and the RM memory when it changes the placement.
pub fn update_leaf_system(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
) -> Result<(), Error> {
    if pfns.is_empty() {
        return Err(Error::Invalid);
    }
    let mut none = Vec::new();
    none.try_reserve_exact(pfns.len()).map_err(|_| Error::NoMemory)?;
    none.resize(pfns.len(), None);
    // NvSysWhy: 1 dropping the old mappings (an error name in NvSysErr), 2 the
    // OS descriptor, 3 mapping it.
    let why = |code: u32, error: Error| {
        crate::diag::record_named_bytes(b"NvSysWhy", code);
        crate::diag::record_named_bytes(b"NvSysErr", error as u32);
        error
    };
    update_leaf(passive, adapter, id, process, first_va, allocation_offset, &none)
        .map_err(|error| why(1, error))?;

    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let space = ensure_space(&io, process)?;
    // One OS descriptor per SYSTEM_CHUNK_PAGES: RM refused large ones (512 and
    // 335 pages) with NV_ERR_INVALID_ADDRESS after pinning them (bpftrace:
    // every pin succeeded), and the range stayed unmapped (ring fetch faults).
    // A refused chunk still fails the update; the chunks mapped before it stay.
    let chunk_pages = SYSTEM_CHUNK_PAGES.max(1);
    let mut first_error = None;
    for (index, chunk) in pfns.chunks(chunk_pages).enumerate() {
        let delta = (index * chunk_pages) as u64 * nm::PAGE_BYTES;
        let va = first_va.checked_add(delta).ok_or(Error::Invalid)?;
        let offset = allocation_offset.checked_add(delta).ok_or(Error::Invalid)?;
        if let Err(error) = map_system_chunk(&io, id, process, space, va, offset, chunk, &why) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Pages per OS descriptor of `update_leaf_system`.
const SYSTEM_CHUNK_PAGES: usize = 128;

#[allow(clippy::too_many_arguments)]
fn map_system_chunk(
    io: &Io<'_>,
    id: u32,
    process: usize,
    space: u32,
    va: u64,
    allocation_offset: u64,
    pfns: &[u64],
    why: &dyn Fn(u32, Error) -> Error,
) -> Result<(), Error> {
    let len = (pfns.len() as u64)
        .checked_mul(nm::PAGE_BYTES)
        .ok_or(Error::Invalid)?;
    let desc = STATE.lock().mint()?;
    alloc_system_descriptor(io, desc, pfns, len).map_err(|error| why(2, error))?;
    let virt = STATE.lock().mint()?;
    let shown = nm::MappingPiece {
        va,
        memory_offset: 0,
        len,
    };
    let used = match map_fixed(io, desc, len, true, space, virt, shown) {
        Ok(used) => used,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, desc);
            return Err(why(3, error));
        }
    };
    let mapping = HostMapping {
        process,
        piece: nm::MappingPiece {
            va,
            memory_offset: allocation_offset,
            len,
        },
        virt: used,
        desc,
    };
    let mut kept = Vec::new();
    if kept.try_reserve_exact(pfns.len()).is_ok() {
        kept.extend_from_slice(pfns);
    }
    let mut state = STATE.lock();
    if !kept.is_empty() && state.desc_pfns.try_reserve(1).is_ok() {
        state.desc_pfns.push((desc, kept));
    }
    let allocation = state.allocation_mut(id)?;
    if allocation.mappings.try_reserve(1).is_err() {
        drop(state);
        let _ = unmap_fixed(io, 0, mapping);
        return Err(Error::NoMemory);
    }
    allocation.mappings.push(mapping);
    allocation
        .mappings
        .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
    Ok(())
}

/// A leaf update of an allocation this driver has no RM memory for (dxgkrnl's
/// monitored-fence and progress-fence storage, any non-native allocation) in
/// a process with a native VA space. The GPU signals queue progress and
/// monitored fences by writing those VAs (0x10140...), so they must be mapped
/// in RM's space too: system pages (`pfns`, one per PTE) become an OS
/// descriptor mapped at the exact VA; `pfns` empty only drops what the
/// process mapped in [first_va, first_va + pages * 4 KiB).
pub fn update_foreign_leaf(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    first_va: u64,
    pages: u64,
    pfns: &[u64],
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let len = pages.checked_mul(nm::PAGE_BYTES).ok_or(Error::Invalid)?;
    let stale: Vec<HostMapping> = {
        let mut state = STATE.lock();
        let mut stale = Vec::new();
        let mut kept = Vec::new();
        for mapping in state.foreign.drain(..) {
            if mapping.process == process
                && nm::ranges_overlap(mapping.piece.va, mapping.piece.len, first_va, len)
            {
                stale.push(mapping);
            } else {
                kept.push(mapping);
            }
        }
        state.foreign = kept;
        stale
    };
    for mapping in stale {
        let _ = unmap_fixed(&io, 0, mapping);
    }
    if pfns.is_empty() {
        return Ok(());
    }
    let space = ensure_space(&io, process)?;
    // Native allocations VidMm no longer shows here (see `update_leaf`).
    evict_range(&io, space, process, first_va, len, None)?;
    let desc = STATE.lock().mint()?;
    alloc_system_descriptor(&io, desc, pfns, len)?;
    let virt = STATE.lock().mint()?;
    let piece = nm::MappingPiece {
        va: first_va,
        memory_offset: 0,
        len,
    };
    let used = match map_fixed(&io, desc, len, true, space, virt, piece) {
        Ok(used) => used,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, desc);
            return Err(error);
        }
    };
    let mapping = HostMapping {
        process,
        piece,
        virt: used,
        desc,
    };
    let mut state = STATE.lock();
    if state.foreign.try_reserve(1).is_err() {
        drop(state);
        let _ = unmap_fixed(&io, 0, mapping);
        return Err(Error::NoMemory);
    }
    state.foreign.push(mapping);
    Ok(())
}

/// CPU access to `len` bytes at GPU VA `va` of `process` exactly as the GPU
/// sees them: VidMm's system pages for a system-placed native allocation, the
/// RM memory otherwise. `visit` gets a temporary pointer; the range must not
/// cross a 4-KiB page. IRQL <= DISPATCH; the RM-memory case needs `passive`
/// (`Err(Busy)` without). `Ok(false)` when nothing native maps `va`.
pub fn patch_gpu_bytes(
    passive: Option<PassiveLevel>,
    adapter: &AdapterContext,
    process: usize,
    va: u64,
    len: u64,
    visit: impl FnOnce(*mut u8),
) -> Result<bool, Error> {
    if len == 0 || (va & 0xfff) + len > 0x1000 {
        return Err(Error::Invalid);
    }
    enum Where {
        System(u64),
        Rm(u32, u64),
    }
    let found = {
        let state = STATE.lock();
        let mut found = None;
        'outer: for allocation in state.allocations.iter() {
            for m in allocation.mappings.iter().filter(|m| m.process == process) {
                let Some(end) = m.piece.end() else { continue };
                if va < m.piece.va || va + len > end {
                    continue;
                }
                let delta = va - m.piece.va;
                if m.desc != 0 {
                    let pfn = state
                        .desc_pfns
                        .iter()
                        .find(|(desc, _)| *desc == m.desc)
                        .and_then(|(_, pfns)| pfns.get((delta >> 12) as usize).copied());
                    if let Some(pfn) = pfn {
                        found = Some(Where::System((pfn << 12) | (va & 0xfff)));
                    }
                } else {
                    found = Some(Where::Rm(allocation.id, m.piece.memory_offset + delta));
                }
                break 'outer;
            }
        }
        found
    };
    match found {
        None => Ok(false),
        Some(Where::System(physical)) => {
            let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
            pa.QuadPart = (physical & !0xfff) as i64;
            // SAFETY: a page VidMm keeps locked while it is mapped for the GPU;
            // MmCached matches system RAM, and the view is released below.
            let view = unsafe { MmMapIoSpace(pa, 0x1000, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
            if view.is_null() {
                return Err(Error::NoMemory);
            }
            visit(unsafe { view.add((physical & 0xfff) as usize) });
            unsafe { MmUnmapIoSpace(view.cast(), 0x1000) };
            Ok(true)
        }
        Some(Where::Rm(id, offset)) => {
            // The RM-memory view needs PASSIVE; system pages work up to DISPATCH.
            let Some(passive) = passive else {
                return Err(Error::Busy);
            };
            let mut visit = Some(visit);
            // SAFETY: PASSIVE; the closure only writes inside the given range.
            unsafe {
                with_cpu_range(passive, adapter, id, offset, len, |_, native, got| {
                    if got == len {
                        if let Some(visit) = visit.take() {
                            visit(native);
                        }
                    }
                    true
                })
            }
        }
    }
}

/// Apply one native leaf update.  Non-leaf updates deliberately have no host
/// operation.  Invalid PTEs unmap.  A changed live mapping is refused until the
/// parent marks that address space idle; unmapping and fresh mapping remain
/// synchronous, so a following FLUSH_TLB has no deferred work.
pub fn update_leaf(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (memory, size, system_backed, old) = {
        let state = STATE.lock();
        let allocation = state.allocation(id)?;
        if allocation.poisoned || allocation.epoch != client.epoch {
            return Err(Error::Poisoned);
        }
        (
            allocation.memory,
            allocation.size,
            allocation.system_memory != 0,
            allocation.mappings.clone(),
        )
    };
    let space = ensure_space(&io, process)?;
    let count = pieces.len() as u64;
    let len = count.checked_mul(nm::PAGE_BYTES).ok_or(Error::Invalid)?;
    if count == 0 || first_va & (nm::PAGE_BYTES - 1) != 0 {
        return Err(Error::Invalid);
    }
    let mut new = Vec::new();
    new.try_reserve_exact(pieces.len() + 2)
        .map_err(|_| Error::NoMemory)?;
    for (index, piece) in pieces.iter().enumerate() {
        if let Some(piece) = piece {
            let expected = nm::MappingPiece {
                va: first_va + index as u64 * nm::PAGE_BYTES,
                memory_offset: allocation_offset + index as u64 * nm::PAGE_BYTES,
                len: nm::PAGE_BYTES,
            };
            if *piece != expected {
                return Err(Error::Invalid);
            }
            nm::validate_piece(*piece, size).map_err(|_| Error::Invalid)?;
            new.push(*piece);
        }
    }
    nm::coalesce(&mut new);

    let old_process: Vec<HostMapping> = old
        .iter()
        .copied()
        .filter(|m| m.process == process)
        .collect();
    let old_pieces: Vec<nm::MappingPiece> = old_process.iter().map(|m| m.piece).collect();
    let unmapped =
        nm::replace_range(&old_pieces, first_va, len, None, size).map_err(|_| Error::Invalid)?;
    let mut final_pieces = unmapped.pieces;
    for piece in new.iter().copied() {
        final_pieces = nm::replace_range(&final_pieces, piece.va, piece.len, Some(piece), size)
            .map_err(|_| Error::Overlap)?
            .pieces;
    }
    // No coalescing across updates: every existing piece is one live RM virtual
    // object. VidMm fills a large allocation one page table (2 MiB of VA) per
    // call; merging the new piece into the previous call's neighbour remapped
    // the merged range while the neighbour's object still held its part, and RM
    // refused the fixed VA (NV_ERR_INSUFFICIENT_RESOURCES).
    final_pieces.sort_unstable_by_key(|piece| piece.va);

    // GPU VAs are per process: only this process's other mappings can collide.
    // The page table VidMm writes is the truth: whatever another allocation or
    // a foreign range still holds at these VAs is gone from VidMm's view, so it
    // goes from RM's too (RM refused the new fixed mapping over the stale one:
    // NV_ERR_INVALID_ARGUMENT, and the GPU faulted on the unmapped range).
    evict_range(&io, space, process, first_va, len, Some(id))?;

    let affected_old: Vec<HostMapping> = old_process
        .iter()
        .copied()
        .filter(|m| nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len))
        .collect();
    let update_end = first_va.checked_add(len).ok_or(Error::Invalid)?;
    let span_start = affected_old
        .iter()
        .map(|m| m.piece.va)
        .min()
        .unwrap_or(first_va)
        .min(first_va);
    let span_end = affected_old
        .iter()
        .filter_map(|m| m.piece.end())
        .max()
        .unwrap_or(update_end)
        .max(update_end);
    let affected_new: Vec<nm::MappingPiece> = final_pieces
        .iter()
        .copied()
        .filter(|piece| piece.va < span_end && span_start < piece.end().unwrap_or(piece.va))
        .collect();
    if affected_old
        .iter()
        .map(|m| m.piece)
        .eq(affected_new.iter().copied())
    {
        return Ok(());
    }

    // Replacing a live mapping: VidMm only changes an allocation's placement once
    // no GPU work that uses it is outstanding (it waits for the paging and
    // context fences first), so RM's idle-channels quiesce is only a belt on
    // top. It is best effort: its refusal (Busy) had VidMm's every move between
    // system pages and video memory skipped (NvPgLeafWhy 0x311).
    if !affected_old.is_empty()
        && super::native_channel::quiesce(passive, adapter, process).is_err()
    {
        let n = QUIESCE_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvQuiesceRef", n);
    }

    // Mint handles before tearing down anything.  A local handle-space
    // exhaustion therefore cannot strand the old mapping.
    let mut planned = Vec::new();
    planned
        .try_reserve_exact(affected_new.len())
        .map_err(|_| Error::NoMemory)?;
    for piece in affected_new.iter().copied() {
        planned.push((piece, STATE.lock().mint()?));
    }
    for mapping in affected_old.iter().copied() {
        if unmap_fixed(&io, memory, mapping).is_err() {
            STATE.lock().allocation_mut(id)?.poisoned = true;
            return Err(Error::Poisoned);
        }
    }

    let mut made = Vec::new();
    for (piece, virt) in planned.iter().copied() {
        let mapped = map_fixed(&io, memory, size, system_backed, space, virt, piece);
        if let Err(error) = mapped {
            // Who collided: the process key, the allocation, its space and how
            // many allocations / tracked mappings of this process exist.
            {
                let state = STATE.lock();
                let tracked = state
                    .allocations
                    .iter()
                    .flat_map(|a| a.mappings.iter())
                    .filter(|m| m.process == process)
                    .count();
                crate::diag::record_named_bytes(b"NvMapProc", process as u32);
                crate::diag::record_named_bytes(b"NvMapId", id);
                crate::diag::record_named_bytes(b"NvMapSpace", space);
                crate::diag::record_named_bytes(b"NvMapTracked", tracked as u32);
                crate::diag::record_named_bytes(b"NvMapSpaces", state.spaces.len() as u32);
            }
            for mapping in made.iter().copied() {
                let _ = unmap_fixed(&io, memory, mapping);
            }
            // Best-effort rollback.  If it cannot be completed, poison this
            // allocation: no success may be reported against a guessed map set.
            let mut rollback = Vec::new();
            let mut rollback_ok = true;
            for old_mapping in affected_old.iter().copied() {
                if old_mapping.desc != 0 {
                    // Its descriptor is gone with the unmap; nothing to restore.
                    rollback_ok = false;
                    break;
                }
                let virt = match STATE.lock().mint() {
                    Ok(v) => v,
                    Err(_) => {
                        rollback_ok = false;
                        break;
                    }
                };
                let Ok(virt) = map_fixed(
                    &io,
                    memory,
                    size,
                    system_backed,
                    space,
                    virt,
                    old_mapping.piece,
                ) else {
                    rollback_ok = false;
                    break;
                };
                rollback.push(HostMapping {
                    virt,
                    ..old_mapping
                });
            }
            if !rollback_ok {
                STATE.lock().allocation_mut(id)?.poisoned = true;
                return Err(Error::Poisoned);
            }
            let mut state = STATE.lock();
            let allocation = state.allocation_mut(id)?;
            allocation.mappings.retain(|mapping| {
                !(mapping.process == process
                    && mapping.piece.va < span_end
                    && span_start < mapping.piece.end().unwrap_or(mapping.piece.va))
            });
            allocation.mappings.extend(rollback);
            allocation
                .mappings
                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
            return Err(error);
        }
        made.push(HostMapping {
            process,
            piece,
            // The window's handle when the piece went into the low window.
            virt: mapped.unwrap_or(virt),
            desc: 0,
        });
    }

    let mut state = STATE.lock();
    let allocation = state.allocation_mut(id)?;
    allocation.mappings.retain(|mapping| {
        !(mapping.process == process
            && nm::ranges_overlap(mapping.piece.va, mapping.piece.len, first_va, len))
    });
    allocation.mappings.extend(made);
    allocation
        .mappings
        .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
    Ok(())
}

/// Mark whether an address space has been quiesced for a live replacement.
/// The scheduler/root-page-table integration calls this; absence means busy.
pub fn set_address_space_idle(process: usize, idle: bool) {
    let mut state = STATE.lock();
    if idle {
        if !state.idle_spaces.contains(&process) {
            let _ = state.idle_spaces.try_reserve_exact(1);
            if !state.idle_spaces.contains(&process) {
                state.idle_spaces.push(process);
            }
        }
    } else {
        state.idle_spaces.retain(|p| *p != process);
    }
}

pub fn notify_residency(id: u32, physical_address: u64, resident: bool) -> Result<(), Error> {
    let mut state = STATE.lock();
    let allocation = state.allocation_mut(id)?;
    allocation.resident = resident.then_some((physical_address, true));
    Ok(())
}

fn release_cpu_chunk(io: &Io<'_>, memory: u32, chunk: &mut CpuChunk) -> Result<(), Error> {
    if chunk.map_ch == 0 {
        return Ok(());
    }
    if chunk.host_id != 0 {
        nvrm::release_host_map_within(
            io.passive,
            io.adapter,
            chunk.map_ch,
            chunk.host_id,
            TIMEOUT_MS,
        )
        .map_err(|_| Error::Host)?;
        chunk.host_id = 0;
    }
    if chunk.rm_mapped {
        let block = rc::nvos34(io.client.root, nd::H_DEVICE, memory, chunk.cookie);
        let mut response = [0u8; REPLY_MAX];
        let n = io.exchange(
            io.client.ctl,
            rc::nv_cmd(rc::ESC_RM_UNMAP_MEMORY, rc::NVOS34_BYTES as u32),
            &block,
            &[],
            &mut response,
        )?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS34_STATUS_AT)
            .map_err(|_| Error::Host)?;
        chunk.cookie = 0;
        chunk.rm_mapped = false;
    }
    io.close_file(chunk.map_ch)?;
    chunk.map_ch = 0;
    Ok(())
}

fn cpu_map_chunk(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    expected_gpa: Option<u64>,
) -> Result<CpuChunk, Error> {
    let region = nvrm::region_for(io.adapter, io.client.minor).ok_or(Error::Host)?;
    let expected = match expected_gpa {
        Some(base) => Some(base.checked_add(offset).ok_or(Error::WrongAddress)?),
        None => None,
    };
    let window_offset = match expected {
        Some(expected) => Some(
            expected
                .checked_sub(region.base)
                .ok_or(Error::WrongAddress)?,
        ),
        None => None,
    };
    let map_ch = io.open_file(io.client.minor)?;
    if let Err(error) = io.register_fd(map_ch) {
        let _ = io.close_file(map_ch);
        return Err(error);
    }
    let mut chunk = CpuChunk {
        physical: 0,
        map_ch,
        cookie: 0,
        rm_mapped: false,
        host_id: 0,
    };
    let block = rc::nvos33_with_fd(
        io.client.root,
        nd::H_DEVICE,
        allocation.memory,
        offset,
        len,
        map_ch,
    );
    let mut response = [0u8; REPLY_MAX];
    let cookie = match io.exchange(
        io.client.ctl,
        rc::nv_cmd(rc::ESC_RM_MAP_MEMORY, rc::NVOS33_FD_BYTES as u32),
        &block,
        &[],
        &mut response,
    ) {
        Ok(n) => (|| {
            let reply = response.get(..n).ok_or(Error::Host)?;
            let reply = rc::rm_reply(reply, rc::NVOS33_STATUS_AT).map_err(|_| Error::Host)?;
            rc::map_cookie(&reply).ok_or(Error::Host)
        })(),
        Err(error) => Err(error),
    };
    match cookie {
        Ok(cookie) => {
            chunk.cookie = cookie;
            chunk.rm_mapped = true;
        }
        Err(error) => {
            let _ = io.close_file(map_ch);
            return Err(error);
        }
    }
    let mapped = match window_offset {
        Some(window_offset) => nvrm::host_mmap_at_within(
            io.passive,
            io.adapter,
            KMD,
            map_ch,
            true,
            0,
            window_offset,
            len,
            TIMEOUT_MS,
        ),
        None => nvrm::host_mmap_within(
            io.passive, io.adapter, KMD, map_ch, true, 0, len, TIMEOUT_MS,
        ),
    };
    let mapped = match mapped {
        Ok(mapped) => {
            chunk.host_id = mapped.host_id;
            mapped
        }
        Err(_) => {
            let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
            return Err(Error::Host);
        }
    };
    if mapped.size < len {
        let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
        return Err(Error::Host);
    }
    let physical = region.base.checked_add(mapped.offset);
    if physical.is_none_or(|physical| expected.is_some_and(|expected| physical != expected)) {
        let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
        return Err(Error::WrongAddress);
    }
    chunk.physical = physical.ok_or(Error::WrongAddress)?;
    Ok(chunk)
}

fn cpu_map(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    aperture_offset: Option<u64>,
    expected_gpa: Option<u64>,
) -> Result<CpuMapping, Error> {
    let end = offset.checked_add(len).ok_or(Error::Invalid)?;
    if len == 0
        || end > allocation.size
        || (allocation.system_memory == 0
            && (offset & (nm::PAGE_BYTES - 1) != 0 || len & (nm::PAGE_BYTES - 1) != 0))
        || aperture_offset.is_some() != expected_gpa.is_some()
        || (expected_gpa.is_none() && len > nvrm::MAX_MAP_BYTES)
    {
        return Err(Error::Invalid);
    }
    if allocation.system_memory != 0 {
        // Separate MDL mappings are not contiguous. Never expose the first
        // chunk (or a process-local user VA) as a view of the whole allocation.
        if allocation.system_kernel_va == 0 {
            return Err(Error::Unsupported);
        }
        let address = allocation
            .system_kernel_va
            .checked_add(offset)
            .ok_or(Error::Invalid)?;
        return Ok(CpuMapping {
            view: CpuView {
                address,
                len,
                generation: 0,
            },
            chunks: Vec::new(),
            kernel_va: 0,
            aperture_offset,
        });
    }

    let count =
        usize::try_from((len - 1) / nvrm::MAX_MAP_BYTES + 1).map_err(|_| Error::NoMemory)?;
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let mut chunk_offset = offset;
    while chunk_offset < end {
        let chunk_len = (end - chunk_offset).min(nvrm::MAX_MAP_BYTES);
        match cpu_map_chunk(io, allocation, chunk_offset, chunk_len, expected_gpa) {
            Ok(chunk) => chunks.push(chunk),
            Err(error) => {
                for chunk in chunks.iter_mut() {
                    let _ = release_cpu_chunk(io, allocation.memory, chunk);
                }
                return Err(error);
            }
        }
        chunk_offset += chunk_len;
    }
    let physical = match expected_gpa {
        Some(expected) => expected,
        None => chunks
            .first()
            .map(|chunk| chunk.physical)
            .ok_or(Error::Host)?,
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = physical as i64;
    // A fixed aperture has contiguous placements by construction, so one
    // aggregate guest view is valid. Dynamic mappings are limited to one chunk.
    let kernel_va = unsafe { MmMapIoSpace(pa, len, _MEMORY_CACHING_TYPE::MmNonCached) } as u64;
    let mut mapping = CpuMapping {
        view: CpuView {
            address: kernel_va,
            len,
            generation: chunks.first().map_or(0, |chunk| chunk.host_id),
        },
        chunks,
        kernel_va,
        aperture_offset,
    };
    if kernel_va == 0 {
        let _ = cpu_unmap_for(io, allocation.memory, &mut mapping);
        return Err(Error::NoMemory);
    }
    Ok(mapping)
}

fn cpu_unmap_for(io: &Io<'_>, memory: u32, mapping: &mut CpuMapping) -> Result<(), Error> {
    if mapping.kernel_va != 0 {
        unsafe { MmUnmapIoSpace(mapping.kernel_va as *mut c_void, mapping.view.len) };
        mapping.kernel_va = 0;
    }
    let mut ok = true;
    // A failed step leaves that chunk's fd and later steps intact for retry,
    // but does not prevent teardown of any other chunk.
    for chunk in mapping.chunks.iter_mut() {
        if release_cpu_chunk(io, memory, chunk).is_err() {
            ok = false;
        }
    }
    if ok {
        Ok(())
    } else {
        Err(Error::Host)
    }
}

/// Run a synchronous content operation over a range of the allocation's real
/// CPU bytes. Dynamic placements need not be contiguous, so temporary mappings
/// are made and released one page-rounded, bounded chunk at a time.
pub unsafe fn with_cpu_range(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    length: u64,
    mut f: impl FnMut(u64, *mut u8, u64) -> bool,
) -> Result<bool, Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (snapshot, existing, pending) = {
        let state = STATE.lock();
        let a = state.allocation(id)?;
        let pending = a
            .cpu
            .as_ref()
            .is_some_and(|cpu| cpu.aperture_offset.is_none());
        let existing = a.cpu.as_ref().and_then(|cpu| {
            (cpu.aperture_offset.is_some() && (a.system_memory != 0 || cpu.kernel_va != 0))
                .then_some(cpu.view)
        });
        (
            Allocation {
                id: a.id,
                epoch: a.epoch,
                memory: a.memory,
                size: a.size,
                system_memory: a.system_memory,
                system_kernel_va: a.system_kernel_va,
                pins: Vec::new(),
                mappings: Vec::new(),
                cpu: None,
                resident: None,
                poisoned: a.poisoned,
                writeback: a.writeback,
            },
            existing,
            pending,
        )
    };
    let end = offset.checked_add(length).ok_or(Error::Invalid)?;
    if end > snapshot.size {
        return Err(Error::Invalid);
    }
    if length == 0 {
        return Ok(true);
    }
    if pending {
        // A previous temporary teardown failed after its guest view was removed.
        // Retry the retained host state before making another mapping.
        let mut mapping = STATE
            .lock()
            .allocation_mut(id)?
            .cpu
            .take()
            .ok_or(Error::Invalid)?;
        if let Err(error) = cpu_unmap_for(&io, snapshot.memory, &mut mapping) {
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
    } else if STATE.lock().allocation(id)?.cpu.is_some() && existing.is_none() {
        return Err(Error::Busy);
    }
    if let Some(view) = existing {
        let address = view.address.checked_add(offset).ok_or(Error::Invalid)?;
        return Ok(f(0, address as *mut u8, length));
    }
    if snapshot.system_memory != 0 {
        if snapshot.system_kernel_va == 0 {
            return Err(Error::Unsupported);
        }
        let address = snapshot
            .system_kernel_va
            .checked_add(offset)
            .ok_or(Error::Invalid)?;
        return Ok(f(0, address as *mut u8, length));
    }

    let mut completed = 0u64;
    while completed < length {
        let request_offset = offset + completed;
        let map_offset = request_offset & !(nm::PAGE_BYTES - 1);
        let page_offset = request_offset - map_offset;
        let chunk_len = (length - completed).min(nvrm::MAX_MAP_BYTES - page_offset);
        let request_end = request_offset + chunk_len;
        let map_end = request_end
            .checked_add(nm::PAGE_BYTES - 1)
            .map(|end| end & !(nm::PAGE_BYTES - 1))
            .unwrap_or(snapshot.size)
            .min(snapshot.size);
        let mut mapping = cpu_map(&io, &snapshot, map_offset, map_end - map_offset, None, None)?;
        // SAFETY: page_offset is within the page-rounded mapping made above.
        let address = (mapping.view.address as *mut u8).add(page_offset as usize);
        let accepted = f(completed, address, chunk_len);
        if let Err(error) = cpu_unmap_for(&io, snapshot.memory, &mut mapping) {
            // Keep partially released host state reachable for a later retry or
            // transport teardown; never silently drop a live mapping.
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
        if !accepted {
            return Ok(false);
        }
        completed += chunk_len;
    }
    Ok(true)
}

/// Map a native CPU-host-aperture request.  pSystemMem is already the direct
/// Lock2 view.  Device local memory uses NVOS33 + the RM window, and succeeds
/// only when that window placement is exactly dxgkrnl's requested GPA.
pub fn map_aperture(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    aperture_offset: u64,
    expected_gpa: u64,
) -> Result<CpuView, Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let snapshot = {
        let state = STATE.lock();
        let a = state.allocation(id)?;
        Allocation {
            id: a.id,
            epoch: a.epoch,
            memory: a.memory,
            size: a.size,
            system_memory: a.system_memory,
            system_kernel_va: a.system_kernel_va,
            pins: Vec::new(),
            mappings: Vec::new(),
            cpu: None,
            resident: None,
            poisoned: a.poisoned,
            writeback: a.writeback,
        }
    };
    let mut mapping = cpu_map(
        &io,
        &snapshot,
        0,
        snapshot.size,
        Some(aperture_offset),
        Some(expected_gpa),
    )?;
    let view = mapping.view;
    let mut state = STATE.lock();
    let allocation = state.allocation_mut(id)?;
    if allocation.cpu.is_some() {
        drop(state);
        let mut mapping = mapping;
        let _ = cpu_unmap_for(&io, snapshot.memory, &mut mapping);
        return Err(Error::Busy);
    }
    allocation.cpu = Some(mapping);
    Ok(view)
}

pub fn unmap_aperture(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    aperture_offset: u64,
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (id, memory) = {
        let state = STATE.lock();
        let allocation = state
            .allocations
            .iter()
            .find(|a| {
                a.cpu
                    .as_ref()
                    .is_some_and(|cpu| cpu.aperture_offset == Some(aperture_offset))
            })
            .ok_or(Error::Invalid)?;
        (allocation.id, allocation.memory)
    };
    let mut mapping = STATE
        .lock()
        .allocation_mut(id)?
        .cpu
        .take()
        .ok_or(Error::Invalid)?;
    match cpu_unmap_for(&io, memory, &mut mapping) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Preserve whatever remains live for a later retry or transport
            // teardown; a failed host unmap is not permission to forget it.
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            Err(error)
        }
    }
}

/// Destroy an allocation only after every GPU and CPU view is gone.  Failure
/// leaves the record and its MDLs alive; the caller must not report reclamation
/// or unlock pSystemMem early.
pub fn destroy(passive: PassiveLevel, adapter: &AdapterContext, id: u32) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (memory, mappings, has_cpu) = {
        let state = STATE.lock();
        let allocation = state.allocation(id)?;
        (
            allocation.memory,
            allocation.mappings.clone(),
            allocation.cpu.is_some(),
        )
    };
    if has_cpu {
        return Err(Error::Busy);
    }
    for mapping in mappings {
        unmap_fixed(&io, memory, mapping)?;
        STATE
            .lock()
            .allocation_mut(id)?
            .mappings
            .retain(|live| !(live.virt == mapping.virt && live.piece.va == mapping.piece.va));
    }
    io.rm_free(nd::H_DEVICE, memory)?;
    let mut allocation = {
        let mut state = STATE.lock();
        let index = state
            .allocations
            .iter()
            .position(|a| a.id == id)
            .ok_or(Error::Invalid)?;
        state.allocations.remove(index)
    };
    unlock_pins(&mut allocation.pins);
    Ok(())
}

/// `DxgkDdiDestroyProcess`: release every RM virtual object the process still
/// maps and its VA space. Process contexts are keyed by address, so a later
/// process can reuse the key: left behind, the dead process's fixed-VA objects
/// made RM refuse the new process's identical VidMm VAs (the D3D11 UMD's GPFIFO
/// at 0x4000000, NV_ERR_INSUFFICIENT_RESOURCES). Returns how many mappings
/// could not be released (they stay recorded and the space is kept).
pub fn release_process(passive: PassiveLevel, adapter: &AdapterContext, process: usize) -> u32 {
    if process == 0 {
        return 0;
    }
    let Ok(_guard) = lock(passive) else {
        return 1;
    };
    let Ok(client) = STATE.lock().client() else {
        // No native client: nothing was ever mapped for anyone.
        let mut state = STATE.lock();
        state.spaces.retain(|(owner, _)| *owner != process);
        state.idle_spaces.retain(|p| *p != process);
        return 0;
    };
    let io = Io {
        passive,
        adapter,
        client,
    };
    let owned: Vec<(u32, u32, HostMapping)> = {
        let state = STATE.lock();
        let mut owned = Vec::new();
        for allocation in state.allocations.iter() {
            for mapping in allocation.mappings.iter().filter(|m| m.process == process) {
                if owned.try_reserve(1).is_err() {
                    return 1;
                }
                owned.push((allocation.id, allocation.memory, *mapping));
            }
        }
        owned
    };
    let mut kept = 0;
    let foreign: Vec<HostMapping> = {
        let mut state = STATE.lock();
        let mut mine = Vec::new();
        state.foreign.retain(|m| {
            if m.process == process {
                mine.push(*m);
                false
            } else {
                true
            }
        });
        mine
    };
    for mapping in foreign {
        if unmap_fixed(&io, 0, mapping).is_err() {
            kept += 1;
        }
    }
    for (id, memory, mapping) in owned {
        if unmap_fixed(&io, memory, mapping).is_ok() {
            if let Ok(allocation) = STATE.lock().allocation_mut(id) {
                allocation
                    .mappings
                    .retain(|live| !(live.virt == mapping.virt && live.piece.va == mapping.piece.va));
            }
        } else {
            kept += 1;
        }
    }
    if kept == 0 {
        let space = STATE
            .lock()
            .spaces
            .iter()
            .find(|(owner, _)| *owner == process)
            .map(|(_, space)| *space);
        if let Some(space) = space {
            if let Some(window) = window_of(space) {
                if io.rm_free(nd::H_DEVICE, window).is_ok() {
                    STATE.lock().windows.retain(|(owner, _)| *owner != space);
                }
            }
            if io.rm_free(nd::H_DEVICE, space).is_ok() {
                STATE.lock().spaces.retain(|(owner, _)| *owner != process);
            } else {
                kept += 1;
            }
        }
        STATE.lock().idle_spaces.retain(|p| *p != process);
    }
    kept
}

/// Drop only guest-owned state after the transport already destroyed every RM
/// object.  No host request is sent and no pSystemMem page is unlocked early.
pub fn discard(id: u32) -> Result<(), Error> {
    let mut state = STATE.lock();
    let index = state
        .allocations
        .iter()
        .position(|a| a.id == id)
        .ok_or(Error::Invalid)?;
    let mut allocation = state.allocations.remove(index);
    drop(state);
    if let Some(cpu) = allocation.cpu.as_mut() {
        if cpu.kernel_va != 0 {
            // PASSIVE (DestroyAllocation); the host mapping is already gone.
            unsafe { MmUnmapIoSpace(cpu.kernel_va as *mut c_void, cpu.view.len) };
            cpu.kernel_va = 0;
        }
    }
    unlock_pins(&mut allocation.pins);
    Ok(())
}

/// Forget host objects after the transport-wide close sweep.  Kernel CPU views
/// and MDLs still belong to the guest and are released here, at PASSIVE.
pub fn forget(passive: PassiveLevel) {
    let Ok(_guard) = lock(passive) else {
        return;
    };
    let mut state = STATE.lock();
    let mut allocations = core::mem::take(&mut state.allocations);
    state.client = None;
    state.spaces.clear();
    state.windows.clear();
    state.foreign.clear();
    state.desc_pfns.clear();
    state.idle_spaces.clear();
    drop(state);
    for allocation in allocations.iter_mut() {
        if let Some(cpu) = allocation.cpu.as_mut() {
            if cpu.kernel_va != 0 {
                unsafe { MmUnmapIoSpace(cpu.kernel_va as *mut c_void, cpu.view.len) };
                cpu.kernel_va = 0;
            }
        }
        unlock_pins(&mut allocation.pins);
    }
}

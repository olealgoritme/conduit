//! Native Blackwell contexts and user-mode CUDA hardware queues.
//!
//! This is the I/O half of `helios_kmd_logic::native_channel`.  It uses the
//! ordinary `nvrm::forward` path and the same KMD-owned RM client as native
//! memory, so every host file is owned, accounted, allow-listed by the host's 610.57.04
//! RM policy, and reclaimed by the existing teardown machinery.  There is no
//! global channel: the native-memory client/VA space is shared by reference,
//! each HW context owns a TSG/context share, and each HW queue owns its channel
//! and USERD/doorbell mappings.
//!
//! A WDDM ring allocation is resolved by the allocation subsystem into
//! [`NativeRingBacking`] of the shared `native_memory` client.  The mapping is
//! already real and exact; channel creation never substitutes another VA or
//! duplicates it into a second client.

use super::gpu::DeviceOwner;
use super::native_memory::NativeClient;
use super::nvrm::{self, MapRefusal, Refusal};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::device::ProcessContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::ce_present::{self as cp, Token};
use helios_kmd_logic::native_channel as nc;
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{self as rc};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{PHYSICAL_ADDRESS, _MEMORY_CACHING_TYPE};

const TIMEOUT_MS: u64 = 2_500;
const BLACKWELL_COMPUTE_B: u32 = 0xcec0;
/// The GB20x 3D class (`clce97.h`).
const BLACKWELL_B: u32 = 0xce97;
const FIRST_NATIVE_HANDLE: u32 = 0x4e43_1000;
const HANDLE_STRIDE: u32 = 16;

static NEXT_NATIVE_HANDLE: AtomicU32 = AtomicU32::new(FIRST_NATIVE_HANDLE);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeError {
    Invalid,
    Busy,
    NoMemory,
    Unsupported,
    Transport,
    Rm,
    UnbackedRing,
}

impl From<nc::PrivateError> for NativeError {
    fn from(value: nc::PrivateError) -> Self {
        match value {
            nc::PrivateError::QueueFlags | nc::PrivateError::ProgressFence => Self::Unsupported,
            _ => Self::Invalid,
        }
    }
}

fn refusal(value: Refusal) -> NativeError {
    match value {
        Refusal::NoResources | Refusal::Transport(VirtioError::OutOfMemory) => {
            NativeError::NoMemory
        }
        Refusal::MsgType | Refusal::Forbidden => NativeError::Unsupported,
        Refusal::NotOwned | Refusal::BadRange => NativeError::Invalid,
        Refusal::Transport(_) => NativeError::Transport,
    }
}

fn map_refusal(value: MapRefusal) -> NativeError {
    match value {
        MapRefusal::NoResources => NativeError::NoMemory,
        MapRefusal::NotOwned | MapRefusal::BadRange => NativeError::Invalid,
        MapRefusal::Host(_) => NativeError::Rm,
        MapRefusal::Transport(e) => refusal(Refusal::Transport(e)),
    }
}

/// A live mapping in the shared native-memory VA space behind the D3DKMT
/// allocation named by CreateHwQueue. Its record and RM object outlive queue
/// creation and are reclaimed by native-memory teardown, not by the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeRingBacking {
    pub memory: u32,
    pub mapped_bytes: u64,
    pub va_space: u32,
    /// The ring's native allocation and the byte offset of the ring in it
    /// (diagnosis: the KMD reads the GPFIFO back through its own CPU view).
    pub id: u32,
    pub offset: u64,
}

/// Process-native state is taken out of its spinlock before any host call.
/// `Busy` therefore never appears at submit IRQL; submission uses only the
/// completed queue's own views.
pub(crate) enum NativeProcessState {
    Empty,
    Busy,
    Ready(NativeProcess),
}

pub(crate) struct NativeProcess {
    client: NativeClient,
    vaspace: u32,
    epoch: u64,
}

#[derive(Clone, Copy)]
struct Io<'a> {
    passive: PassiveLevel,
    adapter: &'a AdapterContext,
    epoch: u64,
}

impl Io<'_> {
    fn send(self, req: &[u8], resp: &mut [u8]) -> Result<usize, NativeError> {
        let mut seen = None;
        let n = nvrm::forward(
            self.passive,
            self.adapter,
            DeviceOwner::KMD_RM,
            0,
            req,
            resp,
            TIMEOUT_MS,
            0,
            0,
            &mut seen,
        )
        .map_err(refusal)?;
        if seen.is_some_and(|epoch| epoch != self.epoch) {
            return Err(NativeError::Transport);
        }
        Ok(n)
    }

    fn exchange(
        self,
        handle: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, NativeError> {
        let total = rc::MSG_HDR + rc::IOCTL_REQ + data.len() + nested.len();
        let mut req = Vec::new();
        req.try_reserve_exact(total)
            .map_err(|_| NativeError::NoMemory)?;
        req.resize(total, 0);
        let n = rc::build_ioctl(&mut req, handle, cmd, data, nested).ok_or(NativeError::Invalid)?;
        self.send(req.get(..n).ok_or(NativeError::Invalid)?, resp)
    }

    fn open_file(self, device_type: u32) -> Result<u32, NativeError> {
        let mut req = [0u8; 32];
        let n = rc::build_open(&mut req, device_type).ok_or(NativeError::Invalid)?;
        let mut resp = [0u8; 64];
        let len = self.send(req.get(..n).ok_or(NativeError::Invalid)?, &mut resp)?;
        let reply = resp.get(..len).ok_or(NativeError::Transport)?;
        if let Some(handle) = rc::parse_open_reply(reply) {
            return Ok(handle);
        }
        if let Some(handle) = rc::get32(reply, 4).filter(|h| *h != 0) {
            let _ = self.close_file(handle);
        }
        Err(NativeError::Rm)
    }

    fn close_file(self, handle: u32) -> Result<(), NativeError> {
        if handle == 0 {
            return Ok(());
        }
        let mut req = [0u8; 32];
        let n = rc::build_close(&mut req, handle).ok_or(NativeError::Invalid)?;
        let mut resp = [0u8; 64];
        let len = self.send(req.get(..n).ok_or(NativeError::Invalid)?, &mut resp)?;
        if resp
            .get(..len)
            .and_then(rc::reply_status)
            .is_some_and(|status| status == 0)
        {
            Ok(())
        } else {
            Err(NativeError::Rm)
        }
    }

    fn alloc(
        self,
        client: &NativeClient,
        parent: u32,
        new_handle: u32,
        class: u32,
        params: &[u8],
    ) -> Result<(), NativeError> {
        let block = rc::nvos64(client.root, parent, new_handle, class, params.len() as u32);
        let mut resp = [0u8; 512];
        let n = self.exchange(
            client.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            params,
            &mut resp,
        )?;
        rc::rm_reply(
            resp.get(..n).ok_or(NativeError::Transport)?,
            rc::NVOS64_STATUS_AT,
        )
        .map_err(|error| {
            // NvChAlCls/NvChAlSt: the class and RM status of a refused alloc.
            crate::diag::record_named_bytes(b"NvChAlCls", class);
            crate::diag::record_named_bytes(b"NvChAlSt", rm_error_code(error));
            NativeError::Rm
        })?;
        Ok(())
    }

    fn free(self, client: &NativeClient, parent: u32, object: u32) -> Result<(), NativeError> {
        if object == 0 {
            return Ok(());
        }
        let block = rc::nvos00(client.root, parent, object);
        let mut resp = [0u8; 128];
        let n = self.exchange(
            client.ctl,
            rc::nv_cmd(rc::ESC_RM_FREE, 16),
            &block,
            &[],
            &mut resp,
        )?;
        rc::rm_reply(
            resp.get(..n).ok_or(NativeError::Transport)?,
            rc::NVOS00_STATUS_AT,
        )
        .map_err(|_| NativeError::Rm)?;
        Ok(())
    }

    fn control(
        self,
        client: &NativeClient,
        object: u32,
        command: u32,
        params: &mut [u8],
    ) -> Result<(), NativeError> {
        let block = rc::nvos54(client.root, object, command, params.len() as u32);
        let mut resp = [0u8; 1024];
        let n = self.exchange(
            client.ctl,
            rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
            &block,
            params,
            &mut resp,
        )?;
        let reply = rc::rm_reply(
            resp.get(..n).ok_or(NativeError::Transport)?,
            rc::NVOS54_STATUS_AT,
        )
        .map_err(|error| {
            // NvChCtlCmd/NvChCtlSt: the command and RM status of a refused control.
            crate::diag::record_named_bytes(b"NvChCtlCmd", command);
            crate::diag::record_named_bytes(b"NvChCtlSt", rm_error_code(error));
            NativeError::Rm
        })?;
        let count = reply.nested.len().min(params.len());
        params[..count].copy_from_slice(&reply.nested[..count]);
        if count != params.len() {
            return Err(NativeError::Transport);
        }
        Ok(())
    }

    fn register_fd(self, client: &NativeClient, channel: u32) -> Result<(), NativeError> {
        let data = rc::register_fd_params(client.ctl);
        let mut resp = [0u8; 64];
        let command = rc::nv_cmd(rc::ESC_REGISTER_FD, 4);
        match self.exchange(channel, command, &data, &[], &mut resp) {
            Ok(n) => {
                // As in librmclient, an RM refusal of this advisory association
                // is non-fatal.  The subsequent map still reports its own error.
                let _ = resp.get(..n);
                Ok(())
            }
            Err(NativeError::Transport) => Err(NativeError::Transport),
            Err(_) => Ok(()),
        }
    }

    /// Quiesce every channel in this KMD client before a live VA replacement or
    /// teardown. This is a real host-RM wait, not a bookkeeping "idle" flag.
    fn idle_channels(self, client: &NativeClient) -> Result<(), NativeError> {
        let mut data = [0u8; 52];
        data[0..4].copy_from_slice(&client.root.to_le_bytes());
        data[4..8].copy_from_slice(&nd::H_DEVICE.to_le_bytes());
        let mut response = [0u8; 512];
        let n = self.exchange(client.gpu, rc::nv_cmd(0x41, 52), &data, &[], &mut response)?;
        let reply = response.get(..n).ok_or(NativeError::Transport)?;
        rc::rm_reply(reply, 48).map_err(|_| NativeError::Transport)?;
        Ok(())
    }
}

/// Stop new submissions and wait for outstanding work before touching live VAs.
pub(crate) fn quiesce(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
) -> Result<(), NativeError> {
    let (client, _) = super::native_memory::channel_client(passive, adapter, process)
        .map_err(|_| NativeError::Transport)?;
    let io = Io {
        passive,
        adapter,
        epoch: client.epoch,
    };
    io.idle_channels(&client)
}

fn process_slot(process: &ProcessContext) -> &SpinLock<NativeProcessState> {
    process.native_state()
}

fn replace_process(process: &ProcessContext, next: NativeProcessState) -> NativeProcessState {
    core::mem::replace(&mut *process_slot(process).lock(), next)
}

fn ensure_process(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: &ProcessContext,
) -> Result<(), NativeError> {
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    match replace_process(process, NativeProcessState::Busy) {
        NativeProcessState::Ready(previous) => {
            if previous.epoch == epoch && epoch != 0 {
                *process_slot(process).lock() = NativeProcessState::Ready(previous);
                return Ok(());
            }
            // A previous transport generation is simply forgotten here. Its
            // host files were closed by the transport sweep; old queues carry
            // the same epoch and refuse submission/teardown host calls.
            *process_slot(process).lock() = NativeProcessState::Empty;
        }
        NativeProcessState::Busy => {
            *process_slot(process).lock() = NativeProcessState::Busy;
            return Err(NativeError::Busy);
        }
        NativeProcessState::Empty => {}
    }
    if epoch == 0 {
        *process_slot(process).lock() = NativeProcessState::Empty;
        return Err(NativeError::Transport);
    }
    let (client, vaspace) = super::native_memory::channel_client(
        passive,
        adapter,
        process as *const ProcessContext as usize,
    )
    .map_err(|_| NativeError::Transport)?;
    if client.epoch != epoch {
        *process_slot(process).lock() = NativeProcessState::Empty;
        return Err(NativeError::Transport);
    }
    *process_slot(process).lock() = NativeProcessState::Ready(NativeProcess {
        client,
        vaspace,
        epoch,
    });
    Ok(())
}

fn take_process(process: &ProcessContext) -> Result<NativeProcess, NativeError> {
    match replace_process(process, NativeProcessState::Busy) {
        NativeProcessState::Ready(native) => Ok(native),
        NativeProcessState::Busy => {
            *process_slot(process).lock() = NativeProcessState::Busy;
            Err(NativeError::Busy)
        }
        NativeProcessState::Empty => {
            *process_slot(process).lock() = NativeProcessState::Empty;
            Err(NativeError::Transport)
        }
    }
}

fn restore_process(process: &ProcessContext, native: NativeProcess) {
    *process_slot(process).lock() = NativeProcessState::Ready(native);
}

fn validate_blackwell(io: Io<'_>, client: &NativeClient) -> Result<(), NativeError> {
    let mut params = Vec::new();
    params
        .try_reserve_exact(cc::CLASSLIST_BYTES)
        .map_err(|_| NativeError::NoMemory)?;
    params.resize(cc::CLASSLIST_BYTES, 0);
    io.control(client, nd::H_DEVICE, cc::CTRL_GET_CLASSLIST_V2, &mut params)?;
    let count = (rc::get32(&params, 0).unwrap_or(0) as usize).min(cc::CLASSLIST_MAX);
    let has = |class: u32| (0..count).any(|i| rc::get32(&params, 4 + 4 * i) == Some(class));
    if cc::gen_from_class_list(&params) != Some(cp::Gen::Gb202) || !has(BLACKWELL_COMPUTE_B) {
        return Err(NativeError::Unsupported);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ObjectHandles {
    first: u32,
}

impl ObjectHandles {
    fn mint() -> Result<Self, NativeError> {
        let first = NEXT_NATIVE_HANDLE.fetch_add(HANDLE_STRIDE, Ordering::Relaxed);
        if first < FIRST_NATIVE_HANDLE || first > 0x7fff_fff0 {
            return Err(NativeError::NoMemory);
        }
        Ok(Self { first })
    }

    fn at(self, index: u32) -> u32 {
        self.first + index
    }
}

/// One native WDDM HW context.  Its process outlives it by WDDM contract.
pub(crate) struct NativeContext {
    adapter: *mut AdapterContext,
    process: *mut ProcessContext,
    owner: DeviceOwner,
    epoch: u64,
    pub(crate) tsg: u32,
    pub(crate) ctxshare: u32,
    /// TSG scheduling is shared by every queue of the context and uses the
    /// 610.57.04-allow-listed NVA06C control, not the unlisted channel control.
    scheduled: AtomicU32,
}

impl NativeContext {
    pub(crate) fn owner(&self) -> DeviceOwner {
        self.owner
    }

    pub(crate) fn create(
        passive: PassiveLevel,
        adapter: &AdapterContext,
        process: &mut ProcessContext,
        owner: DeviceOwner,
    ) -> Result<Self, NativeError> {
        ensure_process(passive, adapter, process)?;
        let handles = ObjectHandles::mint()?;
        let tsg = handles.at(0);
        let ctxshare = handles.at(1);
        let mut native = take_process(process)?;
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        let result = (|| {
            validate_blackwell(io, &native.client)?;
            io.alloc(
                &native.client,
                nd::H_DEVICE,
                tsg,
                cc::KEPLER_CHANNEL_GROUP_A,
                &cc::channel_group_params(native.vaspace, cc::ENGINE_GRAPHICS),
            )?;
            if let Err(error) = io.alloc(
                &native.client,
                tsg,
                ctxshare,
                cc::FERMI_CONTEXT_SHARE_A,
                &cc::ctxshare_params(native.vaspace),
            ) {
                let _ = io.free(&native.client, nd::H_DEVICE, tsg);
                return Err(error);
            }
            Ok(())
        })();
        let epoch = native.epoch;
        restore_process(process, native);
        result?;
        Ok(Self {
            adapter: adapter as *const AdapterContext as *mut AdapterContext,
            process,
            owner,
            epoch,
            tsg,
            ctxshare,
            scheduled: AtomicU32::new(0),
        })
    }

    pub(crate) fn destroy(&mut self, passive: PassiveLevel) -> Result<(), NativeError> {
        let Some(adapter) = (unsafe { self.adapter.as_ref() }) else {
            return Ok(());
        };
        let Some(process) = (unsafe { self.process.as_mut() }) else {
            return Ok(());
        };
        let mut native = match take_process(process) {
            Ok(native) => native,
            // The transport-wide close already reclaimed the whole client.
            Err(NativeError::Transport) => return Ok(()),
            Err(error) => return Err(error),
        };
        if native.epoch != self.epoch {
            restore_process(process, native);
            return Ok(());
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        if self.scheduled.swap(0, Ordering::AcqRel) != 0 {
            let mut params = cc::schedule_params(false);
            let _ = io.control(
                &native.client,
                self.tsg,
                cc::CTRL_GPFIFO_SCHEDULE,
                &mut params,
            );
        }
        // Queue destruction precedes context destruction, so this also frees
        // the context share without any live channel beneath it.
        let result = io.free(&native.client, nd::H_DEVICE, self.tsg);
        restore_process(process, native);
        result
    }
}

#[derive(Clone, Copy, Default)]
struct CpuView {
    map_ch: u32,
    cookie: u64,
    host_id: u32,
    host_mapped: bool,
    va: u64,
    len: u64,
    node_dev: u32,
    parent: u32,
    mem: u32,
}

#[derive(Default)]
struct QueueParts {
    handles: Option<ObjectHandles>,
    ctl_allocated: bool,
    ctl: CpuView,
    usermode_allocated: bool,
    usermode: CpuView,
    channel_allocated: bool,
    token: u32,
}

impl QueueParts {
    /// The transport is gone: only drop local VA mappings. Every host handle
    /// and host mapping has already been reclaimed by the transport sweep.
    fn drop_views(&mut self) {
        for view in [&mut self.usermode, &mut self.ctl] {
            if view.va != 0 {
                // SAFETY: this exact MmMapIoSpace view belongs to the old
                // generation and is removed without touching its replacement.
                unsafe { MmUnmapIoSpace(view.va as *mut c_void, view.len) };
                view.va = 0;
            }
        }
    }

    fn destroy(&mut self, io: Io<'_>, client: &NativeClient, tsg: u32) -> Result<(), NativeError> {
        let Some(handles) = self.handles else {
            return Ok(());
        };
        if self.channel_allocated {
            // Never unmap USERD, the doorbell, or the ring while RM may still
            // own a live channel which can address them.
            io.free(client, tsg, handles.at(5))?;
            self.channel_allocated = false;
        }
        cpu_unmap(io, client, &mut self.usermode)?;
        if self.usermode_allocated {
            io.free(client, nd::H_SUBDEVICE, handles.at(4))?;
            self.usermode_allocated = false;
        }
        cpu_unmap(io, client, &mut self.ctl)?;
        if self.ctl_allocated {
            io.free(client, nd::H_DEVICE, handles.at(3))?;
            self.ctl_allocated = false;
        }
        Ok(())
    }
}

/// One native queue.  The hot submission path touches only `userd`, `usermode`,
/// `token`, `ring_bytes`, and `gp_put`; everything else is PASSIVE teardown
/// state.  The progress-fence addresses are OS-supplied GPU-write storage and
/// are never written here; the NVIDIA personality's progress poll
/// (`ddi::hws`) only READS the CPU view, to tell dxgkrnl when it moved.
pub(crate) struct NativeQueue {
    adapter: *mut AdapterContext,
    context: *mut NativeContext,
    epoch: u64,
    parts: QueueParts,
    ring_bytes: u32,
    ring_id: u32,
    ring_offset: u64,
    gp_put: SpinLock<u32>,
    progress_fence_cpu: u64,
    _progress_fence_gpu: u64,
}

impl NativeQueue {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create(
        passive: PassiveLevel,
        adapter: &AdapterContext,
        context: &NativeContext,
        backing: NativeRingBacking,
        queue: &nc::QueuePrivate,
        queue_flags: u32,
        progress_cpu: u64,
        progress_gpu: u64,
    ) -> Result<Self, NativeError> {
        nc::validate_queue_flags(queue_flags)?;
        nc::validate_progress_fence(progress_cpu, progress_gpu)?;
        if backing.memory == 0 || backing.mapped_bytes < u64::from(queue.ring_bytes) {
            return Err(NativeError::UnbackedRing);
        }
        let process = unsafe { context.process.as_mut() }.ok_or(NativeError::Invalid)?;
        let handles = ObjectHandles::mint()?;
        let mut native = take_process(process)?;
        if native.epoch != context.epoch
            || (backing.va_space != 0 && backing.va_space != native.vaspace)
        {
            crate::diag::record_named_bytes(b"NvQStep", 0x80);
            crate::diag::record_named_bytes(b"NvQSpaces", backing.va_space ^ native.vaspace);
            restore_process(process, native);
            return Err(NativeError::Transport);
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        let mut parts = QueueParts {
            handles: Some(handles),
            ..QueueParts::default()
        };
        let result = create_queue_objects(
            io,
            &native.client,
            context.tsg,
            context.ctxshare,
            &context.scheduled,
            queue,
            &mut parts,
        );
        restore_process(process, native);
        result?;
        Ok(Self {
            adapter: context.adapter,
            context: context as *const NativeContext as *mut NativeContext,
            epoch: context.epoch,
            parts,
            ring_bytes: queue.ring_bytes,
            ring_id: backing.id,
            ring_offset: backing.offset,
            gp_put: SpinLock::new(0),
            progress_fence_cpu: progress_cpu,
            _progress_fence_gpu: progress_gpu,
        })
    }

    /// Read the progress fence's current value through the kernel CPU view
    /// dxgkrnl supplied at CreateHwQueue (`HwQueueProgressFenceCPUVirtualAddress`,
    /// documented as a kernel-mode VA). Any IRQL; one volatile 64-bit load.
    pub(crate) fn read_progress(&self) -> u64 {
        if self.progress_fence_cpu == 0 || self.progress_fence_cpu % 8 != 0 {
            return 0;
        }
        // SAFETY: nonzero (validated at creation) and 8-aligned (checked above),
        // and live until DestroyHwQueue returns; the poll unregisters a queue
        // before destroying it.
        unsafe { (self.progress_fence_cpu as *const u64).read_volatile() }
    }

    pub(crate) fn token(&self) -> Token {
        Token(self.parts.token)
    }

    /// DISPATCH-safe: one spinlock and three volatile stores/barriers.  No RM
    /// message, allocation, completion write, or host round trip is possible.
    pub(crate) fn submit(&self, private: &[u8]) -> Result<(), NativeError> {
        let adapter = unsafe { self.adapter.as_ref() }.ok_or(NativeError::Transport)?;
        let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
        if epoch == 0 || epoch != self.epoch {
            return Err(NativeError::Transport);
        }
        let mut previous = self.gp_put.lock();
        let submit = nc::parse_submit_private(private, *previous, self.ring_bytes)
            .map_err(|_| NativeError::Invalid)?;
        let ctl = self.parts.ctl.va;
        let doorbell = self.parts.usermode.va;
        if ctl == 0 || doorbell == 0 || self.parts.token == 0 {
            return Err(NativeError::Transport);
        }
        // SAFETY: the completed queue owns these live kernel views.  USERD is
        // at 0x1000 of the control view and GP_PUT is four bytes at +0x8c.
        unsafe {
            write32(
                ctl + cc::USERD_OFFSET + u64::from(cp::USERD_GP_PUT),
                submit.new_gp_put,
            )
        };
        full_barrier();
        // SAFETY: NOTIFY_CHANNEL_PENDING is four bytes at +0x90 in the 64-KiB
        // BLACKWELL_USERMODE_A mapping.  This is the RM-issued real token.
        unsafe { write32(doorbell + u64::from(cp::DOORBELL_OFFSET), self.parts.token) };
        *previous = submit.new_gp_put;
        Ok(())
    }

    pub(crate) fn destroy(&mut self, passive: PassiveLevel) -> Result<(), NativeError> {
        // Where the GPU got to (NvQGpGet/NvQGpPut from USERD, NvQFence the
        // progress fence), recorded for the last queue torn down.
        if self.parts.ctl.va != 0 && *self.gp_put.lock() != 0 {
            // Only queues that submitted: the values describe a real submission.
            // SAFETY: the queue's live control view; USERD GP_GET/GP_PUT are
            // four bytes each inside it.
            let (get, put) = unsafe {
                let userd = self.parts.ctl.va + cc::USERD_OFFSET;
                (
                    ((userd + u64::from(cp::USERD_GP_GET)) as *const u32).read_volatile(),
                    ((userd + u64::from(cp::USERD_GP_PUT)) as *const u32).read_volatile(),
                )
            };
            crate::diag::record_named_bytes(b"NvQGpGet", get);
            crate::diag::record_named_bytes(b"NvQGpPut", put);
            crate::diag::record_named_bytes(b"NvQFence", self.read_progress() as u32);
            crate::diag::record_named_bytes(b"NvQToken", self.parts.token);
            // The channel's error notifier (NvNotification at ctl+0): info32 at
            // +8, info16|status<<16 at +12. Nonzero status = RM put it in error.
            // SAFETY: the same live control view.
            let (info32, word) = unsafe {
                let ctl = self.parts.ctl.va;
                (
                    ((ctl + 8) as *const u32).read_volatile(),
                    ((ctl + 12) as *const u32).read_volatile(),
                )
            };
            crate::diag::record_named_bytes(b"NvQErrInfo", info32);
            crate::diag::record_named_bytes(b"NvQErrSt", word);
            // The first two GPFIFO entries as RM memory holds them: zero means
            // the UMD's CPU writes never reached the memory the GPU fetches.
            if let Some(adapter) = unsafe { self.adapter.as_ref() } {
                let mut words = [0u32; 4];
                // SAFETY: PASSIVE (DestroyHwQueue); copies 16 bytes out.
                let read = unsafe {
                    super::native_memory::with_cpu_range(
                        passive,
                        adapter,
                        self.ring_id,
                        self.ring_offset,
                        16,
                        |relative, native, len| {
                            let at = relative as usize / 4;
                            let n = (len as usize / 4).min(4usize.saturating_sub(at));
                            for i in 0..n {
                                words[at + i] = (native as *const u32).add(i).read_volatile();
                            }
                            true
                        },
                    )
                };
                crate::diag::record_named_bytes(b"NvQRingRd", u32::from(read.is_ok_and(|ok| ok)));
                crate::diag::record_named_bytes(b"NvQRing0", words[0]);
                crate::diag::record_named_bytes(b"NvQRing1", words[1]);
                crate::diag::record_named_bytes(b"NvQRing2", words[2]);
            }
        }
        let Some(adapter) = (unsafe { self.adapter.as_ref() }) else {
            return Ok(());
        };
        let Some(context) = (unsafe { self.context.as_ref() }) else {
            return Ok(());
        };
        let Some(process) = (unsafe { context.process.as_mut() }) else {
            return Ok(());
        };
        let mut native = match take_process(process) {
            Ok(native) => native,
            Err(NativeError::Transport) => {
                self.parts.drop_views();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if native.epoch != self.epoch {
            self.parts.drop_views();
            restore_process(process, native);
            return Ok(());
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        let result = self.parts.destroy(io, &native.client, context.tsg);
        restore_process(process, native);
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn create_queue_objects(
    io: Io<'_>,
    client: &NativeClient,
    tsg: u32,
    ctxshare: u32,
    scheduled: &AtomicU32,
    queue: &nc::QueuePrivate,
    parts: &mut QueueParts,
) -> Result<(), NativeError> {
    let handles = parts.handles.ok_or(NativeError::Invalid)?;
    let result = (|| {
        crate::diag::record_named_bytes(b"NvQStep", 1);
        io.alloc(
            client,
            nd::H_DEVICE,
            handles.at(3),
            cc::NV01_MEMORY_SYSTEM,
            &helios_kmd_logic::rm_sysmem::params(
                client.root,
                helios_kmd_logic::rm_sysmem::Cache::Cached,
                cc::CTL_BYTES,
            ),
        )?;
        parts.ctl_allocated = true;
        crate::diag::record_named_bytes(b"NvQStep", 2);
        parts.ctl = cpu_map(
            io,
            client,
            nd::H_DEVICE,
            handles.at(3),
            cc::MapNode::Ctl,
            cc::CTL_BYTES,
            _MEMORY_CACHING_TYPE::MmCached,
        )?;
        zero(parts.ctl.va, cc::CTL_BYTES);

        crate::diag::record_named_bytes(b"NvQStep", 3);
        io.alloc(
            client,
            nd::H_SUBDEVICE,
            handles.at(4),
            cp::Gen::Gb202.classes().usermode,
            &cc::usermode_params(),
        )?;
        parts.usermode_allocated = true;
        crate::diag::record_named_bytes(b"NvQStep", 4);
        parts.usermode = cpu_map(
            io,
            client,
            nd::H_SUBDEVICE,
            handles.at(4),
            cc::MapNode::Gpu,
            cp::USERMODE_BYTES,
            _MEMORY_CACHING_TYPE::MmNonCached,
        )?;

        crate::diag::record_named_bytes(b"NvQStep", 5);
        let channel = cc::channel_params(&cc::ChannelDesc {
            h_ctl: handles.at(3),
            gpfifo_va: queue.gpfifo_va,
            entries: queue.entries(),
            h_ctxshare: ctxshare,
            userd_offset: cc::USERD_OFFSET,
            engine_type: cc::ENGINE_GRAPHICS,
        });
        io.alloc(
            client,
            tsg,
            handles.at(5),
            cp::Gen::Gb202.classes().gpfifo,
            &channel,
        )?;
        parts.channel_allocated = true;
        crate::diag::record_named_bytes(b"NvQStep", 6);
        let mut bind = cc::bind_params(cc::ENGINE_GRAPHICS);
        io.control(client, handles.at(5), cc::CTRL_BIND, &mut bind)?;
        io.alloc(
            client,
            handles.at(5),
            handles.at(6),
            BLACKWELL_COMPUTE_B,
            &[0u8; 16],
        )?;
        // The D3D UMDs drive the 3D class on subchannel 0 and the graphics copy
        // engine on subchannel 4 (they never SET_OBJECT them; their pushbuffers
        // assume both). RM maps a class's GR context buffers when its object is
        // allocated (kgrctxMapCtxBuffers), so without a 3D object the 3D
        // pipeline wrote through unset context addresses (Xid 31 at VA 0,
        // HUBCLIENT_SCC_NB). Best effort: NvCh3dSt / NvChCeSt record RM's
        // answer (0 = allocated).
        let three_d = io.alloc(client, handles.at(5), handles.at(7), BLACKWELL_B, &[0u8; 16]);
        crate::diag::record_named_bytes(b"NvCh3dSt", u32::from(three_d.is_err()));
        // The copy object must be the graphics runlist's copy engine (GRCE):
        // engine type 0 let RM bind an async CE, and every method the UMD sent
        // to subchannel 4 raised PBDMA DEVICE (Xid 32, SET_RENDER_ENABLE_C).
        let grce = grce_engine(io, client).unwrap_or(0);
        crate::diag::record_named_bytes(b"NvChGrce", grce);
        let copy = io.alloc(
            client,
            handles.at(5),
            handles.at(8),
            cp::Gen::Gb202.classes().copy,
            &cc::ce_object_params(grce),
        );
        crate::diag::record_named_bytes(b"NvChCeSt", u32::from(copy.is_err()));
        crate::diag::record_named_bytes(b"NvQStep", 8);
        let mut index = cc::token_notif_index_params();
        io.control(
            client,
            handles.at(5),
            cc::CTRL_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX,
            &mut index,
        )?;
        crate::diag::record_named_bytes(b"NvQStep", 9);
        let mut token = cc::token_params();
        io.control(
            client,
            handles.at(5),
            cc::CTRL_GET_WORK_SUBMIT_TOKEN,
            &mut token,
        )?;
        parts.token = cc::parse_token(&token)
            .filter(|token| token.0 != 0)
            .ok_or(NativeError::Transport)?
            .0;
        crate::diag::record_named_bytes(b"NvQStep", 10);
        if scheduled
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let mut schedule = cc::schedule_params(true);
            if let Err(error) = io.control(client, tsg, cc::CTRL_GPFIFO_SCHEDULE, &mut schedule) {
                scheduled.store(0, Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = parts.destroy(io, client, tsg);
        return Err(error);
    }
    Ok(())
}

const MAPPED: u64 = 1 << 63;

fn cpu_map(
    io: Io<'_>,
    client: &NativeClient,
    parent: u32,
    memory: u32,
    node: cc::MapNode,
    len: u64,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Result<CpuView, NativeError> {
    let mut view = CpuView {
        len,
        parent,
        mem: memory,
        node_dev: match node {
            cc::MapNode::Ctl => rc::DEV_CTL,
            cc::MapNode::Gpu => client.minor,
        },
        ..CpuView::default()
    };
    let result = (|| {
        view.map_ch = io.open_file(view.node_dev)?;
        if node == cc::MapNode::Gpu {
            io.register_fd(client, view.map_ch)?;
        }
        let block = rc::nvos33_with_fd(client.root, parent, memory, 0, len, view.map_ch);
        let mut resp = [0u8; 160];
        let n = io.exchange(
            client.ctl,
            rc::nv_cmd(rc::ESC_RM_MAP_MEMORY, rc::NVOS33_FD_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )?;
        let reply = rc::rm_reply(
            resp.get(..n).ok_or(NativeError::Transport)?,
            rc::NVOS33_STATUS_AT,
        )
        .map_err(|_| NativeError::Rm)?;
        view.cookie = rc::map_cookie(&reply).ok_or(NativeError::Transport)? | MAPPED;
        let host = nvrm::host_mmap_within(
            io.passive,
            io.adapter,
            DeviceOwner::KMD_RM,
            view.map_ch,
            true,
            0,
            len,
            TIMEOUT_MS,
        )
        .map_err(map_refusal)?;
        view.host_id = host.host_id;
        view.host_mapped = true;
        if host.size < len {
            return Err(NativeError::Transport);
        }
        view.va = kernel_map(io.adapter, view.node_dev, host.offset, len, cache)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = cpu_unmap(io, client, &mut view);
        return Err(error);
    }
    Ok(view)
}

fn kernel_map(
    adapter: &AdapterContext,
    node_dev: u32,
    offset: u64,
    len: u64,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Result<u64, NativeError> {
    let region = nvrm::region_for(adapter, node_dev).ok_or(NativeError::Unsupported)?;
    let physical = helios_kmd_logic::window_units::place(region.base, region.len, offset, len)
        .ok_or(NativeError::Invalid)?;
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = physical as i64;
    // SAFETY: PASSIVE; `place` proved the page-aligned range is inside the
    // host-visible region occupied by this mapping.
    let va = unsafe { MmMapIoSpace(pa, len, cache) };
    if va.is_null() {
        return Err(NativeError::NoMemory);
    }
    Ok(va as u64)
}

fn cpu_unmap(io: Io<'_>, client: &NativeClient, view: &mut CpuView) -> Result<(), NativeError> {
    if view.va != 0 {
        // SAFETY: this exact view came from MmMapIoSpace and is removed once.
        unsafe { MmUnmapIoSpace(view.va as *mut c_void, view.len) };
        view.va = 0;
    }
    if view.host_mapped {
        nvrm::release_host_map_within(
            io.passive,
            io.adapter,
            view.map_ch,
            view.host_id,
            TIMEOUT_MS,
        )
        .map_err(|_| NativeError::Transport)?;
        view.host_mapped = false;
    }
    if view.cookie & MAPPED != 0 {
        let block = rc::nvos34(client.root, view.parent, view.mem, view.cookie & !MAPPED);
        let mut resp = [0u8; 128];
        let n = io.exchange(
            client.ctl,
            rc::nv_cmd(rc::ESC_RM_UNMAP_MEMORY, rc::NVOS34_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )?;
        let reply = resp.get(..n).ok_or(NativeError::Transport)?;
        rc::rm_reply(reply, rc::NVOS34_STATUS_AT).map_err(|_| NativeError::Transport)?;
        view.cookie = 0;
    }
    io.close_file(view.map_ch)?;
    view.map_ch = 0;
    Ok(())
}

fn zero(base: u64, len: u64) {
    let mut offset = 0;
    while offset + 8 <= len {
        // SAFETY: the caller mapped `len` bytes and all offsets are 8-aligned.
        unsafe { write64(base + offset, 0) };
        offset += 8;
    }
    full_barrier();
}

/// The first GRCE (graphics-runlist copy engine) RM lists, cached.
static GRCE_ENGINE: AtomicU32 = AtomicU32::new(0);

fn grce_engine(io: Io<'_>, client: &NativeClient) -> Option<u32> {
    let known = GRCE_ENGINE.load(Ordering::Relaxed);
    if known != 0 {
        return Some(known);
    }
    let mut list = Vec::new();
    list.try_reserve_exact(cc::ENGINES_BYTES).ok()?;
    list.resize(cc::ENGINES_BYTES, 0u8);
    io.control(client, nd::H_SUBDEVICE, cc::CTRL_GET_ENGINES_V2, &mut list).ok()?;
    let mut types = [0u32; cc::MAX_CAPS_QUERIES];
    let n = cc::copy_engines(&list, &mut types);
    for &t in types.get(..n).unwrap_or(&[]) {
        let mut params = cc::ce_caps_params(t);
        if io.control(client, nd::H_SUBDEVICE, cc::CTRL_CE_GET_CAPS_V2, &mut params).is_ok() {
            if let Some(caps) = cc::parse_ce_caps(&params, t) {
                if caps.is_grce() {
                    GRCE_ENGINE.store(t, Ordering::Relaxed);
                    return Some(t);
                }
            }
        }
    }
    None
}

fn rm_error_code(error: rc::RmError) -> u32 {
    match error {
        rc::RmError::Status(status) => status,
        rc::RmError::Host(errno) => 0x8000_0000 | errno,
        rc::RmError::Malformed => 0xffff_ffff,
    }
}

unsafe fn write32(address: u64, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) };
}

unsafe fn write64(address: u64, value: u64) {
    unsafe { (address as *mut u64).write_volatile(value) };
}

fn full_barrier() {
    // SAFETY: SSE2 is baseline on the only supported target.
    unsafe { core::arch::x86_64::_mm_mfence() };
}

/// Forget the process's association after all contexts and queues are gone.
/// The shared native-memory client and VA space have transport lifetime and are
/// reclaimed by `native_memory::forget`, never closed per process here.
pub(crate) fn destroy_process(
    _passive: PassiveLevel,
    _adapter: &AdapterContext,
    process: &mut ProcessContext,
) -> Result<(), NativeError> {
    match replace_process(process, NativeProcessState::Empty) {
        NativeProcessState::Empty | NativeProcessState::Ready(_) => Ok(()),
        NativeProcessState::Busy => {
            *process_slot(process).lock() = NativeProcessState::Busy;
            Err(NativeError::Busy)
        }
    }
}

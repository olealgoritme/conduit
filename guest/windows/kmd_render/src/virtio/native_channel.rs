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

pub(crate) mod pg_channel;

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
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::ce_present::{self as cp, Token};
use helios_kmd_logic::native_channel as nc;
use helios_kmd_logic::native_teardown as nt;
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{self as rc};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{PHYSICAL_ADDRESS, _MEMORY_CACHING_TYPE};

const TIMEOUT_MS: u64 = 2_500;
const BLACKWELL_COMPUTE_B: u32 = 0xcec0;
/// (subchannel, class) the D3D UMDs drive on a graphics channel: 3D, compute,
/// inline-to-memory, copy (the copy class from the generation table).
const BINDINGS: [(u32, u32); 4] = [(0, 0xce97), (1, 0xcec0), (2, 0xcd40), (4, 0)];
/// The GB20x 3D class (`clce97.h`).
const BLACKWELL_B: u32 = 0xce97;
/// The GB100 inline-to-memory class (`clcd40.h`).
const BLACKWELL_INLINE_TO_MEMORY_A: u32 = 0xcd40;
/// The 2D class (`cl902d.h`).
const FERMI_TWOD_A: u32 = 0x902d;
/// RM's software-method class (`cl5080.h`), bound by the UMDs on subchannel 5.
const NV50_DEFERRED_API_CLASS: u32 = 0x5080;
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
    /// The queue was recovered from a fault or hang (`ddi::hws::recover_native`):
    /// nothing reaches its channel any more.
    Dead,
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

/// A process's native association (`nt::Slot`): one thread initializes it, every
/// create and destroy works on a copy, so concurrent teardowns of one process
/// never refuse each other. Submission uses only the completed queue's views.
pub(crate) type NativeProcessState = nt::Slot<NativeProcess>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeProcess {
    client: NativeClient,
    vaspace: u32,
    epoch: u64,
}

/// How long a create or destroy waits out another thread's initialization of the
/// process (`sleep_ms` rounds up to the timer period, ~16 ms: about 5 s).
const SLOT_WAIT_TRIES: u32 = 320;

/// RM objects failed teardowns left allocated, freed again at DestroyProcess
/// (dxgkrnl never repeats a Destroy DDI).
static LEDGER: SpinLock<nt::Ledger<64>> = SpinLock::new(nt::Ledger::new());
/// `NvTdFiled` objects filed, `NvTdFreed` freed at DestroyProcess, `NvTdLost` neither
/// (another generation, refused again, no client, or a slot that never came up).
static TD_FILED: AtomicU32 = AtomicU32::new(0);
static TD_FREED: AtomicU32 = AtomicU32::new(0);
static TD_LOST: AtomicU32 = AtomicU32::new(0);

/// RM objects one queue teardown could not free: (parent, object), children first.
#[derive(Clone, Copy, Default)]
struct Left {
    items: [(u32, u32); 3],
    n: usize,
}

impl Left {
    fn add(&mut self, parent: u32, object: u32) {
        if let Some(slot) = self.items.get_mut(self.n) {
            *slot = (parent, object);
            self.n += 1;
        }
    }
}

fn file_leftover(process: usize, epoch: u64, parent: u32, object: u32) {
    let filed = LEDGER.lock().push(nt::Leftover {
        process,
        epoch,
        parent,
        object,
    });
    if filed {
        TD_FILED.fetch_add(1, Ordering::Relaxed);
    } else {
        TD_LOST.fetch_add(1, Ordering::Relaxed);
    }
}

fn file_leftovers(process: usize, epoch: u64, left: &mut Left) {
    for &(parent, object) in left.items.get(..left.n).unwrap_or(&[]) {
        file_leftover(process, epoch, parent, object);
    }
    *left = Left::default();
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
        // NVOS30_PARAMETERS: 4 handles, 3 NvP64 arrays, flags, timeout, status = 52
        // bytes, 56 with the struct's 8-byte alignment (the size the ioctl encodes).
        let mut data = [0u8; NVOS30_BYTES];
        data[0..4].copy_from_slice(&client.root.to_le_bytes());
        data[4..8].copy_from_slice(&nd::H_DEVICE.to_le_bytes());
        let mut response = [0u8; 512];
        let n = self.exchange(
            client.gpu,
            rc::nv_cmd(0x41, NVOS30_BYTES as u32),
            &data,
            &[],
            &mut response,
        )?;
        let reply = response.get(..n).ok_or(NativeError::Transport)?;
        rc::rm_reply(reply, 48).map_err(|_| NativeError::Transport)?;
        Ok(())
    }
}

/// `sizeof(NVOS30_PARAMETERS)` on x86-64 (`NV_ESC_RM_IDLE_CHANNELS`).
const NVOS30_BYTES: usize = 56;

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

/// The process's association for the live transport, initializing it on first use;
/// another thread's initialization is waited out.
fn ensure_process(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: &ProcessContext,
) -> Result<NativeProcess, NativeError> {
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    for _ in 0..SLOT_WAIT_TRIES {
        // A previous transport generation is simply forgotten here. Its host files
        // were closed by the transport sweep; old queues carry the same epoch and
        // refuse submission/teardown host calls.
        let begin = process_slot(process)
            .lock()
            .begin(|native| native.epoch == epoch && epoch != 0);
        match begin {
            nt::Begin::Ready(native) => return Ok(native),
            nt::Begin::Wait => super::ctrl::sleep_ms(passive, 1),
            nt::Begin::Initialize => {
                let result = if epoch == 0 {
                    Err(NativeError::Transport)
                } else {
                    super::native_memory::channel_client(
                        passive,
                        adapter,
                        process as *const ProcessContext as usize,
                    )
                    .map_err(|_| NativeError::Transport)
                    .and_then(|(client, vaspace)| {
                        if client.epoch == epoch {
                            Ok(NativeProcess {
                                client,
                                vaspace,
                                epoch,
                            })
                        } else {
                            Err(NativeError::Transport)
                        }
                    })
                };
                process_slot(process).lock().finish(result.as_ref().ok().copied());
                return result;
            }
        }
    }
    Err(NativeError::Busy)
}

/// A copy of the process's ready association (`Transport` when it has none),
/// waiting out another thread's initialization.
fn peek_process(passive: PassiveLevel, process: &ProcessContext) -> Result<NativeProcess, NativeError> {
    for _ in 0..SLOT_WAIT_TRIES {
        let peek = process_slot(process).lock().peek();
        match peek {
            nt::Peek::Ready(native) => return Ok(native),
            nt::Peek::Empty => return Err(NativeError::Transport),
            nt::Peek::Wait => super::ctrl::sleep_ms(passive, 1),
        }
    }
    Err(NativeError::Busy)
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
    /// dxgkrnl's own handle for this context (`DXGKARG_CREATECONTEXT.hContext` on
    /// input), which `DxgkCbInvalidateHwContext` names; 0 = unknown.
    dxg_context: usize,
    /// Live graphics-kind queues (channels on VEID 0).
    graphics_queues: AtomicU32,
    /// VEIDs taken by compute-only channels of this TSG (bit n = VEID n).
    veids: AtomicU64,
    /// `NvPreempt`: the TSG's preemption modes, set once, and its GfxP buffers.
    preempt: Preempt,
    /// The TSG's hardware id (`NVA06C_CTRL_CMD_GET_INFO`), asked once by a hang snapshot
    /// (`ddi::native_snap`); u32::MAX = not asked yet.
    hw_tsg: AtomicU32,
}

/// A TSG's preemption state (`NvPreempt`, `helios_kmd_logic::ctxsw_preempt`): the gate
/// (`ctxsw_preempt::gate`) and the GfxP buffers bound to its GR context, memory `i` at
/// handle `first + i`, its virtual allocation at `first + BIND_BUFFERS + i`.
#[derive(Default)]
struct Preempt {
    gate: AtomicU32,
    /// First handle of the buffers' block; 0 = none.
    first: AtomicU32,
    /// Allocated objects: bit `i` memory `i`, bit `BIND_BUFFERS + i` its virtual allocation.
    live: AtomicU32,
}

impl NativeContext {
    pub(crate) fn set_dxg_context(&mut self, handle: usize) {
        self.dxg_context = handle;
    }

    pub(crate) fn dxg_context(&self) -> usize {
        self.dxg_context
    }

    /// Take the context's TSG off the runlist (`NVA06C_CTRL_CMD_GPFIFO_SCHEDULE`
    /// false): RM preempts it and runs nothing of it again. Recovery of a faulted or
    /// hung context; idempotent (a context never scheduled, or already taken off,
    /// sends nothing). PASSIVE.
    pub(crate) fn unschedule(&self, passive: PassiveLevel) -> Result<(), NativeError> {
        if self.scheduled.swap(0, Ordering::AcqRel) == 0 {
            return Ok(());
        }
        let tsg = self.tsg;
        self.with_client(passive, |io, client| {
            let mut params = cc::schedule_params(false);
            io.control(client, tsg, cc::CTRL_GPFIFO_SCHEDULE, &mut params)
        })
    }

    /// Run `f` with this context's RM client, if its transport generation is live.
    fn with_client(
        &self,
        passive: PassiveLevel,
        f: impl FnOnce(Io<'_>, &NativeClient) -> Result<(), NativeError>,
    ) -> Result<(), NativeError> {
        let Some(adapter) = (unsafe { self.adapter.as_ref() }) else {
            return Err(NativeError::Transport);
        };
        let Some(process) = (unsafe { self.process.as_ref() }) else {
            return Err(NativeError::Transport);
        };
        // A copy of the association: the slot stays usable by other threads.
        let native = peek_process(passive, process)?;
        if native.epoch != self.epoch {
            return Err(NativeError::Transport);
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        f(io, &native.client)
    }

    pub(crate) fn owner(&self) -> DeviceOwner {
        self.owner
    }

    /// The TSG's hardware id (the `TSGID` a host Xid 109 names), asked of RM once and kept;
    /// 0xffff when RM refused. PASSIVE (`ddi::native_snap`).
    pub(crate) fn hw_tsg_id(&self, passive: PassiveLevel) -> u32 {
        let known = self.hw_tsg.load(Ordering::Relaxed);
        if known != u32::MAX {
            return known;
        }
        let tsg = self.tsg;
        let mut id = 0xffffu32;
        let _ = self.with_client(passive, |io, client| {
            let mut info = [0u8; 4];
            io.control(client, tsg, helios_kmd_logic::ctxsw_preempt::CTRL_TSG_GET_INFO, &mut info)?;
            id = u32::from_le_bytes(info);
            Ok(())
        });
        self.hw_tsg.store(id, Ordering::Relaxed);
        id
    }

    /// The process key native memory files this context's mappings under.
    pub(crate) fn process_key(&self) -> usize {
        self.process as usize
    }

    pub(crate) fn create(
        passive: PassiveLevel,
        adapter: &AdapterContext,
        process: &mut ProcessContext,
        owner: DeviceOwner,
    ) -> Result<Self, NativeError> {
        let native = ensure_process(passive, adapter, process)?;
        let key = &*process as *const ProcessContext as usize;
        let handles = ObjectHandles::mint()?;
        let tsg = handles.at(0);
        let ctxshare = handles.at(1);
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
                if io.free(&native.client, nd::H_DEVICE, tsg).is_err() {
                    file_leftover(key, native.epoch, nd::H_DEVICE, tsg);
                }
                return Err(error);
            }
            Ok(())
        })();
        let epoch = native.epoch;
        result?;
        Ok(Self {
            adapter: adapter as *const AdapterContext as *mut AdapterContext,
            process,
            owner,
            epoch,
            tsg,
            ctxshare,
            scheduled: AtomicU32::new(0),
            dxg_context: 0,
            graphics_queues: AtomicU32::new(0),
            veids: AtomicU64::new(0),
            preempt: Preempt::default(),
            hw_tsg: AtomicU32::new(u32::MAX),
        })
    }

    pub(crate) fn destroy(&mut self, passive: PassiveLevel) -> Result<(), NativeError> {
        let Some(adapter) = (unsafe { self.adapter.as_ref() }) else {
            return Ok(());
        };
        let Some(process) = (unsafe { self.process.as_ref() }) else {
            return Ok(());
        };
        let native = match peek_process(passive, process) {
            Ok(native) => native,
            // The transport-wide close already reclaimed the whole client.
            Err(NativeError::Transport) => return Ok(()),
            // No client to free it with: the transport's sweep reclaims it.
            Err(_) => {
                TD_LOST.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        if native.epoch != self.epoch {
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
        // the context share without any live channel beneath it. dxgkrnl never
        // repeats DestroyContext: a TSG RM would not free is filed for
        // DestroyProcess, and the context goes either way.
        let key = self.process_key();
        if io.free(&native.client, nd::H_DEVICE, self.tsg).is_err() {
            file_leftover(key, native.epoch, nd::H_DEVICE, self.tsg);
            // The GR context may still reference its GfxP buffers: they go with the TSG.
            free_preempt_buffers(io, &native.client, &self.preempt, Some((key, native.epoch)), true);
        } else {
            free_preempt_buffers(io, &native.client, &self.preempt, Some((key, native.epoch)), false);
        }
        Ok(())
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
    /// The queue's own context share (`handles.at(12)`, a non-zero VEID), and
    /// whether RM kept it at teardown (its VEID stays taken).
    ctxshare_allocated: bool,
    ctxshare_left: bool,
    channel_allocated: bool,
    token: u32,
    /// What the last `destroy` could not free, for the caller to file.
    left: Left,
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

    /// Every step is tried; the first error is returned. What RM would not free,
    /// or must not free yet, goes to `self.left` (children first): the channel if
    /// its free failed, and then also USERD and the doorbell memory, which a
    /// channel RM still owns can address. CPU views (kernel view, host map, RM's
    /// CPU mapping and its file) never stay: the GPU does not use them.
    fn destroy(&mut self, io: Io<'_>, client: &NativeClient, tsg: u32) -> Result<(), NativeError> {
        let Some(handles) = self.handles else {
            return Ok(());
        };
        let mut result = Ok(());
        let mut channel_gone = true;
        if self.channel_allocated {
            if let Err(error) = io.free(client, tsg, handles.at(5)) {
                channel_gone = false;
                self.left.add(tsg, handles.at(5));
                result = Err(error);
            }
            self.channel_allocated = false;
        }
        if self.ctxshare_allocated {
            self.ctxshare_allocated = false;
            let freed = channel_gone && io.free(client, tsg, handles.at(12)).is_ok();
            if !freed {
                self.ctxshare_left = true;
                self.left.add(tsg, handles.at(12));
                if result.is_ok() {
                    result = Err(NativeError::Rm);
                }
            }
        }
        for view in [&mut self.usermode, &mut self.ctl] {
            // `cpu_unmap` drops the kernel view first, whatever fails after it.
            if let Err(error) = cpu_unmap(io, client, view) {
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        for (allocated, parent, object) in [
            (&mut self.usermode_allocated, nd::H_SUBDEVICE, handles.at(4)),
            (&mut self.ctl_allocated, nd::H_DEVICE, handles.at(3)),
        ] {
            if !*allocated {
                continue;
            }
            *allocated = false;
            if channel_gone {
                match io.free(client, parent, object) {
                    Ok(()) => continue,
                    Err(error) => {
                        if result.is_ok() {
                            result = Err(error);
                        }
                    }
                }
            }
            self.left.add(parent, object);
        }
        result
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
    /// Our own locked, non-paged view of the progress fence (see
    /// `lock_fence_view`) and its MDL; 0 when locking failed.
    fence_view: u64,
    fence_mdl: usize,
    /// Subchannel 4 bound to the copy class (`bind_copy_subchannel`), and how
    /// many submissions were inspected for it.
    copy_bound: core::sync::atomic::AtomicBool,
    copy_tries: AtomicU32,
    _progress_fence_gpu: u64,
    /// Recovered (`mark_dead`): submissions no longer reach the channel.
    dead: core::sync::atomic::AtomicBool,
    /// The compute-only channel's VEID (0: the channel is on the context's
    /// share), and whether the queue counts as the context's graphics queue.
    veid: u32,
    graphics: bool,
    /// The GPFIFO ring's GPU VA, and the process id of the queue's creator (diagnosis:
    /// `ddi::native_snap`).
    gpfifo_va: u64,
    pid: u32,
}

/// What a hang snapshot (`ddi::native_snap`) records of one native queue, read without a host
/// call (`NativeQueue::snap`).
pub(crate) struct QueueSnap {
    pub pid: u32,
    pub token: u32,
    pub veid: u32,
    pub graphics: bool,
    pub dead: bool,
    pub copy_bound: bool,
    pub kmd_gp_put: u32,
    pub userd: [u32; helios_kmd_logic::native_snap::USERD_DWORDS],
    pub notifier_info32: u32,
    pub notifier_word: u32,
    pub ring_entries: u32,
    pub gpfifo_va: u64,
    pub fence_now: u64,
    pub fence_gpu: u64,
    pub channel: u32,
    pub tsg: u32,
    pub ctxshare: u32,
    pub process_key: usize,
    pub context_key: usize,
}

extern "system" {
    /// `PsGetCurrentProcessId()`: any IRQL.
    fn PsGetCurrentProcessId() -> *mut c_void;
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
        let process = unsafe { context.process.as_ref() }.ok_or(NativeError::Invalid)?;
        let handles = ObjectHandles::mint()?;
        let native = peek_process(passive, process)?;
        if native.epoch != context.epoch
            || (backing.va_space != 0 && backing.va_space != native.vaspace)
        {
            crate::diag::record_named_bytes(b"NvQStep", 0x80);
            crate::diag::record_named_bytes(b"NvQSpaces", backing.va_space ^ native.vaspace);
            return Err(NativeError::Transport);
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        // A compute queue next to a graphics queue gets NVIDIA's layout: a
        // compute-only channel on a VEID of its own. On VEID 0 its compute
        // methods interleaved with the graphics channel's 3D stream (Steel
        // Nomad DX12: Xid 13 MME MISSING_MACRO_DATA). NvQVeidOff=1 keeps every
        // queue on VEID 0. NvQVeid* record the outcome.
        let mut veid = 0u32;
        let mut parts = QueueParts::default();
        let mut result = Err(NativeError::Invalid);
        let graphics_queues = context.graphics_queues.load(Ordering::Acquire);
        let veid_off =
            crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvQVeidOff"), 0) != 0;
        if nc::wants_own_veid(queue.kind(), graphics_queues) && !veid_off {
            if let Some(pick) = take_veid(&context.veids) {
                parts = QueueParts {
                    handles: Some(handles),
                    ..QueueParts::default()
                };
                result = create_queue_objects_on(
                    io,
                    &native.client,
                    context,
                    native.vaspace,
                    queue,
                    &mut parts,
                    Some(OwnVeid {
                        veid: pick,
                        vaspace: native.vaspace,
                    }),
                );
                file_leftovers(context.process_key(), native.epoch, &mut parts.left);
                if result.is_ok() {
                    veid = pick;
                    let made = VEID_MADE.fetch_add(1, Ordering::Relaxed) + 1;
                    crate::diag::record_named_bytes(b"NvQVeidMade", made);
                    crate::diag::record_named_bytes(b"NvQVeidLast", pick);
                } else {
                    // Its subcontext stays taken if RM kept the share.
                    if !parts.ctxshare_left {
                        context.veids.fetch_and(!(1u64 << pick), Ordering::AcqRel);
                    }
                    let failed = VEID_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
                    crate::diag::record_named_bytes(b"NvQVeidFail", failed);
                }
            }
        }
        if veid == 0 {
            let handles = if result.is_err() && parts.handles.is_some() {
                ObjectHandles::mint()?
            } else {
                handles
            };
            parts = QueueParts {
                handles: Some(handles),
                ..QueueParts::default()
            };
            result = create_queue_objects(
                io,
                &native.client,
                context,
                native.vaspace,
                queue,
                &mut parts,
            );
            // A half-made queue's objects RM would not free go to DestroyProcess.
            file_leftovers(context.process_key(), native.epoch, &mut parts.left);
        }
        result?;
        let graphics = queue.kind() == nc::QUEUE_KIND_GRAPHICS;
        if graphics {
            context.graphics_queues.fetch_add(1, Ordering::AcqRel);
        }
        // NvCeBindOn: bit 0 enables the binding, bits 8..12 are subchannel 4's
        // SET_OBJECT ENGINE field (its engine on the GR runlist; 0 = GR itself).
        let knob = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvCeBindOn"), 0);
        CE_BIND_ENABLED.store(knob & 1 != 0, Ordering::Relaxed);
        CE_BIND_ENGINE.store((knob >> 8) & 0x1f, Ordering::Relaxed);
        // Last, so no early return can leave the page locked.
        let (fence_view, fence_mdl) = lock_fence_view(progress_cpu);
        crate::diag::record_named_bytes(b"NvQFenceLock", u32::from(fence_view != 0));
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
            fence_view,
            fence_mdl,
            copy_bound: core::sync::atomic::AtomicBool::new(false),
            copy_tries: AtomicU32::new(0),
            _progress_fence_gpu: progress_gpu,
            dead: core::sync::atomic::AtomicBool::new(false),
            veid,
            graphics,
            gpfifo_va: queue.gpfifo_va,
            // SAFETY: a scalar read of the current process id (CreateHwQueue runs in the
            // creating process).
            pid: unsafe { PsGetCurrentProcessId() } as usize as u32,
        })
    }

    /// Read the progress fence's current value through the kernel CPU view
    /// dxgkrnl supplied at CreateHwQueue (`HwQueueProgressFenceCPUVirtualAddress`,
    /// documented as a kernel-mode VA). Any IRQL; one volatile 64-bit load.
    pub(crate) fn read_progress(&self) -> u64 {
        // Only our locked view: the poll runs at DISPATCH, and dxgkrnl's own view
        // was found unmapped there (bugcheck 0xD1 in hws_poll_timer_callback
        // after a D3D11 device hang).
        if self.fence_view == 0 || self.fence_view % 8 != 0 {
            return 0;
        }
        // SAFETY: our MDL keeps the page locked and mapped until Drop.
        unsafe { (self.fence_view as *const u64).read_volatile() }
    }

    /// The channel's error notifier, if RM put the channel in error. Any IRQL: two
    /// volatile loads from the queue's control view.
    pub(crate) fn channel_error(&self) -> Option<helios_kmd_logic::native_hang::ChannelError> {
        use helios_kmd_logic::native_hang as nh;
        let ctl = self.parts.ctl.va;
        if ctl == 0 {
            return None;
        }
        // SAFETY: the queue's live control view (CTL_BYTES, non-paged); the
        // notifier's words are 4-byte aligned inside its first page.
        let (info32, word) = unsafe {
            (
                ((ctl + nh::ERROR_INFO32_AT) as *const u32).read_volatile(),
                ((ctl + nh::ERROR_WORD_AT) as *const u32).read_volatile(),
            )
        };
        nh::channel_error(info32, word)
    }

    /// USERD GP_GET: how far the channel fetched its ring. Any IRQL; 0 unmapped.
    pub(crate) fn gp_get(&self) -> u32 {
        let ctl = self.parts.ctl.va;
        if ctl == 0 {
            return 0;
        }
        // SAFETY: as in `channel_error`; USERD is at USERD_OFFSET of the view.
        unsafe { ((ctl + cc::USERD_OFFSET + u64::from(cp::USERD_GP_GET)) as *const u32).read_volatile() }
    }

    /// Stop every later submission from reaching the channel. Once this returns no
    /// submit is between its check and its doorbell write (both happen under the
    /// GP_PUT lock). Any IRQL.
    pub(crate) fn mark_dead(&self) {
        self.dead.store(true, Ordering::Release);
        drop(self.gp_put.lock());
    }

    /// Write `value` into the progress fence (never moving it backwards), through
    /// the KMD's locked view: the completion of work a dead channel will never do.
    /// `false` when there is no view. Any IRQL.
    pub(crate) fn complete_progress(&self, value: u64) -> bool {
        if self.fence_view == 0 || self.fence_view % 8 != 0 {
            return false;
        }
        // SAFETY: our MDL keeps the page locked and mapped writable until Drop;
        // the GPU writes the same 8 bytes only forward, as this does.
        unsafe {
            (*(self.fence_view as *const core::sync::atomic::AtomicU64))
                .fetch_max(value, Ordering::AcqRel)
        };
        true
    }

    /// The context this queue belongs to (it outlives the queue).
    pub(crate) fn context(&self) -> Option<&NativeContext> {
        // SAFETY: WDDM destroys every queue before its context.
        unsafe { self.context.as_ref() }
    }

    /// `NV906F_CTRL_CMD_RESET_CHANNEL` on this queue's channel, channel only (no
    /// engine reset). Recovery of a faulted or hung context. PASSIVE.
    pub(crate) fn reset_channel(&self, passive: PassiveLevel) -> Result<(), NativeError> {
        use helios_kmd_logic::native_hang as nh;
        let (Some(handles), true) = (self.parts.handles, self.parts.channel_allocated) else {
            return Ok(());
        };
        let context = self.context().ok_or(NativeError::Invalid)?;
        if context.epoch != self.epoch {
            return Err(NativeError::Transport);
        }
        context.with_client(passive, |io, client| {
            let mut params = nh::reset_channel_params();
            io.control(client, handles.at(5), nh::CTRL_RESET_CHANNEL, &mut params)
        })
    }

    /// Whether subchannel 4 still needs its SET_OBJECT (and attempts remain).
    pub(crate) fn wants_copy_binding(&self) -> bool {
        // Off unless NvCeBindOn=1: NVK on RM runs copy methods on subchannel 4
        // of its GR channel with a parameterless copy object and no SET_OBJECT,
        // and every variant with an explicit binding still raised PBDMA DEVICE.
        CE_BIND_ENABLED.load(Ordering::Relaxed)
            && !self.copy_bound.load(Ordering::Relaxed)
            && self.copy_tries.load(Ordering::Relaxed) < 64
    }

    /// The D3D UMDs drive the graphics copy engine on subchannel 4 without ever
    /// binding it (subchannels 0..3 work unbound on the GR channel; 4 raised
    /// PBDMA DEVICE, Xid 32, until bound). Their graphics pushbuffers open with
    /// a run of NOP headers (14..19 zero dwords in the 610.88 captures too):
    /// the first such run in the first 128 bytes of `dma_va` gets
    /// SET_OBJECT(subchannel 4, the copy class), before GP_PUT is written.
    /// NvCeBind: 1 bound, 2 no NOP run, 3 unknown header, 4 not mapped, 5 error.
    pub(crate) fn bind_copy_subchannel(&self, passive: Option<PassiveLevel>, dma_va: u64) {
        self.copy_tries.fetch_add(1, Ordering::Relaxed);
        let (Some(adapter), Some(context)) =
            (unsafe { self.adapter.as_ref() }, unsafe { self.context.as_ref() })
        else {
            return;
        };
        let process = context.process as usize;
        let len = 0x80u64.min(0x1000 - (dma_va & 0xfff)) & !3;
        let copy_class = cp::Gen::Gb202.classes().copy;
        let mut outcome = 2u32;
        let patched = super::native_memory::patch_gpu_bytes(passive, adapter, process, dma_va, len, |p| {
            let words = p as *mut u32;
            let n = (len / 4) as usize;
            let mut pos = 0usize;
            let mut run: Option<usize> = None;
            while pos < n {
                // SAFETY: pos < n dwords inside the mapped range.
                let header = unsafe { words.add(pos).read_volatile() };
                if header == 0 {
                    let start = *run.get_or_insert(pos);
                    if pos + 1 - start >= BINDINGS.len() * 2 {
                        // SET_OBJECT (INC_METHOD, count 1, method 0) for every
                        // subchannel the UMD drives, as NVK binds its channel
                        // (subchannel 4 alone left the D3D11 clear failing).
                        for (i, (subchannel, class)) in BINDINGS.iter().enumerate() {
                            let class = if *subchannel == 4 {
                                copy_class | (CE_BIND_ENGINE.load(Ordering::Relaxed) << 16)
                            } else {
                                *class
                            };
                            unsafe {
                                words
                                    .add(start + 2 * i)
                                    .write_volatile((1 << 29) | (1 << 16) | (subchannel << 13));
                                words.add(start + 2 * i + 1).write_volatile(class);
                            }
                        }
                        outcome = 1;
                        return;
                    }
                    pos += 1;
                    continue;
                }
                run = None;
                let count = ((header >> 16) & 0x1fff) as usize;
                match header >> 29 {
                    1 | 3 | 5 => pos += 1 + count,
                    4 => pos += 1,
                    _ => {
                        outcome = 3;
                        return;
                    }
                }
            }
        });
        match patched {
            Ok(true) => {}
            Ok(false) => outcome = 4,
            Err(_) => outcome = 5,
        }
        if outcome == 1 {
            core::sync::atomic::fence(Ordering::SeqCst);
            self.copy_bound.store(true, Ordering::Relaxed);
        }
        // Recorded at teardown (PASSIVE): this may run at DISPATCH.
        CE_BIND_OUTCOME.store(outcome, Ordering::Relaxed);
        CE_BIND_SEEN.fetch_or(1 << outcome, Ordering::Relaxed);
        if outcome == 1 {
            CE_BIND_DONE.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn token(&self) -> Token {
        Token(self.parts.token)
    }

    /// The KMD's GP_PUT (the GP_PUT lock). Any IRQL up to DISPATCH.
    pub(crate) fn kmd_gp_put(&self) -> u32 {
        *self.gp_put.lock()
    }

    /// The queue's state for a hang snapshot: USERD, the error notifier, the progress fence,
    /// handles. No host call; the GP_PUT lock for the KMD's GP_PUT. Any IRQL up to DISPATCH.
    pub(crate) fn snap(&self) -> QueueSnap {
        use helios_kmd_logic::native_snap::USERD_DWORDS;
        let ctl = self.parts.ctl.va;
        let mut userd = [0u32; USERD_DWORDS];
        let (mut info32, mut word) = (0, 0);
        if ctl != 0 {
            for (i, v) in userd.iter_mut().enumerate() {
                // SAFETY: the queue's live control view (CTL_BYTES, non-paged); USERD's
                // first 256 bytes lie inside it at USERD_OFFSET.
                *v = unsafe { ((ctl + cc::USERD_OFFSET + 4 * i as u64) as *const u32).read_volatile() };
            }
            // SAFETY: as in `channel_error`.
            unsafe {
                info32 = ((ctl + helios_kmd_logic::native_hang::ERROR_INFO32_AT) as *const u32).read_volatile();
                word = ((ctl + helios_kmd_logic::native_hang::ERROR_WORD_AT) as *const u32).read_volatile();
            }
        }
        let context = self.context();
        QueueSnap {
            pid: self.pid,
            token: self.parts.token,
            veid: self.veid,
            graphics: self.graphics,
            dead: self.dead.load(Ordering::Acquire),
            copy_bound: self.copy_bound.load(Ordering::Relaxed),
            kmd_gp_put: *self.gp_put.lock(),
            userd,
            notifier_info32: info32,
            notifier_word: word,
            ring_entries: self.ring_bytes / 8,
            gpfifo_va: self.gpfifo_va,
            fence_now: self.read_progress(),
            fence_gpu: self._progress_fence_gpu,
            channel: match (self.parts.handles, self.parts.channel_allocated) {
                (Some(handles), true) => handles.at(5),
                _ => 0,
            },
            tsg: context.map_or(0, |c| c.tsg),
            ctxshare: context.map_or(0, |c| c.ctxshare),
            process_key: context.map_or(0, |c| c.process_key()),
            context_key: context.map_or(0, |c| c as *const NativeContext as usize),
        }
    }

    /// Read `out.len()` GPFIFO entries from ring index `first` on (wrapping) through the
    /// KMD's CPU view of the ring's allocation. `(lo, hi)` per entry; a native-snapshot read
    /// status (`helios_kmd_logic::native_snap::RD_*`). PASSIVE.
    pub(crate) fn snap_ring(
        &self,
        passive: PassiveLevel,
        adapter: &AdapterContext,
        first: u32,
        out: &mut [(u32, u32)],
    ) -> u32 {
        use helios_kmd_logic::native_snap as ns;
        let entries = self.ring_bytes / 8;
        if entries == 0 || self.ring_id == 0 {
            return ns::RD_NOT_MAPPED;
        }
        let mut done = 0usize;
        while done < out.len() {
            let index = (first + done as u32) % entries;
            let run = (out.len() - done).min((entries - index) as usize);
            let mut words = [0u32; 2 * ns::WINDOW];
            let n = run.min(ns::WINDOW);
            // SAFETY: PASSIVE; the closure copies at most `n` entries out.
            let read = unsafe {
                super::native_memory::with_cpu_range(
                    passive,
                    adapter,
                    self.ring_id,
                    self.ring_offset + u64::from(index) * 8,
                    n as u64 * 8,
                    |relative, native, len| {
                        let at = relative as usize / 4;
                        let k = (len as usize / 4).min((2 * n).saturating_sub(at));
                        for i in 0..k {
                            words[at + i] = (native as *const u32).add(i).read_volatile();
                        }
                        true
                    },
                )
            };
            match read {
                Ok(true) => {}
                Ok(false) => return ns::RD_NOT_MAPPED,
                Err(_) => return ns::RD_ERROR,
            }
            for i in 0..n {
                out[done + i] = (words[2 * i], words[2 * i + 1]);
            }
            done += n;
        }
        ns::RD_OK
    }

    /// DISPATCH-safe: one spinlock and three volatile stores/barriers.  No RM
    /// message, allocation, completion write, or host round trip is possible.
    pub(crate) fn submit(&self, private: &[u8]) -> Result<(), NativeError> {
        let adapter = unsafe { self.adapter.as_ref() }.ok_or(NativeError::Transport)?;
        // The cached epoch: the same value as `with_virtio(..nvrm_epoch)`, without the
        // transport spinlock on every submission.
        let epoch = adapter.nvrm_epoch_fast();
        if epoch == 0 || epoch != self.epoch {
            return Err(NativeError::Transport);
        }
        let mut previous = self.gp_put.lock();
        if self.dead.load(Ordering::Acquire) {
            return Err(NativeError::Dead);
        }
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
            crate::diag::record_named_bytes(b"NvCeBind", CE_BIND_OUTCOME.load(Ordering::Relaxed));
            crate::diag::record_named_bytes(b"NvCeSeen", CE_BIND_SEEN.load(Ordering::Relaxed));
            crate::diag::record_named_bytes(b"NvCeDone", CE_BIND_DONE.load(Ordering::Relaxed));
            crate::diag::record_named_bytes(
                b"NvCeIrql",
                CE_BIND_IRQL.load(Ordering::Relaxed),
            );
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
        let Some(process) = (unsafe { context.process.as_ref() }) else {
            return Ok(());
        };
        let native = match peek_process(passive, process) {
            Ok(native) => native,
            Err(NativeError::Transport) => {
                self.parts.drop_views();
                return Ok(());
            }
            // No client to free its objects with: the transport's sweep reclaims
            // them; the kernel views go now.
            Err(_) => {
                self.parts.drop_views();
                TD_LOST.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        if native.epoch != self.epoch {
            self.parts.drop_views();
            return Ok(());
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        // dxgkrnl never repeats DestroyHwQueue: what RM would not free is filed for
        // DestroyProcess and the queue goes either way (its fence page unlocks in
        // `Drop`).
        let _ = self.parts.destroy(io, &native.client, context.tsg);
        file_leftovers(context.process_key(), native.epoch, &mut self.parts.left);
        self.release_context_slots(context);
        Ok(())
    }

    /// Give the context back this queue's graphics count and VEID (a VEID
    /// whose share RM kept stays taken). Once, at teardown.
    fn release_context_slots(&mut self, context: &NativeContext) {
        if core::mem::take(&mut self.graphics) {
            context.graphics_queues.fetch_sub(1, Ordering::AcqRel);
        }
        let veid = core::mem::take(&mut self.veid);
        if veid != 0 && !self.parts.ctxshare_left {
            context.veids.fetch_and(!(1u64 << veid), Ordering::AcqRel);
        }
    }
}

static VEID_MADE: AtomicU32 = AtomicU32::new(0);
static VEID_FAILED: AtomicU32 = AtomicU32::new(0);

/// Take the lowest free non-zero VEID of a TSG's `veids` mask.
fn take_veid(veids: &AtomicU64) -> Option<u32> {
    let mut used = veids.load(Ordering::Acquire);
    loop {
        let pick = nc::pick_veid(used)?;
        match veids.compare_exchange_weak(
            used,
            used | (1u64 << pick),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(pick),
            Err(now) => used = now,
        }
    }
}

/// The Vulkan ICD's node-3 copy context's own channel (`ddi::nv_copy`). The
/// context submits with `SubmitCommand` and the work in its private data as
/// GPFIFO entries (`helios_kmd_logic::nv_copy`); NVIDIA's KMD runs them on a
/// copy channel of its own, and so does this one: a native TSG and GR-runlist
/// channel in the process's VA space, built like a native queue's
/// (`create_queue_objects`: the graphics copy engine on subchannel 4, the
/// deferred-API object), over a KMD-owned system-memory block (`nv::ring`)
/// that holds the GPFIFO ring, one release segment per submission and the
/// progress semaphore the KMD's release segment writes.
pub(crate) struct CopyChannel {
    context: NativeContext,
    parts: QueueParts,
    /// The block's RM objects: memory at(0), its virtual allocation at(1).
    block_handles: ObjectHandles,
    block_allocated: bool,
    block_virtual: bool,
    block: CpuView,
    block_gpu: u64,
    gp_put: SpinLock<u32>,
    dead: core::sync::atomic::AtomicBool,
}

impl CopyChannel {
    /// PASSIVE (CreateContext of the copy context).
    pub(crate) fn create(
        passive: PassiveLevel,
        adapter: &AdapterContext,
        process: &mut ProcessContext,
        owner: DeviceOwner,
    ) -> Result<Self, NativeError> {
        let block_handles = ObjectHandles::mint()?;
        let context = NativeContext::create(passive, adapter, process, owner)?;
        let mut channel = Self {
            context,
            parts: QueueParts::default(),
            block_handles,
            block_allocated: false,
            block_virtual: false,
            block: CpuView::default(),
            block_gpu: 0,
            gp_put: SpinLock::new(0),
            dead: core::sync::atomic::AtomicBool::new(false),
        };
        // NvCcxErr: 0 built, else 0x100 | the error of the step that failed.
        if let Err(error) = channel.build(passive, adapter) {
            crate::diag::record_named_bytes(b"NvCcxErr", 0x100 | error as u32);
            let _ = channel.destroy(passive);
            return Err(error);
        }
        crate::diag::record_named_bytes(b"NvCcxErr", 0);
        crate::diag::record_named_bytes(b"NvCcxVa", (channel.block_gpu >> 12) as u32);
        crate::diag::record_named_bytes(b"NvCcxTok", channel.parts.token);
        Ok(channel)
    }

    fn build(&mut self, passive: PassiveLevel, adapter: &AdapterContext) -> Result<(), NativeError> {
        use helios_kmd_logic::nv_copy::ring;
        let process = unsafe { self.context.process.as_ref() }.ok_or(NativeError::Invalid)?;
        let native = peek_process(passive, process)?;
        if native.epoch != self.context.epoch {
            return Err(NativeError::Transport);
        }
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        let client = &native.client;
        let memory = self.block_handles.at(0);
        let virt = self.block_handles.at(1);
        io.alloc(
            client,
            nd::H_DEVICE,
            memory,
            cc::NV01_MEMORY_SYSTEM,
            &helios_kmd_logic::rm_sysmem::params(
                client.root,
                helios_kmd_logic::rm_sysmem::Cache::Cached,
                ring::BYTES,
            ),
        )?;
        self.block_allocated = true;
        self.block = cpu_map(
            io,
            client,
            nd::H_DEVICE,
            memory,
            cc::MapNode::Ctl,
            ring::BYTES,
            _MEMORY_CACHING_TYPE::MmCached,
        )?;
        zero(self.block.va, ring::BYTES);
        // A 4-KiB-paged virtual allocation RM places itself (outside the VA
        // windows the KMD keeps for VidMm's addresses) and the block mapped
        // into it with 4-KiB system-memory PTEs.
        io.alloc(
            client,
            nd::H_DEVICE,
            virt,
            cc::NV50_MEMORY_VIRTUAL,
            &cc::virtual_params_4k(client.root, native.vaspace, None, ring::BYTES),
        )?;
        self.block_virtual = true;
        let block = cc::nvos46(&cc::DmaMap {
            root: client.root,
            h_device: nd::H_DEVICE,
            h_dma: virt,
            h_memory: memory,
            length: ring::BYTES,
            flags: cc::MAP_FLAGS_SYSMEM,
        });
        let mut resp = [0u8; 256];
        let n = io.exchange(
            client.ctl,
            rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )?;
        let reply = rc::rm_reply(resp.get(..n).ok_or(NativeError::Transport)?, cc::NVOS46_STATUS_AT)
            .map_err(|error| {
                crate::diag::record_named_bytes(b"NvCcxMapSt", rm_error_code(error));
                NativeError::Rm
            })?;
        self.block_gpu = cc::map_dma_va(reply.data).ok_or(NativeError::Transport)?;
        if self.block_gpu == 0 || self.block_gpu % 0x1000 != 0 {
            return Err(NativeError::Invalid);
        }
        self.parts.handles = Some(ObjectHandles::mint()?);
        let queue = nc::QueuePrivate {
            word_10: 0,
            word_1c: 0,
            gpfifo_va: self.block_gpu,
            ring_bytes: ring::GPFIFO_BYTES,
            allocation: 0,
        };
        let result = create_queue_objects(
            io,
            client,
            &self.context,
            native.vaspace,
            &queue,
            &mut self.parts,
        );
        file_leftovers(self.context.process_key(), native.epoch, &mut self.parts.left);
        result
    }

    /// The process key native memory files the process's mappings under.
    pub(crate) fn process_key(&self) -> usize {
        self.context.process_key()
    }

    /// The last sequence the channel finished (its progress semaphore). Any IRQL.
    pub(crate) fn progress(&self) -> u64 {
        use helios_kmd_logic::nv_copy::ring;
        if self.block.va == 0 {
            return 0;
        }
        // SAFETY: the live, non-paged kernel view of the block; 8-aligned.
        unsafe { ((self.block.va + ring::SEMAPHORE_AT) as *const u64).read_volatile() }
    }

    /// USERD GP_GET. Any IRQL; 0 unmapped.
    pub(crate) fn gp_get(&self) -> u32 {
        let ctl = self.parts.ctl.va;
        if ctl == 0 {
            return 0;
        }
        // SAFETY: as `NativeQueue::gp_get`.
        unsafe { ((ctl + cc::USERD_OFFSET + u64::from(cp::USERD_GP_GET)) as *const u32).read_volatile() }
    }

    /// The channel's error notifier. Any IRQL.
    pub(crate) fn channel_error(&self) -> Option<helios_kmd_logic::native_hang::ChannelError> {
        use helios_kmd_logic::native_hang as nh;
        let ctl = self.parts.ctl.va;
        if ctl == 0 {
            return None;
        }
        // SAFETY: as `NativeQueue::channel_error`.
        let (info32, word) = unsafe {
            (
                ((ctl + nh::ERROR_INFO32_AT) as *const u32).read_volatile(),
                ((ctl + nh::ERROR_WORD_AT) as *const u32).read_volatile(),
            )
        };
        nh::channel_error(info32, word)
    }

    /// Recovered: nothing reaches the channel any more. Any IRQL.
    pub(crate) fn mark_dead(&self) {
        self.dead.store(true, Ordering::Release);
        drop(self.gp_put.lock());
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Append `entries` and the KMD's release of `seq` to the ring and ring the
    /// doorbell. DISPATCH-safe: one spinlock, volatile stores into the KMD's
    /// own views. `Busy` when the ring has no room (the GPU is behind).
    pub(crate) fn submit(&self, entries: &[helios_kmd_logic::nv_copy::GpEntry], seq: u64) -> Result<(), NativeError> {
        use helios_kmd_logic::nv_copy::{self as nv, ring};
        let adapter = unsafe { self.context.adapter.as_ref() }.ok_or(NativeError::Transport)?;
        let epoch = adapter.nvrm_epoch_fast();
        if epoch == 0 || epoch != self.context.epoch {
            return Err(NativeError::Transport);
        }
        let ctl = self.parts.ctl.va;
        let doorbell = self.parts.usermode.va;
        if ctl == 0 || doorbell == 0 || self.parts.token == 0 || self.block.va == 0 {
            return Err(NativeError::Transport);
        }
        let mut put = self.gp_put.lock();
        if self.dead.load(Ordering::Acquire) {
            return Err(NativeError::Dead);
        }
        let need = entries.len() as u32 + 1;
        if need > ring::MAX_SUBMIT_ENTRIES || ring::room(*put, self.gp_get()) < need {
            return Err(NativeError::Busy);
        }
        // The release segment for this sequence.
        let release_off = ring::release_offset(seq);
        let release = nv::release_segment(self.block_gpu + ring::SEMAPHORE_AT, seq);
        for (i, word) in release.iter().enumerate() {
            // SAFETY: inside the block's kernel view (`release_offset` + 28 bytes).
            unsafe { write32(self.block.va + release_off + 4 * i as u64, *word) };
        }
        let mut position = *put;
        let release_entry = nv::GpEntry {
            va: self.block_gpu + release_off,
            dwords: nv::RELEASE_DWORDS as u32,
        };
        for entry in entries.iter().chain(core::iter::once(&release_entry)) {
            let (lo, hi) = entry.encode();
            let at = self.block.va + u64::from(position) * 8;
            // SAFETY: `position` < ENTRIES, inside the ring at the block's start.
            unsafe {
                write32(at, lo);
                write32(at + 4, hi);
            }
            position = (position + 1) % ring::ENTRIES;
        }
        full_barrier();
        // SAFETY: as `NativeQueue::submit`: USERD GP_PUT, then the doorbell.
        unsafe { write32(ctl + cc::USERD_OFFSET + u64::from(cp::USERD_GP_PUT), position) };
        full_barrier();
        unsafe { write32(doorbell + u64::from(cp::DOORBELL_OFFSET), self.parts.token) };
        *put = position;
        Ok(())
    }

    /// PASSIVE (DestroyContext): the channel, the block and the TSG. What RM
    /// would not free is filed for DestroyProcess.
    pub(crate) fn destroy(&mut self, passive: PassiveLevel) -> Result<(), NativeError> {
        let (Some(adapter), Some(process)) =
            (unsafe { self.context.adapter.as_ref() }, unsafe { self.context.process.as_ref() })
        else {
            return Ok(());
        };
        let native = match peek_process(passive, process) {
            Ok(native) if native.epoch == self.context.epoch => native,
            _ => {
                self.parts.drop_views();
                if self.block.va != 0 {
                    // SAFETY: this exact MmMapIoSpace view, removed once.
                    unsafe { MmUnmapIoSpace(self.block.va as *mut c_void, self.block.len) };
                    self.block.va = 0;
                }
                return Ok(());
            }
        };
        let io = Io {
            passive,
            adapter,
            epoch: native.epoch,
        };
        let client = &native.client;
        let _ = self.parts.destroy(io, client, self.context.tsg);
        file_leftovers(self.context.process_key(), native.epoch, &mut self.parts.left);
        let _ = cpu_unmap(io, client, &mut self.block);
        if self.block_virtual {
            self.block_virtual = false;
            if io.free(client, nd::H_DEVICE, self.block_handles.at(1)).is_err() {
                file_leftover(self.context.process_key(), native.epoch, nd::H_DEVICE, self.block_handles.at(1));
            }
        }
        if self.block_allocated {
            self.block_allocated = false;
            if io.free(client, nd::H_DEVICE, self.block_handles.at(0)).is_err() {
                file_leftover(self.context.process_key(), native.epoch, nd::H_DEVICE, self.block_handles.at(0));
            }
        }
        self.context.destroy(passive)
    }
}

#[allow(clippy::too_many_arguments)]
/// `NvChT<k>` / `NvChV<k>` / `NvChS<k>` (a 16-entry ring): each native channel's
/// work-submit token (low bits = the host channel id an Xid names), its GPFIFO
/// VA page and its context share handle, so host faults can be tied to a guest queue.
fn note_channel(token: u32, gpfifo_va: u64, space: u32) {
    static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let k = (n % 16) as u8;
    let digit = if k < 10 { b'0' + k } else { b'a' + k - 10 };
    crate::diag::record_named_bytes(&[b'N', b'v', b'C', b'h', b'T', digit], token);
    crate::diag::record_named_bytes(&[b'N', b'v', b'C', b'h', b'V', digit], (gpfifo_va >> 12) as u32);
    crate::diag::record_named_bytes(&[b'N', b'v', b'C', b'h', b'S', digit], space);
}

fn create_queue_objects(
    io: Io<'_>,
    client: &NativeClient,
    context: &NativeContext,
    vaspace: u32,
    queue: &nc::QueuePrivate,
    parts: &mut QueueParts,
) -> Result<(), NativeError> {
    create_queue_objects_on(io, client, context, vaspace, queue, parts, None)
}

/// A compute-only channel's own subcontext: `veid` (non-zero) in the VA space
/// `vaspace` of the TSG.
#[derive(Clone, Copy)]
struct OwnVeid {
    veid: u32,
    vaspace: u32,
}

/// `NvPreempt` (`helios_kmd_logic::ctxsw_preempt`): set the context-switch preemption modes
/// of a new channel's TSG after its engine objects, as NVIDIA's drivers do, and read the
/// modes in force back. The modes are the TSG's (one GR context): only the TSG's first VEID 0
/// channel sets them, both in one call, and a refusal leaves both at RM's defaults; later
/// channels of the TSG (compute-only ones on other VEIDs included) set nothing. GFXP binds
/// per-TSG buffers the KMD allocates (`CTXSW_PREEMPTION_BIND`), WFI / GFXP_POOL / a compute
/// mode alone go through `SET_CTXSW_PREEMPTION_MODE`.
///
/// Best effort, never fails the queue. Registry values: `NvPrSetSt` = 0 set on the channel,
/// 1 on the TSG (the channel refused), 2 both refused (SET); `NvPrBind` = 0 bound on the TSG,
/// 1 on the channel, 2 both refused, 3 no buffers (BIND); `NvPrSt` = RM's status of the last
/// refused set or bind; `NvPrBufErr` why the buffers were not made (1 no MAIN size, 2
/// alignment, 3 too large, 0x1i size query i refused, 0x2i / 0x3i / 0x4i memory / virtual /
/// map of buffer i refused, 0x5i a mapped VA not aligned); `NvPrSz<i>` RM's size of buffer i
/// in 4-KiB units; `NvPrVa` the MAIN buffer's VA >> 16; `NvPrBufN` TSGs with buffers bound;
/// `NvPrN` = TSGs set; `NvPrSkip` = channels that left the modes to their TSG's first
/// channel; `NvPrWant` = `gfxp << 8 | cilp` asked, `NvPrGfx` / `NvPrCmp` the same read back on
/// the last graphics / compute-only channel (0xffffffff = the read was refused);
/// `NvPrTsg<k>` the TSG's hardware id with the channel's token.
#[allow(clippy::too_many_arguments)]
fn set_preemption(
    io: Io<'_>,
    client: &NativeClient,
    context: &NativeContext,
    channel: u32,
    vaspace: u32,
    graphics: bool,
    token: u32,
) {
    use helios_kmd_logic::ctxsw_preempt as pre;
    static SET: AtomicU32 = AtomicU32::new(0);
    static SKIPPED: AtomicU32 = AtomicU32::new(0);
    let tsg = context.tsg;
    let knob = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPreempt"), 0);
    let plan = pre::plan(knob, graphics);
    let gate = &context.preempt.gate;
    if pre::claims(&plan, graphics, gate.load(Ordering::Acquire))
        && gate
            .compare_exchange(
                pre::gate::UNTRIED,
                pre::gate::BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    {
        crate::diag::record_named_bytes(b"NvPrWant", pre::expected_word(&plan));
        let accepted = match plan.route() {
            pre::Route::Bind => bind_gfxp(io, client, context, channel, vaspace, &plan),
            pre::Route::Set => set_modes(io, client, tsg, channel, &plan),
            pre::Route::Nothing => false,
        };
        gate.store(pre::gate_after(accepted), Ordering::Release);
        if accepted {
            crate::diag::record_named_bytes(b"NvPrN", SET.fetch_add(1, Ordering::Relaxed) + 1);
        }
    } else if pre::plan(knob, true).sets() {
        crate::diag::record_named_bytes(b"NvPrSkip", SKIPPED.fetch_add(1, Ordering::Relaxed) + 1);
    }
    if plan.query {
        let mut get = pre::get_params(channel);
        let word = match io.control(client, nd::H_SUBDEVICE, pre::CTRL_GET_CTXSW_MODES, &mut get) {
            Ok(()) => pre::modes_word(&get),
            Err(_) => u32::MAX,
        };
        crate::diag::record_named_bytes(if graphics { b"NvPrGfx" } else { b"NvPrCmp" }, word);
        // The hardware TSG id next to the channel's work-submit token (low 16 bits), the
        // numbers a host `Xid 109 ... Info` (PREV/NEXT_TSGID) and `channel 0x..` name:
        // `NvPrTsg<k>` = tsgID << 16 | token & 0xffff, a 16-entry ring (`NvPrTsgN`).
        let mut info = [0u8; 4];
        let id = match io.control(client, tsg, pre::CTRL_TSG_GET_INFO, &mut info) {
            Ok(()) => u32::from_le_bytes(info),
            Err(_) => 0xffff,
        };
        static TSGS: AtomicU32 = AtomicU32::new(0);
        let n = TSGS.fetch_add(1, Ordering::Relaxed);
        let k = (n % 16) as u8;
        let digit = if k < 10 { b'0' + k } else { b'a' + k - 10 };
        crate::diag::record_named_bytes(
            &[b'N', b'v', b'P', b'r', b'T', b's', b'g', digit],
            (id & 0xffff) << 16 | (token & 0xffff),
        );
        crate::diag::record_named_bytes(b"NvPrTsgN", n + 1);
    }
}

/// A control whose refusal's RM status the caller wants (`Err` = `rm_error_code`, or
/// 0xfffffffe for a transport failure).
fn control_status(
    io: Io<'_>,
    client: &NativeClient,
    object: u32,
    command: u32,
    params: &mut [u8],
) -> Result<(), u32> {
    let block = rc::nvos54(client.root, object, command, params.len() as u32);
    let mut resp = [0u8; 1024];
    let n = io
        .exchange(
            client.ctl,
            rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
            &block,
            params,
            &mut resp,
        )
        .map_err(|_| 0xffff_fffeu32)?;
    let reply = rc::rm_reply(resp.get(..n).ok_or(0xffff_fffeu32)?, rc::NVOS54_STATUS_AT)
        .map_err(rm_error_code)?;
    let count = reply.nested.len().min(params.len());
    params[..count].copy_from_slice(&reply.nested[..count]);
    if count != params.len() {
        return Err(0xffff_fffe);
    }
    Ok(())
}

/// `SET_CTXSW_PREEMPTION_MODE` with both of `plan`'s modes in one call, on the channel, else
/// on the TSG. Whether RM took it.
fn set_modes(
    io: Io<'_>,
    client: &NativeClient,
    tsg: u32,
    channel: u32,
    plan: &helios_kmd_logic::ctxsw_preempt::Plan,
) -> bool {
    use helios_kmd_logic::ctxsw_preempt as pre;
    let Some(params) = pre::set_params(channel, plan) else {
        return false;
    };
    let set = pre::CTRL_SET_PREEMPTION_MODE;
    let mut on_channel = params;
    let status = match control_status(io, client, nd::H_SUBDEVICE, set, &mut on_channel) {
        Ok(()) => 0,
        Err(st) => {
            crate::diag::record_named_bytes(b"NvPrSt", st);
            let mut on_tsg = params;
            on_tsg[4..8].copy_from_slice(&tsg.to_le_bytes());
            match control_status(io, client, nd::H_SUBDEVICE, set, &mut on_tsg) {
                Ok(()) => 1,
                Err(st) => {
                    crate::diag::record_named_bytes(b"NvPrSt", st);
                    2
                }
            }
        }
    };
    crate::diag::record_named_bytes(b"NvPrSetSt", status);
    status != 2
}

/// GFXP for the TSG: RM's buffer sizes, the five buffers in video memory mapped into the
/// TSG's VA space `vaspace`, and `CTXSW_PREEMPTION_BIND` (with the plan's compute mode in the
/// same call) on the TSG, else on the channel. Buffers RM did not take are freed again.
/// Whether RM took it.
fn bind_gfxp(
    io: Io<'_>,
    client: &NativeClient,
    context: &NativeContext,
    channel: u32,
    vaspace: u32,
    plan: &helios_kmd_logic::ctxsw_preempt::Plan,
) -> bool {
    use helios_kmd_logic::ctxsw_preempt as pre;
    static BOUND: AtomicU32 = AtomicU32::new(0);
    let state = &context.preempt;
    let mut bufs = [pre::Buffer::default(); pre::BIND_BUFFERS];
    for (i, buf) in bufs.iter_mut().enumerate() {
        let mut props = pre::props_params(i);
        let props_cmd = pre::CTRL_ENGINE_CONTEXT_PROPERTIES;
        if let Err(st) = control_status(io, client, nd::H_DEVICE, props_cmd, &mut props) {
            crate::diag::record_named_bytes(b"NvPrSt", st);
            crate::diag::record_named_bytes(b"NvPrBufErr", 0x10 | i as u32);
            crate::diag::record_named_bytes(b"NvPrBind", 3);
            return false;
        }
        *buf = pre::parse_props(&props);
        crate::diag::record_named_bytes(
            &[b'N', b'v', b'P', b'r', b'S', b'z', b'0' + i as u8],
            (buf.size >> 12) as u32,
        );
    }
    if let Err(error) = pre::check_layout(&bufs) {
        crate::diag::record_named_bytes(b"NvPrBufErr", error.code());
        crate::diag::record_named_bytes(b"NvPrBind", 3);
        return false;
    }
    let Ok(handles) = ObjectHandles::mint() else {
        crate::diag::record_named_bytes(b"NvPrBind", 3);
        return false;
    };
    state.first.store(handles.at(0), Ordering::Release);
    let mut vas = [0u64; pre::BIND_BUFFERS];
    let made = (|| -> Result<(), u32> {
        for (i, buf) in bufs.iter().enumerate() {
            if buf.size == 0 {
                continue;
            }
            let memory = handles.at(i as u32);
            let virt = handles.at((pre::BIND_BUFFERS + i) as u32);
            let params = pre::vidmem_params(client.root, buf);
            io.alloc(client, nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &params)
                .map_err(|_| 0x20 | i as u32)?;
            state.live.fetch_or(1 << i, Ordering::AcqRel);
            let params = pre::virtual_params(client.root, vaspace, buf);
            io.alloc(client, nd::H_DEVICE, virt, cc::NV50_MEMORY_VIRTUAL, &params)
                .map_err(|_| 0x30 | i as u32)?;
            state.live.fetch_or(1 << (pre::BIND_BUFFERS + i), Ordering::AcqRel);
            let va = map_dma(io, client, virt, memory, buf.size).map_err(|_| 0x40 | i as u32)?;
            if !pre::va_ok(va, buf) {
                return Err(0x50 | i as u32);
            }
            vas[i] = va;
        }
        Ok(())
    })();
    if let Err(code) = made {
        crate::diag::record_named_bytes(b"NvPrBufErr", code);
        crate::diag::record_named_bytes(b"NvPrBind", 3);
        free_preempt_buffers(io, client, state, None, false);
        return false;
    }
    crate::diag::record_named_bytes(b"NvPrVa", (vas[0] >> 16) as u32);
    // The TSG first: the vGPU guest RM names the channel group when the channel has one
    // (`_rpcAllocBuffersForKGrObj`).
    let bound = [context.tsg, channel]
        .iter()
        .enumerate()
        .find_map(|(k, &target)| {
            let mut bind = pre::bind_params(client.root, target, plan, &vas)?;
            let cmd = pre::CTRL_PREEMPTION_BIND;
            match control_status(io, client, nd::H_SUBDEVICE, cmd, &mut bind) {
                Ok(()) => Some(k as u32),
                Err(st) => {
                    crate::diag::record_named_bytes(b"NvPrSt", st);
                    None
                }
            }
        });
    match bound {
        Some(k) => {
            crate::diag::record_named_bytes(b"NvPrBind", k);
            crate::diag::record_named_bytes(b"NvPrBufN", BOUND.fetch_add(1, Ordering::Relaxed) + 1);
            true
        }
        None => {
            crate::diag::record_named_bytes(b"NvPrBind", 2);
            free_preempt_buffers(io, client, state, None, false);
            false
        }
    }
}

/// `MAP_MEMORY_DMA` of `memory` into the virtual allocation `virt`; the GPU VA.
fn map_dma(
    io: Io<'_>,
    client: &NativeClient,
    virt: u32,
    memory: u32,
    length: u64,
) -> Result<u64, NativeError> {
    let block = cc::nvos46(&cc::DmaMap {
        root: client.root,
        h_device: nd::H_DEVICE,
        h_dma: virt,
        h_memory: memory,
        length,
        flags: 0,
    });
    let mut resp = [0u8; 256];
    let n = io.exchange(
        client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut resp,
    )?;
    let reply = rc::rm_reply(resp.get(..n).ok_or(NativeError::Transport)?, cc::NVOS46_STATUS_AT)
        .map_err(|error| {
            crate::diag::record_named_bytes(b"NvPrSt", rm_error_code(error));
            NativeError::Rm
        })?;
    cc::map_dma_va(reply.data).ok_or(NativeError::Transport)
}

/// Free a TSG's GfxP buffers: each virtual allocation (which unmaps it), then its memory.
/// `file` = (process key, epoch) to file what RM would not free for DestroyProcess; with
/// `keep` everything is filed without being freed (its TSG could not be freed and may still
/// reference them).
fn free_preempt_buffers(
    io: Io<'_>,
    client: &NativeClient,
    state: &Preempt,
    file: Option<(usize, u64)>,
    keep: bool,
) {
    use helios_kmd_logic::ctxsw_preempt as pre;
    let first = state.first.swap(0, Ordering::AcqRel);
    let live = state.live.swap(0, Ordering::AcqRel);
    if first == 0 {
        return;
    }
    let order = (0..pre::BIND_BUFFERS).flat_map(|i| [pre::BIND_BUFFERS + i, i]);
    for bit in order {
        if live & (1 << bit) == 0 {
            continue;
        }
        let object = first + bit as u32;
        if keep || io.free(client, nd::H_DEVICE, object).is_err() {
            if let Some((key, epoch)) = file {
                file_leftover(key, epoch, nd::H_DEVICE, object);
            }
        }
    }
}

/// `own` None: the graphics layout, a channel on the context's share (VEID 0)
/// with every object the UMDs drive. `own` Some: NVIDIA's compute-only queue,
/// a channel on runqueue 1 under a SPECIFIED context share of its own, with
/// the compute and deferred-API objects only.
#[allow(clippy::too_many_arguments)]
fn create_queue_objects_on(
    io: Io<'_>,
    client: &NativeClient,
    context: &NativeContext,
    vaspace: u32,
    queue: &nc::QueuePrivate,
    parts: &mut QueueParts,
    own: Option<OwnVeid>,
) -> Result<(), NativeError> {
    let tsg = context.tsg;
    let ctxshare = context.ctxshare;
    let scheduled = &context.scheduled;
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

        let (channel_share, channel_flags) = match own {
            Some(own) => {
                crate::diag::record_named_bytes(b"NvQStep", 0x41);
                io.alloc(
                    client,
                    tsg,
                    handles.at(12),
                    cc::FERMI_CONTEXT_SHARE_A,
                    &cc::ctxshare_specified_params(own.vaspace, own.veid),
                )?;
                parts.ctxshare_allocated = true;
                (handles.at(12), cc::CHANNEL_FLAGS_ASYNC_COMPUTE)
            }
            None => (ctxshare, 0),
        };
        crate::diag::record_named_bytes(b"NvQStep", 5);
        let channel = cc::channel_params(&cc::ChannelDesc {
            h_ctl: handles.at(3),
            gpfifo_va: queue.gpfifo_va,
            entries: queue.entries(),
            h_ctxshare: channel_share,
            userd_offset: cc::USERD_OFFSET,
            engine_type: cc::ENGINE_GRAPHICS,
            flags: channel_flags,
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
        if own.is_some() {
            // The compute queues bind NV50_DEFERRED_API_CLASS on subchannel 5
            // like every other queue; 3D (VEID 0 only) and copy (runqueue 0
            // only) are refused on this channel and the queue drives neither.
            let deferred = io.alloc(
                client,
                handles.at(5),
                handles.at(9),
                NV50_DEFERRED_API_CLASS,
                &[0u8; 1],
            );
            crate::diag::record_named_bytes(b"NvQVeidDapi", u32::from(deferred.is_err()));
        } else {
            // The D3D UMDs drive the 3D class on subchannel 0 and the graphics copy
            // engine on subchannel 4 (they never SET_OBJECT them; their pushbuffers
            // assume both). RM maps a class's GR context buffers when its object is
            // allocated (kgrctxMapCtxBuffers), so without a 3D object the 3D
            // pipeline wrote through unset context addresses (Xid 31 at VA 0,
            // HUBCLIENT_SCC_NB). Best effort: NvCh3dSt / NvChCeSt record RM's
            // answer (0 = allocated).
            let three_d =
                io.alloc(client, handles.at(5), handles.at(7), BLACKWELL_B, &[0u8; 16]);
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
            // The D3D UMDs' copy queues open with SET_OBJECT(subchannel 5,
            // NV50_DEFERRED_API_CLASS). On a GR channel without that object every
            // later subchannel-4 (copy engine) method raises PBDMA DEVICE (Xid 32,
            // reproduced on the host RM); with the object allocated, RM accepts the
            // binding and the copy engine keeps working. Best effort: NvChDapiSt.
            let deferred = io.alloc(
                client,
                handles.at(5),
                handles.at(9),
                NV50_DEFERRED_API_CLASS,
                &[0u8; 1],
            );
            crate::diag::record_named_bytes(b"NvChDapiSt", u32::from(deferred.is_err()));
            // NVIDIA's KMD puts an inline-to-memory and a 2D object on every graphics
            // channel next to the 3D one: the Vulkan ICD asks their engine ids by the
            // pseudo handles 0xff1fe01a / 0xff1fe02c (610.88 capture) and drives them
            // without allocating them itself. Best effort: NvChI2mSt / NvCh2dSt.
            let i2m = io.alloc(
                client,
                handles.at(5),
                handles.at(10),
                BLACKWELL_INLINE_TO_MEMORY_A,
                &[0u8; 16],
            );
            crate::diag::record_named_bytes(b"NvChI2mSt", u32::from(i2m.is_err()));
            let two_d =
                io.alloc(client, handles.at(5), handles.at(11), FERMI_TWOD_A, &[0u8; 16]);
            crate::diag::record_named_bytes(b"NvCh2dSt", u32::from(two_d.is_err()));
        }
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
        note_channel(parts.token, queue.gpfifo_va, channel_share);
        // `NvPreempt` (default 0): the TSG's context-switch preemption modes, once, through
        // its first VEID 0 channel, before the TSG is first scheduled.
        set_preemption(io, client, context, handles.at(5), vaspace, own.is_none(), parts.token);
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

/// `bind_copy_subchannel` enabled (service value NvCeBindOn=1, read at queue
/// creation).
pub(crate) static CE_BIND_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// SET_OBJECT ENGINE field for subchannel 4 (NvCeBindOn bits 8..12).
static CE_BIND_ENGINE: AtomicU32 = AtomicU32::new(0);

/// The IRQL the last subchannel-4 binding attempt ran at (NvCeIrql).
pub(crate) static CE_BIND_IRQL: AtomicU32 = AtomicU32::new(0xff);

/// The last `bind_copy_subchannel` outcome (NvCeBind, written at teardown),
/// every outcome seen as a bit mask (NvCeSeen) and the bindings made (NvCeDone).
static CE_BIND_OUTCOME: AtomicU32 = AtomicU32::new(0);
static CE_BIND_SEEN: AtomicU32 = AtomicU32::new(0);
static CE_BIND_DONE: AtomicU32 = AtomicU32::new(0);

/// The first GRCE (graphics-runlist copy engine) RM lists, cached.
static GRCE_ENGINE: AtomicU32 = AtomicU32::new(0);

fn grce_engine(io: Io<'_>, client: &NativeClient) -> Option<u32> {
    let known = GRCE_ENGINE.load(Ordering::Relaxed);
    if known != 0 {
        return Some(known);
    }
    // RM's own answer first: the copy engine partnered with GR0 on runqueue 0
    // for this channel class (NV2080_CTRL_CMD_GPU_GET_ENGINE_PARTNERLIST, what
    // RM itself uses for a CE object on a GR channel). The first GRCE in the
    // caps list (COPY0) was not it: subchannel 4 still raised PBDMA DEVICE.
    let mut partners = [0u8; 144];
    partners[0..4].copy_from_slice(&1u32.to_le_bytes()); // NV2080_ENGINE_TYPE_GR0
    partners[4..8].copy_from_slice(&cp::Gen::Gb202.classes().gpfifo.to_le_bytes());
    let asked = io.control(client, nd::H_SUBDEVICE, 0x2080_0147, &mut partners);
    // NvChGrPartQ: 0x100 + error kind when the query failed, else the count.
    crate::diag::record_named_bytes(
        b"NvChGrPartQ",
        match asked {
            Ok(()) => u32::from_le_bytes([partners[12], partners[13], partners[14], partners[15]]),
            Err(error) => 0x100 | error as u32,
        },
    );
    if asked.is_ok() {
        let count = u32::from_le_bytes([partners[12], partners[13], partners[14], partners[15]]);
        if count >= 1 {
            let first = u32::from_le_bytes([partners[16], partners[17], partners[18], partners[19]]);
            crate::diag::record_named_bytes(b"NvChGrPartn", first | (count << 16));
            if first != 0 {
                GRCE_ENGINE.store(first, Ordering::Relaxed);
                return Some(first);
            }
        }
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

extern "C" {
    fn helios_lock_system_read_seh(va: *mut c_void, length: u32, mapped: *mut *mut c_void) -> *mut c_void;
    fn helios_unlock_system_buffer(mdl: *mut c_void);
}

/// Lock the 8-byte progress fence behind dxgkrnl's CPU view and map it once in
/// our own non-paged view. PASSIVE (CreateHwQueue). (0, 0) on failure.
fn lock_fence_view(cpu_va: u64) -> (u64, usize) {
    if cpu_va == 0 || cpu_va % 8 != 0 {
        return (0, 0);
    }
    let mut mapped: *mut c_void = core::ptr::null_mut();
    // SAFETY: the shim catches the probe's raise; 8 bytes never cross a page.
    let mdl = unsafe { helios_lock_system_read_seh(cpu_va as *mut c_void, 8, &mut mapped) };
    if mdl.is_null() || mapped.is_null() {
        return (0, 0);
    }
    (mapped as u64, mdl as usize)
}

impl Drop for NativeQueue {
    fn drop(&mut self) {
        if self.fence_mdl != 0 {
            // SAFETY: the MDL from lock_fence_view, released once.
            unsafe { helios_unlock_system_buffer(self.fence_mdl as *mut c_void) };
            self.fence_mdl = 0;
            self.fence_view = 0;
        }
    }
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

/// Forget the process's association after all contexts and queues are gone, and
/// free again what their failed teardowns left in RM (children first). The shared
/// native-memory client and VA space have transport lifetime and are reclaimed by
/// `native_memory::forget`; the process's own VA space goes in
/// `native_memory::release_process`, after this. `Busy` only while another
/// thread is still initializing the process, which WDDM's ordering rules out.
pub(crate) fn destroy_process(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: &mut ProcessContext,
) -> Result<(), NativeError> {
    let key = process as *const ProcessContext as usize;
    let mut ready = None;
    let mut cleared = false;
    for _ in 0..SLOT_WAIT_TRIES {
        let outcome = process_slot(process).lock().clear();
        match outcome {
            Ok(native) => {
                ready = native;
                cleared = true;
                break;
            }
            Err(nt::Initializing) => super::ctrl::sleep_ms(passive, 1),
        }
    }
    if !cleared {
        return Err(NativeError::Busy);
    }
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    LEDGER.lock().keep_epoch(epoch);
    let mut touched = false;
    loop {
        let next = LEDGER.lock().take(key);
        let Some(leftover) = next else {
            break;
        };
        touched = true;
        let freed = ready.filter(|native| native.epoch == leftover.epoch).is_some_and(|native| {
            let io = Io {
                passive,
                adapter,
                epoch: native.epoch,
            };
            io.free(&native.client, leftover.parent, leftover.object).is_ok()
        });
        if freed {
            TD_FREED.fetch_add(1, Ordering::Relaxed);
        } else {
            TD_LOST.fetch_add(1, Ordering::Relaxed);
        }
    }
    if touched || TD_LOST.load(Ordering::Relaxed) != 0 {
        crate::diag::record_named_bytes(b"NvTdFiled", TD_FILED.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"NvTdFreed", TD_FREED.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"NvTdLost", TD_LOST.load(Ordering::Relaxed));
    }
    Ok(())
}

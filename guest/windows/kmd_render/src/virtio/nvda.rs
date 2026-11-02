//! NVIDIA's Windows UMD personality: NVDA RM controls over a KMD-owned RM client.
//!
//! This is deliberately a small, capture-backed slice. The UMD names pseudo handles;
//! this module owns the corresponding real host-RM root/device/subdevice objects and
//! translates only those. Channel and engine pseudo handles are refused until their
//! WDDM object paths create real objects. Non-RM NVDA functions are not replayed.

use super::gpu::DeviceOwner;
use super::nvrm::{self, Refusal};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_client::{self as rc, Fail, FailKind, Out, Step};
use wdk_sys::{
    NTSTATUS, STATUS_DEVICE_BUSY, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER,
    STATUS_IO_DEVICE_ERROR, STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS,
};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;
const TIMEOUT_MS: u64 = 2_500;
const KNOB_UNREAD: u32 = u32::MAX;
const STEPS: [Step; 9] = [
    Step::OpenCtl,
    Step::VersionQuery,
    Step::VersionStrict,
    Step::CardInfo,
    Step::AllocRoot,
    Step::OpenGpu,
    Step::RegisterGpuFd,
    Step::AllocDevice,
    Step::AllocSubdevice,
];

static KNOB: AtomicU32 = AtomicU32::new(KNOB_UNREAD);
static ESCAPES: AtomicU32 = AtomicU32::new(0);
static CONTROLS: AtomicU32 = AtomicU32::new(0);
static UNSUPPORTED: AtomicU32 = AtomicU32::new(0);
static REFUSALS: AtomicU32 = AtomicU32::new(0);
static BRING_UPS: AtomicU32 = AtomicU32::new(0);
static FAILURE: AtomicU32 = AtomicU32::new(0);
static LAST_FUNCTION: AtomicU32 = AtomicU32::new(0);
static LAST_CONTROL: AtomicU32 = AtomicU32::new(0);
static REGISTER_FD_REFUSED: AtomicU32 = AtomicU32::new(0);

fn persona() -> nd::Persona {
    let value = KNOB.load(Ordering::Relaxed);
    if value == KNOB_UNREAD {
        // A service-root knob overrides; otherwise the NVIDIA INF's
        // Parameters\NvUmd (infverif allows service AddReg only there).
        let root = crate::diag::read_config_dword(crate::diag::knobs::NVIDIA_UMD, u32::MAX);
        let read = if root == u32::MAX {
            crate::diag::read_parameters_dword(crate::diag::knobs::NVIDIA_UMD, 0)
        } else {
            root
        };
        KNOB.store(read, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"NvUmdMode", read);
        return nd::Persona::from_knob(read);
    }
    nd::Persona::from_knob(value)
}

/// Whether an NVDA-magic buffer belongs to this personality rather than Helios's
/// own escape namespace. Off is genuinely inert: no RM traffic and no census.
pub fn enabled() -> bool {
    persona() != nd::Persona::Off
}

/// Forget the client after the transport-wide close sweep. Its handles are already
/// gone, so no close is sent here.
pub fn forget() {
    *SESSION.lock() = Session::new();
    KNOB.store(KNOB_UNREAD, Ordering::Relaxed);
}

pub fn publish_counters() {
    if KNOB.load(Ordering::Relaxed) == KNOB_UNREAD && ESCAPES.load(Ordering::Relaxed) == 0 {
        return;
    }
    crate::diag::record_named_bytes(b"NvUmdEsc", ESCAPES.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdRm", CONTROLS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdUnsup", UNSUPPORTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdRef", REFUSALS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdUp", BRING_UPS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdFail", FAILURE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdFn", LAST_FUNCTION.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdCmd", LAST_CONTROL.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvUmdRegFd", REGISTER_FD_REFUSED.load(Ordering::Relaxed));
}

#[derive(Clone, Copy)]
struct Session {
    adapter: usize,
    epoch: u64,
    initializing: bool,
    up: u8,
    ctl: u32,
    gpu: u32,
    root: u32,
    minor: u32,
    gpu_id: u32,
    version: [u8; rc::VERSION_STR_BYTES],
    objects: [u32; 64],
    objects_len: u8,
    failure: Option<rc::Failure>,
}

impl Session {
    const fn new() -> Self {
        Self {
            adapter: 0,
            epoch: 0,
            initializing: false,
            up: 0,
            ctl: 0,
            gpu: 0,
            root: 0,
            minor: 0,
            gpu_id: 0,
            version: [0; rc::VERSION_STR_BYTES],
            objects: [0; 64],
            objects_len: 0,
            failure: None,
        }
    }

    fn ready(&self) -> bool {
        self.failure.is_none() && usize::from(self.up) == STEPS.len()
    }

    fn map(&self) -> nd::ClientMap {
        nd::ClientMap {
            root: self.root,
            device: nd::H_DEVICE,
            subdevice: nd::H_SUBDEVICE,
            objects: self.objects,
            objects_len: self.objects_len,
        }
    }

    fn note_memory(&mut self, memory: u32) {
        nd::note_object(&mut self.objects, &mut self.objects_len, memory);
    }

    fn forget_memory(&mut self, memory: u32) {
        nd::forget_object(&mut self.objects, &mut self.objects_len, memory);
    }

    fn next(&self) -> Option<Step> {
        STEPS.get(usize::from(self.up)).copied()
    }

    fn apply(&mut self, step: Step, out: Out) {
        match (step, out) {
            (Step::OpenCtl, Out::Handle(h)) => self.ctl = h,
            (Step::VersionQuery, Out::Version(v)) => self.version = v,
            (Step::CardInfo, Out::Card(c)) => {
                self.minor = c.minor;
                self.gpu_id = c.gpu_id;
            }
            (Step::AllocRoot, Out::Handle(h)) => self.root = h,
            (Step::OpenGpu, Out::Handle(h)) => self.gpu = h,
            _ => {}
        }
        self.up = self.up.saturating_add(1);
        self.initializing = false;
    }
}

static SESSION: SpinLock<Session> = SpinLock::new(Session::new());

#[derive(Debug, Clone, Copy)]
enum Error {
    Busy,
    Failed(rc::Failure),
    Host(i32),
    Transport,
    Malformed,
    UnknownHandle,
}

impl From<Error> for NTSTATUS {
    fn from(e: Error) -> Self {
        match e {
            Error::Busy => STATUS_DEVICE_BUSY,
            Error::Failed(_) | Error::Host(_) | Error::Transport | Error::Malformed => {
                STATUS_IO_DEVICE_ERROR
            }
            Error::UnknownHandle => STATUS_INVALID_DEVICE_REQUEST,
        }
    }
}

fn refusal(e: Refusal) -> Fail {
    match e {
        Refusal::MsgType => Fail::new(FailKind::Refused, 1),
        Refusal::NotOwned => Fail::new(FailKind::Refused, 2),
        Refusal::NoResources => Fail::new(FailKind::Refused, 3),
        Refusal::Forbidden => Fail::new(FailKind::Refused, 4),
        Refusal::BadRange => Fail::new(FailKind::Refused, 5),
        Refusal::Transport(VirtioError::Timeout) => Fail::new(FailKind::Transport, 1),
        Refusal::Transport(VirtioError::QueueFull | VirtioError::OutOfMemory) => {
            Fail::new(FailKind::Transport, 2)
        }
        Refusal::Transport(_) => Fail::new(FailKind::Transport, 3),
    }
}

struct Io<'a> {
    passive: PassiveLevel,
    adapter: &'a AdapterContext,
    epoch: u64,
}

impl Io<'_> {
    fn send(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, Fail> {
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
        .map_err(refusal)?;
        if seen.is_some_and(|epoch| epoch != self.epoch) {
            return Err(Fail::new(FailKind::Transport, 0xe0));
        }
        Ok(n)
    }

    fn exchange(
        &self,
        handle: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, Fail> {
        let total = rc::MSG_HDR + rc::IOCTL_REQ + data.len() + nested.len();
        let mut req = Vec::new();
        req.try_reserve_exact(total)
            .map_err(|_| Fail::new(FailKind::Os, 1))?;
        req.resize(total, 0);
        let n = rc::build_ioctl(&mut req, handle, cmd, data, nested)
            .ok_or(Fail::new(FailKind::Parse, 1))?;
        self.send(req.get(..n).ok_or(Fail::new(FailKind::Parse, 1))?, resp)
    }

    fn open_file(&self, device_type: u32) -> Result<u32, Fail> {
        let mut req = [0u8; 32];
        let n = rc::build_open(&mut req, device_type).ok_or(Fail::new(FailKind::Parse, 2))?;
        let mut resp = [0u8; 64];
        let len = self.send(
            req.get(..n).ok_or(Fail::new(FailKind::Parse, 2))?,
            &mut resp,
        )?;
        let reply = resp.get(..len).ok_or(Fail::new(FailKind::Parse, 2))?;
        if let Some(h) = rc::parse_open_reply(reply) {
            return Ok(h);
        }
        match rc::reply_status(reply) {
            Some(s) if s != 0 => Err(Fail::new(FailKind::Host, s.unsigned_abs())),
            _ => {
                if let Some(h) = rc::get32(reply, 4).filter(|h| *h != 0) {
                    self.close_file(h);
                }
                Err(Fail::new(FailKind::Parse, 2))
            }
        }
    }

    fn close_file(&self, handle: u32) -> bool {
        if handle == 0 {
            return true;
        }
        let mut req = [0u8; 32];
        let Some(n) = rc::build_close(&mut req, handle) else {
            return false;
        };
        let mut resp = [0u8; 64];
        self.send(req.get(..n).unwrap_or(&[]), &mut resp)
            .ok()
            .and_then(|len| resp.get(..len))
            .and_then(rc::reply_status)
            .is_some_and(|status| status == 0)
    }

    fn rm_alloc(
        &self,
        c: &Session,
        parent: u32,
        h_new: u32,
        class: u32,
        params: &[u8],
    ) -> Result<Out, Fail> {
        let block = rc::nvos64(c.root, parent, h_new, class, params.len() as u32);
        let mut resp = [0u8; 384];
        let n = self.exchange(
            c.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            params,
            &mut resp,
        )?;
        rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 3))?,
            rc::NVOS64_STATUS_AT,
        )
        .map_err(Fail::from)?;
        Ok(Out::Unit)
    }

    fn perform(&self, step: Step, c: &Session) -> Result<Out, Fail> {
        match step {
            Step::OpenCtl => self.open_file(rc::DEV_CTL).map(Out::Handle),
            Step::VersionQuery => self.version_query(c),
            Step::VersionStrict => self.version_strict(c),
            Step::CardInfo => self.card_info(c),
            Step::AllocRoot => self.alloc_root(c),
            Step::OpenGpu => self.open_file(c.minor).map(Out::Handle),
            Step::RegisterGpuFd => self.register_fd(c),
            Step::AllocDevice => self.rm_alloc(
                c,
                c.root,
                nd::H_DEVICE,
                rc::NV01_DEVICE_0,
                &rc::device_params(),
            ),
            Step::AllocSubdevice => self.rm_alloc(
                c,
                nd::H_DEVICE,
                nd::H_SUBDEVICE,
                rc::NV20_SUBDEVICE_0,
                &rc::subdevice_params(),
            ),
            _ => Err(Fail::new(FailKind::Parse, 4)),
        }
    }

    fn version_query(&self, c: &Session) -> Result<Out, Fail> {
        let blank = [0u8; rc::VERSION_STR_BYTES];
        let data = rc::version_params(rc::VERSION_CMD_QUERY, &blank);
        let mut resp = [0u8; 384];
        let n = self.exchange(
            c.ctl,
            rc::nv_cmd(rc::ESC_CHECK_VERSION_STR, rc::VERSION_BYTES as u32),
            &data,
            &[],
            &mut resp,
        )?;
        Ok(Out::Version(rc::version_from_query_reply(
            resp.get(..n).unwrap_or(&[]),
        )))
    }

    fn version_strict(&self, c: &Session) -> Result<Out, Fail> {
        if c.version[0] == 0 {
            return Ok(Out::Unit);
        }
        let data = rc::version_params(rc::VERSION_CMD_STRICT, &c.version);
        let mut resp = [0u8; 384];
        let n = self.exchange(
            c.ctl,
            rc::nv_cmd(rc::ESC_CHECK_VERSION_STR, rc::VERSION_BYTES as u32),
            &data,
            &[],
            &mut resp,
        )?;
        rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 5))?)
            .map(|_| Out::Unit)
            .map_err(|e| match e {
                rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
                rc::ReplyError::Short => Fail::new(FailKind::Parse, 5),
            })
    }

    fn card_info(&self, c: &Session) -> Result<Out, Fail> {
        let mut data = Vec::new();
        data.try_reserve_exact(rc::CARD_INFO_BYTES)
            .map_err(|_| Fail::new(FailKind::Os, 2))?;
        data.resize(rc::CARD_INFO_BYTES, 0);
        let mut resp = Vec::new();
        resp.try_reserve_exact(rc::REPLY_DATA + rc::CARD_INFO_BYTES + 64)
            .map_err(|_| Fail::new(FailKind::Os, 2))?;
        resp.resize(rc::REPLY_DATA + rc::CARD_INFO_BYTES + 64, 0);
        let n = self.exchange(
            c.ctl,
            rc::nv_cmd(rc::ESC_CARD_INFO, rc::CARD_INFO_BYTES as u32),
            &data,
            &[],
            &mut resp,
        )?;
        let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 6))?)
            .map_err(|e| match e {
                rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
                rc::ReplyError::Short => Fail::new(FailKind::Parse, 6),
            })?;
        rc::parse_card_info(reply.data)
            .map(Out::Card)
            .ok_or(Fail::new(FailKind::Parse, 7))
    }

    fn alloc_root(&self, c: &Session) -> Result<Out, Fail> {
        let block = rc::alloc_root();
        let mut resp = [0u8; 384];
        let n = self.exchange(
            c.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )?;
        let reply = rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 8))?,
            rc::NVOS64_STATUS_AT,
        )
        .map_err(Fail::from)?;
        rc::alloc_new_handle(&reply)
            .map(Out::Handle)
            .ok_or(Fail::new(FailKind::Parse, 9))
    }

    fn register_fd(&self, c: &Session) -> Result<Out, Fail> {
        let data = rc::register_fd_params(c.ctl);
        let mut resp = [0u8; 384];
        let cmd = rc::nv_cmd(rc::ESC_REGISTER_FD, 4);
        match self.exchange(c.gpu, cmd, &data, &[], &mut resp) {
            Ok(n) => {
                let ok = resp
                    .get(..n)
                    .is_some_and(|reply| rc::parse_ioctl_reply(reply).is_ok());
                if !ok {
                    REGISTER_FD_REFUSED.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(f) if f.kind == FailKind::Transport => return Err(f),
            Err(_) => {
                REGISTER_FD_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Out::Unit)
    }
}

#[derive(Clone, Copy)]
struct Ready {
    epoch: u64,
    ctl: u32,
    gpu: u32,
    root: u32,
    minor: u32,
    gpu_id: u32,
    map: nd::ClientMap,
}

fn ready(session: &Session) -> Ready {
    Ready {
        epoch: session.epoch,
        ctl: session.ctl,
        gpu: session.gpu,
        root: session.root,
        minor: session.minor,
        gpu_id: session.gpu_id,
        map: session.map(),
    }
}

fn ensure_ready(passive: PassiveLevel, adapter: &AdapterContext) -> Result<Ready, Error> {
    let adapter_key = adapter as *const AdapterContext as usize;
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    if epoch == 0 {
        return Err(Error::Transport);
    }
    let io = Io {
        passive,
        adapter,
        epoch,
    };
    for _ in 0..STEPS.len() {
        let (step, snapshot) = {
            let mut session = SESSION.lock();
            if session.adapter != 0 && session.adapter != adapter_key {
                return Err(Error::Busy);
            }
            if session.epoch != epoch {
                *session = Session::new();
                session.adapter = adapter_key;
                session.epoch = epoch;
            }
            if session.ready() {
                return Ok(ready(&session));
            }
            if let Some(failure) = session.failure {
                return Err(Error::Failed(failure));
            }
            if session.initializing {
                return Err(Error::Busy);
            }
            let Some(step) = session.next() else {
                return Err(Error::Malformed);
            };
            session.initializing = true;
            (step, *session)
        };
        match io.perform(step, &snapshot) {
            Ok(out) => {
                let mut session = SESSION.lock();
                if session.epoch != epoch {
                    return Err(Error::Transport);
                }
                session.apply(step, out);
                if session.ready() {
                    BRING_UPS.fetch_add(1, Ordering::Relaxed);
                    return Ok(ready(&session));
                }
            }
            Err(fail) => {
                let failure = rc::Failure { step, fail };
                let (ctl, gpu) = {
                    let mut session = SESSION.lock();
                    if session.epoch != epoch {
                        return Err(Error::Transport);
                    }
                    session.failure = Some(failure);
                    session.initializing = false;
                    (session.ctl, session.gpu)
                };
                FAILURE.store(failure.pack(), Ordering::Relaxed);
                crate::diag::record_named_bytes(b"NvUmdFail", failure.pack());
                io.close_file(gpu);
                io.close_file(ctl);
                let mut session = SESSION.lock();
                if session.epoch == epoch {
                    session.ctl = 0;
                    session.gpu = 0;
                }
                return Err(Error::Failed(failure));
            }
        }
    }
    Err(Error::Malformed)
}

static HOST_VERSION: SpinLock<helios_kmd_logic::host_rm_version::Probe> =
    SpinLock::new(helios_kmd_logic::host_rm_version::Probe::new());

/// `"NvHostRmVersion\0"` as UTF-16 (`host_rm_version::VALUE_NAME`).
const HOST_VERSION_NAME: [u16; 16] = {
    let name = helios_kmd_logic::host_rm_version::VALUE_NAME.as_bytes();
    let mut out = [0u16; 16];
    let mut i = 0;
    while i < name.len() {
        out[i] = name[i] as u16;
        i += 1;
    }
    out
};

/// Publish the host RM's version string as `NvHostRmVersion` (REG_SZ, service key) once per
/// transport generation on the RTX personality, whatever `NvUmd` says: the installer compares
/// it with an NVIDIA driver package before it stages NVIDIA's own UMDs, which is also when no
/// NVIDIA UMD has brought the session up yet. A control-channel open, one `QUERY`, a close;
/// nothing else of the host's RM is touched. Called from the registry mirror thread. PASSIVE.
pub fn publish_host_version(passive: PassiveLevel, adapter: &AdapterContext) {
    use helios_kmd_logic::host_rm_version as hv;
    if !super::config::nvidia_personality() {
        return;
    }
    let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    if !HOST_VERSION.lock().begin(epoch) {
        return;
    }
    let io = Io {
        passive,
        adapter,
        epoch,
    };
    let mut probe = Session::new();
    let ok = match io.open_file(rc::DEV_CTL) {
        Ok(ctl) => {
            probe.ctl = ctl;
            let reply = io.version_query(&probe);
            io.close_file(ctl);
            match reply {
                Ok(Out::Version(v)) => {
                    let mut text = [0u16; hv::MAX_TEXT + 1];
                    let bytes = hv::text(&v).and_then(|t| hv::utf16z(t, &mut text));
                    match bytes {
                        Some(bytes) => {
                            crate::diag::record_named_string(&HOST_VERSION_NAME, &text, bytes);
                            true
                        }
                        // The host answered without a usable string: asking again will not help.
                        None => true,
                    }
                }
                _ => false,
            }
        }
        Err(_) => false,
    };
    HOST_VERSION.lock().finish(epoch, ok);
}

/// Return the shared native-memory client, installing its GPU_DEVICE VA space on
/// first use. The client belongs to the KMD, never to the escaping process.
pub fn memory_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    _process: usize,
) -> Result<super::native_memory::NativeClient, super::native_memory::Error> {
    let ready = ensure_ready(passive, adapter).map_err(|_| super::native_memory::Error::Host)?;
    let client = super::native_memory::NativeClient {
        epoch: ready.epoch,
        ctl: ready.ctl,
        gpu: ready.gpu,
        root: ready.root,
        minor: ready.minor,
        gpu_id: ready.gpu_id,
    };
    match super::native_memory::install_client(passive, adapter, client) {
        Ok(()) => Ok(client),
        Err(super::native_memory::Error::Busy) => {
            super::native_memory::channel_client(passive, adapter, _process)
                .map(|(client, _)| client)
        }
        Err(error) => Err(error),
    }
}

/// Serve one NVDA private buffer. The caller has already matched NVDA_MAGIC.
/// `h_device` may be zero for NVAPI's adapter-scoped calls. CUDA's two
/// context-scoped local replies validate both raw WDDM scopes in pure logic.
pub fn escape(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    h_device: usize,
    h_context: usize,
    buf: &mut [u8],
) -> NTSTATUS {
    let header = match nd::parse_header(buf) {
        Ok(header) => header,
        Err(_) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        }
    };
    ESCAPES.fetch_add(1, Ordering::Relaxed);
    LAST_FUNCTION.store(header.function, Ordering::Relaxed);
    // Pure local replies never initialize the KMD RM client and never touch the
    // transport. This also returns shortened 0x30 replies directly, while the
    // backing buffer still has its original (larger) length.
    if header.function == nd::NVDA_FN_USER_SHARED && buf.len() == nd::NVDA_FN_USER_SHARED_BYTES {
        return user_shared_block(buf);
    }
    match nd::apply_local(buf, h_device, h_context) {
        Ok(nd::LocalOutcome::Applied) => {
            // The Vulkan ICD's per-head query: head 0 is the mode this adapter scans out.
            let (width, height) = adapter.scanout_extent();
            let hz = ((adapter.effective_refresh_mhz() + 500) / 1000).max(1);
            if nd::is_head0_query(buf) {
                nd::set_head_mode(buf, width, height, hz);
            } else if nd::is_raster_query(buf) {
                nd::set_raster(buf, width, height, hz);
            } else if nd::is_display_info(buf) {
                nd::set_display_info_mode(buf, width, height);
            }
            return STATUS_SUCCESS;
        }
        Ok(nd::LocalOutcome::ForwardRmControl) => {}
        Err(nd::LocalError::Invalid) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        }
        Err(nd::LocalError::Unsupported) => {
            UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
            return STATUS_NOT_IMPLEMENTED;
        }
    }
    let control = match nd::parse_rm_control(buf) {
        Ok(control) => control,
        Err(nd::ControlError::NotRmControl) => {
            UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
            return STATUS_NOT_IMPLEMENTED;
        }
        Err(_) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return STATUS_INVALID_PARAMETER;
        }
    };
    CONTROLS.fetch_add(1, Ordering::Relaxed);
    LAST_CONTROL.store(control.cmd, Ordering::Relaxed);
    if persona() != nd::Persona::Forward {
        return STATUS_NOT_IMPLEMENTED;
    }
    let ready = match ensure_ready(passive, adapter) {
        Ok(ready) => ready,
        Err(error) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return error.into();
        }
    };
    let io = Io {
        passive,
        adapter,
        epoch: ready.epoch,
    };

    let request_len = rc::MSG_HDR + rc::IOCTL_REQ + rc::NVOS54_BYTES + control.params.len();
    let response_len = rc::REPLY_DATA + rc::NVOS54_BYTES + control.params.len();
    let mut request = Vec::new();
    if request.try_reserve_exact(request_len).is_err() {
        return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
    }
    request.resize(request_len, 0);
    if nd::build_host_control(&mut request, ready.ctl, &control, &ready.map).is_err() {
        REFUSALS.fetch_add(1, Ordering::Relaxed);
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    let mut response = Vec::new();
    if response.try_reserve_exact(response_len).is_err() {
        return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
    }
    response.resize(response_len, 0);
    let n = match io.send(&request, &mut response) {
        Ok(n) => n,
        Err(_) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return STATUS_IO_DEVICE_ERROR;
        }
    };
    let reply = match nd::parse_host_reply_for(
        response.get(..n).unwrap_or(&[]),
        ready.ctl,
        &control,
        &ready.map,
    ) {
        Ok(reply) => reply,
        Err(nd::HostReplyError::Host(_)) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return STATUS_IO_DEVICE_ERROR;
        }
        Err(nd::HostReplyError::Malformed) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            return STATUS_IO_DEVICE_ERROR;
        }
    };
    match nd::apply_host_reply(buf, &reply) {
        Ok(()) => STATUS_SUCCESS,
        Err(_) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            STATUS_IO_DEVICE_ERROR
        }
    }
}

/// Register one real object returned by native allocation for later RM controls.
pub fn note_memory(memory: u32) {
    SESSION.lock().note_memory(memory);
}

/// The allocation behind `memory` was destroyed: its handle is no longer nameable.
pub fn forget_memory(memory: u32) {
    SESSION.lock().forget_memory(memory);
}

extern "system" {
    fn ZwAllocateVirtualMemory(
        process: wdk_sys::HANDLE,
        base: *mut *mut core::ffi::c_void,
        zero_bits: usize,
        size: *mut usize,
        allocation_type: u32,
        protect: u32,
    ) -> NTSTATUS;
}

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_READWRITE: u32 = 0x04;
const USER_SHARED_BYTES: usize = 0x1_0000;

/// NVDA 0x01000077 (Vulkan ICD, adapter scope): NVIDIA's KMD answers a user-space
/// address of a block it maps into the caller (the capture: 0x1b0b7f59000); the ICD
/// keeps it and reads it while it creates a device (a count at +0x60), and crashed
/// on the null it had without an answer. A zeroed block of the caller's own memory
/// stands in: allocated in the calling process (an escape runs in its context),
/// released by the OS with the process, nothing for the KMD to track.
fn user_shared_block(buf: &mut [u8]) -> NTSTATUS {
    let mut base: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut size = USER_SHARED_BYTES;
    // SAFETY: NtCurrentProcess() (-1) in the caller's context; out-pointers are locals.
    let status = unsafe {
        ZwAllocateVirtualMemory(
            usize::MAX as wdk_sys::HANDLE,
            &mut base,
            0,
            &mut size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if status < 0 || base.is_null() {
        crate::diag::record_named_bytes(b"NvUserShErr", status as u32);
        return STATUS_INVALID_PARAMETER;
    }
    nd::answer_user_shared(buf, base as u64);
    STATUS_SUCCESS
}

/// Tell the host which NVIDIA UMD release this guest speaks (REG_SZ
/// `NvUmdVersion`: the service key's override, else the INF's under
/// `Parameters`; the installer stages only a package of that release) and read
/// its verdict (`helios_kmd_logic::umd_version`). StartDevice, PASSIVE,
/// transport up.
///
/// `false` only when the host says its RM is another release: the start then
/// fails, and the host refuses this guest's RM calls until a start reports a
/// release it accepts. No release recorded, a value that is not one, a host
/// without the check, or a transport error: recorded, and the start goes on.
///
/// `NvUmdVer` is the release reported (`61088` for 610.88; 0 = none),
/// `NvUmdChk` the outcome (0 match, 1 mismatch, 2 unchecked by the host,
/// 3 branch only, 0x10 host without the check, 0x11 host errno, 0x12 malformed
/// reply, 0x13 transport error, 0xfe a value that is not a REG_SZ release,
/// 0xff not recorded), `NvHostRm` the host RM release (`6105704` for
/// 610.57.04; 0 if not told).
#[inline(never)]
pub fn check_umd_release(passive: PassiveLevel, adapter: &AdapterContext) -> bool {
    use helios_kmd_logic::umd_version as uv;
    let name = crate::diag::knobs::NVIDIA_UMD_VERSION;
    let value = uv::configured(
        crate::diag::read_config_value(name, uv::Value::Absent, uv::Value::from_registry),
        || crate::diag::read_parameters_value(name, uv::Value::Absent, uv::Value::from_registry),
    );
    let release = match value {
        uv::Value::Release(release) => release,
        uv::Value::Absent => {
            crate::diag::record_named_bytes(b"NvUmdVer", 0);
            crate::diag::record_named_bytes(b"NvUmdChk", 0xff);
            crate::kmsg(c"Helios: NVIDIA UMD release not recorded (NvUmdVersion); not checked\n");
            return true;
        }
        uv::Value::Unusable => {
            crate::diag::record_named_bytes(b"NvUmdVer", 0);
            crate::diag::record_named_bytes(b"NvUmdChk", 0xfe);
            crate::kmsg(
                c"Helios: NvUmdVersion is not a REG_SZ NVIDIA release such as 610.88; not checked\n",
            );
            return true;
        }
    };
    crate::diag::record_named_bytes(b"NvUmdVer", release.dword());
    let req = uv::build_request(release);
    let mut resp = [0u8; uv::REPLY_BYTES];
    let outcome = match super::ctrl::raw_roundtrip(passive, adapter, &req, &mut resp, TIMEOUT_MS) {
        Ok(n) => uv::parse_reply(&resp, n),
        Err(_) => {
            crate::diag::record_named_bytes(b"NvUmdChk", 0x13);
            return true;
        }
    };
    crate::diag::record_named_bytes(b"NvUmdChk", outcome.code());
    crate::diag::record_named_bytes(b"NvHostRm", outcome.host());
    if !outcome.start_allowed() {
        crate::kmsg(
            c"Helios: the NVIDIA UMDs are not the host RM's release (NvUmdChk=1); start refused\n",
        );
    }
    outcome.start_allowed()
}

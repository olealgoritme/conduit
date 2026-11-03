//! The RM surface served to a guest that runs NVIDIA's own Windows UMDs.
//!
//! [`abi::rmallow`] is RM's own rule: what RM exports to an unprivileged
//! caller, applied on the guest's behalf, always. That set is wide -- several
//! hundred controls -- because it is written for any userspace driver. A
//! Windows guest in the NVIDIA UMD personality (`CONDUIT_NVIDIA_UMD=1`) reaches
//! RM through one path only: its KMD forwards the RM controls NVIDIA's UMDs put
//! in their `NVDA` escapes, and makes its own clients, channels and memory.
//! What that path sends is known, and this file is that list. Anything else
//! is something the guest's own driver never sends, and is answered with
//! `NV_ERR_INSUFFICIENT_PERMISSIONS` without reaching RM.
//!
//! On top of the lists:
//!
//! * every handle a request names as its client must be a client this guest
//!   made (any file of this backend), and a mapping must name an object this
//!   guest allocated, of a class that is memory rather than the device, the
//!   subdevice or the client itself (mapping those maps GPU registers);
//! * memory named by a CPU address only ever comes as
//!   `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, which `osdesc.rs` translates to the
//!   guest's own pages;
//! * `NV_ESC_RM_IDLE_CHANNELS` may not carry a channel list (RM would read it
//!   through pointers in this process and could name other clients' channels);
//! * parameter blocks are bounded: RM's exact sizes come from
//!   [`abi::rmallow`], the controls RM forwards to GSP unread are pinned to the
//!   sizes NVIDIA's UMDs were captured sending, and everything has a ceiling.
//!
//! # Where the lists come from
//!
//! * NVIDIA's own UMDs (D3D11, D3D12, Vulkan, CUDA, DWM) on a real RTX 5090
//!   under NVIDIA's own KMD, driver 610.88: every `NVDA` RM control
//!   (function `0x2b`) in the nvtrace captures, decoded from the
//!   `pPrivateDriverData` of each `D3DKMTEscape`.
//! * What the guest KMD allocates and controls itself (`kmd_render`
//!   `native_channel.rs`, `native_memory.rs`, `nvda.rs`, `rm_client`), checked
//!   against the backend's own end-of-run census (`RM_CONTROL command(s)`,
//!   `RM_ALLOC class(es)`) from runs of the personality in a VM.
//!
//! The `NV0073` (display common) controls the Vulkan ICD sends are answered by
//! the guest KMD itself and never forwarded, so they are not here.
//!
//! # Modes
//!
//! `CONDUIT_RM_POLICY` = `off`, `log` or `enforce`. Unset, it is `log` in the
//! NVIDIA UMD personality and `off` otherwise. `log` serves everything and
//! names what `enforce` would refuse (rate-limited, plus one teardown line
//! with the totals), so the lists can be completed from a real run before
//! they are enforced. RM's own rule ([`abi::rmallow`]) is enforced in every
//! mode; this one only narrows it.

use std::collections::HashMap;

/// The environment variable that selects the [`Mode`].
pub const ENV: &str = "CONDUIT_RM_POLICY";

/// `NV_ERR_INSUFFICIENT_PERMISSIONS` (nvstatuscodes.h).
pub const NV_ERR_INSUFFICIENT_PERMISSIONS: u32 = 0x1b;

/// Largest RM control parameter block served. The largest NVIDIA's UMDs were
/// captured sending is 9240 bytes (`NV2080_CTRL_CMD_GR_GET_GLOBAL_SM_ORDER`).
pub const MAX_CONTROL_PARAMS: u32 = 32 * 1024;
/// Largest allocation parameter block served. RM's own sizes are far smaller;
/// `abi::rmallow` requires the exact one besides.
pub const MAX_ALLOC_PARAMS: usize = 4096;
/// RM clients one guest may hold at once. The KMD makes one, plus one per
/// process that runs NVIDIA's UMDs.
pub const MAX_CLIENTS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The policy is not consulted.
    Off,
    /// Everything is served; what `Enforce` would refuse is logged.
    Log,
    /// What the policy does not allow is refused.
    Enforce,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Some(Self::Off),
            "log" | "log-only" | "logonly" => Some(Self::Log),
            "enforce" | "on" | "1" => Some(Self::Enforce),
            _ => None,
        }
    }

    /// The mode when [`ENV`] is unset.
    pub const fn default_for(nvidia_umd: bool) -> Self {
        if nvidia_umd { Self::Log } else { Self::Off }
    }

    /// [`ENV`] if it is set and valid, else the default. An invalid value is
    /// returned as `Err` with the default, so the caller can say so.
    pub fn from_env_value(value: Option<&str>, nvidia_umd: bool) -> Result<Self, Self> {
        match value {
            None => Ok(Self::default_for(nvidia_umd)),
            Some(v) => Self::parse(v).ok_or(Self::default_for(nvidia_umd)),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Log => "log",
            Self::Enforce => "enforce",
        }
    }
}

/// Who sends a control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sender {
    /// NVIDIA's UMDs, forwarded by the guest KMD's `NVDA` path.
    Umd,
    /// The guest KMD, for its own channels, memory and clients.
    Kmd,
    Both,
}

/// One RM control the personality sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctrl {
    pub cmd: u32,
    pub name: &'static str,
    pub from: Sender,
    /// The only parameter size served, for a control RM hands to GSP without
    /// a size of its own (the legacy RPC range, `cmd & 0xc000 == 0x8000`).
    /// `None` where [`abi::rmallow`] already holds RM's exact size.
    pub params: Option<u32>,
}

const fn c(cmd: u32, name: &'static str, from: Sender) -> Ctrl {
    Ctrl {
        cmd,
        name,
        from,
        params: None,
    }
}

const fn gsp(cmd: u32, name: &'static str, params: u32) -> Ctrl {
    Ctrl {
        cmd,
        name,
        from: Sender::Umd,
        params: Some(params),
    }
}

/// The RM controls served, sorted by command.
pub static CONTROLS: &[Ctrl] = &[
    c(
        0x0000_0102,
        "NV0000_CTRL_CMD_SYSTEM_GET_CPU_INFO",
        Sender::Umd,
    ),
    c(
        0x0000_0204,
        "NV0000_CTRL_CMD_GPU_GET_DEVICE_IDS",
        Sender::Umd,
    ),
    c(
        0x0000_0205,
        "NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2",
        Sender::Umd,
    ),
    c(0x0000_021b, "NV0000_CTRL_CMD_GPU_GET_PCI_INFO", Sender::Umd),
    c(
        0x0000_0275,
        "NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID",
        Sender::Umd,
    ),
    c(
        0x0000_0301,
        "NV0000_CTRL_CMD_GSYNC_GET_ATTACHED_IDS",
        Sender::Umd,
    ),
    c(
        0x0000_3d05,
        "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD",
        Sender::Kmd,
    ),
    c(
        0x0000_3d06,
        "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD",
        Sender::Kmd,
    ),
    c(
        0x0080_0289,
        "NV0080_CTRL_CMD_GPU_GET_VIRTUALIZATION_MODE",
        Sender::Umd,
    ),
    c(
        0x0080_0292,
        "NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2",
        Sender::Both,
    ),
    // The guest KMD's GfxP buffer sizes (`NvPreempt` GFXP).
    c(
        0x0080_1707,
        "NV0080_CTRL_CMD_FIFO_GET_ENGINE_CONTEXT_PROPERTIES",
        Sender::Kmd,
    ),
    c(
        0x0080_1806,
        "NV0080_CTRL_CMD_DMA_ADV_SCHED_GET_VA_CAPS",
        Sender::Umd,
    ),
    c(0x0080_180d, "NV0080_CTRL_CMD_DMA_GET_CAPS", Sender::Umd),
    c(0x0080_1b01, "NV0080_CTRL_CMD_NVENC_GET_CAPS", Sender::Umd),
    c(
        0x0080_1c02,
        "NV0080_CTRL_CMD_NVDEC_GET_CAPS_V2",
        Sender::Umd,
    ),
    gsp(0x0080_9908, "NV0080 legacy GSP control 0x9908", 16),
    c(
        0x0090_010b,
        "NV0090_CTRL_CMD_SET_LG_SECTOR_PROMOTION",
        Sender::Umd,
    ),
    c(0x2080_0102, "NV2080_CTRL_CMD_GPU_GET_INFO_V2", Sender::Umd),
    c(
        0x2080_0119,
        "NV2080_CTRL_CMD_GPU_GET_SIMULATION_INFO",
        Sender::Umd,
    ),
    c(
        0x2080_012f,
        "NV2080_CTRL_CMD_GPU_QUERY_ECC_STATUS",
        Sender::Umd,
    ),
    c(0x2080_0142, "NV2080_CTRL_CMD_GPU_GET_ID", Sender::Umd),
    c(
        0x2080_0147,
        "NV2080_CTRL_CMD_GPU_GET_ENGINE_PARTNERLIST",
        Sender::Kmd,
    ),
    c(
        0x2080_0170,
        "NV2080_CTRL_CMD_GPU_GET_ENGINES_V2",
        Sender::Both,
    ),
    // The guest KMD's GR completion notification (`native_completion`).
    c(
        0x2080_0301,
        "NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION",
        Sender::Kmd,
    ),
    c(0x2080_0403, "NV2080_CTRL_CMD_TIMER_GET_TIME", Sender::Umd),
    c(
        0x2080_0406,
        "NV2080_CTRL_CMD_TIMER_GET_GPU_CPU_TIME_CORRELATION_INFO",
        Sender::Umd,
    ),
    c(
        0x2080_1206,
        "NV2080_CTRL_CMD_GR_GET_ZCULL_INFO",
        Sender::Umd,
    ),
    // The guest KMD's TSG preemption modes (`NvPreempt`).
    c(
        0x2080_1210,
        "NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE",
        Sender::Kmd,
    ),
    c(
        0x2080_1211,
        "NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND",
        Sender::Kmd,
    ),
    c(
        0x2080_121b,
        "NV2080_CTRL_CMD_GR_GET_GLOBAL_SM_ORDER",
        Sender::Umd,
    ),
    c(
        0x2080_1235,
        "NV2080_CTRL_CMD_GR_GET_CTXSW_MODES",
        Sender::Kmd,
    ),
    c(
        0x2080_1315,
        "NV2080_CTRL_CMD_FB_GET_GPU_CACHE_INFO",
        Sender::Umd,
    ),
    c(
        0x2080_1320,
        "NV2080_CTRL_CMD_FB_GET_FB_REGION_INFO",
        Sender::Umd,
    ),
    c(
        0x2080_1352,
        "NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT",
        Sender::Umd,
    ),
    c(0x2080_1801, "NV2080_CTRL_CMD_BUS_GET_PCI_INFO", Sender::Umd),
    c(0x2080_2a03, "NV2080_CTRL_CMD_CE_GET_CAPS_V2", Sender::Both),
    gsp(0x2080_8159, "NV2080 legacy GSP control 0x8159", 332),
    gsp(0x2080_a0d1, "NV2080 legacy GSP control 0xa0d1", 2024),
    c(0x906f_0101, "NV906F_CTRL_GET_CLASS_ENGINEID", Sender::Umd),
    c(0xa06c_0101, "NVA06C_CTRL_CMD_GPFIFO_SCHEDULE", Sender::Kmd),
    c(0xa06c_0106, "NVA06C_CTRL_CMD_GET_INFO", Sender::Kmd),
    c(0xa06f_0104, "NVA06F_CTRL_CMD_BIND", Sender::Kmd),
    c(
        0xc36f_0108,
        "NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN",
        Sender::Kmd,
    ),
    c(
        0xc36f_010a,
        "NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX",
        Sender::Kmd,
    ),
];

/// Controls NVIDIA's UMDs send that act on the host GPU as a whole, not on
/// this guest's objects. In `enforce` mode they are answered `NV_OK` here and
/// never reach RM: the caller only needs the call to succeed.
///
/// `NV0080_CTRL_CMD_PERF_CUDA_LIMIT_SET_CONTROL` (CUDA, at context creation)
/// switches the device's CUDA clock limit, which every host process shares.
pub static ABSORBED: &[Ctrl] = &[c(
    0x0080_1909,
    "NV0080_CTRL_CMD_PERF_CUDA_LIMIT_SET_CONTROL",
    Sender::Umd,
)];

/// `NV01_ROOT`, `NV01_ROOT_NON_PRIV` and `NV01_ROOT_CLIENT`: the classes whose
/// allocation makes a client.
pub const ROOT_CLASSES: [u32; 3] = [0x0, 0x1, 0x41];
/// `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`.
pub const OS_DESCRIPTOR: u32 = 0x71;
/// `NV01_DEVICE_0` and `NV20_SUBDEVICE_0`: mapping one maps GPU registers.
const REGISTER_CLASSES: [u32; 2] = [0x80, 0x2080];

/// The classes allocated, sorted. Engine and channel classes are listed for
/// every generation from Turing on: the guest KMD picks the ones the GPU's
/// class list names, and the captures are from Blackwell.
pub static CLASSES: &[(u32, &str)] = &[
    (0x003e, "NV01_MEMORY_SYSTEM"),
    (0x0040, "NV01_MEMORY_LOCAL_USER"),
    (0x0041, "NV01_ROOT_CLIENT"),
    (0x0071, "NV01_MEMORY_SYSTEM_OS_DESCRIPTOR"),
    // The guest KMD's HW-queue completion events (`native_completion`).
    (0x0079, "NV01_EVENT_OS_EVENT"),
    (0x0080, "NV01_DEVICE_0"),
    (0x2080, "NV20_SUBDEVICE_0"),
    (0x5080, "NV50_DEFERRED_API_CLASS"),
    (0x50a0, "NV50_MEMORY_VIRTUAL"),
    (0x902d, "FERMI_TWOD_A"),
    (0x9067, "FERMI_CONTEXT_SHARE_A"),
    (0x90f1, "FERMI_VASPACE_A"),
    (0xa06c, "KEPLER_CHANNEL_GROUP_A"),
    (0xa140, "KEPLER_INLINE_TO_MEMORY_B"),
    (0xc461, "TURING_USERMODE_A"),
    (0xc46f, "TURING_CHANNEL_GPFIFO_A"),
    (0xc561, "AMPERE_USERMODE_A"),
    (0xc56f, "AMPERE_CHANNEL_GPFIFO_A"),
    (0xc597, "TURING_A"),
    (0xc5b5, "TURING_DMA_COPY_A"),
    (0xc5c0, "TURING_COMPUTE_A"),
    (0xc661, "HOPPER_USERMODE_A"),
    (0xc697, "AMPERE_A"),
    (0xc6b5, "AMPERE_DMA_COPY_A"),
    (0xc6c0, "AMPERE_COMPUTE_A"),
    (0xc761, "BLACKWELL_USERMODE_A"),
    (0xc797, "AMPERE_B"),
    (0xc7b5, "AMPERE_DMA_COPY_B"),
    (0xc7c0, "AMPERE_COMPUTE_B"),
    (0xc86f, "HOPPER_CHANNEL_GPFIFO_A"),
    (0xc8b5, "HOPPER_DMA_COPY_A"),
    (0xc96f, "BLACKWELL_CHANNEL_GPFIFO_A"),
    (0xc997, "ADA_A"),
    (0xc9b5, "BLACKWELL_DMA_COPY_A"),
    (0xc9c0, "ADA_COMPUTE_A"),
    (0xca6f, "BLACKWELL_CHANNEL_GPFIFO_B"),
    (0xcab5, "BLACKWELL_DMA_COPY_B"),
    (0xcb97, "HOPPER_A"),
    (0xcbc0, "HOPPER_COMPUTE_A"),
    (0xcd40, "BLACKWELL_INLINE_TO_MEMORY_A"),
    (0xcd97, "BLACKWELL_A"),
    (0xcdc0, "BLACKWELL_COMPUTE_A"),
    (0xce97, "BLACKWELL_B"),
    (0xcec0, "BLACKWELL_COMPUTE_B"),
];

// The NVIDIA escapes (`'F'` ioctls) served, by number.
const ESC_RM_ALLOC_MEMORY: u32 = 0x27;
const ESC_RM_FREE: u32 = 0x29;
const ESC_RM_CONTROL: u32 = 0x2a;
const ESC_RM_ALLOC: u32 = 0x2b;
const ESC_RM_IDLE_CHANNELS: u32 = 0x41;
const ESC_RM_MAP_MEMORY: u32 = 0x4e;
const ESC_RM_UNMAP_MEMORY: u32 = 0x4f;
const ESC_RM_MAP_MEMORY_DMA: u32 = 0x57;
const ESC_RM_UNMAP_MEMORY_DMA: u32 = 0x58;
const ESC_CARD_INFO: u32 = 0xc8;
const ESC_REGISTER_FD: u32 = 0xc9;
const ESC_ALLOC_OS_EVENT: u32 = 0xce;
const ESC_FREE_OS_EVENT: u32 = 0xcf;
const ESC_CHECK_VERSION_STR: u32 = 0xd2;

/// The escapes served, and where each keeps its client handle and its status.
pub static ESCAPES: &[Escape] = &[
    Escape::new(
        ESC_RM_ALLOC_MEMORY,
        "NV_ESC_RM_ALLOC_MEMORY",
        Some(0),
        Some(40),
    ),
    Escape::new(ESC_RM_FREE, "NV_ESC_RM_FREE", Some(0), Some(12)),
    Escape::new(ESC_RM_CONTROL, "NV_ESC_RM_CONTROL", Some(0), Some(28)),
    // The status is at 40 in NVOS64 and 28 in NVOS21; see `status_at`.
    Escape::new(ESC_RM_ALLOC, "NV_ESC_RM_ALLOC", Some(0), Some(40)),
    Escape::new(
        ESC_RM_IDLE_CHANNELS,
        "NV_ESC_RM_IDLE_CHANNELS",
        Some(0),
        Some(48),
    ),
    Escape::new(ESC_RM_MAP_MEMORY, "NV_ESC_RM_MAP_MEMORY", Some(0), Some(40)),
    Escape::new(
        ESC_RM_UNMAP_MEMORY,
        "NV_ESC_RM_UNMAP_MEMORY",
        Some(0),
        Some(24),
    ),
    Escape::new(
        ESC_RM_MAP_MEMORY_DMA,
        "NV_ESC_RM_MAP_MEMORY_DMA",
        Some(0),
        Some(56),
    ),
    Escape::new(
        ESC_RM_UNMAP_MEMORY_DMA,
        "NV_ESC_RM_UNMAP_MEMORY_DMA",
        Some(0),
        Some(40),
    ),
    Escape::new(ESC_CARD_INFO, "NV_ESC_CARD_INFO", None, None),
    Escape::new(ESC_REGISTER_FD, "NV_ESC_REGISTER_FD", None, None),
    // nv_ioctl_alloc_os_event_t { hClient, hDevice, fd, Status }: the guest KMD's
    // completion event files (`native_completion`).
    Escape::new(
        ESC_ALLOC_OS_EVENT,
        "NV_ESC_ALLOC_OS_EVENT",
        Some(0),
        Some(12),
    ),
    Escape::new(ESC_FREE_OS_EVENT, "NV_ESC_FREE_OS_EVENT", Some(0), Some(12)),
    Escape::new(
        ESC_CHECK_VERSION_STR,
        "NV_ESC_CHECK_VERSION_STR",
        None,
        None,
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Escape {
    pub nr: u32,
    pub name: &'static str,
    /// Offset of the client handle (`hClient` / `hRoot`) in the top-level
    /// struct, for an escape that names one.
    pub client_at: Option<usize>,
    /// Offset of RM's status word in the top-level struct.
    pub status_at: Option<usize>,
}

impl Escape {
    const fn new(
        nr: u32,
        name: &'static str,
        client_at: Option<usize>,
        status_at: Option<usize>,
    ) -> Self {
        Self {
            nr,
            name,
            client_at,
            status_at,
        }
    }
}

pub fn escape(nr: u32) -> Option<&'static Escape> {
    ESCAPES.iter().find(|e| e.nr == nr)
}

pub fn control(cmd: u32) -> Option<&'static Ctrl> {
    CONTROLS
        .binary_search_by_key(&cmd, |c| c.cmd)
        .ok()
        .map(|i| &CONTROLS[i])
}

pub fn absorbed(cmd: u32) -> Option<&'static Ctrl> {
    ABSORBED.iter().find(|c| c.cmd == cmd)
}

pub fn class_name(class: u32) -> Option<&'static str> {
    CLASSES
        .binary_search_by_key(&class, |(c, _)| *c)
        .ok()
        .map(|i| CLASSES[i].1)
}

/// Where RM writes its status for this escape and top-level struct length.
pub fn status_at(nr: u32, data_len: usize) -> Option<usize> {
    if nr == ESC_RM_ALLOC {
        return match data_len {
            48 => Some(40),
            32 => Some(28),
            _ => None,
        };
    }
    escape(nr).and_then(|e| e.status_at)
}

/// The clients and objects this guest made, read from RM's answers.
#[derive(Default)]
pub struct Owned {
    /// Client handle -> the backend file it was made on (RM frees it when
    /// that file closes).
    clients: HashMap<u32, u64>,
    /// (client, object) -> class.
    objects: HashMap<(u32, u32), u32>,
}

impl Owned {
    pub fn client(&self, h: u32) -> bool {
        self.clients.contains_key(&h)
    }

    pub fn clients(&self) -> usize {
        self.clients.len()
    }

    pub fn class_of(&self, client: u32, h: u32) -> Option<u32> {
        self.objects.get(&(client, h)).copied()
    }

    /// RM allocated `new` of `class` under `client` (for a root class, `new`
    /// is the client itself and `client` is ignored).
    pub fn allocated(&mut self, file: u64, client: u32, new: u32, class: u32) {
        if ROOT_CLASSES.contains(&class) {
            self.clients.insert(new, file);
            self.objects.insert((new, new), class);
        } else if self.clients.contains_key(&client) {
            self.objects.insert((client, new), class);
        }
    }

    /// RM freed `h` in `client`; freeing the client frees all of it. Children
    /// of a freed object stay recorded until the client goes: RM no longer
    /// knows their handles, so a request naming one fails in RM.
    pub fn freed(&mut self, client: u32, h: u32) {
        if h == client {
            self.clients.remove(&client);
            self.objects.retain(|(c, _), _| *c != client);
        } else {
            self.objects.remove(&(client, h));
        }
    }

    /// The backend file closed: RM freed every client made on it.
    pub fn file_closed(&mut self, file: u64) {
        let gone: Vec<u32> = self
            .clients
            .iter()
            .filter(|(_, f)| **f == file)
            .map(|(c, _)| *c)
            .collect();
        for c in gone {
            self.freed(c, c);
        }
    }
}

/// What to do with one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Answer `NV_OK` without forwarding (enforce mode only).
    Absorb(&'static Ctrl),
    /// Not served. `what` names the request (the rate-limit key), `why` says
    /// which rule.
    Deny {
        what: String,
        why: String,
    },
}

fn deny(what: impl Into<String>, why: impl Into<String>) -> Verdict {
    Verdict::Deny {
        what: what.into(),
        why: why.into(),
    }
}

fn word(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
}

fn quad(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|w| u64::from_le_bytes(w.try_into().unwrap()))
}

/// Judge one `'F'` ioctl. `data` is the top-level struct the guest sent and
/// `nested` what follows it (the parameters a pointer in it addresses).
pub fn judge(owned: &Owned, nr: u32, data: &[u8], nested: &[u8]) -> Verdict {
    let Some(esc) = escape(nr) else {
        return deny(
            format!("escape {nr:#04x}"),
            "not an escape the NVIDIA UMD personality sends",
        );
    };
    let short = || deny(esc.name, format!("{} bytes is too short", data.len()));

    // Every RM call names a client, and it has to be one of this guest's.
    // A root allocation is the exception: it makes the client.
    let creates_client =
        nr == ESC_RM_ALLOC && word(data, 12).is_some_and(|c| ROOT_CLASSES.contains(&c));
    if let Some(at) = esc.client_at
        && !creates_client
    {
        let Some(client) = word(data, at) else {
            return short();
        };
        if !owned.client(client) {
            return deny(
                format!("{} on a foreign client", esc.name),
                format!("client {client:#010x} was not made by this guest"),
            );
        }
    }

    match nr {
        ESC_RM_CONTROL => {
            // NVOS54: hClient, hObject, cmd at 8, paramsSize at 24.
            let (Some(cmd), Some(size)) = (word(data, 8), word(data, 24)) else {
                return short();
            };
            if let Some(a) = absorbed(cmd) {
                return Verdict::Absorb(a);
            }
            let Some(ctrl) = control(cmd) else {
                return deny(
                    format!("RM_CONTROL {cmd:#010x}"),
                    "not in the NVIDIA UMD personality's control list",
                );
            };
            if size > MAX_CONTROL_PARAMS {
                return deny(
                    format!("RM_CONTROL {cmd:#010x} {}", ctrl.name),
                    format!("{size} parameter bytes, more than {MAX_CONTROL_PARAMS}"),
                );
            }
            if let Some(want) = ctrl.params
                && size != want
            {
                return deny(
                    format!("RM_CONTROL {cmd:#010x} {}", ctrl.name),
                    format!("{size} parameter bytes; NVIDIA's UMDs send {want}"),
                );
            }
            Verdict::Allow
        }
        ESC_RM_ALLOC => {
            let Some(class) = word(data, 12) else {
                return short();
            };
            if creates_client {
                if owned.clients() >= MAX_CLIENTS {
                    return deny(
                        "RM_ALLOC of a client",
                        format!("this guest already holds {MAX_CLIENTS} RM clients"),
                    );
                }
                return Verdict::Allow;
            }
            if class_name(class).is_none() {
                return deny(
                    format!("RM_ALLOC class {class:#06x}"),
                    "not in the NVIDIA UMD personality's class list",
                );
            }
            if nested.len() > MAX_ALLOC_PARAMS {
                return deny(
                    format!("RM_ALLOC class {class:#06x}"),
                    format!(
                        "{} parameter bytes, more than {MAX_ALLOC_PARAMS}",
                        nested.len()
                    ),
                );
            }
            Verdict::Allow
        }
        ESC_RM_ALLOC_MEMORY => {
            // NVOS02: hRoot, hObjectParent, hObjectNew, hClass at 12.
            let Some(class) = word(data, 12) else {
                return short();
            };
            if class != OS_DESCRIPTOR {
                return deny(
                    format!("RM_ALLOC_MEMORY class {class:#06x}"),
                    "only NV01_MEMORY_SYSTEM_OS_DESCRIPTOR is allocated this way",
                );
            }
            Verdict::Allow
        }
        ESC_RM_IDLE_CHANNELS => {
            // NVOS30: numChannels at 12, the three list pointers at 16, 24, 32.
            let (Some(n), Some(a), Some(b), Some(c)) = (
                word(data, 12),
                quad(data, 16),
                quad(data, 24),
                quad(data, 32),
            ) else {
                return short();
            };
            if n != 0 || a != 0 || b != 0 || c != 0 {
                return deny(
                    "NV_ESC_RM_IDLE_CHANNELS with a channel list",
                    format!("{n} channel(s) named through pointers RM would read here"),
                );
            }
            Verdict::Allow
        }
        ESC_RM_MAP_MEMORY => {
            // NVOS33: hClient, hDevice, hMemory at 8.
            let (Some(client), Some(memory)) = (word(data, 0), word(data, 8)) else {
                return short();
            };
            match owned.class_of(client, memory) {
                None => deny(
                    "NV_ESC_RM_MAP_MEMORY of an unknown object",
                    format!("object {memory:#010x} was not allocated by this guest"),
                ),
                Some(class)
                    if ROOT_CLASSES.contains(&class) || REGISTER_CLASSES.contains(&class) =>
                {
                    deny(
                        format!("NV_ESC_RM_MAP_MEMORY of class {class:#06x}"),
                        "a client, device or subdevice maps GPU registers, not memory",
                    )
                }
                Some(_) => Verdict::Allow,
            }
        }
        _ => Verdict::Allow,
    }
}

/// `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD`: makes an object in the
/// caller's client from another file's export.
const CTRL_IMPORT_OBJECT_FROM_FD: u32 = 0x3d06;
/// `object.data.rmObject.hObject` in its parameters (`fd`, `object.type`,
/// then `hDevice`, `hParent`, `hObject`).
const IMPORT_HOBJECT_AT: usize = 16;
/// The class recorded for an object this guest imported: RM knows it, the
/// request did not say. Treated as memory.
pub const IMPORTED: u32 = u32::MAX;

/// Read RM's answer to a request this guest sent and keep the books.
/// `data` is the top-level struct as RM returned it, `nested` the parameter
/// block after it.
pub fn observe(owned: &mut Owned, file: u64, nr: u32, request: &[u8], data: &[u8], nested: &[u8]) {
    let ok = |at: Option<usize>| at.and_then(|a| word(data, a)) == Some(0);
    match nr {
        ESC_RM_CONTROL
            if ok(status_at(nr, request.len()))
                && word(data, 8) == Some(CTRL_IMPORT_OBJECT_FROM_FD) =>
        {
            if let (Some(client), Some(h)) = (word(data, 0), word(nested, IMPORT_HOBJECT_AT)) {
                owned.allocated(file, client, h, IMPORTED);
            }
        }
        ESC_RM_ALLOC if ok(status_at(nr, request.len())) => {
            if let (Some(client), Some(new), Some(class)) =
                (word(data, 0), word(data, 8), word(data, 12))
            {
                owned.allocated(file, client, new, class);
            }
        }
        ESC_RM_ALLOC_MEMORY if ok(status_at(nr, request.len())) => {
            if let (Some(client), Some(new), Some(class)) =
                (word(data, 0), word(data, 8), word(data, 12))
            {
                owned.allocated(file, client, new, class);
            }
        }
        ESC_RM_FREE if ok(status_at(nr, request.len())) => {
            if let (Some(client), Some(h)) = (word(data, 0), word(data, 8)) {
                owned.freed(client, h);
            }
        }
        _ => {}
    }
}

/// Distinct requests [`Misses`] keeps a count for. A guest can name any of
/// 2^32 controls or classes; past this many, the rest share one count.
pub const MAX_MISS_KEYS: usize = 1024;
/// The count the requests past [`MAX_MISS_KEYS`] share.
const OTHER: &str = "(further requests)";

/// Counts and rate-limited log lines for what the policy did not allow.
#[derive(Default)]
pub struct Misses {
    seen: std::collections::BTreeMap<String, u64>,
}

impl Misses {
    /// Count one; true when this one should be logged (the 1st, 10th, 100th...).
    pub fn note(&mut self, what: &str) -> (u64, bool) {
        let key = if self.seen.contains_key(what) || self.seen.len() < MAX_MISS_KEYS {
            what
        } else {
            OTHER
        };
        let n = self.seen.entry(key.to_string()).or_insert(0);
        *n += 1;
        (*n, power_of_ten(*n))
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    pub fn summary(&self) -> String {
        self.seen
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn power_of_ten(mut n: u64) -> bool {
    if n == 0 {
        return false;
    }
    while n.is_multiple_of(10) {
        n /= 10;
    }
    n == 1
}

// ---------------------------------------------------------------------------
// The backend's side: where the rules above are applied.
// ---------------------------------------------------------------------------

use super::Status;
use protocol::messages::{IoctlReq, IoctlResp, MsgHeader};

impl super::NvidiaBackend {
    /// Refuse an RM request: RM's status word, when the escape has one, says
    /// `NV_ERR_INSUFFICIENT_PERMISSIONS` and the ioctl succeeds (NVIDIA's
    /// userspace reads a status; an errno makes it retry or hang). Otherwise
    /// the ioctl fails with `EPERM`.
    fn refuse_rm(
        &self,
        cookie: u64,
        nr: u32,
        data_len: usize,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let mut out = param_in.to_vec();
        match status_at(nr, data_len).filter(|at| at + 4 <= data_len.min(out.len())) {
            Some(at) => {
                out[at..at + 4].copy_from_slice(&NV_ERR_INSUFFICIENT_PERMISSIONS.to_le_bytes());
                self.write_ioctl_resp(resp_buf, cookie, &out)
            }
            None => self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM),
        }
    }

    /// Count a miss and log it at the 1st, 10th, 100th... time.
    fn note_rm_miss(&mut self, what: &str, why: &str) {
        let (n, log) = self.rm_misses.note(what);
        if log {
            let mode = self.start.rm_policy;
            let verb = if mode == Mode::Enforce {
                "refused"
            } else {
                "would refuse"
            };
            log::warn!(
                "RM policy ({}): {verb} {what}: {why} [{n} so far; {ENV}=log|enforce|off]",
                mode.as_str()
            );
        }
    }

    /// The UMD release lock and the RM policy, for one `'F'` ioctl, before
    /// it is served. `Some` is the response when it is answered here.
    pub(super) fn rm_policy_gate(
        &mut self,
        cookie: u64,
        nr: u32,
        ireq: &IoctlReq,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> Option<usize> {
        let data_len = (ireq.data_len as usize).min(param_in.len());
        // A guest whose UMDs are not the host RM's release is not served RM
        // at all; it may still ask the version, which is how it finds out.
        if self.umd_report.is_some_and(|r| r.refuses()) && nr != ESC_CHECK_VERSION_STR {
            traced_refusal!(self, Allowlist);
            return Some(self.refuse_rm(cookie, nr, data_len, param_in, resp_buf));
        }
        let mode = self.start.rm_policy;
        if mode == Mode::Off {
            return None;
        }
        let (data, nested) = param_in.split_at(data_len);
        match judge(&self.rm_owned, nr, data, nested) {
            Verdict::Allow => None,
            Verdict::Absorb(ctrl) => {
                if mode != Mode::Enforce {
                    self.note_rm_miss(
                        &format!("RM_CONTROL {:#010x} {}", ctrl.cmd, ctrl.name),
                        "acts on the host GPU as a whole; enforce answers it NV_OK here",
                    );
                    return None;
                }
                self.rm_absorbed += 1;
                traced_refusal!(self, Local);
                let mut out = param_in.to_vec();
                // NVOS54.status, at 28.
                out.get_mut(28..32)?.copy_from_slice(&0u32.to_le_bytes());
                Some(self.write_ioctl_resp(resp_buf, cookie, &out))
            }
            Verdict::Deny { what, why } => {
                self.note_rm_miss(&what, &why);
                if mode != Mode::Enforce {
                    return None;
                }
                traced_refusal!(self, Allowlist);
                Some(self.refuse_rm(cookie, nr, data_len, param_in, resp_buf))
            }
        }
    }

    /// A UVM ioctl, under the policy: the NVIDIA UMD personality never opens
    /// UVM (Windows CUDA does not use it).
    pub(super) fn rm_policy_uvm(
        &mut self,
        cookie: u64,
        request: u64,
        resp_buf: &mut [u8],
    ) -> Option<usize> {
        let mode = self.start.rm_policy;
        if mode == Mode::Off {
            return None;
        }
        self.note_rm_miss(
            &format!("UVM ioctl {request:#x}"),
            "the NVIDIA UMD personality does not use UVM",
        );
        (mode == Mode::Enforce)
            .then(|| self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM))
    }

    /// After an `'F'` ioctl was served: keep the books of what RM made and
    /// freed for this guest.
    pub(super) fn rm_policy_note(&mut self, payload: &[u8], resp: &[u8]) {
        let watch = self.start.rm_policy != Mode::Off || self.start.nvidia_umd;
        if !watch || payload.len() < size_of::<IoctlReq>() {
            return;
        }
        let req = super::read_struct::<IoctlReq>(payload, 0);
        let request = req.cmd as u64;
        if (request >> 8) & 0xFF != b'F' as u64 {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head || super::read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let nr = (request & 0xFF) as u32;
        let data_len = req.data_len as usize;
        let body = &payload[size_of::<IoctlReq>()..];
        let (Some(sent), Some(answered)) = (body.get(..data_len), resp[head..].get(..data_len))
        else {
            return;
        };
        let nested = resp[head + data_len..]
            .get(..req.nested_len as usize)
            .unwrap_or(&[]);
        let before = self.rm_owned.clients();
        observe(
            &mut self.rm_owned,
            self.current_handle as u64,
            nr,
            sent,
            answered,
            nested,
        );
        if self.start.nvidia_umd
            && self.rm_owned.clients() > before
            && self.umd_report.is_none()
            && !self.umd_unreported_logged
        {
            self.umd_unreported_logged = true;
            log::warn!(
                "NVIDIA UMD release check: the guest made an RM client without reporting its \
                 UMD release (a guest KMD without the release check); the UMD/RM release \
                 match is not checked"
            );
        }
    }

    /// The backend file `file` closed: RM freed the clients made on it.
    pub(super) fn rm_policy_file_closed(&mut self, file: u64) {
        self.rm_owned.file_closed(file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: u64 = 7;
    const CLIENT: u32 = 0xc1d0_0042;

    fn put(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn with_client() -> Owned {
        let mut o = Owned::default();
        o.allocated(FILE, 0, CLIENT, 0x41);
        o
    }

    fn control_req(client: u32, cmd: u32, size: u32) -> [u8; 32] {
        let mut d = [0u8; 32];
        put(&mut d, 0, client);
        put(&mut d, 4, client);
        put(&mut d, 8, cmd);
        put(&mut d, 24, size);
        d
    }

    fn alloc_req(client: u32, parent: u32, new: u32, class: u32) -> [u8; 48] {
        let mut d = [0u8; 48];
        put(&mut d, 0, client);
        put(&mut d, 4, parent);
        put(&mut d, 8, new);
        put(&mut d, 12, class);
        d
    }

    fn denied(v: &Verdict) -> bool {
        matches!(v, Verdict::Deny { .. })
    }

    #[test]
    fn the_tables_are_sorted_and_unique() {
        assert!(CONTROLS.windows(2).all(|w| w[0].cmd < w[1].cmd));
        assert!(CLASSES.windows(2).all(|w| w[0].0 < w[1].0));
        for a in ABSORBED {
            assert!(control(a.cmd).is_none(), "{:#x} is both", a.cmd);
        }
        // Only the legacy GSP range carries a pinned size.
        for c in CONTROLS {
            assert_eq!(c.params.is_some(), c.cmd & 0xc000 == 0x8000, "{:#x}", c.cmd);
        }
    }

    #[test]
    fn the_mode_comes_from_the_environment_or_the_personality() {
        assert_eq!(Mode::from_env_value(None, true), Ok(Mode::Log));
        assert_eq!(Mode::from_env_value(None, false), Ok(Mode::Off));
        assert_eq!(
            Mode::from_env_value(Some("enforce"), false),
            Ok(Mode::Enforce)
        );
        assert_eq!(Mode::from_env_value(Some(" Log "), false), Ok(Mode::Log));
        assert_eq!(Mode::from_env_value(Some("off"), true), Ok(Mode::Off));
        assert_eq!(Mode::from_env_value(Some("bogus"), true), Err(Mode::Log));
    }

    #[test]
    fn a_listed_control_on_an_own_client_is_served() {
        let o = with_client();
        let d = control_req(CLIENT, 0x2080_0102, 580);
        assert_eq!(judge(&o, ESC_RM_CONTROL, &d, &[0; 580]), Verdict::Allow);
    }

    #[test]
    fn an_unlisted_control_is_refused_by_name() {
        let o = with_client();
        // NV2080_CTRL_CMD_PERF_BOOST: RM serves it to an unprivileged caller.
        let d = control_req(CLIENT, 0x2080_200a, 8);
        let Verdict::Deny { what, .. } = judge(&o, ESC_RM_CONTROL, &d, &[]) else {
            panic!("served");
        };
        assert_eq!(what, "RM_CONTROL 0x2080200a");
    }

    #[test]
    fn a_control_on_another_client_is_refused() {
        let o = with_client();
        let d = control_req(0xc1d0_0001, 0x2080_0102, 580);
        assert!(denied(&judge(&o, ESC_RM_CONTROL, &d, &[])));
    }

    #[test]
    fn the_legacy_gsp_controls_have_one_size() {
        let o = with_client();
        assert_eq!(
            judge(
                &o,
                ESC_RM_CONTROL,
                &control_req(CLIENT, 0x0080_9908, 16),
                &[]
            ),
            Verdict::Allow
        );
        assert!(denied(&judge(
            &o,
            ESC_RM_CONTROL,
            &control_req(CLIENT, 0x0080_9908, 4096),
            &[]
        )));
    }

    #[test]
    fn an_oversized_control_is_refused() {
        let o = with_client();
        let d = control_req(CLIENT, 0x2080_0102, MAX_CONTROL_PARAMS + 1);
        assert!(denied(&judge(&o, ESC_RM_CONTROL, &d, &[])));
    }

    #[test]
    fn the_cuda_clock_limit_is_absorbed() {
        let o = with_client();
        let d = control_req(CLIENT, 0x0080_1909, 1);
        assert!(matches!(
            judge(&o, ESC_RM_CONTROL, &d, &[1]),
            Verdict::Absorb(_)
        ));
    }

    #[test]
    fn a_client_can_be_made_and_is_then_owned() {
        let mut o = Owned::default();
        let req = alloc_req(0, 0, 0, 0x41);
        assert_eq!(judge(&o, ESC_RM_ALLOC, &req, &[]), Verdict::Allow);
        let mut reply = req;
        put(&mut reply, 8, CLIENT);
        observe(&mut o, FILE, ESC_RM_ALLOC, &req, &reply, &[]);
        assert!(o.client(CLIENT));
        // A failed allocation records nothing.
        let mut failed = alloc_req(CLIENT, CLIENT, 0x10, 0x80);
        put(&mut failed, 40, 0x22);
        observe(&mut o, FILE, ESC_RM_ALLOC, &failed, &failed, &[]);
        assert_eq!(o.class_of(CLIENT, 0x10), None);
    }

    #[test]
    fn classes_outside_the_list_are_refused() {
        let o = with_client();
        // NV01_MEMORY_LOCAL_PHYSICAL (0xc2) and GT200_DEBUGGER (0x83de).
        for class in [0xc2, 0x83de] {
            let d = alloc_req(CLIENT, CLIENT, 0x10, class);
            assert!(denied(&judge(&o, ESC_RM_ALLOC, &d, &[])), "{class:#x}");
        }
        let d = alloc_req(CLIENT, CLIENT, 0x10, 0x80);
        assert_eq!(judge(&o, ESC_RM_ALLOC, &d, &[0; 56]), Verdict::Allow);
        assert!(denied(&judge(
            &o,
            ESC_RM_ALLOC,
            &d,
            &vec![0; MAX_ALLOC_PARAMS + 1]
        )));
    }

    #[test]
    fn the_number_of_clients_is_bounded() {
        let mut o = Owned::default();
        for i in 0..MAX_CLIENTS as u32 {
            o.allocated(FILE, 0, 0xc1d0_0000 + i, 0x41);
        }
        assert!(denied(&judge(
            &o,
            ESC_RM_ALLOC,
            &alloc_req(0, 0, 0, 0x41),
            &[]
        )));
    }

    #[test]
    fn memory_by_address_is_only_an_os_descriptor() {
        let o = with_client();
        let mut d = [0u8; 56];
        put(&mut d, 0, CLIENT);
        put(&mut d, 12, OS_DESCRIPTOR);
        assert_eq!(judge(&o, ESC_RM_ALLOC_MEMORY, &d, &[]), Verdict::Allow);
        put(&mut d, 12, 0x3e);
        assert!(denied(&judge(&o, ESC_RM_ALLOC_MEMORY, &d, &[])));
    }

    #[test]
    fn idle_channels_may_not_carry_a_list() {
        let o = with_client();
        let mut d = [0u8; 56];
        put(&mut d, 0, CLIENT);
        assert_eq!(judge(&o, ESC_RM_IDLE_CHANNELS, &d, &[]), Verdict::Allow);
        put(&mut d, 12, 1);
        d[16..24].copy_from_slice(&0x7fff_0000_1000u64.to_le_bytes());
        assert!(denied(&judge(&o, ESC_RM_IDLE_CHANNELS, &d, &[])));
    }

    #[test]
    fn a_mapping_must_name_owned_memory() {
        let mut o = with_client();
        o.allocated(FILE, CLIENT, 0x10, 0x80);
        o.allocated(FILE, CLIENT, 0x20, 0x2080);
        o.allocated(FILE, CLIENT, 0x30, 0x40);
        let map = |memory: u32| {
            let mut d = [0u8; 56];
            put(&mut d, 0, CLIENT);
            put(&mut d, 4, 0x10);
            put(&mut d, 8, memory);
            d
        };
        assert_eq!(
            judge(&o, ESC_RM_MAP_MEMORY, &map(0x30), &[]),
            Verdict::Allow
        );
        // The subdevice, the device and the client map registers.
        for h in [0x20, 0x10, CLIENT] {
            assert!(
                denied(&judge(&o, ESC_RM_MAP_MEMORY, &map(h), &[])),
                "{h:#x}"
            );
        }
        // Never allocated.
        assert!(denied(&judge(&o, ESC_RM_MAP_MEMORY, &map(0x99), &[])));
    }

    #[test]
    fn freeing_and_closing_forget_what_rm_freed() {
        let mut o = with_client();
        o.allocated(FILE, CLIENT, 0x30, 0x40);
        let mut free = [0u8; 16];
        put(&mut free, 0, CLIENT);
        put(&mut free, 4, CLIENT);
        put(&mut free, 8, 0x30);
        observe(&mut o, FILE, ESC_RM_FREE, &free, &free, &[]);
        assert_eq!(o.class_of(CLIENT, 0x30), None);
        assert!(o.client(CLIENT));
        o.file_closed(FILE + 1);
        assert!(o.client(CLIENT));
        o.file_closed(FILE);
        assert!(!o.client(CLIENT));
    }

    #[test]
    fn an_imported_object_can_be_mapped() {
        let mut o = with_client();
        let req = control_req(CLIENT, CTRL_IMPORT_OBJECT_FROM_FD, 20);
        let mut params = [0u8; 20];
        put(&mut params, IMPORT_HOBJECT_AT, 0x77);
        observe(&mut o, FILE, ESC_RM_CONTROL, &req, &req, &params);
        assert_eq!(o.class_of(CLIENT, 0x77), Some(IMPORTED));
        let mut map = [0u8; 56];
        put(&mut map, 0, CLIENT);
        put(&mut map, 8, 0x77);
        assert_eq!(judge(&o, ESC_RM_MAP_MEMORY, &map, &[]), Verdict::Allow);
    }

    #[test]
    fn escapes_outside_the_list_are_refused() {
        let o = with_client();
        // NV_ESC_RM_DUP_OBJECT, NV_ESC_RM_SHARE, NV_ESC_RM_VID_HEAP_CONTROL,
        // NV_ESC_RM_ACCESS_REGISTRY, NV_ESC_RM_I2C_ACCESS.
        for nr in [0x34, 0x35, 0x4a, 0x4d, 0x39] {
            assert!(denied(&judge(&o, nr, &[0; 64], &[])), "{nr:#x}");
        }
        for nr in [ESC_CARD_INFO, ESC_CHECK_VERSION_STR, ESC_REGISTER_FD] {
            assert_eq!(judge(&o, nr, &[0; 72], &[]), Verdict::Allow);
        }
    }

    #[test]
    fn the_kmd_completion_events_are_served_on_its_own_client_only() {
        let o = with_client();
        let mut ev = [0u8; 16];
        ev[0..4].copy_from_slice(&CLIENT.to_le_bytes());
        for nr in [ESC_ALLOC_OS_EVENT, ESC_FREE_OS_EVENT] {
            assert_eq!(judge(&o, nr, &ev, &[]), Verdict::Allow, "{nr:#x}");
            let mut foreign = ev;
            foreign[0..4].copy_from_slice(&0xc1d0_0bad_u32.to_le_bytes());
            assert!(denied(&judge(&o, nr, &foreign, &[])), "{nr:#x}");
            assert_eq!(status_at(nr, 16), Some(12));
        }
        assert_eq!(class_name(0x79), Some("NV01_EVENT_OS_EVENT"));
        assert!(control(0x2080_0301).is_some());
    }

    #[test]
    fn misses_are_logged_at_powers_of_ten() {
        let mut m = Misses::default();
        let logged: Vec<u64> = (1..=1000)
            .filter_map(|_| {
                let (n, log) = m.note("x");
                log.then_some(n)
            })
            .collect();
        assert_eq!(logged, vec![1, 10, 100, 1000]);
        assert_eq!(m.summary(), "x=1000");
    }

    #[test]
    fn misses_are_bounded_however_many_requests_a_guest_names() {
        let mut m = Misses::default();
        for i in 0..(MAX_MISS_KEYS as u32 + 500) {
            m.note(&format!("RM_CONTROL {i:#x}"));
        }
        assert_eq!(m.seen.len(), MAX_MISS_KEYS + 1);
        assert_eq!(m.seen.get(OTHER), Some(&500));
    }

    #[test]
    fn status_words_are_where_rm_writes_them() {
        assert_eq!(status_at(ESC_RM_ALLOC, 48), Some(40));
        assert_eq!(status_at(ESC_RM_ALLOC, 32), Some(28));
        assert_eq!(status_at(ESC_RM_CONTROL, 32), Some(28));
        assert_eq!(status_at(ESC_CARD_INFO, 0), None);
    }
}

//! The backend: one guest's view of the host NVIDIA driver.
//!
//! `mod.rs` holds the state and the message dispatch. Each kind of message,
//! and each shape of ioctl, is served from its own file.
use protocol::messages::*;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use crate::error::{DeviceError, Result};
use crate::handle_table::HandleTable;
use crate::shm::{ShmAllocator, ZoneConfig};

/// Note why the request being served is answered here, for the trace.
///
/// Used on refusal paths, never on the forwarding path. Expands to nothing
/// when the `trace` feature is off.
macro_rules! traced_refusal {
    ($self:expr, $why:ident) => {
        #[cfg(feature = "trace")]
        $self.trace_refusal.set(conduit_trace::Refusal::$why);
    };
}

// ============================================================
// Device path helpers
// ============================================================

const MAX_GPU: u8 = 8;

/// Field offsets in NVOS54_PARAMETERS, the struct RM_CONTROL carries.
///
/// `status` is the one that matters and the one that is easy to miss: it is
/// written by RM on the way out and is independent of the ioctl return value.
/// The floor a second-level buffer is sized to, whatever length the guest
/// derived for it. See where it is used: the length is read at a table-supplied
/// offset, the table was generated from a different driver release, and the
/// cost of it being wrong must not be heap corruption in this process.
const DEEP_BUF_FLOOR: usize = 64 * 1024;

/// `NVOS64_PARAMETERS.status`, where RM writes its answer to an allocation.
const NVOS64_STATUS: usize = 40;
/// RM_ALLOC classes that make a client rather than an object in one.
const ROOT_CLASSES: [u32; 3] = [0x0, 0x1, 0x41];

const NVOS54_CMD: usize = 8;
const NVOS54_PARAMS_SIZE: usize = 24;
const NVOS54_STATUS: usize = 28;
const NVOS54_TOTAL: usize = 32;

/// `NV_OK`. Every other value is a refusal of some kind.
const NV_OK: u32 = 0;

/// The host path an `Open` refers to.
///
/// The wire encoding is one flat `u32`: a GPU is its own minor number and the
/// singleton devices take values above every possible minor.
///
/// A render node's name is not derivable from its index: the host numbers them
/// per DRM device, so the guest's index has to be looked up in the same list
/// the guest was given.
fn device_path_with(device_type: u32, dri: &[DriDevice]) -> Result<CString> {
    let kind = DeviceKind::from_device_type(device_type)
        .ok_or(DeviceError::InvalidDeviceKind(device_type))?;
    let path = match kind {
        DeviceKind::Gpu(n) => {
            if n >= MAX_GPU as u32 {
                return Err(DeviceError::GpuIndexOutOfRange(n));
            }
            format!("/dev/nvidia{n}")
        }
        DeviceKind::Ctl => "/dev/nvidiactl".to_string(),
        DeviceKind::Uvm => "/dev/nvidia-uvm".to_string(),
        DeviceKind::UvmTools => "/dev/nvidia-uvm-tools".to_string(),
        DeviceKind::Modeset => "/dev/nvidia-modeset".to_string(),
        DeviceKind::Dri(n) => {
            let d = dri
                .get(n as usize)
                .ok_or(DeviceError::InvalidDeviceKind(device_type))?;
            format!("/dev/dri/{}", d.name)
        }
    };
    Ok(CString::new(path).expect("a device path has no interior NUL"))
}

/// Backend-side result codes, mapped to the errno the guest driver sees.
///
/// The driver has no status vocabulary of its own: it tests `(s32)status < 0`
/// and returns that value from the syscall, so every one of these has to become
/// a plausible errno or userspace gets a nonsense failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    InvalidMsgType,
    InvalidDevice,
    OpenFailed,
    BadHandle,
    IoctlFailed,
    BufferTooSmall,
}

impl Status {
    fn errno(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::InvalidMsgType => libc::EPROTO,
            Self::InvalidDevice => libc::ENODEV,
            Self::OpenFailed => libc::EIO,
            Self::BadHandle => libc::EBADF,
            Self::IoctlFailed => libc::EIO,
            Self::BufferTooSmall => libc::ENOSPC,
        }
    }
}

/// A DRM render node the host owns, as the guest is told about it.
#[derive(Clone)]
struct DriDevice {
    name: String,
    major: u32,
    minor: u32,
    /// Which GPU slot it belongs to. The guest matches this against the GPU's
    /// minor to decide which card the node hangs off; it is ours, not NVIDIA's.
    slot_index: u32,
    /// `DRM_NVIDIA_GET_DEV_INFO` as the host's own node answers it, passed
    /// through rather than reconstructed.
    ///
    /// The guest used to answer this ioctl from constants -- gpu_id from the
    /// slot index, and page kind 6 / generation 2 / sector layout 1 under a
    /// comment reading "Turing/Ampere". The gpu_id was simply wrong: the ICD
    /// matches its RM device to a DRM node by it, the host answers 0x100 for a
    /// card at 0000:01:00.0 and the guest answered 0, so no node was ever
    /// matched and VkPhysicalDeviceDrmPropertiesEXT reported hasRender =
    /// false. The tiling fields were right for the two cards they name and
    /// silently wrong elsewhere, which is the kind of wrong that produces a
    /// scrambled frame rather than an error.
    dev_info: [u32; NV_DEV_INFO_WORDS],
}

/// `struct drm_nvidia_get_dev_info_params` is nine `u32`s. Carried as words
/// because nothing here needs to interpret them -- only the guest does.
const NV_DEV_INFO_WORDS: usize = 9;

/// `_IOWR('d', DRM_COMMAND_BASE + DRM_NVIDIA_GET_DEV_INFO, params)`, i.e.
/// direction read|write, 36 bytes, type 'd', nr 0x43.
// `libc::Ioctl` is `c_ulong` on glibc and `c_int` on musl; the bit pattern is
// what the kernel reads either way.
const DRM_IOCTL_NVIDIA_GET_DEV_INFO: libc::Ioctl = 0xC024_6443_u32 as libc::Ioctl;

/// Ask a host render node what it is.
///
/// `None` when the node cannot be opened or refuses the ioctl, which leaves
/// the guest on its own constants -- wrong, but no worse than before, and
/// said out loud rather than discovered later in a frame.
fn host_dev_info(path: &str) -> Option<[u32; NV_DEV_INFO_WORDS]> {
    let c_path = CString::new(path).ok()?;
    // SAFETY: a NUL-terminated path, and the fd is closed below.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        log::warn!(
            "{path}: cannot open to ask what it is ({})",
            std::io::Error::last_os_error()
        );
        return None;
    }
    let mut params = [0u32; NV_DEV_INFO_WORDS];
    // SAFETY: `params` is exactly the 36 bytes the ioctl's size field declares.
    let rc = unsafe {
        libc::ioctl(
            fd,
            DRM_IOCTL_NVIDIA_GET_DEV_INFO,
            params.as_mut_ptr() as *mut libc::c_void,
        )
    };
    let err = std::io::Error::last_os_error();
    // SAFETY: fd came from open() above and is not used again.
    unsafe { libc::close(fd) };
    if rc != 0 {
        log::warn!("{path}: GET_DEV_INFO refused ({err})");
        return None;
    }
    Some(params)
}

// ============================================================
// NvidiaBackend
// ============================================================

pub struct NvidiaBackend {
    /// The message being served, so a response can echo its type, and the
    /// handle it named, so handlers need not thread either through.
    current_msg: MsgType,
    current_handle: u32,
    /// Top-level parameter length of the ioctl being served. The response has
    /// to split the bytes the same way the request did.
    current_data_len: u32,
    handles: HandleTable,
    shm: ShmAllocator,
    /// Active RM_MAP_MEMORY mappings, keyed by SHM offset.
    ///
    /// The SHM offset is written into pLinearAddress in the response to the
    /// guest, so userspace echoes it back as pLinearAddress in RM_UNMAP_MEMORY.
    /// This gives us a unique, unambiguous lookup key without leaking host VAs.
    active_maps: crate::mmap::MmapContext,
    /// Which device each open handle names, for mappings made without one.
    handle_kinds: std::collections::HashMap<u64, DeviceKind>,
    /// Host driver version, learned from the first successful
    /// `NV_ESC_CHECK_VERSION_STR`.
    driver: Option<abi::version::DriverVersion>,
    /// ABI profile selected for `driver`, if one exists.
    abi: Option<&'static [abi::versions::IoctlEntry]>,
    /// The RM controls whose parameters carry a pointer RM dereferences, for
    /// `driver`, and whether the table is this release's own. `None` until the
    /// release is known; the backend refuses to start without one.
    rmctrl: Option<abi::rmctrl::Selected>,
    /// The controls and classes RM exports to an unprivileged caller, for
    /// `driver`, and whether the tables are this release's own. `None` until
    /// the release is known; the backend refuses to start without them.
    ///
    /// RM applies this rule to the *backend*, which is a service account on
    /// the host, not to the guest. So the backend applies it on the guest's
    /// behalf, before forwarding. See `abi::rmallow`.
    rmallow: Option<abi::rmallow::Selected>,
    /// The UVM commands the host release defines, their sizes, and where the
    /// descriptors sit in them. `None` until the release is known, and until
    /// then nothing says what a UVM call even is, so none is served.
    uvm: Option<abi::uvm::Selected>,
    /// Where the host release keeps the CPU address on each of the three
    /// routes that let a caller name memory by one. `None` until the release
    /// is known, and until then those routes are not recognised -- which is
    /// why nothing is served before a release is known at all.
    osdesc: Option<abi::osdesc::OsDesc>,
    /// Where an allocation's size is and what tells the guest how much video
    /// memory there is, for this release. See `vidmem.rs`.
    vidmem: Option<abi::vidmem::Selected>,
    /// NVOS32 FREE calls seen. Not charged back, because the table has no
    /// layout for them; reported so a workload that uses them is noticed.
    vidmem_untracked_frees: u64,
    /// Guest RAM, as something to map from. `None` until the transport
    /// supplies it, and without it a registration by address has nowhere to
    /// find the guest's pages and is refused.
    guest_ram: Option<Box<dyn crate::guestmem::GuestRam>>,
    /// How many registrations by address were served, and how many bytes they
    /// covered at their peak. Reported at teardown beside whatever is left
    /// over, which is the number that must be zero.
    registrations_served: u64,
    registrations_peak: usize,
    /// Guest memory registered with RM by address, by the client and object
    /// handle that own it. Holding the [`Stitched`] span is what keeps the
    /// host mapping alive: RM pinned those pages for the life of the object,
    /// so unmapping them earlier would leave RM pointing at nothing.
    ///
    /// Keyed by the RM handles and not by the file, because the two need not
    /// be the same file: `NV_ESC_RM_ALLOC_MEMORY` is `NV_ACTUAL_DEVICE_ONLY`
    /// and so arrives on `/dev/nvidia0`, while the `NV_ESC_RM_FREE` that ends
    /// it goes to the control file. A client handle is unique within RM, so
    /// the pair names the object on its own. The file is kept beside the span
    /// only so that closing it releases what it registered.
    ///
    /// [`Stitched`]: crate::guestmem::Stitched
    registrations: std::collections::HashMap<(u32, u32), (u64, crate::guestmem::Stitched)>,
    /// UVM files whose VA space was found to allow pageable access on a
    /// release with no flag to forbid it. The host file is initialised by the
    /// time the answer comes back, so the refusal attaches to the handle.
    uvm_denied: std::collections::HashSet<u64>,
    /// Files with an OS event allocated on them (`NV_ESC_ALLOC_OS_EVENT`
    /// that RM accepted), by handle, each with the (client, fd) pairs RM
    /// holds. `NV_ESC_RM_GET_EVENT_DATA` is served only on these. See
    /// `os_event.rs`.
    os_events: std::collections::HashMap<u64, std::collections::HashSet<(u32, u32)>>,
    /// Controls and classes refused by the RM allowlist, by what was asked
    /// for and why. Reported at teardown.
    ///
    /// Separate from `caps_refused`: a capability being off is a decision
    /// somebody made on the command line, and this is RM's own rule. A probe
    /// that regresses needs to say which of the two stopped it.
    allow_refused: std::collections::BTreeMap<String, u64>,
    /// Where device memory is placed so the guest can address it. `None` until
    /// the transport supplies one, and without it a mapping can be made on the
    /// host but never reached from the guest.
    window: Option<Box<dyn crate::shm::WindowPlacer>>,
    /// Window placements made for a DRM object, keyed by the node handle and
    /// the object's mmap offset on the host.
    ///
    /// Keyed by both because one open of a node holds many objects, and they
    /// are told apart only by that offset. Keyed at all because a buffer is
    /// mapped more than once -- the guest maps it, exports it, an importer maps
    /// it again -- and each placement costs a slice of a finite window.
    dri_maps: std::collections::HashMap<(u64, u64), u32>,
    /// The render nodes the guest was told of, by the index it opens them by.
    /// `None` until GET_SYS_FILES has answered.
    dri_given: std::cell::RefCell<Option<Vec<DriDevice>>>,
    /// UVM semaphore pools placed in the aperture. See `aperture.rs`.
    aperture: aperture::Aperture,
    /// Every message this backend has served, by kind.
    ///
    /// Kept because "how often does the guest have to ask the host anything"
    /// is the question a benchmark of this design turns on, and counting log
    /// lines answers a different one -- what the log level happened to print.
    msg_counts: std::collections::BTreeMap<&'static str, u64>,
    /// Every live placement, by the id the guest quotes to take it back.
    live_maps: std::collections::HashMap<u32, LiveMap>,
    /// Guarded buffers for parameter blocks, reused across calls.
    guards: std::cell::RefCell<crate::guarded::GuardPool>,
    /// What this guest is served. See `crate::caps`.
    caps: crate::caps::Caps,
    /// Opens and allocations refused because their capability is off, by
    /// what was asked for. Reported at teardown.
    caps_refused: std::collections::BTreeMap<String, u64>,
    /// Every `RM_ALLOC` class and `RM_CONTROL` command a workload asked for,
    /// and how often.
    ///
    /// Narrowing these to what the pipeline uses is the step that actually
    /// reduces what a guest can reach in the host driver -- an unprivileged
    /// helper process contains a bug in *this* code, not one in NVIDIA's kernel
    /// module, and only fewer reachable commands helps with the second. A
    /// filter cannot be written from a guess, so this is the instrument that
    /// says what the set really is.
    rm_classes: std::collections::BTreeMap<u32, u64>,
    rm_controls: std::collections::BTreeMap<u32, u64>,
    /// Every ioctl forwarded, by namespace and number.
    ///
    /// There are three namespaces, not one, and that is the point of counting
    /// this way: NVIDIA's own escapes (`F`), the DRM node's (`d`) and
    /// modeset's (`m`). Only the first has an ABI table. A buffer-sharing run
    /// measured 1212 forwarded ioctls with *zero* RM allocations or controls
    /// among them -- so an allowlist written against RM alone would leave the
    /// path a compositor actually uses completely unfiltered.
    ioctls_by_ns: std::collections::BTreeMap<(char, u32), u64>,
    /// Escapes that failed the check, and how often, so a run can say what a
    /// workload actually needed. Reported at teardown.
    abi_refused: std::collections::BTreeMap<u32, u64>,
    /// Descriptors opened and closed since a transport last asked, so it can
    /// keep a poll set in step with them.
    ///
    /// The backend cannot watch them itself: it holds no queue to deliver a
    /// notification on, and this crate names no VMM. It says what is
    /// watchable; the transport decides how to watch it.
    watch_added: Vec<(u32, RawFd)>,
    watch_removed: Vec<u32>,
    /// Host fences (sync_files) handed to the guest as handles, docs/SYNC.md.
    /// Watched like a descriptor but reported once: a signalled sync_file
    /// stays readable forever.
    fences: std::collections::HashSet<u64>,
    fence_watch_added: Vec<(u32, RawFd)>,
    /// An already-signalled sync_file, made once on demand: what a guest
    /// fence that has signalled becomes when the host GPU must wait on it.
    signalled: Option<OwnedFd>,
    next_mapping_id: u32,
    /// Where forwarded ioctls go. The host driver, except under test.
    host: Box<dyn HostDriver>,
    /// The display broker link, when the device has a display
    /// (docs/SCANOUT.md). `None`: flips are acked and dropped.
    display: Option<std::sync::Arc<crate::display::DisplayLink>>,
    /// dma-bufs exported for scanout, one per (owner file, host GEM handle).
    dmabufs: crate::display::DmabufCache,
    /// The modifier of each GEM object NVK imported through nvidia-drm, for
    /// RM-export blobs (`rm_import.rs`).
    rm_layouts: rm_import::RmLayouts,
    /// Where RM memory objects live, followed to the GEM objects NVK makes
    /// of them (`rm_import::RmPlacements`).
    rm_placements: rm_import::RmPlacements,
    /// `RmResourceImport`s served (rm_resource.rs).
    rm_resource_imports: u64,
    /// The guest's clipboard transfer being reassembled (`ClipboardToHost`).
    clip_in: clipboard::ClipIn,
    /// Video memory charged to this guest, against its limit. See
    /// `crate::vram`. `Vram::new(None)` is no limit, which is what a VMM that
    /// never sets one gets.
    vram: crate::vram::Vram,
    /// Ceilings on the timeouts a guest may forward (bounds.rs).
    bounds: bounds::Bounds,
    /// Venus (docs/VENUS.md), with `--venus` only. `None`: `GpuCmd` is
    /// refused as an unknown message is.
    #[cfg(feature = "venus")]
    venus: Option<crate::venus::Venus>,
    /// The token of the `GpuCmd` just dispatched, when it was a fenced
    /// command now waiting for its fence. See [`NvidiaBackend::take_held`].
    #[cfg(feature = "venus")]
    venus_held: Option<u64>,
    /// Why the request being served was answered here rather than by the
    /// host, for the trace. Set on refusal paths only; read and cleared by
    /// `dispatch_traced`. See docs/TRACING.md.
    #[cfg(feature = "trace")]
    trace_refusal: std::cell::Cell<conduit_trace::Refusal>,
}

/// A placement the guest can hand back, and everything needed to undo it.
struct LiveMap {
    key: (u64, u64),
    region: crate::shm::ShmRegion,
    length: u64,
}

/// `NvKmsIoctlCommand::NVKMS_IOCTL_REGISTER_SURFACE`, the one that names the
/// memory it registers by a file descriptor. Its enum index moves between
/// releases (nvkms-api.h): 16 in 535, 17 from 580 through 610, 16 again in
/// 615. Read with the wrong index, the fd goes to the host untranslated, NVKMS
/// fails the call (-EPERM) and NVIDIA's EGL crashes importing a dma-buf.
fn nvkms_register_surface(v: Option<abi::version::DriverVersion>) -> u32 {
    use abi::version::DriverVersion as V;
    match v {
        Some(v) if v >= V::new(580, 0, 0) && v < V::new(615, 0, 0) => 17,
        _ => 16,
    }
}
/// `NVKMS_IOCTL_QUERY_DISP`. Third in the enum since NVKMS's first release;
/// the commands that moved (see above) all come after it.
const NVKMS_QUERY_DISP: u32 = 2;
/// `sizeof(struct NvKmsQueryDispRequest)`: a device handle and a disp handle.
/// The reply follows it and is the part whose size varies between releases.
const NVKMS_QUERY_DISP_REQUEST: usize = 8;
/// Byte offset of `planes[0].u` inside `NvKmsRegisterSurfaceRequest`.
const NVKMS_SURFACE_FD_OFFSET: usize = 16;

/// The result of checking one guest ioctl against the host's ABI profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiCheck {
    /// The escape is known and the guest's parameter size matches.
    Ok,
    /// No profile yet -- CHECK_VERSION_STR has not been seen.
    NoProfile,
    /// The escape is not in this driver's table.
    UnknownEscape,
    /// The escape is variable length; there is no size to check.
    VariableLength,
    /// The guest disagrees with the host ABI about this struct's size.
    SizeMismatch { expected: u32, actual: u32 },
}
impl NvidiaBackend {
    /// Create a backend with a custom SHM zone config.
    pub fn new(cfg: ZoneConfig) -> Self {
        Self::with_shm(ShmAllocator::new(cfg))
    }

    fn with_shm(shm: ShmAllocator) -> Self {
        Self {
            window: None,
            dri_maps: std::collections::HashMap::new(),
            aperture: Default::default(),
            dri_given: Default::default(),
            msg_counts: std::collections::BTreeMap::new(),
            live_maps: std::collections::HashMap::new(),
            guards: Default::default(),
            rmctrl: None,
            caps: crate::caps::Caps::DEFAULT,
            caps_refused: std::collections::BTreeMap::new(),
            vram: crate::vram::Vram::new(None),
            bounds: bounds::Bounds::NORMAL,
            rmallow: None,
            uvm: None,
            osdesc: None,
            vidmem: None,
            vidmem_untracked_frees: 0,
            guest_ram: None,
            registrations: std::collections::HashMap::new(),
            registrations_served: 0,
            registrations_peak: 0,
            uvm_denied: std::collections::HashSet::new(),
            os_events: std::collections::HashMap::new(),
            allow_refused: std::collections::BTreeMap::new(),
            abi_refused: std::collections::BTreeMap::new(),
            rm_classes: std::collections::BTreeMap::new(),
            rm_controls: std::collections::BTreeMap::new(),
            ioctls_by_ns: std::collections::BTreeMap::new(),
            watch_added: Vec::new(),
            watch_removed: Vec::new(),
            fences: std::collections::HashSet::new(),
            fence_watch_added: Vec::new(),
            signalled: None,
            next_mapping_id: 1,
            current_msg: MsgType::Ioctl,
            current_handle: 0,
            current_data_len: 0,
            handles: HandleTable::new(),
            shm,
            active_maps: crate::mmap::MmapContext::new(),
            handle_kinds: std::collections::HashMap::new(),
            driver: None,
            abi: None,
            host: Box::new(RealHost),
            display: None,
            dmabufs: Default::default(),
            rm_layouts: Default::default(),
            rm_placements: Default::default(),
            rm_resource_imports: 0,
            clip_in: Default::default(),
            #[cfg(feature = "venus")]
            venus: None,
            #[cfg(feature = "venus")]
            venus_held: None,
            #[cfg(feature = "trace")]
            trace_refusal: Default::default(),
        }
    }

    /// Send every forwarded ioctl to `host` instead of the host driver.
    pub fn set_host(&mut self, host: Box<dyn HostDriver>) {
        self.host = host;
    }

    /// Create a backend with the default 256 MiB zone split.
    pub fn with_default_zones() -> Self {
        Self::new(ZoneConfig::default_1gib())
    }

    /// Total SHM BAR size (for VMM config space).
    pub fn shm_total_size(&self) -> u64 {
        self.shm.total_size()
    }

    /// Raw memfd fd (for KVM memslot creation).
    pub fn shm_memfd_raw(&self) -> i32 {
        self.shm.memfd_raw()
    }

    /// Choose what this guest is served.
    pub fn set_caps(&mut self, caps: crate::caps::Caps) {
        self.caps = caps;
    }

    pub fn caps(&self) -> crate::caps::Caps {
        self.caps
    }

    /// Set the guest's video-memory budget. `None` is no limit.
    ///
    /// Refused with a limit on a release that has no video-memory table of its
    /// own: the limit could not be enforced, and a backend that announces a
    /// limit it is not holding the guest to is worse than one with none.
    pub fn set_vram_limit_mib(&mut self, mib: Option<u64>) -> std::result::Result<(), String> {
        if mib.is_some() && !self.vidmem.is_some_and(|s| s.exact) {
            return Err(match self.driver {
                Some(v) => format!(
                    "--vram-limit-mib: host driver {v} has no video-memory table of its own"
                ),
                None => "--vram-limit-mib: the host driver release is not known yet".to_string(),
            });
        }
        self.vram = crate::vram::Vram::new(mib);
        Ok(())
    }

    /// Safe mode: the blocking timeouts a guest may forward shrink to
    /// [`bounds::Bounds::SAFE`]. The caller also sets a video-memory limit.
    pub fn set_safe_mode(&mut self, on: bool) {
        self.bounds = if on {
            bounds::Bounds::SAFE
        } else {
            bounds::Bounds::NORMAL
        };
    }

    /// The budget this backend enforces, in MiB; 0 when there is none.
    ///
    /// Announced in device config, where the VMM compares it against what it
    /// was configured with, so a backend that is not enforcing that limit
    /// cannot go unnoticed. The guest is not told.
    pub fn vram_limit_mib(&self) -> u64 {
        self.vram.limit_mib()
    }

    /// Count a refusal for want of a capability, and say which one once.
    fn refuse_for_caps(&mut self, what: String, needs: &str) {
        traced_refusal!(self, Caps);
        let n = self.caps_refused.entry(what.clone()).or_insert(0);
        if *n == 0 {
            log::warn!(
                "{what} refused: needs --caps {needs} (serving {})",
                self.caps
            );
        }
        *n += 1;
    }

    /// The host driver's release, from the host itself: the ABI profile is
    /// chosen from it before the first guest message, so no guest ioctl is
    /// ever served without one. Refused when no profile covers the release.
    pub fn set_host_driver_version(
        &mut self,
        v: abi::version::DriverVersion,
    ) -> std::result::Result<(), String> {
        let Some(t) = abi::versions::table_for(v) else {
            return Err(format!("host driver {v} is older than every ABI profile"));
        };
        self.driver = Some(v);
        self.abi = Some(t);
        // Without a table nothing can be said about which controls carry a
        // pointer RM dereferences, and the backend would forward all of them.
        // It refuses to start instead. Unreachable as things are -- the tables
        // and the ABI profiles start at the same release -- which is the
        // reason to state it here rather than discover it later.
        let Some(sel) = abi::rmctrl::select(v) else {
            return Err(format!("host driver {v} has no RM pointer table"));
        };
        self.rmctrl = Some(sel);
        // Likewise for the allowlist: without it the backend would forward
        // every control RM is willing to run for a service account, which is
        // most of them.
        let Some(allow) = abi::rmallow::select(v) else {
            return Err(format!("host driver {v} has no RM allowlist"));
        };
        self.rmallow = Some(allow);
        // And for UVM. Nothing here is a privilege rule -- UVM has none to
        // read -- but without the table a UVM call has no size to be checked
        // against and no way to say where its descriptors are, which is the
        // whole of what the backend can check there.
        let Some(uvm) = abi::uvm::select(v) else {
            return Err(format!("host driver {v} has no UVM command table"));
        };
        self.uvm = Some(uvm);
        // And the routes that name memory by a CPU address, so they can be
        // recognised and refused. Without the table they would not be.
        let Some(osdesc) = abi::osdesc::select(v) else {
            return Err(format!("host driver {v} has no OS-descriptor table"));
        };
        self.osdesc = Some(osdesc);
        self.vidmem = abi::vidmem::select(v);
        log::info!(
            "host driver {v}: {} RM controls carry a pointer RM dereferences",
            sel.table.len()
        );
        log::info!(
            "host driver {v}: RM exports {} controls and {} classes to an unprivileged caller, \
             less {} refused under every cap",
            allow.ctrl.len(),
            allow.class.len(),
            abi::rmallow::DENY.len()
        );
        if !sel.exact {
            // The table is an older release's. It cannot describe a control
            // this release added, so a control any release describes and this
            // table does not is refused rather than forwarded.
            log::warn!(
                "host driver {v} has no RM pointer table of its own; using an older release's, \
                 and refusing every control another release describes that it does not"
            );
        }
        log::info!("host driver {v}: ABI profile selected, {} escapes", t.len());
        Ok(())
    }

    /// The host releases the backend starts on without
    /// `--allow-nearest-abi`: those every table [`Self::inexact_tables`]
    /// counts has as its own. `conduit doctor` lists the same
    /// (packaging/supported-drivers.sh; a test keeps them equal).
    pub fn accepted_releases() -> Vec<abi::version::DriverVersion> {
        abi::rmctrl::supported_versions()
            .filter(|v| abi::rmallow::select(*v).is_some_and(|s| s.exact))
            .filter(|v| abi::uvm::select(*v).is_some_and(|s| s.exact))
            .filter(|v| abi::vidmem::select(*v).is_some_and(|s| s.exact))
            .collect()
    }

    /// The tables chosen for the host release that are not that release's
    /// own but the nearest older one's, by name. Empty when every table is
    /// exact (or no release is set yet).
    ///
    /// A nearest table is not a working fallback: the RM allowlist then
    /// requires every release to agree about a class or control, and refuses
    /// one whose parameter size changed -- which a channel allocation's always
    /// has, so a guest fails at its first channel rather than at start.
    pub fn inexact_tables(&self) -> Vec<&'static str> {
        if self.driver.is_none() {
            return Vec::new();
        }
        // The escape profile is not counted: it comes from gVisor's nvproxy,
        // which lags NVIDIA's releases, and the escapes it describes have
        // kept their sizes across them (CHECK_VERSION_STR still guards it).
        let mut out = Vec::new();
        if self.rmctrl.is_some_and(|s| !s.exact) {
            out.push("RM pointer table");
        }
        if self.rmallow.is_some_and(|s| !s.exact) {
            out.push("RM allowlist");
        }
        if self.uvm.is_some_and(|s| !s.exact) {
            out.push("UVM command table");
        }
        if self.vidmem.is_some_and(|s| !s.exact) {
            out.push("video-memory table");
        }
        out
    }

    /// Whether `take_watch_updates` or `take_fence_watches` has anything to
    /// hand over, for a transport to look without taking.
    pub fn has_watch_updates(&self) -> bool {
        !(self.watch_added.is_empty()
            && self.watch_removed.is_empty()
            && self.fence_watch_added.is_empty())
    }

    /// Descriptors opened and closed since this was last called.
    ///
    /// A transport calls it after serving messages and keeps its poll set in
    /// step. Draining rather than reading, so two transports cannot both think
    /// they own a watch.
    pub fn take_watch_updates(&mut self) -> (Vec<(u32, RawFd)>, Vec<u32>) {
        (
            std::mem::take(&mut self.watch_added),
            std::mem::take(&mut self.watch_removed),
        )
    }

    /// Host fences opened since this was last called, to be watched until
    /// they first become readable and then dropped from the poll set (they
    /// stay readable). Their removal comes through `take_watch_updates` like
    /// any other handle's, when the guest closes them.
    pub fn take_fence_watches(&mut self) -> Vec<(u32, RawFd)> {
        std::mem::take(&mut self.fence_watch_added)
    }

    /// Give the backend somewhere to place device memory.
    ///
    /// Until this is called every `RM_MAP_MEMORY` still succeeds on the host --
    /// the mapping is real -- but the `mmap` that follows is refused, because
    /// there is no address in the guest that names it.
    pub fn set_window(&mut self, placer: Box<dyn crate::shm::WindowPlacer>) {
        self.window = Some(placer);
    }

    /// Guest RAM, from the transport, which is the only part that has it.
    ///
    /// Without it the backend cannot build a host address out of a guest's
    /// pages, so memory registered by CPU address stays refused -- which is
    /// what it is in a backend that has no transport at all.
    pub fn set_guest_ram(&mut self, ram: Box<dyn crate::guestmem::GuestRam>) {
        self.guest_ram = Some(ram);
    }

    /// How many host descriptors the guest currently holds open.
    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }

    /// How many registrations by address the guest is holding host mappings
    /// for. A number that does not come back to zero is guest memory this
    /// process keeps mapped after RM has let go of it.
    pub fn registration_count(&self) -> usize {
        self.registrations.len()
    }

    /// Free bytes per SHM zone, as `(uc, wc, wb)`. For tests that assert a
    /// mapping cycle gives back exactly what it took.
    pub fn shm_free_bytes(&self) -> (u64, u64, u64) {
        self.shm.free_bytes()
    }

    pub fn shm_base_ptr(&self) -> *mut u8 {
        self.shm.base_ptr()
    }

    /// Override the SHM base pointer to the guest memory HVA.
    /// Called by the VMM after guest memory setup.
    pub fn set_shm_base(&mut self, ptr: *mut u8) {
        self.shm.set_base_ptr(ptr);
    }

    /// Create a minimal backend suitable for unit tests (8-page total BAR).
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
        })
    }

    // ------------------------------------------------------------------
    // Teardown
    //
    // Called by the VMM on:
    //   - normal VM shutdown (virtio device reset before exit)
    //   - ungraceful VM exit (SIGKILL, crash, libkrun teardown)
    //
    // Draining the handle table closes every host fd, which triggers the
    // host NVIDIA driver's fd-release path and frees all RM objects.
    // Analogous to nvproxy's Release() in nvproxy.go.
    // ------------------------------------------------------------------

    pub fn teardown(&mut self) {
        self.teardown_with("backend teardown");
    }

    /// [`NvidiaBackend::teardown`], with why the guest's generation ended for
    /// the display's log.
    fn teardown_with(&mut self, why: &str) {
        // Venus first: its scanout goes off before the display drops every
        // buffer of the generation.
        #[cfg(feature = "venus")]
        self.teardown_venus();
        self.teardown_scanout(why);
        log::info!(
            "NvidiaBackend::teardown: video memory {} MiB in use, peak {} MiB, {} allocation(s) refused, limit {}",
            self.vram.in_use() >> 20,
            self.vram.peak() >> 20,
            self.vram.refused(),
            match self.vram.limit() {
                Some(l) => format!("{} MiB", l >> 20),
                None => "none".to_string(),
            }
        );
        if self.vidmem_untracked_frees > 0 {
            log::warn!(
                "NvidiaBackend::teardown: {} VID_HEAP_CONTROL FREE call(s) not charged back",
                self.vidmem_untracked_frees
            );
        }
        for (id, pool) in self.aperture.take_all() {
            self.unmap_uvm_pool(id, pool);
        }
        log::info!(
            "NvidiaBackend::teardown: draining {} handles, {} active maps",
            self.handles.len(),
            self.active_maps.len()
        );
        if !self.allow_refused.is_empty() {
            log::warn!(
                "NvidiaBackend::teardown: refused by the RM allowlist: {}",
                self.allow_refused
                    .iter()
                    .map(|(k, n)| format!("{k} x{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        // Guest memory this process still has mapped after RM has been told to
        // let go of it. Anything but zero is a leak of the guest's pages into
        // the backend's address space, and the number is only visible here.
        if self.registrations_served > 0 || !self.registrations.is_empty() {
            let left = self.registrations.len();
            let line = format!(
                "NvidiaBackend::teardown: {} registration(s) by address, at most {} held at once, {left} still held",
                self.registrations_served, self.registrations_peak,
            );
            if left == 0 {
                log::info!("{line}");
            } else {
                log::warn!("{line} -- guest memory is still mapped here");
            }
        }
        if !self.caps_refused.is_empty() {
            log::warn!(
                "NvidiaBackend::teardown: refused for want of a capability: {}",
                self.caps_refused
                    .iter()
                    .map(|(w, n)| format!("{w}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if !self.abi_refused.is_empty() {
            log::warn!(
                "NvidiaBackend::teardown: refused {} ioctl(s) the ABI profile does not describe: {}",
                self.abi_refused.values().sum::<u64>(),
                self.abi_refused
                    .iter()
                    .map(|(e, n)| format!("{e:#04x}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        // The whole forwarded surface, by namespace. A filter has to cover all
        // of these, and today only 'F' has a table to check against at all.
        if !self.ioctls_by_ns.is_empty() {
            let mut by_ns: std::collections::BTreeMap<char, Vec<String>> = Default::default();
            for ((ns, nr), n) in &self.ioctls_by_ns {
                by_ns.entry(*ns).or_default().push(format!("{nr:#04x}={n}"));
            }
            for (ns, entries) in by_ns {
                let what = match ns {
                    'F' => "nvidia escapes",
                    'd' => "DRM ioctls",
                    'm' => "modeset ioctls",
                    'u' => "UVM ioctls",
                    _ => "unknown namespace",
                };
                log::info!(
                    "NvidiaBackend::teardown: {} {what} ({}): {}",
                    entries.len(),
                    ns,
                    entries.join(" ")
                );
            }
        }

        // The two sets a filter would be written from. Printed whole rather
        // than summarised: the long tail is the interesting part, because that
        // is where something a pipeline needs exactly once hides.
        if !self.rm_classes.is_empty() {
            log::info!(
                "NvidiaBackend::teardown: {} RM_ALLOC class(es): {}",
                self.rm_classes.len(),
                self.rm_classes
                    .iter()
                    .map(|(c, n)| format!("{c:#06x}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if !self.rm_controls.is_empty() {
            log::info!(
                "NvidiaBackend::teardown: {} RM_CONTROL command(s): {}",
                self.rm_controls.len(),
                self.rm_controls
                    .iter()
                    .map(|(c, n)| format!("{c:#010x}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let total: u64 = self.msg_counts.values().sum();
        log::info!(
            "NvidiaBackend::teardown: served {total} message(s): {}",
            self.msg_counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Restore SHM backing and reclaim every extent before closing host
        // fds. A guest process that exits without unmapping is the normal
        // case, not an error -- most of the mappings in a captured trace are
        // still live when the process ends.
        let leftovers: Vec<_> = self
            .active_maps
            .drain()
            .into_iter()
            .map(|e| e.region)
            .collect();
        for region in leftovers {
            self.release_window_region(&region, "teardown");
        }
        self.handles.drain_all();
    }

    /// Device reset (a guest reboot, or its driver reloading): release
    /// everything the previous boot held on the host -- every open file and
    /// with it every RM client and object, VRAM charge, window and aperture
    /// placement, registration of guest memory and scanout export -- and start
    /// again as a freshly built backend would.
    ///
    /// What the transport and the command line set up is kept: the driver
    /// release and its tables, caps, the VRAM limit (its usage starts at
    /// zero), the window, guest RAM, the display link and the SHM allocator.
    pub fn reset(&mut self) {
        log::info!(
            "device reset: releasing {} open file(s), {} window placement(s) and {} aperture pool(s) of the previous boot",
            self.handles.len(),
            self.live_maps.len() + self.active_maps.len(),
            self.aperture.len()
        );
        // The frontend dropped every placement in the window and the aperture
        // when it reset the device (QEMU's virtio_reset does, as the spec
        // asks), so nothing is withdrawn: only this side's books are cleared.
        let window = self.window.take();
        // Placements made for DRM objects: these outlive the file that made them.
        let live: Vec<u32> = self.live_maps.keys().copied().collect();
        for id in live {
            let Some(m) = self.live_maps.remove(&id) else {
                continue;
            };
            self.active_maps.remove(m.region.offset);
            if let Err(e) = self.shm.free(&m.region) {
                log::warn!("device reset: freeing the window region of {id}: {e}");
            }
        }
        self.dri_maps.clear();
        // Every file, through the guest's own close path: it withdraws the
        // file's placements, drops its pools, registrations and exports, and
        // closing the host fd makes RM free every object made on it.
        let mut scratch = [0u8; size_of::<MsgHeader>() + 64];
        for handle in self.handles.handles() {
            self.current_handle = handle as u32;
            self.handle_close(0, &[], &mut scratch);
        }
        let next_handle = self.handles.next_handle();
        // Venus first, with no window: its region 3 placements went with the
        // rest. What it held is kept across the reset, emptied.
        #[cfg(feature = "venus")]
        let venus = self.reset_venus();
        self.teardown_with("device reset");

        // Everything else back to how `new` left it.
        let tiny = ShmAllocator::new(ZoneConfig {
            uc_size: 4096,
            wc_size: 0,
            wb_size: 0,
        });
        let mut old = std::mem::replace(self, Self::with_shm(tiny));
        std::mem::swap(&mut self.shm, &mut old.shm);
        std::mem::swap(&mut self.host, &mut old.host);
        self.window = window;
        self.guest_ram = old.guest_ram.take();
        // The guest's picture went with it (`teardown_scanout`): the viewer
        // was blanked, and the boot console (if any) shows the reboot --
        // firmware, boot menu, disk password -- until the guest's driver
        // flips again.
        self.display = old.display.take();
        self.caps = old.caps;
        self.driver = old.driver;
        self.abi = old.abi;
        self.rmctrl = old.rmctrl.take();
        self.rmallow = old.rmallow.take();
        self.uvm = old.uvm.take();
        self.osdesc = old.osdesc.take();
        self.vidmem = old.vidmem.take();
        self.vram = crate::vram::Vram::new(old.vram.limit().map(|b| b >> 20));
        #[cfg(feature = "venus")]
        {
            self.venus = venus;
        }
        // The transport still has to drop the watches of the files closed above.
        self.watch_removed = std::mem::take(&mut old.watch_removed);
        self.handles.skip_to(next_handle);
        self.next_mapping_id = old.next_mapping_id;
        log::info!("device reset: done; the backend is as freshly started");
    }

    // ------------------------------------------------------------------
    // Top-level dispatch
    // ------------------------------------------------------------------

    pub fn dispatch(&mut self, req_buf: &[u8], resp_buf: &mut [u8]) -> usize {
        if req_buf.len() < size_of::<MsgHeader>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, 0);
        }
        let hdr = read_struct::<MsgHeader>(req_buf, 0);

        let Some(msg_type) = MsgType::from_u32(hdr.msg_type) else {
            log::warn!("unknown msg_type {}", hdr.msg_type);
            self.current_msg = MsgType::Ioctl;
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, 0);
        };
        self.current_msg = msg_type;
        *self
            .msg_counts
            .entry(match msg_type {
                MsgType::Open => "open",
                MsgType::Close => "close",
                MsgType::Ioctl => "ioctl",
                MsgType::Mmap => "mmap",
                MsgType::Munmap => "munmap",
                MsgType::GetProcFiles => "get_proc_files",
                MsgType::GetSysFiles => "get_sys_files",
                MsgType::EventReady => "event_ready",
                MsgType::ScanoutFlip => "scanout_flip",
                MsgType::ScanoutDisable => "scanout_disable",
                MsgType::InputEvent => "input_event",
                MsgType::DisplayMode => "display_mode",
                MsgType::CursorUpdate => "cursor_update",
                MsgType::ClipboardFromHost => "clipboard_from_host",
                MsgType::ScanoutReleased => "scanout_released",
                MsgType::ClipboardToHost => "clipboard_to_host",
                MsgType::ClipboardRequest => "clipboard_request",
                MsgType::GpuCmd => "gpu_cmd",
                MsgType::RmResourceImport => "rm_resource_import",
            })
            .or_insert(0) += 1;
        // The handle travels in the header, not the payload -- every message
        // after Open acts on one, and Open's response returns one the same way.
        self.current_handle = hdr.handle;

        // A fence handle is a sync_file, not a device: only Close may name
        // it. Forwarded, an ioctl on it would reach the sync_file's own
        // (SYNC_IOC_MERGE makes descriptors here).
        if msg_type != MsgType::Close && self.fences.contains(&(hdr.handle as u64)) {
            return self.write_error_resp(resp_buf, Status::BadHandle, 0, 0);
        }

        let payload = &req_buf[size_of::<MsgHeader>()..];
        match msg_type {
            MsgType::Open => self.handle_open(0, payload, resp_buf),
            MsgType::Close => self.handle_close(0, payload, resp_buf),
            MsgType::Ioctl => self.handle_ioctl(0, payload, resp_buf),
            MsgType::Mmap => self.handle_mmap(payload, resp_buf),
            MsgType::Munmap => self.handle_munmap(payload, resp_buf),
            MsgType::GetProcFiles => self.handle_get_files(FileTree::Proc, resp_buf),
            MsgType::GetSysFiles => self.handle_get_files(FileTree::Sys, resp_buf),
            // Host to guest only. A guest that sends one is confused about the
            // direction of the queue, and saying so beats serving it.
            MsgType::EventReady
            | MsgType::InputEvent
            | MsgType::DisplayMode
            | MsgType::ClipboardFromHost
            | MsgType::ScanoutReleased => {
                log::warn!(
                    "{msg_type:?} arrived from the guest; that message only travels outward"
                );
                self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL)
            }
            MsgType::ScanoutFlip => self.handle_scanout_flip(payload, resp_buf),
            MsgType::ScanoutDisable => self.handle_scanout_disable(payload, resp_buf),
            MsgType::CursorUpdate => self.handle_cursor_update(payload, resp_buf),
            MsgType::ClipboardToHost => self.handle_clipboard_to_host(payload, resp_buf),
            MsgType::ClipboardRequest => self.handle_clipboard_request(resp_buf),
            MsgType::GpuCmd => self.handle_gpu_cmd(payload, resp_buf),
            MsgType::RmResourceImport => self.handle_rm_resource_import(payload, resp_buf),
        }
    }
}

/// Close whatever the guest left open.
///
/// This was an inherent method named `drop` rather than a `Drop` impl, so it
/// never ran: a backend that went out of scope without `teardown()` leaked
/// every host fd it held. `cargo` reported it only as an unused-method warning.
impl Drop for NvidiaBackend {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            log::warn!(
                "NvidiaBackend dropped with {} handles still open — \
                 call teardown() before dropping for clean shutdown",
                self.handles.len()
            );
            self.handles.drain_all();
        }
    }
}

// ============================================================
// Serialisation helpers
// ============================================================

fn read_struct<T: Copy>(buf: &[u8], offset: usize) -> T {
    assert!(buf.len() >= offset + size_of::<T>());
    unsafe { (buf.as_ptr().add(offset) as *const T).read_unaligned() }
}

fn write_struct<T: Copy>(buf: &mut [u8], val: &T) -> usize {
    let sz = size_of::<T>();
    assert!(buf.len() >= sz);
    unsafe { (buf.as_mut_ptr() as *mut T).write_unaligned(*val) }
    sz
}

mod aperture;
mod bounds;
mod clipboard;
mod fence;
mod files;
pub use files::FileTree;
mod gpu_cmd;
mod host;
pub use host::{HostDriver, RealHost};
mod ioctl;
mod nested;
mod open;
mod os_event;
mod osdesc;
mod resp;
mod rm_fd;
mod rm_import;
pub use rm_import::{RmObject, RmPlacement, SurfaceLayout, Tiling};
mod rm_resource;
mod rmctrl;
mod scanout;
mod simple;
#[cfg(feature = "trace")]
mod trace;
mod uvm;
mod vidmem;
mod window;

#[cfg(test)]
mod tests;

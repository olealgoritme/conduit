//! A vhost-user backend serving `NvidiaBackend` to a guest.
//!
//! The guest driver (`driver/virtio_gpu_nv.c`) binds virtio device ID 45 and
//! posts one descriptor chain per request: a readable descriptor holding the
//! request, and a writable one for the response. That is the whole transport.
//!
//! Attach it to QEMU >= 11.1 with the generic vhost-user device (named
//! `vhost-user-test-device-pci` there), which asks for the shared-memory
//! regions with `GET_SHMEM_CONFIG` (`device::shm_regions`):
//!
//! ```text
//! qemu-system-x86_64 \
//!   -chardev socket,id=nv,path=/tmp/nvgpu.sock \
//!   -device vhost-user-test-device-pci,chardev=nv,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036 \
//!   -object memory-backend-memfd,id=mem,size=8G,share=on -machine q35,memory-backend=mem
//! ```
//!
//! Stock 11.1 caps vhost-user device config at 256 bytes; see
//! conduit/host/qemu/patches for the two patches this device needs.
//!
//! Guest memory must be shared (`memory-backend-memfd,share=on`) or the backend
//! cannot read the request the guest wrote.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use device::caps::Caps;
use device::display::{DisplayLink, DisplayMode, InputSink};
use device::host;
use device::nvidia::NvidiaBackend;
use device::shm::WindowPlacer;
use device::shm_regions::{
    APERTURE_LEN, SHM_ID_APERTURE, SHM_ID_WINDOW, page_align, pool_map_flags, region_sizes,
};
use device::virtio::{NUM_QUEUES, QUEUE_SIZE, VIRTIO_ID_GPU_NV, VirtioGpuNvConfig};
use protocol::messages::{
    CLIPBOARD_MESSAGE_HEAD, CLIPBOARD_MIME_TEXT, ClipboardChunk, DISPLAY_MODE_MESSAGE_LEN,
    DisplayModeEvent, InputEventEntry, MsgHeader, MsgType, clipboard_mime, encode_clipboard_chunk,
    encode_display_mode, encode_input_events, input_events_that_fit,
};
use std::os::fd::{BorrowedFd, RawFd};
use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserShMemConfig,
    VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Backend, VhostUserFrontendReqHandler};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_VERSION_1};
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::QueueOwnedT;
use vm_memory::{
    Address, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend,
    GuestMemoryLoadGuard, GuestMemoryMmap, GuestMemoryRegion,
};

/// The driver calls `virtio_find_vqs(vdev, 2, ...)` and fails probe on the
/// error from that call, so offering fewer is fatal before config is read.
const QUEUE_COUNT: usize = NUM_QUEUES;
/// Largest response we will build for one request.
const RESP_MAX: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(version, about = "vhost-user backend for virtio-nvgpu")]
struct Args {
    /// Unix socket QEMU connects to.
    #[arg(long, default_value = "/tmp/nvgpu.sock")]
    socket: String,

    /// Where the host driver publishes itself. Overridable for testing
    /// against a fixture tree rather than a live driver.
    #[arg(long, default_value = host::PROC_NVIDIA)]
    proc_nvidia: PathBuf,

    /// What the guest is served: a comma list of graphics, compute, video and
    /// utility. Compute (CUDA, through nvidia-uvm) is off unless named.
    #[arg(long, default_value_t = Caps::DEFAULT, value_parser = Caps::parse)]
    caps: Caps,

    /// Video memory the guest may hold, in MiB. Omitted, it may take the
    /// whole card. The limit is announced in device config, so a VMM given
    /// one refuses to start a guest on a backend that is not enforcing it.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    vram_limit_mib: Option<u64>,

    /// Give the guest a display with this preferred mode, WxH@HZ
    /// (docs/SCANOUT.md). Given bare, 2560x1440@240. Omitted (and no
    /// --display-socket), the device has no display and the guest no KMS.
    #[arg(long, value_name = "WxH@HZ", num_args = 0..=1,
          default_missing_value = "2560x1440@240", value_parser = DisplayMode::parse)]
    display: Option<DisplayMode>,

    /// The display broker's socket. Frames go there as dma-bufs, input comes
    /// back. Implies --display. Connected lazily and reconnected; with no
    /// broker listening, flips are acked and dropped.
    #[arg(long, value_name = "PATH")]
    display_socket: Option<PathBuf>,

    /// Give the guest head a cursor plane whose image the viewer shows as the
    /// host pointer (zero-latency cursor). `off`: the guest compositor draws
    /// the cursor into its frames, as before.
    #[arg(long, value_name = "on|off", default_value = "on",
          value_parser = ["on", "off"])]
    display_cursor: String,

    /// Start even when the host driver release has no ABI tables of its own,
    /// using the nearest older release's. Expect guests to fail at their first
    /// channel allocation: the allowlist refuses every class whose size
    /// changed between releases.
    #[arg(long)]
    allow_nearest_abi: bool,
}

/// Places device memory through the vhost-user backend request channel.
///
/// The VMM owns the window's address space and the memory slot that describes
/// it, so it is the only process whose `MAP_FIXED` the guest can see. This
/// hands the descriptor over and lets it do the placement.
/// Guest RAM, as the backend can map from it.
///
/// A vhost-user frontend sends each region as a file descriptor, and
/// `vhost-user-backend` builds every region with `MmapRegion::from_file`, so
/// the fd and the offset within it survive into here. That is what makes a
/// host address aliasing a guest's own pages possible at all; without the fd
/// there would be only this process's mapping, which cannot be re-mapped
/// somewhere else.
///
/// Holds the atomic handle rather than a snapshot, so a memory table replaced
/// after this was built is seen rather than silently stale.
struct VhostGuestRam(GuestMemoryAtomic<GuestMemoryMmap>);

impl device::guestmem::GuestRam for VhostGuestRam {
    fn backing(&self, gpa: u64) -> Option<device::guestmem::Backing> {
        let mem = self.0.memory();
        let region = mem.find_region(GuestAddress(gpa))?;
        // A region the frontend sent without a descriptor cannot be mapped
        // from, only read through. None has been seen, and a registration is
        // refused rather than served from a mapping we cannot alias.
        let file = region.file_offset()?;
        let within = gpa.checked_sub(region.start_addr().raw_value())?;
        Some(device::guestmem::Backing {
            fd: file.file().try_clone().ok()?.into(),
            offset: file.start().checked_add(within)?,
            len: region.len().checked_sub(within)?,
        })
    }
}

struct VhostWindow(Backend);

/// Set once the frontend asks for `GET_SHMEM_CONFIG`: a spec frontend (QEMU
/// >= 11.1) that lays the regions out and picks mapping addresses itself.
/// nesbox never asks. One process serves one VM, and a device reset keeps
/// the same frontend, so this is never cleared.
static SPEC_SHMEM_FRONTEND: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl WindowPlacer for VhostWindow {
    fn place(
        &self,
        shm_offset: u64,
        len: u64,
        fd: RawFd,
        fd_offset: u64,
        writable: bool,
    ) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID,
            padding: [0; 7],
            fd_offset,
            shm_offset,
            len: page_align(len),
            flags: if writable {
                VhostUserMMapFlags::WRITABLE.bits()
            } else {
                0
            },
        };
        // SAFETY: the descriptor is owned by the handle table for the whole of
        // this call, and is only borrowed to be sent.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn withdraw(&self, shm_offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset,
            len: page_align(len),
            flags: 0,
        };
        self.0
            .shmem_unmap(&req)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn place_pool(&self, offset: u64, len: u64, fd: RawFd, addr: u64) -> device::error::Result<()> {
        // On the aperture, fd_offset is the pool's host address as well as
        // its offset in the file: UVM takes the mapping nowhere else.
        let req = VhostUserMMap {
            shmid: NV_SHM_ID_APERTURE,
            padding: [0; 7],
            fd_offset: addr,
            shm_offset: offset,
            len: page_align(len),
            flags: pool_map_flags(
                VhostUserMMapFlags::WRITABLE.bits(),
                SPEC_SHMEM_FRONTEND.load(std::sync::atomic::Ordering::Relaxed),
            ),
        };
        // SAFETY: as in `place`.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn withdraw_pool(&self, offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID_APERTURE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: offset,
            len: page_align(len),
            flags: 0,
        };
        self.0
            .shmem_unmap(&req)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }
}

/// What the event thread is told to start and stop watching.
enum Watch {
    Add(u32, OwnedFd),
    Remove(u32),
}

/// One `EventReady` message: a bare header naming the descriptor.
fn event_ready_bytes(handle: u32) -> Vec<u8> {
    let hdr = MsgHeader::ok(MsgType::EventReady, handle);
    // The wire form is the struct's bytes, which is what the driver reads.
    let p = &hdr as *const MsgHeader as *const u8;
    unsafe { std::slice::from_raw_parts(p, size_of::<MsgHeader>()) }.to_vec()
}

/// Put one message on the event queue, into a buffer the guest posted there.
///
/// Returns false when the guest has posted none, which is the normal state of
/// a guest whose driver predates this queue having a use -- and a reason to
/// drop the notification rather than to fail.
fn push_event(vring: &VringRwLock, mem: &GuestMemoryAtomic<GuestMemoryMmap>, handle: u32) -> bool {
    let guard = mem.memory();
    let mut vr = vring.get_mut();
    let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
        return false;
    };
    let Some(chain) = avail.next() else {
        return false;
    };
    let head = chain.head_index();
    drop(vr);

    let bytes = event_ready_bytes(handle);
    let mut written = 0usize;
    for desc in chain {
        if desc.is_write_only() {
            let n = std::cmp::min(desc.len() as usize, bytes.len());
            if guard.write_slice(&bytes[..n], desc.addr()).is_ok() {
                written = n;
            }
            break;
        }
    }

    if vring.add_used(head, written as u32).is_err() {
        return false;
    }
    let _ = vring.signal_used_queue();
    written > 0
}

/// The event queue and guest memory, once the guest has brought them up.
type EventTarget = Arc<Mutex<Option<(VringRwLock, GuestMemoryAtomic<GuestMemoryMmap>)>>>;

/// Input from the display broker, onto the event queue as `InputEvent`
/// messages: as many events per message as the guest's buffer holds.
struct VqInputSink {
    target: EventTarget,
    warned_small: bool,
    warned_clip_small: bool,
    msg: Vec<u8>,
}

/// Clipboard chunks put on the event queue per call: buffers are shared with
/// input, which must not starve behind a large paste.
const CLIP_CHUNKS_PER_PASS: usize = 16;

impl InputSink for VqInputSink {
    fn push(&mut self, events: &[InputEventEntry]) -> usize {
        let Some((vring, mem)) = self.target.lock().expect("event target").clone() else {
            // No guest yet: nobody to type at. Dropped, not queued.
            return events.len();
        };
        let guard = mem.memory();
        let mut done = 0;
        let mut signalled = false;
        while done < events.len() {
            let mut vr = vring.get_mut();
            let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else {
                break; // no buffer posted; the rest is retried shortly
            };
            let head = chain.head_index();
            drop(vr);

            let mut written = 0u32;
            if let Some(desc) = chain.clone().find(|d| d.is_write_only()) {
                let fit = input_events_that_fit(desc.len() as usize);
                if fit == 0 {
                    // A driver whose event buffers hold only a header cannot
                    // take input at all; say so once and drop it.
                    if !self.warned_small {
                        log::warn!(
                            "display: guest event buffers are {} bytes, too small for input; input dropped (driver predates InputEvent?)",
                            desc.len()
                        );
                        self.warned_small = true;
                    }
                    done = events.len();
                } else {
                    let take = fit.min(events.len() - done);
                    self.msg
                        .resize(protocol::messages::input_event_message_len(take), 0);
                    let n = encode_input_events(&events[done..done + take], &mut self.msg)
                        .expect("sized for it");
                    if guard.write_slice(&self.msg[..n], desc.addr()).is_ok() {
                        written = n as u32;
                    }
                    done += take;
                }
            }
            if vring.add_used(head, written).is_err() {
                break;
            }
            signalled = true;
        }
        if signalled {
            let _ = vring.signal_used_queue();
        }
        done
    }

    fn mode(&mut self, m: &DisplayModeEvent) -> bool {
        let Some((vring, mem)) = self.target.lock().expect("event target").clone() else {
            // No guest yet: it boots with the configured mode anyway.
            return true;
        };
        let guard = mem.memory();
        let mut vr = vring.get_mut();
        let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
            return false;
        };
        let Some(chain) = avail.next() else {
            return false; // no buffer posted; retried shortly
        };
        let head = chain.head_index();
        drop(vr);
        let mut written = 0u32;
        if let Some(desc) = chain.clone().find(|d| d.is_write_only())
            && desc.len() as usize >= DISPLAY_MODE_MESSAGE_LEN
        {
            let mut msg = [0u8; DISPLAY_MODE_MESSAGE_LEN];
            let n = encode_display_mode(m, &mut msg).expect("sized for it");
            if guard.write_slice(&msg[..n], desc.addr()).is_ok() {
                written = n as u32;
            }
        }
        if vring.add_used(head, written).is_err() {
            return false;
        }
        let _ = vring.signal_used_queue();
        true
    }

    fn clipboard(&mut self, generation: u64, data: &[u8], mut offset: usize) -> usize {
        let Some((vring, mem)) = self.target.lock().expect("event target").clone() else {
            // No guest yet: kept, and retried once there is one. The guest
            // also asks again (ClipboardRequest) once its driver is up.
            return offset;
        };
        let guard = mem.memory();
        let mut signalled = false;
        for _ in 0..CLIP_CHUNKS_PER_PASS {
            if offset >= data.len() {
                break;
            }
            let mut vr = vring.get_mut();
            let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else {
                break; // no buffer posted; the rest is retried shortly
            };
            let head = chain.head_index();
            drop(vr);
            let mut written = 0u32;
            if let Some(desc) = chain.clone().find(|d| d.is_write_only()) {
                let room = (desc.len() as usize).saturating_sub(CLIPBOARD_MESSAGE_HEAD);
                if room == 0 {
                    if !self.warned_clip_small {
                        log::warn!(
                            "display: guest event buffers are {} bytes, too small for clipboard; host clipboard dropped (driver predates ClipboardFromHost?)",
                            desc.len()
                        );
                        self.warned_clip_small = true;
                    }
                    offset = data.len();
                } else {
                    let take = room.min(data.len() - offset);
                    let c = ClipboardChunk {
                        generation,
                        total_len: data.len() as u32,
                        offset: offset as u32,
                        len: take as u32,
                        flags: 0,
                        mime: clipboard_mime(CLIPBOARD_MIME_TEXT),
                    };
                    self.msg.resize(CLIPBOARD_MESSAGE_HEAD + take, 0);
                    let n = encode_clipboard_chunk(
                        MsgType::ClipboardFromHost,
                        &c,
                        &data[offset..offset + take],
                        &mut self.msg,
                    )
                    .expect("sized for it");
                    if guard.write_slice(&self.msg[..n], desc.addr()).is_ok() {
                        written = n as u32;
                    }
                    offset += take;
                }
            }
            if vring.add_used(head, written).is_err() {
                break;
            }
            signalled = true;
        }
        if signalled {
            let _ = vring.signal_used_queue();
        }
        offset
    }
}

/// Watch the host's descriptors and tell the guest when one has something to
/// say.
///
/// This exists because the guest cannot find out any other way. NVIDIA's
/// user-mode driver waits for the GPU by polling the descriptor its RM event
/// is delivered on; the interrupt is the host's, and so is the descriptor that
/// becomes readable. Without this relay the guest's `poll` has nothing to
/// report and the driver spins -- measured at a whole core per guest at 100
/// frames a second.
///
/// A descriptor is dropped from the set after it is reported and put back a
/// millisecond later. Level-triggered polling would otherwise spin here
/// instead: the descriptor stays readable until the *guest* consumes the
/// event, which happens through an ioctl this thread never sees. Re-arming on
/// a timer costs a duplicate notification at worst, and the guest answers one
/// by waking, finding nothing, and waiting again.
fn event_pump(rx: Receiver<Watch>, vring: VringRwLock, mem: GuestMemoryAtomic<GuestMemoryMmap>) {
    // How often to re-check a descriptor that is still readable. See the
    // sweep below; this is a safety net, not the notification path.
    const SWEEP: Duration = Duration::from_millis(1);

    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        log::error!(
            "event pump: epoll_create1: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let epfd = unsafe { OwnedFd::from_raw_fd(epfd) };

    let mut watched: HashMap<u64, OwnedFd> = HashMap::new();
    let mut last_sweep = Instant::now();

    // Edge-triggered. Level-triggered would report a descriptor as readable
    // until the *guest* consumes the event, which happens through an ioctl
    // this thread never sees -- so the pump would spin between notifying and
    // being believed. Parking the descriptor for a millisecond instead cost
    // 11% of the frames in an encode run, and parking it for 100 us cost more
    // than that, because then the pump spun on the host's CPU and took it from
    // the guest. An edge costs neither.
    let ctl = |op: i32, fd: i32, handle: u32| {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET) as u32,
            u64: handle as u64,
        };
        unsafe { libc::epoll_ctl(epfd.as_raw_fd(), op, fd, &mut ev) }
    };

    loop {
        // Drain the control channel first: a descriptor closed on the other
        // thread must leave the set before it can be reported again.
        loop {
            match rx.try_recv() {
                Ok(Watch::Add(handle, fd)) => {
                    if ctl(libc::EPOLL_CTL_ADD, fd.as_raw_fd(), handle) == 0 {
                        watched.insert(handle as u64, fd);
                    }
                }
                Ok(Watch::Remove(handle)) => {
                    if let Some(fd) = watched.remove(&(handle as u64)) {
                        ctl(libc::EPOLL_CTL_DEL, fd.as_raw_fd(), handle);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // The safety net: an edge can be missed if a descriptor was already
        // readable when it was added, or if a notification found no buffer
        // posted. Every 10 ms, ask the descriptors directly and re-notify the
        // ones that still have something to say. A lost wake costs a tenth of
        // a frame at 60 Hz rather than a hang.
        if last_sweep.elapsed() >= SWEEP {
            last_sweep = Instant::now();
            for (&handle, fd) in watched.iter() {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 && pfd.revents & libc::POLLIN != 0 {
                    push_event(&vring, &mem, handle as u32);
                }
            }
        }

        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        let n = unsafe {
            libc::epoll_wait(
                epfd.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                SWEEP.as_millis() as i32,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("event pump: epoll_wait: {err}");
            return;
        }

        for ev in events.iter().take(n as usize) {
            // Copied out first: epoll_event is packed, so its field cannot be
            // borrowed.
            let handle = { ev.u64 } as u32;
            if !push_event(&vring, &mem, handle) {
                log::debug!("event pump: no buffer posted for handle {handle}; dropped");
            }
        }
    }
}

/// The queue the host posts events on. The guest posts empty buffers here and
/// the event pump fills them; nothing the guest sends on it is a request.
const EVENT_QUEUE: usize = 1;

/// The shared-memory id the guest driver looks the window up by, which must
/// match the capability the VMM publishes.
const NV_SHM_ID: u8 = SHM_ID_WINDOW;
/// The UVM aperture, where semaphore pools are placed. See `device/src/nvidia/aperture.rs`.
const NV_SHM_ID_APERTURE: u8 = SHM_ID_APERTURE;

struct NvGpuBackend {
    nvidia: Arc<Mutex<NvidiaBackend>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    config: VirtioGpuNvConfig,
    /// Started on the first message, because the event queue and guest memory
    /// are not known before then.
    watches: Option<Sender<Watch>>,
    /// Where display input goes; filled in alongside `watches`.
    input_target: EventTarget,
    /// The request and response of the chain being served, kept across chains.
    /// A fresh 64 KiB response zeroed per request cost more than the host's
    /// whole RM call, and only the bytes dispatch writes are sent back.
    req: Vec<u8>,
    resp: Vec<u8>,
    /// A request was served since the device was last (re)started. A restart
    /// that finds this set is a guest that rebooted or reloaded its driver.
    served: bool,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    ///
    /// The guest driver rejects `num_gpus == 0`, so a host with no NVIDIA
    /// module loaded is refused here, where the reason can be stated, rather
    /// than in a guest as a bare -EINVAL from probe.
    fn new(
        proc_nvidia: &Path,
        allow_nearest_abi: bool,
        caps: Caps,
        vram_limit_mib: Option<u64>,
        display: Option<(DisplayMode, Arc<DisplayLink>, bool)>,
        input_target: EventTarget,
    ) -> anyhow::Result<Self> {
        let version = host::driver_version(proc_nvidia).ok_or_else(|| {
            anyhow::anyhow!(
                "no NVIDIA driver version at {} -- is the kernel module loaded?",
                proc_nvidia.display()
            )
        })?;
        let gpus = host::gpu_slots(proc_nvidia);
        anyhow::ensure!(
            !gpus.is_empty(),
            "driver {version} is loaded but owns no GPUs; the guest driver rejects an empty table"
        );
        log::info!("host driver {version}, {} GPU(s)", gpus.len());

        let mut nvidia = NvidiaBackend::with_default_zones();
        let release = abi::version::DriverVersion::parse(&version)
            .ok_or_else(|| anyhow::anyhow!("host driver version {version:?} does not parse"))?;
        nvidia
            .set_host_driver_version(release)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
        let inexact = nvidia.inexact_tables();
        if !inexact.is_empty() {
            let what = format!(
                "host driver {version} has no ABI tables of its own ({}); only an older release's",
                inexact.join(", ")
            );
            anyhow::ensure!(
                allow_nearest_abi,
                "refusing to start: {what}. A guest would fail at its first channel allocation. \
                 Install a supported driver release, update Conduit, or pass --allow-nearest-abi \
                 to try anyway"
            );
            log::warn!("{what}: starting anyway (--allow-nearest-abi)");
        }
        nvidia.set_caps(caps);
        nvidia
            .set_vram_limit_mib(vram_limit_mib)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;

        let nvidia_vram_mib = nvidia.vram_limit_mib();
        let mut config = VirtioGpuNvConfig::new(&version, &gpus, caps, nvidia_vram_mib);
        if let Some((mode, link, cursor)) = display {
            config.set_display(mode.width, mode.height, mode.refresh_hz);
            if cursor {
                config.set_cursor();
            }
            nvidia.set_display(link);
        }
        Ok(Self {
            nvidia: Arc::new(Mutex::new(nvidia)),
            mem: None,
            event_idx: false,
            // Phase A forwards ioctls only. nvidia-smi needs no mapping at all
            // -- 100 ioctls and one mmap in the captured trace -- so a guest
            // can enumerate the GPU before the shared window exists.
            config,
            watches: None,
            input_target,
            req: Vec::new(),
            resp: vec![0u8; RESP_MAX],
            served: false,
        })
    }

    /// Keep the event thread's poll set in step with the descriptors the
    /// backend has open, starting the thread on first use.
    ///
    /// Each descriptor is duplicated before it is handed over. The handle table
    /// owns the original and may close it at any time; a watch holding the same
    /// number would then be watching whatever opened next.
    fn sync_watches(&mut self, vrings: &[VringRwLock]) {
        let (added, removed) = self
            .nvidia
            .lock()
            .expect("backend mutex")
            .take_watch_updates();
        if added.is_empty() && removed.is_empty() && self.watches.is_some() {
            return;
        }

        if self.watches.is_none() {
            let (Some(mem), Some(vring)) = (self.mem.clone(), vrings.get(1).cloned()) else {
                return;
            };
            *self.input_target.lock().expect("event target") = Some((vring.clone(), mem.clone()));
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name("nvgpu-events".into())
                .spawn(move || event_pump(rx, vring, mem))
                .map(|_| self.watches = Some(tx))
                .unwrap_or_else(|e| log::error!("event pump would not start: {e}"));
        }
        let Some(tx) = self.watches.as_ref() else {
            return;
        };

        for (handle, fd) in added {
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                log::warn!(
                    "watch on handle {handle}: dup: {}",
                    std::io::Error::last_os_error()
                );
                continue;
            }
            let _ = tx.send(Watch::Add(handle, unsafe { OwnedFd::from_raw_fd(dup) }));
        }
        for handle in removed {
            let _ = tx.send(Watch::Remove(handle));
        }
    }

    /// Release everything the guest's previous boot held on the host.
    fn reset(&mut self, why: &str) {
        log::info!("{why}: resetting the device");
        self.nvidia.lock().expect("backend mutex").reset();
        self.served = false;
    }

    /// Drain one virtqueue, dispatching every chain.
    fn process(
        &mut self,
        vring: &VringRwLock,
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> std::io::Result<bool> {
        let mut used = false;
        loop {
            let mut guard = vring.get_mut();
            let Ok(mut avail) = guard.get_queue_mut().iter(mem.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else { break };
            drop(guard);

            let head = chain.head_index();
            let mut resp_desc = None;
            self.req.clear();

            for desc in chain.clone() {
                if desc.is_write_only() {
                    resp_desc = Some(desc);
                } else {
                    let at = self.req.len();
                    self.req.resize(at + desc.len() as usize, 0);
                    mem.read_slice(&mut self.req[at..], desc.addr())
                        .map_err(|e| {
                            std::io::Error::other(format!("read request descriptor: {e}"))
                        })?;
                }
            }

            let written = match resp_desc {
                Some(d) => {
                    let cap = std::cmp::min(d.len() as usize, RESP_MAX);
                    let resp = &mut self.resp[..cap];
                    let n = self
                        .nvidia
                        .lock()
                        .expect("backend mutex")
                        .dispatch(&self.req, resp);
                    if n > 0 {
                        mem.write_slice(&resp[..n], d.addr()).map_err(|e| {
                            std::io::Error::other(format!("write response descriptor: {e}"))
                        })?;
                    }
                    n
                }
                None => {
                    log::warn!("chain {head} has no writable descriptor; dropping");
                    0
                }
            };

            vring
                .add_used(head, written as u32)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            used = true;
            self.served = true;
        }
        Ok(used)
    }
}

impl VhostUserBackendMut for NvGpuBackend {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        QUEUE_COUNT
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE as usize
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_F_NOTIFY_ON_EMPTY)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            // The channel mapping requests travel up on, and the feature that
            // gates the request itself. Device memory has to be placed by the
            // VMM: `MAP_FIXED` here would rewrite only this process's page
            // tables, and the memory slot the guest reads through describes
            // the VMM's address space, not ours.
            | VhostUserProtocolFeatures::BACKEND_REQ
            | VhostUserProtocolFeatures::SHMEM
            // A frontend that resets the device on a guest reset says so,
            // and every host object of the previous boot is released.
            | VhostUserProtocolFeatures::RESET_DEVICE
    }

    fn reset_device(&mut self) {
        self.reset("RESET_DEVICE");
    }

    /// Features are set each time the frontend starts the device. QEMU's
    /// generic vhost-user device (`vhost-user-test-device-pci`) never sends
    /// RESET_DEVICE: on a guest reset it stops the rings and, when the guest
    /// boots again, starts the device anew -- which is where this is called.
    /// Having served requests before then means they came from the previous
    /// boot, whose files, RM objects and VRAM would otherwise stay held until
    /// this process exits.
    fn acked_features(&mut self, _features: u64) {
        if self.served {
            self.reset("device restarted (guest reboot or driver reload)");
        }
    }

    fn set_backend_req_fd(&mut self, backend: Backend) {
        log::info!("window: request channel open; device memory is now mappable");
        self.nvidia
            .lock()
            .unwrap()
            .set_window(Box::new(VhostWindow(backend)));
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        self.config.read(offset, size)
    }

    /// The regions a frontend lays out for us: QEMU >= 11.1 asks, because
    /// SHMEM is offered, and refuses the device if this goes unanswered.
    /// nesbox never asks; it has the same two sizes built in.
    fn get_shmem_config(&self) -> std::io::Result<VhostUserShMemConfig> {
        SPEC_SHMEM_FRONTEND.store(true, std::sync::atomic::Ordering::Relaxed);
        let window = self.nvidia.lock().expect("backend mutex").shm_total_size();
        let (n, sizes) = region_sizes(window, APERTURE_LEN);
        log::info!(
            "shared memory: window {} MiB (shmid {SHM_ID_WINDOW}), aperture {} MiB (shmid {SHM_ID_APERTURE})",
            sizes[SHM_ID_WINDOW as usize] >> 20,
            sizes[SHM_ID_APERTURE as usize] >> 20
        );
        Ok(VhostUserShMemConfig::new(n, &sizes))
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        // Also the backend's, so memory a guest registers by CPU address can
        // be found. The handle is atomic, so a later table replaces what this
        // one sees rather than leaving the backend on a stale set of regions.
        self.nvidia
            .lock()
            .unwrap()
            .set_guest_ram(Box::new(VhostGuestRam(mem.clone())));
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[VringRwLock],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        if device_event as usize >= QUEUE_COUNT {
            return Err(std::io::Error::other(format!(
                "event for unknown queue {device_event}"
            )));
        }
        // The event queue carries buffers the guest posted for *us* to fill, not
        // requests. Serving them as requests is a loop with no bottom: each one
        // is dispatched, answered with "unknown message", handed back filled,
        // re-posted by the guest, and kicked again -- 7.4 million times in five
        // seconds, measured, the first time two guests ran at once. The pump
        // thread owns this queue; a kick on it needs no work here.
        if device_event as usize == EVENT_QUEUE {
            return Ok(());
        }

        let mem = self
            .mem
            .as_ref()
            .ok_or_else(|| std::io::Error::other("guest memory not set"))?
            .memory();

        let vring = &vrings[device_event as usize];
        if self.event_idx {
            // With EVENT_IDX the guest suppresses notifications, so re-arm and
            // drain again rather than waiting for a kick that will not come.
            loop {
                vring.disable_notification().ok();
                self.process(vring, &mem)?;
                if !vring.enable_notification().unwrap_or(false) {
                    break;
                }
            }
        } else {
            self.process(vring, &mem)?;
        }
        // After serving, not before: a message that opened a descriptor has to
        // have been served for the backend to know about it.
        self.sync_watches(vrings);
        vring
            .signal_used_queue()
            .map_err(|e| std::io::Error::other(format!("signal used queue: {e}")))?;
        Ok(())
    }
}

/// The listening socket systemd passes on socket activation (`sd_listen_fds`:
/// `LISTEN_PID` is this process, `LISTEN_FDS` is 1, the socket is fd 3).
fn activated_listener() -> Option<vhost::vhost_user::Listener> {
    const SD_LISTEN_FDS_START: RawFd = 3;
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let n: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || n != 1 {
        return None;
    }
    // Only a listening unix stream socket will do.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on a plain fd number into a local buffer.
    if unsafe { libc::fstat(SD_LISTEN_FDS_START, &mut st) } != 0
        || st.st_mode & libc::S_IFMT != libc::S_IFSOCK
    {
        return None;
    }
    // SAFETY: fd 3 is ours: the service manager handed it over and nothing
    // else in this process owns it.
    Some(unsafe { vhost::vhost_user::Listener::from_raw_fd(SD_LISTEN_FDS_START) })
}

/// Like `VhostUserDaemon::serve`: a guest that quits mid-message is a normal end.
fn disconnect_is_ok(e: vhost_user_backend::Error) -> Result<(), vhost_user_backend::Error> {
    use vhost::vhost_user::Error as VuError;
    match e {
        vhost_user_backend::Error::HandleRequest(VuError::Disconnected)
        | vhost_user_backend::Error::HandleRequest(VuError::PartialMessage) => Ok(()),
        e => Err(e),
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    // Before the sandbox, which may not allow the fstat that checks it.
    let activated = activated_listener();
    // Before any device is opened: the host driver judges every guest call by
    // this process's credentials (device::posture).
    device::posture::enforce()?;

    // The sandbox, before anything else: Landlock and seccomp cover this
    // thread and every thread started after them, and `VhostUserDaemon::new`
    // starts one. Everything the backend needs afterwards -- the GPU nodes,
    // the driver's own trees, and the directory its socket is bound in -- is
    // named here, because a ruleset cannot be added to once it is in force.
    let socket_dir = Path::new(&args.socket)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let report = device::sandbox::enter(&device::sandbox::Paths {
        devices: device::sandbox::gpu_nodes(),
        read_only: vec![
            args.proc_nvidia.clone(),
            PathBuf::from("/sys/class/drm"),
            PathBuf::from("/sys/bus/pci/devices"),
            PathBuf::from("/sys/devices"),
        ],
        sockets: vec![socket_dir],
    })?;
    log::info!(
        "sandbox: landlock ABI {}, {} seccomp instructions; no path outside the GPU nodes, the \
         driver's own trees and the socket's directory, no executable mapping, no process, no \
         socket but AF_UNIX",
        report.landlock_abi,
        report.seccomp_rules
    );

    log::info!(
        "virtio-nvgpu vhost-user backend: device id {VIRTIO_ID_GPU_NV}, socket {}, caps {}, {}",
        args.socket,
        args.caps,
        match args.vram_limit_mib {
            Some(m) => format!("video memory limited to {m} MiB"),
            None => "no video memory limit".to_string(),
        }
    );

    // The display: a mode for device config, and a link to the broker with a
    // thread of its own that connects, reads input and reconnects. Started
    // after the sandbox, which must go on while this process is one thread.
    let input_target: EventTarget = Arc::new(Mutex::new(None));
    let display = if args.display.is_some() || args.display_socket.is_some() {
        let mode = args.display.unwrap_or(DisplayMode::DEFAULT);
        let link = DisplayLink::with_mode(args.display_socket.clone(), mode);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink = VqInputSink {
            target: input_target.clone(),
            warned_small: false,
            warned_clip_small: false,
            msg: Vec::new(),
        };
        if link.path().is_some() {
            let l = link.clone();
            std::thread::Builder::new()
                .name("nvgpu-display".into())
                .spawn(move || l.run(Box::new(sink), &stop))
                .map_err(|e| anyhow::anyhow!("display thread: {e}"))?;
        }
        let cursor = args.display_cursor == "on";
        log::info!(
            "display: {mode}, cursor plane {}, broker {}",
            if cursor { "on" } else { "off" },
            match &args.display_socket {
                Some(p) => p.display().to_string(),
                None => "none (flips are acked and dropped)".to_string(),
            }
        );
        Some((mode, link, cursor))
    } else {
        None
    };

    let backend = Arc::new(RwLock::new(NvGpuBackend::new(
        &args.proc_nvidia,
        args.allow_nearest_abi,
        args.caps,
        args.vram_limit_mib,
        display,
        input_target,
    )?));
    // vhost_user_backend::Error does not implement std::error::Error, so it
    // cannot ride `?` on its own.
    let mut daemon = VhostUserDaemon::new(
        "virtio-nvgpu".to_string(),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("create daemon: {e:?}"))?;

    match activated {
        // systemd (conduit-backend@NAME.socket) holds the listening socket and
        // started this process on QEMU's connection: serve that one
        // connection, then exit, so the backend lives exactly as long as the VM.
        Some(mut listener) => {
            log::info!("socket activation: serving the listener systemd passed in");
            daemon
                .start(&mut listener)
                .and_then(|()| daemon.wait())
                .or_else(disconnect_is_ok)
                .map_err(|e| anyhow::anyhow!("serve (socket activation): {e:?}"))?;
        }
        None => {
            let _ = std::fs::remove_file(&args.socket);
            daemon
                .serve(&args.socket)
                .map_err(|e| anyhow::anyhow!("serve {}: {e:?}", args.socket))?;
        }
    }

    backend
        .write()
        .expect("backend lock")
        .nvidia
        .lock()
        .expect("nvidia lock")
        .teardown();
    log::info!("backend exited");
    Ok(())
}

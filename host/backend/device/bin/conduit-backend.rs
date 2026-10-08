//! A vhost-user backend serving `NvidiaBackend` to a guest.
//!
//! The guest driver (`guest/linux/conduit_gpu.c`) binds virtio device ID 45 and
//! posts one descriptor chain per request: a readable descriptor holding the
//! request, and a writable one for the response. That is the whole transport.
//! Virtio allows either to be split across several descriptors, and another
//! guest may post a reply as a header buffer and a body buffer, so every
//! queue here reads and writes whole chains (`device::chain`).
//!
//! Attach it to QEMU >= 11.1 with the generic vhost-user device (named
//! `vhost-user-test-device-pci` there), which asks for the shared-memory
//! regions with `GET_SHMEM_CONFIG` (`device::shm_regions`):
//!
//! ```text
//! qemu-system-x86_64 \
//!   -chardev socket,id=nv,path=/tmp/nvgpu.sock \
//!   -device vhost-user-test-device-pci,chardev=nv,virtio-id=45,num_vqs=3,vq_size=256,config_size=4036 \
//!   -object memory-backend-memfd,id=mem,size=8G,share=on -machine q35,memory-backend=mem
//! ```
//!
//! Stock 11.1 caps vhost-user device config at 256 bytes; see
//! conduit/host/qemu/patches for the two patches this device needs.
//!
//! Guest memory must be shared (`memory-backend-memfd,share=on`) or the backend
//! cannot read the request the guest wrote.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use device::caps::Caps;
use device::chain::{
    ReadableAfterWritable, Segment, capacity, sort_chain, writable, write_scattered,
};
use device::display::{
    DisplayLink, DisplayMode, GuestInputClaims, InputSink, PresentedSink, ReleaseSink,
};
use device::host;
use device::nvidia::NvidiaBackend;
use device::shm::WindowPlacer;
#[cfg(not(feature = "venus"))]
use device::shm_regions::region_sizes;
use device::shm_regions::{
    APERTURE_LEN, SHM_ID_APERTURE, SHM_ID_WINDOW, page_align, pool_map_flags,
};
#[cfg(feature = "venus")]
use device::shm_regions::{SHM_ID_VENUS, region_sizes_with_venus};
use device::virtio::{CURSOR_QUEUE, NUM_QUEUES, QUEUE_SIZE, VIRTIO_ID_GPU_NV, VirtioGpuNvConfig};
use protocol::messages::{
    CLIPBOARD_MESSAGE_HEAD, CLIPBOARD_MIME_TEXT, ClipboardChunk, DISPLAY_MODE_MESSAGE_LEN,
    DisplayModeEvent, InputEventEntry, MsgHeader, MsgType, NVGPU_CFG_TAKES_INPUT,
    NVGPU_F_SCANOUT_PRESENTED, NVGPU_F_SCANOUT_RELEASE, SCANOUT_PRESENTED_MESSAGE_LEN,
    SCANOUT_RELEASED_MESSAGE_LEN, ScanoutPresented, ScanoutReleased, clipboard_mime,
    encode_clipboard_chunk, encode_display_mode, encode_input_events, encode_scanout_presented,
    encode_scanout_released, input_events_that_fit,
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
use virtio_queue::{QueueOwnedT, QueueT};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend,
    GuestMemoryLoadGuard, GuestMemoryMmap, GuestMemoryRegion,
};

/// The driver calls `virtio_find_vqs(vdev, 2, ...)` and fails probe on the
/// error from that call, so offering fewer is fatal before config is read. A
/// third, the cursor queue, is served when the VMM exposes it (`num_vqs=3`);
/// the Linux driver finds its two and ignores it.
const QUEUE_COUNT: usize = NUM_QUEUES + 1;
const _: () = assert!(CURSOR_QUEUE == NUM_QUEUES);
/// Largest response we will build for one request.
const RESP_MAX: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(version, about = "Conduit GPU backend (vhost-user)")]
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

    /// A display client's (broker's) socket. Frames go there as dma-bufs,
    /// input comes back. Implies --display. Give it more than once for
    /// several clients at the same time (the local viewer and a stream
    /// host); each is connected lazily and reconnected on its own. With no
    /// client listening (or none that wants frames), flips are acked and
    /// dropped without being exported.
    #[arg(long, value_name = "PATH")]
    display_socket: Vec<PathBuf>,

    /// Give the guest head a cursor plane whose image the viewer shows as the
    /// host pointer (zero-latency cursor). `off`: the guest compositor draws
    /// the cursor into its frames, as before.
    #[arg(long, value_name = "on|off", default_value = "on",
          value_parser = ["on", "off"])]
    display_cursor: String,

    /// The VM's emulated screen as a VNC server on this unix socket (QEMU
    /// `-vnc unix:PATH`): the boot console. Shown in the display clients,
    /// with the keyboard and pointer, whenever the guest's driver is not
    /// showing frames -- firmware setup, boot menu, disk password, early
    /// kernel output. Connected lazily and reconnected; needs --display or
    /// --display-socket. Frames go as shared memory (a viewer with
    /// --present-mode=auto or shm shows them).
    #[arg(long, value_name = "PATH")]
    console_vnc: Option<PathBuf>,

    /// Start even when the host driver release has no ABI tables of its own,
    /// using the nearest older release's. Expect guests to fail at their first
    /// channel allocation: the allowlist refuses every class whose size
    /// changed between releases.
    #[arg(long)]
    allow_nearest_abi: bool,

    /// Record every guest request to this file (docs/TRACING.md). JSON Lines,
    /// or the binary format for a name ending in .bin or with
    /// --trace-format bin. Also taken from CONDUIT_TRACE. SIGUSR1 pauses and
    /// resumes it.
    #[cfg(feature = "trace")]
    #[arg(long, value_name = "PATH")]
    trace: Option<PathBuf>,

    /// json or bin. Default: from the file name (.bin is binary), else json.
    /// Also taken from CONDUIT_TRACE_FORMAT.
    #[cfg(feature = "trace")]
    #[arg(long, value_name = "json|bin", value_parser = ["json", "bin"])]
    trace_format: Option<String>,

    /// A control socket for tracing a running backend: `conduit trace NAME`
    /// connects to it to stream records live. Must be in a directory the
    /// backend can create sockets in.
    #[cfg(feature = "trace")]
    #[arg(long, value_name = "PATH")]
    trace_socket: Option<PathBuf>,

    /// Size of the window (shared memory region 1), where every guest CPU
    /// mapping of RM memory is placed, in MiB: a power of two from 32 to
    /// 4194304, or `auto`, the host GPU's BAR1 (as Resizable BAR on bare
    /// metal) within what the guest's 64-bit MMIO window holds, 4096 when
    /// there is no GPU to go by. Address space, not memory. conduit-vmm's
    /// `gpu-forward.window-mib` must be the same number (QEMU asks;
    /// conduit-vmm's BAR is configured); `--print-window-mib` tells it.
    #[arg(long, value_name = "auto|MIB", default_value = "auto")]
    window_mib: device::shm_regions::WindowMib,

    /// Print the window size `--window-mib` comes to on this host, in MiB,
    /// and exit (`conduit up` gives that number to the backend and to
    /// conduit-vmm alike).
    #[arg(long)]
    print_window_mib: bool,

    /// Serve Venus to a Windows guest (docs/VENUS.md): sets the config bit,
    /// answers GpuCmd and advertises shared memory region 3. Needs a frontend
    /// that asks for the region table (QEMU); conduit-vmm's BARs are fixed.
    #[cfg(feature = "venus")]
    #[arg(long)]
    venus: bool,

    /// Size of region 3, where host-visible Venus blobs are mapped, in MiB.
    /// A power of two.
    #[cfg(feature = "venus")]
    #[arg(long, value_name = "MIB",
          default_value_t = device::shm_regions::VENUS_HOSTMEM_MIB_DEFAULT)]
    venus_hostmem_mib: u64,

    /// The conduit-venus renderer's socket. Required with --venus.
    #[cfg(feature = "venus")]
    #[arg(long, value_name = "PATH")]
    venus_renderer: Option<PathBuf>,

    /// Serve guest-memory blobs (docs/VENUS.md "Guest-memory blobs"): Venus
    /// resources over the guest's own pages, which the host GPU copies
    /// into directly. Sets config bit NVGPU_CFG_GUEST_BLOB when the renderer
    /// can import host memory. Opt-in while new.
    #[cfg(feature = "venus")]
    #[arg(long, requires = "venus")]
    venus_guest_blobs: bool,
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
/// conduit-vmm never asks. One process serves one VM, and a device reset keeps
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

    #[cfg(feature = "venus")]
    fn place_blob(&self, offset: u64, len: u64, fd: RawFd) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: SHM_ID_VENUS,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: offset,
            len: page_align(len),
            flags: VhostUserMMapFlags::WRITABLE.bits(),
        };
        // SAFETY: the descriptor is owned by the Venus resource for the whole
        // of this call, and is only borrowed to be sent.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    #[cfg(feature = "venus")]
    fn withdraw_blob(&self, offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: SHM_ID_VENUS,
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
    /// A descriptor, and whether it is a fence: reported once, with its
    /// status, then dropped from the set (docs/SYNC.md).
    Add(u32, OwnedFd, bool),
    Remove(u32),
}

/// One `EventReady` message: a bare header naming the descriptor. `status`
/// is 0, or a fence's error as a negative errno.
fn event_ready_bytes(handle: u32, status: i32) -> Vec<u8> {
    let mut hdr = MsgHeader::ok(MsgType::EventReady, handle);
    hdr.status = status;
    // The wire form is the struct's bytes, which is what the driver reads.
    let p = &hdr as *const MsgHeader as *const u8;
    unsafe { std::slice::from_raw_parts(p, size_of::<MsgHeader>()) }.to_vec()
}

/// A signalled sync_file's outcome: 0, or its error as a negative errno (a
/// host fence that timed out is `-ETIMEDOUT`).
fn sync_file_status(fd: RawFd) -> i32 {
    sync_file_raw_status(fd).map_or(0, |s| s.min(0))
}

/// `sync_file_info.status` as the kernel reports it: 1 signalled, 0 pending,
/// negative an error. `None` for a descriptor that is not a sync_file.
/// `SYNC_IOC_FILE_INFO` with no fence array asks for the status alone.
fn sync_file_raw_status(fd: RawFd) -> Option<i32> {
    // struct sync_file_info: char name[32]; s32 status; u32 flags;
    // u32 num_fences; u32 pad; u64 sync_fence_info.
    const SYNC_IOC_FILE_INFO: u64 = (3 << 30) | (56 << 16) | ((b'>' as u64) << 8) | 4;
    let mut info = [0u8; 56];
    // SAFETY: a live 56-byte buffer, the size the request declares.
    let rc = unsafe { libc::ioctl(fd, SYNC_IOC_FILE_INFO as libc::Ioctl, info.as_mut_ptr()) };
    (rc == 0).then(|| i32::from_le_bytes(info[32..36].try_into().expect("4 bytes")))
}

/// Put one message on the event queue, into a buffer the guest posted there.
///
/// Returns false when the guest has posted none, which is the normal state of
/// a guest whose driver predates this queue having a use -- and a reason to
/// drop the notification rather than to fail.
fn push_event(
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    handle: u32,
    status: i32,
) -> bool {
    #[cfg(feature = "trace")]
    if device::trace::enabled() {
        return push_event_traced(vring, mem, handle, status);
    }
    deliver_event(vring, mem, handle, status)
}

/// `push_event`, recorded: an `event` record whose latency is the time it
/// took to hand the notification to the guest. One the guest had no buffer
/// posted for is recorded with errno ENOBUFS.
#[cfg(feature = "trace")]
#[cold]
fn push_event_traced(
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    handle: u32,
    status: i32,
) -> bool {
    use device::trace::format::{Call, Kind, Record};
    let t0 = device::trace::now_ns();
    let delivered = deliver_event(vring, mem, handle, status);
    device::trace::emit(Record {
        ts_ns: t0,
        handle,
        kind: Kind::Event,
        call: Call::Event,
        errno: if delivered { 0 } else { libc::ENOBUFS },
        reply_ns: device::trace::now_ns().saturating_sub(t0),
        ..Default::default()
    });
    delivered
}

fn deliver_event(
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    handle: u32,
    status: i32,
) -> bool {
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

    // Across every writable buffer: a guest may post the event buffer split
    // in two (header, body), where the Linux guest posts one.
    let bytes = event_ready_bytes(handle, status);
    let written = write_scattered(&*guard, &writable(chain), &bytes).unwrap_or(0);

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
    /// The guest has posted event-queue buffers since the queue last
    /// started (see `takes_input`).
    posted: bool,
    /// What the guest's driver said about taking input, learnt on the
    /// request path.
    claims: Arc<GuestInputClaims>,
}

/// Whether the guest has posted buffers on the event queue since it last
/// started it: the queue is running and the guest's avail index has moved
/// (or the device has consumed from it). `latch` is the answer so far; it is
/// kept while the queue runs, so a guest that has every buffer out at the
/// moment still counts, and cleared when the queue stops (a reset or reboot:
/// the next driver has to post again).
fn event_buffers_posted(
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    latch: bool,
) -> bool {
    let vr = vring.get_ref();
    let q = vr.get_queue();
    if !q.ready() || q.avail_ring() == 0 {
        return false;
    }
    latch
        || q.next_avail() != 0
        || q.avail_idx(&*mem.memory(), std::sync::atomic::Ordering::Acquire)
            .is_ok_and(|i| i.0 != 0)
}

/// Clipboard chunks put on the event queue per call: buffers are shared with
/// input, which must not starve behind a large paste.
const CLIP_CHUNKS_PER_PASS: usize = 16;

impl InputSink for VqInputSink {
    /// The guest takes `InputEvent`s if its driver declares it (acks
    /// `NVGPU_CFG_TAKES_INPUT`, or is the Linux module from before the bit)
    /// and posts event-queue buffers (`device::display::guest_takes_input`).
    /// The Linux guest does both at probe, long before its first frame. The
    /// Windows KMD may run the event queue (for `EventReady`) but never acks
    /// the bit, and has nowhere to put an `InputEvent`.
    fn takes_input(&mut self) -> bool {
        let target = self.target.lock().expect("event target").clone();
        self.posted = match target {
            Some((vring, mem)) => event_buffers_posted(&vring, &mem, self.posted),
            // No guest request served yet: nothing posted that we know of.
            None => false,
        };
        self.claims.takes_input(self.posted)
    }

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

            // Sized to all its writable buffers together, which a guest may
            // post split (header, body) where the Linux guest posts one.
            let segs = writable(chain);
            let mut written = 0u32;
            if !segs.is_empty() {
                let fit = input_events_that_fit(capacity(&segs));
                if fit == 0 {
                    // A driver whose event buffers hold only a header cannot
                    // take input at all; say so once and drop it.
                    if !self.warned_small {
                        log::warn!(
                            "display: guest event buffers are {} bytes, too small for input; input dropped (driver predates InputEvent?)",
                            capacity(&segs)
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
                    if let Ok(w) = write_scattered(&*guard, &segs, &self.msg[..n]) {
                        written = w as u32;
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
        let segs = writable(chain);
        let mut written = 0u32;
        if capacity(&segs) >= DISPLAY_MODE_MESSAGE_LEN {
            let mut msg = [0u8; DISPLAY_MODE_MESSAGE_LEN];
            let n = encode_display_mode(m, &mut msg).expect("sized for it");
            if let Ok(w) = write_scattered(&*guard, &segs, &msg[..n]) {
                written = w as u32;
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
            let segs = writable(chain);
            let mut written = 0u32;
            if !segs.is_empty() {
                let room = capacity(&segs).saturating_sub(CLIPBOARD_MESSAGE_HEAD);
                if room == 0 {
                    if !self.warned_clip_small {
                        log::warn!(
                            "display: guest event buffers are {} bytes, too small for clipboard; host clipboard dropped (driver predates ClipboardFromHost?)",
                            capacity(&segs)
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
                    if let Ok(w) = write_scattered(&*guard, &segs, &self.msg[..n]) {
                        written = w as u32;
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

/// `ScanoutReleased` onto the event queue, one message per buffer posted
/// (docs/SCANOUT.md "Buffer release"). Shares the queue with input and
/// `EventReady`; a guest that posts no buffer gets them later.
struct VqReleaseSink {
    target: EventTarget,
    warned_small: std::sync::atomic::AtomicBool,
}

impl ReleaseSink for VqReleaseSink {
    fn released(&self, r: &[ScanoutReleased]) -> usize {
        let Some((vring, mem)) = self.target.lock().expect("event target").clone() else {
            // No guest yet: nobody waits for anything.
            return r.len();
        };
        let guard = mem.memory();
        let mut done = 0;
        let mut signalled = false;
        while done < r.len() {
            let mut vr = vring.get_mut();
            let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else {
                break; // no buffer posted; the rest is retried shortly
            };
            let head = chain.head_index();
            drop(vr);
            let segs = writable(chain);
            let mut written = 0u32;
            if capacity(&segs) >= SCANOUT_RELEASED_MESSAGE_LEN {
                let mut msg = [0u8; SCANOUT_RELEASED_MESSAGE_LEN];
                let n = encode_scanout_released(&r[done], &mut msg).expect("sized for it");
                if let Ok(w) = write_scattered(&*guard, &segs, &msg[..n]) {
                    written = w as u32;
                }
            } else if !self
                .warned_small
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                log::warn!(
                    "display: guest event buffers are {} bytes, too small for ScanoutReleased; dropped",
                    capacity(&segs)
                );
            }
            done += 1;
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
}

/// `ScanoutPresented` onto the same event queue: one message into one posted
/// buffer, or dropped when none is posted (a presentation report is stale
/// within a frame; the guest falls back to its own timer for that flip).
impl PresentedSink for VqReleaseSink {
    fn presented(&self, p: &ScanoutPresented) -> bool {
        let Some((vring, mem)) = self.target.lock().expect("event target").clone() else {
            return false;
        };
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
        let segs = writable(chain);
        let mut written = 0u32;
        if capacity(&segs) >= SCANOUT_PRESENTED_MESSAGE_LEN {
            let mut msg = [0u8; SCANOUT_PRESENTED_MESSAGE_LEN];
            let n = encode_scanout_presented(p, &mut msg).expect("sized for it");
            if let Ok(w) = write_scattered(&*guard, &segs, &msg[..n]) {
                written = w as u32;
            }
        } else if !self
            .warned_small
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            log::warn!(
                "display: guest event buffers are {} bytes, too small for ScanoutPresented; dropped",
                capacity(&segs)
            );
        }
        if vring.add_used(head, written).is_err() {
            return false;
        }
        let _ = vring.signal_used_queue();
        written != 0
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
    // How soon the sweep may report a descriptor again after it was last
    // reported. The sweep exists for the report that never arrived -- an edge
    // missed on add, a notification that found no buffer -- and those are
    // still found within SWEEP. A descriptor the guest *was* told about
    // stays readable until the guest drains it with an ioctl, and re-sending
    // it every SWEEP was an EventReady interrupt per millisecond per idle
    // event file for as long as nobody drained it. Its repeat is the safety
    // net for an event the guest woke for and then left queued, which still
    // finds it within this.
    const REPEAT: Duration = Duration::from_millis(10);

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
    // Fences: reported once and then forgotten, since a signalled sync_file
    // never stops being readable and the sweep would re-send it every pass.
    let mut once: HashSet<u64> = HashSet::new();
    // When each descriptor was last reported (REPEAT).
    let mut last_report: HashMap<u64, Instant> = HashMap::new();
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
                Ok(Watch::Add(handle, fd, fence)) => {
                    if ctl(libc::EPOLL_CTL_ADD, fd.as_raw_fd(), handle) == 0 {
                        // Edge-triggered misses a descriptor that is already
                        // readable when it is added: a fence made for a value
                        // the GPU has passed signals before it gets here.
                        // Ask it once now rather than leave it to the sweep,
                        // which costs up to a millisecond per such fence.
                        let mut pfd = libc::pollfd {
                            fd: fd.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        let ready = unsafe { libc::poll(&mut pfd, 1, 0) } > 0
                            && pfd.revents & libc::POLLIN != 0;
                        if ready && fence {
                            // Reported once and never watched, as the sweep
                            // would do for a signalled fence.
                            let status = sync_file_status(fd.as_raw_fd());
                            if push_event(&vring, &mem, handle, status) {
                                ctl(libc::EPOLL_CTL_DEL, fd.as_raw_fd(), handle);
                                continue;
                            }
                        }
                        if ready && push_event(&vring, &mem, handle, 0) {
                            last_report.insert(handle as u64, Instant::now());
                        }
                        watched.insert(handle as u64, fd);
                        if fence {
                            once.insert(handle as u64);
                        }
                    }
                }
                Ok(Watch::Remove(handle)) => {
                    once.remove(&(handle as u64));
                    last_report.remove(&(handle as u64));
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
        // posted. Every millisecond, ask the descriptors directly and
        // re-notify the ones that still have something to say and were not
        // told within REPEAT. A lost wake costs a sixteenth of a frame at
        // 60 Hz rather than a hang.
        let mut reported: Vec<u64> = Vec::new();
        if last_sweep.elapsed() >= SWEEP {
            let now = Instant::now();
            last_sweep = now;
            for (&handle, fd) in watched.iter() {
                if last_report
                    .get(&handle)
                    .is_some_and(|&t| now.duration_since(t) < REPEAT)
                {
                    continue;
                }
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 && pfd.revents & libc::POLLIN != 0 {
                    let status = if once.contains(&handle) {
                        sync_file_status(fd.as_raw_fd())
                    } else {
                        0
                    };
                    if push_event(&vring, &mem, handle as u32, status) {
                        if once.contains(&handle) {
                            reported.push(handle);
                        } else {
                            last_report.insert(handle, now);
                        }
                    }
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
            let fence = once.contains(&(handle as u64));
            // Already sent by the sweep above: one EventReady per fence.
            if fence && reported.contains(&(handle as u64)) {
                continue;
            }
            let status = match watched.get(&(handle as u64)) {
                Some(fd) if fence => sync_file_status(fd.as_raw_fd()),
                _ => 0,
            };
            if !push_event(&vring, &mem, handle, status) {
                // A fence stays in the set, and the sweep sends it again.
                log::debug!("event pump: no buffer posted for handle {handle}; dropped");
            } else if fence {
                reported.push(handle as u64);
            } else {
                last_report.insert(handle as u64, Instant::now());
            }
        }
        // A fence the guest has heard about is done here. Its descriptor
        // stays open until the guest closes the handle.
        for handle in reported {
            once.remove(&handle);
            if let Some(fd) = watched.remove(&handle) {
                ctl(libc::EPOLL_CTL_DEL, fd.as_raw_fd(), handle as u32);
            }
        }
    }
}

/// The `seq` of a `ScanoutFlip` request (`MsgHeader` then the flip), for
/// stage timing; `None` for any other message.
fn flip_seq(req: &[u8]) -> Option<u64> {
    let hdr = size_of::<protocol::messages::MsgHeader>();
    if req.get(..4)? != (MsgType::ScanoutFlip as u32).to_le_bytes() {
        return None;
    }
    protocol::messages::ScanoutFlip::from_bytes(req.get(hdr..)?).map(|f| f.seq)
}

/// Return held chains whose fences have signalled: write each response and
/// put the chain on the used ring. Returns whether any was.
#[cfg(feature = "venus")]
fn deliver_completions(
    nvidia: &Mutex<NvidiaBackend>,
    held: &HeldChains,
    vring: &VringRwLock,
    mem: &GuestMemoryMmap,
) -> bool {
    let done = nvidia.lock().expect("backend mutex").venus_completions();
    if done.is_empty() {
        return false;
    }
    let mut chains = held.lock().expect("held chains");
    let mut any = false;
    let staged = device::stage::on();
    let mut used: Vec<(u32, u32, u64)> = Vec::new();
    for c in done {
        let Some(chain) = chains.remove(&c.token) else {
            continue; // its queue was reset meanwhile
        };
        // Capped as the control queue caps a response it builds itself.
        let n = c.resp.len().min(capacity(&chain.resp)).min(RESP_MAX);
        if let Err(e) = write_scattered(mem, &chain.resp, &c.resp[..n]) {
            log::warn!("venus: writing the response of chain {}: {e}", chain.head);
        }
        if let Err(e) = vring.add_used(chain.head, n as u32) {
            log::warn!("venus: returning chain {}: {e}", chain.head);
            continue;
        }
        if staged {
            let (ctx, ring, id) = c.fence;
            device::stage::stamp(device::stage::Rec::fence(
                device::stage::H_USED,
                ctx,
                ring,
                id,
                device::stage::now_ns(),
            ));
            used.push(c.fence);
        }
        any = true;
    }
    drop(chains);
    if any {
        let _ = vring.signal_used_queue();
    }
    if !used.is_empty() {
        let now = device::stage::now_ns();
        for (ctx, ring, id) in used {
            device::stage::stamp(device::stage::Rec::fence(
                device::stage::H_IRQ,
                ctx,
                ring,
                id,
                now,
            ));
        }
    }
    any
}

/// Wait on the renderer's fence descriptor and return chains as their
/// fences signal (docs/VENUS.md "Fences"). Started with `--venus` only, once
/// the control queue is up.
///
/// The descriptor is what wakes this; the timeout is a safety net for a
/// wake that came while the backend lock was held elsewhere, short while
/// chains are waiting and long while none are.
///
/// It is also how renderer death is found between commands (docs/VENUS.md
/// "Reset and close"): the renderer reports it through the same descriptor,
/// Venus releases everything and fails every held chain `RESP_ERR_UNSPEC`,
/// which this returns, and from then on refuses every `GpuCmd`. No fence
/// will ever signal again, so the pump then stops; chains failed later
/// (none are held once the renderer is lost) go back from the queue handler.
#[cfg(feature = "venus")]
fn fence_pump(
    fd: OwnedFd,
    nvidia: Arc<Mutex<NvidiaBackend>>,
    held: HeldChains,
    vring: VringRwLock,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
) {
    loop {
        let waiting = !held.lock().expect("held chains").is_empty();
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, if waiting { 2 } else { 100 }) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("fence pump: poll: {err}");
            return;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            log::error!("fence pump: the renderer's fence descriptor is gone");
            return;
        }
        let delivered = deliver_completions(&nvidia, &held, &vring, &mem.memory());
        if nvidia.lock().expect("backend mutex").venus_lost() {
            log::error!(
                "fence pump: the renderer is gone; held chains failed, GpuCmd refused from now on"
            );
            return;
        }
        // Readable with nothing to deliver: do not spin on it.
        if n > 0 && !delivered {
            std::thread::sleep(Duration::from_millis(1));
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
    /// Whether the guest takes display input: set from the acked features
    /// and the requests served, read by the input sink.
    input_claims: Arc<GuestInputClaims>,
    /// The display, when there is one: told whether the guest wants
    /// `ScanoutReleased` at every device start and reset.
    display_link: Option<Arc<DisplayLink>>,
    /// The request and response of the chain being served, kept across chains.
    /// A fresh 64 KiB response zeroed per request cost more than the host's
    /// whole RM call, and only the bytes dispatch writes are sent back.
    req: Vec<u8>,
    resp: Vec<u8>,
    /// The chain's readable and writable buffers, kept across chains for the
    /// same reason.
    readable: Vec<Segment>,
    writable: Vec<Segment>,
    /// A request was served since the device was last (re)started. A restart
    /// that finds this set is a guest that rebooted or reloaded its driver.
    served: bool,
    /// Requests served so far (every queue), and those of the cursor queue (virtqueue 2): the
    /// evidence that a guest's cursor commands travel there (`cursor queue:` log lines).
    served_n: u64,
    cursor_q_n: u64,
    /// The backend has descriptors for the event thread to start or stop
    /// watching. Noted under the lock each request already takes, so a
    /// drain that opened and closed nothing -- nearly all of them -- does not
    /// lock the backend again to find that out.
    watch_dirty: bool,
    /// The drain just served a `GpuCmd`, so a fence may be ready to return
    /// at once. Nothing else can complete a held chain, and the fence pump
    /// returns those anyway; asking after every RM call cost an eventfd read
    /// and two more locks of the backend.
    #[cfg(feature = "venus")]
    gpu_cmd_seen: bool,
    /// Stage timing: `ScanoutFlip`s put on the used ring by this drain, by
    /// `seq`, stamped `H_IRQ` once the guest has been notified. Empty while
    /// stage timing is off.
    flips_used: Vec<u64>,
    /// Venus, with `--venus`: the size of region 3 (0 without), and the
    /// fenced chains waiting for their fences.
    #[cfg(feature = "venus")]
    venus: VenusChains,
}

/// A fenced `GpuCmd` chain, kept off the used ring until its fence signals
/// (docs/VENUS.md "Fences"): where its response goes, every writable buffer
/// of the chain in order.
#[cfg(feature = "venus")]
struct HeldChain {
    head: u16,
    resp: Vec<Segment>,
}

#[cfg(feature = "venus")]
type HeldChains = Arc<Mutex<HashMap<u64, HeldChain>>>;

#[cfg(feature = "venus")]
#[derive(Default)]
struct VenusChains {
    /// Region 3's size; 0 is no Venus, and then nothing below is used.
    hostmem_len: u64,
    held: HeldChains,
    /// The fence pump is running.
    pump: bool,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    ///
    /// The guest driver rejects `num_gpus == 0`, so a host with no NVIDIA
    /// module loaded is refused here, where the reason can be stated, rather
    /// than in a guest as a bare -EINVAL from probe.
    #[allow(clippy::too_many_arguments)]
    fn new(
        proc_nvidia: &Path,
        allow_nearest_abi: bool,
        caps: Caps,
        vram_limit_mib: Option<u64>,
        window_len: u64,
        display: Option<(DisplayMode, Arc<DisplayLink>, bool)>,
        input_target: EventTarget,
        input_claims: Arc<GuestInputClaims>,
        safe_mode: bool,
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

        let mut nvidia = NvidiaBackend::new(device::shm::ZoneConfig::for_window(window_len));
        let release = abi::version::DriverVersion::parse(&version)
            .ok_or_else(|| anyhow::anyhow!("host driver version {version:?} does not parse"))?;
        nvidia
            .set_host_driver_version(release)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
        let inexact = nvidia.inexact_tables();
        if !inexact.is_empty() {
            let what = format!(
                "host driver {version} has no ABI tables of its own ({}); the GET_DEV_INFO and NVKMS \
                 tables have no older release's to fall back on (their layouts are not \
                 monotonic), the rest use the nearest older one's",
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
        nvidia.set_safe_mode(safe_mode);
        nvidia
            .set_vram_limit_mib(vram_limit_mib)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;

        let nvidia_vram_mib = nvidia.vram_limit_mib();
        let mut config = VirtioGpuNvConfig::new(&version, &gpus, caps, nvidia_vram_mib);
        // The event pump below reports fence handles once (docs/SYNC.md).
        config.set_drm_fences();
        let mut display_link = None;
        if let Some((mode, link, cursor)) = display {
            config.set_display(mode.width, mode.height, mode.refresh_hz);
            if cursor {
                config.set_cursor();
            }
            let sink = Arc::new(VqReleaseSink {
                target: input_target.clone(),
                warned_small: std::sync::atomic::AtomicBool::new(false),
            });
            link.set_release_sink(sink.clone());
            link.set_presented_sink(sink);
            display_link = Some(link.clone());
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
            input_claims,
            display_link,
            req: Vec::new(),
            resp: vec![0u8; RESP_MAX],
            readable: Vec::new(),
            writable: Vec::new(),
            served: false,
            served_n: 0,
            cursor_q_n: 0,
            watch_dirty: true,
            #[cfg(feature = "venus")]
            gpu_cmd_seen: false,
            flips_used: Vec::new(),
            #[cfg(feature = "venus")]
            venus: VenusChains::default(),
        })
    }

    /// Serve Venus (`--venus`): the config bit, `GpuCmd`, and region 3 of
    /// `hostmem_len` bytes.
    #[cfg(feature = "venus")]
    fn enable_venus(
        &mut self,
        renderer: Box<dyn conduit_venus::Renderer>,
        hostmem_len: u64,
        guest_blobs: bool,
    ) {
        let display = ({ self.config.features } & protocol::messages::NVGPU_CFG_DISPLAY != 0)
            .then_some(device::display::DisplayMode {
                width: self.config.display_width,
                height: self.config.display_height,
                refresh_hz: self.config.display_refresh_hz,
            });
        self.config.set_venus();
        let mut venus = device::venus::Venus::new(renderer, hostmem_len, display);
        if venus.rm_import() {
            self.config.set_rm_import();
        }
        if guest_blobs && venus.enable_guest_blobs() {
            self.config.set_guest_blob();
        }
        // A Windows guest's hardware cursor rides the cursor plane's path.
        if { self.config.features } & protocol::messages::NVGPU_CFG_CURSOR != 0
            && venus.enable_cursor()
        {
            self.config.set_venus_cursor();
            // ...on its own queue, served ahead of the control queue's Venus
            // traffic (it only works where the VMM has `num_vqs=3`; the guest
            // checks that the queue exists).
            self.config.set_cursor_queue();
        }
        self.nvidia.lock().expect("backend mutex").set_venus(venus);
        self.venus.hostmem_len = hostmem_len;
    }

    /// Return every held chain whose fence has signalled, and start the
    /// thread that does so on its own once there is a queue to return them
    /// to. Inert without `--venus`.
    #[cfg(feature = "venus")]
    fn venus_complete(&mut self, vring: &VringRwLock, mem: &GuestMemoryMmap) {
        if self.venus.hostmem_len == 0 {
            return;
        }
        if !std::mem::take(&mut self.gpu_cmd_seen) && self.venus.pump {
            return;
        }
        deliver_completions(&self.nvidia, &self.venus.held, vring, mem);
        if self.venus.pump {
            return;
        }
        let (Some(atomic), Some(fd)) = (
            self.mem.clone(),
            self.nvidia.lock().expect("backend mutex").venus_fence_fd(),
        ) else {
            return;
        };
        let (nvidia, held, vring) = (self.nvidia.clone(), self.venus.held.clone(), vring.clone());
        std::thread::Builder::new()
            .name("nvgpu-fences".into())
            .spawn(move || fence_pump(fd, nvidia, held, vring, atomic))
            .map(|_| self.venus.pump = true)
            .unwrap_or_else(|e| log::error!("fence pump would not start: {e}"));
    }

    /// Keep the event thread's poll set in step with the descriptors the
    /// backend has open, starting the thread on first use.
    ///
    /// Each descriptor is duplicated before it is handed over. The handle table
    /// owns the original and may close it at any time; a watch holding the same
    /// number would then be watching whatever opened next.
    fn sync_watches(&mut self, vrings: &[VringRwLock]) {
        if !std::mem::take(&mut self.watch_dirty) && self.watches.is_some() {
            return;
        }
        let (added, removed, fences) = {
            let mut nvidia = self.nvidia.lock().expect("backend mutex");
            let (added, removed) = nvidia.take_watch_updates();
            (added, removed, nvidia.take_fence_watches())
        };
        if added.is_empty() && removed.is_empty() && fences.is_empty() && self.watches.is_some() {
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

        // Adds before removes: a fence made and closed between two passes
        // must not be left watched.
        let all = added
            .into_iter()
            .map(|(h, fd)| (h, fd, false))
            .chain(fences.into_iter().map(|(h, fd)| (h, fd, true)));
        for (handle, fd, fence) in all {
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                log::warn!(
                    "watch on handle {handle}: dup: {}",
                    std::io::Error::last_os_error()
                );
                continue;
            }
            let _ = tx.send(Watch::Add(
                handle,
                unsafe { OwnedFd::from_raw_fd(dup) },
                fence,
            ));
        }
        for handle in removed {
            let _ = tx.send(Watch::Remove(handle));
        }
    }

    /// Release everything the guest's previous boot held on the host.
    fn reset(&mut self, why: &str) {
        log::info!("{why}: resetting the device");
        self.nvidia.lock().expect("backend mutex").reset();
        // A reset closes every descriptor, and the event thread must hear.
        self.watch_dirty = true;
        // Held chains belonged to the queue the frontend just reset.
        #[cfg(feature = "venus")]
        self.venus.held.lock().expect("held chains").clear();
        self.served = false;
        // A new driver: its first cursor-queue request is logged again.
        self.cursor_q_n = 0;
        self.input_claims.reset();
        if let Some(link) = self.display_link.as_ref() {
            link.set_release_enabled(false);
            link.set_presented_enabled(false);
        }
    }

    /// Drain one virtqueue, dispatching every chain.
    ///
    /// Whether to trace is decided here, once per drain, and the drain itself
    /// is compiled twice: with tracing off it is exactly the untraced loop,
    /// with no per-request check at all (docs/TRACING.md).
    /// Serve `vring`. With `prio` (the cursor queue, when `vring` is the
    /// control queue), `prio` is served first and again after every control
    /// request, so a cursor command waits for at most one control request,
    /// never for the queue's whole backlog of Venus traffic.
    fn process(
        &mut self,
        vring: &VringRwLock,
        prio: Option<&VringRwLock>,
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> std::io::Result<bool> {
        #[cfg(feature = "trace")]
        if device::trace::enabled() {
            return self.drain::<true>(vring, prio, mem);
        }
        self.drain::<false>(vring, prio, mem)
    }

    /// Serve the cursor queue now, if it has anything, and tell the guest.
    fn serve_prio<const TRACE: bool>(
        &mut self,
        prio: Option<&VringRwLock>,
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> std::io::Result<()> {
        let before = self.served_n;
        if let Some(p) = prio
            && self.drain::<TRACE>(p, None, mem)?
        {
            p.signal_used_queue()
                .map_err(|e| std::io::Error::other(format!("signal used queue: {e}")))?;
        }
        self.note_cursor_q(self.served_n - before);
        Ok(())
    }

    /// `n` more requests were served on the cursor queue: log the first, then every 1000th.
    fn note_cursor_q(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        let was = self.cursor_q_n;
        self.cursor_q_n += n;
        if was == 0 {
            log::info!("cursor queue: first request served (virtqueue 2)");
        } else if was / 1000 != self.cursor_q_n / 1000 {
            log::info!(
                "cursor queue: {} requests served (virtqueue 2)",
                self.cursor_q_n
            );
        }
    }

    fn drain<const TRACE: bool>(
        &mut self,
        vring: &VringRwLock,
        prio: Option<&VringRwLock>,
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> std::io::Result<bool> {
        self.serve_prio::<TRACE>(prio, mem)?;
        let mut used = false;
        loop {
            let mut guard = vring.get_mut();
            let Ok(mut avail) = guard.get_queue_mut().iter(mem.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else { break };
            drop(guard);
            #[cfg(feature = "trace")]
            let t_recv = if TRACE { device::trace::now_ns() } else { 0 };
            #[cfg(feature = "trace")]
            let mut record = None;

            let head = chain.head_index();
            self.req.clear();

            // The request may span several readable descriptors and the
            // response several writable ones (virtio allows either; the
            // Linux guest posts one of each), readable first.
            let layout = sort_chain(chain.clone(), &mut self.readable, &mut self.writable);
            if layout.is_ok() {
                for &(addr, len) in &self.readable {
                    let at = self.req.len();
                    self.req.resize(at + len as usize, 0);
                    mem.read_slice(&mut self.req[at..], addr).map_err(|e| {
                        std::io::Error::other(format!("read request descriptor: {e}"))
                    })?;
                }
            }

            // The Linux module identifies itself by what it asks for (input
            // routing, `GuestInputClaims`).
            if let Some(t) = self.req.get(..4) {
                self.input_claims
                    .saw_request(u32::from_le_bytes(t.try_into().expect("4 bytes")));
            }

            let written = match layout {
                Err(ReadableAfterWritable) => {
                    log::warn!(
                        "chain {head} has a readable descriptor after a writable one; dropping"
                    );
                    0
                }
                Ok(()) if self.writable.is_empty() => {
                    log::warn!("chain {head} has no writable descriptor; dropping");
                    0
                }
                Ok(()) => {
                    let cap = std::cmp::min(capacity(&self.writable), RESP_MAX);
                    let resp = &mut self.resp[..cap];
                    let mut nvidia = self.nvidia.lock().expect("backend mutex");
                    #[cfg(feature = "trace")]
                    let n = if TRACE {
                        let (n, r) = nvidia.dispatch_traced(&self.req, resp, t_recv);
                        record = Some(r);
                        n
                    } else {
                        nvidia.dispatch(&self.req, resp)
                    };
                    #[cfg(not(feature = "trace"))]
                    let n = nvidia.dispatch(&self.req, resp);
                    self.watch_dirty |= nvidia.has_watch_updates();
                    #[cfg(feature = "venus")]
                    if self.req.get(..4) == Some(&(MsgType::GpuCmd as u32).to_le_bytes()[..]) {
                        self.gpu_cmd_seen = true;
                    }
                    // A fenced GpuCmd: nothing written, and the chain stays
                    // off the used ring until the fence pump returns it.
                    // Recorded under the backend lock, so its completion
                    // cannot be looked for before it is here.
                    #[cfg(feature = "venus")]
                    if let Some(token) = nvidia.take_held() {
                        self.venus.held.lock().expect("held chains").insert(
                            token,
                            HeldChain {
                                head,
                                resp: self.writable.clone(),
                            },
                        );
                        drop(nvidia);
                        #[cfg(feature = "trace")]
                        if TRACE && let Some(mut r) = record {
                            r.reply_ns = device::trace::now_ns().saturating_sub(t_recv);
                            device::trace::emit(r);
                        }
                        used = true;
                        self.served = true;
                        self.served_n += 1;
                        self.serve_prio::<TRACE>(prio, mem)?;
                        continue;
                    }
                    drop(nvidia);
                    if n > 0 {
                        write_scattered(&**mem, &self.writable, &resp[..n]).map_err(|e| {
                            std::io::Error::other(format!("write response descriptor: {e}"))
                        })?;
                    }
                    n
                }
            };

            vring
                .add_used(head, written as u32)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            if device::stage::on()
                && let Some(seq) = flip_seq(&self.req)
            {
                device::stage::flip(device::stage::H_USED, seq);
                self.flips_used.push(seq);
            }
            #[cfg(feature = "trace")]
            if TRACE && let Some(mut r) = record {
                r.reply_ns = device::trace::now_ns().saturating_sub(t_recv);
                device::trace::emit(r);
            }
            used = true;
            self.served = true;
            self.served_n += 1;
            self.serve_prio::<TRACE>(prio, mem)?;
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
            // Acked by a guest that consumes `InputEvent` (the Linux module).
            | u64::from(NVGPU_CFG_TAKES_INPUT)
            // Acked by a guest that wants `ScanoutReleased`; offered only
            // with a display.
            // Likewise `ScanoutPresented`.
            | if self.display_link.is_some() {
                u64::from(NVGPU_F_SCANOUT_RELEASE) | u64::from(NVGPU_F_SCANOUT_PRESENTED)
            } else {
                0
            }
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
    ///
    /// The acked features also say whether this guest's driver takes display
    /// input (`NVGPU_CFG_TAKES_INPUT`).
    fn acked_features(&mut self, features: u64) {
        if self.served {
            self.reset("device restarted (guest reboot or driver reload)");
        }
        self.input_claims.device_started(features);
        let release = features & u64::from(NVGPU_F_SCANOUT_RELEASE) != 0;
        let presented = features & u64::from(NVGPU_F_SCANOUT_PRESENTED) != 0;
        if let Some(link) = self.display_link.as_ref() {
            link.set_release_enabled(release);
            link.set_presented_enabled(presented);
        }
        log::info!(
            "guest driver features {features:#x}: {}{}{}",
            if features & u64::from(NVGPU_CFG_TAKES_INPUT) != 0 {
                "takes Conduit input"
            } else {
                "no NVGPU_CFG_TAKES_INPUT (Windows, or a Linux module from before it)"
            },
            if release {
                ", wants scanout buffer releases"
            } else {
                ""
            },
            if presented {
                ", wants presentation feedback"
            } else {
                ""
            }
        );
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
    /// conduit-vmm never asks; its config carries the window's size.
    fn get_shmem_config(&self) -> std::io::Result<VhostUserShMemConfig> {
        SPEC_SHMEM_FRONTEND.store(true, std::sync::atomic::Ordering::Relaxed);
        let window = self.nvidia.lock().expect("backend mutex").shm_total_size();
        #[cfg(feature = "venus")]
        let (n, sizes) = region_sizes_with_venus(window, APERTURE_LEN, self.venus.hostmem_len);
        #[cfg(not(feature = "venus"))]
        let (n, sizes) = region_sizes(window, APERTURE_LEN);
        #[cfg(feature = "venus")]
        if self.venus.hostmem_len != 0 {
            log::info!(
                "shared memory: Venus host-visible region {} MiB (shmid {SHM_ID_VENUS})",
                sizes[SHM_ID_VENUS as usize] >> 20
            );
        }
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
        // The cursor queue rides along with every control drain (`process`);
        // a kick on it alone is served here like any queue, with no
        // interleaving and no held chains (those are the control queue's).
        let cursor = device_event as usize == CURSOR_QUEUE;
        let prio = if cursor {
            None
        } else {
            vrings.get(CURSOR_QUEUE)
        };
        device::stage::kick();
        let before = self.served_n;
        let mut used = false;
        if self.event_idx {
            // With EVENT_IDX the guest suppresses notifications, so re-arm and
            // drain again rather than waiting for a kick that will not come.
            loop {
                vring.disable_notification().ok();
                used |= self.process(vring, prio, &mem)?;
                if !vring.enable_notification().unwrap_or(false) {
                    break;
                }
            }
        } else {
            used |= self.process(vring, prio, &mem)?;
        }
        if cursor {
            self.note_cursor_q(self.served_n - before);
        }
        #[cfg(feature = "venus")]
        if !cursor {
            self.venus_complete(vring, &mem);
        }
        // After serving, not before: a message that opened a descriptor has to
        // have been served for the backend to know about it.
        self.sync_watches(vrings);
        // Only for something put on the used ring: a kick that found nothing
        // (or only held chains, which the fence pump signals for) would cost
        // the guest an interrupt that tells it nothing.
        if used {
            vring
                .signal_used_queue()
                .map_err(|e| std::io::Error::other(format!("signal used queue: {e}")))?;
        }
        for seq in self.flips_used.drain(..) {
            device::stage::flip(device::stage::H_IRQ, seq);
        }
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

/// The `--trace` file (or `CONDUIT_TRACE`), created and truncated.
#[cfg(feature = "trace")]
fn open_trace_file(
    args: &Args,
) -> anyhow::Result<Option<(std::fs::File, device::trace::format::read::Format)>> {
    use device::trace::format::read::Format;
    let Some(path) = args
        .trace
        .clone()
        .or_else(|| std::env::var_os("CONDUIT_TRACE").map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return Ok(None);
    };
    let format = match args
        .trace_format
        .clone()
        .or_else(|| std::env::var("CONDUIT_TRACE_FORMAT").ok())
    {
        Some(f) => Format::parse(&f)
            .ok_or_else(|| anyhow::anyhow!("trace format {f:?}: expected json or bin"))?,
        None => Format::for_path(&path),
    };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("trace file {}: {e}", path.display()))?;
    log::info!("trace: writing {format:?} records to {}", path.display());
    Ok(Some((file, format)))
}

/// Region 3's size as `--venus` and `--venus-hostmem-mib` make it, or 0
/// (refused sizes are reported where the region is made).
fn venus_len(args: &Args) -> u64 {
    #[cfg(feature = "venus")]
    if args.venus {
        return device::shm_regions::venus_hostmem_len(args.venus_hostmem_mib).unwrap_or(0);
    }
    let _ = args;
    0
}

/// `--window-mib` on this host, in MiB, and why: `auto` from the GPU's BAR1
/// and the CPU's physical address bits (`shm_regions::auto_window_mib`).
fn window_mib(args: &Args) -> (u64, String) {
    use device::shm_regions::{self as r, WindowMib};
    let (bar1, bits) = r::host_window_inputs();
    let limit = r::window_mib_limit(bits, venus_len(args));
    let host = format!(
        "host BAR1 {}, {} physical address bits, guest limit {limit} MiB",
        bar1.map_or("unknown".into(), |b| format!("{} MiB", b >> 20)),
        bits.map_or("unknown".into(), |b| b.to_string()),
    );
    match args.window_mib {
        WindowMib::Auto => (
            r::auto_window_mib(bar1, bits, venus_len(args)),
            format!("auto: {host}"),
        ),
        WindowMib::Mib(n) => (n, format!("--window-mib {n}: {host}")),
    }
}

/// The renderer `--venus` talks to: the conduit-venus process at
/// `--venus-renderer`. `CONDUIT_VENUS_MOCK=1` serves from the in-memory
/// mock instead, which renders nothing, for testing the device without a
/// renderer. Connected after the sandbox, as a display client is: the
/// client starts a thread, which the sandbox must already cover.
#[cfg(feature = "venus")]
fn venus_renderer(args: &Args) -> anyhow::Result<Box<dyn conduit_venus::Renderer>> {
    if std::env::var("CONDUIT_VENUS_MOCK").as_deref() == Ok("1") {
        log::warn!(
            "venus: CONDUIT_VENUS_MOCK=1: serving from the mock renderer, which draws nothing"
        );
        return Ok(Box::new(conduit_venus::mock::Mock::new()));
    }
    let path = args.venus_renderer.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to start: --venus needs --venus-renderer PATH (conduit-venus's socket)"
        )
    })?;
    let client = conduit_venus::ipc::IpcClient::connect(path).map_err(|e| {
        anyhow::anyhow!(
            "refusing to start: venus renderer at {}: {e}",
            path.display()
        )
    })?;
    log::info!("venus: renderer at {}", path.display());
    Ok(Box::new(client))
}

/// Whether the guest still drives the device, for the boot console: its
/// event queue was running and is not any more. QEMU stops a vhost-user
/// device's rings on a guest reset, but (for its generic vhost-user device)
/// says nothing else until the next boot's driver starts it again -- and the
/// firmware screen in between is what the console is for.
fn guest_queues_probe(target: EventTarget) -> device::console::GuestProbe {
    let mut was_ready = false;
    Box::new(move || {
        let Some((vring, _)) = target.lock().expect("event target").clone() else {
            return true; // no guest queues yet: nothing to judge by
        };
        let ready = vring.get_ref().get_queue().ready();
        let stopped = was_ready && !ready;
        was_ready = ready;
        !stopped
    })
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
    // Before the sandbox and before any device: sysfs and CPUID only.
    let (window_mib, window_why) = window_mib(&args);
    if args.print_window_mib {
        device::shm_regions::window_len(window_mib)
            .map_err(|e| anyhow::anyhow!("{e} ({window_why})"))?;
        println!("{window_mib}");
        return Ok(());
    }
    // Before the sandbox, which may not allow the fstat that checks it.
    let activated = activated_listener();
    // Before any device is opened: the host driver judges every guest call by
    // this process's credentials (device::posture).
    device::posture::enforce()?;
    log::info!("open file limit {}", device::sandbox::raise_nofile());

    // The sandbox, before anything else: Landlock and seccomp cover this
    // thread and every thread started after them, and `VhostUserDaemon::new`
    // starts one. Everything the backend needs afterwards -- the GPU nodes,
    // the driver's own trees, and the directory its socket is bound in -- is
    // named here, because a ruleset cannot be added to once it is in force.
    let dir_of = |p: &Path| {
        p.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let socket_dir = dir_of(Path::new(&args.socket));
    #[allow(unused_mut)]
    let mut socket_dirs = vec![socket_dir];
    // The trace file is opened now: under the sandbox no file can be created.
    #[cfg(feature = "trace")]
    let trace_file = open_trace_file(&args)?;
    #[cfg(feature = "trace")]
    if let Some(p) = &args.trace_socket {
        let d = dir_of(p);
        if !socket_dirs.contains(&d) {
            socket_dirs.push(d);
        }
    }
    let report = device::sandbox::enter(&device::sandbox::Paths {
        devices: device::sandbox::gpu_nodes(),
        read_only: vec![
            args.proc_nvidia.clone(),
            PathBuf::from("/sys/class/drm"),
            PathBuf::from("/sys/bus/pci/devices"),
            PathBuf::from("/sys/devices"),
        ],
        sockets: socket_dirs,
    })?;
    log::info!(
        "sandbox: landlock ABI {}, {} seccomp instructions; no path outside the GPU nodes, the \
         driver's own trees and the socket's directory, no executable mapping, no process, no \
         socket but AF_UNIX",
        report.landlock_abi,
        report.seccomp_rules
    );

    // Read once, here. The strictest posture for a first run on a GPU that
    // also drives a desktop: the blocking timeouts a guest may forward cut to
    // 1 s (nvidia/bounds.rs).
    let safe_mode = std::env::var("CONDUIT_SAFE_MODE").as_deref() == Ok("1");
    if safe_mode {
        // The video-memory limit is not decided here: the CLI computes the one
        // final number (cli/src/protect.rs `final_limit_mib`: the smallest of
        // the owner's number, the display default and the 2 GiB safe-mode cap)
        // and this process enforces what it was given.
        log::warn!(
            "SAFE MODE is on (CONDUIT_SAFE_MODE=1): forwarded blocking timeouts clamped to 1 s; \
             video memory is held to --vram-limit-mib as the CLI computed it ({})",
            args.vram_limit_mib
                .map_or("none given, so no limit".to_string(), |m| format!(
                    "{m} MiB"
                ))
        );
    }
    log::info!(
        "conduit-backend: device id {VIRTIO_ID_GPU_NV}, socket {}, caps {}, {}",
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
    let input_claims = Arc::new(GuestInputClaims::default());
    let display = if args.display.is_some() || !args.display_socket.is_empty() {
        let mode = args.display.unwrap_or(DisplayMode::DEFAULT);
        let link = DisplayLink::with_paths(args.display_socket.clone(), mode);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink = VqInputSink {
            target: input_target.clone(),
            warned_small: false,
            warned_clip_small: false,
            msg: Vec::new(),
            posted: false,
            claims: input_claims.clone(),
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
            if args.display_socket.is_empty() {
                "none (flips are acked and dropped)".to_string()
            } else {
                args.display_socket
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        Some((mode, link, cursor))
    } else {
        None
    };

    // The boot console: QEMU's VNC screen while the guest's driver shows
    // nothing. Its thread, like the display's, starts after the sandbox.
    let _console = match (&args.console_vnc, &display) {
        (Some(path), Some((_, link, _))) => {
            let console = device::console::Console::start(
                link.clone(),
                path.clone(),
                Some(guest_queues_probe(input_target.clone())),
            )
            .map_err(|e| anyhow::anyhow!("console thread: {e}"))?;
            log::info!("console: the VM's screen from {}", path.display());
            Some(console)
        }
        (Some(path), None) => {
            log::warn!(
                "console: --console-vnc {} ignored: the device has no display (--display or \
                 --display-socket)",
                path.display()
            );
            None
        }
        (None, _) => None,
    };

    let window_len = device::shm_regions::window_len(window_mib)
        .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
    log::info!("window: {window_mib} MiB (shmid {SHM_ID_WINDOW}; {window_why})");
    let limit = device::shm_regions::window_mib_limit(
        device::shm_regions::host_phys_bits(),
        venus_len(&args),
    );
    if window_mib > limit {
        log::warn!(
            "window: {window_mib} MiB is above the {limit} MiB this host's guests can place; \
             the firmware may leave the device's BAR unassigned"
        );
    }
    #[allow(unused_mut)]
    let mut nvgpu = NvGpuBackend::new(
        &args.proc_nvidia,
        args.allow_nearest_abi,
        args.caps,
        args.vram_limit_mib,
        window_len,
        display,
        input_target,
        input_claims,
        safe_mode,
    )?;
    #[cfg(feature = "venus")]
    if args.venus {
        let len = device::shm_regions::venus_hostmem_len(args.venus_hostmem_mib)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
        nvgpu.enable_venus(venus_renderer(&args)?, len, args.venus_guest_blobs);
        log::info!(
            "venus: serving GpuCmd, region 3 {} MiB",
            args.venus_hostmem_mib
        );
    }
    let backend = Arc::new(RwLock::new(nvgpu));
    // Frame stage stamps from the start (docs/TRACING.md "Frame stage
    // timing"); `stages on` on the trace socket otherwise.
    if device::stage::init_from_env() {
        log::info!("stage timing on (CONDUIT_STAGE_TRACE)");
    }
    #[cfg(feature = "trace")]
    {
        if let Some(v) = host::driver_version(&args.proc_nvidia)
            .as_deref()
            .and_then(abi::version::DriverVersion::parse)
        {
            device::trace::set_driver(v);
        }
        device::trace::start(device::trace::Options {
            file: trace_file,
            socket: args.trace_socket.clone(),
        })
        .map_err(|e| anyhow::anyhow!("tracing: {e}"))?;
    }

    // vhost_user_backend::Error does not implement std::error::Error, so it
    // cannot ride `?` on its own.
    let mut daemon = VhostUserDaemon::new(
        "conduit-backend".to_string(),
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
            let served = daemon.start(&mut listener).and_then(|()| daemon.wait());
            // As `serve` does: stop the vring workers, or dropping the daemon
            // waits for them forever.
            for h in daemon.get_epoll_handlers() {
                h.send_exit_event();
            }
            served
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
    // Exit now: the device is torn down, and dropping the daemon and the
    // backend waits on worker threads (vring, events) that may never return.
    // Socket activation relies on this process ending with the VM.
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flip_request_gives_its_seq() {
        let f = protocol::messages::ScanoutFlip {
            seq: 0x1234_5678_9abc,
            ..Default::default()
        };
        let mut req = Vec::new();
        for v in [MsgType::ScanoutFlip as u32, 0, 0, 0] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(&f.to_bytes());
        assert_eq!(flip_seq(&req), Some(0x1234_5678_9abc));
        assert_eq!(flip_seq(&req[..40]), None, "short");
        req[0] = MsgType::GpuCmd as u8;
        assert_eq!(flip_seq(&req), None, "another message");
    }

    /// The guest takes input once it has posted event-queue buffers (the
    /// Linux guest, at probe), never while the queue is not running (the
    /// Windows KMD never starts it), and not again after it stops until the
    /// next driver posts anew.
    #[test]
    fn event_buffers_count_once_the_guest_posts_them() {
        let gm = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mem = GuestMemoryAtomic::new(gm);
        let vring = VringRwLock::new(mem.clone(), 256).unwrap();
        // A guest that never starts the queue.
        assert!(!event_buffers_posted(&vring, &mem, false));
        vring.set_queue_size(16);
        vring.set_queue_info(0x1000, 0x2000, 0x3000).unwrap();
        vring.set_queue_ready(true);
        // Started, nothing posted yet.
        assert!(!event_buffers_posted(&vring, &mem, false));
        // Posted: the avail index moved.
        let avail_idx = GuestAddress(0x2002);
        mem.memory().write_obj(64u16.to_le(), avail_idx).unwrap();
        assert!(event_buffers_posted(&vring, &mem, false));
        // Kept while the queue runs, whatever the index reads (wrapped).
        mem.memory().write_obj(0u16, avail_idx).unwrap();
        assert!(event_buffers_posted(&vring, &mem, true));
        assert!(!event_buffers_posted(&vring, &mem, false));
        // Stopped (a reset): gone, latch or not.
        mem.memory().write_obj(64u16.to_le(), avail_idx).unwrap();
        vring.set_queue_ready(false);
        assert!(!event_buffers_posted(&vring, &mem, true));
    }

    /// The sink routes input to the guest only for a driver that declares it
    /// and has posted event buffers: a Linux guest with the feature bit, a
    /// Linux guest whose module predates the bit (known by its `GetSysFiles`),
    /// but not the Windows KMD, which runs the event queue without the bit.
    #[test]
    fn the_sink_takes_input_only_for_a_guest_that_declares_it() {
        const VERSION_1: u64 = 1 << VIRTIO_F_VERSION_1;
        let gm = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mem = GuestMemoryAtomic::new(gm);
        let vring = VringRwLock::new(mem.clone(), 256).unwrap();
        let target: EventTarget = Arc::new(Mutex::new(None));
        let claims = Arc::new(GuestInputClaims::default());
        let mut sink = VqInputSink {
            target: target.clone(),
            warned_small: false,
            warned_clip_small: false,
            msg: Vec::new(),
            posted: false,
            claims: claims.clone(),
        };
        let live = |on: bool| {
            vring.set_queue_ready(on);
        };
        vring.set_queue_size(16);
        vring.set_queue_info(0x1000, 0x2000, 0x3000).unwrap();
        mem.memory()
            .write_obj(64u16.to_le(), GuestAddress(0x2002))
            .unwrap();

        // No guest request yet: no target, nothing taken.
        claims.device_started(VERSION_1 | u64::from(NVGPU_CFG_TAKES_INPUT));
        assert!(!sink.takes_input());
        *target.lock().unwrap() = Some((vring.clone(), mem.clone()));

        // Linux with the bit: once the queue is live.
        live(false);
        assert!(!sink.takes_input(), "queue not started");
        live(true);
        assert!(sink.takes_input());

        // Linux from before the bit: once it asked for sys files.
        claims.device_started(VERSION_1);
        assert!(!sink.takes_input());
        claims.saw_request(MsgType::GetSysFiles as u32);
        assert!(sink.takes_input());

        // Windows: event queue live and posted, no bit, no sys files.
        claims.device_started(VERSION_1);
        claims.saw_request(MsgType::ScanoutFlip as u32);
        claims.saw_request(MsgType::Open as u32);
        claims.saw_request(MsgType::GpuCmd as u32);
        assert!(!sink.takes_input(), "input stays on QEMU's devices");
        // NVK on RM there forwards GetSysFiles from user mode: too late to
        // be the Linux module.
        claims.saw_request(MsgType::GetSysFiles as u32);
        assert!(!sink.takes_input(), "Windows NVK is not the Linux module");
    }

    /// A fence's status rides in the header, signed, as the guest reads it.
    #[test]
    fn event_ready_carries_a_fence_status() {
        let b = event_ready_bytes(42, -libc::ETIMEDOUT);
        let hdr: MsgHeader = unsafe { std::ptr::read_unaligned(b.as_ptr() as *const MsgHeader) };
        assert_eq!(hdr.msg_type, MsgType::EventReady as u32);
        assert_eq!(hdr.handle, 42);
        assert_eq!(hdr.status, -libc::ETIMEDOUT);
        assert_eq!(event_ready_bytes(42, 0)[8..12], [0, 0, 0, 0]);
    }

    /// SYNC_IOC_FILE_INFO against the real kernel: a signalled sync_file
    /// reads as 0, not as its raw status of 1. Skipped without a render node.
    #[test]
    fn a_signalled_sync_file_has_status_zero() {
        let Some(node) = std::fs::read_dir("/dev/dri").ok().and_then(|d| {
            d.flatten()
                .map(|e| e.path())
                .find(|p| p.to_string_lossy().contains("renderD"))
        }) else {
            eprintln!("SKIP: no /dev/dri/renderD*; this test passes without testing anything");
            return;
        };
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
        else {
            eprintln!("SKIP: cannot open {}", node.display());
            return;
        };
        let iowr = |nr: u64, size: u64| (3u64 << 30) | (size << 16) | ((b'd' as u64) << 8) | nr;
        let mut create = [0u32, 1]; // handle, DRM_SYNCOBJ_CREATE_SIGNALED
        let rc = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                iowr(0xbf, 8) as libc::Ioctl,
                create.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "SYNCOBJ_CREATE");
        let mut export = [create[0], 1, u32::MAX, 0]; // EXPORT_SYNC_FILE, fd -1
        let rc = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                iowr(0xc1, 16) as libc::Ioctl,
                export.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "SYNCOBJ_HANDLE_TO_FD");
        let sync = unsafe { OwnedFd::from_raw_fd(export[2] as i32) };
        assert_eq!(
            sync_file_raw_status(sync.as_raw_fd()),
            Some(1),
            "the ioctl is right"
        );
        assert_eq!(sync_file_status(sync.as_raw_fd()), 0);
        // Not a sync_file at all: no error invented.
        assert_eq!(sync_file_raw_status(file.as_raw_fd()), None);
        assert_eq!(sync_file_status(file.as_raw_fd()), 0);
    }
}

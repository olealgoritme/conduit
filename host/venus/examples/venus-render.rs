//! GPU rendering through Venus, without a guest: hand-encoded Venus commands
//! through a running `conduit-venus --socket PATH` (the IPC client the backend
//! uses), drawing into a host-visible linear image and reading it back
//! through the HOST3D blob, as a guest would through region 3.
//!
//!   instance → physical device (NVIDIA picked by vendorID) → device →
//!   vkGetDeviceQueue2 (ring 1, VkDeviceQueueTimelineInfoMESA) →
//!   VkImage 256x256 B8G8R8A8_UNORM LINEAR → host-visible VkDeviceMemory →
//!   blob(blob_id = memory id) mmapped →
//!   command buffer: barrier, vkCmdClearColorImage (red, whole image),
//!     barrier, render pass with loadOp CLEAR over a sub-rectangle (green)
//!     and vkCmdClearAttachments (blue, a smaller one), barrier to HOST →
//!   vkQueueSubmit(fence) → virtio-gpu fence on ring 1 (the guest's path) →
//!   vkWaitForFences → verify every pixel through the mapping → PASS/FAIL →
//!   Renderer::export_scanout of the same blob, verified again through the
//!   dma-buf.
//!
//! Encoding (venus-protocol, see the generated
//! `third_party/build/venus-protocol/vn_protocol_renderer_*.h` decoders):
//! every command is `VkCommandTypeEXT (i32) | VkCommandFlagsEXT (u32) | args`.
//! Scalars are little-endian, 4-byte aligned, u64 not padded. A pointer is a
//! u64 0/1 followed by the pointee; an array is a u64 element count followed
//! by the elements; a struct is `sType | pNext (pointer) | members`; handles
//! are u64 object ids chosen by the guest; a union is a u32 tag and the
//! member. Output-only struct members are not sent ("partial" decoders).
//! Commands with `GENERATE_REPLY` write `type | VkResult? | outputs` into the
//! reply stream.
//!
//!   cargo run --example venus-render -- /tmp/venus.sock

use conduit_venus::ipc::IpcClient;
use conduit_venus::{CAPSET_VENUS, Renderer, ScanoutLayout};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const MAPPABLE: u32 = 1;
const SHAREABLE: u32 = 2;

// VkCommandTypeEXT (vn_protocol_renderer_defines.h)
const CMD_CREATE_INSTANCE: i32 = 0;
const CMD_ENUMERATE_PHYSICAL_DEVICES: i32 = 2;
const CMD_GET_PHYSICAL_DEVICE_PROPERTIES: i32 = 6;
const CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES: i32 = 8;
const CMD_CREATE_DEVICE: i32 = 11;
const CMD_QUEUE_SUBMIT: i32 = 18;
const CMD_ALLOCATE_MEMORY: i32 = 21;
const CMD_BIND_IMAGE_MEMORY: i32 = 29;
const CMD_GET_IMAGE_MEMORY_REQUIREMENTS: i32 = 31;
const CMD_CREATE_FENCE: i32 = 35;
const CMD_GET_FENCE_STATUS: i32 = 38;
const CMD_WAIT_FOR_FENCES: i32 = 39;
const CMD_CREATE_IMAGE: i32 = 54;
const CMD_GET_IMAGE_SUBRESOURCE_LAYOUT: i32 = 56;
const CMD_CREATE_IMAGE_VIEW: i32 = 57;
const CMD_CREATE_FRAMEBUFFER: i32 = 80;
const CMD_CREATE_RENDER_PASS: i32 = 82;
const CMD_CREATE_COMMAND_POOL: i32 = 85;
const CMD_ALLOCATE_COMMAND_BUFFERS: i32 = 88;
const CMD_BEGIN_COMMAND_BUFFER: i32 = 90;
const CMD_END_COMMAND_BUFFER: i32 = 91;
const CMD_CMD_CLEAR_COLOR_IMAGE: i32 = 119;
const CMD_CMD_CLEAR_ATTACHMENTS: i32 = 121;
const CMD_CMD_PIPELINE_BARRIER: i32 = 126;
const CMD_CMD_BEGIN_RENDER_PASS: i32 = 133;
const CMD_CMD_END_RENDER_PASS: i32 = 135;
const CMD_GET_DEVICE_QUEUE_2: i32 = 155;
const CMD_SET_REPLY_COMMAND_STREAM: i32 = 178;
const GENERATE_REPLY: u32 = 1;

// VkStructureType
const ST_APPLICATION_INFO: i32 = 0;
const ST_INSTANCE_CREATE_INFO: i32 = 1;
const ST_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const ST_DEVICE_CREATE_INFO: i32 = 3;
const ST_SUBMIT_INFO: i32 = 4;
const ST_MEMORY_ALLOCATE_INFO: i32 = 5;
const ST_FENCE_CREATE_INFO: i32 = 8;
const ST_IMAGE_CREATE_INFO: i32 = 14;
const ST_IMAGE_VIEW_CREATE_INFO: i32 = 15;
const ST_FRAMEBUFFER_CREATE_INFO: i32 = 37;
const ST_RENDER_PASS_CREATE_INFO: i32 = 38;
const ST_COMMAND_POOL_CREATE_INFO: i32 = 39;
const ST_COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
const ST_COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
const ST_RENDER_PASS_BEGIN_INFO: i32 = 43;
const ST_IMAGE_MEMORY_BARRIER: i32 = 45;
const ST_DEVICE_QUEUE_INFO_2: i32 = 1000145003;
/// VK_MESA_venus_protocol (extension 385), enum offset 5. vkr refuses a
/// queue without it: it binds the queue to a virtio-gpu fence ring.
const ST_DEVICE_QUEUE_TIMELINE_INFO_MESA: i32 = 1000384005;

const FORMAT_B8G8R8A8_UNORM: u32 = 44;
const LAYOUT_UNDEFINED: u32 = 0;
const LAYOUT_GENERAL: u32 = 1;
const ASPECT_COLOR: u32 = 1;
const QUEUE_FAMILY_IGNORED: u32 = !0;

const STAGE_TOP_OF_PIPE: u32 = 0x1;
const STAGE_COLOR_ATTACHMENT_OUTPUT: u32 = 0x400;
const STAGE_TRANSFER: u32 = 0x1000;
const STAGE_HOST: u32 = 0x4000;
const ACCESS_COLOR_ATTACHMENT_READ: u32 = 0x80;
const ACCESS_COLOR_ATTACHMENT_WRITE: u32 = 0x100;
const ACCESS_TRANSFER_WRITE: u32 = 0x1000;
const ACCESS_HOST_READ: u32 = 0x2000;

const MEM_HOST_VISIBLE: u32 = 2;
const MEM_HOST_COHERENT: u32 = 4;
const MEM_HOST_CACHED: u32 = 8;

const ID_INSTANCE: u64 = 1;
const ID_PHYS: [u64; 4] = [10, 11, 12, 13];
const ID_DEVICE: u64 = 20;
const ID_QUEUE: u64 = 21;
const ID_IMAGE: u64 = 40;
const ID_MEM: u64 = 41;
const ID_VIEW: u64 = 42;
const ID_RENDER_PASS: u64 = 43;
const ID_FRAMEBUFFER: u64 = 44;
const ID_POOL: u64 = 50;
const ID_CMD: u64 = 51;
const ID_FENCE: u64 = 52;

/// The queue's virtio-gpu fence ring (VkDeviceQueueTimelineInfoMESA.ringIdx).
const QUEUE_RING: u32 = 1;

const RES_REPLY: u32 = 1;
const RES_IMAGE: u32 = 2;
const REPLY_SIZE: usize = 1 << 16;

const W: u32 = 256;
const H: u32 = 256;
// BGRA8 colors as floats (r, g, b, a) and as the bytes in memory (b, g, r, a).
const RED: ([f32; 4], [u8; 4]) = ([1.0, 0.0, 0.0, 1.0], [0, 0, 255, 255]);
const GREEN: ([f32; 4], [u8; 4]) = ([0.0, 1.0, 0.0, 1.0], [0, 255, 0, 255]);
const BLUE: ([f32; 4], [u8; 4]) = ([0.0, 0.0, 1.0, 1.0], [255, 0, 0, 255]);
/// (x, y, w, h): render area cleared green by loadOp, and the
/// vkCmdClearAttachments rectangle cleared blue inside it.
const GREEN_RECT: (u32, u32, u32, u32) = (64, 32, 128, 96);
const BLUE_RECT: (u32, u32, u32, u32) = (100, 50, 40, 20);

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.u32(v as u32)
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.u32(v.to_bits())
    }
    fn cmd(&mut self, ty: i32, flags: u32) -> &mut Self {
        self.i32(ty).u32(flags)
    }
    /// A present pointer, or an array of `n` elements: the count is a u64.
    fn ptr(&mut self) -> &mut Self {
        self.u64(1)
    }
    fn null(&mut self) -> &mut Self {
        self.u64(0)
    }
    fn arr(&mut self, n: u64) -> &mut Self {
        self.u64(n)
    }
    /// `sType | pNext = NULL`
    fn st(&mut self, s_type: i32) -> &mut Self {
        self.i32(s_type).null()
    }
    /// VkImageSubresourceRange: color, mip 0, layer 0, one each.
    fn color_range(&mut self) -> &mut Self {
        self.u32(ASPECT_COLOR).u32(0).u32(1).u32(0).u32(1)
    }
    /// VkClearColorValue (union tag 0 = float32[4]).
    fn clear_color(&mut self, c: [f32; 4]) -> &mut Self {
        self.u32(0).arr(4);
        for v in c {
            self.f32(v);
        }
        self
    }
    /// VkRect2D: offset (i32, i32), extent (u32, u32).
    fn rect(&mut self, (x, y, w, h): (u32, u32, u32, u32)) -> &mut Self {
        self.i32(x as i32).i32(y as i32).u32(w).u32(h)
    }
    /// vkCmdPipelineBarrier with one image barrier on ID_IMAGE.
    fn image_barrier(
        &mut self,
        (src_stage, dst_stage): (u32, u32),
        (src_access, dst_access): (u32, u32),
        (old, new): (u32, u32),
    ) -> &mut Self {
        self.cmd(CMD_CMD_PIPELINE_BARRIER, 0).u64(ID_CMD).u32(src_stage).u32(dst_stage).u32(0); // dependencyFlags
        self.u32(0).arr(0); // memory barriers
        self.u32(0).arr(0); // buffer barriers
        self.u32(1).arr(1).st(ST_IMAGE_MEMORY_BARRIER);
        self.u32(src_access)
            .u32(dst_access)
            .u32(old)
            .u32(new)
            .u32(QUEUE_FAMILY_IGNORED)
            .u32(QUEUE_FAMILY_IGNORED)
            .u64(ID_IMAGE)
            .color_range()
    }
}

struct Venus {
    c: IpcClient,
    reply: *mut u8,
    fence: u64,
}

impl Venus {
    /// Wait for virtio-gpu fence `id`.
    fn wait_fence(&mut self, id: u64, timeout: Duration) -> bool {
        let t = Instant::now();
        loop {
            if self.c.signalled().expect("renderer gone").iter().any(|s| s.fence_id == id) {
                return true;
            }
            if t.elapsed() > timeout {
                return false;
            }
            let mut p = libc::pollfd { fd: self.c.fence_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd on a live descriptor.
            unsafe { libc::poll(&mut p, 1, 100) };
        }
    }

    /// Submit, then wait for a ring-0 fence: the context's messages are
    /// handled in order, so it retires once the submit has been decoded and
    /// executed (and any reply written).
    fn run(&mut self, cmds: &Enc) -> Vec<u8> {
        let mut e = Enc::default();
        e.cmd(CMD_SET_REPLY_COMMAND_STREAM, 0).ptr().u32(RES_REPLY).u64(0).u64(REPLY_SIZE as u64);
        e.0.extend_from_slice(&cmds.0);
        // SAFETY: reply maps REPLY_SIZE bytes.
        unsafe { std::ptr::write_bytes(self.reply, 0xee, 64) };
        self.c.submit(1, &e.0).expect("submit");
        self.fence += 1;
        self.c.create_fence(1, 0, self.fence).expect("create_fence");
        let f = self.fence;
        assert!(self.wait_fence(f, Duration::from_secs(20)), "fence {f} never signalled");
        // SAFETY: reply maps REPLY_SIZE bytes.
        unsafe { std::slice::from_raw_parts(self.reply, 4096).to_vec() }
    }

    /// Run one command that returns a VkResult; panics unless VK_SUCCESS.
    fn call(&mut self, name: &str, cmd: i32, e: &Enc) -> Vec<u8> {
        let r = self.run(e);
        let (ty, ret) = (i32_at(&r, 0), i32_at(&r, 4));
        assert_eq!(ty, cmd, "{name}: reply is for command {ty:#x}, not {cmd} (context went fatal?)");
        println!("{name} -> VkResult {ret}");
        assert_eq!(ret, 0, "{name} failed");
        r
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn i32_at(b: &[u8], o: usize) -> i32 {
    u32_at(b, o) as i32
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn fd_kind(fd: i32) -> String {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).map(|p| p.display().to_string()).unwrap_or_default()
}

fn inside((rx, ry, rw, rh): (u32, u32, u32, u32), x: u32, y: u32) -> bool {
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

fn expected(x: u32, y: u32) -> [u8; 4] {
    if inside(BLUE_RECT, x, y) {
        BLUE.1
    } else if inside(GREEN_RECT, x, y) {
        GREEN.1
    } else {
        RED.1
    }
}

/// Compare every pixel of a mapping (`base + offset`, `pitch` bytes per row).
/// Returns the number of wrong pixels, printing the first few.
fn verify(what: &str, base: *const u8, offset: u64, pitch: u64) -> usize {
    let mut bad = 0;
    for y in 0..H {
        for x in 0..W {
            let o = (offset + y as u64 * pitch + x as u64 * 4) as usize;
            // SAFETY: inside the mapping (offset + (H-1)*pitch + W*4 ≤ size,
            // checked by the caller).
            let got: [u8; 4] = unsafe { std::ptr::read_volatile(base.add(o).cast()) };
            let want = expected(x, y);
            if got != want {
                if bad < 5 {
                    println!("  {what}: ({x},{y}) = {got:02x?}, want {want:02x?}");
                }
                bad += 1;
            }
        }
    }
    let samples = [(0, 0), (70, 40), (110, 55), (255, 255)];
    let s: Vec<String> = samples
        .iter()
        .map(|&(x, y)| {
            let o = (offset + y as u64 * pitch + x as u64 * 4) as usize;
            // SAFETY: as above.
            let p: [u8; 4] = unsafe { std::ptr::read_volatile(base.add(o).cast()) };
            format!("({x},{y})={p:02x?}")
        })
        .collect();
    println!("{what}: {} / {} pixels wrong; BGRA samples {}", bad, W * H, s.join(" "));
    bad
}

fn mmap(fd: i32, size: usize) -> Option<*mut u8> {
    // SAFETY: a fresh shared mapping of a descriptor we hold.
    let p =
        unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    (p != libc::MAP_FAILED).then_some(p.cast())
}

fn main() {
    let path = std::env::args().nth(1).expect("socket path");
    let mut c = IpcClient::connect(path.as_ref()).expect("connect");
    c.ctx_create(1, CAPSET_VENUS, b"venus-render").expect("ctx_create");
    let reply_blob = c.create_blob(1, RES_REPLY, 0, REPLY_SIZE as u64, MAPPABLE).expect("reply blob");
    c.ctx_attach(1, RES_REPLY).expect("attach reply blob");
    let reply = mmap(reply_blob.fd.as_raw_fd(), REPLY_SIZE).expect("mmap reply blob");
    let mut v = Venus { c, reply, fence: 0 };
    // VENUS_STAGES=1: stage timing on for the run, and the ring fence's
    // stamps printed after it signals (docs/TRACING.md "Frame stage timing");
    // VENUS_STAGES_ALL=1 prints every stamp of the run.
    let stages = std::env::var_os("VENUS_STAGES").is_some();
    if stages {
        v.c.stages(true).expect("stages on (renderer without FEATURE_STAGE_TRACE?)");
    }

    // vkCreateInstance(apiVersion 1.3)
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_INSTANCE, GENERATE_REPLY);
    e.ptr().st(ST_INSTANCE_CREATE_INFO).u32(0); // flags
    e.ptr()
        .st(ST_APPLICATION_INFO)
        .null() // pApplicationName
        .u32(0)
        .null() // pEngineName
        .u32(0)
        .u32((1 << 22) | (3 << 12));
    e.u32(0).arr(0).u32(0).arr(0); // layers, extensions
    e.null().ptr().u64(ID_INSTANCE);
    v.call("vkCreateInstance", CMD_CREATE_INSTANCE, &e);

    // vkEnumeratePhysicalDevices, then the properties of each to find NVIDIA.
    let mut e = Enc::default();
    e.cmd(CMD_ENUMERATE_PHYSICAL_DEVICES, GENERATE_REPLY).u64(ID_INSTANCE);
    e.ptr().u32(ID_PHYS.len() as u32).arr(ID_PHYS.len() as u64);
    for id in ID_PHYS {
        e.u64(id);
    }
    let r = v.run(&e);
    assert_eq!(i32_at(&r, 0), CMD_ENUMERATE_PHYSICAL_DEVICES);
    let count = (u32_at(&r, 16) as usize).min(ID_PHYS.len());
    println!("vkEnumeratePhysicalDevices -> VkResult {}, {count} device(s)", i32_at(&r, 4));
    let mut phys = None;
    for &id in &ID_PHYS[..count] {
        let mut e = Enc::default();
        e.cmd(CMD_GET_PHYSICAL_DEVICE_PROPERTIES, GENERATE_REPLY).u64(id).ptr();
        let r = v.run(&e);
        assert_eq!(i32_at(&r, 0), CMD_GET_PHYSICAL_DEVICE_PROPERTIES);
        // type | ptr | apiVersion | driverVersion | vendorID | deviceID |
        // deviceType | name (u64 count, 256 chars)
        let (api, drv, vendor, dev, ty) =
            (u32_at(&r, 12), u32_at(&r, 16), u32_at(&r, 20), u32_at(&r, 24), u32_at(&r, 28));
        let name = &r[40..40 + 256];
        let name = String::from_utf8_lossy(&name[..name.iter().position(|&b| b == 0).unwrap_or(256)]);
        println!(
            "  id {id}: {name} vendor {vendor:#06x} device {dev:#06x} type {ty} api {}.{}.{} driver {drv:#x}",
            api >> 22,
            (api >> 12) & 0x3ff,
            api & 0xfff
        );
        if vendor == 0x10de && phys.is_none() {
            phys = Some(id);
        }
    }
    let phys = phys.unwrap_or(ID_PHYS[0]);
    println!("  using physical device id {phys}");

    // vkGetPhysicalDeviceMemoryProperties
    let mut e = Enc::default();
    e.cmd(CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES, GENERATE_REPLY).u64(phys).ptr().arr(32).arr(16);
    let r = v.run(&e);
    assert_eq!(i32_at(&r, 0), CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES);
    let ntypes = u32_at(&r, 12) as usize;
    let type_flags: Vec<u32> = (0..ntypes).map(|i| u32_at(&r, 24 + i * 8)).collect();

    // vkCreateDevice(queue family 0, one queue)
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_DEVICE, GENERATE_REPLY).u64(phys);
    e.ptr().st(ST_DEVICE_CREATE_INFO).u32(0);
    e.u32(1).arr(1).st(ST_DEVICE_QUEUE_CREATE_INFO);
    e.u32(0).u32(0).u32(1).arr(1).f32(1.0); // flags, family, count, priorities
    e.u32(0).arr(0).u32(0).arr(0).null(); // layers, extensions, features
    e.null().ptr().u64(ID_DEVICE);
    v.call("vkCreateDevice", CMD_CREATE_DEVICE, &e);

    // vkGetDeviceQueue2(family 0, index 0) chained with
    // VkDeviceQueueTimelineInfoMESA{ringIdx}. Returns nothing; the queue id
    // is ours. Batched with the image creation below.
    let mut e = Enc::default();
    e.cmd(CMD_GET_DEVICE_QUEUE_2, 0).u64(ID_DEVICE);
    e.ptr().i32(ST_DEVICE_QUEUE_INFO_2);
    e.ptr().st(ST_DEVICE_QUEUE_TIMELINE_INFO_MESA).u32(QUEUE_RING);
    e.u32(0).u32(0).u32(0); // flags, family, index
    e.ptr().u64(ID_QUEUE);

    // vkCreateImage: 256x256 B8G8R8A8_UNORM, LINEAR, transfer dst + color
    // attachment.
    e.cmd(CMD_CREATE_IMAGE, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_IMAGE_CREATE_INFO);
    e.u32(0) // flags
        .u32(1) // VK_IMAGE_TYPE_2D
        .u32(FORMAT_B8G8R8A8_UNORM)
        .u32(W)
        .u32(H)
        .u32(1)
        .u32(1) // mipLevels
        .u32(1) // arrayLayers
        .u32(1) // samples
        .u32(1) // VK_IMAGE_TILING_LINEAR
        .u32(0x1 | 0x2 | 0x10) // TRANSFER_SRC | TRANSFER_DST | COLOR_ATTACHMENT
        .u32(0) // EXCLUSIVE
        .u32(0)
        .arr(0)
        .u32(LAYOUT_UNDEFINED);
    e.null().ptr().u64(ID_IMAGE);
    v.call("vkGetDeviceQueue2 + vkCreateImage(256x256 BGRA8 LINEAR)", CMD_CREATE_IMAGE, &e);

    // vkGetImageMemoryRequirements → type | ptr | size | alignment | typeBits
    let mut e = Enc::default();
    e.cmd(CMD_GET_IMAGE_MEMORY_REQUIREMENTS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_IMAGE).ptr();
    let r = v.run(&e);
    assert_eq!(i32_at(&r, 0), CMD_GET_IMAGE_MEMORY_REQUIREMENTS);
    let (req_size, req_align, req_bits) = (u64_at(&r, 12), u64_at(&r, 20), u32_at(&r, 28));
    println!("vkGetImageMemoryRequirements: size {req_size} alignment {req_align} memoryTypeBits {req_bits:#x}");

    // vkGetImageSubresourceLayout → type | ptr | offset | size | rowPitch | ...
    let mut e = Enc::default();
    e.cmd(CMD_GET_IMAGE_SUBRESOURCE_LAYOUT, GENERATE_REPLY)
        .u64(ID_DEVICE)
        .u64(ID_IMAGE)
        .ptr()
        .u32(ASPECT_COLOR)
        .u32(0)
        .u32(0)
        .ptr();
    let r = v.run(&e);
    assert_eq!(i32_at(&r, 0), CMD_GET_IMAGE_SUBRESOURCE_LAYOUT);
    let (lay_off, lay_size, pitch) = (u64_at(&r, 12), u64_at(&r, 20), u64_at(&r, 28));
    println!("vkGetImageSubresourceLayout: offset {lay_off} size {lay_size} rowPitch {pitch}");
    assert!(lay_off + (H as u64 - 1) * pitch + W as u64 * 4 <= req_size);

    // Host-visible + coherent, cached if offered, among the allowed types.
    let want = MEM_HOST_VISIBLE | MEM_HOST_COHERENT;
    let ok = |i: usize| req_bits & (1 << i) != 0 && type_flags[i] & want == want;
    let mem_type = (0..ntypes)
        .find(|&i| ok(i) && type_flags[i] & MEM_HOST_CACHED != 0)
        .or_else(|| (0..ntypes).find(|&i| ok(i)))
        .expect("a host-visible memory type the image accepts");
    let size = req_size.div_ceil(4096) * 4096;

    // vkAllocateMemory + vkBindImageMemory
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_MEMORY_ALLOCATE_INFO).u64(size).u32(mem_type as u32);
    e.null().ptr().u64(ID_MEM);
    v.call(
        &format!("vkAllocateMemory({size} bytes, type {mem_type} flags {:#x})", type_flags[mem_type]),
        CMD_ALLOCATE_MEMORY,
        &e,
    );
    let mut e = Enc::default();
    e.cmd(CMD_BIND_IMAGE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_IMAGE).u64(ID_MEM).u64(0);
    v.call("vkBindImageMemory", CMD_BIND_IMAGE_MEMORY, &e);

    // The memory as a guest sees it: a HOST3D blob, mmapped.
    let blob =
        v.c.create_blob(1, RES_IMAGE, ID_MEM, size, MAPPABLE | SHAREABLE).expect("create_blob for the image memory");
    println!(
        "create_blob(HOST3D, blob_id {ID_MEM}, {size}, MAPPABLE|SHAREABLE): map_info {:#x}, fd {}",
        blob.map_info,
        fd_kind(blob.fd.as_raw_fd())
    );
    let pixels = mmap(blob.fd.as_raw_fd(), size as usize).expect("mmap image blob");
    // Poison it, so a pass means the GPU wrote every pixel.
    // SAFETY: pixels maps `size` bytes.
    unsafe { std::ptr::write_bytes(pixels, 0xcd, size as usize) };

    // Render pass objects: view, render pass (loadOp CLEAR, GENERAL in and
    // out), framebuffer.
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_IMAGE_VIEW, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_IMAGE_VIEW_CREATE_INFO);
    e.u32(0)
        .u64(ID_IMAGE)
        .u32(1) // VK_IMAGE_VIEW_TYPE_2D
        .u32(FORMAT_B8G8R8A8_UNORM)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0) // identity swizzle
        .color_range();
    e.null().ptr().u64(ID_VIEW);
    v.call("vkCreateImageView", CMD_CREATE_IMAGE_VIEW, &e);

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_RENDER_PASS, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_RENDER_PASS_CREATE_INFO).u32(0);
    e.u32(1).arr(1); // one VkAttachmentDescription
    e.u32(0)
        .u32(FORMAT_B8G8R8A8_UNORM)
        .u32(1) // samples
        .u32(1) // loadOp CLEAR
        .u32(0) // storeOp STORE
        .u32(2) // stencil DONT_CARE
        .u32(1)
        .u32(LAYOUT_GENERAL)
        .u32(LAYOUT_GENERAL);
    e.u32(1).arr(1); // one VkSubpassDescription
    e.u32(0).u32(0); // flags, GRAPHICS
    e.u32(0).arr(0); // input attachments
    e.u32(1).arr(1).u32(0).u32(LAYOUT_GENERAL); // color attachment 0
    e.arr(0); // pResolveAttachments
    e.null(); // pDepthStencilAttachment
    e.u32(0).arr(0); // preserve
    e.u32(0).arr(0); // dependencies
    e.null().ptr().u64(ID_RENDER_PASS);
    v.call("vkCreateRenderPass", CMD_CREATE_RENDER_PASS, &e);

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_FRAMEBUFFER, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_FRAMEBUFFER_CREATE_INFO);
    e.u32(0).u64(ID_RENDER_PASS).u32(1).arr(1).u64(ID_VIEW).u32(W).u32(H).u32(1);
    e.null().ptr().u64(ID_FRAMEBUFFER);
    v.call("vkCreateFramebuffer", CMD_CREATE_FRAMEBUFFER, &e);

    // Command pool, command buffer, fence.
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_COMMAND_POOL, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_COMMAND_POOL_CREATE_INFO).u32(0).u32(0);
    e.null().ptr().u64(ID_POOL);
    v.call("vkCreateCommandPool", CMD_CREATE_COMMAND_POOL, &e);

    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_COMMAND_BUFFERS, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr()
        .st(ST_COMMAND_BUFFER_ALLOCATE_INFO)
        .u64(ID_POOL)
        .u32(0) // PRIMARY
        .u32(1);
    e.arr(1).u64(ID_CMD);
    v.call("vkAllocateCommandBuffers", CMD_ALLOCATE_COMMAND_BUFFERS, &e);

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_FENCE, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_FENCE_CREATE_INFO).u32(0);
    e.null().ptr().u64(ID_FENCE);
    v.call("vkCreateFence", CMD_CREATE_FENCE, &e);

    // Record. vkCmd* have no replies; the stream ends with
    // vkEndCommandBuffer, whose reply says the context survived them all.
    let mut e = Enc::default();
    e.cmd(CMD_BEGIN_COMMAND_BUFFER, 0).u64(ID_CMD);
    e.ptr().st(ST_COMMAND_BUFFER_BEGIN_INFO).u32(1).null(); // ONE_TIME_SUBMIT
    e.image_barrier(
        (STAGE_TOP_OF_PIPE, STAGE_TRANSFER),
        (0, ACCESS_TRANSFER_WRITE),
        (LAYOUT_UNDEFINED, LAYOUT_GENERAL),
    );
    e.cmd(CMD_CMD_CLEAR_COLOR_IMAGE, 0)
        .u64(ID_CMD)
        .u64(ID_IMAGE)
        .u32(LAYOUT_GENERAL)
        .ptr()
        .clear_color(RED.0)
        .u32(1)
        .arr(1)
        .color_range();
    e.image_barrier(
        (STAGE_TRANSFER, STAGE_COLOR_ATTACHMENT_OUTPUT),
        (ACCESS_TRANSFER_WRITE, ACCESS_COLOR_ATTACHMENT_READ | ACCESS_COLOR_ATTACHMENT_WRITE),
        (LAYOUT_GENERAL, LAYOUT_GENERAL),
    );
    e.cmd(CMD_CMD_BEGIN_RENDER_PASS, 0).u64(ID_CMD);
    e.ptr()
        .st(ST_RENDER_PASS_BEGIN_INFO)
        .u64(ID_RENDER_PASS)
        .u64(ID_FRAMEBUFFER)
        .rect(GREEN_RECT)
        .u32(1)
        .arr(1)
        .u32(0) // VkClearValue tag: color
        .clear_color(GREEN.0);
    e.u32(0); // VK_SUBPASS_CONTENTS_INLINE
    e.cmd(CMD_CMD_CLEAR_ATTACHMENTS, 0).u64(ID_CMD);
    e.u32(1).arr(1).u32(ASPECT_COLOR).u32(0).u32(0).clear_color(BLUE.0);
    e.u32(1).arr(1).rect(BLUE_RECT).u32(0).u32(1);
    e.cmd(CMD_CMD_END_RENDER_PASS, 0).u64(ID_CMD);
    e.image_barrier(
        (STAGE_COLOR_ATTACHMENT_OUTPUT, STAGE_HOST),
        (ACCESS_COLOR_ATTACHMENT_WRITE, ACCESS_HOST_READ),
        (LAYOUT_GENERAL, LAYOUT_GENERAL),
    );
    e.cmd(CMD_END_COMMAND_BUFFER, GENERATE_REPLY).u64(ID_CMD);
    v.call("record + vkEndCommandBuffer", CMD_END_COMMAND_BUFFER, &e);

    // SAFETY: pixels maps `size` bytes.
    let before: [u8; 4] = unsafe { std::ptr::read_volatile(pixels.cast()) };
    println!("before submit, pixel (0,0) = {before:02x?} (poison)");

    // vkQueueSubmit(one command buffer, fence)
    let mut e = Enc::default();
    e.cmd(CMD_QUEUE_SUBMIT, GENERATE_REPLY).u64(ID_QUEUE);
    e.u32(1).arr(1).st(ST_SUBMIT_INFO);
    e.u32(0).arr(0).arr(0); // wait semaphores, wait stage masks
    e.u32(1).arr(1).u64(ID_CMD);
    e.u32(0).arr(0); // signal semaphores
    e.u64(ID_FENCE);
    let t = Instant::now();
    v.call("vkQueueSubmit", CMD_QUEUE_SUBMIT, &e);

    // The guest's way to wait: a virtio-gpu fence on the queue's ring, which
    // vkr retires when the queue's work before it is done.
    v.fence += 1;
    let ring_fence = v.fence;
    v.c.create_fence(1, QUEUE_RING, ring_fence).expect("create_fence on the queue ring");
    let ring_ok = v.wait_fence(ring_fence, Duration::from_secs(10));
    println!(
        "virtio-gpu fence {ring_fence} on ring {QUEUE_RING}: {} after {:?}",
        if ring_ok { "signalled" } else { "NOT signalled" },
        t.elapsed()
    );
    if stages {
        // The sync thread reports the GPU duration right after the fence.
        std::thread::sleep(Duration::from_millis(50));
        let all = std::env::var_os("VENUS_STAGES_ALL").is_some();
        let mut recs: Vec<_> =
            v.c.stages(false)
                .expect("stages")
                .into_iter()
                .filter(|r| all || (r.ring == QUEUE_RING as u8 && r.id == ring_fence))
                .collect();
        recs.sort_by_key(|r| r.ts_ns);
        let t0 = recs.first().map_or(0, |r| r.ts_ns);
        for r in &recs {
            let extra =
                if r.stage == conduit_venus::stage::V_GPU { format!(" (gpu {} ns)", r.aux) } else { String::new() };
            println!(
                "  stage {:>3} {:<18} +{:>8.1} us ctx {} ring {} id {}{extra}",
                r.stage,
                conduit_venus::stage::name(r.stage),
                (r.ts_ns - t0) as f64 / 1e3,
                r.ctx,
                r.ring,
                r.id
            );
        }
    }

    // And the Vulkan way: vkWaitForFences (synchronous in the renderer).
    let mut e = Enc::default();
    e.cmd(CMD_WAIT_FOR_FENCES, GENERATE_REPLY).u64(ID_DEVICE).u32(1).arr(1).u64(ID_FENCE).u32(1).u64(5_000_000_000);
    v.call("vkWaitForFences(5 s)", CMD_WAIT_FOR_FENCES, &e);
    let mut e = Enc::default();
    e.cmd(CMD_GET_FENCE_STATUS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_FENCE);
    v.call("vkGetFenceStatus", CMD_GET_FENCE_STATUS, &e);

    let bad = verify("blob mapping", pixels, lay_off, pitch);
    let mut pass = bad == 0;

    // Export the same memory for scanout, with the layout a guest would give
    // in SET_SCANOUT_BLOB (the linear image's own), and read it through the
    // dma-buf. virtio B8G8R8A8 is DRM ARGB8888.
    let layout = ScanoutLayout {
        width: W,
        height: H,
        stride: pitch as u32,
        offset: lay_off as u32,
        fourcc: u32::from_le_bytes(*b"AR24"),
    };
    match v.c.export_scanout(RES_IMAGE, layout) {
        Ok(d) => {
            let fourcc = d.fourcc.to_le_bytes();
            println!(
                "export_scanout(res {RES_IMAGE}, {W}x{H}) -> fd {}, {}x{} stride {} offset {} fourcc {:?} modifier {:#x}",
                fd_kind(d.fd.as_raw_fd()),
                d.width,
                d.height,
                d.stride,
                d.offset,
                std::str::from_utf8(&fourcc).unwrap_or("?"),
                d.modifier
            );
            if (d.width, d.height, d.stride, d.offset, d.fourcc, d.modifier)
                != (W, H, layout.stride, layout.offset, layout.fourcc, conduit_venus::DRM_FORMAT_MOD_LINEAR)
            {
                println!("  dma-buf layout is not the guest's: {layout:?}");
                pass = false;
            }
            match mmap(d.fd.as_raw_fd(), size as usize) {
                Some(p) => {
                    let bad = verify("dma-buf mapping", p, d.offset as u64, d.stride as u64);
                    pass &= bad == 0;
                    // VENUS_DUMP=FILE: the dma-buf's pixels as raw BGRA rows, to look at.
                    if let Some(out) = std::env::var_os("VENUS_DUMP") {
                        let mut raw = Vec::with_capacity((W * H * 4) as usize);
                        for y in 0..H as u64 {
                            let row = (d.offset as u64 + y * d.stride as u64) as usize;
                            // SAFETY: inside the mapping, as for verify.
                            raw.extend_from_slice(unsafe { std::slice::from_raw_parts(p.add(row), W as usize * 4) });
                        }
                        std::fs::write(&out, raw).expect("VENUS_DUMP");
                        println!("dumped {W}x{H} BGRA to {}", out.to_string_lossy());
                    }
                    // SAFETY: the mapping made above.
                    unsafe { libc::munmap(p.cast(), size as usize) };
                }
                None => println!("  mmap of the exported dma-buf failed: {}", std::io::Error::last_os_error()),
            }
        }
        Err(e) => {
            println!("export_scanout(res {RES_IMAGE}) = {e}");
            pass = false;
        }
    }

    // SAFETY: the mappings made above.
    unsafe {
        libc::munmap(pixels.cast(), size as usize);
        libc::munmap(v.reply.cast(), REPLY_SIZE);
    }
    drop(blob);
    v.c.unref(RES_IMAGE);
    v.c.ctx_destroy(1);
    println!("renderer still up: {:?}", v.c.capset_info(0).map(|i| i.id));
    println!("{}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}

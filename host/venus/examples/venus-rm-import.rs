//! An RM-export blob as a Venus guest consumes it, without a guest
//! (docs/VENUS.md "RM-export blobs"): a dma-buf of memory NVK-on-RM style
//! code rendered into is imported into a running `conduit-venus --socket
//! PATH` (`Renderer::import_dmabuf`, what the backend does for
//! `RESOURCE_CREATE_BLOB` with `BLOB_MEM_RM_EXPORT`), and then, through
//! hand-encoded Venus commands, used the way the D3D bridge would:
//!
//!   VkImage (VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, explicit modifier and
//!   plane layout, external memory DMA_BUF) →
//!   vkAllocateMemory(VkImportMemoryResourceInfoMESA{resource},
//!   VkMemoryDedicatedAllocateInfo) → bind →
//!   vkCmdCopyImage into a host-visible LINEAR image → read it back through
//!   a HOST3D blob and check every pixel.
//!
//! The dma-buf comes from the environment, as `rm_export_exec` (built from
//! guest/nvk-rm/tests/host_import_spike.c's RM half) leaves it: it allocates
//! RM vidmem as nvk-rm does, writes the pattern `(y << 16) | x` in the layout
//! under test, exports it through nvidia-drm and runs this with
//! RM_DMABUF_FD, RM_SIZE, RM_IMAGE_SIZE, RM_MODIFIER, RM_PITCH, RM_W, RM_H.
//!
//!   rm_export_exec bl5 cargo run --example venus-rm-import -- /tmp/venus.sock

use conduit_venus::ipc::IpcClient;
use conduit_venus::{CAPSET_VENUS, FEATURE_IMPORT_DMABUF, Renderer};
use std::os::fd::{AsRawFd, BorrowedFd};
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
const CMD_WAIT_FOR_FENCES: i32 = 39;
const CMD_CREATE_IMAGE: i32 = 54;
const CMD_GET_IMAGE_SUBRESOURCE_LAYOUT: i32 = 56;
const CMD_CREATE_COMMAND_POOL: i32 = 85;
const CMD_ALLOCATE_COMMAND_BUFFERS: i32 = 88;
const CMD_BEGIN_COMMAND_BUFFER: i32 = 90;
const CMD_END_COMMAND_BUFFER: i32 = 91;
const CMD_CMD_COPY_IMAGE: i32 = 113;
const CMD_CMD_PIPELINE_BARRIER: i32 = 126;
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
const ST_COMMAND_POOL_CREATE_INFO: i32 = 39;
const ST_COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
const ST_COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
const ST_IMAGE_MEMORY_BARRIER: i32 = 45;
const ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: i32 = 1000072001;
const ST_MEMORY_DEDICATED_ALLOCATE_INFO: i32 = 1000127001;
const ST_DEVICE_QUEUE_INFO_2: i32 = 1000145003;
const ST_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT: i32 = 1000158004;
const ST_IMPORT_MEMORY_RESOURCE_INFO_MESA: i32 = 1000384002;
const ST_DEVICE_QUEUE_TIMELINE_INFO_MESA: i32 = 1000384005;

const FORMAT_B8G8R8A8_UNORM: u32 = 44;
const TILING_LINEAR: u32 = 1;
const TILING_DRM_FORMAT_MODIFIER: u32 = 1000158000;
const LAYOUT_UNDEFINED: u32 = 0;
const LAYOUT_GENERAL: u32 = 1;
const ASPECT_COLOR: u32 = 1;
const QUEUE_FAMILY_IGNORED: u32 = !0;
const QUEUE_FAMILY_EXTERNAL: u32 = !1;
const HANDLE_TYPE_DMA_BUF: u32 = 0x200;

const STAGE_TOP_OF_PIPE: u32 = 0x1;
const STAGE_TRANSFER: u32 = 0x1000;
const STAGE_HOST: u32 = 0x4000;
const STAGE_ALL_COMMANDS: u32 = 0x10000;
const ACCESS_TRANSFER_READ: u32 = 0x800;
const ACCESS_TRANSFER_WRITE: u32 = 0x1000;
const ACCESS_HOST_READ: u32 = 0x2000;

const MEM_DEVICE_LOCAL: u32 = 1;
const MEM_HOST_VISIBLE: u32 = 2;
const MEM_HOST_COHERENT: u32 = 4;
const MEM_HOST_CACHED: u32 = 8;

const ID_INSTANCE: u64 = 1;
const ID_PHYS: [u64; 4] = [10, 11, 12, 13];
const ID_DEVICE: u64 = 20;
const ID_QUEUE: u64 = 21;
const ID_SRC: u64 = 40;
const ID_SRC_MEM: u64 = 41;
const ID_DST: u64 = 42;
const ID_DST_MEM: u64 = 43;
const ID_POOL: u64 = 50;
const ID_CMD: u64 = 51;
const ID_FENCE: u64 = 52;

const QUEUE_RING: u32 = 1;

const RES_REPLY: u32 = 1;
const RES_DST: u32 = 2;
const RES_RM: u32 = 3;
const REPLY_SIZE: usize = 1 << 16;

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
    /// A string in an array of strings: its size with the NUL, then the
    /// bytes padded to 4.
    fn string(&mut self, s: &str) -> &mut Self {
        let n = s.len() + 1;
        self.arr(n as u64);
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
        self
    }
    fn color_range(&mut self) -> &mut Self {
        self.u32(ASPECT_COLOR).u32(0).u32(1).u32(0).u32(1)
    }
    fn color_layers(&mut self) -> &mut Self {
        self.u32(ASPECT_COLOR).u32(0).u32(0).u32(1)
    }
    #[allow(clippy::too_many_arguments)]
    fn image_barrier(
        &mut self,
        image: u64,
        (src_stage, dst_stage): (u32, u32),
        (src_access, dst_access): (u32, u32),
        (old, new): (u32, u32),
        (src_qf, dst_qf): (u32, u32),
    ) -> &mut Self {
        self.cmd(CMD_CMD_PIPELINE_BARRIER, 0).u64(ID_CMD).u32(src_stage).u32(dst_stage).u32(0);
        self.u32(0).arr(0);
        self.u32(0).arr(0);
        self.u32(1).arr(1).st(ST_IMAGE_MEMORY_BARRIER);
        self.u32(src_access).u32(dst_access).u32(old).u32(new).u32(src_qf).u32(dst_qf).u64(image).color_range()
    }
}

struct Venus {
    c: IpcClient,
    reply: *mut u8,
    fence: u64,
}

impl Venus {
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

    /// Run one command that returns a VkResult; the result.
    fn try_call(&mut self, name: &str, cmd: i32, e: &Enc) -> (i32, Vec<u8>) {
        let r = self.run(e);
        let (ty, ret) = (i32_at(&r, 0), i32_at(&r, 4));
        assert_eq!(ty, cmd, "{name}: reply is for command {ty:#x}, not {cmd} (context went fatal?)");
        println!("{name} -> VkResult {ret}");
        (ret, r)
    }

    fn call(&mut self, name: &str, cmd: i32, e: &Enc) -> Vec<u8> {
        let (ret, r) = self.try_call(name, cmd, e);
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

fn env_u64(name: &str) -> u64 {
    let v = std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set (run under rm_export_exec)"));
    let v = v.trim();
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => v.parse(),
    }
    .unwrap_or_else(|_| panic!("{name}={v:?}"))
}

fn mmap(fd: i32, size: usize) -> Option<*mut u8> {
    // SAFETY: a fresh shared mapping of a descriptor we hold.
    let p =
        unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    (p != libc::MAP_FAILED).then_some(p.cast())
}

/// vkCreateImage of the imported image, with `row_pitch` in its explicit
/// plane layout.
fn create_src(v: &mut Venus, w: u32, h: u32, modifier: u64, row_pitch: u64, size: u64) -> i32 {
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_IMAGE, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().i32(ST_IMAGE_CREATE_INFO);
    // pNext: VkExternalMemoryImageCreateInfo{DMA_BUF} ->
    // VkImageDrmFormatModifierExplicitCreateInfoEXT{modifier, one plane}.
    e.ptr().i32(ST_EXTERNAL_MEMORY_IMAGE_CREATE_INFO);
    e.ptr().st(ST_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT);
    e.u64(modifier).u32(1).arr(1);
    e.u64(0).u64(size).u64(row_pitch).u64(0).u64(0); // VkSubresourceLayout
    e.u32(HANDLE_TYPE_DMA_BUF);
    e.u32(0) // flags
        .u32(1) // 2D
        .u32(FORMAT_B8G8R8A8_UNORM)
        .u32(w)
        .u32(h)
        .u32(1)
        .u32(1)
        .u32(1)
        .u32(1)
        .u32(TILING_DRM_FORMAT_MODIFIER)
        .u32(0x1 | 0x2 | 0x4) // TRANSFER_SRC | TRANSFER_DST | SAMPLED
        .u32(0)
        .u32(0)
        .arr(0)
        .u32(LAYOUT_UNDEFINED);
    e.null().ptr().u64(ID_SRC);
    v.try_call(
        &format!("vkCreateImage({w}x{h} BGRA8, modifier {modifier:#018x}, rowPitch {row_pitch}, DMA_BUF)"),
        CMD_CREATE_IMAGE,
        &e,
    )
    .0
}

fn main() {
    let path = std::env::args().nth(1).expect("socket path");
    let fd = env_u64("RM_DMABUF_FD") as i32;
    let (dmabuf_size, image_size) = (env_u64("RM_SIZE"), env_u64("RM_IMAGE_SIZE"));
    let (modifier, pitch) = (env_u64("RM_MODIFIER"), env_u64("RM_PITCH"));
    let (w, h) = (env_u64("RM_W") as u32, env_u64("RM_H") as u32);
    println!(
        "dma-buf fd {fd}: {dmabuf_size} bytes; image {w}x{h} modifier {modifier:#018x} pitch {pitch} size {image_size}"
    );

    let mut c = IpcClient::connect(path.as_ref()).expect("connect");
    assert_eq!(c.features() & FEATURE_IMPORT_DMABUF, FEATURE_IMPORT_DMABUF, "renderer cannot import dma-bufs");
    c.ctx_create(1, CAPSET_VENUS, b"venus-rm-import").expect("ctx_create");
    let reply_blob = c.create_blob(1, RES_REPLY, 0, REPLY_SIZE as u64, MAPPABLE).expect("reply blob");
    c.ctx_attach(1, RES_REPLY).expect("attach reply blob");
    let reply = mmap(reply_blob.fd.as_raw_fd(), REPLY_SIZE).expect("mmap reply blob");

    // What the backend does for RESOURCE_CREATE_BLOB(BLOB_MEM_RM_EXPORT).
    // SAFETY: the descriptor rm_export_exec left us, open for our lifetime.
    let dmabuf = unsafe { BorrowedFd::borrow_raw(fd) };
    c.import_dmabuf(RES_RM, dmabuf, image_size).expect("import_dmabuf");
    c.ctx_attach(1, RES_RM).expect("attach the imported resource");
    println!("import_dmabuf(res {RES_RM}, {image_size} bytes) + ctx_attach: ok");
    // From here only the renderer holds the memory: close ours.
    // SAFETY: our own descriptor, not used again.
    unsafe { libc::close(fd) };

    let mut v = Venus { c, reply, fence: 0 };

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_INSTANCE, GENERATE_REPLY);
    e.ptr().st(ST_INSTANCE_CREATE_INFO).u32(0);
    e.ptr().st(ST_APPLICATION_INFO).null().u32(0).null().u32(0).u32((1 << 22) | (3 << 12));
    e.u32(0).arr(0).u32(0).arr(0);
    e.null().ptr().u64(ID_INSTANCE);
    v.call("vkCreateInstance", CMD_CREATE_INSTANCE, &e);

    let mut e = Enc::default();
    e.cmd(CMD_ENUMERATE_PHYSICAL_DEVICES, GENERATE_REPLY).u64(ID_INSTANCE);
    e.ptr().u32(ID_PHYS.len() as u32).arr(ID_PHYS.len() as u64);
    for id in ID_PHYS {
        e.u64(id);
    }
    let r = v.run(&e);
    let count = (u32_at(&r, 16) as usize).min(ID_PHYS.len());
    let mut phys = None;
    for &id in &ID_PHYS[..count] {
        let mut e = Enc::default();
        e.cmd(CMD_GET_PHYSICAL_DEVICE_PROPERTIES, GENERATE_REPLY).u64(id).ptr();
        let r = v.run(&e);
        if u32_at(&r, 20) == 0x10de && phys.is_none() {
            phys = Some(id);
        }
    }
    let phys = phys.unwrap_or(ID_PHYS[0]);

    let mut e = Enc::default();
    e.cmd(CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES, GENERATE_REPLY).u64(phys).ptr().arr(32).arr(16);
    let r = v.run(&e);
    let ntypes = u32_at(&r, 12) as usize;
    let type_flags: Vec<u32> = (0..ntypes).map(|i| u32_at(&r, 24 + i * 8)).collect();

    // The device, with the one extension a guest asks for here (vkr adds the
    // external-memory fd ones itself).
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_DEVICE, GENERATE_REPLY).u64(phys);
    e.ptr().st(ST_DEVICE_CREATE_INFO).u32(0);
    e.u32(1).arr(1).st(ST_DEVICE_QUEUE_CREATE_INFO);
    e.u32(0).u32(0).u32(1).arr(1).f32(1.0);
    e.u32(0).arr(0);
    e.u32(1).arr(1).string("VK_EXT_image_drm_format_modifier");
    e.null();
    e.null().ptr().u64(ID_DEVICE);
    v.call("vkCreateDevice(+VK_EXT_image_drm_format_modifier)", CMD_CREATE_DEVICE, &e);

    let mut e = Enc::default();
    e.cmd(CMD_GET_DEVICE_QUEUE_2, 0).u64(ID_DEVICE);
    e.ptr().i32(ST_DEVICE_QUEUE_INFO_2);
    e.ptr().st(ST_DEVICE_QUEUE_TIMELINE_INFO_MESA).u32(QUEUE_RING);
    e.u32(0).u32(0).u32(0);
    e.ptr().u64(ID_QUEUE);
    e.cmd(CMD_CREATE_FENCE, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_FENCE_CREATE_INFO).u32(0);
    e.null().ptr().u64(ID_FENCE);
    v.call("vkGetDeviceQueue2 + vkCreateFence", CMD_CREATE_FENCE, &e);

    // The imported image: the explicit row pitch first (what the spike found
    // to work), then 0.
    let mut created = false;
    for rp in [pitch, 0] {
        if create_src(&mut v, w, h, modifier, rp, image_size) == 0 {
            created = true;
            break;
        }
    }
    assert!(created, "no plane layout made the image");

    let mut e = Enc::default();
    e.cmd(CMD_GET_IMAGE_MEMORY_REQUIREMENTS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_SRC).ptr();
    let r = v.run(&e);
    let (src_size, src_bits) = (u64_at(&r, 12), u32_at(&r, 28));
    println!("imported image needs {src_size} bytes, memoryTypeBits {src_bits:#x}");
    let src_type = (0..ntypes)
        .find(|&i| src_bits & (1 << i) != 0 && type_flags[i] & MEM_DEVICE_LOCAL != 0)
        .or_else(|| (0..ntypes).find(|&i| src_bits & (1 << i) != 0))
        .expect("a memory type for the imported image");

    // vkAllocateMemory(VkImportMemoryResourceInfoMESA{res} ->
    // VkMemoryDedicatedAllocateInfo{image}): what the guest's Venus driver
    // sends for a resource-id import.
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().i32(ST_MEMORY_ALLOCATE_INFO);
    e.ptr().i32(ST_IMPORT_MEMORY_RESOURCE_INFO_MESA);
    e.ptr().st(ST_MEMORY_DEDICATED_ALLOCATE_INFO).u64(ID_SRC).u64(0);
    e.u32(RES_RM);
    e.u64(src_size).u32(src_type as u32);
    e.null().ptr().u64(ID_SRC_MEM);
    v.call(
        &format!("vkAllocateMemory(import resource {RES_RM}, {src_size} bytes, type {src_type})"),
        CMD_ALLOCATE_MEMORY,
        &e,
    );
    let mut e = Enc::default();
    e.cmd(CMD_BIND_IMAGE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_SRC).u64(ID_SRC_MEM).u64(0);
    v.call("vkBindImageMemory(imported)", CMD_BIND_IMAGE_MEMORY, &e);

    // The destination: LINEAR, host-visible, read through a HOST3D blob.
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_IMAGE, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_IMAGE_CREATE_INFO);
    e.u32(0)
        .u32(1)
        .u32(FORMAT_B8G8R8A8_UNORM)
        .u32(w)
        .u32(h)
        .u32(1)
        .u32(1)
        .u32(1)
        .u32(1)
        .u32(TILING_LINEAR)
        .u32(0x2) // TRANSFER_DST
        .u32(0)
        .u32(0)
        .arr(0)
        .u32(LAYOUT_UNDEFINED);
    e.null().ptr().u64(ID_DST);
    v.call("vkCreateImage(LINEAR destination)", CMD_CREATE_IMAGE, &e);
    let mut e = Enc::default();
    e.cmd(CMD_GET_IMAGE_MEMORY_REQUIREMENTS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_DST).ptr();
    let r = v.run(&e);
    let (dst_req, dst_bits) = (u64_at(&r, 12), u32_at(&r, 28));
    let mut e = Enc::default();
    e.cmd(CMD_GET_IMAGE_SUBRESOURCE_LAYOUT, GENERATE_REPLY)
        .u64(ID_DEVICE)
        .u64(ID_DST)
        .ptr()
        .u32(ASPECT_COLOR)
        .u32(0)
        .u32(0)
        .ptr();
    let r = v.run(&e);
    let (dst_off, dst_pitch) = (u64_at(&r, 12), u64_at(&r, 28));
    let want = MEM_HOST_VISIBLE | MEM_HOST_COHERENT;
    let ok = |i: usize| dst_bits & (1 << i) != 0 && type_flags[i] & want == want;
    let dst_type = (0..ntypes)
        .find(|&i| ok(i) && type_flags[i] & MEM_HOST_CACHED != 0)
        .or_else(|| (0..ntypes).find(|&i| ok(i)))
        .expect("a host-visible memory type");
    let dst_size = dst_req.div_ceil(4096) * 4096;
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_MEMORY_ALLOCATE_INFO).u64(dst_size).u32(dst_type as u32);
    e.null().ptr().u64(ID_DST_MEM);
    v.call("vkAllocateMemory(destination)", CMD_ALLOCATE_MEMORY, &e);
    let mut e = Enc::default();
    e.cmd(CMD_BIND_IMAGE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_DST).u64(ID_DST_MEM).u64(0);
    v.call("vkBindImageMemory(destination)", CMD_BIND_IMAGE_MEMORY, &e);
    let blob = v.c.create_blob(1, RES_DST, ID_DST_MEM, dst_size, MAPPABLE | SHAREABLE).expect("destination blob");
    let pixels = mmap(blob.fd.as_raw_fd(), dst_size as usize).expect("mmap destination");
    // SAFETY: pixels maps dst_size bytes.
    unsafe { std::ptr::write_bytes(pixels, 0xcd, dst_size as usize) };

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_COMMAND_POOL, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_COMMAND_POOL_CREATE_INFO).u32(0).u32(0);
    e.null().ptr().u64(ID_POOL);
    v.call("vkCreateCommandPool", CMD_CREATE_COMMAND_POOL, &e);
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_COMMAND_BUFFERS, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_COMMAND_BUFFER_ALLOCATE_INFO).u64(ID_POOL).u32(0).u32(1);
    e.arr(1).u64(ID_CMD);
    v.call("vkAllocateCommandBuffers", CMD_ALLOCATE_COMMAND_BUFFERS, &e);

    // Acquire the imported image from its external owner, copy it, hand the
    // copy to the host.
    let mut e = Enc::default();
    e.cmd(CMD_BEGIN_COMMAND_BUFFER, 0).u64(ID_CMD);
    e.ptr().st(ST_COMMAND_BUFFER_BEGIN_INFO).u32(1).null();
    e.image_barrier(
        ID_SRC,
        (STAGE_ALL_COMMANDS, STAGE_TRANSFER),
        (0, ACCESS_TRANSFER_READ),
        (LAYOUT_GENERAL, LAYOUT_GENERAL),
        (QUEUE_FAMILY_EXTERNAL, 0),
    );
    e.image_barrier(
        ID_DST,
        (STAGE_TOP_OF_PIPE, STAGE_TRANSFER),
        (0, ACCESS_TRANSFER_WRITE),
        (LAYOUT_UNDEFINED, LAYOUT_GENERAL),
        (QUEUE_FAMILY_IGNORED, QUEUE_FAMILY_IGNORED),
    );
    e.cmd(CMD_CMD_COPY_IMAGE, 0).u64(ID_CMD).u64(ID_SRC).u32(LAYOUT_GENERAL).u64(ID_DST).u32(LAYOUT_GENERAL);
    e.u32(1).arr(1);
    e.color_layers().i32(0).i32(0).i32(0);
    e.color_layers().i32(0).i32(0).i32(0);
    e.u32(w).u32(h).u32(1);
    e.image_barrier(
        ID_DST,
        (STAGE_TRANSFER, STAGE_HOST),
        (ACCESS_TRANSFER_WRITE, ACCESS_HOST_READ),
        (LAYOUT_GENERAL, LAYOUT_GENERAL),
        (QUEUE_FAMILY_IGNORED, QUEUE_FAMILY_IGNORED),
    );
    e.cmd(CMD_END_COMMAND_BUFFER, GENERATE_REPLY).u64(ID_CMD);
    v.call("record copy + vkEndCommandBuffer", CMD_END_COMMAND_BUFFER, &e);

    let mut e = Enc::default();
    e.cmd(CMD_QUEUE_SUBMIT, GENERATE_REPLY).u64(ID_QUEUE);
    e.u32(1).arr(1).st(ST_SUBMIT_INFO);
    e.u32(0).arr(0).arr(0);
    e.u32(1).arr(1).u64(ID_CMD);
    e.u32(0).arr(0);
    e.u64(ID_FENCE);
    v.call("vkQueueSubmit", CMD_QUEUE_SUBMIT, &e);
    v.fence += 1;
    let f = v.fence;
    v.c.create_fence(1, QUEUE_RING, f).expect("create_fence on the queue ring");
    assert!(v.wait_fence(f, Duration::from_secs(10)), "queue ring fence");
    let mut e = Enc::default();
    e.cmd(CMD_WAIT_FOR_FENCES, GENERATE_REPLY).u64(ID_DEVICE).u32(1).arr(1).u64(ID_FENCE).u32(1).u64(5_000_000_000);
    v.call("vkWaitForFences", CMD_WAIT_FOR_FENCES, &e);

    // Every pixel must name itself: (y << 16) | x.
    let mut bad = 0u64;
    for y in 0..h {
        for x in 0..w {
            let o = (dst_off + y as u64 * dst_pitch + x as u64 * 4) as usize;
            // SAFETY: inside the destination mapping.
            let got = unsafe { std::ptr::read_volatile(pixels.add(o).cast::<u32>()) };
            let want = (y << 16) | x;
            if got != want {
                if bad < 5 {
                    println!("  ({x},{y}) = {got:#010x}, want {want:#010x}");
                }
                bad += 1;
            }
        }
    }
    println!("{bad} / {} pixels wrong", w as u64 * h as u64);
    // SAFETY: the mappings made above.
    unsafe {
        libc::munmap(pixels.cast(), dst_size as usize);
        libc::munmap(v.reply.cast(), REPLY_SIZE);
    }
    drop(blob);
    v.c.unref(RES_DST);
    v.c.ctx_detach(1, RES_RM);
    v.c.unref(RES_RM);
    v.c.ctx_destroy(1);
    println!("renderer still up: {:?}", v.c.capset_info(0).map(|i| i.id));
    println!("{}", if bad == 0 { "PASS" } else { "FAIL" });
    std::process::exit(if bad == 0 { 0 } else { 1 });
}

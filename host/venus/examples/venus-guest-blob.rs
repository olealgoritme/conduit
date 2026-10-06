//! A guest-memory blob as the Windows KMD will use it, without a guest
//! (docs/VENUS.md "Guest-memory blobs"): a memfd stands in for the VM's RAM,
//! a scattered page list of it is imported into a running `conduit-venus
//! --socket PATH` (`Renderer::import_guest_pages`, what the backend does for
//! `RESOURCE_CREATE_BLOB` with `BLOB_MEM_GUEST`), and then, through
//! hand-encoded Venus commands, used the way the KMD's Present blt will:
//!
//!   vkGetMemoryResourcePropertiesMESA(resource) ->
//!   vkAllocateMemory(VkImportMemoryResourceInfoMESA{resource}) ->
//!   vkCreateBuffer + bind ->
//!   vkCmdCopyImageToBuffer from a 1600x900 OPTIMAL image ->
//!   barrier TRANSFER_WRITE -> HOST_READ, fence.
//!
//! Every pixel is then checked through the test's own mapping of the memfd,
//! page by page in the scattered order: what the guest reads. The copy is
//! repeated and timed with GPU timestamps (timestampPeriod taken as 1 ns,
//! NVIDIA's). It ends with the teardown the contract asks for: destroy the
//! buffer, free the memory, unref.
//!
//!   cargo run --example venus-guest-blob -- /tmp/venus.sock

#![allow(dead_code)]

use conduit_venus::ipc::IpcClient;
use conduit_venus::{CAPSET_VENUS, FEATURE_IMPORT_GUEST_PAGES, PageRun, Renderer};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
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

// Guest-memory blobs
const CMD_FREE_MEMORY: i32 = 22;
const CMD_BIND_BUFFER_MEMORY: i32 = 28;
const CMD_GET_BUFFER_MEMORY_REQUIREMENTS: i32 = 30;
const CMD_RESET_FENCES: i32 = 37;
const CMD_CREATE_QUERY_POOL: i32 = 47;
const CMD_GET_QUERY_POOL_RESULTS: i32 = 49;
const CMD_CREATE_BUFFER: i32 = 50;
const CMD_DESTROY_BUFFER: i32 = 51;
const CMD_CMD_COPY_IMAGE_TO_BUFFER: i32 = 116;
const CMD_CMD_CLEAR_COLOR_IMAGE: i32 = 119;
const CMD_CMD_RESET_QUERY_POOL: i32 = 129;
const CMD_CMD_WRITE_TIMESTAMP: i32 = 130;
const CMD_GET_MEMORY_RESOURCE_PROPERTIES_MESA: i32 = 192;
const ST_QUERY_POOL_CREATE_INFO: i32 = 11;
const ST_BUFFER_CREATE_INFO: i32 = 12;
const ST_MEMORY_BARRIER: i32 = 46;
const ST_MEMORY_RESOURCE_PROPERTIES_MESA: i32 = 1000384001;
const ST_MEMORY_RESOURCE_ALLOCATION_SIZE_PROPERTIES_MESA: i32 = 1000384003;
const TILING_OPTIMAL: u32 = 0;
const LAYOUT_TRANSFER_SRC: u32 = 6;
const LAYOUT_TRANSFER_DST: u32 = 7;
const QUERY_TYPE_TIMESTAMP: u32 = 2;
const ID_BUF: u64 = 60;
const ID_GMEM: u64 = 61;
const ID_QP: u64 = 62;
const RES_GUEST: u32 = 4;

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

    fn call_quiet(&mut self, name: &str, cmd: i32, e: &Enc) -> Vec<u8> {
        let r = self.run(e);
        let (ty, ret) = (i32_at(&r, 0), i32_at(&r, 4));
        assert_eq!(ty, cmd, "{name}: reply is for command {ty:#x}, not {cmd} (context went fatal?)");
        assert_eq!(ret, 0, "{name} failed: VkResult {ret}");
        r
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

fn mmap(fd: i32, size: usize) -> Option<*mut u8> {
    // SAFETY: a fresh shared mapping of a descriptor we hold.
    let p =
        unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    (p != libc::MAP_FAILED).then_some(p.cast())
}

const W: u32 = 1600;
const H: u32 = 900;
const PAGE: u64 = 4096;
const ITER: u32 = 100;

/// Page i of the frame lives at memfd page `ram_page(i)`: pairs of pages in
/// reverse order, from 16 MiB on, so no two neighbours are contiguous.
fn ram_page(i: u64, pages: u64) -> u64 {
    let pairs = pages.div_ceil(2);
    4096 + (pairs - 1 - i / 2) * 2 + i % 2
}

fn main() {
    let path = std::env::args().nth(1).expect("socket path");
    let size = (W as u64 * 4 * H as u64).div_ceil(PAGE) * PAGE;
    let pages = size / PAGE;
    // The "VM's RAM": a sealed memfd, as QEMU's memory-backend-memfd makes it.
    let ram_len = 64u64 << 20;
    // SAFETY: plain syscalls on a fresh descriptor.
    let ram = unsafe {
        let fd = libc::memfd_create(c"guest-ram".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        assert!(fd >= 0);
        let fd = OwnedFd::from_raw_fd(fd);
        assert_eq!(libc::ftruncate(fd.as_raw_fd(), ram_len as i64), 0);
        assert_eq!(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW), 0);
        fd
    };
    let view = mmap(ram.as_raw_fd(), ram_len as usize).expect("mmap guest RAM");
    let runs: Vec<PageRun> = (0..pages).map(|i| PageRun { offset: ram_page(i, pages) * PAGE, len: PAGE }).collect();

    let mut c = IpcClient::connect(path.as_ref()).expect("connect");
    assert_ne!(c.features() & FEATURE_IMPORT_GUEST_PAGES, 0, "renderer cannot import guest pages");
    c.ctx_create(1, CAPSET_VENUS, b"venus-guest-blob").expect("ctx_create");
    let reply_blob = c.create_blob(1, RES_REPLY, 0, REPLY_SIZE as u64, MAPPABLE).expect("reply blob");
    c.ctx_attach(1, RES_REPLY).expect("attach reply blob");
    let reply = mmap(reply_blob.fd.as_raw_fd(), REPLY_SIZE).expect("mmap reply blob");

    // What the backend does for RESOURCE_CREATE_BLOB(BLOB_MEM_GUEST).
    let t = Instant::now();
    c.import_guest_pages(RES_GUEST, ram.as_fd(), &runs).expect("import_guest_pages");
    c.ctx_attach(1, RES_GUEST).expect("attach the guest blob");
    println!("import_guest_pages(res {RES_GUEST}, {} runs, {size} bytes) + attach: {:?}", runs.len(), t.elapsed());

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

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_DEVICE, GENERATE_REPLY).u64(phys);
    e.ptr().st(ST_DEVICE_CREATE_INFO).u32(0);
    e.u32(1).arr(1).st(ST_DEVICE_QUEUE_CREATE_INFO);
    e.u32(0).u32(0).u32(1).arr(1).f32(1.0);
    e.u32(0).arr(0);
    e.u32(0).arr(0);
    e.null();
    e.null().ptr().u64(ID_DEVICE);
    v.call("vkCreateDevice", CMD_CREATE_DEVICE, &e);

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

    // The resource's memory types and size, as the KMD asks for them.
    let mut e = Enc::default();
    e.cmd(CMD_GET_MEMORY_RESOURCE_PROPERTIES_MESA, GENERATE_REPLY).u64(ID_DEVICE).u32(RES_GUEST);
    e.ptr().i32(ST_MEMORY_RESOURCE_PROPERTIES_MESA);
    e.ptr().st(ST_MEMORY_RESOURCE_ALLOCATION_SIZE_PROPERTIES_MESA);
    let r = v.call("vkGetMemoryResourcePropertiesMESA", CMD_GET_MEMORY_RESOURCE_PROPERTIES_MESA, &e);
    let (alloc_size, res_bits) = (u64_at(&r, 40), u32_at(&r, 48));
    let names = |f: u32| {
        [(MEM_DEVICE_LOCAL, "DL"), (MEM_HOST_VISIBLE, "HV"), (MEM_HOST_COHERENT, "HC"), (MEM_HOST_CACHED, "CACHED")]
            .iter()
            .filter(|(b, _)| f & b != 0)
            .map(|(_, n)| *n)
            .collect::<Vec<_>>()
            .join("|")
    };
    for i in (0..ntypes).filter(|i| res_bits & (1 << i) != 0) {
        println!("  resource memory type {i}: {}", names(type_flags[i]));
    }
    println!("  allocationSize {alloc_size}");
    assert_eq!(alloc_size, size, "allocation size is the blob's");
    let coherent = MEM_HOST_VISIBLE | MEM_HOST_COHERENT;
    let mem_type = (0..ntypes)
        .find(|&i| res_bits & (1 << i) != 0 && type_flags[i] & coherent == coherent)
        .expect("a coherent memory type for the guest blob");

    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().i32(ST_MEMORY_ALLOCATE_INFO);
    e.ptr().st(ST_IMPORT_MEMORY_RESOURCE_INFO_MESA).u32(RES_GUEST);
    e.u64(alloc_size).u32(mem_type as u32);
    e.null().ptr().u64(ID_GMEM);
    let t = Instant::now();
    v.call(&format!("vkAllocateMemory(import resource {RES_GUEST}, type {mem_type})"), CMD_ALLOCATE_MEMORY, &e);
    println!("  import took {:?} (round trip)", t.elapsed());

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_BUFFER, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_BUFFER_CREATE_INFO).u32(0).u64(size).u32(0x2 /* TRANSFER_DST */).u32(0).u32(0).arr(0);
    e.null().ptr().u64(ID_BUF);
    v.call("vkCreateBuffer", CMD_CREATE_BUFFER, &e);
    let mut e = Enc::default();
    e.cmd(CMD_GET_BUFFER_MEMORY_REQUIREMENTS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_BUF).ptr();
    let r = v.run(&e);
    let (buf_req, buf_bits) = (u64_at(&r, 12), u32_at(&r, 28));
    println!("  buffer needs {buf_req} bytes, memoryTypeBits {buf_bits:#x}");
    let mut e = Enc::default();
    e.cmd(CMD_BIND_BUFFER_MEMORY, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_BUF).u64(ID_GMEM).u64(0);
    v.call("vkBindBufferMemory(guest blob)", CMD_BIND_BUFFER_MEMORY, &e);

    // The source: an OPTIMAL device-local image, as the app's frame is.
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_IMAGE, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_IMAGE_CREATE_INFO);
    e.u32(0).u32(1).u32(FORMAT_B8G8R8A8_UNORM).u32(W).u32(H).u32(1).u32(1).u32(1).u32(1);
    e.u32(TILING_OPTIMAL).u32(0x1 | 0x2).u32(0).u32(0).arr(0).u32(LAYOUT_UNDEFINED);
    e.null().ptr().u64(ID_SRC);
    v.call("vkCreateImage(OPTIMAL source)", CMD_CREATE_IMAGE, &e);
    let mut e = Enc::default();
    e.cmd(CMD_GET_IMAGE_MEMORY_REQUIREMENTS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_SRC).ptr();
    let r = v.run(&e);
    let (src_size, src_bits) = (u64_at(&r, 12), u32_at(&r, 28));
    let src_type = (0..ntypes)
        .find(|&i| src_bits & (1 << i) != 0 && type_flags[i] & MEM_DEVICE_LOCAL != 0)
        .expect("device-local memory");
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_MEMORY_ALLOCATE_INFO).u64(src_size).u32(src_type as u32);
    e.null().ptr().u64(ID_SRC_MEM);
    v.call("vkAllocateMemory(source)", CMD_ALLOCATE_MEMORY, &e);
    let mut e = Enc::default();
    e.cmd(CMD_BIND_IMAGE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_SRC).u64(ID_SRC_MEM).u64(0);
    v.call("vkBindImageMemory(source)", CMD_BIND_IMAGE_MEMORY, &e);

    let mut e = Enc::default();
    e.cmd(CMD_CREATE_QUERY_POOL, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_QUERY_POOL_CREATE_INFO).u32(0).u32(QUERY_TYPE_TIMESTAMP).u32(2).u32(0);
    e.null().ptr().u64(ID_QP);
    v.call("vkCreateQueryPool", CMD_CREATE_QUERY_POOL, &e);
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_COMMAND_POOL, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_COMMAND_POOL_CREATE_INFO).u32(0x2 /* RESET_COMMAND_BUFFER */).u32(0);
    e.null().ptr().u64(ID_POOL);
    v.call("vkCreateCommandPool", CMD_CREATE_COMMAND_POOL, &e);
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_COMMAND_BUFFERS, GENERATE_REPLY).u64(ID_DEVICE);
    e.ptr().st(ST_COMMAND_BUFFER_ALLOCATE_INFO).u64(ID_POOL).u32(0).u32(1);
    e.arr(1).u64(ID_CMD);
    v.call("vkAllocateCommandBuffers", CMD_ALLOCATE_COMMAND_BUFFERS, &e);

    let mut gpu_ns = Vec::new();
    let mut bad_total = 0u64;
    let mut checked = 0;
    for frame in 1..=ITER {
        // Clear the image to a colour that names the frame, copy it into the
        // guest blob between two timestamps, hand the bytes to the host.
        let mut e = Enc::default();
        e.cmd(CMD_BEGIN_COMMAND_BUFFER, 0).u64(ID_CMD);
        e.ptr().st(ST_COMMAND_BUFFER_BEGIN_INFO).u32(1).null();
        e.image_barrier(
            ID_SRC,
            (STAGE_TOP_OF_PIPE, STAGE_TRANSFER),
            (0, ACCESS_TRANSFER_WRITE),
            (LAYOUT_UNDEFINED, LAYOUT_TRANSFER_DST),
            (QUEUE_FAMILY_IGNORED, QUEUE_FAMILY_IGNORED),
        );
        e.cmd(CMD_CMD_CLEAR_COLOR_IMAGE, 0).u64(ID_CMD).u64(ID_SRC).u32(LAYOUT_TRANSFER_DST);
        // VkClearColorValue: tag 0 (float32[4]), R G B A.
        e.ptr().u32(0).arr(4).f32(frame as f32 / 255.0).f32(0x5a as f32 / 255.0).f32(0xa5 as f32 / 255.0).f32(1.0);
        e.u32(1).arr(1).color_range();
        e.image_barrier(
            ID_SRC,
            (STAGE_TRANSFER, STAGE_TRANSFER),
            (ACCESS_TRANSFER_WRITE, ACCESS_TRANSFER_READ),
            (LAYOUT_TRANSFER_DST, LAYOUT_TRANSFER_SRC),
            (QUEUE_FAMILY_IGNORED, QUEUE_FAMILY_IGNORED),
        );
        e.cmd(CMD_CMD_RESET_QUERY_POOL, 0).u64(ID_CMD).u64(ID_QP).u32(0).u32(2);
        e.cmd(CMD_CMD_WRITE_TIMESTAMP, 0).u64(ID_CMD).u32(STAGE_TRANSFER).u64(ID_QP).u32(0);
        e.cmd(CMD_CMD_COPY_IMAGE_TO_BUFFER, 0).u64(ID_CMD).u64(ID_SRC).u32(LAYOUT_TRANSFER_SRC).u64(ID_BUF);
        e.u32(1).arr(1).u64(0).u32(W).u32(H).color_layers().i32(0).i32(0).i32(0).u32(W).u32(H).u32(1);
        e.cmd(CMD_CMD_WRITE_TIMESTAMP, 0).u64(ID_CMD).u32(STAGE_TRANSFER).u64(ID_QP).u32(1);
        e.cmd(CMD_CMD_PIPELINE_BARRIER, 0).u64(ID_CMD).u32(STAGE_TRANSFER).u32(STAGE_HOST).u32(0);
        e.u32(1).arr(1).st(ST_MEMORY_BARRIER).u32(ACCESS_TRANSFER_WRITE).u32(ACCESS_HOST_READ);
        e.u32(0).arr(0);
        e.u32(0).arr(0);
        e.cmd(CMD_END_COMMAND_BUFFER, GENERATE_REPLY).u64(ID_CMD);
        v.call_quiet("record + vkEndCommandBuffer", CMD_END_COMMAND_BUFFER, &e);

        let mut e = Enc::default();
        e.cmd(CMD_RESET_FENCES, GENERATE_REPLY).u64(ID_DEVICE).u32(1).arr(1).u64(ID_FENCE);
        v.call_quiet("vkResetFences", CMD_RESET_FENCES, &e);
        let mut e = Enc::default();
        e.cmd(CMD_QUEUE_SUBMIT, GENERATE_REPLY).u64(ID_QUEUE);
        e.u32(1).arr(1).st(ST_SUBMIT_INFO);
        e.u32(0).arr(0).arr(0);
        e.u32(1).arr(1).u64(ID_CMD);
        e.u32(0).arr(0);
        e.u64(ID_FENCE);
        v.call_quiet("vkQueueSubmit", CMD_QUEUE_SUBMIT, &e);
        v.fence += 1;
        let f = v.fence;
        v.c.create_fence(1, QUEUE_RING, f).expect("create_fence on the queue ring");
        assert!(v.wait_fence(f, Duration::from_secs(10)), "queue ring fence");

        let mut e = Enc::default();
        e.cmd(CMD_GET_QUERY_POOL_RESULTS, GENERATE_REPLY).u64(ID_DEVICE).u64(ID_QP).u32(0).u32(2);
        e.u64(16).arr(16).u64(8).u32(0x1 | 0x2 /* 64_BIT | WAIT */);
        let r = v.call_quiet("vkGetQueryPoolResults", CMD_GET_QUERY_POOL_RESULTS, &e);
        let (t0, t1) = (u64_at(&r, 16), u64_at(&r, 24));
        if frame > 5 {
            gpu_ns.push(t1.saturating_sub(t0));
        }

        if frame == 1 || frame % 16 == 0 || frame == ITER {
            // What the guest reads: its pages, in its order.
            let mut bad = 0u64;
            for i in 0..pages {
                let page = ram_page(i, pages) * PAGE;
                let n =
                    (size - i * PAGE).min(PAGE).min(W as u64 * 4 * H as u64 - (i * PAGE).min(W as u64 * 4 * H as u64));
                for o in (0..n).step_by(4) {
                    // SAFETY: inside the guest RAM mapping.
                    let px = unsafe { std::ptr::read_volatile(view.add((page + o) as usize).cast::<u32>()) };
                    let want = 0xff00_0000 | ((frame & 0xff) << 16) | (0x5a << 8) | 0xa5;
                    if px != want {
                        if bad_total + bad < 3 {
                            println!("  frame {frame}: page {i} +{o}: {px:#010x}, want {want:#010x}");
                        }
                        bad += 1;
                    }
                }
            }
            bad_total += bad;
            checked += 1;
        }
    }
    gpu_ns.sort_unstable();
    let avg = gpu_ns.iter().sum::<u64>() as f64 / gpu_ns.len() as f64 / 1e6;
    println!(
        "RESULT guest blob via Venus: gpu copy 1600x900 BGRA avg {avg:.3} ms (min {:.3}, max {:.3}), {checked} frames checked, {bad_total} bad px",
        gpu_ns[0] as f64 / 1e6,
        gpu_ns[gpu_ns.len() - 1] as f64 / 1e6
    );

    // Teardown in the contract's order: buffer, memory, then the resource.
    let mut e = Enc::default();
    e.cmd(CMD_DESTROY_BUFFER, 0).u64(ID_DEVICE).u64(ID_BUF).null();
    e.cmd(CMD_FREE_MEMORY, 0).u64(ID_DEVICE).u64(ID_GMEM).null();
    e.cmd(CMD_RESET_FENCES, GENERATE_REPLY).u64(ID_DEVICE).u32(1).arr(1).u64(ID_FENCE);
    v.call("vkDestroyBuffer + vkFreeMemory", CMD_RESET_FENCES, &e);
    v.c.ctx_detach(1, RES_GUEST);
    v.c.unref(RES_GUEST);
    // SAFETY: the mappings made above.
    unsafe {
        libc::munmap(v.reply.cast(), REPLY_SIZE);
        libc::munmap(view.cast(), ram_len as usize);
    }
    v.c.unref(RES_REPLY);
    v.c.ctx_destroy(1);
    println!("renderer still up: {:?}", v.c.capset_info(0).map(|i| i.id));
    println!("{}", if bad_total == 0 { "PASS" } else { "FAIL" });
    std::process::exit(if bad_total == 0 { 0 } else { 1 });
}

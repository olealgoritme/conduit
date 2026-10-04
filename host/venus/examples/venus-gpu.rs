//! Real Venus traffic without a guest: hand-encoded Venus commands through a
//! running `conduit-venus --socket PATH`, enough to reach the host Vulkan
//! driver and get HOST3D blobs backed by real `VkDeviceMemory`:
//!
//!   vkCreateInstance → vkEnumeratePhysicalDevices →
//!   vkGetPhysicalDeviceMemoryProperties → vkCreateDevice →
//!   vkAllocateMemory (host-visible, and device-local exported as dma-buf) →
//!   RESOURCE_CREATE_BLOB(blob_id = the VkDeviceMemory id) → mmap / export.
//!
//! Venus object ids are chosen by the guest driver, which is what lets a
//! guest (and this test) name a VkDeviceMemory as a blob_id. Replies come
//! back in a shm blob set as the reply stream; a ring-0 fence after each
//! submit marks when the render server has processed it.
//!
//!   cargo run --example venus-gpu -- /tmp/venus.sock

use conduit_venus::ipc::IpcClient;
use conduit_venus::{CAPSET_VENUS, Renderer, ScanoutLayout};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const MAPPABLE: u32 = 1;
const SHAREABLE: u32 = 2;

const CMD_CREATE_INSTANCE: i32 = 0;
const CMD_ENUMERATE_PHYSICAL_DEVICES: i32 = 2;
const CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES: i32 = 8;
const CMD_CREATE_DEVICE: i32 = 11;
const CMD_ALLOCATE_MEMORY: i32 = 21;
const CMD_SET_REPLY_COMMAND_STREAM: i32 = 178;
const GENERATE_REPLY: u32 = 1;

const ST_APPLICATION_INFO: i32 = 0;
const ST_INSTANCE_CREATE_INFO: i32 = 1;
const ST_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const ST_DEVICE_CREATE_INFO: i32 = 3;
const ST_MEMORY_ALLOCATE_INFO: i32 = 5;
const ST_EXPORT_MEMORY_ALLOCATE_INFO: i32 = 1000072002;
const HANDLE_TYPE_DMA_BUF: u32 = 0x200;

const MEM_DEVICE_LOCAL: u32 = 1;
const MEM_HOST_VISIBLE: u32 = 2;
const MEM_HOST_COHERENT: u32 = 4;

const ID_INSTANCE: u64 = 1;
const ID_PHYS: [u64; 4] = [10, 11, 12, 13];
const ID_DEVICE: u64 = 20;
const ID_MEM_HOST: u64 = 30;
const ID_MEM_LOCAL: u64 = 31;

const RES_REPLY: u32 = 1;
const RES_HOST: u32 = 2;
const RES_LOCAL: u32 = 3;
const REPLY_SIZE: usize = 1 << 16;
const MIB: u64 = 1 << 20;

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
}

struct Venus {
    c: IpcClient,
    reply: *mut u8,
    fence: u64,
}

impl Venus {
    /// Submit, then wait for a ring-0 fence: the render server handles a
    /// context's messages in order, so the fence retires after the submit.
    fn run(&mut self, cmds: &Enc) -> Vec<u8> {
        // Reply stream at offset 0 of the shm blob, reset for every submit.
        let mut e = Enc::default();
        e.cmd(CMD_SET_REPLY_COMMAND_STREAM, 0).u64(1).u32(RES_REPLY).u64(0).u64(REPLY_SIZE as u64);
        e.0.extend_from_slice(&cmds.0);
        // SAFETY: reply maps REPLY_SIZE bytes.
        unsafe { std::ptr::write_bytes(self.reply, 0xee, 64) };
        self.c.submit(1, &e.0).expect("submit");
        self.fence += 1;
        self.c.create_fence(1, 0, self.fence).expect("create_fence");
        let t = Instant::now();
        loop {
            if self.c.signalled().expect("renderer gone").iter().any(|s| s.fence_id == self.fence) {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "fence {} never signalled", self.fence);
            let mut p = libc::pollfd { fd: self.c.fence_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd on a live descriptor.
            unsafe { libc::poll(&mut p, 1, 100) };
        }
        // SAFETY: reply maps REPLY_SIZE bytes.
        unsafe { std::slice::from_raw_parts(self.reply, 4096).to_vec() }
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

/// What the kernel says the fd is: `/dmabuf:` for a dma-buf.
fn fd_kind(fd: i32) -> String {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).map(|p| p.display().to_string()).unwrap_or_default()
}

fn check_result(name: &str, r: &[u8], cmd: i32) -> i32 {
    let (ty, ret) = (i32_at(r, 0), i32_at(r, 4));
    assert_eq!(ty, cmd, "{name}: reply is for command {ty} (no reply written?)");
    println!("{name} -> VkResult {ret}");
    ret
}

fn main() {
    let path = std::env::args().nth(1).expect("socket path");
    let mut c = IpcClient::connect(path.as_ref()).expect("connect");
    c.ctx_create(1, CAPSET_VENUS, b"venus-gpu").expect("ctx_create");
    let reply_blob = c.create_blob(1, RES_REPLY, 0, REPLY_SIZE as u64, MAPPABLE).expect("reply blob");
    c.ctx_attach(1, RES_REPLY).expect("attach reply blob");
    // SAFETY: maps the shm blob that the renderer writes replies into.
    let reply = unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            REPLY_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            reply_blob.fd.as_raw_fd(),
            0,
        );
        assert_ne!(p, libc::MAP_FAILED);
        p.cast::<u8>()
    };
    let mut v = Venus { c, reply, fence: 0 };

    // vkCreateInstance(apiVersion 1.3, no layers or extensions: vkr refuses any)
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_INSTANCE, GENERATE_REPLY);
    e.u64(1).i32(ST_INSTANCE_CREATE_INFO).u64(0).u32(0);
    e.u64(1).i32(ST_APPLICATION_INFO).u64(0).u64(0).u32(0).u64(0).u32(0).u32((1 << 22) | (3 << 12));
    e.u32(0).u64(0).u32(0).u64(0);
    e.u64(0); // pAllocator
    e.u64(1).u64(ID_INSTANCE);
    let r = v.run(&e);
    assert_eq!(check_result("vkCreateInstance", &r, CMD_CREATE_INSTANCE), 0);

    // vkEnumeratePhysicalDevices(count 4, ids chosen here)
    let mut e = Enc::default();
    e.cmd(CMD_ENUMERATE_PHYSICAL_DEVICES, GENERATE_REPLY).u64(ID_INSTANCE);
    e.u64(1).u32(ID_PHYS.len() as u32).u64(ID_PHYS.len() as u64);
    for id in ID_PHYS {
        e.u64(id);
    }
    let r = v.run(&e);
    let ret = check_result("vkEnumeratePhysicalDevices", &r, CMD_ENUMERATE_PHYSICAL_DEVICES);
    assert!(ret == 0 || ret == 5, "VK_SUCCESS or VK_INCOMPLETE");
    let count = u32_at(&r, 16);
    println!("  {count} physical device(s); using id {}", ID_PHYS[0]);

    // vkGetPhysicalDeviceMemoryProperties: the partial out-struct is just its
    // two array sizes (the elements have no input fields).
    let mut e = Enc::default();
    e.cmd(CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES, GENERATE_REPLY).u64(ID_PHYS[0]).u64(1).u64(32).u64(16);
    let r = v.run(&e);
    assert_eq!(i32_at(&r, 0), CMD_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES);
    // [type][ptr u64][typeCount u32][32 u64][32 × (flags, heap)][heapCount][16 u64][16 × (size u64, flags u32)]
    let ntypes = u32_at(&r, 12) as usize;
    let types: Vec<(u32, u32)> = (0..ntypes).map(|i| (u32_at(&r, 24 + i * 8), u32_at(&r, 28 + i * 8))).collect();
    let heaps_at = 24 + 32 * 8;
    let nheaps = u32_at(&r, heaps_at) as usize;
    for (i, (flags, heap)) in types.iter().enumerate() {
        println!("  memory type {i}: flags {flags:#x} heap {heap}");
    }
    for h in 0..nheaps {
        let o = heaps_at + 12 + h * 12;
        println!("  heap {h}: {} MiB flags {:#x}", u64_at(&r, o) >> 20, u32_at(&r, o + 8));
    }
    let host_type = types
        .iter()
        .position(|(f, _)| f & (MEM_HOST_VISIBLE | MEM_HOST_COHERENT) == (MEM_HOST_VISIBLE | MEM_HOST_COHERENT))
        .expect("a host-visible coherent memory type") as u32;
    let local_type =
        types.iter().position(|(f, _)| f & MEM_DEVICE_LOCAL != 0 && f & MEM_HOST_VISIBLE == 0).map(|i| i as u32);

    // vkCreateDevice(queue family 0, one queue)
    let mut e = Enc::default();
    e.cmd(CMD_CREATE_DEVICE, GENERATE_REPLY).u64(ID_PHYS[0]);
    e.u64(1).i32(ST_DEVICE_CREATE_INFO).u64(0).u32(0);
    e.u32(1).u64(1).i32(ST_DEVICE_QUEUE_CREATE_INFO).u64(0).u32(0).u32(0).u32(1).u64(1).f32(1.0);
    e.u32(0).u64(0).u32(0).u64(0).u64(0);
    e.u64(0).u64(1).u64(ID_DEVICE);
    let r = v.run(&e);
    assert_eq!(check_result("vkCreateDevice", &r, CMD_CREATE_DEVICE), 0);

    // vkAllocateMemory, host-visible: what a guest maps through region 3.
    let mut e = Enc::default();
    e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
    e.u64(1).i32(ST_MEMORY_ALLOCATE_INFO).u64(0).u64(MIB).u32(host_type);
    e.u64(0).u64(1).u64(ID_MEM_HOST);
    let r = v.run(&e);
    assert_eq!(check_result(&format!("vkAllocateMemory(1 MiB, type {host_type})"), &r, CMD_ALLOCATE_MEMORY), 0);

    match v.c.create_blob(1, RES_HOST, ID_MEM_HOST, MIB, MAPPABLE) {
        Ok(b) => {
            println!(
                "create_blob(HOST3D, blob_id {ID_MEM_HOST}, 1 MiB, MAPPABLE) = map_info {:#x}, fd {}",
                b.map_info,
                fd_kind(b.fd.as_raw_fd())
            );
            // SAFETY: maps the blob fd as the backend would for region 3.
            unsafe {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    MIB as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    b.fd.as_raw_fd(),
                    0,
                );
                if p == libc::MAP_FAILED {
                    println!("  mmap of blob fd FAILED: {}", std::io::Error::last_os_error());
                } else {
                    *p.cast::<u64>() = 0x0123_4567_89ab_cdef;
                    println!("  mmap ok, wrote/read {:#x}", *p.cast::<u64>());
                    libc::munmap(p, MIB as usize);
                }
            }
        }
        Err(e) => println!("create_blob(HOST3D, blob_id {ID_MEM_HOST}, MAPPABLE) = {e}"),
    }

    // Device-local, exportable as dma-buf: the shape of a scanout image's memory.
    if let Some(local_type) = local_type {
        let mut e = Enc::default();
        e.cmd(CMD_ALLOCATE_MEMORY, GENERATE_REPLY).u64(ID_DEVICE);
        e.u64(1).i32(ST_MEMORY_ALLOCATE_INFO);
        e.u64(1).i32(ST_EXPORT_MEMORY_ALLOCATE_INFO).u64(0).u32(HANDLE_TYPE_DMA_BUF);
        e.u64(8 * MIB).u32(local_type);
        e.u64(0).u64(1).u64(ID_MEM_LOCAL);
        let r = v.run(&e);
        let ret = check_result(
            &format!("vkAllocateMemory(8 MiB, type {local_type}, export dma-buf)"),
            &r,
            CMD_ALLOCATE_MEMORY,
        );
        if ret == 0 {
            match v.c.create_blob(1, RES_LOCAL, ID_MEM_LOCAL, 8 * MIB, SHAREABLE) {
                Ok(_) => {
                    println!("create_blob(HOST3D, blob_id {ID_MEM_LOCAL}, 8 MiB, SHAREABLE) ok");
                    match v.c.export_scanout(
                        RES_LOCAL,
                        ScanoutLayout {
                            width: 1920,
                            height: 1080,
                            stride: 1920 * 4,
                            offset: 0,
                            fourcc: u32::from_le_bytes(*b"XR24"),
                        },
                    ) {
                        Ok(d) => println!(
                            "export_scanout -> fd {}, {}x{} stride {} offset {} fourcc {:?} modifier {:#x}",
                            fd_kind(d.fd.as_raw_fd()),
                            d.width,
                            d.height,
                            d.stride,
                            d.offset,
                            std::str::from_utf8(&d.fourcc.to_le_bytes()).unwrap_or("?"),
                            d.modifier
                        ),
                        Err(e) => println!("export_scanout = {e}"),
                    }
                }
                Err(e) => println!("create_blob(blob_id {ID_MEM_LOCAL}, SHAREABLE) = {e}"),
            }
        }
    }

    v.c.unref(RES_LOCAL);
    v.c.unref(RES_HOST);
    v.c.ctx_destroy(1);
    println!("renderer still up: {:?}", v.c.capset_info(0).map(|i| i.id));
}

//! Drives a running `conduit-venus --socket PATH` through the IPC client as
//! far as the API goes without a guest: capsets, a context, a shm blob (the
//! kind Venus uses for its command ring, blob_id 0), a fence, and the refusals
//! for what needs real Venus objects.
//!
//!   cargo run --example venus-smoke -- /tmp/venus.sock

use conduit_venus::ipc::IpcClient;
use conduit_venus::{CAPSET_VENUS, Renderer, ScanoutLayout};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const MAPPABLE: u32 = 1;

fn main() {
    let path = std::env::args().nth(1).expect("socket path");
    let mut c = IpcClient::connect(path.as_ref()).expect("connect");

    let info = c.capset_info(0).expect("capset_info");
    println!("capset_info(0) = {info:?}");
    println!("capset_info(1) = {:?}", c.capset_info(1).err());
    let caps = c.capset(CAPSET_VENUS, 0).expect("capset");
    println!("capset(4, 0): {} bytes, head {:02x?}", caps.len(), &caps[..caps.len().min(32)]);

    c.ctx_create(1, CAPSET_VENUS, b"venus-smoke").expect("ctx_create");
    println!("ctx_create(1) ok");

    let b = c.create_blob(1, 1, 0, 1 << 20, MAPPABLE).expect("create_blob shm");
    println!("create_blob(blob_id 0, 1 MiB, MAPPABLE) = map_info {:#x}, size {}", b.map_info, b.size);
    // SAFETY: maps the received fd for a write/read round trip.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            1 << 20,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            b.fd.as_raw_fd(),
            0,
        );
        assert_ne!(p, libc::MAP_FAILED, "mmap blob");
        *p.cast::<u32>().add(1000) = 0x5eed;
        println!("blob mmap ok, wrote/read {:#x}", *p.cast::<u32>().add(1000));
        libc::munmap(p, 1 << 20);
    }
    c.ctx_attach(1, 1).expect("ctx_attach");
    c.ctx_detach(1, 1);
    c.ctx_attach(1, 1).expect("ctx_attach again");
    println!("ctx_attach/detach ok");

    println!(
        "create_blob(blob_id 77, no such VkDeviceMemory) = {:?}",
        c.create_blob(1, 2, 77, 1 << 20, MAPPABLE).err()
    );
    println!(
        "export_scanout(shm blob) = {:?}",
        c.export_scanout(
            1,
            ScanoutLayout { width: 64, height: 64, stride: 64 * 4, offset: 0, fourcc: u32::from_le_bytes(*b"XR24") }
        )
        .err()
    );

    match c.create_fence(1, 0, 1) {
        Ok(()) => {
            let t = Instant::now();
            let mut got = Vec::new();
            while got.is_empty() && t.elapsed() < Duration::from_secs(5) {
                let mut p = libc::pollfd { fd: c.fence_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
                // SAFETY: one pollfd on a live descriptor.
                unsafe { libc::poll(&mut p, 1, 100) };
                got = c.signalled().expect("renderer gone");
            }
            println!("create_fence(1, ring 0, 1) -> signalled {got:?} after {:?}", t.elapsed());
        }
        Err(e) => println!("create_fence(1, ring 0, 1) = {e}"),
    }

    println!("submit(1, empty) = {:?}", c.submit(1, &[]));
    c.unref(1);
    c.ctx_destroy(1);
    // Round trip after the fire-and-forget calls: the renderer is still up.
    println!("capset_info(0) after teardown = {:?}", c.capset_info(0).map(|i| i.id));
}

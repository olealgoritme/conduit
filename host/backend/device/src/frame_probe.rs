//! End-to-end frame identity probe (debug, off unless `CONDUIT_FRAME_PROBE=<file>`).
//!
//! A guest test application (`d3d11_iflip counter`) clears every frame to a colour that encodes
//! its frame index (R = bits 0..7, G = 8..15, B = 16..23, A = 255). For every frame the backend
//! sends a display client, this reads the first pixel of the guest's buffer through a CPU mapping
//! of the exported dma-buf (pixel (0,0) is the first four bytes in a linear and a block-linear
//! layout alike) and logs `S <stamp> <index> <t_ns>`; every presentation report of a display client
//! is logged as `P <stamp> <present_ns> <kind>`. Joining the two by stamp gives what the screen
//! really showed: unique frames per second, repeats, gaps and reordering
//! (`host/backend/tools/frame_probe.py`).
//!
//! Costs one mmap/munmap per sent frame while on; nothing at all while off.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::RawFd;
use std::sync::{Mutex, OnceLock};

struct Probe {
    out: Mutex<BufWriter<File>>,
}

fn probe() -> Option<&'static Probe> {
    static P: OnceLock<Option<Probe>> = OnceLock::new();
    P.get_or_init(|| {
        let path = std::env::var("CONDUIT_FRAME_PROBE").ok()?;
        match File::create(&path) {
            Ok(f) => {
                log::info!("display: frame probe on, writing {path}");
                Some(Probe {
                    out: Mutex::new(BufWriter::with_capacity(1 << 16, f)),
                })
            }
            Err(e) => {
                log::warn!("display: frame probe: {path}: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Open the probe's file now (before the sandbox, under which no file can be created).
pub fn init() {
    let _ = probe();
}

/// Whether the probe is on (one load).
pub fn on() -> bool {
    probe().is_some()
}

fn mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The first 32-bit pixel at `offset` of the dma-buf `fd`, through a read-only CPU mapping of the
/// page that holds it. `None` when the buffer cannot be mapped.
pub fn first_pixel(fd: RawFd, offset: u64) -> Option<u32> {
    let page = 4096u64;
    let base = offset & !(page - 1);
    let within = (offset - base) as usize;
    // SAFETY: a fresh read-only shared mapping of one page of a live descriptor, unmapped below;
    // the read is volatile because the device writes the memory.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            page as usize,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            base as libc::off_t,
        );
        if p == libc::MAP_FAILED {
            return None;
        }
        let v = std::ptr::read_volatile((p as *const u8).add(within) as *const u32);
        libc::munmap(p, page as usize);
        Some(v)
    }
}

/// The frame index a pixel encodes (`d3d11_iflip counter`, B8G8R8A8 in memory), or `None` when
/// alpha is not 255 (not a probe frame).
pub fn decode(pixel: u32) -> Option<u32> {
    if pixel >> 24 != 0xFF {
        return None;
    }
    let b = pixel & 0xFF;
    let g = (pixel >> 8) & 0xFF;
    let r = (pixel >> 16) & 0xFF;
    Some(r | (g << 8) | (b << 16))
}

/// A frame with this stamp was sent from dma-buf `fd` (plane offset `offset`).
pub fn sent(stamp: u32, fd: RawFd, offset: u64) {
    let Some(p) = probe() else { return };
    let t = mono_ns();
    let idx = match first_pixel(fd, offset) {
        Some(px) => decode(px).map_or(-1i64, i64::from),
        None => -2,
    };
    let _ = writeln!(p.out.lock().unwrap(), "S {stamp} {idx} {t}");
}

/// A display client reported the frame with this stamp on screen.
pub fn presented(stamp: u32, present_ns: u64, kind: u32) {
    let Some(p) = probe() else { return };
    let mut w = p.out.lock().unwrap();
    let _ = writeln!(w, "P {stamp} {present_ns} {kind}");
    // Flush now and then so a short run's file is complete without a clean exit.
    let _ = w.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_what_the_guest_app_encodes() {
        // idx 0x0A0B0C: R = 0x0C, G = 0x0B, B = 0x0A; memory B,G,R,A -> u32 0xFF0C0B0A.
        assert_eq!(decode(0xFF0C_0B0A), Some(0x0A0B0C));
        assert_eq!(decode(0x000C_0B0A), None, "alpha 0: not a probe frame");
    }
}

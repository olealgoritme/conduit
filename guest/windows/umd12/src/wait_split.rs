//! Where the presenting thread's frame goes (`Umd12WaitSplit`, default off).
//!
//! The frame CPU line says how much of a frame the presenting thread is not on
//! the CPU; this says where. The thread that calls `pfnPresent` is the
//! presenting thread. On it, and only on it, every forwarding DDI's wall time
//! is summed (the `ddi_time!` hook), and the DDIs that can block (Present,
//! ExecuteCommandLists, the fence and residency DDIs, Map / Unmap) also take
//! the thread's cycle time (`QueryThreadCycleTime`) at entry and exit, so their
//! time splits exactly into on and off the CPU. Every 256 frames:
//!
//! ```text
//! D3D12 presenting thread (256 frames): frame W us = on CPU C + off CPU O;
//!   in driver DDIs D us, off CPU Do: present a us (off b), ...; other DDIs r us;
//!   outside the driver X us, off CPU Xo us
//! ```
//!
//! `Xo` is the time the thread waits in the runtime or the application with
//! none of this driver's code on the stack: an application's fence event
//! (`ID3D12Fence::SetEventOnCompletion` is the runtime's own
//! `D3DKMTWaitForSynchronizationObjectFromCpu` with an event, then the
//! application's `WaitForSingleObject`; no DDI or kernel callback of ours is
//! called), a frame-latency waitable, a `Sleep`. The kernel callbacks this
//! driver calls itself run inside its DDIs and are counted there.
//!
//! Off: one relaxed load per DDI (shared with `Umd12DdiTimes`). On, a thread
//! that never presents pays one thread-local read per DDI; the presenting
//! thread pays two `Instant`s per DDI and two `QueryThreadCycleTime`s per
//! blocking DDI (about twenty per frame).

use std::cell::Cell;
use std::time::Instant;

/// The DDIs whose time is split into on and off the CPU.
const BLOCKING: [&str; 8] = [
    "present",
    "execute_command_lists",
    "signal_fence",
    "wait_for_fence",
    "map_heap",
    "unmap_heap",
    "make_resident",
    "evict",
];

/// Frames per line.
const EVERY: u64 = 256;

#[derive(Clone, Copy, Default)]
struct Bucket {
    wall_ns: u64,
    cycles: u64,
    calls: u64,
}

#[derive(Clone, Copy, Default)]
struct Window {
    started: Option<(Instant, u64, u64)>,
    frames: u64,
    /// Outermost DDIs on this thread, wall.
    ddi_ns: u64,
    blocking: [Bucket; BLOCKING.len()],
}

std::thread_local! {
    static PRESENTER: Cell<bool> = const { Cell::new(false) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static WINDOW: Cell<Window> = const { Cell::new(Window {
        started: None,
        frames: 0,
        ddi_ns: 0,
        blocking: [Bucket { wall_ns: 0, cycles: 0, calls: 0 }; BLOCKING.len()],
    }) };
}

/// A DDI's entry on the presenting thread; `None` everywhere else.
pub(crate) struct Entry {
    started: Instant,
    blocking: Option<(usize, u64)>,
    /// The outermost DDI on the thread; a nested one only keeps the depth.
    outermost: bool,
}

#[inline]
pub(crate) fn enabled() -> bool {
    crate::knobs12::umd12_wait_split()
}

/// Thread cycle time of the calling thread (TSC ticks, kernel and user).
fn thread_cycles() -> u64 {
    use windows::Win32::Foundation::{BOOL, HANDLE};
    use windows::Win32::System::Threading::GetCurrentThread;
    // kernel32, which this DLL imports already; declared here rather than
    // through the `windows` crate's `Win32_System_WindowsProgramming` feature.
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> BOOL;
    }
    let mut c = 0u64;
    // SAFETY: the pseudo-handle of this thread and a writable local.
    let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}

fn tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: RDTSC has no preconditions.
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(target_arch = "x86")]
    // SAFETY: as above.
    unsafe {
        core::arch::x86::_rdtsc()
    }
}

/// A DDI begins (called by `ddi_time` when the knob is on).
#[inline]
pub(crate) fn enter(name: &'static str) -> Option<Entry> {
    if !PRESENTER.with(Cell::get) {
        // The present DDI marks its thread before its first timed call ends.
        if name != "present" {
            return None;
        }
        PRESENTER.with(|p| p.set(true));
    }
    let depth = DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if depth != 0 {
        return Some(Entry { started: Instant::now(), blocking: None, outermost: false });
    }
    let blocking = BLOCKING.iter().position(|b| *b == name).map(|i| (i, thread_cycles()));
    Some(Entry { started: Instant::now(), blocking, outermost: true })
}

/// The DDI `enter` returned `entry` for ends.
#[inline]
pub(crate) fn leave(entry: Entry) {
    if !entry.outermost {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        return;
    }
    let wall = u64::try_from(entry.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let cycles = entry.blocking.map(|(i, c0)| (i, thread_cycles().saturating_sub(c0)));
    DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    WINDOW.with(|w| {
        let mut win = w.get();
        win.ddi_ns += wall;
        if let Some((i, c)) = cycles {
            let b = &mut win.blocking[i];
            b.wall_ns += wall;
            b.cycles += c;
            b.calls += 1;
        }
        w.set(win);
    });
}

/// One frame closed on the presenting thread (`FrameStats::note_present`).
pub(crate) fn on_present() {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let cyc = thread_cycles();
    let t = tsc();
    let done = WINDOW.with(|w| {
        let mut win = w.get();
        let Some(start) = win.started else {
            w.set(Window { started: Some((now, cyc, t)), ..Window::default() });
            return None;
        };
        win.frames += 1;
        if win.frames < EVERY {
            w.set(win);
            return None;
        }
        w.set(Window { started: Some((now, cyc, t)), ..Window::default() });
        Some((win, start))
    });
    if let Some((win, (t0, c0, tsc0))) = done {
        log_window(&win, now.duration_since(t0).as_nanos() as u64, cyc.saturating_sub(c0), t.saturating_sub(tsc0));
    }
}

fn log_window(win: &Window, wall_ns: u64, cycles: u64, tsc_ticks: u64) {
    let f = win.frames.max(1);
    // TSC ticks per ns over the window (thread cycle time counts TSC ticks).
    let per_ns = if wall_ns != 0 { tsc_ticks as f64 / wall_ns as f64 } else { 0.0 };
    let ns_of = |c: u64| if per_ns > 0.0 { (c as f64 / per_ns) as u64 } else { 0 };
    let us = |ns: u64| ns / f / 1000;
    let cpu_ns = ns_of(cycles).min(wall_ns);
    let mut blocking_wall = 0u64;
    let mut blocking_off = 0u64;
    let mut parts = String::new();
    for (i, b) in win.blocking.iter().enumerate() {
        if b.calls == 0 {
            continue;
        }
        let off = b.wall_ns.saturating_sub(ns_of(b.cycles));
        blocking_wall += b.wall_ns;
        blocking_off += off;
        parts.push_str(&format!(
            " {} {} us (off CPU {}, {} calls);",
            BLOCKING[i],
            us(b.wall_ns),
            us(off),
            b.calls / f
        ));
    }
    let other = win.ddi_ns.saturating_sub(blocking_wall);
    let outside = wall_ns.saturating_sub(win.ddi_ns);
    let off = wall_ns - cpu_ns;
    let outside_off = off.saturating_sub(blocking_off);
    crate::log_error!(
        "D3D12 presenting thread ({} frames): frame {} us = on CPU {} + off CPU {}; in driver \
         DDIs {} us, off CPU {}:{} other DDIs {} us (counted on the CPU); outside the driver {} \
         us, off CPU {} (runtime / application waits: fence events, frame-latency waits, sleeps)",
        win.frames,
        us(wall_ns),
        us(cpu_ns),
        us(off),
        us(win.ddi_ns),
        us(blocking_off),
        parts,
        us(other),
        us(outside),
        us(outside_off),
    );
}

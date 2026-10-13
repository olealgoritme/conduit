//! Completion hints (`Nvk12CompletionHint`, default 0): the engine's queue worker tells
//! the KMD which ExecuteCommandLists batches finished, so their HE12 v4 packets
//! complete without waiting for the host's `EventReady`.
//!
//! With `Nvk12EclSync=2` every NVK batch has an RM fence, and its WDDM packet
//! completes when the KMD sees that fence fire. The fire travels from the host
//! (nvidia-drm's semaphore-surface callback, the backend's event thread, the virtio
//! event queue, the KMD's DPC), and dxgkrnl admits a batch only once the packet two
//! ahead of it completed, so that trip sits between batches on the GPU whenever the
//! batch in flight is shorter than it. The worker (vkd3d patch 0011) can see a batch
//! finish in the guest: the batch's producer timeline is CPU-visible memory the GPU
//! writes right after it. While it waits for an admission it polls that timeline for
//! up to `Nvk12CompletionHint` microseconds and calls [`ecl_done_hint`] with the
//! fences of the batches it saw finish; this sends them to the KMD in one
//! `HELIOS_ESCAPE_ECL_DONE`, and the KMD fires the ones it holds for this process's
//! gate (`kmd_logic::ecl_done`). The host's `EventReady` that follows is a repeat.
//!
//! The escape goes through this module's own D3DKMT adapter and device (the KMD takes
//! the process from the device's `hKmdProcess`), opened once per process: the
//! callback runs on engine worker threads, where the runtime's callbacks are not to
//! be used. A KMD that does not know the verb fails the probe, and hints stay off.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use helios_protocol::{HeliosEclDone, HELIOS_ECL_DONE_MAX};

use crate::{ddi12, log_error};

#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTEnumAdapters2(arg: *mut ddi12::D3DKMT_ENUMADAPTERS2) -> i32;
    fn D3DKMTCloseAdapter(arg: *const ddi12::D3DKMT_CLOSEADAPTER) -> i32;
    fn D3DKMTCreateDevice(arg: *mut ddi12::D3DKMT_CREATEDEVICE) -> i32;
    fn D3DKMTDestroyDevice(arg: *const ddi12::D3DKMT_DESTROYDEVICE) -> i32;
    fn D3DKMTEscape(arg: *const ddi12::D3DKMT_ESCAPE) -> i32;
}

/// `D3DDDI_ESCAPEFLAGS` bit 3, `NoAdapterSynchronization`: the escape needs no
/// adapter-wide lock (it touches only the KMD's handle table, under its own lock).
const ESCAPE_NO_ADAPTER_SYNC: u32 = 1 << 3;

/// Most adapters looked at for the Helios one.
const MAX_ADAPTERS: usize = 16;

/// This process's D3DKMT adapter and device on the Helios adapter.
struct Client {
    adapter: ddi12::D3DKMT_HANDLE,
    device: ddi12::D3DKMT_HANDLE,
    flags: u32,
}

static CLIENT: OnceLock<Option<Client>> = OnceLock::new();

/// Counters for the log line and the bench export: escapes sent, escapes that
/// failed, and the KMD's verdicts on the fences they named.
pub(crate) struct HintStats {
    pub escapes: AtomicU64,
    pub failed: AtomicU64,
    pub fired: AtomicU64,
    pub late: AtomicU64,
    pub refused: AtomicU64,
}

pub(crate) static HINT_STATS: HintStats = HintStats {
    escapes: AtomicU64::new(0),
    failed: AtomicU64::new(0),
    fired: AtomicU64::new(0),
    late: AtomicU64::new(0),
    refused: AtomicU64::new(0),
};

/// Values in [`hint_stats`], in this order.
pub(crate) const HINT_STATS_LEN: usize = 5;

pub(crate) fn hint_stats() -> [u64; HINT_STATS_LEN] {
    let s = &HINT_STATS;
    [
        s.escapes.load(Ordering::Relaxed),
        s.failed.load(Ordering::Relaxed),
        s.fired.load(Ordering::Relaxed),
        s.late.load(Ordering::Relaxed),
        s.refused.load(Ordering::Relaxed),
    ]
}

/// One `HELIOS_ESCAPE_ECL_DONE` naming `fences` (at most [`HELIOS_ECL_DONE_MAX`]).
/// The NTSTATUS, and the request with the KMD's out fields.
fn send(
    adapter: ddi12::D3DKMT_HANDLE,
    device: ddi12::D3DKMT_HANDLE,
    flags: u32,
    fences: &[u32],
) -> (i32, HeliosEclDone) {
    let mut req = HeliosEclDone::new(fences);
    // SAFETY: an all-zero D3DKMT_ESCAPE is a valid value (handles, a type of 0, flags,
    // a null pointer, sizes); every field the call reads is set below.
    let mut esc: ddi12::D3DKMT_ESCAPE = unsafe { core::mem::zeroed() };
    esc.hAdapter = adapter;
    esc.hDevice = device;
    esc.Type = ddi12::_D3DKMT_ESCAPETYPE_D3DKMT_ESCAPE_DRIVERPRIVATE;
    esc.Flags.__bindgen_anon_1.Value = flags;
    esc.pPrivateDriverData = (&mut req as *mut HeliosEclDone).cast();
    esc.PrivateDriverDataSize = core::mem::size_of::<HeliosEclDone>() as u32;
    // SAFETY: a fully initialised argument; `req` outlives the call, and the KMD writes
    // at most `PrivateDriverDataSize` bytes back into it.
    let status = unsafe { D3DKMTEscape(&esc) };
    (status, req)
}

/// Find the Helios adapter (the one that answers this verb), make a device on it and
/// keep both. `None` (nothing kept) when no adapter answers.
fn open() -> Option<Client> {
    // SAFETY: zeroed POD out-structs for the documented two-call enumeration.
    let mut infos: [ddi12::D3DKMT_ADAPTERINFO; MAX_ADAPTERS] = unsafe { core::mem::zeroed() };
    let mut en: ddi12::D3DKMT_ENUMADAPTERS2 = unsafe { core::mem::zeroed() };
    en.NumAdapters = MAX_ADAPTERS as u32;
    en.pAdapters = infos.as_mut_ptr();
    // SAFETY: `pAdapters` holds `NumAdapters` entries.
    if unsafe { D3DKMTEnumAdapters2(&mut en) } != 0 {
        log_error!("NVK completion hints: D3DKMTEnumAdapters2 failed; hints off");
        return None;
    }
    let count = (en.NumAdapters as usize).min(MAX_ADAPTERS);
    let mut found = None;
    for info in &infos[..count] {
        if found.is_none() {
            found = probe(info.hAdapter);
        }
        if !matches!(&found, Some(c) if c.adapter == info.hAdapter) {
            // SAFETY: an adapter handle the enumeration opened for this process.
            let close = ddi12::D3DKMT_CLOSEADAPTER {
                hAdapter: info.hAdapter,
            };
            unsafe { D3DKMTCloseAdapter(&close) };
        }
    }
    if found.is_none() {
        log_error!("NVK completion hints: no adapter serves HELIOS_ESCAPE_ECL_DONE (older KMD?); hints off");
    }
    found
}

/// A device on `adapter` and the verb's answer with nothing to fire (handle 0): the
/// Helios KMD that serves it answers success with one refusal. Tried without, then
/// with the adapter lock. `None` (device destroyed) when the adapter does not serve it.
fn probe(adapter: ddi12::D3DKMT_HANDLE) -> Option<Client> {
    // SAFETY: zeroed POD; `hAdapter` (the union's first arm) is set before the call.
    let mut cd: ddi12::D3DKMT_CREATEDEVICE = unsafe { core::mem::zeroed() };
    cd.__bindgen_anon_1.hAdapter = adapter;
    // SAFETY: a valid adapter handle of this process.
    if unsafe { D3DKMTCreateDevice(&mut cd) } != 0 {
        return None;
    }
    let device = cd.hDevice;
    for flags in [ESCAPE_NO_ADAPTER_SYNC, 0] {
        let (status, reply) = send(adapter, device, flags, &[0]);
        if status == 0 && reply.out_refused == 1 {
            log_error!(
                "NVK completion hints: on (adapter {adapter:#x}, escape flags {flags:#x}, \
                 poll {} us per admission wait)",
                crate::knobs12::nvk12_completion_hint_us()
            );
            return Some(Client {
                adapter,
                device,
                flags,
            });
        }
    }
    let destroy = ddi12::D3DKMT_DESTROYDEVICE { hDevice: device };
    // SAFETY: the device made above, used by nothing else.
    unsafe { D3DKMTDestroyDevice(&destroy) };
    None
}

/// The engine worker's callback (vkd3d patch 0011): name `count` finished batches'
/// ECL fences to the KMD. Called on engine worker threads, any number at once.
///
/// # Safety
/// `fences` points at `count` readable `u32`s (or `count` is 0).
unsafe extern "C" fn ecl_done_hint(fences: *const u32, count: u32) {
    if fences.is_null() || count == 0 {
        return;
    }
    let Some(Some(c)) = CLIENT.get() else {
        return;
    };
    // SAFETY: the caller's guarantee.
    let all = unsafe { core::slice::from_raw_parts(fences, count as usize) };
    for chunk in all.chunks(HELIOS_ECL_DONE_MAX) {
        let (status, reply) = send(c.adapter, c.device, c.flags, chunk);
        HINT_STATS.escapes.fetch_add(1, Ordering::Relaxed);
        if status != 0 {
            let n = HINT_STATS.failed.fetch_add(1, Ordering::Relaxed);
            if n < 4 || n % 4096 == 0 {
                log_error!(
                    "NVK completion hints: escape failed status={:#010x} (x{})",
                    status as u32,
                    n + 1
                );
            }
            continue;
        }
        HINT_STATS
            .fired
            .fetch_add(u64::from(reply.out_fired), Ordering::Relaxed);
        HINT_STATS
            .late
            .fetch_add(u64::from(reply.out_late), Ordering::Relaxed);
        HINT_STATS
            .refused
            .fetch_add(u64::from(reply.out_refused), Ordering::Relaxed);
    }
}

/// Turn completion hints on for this process when `Nvk12CompletionHint` asks and the
/// device runs ECL fences (`Nvk12EclSync=2` served). Once per process: the first
/// device decides, later ones change nothing.
pub(crate) fn enable_if_wanted(dev: &crate::device12::HeliosD3D12Device) {
    let spin_us = crate::knobs12::nvk12_completion_hint_us();
    if spin_us == 0 || !dev.engine.is_nvk() || !super::nvk12::ecl_fences_wanted(dev) {
        return;
    }
    let client = CLIENT.get_or_init(open);
    if client.is_some() {
        crate::bridge12::set_completion_hint(ecl_done_hint as usize, spin_us);
    }
}

/// Values [`helios_umd12_ecl_stats_v1`] writes.
pub(crate) const ECL_STATS_V1_LEN: usize = crate::bridge12::ECL_FENCE_STATS_LEN + HINT_STATS_LEN;

/// The process's admission and completion-hint counters, for tools in the same
/// process (`tools/d3d12_gpu_bound.cpp`, test `submit`), which read them before and
/// after a run: the engine's [`crate::bridge12::ecl_fence_stats`] (33 values; index 14
/// admissions, 15 commit-to-admission sum in us, 16..=22 its histogram <50 us, <200 us,
/// <1 ms, <2 ms, <5 ms, <20 ms, >=20 ms; 28 hint calls, 29 fences named, 30 ring
/// entries overwritten unseen, 31 done-to-admission sum in us, 32 its count), then
/// [`hint_stats`] (escapes, failed escapes, fired, late, refused). Returns how many it
/// wrote (at most `count`).
///
/// # Safety
/// `out` points at `count` writable `u64`s (or `count` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_umd12_ecl_stats_v1(out: *mut u64, count: u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    let mut all = [0u64; ECL_STATS_V1_LEN];
    let st = crate::bridge12::ecl_fence_stats();
    all[..st.len()].copy_from_slice(&st);
    all[st.len()..].copy_from_slice(&hint_stats());
    let n = (count as usize).min(ECL_STATS_V1_LEN);
    // SAFETY: the caller's guarantee; `n <= count`.
    unsafe { core::ptr::copy_nonoverlapping(all.as_ptr(), out, n) };
    n as u32
}

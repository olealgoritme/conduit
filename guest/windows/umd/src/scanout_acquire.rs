//! D4a scanout-read acquire — the UMD half (FIX-DESIGN-d4a.md §4).
//!
//! The KMD keeps a generation-qualified READ LEDGER (one nonpaged page:
//! `resid`, `generation`, `issued`, and `retired`) and signals every
//! registered auto-reset event after retirements. This module is the UMD's
//! plumbing for that contract:
//!
//!   * probe the capability once per process (`HELIOS_ESCAPE_MAP_READ_LEDGER`
//!     op PROBE — an old KMD fails the escape from its unknown-verb arm, and
//!     the whole feature latches OFF with one log line);
//!   * map the ledger page read-only into this process, per device (op MAP;
//!     the KMD tracks the mapping owner-keyed, so a per-device view is the one
//!     shape that is correct under every owner-reclaim implementation);
//!   * create + register one auto-reset event per device
//!     (`HELIOS_ESCAPE_SCANOUT_EVENT` op REGISTER) and hand it to the DXVK
//!     engine's per-device signaler thread through the bridge. A refused
//!     event is closed and DXVK gets none (it then polls the ledger at 1 ms);
//!     the device asks again on its presents and flushes
//!     ([`retry_register`], `helios_umd_common::scanout_event`);
//!   * export the reader surface (`helios_scanout_*`) the statically linked
//!     DXVK engine resolves BY NAME from this DLL (dxvk-helios/src/dxvk/
//!     dxvk_helios_scanout_acquire.cpp) — by-name rather than a link-time
//!     reference so the fork's standalone d3d11.dll target keeps linking.
//!
//! Escape transport: `pfnEscapeCb` from the bindgen'd `D3DDDI_DEVICECALLBACKS`
//! — previously unused. Per the WDK's own signature the first argument is the
//! **runtime adapter handle** (`PFND3DDDI_ESCAPECB(hAdapter, ...)`), captured
//! at `OpenAdapter*`; `D3DDDICB_ESCAPE.hDevice` carries the runtime **device**
//! handle, which dxgkrnl resolves to the KM device handle the KMD mints its
//! `DeviceOwner` from (kmd_render ddi/escape.rs). Flags stay all-zero:
//! `HardwareAccess = 0` is the 26th-session owner directive (a HardwareAccess
//! escape serializes on the dxgkrnl adapter CORE resource), and these verbs
//! are PASSIVE. Ledger/event operations are device-scoped; snapshot release
//! queries also pass the presenting context to inspect its pending descriptor.
//!
//! Concurrency model: one process-global registry (`Mutex<Vec<DeviceEntry>>`).
//! Every ledger read — the DXVK exports below — happens UNDER that mutex and
//! only through a currently-registered mapping, and teardown removes the entry
//! under the same mutex before it unmaps, so a reader can never touch a VA
//! whose device is mid-destroy. The reads are a handful of atomic loads per
//! flush; the mutex is uncontended at that rate. The one lock-free fast path
//! is [`helios_scanout_acquire_enabled`], a single Acquire load, which is what
//! keeps the knob-off / probe-failed path bit-identical to a build without the
//! mechanism.

use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use helios_umd_common::scanout_event::{handoff, RetrySchedule};

use helios_protocol::{
    HeliosEscapeSnapshotStatus, HELIOS_ESCAPE_SNAPSHOT_STATUS, HELIOS_SCANOUT_CAP_SNAPSHOT_STATUS,
    HELIOS_SNAPSHOT_BUSY, HELIOS_SNAPSHOT_IDLE,
    HeliosEscapeHeader, HeliosEscapeMapReadLedger, HeliosEscapeScanoutEvent, HeliosReadLedgerPage,
    HeliosReadLedgerSlot, HELIOS_ESCAPE_MAP_READ_LEDGER, HELIOS_ESCAPE_SCANOUT_EVENT,
    HELIOS_READ_LEDGER_MAGIC, HELIOS_READ_LEDGER_SLOTS, HELIOS_READ_LEDGER_VERSION,
    HELIOS_SCANOUT_ACQ_OK, HELIOS_SCANOUT_ACQ_OP_MAP, HELIOS_SCANOUT_ACQ_OP_PROBE,
    HELIOS_SCANOUT_ACQ_OP_REGISTER, HELIOS_SCANOUT_ACQ_OP_UNMAP, HELIOS_SCANOUT_ACQ_OP_UNREGISTER,
    HELIOS_SCANOUT_ACQ_PROBE_ACK, HELIOS_SCANOUT_ACQ_TABLE_FULL,
    HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM, HELIOS_SCANOUT_CAP_READ_LEDGER,
    HELIOS_SCANOUT_CAP_SNAPSHOT_BIND, HELIOS_SCANOUT_CAP_WINDOWED_BLT_SNAPSHOT,
};

use crate::ddi;
use crate::device_funcs::HeliosDevice;
use crate::log_error;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        security_attributes: *mut core::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut core::ffi::c_void;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

// The KMD-view loss table shared with the Venus ICD
// (`umd_common/bridge/helios_kmdmap.h`, compiled in by `bridge_kmdmap.cpp`).
// The KMD maps the ledger page into this process itself and unmaps it in
// DxgkDdiDestroyDevice when it stops under live processes (a live driver
// update); a registered page that vanishes is backed with zeros instead of
// faulting (dwm.exe died here at the 319.1 -> 319.2 swap), and the device's
// recorded loss epoch moves so the readers below stop using it.
unsafe extern "C" {
    fn helios_kmdmap_c_attach() -> i32;
    fn helios_kmdmap_c_detach();
    fn helios_kmdmap_c_register(va: *const core::ffi::c_void, size: u64, owner: u64);
    fn helios_kmdmap_c_unregister_owner(owner: u64);
    fn helios_kmdmap_c_lost(epoch: i32) -> bool;
}

/// Probe latch. UNKNOWN until the first device init runs the probe; then OK or
/// OFF for the rest of the process — "never retry per present" is the §4
/// contract, and per-device init reads the latch instead of re-probing.
const PROBE_UNKNOWN: u32 = 0;
const PROBE_OK: u32 = 1;
const PROBE_OFF: u32 = 2;
static PROBE_STATE: AtomicU32 = AtomicU32::new(PROBE_UNKNOWN);
static LEDGER_ADDRESS_REFUSALS: AtomicUsize = AtomicUsize::new(0);

/// The PROBE reply's `out_size` capability bitmask
/// (`HELIOS_SCANOUT_CAP_*`), latched beside [`PROBE_STATE`] when the ACK
/// arrives. A pre-capability KMD (<= 22.22.222.0) ACKs with `out_size == 0`
/// — no bits, so features gated on a bit stay OFF (skew-safe in both
/// directions). Only meaningful once `PROBE_STATE == PROBE_OK`.
static PROBE_CAPS: AtomicU32 = AtomicU32::new(0);

/// D4b: whether the KMD advertised `HELIOS_SCANOUT_CAP_SNAPSHOT_BIND` in the
/// probe reply, i.e. it honors `HELIOS_PRESENT_PRIVATE_FLAG_SNAPSHOT` on the
/// DMA-flip bind path. Two relaxed loads — cheap enough for the per-present
/// substitution gate. `false` until the probe has ACK'd, which also covers
/// the `ScanoutAcquire=0` case (the probe never runs, so the capability can
/// never latch; `init_for_device` logs that coupling once per device).
pub(crate) fn scanout_snapshot_capable() -> bool {
    // Acquire on the state pairs with the Release store in `init_for_device`:
    // seeing PROBE_OK guarantees the caps store that preceded it is visible.
    PROBE_STATE.load(Ordering::Acquire) == PROBE_OK
        && PROBE_CAPS.load(Ordering::Relaxed) & HELIOS_SCANOUT_CAP_SNAPSHOT_BIND != 0
}

/// Whether the KMD probe explicitly advertised registered monotonic
/// present-stream markers.  The acquire probe is the shared capability latch;
/// an old KMD, a probe failure, or `ScanoutAcquire=0` stays false and forces
/// the historical present gate.
pub(crate) fn async_present_stream_capable() -> bool {
    PROBE_STATE.load(Ordering::Acquire) == PROBE_OK
        && PROBE_CAPS.load(Ordering::Relaxed) & HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM != 0
}

/// Whether the KMD can consume a typed snapshot as the source of a windowed
/// BLT. This requires the same probe/ledger transport as direct snapshots but
/// is deliberately a separate capability: a direct-bind-only KMD must retain
/// its existing windowed copy path.
pub(crate) fn windowed_blt_snapshot_capable() -> bool {
    PROBE_STATE.load(Ordering::Acquire) == PROBE_OK && {
        let caps = PROBE_CAPS.load(Ordering::Relaxed);
        caps & (HELIOS_SCANOUT_CAP_WINDOWED_BLT_SNAPSHOT
            | HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM
            | HELIOS_SCANOUT_CAP_READ_LEDGER)
            == (HELIOS_SCANOUT_CAP_WINDOWED_BLT_SNAPSHOT
                | HELIOS_SCANOUT_CAP_ASYNC_PRESENT_STREAM
                | HELIOS_SCANOUT_CAP_READ_LEDGER)
    }
}

/// The lock-free fast-path flag the DXVK export reads once per flush:
/// knob ON && probe OK && at least one live ledger mapping. Maintained under
/// the registry mutex, read anywhere.
static ENABLED: AtomicU32 = AtomicU32::new(0);


/// Per-device acquire state. `key` is the `HeliosDevice` pointer value —
/// identity only, never dereferenced here.
struct DeviceEntry {
    key: usize,
    /// Runtime device handle for `D3DDDICB_ESCAPE.hDevice` (owner identity).
    h_rt_device: usize,
    /// Runtime adapter handle this device's escapes go through.
    rt_adapter: usize,
    /// The device's `D3DDDI_DEVICECALLBACKS` table — runtime-owned, valid for
    /// the device DDI lifetime (teardown runs inside `DestroyDevice`).
    kt_callbacks: usize,
    /// User VA of this device's read-only [`HeliosReadLedgerPage`] view.
    /// 0 = mapping failed; the entry then contributes nothing to lookups.
    ledger_va: usize,
    /// Loss epoch at the time the ledger view was registered in the shared
    /// KMD-view table (only meaningful with `ledger_va != 0`). Once the epoch
    /// moves the KMD is gone and the view is zeros, so readers skip it.
    loss_epoch: i32,
    /// Owned auto-reset event handle the KMD accepted (0 = none: creation
    /// failed or REGISTER was refused; the DXVK signaler then polls the
    /// ledger every 1 ms while a gate is armed). Nonzero means registered.
    event: usize,
    /// Set while a refused registration waits for its next attempt (only for
    /// a device with a live ledger mapping). Counted in [`RETRY_PENDING`].
    retry: Option<RetrySchedule>,
}

/// Registry entries with `retry` set: the present/flush hook's one relaxed
/// load ([`retry_register`]) skips the registry while this is 0.
static RETRY_PENDING: AtomicUsize = AtomicUsize::new(0);

/// The earliest `retry` due time of any entry ([`now_ms`] clock), so the hook
/// takes the registry mutex only when some retry is due, not on every
/// present while one waits. Lowered on every new schedule; a stale low value
/// only costs one lock that finds nothing due.
static RETRY_NEXT_DUE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

fn note_retry_due(due_ms: u64) {
    RETRY_NEXT_DUE_MS.fetch_min(due_ms, Ordering::Relaxed);
}

/// Milliseconds since the first call (the retry clock).
fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Create an auto-reset event, REGISTER it, and keep it only if the KMD
/// accepted it. Returns the registered handle, 0 when refused (the refused
/// handle is closed here: an unsignaled event handed to DXVK would put its
/// signaler on the 10 ms timeout instead of the 1 ms poll).
///
/// # Safety
/// Same contract as [`call_escape`].
unsafe fn create_and_register(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    attempt: &str,
) -> usize {
    // Auto-reset: the signaler is level-triggered, every wake re-reads the
    // ledger, so a lost or coalesced signal is a bounded hiccup, never a hang.
    // SAFETY: plain kernel32 call; all-null/0 arguments are the documented
    // anonymous auto-reset unsignaled shape.
    let event = unsafe { CreateEventW(core::ptr::null_mut(), 0, 0, core::ptr::null()) } as usize;
    if event == 0 {
        log_error!("scanout-acquire: CreateEventW failed ({attempt}) — signaler polls the ledger every 1 ms");
        return 0;
    }
    // SAFETY: the caller's contract; `event` is a live handle in this process.
    let reply = unsafe {
        escape_scanout_event(
            kt_callbacks,
            rt_adapter,
            h_rt_device,
            HELIOS_SCANOUT_ACQ_OP_REGISTER,
            event,
        )
    };
    let decision = handoff(reply, HELIOS_SCANOUT_ACQ_OK, event);
    if let Some(closed) = decision.to_close() {
        match reply {
            Ok(HELIOS_SCANOUT_ACQ_TABLE_FULL) => log_error!(
                "scanout-acquire: event REGISTER refused, table full ({attempt}; counted AqRgF) — \
                 signaler polls the ledger every 1 ms, REGISTER retried on present/flush"
            ),
            Ok(state) => log_error!(
                "scanout-acquire: event REGISTER answered out_state={state} ({attempt}) — \
                 signaler polls the ledger every 1 ms, REGISTER retried on present/flush"
            ),
            Err(hr) => log_error!(
                "scanout-acquire: event REGISTER escape failed hr=0x{:08x} ({attempt}) — \
                 signaler polls the ledger every 1 ms, REGISTER retried on present/flush",
                hr as u32
            ),
        }
        // SAFETY: created above, never shared: the KMD refused it (it took
        // no reference) and nothing else has seen the handle.
        unsafe { CloseHandle(closed as *mut core::ffi::c_void) };
    }
    decision.delivered()
}

static REGISTRY: Mutex<Vec<DeviceEntry>> = Mutex::new(Vec::new());

/// Build and issue one Helios escape through `pfnEscapeCb`.
///
/// Returns the callback's HRESULT (negative = failed). The payload is read and
/// written in place.
///
/// # Safety
/// `kt_callbacks` must point at the device's live callback table and
/// `payload`/`payload_size` must describe a writable escape struct.
unsafe fn call_escape(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    h_rt_context: usize,
    payload: *mut core::ffi::c_void,
    payload_size: u32,
) -> i32 {
    // SAFETY: caller guarantees the table pointer; a None slot is answered
    // with a failure HRESULT instead of a call through null.
    let Some(escape_cb) = (unsafe { &*kt_callbacks }).pfnEscapeCb else {
        return i32::MIN; // distinct “no callback” failure; logged by the caller
    };
    let mut esc = ddi::D3DDDICB_ESCAPE::default();
    // Flags stay zero-initialized: HardwareAccess = 0 (owner directive; these
    // verbs are PASSIVE and non-blocking), no DeviceStatusQuery, nothing else.
    esc.hDevice = h_rt_device as ddi::HANDLE;
    esc.hContext = h_rt_context as ddi::HANDLE;
    esc.pPrivateDriverData = payload;
    esc.PrivateDriverDataSize = payload_size;
    // SAFETY: the callback contract (PFND3DDDI_ESCAPECB) takes the runtime
    // adapter handle and a fully-initialized D3DDDICB_ESCAPE whose buffer
    // outlives the call; both hold here.
    unsafe { escape_cb(rt_adapter as ddi::HANDLE, &esc) }
}

/// One `HELIOS_ESCAPE_MAP_READ_LEDGER` round-trip.
///
/// # Safety
/// Same contract as [`call_escape`].
unsafe fn escape_map_ledger(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    op: u32,
) -> Result<HeliosEscapeMapReadLedger, i32> {
    let mut payload = HeliosEscapeMapReadLedger {
        hdr: HeliosEscapeHeader::new(
            HELIOS_ESCAPE_MAP_READ_LEDGER,
            size_of::<HeliosEscapeMapReadLedger>() as u32,
        ),
        out_user_va: 0,
        op,
        out_size: 0,
        out_state: !0, // never a legal out_state: a KMD that answers must overwrite it
        _pad: 0,
    };
    // SAFETY: `payload` is a live stack struct of exactly the advertised size.
    let hr = unsafe {
        call_escape(
            kt_callbacks,
            rt_adapter,
            h_rt_device,
            0,
            (&mut payload as *mut HeliosEscapeMapReadLedger).cast(),
            size_of::<HeliosEscapeMapReadLedger>() as u32,
        )
    };
    if hr < 0 {
        Err(hr)
    } else {
        Ok(payload)
    }
}

/// One `HELIOS_ESCAPE_SCANOUT_EVENT` round-trip.
///
/// # Safety
/// Same contract as [`call_escape`].
unsafe fn escape_scanout_event(
    kt_callbacks: *const ddi::D3DDDI_DEVICECALLBACKS,
    rt_adapter: usize,
    h_rt_device: usize,
    op: u32,
    event: usize,
) -> Result<u32, i32> {
    let mut payload = HeliosEscapeScanoutEvent {
        hdr: HeliosEscapeHeader::new(
            HELIOS_ESCAPE_SCANOUT_EVENT,
            size_of::<HeliosEscapeScanoutEvent>() as u32,
        ),
        event_handle: event as u64,
        op,
        out_state: !0,
    };
    // SAFETY: `payload` is a live stack struct of exactly the advertised size.
    let hr = unsafe {
        call_escape(
            kt_callbacks,
            rt_adapter,
            h_rt_device,
            0,
            (&mut payload as *mut HeliosEscapeScanoutEvent).cast(),
            size_of::<HeliosEscapeScanoutEvent>() as u32,
        )
    };
    if hr < 0 {
        Err(hr)
    } else {
        Ok(payload.out_state)
    }
}

/// Called only between serialized presents, after the caller stops selecting
/// this private WindowedBlt ring. Direct-flip ownership is outside this query.
pub(crate) fn windowed_snapshot_idle(dev: &HeliosDevice, resource_id: u32) -> bool {
    if PROBE_CAPS.load(Ordering::Acquire) & HELIOS_SCANOUT_CAP_SNAPSHOT_STATUS == 0 {
        return false;
    }
    let Some(context) = dev.context.as_ref() else {
        return false;
    };
    let rt_adapter = dev.rt_adapter;
    if rt_adapter == 0 || dev.kt_callbacks.is_null() {
        return false;
    }
    let mut payload = HeliosEscapeSnapshotStatus {
        hdr: HeliosEscapeHeader::new(
            HELIOS_ESCAPE_SNAPSHOT_STATUS,
            size_of::<HeliosEscapeSnapshotStatus>() as u32,
        ),
        resource_id,
        out_state: HELIOS_SNAPSHOT_BUSY,
    };
    // SAFETY: device/context and callback table outlive this DDI; payload is a
    // writable stack struct with the exact advertised size.
    let hr = unsafe {
        call_escape(
            dev.kt_callbacks,
            rt_adapter,
            dev.h_rt_device as usize,
            context.handle.as_ptr() as usize,
            (&mut payload as *mut HeliosEscapeSnapshotStatus).cast(),
            size_of::<HeliosEscapeSnapshotStatus>() as u32,
        )
    };
    hr == 0 && payload.out_state == HELIOS_SNAPSHOT_IDLE
}

/// Acquire-load one `u32` field of the mapped ledger page.
///
/// # Safety
/// `va + offset` must lie inside a currently-mapped ledger page — guaranteed
/// by only calling this under the registry mutex against a registered entry.
#[inline]
unsafe fn ledger_load(va: usize, offset: usize) -> u32 {
    // SAFETY: caller guarantees the address is inside the live 4 KiB mapping;
    // all page fields are naturally aligned u32s, so the AtomicU32 view is
    // valid, and atomics are the correct way to read KMD-shared memory.
    unsafe { (*((va + offset) as *const AtomicU32)).load(Ordering::Acquire) }
}

/// Acquire-load one naturally aligned `u64` field of the mapped ledger page.
///
/// # Safety
/// Same mapping-lifetime and bounds contract as [`ledger_load`]. The v2 wire
/// layout explicitly aligns every shared `u64`, so this atomic view is valid.
#[inline]
unsafe fn ledger_load_u64(va: usize, offset: usize) -> u64 {
    // SAFETY: caller guarantees a live mapping and the protocol layout pins the
    // 8-byte alignment of generation/issued/retired.
    unsafe { (*((va + offset) as *const AtomicU64)).load(Ordering::Acquire) }
}

const LEDGER_MAGIC_OFF: usize = offset_of!(HeliosReadLedgerPage, magic);
const LEDGER_VERSION_OFF: usize = offset_of!(HeliosReadLedgerPage, version);
const LEDGER_SLOT_COUNT_OFF: usize = offset_of!(HeliosReadLedgerPage, slot_count);
const LEDGER_SLOTS_OFF: usize = offset_of!(HeliosReadLedgerPage, slots);
const SLOT_SIZE: usize = size_of::<HeliosReadLedgerSlot>();
const SLOT_RESID_OFF: usize = offset_of!(HeliosReadLedgerSlot, resid);
const SLOT_GENERATION_OFF: usize = offset_of!(HeliosReadLedgerSlot, generation);
const SLOT_ISSUED_OFF: usize = offset_of!(HeliosReadLedgerSlot, issued);
const SLOT_RETIRED_OFF: usize = offset_of!(HeliosReadLedgerSlot, retired);

/// Validate a freshly-mapped ledger page's complete v2 header. A mismatch
/// means a driver skew and the feature must treat the page as absent.
///
/// # Safety
/// `va` must be a live mapping of at least `size_of::<HeliosReadLedgerPage>()`.
unsafe fn ledger_page_valid(va: usize) -> bool {
    // SAFETY: caller guarantees the mapping covers the page header.
    let magic = unsafe { ledger_load(va, LEDGER_MAGIC_OFF) };
    // SAFETY: as above.
    let version = unsafe { ledger_load(va, LEDGER_VERSION_OFF) };
    // SAFETY: as above.
    let slot_count = unsafe { ledger_load(va, LEDGER_SLOT_COUNT_OFF) };
    magic == HELIOS_READ_LEDGER_MAGIC
        && version == HELIOS_READ_LEDGER_VERSION
        && slot_count == HELIOS_READ_LEDGER_SLOTS as u32
}

/// A ledger view the readers may use: mapped, and the KMD that mapped it has
/// not gone away since (after a loss the view is zero pages, see above).
fn ledger_live(e: &DeviceEntry) -> bool {
    // SAFETY: plain call into bridge_kmdmap.cpp.
    e.ledger_va != 0 && !unsafe { helios_kmdmap_c_lost(e.loss_epoch) }
}

/// Recompute the fast-path flag from the registry. Call under the mutex.
fn recompute_enabled(entries: &[DeviceEntry]) {
    let on = crate::scanout_acquire_knob()
        && PROBE_STATE.load(Ordering::Relaxed) == PROBE_OK
        && entries.iter().any(|e| e.ledger_va != 0);
    ENABLED.store(on as u32, Ordering::Release);
}

/// Wire one freshly-created device into the acquire machinery. Called from
/// `create_device` after the device is committed (post-defuse), so failure
/// here never has to unwind device creation — every failure arm degrades to
/// "feature off for this device", counted and logged once.
///
/// Returns the registered auto-reset event handle to hand to the DXVK device's
/// signaler (0 = none: creation failed or the KMD refused it; the signaler
/// then polls the ledger every 1 ms while a gate is armed, and the device
/// retries the REGISTER on present/flush).
pub(crate) fn init_for_device(dev: &HeliosDevice) -> usize {
    if !crate::scanout_acquire_knob() {
        // Kill switch: no escapes, no event, no mapping — bit-identical off.
        // COUPLING, stated loudly: the D4b snapshot capability rides THIS
        // probe, so ScanoutAcquire=0 also leaves `scanout_snapshot_capable`
        // false and the snapshot substitution off, whatever ScanoutSnapshot
        // says. One line per device create, not per present.
        if crate::scanout_snapshot_knob() {
            log_error!(
                "scanout-acquire: ScanoutAcquire=0 skips the capability probe — \
                 D4b snapshot substitution stays OFF for this process"
            );
        }
        return 0;
    }
    if dev.kt_callbacks.is_null() {
        return 0;
    }
    let rt_adapter = dev.rt_adapter;
    if rt_adapter == 0 {
        log_error!(
            "scanout-acquire: no runtime adapter handle captured; feature off for this device"
        );
        return 0;
    }
    let key = dev as *const HeliosDevice as usize;
    let h_rt_device = dev.h_rt_device as usize;
    let kt_callbacks = dev.kt_callbacks;

    let Ok(mut reg) = REGISTRY.lock() else {
        return 0;
    };

    // Probe once per process. An old KMD answers STATUS_NOT_IMPLEMENTED from
    // its unknown-verb arm — the capability signal — and the feature latches
    // OFF for the process: one log line, never retried per present (§4).
    match PROBE_STATE.load(Ordering::Relaxed) {
        PROBE_OK => {}
        PROBE_OFF => return 0,
        _ => {
            // SAFETY: `kt_callbacks` is the device's live callback table and
            // the payload is a stack struct sized to its own advertisement.
            let probed = unsafe {
                escape_map_ledger(
                    kt_callbacks,
                    rt_adapter,
                    h_rt_device,
                    HELIOS_SCANOUT_ACQ_OP_PROBE,
                )
            };
            match probed {
                Ok(p) if p.out_state == HELIOS_SCANOUT_ACQ_PROBE_ACK => {
                    // The ACK's out_size is the capability bitmask
                    // (HELIOS_SCANOUT_CAP_*; 0 from a pre-capability KMD).
                    // Caps first, then the Release store of OK: an Acquire
                    // reader that sees OK (`scanout_snapshot_capable`) is
                    // guaranteed to see the caps that came with it.
                    PROBE_CAPS.store(p.out_size, Ordering::Relaxed);
                    PROBE_STATE.store(PROBE_OK, Ordering::Release);
                    log_error!(
                        "scanout-acquire: KMD probe ACK — read-ledger capability present, caps=0x{:x}",
                        p.out_size
                    );
                }
                Ok(p) => {
                    PROBE_STATE.store(PROBE_OFF, Ordering::Relaxed);
                    log_error!(
                        "scanout-acquire: probe answered out_state={} (not ACK) — feature OFF",
                        p.out_state
                    );
                    return 0;
                }
                Err(hr) => {
                    PROBE_STATE.store(PROBE_OFF, Ordering::Relaxed);
                    log_error!(
                        "scanout-acquire: probe escape failed hr=0x{:08x} (old KMD?) — feature OFF",
                        hr as u32
                    );
                    return 0;
                }
            }
        }
    }

    // Loss epoch recorded when the ledger view is registered (below).
    let mut loss_epoch: i32 = 0;
    // Map this device's read-only view of the ledger page.
    // SAFETY: as for the probe above.
    let ledger_va = match unsafe {
        escape_map_ledger(
            kt_callbacks,
            rt_adapter,
            h_rt_device,
            HELIOS_SCANOUT_ACQ_OP_MAP,
        )
    } {
        Ok(m)
            if m.out_state == HELIOS_SCANOUT_ACQ_OK
                && m.out_user_va != 0
                && (m.out_size as usize) >= size_of::<HeliosReadLedgerPage>() =>
        {
            let va = usize::try_from(m.out_user_va)
                .ok()
                .filter(|va| va.checked_add(size_of::<HeliosReadLedgerPage>()).is_some());
            if va.is_none() {
                let n = LEDGER_ADDRESS_REFUSALS.fetch_add(1, Ordering::Relaxed) + 1;
                log_error!(
                    "scanout-acquire: ledger address exceeds process width va=0x{:x} refusals={n}",
                    m.out_user_va
                );
            }
            // SAFETY: the KMD mapped the complete page into this process;
            // the checked conversion preserves its address and full extent.
            let valid = va.filter(|va| unsafe { ledger_page_valid(*va) });
            if let Some(va) = valid {
                // SAFETY: plain calls into bridge_kmdmap.cpp; `va` is the
                // KMD's live view of `out_size` bytes, owned by this device
                // (keyed by `key`) until teardown unregisters it.
                unsafe {
                    loss_epoch = helios_kmdmap_c_attach();
                    helios_kmdmap_c_register(
                        va as *const core::ffi::c_void,
                        u64::from(m.out_size),
                        key as u64,
                    );
                }
                va
            } else {
                log_error!(
                    "scanout-acquire: mapped ledger page failed v2 header check — unmapping, feature off for this device"
                );
                // Best effort: return the bogus mapping rather than leak it.
                // SAFETY: as for the map call.
                let _ = unsafe {
                    escape_map_ledger(
                        kt_callbacks,
                        rt_adapter,
                        h_rt_device,
                        HELIOS_SCANOUT_ACQ_OP_UNMAP,
                    )
                };
                0
            }
        }
        Ok(m) => {
            log_error!(
                "scanout-acquire: MAP refused out_state={} va=0x{:x} size={} — feature off for this device",
                m.out_state,
                m.out_user_va,
                m.out_size
            );
            0
        }
        Err(hr) => {
            log_error!(
                "scanout-acquire: MAP escape failed hr=0x{:08x} — feature off for this device",
                hr as u32
            );
            0
        }
    };

    // Create + register the per-device retirement event. Refused: no event
    // for DXVK (1 ms ledger polling) and a retry on later presents/flushes,
    // for a device whose ledger is mapped (without it nothing is ever armed).
    // SAFETY: as for the map call.
    let event = unsafe { create_and_register(kt_callbacks, rt_adapter, h_rt_device, "device init") };
    let retry = (event == 0 && ledger_va != 0).then(|| RetrySchedule::refused_at(now_ms()));
    if let Some(r) = retry {
        RETRY_PENDING.fetch_add(1, Ordering::Relaxed);
        note_retry_due(r.due_ms());
    }

    reg.push(DeviceEntry {
        key,
        h_rt_device,
        rt_adapter,
        kt_callbacks: kt_callbacks as usize,
        ledger_va,
        loss_epoch,
        event,
        retry,
    });
    recompute_enabled(&reg);
    log_error!(
        "scanout-acquire: device wired (ledger={} event_registered={} retry={} devices={})",
        (ledger_va != 0) as u32,
        (event != 0) as u32,
        retry.is_some() as u32,
        reg.len()
    );

    event
}

/// Present/flush hook: a device whose REGISTER was refused asks again once its
/// [`RetrySchedule`] is due, and hands an accepted event to its DXVK signaler
/// (which then leaves the 1 ms poll for the event). One relaxed load while no
/// device in the process waits for a retry.
pub(crate) fn retry_register(dev: &HeliosDevice) {
    if RETRY_PENDING.load(Ordering::Relaxed) == 0 {
        return;
    }
    let now = now_ms();
    if now < RETRY_NEXT_DUE_MS.load(Ordering::Relaxed) {
        return;
    }
    let key = dev as *const HeliosDevice as usize;
    // Pick the attempt under the lock, run the escape outside it (the DXVK
    // signaler reads the ledger through this mutex every poll), store the
    // outcome under it again. Only this device's own DDI touches its entry,
    // and DestroyDevice cannot run concurrently with it.
    let (kt_callbacks, rt_adapter, h_rt_device) = {
        let Ok(mut reg) = REGISTRY.lock() else {
            return;
        };
        let picked = match reg.iter_mut().find(|e| e.key == key) {
            Some(e) => match e.retry.as_mut() {
                Some(r) if r.due(now) => {
                    // Not due again until this attempt's outcome reschedules it.
                    r.refused_again(now);
                    Some((e.kt_callbacks, e.rt_adapter, e.h_rt_device))
                }
                _ => None,
            },
            None => None,
        };
        // The earliest due time of the entries that still wait, after the
        // reschedule above, and never sooner than one retry interval: a due
        // device that does not present (an idle one in a multi-device process)
        // must not send every other device's present through this mutex.
        let next = reg.iter().filter_map(|e| e.retry.map(|r| r.due_ms())).min().unwrap_or(u64::MAX);
        RETRY_NEXT_DUE_MS.store(next.max(now + RetrySchedule::FIRST_MS), Ordering::Relaxed);
        let Some(picked) = picked else {
            return;
        };
        picked
    };
    // SAFETY: the callback table and handles belong to this live device (its
    // DDI is running); teardown removes the entry only inside DestroyDevice.
    let event = unsafe {
        create_and_register(
            kt_callbacks as *const ddi::D3DDDI_DEVICECALLBACKS,
            rt_adapter,
            h_rt_device,
            "retry",
        )
    };
    {
        let Ok(mut reg) = REGISTRY.lock() else {
            return;
        };
        let Some(e) = reg.iter_mut().find(|e| e.key == key) else {
            return;
        };
        if event == 0 {
            // Rescheduled above (backed off); publish its due time.
            if let Some(r) = e.retry {
                note_retry_due(r.due_ms());
            }
            return;
        }
        e.event = event;
        e.retry = None;
        RETRY_PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    // Outside the registry lock: the DXVK signaler takes its own mutex and
    // then this registry (`processRetirements`, `armFence`), never the reverse.
    if dev.dxvk.set_scanout_acquire_event(event) {
        log_error!("scanout-acquire: event REGISTER accepted on retry — signaler leaves 1 ms polling");
    }
}

/// Tear one device out of the acquire machinery. Called from
/// `ddi_destroy_device` AFTER the bridge device dropped — i.e. after the DXVK
/// half's §5.3 sequence (stop arming → signal every gate to max → join the
/// signaler) has completed inside `~DxvkDevice` — so the event handle closed
/// here has no waiter left and the VA unmapped here has no reader left (all
/// remaining readers go through the registry mutex and can no longer pick this
/// entry once it is removed).
pub(crate) fn teardown_for_device(key: usize) {
    let entry = {
        let Ok(mut reg) = REGISTRY.lock() else {
            return;
        };
        let Some(index) = reg.iter().position(|e| e.key == key) else {
            return; // knob off / probe off / never wired — nothing to do
        };
        let entry = reg.swap_remove(index);
        recompute_enabled(&reg);
        entry
    };
    // Escapes + CloseHandle outside the lock: no reader can reach the entry
    // any more, and the KMD side is keyed by our h_rt_device owner identity.
    let kt_callbacks = entry.kt_callbacks as *const ddi::D3DDDI_DEVICECALLBACKS;
    if entry.retry.is_some() {
        RETRY_PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    if entry.event != 0 {
        // A nonzero event is a registered one.
        // SAFETY: the callback table stays valid for the whole DestroyDevice
        // DDI, which is where this runs.
        let unregistered = unsafe {
            escape_scanout_event(
                kt_callbacks,
                entry.rt_adapter,
                entry.h_rt_device,
                HELIOS_SCANOUT_ACQ_OP_UNREGISTER,
                entry.event,
            )
        };
        if let Err(hr) = unregistered {
            // The KMD's owner-keyed reclaim in destroy_device is the
            // backstop; the failure is logged, not fatal.
            log_error!(
                "scanout-acquire: event UNREGISTER failed hr=0x{:08x} (KMD owner reclaim is the backstop)",
                hr as u32
            );
        }
        // SAFETY: `entry.event` is the handle this module created and owns;
        // the DXVK signaler that waited on it joined before the bridge drop
        // returned, so no waiter survives.
        unsafe { CloseHandle(entry.event as *mut core::ffi::c_void) };
    }
    if entry.ledger_va != 0 {
        // Out of the shared KMD-view table BEFORE the KMD unmaps the view.
        // SAFETY: plain calls into bridge_kmdmap.cpp, paired with init.
        unsafe {
            helios_kmdmap_c_unregister_owner(entry.key as u64);
            helios_kmdmap_c_detach();
        }
        // SAFETY: as for the unregister escape above.
        let unmapped = unsafe {
            escape_map_ledger(
                kt_callbacks,
                entry.rt_adapter,
                entry.h_rt_device,
                HELIOS_SCANOUT_ACQ_OP_UNMAP,
            )
        };
        if let Err(hr) = unmapped {
            log_error!(
                "scanout-acquire: ledger UNMAP failed hr=0x{:08x} (KMD owner reclaim is the backstop)",
                hr as u32
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The reader surface the DXVK engine resolves by name (helios_scanout_*).
//
// Exported with the C ABI so dxvk_helios_scanout_acquire.cpp can GetProcAddress
// them out of this DLL. bool return/parameters are the one-byte C `_Bool` on
// both sides of the seam. None of these can panic: with panic = "abort" in
// both profiles an unwind here would kill dwm.
// ---------------------------------------------------------------------------

/// Fast-path flag: knob ON, probe OK, at least one live ledger mapping. The
/// single check DXVK's flush path performs before doing anything else.
#[no_mangle]
pub extern "C" fn helios_scanout_acquire_enabled() -> bool {
    ENABLED.load(Ordering::Acquire) != 0
}

/// Version-2 reader protocol: find a `resid`, Acquire-read its generation and
/// counters, then revalidate both identity fields. A same-resid generation
/// change is a re-claim, so retry a full scan rather than treating it as no
/// current read. The explicit `_v2` name makes an old DXVK/UMD pair fail closed.
#[no_mangle]
pub extern "C" fn helios_scanout_ledger_lookup_v2(
    resid: u32,
    out_generation: *mut u64,
    out_issued: *mut u64,
    out_retired: *mut u64,
) -> bool {
    if resid == 0 || out_generation.is_null() || out_issued.is_null() || out_retired.is_null() {
        return false;
    }
    if ENABLED.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Ok(reg) = REGISTRY.lock() else {
        return false;
    };
    let Some(va) = reg.iter().find(|e| ledger_live(e)).map(|e| e.ledger_va) else {
        return false;
    };
    // SAFETY: `va` belongs to a registered entry and the registry mutex is
    // held for the whole read, so the mapping cannot be torn down under us.
    let slot_count = unsafe { ledger_load(va, LEDGER_SLOT_COUNT_OFF) } as usize;
    let slots = slot_count.min(HELIOS_READ_LEDGER_SLOTS);
    for _ in 0..HELIOS_READ_LEDGER_SLOTS {
        let mut changed = false;
        for i in 0..slots {
            let slot = va + LEDGER_SLOTS_OFF + i * SLOT_SIZE;
            // SAFETY: as above; `slot` stays inside the mapped v2 page.
            if unsafe { ledger_load(slot, SLOT_RESID_OFF) } != resid {
                continue;
            }
            // The reader order of protocol/include/helios_read_ledger.h
            // (helios_read_ledger_lookup), whose stress test found a torn read
            // in the older order: resid, generation, RE-CHECK resid, issued,
            // retired, then generation and resid again; restart on a change.
            // SAFETY: as above.
            let generation = unsafe { ledger_load_u64(slot, SLOT_GENERATION_OFF) };
            core::sync::atomic::fence(Ordering::Acquire);
            // SAFETY: as above.
            if unsafe { ledger_load(slot, SLOT_RESID_OFF) } != resid {
                changed = true;
                break;
            }
            // SAFETY: as above.
            let issued = unsafe { ledger_load_u64(slot, SLOT_ISSUED_OFF) };
            // SAFETY: as above.
            let retired = unsafe { ledger_load_u64(slot, SLOT_RETIRED_OFF) };
            core::sync::atomic::fence(Ordering::Acquire);
            // SAFETY: both re-reads close recycle and same-resid re-claim races.
            let final_generation = unsafe { ledger_load_u64(slot, SLOT_GENERATION_OFF) };
            // SAFETY: as above.
            let final_resid = unsafe { ledger_load(slot, SLOT_RESID_OFF) };
            if final_generation != generation || final_resid != resid {
                changed = true;
                break;
            }
            if generation == 0 {
                // A claim being published or freed (resid set, generation not
                // yet): no valid claim in this slot; keep scanning.
                continue;
            }
            // SAFETY: out pointers were null-checked; the caller owns them.
            unsafe {
                *out_generation = generation;
                *out_issued = issued;
                *out_retired = retired;
            }
            return true;
        }
        if !changed {
            return false;
        }
    }
    false
}

/// Snapshot v2 slots for the DXVK signaler's level-triggered pass. A slot that
/// changes resid or generation while sampled is zeroed, never attributed to an
/// old or new claim. Returns the number of slots written (0 = feature off).
#[no_mangle]
pub extern "C" fn helios_scanout_ledger_snapshot_v2(
    out_slots: *mut HeliosReadLedgerSlot,
    max_slots: u32,
) -> u32 {
    if out_slots.is_null() || max_slots == 0 {
        return 0;
    }
    if ENABLED.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let Ok(reg) = REGISTRY.lock() else {
        return 0;
    };
    let Some(va) = reg.iter().find(|e| ledger_live(e)).map(|e| e.ledger_va) else {
        return 0;
    };
    // SAFETY: see helios_scanout_ledger_lookup_v2 — same mutex-held contract.
    let slot_count = unsafe { ledger_load(va, LEDGER_SLOT_COUNT_OFF) } as usize;
    let slots = slot_count
        .min(HELIOS_READ_LEDGER_SLOTS)
        .min(max_slots as usize);
    for i in 0..slots {
        let slot = va + LEDGER_SLOTS_OFF + i * SLOT_SIZE;
        // SAFETY: as above.
        let resid = unsafe { ledger_load(slot, SLOT_RESID_OFF) };
        // SAFETY: as above.
        let generation = unsafe { ledger_load_u64(slot, SLOT_GENERATION_OFF) };
        core::sync::atomic::fence(Ordering::Acquire);
        // Re-check the identity before the counters (helios_read_ledger.h order).
        // SAFETY: as above.
        let resid_again = unsafe { ledger_load(slot, SLOT_RESID_OFF) };
        // SAFETY: as above.
        let issued = unsafe { ledger_load_u64(slot, SLOT_ISSUED_OFF) };
        // SAFETY: as above.
        let retired = unsafe { ledger_load_u64(slot, SLOT_RETIRED_OFF) };
        core::sync::atomic::fence(Ordering::Acquire);
        // SAFETY: both fields identify this exact sampled claim.
        let stable = resid != 0
            && resid_again == resid
            && generation != 0
            && unsafe { ledger_load_u64(slot, SLOT_GENERATION_OFF) } == generation
            // SAFETY: as above.
            && unsafe { ledger_load(slot, SLOT_RESID_OFF) } == resid;
        let snapshot = if stable {
            HeliosReadLedgerSlot {
                resid,
                _pad0: 0,
                generation,
                issued,
                retired,
            }
        } else {
            HeliosReadLedgerSlot {
                resid: 0,
                _pad0: 0,
                generation: 0,
                issued: 0,
                retired: 0,
            }
        };
        // SAFETY: the caller promises `max_slots` slot capacity; `i` is
        // bounded by `slots <= max_slots`.
        unsafe {
            *out_slots.add(i) = snapshot;
        }
    }
    slots as u32
}

// ---- Flush gate capabilities (docs/flush-gate.md, decision 7) ------------

/// Venus stream points and the wire rung: `HELIOS_SCANOUT_CAP_FLUSH_GATE` in
/// the PROBE reply. An older KMD copies an `HEFL` and gates nothing, so the
/// UMD never sends one without the bit.
pub(crate) fn flush_gate_capable() -> bool {
    PROBE_STATE.load(Ordering::Acquire) == PROBE_OK
        && PROBE_CAPS.load(Ordering::Relaxed) & helios_protocol::HELIOS_SCANOUT_CAP_FLUSH_GATE != 0
}

/// NVRM `QUERY_CAPS.supported_ops`, once per process: 0 unknown, else the
/// bits with bit 63 forced on as "asked" (bit 63 is no capability).
static NVRM_OPS: AtomicU64 = AtomicU64::new(0);
const NVRM_OPS_ASKED: u64 = 1 << 63;

/// The RM-fence flush gate: NVRM `QUERY_CAPS.supported_ops` bit 34
/// (`HELIOS_NVRM_CAP_FLUSH_GATE`), asked once per process through this
/// device's escape callback.
///
/// # Safety
/// `dev` is a live device (its callback table valid for the call).
pub(crate) unsafe fn nvrm_flush_gate_capable(dev: &HeliosDevice) -> bool {
    // SAFETY: the caller's contract.
    (unsafe { nvrm_supported_ops(dev) } & helios_protocol::HELIOS_NVRM_CAP_FLUSH_GATE) != 0
}

/// The copy-engine Present record (`'HEF3'`): NVRM `QUERY_CAPS.supported_ops`
/// bit 37 (`HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3`), the KMD reads it.
///
/// # Safety
/// `dev` is a live device (its callback table valid for the call).
pub(crate) unsafe fn nvrm_rm_fence_tail_v3_capable(dev: &HeliosDevice) -> bool {
    // SAFETY: the caller's contract.
    (unsafe { nvrm_supported_ops(dev) } & helios_protocol::HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3) != 0
}

/// NVRM `QUERY_CAPS.supported_ops` (bit 63 set once asked), asked once per
/// process through this device's escape callback.
///
/// # Safety
/// `dev` is a live device (its callback table valid for the call).
unsafe fn nvrm_supported_ops(dev: &HeliosDevice) -> u64 {
    let mut ops = NVRM_OPS.load(Ordering::Relaxed);
    if ops == 0 {
        ops = NVRM_OPS_ASKED;
        let rt_adapter = dev.rt_adapter;
        if !dev.kt_callbacks.is_null() && rt_adapter != 0 {
            // SAFETY: HeliosNvrmQueryCaps is plain data; all-zero is valid.
            let mut q: helios_protocol::HeliosNvrmQueryCaps = unsafe { core::mem::zeroed() };
            q.head.hdr = HeliosEscapeHeader::new(
                helios_protocol::HELIOS_ESCAPE_NVRM,
                size_of::<helios_protocol::HeliosNvrmQueryCaps>() as u32,
            );
            q.head.abi_version = helios_protocol::HELIOS_NVRM_ABI_VERSION;
            q.head.op = helios_protocol::HELIOS_NVRM_OP_QUERY_CAPS;
            // SAFETY: `q` is a live stack struct of exactly the advertised size.
            let hr = unsafe {
                call_escape(
                    dev.kt_callbacks,
                    rt_adapter,
                    dev.h_rt_device as usize,
                    0,
                    (&mut q as *mut helios_protocol::HeliosNvrmQueryCaps).cast(),
                    size_of::<helios_protocol::HeliosNvrmQueryCaps>() as u32,
                )
            };
            if hr >= 0 && q.head.status == 0 {
                ops |= q.supported_ops;
            }
            log_error!(
                "nvrm: QUERY_CAPS hr=0x{:08x} status={} supported_ops=0x{:016x}",
                hr as u32,
                q.head.status,
                q.supported_ops
            );
        }
        NVRM_OPS.store(ops, Ordering::Relaxed);
    }
    ops
}

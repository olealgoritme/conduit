//! "Adapter-wide device removed" instrument: the pure half. Which status a DDI may legally
//! answer, the verdict a returned status gets, the fixed-size failure rings, the sticky
//! first-fatal record and the longest-event records. All of it atomics and arithmetic, no clock,
//! no registry, no WDK type. The I/O half is `kmd_render/src/ddi/device_lost.rs` (statics, the
//! DDI wrappers in `ddi/traced.rs`, the registry mirror). Design, the counters to read and the
//! checklist: `docs/zero-copy-present.md`, "Adapter-wide device removed".
//!
//! WHY. A tester saw every live D3D device on the adapter get `D3DDDIERR_DEVICEREMOVED` between
//! two instants 17 s apart, with no TDR event, no dump, no device restart and nothing on the
//! host. dxgkrnl marks an adapter lost for reasons that leave no trace in the System log: a DDI
//! answering a status it does not accept (`BuildPagingBuffer` answers only `STATUS_SUCCESS` and
//! `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER`; the scheduler DDIs only `STATUS_SUCCESS`), a
//! `STATUS_GRAPHICS_*` or device-removed-class status from a DDI that has no business with it,
//! or a DDI that does not return for tens of seconds. Before this module the KMD kept nothing
//! about the statuses it returned. Now every wrapped DDI leaves: the last 16 non-success
//! returns (DDI id, status, interrupt time, a hint), the last 8 that were NOT in the DDI's
//! expected set, the first FATAL one for the whole image lifetime (first wins, never
//! overwritten), the DDIs in flight and the longest-running calls.
//!
//! Nothing here changes what the driver answers.

use core::sync::atomic::{AtomicU32, Ordering};

/// `STATUS_SUCCESS`.
pub const STATUS_SUCCESS: i32 = 0;
/// `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (0xC01E0001): the one non-success answer
/// `BuildPagingBuffer` may give (`paging::is_legal_status`).
pub const STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER: i32 = 0xC01E_0001u32 as i32;
pub const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
pub const STATUS_NOT_IMPLEMENTED: i32 = 0xC000_0002u32 as i32;
pub const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
pub const STATUS_NO_SUCH_DEVICE: i32 = 0xC000_000Eu32 as i32;
pub const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC000_0010u32 as i32;
pub const STATUS_NO_MEMORY: i32 = 0xC000_0017u32 as i32;
pub const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
pub const STATUS_INSUFFICIENT_RESOURCES: i32 = 0xC000_009Au32 as i32;
pub const STATUS_DEVICE_NOT_CONNECTED: i32 = 0xC000_009Du32 as i32;
pub const STATUS_DEVICE_NOT_READY: i32 = 0xC000_00A3u32 as i32;
pub const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
pub const STATUS_DEVICE_DOES_NOT_EXIST: i32 = 0xC000_00C0u32 as i32;
pub const STATUS_CANCELLED: i32 = 0xC000_0120u32 as i32;
pub const STATUS_IO_DEVICE_ERROR: i32 = 0xC000_0185u32 as i32;
pub const STATUS_DEVICE_REMOVED: i32 = 0xC000_02B6u32 as i32;
pub const STATUS_DEVICE_BUSY: i32 = 0x8000_0011u32 as i32;

/// DDI ids: the key of every table and record here. Dense, below [`ddi::MAX`], append only (the
/// ids are owner-readable ABI in the service key).
pub mod ddi {
    pub const NONE: u32 = 0;
    pub const START_DEVICE: u32 = 1;
    pub const STOP_DEVICE: u32 = 2;
    pub const REMOVE_DEVICE: u32 = 3;
    pub const SET_POWER_STATE: u32 = 4;
    pub const CREATE_DEVICE: u32 = 5;
    pub const DESTROY_DEVICE: u32 = 6;
    pub const CREATE_CONTEXT: u32 = 7;
    pub const DESTROY_CONTEXT: u32 = 8;
    pub const CREATE_PROCESS: u32 = 9;
    pub const DESTROY_PROCESS: u32 = 10;
    pub const CREATE_ALLOCATION: u32 = 11;
    pub const DESTROY_ALLOCATION: u32 = 12;
    pub const BUILD_PAGING_BUFFER: u32 = 13;
    pub const SUBMIT_COMMAND: u32 = 14;
    pub const SUBMIT_COMMAND_VIRTUAL: u32 = 15;
    pub const PREEMPT_COMMAND: u32 = 16;
    pub const RESET_FROM_TIMEOUT: u32 = 17;
    pub const RESTART_FROM_TIMEOUT: u32 = 18;
    pub const RESET_ENGINE: u32 = 19;
    pub const QUERY_ENGINE_STATUS: u32 = 20;
    pub const RENDER: u32 = 21;
    pub const RENDER_KM: u32 = 22;
    pub const RENDER_GDI: u32 = 23;
    pub const PRESENT: u32 = 24;
    pub const OPEN_ALLOCATION: u32 = 25;
    pub const CLOSE_ALLOCATION: u32 = 26;
    pub const MAP_CPU_HOST_APERTURE: u32 = 27;
    pub const SET_VIDPN_SOURCE_ADDRESS: u32 = 28;
    pub const COMMIT_VIDPN: u32 = 29;
    pub const ESCAPE: u32 = 30;
    pub const QUERY_ADAPTER_INFO: u32 = 31;
    pub const UNMAP_CPU_HOST_APERTURE: u32 = 32;
    pub const PATCH: u32 = 33;
    pub const CONTROL_INTERRUPT: u32 = 34;
    pub const IS_SUPPORTED_VIDPN: u32 = 35;
    pub const UPDATE_ACTIVE_VIDPN_PATH: u32 = 36;
    pub const SET_VIDPN_SOURCE_VISIBILITY: u32 = 37;
    pub const ENUM_VIDPN_COFUNC: u32 = 38;
    pub const RECOMMEND_FUNCTIONAL_VIDPN: u32 = 39;
    pub const QUERY_CHILD_STATUS: u32 = 40;
    pub const QUERY_CHILD_RELATIONS: u32 = 41;
    /// Not a DDI: `DxgkCbIndicateChildStatus`'s answer to the HPD worker (hot-plug).
    pub const CB_INDICATE_CHILD: u32 = 42;
    /// Not a DDI: `DxgkCbSynchronizeExecution` / `DxgkCbNotifyInterrupt` refused a DMA completion.
    pub const CB_NOTIFY_DMA: u32 = 43;
    /// One past the largest id; tables are this long.
    pub const MAX: usize = 48;

    pub const ALL: [(u32, &str); 44] = [
        (NONE, "none"),
        (START_DEVICE, "StartDevice"),
        (STOP_DEVICE, "StopDevice"),
        (REMOVE_DEVICE, "RemoveDevice"),
        (SET_POWER_STATE, "SetPowerState"),
        (CREATE_DEVICE, "CreateDevice"),
        (DESTROY_DEVICE, "DestroyDevice"),
        (CREATE_CONTEXT, "CreateContext"),
        (DESTROY_CONTEXT, "DestroyContext"),
        (CREATE_PROCESS, "CreateProcess"),
        (DESTROY_PROCESS, "DestroyProcess"),
        (CREATE_ALLOCATION, "CreateAllocation"),
        (DESTROY_ALLOCATION, "DestroyAllocation"),
        (BUILD_PAGING_BUFFER, "BuildPagingBuffer"),
        (SUBMIT_COMMAND, "SubmitCommand"),
        (SUBMIT_COMMAND_VIRTUAL, "SubmitCommandVirtual"),
        (PREEMPT_COMMAND, "PreemptCommand"),
        (RESET_FROM_TIMEOUT, "ResetFromTimeout"),
        (RESTART_FROM_TIMEOUT, "RestartFromTimeout"),
        (RESET_ENGINE, "ResetEngine"),
        (QUERY_ENGINE_STATUS, "QueryEngineStatus"),
        (RENDER, "Render"),
        (RENDER_KM, "RenderKm"),
        (RENDER_GDI, "RenderGdi"),
        (PRESENT, "Present"),
        (OPEN_ALLOCATION, "OpenAllocation"),
        (CLOSE_ALLOCATION, "CloseAllocation"),
        (MAP_CPU_HOST_APERTURE, "MapCpuHostAperture"),
        (SET_VIDPN_SOURCE_ADDRESS, "SetVidPnSourceAddress"),
        (COMMIT_VIDPN, "CommitVidPn"),
        (ESCAPE, "Escape"),
        (QUERY_ADAPTER_INFO, "QueryAdapterInfo"),
        (UNMAP_CPU_HOST_APERTURE, "UnmapCpuHostAperture"),
        (PATCH, "Patch"),
        (CONTROL_INTERRUPT, "ControlInterrupt"),
        (IS_SUPPORTED_VIDPN, "IsSupportedVidPn"),
        (UPDATE_ACTIVE_VIDPN_PATH, "UpdateActiveVidPnPresentPath"),
        (SET_VIDPN_SOURCE_VISIBILITY, "SetVidPnSourceVisibility"),
        (ENUM_VIDPN_COFUNC, "EnumVidPnCofuncModality"),
        (RECOMMEND_FUNCTIONAL_VIDPN, "RecommendFunctionalVidPn"),
        (QUERY_CHILD_STATUS, "QueryChildStatus"),
        (QUERY_CHILD_RELATIONS, "QueryChildRelations"),
        (CB_INDICATE_CHILD, "cb:IndicateChildStatus"),
        (CB_NOTIFY_DMA, "cb:NotifyDmaCompleted"),
    ];
}

/// How a DDI's answer is judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// `BuildPagingBuffer`: VidMm accepts `STATUS_SUCCESS` and the DMA-buffer retry, anything
    /// else is "invalid error code" and a lost adapter (or bugcheck 0x10E).
    Paging,
    /// The scheduler DDIs: `STATUS_SUCCESS` only (`SubmitCommand*` bugchecks 0x119/2 otherwise).
    Scheduler,
    /// The VidPN / child DDIs: `STATUS_GRAPHICS_*` and the monitor statuses are their language.
    VidPn,
    /// Allocation DDIs and the aperture map: resource statuses are expected.
    Alloc,
    /// Render / Present / Patch.
    Render,
    /// `DxgkDdiEscape`: user mode talks to it, every ordinary error is expected.
    Escape,
    /// PnP, power, device / context / process objects: any failure is a surprise.
    Lifecycle,
    /// `QueryAdapterInfo`, `ControlInterrupt`: refusing an unknown type is expected.
    Query,
    /// A callback of dxgkrnl's whose failure the KMD saw.
    Callback,
}

/// The family of `ddi`.
pub const fn family(ddi: u32) -> Family {
    match ddi {
        ddi::BUILD_PAGING_BUFFER => Family::Paging,
        ddi::SUBMIT_COMMAND
        | ddi::SUBMIT_COMMAND_VIRTUAL
        | ddi::PREEMPT_COMMAND
        | ddi::RESET_FROM_TIMEOUT
        | ddi::RESTART_FROM_TIMEOUT
        | ddi::RESET_ENGINE
        | ddi::QUERY_ENGINE_STATUS => Family::Scheduler,
        ddi::SET_VIDPN_SOURCE_ADDRESS
        | ddi::COMMIT_VIDPN
        | ddi::IS_SUPPORTED_VIDPN
        | ddi::UPDATE_ACTIVE_VIDPN_PATH
        | ddi::SET_VIDPN_SOURCE_VISIBILITY
        | ddi::ENUM_VIDPN_COFUNC
        | ddi::RECOMMEND_FUNCTIONAL_VIDPN
        | ddi::QUERY_CHILD_STATUS
        | ddi::QUERY_CHILD_RELATIONS => Family::VidPn,
        ddi::CREATE_ALLOCATION
        | ddi::DESTROY_ALLOCATION
        | ddi::OPEN_ALLOCATION
        | ddi::CLOSE_ALLOCATION
        | ddi::MAP_CPU_HOST_APERTURE
        | ddi::UNMAP_CPU_HOST_APERTURE => Family::Alloc,
        ddi::RENDER | ddi::RENDER_KM | ddi::RENDER_GDI | ddi::PRESENT | ddi::PATCH => {
            Family::Render
        }
        ddi::ESCAPE => Family::Escape,
        ddi::QUERY_ADAPTER_INFO | ddi::CONTROL_INTERRUPT => Family::Query,
        ddi::CB_INDICATE_CHILD | ddi::CB_NOTIFY_DMA => Family::Callback,
        _ => Family::Lifecycle,
    }
}

/// `STATUS_GRAPHICS_*`: facility 0x1E, severity error or warning (0xC01Exxxx, 0x801Exxxx).
pub const fn is_graphics_class(status: i32) -> bool {
    let hi = (status as u32) >> 16;
    hi == 0xC01E || hi == 0x801E
}

/// The monitor-descriptor statuses (0xC01Dxxxx) the VidPN / child DDIs answer in the course of
/// ordinary enumeration.
pub const fn is_monitor_class(status: i32) -> bool {
    ((status as u32) >> 16) == 0xC01D
}

/// Statuses that say "the device is gone" to anything that sees them.
pub const fn is_removed_class(status: i32) -> bool {
    status == STATUS_DEVICE_REMOVED
        || status == STATUS_DEVICE_NOT_CONNECTED
        || status == STATUS_NO_SUCH_DEVICE
        || status == STATUS_DEVICE_DOES_NOT_EXIST
        || status == STATUS_IO_DEVICE_ERROR
}

/// What a returned status means for the "device lost" question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// `STATUS_SUCCESS` or an informational / success status (non-negative).
    Ok = 0,
    /// A failure in the DDI's expected set: ordinary traffic (an unknown escape, a refused
    /// allocation, a graphics status from a VidPN enumeration).
    Expected = 1,
    /// A failure outside the expected set that is not loss-class (`STATUS_INVALID_PARAMETER`
    /// from `CreateContext`, `STATUS_NOT_SUPPORTED` from `StartDevice`).
    Suspect = 2,
    /// A status dxgkrnl may turn into a lost adapter or a bugcheck: any non-success the
    /// scheduler / paging DDIs return outside their legal set, a removed-class status from any
    /// DDI, a `STATUS_GRAPHICS_*` or `STATUS_DEVICE_NOT_READY` outside the DDI's expected set.
    Fatal = 3,
}

/// Whether `status` is in `ddi`'s expected set of failures.
pub const fn is_expected(ddi: u32, status: i32) -> bool {
    match family(ddi) {
        // Legal for the DDI, not "a failure to look at": the paging rule is the same function
        // as `paging::is_legal_status`.
        Family::Paging => status == STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
        Family::Scheduler => false,
        Family::VidPn => {
            is_graphics_class(status)
                || is_monitor_class(status)
                || status == STATUS_NOT_SUPPORTED
                || status == STATUS_INVALID_PARAMETER
                || status == STATUS_BUFFER_TOO_SMALL
                || status == STATUS_NO_MEMORY
                || status == STATUS_INSUFFICIENT_RESOURCES
        }
        Family::Alloc => {
            status == STATUS_NO_MEMORY
                || status == STATUS_INSUFFICIENT_RESOURCES
                || status == STATUS_INVALID_PARAMETER
                || status == STATUS_NOT_SUPPORTED
                || status == STATUS_BUFFER_TOO_SMALL
                || status == STATUS_ACCESS_DENIED
                || status == STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER
        }
        Family::Render => {
            status == STATUS_INVALID_PARAMETER
                || status == STATUS_NOT_SUPPORTED
                || status == STATUS_BUFFER_TOO_SMALL
                || status == STATUS_NO_MEMORY
                || status == STATUS_INSUFFICIENT_RESOURCES
                || status == STATUS_ACCESS_DENIED
                || status == STATUS_DEVICE_BUSY
                || status == STATUS_CANCELLED
                || status == STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER
        }
        // Every ordinary error but a loss-class one, which `verdict` catches first.
        Family::Escape => true,
        Family::Lifecycle => false,
        Family::Query => {
            status == STATUS_NOT_SUPPORTED
                || status == STATUS_NOT_IMPLEMENTED
                || status == STATUS_BUFFER_TOO_SMALL
                || status == STATUS_INVALID_PARAMETER
        }
        Family::Callback => false,
    }
}

/// The verdict of `status` returned by `ddi`.
pub const fn verdict(ddi: u32, status: i32) -> Verdict {
    if status >= 0 {
        return Verdict::Ok;
    }
    match family(ddi) {
        Family::Paging | Family::Scheduler => {
            if is_expected(ddi, status) {
                Verdict::Expected
            } else {
                // VidMm / VidSch accept nothing else from these DDIs.
                Verdict::Fatal
            }
        }
        fam => {
            if is_expected(ddi, status) && !is_removed_class(status) {
                return Verdict::Expected;
            }
            let loss = is_removed_class(status)
                || is_graphics_class(status)
                || status == STATUS_DEVICE_NOT_READY;
            // An Escape may answer NOT_READY (user mode is told, nothing else sees it).
            if matches!(fam, Family::Escape) && status == STATUS_DEVICE_NOT_READY {
                return Verdict::Expected;
            }
            if loss {
                Verdict::Fatal
            } else {
                Verdict::Suspect
            }
        }
    }
}

/// `ddi` in the top byte, a 24-bit hint below it.
pub const fn pack_ddi_hint(ddi: u32, hint: u32) -> u32 {
    (ddi << 24) | (hint & 0x00FF_FFFF)
}

/// The inverse of [`pack_ddi_hint`].
pub const fn unpack_ddi_hint(word: u32) -> (u32, u32) {
    (word >> 24, word & 0x00FF_FFFF)
}

// ---- the rings -----------------------------------------------------------------------------

/// One ring entry: three words (a value, the packed DDI and hint, a time). Written field by
/// field then published by the ring's sequence, so a reader that sees the sequence move sees
/// whole entries except for the one being written, which a diagnostic tolerates.
pub struct Slot {
    pub a: AtomicU32,
    pub b: AtomicU32,
    pub t: AtomicU32,
}

impl Slot {
    const NEW: Slot = Slot {
        a: AtomicU32::new(0),
        b: AtomicU32::new(0),
        t: AtomicU32::new(0),
    };
}

/// A ring of the last `N` events. `a` is the status (failure rings) or the duration in ms (the
/// slow ring); `b` is [`pack_ddi_hint`]; `t` the interrupt time in ms. Lock free: any IRQL.
pub struct EventRing<const N: usize> {
    seq: AtomicU32,
    slots: [Slot; N],
}

impl<const N: usize> EventRing<N> {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            slots: [Slot::NEW; N],
        }
    }

    /// Append an event. Returns its number (1 for the first).
    pub fn push(&self, a: u32, b: u32, t: u32) -> u32 {
        let n = self.seq.fetch_add(1, Ordering::AcqRel);
        let s = &self.slots[(n as usize) % N];
        s.a.store(a, Ordering::Relaxed);
        s.b.store(b, Ordering::Relaxed);
        s.t.store(t, Ordering::Release);
        n.wrapping_add(1)
    }

    /// Events pushed so far (wraps at 2^32).
    pub fn count(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }

    /// The `i`-th slot (`0..N`), in storage order. The newest is slot `(count - 1) % N`.
    pub fn slot(&self, i: usize) -> (u32, u32, u32) {
        let s = &self.slots[i % N];
        (
            s.a.load(Ordering::Relaxed),
            s.b.load(Ordering::Relaxed),
            s.t.load(Ordering::Acquire),
        )
    }

    /// The `back`-th newest event (0 = newest), or `None` when fewer were pushed.
    pub fn nth_newest(&self, back: usize) -> Option<(u32, u32, u32)> {
        let n = self.count() as usize;
        if back >= N || back >= n {
            return None;
        }
        Some(self.slot((n - 1 - back) % N))
    }
}

impl<const N: usize> Default for EventRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The sticky first-fatal record: the FIRST event a caller reports, kept for the image's
/// lifetime. Later events only bump [`Self::count`]. First wins by one compare-exchange, so two
/// CPUs failing at once leave exactly one complete record (the loser's fields are never mixed
/// in: only the winner writes them, then publishes `ready`).
pub struct FirstFatal {
    claimed: AtomicU32,
    ready: AtomicU32,
    count: AtomicU32,
    pub ddi: AtomicU32,
    pub status: AtomicU32,
    pub t: AtomicU32,
    pub thread: AtomicU32,
    pub hint: AtomicU32,
    pub irql: AtomicU32,
    /// The failure sequence number (the failure ring's count) when it happened.
    pub seq: AtomicU32,
    /// The DDIs in flight at that moment, as two 32-bit halves of a bitmask by DDI id.
    pub inflight_lo: AtomicU32,
    pub inflight_hi: AtomicU32,
}

impl FirstFatal {
    pub const fn new() -> Self {
        Self {
            claimed: AtomicU32::new(0),
            ready: AtomicU32::new(0),
            count: AtomicU32::new(0),
            ddi: AtomicU32::new(0),
            status: AtomicU32::new(0),
            t: AtomicU32::new(0),
            thread: AtomicU32::new(0),
            hint: AtomicU32::new(0),
            irql: AtomicU32::new(0),
            seq: AtomicU32::new(0),
            inflight_lo: AtomicU32::new(0),
            inflight_hi: AtomicU32::new(0),
        }
    }

    /// Report a fatal event. `fill` writes the record's fields and runs only for the first.
    /// Returns whether this call was the first.
    pub fn note(&self, fill: impl FnOnce(&Self)) -> bool {
        self.count.fetch_add(1, Ordering::Relaxed);
        if self
            .claimed
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        fill(self);
        self.ready.store(1, Ordering::Release);
        true
    }

    /// Fatal events reported, the first included.
    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }

    /// The record is complete (a reader that sees `false` with `count() > 0` caught the winner
    /// mid-write).
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) != 0
    }
}

impl Default for FirstFatal {
    fn default() -> Self {
        Self::new()
    }
}

/// The longest value seen, with a tag and a time: `note` replaces the record only when the value
/// is strictly larger.
pub struct Longest {
    pub value: AtomicU32,
    pub tag: AtomicU32,
    pub t: AtomicU32,
}

impl Longest {
    pub const fn new() -> Self {
        Self {
            value: AtomicU32::new(0),
            tag: AtomicU32::new(0),
            t: AtomicU32::new(0),
        }
    }

    /// Offer `value` (tagged `tag`, at time `t`). Returns whether it is the new record.
    pub fn note(&self, value: u32, tag: u32, t: u32) -> bool {
        let mut cur = self.value.load(Ordering::Relaxed);
        while value > cur {
            match self
                .value
                .compare_exchange(cur, value, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.tag.store(tag, Ordering::Relaxed);
                    self.t.store(t, Ordering::Release);
                    return true;
                }
                Err(now) => cur = now,
            }
        }
        false
    }
}

impl Default for Longest {
    fn default() -> Self {
        Self::new()
    }
}

/// Which DDIs are inside a wrapped call: a count per id, so concurrent calls of one DDI nest.
pub struct InFlight {
    n: [AtomicU32; ddi::MAX],
    entered_ms: [AtomicU32; ddi::MAX],
}

impl InFlight {
    const ZERO: AtomicU32 = AtomicU32::new(0);

    pub const fn new() -> Self {
        Self {
            n: [Self::ZERO; ddi::MAX],
            entered_ms: [Self::ZERO; ddi::MAX],
        }
    }

    /// A call of `id` begins at `now_ms` (never 0 in the stamp: 0 means "never").
    pub fn enter(&self, id: u32, now_ms: u32) {
        let i = id as usize;
        if i >= ddi::MAX {
            return;
        }
        self.entered_ms[i].store(now_ms.max(1), Ordering::Relaxed);
        self.n[i].fetch_add(1, Ordering::AcqRel);
    }

    /// A call of `id` ends.
    pub fn leave(&self, id: u32) {
        let i = id as usize;
        if i >= ddi::MAX {
            return;
        }
        // Never below zero: a leave without an enter (a generation reset) is ignored.
        let _ = self.n[i].fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v| v.checked_sub(1));
    }

    /// Calls of `id` in flight now.
    pub fn count(&self, id: u32) -> u32 {
        self.n.get(id as usize).map_or(0, |a| a.load(Ordering::Acquire))
    }

    /// The in-flight set as a bitmask by id, as (ids 0..32, ids 32..64).
    pub fn mask(&self) -> (u32, u32) {
        let (mut lo, mut hi) = (0u32, 0u32);
        let mut i = 0;
        while i < ddi::MAX {
            if self.n[i].load(Ordering::Acquire) != 0 {
                if i < 32 {
                    lo |= 1 << i;
                } else {
                    hi |= 1 << (i - 32);
                }
            }
            i += 1;
        }
        (lo, hi)
    }

    /// The in-flight DDI that entered longest ago, as (id, age in ms at `now_ms`), or `None`.
    /// Entry stamps are the LAST entry of each id, so with concurrent calls of one id the age is
    /// the newest entry's: a lower bound, which is the safe direction for "who is stuck".
    pub fn oldest(&self, now_ms: u32) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32)> = None;
        let mut i = 0;
        while i < ddi::MAX {
            if self.n[i].load(Ordering::Acquire) != 0 {
                let at = self.entered_ms[i].load(Ordering::Relaxed);
                let age = if at == 0 { 0 } else { now_ms.wrapping_sub(at) };
                let age = if age >= 0x8000_0000 { 0 } else { age };
                if best.is_none_or(|(_, a)| age > a) {
                    best = Some((i as u32, age));
                }
            }
            i += 1;
        }
        best
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new()
    }
}

// ---- paging operations ----------------------------------------------------------------------

/// What one `BuildPagingBuffer` call did, as recorded in `PgLastRes` and the Evict counters.
pub mod paging_result {
    /// The content operation ran.
    pub const EXECUTED: u32 = 1;
    /// Not this driver's operation (another segment, a device-local allocation, an op the null
    /// engine consumes): success, nothing was due.
    pub const NOT_OURS: u32 = 2;
    /// The operation was ours and did NOT happen; answered `STATUS_SUCCESS` (`PgSkipV`).
    pub const SKIPPED: u32 = 3;
    /// The content mutex could not be taken: the whole operation was skipped.
    pub const NO_GUARD: u32 = 4;
    /// The call arrived above PASSIVE_LEVEL (`PgEi`): the operation was skipped.
    pub const BAD_IRQL: u32 = 5;
    /// No BAR segment is active: the pure null engine answered.
    pub const NO_BAR: u32 = 6;
    /// A page-table update.
    pub const PTE: u32 = 7;
    /// The arguments were null: `STATUS_INVALID_PARAMETER` was returned.
    pub const BAD_ARGS: u32 = 8;
}

/// What an evict-or-page-in classification of a paging operation says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagingKind {
    /// Not a content move (page-table update, discard, fill, move inside the segment).
    Other,
    /// Content leaving the segment for system memory: the eviction of an allocation.
    Evict,
    /// Content entering the segment from system memory.
    PageIn,
}

/// The kind of a classic TRANSFER from its two ends: `src_seg` / `dst_seg` are the segment ids
/// (0 = system memory, `bar` = the BAR segment).
pub const fn transfer_kind(src_seg: u32, dst_seg: u32, bar: u32) -> PagingKind {
    if dst_seg == 0 && src_seg == bar {
        PagingKind::Evict
    } else if src_seg == 0 && dst_seg == bar {
        PagingKind::PageIn
    } else {
        PagingKind::Other
    }
}

/// The names written to the service key by `kmd_render/src/ddi/device_lost.rs`, nothing else
/// writes them; at most 14 characters each; none equal to any other counter in `kmd_render` or
/// `kmd_logic` (host-tested by scanning both trees). The ring entries are written under dynamic
/// names, `<stem><kind><two hex digits>`: stems [`RING_STEMS`], kinds `S` (status, or ms for
/// the slow ring), `D` (DDI id in the top byte, hint below), `T` (interrupt time ms).
///
/// Order to READ them after an event: `LostN` (any fatal?), `Lost*` (the first), `DdiFailN`,
/// `DdiSuspN`, the `Dx*` ring, `DdiOldId`/`DdiOldMs`, `DdiInflL`/`DdiInflH`, `Dz*` and
/// `DdiLong*`, `PgLast*`, `PgEv*`, `PgLong*`, `VnLk*`, `ScLkHold*`, the call counts.
pub const COUNTERS: [&str; 60] = [
    // The sticky first-fatal record.
    "LostN",
    "LostDdi",
    "LostSt",
    "LostT",
    "LostThr",
    "LostHint",
    "LostIrql",
    "LostSeq",
    "LostInfL",
    "LostInfH",
    // The failure rings' totals and the slow-call record.
    "DdiFailN",
    "DdiSuspN",
    "DdiSlowN",
    "DdiLongMs",
    "DdiLongId",
    "DdiLongT",
    "DdiOldId",
    "DdiOldMs",
    "DdiInflL",
    "DdiInflH",
    "DdiPubT",
    // Calls of the DDIs a TDR or a teardown drives.
    "NPreempt",
    "NResetTmo",
    "NRestartTmo",
    "NResetEng",
    "NCreateDev",
    "NDestroyDev",
    "NCreateCtx",
    "NDestroyCtx",
    "NCreateProc",
    "NDestroyProc",
    "NStopDev",
    "NSetPower",
    "TResetTmo",
    "TPreempt",
    // The last paging operation and the evictions by result.
    "PgLastOp",
    "PgLastRes",
    "PgLastAl",
    "PgLastSz",
    "PgLastT",
    "PgLastUs",
    "PgEvTot",
    "PgEvOk",
    "PgEvSkip",
    "PgEvNo",
    "PgEvBad",
    "PgPiOk",
    "PgPiSkip",
    "PgLongUs",
    "PgLongOp",
    "PgLongT",
    "PgMtxMaxUs",
    "PgMtxFail",
    // The Venus mutex and the scanout mutex.
    "VnLkN",
    "VnLkWaitMs",
    "VnLkHoldMs",
    "VnLkHoldT",
    "VnLkThr",
    "VnLkHeldMs",
    "ScLkHoldMs",
];

/// The stems of the dynamically named ring entries: `Dd` = the last 16 non-success returns,
/// `Dx` = the last 8 that were outside the DDI's expected set, `Dz` = the last 8 calls that
/// took 250 ms or more.
pub const RING_STEMS: [&str; 3] = ["Dd", "Dx", "Dz"];
/// Entries in the `Dd`, `Dx` and `Dz` rings.
pub const FAIL_RING_LEN: usize = 16;
pub const SUSPECT_RING_LEN: usize = 8;
pub const SLOW_RING_LEN: usize = 8;
/// A DDI call at least this long (ms) is a slow call.
pub const SLOW_CALL_MS: u32 = 250;
/// A `DxgkDdiEscape` at least this long is slow: escapes wait on host fences for a living (a
/// quarter second is routine), so the ordinary threshold would fill the slow ring with them and
/// push out the teardown that matters.
pub const SLOW_ESCAPE_MS: u32 = 5_000;

/// The duration (ms) from which a call of `ddi` is recorded as slow.
pub const fn slow_call_ms(ddi: u32) -> u32 {
    if ddi == self::ddi::ESCAPE {
        SLOW_ESCAPE_MS
    } else {
        SLOW_CALL_MS
    }
}

/// Whether `name` is one of the dynamic ring names (`Dd`/`Dx`/`Dz`, `S`/`D`/`T`, two hex
/// digits).
pub fn is_ring_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 5
        && RING_STEMS.iter().any(|s| s.as_bytes() == &b[..2])
        && matches!(b[2], b'S' | b'D' | b'T')
        && b[3].is_ascii_hexdigit()
        && b[4].is_ascii_hexdigit()
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::paging;
    use std::format;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn ddi_ids_are_dense_unique_named_and_in_range() {
        let mut seen = [false; ddi::MAX];
        for (i, (id, name)) in ddi::ALL.iter().enumerate() {
            assert_eq!(*id as usize, i, "{name}: ids are dense and in order");
            assert!((*id as usize) < ddi::MAX);
            assert!(!seen[*id as usize]);
            seen[*id as usize] = true;
            assert!(!name.is_empty());
        }
        // The id fits the top byte of the packed word.
        assert!(ddi::MAX <= 256);
    }

    #[test]
    fn success_is_ok_for_every_ddi() {
        for (id, _) in ddi::ALL {
            assert_eq!(verdict(id, STATUS_SUCCESS), Verdict::Ok);
            // Informational and warning-free positive statuses too.
            assert_eq!(verdict(id, 0x0000_0102), Verdict::Ok);
        }
    }

    #[test]
    fn paging_accepts_exactly_what_vidmm_accepts() {
        // The same rule as `paging::is_legal_status`, for every status that matters.
        for st in [
            STATUS_SUCCESS,
            STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
            STATUS_INVALID_PARAMETER,
            STATUS_INSUFFICIENT_RESOURCES,
            STATUS_UNSUCCESSFUL,
            STATUS_DEVICE_NOT_READY,
            STATUS_NO_MEMORY,
            0xC01E_0300u32 as i32,
        ] {
            let v = verdict(ddi::BUILD_PAGING_BUFFER, st);
            assert_eq!(
                v <= Verdict::Expected,
                paging::is_legal_status(st),
                "{st:#x}: {v:?}"
            );
        }
        assert_eq!(
            verdict(ddi::BUILD_PAGING_BUFFER, STATUS_INVALID_PARAMETER),
            Verdict::Fatal,
            "STATUS_INVALID_PARAMETER from paging is the case the first-fatal record exists for"
        );
        assert_eq!(
            verdict(ddi::BUILD_PAGING_BUFFER, STATUS_INSUFFICIENT_RESOURCES),
            Verdict::Fatal
        );
        assert_eq!(
            verdict(
                ddi::BUILD_PAGING_BUFFER,
                STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER
            ),
            Verdict::Expected
        );
    }

    #[test]
    fn scheduler_ddis_accept_only_success() {
        for id in [
            ddi::SUBMIT_COMMAND,
            ddi::SUBMIT_COMMAND_VIRTUAL,
            ddi::PREEMPT_COMMAND,
            ddi::RESET_FROM_TIMEOUT,
            ddi::RESTART_FROM_TIMEOUT,
            ddi::RESET_ENGINE,
            ddi::QUERY_ENGINE_STATUS,
        ] {
            for st in [
                STATUS_INVALID_PARAMETER,
                STATUS_NOT_SUPPORTED,
                STATUS_DEVICE_NOT_READY,
                STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER,
            ] {
                assert_eq!(verdict(id, st), Verdict::Fatal, "{id} {st:#x}");
            }
        }
    }

    #[test]
    fn removed_class_is_fatal_from_every_ddi() {
        for (id, name) in ddi::ALL {
            if id == ddi::NONE {
                continue;
            }
            for st in [
                STATUS_DEVICE_REMOVED,
                STATUS_DEVICE_NOT_CONNECTED,
                STATUS_NO_SUCH_DEVICE,
                STATUS_DEVICE_DOES_NOT_EXIST,
                STATUS_IO_DEVICE_ERROR,
            ] {
                assert_eq!(verdict(id, st), Verdict::Fatal, "{name} {st:#x}");
            }
        }
    }

    #[test]
    fn vidpn_speaks_graphics_but_nothing_else_does() {
        let invalid = 0xC01E_0300u32 as i32; // a STATUS_GRAPHICS_* value
        assert_eq!(
            verdict(ddi::IS_SUPPORTED_VIDPN, invalid),
            Verdict::Expected
        );
        assert_eq!(
            verdict(ddi::QUERY_CHILD_STATUS, 0xC01D_0001u32 as i32),
            Verdict::Expected
        );
        for id in [
            ddi::START_DEVICE,
            ddi::CREATE_DEVICE,
            ddi::DESTROY_DEVICE,
            ddi::SET_POWER_STATE,
            ddi::CB_INDICATE_CHILD,
            ddi::RENDER,
            ddi::OPEN_ALLOCATION,
        ] {
            assert_eq!(verdict(id, invalid), Verdict::Fatal, "{id}");
        }
        // NOT_READY outside an escape is a lost-class answer.
        assert_eq!(
            verdict(ddi::CREATE_ALLOCATION, STATUS_DEVICE_NOT_READY),
            Verdict::Fatal
        );
        assert_eq!(
            verdict(ddi::ESCAPE, STATUS_DEVICE_NOT_READY),
            Verdict::Expected
        );
    }

    #[test]
    fn ordinary_refusals_are_expected_or_suspect_not_fatal() {
        assert_eq!(
            verdict(ddi::CREATE_ALLOCATION, STATUS_NO_MEMORY),
            Verdict::Expected
        );
        assert_eq!(
            verdict(ddi::ESCAPE, STATUS_INVALID_PARAMETER),
            Verdict::Expected
        );
        assert_eq!(
            verdict(ddi::ESCAPE, STATUS_BUFFER_TOO_SMALL),
            Verdict::Expected
        );
        assert_eq!(
            verdict(ddi::CREATE_CONTEXT, STATUS_INVALID_PARAMETER),
            Verdict::Suspect
        );
        assert_eq!(
            verdict(ddi::START_DEVICE, STATUS_INSUFFICIENT_RESOURCES),
            Verdict::Suspect
        );
        assert_eq!(
            verdict(ddi::QUERY_ADAPTER_INFO, STATUS_NOT_SUPPORTED),
            Verdict::Expected
        );
    }

    #[test]
    fn escapes_are_slow_later_than_everything_else() {
        assert_eq!(slow_call_ms(ddi::ESCAPE), SLOW_ESCAPE_MS);
        for (id, _) in ddi::ALL {
            if id != ddi::ESCAPE {
                assert_eq!(slow_call_ms(id), SLOW_CALL_MS);
            }
        }
        assert!(SLOW_ESCAPE_MS > SLOW_CALL_MS);
    }

    #[test]
    fn graphics_class_is_the_c01e_facility_only() {
        assert!(is_graphics_class(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER));
        assert!(is_graphics_class(0x801E_0001u32 as i32));
        assert!(!is_graphics_class(0xC01D_0001u32 as i32));
        assert!(!is_graphics_class(STATUS_INVALID_PARAMETER));
        assert!(is_monitor_class(0xC01D_0001u32 as i32));
    }

    #[test]
    fn pack_round_trips_and_masks_the_hint() {
        let w = pack_ddi_hint(ddi::BUILD_PAGING_BUFFER, 0xFFAB_CDEF);
        assert_eq!(unpack_ddi_hint(w), (ddi::BUILD_PAGING_BUFFER, 0x00AB_CDEF));
        assert_eq!(unpack_ddi_hint(pack_ddi_hint(255, 0)), (255, 0));
    }

    #[test]
    fn ring_keeps_the_newest_n_in_order() {
        let r = EventRing::<4>::new();
        assert_eq!(r.count(), 0);
        assert_eq!(r.nth_newest(0), None);
        for i in 1..=3u32 {
            assert_eq!(r.push(i, i * 10, i * 100), i);
        }
        assert_eq!(r.nth_newest(0), Some((3, 30, 300)));
        assert_eq!(r.nth_newest(2), Some((1, 10, 100)));
        assert_eq!(r.nth_newest(3), None, "only three pushed");
        for i in 4..=9u32 {
            r.push(i, i * 10, i * 100);
        }
        assert_eq!(r.count(), 9);
        // The last four: 9, 8, 7, 6.
        let got: Vec<u32> = (0..4).map(|b| r.nth_newest(b).unwrap().0).collect();
        assert_eq!(got, [9, 8, 7, 6]);
        assert_eq!(r.nth_newest(4), None, "older than the ring");
    }

    #[test]
    fn first_fatal_is_first_wins_and_counts_the_rest() {
        let f = FirstFatal::new();
        assert!(!f.is_ready());
        assert_eq!(f.count(), 0);
        assert!(f.note(|r| {
            r.ddi.store(ddi::BUILD_PAGING_BUFFER, Ordering::Relaxed);
            r.status.store(STATUS_INVALID_PARAMETER as u32, Ordering::Relaxed);
            r.t.store(1234, Ordering::Relaxed);
        }));
        assert!(f.is_ready());
        let mut ran = false;
        assert!(!f.note(|_| ran = true), "the second is not first");
        assert!(!ran, "the second never writes the record");
        assert!(!f.note(|r| r.ddi.store(99, Ordering::Relaxed)));
        assert_eq!(f.count(), 3);
        assert_eq!(f.ddi.load(Ordering::Relaxed), ddi::BUILD_PAGING_BUFFER);
        assert_eq!(f.t.load(Ordering::Relaxed), 1234);
        assert_eq!(
            f.status.load(Ordering::Relaxed),
            STATUS_INVALID_PARAMETER as u32
        );
    }

    #[test]
    fn longest_keeps_only_a_strictly_larger_value() {
        let l = Longest::new();
        assert!(l.note(10, 1, 100));
        assert!(!l.note(10, 2, 200), "equal does not replace");
        assert!(!l.note(5, 3, 300));
        assert!(l.note(11, 4, 400));
        assert_eq!(l.value.load(Ordering::Relaxed), 11);
        assert_eq!(l.tag.load(Ordering::Relaxed), 4);
        assert_eq!(l.t.load(Ordering::Relaxed), 400);
        assert!(!l.note(0, 9, 9));
    }

    #[test]
    fn in_flight_nests_and_never_underflows() {
        let f = InFlight::new();
        assert_eq!(f.mask(), (0, 0));
        assert_eq!(f.oldest(1000), None);
        f.enter(ddi::DESTROY_DEVICE, 100);
        f.enter(ddi::DESTROY_DEVICE, 150);
        f.enter(ddi::BUILD_PAGING_BUFFER, 900);
        f.enter(ddi::QUERY_CHILD_RELATIONS, 950);
        assert_eq!(f.count(ddi::DESTROY_DEVICE), 2);
        let (lo, hi) = f.mask();
        assert_eq!(lo, (1 << ddi::DESTROY_DEVICE) | (1 << ddi::BUILD_PAGING_BUFFER));
        assert_eq!(hi, 1 << (ddi::QUERY_CHILD_RELATIONS - 32));
        // The newest entry stamp of each id: DestroyDevice entered (last) at 150.
        assert_eq!(f.oldest(1000), Some((ddi::DESTROY_DEVICE, 850)));
        f.leave(ddi::DESTROY_DEVICE);
        assert_eq!(f.count(ddi::DESTROY_DEVICE), 1);
        f.leave(ddi::DESTROY_DEVICE);
        f.leave(ddi::DESTROY_DEVICE);
        assert_eq!(f.count(ddi::DESTROY_DEVICE), 0, "a stray leave is ignored");
        assert_eq!(f.oldest(1000), Some((ddi::BUILD_PAGING_BUFFER, 100)));
        // Out-of-range ids are ignored, not a panic.
        f.enter(200, 1);
        f.leave(200);
        assert_eq!(f.count(200), 0);
    }

    #[test]
    fn in_flight_age_never_wraps_into_a_huge_number() {
        let f = InFlight::new();
        f.enter(ddi::ESCAPE, 5000);
        assert_eq!(f.oldest(4000), Some((ddi::ESCAPE, 0)), "a stamp ahead of now is age 0");
        f.enter(ddi::PRESENT, 0);
        assert_eq!(f.oldest(10), Some((ddi::PRESENT, 9)), "stamp 0 is stored as 1");
    }

    #[test]
    fn transfer_kind_names_evictions_and_page_ins() {
        let bar = 3;
        assert_eq!(transfer_kind(bar, 0, bar), PagingKind::Evict);
        assert_eq!(transfer_kind(0, bar, bar), PagingKind::PageIn);
        assert_eq!(transfer_kind(bar, bar, bar), PagingKind::Other);
        assert_eq!(transfer_kind(1, 0, bar), PagingKind::Other, "another segment");
        assert_eq!(transfer_kind(0, 0, bar), PagingKind::Other);
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
            assert!(!is_ring_name(n), "{n} looks like a ring entry");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        assert!(is_ring_name("DdS0F"));
        assert!(is_ring_name("DxT07"));
        assert!(is_ring_name("DzD1a"));
        assert!(!is_ring_name("DdS0G"));
        assert!(!is_ring_name("DaS00"));
        assert!(!is_ring_name("DdX00"));
        assert!(!is_ring_name("DdS000"));
    }

    /// Every `b"..."` literal in the Rust files under `root` (text, file).
    fn byte_literals(root: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    let mut rest = text.as_str();
                    while let Some(i) = rest.find("b\"") {
                        let before = rest[..i].chars().last();
                        let tail = &rest[i + 2..];
                        let Some(end) = tail.find('"') else { break };
                        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            out.push((tail[..end].into(), p.clone()));
                        }
                        rest = &tail[end + 1..];
                    }
                }
            }
        }
        out
    }

    #[test]
    fn counters_collide_with_nothing_in_either_tree() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Not in the other lists of this crate.
        for other in crate::stall_diag::COUNTERS
            .iter()
            .chain(crate::foreign_flip::COUNTERS.iter())
            .chain(crate::flip_completion::COUNTERS.iter())
        {
            assert!(!COUNTERS.contains(other), "{other} collides");
            assert!(!is_ring_name(other), "{other} looks like a ring entry");
        }
        let render = manifest.join("../kmd_render/src");
        let mut scanned = 0;
        if render.exists() {
            let lits = byte_literals(&render);
            assert!(lits.len() > 500, "scan found {} literals", lits.len());
            let mut spelled_in_writer: Vec<String> = Vec::new();
            for (lit, file) in &lits {
                let in_writer = file.file_name().is_some_and(|n| n == "device_lost.rs");
                assert!(
                    !is_ring_name(lit),
                    "{lit} in {} looks like a ring entry name",
                    file.display()
                );
                for mine in COUNTERS {
                    if lit == mine {
                        assert!(in_writer, "{mine} is also written by {}", file.display());
                    }
                    if lit.len() > 14 && lit[..14] == *mine {
                        panic!("{lit} in {} truncates onto {mine}", file.display());
                    }
                }
                if in_writer {
                    spelled_in_writer.push(lit.clone());
                }
                scanned += 1;
            }
            // Every name this list promises is spelled by the writer.
            for mine in COUNTERS {
                assert!(
                    spelled_in_writer.iter().any(|l| l == mine),
                    "{mine} is listed but ddi/device_lost.rs never writes it"
                );
            }
            // The writer spells nothing that is not listed (its other literals would be
            // unreviewed registry values).
            for l in &spelled_in_writer {
                assert!(
                    COUNTERS.contains(&l.as_str()) || l.len() <= 1,
                    "ddi/device_lost.rs spells {l}, which is not in COUNTERS"
                );
            }
        }
        // In this crate the names are quoted strings; only the lists may spell them.
        let mut stack = vec![manifest.join("src")];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let file = p.file_name().unwrap().to_string_lossy().into_owned();
                    if file == "device_lost.rs" {
                        continue;
                    }
                    let text = std::fs::read_to_string(&p).unwrap();
                    for mine in COUNTERS {
                        assert!(
                            !text.contains(&format!("\"{mine}\"")),
                            "{file} spells the counter {mine}"
                        );
                    }
                    scanned += 1;
                }
            }
        }
        assert!(scanned > 20);
    }
}

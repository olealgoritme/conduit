//! Native NVIDIA RM memory service for the 610.88 Windows UMD personality.
//!
//! This path does not create, import, or adopt a GEM/Venus object.  The parent
//! supplies an already brought-up host RM client (`NativeClient`); this module
//! adds a `FERMI_VASPACE_A(GPU_DEVICE)`, real memory objects, and fixed
//! `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` mappings at VidMm's exact VAs.
//!
//! Locking: one PASSIVE mutex serializes RM operations on published objects and
//! every lifetime change; its wait is bounded (`LOCK_MAX_MS`), so a holder stuck
//! on the host fails its waiters with `Busy` instead of blocking them forever.
//! `create` makes the RM object of a new, not yet published allocation outside
//! the mutex.  The leaf spinlock is used only to copy or publish plain state; it
//! is never held over a host round trip, an MDL operation, or an allocation.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::ce_route as cr;
use helios_kmd_logic::desc_cache::{DescTable, Released};
use helios_kmd_logic::hot_path::Paging;
use helios_kmd_logic::rm_batch as rb;
use helios_kmd_logic::native_memory as nm;
use helios_kmd_logic::native_perf as np;
use helios_kmd_logic::native_teardown as nt;
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client as rc;
use wdk_sys::ntddk::{
    MmMapIoSpace, MmMapLockedPagesSpecifyCache, MmUnmapIoSpace, MmUnmapLockedPages,
};
use wdk_sys::{MDL, NTSTATUS, PHYSICAL_ADDRESS, PMDL, _MEMORY_CACHING_TYPE};

use super::gpu::DeviceOwner;
use super::nvrm::{self, Refusal};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::{PassiveMutex, PassiveMutexGuard, SpinLock};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;
const TIMEOUT_MS: u64 = 2_500;
const REPLY_MAX: usize = 512;
const H_VASPACE: u32 = 0x4e57_0001;
const FIRST_DYNAMIC_HANDLE: u32 = 0x4e57_1000;
const PIN_CHUNK_BYTES: u64 = u32::MAX as u64 & !(nm::PAGE_BYTES - 1);
const LOCAL_4K_MAP_FLAGS: u32 = 1 << 8;


/// VA watch (test knobs `NvWatch0`..`NvWatch3`: 4-KiB page numbers, 0 = off). Every
/// page-table update, mapping, eviction and unmap that covers a watched page is
/// logged in a 32-entry ring: `NvWa<k>` = the operation's first VA page, `NvWc<k>`
/// = op << 24 | detail << 16 | process & 0xffff, `NvWN` = events so far. Ops:
/// 1 UpdatePageTable seen (detail: bit0 native, bit1 level != 0, bit2 PTE 0 valid,
/// bit3 PTE 0 system page, bit4 not PASSIVE, bit5 system process), 2/3 foreign
/// map ok/failed, 4 foreign drop, 5/6 native vidmem map ok/failed, 7/8 system map
/// ok/failed, 10 unmapped (also by an eviction; detail = the `Error` on failure).
pub mod watch {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    static PAGES: [AtomicU64; 4] = [const { AtomicU64::new(u64::MAX) }; 4];
    static READ: AtomicU32 = AtomicU32::new(0);
    static EVENTS: AtomicU32 = AtomicU32::new(0);

    fn pages() -> [u64; 4] {
        if READ.load(Ordering::Acquire) == 0 {
            let names: [&'static [u8]; 4] = [b"NvWatch0", b"NvWatch1", b"NvWatch2", b"NvWatch3"];
            for (slot, name) in PAGES.iter().zip(names) {
                let v = crate::diag::read_config_dword(crate::diag::KnobName::new(name), 0);
                slot.store(if v == 0 { u64::MAX } else { u64::from(v) }, Ordering::Relaxed);
            }
            READ.store(1, Ordering::Release);
        }
        [0, 1, 2, 3].map(|i| PAGES[i].load(Ordering::Relaxed))
    }

    /// Whether [va, va + len) covers a watched page.
    pub fn covers(va: u64, len: u64) -> bool {
        let first = va >> 12;
        let end = va.saturating_add(len.max(1)).saturating_add(0xfff) >> 12;
        pages().iter().any(|&p| p != u64::MAX && p >= first && p < end)
    }

    pub fn event(va: u64, len: u64, op: u32, detail: u32, process: usize) {
        if !covers(va, len) {
            return;
        }
        let n = EVENTS.fetch_add(1, Ordering::Relaxed);
        let k = (n % 32) as u8;
        let mut a = *b"NvWa00";
        a[4] = b'0' + k / 10;
        a[5] = b'0' + k % 10;
        crate::diag::record_named_bytes(&a, (va >> 12) as u32);
        let mut c = *b"NvWc00";
        c[4] = b'0' + k / 10;
        c[5] = b'0' + k % 10;
        crate::diag::record_named_bytes(&c, (op << 24) | ((detail & 0xff) << 16) | (process as u32 & 0xffff));
        crate::diag::record_named_bytes(b"NvWN", n + 1);
    }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// `NvPerfMem` (REG_DWORD, default 0): the memory-performance paths of
/// `helios_kmd_logic::native_perf::PerfMem`, read once (PASSIVE) and mirrored to
/// `NvPerfMemOn`. 0 keeps every old path.
pub(crate) fn perf() -> np::PerfMem {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPerfMem"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"NvPerfMemOn", value);
    }
    np::PerfMem(value)
}

/// `NvSysChunk` (REG_DWORD, default 0 = 128): pages per OS descriptor of a
/// system-page leaf update (`native_perf::system_chunk_pages`).
fn system_chunk_pages() -> usize {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvSysChunk"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    np::system_chunk_pages(value)
}

/// Host round trips of native-memory RM work (every `Io::send` and OS-descriptor
/// registration). Leaf updates read the difference across one update; it also
/// counts round trips other threads make meanwhile outside the I/O mutex (a
/// concurrent `create`), so per-update figures are an upper bound.
static ROUND_TRIPS: AtomicU32 = AtomicU32::new(0);

fn round_trips() -> u32 {
    ROUND_TRIPS.load(Ordering::Relaxed)
}

/// Native-memory host round trips so far (`ddi::hot_path`'s `HpMemRtPs`). Any IRQL.
pub(crate) fn round_trip_count() -> u32 {
    round_trips()
}

/// `NvPaging` (REG_DWORD, default 0): `helios_kmd_logic::hot_path::Paging`, read
/// once (PASSIVE) and mirrored to `PgKnob`.
fn paging() -> Paging {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPaging"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    Paging(value)
}

/// `NvDescCacheMax` (REG_DWORD, default 256): idle OS descriptors kept for reuse
/// under `NvPaging` 0x2; 0 keeps none.
fn desc_cache_max() -> usize {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(
            crate::diag::KnobName::new(b"NvDescCacheMax"),
            helios_kmd_logic::desc_cache::DEFAULT_MAX_IDLE as u32,
        );
        if value == u32::MAX {
            value = helios_kmd_logic::desc_cache::DEFAULT_MAX_IDLE as u32;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    value as usize
}

/// Whether idle OS descriptors are kept (`NvPaging` 0x2 and a bound above 0).
fn desc_cache_on() -> bool {
    paging().desc_cache() && desc_cache_max() != 0
}

// Paging fast-path counters (`Pg*`), published by the mirror pass (`publish_hot`):
/// `PgBatchN` batches sent, `PgBatchCalls` RM calls they carried, `PgBatchRef`
/// batches the host refused whole (the host then gets single calls), `PgBatchErr`
/// batches that failed in transport, `PgBatchUs` / `PgBatchMaxUs` their round trips.
static BATCHES: AtomicU32 = AtomicU32::new(0);
static BATCH_CALLS: AtomicU32 = AtomicU32::new(0);
static BATCH_REFUSED: AtomicU32 = AtomicU32::new(0);
static BATCH_FAILED: AtomicU32 = AtomicU32::new(0);
static BATCH_US: AtomicU32 = AtomicU32::new(0);
static BATCH_MAX_US: AtomicU32 = AtomicU32::new(0);
/// `PgDescNew` OS descriptors registered, `PgDescHit` reused for the same pages
/// (`PgDescIdleHit` of them taken from the idle cache), `PgDescFree` freed,
/// `PgDescEvict` idle ones freed by the bound or their allocation's move;
/// `PgDescLive` / `PgDescIdle` the table now.
static DESC_NEW: AtomicU32 = AtomicU32::new(0);
static DESC_HIT: AtomicU32 = AtomicU32::new(0);
static DESC_IDLE_HIT: AtomicU32 = AtomicU32::new(0);
static DESC_FREED: AtomicU32 = AtomicU32::new(0);
static DESC_EVICTED: AtomicU32 = AtomicU32::new(0);
/// `PgIdxN` per-process lookups (evictions, CPU patches, process teardown),
/// `PgIdxScan` allocations they looked at (all of them before the index).
static IDX_LOOKUPS: AtomicU32 = AtomicU32::new(0);
static IDX_SCANNED: AtomicU32 = AtomicU32::new(0);

/// `NvEvict` and `NvQuiesceRef`, written by the registry mirror's pass
/// (`ddi::hot_path::publish`) instead of on every eviction / refusal; the `Pg*`
/// paging counters above. PASSIVE.
pub(crate) fn publish_hot() {
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    let rec = crate::diag::record_named_bytes;
    rec(b"PgKnob", paging().0);
    rec(b"PgBatchN", r(&BATCHES));
    rec(b"PgBatchCalls", r(&BATCH_CALLS));
    rec(b"PgBatchRef", r(&BATCH_REFUSED));
    rec(b"PgBatchErr", r(&BATCH_FAILED));
    rec(b"PgBatchUs", r(&BATCH_US));
    rec(b"PgBatchMaxUs", r(&BATCH_MAX_US));
    rec(b"PgDescNew", r(&DESC_NEW));
    rec(b"PgDescHit", r(&DESC_HIT));
    rec(b"PgDescIdleHit", r(&DESC_IDLE_HIT));
    rec(b"PgDescFree", r(&DESC_FREED));
    rec(b"PgDescEvict", r(&DESC_EVICTED));
    let (live, idle, procs, broken) = {
        let state = STATE.lock();
        let (n, idle) = state.descs.counts();
        (n - idle, idle, state.by_process.len(), state.index_broken)
    };
    rec(b"PgDescLive", live as u32);
    rec(b"PgDescIdle", idle as u32);
    rec(b"PgIdxN", r(&IDX_LOOKUPS));
    rec(b"PgIdxScan", r(&IDX_SCANNED));
    rec(b"PgIdxProcs", procs as u32);
    rec(b"PgIdxBroken", u32::from(broken));
    // The leaf-update costs, also here so they are fresh between their own
    // (every 64th update) writes.
    for cost in [&LEAF_COST, &SYSTEM_COST] {
        rec(cost.names[0], r(&cost.updates));
        rec(cost.names[1], r(&cost.trips));
        rec(cost.names[2], r(&cost.micros));
        rec(cost.names[3], r(&cost.max_micros));
    }
    let evicted = EVICTED.load(Ordering::Relaxed);
    if evicted != 0 {
        crate::diag::record_named_bytes(b"NvEvict", evicted);
    }
    let refused = QUIESCE_REFUSED.load(Ordering::Relaxed);
    if refused != 0 {
        crate::diag::record_named_bytes(b"NvQuiesceRef", refused);
    }
}

/// Leaf-update cost counters (`NvPm*`), published on the first update and every
/// 64th: `NvPmUpd`/`NvPmRt`/`NvPmUs` video-memory updates, their round trips and
/// microseconds, `NvPmMaxUs` the longest; `NvPmSys*` the same for system-page
/// updates; `NvPmSkip` system-page updates that mapped what was mapped already.
struct LeafCost {
    updates: AtomicU32,
    trips: AtomicU32,
    micros: AtomicU32,
    max_micros: AtomicU32,
    names: [&'static [u8]; 4],
}

static LEAF_COST: LeafCost = LeafCost::new([b"NvPmUpd", b"NvPmRt", b"NvPmUs", b"NvPmMaxUs"]);
static SYSTEM_COST: LeafCost =
    LeafCost::new([b"NvPmSysUpd", b"NvPmSysRt", b"NvPmSysUs", b"NvPmSysMaxUs"]);
static SKIPPED_UNCHANGED: AtomicU32 = AtomicU32::new(0);
/// `NvPmBig` / `NvPmHuge`: video-memory mappings made with 64-KiB / 2-MiB pages;
/// `NvPmLgFail`: large-page mappings RM refused (mapped again with 4 KiB);
/// `NvPmHugeMem` / `NvPmHugeFail`: allocations given 2-MiB-page memory / refused it;
/// `NvPmDefer`: RM operations that deferred their TLB invalidate; `NvPmTlbInv`:
/// explicit invalidates after a deferred batch failed (`NvPmTlbErr`: refused).
static MAPPED_BIG: AtomicU32 = AtomicU32::new(0);
static MAPPED_HUGE: AtomicU32 = AtomicU32::new(0);
static LARGE_REFUSED: AtomicU32 = AtomicU32::new(0);
static HUGE_MEMORY: AtomicU32 = AtomicU32::new(0);
static HUGE_REFUSED: AtomicU32 = AtomicU32::new(0);
static DEFERRED: AtomicU32 = AtomicU32::new(0);
static TLB_RESCUE: AtomicU32 = AtomicU32::new(0);
static TLB_RESCUE_FAILED: AtomicU32 = AtomicU32::new(0);
/// Large-page mappings refused in a row; from [`LARGE_STREAK_LIMIT`] on no more
/// are tried (until the driver restarts), so a host whose RM refuses them all
/// costs at most that many extra round trips.
static LARGE_STREAK: AtomicU32 = AtomicU32::new(0);
const LARGE_STREAK_LIMIT: u32 = 32;

impl LeafCost {
    const fn new(names: [&'static [u8]; 4]) -> Self {
        Self {
            updates: AtomicU32::new(0),
            trips: AtomicU32::new(0),
            micros: AtomicU32::new(0),
            max_micros: AtomicU32::new(0),
            names,
        }
    }

    /// One update that started at `start` (100 ns) with `trips_before` round trips.
    fn note(&self, start: u64, trips_before: u32) {
        let micros =
            np::ticks_to_us(crate::adapter::foreign_scanout::now_100ns().saturating_sub(start));
        let trips = round_trips().wrapping_sub(trips_before);
        let n = self.updates.fetch_add(1, Ordering::Relaxed) + 1;
        let total_trips = self.trips.fetch_add(trips, Ordering::Relaxed).wrapping_add(trips);
        let total_micros = self
            .micros
            .fetch_add(micros, Ordering::Relaxed)
            .wrapping_add(micros);
        let max = self.max_micros.fetch_max(micros, Ordering::Relaxed).max(micros);
        if np::publish_due(n) {
            crate::diag::record_named_bytes(self.names[0], n);
            crate::diag::record_named_bytes(self.names[1], total_trips);
            crate::diag::record_named_bytes(self.names[2], total_micros);
            crate::diag::record_named_bytes(self.names[3], max);
        }
    }
}

/// Count `counter` and publish it under `name` on the publish cadence.
fn bump(counter: &AtomicU32, name: &[u8]) {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if np::publish_due(n) {
        crate::diag::record_named_bytes(name, n);
    }
}
/// Replacements whose RM idle-channels quiesce was refused (`update_leaf`).
static QUIESCE_REFUSED: AtomicU32 = AtomicU32::new(0);
static IO_MUTEX: SpinLock<Option<&'static PassiveMutex>> = SpinLock::new(None);
static STATE: SpinLock<State> = SpinLock::new(State::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotReady,
    Busy,
    Invalid,
    Unsupported,
    NoMemory,
    Host,
    WrongAddress,
    Overlap,
    Poisoned,
}

impl Error {
    pub const fn ntstatus(self) -> NTSTATUS {
        match self {
            Self::NotReady => wdk_sys::STATUS_DEVICE_NOT_READY,
            Self::Busy => wdk_sys::STATUS_DEVICE_BUSY,
            Self::Invalid | Self::Overlap => wdk_sys::STATUS_INVALID_PARAMETER,
            Self::Unsupported => wdk_sys::STATUS_NOT_SUPPORTED,
            Self::NoMemory => wdk_sys::STATUS_NO_MEMORY,
            Self::Host | Self::WrongAddress | Self::Poisoned => wdk_sys::STATUS_IO_DEVICE_ERROR,
        }
    }
}

/// Base RM objects supplied by the parent's normal root/device/subdevice
/// bring-up.  No handle is guessed or borrowed from a process client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeClient {
    pub epoch: u64,
    pub ctl: u32,
    pub gpu: u32,
    pub root: u32,
    pub minor: u32,
    /// RM's GPU id (`CARD_INFO`), which picks the GPU's DRM node.
    pub gpu_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Created {
    pub id: u32,
    pub size: u64,
    pub segment: u32,
    pub system_memory: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuView {
    pub address: u64,
    pub len: u64,
    /// Zero for the direct `pSystemMem` view.  Aperture unmaps use this value
    /// only as a generation check for RM-backed views.
    pub generation: u32,
}

#[derive(Clone, Copy)]
struct HostMapping {
    process: usize,
    piece: nm::MappingPiece,
    virt: u32,
    /// The OS-descriptor memory object of VidMm's system pages this mapping
    /// shows (`update_leaf_system`), 0 when it maps the allocation's RM memory.
    desc: u32,
}

struct Pin {
    mdl: usize,
    pages: usize,
    kernel_va: u64,
}

struct CpuChunk {
    physical: u64,
    map_ch: u32,
    cookie: u64,
    rm_mapped: bool,
    host_id: u32,
}

struct CpuMapping {
    view: CpuView,
    chunks: Vec<CpuChunk>,
    kernel_va: u64,
    aperture_offset: Option<u64>,
}

struct Allocation {
    id: u32,
    epoch: u64,
    memory: u32,
    size: u64,
    system_memory: u64,
    system_kernel_va: u64,
    pins: Vec<Pin>,
    mappings: Vec<HostMapping>,
    cpu: Option<CpuMapping>,
    resident: Option<(u64, bool)>,
    poisoned: bool,
    /// What CreateAllocation wrote into the NVDA record. dxgkrnl hands the UMD
    /// the record the KMD's OpenAllocation writes (create's copy is dropped),
    /// so the same values are written again at open (`writeback_open`).
    writeback: Option<Writeback>,
    /// The 2D picture the D3D record describes (`nm::d3d_surface`), if any.
    surface: Option<nm::D3dSurface>,
    /// The host GEM handle of this memory on the native scanout DRM file
    /// (`native_scanout`), 0 until the allocation is first shown.
    scanout_gem: u32,
    /// The RM memory has 2-MiB pages (NvPerfMem 0x10): mappings may use them.
    huge: bool,
}

/// The record fields CreateAllocation wrote, re-applied at open.
#[derive(Clone, Copy)]
enum Writeback {
    /// Records of no known UMD: RM handles and 64-KiB-unit sizes.
    Native(nm::PrivateWriteback),
    /// D3D11/D3D12 records: the 610.88 KMD's field set (`nm::d3d_writeback`).
    D3d([(usize, u32); nm::D3D_WRITEBACK_FIELDS]),
    /// Vulkan (`NVGL`) records: `nm::nvgl_writeback`.
    Nvgl([(usize, u32); nm::NVGL_WRITEBACK_FIELDS]),
    /// CUDA (`**VN`) records: `nm::cuda_writeback`.
    Cuda([(usize, u32); nm::CUDA_WRITEBACK_FIELDS]),
}

impl Writeback {
    fn apply(&self, bytes: &mut [u8]) -> Result<(), nm::PrivateError> {
        match self {
            Self::Native(w) => nm::writeback_allocation_private(bytes, w),
            Self::D3d(fields) => nm::apply_fields(bytes, fields),
            Self::Nvgl(fields) => nm::apply_fields(bytes, fields),
            Self::Cuda(fields) => nm::apply_fields(bytes, fields),
        }
    }
}

struct State {
    client: Option<NativeClient>,
    allocations: Vec<Allocation>,
    spaces: Vec<(usize, u32)>,
    /// Per-process space -> its windows: (space, window, first VA, end VA), see
    /// [`LOW_WINDOW_BASE`] and [`HIGH_WINDOW_BASE`].
    windows: Vec<(u32, u32, u64, u64)>,
    /// System pages of NON-native allocations (dxgkrnl's monitored fences and
    /// the like) mapped into a process's space (`update_foreign_leaf`).
    foreign: Vec<HostMapping>,
    /// Every OS-descriptor object of the paging path (system-page and foreign
    /// leaf updates) with its pages: for CPU access to what the GPU sees there
    /// (`patch_gpu_bytes`), the unchanged checks, and reuse for the same pages
    /// (`helios_kmd_logic::desc_cache`, `NvPaging` 0x2).
    descs: DescTable,
    /// Per process, the allocations that have (or had: pruned lazily) a mapping
    /// in its space, so an update looks at that process's allocations only.
    by_process: Vec<(usize, Vec<u32>)>,
    /// A `by_process` record could not be made (no memory): every lookup scans
    /// all allocations, as before the index, until the next transport reset.
    index_broken: bool,
    orphan_pins: Vec<Pin>,
    idle_spaces: Vec<usize>,
    next_handle: u32,
}

impl State {
    const fn new() -> Self {
        Self {
            client: None,
            allocations: Vec::new(),
            spaces: Vec::new(),
            windows: Vec::new(),
            foreign: Vec::new(),
            descs: DescTable::new(),
            by_process: Vec::new(),
            index_broken: false,
            orphan_pins: Vec::new(),
            idle_spaces: Vec::new(),
            next_handle: FIRST_DYNAMIC_HANDLE,
        }
    }

    fn client(&self) -> Result<NativeClient, Error> {
        self.client.ok_or(Error::NotReady)
    }

    /// `allocations` is kept sorted by id (`create` inserts in place).
    fn index_of(&self, id: u32) -> Option<usize> {
        self.allocations.binary_search_by_key(&id, |a| a.id).ok()
    }

    fn allocation(&self, id: u32) -> Result<&Allocation, Error> {
        let i = self.index_of(id).ok_or(Error::Invalid)?;
        Ok(&self.allocations[i])
    }

    fn allocation_mut(&mut self, id: u32) -> Result<&mut Allocation, Error> {
        let i = self.index_of(id).ok_or(Error::Invalid)?;
        Ok(&mut self.allocations[i])
    }

    /// Record that allocation `id` has a mapping in `process`'s space.
    fn note_mapped(&mut self, process: usize, id: u32) {
        if self.index_broken {
            return;
        }
        let at = match self.by_process.iter().position(|(p, _)| *p == process) {
            Some(at) => at,
            None => {
                if self.by_process.try_reserve(1).is_err() {
                    self.index_broken = true;
                    return;
                }
                self.by_process.push((process, Vec::new()));
                self.by_process.len() - 1
            }
        };
        let ids = &mut self.by_process[at].1;
        if let Err(i) = ids.binary_search(&id) {
            if ids.try_reserve(1).is_err() {
                self.index_broken = true;
                return;
            }
            ids.insert(i, id);
        }
    }

    /// The allocations that may have a mapping in `process`'s space: the index's
    /// (a superset), or all of them while the index is broken.
    fn candidates(&self, process: usize, out: &mut Vec<u32>) -> Result<(), Error> {
        out.clear();
        if self.index_broken {
            out.try_reserve(self.allocations.len()).map_err(|_| Error::NoMemory)?;
            out.extend(self.allocations.iter().map(|a| a.id));
        } else if let Some((_, ids)) = self.by_process.iter().find(|(p, _)| *p == process) {
            out.try_reserve(ids.len()).map_err(|_| Error::NoMemory)?;
            out.extend_from_slice(ids);
        }
        IDX_SCANNED.fetch_add(out.len() as u32, Ordering::Relaxed);
        IDX_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Drop the ids of `process`'s index entry whose allocation is gone or has no
    /// mapping there any more.
    fn prune_index(&mut self, process: usize) {
        let Some(at) = self.by_process.iter().position(|(p, _)| *p == process) else {
            return;
        };
        let mut ids = core::mem::take(&mut self.by_process[at].1);
        ids.retain(|id| {
            self.index_of(*id).is_some_and(|i| {
                self.allocations[i].mappings.iter().any(|m| m.process == process)
            })
        });
        if ids.is_empty() {
            self.by_process.swap_remove(at);
        } else {
            self.by_process[at].1 = ids;
        }
    }

    fn mint(&mut self) -> Result<u32, Error> {
        let out = self.next_handle;
        if out == 0 || out >= 0x4e5f_ffff {
            return Err(Error::NoMemory);
        }
        self.next_handle = out.checked_add(1).ok_or(Error::NoMemory)?;
        Ok(out)
    }
}

fn mutex(passive: PassiveLevel) -> Result<&'static PassiveMutex, Error> {
    if let Some(mutex) = *IO_MUTEX.lock() {
        return Ok(mutex);
    }
    let mutex = Box::leak(Box::new(
        PassiveMutex::try_new(passive).ok_or(Error::NoMemory)?,
    ));
    let mut slot = IO_MUTEX.lock();
    if let Some(existing) = *slot {
        return Ok(existing);
    }
    *slot = Some(mutex);
    Ok(mutex)
}

/// The longest any caller waits for the native-memory mutex. Holders make host
/// round trips, each bounded and refused at once while the host does not answer
/// (`ctrl::raw_roundtrip`), so a holder normally lets go well within this; VidMm's
/// paging thread must still never wait on one indefinitely.
const LOCK_MAX_MS: u64 = 20_000;
/// Waits for the mutex that gave up after [`LOCK_MAX_MS`] (`NvMemLockTo`).
static LOCK_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

fn lock(passive: PassiveLevel) -> Result<PassiveMutexGuard<'static>, Error> {
    let start = crate::adapter::foreign_scanout::now_100ns();
    match mutex(passive)?.lock_within(passive, LOCK_MAX_MS) {
        Some(guard) => Ok(guard),
        None => {
            // An escape abort returns early; only a wait that ran out is a timeout.
            let waited = crate::adapter::foreign_scanout::now_100ns().saturating_sub(start);
            if waited >= LOCK_MAX_MS * 10_000 {
                let n = LOCK_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
                crate::diag::record_named_bytes(b"NvMemLockTo", n);
            }
            Err(Error::Busy)
        }
    }
}

pub(crate) struct Io<'a> {
    pub(super) passive: PassiveLevel,
    pub(super) adapter: &'a AdapterContext,
    pub(super) client: NativeClient,
}

impl Io<'_> {
    pub(super) fn send(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, Error> {
        ROUND_TRIPS.fetch_add(1, Ordering::Relaxed);
        let mut seen = None;
        let n = nvrm::forward(
            self.passive,
            self.adapter,
            KMD,
            0,
            req,
            resp,
            TIMEOUT_MS,
            0,
            0,
            &mut seen,
        )
        .map_err(|_| Error::Host)?;
        if seen.is_some_and(|epoch| epoch != self.client.epoch) {
            return Err(Error::NotReady);
        }
        Ok(n)
    }

    pub(super) fn exchange(
        &self,
        file: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, Error> {
        let total = rc::MSG_HDR + rc::IOCTL_REQ + data.len() + nested.len();
        let mut request = Vec::new();
        request
            .try_reserve_exact(total)
            .map_err(|_| Error::NoMemory)?;
        request.resize(total, 0);
        let n = rc::build_ioctl(&mut request, file, cmd, data, nested).ok_or(Error::Invalid)?;
        self.send(request.get(..n).ok_or(Error::Invalid)?, resp)
    }

    fn rm_alloc(
        &self,
        parent: u32,
        object: u32,
        class: u32,
        params: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let block = rc::nvos64(self.client.root, parent, object, class, params.len() as u32);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            self.client.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            params,
            &mut response,
        )?;
        let reply = rc::parse_ioctl_reply(response.get(..n).ok_or(Error::Host)?)
            .map_err(|_| Error::Host)?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS64_STATUS_AT).map_err(
            |error| {
                // NvRmAlCls: the refused class; NvRmAlSt: NV_STATUS, 0x8000_0000|errno
                // for a host refusal, 0xffff_ffff for a malformed reply.
                crate::diag::record_named_bytes(b"NvRmAlCls", class);
                crate::diag::record_named_bytes(b"NvRmAlSt", rm_error_code(error));
                Error::Host
            },
        )?;
        Ok(reply.nested.to_vec())
    }

    fn rm_free(&self, parent: u32, object: u32) -> Result<(), Error> {
        let block = rc::nvos00(self.client.root, parent, object);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            self.client.ctl,
            rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
            &block,
            &[],
            &mut response,
        )?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS00_STATUS_AT)
            .map(|_| ())
            .map_err(|_| Error::Host)
    }

    pub(super) fn open_file(&self, device_type: u32) -> Result<u32, Error> {
        let mut request = [0u8; 32];
        let n = rc::build_open(&mut request, device_type).ok_or(Error::Invalid)?;
        let mut response = [0u8; 64];
        let len = self.send(request.get(..n).ok_or(Error::Invalid)?, &mut response)?;
        rc::parse_open_reply(response.get(..len).ok_or(Error::Host)?).ok_or(Error::Host)
    }

    pub(super) fn close_file(&self, handle: u32) -> Result<(), Error> {
        if handle == 0 {
            return Ok(());
        }
        let mut request = [0u8; 32];
        let n = rc::build_close(&mut request, handle).ok_or(Error::Invalid)?;
        let mut response = [0u8; 64];
        let len = self.send(request.get(..n).ok_or(Error::Invalid)?, &mut response)?;
        response
            .get(..len)
            .and_then(rc::reply_status)
            .filter(|status| *status == 0)
            .map(|_| ())
            .ok_or(Error::Host)
    }

    fn register_fd(&self, channel: u32) -> Result<(), Error> {
        let params = rc::register_fd_params(self.client.ctl);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            channel,
            rc::nv_cmd(rc::ESC_REGISTER_FD, 4),
            &params,
            &[],
            &mut response,
        )?;
        rc::parse_ioctl_reply(response.get(..n).ok_or(Error::Host)?)
            .map(|_| ())
            .map_err(|_| Error::Host)
    }
}

/// Install the base client and its GPU_DEVICE VA space. Per-process spaces are
/// created on demand; the parent's base RM bring-up owns ctl/gpu/root. All VA
/// spaces are freed by the transport-wide client teardown.
pub fn install_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    client: NativeClient,
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    {
        let state = STATE.lock();
        if state.client.is_some() || !state.allocations.is_empty() {
            return Err(Error::Busy);
        }
    }
    let io = Io {
        passive,
        adapter,
        client,
    };
    io.rm_alloc(
        nd::H_DEVICE,
        H_VASPACE,
        cc::FERMI_VASPACE_A,
        &cc::vaspace_params(),
    )?;
    {
        let mut state = STATE.lock();
        state.spaces.clear();
        state.windows.clear();
        state.foreign.clear();
        state.descs.clear();
        state.by_process.clear();
        state.index_broken = false;
        state
            .spaces
            .try_reserve_exact(1)
            .map_err(|_| Error::NoMemory)?;
    }
    STATE.lock().client = Some(client);
    STATE.lock().spaces.push((0, H_VASPACE));
    Ok(())
}

/// The shared client and VA space used by channel creation. The process token is
/// only an association key; it never selects or borrows a user-mode RM client.
pub fn channel_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
) -> Result<(NativeClient, u32), Error> {
    // Under the I/O mutex (a recursive KMUTEX: `create` already holds it), like
    // every other `ensure_space` caller, so two threads cannot both find no space
    // for one process and each allocate one.
    let _guard = lock(passive)?;
    // Copy the client out first: a guard in an `if let` scrutinee lives through
    // the body (edition 2021), and `ensure_space` takes STATE again on this
    // non-reentrant spinlock (CreateContext spun forever in ensure_space).
    let existing = STATE.lock().client.as_ref().copied();
    if let Some(client) = existing {
        let io = Io {
            passive,
            adapter,
            client,
        };
        let space = ensure_space(&io, process)?;
        return Ok((client, space));
    }
    // First user: bring the client up, then give this process its OWN space like
    // every later one. Answering the shared device space here bound the first
    // process's channels (DWM's, at every boot) to a space its mappings never
    // went to: ring fetch FAULT_PDE at 0x4000000.
    let client = super::nvda::memory_client(passive, adapter, process)?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let space = ensure_space(&io, process)?;
    Ok((client, space))
}

/// Return this process's real VA space, creating it on first use. VidMm's
/// address ranges remain process-scoped; no second process can alias them by
/// sharing a root VA space.
fn ensure_space(io: &Io<'_>, process: usize) -> Result<u32, Error> {
    if let Some((_, space)) = STATE
        .lock()
        .spaces
        .iter()
        .find(|(owner, _)| *owner == process)
    {
        return Ok(*space);
    }
    STATE
        .lock()
        .spaces
        .try_reserve_exact(1)
        .map_err(|_| Error::NoMemory)?;
    let space = STATE.lock().mint()?;
    // NvVaDevice=1 (experiment): the device's own VA space (index GPU_DEVICE,
    // shared by every process, no window) as NVK on RM uses, to tell whether
    // the new per-process spaces are what the graphics copy engine fails on.
    let device_space =
        crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvVaDevice"), 0) == 1;
    let reply = io.rm_alloc(
        nd::H_DEVICE,
        space,
        cc::FERMI_VASPACE_A,
        &if device_space { cc::vaspace_params() } else { cc::vaspace_params_vidmm() },
    )?;
    if device_space {
        STATE.lock().spaces.push((process, space));
        return Ok(space);
    }
    // Diagnosis: the space RM made (vaSize at +8, vaBase at +40 of the
    // returned NV_VASPACE_ALLOCATION_PARAMETERS), in 64-KiB units.
    crate::diag::record_named_bytes(
        b"NvSpaceSz",
        (rc::get64(&reply, 8).unwrap_or(u64::MAX) >> 16) as u32,
    );
    crate::diag::record_named_bytes(
        b"NvSpaceBase",
        (rc::get64(&reply, 40).unwrap_or(u64::MAX) >> 16) as u32,
    );
    // One virtual object over all of VidMm's low VAs, before RM maps anything
    // of its own into the space: RM's first-fit internal mappings (GR context
    // buffers) then land above 4.5 GiB, in the [4 GiB, 8 GiB) range the KMD
    // reserves from VidMm, and every VidMm VA below 4 GiB is mapped inside
    // this window at its exact address.
    let window = match STATE.lock().mint() {
        Ok(window) => window,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, space);
            return Err(error);
        }
    };
    let params = cc::virtual_params(
        io.client.root,
        space,
        Some(LOW_WINDOW_BASE),
        LOW_WINDOW_END - LOW_WINDOW_BASE,
    );
    if let Err(error) = io.rm_alloc(nd::H_DEVICE, window, cc::NV50_MEMORY_VIRTUAL, &params) {
        let _ = io.rm_free(nd::H_DEVICE, space);
        return Err(error);
    }
    {
        let mut state = STATE.lock();
        state.spaces.push((process, space));
        let _ = state.windows.try_reserve(2);
        state.windows.push((space, window, LOW_WINDOW_BASE, LOW_WINDOW_END));
    }
    // The same above the [4 GiB, 8 GiB) range the KMD reserves from VidMm: RM's own
    // first-fit mappings stay inside that range instead of growing into VAs VidMm
    // hands out (they reached 0x2_3200_0000, and every later fixed mapping there was
    // refused with NO_MEMORY). Optional: without it, mappings fall back to fixed
    // virtual objects.
    // The guard of `STATE.lock()` must end before the block, which locks STATE again.
    let minted = STATE.lock().mint();
    if let Ok(high) = minted {
        let params = cc::virtual_params(
            io.client.root,
            space,
            Some(HIGH_WINDOW_BASE),
            HIGH_WINDOW_END - HIGH_WINDOW_BASE,
        );
        match io.rm_alloc(nd::H_DEVICE, high, cc::NV50_MEMORY_VIRTUAL, &params) {
            Ok(_) => {
                let mut state = STATE.lock();
                let _ = state.windows.try_reserve(1);
                state.windows.push((space, high, HIGH_WINDOW_BASE, HIGH_WINDOW_END));
            }
            Err(error) => crate::diag::record_named_bytes(b"NvHighWin", error as u32 + 1),
        }
    }
    crate::diag::record_named_bytes(b"NvSpaceProc", process as u32);
    crate::diag::record_named_bytes(b"NvSpaceH", space);
    // Diagnosis: where RM puts a non-fixed 2-MiB virtual object now (in 64-KiB
    // units; 0xffffffff = refused), i.e. where its own mappings would go.
    let minted = STATE.lock().mint();
    if let Ok(probe) = minted {
        let params = cc::virtual_params(io.client.root, space, None, 0x20_0000);
        let at = match io.rm_alloc(nd::H_DEVICE, probe, cc::NV50_MEMORY_VIRTUAL, &params) {
            Ok(reply) => {
                let _ = io.rm_free(nd::H_DEVICE, probe);
                (rc::get64(&reply, 80).unwrap_or(u64::MAX) >> 16) as u32
            }
            Err(_) => u32::MAX,
        };
        crate::diag::record_named_bytes(b"NvSpaceFree", at);
    }
    Ok(space)
}

/// The low window of a per-process space: [64 KiB, 4 GiB), VidMm's fences
/// (0x10000) and the UMDs' low VAs included.
const LOW_WINDOW_BASE: u64 = 0x1_0000;
const LOW_WINDOW_END: u64 = 0x1_0000_0000;
/// The high window of a per-process space: [8 GiB, 64 GiB), where VidMm places
/// allocations above the reserved [4 GiB, 8 GiB) and CUDA reserves its fixed range. A window over the whole 16 TiB
/// exhausted RM's page-table pool (pool_alloc NO_MEMORY); VAs above 64 GiB still
/// map through fixed virtual objects.
const HIGH_WINDOW_BASE: u64 = 0x2_0000_0000;
const HIGH_WINDOW_END: u64 = 0x10_0000_0000;

/// The window of `space` that holds [va, end), if any.
fn window_for(space: u32, va: u64, end: u64) -> Option<u32> {
    STATE
        .lock()
        .windows
        .iter()
        .find(|(owner, _, base, limit)| *owner == space && va >= *base && end <= *limit)
        .map(|(_, window, _, _)| *window)
}

fn windows_of(space: u32) -> Vec<u32> {
    STATE
        .lock()
        .windows
        .iter()
        .filter(|(owner, _, _, _)| *owner == space)
        .map(|(_, window, _, _)| *window)
        .collect()
}

fn is_window(virt: u32) -> bool {
    STATE.lock().windows.iter().any(|(_, window, _, _)| *window == virt)
}


/// Validate a UMD GPFIFO allocation before it becomes a hardware queue. The
/// entire ring must be covered by one live native mapping at the exact VA.
pub fn ring_backing(
    id: u32,
    process: usize,
    gpfifo_va: u64,
    ring_bytes: u64,
) -> Result<super::native_channel::NativeRingBacking, Error> {
    let state = STATE.lock();
    let allocation = state.allocation(id)?;
    let end = gpfifo_va.checked_add(ring_bytes).ok_or(Error::Invalid)?;
    if ring_bytes == 0 || ring_bytes > allocation.size {
        return Err(Error::Invalid);
    }
    // Only the queue's own process: every process has its own VA space and the
    // same VA in another one (a shared ring, a stale piece) is another mapping.
    let Some(mapping) = allocation.mappings.iter().find(|mapping| {
        mapping.process == process
            && mapping.piece.va <= gpfifo_va
            && mapping
                .piece
                .end()
                .is_some_and(|mapping_end| mapping_end >= end)
    }) else {
        // MakeResident is asynchronous: the UMD creates its queue before VidMm
        // has paged the ring in and updated its PTEs. RM's channel needs only
        // the GPFIFO VA, which VidMm maps before any submission runs; the
        // VA-space match is checked once mapped (va_space 0 = not yet).
        return Ok(super::native_channel::NativeRingBacking {
            memory: allocation.memory,
            mapped_bytes: ring_bytes,
            va_space: 0,
            id,
            offset: 0,
        });
    };
    let va_space = state
        .spaces
        .iter()
        .find(|(process, _)| *process == mapping.process)
        .map(|(_, space)| *space)
        .ok_or(Error::NotReady)?;
    Ok(super::native_channel::NativeRingBacking {
        memory: allocation.memory,
        mapped_bytes: ring_bytes,
        va_space,
        id,
        offset: mapping.piece.memory_offset + (gpfifo_va - mapping.piece.va),
    })
}

fn local_params(root: u32, size: u64) -> [u8; rc::MEM_ALLOC_BYTES] {
    rc::mem_alloc_params(
        root,
        &rc::SurfaceLayout {
            width: 0,
            height: 0,
            pitch: 0,
            size,
        },
    )
}

extern "C" {
    fn helios_lock_user_pages_seh(virtual_address: *mut c_void, length: u32) -> PMDL;
    fn helios_unlock_system_buffer(mdl: PMDL);
}

fn unlock_pins(pins: &mut Vec<Pin>) {
    for pin in pins.drain(..) {
        if pin.kernel_va != 0 {
            // SAFETY: this exact kernel mapping belongs to the pin and is removed
            // before its MDL is unlocked, at PASSIVE.
            unsafe { MmUnmapLockedPages(pin.kernel_va as *mut c_void, pin.mdl as PMDL) };
        }
        if pin.mdl != 0 {
            // SAFETY: each MDL was returned by helios_lock_user_pages_seh and is
            // owned by this pin.  PASSIVE, outside STATE's spinlock.
            unsafe { helios_unlock_system_buffer(pin.mdl as PMDL) };
        }
    }
}

fn preserve_ambiguous_pins(pins: &mut Vec<Pin>) {
    STATE.lock().orphan_pins.append(pins);
}

/// A create that will not finish gives back its RM memory; the pins unlock only
/// when RM confirms the free (else the host may still name the pages).
fn abandon_memory(io: &Io<'_>, memory: u32, pins: &mut Vec<Pin>) {
    if io.rm_free(nd::H_DEVICE, memory).is_ok() {
        unlock_pins(pins);
    } else {
        preserve_ambiguous_pins(pins);
    }
}

fn pin_system_memory(system_memory: u64, size: u64) -> Result<(Vec<Pin>, Vec<u64>), Error> {
    let page_count = usize::try_from(size / nm::PAGE_BYTES).map_err(|_| Error::NoMemory)?;
    let mut pins = Vec::new();
    let mut pfns = Vec::new();
    pfns.try_reserve_exact(page_count)
        .map_err(|_| Error::NoMemory)?;
    let mut offset = 0u64;
    while offset < size {
        let len = (size - offset).min(PIN_CHUNK_BYTES);
        let address = system_memory.checked_add(offset).ok_or(Error::Invalid)?;
        let mdl = unsafe { helios_lock_user_pages_seh(address as *mut c_void, len as u32) };
        if mdl.is_null() {
            unlock_pins(&mut pins);
            return Err(Error::Invalid);
        }
        let pages = usize::try_from(len / nm::PAGE_BYTES).map_err(|_| Error::NoMemory)?;
        // SAFETY: a locked MDL's PFN array follows MDL and has `pages` entries.
        let chunk = unsafe {
            core::slice::from_raw_parts(
                (mdl as *const u8).add(core::mem::size_of::<MDL>()) as *const u64,
                pages,
            )
        };
        pfns.extend_from_slice(chunk);
        let kernel_va = if size <= PIN_CHUNK_BYTES {
            unsafe {
                MmMapLockedPagesSpecifyCache(
                    mdl,
                    0, // KernelMode: this view is valid in every process context
                    _MEMORY_CACHING_TYPE::MmCached,
                    core::ptr::null_mut(),
                    0,
                    16, // NormalPagePriority
                ) as u64
            }
        } else {
            0
        };
        pins.push(Pin {
            mdl: mdl as usize,
            pages,
            kernel_va,
        });
        offset += len;
    }
    if pfns.len() != page_count || pins.iter().map(|p| p.pages).sum::<usize>() != page_count {
        unlock_pins(&mut pins);
        return Err(Error::Invalid);
    }
    Ok((pins, pfns))
}

fn alloc_system_descriptor(io: &Io<'_>, memory: u32, pfns: &[u64], size: u64) -> Result<(), Error> {
    let (kind, deep, big) = nvrm::page_run_table(io.passive, pfns).map_err(|_| Error::NoMemory)?;
    let block = cr::nvos02_osdesc(
        io.client.root,
        nd::H_DEVICE,
        memory,
        pfns.first()
            .copied()
            .unwrap_or(0)
            .wrapping_mul(nm::PAGE_BYTES),
        size,
    );
    let mut request = [0u8; rc::MSG_HDR + rc::IOCTL_REQ + cr::NVOS02_FD_BYTES];
    let n = rc::build_ioctl(
        &mut request,
        io.client.gpu,
        rc::nv_cmd(cr::ESC_RM_ALLOC_MEMORY, cr::NVOS02_FD_BYTES as u32),
        &block,
        &[],
    )
    .ok_or(Error::Invalid)?;
    let mut response = [0u8; REPLY_MAX];
    ROUND_TRIPS.fetch_add(1, Ordering::Relaxed);
    let sent = nvrm::forward_kmd_registration(
        io.passive,
        io.adapter,
        request.get(..n).ok_or(Error::Invalid)?,
        kind,
        &deep,
        &mut response,
        TIMEOUT_MS,
    );
    let reply = match sent {
        Ok(len) => {
            drop(big);
            response.get(..len).ok_or(Error::Host)?
        }
        Err(Refusal::Transport(VirtioError::Timeout)) => {
            // The host may have consumed the table and retained the registration;
            // the MDLs must remain locked until the client/transport is torn down.
            core::mem::forget(big);
            return Err(Error::Host);
        }
        Err(refusal) => {
            // NvSysHost: 0x2000_0000 | refusal kind for a transport/forward refusal.
            crate::diag::record_named_bytes(b"NvSysHost", 0x2000_0000 | refusal_code(&refusal));
            crate::diag::record_named_bytes(b"NvSysPages", pfns.len() as u32);
            return Err(Error::Host);
        }
    };
    rc::rm_reply(reply, cr::NVOS02_STATUS_AT).map(|_| ()).map_err(|error| {
        // NvSysHost: RM's NV_STATUS, 0x1000_0000 | errno for a host refusal,
        // 0x3000_0000 for a malformed reply; NvSysPages: the descriptor's pages.
        let code = match error {
            rc::RmError::Status(status) => status,
            rc::RmError::Host(errno) => 0x1000_0000 | errno,
            rc::RmError::Malformed => 0x3000_0000,
        };
        crate::diag::record_named_bytes(b"NvSysHost", code);
        crate::diag::record_named_bytes(b"NvSysPages", pfns.len() as u32);
        // The refused page frames' range, and how many lie outside guest RAM
        // as Windows reports it below 4 GiB / above (pfn >= 0x80000 and < 0x100000).
        let lo = pfns.iter().copied().min().unwrap_or(0);
        let hi = pfns.iter().copied().max().unwrap_or(0);
        let hole = pfns.iter().filter(|&&p| (0x80000..0x100000).contains(&p)).count();
        crate::diag::record_named_bytes(b"NvSysPfnLo", lo as u32);
        crate::diag::record_named_bytes(b"NvSysPfnHi", hi as u32);
        crate::diag::record_named_bytes(b"NvSysPfnHole", hole as u32);
        crate::diag::record_named_bytes(b"NvSysPfn0", pfns.first().copied().unwrap_or(0) as u32);
        Error::Host
    })
}

fn refusal_code(refusal: &Refusal) -> u32 {
    match refusal {
        Refusal::MsgType => 1,
        Refusal::NotOwned => 2,
        Refusal::NoResources => 3,
        Refusal::Forbidden => 4,
        Refusal::BadRange => 5,
        Refusal::Transport(_) => 6,
        #[allow(unreachable_patterns)]
        _ => 7,
    }
}

/// Create the real local-linear or pSystemMem OS-descriptor backing.  All
/// private-data validation has already completed in `kmd_logic`; no host
/// allocation is attempted for an unsupported shape.
static STEP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
fn step(n: u32) {
    STEP.store(n, Ordering::Relaxed);
}
/// The last checkpoint `create` reached (diagnostic for refused allocations).
pub fn last_step() -> u32 {
    STEP.load(Ordering::Relaxed)
}

pub fn create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    private: &nm::AllocationPrivate,
    private_bytes: &mut [u8],
) -> Result<Created, Error> {
    step(1);
    // The client, the base VA space and the new handle are taken under the mutex;
    // the RM object itself is made after it is released (no holder of the mutex
    // waits on this allocation's round trips, so VidMm's paging thread is never
    // queued behind a host that does not answer), and the record is published
    // under it again, only if the client generation is still the one used.
    let (client, epoch, space, memory) = {
        let _guard = lock(passive)?;
        step(2);
        // As in `channel_client`: release STATE before `channel_client` re-takes it.
        let existing = STATE.lock().client.as_ref().copied();
        let client = match existing {
            Some(client) => client,
            None => channel_client(passive, adapter, 0)?.0,
        };
        step(3);
        let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
        if epoch == 0 || epoch != client.epoch {
            return Err(Error::NotReady);
        }
        let io = Io {
            passive,
            adapter,
            client,
        };
        let space = ensure_space(&io, 0)?;
        step(4);
        let memory = STATE.lock().mint()?;
        (client, epoch, space, memory)
    };
    let io = Io {
        passive,
        adapter,
        client,
    };
    let pins = Vec::new();
    let mut huge = false;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        return Err(Error::NoMemory);
    }

    let (actual_size, mut pins) = if private.system_backed() {
        let (mut pins, pfns) = pin_system_memory(private.system_memory, private.size)?;
        if let Err(error) = alloc_system_descriptor(&io, memory, &pfns, private.size) {
            // A timeout may have left the descriptor live.  Its memory handle is
            // deliberately leaked with the pins until the transport sweep.
            if matches!(error, Error::Host) {
                preserve_ambiguous_pins(&mut pins);
            } else if io.rm_free(nd::H_DEVICE, memory).is_ok() {
                unlock_pins(&mut pins);
            } else {
                preserve_ambiguous_pins(&mut pins);
            }
            return Err(error);
        }
        (private.size, pins)
    } else {
        step(5);
        let params = local_params(client.root, private.size);
        // NvPerfMem 0x10: GPU-only memory of 2 MiB and more with 2-MiB pages, so
        // its 2-MiB-aligned mappings can use huge PTEs; a refusal falls back to
        // the big-page memory every other allocation gets.
        let reply = if np::wants_huge_memory(
            perf(),
            np::is_gpu_only_record(private_bytes),
            false,
            private.size,
        ) {
            let mut huge_params = params;
            let (attr, attr2) = np::huge_attrs(rc::SURFACE_ATTR, rc::SURFACE_ATTR2);
            huge_params[24..28].copy_from_slice(&attr.to_le_bytes());
            huge_params[28..32].copy_from_slice(&attr2.to_le_bytes());
            huge_params[72..80].copy_from_slice(&np::HUGE_PAGE.to_le_bytes());
            match io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &huge_params) {
                Ok(reply) => {
                    huge = true;
                    bump(&HUGE_MEMORY, b"NvPmHugeMem");
                    Ok(reply)
                }
                Err(_) => {
                    bump(&HUGE_REFUSED, b"NvPmHugeFail");
                    io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &params)
                }
            }
        } else {
            io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &params)
        };
        match reply {
            Ok(reply) => {
                let actual = rc::get64(&reply, 64).unwrap_or(private.size);
                if actual < private.size
                    || actual % nm::PAGE_BYTES != 0
                    || actual >= nm::GPU_ADDRESS_LIMIT
                {
                    let _ = io.rm_free(nd::H_DEVICE, memory);
                    return Err(Error::Host);
                }
                (actual, pins)
            }
            Err(error) => return Err(error),
        }
    };

    step(6);
    let writeback = match nm::PrivateWriteback::new(
        memory,
        client.root,
        nd::H_DEVICE,
        actual_size,
        private.kind,
        rc::SURFACE_ATTR,
        rc::SURFACE_ATTR2,
        space,
    ) {
        Some(writeback) => writeback,
        None => {
            abandon_memory(&io, memory, &mut pins);
            return Err(Error::Invalid);
        }
    };
    step(7);
    // D3D records keep their inputs (0x8c, 0x10c..0x120) and get the D3D field
    // set; the size in 64-KiB units is what the UMD reserves VA for.
    // The picture fields are inputs; read them before the writeback below sets
    // +0xd0 (after which the record no longer reads as a D3D one).
    let surface = nm::d3d_surface(private_bytes);
    let writeback = if nm::is_d3d_record(private_bytes) {
        let units = u32::try_from(private.size / nm::ALLOCATION_UNIT).unwrap_or(u32::MAX);
        match nm::d3d_writeback(private_bytes, units) {
            Some(fields) => Writeback::D3d(fields),
            None => Writeback::Native(writeback),
        }
    } else if nm::is_nvgl_record(private_bytes) {
        // Vulkan records: the 610.88 KMD's NVGL field set, not CUDA's handles.
        match nm::nvgl_writeback(private_bytes) {
            Some(fields) => Writeback::Nvgl(fields),
            None => Writeback::Native(writeback),
        }
    } else if nm::is_cuda_record(private_bytes) {
        // CUDA records: the 610.88 KMD's field set (no RM handles in the record).
        match nm::cuda_writeback(private_bytes) {
            Some(fields) => Writeback::Cuda(fields),
            None => Writeback::Native(writeback),
        }
    } else {
        Writeback::Native(writeback)
    };
    if writeback.apply(private_bytes).is_err() {
        abandon_memory(&io, memory, &mut pins);
        return Err(Error::Invalid);
    }
    let segment = if private.system_backed() {
        nm::SYSTEM_MEMORY_SEGMENT_ID
    } else {
        crate::ddi::gpummu::MEMORY_SEGMENT_ID
    };
    let allocation = Allocation {
        id,
        epoch,
        memory,
        size: actual_size,
        system_memory: private.system_memory,
        system_kernel_va: if pins.len() == 1 {
            pins[0].kernel_va
        } else {
            0
        },
        pins,
        mappings: Vec::new(),
        cpu: None,
        resident: None,
        poisoned: false,
        writeback: Some(writeback),
        surface,
        scanout_gem: 0,
        huge,
    };
    // Publish under the mutex. A transport sweep that ran meanwhile (`forget`)
    // closed this client with the object in it: the record is dropped and its pins
    // kept, as for any object whose host state is unknown.
    let mut unpublished = Some(allocation);
    let refused = match lock(passive) {
        Ok(_guard) => {
            let mut state = STATE.lock();
            if state.client.as_ref().map(|c| c.epoch) != Some(epoch) {
                Some((Error::NotReady, false))
            } else if state.allocations.try_reserve_exact(1).is_err() {
                Some((Error::NoMemory, true))
            } else {
                if let Some(allocation) = unpublished.take() {
                    // Sorted by id (`State::index_of`): ids are minted before the
                    // RM object is made, so a slower create publishes a lower id.
                    let at = state.allocations.partition_point(|a| a.id < allocation.id);
                    state.allocations.insert(at, allocation);
                }
                None
            }
        }
        Err(error) => Some((error, true)),
    };
    if let Some((error, free)) = refused {
        // Outside the mutex: the object was never published, nothing else names it.
        let mut pins = unpublished.map(|a| a.pins).unwrap_or_default();
        if free && io.rm_free(nd::H_DEVICE, memory).is_ok() {
            unlock_pins(&mut pins);
        } else {
            preserve_ambiguous_pins(&mut pins);
        }
        return Err(error);
    }
    super::nvda::note_memory(memory);
    Ok(Created {
        id,
        size: actual_size,
        segment,
        system_memory: private.system_memory,
    })
}

fn rm_error_code(error: rc::RmError) -> u32 {
    match error {
        rc::RmError::Status(status) => status,
        rc::RmError::Host(errno) => 0x8000_0000 | errno,
        rc::RmError::Malformed => 0xffff_ffff,
    }
}

fn map_fixed(
    io: &Io<'_>,
    memory: u32,
    allocation_size: u64,
    system_backed: bool,
    space: u32,
    virt: u32,
    piece: nm::MappingPiece,
) -> Result<u32, Error> {
    map_fixed_opts(io, memory, allocation_size, system_backed, space, virt, piece, false)
}

/// Whether the RM memory `memory` of a native allocation has 2-MiB pages. Takes
/// STATE: never call it with a STATE guard alive.
fn memory_is_huge(memory: u32) -> bool {
    STATE
        .lock()
        .allocations
        .iter()
        .any(|a| a.memory == memory && a.huge)
}

/// [`map_fixed`] with the RM page size the piece allows (NvPerfMem 0x08 big,
/// 0x10 huge pages for video memory; a large-page mapping RM refuses is made
/// again with 4-KiB pages) and, with `defer`, the TLB invalidate left to a later
/// operation of the same leaf update (NvPerfMem 0x20).
#[allow(clippy::too_many_arguments)]
fn map_fixed_opts(
    io: &Io<'_>,
    memory: u32,
    allocation_size: u64,
    system_backed: bool,
    space: u32,
    virt: u32,
    piece: nm::MappingPiece,
    defer: bool,
) -> Result<u32, Error> {
    let defer_flag = if defer { np::MAP_FLAGS_DEFER_TLB } else { 0 };
    if !system_backed {
        let perf = perf();
        let huge = perf.huge_pages() && memory_is_huge(memory);
        let mut page = np::map_page_size(piece, perf.big_pages(), huge);
        if LARGE_STREAK.load(Ordering::Relaxed) >= LARGE_STREAK_LIMIT {
            // RM refused every large-page mapping lately: each try costs a round trip.
            page = np::MAP_PAGE_4K;
        }
        // Huge, then big pages; RM refusing one (or the fixed virtual object)
        // steps down, and 4 KiB below is what every mapping was before.
        while page != np::MAP_PAGE_4K {
            let flags = np::with_page_size(LOCAL_4K_MAP_FLAGS, page) | defer_flag;
            match map_fixed_flags(io, memory, allocation_size, false, space, virt, piece, flags) {
                Ok(used) => {
                    LARGE_STREAK.store(0, Ordering::Relaxed);
                    if page == np::MAP_PAGE_HUGE {
                        bump(&MAPPED_HUGE, b"NvPmHuge");
                    } else {
                        bump(&MAPPED_BIG, b"NvPmBig");
                    }
                    if defer {
                        bump(&DEFERRED, b"NvPmDefer");
                    }
                    return Ok(used);
                }
                Err(Error::Host) => {
                    bump(&LARGE_REFUSED, b"NvPmLgFail");
                    LARGE_STREAK.fetch_add(1, Ordering::Relaxed);
                    page = if page == np::MAP_PAGE_HUGE && perf.big_pages() {
                        np::MAP_PAGE_BIG
                    } else {
                        np::MAP_PAGE_4K
                    };
                }
                Err(error) => return Err(error),
            }
        }
    }
    let base = if system_backed {
        cc::MAP_FLAGS_SYSMEM
    } else {
        LOCAL_4K_MAP_FLAGS
    };
    let flags = base | defer_flag;
    let used = map_fixed_flags(io, memory, allocation_size, system_backed, space, virt, piece, flags)?;
    if defer {
        bump(&DEFERRED, b"NvPmDefer");
    }
    Ok(used)
}

#[allow(clippy::too_many_arguments)]
fn map_fixed_flags(
    io: &Io<'_>,
    memory: u32,
    allocation_size: u64,
    system_backed: bool,
    space: u32,
    virt: u32,
    piece: nm::MappingPiece,
    flags: u32,
) -> Result<u32, Error> {
    nm::validate_piece(piece, allocation_size).map_err(|_| Error::Invalid)?;
    let end = piece.end().ok_or(Error::Invalid)?;
    if let Some(window) = window_for(space, piece.va, end) {
        return map_in_window(io, memory, window, flags, piece);
    }
    // System pages map with 4-KiB PTEs at VidMm's 4-KiB-aligned VAs: their virtual
    // object must be 4-KiB-paged, or RM aligns it down to the big page.
    let params = if system_backed {
        cc::virtual_params_4k(io.client.root, space, Some(piece.va), piece.len)
    } else {
        cc::virtual_params(io.client.root, space, Some(piece.va), piece.len)
    };
    io.rm_alloc(nd::H_DEVICE, virt, cc::NV50_MEMORY_VIRTUAL, &params)
        .inspect_err(|_| {
            // The refused fixed VA (4-KiB page number) and length in pages.
            crate::diag::record_named_bytes(b"NvMapVaPg", (piece.va >> 12) as u32);
            crate::diag::record_named_bytes(b"NvMapLenPg", (piece.len >> 12) as u32);
        })?;
    let block = nm::nvos46_range(
        io.client.root,
        nd::H_DEVICE,
        virt,
        memory,
        piece.memory_offset,
        piece.len,
        flags,
    );
    let mut response = [0u8; REPLY_MAX];
    let mapped = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut response,
    );
    let got = match mapped {
        Ok(n) => (|| {
            let reply = response.get(..n).ok_or(Error::Host)?;
            let reply = rc::rm_reply(reply, cc::NVOS46_STATUS_AT).map_err(|error| {
                crate::diag::record_named_bytes(b"NvRmMapSt", rm_error_code(error));
                Error::Host
            })?;
            cc::map_dma_va(reply.data).ok_or(Error::Host)
        })(),
        Err(error) => Err(error),
    };
    let got = match got {
        Ok(got) => got,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, virt);
            return Err(error);
        }
    };
    if got != piece.va {
        // RM must not silently relocate a WDDM mapping.  Tear down the address
        // RM returned, not the address we wished it had returned.
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, virt, memory, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        let _ = io.rm_free(nd::H_DEVICE, virt);
        note_wrong_address(piece.va, got);
        return Err(Error::WrongAddress);
    }
    Ok(virt)
}

/// `NvWrongVa` / `NvWrongGot`: the page numbers of the VA a fixed mapping asked for
/// and the one RM answered when they differ; `NvWrongN` how often.
fn note_wrong_address(want: u64, got: u64) {
    let n = WRONG_ADDRESS.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvWrongN", n);
    crate::diag::record_named_bytes(b"NvWrongVa", (want >> 12) as u32);
    crate::diag::record_named_bytes(b"NvWrongGot", (got >> 12) as u32);
}

static WRONG_ADDRESS: AtomicU32 = AtomicU32::new(0);
static FAILED_MAPS: AtomicU32 = AtomicU32::new(0);

/// `NvFail<k>` (k = 0..7, a ring): the 4-KiB page number of the first VA of a leaf
/// update that left its range unmapped, `NvFailE<k>` = why (low byte the
/// `Error`, next byte the site: 1 native vidmem, 2 system pages, 3/4/5 foreign
/// pages' eviction / OS descriptor / mapping).
fn note_failed_map(va: u64, site: u32, error: Error) {
    let n = FAILED_MAPS.fetch_add(1, Ordering::Relaxed);
    let k = (n % 8) as u8;
    let mut name = *b"NvFail0";
    name[6] = b'0' + k;
    crate::diag::record_named_bytes(&name, (va >> 12) as u32);
    let mut why = *b"NvFailE0";
    why[7] = b'0' + k;
    crate::diag::record_named_bytes(&why, (site << 8) | error as u32);
    crate::diag::record_named_bytes(b"NvFailN", n + 1);
}

/// `NVOS46_FLAGS_DMA_OFFSET_FIXED_TRUE`: for a virtual object, `dmaOffset` is
/// the absolute VA of the mapping inside it.
const MAP_FLAGS_DMA_OFFSET_FIXED: u32 = 1 << 15;

/// Map `piece` at its exact VA inside the space's low window (no virtual object
/// of its own). Returns the window handle, which the mapping records.
fn map_in_window(
    io: &Io<'_>,
    memory: u32,
    window: u32,
    flags: u32,
    piece: nm::MappingPiece,
) -> Result<u32, Error> {
    let mut block = nm::nvos46_range(
        io.client.root,
        nd::H_DEVICE,
        window,
        memory,
        piece.memory_offset,
        piece.len,
        flags | MAP_FLAGS_DMA_OFFSET_FIXED,
    );
    block[cc::NVOS46_DMA_OFFSET_AT..cc::NVOS46_DMA_OFFSET_AT + 8]
        .copy_from_slice(&piece.va.to_le_bytes());
    let mut response = [0u8; REPLY_MAX];
    let n = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut response,
    )?;
    let reply = response.get(..n).ok_or(Error::Host)?;
    let reply = rc::rm_reply(reply, cc::NVOS46_STATUS_AT).map_err(|error| {
        crate::diag::record_named_bytes(b"NvRmWinSt", rm_error_code(error));
        crate::diag::record_named_bytes(b"NvMapVaPg", (piece.va >> 12) as u32);
        Error::Host
    })?;
    let got = cc::map_dma_va(reply.data).ok_or(Error::Host)?;
    if got != piece.va {
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, window, memory, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        note_wrong_address(piece.va, got);
        return Err(Error::WrongAddress);
    }
    Ok(window)
}

fn unmap_fixed(io: &Io<'_>, memory: u32, mapping: HostMapping) -> Result<(), Error> {
    unmap_fixed_opts(io, memory, mapping, false)
}

/// [`unmap_fixed`]; with `defer` the TLB invalidate is left to a later operation
/// of the same leaf update (NvPerfMem 0x20).
fn unmap_fixed_opts(io: &Io<'_>, memory: u32, mapping: HostMapping, defer: bool) -> Result<(), Error> {
    watch::event(mapping.piece.va, mapping.piece.len, 10, 0, mapping.process);
    let memory = if mapping.desc != 0 { mapping.desc } else { memory };
    let mut block = nm::nvos47_unmap(
        io.client.root,
        nd::H_DEVICE,
        mapping.virt,
        memory,
        mapping.piece.va,
    );
    if defer {
        block[np::NVOS47_FLAGS_AT..np::NVOS47_FLAGS_AT + 4]
            .copy_from_slice(&np::UNMAP_FLAGS_DEFER_TLB.to_le_bytes());
        bump(&DEFERRED, b"NvPmDefer");
    }
    let mut response = [0u8; REPLY_MAX];
    let n = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
        &block,
        &[],
        &mut response,
    )?;
    rc::rm_reply(response.get(..n).ok_or(Error::Host)?, cc::NVOS47_STATUS_AT)
        .map_err(|_| Error::Host)?;
    // The space's low window outlives its mappings (`release_process`).
    if !is_window(mapping.virt) {
        io.rm_free(nd::H_DEVICE, mapping.virt)?;
    }
    if mapping.desc != 0 {
        // Freed unless another mapping shows it or it is kept idle (`NvPaging` 0x2).
        release_desc(io, mapping.desc)?;
    }
    Ok(())
}

/// An explicit TLB invalidate of `space` (`NV0080_CTRL_CMD_DMA_INVALIDATE_TLB`),
/// for a leaf update whose deferred operations were not followed by the one
/// that invalidates (it failed). Best effort, counted (`NvPmTlbInv`, refusals
/// `NvPmTlbErr`).
fn invalidate_tlb(io: &Io<'_>, space: u32) {
    let params = np::invalidate_tlb_params(space);
    let block = rc::nvos54(
        io.client.root,
        nd::H_DEVICE,
        np::CTRL_DMA_INVALIDATE_TLB,
        params.len() as u32,
    );
    let mut response = [0u8; REPLY_MAX];
    let sent = io.exchange(
        io.client.ctl,
        rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
        &block,
        &params,
        &mut response,
    );
    let ok = match sent {
        Ok(n) => response
            .get(..n)
            .is_some_and(|reply| rc::rm_reply(reply, rc::NVOS54_STATUS_AT).is_ok()),
        Err(_) => false,
    };
    bump(&TLB_RESCUE, b"NvPmTlbInv");
    if !ok {
        bump(&TLB_RESCUE_FAILED, b"NvPmTlbErr");
    }
}

// ── paging fast paths: shared/cached OS descriptors, batched RM calls ─────────────────────

/// Whether this update's RM calls go to the host as `RmBatch`es: `NvPaging` 0x1,
/// the host's `NVGPU_CFG_RM_BATCH`, and no whole-batch refusal from it so far.
fn batch_on(io: &Io<'_>) -> bool {
    paging().batch()
        && BATCH_REFUSED.load(Ordering::Relaxed) == 0
        && io
            .adapter
            .with_virtio(|v| v.nvrm_device_features())
            .unwrap_or(0)
            & helios_protocol::NVGPU_CFG_RM_BATCH
            != 0
}

/// Why a batch did not come back answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchFailure {
    /// Nothing ran (the host refused it whole, or it never left): use single calls.
    NotSent,
    /// Unknown what ran (a timeout, a reset): treat every entry as failed and
    /// keep whatever the host may still read.
    Indeterminate(Error),
}

/// Send `batch` and leave its checked reply in `resp`.
fn send_batch(io: &Io<'_>, batch: &rb::Batch, resp: &mut Vec<u8>) -> Result<(), BatchFailure> {
    resp.clear();
    resp.try_reserve_exact(batch.reply_len())
        .map_err(|_| BatchFailure::NotSent)?;
    resp.resize(batch.reply_len(), 0);
    ROUND_TRIPS.fetch_add(1, Ordering::Relaxed);
    let start = crate::adapter::foreign_scanout::now_100ns();
    let sent = nvrm::forward_kmd_batch(io.passive, io.adapter, batch.request(), resp, TIMEOUT_MS);
    let us = np::ticks_to_us(crate::adapter::foreign_scanout::now_100ns().saturating_sub(start));
    BATCH_US.fetch_add(us, Ordering::Relaxed);
    BATCH_MAX_US.fetch_max(us, Ordering::Relaxed);
    let n = match sent {
        Ok((n, epoch)) => {
            if epoch != io.client.epoch {
                BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
                return Err(BatchFailure::Indeterminate(Error::NotReady));
            }
            n
        }
        Err(Refusal::Transport(VirtioError::Timeout)) => {
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err(BatchFailure::Indeterminate(Error::Host));
        }
        Err(Refusal::Transport(VirtioError::DeviceError)) => {
            // The transport is gone: what reached the host is unknown.
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err(BatchFailure::Indeterminate(Error::Host));
        }
        Err(_) => {
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err(BatchFailure::NotSent);
        }
    };
    resp.truncate(n);
    match rb::check_reply(resp, batch.caps()) {
        Ok(_) => {
            BATCHES.fetch_add(1, Ordering::Relaxed);
            BATCH_CALLS.fetch_add(batch.len() as u32, Ordering::Relaxed);
            Ok(())
        }
        Err(rb::ReplyError::Refused(_)) => {
            // Nothing ran. A host that refuses one refuses them all: single calls
            // from now on (`PgBatchRef`).
            BATCH_REFUSED.fetch_add(1, Ordering::Relaxed);
            Err(BatchFailure::NotSent)
        }
        Err(_) => {
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            Err(BatchFailure::Indeterminate(Error::Host))
        }
    }
}

/// Whether entry `index` of a checked batch reply was served and RM said OK.
fn entry_ok(resp: &[u8], caps: &[u32], index: usize) -> bool {
    rb::entry_reply(resp, caps, index).is_some_and(|e| e.served && !e.failed)
}

/// One mapping of `pfns` (one 4-KiB PTE each) at `va` through an OS descriptor.
#[derive(Clone, Copy)]
struct DescJob<'a> {
    va: u64,
    pfns: &'a [u64],
    defer: bool,
}

/// A made descriptor mapping: (virtual object or window, descriptor); or where it
/// failed (2 the descriptor, 3 mapping it) and why.
type DescMade = Result<(u32, u32), (u32, Error)>;

/// An OS descriptor over exactly `pfns`: one already made (`PgDescHit`) or a new
/// handle to register (`.1` true).
fn take_desc(pfns: &[u64], owner: u32) -> Result<(u32, bool), Error> {
    let mut state = STATE.lock();
    if let Some((desc, was_idle)) = state.descs.acquire(pfns, owner) {
        DESC_HIT.fetch_add(1, Ordering::Relaxed);
        if was_idle {
            DESC_IDLE_HIT.fetch_add(1, Ordering::Relaxed);
        }
        return Ok((desc, false));
    }
    Ok((state.mint()?, true))
}

/// A descriptor taken by [`take_desc`] whose mapping was not made: a new one is
/// freed, a reused one gives its reference back.
fn give_back_desc(io: &Io<'_>, desc: u32, new: bool) {
    if new {
        let _ = io.rm_free(nd::H_DEVICE, desc);
        DESC_FREED.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = release_desc(io, desc);
    }
}

/// One mapping of `desc` went: free it if nothing else maps it and it is not
/// kept idle (`NvPaging` 0x2), and free the idle ones the bound pushes out.
fn release_desc(io: &Io<'_>, desc: u32) -> Result<(), Error> {
    let mut evicted = Vec::new();
    let released = STATE
        .lock()
        .descs
        .release(desc, desc_cache_on(), desc_cache_max(), &mut evicted);
    free_descs(io, &evicted, &DESC_EVICTED);
    match released {
        Released::Free => {
            DESC_FREED.fetch_add(1, Ordering::Relaxed);
            io.rm_free(nd::H_DEVICE, desc)
        }
        Released::Cached | Released::InUse => Ok(()),
    }
}

/// Free OS descriptors nothing maps (best effort; batched when on).
fn free_descs(io: &Io<'_>, descs: &[u32], counter: &AtomicU32) {
    if descs.is_empty() {
        return;
    }
    counter.fetch_add(descs.len() as u32, Ordering::Relaxed);
    if batch_on(io) && descs.len() > 1 {
        let mut batch = rb::Batch::new();
        let mut resp = Vec::new();
        let mut rest = descs;
        while !rest.is_empty() {
            batch.clear();
            let mut taken = 0;
            for desc in rest {
                let block = rc::nvos00(io.client.root, nd::H_DEVICE, *desc);
                let pushed = batch.push_ioctl(
                    io.client.ctl,
                    rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
                    &block,
                    &[],
                    None,
                    Some(rc::NVOS00_STATUS_AT),
                    0,
                    0,
                );
                if pushed.is_err() {
                    break;
                }
                taken += 1;
            }
            if taken == 0 || send_batch(io, &batch, &mut resp) == Err(BatchFailure::NotSent) {
                break;
            }
            rest = &rest[taken..];
        }
        if rest.is_empty() {
            return;
        }
        for desc in rest {
            let _ = io.rm_free(nd::H_DEVICE, *desc);
        }
        return;
    }
    for desc in descs {
        let _ = io.rm_free(nd::H_DEVICE, *desc);
    }
}

/// Drop the idle descriptors of allocation `id` (its system pages are no longer
/// known to be its own: it moved to video memory, or it is destroyed).
fn drop_idle_descs_of(io: &Io<'_>, id: u32) {
    let mut gone = Vec::new();
    STATE.lock().descs.drop_idle_of(id, &mut gone);
    free_descs(io, &gone, &DESC_EVICTED);
}

/// Make `jobs`' descriptor mappings in `space` for allocation `owner` (0: foreign
/// pages), one result per job in `out`, in order. Batched under `NvPaging` 0x1,
/// else one round trip per call; descriptors over the same pages are reused.
fn map_descs(io: &Io<'_>, owner: u32, space: u32, jobs: &[DescJob<'_>], out: &mut Vec<DescMade>) -> Result<(), Error> {
    out.clear();
    out.try_reserve_exact(jobs.len()).map_err(|_| Error::NoMemory)?;
    let mut done = 0;
    if batch_on(io) {
        done = map_descs_batched(io, owner, space, jobs, out);
    }
    for job in &jobs[done..] {
        out.push(map_desc_single(io, owner, space, job));
    }
    Ok(())
}

/// One descriptor mapping, a round trip per call (the path before `NvPaging`).
fn map_desc_single(io: &Io<'_>, owner: u32, space: u32, job: &DescJob<'_>) -> DescMade {
    let len = (job.pfns.len() as u64)
        .checked_mul(nm::PAGE_BYTES)
        .ok_or((2, Error::Invalid))?;
    let (desc, new) = take_desc(job.pfns, owner).map_err(|e| (2, e))?;
    if new {
        alloc_system_descriptor(io, desc, job.pfns, len).map_err(|e| (2, e))?;
        DESC_NEW.fetch_add(1, Ordering::Relaxed);
    }
    // Minted first: a STATE guard in a `match` scrutinee would live through the
    // arms, and `give_back_desc` takes STATE again.
    let minted = STATE.lock().mint();
    let virt = match minted {
        Ok(virt) => virt,
        Err(error) => {
            give_back_desc(io, desc, new);
            return Err((3, error));
        }
    };
    let shown = nm::MappingPiece {
        va: job.va,
        memory_offset: 0,
        len,
    };
    match map_fixed_opts(io, desc, len, true, space, virt, shown, job.defer) {
        Ok(used) => {
            if new {
                // Unrecorded (no memory): it is freed at its unmap, as before.
                let _ = STATE.lock().descs.insert(desc, job.pfns, owner);
            }
            Ok((used, desc))
        }
        Err(error) => {
            give_back_desc(io, desc, new);
            Err((3, error))
        }
    }
}

/// What one job of a batch planned.
struct Planned {
    desc: u32,
    new_desc: bool,
    /// The virtual object to make, 0 when the job maps into a window.
    virt: u32,
    window: u32,
    len: u64,
    at_desc: Option<usize>,
    at_virt: Option<usize>,
    at_map: usize,
    /// An indirect page-run table the host reads while serving the batch.
    big: Option<super::hal::DmaBuffer>,
}

/// [`map_descs`] in batches of up to 16 jobs: each job's registration (when its
/// pages have no descriptor yet), virtual object (outside the windows) and map,
/// one group that stops at its first failure. Returns how many jobs it answered
/// (pushed into `out`); the rest go the single-call way.
fn map_descs_batched(io: &Io<'_>, owner: u32, space: u32, jobs: &[DescJob<'_>], out: &mut Vec<DescMade>) -> usize {
    let mut batch = rb::Batch::new();
    let mut resp = Vec::new();
    let mut plans: Vec<Planned> = Vec::new();
    let mut done = 0;
    while done < jobs.len() {
        batch.clear();
        plans.clear();
        let before = done;
        // Plan as many jobs as fit.
        for (k, job) in jobs[done..].iter().enumerate() {
            if plans.try_reserve(1).is_err() {
                break;
            }
            match plan_desc_job(io, owner, space, job, k as u16, &mut batch) {
                Ok(plan) => plans.push(plan),
                Err(None) => break, // full: send what is planned
                Err(Some(error)) => {
                    if plans.is_empty() {
                        // This job cannot be planned (no memory, a refused table).
                        out.push(Err((2, error)));
                        done += 1;
                    }
                    break;
                }
            }
        }
        if plans.is_empty() {
            if done == before {
                // Not even one job fits a batch: the rest go one call at a time.
                return done;
            }
            continue;
        }
        match send_batch(io, &batch, &mut resp) {
            Ok(()) => {
                for (plan, job) in plans.drain(..).zip(&jobs[done..]) {
                    out.push(finish_desc_job(io, owner, space, job, plan, &resp, batch.caps()));
                    done += 1;
                }
            }
            Err(BatchFailure::NotSent) => {
                // Nothing ran: undo the plans, the rest go one call at a time.
                for plan in plans.drain(..) {
                    if !plan.new_desc {
                        let _ = release_desc(io, plan.desc);
                    }
                }
                return done;
            }
            Err(BatchFailure::Indeterminate(error)) => {
                // The host may hold any of it and may still read the tables:
                // keep them, leak the handles (never minted again).
                for plan in plans.drain(..) {
                    core::mem::forget(plan.big);
                    out.push(Err((if plan.new_desc { 2 } else { 3 }, error)));
                    done += 1;
                }
            }
        }
    }
    done
}

/// Plan one job into `batch`: `Err(None)` when it does not fit (nothing taken),
/// `Err(Some(_))` when it cannot be planned at all.
fn plan_desc_job(
    io: &Io<'_>,
    owner: u32,
    space: u32,
    job: &DescJob<'_>,
    group: u16,
    batch: &mut rb::Batch,
) -> Result<Planned, Option<Error>> {
    let len = (job.pfns.len() as u64)
        .checked_mul(nm::PAGE_BYTES)
        .ok_or(Some(Error::Invalid))?;
    let end = job.va.checked_add(len).ok_or(Some(Error::Invalid))?;
    let window = window_for(space, job.va, end).unwrap_or(0);
    let (desc, new_desc) = take_desc(job.pfns, owner).map_err(Some)?;
    let mark = batch.mark();
    // A job goes in whole or not at all: undo what was taken and added.
    let undo = |batch: &mut rb::Batch, error: Option<Error>| {
        batch.rollback(mark);
        if !new_desc {
            let _ = release_desc(io, desc);
        }
        error
    };
    let pushed = |e: rb::PushError| match e {
        rb::PushError::Full => None,
        rb::PushError::NoMemory => Some(Error::NoMemory),
        rb::PushError::Invalid => Some(Error::Invalid),
    };
    let flags = rb::SKIP_GROUP_ON_FAIL;
    let mut big = None;
    let at_desc = if new_desc {
        let (kind, deep, table) = match nvrm::page_run_table(io.passive, job.pfns) {
            Ok(t) => t,
            Err(_) => return Err(undo(batch, Some(Error::NoMemory))),
        };
        big = table;
        let block = cr::nvos02_osdesc(
            io.client.root,
            nd::H_DEVICE,
            desc,
            job.pfns.first().copied().unwrap_or(0).wrapping_mul(nm::PAGE_BYTES),
            len,
        );
        match batch.push_ioctl(
            io.client.gpu,
            rc::nv_cmd(cr::ESC_RM_ALLOC_MEMORY, cr::NVOS02_FD_BYTES as u32),
            &block,
            &[],
            Some((kind, &deep)),
            Some(cr::NVOS02_STATUS_AT),
            group,
            flags,
        ) {
            Ok(at) => Some(at),
            Err(e) => return Err(undo(batch, pushed(e))),
        }
    } else {
        None
    };
    let mut virt = 0;
    let at_virt = if window == 0 {
        let minted = STATE.lock().mint();
        virt = match minted {
            Ok(virt) => virt,
            Err(error) => return Err(undo(batch, Some(error))),
        };
        let params = cc::virtual_params_4k(io.client.root, space, Some(job.va), len);
        let block = rc::nvos64(
            io.client.root,
            nd::H_DEVICE,
            virt,
            cc::NV50_MEMORY_VIRTUAL,
            params.len() as u32,
        );
        match batch.push_ioctl(
            io.client.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            &params,
            None,
            Some(rc::NVOS64_STATUS_AT),
            group,
            flags,
        ) {
            Ok(at) => Some(at),
            Err(e) => return Err(undo(batch, pushed(e))),
        }
    } else {
        None
    };
    let defer_flag = if job.defer { np::MAP_FLAGS_DEFER_TLB } else { 0 };
    let mut map_flags = cc::MAP_FLAGS_SYSMEM | defer_flag;
    let target = if window != 0 {
        map_flags |= MAP_FLAGS_DMA_OFFSET_FIXED;
        window
    } else {
        virt
    };
    let mut block = nm::nvos46_range(io.client.root, nd::H_DEVICE, target, desc, 0, len, map_flags);
    if window != 0 {
        block[cc::NVOS46_DMA_OFFSET_AT..cc::NVOS46_DMA_OFFSET_AT + 8]
            .copy_from_slice(&job.va.to_le_bytes());
    }
    let at_map = match batch.push_ioctl(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        None,
        Some(cc::NVOS46_STATUS_AT),
        group,
        flags,
    ) {
        Ok(at) => at,
        Err(e) => return Err(undo(batch, pushed(e))),
    };
    Ok(Planned {
        desc,
        new_desc,
        virt,
        window,
        len,
        at_desc,
        at_virt,
        at_map,
        big,
    })
}

/// Read one job's entries of an answered batch; undo what was made of a failed one.
fn finish_desc_job(
    io: &Io<'_>,
    owner: u32,
    space: u32,
    job: &DescJob<'_>,
    plan: Planned,
    resp: &[u8],
    caps: &[u32],
) -> DescMade {
    let _ = space;
    drop(plan.big);
    let desc_ok = plan.at_desc.is_none_or(|i| entry_ok(resp, caps, i));
    let virt_ok = plan.at_virt.is_none_or(|i| entry_ok(resp, caps, i));
    let map_reply = rb::entry_reply(resp, caps, plan.at_map);
    let mapped_at = map_reply
        .filter(|e| e.served && !e.failed)
        .and_then(|e| rc::parse_ioctl_reply(e.bytes).ok())
        .and_then(|reply| cc::map_dma_va(reply.data));
    if desc_ok && plan.at_desc.is_some() {
        DESC_NEW.fetch_add(1, Ordering::Relaxed);
    }
    if desc_ok && virt_ok && mapped_at == Some(job.va) {
        if job.defer {
            bump(&DEFERRED, b"NvPmDefer");
        }
        if plan.new_desc {
            let _ = STATE.lock().descs.insert(plan.desc, job.pfns, owner);
        }
        return Ok((if plan.window != 0 { plan.window } else { plan.virt }, plan.desc));
    }
    // Failed: say where, and take down what was made.
    if !desc_ok {
        if let Some(e) = plan.at_desc.and_then(|i| rb::entry_reply(resp, caps, i)) {
            // NvSysHost: RM's NV_STATUS, 0x1000_0000 for a host refusal.
            let code = e.rm_status.unwrap_or(0x1000_0000);
            crate::diag::record_named_bytes(b"NvSysHost", code);
            crate::diag::record_named_bytes(b"NvSysPages", job.pfns.len() as u32);
        }
        // The rest of the group was skipped: nothing else exists.
        return Err((2, Error::Host));
    }
    let error = if let Some(got) = mapped_at {
        // RM must not relocate a WDDM mapping: take down what it made.
        let target = if plan.window != 0 { plan.window } else { plan.virt };
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, target, plan.desc, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        note_wrong_address(job.va, got);
        Error::WrongAddress
    } else {
        if let Some(e) = map_reply.filter(|e| e.served) {
            crate::diag::record_named_bytes(
                if plan.window != 0 { b"NvRmWinSt" } else { b"NvRmMapSt" },
                e.rm_status.unwrap_or(0xffff_ffff),
            );
        }
        if !virt_ok {
            crate::diag::record_named_bytes(b"NvMapVaPg", (job.va >> 12) as u32);
            crate::diag::record_named_bytes(b"NvMapLenPg", (plan.len >> 12) as u32);
        }
        Error::Host
    };
    if plan.virt != 0 && virt_ok {
        let _ = io.rm_free(nd::H_DEVICE, plan.virt);
    }
    give_back_desc(io, plan.desc, plan.new_desc);
    Err((3, error))
}

/// Unmap each `(memory, mapping, defer)` (the descriptor of a descriptor mapping
/// given back, [`release_desc`]): one result per item in `out`. Batched under
/// `NvPaging` 0x1; with `abort` the first failure stops the rest (as a loop of
/// [`unmap_fixed_opts`] that returns at its first error does).
fn unmap_many(io: &Io<'_>, items: &[(u32, HostMapping, bool)], abort: bool, out: &mut Vec<Result<(), Error>>) -> Result<(), Error> {
    out.clear();
    out.try_reserve_exact(items.len()).map_err(|_| Error::NoMemory)?;
    let mut done = 0;
    if batch_on(io) && !items.is_empty() {
        done = unmap_many_batched(io, items, abort, out);
    }
    for (memory, mapping, defer) in &items[done..] {
        if abort && out.iter().any(|r| r.is_err()) {
            out.push(Err(Error::Host));
            continue;
        }
        out.push(unmap_fixed_opts(io, *memory, *mapping, *defer));
    }
    Ok(())
}

fn unmap_many_batched(io: &Io<'_>, items: &[(u32, HostMapping, bool)], abort: bool, out: &mut Vec<Result<(), Error>>) -> usize {
    struct Plan {
        at_unmap: usize,
        at_virt: Option<usize>,
        at_desc: Option<usize>,
    }
    let mut batch = rb::Batch::new();
    let mut resp = Vec::new();
    let mut plans: Vec<Plan> = Vec::new();
    let mut done = 0;
    let cache = desc_cache_on();
    let max_idle = desc_cache_max();
    while done < items.len() {
        if abort && out.iter().any(|r| r.is_err()) {
            return done;
        }
        batch.clear();
        plans.clear();
        for (k, (memory, m, defer)) in items[done..].iter().enumerate() {
            if plans.try_reserve(1).is_err() {
                break;
            }
            let mark = batch.mark();
            let group = k as u16;
            let mut flags = rb::SKIP_GROUP_ON_FAIL;
            if abort {
                flags |= rb::ABORT_ON_FAIL;
            }
            let target = if m.desc != 0 { m.desc } else { *memory };
            let mut block = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, m.virt, target, m.piece.va);
            if *defer {
                block[np::NVOS47_FLAGS_AT..np::NVOS47_FLAGS_AT + 4]
                    .copy_from_slice(&np::UNMAP_FLAGS_DEFER_TLB.to_le_bytes());
            }
            let Ok(at_unmap) = batch.push_ioctl(
                io.client.ctl,
                rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
                &block,
                &[],
                None,
                Some(cc::NVOS47_STATUS_AT),
                group,
                flags,
            ) else {
                break;
            };
            // `Err(())`: it did not fit; the item goes whole into the next batch.
            let free = |batch: &mut rb::Batch, object: u32| -> Result<Option<usize>, ()> {
                let block = rc::nvos00(io.client.root, nd::H_DEVICE, object);
                batch
                    .push_ioctl(
                        io.client.ctl,
                        rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
                        &block,
                        &[],
                        None,
                        Some(rc::NVOS00_STATUS_AT),
                        group,
                        rb::SKIP_GROUP_ON_FAIL,
                    )
                    .map(Some)
                    .map_err(|_| ())
            };
            let at_virt = if is_window(m.virt) { Ok(None) } else { free(&mut batch, m.virt) };
            let at_desc = if m.desc != 0 && STATE.lock().descs.would_free(m.desc, cache, max_idle) {
                free(&mut batch, m.desc)
            } else {
                Ok(None)
            };
            let (Ok(at_virt), Ok(at_desc)) = (at_virt, at_desc) else {
                batch.rollback(mark);
                break;
            };
            plans.push(Plan { at_unmap, at_virt, at_desc });
        }
        if plans.is_empty() {
            return done;
        }
        match send_batch(io, &batch, &mut resp) {
            Ok(()) => {}
            Err(BatchFailure::NotSent) => return done,
            Err(BatchFailure::Indeterminate(error)) => {
                for _ in plans.drain(..) {
                    out.push(Err(error));
                    done += 1;
                }
                continue;
            }
        }
        let caps = batch.caps();
        for (plan, (_, m, _)) in plans.drain(..).zip(&items[done..]) {
            done += 1;
            if !entry_ok(&resp, caps, plan.at_unmap) {
                out.push(Err(Error::Host));
                continue;
            }
            watch::event(m.piece.va, m.piece.len, 10, 0, m.process);
            let virt_ok = plan.at_virt.is_none_or(|i| entry_ok(&resp, caps, i));
            let mut result = if virt_ok { Ok(()) } else { Err(Error::Host) };
            if m.desc != 0 {
                match plan.at_desc {
                    Some(i) if entry_ok(&resp, caps, i) => {
                        STATE.lock().descs.forget(m.desc);
                        DESC_FREED.fetch_add(1, Ordering::Relaxed);
                    }
                    // Not freed in the batch (kept, or its free was skipped).
                    _ => {
                        if let Err(error) = release_desc(io, m.desc) {
                            result = Err(error);
                        }
                    }
                }
            }
            out.push(result);
        }
    }
    done
}

/// Write the record values CreateAllocation produced for native allocation
/// `id` into `bytes` (an NVDA record dxgkrnl passes to OpenAllocation). The
/// UMD reads sizes and handles from it: without them the D3D12 UMD reserved a
/// zero-size VA range (INVALID_PARAMETER) and removed its device.
/// What `native_scanout` needs to show an allocation: its RM memory, size,
/// 2D picture and the GEM handle it was given before (0 = none yet).
#[derive(Clone, Copy, Debug)]
pub struct ScanoutSource {
    pub memory: u32,
    pub size: u64,
    pub surface: nm::D3dSurface,
    pub gem: u32,
    pub system_backed: bool,
}

pub fn scanout_source(id: u32) -> Result<ScanoutSource, Error> {
    let state = STATE.lock();
    let a = state.allocation(id)?;
    let surface = a.surface.ok_or(Error::Unsupported)?;
    Ok(ScanoutSource {
        memory: a.memory,
        size: a.size,
        surface,
        gem: a.scanout_gem,
        system_backed: a.system_memory != 0,
    })
}

/// The GEM handle and client epoch of a primary already imported for scanout on
/// the live client (`None`: not imported, poisoned, another epoch, no client).
/// STATE spinlock only, no I/O mutex: `native_scanout`'s flip fast path
/// (`NvHotPath` 0x1).
pub fn scanout_imported(id: u32) -> Option<(u32, u64)> {
    let state = STATE.lock();
    let client = state.client.as_ref()?;
    let a = state.allocation(id).ok()?;
    if a.scanout_gem == 0 || a.poisoned || a.epoch != client.epoch || a.surface.is_none() {
        return None;
    }
    Some((a.scanout_gem, client.epoch))
}

/// The width and height of an allocation's 2D picture (DescribeAllocation).
pub fn surface_extent(id: u32) -> Option<(u32, u32)> {
    let state = STATE.lock();
    let s = state.allocation(id).ok()?.surface?;
    Some((s.width, s.height))
}

pub fn set_scanout_gem(id: u32, gem: u32) -> Result<(), Error> {
    STATE.lock().allocation_mut(id)?.scanout_gem = gem;
    Ok(())
}

/// Run `f` with the native-memory I/O (the PASSIVE mutex held, the KMD client).
pub(crate) fn with_io<R>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    f: impl FnOnce(&Io<'_>) -> Result<R, Error>,
) -> Result<R, Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    f(&io)
}

pub fn writeback_open(id: u32, bytes: &mut [u8]) -> Result<(), Error> {
    let writeback = STATE.lock().allocation(id)?.writeback.ok_or(Error::Invalid)?;
    writeback.apply(bytes).map_err(|_| Error::Invalid)
}

/// How many mappings `evict_range` dropped (NvEvict) and how many of their
/// outside remainders could not be shown again (NvEvictFail).
static EVICTED: AtomicU32 = AtomicU32::new(0);
static EVICT_FAILED: AtomicU32 = AtomicU32::new(0);

/// Drop every mapping of `process` in `[first_va, first_va + len)` that VidMm's
/// page table no longer shows there: foreign ranges and other allocations
/// (`keep` excepted). The parts of a dropped mapping outside the range are
/// mapped again (RM unmaps whole objects); one that cannot be stays unmapped,
/// as if VidMm had not written it yet. Callers hold the RM lock.
fn evict_range(
    io: &Io<'_>,
    space: u32,
    process: usize,
    first_va: u64,
    len: u64,
    keep: Option<u32>,
) -> Result<(), Error> {
    // NvEvictOff=1 (test knob): leave other mappings alone, as before eviction.
    static EVICT_OFF: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut off = EVICT_OFF.load(Ordering::Relaxed);
    if off == u32::MAX {
        off = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvEvictOff"), 0);
        EVICT_OFF.store(off, Ordering::Relaxed);
    }
    if off == 1 {
        return Ok(());
    }
    let end = first_va.checked_add(len).ok_or(Error::Invalid)?;
    let hit = |m: &HostMapping| {
        m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
    };
    struct Victim {
        id: u32,
        memory: u32,
        size: u64,
        system_backed: bool,
        mapping: HostMapping,
        pfns: Option<Vec<u64>>,
    }
    let mut candidates = Vec::new();
    let (foreign, victims) = {
        let mut state = STATE.lock();
        // Only this process's allocations can map here (`by_process`).
        state.candidates(process, &mut candidates)?;
        let mut foreign = Vec::new();
        if state.foreign.iter().any(|m| hit(m)) {
            foreign.try_reserve(state.foreign.len()).map_err(|_| Error::NoMemory)?;
            foreign.extend(state.foreign.iter().copied().filter(|m| hit(m)));
            state.foreign.retain(|m| !hit(m));
        }
        let mut victims = Vec::new();
        let mut emptied = false;
        for &id in candidates.iter().filter(|id| Some(**id) != keep) {
            let Some(index) = state.index_of(id) else {
                emptied = true;
                continue;
            };
            let allocation = &state.allocations[index];
            let (memory, size, system_backed) =
                (allocation.memory, allocation.size, allocation.system_memory != 0);
            if !allocation.mappings.iter().any(|m| hit(m)) {
                emptied |= !allocation.mappings.iter().any(|m| m.process == process);
                continue;
            }
            for mapping in allocation.mappings.iter().copied().filter(|m| hit(m)) {
                let pfns = if mapping.desc != 0 {
                    state.descs.pfns(mapping.desc).and_then(|pfns| {
                        let mut copy = Vec::new();
                        copy.try_reserve_exact(pfns.len()).ok()?;
                        copy.extend_from_slice(pfns);
                        Some(copy)
                    })
                } else {
                    None
                };
                victims.try_reserve(1).map_err(|_| Error::NoMemory)?;
                victims.push(Victim { id, memory, size, system_backed, mapping, pfns });
            }
            state.allocations[index].mappings.retain(|m| !hit(m));
        }
        if emptied {
            state.prune_index(process);
        }
        (foreign, victims)
    };
    if foreign.is_empty() && victims.is_empty() {
        return Ok(());
    }
    // `NvEvict`: published by the mirror pass (`publish_hot`).
    EVICTED.fetch_add((foreign.len() + victims.len()) as u32, Ordering::Relaxed);
    let failed = || {
        let n = EVICT_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvEvictFail", n);
    };
    // Every unmap in as few round trips as the batch allows (`NvPaging` 0x1).
    let mut items = Vec::new();
    items
        .try_reserve_exact(foreign.len() + victims.len())
        .map_err(|_| Error::NoMemory)?;
    items.extend(foreign.iter().map(|m| (0u32, *m, false)));
    items.extend(victims.iter().map(|v| (v.memory, v.mapping, false)));
    let mut results = Vec::new();
    unmap_many(io, &items, false, &mut results)?;
    for (victim, unmapped) in victims.into_iter().zip(results.into_iter().skip(foreign.len())) {
        let m = victim.mapping;
        if unmapped.is_err() {
            if let Ok(allocation) = STATE.lock().allocation_mut(victim.id) {
                allocation.poisoned = true;
            }
            failed();
            continue;
        }
        let Some(m_end) = m.piece.end() else {
            continue;
        };
        let mut rest = [None, None];
        if m.piece.va < first_va {
            rest[0] = Some((m.piece.va, first_va - m.piece.va));
        }
        if m_end > end {
            rest[1] = Some((end, m_end - end));
        }
        for (va, bytes) in rest.into_iter().flatten() {
            let delta = va - m.piece.va;
            let piece = nm::MappingPiece {
                va,
                memory_offset: m.piece.memory_offset + delta,
                len: bytes,
            };
            let shown = (|| -> Result<HostMapping, Error> {
                if m.desc == 0 {
                    let virt = STATE.lock().mint()?;
                    let used = map_fixed(io, victim.memory, victim.size, victim.system_backed, space, virt, piece)?;
                    return Ok(HostMapping { process, piece, virt: used, desc: 0 });
                }
                let pfns = victim.pfns.as_ref().ok_or(Error::Invalid)?;
                let first = (delta / nm::PAGE_BYTES) as usize;
                let count = (bytes / nm::PAGE_BYTES) as usize;
                let sub = pfns.get(first..first + count).ok_or(Error::Invalid)?;
                let job = DescJob { va, pfns: sub, defer: false };
                let (used, desc) = map_desc_single(io, victim.id, space, &job).map_err(|(_, e)| e)?;
                Ok(HostMapping { process, piece, virt: used, desc })
            })();
            let Ok(shown) = shown else {
                failed();
                continue;
            };
            let recorded = {
                let mut state = STATE.lock();
                let recorded = match state.allocation_mut(victim.id) {
                    Ok(allocation) => {
                        if allocation.mappings.try_reserve(1).is_ok() {
                            allocation.mappings.push(shown);
                            allocation
                                .mappings
                                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
                            true
                        } else {
                            false
                        }
                    }
                    Err(_) => false,
                };
                if recorded {
                    state.note_mapped(process, victim.id);
                }
                recorded
            };
            if !recorded {
                let _ = unmap_fixed(io, victim.memory, shown);
                failed();
            }
        }
    }
    Ok(())
}

/// A leaf update whose PTEs place a device-local native allocation in SYSTEM
/// pages. VidMm keeps CPU-accessed allocations there (the UMD's Lock2 view is
/// those pages), so the GPU must see the same pages, not the allocation's RM
/// video memory: GPFIFO entries and pushbuffers the UMD wrote were otherwise
/// invisible to the GPU. Whatever the process mapped in the range is dropped,
/// the pages (`pfns`, one per 4-KiB PTE) become an RM OS-descriptor object and
/// that is mapped at the exact VA. VidMm's Transfer operations move contents
/// between these pages and the RM memory when it changes the placement.
pub fn update_leaf_system(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
) -> Result<(), Error> {
    if pfns.is_empty() {
        return Err(Error::Invalid);
    }
    let mut none = Vec::new();
    none.try_reserve_exact(pfns.len()).map_err(|_| Error::NoMemory)?;
    none.resize(pfns.len(), None);
    // NvSysWhy: 1 dropping the old mappings (an error name in NvSysErr), 2 the
    // OS descriptor, 3 mapping it.
    let why = |code: u32, error: Error| {
        crate::diag::record_named_bytes(b"NvSysWhy", code);
        crate::diag::record_named_bytes(b"NvSysErr", error as u32);
        error
    };
    let start = crate::adapter::foreign_scanout::now_100ns();
    let trips = round_trips();
    let chunk_pages = system_chunk_pages();
    let perf = perf();
    // NvPerfMem 0x40: VidMm rewrites a range with the pages that are mapped there
    // already (a re-residency, a page-table rebuild): nothing to unmap or map.
    if perf.skip_unchanged()
        && system_mapping_unchanged(id, process, first_va, allocation_offset, pfns, chunk_pages)
    {
        bump(&SKIPPED_UNCHANGED, b"NvPmSkip");
        return Ok(());
    }
    let result = update_leaf_system_inner(
        passive,
        adapter,
        id,
        process,
        first_va,
        allocation_offset,
        pfns,
        &none,
        chunk_pages,
        perf.tlb_defer(),
        &why,
    );
    SYSTEM_COST.note(start, trips);
    result
}

/// Whether the allocation's mappings in `process` over the update's range are
/// exactly the system-page chunks the update would make (`native_perf`).
fn system_mapping_unchanged(
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
    chunk_pages: usize,
) -> bool {
    let state = STATE.lock();
    let Ok(allocation) = state.allocation(id) else {
        return false;
    };
    if allocation.poisoned {
        return false;
    }
    let mut live = Vec::new();
    if live.try_reserve(allocation.mappings.len()).is_err() {
        return false;
    }
    for m in allocation.mappings.iter().filter(|m| m.process == process) {
        let shown = if m.desc != 0 {
            match state.descs.pfns(m.desc) {
                Some(pages) => Some(pages),
                // A descriptor whose pages were not kept: cannot tell.
                None => return false,
            }
        } else {
            None
        };
        live.push(np::LiveMapping { piece: m.piece, pfns: shown });
    }
    np::system_update_unchanged(&live, first_va, allocation_offset, pfns, chunk_pages)
}

#[allow(clippy::too_many_arguments)]
fn update_leaf_system_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
    none: &[Option<nm::MappingPiece>],
    chunk_pages: usize,
    defer_on: bool,
    why: &dyn Fn(u32, Error) -> Error,
) -> Result<(), Error> {
    update_leaf_staged(passive, adapter, id, process, first_va, allocation_offset, none)
        .map_err(|error| why(1, error))?;

    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let space = ensure_space(&io, process)?;
    // One OS descriptor per chunk (`NvSysChunk`, default 128 pages): RM refused
    // large ones (512 and 335 pages) with NV_ERR_INVALID_ADDRESS after pinning
    // them (bpftrace: every pin succeeded), and the range stayed unmapped (ring
    // fetch faults). A refused chunk still fails the update; the chunks mapped
    // before it stay. With NvPerfMem 0x20 every chunk's mapping but the last
    // defers its TLB invalidate. Pages that already have a descriptor reuse it,
    // and with `NvPaging` 0x1 every chunk's calls go in one round trip.
    let chunks = np::system_chunks(first_va, allocation_offset, pfns.len(), chunk_pages);
    let mut jobs = Vec::new();
    jobs.try_reserve_exact(chunks.len()).map_err(|_| Error::NoMemory)?;
    for (index, chunk) in chunks.iter().enumerate() {
        jobs.push(DescJob {
            va: chunk.va,
            pfns: pfns
                .get(chunk.first..chunk.first + chunk.pages)
                .ok_or(Error::Invalid)?,
            defer: np::defer_tlb(defer_on, index, chunks.len()),
        });
    }
    let mut made = Vec::new();
    map_descs(&io, id, space, &jobs, &mut made)?;
    let mut first_error = None;
    let mut deferred = false;
    let mut last_ok = false;
    for ((chunk, job), result) in chunks.iter().zip(&jobs).zip(made) {
        let recorded = result.map_err(|(stage, error)| why(stage, error)).and_then(|(virt, desc)| {
            let mapping = HostMapping {
                process,
                piece: nm::MappingPiece {
                    va: chunk.va,
                    memory_offset: chunk.memory_offset,
                    len: chunk.bytes(),
                },
                virt,
                desc,
            };
            let mut state = STATE.lock();
            let pushed = match state.allocation_mut(id) {
                Ok(allocation) => {
                    if allocation.mappings.try_reserve(1).is_ok() {
                        allocation.mappings.push(mapping);
                        allocation
                            .mappings
                            .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
                        true
                    } else {
                        false
                    }
                }
                Err(_) => false,
            };
            if pushed {
                state.note_mapped(process, id);
                return Ok(());
            }
            drop(state);
            let _ = unmap_fixed(&io, 0, mapping);
            Err(Error::NoMemory)
        });
        last_ok = recorded.is_ok();
        match recorded {
            Err(error) => {
                note_failed_map(chunk.va, 2, error);
                watch::event(chunk.va, chunk.bytes(), 8, error as u32, process);
                first_error.get_or_insert(error);
            }
            Ok(()) => {
                deferred |= job.defer;
                watch::event(chunk.va, chunk.bytes(), 7, 0, process);
            }
        }
    }
    if deferred && !last_ok {
        // The last chunk would have invalidated for the deferred ones.
        invalidate_tlb(&io, space);
    }
    first_error.map_or(Ok(()), Err)
}

/// A leaf update of an allocation this driver has no RM memory for (dxgkrnl's
/// monitored-fence and progress-fence storage, any non-native allocation) in
/// a process with a native VA space. The GPU signals queue progress and
/// monitored fences by writing those VAs (0x10140...), so they must be mapped
/// in RM's space too: system pages (`pfns`, one per PTE) become an OS
/// descriptor mapped at the exact VA; `pfns` empty only drops what the
/// process mapped in [first_va, first_va + pages * 4 KiB).
pub fn update_foreign_leaf(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    first_va: u64,
    pages: u64,
    pfns: &[u64],
) -> Result<(), Error> {
    // dxgkrnl maps a process's monitored-fence and progress-fence pages before
    // the process makes any native allocation, often before anything brought
    // the KMD's RM client up: bring it up here, or those pages were never
    // mirrored (NvFgnFail with NotReady) and the GPU's fence writes faulted
    // (Xid 31, CE FAULT_PTE writes at the fence VAs).
    let up = STATE.lock().client.is_some();
    if !up && !pfns.is_empty() {
        super::nvda::memory_client(passive, adapter, process)?;
    }
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let len = pages.checked_mul(nm::PAGE_BYTES).ok_or(Error::Invalid)?;
    // `NvHotPath` 0x2: VidMm writes the pages that are mapped there already (a
    // re-residency, a page-table rebuild): nothing to unmap or map (`HpFgnSkip`).
    // A live foreign mapping means nothing else was mapped over it since: every
    // native update evicts the foreign mappings in its range.
    if crate::ddi::hot_path::knob(passive).skip_foreign_unchanged()
        && foreign_mapping_unchanged(process, first_va, len, pfns)
    {
        crate::ddi::hot_path::count(&crate::ddi::hot_path::FOREIGN_SKIPPED);
        watch::event(first_va, len, 2, 0x80, process);
        return Ok(());
    }
    let stale: Vec<HostMapping> = {
        let hit = |m: &HostMapping| {
            m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
        };
        let mut state = STATE.lock();
        let mut stale = Vec::new();
        // Only the overlapping ones are copied out; the list is not rebuilt.
        let n = state.foreign.iter().filter(|m| hit(m)).count();
        if n != 0 {
            stale.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
            stale.extend(state.foreign.iter().copied().filter(|m| hit(m)));
            state.foreign.retain(|m| !hit(m));
        }
        stale
    };
    if !stale.is_empty() {
        let mut items = Vec::new();
        items.try_reserve_exact(stale.len()).map_err(|_| Error::NoMemory)?;
        items.extend(stale.iter().map(|m| (0u32, *m, false)));
        let mut results = Vec::new();
        unmap_many(&io, &items, false, &mut results)?;
    }
    if pfns.is_empty() {
        watch::event(first_va, len, 4, 0, process);
        return Ok(());
    }
    let space = ensure_space(&io, process)?;
    // Native allocations VidMm no longer shows here (see `update_leaf`).
    evict_range(&io, space, process, first_va, len, None)
        .inspect_err(|e| note_failed_map(first_va, 3, *e))?;
    let piece = nm::MappingPiece {
        va: first_va,
        memory_offset: 0,
        len,
    };
    // The descriptor (reused for the same pages), its virtual object and the map:
    // one round trip under `NvPaging` 0x1.
    let job = DescJob {
        va: first_va,
        pfns,
        defer: false,
    };
    let mut made = Vec::new();
    map_descs(&io, 0, space, core::slice::from_ref(&job), &mut made)?;
    let (used, desc) = match made.pop().unwrap_or(Err((3, Error::Host))) {
        Ok(made) => made,
        Err((stage, error)) => {
            note_failed_map(first_va, if stage == 2 { 4 } else { 5 }, error);
            if stage != 2 {
                watch::event(first_va, len, 3, error as u32, process);
            }
            return Err(error);
        }
    };
    watch::event(first_va, len, 2, 0, process);
    let mapping = HostMapping {
        process,
        piece,
        virt: used,
        desc,
    };
    let mut state = STATE.lock();
    if state.foreign.try_reserve(1).is_err() {
        drop(state);
        let _ = unmap_fixed(&io, 0, mapping);
        return Err(Error::NoMemory);
    }
    state.foreign.push(mapping);
    Ok(())
}

/// Whether `process`'s foreign mappings over `[first_va, first_va + len)` are exactly
/// one mapping of exactly that range whose descriptor holds exactly `pfns`
/// (`helios_kmd_logic::hot_path::foreign_update_unchanged`).
fn foreign_mapping_unchanged(process: usize, first_va: u64, len: u64, pfns: &[u64]) -> bool {
    let state = STATE.lock();
    let mut overlapping: [(nm::MappingPiece, Option<&[u64]>); 2] = [(
        nm::MappingPiece {
            va: 0,
            memory_offset: 0,
            len: 0,
        },
        None,
    ); 2];
    let mut n = 0usize;
    for m in state
        .foreign
        .iter()
        .filter(|m| m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len))
    {
        if n == overlapping.len() {
            return false;
        }
        let pages = if m.desc != 0 { state.descs.pfns(m.desc) } else { None };
        overlapping[n] = (m.piece, pages);
        n += 1;
    }
    helios_kmd_logic::hot_path::foreign_update_unchanged(&overlapping[..n], first_va, len, pfns)
}

/// CPU access to `len` bytes at GPU VA `va` of `process` exactly as the GPU
/// sees them: VidMm's system pages for a system-placed native allocation, the
/// RM memory otherwise. `visit` gets a temporary pointer; the range must not
/// cross a 4-KiB page. IRQL <= DISPATCH; the RM-memory case needs `passive`
/// (`Err(Busy)` without). `Ok(false)` when nothing native maps `va`.
pub fn patch_gpu_bytes(
    passive: Option<PassiveLevel>,
    adapter: &AdapterContext,
    process: usize,
    va: u64,
    len: u64,
    visit: impl FnOnce(*mut u8),
) -> Result<bool, Error> {
    if len == 0 || (va & 0xfff) + len > 0x1000 {
        return Err(Error::Invalid);
    }
    enum Where {
        System(u64),
        Rm(u32, u64),
    }
    let found = {
        let state = STATE.lock();
        let look = |allocation: &Allocation| -> Option<Where> {
            let m = allocation.mappings.iter().find(|m| {
                m.process == process
                    && va >= m.piece.va
                    && m.piece.end().is_some_and(|end| va + len <= end)
            })?;
            let delta = va - m.piece.va;
            if m.desc != 0 {
                let pfn = state
                    .descs
                    .pfns(m.desc)
                    .and_then(|pfns| pfns.get((delta >> 12) as usize).copied())?;
                Some(Where::System((pfn << 12) | (va & 0xfff)))
            } else {
                Some(Where::Rm(allocation.id, m.piece.memory_offset + delta))
            }
        };
        // This process's allocations only (`by_process`); no allocation here (any
        // IRQL up to DISPATCH).
        IDX_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        if state.index_broken {
            IDX_SCANNED.fetch_add(state.allocations.len() as u32, Ordering::Relaxed);
            state.allocations.iter().find_map(look)
        } else {
            let ids = state
                .by_process
                .iter()
                .find(|(p, _)| *p == process)
                .map_or(&[][..], |(_, ids)| ids.as_slice());
            IDX_SCANNED.fetch_add(ids.len() as u32, Ordering::Relaxed);
            ids.iter()
                .filter_map(|id| state.index_of(*id).map(|i| &state.allocations[i]))
                .find_map(look)
        }
    };
    match found {
        None => Ok(false),
        Some(Where::System(physical)) => {
            let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
            pa.QuadPart = (physical & !0xfff) as i64;
            // SAFETY: a page VidMm keeps locked while it is mapped for the GPU;
            // MmCached matches system RAM, and the view is released below.
            let view = unsafe { MmMapIoSpace(pa, 0x1000, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
            if view.is_null() {
                return Err(Error::NoMemory);
            }
            visit(unsafe { view.add((physical & 0xfff) as usize) });
            unsafe { MmUnmapIoSpace(view.cast(), 0x1000) };
            Ok(true)
        }
        Some(Where::Rm(id, offset)) => {
            // The RM-memory view needs PASSIVE; system pages work up to DISPATCH.
            let Some(passive) = passive else {
                return Err(Error::Busy);
            };
            let mut visit = Some(visit);
            // SAFETY: PASSIVE; the closure only writes inside the given range.
            unsafe {
                with_cpu_range(passive, adapter, id, offset, len, |_, native, got| {
                    if got == len {
                        if let Some(visit) = visit.take() {
                            visit(native);
                        }
                    }
                    true
                })
            }
        }
    }
}

/// Apply one native leaf update.  Non-leaf updates deliberately have no host
/// operation.  Invalid PTEs unmap.  A changed live mapping is refused until the
/// parent marks that address space idle; unmapping and fresh mapping remain
/// synchronous, so a following FLUSH_TLB has no deferred work.
/// Where the last `update_leaf` stopped (`leaf_stage`): counted per stage on an
/// error (`NvLf<stage>`), and a watch event (op 11, detail = stage; 0xff = no
/// change needed, 0xfe = mapped) for watched pages.
static LEAF_STAGE: AtomicU32 = AtomicU32::new(0);

/// `NvLfPad`: leaf-update pages past the end of the RM memory (VidMm's padding),
/// skipped; `NvLfPadOff` / `NvLfPadSz` the last one's offset and the memory size
/// (4-KiB pages).
fn note_padding(offset: u64, size: u64) {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 256 == 0 {
        crate::diag::record_named_bytes(b"NvLfPad", n);
        crate::diag::record_named_bytes(b"NvLfPadOff", (offset >> 12) as u32);
        crate::diag::record_named_bytes(b"NvLfPadSz", (size >> 12) as u32);
    }
}

fn leaf_stage(stage: u32) {
    LEAF_STAGE.store(stage, Ordering::Relaxed);
}

pub fn update_leaf(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
) -> Result<(), Error> {
    let start = crate::adapter::foreign_scanout::now_100ns();
    let trips = round_trips();
    let result = update_leaf_staged(passive, adapter, id, process, first_va, allocation_offset, pieces);
    LEAF_COST.note(start, trips);
    result
}

/// [`update_leaf`] without the cost counters (`update_leaf_system` counts its own).
fn update_leaf_staged(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
) -> Result<(), Error> {
    leaf_stage(1);
    let result = update_leaf_inner(passive, adapter, id, process, first_va, allocation_offset, pieces);
    let stage = LEAF_STAGE.load(Ordering::Relaxed);
    let len = pieces.len() as u64 * nm::PAGE_BYTES;
    match result {
        Ok(()) => watch::event(first_va, len, 11, stage, process),
        Err(error) => {
            watch::event(first_va, len, 11, stage, process);
            static COUNTS: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
            let k = (stage as usize).min(15);
            let n = COUNTS[k].fetch_add(1, Ordering::Relaxed) + 1;
            let digit = if k < 10 { b'0' + k as u8 } else { b'a' + k as u8 - 10 };
            crate::diag::record_named_bytes(&[b'N', b'v', b'L', b'f', digit], n);
            crate::diag::record_named_bytes(b"NvLfErr", (stage << 8) | error as u32);
            crate::diag::record_named_bytes(b"NvLfVa", (first_va >> 12) as u32);
            crate::diag::record_named_bytes(b"NvLfOff", (allocation_offset >> 12) as u32);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn update_leaf_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    leaf_stage(2);
    let count = pieces.len() as u64;
    let len = count.checked_mul(nm::PAGE_BYTES).ok_or(Error::Invalid)?;
    // Only this process's mappings of the allocation that overlap the update are
    // copied out (pieces that do not overlap it never reach the span compared or
    // changed below); the allocation's whole list is not cloned.
    let mut old_process: Vec<HostMapping> = Vec::new();
    let (memory, size, system_backed) = {
        let state = STATE.lock();
        let allocation = state.allocation(id)?;
        if allocation.poisoned || allocation.epoch != client.epoch {
            return Err(Error::Poisoned);
        }
        let hit = |m: &&HostMapping| {
            m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
        };
        let n = allocation.mappings.iter().filter(hit).count();
        old_process.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        old_process.extend(allocation.mappings.iter().filter(hit).copied());
        (allocation.memory, allocation.size, allocation.system_memory != 0)
    };
    if pieces.iter().any(|p| p.is_some()) {
        // Placed in video memory: VidMm's system pages for it are its to free, so
        // its idle OS descriptors go (`helios_kmd_logic::desc_cache`).
        drop_idle_descs_of(&io, id);
    }
    leaf_stage(3);
    let space = ensure_space(&io, process)?;
    leaf_stage(4);
    if count == 0 || first_va & (nm::PAGE_BYTES - 1) != 0 {
        return Err(Error::Invalid);
    }
    let mut new = Vec::new();
    new.try_reserve_exact(pieces.len() + 2)
        .map_err(|_| Error::NoMemory)?;
    for (index, piece) in pieces.iter().enumerate() {
        if let Some(piece) = piece {
            let expected = nm::MappingPiece {
                va: first_va + index as u64 * nm::PAGE_BYTES,
                memory_offset: allocation_offset + index as u64 * nm::PAGE_BYTES,
                len: nm::PAGE_BYTES,
            };
            if *piece != expected {
                return Err(Error::Invalid);
            }
            // VidMm maps an allocation's VA range in its own granularity: the
            // pages past the RM memory's end are its padding, which nothing
            // reads. Map what exists; refusing the whole update left offset 0
            // unmapped too (CE writes faulted at the allocation's first page).
            if piece.memory_end().is_none_or(|end| end > size) {
                note_padding(piece.memory_offset, size);
                continue;
            }
            leaf_stage(5);
            nm::validate_piece(*piece, size).map_err(|_| Error::Invalid)?;
            leaf_stage(4);
            new.push(*piece);
        }
    }
    nm::coalesce(&mut new);

    let mut old_pieces: Vec<nm::MappingPiece> = Vec::new();
    old_pieces
        .try_reserve_exact(old_process.len())
        .map_err(|_| Error::NoMemory)?;
    old_pieces.extend(old_process.iter().map(|m| m.piece));
    leaf_stage(6);
    let unmapped =
        nm::replace_range(&old_pieces, first_va, len, None, size).map_err(|_| Error::Invalid)?;
    leaf_stage(7);
    let mut final_pieces = unmapped.pieces;
    for piece in new.iter().copied() {
        final_pieces = nm::replace_range(&final_pieces, piece.va, piece.len, Some(piece), size)
            .map_err(|_| Error::Overlap)?
            .pieces;
    }
    // No coalescing across updates: every existing piece is one live RM virtual
    // object. VidMm fills a large allocation one page table (2 MiB of VA) per
    // call; merging the new piece into the previous call's neighbour remapped
    // the merged range while the neighbour's object still held its part, and RM
    // refused the fixed VA (NV_ERR_INSUFFICIENT_RESOURCES).
    final_pieces.sort_unstable_by_key(|piece| piece.va);

    // GPU VAs are per process: only this process's other mappings can collide.
    // The page table VidMm writes is the truth: whatever another allocation or
    // a foreign range still holds at these VAs is gone from VidMm's view, so it
    // goes from RM's too (RM refused the new fixed mapping over the stale one:
    // NV_ERR_INVALID_ARGUMENT, and the GPU faulted on the unmapped range).
    leaf_stage(8);
    evict_range(&io, space, process, first_va, len, Some(id))?;
    leaf_stage(9);

    let affected_old: Vec<HostMapping> = old_process
        .iter()
        .copied()
        .filter(|m| nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len))
        .collect();
    let update_end = first_va.checked_add(len).ok_or(Error::Invalid)?;
    let span_start = affected_old
        .iter()
        .map(|m| m.piece.va)
        .min()
        .unwrap_or(first_va)
        .min(first_va);
    let span_end = affected_old
        .iter()
        .filter_map(|m| m.piece.end())
        .max()
        .unwrap_or(update_end)
        .max(update_end);
    let affected_new: Vec<nm::MappingPiece> = final_pieces
        .iter()
        .copied()
        .filter(|piece| piece.va < span_end && span_start < piece.end().unwrap_or(piece.va))
        .collect();
    if affected_old
        .iter()
        .map(|m| m.piece)
        .eq(affected_new.iter().copied())
    {
        leaf_stage(0xff);
        return Ok(());
    }
    leaf_stage(10);

    // Replacing a live mapping: VidMm only changes an allocation's placement once
    // no GPU work that uses it is outstanding (it waits for the paging and
    // context fences first), so RM's idle-channels quiesce is only a belt on
    // top. It is best effort: its refusal (Busy) had VidMm's every move between
    // system pages and video memory skipped (NvPgLeafWhy 0x311).
    // `NvHotPath` 0x4 skips it (`HpQuiSkip`); otherwise it is timed (`HpQuiN`, `HpQuiUs`,
    // `HpQuiMaxUs`): one host round trip that waits for the client's channels.
    if !affected_old.is_empty() {
        if crate::ddi::hot_path::knob(passive).no_quiesce() {
            crate::ddi::hot_path::count(&crate::ddi::hot_path::QUIESCE_SKIPPED);
        } else {
            let started = crate::adapter::foreign_scanout::now_100ns();
            let quiesced = super::native_channel::quiesce(passive, adapter, process);
            crate::ddi::hot_path::note_quiesce(
                crate::adapter::foreign_scanout::now_100ns().saturating_sub(started),
            );
            if quiesced.is_err() {
                // `NvQuiesceRef`: published by the mirror pass (`publish_hot`).
                QUIESCE_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // Mint handles before tearing down anything.  A local handle-space
    // exhaustion therefore cannot strand the old mapping.
    let mut planned = Vec::new();
    planned
        .try_reserve_exact(affected_new.len())
        .map_err(|_| Error::NoMemory)?;
    for piece in affected_new.iter().copied() {
        planned.push((piece, STATE.lock().mint()?));
    }
    leaf_stage(11);
    // NvPerfMem 0x20: every unmap and map of this update but the last defers its
    // TLB invalidate; the last one's invalidate covers them all. An update that
    // fails after a deferred operation invalidates explicitly.
    let defer_on = perf().tlb_defer();
    let operations = affected_old.len() + planned.len();
    let mut deferred = false;
    // Every unmap in as few round trips as the batch allows (`NvPaging` 0x1); the
    // first failure stops the rest, as the one-by-one loop did.
    let mut items = Vec::new();
    items
        .try_reserve_exact(affected_old.len())
        .map_err(|_| Error::NoMemory)?;
    for (index, mapping) in affected_old.iter().copied().enumerate() {
        items.push((memory, mapping, np::defer_tlb(defer_on, index, operations)));
    }
    let mut unmapped = Vec::new();
    unmap_many(&io, &items, true, &mut unmapped)?;
    for (item, result) in items.iter().zip(&unmapped) {
        if result.is_err() {
            if deferred {
                invalidate_tlb(&io, space);
            }
            STATE.lock().allocation_mut(id)?.poisoned = true;
            return Err(Error::Poisoned);
        }
        deferred |= item.2;
    }
    leaf_stage(13);

    let mut made = Vec::new();
    for (index, (piece, virt)) in planned.iter().copied().enumerate() {
        let defer = np::defer_tlb(defer_on, affected_old.len() + index, operations);
        let mapped = map_fixed_opts(&io, memory, size, system_backed, space, virt, piece, defer);
        if mapped.is_ok() {
            deferred |= defer;
        }
        watch::event(
            piece.va,
            piece.len,
            if mapped.is_ok() { 5 } else { 6 },
            mapped.as_ref().err().map_or(0, |e| *e as u32),
            process,
        );
        if let Err(error) = mapped {
            note_failed_map(piece.va, 1, error);
            // Who collided: the process key, the allocation, its space and how
            // many allocations / tracked mappings of this process exist.
            {
                let state = STATE.lock();
                let tracked = state
                    .allocations
                    .iter()
                    .flat_map(|a| a.mappings.iter())
                    .filter(|m| m.process == process)
                    .count();
                crate::diag::record_named_bytes(b"NvMapProc", process as u32);
                crate::diag::record_named_bytes(b"NvMapId", id);
                crate::diag::record_named_bytes(b"NvMapSpace", space);
                crate::diag::record_named_bytes(b"NvMapTracked", tracked as u32);
                crate::diag::record_named_bytes(b"NvMapSpaces", state.spaces.len() as u32);
            }
            for mapping in made.iter().copied() {
                let _ = unmap_fixed(&io, memory, mapping);
            }
            // Best-effort rollback.  If it cannot be completed, poison this
            // allocation: no success may be reported against a guessed map set.
            let mut rollback = Vec::new();
            let mut rollback_ok = true;
            for old_mapping in affected_old.iter().copied() {
                if old_mapping.desc != 0 {
                    // Its descriptor is gone with the unmap; nothing to restore.
                    rollback_ok = false;
                    break;
                }
                let virt = match STATE.lock().mint() {
                    Ok(v) => v,
                    Err(_) => {
                        rollback_ok = false;
                        break;
                    }
                };
                let Ok(virt) = map_fixed(
                    &io,
                    memory,
                    size,
                    system_backed,
                    space,
                    virt,
                    old_mapping.piece,
                ) else {
                    rollback_ok = false;
                    break;
                };
                rollback.push(HostMapping {
                    virt,
                    ..old_mapping
                });
            }
            if !rollback_ok {
                if deferred {
                    invalidate_tlb(&io, space);
                }
                STATE.lock().allocation_mut(id)?.poisoned = true;
                return Err(Error::Poisoned);
            }
            let mut state = STATE.lock();
            let allocation = state.allocation_mut(id)?;
            allocation.mappings.retain(|mapping| {
                !(mapping.process == process
                    && mapping.piece.va < span_end
                    && span_start < mapping.piece.end().unwrap_or(mapping.piece.va))
            });
            let restored = !rollback.is_empty();
            allocation.mappings.extend(rollback);
            allocation
                .mappings
                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
            if restored {
                state.note_mapped(process, id);
            }
            return Err(error);
        }
        made.push(HostMapping {
            process,
            piece,
            // The window's handle when the piece went into the low window.
            virt: mapped.unwrap_or(virt),
            desc: 0,
        });
    }

    let mut state = STATE.lock();
    let allocation = state.allocation_mut(id)?;
    allocation.mappings.retain(|mapping| {
        !(mapping.process == process
            && nm::ranges_overlap(mapping.piece.va, mapping.piece.len, first_va, len))
    });
    let any = !made.is_empty();
    allocation.mappings.extend(made);
    allocation
        .mappings
        .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
    if any {
        state.note_mapped(process, id);
    }
    leaf_stage(0xfe);
    Ok(())
}

/// Mark whether an address space has been quiesced for a live replacement.
/// The scheduler/root-page-table integration calls this; absence means busy.
pub fn set_address_space_idle(process: usize, idle: bool) {
    let mut state = STATE.lock();
    if idle {
        if !state.idle_spaces.contains(&process) {
            let _ = state.idle_spaces.try_reserve_exact(1);
            if !state.idle_spaces.contains(&process) {
                state.idle_spaces.push(process);
            }
        }
    } else {
        state.idle_spaces.retain(|p| *p != process);
    }
}

pub fn notify_residency(id: u32, physical_address: u64, resident: bool) -> Result<(), Error> {
    let mut state = STATE.lock();
    let allocation = state.allocation_mut(id)?;
    allocation.resident = resident.then_some((physical_address, true));
    Ok(())
}

fn release_cpu_chunk(io: &Io<'_>, memory: u32, chunk: &mut CpuChunk) -> Result<(), Error> {
    if chunk.map_ch == 0 {
        return Ok(());
    }
    if chunk.host_id != 0 {
        nvrm::release_host_map_within(
            io.passive,
            io.adapter,
            chunk.map_ch,
            chunk.host_id,
            TIMEOUT_MS,
        )
        .map_err(|_| Error::Host)?;
        chunk.host_id = 0;
    }
    if chunk.rm_mapped {
        let block = rc::nvos34(io.client.root, nd::H_DEVICE, memory, chunk.cookie);
        let mut response = [0u8; REPLY_MAX];
        let n = io.exchange(
            io.client.ctl,
            rc::nv_cmd(rc::ESC_RM_UNMAP_MEMORY, rc::NVOS34_BYTES as u32),
            &block,
            &[],
            &mut response,
        )?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS34_STATUS_AT)
            .map_err(|_| Error::Host)?;
        chunk.cookie = 0;
        chunk.rm_mapped = false;
    }
    io.close_file(chunk.map_ch)?;
    chunk.map_ch = 0;
    Ok(())
}

fn cpu_map_chunk(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    expected_gpa: Option<u64>,
) -> Result<CpuChunk, Error> {
    let region = nvrm::region_for(io.adapter, io.client.minor).ok_or(Error::Host)?;
    let expected = match expected_gpa {
        Some(base) => Some(base.checked_add(offset).ok_or(Error::WrongAddress)?),
        None => None,
    };
    let window_offset = match expected {
        Some(expected) => Some(
            expected
                .checked_sub(region.base)
                .ok_or(Error::WrongAddress)?,
        ),
        None => None,
    };
    let map_ch = io.open_file(io.client.minor)?;
    if let Err(error) = io.register_fd(map_ch) {
        let _ = io.close_file(map_ch);
        return Err(error);
    }
    let mut chunk = CpuChunk {
        physical: 0,
        map_ch,
        cookie: 0,
        rm_mapped: false,
        host_id: 0,
    };
    let block = rc::nvos33_with_fd(
        io.client.root,
        nd::H_DEVICE,
        allocation.memory,
        offset,
        len,
        map_ch,
    );
    let mut response = [0u8; REPLY_MAX];
    let cookie = match io.exchange(
        io.client.ctl,
        rc::nv_cmd(rc::ESC_RM_MAP_MEMORY, rc::NVOS33_FD_BYTES as u32),
        &block,
        &[],
        &mut response,
    ) {
        Ok(n) => (|| {
            let reply = response.get(..n).ok_or(Error::Host)?;
            let reply = rc::rm_reply(reply, rc::NVOS33_STATUS_AT).map_err(|_| Error::Host)?;
            rc::map_cookie(&reply).ok_or(Error::Host)
        })(),
        Err(error) => Err(error),
    };
    match cookie {
        Ok(cookie) => {
            chunk.cookie = cookie;
            chunk.rm_mapped = true;
        }
        Err(error) => {
            let _ = io.close_file(map_ch);
            return Err(error);
        }
    }
    let mapped = match window_offset {
        Some(window_offset) => nvrm::host_mmap_at_within(
            io.passive,
            io.adapter,
            KMD,
            map_ch,
            true,
            0,
            window_offset,
            len,
            TIMEOUT_MS,
        ),
        None => nvrm::host_mmap_within(
            io.passive, io.adapter, KMD, map_ch, true, 0, len, TIMEOUT_MS,
        ),
    };
    let mapped = match mapped {
        Ok(mapped) => {
            chunk.host_id = mapped.host_id;
            mapped
        }
        Err(_) => {
            let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
            return Err(Error::Host);
        }
    };
    if mapped.size < len {
        let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
        return Err(Error::Host);
    }
    let physical = region.base.checked_add(mapped.offset);
    if physical.is_none_or(|physical| expected.is_some_and(|expected| physical != expected)) {
        let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
        return Err(Error::WrongAddress);
    }
    chunk.physical = physical.ok_or(Error::WrongAddress)?;
    Ok(chunk)
}

fn cpu_map(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    aperture_offset: Option<u64>,
    expected_gpa: Option<u64>,
) -> Result<CpuMapping, Error> {
    let end = offset.checked_add(len).ok_or(Error::Invalid)?;
    if len == 0
        || end > allocation.size
        || (allocation.system_memory == 0
            && (offset & (nm::PAGE_BYTES - 1) != 0 || len & (nm::PAGE_BYTES - 1) != 0))
        || aperture_offset.is_some() != expected_gpa.is_some()
        || (expected_gpa.is_none() && len > nvrm::MAX_MAP_BYTES)
    {
        return Err(Error::Invalid);
    }
    if allocation.system_memory != 0 {
        // Separate MDL mappings are not contiguous. Never expose the first
        // chunk (or a process-local user VA) as a view of the whole allocation.
        if allocation.system_kernel_va == 0 {
            return Err(Error::Unsupported);
        }
        let address = allocation
            .system_kernel_va
            .checked_add(offset)
            .ok_or(Error::Invalid)?;
        return Ok(CpuMapping {
            view: CpuView {
                address,
                len,
                generation: 0,
            },
            chunks: Vec::new(),
            kernel_va: 0,
            aperture_offset,
        });
    }

    let count =
        usize::try_from((len - 1) / nvrm::MAX_MAP_BYTES + 1).map_err(|_| Error::NoMemory)?;
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let mut chunk_offset = offset;
    while chunk_offset < end {
        let chunk_len = (end - chunk_offset).min(nvrm::MAX_MAP_BYTES);
        match cpu_map_chunk(io, allocation, chunk_offset, chunk_len, expected_gpa) {
            Ok(chunk) => chunks.push(chunk),
            Err(error) => {
                for chunk in chunks.iter_mut() {
                    let _ = release_cpu_chunk(io, allocation.memory, chunk);
                }
                return Err(error);
            }
        }
        chunk_offset += chunk_len;
    }
    let physical = match expected_gpa {
        Some(expected) => expected,
        None => chunks
            .first()
            .map(|chunk| chunk.physical)
            .ok_or(Error::Host)?,
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = physical as i64;
    // A fixed aperture has contiguous placements by construction, so one
    // aggregate guest view is valid. Dynamic mappings are limited to one chunk.
    let kernel_va = unsafe { MmMapIoSpace(pa, len, _MEMORY_CACHING_TYPE::MmNonCached) } as u64;
    let mut mapping = CpuMapping {
        view: CpuView {
            address: kernel_va,
            len,
            generation: chunks.first().map_or(0, |chunk| chunk.host_id),
        },
        chunks,
        kernel_va,
        aperture_offset,
    };
    if kernel_va == 0 {
        let _ = cpu_unmap_for(io, allocation.memory, &mut mapping);
        return Err(Error::NoMemory);
    }
    Ok(mapping)
}

fn cpu_unmap_for(io: &Io<'_>, memory: u32, mapping: &mut CpuMapping) -> Result<(), Error> {
    if mapping.kernel_va != 0 {
        unsafe { MmUnmapIoSpace(mapping.kernel_va as *mut c_void, mapping.view.len) };
        mapping.kernel_va = 0;
    }
    let mut ok = true;
    // A failed step leaves that chunk's fd and later steps intact for retry,
    // but does not prevent teardown of any other chunk.
    for chunk in mapping.chunks.iter_mut() {
        if release_cpu_chunk(io, memory, chunk).is_err() {
            ok = false;
        }
    }
    if ok {
        Ok(())
    } else {
        Err(Error::Host)
    }
}

/// Run a synchronous content operation over a range of the allocation's real
/// CPU bytes. Dynamic placements need not be contiguous, so temporary mappings
/// are made and released one page-rounded, bounded chunk at a time.
pub unsafe fn with_cpu_range(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    length: u64,
    mut f: impl FnMut(u64, *mut u8, u64) -> bool,
) -> Result<bool, Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (snapshot, existing, pending) = {
        let state = STATE.lock();
        let a = state.allocation(id)?;
        let pending = a
            .cpu
            .as_ref()
            .is_some_and(|cpu| cpu.aperture_offset.is_none());
        let existing = a.cpu.as_ref().and_then(|cpu| {
            (cpu.aperture_offset.is_some() && (a.system_memory != 0 || cpu.kernel_va != 0))
                .then_some(cpu.view)
        });
        (
            Allocation {
                id: a.id,
                epoch: a.epoch,
                memory: a.memory,
                size: a.size,
                system_memory: a.system_memory,
                system_kernel_va: a.system_kernel_va,
                pins: Vec::new(),
                mappings: Vec::new(),
                cpu: None,
                resident: None,
                poisoned: a.poisoned,
                writeback: a.writeback,
                surface: a.surface,
                scanout_gem: 0,
                huge: a.huge,
            },
            existing,
            pending,
        )
    };
    let end = offset.checked_add(length).ok_or(Error::Invalid)?;
    if end > snapshot.size {
        return Err(Error::Invalid);
    }
    if length == 0 {
        return Ok(true);
    }
    if pending {
        // A previous temporary teardown failed after its guest view was removed.
        // Retry the retained host state before making another mapping.
        let mut mapping = STATE
            .lock()
            .allocation_mut(id)?
            .cpu
            .take()
            .ok_or(Error::Invalid)?;
        if let Err(error) = cpu_unmap_for(&io, snapshot.memory, &mut mapping) {
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
    } else if STATE.lock().allocation(id)?.cpu.is_some() && existing.is_none() {
        return Err(Error::Busy);
    }
    if let Some(view) = existing {
        let address = view.address.checked_add(offset).ok_or(Error::Invalid)?;
        return Ok(f(0, address as *mut u8, length));
    }
    if snapshot.system_memory != 0 {
        if snapshot.system_kernel_va == 0 {
            return Err(Error::Unsupported);
        }
        let address = snapshot
            .system_kernel_va
            .checked_add(offset)
            .ok_or(Error::Invalid)?;
        return Ok(f(0, address as *mut u8, length));
    }

    let mut completed = 0u64;
    while completed < length {
        let request_offset = offset + completed;
        let map_offset = request_offset & !(nm::PAGE_BYTES - 1);
        let page_offset = request_offset - map_offset;
        let chunk_len = (length - completed).min(nvrm::MAX_MAP_BYTES - page_offset);
        let request_end = request_offset + chunk_len;
        let map_end = request_end
            .checked_add(nm::PAGE_BYTES - 1)
            .map(|end| end & !(nm::PAGE_BYTES - 1))
            .unwrap_or(snapshot.size)
            .min(snapshot.size);
        let mut mapping = cpu_map(&io, &snapshot, map_offset, map_end - map_offset, None, None)?;
        // SAFETY: page_offset is within the page-rounded mapping made above.
        let address = (mapping.view.address as *mut u8).add(page_offset as usize);
        let accepted = f(completed, address, chunk_len);
        if let Err(error) = cpu_unmap_for(&io, snapshot.memory, &mut mapping) {
            // Keep partially released host state reachable for a later retry or
            // transport teardown; never silently drop a live mapping.
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
        if !accepted {
            return Ok(false);
        }
        completed += chunk_len;
    }
    Ok(true)
}

/// Map a native CPU-host-aperture request.  pSystemMem is already the direct
/// Lock2 view.  Device local memory uses NVOS33 + the RM window, and succeeds
/// only when that window placement is exactly dxgkrnl's requested GPA.
pub fn map_aperture(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    aperture_offset: u64,
    expected_gpa: u64,
) -> Result<CpuView, Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let snapshot = {
        let state = STATE.lock();
        let a = state.allocation(id)?;
        Allocation {
            id: a.id,
            epoch: a.epoch,
            memory: a.memory,
            size: a.size,
            system_memory: a.system_memory,
            system_kernel_va: a.system_kernel_va,
            pins: Vec::new(),
            mappings: Vec::new(),
            cpu: None,
            resident: None,
            poisoned: a.poisoned,
            writeback: a.writeback,
            surface: a.surface,
            scanout_gem: 0,
            huge: a.huge,
        }
    };
    let mut mapping = cpu_map(
        &io,
        &snapshot,
        0,
        snapshot.size,
        Some(aperture_offset),
        Some(expected_gpa),
    )?;
    let view = mapping.view;
    let mut state = STATE.lock();
    let allocation = state.allocation_mut(id)?;
    if allocation.cpu.is_some() {
        drop(state);
        let mut mapping = mapping;
        let _ = cpu_unmap_for(&io, snapshot.memory, &mut mapping);
        return Err(Error::Busy);
    }
    allocation.cpu = Some(mapping);
    Ok(view)
}

pub fn unmap_aperture(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    aperture_offset: u64,
) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (id, memory) = {
        let state = STATE.lock();
        let allocation = state
            .allocations
            .iter()
            .find(|a| {
                a.cpu
                    .as_ref()
                    .is_some_and(|cpu| cpu.aperture_offset == Some(aperture_offset))
            })
            .ok_or(Error::Invalid)?;
        (allocation.id, allocation.memory)
    };
    let mut mapping = STATE
        .lock()
        .allocation_mut(id)?
        .cpu
        .take()
        .ok_or(Error::Invalid)?;
    match cpu_unmap_for(&io, memory, &mut mapping) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Preserve whatever remains live for a later retry or transport
            // teardown; a failed host unmap is not permission to forget it.
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            Err(error)
        }
    }
}

/// Destroy an allocation only after every GPU and CPU view is gone.  Failure
/// leaves the record and its MDLs alive; the caller must not report reclamation
/// or unlock pSystemMem early.
pub fn destroy(passive: PassiveLevel, adapter: &AdapterContext, id: u32) -> Result<(), Error> {
    let _guard = lock(passive)?;
    let client = STATE.lock().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (memory, mappings, gem) = {
        let state = STATE.lock();
        let allocation = state.allocation(id)?;
        (
            allocation.memory,
            allocation.mappings.clone(),
            allocation.scanout_gem,
        )
    };
    // A CPU view dxgkrnl did not unmap first (an UnmapApertureSegment that came
    // above PASSIVE leaves it, `cpu_host_aperture`) goes here: nothing maps the
    // allocation through the aperture once it is destroyed.
    let cpu = STATE.lock().allocation_mut(id)?.cpu.take();
    if let Some(mut mapping) = cpu {
        if let Err(error) = cpu_unmap_for(&io, memory, &mut mapping) {
            STATE.lock().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
    }
    if gem != 0 {
        super::native_scanout::release_gem(&io, gem);
        STATE.lock().allocation_mut(id)?.scanout_gem = 0;
    }
    // Every mapping is tried (in as few round trips as `NvPaging` 0x1 allows); one
    // RM refuses stays recorded (and the allocation with it), the others are gone
    // and not named again.
    let mut items = Vec::new();
    items.try_reserve_exact(mappings.len()).map_err(|_| Error::NoMemory)?;
    items.extend(mappings.iter().map(|m| (memory, *m, false)));
    let mut results = Vec::new();
    unmap_many(&io, &items, false, &mut results)?;
    let mut unmapped = Ok(());
    for (mapping, result) in mappings.iter().zip(results) {
        match result {
            Ok(()) => {
                STATE
                    .lock()
                    .allocation_mut(id)?
                    .mappings
                    .retain(|live| !(live.virt == mapping.virt && live.piece.va == mapping.piece.va));
            }
            Err(error) => unmapped = Err(error),
        }
    }
    unmapped?;
    // Its pages are VidMm's to free now: no idle descriptor may outlive it.
    drop_idle_descs_of(&io, id);
    io.rm_free(nd::H_DEVICE, memory)?;
    super::nvda::forget_memory(memory);
    let mut allocation = {
        let mut state = STATE.lock();
        let index = state.index_of(id).ok_or(Error::Invalid)?;
        state.allocations.remove(index)
    };
    unlock_pins(&mut allocation.pins);
    Ok(())
}

/// `DxgkDdiDestroyProcess`: release every RM virtual object the process still
/// maps and its VA space. Process contexts are keyed by address, so a later
/// process can reuse the key: left behind, the dead process's fixed-VA objects
/// made RM refuse the new process's identical VidMm VAs (the D3D11 UMD's GPFIFO
/// at 0x4000000, NV_ERR_INSUFFICIENT_RESOURCES). Returns how many mappings
/// could not be released (they stay recorded and the space is kept).
pub fn release_process(passive: PassiveLevel, adapter: &AdapterContext, process: usize) -> u32 {
    if process == 0 {
        return 0;
    }
    let Ok(_guard) = lock(passive) else {
        return 1;
    };
    let Ok(client) = STATE.lock().client() else {
        // No native client: nothing was ever mapped for anyone.
        let mut state = STATE.lock();
        state.spaces.retain(|(owner, _)| *owner != process);
        state.idle_spaces.retain(|p| *p != process);
        return 0;
    };
    let io = Io {
        passive,
        adapter,
        client,
    };
    let owned: Vec<(u32, u32, HostMapping)> = {
        let state = STATE.lock();
        let mut owned = Vec::new();
        let mut candidates = Vec::new();
        if state.candidates(process, &mut candidates).is_err() {
            return 1;
        }
        for id in candidates {
            let Some(i) = state.index_of(id) else { continue };
            let allocation = &state.allocations[i];
            for mapping in allocation.mappings.iter().filter(|m| m.process == process) {
                if owned.try_reserve(1).is_err() {
                    return 1;
                }
                owned.push((allocation.id, allocation.memory, *mapping));
            }
        }
        owned
    };
    let mut kept = 0;
    let foreign: Vec<HostMapping> = {
        let mut state = STATE.lock();
        let mut mine = Vec::new();
        if mine.try_reserve(state.foreign.len()).is_err() {
            return 1;
        }
        state.foreign.retain(|m| {
            if m.process == process {
                mine.push(*m);
                false
            } else {
                true
            }
        });
        mine
    };
    // Every unmap in as few round trips as `NvPaging` 0x1 allows.
    let mut items = Vec::new();
    if items.try_reserve_exact(foreign.len() + owned.len()).is_err() {
        return 1;
    }
    items.extend(foreign.iter().map(|m| (0u32, *m, false)));
    items.extend(owned.iter().map(|(_, memory, m)| (*memory, *m, false)));
    let mut results = Vec::new();
    if unmap_many(&io, &items, false, &mut results).is_err() {
        return 1;
    }
    let (foreign_results, owned_results) = results.split_at(foreign.len());
    for (mapping, result) in foreign.into_iter().zip(foreign_results) {
        if result.is_err() {
            kept += 1;
            // Still mapped: recorded under the dead key (see below).
            let mut state = STATE.lock();
            if state.foreign.try_reserve(1).is_ok() {
                state.foreign.push(HostMapping {
                    process: nt::DEAD_PROCESS,
                    ..mapping
                });
            }
        }
    }
    for ((id, _, mapping), result) in owned.into_iter().zip(owned_results) {
        if result.is_ok() {
            if let Ok(allocation) = STATE.lock().allocation_mut(id) {
                allocation
                    .mappings
                    .retain(|live| !(live.virt == mapping.virt && live.piece.va == mapping.piece.va));
            }
        } else {
            kept += 1;
        }
    }
    if kept == 0 {
        let space = STATE
            .lock()
            .spaces
            .iter()
            .find(|(owner, _)| *owner == process)
            .map(|(_, space)| *space);
        if let Some(space) = space {
            for window in windows_of(space) {
                if io.rm_free(nd::H_DEVICE, window).is_ok() {
                    STATE.lock().windows.retain(|(_, live, _, _)| *live != window);
                }
            }
            if io.rm_free(nd::H_DEVICE, space).is_ok() {
                STATE.lock().spaces.retain(|(owner, _)| *owner != process);
            } else {
                kept += 1;
            }
        }
        STATE.lock().idle_spaces.retain(|p| *p != process);
    }
    if kept != 0 {
        // What RM would not release stays recorded, but no longer under this key:
        // the key is the freed ProcessContext's address, and a new process at the
        // same address must get a fresh space, not the dead one's. Allocation
        // teardown and the transport's sweep still find it.
        let mut state = STATE.lock();
        for allocation in state.allocations.iter_mut() {
            for mapping in allocation.mappings.iter_mut() {
                mapping.process = nt::retire_owner(mapping.process, process);
            }
        }
        for allocation in state.allocations.iter_mut() {
            allocation
                .mappings
                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
        }
        for (owner, _) in state.spaces.iter_mut() {
            *owner = nt::retire_owner(*owner, process);
        }
        state.idle_spaces.retain(|p| *p != process);
    }
    // A new process may get this key: its index starts empty. (What stayed is
    // recorded under the dead key, which no update names.)
    {
        let mut state = STATE.lock();
        if let Some(at) = state.by_process.iter().position(|(p, _)| *p == process) {
            state.by_process.swap_remove(at);
        }
    }
    kept
}

/// Drop only guest-owned state after the transport already destroyed every RM
/// object.  No host request is sent and no pSystemMem page is unlocked early.
pub fn discard(id: u32) -> Result<(), Error> {
    let mut state = STATE.lock();
    let index = state.index_of(id).ok_or(Error::Invalid)?;
    let mut allocation = state.allocations.remove(index);
    // The transport destroyed every RM object: its idle descriptors are gone too.
    let mut dead = Vec::new();
    state.descs.drop_idle_of(id, &mut dead);
    drop(state);
    drop(dead);
    if let Some(cpu) = allocation.cpu.as_mut() {
        if cpu.kernel_va != 0 {
            // PASSIVE (DestroyAllocation); the host mapping is already gone.
            unsafe { MmUnmapIoSpace(cpu.kernel_va as *mut c_void, cpu.view.len) };
            cpu.kernel_va = 0;
        }
    }
    unlock_pins(&mut allocation.pins);
    Ok(())
}

/// Forget host objects after the transport-wide close sweep.  Kernel CPU views
/// and MDLs still belong to the guest and are released here, at PASSIVE.
pub fn forget(passive: PassiveLevel) {
    // Unbounded: a skipped forget would leave the old generation's client in place
    // and every later `create` refused as NotReady. Holders' round trips are
    // bounded, so this wait ends.
    let Ok(_guard) = mutex(passive).and_then(|m| m.lock(passive).ok_or(Error::Busy)) else {
        return;
    };
    let mut state = STATE.lock();
    let mut allocations = core::mem::take(&mut state.allocations);
    state.client = None;
    state.spaces.clear();
    state.windows.clear();
    state.foreign.clear();
    // The sweep closed every RM object: the descriptors went with them.
    state.descs.clear();
    state.by_process.clear();
    state.index_broken = false;
    state.idle_spaces.clear();
    // Pins kept because the host's answer was unknown: the sweep closed every
    // object that could name them.
    let mut orphans = core::mem::take(&mut state.orphan_pins);
    drop(state);
    unlock_pins(&mut orphans);
    for allocation in allocations.iter_mut() {
        if let Some(cpu) = allocation.cpu.as_mut() {
            if cpu.kernel_va != 0 {
                unsafe { MmUnmapIoSpace(cpu.kernel_va as *mut c_void, cpu.view.len) };
                cpu.kernel_va = 0;
            }
        }
        unlock_pins(&mut allocation.pins);
    }
}

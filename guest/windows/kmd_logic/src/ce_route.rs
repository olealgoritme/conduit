//! The copy-engine Present ROUTE (`RmCopyEngine` = 1, milestone M3c-2): the pure half. A windowed
//! Blt Present whose producer sent a valid `'HEF3'` record (`ce_record`) is copied by the KMD's own
//! copy-engine channel (`rm_ce_channel`, `ce_present`) instead of the Venus ring-1 copy, with the
//! Venus copy of the SAME queued request as the automatic fallback. Design, the decision table,
//! the destination lifecycle and the failure matrix: `docs/rm-copy-engine-present.md` section 15
//! ("M3c-2 as built"). The I/O half is `kmd_render/src/ddi/ce_present_route.rs` (decision,
//! dispatch, completion, counters) and `kmd_render/src/virtio/rm_client/ce_route.rs` (the RM
//! calls: lazy channel bring-up, the destination OS descriptor, the producer dup).
//!
//! What is here:
//!
//! * [`Why`], [`Facts`], [`decide`]: the per-Present decision, in a fixed order, with the reason a
//!   Present keeps the Venus copy (`CeRtWhy`, `CeRtMask`).
//! * The `h_client` rule itself is `ce_record::client_check` / `ce_record::both_owned` (the
//!   record's semaphore and source clients must be RM clients the PRESENTING process created).
//! * [`Desc`], [`Dst`], [`retire_plan`]: the per-destination OS-descriptor record and the order of
//!   its retirement (stop new copies, drain, free, unpin).
//! * [`Jobs`], [`Job`], [`Prep`], [`JobState`]: the queued and submitted copies, keyed by the
//!   WindowedBlt token of the request that carries them.
//! * [`Chan`]: the route's own strikes per transport generation.
//! * [`nvos02_osdesc`], the destination slots' handles and GPU VAs, the deadlines.
//! * [`COUNTERS`], [`WRITERS`]: names (`CeRt*`, at most 14 characters).

use crate::ce_present::{self as cp, Retire, Route};
use crate::ce_record::ClientCheck;
use crate::rm_ce_channel as cc;

// ── the decision ─────────────────────────────────────────────────────────────────────────────

/// Why a Present keeps the Venus copy (`CeRtWhy`; `CeRtMask` has bit `code - 1`). Codes 1 to 17
/// are Present-time decisions (nothing was queued for the copy engine); 18 to 23 are dispatch-time
/// fallbacks (the request was queued, and the worker submits its Venus copy instead). New codes are
/// appended, never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Why {
    /// The Present carries no RM-fence boundary (no `HERF`/`HEPR` fence tail for it).
    NoBoundary = 1,
    /// No valid record stashed for THIS Present's fence (`take_ce_record(boundary)` found none).
    NoRecord = 2,
    /// The `h_client` rule: a record client that is not the presenting process's own (or that
    /// the KMD cannot attribute: `NvDupHarden` 0, a full client table). Also `CeRtClient`.
    Client = 3,
    /// The channel is not up yet: a lazy bring-up was requested from the HPD worker.
    ChannelDown = 4,
    /// The route is off for this transport generation (three route strikes, or the channel's own
    /// service struck out).
    RouteOff = 5,
    /// The source is not one the route copies (`ce_present::source_plan`: layout, compression,
    /// generation).
    Source = 6,
    /// The source and destination formats are not a pair `remap_for` converts.
    Format = 7,
    /// The record's image is not the destination's size.
    Extent = 8,
    /// The destination is not a KMD standard buffer with system backing that fits one GPU window.
    Destination = 9,
    /// The destination's system copy is marked invalid (its Venus blob is newer than its pages).
    Stale = 10,
    /// A process other than the presenter has the destination open.
    Foreign = 11,
    /// The destination has a guest blob (`GuestBlob`): a destination uses one of the two, never
    /// both at once.
    GuestBlob = 12,
    /// Three strikes on this destination: off until it is destroyed.
    Struck = 13,
    /// A copy into this destination timed out or its channel failed with it in flight.
    Poisoned = 14,
    /// The worker found the destination's leases do not cover it (until they change).
    Uncovered = 15,
    /// The job table or the destination table is full.
    Full = 16,
    /// The WindowedBlt queue or the private record refused the request at Present.
    Queue = 17,
    /// Dispatch: the producer mapping or the destination descriptor was not made in time.
    NotReady = 18,
    /// Dispatch: the producer's dup or GPU mapping failed (a strike).
    Dup = 19,
    /// Dispatch: the destination's OS descriptor could not be made (a strike).
    Desc = 20,
    /// Dispatch: the channel refused the push (a strike unless the ring was full).
    Submit = 21,
    /// Dispatch: the channel went down between the Present and the dispatch.
    ChannelLost = 22,
    /// Dispatch: the destination is being retired (a lease change or its destruction).
    Retiring = 23,
}

/// Every reason, in code order.
pub const ALL_WHY: [Why; 23] = [
    Why::NoBoundary,
    Why::NoRecord,
    Why::Client,
    Why::ChannelDown,
    Why::RouteOff,
    Why::Source,
    Why::Format,
    Why::Extent,
    Why::Destination,
    Why::Stale,
    Why::Foreign,
    Why::GuestBlob,
    Why::Struck,
    Why::Poisoned,
    Why::Uncovered,
    Why::Full,
    Why::Queue,
    Why::NotReady,
    Why::Dup,
    Why::Desc,
    Why::Submit,
    Why::ChannelLost,
    Why::Retiring,
];

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The `CeRtMask` bit.
    pub const fn bit(self) -> u32 {
        1u32 << (self.code() - 1)
    }

    /// A failure of the route itself, counted as a strike against the destination.
    pub const fn strikes(self) -> bool {
        matches!(self, Why::Dup | Why::Desc | Why::Submit)
    }

    /// Decided at dispatch (the request was queued for the copy engine).
    pub const fn at_dispatch(self) -> bool {
        self.code() >= Why::NotReady.code()
    }
}

/// Everything [`decide`] reads, gathered by the Present arm. The Present arm only consults the
/// route for a Blt that the `BltAsync` entry decision already admitted (a foreign source into a
/// KMD standard buffer, no snapshot, `BltAsync` 1, `ForeignCopy` 1) with `RmCopyEngine` 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The Present carries an RM-fence boundary.
    pub boundary: bool,
    /// A valid record is stashed for that very boundary.
    pub record: bool,
    /// The `h_client` rule over both record clients (`ce_record::both_owned`).
    pub client: ClientCheck,
    /// The route is off for the generation ([`Chan::off`], or the channel service disabled).
    pub route_off: bool,
    /// The channel is up (`Ready`).
    pub channel_up: bool,
    /// The source: `Source`, `Format` or `Extent` when refused.
    pub source: Result<(), Why>,
    /// The destination: `Destination`, `Stale`, `Foreign` or `GuestBlob` when refused.
    pub destination: Result<(), Why>,
    /// The destination's own record: `Struck`, `Poisoned`, `Uncovered` or `Retiring`.
    pub dst_state: Result<(), Why>,
    /// The job table and the destination table have room.
    pub room: bool,
}

/// The order: boundary, record, client, route, channel, source, destination, the destination's
/// record, room. The client rule comes before anything that would make the KMD act on the
/// record's handles; the channel before the source because the source plan needs the channel's
/// generation.
pub fn decide(f: &Facts) -> Result<(), Why> {
    if !f.boundary {
        return Err(Why::NoBoundary);
    }
    if !f.record {
        return Err(Why::NoRecord);
    }
    if f.client != ClientCheck::Owned {
        return Err(Why::Client);
    }
    if f.route_off {
        return Err(Why::RouteOff);
    }
    if !f.channel_up {
        return Err(Why::ChannelDown);
    }
    f.source?;
    f.destination?;
    f.dst_state?;
    if !f.room {
        return Err(Why::Full);
    }
    Ok(())
}

// ── the destination ──────────────────────────────────────────────────────────────────────────

/// The destination's OS descriptor (`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over its lease pages, in
/// the channel's client, GPU-mapped in the channel's VA space).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desc {
    /// None made (or the last one was freed).
    Absent,
    /// The worker is making it (under the content transaction).
    Creating,
    /// Made and mapped: copies may go.
    Ready { va: u64, len: u64 },
    /// The leases did not cover the destination when the worker looked: no copy until they
    /// change (a lease change resets it to `Absent`).
    Uncovered,
    /// Being retired: no new copy.
    Draining,
    /// A free that was not confirmed, or a copy that never completed: the pages stay pinned
    /// until the transport generation ends.
    Leaked,
}

/// One destination: its resource id, its slot (handles and GPU window), its descriptor and the
/// copy engine's per-destination route state (strikes, poison, the last submitted value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dst {
    pub resource_id: u32,
    pub slot: u8,
    pub desc: Desc,
    pub route: Route,
}

impl Dst {
    pub const fn new(resource_id: u32, slot: u8) -> Self {
        Self { resource_id, slot, desc: Desc::Absent, route: Route::new() }
    }

    /// May a new copy be QUEUED for this destination (Present time)? A destination with no
    /// descriptor yet is admitted: the worker makes it before the dispatch.
    pub const fn admits(&self) -> Result<(), Why> {
        match self.route.admits() {
            Err(cp::Why::StruckOut) => return Err(Why::Struck),
            Err(_) => return Err(Why::Poisoned),
            Ok(()) => {}
        }
        match self.desc {
            Desc::Absent | Desc::Creating | Desc::Ready { .. } => Ok(()),
            Desc::Uncovered => Err(Why::Uncovered),
            Desc::Draining => Err(Why::Retiring),
            Desc::Leaked => Err(Why::Poisoned),
        }
    }

    /// The descriptor's VA when a copy may be SUBMITTED now (dispatch time).
    pub const fn ready_va(&self) -> Result<u64, Why> {
        if self.route.is_disabled() {
            return Err(Why::Struck);
        }
        if self.route.is_poisoned() {
            return Err(Why::Poisoned);
        }
        match self.desc {
            Desc::Ready { va, .. } => Ok(va),
            Desc::Absent | Desc::Creating => Err(Why::NotReady),
            Desc::Uncovered => Err(Why::Uncovered),
            Desc::Draining => Err(Why::Retiring),
            Desc::Leaked => Err(Why::Poisoned),
        }
    }
}

/// What the retirement of a destination's descriptor does (`rm-copy-engine-present.md` 3: stop
/// new copies, wait for the last submitted value, free the mapping and the descriptor, unlock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirePlan {
    /// No descriptor: nothing on the host names the pages (the pin, if any, may go).
    Nothing,
    /// Copies may still write the pages: wait (bounded) until the completion value reaches this.
    Wait(u64),
    /// Nothing can write the pages: free the mapping and the descriptor, then unpin.
    Free,
    /// A copy that was poisoned may still write them, or a free was not confirmed: keep the pin
    /// until the transport generation ends.
    Leak,
}

/// The retirement step for `d` at completion value `completed`.
pub const fn retire_plan(d: &Dst, completed: u64) -> RetirePlan {
    match d.desc {
        Desc::Absent | Desc::Uncovered => RetirePlan::Nothing,
        Desc::Leaked => RetirePlan::Leak,
        Desc::Creating | Desc::Ready { .. } | Desc::Draining => match d.route.teardown(completed) {
            cp::Teardown::Unlock => RetirePlan::Free,
            cp::Teardown::WaitFor(v) => RetirePlan::Wait(v),
            cp::Teardown::Leak => RetirePlan::Leak,
        },
    }
}

/// Destination slots: each has its own handles and its own GPU window.
pub const MAX_DSTS: usize = 8;
/// The descriptor of slot `i` and its `NV50_MEMORY_VIRTUAL`, outside every other namespace of the
/// channel's client (`rm_ce_channel::H_*` 0x01..0x13 above `H_BASE`).
pub const H_DST_BASE: u32 = cc::H_BASE + 0x80;

pub const fn dst_handles(slot: u8) -> (u32, u32) {
    let h = H_DST_BASE + 2 * slot as u32;
    (h, h + 1)
}

/// The GPU window of slot `i`: 64 MiB windows from 32 windows above the channel's base (the ring
/// and the self-test's buffers use the first three).
pub const DST_VA_BASE: u64 = cc::VA_BASE + 32 * cc::VA_WINDOW;

pub const fn dst_va(slot: u8) -> u64 {
    DST_VA_BASE + slot as u64 * cc::VA_WINDOW
}

const _: () = assert!(dst_va(MAX_DSTS as u8 - 1) + cc::VA_WINDOW <= cp::MAX_VA);

/// Whether a destination of `cover` bytes fits one slot's window.
pub const fn fits_window(cover: u64) -> bool {
    cover != 0 && cover <= cc::VA_WINDOW && cover % 4096 == 0
}

// ── the OS descriptor's registration ─────────────────────────────────────────────────────────

/// `NV_ESC_RM_ALLOC_MEMORY` (`nv_escape.h`): the route librmclient's `crm_alloc_os_descriptor`
/// takes, and the one the host's `osdesc.rs` serves as `Registration::AllocMemory`.
pub const ESC_RM_ALLOC_MEMORY: u32 = 0x27;
/// `nv_ioctl_nvos02_parameters_with_fd`: the 48-byte `NVOS02_PARAMETERS` and the trailing fd.
pub const NVOS02_FD_BYTES: usize = 56;
/// `NVOS02_PARAMETERS.status`.
pub const NVOS02_STATUS_AT: usize = 40;
/// `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`.
pub const NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: u32 = 0x71;
/// librmclient's default flags: `LOCATION_PCI | COHERENCY_CACHED | PHYSICALITY_NONCONTIGUOUS |
/// MAPPING_NO_MAP` (`nv_ioctl_defs.h`, from `nvos.h`).
pub const NVOS02_OSDESC_FLAGS: u32 = (1 << 4) | (1 << 12) | (1 << 30);

/// The 56-byte block of an OS-descriptor registration: `hRoot` 0, `hObjectParent` 4 (the
/// device), `hObjectNew` 8, `hClass` 12, `flags` 16, `pMemory` 24 (the host replaces it with its
/// stitched alias of the page runs; any value here is only logged), `limit` 32 (`size - 1`),
/// `status` 40, the fd 48 (-1: none). Offsets as the 610.57.04 `osdesc` table has them
/// (`alloc_memory`: class at 12, address at 24, limit at 32, status at 40).
pub fn nvos02_osdesc(root: u32, device: u32, h_new: u32, addr: u64, size: u64) -> [u8; 56] {
    let mut b = [0u8; NVOS02_FD_BYTES];
    b[0..4].copy_from_slice(&root.to_le_bytes());
    b[4..8].copy_from_slice(&device.to_le_bytes());
    b[8..12].copy_from_slice(&h_new.to_le_bytes());
    b[12..16].copy_from_slice(&NV01_MEMORY_SYSTEM_OS_DESCRIPTOR.to_le_bytes());
    b[16..20].copy_from_slice(&NVOS02_OSDESC_FLAGS.to_le_bytes());
    b[24..32].copy_from_slice(&addr.to_le_bytes());
    b[32..40].copy_from_slice(&size.saturating_sub(1).to_le_bytes());
    b[48..52].copy_from_slice(&(-1i32).to_le_bytes());
    b
}

/// The PFNs of `[0, cover)` from the runs `guest_blob::build_runs` produced (whole pages, in
/// allocation order): what the KMD's page-run table encodes. `false`: `out` cannot hold them.
pub fn pfns_of_runs(runs: &[crate::guest_blob::Run], out: &mut [u64]) -> Option<usize> {
    let mut n = 0usize;
    for r in runs {
        if r.addr % 4096 != 0 || r.len % 4096 != 0 || r.len == 0 {
            return None;
        }
        let mut pfn = r.addr / 4096;
        for _ in 0..r.len / 4096 {
            *out.get_mut(n)? = pfn;
            n += 1;
            pfn += 1;
        }
    }
    Some(n)
}

// ── the jobs ─────────────────────────────────────────────────────────────────────────────────

/// What the worker made ready for a queued copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prep {
    /// Not yet looked at.
    Pending,
    /// The producer's semaphore entry and image base in the channel's VA space.
    Ready { sem_va: u64, src_va: u64 },
    /// It could not be made: the dispatch takes the Venus copy.
    Failed(Why),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    /// Queued in the WindowedBlt FIFO; the worker has not dispatched it.
    Queued,
    /// Submitted to the copy engine: done once the completion value reaches `value`.
    /// `t_submit` is the interrupt time (100 ns) of the doorbell.
    Submitted { value: u64, t_submit: u64 },
}

/// One copy routed to the copy engine, keyed by the WindowedBlt request `(token, boundary)` that
/// carries it (and whose Venus copy is the fallback).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job<P: Copy> {
    pub token: u64,
    pub boundary: u64,
    pub dst: u32,
    pub prep: Prep,
    pub state: JobState,
    pub payload: P,
}

/// The jobs, at most `N`.
pub struct Jobs<P: Copy, const N: usize> {
    slots: [Option<Job<P>>; N],
}

impl<P: Copy, const N: usize> Default for Jobs<P, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Copy, const N: usize> Jobs<P, N> {
    pub const fn new() -> Self {
        Self { slots: [None; N] }
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    pub fn has_room(&self) -> bool {
        self.slots.iter().any(Option::is_none)
    }

    /// Submitted and not yet retired.
    pub fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|j| matches!(j.state, JobState::Submitted { .. }))
            .count()
    }

    /// Add a queued job. `false`: full, or the key is already present.
    pub fn add(&mut self, token: u64, boundary: u64, dst: u32, payload: P) -> bool {
        if token == 0 || self.find(token, boundary).is_some() {
            return false;
        }
        match self.slots.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(Job {
                    token,
                    boundary,
                    dst,
                    prep: Prep::Pending,
                    state: JobState::Queued,
                    payload,
                });
                true
            }
            None => false,
        }
    }

    pub fn find(&self, token: u64, boundary: u64) -> Option<&Job<P>> {
        self.slots
            .iter()
            .flatten()
            .find(|j| j.token == token && j.boundary == boundary)
    }

    pub fn find_mut(&mut self, token: u64, boundary: u64) -> Option<&mut Job<P>> {
        self.slots
            .iter_mut()
            .flatten()
            .find(|j| j.token == token && j.boundary == boundary)
    }

    pub fn remove(&mut self, token: u64, boundary: u64) -> Option<Job<P>> {
        let slot = self.slots.iter_mut().find(|s| {
            s.as_ref()
                .is_some_and(|j| j.token == token && j.boundary == boundary)
        })?;
        slot.take()
    }

    /// The first queued job whose preparation is still pending.
    pub fn first_pending(&self) -> Option<&Job<P>> {
        self.slots
            .iter()
            .flatten()
            .find(|j| j.state == JobState::Queued && j.prep == Prep::Pending)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Job<P>> {
        self.slots.iter().flatten()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Job<P>> {
        self.slots.iter_mut().flatten()
    }

    /// Remove and return the first job `pred` selects.
    pub fn take_first(&mut self, pred: impl Fn(&Job<P>) -> bool) -> Option<Job<P>> {
        let slot = self
            .slots
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(&pred))?;
        slot.take()
    }

    /// Whether a SUBMITTED job targets `dst`.
    pub fn submitted_for(&self, dst: u32) -> bool {
        self.iter()
            .any(|j| j.dst == dst && matches!(j.state, JobState::Submitted { .. }))
    }

    /// Remove and return one submitted job `retire` decides for (`Retire` or `Discharge`) at
    /// `completed`; `timed_out(dst)` says whether the destination's poll timed out.
    pub fn take_settled(
        &mut self,
        completed: u64,
        timed_out: impl Fn(u32) -> bool,
    ) -> Option<(Job<P>, Retire)> {
        let slot = self.slots.iter_mut().find(|s| match s {
            Some(Job { state: JobState::Submitted { value, .. }, dst, .. }) => {
                cp::retire(*value, completed, timed_out(*dst)) != Retire::Wait
            }
            _ => false,
        })?;
        let job = slot.take()?;
        let JobState::Submitted { value, .. } = job.state else {
            return None;
        };
        Some((job, cp::retire(value, completed, timed_out(job.dst))))
    }

    /// Remove every job, calling `f` on each (a transport that is gone).
    pub fn drain(&mut self, mut f: impl FnMut(Job<P>)) {
        for s in self.slots.iter_mut() {
            if let Some(j) = s.take() {
                f(j);
            }
        }
    }
}

/// The jobs table's size: the WindowedBlt FIFO's depth is the real bound; this is enough for a
/// few windows each with a frame queued and one in flight.
pub const MAX_JOBS: usize = 16;

// ── the route's own strikes ──────────────────────────────────────────────────────────────────

/// Route strikes per transport generation: a channel that broke with copies in flight, or a copy
/// that timed out (the channel is then torn down). Three, and the route is off until the next
/// generation (`CeRtOff`).
pub const MAX_ROUTE_STRIKES: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chan {
    strikes: u8,
}

impl Chan {
    pub const fn new() -> Self {
        Self { strikes: 0 }
    }

    /// One more strike; whether the route is now off.
    pub fn strike(&mut self) -> bool {
        self.strikes = self.strikes.saturating_add(1);
        self.off()
    }

    pub const fn off(&self) -> bool {
        self.strikes >= MAX_ROUTE_STRIKES
    }

    pub const fn strikes(&self) -> u8 {
        self.strikes
    }
}

// ── deadlines ────────────────────────────────────────────────────────────────────────────────

/// How long a submitted copy may take before it is discharged (the destination poisoned, the
/// Present's DMA fence retired without it): `ce_present::TIMEOUT_AFTER_PRODUCER_MS`. The clock
/// starts at the dispatch, which the worker makes only once the producer's boundary is ready.
pub const COMPLETE_MS: u64 = cp::TIMEOUT_AFTER_PRODUCER_MS;
/// The drain of a destination being retired (lease change, destroy): at most this long for its
/// last copy, then it is leaked (pinned until the generation ends). The guest blob's `FENCE_MS`.
pub const DRAIN_MS: u64 = 250;
/// How long the paging path waits for the channel's RM I/O to be free before it gives up the free
/// (and leaks the descriptor until the channel's client is closed).
pub const IO_WAIT_MS: u64 = 250;
/// Each RM step of the retire (unmap, free), in one bounded section.
pub const FREE_MS: u32 = 1_000;
/// The worker's preparation of one job (dup/map, the descriptor), in one bounded section.
pub const PREP_MS: u32 = 2_000;
/// After a dispatch, the worker spins at most this long for the copy (about 0.2 to 0.35 ms for a
/// 1600x900 frame) before it goes back to its wait.
pub const SETTLE_SPIN_US: u64 = 750;
/// The worker's wait while copies are in flight or jobs need preparing (relative, 100 ns).
pub const POLL_DUE_100NS: i64 = -5_000;

/// The whole of one paging-path hook (a lease change or a destroy, content transaction held on
/// VidMm's paging thread): the drain, the wait for the channel's I/O and the free together, by the
/// interrupt-time clock. Each phase is also capped by its own bound above.
pub const LEASE_HOOK_MS: u64 = 1_000;

/// 100 ns units per millisecond (interrupt time).
pub const UNITS_PER_MS: u64 = 10_000;

/// The interrupt-time deadline `ms` from `now` (100 ns units), saturated.
pub const fn deadline(now: u64, ms: u64) -> u64 {
    now.saturating_add(ms.saturating_mul(UNITS_PER_MS))
}

/// Whether `end` has passed at `now`.
pub const fn expired(now: u64, end: u64) -> bool {
    now >= end
}

/// Whole milliseconds left until `end` at `now`, rounded up (0 once it passed). A bounded wait
/// sleeps, then reads the clock again: a `sleep_ms(1)` that rounds up to the timer quantum
/// (about 15.6 ms) overshoots the deadline by at most one quantum, never multiplies it.
pub const fn left_ms(now: u64, end: u64) -> u64 {
    if now >= end {
        0
    } else {
        (end - now).div_ceil(UNITS_PER_MS)
    }
}

/// The earlier of two deadlines.
pub const fn earlier(a: u64, b: u64) -> u64 {
    if a < b {
        a
    } else {
        b
    }
}

/// Why a submitted copy was discharged, for what it charges its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discharge {
    /// Its own deadline passed: the head of the channel (a producer whose semaphore never
    /// reached the record's value, or a hung copy). Strike and poison its destination.
    Own,
    /// Queued behind another destination's stalled copy, or in flight when the channel broke:
    /// no strike, no poison. Its pages stay pinned until the generation ends (RM may not have
    /// cancelled the copy), under a fresh descriptor the destination may route again.
    Bystander,
}

impl Discharge {
    /// `(strike, poison)` for the destination.
    pub const fn charges(self) -> (bool, bool) {
        match self {
            Discharge::Own => (true, true),
            Discharge::Bystander => (false, false),
        }
    }
}

/// Microseconds from `t0` to `t1` (interrupt time, 100 ns), saturated to `u32`.
pub const fn us(t0: u64, t1: u64) -> u32 {
    let d = t1.saturating_sub(t0) / 10;
    if d > u32::MAX as u64 {
        u32::MAX
    } else {
        d as u32
    }
}

// ── names ────────────────────────────────────────────────────────────────────────────────────

/// The counters the route writes, all in `kmd_render/src/ddi/ce_present_route.rs`. At most 14
/// characters, prefix `CeRt`, unique across `kmd_render` and `kmd_logic`.
pub const COUNTERS: &[&str] = &[
    // Presents that reached the decision; routed (queued for the copy engine); completed by it.
    "CeRtSeen",
    "CeRtRouted",
    "CeRtDone",
    // Fallbacks (Present time and dispatch time), the last reason, every reason seen; of them
    // the dispatch-time ones; refusals by the h_client rule.
    "CeRtFall",
    "CeRtWhy",
    "CeRtMask",
    "CeRtDispFall",
    "CeRtClient",
    // Strikes against destinations, destinations struck out, route strikes, route off for the
    // generation, poisoned destinations, destinations whose pages stay pinned, Presents
    // discharged at their own deadline, Presents discharged as bystanders of a channel failure,
    // lazy bring-ups asked for.
    "CeRtStrike",
    "CeRtStruck",
    "CeRtChStrike",
    "CeRtOff",
    "CeRtPoison",
    "CeRtLeak",
    "CeRtTimeout",
    "CeRtChFail",
    "CeRtUp",
    // In flight now and the most at once.
    "CeRtInfl",
    "CeRtPeak",
    // Per-stage microseconds (sums): the decision, the preparation (dup/map and descriptor), the
    // submit, dispatch (producer satisfied) to completion seen (and its maximum), the polls.
    "CeRtDecUs",
    "CeRtDupUs",
    "CeRtSubUs",
    "CeRtDoneUs",
    "CeRtDoneMax",
    "CeRtPollUs",
    // Destination descriptors created, dropped, live; page runs of the last one.
    "CeRtDstNew",
    "CeRtDstDrop",
    "CeRtDstLive",
    "CeRtRuns",
];

/// The files that write [`COUNTERS`] (relative to `kmd_render/src`).
pub const WRITERS: [&str; 1] = ["ddi/ce_present_route.rs"];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    // ── the decision ─────────────────────────────────────────────────────────────────────────

    fn ok_facts() -> Facts {
        Facts {
            boundary: true,
            record: true,
            client: ClientCheck::Owned,
            route_off: false,
            channel_up: true,
            source: Ok(()),
            destination: Ok(()),
            dst_state: Ok(()),
            room: true,
        }
    }

    /// An independent statement of the rule: the first refusal in the documented order.
    fn reference(f: &Facts) -> Result<(), Why> {
        let checks: [(bool, Why); 5] = [
            (f.boundary, Why::NoBoundary),
            (f.record, Why::NoRecord),
            (f.client == ClientCheck::Owned, Why::Client),
            (!f.route_off, Why::RouteOff),
            (f.channel_up, Why::ChannelDown),
        ];
        for (ok, why) in checks {
            if !ok {
                return Err(why);
            }
        }
        for r in [f.source, f.destination, f.dst_state] {
            r?;
        }
        if f.room {
            Ok(())
        } else {
            Err(Why::Full)
        }
    }

    #[test]
    fn the_decision_over_its_whole_input_space() {
        let clients = [ClientCheck::Owned, ClientCheck::NotOwned, ClientCheck::Unknown];
        let sources = [Ok(()), Err(Why::Source), Err(Why::Format), Err(Why::Extent)];
        let dsts = [
            Ok(()),
            Err(Why::Destination),
            Err(Why::Stale),
            Err(Why::Foreign),
            Err(Why::GuestBlob),
        ];
        let states = [
            Ok(()),
            Err(Why::Struck),
            Err(Why::Poisoned),
            Err(Why::Uncovered),
            Err(Why::Retiring),
        ];
        let mut routed = 0;
        let mut n = 0;
        for bits in 0..32u32 {
            for &client in &clients {
                for &source in &sources {
                    for &destination in &dsts {
                        for &dst_state in &states {
                            let f = Facts {
                                boundary: bits & 1 != 0,
                                record: bits & 2 != 0,
                                client,
                                route_off: bits & 4 != 0,
                                channel_up: bits & 8 != 0,
                                source,
                                destination,
                                dst_state,
                                room: bits & 16 != 0,
                            };
                            let d = decide(&f);
                            assert_eq!(d, reference(&f), "{f:?}");
                            if d.is_ok() {
                                routed += 1;
                                // Routed ONLY with every precondition.
                                assert_eq!(f, ok_facts());
                            }
                            n += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(routed, 1);
        assert_eq!(n, 32 * 3 * 4 * 5 * 5);
    }

    #[test]
    fn the_client_rule_comes_before_the_channel_and_the_destination() {
        let mut f = ok_facts();
        f.client = ClientCheck::NotOwned;
        f.channel_up = false;
        f.destination = Err(Why::Foreign);
        assert_eq!(decide(&f), Err(Why::Client));
        f.client = ClientCheck::Unknown;
        assert_eq!(decide(&f), Err(Why::Client), "unsure is refused, never assumed owned");
        // A missing record is reported before the client rule (nothing to judge).
        f.record = false;
        assert_eq!(decide(&f), Err(Why::NoRecord));
    }

    #[test]
    fn reason_codes_and_bits() {
        let mut mask = 0u32;
        for (i, w) in ALL_WHY.iter().enumerate() {
            assert_eq!(w.code() as usize, i + 1);
            assert_eq!(w.bit(), 1 << i);
            assert_eq!(mask & w.bit(), 0);
            mask |= w.bit();
            assert_eq!(w.at_dispatch(), w.code() >= 18);
        }
        let strikes: Vec<Why> = ALL_WHY.iter().copied().filter(|w| w.strikes()).collect();
        assert_eq!(strikes, [Why::Dup, Why::Desc, Why::Submit]);
        assert!(strikes.iter().all(|w| w.at_dispatch()));
    }

    // ── the destination ──────────────────────────────────────────────────────────────────────

    #[test]
    fn a_destination_admits_until_it_is_struck_or_poisoned() {
        let mut d = Dst::new(7, 0);
        assert_eq!(d.admits(), Ok(()));
        assert_eq!(d.ready_va(), Err(Why::NotReady));
        d.desc = Desc::Ready { va: dst_va(0), len: 4096 };
        assert_eq!(d.ready_va(), Ok(dst_va(0)));
        d.desc = Desc::Uncovered;
        assert_eq!(d.admits(), Err(Why::Uncovered));
        d.desc = Desc::Draining;
        assert_eq!(d.admits(), Err(Why::Retiring));
        assert_eq!(d.ready_va(), Err(Why::Retiring));
        d.desc = Desc::Leaked;
        assert_eq!(d.admits(), Err(Why::Poisoned));
        let mut d = Dst::new(7, 0);
        d.desc = Desc::Ready { va: dst_va(0), len: 4096 };
        for _ in 0..cp::MAX_STRIKES {
            assert!(d.admits().is_ok());
            d.route.on_failure(cp::Why::RmError);
        }
        assert_eq!(d.admits(), Err(Why::Struck));
        assert_eq!(d.ready_va(), Err(Why::Struck));
        // A timed-out copy poisons the destination at once.
        let mut d = Dst::new(7, 1);
        d.desc = Desc::Ready { va: dst_va(1), len: 4096 };
        d.route.on_submit(5, 4);
        d.route.on_producer_fired(0, 4);
        assert_eq!(d.route.poll(4, COMPLETE_MS + 1), cp::Poll::TimedOut);
        assert_eq!(d.admits(), Err(Why::Poisoned));
        assert_eq!(d.ready_va(), Err(Why::Poisoned));
    }

    #[test]
    fn the_retirement_order() {
        let mut d = Dst::new(7, 0);
        assert_eq!(retire_plan(&d, 0), RetirePlan::Nothing);
        d.desc = Desc::Ready { va: dst_va(0), len: 4096 };
        assert_eq!(retire_plan(&d, 0), RetirePlan::Free);
        d.route.on_submit(10, 3);
        // A copy in flight: wait for exactly its value; done: free.
        assert_eq!(retire_plan(&d, 3), RetirePlan::Wait(10));
        d.desc = Desc::Draining;
        assert_eq!(retire_plan(&d, 9), RetirePlan::Wait(10));
        assert_eq!(retire_plan(&d, 10), RetirePlan::Free);
        // Poisoned with the copy outstanding: never unlock.
        d.route.on_producer_fired(0, 3);
        let _ = d.route.poll(3, COMPLETE_MS + 1);
        assert_eq!(retire_plan(&d, 3), RetirePlan::Leak);
        // The late completion lifts it.
        assert_eq!(retire_plan(&d, 10), RetirePlan::Free);
        d.desc = Desc::Leaked;
        assert_eq!(retire_plan(&d, 10), RetirePlan::Leak);
        d.desc = Desc::Uncovered;
        assert_eq!(retire_plan(&d, 0), RetirePlan::Nothing);
        // A rebuilt channel counts from 1 again: the old ring's values are forgotten, strikes
        // stay, so a new descriptor is not held back by a value the new ring never reaches.
        let mut d = Dst::new(9, 3);
        d.desc = Desc::Ready { va: dst_va(3), len: 4096 };
        d.route.on_submit(500, 499);
        d.route.on_failure(cp::Why::RmError);
        assert_eq!(retire_plan(&d, 1), RetirePlan::Wait(500));
        d.route.on_channel_gone();
        assert_eq!(retire_plan(&d, 1), RetirePlan::Free);
        assert_eq!(d.route.strikes(), 1);
        assert!(!d.route.is_poisoned());
    }

    #[test]
    fn slots_have_their_own_handles_and_windows() {
        let mut hs = Vec::new();
        for s in 0..MAX_DSTS as u8 {
            let (m, v) = dst_handles(s);
            hs.push(m);
            hs.push(v);
            assert!(dst_va(s) % cc::VA_WINDOW == 0);
            assert!(dst_va(s) >= cc::VA_SELF_DST + cc::VA_WINDOW);
            assert!(dst_va(s) + cc::VA_WINDOW <= cp::MAX_VA);
        }
        let n = hs.len();
        hs.sort();
        hs.dedup();
        assert_eq!(hs.len(), n);
        for h in &hs {
            for c in [
                cc::H_VASPACE,
                cc::H_USERMODE,
                cc::H_CTL,
                cc::H_RING,
                cc::H_RING_VIRT,
                cc::H_TSG,
                cc::H_CTXSHARE,
                cc::H_CHANNEL,
                cc::H_CE,
                cc::H_SELF_SRC,
                cc::H_SELF_SRC_VIRT,
                cc::H_SELF_DST,
                cc::H_SELF_DST_VIRT,
                crate::rm_client::H_DEVICE,
                crate::rm_client::H_SUBDEVICE,
            ] {
                assert_ne!(*h, c);
            }
        }
        // 1600x900x4 rounded to pages fits; 4K BGRA (33 MB) fits; 64 MiB + a page does not.
        assert!(fits_window(1407 * 4096));
        assert!(fits_window(3840 * 2160 * 4));
        assert!(!fits_window(cc::VA_WINDOW + 4096));
        assert!(!fits_window(0));
    }

    #[test]
    fn the_os_descriptor_block_is_librmclients() {
        let (h, _) = dst_handles(2);
        let b = nvos02_osdesc(0xc1d0_0001, crate::rm_client::H_DEVICE, h, 0x1234_5000, 1407 * 4096);
        let rd32 = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        let rd64 = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        assert_eq!(b.len(), 56);
        assert_eq!(rd32(0), 0xc1d0_0001);
        assert_eq!(rd32(4), crate::rm_client::H_DEVICE);
        assert_eq!(rd32(8), dst_handles(2).0);
        assert_eq!(rd32(12), 0x71);
        // LOCATION_PCI (0) | COHERENCY_CACHED (1 << 12) | NONCONTIGUOUS (1 << 4) | NO_MAP (1 << 30)
        assert_eq!(rd32(16), 0x4000_1010);
        assert_eq!(rd32(20), 0);
        assert_eq!(rd64(24), 0x1234_5000);
        assert_eq!(rd64(32), 1407 * 4096 - 1);
        assert_eq!(rd32(NVOS02_STATUS_AT), 0);
        assert_eq!(rd32(44), 0);
        assert_eq!(rd32(48), 0xffff_ffff);
        assert_eq!(rd32(52), 0);
        // The ioctl number librmclient sends: _IOWR('F', 0x27, 56).
        assert_eq!(crate::rm_client::nv_cmd(ESC_RM_ALLOC_MEMORY, 56), 0xc038_4627);
        assert_eq!(ESC_RM_ALLOC_MEMORY, crate::nvrm_clients::NV_ESC_RM_ALLOC_MEMORY);
    }

    #[test]
    fn pfns_from_runs() {
        use crate::guest_blob::Run;
        let runs = [Run { addr: 0x10_0000, len: 3 * 4096 }, Run { addr: 0x40_0000, len: 4096 }];
        let mut out = [0u64; 8];
        assert_eq!(pfns_of_runs(&runs, &mut out), Some(4));
        assert_eq!(&out[..4], &[0x100, 0x101, 0x102, 0x400]);
        let mut small = [0u64; 3];
        assert_eq!(pfns_of_runs(&runs, &mut small), None);
        assert_eq!(pfns_of_runs(&[Run { addr: 0x10_0800, len: 4096 }], &mut out), None);
    }

    // ── the jobs ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn jobs_are_keyed_by_token_and_stream() {
        let mut j: Jobs<u8, 3> = Jobs::new();
        assert!(j.is_empty());
        assert!(j.add(1, 0xa, 7, 0));
        assert!(!j.add(1, 0xa, 7, 0), "the same request twice");
        assert!(j.add(1, 0xb, 8, 0), "the same token of another stream is another request");
        assert!(!j.add(0, 0xb, 8, 0), "token 0 is never a request");
        assert!(j.add(2, 0xa, 7, 0));
        assert!(!j.has_room());
        assert!(!j.add(3, 0xa, 7, 0));
        assert_eq!(j.first_pending().map(|x| x.token), Some(1));
        j.find_mut(1, 0xa).unwrap().prep = Prep::Ready { sem_va: 1, src_va: 2 };
        assert_eq!(j.first_pending().map(|x| (x.token, x.boundary)), Some((1, 0xb)));
        assert_eq!(j.remove(1, 0xb).map(|x| x.dst), Some(8));
        assert!(j.has_room());
        assert_eq!(j.len(), 2);
    }

    #[test]
    fn a_submitted_job_settles_only_on_its_value_or_its_timeout() {
        let mut j: Jobs<u8, 4> = Jobs::new();
        assert!(j.add(1, 0xa, 7, 0));
        assert!(j.add(2, 0xa, 8, 0));
        j.find_mut(1, 0xa).unwrap().state = JobState::Submitted { value: 5, t_submit: 0 };
        j.find_mut(2, 0xa).unwrap().state = JobState::Submitted { value: 6, t_submit: 0 };
        assert_eq!(j.in_flight(), 2);
        assert!(j.submitted_for(7) && j.submitted_for(8) && !j.submitted_for(9));
        // Nothing at 4; queued jobs never settle.
        assert!(j.take_settled(4, |_| false).is_none());
        let (a, r) = j.take_settled(5, |_| false).unwrap();
        assert_eq!((a.token, r), (1, Retire::Retire));
        assert!(j.take_settled(5, |_| false).is_none());
        // A timed-out destination discharges its copy.
        let (b, r) = j.take_settled(5, |d| d == 8).unwrap();
        assert_eq!((b.token, r), (2, Retire::Discharge));
        assert!(j.is_empty());
        // A late watermark retires (not discharges) even with the timeout flag.
        assert!(j.add(3, 0xa, 8, 0));
        j.find_mut(3, 0xa).unwrap().state = JobState::Submitted { value: 9, t_submit: 0 };
        assert_eq!(j.take_settled(9, |_| true).map(|x| x.1), Some(Retire::Retire));
        let mut n = 0;
        assert!(j.add(4, 0xa, 8, 0));
        j.drain(|_| n += 1);
        assert_eq!(n, 1);
        assert!(j.is_empty());
        // `take_first` takes the first match only; `iter_mut` reaches every job.
        assert!(j.add(5, 0xa, 1, 0));
        assert!(j.add(6, 0xa, 2, 0));
        assert!(j.add(7, 0xa, 2, 0));
        for x in j.iter_mut() {
            x.prep = Prep::Failed(Why::ChannelLost);
        }
        assert!(j.iter().all(|x| x.prep == Prep::Failed(Why::ChannelLost)));
        assert_eq!(j.take_first(|x| x.dst == 2).map(|x| x.token), Some(6));
        assert_eq!(j.take_first(|x| x.dst == 2).map(|x| x.token), Some(7));
        assert!(j.take_first(|x| x.dst == 2).is_none());
        assert_eq!(j.len(), 1);
    }

    #[test]
    fn three_route_strikes_turn_the_route_off() {
        let mut c = Chan::new();
        assert!(!c.off());
        assert!(!c.strike());
        assert!(!c.strike());
        assert!(c.strike());
        assert!(c.off());
        assert_eq!(c.strikes(), 3);
    }

    #[test]
    fn deadlines_are_clock_time() {
        let now = 1_000_000;
        let end = deadline(now, 250);
        assert_eq!(end, now + 250 * UNITS_PER_MS);
        assert!(!expired(now, end));
        assert_eq!(left_ms(now, end), 250);
        // A partial millisecond rounds up; a passed deadline leaves 0.
        assert_eq!(left_ms(end - 1, end), 1);
        assert_eq!(left_ms(end, end), 0);
        assert!(expired(end, end));
        assert_eq!(left_ms(end + 5, end), 0);
        assert_eq!(deadline(u64::MAX - 3, 10), u64::MAX);
        assert_eq!(earlier(5, 7), 5);
        assert_eq!(earlier(7, 5), 5);
        // A loop that sleeps a 15.6 ms quantum per turn and re-reads the clock ends within one
        // quantum of the deadline, however many turns a counted loop would have made.
        let quantum = 156_000u64;
        let mut t = now;
        let mut turns = 0;
        while !expired(t, end) {
            t += quantum;
            turns += 1;
        }
        assert!(t - end < quantum);
        assert_eq!(turns, 17);
        // The paging hook's phases fit its whole bound.
        assert!(DRAIN_MS + IO_WAIT_MS <= LEASE_HOOK_MS);
    }

    #[test]
    fn a_bystander_is_never_charged() {
        assert_eq!(Discharge::Own.charges(), (true, true));
        assert_eq!(Discharge::Bystander.charges(), (false, false));
    }

    #[test]
    fn microseconds() {
        assert_eq!(us(0, 10), 1);
        assert_eq!(us(10, 0), 0);
        assert_eq!(us(0, u64::MAX), u32::MAX);
        assert!(POLL_DUE_100NS <= -crate::hpd_wake::MIN_WAIT_100NS);
    }

    // ── names ────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("CeRt"), "{n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        for n in COUNTERS {
            assert!(!cp::COUNTERS.contains(n), "{n}");
            assert!(!cc::COUNTERS.contains(n), "{n}");
            assert!(!crate::ce_record::COUNTERS.contains(n), "{n}");
            assert!(!crate::blt_async::COUNTERS.contains(n), "{n}");
            assert!(!crate::guest_blob::COUNTERS.contains(n), "{n}");
            assert!(!crate::onscanout::COUNTERS.contains(n), "{n}");
            assert_ne!(*n, cp::KNOB);
        }
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(render) = render_src() else {
            return;
        };
        let mut written = Vec::new();
        for f in WRITERS {
            written.extend(literals(&std::fs::read_to_string(render.join(f)).unwrap()));
        }
        for n in COUNTERS {
            assert!(written.iter().any(|l| l == n), "{n} is listed but not written by {WRITERS:?}");
        }
        for l in written.iter().filter(|l| l.starts_with("Ce")) {
            assert!(COUNTERS.contains(&l.as_str()), "{l} is written by {WRITERS:?} but not listed");
        }
        // No other file spells these names (nor any `CeRt` name).
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().replace('\\', "/");
                    if WRITERS.iter().any(|w| s.ends_with(w)) {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(!text.contains("b\"CeRt"), "{s} spells a CeRt counter name");
                }
            }
        }
        assert!(checked > 20);
    }
}

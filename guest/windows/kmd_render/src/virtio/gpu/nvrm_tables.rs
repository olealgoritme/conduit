//! Ownership of what a process holds through `HELIOS_ESCAPE_NVRM`: backend RM
//! handles (a forwarded `Open`), CPU mappings of host memory (`MMAP`) and pinned
//! user pages (`PIN`).
//!
//! The host hands out one handle per opened RM file (`/dev/nvidiactl`, a GPU, a
//! DRM node, UVM). Without a table here any process could name any other's
//! handle in a forwarded `Ioctl`/`Close`/`Mmap`, so a handle is recorded against
//! the device that opened it and every handle-taking request is checked against
//! it. The tables also let device teardown release what a crashed process left
//! behind — the host keeps those objects, and their VRAM, until the file is
//! closed, and pinned pages stay locked until someone unlocks them.
//!
//! Field-disjoint from the control queue and the fence tables, like
//! `resource_tables`; capacity is reserved at init so no push allocates under the
//! spinlock, and handle slots are reserved before the wire round trip and
//! committed after it so "open on the host but untracked" cannot exist for a
//! refused open.
//!
//! Anything with a `Drop` that must run at PASSIVE (a locked MDL, a contiguous
//! buffer) is never dropped under the lock: the `take_*` methods hand it back by
//! value and the caller releases it after the lock is gone.

use super::*;
use alloc::boxed::Box;
use helios_kmd_logic::nvrm_clients::{ClientTable, Commit, Verdict};
use helios_kmd_logic::nvrm_fence::{is_fence as is_fence_type, Noted, DEVICE_TYPE_FENCE};
use helios_kmd_logic::rm_fence_present::{same_process, Attach, FenceMeta};
use helios_kmd_logic::rm_limits::{self, Admit, Bounds};
use helios_kmd_logic::rm_window::{self, Account, Policy};
use helios_kmd_logic::sweep_budget::{PinAction, PinFate};

/// The sanity bounds of the handle and mapping tables, and where they start: the shapes are
/// `helios_kmd_logic::rm_limits::{HANDLES, MAPS}` (host-tested there). The tables GROW from
/// 1024 slots (what they always held) up to the global bound, at PASSIVE and outside the lock
/// (`grow_nvrm_tables`); the bounds are far above anything a real client reaches and are there
/// so a hostile process cannot take the non-paged pool. A bound that is hit is counted
/// (`NvHdlORef`, `NvHdlGRef`, `NvHdlFRef`, `NvMapTRef`, `NvSanityRef`). `NvWinPolicy` = 0 puts
/// the old fixed numbers back (`rm_limits::*_LEGACY`), nothing grows. The table:
/// `docs/nvrm-escape.md` section 5.
///
/// Backend handles tracked across every process (fence handles included), at most.
pub const MAX_NVRM_HANDLES: usize = rm_limits::HANDLES.global_max;
/// Most one process may hold open at once (`QUERY_CAPS` reports the bound in force).
pub const MAX_NVRM_HANDLES_PER_OWNER: usize = rm_limits::HANDLES.per_owner_max;
/// Most fence handles the KMD holds at once as its own (attached to a present, or
/// discarded and owed a `Close`), across every process. A fence moves out of its
/// creator's per-process quota when the KMD takes it, and into this one: gates hold
/// up to 8 x 128 points, so without a bound of its own the table could fill with
/// them (and the KMD's RM client, which shares the `KMD_RM` owner, would be refused
/// its `Open`s).
pub const MAX_NVRM_ATTACHED_FENCES: usize = 512;
/// Live `MMAP` mappings across every process, and per process (sanity bounds). The user
/// views live in `AdapterContext::mappings`, one adapter-wide table of 8192 entries shared
/// with every blob view (`mapping.rs`, `MAX_MAPPINGS`): a bound above it could never be
/// reached, so this is it (a view refused there is counted as `NvWinRTab`).
pub const MAX_NVRM_MAPS: usize = rm_limits::MAPS.global_max;
pub const MAX_NVRM_MAPS_PER_OWNER: usize = rm_limits::MAPS.per_owner_max;
/// Owners the window account can hold at once (rows), see `rm_window::Account`.
const NVRM_WINDOW_OWNER_ROWS: usize = 512;

/// Most live pins across every process, and per process.
pub const MAX_NVRM_PINS: usize = 1024;
pub const MAX_NVRM_PINS_PER_OWNER: usize = 256;
/// Most pages one pin may lock (just under 1 GiB). Also keeps the page-run table
/// within `page_runs::INDIRECT_MAX_RUNS`, whatever the scatter.
pub const MAX_NVRM_PIN_PAGES: usize = 262_143;

extern "C" {
    /// `MmUnlockPages` + `IoFreeMdl` (`src/seh_shim.c`); callable from any process
    /// context for a user MDL that was never mapped (the pins never are).
    fn helios_unlock_system_buffer(mdl: wdk_sys::PMDL);
    /// Take one reference on the CALLING process's EPROCESS and return it
    /// (`src/seh_shim.c`); balanced by exactly one `helios_dereference_process`.
    fn helios_reference_current_process() -> *mut core::ffi::c_void;
    /// Drop a reference `helios_reference_current_process` returned. PASSIVE, any
    /// process context.
    fn helios_dereference_process(process: *mut core::ffi::c_void);
}

/// One tracked handle.
pub(super) struct NvrmHandleSlot {
    owner: DeviceOwner,
    handle: u32,
    /// The host `device_type` the handle was opened with (255 = control, a GPU
    /// minor, 256 = UVM, 257 = UVM tools, 512+ = DRM); decides which region an
    /// `Mmap` is in.
    device_type: u32,
    /// The host said the file was readable (`EventReady`) while no event was
    /// registered for it; the next `EVENT_REGISTER` consumes it and signals at
    /// once. A flag, not a count: the consumer drains until empty.
    ready_latched: bool,
    /// Fence handles only: who created it, how it fired, what it was attached to
    /// (`rm-fence-marker.md`). Inert (`FenceMeta::new(0)`) for every other kind.
    fence: FenceMeta,
}

/// One live mapping, for the host `Munmap` at `Close` / teardown.
///
/// `kmd_id` is OURS: unique, nonzero, the key in `AdapterContext::mappings` and
/// what the ABI hands out as the mapping id. `host_id` is what the host answered
/// and is NOT unique — the RM path replies 0 for every mapping, and a repeat
/// mapping of the same DRM object can reply the same nonzero id.
pub(super) struct NvrmMapSlot {
    owner: DeviceOwner,
    handle: u32,
    kmd_id: u32,
    host_id: u32,
    /// Bytes mapped (the request's size, page multiple).
    size: u64,
    /// Whether it is a view of the UVM aperture (region 2) rather than the RM
    /// window (region 1): only the window is subject to the byte quota.
    uvm: bool,
}

/// One pin: user pages locked for an OS-descriptor registration, and the
/// page-run table that names them to the host.
///
/// Dropping a pin UNLOCKS its pages, at PASSIVE: the lock is released exactly
/// once, by whatever path ends up owning the value last. That is what makes a
/// transport torn down with pins still in its table (`StopDevice` before the
/// owners' `DestroyDevice`) release them instead of leaving user pages locked.
/// Nothing here may drop one under the virtio lock: the `take_*` methods hand it
/// back by value.
///
/// # A pin that is deliberately NOT dropped ([`NvrmPin::leak`])
///
/// When the host may still alias the pages (a `FORWARD` claimed the table and the
/// host never confirmed closing the RM files that hold the GPU mapping), unlocking
/// them would let the guest reuse that RAM underneath the GPU. Such a pin is
/// leaked: the pages stay locked. Locked user pages make the kernel bugcheck when
/// the owning process's address space is torn down (0x76
/// `PROCESS_HAS_LOCKED_PAGES`), so a leak alone trades DMA into reused memory for
/// a bugcheck at the owner's exit.
///
/// To soften that, every pin holds one reference on its owning process's EPROCESS,
/// taken in [`NvrmPin::new`] (the pinning thread, in the owning process) and
/// released by `Drop` AFTER the unlock. A leaked pin never drops, so it keeps the
/// process object alive: the process can end (threads gone, handles closed) but
/// its EPROCESS stays referenced, a zombie. The intent is that the locked-pages
/// check, which this code believes runs when the process OBJECT is deleted
/// (`PspProcessDelete` -> `MmDeleteProcessAddressSpace`), never runs.
///
/// NOT VERIFIED: that belief is from memory of the NT sources, not from a test or
/// a reading of this kernel's symbols. If the check instead runs when the last
/// thread exits (`PspExitProcess` -> `MmCleanProcessAddressSpace`), the extra
/// reference changes nothing and a leaked pin still bugchecks 0x76 at the owner's
/// exit (argument 2 the process, argument 3 the locked-page count). That outcome
/// is what happened before the reference existed, so the reference cannot make
/// it worse; a pin that unlocks normally drops its reference at once, so the
/// normal path gains no zombie.
pub struct NvrmPin {
    owner: DeviceOwner,
    handle: u32,
    id: u32,
    h_root: u32,
    h_object: u32,
    /// The locked MDL (a `PMDL` as an address); unlocked with
    /// `helios_unlock_system_buffer` at PASSIVE.
    pub(crate) mdl: usize,
    /// `HELIOS_NVRM_DEEP_PAGE_RUNS` or `_INDIRECT`: what goes in `deep_ptr_offset`.
    pub(crate) deep_kind: u32,
    /// The deep block the message carries: the whole run table (direct), or the
    /// one-run table that says where the big one lives (indirect).
    pub(crate) deep: Box<[u8]>,
    /// The indirect case's big table, in contiguous non-paged memory that must
    /// outlive the pin. PASSIVE-only to drop.
    pub(crate) big: Option<DmaBuffer>,
    /// A `FORWARD` has claimed the table: the GPU may hold the pages.
    used: bool,
    pub(crate) npages: u32,
    /// The owning process's EPROCESS (an address; one reference is held, see the
    /// type docs), or 0 if there was none. Released only by `Drop`, after the
    /// unlock, and never for a leaked pin.
    process: usize,
}

impl NvrmPin {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner: DeviceOwner,
        handle: u32,
        h_root: u32,
        h_object: u32,
        mdl: usize,
        deep_kind: u32,
        deep: Box<[u8]>,
        big: Option<DmaBuffer>,
        npages: u32,
    ) -> Self {
        // SAFETY: PASSIVE (the pin escape's contract), in the process that just
        // locked `mdl`, so the reference is on THAT process. The matching
        // dereference is in `Drop`, which runs once.
        let process = unsafe { helios_reference_current_process() } as usize;
        Self {
            owner,
            handle,
            id: 0,
            h_root,
            h_object,
            mdl,
            deep_kind,
            deep,
            big,
            used: false,
            npages,
            process,
        }
    }

    /// Leave this pin's pages locked for good, and its process referenced: the
    /// host may still alias them. Counted (`NvPinLeak`). The value is consumed
    /// without running `Drop`, so neither the unlock nor the process dereference
    /// happens, and the MDL, table buffer and EPROCESS stay allocated until the
    /// next boot.
    pub fn leak(self) {
        crate::virtio::nvrm::NVRM_PIN_LEAKS.fetch_add(1, Ordering::Relaxed);
        core::mem::forget(self);
    }

    /// A `FORWARD` has claimed this pin's table, so the host (and through it the
    /// GPU) may hold an alias of its pages until it closes the RM files involved.
    /// A pin nothing claimed was never described to the host.
    pub fn host_may_alias(&self) -> bool {
        self.used
    }
}

/// What a fence's `EventReady` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FenceFire {
    /// Not a fence the KMD tracks.
    NotFence,
    /// It had fired already: ignored.
    Repeat,
    /// The first fire, with what the fence was attached to.
    Fired(Attach),
}

/// Who claims a fence for a carrier.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FenceClaim {
    /// An NVRM owner (carrier (a): the source's owner).
    Owner(DeviceOwner),
    /// A process, by its `hKmdProcess` (carrier (b)): the fence was created in it.
    Process(usize),
}

/// Why a fence could not be taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceRefusal {
    NotOwned,
    NotFence,
    AlreadyAttached,
    /// [`MAX_NVRM_ATTACHED_FENCES`] fences are the KMD's already.
    Quota,
}

/// What `commit_nvrm_fence` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceCommit {
    /// Recorded as the caller's. `fired`: its `EventReady` had already arrived and
    /// is latched.
    Recorded { fired: bool },
    /// The number is already a live handle: nothing was recorded.
    Duplicate,
}

impl Drop for NvrmPin {
    fn drop(&mut self) {
        // SAFETY: `mdl` is the locked MDL `helios_lock_user_pages_seh` returned,
        // owned by this pin alone (no other table or copy holds it) and released
        // exactly once, here. PASSIVE by this type's contract; the user MDL has
        // no system mapping, so no process context is required. The contiguous
        // table buffer (`big`) is freed by the field drop that follows.
        unsafe { helios_unlock_system_buffer(self.mdl as wdk_sys::PMDL) };
        crate::virtio::nvrm::NVRM_UNPINS.fetch_add(1, Ordering::Relaxed);
        // After the unlock: `MmUnlockPages` of a user MDL reaches the process it
        // was locked in, which this reference keeps valid. Released here and
        // nowhere else (`leak` forgets the value instead), exactly once, at
        // PASSIVE, in whichever process context this runs.
        if self.process != 0 {
            // SAFETY: the reference `new` took, not yet released.
            unsafe { helios_dereference_process(self.process as *mut core::ffi::c_void) };
            self.process = 0;
        }
    }
}

/// What `take_nvrm_unused_pin` found.
pub enum PinTake {
    Taken(NvrmPin),
    /// A `FORWARD` already used it: only the KMD releases it now.
    InUse,
    NotFound,
}

/// Build the fence book in its own (popped) frame; see the field's comment.
#[inline(never)]
pub(super) fn new_fence_book() -> Box<helios_kmd_logic::nvrm_fence::FenceBook> {
    Box::new(helios_kmd_logic::nvrm_fence::FenceBook::new())
}

/// Build the client table directly on the heap, zeroed: `Box::new(ClientTable::new())` would
/// build the 4 KiB value on `VirtioGpu::init`'s stack first. The all-zero pattern is the empty
/// table (`an_all_zero_table_is_an_empty_table` in `kmd_logic` pins it).
#[inline(never)]
pub(super) fn new_client_table() -> Box<ClientTable> {
    let layout = core::alloc::Layout::new::<ClientTable>();
    // SAFETY: `layout` has a nonzero size. A null result is turned into the allocation error
    // handler, as `Box::new` does. The pointer is valid for `ClientTable`, whose all-zero bytes are
    // a valid (empty) table, and it came from the global allocator with this exact layout, which
    // is what `Box::from_raw` requires.
    unsafe {
        let p = alloc::alloc::alloc_zeroed(layout) as *mut ClientTable;
        if p.is_null() {
            alloc::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(p)
    }
}

/// The handle and mapping tables' bounds for `policy` (the shapes live in
/// `helios_kmd_logic::rm_limits`, where the host tests hold them to their promises).
pub(super) fn table_bounds(policy: Policy) -> (Bounds, Bounds, Bounds) {
    match policy {
        Policy::Dynamic => (rm_limits::HANDLES, rm_limits::MAPS, rm_limits::EVENTS),
        Policy::Legacy => (
            rm_limits::HANDLES_LEGACY,
            rm_limits::MAPS_LEGACY,
            rm_limits::EVENTS_LEGACY,
        ),
    }
}

/// The window account and both tables' bounds, in ONE box. `VirtioGpu::init` builds the
/// transport by value on a boot-stack frame that was already at its budget (17936 bytes was the
/// last good nested pair, 18800 did not boot: `tools/kmd-frame-sizes.ps1`): holding the two
/// `Bounds` (64 bytes each) inline, or returning them next to a `Box`, would grow it.
pub(super) struct NvrmLimits {
    /// Who holds how many bytes of the RM window, the reserve, the refusals.
    pub(super) acct: Account,
    /// Sanity bounds and growth rules of the handle and mapping tables (the legacy fixed numbers
    /// under `NvWinPolicy` = 0).
    pub(super) handle_bounds: Bounds,
    pub(super) map_bounds: Bounds,
    /// The event registry's shape (derived from the handle table's).
    pub(super) event_bounds: Bounds,
}

/// The window account, the policy it runs and both tables' bounds, from the knobs
/// (`NvWinPolicy`, `NvWinReserveMb`, `NvWinMaxMb`) and the window the device reported.
/// PASSIVE (transport init). `None` when the owner rows cannot be allocated.
///
/// `#[inline(never)]`, and it returns one pointer: its locals (the registry reads, the config)
/// live and die in its own frame, not in `VirtioGpu::init`'s.
#[inline(never)]
pub(super) fn new_window_account(window: Option<HostVisibleWindow>) -> Option<Box<NvrmLimits>> {
    use crate::diag::{knobs, read_config_dword};
    let policy = Policy::from_knob(read_config_dword(knobs::NV_WIN_POLICY, 1));
    let cfg = rm_window::Config::new(
        window.map_or(0, |w| w.len),
        read_config_dword(knobs::NV_WIN_RESERVE_MB, rm_window::DEFAULT_RESERVE_MIB),
        read_config_dword(knobs::NV_WIN_MAX_MB, 0),
        policy,
    );
    let acct = Account::new(cfg, NVRM_WINDOW_OWNER_ROWS)?;
    let (handle_bounds, map_bounds, event_bounds) = table_bounds(policy);
    crate::virtio::nvrm_window::configure(
        &cfg,
        handle_bounds.per_owner_max,
        map_bounds.per_owner_max,
    );
    crate::virtio::nvrm_window::HDL_CAP.store(handle_bounds.initial as u32, Ordering::Relaxed);
    crate::virtio::nvrm_window::MAP_CAP.store(map_bounds.initial as u32, Ordering::Relaxed);
    Some(Box::new(NvrmLimits {
        acct,
        handle_bounds,
        map_bounds,
        event_bounds,
    }))
}

/// Give the handle table and the mapping table the room their next reservation wants.
///
/// PASSIVE, outside every lock, BEFORE the reservation (`reserve_nvrm_handle_slot`,
/// `begin_nvrm_fence_create`, a `MMAP`): the new storage is allocated here and only SWAPPED
/// in under the lock, so nothing allocates (or frees) with the spinlock held. A table
/// that wants to grow and cannot (the allocator refuses, or it is at its bound) is left
/// alone: the reservation then refuses by the bound (counted) or, for the allocator, as
/// `NvTblOom`. Cheap when nothing is wanted: one lock hold, two comparisons.
pub fn grow_nvrm_tables(adapter: &crate::adapter::AdapterContext) {
    /// New storage for `n` entries, counted `NvTblOom` when the allocator refuses.
    fn fresh<T>(n: Option<usize>) -> Option<Vec<T>> {
        let n = n?;
        let mut v = Vec::new();
        match v.try_reserve_exact(n) {
            Ok(()) => Some(v),
            Err(_) => {
                crate::virtio::nvrm_window::TBL_OOM.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
    // Each step at least doubles a table, so a few suffice for the 16x between initial and
    // bound; the loop is bounded in any case.
    for _ in 0..6 {
        let wants = adapter.with_virtio(|v| {
            (
                v.nvrm_handles_want(),
                v.nvrm_maps_want(),
                v.nvrm_events_want(),
            )
        });
        let Ok((h, m, e)) = wants else {
            return;
        };
        if h.is_none() && m.is_none() && e.is_none() {
            return;
        }
        let fresh_h = fresh::<NvrmHandleSlot>(h);
        let fresh_m = fresh::<NvrmMapSlot>(m);
        let fresh_e = e.and_then(|n| {
            let s = helios_kmd_logic::nvrm_events::Registry::<core::ptr::NonNull<wdk_sys::KEVENT>>::spare(n);
            if s.is_none() {
                crate::virtio::nvrm_window::TBL_OOM.fetch_add(1, Ordering::Relaxed);
            }
            s
        });
        if fresh_h.is_none() && fresh_m.is_none() && fresh_e.is_none() {
            return;
        }
        // The swap is under the lock; the old (now empty) storage comes back and is freed
        // here, outside it.
        let old = adapter.with_virtio(|v| {
            (
                v.nvrm_handles_install(fresh_h),
                v.nvrm_maps_install(fresh_m),
                v.nvrm_events_install(fresh_e),
            )
        });
        drop(old);
    }
}

impl VirtioGpu {
    // ---- RM clients (cross-client hardening) -----------------------------------------
    //
    // `kmd_logic::nvrm_clients` holds the rules; these are the table's doors, all short and
    // allocation-free (they run under the virtio spinlock). The glue is
    // `virtio/nvrm_harden.rs`.

    /// Judge a forwarded `Ioctl` request of `owner` on a backend file of `device_type`:
    /// does every client and backend handle it names belong to `owner`? Called in the
    /// same lock hold that resolved the file's `device_type`.
    pub fn nvrm_judge(&self, owner: DeviceOwner, device_type: u32, req: &[u8]) -> Verdict {
        helios_kmd_logic::nvrm_clients::judge(
            &self.nvrm_clients,
            owner.raw(),
            device_type,
            req,
            |h| self.nvrm_handle_owned(owner, h),
        )
    }

    /// Whether `owner` was given RM client `client`.
    pub fn nvrm_client_owned(&self, owner: DeviceOwner, client: u32) -> bool {
        self.nvrm_clients.is_client_owned_by(owner.raw(), client)
    }

    /// Promise a table slot to a client allocation about to be forwarded. `false`: full.
    pub fn reserve_nvrm_client(&mut self, owner: DeviceOwner) -> bool {
        self.nvrm_clients.reserve(owner.raw())
    }

    /// Give `owner`'s promised slot back (the allocation failed).
    pub fn cancel_nvrm_client(&mut self, owner: DeviceOwner) {
        self.nvrm_clients.cancel(owner.raw());
    }

    /// Record the client RM made for `owner` through file `via`; `reserved` consumes the
    /// promise [`Self::reserve_nvrm_client`] made.
    pub fn commit_nvrm_client(
        &mut self,
        owner: DeviceOwner,
        via: u32,
        client: u32,
        reserved: bool,
    ) -> Commit {
        self.nvrm_clients.commit(owner.raw(), via, client, reserved)
    }

    /// Forget one client of `owner` (its free went through). `false`: not its client.
    pub fn forget_nvrm_client(&mut self, owner: DeviceOwner, client: u32) -> bool {
        self.nvrm_clients.forget_client(owner.raw(), client)
    }

    /// Forget every client of `owner` made through file `via` (the file closed).
    pub fn forget_nvrm_clients_via(&mut self, owner: DeviceOwner, via: u32) -> u32 {
        self.nvrm_clients.forget_via(owner.raw(), via)
    }

    /// Forget every client of `owner` (its device is destroyed).
    pub fn forget_nvrm_clients_for_owner(&mut self, owner: DeviceOwner) -> u32 {
        self.nvrm_clients.forget_owner(owner.raw())
    }

    /// Forget every client of every owner (the transport's handles are all closed).
    pub fn clear_nvrm_clients(&mut self) -> u32 {
        self.nvrm_clients.clear()
    }

    // ---- handles -----------------------------------------------------------------

    /// Reserve a tracking slot for an in-flight `Open`. Refuses when the table
    /// or this owner's quota is full, BEFORE the host is asked to open anything.
    pub fn reserve_nvrm_handle_slot(&mut self, owner: DeviceOwner) -> bool {
        // A fence the KMD took over for a present is not the owner's: it counts against
        // `MAX_NVRM_ATTACHED_FENCES`, not against the `KMD_RM` owner's Opens.
        let mine = self
            .nvrm_handles
            .iter()
            .filter(|s| s.owner == owner && s.fence.attached() == Attach::None)
            .count();
        let live = self.nvrm_handles.len() + self.nvrm_reserved;
        let verdict = rm_limits::admit(
            &self.nvrm_limits.handle_bounds,
            self.nvrm_handles.capacity(),
            live,
            mine,
        );
        if verdict != Admit::Ok {
            // `NeedGrow` here means the PASSIVE pre-grow (`grow_nvrm_tables`) did not
            // cover this reservation (the allocator refused, or more concurrent
            // reservers than the headroom): refused and counted as `NvTblOom`.
            crate::virtio::nvrm_window::count_handle_refusal(verdict);
            return false;
        }
        self.nvrm_reserved += 1;
        let now = (live + 1).min(u32::MAX as usize) as u32;
        crate::virtio::nvrm_window::HDL_LIVE.store(now, Ordering::Relaxed);
        crate::virtio::nvrm_window::HDL_PEAK.fetch_max(now, Ordering::Relaxed);
        true
    }

    /// The capacity the handle table wants before the next reservation, if it wants more.
    /// Called outside the lock's critical work (a short hold), then the allocation happens
    /// with no lock held ([`grow_nvrm_tables`]).
    pub(super) fn nvrm_handles_want(&self) -> Option<usize> {
        rm_limits::want_capacity(
            &self.nvrm_limits.handle_bounds,
            self.nvrm_handles.capacity(),
            self.nvrm_handles.len() + self.nvrm_reserved,
        )
    }

    /// Swap in the larger storage allocated outside the lock. Moves the entries (a copy, no
    /// allocation: the new storage holds them all) and hands back the old storage, empty,
    /// for the caller to free with no lock held. A `fresh` that is not larger than what is
    /// here (another caller grew it first) or cannot hold what is here comes back untouched.
    pub(super) fn nvrm_handles_install(
        &mut self,
        fresh: Option<Vec<NvrmHandleSlot>>,
    ) -> Option<Vec<NvrmHandleSlot>> {
        let mut fresh = fresh?;
        if fresh.capacity() <= self.nvrm_handles.capacity()
            || fresh.capacity() < self.nvrm_handles.len()
        {
            return Some(fresh);
        }
        fresh.append(&mut self.nvrm_handles);
        core::mem::swap(&mut self.nvrm_handles, &mut fresh);
        crate::virtio::nvrm_window::HDL_CAP.store(
            self.nvrm_handles.capacity().min(u32::MAX as usize) as u32,
            Ordering::Relaxed,
        );
        crate::virtio::nvrm_window::HDL_GROWS.fetch_add(1, Ordering::Relaxed);
        Some(fresh)
    }

    /// The live-handles gauge after an entry left the table.
    fn note_handles_live(&self) {
        let live = (self.nvrm_handles.len() + self.nvrm_reserved).min(u32::MAX as usize) as u32;
        crate::virtio::nvrm_window::HDL_LIVE.store(live, Ordering::Relaxed);
    }

    /// Put `handle` back after a forwarded `Close` the host did not take (it is still open
    /// there, and still `owner`'s). Takes a slot of the storage the reservations leave free
    /// (`Bounds::restore_slack`): no bound, no fairness (the entry was admitted when it was
    /// made) and no growth (this runs where nothing may allocate). `false`, and `NvRestLost`
    /// counted, when even that is gone: the handle is then open on the host and untracked
    /// here until the transport's sweep.
    pub fn restore_nvrm_handle(&mut self, owner: DeviceOwner, handle: u32, device_type: u32) -> bool {
        if !rm_limits::restore_room(
            self.nvrm_handles.capacity(),
            self.nvrm_handles.len() + self.nvrm_reserved,
        ) {
            crate::virtio::nvrm_window::REST_LOST.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.nvrm_handles.push(NvrmHandleSlot {
            owner,
            handle,
            device_type,
            ready_latched: false,
            fence: FenceMeta::new(0),
        });
        self.note_handles_live();
        true
    }

    /// Commit a reserved slot once the host has opened `handle`.
    pub fn commit_nvrm_handle(&mut self, owner: DeviceOwner, handle: u32, device_type: u32) {
        self.nvrm_reserved = self.nvrm_reserved.saturating_sub(1);
        self.nvrm_handles.push(NvrmHandleSlot {
            owner,
            handle,
            device_type,
            ready_latched: false,
            fence: FenceMeta::new(0),
        });
    }

    /// Release a reserved slot after a refused or failed `Open`.
    pub fn cancel_nvrm_reservation(&mut self) {
        self.nvrm_reserved = self.nvrm_reserved.saturating_sub(1);
    }

    /// Whether `owner` opened `handle` (and has not closed it).
    pub fn nvrm_handle_owned(&self, owner: DeviceOwner, handle: u32) -> bool {
        self.nvrm_handles
            .iter()
            .any(|s| s.owner == owner && s.handle == handle)
    }

    /// The `device_type` `owner` opened `handle` with, or `None` if it is not
    /// theirs.
    pub fn nvrm_handle_device_type(&self, owner: DeviceOwner, handle: u32) -> Option<u32> {
        self.nvrm_handles
            .iter()
            .find(|s| s.owner == owner && s.handle == handle)
            .map(|s| s.device_type)
    }

    /// Forget `handle` after the host closed it. `false` if `owner` does not own it.
    pub fn take_nvrm_handle(&mut self, owner: DeviceOwner, handle: u32) -> bool {
        let Some(idx) = self
            .nvrm_handles
            .iter()
            .position(|s| s.owner == owner && s.handle == handle)
        else {
            return false;
        };
        self.nvrm_handles.swap_remove(idx);
        self.note_handles_live();
        true
    }

    /// Pop one handle still owned by `owner` (device teardown closes it on the
    /// host outside the lock, one at a time): the handle and its `device_type`.
    pub fn take_nvrm_handle_for_owner(&mut self, owner: DeviceOwner) -> Option<(u32, u32)> {
        let idx = self.nvrm_handles.iter().position(|s| s.owner == owner)?;
        let s = self.nvrm_handles.swap_remove(idx);
        self.note_handles_live();
        Some((s.handle, s.device_type))
    }

    /// Pop one handle of ANY owner (the transport is being retired and every
    /// remaining handle is closed on the host first, one at a time, outside the
    /// lock): the owner, the handle and its `device_type`.
    pub fn take_nvrm_handle_any(&mut self) -> Option<(DeviceOwner, u32, u32)> {
        let s = self.nvrm_handles.pop()?;
        self.note_handles_live();
        Some((s.owner, s.handle, s.device_type))
    }

    /// The table index of `handle` (any owner), for the DPC that serves one `EventReady`: ONE
    /// scan under the virtio lock, shared by [`Self::fence_note_fired_at`] and
    /// [`Self::latch_nvrm_ready_at`] (it used to be two per event, up to a ring's worth of
    /// events per drain, over a table that can now hold 16384 entries). Valid until the table
    /// changes: the caller holds the lock and changes nothing in between.
    pub(super) fn nvrm_handle_index(&self, handle: u32) -> Option<usize> {
        self.nvrm_handles.iter().position(|s| s.handle == handle)
    }

    /// Latch an `EventReady` for the handle at `idx` (see `NvrmHandleSlot::ready_latched`).
    /// `false` if no process has it open (`idx` is `None`).
    pub(super) fn latch_nvrm_ready_at(&mut self, idx: Option<usize>) -> bool {
        match idx.and_then(|i| self.nvrm_handles.get_mut(i)) {
            Some(s) => {
                s.ready_latched = true;
                true
            }
            None => false,
        }
    }

    /// Consume the latch of `handle` if `owner` has it open and one is set.
    pub(super) fn take_nvrm_ready_latch(&mut self, owner: DeviceOwner, handle: u32) -> bool {
        match self
            .nvrm_handles
            .iter_mut()
            .find(|s| s.owner == owner && s.handle == handle)
        {
            Some(s) => core::mem::replace(&mut s.ready_latched, false),
            None => false,
        }
    }

    // ---- fence handles -------------------------------------------------------------
    //
    // A handle a forwarded `SEMSURF_FENCE_CREATE` returned (`kmd_logic::nvrm_fence`).
    // It lives in `nvrm_handles` like any other, recorded under
    // `DEVICE_TYPE_FENCE`, so quotas, `EVENT_REGISTER`, the latch, `Close` and
    // teardown all apply unchanged.

    /// A create is about to be forwarded: reserve a tracking slot exactly as an
    /// `Open` does (a full table or quota refuses BEFORE the host makes a fence)
    /// and start keeping `EventReady`s for handles nobody owns yet. `false`: no
    /// room, nothing was started.
    pub fn begin_nvrm_fence_create(&mut self, owner: DeviceOwner) -> bool {
        if !self.reserve_nvrm_handle_slot(owner) {
            return false;
        }
        self.nvrm_fences.begin();
        true
    }

    /// The create failed or its reply held no usable handle: undo `begin`.
    pub fn cancel_nvrm_fence_create(&mut self) {
        self.cancel_nvrm_reservation();
        self.nvrm_fences.finish(None);
    }

    /// The host made fence `handle`: record it as `owner`'s, in one lock hold with
    /// taking whatever `EventReady` already arrived for it, which is latched so the
    /// first `EVENT_REGISTER` signals at once.
    pub fn commit_nvrm_fence(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
        process: usize,
    ) -> FenceCommit {
        // The host never hands out a number that is live. If one is, recording it
        // would make two owners of one handle; refuse and leave the other alone.
        if self.nvrm_handles.iter().any(|s| s.handle == handle) {
            self.cancel_nvrm_fence_create();
            return FenceCommit::Duplicate;
        }
        let early = self.nvrm_fences.finish_status(Some(handle));
        self.commit_nvrm_handle(owner, handle, DEVICE_TYPE_FENCE);
        if let Some(s) = self.nvrm_handles.last_mut() {
            s.fence = FenceMeta::new(process);
            if let Some(status) = early {
                s.ready_latched = true;
                s.fence.set_fired(status);
            }
        }
        FenceCommit::Recorded {
            fired: early.is_some(),
        }
    }

    /// An `EventReady` for a handle nobody has open: keep it if a create is in
    /// flight that may own it.
    ///
    /// The status is kept with it (0 or the fence's error), so a fence that fired
    /// with an error before its create's reply was recorded still counts as one.
    pub(super) fn note_nvrm_fence_ready(&mut self, handle: u32, status: i32) -> Noted {
        self.nvrm_fences.note_ready_status(handle, status)
    }

    /// An `EventReady{handle, status}` for a fence: record that it fired (the first
    /// one only; see `FenceMeta::set_fired`). The caller routes by what it was
    /// attached to.
    pub(super) fn fence_note_fired_at(&mut self, idx: Option<usize>, status: i32) -> FenceFire {
        match idx
            .and_then(|i| self.nvrm_handles.get_mut(i))
            .filter(|s| is_fence_type(s.device_type))
        {
            None => FenceFire::NotFence,
            Some(s) => {
                if s.fence.set_fired(status) {
                    FenceFire::Fired(s.fence.attached())
                } else {
                    FenceFire::Repeat
                }
            }
        }
    }

    /// Whether fence `handle` fired. A handle that is not there (closed behind our
    /// back) reads as fired: nothing will ever fire it, and a queued present must
    /// not wedge on it.
    pub fn fence_fired(&self, handle: u32) -> bool {
        self.nvrm_handles
            .iter()
            .find(|s| s.handle == handle && is_fence_type(s.device_type))
            .map_or(true, |s| s.fence.fired().is_some())
    }

    /// The status `handle` fired with.
    pub fn fence_status(&self, handle: u32) -> Option<i32> {
        self.nvrm_handles
            .iter()
            .find(|s| s.handle == handle && is_fence_type(s.device_type))
            .and_then(|s| s.fence.fired())
    }

    /// Take a fence over for a carrier: check the caller's claim, re-tag it to the
    /// KMD (every later user call on it is `NOT_OWNED`) and record what it waits
    /// for. Returns the status it had already fired with, if it had. One lock hold:
    /// the checks and the re-tag cannot be split by a concurrent `Close`.
    pub fn fence_attach(
        &mut self,
        by: FenceClaim,
        handle: u32,
        to: Attach,
    ) -> Result<Option<i32>, FenceRefusal> {
        let idx = self.fence_claim_index(by, handle, true)?;
        let s = &mut self.nvrm_handles[idx];
        match s.fence.attach(to) {
            Ok(early) => {
                s.owner = DeviceOwner::KMD_RM;
                Ok(early)
            }
            Err(_) => Err(FenceRefusal::AlreadyAttached),
        }
    }

    /// Whether [`Self::fence_attach`] would take `handle` for `by` right now, with
    /// nothing changed. A carrier that must reserve something of its own first (a
    /// gate and a stream slot) asks this first, so a refusal costs it nothing. The
    /// answer holds for as long as the caller keeps the lock it asked under.
    pub fn fence_claimable(&self, by: FenceClaim, handle: u32) -> Result<(), FenceRefusal> {
        self.fence_claim_index(by, handle, true).map(|_| ())
    }

    /// The checks of a claim, in order (the first failure wins): the handle exists,
    /// is `by`'s, is a fence, is not already taken, and (`quota`) the KMD has room
    /// for one more of its own. Read-only; returns the table index.
    fn fence_claim_index(
        &self,
        by: FenceClaim,
        handle: u32,
        quota: bool,
    ) -> Result<usize, FenceRefusal> {
        let Some(idx) = self.nvrm_handles.iter().position(|s| s.handle == handle) else {
            return Err(FenceRefusal::NotOwned);
        };
        let s = &self.nvrm_handles[idx];
        match by {
            FenceClaim::Owner(owner) => {
                if s.owner != owner {
                    return Err(FenceRefusal::NotOwned);
                }
            }
            FenceClaim::Process(process) => {
                // A fence of this process, not already the KMD's. Nothing else
                // about the creating device matters: the presenting device is not it.
                if s.owner == DeviceOwner::KMD_RM
                    || !is_fence_type(s.device_type)
                    || !same_process(s.fence.process(), process)
                {
                    return Err(FenceRefusal::NotOwned);
                }
            }
        }
        if !is_fence_type(s.device_type) {
            return Err(FenceRefusal::NotFence);
        }
        if s.fence.attached() != Attach::None || s.fence.close_wanted() {
            return Err(FenceRefusal::AlreadyAttached);
        }
        if quota && self.attached_fences() >= MAX_NVRM_ATTACHED_FENCES {
            return Err(FenceRefusal::Quota);
        }
        Ok(idx)
    }

    /// Fences the KMD holds as its own (attached, discarded or restored), until it
    /// has closed them.
    fn attached_fences(&self) -> usize {
        self.nvrm_handles
            .iter()
            .filter(|s| s.fence.attached() != Attach::None)
            .count()
    }

    /// Take a fence of `by` only to close it: a `HERF` / `HEPR` tail whose marker
    /// could not be attached still names a handle the UMD gave up (the call returned
    /// success, so the UMD cannot know), and a handle nobody closes counts against
    /// its 128-per-process quota for good. Re-tags it to the KMD and owes the host
    /// its `Close` at once. Returns whether this call made the debt (the caller wakes
    /// the worker). No quota check: this is what frees an entry.
    pub fn fence_discard(&mut self, by: FenceClaim, handle: u32) -> Result<bool, FenceRefusal> {
        let idx = self.fence_claim_index(by, handle, false)?;
        let s = &mut self.nvrm_handles[idx];
        if s.fence.attach(Attach::Discard).is_err() {
            return Err(FenceRefusal::AlreadyAttached);
        }
        s.owner = DeviceOwner::KMD_RM;
        let made = s.fence.want_close();
        if made {
            crate::virtio::nvrm::FENCE_CLOSE_OWED.store(1, Ordering::Release);
        }
        Ok(made)
    }

    /// Undo [`Self::fence_attach`] for a carrier that then failed (the entry was
    /// never queued): hand the handle back to `owner`.
    pub fn fence_unattach(&mut self, owner: DeviceOwner, handle: u32) {
        if let Some(s) = self
            .nvrm_handles
            .iter_mut()
            .find(|s| s.handle == handle && s.owner == DeviceOwner::KMD_RM)
        {
            s.fence.unattach();
            s.owner = owner;
        }
    }

    /// The KMD owes the host a `Close` of fence `handle`. Returns whether this call
    /// made the debt (the caller then wakes the worker).
    ///
    /// Only for a fence the KMD took over (owned by `KMD_RM` and attached to
    /// something): a stale queue entry, or a purged gate's point, can name a number
    /// the host has since reused for somebody else's live fence, and that one must
    /// never be closed from here.
    pub fn fence_want_close(&mut self, handle: u32) -> bool {
        let made = self
            .nvrm_handles
            .iter_mut()
            .find(|s| {
                s.handle == handle
                    && is_fence_type(s.device_type)
                    && s.owner == DeviceOwner::KMD_RM
                    && s.fence.attached() != Attach::None
            })
            .is_some_and(|s| s.fence.want_close());
        if made {
            crate::virtio::nvrm::FENCE_CLOSE_OWED.store(1, Ordering::Release);
        }
        made
    }

    /// Pop one fence the KMD owes a `Close`, out of the table (take-then-send, as
    /// `Close` does: the host may reuse the number the moment it closes it).
    pub fn take_fence_to_close(&mut self) -> Option<u32> {
        let idx = self
            .nvrm_handles
            .iter()
            .position(|s| is_fence_type(s.device_type) && s.fence.close_wanted())?;
        let handle = self.nvrm_handles.swap_remove(idx).handle;
        self.note_handles_live();
        Some(handle)
    }

    /// The host did not take the `Close` of `handle` that [`Self::take_fence_to_close`]
    /// popped: put it back as the KMD's, so the transport sweep closes it.
    ///
    /// It goes back attached to nothing (`Attach::Discard`, no `Close` owed again, or
    /// the worker would retry it forever): it is the KMD's, counted against the KMD's
    /// own fence quota and not the RM client's, and only the sweep closes it.
    pub fn restore_fence_after_failed_close(&mut self, handle: u32) {
        if !rm_limits::restore_room(
            self.nvrm_handles.capacity(),
            self.nvrm_handles.len() + self.nvrm_reserved,
        ) {
            // No slot (the slack kept for this was used up): the fence stays open on the
            // host and untracked here. Counted, never silent (`NvRestLost`).
            crate::virtio::nvrm_window::REST_LOST.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut fence = FenceMeta::new(0);
        let _ = fence.attach(Attach::Discard);
        self.nvrm_handles.push(NvrmHandleSlot {
            owner: DeviceOwner::KMD_RM,
            handle,
            device_type: DEVICE_TYPE_FENCE,
            ready_latched: false,
            fence,
        });
    }

    /// Fences the KMD still owes a `Close`.
    pub fn fences_owing_close(&self) -> usize {
        self.nvrm_handles
            .iter()
            .filter(|s| is_fence_type(s.device_type) && s.fence.close_wanted())
            .count()
    }

    /// The transport generation, for `HeliosNvrmHeader.epoch`: it changes when
    /// the device is reset or the transport replaced, which is exactly when every
    /// backend handle of the earlier generation stopped existing. The wire fence
    /// base is already stride-separated per transport instance.
    pub fn nvrm_epoch(&self) -> u64 {
        self.wire_fence_base
    }

    // ---- mappings -----------------------------------------------------------------

    /// The shared-memory region an `Mmap` on a handle of `device_type` points
    /// into: the UVM aperture for UVM proper (256), the RM window for everything
    /// else — UVM tools (257) included, as in the guest module and the host.
    pub fn nvrm_region(&self, device_type: u32) -> Option<HostVisibleWindow> {
        if device_type == 256 {
            self.nvrm_aperture
        } else {
            self.nvrm_window
        }
    }

    /// May `owner` hold one more mapping, by the table's sanity bounds (per process, whole
    /// table, fairness when scarce)? The pre-check before the host is asked. `NeedGrow` means
    /// the table is full but may grow: the caller runs [`grow_nvrm_tables`] and asks again.
    pub fn nvrm_map_admit(&self, owner: DeviceOwner) -> Admit {
        let mine = self.nvrm_maps.iter().filter(|s| s.owner == owner).count();
        rm_limits::admit(
            &self.nvrm_limits.map_bounds,
            self.nvrm_maps.capacity(),
            self.nvrm_maps.len(),
            mine,
        )
    }

    /// The capacity the mapping table wants before the next push, if it wants more.
    pub(super) fn nvrm_maps_want(&self) -> Option<usize> {
        rm_limits::want_capacity(
            &self.nvrm_limits.map_bounds,
            self.nvrm_maps.capacity(),
            self.nvrm_maps.len(),
        )
    }

    /// Swap in the larger mapping storage allocated outside the lock (see
    /// [`Self::nvrm_handles_install`]).
    pub(super) fn nvrm_maps_install(
        &mut self,
        fresh: Option<Vec<NvrmMapSlot>>,
    ) -> Option<Vec<NvrmMapSlot>> {
        let mut fresh = fresh?;
        if fresh.capacity() <= self.nvrm_maps.capacity() || fresh.capacity() < self.nvrm_maps.len()
        {
            return Some(fresh);
        }
        fresh.append(&mut self.nvrm_maps);
        core::mem::swap(&mut self.nvrm_maps, &mut fresh);
        crate::virtio::nvrm_window::MAP_CAP.store(
            self.nvrm_maps.capacity().min(u32::MAX as usize) as u32,
            Ordering::Relaxed,
        );
        crate::virtio::nvrm_window::MAP_GROWS.fetch_add(1, Ordering::Relaxed);
        Some(fresh)
    }

    /// May `owner` map `size` more bytes of the window? The pre-check before the host is
    /// asked, counting a refusal by reason. Always for the UVM aperture (its own region,
    /// sized by the host). `live_privileged`: the caller's evidence that `owner` is the
    /// privileged device (see `virtio::nvrm_window::live_privileged`; it is read before the
    /// lock is taken).
    pub fn nvrm_window_admit(
        &mut self,
        owner: DeviceOwner,
        uvm: bool,
        size: u64,
        live_privileged: bool,
    ) -> Result<(), rm_window::Refusal> {
        if uvm {
            return Ok(());
        }
        let r = self
            .nvrm_limits
            .acct
            .admit(owner.raw() as u64, live_privileged, size);
        if let Err(why) = r {
            crate::virtio::nvrm_window::count_refusal(why);
        }
        r
    }

    /// What `WINDOW_INFO` tells `owner` (`helios_kmd_logic::rm_window::Account::info`). Read
    /// only: no counter moves.
    pub fn nvrm_window_info(
        &self,
        owner: DeviceOwner,
        live_privileged: bool,
    ) -> rm_window::Info {
        self.nvrm_limits.acct
            .info(owner.raw() as u64, live_privileged)
    }

    /// `owner` set the foreign scanout source: it is the privileged device from now until
    /// its device is destroyed (the reserve is its to use).
    pub fn nvrm_window_mark_privileged(&mut self, owner: DeviceOwner) {
        let _ = self
            .nvrm_limits
            .acct
            .mark_privileged(owner.raw() as u64, 0);
        crate::virtio::nvrm_window::mirror(&self.nvrm_limits.acct.snapshot());
    }

    /// `owner`'s device is destroyed: forget what the window account holds for it (the
    /// per-map releases already ran for each mapping taken; this drops the sticky
    /// privileged mark and any row left by a map whose slot was already gone).
    pub fn nvrm_window_forget_owner(&mut self, owner: DeviceOwner) {
        let _ = self.nvrm_limits.acct.forget_owner(owner.raw() as u64);
        crate::virtio::nvrm_window::mirror(&self.nvrm_limits.acct.snapshot());
    }

    /// Track a new mapping and mint its id. `None` when the handle is no longer
    /// `owner`'s (a concurrent `Close` got there first — the host may already have
    /// reused the number), or a table bound or the window policy refuses, or ids ran out.
    /// Checked and pushed under one lock hold, so a mapping can never be recorded
    /// against a handle that is gone. A non-UVM mapping is charged to the window account
    /// here (the pre-check ran before the host round trip; other maps may have landed
    /// since), and a refusal is counted by reason.
    #[allow(clippy::too_many_arguments)]
    pub fn push_nvrm_map(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
        host_id: u32,
        size: u64,
        uvm: bool,
        pid: u32,
        live_privileged: bool,
    ) -> Option<u32> {
        if !self.nvrm_handle_owned(owner, handle) {
            return None;
        }
        // Storage first: this push must not allocate, so a slot has to be free (the
        // PASSIVE pre-grow made one unless the allocator refused or a bound was hit).
        if self.nvrm_map_admit(owner) != Admit::Ok {
            crate::virtio::nvrm_window::count_map_table_refusal();
            return None;
        }
        // Re-checked here under the same hold as the push.
        if !uvm
            && self
                .nvrm_limits
            .acct
                .charge(owner.raw() as u64, pid, live_privileged, size)
                .map_err(crate::virtio::nvrm_window::count_refusal)
                .is_err()
        {
            return None;
        }
        // Driver-wide counter, not per transport: the view this id names outlives
        // the transport (see `helios_kmd_logic::nvrm_views`). Minted last, after
        // every refusal above, so a refusal costs no id.
        let Some(kmd_id) = crate::virtio::nvrm::mint_map_id() else {
            if !uvm {
                self.nvrm_limits.acct.release(owner.raw() as u64, size);
            }
            return None;
        };
        self.nvrm_maps.push(NvrmMapSlot {
            owner,
            handle,
            kmd_id,
            host_id,
            size,
            uvm,
        });
        self.map_added(size);
        Some(kmd_id)
    }

    /// A mapping of `size` bytes was recorded: the all-owners gauge (`NvMapMb`, the UVM
    /// aperture included) and the window gauges follow, with no scan of the table.
    fn map_added(&self, size: u64) {
        crate::virtio::nvrm::NVRM_MAP_BYTES.fetch_add(size, core::sync::atomic::Ordering::Relaxed);
        crate::virtio::nvrm_window::mirror(&self.nvrm_limits.acct.snapshot());
    }

    /// A mapping slot was taken out of the table: give its bytes back.
    fn map_removed(&mut self, s: &NvrmMapSlot) {
        let _ = crate::virtio::nvrm::NVRM_MAP_BYTES.fetch_update(
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
            |b| Some(b.saturating_sub(s.size)),
        );
        if !s.uvm {
            self.nvrm_limits.acct.release(s.owner.raw() as u64, s.size);
        }
        crate::virtio::nvrm_window::mirror(&self.nvrm_limits.acct.snapshot());
    }

    /// Whether a live mapping already carries this nonzero host id on `handle`.
    /// A repeat mapping of the same object can come back with the same id, and the
    /// host `Munmap` of the first must not be sent for the second's failure (or the
    /// first's removal while the second lives).
    pub fn nvrm_host_map_tracked(&self, handle: u32, host_id: u32) -> bool {
        host_id != 0
            && self
                .nvrm_maps
                .iter()
                .any(|s| s.handle == handle && s.host_id == host_id)
    }

    /// Forget mapping `kmd_id` if `owner` made it: its handle and host id.
    pub fn take_nvrm_map(&mut self, owner: DeviceOwner, kmd_id: u32) -> Option<(u32, u32)> {
        let idx = self
            .nvrm_maps
            .iter()
            .position(|s| s.owner == owner && s.kmd_id == kmd_id)?;
        let s = self.nvrm_maps.swap_remove(idx);
        self.map_removed(&s);
        Some((s.handle, s.host_id))
    }

    /// Pop one mapping `owner` made on `handle` (`Close` releases them first):
    /// its id and host id.
    pub fn take_nvrm_map_for_handle(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
    ) -> Option<(u32, u32)> {
        let idx = self
            .nvrm_maps
            .iter()
            .position(|s| s.owner == owner && s.handle == handle)?;
        let s = self.nvrm_maps.swap_remove(idx);
        self.map_removed(&s);
        Some((s.kmd_id, s.host_id))
    }

    /// Pop one mapping `owner` still holds (device teardown): its handle, id and
    /// host id.
    pub fn take_nvrm_map_for_owner(&mut self, owner: DeviceOwner) -> Option<(u32, u32, u32)> {
        let idx = self.nvrm_maps.iter().position(|s| s.owner == owner)?;
        let s = self.nvrm_maps.swap_remove(idx);
        self.map_removed(&s);
        Some((s.handle, s.kmd_id, s.host_id))
    }

    /// Pop one mapping of ANY owner (see [`Self::take_nvrm_handle_any`]): its
    /// handle and host id.
    pub fn take_nvrm_map_any(&mut self) -> Option<(u32, u32)> {
        let s = self.nvrm_maps.pop()?;
        self.map_removed(&s);
        Some((s.handle, s.host_id))
    }

    // ---- pins ------------------------------------------------------------------------

    /// How many pins `owner` holds (for the quota check before locking pages).
    pub fn nvrm_pin_count(&self, owner: DeviceOwner) -> usize {
        self.nvrm_pins.iter().filter(|p| p.owner == owner).count()
    }

    /// Track a new pin and mint its id. Hands the pin BACK if the handle is no
    /// longer `owner`'s, a quota is full or ids ran out, so the caller unlocks it
    /// outside the lock.
    pub fn push_nvrm_pin(&mut self, mut pin: NvrmPin) -> Result<u32, NvrmPin> {
        let mine = self.nvrm_pins.iter().filter(|p| p.owner == pin.owner).count();
        if !self.nvrm_handle_owned(pin.owner, pin.handle)
            || self.nvrm_pins.len() >= MAX_NVRM_PINS
            || mine >= MAX_NVRM_PINS_PER_OWNER
            || self.nvrm_next_pin == 0
        {
            return Err(pin);
        }
        pin.id = self.nvrm_next_pin;
        // Never reuse an id; on exhaustion further pins are refused (0 = none).
        self.nvrm_next_pin = self.nvrm_next_pin.checked_add(1).unwrap_or(0);
        let id = pin.id;
        self.nvrm_pins.push(pin);
        Ok(id)
    }

    /// The length of the deep block of pin `id`, if it is `owner`'s, was made on
    /// `handle` and no `FORWARD` has claimed it yet.
    pub fn nvrm_pin_deep_len(&self, owner: DeviceOwner, handle: u32, id: u32) -> Option<usize> {
        self.nvrm_pins
            .iter()
            .find(|p| p.owner == owner && p.handle == handle && p.id == id && !p.used)
            .map(|p| p.deep.len())
    }

    /// Copy pin `id`'s deep block into `out` (exactly its length) and mark the pin
    /// claimed by a `FORWARD`. Returns the `deep_ptr_offset` value to write.
    pub fn claim_nvrm_pin_deep(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
        id: u32,
        out: &mut [u8],
    ) -> Option<u32> {
        let pin = self
            .nvrm_pins
            .iter_mut()
            .find(|p| p.owner == owner && p.handle == handle && p.id == id && !p.used)?;
        if pin.deep.len() != out.len() {
            return None;
        }
        out.copy_from_slice(&pin.deep);
        pin.used = true;
        Some(pin.deep_kind)
    }

    /// Remove pin `id` whatever its state (the registration failed, or teardown).
    pub fn take_nvrm_pin(&mut self, owner: DeviceOwner, id: u32) -> Option<NvrmPin> {
        let idx = self
            .nvrm_pins
            .iter()
            .position(|p| p.owner == owner && p.id == id)?;
        Some(self.nvrm_pins.swap_remove(idx))
    }

    /// `UNPIN`: remove pin `id` only if no `FORWARD` has claimed it.
    pub fn take_nvrm_unused_pin(&mut self, owner: DeviceOwner, id: u32) -> PinTake {
        let Some(idx) = self
            .nvrm_pins
            .iter()
            .position(|p| p.owner == owner && p.id == id)
        else {
            return PinTake::NotFound;
        };
        if self.nvrm_pins.get(idx).is_some_and(|p| p.used) {
            return PinTake::InUse;
        }
        PinTake::Taken(self.nvrm_pins.swap_remove(idx))
    }

    /// A successful `RM_FREE` of `(h_root, h_old)`: pop one claimed pin it frees —
    /// the one tagged with that object, or any under the client when the client
    /// itself is freed (`h_old == h_root`).
    pub fn take_nvrm_pin_for_free(
        &mut self,
        owner: DeviceOwner,
        h_root: u32,
        h_old: u32,
    ) -> Option<NvrmPin> {
        let idx = self.nvrm_pins.iter().position(|p| {
            p.owner == owner && p.used && p.h_root == h_root && (p.h_object == h_old || h_old == h_root)
        })?;
        Some(self.nvrm_pins.swap_remove(idx))
    }

    /// `Close` of `handle`: pop one pin made on it.
    pub fn take_nvrm_pin_for_handle(&mut self, owner: DeviceOwner, handle: u32) -> Option<NvrmPin> {
        let idx = self
            .nvrm_pins
            .iter()
            .position(|p| p.owner == owner && p.handle == handle)?;
        Some(self.nvrm_pins.swap_remove(idx))
    }

    /// Device teardown: pop one pin `owner` still holds.
    pub fn take_nvrm_pin_for_owner(&mut self, owner: DeviceOwner) -> Option<NvrmPin> {
        let idx = self.nvrm_pins.iter().position(|p| p.owner == owner)?;
        Some(self.nvrm_pins.swap_remove(idx))
    }

    /// Pop one pin of ANY owner (see [`Self::take_nvrm_handle_any`]); the caller
    /// unlocks it outside the lock.
    pub fn take_nvrm_pin_any(&mut self) -> Option<NvrmPin> {
        self.nvrm_pins.pop()
    }

    // ---- transport teardown -------------------------------------------------------

    /// The transport is being dropped (`StopDevice`, a failed start, a
    /// replacement): whatever its owners left tracked dies with it. Called from
    /// `Drop`, PASSIVE, outside the virtio lock, AFTER the device was reset.
    ///
    /// This is the FALLBACK, for a transport that could not be asked (it failed, or
    /// was already gone) or that something re-populated after the live sweep. It
    /// sends nothing, so it cannot tell the host to let go of anything, and the
    /// reset does NOT make the host drop its RM files either (the backend resets
    /// only at its next feature negotiation, i.e. the next `StartDevice`): a pin
    /// unlocked here may still be held by the host. The path that is safe for the
    /// pages is `nvrm::retire_transport`, which closes every handle on the host
    /// while the transport is alive and only then lets this run. What this does
    /// with a pin depends on whether the host may alias it (a `FORWARD` claimed
    /// its table): an unclaimed pin is unlocked; a claimed one stays locked
    /// (`NvPinLeak`) with its owner's EPROCESS reference held, because unlocking it
    /// would let the guest reuse RAM the GPU may still write. The cost of that
    /// choice is the locked pages themselves: they are meant not to bugcheck the
    /// owner at exit (0x76) only because the held process reference keeps its
    /// process object alive, which is UNVERIFIED (see `NvrmPin`).
    ///
    /// Idempotent against the live sweep and the per-device `close_all_for_owner`:
    /// all take entries out of these same tables, so whichever runs first releases
    /// them and the others find nothing (once the transport is gone `with_virtio`
    /// fails and the per-device path is a no-op).
    ///
    /// The user VIEWS of the mappings are not here: they live in
    /// `AdapterContext::mappings` and can only be unmapped in their owning process
    /// (see `MappingTable::mark_nvrm_views_stale`). Event registrations are
    /// released by `teardown_nvrm_events`. Returns how many entries were still tracked.
    pub(super) fn teardown_nvrm_state(&mut self) -> u32 {
        let mut swept = 0u32;
        // Pins: popped one at a time and dropped (= unlocked) here, not under any lock.
        //
        // Anything still tracked here was NOT confirmed closed by the host: the live
        // sweep (`close_all_on_host`) takes every pin out of the table itself, so a
        // pin found now belongs to a transport that already failed (nothing was
        // sent) or was re-populated concurrently. The host keeps its RM files across
        // a device reset, so a pin a `FORWARD` claimed may still be aliased by the
        // GPU; unlocking it would let the guest reuse that RAM underneath the host.
        // Those stay locked (`NvPinLeak`), holding their owner process's EPROCESS
        // reference (see `NvrmPin`: whether that keeps the owner from bugchecking
        // 0x76 at exit is unverified); unclaimed ones are unlocked as before.
        // Nothing was confirmed closed on this path, so the fate is `Leak`.
        while let Some(pin) = self.nvrm_pins.pop() {
            swept = swept.saturating_add(1);
            match PinFate::Leak.action(pin.host_may_alias()) {
                PinAction::Leak => pin.leak(),
                PinAction::Unlock => drop(pin),
            }
        }
        swept = swept
            .saturating_add(self.nvrm_maps.len() as u32)
            .saturating_add(self.nvrm_handles.len() as u32);
        // A fence handle is a handle: the `Close` and device-destroy paths count it
        // as closed (`NvFenceCl`), so this one must too, or `NvFence - NvFenceCl`
        // drifts upward for every fence a stop swept.
        let fences = self
            .nvrm_handles
            .iter()
            .filter(|s| is_fence_type(s.device_type))
            .count() as u32;
        crate::virtio::nvrm::NVRM_FENCES_CLOSED.fetch_add(fences, Ordering::Relaxed);
        // Plain data: nothing in them needs PASSIVE.
        self.nvrm_maps.clear();
        self.nvrm_handles.clear();
        self.nvrm_reserved = 0;
        // Nothing is mapped or privileged in a transport that is gone; the high-water mark
        // and the refusal counts stay.
        self.nvrm_limits.acct.clear();
        crate::virtio::nvrm_window::reset_gauges();
        crate::virtio::nvrm::NVRM_SWEPT.fetch_add(swept, Ordering::Relaxed);
        swept
    }
}

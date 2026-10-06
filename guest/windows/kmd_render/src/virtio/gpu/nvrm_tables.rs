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
use helios_kmd_logic::nvrm_fence::{is_fence as is_fence_type, Noted, DEVICE_TYPE_FENCE};
use helios_kmd_logic::sweep_budget::{PinAction, PinFate};

/// Most backend handles tracked across every process.
pub const MAX_NVRM_HANDLES: usize = 1024;
/// Most one process may hold open at once (`QUERY_CAPS` reports it).
pub const MAX_NVRM_HANDLES_PER_OWNER: usize = 128;
/// Most live `MMAP` mappings across every process, and per process.
pub const MAX_NVRM_MAPS: usize = 1024;
pub const MAX_NVRM_MAPS_PER_OWNER: usize = 256;
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

impl VirtioGpu {
    // ---- handles -----------------------------------------------------------------

    /// Reserve a tracking slot for an in-flight `Open`. Refuses when the table
    /// or this owner's quota is full, BEFORE the host is asked to open anything.
    pub fn reserve_nvrm_handle_slot(&mut self, owner: DeviceOwner) -> bool {
        let mine = self.nvrm_handles.iter().filter(|s| s.owner == owner).count();
        if self.nvrm_handles.len() + self.nvrm_reserved >= MAX_NVRM_HANDLES
            || mine >= MAX_NVRM_HANDLES_PER_OWNER
        {
            return false;
        }
        self.nvrm_reserved += 1;
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
        true
    }

    /// Pop one handle still owned by `owner` (device teardown closes it on the
    /// host outside the lock, one at a time): the handle and its `device_type`.
    pub fn take_nvrm_handle_for_owner(&mut self, owner: DeviceOwner) -> Option<(u32, u32)> {
        let idx = self.nvrm_handles.iter().position(|s| s.owner == owner)?;
        let s = self.nvrm_handles.swap_remove(idx);
        Some((s.handle, s.device_type))
    }

    /// Pop one handle of ANY owner (the transport is being retired and every
    /// remaining handle is closed on the host first, one at a time, outside the
    /// lock): the owner, the handle and its `device_type`.
    pub fn take_nvrm_handle_any(&mut self) -> Option<(DeviceOwner, u32, u32)> {
        let s = self.nvrm_handles.pop()?;
        Some((s.owner, s.handle, s.device_type))
    }

    /// Latch an `EventReady` for `handle` (see `NvrmHandleSlot::ready_latched`).
    /// `false` if no process has it open.
    pub(super) fn latch_nvrm_ready(&mut self, handle: u32) -> bool {
        match self.nvrm_handles.iter_mut().find(|s| s.handle == handle) {
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
    pub fn commit_nvrm_fence(&mut self, owner: DeviceOwner, handle: u32) -> FenceCommit {
        // The host never hands out a number that is live. If one is, recording it
        // would make two owners of one handle; refuse and leave the other alone.
        if self.nvrm_handles.iter().any(|s| s.handle == handle) {
            self.cancel_nvrm_fence_create();
            return FenceCommit::Duplicate;
        }
        let fired = self.nvrm_fences.finish(Some(handle));
        self.commit_nvrm_handle(owner, handle, DEVICE_TYPE_FENCE);
        if fired {
            if let Some(s) = self.nvrm_handles.last_mut() {
                s.ready_latched = true;
            }
        }
        FenceCommit::Recorded { fired }
    }

    /// Whether `handle` is a fence some process holds (any owner: an `EventReady`
    /// names no owner).
    pub(super) fn nvrm_handle_is_fence(&self, handle: u32) -> bool {
        self.nvrm_handles
            .iter()
            .any(|s| s.handle == handle && is_fence_type(s.device_type))
    }

    /// An `EventReady` for a handle nobody has open: keep it if a create is in
    /// flight that may own it.
    pub(super) fn note_nvrm_fence_ready(&mut self, handle: u32) -> Noted {
        self.nvrm_fences.note_ready(handle)
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

    /// How many mappings `owner` holds (for the quota check before asking the host).
    pub fn nvrm_map_count(&self, owner: DeviceOwner) -> usize {
        self.nvrm_maps.iter().filter(|s| s.owner == owner).count()
    }

    /// Track a new mapping and mint its id. `None` when the handle is no longer
    /// `owner`'s (a concurrent `Close` got there first — the host may already have
    /// reused the number), or the table or `owner`'s quota is full, or ids ran out.
    /// Checked and pushed under one lock hold, so a mapping can never be recorded
    /// against a handle that is gone.
    pub fn push_nvrm_map(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
        host_id: u32,
        size: u64,
        uvm: bool,
    ) -> Option<u32> {
        if !self.nvrm_handle_owned(owner, handle) {
            return None;
        }
        // Re-checked here under the same hold as the push (the pre-check in
        // `nvrm_map_bytes_room` ran before the host round trip).
        if !self.nvrm_map_bytes_room(owner, uvm, size) {
            crate::virtio::nvrm::NVRM_MAP_QUOTA_REFUSED.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mine = self.nvrm_maps.iter().filter(|s| s.owner == owner).count();
        if self.nvrm_maps.len() >= MAX_NVRM_MAPS || mine >= MAX_NVRM_MAPS_PER_OWNER {
            return None;
        }
        // Driver-wide counter, not per transport: the view this id names outlives
        // the transport (see `helios_kmd_logic::nvrm_views`). Minted last, after
        // every refusal above, so a refusal costs no id.
        let kmd_id = crate::virtio::nvrm::mint_map_id()?;
        self.nvrm_maps.push(NvrmMapSlot {
            owner,
            handle,
            kmd_id,
            host_id,
            size,
            uvm,
        });
        self.refresh_map_gauge();
        Some(kmd_id)
    }

    /// The per-device byte quota for views of the RM window: a quarter of the window
    /// (read from the device at init, not assumed), so one process cannot starve the
    /// others of window space. 0 with no window.
    pub fn nvrm_map_byte_quota(&self) -> u64 {
        self.nvrm_window.map_or(0, |w| w.len / 4)
    }

    /// Whether `owner` may map `size` more bytes: always for the UVM aperture (it has
    /// its own region and the host sizes it), else within [`Self::nvrm_map_byte_quota`].
    pub fn nvrm_map_bytes_room(&self, owner: DeviceOwner, uvm: bool, size: u64) -> bool {
        if uvm {
            return true;
        }
        let mine: u64 = self
            .nvrm_maps
            .iter()
            .filter(|s| s.owner == owner && !s.uvm)
            .fold(0u64, |a, s| a.saturating_add(s.size));
        mine.saturating_add(size) <= self.nvrm_map_byte_quota()
    }

    /// Publish the bytes currently mapped, all owners (`NvMapMb`).
    fn refresh_map_gauge(&self) {
        let total = self
            .nvrm_maps
            .iter()
            .fold(0u64, |a, s| a.saturating_add(s.size));
        crate::virtio::nvrm::NVRM_MAP_BYTES.store(total, core::sync::atomic::Ordering::Relaxed);
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
        self.refresh_map_gauge();
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
        self.refresh_map_gauge();
        Some((s.kmd_id, s.host_id))
    }

    /// Pop one mapping `owner` still holds (device teardown): its handle, id and
    /// host id.
    pub fn take_nvrm_map_for_owner(&mut self, owner: DeviceOwner) -> Option<(u32, u32, u32)> {
        let idx = self.nvrm_maps.iter().position(|s| s.owner == owner)?;
        let s = self.nvrm_maps.swap_remove(idx);
        self.refresh_map_gauge();
        Some((s.handle, s.kmd_id, s.host_id))
    }

    /// Pop one mapping of ANY owner (see [`Self::take_nvrm_handle_any`]): its
    /// handle and host id.
    pub fn take_nvrm_map_any(&mut self) -> Option<(u32, u32)> {
        let s = self.nvrm_maps.pop()?;
        self.refresh_map_gauge();
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
        crate::virtio::nvrm::NVRM_SWEPT.fetch_add(swept, Ordering::Relaxed);
        swept
    }
}

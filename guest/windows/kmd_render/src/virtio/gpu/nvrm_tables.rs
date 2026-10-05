//! Ownership of the backend handles a process opened through
//! `HELIOS_ESCAPE_NVRM` (a forwarded RM `Open`).
//!
//! The host hands out one handle per opened RM file (`/dev/nvidiactl`, a GPU, a
//! DRM node, UVM). Without a table here any process could name any other's
//! handle in a forwarded `Ioctl`/`Close`/`Mmap`, so a handle is recorded against
//! the device that opened it and every handle-taking request is checked against
//! it. The table also lets device teardown close what a crashed process left
//! open — the host keeps those objects, and their VRAM, until the file is closed.
//!
//! Field-disjoint from the control queue and the fence tables, like
//! `resource_tables`; capacity is reserved at init so no push allocates under the
//! spinlock, and slots are reserved before the wire round trip and committed
//! after it so "open on the host but untracked" cannot exist for a refused open.

use super::*;

/// Most backend handles tracked across every process.
pub const MAX_NVRM_HANDLES: usize = 1024;
/// Most one process may hold open at once (`QUERY_CAPS` reports it).
pub const MAX_NVRM_HANDLES_PER_OWNER: usize = 128;
/// Most live `MMAP` mappings across every process, and per process.
pub const MAX_NVRM_MAPS: usize = 1024;
pub const MAX_NVRM_MAPS_PER_OWNER: usize = 256;

/// One tracked handle.
pub(super) struct NvrmHandleSlot {
    owner: DeviceOwner,
    handle: u32,
    /// The host `device_type` the handle was opened with (255 = control, a GPU
    /// minor, 256/257 = UVM, 512+ = DRM); decides which region an `Mmap` is in.
    device_type: u32,
}

/// One live mapping, for the host `Munmap` at `Close` / teardown.
pub(super) struct NvrmMapSlot {
    owner: DeviceOwner,
    handle: u32,
    mapping_id: u32,
}

impl VirtioGpu {
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
        });
    }

    /// The `device_type` `owner` opened `handle` with, or `None` if it is not
    /// theirs.
    pub fn nvrm_handle_device_type(&self, owner: DeviceOwner, handle: u32) -> Option<u32> {
        self.nvrm_handles
            .iter()
            .find(|s| s.owner == owner && s.handle == handle)
            .map(|s| s.device_type)
    }

    /// The shared-memory region an `Mmap` on a handle of `device_type` points
    /// into: the UVM aperture for UVM, the RM window for everything else.
    pub fn nvrm_region(&self, device_type: u32) -> Option<HostVisibleWindow> {
        if device_type == 256 || device_type == 257 {
            self.nvrm_aperture
        } else {
            self.nvrm_window
        }
    }

    /// Track a new mapping. `false` when the table or `owner`'s quota is full.
    pub fn push_nvrm_map(&mut self, owner: DeviceOwner, handle: u32, mapping_id: u32) -> bool {
        let mine = self.nvrm_maps.iter().filter(|s| s.owner == owner).count();
        if self.nvrm_maps.len() >= MAX_NVRM_MAPS || mine >= MAX_NVRM_MAPS_PER_OWNER {
            return false;
        }
        self.nvrm_maps.push(NvrmMapSlot {
            owner,
            handle,
            mapping_id,
        });
        true
    }

    /// How many mappings `owner` holds (for the quota check before asking the host).
    pub fn nvrm_map_count(&self, owner: DeviceOwner) -> usize {
        self.nvrm_maps.iter().filter(|s| s.owner == owner).count()
    }

    /// Forget `mapping_id` if `owner` made it, returning the handle it belongs to.
    pub fn take_nvrm_map(&mut self, owner: DeviceOwner, mapping_id: u32) -> Option<u32> {
        let idx = self
            .nvrm_maps
            .iter()
            .position(|s| s.owner == owner && s.mapping_id == mapping_id)?;
        Some(self.nvrm_maps.swap_remove(idx).handle)
    }

    /// Pop one mapping `owner` made on `handle` (`Close` releases them first).
    pub fn take_nvrm_map_for_handle(&mut self, owner: DeviceOwner, handle: u32) -> Option<u32> {
        let idx = self
            .nvrm_maps
            .iter()
            .position(|s| s.owner == owner && s.handle == handle)?;
        Some(self.nvrm_maps.swap_remove(idx).mapping_id)
    }

    /// Pop one mapping `owner` still holds (device teardown).
    pub fn take_nvrm_map_for_owner(&mut self, owner: DeviceOwner) -> Option<(u32, u32)> {
        let idx = self.nvrm_maps.iter().position(|s| s.owner == owner)?;
        let s = self.nvrm_maps.swap_remove(idx);
        Some((s.handle, s.mapping_id))
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
    /// host outside the lock, one at a time).
    pub fn take_nvrm_handle_for_owner(&mut self, owner: DeviceOwner) -> Option<u32> {
        let idx = self.nvrm_handles.iter().position(|s| s.owner == owner)?;
        Some(self.nvrm_handles.swap_remove(idx).handle)
    }

    /// The transport generation, for `HeliosNvrmHeader.epoch`: it changes when
    /// the device is reset or the transport replaced, which is exactly when every
    /// backend handle of the earlier generation stopped existing. The wire fence
    /// base is already stride-separated per transport instance.
    pub fn nvrm_epoch(&self) -> u64 {
        self.wire_fence_base
    }
}

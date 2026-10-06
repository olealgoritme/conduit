//! The table half of `RM_RESOURCE_IMPORT` (`virtio/rm_resource_import.rs`): the
//! gating decision, read from the handle table and the foreign table in ONE lock
//! hold, and the re-check after the host round trip.
//!
//! Pure table work under the device spinlock: no allocation, nothing that waits.
//! The rule itself is `helios_kmd_logic::rm_resource_import::authorize`.

use super::*;
use helios_kmd_logic::rm_resource_import::{authorize, Refusal};

impl VirtioGpu {
    /// Whether `owner` may ask the host for a GEM handle of `resource_id` in its
    /// DRM file `rm_handle`: the handle is the caller's DRM node, the resource is
    /// a live foreign one the caller's device created or the caller's `process`
    /// holds an open of, and it is not destroyed. The handle's `device_type` and
    /// the foreign record are read in the same hold, so neither can be stale
    /// against the other.
    ///
    /// Returns the transport generation (`nvrm_epoch`) the answer holds in, for
    /// [`Self::rm_resource_import_still_valid`].
    pub fn rm_resource_import_begin(
        &self,
        owner: DeviceOwner,
        process: usize,
        rm_handle: u32,
        resource_id: u32,
    ) -> Result<u64, Refusal> {
        authorize(
            &self.foreign,
            self.nvrm_handle_device_type(owner, rm_handle),
            owner.raw() as u64,
            process as u64,
            rm_handle,
            resource_id,
        )?;
        Ok(self.nvrm_epoch())
    }

    /// After the round trip: the caller still owns `rm_handle` in the same
    /// transport generation. A handle closed during the wait may already name
    /// another process's file (the host reuses numbers), so a GEM handle made in
    /// it must not be reported as the caller's.
    pub fn rm_resource_import_still_valid(
        &self,
        owner: DeviceOwner,
        rm_handle: u32,
        epoch: u64,
    ) -> bool {
        self.nvrm_epoch() == epoch && self.nvrm_handle_owned(owner, rm_handle)
    }
}

//! RmResourceImport (docs/VENUS.md "RM-export resources in a second
//! process"): an RM-export resource made a GEM object of another render node
//! of the same guest.
//!
//! An RM-export blob is memory one NVK process rendered into, which the
//! backend holds as a dma-buf for the resource's whole life. A second NVK
//! process (a D3D app that opened the first one's shared surface, DWM
//! composing it) must map the same RM memory in its own RM client. It never
//! learns the first process's RM handles (the Helios KMD forbids that); it
//! names the resource instead, and the backend imports the dma-buf it holds
//! on the caller's render node (`PRIME_FD_TO_HANDLE`). nvidia-drm recognises
//! its own dma-buf and answers a handle to the same GEM object; NVK then does
//! what it does for any dma-buf on Linux: `DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY`
//! to a control descriptor of its own and `OS_UNIX_IMPORT_OBJECT_FROM_FD` into
//! its client (both forwarded ioctls, already served).
//!
//! A Venus blob (host Vulkan memory a Venus context allocated) is served the
//! same way when the renderer exported it as a dma-buf: spike X4 showed
//! NVIDIA's Vulkan driver's `DMA_BUF` exports import into an RM client
//! exactly (`OPAQUE_FD` ones do not, and are refused with `EINVAL`). Its
//! modifier is not known to the host; the reply says so.
//!
//! The resource id is the only name that crosses processes, so the KMD's
//! checks (the caller opened that resource) are the ones that matter; the
//! backend checks what it can see: the file is a render node of this guest
//! and the resource is an RM-export blob of this guest. The new handle's
//! layout is remembered like a GEM import's, so the caller may present or
//! re-export it with the right modifier.

use super::*;
#[cfg(feature = "venus")]
use protocol::messages::RM_RESOURCE_IMPORT_MODIFIER;
use protocol::messages::{RmResourceImport, RmResourceImportReply};
#[cfg(feature = "venus")]
use std::os::fd::AsRawFd;

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: `_IOWR('d', 0x2e, struct drm_prime_handle)`,
/// a 12-byte `{u32 handle; u32 flags; s32 fd}`.
pub(super) const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = 0xC00C_642E;

/// `PRIME_FD_TO_HANDLE` of `dmabuf` on `drm_fd`: the GEM handle.
pub(super) fn prime_import(
    host: &dyn HostDriver,
    drm_fd: RawFd,
    dmabuf: RawFd,
) -> std::result::Result<u32, i32> {
    let mut arg = [0u8; 12];
    arg[8..12].copy_from_slice(&dmabuf.to_le_bytes());
    host.ioctl(drm_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut arg)?;
    let handle = u32::from_le_bytes(arg[0..4].try_into().unwrap());
    if handle == 0 {
        return Err(libc::EIO);
    }
    Ok(handle)
}

impl NvidiaBackend {
    pub(super) fn handle_rm_resource_import(
        &mut self,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let Some(r) = RmResourceImport::from_bytes(payload) else {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, libc::EINVAL);
        };
        if r.flags != 0 || r.reserved != 0 {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, 0, libc::EINVAL);
        }
        match self.rm_resource_import(r) {
            Ok(reply) => {
                let n = self.write_hdr(resp_buf, 0, 0);
                let body = reply.to_bytes();
                if resp_buf.len() < n + body.len() {
                    return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
                }
                resp_buf[n..n + body.len()].copy_from_slice(&body);
                n + body.len()
            }
            Err(errno) => {
                log::warn!(
                    "rm resource import: resource {} on file {}: errno {errno}",
                    r.resource_id,
                    r.owner_handle
                );
                self.write_error_resp(resp_buf, Status::IoctlFailed, 0, errno)
            }
        }
    }

    #[cfg(feature = "venus")]
    fn rm_resource_import(
        &mut self,
        r: RmResourceImport,
    ) -> std::result::Result<RmResourceImportReply, i32> {
        let Some(venus) = self.venus.as_ref().filter(|v| v.rm_import()) else {
            return Err(libc::EOPNOTSUPP);
        };
        // A GEM handle means something only on a DRM file of this guest.
        let owner = r.owner_handle as u64;
        if !matches!(self.handle_kinds.get(&owner), Some(DeviceKind::Dri(_))) {
            return Err(libc::EBADF);
        }
        let drm_fd = self.handles.get_raw(owner).map_err(|_| libc::EBADF)?;
        let res = venus.rm_resource(r.resource_id)?;
        let gem = prime_import(&*self.host, drm_fd, res.dmabuf.as_raw_fd())?;
        let (size, modifier) = (res.size, res.modifier);
        // As if the caller had imported it with that layout itself. An
        // unknown layout (a Venus blob's) is not recorded as one: the
        // caller's own GEM import, if it makes one, says what it is.
        if modifier.is_some() {
            self.rm_layouts.insert(r.owner_handle, gem, modifier);
        }
        self.rm_resource_imports += 1;
        log::debug!(
            "rm resource import: resource {} is GEM handle {gem} on file {} ({size} bytes, \
             modifier {})",
            r.resource_id,
            r.owner_handle,
            modifier.map_or("unknown".into(), |m| format!("{m:#018x}"))
        );
        Ok(RmResourceImportReply {
            gem_handle: gem,
            flags: if modifier.is_some() {
                RM_RESOURCE_IMPORT_MODIFIER
            } else {
                0
            },
            size,
            modifier: modifier.unwrap_or(0),
        })
    }

    /// Without Venus there are no RM-export resources.
    #[cfg(not(feature = "venus"))]
    fn rm_resource_import(
        &mut self,
        _r: RmResourceImport,
    ) -> std::result::Result<RmResourceImportReply, i32> {
        Err(libc::EOPNOTSUPP)
    }

    /// `RmResourceImport`s served, for the teardown report and tests.
    pub fn rm_resource_imports(&self) -> u64 {
        self.rm_resource_imports
    }
}

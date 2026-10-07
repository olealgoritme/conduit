//! Explicit sync: nvidia-drm's semaphore-surface fences, as guest fences.
//!
//! The host driver hands out a fence as a sync_file descriptor, and a
//! descriptor in this process means nothing to the guest. So the descriptor
//! stays here, under a handle like any other file, and the guest is given the
//! handle: the transport watches it and sends one `EventReady` when it signals,
//! which is when the guest signals the dma_fence standing in for it. The
//! other direction -- the GPU waiting on a guest fence -- names such a handle
//! or, for a fence that has already signalled, 0. See docs/SYNC.md.

use super::*;

/// `DRM_COMMAND_BASE + DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE`: carries a pointer
/// to an NVKMS import block, so it goes through `dispatch_nested`.
pub(super) const SEMSURF_FENCE_CTX_CREATE: u32 = 0x54;
/// `... SEMSURF_FENCE_CREATE`: returns a sync_file in `fd`.
pub(super) const SEMSURF_FENCE_CREATE: u32 = 0x55;
/// `... SEMSURF_FENCE_WAIT`: takes a sync_file in `fd`.
pub(super) const SEMSURF_FENCE_WAIT: u32 = 0x56;
/// `... PRIME_FENCE_CONTEXT_CREATE`: two pointers and a memFd, none of which
/// the guest can name here. Not served (docs/SYNC.md).
pub(super) const PRIME_FENCE_CONTEXT_CREATE: u32 = 0x45;

/// `struct drm_nvidia_semsurf_fence_ctx_create_params`: u64 index, u64 ptr,
/// u64 size, u32 handle, u32 pad.
pub(super) const CTX_CREATE_SIZE: usize = 32;
pub(super) const CTX_CREATE_PTR: usize = 8;
pub(super) const CTX_CREATE_LEN: usize = 16;
/// `struct NvKmsKapiPrivImportSemaphoreSurfaceParams { NvHandle hClient;
/// NvHandle hSemaphoreSurface; NvU64 size; }`: the client sits first.
const IMPORT_HCLIENT: usize = 0;

/// `struct drm_nvidia_semsurf_fence_create_params`: u32 ctx, u32 timeout_ms,
/// u64 wait_value, s32 fd, u32 pad.
const CREATE_SIZE: usize = 24;
const CREATE_FD: usize = 16;
/// `struct drm_nvidia_semsurf_fence_wait_params`: u32 ctx, s32 fd,
/// u64 pre_wait_value, u64 post_wait_value.
const WAIT_SIZE: usize = 24;
const WAIT_FD: usize = 4;

/// Host fences one guest may hold unsignalled at once. Each is a descriptor
/// here, and the host driver times every one out within 5 s, so a guest near
/// this is not presenting -- it is leaking.
pub(super) const MAX_FENCES: usize = 4096;

/// The DRM core's syncobj ioctls, for the one signalled sync_file.
const fn drm_iowr(nr: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | ((b'd' as u64) << 8) | nr
}
const DRM_IOCTL_SYNCOBJ_CREATE: u64 = drm_iowr(0xbf, 8);
const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = drm_iowr(0xc0, 8);
const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u64 = drm_iowr(0xc1, 16);
const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1;
const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1;

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("a 4-byte field"))
}

impl NvidiaBackend {
    /// Whether the semaphore surface a context import names belongs to a
    /// client this guest was given. NVKMS dups the object from that client in
    /// kernel context, so the host driver is not the one to ask.
    pub(super) fn fence_ctx_import_allowed(&self, nested: &[u8]) -> bool {
        if nested.len() < IMPORT_HCLIENT + 4 {
            return false;
        }
        let client = word(nested, IMPORT_HCLIENT);
        let ok = self.vram.issued_anywhere(client);
        if !ok {
            log::warn!(
                "SEMSURF_FENCE_CTX_CREATE names client {client:#x}, not one of this guest's"
            );
        }
        ok
    }

    /// `SEMSURF_FENCE_CREATE`: make the host fence, keep its descriptor, and
    /// answer with the handle that names it.
    pub(super) fn dispatch_fence_create(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        if param_in.len() != CREATE_SIZE || self.current_data_len as usize != CREATE_SIZE {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        if self.fences.len() >= MAX_FENCES {
            log::warn!("SEMSURF_FENCE_CREATE: {MAX_FENCES} fences already outstanding");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EAGAIN);
        }
        let mut p = param_in.to_vec();
        if self.bounds.clamp_fence_create(&mut p) {
            log::warn!("SEMSURF_FENCE_CREATE: timeout clamped");
        }
        // -1 in, so a host that answers without writing the field is caught
        // rather than leaving the guest's number to be taken as ours.
        p[CREATE_FD..CREATE_FD + 4].copy_from_slice(&(-1i32).to_le_bytes());
        if let Err(errno) = self.host.ioctl(host_fd, request, &mut p) {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }
        let raw = word(&p, CREATE_FD) as i32;
        if raw < 0 {
            log::warn!("SEMSURF_FENCE_CREATE succeeded with no descriptor");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EIO);
        }
        // SAFETY: the host driver just installed this descriptor in this
        // process for this call; nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let handle = self.handles.insert(fd);
        self.fences.insert(handle);
        self.fence_watch_added.push((handle as u32, raw));
        p[CREATE_FD..CREATE_FD + 4].copy_from_slice(&(handle as u32).to_le_bytes());
        log::debug!("fence handle={handle} (fd={raw})");
        self.write_ioctl_resp(resp_buf, cookie, &p)
    }

    /// `SEMSURF_FENCE_WAIT`: the guest's `fd` field names one of our fence
    /// handles, or is 0 for a fence that has already signalled.
    pub(super) fn dispatch_fence_wait(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        if param_in.len() != WAIT_SIZE || self.current_data_len as usize != WAIT_SIZE {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        let named = word(param_in, WAIT_FD) as u64;
        let fd = if named == 0 {
            match self.signalled_sync_file(host_fd) {
                Ok(fd) => fd,
                Err(errno) => {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
            }
        } else if self.fences.contains(&named) {
            match self.handles.get_raw(named) {
                Ok(fd) => fd,
                Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
            }
        } else {
            // Closed already (the guest closes a fence once it signalled) or
            // never a fence. ENOENT tells the guest to retry with 0.
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOENT);
        };
        let mut p = param_in.to_vec();
        p[WAIT_FD..WAIT_FD + 4].copy_from_slice(&fd.to_le_bytes());
        if let Err(errno) = self.host.ioctl(host_fd, request, &mut p) {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }
        // The guest reads back what it sent, never a descriptor of ours.
        p[WAIT_FD..WAIT_FD + 4].copy_from_slice(&(named as u32).to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &p)
    }

    /// A sync_file that has signalled, made once from a signalled syncobj on
    /// the host's DRM node and kept: any number of waits can name it.
    pub(super) fn signalled_sync_file(&mut self, drm_fd: RawFd) -> std::result::Result<RawFd, i32> {
        use std::os::fd::AsRawFd;
        if let Some(fd) = self.signalled.as_ref() {
            return Ok(fd.as_raw_fd());
        }
        let mut create = [0u8; 8];
        create[4..8].copy_from_slice(&DRM_SYNCOBJ_CREATE_SIGNALED.to_le_bytes());
        self.host
            .ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut create)
            .inspect_err(|e| log::warn!("signalled sync_file: SYNCOBJ_CREATE: errno {e}"))?;
        let syncobj = word(&create, 0);
        let mut export = [0u8; 16];
        export[0..4].copy_from_slice(&syncobj.to_le_bytes());
        export[4..8]
            .copy_from_slice(&DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE.to_le_bytes());
        export[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        let exported = self
            .host
            .ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &mut export);
        let mut destroy = [0u8; 8];
        destroy[0..4].copy_from_slice(&syncobj.to_le_bytes());
        let _ = self
            .host
            .ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut destroy);
        exported.inspect_err(|e| log::warn!("signalled sync_file: HANDLE_TO_FD: errno {e}"))?;
        let raw = word(&export, 8) as i32;
        if raw < 0 {
            return Err(libc::EIO);
        }
        // SAFETY: just exported into this process for this call.
        self.signalled = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        Ok(raw)
    }
}

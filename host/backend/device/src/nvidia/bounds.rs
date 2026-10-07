//! How long a guest may make the host wait.
//!
//! Two forwarded calls carry a timeout the guest chose, and the host driver
//! honours it with a lock held that the host's own desktop needs:
//!
//! - `NV_ESC_RM_IDLE_CHANNELS` (`NVOS30_PARAMETERS`) takes the GPU group lock
//!   and idles the named engines for `timeout`.
//! - `DRM_NVIDIA_SEMSURF_FENCE_CREATE` carries `timeout_ms`, after which the
//!   host fence signals with an error.
//!
//! Both are clamped to a ceiling before they are forwarded; a request under the
//! ceiling is forwarded byte for byte. A timeout of 0 is left alone: for the
//! fence it means "the driver's default" (5000 ms, `nvidia-drm-ioctl.h`), and
//! for IDLE_CHANNELS it is RM's own default.
//!
//! # The ceilings
//!
//! The normal ones are generous on purpose: the driver's own default fence
//! timeout is 5 s, so no client that leaves the default is touched and one that
//! asks for twice that still gets it. IDLE_CHANNELS is clamped to 10 s, and
//! its field is taken to be in microseconds, as RM's timeout fields are (the
//! open source holds only the plumbing, not the unit; a wrong guess makes the
//! ceiling looser, never tighter than intended). Safe mode
//! (`CONDUIT_SAFE_MODE=1`) uses 1 s for both.

/// `NVOS30_PARAMETERS::timeout`: four u32 (hClient, hDevice, hChannel,
/// numChannels) to 16, three 8-byte pointers to 40, `flags` at 40, `timeout`
/// at 44, `status` at 48 (the struct is 56 bytes).
const NVOS30_TIMEOUT: usize = 44;
/// `drm_nvidia_semsurf_fence_create_params::timeout_ms`.
const FENCE_CREATE_TIMEOUT_MS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub idle_channels_us: u32,
    pub fence_timeout_ms: u32,
}

impl Bounds {
    pub const NORMAL: Bounds = Bounds {
        idle_channels_us: 10_000_000,
        fence_timeout_ms: 10_000,
    };
    pub const SAFE: Bounds = Bounds {
        idle_channels_us: 1_000_000,
        fence_timeout_ms: 1_000,
    };

    /// `NVOS30_PARAMETERS`, clamped. Returns whether it changed.
    pub fn clamp_idle_channels(&self, p: &mut [u8]) -> bool {
        clamp_word(p, NVOS30_TIMEOUT, self.idle_channels_us)
    }

    /// `drm_nvidia_semsurf_fence_create_params`, clamped.
    pub fn clamp_fence_create(&self, p: &mut [u8]) -> bool {
        clamp_word(p, FENCE_CREATE_TIMEOUT_MS, self.fence_timeout_ms)
    }
}

fn clamp_word(p: &mut [u8], at: usize, max: u32) -> bool {
    let Some(w) = p.get_mut(at..at + 4) else {
        return false;
    };
    let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
    if v > max {
        w.copy_from_slice(&max.to_le_bytes());
        true
    } else {
        false
    }
}

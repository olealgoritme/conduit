//! `HELIOS_ESCAPE_ECL_DONE`: the D3D12 UMD tells the KMD that ExecuteCommandLists
//! batches have finished, so their HE12 v4 packets complete without waiting for the
//! host's `EventReady` (`guest/windows/docs/rm-fence-marker.md`, "Completion hints").
//!
//! An HE12 v4 packet withholds its DMA completion until the batch's ECL fence fires.
//! The fence fires on the host (nvidia-drm semaphore-surface callback, the backend's
//! event thread, the virtio event queue, the KMD's DPC). The engine's queue worker in
//! the guest can see the batch finish earlier, by reading the batch's completion
//! timeline (CPU-visible memory the GPU writes right after the batch). With
//! `Nvk12CompletionHint` on it names the fences of the batches it saw finish here,
//! and the KMD fires those fences at once. The host's `EventReady` that follows is a
//! repeat and changes nothing.
//!
//! The KMD fires only a fence that is attached to the calling process's RM gate
//! (an HE12 v4 or present marker of that process) and has not fired yet; anything
//! else is counted and ignored. A process can therefore only complete its own
//! packets early, which is what a buggy hint costs: its own fence signals and
//! presents run ahead of its own work.
//!
//! Layout (96 bytes): the header, `count` (in), three result counts (out), then up
//! to [`HELIOS_ECL_DONE_MAX`] fence handles. The escape needs a device (`hDevice`):
//! its `hKmdProcess` is the process the fences must belong to.

use bytemuck::{Pod, Zeroable};

use crate::escape::{HeliosEscapeHeader, HELIOS_ESCAPE_MAGIC, HELIOS_ESCAPE_VERSION};

/// The escape verb. `0x0016` is NVRM, `0x0017` the Venus submit batch, `0x0018`
/// the foreign resource.
pub const HELIOS_ESCAPE_ECL_DONE: u32 = 0x0019;

/// Most fence handles one escape names.
pub const HELIOS_ECL_DONE_MAX: usize = 16;

/// The `HELIOS_ESCAPE_ECL_DONE` buffer. 96 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosEclDone {
    pub hdr: HeliosEscapeHeader,
    /// In: how many entries of `fences` are valid (1..=[`HELIOS_ECL_DONE_MAX`]).
    pub count: u32,
    /// Out: fences this call fired.
    pub out_fired: u32,
    /// Out: fences that had fired already (the host's `EventReady` or an earlier
    /// hint came first).
    pub out_late: u32,
    /// Out: handles that are not a fence attached to this process's RM gate
    /// (unknown or closed, another process's, not attached yet).
    pub out_refused: u32,
    /// In: the fence handles, as the HE12 v4 records named them.
    pub fences: [u32; HELIOS_ECL_DONE_MAX],
}

const _: () = assert!(core::mem::size_of::<HeliosEclDone>() == 96);

impl HeliosEclDone {
    /// A request naming `fences` (at most [`HELIOS_ECL_DONE_MAX`]; extra ones are
    /// left out, and `count` says how many went in).
    pub fn new(fences: &[u32]) -> Self {
        let mut req = Self::zeroed();
        let n = fences.len().min(HELIOS_ECL_DONE_MAX);
        req.hdr =
            HeliosEscapeHeader::new(HELIOS_ESCAPE_ECL_DONE, core::mem::size_of::<Self>() as u32);
        req.count = n as u32;
        req.fences[..n].copy_from_slice(&fences[..n]);
        req
    }

    /// The valid handles of a request the KMD read, or `None` when the request is
    /// malformed: wrong magic or version, a header size smaller than the struct,
    /// or a `count` outside 1..=[`HELIOS_ECL_DONE_MAX`].
    pub fn valid_fences(&self) -> Option<&[u32]> {
        let n = self.count as usize;
        if self.hdr.magic != HELIOS_ESCAPE_MAGIC
            || self.hdr.version != HELIOS_ESCAPE_VERSION
            || self.hdr.cmd_type != HELIOS_ESCAPE_ECL_DONE
            || (self.hdr.size as usize) < core::mem::size_of::<Self>()
            || n == 0
            || n > HELIOS_ECL_DONE_MAX
        {
            return None;
        }
        Some(&self.fences[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        assert_eq!(core::mem::size_of::<HeliosEclDone>(), 96);
        assert_eq!(core::mem::offset_of!(HeliosEclDone, count), 16);
        assert_eq!(core::mem::offset_of!(HeliosEclDone, out_refused), 28);
        assert_eq!(core::mem::offset_of!(HeliosEclDone, fences), 32);
    }

    #[test]
    fn new_request_is_valid_and_clamped() {
        let req = HeliosEclDone::new(&[7, 9]);
        assert_eq!(req.valid_fences(), Some(&[7u32, 9][..]));
        assert_eq!(req.hdr.size, 96);

        let many: [u32; 20] = core::array::from_fn(|i| i as u32 + 1);
        let req = HeliosEclDone::new(&many);
        assert_eq!(req.count as usize, HELIOS_ECL_DONE_MAX);
        assert_eq!(req.valid_fences().unwrap(), &many[..HELIOS_ECL_DONE_MAX]);
    }

    #[test]
    fn malformed_requests_are_refused() {
        assert_eq!(HeliosEclDone::new(&[]).valid_fences(), None);

        let mut req = HeliosEclDone::new(&[1]);
        req.count = HELIOS_ECL_DONE_MAX as u32 + 1;
        assert_eq!(req.valid_fences(), None);

        let mut req = HeliosEclDone::new(&[1]);
        req.hdr.size = 95;
        assert_eq!(req.valid_fences(), None);

        let mut req = HeliosEclDone::new(&[1]);
        req.hdr.magic ^= 1;
        assert_eq!(req.valid_fences(), None);

        let mut req = HeliosEclDone::new(&[1]);
        req.hdr.cmd_type = crate::escape::HELIOS_ESCAPE_QUERY_STATS;
        assert_eq!(req.valid_fences(), None);
    }
}

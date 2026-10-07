//! STUB of the producer's cross-client dup and GPU mapping (`NV_ESC_RM_DUP_OBJECT` of the
//! record's semaphore memory and source image into the copy-engine channel's client, then
//! `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` in its VA space). The real module is written beside
//! the shadow mode (M3c-1, `RmCopyEngine` = 3) and replaces this file whole, with the same surface:
//!
//! * [`Producer`]: the GPU VAs, in the channel's VA space, of the semaphore ENTRY (the record's
//!   `semaphore.offset` included) and of the source image's base (its `source.offset` NOT
//!   included: the route adds `SourcePlan::offset`).
//! * [`dup_map_record`]: cached per `(h_client, h_memory)`; the source mapped with the PTE kind of
//!   the record's modifier; a failure is never fatal (the route takes the Venus copy).
//! * [`release_all`]: unmap and free every dup (the channel's teardown, before its client goes).
//!
//! Until then every call fails with [`NOT_IMPLEMENTED`], so the route (M3c-2) always falls back
//! to the Venus copy at dispatch (`CeRtWhy` 19, `Dup`), after its decision, its queueing and its
//! destination descriptor ran.

use super::ce_channel::Handles;
use super::Io;
use crate::device::StashedCeRecord;
use helios_kmd_logic::ce_present::Gen;
use helios_kmd_logic::rm_client::{Fail, FailKind};

/// The producer's semaphore entry and image base in the channel's VA space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Producer {
    pub sem_va: u64,
    pub src_va: u64,
}

/// What the stub answers: `Layout` 0x7E, "not implemented".
pub(crate) const NOT_IMPLEMENTED: Fail = Fail::new(FailKind::Layout, 0x7E);

/// Dup and map the record's semaphore memory and source image (STUB: always [`NOT_IMPLEMENTED`]).
pub(crate) fn dup_map_record(
    _io: &Io<'_>,
    _h: &Handles,
    _rec: &StashedCeRecord,
    _gen: Gen,
) -> Result<Producer, Fail> {
    Err(NOT_IMPLEMENTED)
}

/// Unmap and free every dup (STUB: there are none).
pub(crate) fn release_all(_io: &Io<'_>, _h: &Handles) {}

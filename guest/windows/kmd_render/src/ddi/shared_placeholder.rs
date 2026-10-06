//! Shared placeholder allocations: the I/O half. The decision is
//! `helios_kmd_logic::shared_placeholder` (host-tested); this file derives its inputs from the
//! private data, counts what happened, and nothing else. The backing itself (host-less,
//! resource id 0) is built in `create_allocation.rs` next to every other backing class.
//! Design: `docs/shared-foreign-surfaces.md`, "Shared placeholder allocations".
//!
//! Counters (names at most 13 characters; PASSIVE only, the first event and every 64th reach
//! the registry, the rest are atomics):
//!
//! * `ShPhMade`: placeholders created (host-less shared STANDARD allocations).
//! * `ShPhBytes`: the size of the last one.
//! * `ShPhRefuse`: placeholders refused with `STATUS_NO_MEMORY` (a soft per-resource failure);
//!   `ShPhWhy` holds the last reason code (`Refusal::code`, 0x10 = too large).
//! * `ShPhNear`: shared STANDARD allocations with adopt id 0 and context 0 that were NOT
//!   placeholders and took the ordinary path (an unexpected private-data size or an identity
//!   bit); `ShPhWhy` holds the last `Existing::code`.
//! * `ShPhOpen`: opens of an identity-less allocation of the placeholder shape (any process,
//!   including the creator's own device open). Such an open succeeds with no identity: the
//!   opener's Present rules see an unresolved allocation (`present_foreign`).
//! * `ShPhFree`: placeholders destroyed.

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::pod_read_unaligned;
use helios_kmd_logic::shared_placeholder::{self as sp, identity};
use helios_protocol::{
    HeliosWddmAllocMeta, HeliosWddmAllocPrivate, HELIOS_BLOB_MEM_RM_EXPORT,
    HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT, HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK,
    HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE, HELIOS_WDDM_ALLOC_MISC_PRIMARY,
    HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK, HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER,
};

// The kind `helios_kmd_logic` spells as a literal is the protocol's, and the private-data size
// it names is the two records it describes.
const _: () = assert!(
    sp::KIND_STANDARD == helios_protocol::HELIOS_WDDM_ALLOC_KIND_STANDARD
        && sp::PLACEHOLDER_PRIVATE_BYTES as usize
            == size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>()
);

static MADE: AtomicU32 = AtomicU32::new(0);
static REFUSED: AtomicU32 = AtomicU32::new(0);
static NEAR: AtomicU32 = AtomicU32::new(0);
static OPENED: AtomicU32 = AtomicU32::new(0);
static FREED: AtomicU32 = AtomicU32::new(0);

/// The [`identity`] bits of one allocation's private data. `misc_flags` is the meta's (zero
/// when the meta is absent); `layout_trailer` is whether a valid layout trailer is present.
pub(crate) fn identity_bits(
    ap: &HeliosWddmAllocPrivate,
    misc_flags: u32,
    layout_trailer: bool,
) -> u32 {
    let mut bits = 0;
    if ap.blob_id != 0 {
        bits |= identity::BLOB_ID;
    }
    if ap.blob_mem == HELIOS_BLOB_MEM_RM_EXPORT {
        bits |= identity::RM_EXPORT;
    }
    if ap.blob_flags & HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER != 0 {
        bits |= identity::TRACKER;
    }
    if layout_trailer {
        bits |= identity::LAYOUT_TRAILER;
    }
    if misc_flags & HELIOS_WDDM_ALLOC_MISC_PRIMARY != 0 {
        bits |= identity::PRIMARY;
    }
    if misc_flags & HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE != 0 {
        bits |= identity::GDI_TEXTURE;
    }
    if misc_flags & HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT != 0 {
        bits |= identity::DIRECT_SCANOUT;
    }
    if misc_flags & (HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK | HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK)
        != 0
    {
        bits |= identity::STANDARD_TYPE;
    }
    bits
}

/// The decision's input for one create-time allocation.
pub(crate) fn create_input(
    ap: &HeliosWddmAllocPrivate,
    create_flags: u32,
    private_size: usize,
    misc_flags: u32,
    layout_trailer: bool,
) -> sp::Input {
    sp::Input {
        kind: ap.kind,
        create_flags,
        adopt_resource_id: ap.adopt_resource_id,
        ctx_id: ap.ctx_id,
        private_size: private_size.min(u32::MAX as usize) as u32,
        size: ap.size,
        identity: identity_bits(ap, misc_flags, layout_trailer),
    }
}

/// Count a decision that was not ordinary and unremarkable: a refusal (the caller then fails
/// soft) or a near miss. A `Placeholder` verdict is counted by [`note_created`] once the
/// allocation exists.
pub(crate) fn note_verdict(input: &sp::Input, verdict: sp::Verdict) {
    match verdict {
        sp::Verdict::Refuse(r) => {
            let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 64 == 0 {
                crate::diag::record_named_bytes(b"ShPhWhy", r.code());
                crate::diag::record_named_bytes(b"ShPhRefuse", n);
            }
        }
        sp::Verdict::Existing(why)
            if input.kind == sp::KIND_STANDARD
                && sp::is_shared(input.create_flags)
                && input.adopt_resource_id == 0
                && input.ctx_id == 0 =>
        {
            let n = NEAR.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 64 == 0 {
                crate::diag::record_named_bytes(b"ShPhWhy", why.code());
                crate::diag::record_named_bytes(b"ShPhNear", n);
            }
        }
        _ => {}
    }
}

/// A placeholder of `size` bytes was created.
pub(crate) fn note_created(size: u64) {
    let n = MADE.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"ShPhBytes", size.min(u64::from(u32::MAX)) as u32);
        crate::diag::record_named_bytes(b"ShPhMade", n);
    }
}

/// A placeholder was destroyed.
pub(crate) fn note_destroyed() {
    let n = FREED.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"ShPhFree", n);
    }
}

/// An open found no identity in the allocation's private data. Count it when the data has the
/// placeholder's shape (the open has no creation flags, so the shared test is taken as met).
///
/// # Safety
/// `private` is null or valid for `size` bytes (the DDI contract for the open's buffer).
pub(crate) unsafe fn note_identityless_open(
    private: *const c_void,
    size: u32,
    misc_flags: u32,
) {
    if private.is_null() || size as usize != size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>() {
        return;
    }
    // SAFETY: `size` >= 48 was just checked; read unaligned, like every private-data read.
    let bytes =
        unsafe { core::slice::from_raw_parts(private as *const u8, size_of::<HeliosWddmAllocPrivate>()) };
    let ap: HeliosWddmAllocPrivate = pod_read_unaligned(bytes);
    if !ap.is_valid() {
        return;
    }
    let input = create_input(&ap, 0, size as usize, misc_flags, false);
    if !sp::has_placeholder_shape(&input) {
        return;
    }
    let n = OPENED.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"ShPhOpen", n);
    }
}

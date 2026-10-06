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
//!   `ShPhRefWhy` holds the last reason code (`Refusal::code`, 0x10 = too large).
//! * `ShPhNear`: shared STANDARD allocations with adopt id 0 and context 0 that were NOT
//!   placeholders and took the ordinary path (an unexpected private-data size or an identity
//!   bit); `ShPhNearWhy` holds the last `Existing::code`.
//! * Making a wrong shared-bit assumption visible: `ShPhShape` counts identity-less STANDARD
//!   allocations (adopt 0, ctx 0, no identity bit) whatever their flags and size, and
//!   `ShPhFl1`..`ShPhFl8` hold the creation-flags word of the first eight of them (a shared
//!   texture should read 3). `ShPhNotSh` counts those whose shared bit was CLEAR (so they took
//!   the ordinary path), with `ShPhFlg` / `ShPhPriv` the flags word and private size of the
//!   last one. If bit 1 is not `CreateShared`, the dump shows the shape arriving here.
//! * `CrPrivSmall` / `CrApInvalid`: the two early refusals of `create_one` before any decision
//!   (private data shorter than 48 bytes, value = its length; a record whose magic or version
//!   is wrong, value = the magic word).
//! * `ShPhOpen`: opens of an identity-less allocation of the placeholder shape (any process,
//!   including the creator's own device open). Such an open succeeds with no identity: the
//!   opener's Present rules see an unresolved allocation (`present_foreign`).
//! * `ShPhFree`: placeholders destroyed.

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use bytemuck::pod_read_unaligned;
use helios_kmd_logic::shared_placeholder::{self as sp, words};
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

// The protocol words `helios_kmd_logic::shared_placeholder::identity_bits` spells as literals.
const _: () = assert!(
    words::BLOB_MEM_RM_EXPORT == HELIOS_BLOB_MEM_RM_EXPORT
        && words::BLOB_FLAG_GLOBAL_VIDMM_TRACKER == HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER
        && words::MISC_PRIMARY == HELIOS_WDDM_ALLOC_MISC_PRIMARY
        && words::MISC_DIRECT_SCANOUT == HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT
        && words::MISC_OPTIMAL_GDI_TEXTURE == HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE
        && words::MISC_STANDARD_TYPE_MASK == HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK
        && words::MISC_GDI_TYPE_MASK == HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK
);

static MADE: AtomicU32 = AtomicU32::new(0);
static REFUSED: AtomicU32 = AtomicU32::new(0);
static NEAR: AtomicU32 = AtomicU32::new(0);
static OPENED: AtomicU32 = AtomicU32::new(0);
static FREED: AtomicU32 = AtomicU32::new(0);
static SHAPE: AtomicU32 = AtomicU32::new(0);
static NOT_SHARED: AtomicU32 = AtomicU32::new(0);
static EARLY_SMALL: AtomicU32 = AtomicU32::new(0);
static EARLY_INVALID: AtomicU32 = AtomicU32::new(0);

/// How many identity-less STANDARD allocations have their creation flags recorded one by one.
const FLAG_SAMPLES: u32 = 8;

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
        identity: sp::identity_bits(&sp::PrivateFacts {
            blob_id: ap.blob_id,
            blob_mem: ap.blob_mem,
            blob_flags: ap.blob_flags,
            misc_flags,
            layout_trailer,
        }),
    }
}

/// Count a decision that was not ordinary and unremarkable: a refusal (the caller then fails
/// soft), a near miss, and every identity-less STANDARD allocation with its flags (so a wrong
/// shared-bit assumption shows). A `Placeholder` verdict is counted by [`note_created`] once the
/// allocation exists.
pub(crate) fn note_verdict(input: &sp::Input, verdict: sp::Verdict) {
    if sp::is_identityless_standard(input) {
        let n = SHAPE.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= FLAG_SAMPLES {
            // `ShPhFl1`..`ShPhFl8`.
            let name = [b'S', b'h', b'P', b'h', b'F', b'l', b'0' + n as u8];
            crate::diag::record_named_bytes(&name, input.create_flags);
        }
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"ShPhShape", n);
        }
        if !sp::is_shared(input.create_flags) {
            let m = NOT_SHARED.fetch_add(1, Ordering::Relaxed) + 1;
            if m <= FLAG_SAMPLES || m % 64 == 0 {
                crate::diag::record_named_bytes(b"ShPhFlg", input.create_flags);
                crate::diag::record_named_bytes(b"ShPhPriv", input.private_size);
                crate::diag::record_named_bytes(b"ShPhNotSh", m);
            }
        }
    }
    match verdict {
        sp::Verdict::Refuse(r) => {
            let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 64 == 0 {
                crate::diag::record_named_bytes(b"ShPhRefWhy", r.code());
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
                crate::diag::record_named_bytes(b"ShPhNearWhy", why.code());
                crate::diag::record_named_bytes(b"ShPhNear", n);
            }
        }
        _ => {}
    }
}

/// `create_one` refused the private data before any decision: shorter than the 48-byte record
/// (`value` = its length) or not a valid record (`value` = its magic word). First and every
/// 64th each.
pub(crate) fn note_early_refusal(too_small: bool, value: u32) {
    if too_small {
        let n = EARLY_SMALL.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"CrPrivSmall", value);
        }
    } else {
        let n = EARLY_INVALID.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"CrApInvalid", value);
        }
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

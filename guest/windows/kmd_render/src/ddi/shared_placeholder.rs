//! Placeholder allocations: the I/O half. The decision is
//! `helios_kmd_logic::shared_placeholder` (host-tested); this file derives its inputs from the
//! private data, counts what happened, and nothing else. The backing itself (host-less,
//! resource id 0) is built in `create_allocation.rs` next to every other backing class.
//! Design: `docs/shared-foreign-surfaces.md`, "Shared placeholder allocations".
//!
//! Counters (names at most 14 characters; PASSIVE only, the first event and every 64th reach
//! the registry, the rest are atomics; zeroed at every StartDevice by [`reset_for_start`]):
//!
//! * `ShPhMade`: placeholders created (host-less identity-less STANDARD allocations; sharing is
//!   NOT part of the decision). `ShPhShared` / `ShPhUnsh`: of those, whose creator's meta
//!   `misc_flags` declared sharing (D3D10 DDI `SHARED` 0x2 / `SHARED_KEYEDMUTEX` 0x100: the only
//!   place the shared intent is visible, the UMD sets no shared flag in `D3DDDICB_ALLOCATE` /
//!   `ALLOCATIONINFO2`) / did not (`ShPhMade == ShPhShared + ShPhUnsh`).
//! * `ShPhRet`: the NTSTATUS `create_one` last returned for the placeholder SHAPE (0 = success;
//!   written for the first eight shape returns, every failure's first eight, then every 64th),
//!   `ShPhFail` the number of failures of the shape.
//! * `ShPhBytes`: the size of the last one.
//! * `ShPhRefuse`: placeholders refused with `STATUS_NO_MEMORY` (a soft per-resource failure);
//!   `ShPhRefWhy` holds the last reason code (`Refusal::code`, 0x10 = too large).
//! * `ShPhNear`: identity-less STANDARD allocations (adopt 0, ctx 0, no identity bit) that were
//!   NOT placeholders (an unexpected private-data size); `ShPhNearWhy` holds the last
//!   `Existing::code`.
//! * How the shared intent is signalled, if it is: `ShPhShape` counts identity-less STANDARD
//!   allocations whatever their size. `ShPhFl1..8` hold the `DXGKARG_CREATEALLOCATION.Flags` word
//!   of the first eight, `ShPhIf1..8` the `DXGK_ALLOCATIONINFO.Flags` word of the first eight
//!   (as the runtime handed it in), `ShPhRs1..8` the resource facts (bit 0 = a resource create,
//!   bits 8..15 = NumAllocations, bits 16..23 = the allocation index). `ShPhFlg`, `ShPhPriv`,
//!   `ShPhInfFlg`, `ShPhRes` are the same four values of the most recent one written (the first
//!   eight and every 64th). `ShPhNotSh` counts those whose creation-flags bit 1 was clear (the
//!   kernel-mode flags word has no shared bit; v323 hardware read 1).
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
static MADE_SHARED: AtomicU32 = AtomicU32::new(0);
static MADE_UNSH: AtomicU32 = AtomicU32::new(0);
static RET_SEEN: AtomicU32 = AtomicU32::new(0);
static RET_FAIL: AtomicU32 = AtomicU32::new(0);
static EARLY_SMALL: AtomicU32 = AtomicU32::new(0);
static EARLY_INVALID: AtomicU32 = AtomicU32::new(0);

/// A new generation (StartDevice): zero the counters and write zeros over their service-key
/// values. These are written only when an event happens, so without this a value from an
/// earlier run stays readable as this one's until its event recurs. PASSIVE.
pub(crate) fn reset_for_start() {
    for c in [
        &MADE,
        &REFUSED,
        &NEAR,
        &OPENED,
        &FREED,
        &SHAPE,
        &NOT_SHARED,
        &EARLY_SMALL,
        &EARLY_INVALID,
        &MADE_SHARED,
        &MADE_UNSH,
        &RET_SEEN,
        &RET_FAIL,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    use crate::diag::record_named_bytes as rec;
    for name in [
        &b"ShPhMade"[..],
        b"ShPhBytes",
        b"ShPhRefuse",
        b"ShPhRefWhy",
        b"ShPhNear",
        b"ShPhNearWhy",
        b"ShPhShape",
        b"ShPhNotSh",
        b"ShPhShared",
        b"ShPhUnsh",
        b"ShPhRet",
        b"ShPhFail",
        b"ShPhFlg",
        b"ShPhPriv",
        b"ShPhInfFlg",
        b"ShPhRes",
        b"ShPhOpen",
        b"ShPhFree",
        b"CrPrivSmall",
        b"CrApInvalid",
    ] {
        rec(name, 0);
    }
    let mut n = 1u8;
    while n <= FLAG_SAMPLES as u8 {
        rec(&[b'S', b'h', b'P', b'h', b'F', b'l', b'0' + n], 0);
        rec(&[b'S', b'h', b'P', b'h', b'I', b'f', b'0' + n], 0);
        rec(&[b'S', b'h', b'P', b'h', b'R', b's', b'0' + n], 0);
        n += 1;
    }
}

/// How many identity-less STANDARD allocations have their creation flags recorded one by one.
const FLAG_SAMPLES: u32 = 8;

/// Where in the create call an allocation sits: the facts that, with the flags words, say how
/// a shared intent reaches the KMD.
#[derive(Clone, Copy)]
pub(crate) struct Where {
    /// `DXGK_ALLOCATIONINFO.Flags` as the runtime handed it in (read before the KMD writes it).
    pub info_flags: u32,
    /// The call creates a resource (`DXGKARG_CREATEALLOCATION.Flags.Resource`).
    pub resource: bool,
    /// `DXGKARG_CREATEALLOCATION.NumAllocations`.
    pub num_allocations: u32,
    /// This allocation's index in `pAllocationInfo`.
    pub index: u32,
}

impl Where {
    /// Resource facts packed for the registry: bit 0 resource create, bits 8..15 the number of
    /// allocations, bits 16..23 the index.
    fn packed(self) -> u32 {
        u32::from(self.resource)
            | (self.num_allocations.min(0xFF) << 8)
            | (self.index.min(0xFF) << 16)
    }
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
/// soft), a near miss, and every identity-less STANDARD allocation with its flags words (so the
/// next dump says how a shared intent is signalled). A `Placeholder` verdict is counted by
/// [`note_created`] once the allocation exists.
pub(crate) fn note_verdict(input: &sp::Input, verdict: sp::Verdict, at: Where) {
    if sp::is_identityless_standard(input) {
        let n = SHAPE.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= FLAG_SAMPLES {
            // `ShPhFl1..8`, `ShPhIf1..8`, `ShPhRs1..8`.
            let d = b'0' + n as u8;
            crate::diag::record_named_bytes(
                &[b'S', b'h', b'P', b'h', b'F', b'l', d],
                input.create_flags,
            );
            crate::diag::record_named_bytes(&[b'S', b'h', b'P', b'h', b'I', b'f', d], at.info_flags);
            crate::diag::record_named_bytes(&[b'S', b'h', b'P', b'h', b'R', b's', d], at.packed());
        }
        if n <= FLAG_SAMPLES || n % 64 == 0 {
            crate::diag::record_named_bytes(b"ShPhFlg", input.create_flags);
            crate::diag::record_named_bytes(b"ShPhPriv", input.private_size);
            crate::diag::record_named_bytes(b"ShPhInfFlg", at.info_flags);
            crate::diag::record_named_bytes(b"ShPhRes", at.packed());
            crate::diag::record_named_bytes(b"ShPhShape", n);
        }
        if !sp::is_shared(input.create_flags) {
            let m = NOT_SHARED.fetch_add(1, Ordering::Relaxed) + 1;
            if m <= FLAG_SAMPLES || m % 64 == 0 {
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
            if input.kind == sp::KIND_STANDARD && input.adopt_resource_id == 0 && input.ctx_id == 0 =>
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

/// A placeholder of `size` bytes was created. `creator_shared`: the creator's meta declared
/// sharing (`ShPhShared`, else `ShPhUnsh`); not part of the decision.
pub(crate) fn note_created(size: u64, creator_shared: bool) {
    let n = MADE.fetch_add(1, Ordering::Relaxed) + 1;
    let (counter, name): (&AtomicU32, &[u8]) = if creator_shared {
        (&MADE_SHARED, b"ShPhShared")
    } else {
        (&MADE_UNSH, b"ShPhUnsh")
    };
    let k = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"ShPhBytes", size.min(u64::from(u32::MAX)) as u32);
        crate::diag::record_named_bytes(b"ShPhMade", n);
        crate::diag::record_named_bytes(name, k);
    } else if k == 1 {
        // The first of either kind is always visible, even when the other kind got there first.
        crate::diag::record_named_bytes(name, k);
    }
}

/// `create_one` returned `result` for an allocation of the placeholder SHAPE (identity-less
/// STANDARD, whatever its size): the status is the breadcrumb that says whether the shape still
/// fails anywhere, and where (`ShPhRet`, with `ShPhFail` counting the failures).
pub(crate) fn note_shape_return(result: Result<(), i32>) {
    let status = match result {
        Ok(()) => 0u32,
        Err(status) => status as u32,
    };
    let seen = RET_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    let fails = if status != 0 {
        RET_FAIL.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        RET_FAIL.load(Ordering::Relaxed)
    };
    let record = if status != 0 {
        fails <= FLAG_SAMPLES || fails % 64 == 0
    } else {
        seen <= FLAG_SAMPLES || seen % 64 == 0
    };
    if record {
        crate::diag::record_named_bytes(b"ShPhRet", status);
        if status != 0 {
            crate::diag::record_named_bytes(b"ShPhFail", fails);
        }
    }
}

/// A placeholder was destroyed.
pub(crate) fn note_destroyed() {
    let n = FREED.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"ShPhFree", n);
    }
}

/// Whether an open that found no identity in the allocation's private data has the placeholder's
/// shape (the open has no creation flags, and the decision does not read them). The KMD's own
/// record of a host-less placeholder at open time: flips of it complete as kept pictures
/// (`helios_kmd_logic::flip_completion`, the Present's identity-less flip arm).
///
/// # Safety
/// `private` is null or valid for `size` bytes (the DDI contract for the open's buffer).
pub(crate) unsafe fn identityless_open_is_placeholder(
    private: *const c_void,
    size: u32,
    misc_flags: u32,
) -> bool {
    if private.is_null() || size as usize != size_of::<HeliosWddmAllocPrivate>() + size_of::<HeliosWddmAllocMeta>() {
        return false;
    }
    // SAFETY: `size` >= 48 was just checked; read unaligned, like every private-data read.
    let bytes =
        unsafe { core::slice::from_raw_parts(private as *const u8, size_of::<HeliosWddmAllocPrivate>()) };
    let ap: HeliosWddmAllocPrivate = pod_read_unaligned(bytes);
    if !ap.is_valid() {
        return false;
    }
    let input = create_input(&ap, 0, size as usize, misc_flags, false);
    sp::has_placeholder_shape(&input)
}

/// An open found no identity in the allocation's private data. Count it when the data has the
/// placeholder's shape (see [`identityless_open_is_placeholder`]); returns whether it had.
///
/// # Safety
/// `private` is null or valid for `size` bytes (the DDI contract for the open's buffer).
pub(crate) unsafe fn note_identityless_open(
    private: *const c_void,
    size: u32,
    misc_flags: u32,
) -> bool {
    if !unsafe { identityless_open_is_placeholder(private, size, misc_flags) } {
        return false;
    }
    let n = OPENED.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"ShPhOpen", n);
    }
    true
}

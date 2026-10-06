//! Placeholder allocations: the pure decision. An NVK process that cannot mint a KMD resource
//! id for a texture (A8, R10G10B10A2, fp16, R8G8, NV12 without the shared-format cap, BGRA8
//! with `NVK_HELIOS_RESID=0`) still has to hand the D3D runtime a WDDM allocation, so it
//! creates a PLACEHOLDER: a `STANDARD` allocation with no identity (adopt id 0, context 0, the
//! 96-byte private data). The real content is exchanged by NVK through other means; the
//! placeholder is never flipped, scanned out or copied.
//!
//! The ordinary `STANDARD` path backs such an allocation with a real Venus present buffer
//! (a Vulkan buffer and device memory on the host Venus renderer, a registered
//! dedicated-Present-buffer row, BAR placement). That machinery exists for the KMD-originated
//! DWM / IddCx surfaces, needs a working Venus client, and made `pfnAllocateCb` fail for the
//! shared placeholder, which the runtime turns into `DXGI_ERROR_DEVICE_REMOVED`. A placeholder
//! therefore takes a HOST-LESS backing instead: resource id 0 (the "unbacked allocation" every
//! resource-keyed path already treats as nothing), aperture placement like every adopted
//! allocation, and no identity written back. An opener sees an identity-less allocation
//! (`present: None`), exactly the "foreign-unknown" case the Present rules already skip and
//! count (`present_foreign`), never a Venus resource it could misread.
//!
//! SHARED OR NOT IS NOT PART OF THE DECISION. The first design gated on
//! `DXGKARG_CREATEALLOCATION::Flags` bit 1 (`CreateShared`); the hardware run (v323) showed
//! the placeholder arriving with flags 1 (`Resource` only): the kernel-mode flags word has no
//! shared bit (the only header on disk, Win8-era, has `Resource : 1, Reserved : 31`), and
//! `DXGK_ALLOCATIONINFOFLAGS` has none either. The KMD is not told at create time that an
//! allocation will be shared (the creator's `misc_flags` in the meta is its own, untrusted,
//! word). So the shape alone decides, and the flags words are only recorded, to say how the
//! shared intent is signalled if it is signalled at all.
//!
//! This module only DECIDES. The rule is deliberately narrow: an allocation that carries ANY
//! identity (an adopt id, a context, a blob id, a declared RM-export memory type, a tracker
//! flag, a layout trailer, a primary / GDI / scan-out / standard-type bit) is never a
//! placeholder and takes the ordinary path with exactly today's validation. The KMD-originated
//! standard allocations (`GetStandardAllocationDriverData`: shared primary, shadow, staging,
//! GDI surface) always carry the standard-type bits (enum values 1..4), so they stop at the
//! identity row even when the KMD has no Venus context to put in them.
//! Design and the table: `docs/shared-foreign-surfaces.md`, "Shared placeholder allocations".

/// `HELIOS_WDDM_ALLOC_KIND_STANDARD` (`protocol/src/wddm.rs`); pinned by the KMD build.
pub const KIND_STANDARD: u32 = 2;

/// `DXGK_CREATEALLOCATIONFLAGS` bit 1, the user-mode `D3DKMT_CREATEALLOCATIONFLAGS::CreateShared`
/// position. DIAGNOSTIC ONLY (it splits `ShPhShared` from `ShPhUnsh`): v323 hardware shows the
/// KMD flags word reads 1 for the NVK shared placeholders, so this bit is never the gate.
pub const CREATE_FLAG_SHARED: u32 = 0x0000_0002;

/// The creator's own declaration of sharing in the meta `misc_flags`: the D3D10 DDI
/// `RESOURCE_MISC_SHARED` (0x2) and `SHARED_KEYEDMUTEX` (0x100) bits the UMDs copy from the
/// create call. Untrusted, diagnostic only.
pub const CREATOR_MISC_SHARED: u32 = 0x0000_0002 | 0x0000_0100;

/// The per-allocation private data of an id-less allocation: `HeliosWddmAllocPrivate` (48) +
/// `HeliosWddmAllocMeta` (48). 128 / 144 bytes carry a layout trailer, which is an identity.
pub const PLACEHOLDER_PRIVATE_BYTES: u32 = 96;

/// The largest placeholder VidMm is asked to account (4 GiB: a 16384 x 16384 fp16 image is
/// 2 GiB). The size is creator-supplied and nothing host-side bounds it for a host-less
/// allocation, so a larger one is refused softly.
pub const MAX_PLACEHOLDER_BYTES: u64 = 4 << 30;

/// Bits of [`Input::identity`]: every fact that makes an allocation more than a bare
/// placeholder. The KMD computes them from the private data; any one set means "not a
/// placeholder".
pub mod identity {
    /// `ap.blob_id != 0`: names a host object.
    pub const BLOB_ID: u32 = 1 << 0;
    /// `blob_mem == HELIOS_BLOB_MEM_RM_EXPORT`: declares a foreign adoption.
    pub const RM_EXPORT: u32 = 1 << 1;
    /// `blob_flags` carries the global VidMm tracker shape.
    pub const TRACKER: u32 = 1 << 2;
    /// A valid `HeliosWddmAllocLayout` trailer is present.
    pub const LAYOUT_TRAILER: u32 = 1 << 3;
    /// `MISC_PRIMARY`.
    pub const PRIMARY: u32 = 1 << 4;
    /// `MISC_OPTIMAL_GDI_TEXTURE`.
    pub const GDI_TEXTURE: u32 = 1 << 5;
    /// `MISC_DIRECT_SCANOUT`.
    pub const DIRECT_SCANOUT: u32 = 1 << 6;
    /// A nonzero `D3DKMDT_STANDARDALLOCATION_TYPE` or GDI surface type in `misc_flags`: the
    /// allocation was described by `GetStandardAllocationDriverData`, i.e. KMD-originated.
    pub const STANDARD_TYPE: u32 = 1 << 7;
    /// Every defined bit.
    pub const ALL: u32 = (1 << 8) - 1;
}

/// `DXGK_CREATEALLOCATIONFLAGS` bit 0, `Resource` (read by `dxgkddi_create_allocation`).
pub const CREATE_FLAG_RESOURCE: u32 = 0x0000_0001;

/// Protocol words the identity mapping reads; each is pinned to the protocol's constant by a
/// `const` assertion in `kmd_render/src/ddi/shared_placeholder.rs`.
pub mod words {
    /// `HELIOS_BLOB_MEM_RM_EXPORT`.
    pub const BLOB_MEM_RM_EXPORT: u32 = 0x8000_0001;
    /// `HELIOS_WDDM_BLOB_FLAG_GLOBAL_VIDMM_TRACKER`.
    pub const BLOB_FLAG_GLOBAL_VIDMM_TRACKER: u32 = 0x2000_0000;
    /// `HELIOS_WDDM_ALLOC_MISC_PRIMARY`.
    pub const MISC_PRIMARY: u32 = 0x8000_0000;
    /// `HELIOS_WDDM_ALLOC_MISC_DIRECT_SCANOUT`.
    pub const MISC_DIRECT_SCANOUT: u32 = 0x4000_0000;
    /// `HELIOS_WDDM_ALLOC_MISC_OPTIMAL_GDI_TEXTURE`.
    pub const MISC_OPTIMAL_GDI_TEXTURE: u32 = 0x2000_0000;
    /// `HELIOS_WDDM_ALLOC_MISC_STANDARD_TYPE_MASK`.
    pub const MISC_STANDARD_TYPE_MASK: u32 = 0x0F00_0000;
    /// `HELIOS_WDDM_ALLOC_MISC_GDI_TYPE_MASK`.
    pub const MISC_GDI_TYPE_MASK: u32 = 0x00F0_0000;
}

/// The private-data fields the identity mapping reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PrivateFacts {
    /// `HeliosWddmAllocPrivate::blob_id`.
    pub blob_id: u64,
    /// `HeliosWddmAllocPrivate::blob_mem`.
    pub blob_mem: u32,
    /// `HeliosWddmAllocPrivate::blob_flags`.
    pub blob_flags: u32,
    /// `HeliosWddmAllocMeta::misc_flags` (0 when the meta is absent).
    pub misc_flags: u32,
    /// A valid `HeliosWddmAllocLayout` trailer is present.
    pub layout_trailer: bool,
}

/// The [`identity`] bits of one allocation's private data.
pub const fn identity_bits(f: &PrivateFacts) -> u32 {
    let mut bits = 0;
    if f.blob_id != 0 {
        bits |= identity::BLOB_ID;
    }
    if f.blob_mem == words::BLOB_MEM_RM_EXPORT {
        bits |= identity::RM_EXPORT;
    }
    if f.blob_flags & words::BLOB_FLAG_GLOBAL_VIDMM_TRACKER != 0 {
        bits |= identity::TRACKER;
    }
    if f.layout_trailer {
        bits |= identity::LAYOUT_TRAILER;
    }
    if f.misc_flags & words::MISC_PRIMARY != 0 {
        bits |= identity::PRIMARY;
    }
    if f.misc_flags & words::MISC_OPTIMAL_GDI_TEXTURE != 0 {
        bits |= identity::GDI_TEXTURE;
    }
    if f.misc_flags & words::MISC_DIRECT_SCANOUT != 0 {
        bits |= identity::DIRECT_SCANOUT;
    }
    if f.misc_flags & (words::MISC_STANDARD_TYPE_MASK | words::MISC_GDI_TYPE_MASK) != 0 {
        bits |= identity::STANDARD_TYPE;
    }
    bits
}

/// Everything the decision reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Input {
    /// `HeliosWddmAllocPrivate::kind`.
    pub kind: u32,
    /// `DXGKARG_CREATEALLOCATION::Flags` as the raw word. NOT read by [`decide`]; carried so
    /// the counters can say what arrived.
    pub create_flags: u32,
    /// `HeliosWddmAllocPrivate::adopt_resource_id`.
    pub adopt_resource_id: u32,
    /// `HeliosWddmAllocPrivate::ctx_id` as the creator wrote it (before the KMD fills in its
    /// own context for an ordinary standard allocation).
    pub ctx_id: u32,
    /// The per-allocation private data size.
    pub private_size: u32,
    /// `HeliosWddmAllocPrivate::size`.
    pub size: u64,
    /// [`identity`] bits.
    pub identity: u32,
}

/// Why an allocation is NOT a placeholder (it takes the ordinary path, validated as before).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Existing {
    NotStandard,
    AdoptId,
    Context,
    PrivateSize,
    Identity,
}

/// Why a placeholder was refused. The status is the soft one: `STATUS_NO_MEMORY`, which the
/// runtime reports as `E_OUTOFMEMORY` for that one resource instead of a removed device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// `size` above [`MAX_PLACEHOLDER_BYTES`].
    TooLarge,
}

/// The decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Not a placeholder: the ordinary path, unchanged.
    Existing(Existing),
    /// An identity-less STANDARD allocation: host-less backing.
    Placeholder,
    /// A placeholder the KMD will not create.
    Refuse(Refusal),
}

impl Existing {
    /// The counter / trace code (1..; 2 was the retired shared-flag row and is not reused).
    pub const fn code(self) -> u32 {
        match self {
            Existing::NotStandard => 1,
            Existing::AdoptId => 3,
            Existing::Context => 4,
            Existing::PrivateSize => 5,
            Existing::Identity => 6,
        }
    }
}

impl Refusal {
    /// The counter / trace code (0x10..), disjoint from [`Existing::code`].
    pub const fn code(self) -> u32 {
        match self {
            Refusal::TooLarge => 0x10,
        }
    }
}

/// Whether the creation flags word has the user-mode `CreateShared` bit. Diagnostic only.
pub const fn is_shared(create_flags: u32) -> bool {
    create_flags & CREATE_FLAG_SHARED != 0
}

/// Whether the creator's meta `misc_flags` declare a shared resource. Diagnostic only.
pub const fn creator_declares_shared(misc_flags: u32) -> bool {
    misc_flags & CREATOR_MISC_SHARED != 0
}

/// Decide. Pure; the checks run in a fixed order so the reason is deterministic.
pub const fn decide(i: &Input) -> Verdict {
    if i.kind != KIND_STANDARD {
        return Verdict::Existing(Existing::NotStandard);
    }
    if i.adopt_resource_id != 0 {
        return Verdict::Existing(Existing::AdoptId);
    }
    if i.ctx_id != 0 {
        return Verdict::Existing(Existing::Context);
    }
    if i.private_size != PLACEHOLDER_PRIVATE_BYTES {
        return Verdict::Existing(Existing::PrivateSize);
    }
    if i.identity != 0 {
        return Verdict::Existing(Existing::Identity);
    }
    if i.size > MAX_PLACEHOLDER_BYTES {
        return Verdict::Refuse(Refusal::TooLarge);
    }
    Verdict::Placeholder
}

/// Whether an allocation of this shape is a placeholder, for the open-side counter.
pub const fn has_placeholder_shape(i: &Input) -> bool {
    matches!(decide(i), Verdict::Placeholder)
}

/// A STANDARD allocation with no identity at all, whatever its private size and creation
/// flags: the shape a placeholder has (a superset of the placeholders: a wrong private size
/// still counts), used to make a wrong size assumption visible.
pub const fn is_identityless_standard(i: &Input) -> bool {
    i.kind == KIND_STANDARD && i.adopt_resource_id == 0 && i.ctx_id == 0 && i.identity == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Input = Input {
        kind: KIND_STANDARD,
        create_flags: 0x3,
        adopt_resource_id: 0,
        ctx_id: 0,
        private_size: 96,
        size: 8 << 20,
        identity: 0,
    };

    #[test]
    fn the_nvk_shared_placeholder_is_one() {
        // CARFlg 3 = Resource | CreateShared, STANDARD, adopt 0, ctx 0, 96 bytes.
        assert_eq!(decide(&PLAIN), Verdict::Placeholder);
        assert!(has_placeholder_shape(&PLAIN));
    }

    /// kind x shared x adopt x ctx x private size, every combination that is not the one
    /// placeholder row is "existing" with the first failing reason.
    #[test]
    fn the_full_table() {
        for kind in [0u32, 1, 2, 3, 4] {
            for flags in [0u32, 1, 2, 3, 0xFFFF_FFFF] {
                for adopt in [0u32, 7] {
                    for ctx in [0u32, 5] {
                        for size in [0u32, 48, 72, 96, 128, 144] {
                            let i = Input {
                                kind,
                                create_flags: flags,
                                adopt_resource_id: adopt,
                                ctx_id: ctx,
                                private_size: size,
                                ..PLAIN
                            };
                            let want = if kind != 2 {
                                Verdict::Existing(Existing::NotStandard)
                            } else if adopt != 0 {
                                Verdict::Existing(Existing::AdoptId)
                            } else if ctx != 0 {
                                Verdict::Existing(Existing::Context)
                            } else if size != 96 {
                                Verdict::Existing(Existing::PrivateSize)
                            } else {
                                Verdict::Placeholder
                            };
                            assert_eq!(decide(&i), want, "{i:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn any_identity_bit_means_not_a_placeholder() {
        for bit in 0..8 {
            let i = Input {
                identity: 1 << bit,
                ..PLAIN
            };
            assert_eq!(decide(&i), Verdict::Existing(Existing::Identity), "bit {bit}");
            assert!(!has_placeholder_shape(&i));
        }
        assert_eq!(identity::ALL, 0xFF);
        let all = Input {
            identity: identity::ALL,
            ..PLAIN
        };
        assert_eq!(decide(&all), Verdict::Existing(Existing::Identity));
    }

    #[test]
    fn identity_carrying_allocations_never_become_placeholders() {
        // An adopted foreign allocation (DEVICE_MEMORY, adopt id, ctx, 128 bytes) is validated
        // exactly as before whatever the shared flag says.
        let foreign = Input {
            kind: 1,
            adopt_resource_id: 42,
            ctx_id: 3,
            private_size: 128,
            identity: identity::RM_EXPORT | identity::LAYOUT_TRAILER,
            ..PLAIN
        };
        assert_eq!(decide(&foreign), Verdict::Existing(Existing::NotStandard));
        // A STANDARD one that tries the same with a placeholder-looking size is refused as
        // carrying an adopt id first.
        let forged = Input {
            adopt_resource_id: 42,
            identity: identity::RM_EXPORT,
            ..PLAIN
        };
        assert_eq!(decide(&forged), Verdict::Existing(Existing::AdoptId));
        // The KMD-originated standard allocations (shared primary, shadow, staging, GDI
        // surface) carry the KMD's own context and standard-type bits.
        let kmd = Input {
            ctx_id: 1,
            identity: identity::STANDARD_TYPE,
            ..PLAIN
        };
        assert_eq!(decide(&kmd), Verdict::Existing(Existing::Context));
        let kmd_no_ctx = Input {
            identity: identity::STANDARD_TYPE,
            ..PLAIN
        };
        assert_eq!(decide(&kmd_no_ctx), Verdict::Existing(Existing::Identity));
    }

    #[test]
    fn size_is_bounded_softly() {
        let at = Input {
            size: MAX_PLACEHOLDER_BYTES,
            ..PLAIN
        };
        assert_eq!(decide(&at), Verdict::Placeholder);
        let over = Input {
            size: MAX_PLACEHOLDER_BYTES + 1,
            ..PLAIN
        };
        assert_eq!(decide(&over), Verdict::Refuse(Refusal::TooLarge));
        let huge = Input {
            size: u64::MAX,
            ..PLAIN
        };
        assert_eq!(decide(&huge), Verdict::Refuse(Refusal::TooLarge));
        // A zero size is the ordinary path's one-page default, not a refusal.
        let zero = Input { size: 0, ..PLAIN };
        assert_eq!(decide(&zero), Verdict::Placeholder);
        // The refusal of a size is only reached by a would-be placeholder: an unshared or
        // identity-carrying allocation keeps the ordinary path whatever its size.
        // The flags word is not part of the decision: the size cap holds for flags 1 as well.
        let unshared_huge = Input {
            create_flags: 1,
            size: u64::MAX,
            ..PLAIN
        };
        assert_eq!(decide(&unshared_huge), Verdict::Refuse(Refusal::TooLarge));
    }

    /// Real `DXGKARG_CREATEALLOCATION::Flags` words. `Resource | CreateShared` reads 3 in the
    /// `CARFlg` breadcrumb of a shared texture (the documented value to confirm on the VM; the
    /// user-mode `D3DKMT_CREATEALLOCATIONFLAGS` has `CreateShared` at 0x2 too).
    #[test]
    fn the_shared_flag_is_extracted_from_real_flag_words() {
        assert_eq!(CREATE_FLAG_RESOURCE, 1);
        assert_eq!(CREATE_FLAG_SHARED, 2);
        let cases: [(u32, bool); 9] = [
            (0x0, false),
            (0x1, false),
            (0x2, true),
            (0x3, true),
            (0x5, false),
            (0x7, true),
            (0x4, false),
            (0xFFFF_FFFD, false),
            (0xFFFF_FFFF, true),
        ];
        for (word, shared) in cases {
            assert_eq!(is_shared(word), shared, "{word:#x}");
        }
    }

    #[test]
    fn identity_words_match_the_protocol_values_documented_here() {
        // The render crate pins each of these to the protocol constant at build time.
        assert_eq!(words::BLOB_MEM_RM_EXPORT, 0x8000_0001);
        assert_eq!(words::BLOB_FLAG_GLOBAL_VIDMM_TRACKER, 0x2000_0000);
        assert_eq!(words::MISC_PRIMARY, 0x8000_0000);
        assert_eq!(words::MISC_DIRECT_SCANOUT, 0x4000_0000);
        assert_eq!(words::MISC_OPTIMAL_GDI_TEXTURE, 0x2000_0000);
        assert_eq!(words::MISC_STANDARD_TYPE_MASK, 0x0F00_0000);
        assert_eq!(words::MISC_GDI_TYPE_MASK, 0x00F0_0000);
    }

    #[test]
    fn a_bare_placeholder_has_no_identity_bits() {
        // What the UMD sends: HOST3D (2) blob_mem, MAPPABLE (1) flags, misc = the D3D10 DDI
        // flags (SHARED 0x2, KEYEDMUTEX 0x100), no trailer.
        for misc in [0u32, 0x2, 0x100, 0x102, 0x0000_0008, 0x1000_0000] {
            let f = PrivateFacts {
                blob_id: 0,
                blob_mem: 2,
                blob_flags: 1,
                misc_flags: misc,
                layout_trailer: false,
            };
            assert_eq!(identity_bits(&f), 0, "misc {misc:#x}");
        }
    }

    #[test]
    fn every_identity_bit_is_produced_by_its_own_fact_and_only_it() {
        let base = PrivateFacts {
            blob_mem: 2,
            blob_flags: 1,
            ..Default::default()
        };
        let tracker_flags = 1 | words::BLOB_FLAG_GLOBAL_VIDMM_TRACKER;
        let singles: [(PrivateFacts, u32); 12] = [
            (PrivateFacts { blob_id: 7, ..base }, identity::BLOB_ID),
            (PrivateFacts { blob_id: u64::MAX, ..base }, identity::BLOB_ID),
            (PrivateFacts { blob_mem: words::BLOB_MEM_RM_EXPORT, ..base }, identity::RM_EXPORT),
            (PrivateFacts { blob_flags: tracker_flags, ..base }, identity::TRACKER),
            (PrivateFacts { layout_trailer: true, ..base }, identity::LAYOUT_TRAILER),
            (PrivateFacts { misc_flags: words::MISC_PRIMARY, ..base }, identity::PRIMARY),
            (
                PrivateFacts { misc_flags: words::MISC_OPTIMAL_GDI_TEXTURE, ..base },
                identity::GDI_TEXTURE,
            ),
            (
                PrivateFacts { misc_flags: words::MISC_DIRECT_SCANOUT, ..base },
                identity::DIRECT_SCANOUT,
            ),
            (PrivateFacts { misc_flags: 0x0100_0000, ..base }, identity::STANDARD_TYPE),
            (PrivateFacts { misc_flags: 0x0800_0000, ..base }, identity::STANDARD_TYPE),
            (PrivateFacts { misc_flags: 0x0010_0000, ..base }, identity::STANDARD_TYPE),
            (PrivateFacts { misc_flags: 0x0080_0000, ..base }, identity::STANDARD_TYPE),
        ];
        for (f, want) in singles {
            assert_eq!(identity_bits(&f), want, "{f:?}");
        }
    }

    /// Every one of the 2^8 combinations of facts maps to exactly that combination of bits.
    #[test]
    fn every_identity_combination() {
        for mask in 0u32..(1 << 8) {
            let pick = |bit: u32, word: u32| if mask & bit != 0 { word } else { 0 };
            let f = PrivateFacts {
                blob_id: if mask & identity::BLOB_ID != 0 { 0x1234 } else { 0 },
                blob_mem: if mask & identity::RM_EXPORT != 0 {
                    words::BLOB_MEM_RM_EXPORT
                } else {
                    2
                },
                blob_flags: 1 | pick(identity::TRACKER, words::BLOB_FLAG_GLOBAL_VIDMM_TRACKER),
                layout_trailer: mask & identity::LAYOUT_TRAILER != 0,
                misc_flags: pick(identity::PRIMARY, words::MISC_PRIMARY)
                    | pick(identity::GDI_TEXTURE, words::MISC_OPTIMAL_GDI_TEXTURE)
                    | pick(identity::DIRECT_SCANOUT, words::MISC_DIRECT_SCANOUT)
                    | pick(identity::STANDARD_TYPE, 0x0200_0000),
            };
            assert_eq!(identity_bits(&f), mask, "{f:?}");
            // And through the decision: any bit means "not a placeholder".
            let i = Input { identity: identity_bits(&f), ..PLAIN };
            assert_eq!(
                decide(&i),
                if mask == 0 {
                    Verdict::Placeholder
                } else {
                    Verdict::Existing(Existing::Identity)
                }
            );
        }
    }

    #[test]
    fn the_identityless_shape_ignores_flags_and_size() {
        for flags in [0u32, 1, 2, 3] {
            for size in [0u32, 96, 128] {
                let i = Input { create_flags: flags, private_size: size, ..PLAIN };
                assert!(is_identityless_standard(&i));
            }
        }
        assert!(!is_identityless_standard(&Input { adopt_resource_id: 1, ..PLAIN }));
        assert!(!is_identityless_standard(&Input { ctx_id: 1, ..PLAIN }));
        assert!(!is_identityless_standard(&Input { kind: 1, ..PLAIN }));
        assert!(!is_identityless_standard(&Input { identity: identity::BLOB_ID, ..PLAIN }));
    }

    #[test]
    fn the_creation_flags_word_is_not_part_of_the_decision() {
        // v323 hardware: the NVK placeholder arrives with flags 1 (Resource only).
        for flags in [0u32, 1, 2, 3, 0xFFFF_FFFF, 0xFFFF_FFFC] {
            let i = Input {
                create_flags: flags,
                ..PLAIN
            };
            assert_eq!(decide(&i), Verdict::Placeholder, "{flags:#x}");
            assert!(has_placeholder_shape(&i));
        }
        assert!(!has_placeholder_shape(&Input {
            ctx_id: 1,
            ..PLAIN
        }));
    }

    /// Every allocation `GetStandardAllocationDriverData` can describe (enum values 1..4:
    /// shared primary, shadow, staging, GDI surface, GDI types 1..) carries the standard-type
    /// bits, with the KMD's Venus context or, when Venus is down, without it; neither is a
    /// placeholder.
    #[test]
    fn a_kmd_originated_standard_allocation_is_never_a_placeholder() {
        for ty in 1u32..=4 {
            for gdi in [0u32, 1, 2, 3] {
                for venus_ctx in [0u32, 1, 9] {
                    for primary in [false, true] {
                        let misc = (ty << 24)
                            | (gdi << 20)
                            | if primary { words::MISC_PRIMARY } else { 0 };
                        let id = identity_bits(&PrivateFacts {
                            blob_mem: 2,
                            blob_flags: 1,
                            misc_flags: misc,
                            ..Default::default()
                        });
                        assert_ne!(id & identity::STANDARD_TYPE, 0);
                        let i = Input {
                            create_flags: 1,
                            ctx_id: venus_ctx,
                            identity: id,
                            ..PLAIN
                        };
                        let v = decide(&i);
                        assert!(
                            matches!(
                                v,
                                Verdict::Existing(Existing::Context)
                                    | Verdict::Existing(Existing::Identity)
                            ),
                            "{i:?} {v:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_creators_shared_declaration_is_diagnostic_only() {
        assert!(!creator_declares_shared(0));
        assert!(creator_declares_shared(0x2));
        assert!(creator_declares_shared(0x100));
        assert!(creator_declares_shared(0x102));
        assert!(!creator_declares_shared(0x1000_0000));
        // The misc bits that mark sharing never reach the identity mapping.
        let f = PrivateFacts {
            blob_mem: 2,
            blob_flags: 1,
            misc_flags: 0x102,
            ..Default::default()
        };
        assert_eq!(identity_bits(&f), 0);
    }

    #[test]
    fn codes_are_distinct_and_nonzero() {
        let codes = [
            Existing::NotStandard.code(),
            Existing::AdoptId.code(),
            Existing::Context.code(),
            Existing::PrivateSize.code(),
            Existing::Identity.code(),
            Refusal::TooLarge.code(),
        ];
        for (a, x) in codes.iter().enumerate() {
            assert_ne!(*x, 0);
            for y in &codes[a + 1..] {
                assert_ne!(x, y);
            }
        }
    }
}

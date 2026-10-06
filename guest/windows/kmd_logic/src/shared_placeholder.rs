//! Shared placeholder allocations: the pure decision. An NVK process that cannot mint a KMD
//! resource id for a SHARED texture (A8, R10G10B10A2, fp16, R8G8, NV12 without the
//! shared-format cap, BGRA8 with `NVK_HELIOS_RESID=0`) still has to hand the D3D runtime a
//! WDDM allocation, so it creates a PLACEHOLDER: a `STANDARD` allocation with no identity
//! (adopt id 0, context 0, the 96-byte private data, the shared creation flag set). The real
//! content is exchanged by NVK through other means; the placeholder is never flipped, scanned
//! out or copied.
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
//! This module only DECIDES. The rule is deliberately narrow: an allocation that carries ANY
//! identity (an adopt id, a context, a blob id, a declared RM-export memory type, a tracker
//! flag, a layout trailer, a primary / GDI / scan-out / standard-type bit) or is not shared is
//! never a placeholder and takes the ordinary path with exactly today's validation.
//! Design and the table: `docs/shared-foreign-surfaces.md`, "Shared placeholder allocations".

/// `HELIOS_WDDM_ALLOC_KIND_STANDARD` (`protocol/src/wddm.rs`); pinned by the KMD build.
pub const KIND_STANDARD: u32 = 2;

/// `DXGK_CREATEALLOCATIONFLAGS` bit 1, `CreateShared` (bit 0 is `Resource`, which
/// `dxgkddi_create_allocation` already reads). The first `CARFlg` value of a shared texture
/// reads 3.
pub const CREATE_FLAG_SHARED: u32 = 0x0000_0002;

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

/// Everything the decision reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Input {
    /// `HeliosWddmAllocPrivate::kind`.
    pub kind: u32,
    /// `DXGKARG_CREATEALLOCATION::Flags` as the raw word.
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
    NotShared,
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
    /// A shared, identity-less STANDARD allocation: host-less backing.
    Placeholder,
    /// A placeholder the KMD will not create.
    Refuse(Refusal),
}

impl Existing {
    /// The counter / trace code (1..).
    pub const fn code(self) -> u32 {
        match self {
            Existing::NotStandard => 1,
            Existing::NotShared => 2,
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

/// Whether the creation flags say the resource is shared.
pub const fn is_shared(create_flags: u32) -> bool {
    create_flags & CREATE_FLAG_SHARED != 0
}

/// Decide. Pure; the checks run in a fixed order so the reason is deterministic.
pub const fn decide(i: &Input) -> Verdict {
    if i.kind != KIND_STANDARD {
        return Verdict::Existing(Existing::NotStandard);
    }
    if !is_shared(i.create_flags) {
        return Verdict::Existing(Existing::NotShared);
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

/// Whether an allocation of this shape is a placeholder, for the open-side counter (the open
/// does not see the creation flags, so the shared test is taken as met).
pub const fn has_placeholder_shape(i: &Input) -> bool {
    matches!(
        decide(&Input {
            create_flags: i.create_flags | CREATE_FLAG_SHARED,
            ..*i
        }),
        Verdict::Placeholder
    )
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
                            } else if flags & 2 == 0 {
                                Verdict::Existing(Existing::NotShared)
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
        let unshared_huge = Input {
            create_flags: 1,
            size: u64::MAX,
            ..PLAIN
        };
        assert_eq!(decide(&unshared_huge), Verdict::Existing(Existing::NotShared));
    }

    #[test]
    fn the_shared_bit_is_bit_one_and_only_bit_one() {
        assert!(!is_shared(0));
        assert!(!is_shared(1));
        assert!(is_shared(2));
        assert!(is_shared(3));
        assert!(!is_shared(!2));
    }

    #[test]
    fn open_side_shape_ignores_the_creation_flags() {
        let unshared = Input {
            create_flags: 0,
            ..PLAIN
        };
        assert_eq!(decide(&unshared), Verdict::Existing(Existing::NotShared));
        assert!(has_placeholder_shape(&unshared));
        assert!(!has_placeholder_shape(&Input {
            ctx_id: 1,
            ..PLAIN
        }));
    }

    #[test]
    fn codes_are_distinct_and_nonzero() {
        let codes = [
            Existing::NotStandard.code(),
            Existing::NotShared.code(),
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

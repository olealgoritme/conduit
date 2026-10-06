//! Transport-generation identities that stay unique across a driver IMAGE RELOAD
//! (docs/zero-copy-present.md section 25, "Wrong buffer on scanout after a device restart").
//!
//! Two identities were minted from process-lifetime statics that start at a constant:
//!
//! * the transport serial every `AllocationContext` is stamped with (`TRANSPORT_SERIAL`, first
//!   StartDevice -> 1), which `is_current_generation` compares to refuse an allocation of an older
//!   generation (resource ids restart at 1 in each generation);
//! * the NVRM `epoch` (`wire_fence_base`, first transport -> 1) every escape reply carries, which
//!   librmclient compares to the one QUERY_CAPS gave at init (`helios_nvrm_reply_is_lost`): a
//!   change latches "device lost" and the process reopens once instead of looping.
//!
//! `pnputil /restart-device` RELOADS the image (hardware: `StartN` 1 after each restart), so every
//! static is zero again and BOTH identities repeat: the first generation of the new image is
//! "serial 1, epoch 1", exactly what the previous image's first generation was. An allocation (or a
//! long-lived NVK client such as DWM, the shell, a game) that survived the restart then compared
//! equal to the new generation: no loss was seen, and the stale client went on with handle numbers,
//! GEM numbers and resource ids of the old generation against a new generation that reuses them.
//!
//! The fix is a per-image SALT, taken once from the monotonic clock the first time an identity is
//! needed, and mixed into both. Two images loaded at least [`SALT_QUANTUM_100NS`] apart (a driver
//! unload and load takes far longer) get different salts, and the salt only grows within a boot.
//! Argument-only: the clock read and the static that holds the salt live in `kmd_render`.

/// One salt step in 100 ns units (2^20, about 0.105 s). An unload and a load of the image cannot
/// take less, so two images of one boot never share a salt.
pub const SALT_QUANTUM_100NS: u64 = 1 << 20;

/// Low bits of every identity that count generations within one image (16.7 million starts).
pub const COUNT_BITS: u32 = 24;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;

/// The image salt for a monotonic clock reading (100 ns since boot). Never 0.
pub const fn image_salt(now_100ns: u64) -> u64 {
    let s = now_100ns >> 20;
    if s == 0 {
        1
    } else {
        s
    }
}

/// The transport serial of the `n`-th StartDevice of an image (`n` counts from 1). Never 0 (the
/// salt is at least 1), strictly increasing in `n`, and different from every serial of an image
/// with another salt as long as `n` stays below 2^24.
pub const fn transport_serial(salt: u64, n: u64) -> u64 {
    (salt << COUNT_BITS) | (n & COUNT_MASK)
}

/// The NVRM epoch of the transport whose wire-fence base is `wire_fence_base` (the base advances
/// by `stride` per generation and starts at 1, so `(base - 1) / stride` is the generation index
/// within the image). Never 0 (0 is the "no transport" reading): the salt is at least 1.
pub const fn nvrm_epoch(salt: u64, wire_fence_base: u64, stride: u64) -> u64 {
    let index = if stride == 0 {
        0
    } else {
        wire_fence_base.saturating_sub(1) / stride
    };
    (salt << COUNT_BITS) | (index & COUNT_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRIDE: u64 = 1 << 32;

    #[test]
    fn salt_is_never_zero() {
        assert_eq!(image_salt(0), 1);
        assert_eq!(image_salt(SALT_QUANTUM_100NS - 1), 1);
        assert_eq!(image_salt(SALT_QUANTUM_100NS), 1);
        assert_eq!(image_salt(2 * SALT_QUANTUM_100NS), 2);
    }

    #[test]
    fn salt_never_goes_back_with_the_clock() {
        let mut last = 0;
        for t in (0..200).map(|i| i * 3_000_000u64) {
            let s = image_salt(t);
            assert!(s >= last);
            last = s;
        }
    }

    #[test]
    fn first_generation_of_two_images_differs() {
        // The incident: after an image reload both images minted serial 1 and epoch 1.
        let old = image_salt(600 * 10_000_000); // loaded 10 minutes after boot
        let new = image_salt(780 * 10_000_000); // reloaded three minutes later
        assert_ne!(old, new);
        assert_ne!(transport_serial(old, 1), transport_serial(new, 1));
        assert_ne!(nvrm_epoch(old, 1, STRIDE), nvrm_epoch(new, 1, STRIDE));
        // Not the bare constants the old code produced.
        assert_ne!(transport_serial(new, 1), 1);
        assert_ne!(nvrm_epoch(new, 1, STRIDE), 1);
    }

    #[test]
    fn generations_within_an_image_stay_distinct_and_ordered() {
        let salt = image_salt(5_000_000_000);
        let mut last_serial = 0;
        let mut last_epoch = 0;
        for k in 0..64u64 {
            let serial = transport_serial(salt, k + 1);
            let epoch = nvrm_epoch(salt, 1 + k * STRIDE, STRIDE);
            assert!(serial > last_serial);
            assert!(epoch > last_epoch);
            last_serial = serial;
            last_epoch = epoch;
        }
    }

    #[test]
    fn never_zero() {
        assert_ne!(transport_serial(1, 1), 0);
        assert_ne!(nvrm_epoch(1, 1, STRIDE), 0);
        // A zero stride cannot divide; it reads as generation 0 and is still nonzero.
        assert_ne!(nvrm_epoch(1, 1, 0), 0);
    }

    #[test]
    fn a_stale_serial_is_refused_by_the_current_generation_check() {
        // `paging::alloc_is_current` is the consumer: serial 1 of the old image against serial 1 of
        // the new one used to be "current".
        let old_img = transport_serial(image_salt(600 * 10_000_000), 1);
        let new_img = transport_serial(image_salt(780 * 10_000_000), 1);
        assert!(!crate::paging::alloc_is_current(old_img, Some(new_img)));
        assert!(crate::paging::alloc_is_current(new_img, Some(new_img)));
        assert!(!crate::paging::alloc_is_current(1, Some(new_img)));
    }
}

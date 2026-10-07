//! Which Vulkan queue the KMD's windowed-Present copies run on (`CopyQueue`).
//!
//! The KMD's Venus device has always had ONE queue, family 0, queue 0, bound to ring 1. On the
//! NVIDIA host that family is the graphics engine (graphics + compute + transfer), so the
//! windowed Present copy (5.76 MB at 1600x900) waits for graphics timeslices next to the game's
//! own NVK channel: measured on the host, the copy takes 208 us alone and p50 2.3 ms against a
//! heavy competing graphics load, about 130 us more per frame under Heaven. The same copy on the
//! transfer-only family (the copy engine) runs beside the load: p50 306 us.
//!
//! With `CopyQueue` 1 the device gets a SECOND queue on a transfer-only family (chosen from
//! `vkGetPhysicalDeviceQueueFamilyProperties`, never a hardcoded index), bound to its own ring
//! ([`COPY_RING_IDX`]), and the Present copies whose commands a transfer queue can run go there.
//! Everything here is pure; the Venus half is `kmd_render/src/virtio/venus`, the knob and the
//! counters are `kmd_render/src/ddi/copy_queue.rs`. `docs/zero-copy-present.md` section 24.13.
//!
//! Queue-family ownership (the rule [`barrier_family`] encodes). Every resource a Present copy
//! touches is either external (the source image, a Venus allocation or a foreign NVK resource;
//! the destination Present buffer) or private to the copy (a guest-blob buffer):
//!
//! * External resources stay `VK_SHARING_MODE_EXCLUSIVE` and keep the protocol they already
//!   have: each copy acquires them from `VK_QUEUE_FAMILY_EXTERNAL` and releases them back. Only
//!   the family on the KMD side of the pair changes: the family of the queue that runs the copy.
//!   Their resting owner is EXTERNAL, so a copy on family 0 and a later one on the transfer
//!   family never hand a resource to each other directly. CONCURRENT was not chosen: the image
//!   and buffer create infos must match their creators' (DXVK, NVK, the KMD's own exported
//!   Present buffer) for the import to be valid, and those are exclusive.
//! * A guest-blob buffer (`GuestBlob`) is exclusive and never leaves the KMD device. It is only
//!   ever written by copies recorded for one family (the route of a cache record is fixed when
//!   it is recorded, and every copy into one guest buffer comes from such a record; see
//!   [`conflicts`] for the rare case of two records of different families sharing a resource)
//!   and read by the CPU, so it needs no ownership transfer: the first use acquires it
//!   implicitly and its contents never need to survive a family change (every copy rewrites the
//!   whole extent).
//! * There are no semaphores in these submissions. Completion is the virtio wire fence of the
//!   SUBMIT_3D, created by the host as an empty `vkQueueSubmit` with a fence on the queue bound
//!   to the submission's ring: a copy on the transfer queue is fenced on [`COPY_RING_IDX`], so
//!   its fence orders after the copy. Wire fences are retired per command (the transport's
//!   in-flight table, not a watermark), so every waiter sees the copy's completion unchanged.

/// `VK_QUEUE_GRAPHICS_BIT`.
pub const QUEUE_GRAPHICS: u32 = 0x1;
/// `VK_QUEUE_COMPUTE_BIT`.
pub const QUEUE_COMPUTE: u32 = 0x2;
/// `VK_QUEUE_TRANSFER_BIT`.
pub const QUEUE_TRANSFER: u32 = 0x4;

/// `VK_COMMAND_TYPE_vkGetPhysicalDeviceQueueFamilyProperties_EXT`.
pub const CMD_GET_PHYSICAL_DEVICE_QUEUE_FAMILY_PROPERTIES: u32 = 7;

/// The most queue families the KMD asks for (the 5090 reports 3; a family past this index is
/// not considered).
pub const MAX_FAMILIES: u32 = 8;

/// The ring the main queue (family 0, queue 0) is bound to: every KMD submission until now.
pub const MAIN_RING_IDX: u32 = 1;
/// The ring the transfer queue is bound to. Rings are per Venus context, so this is free in the
/// KMD's own context; virglrenderer refuses a second queue on an already bound ring, and a
/// fence on a ring with no queue destroys the whole context, so nothing is ever fenced on it
/// unless the queue was obtained ([`Device::copy_ready`]).
pub const COPY_RING_IDX: u32 = 2;

/// The family of the main queue.
pub const MAIN_FAMILY: u32 = 0;

/// The `CopyQueue` knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    /// 0 (default): every copy on family 0, the device exactly as before (one queue, no
    /// queue-family query at bring-up).
    Main,
    /// 1: a transfer-only queue when the device has a transfer-only family.
    Transfer,
}

impl Knob {
    /// 1 is the transfer queue; every other value is the default.
    pub const fn from_raw(raw: u32) -> Self {
        if raw == 1 {
            Self::Transfer
        } else {
            Self::Main
        }
    }

    pub const fn raw(self) -> u32 {
        match self {
            Self::Main => 0,
            Self::Transfer => 1,
        }
    }
}

/// One `VkQueueFamilyProperties`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Family {
    pub flags: u32,
    pub count: u32,
    pub timestamp_bits: u32,
    /// `minImageTransferGranularity` (width, height, depth).
    pub granularity: [u32; 3],
}

impl Family {
    /// The six 32-bit words of one reply entry, in wire order.
    pub const fn from_words(w: [u32; 6]) -> Self {
        Self {
            flags: w[0],
            count: w[1],
            timestamp_bits: w[2],
            granularity: [w[3], w[4], w[5]],
        }
    }

    /// Transfer, and neither graphics nor compute: a copy engine.
    pub const fn transfer_only(&self) -> bool {
        self.flags & QUEUE_TRANSFER != 0
            && self.flags & (QUEUE_GRAPHICS | QUEUE_COMPUTE) == 0
            && self.count > 0
    }
}

/// The transfer-only family the copy queue is created on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub index: u32,
    pub granularity: [u32; 3],
}

/// The first transfer-only family other than the main one, or `None` (every copy stays on
/// family 0).
pub fn choose_transfer_family(families: &[Family]) -> Option<Choice> {
    families
        .iter()
        .enumerate()
        .take(MAX_FAMILIES as usize)
        .find(|(i, f)| *i as u32 != MAIN_FAMILY && f.transfer_only())
        .map(|(i, f)| Choice {
            index: i as u32,
            granularity: f.granularity,
        })
}

/// `vkGetPhysicalDeviceQueueFamilyProperties(physicalDevice, &count = max, props[max])`. The
/// partial encoding of `VkQueueFamilyProperties` is empty (every member is an output), so the
/// array is its size alone. The reply is `cmd | ptr | count | array_size | 6 x u32 per family`
/// (no `VkResult`).
pub fn encode_get_queue_family_properties(physical_device: u64, max: u32) -> crate::Writer {
    let mut w = crate::Writer::new();
    w.header(
        CMD_GET_PHYSICAL_DEVICE_QUEUE_FAMILY_PROPERTIES,
        crate::CMD_FLAG_GENERATE_REPLY,
    );
    w.u64(physical_device);
    w.count(true); // pQueueFamilyPropertyCount
    w.u32(max);
    w.u64(u64::from(max)); // pQueueFamilyProperties array_size, no members
    w
}

/// What the device was created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Device {
    /// The transfer family the copy queue is on, when the device has the queue.
    pub family: Option<u32>,
    /// That family's `minImageTransferGranularity`.
    pub granularity: [u32; 3],
}

impl Device {
    pub const MAIN_ONLY: Self = Self {
        family: None,
        granularity: [0, 0, 0],
    };

    /// The transfer queue exists (and its ring is bound): copies may go there.
    pub const fn copy_ready(&self) -> bool {
        self.family.is_some()
    }
}

/// One `vkCreateDevice` attempt: the extension tier of the existing ladder (0 export trio plus
/// modifier, 1 export trio, 2 none) and whether the transfer queue is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    pub tier: u32,
    pub transfer: bool,
}

/// The last tier of the extension ladder.
pub const LAST_TIER: u32 = 2;

/// The first attempt: the ladder's start tier, with the transfer queue when one was chosen.
pub const fn first_attempt(start_tier: u32, transfer: bool) -> Attempt {
    Attempt {
        tier: start_tier,
        transfer,
    }
}

/// The attempt after a refused one. With the transfer queue the whole ladder is walked first;
/// when even its last tier is refused, the ladder starts again from `start_tier` WITHOUT it,
/// which is exactly the sequence of devices a `CopyQueue` 0 boot tries. `None`: bring-up fails,
/// as it did before.
pub const fn next_attempt(refused: Attempt, start_tier: u32) -> Option<Attempt> {
    if refused.tier < LAST_TIER {
        Some(Attempt {
            tier: refused.tier + 1,
            transfer: refused.transfer,
        })
    } else if refused.transfer {
        Some(Attempt {
            tier: start_tier,
            transfer: false,
        })
    } else {
        None
    }
}

/// Whether one image region of a copy on a queue with `granularity` is legal
/// (`minImageTransferGranularity`, Vulkan "Queue Family Properties"): (0,0,0) allows only whole
/// subresources (offset 0, extent equal to the image); otherwise every offset must be a multiple
/// of the granularity and every extent a multiple of it, or reach the image's edge.
pub fn granularity_ok(
    granularity: [u32; 3],
    offset: [u32; 3],
    extent: [u32; 3],
    image: [u32; 3],
) -> bool {
    if granularity == [0, 0, 0] {
        return offset == [0, 0, 0] && extent == image;
    }
    (0..3).all(|i| {
        let g = granularity[i];
        if g == 0 {
            // One zero axis (not a shape real hardware reports): the whole-axis rule on it.
            return offset[i] == 0 && extent[i] == image[i];
        }
        let end = offset[i].checked_add(extent[i]);
        offset[i] % g == 0
            && end.is_some_and(|e| e <= image[i])
            && (extent[i] % g == 0 || end == Some(image[i]))
    })
}

/// The queue a copy runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Family 0, ring 1: every copy before this knob.
    Main,
    /// The transfer-only family, ring [`COPY_RING_IDX`].
    Transfer,
}

impl Route {
    pub const fn ring_idx(self) -> u32 {
        match self {
            Self::Main => MAIN_RING_IDX,
            Self::Transfer => COPY_RING_IDX,
        }
    }
}

/// Why a copy the knob wanted on the transfer queue runs on family 0 (`CqWhy`, bit `code - 1`
/// in `CqMask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The device has no transfer queue: no transfer-only family, or `vkCreateDevice` refused
    /// the second queue (`CqDevFall`), or the queue was not obtained.
    NoQueue,
    /// The destination is an image (a DWM primary or another OPTIMAL image). Those copies stay
    /// on the queue the scan-out copies of the same images use, so their ring-1 order holds.
    ImageDestination,
    /// The copy converts formats with `vkCmdBlitImage`, which needs a graphics queue.
    Blit,
    /// The copy region breaks the transfer family's `minImageTransferGranularity`.
    Granularity,
}

impl Why {
    pub const fn code(self) -> u32 {
        match self {
            Self::NoQueue => 1,
            Self::ImageDestination => 2,
            Self::Blit => 3,
            Self::Granularity => 4,
        }
    }

    pub const fn bit(self) -> u32 {
        1 << (self.code() - 1)
    }

    pub const ALL: [Self; 4] = [
        Self::NoQueue,
        Self::ImageDestination,
        Self::Blit,
        Self::Granularity,
    ];
}

/// What one newly recorded Present copy is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyShape {
    /// The destination is a buffer (a KMD standard Present buffer or its guest blob).
    pub buffer_destination: bool,
    /// The copy needs `vkCmdBlitImage` (a format conversion).
    pub blit: bool,
    /// The source image's extent; the copy reads `(0,0,0)` .. `copy_extent`.
    pub image_extent: [u32; 3],
    pub copy_extent: [u32; 3],
}

/// The route of a newly recorded copy and, when the knob wanted the transfer queue and did not
/// get it, why. Decided once per cache record (the record's command buffer is allocated from a
/// pool of that family and is only ever submitted to that family's queue).
pub fn route(knob: Knob, device: Device, copy: CopyShape) -> (Route, Option<Why>) {
    if knob == Knob::Main {
        return (Route::Main, None);
    }
    let why = if !device.copy_ready() {
        Some(Why::NoQueue)
    } else if !copy.buffer_destination {
        Some(Why::ImageDestination)
    } else if copy.blit {
        Some(Why::Blit)
    } else if !granularity_ok(
        device.granularity,
        [0, 0, 0],
        copy.copy_extent,
        copy.image_extent,
    ) {
        Some(Why::Granularity)
    } else {
        None
    };
    match why {
        None => (Route::Transfer, None),
        Some(why) => (Route::Main, Some(why)),
    }
}

/// The KMD-side queue family of an EXTERNAL acquire/release barrier in a command recorded for
/// `route` (the other side is always `VK_QUEUE_FAMILY_EXTERNAL`). `transfer_family` is the
/// device's copy family; a Transfer route without one cannot be recorded and falls back to
/// family 0 (never reached: [`route`] gives Transfer only with a family).
pub const fn barrier_family(route: Route, transfer_family: Option<u32>) -> u32 {
    match (route, transfer_family) {
        (Route::Transfer, Some(f)) => f,
        _ => MAIN_FAMILY,
    }
}

/// One cached copy record, as [`conflicts`] sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub route: Route,
    pub source: u32,
    pub destination: u32,
    /// The wire fence of its last submission (0: never submitted).
    pub last_fence: u64,
}

/// Whether `other` must have retired before a copy of `next` is submitted.
///
/// Copies on ONE queue run in submission order; copies on two queues do not. Two records that
/// share a resource (the same destination written from sources of different formats, one
/// converted on family 0 and one copied on the transfer queue; or one source read into two
/// destinations of different kinds) could then overlap: two frames written out of order, or an
/// EXTERNAL acquire on one family while the other still owns the resource. So before a copy is
/// submitted, every record of the OTHER route that shares its source or destination and has a
/// submission is waited for (`CqSwitch`). Records on the same route never conflict.
pub fn conflicts(next: Record, other: Record) -> bool {
    other.route != next.route
        && other.last_fence != 0
        && (other.destination == next.destination
            || other.source == next.source
            || other.destination == next.source
            || other.source == next.destination)
}

/// How long a queue-switch wait may take before the copy is submitted anyway (the frame may then
/// show the older copy's content once; `CqSwitchTo`). A copy takes well under a millisecond on
/// the host; this bounds a sick host, not the normal case.
pub const SWITCH_WAIT_MS: u64 = 100;

/// Pack a granularity into one counter word: one byte per axis, saturated at 255.
pub const fn pack_granularity(g: [u32; 3]) -> u32 {
    const fn sat(v: u32) -> u32 {
        if v > 255 {
            255
        } else {
            v
        }
    }
    sat(g[0]) | (sat(g[1]) << 8) | (sat(g[2]) << 16)
}

/// The counter names (at most 14 characters, unique across `kmd_render` and `kmd_logic`), all
/// written by `kmd_render/src/ddi/copy_queue.rs` and nowhere else.
pub const COUNTERS: &[&str] = &[
    // Bring-up: the knob in force, the families seen, the family chosen and its granularity,
    // whether the device got the queue, and a refused two-queue device.
    "CqKnob",
    "CqFamN",
    "CqFam",
    "CqGran",
    "CqReady",
    "CqDevFall",
    // Per copy: submissions on each family, copies the knob wanted on the transfer queue that ran
    // on family 0, the last and every reason, queue-switch waits and those that timed out.
    "CqMain",
    "CqXfer",
    "CqFall",
    "CqWhy",
    "CqMask",
    "CqSwitch",
    "CqSwitchTo",
];

/// `CqFam` when the device has no transfer queue.
pub const NO_FAMILY: u32 = u32::MAX;

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const GFX: Family = Family::from_words([
        QUEUE_GRAPHICS | QUEUE_COMPUTE | QUEUE_TRANSFER,
        16,
        64,
        1,
        1,
        1,
    ]);
    const XFER: Family = Family::from_words([QUEUE_TRANSFER, 2, 64, 1, 1, 1]);
    const COMP: Family = Family::from_words([QUEUE_COMPUTE | QUEUE_TRANSFER, 8, 64, 1, 1, 1]);

    #[test]
    fn the_5090_layout_picks_family_1() {
        // Host measurement: 0 graphics (16 queues), 1 transfer-only (2), 2 compute.
        let c = choose_transfer_family(&[GFX, XFER, COMP]).unwrap();
        assert_eq!(c.index, 1);
        assert_eq!(c.granularity, [1, 1, 1]);
    }

    #[test]
    fn the_family_is_found_by_its_flags_not_its_index() {
        let c = choose_transfer_family(&[GFX, COMP, XFER]).unwrap();
        assert_eq!(c.index, 2);
        // Sparse binding alongside transfer is still a copy engine.
        let sparse = Family::from_words([QUEUE_TRANSFER | 0x8, 1, 0, 4, 4, 1]);
        let c = choose_transfer_family(&[GFX, sparse]).unwrap();
        assert_eq!((c.index, c.granularity), (1, [4, 4, 1]));
    }

    #[test]
    fn no_transfer_only_family_means_none() {
        assert_eq!(choose_transfer_family(&[GFX, COMP]), None);
        assert_eq!(choose_transfer_family(&[GFX]), None);
        assert_eq!(choose_transfer_family(&[]), None);
        // A transfer-only family with no queues is not usable.
        let empty = Family::from_words([QUEUE_TRANSFER, 0, 0, 1, 1, 1]);
        assert_eq!(choose_transfer_family(&[GFX, empty]), None);
        // Family 0 is the main queue's, whatever its flags.
        assert_eq!(choose_transfer_family(&[XFER, GFX]), None);
    }

    #[test]
    fn families_past_the_request_are_ignored() {
        let mut v: Vec<Family> = (0..MAX_FAMILIES).map(|_| GFX).collect();
        v.push(XFER);
        assert_eq!(choose_transfer_family(&v), None);
        v[(MAX_FAMILIES - 1) as usize] = XFER;
        assert_eq!(choose_transfer_family(&v).unwrap().index, MAX_FAMILIES - 1);
    }

    #[test]
    fn query_golden_bytes() {
        let w = encode_get_queue_family_properties(0x1122_3344_5566_7788, 8);
        let b = w.finished().unwrap();
        let mut want: Vec<u8> = Vec::new();
        want.extend_from_slice(&7u32.to_le_bytes()); // command type
        want.extend_from_slice(&1u32.to_le_bytes()); // GENERATE_REPLY
        want.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        want.extend_from_slice(&1u64.to_le_bytes()); // count pointer present
        want.extend_from_slice(&8u32.to_le_bytes()); // *count
        want.extend_from_slice(&8u64.to_le_bytes()); // array size, no members
        assert_eq!(b, &want[..]);
    }

    #[test]
    fn knob_values() {
        assert_eq!(Knob::from_raw(0), Knob::Main);
        assert_eq!(Knob::from_raw(1), Knob::Transfer);
        assert_eq!(Knob::from_raw(2), Knob::Main);
        assert_eq!(Knob::from_raw(u32::MAX), Knob::Main);
        assert_eq!(Knob::Transfer.raw(), 1);
    }

    fn walk(start: u32, transfer: bool) -> Vec<Attempt> {
        let mut out = std::vec![first_attempt(start, transfer)];
        while let Some(n) = next_attempt(*out.last().unwrap(), start) {
            out.push(n);
            assert!(out.len() < 16);
        }
        out
    }

    #[test]
    fn the_ladder_without_the_queue_is_the_old_one() {
        for start in 0..=LAST_TIER {
            let a = walk(start, false);
            let tiers: Vec<u32> = a.iter().map(|a| a.tier).collect();
            let want: Vec<u32> = (start..=LAST_TIER).collect();
            assert_eq!(tiers, want);
            assert!(a.iter().all(|a| !a.transfer));
        }
    }

    #[test]
    fn the_ladder_with_the_queue_falls_back_to_the_old_one() {
        let a = walk(0, true);
        let want = [
            (0, true),
            (1, true),
            (2, true),
            (0, false),
            (1, false),
            (2, false),
        ];
        let got: Vec<(u32, bool)> = a.iter().map(|a| (a.tier, a.transfer)).collect();
        assert_eq!(got, want);
        let a = walk(1, true);
        let got: Vec<(u32, bool)> = a.iter().map(|a| (a.tier, a.transfer)).collect();
        assert_eq!(got, [(1, true), (2, true), (1, false), (2, false)]);
    }

    #[test]
    fn granularity_rules() {
        let img = [1600, 900, 1];
        // Whole-image copies are legal at any granularity.
        for g in [
            [0, 0, 0],
            [1, 1, 1],
            [8, 8, 1],
            [64, 64, 1],
            [2048, 2048, 1],
        ] {
            assert!(granularity_ok(g, [0, 0, 0], img, img), "{g:?}");
        }
        // (0,0,0): only whole subresources.
        assert!(!granularity_ok([0, 0, 0], [0, 0, 0], [800, 900, 1], img));
        assert!(!granularity_ok([0, 0, 0], [8, 0, 0], [1592, 900, 1], img));
        // (8,8,1): multiples, or reaching the edge.
        assert!(granularity_ok([8, 8, 1], [0, 0, 0], [800, 896, 1], img));
        assert!(granularity_ok([8, 8, 1], [8, 8, 0], [1592, 892, 1], img));
        assert!(!granularity_ok([8, 8, 1], [0, 0, 0], [801, 900, 1], img));
        assert!(!granularity_ok([8, 8, 1], [4, 0, 0], [1596, 900, 1], img));
        // Past the image is never legal.
        assert!(!granularity_ok([1, 1, 1], [0, 0, 0], [1601, 900, 1], img));
        assert!(!granularity_ok([1, 1, 1], [u32::MAX, 0, 0], [2, 1, 1], img));
        // A zero axis alone: whole axis.
        assert!(granularity_ok([8, 8, 0], [0, 0, 0], img, img));
        assert!(!granularity_ok([8, 0, 1], [0, 0, 0], [1600, 899, 1], img));
    }

    const READY: Device = Device {
        family: Some(1),
        granularity: [1, 1, 1],
    };

    fn plain() -> CopyShape {
        CopyShape {
            buffer_destination: true,
            blit: false,
            image_extent: [1600, 900, 1],
            copy_extent: [1600, 900, 1],
        }
    }

    #[test]
    fn knob_zero_is_always_main_and_never_a_fallback() {
        for device in [Device::MAIN_ONLY, READY] {
            for buffer_destination in [false, true] {
                for blit in [false, true] {
                    let c = CopyShape {
                        buffer_destination,
                        blit,
                        ..plain()
                    };
                    assert_eq!(route(Knob::Main, device, c), (Route::Main, None));
                }
            }
        }
    }

    #[test]
    fn knob_one_routes_plain_buffer_copies_to_transfer() {
        assert_eq!(
            route(Knob::Transfer, READY, plain()),
            (Route::Transfer, None)
        );
    }

    #[test]
    fn every_fallback_has_its_reason() {
        assert_eq!(
            route(Knob::Transfer, Device::MAIN_ONLY, plain()),
            (Route::Main, Some(Why::NoQueue))
        );
        let image = CopyShape {
            buffer_destination: false,
            ..plain()
        };
        assert_eq!(
            route(Knob::Transfer, READY, image),
            (Route::Main, Some(Why::ImageDestination))
        );
        let blit = CopyShape {
            blit: true,
            ..plain()
        };
        assert_eq!(
            route(Knob::Transfer, READY, blit),
            (Route::Main, Some(Why::Blit))
        );
        let coarse = Device {
            family: Some(1),
            granularity: [0, 0, 0],
        };
        let partial = CopyShape {
            copy_extent: [1599, 900, 1],
            ..plain()
        };
        assert_eq!(
            route(Knob::Transfer, coarse, partial),
            (Route::Main, Some(Why::Granularity))
        );
        assert_eq!(
            route(Knob::Transfer, coarse, plain()),
            (Route::Transfer, None)
        );
    }

    #[test]
    fn transfer_is_only_ever_chosen_with_a_queue_a_buffer_and_no_blit() {
        for family in [None, Some(1), Some(2)] {
            for g in [[0, 0, 0], [1, 1, 1], [8, 8, 1]] {
                for buffer_destination in [false, true] {
                    for blit in [false, true] {
                        for w in [1599, 1600] {
                            let device = Device {
                                family,
                                granularity: g,
                            };
                            let c = CopyShape {
                                buffer_destination,
                                blit,
                                image_extent: [1600, 900, 1],
                                copy_extent: [w, 900, 1],
                            };
                            let (r, why) = route(Knob::Transfer, device, c);
                            assert_eq!(r == Route::Transfer, why.is_none());
                            if r == Route::Transfer {
                                assert!(family.is_some() && buffer_destination && !blit);
                                assert!(granularity_ok(
                                    g,
                                    [0, 0, 0],
                                    c.copy_extent,
                                    c.image_extent
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rings_and_barrier_families() {
        assert_eq!(Route::Main.ring_idx(), 1);
        assert_eq!(Route::Transfer.ring_idx(), 2);
        assert_ne!(MAIN_RING_IDX, COPY_RING_IDX);
        assert_eq!(barrier_family(Route::Main, Some(1)), 0);
        assert_eq!(barrier_family(Route::Main, None), 0);
        assert_eq!(barrier_family(Route::Transfer, Some(1)), 1);
        assert_eq!(barrier_family(Route::Transfer, Some(2)), 2);
        assert_eq!(barrier_family(Route::Transfer, None), 0);
    }

    #[test]
    fn conflicts_need_another_route_a_shared_resource_and_a_submission() {
        let next = Record {
            route: Route::Transfer,
            source: 10,
            destination: 20,
            last_fence: 0,
        };
        let main_same_dst = Record {
            route: Route::Main,
            source: 11,
            destination: 20,
            last_fence: 5,
        };
        assert!(conflicts(next, main_same_dst));
        assert!(!conflicts(
            next,
            Record {
                last_fence: 0,
                ..main_same_dst
            }
        ));
        assert!(!conflicts(
            next,
            Record {
                route: Route::Transfer,
                ..main_same_dst
            }
        ));
        let main_same_src = Record {
            route: Route::Main,
            source: 10,
            destination: 21,
            last_fence: 5,
        };
        assert!(conflicts(next, main_same_src));
        let unrelated = Record {
            route: Route::Main,
            source: 11,
            destination: 21,
            last_fence: 5,
        };
        assert!(!conflicts(next, unrelated));
        // A resource that is the source of one and the destination of the other.
        let crossed = Record {
            route: Route::Main,
            source: 20,
            destination: 30,
            last_fence: 5,
        };
        assert!(conflicts(next, crossed));
        // Symmetric.
        let n2 = Record {
            last_fence: 7,
            ..next
        };
        assert!(conflicts(
            Record {
                last_fence: 0,
                ..main_same_dst
            },
            n2
        ));
    }

    #[test]
    fn granularity_packing() {
        assert_eq!(pack_granularity([1, 1, 1]), 0x01_01_01);
        assert_eq!(pack_granularity([0, 0, 0]), 0);
        assert_eq!(pack_granularity([4096, 8, 1]), 0x01_08_ff);
    }

    #[test]
    fn reason_codes_are_distinct_and_fit_the_mask() {
        let mut codes: Vec<u32> = Why::ALL.iter().map(|w| w.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), Why::ALL.len());
        assert!(codes.iter().all(|&c| (1..=32).contains(&c)));
        let mask = Why::ALL.iter().fold(0, |m, w| m | w.bit());
        assert_eq!(mask, 0b1111);
    }

    // ── counter names ────────────────────────────────────────────────────────────────────

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Cq"));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        for n in COUNTERS {
            assert!(!crate::blt_async::COUNTERS.contains(n));
            assert!(!crate::guest_blob::COUNTERS.contains(n));
        }
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join("ddi/copy_queue.rs")).unwrap();
        let written = literals(&text);
        for n in COUNTERS {
            assert!(
                written.iter().any(|l| l == n),
                "{n} is listed but not written by ddi/copy_queue.rs"
            );
        }
        for l in written.iter().filter(|l| l.starts_with("Cq")) {
            assert!(
                COUNTERS.contains(&l.as_str()),
                "{l} is written by ddi/copy_queue.rs but not listed"
            );
        }
    }

    #[test]
    fn no_other_file_writes_these_names_and_the_knob_is_in_diag() {
        let Some(render) = render_src() else {
            return;
        };
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().into_owned();
                    if s.ends_with("ddi/copy_queue.rs") || s.ends_with("/diag.rs") {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(!text.contains("b\"Cq"), "{s} spells a Cq counter name");
                    assert!(!text.contains("b\"CopyQueue\""), "{s} spells the knob name");
                }
            }
        }
        assert!(checked > 20);
        let diag = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        assert!(diag.contains("KnobName::new(b\"CopyQueue\")"));
    }
}

//! The display's mode list (host `protocol::modes`, docs/SCANOUT.md "Mode list").
//!
//! The host's virtual monitor offers a list of modes, like a physical monitor's
//! EDID: native first, then the standard modes that fit within native and the
//! VM's custom modes, all at one refresh rate. A host that has one sends it as
//! `DisplayModeList` (message 34) on the event queue to a guest that acked the
//! device feature [`FEATURE`]; this module parses it, and builds the list the KMD
//! offers Windows from it -- or, without one (an older host), from native and the
//! standard modes alone.
//!
//! The KMD puts every size of the list in the VidPN source and target mode sets
//! and the monitor's source mode set, so Display Settings and games' fullscreen
//! menus list them, and keeps a committed non-native size as the scanout extent.
//! A pinned size on one end of the path restricts the other end to that same
//! size ([`ModeList::cofunctional`]): the KMD scans out exactly the source size.
//!
//! Sizes are stored packed, `(width << 16) | height`, which is also the form the
//! adapter publishes through atomics (every extent here is at most 16384).

use crate::{MAX_DISPLAY_EXTENT, MIN_DISPLAY_HEIGHT, MIN_DISPLAY_WIDTH};

/// The device feature bit (`NVGPU_F_MODE_LIST`, virtio feature 21).
pub const FEATURE: u64 = 1 << 21;
/// Host `MsgType::DisplayModeList`.
pub const MSG_DISPLAY_MODE_LIST: u32 = 34;
/// At most this many modes in a list (host `protocol::modes::MODE_LIST_MAX`).
pub const MODE_LIST_MAX: usize = 48;
/// Message header, then the list header (scanout, count, refresh_mhz, flags).
pub const MSG_HEADER_BYTES: usize = 16;
pub const LIST_HEADER_BYTES: usize = 16;
pub const ENTRY_BYTES: usize = 8;

/// Bytes of a whole `DisplayModeList` message of `count` modes.
pub const fn msg_bytes(count: usize) -> usize {
    MSG_HEADER_BYTES + LIST_HEADER_BYTES + count * ENTRY_BYTES
}

/// The standard modes, the host's own table (`protocol::modes::STANDARD_MODES`).
pub const STANDARD_MODES: [(u32, u32); 25] = [
    (640, 480),
    (800, 600),
    (1024, 768),
    (1280, 720),
    (1280, 800),
    (1280, 960),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1440, 1080),
    (1600, 900),
    (1600, 1200),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1080),
    (2560, 1440),
    (2560, 1600),
    (3440, 1440),
    (3840, 1080),
    (3840, 1600),
    (3840, 2160),
    (5120, 1440),
    (5120, 2160),
    (5120, 2880),
];

/// `(w << 16) | h`.
pub const fn pack(w: u32, h: u32) -> u32 {
    (w << 16) | (h & 0xFFFF)
}

/// The inverse of [`pack`].
pub const fn unpack(p: u32) -> (u32, u32) {
    (p >> 16, p & 0xFFFF)
}

/// A size the KMD can offer: within the extents it adopts for native.
pub const fn usable(w: u32, h: u32) -> bool {
    w >= MIN_DISPLAY_WIDTH && h >= MIN_DISPLAY_HEIGHT && w <= MAX_DISPLAY_EXTENT && h <= MAX_DISPLAY_EXTENT
}

/// The modes offered, native first. Fixed-size and `Copy`: built on the stack of
/// a VidPN DDI, no allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeList {
    len: usize,
    packed: [u32; MODE_LIST_MAX],
}

impl ModeList {
    pub const EMPTY: Self = Self {
        len: 0,
        packed: [0; MODE_LIST_MAX],
    };

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Mode `i`, `(width, height)`.
    pub fn get(&self, i: usize) -> Option<(u32, u32)> {
        (i < self.len).then(|| unpack(self.packed[i]))
    }

    /// The packed modes.
    pub fn packed(&self) -> &[u32] {
        &self.packed[..self.len]
    }

    pub fn contains(&self, w: u32, h: u32) -> bool {
        self.packed().contains(&pack(w, h))
    }

    /// Append, unless unusable, present or full. Whether it was added.
    fn push(&mut self, w: u32, h: u32) -> bool {
        if !usable(w, h) || self.contains(w, h) || self.len == MODE_LIST_MAX {
            return false;
        }
        self.packed[self.len] = pack(w, h);
        self.len += 1;
        true
    }

    /// Without a host list: native, then the standard modes that fit within it,
    /// largest area first. An unusable native yields an empty list (the caller
    /// keeps its single-mode behaviour).
    pub fn standard(native_w: u32, native_h: u32) -> Self {
        let mut l = Self::EMPTY;
        if !l.push(native_w, native_h) {
            return l;
        }
        // STANDARD_MODES is ordered by area only roughly: walk it largest first
        // by area, ties wider first, the host's order.
        let mut taken = [false; STANDARD_MODES.len()];
        loop {
            let mut best: Option<usize> = None;
            for (i, &(w, h)) in STANDARD_MODES.iter().enumerate() {
                if taken[i] || w > native_w || h > native_h {
                    continue;
                }
                best = match best {
                    None => Some(i),
                    Some(b) => {
                        let (bw, bh) = STANDARD_MODES[b];
                        let (a, ba) = (u64::from(w) * u64::from(h), u64::from(bw) * u64::from(bh));
                        if a > ba || (a == ba && w > bw) {
                            Some(i)
                        } else {
                            Some(b)
                        }
                    }
                };
            }
            let Some(i) = best else {
                break;
            };
            taken[i] = true;
            let (w, h) = STANDARD_MODES[i];
            l.push(w, h);
        }
        l
    }

    /// From the host's list: native first whatever the host put first (the KMD's
    /// native is the monitor's, from its EDID), then the host's modes in its
    /// order, unusable ones and repeats skipped.
    pub fn from_host(native_w: u32, native_h: u32, host: &HostList) -> Self {
        let mut l = Self::EMPTY;
        if !l.push(native_w, native_h) {
            return l;
        }
        for &p in host.packed() {
            let (w, h) = unpack(p);
            l.push(w, h);
        }
        l
    }

    /// The sizes offered on one end of a path, given what is pinned on the other
    /// end (`None` = nothing pinned): everything, or exactly the pinned size when
    /// it is in the list (empty when it is not -- no cofunctional mode exists).
    pub fn cofunctional(&self, pinned_other: Option<(u32, u32)>) -> Self {
        match pinned_other {
            None => *self,
            Some((w, h)) => {
                let mut l = Self::EMPTY;
                if self.contains(w, h) {
                    l.packed[0] = pack(w, h);
                    l.len = 1;
                }
                l
            }
        }
    }

    /// Whether a VidPN with these pinned sizes can be shown: each pinned size is
    /// in the list, and a pinned source and target agree (the KMD scans out the
    /// source size unscaled).
    pub fn supports(&self, source: Option<(u32, u32)>, target: Option<(u32, u32)>) -> bool {
        let ok = |m: Option<(u32, u32)>| m.is_none_or(|(w, h)| self.contains(w, h));
        if !ok(source) || !ok(target) {
            return false;
        }
        match (source, target) {
            (Some(s), Some(t)) => s == t,
            _ => true,
        }
    }
}

/// A `DisplayModeList` as the host sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostList {
    len: usize,
    packed: [u32; MODE_LIST_MAX],
    /// The rate of every mode, millihertz (0 if the host's was out of range).
    pub refresh_mhz: u32,
}

impl HostList {
    /// A list from packed sizes already checked by [`parse`] (the adapter's
    /// published copy); unusable entries and repeats are dropped again, extra
    /// entries past the cap ignored.
    pub fn from_packed(packed: &[u32], refresh_mhz: u32) -> Self {
        let mut l = Self {
            len: 0,
            packed: [0; MODE_LIST_MAX],
            refresh_mhz,
        };
        for &p in packed.iter().take(MODE_LIST_MAX) {
            let (w, h) = unpack(p);
            if usable(w, h) && !l.packed().contains(&p) {
                l.packed[l.len] = p;
                l.len += 1;
            }
        }
        l
    }

    pub fn packed(&self) -> &[u32] {
        &self.packed[..self.len]
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
}

/// Parse a whole `DisplayModeList` message (`bytes[..len]`, header included).
/// `None` for another message, another scanout, a count of 0 or over the cap, or
/// a message shorter than its count. Unusable entries and repeats are dropped
/// here; a list left empty is `None`.
pub fn parse(bytes: &[u8], len: usize) -> Option<HostList> {
    let bytes = bytes.get(..len.min(bytes.len()))?;
    if le32(bytes, 0)? != MSG_DISPLAY_MODE_LIST {
        return None;
    }
    let p = MSG_HEADER_BYTES;
    if le32(bytes, p)? != 0 {
        return None;
    }
    let count = le32(bytes, p + 4)? as usize;
    if count == 0 || count > MODE_LIST_MAX || bytes.len() < msg_bytes(count) {
        return None;
    }
    let mhz = le32(bytes, p + 8)?;
    let mut l = HostList {
        len: 0,
        packed: [0; MODE_LIST_MAX],
        refresh_mhz: if (crate::MIN_REFRESH_MHZ..=crate::MAX_REFRESH_MHZ).contains(&mhz) {
            mhz
        } else {
            0
        },
    };
    for i in 0..count {
        let at = p + LIST_HEADER_BYTES + i * ENTRY_BYTES;
        let (w, h) = (le32(bytes, at)?, le32(bytes, at + 4)?);
        if !usable(w, h) || l.packed().contains(&pack(w, h)) {
            continue;
        }
        l.packed[l.len] = pack(w, h);
        l.len += 1;
    }
    (l.len != 0).then_some(l)
}

/// The scanout extent: the committed source size, if one was committed (packed,
/// 0 = none), else native.
pub const fn scanout_extent(committed_packed: u32, native: (u32, u32)) -> (u32, u32) {
    if committed_packed == 0 {
        native
    } else {
        unpack(committed_packed)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    fn sizes(l: &ModeList) -> std::vec::Vec<(u32, u32)> {
        (0..l.len()).filter_map(|i| l.get(i)).collect()
    }

    fn msg(scanout: u32, count: u32, mhz: u32, modes: &[(u32, u32)]) -> std::vec::Vec<u8> {
        let mut b = std::vec::Vec::new();
        for v in [MSG_DISPLAY_MODE_LIST, 0, 0, 0, scanout, count, mhz, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for &(w, h) in modes {
            b.extend_from_slice(&w.to_le_bytes());
            b.extend_from_slice(&h.to_le_bytes());
        }
        b
    }

    #[test]
    fn the_standard_table_is_the_hosts() {
        // Kept in step with host/backend/protocol/src/modes.rs by hand: same
        // length, same first and last entries, ascending area within reason.
        assert_eq!(STANDARD_MODES.len(), 25);
        assert_eq!(STANDARD_MODES[0], (640, 480));
        assert_eq!(STANDARD_MODES[24], (5120, 2880));
        assert_eq!(FEATURE, 1 << 21);
        assert_eq!(msg_bytes(MODE_LIST_MAX), 416);
    }

    #[test]
    fn without_a_host_list_native_and_the_standard_modes_that_fit() {
        let l = ModeList::standard(1920, 1080);
        let s = sizes(&l);
        assert_eq!(s[0], (1920, 1080));
        assert!(s.iter().all(|&(w, h)| w <= 1920 && h <= 1080));
        assert!(s.contains(&(1440, 1080)));
        assert!(!s.contains(&(1920, 1200)));
        assert_eq!(*s.last().unwrap(), (640, 480));
        for w in s[1..].windows(2) {
            assert!(w[0].0 * w[0].1 >= w[1].0 * w[1].1, "{w:?}");
        }
        let mut d = s.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), s.len());

        let uw = sizes(&ModeList::standard(5120, 1440));
        assert_eq!(uw[0], (5120, 1440));
        assert_eq!(uw.iter().filter(|m| **m == (5120, 1440)).count(), 1);
        for m in [(3840, 1080), (3440, 1440), (2560, 1440), (1280, 960)] {
            assert!(uw.contains(&m), "{m:?}");
        }
        // Below every standard mode: native alone; unusable native: nothing.
        assert_eq!(sizes(&ModeList::standard(400, 300)), vec![(400, 300)]);
        assert!(ModeList::standard(100, 100).is_empty());
    }

    #[test]
    fn the_host_list_parses() {
        let b = msg(0, 3, 240_000, &[(5120, 1440), (1234, 567), (1920, 1080)]);
        let h = parse(&b, b.len()).unwrap();
        assert_eq!(h.refresh_mhz, 240_000);
        assert_eq!(h.packed(), &[pack(5120, 1440), pack(1234, 567), pack(1920, 1080)]);
        // A longer buffer than the message is fine; `len` bounds it.
        let mut big = b.clone();
        big.resize(512, 0xAA);
        assert_eq!(parse(&big, b.len()), Some(h));
        assert_eq!(parse(&big, big.len()), Some(h));
    }

    #[test]
    fn bad_lists_are_refused() {
        let b = msg(1, 1, 60_000, &[(1920, 1080)]);
        assert_eq!(parse(&b, b.len()), None, "another scanout");
        let b = msg(0, 0, 60_000, &[]);
        assert_eq!(parse(&b, b.len()), None, "empty");
        let b = msg(0, 2, 60_000, &[(1920, 1080)]);
        assert_eq!(parse(&b, b.len()), None, "shorter than its count");
        let b = msg(0, 49, 60_000, &[(1920, 1080); 49]);
        assert_eq!(parse(&b, b.len()), None, "over the cap");
        let b = msg(0, 2, 60_000, &[(100, 100), (99_999, 1080)]);
        assert_eq!(parse(&b, b.len()), None, "nothing usable");
        let mut b = msg(0, 1, 60_000, &[(1920, 1080)]);
        b[0] = 33;
        assert_eq!(parse(&b, b.len()), None, "another message");
        assert_eq!(parse(&b[..10], 10), None);
        // Truncated by `len` though the buffer holds it.
        let b = msg(0, 2, 60_000, &[(1920, 1080), (1280, 720)]);
        assert_eq!(parse(&b, b.len() - 1), None);
    }

    #[test]
    fn host_entries_are_filtered_and_rates_bounded() {
        let b = msg(0, 4, 5_000_000, &[(1920, 1080), (100, 100), (1920, 1080), (1280, 960)]);
        let h = parse(&b, b.len()).unwrap();
        assert_eq!(h.packed(), &[pack(1920, 1080), pack(1280, 960)]);
        assert_eq!(h.refresh_mhz, 0);
    }

    #[test]
    fn the_monitors_native_comes_first_whatever_the_host_sent() {
        let b = msg(0, 3, 60_000, &[(2560, 1440), (1920, 1080), (5120, 1440)]);
        let h = parse(&b, b.len()).unwrap();
        let l = ModeList::from_host(5120, 1440, &h);
        assert_eq!(sizes(&l), vec![(5120, 1440), (2560, 1440), (1920, 1080)]);
        // A custom mode larger than native is kept.
        let b = msg(0, 2, 60_000, &[(1920, 1080), (2560, 1440)]);
        let l = ModeList::from_host(1920, 1080, &parse(&b, b.len()).unwrap());
        assert_eq!(sizes(&l), vec![(1920, 1080), (2560, 1440)]);
    }

    #[test]
    fn a_pinned_size_restricts_the_other_end() {
        let l = ModeList::standard(1920, 1080);
        assert_eq!(l.cofunctional(None), l);
        assert_eq!(sizes(&l.cofunctional(Some((1280, 720)))), vec![(1280, 720)]);
        assert!(l.cofunctional(Some((1234, 567))).is_empty());
    }

    #[test]
    fn supported_vidpns() {
        let l = ModeList::standard(1920, 1080);
        assert!(l.supports(None, None));
        assert!(l.supports(Some((1280, 720)), None));
        assert!(l.supports(None, Some((1920, 1080))));
        assert!(l.supports(Some((1280, 720)), Some((1280, 720))));
        assert!(!l.supports(Some((1280, 720)), Some((1920, 1080))));
        assert!(!l.supports(Some((1234, 567)), None));
        assert!(!l.supports(None, Some((2560, 1440))));
    }

    #[test]
    fn a_published_copy_round_trips() {
        let b = msg(0, 3, 60_000, &[(5120, 1440), (1234, 567), (1920, 1080)]);
        let h = parse(&b, b.len()).unwrap();
        assert_eq!(HostList::from_packed(h.packed(), h.refresh_mhz), h);
        let junk = [pack(1920, 1080), 0, pack(1920, 1080), pack(100, 100)];
        assert_eq!(HostList::from_packed(&junk, 0).packed(), &[pack(1920, 1080)]);
    }

    #[test]
    fn the_scanout_extent_is_the_committed_size_or_native() {
        assert_eq!(scanout_extent(0, (5120, 1440)), (5120, 1440));
        assert_eq!(scanout_extent(pack(1280, 960), (5120, 1440)), (1280, 960));
        assert_eq!(unpack(pack(16384, 16384)), (16384, 16384));
    }
}

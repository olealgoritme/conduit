//! `NvSemWatch`: one GPU semaphore address watched end to end (the I/O half is
//! `kmd_render::ddi::sem_watch`).
//!
//! The knob `NvSemWatch` is the VA's low 32 bits, `NvSemWatchHi` the high ones (0 = off when
//! both are 0). Three registry rings of [`RING`] records, four dwords each, published by the
//! mirror pass as `NvSw<ring><field><kk>` (ring `R`, `V` or `M`, field `a`..`d`, `kk` the slot
//! = record number % [`RING`], two decimal digits), with `NvSw<ring>N` the records ever:
//!
//! * `R` releases to the VA seen in a submission (or done by a KMD executor): `a` =
//!   [`release_word`] (source, semaphore kind, node, operation, channel id), `b` the payload's low 32
//!   bits, `c` interrupt time (ms), `d` the submitter's pid (or the process key's low 32 bits).
//! * `V` changes of the value at the VA as the GPU sees it, sampled every
//!   [`SAMPLE_MS`]: `a` / `b` the value's low / high 32 bits, `c` time (ms), `d` the
//!   process key's low 32 bits.
//! * `M` CPU-mapping and paging events of the allocation holding the VA: `a` =
//!   [`map_word`] (op, allocation id), `b` / `d` the op's details ([`map_op`]), `c` time (ms).

/// Records per ring.
pub const RING: usize = 16;
/// How often the value is sampled while the watch is on (ms).
pub const SAMPLE_MS: u32 = 100;
/// Native-submit segments scanned per submission, and dwords per segment.
pub const SUBMIT_SEGMENTS: u32 = 64;
pub const SUBMIT_SEG_DWORDS: u32 = 4096;

/// The watched VA from the knobs: `None` when off (both 0, or unset).
pub const fn target(lo: u32, hi: u32) -> Option<u64> {
    let lo = if lo == u32::MAX { 0 } else { lo };
    let hi = if hi == u32::MAX { 0 } else { hi };
    let va = (hi as u64) << 32 | lo as u64;
    if va == 0 {
        None
    } else {
        Some(va)
    }
}

/// Where a release (or a dropped submission) was seen.
pub mod src {
    /// A native HW-queue submission (`SubmitCommandToHwQueue`), scanned at submit.
    pub const NATIVE: u32 = 1;
    /// A copy-context channel submission (`nv_copy`), scanned from the mirror pass.
    pub const COPY_CHANNEL: u32 = 2;
    /// The copy context's CPU executor wrote it (`NvCcCpu`).
    pub const COPY_CPU: u32 = 3;
    /// Dropped: an NVDA HW-queue context on a node without a channel; completed unrun.
    pub const SOFT_DROP: u32 = 4;
    /// Dropped: an NVDA SubmitCommand context without a channel or record; completed unrun.
    pub const COPY_DROP: u32 = 5;
    pub const COUNT: usize = 6;
}

/// `R` ring word `a`: source << 28 | semaphore kind << 24 | node << 16 | operation << 12 |
/// channel id (12 bits). The operation is `native_snap::SemOp::op` (for a host `SEM_EXECUTE`
/// 1 RELEASE, 6 REDUCTION: then `b` is the reduction's operand, not the value written).
pub const fn release_word(source: u32, sem_kind: u8, node: u32, op: u8, chid: u32) -> u32 {
    (source & 0xf) << 28 | (sem_kind as u32 & 0xf) << 24 | (node & 0xff) << 16 | (op as u32 & 0xf) << 12 | (chid & 0xfff)
}

/// CPU-mapping / paging operations of the watched allocation (`M` ring).
pub mod map_op {
    /// `MapCpuHostAperture` mapped it: `b` aperture offset >> 12, `d` guest physical >> 12.
    pub const APERTURE_MAP: u32 = 1;
    /// `MapCpuHostAperture` failed: `b` aperture offset >> 12, `d` the error + 1.
    pub const APERTURE_MAP_FAIL: u32 = 2;
    /// `UnmapCpuHostAperture`: `b` aperture offset >> 12, `d` 0 ok / error + 1.
    pub const APERTURE_UNMAP: u32 = 3;
    /// A paging content operation: `b` the `pg_check::op_word`, `d` 0.
    pub const PAGING: u32 = 4;
    /// Destroyed.
    pub const DESTROY: u32 = 5;
}

/// `M` ring word `a`: op << 24 | allocation id (24 bits).
pub const fn map_word(op: u32, id: u32) -> u32 {
    (op & 0xff) << 24 | (id & 0xff_ffff)
}

/// A ring of four-dword records; `n` records ever.
#[derive(Clone, Copy)]
pub struct Ring {
    pub recs: [[u32; 4]; RING],
    pub n: u32,
    /// Records already published.
    pub published: u32,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub const fn new() -> Self {
        Self {
            recs: [[0; 4]; RING],
            n: 0,
            published: 0,
        }
    }

    pub fn push(&mut self, rec: [u32; 4]) {
        self.recs[self.n as usize % RING] = rec;
        self.n = self.n.wrapping_add(1);
    }

    /// The records not yet published (at most the ring), as (slot, record); marks them
    /// published.
    pub fn take_new(&mut self, out: &mut [(usize, [u32; 4]); RING]) -> usize {
        let pending = self.n.wrapping_sub(self.published).min(RING as u32);
        let first = self.n.wrapping_sub(pending);
        for k in 0..pending {
            let i = first.wrapping_add(k);
            let slot = i as usize % RING;
            out[k as usize] = (slot, self.recs[slot]);
        }
        self.published = self.n;
        pending as usize
    }
}

/// The registry name of one ring field: `NvSw` + ring + field + two digits.
pub const fn name(ring: u8, field: u8, slot: usize) -> [u8; 8] {
    [b'N', b'v', b'S', b'w', ring, field, b'0' + (slot / 10 % 10) as u8, b'0' + (slot % 10) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs() {
        assert_eq!(target(0, 0), None);
        assert_eq!(target(u32::MAX, u32::MAX), None);
        assert_eq!(target(0x852d070, 0), Some(0x852d070));
        assert_eq!(target(0x852d070, u32::MAX), Some(0x852d070));
        assert_eq!(target(0x1000, 1), Some(0x1_0000_1000));
    }

    #[test]
    fn words_and_names() {
        assert_eq!(release_word(src::NATIVE, 1, 0, 1, 0x30), 0x1100_1030);
        assert_eq!(release_word(src::COPY_DROP, 0, 3, 0, 0x1234), 0x5003_0234);
        assert_eq!(release_word(src::NATIVE, 1, 2, 6, 0x31), 0x1102_6031);
        assert_eq!(map_word(map_op::APERTURE_MAP, 2414), 0x0100_096e);
        assert_eq!(&name(b'R', b'a', 7), b"NvSwRa07");
        assert_eq!(&name(b'V', b'c', 15), b"NvSwVc15");
    }

    #[test]
    fn ring_publishes_each_record_once() {
        let mut r = Ring::new();
        let mut out = [(0usize, [0u32; 4]); RING];
        assert_eq!(r.take_new(&mut out), 0);
        r.push([1, 0, 0, 0]);
        r.push([2, 0, 0, 0]);
        assert_eq!(r.take_new(&mut out), 2);
        assert_eq!((out[0].0, out[0].1[0], out[1].0, out[1].1[0]), (0, 1, 1, 2));
        assert_eq!(r.take_new(&mut out), 0);
        // 20 more: only the ring's last 16 are published, oldest first.
        for i in 0..20u32 {
            r.push([100 + i, 0, 0, 0]);
        }
        assert_eq!(r.take_new(&mut out), RING);
        assert_eq!(out[0].1[0], 104);
        assert_eq!(out[0].0, 6);
        assert_eq!(out[RING - 1].1[0], 119);
        assert_eq!(r.n, 22);
    }
}

//! A publisher of the session-wide hand-off ledger, for the D3D12 driver.
//!
//! The ledger ("Local\\HeliosHandoffLedger5", `docs/shared-surfaces.md` section 4) is written
//! and read by the D3D11 driver's DXVK bridge (`umd/bridge/dxvk_bridge.cpp`, namespace
//! `helios_handoff`). `FlipLedger` (`docs/dwm-on-nvk.md` 4.5) has every NVK app publish a point
//! per flip-model present on the presented buffer's resource id, completed when the frame's GPU
//! work is done; a DWM on NVK composes the newest buffer of a swap chain whose point is complete.
//! A D3D12 app has no DXVK, so this module writes the same records: one device record per
//! process, points from the ledger-wide sequence, the slot entry of the buffer's key, and the
//! completion once the caller saw the frame finish.
//!
//! The layout below is the C++ one, byte for byte (`static_assert`s there, `const` asserts
//! here). What this publisher leaves out: the reclamation sweeps (a full table refuses, counted
//! by the caller) and the recent-points history only readers that also publish need
//! (`sample_before`), which it still fills so a D3D11 reader's bound logic sees consistent data.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAGIC: u32 = 0x354C_4448; // 'HDL5'
pub const DEVICES: usize = 4096;
pub const SLOTS: usize = 32768;
pub const PROBE: usize = 64;
pub const PIDS: usize = 4;
pub const ENTRIES: usize = 4;
pub const RECENT: usize = 12;
pub const TOMBSTONE: u32 = 0xFFFF_FFFF;
pub const POINT_MASK: u64 = (1u64 << 40) - 1;
pub const GEN_MASK: u32 = 0xFFF;

#[repr(C)]
pub struct Device {
    pub pid: AtomicU32,
    pub gen: AtomicU32,
    pub completed: AtomicU64,
    pub recent: [AtomicU64; RECENT],
    pub recent_head: AtomicU32,
    pub reserved: u32,
}

#[repr(C)]
pub struct Slot {
    pub key: AtomicU32,
    pub extra_holders: AtomicU32,
    pub pids: [AtomicU32; PIDS],
    pub points: [AtomicU64; ENTRIES],
    pub reserved: u64,
}

#[repr(C)]
pub struct Table {
    pub magic: AtomicU32,
    pub version: u32,
    pub next_device: AtomicU32,
    pub records_in_use: AtomicU32,
    pub slots_in_use: AtomicU32,
    pub fallbacks: AtomicU32,
    pub sweeps: AtomicU32,
    pub handoffs: AtomicU32,
    pub publish_seq: AtomicU64,
    pub devices: [Device; DEVICES],
    pub slots: [Slot; SLOTS],
}

const _: () = assert!(core::mem::size_of::<Device>() == 16 + 8 * RECENT + 8);
const _: () = assert!(core::mem::size_of::<Slot>() == 64);
const _: () = assert!(
    core::mem::size_of::<Table>()
        == 40 + core::mem::size_of::<Device>() * DEVICES + core::mem::size_of::<Slot>() * SLOTS
);

/// `device (12 bits) << 52 | generation (12 bits) << 40 | point (40 bits)`.
pub const fn pack(device: u32, gen: u32, point: u64) -> u64 {
    ((device as u64) << 52) | (((gen & GEN_MASK) as u64) << 40) | (point & POINT_MASK)
}

const fn hash(key: u32) -> usize {
    (key.wrapping_mul(2_654_435_761) as usize) % SLOTS
}

/// The slot holding `key`, if any.
pub fn find(t: &Table, key: u32) -> Option<&Slot> {
    let mut h = hash(key);
    for _ in 0..PROBE {
        let k = t.slots[h].key.load(Ordering::Acquire);
        if k == key {
            return Some(&t.slots[h]);
        }
        if k == 0 {
            return None;
        }
        h = (h + 1) % SLOTS;
    }
    None
}

/// The slot of `key`, claimed (empty or a tombstone) when there is none; `pid` holds it.
pub fn find_or_claim(t: &Table, key: u32, pid: u32) -> Option<&Slot> {
    if let Some(s) = find(t, key) {
        add_holder(s, pid);
        return Some(s);
    }
    let mut h = hash(key);
    for _ in 0..PROBE {
        let s = &t.slots[h];
        let k = s.key.load(Ordering::Acquire);
        if k == key {
            add_holder(s, pid);
            return Some(s);
        }
        if (k == 0 || k == TOMBSTONE)
            && s.key.compare_exchange(k, key, Ordering::AcqRel, Ordering::Acquire).is_ok()
        {
            for e in &s.points {
                e.store(0, Ordering::Release);
            }
            s.extra_holders.store(0, Ordering::Relaxed);
            for p in &s.pids {
                p.store(0, Ordering::Relaxed);
            }
            t.slots_in_use.fetch_add(1, Ordering::Relaxed);
            add_holder(s, pid);
            return Some(s);
        }
        if s.key.load(Ordering::Acquire) == key {
            add_holder(s, pid);
            return Some(s);
        }
        h = (h + 1) % SLOTS;
    }
    None
}

fn add_holder(s: &Slot, pid: u32) {
    if s.pids.iter().any(|p| p.load(Ordering::Relaxed) == pid) {
        return;
    }
    for p in &s.pids {
        if p.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return;
        }
    }
    s.extra_holders.fetch_add(1, Ordering::Relaxed);
}

/// Claim a device record for `pid` (a fresh one, else a freed one). `(record, generation)`.
pub fn claim_device(t: &Table, pid: u32) -> Option<(u32, u32)> {
    let take = |d: usize| {
        let dev = &t.devices[d];
        let gen = dev.gen.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        dev.completed.store(0, Ordering::Release);
        for r in &dev.recent {
            r.store(0, Ordering::Relaxed);
        }
        dev.recent_head.store(0, Ordering::Release);
        t.records_in_use.fetch_add(1, Ordering::Relaxed);
        (d as u32, gen)
    };
    let mut d = t.next_device.load(Ordering::Relaxed);
    while (d as usize) < DEVICES {
        match t.next_device.compare_exchange_weak(d, d + 1, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                t.devices[d as usize].pid.store(pid, Ordering::Release);
                return Some(take(d as usize));
            }
            Err(now) => d = now,
        }
    }
    for i in 0..DEVICES {
        if t.devices[i].pid.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return Some(take(i));
        }
    }
    None
}

/// Whether a packed entry is done: empty, its record moved on, or its point completed.
pub fn entry_done(t: &Table, packed: u64) -> bool {
    if packed == 0 {
        return true;
    }
    let d = (packed >> 52) as usize;
    if d >= DEVICES {
        return true;
    }
    let dev = &t.devices[d];
    (dev.gen.load(Ordering::Acquire) & GEN_MASK) != ((packed >> 40) as u32 & GEN_MASK)
        || dev.completed.load(Ordering::Acquire) >= (packed & POINT_MASK)
}

/// Put `packed` into the slot entry of its device (or a done entry). False: no entry free.
pub fn publish_point(t: &Table, s: &Slot, packed: u64) -> bool {
    let device_gen = !POINT_MASK;
    for e in &s.points {
        let mut cur = e.load(Ordering::Acquire);
        if cur != 0 && (cur & device_gen) == (packed & device_gen) {
            while (cur & device_gen) == (packed & device_gen) && (cur & POINT_MASK) < (packed & POINT_MASK) {
                match e.compare_exchange_weak(cur, packed, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(now) => cur = now,
                }
            }
            return true;
        }
    }
    for e in &s.points {
        let cur = e.load(Ordering::Acquire);
        if entry_done(t, cur) && e.compare_exchange(cur, packed, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return true;
        }
    }
    false
}

/// One device record of this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub index: u32,
    pub gen: u32,
}

/// A point published by [`publish`]: hand it to [`complete`] when the frame's work is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    pub record: Record,
    pub point: u64,
}

/// Why [`publish`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    NoKey,
    SequenceExhausted,
    NoSlot,
    SlotFull,
}

/// Publish the next point of `record` on `key`.
pub fn publish(t: &Table, record: Record, pid: u32, key: u32) -> Result<Published, Refused> {
    if key == 0 {
        return Err(Refused::NoKey);
    }
    let point = t.publish_seq.fetch_add(1, Ordering::AcqRel) + 1;
    if point > POINT_MASK {
        t.fallbacks.fetch_add(1, Ordering::Relaxed);
        return Err(Refused::SequenceExhausted);
    }
    let dev = &t.devices[record.index as usize];
    let h = dev.recent_head.fetch_add(1, Ordering::Relaxed) as usize;
    dev.recent[h % RECENT].store(point, Ordering::Release);
    let Some(slot) = find_or_claim(t, key, pid) else {
        t.fallbacks.fetch_add(1, Ordering::Relaxed);
        return Err(Refused::NoSlot);
    };
    if !publish_point(t, slot, pack(record.index, record.gen, point)) {
        t.fallbacks.fetch_add(1, Ordering::Relaxed);
        return Err(Refused::SlotFull);
    }
    t.handoffs.fetch_add(1, Ordering::Relaxed);
    Ok(Published { record, point })
}

/// The frame behind `p` is done: its record's completion reaches `p.point` (never goes back).
pub fn complete(t: &Table, p: Published) {
    let dev = &t.devices[p.record.index as usize];
    if dev.gen.load(Ordering::Acquire) != p.record.gen {
        return;
    }
    let mut cur = dev.completed.load(Ordering::Relaxed);
    while cur < p.point {
        match dev.completed.compare_exchange_weak(cur, p.point, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
}

#[cfg(windows)]
pub use mapping::table;

#[cfg(windows)]
mod mapping {
    use super::{Table, MAGIC};
    use core::ffi::c_void;
    use core::sync::atomic::Ordering;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(
            file: *mut c_void,
            attributes: *mut SecurityAttributes,
            protect: u32,
            size_high: u32,
            size_low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn MapViewOfFile(mapping: *mut c_void, access: u32, high: u32, low: u32, size: usize) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: *const u16,
            revision: u32,
            sd: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
    const INVALID_HANDLE_VALUE: *mut c_void = usize::MAX as *mut c_void;

    /// The session's ledger, mapped once per process with the C++ side's name and security
    /// (every principal of the session may map it: DWM runs as its own user). `None` when it
    /// cannot be mapped or holds another layout.
    pub fn table() -> Option<&'static Table> {
        static TABLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let p = *TABLE.get_or_init(|| {
            let sddl: Vec<u16> = "D:(A;;GA;;;WD)(A;;GA;;;SY)(A;;GA;;;AC)S:(ML;;NW;;;LW)\0"
                .encode_utf16()
                .collect();
            let name: Vec<u16> = "Local\\HeliosHandoffLedger5\0".encode_utf16().collect();
            let size = core::mem::size_of::<Table>();
            // SAFETY: NUL-terminated strings; the descriptor is freed after use; the view and
            // the mapping handle stay for the life of the process (the C++ side's reason: the
            // section's name lives only while a handle is open).
            unsafe {
                let mut sd = core::ptr::null_mut();
                let mut sa = SecurityAttributes { length: core::mem::size_of::<SecurityAttributes>() as u32, descriptor: core::ptr::null_mut(), inherit: 0 };
                if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, core::ptr::null_mut()) != 0 {
                    sa.descriptor = sd;
                }
                let mapping = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    if sa.descriptor.is_null() { core::ptr::null_mut() } else { &mut sa },
                    PAGE_READWRITE,
                    0,
                    size as u32,
                    name.as_ptr(),
                );
                if !sd.is_null() {
                    LocalFree(sd);
                }
                if mapping.is_null() {
                    return 0;
                }
                let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size);
                if view.is_null() {
                    CloseHandle(mapping);
                    return 0;
                }
                let t = &*(view as *const Table);
                let _ = t.magic.compare_exchange(0, MAGIC, Ordering::AcqRel, Ordering::Acquire);
                if t.magic.load(Ordering::Acquire) != MAGIC {
                    return 0;
                }
                view as usize
            }
        });
        // SAFETY: a live view of a `Table`-sized section, never unmapped.
        (p != 0).then(|| unsafe { &*(p as *const Table) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Box<Table> {
        // SAFETY: every field is an integer or an atomic integer; all-zero is the fresh state.
        unsafe {
            let layout = std::alloc::Layout::new::<Table>();
            let p = std::alloc::alloc_zeroed(layout) as *mut Table;
            assert!(!p.is_null());
            Box::from_raw(p)
        }
    }

    #[test]
    fn a_published_point_is_pending_until_completed() {
        let t = table();
        let r = claim_device(&t, 100).map(|(index, gen)| Record { index, gen }).unwrap();
        let p = publish(&t, r, 100, 42).unwrap();
        let slot = find(&t, 42).unwrap();
        let packed = slot.points.iter().map(|e| e.load(Ordering::Relaxed)).find(|&v| v != 0).unwrap();
        assert_eq!(packed, pack(r.index, r.gen, p.point));
        assert!(!entry_done(&t, packed));
        complete(&t, p);
        assert!(entry_done(&t, packed));
        assert_eq!(slot.pids[0].load(Ordering::Relaxed), 100);
    }

    #[test]
    fn a_device_keeps_one_entry_per_slot_with_its_newest_point() {
        let t = table();
        let r = claim_device(&t, 7).map(|(index, gen)| Record { index, gen }).unwrap();
        let a = publish(&t, r, 7, 5).unwrap();
        let b = publish(&t, r, 7, 5).unwrap();
        assert!(b.point > a.point);
        let slot = find(&t, 5).unwrap();
        let used: Vec<u64> = slot.points.iter().map(|e| e.load(Ordering::Relaxed)).filter(|&v| v != 0).collect();
        assert_eq!(used, vec![pack(r.index, r.gen, b.point)]);
        complete(&t, a);
        assert!(!entry_done(&t, used[0]));
        complete(&t, b);
        assert!(entry_done(&t, used[0]));
    }

    #[test]
    fn completion_never_goes_back_and_a_stale_record_is_ignored() {
        let t = table();
        let r = claim_device(&t, 9).map(|(index, gen)| Record { index, gen }).unwrap();
        let a = publish(&t, r, 9, 3).unwrap();
        let b = publish(&t, r, 9, 3).unwrap();
        complete(&t, b);
        complete(&t, a);
        assert_eq!(t.devices[r.index as usize].completed.load(Ordering::Relaxed), b.point);
        let stale = Published { record: Record { index: r.index, gen: r.gen + 1 }, point: b.point + 10 };
        complete(&t, stale);
        assert_eq!(t.devices[r.index as usize].completed.load(Ordering::Relaxed), b.point);
    }

    #[test]
    fn zero_keys_are_refused_and_distinct_keys_get_distinct_slots() {
        let t = table();
        let r = claim_device(&t, 1).map(|(index, gen)| Record { index, gen }).unwrap();
        assert_eq!(publish(&t, r, 1, 0), Err(Refused::NoKey));
        publish(&t, r, 1, 10).unwrap();
        publish(&t, r, 1, 11).unwrap();
        assert!(!core::ptr::eq(find(&t, 10).unwrap(), find(&t, 11).unwrap()));
        assert_eq!(t.slots_in_use.load(Ordering::Relaxed), 2);
    }
}

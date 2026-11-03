//! One long-term pin of all of a guest's RAM, taken once when its regions are
//! known and held for the life of the backend.
//!
//! RM registers guest pages (OS descriptors, `osdesc.rs`) by pinning them with
//! `FOLL_LONGTERM`, and the kernel first migrates every page it will not pin
//! long-term where it lies: a page in a `MIGRATE_CMA` pageblock. A host kernel
//! with Kexec HandOver enabled by default (Ubuntu's 7.0) keeps its KHO scratch
//! areas -- gigabytes -- as `MIGRATE_CMA`, and a VM's guest RAM, movable shmem
//! in 2 MiB THPs, lands there. A pin that meets such a page has to split the
//! THP and move it while the guest is using it, and when one 4 KiB page of it
//! will not move the whole registration is refused (`NV_ERR_INVALID_ADDRESS`)
//! and the GPU VA it was for stays unmapped.
//!
//! So the backend pins all of guest RAM itself, the same way RM does --
//! as RM OS descriptors over its own mapping of each region -- in chunks, early
//! and once. Every guest page that sat in a CMA pageblock is migrated out then,
//! and the pin keeps it out (compaction may otherwise move a movable page back
//! into one). Later registrations pin pages that are already long-term
//! pinnable and never need a migration. The RM client and everything it holds
//! go when this is dropped, or with the process.
//!
//! `CONDUIT_PIN_GUEST_RAM` = `1` or `0`; unset, it is on for the NVIDIA UMD
//! personality only.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// The switch.
pub const ENV: &str = "CONDUIT_PIN_GUEST_RAM";

/// Whether to pin, from the switch's value and the personality.
pub fn enabled(value: Option<&str>, nvidia_umd: bool) -> bool {
    match value.map(str::trim) {
        Some("1" | "on" | "yes" | "true") => true,
        Some("0" | "off" | "no" | "false") => false,
        _ => nvidia_umd,
    }
}

const PAGE: u64 = 4096;

/// One RM pin. Each holds the mmap lock of this process while the kernel pins
/// and migrates, so a chunk is kept small enough not to stall the vring
/// threads' own mappings for long.
pub const CHUNK: u64 = 256 << 20;

/// What a chunk that will not pin is split into, so one stuck page costs 2 MiB
/// rather than the whole chunk.
pub const PIECE: u64 = 2 << 20;

/// Waits before each further attempt at a refused pin.
pub const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(2),
    Duration::from_millis(10),
    Duration::from_millis(50),
    Duration::from_millis(200),
];

/// `NV_ERR_INVALID_ADDRESS`: the kernel would not pin the pages.
pub const NV_ERR_INVALID_ADDRESS: u32 = 0x1e;

/// Why a pin was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    /// RM's status.
    Rm(u32),
    /// The ioctl itself failed: the errno.
    Host(i32),
}

impl PinError {
    /// Only a pin the kernel refused is worth asking again.
    fn retryable(self) -> bool {
        self == PinError::Rm(NV_ERR_INVALID_ADDRESS)
    }
}

/// Something that pins `len` bytes at `addr` in this process long-term and
/// holds them until it is dropped.
pub trait Pinner: Send {
    fn pin(&mut self, addr: u64, len: u64) -> Result<(), PinError>;
}

/// A region of guest RAM, as the file it is mapped from.
#[derive(Debug)]
pub struct FileRange {
    pub fd: OwnedFd,
    pub offset: u64,
    pub len: u64,
}

/// What one round of pinning did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes newly pinned.
    pub pinned: u64,
    /// Bytes already pinned by an earlier round (the same file range).
    pub already: u64,
    /// Bytes that would not pin on any attempt, in [`PIECE`]s.
    pub refused: u64,
    /// Pins that were refused at first and held on a later attempt.
    pub retried: u64,
    /// RM objects taken.
    pub pins: u64,
}

/// A mapping of guest RAM made for the pin, unmapped on drop.
struct Mapping {
    addr: u64,
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped by `map` below with exactly this address and length,
        // and nothing else refers to it.
        unsafe { libc::munmap(self.addr as *mut libc::c_void, self.len) };
    }
}

// SAFETY: only an address and a length; the mapping is this process's.
unsafe impl Send for Mapping {}

/// The guest's RAM, pinned.
pub struct GuestRamPin<P: Pinner> {
    // Declared first, so dropped first: RM lets go of the pages before the
    // mapping it pinned them through goes.
    pinner: P,
    maps: Vec<Mapping>,
    /// What is pinned: (device, inode, start, end) of each file range.
    pinned: Vec<(u64, u64, u64, u64)>,
    sleep: fn(Duration),
    total: Stats,
}

impl<P: Pinner> GuestRamPin<P> {
    pub fn new(pinner: P) -> Self {
        Self {
            pinner,
            maps: Vec::new(),
            pinned: Vec::new(),
            sleep: std::thread::sleep,
            total: Stats::default(),
        }
    }

    #[cfg(test)]
    fn without_sleeping(mut self) -> Self {
        self.sleep = |_| {};
        self
    }

    /// Everything pinned so far, over every round.
    pub fn total(&self) -> Stats {
        self.total
    }

    /// Pin every part of `ranges` not already pinned.
    pub fn pin_ranges(&mut self, ranges: Vec<FileRange>) -> Stats {
        let mut st = Stats::default();
        for r in ranges {
            let start = r.offset & !(PAGE - 1);
            let end = (r.offset + r.len).div_ceil(PAGE) * PAGE;
            if end <= start {
                continue;
            }
            let Some((dev, ino)) = file_id(&r.fd) else {
                log::warn!("guest RAM pin: cannot identify a region's file; left unpinned");
                continue;
            };
            let gaps = uncovered(&self.pinned, dev, ino, start, end);
            st.already += (end - start) - gaps.iter().map(|(s, e)| e - s).sum::<u64>();
            for (s, e) in gaps {
                let len = e - s;
                let Some(map) = map(&r.fd, s, len) else {
                    log::warn!(
                        "guest RAM pin: mapping {len:#x} bytes at file offset {s:#x} failed: {}",
                        std::io::Error::last_os_error()
                    );
                    continue;
                };
                let base = map.addr;
                self.maps.push(map);
                let mut at = 0;
                while at < len {
                    let n = CHUNK.min(len - at);
                    self.pin_chunk(base + at, n, &mut st);
                    at += n;
                }
                self.pinned.push((dev, ino, s, e));
            }
        }
        self.total.pinned += st.pinned;
        self.total.refused += st.refused;
        self.total.retried += st.retried;
        self.total.pins += st.pins;
        st
    }

    /// Pin one chunk, splitting it into pieces when it will not pin whole.
    fn pin_chunk(&mut self, addr: u64, len: u64, st: &mut Stats) {
        if self.pin_with_retries(addr, len, st).is_ok() {
            st.pinned += len;
            return;
        }
        let mut at = 0;
        while at < len {
            let n = PIECE.min(len - at);
            match self.pin_with_retries(addr + at, n, st) {
                Ok(()) => st.pinned += n,
                Err(e) => {
                    st.refused += n;
                    log::warn!(
                        "guest RAM pin: {n:#x} bytes at {:#x} would not pin ({e:?}); a \
                         registration of pages there may still need a migration",
                        addr + at
                    );
                }
            }
            at += n;
        }
    }

    fn pin_with_retries(&mut self, addr: u64, len: u64, st: &mut Stats) -> Result<(), PinError> {
        let mut attempt = 0;
        loop {
            match self.pinner.pin(addr, len) {
                Ok(()) => {
                    st.pins += 1;
                    if attempt > 0 {
                        st.retried += 1;
                    }
                    return Ok(());
                }
                Err(e) if e.retryable() && attempt < RETRY_DELAYS.len() => {
                    (self.sleep)(RETRY_DELAYS[attempt]);
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn file_id(fd: &OwnedFd) -> Option<(u64, u64)> {
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut s: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a live descriptor and a valid out-parameter.
    (unsafe { libc::fstat(fd.as_raw_fd(), &mut s) } == 0).then_some((s.st_dev, s.st_ino))
}

/// The parts of [start, end) of file (dev, ino) that no pinned range covers.
fn uncovered(
    pinned: &[(u64, u64, u64, u64)],
    dev: u64,
    ino: u64,
    start: u64,
    end: u64,
) -> Vec<(u64, u64)> {
    let mut have: Vec<(u64, u64)> = pinned
        .iter()
        .filter(|p| p.0 == dev && p.1 == ino && p.2 < end && p.3 > start)
        .map(|p| (p.2.max(start), p.3.min(end)))
        .collect();
    have.sort_unstable();
    let mut out = Vec::new();
    let mut at = start;
    for (s, e) in have {
        if s > at {
            out.push((at, s));
        }
        at = at.max(e);
    }
    if at < end {
        out.push((at, end));
    }
    out
}

fn map(fd: &OwnedFd, offset: u64, len: u64) -> Option<Mapping> {
    // SAFETY: a fresh shared mapping of the region's file at an address of the
    // kernel's choosing.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            offset as libc::off_t,
        )
    };
    (p != libc::MAP_FAILED).then(|| Mapping {
        addr: p as u64,
        len: len as usize,
    })
}

/// Pins through RM, the way a registration does: a client of its own on
/// `/dev/nvidiactl`, a device, and one `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` per
/// pin on `/dev/nvidiaN`. Freeing the client releases them all.
pub struct RmPinner {
    ctl: OwnedFd,
    /// Held open: the descriptors were made on it, and RM drops them with it.
    dev: OwnedFd,
    client: u32,
    next: u32,
}

const NV01_ROOT_CLIENT: u32 = 0x41;
const NV01_DEVICE_0: u32 = 0x80;
const NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: u32 = 0x71;
const H_DEVICE: u32 = 0xc0de_0080;
const H_FIRST_PIN: u32 = 0xc0de_1000;
/// NVOS02 flags of an OS descriptor: PHYSICALITY NONCONTIGUOUS, LOCATION PCI,
/// COHERENCY CACHED, MAPPING NO_MAP -- what the guest driver sends.
const NVOS02_OSDESC_FLAGS: u32 = (1 << 4) | (1 << 12) | (1 << 30);

fn nv_ioctl(fd: &OwnedFd, escape: u32, arg: &mut [u8]) -> Result<(), i32> {
    let req = abi::ioctl::_IOWR(escape, arg.len() as u32);
    // SAFETY: `arg` is a live buffer of exactly the size the request encodes.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::Ioctl, arg.as_mut_ptr()) };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(())
    }
}

fn open_rw(path: &str) -> Result<OwnedFd, String> {
    let c = std::ffi::CString::new(path).unwrap();
    // SAFETY: a valid path.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!("{path}: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: just opened, owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// NVOS21: hRoot, hObjectParent, hObjectNew, hClass, pAllocParms, paramsSize,
/// status. Returns (status, hObjectNew).
fn rm_alloc(
    ctl: &OwnedFd,
    root: u32,
    parent: u32,
    new: u32,
    class: u32,
    params: &mut [u8],
) -> Result<(u32, u32), i32> {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&root.to_le_bytes());
    a[4..8].copy_from_slice(&parent.to_le_bytes());
    a[8..12].copy_from_slice(&new.to_le_bytes());
    a[12..16].copy_from_slice(&class.to_le_bytes());
    let p = if params.is_empty() {
        0
    } else {
        params.as_mut_ptr() as u64
    };
    a[16..24].copy_from_slice(&p.to_le_bytes());
    a[24..28].copy_from_slice(&(params.len() as u32).to_le_bytes());
    nv_ioctl(ctl, abi::ioctl::NV_ESC_RM_ALLOC, &mut a)?;
    let word = |at: usize| u32::from_le_bytes(a[at..at + 4].try_into().unwrap());
    Ok((word(28), word(8)))
}

impl RmPinner {
    /// A client and a device on GPU `minor` (device instance `device_id`).
    pub fn open(minor: u32, device_id: u32) -> Result<Self, String> {
        let ctl = open_rw("/dev/nvidiactl")?;
        let (st, client) = rm_alloc(&ctl, 0, 0, 0, NV01_ROOT_CLIENT, &mut [])
            .map_err(|e| format!("allocating an RM client: errno {e}"))?;
        if st != 0 || client == 0 {
            return Err(format!("allocating an RM client: status {st:#x}"));
        }
        // Freeing the client on any later failure: the ctl fd closing does it.
        let dev = open_rw(&format!("/dev/nvidia{minor}"))?;
        let mut reg = (ctl.as_raw_fd()).to_le_bytes();
        nv_ioctl(&dev, abi::ioctl::NV_ESC_REGISTER_FD, &mut reg)
            .map_err(|e| format!("registering /dev/nvidia{minor} with the client: errno {e}"))?;
        // NV0080_ALLOC_PARAMETERS: deviceId, hClientShare, ... vaMode (56 bytes).
        let mut p = [0u8; 56];
        p[0..4].copy_from_slice(&device_id.to_le_bytes());
        p[4..8].copy_from_slice(&client.to_le_bytes());
        let (st, _) = rm_alloc(&ctl, client, client, H_DEVICE, NV01_DEVICE_0, &mut p)
            .map_err(|e| format!("allocating device {device_id}: errno {e}"))?;
        if st != 0 {
            return Err(format!("allocating device {device_id}: status {st:#x}"));
        }
        Ok(Self {
            ctl,
            dev,
            client,
            next: H_FIRST_PIN,
        })
    }
}

impl Pinner for RmPinner {
    fn pin(&mut self, addr: u64, len: u64) -> Result<(), PinError> {
        // nv_ioctl_nvos02_parameters_with_fd: NVOS02 (hRoot, hObjectParent,
        // hObjectNew, hClass, flags, pMemory@24, limit@32, status@40), fd@48.
        let h = self.next;
        let mut a = [0u8; 56];
        a[0..4].copy_from_slice(&self.client.to_le_bytes());
        a[4..8].copy_from_slice(&H_DEVICE.to_le_bytes());
        a[8..12].copy_from_slice(&h.to_le_bytes());
        a[12..16].copy_from_slice(&NV01_MEMORY_SYSTEM_OS_DESCRIPTOR.to_le_bytes());
        a[16..20].copy_from_slice(&NVOS02_OSDESC_FLAGS.to_le_bytes());
        a[24..32].copy_from_slice(&addr.to_le_bytes());
        a[32..40].copy_from_slice(&(len - 1).to_le_bytes());
        a[48..52].copy_from_slice(&(-1i32).to_le_bytes());
        nv_ioctl(&self.dev, abi::ioctl::NV_ESC_RM_ALLOC_MEMORY, &mut a).map_err(PinError::Host)?;
        match u32::from_le_bytes(a[40..44].try_into().unwrap()) {
            0 => {
                self.next += 1;
                Ok(())
            }
            st => Err(PinError::Rm(st)),
        }
    }
}

impl Drop for RmPinner {
    fn drop(&mut self) {
        // NVOS00: hRoot, hObjectParent, hObjectOld, status. Freeing the client
        // frees the device and every descriptor, which unpins the pages; the
        // descriptors' own close would do the same.
        let mut a = [0u8; 16];
        a[0..4].copy_from_slice(&self.client.to_le_bytes());
        a[8..12].copy_from_slice(&self.client.to_le_bytes());
        let _ = nv_ioctl(&self.ctl, abi::ioctl::NV_ESC_RM_FREE, &mut a);
    }
}

/// `/proc/vmstat`'s `pgmigrate_success`: system-wide, so only a rough count of
/// what one round moved.
pub fn migrated_pages() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/vmstat").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("pgmigrate_success "))
        .and_then(|v| v.trim().parse().ok())
}

/// The backend's pin: opened on first use, then fed every memory table the
/// frontend sends, each on a thread of its own so a vhost-user message is not
/// held while gigabytes are pinned. Rounds run one after another.
pub struct Shared {
    state: std::sync::Mutex<State>,
}

enum State {
    Unopened,
    Open(GuestRamPin<RmPinner>),
    Unavailable,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(State::Unopened),
        }
    }
}

impl Shared {
    /// Pin `ranges` on a thread; returns at once.
    pub fn pin_async(self: &std::sync::Arc<Self>, ranges: Vec<FileRange>) {
        if ranges.is_empty() {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("guest-ram-pin".into())
            .spawn(move || me.pin(ranges));
        if let Err(e) = spawned {
            log::warn!("guest RAM pin: no thread for it ({e}); left unpinned");
        }
    }

    fn pin(&self, ranges: Vec<FileRange>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*state, State::Unopened) {
            *state = match RmPinner::open(0, 0) {
                Ok(rm) => State::Open(GuestRamPin::new(rm)),
                Err(e) => {
                    log::warn!(
                        "guest RAM pin: no RM client for it ({e}); guest RAM is not pinned, and \
                         a registration may have to migrate pages out of CMA pageblocks"
                    );
                    State::Unavailable
                }
            };
        }
        let State::Open(pin) = &mut *state else {
            return;
        };
        let asked: u64 = ranges.iter().map(|r| r.len).sum();
        let t = std::time::Instant::now();
        let before = migrated_pages();
        let st = pin.pin_ranges(ranges);
        let moved = migrated_pages()
            .zip(before)
            .map(|(a, b)| a.saturating_sub(b));
        let line = format!(
            "guest RAM pin: {} MiB asked, {} MiB newly pinned in {} RM pins ({} after a retry), \
             {} MiB already pinned, {} MiB refused, in {} ms; pages migrated system-wide meanwhile: {}; \
             {} MiB held in all",
            asked >> 20,
            st.pinned >> 20,
            st.pins,
            st.retried,
            st.already >> 20,
            st.refused >> 20,
            t.elapsed().as_millis(),
            moved.map_or("unknown".to_string(), |m| m.to_string()),
            pin.total().pinned >> 20,
        );
        if st.refused == 0 {
            log::info!("{line}");
        } else {
            log::warn!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A pinner that records what it was asked and refuses as told.
    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<(u64, u64)>>>,
        /// Refuse this many first attempts with NV_ERR_INVALID_ADDRESS.
        refuse_first: Arc<Mutex<usize>>,
        /// Refuse any pin covering this byte offset of the first mapping, always.
        stuck: Option<u64>,
        base: Arc<Mutex<Option<u64>>>,
    }

    impl Pinner for Fake {
        fn pin(&mut self, addr: u64, len: u64) -> Result<(), PinError> {
            self.calls.lock().unwrap().push((addr, len));
            let base = *self.base.lock().unwrap().get_or_insert(addr);
            if let Some(s) = self.stuck
                && addr <= base + s
                && base + s < addr + len
            {
                return Err(PinError::Rm(NV_ERR_INVALID_ADDRESS));
            }
            let mut r = self.refuse_first.lock().unwrap();
            if *r > 0 {
                *r -= 1;
                return Err(PinError::Rm(NV_ERR_INVALID_ADDRESS));
            }
            Ok(())
        }
    }

    fn memfd(len: u64) -> OwnedFd {
        // SAFETY: plain syscalls on a fresh memfd.
        unsafe {
            let fd = libc::memfd_create(c"pin-test".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0);
            OwnedFd::from_raw_fd(fd)
        }
    }

    fn range(fd: &OwnedFd, offset: u64, len: u64) -> FileRange {
        FileRange {
            fd: fd.try_clone().unwrap(),
            offset,
            len,
        }
    }

    #[test]
    fn the_switch_defaults_to_the_personality() {
        assert!(enabled(None, true));
        assert!(!enabled(None, false));
        assert!(enabled(Some("1"), false));
        assert!(!enabled(Some("0"), true));
        assert!(enabled(Some("bogus"), true));
    }

    /// Guest RAM is pinned in chunks, all of it, once.
    #[test]
    fn every_region_is_pinned_in_chunks() {
        let fake = Fake::default();
        let mut p = GuestRamPin::new(fake.clone()).without_sleeping();
        let fd = memfd(CHUNK * 2 + PIECE);
        let st = p.pin_ranges(vec![range(&fd, 0, CHUNK * 2 + PIECE)]);
        assert_eq!(st.pinned, CHUNK * 2 + PIECE);
        assert_eq!(st.refused, 0);
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls.iter().map(|c| c.1).collect::<Vec<_>>(),
            vec![CHUNK, CHUNK, PIECE]
        );
        assert_eq!(calls[1].0, calls[0].0 + CHUNK, "chunks are consecutive");
    }

    /// A memory table sent again names the same file ranges: nothing new is
    /// pinned, and only a part not pinned before is.
    #[test]
    fn a_range_already_pinned_is_not_pinned_again() {
        let fake = Fake::default();
        let mut p = GuestRamPin::new(fake.clone()).without_sleeping();
        let fd = memfd(8 * PIECE);
        p.pin_ranges(vec![range(&fd, 0, 4 * PIECE)]);
        let st = p.pin_ranges(vec![range(&fd, 2 * PIECE, 4 * PIECE)]);
        assert_eq!(st.pinned, 2 * PIECE, "only [4, 6) pieces were new");
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1, 2 * PIECE);
        let st = p.pin_ranges(vec![range(&fd, 0, 6 * PIECE)]);
        assert_eq!(st.pinned, 0);
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
    }

    /// A refused pin is asked again; one that held on a later attempt counts.
    #[test]
    fn a_refused_pin_is_retried() {
        let fake = Fake::default();
        *fake.refuse_first.lock().unwrap() = 2;
        let mut p = GuestRamPin::new(fake.clone()).without_sleeping();
        let fd = memfd(PIECE);
        let st = p.pin_ranges(vec![range(&fd, 0, PIECE)]);
        assert_eq!(st.pinned, PIECE);
        assert_eq!(st.retried, 1);
        assert_eq!(fake.calls.lock().unwrap().len(), 3);
    }

    /// A chunk with a page that never pins is split, and only the piece
    /// holding that page is left unpinned.
    #[test]
    fn a_chunk_that_will_not_pin_is_split_into_pieces() {
        let fake = Fake {
            stuck: Some(3 * PIECE + 5 * PAGE),
            ..Default::default()
        };
        let mut p = GuestRamPin::new(fake.clone()).without_sleeping();
        let fd = memfd(8 * PIECE);
        let st = p.pin_ranges(vec![range(&fd, 0, 8 * PIECE)]);
        assert_eq!(st.refused, PIECE);
        assert_eq!(st.pinned, 7 * PIECE);
        let calls = fake.calls.lock().unwrap().clone();
        // The chunk (1 + 4 retries), then 8 pieces, the stuck one 1 + 4 times.
        assert_eq!(calls.len(), 5 + 7 + 5);
    }

    /// Against the host's own RM: pins a memfd the way the backend pins guest
    /// RAM. Needs an NVIDIA GPU; run with `--ignored`.
    #[test]
    #[ignore]
    fn pins_through_the_host_rm() {
        let rm = RmPinner::open(0, 0).expect("an RM client on GPU 0");
        let mut p = GuestRamPin::new(rm);
        let fd = memfd(CHUNK + PIECE);
        // Written first, as guest RAM is: movable pages, some of which the
        // allocator may place in CMA pageblocks, so the pin has to move them.
        let m = map(&fd, 0, CHUNK + PIECE).unwrap();
        // SAFETY: inside the mapping just made.
        unsafe { std::ptr::write_bytes(m.addr as *mut u8, 0x5a, m.len) };
        drop(m);
        let before = migrated_pages();
        let st = p.pin_ranges(vec![range(&fd, 0, CHUNK + PIECE)]);
        eprintln!(
            "{st:?}, pgmigrate_success +{:?}",
            migrated_pages().zip(before).map(|(a, b)| a - b)
        );
        assert_eq!(st.refused, 0);
        assert_eq!(st.pinned, CHUNK + PIECE);
    }
}

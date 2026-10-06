//! The policy for the RM window (shared-memory region 1): who may map how many bytes of it.
//! The pure half; the I/O half is `kmd_render/src/virtio/nvrm_window.rs` and the table doors
//! in `virtio/gpu/nvrm_tables.rs`. Documented in `docs/nvrm-escape.md`, "The RM window
//! policy".
//!
//! # What this accounts, and what it does not
//!
//! User-mode NVK maps RM memory with `HELIOS_ESCAPE_NVRM` `MMAP`; the HOST places each
//! mapping in region 1 (its own extent allocator, `host/backend/device/src/shm.rs`) and the
//! reply names where. The KMD never chooses an address in the window and holds no free list
//! of it, so what it can count is BYTES: the sum of the sizes of the live window mappings.
//! That is an approximation of the window's real occupancy (the host also holds extents for
//! mappings armed but not yet `MMAP`ed, and splits the window into caching zones), which is
//! why a host refusal ("no room") is a separate, counted reason: the host stays the judge of
//! placement, this policy decides how the bytes are SHARED.
//!
//! # The policy (`Policy::Dynamic`)
//!
//! * No fixed per-process share. Any device may map until the window is full.
//! * A reserve (default 256 MiB, `NvWinReserveMb`, never more than a quarter of the cap) at the
//!   top of the window is held back for the privileged device (DWM's): a non-privileged map
//!   is refused once it would take the in-use total past `cap - reserve`; a privileged one
//!   may use everything up to `cap`.
//! * `cap` is the window size, or `NvWinMaxMb` when that is set and smaller (an operator
//!   bound on the non-paged pool the MDLs cost: 2 KiB per MiB mapped).
//! * A refusal is counted by reason and answered `NO_RESOURCES` (what the UMD already
//!   understands: NVK falls back to system memory).
//!
//! `Policy::Legacy` is the old rule, byte for byte: one device may hold at most a quarter of
//! the window; nothing else is checked and there is no reserve.
//!
//! # Reclaim (designed, deliberately not implemented)
//!
//! A mapping the process holds has a live user-mode virtual address. Taking it back would
//! have to `MmUnmapLockedPages` in the owning process (only legal in that process's
//! context, at PASSIVE) while user mode may be reading or writing through it, and a later
//! access would fault the process instead of failing a call. No recorded mapping is idle in
//! a way the KMD can prove, so nothing here evicts. See the document for the design that
//! would be safe (a cooperative "release hint" the UMD acts on).
//!
//! Everything is `u64` bytes; no figure is narrowed.

extern crate alloc;
use alloc::vec::Vec;

/// The page size every mapping is a multiple of.
pub const PAGE: u64 = 4096;
/// The default reserve held back for the privileged device, in MiB.
pub const DEFAULT_RESERVE_MIB: u32 = 256;
/// How many owners [`Account::top`] reports.
pub const TOP_N: usize = 4;

/// Which rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// A quarter of the window per device, nothing else (the behaviour before this policy).
    Legacy,
    /// Share until full, minus a reserve for the privileged device.
    Dynamic,
}

impl Policy {
    /// The `NvWinPolicy` knob: 0 selects the legacy quota, anything else the dynamic policy.
    pub fn from_knob(value: u32) -> Self {
        if value == 0 {
            Policy::Legacy
        } else {
            Policy::Dynamic
        }
    }

    pub fn as_u32(self) -> u32 {
        match self {
            Policy::Legacy => 0,
            Policy::Dynamic => 1,
        }
    }
}

/// Why a map was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The window (up to `cap`) has not that much room left. Counted in `NvWinRFull`.
    WindowFull,
    /// A non-privileged map would eat into the reserve. `NvWinRRes`.
    ReserveHit,
    /// The one map is larger than the window could ever give it (an empty window would
    /// still refuse). `NvWinRBig`.
    TooBig,
    /// The bookkeeping is full: no owner row to charge. `NvWinRTab`.
    TableFull,
    /// Legacy policy only: this device is at its quarter of the window. Counted in the
    /// refusal total (`NvMapQRef`) only.
    Quota,
}

/// The refusals, by reason, and the failures that are not decisions of this policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub window_full: u32,
    pub reserve_hit: u32,
    pub too_big: u32,
    pub table_full: u32,
    pub quota: u32,
    /// Address space exhausted: the user view could not be made (MDL allocation, or
    /// `MmMapLockedPagesSpecifyCache`). Reported by the caller ([`Account::note_addr_space`]).
    pub addr_space: u32,
    /// The host refused the `Mmap` (any errno). [`Account::note_host_refused`].
    pub host_refused: u32,
    /// The errno of the last host refusal (12, ENOMEM, is "the host's window zone is full").
    pub last_host_errno: u32,
}

impl Stats {
    /// Every refusal and failure, all reasons: `NvMapQRef`.
    pub fn total(&self) -> u32 {
        self.window_full
            .saturating_add(self.reserve_hit)
            .saturating_add(self.too_big)
            .saturating_add(self.table_full)
            .saturating_add(self.quota)
            .saturating_add(self.addr_space)
            .saturating_add(self.host_refused)
    }
}

/// The sizes the policy works from. All derived once, from the window the device reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The window as the device reported it (bytes; 0 = no window).
    pub window: u64,
    /// What may be mapped at most: the window, or the operator's smaller bound, page-rounded
    /// down.
    pub cap: u64,
    /// Held back for the privileged device (never more than `cap`).
    pub reserve: u64,
    pub policy: Policy,
}

impl Config {
    /// `window_bytes`: the window's length. `reserve_mib`: `NvWinReserveMb`, clamped to a
    /// quarter of the cap (so an ordinary device always has at least three quarters).
    /// `max_mib`: `NvWinMaxMb` (0 = no bound beyond the window). Any u32 MiB value is valid:
    /// the products are u64 (4 PiB at most).
    pub fn new(window_bytes: u64, reserve_mib: u32, max_mib: u32, policy: Policy) -> Self {
        let mut cap = window_bytes & !(PAGE - 1);
        if max_mib != 0 {
            cap = cap.min((u64::from(max_mib) << 20) & !(PAGE - 1));
        }
        // At most a quarter of the cap: a reserve as large as a small window (a 256 MiB BAR1
        // without ReBAR, or `NvWinMaxMb` at or below the reserve) would leave an ordinary
        // device nothing at all, which is worse than the legacy window/4 it replaces.
        let reserve = (u64::from(reserve_mib) << 20).min((cap / 4) & !(PAGE - 1));
        Config {
            window: window_bytes,
            cap,
            reserve,
            policy,
        }
    }

    /// The most a non-privileged device may bring the in-use total to (dynamic policy).
    pub fn ordinary_limit(&self) -> u64 {
        self.cap - self.reserve
    }

    /// The legacy per-device quota: a quarter of the window as reported.
    pub fn legacy_quota(&self) -> u64 {
        self.window / 4
    }
}

/// One owner's row. Rows exist while an owner holds window mappings, or is privileged.
#[derive(Debug, Clone, Copy)]
struct Row {
    owner: u64,
    pid: u32,
    bytes: u64,
    maps: u32,
    privileged: bool,
}

/// One entry of [`Account::top`]. `bytes == 0` marks an unused rank.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Top {
    pub owner: u64,
    pub pid: u32,
    pub bytes: u64,
}

/// `Info.flags` bits: the values of `helios_protocol::HELIOS_NVRM_WINDOW_FLAG_*` (the render
/// crate asserts they agree; this crate has no protocol dependency).
pub const INFO_OWNER_LIMIT: u32 = 1 << 0;
pub const INFO_CAN_GROW: u32 = 1 << 1;
pub const INFO_SHARED_CEILING: u32 = 1 << 2;

/// What `WINDOW_INFO` reports for one device ([`Account::info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub window_bytes: u64,
    pub used_bytes: u64,
    pub owner_limit_bytes: u64,
    pub owner_used_bytes: u64,
    pub flags: u32,
}

/// Everything the counters publish, copied out in one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub config: Config,
    pub in_use: u64,
    pub peak: u64,
    /// Bytes in use past `cap - reserve`: how deep the privileged device is into the reserve.
    pub reserve_use: u64,
    pub maps: u64,
    pub privileged_owners: u32,
    pub owners: u32,
    pub stats: Stats,
    pub top: [Top; TOP_N],
}

/// The accounting. Never allocates after [`Account::new`].
pub struct Account {
    cfg: Config,
    in_use: u64,
    peak: u64,
    maps: u64,
    rows: Vec<Row>,
    stats: Stats,
}

impl Account {
    /// An empty account with room for `owner_rows` distinct owners (and privileged marks).
    /// `None` when the allocator refuses. PASSIVE: this allocates.
    pub fn new(cfg: Config, owner_rows: usize) -> Option<Self> {
        let mut rows = Vec::new();
        rows.try_reserve_exact(owner_rows).ok()?;
        Some(Account {
            cfg,
            in_use: 0,
            peak: 0,
            maps: 0,
            rows,
            stats: Stats::default(),
        })
    }

    pub fn config(&self) -> Config {
        self.cfg
    }

    pub fn in_use(&self) -> u64 {
        self.in_use
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// How many owners the account can hold at once.
    pub fn row_capacity(&self) -> usize {
        self.rows.capacity()
    }

    fn row_index(&self, owner: u64) -> Option<usize> {
        self.rows.iter().position(|r| r.owner == owner)
    }

    /// Whether `owner` was marked privileged earlier (and has not been forgotten).
    pub fn is_marked_privileged(&self, owner: u64) -> bool {
        self.rows.iter().any(|r| r.owner == owner && r.privileged)
    }

    /// Bytes `owner` has mapped now.
    pub fn bytes_of(&self, owner: u64) -> u64 {
        self.rows
            .iter()
            .find(|r| r.owner == owner)
            .map_or(0, |r| r.bytes)
    }

    /// The decision, without side effects: may `owner` map `size` more bytes?
    /// `live_privileged`: the caller's own evidence that `owner` is the privileged device
    /// right now (it holds the scanout, or is the KMD's own client); the sticky mark
    /// ([`Self::mark_privileged`]) counts too.
    ///
    /// `size == 0` is allowed (nothing to place). A size that is not a page multiple is the
    /// caller's `BadRange`, not a decision here, and is rounded up for the arithmetic.
    pub fn check(&self, owner: u64, live_privileged: bool, size: u64) -> Result<(), Refusal> {
        if size == 0 {
            return Ok(());
        }
        let size = match size.checked_add(PAGE - 1) {
            Some(s) => s & !(PAGE - 1),
            // No window is that big; the old rule would have said "over quota".
            None => {
                return Err(match self.cfg.policy {
                    Policy::Legacy => Refusal::Quota,
                    Policy::Dynamic => Refusal::TooBig,
                })
            }
        };
        match self.cfg.policy {
            Policy::Legacy => {
                let mine = self.bytes_of(owner);
                match mine.checked_add(size) {
                    Some(t) if t <= self.cfg.legacy_quota() => Ok(()),
                    _ => Err(Refusal::Quota),
                }
            }
            Policy::Dynamic => {
                let privileged = live_privileged || self.is_marked_privileged(owner);
                let limit = if privileged {
                    self.cfg.cap
                } else {
                    self.cfg.ordinary_limit()
                };
                if size > limit {
                    // An empty window would refuse it too.
                    return Err(Refusal::TooBig);
                }
                match self.in_use.checked_add(size) {
                    Some(t) if t <= limit => Ok(()),
                    Some(t) if !privileged && t <= self.cfg.cap => Err(Refusal::ReserveHit),
                    _ => Err(Refusal::WindowFull),
                }
            }
        }
    }

    /// Count a refusal.
    pub fn refuse(&mut self, why: Refusal) {
        let s = &mut self.stats;
        let c = match why {
            Refusal::WindowFull => &mut s.window_full,
            Refusal::ReserveHit => &mut s.reserve_hit,
            Refusal::TooBig => &mut s.too_big,
            Refusal::TableFull => &mut s.table_full,
            Refusal::Quota => &mut s.quota,
        };
        *c = c.saturating_add(1);
    }

    /// [`Self::check`], counting a refusal. The pre-check before the host is asked.
    pub fn admit(&mut self, owner: u64, live_privileged: bool, size: u64) -> Result<(), Refusal> {
        let r = self.check(owner, live_privileged, size);
        if let Err(why) = r {
            self.refuse(why);
        }
        r
    }

    /// The host refused the `Mmap` with `errno` (positive; 0 when it is not known).
    pub fn note_host_refused(&mut self, errno: u32) {
        self.stats.host_refused = self.stats.host_refused.saturating_add(1);
        self.stats.last_host_errno = errno;
    }

    /// The user view could not be made after the host mapped (address space, MDL).
    pub fn note_addr_space(&mut self) {
        self.stats.addr_space = self.stats.addr_space.saturating_add(1);
    }

    /// Charge a map the host has placed: re-checks (the pre-check ran before a host round
    /// trip, other maps may have landed since), finds or makes the owner's row (`pid` is
    /// kept for the report) and adds the bytes. A refusal here is counted and the caller
    /// must undo the host mapping.
    pub fn charge(
        &mut self,
        owner: u64,
        pid: u32,
        live_privileged: bool,
        size: u64,
    ) -> Result<(), Refusal> {
        if size == 0 {
            return Ok(());
        }
        if let Err(why) = self.check(owner, live_privileged, size) {
            self.refuse(why);
            return Err(why);
        }
        // The check rounded `size` up the same way; so does the charge.
        let size = (size + (PAGE - 1)) & !(PAGE - 1);
        let idx = match self.row_index(owner) {
            Some(i) => i,
            None => {
                if self.rows.len() >= self.rows.capacity() {
                    self.refuse(Refusal::TableFull);
                    return Err(Refusal::TableFull);
                }
                self.rows.push(Row {
                    owner,
                    pid,
                    bytes: 0,
                    maps: 0,
                    privileged: false,
                });
                self.rows.len() - 1
            }
        };
        let row = &mut self.rows[idx];
        if row.pid == 0 {
            row.pid = pid;
        }
        row.bytes = row.bytes.saturating_add(size);
        row.maps = row.maps.saturating_add(1);
        self.in_use = self.in_use.saturating_add(size);
        self.maps = self.maps.saturating_add(1);
        if self.in_use > self.peak {
            self.peak = self.in_use;
        }
        Ok(())
    }

    /// One map of `size` bytes of `owner`'s went away. The same rounding as the charge; a
    /// release larger than what is charged clamps at zero instead of wrapping.
    pub fn release(&mut self, owner: u64, size: u64) {
        if size == 0 {
            return;
        }
        let size = size.saturating_add(PAGE - 1) & !(PAGE - 1);
        let Some(idx) = self.row_index(owner) else {
            return;
        };
        let row = &mut self.rows[idx];
        let freed = size.min(row.bytes);
        row.bytes -= freed;
        row.maps = row.maps.saturating_sub(1);
        self.in_use = self.in_use.saturating_sub(freed);
        self.maps = self.maps.saturating_sub(1);
        if row.maps == 0 && !row.privileged {
            self.rows.swap_remove(idx);
        }
    }

    /// Everything `owner` holds is gone (device retired): free it all, forget its sticky
    /// mark. Returns the bytes freed.
    pub fn forget_owner(&mut self, owner: u64) -> u64 {
        let Some(idx) = self.row_index(owner) else {
            return 0;
        };
        let row = self.rows.swap_remove(idx);
        self.in_use = self.in_use.saturating_sub(row.bytes);
        self.maps = self.maps.saturating_sub(u64::from(row.maps));
        row.bytes
    }

    /// Mark `owner` the privileged device (it set a scanout source). It keeps the mark until
    /// [`Self::forget_owner`]. `false`: no row could be made (counted as `table_full`).
    pub fn mark_privileged(&mut self, owner: u64, pid: u32) -> bool {
        if let Some(i) = self.row_index(owner) {
            self.rows[i].privileged = true;
            return true;
        }
        if self.rows.len() >= self.rows.capacity() {
            self.refuse(Refusal::TableFull);
            return false;
        }
        self.rows.push(Row {
            owner,
            pid,
            bytes: 0,
            maps: 0,
            privileged: true,
        });
        true
    }

    /// The transport is gone: nothing is mapped, nobody is privileged. Statistics stay.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.in_use = 0;
        self.maps = 0;
    }

    /// The `TOP_N` owners by mapped bytes, largest first (ties: lower owner token first).
    /// O(rows x TOP_N), no allocation.
    pub fn top(&self) -> [Top; TOP_N] {
        let mut out = [Top::default(); TOP_N];
        for r in self.rows.iter().filter(|r| r.bytes != 0) {
            let cand = Top {
                owner: r.owner,
                pid: r.pid,
                bytes: r.bytes,
            };
            let mut at = TOP_N;
            for (i, t) in out.iter().enumerate() {
                if t.bytes == 0 || cand.bytes > t.bytes || (cand.bytes == t.bytes && cand.owner < t.owner)
                {
                    at = i;
                    break;
                }
            }
            if at < TOP_N {
                let mut i = TOP_N - 1;
                while i > at {
                    out[i] = out[i - 1];
                    i -= 1;
                }
                out[at] = cand;
            }
        }
        out
    }

    /// What `WINDOW_INFO` reports to `owner` (see `helios_protocol::HeliosNvrmWindowInfo`):
    /// the window, what is mapped, the ceiling that applies to this device and what it holds.
    /// `live_privileged` as in [`Self::check`]. No side effect.
    ///
    /// Dynamic policy: the ceiling is on the all-owners total (`INFO_SHARED_CEILING`): `cap`
    /// for the privileged device, `cap - reserve` for everyone else, so the room left for a
    /// device is `owner_limit - used`. Legacy: a quarter of the window, a ceiling on the
    /// device's own bytes (flag clear): the room is `owner_limit - owner_used`.
    pub fn info(&self, owner: u64, live_privileged: bool) -> Info {
        let window = self.cfg.window;
        let (limit, mut flags) = match self.cfg.policy {
            Policy::Legacy => (self.cfg.legacy_quota(), 0),
            Policy::Dynamic => {
                let privileged = live_privileged || self.is_marked_privileged(owner);
                let limit = if privileged {
                    self.cfg.cap
                } else {
                    self.cfg.ordinary_limit()
                };
                (limit, INFO_SHARED_CEILING)
            }
        };
        if limit < window {
            flags |= INFO_OWNER_LIMIT;
        }
        Info {
            window_bytes: window,
            used_bytes: self.in_use,
            owner_limit_bytes: limit,
            owner_used_bytes: self.bytes_of(owner),
            flags,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let ordinary = self.cfg.ordinary_limit();
        Snapshot {
            config: self.cfg,
            in_use: self.in_use,
            peak: self.peak,
            reserve_use: self.in_use.saturating_sub(ordinary),
            maps: self.maps,
            privileged_owners: self.rows.iter().filter(|r| r.privileged).count() as u32,
            owners: self.rows.len() as u32,
            stats: self.stats,
            top: self.top(),
        }
    }
}

/// The registry counters `kmd_render/src/virtio/nvrm_window.rs` publishes (each is a
/// `b"..."` literal there, and only there). Listed here so a host test can hold them to
/// `record_named_bytes`' 14-character limit and to uniqueness across both crates.
pub const COUNTERS: &[&str] = &[
    "NvWinPol",
    "NvWinCapMb",
    "NvWinResMb",
    "NvWinUseMb",
    "NvWinPeakMb",
    "NvWinFreeMb",
    "NvWinRsvUse",
    "NvWinMaps",
    "NvWinOwn",
    "NvWinPriv",
    "NvWinRFull",
    "NvWinRRes",
    "NvWinRBig",
    "NvWinRTab",
    "NvWinRAddr",
    "NvWinRHost",
    "NvWinHErrno",
    "NvWinT1Pid",
    "NvWinT1Mb",
    "NvWinT2Pid",
    "NvWinT2Mb",
    "NvWinT3Pid",
    "NvWinT3Mb",
    "NvWinT4Pid",
    "NvWinT4Mb",
    "NvHdlLive",
    "NvHdlPeak",
    "NvHdlCap",
    "NvHdlGrow",
    "NvHdlORef",
    "NvHdlGRef",
    "NvHdlFRef",
    "NvMapTCap",
    "NvMapTGrow",
    "NvMapTRef",
    "NvTblOom",
    "NvPinQRef",
    "NvWinInfo",
    "NvSanityRef",
];

/// Pack a [`Top`] for one atomic: pid in the high half, MiB (saturating) in the low half.
/// An unused rank is 0.
pub fn pack_top(t: Top) -> u64 {
    if t.bytes == 0 {
        return 0;
    }
    let mib = (t.bytes >> 20).min(u64::from(u32::MAX));
    (u64::from(t.pid) << 32) | mib
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn dynamic(window: u64, reserve_mib: u32) -> Account {
        Account::new(Config::new(window, reserve_mib, 0, Policy::Dynamic), 64).unwrap()
    }

    fn legacy(window: u64) -> Account {
        Account::new(Config::new(window, 256, 0, Policy::Legacy), 64).unwrap()
    }

    extern crate std;

    /// Every `b"..."` literal under `root`: (literal, file).
    fn byte_literals(root: &std::path::Path) -> std::vec::Vec<(std::string::String, std::path::PathBuf)> {
        let mut out = std::vec::Vec::new();
        let mut stack = std::vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    let mut rest = text.as_str();
                    while let Some(i) = rest.find("b\"") {
                        let before = rest[..i].chars().last();
                        let tail = &rest[i + 2..];
                        let Some(end) = tail.find('"') else { break };
                        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            out.push((tail[..end].into(), p.clone()));
                        }
                        rest = &tail[end + 1..];
                    }
                }
            }
        }
        out
    }

    #[test]
    fn counter_names_fit_are_unique_and_live_only_in_the_publisher() {
        let mut names: std::vec::Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        // The sibling trees, when present (a copy of this crate without them scans nothing).
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut scanned = false;
        for tree in ["../kmd_render/src", "src"] {
            let root = manifest.join(tree);
            if !root.exists() {
                continue;
            }
            scanned = true;
            for (lit, file) in byte_literals(&root) {
                if COUNTERS.contains(&lit.as_str()) {
                    assert!(
                        file.file_name().is_some_and(|n| n == "nvrm_window.rs"),
                        "counter {lit} is also written in {}",
                        file.display()
                    );
                }
            }
        }
        let _ = scanned;
        // Every one of them is written exactly once where the render crate is present.
        let render = manifest.join("../kmd_render/src/virtio/nvrm_window.rs");
        if render.exists() {
            let all = byte_literals(render.parent().unwrap());
            for name in COUNTERS {
                let n = all
                    .iter()
                    .filter(|(l, f)| l == name && f.file_name().is_some_and(|n| n == "nvrm_window.rs"))
                    .count();
                assert_eq!(n, 1, "{name} must be published exactly once");
            }
        }
    }

    #[test]
    fn config_derives_cap_and_reserve_in_u64() {
        let c = Config::new(32 * GIB, 256, 0, Policy::Dynamic);
        assert_eq!(c.cap, 32 * GIB);
        assert_eq!(c.reserve, 256 * MIB);
        assert_eq!(c.ordinary_limit(), 32 * GIB - 256 * MIB);
        // 64 GiB is past u32 in bytes and in KiB; everything here is u64.
        let c = Config::new(64 * GIB, 256, 0, Policy::Dynamic);
        assert_eq!(c.cap, 64 * GIB);
        // The largest representable knob values.
        let c = Config::new(u64::MAX, u32::MAX, u32::MAX, Policy::Dynamic);
        assert_eq!(c.cap, (u64::from(u32::MAX) << 20) & !(PAGE - 1));
        // The reserve is clamped to a quarter of the cap, however large the knob.
        assert_eq!(c.reserve, c.cap / 4);
        assert_eq!(c.ordinary_limit(), c.cap - c.cap / 4);
    }

    #[test]
    fn config_max_bound_only_lowers_the_cap() {
        let c = Config::new(32 * GIB, 256, 8 * 1024, Policy::Dynamic);
        assert_eq!(c.cap, 8 * GIB);
        let c = Config::new(4 * GIB, 256, 64 * 1024, Policy::Dynamic);
        assert_eq!(c.cap, 4 * GIB);
        // A window that is not a page multiple rounds down.
        let c = Config::new(10 * MIB + 5, 0, 0, Policy::Dynamic);
        assert_eq!(c.cap, 10 * MIB);
    }

    #[test]
    fn reserve_never_exceeds_a_quarter_of_the_cap() {
        // A 128 MiB window with the default 256 MiB reserve: 32 MiB reserved, 96 MiB ordinary.
        let c = Config::new(128 * MIB, 256, 0, Policy::Dynamic);
        assert_eq!(c.reserve, 32 * MIB);
        assert_eq!(c.ordinary_limit(), 96 * MIB);
        let a = Account::new(c, 4).unwrap();
        assert_eq!(a.check(1, false, 96 * MIB), Ok(()));
        assert_eq!(a.check(1, false, 96 * MIB + 4096), Err(Refusal::TooBig));
        assert_eq!(a.check(1, true, 128 * MIB), Ok(()));
    }

    /// Small windows (a BAR1 without ReBAR) and operator bounds at or below the reserve: an
    /// ordinary device is never locked out, and never gets less than the legacy window/4.
    #[test]
    fn small_windows_table() {
        // (window MiB, NvWinMaxMb, reserve knob MiB) -> (cap MiB, reserve MiB)
        let rows: &[(u64, u32, u32, u64, u64)] = &[
            (256, 0, 256, 256, 64),      // BAR1 256 MiB, default reserve
            (256, 256, 256, 256, 64),    // bound equal to the window
            (512, 0, 256, 512, 128),
            (1024, 0, 256, 1024, 256),   // the reserve fits exactly at a quarter
            (4096, 0, 256, 4096, 256),   // 4 GiB: the knob applies
            (32768, 0, 256, 32768, 256),
            (32768, 128, 256, 128, 32),  // NvWinMaxMb below the reserve
            (32768, 256, 256, 256, 64),  // NvWinMaxMb == reserve
            (256, 0, 0, 256, 0),         // reserve knob 0
            (4, 0, 256, 4, 1),
            (0, 0, 256, 0, 0),           // no window
        ];
        for &(win, max, knob, cap, reserve) in rows {
            let c = Config::new(win * MIB, knob, max, Policy::Dynamic);
            assert_eq!((c.cap, c.reserve), (cap * MIB, reserve * MIB), "{win} {max} {knob}");
            assert!(c.ordinary_limit() >= c.cap - c.cap / 4 || c.cap == 0, "{win} {max} {knob}");
            assert!(
                c.ordinary_limit() >= c.legacy_quota().min(c.cap),
                "never below the legacy quota: {win} {max} {knob}"
            );
            if cap > 0 {
                let a = Account::new(c, 4).unwrap();
                // A first ordinary map of a quarter of the window is accepted (crm_smoke, NVK
                // before DWM's first SCANOUT_SET).
                assert_eq!(a.check(1, false, cap * MIB / 4), Ok(()), "{win} {max} {knob}");
                assert_eq!(a.check(1, false, 4096), Ok(()));
            }
        }
    }

    #[test]
    fn policy_knob() {
        assert_eq!(Policy::from_knob(0), Policy::Legacy);
        assert_eq!(Policy::from_knob(1), Policy::Dynamic);
        assert_eq!(Policy::from_knob(77), Policy::Dynamic);
        assert_eq!(Policy::Legacy.as_u32(), 0);
        assert_eq!(Policy::Dynamic.as_u32(), 1);
    }

    #[test]
    fn dynamic_shares_the_window_without_a_per_owner_share() {
        let mut a = dynamic(4 * GIB, 256);
        // One owner takes far more than a quarter.
        for _ in 0..29 {
            a.charge(1, 10, false, 128 * MIB).unwrap();
        }
        assert_eq!(a.bytes_of(1), 29 * 128 * MIB);
        assert!(a.bytes_of(1) > 4 * GIB / 4);
        // Another owner still fits.
        a.charge(2, 20, false, 64 * MIB).unwrap();
    }

    #[test]
    fn reserve_boundary_is_exact() {
        let mut a = dynamic(GIB, 256); // ordinary limit 768 MiB
        a.charge(1, 1, false, 768 * MIB - 4096).unwrap();
        // One page more than the limit: reserve hit, not window full.
        assert_eq!(a.check(1, false, 8192), Err(Refusal::ReserveHit));
        // Exactly the limit is fine.
        a.charge(1, 1, false, 4096).unwrap();
        assert_eq!(a.in_use(), 768 * MIB);
        assert_eq!(a.check(2, false, 4096), Err(Refusal::ReserveHit));
        // The privileged device uses the reserve, up to the cap.
        assert_eq!(a.check(3, true, 256 * MIB), Ok(()));
        assert_eq!(a.check(3, true, 256 * MIB + 4096), Err(Refusal::WindowFull));
        a.charge(3, 3, true, 256 * MIB).unwrap();
        assert_eq!(a.in_use(), GIB);
        assert_eq!(a.snapshot().reserve_use, 256 * MIB);
        // Full for everyone now.
        assert_eq!(a.check(3, true, 4096), Err(Refusal::WindowFull));
        // Not even a page for an ordinary device, and since it is past the cap the reason
        // is "full", not "reserve".
        assert_eq!(a.check(1, false, 4096), Err(Refusal::WindowFull));
    }

    #[test]
    fn non_privileged_past_cap_is_window_full_not_reserve() {
        let mut a = dynamic(GIB, 256);
        a.charge(1, 1, true, GIB - 4096).unwrap();
        // 8 KiB more: past the cap altogether.
        assert_eq!(a.check(2, false, 8192), Err(Refusal::WindowFull));
        // One page: inside the cap, but it is the reserve's.
        assert_eq!(a.check(2, false, 4096), Err(Refusal::ReserveHit));
    }

    #[test]
    fn too_big_is_a_map_that_could_never_fit() {
        let a = dynamic(GIB, 256);
        assert_eq!(a.check(1, false, 768 * MIB + 4096), Err(Refusal::TooBig));
        assert_eq!(a.check(1, false, 768 * MIB), Ok(()));
        assert_eq!(a.check(1, true, GIB + 4096), Err(Refusal::TooBig));
        assert_eq!(a.check(1, true, GIB), Ok(()));
    }

    #[test]
    fn hostile_sizes_never_overflow() {
        let mut a = dynamic(32 * GIB, 256);
        for size in [u64::MAX, u64::MAX - 4095, u64::MAX - 1, 1 << 63, (1 << 63) + 1] {
            assert_eq!(a.check(1, false, size), Err(Refusal::TooBig), "{size:#x}");
            assert_eq!(a.charge(1, 1, true, size), Err(Refusal::TooBig), "{size:#x}");
        }
        assert_eq!(a.in_use(), 0);
        // A huge release does not wrap.
        a.charge(1, 1, false, 4096).unwrap();
        a.release(1, u64::MAX);
        assert_eq!(a.in_use(), 0);
        // Legacy too.
        let l = legacy(32 * GIB);
        assert_eq!(l.check(1, false, u64::MAX), Err(Refusal::Quota));
        assert_eq!(l.check(1, false, u64::MAX / 2), Err(Refusal::Quota));
    }

    #[test]
    fn in_use_near_u64_max_cannot_wrap_the_check() {
        // A 4 PiB window (u32::MAX MiB) is as large as the knobs can say.
        let mut a = Account::new(
            Config::new(u64::from(u32::MAX) << 20, 0, 0, Policy::Dynamic),
            4,
        )
        .unwrap();
        a.charge(1, 1, false, 1 << 50).unwrap();
        a.charge(1, 1, false, 1 << 50).unwrap();
        assert_eq!(a.in_use(), 1 << 51);
        assert_eq!(a.bytes_of(1), 1 << 51);
    }

    #[test]
    fn sixty_four_gib_window_many_maps() {
        let mut a = dynamic(64 * GIB, 256);
        // 2 MiB maps until the ordinary limit: 32 640 of them.
        let mut n = 0u64;
        while a.check(1, false, 2 * MIB).is_ok() {
            a.charge(1, 1, false, 2 * MIB).unwrap();
            n += 1;
        }
        assert_eq!(n, (64 * GIB - 256 * MIB) / (2 * MIB));
        assert_eq!(a.check(2, false, 4096), Err(Refusal::ReserveHit));
        assert_eq!(a.snapshot().maps, n);
        assert_eq!(a.snapshot().peak, 64 * GIB - 256 * MIB);
        // The window's gigabytes in MiB fit the u32 the counters publish.
        assert!((a.snapshot().in_use >> 20) < u64::from(u32::MAX));
    }

    #[test]
    fn release_and_peak() {
        let mut a = dynamic(GIB, 0);
        a.charge(1, 1, false, 100 * MIB).unwrap();
        a.charge(2, 2, false, 200 * MIB).unwrap();
        assert_eq!(a.snapshot().peak, 300 * MIB);
        a.release(1, 100 * MIB);
        assert_eq!(a.in_use(), 200 * MIB);
        assert_eq!(a.snapshot().peak, 300 * MIB, "the peak is a high-water mark");
        assert_eq!(a.snapshot().owners, 1, "an owner with nothing mapped has no row");
        a.charge(1, 1, false, 50 * MIB).unwrap();
        assert_eq!(a.snapshot().peak, 300 * MIB);
        a.charge(1, 1, false, 100 * MIB).unwrap();
        assert_eq!(a.snapshot().peak, 350 * MIB);
    }

    #[test]
    fn sizes_are_page_rounded_consistently() {
        let mut a = dynamic(GIB, 0);
        // A size that is not a page multiple is charged and released as the page it takes.
        a.charge(1, 1, false, 1).unwrap();
        assert_eq!(a.in_use(), 4096);
        a.release(1, 1);
        assert_eq!(a.in_use(), 0);
        assert_eq!(a.snapshot().maps, 0);
    }

    #[test]
    fn free_all_by_owner_on_retire() {
        let mut a = dynamic(GIB, 0);
        for _ in 0..10 {
            a.charge(1, 1, false, 10 * MIB).unwrap();
        }
        a.charge(2, 2, false, 20 * MIB).unwrap();
        assert_eq!(a.forget_owner(1), 100 * MIB);
        assert_eq!(a.in_use(), 20 * MIB);
        assert_eq!(a.snapshot().maps, 1);
        assert_eq!(a.bytes_of(1), 0);
        // Forgetting an owner that is not there changes nothing.
        assert_eq!(a.forget_owner(99), 0);
        // Late releases of the forgotten owner's maps (a racing MUNMAP) are harmless.
        a.release(1, 10 * MIB);
        assert_eq!(a.in_use(), 20 * MIB);
        assert_eq!(a.snapshot().maps, 1);
    }

    #[test]
    fn privilege_is_sticky_until_forgotten() {
        let mut a = dynamic(GIB, 256);
        a.charge(1, 1, false, 768 * MIB).unwrap();
        assert_eq!(a.check(7, false, 4096), Err(Refusal::ReserveHit));
        assert!(a.mark_privileged(7, 70));
        assert!(a.is_marked_privileged(7));
        assert_eq!(a.check(7, false, 4096), Ok(()), "the mark alone is enough");
        a.charge(7, 70, false, 4096).unwrap();
        a.release(7, 4096);
        // The row survives with nothing mapped: still privileged.
        assert!(a.is_marked_privileged(7));
        assert_eq!(a.snapshot().privileged_owners, 1);
        a.forget_owner(7);
        assert!(!a.is_marked_privileged(7));
        assert_eq!(a.check(7, false, 4096), Err(Refusal::ReserveHit));
    }

    #[test]
    fn live_privilege_without_a_mark() {
        let a = dynamic(GIB, 256);
        assert_eq!(a.check(5, true, GIB), Ok(()));
        assert_eq!(a.check(5, false, GIB), Err(Refusal::TooBig));
    }

    #[test]
    fn legacy_is_a_quarter_per_owner_and_nothing_else() {
        let mut a = legacy(4 * GIB); // quota 1 GiB
        a.charge(1, 1, false, GIB).unwrap();
        assert_eq!(a.check(1, false, 4096), Err(Refusal::Quota));
        // Privilege and reserve mean nothing here.
        assert_eq!(a.check(1, true, 4096), Err(Refusal::Quota));
        // Another owner has its own quarter; the total is not checked (4 owners fill it).
        a.charge(2, 2, false, GIB).unwrap();
        a.charge(3, 3, false, GIB).unwrap();
        a.charge(4, 4, false, GIB).unwrap();
        a.charge(5, 5, false, GIB).unwrap();
        assert_eq!(a.in_use(), 5 * GIB, "legacy never looked at the total");
        // Exactly at the quota is allowed, one page over is not.
        let b = legacy(4 * GIB);
        assert_eq!(b.check(1, false, GIB), Ok(()));
        assert_eq!(b.check(1, false, GIB + 4096), Err(Refusal::Quota));
    }

    #[test]
    fn legacy_equals_the_old_function() {
        // The code this replaces: `mine + size <= window / 4` with saturating adds.
        fn old(mine: u64, size: u64, window: u64) -> bool {
            mine.saturating_add(size) <= window / 4
        }
        let windows = [0, 4096, 4 * GIB, 4 * GIB + 8192, 32 * GIB];
        let sizes = [4096, 8192, MIB, 256 * MIB, GIB, 2 * GIB, 8 * GIB];
        for &w in &windows {
            for &m in &sizes {
                for &s in &sizes {
                    let mut a = Account::new(Config::new(w, 256, 0, Policy::Legacy), 4).unwrap();
                    // Put `m` bytes on the owner without going through the policy.
                    a.rows.push(Row {
                        owner: 1,
                        pid: 1,
                        bytes: m,
                        maps: 1,
                        privileged: false,
                    });
                    assert_eq!(a.check(1, false, s).is_ok(), old(m, s, w), "w={w} m={m} s={s}");
                }
            }
        }
    }

    #[test]
    fn table_full_is_counted_and_charges_nothing() {
        let mut a = Account::new(Config::new(GIB, 0, 0, Policy::Dynamic), 2).unwrap();
        a.charge(1, 1, false, 4096).unwrap();
        a.charge(2, 2, false, 4096).unwrap();
        assert_eq!(a.charge(3, 3, false, 4096), Err(Refusal::TableFull));
        assert_eq!(a.stats().table_full, 1);
        assert_eq!(a.in_use(), 8192);
        // An existing owner still charges.
        a.charge(1, 1, false, 4096).unwrap();
        // Freeing a row makes room.
        a.release(2, 4096);
        a.charge(3, 3, false, 4096).unwrap();
        // A privileged mark needs a row too.
        let mut b = Account::new(Config::new(GIB, 0, 0, Policy::Dynamic), 1).unwrap();
        assert!(b.mark_privileged(1, 1));
        assert!(!b.mark_privileged(2, 2));
        assert_eq!(b.stats().table_full, 1);
        assert_eq!(b.row_capacity(), 1);
    }

    #[test]
    fn refusals_are_counted_by_reason() {
        let mut a = dynamic(GIB, 256);
        assert_eq!(a.admit(1, false, 2 * GIB), Err(Refusal::TooBig));
        a.charge(1, 1, false, 768 * MIB).unwrap();
        assert_eq!(a.admit(1, false, 4096), Err(Refusal::ReserveHit));
        assert_eq!(a.admit(1, false, 4096), Err(Refusal::ReserveHit));
        a.charge(2, 2, true, 256 * MIB).unwrap();
        assert_eq!(a.admit(2, true, 4096), Err(Refusal::WindowFull));
        a.note_host_refused(12);
        a.note_host_refused(22);
        a.note_addr_space();
        let s = a.stats();
        assert_eq!(s.too_big, 1);
        assert_eq!(s.reserve_hit, 2);
        assert_eq!(s.window_full, 1);
        assert_eq!(s.host_refused, 2);
        assert_eq!(s.last_host_errno, 22);
        assert_eq!(s.addr_space, 1);
        assert_eq!(s.total(), 1 + 2 + 1 + 2 + 1);
        // A successful admit counts nothing.
        let mut b = dynamic(GIB, 256);
        assert_eq!(b.admit(1, false, 4096), Ok(()));
        assert_eq!(b.stats().total(), 0);
    }

    #[test]
    fn top_four_by_bytes() {
        let mut a = dynamic(64 * GIB, 0);
        for (owner, mib) in [(1u64, 10u64), (2, 50), (3, 30), (4, 70), (5, 20), (6, 60)] {
            a.charge(owner, owner as u32 * 100, false, mib * MIB).unwrap();
        }
        let top = a.top();
        let owners: [u64; 4] = [top[0].owner, top[1].owner, top[2].owner, top[3].owner];
        assert_eq!(owners, [4, 6, 2, 3]);
        assert_eq!(top[0].bytes, 70 * MIB);
        assert_eq!(top[0].pid, 400);
        // Ties: the lower token first, deterministic.
        let mut b = dynamic(GIB, 0);
        b.charge(9, 9, false, 5 * MIB).unwrap();
        b.charge(3, 3, false, 5 * MIB).unwrap();
        assert_eq!(b.top()[0].owner, 3);
        assert_eq!(b.top()[1].owner, 9);
        assert_eq!(b.top()[2], Top::default());
        // Fewer owners than ranks.
        let c = dynamic(GIB, 0);
        assert_eq!(c.top(), [Top::default(); TOP_N]);
    }

    #[test]
    fn top_ignores_privileged_owners_with_nothing_mapped() {
        let mut a = dynamic(GIB, 256);
        a.mark_privileged(1, 1);
        assert_eq!(a.top()[0], Top::default());
        assert_eq!(a.snapshot().owners, 1);
    }

    #[test]
    fn pack_top_fits_one_atomic() {
        assert_eq!(pack_top(Top::default()), 0);
        let t = Top {
            owner: 5,
            pid: 1234,
            bytes: 17 * GIB,
        };
        let p = pack_top(t);
        assert_eq!(p >> 32, 1234);
        assert_eq!(p & 0xFFFF_FFFF, 17 * 1024);
        let big = Top {
            owner: 1,
            pid: u32::MAX,
            bytes: u64::MAX,
        };
        assert_eq!(pack_top(big), (u64::from(u32::MAX) << 32) | u64::from(u32::MAX));
    }

    #[test]
    fn clear_forgets_marks_and_bytes_but_keeps_statistics() {
        let mut a = dynamic(GIB, 256);
        a.charge(1, 1, false, 4096).unwrap();
        a.mark_privileged(2, 2);
        let _ = a.admit(1, false, 2 * GIB);
        a.clear();
        assert_eq!(a.in_use(), 0);
        assert_eq!(a.snapshot().owners, 0);
        assert!(!a.is_marked_privileged(2));
        assert_eq!(a.stats().too_big, 1);
        assert_eq!(a.snapshot().peak, 4096);
    }

    /// A long interleaving of maps, unmaps, retires and refusals from several owners keeps
    /// the totals equal to a full recomputation and never exceeds the cap.
    #[test]
    fn concurrent_style_sequence_keeps_the_books() {
        let mut a = Account::new(Config::new(2 * GIB, 128, 0, Policy::Dynamic), 16).unwrap();
        let mut live: Vec<(u64, u64)> = Vec::new(); // (owner, size)
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for step in 0..20_000u32 {
            let r = next();
            let owner = (r % 6) + 1;
            match (r >> 8) % 5 {
                0 | 1 | 2 => {
                    let size = (((r >> 16) % 64) + 1) * 2 * MIB;
                    let privileged = owner == 1;
                    if a.charge(owner, owner as u32, privileged, size).is_ok() {
                        live.push((owner, size));
                    }
                }
                3 => {
                    if !live.is_empty() {
                        let i = (r >> 20) as usize % live.len();
                        let (o, s) = live.swap_remove(i);
                        a.release(o, s);
                    }
                }
                _ => {
                    if step % 97 == 0 {
                        let mut freed = 0;
                        live.retain(|&(o, s)| {
                            if o == owner {
                                freed += s;
                                false
                            } else {
                                true
                            }
                        });
                        assert_eq!(a.forget_owner(owner), freed);
                    }
                }
            }
            let sum: u64 = live.iter().map(|&(_, s)| s).sum();
            assert_eq!(a.in_use(), sum, "step {step}");
            assert!(a.in_use() <= 2 * GIB);
            assert_eq!(a.snapshot().maps, live.len() as u64);
        }
        for owner in 1..=6 {
            let sum: u64 = live.iter().filter(|&&(o, _)| o == owner).map(|&(_, s)| s).sum();
            assert_eq!(a.bytes_of(owner), sum);
        }
    }

    #[test]
    fn an_ordinary_owner_never_passes_the_ordinary_limit() {
        let mut a = Account::new(Config::new(GIB, 256, 0, Policy::Dynamic), 16).unwrap();
        let mut x = 7u64;
        for _ in 0..5_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let owner = (x >> 60) + 2; // 2..=17, never privileged
            let size = (((x >> 20) % 32) + 1) * MIB;
            let _ = a.charge(owner, 1, false, size);
            assert!(a.in_use() <= GIB - 256 * MIB);
        }
    }

    #[test]
    fn info_for_an_ordinary_device_under_the_dynamic_policy() {
        let mut a = dynamic(32 * GIB, 256);
        a.charge(1, 10, false, 3 * GIB).unwrap();
        a.charge(2, 20, false, GIB).unwrap();
        let i = a.info(1, false);
        assert_eq!(i.window_bytes, 32 * GIB);
        assert_eq!(i.used_bytes, 4 * GIB);
        assert_eq!(i.owner_limit_bytes, 32 * GIB - 256 * MIB);
        assert_eq!(i.owner_used_bytes, 3 * GIB);
        assert_eq!(i.flags, INFO_OWNER_LIMIT | INFO_SHARED_CEILING);
        // The room: the ceiling on the total minus the total.
        assert_eq!(i.owner_limit_bytes - i.used_bytes, 32 * GIB - 256 * MIB - 4 * GIB);
        // A device with nothing mapped reports 0 used and the same ceiling.
        let z = a.info(99, false);
        assert_eq!(z.owner_used_bytes, 0);
        assert_eq!(z.owner_limit_bytes, i.owner_limit_bytes);
        assert_eq!(z.used_bytes, 4 * GIB);
    }

    #[test]
    fn info_for_the_privileged_device_has_no_reserve_to_subtract() {
        let mut a = dynamic(32 * GIB, 256);
        a.mark_privileged(7, 70);
        let i = a.info(7, false);
        assert_eq!(i.owner_limit_bytes, 32 * GIB);
        // Not below the window: no "owner limit" flag, but the ceiling is still shared.
        assert_eq!(i.flags, INFO_SHARED_CEILING);
        // Live evidence without a mark gives the same answer.
        assert_eq!(a.info(8, true), a.info(8, true));
        assert_eq!(a.info(8, true).owner_limit_bytes, 32 * GIB);
    }

    #[test]
    fn info_with_no_reserve_and_no_bound_reports_the_whole_window() {
        let a = dynamic(32 * GIB, 0);
        let i = a.info(1, false);
        assert_eq!(i.owner_limit_bytes, 32 * GIB);
        assert_eq!(i.flags, INFO_SHARED_CEILING);
    }

    #[test]
    fn info_follows_the_operator_bound() {
        let a = Account::new(Config::new(32 * GIB, 256, 8 * 1024, Policy::Dynamic), 4).unwrap();
        let i = a.info(1, false);
        assert_eq!(i.window_bytes, 32 * GIB);
        assert_eq!(i.owner_limit_bytes, 8 * GIB - 256 * MIB);
        assert!(i.flags & INFO_OWNER_LIMIT != 0);
    }

    #[test]
    fn info_under_the_legacy_quota_is_per_device() {
        let mut a = legacy(32 * GIB);
        a.charge(1, 1, false, 2 * GIB).unwrap();
        let i = a.info(1, false);
        assert_eq!(i.owner_limit_bytes, 8 * GIB);
        assert_eq!(i.owner_used_bytes, 2 * GIB);
        assert_eq!(i.used_bytes, 2 * GIB);
        // No shared-ceiling flag: the room is limit minus the device's own bytes.
        assert_eq!(i.flags, INFO_OWNER_LIMIT);
        // Privilege means nothing here.
        assert_eq!(a.info(1, true), i);
    }

    #[test]
    fn info_has_no_side_effects_and_is_u64_clean() {
        let mut a = dynamic(128 * GIB, 256);
        a.charge(1, 1, false, 100 * GIB).unwrap();
        let before = (a.in_use(), a.stats(), a.snapshot().peak, a.snapshot().owners);
        for _ in 0..1000 {
            let _ = a.info(1, false);
            let _ = a.info(2, true);
        }
        assert_eq!(before, (a.in_use(), a.stats(), a.snapshot().peak, a.snapshot().owners));
        let i = a.info(1, false);
        assert_eq!(i.window_bytes, 128 * GIB);
        assert_eq!(i.owner_used_bytes, 100 * GIB);
        assert!(i.window_bytes > u64::from(u32::MAX));
        // No window at all: everything zero, still an answer.
        let n = dynamic(0, 256);
        let z = n.info(1, false);
        assert_eq!(
            (z.window_bytes, z.used_bytes, z.owner_limit_bytes, z.owner_used_bytes),
            (0, 0, 0, 0)
        );
        assert_eq!(z.flags & INFO_OWNER_LIMIT, 0);
    }

    #[test]
    fn info_after_forget_reports_zero_used() {
        let mut a = dynamic(GIB, 0);
        a.charge(1, 1, false, 100 * MIB).unwrap();
        assert_eq!(a.info(1, false).owner_used_bytes, 100 * MIB);
        a.forget_owner(1);
        assert_eq!(a.info(1, false).owner_used_bytes, 0);
        assert_eq!(a.info(1, false).used_bytes, 0);
    }

    #[test]
    fn fragmentation_is_the_hosts_to_judge() {
        // The KMD cannot see extents: a host refusal for lack of a contiguous extent is
        // counted on its own, with the errno, while the byte accounting says there is room.
        let mut a = dynamic(GIB, 256);
        a.charge(1, 1, false, 100 * MIB).unwrap();
        assert_eq!(a.check(1, false, 256 * MIB), Ok(()));
        a.note_host_refused(12);
        assert_eq!(a.stats().host_refused, 1);
        assert_eq!(a.stats().last_host_errno, 12);
        assert_eq!(a.in_use(), 100 * MIB);
    }
}

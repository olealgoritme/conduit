//! Hang snapshots of the native NVIDIA channels (`NvSnapMs`, `NvSnapNow`, `NvSnapVa`): the
//! pure half. The I/O half (registry knobs, the queue registry, CPU reads of GPFIFO rings,
//! pushbuffers and semaphores, the VA lookups) is `kmd_render/src/ddi/native_snap.rs`; the
//! host decoder is `pr198-tools/agent-snap/snapdec.py`.
//!
//! A snapshot answers "where is each queue stuck": per native HW queue its channel (the
//! work-submit token's channel id is the `channel 0x..` of a host Xid), GP_GET / GP_PUT, the
//! progress fence (submitted / completed), USERD, the GPFIFO entries around GP_GET, and for the
//! last few pushbuffer segments up to GP_GET their first and last dwords, every semaphore
//! operation in them ([`SemOp`]: host `SEM_EXECUTE`, 3D / compute `REPORT_SEMAPHORE_EXECUTE` and
//! `SET_REPORT_SEMAPHORE_D`, copy `LAUNCH_DMA` releases) and every GPU VA they point the front
//! end or the MME at ([`AddrRef`]: MME DMA reads / writes, inline-to-memory, QMDs, constant
//! buffers, index buffers, texture pools, ...). Each such VA is looked up in the KMD's
//! native-memory mapping table ([`va_entry`]) and the page-table operation ring, and the
//! current value at every semaphore address is read back.
//!
//! The decoder ([`Scanner`]) runs over the segments in GPFIFO order with one state, so a
//! method pair split across segments (an address set in one, used in the next) still resolves.
//!
//! # Blob layout (all little-endian; `NvSnapBlob`, `NvSnapB0`..`NvSnapB3`, `NvSnapVaR`)
//!
//! ```text
//! header (HDR_BYTES = 64):
//!   0 magic "NSNP" | 4 version | 8 header bytes | 12 total bytes | 16 seq | 20 reason
//!  24 trigger token | 28 queue records | 32 now (100 ns, u64) | 40 duration us
//!  44 NvSnapMs in force | 48 NvHangMs in force | 52 flags (FLAG_*)
//!  56 HwsSubmit | 60 HwsPollTick
//! then the queue records (QREC_*), then one VA table record (VREC_*)
//! queue record:
//!   0 bytes | 4 kind (REC_QUEUE) | 8 slot | 12 node | 16 pid | 20 flags (Q_*) | 24 veid
//!  28 token | 32 TSG handle | 36 context-share handle | 40 channel handle | 44 TSG hw id
//!  48 process key u64 | 56 context key u64 | 64 GPFIFO VA u64 | 72 ring entries
//!  76 KMD GP_PUT | 80 USERD GP_GET | 84 USERD GP_PUT | 88 submitted u64 | 96 signalled u64
//! 104 fence now u64 | 112 fence GPU VA u64 | 120 ms without progress | 124 notifier info32
//! 128 notifier word | 132 ring read status | 136 USERD dwords [USERD_DWORDS]
//! 392 window first index | 396 window entries | 400 segments | 404 0
//! 408 window entries [WINDOW] x (lo, hi) | 472 segment records
//! segment record:
//!   0 bytes | 4 GP index | 8 VA u64 | 16 length (dwords) | 20 read status | 24 scanned dwords
//!  28 scan status (SCAN_*) | 32 lost at | 36 semaphore ops seen | 40 semaphore ops recorded
//!  44 tail start | 48 tail aligned (first header >= tail start, u32::MAX none) | 52 head dwords
//!  56 tail dwords | 60 MME calls | 64 address refs seen | 68 address refs recorded
//!  72 VA-table index of the segment's own VA | 76 flags (S_*)
//!  80 head [n] | tail [n] | semaphore ops [n] x SEM_BYTES | address refs [n] x REF_BYTES
//! semaphore op (SEM_BYTES = 40):
//!   0 kind | 1 subchannel | 2 operation | 3 flags (SEM_ACQUIRE, SEM_PAYLOAD64)
//!   4 execute word | 8 at (dword offset) | 12 class | 16 address u64 | 24 payload u64
//!  32 VA-table index | 36 0
//! address ref (REF_BYTES = 32):
//!   0 tag (TAG_*) | 2 subchannel | 3 flags (REF_HI_SEEN) | 4 method | 8 at | 12 length
//!  16 VA u64 | 24 VA-table index | 28 class
//! VA table record:
//!   0 bytes | 4 kind (REC_VA) | 8 entries | 12 pt-ring on | 16 entries [n] x VA_BYTES
//! VA entry (VA_BYTES = 112):
//!   0 VA u64 | 8 process key u64 | 16 hit (HIT_*) | 20 allocation id | 24 RM memory handle
//!  28 flags (V_*) | 32 piece VA u64 | 40 piece length u64 | 48 memory offset u64
//!  56 allocation size u64 | 64 resident PA u64 | 72 value u64 (8 bytes at VA when read)
//!  80 value status (RD_*) | 84 last ring kind | 88 last ring time (100 ns) u64
//!  96 last ring VA u64 | 104 last ring length u32 | 108 touch-table kind
//! ```

/// `NvSnapMs` when the service key has no value; 0 turns the automatic snapshot off.
pub const DEFAULT_SNAP_MS: u32 = 3_000;
pub const MIN_SNAP_MS: u32 = 500;
pub const MAX_SNAP_MS: u32 = 600_000;
/// Two automatic snapshots at least this far apart (100 ns).
pub const AUTO_MIN_GAP_100NS: u64 = 20_000_000;
/// Automatic snapshots per boot at most (a manual one is always taken).
pub const AUTO_MAX: u32 = 32;
/// The snapshot's memory reads stop after this long (100 ns); later reads report RD_BUDGET.
pub const READ_BUDGET_100NS: u64 = 40_000_000;
/// The longest blob (bytes).
pub const BLOB_CAP: usize = 256 * 1024;

/// The stall timeout for an `NvSnapMs` value: `None` (0) = automatic snapshots off.
pub const fn snap_timeout_100ns(knob_ms: u32) -> Option<u64> {
    if knob_ms == 0 || knob_ms == u32::MAX {
        return None;
    }
    let ms = if knob_ms < MIN_SNAP_MS {
        MIN_SNAP_MS
    } else if knob_ms > MAX_SNAP_MS {
        MAX_SNAP_MS
    } else {
        knob_ms
    };
    Some(ms as u64 * 10_000)
}

/// Why a snapshot was taken.
pub mod reason {
    /// A queue with work outstanding made no progress for `NvSnapMs`.
    pub const AUTO: u32 = 1;
    /// `NvSnapNow` was set.
    pub const MANUAL: u32 = 2;
    /// A queue's error notifier was set (RM put the channel in error).
    pub const ERROR: u32 = 3;
    /// `NvSnapVa`: a VA lookup only (no queue records).
    pub const VA_QUERY: u32 = 4;
}

/// The host channel id of a Blackwell work-submit token (`NV_VIRTUAL_FUNCTION_DOORBELL_VECTOR`).
pub const fn token_chid(token: u32) -> u32 {
    token & 0xfff
}

/// The runlist of a work-submit token (`NV_VIRTUAL_FUNCTION_DOORBELL_RUNLIST_ID`).
pub const fn token_runlist(token: u32) -> u32 {
    (token >> 16) & 0x7f
}

// ── GPFIFO entries ───────────────────────────────────────────────────────────────────────────

/// One decoded GPFIFO entry (`NVCA6F_GP_ENTRY0/1`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpEntry {
    pub va: u64,
    /// Segment length in dwords; 0 = a control entry (`opcode`).
    pub dwords: u32,
    pub subroutine: bool,
    pub sync_wait: bool,
    pub conditional: bool,
    pub opcode: u8,
}

pub const fn gp_entry(lo: u32, hi: u32) -> GpEntry {
    GpEntry {
        va: (lo & !3) as u64 | ((hi & 0xff) as u64) << 32,
        dwords: (hi >> 10) & 0x1f_ffff,
        subroutine: (hi >> 9) & 1 != 0,
        sync_wait: hi >> 31 != 0,
        conditional: lo & 1 != 0,
        opcode: hi as u8,
    }
}

/// GPFIFO entries recorded around GP_GET.
pub const WINDOW: usize = 8;
/// The window starts this many entries before GP_GET.
pub const WINDOW_BEFORE: u32 = 6;
/// Segments decoded per queue at most.
pub const MAX_SEGMENTS: usize = 6;

/// The first ring index of the window around `gp_get` in a ring of `entries`.
pub const fn window_first(gp_get: u32, entries: u32) -> u32 {
    if entries == 0 {
        return 0;
    }
    let back = if WINDOW_BEFORE < entries { WINDOW_BEFORE } else { entries - 1 };
    (gp_get % entries + entries - back) % entries
}

/// The segments to decode, in GPFIFO order: the window positions (0-based offsets into
/// `window`, whose first entry has ring index `first`) of the up-to-[`MAX_SEGMENTS`] entries
/// ending at GP_GET (GP_GET itself only when it is not GP_PUT, i.e. still to run), skipping
/// control entries, and the entry whose segment contains `pb_get` (USERD's pushbuffer GET)
/// when it lies in the window. Returns `(positions, count)`.
pub fn pick_segments(
    window: &[GpEntry],
    first: u32,
    entries: u32,
    gp_get: u32,
    gp_put: u32,
    pb_get: u64,
) -> ([u8; MAX_SEGMENTS], usize) {
    let mut out = [0u8; MAX_SEGMENTS];
    let mut n = 0usize;
    if entries == 0 || window.is_empty() {
        return (out, 0);
    }
    let get_pos = ((gp_get % entries + entries - first % entries) % entries) as usize;
    let last = if gp_get % entries != gp_put % entries { get_pos } else { get_pos.wrapping_sub(1) };
    let mut contains_get = None;
    if pb_get != 0 {
        contains_get = window.iter().position(|e| {
            e.dwords != 0 && pb_get >= e.va && pb_get < e.va + 4 * e.dwords as u64
        });
    }
    // Walk back from `last` collecting real segments.
    let mut pos = last;
    let mut picked = [false; WINDOW];
    while pos < window.len() && n < MAX_SEGMENTS {
        if window[pos].dwords != 0 {
            picked[pos] = true;
            n += 1;
        }
        if pos == 0 {
            break;
        }
        pos -= 1;
    }
    if let Some(p) = contains_get {
        if p < WINDOW && !picked[p] {
            if n == MAX_SEGMENTS {
                // Drop the oldest picked one.
                if let Some(old) = picked.iter().position(|x| *x) {
                    picked[old] = false;
                }
            }
            picked[p] = true;
        }
    }
    let mut k = 0usize;
    for (p, on) in picked.iter().enumerate() {
        if *on && k < MAX_SEGMENTS {
            out[k] = p as u8;
            k += 1;
        }
    }
    (out, k)
}

// ── pushbuffer decoding ──────────────────────────────────────────────────────────────────────

/// Semaphore operation kinds.
pub mod sem_kind {
    /// Host `SEM_ADDR_LO/HI`, `SEM_PAYLOAD_LO/HI`, `SEM_EXECUTE` (`NVC86F`/`NVCA6F`).
    pub const HOST: u8 = 1;
    /// Host `SEMAPHOREA..D` (the pre-Hopper form).
    pub const HOST_LEGACY: u8 = 2;
    /// 3D / compute `SET_REPORT_SEMAPHORE_PAYLOAD/ADDRESS_*` + `REPORT_SEMAPHORE_EXECUTE`.
    pub const GR_EXEC: u8 = 3;
    /// 3D / compute `SET_REPORT_SEMAPHORE_A..D`.
    pub const GR_LEGACY: u8 = 4;
    /// Copy `SET_SEMAPHORE_*` + `LAUNCH_DMA` semaphore (a release).
    pub const COPY: u8 = 5;
}

pub const SEM_ACQUIRE: u8 = 1;
pub const SEM_PAYLOAD64: u8 = 2;

/// One semaphore operation in a segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemOp {
    pub kind: u8,
    pub subch: u8,
    pub op: u8,
    pub flags: u8,
    pub word: u32,
    pub at: u32,
    pub class: u32,
    pub addr: u64,
    pub payload: u64,
}

impl SemOp {
    pub const fn acquire(&self) -> bool {
        self.flags & SEM_ACQUIRE != 0
    }
}

/// What a front-end / MME address points at.
pub mod tag {
    pub const MME_READ: u16 = 1;
    pub const MME_READ_FIFOED: u16 = 2;
    pub const MME_WRITE: u16 = 3;
    pub const MME_REDUCTION: u16 = 4;
    /// Inline-to-memory destination (`OFFSET_OUT` + `LAUNCH_DMA`).
    pub const I2M: u16 = 5;
    /// Compute `SEND_PCAS_A` (QMD address >> 8).
    pub const QMD: u16 = 6;
    pub const CONSTANT_BUFFER: u16 = 7;
    pub const INDEX_BUFFER: u16 = 8;
    pub const TEX_HEADER_POOL: u16 = 9;
    pub const TEX_SAMPLER_POOL: u16 = 10;
    pub const SHADER_LOCAL_MEMORY: u16 = 11;
    pub const PROGRAM: u16 = 12;
    pub const VERTEX_STREAM: u16 = 13;
    /// `SET_MME_MEM_ADDRESS_B` written (the address an MME macro's DMA may use).
    pub const MME_ADDRESS: u16 = 14;

    /// Kept even when the record array is full (they replace lower-priority refs).
    pub const fn priority(tag: u16) -> bool {
        matches!(tag, MME_READ | MME_READ_FIFOED | MME_WRITE | MME_REDUCTION | I2M | QMD | MME_ADDRESS)
    }
}

/// The upper half of the address was set (in this or an earlier segment of the scan).
pub const REF_HI_SEEN: u8 = 1;

/// One address the segment points the front end / MME at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddrRef {
    pub tag: u16,
    pub subch: u8,
    pub flags: u8,
    pub method: u32,
    pub at: u32,
    pub len: u32,
    pub va: u64,
    pub class: u32,
}

/// Scan outcome.
pub mod scan_status {
    /// The whole segment was decoded.
    pub const COMPLETE: u32 = 0;
    /// Stopped at the scan cap.
    pub const CAPPED: u32 = 1;
    /// An unknown header: decoding stopped at `lost_at`.
    pub const LOST: u32 = 2;
    /// A read failed (see the segment's read status).
    pub const READ_FAILED: u32 = 3;
    /// `END_PB_SEGMENT`.
    pub const ENDED: u32 = 4;
    /// Not scanned (budget, control entry).
    pub const SKIPPED: u32 = 5;
}

pub const SEM_CAP: usize = 16;
pub const REF_CAP: usize = 48;

/// The subchannel bindings the D3D UMDs assume on a native graphics channel
/// (`native_channel::BINDINGS` and the deferred-API object on 5).
pub const DEFAULT_CLASSES: [u32; 8] = [0xce97, 0xcec0, 0xcd40, 0x902d, 0xcab5, 0x5080, 0, 0];

const fn is_gr(class: u32) -> bool {
    matches!(class & 0xff, 0x97 | 0xc0)
}

const fn is_3d(class: u32) -> bool {
    class & 0xff == 0x97
}

const fn is_compute(class: u32) -> bool {
    class & 0xff == 0xc0
}

const fn is_copy(class: u32) -> bool {
    class & 0xff == 0xb5
}

const fn is_i2m(class: u32) -> bool {
    class & 0xff == 0x40
}

#[derive(Clone, Copy, Default)]
struct SubState {
    /// REPORT_SEMAPHORE payload lo, hi, address lo, hi.
    rep: [u32; 4],
    /// SET_REPORT_SEMAPHORE_A, B, C.
    rep_legacy: [u32; 3],
    /// Copy SET_SEMAPHORE_A (upper), B (lower), payload, payload upper.
    copy: [u32; 4],
    mme_hi: u32,
    mme_lo: u32,
    mme_hi_seen: bool,
    i2m_hi: u32,
    i2m_lo: u32,
    i2m_hi_seen: bool,
    cb_size: u32,
    /// Generic hi halves by tag (indexed by tag), for the simple pairs.
    hi: [u32; 16],
    hi_seen: u16,
}

/// The pushbuffer decoder: fed dword by dword over one or more segments in GPFIFO order.
pub struct Scanner {
    pos: u32,
    remaining: u32,
    method: u32,
    subch: u32,
    mode: u32,
    first: bool,
    pub classes: [u32; 8],
    host: [u32; 4],
    host_legacy: [u32; 3],
    subs: [SubState; 8],
    // Per segment:
    pub sems: [SemOp; SEM_CAP],
    pub n_sems: u32,
    pub sems_seen: u32,
    pub refs: [AddrRef; REF_CAP],
    pub n_refs: u32,
    pub refs_seen: u32,
    pub lost_at: Option<u32>,
    pub ended: bool,
    tail_start: u32,
    pub tail_aligned: u32,
    pub mme_calls: u32,
    pub headers: u32,
}

impl Scanner {
    pub const fn new(classes: [u32; 8]) -> Self {
        const S: SubState = SubState {
            rep: [0; 4],
            rep_legacy: [0; 3],
            copy: [0; 4],
            mme_hi: 0,
            mme_lo: 0,
            mme_hi_seen: false,
            i2m_hi: 0,
            i2m_lo: 0,
            i2m_hi_seen: false,
            cb_size: 0,
            hi: [0; 16],
            hi_seen: 0,
        };
        const Z: SemOp = SemOp {
            kind: 0,
            subch: 0,
            op: 0,
            flags: 0,
            word: 0,
            at: 0,
            class: 0,
            addr: 0,
            payload: 0,
        };
        const R: AddrRef = AddrRef {
            tag: 0,
            subch: 0,
            flags: 0,
            method: 0,
            at: 0,
            len: 0,
            va: 0,
            class: 0,
        };
        Self {
            pos: 0,
            remaining: 0,
            method: 0,
            subch: 0,
            mode: 0,
            first: false,
            classes,
            host: [0; 4],
            host_legacy: [0; 3],
            subs: [S; 8],
            sems: [Z; SEM_CAP],
            n_sems: 0,
            sems_seen: 0,
            refs: [R; REF_CAP],
            n_refs: 0,
            refs_seen: 0,
            lost_at: None,
            ended: false,
            tail_start: u32::MAX,
            tail_aligned: u32::MAX,
            mme_calls: 0,
            headers: 0,
        }
    }

    /// Start a new segment: the per-segment outputs are cleared, the method state
    /// (bindings, address halves, semaphore registers) carries over. `tail_start` is the
    /// dword offset from which [`Scanner::tail_aligned`] looks for the first header.
    pub fn begin_segment(&mut self, tail_start: u32) {
        self.pos = 0;
        // A method's data never spans two GPFIFO segments.
        self.remaining = 0;
        self.n_sems = 0;
        self.sems_seen = 0;
        self.n_refs = 0;
        self.refs_seen = 0;
        self.lost_at = None;
        self.ended = false;
        self.tail_start = tail_start;
        self.tail_aligned = u32::MAX;
        self.mme_calls = 0;
        self.headers = 0;
    }

    pub const fn position(&self) -> u32 {
        self.pos
    }

    pub fn sems(&self) -> &[SemOp] {
        &self.sems[..(self.n_sems as usize).min(SEM_CAP)]
    }

    pub fn refs(&self) -> &[AddrRef] {
        &self.refs[..(self.n_refs as usize).min(REF_CAP)]
    }

    /// Feed one dword. `false`: decoding of this segment stopped (lost sync or
    /// `END_PB_SEGMENT`); the rest of it is not decoded.
    pub fn feed(&mut self, w: u32) -> bool {
        if self.lost_at.is_some() || self.ended {
            return false;
        }
        let at = self.pos;
        self.pos = self.pos.wrapping_add(1);
        if self.remaining > 0 {
            let (sc, m) = (self.subch, self.method);
            self.write(sc, m, w, at);
            match self.mode {
                1 => self.method = self.method.wrapping_add(4),
                5 if self.first => {
                    self.method = self.method.wrapping_add(4);
                    self.first = false;
                }
                _ => {}
            }
            self.remaining -= 1;
            return true;
        }
        self.headers += 1;
        if at >= self.tail_start && self.tail_aligned == u32::MAX {
            self.tail_aligned = at;
        }
        let count = (w >> 16) & 0x1fff;
        let sc = (w >> 13) & 7;
        let m = (w & 0xfff) << 2;
        match w >> 29 {
            0 => {
                // NOP (0), or a subdevice-mask operation (one dword, no data).
                if w != 0 && (w >> 16) & 3 == 0 {
                    self.lost_at = Some(at);
                    return false;
                }
            }
            1 | 3 | 5 => {
                self.mode = w >> 29;
                self.method = m;
                self.subch = sc;
                self.remaining = count;
                self.first = true;
            }
            4 => self.write(sc, m, count, at),
            7 => {
                self.ended = true;
                return false;
            }
            _ => {
                self.lost_at = Some(at);
                return false;
            }
        }
        true
    }

    fn push_sem(&mut self, op: SemOp) {
        self.sems_seen += 1;
        let n = self.n_sems as usize;
        if n < SEM_CAP {
            self.sems[n] = op;
            self.n_sems += 1;
        } else if op.acquire() {
            // Full: an acquire replaces the latest release.
            if let Some(i) = self.sems.iter().rposition(|s| !s.acquire()) {
                self.sems[i] = op;
            }
        }
    }

    fn push_ref(&mut self, r: AddrRef) {
        self.refs_seen += 1;
        let n = self.n_refs as usize;
        if n < REF_CAP {
            self.refs[n] = r;
            self.n_refs += 1;
        } else if tag::priority(r.tag) {
            if let Some(i) = self.refs.iter().position(|x| !tag::priority(x.tag)) {
                self.refs[i] = r;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn addr_ref(&mut self, tag: u16, sc: u32, method: u32, at: u32, len: u32, va: u64, hi_seen: bool) {
        let class = self.classes[sc as usize & 7];
        self.push_ref(AddrRef {
            tag,
            subch: sc as u8,
            flags: if hi_seen { REF_HI_SEEN } else { 0 },
            method,
            at,
            len,
            va,
            class,
        });
    }

    /// A simple (hi method, lo method) pair: the lo write emits the ref.
    fn pair(&mut self, sc: u32, t: u16, hi_mask: u32, is_hi: bool, data: u32, method: u32, at: u32, len: u32) {
        let s = &mut self.subs[sc as usize & 7];
        let i = (t as usize) & 15;
        if is_hi {
            s.hi[i] = data & hi_mask;
            s.hi_seen |= 1 << i;
        } else {
            let va = (s.hi[i] as u64) << 32 | data as u64;
            let seen = s.hi_seen & (1 << i) != 0;
            self.addr_ref(t, sc, method, at, len, va, seen);
        }
    }

    fn write(&mut self, sc: u32, m: u32, data: u32, at: u32) {
        let sc = sc & 7;
        if m < 0x100 {
            self.host_method(sc, m, data, at);
            return;
        }
        let class = self.classes[sc as usize];
        if is_gr(class) || is_i2m(class) {
            match m {
                0x188 => {
                    let s = &mut self.subs[sc as usize];
                    s.i2m_hi = data & 0x1ff_ffff;
                    s.i2m_hi_seen = true;
                    return;
                }
                0x18c => {
                    self.subs[sc as usize].i2m_lo = data;
                    return;
                }
                0x1b0 => {
                    let s = self.subs[sc as usize];
                    let va = (s.i2m_hi as u64) << 32 | s.i2m_lo as u64;
                    self.addr_ref(tag::I2M, sc, m, at, 0, va, s.i2m_hi_seen);
                    return;
                }
                _ => {}
            }
        }
        if is_gr(class) {
            match m {
                0x158..=0x164 => {
                    self.subs[sc as usize].rep[((m - 0x158) / 4) as usize] = data;
                }
                0x168 => {
                    let r = self.subs[sc as usize].rep;
                    let op = (data & 3) as u8;
                    let p64 = (data >> 27) & 1 != 0;
                    // RELEASE 0, ACQUIRE 1 (REPORT_ONLY 2 and TRAP 3 are not syncs).
                    if op <= 1 {
                        self.push_sem(SemOp {
                            kind: sem_kind::GR_EXEC,
                            subch: sc as u8,
                            op,
                            flags: if op == 1 { SEM_ACQUIRE } else { 0 }
                                | if p64 { SEM_PAYLOAD64 } else { 0 },
                            word: data,
                            at,
                            class,
                            addr: ((r[3] & 0x1ff_ffff) as u64) << 32 | r[2] as u64,
                            payload: if p64 { (r[1] as u64) << 32 | r[0] as u64 } else { r[0] as u64 },
                        });
                    }
                }
                0x1b00..=0x1b08 => {
                    self.subs[sc as usize].rep_legacy[((m - 0x1b00) / 4) as usize] = data;
                }
                0x1b0c => {
                    let r = self.subs[sc as usize].rep_legacy;
                    let op = (data & 3) as u8;
                    if op <= 1 {
                        self.push_sem(SemOp {
                            kind: sem_kind::GR_LEGACY,
                            subch: sc as u8,
                            op,
                            flags: if op == 1 { SEM_ACQUIRE } else { 0 },
                            word: data,
                            at,
                            class,
                            addr: ((r[0] & 0x1ff_ffff) as u64) << 32 | r[1] as u64,
                            payload: r[2] as u64,
                        });
                    }
                }
                0x550 => {
                    let s = &mut self.subs[sc as usize];
                    s.mme_hi = data & 0x1ff_ffff;
                    s.mme_hi_seen = true;
                }
                0x554 => {
                    let s = &mut self.subs[sc as usize];
                    s.mme_lo = data;
                    let (va, seen) = ((s.mme_hi as u64) << 32 | data as u64, s.mme_hi_seen);
                    self.addr_ref(tag::MME_ADDRESS, sc, m, at, 0, va, seen);
                }
                0x55c | 0x560 | 0x564 | 0x568 => {
                    let s = self.subs[sc as usize];
                    let t = match m {
                        0x55c => tag::MME_READ,
                        0x560 => tag::MME_READ_FIFOED,
                        0x564 => tag::MME_WRITE,
                        _ => tag::MME_REDUCTION,
                    };
                    let len = if m == 0x568 { 0 } else { data };
                    let va = (s.mme_hi as u64) << 32 | s.mme_lo as u64;
                    self.addr_ref(t, sc, m, at, len, va, s.mme_hi_seen);
                }
                0x790 => self.pair(sc, tag::SHADER_LOCAL_MEMORY, 0xff, true, data, m, at, 0),
                0x794 => self.pair(sc, tag::SHADER_LOCAL_MEMORY, 0xff, false, data, m, at, 0),
                0x155c => self.pair(sc, tag::TEX_SAMPLER_POOL, 0x1ff_ffff, true, data, m, at, 0),
                0x1560 => self.pair(sc, tag::TEX_SAMPLER_POOL, 0x1ff_ffff, false, data, m, at, 0),
                0x1574 => self.pair(sc, tag::TEX_HEADER_POOL, 0x1ff_ffff, true, data, m, at, 0),
                0x1578 => self.pair(sc, tag::TEX_HEADER_POOL, 0x1ff_ffff, false, data, m, at, 0),
                _ => {}
            }
        }
        if is_3d(class) {
            match m {
                // CALL_MME_MACRO(j): 0x3800 + 8j.
                0x3800..=0x3ffc if m & 7 == 0 => self.mme_calls += 1,
                0x17c8 => self.pair(sc, tag::INDEX_BUFFER, 0xff, true, data, m, at, 0),
                0x17cc => self.pair(sc, tag::INDEX_BUFFER, 0xff, false, data, m, at, 0),
                0x2380 => self.subs[sc as usize].cb_size = data & 0x1_ffff,
                0x2384 => self.pair(sc, tag::CONSTANT_BUFFER, 0xff, true, data, m, at, 0),
                0x2388 => {
                    let size = self.subs[sc as usize].cb_size;
                    self.pair(sc, tag::CONSTANT_BUFFER, 0xff, false, data, m, at, size)
                }
                // SET_PIPELINE_PROGRAM_ADDRESS_A/B(j): 0x2014 / 0x2018 + 64j, j < 6.
                0x2014..=0x2158 if (m - 0x2014) % 64 == 0 => {
                    self.pair(sc, tag::PROGRAM, 0x1ff_ffff, true, data, m, at, 0)
                }
                0x2018..=0x215c if (m - 0x2018) % 64 == 0 => {
                    self.pair(sc, tag::PROGRAM, 0x1ff_ffff, false, data, m, at, 0)
                }
                // SET_VERTEX_STREAM_A_LOCATION_A/B(j): 0x1c04 / 0x1c08 + 16j.
                0x1c04..=0x1cf4 if (m - 0x1c04) % 16 == 0 => {
                    self.pair(sc, tag::VERTEX_STREAM, 0xff, true, data, m, at, 0)
                }
                0x1c08..=0x1cf8 if (m - 0x1c08) % 16 == 0 => {
                    self.pair(sc, tag::VERTEX_STREAM, 0xff, false, data, m, at, 0)
                }
                _ => {}
            }
        }
        if is_compute(class) && m == 0x2b4 {
            // SEND_PCAS_A: QMD_ADDRESS_SHIFTED8.
            self.addr_ref(tag::QMD, sc, m, at, 0, (data as u64) << 8, true);
        }
        if is_copy(class) {
            match m {
                0x240..=0x24c => self.subs[sc as usize].copy[((m - 0x240) / 4) as usize] = data,
                0x300 if (data >> 3) & 3 != 0 => {
                    let c = self.subs[sc as usize].copy;
                    let p64 = (data >> 27) & 1 != 0;
                    self.push_sem(SemOp {
                        kind: sem_kind::COPY,
                        subch: sc as u8,
                        op: ((data >> 3) & 3) as u8,
                        flags: if p64 { SEM_PAYLOAD64 } else { 0 },
                        word: data,
                        at,
                        class,
                        addr: ((c[0] & 0x1ff_ffff) as u64) << 32 | c[1] as u64,
                        payload: if p64 { (c[3] as u64) << 32 | c[2] as u64 } else { c[2] as u64 },
                    });
                }
                _ => {}
            }
        }
    }

    fn host_method(&mut self, sc: u32, m: u32, data: u32, at: u32) {
        match m {
            0x00 => self.classes[sc as usize] = data & 0xffff,
            0x10 | 0x14 | 0x18 => self.host_legacy[((m - 0x10) / 4) as usize] = data,
            0x1c => {
                let h = self.host_legacy;
                let op = (data & 0x1f) as u8;
                // ACQUIRE 1, RELEASE 2, ACQ_GEQ 4, ACQ_AND 8, REDUCTION 0x10.
                let acquire = op & (1 | 4 | 8) != 0;
                self.push_sem(SemOp {
                    kind: sem_kind::HOST_LEGACY,
                    subch: sc as u8,
                    op,
                    flags: if acquire { SEM_ACQUIRE } else { 0 },
                    word: data,
                    at,
                    class: 0,
                    addr: ((h[0] & 0xff) as u64) << 32 | (h[1] & !3) as u64,
                    payload: h[2] as u64,
                });
            }
            0x5c | 0x60 | 0x64 | 0x68 => self.host[((m - 0x5c) / 4) as usize] = data,
            0x6c => {
                let h = self.host;
                let op = (data & 7) as u8;
                let p64 = (data >> 24) & 1 != 0;
                // ACQUIRE 0, RELEASE 1, ACQ_STRICT_GEQ 2, ACQ_CIRC_GEQ 3, ACQ_AND 4,
                // ACQ_NOR 5, REDUCTION 6.
                let acquire = matches!(op, 0 | 2 | 3 | 4 | 5);
                self.push_sem(SemOp {
                    kind: sem_kind::HOST,
                    subch: sc as u8,
                    op,
                    flags: if acquire { SEM_ACQUIRE } else { 0 } | if p64 { SEM_PAYLOAD64 } else { 0 },
                    word: data,
                    at,
                    class: 0,
                    addr: ((h[1] & 0x1ff_ffff) as u64) << 32 | (h[0] & !3) as u64,
                    payload: if p64 { (h[3] as u64) << 32 | h[2] as u64 } else { h[2] as u64 },
                });
            }
            _ => {}
        }
    }
}

// ── the automatic trigger ────────────────────────────────────────────────────────────────────

/// What a [`StallWatch::tick`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    None,
    /// Busy without progress for the timeout (once per stall).
    Stall,
    /// The error notifier was seen set (once per queue).
    Error,
}

/// One look at a queue by the mirror thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StallSample {
    pub now: u64,
    /// Work the progress fence does not show complete, or USERD GP_GET short of the
    /// KMD's GP_PUT.
    pub busy: bool,
    pub progress: u64,
    pub gp_get: u32,
    pub error: bool,
}

/// The snapshot's own stall detection (independent of `native_hang`'s recovery watch).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StallWatch {
    progress: u64,
    gp_get: u32,
    since: Option<u64>,
    fired: bool,
    error_seen: bool,
}

impl StallWatch {
    pub const fn new() -> Self {
        Self {
            progress: 0,
            gp_get: 0,
            since: None,
            fired: false,
            error_seen: false,
        }
    }

    pub fn tick(&mut self, s: StallSample, timeout: Option<u64>) -> Trigger {
        if s.error && !self.error_seen {
            self.error_seen = true;
            return Trigger::Error;
        }
        let moved = s.progress != self.progress || s.gp_get != self.gp_get;
        if !s.busy || self.since.is_none() || moved {
            self.progress = s.progress;
            self.gp_get = s.gp_get;
            self.since = s.busy.then_some(s.now);
            self.fired = false;
            return Trigger::None;
        }
        match (timeout, self.since) {
            (Some(t), Some(since)) if !self.fired && s.now.saturating_sub(since) >= t => {
                self.fired = true;
                Trigger::Stall
            }
            _ => Trigger::None,
        }
    }

    /// How long the queue has been busy without progress (ms; 0 = not stalled).
    pub fn stalled_ms(&self, now: u64) -> u32 {
        self.since
            .map_or(0, |since| (now.saturating_sub(since) / 10_000).min(u32::MAX as u64) as u32)
    }
}

// ── the blob ─────────────────────────────────────────────────────────────────────────────────

pub const MAGIC: u32 = u32::from_le_bytes(*b"NSNP");
pub const VERSION: u32 = 1;
pub const HDR_BYTES: usize = 64;
pub const REC_QUEUE: u32 = 1;
pub const REC_VA: u32 = 2;
pub const USERD_DWORDS: usize = 64;
pub const QREC_FIXED: usize = 408 + WINDOW * 8;
pub const SEG_FIXED: usize = 80;
pub const SEM_BYTES: usize = 40;
pub const REF_BYTES: usize = 32;
pub const VA_BYTES: usize = 112;
pub const VA_FIXED: usize = 16;
/// Head / tail dwords recorded per segment at most.
pub const HEAD_DWORDS: usize = 64;
pub const TAIL_DWORDS: usize = 64;
/// Dwords decoded per segment at most.
pub const SCAN_MAX_DWORDS: u32 = 16 * 1024;
/// VA table entries at most.
pub const VA_TABLE_CAP: usize = 512;

/// Header flags.
pub const FLAG_TRUNCATED: u32 = 1;
pub const FLAG_BUDGET: u32 = 2;
pub const FLAG_PIN_FAILED: u32 = 4;
pub const FLAG_VA_TABLE_FULL: u32 = 8;

/// Queue flags.
pub const Q_GRAPHICS: u32 = 1;
pub const Q_DEAD: u32 = 2;
pub const Q_OUTSTANDING: u32 = 4;
pub const Q_FLAGGED: u32 = 8;
pub const Q_IN_GRACE: u32 = 0x10;
pub const Q_WATCH_DEAD: u32 = 0x20;
pub const Q_DETAIL: u32 = 0x40;
pub const Q_ERROR: u32 = 0x80;
pub const Q_TRIGGER: u32 = 0x100;
pub const Q_COPY_BOUND: u32 = 0x200;

/// Segment flags.
pub const S_CONTAINS_PB_GET: u32 = 1;
pub const S_AT_GP_GET: u32 = 2;
pub const S_BEFORE_GP_GET: u32 = 4;

/// VA lookup hits.
pub const HIT_NONE: u32 = 0;
/// A native allocation's RM memory mapped there.
pub const HIT_NATIVE_RM: u32 = 1;
/// VidMm's system pages of a native allocation (an OS-descriptor mapping).
pub const HIT_NATIVE_SYSTEM: u32 = 2;
/// A non-native allocation's system pages (dxgkrnl's monitored fences and the like).
pub const HIT_FOREIGN: u32 = 3;
/// The lookup could not run (state busy).
pub const HIT_UNKNOWN: u32 = 0xff;

/// VA entry flags.
pub const V_RESIDENT_KNOWN: u32 = 1;
pub const V_POISONED: u32 = 2;
pub const V_SYSTEM_MEMORY: u32 = 4;
pub const V_HUGE: u32 = 8;
pub const V_TOUCH_TABLE: u32 = 0x10;

/// Read statuses.
pub const RD_OK: u32 = 0;
pub const RD_NOT_MAPPED: u32 = 1;
pub const RD_ERROR: u32 = 2;
pub const RD_BUDGET: u32 = 3;
pub const RD_NOT_READ: u32 = 4;

/// An append-only little-endian writer over a fixed buffer; writes past the end are dropped
/// and remembered ([`Writer::overflowed`]).
pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflow: false,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Bytes still free.
    pub const fn room(&self) -> usize {
        self.buf.len() - self.len
    }

    pub fn u8(&mut self, v: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = v;
            self.len += 1;
        } else {
            self.overflow = true;
        }
    }

    pub fn u16(&mut self, v: u16) {
        for b in v.to_le_bytes() {
            self.u8(b);
        }
    }

    pub fn u32(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.u8(b);
        }
    }

    pub fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.u8(b);
        }
    }

    pub fn zeros(&mut self, n: usize) {
        for _ in 0..n {
            self.u8(0);
        }
    }

    /// Overwrite a u32 already written at `at`.
    pub fn patch_u32(&mut self, at: usize, v: u32) {
        if at + 4 <= self.len {
            self.buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    /// Cut back to `len` (a record that did not fit).
    pub fn truncate(&mut self, len: usize) {
        if len < self.len {
            self.len = len;
        }
    }
}

/// The header (its sizes, duration and flags are patched at the end with [`finish_header`]).
#[allow(clippy::too_many_arguments)]
pub fn write_header(
    w: &mut Writer<'_>,
    seq: u32,
    reason: u32,
    token: u32,
    now: u64,
    snap_ms: u32,
    hang_ms: u32,
    submits: u32,
    poll_ticks: u32,
) {
    w.u32(MAGIC);
    w.u32(VERSION);
    w.u32(HDR_BYTES as u32);
    w.u32(0);
    w.u32(seq);
    w.u32(reason);
    w.u32(token);
    w.u32(0);
    w.u64(now);
    w.u32(0);
    w.u32(snap_ms);
    w.u32(hang_ms);
    w.u32(0);
    w.u32(submits);
    w.u32(poll_ticks);
}

pub fn finish_header(w: &mut Writer<'_>, records: u32, duration_us: u32, flags: u32) {
    let total = w.len() as u32;
    w.patch_u32(12, total);
    w.patch_u32(28, records);
    w.patch_u32(40, duration_us);
    w.patch_u32(52, flags);
}

/// A queue record's fixed part.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueueFields {
    pub slot: u32,
    pub node: u32,
    pub pid: u32,
    pub flags: u32,
    pub veid: u32,
    pub token: u32,
    pub tsg: u32,
    pub ctxshare: u32,
    pub channel: u32,
    pub hw_tsg: u32,
    pub process: u64,
    pub context: u64,
    pub gpfifo_va: u64,
    pub ring_entries: u32,
    pub kmd_gp_put: u32,
    pub gp_get: u32,
    pub userd_gp_put: u32,
    pub submitted: u64,
    pub signalled: u64,
    pub fence_now: u64,
    pub fence_gpu: u64,
    pub stall_ms: u32,
    pub notifier_info32: u32,
    pub notifier_word: u32,
    pub ring_status: u32,
    pub window_first: u32,
    pub window_n: u32,
}

/// Where a record started and where its count to patch sits.
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    start: usize,
    count_at: usize,
}

/// Write a queue record's fixed part (USERD and the GPFIFO window included); its segment
/// records follow, then [`end_queue`].
pub fn begin_queue(
    w: &mut Writer<'_>,
    q: &QueueFields,
    userd: &[u32; USERD_DWORDS],
    window: &[(u32, u32); WINDOW],
) -> Mark {
    let start = w.len();
    w.u32(0);
    w.u32(REC_QUEUE);
    for v in [q.slot, q.node, q.pid, q.flags, q.veid, q.token, q.tsg, q.ctxshare, q.channel, q.hw_tsg] {
        w.u32(v);
    }
    w.u64(q.process);
    w.u64(q.context);
    w.u64(q.gpfifo_va);
    for v in [q.ring_entries, q.kmd_gp_put, q.gp_get, q.userd_gp_put] {
        w.u32(v);
    }
    w.u64(q.submitted);
    w.u64(q.signalled);
    w.u64(q.fence_now);
    w.u64(q.fence_gpu);
    for v in [q.stall_ms, q.notifier_info32, q.notifier_word, q.ring_status] {
        w.u32(v);
    }
    for v in userd.iter() {
        w.u32(*v);
    }
    w.u32(q.window_first);
    w.u32(q.window_n);
    let count_at = w.len();
    w.u32(0);
    w.u32(0);
    for (lo, hi) in window.iter() {
        w.u32(*lo);
        w.u32(*hi);
    }
    Mark { start, count_at }
}

/// Close a queue record: its segment count and size.
pub fn end_queue(w: &mut Writer<'_>, mark: Mark, segments: u32) {
    w.patch_u32(mark.count_at, segments);
    let bytes = (w.len() - mark.start) as u32;
    w.patch_u32(mark.start, bytes);
}

/// A segment record's fixed part.
#[derive(Clone, Copy, Debug, Default)]
pub struct SegFields {
    pub index: u32,
    pub va: u64,
    pub len: u32,
    pub read_status: u32,
    pub scanned: u32,
    pub scan_status: u32,
    pub lost_at: u32,
    pub sems_seen: u32,
    pub n_sems: u32,
    pub tail_start: u32,
    pub tail_aligned: u32,
    pub n_head: u32,
    pub n_tail: u32,
    pub mme_calls: u32,
    pub refs_seen: u32,
    pub n_refs: u32,
    pub va_index: u32,
    pub flags: u32,
}

/// Write a segment record's fixed part; the head and tail dwords, `n_sems` [`write_sem`]s
/// and `n_refs` [`write_ref`]s follow, then [`end_segment`].
pub fn begin_segment(w: &mut Writer<'_>, s: &SegFields) -> Mark {
    let start = w.len();
    w.u32(0);
    w.u32(s.index);
    w.u64(s.va);
    for v in [
        s.len,
        s.read_status,
        s.scanned,
        s.scan_status,
        s.lost_at,
        s.sems_seen,
        s.n_sems,
        s.tail_start,
        s.tail_aligned,
        s.n_head,
        s.n_tail,
        s.mme_calls,
        s.refs_seen,
        s.n_refs,
        s.va_index,
        s.flags,
    ] {
        w.u32(v);
    }
    Mark { start, count_at: start }
}

pub fn end_segment(w: &mut Writer<'_>, mark: Mark) {
    let bytes = (w.len() - mark.start) as u32;
    w.patch_u32(mark.start, bytes);
}

/// One VA-table entry, as the KMD fills it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VaEntry {
    pub va: u64,
    pub process: u64,
    pub hit: u32,
    pub id: u32,
    pub memory: u32,
    pub flags: u32,
    pub piece_va: u64,
    pub piece_len: u64,
    pub memory_offset: u64,
    pub size: u64,
    pub resident_pa: u64,
    pub value: u64,
    pub value_status: u32,
    pub ring_kind: u32,
    pub ring_t: u64,
    pub ring_va: u64,
    pub ring_len: u32,
    pub touch_kind: u32,
}

/// The VA table: lookups deduplicated by (process, VA), over caller storage (the KMD's is
/// on the heap: [`VA_TABLE_CAP`] entries do not belong on a kernel stack).
pub struct VaTable<'a> {
    pub entries: &'a mut [VaEntry],
    pub n: usize,
    pub full: bool,
}

impl<'a> VaTable<'a> {
    pub fn new(entries: &'a mut [VaEntry]) -> Self {
        Self {
            entries,
            n: 0,
            full: false,
        }
    }

    pub fn used(&self) -> &[VaEntry] {
        &self.entries[..self.n]
    }

    /// The index of (process, va), adding a fresh entry (`hit` = HIT_UNKNOWN until filled)
    /// when new. `(index, new)`; `None` when full.
    pub fn intern(&mut self, process: u64, va: u64) -> Option<(u32, bool)> {
        if let Some(i) = self.entries[..self.n].iter().position(|e| e.va == va && e.process == process) {
            return Some((i as u32, false));
        }
        if self.n >= self.entries.len() {
            self.full = true;
            return None;
        }
        self.entries[self.n] = VaEntry {
            va,
            process,
            hit: HIT_UNKNOWN,
            value_status: RD_NOT_READ,
            ..VaEntry::default()
        };
        self.n += 1;
        Some(((self.n - 1) as u32, true))
    }
}

pub fn write_va_entry(w: &mut Writer<'_>, e: &VaEntry) {
    w.u64(e.va);
    w.u64(e.process);
    w.u32(e.hit);
    w.u32(e.id);
    w.u32(e.memory);
    w.u32(e.flags);
    w.u64(e.piece_va);
    w.u64(e.piece_len);
    w.u64(e.memory_offset);
    w.u64(e.size);
    w.u64(e.resident_pa);
    w.u64(e.value);
    w.u32(e.value_status);
    w.u32(e.ring_kind);
    w.u64(e.ring_t);
    w.u64(e.ring_va);
    w.u32(e.ring_len);
    w.u32(e.touch_kind);
}

pub fn write_va_table(w: &mut Writer<'_>, entries: &[VaEntry], ring_on: bool) {
    let start = w.len();
    w.u32(0);
    w.u32(REC_VA);
    w.u32(entries.len() as u32);
    w.u32(u32::from(ring_on));
    for e in entries {
        write_va_entry(w, e);
    }
    let bytes = (w.len() - start) as u32;
    w.patch_u32(start, bytes);
}

pub fn write_sem(w: &mut Writer<'_>, s: &SemOp, va_index: u32) {
    w.u8(s.kind);
    w.u8(s.subch);
    w.u8(s.op);
    w.u8(s.flags);
    w.u32(s.word);
    w.u32(s.at);
    w.u32(s.class);
    w.u64(s.addr);
    w.u64(s.payload);
    w.u32(va_index);
    w.u32(0);
}

pub fn write_ref(w: &mut Writer<'_>, r: &AddrRef, va_index: u32) {
    w.u16(r.tag);
    w.u8(r.subch);
    w.u8(r.flags);
    w.u32(r.method);
    w.u32(r.at);
    w.u32(r.len);
    w.u64(r.va);
    w.u32(va_index);
    w.u32(r.class);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inc(sc: u32, method: u32, count: u32) -> u32 {
        (1 << 29) | (count << 16) | (sc << 13) | (method >> 2)
    }

    fn feed_all(s: &mut Scanner, words: &[u32]) {
        for w in words {
            if !s.feed(*w) {
                break;
            }
        }
    }

    #[test]
    fn knob() {
        assert_eq!(snap_timeout_100ns(0), None);
        assert_eq!(snap_timeout_100ns(u32::MAX), None);
        assert_eq!(snap_timeout_100ns(100), Some(500 * 10_000));
        assert_eq!(snap_timeout_100ns(3000), Some(3000 * 10_000));
        assert_eq!(snap_timeout_100ns(10_000_000), Some(600_000 * 10_000));
    }

    #[test]
    fn token_fields() {
        // GB202: VECTOR 11:0, RUNLIST_ID 22:16, RUNLIST_DOORBELL 30.
        let token = (1 << 30) | (0xd << 16) | 0x37;
        assert_eq!(token_chid(token), 0x37);
        assert_eq!(token_runlist(token), 0xd);
    }

    #[test]
    fn gp_entry_fields() {
        let e = gp_entry(0x1234_5678 | 1, (0x40 << 10) | 0x12 | (1 << 9) | (1 << 31));
        assert_eq!(e.va, 0x12_1234_5678);
        assert_eq!(e.dwords, 0x40);
        assert!(e.subroutine && e.sync_wait && e.conditional);
        // nv_copy's encoding round-trips.
        let (lo, hi) = crate::nv_copy::GpEntry { va: 0x7f_dead_b000, dwords: 7 }.encode();
        let d = gp_entry(lo, hi);
        assert_eq!((d.va, d.dwords), (0x7f_dead_b000, 7));
    }

    #[test]
    fn window_and_segments() {
        assert_eq!(window_first(10, 1024), 4);
        assert_eq!(window_first(2, 1024), 1020);
        assert_eq!(window_first(0, 4), 1);
        let seg = |va: u64| GpEntry { va, dwords: 16, ..GpEntry::default() };
        let mut w = [GpEntry::default(); WINDOW];
        for (i, e) in w.iter_mut().enumerate() {
            *e = seg(0x1000 * (i as u64 + 1));
        }
        // gp_get at window position 6; idle (get == put): positions 0..=5, the last 6.
        let (p, n) = pick_segments(&w, 100, 1024, 106, 106, 0);
        assert_eq!(&p[..n], &[0, 1, 2, 3, 4, 5]);
        // Busy (get != put): GP_GET's own entry is included, the oldest dropped.
        let (p, n) = pick_segments(&w, 100, 1024, 106, 107, 0);
        assert_eq!(&p[..n], &[1, 2, 3, 4, 5, 6]);
        // A control entry is skipped; the PB GET segment (position 0) comes back.
        w[3].dwords = 0;
        let (p, n) = pick_segments(&w, 100, 1024, 106, 107, 0x1008);
        assert_eq!(&p[..n], &[0, 1, 2, 4, 5, 6]);
        // Ring wrap: first 1020, get 2 -> position 6.
        let (p, n) = pick_segments(&w, 1020, 1024, 2, 2, 0);
        assert_eq!(&p[..n], &[0, 1, 2, 4, 5]);
    }

    #[test]
    fn host_acquire_and_release() {
        let mut s = Scanner::new(DEFAULT_CLASSES);
        s.begin_segment(u32::MAX);
        feed_all(
            &mut s,
            &[
                0,
                inc(0, 0x5c, 5),
                0x89ab_cdec,
                0x12,
                7,
                0,
                (1 << 24) | 3, // ACQ_CIRC_GEQ, 64-bit
                inc(0, 0x5c, 5),
                0x1000,
                0x1,
                9,
                0,
                1, // RELEASE
            ],
        );
        let sems = s.sems();
        assert_eq!(sems.len(), 2);
        assert_eq!(sems[0].kind, sem_kind::HOST);
        assert!(sems[0].acquire());
        assert_eq!(sems[0].addr, 0x12_89ab_cdec);
        assert_eq!(sems[0].payload, 7);
        assert_eq!(sems[0].at, 6);
        assert!(!sems[1].acquire());
        assert_eq!(sems[1].addr, 0x1_0000_1000);
    }

    #[test]
    fn gr_semaphores_and_immediate() {
        let mut s = Scanner::new(DEFAULT_CLASSES);
        s.begin_segment(u32::MAX);
        feed_all(
            &mut s,
            &[
                inc(0, 0x158, 5),
                5,
                0,
                0x2000,
                0x3,
                (1 << 27) | 1, // ACQUIRE, 64-bit payload
                inc(1, 0x1b00, 4),
                0x4,
                0x3000,
                11,
                0, // RELEASE
                inc(0, 0x158, 5),
                1,
                0,
                0,
                0,
                2, // REPORT_ONLY: not recorded
            ],
        );
        let sems = s.sems();
        assert_eq!(sems.len(), 2);
        assert_eq!((sems[0].kind, sems[0].addr, sems[0].payload), (sem_kind::GR_EXEC, 0x3_0000_2000, 5));
        assert!(sems[0].acquire());
        assert_eq!((sems[1].kind, sems[1].addr, sems[1].payload), (sem_kind::GR_LEGACY, 0x4_0000_3000, 11));
        assert_eq!(sems[1].class, 0xcec0);
        assert_eq!(s.sems_seen, 2);
    }

    #[test]
    fn mme_dma_and_pairs() {
        let mut s = Scanner::new(DEFAULT_CLASSES);
        s.begin_segment(u32::MAX);
        feed_all(&mut s, &[inc(0, 0x550, 2), 0x7f, 0xdead_0000]);
        // The address carries over into the next segment.
        s.begin_segment(u32::MAX);
        let immd_read = (4 << 29) | (6 << 16) | (0x55c >> 2); // MME_DMA_READ, immediate: 6 dwords
        feed_all(
            &mut s,
            &[
                immd_read,
                inc(0, 0x560, 1),
                12,
                inc(0, 0x2380, 3),
                0x100,
                0x5,
                0xbeef_0000,
                inc(0, 0x3800, 2),
                1,
                2,
                inc(1, 0x2b4, 1),
                0x1234_5600,
            ],
        );
        let refs = s.refs();
        assert_eq!(refs.len(), 4);
        assert_eq!((refs[0].tag, refs[0].va, refs[0].len), (tag::MME_READ, 0x7f_dead_0000, 6));
        assert_eq!(refs[0].flags, REF_HI_SEEN);
        assert_eq!((refs[1].tag, refs[1].len), (tag::MME_READ_FIFOED, 12));
        assert_eq!((refs[2].tag, refs[2].va, refs[2].len), (tag::CONSTANT_BUFFER, 0x5_beef_0000, 0x100));
        assert_eq!((refs[3].tag, refs[3].va), (tag::QMD, 0x12_3456_0000));
        assert_eq!(s.mme_calls, 1);
    }

    #[test]
    fn set_object_and_copy_release() {
        let mut s = Scanner::new([0; 8]);
        s.begin_segment(u32::MAX);
        feed_all(
            &mut s,
            &[
                inc(4, 0, 1),
                0xcab5,
                inc(4, 0x240, 4),
                0x1,
                0x40,
                3,
                0,
                inc(4, 0x300, 1),
                1 << 3,
            ],
        );
        assert_eq!(s.classes[4], 0xcab5);
        let sems = s.sems();
        assert_eq!(sems.len(), 1);
        assert_eq!((sems[0].kind, sems[0].addr, sems[0].payload), (sem_kind::COPY, 0x1_0000_0040, 3));
    }

    #[test]
    fn noninc_oneinc_and_tail_alignment() {
        let mut s = Scanner::new(DEFAULT_CLASSES);
        s.begin_segment(4);
        // NONINC 3 to 0x5c: all three land on SEM_ADDR_LO; ONE_INC to 0x64: 7 -> PAYLOAD_LO,
        // the rest -> PAYLOAD_HI.
        let noninc = (3 << 29) | (3 << 16) | (0x5c >> 2);
        let oneinc = (5 << 29) | (3 << 16) | (0x64 >> 2);
        feed_all(&mut s, &[noninc, 1, 2, 0x100, oneinc, 7, 8, 9, inc(0, 0x6c, 1), 0]);
        assert_eq!(s.tail_aligned, 4);
        let sem = s.sems()[0];
        assert_eq!(sem.addr, 0x100);
        assert_eq!(sem.payload, 7);
        assert_eq!(s.host, [0x100, 0, 7, 9]);
    }

    #[test]
    fn lost_and_end() {
        let mut s = Scanner::new(DEFAULT_CLASSES);
        s.begin_segment(u32::MAX);
        assert!(!s.feed(6 << 29));
        assert_eq!(s.lost_at, Some(0));
        s.begin_segment(u32::MAX);
        assert!(s.feed(0));
        assert!(!s.feed(7 << 29));
        assert!(s.ended);
        assert!(!s.feed(0));
    }

    #[test]
    fn sem_cap_keeps_acquires() {
        let mut s = Scanner::new(DEFAULT_CLASSES);
        s.begin_segment(u32::MAX);
        for _ in 0..SEM_CAP {
            feed_all(&mut s, &[inc(0, 0x6c, 1), 1]);
        }
        feed_all(&mut s, &[inc(0, 0x6c, 1), 0]);
        assert_eq!(s.sems_seen, SEM_CAP as u32 + 1);
        assert!(s.sems().iter().any(|x| x.acquire()));
    }

    #[test]
    fn stall_watch() {
        let t = Some(3_000 * 10_000u64);
        let ms = |n: u64| n * 10_000;
        let mut w = StallWatch::new();
        let s = |now: u64, busy: bool, progress: u64, gp_get: u32| StallSample {
            now,
            busy,
            progress,
            gp_get,
            error: false,
        };
        assert_eq!(w.tick(s(ms(0), true, 5, 1), t), Trigger::None);
        assert_eq!(w.tick(s(ms(2_000), true, 5, 1), t), Trigger::None);
        assert_eq!(w.stalled_ms(ms(2_000)), 2_000);
        assert_eq!(w.tick(s(ms(3_000), true, 5, 1), t), Trigger::Stall);
        assert_eq!(w.tick(s(ms(9_000), true, 5, 1), t), Trigger::None);
        // Progress re-arms.
        assert_eq!(w.tick(s(ms(9_500), true, 6, 1), t), Trigger::None);
        assert_eq!(w.tick(s(ms(12_500), true, 6, 1), t), Trigger::Stall);
        // Idle clears.
        assert_eq!(w.tick(s(ms(13_000), false, 6, 1), t), Trigger::None);
        assert_eq!(w.stalled_ms(ms(20_000)), 0);
        // Off.
        assert_eq!(w.tick(s(ms(14_000), true, 6, 1), None), Trigger::None);
        assert_eq!(w.tick(s(ms(99_000), true, 6, 1), None), Trigger::None);
        // An error once.
        let mut e = s(ms(100_000), true, 6, 1);
        e.error = true;
        assert_eq!(w.tick(e, t), Trigger::Error);
        assert_eq!(w.tick(e, None), Trigger::None);
    }

    /// A whole snapshot through the same writers the KMD uses: the record sizes match the
    /// documented layout, and with `NSNP_FIXTURE=<path>` the blob is written there for
    /// `snapdec.py` to decode.
    #[test]
    fn fixture_blob() {
        extern crate std;
        let mut buf = [0u8; 16 * 1024];
        let mut store = [VaEntry::default(); 16];
        let mut table = VaTable::new(&mut store);
        let mut w = Writer::new(&mut buf);
        let token = (1 << 30) | (2 << 16) | 0x37;
        write_header(&mut w, 1, reason::AUTO, token, 50_000_000, 3000, 10_000, 1234, 99);
        let mut userd = [0u32; USERD_DWORDS];
        userd[0x88 / 4] = 11;
        userd[0x8c / 4] = 12;
        userd[0x44 / 4] = 0x0010_0040;
        userd[0x60 / 4] = 0x1;
        let mut window = [(0u32, 0u32); WINDOW];
        for (i, e) in window.iter_mut().enumerate() {
            let va = 0x1_0010_0000u64 + 0x1000 * i as u64;
            *e = crate::nv_copy::GpEntry { va, dwords: 32 }.encode();
        }
        let q = QueueFields {
            slot: 3,
            node: 0,
            pid: 4242,
            flags: Q_GRAPHICS | Q_OUTSTANDING | Q_DETAIL | Q_TRIGGER,
            token,
            hw_tsg: 0x1c,
            process: 0xffff_8000_1234_0000,
            gpfifo_va: 0x1_0000_0000,
            ring_entries: 1024,
            kmd_gp_put: 12,
            gp_get: 11,
            userd_gp_put: 12,
            submitted: 77,
            signalled: 76,
            fence_now: 76,
            fence_gpu: 0x2_0000_0000,
            stall_ms: 3100,
            window_first: window_first(11, 1024),
            window_n: WINDOW as u32,
            ..QueueFields::default()
        };
        let mark = begin_queue(&mut w, &q, &userd, &window);
        let qrec = w.len() - mark.start;
        assert_eq!(qrec, QREC_FIXED);
        // One segment: an MME DMA read of an address, a host acquire, a release.
        let inc = |sc: u32, m: u32, n: u32| (1 << 29) | (n << 16) | (sc << 13) | (m >> 2);
        let words = [
            inc(0, 0x550, 2),
            0x3,
            0x4560_0000,
            inc(0, 0x560, 1),
            16,
            inc(0, 0x3800, 1),
            0,
            inc(0, 0x5c, 5),
            0x0,
            0x2,
            5,
            0,
            3,
            inc(0, 0x168, 1),
            0,
        ];
        let mut sc = Scanner::new(DEFAULT_CLASSES);
        sc.begin_segment(0);
        for d in words {
            sc.feed(d);
        }
        let seg_va = 0x1_0010_0000u64 + 0x1000 * 4;
        let seg_index = table.intern(q.process, seg_va).unwrap().0;
        let f = SegFields {
            index: 10,
            va: seg_va,
            len: words.len() as u32,
            scanned: words.len() as u32,
            lost_at: u32::MAX,
            sems_seen: sc.sems_seen,
            n_sems: sc.sems().len() as u32,
            tail_aligned: sc.tail_aligned,
            n_head: words.len() as u32,
            n_tail: words.len() as u32,
            mme_calls: sc.mme_calls,
            refs_seen: sc.refs_seen,
            n_refs: sc.refs().len() as u32,
            va_index: seg_index,
            flags: S_BEFORE_GP_GET | S_CONTAINS_PB_GET,
            ..SegFields::default()
        };
        let sm = begin_segment(&mut w, &f);
        assert_eq!(w.len() - sm.start, SEG_FIXED);
        for d in words.iter().chain(words.iter()) {
            w.u32(*d);
        }
        for s in sc.sems() {
            let i = table.intern(q.process, s.addr).unwrap().0;
            write_sem(&mut w, s, i);
        }
        for r in sc.refs() {
            let i = table.intern(q.process, r.va).unwrap().0;
            write_ref(&mut w, r, i);
        }
        end_segment(&mut w, sm);
        end_queue(&mut w, mark, 1);
        // Resolve: the acquire's semaphore holds 4 (wait for 5), the MME address is unmapped.
        for e in table.entries[..table.n].iter_mut() {
            if e.va == 0x2_0000_0000 {
                e.hit = HIT_FOREIGN;
                e.piece_va = e.va;
                e.piece_len = 0x1000;
                e.value = 4;
                e.value_status = RD_OK;
            } else if e.va == seg_va {
                e.hit = HIT_NATIVE_SYSTEM;
                e.id = 9;
                e.flags = V_RESIDENT_KNOWN;
                e.resident_pa = 0x1234_5000;
                e.value_status = RD_OK;
            } else {
                e.hit = HIT_NONE;
                e.value_status = RD_NOT_MAPPED;
                e.ring_kind = K_UNMAP_FOR_TEST;
                e.ring_t = 40_000_000;
                e.ring_va = 0x3_4560_0000;
                e.ring_len = 0x10000;
            }
        }
        write_va_table(&mut w, table.used(), true);
        finish_header(&mut w, 1, 321, 0);
        assert!(!w.overflowed());
        let len = w.len();
        if let Some(path) = std::env::var_os("NSNP_FIXTURE") {
            std::fs::write(path, &buf[..len]).unwrap();
        }
    }

    const K_UNMAP_FOR_TEST: u32 = crate::pt_ring::K_UNMAP as u32;

    #[test]
    fn writer_and_layout() {
        let mut buf = [0u8; 4096];
        let mut w = Writer::new(&mut buf);
        write_header(&mut w, 3, reason::MANUAL, 0x37, 0x1122, 3000, 10_000, 5, 6);
        assert_eq!(w.len(), HDR_BYTES);
        let mut store = [VaEntry::default(); 4];
        let mut t = VaTable::new(&mut store);
        let (i, new) = t.intern(1, 0x1000).unwrap();
        assert!(new && i == 0);
        assert_eq!(t.intern(1, 0x1000), Some((0, false)));
        assert_eq!(t.intern(2, 0x1000), Some((1, true)));
        let at = w.len();
        write_va_table(&mut w, t.used(), true);
        assert_eq!(w.len() - at, VA_FIXED + 2 * VA_BYTES);
        let at = w.len();
        write_sem(&mut w, &SemOp::default(), 0);
        assert_eq!(w.len() - at, SEM_BYTES);
        let at = w.len();
        write_ref(&mut w, &AddrRef::default(), 0);
        assert_eq!(w.len() - at, REF_BYTES);
        finish_header(&mut w, 0, 7, FLAG_BUDGET);
        let total = w.len() as u32;
        assert_eq!(&buf[0..4], b"NSNP");
        assert_eq!(u32::from_le_bytes(buf[12..16].try_into().unwrap()), total);
        assert_eq!(u32::from_le_bytes(buf[52..56].try_into().unwrap()), FLAG_BUDGET);
        // Overflow is remembered.
        let mut small = [0u8; 3];
        let mut w = Writer::new(&mut small);
        w.u32(1);
        assert!(w.overflowed());
        // The table refuses past its size.
        let mut store = [VaEntry::default(); 1];
        let mut t = VaTable::new(&mut store);
        assert!(t.intern(0, 1).is_some());
        assert!(t.intern(0, 2).is_none());
        assert!(t.full);
    }
}

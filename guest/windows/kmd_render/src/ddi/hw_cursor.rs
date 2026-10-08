//! Hardware cursor (`HwCursor`, default 1): the I/O half. The pure half (which shapes, the
//! conversion to premultiplied ARGB, the caps rule, the slot layout) is
//! `helios_kmd_logic::hw_cursor`; the design, the transitions and the test recipe are
//! `docs/independent-flip.md` section 12, the host side `docs/SCANOUT.md` "Hardware cursor,
//! Windows guests".
//!
//! WHY. Without a hardware pointer dxgkrnl has DWM draw the cursor into the frames DWM composes.
//! Under independent flip DWM composes nothing: the application's own buffer is scanned out, and
//! the cursor is gone. With a hardware pointer nobody draws it into any frame; the host shows it
//! as the host pointer's image (the viewer's `wl_pointer` cursor, as for a Linux guest's cursor
//! plane), so it is there whatever path the frames take, and moves at host rate with no guest
//! round trip.
//!
//! WHAT. `DXGK_DRIVERCAPS` reports a 256x256 monochrome / color / masked-color pointer
//! ([`advertised`]). `DxgkDdiSetPointerShape` converts the shape into one of two slots of a
//! KMD-owned linear Venus image (256 x 512, host-visible, exported to the host like a primary),
//! the slot not on screen, and names it to the host with `HELIOS_CMD_SET_CURSOR_BLOB`.
//! `DxgkDdiSetPointerPosition` costs one spinlock unless the visibility changes; then it shows or
//! hides the image. A position never travels: the host pointer is where the cursor is.
//!
//! FAILURE. Any shape the host cannot take, a blob that cannot be made, or a host that refuses
//! the command answers `STATUS_UNSUCCESSFUL`, and dxgkrnl draws that shape in software, as
//! before; the host image is hidden first so two cursors never show. A host that REFUSES the
//! command (an old backend; `CurHostErr`) is not asked again in this generation. A command that
//! only TIMED OUT is not a refusal (`hc::after_failure`): it is on the in-order control queue
//! behind the Venus traffic and the host runs it late, so it counts as taken (`CurTmo`) and the
//! host image stays. Falling back to software for it hid a working host cursor and left the
//! pointer to DWM's frames, which froze it for as long as the backend was busy. A command that
//! never reached the queue keeps the host image as it is and is sent again (`CurRetry`).
//!
//! GENERATIONS. Everything is per transport generation (`reset_for_start`): the blob belongs to
//! the Venus client of one generation and its id may name another resource in the next. dxgkrnl
//! sets the shape again after a restart or a mode change; until then the host shows none (the
//! host hides the cursor when the device resets).
//!
//! LOCKING. `STATE` is a leaf spinlock over plain data, never held across a host round trip or a
//! mapping. `BUSY` lets one pointer operation at a time do I/O; a position call that finds it
//! taken only records the visibility, and the holder applies it before it lets go
//! ([`settle`]). Both DDIs are PASSIVE.

use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};

use bytemuck::Zeroable;

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::gpu::OwnerFilter;
use crate::virtio::VirtioError;
use helios_kmd_logic::hw_cursor::{self as hc, AfterFailure, HostFailure, Refuse, Shape, Slots};
use helios_protocol::{
    HeliosSetCursorBlob, HELIOS_CURSOR_BLOB_F_VISIBLE, VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::PHYSICAL_ADDRESS;

// The pure half restates the host's bits (it has no protocol dependency); they must agree.
const _: () = assert!(hc::CFG_CURSOR == helios_protocol::NVGPU_CFG_CURSOR);
const _: () = assert!(hc::CFG_VENUS_CURSOR == helios_protocol::NVGPU_CFG_VENUS_CURSOR);
const _: () = assert!(hc::CFG_VENUS == helios_kmd_logic::guest_blob::contract::CFG_VENUS);

/// How long the host gets for one cursor command (it runs inside a pointer DDI).
const HOST_TIMEOUT_MS: u64 = 500;
/// How long a shape waits for another pointer operation's I/O.
const BUSY_WAIT_MS: u64 = 50;

// ---- counters (names at most 14 characters; `hc::COUNTERS`) ---------------------------
//
// `CurKnob` the knob, `CurCaps` 1 when the caps report the pointer, `CurShapeN` / `CurPosN`
// SetPointerShape / SetPointerPosition calls, `CurShow` / `CurHide` show / hide commands the host
// took, `CurFmt` the last shape's kind (1 mono, 2 color, 4 masked; +0x100 premultiplied here),
// `CurSize` its `width << 16 | height`, `CurRefuse` shapes left to the software cursor,
// `CurWhy` the last reason (`hc::Refuse::code`), `CurHostErr` failed host commands, `CurXor`
// the last shape's inverting pixels. Written as zeros at every StartDevice, then from the
// periodic `scanout_trace` dump when one moved.

static KNOB: AtomicU32 = AtomicU32::new(0);
static CAPS: AtomicU32 = AtomicU32::new(0);
static SHAPE_N: AtomicU32 = AtomicU32::new(0);
static POS_N: AtomicU32 = AtomicU32::new(0);
static SHOW: AtomicU32 = AtomicU32::new(0);
static HIDE: AtomicU32 = AtomicU32::new(0);
static FMT: AtomicU32 = AtomicU32::new(0);
static SIZE: AtomicU32 = AtomicU32::new(0);
static REFUSE: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static HOST_ERR: AtomicU32 = AtomicU32::new(0);
static XOR: AtomicU32 = AtomicU32::new(0);
static RTT_US: AtomicU32 = AtomicU32::new(0);
static RTT_MAX: AtomicU32 = AtomicU32::new(0);
static TMO: AtomicU32 = AtomicU32::new(0);
static GATE_MS: AtomicU32 = AtomicU32::new(0);
static SW_N: AtomicU32 = AtomicU32::new(0);
static SW_MS: AtomicU32 = AtomicU32::new(0);
static SW_MAX: AtomicU32 = AtomicU32::new(0);
static RETRY: AtomicU32 = AtomicU32::new(0);
/// When the current software-cursor episode began (0: none).
static SW_SINCE: AtomicU64 = AtomicU64::new(0);
/// When an owed command last failed to reach the queue (0: nothing to retry).
static NOT_SENT_AT: AtomicU64 = AtomicU64::new(0);
static DIRTY: AtomicU32 = AtomicU32::new(0);

fn now_100ns() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

fn ms_from_100ns(t: u64) -> u32 {
    (t / 10_000).min(u64::from(u32::MAX)) as u32
}

fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

fn set(c: &AtomicU32, v: u32) {
    c.store(v, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

fn write_block() {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"CurKnob", r(&KNOB));
    crate::diag::record_named_bytes(b"CurCaps", r(&CAPS));
    crate::diag::record_named_bytes(b"CurShapeN", r(&SHAPE_N));
    crate::diag::record_named_bytes(b"CurPosN", r(&POS_N));
    crate::diag::record_named_bytes(b"CurShow", r(&SHOW));
    crate::diag::record_named_bytes(b"CurHide", r(&HIDE));
    crate::diag::record_named_bytes(b"CurFmt", r(&FMT));
    crate::diag::record_named_bytes(b"CurSize", r(&SIZE));
    crate::diag::record_named_bytes(b"CurRefuse", r(&REFUSE));
    crate::diag::record_named_bytes(b"CurWhy", r(&WHY));
    crate::diag::record_named_bytes(b"CurHostErr", r(&HOST_ERR));
    crate::diag::record_named_bytes(b"CurXor", r(&XOR));
    crate::diag::record_named_bytes(b"CurRttUs", r(&RTT_US));
    crate::diag::record_named_bytes(b"CurRttMax", r(&RTT_MAX));
    crate::diag::record_named_bytes(b"CurTmo", r(&TMO));
    crate::diag::record_named_bytes(b"CurGateMs", r(&GATE_MS));
    crate::diag::record_named_bytes(b"CurSwN", r(&SW_N));
    crate::diag::record_named_bytes(b"CurSwMs", r(&SW_MS));
    crate::diag::record_named_bytes(b"CurSwMax", r(&SW_MAX));
    crate::diag::record_named_bytes(b"CurRetry", r(&RETRY));
}

/// From the periodic `scanout_trace` dump: the block, when a count moved. PASSIVE.
pub(crate) fn publish() {
    if DIRTY.swap(0, Ordering::Relaxed) != 0 {
        write_block();
    }
}

// ---- state ----------------------------------------------------------------------------

/// The cursor image of one generation.
#[derive(Clone, Copy)]
struct Blob {
    serial: u64,
    res_id: u32,
    slots: Slots,
}

struct State {
    blob: Option<Blob>,
    /// The shape last written, and its slot.
    shape: Option<(Shape, u32)>,
    /// The shape changed since the host was last told.
    shape_dirty: bool,
    /// What dxgkrnl last said (`SetPointerPosition.Flags.Visible`).
    visible: bool,
    /// What the host shows.
    host_shown: bool,
    /// The host refused the command: the software cursor for the rest of the generation.
    host_refused: bool,
    x: i32,
    y: i32,
}

impl State {
    const NEW: State = State {
        blob: None,
        shape: None,
        shape_dirty: false,
        visible: false,
        host_shown: false,
        host_refused: false,
        x: 0,
        y: 0,
    };

    /// Whether the host should show the cursor now.
    fn want(&self) -> bool {
        self.visible && self.shape.is_some() && self.blob.is_some() && !self.host_refused
    }

    /// The command that brings the host to [`Self::want`], if one is owed.
    fn owed(&self) -> Option<HeliosSetCursorBlob> {
        let want = self.want();
        if want == self.host_shown && !(want && self.shape_dirty) {
            return None;
        }
        let mut c = HeliosSetCursorBlob::zeroed();
        c.x = self.x;
        c.y = self.y;
        if let (true, Some((s, slot)), Some(b)) = (want, self.shape, self.blob) {
            c.resource_id = b.res_id;
            c.width = s.width;
            c.height = s.height;
            c.format = VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM;
            c.stride = b.slots.row_pitch;
            c.offset = b.slots.offset(slot);
            c.hot_x = s.hot_x;
            c.hot_y = s.hot_y;
            c.flags = HELIOS_CURSOR_BLOB_F_VISIBLE;
        }
        Some(c)
    }
}

static STATE: SpinLock<State> = SpinLock::new(State::NEW);
static BUSY: AtomicBool = AtomicBool::new(false);

/// A new generation: the knob, the caps, the counters zeroed and written, the state forgotten.
/// StartDevice, PASSIVE, after the transport is up.
pub(crate) fn reset_for_start(adapter: &AdapterContext, knobs: &crate::adapter::AdapterKnobs) {
    *STATE.lock() = State::NEW;
    KNOB.store(knobs.hw_cursor, Ordering::Relaxed);
    let caps = hc::advertise(knobs.hw_cursor, knobs.display_half, host_features(adapter));
    CAPS.store(u32::from(caps), Ordering::Relaxed);
    for c in [
        &SHAPE_N, &POS_N, &SHOW, &HIDE, &FMT, &SIZE, &REFUSE, &WHY, &HOST_ERR, &XOR, &RTT_US,
        &RTT_MAX, &TMO, &GATE_MS, &SW_N, &SW_MS, &SW_MAX, &RETRY,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    SW_SINCE.store(0, Ordering::Relaxed);
    NOT_SENT_AT.store(0, Ordering::Relaxed);
    DIRTY.store(0, Ordering::Relaxed);
    write_block();
}

fn host_features(adapter: &AdapterContext) -> Option<u32> {
    adapter.with_virtio(|v| v.nvrm_device_features()).ok()
}

/// Whether `DXGK_DRIVERCAPS` reports the pointer (`query_driver_caps`). Any IRQL the caps query
/// runs at (PASSIVE). Recorded, so the pointer DDIs follow what dxgkrnl was told.
pub(crate) fn advertised(adapter: &AdapterContext) -> bool {
    let knobs = adapter.knobs();
    let caps = hc::advertise(knobs.hw_cursor, knobs.display_half, host_features(adapter));
    if u32::from(caps) != CAPS.swap(u32::from(caps), Ordering::Relaxed) {
        DIRTY.store(1, Ordering::Relaxed);
    }
    caps
}

fn caps_on() -> bool {
    CAPS.load(Ordering::Relaxed) != 0
}

// ---- the DDIs -------------------------------------------------------------------------

/// `DxgkDdiSetPointerShape` with the display half on.
///
/// # Safety
/// `args` is dxgkrnl's argument, valid for the call; `pPixels` points at the shape it
/// describes. PASSIVE_LEVEL.
pub(crate) unsafe fn set_pointer_shape(
    adapter: &AdapterContext,
    args: &DXGKARG_SETPOINTERSHAPE,
) -> NTSTATUS {
    if !caps_on() {
        // No pointer was reported: the software cursor, as before this module.
        return STATUS_SUCCESS;
    }
    bump(&SHAPE_N);
    // SAFETY: PASSIVE per the DDI contract.
    let passive = unsafe { PassiveLevel::assume() };
    // SAFETY: the flags union's `Value` is its whole word.
    let flags = unsafe { args.Flags.__bindgen_anon_1.Value };
    let shape = match hc::validate(
        flags,
        args.Width,
        args.Height,
        args.Pitch,
        args.XHot,
        args.YHot,
        args.VidPnSourceId,
        !args.pPixels.is_null(),
    ) {
        Ok(s) => s,
        Err(why) => return refuse(passive, adapter, why),
    };
    if STATE.lock().host_refused {
        return refuse(passive, adapter, Refuse::Host);
    }
    // SAFETY: dxgkrnl's buffer holds the shape `validate` accepted: `src_len` bytes.
    let src = unsafe { core::slice::from_raw_parts(args.pPixels as *const u8, shape.src_len()) };
    let Some(_gate) = Gate::wait(passive) else {
        return refuse(passive, adapter, Refuse::Host);
    };
    let Some(blob) = ensure_blob(passive, adapter) else {
        drop(_gate);
        return refuse(passive, adapter, Refuse::Blob);
    };
    let slot = {
        let st = STATE.lock();
        st.shape.map_or(0, |(_, s)| Slots::next(s))
    };
    let Some(xor) = write_shape(passive, adapter, &blob, slot, &shape, src) else {
        drop(_gate);
        return refuse(passive, adapter, Refuse::Blob);
    };
    set(
        &FMT,
        shape.kind.code()
            | if hc::needs_premultiply(&shape, src) {
                0x100
            } else {
                0
            },
    );
    set(&SIZE, shape.width << 16 | shape.height);
    set(&XOR, xor);
    {
        let mut st = STATE.lock();
        st.shape = Some((shape, slot));
        st.shape_dirty = true;
    }
    let ok = settle(passive, adapter);
    drop(_gate);
    after_gate(passive, adapter);
    if ok {
        STATUS_SUCCESS
    } else {
        refuse(passive, adapter, Refuse::Host)
    }
}

/// `DxgkDdiSetPointerPosition` with the display half on. Always succeeds (failure is not in its
/// legal set): a visibility the host did not take leaves the host image hidden.
///
/// # Safety
/// `args` is dxgkrnl's argument, valid for the call. PASSIVE_LEVEL.
pub(crate) unsafe fn set_pointer_position(
    adapter: &AdapterContext,
    args: &DXGKARG_SETPOINTERPOSITION,
) -> NTSTATUS {
    if !caps_on() || args.VidPnSourceId != 0 {
        return STATUS_SUCCESS;
    }
    POS_N.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the flags union's `Value` is its whole word.
    let visible = unsafe { args.Flags.__bindgen_anon_1.Value } & 1 != 0;
    let changed = {
        let mut st = STATE.lock();
        if visible {
            st.x = args.X;
            st.y = args.Y;
        }
        let changed = st.visible != visible;
        st.visible = visible;
        changed
    };
    if !changed {
        // A command that never reached the queue is owed: try again, at most every 250 ms (this
        // runs at mouse rate), and only when no other pointer operation does I/O.
        let failed = NOT_SENT_AT.load(Ordering::Relaxed);
        if failed != 0 && hc::retry_due(now_100ns(), failed) {
            if let Some(gate) = Gate::try_take() {
                NOT_SENT_AT.store(0, Ordering::Relaxed);
                bump(&RETRY);
                // SAFETY: PASSIVE per the DDI contract.
                let passive = unsafe { PassiveLevel::assume() };
                let _ = settle(passive, adapter);
                drop(gate);
            }
        }
        return STATUS_SUCCESS;
    }
    DIRTY.store(1, Ordering::Relaxed);
    // SAFETY: PASSIVE per the DDI contract.
    let passive = unsafe { PassiveLevel::assume() };
    // Taken: its holder settles before it lets go.
    if let Some(gate) = Gate::try_take() {
        let _ = settle(passive, adapter);
        drop(gate);
        after_gate(passive, adapter);
    }
    STATUS_SUCCESS
}

/// Leave the shape to the software cursor: hide the host image first (best effort), count.
fn refuse(passive: PassiveLevel, adapter: &AdapterContext, why: Refuse) -> NTSTATUS {
    bump(&REFUSE);
    set(&WHY, why.code());
    // A software-cursor episode starts (it ends when the host shows a shape again).
    if SW_SINCE
        .compare_exchange(0, now_100ns().max(1), Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        bump(&SW_N);
    }
    let shown = {
        let mut st = STATE.lock();
        st.shape = None;
        st.shape_dirty = false;
        st.host_shown
    };
    if shown {
        if let Some(gate) = Gate::try_take() {
            let _ = settle(passive, adapter);
            drop(gate);
        }
    }
    STATUS_UNSUCCESSFUL
}

// ---- I/O ------------------------------------------------------------------------------

/// The right to do pointer I/O.
struct Gate;

impl Gate {
    fn try_take() -> Option<Gate> {
        BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Gate)
    }

    fn wait(passive: PassiveLevel) -> Option<Gate> {
        let t0 = now_100ns();
        let note = || {
            let ms = ms_from_100ns(now_100ns().saturating_sub(t0));
            if ms > GATE_MS.fetch_max(ms, Ordering::Relaxed) {
                DIRTY.store(1, Ordering::Relaxed);
            }
        };
        for _ in 0..BUSY_WAIT_MS {
            if let Some(g) = Self::try_take() {
                note();
                return Some(g);
            }
            crate::virtio::ctrl::sleep_ms(passive, 1);
        }
        note();
        Self::try_take()
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

/// A position call may have changed the visibility while the gate was held and found it taken:
/// settle once more if something is owed and nobody else holds the gate.
fn after_gate(passive: PassiveLevel, adapter: &AdapterContext) {
    for _ in 0..4 {
        // A command that could not be queued is retried later, rate limited, not here.
        if STATE.lock().owed().is_none() || NOT_SENT_AT.load(Ordering::Relaxed) != 0 {
            return;
        }
        let Some(gate) = Gate::try_take() else {
            return;
        };
        let _ = settle(passive, adapter);
        drop(gate);
    }
}

/// Bring the host to what `STATE` wants, with the gate held. False when the host did not take a
/// command (it then shows nothing of ours, as far as this side can tell).
fn settle(passive: PassiveLevel, adapter: &AdapterContext) -> bool {
    for _ in 0..4 {
        let Some(cmd) = STATE.lock().owed() else {
            return true;
        };
        let show = cmd.flags & HELIOS_CURSOR_BLOB_F_VISIBLE != 0;
        let t0 = now_100ns();
        let result = crate::virtio::ctrl::set_cursor_blob(passive, adapter, cmd, HOST_TIMEOUT_MS);
        let t1 = now_100ns();
        let rtt = (t1.saturating_sub(t0) / 10).min(u64::from(u32::MAX)) as u32;
        let failure = match result {
            Ok(()) => None,
            Err(VirtioError::Timeout) => Some(HostFailure::Late),
            // An answer with an error, or the transport gone: the host will not show it.
            Err(VirtioError::DeviceError) => Some(HostFailure::Refused),
            Err(_) => Some(HostFailure::NotSent),
        };
        let mut st = STATE.lock();
        match failure.map(hc::after_failure) {
            None | Some(AfterFailure::AssumeTaken) => {
                st.host_shown = show;
                if show {
                    st.shape_dirty = false;
                }
                drop(st);
                NOT_SENT_AT.store(0, Ordering::Relaxed);
                if failure.is_some() {
                    bump(&TMO);
                } else {
                    RTT_US.store(rtt, Ordering::Relaxed);
                    RTT_MAX.fetch_max(rtt, Ordering::Relaxed);
                }
                bump(if show { &SHOW } else { &HIDE });
                if show {
                    // The host shows a shape again: a software-cursor episode, if any, ends.
                    let since = SW_SINCE.swap(0, Ordering::Relaxed);
                    if since != 0 {
                        let ms = ms_from_100ns(t1.saturating_sub(since));
                        SW_MS.store(ms, Ordering::Relaxed);
                        SW_MAX.fetch_max(ms, Ordering::Relaxed);
                    }
                }
            }
            Some(AfterFailure::RetryLater) => {
                // Never queued: the host image stays as it was; the command stays owed and a
                // later pointer call sends it (`set_pointer_position`, rate limited).
                drop(st);
                NOT_SENT_AT.store(t1.max(1), Ordering::Relaxed);
                bump(&HOST_ERR);
                return true;
            }
            Some(AfterFailure::SoftwareCursor) => {
                // A refusal is an answer (an old backend): do not ask again this generation.
                st.host_refused = true;
                st.shape_dirty = false;
                if !show {
                    // A hide the host did not take: assume nothing of ours is shown from here.
                    st.host_shown = false;
                }
                drop(st);
                bump(&HOST_ERR);
                return false;
            }
        }
    }
    true
}

/// The cursor image of this generation, made on first use.
fn ensure_blob(passive: PassiveLevel, adapter: &AdapterContext) -> Option<Blob> {
    let serial = adapter.transport_generation()?.serial;
    if let Some(b) = STATE.lock().blob.filter(|b| b.serial == serial) {
        return Some(b);
    }
    let made = adapter
        .with_venus_client(passive, |c| {
            c.allocate_linear_scanout_image_blob(adapter, hc::MAX_DIM, hc::MAX_DIM * hc::SLOTS)
        })
        .ok()?
        .ok()?;
    let slots = Slots::new(
        u64::from(made.plane_offset),
        u64::from(made.row_pitch),
        made.blob.size,
    )?;
    let blob = Blob {
        serial,
        res_id: made.blob.res_id,
        slots,
    };
    let mut st = STATE.lock();
    st.blob = Some(blob);
    st.shape = None;
    st.host_shown = false;
    Some(blob)
}

/// Convert `shape` into slot `slot` of the image. Returns the inverting pixels, `None` when the
/// image could not be mapped.
fn write_shape(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    blob: &Blob,
    slot: u32,
    shape: &Shape,
    src: &[u8],
) -> Option<u32> {
    let prep = crate::virtio::ctrl::map_blob_prepare(
        passive,
        adapter,
        OwnerFilter::Exactly(None),
        blob.res_id,
    )
    .ok()?;
    let base = u64::from(blob.slots.offset(slot));
    let end = base + u64::from(blob.slots.row_pitch) * u64::from(hc::MAX_DIM);
    if end > prep.size {
        return None;
    }
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = prep.gpa as i64;
    let cache = crate::ddi::map_cache_to_mm(prep.map_cache);
    // SAFETY: PASSIVE; the range was RESOURCE_MAP_BLOB'd into the host-visible window by
    // `map_blob_prepare`, with the host's cache attribute. Unmapped below.
    let va = unsafe { MmMapIoSpace(pa, prep.size, cache) } as *mut u8;
    if va.is_null() {
        return None;
    }
    let premul = hc::needs_premultiply(shape, src);
    let mut row = [0u32; hc::MAX_DIM as usize];
    let mut xor = 0;
    for y in 0..shape.height {
        xor += hc::convert_row(shape, src, y, premul, &mut row);
        let at = base + u64::from(y) * u64::from(blob.slots.row_pitch);
        // SAFETY: `at + width * 4 <= end <= prep.size`, inside the mapping; the row buffer is a
        // local of `MAX_DIM` pixels and `width <= MAX_DIM`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                row.as_ptr() as *const u8,
                va.add(at as usize),
                shape.width as usize * 4,
            )
        };
    }
    // The pixels are in memory before the host is told about them.
    fence(Ordering::SeqCst);
    // SAFETY: the mapping made above, exact size.
    unsafe { MmUnmapIoSpace(va as *mut core::ffi::c_void, prep.size) };
    Some(xor)
}

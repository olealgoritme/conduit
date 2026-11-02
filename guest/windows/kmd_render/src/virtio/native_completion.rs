//! Event-driven completion of native HW queues: the I/O half of
//! `helios_kmd_logic::native_completion`.
//!
//! The KMD's native RM client (`native_memory`) gets one control file per
//! interrupt source (GR non-stall, CE0..9 non-stall, host non-stall, GR
//! subdevice notification), each with `NV_ESC_ALLOC_OS_EVENT` naming itself and
//! `NV01_EVENT_OS_EVENT`s on the subdevice delivered to it. The host's event pump
//! watches every file the backend opens, so RM's `nv_post_event` on one becomes an
//! `EventReady` for its handle on the event queue. The event DPC
//! (`virtio::gpu::nvrm_events`) hands such a report to [`on_ready`] instead of the
//! registered-event table, and `ddi::interrupt::drain_used_and_complete` runs
//! `ddi::hws::event_scan` once the virtio lock is released: the progress fences
//! are read and `MONITORED_FENCE_SIGNALED` raised right then, not at the next poll
//! tick.
//!
//! Set up once per client generation at the first native CreateHwQueue (PASSIVE),
//! best effort: a source RM refuses is left out (`NvCmpSt` lists the ones that
//! came up, `NvCmpFail` the last refusal), and with none the poll alone runs as
//! before. Nothing is torn down by hand: the files are the KMD client's and go
//! with the transport's close sweep ([`forget`] drops the record).
//!
//! Knobs (service key, read at each native CreateHwQueue):
//! * `NvCompEv`: sources, bit per `Source` (default 0xF; 0 = off, the old poll).
//! * `NvHwsPollUs`: the fallback poll's period while any queue's progress is found
//!   by the poll (default 1000, 500..10000).
//! * `NvHwsPollEvUs`: its period while every queue with work outstanding is seen
//!   by events (default 4000, up to 20000).

use super::native_memory::{self, Io};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::native_completion::{self as ncomp, PollPeriods, Source};
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::rm_client as rc;

const SOURCES: usize = Source::ALL.len();

/// The backend handle of each source's event file; 0 = not set up.
static FILES: [AtomicU32; SOURCES] = [const { AtomicU32::new(0) }; SOURCES];
/// `EventReady` reports per source (`NvCmpRdy*`).
static READY: [AtomicU32; SOURCES] = [const { AtomicU32::new(0) }; SOURCES];
/// A report came and `ddi::hws::event_scan` has not run for it yet.
static PENDING: AtomicU32 = AtomicU32::new(0);
/// The client generation the setup last ran for (0 = never).
static TRIED_EPOCH: AtomicU64 = AtomicU64::new(0);
/// `NvCmpSt`: `Source::bit`s set up. `NvCmpEvts`: event objects RM made.
static STATUS: AtomicU32 = AtomicU32::new(0);
static EVENTS: AtomicU32 = AtomicU32::new(0);
/// `NvCmpFail`: the last refusal, `(source << 28) | (step << 24) | (status & 0xffffff)`.
static FAIL: AtomicU32 = AtomicU32::new(0);
/// The knobs as last read (`u32::MAX` = not read yet).
static KNOB: AtomicU32 = AtomicU32::new(u32::MAX);
static POLL_US: AtomicU32 = AtomicU32::new(0);
static POLL_EV_US: AtomicU32 = AtomicU32::new(0);

const STEP_OPEN: u32 = 1;
const STEP_OS_EVENT: u32 = 2;
const STEP_ALLOC: u32 = 3;
const STEP_NOTIFY: u32 = 4;
const STEP_CLIENT: u32 = 5;

fn fail(source: Source, step: u32, status: u32) {
    FAIL.store(
        ((source as u32) << 28) | (step << 24) | (status & 0x00ff_ffff),
        Ordering::Relaxed,
    );
}

fn rm_code(error: rc::RmError) -> u32 {
    match error {
        rc::RmError::Malformed => 0x00ff_ffff,
        rc::RmError::Host(errno) => 0x0080_0000 | (errno & 0xffff),
        rc::RmError::Status(status) => status,
    }
}

/// Read the knobs and set the sources up for the live client generation, once.
/// PASSIVE (the first native CreateHwQueue, and every later one: a no-op once
/// tried for this generation).
pub(crate) fn ensure(passive: PassiveLevel, adapter: &AdapterContext) {
    use crate::diag::{read_config_dword, KnobName};
    let knob = read_config_dword(KnobName::new(b"NvCompEv"), ncomp::KNOB_DEFAULT);
    KNOB.store(knob, Ordering::Relaxed);
    POLL_US.store(read_config_dword(KnobName::new(b"NvHwsPollUs"), 0), Ordering::Relaxed);
    POLL_EV_US.store(read_config_dword(KnobName::new(b"NvHwsPollEvUs"), 0), Ordering::Relaxed);
    if knob & 0xF == 0 {
        return;
    }
    let result = native_memory::with_io(passive, adapter, |io| {
        let epoch = io.client.epoch;
        if TRIED_EPOCH.swap(epoch, Ordering::AcqRel) == epoch {
            return Ok(());
        }
        // A new generation: the old files died with the old transport.
        for f in FILES.iter() {
            f.store(0, Ordering::Release);
        }
        STATUS.store(0, Ordering::Relaxed);
        for source in ncomp::sources(knob) {
            if let Some(file) = set_up(io, source) {
                FILES[source as usize].store(file, Ordering::Release);
                STATUS.fetch_or(source.bit(), Ordering::Relaxed);
            }
        }
        Ok(())
    });
    if result.is_err() {
        fail(Source::Gr, STEP_CLIENT, 0);
    }
    crate::diag::record_named_bytes(b"NvCmpSt", STATUS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvCmpEvts", EVENTS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvCmpFail", FAIL.load(Ordering::Relaxed));
}

/// One source: its file, the OS event on it, the event objects, and (GR
/// notification) `SET_NOTIFICATION`. The file's backend handle, or `None` (then
/// nothing of it is left but what the close sweep reclaims).
fn set_up(io: &Io<'_>, source: Source) -> Option<u32> {
    let root = io.client.root;
    let file = match io.open_file(rc::DEV_CTL) {
        Ok(file) => file,
        Err(_) => {
            fail(source, STEP_OPEN, 0);
            return None;
        }
    };
    let mut resp = [0u8; 160];
    let params = ncomp::os_event_params(root, nd::H_SUBDEVICE, file);
    let os_event = io
        .exchange(
            file,
            rc::nv_cmd(ncomp::ESC_ALLOC_OS_EVENT, ncomp::OS_EVENT_BYTES as u32),
            &params,
            &[],
            &mut resp,
        )
        .map_err(|_| rc::RmError::Malformed)
        .and_then(|n| {
            rc::rm_reply(resp.get(..n).unwrap_or(&[]), ncomp::OS_EVENT_STATUS_AT).map(|_| ())
        });
    if let Err(error) = os_event {
        fail(source, STEP_OS_EVENT, rm_code(error));
        let _ = io.close_file(file);
        return None;
    }
    let mut made = 0u32;
    let mut allocated = 0u32;
    for (i, &index) in source.notifiers().iter().enumerate() {
        let block = rc::nvos64(
            root,
            nd::H_SUBDEVICE,
            source.event_handle(i),
            ncomp::NV01_EVENT_OS_EVENT,
            ncomp::EVENT_PARAMS_BYTES as u32,
        );
        let params = ncomp::event_params(root, nd::H_SUBDEVICE, source.notify_index(index), file);
        let alloc = io
            .exchange(
                io.client.ctl,
                rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
                &block,
                &params,
                &mut resp,
            )
            .map_err(|_| rc::RmError::Malformed)
            .and_then(|n| {
                rc::rm_reply(resp.get(..n).unwrap_or(&[]), rc::NVOS64_STATUS_AT).map(|_| ())
            });
        match alloc {
            Ok(()) => {
                made += 1;
                allocated |= 1 << i;
            }
            // An engine this GPU does not have (CE indices past its last copy
            // engine): the others still count.
            Err(error) => fail(source, STEP_ALLOC, rm_code(error)),
        }
    }
    if made != 0 && !source.nonstall() {
        let params = ncomp::set_notification_params(ncomp::NOTIFY_GR0, ncomp::ACTION_REPEAT);
        let block = rc::nvos54(
            root,
            nd::H_SUBDEVICE,
            ncomp::CTRL_EVENT_SET_NOTIFICATION,
            params.len() as u32,
        );
        let control = io
            .exchange(
                io.client.ctl,
                rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
                &block,
                &params,
                &mut resp,
            )
            .map_err(|_| rc::RmError::Malformed)
            .and_then(|n| {
                rc::rm_reply(resp.get(..n).unwrap_or(&[]), rc::NVOS54_STATUS_AT).map(|_| ())
            });
        if let Err(error) = control {
            // Without it a subdevice notification is never delivered: the
            // events stay allocated (freed with the client) but the file is not
            // counted as a source.
            fail(source, STEP_NOTIFY, rm_code(error));
            free_events(io, source, allocated);
            let _ = io.close_file(file);
            return None;
        }
    }
    EVENTS.fetch_add(made, Ordering::Relaxed);
    if made == 0 {
        let _ = io.close_file(file);
        return None;
    }
    Some(file)
}

/// Free the event objects `allocated` (bit per notifier) of `source`, before
/// their file is closed. Best effort: the client's teardown frees what is left.
fn free_events(io: &Io<'_>, source: Source, allocated: u32) {
    let mut resp = [0u8; 128];
    for i in 0..source.notifiers().len() {
        if allocated & (1 << i) == 0 {
            continue;
        }
        let block = rc::nvos00(io.client.root, nd::H_SUBDEVICE, source.event_handle(i));
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
            &block,
            &[],
            &mut resp,
        );
    }
}

/// The transport (and with it the client and its files) is gone. PASSIVE.
pub(crate) fn forget() {
    for f in FILES.iter() {
        f.store(0, Ordering::Release);
    }
    TRIED_EPOCH.store(0, Ordering::Release);
    STATUS.store(0, Ordering::Relaxed);
    PENDING.store(0, Ordering::Release);
}

/// An `EventReady` for backend handle `handle` (the event DPC, under the virtio
/// lock): `true` when it is a completion file's, now owed a scan. Atomics only.
pub(crate) fn on_ready(handle: u32) -> bool {
    if handle == 0 {
        return false;
    }
    for (i, f) in FILES.iter().enumerate() {
        if f.load(Ordering::Relaxed) == handle {
            READY[i].fetch_add(1, Ordering::Relaxed);
            PENDING.store(1, Ordering::Release);
            return true;
        }
    }
    false
}

/// Whether a scan is owed (and no longer), for the caller to run it. Any IRQL.
pub(crate) fn take_pending() -> bool {
    PENDING.load(Ordering::Relaxed) != 0 && PENDING.swap(0, Ordering::AcqRel) != 0
}

/// Whether any source is up: the fallback poll may then use its long period for
/// queues the events see.
pub(crate) fn active() -> bool {
    STATUS.load(Ordering::Relaxed) != 0
}

/// The fallback poll's periods from the knobs.
pub(crate) fn periods() -> PollPeriods {
    PollPeriods::from_knobs(
        POLL_US.load(Ordering::Relaxed),
        POLL_EV_US.load(Ordering::Relaxed),
    )
}

/// Mirror the `NvCmp*` counters. PASSIVE.
pub(crate) fn publish_counters() {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvCmpKnob", r(&KNOB));
    crate::diag::record_named_bytes(b"NvCmpSt", r(&STATUS));
    crate::diag::record_named_bytes(b"NvCmpEvts", r(&EVENTS));
    crate::diag::record_named_bytes(b"NvCmpFail", r(&FAIL));
    crate::diag::record_named_bytes(b"NvCmpRdyGr", r(&READY[Source::Gr as usize]));
    crate::diag::record_named_bytes(b"NvCmpRdyCe", r(&READY[Source::Ce as usize]));
    crate::diag::record_named_bytes(b"NvCmpRdyHo", r(&READY[Source::Host as usize]));
    crate::diag::record_named_bytes(b"NvCmpRdyGn", r(&READY[Source::GrNotify as usize]));
}

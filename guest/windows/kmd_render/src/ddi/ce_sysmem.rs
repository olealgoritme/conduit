//! The copy engine's view of a CPU-visible KMD standard buffer (a GDI staging surface, a shadow)
//! while VidMm holds it in guest system pages (`RedirVram`, `docs/vram-redirection.md` 5.6, 8):
//! an `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over its lease pages, GPU-mapped in the channel, so a GDI
//! BitBlt between that buffer and a VRAM texture is ONE CE copy instead of a CPU copy through the
//! bounce buffer.
//!
//! [`with_standard`] is the whole API: under the content transaction (no paging operation can
//! change the leases while the closure runs) it finds or makes the descriptor of the buffer's
//! `[0, cover)` (whole pages of its leases, the route's page-run rules), and runs the closure with
//! the buffer as a [`CeSurface`]. The closure submits its copies and WAITS for them
//! (`ce_vram::wait`) before it returns: nothing may be in flight on the pages once the transaction
//! is released.
//!
//! Lifetime, as the route's destination descriptors (`ce_present_route`): a descriptor holds a pin
//! of exactly the leases it names, and is freed (then unpinned) BEFORE any lease change
//! ([`before_lease_change`], called from the paging path through `guest_blob`), at destroy
//! ([`destination_gone`]), at the channel's teardown ([`release_all`]), or, when a free is not
//! confirmed, leaked with its pin until the transport generation ends ([`forget`]). Because every
//! copy is waited for inside the transaction, no drain is ever needed.
//!
//! Refusals (`Err`, nothing made): not system-resident (no leases: the buffer is in segment 2, the
//! caller uses the bounce path), the system copy marked stale, a guest blob live on it, partial
//! leases, a size over one window, the channel down or busy, an RM refusal.
//!
//! LOCKING. `STATE` is a leaf spinlock over plain data; pins are dropped at PASSIVE outside it.
//! Order: content transaction -> the channel's I/O.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{Fail, FailKind};
use helios_kmd_logic::rm_vidmem as rv;

use crate::adapter::{AdapterContext, GuestPin, SystemBackingGuard};
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::rm_client::ce_route as rio;
use crate::virtio::rm_client::ce_vram::{self, CeSurface};

/// Not system-resident (or stale, or a guest blob holds it): use the bounce path.
pub(crate) const NOT_SYSTEM: Fail = Fail::new(FailKind::Refused, 0xEA);
/// The leases do not cover `[0, cover)` in whole pages, or it does not fit a window.
pub(crate) const UNCOVERED: Fail = Fail::new(FailKind::Layout, 0xEB);
/// The RM registration may be held by the host: the pages stay pinned until the generation ends.
pub(crate) const UNSURE: Fail = Fail::new(FailKind::Transport, 0xEC);

/// How long a paging hook waits for the channel's I/O before it leaves the descriptor leaked.
const IO_WAIT_MS: u64 = 250;
const FREE_MS: u32 = 1_000;
const MAKE_MS: u32 = 2_000;

struct Entry {
    resid: u32,
    slot: u8,
    va: u64,
    cover: u64,
    pitch: u32,
    width: u32,
    height: u32,
    chan_gen: u64,
    pin: GuestPin,
}

struct State {
    slots: [Option<Entry>; rv::SYS_SLOTS],
    /// Pins of descriptors whose free was not confirmed: kept until the generation ends.
    leaked: Vec<GuestPin>,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    slots: [const { None }; rv::SYS_SLOTS],
    leaked: Vec::new(),
});
/// Nonzero while any descriptor exists (the paging hooks' fast exit).
static ANY: AtomicU32 = AtomicU32::new(0);

static MADE: AtomicU32 = AtomicU32::new(0);
static HIT: AtomicU32 = AtomicU32::new(0);
static REFUSE: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static FREED: AtomicU32 = AtomicU32::new(0);
static LEAK: AtomicU32 = AtomicU32::new(0);

fn refresh_any(g: &State) {
    ANY.store(u32::from(g.slots.iter().any(Option::is_some)), Ordering::Release);
}

/// StartDevice (PASSIVE): zero the counters.
pub(crate) fn reset_for_start() {
    for c in [&MADE, &HIT, &REFUSE, &WHY, &FREED, &LEAK] {
        c.store(0, Ordering::Relaxed);
    }
}

/// Mirror the counters (PASSIVE), once a descriptor was asked for.
pub(crate) fn publish_counters() {
    if MADE.load(Ordering::Relaxed) == 0 && REFUSE.load(Ordering::Relaxed) == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"RvSysMade", MADE.load(Ordering::Relaxed));
    rec(b"RvSysHit", HIT.load(Ordering::Relaxed));
    rec(b"RvSysRefuse", REFUSE.load(Ordering::Relaxed));
    rec(b"RvSysWhy", WHY.load(Ordering::Relaxed));
    rec(b"RvSysFreed", FREED.load(Ordering::Relaxed));
    rec(b"RvSysLeak", LEAK.load(Ordering::Relaxed));
}

fn refuse(f: Fail) -> Fail {
    REFUSE.fetch_add(1, Ordering::Relaxed);
    WHY.store(cc::fail_word(f), Ordering::Relaxed);
    f
}

/// Run `f` with the KMD standard buffer `resource_id` (`pitch` bytes per row, `width` x `height`
/// 32 bpp pixels) as a copy-engine surface over its system pages. `f` must wait for every copy it
/// submits before it returns. PASSIVE, no lock held (this takes the content transaction, then the
/// channel's I/O without waiting long).
pub(crate) fn with_standard<R>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    pitch: u32,
    width: u32,
    height: u32,
    f: impl FnOnce(&CeSurface) -> R,
) -> Result<R, Fail> {
    let Some(guard) = adapter.system_backings.serialize(passive) else {
        return Err(refuse(NOT_SYSTEM));
    };
    let s = surface_locked(passive, adapter, &guard, resource_id, pitch, width, height)?;
    let r = f(&s);
    drop(guard);
    publish_counters();
    Ok(r)
}

fn surface_locked(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guard: &SystemBackingGuard<'_>,
    resource_id: u32,
    pitch: u32,
    width: u32,
    height: u32,
) -> Result<CeSurface, Fail> {
    if guard.system_copy_invalid(resource_id)
        || adapter
            .system_backings
            .guest_record(resource_id)
            .is_some_and(|r| !r.may_unlock())
    {
        return Err(refuse(NOT_SYSTEM));
    }
    let gen = ce_vram::chan_gen();
    {
        let g = STATE.lock();
        if let Some(e) = g.slots.iter().flatten().find(|e| e.resid == resource_id) {
            if e.chan_gen == gen && e.pitch == pitch && e.width == width && e.height == height {
                HIT.fetch_add(1, Ordering::Relaxed);
                return Ok(CeSurface {
                    va: e.va,
                    pitch,
                    width,
                    height,
                    fourcc: 0,
                    chan_gen: gen,
                });
            }
        }
    }
    // A stale entry of this resource (another channel, another shape) goes first.
    free_entry(passive, adapter, resource_id);
    let cover = helios_kmd_logic::guest_blob::cover_len(pitch, height, u64::from(pitch) * u64::from(height))
        .map_err(|_| refuse(UNCOVERED))?;
    if !helios_kmd_logic::ce_route::fits_window(cover) {
        return Err(refuse(UNCOVERED));
    }
    let Some(snapshot) = guard.snapshot(resource_id) else {
        return Err(refuse(NOT_SYSTEM));
    };
    let mut pieces = Vec::new();
    if !snapshot.pieces(&mut pieces) {
        return Err(refuse(UNCOVERED));
    }
    let count = helios_kmd_logic::guest_blob::build_runs(&pieces, cover, |_| {})
        .map_err(|_| refuse(UNCOVERED))?;
    let pages = (cover / 4096) as usize;
    let mut runs: Vec<helios_kmd_logic::guest_blob::Run> = Vec::new();
    let mut pfns: Vec<u64> = Vec::new();
    if runs.try_reserve_exact(count).is_err() || pfns.try_reserve_exact(pages).is_err() {
        return Err(refuse(UNCOVERED));
    }
    let filled = helios_kmd_logic::guest_blob::build_runs(&pieces, cover, |run| runs.push(run));
    pfns.resize(pages, 0);
    if filled != Ok(count) || helios_kmd_logic::ce_route::pfns_of_runs(&runs, &mut pfns) != Some(pages) {
        return Err(refuse(UNCOVERED));
    }
    let Some(pin) = snapshot.pin() else {
        return Err(refuse(UNCOVERED));
    };
    drop(pieces);
    let slot = {
        let g = STATE.lock();
        g.slots.iter().position(Option::is_none)
    };
    let slot = match slot {
        Some(i) => i as u8,
        None => {
            // Full: give the first one back (not this resource's, freed above).
            let victim = STATE.lock().slots.iter().flatten().next().map(|e| e.resid);
            if let Some(v) = victim {
                free_entry(passive, adapter, v);
            }
            match STATE.lock().slots.iter().position(Option::is_none) {
                Some(i) => i as u8,
                None => return Err(refuse(UNCOVERED)),
            }
        }
    };
    let made = rio::with_channel_io(passive, adapter, IO_WAIT_MS, MAKE_MS, |io, h| {
        rio::create_osdesc_io(io, h, rv::sys_handles(slot), rv::sys_va(slot), &pfns, cover)
    });
    match made {
        Some(Ok(va)) => {
            MADE.fetch_add(1, Ordering::Relaxed);
            let mut g = STATE.lock();
            g.slots[slot as usize] = Some(Entry {
                resid: resource_id,
                slot,
                va,
                cover,
                pitch,
                width,
                height,
                chan_gen: gen,
                pin,
            });
            refresh_any(&g);
            Ok(CeSurface {
                va,
                pitch,
                width,
                height,
                fourcc: 0,
                chan_gen: gen,
            })
        }
        Some(Err(rio::DstFail::Clean(f))) => {
            drop(pin);
            Err(refuse(f))
        }
        Some(Err(rio::DstFail::Unsure(_))) => {
            LEAK.fetch_add(1, Ordering::Relaxed);
            STATE.lock().leaked.push(pin);
            Err(refuse(UNSURE))
        }
        None => {
            drop(pin);
            Err(refuse(rio::BUSY))
        }
    }
}

/// Free `resource_id`'s descriptor (if any) and drop its pin; an unconfirmed free leaks the pin.
fn free_entry(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    let taken = {
        let mut g = STATE.lock();
        let i = g.slots.iter().position(|s| s.as_ref().is_some_and(|e| e.resid == resource_id));
        let e = i.and_then(|i| g.slots[i].take());
        refresh_any(&g);
        e
    };
    let Some(e) = taken else {
        return;
    };
    // A descriptor of an older channel went with that channel's client (`release_all` freed it,
    // or the client's close did).
    let freed = e.chan_gen != ce_vram::chan_gen()
        || rio::with_channel_io(passive, adapter, IO_WAIT_MS, FREE_MS, |io, h| {
            rio::free_osdesc_io(io, h, rv::sys_handles(e.slot), e.va, e.cover)
        })
        .unwrap_or(false);
    if freed {
        FREED.fetch_add(1, Ordering::Relaxed);
        drop(e.pin);
    } else {
        LEAK.fetch_add(1, Ordering::Relaxed);
        STATE.lock().leaked.push(e.pin);
    }
}

/// Before ANY change to `resource_id`'s leases (the paging path, content transaction held): its
/// descriptor goes first, so the pages may change. One relaxed load while none exists. PASSIVE.
pub(crate) fn before_lease_change(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    if ANY.load(Ordering::Acquire) == 0 {
        return;
    }
    free_entry(passive, adapter, resource_id);
}

/// `resource_id` is destroyed (content transaction held). PASSIVE.
pub(crate) fn destination_gone(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    before_lease_change(passive, adapter, resource_id);
}

/// The channel's teardown (its I/O held, the GPU idle): free every descriptor through `io`; a
/// confirmed free drops its pin, the rest are leaked until the generation ends. PASSIVE.
pub(crate) fn release_all(io: &crate::virtio::rm_client::Io<'_>, h: &crate::virtio::rm_client::ce_channel::Handles) {
    if ANY.load(Ordering::Acquire) == 0 {
        return;
    }
    for i in 0..rv::SYS_SLOTS {
        let taken = {
            let mut g = STATE.lock();
            let e = g.slots[i].take();
            refresh_any(&g);
            e
        };
        let Some(e) = taken else { continue };
        if rio::free_osdesc_io(io, h, rv::sys_handles(e.slot), e.va, e.cover) {
            FREED.fetch_add(1, Ordering::Relaxed);
            drop(e.pin);
        } else {
            LEAK.fetch_add(1, Ordering::Relaxed);
            STATE.lock().leaked.push(e.pin);
        }
    }
}

/// The transport generation ended (`rm_client::forget`, after the sweep): pins go when the sweep
/// confirmed every close, else they are leaked on purpose (the host may still name the pages).
pub(crate) fn forget(fate: helios_kmd_logic::sweep_budget::PinFate) {
    let (pins, leaked) = {
        let mut g = STATE.lock();
        let pins: Vec<GuestPin> = g.slots.iter_mut().filter_map(|s| s.take().map(|e| e.pin)).collect();
        let leaked = core::mem::take(&mut g.leaked);
        refresh_any(&g);
        (pins, leaked)
    };
    let leak = fate.action(true) == helios_kmd_logic::sweep_budget::PinAction::Leak;
    for p in pins.into_iter().chain(leaked) {
        if leak {
            core::mem::forget(p);
        } else {
            drop(p);
        }
    }
}

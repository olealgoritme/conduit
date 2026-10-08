//! The KMD's RM video-memory objects in the copy-engine channel (`RedirVram`, stages V2-V5 of
//! `docs/vram-redirection.md`): the shared seam for the redirected Blt (the route writes a
//! VRAM destination), the copy-only GDI acceleration (fallback A: copies between VRAM surfaces and
//! CPU-visible ones) and the CPU readers and writers of a GPU-only surface.
//!
//! * [`ce_surface`] / [`ce_surface_cached`]: an object of the allocation service (`vidmem.rs`),
//!   named by its host resource id, dup'd into the channel's client (`NV_ESC_RM_DUP_OBJECT` from
//!   the service's client) and GPU-mapped at a fixed 64 MiB window (big pages, the memory's own
//!   pitch kind; system-memory flags if RM refuses big pages). Cached per resource id
//!   (`rm_vidmem::MapBook`), never per RM handle: the service reuses a slot's handle after a free.
//!   A destroyed object's mapping goes stale at once ([`object_gone`]) and is given back by the
//!   next caller that holds the channel's I/O.
//! * [`copy`]: a VRAM-to-VRAM copy between two mapped objects (`rm_vidmem::vram_copy`), with no
//!   producer to wait for (`ce_channel::submit_copy`); the completion value, [`wait`] for it.
//! * [`transfer`]: CPU bytes to or from a rectangle of an object through the bounce buffer (RM
//!   system memory of the channel's client, CPU-mapped cached, GPU-mapped snooped): the upload of
//!   GDI's CPU-written staging into the GPU-only surface, and the readback for PrintWindow,
//!   capture and a CPU-visible copy. Synchronous and bounded ([`XFER_MS`]).
//!
//! I/O. Every RM message runs at PASSIVE with the channel's `IO_BUSY` held ([`ce_channel::try_io`]:
//! never waited for; busy is `ce_route::BUSY`, try again). The submit and the poll are spinlocks
//! and plain stores. The channel's teardown gives everything back ([`release_all`], before the
//! client's files close); the transport's end forgets it ([`forget`]).
//!
//! LOCKING. `BOOK` is a leaf spinlock over plain data; no I/O under it.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::ce_channel::{self as ce, CpuView, GpuMap, Handles};
use super::Io;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use helios_kmd_logic::ce_present::Remap;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{self as rc, Fail, FailKind};
use helios_kmd_logic::rm_vidmem::{self as rv, Dir, MapBook, MapPlan, Mapped, Rect, Surface};
use helios_kmd_logic::sweep_budget::UNITS_PER_MS;

/// Not an object of the VRAM service (no RM call was made).
pub(crate) const NOT_VRAM: Fail = Fail::new(FailKind::Refused, 0xE6);
/// The object does not fit a window, or a rectangle is outside it.
pub(crate) const BAD_SHAPE: Fail = Fail::new(FailKind::Layout, 0xE7);
/// A copy did not complete in time (the channel is marked broken).
pub(crate) const TIMEOUT: Fail = Fail::new(FailKind::Transport, 0xE8);
/// The channel refused the submit (ring full, no channel, a push it would not build).
pub(crate) const SUBMIT: Fail = Fail::new(FailKind::Transport, 0xE9);

/// The path is switched off (`RvOff`).
pub(crate) const DISABLED: Fail = Fail::new(FailKind::Refused, 0xF0);

static WAIT_TMO: AtomicU32 = AtomicU32::new(0);

/// The longest synchronous transfer or copy wait.
pub(crate) const XFER_MS: u64 = 250;
/// The I/O allowance of one call (dups, maps, the bounce's allocation).
const IO_MS: u64 = 2_000;

/// A VRAM object as the copy engine sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CeSurface {
    /// GPU VA in the channel's address space of byte 0.
    pub va: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    /// Changes whenever the channel's mappings were given back or forgotten: re-resolve then.
    pub chan_gen: u64,
}

impl CeSurface {
    pub(crate) const fn surface(&self) -> Surface {
        Surface {
            va: self.va,
            pitch: self.pitch,
            width: self.width,
            height: self.height,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Bounce {
    cpu: CpuView,
    gpu: GpuMap,
    len: u64,
}

struct Book {
    maps: MapBook,
    bounce: Option<Bounce>,
}

static BOOK: SpinLock<Book> = SpinLock::new(Book {
    maps: MapBook::new(),
    bounce: None,
});

// ---- BOOK, guarded against re-entry ----------------------------------------------------------------
//
// 364.1 hung the guest: a guard taken in a `match` scrutinee stayed alive across its arms, an arm took
// the table again (through `with_io` -> `give_back_stale`) on the same CPU at DISPATCH, and it spun
// forever. Every access goes through [`book`], which records the owning thread and answers a
// re-entry with `None` (counted, `RvBookReent`) instead of spinning: the caller fails that call
// (`REENTRY`) and the guest lives. The rule stays: never hold a guard across I/O or another call
// into this file (bind the result to a local first, never lock in a `match`/`if let` scrutinee).

#[link(name = "ntoskrnl")]
extern "system" {
    fn PsGetCurrentThreadId() -> *mut core::ffi::c_void;
}

static BOOK_OWNER: AtomicU32 = AtomicU32::new(0);
static BOOK_REENT: AtomicU32 = AtomicU32::new(0);

/// A re-entrant access to the map table was refused (a bug: a guard held across a call that comes
/// back into this file).
pub(crate) const REENTRY: Fail = Fail::new(FailKind::Refused, 0xF1);

struct BookGuard<'a> {
    inner: crate::sync::SpinLockGuard<'a, Book>,
}

impl core::ops::Deref for BookGuard<'_> {
    type Target = Book;
    fn deref(&self) -> &Book {
        &self.inner
    }
}

impl core::ops::DerefMut for BookGuard<'_> {
    fn deref_mut(&mut self) -> &mut Book {
        &mut self.inner
    }
}

impl Drop for BookGuard<'_> {
    fn drop(&mut self) {
        // Before the inner guard releases the lock (fields drop after this body).
        BOOK_OWNER.store(0, Ordering::Release);
    }
}

fn thread_tag() -> u32 {
    // SAFETY: a scalar read of the current thread's id; any IRQL.
    let id = (unsafe { PsGetCurrentThreadId() } as usize) as u32;
    id | 1
}

/// The map table, or `None` when this thread already holds it (counted).
fn book() -> Option<BookGuard<'static>> {
    let me = thread_tag();
    if BOOK_OWNER.load(Ordering::Acquire) == me {
        BOOK_REENT.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let inner = BOOK.lock();
    BOOK_OWNER.store(me, Ordering::Release);
    Some(BookGuard { inner })
}
static CHAN_GEN: AtomicU64 = AtomicU64::new(1);
/// Nonzero while any mapping or the bounce exists (the teardown hooks' fast exit).
static ANY: AtomicU32 = AtomicU32::new(0);

static MAP_OK: AtomicU32 = AtomicU32::new(0);
static MAP_FAIL: AtomicU32 = AtomicU32::new(0);
static MAP_STAT: AtomicU32 = AtomicU32::new(0);
static MAP_GIVE: AtomicU32 = AtomicU32::new(0);
static XFER: AtomicU32 = AtomicU32::new(0);
static XFER_FAIL: AtomicU32 = AtomicU32::new(0);
static XFER_WHY: AtomicU32 = AtomicU32::new(0);
static XFER_US: AtomicU32 = AtomicU32::new(0);
static XFER_MAX: AtomicU32 = AtomicU32::new(0);
static COPY: AtomicU32 = AtomicU32::new(0);
static COPY_FAIL: AtomicU32 = AtomicU32::new(0);

fn now() -> u64 {
    ce::now_100ns()
}

fn us_since(t0: u64) -> u32 {
    (now().wrapping_sub(t0) / 10).min(u64::from(u32::MAX)) as u32
}

fn refresh_any(b: &Book) {
    let any = b.maps.live() != 0 || b.bounce.is_some() || b.maps.has_stale();
    ANY.store(u32::from(any), Ordering::Release);
}

/// An NVK-made (foreign) image as a copy-engine SOURCE for a GDI command (`RedirVram`,
/// `docs/vram-redirection.md` 8): its base in the channel, its layout and plan, and the producer's
/// semaphore when the route has seen a record of it at a Present.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForeignSource {
    /// GPU VA in the channel of the memory the image lives in (byte 0 of the RM object; the plan's
    /// offset is applied by `rm_vidmem::foreign_copy`). With a record: the record's mapping.
    pub va: u64,
    pub plan: helios_kmd_logic::ce_present::SourcePlan,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    /// Acquire this before the copy: the producer's frame is complete. `None`: no record seen (the
    /// image is copied as it is in memory).
    pub acquire: Option<helios_kmd_logic::ce_present::Acquire>,
    pub chan_gen: u64,
}

static FGN_REC: AtomicU32 = AtomicU32::new(0);
static FGN_IMP: AtomicU32 = AtomicU32::new(0);
static FGN_FAIL: AtomicU32 = AtomicU32::new(0);
static FGN_WHY: AtomicU32 = AtomicU32::new(0);
static FGN_WRITE: AtomicU32 = AtomicU32::new(0);
static CLEARED: AtomicU32 = AtomicU32::new(0);

fn fgn_fail(f: Fail) -> Fail {
    FGN_FAIL.fetch_add(1, Ordering::Relaxed);
    FGN_WHY.store(cc::fail_word(f), Ordering::Relaxed);
    f
}

/// `resource_id` (an adopted foreign NVK image) as a copy-engine source. With a record the route
/// validated for it: the producer's own objects, dup'd and mapped (`ce_dup`, cached), and its
/// semaphore. Without one: the image's memory imported into the channel's client by resource id
/// (the host makes it a GEM of the channel's DRM file, `GEM_EXPORT_NVKMS`,
/// `OS_UNIX_IMPORT_OBJECT_FROM_FD`: NVK's own route, `nvk-rm` 0031) and mapped with the modifier's
/// page kind, cached per resource id; no acquire. PASSIVE, no lock held; takes the channel's I/O
/// without waiting (`BUSY`). Call it BEFORE `ce_sysmem::with_standard` (the I/O order).
pub(crate) fn foreign_source(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<ForeignSource, Fail> {
    let r = foreign_source_inner(passive, adapter, resource_id).map_err(fgn_fail);
    publish_counters();
    r
}

fn foreign_source_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<ForeignSource, Fail> {
    use helios_kmd_logic::rm_vidmem::off;
    if super::vidmem::off(off::FOREIGN) {
        return Err(DISABLED);
    }
    let view = super::ce_route::chan_view();
    let (true, Some(gen)) = (view.up, view.gen) else {
        return Err(super::ce_route::NO_CHANNEL);
    };
    if let Some(rec) = crate::ddi::ce_present_route::source_record(adapter, resource_id) {
        let desc = super::ce_dup::source_desc(&rec);
        let plan = helios_kmd_logic::ce_present::source_plan(gen, &desc).map_err(|_| BAD_SHAPE)?;
        let p = super::ce_route::prep_producer(passive, adapter, &rec)?;
        FGN_REC.fetch_add(1, Ordering::Relaxed);
        let s = rec.record.source;
        // `ce_dup`'s source VA already carries the plan's offset: give back the base.
        return Ok(ForeignSource {
            va: p.src_va.wrapping_sub(plan.offset),
            plan,
            pitch: s.pitch,
            width: s.width,
            height: s.height,
            fourcc: s.fourcc,
            // Opt-in only (`RvOff` 0x100): the record's value may never be released again.
            acquire: super::vidmem::off(off::FOREIGN_ACQUIRE_ON).then_some(
                helios_kmd_logic::ce_present::Acquire {
                    va: p.sem_va,
                    value: rec.record.semaphore.value,
                },
            ),
            chan_gen: chan_gen(),
        });
    }
    if super::vidmem::off(off::FOREIGN_IMPORT) {
        return Err(DISABLED);
    }
    let (layout, size) = adapter
        .with_virtio(|v| v.foreign_record(resource_id))
        .ok()
        .flatten()
        .ok_or(NOT_VRAM)?;
    let desc = helios_kmd_logic::ce_present::SourceDesc {
        offset: u64::from(layout.offset),
        size,
        modifier: layout.modifier,
        pitch: layout.stride,
        width: layout.width,
        height: layout.height,
        fourcc: layout.fourcc,
        compressed: false,
    };
    let plan = helios_kmd_logic::ce_present::source_plan(gen, &desc).map_err(|_| BAD_SHAPE)?;
    let found = book().ok_or(REENTRY)?.maps.find(resource_id);
    let va = match found {
        Some(m) => m.va,
        None => with_io(passive, adapter, |io, h| import_foreign(io, h, resource_id, size, plan.page_kind))?,
    };
    FGN_IMP.fetch_add(1, Ordering::Relaxed);
    Ok(ForeignSource {
        va,
        plan,
        pitch: layout.stride,
        width: layout.width,
        height: layout.height,
        fourcc: layout.fourcc,
        acquire: None,
        chan_gen: chan_gen(),
    })
}

/// Copy `src_rect` of the foreign image `src` ([`foreign_source`]) to `(dst_x, dst_y)` of `dst`
/// (a VRAM surface, or a staging view inside `ce_sysmem::with_standard`), whose pixel format is
/// `dst_fourcc` (`DRM_FORMAT_*`; R/B are exchanged on the copy engine when the two differ). Acquires
/// the producer's semaphore when the source has one. Submitted now; the completion value
/// ([`wait`]). Spinlocks only.
pub(crate) fn foreign_copy(
    src: &ForeignSource,
    src_rect: Rect,
    dst: &CeSurface,
    dst_x: u32,
    dst_y: u32,
    dst_fourcc: u32,
) -> Result<u64, Fail> {
    if src.chan_gen != chan_gen() || dst.chan_gen != chan_gen() {
        return Err(BAD_SHAPE);
    }
    let remap = helios_kmd_logic::ce_present::remap_for(src.fourcc, dst_fourcc).map_err(|_| BAD_SHAPE)?;
    let copy = rv::foreign_copy(
        &src.plan,
        src.va,
        src.pitch,
        src.width,
        src.height,
        src_rect,
        &dst.surface(),
        dst_x,
        dst_y,
        remap,
    )
    .map_err(|_| BAD_SHAPE)?;
    let r = match src.acquire {
        Some(acquire) => ce::submit_build(|push, gen, done| {
            helios_kmd_logic::ce_present::present_push(push, gen, acquire, &copy, done)
        }),
        None => ce::submit_copy(&copy),
    };
    match r {
        Ok(v) => {
            COPY.fetch_add(1, Ordering::Relaxed);
            Ok(v)
        }
        Err(_) => {
            COPY_FAIL.fetch_add(1, Ordering::Relaxed);
            Err(SUBMIT)
        }
    }
}

/// Copy `src_rect` of `src` (a VRAM surface, or a staging view inside `ce_sysmem::with_standard`;
/// pixel format `src_fourcc`) INTO the foreign NVK image `dst` ([`foreign_source`]) at
/// `(dst_x, dst_y)`: a GDI BitBlt whose destination is an app's image. Block-linear by the copy's
/// destination origin; R/B exchanged when the formats differ. Acquires the producer's semaphore
/// only when `dst.acquire` is set (opt-in, `RvOff` 0x100: the app's frame is complete before GDI
/// writes over it). ORDERING: without the acquire the write may land while the app's GPU work is
/// still writing the same image (GDI on a D3D window's surface is already unordered on bare metal
/// without a flush); the caller waits for the copy before it returns from the GDI command, so the
/// app's LATER submissions follow it on the CPU timeline. Submitted now; the completion value
/// ([`wait`]). Spinlocks only.
pub(crate) fn foreign_write(
    src: &CeSurface,
    src_rect: Rect,
    src_fourcc: u32,
    dst: &ForeignSource,
    dst_x: u32,
    dst_y: u32,
) -> Result<u64, Fail> {
    if src.chan_gen != chan_gen() || dst.chan_gen != chan_gen() {
        return Err(BAD_SHAPE);
    }
    let remap = helios_kmd_logic::ce_present::remap_for(src_fourcc, dst.fourcc).map_err(|_| BAD_SHAPE)?;
    let copy = rv::foreign_write(
        &src.surface(),
        src_rect,
        &dst.plan,
        dst.va,
        dst.pitch,
        dst.width,
        dst.height,
        dst_x,
        dst_y,
        remap,
    )
    .map_err(|_| BAD_SHAPE)?;
    let acquire = dst.acquire;
    let r = ce::submit_build(|push, gen, done| {
        helios_kmd_logic::ce_present::copy_push(push, gen, acquire, &copy, done)
    });
    match r {
        Ok(v) => {
            COPY.fetch_add(1, Ordering::Relaxed);
            FGN_WRITE.fetch_add(1, Ordering::Relaxed);
            Ok(v)
        }
        Err(_) => {
            COPY_FAIL.fetch_add(1, Ordering::Relaxed);
            Err(SUBMIT)
        }
    }
}

/// Import `resource_id` into the channel's client and map it at a slot's window. The caller holds
/// the channel's I/O.
fn import_foreign(
    io: &Io<'_>,
    h: &Handles,
    resource_id: u32,
    size: u64,
    kind: Option<u32>,
) -> Result<u64, Fail> {
    let len = rv::map_len(helios_kmd_logic::round_up_page(size)).ok_or(BAD_SHAPE)?;
    let plan = book().ok_or(REENTRY)?.maps.plan(resource_id);
    let slot = match plan {
        MapPlan::Hit(m) => return Ok(m.va),
        MapPlan::Make { slot, evict } => {
            if let Some(old) = evict {
                let _ = book().ok_or(REENTRY)?.maps.remove(old.slot);
                give_back(io, h, &old);
            }
            slot
        }
    };
    let (h_mem, h_virt) = rv::map_handles(slot);
    // 1. The host: the resource's memory as a GEM of the channel client's DRM file.
    // `RvFgnWhy` of a failed host import: 0x8001_00EE the host does not serve RmResourceImport (config
    // feature bit 14), 0x8001_00ED the KMD's own gate refused it, 0x8003_00xx the host's errno xx,
    // 0x8002_00EF no transport.
    use crate::virtio::rm_resource_import::RiError;
    use helios_kmd_logic::foreign_errno::Verdict;
    let gem = crate::virtio::rm_resource_import::rm_resource_import_kmd(io.passive, io.adapter, h.drm, resource_id)
        .map_err(|e| match e {
            RiError::Local(Verdict::Unsupported) => Fail::new(FailKind::Refused, 0xEE),
            RiError::Local(_) => Fail::new(FailKind::Refused, 0xED),
            RiError::Host(_, errno) => Fail::new(FailKind::Host, errno & 0xff),
            RiError::NoTransport => Fail::new(FailKind::Transport, 0xEF),
        })?
        .gem_handle;
    let r = import_gem(io, h, gem, h_mem);
    // The GEM is only the envelope (our RM handle holds the memory once imported).
    let close = rc::gem_close_params(gem);
    let mut resp = [0u8; super::REPLY_MAX];
    if io.exchange(h.drm, rc::DRM_IOCTL_GEM_CLOSE, &close, &[], &mut resp).is_err() {
        ce::note_soft();
    }
    r?;
    // 2. The mapping: the modifier's page kind, big pages first (as `ce_dup` maps an NVK image).
    let va = rv::map_va(slot);
    let (first, kind_flag) = match kind {
        Some(_) => (cc::MAP_FLAGS_PAGE_SIZE_BIG | cc::MAP_FLAGS_KIND_OVERRIDE, cc::MAP_FLAGS_KIND_OVERRIDE),
        None => (cc::MAP_FLAGS_PAGE_SIZE_BIG, 0),
    };
    let mut mapped = ce::gpu_map_with(io, h, h_virt, h_mem, va, len, first, kind);
    if let Err(f) = mapped {
        if f.kind == FailKind::Rm {
            mapped = ce::gpu_map_with(io, h, h_virt, h_mem, va, len, cc::MAP_FLAGS_SYSMEM | kind_flag, kind);
        }
    }
    match mapped {
        Ok(g) => {
            MAP_OK.fetch_add(1, Ordering::Relaxed);
            let mut b = book().ok_or(REENTRY)?;
            b.maps.insert(slot, resource_id, g.va, len);
            refresh_any(&b);
            Ok(g.va)
        }
        Err(f) => {
            MAP_FAIL.fetch_add(1, Ordering::Relaxed);
            MAP_STAT.store(cc::fail_word(f), Ordering::Relaxed);
            if ce::rm_free(io, h, rc::H_DEVICE, h_mem).is_err() {
                ce::note_soft();
            }
            Err(f)
        }
    }
}

/// GEM `gem` of the channel's DRM file -> a fresh control file -> RM memory `h_mem` of the channel's
/// client (`GEM_EXPORT_NVKMS`, `OS_UNIX_IMPORT_OBJECT_FROM_FD`). The caller holds the channel's I/O.
fn import_gem(io: &Io<'_>, h: &Handles, gem: u32, h_mem: u32) -> Result<(), Fail> {
    let ctl = io.open_file(rc::DEV_CTL)?;
    let r = (|| {
        let data = rv::gem_export_params(gem);
        let nested = ctl.to_le_bytes();
        let mut resp = [0u8; super::REPLY_MAX];
        let n = io.exchange(h.drm, rv::DRM_IOCTL_GEM_EXPORT_NVKMS, &data, &nested, &mut resp)?;
        rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x75))?)
            .map_err(|_| Fail::new(FailKind::Parse, 0x75))?;
        let params = rv::import_from_fd_params(ctl, rc::H_DEVICE, h_mem);
        let block = rc::nvos54(h.root, h.root, rv::CTRL_IMPORT_OBJECT_FROM_FD, params.len() as u32);
        let mut resp = [0u8; super::REPLY_MAX];
        let n = io.exchange(h.ctl, rc::nv_cmd(rc::ESC_RM_CONTROL, 32), &block, &params, &mut resp)?;
        rc::rm_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x76))?, rc::NVOS54_STATUS_AT)
            .map(|_| ())
            .map_err(Fail::from)
    })();
    if !io.close_file(ctl) {
        ce::note_soft();
    }
    r
}

/// The channel's generation of mappings (see [`CeSurface::chan_gen`]).
pub(crate) fn chan_gen() -> u64 {
    CHAN_GEN.load(Ordering::Acquire)
}

fn epoch(adapter: &AdapterContext) -> Option<u64> {
    adapter
        .with_virtio(|v| v.nvrm_epoch())
        .ok()
        .filter(|e| *e != 0)
}

/// Run `f` with the channel's I/O held and an `Io` on the channel's client. `BUSY` when another
/// thread has it, `NO_CHANNEL` when the channel is not up.
fn with_io<T>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    f: impl FnOnce(&Io<'_>, &Handles) -> Result<T, Fail>,
) -> Result<T, Fail> {
    let view = super::ce_route::chan_view();
    let (true, Some(h), Some(epoch)) = (view.up, ce::handles(), epoch(adapter)) else {
        return Err(super::ce_route::NO_CHANNEL);
    };
    if !ce::try_io() {
        return Err(super::ce_route::BUSY);
    }
    let r = {
        let _bounded = crate::ddi::escape_wait::begin_bounded(IO_MS as u32);
        let io = Io {
            passive,
            adapter,
            epoch,
            limit: Some(ce::budget_ms(IO_MS)),
        };
        give_back_stale(&io, &h);
        f(&io, &h)
    };
    ce::end_io();
    r
}

/// `resource_id` as a copy-engine surface, mapped on demand. PASSIVE, no lock held; the caller
/// must NOT hold the channel's I/O (this takes it, without waiting).
pub(crate) fn ce_surface(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<CeSurface, Fail> {
    if let Some(s) = ce_surface_cached(resource_id) {
        return Ok(s);
    }
    super::vidmem::lookup(resource_id).ok_or(NOT_VRAM)?;
    with_io(passive, adapter, |io, h| map_locked(io, h, resource_id))
}

/// `resource_id`'s mapping if it exists now (no RM call). Spinlock only, any IRQL up to DISPATCH.
pub(crate) fn ce_surface_cached(resource_id: u32) -> Option<CeSurface> {
    let obj = super::vidmem::lookup(resource_id)?;
    let m = book()?.maps.find(resource_id)?;
    Some(surface_of(&obj, &m))
}

fn surface_of(obj: &super::vidmem::VramObject, m: &Mapped) -> CeSurface {
    CeSurface {
        va: m.va,
        pitch: obj.pitch,
        width: obj.width,
        height: obj.height,
        fourcc: obj.fourcc,
        chan_gen: chan_gen(),
    }
}

/// The GPU VA of the KMD RM object behind `resource_id` in the channel, mapped on demand: a VRAM
/// surface of the `vidmem` service, or a GDI staging buffer of the `sysmem` service (RM system
/// memory, `RedirVram`). The VA of byte 0; the caller knows the layout. PASSIVE, no lock held but
/// possibly the content transaction (the order is content -> channel I/O); takes the channel's
/// I/O without waiting (`BUSY`).
pub(crate) fn ce_object_va(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) -> Result<u64, Fail> {
    let found = book().ok_or(REENTRY)?.maps.find(resource_id);
    if let Some(m) = found {
        return Ok(m.va);
    }
    let (client, memory, size, sysmem) = rm_object(resource_id).ok_or(NOT_VRAM)?;
    with_io(passive, adapter, |io, h| map_object(io, h, resource_id, client, memory, size, sysmem))
}

/// `(client, memory, size, is system memory)` of a KMD RM object.
fn rm_object(resource_id: u32) -> Option<(u32, u32, u64, bool)> {
    if let Some(o) = super::vidmem::lookup(resource_id) {
        return Some((o.client, o.memory, o.size, false));
    }
    super::sysmem::object(resource_id).map(|(c, m, s)| (c, m, s, true))
}

/// The mapping of `resource_id`, made if needed. The caller holds the channel's I/O.
fn map_locked(io: &Io<'_>, h: &Handles, resource_id: u32) -> Result<CeSurface, Fail> {
    let obj = super::vidmem::lookup(resource_id).ok_or(NOT_VRAM)?;
    let va = map_object(io, h, resource_id, obj.client, obj.memory, obj.size, false)?;
    let m = book().ok_or(REENTRY)?.maps.find(resource_id).ok_or(BAD_SHAPE)?;
    debug_assert_eq!(m.va, va);
    Ok(surface_of(&obj, &m))
}

/// Clear a new VRAM surface to 0 on the copy engine and wait (at most `XFER_MS`). The caller holds
/// the channel's I/O and no `book()` guard.
fn clear_new(io: &Io<'_>, resource_id: u32, va: u64) {
    let Some(obj) = super::vidmem::lookup(resource_id) else {
        return;
    };
    let lines = (obj.size / u64::from(obj.pitch.max(1))) as u32;
    let r = ce::submit_build(|push, _gen, done| {
        rv::clear(push, va, obj.pitch, lines, 0)?;
        helios_kmd_logic::ce_present::release(push, done)
    });
    let ok = match r {
        Ok(v) => wait(io.passive, v, XFER_MS),
        Err(_) => false,
    };
    if ok {
        CLEARED.fetch_add(1, Ordering::Relaxed);
    } else {
        super::vidmem::clear_failed(resource_id);
    }
}

/// Dup `(client, memory)` into the channel's client and map it at a slot's window (VRAM: big pages
/// first; system memory: the snooped system flags first). The caller holds the channel's I/O.
fn map_object(
    io: &Io<'_>,
    h: &Handles,
    resource_id: u32,
    client: u32,
    memory: u32,
    size: u64,
    sysmem: bool,
) -> Result<u64, Fail> {
    let len = rv::map_len(helios_kmd_logic::round_up_page(size)).ok_or(BAD_SHAPE)?;
    let plan = book().ok_or(REENTRY)?.maps.plan(resource_id);
    let slot = match plan {
        MapPlan::Hit(m) => return Ok(m.va),
        MapPlan::Make { slot, evict } => {
            if let Some(old) = evict {
                let _ = book().ok_or(REENTRY)?.maps.remove(old.slot);
                give_back(io, h, &old);
            }
            slot
        }
    };
    let (h_dup, h_virt) = rv::map_handles(slot);
    if let Err(f) = dup(io, h, h_dup, client, memory) {
        MAP_FAIL.fetch_add(1, Ordering::Relaxed);
        MAP_STAT.store(cc::fail_word(f), Ordering::Relaxed);
        ce::note_rm_error();
        return Err(f);
    }
    let va = rv::map_va(slot);
    let (first, second) = if sysmem {
        (rv::map_flags_second(), rv::map_flags_first())
    } else {
        (rv::map_flags_first(), rv::map_flags_second())
    };
    let mut mapped = ce::gpu_map_with(io, h, h_virt, h_dup, va, len, first, None);
    if let Err(f) = mapped {
        if f.kind == FailKind::Rm {
            mapped = ce::gpu_map_with(io, h, h_virt, h_dup, va, len, second, None);
        }
    }
    match mapped {
        Ok(g) => {
            MAP_OK.fetch_add(1, Ordering::Relaxed);
            {
                let mut b = book().ok_or(REENTRY)?;
                b.maps.insert(slot, resource_id, g.va, len);
                refresh_any(&b);
            }
            // A VRAM surface's first mapping clears it before any copy can write it (every write
            // path maps first); RM does not zero video memory on allocation.
            if !sysmem && super::vidmem::claim_clear(resource_id) {
                clear_new(io, resource_id, g.va);
            }
            Ok(g.va)
        }
        Err(f) => {
            MAP_FAIL.fetch_add(1, Ordering::Relaxed);
            MAP_STAT.store(cc::fail_word(f), Ordering::Relaxed);
            ce::note_rm_error();
            if ce::rm_free(io, h, rc::H_DEVICE, h_dup).is_err() {
                ce::note_soft();
            }
            Err(f)
        }
    }
}

/// `NV_ESC_RM_DUP_OBJECT` of `object` of `client` as `h_new` under the channel's device.
fn dup(io: &Io<'_>, h: &Handles, h_new: u32, client: u32, object: u32) -> Result<(), Fail> {
    let block = cc::nvos55(h.root, rc::H_DEVICE, h_new, client, object);
    let mut resp = [0u8; super::REPLY_MAX];
    let n = io.exchange(
        h.ctl,
        rc::nv_cmd(cc::ESC_RM_DUP_OBJECT, cc::NVOS55_BYTES as u32),
        &block,
        &[],
        &mut resp,
    )?;
    rc::rm_reply(
        resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x74))?,
        cc::NVOS55_STATUS_AT,
    )
    .map(|_| ())
    .map_err(Fail::from)
}

/// Give one mapping back: its GPU mapping, then the dup.
fn give_back(io: &Io<'_>, h: &Handles, m: &Mapped) {
    MAP_GIVE.fetch_add(1, Ordering::Relaxed);
    if io.stopping() || io.limit_spent() {
        return;
    }
    let (h_dup, h_virt) = rv::map_handles(m.slot);
    let g = GpuMap {
        virt: h_virt,
        mem: h_dup,
        va: m.va,
        len: m.len,
    };
    let mut ok = ce::gpu_unmap(io, h, &g);
    ok &= ce::rm_free(io, h, rc::H_DEVICE, h_dup).is_ok();
    if !ok {
        ce::note_soft();
    }
}

fn give_back_stale(io: &Io<'_>, h: &Handles) {
    if ANY.load(Ordering::Acquire) == 0 {
        return;
    }
    loop {
        let taken = book().and_then(|mut b| b.maps.take_stale());
        let Some(m) = taken else {
            break;
        };
        give_back(io, h, &m);
    }
    if let Some(b) = book() {
        refresh_any(&b);
    }
}

/// The allocation behind `resource_id` is being destroyed (`vidmem::released`): its mapping is
/// never used again and goes back at the next pass that holds the channel's I/O. Spinlock only.
pub(crate) fn object_gone(resource_id: u32) {
    let Some(mut b) = book() else {
        return;
    };
    if b.maps.mark_stale(resource_id) {
        ANY.store(1, Ordering::Release);
    }
}

/// Poll the channel until its completion reaches `value`, at most `max_ms` (spinning first, then
/// in ticks). `false`: not in time, or the channel failed.
pub(crate) fn wait(passive: PassiveLevel, value: u64, max_ms: u64) -> bool {
    let start = now();
    let deadline = start + max_ms * UNITS_PER_MS;
    loop {
        let Some(p) = ce::poll() else {
            return false;
        };
        if p.notifier != 0 {
            return false;
        }
        if p.completed >= value {
            return true;
        }
        let t = now();
        if t >= deadline {
            // A copy of the KMD's own that did not complete in time: the channel may be stuck (an
            // acquire that never releases, a fault). Nothing more is submitted until the route's
            // worker tears it down (bounded), which discharges what is queued behind it.
            WAIT_TMO.fetch_add(1, Ordering::Relaxed);
            super::ce_route::mark_broken();
            return false;
        }
        if t < start + ce::SPIN_100NS {
            core::hint::spin_loop();
        } else {
            crate::virtio::ctrl::sleep_ms(passive, 1);
        }
    }
}

/// A VRAM-to-VRAM copy of `src_rect` of `src` to `(dst_x, dst_y)` of `dst` (both mapped: resolve
/// them with [`ce_surface`] first), submitted now with no producer to wait for. The completion
/// value ([`wait`]). Spinlocks only.
pub(crate) fn copy(
    src: &CeSurface,
    src_rect: Rect,
    dst: &CeSurface,
    dst_x: u32,
    dst_y: u32,
    remap: Remap,
) -> Result<u64, Fail> {
    let c = rv::vram_copy(&src.surface(), src_rect, &dst.surface(), dst_x, dst_y, remap)
        .map_err(|_| BAD_SHAPE)?;
    match ce::submit_copy(&c) {
        Ok(v) => {
            COPY.fetch_add(1, Ordering::Relaxed);
            Ok(v)
        }
        Err(_) => {
            COPY_FAIL.fetch_add(1, Ordering::Relaxed);
            Err(SUBMIT)
        }
    }
}

/// CPU bytes to (`Upload`) or from (`Readback`) `rect` of `resource_id`, through the bounce
/// buffer. `bytes` holds the rectangle's rows `row_pitch` bytes apart (at least `width * 4` each).
/// Synchronous: the copy is submitted and waited for (at most [`XFER_MS`]). PASSIVE, no lock held,
/// the caller does not hold the channel's I/O.
pub(crate) fn transfer(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    rect: Rect,
    dir: Dir,
    bytes: &mut [u8],
    row_pitch: usize,
) -> Result<(), Fail> {
    let t0 = now();
    let r = with_io(passive, adapter, |io, h| {
        let s = map_locked(io, h, resource_id)?;
        let packed = rv::bounce_surface(rect).map_err(|_| BAD_SHAPE)?;
        let row = packed.pitch as usize;
        let rows = packed.height as usize;
        if row_pitch < row || bytes.len() < row_pitch * (rows - 1) + row {
            return Err(BAD_SHAPE);
        }
        let need = rv::bounce_bytes(rect).map_err(|_| BAD_SHAPE)?;
        let b = bounce(io, h, need)?;
        let copy = rv::bounce_copy(&s.surface(), rect, dir).map_err(|_| BAD_SHAPE)?;
        if dir == Dir::Upload {
            for y in 0..rows {
                // SAFETY: the bounce view holds `need >= row * rows` bytes (checked by
                // `bounce`), mapped until `release_all`; `bytes` holds row `y` (checked above).
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(y * row_pitch),
                        (b.cpu.va as *mut u8).add(y * row),
                        row,
                    );
                }
            }
            ce::full_barrier();
        }
        let value = ce::submit_copy(&copy).map_err(|_| SUBMIT)?;
        if !wait(io.passive, value, XFER_MS) {
            super::ce_route::mark_broken();
            return Err(TIMEOUT);
        }
        if dir == Dir::Readback {
            ce::full_barrier();
            for y in 0..rows {
                // SAFETY: as above, the other way round.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (b.cpu.va as *const u8).add(y * row),
                        bytes.as_mut_ptr().add(y * row_pitch),
                        row,
                    );
                }
            }
        }
        Ok(())
    });
    let us = us_since(t0);
    XFER_US.store(us, Ordering::Relaxed);
    XFER_MAX.fetch_max(us, Ordering::Relaxed);
    match &r {
        Ok(()) => {
            XFER.fetch_add(1, Ordering::Relaxed);
        }
        Err(f) => {
            XFER_FAIL.fetch_add(1, Ordering::Relaxed);
            XFER_WHY.store(cc::fail_word(*f), Ordering::Relaxed);
        }
    }
    r
}

/// The bounce buffer, at least `need` bytes (kept between calls; a larger request replaces it).
/// The caller holds the channel's I/O and nothing is in flight on the old one (transfers wait).
fn bounce(io: &Io<'_>, h: &Handles, need: u64) -> Result<Bounce, Fail> {
    let cur = book().ok_or(REENTRY)?.bounce;
    if let Some(b) = cur {
        if b.len >= need {
            return Ok(b);
        }
    }
    let old = book().ok_or(REENTRY)?.bounce.take();
    if let Some(mut old) = old {
        free_bounce(io, h, &mut old);
    }
    let len = need;
    ce::alloc_sys(io, h, rv::H_BOUNCE, len).inspect_err(|_| ce::note_rm_error())?;
    let cpu = match ce::cpu_map(io, h, rc::H_DEVICE, rv::H_BOUNCE, ce::SYSMEM, len, ce::sysmem_view_cache()) {
        Ok(v) => v,
        Err(f) => {
            let _ = ce::rm_free(io, h, rc::H_DEVICE, rv::H_BOUNCE);
            return Err(f);
        }
    };
    let gpu = match ce::gpu_map(io, h, rv::H_BOUNCE_VIRT, rv::H_BOUNCE, rv::BOUNCE_VA, len) {
        Ok(g) => g,
        Err(f) => {
            let mut cpu = cpu;
            let _ = ce::cpu_unmap(io, h, &mut cpu, true);
            let _ = ce::rm_free(io, h, rc::H_DEVICE, rv::H_BOUNCE);
            return Err(f);
        }
    };
    let b = Bounce { cpu, gpu, len };
    let mut g = book().ok_or(REENTRY)?;
    g.bounce = Some(b);
    ANY.store(1, Ordering::Release);
    Ok(b)
}

fn free_bounce(io: &Io<'_>, h: &Handles, b: &mut Bounce) {
    let send = !io.stopping() && !io.limit_spent();
    let mut ok = ce::cpu_unmap(io, h, &mut b.cpu, send);
    if send {
        ok &= ce::gpu_unmap(io, h, &b.gpu);
        ok &= ce::rm_free(io, h, rc::H_DEVICE, rv::H_BOUNCE).is_ok();
    }
    if !ok {
        ce::note_soft();
    }
}

/// The channel's teardown (the GPU idle, before the client's files close): give every mapping and
/// the bounce back. The caller holds the channel's I/O. One relaxed load when there is nothing.
pub(super) fn release_all(io: &Io<'_>, h: &Handles) {
    // The CE views of standard buffers' system pages first (their own fast exit).
    crate::ddi::ce_sysmem::release_all(io, h);
    if ANY.load(Ordering::Acquire) == 0 {
        CHAN_GEN.fetch_add(1, Ordering::AcqRel);
        return;
    }
    loop {
        let taken = book().and_then(|mut b| b.maps.take_any());
        let Some(m) = taken else {
            break;
        };
        give_back(io, h, &m);
    }
    let bounce = book().and_then(|mut b| b.bounce.take());
    if let Some(mut b) = bounce {
        free_bounce(io, h, &mut b);
    }
    ANY.store(0, Ordering::Release);
    CHAN_GEN.fetch_add(1, Ordering::AcqRel);
}

/// The transport is about to be retired (`ce_channel::drop_views`): the bounce's kernel view is
/// unmapped, nothing sent.
pub(super) fn drop_views() {
    let bounce = book().and_then(|mut b| b.bounce.take());
    if let Some(b) = bounce {
        ce::kernel_unmap(b.cpu.va, b.cpu.len);
    }
}

/// The transport is gone (`ce_channel::forget`): the sweep closed the client and everything in it.
pub(super) fn forget() {
    let Some(mut b) = book() else {
        return;
    };
    b.maps.clear();
    if b.bounce.take().is_some() {
        ce::note_soft();
    }
    ANY.store(0, Ordering::Release);
    CHAN_GEN.fetch_add(1, Ordering::AcqRel);
}

/// Mirror the counters (PASSIVE); with the service's block.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    let live = book().map_or(0, |b| b.maps.live());
    rec(b"RvBookReent", BOOK_REENT.load(Ordering::Relaxed));
    rec(b"RvMapOk", MAP_OK.load(Ordering::Relaxed));
    rec(b"RvMapFail", MAP_FAIL.load(Ordering::Relaxed));
    rec(b"RvMapStat", MAP_STAT.load(Ordering::Relaxed));
    rec(b"RvMapLive", live);
    rec(b"RvMapGive", MAP_GIVE.load(Ordering::Relaxed));
    rec(b"RvXfer", XFER.load(Ordering::Relaxed));
    rec(b"RvXferFail", XFER_FAIL.load(Ordering::Relaxed));
    rec(b"RvXferWhy", XFER_WHY.load(Ordering::Relaxed));
    rec(b"RvXferUs", XFER_US.load(Ordering::Relaxed));
    rec(b"RvXferMax", XFER_MAX.load(Ordering::Relaxed));
    rec(b"RvCopy", COPY.load(Ordering::Relaxed));
    rec(b"RvCopyFail", COPY_FAIL.load(Ordering::Relaxed));
    rec(b"RvWaitTmo", WAIT_TMO.load(Ordering::Relaxed));
    rec(b"RvCleared", CLEARED.load(Ordering::Relaxed));
    if FGN_REC.load(Ordering::Relaxed)
        | FGN_IMP.load(Ordering::Relaxed)
        | FGN_FAIL.load(Ordering::Relaxed)
        | FGN_WRITE.load(Ordering::Relaxed)
        != 0
    {
        rec(b"RvFgnRec", FGN_REC.load(Ordering::Relaxed));
        rec(b"RvFgnImp", FGN_IMP.load(Ordering::Relaxed));
        rec(b"RvFgnFail", FGN_FAIL.load(Ordering::Relaxed));
        rec(b"RvFgnWhy", FGN_WHY.load(Ordering::Relaxed));
        rec(b"RvFgnWrite", FGN_WRITE.load(Ordering::Relaxed));
    }
}

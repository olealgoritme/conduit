//! `RvOff` 0x4000 (opt-in): CDD's CPU-written GDI surfaces in the aperture segment, and the system
//! pages Windows gives them, as the KMD's view of their content.
//!
//! `D3DKMDT_GDISURFACE_STAGING_CPUVISIBLE` "must be a linear format and in a cache-coherent GPU
//! aperture segment" (d3dkmdt.h, `_D3DKMDT_GDISURFACETYPE`). Win32k draws into such a surface on
//! the CPU, through the system pages VidMm backs it with, and CDD then names it as the source of
//! RenderGdi copies. The KMD made these surfaces BAR-eligible (or RM system memory) and read their
//! content from the blob (`read_standard_buffer`), which win32k never writes: 388.1 read every
//! source row of CDD's staging buffer as zero, whatever the backing or the timing, and Explorer's
//! file list and the wallpaper composed black.
//!
//! With the bit set, those surfaces (`STAGING_CPUVISIBLE` and `LOOKUPTABLE`) are placed in the
//! aperture segment only (CpuVisible, Cached, a Venus blob that nothing reads), and the KMD records
//! the system pages Windows maps them to from `UPDATE_PAGE_TABLE` (valid segment-0 PTEs, page
//! `AllocationOffsetInBytes / 4096 + i`). `read_standard_buffer` / `write_standard_buffer` then
//! read and write those pages (mapped for the call), so the GDI executor's CPU path sees what
//! win32k drew. A page not (or no longer) mapped makes the read refuse, never read stale memory.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use wdk_sys::ntddk::{IoAllocateMdl, IoFreeMdl, MmMapLockedPagesSpecifyCache, MmUnmapLockedPages};
use wdk_sys::{_MEMORY_CACHING_TYPE, PMDL, ULONG};

use crate::dxgk::*;
use crate::sync::SpinLock;

/// Surfaces tracked at once (CDD keeps a handful of staging buffers and lookup tables).
const SLOTS: usize = 32;
/// The largest surface tracked (pages): 64 MiB.
const MAX_PAGES: usize = 16384;

struct Entry {
    resource_id: u32,
    /// The system page behind each page of the allocation, 0 when not mapped now.
    pfns: Vec<u64>,
}

static TABLE: SpinLock<[Option<Entry>; SLOTS]> = SpinLock::new([const { None }; SLOTS]);
static ANY: AtomicU32 = AtomicU32::new(0);

/// `RvApReg` (surfaces registered), `RvApPte` (system pages recorded), `RvApRd` / `RvApWr`
/// (reads / writes served from the pages), `RvApMiss` (a read or write that found a page not
/// mapped), `RvApFull` (a surface not tracked: the table full or too large).
static REG: AtomicU32 = AtomicU32::new(0);
static PTE: AtomicU32 = AtomicU32::new(0);
static RD: AtomicU32 = AtomicU32::new(0);
static WR: AtomicU32 = AtomicU32::new(0);
static MISS: AtomicU32 = AtomicU32::new(0);
static FULL: AtomicU32 = AtomicU32::new(0);

/// Whether the opt-in is in force (`RvOff` 0x4000, with `RedirVram` on).
pub(crate) fn on() -> bool {
    crate::virtio::rm_client::vidmem::knob_on()
        && crate::virtio::rm_client::vidmem::off(helios_kmd_logic::rm_vidmem::off::STAGING_APERTURE)
}

/// Whether a KMD standard allocation of this standard / GDI type goes to the aperture with its
/// pages tracked.
pub(crate) fn wants(std_type: u32, gdi_type: u32) -> bool {
    use helios_kmd_logic::rm_standard::{GDI_LOOKUPTABLE, GDI_STAGING_CPUVISIBLE, STD_GDISURFACE};
    on() && std_type == STD_GDISURFACE && (gdi_type == GDI_STAGING_CPUVISIBLE || gdi_type == GDI_LOOKUPTABLE)
}

/// Track `resource_id` (`size` bytes). PASSIVE (CreateAllocation): the page array is allocated
/// here, so the paging path never allocates.
pub(crate) fn register(resource_id: u32, size: u64) {
    let pages = size.div_ceil(4096) as usize;
    if resource_id == 0 || pages == 0 || pages > MAX_PAGES {
        FULL.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut pfns = Vec::new();
    if pfns.try_reserve_exact(pages).is_err() {
        FULL.fetch_add(1, Ordering::Relaxed);
        return;
    }
    pfns.resize(pages, 0);
    let entry = Entry { resource_id, pfns };
    let mut t = TABLE.lock();
    let slot = t
        .iter()
        .position(|e| e.as_ref().is_some_and(|e| e.resource_id == resource_id))
        .or_else(|| t.iter().position(Option::is_none));
    match slot {
        Some(i) => {
            t[i] = Some(entry);
            ANY.store(1, Ordering::Release);
            REG.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            FULL.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `resource_id` is being destroyed. PASSIVE.
pub(crate) fn forget(resource_id: u32) {
    if ANY.load(Ordering::Acquire) == 0 {
        return;
    }
    let old = {
        let mut t = TABLE.lock();
        t.iter_mut()
            .find(|e| e.as_ref().is_some_and(|e| e.resource_id == resource_id))
            .and_then(Option::take)
    };
    drop(old);
}

/// An `UPDATE_PAGE_TABLE` of `resource_id`: record the system pages its valid segment-0 PTEs name,
/// and clear the pages its other PTEs name (unmapped, or now in another segment). Spinlock only,
/// no allocation: any IRQL up to DISPATCH.
///
/// # Safety
/// `update` is dxgkrnl's live operation (its PTE array readable for `NumPageTableEntries`, or one
/// entry with `Repeat`).
pub(crate) unsafe fn note_update(resource_id: u32, update: &DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE) {
    if ANY.load(Ordering::Acquire) == 0
        || update.PageTableLevel != 0
        || update.pPageTableEntries.is_null()
        || update.NumPageTableEntries == 0
    {
        return;
    }
    let mut t = TABLE.lock();
    let Some(e) = t.iter_mut().flatten().find(|e| e.resource_id == resource_id) else {
        return;
    };
    let first = (update.AllocationOffsetInBytes / 4096) as usize;
    let repeat = update.Flags.Repeat() != 0;
    let mut recorded = 0u32;
    for i in 0..update.NumPageTableEntries as usize {
        let Some(slot) = e.pfns.get_mut(first + i) else {
            break;
        };
        // SAFETY: the caller's contract (one entry when `Repeat`).
        let pte = unsafe { core::ptr::read_unaligned(update.pPageTableEntries.add(if repeat { 0 } else { i })) };
        // SAFETY: plain bitfield / scalar reads of the PTE value.
        let bits = unsafe { pte.__bindgen_anon_1.__bindgen_anon_1 };
        if bits.Valid() != 0 && bits.Zero() == 0 && bits.Segment() == 0 {
            // SAFETY: as above.
            *slot = unsafe { pte.__bindgen_anon_2.PageAddress };
            recorded += 1;
        } else {
            *slot = 0;
        }
    }
    PTE.fetch_add(recorded, Ordering::Relaxed);
}

/// Whether `resource_id` is tracked (its content is its system pages).
pub(crate) fn tracked(resource_id: u32) -> bool {
    ANY.load(Ordering::Acquire) != 0
        && TABLE.lock().iter().flatten().any(|e| e.resource_id == resource_id)
}

/// The pages `[first, first + count)` of `resource_id`, if every one is mapped now.
fn pages_of(resource_id: u32, first: usize, count: usize, out: &mut Vec<u64>) -> bool {
    let t = TABLE.lock();
    let Some(e) = t.iter().flatten().find(|e| e.resource_id == resource_id) else {
        return false;
    };
    let Some(range) = e.pfns.get(first..first + count) else {
        return false;
    };
    if range.iter().any(|&p| p == 0) {
        return false;
    }
    out.extend_from_slice(range);
    true
}

/// `NormalPagePriority | MdlMappingNoExecute`.
const PRIORITY: u32 = 16 | 0x4000_0000;
/// `MDL_PAGES_LOCKED`: the pages are VidMm's, resident while mapped in the aperture.
const MDL_PAGES_LOCKED: i16 = 0x0002;

/// Map `pfns` (ordinary RAM) into system space for one call of `f(va)`. PASSIVE.
unsafe fn with_pages(pfns: &[u64], f: impl FnOnce(*mut u8)) -> bool {
    let Ok(len) = ULONG::try_from(pfns.len() * 4096) else {
        return false;
    };
    // SAFETY: a manually populated MDL with no virtual address (blob_map.rs's pattern); the
    // length is page-granular, so its PFN array holds exactly `pfns.len()` entries.
    let mdl: PMDL =
        unsafe { IoAllocateMdl(core::ptr::null_mut(), len, 0, 0, core::ptr::null_mut()) };
    if mdl.is_null() {
        return false;
    }
    // SAFETY: the PFN array follows the MDL header; `IoAllocateMdl` sized it for `len`.
    unsafe {
        let array = mdl.add(1).cast::<u64>();
        for (i, &p) in pfns.iter().enumerate() {
            array.add(i).write(p);
        }
        (*mdl).MdlFlags |= MDL_PAGES_LOCKED;
    }
    // SAFETY: KernelMode (0) does not raise on failure with BugCheckOnFailure = 0.
    let va = unsafe {
        MmMapLockedPagesSpecifyCache(
            mdl,
            0,
            _MEMORY_CACHING_TYPE::MmCached,
            core::ptr::null_mut(),
            0,
            PRIORITY,
        )
    };
    if va.is_null() {
        // SAFETY: our own MDL, never mapped.
        unsafe { IoFreeMdl(mdl) };
        return false;
    }
    f(va.cast::<u8>());
    // SAFETY: the mapping and MDL made above, released once.
    unsafe {
        MmUnmapLockedPages(va, mdl);
        IoFreeMdl(mdl);
    }
    true
}

/// Read `out.len()` bytes at `offset` of a tracked surface from its system pages. `None`: not
/// tracked (the caller reads as before); `Some(false)`: tracked, but a page is not mapped now.
/// PASSIVE.
pub(crate) fn read(resource_id: u32, offset: u64, out: &mut [u8]) -> Option<bool> {
    if !tracked(resource_id) {
        return None;
    }
    let ok = transfer(resource_id, offset, out.len(), |va, at| {
        // SAFETY: `va + at .. + len` lies inside the mapped pages (`transfer`).
        unsafe { core::ptr::copy_nonoverlapping(va.add(at), out.as_mut_ptr(), out.len()) };
    });
    if ok {
        RD.fetch_add(1, Ordering::Relaxed);
    } else {
        MISS.fetch_add(1, Ordering::Relaxed);
    }
    Some(ok)
}

/// Write `rows` rows of `row_bytes` (packed in `src`) at `offset`, `pitch` apart, into a tracked
/// surface's system pages. `None`: not tracked. PASSIVE.
pub(crate) fn write(
    resource_id: u32,
    offset: u64,
    pitch: u32,
    row_bytes: u32,
    rows: u32,
    src: &[u8],
) -> Option<bool> {
    if !tracked(resource_id) {
        return None;
    }
    let span = u64::from(pitch) * u64::from(rows.saturating_sub(1)) + u64::from(row_bytes);
    if rows == 0 || src.len() < row_bytes as usize * rows as usize {
        return Some(false);
    }
    let ok = transfer(resource_id, offset, span as usize, |va, at| {
        for r in 0..rows as usize {
            // SAFETY: row `r` lies inside the mapped span and inside `src`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr().add(r * row_bytes as usize),
                    va.add(at + r * pitch as usize),
                    row_bytes as usize,
                )
            };
        }
    });
    if ok {
        WR.fetch_add(1, Ordering::Relaxed);
    } else {
        MISS.fetch_add(1, Ordering::Relaxed);
    }
    Some(ok)
}

/// Map the pages covering `[offset, offset + len)` and run `f(va, byte offset of `offset` in the
/// mapping)`.
fn transfer(resource_id: u32, offset: u64, len: usize, f: impl FnOnce(*mut u8, usize)) -> bool {
    if len == 0 {
        return true;
    }
    let first = (offset / 4096) as usize;
    let end = offset + len as u64;
    let count = (end.div_ceil(4096) as usize).saturating_sub(first);
    let mut pfns = Vec::new();
    if pfns.try_reserve_exact(count).is_err() || !pages_of(resource_id, first, count, &mut pfns) {
        return false;
    }
    let at = (offset % 4096) as usize;
    // SAFETY: PASSIVE (the callers); the pages are the allocation's, mapped in the aperture now.
    unsafe { with_pages(&pfns, |va| f(va, at)) }
}

/// Mirror the counters (the `Nv*` mirror pass).
pub(crate) fn publish_counters() {
    if REG.load(Ordering::Relaxed) | FULL.load(Ordering::Relaxed) == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"RvApReg", REG.load(Ordering::Relaxed));
    rec(b"RvApPte", PTE.load(Ordering::Relaxed));
    rec(b"RvApRd", RD.load(Ordering::Relaxed));
    rec(b"RvApWr", WR.load(Ordering::Relaxed));
    rec(b"RvApMiss", MISS.load(Ordering::Relaxed));
    rec(b"RvApFull", FULL.load(Ordering::Relaxed));
}

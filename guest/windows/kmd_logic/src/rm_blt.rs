//! Level 5 (`KmdRmClient` = 5): the CPU-copy fallback for a Blt Present whose destination is
//! the RM system-memory primary. The pure half: rect arithmetic, the row-copy plan, the map
//! windows, the byte order rule and the accounting. No memory, no transport, no clock; the
//! I/O half is `kmd_render/src/virtio/rm_client/sysmem_blt.rs`. Design:
//! `docs/kmd-rm-client.md` section 15.17.
//!
//! WHY. The primary of level 5 is RM system memory (a foreign record, `venus_image_id` and
//! `venus_memory_id` zero), not a Venus buffer: the KMD's own Blt arm cannot GPU-copy into
//! it (`begin_present_buffer_write_legacy` knows no such Present buffer). So the pixels
//! reach it through the CPU: a Venus-backed source is GPU-copied into a host-visible LINEAR
//! staging image and read from its guest mapping, a CPU-visible source is read from its own
//! blob mapping, and only the destination rect(s) are written into the primary's mapping.
//!
//! WHAT A PRESENT CARRIES (`DXGKARG_PRESENT`): `DstRect` and `SrcRect` (both `RECT`),
//! `SubRectCnt` and `pDstSubRects` (sub-rectangles in destination space). There are no
//! move rectangles in this structure (those belong to `DXGKARG_PRESENT_DISPLAYONLY`).
//!
//! THE RULES, all of them tested here:
//!
//! * An EMPTY `DstRect` (all zero, or inverted) is "the whole destination": the KMD's own
//!   Blt arm has always copied the full surface and never read a rect, so a Present that
//!   leaves the rects unset must keep meaning that.
//! * A `DstRect` partly outside the primary is clamped to it; one entirely outside copies
//!   nothing (a successful no-op).
//! * `SubRectCnt` of zero means the whole (clamped) `DstRect`. Otherwise each sub-rect is
//!   clipped to it and the survivors are copied; sub-rects that are all empty or outside
//!   copy nothing. More than [`MAX_RECTS`] survivors, or a count over
//!   [`SUB_RECT_SCAN_MAX`], fall back to their bounding box / the whole `DstRect`: more
//!   bytes, never fewer.
//! * `SrcRect` places the source: destination pixel `p` reads source pixel
//!   `p + (SrcRect.origin - DstRect.origin)` when both rects are non-empty and the same
//!   size. Any other combination (a stretch, an unset rect) reads the source at the same
//!   coordinates, which is what the full-surface copy it replaces did.
//! * Everything is clamped so that no byte outside either surface's bytes is addressed.
//!
//! Pixels are 32 bits. The staging image and a BGRA source are bytes `B G R A`; an RGBA
//! source, or a primary created as `ABGR8888`/`XBGR8888`, is `R G B A`; the two differ by a
//! swap of bytes 0 and 2 ([`Swizzle`]).

use crate::foreign_resource::{FOURCC_ABGR8888, FOURCC_ARGB8888, FOURCC_XBGR8888, FOURCC_XRGB8888};
use crate::round_up_page;

/// Bytes per pixel of everything this copies.
pub const BPP: u32 = 4;
/// Page size of a mapping window.
pub const PAGE: u64 = 4096;
/// Rects of one Present that are copied one by one; more are folded into their bounding box.
pub const MAX_RECTS: usize = 32;
/// Sub-rects read from `pDstSubRects`; a Present that names more is copied whole (the list is
/// not trusted to be short, and a few thousand rects is a repaint anyway).
pub const SUB_RECT_SCAN_MAX: u32 = 4096;
/// The most bytes one transient kernel mapping of a surface is asked to cover (the rows of a
/// band, plus the page slack on either side). Bounds the PTEs one `MmMapIoSpace` builds.
pub const BAND_BYTES: u64 = 4 << 20;

/// Why a Present was answered with success but not copied (`RmSysBltWhy` carries the last).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Skip {
    /// The primary's layout is not one this copies (not 32 bpp of a known byte order, a
    /// pitch under the row, a size that does not hold the rows).
    Layout = 1,
    /// The source is a typed WindowedBlt snapshot: its content is ready only when its stream
    /// boundary is, which a synchronous copy cannot wait for.
    Snapshot = 2,
    /// The source's pixel format is not a 32-bit one with a known byte order (CPU source) or
    /// has no Present format (image source).
    SourceFormat = 3,
    /// The source is neither a Venus-backed image nor a CPU-visible allocation.
    SourceKind = 4,
    /// The source is the destination.
    SameResource = 5,
    /// The staging image could not be made.
    Stage = 6,
    /// The GPU copy into the staging image could not be submitted.
    GpuCopy = 7,
    /// The GPU copy did not complete (timeout, or the transport is gone).
    GpuWait = 8,
    /// Another Present held the staging image for longer than the wait.
    Busy = 9,
    /// The source's bytes could not be mapped.
    MapSrc = 10,
    /// The primary's bytes could not be mapped.
    MapDst = 11,
    /// The rect arithmetic found a layout that does not hold the copy.
    Plan = 12,
    /// No Venus client (the transport is down).
    NoVenus = 13,
}

impl Skip {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// A rectangle in `RECT` terms: `left`/`top` inclusive, `right`/`bottom` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// No pixels: zero or negative width or height (an unset `RECT` is all zero).
    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

/// Wide working rect: nothing here may overflow however hostile a `RECT` is.
#[derive(Debug, Clone, Copy)]
struct R {
    l: i64,
    t: i64,
    r: i64,
    b: i64,
}

impl R {
    fn from(r: Rect) -> R {
        R {
            l: i64::from(r.left),
            t: i64::from(r.top),
            r: i64::from(r.right),
            b: i64::from(r.bottom),
        }
    }

    fn empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }

    fn and(&self, o: &R) -> R {
        R {
            l: self.l.max(o.l),
            t: self.t.max(o.t),
            r: self.r.min(o.r),
            b: self.b.min(o.b),
        }
    }

    fn or(&self, o: &R) -> R {
        R {
            l: self.l.min(o.l),
            t: self.t.min(o.t),
            r: self.r.max(o.r),
            b: self.b.max(o.b),
        }
    }
}

/// A 32-bit pixel surface inside a mapped blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub pitch: u32,
    /// Byte offset of row 0 from the start of the blob (`plane_offset`).
    pub offset: u64,
    /// Bytes of the blob a view may address (the blob's size, not the mapping's).
    pub size: u64,
}

impl Surface {
    pub const fn new(width: u32, height: u32, pitch: u32, offset: u64, size: u64) -> Self {
        Surface {
            width,
            height,
            pitch,
            offset,
            size,
        }
    }

    /// The rows hold the pixels and the blob holds the rows.
    pub fn valid(&self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let Some(row) = self.width.checked_mul(BPP) else {
            return false;
        };
        if self.pitch < row || self.pitch % BPP != 0 {
            return false;
        }
        let last = u64::from(self.height - 1) * u64::from(self.pitch) + u64::from(row);
        match self.offset.checked_add(last) {
            Some(end) => end <= self.size,
            None => false,
        }
    }

    /// The whole surface as a rect.
    pub fn full(&self) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: self.width.min(i32::MAX as u32) as i32,
            bottom: self.height.min(i32::MAX as u32) as i32,
        }
    }

    /// Byte offset of pixel (`x`, `y`) from the start of the blob.
    pub fn at(&self, x: u32, y: u32) -> u64 {
        self.offset + u64::from(y) * u64::from(self.pitch) + u64::from(x) * u64::from(BPP)
    }
}

/// One rectangle to copy: destination pixels `[x0, x1) x [y0, y1)`, read from the source at
/// (`sx`, `sy`) for the destination pixel (`x0`, `y0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RectCopy {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub sx: u32,
    pub sy: u32,
}

impl RectCopy {
    pub const fn rows(&self) -> u32 {
        self.y1 - self.y0
    }

    pub const fn row_bytes(&self) -> u64 {
        (self.x1 - self.x0) as u64 * BPP as u64
    }

    /// Bytes of pixels this rect moves.
    pub const fn bytes(&self) -> u64 {
        self.rows() as u64 * self.row_bytes()
    }

    /// Offset in the destination blob of the first byte of destination row `y`.
    pub fn dst_row(&self, dst: &Surface, y: u32) -> u64 {
        dst.at(self.x0, y)
    }

    /// Offset in the source blob of the first byte read for destination row `y`.
    pub fn src_row(&self, src: &Surface, y: u32) -> u64 {
        src.at(self.sx, self.sy + (y - self.y0))
    }

    /// The bytes of the destination blob rows `y0..y1` of this rect touch, `(first, end)`.
    pub fn dst_span(&self, dst: &Surface, y0: u32, y1: u32) -> Option<(u64, u64)> {
        if y1 <= y0 || y0 < self.y0 || y1 > self.y1 {
            return None;
        }
        Some((
            self.dst_row(dst, y0),
            self.dst_row(dst, y1 - 1) + self.row_bytes(),
        ))
    }

    /// The bytes of the source blob read for destination rows `y0..y1`.
    pub fn src_span(&self, src: &Surface, y0: u32, y1: u32) -> Option<(u64, u64)> {
        if y1 <= y0 || y0 < self.y0 || y1 > self.y1 {
            return None;
        }
        Some((
            self.src_row(src, y0),
            self.src_row(src, y1 - 1) + self.row_bytes(),
        ))
    }
}

/// The rects one Present copies. A fixed array: nothing allocates at the Present DDI.
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    rects: [RectCopy; MAX_RECTS],
    len: usize,
}

impl Plan {
    const fn new() -> Plan {
        Plan {
            rects: [RectCopy {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
                sx: 0,
                sy: 0,
            }; MAX_RECTS],
            len: 0,
        }
    }

    fn push(&mut self, r: RectCopy) -> bool {
        if self.len >= MAX_RECTS {
            return false;
        }
        self.rects[self.len] = r;
        self.len += 1;
        true
    }

    pub fn rects(&self) -> &[RectCopy] {
        &self.rects[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes of pixels the plan moves (what `RmSysBltBytes` adds).
    pub fn bytes(&self) -> u64 {
        self.rects()
            .iter()
            .fold(0u64, |a, r| a.saturating_add(r.bytes()))
    }
}

fn rect_copy(r: &R, dx: i64, dy: i64) -> Option<RectCopy> {
    let x0 = u32::try_from(r.l).ok()?;
    let y0 = u32::try_from(r.t).ok()?;
    let x1 = u32::try_from(r.r).ok()?;
    let y1 = u32::try_from(r.b).ok()?;
    let sx = u32::try_from(r.l + dx).ok()?;
    let sy = u32::try_from(r.t + dy).ok()?;
    Some(RectCopy {
        x0,
        y0,
        x1,
        y1,
        sx,
        sy,
    })
}

/// The copy plan of one Present: `dst` the primary, `src` the surface read (the staging
/// image or the source blob), `dst_rect`/`src_rect` the Present's rects, `sub_count` its
/// `SubRectCnt` and `subs` its `pDstSubRects` (read lazily, at most
/// [`SUB_RECT_SCAN_MAX`] of them).
///
/// `Ok` with an empty plan is a successful Present that copies nothing.
pub fn plan<I: Iterator<Item = Rect>>(
    dst: &Surface,
    src: &Surface,
    dst_rect: Rect,
    src_rect: Rect,
    sub_count: u32,
    subs: I,
) -> Result<Plan, Skip> {
    if !dst.valid() || !src.valid() {
        return Err(Skip::Layout);
    }
    let dst_full = R::from(dst.full());
    // The Present's own window, in destination space.
    let window = if dst_rect.is_empty() {
        dst_full
    } else {
        R::from(dst_rect).and(&dst_full)
    };
    // Source placement: the origin delta of two same-sized rects, else none.
    let (dx, dy) = if !dst_rect.is_empty()
        && !src_rect.is_empty()
        && i64::from(dst_rect.right) - i64::from(dst_rect.left)
            == i64::from(src_rect.right) - i64::from(src_rect.left)
        && i64::from(dst_rect.bottom) - i64::from(dst_rect.top)
            == i64::from(src_rect.bottom) - i64::from(src_rect.top)
    {
        (
            i64::from(src_rect.left) - i64::from(dst_rect.left),
            i64::from(src_rect.top) - i64::from(dst_rect.top),
        )
    } else {
        (0, 0)
    };
    // The destination pixels whose source pixel exists.
    let readable = R {
        l: -dx,
        t: -dy,
        r: i64::from(src.width) - dx,
        b: i64::from(src.height) - dy,
    };
    let bound = window.and(&readable);
    let mut plan = Plan::new();
    if bound.empty() {
        return Ok(plan);
    }
    if sub_count == 0 || sub_count > SUB_RECT_SCAN_MAX {
        plan.push(rect_copy(&bound, dx, dy).ok_or(Skip::Plan)?);
        return finish(plan, dst, src);
    }
    let mut hull: Option<R> = None;
    let mut survivors = 0usize;
    for sub in subs.take(sub_count as usize) {
        let r = R::from(sub).and(&bound);
        if r.empty() {
            continue;
        }
        survivors += 1;
        hull = Some(match hull {
            Some(h) => h.or(&r),
            None => r,
        });
        if survivors <= MAX_RECTS {
            plan.push(rect_copy(&r, dx, dy).ok_or(Skip::Plan)?);
        }
    }
    if survivors > MAX_RECTS {
        // Too many to copy one by one: their bounding box, which holds every one of them.
        plan = Plan::new();
        if let Some(h) = hull {
            plan.push(rect_copy(&h, dx, dy).ok_or(Skip::Plan)?);
        }
    }
    finish(plan, dst, src)
}

/// The last proof: every byte every rect names is inside both surfaces.
fn finish(plan: Plan, dst: &Surface, src: &Surface) -> Result<Plan, Skip> {
    for r in plan.rects() {
        if r.x1 <= r.x0 || r.y1 <= r.y0 || r.x1 > dst.width || r.y1 > dst.height {
            return Err(Skip::Plan);
        }
        let (Some(sx1), Some(sy1)) = (r.sx.checked_add(r.x1 - r.x0), r.sy.checked_add(r.y1 - r.y0))
        else {
            return Err(Skip::Plan);
        };
        if sx1 > src.width || sy1 > src.height {
            return Err(Skip::Plan);
        }
        let (Some((_, d_end)), Some((_, s_end))) =
            (r.dst_span(dst, r.y0, r.y1), r.src_span(src, r.y0, r.y1))
        else {
            return Err(Skip::Plan);
        };
        if d_end > dst.size || s_end > src.size {
            return Err(Skip::Plan);
        }
    }
    Ok(plan)
}

// ---- bands and map windows ------------------------------------------------------------------

/// How many rows one band of a rect covers: enough that the larger of the two byte spans,
/// with a page of slack at each end, stays inside `max_span`; at least one row.
pub fn rows_per_band(row_bytes: u64, pitch_a: u32, pitch_b: u32, max_span: u64) -> u32 {
    let pitch = u64::from(pitch_a.max(pitch_b));
    if pitch == 0 {
        return 1;
    }
    let spare = max_span.saturating_sub(row_bytes.saturating_add(2 * PAGE));
    let rows = 1 + spare / pitch;
    rows.min(u64::from(u32::MAX)) as u32
}

/// The row bands `(y0, y1)` of one rect, top to bottom, none empty, together exactly its rows.
#[derive(Debug, Clone, Copy)]
pub struct Bands {
    next: u32,
    end: u32,
    step: u32,
}

impl Iterator for Bands {
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        if self.next >= self.end {
            return None;
        }
        let y0 = self.next;
        let y1 = y0.saturating_add(self.step).min(self.end);
        self.next = y1;
        Some((y0, y1))
    }
}

/// The bands of `rect` against two surfaces of the given pitches, each mapping at most
/// [`BAND_BYTES`] (plus page slack).
pub fn bands(rect: &RectCopy, dst: &Surface, src: &Surface) -> Bands {
    Bands {
        next: rect.y0,
        end: rect.y1,
        step: rows_per_band(rect.row_bytes(), dst.pitch, src.pitch, BAND_BYTES).max(1),
    }
}

/// The page-aligned window of a blob mapping that holds the bytes `[first, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapWindow {
    /// Offset from the start of the blob mapping, a multiple of [`PAGE`].
    pub start: u64,
    /// Bytes to map, a multiple of [`PAGE`].
    pub len: u64,
    /// `first - start`: where the bytes begin inside the window.
    pub delta: u64,
}

/// The window for `[first, end)` of a blob mapping `limit` bytes long (page-rounded); `None`
/// if the range is empty or leaves the mapping.
pub fn map_window(first: u64, end: u64, limit: u64) -> Option<MapWindow> {
    if end <= first {
        return None;
    }
    let start = first & !(PAGE - 1);
    let stop = round_up_page(end);
    if stop < end || stop > limit {
        return None;
    }
    Some(MapWindow {
        start,
        len: stop - start,
        delta: first - start,
    })
}

// ---- byte order -----------------------------------------------------------------------------

/// The memory order of a 32-bit pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Bytes `B G R A|X` (`B8G8R8A8`/`B8G8R8X8`, `DRM_FORMAT_ARGB8888`/`XRGB8888`).
    Bgra,
    /// Bytes `R G B A|X` (`R8G8B8A8`, `DRM_FORMAT_ABGR8888`/`XBGR8888`).
    Rgba,
}

/// The order of DXGI format `dxgi` if it is a 32-bit 8-bit-per-channel one (28/29 R8G8B8A8,
/// 87/91 B8G8R8A8, 88/93 B8G8R8X8); `None` for 10-bit, half float and the rest.
pub const fn order_for_dxgi(dxgi: u32) -> Option<Order> {
    match dxgi {
        87 | 88 | 91 | 93 => Some(Order::Bgra),
        28 | 29 => Some(Order::Rgba),
        _ => None,
    }
}

/// The order of a primary's `DRM_FORMAT_*`.
pub const fn order_for_fourcc(fourcc: u32) -> Option<Order> {
    match fourcc {
        FOURCC_XRGB8888 | FOURCC_ARGB8888 => Some(Order::Bgra),
        FOURCC_XBGR8888 | FOURCC_ABGR8888 => Some(Order::Rgba),
        _ => None,
    }
}

/// What a row copy does to the pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swizzle {
    /// Bytes as they are.
    None,
    /// Bytes 0 and 2 of each pixel exchanged.
    SwapRb,
}

pub const fn swizzle(from: Order, to: Order) -> Swizzle {
    match (from, to) {
        (Order::Bgra, Order::Bgra) | (Order::Rgba, Order::Rgba) => Swizzle::None,
        _ => Swizzle::SwapRb,
    }
}

/// One row with bytes 0 and 2 of each 4-byte pixel exchanged. Copies
/// `min(dst.len(), src.len())` rounded down to whole pixels.
pub fn swap_rb_row(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len()) & !3;
    let mut i = 0;
    while i < n {
        dst[i] = src[i + 2];
        dst[i + 1] = src[i + 1];
        dst[i + 2] = src[i];
        dst[i + 3] = src[i + 3];
        i += 4;
    }
}

// ---- accounting -----------------------------------------------------------------------------

/// Whole MiB of `bytes`, saturating at `u32::MAX` (the counter's width).
pub fn mib(bytes: u64) -> u32 {
    (bytes >> 20).min(u64::from(u32::MAX)) as u32
}

/// Microseconds of a span in 100 ns units (interrupt time).
pub const fn micros(delta_100ns: u64) -> u64 {
    delta_100ns / 10
}

/// A 64-bit counter as the registry's 32-bit value.
pub fn sat32(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// The 5120x1440 primary of the host measurements: pitch 20480, 29,491,200 bytes.
    fn primary() -> Surface {
        Surface::new(5120, 1440, 20480, 0, 29_491_200)
    }

    /// A source of the same size read from a staging image (pitch from Vulkan: 20480 here).
    fn stage() -> Surface {
        Surface::new(5120, 1440, 20480, 0, 29_491_200)
    }

    fn run(dst: &Surface, src: &Surface, d: Rect, s: Rect, subs: &[Rect]) -> Result<Plan, Skip> {
        plan(dst, src, d, s, subs.len() as u32, subs.iter().copied())
    }

    fn one(p: &Plan) -> RectCopy {
        assert_eq!(p.rects().len(), 1, "{p:?}");
        p.rects()[0]
    }

    // ---- the surface ------------------------------------------------------------------------

    #[test]
    fn the_5120x1440_primary_is_a_valid_surface() {
        let p = primary();
        assert!(p.valid());
        assert_eq!(p.pitch, 5120 * 4);
        assert_eq!(p.at(0, 0), 0);
        assert_eq!(p.at(1, 0), 4);
        assert_eq!(p.at(0, 1), 20480);
        assert_eq!(p.at(5119, 1439), 29_491_200 - 4);
        assert_eq!(
            p.full(),
            Rect {
                left: 0,
                top: 0,
                right: 5120,
                bottom: 1440
            }
        );
    }

    #[test]
    fn a_surface_that_does_not_hold_its_rows_is_not_valid() {
        assert!(!Surface::new(0, 10, 40, 0, 400).valid());
        assert!(!Surface::new(10, 0, 40, 0, 400).valid());
        // pitch under the row
        assert!(!Surface::new(10, 10, 36, 0, 400).valid());
        // pitch not a whole pixel
        assert!(!Surface::new(10, 10, 42, 0, 420).valid());
        // blob one byte short of the last row
        assert!(!Surface::new(10, 10, 40, 0, 399).valid());
        assert!(Surface::new(10, 10, 40, 0, 400).valid());
        // the plane offset counts
        assert!(!Surface::new(10, 10, 40, 4096, 4096 + 399).valid());
        assert!(Surface::new(10, 10, 40, 4096, 4096 + 400).valid());
        // the last row needs only the pixels, not the pitch
        assert!(Surface::new(10, 10, 48, 0, 9 * 48 + 40).valid());
        // overflow
        assert!(!Surface::new(1 << 30, 4, 1 << 31, u64::MAX - 5, u64::MAX).valid());
    }

    // ---- zero SubRectCnt: the whole rect ------------------------------------------------------

    #[test]
    fn no_sub_rects_and_no_rects_is_the_whole_primary() {
        let p = run(&primary(), &stage(), Rect::default(), Rect::default(), &[]).unwrap();
        let r = one(&p);
        assert_eq!(
            (r.x0, r.y0, r.x1, r.y1, r.sx, r.sy),
            (0, 0, 5120, 1440, 0, 0)
        );
        assert_eq!(p.bytes(), 29_491_200);
    }

    #[test]
    fn zero_sub_rect_count_is_the_dst_rect_even_with_a_list() {
        // SubRectCnt 0 with a (stale) pointer: the count rules, the iterator is never read.
        let d = Rect::new(100, 50, 300, 150);
        let p = plan(
            &primary(),
            &stage(),
            d,
            d,
            0,
            [Rect::new(0, 0, 1, 1)].iter().copied(),
        )
        .unwrap();
        let r = one(&p);
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (100, 50, 300, 150));
        assert_eq!((r.sx, r.sy), (100, 50));
        assert_eq!(p.bytes(), 200 * 100 * 4);
    }

    #[test]
    fn an_inverted_or_degenerate_dst_rect_is_the_whole_primary() {
        for d in [
            Rect::new(10, 10, 10, 20),
            Rect::new(10, 10, 20, 10),
            Rect::new(300, 300, 100, 100),
        ] {
            let p = run(&primary(), &stage(), d, Rect::default(), &[]).unwrap();
            assert_eq!(p.bytes(), 29_491_200, "{d:?}");
        }
    }

    // ---- clamping -----------------------------------------------------------------------------

    #[test]
    fn a_rect_partly_outside_is_clamped_to_the_primary() {
        let d = Rect::new(-50, -20, 100, 60);
        let p = run(&primary(), &stage(), d, d, &[]).unwrap();
        let r = one(&p);
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (0, 0, 100, 60));
        // same-size src rect: the source follows the clamp
        assert_eq!((r.sx, r.sy), (0, 0));
        let d = Rect::new(5000, 1400, 6000, 2000);
        let p = run(&primary(), &stage(), d, d, &[]).unwrap();
        let r = one(&p);
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (5000, 1400, 5120, 1440));
        assert_eq!(p.bytes(), 120 * 40 * 4);
    }

    #[test]
    fn a_rect_entirely_outside_copies_nothing_and_succeeds() {
        for d in [
            Rect::new(-100, -100, -1, -1),
            Rect::new(5120, 0, 6000, 100),
            Rect::new(0, 1440, 100, 2000),
            Rect::new(i32::MIN, i32::MIN, i32::MIN + 5, i32::MIN + 5),
            Rect::new(i32::MAX - 5, i32::MAX - 5, i32::MAX, i32::MAX),
        ] {
            let p = run(&primary(), &stage(), d, d, &[]).unwrap();
            assert!(p.is_empty(), "{d:?}");
            assert_eq!(p.bytes(), 0);
        }
    }

    #[test]
    fn an_extreme_rect_does_not_overflow() {
        let d = Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        let p = run(&primary(), &stage(), d, Rect::default(), &[]).unwrap();
        assert_eq!(p.bytes(), 29_491_200);
        let subs = [Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX)];
        let p = run(
            &primary(),
            &stage(),
            Rect::default(),
            Rect::default(),
            &subs,
        )
        .unwrap();
        assert_eq!(p.bytes(), 29_491_200);
    }

    // ---- sub-rects ----------------------------------------------------------------------------

    #[test]
    fn sub_rects_are_clipped_to_the_dst_rect_and_the_primary() {
        let d = Rect::new(100, 100, 600, 400);
        let subs = [
            Rect::new(0, 0, 150, 150),     // clipped to 100..150
            Rect::new(550, 350, 700, 500), // clipped to ..600, ..400
            Rect::new(200, 200, 200, 300), // empty: dropped
            Rect::new(800, 800, 900, 900), // outside the dst rect: dropped
            Rect::new(300, 300, 301, 301), // one pixel
        ];
        let p = run(&primary(), &stage(), d, d, &subs).unwrap();
        let got: Vec<_> = p.rects().iter().map(|r| (r.x0, r.y0, r.x1, r.y1)).collect();
        assert_eq!(
            got,
            std::vec![
                (100, 100, 150, 150),
                (550, 350, 600, 400),
                (300, 300, 301, 301)
            ]
        );
        assert_eq!(p.bytes(), (50 * 50 + 50 * 50 + 1) * 4);
    }

    #[test]
    fn sub_rects_all_empty_or_outside_copy_nothing_not_everything() {
        let subs = [Rect::new(5, 5, 5, 5), Rect::new(-9, -9, -1, -1)];
        let p = run(
            &primary(),
            &stage(),
            Rect::default(),
            Rect::default(),
            &subs,
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn a_pointer_with_a_count_of_one_empty_rect_is_still_a_list() {
        let subs = [Rect::default()];
        let p = run(
            &primary(),
            &stage(),
            Rect::default(),
            Rect::default(),
            &subs,
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn more_than_max_rects_fold_into_the_bounding_box() {
        let subs: Vec<Rect> = (0..(MAX_RECTS as i32 + 8))
            .map(|i| Rect::new(i * 10, i * 4, i * 10 + 5, i * 4 + 3))
            .collect();
        let p = run(
            &primary(),
            &stage(),
            Rect::default(),
            Rect::default(),
            &subs,
        )
        .unwrap();
        let r = one(&p);
        let last = MAX_RECTS as i32 + 7;
        assert_eq!(
            (r.x0, r.y0, r.x1, r.y1),
            (0, 0, (last * 10 + 5) as u32, (last * 4 + 3) as u32)
        );
        // every sub-rect is inside it
        for s in &subs {
            assert!(s.left as u32 >= r.x0 && s.right as u32 <= r.x1);
            assert!(s.top as u32 >= r.y0 && s.bottom as u32 <= r.y1);
        }
    }

    #[test]
    fn exactly_max_rects_are_copied_one_by_one() {
        let subs: Vec<Rect> = (0..MAX_RECTS as i32)
            .map(|i| Rect::new(i * 10, 0, i * 10 + 5, 3))
            .collect();
        let p = run(
            &primary(),
            &stage(),
            Rect::default(),
            Rect::default(),
            &subs,
        )
        .unwrap();
        assert_eq!(p.rects().len(), MAX_RECTS);
        assert_eq!(p.bytes(), MAX_RECTS as u64 * 5 * 3 * 4);
    }

    #[test]
    fn a_count_over_the_scan_limit_is_the_whole_window_and_reads_no_list() {
        let d = Rect::new(10, 10, 110, 110);
        let p = plan(
            &primary(),
            &stage(),
            d,
            d,
            SUB_RECT_SCAN_MAX + 1,
            core::iter::empty(),
        )
        .unwrap();
        assert_eq!(p.bytes(), 100 * 100 * 4);
        // at the limit the list is read
        let p = plan(
            &primary(),
            &stage(),
            d,
            d,
            SUB_RECT_SCAN_MAX,
            core::iter::repeat(Rect::new(10, 10, 11, 11)).take(SUB_RECT_SCAN_MAX as usize),
        )
        .unwrap();
        // 4096 one-pixel survivors are more than MAX_RECTS: their bounding box, one pixel
        assert_eq!(p.rects().len(), 1);
        assert_eq!(p.bytes(), 4);
    }

    #[test]
    fn a_short_list_is_taken_as_far_as_it_goes() {
        // SubRectCnt says 5, the iterator has 2: the two are copied.
        let p = plan(
            &primary(),
            &stage(),
            Rect::default(),
            Rect::default(),
            5,
            [Rect::new(0, 0, 4, 4), Rect::new(8, 8, 12, 12)].into_iter(),
        )
        .unwrap();
        assert_eq!(p.rects().len(), 2);
    }

    // ---- the source placement -----------------------------------------------------------------

    #[test]
    fn same_sized_rects_place_the_source_by_their_origin_delta() {
        let d = Rect::new(200, 100, 300, 160);
        let s = Rect::new(20, 10, 120, 70);
        let p = run(&primary(), &stage(), d, s, &[]).unwrap();
        let r = one(&p);
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (200, 100, 300, 160));
        assert_eq!((r.sx, r.sy), (20, 10));
        // row 105 reads source row 15
        assert_eq!(r.src_row(&stage(), 105), stage().at(20, 15));
        assert_eq!(r.dst_row(&primary(), 105), primary().at(200, 105));
    }

    #[test]
    fn a_stretch_or_an_unset_src_rect_reads_the_source_at_the_same_place() {
        let d = Rect::new(200, 100, 300, 160);
        for s in [Rect::new(0, 0, 50, 30), Rect::default()] {
            let p = run(&primary(), &stage(), d, s, &[]).unwrap();
            let r = one(&p);
            assert_eq!((r.sx, r.sy), (200, 100), "{s:?}");
        }
    }

    #[test]
    fn a_source_smaller_than_the_primary_limits_the_copy_to_what_it_has() {
        let small = Surface::new(1920, 1080, 7680, 0, 7680 * 1080);
        let p = run(&primary(), &small, Rect::default(), Rect::default(), &[]).unwrap();
        let r = one(&p);
        assert_eq!((r.x1, r.y1), (1920, 1080));
        assert_eq!(p.bytes(), 1920 * 1080 * 4);
        // a shifted source: the part of the dst rect whose source exists
        let d = Rect::new(0, 0, 100, 100);
        let s = Rect::new(1880, 1040, 1980, 1140);
        let p = run(&primary(), &small, d, s, &[]).unwrap();
        let r = one(&p);
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (0, 0, 40, 40));
        assert_eq!((r.sx, r.sy), (1880, 1040));
        // a shift that leaves the source entirely copies nothing
        let s = Rect::new(2000, 2000, 2100, 2100);
        let p = run(&primary(), &small, d, s, &[]).unwrap();
        assert!(p.is_empty());
        // a negative shift: the leading part has no source
        let d = Rect::new(100, 100, 200, 200);
        let s = Rect::new(60, 60, 160, 160);
        let p = run(&primary(), &small, d, s, &[]).unwrap();
        let r = one(&p);
        assert_eq!((r.x0, r.y0, r.sx, r.sy), (100, 100, 60, 60));
        let d = Rect::new(0, 0, 100, 100);
        let s = Rect::new(60, 60, 160, 160);
        let p = run(&primary(), &small, d, s, &[]).unwrap();
        let r = one(&p);
        assert_eq!(
            (r.x0, r.y0, r.x1, r.y1, r.sx, r.sy),
            (0, 0, 100, 100, 60, 60)
        );
    }

    #[test]
    fn a_source_with_a_plane_offset_and_a_wider_pitch_is_addressed_from_it() {
        // Vulkan's LINEAR layout: row pitch may exceed the row, plane offset may be nonzero.
        let src = Surface::new(1920, 1080, 7936, 4096, 4096 + 7936 * 1080);
        let dst = Surface::new(1920, 1080, 7680, 0, 7680 * 1080);
        let d = Rect::new(10, 20, 110, 70);
        let p = run(&dst, &src, d, d, &[]).unwrap();
        let r = one(&p);
        assert_eq!(r.src_row(&src, 20), 4096 + 20 * 7936 + 10 * 4);
        assert_eq!(r.dst_row(&dst, 20), 20 * 7680 + 10 * 4);
        assert_eq!(r.row_bytes(), 400);
        assert_eq!(
            r.src_span(&src, 20, 70).unwrap(),
            (4096 + 20 * 7936 + 40, 4096 + 69 * 7936 + 40 + 400)
        );
    }

    #[test]
    fn a_layout_that_does_not_hold_the_copy_is_refused() {
        let bad = Surface::new(5120, 1440, 20480, 0, 29_491_199);
        assert_eq!(
            run(&bad, &stage(), Rect::default(), Rect::default(), &[]).err(),
            Some(Skip::Layout)
        );
        assert_eq!(
            run(&primary(), &bad, Rect::default(), Rect::default(), &[]).err(),
            Some(Skip::Layout)
        );
    }

    // ---- the whole-frame numbers --------------------------------------------------------------

    #[test]
    fn a_full_5120x1440_frame_is_29_491_200_bytes_or_28_mib() {
        let p = run(&primary(), &stage(), Rect::default(), Rect::default(), &[]).unwrap();
        assert_eq!(p.bytes(), 29_491_200);
        assert_eq!(mib(p.bytes()), 28);
        assert_eq!(mib(1 << 20), 1);
        assert_eq!(mib((1 << 20) - 1), 0);
        assert_eq!(mib(u64::MAX), u32::MAX);
    }

    #[test]
    fn a_rect_limits_the_bytes() {
        // a 400x40 text line: 64,000 bytes, not 29 MB
        let d = Rect::new(1000, 600, 1400, 640);
        let p = run(&primary(), &stage(), d, d, &[]).unwrap();
        assert_eq!(p.bytes(), 64_000);
    }

    // ---- bands --------------------------------------------------------------------------------

    #[test]
    fn a_full_frame_is_cut_into_bands_that_cover_every_row_once() {
        let p = run(&primary(), &stage(), Rect::default(), Rect::default(), &[]).unwrap();
        let r = one(&p);
        let bs: Vec<_> = bands(&r, &primary(), &stage()).collect();
        assert_eq!(bs.first().unwrap().0, 0);
        assert_eq!(bs.last().unwrap().1, 1440);
        for w in bs.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        let rows: u32 = bs.iter().map(|b| b.1 - b.0).sum();
        assert_eq!(rows, 1440);
        assert!(bs.len() >= 7 && bs.len() <= 9, "{}", bs.len());
        for (y0, y1) in bs {
            assert!(y1 > y0);
            let (f, e) = r.dst_span(&primary(), y0, y1).unwrap();
            let w = map_window(f, e, 29_491_200).unwrap();
            assert!(w.len <= BAND_BYTES + 2 * PAGE, "{}", w.len);
        }
    }

    #[test]
    fn a_small_rect_is_one_band() {
        let d = Rect::new(1000, 600, 1400, 640);
        let p = run(&primary(), &stage(), d, d, &[]).unwrap();
        let r = one(&p);
        let bs: Vec<_> = bands(&r, &primary(), &stage()).collect();
        assert_eq!(bs, std::vec![(600, 640)]);
    }

    #[test]
    fn the_band_follows_the_larger_pitch() {
        let wide = Surface::new(1920, 1080, 1 << 20, 0, (1 << 20) * 1080);
        let n = rows_per_band(7680, 7680, wide.pitch, BAND_BYTES);
        assert!(n >= 1 && u64::from(n) * (1 << 20) <= BAND_BYTES + (1 << 20));
        // a row bigger than the budget is still one row
        assert_eq!(rows_per_band(10 << 20, 1 << 20, 1 << 20, BAND_BYTES), 1);
        assert_eq!(rows_per_band(100, 0, 0, BAND_BYTES), 1);
    }

    #[test]
    fn bands_of_an_empty_rect_are_empty() {
        let r = RectCopy {
            x0: 5,
            y0: 5,
            x1: 9,
            y1: 5,
            sx: 5,
            sy: 5,
        };
        assert_eq!(bands(&r, &primary(), &stage()).count(), 0);
    }

    #[test]
    fn spans_refuse_rows_outside_the_rect() {
        let d = Rect::new(10, 10, 20, 20);
        let p = run(&primary(), &stage(), d, d, &[]).unwrap();
        let r = one(&p);
        assert!(r.dst_span(&primary(), 9, 12).is_none());
        assert!(r.dst_span(&primary(), 10, 21).is_none());
        assert!(r.dst_span(&primary(), 12, 12).is_none());
        assert!(r.dst_span(&primary(), 10, 20).is_some());
    }

    // ---- map windows --------------------------------------------------------------------------

    #[test]
    fn a_map_window_is_page_aligned_and_holds_the_bytes() {
        let w = map_window(5000, 9000, 1 << 20).unwrap();
        assert_eq!(w.start, 4096);
        assert_eq!(w.len, 8192);
        assert_eq!(w.delta, 904);
        assert!(w.start + w.delta + (9000 - 5000) <= w.start + w.len);
        let w = map_window(0, 1, 4096).unwrap();
        assert_eq!((w.start, w.len, w.delta), (0, 4096, 0));
        let w = map_window(4096, 8192, 8192).unwrap();
        assert_eq!((w.start, w.len, w.delta), (4096, 4096, 0));
    }

    #[test]
    fn a_map_window_never_leaves_the_mapping() {
        assert!(map_window(0, 4097, 4096).is_none());
        assert!(map_window(8192, 8193, 8192).is_none());
        assert!(map_window(10, 10, 4096).is_none());
        assert!(map_window(11, 10, 4096).is_none());
        assert!(map_window(u64::MAX - 1, u64::MAX, u64::MAX).is_none());
    }

    #[test]
    fn the_last_row_of_the_primary_maps_inside_the_blob() {
        let p = primary();
        let d = Rect::new(0, 1439, 5120, 1440);
        let plan_ = run(&p, &stage(), d, d, &[]).unwrap();
        let r = one(&plan_);
        let (f, e) = r.dst_span(&p, 1439, 1440).unwrap();
        assert_eq!(e, 29_491_200);
        let w = map_window(f, e, 29_491_200).unwrap();
        assert_eq!(w.start + w.len, 29_491_200);
    }

    // ---- byte order ---------------------------------------------------------------------------

    #[test]
    fn the_byte_order_of_every_format_this_copies() {
        for d in [87, 88, 91, 93] {
            assert_eq!(order_for_dxgi(d), Some(Order::Bgra), "{d}");
        }
        for d in [28, 29] {
            assert_eq!(order_for_dxgi(d), Some(Order::Rgba), "{d}");
        }
        for d in [0, 10, 24, 2, 61] {
            assert_eq!(order_for_dxgi(d), None, "{d}");
        }
        assert_eq!(order_for_fourcc(FOURCC_XRGB8888), Some(Order::Bgra));
        assert_eq!(order_for_fourcc(FOURCC_ARGB8888), Some(Order::Bgra));
        assert_eq!(order_for_fourcc(FOURCC_XBGR8888), Some(Order::Rgba));
        assert_eq!(order_for_fourcc(FOURCC_ABGR8888), Some(Order::Rgba));
        assert_eq!(order_for_fourcc(0), None);
    }

    #[test]
    fn the_primary_of_the_kmd_matches_the_order_of_its_dxgi_format() {
        // `rm_sysmem::fourcc_for_dxgi` is what the primary is created with.
        for dxgi in [28u32, 87, 88] {
            let fourcc = crate::rm_sysmem::fourcc_for_dxgi(dxgi).unwrap();
            assert_eq!(order_for_fourcc(fourcc), order_for_dxgi(dxgi), "{dxgi}");
        }
    }

    #[test]
    fn a_swizzle_is_needed_only_when_the_orders_differ() {
        assert_eq!(swizzle(Order::Bgra, Order::Bgra), Swizzle::None);
        assert_eq!(swizzle(Order::Rgba, Order::Rgba), Swizzle::None);
        assert_eq!(swizzle(Order::Bgra, Order::Rgba), Swizzle::SwapRb);
        assert_eq!(swizzle(Order::Rgba, Order::Bgra), Swizzle::SwapRb);
    }

    #[test]
    fn swap_rb_exchanges_bytes_0_and_2_and_keeps_the_rest() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut dst = [0u8; 9];
        swap_rb_row(&mut dst, &src);
        assert_eq!(dst, [3, 2, 1, 4, 7, 6, 5, 8, 0]);
        // twice is the identity
        let mut back = [0u8; 8];
        swap_rb_row(&mut back, &dst[..8]);
        assert_eq!(&back, &src[..8]);
        // empty and short rows
        swap_rb_row(&mut [], &[]);
        let mut d3 = [9u8; 3];
        swap_rb_row(&mut d3, &[1, 2, 3]);
        assert_eq!(d3, [9, 9, 9]);
    }

    // ---- accounting ---------------------------------------------------------------------------

    #[test]
    fn accounting_units() {
        assert_eq!(micros(0), 0);
        assert_eq!(micros(9), 0);
        assert_eq!(micros(10), 1);
        assert_eq!(micros(10_000_000), 1_000_000);
        assert_eq!(sat32(5), 5);
        assert_eq!(sat32(u64::MAX), u32::MAX);
        let a = Plan::new();
        assert_eq!(a.bytes(), 0);
    }

    #[test]
    fn skip_codes_are_distinct_and_stable() {
        let all = [
            Skip::Layout,
            Skip::Snapshot,
            Skip::SourceFormat,
            Skip::SourceKind,
            Skip::SameResource,
            Skip::Stage,
            Skip::GpuCopy,
            Skip::GpuWait,
            Skip::Busy,
            Skip::MapSrc,
            Skip::MapDst,
            Skip::Plan,
            Skip::NoVenus,
        ];
        for (i, s) in all.iter().enumerate() {
            assert_eq!(s.code(), i as u32 + 1);
        }
    }

    // ---- a plan really moves the bytes it says (a model of the row loop) ---------------------

    /// Run the row loop of the I/O half over two vectors: the oracle for the plan's offsets.
    fn model(dst: &mut [u8], dsurf: &Surface, src: &[u8], ssurf: &Surface, p: &Plan, sw: Swizzle) {
        for r in p.rects() {
            for (y0, y1) in bands(r, dsurf, ssurf) {
                for y in y0..y1 {
                    let d = r.dst_row(dsurf, y) as usize;
                    let s = r.src_row(ssurf, y) as usize;
                    let n = r.row_bytes() as usize;
                    match sw {
                        Swizzle::None => dst[d..d + n].copy_from_slice(&src[s..s + n]),
                        Swizzle::SwapRb => swap_rb_row(&mut dst[d..d + n], &src[s..s + n]),
                    }
                }
            }
        }
    }

    #[test]
    fn the_row_loop_writes_only_the_rect() {
        let (w, h) = (64u32, 48u32);
        let dsurf = Surface::new(w, h, 256, 0, 256 * 48);
        let ssurf = Surface::new(w, h, 320, 64, 64 + 320 * 48);
        let mut dst = std::vec![0xEEu8; dsurf.size as usize];
        let mut src = std::vec![0u8; ssurf.size as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ssurf.at(x, y) as usize;
                src[o..o + 4].copy_from_slice(&[x as u8, y as u8, 0x11, 0xFF]);
            }
        }
        let d = Rect::new(10, 5, 30, 25);
        let p = run(&dsurf, &ssurf, d, d, &[]).unwrap();
        model(&mut dst, &dsurf, &src, &ssurf, &p, Swizzle::None);
        for y in 0..h {
            for x in 0..w {
                let o = dsurf.at(x, y) as usize;
                let inside = (10..30).contains(&x) && (5..25).contains(&y);
                if inside {
                    assert_eq!(&dst[o..o + 4], &[x as u8, y as u8, 0x11, 0xFF], "{x},{y}");
                } else {
                    assert_eq!(&dst[o..o + 4], &[0xEE; 4], "{x},{y}");
                }
            }
            // the padding after the row is never written
            let pad = dsurf.at(w, y) as usize;
            let end = (dsurf.at(0, y) + 256) as usize;
            assert!(dst[pad..end].iter().all(|&b| b == 0xEE));
        }
    }

    #[test]
    fn the_row_loop_with_a_shifted_source_and_a_swap() {
        let (w, h) = (32u32, 32u32);
        let dsurf = Surface::new(w, h, 128, 0, 128 * 32);
        let ssurf = Surface::new(w, h, 128, 0, 128 * 32);
        let mut dst = std::vec![0u8; dsurf.size as usize];
        let mut src = std::vec![0u8; ssurf.size as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ssurf.at(x, y) as usize;
                src[o..o + 4].copy_from_slice(&[1, x as u8, 3, y as u8]);
            }
        }
        let d = Rect::new(8, 8, 16, 16);
        let s = Rect::new(2, 3, 10, 11);
        let p = run(&dsurf, &ssurf, d, s, &[]).unwrap();
        model(&mut dst, &dsurf, &src, &ssurf, &p, Swizzle::SwapRb);
        // destination (8,8) reads source (2,3): bytes [3, x=2, 1, y=3]
        let o = dsurf.at(8, 8) as usize;
        assert_eq!(&dst[o..o + 4], &[3, 2, 1, 3]);
        let o = dsurf.at(15, 15) as usize;
        assert_eq!(&dst[o..o + 4], &[3, 9, 1, 10]);
        let o = dsurf.at(7, 8) as usize;
        assert_eq!(&dst[o..o + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_full_frame_through_the_bands_equals_a_flat_copy() {
        let (w, h) = (300u32, 700u32);
        let dsurf = Surface::new(w, h, 1280, 0, 1280 * 700);
        let ssurf = Surface::new(w, h, 1280, 0, 1280 * 700);
        let src: Vec<u8> = (0..ssurf.size as usize)
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let mut dst = std::vec![0u8; dsurf.size as usize];
        let p = run(&dsurf, &ssurf, Rect::default(), Rect::default(), &[]).unwrap();
        model(&mut dst, &dsurf, &src, &ssurf, &p, Swizzle::None);
        for y in 0..h {
            let o = dsurf.at(0, y) as usize;
            assert_eq!(&dst[o..o + 1200], &src[o..o + 1200], "row {y}");
        }
    }
}

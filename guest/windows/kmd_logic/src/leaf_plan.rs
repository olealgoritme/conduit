//! Native leaf updates that always apply VidMm's page table: the pure half of
//! `kmd_render/src/virtio/native_memory.rs::update_leaf`.
//!
//! The rule: after a leaf update `[va, va + len)` of a process shows exactly
//! what VidMm wrote there. The page table is the truth; the driver's own
//! record of what it mapped before (`HostMapping`s) is only a cache of RM
//! state and is repaired, never trusted to veto the update. The old path ran
//! every recorded piece of the allocation through [`nm::validate_piece`] /
//! [`nm::replace_range`] and refused the WHOLE update when one failed, which
//! left the range unmapped (or stale) while VidMm and the UMD believed it
//! mapped (`BuildPagingBuffer` must answer success). The shapes that did it:
//!
//! * a system-page piece (`update_leaf_system`) records VidMm's allocation
//!   offset for every page VidMm gave, padding included: past the end of the
//!   RM memory, so `validate_piece(old, size)` failed with
//!   `MemoryOutsideAllocation` on the next update of that range (stage 6,
//!   `NvLfErr` 0x602, `NvSysWhy` 1, `NvPgLeafWhy` 0x30d);
//! * the outside parts of a recorded system-page piece were remapped from the
//!   RM video memory, not from the pages they showed (and refused when past
//!   the memory's end: stage 13 and a poisoned allocation);
//! * old pieces that overlap each other (an earlier update that failed half
//!   way) failed `replace_range` with `Overlap`;
//! * a video-memory piece at a VA (or of a length) that is not 64-KiB aligned
//!   got a big-page virtual object, which RM aligns down: `NvWrongVa`.
//!
//! Here: what an update asks for ([`plan_leaf`]), whether it changes anything
//! ([`unchanged`]), the outside parts of a mapping it cuts ([`outside_parts`]),
//! bookkeeping that cannot all be true at once ([`overlapping_entries`]) and
//! the shapes a refused map is tried again in ([`needs_4k_virtual`],
//! [`split_at`]).

extern crate alloc;

use alloc::vec::Vec;

use crate::native_memory::{self as nm, AddressError, MappingPiece, GPU_ADDRESS_LIMIT, PAGE_BYTES};

/// RM's big page (64 KiB): the granularity of a default-page-size virtual object.
pub const BIG_PAGE: u64 = 64 * 1024;

/// What a leaf update asks for, sorted into what is done how.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LeafPlan {
    /// Pages of the allocation's RM memory to map at their VA (coalesced).
    pub map: Vec<MappingPiece>,
    /// Valid pages past the end of the RM memory (VidMm's padding), coalesced.
    /// `memory_offset` is VidMm's allocation offset (past the memory's end).
    pub pad: Vec<MappingPiece>,
    /// Valid PTEs that name nothing mappable (not the page VidMm addresses, an
    /// unaligned allocation offset): left unmapped, counted.
    pub rejected: u32,
    /// The first rejection's reason.
    pub why: Option<AddressError>,
}

impl LeafPlan {
    /// Whether every valid PTE of the update has a mapping planned.
    pub fn complete(&self) -> bool {
        self.rejected == 0
    }
}

/// Sort VidMm's PTEs (`pieces[i]` = page `i`, `None` = invalid PTE: unmapped)
/// into a [`LeafPlan`]. `Err` only for an update that names no valid range at
/// all (no pages, an unaligned or overflowing VA, outside the VA geometry).
pub fn plan_leaf(
    first_va: u64,
    allocation_offset: u64,
    size: u64,
    pieces: &[Option<MappingPiece>],
) -> Result<LeafPlan, AddressError> {
    if pieces.is_empty() {
        return Err(AddressError::ZeroLength);
    }
    if first_va & (PAGE_BYTES - 1) != 0 {
        return Err(AddressError::Unaligned);
    }
    let len = (pieces.len() as u64)
        .checked_mul(PAGE_BYTES)
        .ok_or(AddressError::VirtualOverflow)?;
    let end = first_va.checked_add(len).ok_or(AddressError::VirtualOverflow)?;
    if end > GPU_ADDRESS_LIMIT {
        return Err(AddressError::VirtualOutsideGeometry);
    }
    let mut plan = LeafPlan::default();
    let reject = |plan: &mut LeafPlan, why: AddressError| {
        plan.rejected += 1;
        plan.why.get_or_insert(why);
    };
    for (index, piece) in pieces.iter().enumerate() {
        let Some(piece) = *piece else { continue };
        let va = first_va + index as u64 * PAGE_BYTES;
        let Some(memory_offset) = allocation_offset.checked_add(index as u64 * PAGE_BYTES) else {
            reject(&mut plan, AddressError::MemoryOverflow);
            continue;
        };
        let expected = MappingPiece { va, memory_offset, len: PAGE_BYTES };
        if piece != expected {
            reject(&mut plan, AddressError::NotFound);
            continue;
        }
        if memory_offset & (PAGE_BYTES - 1) != 0 {
            reject(&mut plan, AddressError::Unaligned);
            continue;
        }
        if piece.memory_end().is_none_or(|e| e > size) {
            if plan.pad.try_reserve(1).is_err() {
                reject(&mut plan, AddressError::MemoryOverflow);
                continue;
            }
            plan.pad.push(piece);
            continue;
        }
        if let Err(why) = nm::validate_piece(piece, size) {
            reject(&mut plan, why);
            continue;
        }
        if plan.map.try_reserve(1).is_err() {
            reject(&mut plan, AddressError::MemoryOverflow);
            continue;
        }
        plan.map.push(piece);
    }
    nm::coalesce(&mut plan.map);
    nm::coalesce(&mut plan.pad);
    Ok(plan)
}

/// One recorded mapping of the allocation in the updated process, for
/// [`unchanged`]: its piece and whether it shows system pages (an OS
/// descriptor) rather than the RM memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub piece: MappingPiece,
    pub system: bool,
}

/// Whether the update changes nothing: every recorded mapping of the
/// allocation that overlaps `[va, va + len)` lies inside it, shows the RM
/// memory, and together they show exactly `plan.map` page for page (however
/// they are split), and the plan has no padding and nothing rejected. Then no
/// RM operation is needed (a re-residency, a page-table rebuild).
pub fn unchanged(recorded: &[Recorded], va: u64, len: u64, plan: &LeafPlan) -> bool {
    if !plan.pad.is_empty() || plan.rejected != 0 {
        return false;
    }
    let Some(end) = va.checked_add(len) else { return false };
    let overlapping = || {
        recorded
            .iter()
            .filter(move |r| nm::ranges_overlap(r.piece.va, r.piece.len, va, len))
    };
    if overlapping().any(|r| r.system || r.piece.va < va || r.piece.end().is_none_or(|e| e > end)) {
        return false;
    }
    let mapped_old: u64 = overlapping().map(|r| r.piece.len).sum();
    let mapped_new: u64 = plan.map.iter().map(|p| p.len).sum();
    if mapped_old != mapped_new {
        return false;
    }
    // Same bytes in total; each planned page must be shown at the same offset.
    plan.map.iter().all(|p| {
        (0..p.len / PAGE_BYTES).all(|page| {
            let at = p.va + page * PAGE_BYTES;
            let want = p.memory_offset + page * PAGE_BYTES;
            overlapping().any(|r| {
                at >= r.piece.va
                    && r.piece.end().is_some_and(|e| at < e)
                    && r.piece.memory_offset + (at - r.piece.va) == want
            })
        })
    })
}

/// The parts of `piece` outside `[va, va + len)`, each with its byte distance
/// from the piece's start (a system-page piece shows its pages from there).
/// RM unmaps whole objects, so these are mapped again after the piece goes.
/// No validation against the memory size: the recorded piece was mapped, and
/// its outside parts keep exactly what they showed.
pub fn outside_parts(piece: MappingPiece, va: u64, len: u64) -> [Option<(MappingPiece, u64)>; 2] {
    let mut out = [None, None];
    let (Some(piece_end), Some(end)) = (piece.end(), va.checked_add(len)) else {
        return out;
    };
    if len == 0 || !nm::ranges_overlap(piece.va, piece.len, va, len) {
        out[0] = Some((piece, 0));
        return out;
    }
    if piece.va < va {
        out[0] = Some((
            MappingPiece { va: piece.va, memory_offset: piece.memory_offset, len: va - piece.va },
            0,
        ));
    }
    if piece_end > end {
        let delta = end - piece.va;
        out[1] = Some((
            MappingPiece {
                va: end,
                memory_offset: piece.memory_offset.wrapping_add(delta),
                len: piece_end - end,
            },
            delta,
        ));
    }
    out
}

/// Indices of `pieces` (any order) that overlap an earlier-starting kept one:
/// bookkeeping that cannot all be true at once (RM cannot map two objects at
/// one VA). The first by VA (then by index) is kept; the rest are dropped from
/// the record without being shown again.
pub fn overlapping_entries(pieces: &[MappingPiece]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..pieces.len()).collect();
    order.sort_unstable_by_key(|&i| (pieces[i].va, i));
    let mut dropped = Vec::new();
    let mut reach = 0u64;
    let mut any = false;
    for i in order {
        let p = pieces[i];
        let end = p.end().unwrap_or(u64::MAX);
        if any && p.va < reach {
            dropped.push(i);
            continue;
        }
        any = true;
        reach = end;
    }
    dropped.sort_unstable();
    dropped
}

/// Whether a video-memory piece needs a 4-KiB-paged virtual object: a fixed
/// VA or a length that is not big-page aligned. A default virtual object takes
/// the space's big pages and RM aligns its VA down (`NvWrongVa` 0x232488 came
/// back 0x232480) or rounds its end up into the neighbour's object.
pub const fn needs_4k_virtual(piece: MappingPiece) -> bool {
    piece.va % BIG_PAGE != 0 || piece.len % BIG_PAGE != 0
}

/// `piece` cut at every `unit`-aligned VA (`unit` a power of two, at least a
/// page): the shapes a refused map is tried again in (64 KiB, then 4 KiB).
pub fn split_at(piece: MappingPiece, unit: u64) -> Vec<MappingPiece> {
    let unit = unit.max(PAGE_BYTES);
    let mut out = Vec::new();
    let Some(end) = piece.end() else { return out };
    let mut va = piece.va;
    while va < end {
        let next = ((va / unit) + 1).saturating_mul(unit).min(end);
        if out.try_reserve(1).is_err() {
            return Vec::new();
        }
        out.push(MappingPiece {
            va,
            memory_offset: piece.memory_offset + (va - piece.va),
            len: next - va,
        });
        va = next;
    }
    out
}

/// Pages of a retry at 4 KiB beyond which the update is left unmapped instead
/// (each page is one RM object and round trip).
pub const RETRY_PAGES_MAX: u64 = 1024;

/// `NvLfPadMap` (REG_DWORD, default 1): map VidMm's padding pages (valid PTEs
/// past the end of the RM memory) to one zero page instead of leaving them
/// unmapped. 0 leaves them unmapped (the old behaviour).
pub const fn pad_map_on(knob: u32) -> bool {
    knob != 0
}

/// What VidMm's page table says one page of a leaf update shows (`NvLfVerify`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// An invalid PTE: nothing of the allocation mapped there.
    Unmapped,
    /// The allocation's RM memory at this byte offset.
    Memory(u64),
    /// This system page (page frame number).
    Page(u64),
    /// Padding: mapped (to the zero page) or not.
    Any,
}

/// What a recorded mapping shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown<'a> {
    /// The allocation's RM memory from the piece's `memory_offset`.
    Memory,
    /// These system pages, one per 4 KiB of the piece.
    Pages(&'a [u64]),
    /// System pages the driver did not keep the list of.
    Unknown,
}

/// Pages of the update starting at `first_va` whose recorded mapping is not
/// what `expect` (one per page) asks for: a page shown twice, a missing or
/// extra mapping, another offset or another system page. 0 after every leaf
/// update when the driver's record (and so RM) matches VidMm's page table.
pub fn mismatches(recorded: &[(MappingPiece, Shown<'_>)], first_va: u64, expect: &[Expect]) -> u32 {
    let mut bad = 0u32;
    for (index, want) in expect.iter().enumerate() {
        let va = first_va + index as u64 * PAGE_BYTES;
        let mut covering = recorded
            .iter()
            .filter(|(p, _)| va >= p.va && p.end().is_some_and(|e| va < e));
        let found = covering.next();
        let twice = covering.next().is_some();
        let ok = !twice
            && match (*want, found) {
                (Expect::Any, _) => true,
                (Expect::Unmapped, found) => found.is_none(),
                (Expect::Memory(offset), Some((p, Shown::Memory))) => p.memory_offset + (va - p.va) == offset,
                (Expect::Page(pfn), Some((p, Shown::Pages(pages)))) => {
                    pages.get(((va - p.va) / PAGE_BYTES) as usize) == Some(&pfn)
                }
                _ => false,
            };
        if !ok {
            bad += 1;
        }
    }
    bad
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    const VA: u64 = 0x1bc4_0000;

    fn page(va: u64, off: u64) -> Option<MappingPiece> {
        Some(MappingPiece { va, memory_offset: off, len: PAGE_BYTES })
    }

    fn update(va: u64, off: u64, pages: usize) -> Vec<Option<MappingPiece>> {
        (0..pages)
            .map(|i| page(va + i as u64 * PAGE_BYTES, off + i as u64 * PAGE_BYTES))
            .collect()
    }

    fn rec(va: u64, off: u64, len: u64, system: bool) -> Recorded {
        Recorded { piece: MappingPiece { va, memory_offset: off, len }, system }
    }

    /// Stage 6 (`NvLfErr` 0x602, `NvSysWhy` 1, `NvPgLeafWhy` 0x30d): a system-page
    /// piece recorded VidMm's padding (offset past the 128-KiB RM memory); the next
    /// update of the range refused itself on it, the range stayed as it was.
    #[test]
    fn stage6_system_piece_past_the_memory_end_no_longer_blocks() {
        let size = 0x2_0000;
        // update_leaf_system mapped 0x30 pages: 0x20 real, 0x10 VidMm padding.
        let old = [MappingPiece { va: VA, memory_offset: 0, len: 0x3_0000 }];
        assert_eq!(
            nm::replace_range(&old, VA, 0x3_0000, None, size),
            Err(AddressError::MemoryOutsideAllocation),
            "the old path's failure"
        );
        // The new path: the plan does not look at old pieces at all, and the old
        // piece lies inside the range, so nothing outside is shown again.
        let plan = plan_leaf(VA, 0, size, &update(VA, 0, 0x30)).unwrap();
        assert_eq!(plan.map, vec![MappingPiece { va: VA, memory_offset: 0, len: 0x2_0000 }]);
        assert_eq!(plan.pad, vec![MappingPiece { va: VA + 0x2_0000, memory_offset: 0x2_0000, len: 0x1_0000 }]);
        assert!(plan.complete());
        assert_eq!(outside_parts(old[0], VA, 0x3_0000), [None, None]);
    }

    /// Stage 7: the same piece's outside part, cut by an update of its first
    /// pages, also failed validation; now it is shown again as what it was.
    #[test]
    fn stage7_outside_part_past_the_memory_end_is_kept() {
        let size = 0x2_0000;
        let old = MappingPiece { va: VA, memory_offset: 0, len: 0x3_0000 };
        let first = nm::replace_range(&[old], VA, 0x1000, None, size);
        assert_eq!(first, Err(AddressError::MemoryOutsideAllocation));
        let parts = outside_parts(old, VA, 0x1000);
        assert_eq!(parts[0], None);
        assert_eq!(
            parts[1],
            Some((MappingPiece { va: VA + 0x1000, memory_offset: 0x1000, len: 0x2_f000 }, 0x1000))
        );
    }

    /// Stage 5: a valid PTE whose page is not mappable (an unaligned allocation
    /// offset) refused the whole update; now only that page is left out.
    #[test]
    fn stage5_unmappable_page_rejects_only_itself() {
        let size = 0x10_0000;
        let mut pieces = update(VA, 0x800, 4);
        assert_eq!(nm::validate_piece(pieces[0].unwrap(), size), Err(AddressError::Unaligned));
        let plan = plan_leaf(VA, 0x800, size, &pieces).unwrap();
        assert_eq!((plan.rejected, plan.why), (4, Some(AddressError::Unaligned)));
        assert!(plan.map.is_empty());
        // A piece that is not the page VidMm addressed.
        pieces = update(VA, 0, 4);
        pieces[2] = page(VA, 0x5000);
        let plan = plan_leaf(VA, 0, size, &pieces).unwrap();
        assert_eq!((plan.rejected, plan.why), (1, Some(AddressError::NotFound)));
        assert_eq!(
            plan.map,
            vec![
                MappingPiece { va: VA, memory_offset: 0, len: 0x2000 },
                MappingPiece { va: VA + 0x3000, memory_offset: 0x3000, len: 0x1000 },
            ]
        );
    }

    /// Bookkeeping left overlapping by an update that failed half way failed
    /// `replace_range` with `Overlap`; now the later entry is dropped.
    #[test]
    fn overlapping_old_pieces_are_repaired() {
        let size = 0x10_0000;
        let a = MappingPiece { va: VA, memory_offset: 0, len: 0x4000 };
        let b = MappingPiece { va: VA + 0x2000, memory_offset: 0x2000, len: 0x4000 };
        let c = MappingPiece { va: VA + 0x8000, memory_offset: 0x8000, len: 0x1000 };
        assert_eq!(nm::replace_range(&[a, b], VA + 0x1_0000, 0x1000, None, size), Err(AddressError::Overlap));
        assert_eq!(overlapping_entries(&[b, c, a]), vec![0]);
        assert_eq!(overlapping_entries(&[a, b, c]), vec![1]);
        assert!(overlapping_entries(&[a, c]).is_empty());
        // Identical duplicates: the later one goes.
        assert_eq!(overlapping_entries(&[c, c]), vec![1]);
    }

    /// `NvWrongVa` 0x232488 / `NvWrongGot` 0x232480: a video-memory piece at a VA
    /// that is not 64-KiB aligned got a big-page virtual object.
    #[test]
    fn unaligned_video_memory_pieces_get_a_4k_virtual_object() {
        let p = |va: u64, len: u64| MappingPiece { va, memory_offset: 0, len };
        assert!(needs_4k_virtual(p(0x2_3248_8000, 0x8000)));
        assert!(needs_4k_virtual(p(0x2_3248_0000, 0x8000)));
        assert!(!needs_4k_virtual(p(0x2_3248_0000, 0x2_0000)));
    }

    #[test]
    fn splits_at_unit_boundaries_keep_offsets() {
        let piece = MappingPiece { va: 0x1_f000, memory_offset: 0x5000, len: 0x2_2000 };
        let parts = split_at(piece, BIG_PAGE);
        assert_eq!(
            parts,
            vec![
                MappingPiece { va: 0x1_f000, memory_offset: 0x5000, len: 0x1000 },
                MappingPiece { va: 0x2_0000, memory_offset: 0x6000, len: 0x1_0000 },
                MappingPiece { va: 0x3_0000, memory_offset: 0x1_6000, len: 0x1_0000 },
                MappingPiece { va: 0x4_0000, memory_offset: 0x2_6000, len: 0x1000 },
            ]
        );
        assert_eq!(parts.iter().map(|p| p.len).sum::<u64>(), piece.len);
        assert_eq!(split_at(piece, PAGE_BYTES).len(), 0x22);
    }

    #[test]
    fn unchanged_is_page_for_page_whatever_the_split() {
        let size = 0x10_0000;
        let plan = plan_leaf(VA, 0x1_0000, size, &update(VA, 0x1_0000, 0x20)).unwrap();
        // Recorded as one piece, or as the 64-KiB retry split: unchanged.
        assert!(unchanged(&[rec(VA, 0x1_0000, 0x2_0000, false)], VA, 0x2_0000, &plan));
        assert!(unchanged(
            &[rec(VA, 0x1_0000, 0x1_0000, false), rec(VA + 0x1_0000, 0x2_0000, 0x1_0000, false)],
            VA,
            0x2_0000,
            &plan
        ));
        // Pieces outside the range do not matter.
        assert!(unchanged(
            &[rec(VA, 0x1_0000, 0x2_0000, false), rec(VA + 0x4_0000, 0, 0x1000, true)],
            VA,
            0x2_0000,
            &plan
        ));
        // Other offsets, system pages, a piece reaching out, a hole: changed.
        assert!(!unchanged(&[rec(VA, 0, 0x2_0000, false)], VA, 0x2_0000, &plan));
        assert!(!unchanged(&[rec(VA, 0x1_0000, 0x2_0000, true)], VA, 0x2_0000, &plan));
        assert!(!unchanged(&[rec(VA, 0x1_0000, 0x3_0000, false)], VA, 0x2_0000, &plan));
        assert!(!unchanged(&[rec(VA, 0x1_0000, 0x1_0000, false)], VA, 0x2_0000, &plan));
        // An unmap: unchanged only when nothing is recorded there.
        let none = plan_leaf(VA, 0, size, &vec![None; 0x20]).unwrap();
        assert!(none.map.is_empty() && none.complete());
        assert!(unchanged(&[], VA, 0x2_0000, &none));
        assert!(!unchanged(&[rec(VA, 0, 0x1000, false)], VA, 0x2_0000, &none));
    }

    #[test]
    fn padding_is_never_unchanged_and_invalid_ptes_unmap() {
        let size = 0x1000;
        let mut pieces = update(VA, 0, 3);
        pieces[1] = None;
        let plan = plan_leaf(VA, 0, size, &pieces).unwrap();
        assert_eq!(plan.map, vec![MappingPiece { va: VA, memory_offset: 0, len: 0x1000 }]);
        assert_eq!(plan.pad, vec![MappingPiece { va: VA + 0x2000, memory_offset: 0x2000, len: 0x1000 }]);
        assert!(!unchanged(&[rec(VA, 0, 0x1000, false)], VA, 0x3000, &plan));
    }

    #[test]
    fn whole_update_errors_are_only_range_errors() {
        assert_eq!(plan_leaf(VA, 0, 0x1000, &[]), Err(AddressError::ZeroLength));
        assert_eq!(plan_leaf(VA + 1, 0, 0x1000, &update(VA, 0, 1)), Err(AddressError::Unaligned));
        assert_eq!(
            plan_leaf(GPU_ADDRESS_LIMIT - 0x1000, 0, 0x1_0000, &update(GPU_ADDRESS_LIMIT - 0x1000, 0, 2)),
            Err(AddressError::VirtualOutsideGeometry)
        );
    }

    #[test]
    fn outside_parts_of_an_untouched_piece_is_the_piece() {
        let p = MappingPiece { va: VA, memory_offset: 0x3000, len: 0x2000 };
        assert_eq!(outside_parts(p, VA + 0x2000, 0x1000), [Some((p, 0)), None]);
        let both = outside_parts(p, VA + 0x1000, 0x0);
        // A zero-length range overlaps nothing.
        assert_eq!(both, [Some((p, 0)), None]);
        let mid = MappingPiece { va: VA, memory_offset: 0x3000, len: 0x4000 };
        assert_eq!(
            outside_parts(mid, VA + 0x1000, 0x2000),
            [
                Some((MappingPiece { va: VA, memory_offset: 0x3000, len: 0x1000 }, 0)),
                Some((MappingPiece { va: VA + 0x3000, memory_offset: 0x6000, len: 0x1000 }, 0x3000)),
            ]
        );
    }

    #[test]
    fn pad_knob_defaults_on() {
        assert!(pad_map_on(1));
        assert!(!pad_map_on(0));
    }

    /// The stuck ring of 405.291: VidMm moved a system-placed allocation to new
    /// pages; the GPU mapping kept the old ones. The verify check sees it.
    #[test]
    fn verify_counts_pages_shown_from_the_wrong_place() {
        let old = [10u64, 11, 12, 13];
        let new = [20u64, 21, 22, 23];
        let piece = MappingPiece { va: VA, memory_offset: 0, len: 0x4000 };
        let want: Vec<Expect> = new.iter().map(|p| Expect::Page(*p)).collect();
        assert_eq!(mismatches(&[(piece, Shown::Pages(&old))], VA, &want), 4);
        assert_eq!(mismatches(&[(piece, Shown::Pages(&new))], VA, &want), 0);
        assert_eq!(mismatches(&[(piece, Shown::Unknown)], VA, &want), 4);
        assert_eq!(mismatches(&[], VA, &want), 4);
        // Video memory, an unmap, padding and a page shown twice.
        let want = [Expect::Memory(0x1000), Expect::Unmapped, Expect::Any];
        let a = MappingPiece { va: VA, memory_offset: 0x1000, len: 0x1000 };
        assert_eq!(mismatches(&[(a, Shown::Memory)], VA, &want), 0);
        let b = MappingPiece { va: VA, memory_offset: 0x1000, len: 0x2000 };
        assert_eq!(mismatches(&[(b, Shown::Memory)], VA, &want), 1);
        assert_eq!(mismatches(&[(a, Shown::Memory), (a, Shown::Memory)], VA, &want), 1);
        let c = MappingPiece { va: VA, memory_offset: 0, len: 0x1000 };
        assert_eq!(mismatches(&[(c, Shown::Memory)], VA, &want), 1);
    }
}

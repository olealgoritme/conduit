//! Guest-memory blob as the Blt copy destination (`GuestBlob`, "Candidate B" of
//! `docs/rm-backed-standard.md` 13.3): the pure half. The I/O half is
//! `kmd_render/src/ddi/guest_blob.rs` (knob, counters, creation, teardown) and the Venus objects
//! in `kmd_render/src/virtio/venus/guest_blob.rs`. Design, lifecycle and the failure matrix:
//! `docs/zero-copy-present.md` section 24.12.
//!
//! While VidMm holds a KMD standard Present buffer in system memory (the paging path keeps
//! `MmProbeAndLockPages` leases on those pages in `adapter.system_backings`), the KMD describes
//! exactly those pages to the host as one virtio-gpu GUEST blob, imports it into its own Venus
//! device as a `VkBuffer`, and points the Present copy at it. The GPU then writes the pages
//! dxgkrnl's CPU view reads, and the CPU mirror (`mirror_present_system_backing`) has nothing
//! left to do. Everything the host decides lives in [`contract`], and only there.
//!
//! What is here:
//!
//! * [`build_runs`]: does the lease snapshot cover `[0, cover)`, and the page-run list the create
//!   carries (consecutive PFNs coalesced, the per-entry and per-create limits respected).
//! * [`Record`]: the per-destination state machine `None -> Creating -> Ready -> Draining ->
//!   Gone`, the strike rule, and the one rule that matters most: pages may be unlocked only
//!   when no host object can still write them ([`Record::may_unlock`]).
//! * [`Budget`]: the host's adapter-wide limits on live guest blobs and their runs, tracked so
//!   the KMD refuses before sending.
//! * [`eligible`], [`present_effect`], [`foreign_consumer`]: the per-Present decisions.
//! * [`COUNTERS`]: the counter names, checked against the I/O file by the tests below.

/// Everything the host defines. Reconciled with the host prototype (branch
/// `feat/host-guest-blob-copy-dst`, `docs/VENUS.md` "Guest-memory blobs"); the config feature
/// bit itself is `helios_protocol::NVGPU_CFG_GUEST_BLOB` (`protocol/src/features.rs`), the one
/// host constant this crate cannot hold because it has no dependency on the protocol crate.
pub mod contract {
    /// `VIRTIO_GPU_BLOB_MEM_GUEST`: the blob is the guest pages the entries name.
    pub const BLOB_MEM: u32 = 1;
    /// `VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE`: accepted by the host, with no effect.
    pub const BLOB_FLAGS: u32 = 2;
    /// `USE_MAPPABLE | USE_CROSS_DEVICE`: the host REFUSES a guest blob carrying either.
    pub const REFUSED_BLOB_FLAGS: u32 = 1 | 4;
    /// `blob_id` of a guest blob.
    pub const BLOB_ID: u64 = 0;
    /// `NVGPU_CFG_VENUS` (config `features` bit 10): the guest-blob bit means something only
    /// together with it.
    pub const CFG_VENUS: u32 = 1 << 10;
    /// Bytes of `virtio_gpu_resource_create_blob` (the entries follow it).
    pub const CREATE_BYTES: usize = 56;
    /// Bytes of one `virtio_gpu_mem_entry { u64 addr; u32 length; u32 padding = 0 }`.
    pub const ENTRY_BYTES: usize = 16;
    /// Every entry address is page-aligned and every length a nonzero multiple of this.
    pub const PAGE: u64 = 4096;
    /// Entries one create may carry.
    pub const MAX_ENTRIES: usize = 4096;
    /// Largest guest blob (the sum of its entry lengths).
    pub const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;
    /// Longest entry: the `u32` length field, cut to whole pages. Never reached in practice
    /// (a blob is at most 256 MiB), kept so the splitting rule is explicit.
    pub const MAX_RUN_BYTES: u64 = 0xFFFF_F000;
    /// Runs of all live guest blobs of one device together; beyond, `OUT_OF_MEMORY`.
    pub const MAX_LIVE_RUNS: u32 = 32768;
    /// Live guest blobs of one device; beyond, `OUT_OF_MEMORY`.
    pub const MAX_LIVE_BLOBS: u32 = 1024;

    /// virtio-gpu response types the create can answer with.
    pub const RESP_OK_NODATA: u32 = 0x1100;
    pub const RESP_ERR_UNSPEC: u32 = 0x1200;
    pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
    pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;
    /// The errnos the host puts in the response header (`foreign_errno::from_resp_hdr`).
    pub const EIO: u32 = 5;
    pub const ENOMEM: u32 = 12;
    pub const EFAULT: u32 = 14;
    pub const EINVAL: u32 = 22;
    pub const EOPNOTSUPP: u32 = 95;
    /// Entries from more than one guest RAM backing file (never with QEMU's single `pc.ram`).
    pub const EXDEV: u32 = 18;

    /// `VK_COMMAND_TYPE_vkGetMemoryResourcePropertiesMESA_EXT`.
    pub const CMD_GET_MEMORY_RESOURCE_PROPERTIES_MESA: u32 = 192;
    /// `VK_STRUCTURE_TYPE_MEMORY_RESOURCE_PROPERTIES_MESA`.
    pub const ST_MEMORY_RESOURCE_PROPERTIES_MESA: i32 = 1000384001;
    /// `VK_COMMAND_TYPE_vkCreateBuffer_EXT`.
    pub const CMD_CREATE_BUFFER: u32 = 50;
    /// `VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO`.
    pub const ST_BUFFER_CREATE_INFO: i32 = 12;
    /// `VK_BUFFER_USAGE_TRANSFER_DST_BIT`: the buffer is only ever a copy destination.
    pub const BUFFER_USAGE_TRANSFER_DST: u32 = 0x2;
    /// `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
    pub const VK_ERROR_OUT_OF_DEVICE_MEMORY: i32 = -2;
    /// `VK_ERROR_INVALID_EXTERNAL_HANDLE`.
    pub const VK_ERROR_INVALID_EXTERNAL_HANDLE: i32 = -1000072003;

    /// Whether the device serves guest blobs: `guest_blob_bit` (`NVGPU_CFG_GUEST_BLOB`) and
    /// `NVGPU_CFG_VENUS` are both set in the config `features` word.
    pub const fn advertised(cfg_features: u32, guest_blob_bit: u32) -> bool {
        guest_blob_bit != 0
            && cfg_features & guest_blob_bit == guest_blob_bit
            && cfg_features & CFG_VENUS != 0
    }

    /// One `virtio_gpu_mem_entry`, little-endian.
    pub const fn encode_entry(addr: u64, len: u32) -> [u8; ENTRY_BYTES] {
        let a = addr.to_le_bytes();
        let l = len.to_le_bytes();
        [
            a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], l[0], l[1], l[2], l[3], 0, 0, 0, 0,
        ]
    }

    /// The memory type to import a guest blob with: the first type allowed by the
    /// `memoryTypeBits` `vkGetMemoryResourcePropertiesMESA` reported that is HOST_VISIBLE and
    /// HOST_COHERENT (the host's host-pointer types; the copy relies on snooped coherence).
    /// `None`: no such type, the import is refused.
    pub fn choose_memory_type(
        memory_type_flags: &[u32],
        memory_type_count: u32,
        memory_type_bits: u32,
    ) -> Option<u32> {
        let want = crate::MEMORY_PROPERTY_HOST_VISIBLE | crate::MEMORY_PROPERTY_HOST_COHERENT;
        let mut i = 0u32;
        while i < memory_type_count
            && i < crate::VK_MAX_MEMORY_TYPES
            && (i as usize) < memory_type_flags.len()
        {
            if memory_type_bits & (1u32 << i) != 0 && memory_type_flags[i as usize] & want == want {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// `vkGetMemoryResourcePropertiesMESA(device, resource_id, &props)` with an empty pNext
    /// chain; the reply is `cmd | VkResult | ptr | sType | pNext | memoryTypeBits`.
    pub fn encode_get_resource_properties(device: u64, resource_id: u32) -> crate::Writer {
        let mut w = crate::Writer::new();
        w.header(
            CMD_GET_MEMORY_RESOURCE_PROPERTIES_MESA,
            crate::CMD_FLAG_GENERATE_REPLY,
        );
        w.u64(device);
        w.u32(resource_id);
        w.count(true); // pMemoryResourceProperties
        w.i32(ST_MEMORY_RESOURCE_PROPERTIES_MESA);
        w.count(false); // pNext
        w
    }

    /// A plain `vkCreateBuffer` (no external-memory struct, exclusive, TRANSFER_DST) of `size`
    /// bytes: what the host imports a guest blob into.
    pub fn encode_create_buffer(device: u64, buffer: u64, size: u64) -> crate::Writer {
        let mut w = crate::Writer::new();
        w.header(CMD_CREATE_BUFFER, crate::CMD_FLAG_GENERATE_REPLY);
        w.u64(device);
        w.count(true);
        w.i32(ST_BUFFER_CREATE_INFO);
        w.count(false); // pNext
        w.u32(0); // flags
        w.u64(size);
        w.u32(BUFFER_USAGE_TRANSFER_DST);
        w.u32(crate::SHARING_MODE_EXCLUSIVE);
        w.u32(0); // queueFamilyIndexCount
        w.count(false); // pQueueFamilyIndices
        w.count(false); // pAllocator
        w.count(true);
        w.u64(buffer);
        w
    }

    /// The class of a refused create (`resp_type`, the response header's errno).
    pub const fn classify_create(resp_type: u32, errno: u32) -> super::Why {
        use super::Why;
        match (resp_type, errno) {
            (RESP_ERR_UNSPEC, EOPNOTSUPP) => Why::HostUnsupported,
            (RESP_ERR_UNSPEC, EIO) => Why::HostRenderer,
            (RESP_ERR_INVALID_CONTEXT_ID, _) => Why::HostContext,
            (RESP_ERR_INVALID_RESOURCE_ID, _) => Why::HostResourceId,
            (RESP_ERR_INVALID_PARAMETER, EFAULT) => Why::HostFault,
            (RESP_ERR_INVALID_PARAMETER, EXDEV) => Why::HostCrossFile,
            (RESP_ERR_INVALID_PARAMETER, _) => Why::HostShape,
            (RESP_ERR_OUT_OF_MEMORY, _) => Why::HostNoMemory,
            _ => Why::HostOther,
        }
    }

    /// The class of a refused import (`vkAllocateMemory` with `VkImportMemoryResourceInfoMESA`,
    /// or `vkGetMemoryResourcePropertiesMESA`).
    pub const fn classify_import(vk_result: i32) -> super::Why {
        match vk_result {
            VK_ERROR_INVALID_EXTERNAL_HANDLE => super::Why::ImportHandle,
            VK_ERROR_OUT_OF_DEVICE_MEMORY => super::Why::ImportNoMemory,
            _ => super::Why::ImportOther,
        }
    }
}

use contract::{MAX_BLOB_BYTES, MAX_ENTRIES, MAX_LIVE_BLOBS, MAX_LIVE_RUNS, MAX_RUN_BYTES, PAGE};

/// Why a destination does not (or no longer) use a guest blob. `code` is what `GbWhy` holds,
/// `bit` what `GbMask` collects. Codes 1 to 11 are decisions (no strike); 12 to 28 are
/// failures, each a strike; later codes are each one or the other ([`Why::strikes`]). New
/// codes are appended, never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// `GuestBlob` is 0.
    KnobOff,
    /// The host does not advertise `NVGPU_CFG_GUEST_BLOB` with `NVGPU_CFG_VENUS`.
    NotAdvertised,
    /// The destination is not a KMD standard buffer.
    NotBuffer,
    /// The leases do not cover `[0, cover)` (the allocation is in the BAR segment, or VidMm
    /// evicted only part of it, or a lease was refused).
    Uncovered,
    /// A covered page is not one whole, page-aligned physical page of one lease.
    Unaligned,
    /// More runs than one create may carry.
    TooManyRuns,
    /// The blob would be larger than the host allows (or its size is 0).
    TooBig,
    /// The host's live-run or live-blob total would be exceeded.
    Budget,
    /// A Venus process other than the presenter has the destination open: it samples the
    /// Venus blob, which a guest-blob copy would leave stale.
    ForeignConsumer,
    /// Three strikes: disabled for this destination until it is destroyed.
    Disabled,
    /// Being created or drained by another thread.
    Busy,
    /// `RESP_ERR_UNSPEC` + `EOPNOTSUPP`: the host's guest-blob support is off.
    HostUnsupported,
    /// `RESP_ERR_INVALID_CONTEXT_ID`.
    HostContext,
    /// `RESP_ERR_INVALID_RESOURCE_ID`.
    HostResourceId,
    /// `RESP_ERR_INVALID_PARAMETER` + `EINVAL`: the shape (entries, size, flags).
    HostShape,
    /// `RESP_ERR_INVALID_PARAMETER` + `EFAULT`: an entry outside guest RAM.
    HostFault,
    /// `RESP_ERR_OUT_OF_MEMORY` (+ `ENOMEM`): the host's limits.
    HostNoMemory,
    /// `RESP_ERR_UNSPEC` + `EIO`: the renderer refused.
    HostRenderer,
    /// Any other refusal of the create.
    HostOther,
    /// The control queue timed out or the transport failed.
    Transport,
    /// The import answered `VK_ERROR_INVALID_EXTERNAL_HANDLE`.
    ImportHandle,
    /// The import answered `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
    ImportNoMemory,
    /// Any other Venus failure of the import (properties, buffer create, bind, record).
    ImportOther,
    /// No HOST_VISIBLE|HOST_COHERENT type among the resource's memory types.
    NoMemoryType,
    /// A KMD table was full or an allocation failed.
    Kmd,
    /// The drain of the copies into the guest buffer did not finish in time: the pages stay
    /// pinned for the life of the destination (`GbLeak`).
    DrainTimeout,
    /// A step of the release (destroy, free, fence, unref) failed: the pages stay pinned.
    ReleaseFailed,
    /// `RESP_ERR_INVALID_PARAMETER` + `EXDEV`: the entries span more than one guest RAM
    /// backing file.
    HostCrossFile,
    /// The destination's system copy is marked invalid (a skipped eviction, `BltNoMirror`): a
    /// page-in will be skipped and the Venus blob kept, so the pages must not become the copy
    /// target (and a live guest blob is retired). A decision, no strike.
    SystemStale,
    /// The create was sent and not answered within [`deadline::CREATE_MS`]: the host may still
    /// create the blob over the pages, so they stay pinned (poisons).
    CreateTimeout,
}

impl Why {
    pub const ALL: [Why; 30] = [
        Why::KnobOff,
        Why::NotAdvertised,
        Why::NotBuffer,
        Why::Uncovered,
        Why::Unaligned,
        Why::TooManyRuns,
        Why::TooBig,
        Why::Budget,
        Why::ForeignConsumer,
        Why::Disabled,
        Why::Busy,
        Why::HostUnsupported,
        Why::HostContext,
        Why::HostResourceId,
        Why::HostShape,
        Why::HostFault,
        Why::HostNoMemory,
        Why::HostRenderer,
        Why::HostOther,
        Why::Transport,
        Why::ImportHandle,
        Why::ImportNoMemory,
        Why::ImportOther,
        Why::NoMemoryType,
        Why::Kmd,
        Why::DrainTimeout,
        Why::ReleaseFailed,
        Why::HostCrossFile,
        Why::SystemStale,
        Why::CreateTimeout,
    ];

    /// 1-based, stable: the value of `GbWhy`.
    pub const fn code(self) -> u32 {
        match self {
            Why::KnobOff => 1,
            Why::NotAdvertised => 2,
            Why::NotBuffer => 3,
            Why::Uncovered => 4,
            Why::Unaligned => 5,
            Why::TooManyRuns => 6,
            Why::TooBig => 7,
            Why::Budget => 8,
            Why::ForeignConsumer => 9,
            Why::Disabled => 10,
            Why::Busy => 11,
            Why::HostUnsupported => 12,
            Why::HostContext => 13,
            Why::HostResourceId => 14,
            Why::HostShape => 15,
            Why::HostFault => 16,
            Why::HostNoMemory => 17,
            Why::HostRenderer => 18,
            Why::HostOther => 19,
            Why::Transport => 20,
            Why::ImportHandle => 21,
            Why::ImportNoMemory => 22,
            Why::ImportOther => 23,
            Why::NoMemoryType => 24,
            Why::Kmd => 25,
            Why::DrainTimeout => 26,
            Why::ReleaseFailed => 27,
            Why::HostCrossFile => 28,
            Why::SystemStale => 29,
            Why::CreateTimeout => 30,
        }
    }

    /// The `GbMask` bit: `1 << (code - 1)`.
    pub const fn bit(self) -> u32 {
        1u32 << (self.code() - 1)
    }

    /// Whether this is a failure (a strike) rather than a decision. Decisions are codes 1 to 11
    /// and the ones appended later ([`Why::SystemStale`]).
    pub const fn strikes(self) -> bool {
        !matches!(self.code(), 1..=11 | 29)
    }

    /// Whether the failure leaves host objects that may still write the pages, so the
    /// destination must keep them pinned and never retry.
    pub const fn poisons(self) -> bool {
        matches!(
            self,
            Why::DrainTimeout | Why::ReleaseFailed | Why::CreateTimeout
        )
    }
}

/// The KMD's own bounds on every wait of a create and of a retire (policy, not host contract).
///
/// A create runs on a Present thread under the content transaction, a retire on the paging
/// thread (`BuildPagingBuffer`), a Present thread or StopDevice, under the content transaction
/// and the Venus mutex: a sick host must cost them at most a few seconds, never the 30 s of a
/// default control round trip or ring wait per step. The I/O half runs each phase inside a
/// bounded section (`ddi::escape_wait::begin_bounded`) whose deadline every wait primitive it
/// reaches already obeys (the control round trip, the Venus ring wait, the mutex acquires, the
/// enqueue retry budget: `wait_bound`), and passes the per-step limits below where a call takes
/// a timeout of its own. A phase that runs out is a strike that POISONS the destination (the
/// host may still act on what was sent: the pages stay pinned until the generation ends);
/// `BuildPagingBuffer` answers success either way.
pub mod deadline {
    /// One wire fence of a copy into a guest buffer, or one queue marker (its
    /// `vkWaitForFences` carries this as its host-side timeout, so a marker never blocks the
    /// host's ring for longer either).
    pub const FENCE_MS: u32 = 250;
    /// The Venus half of a retire, all of it: the Venus mutex, the drain (every fence and the
    /// marker), the release of the cached commands, the destroy, the free and the fence after it.
    pub const DRAIN_MS: u32 = 1_000;
    /// The `RESOURCE_UNREF` of a guest blob.
    pub const UNREF_MS: u32 = 1_000;
    /// The `RESOURCE_CREATE_BLOB` round trip.
    pub const CREATE_MS: u32 = 1_000;
    /// The import: the Venus mutex and every ring command of it, its unwind included.
    pub const IMPORT_MS: u32 = 1_000;
    /// The longest a create can block its thread: the create, the import, the UNREF of a
    /// refused import.
    pub const CREATE_TOTAL_MS: u32 = CREATE_MS + IMPORT_MS + UNREF_MS;

    /// The limits of one retire.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Limits {
        pub drain_ms: u32,
        pub unref_ms: u32,
    }

    impl Limits {
        /// A retire from the paging path, a Present or a destroy.
        pub const NORMAL: Limits = Limits {
            drain_ms: DRAIN_MS,
            unref_ms: UNREF_MS,
        };

        /// A retire under a caller's per-call allowance (StopDevice's sweep budget,
        /// `SweepBudget::call_timeout_ms`): each limit cut to `cap_ms`, never 0 (a 0 deadline
        /// would mean "none").
        pub const fn capped(cap_ms: u64) -> Limits {
            Limits {
                drain_ms: cut(DRAIN_MS, cap_ms),
                unref_ms: cut(UNREF_MS, cap_ms),
            }
        }

        /// The longest the retire can block its thread.
        pub const fn total_ms(&self) -> u32 {
            self.drain_ms.saturating_add(self.unref_ms)
        }
    }

    const fn cut(limit_ms: u32, cap_ms: u64) -> u32 {
        let v = if (limit_ms as u64) < cap_ms {
            limit_ms
        } else {
            cap_ms as u32
        };
        if v == 0 {
            1
        } else {
            v
        }
    }

    /// The timeout of one fence wait inside a section with `left_ms` remaining (`None`: not in
    /// a bounded section): [`FENCE_MS`], cut to what is left, at least 1 ms.
    pub const fn fence_wait_ms(left_ms: Option<u32>) -> u32 {
        match left_ms {
            Some(left) if left < FENCE_MS => {
                if left == 0 {
                    1
                } else {
                    left
                }
            }
            _ => FENCE_MS,
        }
    }
}

/// One page run of the create: guest-physical `addr`, `len` bytes, both page multiples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub addr: u64,
    pub len: u64,
}

/// One lease of the destination's system backing, as the KMD reads it from the lease's MDL:
/// allocation bytes `[blob_offset, blob_offset + size)`, whose first byte is `byte_offset`
/// bytes into the physical page `pfns[0]`.
#[derive(Clone, Copy, Debug)]
pub struct Piece<'a> {
    pub blob_offset: u64,
    pub size: u64,
    pub byte_offset: u64,
    pub pfns: &'a [u64],
}

/// The bytes a guest blob for a `pitch` x `height` destination covers: `pitch * height` rounded
/// up to whole pages (the host wants a nonzero page multiple). Also at most the allocation
/// (`allocation_size`), so the cover never names a byte the allocation does not own.
pub fn cover_len(pitch: u32, height: u32, allocation_size: u64) -> Result<u64, Why> {
    let bytes = u64::from(pitch)
        .checked_mul(u64::from(height))
        .ok_or(Why::TooBig)?;
    if bytes == 0 {
        return Err(Why::TooBig);
    }
    let cover = bytes.checked_add(PAGE - 1).ok_or(Why::TooBig)? / PAGE * PAGE;
    if cover > MAX_BLOB_BYTES {
        return Err(Why::TooBig);
    }
    if cover > allocation_size {
        // A destination whose last page is shared with nothing still has a whole page in its
        // allocation (allocations are page-rounded); one that has not is not ours to describe.
        return Err(Why::Uncovered);
    }
    Ok(cover)
}

/// Walk the pages of `[0, cover)` through `pieces` (sorted by `blob_offset`, non-overlapping,
/// as the backing table stores them) and hand each run of physically consecutive pages to
/// `sink`, cut at [`MAX_RUN_BYTES`]. Returns the number of runs.
///
/// Refuses, before `sink` has seen anything wrong:
/// * [`Why::Uncovered`] when a page of the cover lies in no piece;
/// * [`Why::Unaligned`] when a page is not one whole, page-aligned physical page of one
///   piece (the host takes page-aligned entries only), or a piece is malformed;
/// * [`Why::TooManyRuns`] past [`MAX_ENTRIES`];
/// * [`Why::TooBig`] for a cover of 0 or past [`MAX_BLOB_BYTES`].
///
/// `sink` may have been called for a prefix when an error is returned; call it once with a
/// no-op sink to count and validate, then again to fill.
pub fn build_runs(
    pieces: &[Piece<'_>],
    cover: u64,
    mut sink: impl FnMut(Run),
) -> Result<usize, Why> {
    if cover == 0 || cover % PAGE != 0 || cover > MAX_BLOB_BYTES {
        return Err(Why::TooBig);
    }
    // Sorted and disjoint: the table's invariant, checked here rather than trusted.
    let mut prev_end = 0u64;
    for (i, p) in pieces.iter().enumerate() {
        let end = p.blob_offset.checked_add(p.size).ok_or(Why::Unaligned)?;
        if p.size == 0 || (i > 0 && p.blob_offset < prev_end) {
            return Err(Why::Unaligned);
        }
        prev_end = end;
    }
    let mut runs = 0usize;
    let mut current: Option<Run> = None;
    let mut piece = 0usize;
    let mut offset = 0u64;
    while offset < cover {
        while piece < pieces.len() && pieces[piece].blob_offset + pieces[piece].size <= offset {
            piece += 1;
        }
        let Some(p) = pieces.get(piece) else {
            return Err(Why::Uncovered);
        };
        if p.blob_offset > offset {
            return Err(Why::Uncovered);
        }
        // The whole page must lie in this piece.
        if offset + PAGE > p.blob_offset + p.size {
            // The next piece holds the rest of the page, or nothing does.
            let next_covers = pieces
                .get(piece + 1)
                .is_some_and(|n| n.blob_offset == p.blob_offset + p.size);
            return Err(if next_covers {
                Why::Unaligned
            } else {
                Why::Uncovered
            });
        }
        let in_piece = p
            .byte_offset
            .checked_add(offset - p.blob_offset)
            .ok_or(Why::Unaligned)?;
        if in_piece % PAGE != 0 {
            return Err(Why::Unaligned);
        }
        let index = usize::try_from(in_piece / PAGE).map_err(|_| Why::Unaligned)?;
        let pfn = *p.pfns.get(index).ok_or(Why::Unaligned)?;
        let addr = pfn.checked_mul(PAGE).ok_or(Why::Unaligned)?;
        current = match current {
            Some(mut run) if run.addr + run.len == addr && run.len + PAGE <= MAX_RUN_BYTES => {
                run.len += PAGE;
                Some(run)
            }
            Some(run) => {
                runs += 1;
                if runs > MAX_ENTRIES {
                    return Err(Why::TooManyRuns);
                }
                sink(run);
                Some(Run { addr, len: PAGE })
            }
            None => Some(Run { addr, len: PAGE }),
        };
        offset += PAGE;
    }
    if let Some(run) = current {
        runs += 1;
        if runs > MAX_ENTRIES {
            return Err(Why::TooManyRuns);
        }
        sink(run);
    }
    Ok(runs)
}

/// The host's adapter-wide totals over every live guest blob: refuse before sending what the
/// host would answer with `OUT_OF_MEMORY`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    runs: u32,
    blobs: u32,
}

impl Budget {
    pub const fn new() -> Self {
        Self { runs: 0, blobs: 0 }
    }

    /// Whether one more blob of `runs` runs fits.
    pub fn admits(&self, runs: usize) -> Result<(), Why> {
        let runs = u32::try_from(runs).map_err(|_| Why::Budget)?;
        if self.blobs >= MAX_LIVE_BLOBS || self.runs.saturating_add(runs) > MAX_LIVE_RUNS {
            return Err(Why::Budget);
        }
        Ok(())
    }

    /// A blob of `runs` runs was SENT (charged before the create, so two creators cannot both
    /// fit into the last slot). Fails exactly when [`Self::admits`] does.
    pub fn charge(&mut self, runs: usize) -> Result<(), Why> {
        self.admits(runs)?;
        self.runs += runs as u32;
        self.blobs += 1;
        Ok(())
    }

    /// A blob of `runs` runs is gone on the host (unref'd, or its create was refused).
    pub fn refund(&mut self, runs: u32) {
        self.runs = self.runs.saturating_sub(runs);
        self.blobs = self.blobs.saturating_sub(1);
    }

    /// The transport generation ended: the host forgot every blob.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub const fn live_runs(&self) -> u32 {
        self.runs
    }

    pub const fn live_blobs(&self) -> u32 {
        self.blobs
    }
}

/// Failures that disable the guest blob for one destination.
pub const MAX_STRIKES: u8 = 3;

/// Where one destination's guest blob is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Never made (or its create failed and it may be tried again).
    None,
    /// A thread is creating and importing it (the content transaction is held).
    Creating,
    /// Live: Present copies go to it.
    Ready,
    /// Retired: no new copy goes to it; the copies in flight are drained and the host objects
    /// released. Stays here when a step of that failed (the pages then stay pinned).
    Draining,
    /// Released: no host object names the pages any more.
    Gone,
}

/// What [`Record::begin_drain`] asks the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    /// Nothing exists on the host: the pages may be unlocked now.
    Nothing,
    /// Drain and release guest resource `guest`, then call [`Record::drained`] (or
    /// [`Record::drain_failed`]).
    Release { guest: u32 },
    /// An earlier release failed: the host may still write the pages. Leave them pinned.
    Poisoned,
}

/// One destination's guest-blob record. Keyed by the destination's Venus resource id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub resource_id: u32,
    pub state: State,
    pub strikes: u8,
    /// The guest blob's resource id while `Creating` (once known), `Ready` or `Draining`.
    pub guest: u32,
    /// Its run count (what [`Budget::refund`] gives back).
    pub runs: u32,
    /// Set by a failure that may have left the host writing the pages.
    pub poisoned: bool,
}

impl Record {
    pub const fn new(resource_id: u32) -> Self {
        Self {
            resource_id,
            state: State::None,
            strikes: 0,
            guest: 0,
            runs: 0,
            poisoned: false,
        }
    }

    pub const fn disabled(&self) -> bool {
        self.strikes >= MAX_STRIKES || self.poisoned
    }

    /// Present copies go to the guest blob.
    pub const fn copy_target(&self) -> bool {
        matches!(self.state, State::Ready)
    }

    /// The pages the guest blob names may be unlocked: nothing on the host can write them.
    /// The invariant the paging path relies on: false from `Creating` until `Gone`, and for
    /// ever after a poisoning failure.
    pub const fn may_unlock(&self) -> bool {
        !self.poisoned && matches!(self.state, State::None | State::Gone)
    }

    /// Start a create. `None`/`Gone` and not disabled only.
    pub fn begin_create(&mut self) -> Result<(), Why> {
        if self.disabled() {
            return Err(Why::Disabled);
        }
        match self.state {
            State::None | State::Gone => {
                self.state = State::Creating;
                self.guest = 0;
                self.runs = 0;
                Ok(())
            }
            State::Creating | State::Draining => Err(Why::Busy),
            State::Ready => Err(Why::Busy),
        }
    }

    /// The create reached the host as `guest` with `runs` runs (before the import): from here
    /// a failure must release it.
    pub fn sent(&mut self, guest: u32, runs: u32) {
        if self.state == State::Creating {
            self.guest = guest;
            self.runs = runs;
        }
    }

    /// Created and imported.
    pub fn created(&mut self) -> bool {
        if self.state != State::Creating || self.guest == 0 {
            return false;
        }
        self.state = State::Ready;
        true
    }

    /// The create or the import failed with `why` and everything it made was released (or
    /// nothing was made). One strike; the destination returns to `None`.
    pub fn create_failed(&mut self, why: Why) {
        if self.state != State::Creating {
            return;
        }
        self.strikes = self.strikes.saturating_add(1);
        if why.poisons() {
            self.poisoned = true;
            self.state = State::Draining;
            return;
        }
        self.state = State::None;
        self.guest = 0;
        self.runs = 0;
    }

    /// Retire the guest blob (the leases are about to change, the destination is going, a
    /// foreign consumer appeared).
    pub fn begin_drain(&mut self) -> Drain {
        if self.poisoned {
            return Drain::Poisoned;
        }
        match self.state {
            State::None | State::Gone => Drain::Nothing,
            // Creation holds the same content transaction as every lease change, so a lease
            // change never sees it; refuse to unlock if one ever did.
            State::Creating => Drain::Poisoned,
            State::Ready | State::Draining => {
                self.state = State::Draining;
                Drain::Release { guest: self.guest }
            }
        }
    }

    /// Drained and released: `Gone`. Returns the runs to refund.
    pub fn drained(&mut self) -> u32 {
        if self.state != State::Draining {
            return 0;
        }
        self.state = State::Gone;
        self.guest = 0;
        core::mem::take(&mut self.runs)
    }

    /// The drain or a release step failed with `why`: the record stays `Draining`, poisoned
    /// (the pages stay pinned), and the destination never uses a guest blob again.
    pub fn drain_failed(&mut self, why: Why) {
        self.strikes = self.strikes.saturating_add(1);
        if self.state == State::Draining || why.poisons() {
            self.poisoned = true;
            self.state = State::Draining;
        }
    }
}

/// What a Present may do with a destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligible {
    /// A guest blob is live: copy into it.
    Use,
    /// None yet and nothing forbids one: create it, then copy into it.
    Create,
    /// Not this Present (the reason). The legacy copy runs.
    No(Why),
    /// A live guest blob must be retired first (a foreign consumer appeared), then the legacy
    /// copy runs.
    Retire(Why),
}

/// The facts one Present decision needs.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub knob_on: bool,
    pub advertised: bool,
    pub dst_standard_buffer: bool,
    pub foreign_consumer: bool,
    /// The destination's system copy is marked invalid (`paging::InvalidSet::contains`): a
    /// skipped eviction or a `BltNoMirror` copy into the Venus blob. A page-in of the
    /// destination will be skipped and its Venus blob kept.
    pub system_copy_invalid: bool,
    /// The destination's record, `None` when it has none.
    pub record: Option<Record>,
}

/// The per-Present decision. Order: knob, host, destination shape, foreign consumer, system
/// copy marked invalid, state and strikes.
///
/// The invalid mark: a guest blob makes the leased pages the destination's newest copy, and a
/// marked destination's page-in is skipped in favour of the Venus blob. The two must never
/// meet, so the invariant is "a guest blob is the copy target only while the destination's
/// system copy is not marked invalid": no guest blob is created while the mark is up (here, and
/// re-checked by the create under the content transaction), and the first Blt that finds a
/// mark on a destination whose guest blob is live retires it ([`Eligible::Retire`]) before its
/// own copy, which then goes into the Venus blob (full surface): the blob the skipped page-in
/// keeps is then the newest copy again.
pub fn eligible(f: Facts) -> Eligible {
    if !f.knob_on {
        return Eligible::No(Why::KnobOff);
    }
    if !f.advertised {
        return Eligible::No(Why::NotAdvertised);
    }
    if !f.dst_standard_buffer {
        return Eligible::No(Why::NotBuffer);
    }
    let record = f.record.unwrap_or(Record::new(0));
    if f.foreign_consumer {
        return if record.copy_target() {
            Eligible::Retire(Why::ForeignConsumer)
        } else {
            Eligible::No(Why::ForeignConsumer)
        };
    }
    if f.system_copy_invalid {
        return if record.copy_target() {
            Eligible::Retire(Why::SystemStale)
        } else {
            Eligible::No(Why::SystemStale)
        };
    }
    if record.copy_target() {
        return Eligible::Use;
    }
    if record.disabled() {
        return Eligible::No(Why::Disabled);
    }
    match record.state {
        State::None | State::Gone => Eligible::Create,
        State::Ready => Eligible::Use,
        State::Creating | State::Draining => Eligible::No(Why::Busy),
    }
}

/// What a Present does after its copy about the destination's system pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effect {
    /// CPU-copy the Venus blob into the leased pages.
    pub mirror: bool,
    /// Mark the system copy invalid (`BltNoMirror`).
    pub mark_stale: bool,
}

/// `guest_hit`: the copy went into the guest blob, i.e. into the leased pages themselves.
/// Then there is nothing to mirror and nothing is stale; this wins over `BltNoMirror`.
/// Otherwise `BltNoMirror` marks stale instead of mirroring, as before this feature.
pub const fn present_effect(guest_hit: bool, no_mirror_on: bool) -> Effect {
    if guest_hit {
        Effect {
            mirror: false,
            mark_stale: false,
        }
    } else if no_mirror_on {
        Effect {
            mirror: false,
            mark_stale: true,
        }
    } else {
        Effect {
            mirror: true,
            mark_stale: false,
        }
    }
}

/// Whether a deferred copy must be prepared again at submission: it was prepared for the guest
/// blob `prepared_for` (`None`: not for a guest blob, never), and the destination's copy
/// target is now `current` (the guest blob `VenusClient::guest_target_for` finds, `None`: the
/// Venus blob). A copy whose guest blob was retired since the Present is re-prepared into the
/// current target and submitted (its mirror then decided by [`present_effect`] from the new
/// target), never dropped: the frame lands.
pub const fn retarget_needed(prepared_for: Option<u32>, current: Option<u32>) -> bool {
    match (prepared_for, current) {
        (None, _) => false,
        (Some(was), Some(now)) => was != now,
        (Some(_), None) => true,
    }
}

/// What the DIRECT asynchronous route does with a copy it has prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectCopy {
    /// The route was open only because of a guest blob (`BltNoMirror` 0) and the copy does not
    /// go into one: submit nothing, the legacy arm (which mirrors) runs.
    Refuse,
    /// Submit it; `mark_stale`: mark the system copy invalid first.
    Submit { mark_stale: bool },
}

/// The DIRECT route's decision from the ONE predicate: `guest` is the target the prepare chose
/// (`VenusClient::guest_target_for`, in the same hold of the Venus mutex as the submission),
/// never a second look at the guest buffers. `need_guest`: the route was open only because of
/// the destination's guest blob (`BltNoMirror` is 0). The route is otherwise open only with
/// `BltNoMirror` on, so the stale mark is [`present_effect`]'s with it on.
pub const fn direct_copy(need_guest: bool, guest: bool) -> DirectCopy {
    if need_guest && !guest {
        return DirectCopy::Refuse;
    }
    DirectCopy::Submit {
        mark_stale: present_effect(guest, true).mark_stale,
    }
}

/// Whether an open of `resource_id` belongs to a process other than `presenter` (rows are
/// `(resource_id, process, refs)` of the Present-buffer open table). The presenter is the
/// process whose app device presents into the destination: the census (`rm-backed-standard.md`
/// 8) saw every Shadow/Staging opened by the app itself, so any other opener is a consumer
/// that samples the Venus blob. An unknown presenter (0) counts every open as foreign.
pub fn foreign_consumer(
    rows: impl Iterator<Item = (u32, usize, u32)>,
    resource_id: u32,
    presenter: usize,
) -> bool {
    let mut rows = rows;
    rows.any(|(rid, process, refs)| {
        rid == resource_id && refs != 0 && (presenter == 0 || process != presenter)
    })
}

/// The counters this feature writes, all in `kmd_render/src/ddi/guest_blob.rs`. At most 14
/// characters and unique across `kmd_render` and `kmd_logic`. A test below checks the list
/// against the I/O file.
pub const COUNTERS: &[&str] = &[
    // The knob in force, and whether the host advertises the feature.
    "GbKnob",
    "GbFeat",
    // Guest blobs created and imported; Present copies into one; teardowns at paging, destroy
    // or a foreign consumer; the drain's total and longest microseconds.
    "GbMade",
    "GbHit",
    "GbDrop",
    "GbDrainUs",
    "GbDrainMax",
    // Failures (strikes), the last reason, every reason seen, decisions that kept the legacy
    // copy, destinations disabled by strikes, destinations whose pages stay pinned.
    "GbFail",
    "GbWhy",
    "GbMask",
    "GbRefuse",
    "GbStrike",
    "GbLeak",
    // The last create's runs and bytes; live blobs and runs (the host's limits).
    "GbRuns",
    "GbBytes",
    "GbLive",
    "GbLiveRuns",
    // Deferred copies prepared for a guest blob that was retired before they were submitted
    // (each re-prepared into the destination's current target, so the frame still lands).
    "GbLost",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::contract::*;
    use super::*;
    use std::vec::Vec;

    fn piece(blob_offset: u64, size: u64, byte_offset: u64, pfns: &[u64]) -> Piece<'_> {
        Piece {
            blob_offset,
            size,
            byte_offset,
            pfns,
        }
    }

    fn runs(pieces: &[Piece<'_>], cover: u64) -> Result<Vec<Run>, Why> {
        let mut out = Vec::new();
        let n = build_runs(pieces, cover, |r| out.push(r))?;
        assert_eq!(n, out.len());
        Ok(out)
    }

    #[test]
    fn contiguous_pages_are_one_run() {
        let pfns = [100, 101, 102, 103];
        let p = [piece(0, 4 * PAGE, 0, &pfns)];
        assert_eq!(
            runs(&p, 4 * PAGE).unwrap(),
            [Run {
                addr: 100 * PAGE,
                len: 4 * PAGE
            }]
        );
    }

    #[test]
    fn scattered_pages_are_one_run_each_and_order_is_kept() {
        let pfns = [7, 3, 9, 4];
        let p = [piece(0, 4 * PAGE, 0, &pfns)];
        let r = runs(&p, 4 * PAGE).unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].addr, 7 * PAGE);
        assert_eq!(r[1].addr, 3 * PAGE);
        assert!(r.iter().all(|r| r.len == PAGE));
    }

    #[test]
    fn runs_coalesce_across_pieces() {
        let a = [10, 11];
        let b = [12, 13];
        let p = [piece(0, 2 * PAGE, 0, &a), piece(2 * PAGE, 2 * PAGE, 0, &b)];
        assert_eq!(
            runs(&p, 4 * PAGE).unwrap(),
            [Run {
                addr: 10 * PAGE,
                len: 4 * PAGE
            }]
        );
    }

    #[test]
    fn cover_shorter_than_the_leases_stops_at_the_cover() {
        let pfns = [1, 2, 3, 4, 5];
        let p = [piece(0, 5 * PAGE, 0, &pfns)];
        let r = runs(&p, 2 * PAGE).unwrap();
        assert_eq!(
            r,
            [Run {
                addr: PAGE,
                len: 2 * PAGE
            }]
        );
    }

    #[test]
    fn a_gap_is_uncovered() {
        let a = [1];
        let b = [3];
        let p = [piece(0, PAGE, 0, &a), piece(2 * PAGE, PAGE, 0, &b)];
        assert_eq!(runs(&p, 3 * PAGE), Err(Why::Uncovered));
        assert_eq!(runs(&[], PAGE), Err(Why::Uncovered));
        // Leases that end before the cover does.
        assert_eq!(runs(&p[..1], 2 * PAGE), Err(Why::Uncovered));
    }

    #[test]
    fn a_lease_that_starts_late_is_uncovered() {
        let a = [1];
        let p = [piece(PAGE, PAGE, 0, &a)];
        assert_eq!(runs(&p, 2 * PAGE), Err(Why::Uncovered));
    }

    #[test]
    fn unaligned_physical_start_is_refused() {
        let pfns = [1, 2];
        let p = [piece(0, PAGE, 16, &pfns)];
        assert_eq!(runs(&p, PAGE), Err(Why::Unaligned));
    }

    #[test]
    fn a_page_split_over_two_pieces_is_refused() {
        let a = [1];
        let b = [2];
        let p = [piece(0, PAGE / 2, 0, &a), piece(PAGE / 2, PAGE, 0, &b)];
        assert_eq!(runs(&p, PAGE), Err(Why::Unaligned));
    }

    #[test]
    fn a_piece_with_too_few_pfns_is_refused() {
        let pfns = [1];
        let p = [piece(0, 2 * PAGE, 0, &pfns)];
        assert_eq!(runs(&p, 2 * PAGE), Err(Why::Unaligned));
    }

    #[test]
    fn overlapping_or_empty_pieces_are_refused() {
        let a = [1, 2];
        let p = [piece(0, 2 * PAGE, 0, &a), piece(PAGE, PAGE, 0, &a)];
        assert_eq!(runs(&p, 2 * PAGE), Err(Why::Unaligned));
        let q = [piece(0, 0, 0, &a)];
        assert_eq!(runs(&q, PAGE), Err(Why::Unaligned));
    }

    #[test]
    fn a_page_aligned_offset_into_a_lease_is_fine() {
        // A lease of a range that started one page into its MDL's first page run.
        let pfns = [50, 51, 52];
        let p = [piece(0, 2 * PAGE, PAGE, &pfns)];
        assert_eq!(
            runs(&p, 2 * PAGE).unwrap(),
            [Run {
                addr: 51 * PAGE,
                len: 2 * PAGE
            }]
        );
    }

    #[test]
    fn bad_covers_are_refused() {
        let pfns = [1];
        let p = [piece(0, PAGE, 0, &pfns)];
        assert_eq!(runs(&p, 0), Err(Why::TooBig));
        assert_eq!(runs(&p, 100), Err(Why::TooBig));
        assert_eq!(runs(&p, MAX_BLOB_BYTES + PAGE), Err(Why::TooBig));
    }

    #[test]
    fn exactly_max_entries_pass_and_one_more_is_refused() {
        // Every other page: no two consecutive, so one run per page.
        let pfns: Vec<u64> = (0..(MAX_ENTRIES as u64 + 1)).map(|i| i * 2).collect();
        let p = [piece(0, pfns.len() as u64 * PAGE, 0, &pfns)];
        assert_eq!(
            build_runs(&p, MAX_ENTRIES as u64 * PAGE, |_| {}),
            Ok(MAX_ENTRIES)
        );
        assert_eq!(
            build_runs(&p, (MAX_ENTRIES as u64 + 1) * PAGE, |_| {}),
            Err(Why::TooManyRuns)
        );
    }

    #[test]
    fn heaven_window_scattered_fits() {
        // 1600x900 at pitch 6400: 5,760,000 bytes -> 1407 pages, in a 6,619,136-byte allocation.
        let cover = cover_len(6400, 900, 6_619_136).unwrap();
        assert_eq!(cover, 1407 * PAGE);
        let pfns: Vec<u64> = (0..1616u64).map(|i| 1000 + i * 3).collect();
        let p = [piece(0, 1616 * PAGE, 0, &pfns)];
        assert_eq!(build_runs(&p, cover, |_| {}), Ok(1407));
    }

    #[test]
    fn long_runs_are_split_at_the_entry_limit() {
        // MAX_RUN_BYTES is a page multiple that fits the u32 length field and is above the
        // largest blob, so a legal blob is never split and every run fits its entry.
        assert_eq!(MAX_RUN_BYTES % PAGE, 0);
        assert!(MAX_RUN_BYTES <= u32::MAX as u64);
        assert!(MAX_RUN_BYTES >= MAX_BLOB_BYTES);
        let pfns: Vec<u64> = (0..4096u64).collect();
        let p = [piece(0, 4096 * PAGE, 0, &pfns)];
        let r = runs(&p, 4096 * PAGE).unwrap();
        assert_eq!(
            r,
            [Run {
                addr: 0,
                len: 4096 * PAGE
            }]
        );
        assert!(r
            .iter()
            .all(|r| r.len <= MAX_RUN_BYTES && r.len % PAGE == 0));
    }

    #[test]
    fn counting_and_filling_agree() {
        let pfns = [5, 6, 9, 10, 11, 2];
        let p = [piece(0, 6 * PAGE, 0, &pfns)];
        let n = build_runs(&p, 6 * PAGE, |_| {}).unwrap();
        let r = runs(&p, 6 * PAGE).unwrap();
        assert_eq!(n, r.len());
        assert_eq!(n, 3);
        assert_eq!(r.iter().map(|r| r.len).sum::<u64>(), 6 * PAGE);
    }

    #[test]
    fn cover_rounds_up_to_pages_and_respects_the_allocation() {
        assert_eq!(cover_len(4, 1, 4096), Ok(PAGE));
        assert_eq!(cover_len(4096, 1, 4096), Ok(PAGE));
        assert_eq!(cover_len(4097, 1, 8192), Ok(2 * PAGE));
        assert_eq!(cover_len(4097, 1, 4097), Err(Why::Uncovered));
        assert_eq!(cover_len(0, 10, 4096), Err(Why::TooBig));
        assert_eq!(cover_len(65536, 8192, u64::MAX), Err(Why::TooBig));
        assert_eq!(cover_len(65536, 4096, u64::MAX), Ok(MAX_BLOB_BYTES));
    }

    #[test]
    fn budget_limits_blobs_and_runs() {
        let mut b = Budget::new();
        assert!(b.charge(MAX_LIVE_RUNS as usize).is_ok());
        assert_eq!(b.admits(1), Err(Why::Budget));
        b.refund(MAX_LIVE_RUNS);
        assert_eq!(b, Budget::new());
        for _ in 0..MAX_LIVE_BLOBS {
            b.charge(1).unwrap();
        }
        assert_eq!(b.charge(1), Err(Why::Budget));
        assert_eq!(b.live_blobs(), MAX_LIVE_BLOBS);
        assert_eq!(b.live_runs(), MAX_LIVE_BLOBS);
        b.refund(1);
        assert!(b.charge(1).is_ok());
        b.clear();
        assert_eq!(b.live_blobs(), 0);
        // Refunds never underflow.
        b.refund(5);
        assert_eq!(b, Budget::new());
    }

    #[test]
    fn lifecycle_happy_path() {
        let mut r = Record::new(7);
        assert!(r.may_unlock());
        r.begin_create().unwrap();
        assert!(!r.may_unlock());
        assert!(!r.copy_target());
        r.sent(42, 3);
        assert!(r.created());
        assert!(r.copy_target());
        assert!(!r.may_unlock());
        assert_eq!(r.begin_drain(), Drain::Release { guest: 42 });
        assert!(!r.copy_target());
        assert!(!r.may_unlock());
        assert_eq!(r.drained(), 3);
        assert_eq!(r.state, State::Gone);
        assert!(r.may_unlock());
        assert_eq!(r.begin_drain(), Drain::Nothing);
        // Gone may be created again (the next eviction).
        r.begin_create().unwrap();
        assert_eq!(r.state, State::Creating);
    }

    #[test]
    fn created_needs_a_sent_blob() {
        let mut r = Record::new(7);
        r.begin_create().unwrap();
        assert!(!r.created());
        let mut s = Record::new(7);
        assert!(!s.created());
    }

    #[test]
    fn three_strikes_disable() {
        let mut r = Record::new(1);
        for i in 0..MAX_STRIKES {
            assert!(!r.disabled(), "strike {i}");
            r.begin_create().unwrap();
            r.create_failed(Why::HostShape);
            assert_eq!(r.state, State::None);
            assert!(r.may_unlock());
        }
        assert!(r.disabled());
        assert_eq!(r.begin_create(), Err(Why::Disabled));
    }

    #[test]
    fn a_poisoning_failure_keeps_the_pages_pinned_for_ever() {
        let mut r = Record::new(1);
        r.begin_create().unwrap();
        r.sent(9, 1);
        assert!(r.created());
        assert_eq!(r.begin_drain(), Drain::Release { guest: 9 });
        r.drain_failed(Why::DrainTimeout);
        assert!(r.poisoned);
        assert!(!r.may_unlock());
        assert_eq!(r.begin_drain(), Drain::Poisoned);
        assert_eq!(r.begin_create(), Err(Why::Disabled));
        // drained() of a poisoned record is not reachable through begin_drain; even if
        // called, the record stays unlock-forbidden.
        r.drained();
        assert!(!r.may_unlock());
    }

    #[test]
    fn an_unreleasable_create_failure_poisons() {
        let mut r = Record::new(1);
        r.begin_create().unwrap();
        r.sent(9, 1);
        r.create_failed(Why::ReleaseFailed);
        assert!(r.poisoned);
        assert!(!r.may_unlock());
        assert_eq!(r.begin_drain(), Drain::Poisoned);
    }

    #[test]
    fn creating_is_never_unlockable_and_never_drained() {
        let mut r = Record::new(1);
        r.begin_create().unwrap();
        assert_eq!(r.begin_drain(), Drain::Poisoned);
        assert!(!r.may_unlock());
        assert_eq!(r.begin_create(), Err(Why::Busy));
    }

    #[test]
    fn a_retire_always_leaves_ready() {
        // The StopDevice sweep (`retire_all_for_stop`) retires "the first Ready record" until
        // there is none: every outcome of a retire must leave Ready, or the sweep would spin.
        for outcome in 0..3 {
            let mut r = Record::new(1);
            r.begin_create().unwrap();
            r.sent(3, 1);
            assert!(r.created());
            assert!(r.copy_target());
            assert_eq!(r.begin_drain(), Drain::Release { guest: 3 });
            match outcome {
                0 => {
                    r.drained();
                }
                1 => r.drain_failed(Why::DrainTimeout),
                _ => r.drain_failed(Why::ReleaseFailed),
            }
            assert!(!r.copy_target(), "outcome {outcome}");
            // A poisoned record keeps its pages until the generation reset.
            assert_eq!(r.may_unlock(), outcome == 0, "outcome {outcome}");
        }
    }

    #[test]
    fn a_failed_release_of_a_draining_record_retries_as_poisoned() {
        let mut r = Record::new(1);
        r.begin_create().unwrap();
        r.sent(3, 1);
        r.created();
        r.begin_drain();
        r.drain_failed(Why::ReleaseFailed);
        assert_eq!(r.state, State::Draining);
        assert_eq!(r.begin_drain(), Drain::Poisoned);
    }

    fn facts(record: Option<Record>) -> Facts {
        Facts {
            knob_on: true,
            advertised: true,
            dst_standard_buffer: true,
            foreign_consumer: false,
            system_copy_invalid: false,
            record,
        }
    }

    #[test]
    fn a_marked_system_copy_refuses_a_create_and_retires_a_live_blob() {
        // No record (or None/Gone): no create while marked, a decision, not a strike.
        let mut f = facts(None);
        f.system_copy_invalid = true;
        assert_eq!(eligible(f), Eligible::No(Why::SystemStale));
        assert!(!Why::SystemStale.strikes() && !Why::SystemStale.poisons());
        let mut gone = Record::new(5);
        gone.begin_create().unwrap();
        gone.sent(8, 1);
        gone.created();
        gone.begin_drain();
        gone.drained();
        let mut f = facts(Some(gone));
        f.system_copy_invalid = true;
        assert_eq!(eligible(f), Eligible::No(Why::SystemStale));
        // A live guest blob on a marked destination is retired before the copy.
        let mut live = Record::new(5);
        live.begin_create().unwrap();
        live.sent(8, 1);
        live.created();
        let mut f = facts(Some(live));
        f.system_copy_invalid = true;
        assert_eq!(eligible(f), Eligible::Retire(Why::SystemStale));
        // Unmarked, the same record is used.
        f.system_copy_invalid = false;
        assert_eq!(eligible(f), Eligible::Use);
        // A foreign consumer is decided first (same retire, its own reason).
        f.system_copy_invalid = true;
        f.foreign_consumer = true;
        assert_eq!(eligible(f), Eligible::Retire(Why::ForeignConsumer));
        // Busy and disabled destinations stay refused while marked.
        let mut creating = Record::new(5);
        creating.begin_create().unwrap();
        let mut f = facts(Some(creating));
        f.system_copy_invalid = true;
        assert!(matches!(eligible(f), Eligible::No(_)));
    }

    #[test]
    fn eligibility_order() {
        let mut f = facts(None);
        assert_eq!(eligible(f), Eligible::Create);
        f.knob_on = false;
        f.advertised = false;
        assert_eq!(eligible(f), Eligible::No(Why::KnobOff));
        f.knob_on = true;
        assert_eq!(eligible(f), Eligible::No(Why::NotAdvertised));
        f.advertised = true;
        f.dst_standard_buffer = false;
        assert_eq!(eligible(f), Eligible::No(Why::NotBuffer));
        f.dst_standard_buffer = true;
        f.foreign_consumer = true;
        assert_eq!(eligible(f), Eligible::No(Why::ForeignConsumer));
    }

    #[test]
    fn eligibility_by_state() {
        let mut r = Record::new(5);
        assert_eq!(eligible(facts(Some(r))), Eligible::Create);
        r.begin_create().unwrap();
        assert_eq!(eligible(facts(Some(r))), Eligible::No(Why::Busy));
        r.sent(8, 1);
        r.created();
        assert_eq!(eligible(facts(Some(r))), Eligible::Use);
        let mut f = facts(Some(r));
        f.foreign_consumer = true;
        assert_eq!(eligible(f), Eligible::Retire(Why::ForeignConsumer));
        r.begin_drain();
        assert_eq!(eligible(facts(Some(r))), Eligible::No(Why::Busy));
        r.drained();
        assert_eq!(eligible(facts(Some(r))), Eligible::Create);
        let mut d = Record::new(5);
        d.strikes = MAX_STRIKES;
        assert_eq!(eligible(facts(Some(d))), Eligible::No(Why::Disabled));
    }

    #[test]
    fn guest_wins_over_no_mirror_and_never_marks_stale() {
        assert_eq!(
            present_effect(true, true),
            Effect {
                mirror: false,
                mark_stale: false
            }
        );
        assert_eq!(
            present_effect(true, false),
            Effect {
                mirror: false,
                mark_stale: false
            }
        );
        assert_eq!(
            present_effect(false, true),
            Effect {
                mirror: false,
                mark_stale: true
            }
        );
        assert_eq!(
            present_effect(false, false),
            Effect {
                mirror: true,
                mark_stale: false
            }
        );
    }

    #[test]
    fn a_deferred_copy_whose_guest_blob_went_is_prepared_again() {
        // Not prepared for a guest blob: never touched (the default path).
        assert!(!retarget_needed(None, None));
        assert!(!retarget_needed(None, Some(7)));
        // Still the target: submitted as prepared.
        assert!(!retarget_needed(Some(7), Some(7)));
        // Retired (the Venus blob is the target now), or replaced by a newer guest blob.
        assert!(retarget_needed(Some(7), None));
        assert!(retarget_needed(Some(7), Some(9)));
        // The re-prepared copy's mirror follows its new target: into the Venus blob it is
        // mirrored (or marked stale with `BltNoMirror`), into a guest blob neither.
        assert!(present_effect(false, false).mirror);
        assert!(present_effect(false, true).mark_stale);
        let into_guest = present_effect(true, false);
        assert!(!into_guest.mirror && !into_guest.mark_stale);
    }

    #[test]
    fn the_direct_route_marks_exactly_when_its_copy_misses_the_guest_buffer() {
        // Opened by the guest blob alone: a copy that still goes into it needs no mark; one
        // that does not is refused (the legacy arm mirrors), never submitted unmarked.
        assert_eq!(
            direct_copy(true, true),
            DirectCopy::Submit { mark_stale: false }
        );
        assert_eq!(direct_copy(true, false), DirectCopy::Refuse);
        // Opened by `BltNoMirror`: the copy goes out either way, marked unless it went into
        // the guest buffer (the pages themselves).
        assert_eq!(
            direct_copy(false, true),
            DirectCopy::Submit { mark_stale: false }
        );
        assert_eq!(
            direct_copy(false, false),
            DirectCopy::Submit { mark_stale: true }
        );
        // Never a submission that leaves the pages older than the blob without a mark.
        for need in [false, true] {
            for guest in [false, true] {
                if let DirectCopy::Submit { mark_stale } = direct_copy(need, guest) {
                    assert_eq!(mark_stale, !guest);
                }
            }
        }
    }

    #[test]
    fn foreign_consumer_rule() {
        let rows = [(5u32, 100usize, 1u32), (6, 200, 1), (5, 300, 0)];
        // Only the presenter has 5 open (300's row has no refs).
        assert!(!foreign_consumer(rows.iter().copied(), 5, 100));
        // Another process has it open.
        assert!(foreign_consumer(rows.iter().copied(), 5, 200));
        // Nobody has 7 open.
        assert!(!foreign_consumer(rows.iter().copied(), 7, 100));
        // Unknown presenter: any open is foreign.
        assert!(foreign_consumer(rows.iter().copied(), 5, 0));
    }

    #[test]
    fn why_codes_are_unique_and_fit_the_mask() {
        let mut codes: Vec<u32> = Why::ALL.iter().map(|w| w.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), Why::ALL.len());
        assert!(codes.iter().all(|&c| (1..=32).contains(&c)));
        for w in Why::ALL {
            let decision = w.code() <= 11 || w == Why::SystemStale;
            assert_eq!(w.strikes(), !decision, "{w:?}");
            // Only a strike can poison.
            assert!(!w.poisons() || w.strikes(), "{w:?}");
        }
        assert!(Why::DrainTimeout.poisons() && Why::ReleaseFailed.poisons());
        assert!(!Why::HostShape.poisons());
    }

    #[test]
    fn deadlines_bound_every_wait_to_seconds() {
        use super::deadline::*;
        // Each step is far below the 30 s of a default round trip or ring wait, and a whole
        // retire or create blocks for a few seconds at most.
        for ms in [FENCE_MS, DRAIN_MS, UNREF_MS, CREATE_MS, IMPORT_MS] {
            assert!(ms > 0 && ms <= 1_000, "{ms}");
        }
        assert!(FENCE_MS < DRAIN_MS);
        assert_eq!(CREATE_TOTAL_MS, CREATE_MS + IMPORT_MS + UNREF_MS);
        assert!(CREATE_TOTAL_MS <= 3_000);
        assert!(Limits::NORMAL.total_ms() <= 2_000);
        // A cap cuts both limits, never to 0, and never raises them.
        assert_eq!(Limits::capped(u64::MAX), Limits::NORMAL);
        assert_eq!(
            Limits::capped(300),
            Limits {
                drain_ms: 300,
                unref_ms: 300
            }
        );
        assert_eq!(Limits::capped(0), Limits { drain_ms: 1, unref_ms: 1 });
        for cap in [0u64, 1, 250, 999, 1_000, 1_001, 5_000] {
            let l = Limits::capped(cap);
            assert!(l.drain_ms >= 1 && l.drain_ms <= DRAIN_MS);
            assert!(l.unref_ms >= 1 && l.unref_ms <= UNREF_MS);
        }
        // One fence wait: the per-fence limit, cut to what the section has left, never 0.
        assert_eq!(fence_wait_ms(None), FENCE_MS);
        assert_eq!(fence_wait_ms(Some(10_000)), FENCE_MS);
        assert_eq!(fence_wait_ms(Some(100)), 100);
        assert_eq!(fence_wait_ms(Some(0)), 1);
    }

    #[test]
    fn an_unanswered_create_poisons() {
        let mut r = Record::new(1);
        r.begin_create().unwrap();
        r.create_failed(Why::CreateTimeout);
        assert!(r.poisoned && r.disabled());
        assert!(!r.may_unlock());
        assert_eq!(r.begin_drain(), Drain::Poisoned);
        assert!(Why::CreateTimeout.strikes() && Why::CreateTimeout.poisons());
    }

    #[test]
    fn contract_advertised_needs_both_bits() {
        let gb = 1u32 << 16;
        assert!(advertised(gb | CFG_VENUS, gb));
        assert!(!advertised(gb, gb));
        assert!(!advertised(CFG_VENUS, gb));
        assert!(!advertised(u32::MAX, 0));
    }

    #[test]
    fn contract_entry_layout() {
        let e = encode_entry(0x1_2345_6000, 0x3000);
        assert_eq!(&e[0..8], &0x1_2345_6000u64.to_le_bytes());
        assert_eq!(&e[8..12], &0x3000u32.to_le_bytes());
        assert_eq!(&e[12..16], &[0, 0, 0, 0]);
        assert_eq!(ENTRY_BYTES, 16);
        assert_eq!(CREATE_BYTES, 56);
        assert_eq!(BLOB_MEM, 1);
        assert_eq!(BLOB_FLAGS & REFUSED_BLOB_FLAGS, 0);
        assert_eq!(BLOB_ID, 0);
    }

    #[test]
    fn contract_error_classes() {
        assert_eq!(
            classify_create(RESP_ERR_UNSPEC, EOPNOTSUPP),
            Why::HostUnsupported
        );
        assert_eq!(classify_create(RESP_ERR_UNSPEC, EIO), Why::HostRenderer);
        assert_eq!(classify_create(RESP_ERR_UNSPEC, 0), Why::HostOther);
        assert_eq!(
            classify_create(RESP_ERR_INVALID_CONTEXT_ID, 0),
            Why::HostContext
        );
        assert_eq!(
            classify_create(RESP_ERR_INVALID_RESOURCE_ID, 0),
            Why::HostResourceId
        );
        assert_eq!(
            classify_create(RESP_ERR_INVALID_PARAMETER, EINVAL),
            Why::HostShape
        );
        assert_eq!(
            classify_create(RESP_ERR_INVALID_PARAMETER, EFAULT),
            Why::HostFault
        );
        assert_eq!(
            classify_create(RESP_ERR_INVALID_PARAMETER, EXDEV),
            Why::HostCrossFile
        );
        assert!(Why::HostCrossFile.strikes() && !Why::HostCrossFile.poisons());
        assert_eq!(
            classify_create(RESP_ERR_OUT_OF_MEMORY, ENOMEM),
            Why::HostNoMemory
        );
        assert_eq!(classify_create(0x1234, 0), Why::HostOther);
        assert_eq!(
            classify_import(VK_ERROR_INVALID_EXTERNAL_HANDLE),
            Why::ImportHandle
        );
        assert_eq!(
            classify_import(VK_ERROR_OUT_OF_DEVICE_MEMORY),
            Why::ImportNoMemory
        );
        assert_eq!(classify_import(-1), Why::ImportOther);
        for w in [
            classify_create(RESP_ERR_UNSPEC, EOPNOTSUPP),
            classify_import(-1),
        ] {
            assert!(w.strikes());
        }
    }

    #[test]
    fn contract_memory_type_choice() {
        let v = crate::MEMORY_PROPERTY_HOST_VISIBLE;
        let c = crate::MEMORY_PROPERTY_HOST_COHERENT;
        let d = crate::MEMORY_PROPERTY_DEVICE_LOCAL;
        // The host prototype: types 2 and 3 are HOST_VISIBLE|HOST_COHERENT.
        let flags = [d, d | v, v | c, v | c | crate::MEMORY_PROPERTY_HOST_CACHED];
        assert_eq!(choose_memory_type(&flags, 4, 0b1100), Some(2));
        assert_eq!(choose_memory_type(&flags, 4, 0b1000), Some(3));
        // Visible but not coherent: refused.
        assert_eq!(choose_memory_type(&flags, 4, 0b0010), None);
        assert_eq!(choose_memory_type(&flags, 4, 0), None);
        // Bits beyond the count are ignored.
        assert_eq!(choose_memory_type(&flags, 2, 0b1100), None);
    }

    #[test]
    fn contract_get_resource_properties_bytes() {
        let w = encode_get_resource_properties(0x11, 0x22);
        let b = w.finished().unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&192u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&0x11u64.to_le_bytes());
        want.extend_from_slice(&0x22u32.to_le_bytes());
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&1000384001i32.to_le_bytes());
        want.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(b, &want[..]);
    }

    #[test]
    fn contract_create_buffer_bytes() {
        let w = encode_create_buffer(0x11, 0x33, 0x5000);
        let b = w.finished().unwrap();
        let mut want = Vec::new();
        want.extend_from_slice(&50u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&0x11u64.to_le_bytes());
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&12i32.to_le_bytes());
        want.extend_from_slice(&0u64.to_le_bytes()); // pNext
        want.extend_from_slice(&0u32.to_le_bytes()); // flags
        want.extend_from_slice(&0x5000u64.to_le_bytes());
        want.extend_from_slice(&2u32.to_le_bytes()); // TRANSFER_DST
        want.extend_from_slice(&0u32.to_le_bytes()); // exclusive
        want.extend_from_slice(&0u32.to_le_bytes()); // queueFamilyIndexCount
        want.extend_from_slice(&0u64.to_le_bytes()); // pQueueFamilyIndices
        want.extend_from_slice(&0u64.to_le_bytes()); // pAllocator
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&0x33u64.to_le_bytes());
        assert_eq!(b, &want[..]);
        // The same body as the Present buffer's create minus its external-memory struct.
        let ext = crate::external_memory::encode_present_buffer(0x11, 0x33, 0x5000);
        assert_eq!(ext.finished().unwrap().len(), b.len() + 4 + 8 + 4);
    }

    // ── counter names ────────────────────────────────────────────────────────────────────

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Gb"));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        for n in COUNTERS {
            assert!(!crate::blt_async::COUNTERS.contains(n));
        }
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join("ddi/guest_blob.rs")).unwrap();
        let written = literals(&text);
        for n in COUNTERS {
            assert!(
                written.iter().any(|l| l == n),
                "{n} is listed but not written by ddi/guest_blob.rs"
            );
        }
        for l in written.iter().filter(|l| l.starts_with("Gb")) {
            assert!(
                COUNTERS.contains(&l.as_str()),
                "{l} is written by ddi/guest_blob.rs but not listed"
            );
        }
    }

    #[test]
    fn no_other_file_writes_these_names_and_the_knob_is_in_diag() {
        let Some(render) = render_src() else {
            return;
        };
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().into_owned();
                    if s.ends_with("ddi/guest_blob.rs") || s.ends_with("/diag.rs") {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(!text.contains("b\"Gb"), "{s} spells a Gb counter name");
                    assert!(!text.contains("b\"GuestBlob\""), "{s} spells the knob name");
                }
            }
        }
        assert!(checked > 20);
        let diag = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        assert!(diag.contains("KnobName::new(b\"GuestBlob\")"));
        assert!("GuestBlob".len() <= 14);
    }
}

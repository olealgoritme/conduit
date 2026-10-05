//! `HELIOS_ESCAPE_NVRM` — the RM forwarding escape (NVK on RM, Windows guests).
//!
//! One escape verb, many sub-operations (`op`), carried over the same
//! `D3DKMTEscape` private-data buffer as every other Helios escape. It lets a
//! user-mode RM client (the Windows transport of `guest/rmclient`) speak the
//! device's NVIDIA-RM forwarding protocol (`host/backend/protocol`,
//! `MsgType::{Open,Close,Ioctl,Mmap,Munmap,GetProcFiles,GetSysFiles,EventReady}`)
//! through the KMD, which owns the virtio device. The C mirror for the user
//! side is `guest/rmclient/src/helios_nvrm_escape.h`; the two files are kept
//! byte-identical in layout and the sizes/offsets are asserted on both sides.
//!
//! # What the KMD is, and is not
//!
//! The KMD is a **pipe with ownership**, not an RM client. It does not
//! interpret RM: the per-ioctl pointer fix-ups (data / nested / deep blocks),
//! the device tables read via `GetSysFiles` (slots, allocation sizes, fd
//! translations, UVM commands, OS-descriptor layout) and every RM struct stay
//! in user mode. The KMD reads exactly:
//!
//! * the 16-byte `MsgHeader` of a forwarded request (`msg_type`, `handle`);
//! * for `msg_type == Ioctl` only, the 24-byte `IoctlReq` that follows it
//!   (`cmd, data_len, nested_offset, nested_len, deep_ptr_offset, deep_len`) —
//!   solely to enforce the physical-address rule below and to check that the
//!   declared lengths add up to the request;
//! * the 16-byte `MsgHeader` of an `Open` / `Close` reply (`handle`, `status`),
//!   to learn which backend handle the calling process now owns.
//!
//! Everything else it adds is bookkeeping a process must not be able to get
//! wrong about another process: handle ownership, mapping / pin / event
//! lifetime, teardown on process exit and on device reset.
//!
//! # SECURITY: user mode never supplies or sees a physical address
//!
//! Page runs name guest-physical pages, and the host maps whatever a run names.
//! If a process could put runs in a request it could make the GPU DMA anywhere
//! in guest RAM. So:
//!
//! * `FORWARD` REFUSES (`HELIOS_NVRM_ST_FORBIDDEN`) an `Ioctl` whose
//!   `deep_ptr_offset` is [`HELIOS_NVRM_DEEP_PAGE_RUNS`] or
//!   [`HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT`] — in any request, always.
//! * Runs are produced ONLY by the KMD, from pages it locked itself with
//!   `MmProbeAndLockPages` (`PIN`). The caller never receives them: `PIN`
//!   returns an opaque `pin_id`, and a `FORWARD` that carries `pin_id != 0`
//!   makes the KMD splice its own table into the request (it requires the
//!   request's `deep_ptr_offset` and `deep_len` to be 0, then sets both).
//! * A pin that has been used by a `FORWARD` is **committed**: the GPU may now
//!   hold the pages. It is released only by `Close` of its handle (after the
//!   host has torn the objects down), process exit, or device reset — never by
//!   a user `UNPIN` (`HELIOS_NVRM_ST_PIN_IN_USE`). Otherwise a process could
//!   unpin, free the pages, and have the GPU write into whoever receives them
//!   next. `UNPIN` is for a pin that never reached a `FORWARD` (a failure path).
//!   (A later optimisation may release a committed pin when the KMD sees the
//!   matching successful `RM_FREE`; that needs hClient/hObject tracking and is
//!   not part of ABI v1.)
//!
//! # Buffer layout and sizes
//!
//! Every request is `repr(C)`, padding-free, 8-byte aligned, little-endian, and
//! begins with [`HeliosNvrmHeader`] (40 bytes, whose first 16 bytes are the
//! ordinary [`HeliosEscapeHeader`] with `cmd_type = HELIOS_ESCAPE_NVRM` and
//! `size` = the whole buffer). Variable-length escapes append their bytes
//! directly after the fixed struct; each op below states its exact layout. The
//! KMD requires `buffer length == hdr.size >= size_of::<OpStruct>()` and, for
//! variable ops, the exact total the layout implies. Buffers over
//! [`HELIOS_NVRM_MAX_BUFFER`] are refused (`QUERY_CAPS` reports the KMD's
//! actual limit, which is never larger).
//!
//! # 32-bit / 64-bit rules (WoW64)
//!
//! There are NO pointers and NO `HANDLE`/`size_t`/`long` fields in any struct.
//! Addresses are `u64`; an OS `HANDLE` is a `u64`, **zero-extended** from a
//! 32-bit process exactly as `HeliosEscapeFenceEvent.event_handle` already is.
//! The layout is therefore identical for a 32-bit caller (WoW64) and a 64-bit
//! one, and the KMD needs no thunk. A 32-bit process can still be refused for
//! lack of address space (`MMAP` → [`HELIOS_NVRM_ST_NO_RESOURCES`]).
//!
//! # Result reporting — three layers, never conflate them
//!
//! 1. **The escape's NTSTATUS** is the transport verdict for malformed or
//!    unservable requests: `STATUS_NOT_IMPLEMENTED` (an older KMD that does not
//!    know the verb — this IS the capability probe), `STATUS_INVALID_PARAMETER`
//!    (bad magic/version/size/op), `STATUS_DEVICE_NOT_READY` (no virtio
//!    transport). On failure the buffer is not written.
//! 2. **`HeliosNvrmHeader.status`** (`HELIOS_NVRM_ST_*`) is the KMD's verdict on
//!    an otherwise well-formed request (ownership, quotas, scatter, a stale
//!    handle after reset, a timeout). Written whenever the escape returns
//!    `STATUS_SUCCESS`; `0` is [`HELIOS_NVRM_ST_OK`].
//! 3. **The RM result** — the reply `MsgHeader.status` (a negative errno,
//!    signed) and the RM parameter struct's own `status` field — travels in
//!    the forwarded reply bytes and is NEVER interpreted by the KMD.
//!
//! `epoch` (output of every op) is a device generation number. It changes when
//! the device is reset or the transport replaced; all backend handles, mappings,
//! pins and event registrations of earlier epochs are dead (the KMD has already
//! released them). A client that sees its remembered epoch change must reopen.
//!
//! # Concurrency
//!
//! Any number of threads may issue any ops concurrently; the KMD does not
//! serialise them and ordering between threads is the caller's. `FORWARD` blocks
//! the calling thread at PASSIVE (an event wait, not a poll) until the device
//! replies or `timeout_ms` elapses.

use crate::HeliosEscapeHeader;
use bytemuck::{Pod, Zeroable};

/// The escape verb. `0x0013` is the producer, `0x0014` is retired, `0x0015` is
/// snapshot status.
pub const HELIOS_ESCAPE_NVRM: u32 = 0x0016;
/// Version of this ABI. Carried in every request; a KMD that does not know it
/// answers `STATUS_INVALID_PARAMETER`. Additive changes (new ops, new
/// `HELIOS_NVRM_ST_*`, new flag bits) do NOT bump it — `QUERY_CAPS` is how a
/// client discovers them.
pub const HELIOS_NVRM_ABI_VERSION: u32 = 1;

/// Largest escape buffer this ABI will ever ask for (header + payload + reply).
/// RM parameter blocks are ≤ 64 KiB and a deep block ≤ 64 KiB, so ordinary
/// forwards are far below it; the limit exists so the KMD can bound its staging.
pub const HELIOS_NVRM_MAX_BUFFER: u32 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

/// Report KMD capabilities and limits. See [`HeliosNvrmQueryCaps`].
pub const HELIOS_NVRM_OP_QUERY_CAPS: u32 = 1;
/// Forward one `MsgHeader | payload` request to the device and return its reply.
/// See [`HeliosNvrmForward`].
pub const HELIOS_NVRM_OP_FORWARD: u32 = 2;
/// Host `Mmap` + map the returned device range into the caller.
/// See [`HeliosNvrmMmap`].
pub const HELIOS_NVRM_OP_MMAP: u32 = 3;
/// Unmap a mapping made by `MMAP` and send host `Munmap`.
/// See [`HeliosNvrmMunmap`].
pub const HELIOS_NVRM_OP_MUNMAP: u32 = 4;
/// Register a persistent usermode event for device notifications on a handle.
/// See [`HeliosNvrmEvent`].
pub const HELIOS_NVRM_OP_EVENT_REGISTER: u32 = 5;
/// Remove an `EVENT_REGISTER` registration. See [`HeliosNvrmEvent`].
pub const HELIOS_NVRM_OP_EVENT_UNREGISTER: u32 = 6;
/// Lock user pages and describe them as guest-physical page runs for an
/// OS-descriptor registration. See [`HeliosNvrmPin`].
pub const HELIOS_NVRM_OP_PIN: u32 = 7;
/// Release a `PIN`. See [`HeliosNvrmUnpin`].
pub const HELIOS_NVRM_OP_UNPIN: u32 = 8;

// ---------------------------------------------------------------------------
// KMD status codes (`HeliosNvrmHeader.status`). 0 = success, otherwise positive.
// ---------------------------------------------------------------------------

pub const HELIOS_NVRM_ST_OK: i32 = 0;
/// The `handle` / `mapping_id` / `pin_id` is not owned by the calling process
/// (or does not exist). Deliberately one code: a process learns nothing about
/// another's objects.
pub const HELIOS_NVRM_ST_NOT_OWNED: i32 = 1;
/// `FORWARD` was given a `msg_type` outside [`HELIOS_NVRM_FORWARD_MSG_TYPES`].
pub const HELIOS_NVRM_ST_MSG_TYPE_REFUSED: i32 = 2;
/// The device failed the request at the transport level (no/short reply, or the
/// device reported an error for the virtio request itself). The RM was not
/// reached or its answer is lost.
pub const HELIOS_NVRM_ST_DEVICE_ERROR: i32 = 3;
/// The device was reset / the transport replaced since the call began or since
/// the caller's objects were created. `epoch` holds the new generation.
pub const HELIOS_NVRM_ST_TRANSPORT_RESET: i32 = 4;
/// `PIN`: the range scatters into more runs than the KMD can describe.
pub const HELIOS_NVRM_ST_TOO_SCATTERED: i32 = 5;
/// A KMD quota or resource was exhausted (handles, mappings, pins, nonpaged
/// memory, user address space, ring space past the timeout).
pub const HELIOS_NVRM_ST_NO_RESOURCES: i32 = 6;
/// The op/kind/cache type/flag is valid in this ABI but not provided by this
/// KMD build. `QUERY_CAPS` says what is.
pub const HELIOS_NVRM_ST_UNSUPPORTED: i32 = 7;
/// `FORWARD` exceeded `timeout_ms`. The request may still be in flight; the KMD
/// discards its reply when it arrives and the response area holds nothing.
/// (An in-flight `Close`/`Ioctl` may still take effect — treat the handle as
/// indeterminate and close it.)
pub const HELIOS_NVRM_ST_TIMEOUT: i32 = 8;
/// The reply is longer than `resp_cap` (only when the KMD can tell). The reply
/// is dropped; retry with a larger response area.
pub const HELIOS_NVRM_ST_RESP_TRUNCATED: i32 = 9;
/// A size/alignment/range field is out of bounds for this op (e.g. `PIN` not
/// whole pages, `MMAP` size 0, a request larger than the KMD's limit, an
/// `IoctlReq` whose `data_len + nested_len + deep_len` does not fill the request).
pub const HELIOS_NVRM_ST_BAD_RANGE: i32 = 10;
/// The request carries something user mode may not supply: a page-run
/// `deep_ptr_offset`, or a `pin_id` request whose own deep fields are not 0.
/// See the SECURITY section of the module docs.
pub const HELIOS_NVRM_ST_FORBIDDEN: i32 = 11;
/// `UNPIN`: the pin was committed by a `FORWARD` and is released only by `Close`
/// of its handle, process exit or reset.
pub const HELIOS_NVRM_ST_PIN_IN_USE: i32 = 12;

// ---------------------------------------------------------------------------
// Common header
// ---------------------------------------------------------------------------

/// First 40 bytes of every `HELIOS_ESCAPE_NVRM` buffer.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmHeader {
    /// `cmd_type = HELIOS_ESCAPE_NVRM`, `size` = total buffer bytes.
    pub hdr: HeliosEscapeHeader,
    /// in: [`HELIOS_NVRM_ABI_VERSION`].
    pub abi_version: u32,
    /// in: one of `HELIOS_NVRM_OP_*`.
    pub op: u32,
    /// out: one of `HELIOS_NVRM_ST_*`.
    pub status: i32,
    /// in: zero. out: zero.
    pub reserved: u32,
    /// out: device generation (see the module docs). Written on every success
    /// return, including non-zero `status`.
    pub epoch: u64,
}

pub const HELIOS_NVRM_HEADER_BYTES: usize = 40;

// ---------------------------------------------------------------------------
// QUERY_CAPS
// ---------------------------------------------------------------------------

/// `QUERY_CAPS`. 88 bytes, no trailing data. Safe to call before anything else;
/// it touches no device state, so it works even when the transport is down
/// (then `device_features` is 0 and the other ops would fail).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmQueryCaps {
    pub head: HeliosNvrmHeader,
    /// out: the largest escape buffer this KMD accepts (≤ `HELIOS_NVRM_MAX_BUFFER`).
    pub max_buffer_bytes: u32,
    /// out: `FORWARD` default when `timeout_ms == 0`, in milliseconds.
    pub default_timeout_ms: u32,
    /// out: bit `n` set ⇔ `HELIOS_NVRM_OP_*` value `n` is implemented.
    pub supported_ops: u64,
    /// out: bit `n` set ⇔ event kind `n` (`HELIOS_NVRM_EVENT_*`) is implemented.
    pub supported_event_kinds: u32,
    /// out: bit `n` set ⇔ cache type `n` (`HELIOS_NVRM_CACHE_*`) is implemented.
    pub supported_cache_types: u32,
    /// out: the virtio device's `features` config word (host
    /// `NVGPU_CFG_*` bits: display, cursor, venus, drm fences, …). 0 if the
    /// transport is down.
    pub device_features: u32,
    /// out: per-process limit on live backend handles.
    pub max_handles: u32,
    /// out: per-process limit on live `MMAP` mappings.
    pub max_mappings: u32,
    /// out: per-process limit on live pins.
    pub max_pins: u32,
    /// out: most pages one `PIN` accepts.
    pub max_pin_pages: u32,
    /// out: bit 0 ⇔ the KMD can describe a pin with a direct page-run table; bit 1
    /// ⇔ it can also use an indirect table (ranges scattering into more than
    /// `HELIOS_NVRM_PAGE_RUNS_MAX` runs).
    pub pin_deep_kinds: u32,
}

pub const HELIOS_NVRM_PIN_DEEP_BIT_DIRECT: u32 = 1 << 0;
pub const HELIOS_NVRM_PIN_DEEP_BIT_INDIRECT: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// FORWARD
// ---------------------------------------------------------------------------

/// `msg_type` values (host `MsgType`) that `FORWARD` accepts: Open 1, Close 2,
/// Ioctl 3, GetProcFiles 6, GetSysFiles 7. As a bitmask over the value.
///
/// `Mmap` (4) / `Munmap` (5) are refused — they need KMD mapping work and have
/// their own ops. Everything else (EventReady 8, scanout/input/clipboard
/// 20–27, GpuCmd 30) is refused; those have dedicated paths or are
/// host → guest only.
pub const HELIOS_NVRM_FORWARD_MSG_TYPES: u32 =
    (1 << 1) | (1 << 2) | (1 << 3) | (1 << 6) | (1 << 7);

/// Bytes of the host `MsgHeader` at the start of every request and reply.
pub const HELIOS_NVRM_MSG_HEADER_BYTES: usize = 16;

/// `FORWARD`. 64-byte struct, then the request bytes, then the response area:
///
/// ```text
/// +0                      HeliosNvrmForward            (64 bytes)
/// +64                     request  = MsgHeader | payload   (req_len bytes)
/// +64 + align8(req_len)   response area                    (resp_cap bytes)
/// total (== head.hdr.size) = 64 + align8(req_len) + resp_cap
/// ```
///
/// The request bytes are exactly what the Linux module would put on the wire
/// (`struct nvgpu_ioctl_req` etc.): the KMD forwards them **verbatim** as the
/// device's request and hands the device `resp_cap` writable bytes for the
/// reply, which the KMD copies into the response area. The reply is the
/// device's bytes as-is (`MsgHeader | payload`, or the header-less stream for
/// `GetProcFiles`/`GetSysFiles`); `resp_len` is how many are valid. Size
/// `resp_cap` the way the Linux module sizes `resp_max` (e.g. 128 KiB for
/// `GetSysFiles`).
///
/// Ownership: `Ioctl`/`Close` require `MsgHeader.handle` to be a handle this
/// process opened (`NOT_OWNED` otherwise). `Open` registers the reply's handle
/// to the caller. A successful `Close` first tears down that handle's
/// mappings, pins and event registrations (Linux does the same on release),
/// then is forwarded. `GetProcFiles`/`GetSysFiles` carry `handle = 0` and need
/// no ownership.
///
/// `pin_id != 0` (an `Ioctl` only): the KMD appends the page-run table of that
/// pin (owned by the caller and made under the same `handle`) as the request's
/// deep block and sets `deep_ptr_offset`/`deep_len` itself; the request must
/// carry `deep_ptr_offset == 0 && deep_len == 0`. The pin becomes committed if
/// the registration succeeds, judged by `rm_status_off` (see [`HeliosNvrmPin`]).
/// With `pin_id == 0` a page-run `deep_ptr_offset` is `FORBIDDEN`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmForward {
    pub head: HeliosNvrmHeader,
    /// in: bytes of request following the struct (≥ 16, ≤ the KMD limit).
    pub req_len: u32,
    /// in: bytes of response area after the (8-aligned) request.
    pub resp_cap: u32,
    /// out: valid bytes of reply at the start of the response area.
    pub resp_len: u32,
    /// in: milliseconds to wait for the device; 0 = `default_timeout_ms`.
    pub timeout_ms: u32,
    /// in: a `PIN` id whose KMD-built page-run table rides this request, or 0.
    pub pin_id: u32,
    /// in: with `pin_id != 0`, the byte offset, inside the reply's data block
    /// (after the `MsgHeader` and the 12-byte `IoctlResp`), of the 32-bit RM
    /// status the registration returns (e.g. `NVOS02_PARAMETERS.status`). The KMD
    /// keeps the pin only if the host's status and that word are both 0, and
    /// releases it otherwise. Zero when `pin_id == 0`.
    pub rm_status_off: u32,
}

pub const HELIOS_NVRM_FORWARD_BYTES: usize = 64;

/// Offset of the request bytes in a `FORWARD` buffer.
pub const HELIOS_NVRM_FORWARD_REQ_OFFSET: usize = HELIOS_NVRM_FORWARD_BYTES;

/// Offset of the response area in a `FORWARD` buffer.
pub const fn helios_nvrm_forward_resp_offset(req_len: u32) -> usize {
    HELIOS_NVRM_FORWARD_BYTES + ((req_len as usize + 7) & !7)
}

// ---------------------------------------------------------------------------
// MMAP / MUNMAP
// ---------------------------------------------------------------------------

/// `prot` bits (`MMAP`). Execute is never granted.
pub const HELIOS_NVRM_PROT_READ: u32 = 1;
pub const HELIOS_NVRM_PROT_WRITE: u32 = 2;

/// Cache types (`MMAP.cache_request` / `cache_effective`).
///
/// `DEFAULT` lets the KMD choose and is **write-combined**, matching the Linux
/// module (`pgprot_writecombine` on both of its mmap paths). Mixing attributes
/// across live mappings of the same pages is unsafe; a client should leave
/// this `DEFAULT` unless it has a reason.
pub const HELIOS_NVRM_CACHE_DEFAULT: u32 = 0;
pub const HELIOS_NVRM_CACHE_UC: u32 = 1;
pub const HELIOS_NVRM_CACHE_WC: u32 = 2;
pub const HELIOS_NVRM_CACHE_WB: u32 = 3;

/// `MMAP`. 88 bytes, no trailing data.
///
/// The KMD sends host `Mmap{size, offset, prot}` on `handle` (the RM file the
/// mmap cookie `offset` belongs to — the same value the Linux module passes as
/// `vm_pgoff << PAGE_SHIFT`), receives `{guest_phys_addr, size, mapping_id}`,
/// maps that device range into the caller with `MmMapLockedPagesSpecifyCache`
/// (user mode; read-only unless `PROT_WRITE`) and returns the user VA.
///
/// `mapping_id` is the **host's** id (device-wide unique), so it cannot collide
/// across processes; the KMD keys its table on (owner, mapping_id). The mapping
/// lives until `MUNMAP`, `Close` of `handle`, process exit, or device reset
/// (after which touching the VA is the caller's fault — the KMD has unmapped it).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmMmap {
    pub head: HeliosNvrmHeader,
    /// in: backend handle (from `Open`) the mmap offset belongs to.
    pub handle: u32,
    /// in: `HELIOS_NVRM_PROT_*`.
    pub prot: u32,
    /// in: the RM mmap offset/cookie, passed to the host unchanged.
    pub offset: u64,
    /// in: bytes to map (page multiple, > 0). out: bytes actually mapped.
    pub size: u64,
    /// in: `HELIOS_NVRM_CACHE_*`.
    pub cache_request: u32,
    /// out: the cache type actually used (never `DEFAULT`).
    pub cache_effective: u32,
    /// out: user-mode virtual address of the mapping.
    pub out_user_va: u64,
    /// out: host mapping id; pass to `MUNMAP`.
    pub out_mapping_id: u32,
    /// in: zero. out: when `status` is `DEVICE_ERROR` and the host refused the
    /// mapping, the host's errno (positive); otherwise zero.
    pub flags: u32,
}

pub const HELIOS_NVRM_MMAP_BYTES: usize = 88;

/// `MUNMAP`. 48 bytes. Idempotent per id: a second call is `NOT_OWNED`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmMunmap {
    pub head: HeliosNvrmHeader,
    pub mapping_id: u32,
    pub flags: u32,
}

pub const HELIOS_NVRM_MUNMAP_BYTES: usize = 48;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The host sent `EventReady` for `handle`: that backend file (RM event queue,
/// UVM tools queue, …) became readable. This is what the Linux module turns
/// into a `poll()` wakeup.
pub const HELIOS_NVRM_EVENT_READY: u32 = 1;
/// The device was reset / the transport replaced / is being removed: every
/// handle of the earlier epoch is dead. `handle` is ignored (use 0). Wakes every
/// registration of this kind in the process so a blocked waiter can give up.
pub const HELIOS_NVRM_EVENT_TRANSPORT_LOST: u32 = 2;

/// `HeliosNvrmEvent.out_state`.
pub const HELIOS_NVRM_EVENT_STATE_REGISTERED: u32 = 1;
/// REGISTER: an existing registration for `(handle, kind)` in this process was
/// replaced by the new `event_handle`.
pub const HELIOS_NVRM_EVENT_STATE_REPLACED: u32 = 2;
/// REGISTER: a notification had been latched before the registration; the event
/// was signalled immediately (no lost wakeup).
pub const HELIOS_NVRM_EVENT_STATE_LATCHED_SIGNALED: u32 = 3;
/// UNREGISTER: removed.
pub const HELIOS_NVRM_EVENT_STATE_UNREGISTERED: u32 = 4;
/// UNREGISTER: no such registration.
pub const HELIOS_NVRM_EVENT_STATE_NOT_FOUND: u32 = 5;

/// `EVENT_REGISTER` / `EVENT_UNREGISTER`. 64 bytes, no trailing data.
///
/// `event_handle` is a usermode event `HANDLE` in the CALLING process (zero-
/// extended); the KMD resolves it with `ObReferenceObjectByHandle`
/// (`EVENT_MODIFY_STATE`, `UserMode`) so a bogus handle fails loudly and the
/// reference outlives the handle table entry. Either event type works.
///
/// **Persistent and level-triggered**, unlike the one-shot fence events: each
/// notification does `KeSetEvent`; the registration stays until UNREGISTER,
/// `Close` of `handle`, process exit, or reset. Consumers must therefore drain
/// the RM event source until empty after every wake (the same contract as
/// `poll()`), and reset a manual-reset event themselves.
///
/// **No lost wakeups.** The KMD latches a notification that arrives for a
/// `(handle, kind)` with no registration (and for a registration in the window
/// before the event is armed); REGISTER consumes the latch and signals at once
/// (`LATCHED_SIGNALED`). `EVENT_TRANSPORT_LOST` latches the same way.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmEvent {
    pub head: HeliosNvrmHeader,
    /// in: backend handle (owned by the caller), or 0 for `TRANSPORT_LOST`.
    pub handle: u32,
    /// in: `HELIOS_NVRM_EVENT_*`.
    pub kind: u32,
    /// in: usermode event HANDLE, zero-extended. Ignored by UNREGISTER.
    pub event_handle: u64,
    /// in: zero (reserved).
    pub flags: u32,
    /// out: `HELIOS_NVRM_EVENT_STATE_*`.
    pub out_state: u32,
}

pub const HELIOS_NVRM_EVENT_BYTES: usize = 64;

// ---------------------------------------------------------------------------
// PIN / UNPIN — memory registered by CPU address (NV01_MEMORY_SYSTEM_OS_DESCRIPTOR)
// ---------------------------------------------------------------------------

/// `deep_ptr_offset` values the host reads as "the deep block is a page-run
/// table" (`host/backend/protocol/src/pageruns.rs` `PAGE_RUNS` /
/// `PAGE_RUNS_INDIRECT`). **User mode must never send them** — `FORWARD` refuses
/// them (`HELIOS_NVRM_ST_FORBIDDEN`); only the KMD writes them, for a `pin_id`.
pub const HELIOS_NVRM_DEEP_PAGE_RUNS: u32 = 0xFFFF_FFFE;
pub const HELIOS_NVRM_DEEP_PAGE_RUNS_INDIRECT: u32 = 0xFFFF_FFFD;
/// Host run-count limit of a direct table.
pub const HELIOS_NVRM_PAGE_RUNS_MAX: u32 = 1024;
/// Bytes of a full direct table: `u32 runs, u32 reserved, runs × {u64 gpa, u64 len}`.
pub const HELIOS_NVRM_PAGE_RUNS_DIRECT_BYTES: u32 = 8 + HELIOS_NVRM_PAGE_RUNS_MAX * 16;

/// `PIN`. 80 bytes, no trailing data. Locks user pages for an OS-descriptor
/// registration (`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`) and returns an opaque id.
///
/// Why: RM registers memory by a CPU address and reads it in the caller's address
/// space, which for a guest is meaningless to the host. So the guest pins the
/// pages and names them by guest-physical run instead — what the Linux module
/// does in `nvgpu_ioctl_register_memory`. Here the KMD does ALL of it:
/// `MmProbeAndLockPages` on `[user_va, user_va + length)` (write access,
/// UserMode, in the caller's context), then builds the run table in the host's
/// format and keeps it (for a range scattering past `HELIOS_NVRM_PAGE_RUNS_MAX`
/// runs it keeps an INDIRECT table in its own nonpaged pages). The caller then
/// sends the registration ioctl as a `FORWARD` with `pin_id` set — the table
/// never crosses into user mode (see the SECURITY section).
///
/// **Whole pages only**: `user_va` and `length` must be page multiples
/// (`BAD_RANGE` otherwise) — the host refuses the same. A range needing more
/// runs than the KMD can describe is `TOO_SCATTERED`. Windows user memory
/// scatters badly, so expect INDIRECT tables beyond a few MiB.
///
/// Lifetime. A pin the registration `FORWARD` has not used may be `UNPIN`ned (a
/// failure path). Once a `FORWARD` has used it the GPU may hold the pages, so it
/// is released only by the KMD, when:
///
/// * the registration failed (the host's status or the word at `rm_status_off`
///   is nonzero), immediately;
/// * an `RM_FREE` (`NV_ESC_RM_FREE`, nr 0x29, the flat 16-byte `NVOS00`) succeeds
///   for `(h_root, h_object)`, or for `h_root` itself (`h_object == h_root` frees
///   the client and everything under it) — the one RM call the KMD recognises;
/// * `Close` of `handle`, process exit, or device reset.
///
/// `h_root` and `h_object` are the client and the memory object the registration
/// will create (RM_ALLOC_MEMORY's `hRoot` / `hObjectNew`, chosen by the caller).
/// The tags are the caller's word and the KMD does not verify them against the
/// registration, so they decide only WHEN the KMD releases the pin: a process that
/// mis-tags can have its own pages unlocked while the host still maps them
/// (hardening TODO: take the object handle from the registration itself).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmPin {
    pub head: HeliosNvrmHeader,
    /// in: backend handle (from `Open`) the registration will be made under;
    /// it bounds the pin's lifetime.
    pub handle: u32,
    /// in: zero (reserved).
    pub flags: u32,
    /// in: start of the range in the caller's address space (page aligned).
    pub user_va: u64,
    /// in: length in bytes (page multiple, > 0).
    pub length: u64,
    /// in: the RM client handle the registration will be made under.
    pub h_root: u32,
    /// in: the memory object handle the registration will create.
    pub h_object: u32,
    /// out: id for `FORWARD.pin_id` / `UNPIN`.
    pub out_pin_id: u32,
    /// out: pages locked.
    pub out_npages: u32,
}

pub const HELIOS_NVRM_PIN_BYTES: usize = 80;

/// `UNPIN`. 48 bytes. Releases the locked pages (and any indirect table) of a pin
/// that no `FORWARD` has used; `PIN_IN_USE` otherwise (see [`HeliosNvrmPin`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmUnpin {
    pub head: HeliosNvrmHeader,
    pub pin_id: u32,
    pub flags: u32,
}

pub const HELIOS_NVRM_UNPIN_BYTES: usize = 48;

// ---------------------------------------------------------------------------
// Layout assertions (mirrored by `_Static_assert`s in helios_nvrm_escape.h)
// ---------------------------------------------------------------------------

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosNvrmHeader>() == HELIOS_NVRM_HEADER_BYTES);
    assert!(offset_of!(HeliosNvrmHeader, abi_version) == 16);
    assert!(offset_of!(HeliosNvrmHeader, op) == 20);
    assert!(offset_of!(HeliosNvrmHeader, status) == 24);
    assert!(offset_of!(HeliosNvrmHeader, epoch) == 32);

    assert!(size_of::<HeliosNvrmQueryCaps>() == 88);
    assert!(offset_of!(HeliosNvrmQueryCaps, supported_ops) == 48);
    assert!(offset_of!(HeliosNvrmQueryCaps, pin_deep_kinds) == 84);

    assert!(size_of::<HeliosNvrmForward>() == HELIOS_NVRM_FORWARD_BYTES);
    assert!(offset_of!(HeliosNvrmForward, req_len) == 40);
    assert!(offset_of!(HeliosNvrmForward, resp_cap) == 44);
    assert!(offset_of!(HeliosNvrmForward, resp_len) == 48);
    assert!(offset_of!(HeliosNvrmForward, timeout_ms) == 52);
    assert!(offset_of!(HeliosNvrmForward, pin_id) == 56);
    assert!(offset_of!(HeliosNvrmForward, rm_status_off) == 60);

    assert!(size_of::<HeliosNvrmMmap>() == HELIOS_NVRM_MMAP_BYTES);
    assert!(offset_of!(HeliosNvrmMmap, handle) == 40);
    assert!(offset_of!(HeliosNvrmMmap, offset) == 48);
    assert!(offset_of!(HeliosNvrmMmap, size) == 56);
    assert!(offset_of!(HeliosNvrmMmap, cache_request) == 64);
    assert!(offset_of!(HeliosNvrmMmap, out_user_va) == 72);
    assert!(offset_of!(HeliosNvrmMmap, out_mapping_id) == 80);

    assert!(size_of::<HeliosNvrmMunmap>() == HELIOS_NVRM_MUNMAP_BYTES);
    assert!(offset_of!(HeliosNvrmMunmap, mapping_id) == 40);

    assert!(size_of::<HeliosNvrmEvent>() == HELIOS_NVRM_EVENT_BYTES);
    assert!(offset_of!(HeliosNvrmEvent, event_handle) == 48);
    assert!(offset_of!(HeliosNvrmEvent, out_state) == 60);

    assert!(size_of::<HeliosNvrmPin>() == HELIOS_NVRM_PIN_BYTES);
    assert!(offset_of!(HeliosNvrmPin, user_va) == 48);
    assert!(offset_of!(HeliosNvrmPin, length) == 56);
    assert!(offset_of!(HeliosNvrmPin, h_root) == 64);
    assert!(offset_of!(HeliosNvrmPin, h_object) == 68);
    assert!(offset_of!(HeliosNvrmPin, out_pin_id) == 72);
    assert!(offset_of!(HeliosNvrmPin, out_npages) == 76);

    assert!(size_of::<HeliosNvrmUnpin>() == HELIOS_NVRM_UNPIN_BYTES);
    assert!(offset_of!(HeliosNvrmUnpin, pin_id) == 40);
};

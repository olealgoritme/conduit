//! Private WDDM formats and submission arithmetic for native CUDA hardware queues.
//!
//! The 610.88 CUDA path is deliberately narrow: one context-private layout, one
//! queue-private layout, and user-mode GPFIFO submission.  Everything in this
//! module is argument-only so the byte offsets and wrap rules can be tested on
//! the host.  RM I/O and WDK handle resolution live in `kmd_render`.

use crate::ce_present::MAX_VA;
use crate::native_memory::GPU_ADDRESS_LIMIT;

pub const NVDA_MAGIC: u32 = 0x4e56_4441;
pub const NVDA_CONTEXT_VERSION: u32 = 0x0001_0000;
pub const CONTEXT_PRIVATE_BYTES: usize = 3200;
pub const QUEUE_PRIVATE_BYTES: usize = 180;
pub const SUBMIT_PRIVATE_BYTES: usize = 536;
/// The 610.88 D3D11 UMD's SubmitCommandToHwQueue record (NVDA header, size
/// 0x118 at +0x0c); its new GP_PUT sits at the same +0x78 as the CUDA record's.
pub const D3D_SUBMIT_PRIVATE_BYTES: usize = 280;

pub const CUDA_RING_BYTES: u32 = 0x2000;
/// The 610.88 D3D11 UMD's GPFIFO ring (8192 entries).
pub const D3D_RING_BYTES: u32 = 0x10000;
/// GPFIFO rings accepted from a queue record: a power of two from the CUDA
/// loader's 1024 entries up to 128Ki entries.
pub const MIN_RING_BYTES: u32 = CUDA_RING_BYTES;
pub const MAX_RING_BYTES: u32 = 0x10_0000;
pub const GP_ENTRY_BYTES: u32 = 8;
pub const CUDA_ENGINE_AFFINITY: u32 = 1;
pub const CUDA_CONTEXT_FLAGS: u32 = 0x14;
pub const CUDA_CLIENT_HINT: u32 = 5;

/// `D3DDDI_CREATEHWQUEUEFLAGS.UserModeSubmission` and `NativeProgressFence`.
pub const QUEUE_FLAG_USER_MODE_SUBMISSION: u32 = 0x10;
pub const QUEUE_FLAG_NATIVE_PROGRESS_FENCE: u32 = 0x20;
/// Flags whose semantics this implementation can preserve.  Unknown bits are
/// refused rather than silently changing queue behaviour.
pub const QUEUE_FLAGS_KNOWN: u32 = 0x1f;

const CONTEXT_DECLARED_SIZE_AT: usize = 0x08;
const CONTEXT_CLIENT_HINT_AT: usize = 0x0c;
const QUEUE_WORD_10_AT: usize = 0x10;
const QUEUE_GPFIFO_VA_AT: usize = 0x18;
const QUEUE_WORD_1C_AT: usize = 0x1c;
const QUEUE_RING_BYTES_AT: usize = 0x20;
const QUEUE_ALLOCATION_AT: usize = 0x24;
const QUEUE_TOKEN_AT: usize = 0x40;
const QUEUE_RUNLIST_AT: usize = 0x48;
const QUEUE_CHANNEL_AT: usize = 0x4c;
const QUEUE_SENTINEL_54_AT: usize = 0x54;
const QUEUE_SENTINEL_58_AT: usize = 0x58;
const QUEUE_ONE_60_AT: usize = 0x60;
const SUBMIT_NEW_GP_PUT_AT: usize = 0x78;

fn get32(buf: &[u8], at: usize) -> Option<u32> {
    let s = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn get64(buf: &[u8], at: usize) -> Option<u64> {
    let s = buf.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn put32(buf: &mut [u8], at: usize, value: u32) -> bool {
    let Some(end) = at.checked_add(4) else {
        return false;
    };
    let Some(s) = buf.get_mut(at..end) else {
        return false;
    };
    s.copy_from_slice(&value.to_le_bytes());
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateError {
    Size,
    Magic,
    Version,
    Node,
    EngineAffinity,
    Flags,
    ClientHint,
    GpfifoVa,
    RingBytes,
    Allocation,
    QueueFlags,
    GpPut,
    ProgressFence,
}

/// The validated WDDM values which accompany a native CUDA context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextPrivate {
    pub node_ordinal: u32,
}

/// Parse the 610.88 CreateContextVirtual private header and validate the DDI
/// values which dxgkrnl does not include in that header.  The remaining private
/// payload is opaque to the channel creator and is not copied or reinterpreted.
pub fn parse_context_private(
    buf: &[u8],
    node_ordinal: u32,
    engine_affinity: u32,
    flags: u32,
) -> Result<ContextPrivate, PrivateError> {
    if buf.len() != CONTEXT_PRIVATE_BYTES {
        return Err(PrivateError::Size);
    }
    if get32(buf, 0) != Some(NVDA_MAGIC) {
        return Err(PrivateError::Magic);
    }
    if get32(buf, 4) != Some(NVDA_CONTEXT_VERSION) {
        return Err(PrivateError::Version);
    }
    if node_ordinal != 0 && node_ordinal != 4 {
        return Err(PrivateError::Node);
    }
    if engine_affinity != CUDA_ENGINE_AFFINITY {
        return Err(PrivateError::EngineAffinity);
    }
    if flags != CUDA_CONTEXT_FLAGS {
        return Err(PrivateError::Flags);
    }
    if get32(buf, CONTEXT_DECLARED_SIZE_AT) != Some(CONTEXT_PRIVATE_BYTES as u32) {
        return Err(PrivateError::Size);
    }
    if get32(buf, CONTEXT_CLIENT_HINT_AT) != Some(CUDA_CLIENT_HINT) {
        return Err(PrivateError::ClientHint);
    }
    Ok(ContextPrivate { node_ordinal })
}

/// The NVIDIA personality's node count (NVIDIA's 610.88 KMD reports 16).
pub const HWS_NODE_COUNT: u32 = 16;

/// An NVDA-private `DxgkDdiCreateContext` in the NVIDIA personality (WDDM 2.7
/// hardware scheduling: dxgkrnl passes the UMD's CreateContextVirtual blob to
/// CreateContext, and later names that context as `hHwContext` of
/// CreateHwQueue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwsContextPrivate {
    pub node_ordinal: u32,
    /// Header word +0x0c: 5 for the CUDA loader; other UMDs (D3D11, Vulkan)
    /// are not pinned, only recorded.
    pub client_hint: u32,
    /// Whether this KMD can back the context with a native TSG today.
    pub native: bool,
}

/// Nodes whose contexts get a native graphics-runlist TSG today: 0 (3D) and
/// 4 (the copy node the 610.88 CUDA loader creates its context on). Every
/// other node's context is created without channel state and refuses queues.
pub const fn hws_native_node(node_ordinal: u32) -> bool {
    node_ordinal == 0 || node_ordinal == 4
}

/// Parse the NVDA header of a CreateContext in the NVIDIA personality. Unlike
/// [`parse_context_private`] (the CreateHwContext CUDA path) any client hint is
/// accepted and every reported node is legal; `kmd_flags` are the KMD-side
/// `DXGK_CREATECONTEXTFLAGS`, which must carry VirtualAddressing and
/// HwQueueSupported.
pub fn parse_hws_context_private(
    buf: &[u8],
    node_ordinal: u32,
    engine_affinity: u32,
    kmd_flags: u32,
) -> Result<HwsContextPrivate, PrivateError> {
    if buf.len() != CONTEXT_PRIVATE_BYTES {
        return Err(PrivateError::Size);
    }
    if get32(buf, 0) != Some(NVDA_MAGIC) {
        return Err(PrivateError::Magic);
    }
    if get32(buf, 4) != Some(NVDA_CONTEXT_VERSION) {
        return Err(PrivateError::Version);
    }
    if get32(buf, CONTEXT_DECLARED_SIZE_AT) != Some(CONTEXT_PRIVATE_BYTES as u32) {
        return Err(PrivateError::Size);
    }
    if node_ordinal >= HWS_NODE_COUNT {
        return Err(PrivateError::Node);
    }
    if engine_affinity != CUDA_ENGINE_AFFINITY {
        return Err(PrivateError::EngineAffinity);
    }
    if !crate::hws::is_hw_queue_context(kmd_flags) {
        return Err(PrivateError::Flags);
    }
    let client_hint = get32(buf, CONTEXT_CLIENT_HINT_AT).ok_or(PrivateError::Size)?;
    Ok(HwsContextPrivate {
        node_ordinal,
        client_hint,
        native: hws_native_node(node_ordinal),
    })
}

/// Recognise only the native format, allowing the existing non-NVDA context
/// path to keep its old behaviour without treating arbitrary private data as a
/// CUDA request.
pub fn is_context_private(buf: &[u8]) -> bool {
    buf.len() == CONTEXT_PRIVATE_BYTES && get32(buf, 0) == Some(NVDA_MAGIC)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuePrivate {
    /// Captured input words.  Their meanings are not guessed and they play no
    /// part in RM object construction.
    pub word_10: u32,
    pub word_1c: u32,
    pub gpfifo_va: u64,
    pub ring_bytes: u32,
    pub allocation: u32,
}

impl QueuePrivate {
    pub const fn entries(self) -> u32 {
        self.ring_bytes / GP_ENTRY_BYTES
    }
}

/// Parse a CUDA CreateHwQueue buffer.  A nonzero allocation is only an identity
/// here: the caller must still resolve it to a live RM-backed mapping before
/// creating any object.
pub fn parse_queue_private(buf: &[u8]) -> Result<QueuePrivate, PrivateError> {
    if buf.len() != QUEUE_PRIVATE_BYTES {
        return Err(PrivateError::Size);
    }
    let queue = QueuePrivate {
        word_10: get32(buf, QUEUE_WORD_10_AT).ok_or(PrivateError::Size)?,
        word_1c: get32(buf, QUEUE_WORD_1C_AT).ok_or(PrivateError::Size)?,
        gpfifo_va: get64(buf, QUEUE_GPFIFO_VA_AT).ok_or(PrivateError::Size)?,
        ring_bytes: get32(buf, QUEUE_RING_BYTES_AT).ok_or(PrivateError::Size)?,
        allocation: get32(buf, QUEUE_ALLOCATION_AT).ok_or(PrivateError::Size)?,
    };
    if !queue.ring_bytes.is_power_of_two()
        || !(MIN_RING_BYTES..=MAX_RING_BYTES).contains(&queue.ring_bytes)
    {
        return Err(PrivateError::RingBytes);
    }
    if queue.gpfifo_va == 0
        || queue.gpfifo_va % u64::from(GP_ENTRY_BYTES) != 0
        || queue
            .gpfifo_va
            .checked_add(u64::from(queue.ring_bytes))
            .is_none_or(|end| end > MAX_VA)
    {
        return Err(PrivateError::GpfifoVa);
    }
    if queue.allocation == 0 {
        return Err(PrivateError::Allocation);
    }
    Ok(queue)
}

pub fn validate_queue_flags(flags: u32) -> Result<(), PrivateError> {
    // UserModeSubmission is the CUDA loader's; 610.88's D3D11 queues carry none
    // of it (0, DisableGpuTimeout, NoBroadcastSignal/Wait in the captures) and
    // submit through SubmitCommandToHwQueue.
    if flags & !QUEUE_FLAGS_KNOWN != 0 || flags & QUEUE_FLAG_NATIVE_PROGRESS_FENCE != 0 {
        return Err(PrivateError::QueueFlags);
    }
    Ok(())
}

/// The OS supplies both views of monitored-fence storage at queue creation.
/// They are validated, never initialised or signalled by the CPU.
pub fn validate_progress_fence(cpu_va: u64, gpu_va: u64) -> Result<(), PrivateError> {
    if cpu_va == 0
        || gpu_va == 0
        || gpu_va % 8 != 0
        || gpu_va
            .checked_add(8)
            .is_none_or(|end| end > GPU_ADDRESS_LIMIT)
    {
        return Err(PrivateError::ProgressFence);
    }
    Ok(())
}

/// Encode only the capture-backed CreateHwQueue answers.  In particular +0x50
/// and +0x78 are left byte-for-byte untouched: no meaning is invented for them.
pub fn write_queue_created(
    buf: &mut [u8],
    work_submit_token: u32,
    runlist: u32,
    channel: u32,
) -> Result<(), PrivateError> {
    if buf.len() != QUEUE_PRIVATE_BYTES {
        return Err(PrivateError::Size);
    }
    let writes = [
        (QUEUE_TOKEN_AT, work_submit_token),
        (QUEUE_RUNLIST_AT, runlist),
        (QUEUE_CHANNEL_AT, channel),
        (QUEUE_SENTINEL_54_AT, u32::MAX),
        (QUEUE_SENTINEL_58_AT, u32::MAX),
        (QUEUE_ONE_60_AT, 1),
    ];
    for (at, value) in writes {
        if !put32(buf, at, value) {
            return Err(PrivateError::Size);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmitPrivate {
    pub new_gp_put: u32,
    pub entries_advanced: u32,
}

/// Advance a ring index in `0..=entries`, accepting the single wrap but never a
/// zero-entry submission or an out-of-range producer index.
pub const fn gp_put_advance(previous: u32, new: u32, entries: u32) -> Option<u32> {
    if entries == 0 || previous > entries || new > entries || new == previous {
        return None;
    }
    if new > previous {
        Some(new - previous)
    } else {
        // new < previous: publish through the last slot, then wrap to new.
        Some(entries - previous + new)
    }
}

pub fn parse_submit_private(
    buf: &[u8],
    previous_gp_put: u32,
    ring_bytes: u32,
) -> Result<SubmitPrivate, PrivateError> {
    if !matches!(buf.len(), SUBMIT_PRIVATE_BYTES | D3D_SUBMIT_PRIVATE_BYTES)
        || ring_bytes == 0
        || ring_bytes % GP_ENTRY_BYTES != 0
    {
        return Err(PrivateError::Size);
    }
    let new_gp_put = get32(buf, SUBMIT_NEW_GP_PUT_AT).ok_or(PrivateError::Size)?;
    let entries_advanced = gp_put_advance(previous_gp_put, new_gp_put, ring_bytes / GP_ENTRY_BYTES)
        .ok_or(PrivateError::GpPut)?;
    Ok(SubmitPrivate {
        new_gp_put,
        entries_advanced,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;

    fn put64(buf: &mut [u8], at: usize, value: u64) {
        buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn context() -> std::vec::Vec<u8> {
        let mut b = vec![0u8; CONTEXT_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&NVDA_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&NVDA_CONTEXT_VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&(CONTEXT_PRIVATE_BYTES as u32).to_le_bytes());
        b[12..16].copy_from_slice(&CUDA_CLIENT_HINT.to_le_bytes());
        b
    }

    fn queue() -> std::vec::Vec<u8> {
        let mut b = vec![0u8; QUEUE_PRIVATE_BYTES];
        b[0x10..0x14].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        put64(&mut b, 0x18, 0x20_1234_5000);
        b[0x20..0x24].copy_from_slice(&CUDA_RING_BYTES.to_le_bytes());
        b[0x24..0x28].copy_from_slice(&0x44u32.to_le_bytes());
        b
    }

    #[test]
    fn context_format_accepts_both_captured_nodes_and_rejects_every_changed_field() {
        for node in [0, 4] {
            assert_eq!(
                parse_context_private(&context(), node, 1, 0x14),
                Ok(ContextPrivate { node_ordinal: node })
            );
        }
        assert_eq!(
            parse_context_private(&context(), 1, 1, 0x14),
            Err(PrivateError::Node)
        );
        assert_eq!(
            parse_context_private(&context(), 0, 0, 0x14),
            Err(PrivateError::EngineAffinity)
        );
        assert_eq!(
            parse_context_private(&context(), 0, 1, 0),
            Err(PrivateError::Flags)
        );
        let mut b = context();
        b[12..16].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            parse_context_private(&b, 0, 1, 0x14),
            Err(PrivateError::ClientHint)
        );
        let mut b = context();
        b[0] ^= 1;
        assert_eq!(
            parse_context_private(&b, 0, 1, 0x14),
            Err(PrivateError::Magic)
        );
        assert_eq!(
            parse_context_private(&context()[..3199], 0, 1, 0x14),
            Err(PrivateError::Size)
        );
        assert!(is_context_private(&context()));
        assert!(!is_context_private(&context()[..3199]));
    }

    #[test]
    fn hws_context_accepts_any_client_and_all_sixteen_nodes() {
        let mut b = context();
        b[12..16].copy_from_slice(&2u32.to_le_bytes());
        for node in 0..HWS_NODE_COUNT {
            let p = parse_hws_context_private(&b, node, 1, 0x14).unwrap();
            assert_eq!(p.node_ordinal, node);
            assert_eq!(p.client_hint, 2);
            assert_eq!(p.native, node == 0 || node == 4);
        }
        assert_eq!(
            parse_hws_context_private(&b, 16, 1, 0x14),
            Err(PrivateError::Node)
        );
        assert_eq!(
            parse_hws_context_private(&b, 0, 2, 0x14),
            Err(PrivateError::EngineAffinity)
        );
        // KMD flags: VirtualAddressing (4) and HwQueueSupported (0x10) both needed.
        assert_eq!(
            parse_hws_context_private(&b, 0, 1, 0x10),
            Err(PrivateError::Flags)
        );
        assert!(parse_hws_context_private(&b, 0, 1, 0x34).is_ok());
        let mut bad = context();
        bad[4] ^= 1;
        assert_eq!(
            parse_hws_context_private(&bad, 0, 1, 0x14),
            Err(PrivateError::Version)
        );
        assert_eq!(
            parse_hws_context_private(&context()[..3199], 0, 1, 0x14),
            Err(PrivateError::Size)
        );
    }

    #[test]
    fn queue_format_uses_the_exact_umd_va_and_keeps_unknown_words_opaque() {
        let b = queue();
        let q = parse_queue_private(&b).unwrap();
        assert_eq!(q.gpfifo_va, 0x20_1234_5000);
        assert_eq!(q.ring_bytes, 0x2000);
        assert_eq!(q.entries(), 1024);
        assert_eq!(q.allocation, 0x44);
        assert_eq!((q.word_10, q.word_1c), (0x1234_5678, 0x20));

        let mut bad = queue();
        put64(&mut bad, 0x18, MAX_VA - 0x1000);
        assert_eq!(parse_queue_private(&bad), Err(PrivateError::GpfifoVa));
        let mut bad = queue();
        bad[0x20..0x24].copy_from_slice(&0x1000u32.to_le_bytes());
        assert_eq!(parse_queue_private(&bad), Err(PrivateError::RingBytes));
        let mut bad = queue();
        bad[0x20..0x24].copy_from_slice(&0x3000u32.to_le_bytes());
        assert_eq!(parse_queue_private(&bad), Err(PrivateError::RingBytes));
        let mut bad = queue();
        bad[0x20..0x24].copy_from_slice(&(MAX_RING_BYTES * 2).to_le_bytes());
        assert_eq!(parse_queue_private(&bad), Err(PrivateError::RingBytes));
        let mut bad = queue();
        bad[0x24..0x28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_queue_private(&bad), Err(PrivateError::Allocation));
    }

    #[test]
    fn submit_format_accepts_the_captured_d3d11_record() {
        let mut b = vec![0u8; D3D_SUBMIT_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&NVDA_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        b[0x0c..0x10].copy_from_slice(&0x118u32.to_le_bytes());
        b[0x14..0x18].copy_from_slice(&0x15u32.to_le_bytes());
        b[0x78..0x7c].copy_from_slice(&0x23u32.to_le_bytes());
        b[0x7c..0x80].copy_from_slice(&0x0400_0000u32.to_le_bytes());
        let submit = parse_submit_private(&b, 0x20, D3D_RING_BYTES).unwrap();
        assert_eq!((submit.new_gp_put, submit.entries_advanced), (0x23, 3));
        assert_eq!(
            parse_submit_private(&b[..279], 0x20, D3D_RING_BYTES),
            Err(PrivateError::Size)
        );
    }

    #[test]
    fn queue_format_accepts_the_captured_d3d11_record() {
        // 610.88 nvwgf2umx CreateHwQueue input (d3d11test capture).
        let mut b = vec![0u8; QUEUE_PRIVATE_BYTES];
        b[0..4].copy_from_slice(&NVDA_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        b[8..12].copy_from_slice(&0xb4u32.to_le_bytes());
        b[12..16].copy_from_slice(b"XDVN");
        b[0x10..0x14].copy_from_slice(&0x900u32.to_le_bytes());
        put64(&mut b, 0x18, 0x0400_0000);
        b[0x20..0x24].copy_from_slice(&D3D_RING_BYTES.to_le_bytes());
        b[0x24..0x28].copy_from_slice(&0x4000_0680u32.to_le_bytes());
        let q = parse_queue_private(&b).unwrap();
        assert_eq!(q.gpfifo_va, 0x0400_0000);
        assert_eq!(q.entries(), 8192);
        assert_eq!(q.allocation, 0x4000_0680);
    }

    #[test]
    fn queue_output_is_capture_backed_and_does_not_invent_50_or_78() {
        let mut b = queue();
        b[0x50..0x54].copy_from_slice(&0xaaaa_5555u32.to_le_bytes());
        b[0x78..0x7c].copy_from_slice(&0xbbbb_7777u32.to_le_bytes());
        write_queue_created(&mut b, 0x1234_0567, 0x12, 0x567).unwrap();
        assert_eq!(get32(&b, 0x40), Some(0x1234_0567));
        assert_eq!(get32(&b, 0x48), Some(0x12));
        assert_eq!(get32(&b, 0x4c), Some(0x567));
        assert_eq!(get32(&b, 0x54), Some(u32::MAX));
        assert_eq!(get32(&b, 0x58), Some(u32::MAX));
        assert_eq!(get32(&b, 0x60), Some(1));
        assert_eq!(get32(&b, 0x50), Some(0xaaaa_5555));
        assert_eq!(get32(&b, 0x78), Some(0xbbbb_7777));
    }

    #[test]
    fn submit_arithmetic_handles_forward_wrap_and_bad_indices() {
        assert_eq!(gp_put_advance(10, 12, 1024), Some(2));
        assert_eq!(gp_put_advance(1022, 2, 1024), Some(4));
        assert_eq!(gp_put_advance(1024, 1, 1024), Some(1));
        assert_eq!(gp_put_advance(5, 5, 1024), None);
        assert_eq!(gp_put_advance(5, 1025, 1024), None);
        assert_eq!(gp_put_advance(1025, 0, 1024), None);
        assert_eq!(gp_put_advance(0, 1, 0), None);

        let mut b = vec![0u8; SUBMIT_PRIVATE_BYTES];
        b[0x78..0x7c].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            parse_submit_private(&b, 1022, CUDA_RING_BYTES),
            Ok(SubmitPrivate {
                new_gp_put: 2,
                entries_advanced: 4,
            })
        );
        b[0x78..0x7c].copy_from_slice(&1025u32.to_le_bytes());
        assert_eq!(
            parse_submit_private(&b, 0, CUDA_RING_BYTES),
            Err(PrivateError::GpPut)
        );
        assert_eq!(
            parse_submit_private(&b[..535], 0, CUDA_RING_BYTES),
            Err(PrivateError::Size)
        );
    }

    #[test]
    fn queue_and_progress_flags_do_not_claim_unsupported_semantics() {
        assert_eq!(validate_queue_flags(0x10), Ok(()));
        for captured_d3d11 in [0, 1, 2, 6] {
            assert_eq!(validate_queue_flags(captured_d3d11), Ok(()));
        }
        assert_eq!(validate_queue_flags(0x30), Err(PrivateError::QueueFlags));
        assert_eq!(validate_progress_fence(0x1000, 0x2000), Ok(()));
        assert_eq!(
            validate_progress_fence(0, 0x2000),
            Err(PrivateError::ProgressFence)
        );
        assert_eq!(
            validate_progress_fence(0x1000, MAX_VA - 4),
            Err(PrivateError::ProgressFence)
        );
    }
}

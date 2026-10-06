//! Rules for `DxgkDdiBuildPagingBuffer`: which status the DDI may answer and how
//! a content transfer is bounded by what the driver knows of the allocation.
//!
//! # Legal statuses
//!
//! VidMm accepts exactly two answers from `BuildPagingBuffer`:
//! `STATUS_SUCCESS`, and `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (the DMA
//! buffer was too small; VidMm retries with a bigger one). Anything else is
//! "Driver returned an invalid error code from BuildPagingBuffer" and bugchecks
//! `VIDEO_MEMORY_MANAGEMENT_INTERNAL` (0x10E, parameter 1 = 0xB): measured with
//! `STATUS_INSUFFICIENT_RESOURCES` (0xC000009A). This driver never emits DMA for
//! a content op, so the only status it may ever return is `STATUS_SUCCESS`; a
//! content operation it cannot perform moves nothing and says so through a
//! counter, never through the status.
//!
//! # Bounding a transfer
//!
//! A virtual transfer's `TransferSizeInBytes` was measured at 0x1E10000 for an
//! allocation the driver had recorded as 0x1C20000. The bytes beyond what the
//! driver knows of the allocation are treated as padding: the known part is
//! moved, the rest is not, and the operation still succeeds.

/// `STATUS_SUCCESS`.
pub const STATUS_SUCCESS: i32 = 0;
/// `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (0xC01E0001).
pub const STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER: i32 = 0xC01E_0001u32 as i32;

/// Whether VidMm accepts `status` from `DxgkDdiBuildPagingBuffer`.
pub const fn is_legal_status(status: i32) -> bool {
    status == STATUS_SUCCESS || status == STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER
}

/// The status for a content operation the driver could not (or must not)
/// perform. Always legal; always `STATUS_SUCCESS`.
pub const CONTENT_SKIP_STATUS: i32 = STATUS_SUCCESS;

/// How much of a requested byte range the driver may move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    /// The whole request lies inside the known size.
    Full(u64),
    /// The request runs past the known size: move this many bytes (from the
    /// request's start) and treat the rest as padding.
    Clamped(u64),
    /// The request starts at or beyond the known size: move nothing.
    Nothing,
}

impl Clamp {
    /// Bytes to move.
    pub const fn len(self) -> u64 {
        match self {
            Clamp::Full(n) | Clamp::Clamped(n) => n,
            Clamp::Nothing => 0,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Bound `requested` bytes at `offset` by the `known_size` of the allocation.
pub const fn clamp_range(known_size: u64, offset: u64, requested: u64) -> Clamp {
    match offset.checked_add(requested) {
        Some(end) if end <= known_size => Clamp::Full(requested),
        _ if offset >= known_size => Clamp::Nothing,
        _ => Clamp::Clamped(known_size - offset),
    }
}

/// Row-count alignment for an external LINEAR image, in rows. EMPIRICAL (the
/// NVIDIA external-linear requirement rounds rows up to GOB granularity; 128 is
/// what the measurements produced).
pub const NV_LINEAR_ROW_ALIGN: u64 = 128;

/// Opaque tail slack an external LINEAR image requires beyond the padded rows.
/// Equally empirical.
pub const NV_LINEAR_TAIL_SLACK: u64 = 64 * 1024;

/// Size a blob that will be imported as an external LINEAR VkImage must have:
/// `pitch * align(height, 128) + 64 KiB`, never below one page. Deliberately
/// LARGER than `pitch * height`.
pub const fn linear_blob_size(pitch: u64, height: u64) -> u64 {
    let padded_rows = height.saturating_add(NV_LINEAR_ROW_ALIGN - 1) & !(NV_LINEAR_ROW_ALIGN - 1);
    let size = pitch
        .saturating_mul(padded_rows)
        .saturating_add(NV_LINEAR_TAIL_SLACK);
    if size < 4096 {
        4096
    } else {
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_success_and_insufficient_dma_are_legal() {
        assert!(is_legal_status(STATUS_SUCCESS));
        assert!(is_legal_status(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER));
        // STATUS_INSUFFICIENT_RESOURCES: the status measured to bugcheck 0x10E/0xB.
        assert!(!is_legal_status(0xC000_009Au32 as i32));
        // STATUS_UNSUCCESSFUL, STATUS_NO_MEMORY, STATUS_INVALID_PARAMETER.
        assert!(!is_legal_status(0xC000_0001u32 as i32));
        assert!(!is_legal_status(0xC000_0017u32 as i32));
        assert!(!is_legal_status(0xC000_000Du32 as i32));
    }

    #[test]
    fn a_skipped_content_op_answers_a_legal_status() {
        assert!(is_legal_status(CONTENT_SKIP_STATUS));
    }

    #[test]
    fn clamp_inside_is_full() {
        assert_eq!(clamp_range(100, 0, 100), Clamp::Full(100));
        assert_eq!(clamp_range(100, 40, 60), Clamp::Full(60));
        assert_eq!(clamp_range(100, 100, 0), Clamp::Full(0));
        assert_eq!(clamp_range(100, 0, 0), Clamp::Full(0));
    }

    #[test]
    fn clamp_overrun_keeps_the_known_prefix() {
        assert_eq!(clamp_range(100, 0, 101), Clamp::Clamped(100));
        assert_eq!(clamp_range(100, 40, 61), Clamp::Clamped(60));
        assert_eq!(clamp_range(100, 99, 5), Clamp::Clamped(1));
    }

    #[test]
    fn clamp_beyond_the_allocation_moves_nothing() {
        assert_eq!(clamp_range(100, 100, 1), Clamp::Nothing);
        assert_eq!(clamp_range(100, 500, 10), Clamp::Nothing);
        assert_eq!(clamp_range(100, 101, 0), Clamp::Nothing);
        assert_eq!(clamp_range(0, 0, 1), Clamp::Nothing);
        assert!(clamp_range(100, 500, 10).is_empty());
    }

    #[test]
    fn clamp_survives_overflow() {
        assert_eq!(clamp_range(100, 10, u64::MAX), Clamp::Clamped(90));
        assert_eq!(clamp_range(100, u64::MAX, 2), Clamp::Nothing);
    }

    #[test]
    fn the_measured_5120x1440_transfer_is_clamped_not_refused() {
        // Recorded size 0x1C20000 (5120 x 1440 x 4), VidMm's VIRTUAL_TRANSFER
        // asked for 0x1E10000 at offset 0.
        let c = clamp_range(0x1C2_0000, 0, 0x1E1_0000);
        assert_eq!(c, Clamp::Clamped(0x1C2_0000));
        assert_eq!(c.len(), 29_491_200);
    }

    #[test]
    fn linear_blob_size_5120x1440_vector() {
        // Pitch 20480 (5120 x 4). Tight size is 0x1C20000; the guess pads the
        // 1440 rows to 1536 and adds 64 KiB: 0x1E10000, the number VidMm's
        // transfer carried.
        assert_eq!(20480u64 * 1440, 0x1C2_0000);
        assert_eq!(linear_blob_size(20480, 1440), 0x1E1_0000);
        assert_eq!(linear_blob_size(20480, 1440), 20480 * 1536 + 0x1_0000);
    }

    #[test]
    fn linear_blob_size_measured_vector_and_floor() {
        // 1024x1872 measured 7864320 = pitch * align(1872, 128); the guess is
        // deliberately larger (tail slack).
        assert_eq!(4096 * 1920, 7_864_320);
        assert_eq!(linear_blob_size(4096, 1872), 7_864_320 + 0x1_0000);
        assert!(linear_blob_size(0, 0) >= 4096);
        assert_eq!(linear_blob_size(u64::MAX, u64::MAX), u64::MAX);
    }
}

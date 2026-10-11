//! `EventReady` messages in an event-queue buffer (host `MsgType::EventReady`, 8).
//!
//! Each is a bare 16-byte `MsgHeader { msg_type = 8, handle, status, pad }`: "the
//! backend file `handle` became readable" (a fence's error in `status`). A host
//! that sends them one per buffer fills a buffer, raises an interrupt and costs a
//! DPC per report. One GPU non-stall interrupt makes every NVK device's event file
//! readable at once, so under a game that was tens of thousands of buffers a second
//! through a 16-buffer ring.
//!
//! A guest that acks the device feature [`FEATURE`] (`NVGPU_F_EVENT_PACKED`) gets
//! them packed: the host puts the reports of one pass of its event thread into as
//! few buffers as fit, whole headers back to back, the used length a multiple of 16,
//! at most [`PER_BUFFER_MAX`] per buffer and nothing but `EventReady` in such a
//! buffer. [`count`] says how many a filled buffer holds, [`entry`] reads one.

/// The device feature bit (`NVGPU_F_EVENT_PACKED`, virtio feature 22).
pub const FEATURE: u64 = 1 << 22;
/// Host `MsgType::EventReady`.
pub const MSG_EVENT_READY: u32 = 8;
/// Bytes of one message.
pub const MSG_BYTES: usize = 16;
/// The most messages the host packs into one buffer.
pub const PER_BUFFER_MAX: usize = 32;

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
}

/// How many `EventReady` messages the filled buffer `bytes` holds, of which the
/// host wrote `len` bytes. Without the feature (`packed` false) only the first
/// message is read, as a host that does not pack sends one per buffer. Packed,
/// the messages are counted from the start while they are whole and say
/// `EventReady`, at most [`PER_BUFFER_MAX`]; a buffer whose first message is
/// something else counts 0 (the caller parses it as that message).
pub fn count(bytes: &[u8], len: usize, packed: bool) -> usize {
    let len = len.min(bytes.len());
    let most = if packed {
        (len / MSG_BYTES).min(PER_BUFFER_MAX)
    } else {
        usize::from(len >= MSG_BYTES)
    };
    (0..most)
        .take_while(|i| word(bytes, i * MSG_BYTES) == Some(MSG_EVENT_READY))
        .count()
}

/// Message `i` of the buffer: (handle, status). `None` past the bytes.
pub fn entry(bytes: &[u8], i: usize) -> Option<(u32, i32)> {
    let at = i.checked_mul(MSG_BYTES)?;
    if word(bytes, at)? != MSG_EVENT_READY {
        return None;
    }
    Some((word(bytes, at + 4)?, word(bytes, at + 8)? as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(msgs: &[(u32, u32, i32)]) -> [u8; 512] {
        let mut b = [0u8; 512];
        for (i, &(ty, h, st)) in msgs.iter().enumerate() {
            let at = i * MSG_BYTES;
            b[at..at + 4].copy_from_slice(&ty.to_le_bytes());
            b[at + 4..at + 8].copy_from_slice(&h.to_le_bytes());
            b[at + 8..at + 12].copy_from_slice(&st.to_le_bytes());
        }
        b
    }

    #[test]
    fn a_packed_buffer_counts_its_whole_messages() {
        let b = buf(&[(8, 1, 0), (8, 2, -110), (8, 3, 0)]);
        assert_eq!(count(&b, 48, true), 3);
        assert_eq!(entry(&b, 1), Some((2, -110)));
        // A torn tail is not a message.
        assert_eq!(count(&b, 40, true), 2);
        // The used length bounds the count, not the buffer's size.
        assert_eq!(count(&b, 16, true), 1);
        assert_eq!(count(&b, 15, true), 0);
    }

    #[test]
    fn without_the_feature_only_the_first_message_is_read() {
        let b = buf(&[(8, 1, 0), (8, 2, 0)]);
        assert_eq!(count(&b, 32, false), 1);
        assert_eq!(count(&b, 16, false), 1);
        assert_eq!(count(&b, 8, false), 0);
    }

    #[test]
    fn counting_stops_at_anything_but_event_ready() {
        let b = buf(&[(8, 1, 0), (28, 2, 0), (8, 3, 0)]);
        assert_eq!(count(&b, 48, true), 1);
        assert_eq!(entry(&b, 1), None);
        // A buffer of another message counts 0: the caller parses it as that.
        let r = buf(&[(34, 0, 0)]);
        assert_eq!(count(&r, 64, true), 0);
    }

    #[test]
    fn at_most_the_cap_is_read() {
        let msgs = [(8u32, 7u32, 0i32); 32];
        let b = buf(&msgs);
        assert_eq!(count(&b, 512, true), PER_BUFFER_MAX);
        let mut big = [0u8; 1024];
        big[..512].copy_from_slice(&b);
        big[512..528].copy_from_slice(&b[..16]);
        assert_eq!(count(&big, 528, true), PER_BUFFER_MAX);
        assert_eq!(entry(&big, 40), None);
    }
}

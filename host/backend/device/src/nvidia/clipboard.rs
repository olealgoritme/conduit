//! ClipboardToHost (docs/CLIPBOARD.md): the guest's clipboard arrives in
//! chunks on the control queue, is reassembled here and, once whole and
//! valid UTF-8, handed to the display link, which sends it to the viewer.
//! The viewer decides whether it may reach the host clipboard.

use super::*;
use protocol::messages::{CLIPBOARD_CHUNK_HEAD, CLIPBOARD_MAX_BYTES, ClipboardChunk};

/// One guest -> host transfer being reassembled.
#[derive(Default)]
pub(super) struct ClipIn {
    generation: u64,
    total: usize,
    buf: Vec<u8>,
    active: bool,
}

/// What one chunk did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClipStep {
    /// Accepted; more to come.
    More,
    /// The transfer is whole.
    Done(Vec<u8>),
    /// Refused with this errno; any partial transfer is abandoned.
    Err(i32),
}

impl ClipIn {
    fn abandon(&mut self) {
        *self = Self::default();
    }

    /// Feed one chunk (`c` and exactly its `data`).
    pub(super) fn feed(&mut self, c: &ClipboardChunk, data: &[u8]) -> ClipStep {
        if !c.is_text() {
            self.abandon();
            return ClipStep::Err(libc::EOPNOTSUPP);
        }
        if c.total_len > CLIPBOARD_MAX_BYTES {
            self.abandon();
            return ClipStep::Err(libc::EMSGSIZE);
        }
        let total = c.total_len as usize;
        let (off, len) = (c.offset as usize, c.len as usize);
        if c.generation == 0 || total == 0 || len == 0 || len != data.len() || off + len > total {
            self.abandon();
            return ClipStep::Err(libc::EINVAL);
        }
        if off == 0 {
            // A new transfer; whatever was in progress is abandoned.
            self.generation = c.generation;
            self.total = total;
            self.buf = Vec::with_capacity(total);
            self.active = true;
        } else if !self.active
            || c.generation != self.generation
            || total != self.total
            || off != self.buf.len()
        {
            self.abandon();
            return ClipStep::Err(libc::EINVAL);
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() < self.total {
            return ClipStep::More;
        }
        let text = std::mem::take(&mut self.buf);
        self.abandon();
        if std::str::from_utf8(&text).is_err() {
            return ClipStep::Err(libc::EINVAL);
        }
        ClipStep::Done(text)
    }
}

impl NvidiaBackend {
    pub(super) fn handle_clipboard_to_host(
        &mut self,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let Some(c) = ClipboardChunk::from_bytes(payload) else {
            return self.write_hdr(resp_buf, 0, -libc::EINVAL);
        };
        let data = &payload[CLIPBOARD_CHUNK_HEAD..];
        // The message may be padded; the chunk says how much is data.
        if (c.len as usize) > data.len() {
            self.clip_in = ClipIn::default();
            return self.write_hdr(resp_buf, 0, -libc::EINVAL);
        }
        let data = &data[..c.len as usize];
        let Some(link) = self.display.clone() else {
            return self.write_hdr(resp_buf, 0, -libc::ENODEV);
        };
        match self.clip_in.feed(&c, data) {
            ClipStep::More => self.write_hdr(resp_buf, 0, 0),
            ClipStep::Done(text) => {
                log::debug!("clipboard: guest copied {} bytes", text.len());
                link.clipboard_to_host(text);
                self.write_hdr(resp_buf, 0, 0)
            }
            ClipStep::Err(e) => {
                log::debug!("clipboard: guest chunk refused: errno {e}");
                self.write_hdr(resp_buf, 0, -e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{DisplayLink, wire};
    use protocol::messages::{
        CLIPBOARD_MIME_TEXT, MsgType, clipboard_mime, encode_clipboard_chunk,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn chunk(generation: u64, total: u32, offset: u32, data: &[u8]) -> Vec<u8> {
        chunk_mime(generation, total, offset, data, CLIPBOARD_MIME_TEXT)
    }

    fn chunk_mime(generation: u64, total: u32, offset: u32, data: &[u8], mime: &str) -> Vec<u8> {
        let c = ClipboardChunk {
            generation,
            total_len: total,
            offset,
            len: data.len() as u32,
            flags: 0,
            mime: clipboard_mime(mime),
        };
        let mut out = vec![0u8; 16 + 56 + data.len()];
        encode_clipboard_chunk(MsgType::ClipboardToHost, &c, data, &mut out).unwrap();
        out
    }

    fn status(resp: &[u8]) -> i32 {
        i32::from_le_bytes(resp[8..12].try_into().unwrap())
    }

    fn socketpair() -> (OwnedFd, OwnedFd) {
        let mut sv = [0i32; 2];
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) }
    }

    /// Read whole CMD_CLIPBOARD records until LAST; returns the text.
    fn broker_text(fd: i32) -> Vec<u8> {
        let mut text = Vec::new();
        loop {
            let mut rec = [0u8; wire::CMD_SIZE];
            let n =
                unsafe { libc::recv(fd, rec.as_mut_ptr().cast(), rec.len(), libc::MSG_WAITALL) };
            assert_eq!(n, wire::CMD_SIZE as isize);
            let ty = u16::from_le_bytes([rec[0], rec[1]]);
            if ty != wire::CMD_CLIPBOARD {
                continue;
            }
            let info = rec[12];
            text.extend_from_slice(&rec[13..13 + (info & 0x1f) as usize]);
            if info & wire::CLIP_LAST != 0 {
                return text;
            }
        }
    }

    #[test]
    fn a_chunked_copy_reaches_the_viewer() {
        let mut be = NvidiaBackend::for_test();
        let (ours, broker) = socketpair();
        let link = DisplayLink::new(None);
        link.adopt(ours);
        be.set_display(link.clone());
        let mut resp = [0u8; 64];

        // Before HELLO it is queued, not sent; HELLO sends CAPS then it.
        assert_eq!(be.dispatch(&chunk(1, 6, 0, b"hel"), &mut resp), 16);
        assert_eq!(status(&resp), 0);
        be.dispatch(&chunk(1, 6, 3, b"lo!"), &mut resp);
        assert_eq!(status(&resp), 0);
        link.hello_for_test(wire::CAP_CLIP_LARGE);
        link.retry_clip_for_test();
        assert_eq!(broker_text(broker.as_raw_fd()), b"hello!");
    }

    #[test]
    fn malformed_chunks_are_refused() {
        let mut be = NvidiaBackend::for_test();
        let mut resp = [0u8; 64];
        // No display.
        be.dispatch(&chunk(1, 3, 0, b"abc"), &mut resp);
        assert_eq!(status(&resp), -libc::ENODEV);
        be.set_display(DisplayLink::new(None));
        // Short.
        be.dispatch(&chunk(1, 3, 0, b"abc")[..40], &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        // Wrong type.
        be.dispatch(&chunk_mime(1, 3, 0, b"abc", "image/png"), &mut resp);
        assert_eq!(status(&resp), -libc::EOPNOTSUPP);
        // Too big.
        be.dispatch(&chunk(1, CLIPBOARD_MAX_BYTES + 1, 0, b"abc"), &mut resp);
        assert_eq!(status(&resp), -libc::EMSGSIZE);
        // A continuation without a start, a gap, another generation.
        be.dispatch(&chunk(1, 6, 3, b"abc"), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        be.dispatch(&chunk(2, 9, 0, b"abc"), &mut resp);
        assert_eq!(status(&resp), 0);
        be.dispatch(&chunk(2, 9, 4, b"abc"), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        be.dispatch(&chunk(3, 9, 0, b"abc"), &mut resp);
        be.dispatch(&chunk(4, 9, 3, b"abc"), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        // Not UTF-8: refused on the last chunk.
        be.dispatch(&chunk(5, 2, 0, &[0xc3, 0x28]), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        // Overflowing its own total.
        be.dispatch(&chunk(6, 2, 0, b"abc"), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
    }

    #[test]
    fn offset_zero_restarts_a_transfer() {
        let mut c = ClipIn::default();
        let mk = |g, t, o, d: &[u8]| ClipboardChunk {
            generation: g,
            total_len: t,
            offset: o,
            len: d.len() as u32,
            flags: 0,
            mime: clipboard_mime(CLIPBOARD_MIME_TEXT),
        };
        assert_eq!(c.feed(&mk(1, 6, 0, b"old"), b"old"), ClipStep::More);
        assert_eq!(
            c.feed(&mk(2, 3, 0, b"new"), b"new"),
            ClipStep::Done(b"new".to_vec())
        );
        assert_eq!(
            c.feed(&mk(1, 6, 3, b"old"), b"old"),
            ClipStep::Err(libc::EINVAL)
        );
    }
}

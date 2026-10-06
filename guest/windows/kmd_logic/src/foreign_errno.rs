//! The errno the host reports for an RM-export blob create.
//!
//! For `RESOURCE_CREATE_BLOB` with `blob_mem = 0x80000001` only, the host puts a
//! Linux errno in the 3 padding bytes of the 24-byte response header (bytes 21..24,
//! 24-bit little endian; 0 when there is none). docs/VENUS.md "RM-export blobs".

/// Bytes of a `VirtioGpuCtrlHdr` response.
pub const RESP_HDR_BYTES: usize = 24;

pub const EBADF: u32 = 9;
pub const ENOENT: u32 = 2;
pub const EIO: u32 = 5;
pub const ENOMEM: u32 = 12;
pub const EINVAL: u32 = 22;
pub const ERANGE: u32 = 34;
pub const EOPNOTSUPP: u32 = 95;
/// An older backend answers a message type it does not know with `-EPROTO`
/// (`RmResourceImport`, MsgType 31).
pub const EPROTO: u32 = 71;

/// The errno of a response header, or 0 if the response is too short.
pub fn from_resp_hdr(resp: &[u8]) -> u32 {
    match resp.get(21..RESP_HDR_BYTES) {
        Some([a, b, c]) => u32::from(*a) | (u32::from(*b) << 8) | (u32::from(*c) << 16),
        _ => 0,
    }
}

/// What the KMD tells its caller for a host errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// `rm_handle` is not a render node this guest has open, or the GEM handle does
    /// not exist in it: one answer, as for the NVRM handles.
    NotOwned,
    /// The size is bigger than the object, or the request is malformed.
    BadRange,
    /// The host does not serve the import (`EOPNOTSUPP`, or `EPROTO` from a backend
    /// that predates the message).
    Unsupported,
    /// The host or the renderer is out of memory.
    NoResources,
    /// Anything else (the renderer refused, unknown errno, none given).
    Device,
}

pub fn classify(errno: u32) -> Verdict {
    match errno {
        EBADF | ENOENT => Verdict::NotOwned,
        ERANGE | EINVAL => Verdict::BadRange,
        EOPNOTSUPP | EPROTO => Verdict::Unsupported,
        ENOMEM => Verdict::NoResources,
        _ => Verdict::Device,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(e: u32) -> [u8; 24] {
        let mut h = [0u8; 24];
        h[0] = 0x00; // type
        h[21] = e as u8;
        h[22] = (e >> 8) as u8;
        h[23] = (e >> 16) as u8;
        h
    }

    #[test]
    fn errno_roundtrips_in_the_padding() {
        for e in [0, 1, 9, 34, 95, 0x1234, 0xFF_FFFF] {
            assert_eq!(from_resp_hdr(&hdr(e)), e);
        }
    }

    #[test]
    fn short_response_has_no_errno() {
        assert_eq!(from_resp_hdr(&[0u8; 20]), 0);
        assert_eq!(from_resp_hdr(&[]), 0);
        // a longer response (extra payload after the header) still reads the header's
        let mut long = [0u8; 40];
        long[21] = 9;
        assert_eq!(from_resp_hdr(&long), 9);
    }

    #[test]
    fn host_errnos_map_as_documented() {
        assert_eq!(classify(EBADF), Verdict::NotOwned);
        assert_eq!(classify(ENOENT), Verdict::NotOwned);
        assert_eq!(classify(ERANGE), Verdict::BadRange);
        assert_eq!(classify(EINVAL), Verdict::BadRange);
        assert_eq!(classify(EOPNOTSUPP), Verdict::Unsupported);
        assert_eq!(classify(EPROTO), Verdict::Unsupported);
        assert_eq!(classify(ENOMEM), Verdict::NoResources);
        assert_eq!(classify(EIO), Verdict::Device);
        assert_eq!(classify(0), Verdict::Device);
        assert_eq!(classify(0xABCDE), Verdict::Device);
    }
}

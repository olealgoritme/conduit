//! The host RM's version string, published for the installer.
//!
//! NVIDIA's own Windows user-mode drivers (the opt-in NVIDIA UMD mode of
//! `packaging/windows/Set-HeliosUmd.ps1`) have to come from the same NVIDIA
//! driver branch as the host's RM. The KMD asks the host RM for its version
//! (`NV_ESC_CHECK_VERSION_STR`, `QUERY`) once per transport generation and writes
//! it as the `REG_SZ` value [`VALUE_NAME`] in the service key
//! (`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`), e.g.
//! `610.57.04`. The installer reads it there; it never trusts anything else the
//! value could hold, so only a short run of digits and dots is published.

use crate::rm_client::VERSION_STR_BYTES;

/// The service-key value the KMD writes (REG_SZ).
pub const VALUE_NAME: &str = "NvHostRmVersion";

/// Longest version text published (NVIDIA's are like `610.57.04`).
pub const MAX_TEXT: usize = 32;

/// Probes of one transport generation before the KMD stops asking.
pub const MAX_TRIES: u8 = 3;

/// The publishable text of a `QUERY` reply's NUL-padded version string: the bytes
/// up to the first NUL, when they are 1..=[`MAX_TEXT`] ASCII digits and dots, start
/// with a digit, and hold at least one dot. Anything else is not published.
pub fn text(version: &[u8; VERSION_STR_BYTES]) -> Option<&[u8]> {
    let end = version
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(VERSION_STR_BYTES);
    let s = &version[..end];
    if s.is_empty() || s.len() > MAX_TEXT || !s[0].is_ascii_digit() {
        return None;
    }
    if !s.iter().all(|b| b.is_ascii_digit() || *b == b'.') || !s.contains(&b'.') {
        return None;
    }
    Some(s)
}

/// `text` as a NUL-terminated UTF-16 string in `out`; returns the byte length a
/// `REG_SZ` write takes (terminator included), or `None` when `text` does not fit.
pub fn utf16z(text: &[u8], out: &mut [u16; MAX_TEXT + 1]) -> Option<u32> {
    if text.len() > MAX_TEXT {
        return None;
    }
    for (slot, b) in out.iter_mut().zip(text.iter()) {
        *slot = u16::from(*b);
    }
    out[text.len()] = 0;
    Some(((text.len() + 1) * 2) as u32)
}

/// Whether the version should be asked for now: once per transport generation,
/// with at most [`MAX_TRIES`] attempts when the host does not answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Probe {
    epoch: u64,
    tries: u8,
    done: bool,
}

impl Probe {
    pub const fn new() -> Self {
        Self {
            epoch: 0,
            tries: 0,
            done: false,
        }
    }

    /// Start an attempt for `epoch` (0 = no transport: never). Returns whether
    /// the caller should ask the host now; a new generation starts over.
    pub fn begin(&mut self, epoch: u64) -> bool {
        if epoch == 0 {
            return false;
        }
        if self.epoch != epoch {
            *self = Self {
                epoch,
                tries: 0,
                done: false,
            };
        }
        if self.done || self.tries >= MAX_TRIES {
            return false;
        }
        self.tries += 1;
        true
    }

    /// The attempt for `epoch` published the version (`ok`) or failed. A
    /// result for an older generation changes nothing.
    pub fn finish(&mut self, epoch: u64, ok: bool) {
        if self.epoch == epoch && ok {
            self.done = true;
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(s: &[u8]) -> [u8; VERSION_STR_BYTES] {
        let mut v = [0u8; VERSION_STR_BYTES];
        v[..s.len()].copy_from_slice(s);
        v
    }

    #[test]
    fn a_driver_version_is_published_up_to_its_nul() {
        assert_eq!(text(&padded(b"610.57.04")), Some(&b"610.57.04"[..]));
        assert_eq!(text(&padded(b"615.10")), Some(&b"615.10"[..]));
    }

    #[test]
    fn anything_but_digits_and_dots_is_not_published() {
        assert_eq!(text(&padded(b"")), None);
        assert_eq!(text(&padded(b"610")), None);
        assert_eq!(text(&padded(b".610.57")), None);
        assert_eq!(text(&padded(b"610.57 beta")), None);
        assert_eq!(text(&padded(b"610.57\\..\\x")), None);
        assert_eq!(text(&[b'6'; VERSION_STR_BYTES]), None);
        let mut long = [b'1'; VERSION_STR_BYTES];
        long[1] = b'.';
        long[MAX_TEXT + 1] = 0;
        assert_eq!(text(&long), None);
        long[MAX_TEXT] = 0;
        assert_eq!(text(&long).map(<[u8]>::len), Some(MAX_TEXT));
    }

    #[test]
    fn the_reg_sz_length_counts_the_terminator() {
        let mut out = [0xffffu16; MAX_TEXT + 1];
        assert_eq!(utf16z(b"610.57.04", &mut out), Some(20));
        assert_eq!(out[..10], [54, 49, 48, 46, 53, 55, 46, 48, 52, 0]);
        assert_eq!(utf16z(&[b'1'; MAX_TEXT + 1], &mut out), None);
    }

    #[test]
    fn a_generation_is_asked_until_it_answers_or_runs_out_of_tries() {
        let mut p = Probe::new();
        assert!(!p.begin(0));
        assert!(p.begin(7));
        p.finish(7, false);
        assert!(p.begin(7));
        p.finish(7, false);
        assert!(p.begin(7));
        p.finish(7, false);
        assert!(!p.begin(7), "three tries per generation");
        // A new transport generation asks again.
        assert!(p.begin(8));
        p.finish(8, true);
        assert!(p.done());
        assert!(!p.begin(8));
    }

    #[test]
    fn a_late_result_of_an_older_generation_changes_nothing() {
        let mut p = Probe::new();
        assert!(p.begin(1));
        assert!(p.begin(2));
        p.finish(1, true);
        assert!(!p.done());
        assert!(p.begin(2));
    }
}

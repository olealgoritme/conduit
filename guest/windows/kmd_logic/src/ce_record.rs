//! The copy-engine Present record (`'HEF3'`, `protocol/src/rm_fence_v3.rs`) as the KMD reads it in
//! M3c-0: parsed beside a present marker's RM fence tail, counted, kept next to the fence, and NOT
//! acted on (`docs/rm-copy-engine-present.md` section 13). The parse itself is the protocol's
//! (`HeliosRmFenceTailV3::validate`, `matches_fence`); `kmd_logic` has no dependencies, so this
//! module holds what is pure and host-testable around it: the reason codes, the outcome rule, the
//! publish rule and the counter names. The I/O half is `kmd_render/src/ddi/ce_record.rs`.
//!
//! The capability that tells a producer to send the record, `QUERY_CAPS.supported_ops` bit 37
//! (`HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3`), is set unconditionally by this build: the KMD parses the
//! record whatever `RmCopyEngine` says. A test below pins the bit in the protocol source and in
//! the escape's mask.

/// The `supported_ops` bit this build sets because it parses the record.
pub const CAP_BIT: u32 = 37;

/// Why a record was refused (`CeRecWhy`; `CeRecMask` has bit `code - 1`). Codes 1..=14 are the
/// protocol's `TailV3Error` variants in declaration order (a test reads that enum's source),
/// 15 is a valid record whose semaphore value is not the fence's (`matches_fence` false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Why {
    BadMagic = 1,
    Short = 2,
    Version = 3,
    Flags = 4,
    Incomplete = 5,
    Reserved = 6,
    Handle = 7,
    SemaphoreOffset = 8,
    Value = 9,
    Dimensions = 10,
    Format = 11,
    Pitch = 12,
    Modifier = 13,
    Size = 14,
    FenceValue = 15,
}

/// The names of [`Why`] in code order (`WHY_NAMES[code - 1]`).
pub const WHY_NAMES: [&str; 15] = [
    "BadMagic",
    "Short",
    "Version",
    "Flags",
    "Incomplete",
    "Reserved",
    "Handle",
    "SemaphoreOffset",
    "Value",
    "Dimensions",
    "Format",
    "Pitch",
    "Modifier",
    "Size",
    "FenceValue",
];

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The bit of this reason in `CeRecMask`.
    pub const fn bit(self) -> u32 {
        1 << (self.code() - 1)
    }
}

/// What the protocol parser said about the bytes at the record's offset, without the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    /// No record (the command ends at the fence tail, or a zero magic).
    Absent,
    /// A record that passed `HeliosRmFenceTailV3::validate`.
    Record,
    /// A refused record and the parser's reason.
    Reject(Why),
}

/// What the KMD does with one present marker's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The tail is not a FENCE tail: a record is never read behind anything else. Nothing counted.
    NotFence,
    /// A FENCE tail without a record (`CeRecNoCopy`).
    NoCopy,
    /// A record the KMD keeps beside the fence (`CeRecSeen`).
    Seen,
    /// A refused record (`CeRecBad`, `CeRecWhy`, `CeRecMask`). The fence is unaffected.
    Bad(Why),
}

/// The rule: a record is read only behind a FENCE tail (`HeliosRmFenceTail::is_fence`), and a valid
/// one must name the fence's value (`matches_fence`). `fence_value_matches` is only consulted for a
/// valid record.
pub const fn classify(is_fence: bool, parsed: Parsed, fence_value_matches: bool) -> Outcome {
    if !is_fence {
        return Outcome::NotFence;
    }
    match parsed {
        Parsed::Absent => Outcome::NoCopy,
        Parsed::Reject(why) => Outcome::Bad(why),
        Parsed::Record if fence_value_matches => Outcome::Seen,
        Parsed::Record => Outcome::Bad(Why::FenceValue),
    }
}

/// The command bytes from the record's `offset` to the command's end (the parser's `available`),
/// or `None` when the command ends before the offset.
pub const fn available(cmd_len: usize, offset: usize) -> Option<usize> {
    cmd_len.checked_sub(offset)
}

/// Whether a count reaches the registry at once from the Render DDI (PASSIVE): the first event,
/// then every 256th (a registry write is synchronous and this is the frame rate). The rest is
/// mirrored by the throttled counter block (`publish_counters`).
pub const fn publish_now(n: u32) -> bool {
    n == 1 || n % 256 == 0
}

/// Whether a refusal reaches the registry at once: the first, each new reason, every 64th.
pub const fn publish_bad_now(n: u32, mask_before: u32, why: Why) -> bool {
    n == 1 || mask_before & why.bit() == 0 || n % 64 == 0
}

/// Whether a record's `h_client` is a client the PRESENTING process created (the `NvDupHarden`
/// rule). The KMD records RM clients per NVRM owner (an escape device), not per process, so in
/// this build the answer is always [`ClientCheck::Unknown`] (`kmd_render`'s hook says so).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCheck {
    Owned,
    NotOwned,
    Unknown,
}

/// The counters M3c-0 writes, all in `kmd_render/src/ddi/ce_record.rs`. At most 14 characters,
/// prefix `Ce`, unique across `kmd_render` and `kmd_logic` (the route's planned `CeTail*` names of
/// `ce_present::COUNTERS` are separate and still unwritten).
pub const COUNTERS: &[&str] = &[
    // Records parsed and kept; records refused, the last reason and every reason seen; FENCE
    // tails without a record.
    "CeRecSeen",
    "CeRecBad",
    "CeRecWhy",
    "CeRecMask",
    "CeRecNoCopy",
    // The latest kept record: `semaphore.h_client` (REG_DWORD) and `source.modifier` (REG_QWORD).
    "CeRecLast",
    "CeRecMod",
];

/// The files that write [`COUNTERS`] (relative to `kmd_render/src`).
pub const WRITERS: [&str; 1] = ["ddi/ce_record.rs"];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// `guest/windows/protocol` beside this crate (`tools/kmd-dev/test_kmd_logic.sh` copies it), or
    /// `None` to skip; `HELIOS_REQUIRE_NAME_SCAN=1` makes its absence a failure.
    fn protocol_dir() -> Option<std::path::PathBuf> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol");
        if dir.join("src/rm_fence_v3.rs").exists() {
            return Some(dir);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy protocol next to kmd_logic",
            dir.display()
        );
        None
    }

    #[test]
    fn the_reasons_are_the_parser_s_in_order_then_the_fence_value() {
        let Some(protocol) = protocol_dir() else {
            return;
        };
        let src = std::fs::read_to_string(protocol.join("src/rm_fence_v3.rs")).unwrap();
        let start = src.find("pub enum TailV3Error {").expect("TailV3Error");
        let body = &src[start..];
        let body = &body[body.find('{').unwrap() + 1..body.find('}').unwrap()];
        let variants: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .map(|l| l.trim_end_matches(','))
            .collect();
        assert_eq!(variants, WHY_NAMES[..14]);
        assert_eq!(WHY_NAMES[14], "FenceValue");
        let all = [
            Why::BadMagic,
            Why::Short,
            Why::Version,
            Why::Flags,
            Why::Incomplete,
            Why::Reserved,
            Why::Handle,
            Why::SemaphoreOffset,
            Why::Value,
            Why::Dimensions,
            Why::Format,
            Why::Pitch,
            Why::Modifier,
            Why::Size,
            Why::FenceValue,
        ];
        for (i, w) in all.iter().enumerate() {
            assert_eq!(w.code() as usize, i + 1);
            assert_eq!(std::format!("{w:?}"), WHY_NAMES[i]);
            assert_eq!(w.bit(), 1 << i);
        }
    }

    #[test]
    fn a_record_counts_only_behind_a_fence_and_with_the_fence_s_value() {
        assert_eq!(classify(false, Parsed::Record, true), Outcome::NotFence);
        assert_eq!(classify(false, Parsed::Absent, true), Outcome::NotFence);
        assert_eq!(classify(true, Parsed::Absent, false), Outcome::NoCopy);
        assert_eq!(classify(true, Parsed::Record, true), Outcome::Seen);
        assert_eq!(classify(true, Parsed::Record, false), Outcome::Bad(Why::FenceValue));
        assert_eq!(
            classify(true, Parsed::Reject(Why::Modifier), true),
            Outcome::Bad(Why::Modifier)
        );
    }

    #[test]
    fn the_record_window_of_each_carrier() {
        // HERF 48 (no record) / 168, HEPR 96 / 192.
        assert_eq!(available(48, 72), None);
        assert_eq!(available(72, 72), Some(0));
        assert_eq!(available(168, 72), Some(96));
        assert_eq!(available(96, 96), Some(0));
        assert_eq!(available(192, 96), Some(96));
        assert_eq!(available(256, 96), Some(160));
    }

    #[test]
    fn the_publish_rules() {
        assert!(publish_now(1));
        assert!(!publish_now(2));
        assert!(publish_now(256));
        assert!(publish_bad_now(1, 0, Why::Size));
        assert!(publish_bad_now(5, Why::Short.bit(), Why::Size));
        assert!(!publish_bad_now(5, Why::Size.bit(), Why::Size));
        assert!(publish_bad_now(64, Why::Size.bit(), Why::Size));
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Ce"), "{n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()));
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        for n in COUNTERS {
            assert!(!crate::ce_present::COUNTERS.contains(n), "{n}");
            assert!(!crate::rm_ce_channel::COUNTERS.contains(n), "{n}");
            assert!(!crate::blt_async::COUNTERS.contains(n), "{n}");
            assert!(!crate::guest_blob::COUNTERS.contains(n), "{n}");
            assert!(!crate::onscanout::COUNTERS.contains(n), "{n}");
            assert_ne!(*n, crate::ce_present::KNOB);
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
        let mut written = Vec::new();
        for f in WRITERS {
            written.extend(literals(&std::fs::read_to_string(render.join(f)).unwrap()));
        }
        for n in COUNTERS {
            assert!(written.iter().any(|l| l == n), "{n} is listed but not written by {WRITERS:?}");
        }
        for l in written.iter().filter(|l| l.starts_with("Ce")) {
            assert!(COUNTERS.contains(&l.as_str()), "{l} is written by {WRITERS:?} but not listed");
        }
        // No other file spells these names.
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().replace('\\', "/");
                    if WRITERS.iter().any(|w| s.ends_with(w)) {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in COUNTERS {
                        let lit = std::format!("b\"{n}\"");
                        assert!(!text.contains(&lit), "{s} spells {n}");
                    }
                }
            }
        }
        assert!(checked > 20);
    }

    /// Every protocol `TailV3Error` variant maps to the [`Why`] of the same name in the I/O file.
    #[test]
    fn the_driver_maps_each_parser_reason_to_its_namesake() {
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join(WRITERS[0])).unwrap();
        for n in &WHY_NAMES[..14] {
            let arm = std::format!("TailV3Error::{n} => Why::{n},");
            assert!(text.contains(&arm), "{arm} is missing");
        }
    }

    /// Bit 37 is the protocol's `HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3` (Rust and C), and the escape's
    /// `NVRM_OPS_IMPLEMENTED` (the always-on part of `supported_ops`, no knob) carries it.
    #[test]
    fn query_caps_advertises_bit_37_always() {
        assert_eq!(0x20_0000_0000u64, 1u64 << CAP_BIT);
        if let Some(protocol) = protocol_dir() {
            let rs = std::fs::read_to_string(protocol.join("src/rm_fence_v3.rs")).unwrap();
            assert!(rs.contains(&std::format!(
                "pub const HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3: u64 = 1 << {CAP_BIT};"
            )));
            let h =
                std::fs::read_to_string(protocol.join("include/helios_rm_fence.h")).unwrap();
            assert!(h.contains("#define HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3 0x2000000000ull"));
        }
        let Some(render) = render_src() else {
            return;
        };
        let esc = std::fs::read_to_string(render.join("ddi/escape.rs")).unwrap();
        let start = esc.find("const NVRM_OPS_IMPLEMENTED: u64").expect("NVRM_OPS_IMPLEMENTED");
        let mask = &esc[start..start + esc[start..].find(';').unwrap()];
        assert!(
            mask.contains("helios_protocol::HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3"),
            "bit 37 is not in the always-on mask"
        );
        // Not tied to the copy-engine knob.
        assert!(!mask.contains("RmCopyEngine") && !mask.contains("knob"));
    }
}

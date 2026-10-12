//! Shader bytecode facts for diagnostics, and the machine-wide shader-failure
//! ledger.
//!
//! # Why this exists
//!
//! A shader the driver cannot create is a missing draw or dispatch: the wall
//! renders untextured, a post-process pass never runs. Two different layers can
//! refuse one:
//!
//! 1. **The D3D runtime, before the driver sees it.** `d3d11.dll` reads the
//!    container's `SFI0` chunk (the `D3D_SHADER_REQUIRES_*` flags the compiler
//!    wrote) and fails `Create*Shader` with `E_INVALIDARG` when the device does
//!    not report a required feature. The driver gets no call, so it cannot log
//!    one: the only defence is advertising every feature the engine backs, and
//!    [`required_caps`] names which cap each flag needs.
//! 2. **The engine (DXVK/vkd3d-proton) or the Vulkan driver.** The DDI has no
//!    return value; a failure there used to be a log line and a null handle.
//!    [`record_failure`] makes each one a ledger line.
//!
//! # The ledger
//!
//! `C:\ProgramData\Helios\shader-failures.log`, shared by every process and
//! both UMDs: one line per failed shader or pipeline create, with the API,
//! stage, the container hash, the `SFI0` flags decoded and the reason. Its line
//! count is the counter: `0` after a game session means no shader failed in the
//! driver. Lines are bounded per process ([`LEDGER_LINES_PER_PROCESS`]); the
//! process-local total keeps counting past that and is in every line.
//!
//! The parsing half builds and is tested on Linux (no `windows` dependency).

use core::sync::atomic::{AtomicUsize, Ordering};

/// Program types of the SHDR/SHEX version token (`D3D10_SB_TOKENIZED_PROGRAM_TYPE`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Pixel,
    Vertex,
    Geometry,
    Hull,
    Domain,
    Compute,
}

impl Stage {
    pub fn from_program_type(t: u32) -> Option<Self> {
        Some(match t {
            0 => Self::Pixel,
            1 => Self::Vertex,
            2 => Self::Geometry,
            3 => Self::Hull,
            4 => Self::Domain,
            5 => Self::Compute,
            _ => return None,
        })
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Pixel => "ps",
            Self::Vertex => "vs",
            Self::Geometry => "gs",
            Self::Hull => "hs",
            Self::Domain => "ds",
            Self::Compute => "cs",
        }
    }
}

/// What a DXBC container says about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DxbcInfo {
    /// The 16-byte checksum at offset 4, as the compiler wrote it. Stable per
    /// shader across runs and machines; the name to grep dumps by.
    pub hash: [u8; 16],
    /// `SFI0` flags (`D3D_SHADER_REQUIRES_*`); 0 when the chunk is absent.
    pub requires: u64,
    /// Program type and version from SHDR/SHEX, when present.
    pub stage: Option<Stage>,
    pub major: u32,
    pub minor: u32,
    /// The container holds DXIL (D3D12 SM6) rather than SHDR/SHEX.
    pub dxil: bool,
}

impl DxbcInfo {
    pub fn hash_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.hash {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse a DXBC container. `None` for anything that is not one or whose chunk
/// table leaves the buffer (the bytes come from an application).
pub fn parse_dxbc(b: &[u8]) -> Option<DxbcInfo> {
    if b.len() < 32 || &b[0..4] != b"DXBC" {
        return None;
    }
    let total = u32_at(b, 24)? as usize;
    if total < 32 || total > b.len() {
        return None;
    }
    let b = &b[..total];
    let mut info = DxbcInfo::default();
    info.hash.copy_from_slice(&b[4..20]);
    let count = u32_at(b, 28)? as usize;
    if count > 64 {
        return None;
    }
    for i in 0..count {
        let off = u32_at(b, 32 + 4 * i)? as usize;
        let tag = b.get(off..off.checked_add(8)?)?;
        let size = u32_at(b, off + 4)? as usize;
        let body = b.get(off + 8..(off + 8).checked_add(size)?)?;
        match &tag[0..4] {
            b"SFI0" if body.len() >= 8 => {
                info.requires = u64::from_le_bytes(body[0..8].try_into().ok()?);
            }
            b"SHDR" | b"SHEX" if body.len() >= 4 => {
                let v = u32_at(body, 0)?;
                info.stage = Stage::from_program_type(v >> 16);
                info.major = (v >> 4) & 0xf;
                info.minor = v & 0xf;
            }
            b"DXIL" => info.dxil = true,
            _ => {}
        }
    }
    Some(info)
}

/// `D3D_SHADER_REQUIRES_*` (d3dcommon.h), the ones `d3d11.dll` checks against
/// device caps at shader creation, with the cap that admits each.
pub const REQUIRES: &[(u64, &str, &str)] = &[
    (0x1, "DOUBLES", "D3D11DDICAPS_SHADER_DOUBLES"),
    (0x2, "EARLY_DEPTH_STENCIL", ""),
    (0x4, "UAVS_AT_EVERY_STAGE", "feature level 11_1"),
    (0x8, "64_UAVS", "feature level 11_1"),
    (0x10, "MINIMUM_PRECISION", ""),
    (0x20, "11_1_DOUBLE_EXTENSIONS", "D3D11DDICAPS_SHADER_DOUBLES + 11.1 DDI"),
    (0x40, "11_1_SHADER_EXTENSIONS", "11.1 DDI (SAD4)"),
    (0x80, "LEVEL_9_COMPARISON_FILTERING", ""),
    (0x100, "TILED_RESOURCES", "D3D11_OPTIONS1 tiled tier 1+"),
    (0x200, "STENCIL_REF", "D3D11DDICAPS_SHADER_SPECIFIED_STENCIL_REF (WDDM 2.0+ DDI)"),
    (0x400, "INNER_COVERAGE", "D3D11_OPTIONS2 conservative tier 3 (WDDM 2.0+ DDI)"),
    (
        0x800,
        "TYPED_UAV_LOAD_ADDITIONAL_FORMATS",
        "D3D11DDICAPS_SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS (WDDM 2.0+ DDI)",
    ),
    (0x1000, "ROVS", "D3D11DDICAPS_SHADER_ROVS (WDDM 2.0+ DDI)"),
    (
        0x2000,
        "VIEWPORT_AND_RT_ARRAY_INDEX_FROM_ANY_SHADER_FEEDING_RASTERIZER",
        "D3D11_OPTIONS3 (WDDM 2.0+ DDI)",
    ),
    (0x4000, "WAVE_OPS", ""),
    (0x8000, "INT64_OPS", ""),
    (0x10000, "VIEW_ID", ""),
    (0x20000, "BARYCENTRICS", ""),
    (0x40000, "NATIVE_16BIT_OPS", ""),
    (0x80000, "SHADING_RATE", ""),
    (0x100000, "RAYTRACING_TIER_1_1", ""),
    (0x200000, "SAMPLER_FEEDBACK", ""),
    (0x400000, "ATOMIC_INT64_ON_TYPED_RESOURCE", ""),
    (0x800000, "ATOMIC_INT64_ON_GROUP_SHARED", ""),
    (0x1000000, "DERIVATIVES_IN_MESH_AND_AMPLIFICATION_SHADERS", ""),
    (0x2000000, "RESOURCE_DESCRIPTOR_HEAP_INDEXING", ""),
    (0x4000000, "SAMPLER_DESCRIPTOR_HEAP_INDEXING", ""),
    (0x8000000, "WAVE_MMA", ""),
    (0x10000000, "ATOMIC_INT64_ON_DESCRIPTOR_HEAP_RESOURCE", ""),
];

/// `SFI0` flags as `NAME|NAME`, unknown bits in hex; `-` for none.
pub fn requires_names(flags: u64) -> String {
    if flags == 0 {
        return "-".into();
    }
    let mut out = Vec::new();
    let mut rest = flags;
    for &(bit, name, _) in REQUIRES {
        if flags & bit != 0 {
            out.push(name.to_string());
            rest &= !bit;
        }
    }
    if rest != 0 {
        out.push(format!("0x{rest:x}"));
    }
    out.join("|")
}

/// The caps a shader's `SFI0` flags need from the device (empty strings
/// filtered out): what to check when the runtime refused a create.
pub fn required_caps(flags: u64) -> Vec<&'static str> {
    REQUIRES
        .iter()
        .filter(|&&(bit, _, cap)| flags & bit != 0 && !cap.is_empty())
        .map(|&(_, _, cap)| cap)
        .collect()
}

/// Ledger lines one process may write; the process total keeps counting.
pub const LEDGER_LINES_PER_PROCESS: usize = 256;

static FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Shader/pipeline create failures recorded by this process.
pub fn failure_count() -> usize {
    FAILURES.load(Ordering::Relaxed)
}

/// FNV-1a 64 over the bytes: the name of a raw token stream, which carries no
/// compiler checksum.
pub fn fnv1a64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One ledger line, without the timestamp/process prefix. Pure, for tests.
///
/// `code` is a DXBC container (D3D12, tools) or the raw SHDR/SHEX token stream
/// the D3D11 DDI hands the driver. The token stream has no `SFI0` chunk: the
/// runtime already checked those flags against the caps before calling.
pub fn format_failure(api: &str, stage: &str, code: &[u8], reason: &str, total: usize) -> String {
    if let Some(info) = parse_dxbc(code) {
        return format!(
            "{api} {stage} hash={} sm={}.{}{} requires={} reason={reason} n={total}",
            info.hash_hex(),
            info.major,
            info.minor,
            if info.dxil { " dxil" } else { "" },
            requires_names(info.requires),
        );
    }
    match u32_at(code, 0) {
        Some(v) if code.len() >= 8 => format!(
            "{api} {stage} tokens={:016x} len={} sm={}.{} reason={reason} n={total}",
            fnv1a64(code),
            code.len(),
            (v >> 4) & 0xf,
            v & 0xf,
        ),
        _ => format!("{api} {stage} hash=? len={} reason={reason} n={total}", code.len()),
    }
}

/// Count a failed shader or pipeline create and append it to the ledger.
/// Returns the line (without prefix) for the caller's own log.
pub fn record_failure(api: &str, stage: &str, code: &[u8], reason: &str) -> String {
    let total = FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    let line = format_failure(api, stage, code, reason, total);
    if total <= LEDGER_LINES_PER_PROCESS {
        ledger_append(&line);
    }
    line
}

/// Append a non-shader line (a caps mismatch that predicts refusals) to the
/// ledger. Not counted in [`failure_count`]; bounded like failures.
pub fn ledger_note(line: &str) {
    static NOTES: AtomicUsize = AtomicUsize::new(0);
    if NOTES.fetch_add(1, Ordering::Relaxed) < LEDGER_LINES_PER_PROCESS {
        ledger_append(line);
    }
}

#[cfg(windows)]
fn ledger_append(line: &str) {
    use std::io::Write;
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let text = format!("{secs} {exe} pid={} {line}\r\n", std::process::id());
    // One write per line with append semantics: lines from processes that
    // fail at the same moment do not interleave mid-line.
    let path = std::path::Path::new(r"C:\ProgramData\Helios\shader-failures.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

#[cfg(not(windows))]
fn ledger_append(_line: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal container: header, one SFI0 chunk, one SHEX chunk.
    fn container(requires: u64, version: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"DXBC");
        b.extend_from_slice(&[0xab; 16]);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // total, patched below
        b.extend_from_slice(&2u32.to_le_bytes());
        let sfi = 32 + 8;
        let shex = sfi + 16;
        b.extend_from_slice(&(sfi as u32).to_le_bytes());
        b.extend_from_slice(&(shex as u32).to_le_bytes());
        b.extend_from_slice(b"SFI0");
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&requires.to_le_bytes());
        b.extend_from_slice(b"SHEX");
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&version.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        let total = b.len() as u32;
        b[24..28].copy_from_slice(&total.to_le_bytes());
        b
    }

    #[test]
    fn parses_cs50_with_typed_uav_loads() {
        let b = container(0x800, 0x0005_0050);
        let info = parse_dxbc(&b).unwrap();
        assert_eq!(info.requires, 0x800);
        assert_eq!(info.stage, Some(Stage::Compute));
        assert_eq!((info.major, info.minor), (5, 0));
        assert_eq!(info.hash_hex(), "ab".repeat(16));
        assert_eq!(requires_names(info.requires), "TYPED_UAV_LOAD_ADDITIONAL_FORMATS");
        assert_eq!(
            required_caps(info.requires),
            vec!["D3D11DDICAPS_SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS (WDDM 2.0+ DDI)"]
        );
    }

    #[test]
    fn rejects_truncated_and_foreign_bytes() {
        let b = container(0, 0x0001_0050);
        assert!(parse_dxbc(&b[..40]).is_none());
        assert!(parse_dxbc(b"not a shader at all, no header here!").is_none());
        let mut bad = b.clone();
        bad[32..36].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(parse_dxbc(&bad).is_none());
    }

    #[test]
    fn names_unknown_bits() {
        assert_eq!(requires_names(0), "-");
        assert_eq!(requires_names(0x1 | 0x2000), "DOUBLES|VIEWPORT_AND_RT_ARRAY_INDEX_FROM_ANY_SHADER_FEEDING_RASTERIZER");
        assert_eq!(requires_names(1 << 40), "0x10000000000");
    }

    #[test]
    fn formats_a_ledger_line() {
        let b = container(0x1000, 0x0000_0050);
        let line = format_failure("d3d11", "ps", &b, "engine", 3);
        assert_eq!(line, format!("d3d11 ps hash={} sm=5.0 requires=ROVS reason=engine n=3", "ab".repeat(16)));
        assert!(format_failure("d3d11", "vs", &[1, 2, 3], "x", 1).contains("hash=? len=3"));
        // A raw cs_5_0 token stream, as the D3D11 DDI passes it.
        let mut tokens = 0x0005_0050u32.to_le_bytes().to_vec();
        tokens.extend_from_slice(&2u32.to_le_bytes());
        let line = format_failure("d3d11", "cs", &tokens, "engine", 1);
        assert_eq!(line, format!("d3d11 cs tokens={:016x} len=8 sm=5.0 reason=engine n=1", fnv1a64(&tokens)));
    }
}

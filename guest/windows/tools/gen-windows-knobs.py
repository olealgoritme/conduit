#!/usr/bin/env python3
"""Generate docs/WINDOWS-KNOBS.md, the reference of every Windows guest knob.

Names, types, defaults and source locations come from the code; only the
human part (effect, class, when it is read) lives in windows-knobs.toml next
to this script. Nothing here keeps a second copy of a default.

  gen-windows-knobs.py            write docs/WINDOWS-KNOBS.md
  gen-windows-knobs.py --check    fail if a knob in code has no description, a
                                  description names no knob, or the doc is stale
  gen-windows-knobs.py --audit    list places in docs and code comments whose
                                  "(default N)" disagrees with the code
  gen-windows-knobs.py --refresh-mesa
                                  re-read the Mesa fork (the submodule must be
                                  checked out at its pinned commit) and update
                                  windows-knobs-mesa.json

What is extracted:
  * KMD service key: the `knobs` module of kmd_render/src/diag.rs and every
    read_config_dword(knobs::X, default) (direct or through a one-level
    wrapper function), defaults resolved through `const` items in kmd_render
    and kmd_logic.
  * HKLM\\SOFTWARE\\Helios: BoolKnob/DwordKnob definitions, the ad hoc
    reg_dword/reg_sz reads of the Rust UMDs, the C/C++ bridge and policy
    header reads, and the install scripts' reads.
  * Environment: HELIOS_*/NVK_*/MESA_WSI_* and similar reads in the UMDs and
    bridges, in the patch series (NVK on RM in build order, DXVK, vkd3d-proton,
    Venus ICD), and HELIOS_* reads of the Mesa fork (cached per pinned commit).
  * Host: the keys of cli/src/config.rs (`conduit config`).
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
GW = ROOT / "guest/windows"
DESC = Path(__file__).resolve().with_name("windows-knobs.toml")
MESA_CACHE = Path(__file__).resolve().with_name("windows-knobs-mesa.json")
OUT = ROOT / "docs/WINDOWS-KNOBS.md"
MESA_SUBMODULE = "guest/windows/third_party/mesa"

CLASSES = {
    "supported": "Supported setting",
    "tuning": "Performance tuning",
    "debug": "Debug / diagnostic",
    "experiment": "Experiment, not for users",
    "state": "State written by the driver",
}
CLASS_ORDER = list(CLASSES)


def rel(p: Path) -> str:
    return p.resolve().relative_to(ROOT).as_posix()


def line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


@dataclass
class Knob:
    section: str
    name: str
    type: str
    defaults: list[str] = field(default_factory=list)
    sources: list[str] = field(default_factory=list)
    notes: list = field(default_factory=list)

    def add(self, default: str | None, source: str) -> None:
        if default is not None and default not in self.defaults:
            self.defaults.append(default)
        if source not in self.sources:
            self.sources.append(source)


class Inventory:
    def __init__(self) -> None:
        self.knobs: dict[tuple[str, str], Knob] = {}

    def get(self, section: str, name: str, type_: str) -> Knob:
        key = (section, name)
        if key not in self.knobs:
            self.knobs[key] = Knob(section, name, type_)
        return self.knobs[key]


# ---------------------------------------------------------------------------
# Small Rust/C helpers


def strip_line_comments(text: str) -> str:
    """Blank `//` comments (keeping offsets), leaving string literals alone."""
    out = []
    for line in text.split("\n"):
        idx = find_comment(line)
        out.append(line if idx < 0 else line[:idx] + " " * (len(line) - idx))
    return "\n".join(out)


def find_comment(line: str) -> int:
    in_str = False
    i = 0
    while i < len(line) - 1:
        c = line[i]
        if c == '"' and (i == 0 or line[i - 1] != "\\"):
            in_str = not in_str
        elif not in_str and c == "/" and line[i + 1] == "/":
            return i
        i += 1
    return -1


def balanced_args(text: str, open_pos: int) -> tuple[list[str], int]:
    """Split the argument list starting at text[open_pos] == '(' at depth 0."""
    assert text[open_pos] == "("
    depth = 0
    args, cur = [], []
    i = open_pos
    while i < len(text):
        c = text[i]
        if c in "([{":
            depth += 1
            if depth > 1:
                cur.append(c)
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                last = "".join(cur).strip()
                if last or not args:
                    args.append(last)
                return args, i
            cur.append(c)
        elif c == "," and depth == 1:
            args.append("".join(cur).strip())
            cur = []
        else:
            cur.append(c)
        i += 1
    raise ValueError("unbalanced")


def squash(expr: str) -> str:
    expr = re.sub(r"\s+", " ", expr).strip()
    expr = expr.replace("( ", "(").replace(" )", ")")
    return expr


class ConstTable:
    """`const NAME: T = value;` items, looked up by name (and module hint)."""

    def __init__(self, files: list[Path]) -> None:
        self.defs: dict[str, list[tuple[str, str]]] = {}
        pat = re.compile(r"\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*[^=;]+=\s*([^;]+);")
        for f in files:
            text = strip_line_comments(f.read_text(errors="replace"))
            for m in pat.finditer(text):
                self.defs.setdefault(m.group(1), []).append((rel(f), squash(m.group(2))))

    def lookup(self, path: str) -> str | None:
        parts = path.split("::")
        name = parts[-1]
        cands = self.defs.get(name, [])
        if not cands:
            return None
        if len(cands) > 1 and len(parts) > 1:
            hint = parts[-2]
            narrowed = [c for c in cands if hint in c[0]]
            if narrowed:
                cands = narrowed
        values = {v for _, v in cands}
        if len(values) == 1:
            return values.pop()
        return None

    def resolve(self, expr: str, depth: int = 0) -> str:
        """Replace const paths by their values; keep everything else."""
        expr = squash(expr)
        if depth > 4:
            return expr

        def sub(m: re.Match) -> str:
            path = m.group(0)
            if re.fullmatch(r"[0-9][0-9_xa-fA-F]*", path) or re.match(r"(u8|u16|u32|u64|usize|i32|i64)::", path):
                return path
            val = self.lookup(path)
            if val is None:
                return path.split("::")[-1] if "::" in path else path
            return self.resolve(val, depth + 1)

        out = re.sub(r"(?:[a-z_][a-z0-9_]*::)*[A-Z][A-Z0-9_]+\b(?!\s*\()", sub, expr)
        return normalize_number(out)


def eval_int(node):
    import ast
    if isinstance(node, ast.Constant) and isinstance(node.value, int):
        return node.value
    if isinstance(node, ast.BinOp):
        a, b = eval_int(node.left), eval_int(node.right)
        ops = {ast.BitOr: lambda: a | b, ast.BitAnd: lambda: a & b, ast.LShift: lambda: a << b,
               ast.Add: lambda: a + b, ast.Mult: lambda: a * b, ast.Sub: lambda: a - b}
        return ops[type(node.op)]()
    raise ValueError


def normalize_number(expr: str) -> str:
    import ast
    s = expr.strip()
    if re.fullmatch(r"[0-9xa-fA-F_ ()<>|&+*-]+", s) and re.search(r"[<|&+*-]", s):
        try:
            return str(eval_int(ast.parse(s.replace("_", ""), mode="eval").body))
        except (ValueError, SyntaxError, KeyError):
            pass
    if re.fullmatch(r"[0-9][0-9_]*", s):
        return str(int(s.replace("_", "")))
    if re.fullmatch(r"0x[0-9a-fA-F_]+", s):
        return s.replace("_", "")
    return s


def rust_files(*dirs: Path) -> list[Path]:
    out = []
    for d in dirs:
        for f in sorted(d.rglob("*.rs")):
            parts = set(f.relative_to(d).parts)
            if "target" in parts or "tests" in parts or f.name == "build.rs":
                continue
            out.append(f)
    return out


def c_files(*dirs: Path) -> list[Path]:
    out = []
    for d in dirs:
        for ext in ("*.c", "*.cpp", "*.h", "*.hpp"):
            out.extend(sorted(d.rglob(ext)))
    return out


# ---------------------------------------------------------------------------
# KMD service key

KMD_SECTION = "kmd"
KMD_SRC = GW / "kmd_render/src"
KMD_LOGIC_SRC = GW / "kmd_logic/src"
KMD_CONST_NAMES: dict[str, str] = {}


def extract_kmd(inv: Inventory) -> dict[str, str]:
    diag = KMD_SRC / "diag.rs"
    text = diag.read_text()
    consts = ConstTable(rust_files(KMD_SRC, KMD_LOGIC_SRC))
    m = re.search(r"pub mod knobs \{", text)
    start = m.end()
    end = text.index("\n}\n", start)
    body = text[start:end]
    const_to_name: dict[str, str] = {}
    for km in re.finditer(r"pub const ([A-Z0-9_]+): KnobName = KnobName::new\(([^)]*)\);", body):
        const, arg = km.group(1), km.group(2).strip()
        if arg.startswith('b"'):
            name = arg[2:-1]
        else:
            path = arg
            val = consts.lookup(path)
            if val is None or not val.startswith('b"'):
                raise SystemExit(f"cannot resolve knob name {arg} for {const}")
            name = val[2:-1]
        const_to_name[const] = name
        KMD_CONST_NAMES[const] = name
        k = inv.get(KMD_SECTION, name, "REG_DWORD")
        k.add(None, f"{rel(diag)}:{line_of(text, start + km.start())}")

    files = rust_files(KMD_SRC)
    wrappers: dict[tuple[str, str], str | None] = {}
    use_pat = re.compile(r"(?:crate::diag::)?knobs::([A-Z0-9_]+)\b")
    for f in files:
        raw = f.read_text()
        code = strip_line_comments(raw)
        for um in use_pat.finditer(code):
            if f == diag and start <= um.start() < end:
                continue
            const = um.group(1)
            if const not in const_to_name:
                continue
            name = const_to_name[const]
            default = kmd_default_at(code, um.start(), f, wrappers, consts)
            src = f"{rel(f)}:{line_of(code, um.start())}"
            inv.get(KMD_SECTION, name, "REG_DWORD").add(default, src)
    return const_to_name


def enclosing_call(code: str, pos: int) -> tuple[str, int, int] | None:
    """The innermost call whose argument list contains pos: (ident, open, argindex)."""
    depth = 0
    argidx = 0
    i = pos - 1
    while i >= 0:
        c = code[i]
        if c in ")]}":
            depth += 1
        elif c in "([{":
            if depth == 0:
                if c != "(":
                    return None
                j = i - 1
                while j >= 0 and code[j].isspace():
                    j -= 1
                k = j
                while k >= 0 and (code[k].isalnum() or code[k] in "_:"):
                    k -= 1
                ident = code[k + 1 : j + 1].split("::")[-1]
                return ident, i, argidx
            depth -= 1
        elif c == "," and depth == 0:
            argidx += 1
        elif c == ";" and depth == 0:
            return None
        i -= 1
    return None


def use_aliases(code: str) -> dict[str, str]:
    """`use a::b::c as x;` -> {x: c}."""
    out = {}
    for m in re.finditer(r"\buse\s+([a-z_][a-z0-9_:]*)\s+as\s+([a-z_][a-z0-9_]*)\s*;", code):
        out[m.group(2)] = m.group(1).split("::")[-1]
    for m in re.finditer(r"\buse\s+([a-z_][a-z0-9_:]*)::\{[^}]*?\bself\s+as\s+([a-z_][a-z0-9_]*)", code):
        out[m.group(2)] = m.group(1).split("::")[-1]
    return out


def kmd_default_at(code, pos, f, wrappers, consts) -> str | None:
    call = enclosing_call(code, pos)
    if call is None:
        return None
    ident, open_pos, argidx = call
    if ident == "read_config_dword":
        args, _ = balanced_args(code, open_pos)
        if len(args) == 2:
            return render_default(args[1], code, open_pos, consts)
        return None
    # One-level wrapper: fn ident(..., name: KnobName) { read_config_dword(name, D) }
    key = (rel(f), ident)
    if key not in wrappers:
        wrappers[key] = None
        fm = re.search(r"fn " + re.escape(ident) + r"\s*\(([^)]*)\)", code)
        if fm:
            params = [p.split(":")[0].strip() for p in fm.group(1).split(",")]
            bstart = code.index("{", fm.end())
            depth, j = 0, bstart
            while j < len(code):
                if code[j] == "{":
                    depth += 1
                elif code[j] == "}":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            fbody = code[bstart:j]
            for p in params:
                rm = re.search(r"read_config_dword\(\s*" + re.escape(p) + r"\s*,", fbody)
                if rm:
                    args, _ = balanced_args(fbody, fbody.index("(", rm.start()))
                    wrappers[key] = render_default(args[1], fbody, rm.start(), consts)
                    break
        if wrappers[key] is None:
            # Not a reader (e.g. passes the knob on to a writer).
            return None
    return wrappers[key]


def render_default(expr: str, code: str, at: int, consts: ConstTable) -> str:
    expr = squash(expr)
    # A local `let x = ...;` in the same function: inline it once.
    if re.fullmatch(r"[a-z_][a-z0-9_]*", expr):
        lm = None
        for lm in re.finditer(r"let\s+" + re.escape(expr) + r"\s*=\s*", code[:at]):
            pass
        if lm:
            j = lm.end()
            depth = 0
            while j < len(code):
                c = code[j]
                if c in "([{":
                    depth += 1
                elif c in ")]}":
                    depth -= 1
                elif c == ";" and depth == 0:
                    break
                j += 1
            expr = squash(code[lm.end() : j])
    for alias, target in use_aliases(code).items():
        expr = re.sub(r"\b" + alias + r"::", target + "::", expr)
    expr = re.sub(r"\b(?:crate|helios_kmd_logic|super)::", "", expr)
    expr = re.sub(r"(?:diag::)?read_config_dword\(\s*(?:diag::)?knobs::([A-Z0-9_]+)\s*,[^()]*\)",
                  lambda m: KMD_CONST_NAMES.get(m.group(1), m.group(1)), expr)
    return consts.resolve(expr)


# ---------------------------------------------------------------------------
# HKLM\SOFTWARE\Helios

UMD_SECTION = "umd"
UMD_DIRS = [GW / "umd/src", GW / "umd12/src", GW / "umd_common/src"]
BRIDGE_DIRS = [GW / "umd/bridge", GW / "umd12/bridge", GW / "umd_common/bridge", GW / "protocol/include"]


def rust_trailing_default(code: str, close: int) -> str | None:
    """Follow `.or_else(..)`, `.map(..)`, `.is_some_and(..)`, `.unwrap_or(..)` after a call,
    leaving a closure (`.or_else(|| reg_dword(..))`) to follow the chain it sits in."""
    for _ in range(4):
        d = trailing_chain(code, close)
        if d is not None:
            return d
        call = enclosing_call(code, code.rfind("(", 0, close + 1))
        if call is None or call[0] not in ("or_else", "map", "and_then", "or"):
            return None
        close = balanced_args(code, call[1])[1]
    return None


def trailing_chain(code: str, close: int) -> str | None:
    i = close + 1
    while True:
        m = re.match(r"\s*\.\s*([a-z_]+)\s*\(", code[i:])
        if not m:
            return None
        method = m.group(1)
        open_pos = i + m.end() - 1
        args, end = balanced_args(code, open_pos)
        if method == "unwrap_or":
            return squash(args[0])
        if method == "unwrap_or_else":
            body = squash(re.sub(r"^\|\|\s*", "", args[0]))
            return body if len(body) <= 70 else "computed: see source"
        if method == "is_some_and":
            return "absent"
        if method == "unwrap_or_default":
            return "0"
        i = end + 1


def extract_umd(inv: Inventory, kmd_names: set[str]) -> None:
    consts = ConstTable(rust_files(*UMD_DIRS))
    knob_pat = re.compile(r'\b(BoolKnob|DwordKnob)::new\(\s*c"([A-Za-z0-9_]+)"\s*,\s*([^)]+?)\s*\)', re.S)
    adhoc_pat = re.compile(r'\b(reg_dword|reg_sz|kmd_service_dword)\(\s*c"([A-Za-z0-9_]+)"\s*\)')
    for f in rust_files(*UMD_DIRS):
        code = strip_line_comments(f.read_text())
        for m in knob_pat.finditer(code):
            kind, name, default = m.groups()
            default = consts.resolve(default)
            if kind == "BoolKnob":
                default = {"true": "1 (on)", "false": "0 (off)"}.get(default, default)
            inv.get(UMD_SECTION, name, "REG_DWORD").add(default, f"{rel(f)}:{line_of(code, m.start())}")
        for m in adhoc_pat.finditer(code):
            fn, name = m.group(1), m.group(2)
            src = f"{rel(f)}:{line_of(code, m.start())}"
            if fn == "kmd_service_dword":
                if name in kmd_names:
                    inv.get(KMD_SECTION, name, "REG_DWORD").notes.append(("also read by the D3D11 UMD", src))
                continue
            close = balanced_args(code, code.index("(", m.start()))[1]
            default = rust_trailing_default(code, close)
            type_ = "REG_SZ" if fn == "reg_sz" else "REG_DWORD"
            inv.get(UMD_SECTION, name, type_).add(default if default else "absent", src)

    c_reader = re.compile(
        r'\b(reg_sz|reg_dword|reg_wsz|helios_policy_reg_sz|helios_policy_reg_dword|helios_reg_dword)'
        r'\(\s*L?"([A-Za-z0-9_]+)"\s*,\s*([^,)]*)'
    )
    c_direct = C_REG_DIRECT
    for f in c_files(*BRIDGE_DIRS):
        code = strip_line_comments(f.read_text(errors="replace"))
        for m in c_reader.finditer(code):
            fn, name, arg2 = m.groups()
            src = f"{rel(f)}:{line_of(code, m.start())}"
            if fn.endswith("dword"):
                type_ = "REG_DWORD"
                if fn == "helios_reg_dword":
                    default = normalize_number(arg2)
                else:
                    var = arg2.strip().lstrip("&")
                    before = code[max(0, m.start() - 1500) : m.start()]
                    dm = None
                    for dm in re.finditer(r"\bDWORD\s+" + re.escape(var) + r"\s*=\s*([^;]+);", before):
                        pass
                    default = normalize_number(dm.group(1)) if dm else "absent"
            else:
                type_ = "REG_SZ"
                default = "absent"
            inv.get(UMD_SECTION, name, type_).add(default, src)
        for m in c_direct.finditer(code):
            name, flags = m.groups()
            type_ = "REG_SZ" if "REG_SZ" in flags else "REG_DWORD"
            inv.get(UMD_SECTION, name, type_).add("absent", f"{rel(f)}:{line_of(code, m.start())}")

    ps_pat = re.compile(r'HKLM:\\SOFTWARE\\Helios"?\s+-Name\s+"?([A-Za-z0-9_]+)"?')
    for f in sorted((GW / "packaging/windows").glob("*.ps1")):
        text = f.read_text(errors="replace")
        for m in ps_pat.finditer(text):
            line = text[text.rfind("\n", 0, m.start()) + 1 : text.find("\n", m.end())]
            if "Get-ItemProperty" not in line:
                continue
            inv.get(UMD_SECTION, m.group(1), "REG_DWORD").add("absent", f"{rel(f)}:{line_of(text, m.start())}")


# ---------------------------------------------------------------------------
# Environment

ENV_SECTION = "env"
ENV_NAME = re.compile(r"^(HELIOS|NVK|NAK|MESA_WSI|DXVK_HELIOS|ZINK_VULKAN|VKD3D)_[A-Z0-9_]+$")
# name, index of the default argument (None: no default in the call)
ENV_READERS = {
    "getenv": None,
    "std::getenv": None,
    "GetEnvironmentVariableA": None,
    "GetEnvironmentVariableW": None,
    "GetEnvironmentVariable": None,
    "env::var": None,
    "std::env::var": None,
    "env::var_os": None,
    "std::env::var_os": None,
    "env_bool": None,
    "env_sz": None,
    "os_get_option": None,
    "debug_get_option": 1,
    "debug_get_bool_option": 1,
    "debug_get_num_option": 1,
    "env::getEnvVar": None,
    "getEnvVar": None,
    "vkd3d_get_env_var": None,
    "helios_env_u32": 1,
    "helios_env_get": None,
    "vn_helios_env_enabled": None,
}
ENV_CALL = re.compile(
    r"\b((?:std::)?(?:env::)?[A-Za-z_][A-Za-z0-9_]*)\s*\(\s*(?:L|c)?\"([A-Za-z0-9_]+)\"\s*([,)])"
)
ONCE_CALL = re.compile(r"\bDEBUG_GET_ONCE_(BOOL|NUM)_OPTION\(\s*\w+\s*,\s*\"([A-Za-z0-9_]+)\"\s*,\s*([^)]+)\)")
# Variables our code SETS for the process (the bridge), not reads.
ENV_SET = re.compile(r"\b(?:_putenv_s|SetEnvironmentVariable[AW]?)\(\s*L?\"([A-Za-z0-9_]+)\"\s*,\s*L?\"([^\"]*)\"")


def env_reads(text: str):
    """Yield (name, default or None, offset) for every env read in `text`."""
    for m in ENV_CALL.finditer(text):
        fn, name, nxt = m.groups()
        base = fn.split("::")[-1]
        key = fn if fn in ENV_READERS else base
        if key not in ENV_READERS and base not in ENV_READERS:
            continue
        if not ENV_NAME.match(name):
            continue
        idx = ENV_READERS.get(key, ENV_READERS.get(base))
        default = None
        if idx is not None and nxt == ",":
            open_pos = text.index("(", m.start())
            try:
                args, _ = balanced_args(text, open_pos)
                if len(args) > idx:
                    default = squash(args[idx])
            except ValueError:
                pass
        yield name, default, m.start()
    for m in ONCE_CALL.finditer(text):
        yield m.group(2), squash(m.group(3)), m.start()


def fmt_env_default(default: str | None) -> str:
    if default is None:
        return "unset"
    return normalize_number(default)


def extract_env_tree(inv: Inventory, sets: dict[str, list[str]]) -> None:
    files = [f for f in rust_files(*UMD_DIRS, GW / "icd/win-build")] + c_files(*BRIDGE_DIRS)
    for f in files:
        code = strip_line_comments(f.read_text(errors="replace"))
        for name, default, pos in env_reads(code):
            inv.get(ENV_SECTION, name, "env").add(fmt_env_default(default), f"{rel(f)}:{line_of(code, pos)}")
        for m in ENV_SET.finditer(code):
            sets.setdefault(m.group(1), []).append((m.group(2), f"{rel(f)}:{line_of(code, m.start())}"))


def nvk_series() -> list[Path]:
    """The Windows NVK-on-RM series in build order (guest/nvk-rm/build-windows.sh)."""
    d = ROOT / "guest/nvk-rm"
    first = [p for p in sorted((d / "patches").glob("00*.patch")) if int(p.name[:4]) <= 13]
    rest = []
    for sub in ("patches-windows", "patches-windows-dxvk", "patches-common"):
        rest += sorted((d / sub).glob("*.patch"))
    return first + rest


def series_from_file(d: Path) -> list[Path]:
    s = d / "series"
    if s.exists():
        names = [l.strip() for l in s.read_text().splitlines() if l.strip() and not l.startswith("#")]
        return [d / n for n in names]
    return sorted(d.glob("*.patch"))


# A direct HKLM\SOFTWARE\Helios read in C (the bridges, and our patch series).
C_REG_DIRECT = re.compile(
    r'RegGetValue[AW]\(\s*HKEY_LOCAL_MACHINE\s*,\s*L?"SOFTWARE\\\\Helios"\s*,\s*L?"([A-Za-z0-9_]+)"\s*,\s*([A-Z_| ]+)'
)


def extract_env_patches(inv: Inventory, label: str, patches: list[Path]) -> None:
    """Apply the series' +/- lines in order; what stays added is the code.
    Direct HKLM\\SOFTWARE\\Helios reads in the added code count too."""
    live: dict[str, list[tuple[str, str]]] = {}  # line text -> [(patch, line no)]
    for p in patches:
        lines = p.read_text(errors="replace").split("\n")
        removed, added = [], []
        for no, line in enumerate(lines, 1):
            if line.startswith("+++") or line.startswith("---"):
                continue
            if line.startswith("-"):
                removed.append(line[1:])
            elif line.startswith("+"):
                added.append((line[1:], no))
        for t in removed:
            if live.get(t):
                live[t].pop()
        for t, no in added:
            live.setdefault(t, []).append((rel(p), no))
    alive = {w for where in live.values() for w in where}
    defines: dict[str, list[str]] = {}
    for t, where in live.items():
        dm = re.match(r"\s*#\s*define\s+([A-Z][A-Z0-9_]+)\s+(\S[^/]*?)\s*(?:/[/*].*)?$", t)
        if dm and where:
            defines.setdefault(dm.group(1), [])
            v = normalize_number(dm.group(2).strip("()"))
            if v not in defines[dm.group(1)]:
                defines[dm.group(1)].append(v)

    def resolve_define(d: str) -> str:
        vals = defines.get(d.strip())
        if not vals:
            return d
        return vals[0] if len(vals) == 1 else f"{d.strip()} ({' / '.join(vals)} by platform)"
    for p in patches:
        lines = p.read_text(errors="replace").split("\n")
        for idx, line in enumerate(lines):
            if not line.startswith("+") or line.startswith("+++") or (rel(p), idx + 1) not in alive:
                continue
            t = line[1:]
            if t.strip().startswith(("*", "/*", "//", "#")):
                continue
            # A call split over lines: parse it with the next added lines.
            ctx = t
            for nxt in lines[idx + 1 : idx + 4]:
                if not nxt.startswith("+"):
                    break
                ctx += "\n" + nxt[1:]
            for name, default, pos in env_reads(ctx):
                if pos < len(t):
                    d = fmt_env_default(default)
                    inv.get(ENV_SECTION, name, "env").add(resolve_define(d), f"{rel(p)}:{idx + 1}")
            for m in C_REG_DIRECT.finditer(ctx):
                if m.start() < len(t):
                    type_ = "REG_SZ" if "REG_SZ" in m.group(2) else "REG_DWORD"
                    inv.get(UMD_SECTION, m.group(1), type_).add("absent", f"{rel(p)}:{idx + 1}")


def mesa_pin() -> str:
    out = subprocess.run(
        ["git", "-C", str(ROOT), "ls-tree", "HEAD", MESA_SUBMODULE], capture_output=True, text=True
    ).stdout.split()
    return out[2] if len(out) >= 3 else ""


def refresh_mesa_cache(mesa_dir: str | None) -> None:
    sub = Path(mesa_dir).resolve() if mesa_dir else ROOT / MESA_SUBMODULE
    pin = mesa_pin()
    head = subprocess.run(["git", "-C", str(sub), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    if head != pin:
        raise SystemExit(f"{MESA_SUBMODULE} is at {head or 'nothing'}, the pin is {pin}: check out the pin first")
    files = subprocess.run(
        ["git", "-C", str(sub), "grep", "-l", "-E", r'"HELIOS_[A-Z0-9_]+"', "--", "*.c", "*.cpp", "*.h"],
        capture_output=True,
        text=True,
    ).stdout.split()
    reads = []
    for f in sorted(files):
        code = strip_line_comments((sub / f).read_text(errors="replace"))
        for name, default, pos in env_reads(code):
            if name.startswith("HELIOS_"):
                reads.append({"name": name, "default": fmt_env_default(default),
                              "source": f"{MESA_SUBMODULE}/{f}:{line_of(code, pos)}"})
    MESA_CACHE.write_text(json.dumps({"commit": pin, "reads": reads}, indent=1) + "\n")
    print(f"wrote {rel(MESA_CACHE)}: {len(reads)} reads at {pin[:12]}")


def extract_env_mesa(inv: Inventory, problems: list[str]) -> None:
    if not MESA_CACHE.exists():
        problems.append(f"{rel(MESA_CACHE)} is missing: run with --refresh-mesa")
        return
    cache = json.loads(MESA_CACHE.read_text())
    if cache["commit"] != mesa_pin():
        problems.append(
            f"{rel(MESA_CACHE)} describes Mesa {cache['commit'][:12]}, the submodule pin is {mesa_pin()[:12]}: "
            "check the submodule out and run with --refresh-mesa"
        )
    for r in cache["reads"]:
        inv.get(ENV_SECTION, r["name"], "env").add(r["default"], r["source"])


# ---------------------------------------------------------------------------
# Host config keys


def extract_host() -> list[tuple[str, str, str]]:
    text = (ROOT / "cli/src/config.rs").read_text()
    m = re.search(r"const KEYS: &\[\(&str, &\[&str\], &str\)\] = &\[(.*?)\n\];", text, re.S)
    out = []
    for tm in re.finditer(r'\(\s*"([^"]+)",\s*&\[([^\]]*)\],\s*"((?:[^"\\]|\\.)*)",?\s*\)', m.group(1), re.S):
        vals = ", ".join(v.strip().strip('"') for v in tm.group(2).split(",") if v.strip())
        out.append((tm.group(1), vals, squash(tm.group(3))))
    return out


# ---------------------------------------------------------------------------
# Installation-written values


def extract_install() -> list[tuple[str, str, str, str]]:
    rows = []
    ps = GW / "packaging/windows/install.ps1"
    text = ps.read_text()
    for m in re.finditer(r"Set-ItemProperty \$k -Name (\w+) -Value '([^']*)' -Type (\w+)", text):
        rows.append(("HKLM\\SOFTWARE\\Helios", m.group(1), f"{m.group(3)} \"{m.group(2)}\"",
                     f"{rel(ps)}:{line_of(text, m.start())}"))
    inx = GW / "kmd_render/helios_kmd_render.inx"
    text = inx.read_text()
    sections = {"Helios_Service_AddReg": "service key", "Helios_PCI_MSIX": "device key"}
    for sec, where in sections.items():
        sm = re.search(r"^\[" + sec + r"\]\n(.*?)(?=^\[)", text, re.S | re.M)
        if not sm:
            continue
        for lm in re.finditer(r"^HKR\s*,\s*([^,]*),\s*(\w+)\s*,\s*%(\w+)%\s*,\s*([^\n;]+)", sm.group(1), re.M):
            sub = lm.group(1).strip().strip('"')
            rows.append((f"{where}{chr(92) + sub if sub else ''}", lm.group(2), f"{lm.group(3)} {lm.group(4).strip()}",
                         f"{rel(inx)}:{line_of(text, sm.start(1) + lm.start())}"))
    return rows


# ---------------------------------------------------------------------------
# Driver-written names (state, counters, mirrors)


def kmd_written_names() -> set[str]:
    names = set()
    pat = re.compile(r'record_named_[a-z0-9_]*\(\s*b"([A-Za-z0-9_]+)"')
    for f in rust_files(KMD_SRC):
        names.update(pat.findall(strip_line_comments(f.read_text())))
    text = (KMD_SRC / "diag.rs").read_text()
    names.update(re.findall(r'FaultCounter::\w+ => b"([A-Za-z0-9_]+)"', text))
    return names


# ---------------------------------------------------------------------------
# Rendering


def md_escape(s: str) -> str:
    return s.replace("|", "\\|").replace("\n", " ").replace("<", "&lt;").replace(">", "&gt;")


def code_span(s: str) -> str:
    s = s.replace("|", "\\|")
    return f"`{s}`" if "`" not in s else s


def source_links(sources: list[str], limit: int = 3) -> str:
    links = []
    for s in sources[:limit]:
        path, _, line = s.rpartition(":")
        short = path.split("/")[-1] + ":" + line
        links.append(f"[{short}](../{path}#L{line})")
    more = len(sources) - limit
    if more > 0:
        links.append(f"+{more}")
    return ", ".join(links)


def default_cell(k: Knob, d: dict) -> str:
    vals = k.defaults or ["(not read)"]
    cell = " / ".join(code_span(v) for v in vals)
    if len(vals) > 1:
        cell += " (differs by read site)"
    if d.get("default_note"):
        cell += f"; {md_escape(d['default_note'])}"
    return cell


SECTION_TITLES = {
    KMD_SECTION: (
        "KMD: service key",
        "`HKLM\\SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render`, REG_DWORD. Read by the kernel driver "
        "(`helios_kmd_render.sys`); most are read again at every StartDevice, so `pnputil /restart-device` "
        "applies them, the rest need a reboot (the Read column says which).",
    ),
    UMD_SECTION: (
        "UMDs and ICD policy: `HKLM\\SOFTWARE\\Helios`",
        "Read by the D3D11/D3D12 user-mode drivers, their DXVK/vkd3d-proton bridges, the shared ICD policy "
        "(`protocol/include/helios_icd_policy.h`, also compiled into NVK) and the install scripts. 64-bit view, "
        "also from WoW64 processes. Read once per process unless the Read column says otherwise: a running "
        "process (DWM included) keeps what it started with.",
    ),
    ENV_SECTION: (
        "Environment variables",
        "Per-process: they act only on the process whose environment carries them (and its children). "
        "None needs to be set; the code defaults are the tested configuration. `unset` means the code tests "
        "for presence or a value and does nothing without it.",
    ),
}


def render(inv: Inventory, desc: dict, host, install, sets, written: set[str]) -> str:
    out = []
    w = out.append
    w("<!-- Generated by guest/windows/tools/gen-windows-knobs.py from the code and")
    w("     guest/windows/tools/windows-knobs.toml. Do not edit: edit those and rerun the script. -->")
    w("")
    w("# Windows guest knobs")
    w("")
    w("Every setting the Windows guest stack reads: the kernel driver's service-key values, the user-mode")
    w("drivers' `HKLM\\SOFTWARE\\Helios` values, the environment variables of the UMDs, bridges and our Mesa,")
    w("DXVK and vkd3d-proton patches, and the host's `conduit config` keys that shape what the guest sees.")
    w("Names, types, defaults and source lines come from the code; this page is regenerated and checked")
    w("in CI, so a default that changes in code changes here.")
    w("")
    w("## Recommended configuration")
    w("")
    w("**Recommended configuration = code defaults; no registry values need to be set.** Every knob below")
    w("defaults to the tested configuration when its value is absent. Values left over from a test (an A/B")
    w("row, a bisect) keep acting until deleted: to return a guest to the recommended configuration, delete")
    w("every value this page lists from both keys except the ones the installation writes (next section),")
    w("then reboot.")
    w("")
    w("Classes:")
    w("")
    w("- **Supported setting**: a choice a user may make (for example which ICD the desktop composes on).")
    w("- **Performance tuning**: changes speed or latency, not behaviour; the default is the measured best.")
    w("- **Debug / diagnostic**: tracing, logging, counters, kill switches for bisecting. Off in normal use.")
    w("- **Experiment, not for users**: a path under development or an A/B arm. Leave at the default.")
    w("- **State written by the driver**: values the KMD persists for itself; not settings.")
    w("")
    counts: dict[str, dict[str, int]] = {}
    for (sec, name), k in inv.knobs.items():
        cls = desc[sec][name]["class"]
        counts.setdefault(sec, {}).setdefault(cls, 0)
        counts[sec][cls] += 1
    w("| Location | " + " | ".join(CLASSES[c] for c in CLASS_ORDER) + " | Total |")
    w("|---|" + "---:|" * (len(CLASS_ORDER) + 1))
    totals = {c: 0 for c in CLASS_ORDER}
    for sec in (KMD_SECTION, UMD_SECTION, ENV_SECTION):
        row = counts.get(sec, {})
        for c in CLASS_ORDER:
            totals[c] += row.get(c, 0)
        w(f"| {SECTION_TITLES[sec][0]} | " + " | ".join(str(row.get(c, 0)) for c in CLASS_ORDER)
          + f" | {sum(row.values())} |")
    w("| All | " + " | ".join(str(totals[c]) for c in CLASS_ORDER) + f" | {sum(totals.values())} |")
    w("")
    w("## What the installation writes")
    w("")
    w("These are the only values an installation sets. Everything else on this page is absent after a clean")
    w("install, which means the code default.")
    w("")
    w("| Key | Value | Data | Written by |")
    w("|---|---|---|---|")
    for key, name, data, src in install:
        w(f"| `{key}` | `{name}` | {code_span(data)} | {source_links([src])} |")
    w("")
    w("`install.ps1 -VenusDesktop` deletes `DwmIcd` instead (DWM then composes on Venus). The INF also writes")
    w("the display class values Windows needs to load the UMDs, the Vulkan and OpenGL ICDs")
    w("(`UserModeDriverName`, `VulkanDriverName`, `OpenGLDriverName`, ...); those are driver registration,")
    w("not knobs.")
    w("")

    for sec in (KMD_SECTION, UMD_SECTION, ENV_SECTION):
        title, intro = SECTION_TITLES[sec]
        w(f"## {title}")
        w("")
        w(intro)
        w("")
        items = sorted((k for (s, _), k in inv.knobs.items() if s == sec), key=lambda k: k.name.lower())
        for cls in CLASS_ORDER:
            group = [k for k in items if desc[sec][k.name]["class"] == cls]
            if not group:
                continue
            w(f"### {CLASSES[cls]} ({len(group)})")
            w("")
            if sec == ENV_SECTION:
                w("| Name | Default (code) | Read | Effect | Source |")
                w("|---|---|---|---|---|")
            else:
                w("| Name | Type | Default (code) | Read | Effect | Source |")
                w("|---|---|---|---|---|---|")
            for k in group:
                d = desc[sec][k.name]
                effect = md_escape(d["effect"])
                if d.get("see"):
                    effect += f" See {d['see']}."
                extra = []
                if sec == KMD_SECTION and k.name in written and cls != "state":
                    extra.append("the driver also writes this name (the value in force), so a value it once "
                                 "wrote stays set until deleted")
                if sec == ENV_SECTION and k.name in sets:
                    vals = sorted({v for v, _ in sets[k.name]})
                    extra.append("our code sets it for the process (" + ", ".join(f'"{md_escape(v)}"' for v in vals)
                                 + "): " + source_links([src for _, src in sets[k.name]]))
                for text, src in k.notes:
                    extra.append(f"{text}: {source_links([src])}")
                if extra:
                    effect += " *Note: " + "; ".join(extra) + ".*"
                read = md_escape(d.get("read", ""))
                if sec == ENV_SECTION:
                    w(f"| `{k.name}` | {default_cell(k, d)} | {read} | {effect} | {source_links(k.sources)} |")
                else:
                    w(f"| `{k.name}` | {k.type} | {default_cell(k, d)} | {read} | {effect} | {source_links(k.sources)} |")
            w("")
        if sec == KMD_SECTION:
            w("### Counters and mirrors written by the driver")
            w("")
            w(f"Besides the state values above, the KMD writes {len(written)} counter and mirror values")
            w("(`*Eff` values of the knob in force, fault counters, stage and latency statistics) into the same")
            w("key. They are output, not settings: setting one has no effect, and the driver overwrites it.")
            w("[TRACING.md](TRACING.md) explains the ones used for diagnosis.")
            w("")

    w("## Host: `conduit config` keys that shape the guest")
    w("")
    w("Set on the host with `conduit config set KEY VALUE` ([CLI.md](CLI.md)); stored in")
    w("`~/.config/conduit/config.json`. The text is the key's own help from `cli/src/config.rs`.")
    w("")
    w("| Key | Named values | What it does (default) |")
    w("|---|---|---|")
    for key, vals, text in host:
        w(f"| `{key}` | {md_escape(vals) or 'see text'} | {md_escape(text)} |")
    w("")
    w("Background: [HOST-TUNING.md](HOST-TUNING.md) (`backend.gpu_timeslice_us`, `host.isolation`,")
    w("`backend.cpus`), [VIEWER.md](VIEWER.md) (`viewer.gpu`), [VENUS.md](VENUS.md) (`venus.guest_blobs`).")
    w("")
    return "\n".join(out)


# ---------------------------------------------------------------------------
# Audit: "(default N)" claims in docs and comments against the code


def audit(inv: Inventory) -> list[str]:
    findings = []
    files = [p for p in ROOT.rglob("*.md") if "third_party" not in p.parts and "target" not in p.parts
             and p != OUT and ".git" not in p.parts]
    files += rust_files(KMD_SRC, *UMD_DIRS)
    by_name: dict[str, list[Knob]] = {}
    for k in inv.knobs.values():
        if k.section != ENV_SECTION or "_" in k.name:
            by_name.setdefault(k.name, []).append(k)
    names = sorted(by_name, key=len, reverse=True)
    alt = "|".join(re.escape(n) for n in names)
    val = r"(0x[0-9a-fA-F]+|[0-9][0-9_]*|on|off|ON|OFF|true|false)\b"
    pats = [
        re.compile(r"`?\b(" + alt + r")\b`?\s*\((?:default|absent)[ :=]*\s*" + val),
        re.compile(r"`?\b(" + alt + r")\b`?[^.\n|(]{0,30}?\bdefaults? (?:to |is |= |: )?" + val),
        re.compile(r"`?\b(" + alt + r")\b`?\s*=\s*" + val + r"\s*\((?:the )?default\)"),
    ]
    norm = {"on": "1", "true": "1", "off": "0", "false": "0"}

    def agrees(claim: str, k: Knob) -> bool | None:
        vals = [v.split()[0].lower() for v in k.defaults if v]
        if not vals or not all(re.fullmatch(r"(0x[0-9a-f]+|-?[0-9]+|true|false)", v) for v in vals):
            return None
        c = norm.get(claim.lower(), claim.lower()).replace("_", "")
        for v in vals:
            v2 = norm.get(v, v)
            try:
                if int(c, 0) == int(v2, 0):
                    return True
            except ValueError:
                if c == v2:
                    return True
        return False

    def report(f, text, pos, name, claim):
        for k in by_name[name]:
            if agrees(claim, k) is False:
                findings.append(f"{rel(f)}:{line_of(text, pos)}: {name} claims default {claim}, "
                                f"code: {' / '.join(k.defaults)}")

    for f in files:
        text = f.read_text(errors="replace")
        for pat in pats:
            for m in pat.finditer(text):
                report(f, text, m.start(), m.group(1), m.group(2))
        if f.suffix != ".md":
            continue
        # Markdown tables with a "default" column.
        lines = text.split("\n")
        col = None
        for i, line in enumerate(lines):
            if not line.startswith("|"):
                col = None
                continue
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if col is None:
                low = [c.lower() for c in cells]
                col = next((j for j, c in enumerate(low) if c.startswith("default")), None)
                continue
            if col >= len(cells) or set(cells[0]) <= set("-: "):
                continue
            nm = re.search(r"`(" + alt + r")`", cells[0])
            vm = re.match(r"`?" + val, cells[col])
            if nm and vm:
                pos = sum(len(l) + 1 for l in lines[:i])
                report(f, text, pos, nm.group(1), vm.group(1))
    return sorted(set(findings))


# ---------------------------------------------------------------------------


def build() -> tuple[Inventory, list[str], dict[str, list[str]]]:
    inv = Inventory()
    problems: list[str] = []
    sets: dict[str, list[str]] = {}
    kmd = extract_kmd(inv)
    extract_umd(inv, set(kmd.values()))
    extract_env_tree(inv, sets)
    extract_env_patches(inv, "nvk", nvk_series())
    extract_env_patches(inv, "dxvk", series_from_file(GW / "third_party/patches/dxvk"))
    extract_env_patches(inv, "vkd3d", series_from_file(GW / "third_party/patches/vkd3d-proton"))
    extract_env_patches(inv, "venus", series_from_file(GW / "icd/patches"))
    extract_env_mesa(inv, problems)
    return inv, problems, sets


def strip_lines(doc: str) -> str:
    return re.sub(r"(\[[^\]]+):\d+\]\(([^)#]+)#L\d+\)", r"\1]\(\2)", doc)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--audit", action="store_true")
    ap.add_argument("--refresh-mesa", action="store_true")
    ap.add_argument("--mesa-dir", help="a checkout of the Mesa fork at the pinned commit (default: the submodule)")
    ap.add_argument("--skeleton", action="store_true", help="print TOML stubs for undescribed knobs")
    args = ap.parse_args()

    if args.refresh_mesa:
        refresh_mesa_cache(args.mesa_dir)

    inv, problems, sets = build()
    desc = tomllib.loads(DESC.read_text()) if DESC.exists() else {}
    for sec in (KMD_SECTION, UMD_SECTION, ENV_SECTION):
        desc.setdefault(sec, {})

    missing = [(s, n) for (s, n) in inv.knobs if n not in desc[s]]
    stale = [(s, n) for s in (KMD_SECTION, UMD_SECTION, ENV_SECTION) for n in desc[s] if (s, n) not in inv.knobs]
    for s, n in sorted(missing):
        problems.append(f"[{s}] {n}: read in code ({inv.knobs[(s, n)].sources[0]}) but not described in {DESC.name}")
    for s, n in sorted(stale):
        problems.append(f"[{s}] {n}: described in {DESC.name} but no longer read in code")
    for s in desc:
        for n, d in desc[s].items():
            if d.get("class") not in CLASSES:
                problems.append(f"[{s}] {n}: class must be one of {', '.join(CLASSES)}")
            if not d.get("effect"):
                problems.append(f"[{s}] {n}: no effect text")

    if args.skeleton:
        for s, n in sorted(missing):
            k = inv.knobs[(s, n)]
            print(f'[{s}.{n}]\nclass = ""\nread = ""\neffect = ""\n# default {k.defaults} {k.sources[:2]}\n')
        return 0

    if args.audit:
        for line in audit(inv):
            print(line)
        return 0

    if problems:
        for p in problems:
            print("error:", p, file=sys.stderr)
        return 1

    doc = render(inv, desc, extract_host(), extract_install(), sets, kmd_written_names())
    if args.check:
        current = OUT.read_text() if OUT.exists() else ""
        # Source line numbers move with every edit around a knob; only names,
        # defaults and descriptions make the doc wrong. Drifted line numbers are
        # refreshed by the next regeneration.
        if strip_lines(current) != strip_lines(doc):
            print(f"error: {rel(OUT)} is stale: run guest/windows/tools/gen-windows-knobs.py", file=sys.stderr)
            return 1
        drift = "" if current == doc else " (source line numbers have moved; regenerate when convenient)"
        print(f"{rel(OUT)} is current ({len(inv.knobs)} knobs){drift}")
        return 0
    OUT.write_text(doc)
    print(f"wrote {rel(OUT)} ({len(inv.knobs)} knobs)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

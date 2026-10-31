#!/usr/bin/env python3
"""Generate tools/nvtables.json.gz for mmiodump.py from NVIDIA's
open-gpu-kernel-modules sources (tag 610.57.04).

Usage:
  gen_tables.py [--src DIR] [--out FILE]

--src is an extracted source tree (default: ../ref, made with
  git -C /steam/refsrc/open-gpu-kernel-modules archive 610.57.04 src/... | tar x).
If --src does not exist, the needed paths are taken straight from the git
repo given by --git (default /steam/refsrc/open-gpu-kernel-modules) at --tag.

Tables:
  regs    BAR0 offset -> [name, source header]   (Blackwell first, then
          older architectures as fallback for unchanged blocks)
  ranges  [lo, hi, name] device ranges ("/* RW--D */" defines)
  pages   4 KiB page -> most common register prefix in it (block names)
  rpc     RPC function id -> name (NV_VGPU_MSG_FUNCTION_* / _EVENT_*)
  classes class id -> name (sdk class/clXXXX.h)
  ctrls   RM control cmd -> name (sdk ctrl/**)
"""
import argparse
import collections
import gzip
import io
import json
import os
import re
import subprocess
import sys
import tarfile

HERE = os.path.dirname(os.path.abspath(__file__))

# Register headers in priority order: the first definition of an offset wins.
ARCH_DIRS = [
    "blackwell/gb202", "blackwell/gb20b", "blackwell/gb20c",
    "blackwell/gb100", "blackwell/gb102", "blackwell/gb110",
    "blackwell/gb112", "blackwell/gb10b",
    "hopper/gh100", "ada/ad102", "ampere/ga102", "ampere/ga100",
    "turing/tu102", "disp",
]
SWREF = "src/common/inc/swref/published"
RPC_ENUMS = "src/nvidia/inc/kernel/vgpu/rpc_global_enums.h"
CLASS_DIR = "src/common/sdk/nvidia/inc/class"
CTRL_DIR = "src/common/sdk/nvidia/inc/ctrl"
NEEDED = [SWREF, RPC_ENUMS, CLASS_DIR, CTRL_DIR]

RE_REG = re.compile(
    r"^#define\s+(NV_\w+)\s+(0x[0-9a-fA-F]+)\s*/\*\s*([RW-][W-][-A-Z][1248][RA])\s*\*/")
RE_ARR = re.compile(
    r"^#define\s+(NV_\w+)\(([a-z,\s]+)\)\s+\(?\(?\s*(0x[0-9a-fA-F]+)\s*\+"
    r"([^/]*?)\)?\s*/\*\s*([RW-][W-][-A-Z][1248][RA])\s*\*/")
RE_TERM = re.compile(r"\(?\s*\(?([a-z])\)?\s*\*\s*\(?(0x[0-9a-fA-F]+|\d+)\)?\s*\)?")
RE_SIZE = re.compile(r"^#define\s+(NV_\w+)__SIZE_([12])\s+(\d+|0x[0-9a-fA-F]+)")
RE_RANGE = re.compile(
    r"^#define\s+(NV_\w+)\s+(0x[0-9a-fA-F]+):(0x[0-9a-fA-F]+)\s*/\*\s*RW--D\s*\*/")
RE_X = re.compile(r"^\s*X\(\s*(\w+)\s*,\s*(\w+)\s*,\s*(\w+)\s*\)")
RE_E = re.compile(r"^\s*E\(\s*(\w+)\s*,\s*(\w+)\s*\)")
RE_DEF_HEX = re.compile(r"^#define\s+(\w+)\s+\(?(0x[0-9a-fA-F]+)U?\)?(\s|$)")


class Tree:
    """Read files either from a directory or from a git tag."""

    def __init__(self, src, git, tag):
        self.src = src if src and os.path.isdir(src) else None
        self.files = {}
        if not self.src:
            data = subprocess.run(
                ["git", "-C", git, "archive", "--format=tar", tag] + NEEDED,
                check=True, capture_output=True).stdout
            with tarfile.open(fileobj=io.BytesIO(data)) as tf:
                for m in tf.getmembers():
                    if m.isfile():
                        self.files[m.name] = tf.extractfile(m).read().decode(
                            "utf-8", "replace")

    def list(self, prefix):
        if self.src:
            base = os.path.join(self.src, prefix)
            out = []
            for root, _, names in os.walk(base):
                for n in names:
                    out.append(os.path.relpath(os.path.join(root, n), self.src))
            return sorted(out)
        return sorted(f for f in self.files if f.startswith(prefix + "/"))

    def read(self, path):
        if self.src:
            with open(os.path.join(self.src, path), encoding="utf-8",
                      errors="replace") as f:
                return f.read()
        return self.files[path]


# Headers whose offsets are relative to an engine/structure, not BAR0.
EXCLUDE = {
    "dev_runlist.h", "dev_ctxsw_prog.h", "dev_esched_pbdma.h", "dev_ce_base.h",
    "dev_ram.h", "dev_mmu.h", "dev_fault.h", "kind_macros.h", "hwproject.h",
    "dev_access_counter.h", "dev_fbif_v4.h", "dev_perf.h", "dev_ltc_zb.h",
    "dev_hshub_zb.h", "dev_fuse_zb.h", "dev_nv_xve3g_vf.h", "dev_ctrl.h",
}
# Falcon-relative headers: replicated at each falcon's base.
FALCONS = [("GSP", 0x110000), ("FSP", 0x8f0000), ("SEC", 0x840000),
           ("PWR", 0x10a000)]
FALCON_REL = {
    # header: (name prefix, extra offset from the falcon base, new infix)
    "dev_falcon_v4.h": ("NV_PFALCON_FALCON_", 0x0, "FALCON_"),
    "dev_falcon_second_pri.h": ("NV_PFALCON2_FALCON_", 0x1000, "FALCON2_"),
    "dev_riscv_pri.h": ("NV_PRISCV_RISCV_", 0x1000, "RISCV_"),
}
# PCI config space headers: visible in BAR0 through the NV_PCFG mirror.
PCFG_BASE = 0x88000
PCFG_REL = {"dev_pcfg_pf0.h", "dev_xtl_ep_pcfg_gpu.h", "dev_nv_xve.h"}
# VF/USERMODE registers: on the PF they sit at NV_VIRTUAL_FUNCTION_FULL_PHYS_OFFSET.
VF_REL = {"dev_vm.h"}
VF_FULL_PHYS = 0xb80000


def _header_regs(text):
    """Yield (name, offset) for every register / array element in a header."""
    sizes = {}
    for line in text.splitlines():
        m = RE_SIZE.match(line)
        if m:
            sizes[(m.group(1), int(m.group(2)))] = int(m.group(3), 0)
    for line in text.splitlines():
        m = RE_REG.match(line)
        if m:
            yield m.group(1), int(m.group(2), 16)
            continue
        m = RE_ARR.match(line)
        if not m:
            continue
        name, params, base = m.group(1), m.group(2), int(m.group(3), 16)
        params = [p.strip() for p in params.split(",")]
        strides = {v: int(st, 0) for v, st in RE_TERM.findall(m.group(4))}
        if len(params) not in (1, 2) or any(p not in strides for p in params):
            continue
        n1 = min(sizes.get((name, 1), 16), 4096)
        n2 = min(sizes.get((name, 2), 1), 256) if len(params) == 2 else 1
        for i in range(n1):
            for j in range(n2):
                off = base + i * strides[params[0]]
                label = f"{name}({i})"
                if len(params) == 2:
                    off += j * strides[params[1]]
                    label = f"{name}({i},{j})"
                yield label, off


def parse_regs(tree):
    regs = {}
    ranges = []

    def add(off, name, src):
        if 0 <= off < (1 << 26):
            regs.setdefault(off, [name, src])

    for d in ARCH_DIRS:
        for path in tree.list(f"{SWREF}/{d}"):
            fname = os.path.basename(path)
            if not fname.endswith(".h") or fname in EXCLUDE:
                continue
            src = path[len(SWREF) + 1:]
            text = tree.read(path)
            for line in text.splitlines():
                m = RE_RANGE.match(line)
                if m:
                    hi, lo = int(m.group(2), 16), int(m.group(3), 16)
                    if lo <= hi < (1 << 26):
                        ranges.append([lo, hi, m.group(1), src])
            for name, off in _header_regs(text):
                if fname in FALCON_REL:
                    prefix, extra, infix = FALCON_REL[fname]
                    if not name.startswith(prefix) or off >= 0x1000:
                        continue
                    for unit, base in FALCONS:
                        add(base + extra + off,
                            f"NV_P{unit}_{infix}{name[len(prefix):]}", src)
                elif fname in PCFG_REL:
                    if off < 0x1000:
                        add(PCFG_BASE + off, "NV_PCFG+" + name, src)
                elif fname in VF_REL:
                    if off < 0x40000:
                        add(VF_FULL_PHYS + off, name + "@FULL_PHYS", src)
                    else:
                        add(off, name, src)
                else:
                    add(off, name, src)
    seen = set()
    uniq = []
    for lo, hi, name, src in ranges:
        if (lo, hi, name) not in seen:
            seen.add((lo, hi, name))
            uniq.append([lo, hi, name])
    uniq.append([PCFG_BASE, PCFG_BASE + 0xfff, "NV_PCFG(config mirror)"])
    uniq.append([VF_FULL_PHYS, VF_FULL_PHYS + 0x3ffff, "NV_VIRTUAL_FUNCTION_FULL_PHYS(usermode)"])
    return regs, uniq


def page_prefixes(regs):
    per = collections.defaultdict(collections.Counter)
    for off, (name, _) in regs.items():
        parts = name.split("_")
        prefix = "_".join(parts[:2]) if len(parts) > 1 else name
        per[off >> 12][prefix] += 1
    return {p: c.most_common(1)[0][0] for p, c in per.items()}


def parse_rpc(tree):
    rpc = {}
    for line in tree.read(RPC_ENUMS).splitlines():
        m = RE_X.match(line)
        if m:
            rpc[int(m.group(3), 0)] = m.group(2)
            continue
        m = RE_E.match(line)
        if m:
            rpc[int(m.group(2), 0)] = "EVENT_" + m.group(1)
    return rpc


def parse_classes(tree):
    classes = {}
    for path in tree.list(CLASS_DIR):
        m = re.match(r".*/cl([0-9a-fA-F]{4})\.h$", path)
        if not m:
            continue
        want = int(m.group(1), 16)
        for line in tree.read(path).splitlines():
            d = RE_DEF_HEX.match(line)
            if d and int(d.group(2), 16) == want and \
                    not d.group(1).endswith(("_MESSAGE_ID", "_TYPEDEF")) and \
                    "_CTRL_" not in d.group(1):
                classes.setdefault(want, d.group(1))
    return classes


def parse_ctrls(tree):
    ctrls = {}
    for path in tree.list(CTRL_DIR):
        if not path.endswith(".h"):
            continue
        for line in tree.read(path).splitlines():
            d = RE_DEF_HEX.match(line)
            if d and "_CTRL_CMD_" in d.group(1):
                v = int(d.group(2), 16)
                if v > 0xffff:
                    ctrls.setdefault(v, d.group(1))
    return ctrls


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", default=os.path.join(HERE, "..", "ref"))
    ap.add_argument("--git", default="/steam/refsrc/open-gpu-kernel-modules")
    ap.add_argument("--tag", default="610.57.04")
    ap.add_argument("--out", default=os.path.join(HERE, "nvtables.json.gz"))
    a = ap.parse_args()

    tree = Tree(a.src, a.git, a.tag)
    regs, ranges = parse_regs(tree)
    tables = {
        "source": f"open-gpu-kernel-modules {a.tag}",
        "regs": {f"{k:x}": v for k, v in sorted(regs.items())},
        "ranges": ranges,
        "pages": {f"{k:x}": v for k, v in sorted(page_prefixes(regs).items())},
        "rpc": {str(k): v for k, v in sorted(parse_rpc(tree).items())},
        "classes": {f"{k:x}": v for k, v in sorted(parse_classes(tree).items())},
        "ctrls": {f"{k:x}": v for k, v in sorted(parse_ctrls(tree).items())},
    }
    with gzip.open(a.out, "wt", encoding="utf-8") as f:
        json.dump(tables, f, separators=(",", ":"), sort_keys=True)
    print(f"{a.out}: {len(tables['regs'])} regs, {len(ranges)} ranges, "
          f"{len(tables['rpc'])} rpc ids, {len(tables['classes'])} classes, "
          f"{len(tables['ctrls'])} ctrl cmds", file=sys.stderr)


if __name__ == "__main__":
    main()

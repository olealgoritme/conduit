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
SDK_INC = "src/common/sdk/nvidia/inc"
RPC_C = "src/nvidia/src/kernel/vgpu/rpc.c"
RES_LIST = "src/nvidia/src/kernel/rmapi/resource_list.h"
STATUS_H = "src/common/sdk/nvidia/inc/nvstatuscodes.h"
# Headers with structures (parsed in this order; the first definition wins)
STRUCT_DIRS = [SDK_INC]
STRUCT_FILES = [
    "src/nvidia/inc/kernel/vgpu/sdk-structures.h",
    "src/nvidia/inc/kernel/vgpu/rpc_headers.h",
    "src/nvidia/generated/g_sdk-structures.h",
    "src/nvidia/generated/g_rpc-structures.h",
    "src/nvidia/inc/kernel/gpu/gsp/gsp_static_config.h",
    "src/nvidia/inc/kernel/gpu/gpu_acpi_data.h",
    "src/nvidia/inc/kernel/platform/chipset/chipset.h",
    "src/nvidia/inc/kernel/gpu/nvbitmask.h",
    "src/nvidia/inc/kernel/gpu/gpu_engine_type.h",
    "src/nvidia/inc/kernel/gpu/gsp/gsp_init_args.h",
    "src/nvidia/arch/nvalloc/common/inc/gsp/gspifpub.h",
    "src/nvidia/arch/nvalloc/common/inc/gsp/gsp_fw_wpr_meta.h",
    "src/common/uproc/os/common/include/libos_init_args.h",
    "src/nvidia/generated/g_chipset_nvoc.h",
    "src/nvidia/generated/g_os_nvoc.h",
]
NEEDED = [SWREF, RPC_ENUMS, SDK_INC, RPC_C, RES_LIST] + STRUCT_FILES
# Display classes whose methods are decoded (GB20x: NVCA7x; older as fallback)
DISP_CLASSES = ["ca7d", "ca7e", "ca7b", "ca7a", "cb7d", "cb7e", "cb7b", "cb7a",
                "c97d", "c97e", "c67d", "c67e", "c57d", "c57e", "c37d", "c37e"]
# Engine classes for pushbuffer method names: host (GPFIFO), 3D, compute,
# copy, video. Blackwell's 3D/compute headers are not published with
# methods; the decoder falls back to the newest class of the same family.
ENGINE_CLASSES = ["ca6f", "c96f", "c86f", "c76f", "c56f", "c46f", "c36f",
                  "ce97", "cd97", "cb97", "c997", "c797", "c697", "c597",
                  "cec0", "cdc0", "cbc0", "c9c0", "c7c0", "c6c0", "c5c0",
                  "cab5", "c9b5", "c8b5", "c7b5", "c6b5", "c5b5",
                  "cefa", "cdfa", "c9fa", "c7fa"]
# RPCs whose parameters are not an rpc_<name>_v structure
RPC_PARAM_OVERRIDE = {
    "GSP_SET_SYSTEM_INFO": "GspSystemInfo",
    "GET_GSP_STATIC_INFO": "GspStaticConfigInfo",
    "SET_REGISTRY": "PACKED_REGISTRY_TABLE",
}

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
    """Read files from a git tag (preferred) or from an extracted directory.

    The extracted ref/ tree may be partial, so the git tag is used whenever
    the repository is there; --src only serves as the fallback.
    """

    def __init__(self, src, git, tag):
        self.src = None
        self.files = {}
        if git and os.path.isdir(git):
            data = subprocess.run(
                ["git", "-C", git, "archive", "--format=tar", tag] + NEEDED,
                check=True, capture_output=True).stdout
            with tarfile.open(fileobj=io.BytesIO(data)) as tf:
                for m in tf.getmembers():
                    if m.isfile():
                        self.files[m.name] = tf.extractfile(m).read().decode(
                            "utf-8", "replace")
        elif src and os.path.isdir(src):
            self.src = src
        else:
            sys.exit(f"no source: neither {git} nor {src}")

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

    def exists(self, path):
        if self.src:
            return os.path.isfile(os.path.join(self.src, path))
        return path in self.files


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
    """Class id -> name: the define matching the header's number, then the
    other external class names RM's resource_list.h knows (aliases such as
    NV01_MEMORY_LIST_SYSTEM in cl84a0.h)."""
    classes = {}
    ext = set()
    if tree.exists(RES_LIST):
        ext = set(re.findall(r"External Class\s*\*/\s*(\w+),",
                             tree.read(RES_LIST)))
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
    for path in tree.list(CLASS_DIR):
        for line in tree.read(path).splitlines():
            d = RE_DEF_HEX.match(line)
            if d and d.group(1) in ext:
                classes.setdefault(int(d.group(2), 16), d.group(1))
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


def parse_structs(tree):
    """Structure layouts + constants from the SDK, vGPU/RPC and GSP headers."""
    sys.path.insert(0, HERE)
    import cstructs
    consts = cstructs.Consts()
    files = []
    for d in STRUCT_DIRS:
        files += [f for f in tree.list(d) if f.endswith(".h")]
    files += [f for f in STRUCT_FILES if tree.exists(f)]
    texts = [tree.read(f) for f in files]
    for t in texts:
        consts.add_text(t)
    p = cstructs.Parser(consts)
    for t in texts:
        p.add_text(t)
    return p.layout_all(), consts


def parse_rpc_params(tree, rpc, structs):
    """RPC function id -> parameter structure name."""
    out = {}
    pairs = {}
    if tree.exists(RPC_C):
        for fn, st in re.findall(r"NV_VGPU_MSG_FUNCTION_(\w+),\s*sizeof\((\w+)\)",
                                 tree.read(RPC_C)):
            if st in structs and not st.endswith("_v"):
                pairs.setdefault(fn, st)
    for fid, name in rpc.items():
        base = name[6:] if name.startswith("EVENT_") else name
        cand = [RPC_PARAM_OVERRIDE.get(base), pairs.get(base),
                f"rpc_{base.lower()}_v"]
        for c in cand:
            if c and c in structs:
                out[fid] = c
                break
    return out


def parse_ctrl_params(tree, structs):
    """RM control cmd -> parameter structure (from the finn comments)."""
    out = {}
    rx = re.compile(r"#define\s+(\w+_CTRL_CMD_\w+)\s+\((0x[0-9a-fA-F]+)U?\)\s*"
                    r"/\*\s*finn: Evaluated from \"[^\"]*?(\w+)_MESSAGE_ID\)?\"")
    for path in tree.list(CTRL_DIR):
        if not path.endswith(".h"):
            continue
        for name, val, st in rx.findall(tree.read(path)):
            if st in structs:
                out[int(val, 16)] = st
    return out


def parse_alloc_params(tree, classes, structs):
    """Class id -> allocation parameter structure (RM's resource_list.h)."""
    out = {}
    if not tree.exists(RES_LIST):
        return out
    by_name = {v: k for k, v in classes.items()}
    for path in tree.list(CLASS_DIR):     # every name, also aliases
        for line in tree.read(path).splitlines():
            d = RE_DEF_HEX.match(line)
            if d and "_CTRL_" not in d.group(1):
                by_name.setdefault(d.group(1), int(d.group(2), 16))
    text = tree.read(RES_LIST)
    for cls, info in re.findall(
            r"External Class\s*\*/\s*(\w+),.*?Alloc Param Info\s*\*/\s*([^,]+?\)|RS_NONE)",
            text, re.S):
        m = re.match(r"RS_(?:REQUIRED|OPTIONAL)\((\w+)\)", info)
        if not m or cls not in by_name:
            continue
        st = m.group(1)
        if st in structs or st == "NvHandle":
            out.setdefault(by_name[cls], st)
    return out


def parse_disp_methods(tree, classes=DISP_CLASSES):
    """Class -> {method offset: name} from the class headers."""
    out = {}
    rx = re.compile(r"^#define\s+(NV[0-9A-F]{4}_\w+?)(\(([a-z])\))?\s+"
                    r"\(?(0x[0-9A-Fa-f]+)(?:\s*\+\s*\(\3\)\s*\*\s*(0x[0-9A-Fa-f]+))?\)?\s*(?:/\*.*)?$",
                    re.M)
    rfield = re.compile(r"^#define\s+(NV[0-9A-F]{4}_\w+?)(\([a-z]\))?\s+\d+:\d+", re.M)
    for c in classes:
        path = f"{CLASS_DIR}/cl{c}.h"
        if not tree.exists(path):
            continue
        text = tree.read(path)
        fields = set(m.group(1) for m in rfield.finditer(text))
        cands = [m for m in rx.finditer(text)]
        names = set(m.group(1) for m in cands)
        meth = {}
        for m in cands:
            name, idx, base, stride = m.group(1), m.group(3), \
                int(m.group(4), 16), m.group(5)
            if base % 4:
                continue
            if not any(f.startswith(name + "_") for f in fields):
                # a method without fields: parenthesised value and no
                # method or field name is a prefix of it (that would make
                # it a field value)
                parts = name.split("_")
                pre = ["_".join(parts[:k]) for k in range(2, len(parts))]
                if "(0x" not in m.group(0) or \
                        any(x in names or x in fields for x in pre):
                    continue
            if idx and stride:
                cnt = 32 if "WINDOW" in name else 8
                for k in range(cnt):
                    meth.setdefault(base + k * int(stride, 16), f"{name}({k})")
            else:
                meth.setdefault(base, name)
        if meth:
            out[int(c, 16)] = {f"{k:x}": v for k, v in sorted(meth.items())}
    return out


def parse_status(tree):
    out = {}
    if tree.exists(STATUS_H):
        for name, val in re.findall(r"NV_STATUS_CODE\((\w+),\s*(0x[0-9a-fA-F]+)",
                                    tree.read(STATUS_H)):
            out[int(val, 16)] = name
    return out


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
    rpc = parse_rpc(tree)
    classes = parse_classes(tree)
    structs, _ = parse_structs(tree)
    rpc_params = parse_rpc_params(tree, rpc, structs)
    ctrl_params = parse_ctrl_params(tree, structs)
    alloc_params = parse_alloc_params(tree, classes, structs)
    # keep only the structures reachable from the maps (smaller tables)
    keep = set()
    todo = list(rpc_params.values()) + list(ctrl_params.values()) + \
        list(alloc_params.values())
    while todo:
        n = todo.pop()
        if n in keep or n not in structs:
            continue
        keep.add(n)
        st = structs[n]
        if "alias" in st:
            todo.append(st["alias"])
            continue
        todo += [f[5] for f in st["f"] if f[5]]
    tables = {
        "source": f"open-gpu-kernel-modules {a.tag}",
        "regs": {f"{k:x}": v for k, v in sorted(regs.items())},
        "ranges": ranges,
        "pages": {f"{k:x}": v for k, v in sorted(page_prefixes(regs).items())},
        "rpc": {str(k): v for k, v in sorted(rpc.items())},
        "classes": {f"{k:x}": v for k, v in sorted(classes.items())},
        "ctrls": {f"{k:x}": v for k, v in sorted(parse_ctrls(tree).items())},
        "structs": {k: structs[k] for k in sorted(keep)},
        "rpc_params": {str(k): v for k, v in sorted(rpc_params.items())},
        "ctrl_params": {f"{k:x}": v for k, v in sorted(ctrl_params.items())},
        "alloc_params": {f"{k:x}": v for k, v in sorted(alloc_params.items())},
        "disp_methods": {f"{k:x}": v for k, v in
                         sorted(parse_disp_methods(tree).items())},
        "methods": {f"{k:x}": v for k, v in
                    sorted(parse_disp_methods(tree, ENGINE_CLASSES).items())},
        "status": {f"{k:x}": v for k, v in sorted(parse_status(tree).items())},
    }
    with gzip.open(a.out, "wt", encoding="utf-8") as f:
        json.dump(tables, f, separators=(",", ":"), sort_keys=True)
    print(f"{a.out}: {len(tables['regs'])} regs, {len(ranges)} ranges, "
          f"{len(tables['rpc'])} rpc ids ({len(rpc_params)} with params), "
          f"{len(tables['classes'])} classes ({len(alloc_params)} with alloc "
          f"params), {len(tables['ctrls'])} ctrl cmds ({len(ctrl_params)} "
          f"with params), {len(keep)} structs of {len(structs)}, "
          f"{len(tables['disp_methods'])} display classes, "
          f"{len(tables['status'])} status codes", file=sys.stderr)


if __name__ == "__main__":
    main()

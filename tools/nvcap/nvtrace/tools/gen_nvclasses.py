#!/usr/bin/env python3
"""Build tools/nvclasses.json: method names of NVIDIA GPU classes, for
ntrdump.py --pushbuf.

    tools/gen_nvclasses.py --repo /steam/refsrc/open-gpu-kernel-modules --tag 610.57.04 \\
        [--mesa /steam/refsrc/mesa] > tools/nvclasses.json

Classes come from open-gpu-kernel-modules at the tag
(src/common/sdk/nvidia/inc/class/cl*.h). That tree carries only the class
number for the Blackwell 3D and compute classes (clcd97.h, clce97.h,
clcdc0.h, clcec0.h) and few methods for the GPFIFO and copy classes; with
--mesa, NVIDIA's MIT-licensed class headers that Mesa ships in
src/nouveau/headers/nvidia/classes are read too and the header with more
methods wins. The JSON records which file each class came from.

Per class: name, method offset -> [name, fields], array methods (base,
stride, count), fields [name, hi, lo] and enum values per field.
"""

import argparse
import json
import os
import re
import subprocess
import sys

OGKM_CLASSES = ["c96f", "ca6f", "c86f", "c56f", "c46f", "c36f", "c9b5", "cab5", "c8b5", "c7b5", "902d", "c97d",
                "c97e", "ca7d", "ca7e", "cc7d", "cc7e", "c661", "c761", "c96fsw", "ca6fsw"]
MESA_CLASSES = ["cd97", "ce97", "cdc0", "cec0", "cb97", "cbc0", "c997", "c9c0", "c797", "c7c0",
                "c96f", "ca6f", "c86f", "c56f", "c9b5", "cab5", "c8b5", "c7b5", "902d"]

DEF_RE = re.compile(r"^#define\s+(\w+)(\(\s*\w+\s*\))?\s+(.*?)\s*(/\*.*)?$")
NUM_RE = re.compile(r"^\(?\s*(0x[0-9a-fA-F]+|\d+)[uU]?\s*\)?$")
ARR_RE = re.compile(r"^\(?\s*\(?\s*(0x[0-9a-fA-F]+|\d+)[uU]?\s*\)?\s*\+\s*\(?\s*\(?\s*\w+\s*\)?\s*\*\s*(0x[0-9a-fA-F]+|\d+)[uU]?\s*\)?\s*\)?$")
FIELD_RE = re.compile(r"^\(?\s*(\d+)\s*:\s*(\d+)\s*\)?$")
CLASSID_RE = re.compile(r"^#define\s+([A-Z][A-Z0-9_]+)\s+\(?\s*(0x0000[0-9a-fA-F]{4}|0x[0-9a-fA-F]{4})\s*\)?\s*$")


def git(repo, *args):
    return subprocess.run(["git", "-C", repo] + list(args), check=True,
                          capture_output=True).stdout.decode("utf-8", "replace")


def parse(text, clsnum):
    prefix = "NV%04X_" % clsnum
    nums, arrays, fields = {}, {}, {}
    name = None
    for line in text.split("\n"):
        m = CLASSID_RE.match(line)
        if m and int(m.group(2), 16) == clsnum and not m.group(1).startswith("NV"):
            name = name or m.group(1)
        m = DEF_RE.match(line)
        if not m or not m.group(1).startswith(prefix):
            continue
        key = m.group(1)[len(prefix):]
        val = m.group(3).strip()
        if m.group(2):
            a = ARR_RE.match(val)
            if a:
                arrays[key] = (int(a.group(1), 0), int(a.group(2), 0))
            continue
        f = FIELD_RE.match(val)
        if f:
            fields[key] = (int(f.group(1)), int(f.group(2)))
            continue
        n = NUM_RE.match(val)
        if n:
            nums[key] = int(n.group(1), 0)
    # a number is an enum value when its name extends a field name
    field_names = sorted(fields, key=len, reverse=True)

    def owner_field(k):
        for fn in field_names:
            if k.startswith(fn + "_"):
                return fn
        return None

    methods, enums = {}, {}
    for k, v in nums.items():
        fo = owner_field(k)
        if fo:
            enums.setdefault(fo, {})[str(v)] = k[len(fo) + 1:]
            continue
        if "__SIZE" in k or k.endswith("_SIZE_1") or v >= 0x40000 or v % 4:
            continue
        methods.setdefault(v, k)
    names = sorted(set(methods.values()) | set(arrays), key=len, reverse=True)

    def owner_method(fk):
        for mn in names:
            if fk.startswith(mn + "_"):
                return mn
        return None

    mfields = {}
    for fk, (hi, lo) in fields.items():
        mn = owner_method(fk)
        if mn:
            mfields.setdefault(mn, []).append([fk[len(mn) + 1:], hi, lo])
    out_methods = {}
    for v, k in sorted(methods.items()):
        out_methods[str(v)] = [k, sorted(mfields.get(k, []), key=lambda f: -f[1])]
    out_arrays = []
    for k, (base, stride) in sorted(arrays.items(), key=lambda kv: kv[1]):
        count = nums.get(k + "__SIZE_1", 0)
        out_arrays.append([k, base, stride, count, sorted(mfields.get(k, []), key=lambda f: -f[1])])
    out_enums = {}
    for fk, vals in enums.items():
        mn = owner_method(fk)
        if mn and len(vals) <= 64:
            out_enums[fk] = vals
    return name, out_methods, out_arrays, out_enums


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tag", required=True)
    ap.add_argument("--mesa", help="Mesa checkout for the 3D/compute class headers")
    a = ap.parse_args()
    out = {"source": {"open-gpu-kernel-modules": a.tag}, "classes": {}}
    for c in OGKM_CLASSES:
        path = "src/common/sdk/nvidia/inc/class/cl%s.h" % c
        try:
            text = git(a.repo, "show", "%s:%s" % (a.tag, path))
        except subprocess.CalledProcessError:
            continue
        num = int(c[:4], 16)
        name, methods, arrays, enums = parse(text, num)
        if not methods and not arrays:
            continue
        key = "0x%04X" % num if not c.endswith("sw") else "0x%04Xsw" % num
        out["classes"][key] = {"name": name or ("NV%04X" % num), "file": "ogkm:" + path,
                               "methods": methods, "arrays": arrays, "enums": enums}
    if a.mesa:
        mdir = os.path.join(a.mesa, "src/nouveau/headers/nvidia/classes")
        try:
            rev = git(a.mesa, "rev-parse", "--short", "HEAD").strip()
        except subprocess.CalledProcessError:
            rev = "?"
        out["source"]["mesa"] = rev
        for c in MESA_CLASSES:
            p = os.path.join(mdir, "cl%s.h" % c)
            if not os.path.exists(p):
                continue
            with open(p, encoding="utf-8", errors="replace") as f:
                text = f.read()
            num = int(c, 16)
            name, methods, arrays, enums = parse(text, num)
            key = "0x%04X" % num
            have = out["classes"].get(key)
            richer = not have or len(methods) + len(arrays) > len(have["methods"]) + len(have["arrays"])
            if methods and richer:
                out["classes"][key] = {"name": name or ("NV%04X" % num),
                                       "file": "mesa:src/nouveau/headers/nvidia/classes/cl%s.h" % c,
                                       "methods": methods, "arrays": arrays, "enums": enums}
    json.dump(out, sys.stdout, separators=(",", ":"), sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()

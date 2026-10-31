#!/usr/bin/env python3
"""Build the decoder's static name tables.

    tools/gen_tables.py --ntstatus /usr/x86_64-w64-mingw32/include/ntstatus.h \\
        --nvclasses /steam/refsrc/open-gpu-kernel-modules/src/common/sdk/nvidia/inc/class \\
        --nvclasses /steam/refsrc/mesa/src/nouveau/headers/nvidia/classes --out decoder

  ntstatus.json   NTSTATUS value -> STATUS_* name
  nvmethods.json  NVIDIA class (copy engines *b5, host channels *6f, compute *c0)
                  -> method offset -> method name, from the open-gpu-kernel-modules class headers
"""
import argparse
import json
import os
import re


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ntstatus", required=True)
    ap.add_argument("--nvclasses", required=True, action="append",
                    help="class header directory; repeat, earlier ones win per method")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    st = {}
    for line in open(a.ntstatus, encoding="latin-1"):
        m = re.match(r"#define\s+(STATUS_\w+)\s+\(\(NTSTATUS\)\s*(0x[0-9A-Fa-f]+)L?\)", line)
        if m:
            st.setdefault(str(int(m.group(2), 16)), m.group(1))
    json.dump(st, open(os.path.join(a.out, "ntstatus.json"), "w"), indent=0, sort_keys=True)

    meth = {}
    for cdir in a.nvclasses:
        for fn in sorted(os.listdir(cdir)):
            parse_class(cdir, fn, meth)
    json.dump(meth, open(os.path.join(a.out, "nvmethods.json"), "w"), separators=(",", ":"), sort_keys=True)
    print("ntstatus %d, nv classes %d, methods %d" % (len(st), len(meth), sum(len(v) for v in meth.values())))


def parse_class(cdir, fn, meth):
    m = re.match(r"^cl([0-9a-f]{2}(b5|6f|c0|c5))\.h$", fn)
    if not m:
        return
    cls = int(m.group(1), 16)
    pre = "NV%04X_" % cls
    hexdefs, ranges = [], []
    for line in open(os.path.join(cdir, fn), encoding="latin-1"):
        mm = re.match(r"#define\s+(%s\w+)\s+(.*?)\s*$" % pre, line, re.I)
        if not mm:
            continue
        name, val = mm.group(1), mm.group(2).strip()
        if re.match(r"^\(?\s*\d+\s*:\s*\d+\s*\)?$", val):
            ranges.append(name + "_")
        else:
            mv = re.match(r"^\(?\s*(0x[0-9A-Fa-f]+)\s*\)?$", val)
            if mv:
                hexdefs.append((name, int(mv.group(1), 16)))
    tab = {}
    for name, v in hexdefs:
        if v >= 0x4000 or v % 4 or any(name.startswith(r) for r in ranges):
            continue
        tab.setdefault(str(v), name[len(pre):])
    if tab:
        d = meth.setdefault("%x" % cls, {})
        for k, v in tab.items():
            d.setdefault(k, v)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""objmerge.py DIR: pair run-objects.sh's nvstats -s output with the
NVK_SHADER_STATS lines of each batch (one line per shader compiled, in order)"""
import glob
import os
import sys

d = sys.argv[1]
for b in sorted(p for p in glob.glob(f"{d}/batch.*") if "." not in os.path.basename(p)[6:]):
    names = [l.strip() for l in open(b)]
    outs = [l.split() for l in open(b + ".out")] if os.path.exists(b + ".out") else []
    stats = [l.split() for l in open(b + ".st")] if os.path.exists(b + ".st") else []
    # "NVK shader stats: STAGE hash code_B instrs gprs max_warps cycles spills fills ..."
    stats = [x for x in stats if len(x) > 12 and x[3].startswith("MESA_SHADER")]
    k, seen = 0, set()
    for o in outs:
        seen.add(o[0])
        if o[2] == "OK" and k < len(stats):
            x = stats[k]
            k += 1
            print(o[0], o[1], *x[6:12])
        else:
            print(o[0], o[1], "FAILED")
    for n in names:
        if n not in seen:
            print(n, "-", "FAILED")

#!/usr/bin/env python3
"""Cross-check the structure layouts the driver depends on.

Every offset/size the generated capture rules and table wrappers use
(build/gen/layoutcheck.txt) is evaluated twice:
  - by clang targeting x86_64-pc-windows-msvc with the unmodified WDK headers
    (MSVC record layout and enum rules, i.e. what dxgkrnl and nvlddmkm use)
  - by x86_64-w64-mingw32-gcc with the patched headers (what nvddi.sys uses)
Any difference is an error.

Usage: layoutcheck.py --gen build/gen [--cc ...] [--clang clang]
"""

import argparse
import os
import re
import subprocess
import sys

MINGW = "/usr/x86_64-w64-mingw32/include"
PRELUDE = '#include "wdkshim.h"\n#include <ntddk.h>\n#include <dispmprt.h>\n#include <d3dkmddi.h>\n'


def values(cmd, src, obj, n):
    r = subprocess.run(cmd + ["-c", src, "-o", obj], capture_output=True, text=True)
    if r.returncode:
        sys.stderr.write(r.stderr[-6000:])
        sys.exit("compile failed: %s" % cmd[0])
    dump = subprocess.run(["llvm-objdump", "-s", "-j", ".rdata", obj], capture_output=True, text=True).stdout
    data = bytearray()
    for line in dump.split("\n"):
        m = re.match(r"^ ([0-9a-f]{4,}) ((?:[0-9a-f]{2,8} ){1,4})", line)
        if m:
            for w in m.group(2).split():
                data += bytes.fromhex(w)
    return [int.from_bytes(data[i * 8:i * 8 + 8], "little", signed=True) for i in range(n)]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gen", required=True)
    ap.add_argument("--cc", default="x86_64-w64-mingw32-gcc")
    ap.add_argument("--clang", default="clang")
    a = ap.parse_args()
    exprs = [l.strip() for l in open(os.path.join(a.gen, "layoutcheck.txt")) if l.strip()]
    src = os.path.join(a.gen, "layoutcheck.c")
    with open(src, "w") as f:
        f.write(PRELUDE)
        f.write("const long long nvddi_layout[%d] = {\n" % len(exprs))
        for e in exprs:
            f.write("  (long long)(%s),\n" % e)
        f.write("};\n")
    gcc = [a.cc, "-fms-extensions", "-w", "-I" + os.path.join(a.gen, "wdkinc"), "-I" + MINGW + "/ddk"]
    clang = [a.clang, "--target=x86_64-pc-windows-msvc", "-D__GNUC__=4", "-fms-extensions", "-Wno-everything",
             "-I" + os.path.join(a.gen, "clangstub"), "-I" + os.path.join(a.gen, "wdkraw"),
             "-I" + MINGW + "/ddk", "-I" + MINGW]
    vg = values(gcc, src, os.path.join(a.gen, "layoutcheck_gcc.o"), len(exprs))
    vc = values(clang, src, os.path.join(a.gen, "layoutcheck_clang.o"), len(exprs))
    bad = [(e, x, y) for e, x, y in zip(exprs, vg, vc) if x != y]
    for e, x, y in bad:
        print("MISMATCH %s: gcc %d, clang/msvc %d" % (e, x, y))
    print("layoutcheck: %d expressions, %d mismatches" % (len(exprs), len(bad)))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()

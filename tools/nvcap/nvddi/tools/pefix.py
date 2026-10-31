#!/usr/bin/env python3
"""Check and finish a kernel driver image built by GNU ld.

    pefix.py --driver build/nvddi.sys

- verifies: PE32+ AMD64, subsystem NATIVE, base relocations present, dynamic
  base + NX, no writable+executable section, no TLS directory, imports only
  from ntoskrnl.exe / hal.dll
- marks .text/.rdata/.pdata/.xdata non-pageable (clears IMAGE_SCN_MEM_DISCARDABLE
  if a linker set it on them)
- writes the PE checksum (the loader verifies it for boot drivers)
Exit status 1 on any check failure.
"""

import argparse
import struct
import sys

SCN_EXEC, SCN_WRITE, SCN_DISCARD = 0x20000000, 0x80000000, 0x02000000


def checksum(data, off):
    s = 0
    n = len(data)
    padded = data + (b"\0" if n % 2 else b"")
    for i in range(0, len(padded), 2):
        if i == off or i == off + 2:
            continue
        s += struct.unpack_from("<H", padded, i)[0]
        s = (s & 0xFFFF) + (s >> 16)
    s = (s & 0xFFFF) + (s >> 16)
    return (s + n) & 0xFFFFFFFF


def rva_to_off(secs, rva):
    for (name, va, vs, raw, rs, ch) in secs:
        if va <= rva < va + max(vs, rs):
            return raw + (rva - va)
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--driver", required=True)
    a = ap.parse_args()
    d = bytearray(open(a.driver, "rb").read())
    errs = []
    pe = struct.unpack_from("<I", d, 0x3C)[0]
    if d[pe:pe + 4] != b"PE\0\0":
        sys.exit("not a PE file")
    machine, nsec = struct.unpack_from("<HH", d, pe + 4)
    opt = pe + 24
    magic = struct.unpack_from("<H", d, opt)[0]
    if machine != 0x8664 or magic != 0x20B:
        errs.append("not PE32+ AMD64")
    subsystem, dllch = struct.unpack_from("<HH", d, opt + 68)
    if subsystem != 1:
        errs.append("subsystem %d, want 1 (native)" % subsystem)
    if not dllch & 0x40:
        errs.append("no DYNAMIC_BASE")
    if not dllch & 0x100:
        errs.append("no NX_COMPAT")
    nrva = struct.unpack_from("<I", d, opt + 108)[0]
    dirs = [struct.unpack_from("<II", d, opt + 112 + 8 * i) for i in range(min(nrva, 16))]
    if len(dirs) > 5 and dirs[5][1] == 0:
        errs.append("no base relocations")
    if len(dirs) > 9 and dirs[9][1] != 0:
        errs.append("TLS directory present")
    sh = opt + struct.unpack_from("<H", d, pe + 20)[0]
    secs = []
    for i in range(nsec):
        o = sh + 40 * i
        name = d[o:o + 8].rstrip(b"\0").decode("ascii", "replace")
        vs, va, rs, raw = struct.unpack_from("<IIII", d, o + 8)
        ch = struct.unpack_from("<I", d, o + 36)[0]
        if ch & SCN_EXEC and ch & SCN_WRITE:
            errs.append("section %s is writable and executable" % name)
        if ch & SCN_DISCARD and name in (".text", ".rdata", ".data", ".pdata", ".xdata"):
            struct.pack_into("<I", d, o + 36, ch & ~SCN_DISCARD)
        secs.append((name, va, vs, raw, rs, ch))
    # imports
    if len(dirs) > 1 and dirs[1][0]:
        off = rva_to_off(secs, dirs[1][0])
        while off is not None:
            ilt, _, _, name_rva, iat = struct.unpack_from("<IIIII", d, off)
            if name_rva == 0:
                break
            no = rva_to_off(secs, name_rva)
            dll = bytes(d[no:d.index(b"\0", no)]).decode().lower()
            if dll not in ("ntoskrnl.exe", "hal.dll"):
                errs.append("imports from %s" % dll)
            off += 20
    csum_off = opt + 64
    struct.pack_into("<I", d, csum_off, 0)
    struct.pack_into("<I", d, csum_off, checksum(bytes(d), csum_off))
    open(a.driver, "wb").write(d)
    print("pefix: %s sections=%s checksum=0x%08x" % (
        a.driver, ",".join(s[0] for s in secs), struct.unpack_from("<I", d, csum_off)[0]))
    if errs:
        for e in errs:
            print("pefix: ERROR " + e)
        sys.exit(1)


if __name__ == "__main__":
    main()

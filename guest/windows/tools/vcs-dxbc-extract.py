#!/usr/bin/env python3
"""Extract the DXBC shaders of a Source 2 game into a corpus for
d3d11_shader_corpus.exe, and tally their SFI0 requirement flags.

    vcs-dxbc-extract.py --dict vfx_zstd_2bc2fa87.bin OUT_DIR GAME/csgo/shaders_pc_dir.vpk [...]

Reads each *_dir.vpk (VPK v1/v2) and its numbered archives, takes every
*_{vs,ps,gs,hs,ds,cs}.vcs entry, decompresses its zstd frames (the bytecode
frames use Valve's shader dictionary; ValveResourceFormat ships it as
ValveResourceFormat/Utils/vfx_zstd_<id>.bin) with the `zstd` CLI, and writes
each distinct DXBC container to OUT_DIR/<stage>/<shader>_<sha1-16>.dxbc.
Prints the count per (stage, SFI0 flags) and the shaders behind each flag.

The output is game data: keep it on the test machine, never in the repository.
"""
import argparse
import collections
import hashlib
import os
import struct
import subprocess
import sys

STAGES = {"_vs.vcs": "vs", "_ps.vcs": "ps", "_cs.vcs": "cs", "_gs.vcs": "gs", "_hs.vcs": "hs", "_ds.vcs": "ds"}
ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"
REQUIRES = {
    0x1: "DOUBLES", 0x2: "EARLY_DEPTH_STENCIL", 0x4: "UAVS_AT_EVERY_STAGE", 0x8: "64_UAVS",
    0x10: "MINIMUM_PRECISION", 0x20: "11_1_DOUBLE_EXTENSIONS", 0x40: "11_1_SHADER_EXTENSIONS",
    0x80: "LEVEL_9_COMPARISON_FILTERING", 0x100: "TILED_RESOURCES", 0x200: "STENCIL_REF",
    0x400: "INNER_COVERAGE", 0x800: "TYPED_UAV_LOAD_ADDITIONAL_FORMATS", 0x1000: "ROVS",
    0x2000: "VP_AND_RT_ARRAY_INDEX_FROM_ANY_SHADER_FEEDING_RASTERIZER",
}


def vpk_entries(dir_path):
    d = open(dir_path, "rb").read()
    sig, ver, tree = struct.unpack_from("<III", d, 0)
    if sig != 0x55AA1234:
        raise SystemExit(f"{dir_path}: not a VPK")
    off = 28 if ver == 2 else 12
    end = off + tree

    def string():
        nonlocal off
        e = d.index(b"\0", off)
        s = d[off:e].decode("latin1")
        off = e + 1
        return s

    while True:
        ext = string()
        if not ext:
            break
        while True:
            path = string()
            if not path:
                break
            while True:
                name = string()
                if not name:
                    break
                _crc, pre, arc, aoff, alen, _term = struct.unpack_from("<IHHIIH", d, off)
                off += 18
                preload = d[off:off + pre]
                off += pre
                yield f"{path}/{name}.{ext}", arc, aoff, alen, preload, d, end


def entry_bytes(dir_path, arc, aoff, alen, preload, dird, end):
    if arc == 0x7FFF:
        return preload + dird[end + aoff:end + aoff + alen]
    base = dir_path[: -len("_dir.vpk")]
    with open(f"{base}_{arc:03d}.vpk", "rb") as f:
        f.seek(aoff)
        return preload + f.read(alen)


def frames(data, dictionary):
    out = []
    pos = 0
    while True:
        i = data.find(ZSTD_MAGIC, pos)
        if i < 0:
            break
        for args in (["-D", dictionary], []):
            p = subprocess.run(["zstd", "-d", "-c", "-q", *args], input=data[i:], capture_output=True, timeout=60)
            if p.stdout:
                out.append(p.stdout)
                break
        pos = i + 4
    out.append(data)
    return out


def containers(blob):
    j = 0
    while True:
        j = blob.find(b"DXBC", j)
        if j < 0 or j + 32 > len(blob):
            return
        size = struct.unpack_from("<I", blob, j + 24)[0]
        c = blob[j:j + size]
        j += 4
        if size >= 32 and len(c) == size:
            yield c


def sfi0(c):
    n = struct.unpack_from("<I", c, 28)[0]
    if n > 64 or 32 + 4 * n > len(c):
        return 0
    for o in struct.unpack_from(f"<{n}I", c, 32):
        if c[o:o + 4] == b"SFI0":
            return struct.unpack_from("<Q", c, o + 8)[0]
    return 0


def names(flags):
    return "|".join(v for k, v in REQUIRES.items() if flags & k) or "-"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dict", required=True, help="Valve's vfx zstd dictionary")
    ap.add_argument("out")
    ap.add_argument("vpks", nargs="+", help="*_dir.vpk files")
    a = ap.parse_args()
    seen = set()
    tally = collections.Counter()
    by_flag = collections.defaultdict(set)
    for dir_path in a.vpks:
        for name, arc, aoff, alen, preload, dird, end in vpk_entries(dir_path):
            stage = next((v for k, v in STAGES.items() if name.endswith(k)), None)
            if not stage:
                continue
            data = entry_bytes(dir_path, arc, aoff, alen, preload, dird, end)
            base = os.path.basename(name)[:-4]
            for blob in frames(data, a.dict):
                for c in containers(blob):
                    h = hashlib.sha1(c).hexdigest()[:16]
                    if h in seen:
                        continue
                    seen.add(h)
                    os.makedirs(os.path.join(a.out, stage), exist_ok=True)
                    with open(os.path.join(a.out, stage, f"{base}_{h}.dxbc"), "wb") as f:
                        f.write(c)
                    flags = sfi0(c)
                    tally[(stage, flags)] += 1
                    for bit in REQUIRES:
                        if flags & bit:
                            by_flag[(stage, bit)].add(base)
    print(f"{len(seen)} shaders")
    for (stage, flags), n in sorted(tally.items()):
        print(f"{stage} {names(flags)}: {n}")
    for (stage, bit), shaders in sorted(by_flag.items()):
        print(f"{stage} {REQUIRES[bit]}: {', '.join(sorted(shaders)[:20])}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Compare what two runs of a captured dispatch wrote, per resource, with the capture's formats.

  compare.py CAPTURE_DIR A B

A and B are directories of r<N>.out.bin files (dispatch-replay --dump) or the capture directory itself,
whose r<N>.post.bin files are NVK's own result (NVK_DISPATCH_CAPTURE_POST=1). For each writable resource:
bitwise identical fraction, max and mean absolute difference and the largest relative difference of the
decoded values (images decode by format; buffers as float32 and uint32).
"""
import json
import os
import sys

import numpy as np

# VkFormat -> (numpy dtype, components, kind) ; kind: f float, u uint, s sint, n unorm, p packed
FORMATS = {
    9: ("u1", 1, "n"), 16: ("u1", 2, "n"), 37: ("u1", 4, "n"), 44: ("u1", 4, "n"), 43: ("u1", 4, "n"),
    13: ("u1", 1, "u"), 41: ("u1", 4, "u"),
    70: ("u2", 1, "n"), 77: ("u2", 2, "n"), 91: ("u2", 4, "n"),
    74: ("u2", 1, "u"), 81: ("u2", 2, "u"), 95: ("u2", 4, "u"),
    76: ("f2", 1, "f"), 83: ("f2", 2, "f"), 97: ("f2", 4, "f"),
    98: ("u4", 1, "u"), 101: ("u4", 2, "u"), 104: ("u4", 3, "u"), 107: ("u4", 4, "u"),
    99: ("i4", 1, "s"), 102: ("i4", 2, "s"), 108: ("i4", 4, "s"),
    100: ("f4", 1, "f"), 103: ("f4", 2, "f"), 106: ("f4", 3, "f"), 109: ("f4", 4, "f"),
    126: ("f4", 1, "f"), 124: ("u2", 1, "n"),
    64: ("u4", 1, "p1010102"), 58: ("u4", 1, "p1010102r"), 122: ("u4", 1, "p111110"), 123: ("u4", 1, "p999e5"),
}


def unpack(raw, fmt):
    """Decoded float64 values and the raw words for a bitwise comparison."""
    if fmt not in FORMATS:
        return None
    dt, comps, kind = FORMATS[fmt]
    a = np.frombuffer(raw[: len(raw) - len(raw) % np.dtype(dt).itemsize], dtype=dt)
    if kind == "f":
        return a.astype(np.float64)
    if kind in ("u", "s"):
        return a.astype(np.float64)
    if kind == "n":
        return a.astype(np.float64) / float(np.iinfo(dt).max)
    if kind.startswith("p1010102"):
        v = np.stack([(a >> 0) & 1023, (a >> 10) & 1023, (a >> 20) & 1023, (a >> 30) & 3], -1).astype(np.float64)
        return (v / np.array([1023, 1023, 1023, 3])).ravel()
    if kind == "p111110":
        def uf(bits, mbits):
            e = (bits >> mbits) & 31
            m = bits & ((1 << mbits) - 1)
            return np.where(e == 0, m / (1 << mbits) * 2.0 ** -14,
                            np.where(e == 31, np.inf, (1 + m / (1 << mbits)) * 2.0 ** (e.astype(np.float64) - 15)))
        r = uf(a & 0x7FF, 6)
        g = uf((a >> 11) & 0x7FF, 6)
        b = uf((a >> 22) & 0x3FF, 5)
        return np.stack([r, g, b], -1).ravel()
    if kind == "p999e5":
        e = ((a >> 27) & 31).astype(np.float64)
        sc = 2.0 ** (e - 15 - 9)
        return np.stack([(a & 511) * sc, ((a >> 9) & 511) * sc, ((a >> 18) & 511) * sc], -1).ravel()
    return None


def load(d, rid):
    for name in ("r%03u.out.bin" % rid, "r%03u.post.bin" % rid):
        p = os.path.join(d, name)
        if os.path.exists(p):
            return open(p, "rb").read()
    return None


def stats(va, vb):
    fin = np.isfinite(va) & np.isfinite(vb)
    nan_mismatch = int(np.sum(np.isnan(va) != np.isnan(vb)))
    d = np.abs(va[fin] - vb[fin])
    if d.size == 0:
        return "no finite values"
    rel = d / np.maximum(np.maximum(np.abs(va[fin]), np.abs(vb[fin])), 1e-30)
    return ("max abs %.3g mean abs %.3g max rel %.3g; %.4f%% values differ%s" %
            (d.max(), d.mean(), rel.max(), 100.0 * np.mean(d > 0),
             "; %d NaN mismatches" % nan_mismatch if nan_mismatch else ""))


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        sys.exit(2)
    cap, da, db = sys.argv[1:]
    m = json.load(open(os.path.join(cap, "manifest.json")))
    print("%s: %s vs %s" % (m["src_hash"], da, db))
    for r in m["resources"]:
        if not r.get("writable") or not r.get("size"):
            continue
        rid = r["id"]
        a, b = load(da, rid), load(db, rid)
        if a is None or b is None:
            print("r%03u: missing in %s" % (rid, da if a is None else db))
            continue
        n = min(len(a), len(b))
        a, b = a[:n], b[:n]
        same = np.mean(np.frombuffer(a[: n - n % 4], "u4") == np.frombuffer(b[: n - n % 4], "u4")) if n >= 4 else 1.0
        fmt = r.get("view_format") if r["kind"] == "image" else r.get("format")
        desc = "%s %s" % (r["kind"], r.get("view_format_name") or r.get("format_name") or "")
        va, vb = (unpack(a, fmt), unpack(b, fmt)) if fmt else (None, None)
        if va is None:
            va = np.frombuffer(a[: n - n % 4], "f4").astype(np.float64)
            vb = np.frombuffer(b[: n - n % 4], "f4").astype(np.float64)
            desc += " (as float32)"
        print("r%03u %s: %.4f%% words identical; %s" % (rid, desc.strip(), 100.0 * same, stats(va, vb)))


if __name__ == "__main__":
    main()

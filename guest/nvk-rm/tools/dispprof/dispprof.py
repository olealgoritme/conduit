#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Summarize an NVK_DISPATCH_PROFILE=1 log (nvk-pass-<pid>.txt).

  dispprof.py FILE [--fps N] [--top N] [--skip N] [--device PTR]

Prints the busiest window's dispatch table (the one with the most GPU ms/s
in dispatches) and a whole-run aggregate per compute shader: GPU ms over all
windows, ms/s, ms per frame (with --fps), calls per frame, mean us per call,
the grids seen and the shader's statistics.

--skip N   leave out the first N windows (loading, shader compiles)
--top N    rows per table (default 40)
--device   only lines of that device (the pointer in the "on for device"
           line); by default the device with the most dispatch time

A table line of the driver (one line in the file):
  NVK dispatch profile:  144.034 ms/s  22.3%     57.1 calls/s  2522.36 us/call
  p50  2602.98 max  3006.24 | src 88825d04f9c554c6 code 9b6f3a19 local 8x8x1
  gprs 168 warps 12 instrs 2370 cycles 297141 spills 0 slm 0 smem 0 | grid
  240x135x1:100% groups 32400..32400 | ind 0/0 dgc 0 meta 0
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import defaultdict

PFX = "NVK dispatch profile:"
RE_ON = re.compile(r"NVK dispatch profile: on for device (\S+)")
RE_PASS_ON = re.compile(r"NVK pass profile: level \d+ on for device (\S+)")
RE_WIN = re.compile(
    r"NVK dispatch profile: window (\d+)( \(final\))?, ([\d.]+) s, (\d+) "
    r"dispatches \(([\d.]+)/s\), ([\d.]+) GPU ms/s in dispatches, (\d+) of "
    r"(\d+) shaders active, (\d+) dropped")
RE_ROW = re.compile(
    r"NVK dispatch profile:\s+([\d.]+) ms/s\s+([\d.]+)%\s+([\d.]+) calls/s\s+"
    r"([\d.]+) us/call p50\s+([\d.]+) max\s+([\d.]+) \| src (\w+) code (\w+) "
    r"local (\S+) gprs (\d+) warps (\d+) instrs (\d+) cycles (\d+) spills "
    r"(\d+) slm (\d+) smem (\d+) \| grid (\S+) groups (\S+) \| ind (\d+)/(\d+) "
    r"dgc (\d+) meta (\d+)")
RE_REST = re.compile(
    r"NVK dispatch profile:\s+([\d.]+) ms/s\s+([\d.]+)%\s+([\d.]+) calls/s "
    r"in the (\d+) other shaders")


def parse(path):
    """Windows per device: {dev: [window dict]}; a window has rows."""
    devs = defaultdict(list)
    cur_dev = None
    win = {}
    with open(path, "r", errors="replace") as f:
        for raw in f:
            line = raw.rstrip("\r\n")
            m = RE_ON.search(line) or RE_PASS_ON.search(line)
            if m:
                cur_dev = m.group(1)
                continue
            if PFX not in line:
                continue
            m = RE_WIN.search(line)
            if m:
                w = {
                    "n": int(m.group(1)), "final": bool(m.group(2)),
                    "secs": float(m.group(3)), "dispatches": int(m.group(4)),
                    "ms_s": float(m.group(6)), "active": int(m.group(7)),
                    "dropped": int(m.group(9)), "rows": [], "rest": None,
                }
                devs[cur_dev].append(w)
                win[cur_dev] = w
                continue
            w = win.get(cur_dev)
            if w is None:
                continue
            m = RE_ROW.search(line)
            if m:
                g = m.groups()
                w["rows"].append({
                    "ms_s": float(g[0]), "pct": float(g[1]),
                    "calls_s": float(g[2]), "us": float(g[3]),
                    "p50": float(g[4]), "max": float(g[5]),
                    "src": g[6], "code": g[7], "local": g[8],
                    "gprs": int(g[9]), "warps": int(g[10]),
                    "instrs": int(g[11]), "cycles": int(g[12]),
                    "spills": int(g[13]), "slm": int(g[14]),
                    "smem": int(g[15]), "grid": g[16], "groups": g[17],
                    "ind": int(g[18]), "ind_read": int(g[19]),
                    "dgc": int(g[20]), "meta": int(g[21]),
                })
                continue
            m = RE_REST.search(line)
            if m:
                w["rest"] = (float(m.group(1)), float(m.group(3)),
                             int(m.group(4)))
    return devs


def name(r):
    if r["src"] == "0" * 16 and r["code"] == "0" * 8:
        return "(cubin/execution set)"
    src = r["src"] if r["src"] != "0" * 16 else "-" * 16
    return src


def flags(r):
    f = []
    if r["ind"]:
        f.append(f"ind {r['ind']}/{r['ind_read']}")
    if r["dgc"]:
        f.append(f"dgc {r['dgc']}")
    if r["meta"]:
        f.append(f"meta {r['meta']}")
    return " ".join(f)


def print_window(w, top, fps):
    print(f"busiest window {w['n']}{' (final)' if w['final'] else ''}: "
          f"{w['secs']:.2f} s, {w['dispatches']} dispatches, "
          f"{w['ms_s']:.3f} GPU ms/s in dispatches"
          + (f" = {w['ms_s'] / fps:.3f} ms/frame at {fps:g} fps" if fps else "")
          + f", {w['active']} shaders, {w['dropped']} dropped")
    hdr = (f"{'ms/s':>8} {'ms/fr':>7} {'%':>5} {'calls/s':>8} {'us/call':>8} "
           f"{'p50':>8} {'max':>8}  {'src':16} {'code':8} {'local':>9} "
           f"{'gprs':>4} {'warps':>5} {'instrs':>6}  grid / groups")
    print(hdr)
    for r in w["rows"][:top]:
        msf = f"{r['ms_s'] / fps:7.3f}" if fps else f"{'':7}"
        print(f"{r['ms_s']:8.3f} {msf} {r['pct']:5.1f} {r['calls_s']:8.1f} "
              f"{r['us']:8.2f} {r['p50']:8.2f} {r['max']:8.2f}  "
              f"{name(r):16} {r['code']:8} {r['local']:>9} {r['gprs']:4} "
              f"{r['warps']:5} {r['instrs']:6}  {r['grid']} "
              f"[{r['groups']}] {flags(r)}")
    rest_ms = sum(r["ms_s"] for r in w["rows"][top:])
    rest_n = len(w["rows"][top:])
    if w["rest"]:
        rest_ms += w["rest"][0]
        rest_n += w["rest"][2]
    if rest_n:
        print(f"{rest_ms:8.3f} ms/s in {rest_n} more shaders")


def parse_groups(s):
    if s == "-":
        return None
    lo, hi = s.split("..")
    return int(lo), int(hi)


def parse_grids(s):
    """'120x68x1:93%,60x34x1:7%,+' -> [('120x68x1', 0.93), ...]"""
    out = []
    if s == "-":
        return out
    for part in s.split(","):
        if ":" not in part:
            continue
        g, pct = part.rsplit(":", 1)
        out.append((g, float(pct.rstrip("%")) / 100.0))
    return out


def aggregate(wins, top, fps):
    secs = sum(w["secs"] for w in wins)
    agg = {}
    other_ms = 0.0
    other_calls = 0.0
    for w in wins:
        if w["rest"]:
            other_ms += w["rest"][0] * w["secs"]
            other_calls += w["rest"][1] * w["secs"]
        for r in w["rows"]:
            key = (r["src"], r["code"], r["local"])
            a = agg.get(key)
            if a is None:
                a = agg[key] = {
                    "r": r, "ms": 0.0, "calls": 0.0, "max": 0.0,
                    "grids": defaultdict(float), "lo": None, "hi": None,
                    "ind": 0, "ind_read": 0, "dgc": 0, "meta": 0,
                    "windows": 0, "p50w": 0.0,
                }
            ms = r["ms_s"] * w["secs"]
            calls = r["calls_s"] * w["secs"]
            a["ms"] += ms
            a["calls"] += calls
            a["p50w"] += r["p50"] * calls
            a["max"] = max(a["max"], r["max"])
            a["windows"] += 1
            for k in ("ind", "ind_read", "dgc", "meta"):
                a[k] += r[k]
            for g, share in parse_grids(r["grid"]):
                a["grids"][g] += share * calls
            gr = parse_groups(r["groups"])
            if gr:
                a["lo"] = gr[0] if a["lo"] is None else min(a["lo"], gr[0])
                a["hi"] = gr[1] if a["hi"] is None else max(a["hi"], gr[1])
    total_ms = sum(a["ms"] for a in agg.values()) + other_ms
    if secs <= 0:
        print("no windows")
        return
    print(f"\nwhole run: {len(wins)} windows, {secs:.1f} s, "
          f"{total_ms / secs:.3f} GPU ms/s in dispatches"
          + (f" = {total_ms / secs / fps:.3f} ms/frame at {fps:g} fps"
             if fps else "")
          + f", {len(agg)} shaders")
    cl = "calls/fr" if fps else "calls/s"
    hdr = (f"{'ms/s':>8} {'ms/fr':>7} {'%':>5} {cl:>8} {'us/call':>8} "
           f"{'~p50':>8} {'max':>8}  {'src':16} {'code':8} {'local':>9} "
           f"{'gprs':>4} {'warps':>5} {'instrs':>6}  grids (share of calls) "
           f"[groups]")
    print(hdr)
    rows = sorted(agg.values(), key=lambda a: -a["ms"])
    for a in rows[:top]:
        r = a["r"]
        ms_s = a["ms"] / secs
        msf = f"{ms_s / fps:7.3f}" if fps else f"{'':7}"
        cpf = f"{a['calls'] / secs / fps:8.2f}" if fps else \
            f"{a['calls'] / secs:8.1f}"
        us = a["ms"] * 1e3 / a["calls"] if a["calls"] else 0.0
        p50 = a["p50w"] / a["calls"] if a["calls"] else 0.0
        gtot = sum(a["grids"].values())
        gl = sorted(a["grids"].items(), key=lambda kv: -kv[1])[:3]
        grids = ",".join(f"{g}:{100 * v / gtot:.0f}%" for g, v in gl) \
            if gtot else "-"
        groups = f"[{a['lo']}..{a['hi']}]" if a["lo"] is not None else ""
        fl = []
        if a["ind"]:
            fl.append(f"ind {a['ind']}/{a['ind_read']}")
        if a["dgc"]:
            fl.append(f"dgc {a['dgc']}")
        if a["meta"]:
            fl.append(f"meta {a['meta']}")
        print(f"{ms_s:8.3f} {msf} {100 * a['ms'] / total_ms:5.1f} {cpf} "
              f"{us:8.2f} {p50:8.2f} {a['max']:8.2f}  {name(r):16} "
              f"{r['code']:8} {r['local']:>9} {r['gprs']:4} {r['warps']:5} "
              f"{r['instrs']:6}  {grids} {groups} {' '.join(fl)}")
    rest = sum(a["ms"] for a in rows[top:]) + other_ms
    if rest > 0:
        print(f"{rest / secs:8.3f} ms/s in the other shaders")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("file")
    ap.add_argument("--fps", type=float, default=0.0,
                    help="frames per second of the run, for ms per frame")
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--skip", type=int, default=0,
                    help="leave out the first N windows")
    ap.add_argument("--device", default=None)
    args = ap.parse_args()

    devs = parse(args.file)
    devs = {d: ws for d, ws in devs.items() if ws}
    if not devs:
        sys.exit(f"{args.file}: no NVK dispatch profile windows")
    if args.device:
        if args.device not in devs:
            sys.exit(f"no device {args.device}; devices: {', '.join(map(str, devs))}")
        dev = args.device
    else:
        dev = max(devs, key=lambda d: sum(w["ms_s"] * w["secs"]
                                          for w in devs[d]))
    if len(devs) > 1:
        print(f"devices: {', '.join(map(str, devs))}; showing {dev}")
    wins = devs[dev][args.skip:]
    if not wins:
        sys.exit("no windows left after --skip")
    print_window(max(wins, key=lambda w: w["ms_s"]), args.top, args.fps)
    aggregate(wins, args.top, args.fps)


if __name__ == "__main__":
    main()

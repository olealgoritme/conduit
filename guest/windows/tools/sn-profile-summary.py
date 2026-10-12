#!/usr/bin/env python3
"""Summarise a profiled run of a D3D12 or Vulkan workload on NVK.

    sn-profile-summary.py [--skip N] [--top K] FILE...

FILE: NVK_PASS_PROFILE output (nvk-pass-<pid>.txt, %ProgramData%\\Helios) and/or
the D3D12 UMD log with HELIOS_VKD3D_GPU_TIMESTAMPS=1 ("GPU timestamps:" lines)
and HELIOS_VKD3D_PASS_END_STATS=1 ("Helios pass ends:" lines).

From the pass profile (2 s windows; the first --skip windows, default 2, and
windows with under 50 GPU ms/s are left out as loading):
  - command-buffer GPU ms per second: ~1000 means this process kept the GPU
    busy; well under means the GPU waited for it (or ran other contexts);
  - the split into render passes, operations outside passes and the gaps
    between them inside command buffers;
  - the heaviest pass and operation signatures by mean GPU ms/s.
From the UMD log: GPU busy and idle gap per ExecuteCommandLists batch, and
render passes begun / ended by ResourceBarrier per window.
"""
import argparse
import re
import statistics
import sys
from collections import defaultdict

WIN = re.compile(r"NVK pass profile: window (\d+)( \(final\))?, ([\d.]+) s")
SUM = re.compile(r"summary GPU ms/s: command buffers ([\d.]+) = render passes ([\d.]+) \+ "
                 r"operations ([\d.]+) \+ between them ([\d.]+)")
SIG = re.compile(r"NVK pass profile:\s+([\d.]+) GPU ms/s\s+([\d.]+) us each\s+(\d+)x(.*?)  (.*)$")
GPUTS = re.compile(r"GPU timestamps: (\d+) batches.*?busy avg (\d+) us/batch \((\d+) us/frame\), max (\d+) us; "
                   r"idle gap before a batch avg (\d+) us \((\d+) us/frame\)")
PASSEND = re.compile(r"Helios pass ends: ([\d.]+) s window, (\d+) passes begun, (\d+) ended or suspended")
RB = re.compile(r"ResourceBarrier (\d+) calls \((\d+) barriers\), (\d+) ended an active pass, (\d+) of those")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--skip", type=int, default=2)
    ap.add_argument("--top", type=int, default=15)
    ap.add_argument("files", nargs="+")
    a = ap.parse_args()

    windows = []          # (cmd, passes, ops, between)
    sig_ms = defaultdict(list)
    sig_us = defaultdict(list)
    cur = None
    gputs, passend, rbs = [], [], []
    for fn in a.files:
        with open(fn, errors="replace") as f:
            for line in f:
                line = line.rstrip("\r\n")
                m = WIN.search(line)
                if m:
                    cur = {"n": int(m.group(1)), "sigs": []}
                    continue
                m = SUM.search(line)
                if m and cur is not None:
                    cur["sum"] = tuple(float(x) for x in m.groups())
                    windows.append(cur)
                    continue
                m = SIG.search(line)
                if m and cur is not None and "sum" in cur:
                    cur["sigs"].append((m.group(5).strip(), float(m.group(1)), float(m.group(2))))
                    continue
                m = GPUTS.search(line)
                if m:
                    gputs.append(tuple(int(x) for x in m.groups()))
                    continue
                m = PASSEND.search(line)
                if m:
                    passend.append((float(m.group(1)), int(m.group(2)), int(m.group(3))))
                    continue
                m = RB.search(line)
                if m:
                    rbs.append(tuple(int(x) for x in m.groups()))

    steady = [w for w in windows[a.skip:] if w["sum"][0] >= 50.0]
    if steady:
        cmd = [w["sum"][0] for w in steady]
        print(f"pass profile: {len(windows)} windows, {len(steady)} steady")
        print(f"  command buffers GPU ms/s: median {statistics.median(cmd):.0f} "
              f"(min {min(cmd):.0f}, max {max(cmd):.0f})  -> GPU busy for this process ~{statistics.median(cmd) / 10:.0f}%")
        for i, name in enumerate(("render passes", "operations", "between them")):
            v = [w["sum"][i + 1] for w in steady]
            print(f"  {name:14s} median {statistics.median(v):7.1f} GPU ms/s")
        for w in steady:
            for name, ms, us in w["sigs"]:
                sig_ms[name].append(ms)
                sig_us[name].append(us)
        ranked = sorted(sig_ms.items(), key=lambda kv: -sum(kv[1]) / len(steady))
        print(f"  top {a.top} signatures (mean GPU ms/s over steady windows, us per instance):")
        for name, v in ranked[:a.top]:
            print(f"    {sum(v) / len(steady):8.1f} ms/s {statistics.median(sig_us[name]):9.1f} us  {name[:110]}")
    elif windows:
        print(f"pass profile: {len(windows)} windows, none steady (all under 50 GPU ms/s?)")

    if gputs:
        last = gputs[-1]
        print(f"ECL GPU timestamps (last line): {last[0]} batches, busy {last[1]} us/batch ({last[2]} us/frame), "
              f"max {last[3]} us, idle gap {last[4]} us/batch ({last[5]} us/frame)")
        busy, gap = last[2], last[5]
        if busy + gap:
            print(f"  -> GPU idle between this process's batches: {100 * gap / (busy + gap):.0f}% of busy+gap")
    if passend:
        b = [p[1] / p[0] for p in passend[1:] or passend]
        e = [p[2] / p[0] for p in passend[1:] or passend]
        print(f"render passes: median {statistics.median(b):.0f} begun/s, {statistics.median(e):.0f} ended or suspended/s")
    if rbs:
        last = rbs[-1]
        print(f"ResourceBarrier (last window): {last[0]} calls, {last[1]} barriers, {last[2]} ended a pass, "
              f"{last[3]} of those for nothing")
    if not (windows or gputs or passend):
        print("nothing recognised in", " ".join(a.files), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

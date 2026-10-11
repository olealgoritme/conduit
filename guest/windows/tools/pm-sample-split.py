#!/usr/bin/env python3
"""pm-sample-split.py: why are some frames slow? Splits per-frame evidence into
slow and fast frames of a PresentMon capture.

    pm-sample-split.py PM.csv [--sample FILE ...] [--trace frametrace.csv]
                       [--pkg PKGDIR] [--app cs2.exe] [--slow 1.6]

PM.csv     PresentMon v2 CSV recorded with --qpc_time_ms (CPUStartQPCTime in ms).
--sample   thread-sample.ps1 -Raw -Qpc output (one file per sampled thread).
--trace    the UMD's UmdFrameTrace CSV (C:\\ProgramData\\Helios\\frametrace-<pid>.csv).
--pkg      driver package directory with PDBs, to name helios_umd.dll /
           vulkan_nouveau.dll / librmclient.dll functions (llvm-symbolizer).

A frame is slow when its FrameTime exceeds --slow times the median. Reports:

* periodicity of the slow frames: Rayleigh R of their start times at 15.625 ms
  (the 64 Hz Windows tick and CS2's 64-tick simulation), 4.1667 ms (240 Hz),
  16.667 ms, and the best period between 1 and 40 ms (R ~ 1/sqrt(n) is noise);
  run lengths and the gap between slow frames;
* per sampled thread: where the extra time of a slow frame goes (module, ntdll
  stub or function), as ms per frame in slow frames minus fast frames;
* with --trace: the presenting thread's blocked time inside the driver per
  frame (front-end drains, maps, GetData, flushes, full ring, Present drain),
  slow versus fast.

Pure Python 3, no third-party modules.
"""
import argparse
import bisect
import cmath
import collections
import csv
import math
import re
import statistics
import subprocess
import sys

SYM = {"helios_umd.dll", "vulkan_nouveau.dll", "librmclient.dll", "helios_umd12.dll"}


def fnum(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def load_pm(path, app):
    frames = []
    with open(path, encoding="utf-8-sig", errors="replace") as fh:
        for r in csv.DictReader(fh):
            if app and r.get("Application", "").lower() != app.lower():
                continue
            t, ft = fnum(r.get("CPUStartQPCTime")), fnum(r.get("FrameTime"))
            if t is None or ft is None or ft <= 0 or ft > 250:
                continue
            frames.append({
                "t": t, "ft": ft, "busy": fnum(r.get("CPUBusy")) or 0.0,
                "wait": fnum(r.get("CPUWait")) or 0.0, "mode": r.get("PresentMode", ""),
            })
    frames.sort(key=lambda f: f["t"])
    return frames


def rayleigh(times, period):
    if not times:
        return 0.0
    z = sum(cmath.exp(2j * math.pi * (t % period) / period) for t in times) / len(times)
    return abs(z)


def periodicity(frames, slow):
    ts = [f["t"] for f, s in zip(frames, slow) if s]
    n = len(ts)
    print(f"slow frames: {n} of {len(frames)} ({100.0 * n / max(1, len(frames)):.1f} %), "
          f"{n / max(1e-9, (frames[-1]['t'] - frames[0]['t']) / 1000.0):.1f} per second")
    if n < 10:
        return
    print(f"  Rayleigh R (noise ~{1 / math.sqrt(n):.3f}): " + ", ".join(
        f"{p} ms {rayleigh(ts, p):.3f}" for p in (15.625, 4.1667, 8.3333, 16.6667, 1.0)))
    best = max((rayleigh(ts, p / 100.0), p / 100.0) for p in range(100, 4000))
    print(f"  best period 1..40 ms: {best[1]:.2f} ms (R {best[0]:.3f})")
    runs = collections.Counter()
    k = 0
    for s in slow:
        if s:
            k += 1
        elif k:
            runs[k] += 1
            k = 0
    print("  runs of consecutive slow frames: " + ", ".join(f"{l}x{c}" for l, c in sorted(runs.items())[:8]))
    idx = [i for i, s in enumerate(slow) if s]
    gaps = collections.Counter(b - a for a, b in zip(idx, idx[1:]))
    print("  frames between slow frames: " + ", ".join(f"{g}:{c}" for g, c in sorted(gaps.items())[:12]))
    fs = [f for f, s in zip(frames, slow) if s]
    ff = [f for f, s in zip(frames, slow) if not s]
    for key in ("ft", "busy", "wait"):
        a, b = statistics.fmean(x[key] for x in fs), statistics.fmean(x[key] for x in ff)
        print(f"  {key:5}: slow {a:6.2f} ms  fast {b:6.2f} ms  extra {a - b:+6.2f} ms")


def frame_index(frames):
    starts = [f["t"] for f in frames]

    def find(t):
        i = bisect.bisect_right(starts, t) - 1
        if i < 0 or t >= frames[i]["t"] + frames[i]["ft"]:
            return None
        return i
    return find


def symbolize(pkg, addrs):
    names = {}
    for m, offs in addrs.items():
        offs = sorted(offs)
        inp = "\n".join(hex(0x180000000 + int(o, 16)) for o in offs)
        try:
            out = subprocess.run(["llvm-symbolizer", f"--obj={pkg}/{m}", "--demangle", "--no-inlines",
                                  "--output-style=GNU", "--functions=short"], input=inp,
                                 capture_output=True, text=True, timeout=300).stdout.split("\n")
        except (OSError, subprocess.TimeoutExpired):
            continue
        fn = [out[i] for i in range(0, len(out) - 1, 2)]
        for o, n in zip(offs, fn):
            names[(m, o)] = re.sub(r"\(.*", "", n).strip()[:100]
    return names


def load_samples(path):
    lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
    title = lines[0] if lines else path
    stubs = {}
    if "== syscall stubs" in lines:
        i = lines.index("== syscall stubs") + 1
        while i < len(lines) and not lines[i].startswith("=="):
            p = lines[i].split()
            if len(p) == 2:
                stubs[p[0]] = p[1]
            i += 1
    raw = lines[lines.index("== raw") + 1:] if "== raw" in lines else []
    samples = []
    for l in raw:
        p = l.split()
        if not p or not p[0].startswith("@"):
            continue
        samples.append((float(p[0][1:]), p[1:]))
    return title, stubs, samples


def sample_split(frames, slow, path, pkg, top):
    title, stubs, samples = load_samples(path)
    find = frame_index(frames)
    names = {}
    if pkg:
        addrs = collections.defaultdict(set)
        for _, s in samples:
            m, _, off = s[0].partition("+")
            if m in SYM and off:
                addrs[m].add(off)
        names = symbolize(pkg, addrs)

    def label(ip):
        if ip in stubs:
            return stubs[ip]
        m, _, off = ip.partition("+")
        if m in SYM and (m, off) in names:
            return f"{m}!{names[(m, off)]}"
        return m

    cnt = {True: collections.Counter(), False: collections.Counter()}
    n = {True: 0, False: 0}
    outside = 0
    for t, s in samples:
        i = find(t)
        if i is None:
            outside += 1
            continue
        k = slow[i]
        n[k] += 1
        cnt[k][label(s[0])] += 1
    print(f"\n== {title}: {len(samples)} samples, {n[True]} in slow frames, {n[False]} in fast, "
          f"{outside} outside any frame")
    if not n[True] or not n[False]:
        print("  (need samples in both kinds of frame; was the sampler run with -Qpc during the capture?)")
        return
    fs = statistics.fmean(f["ft"] for f, s in zip(frames, slow) if s)
    ff = statistics.fmean(f["ft"] for f, s in zip(frames, slow) if not s)
    rows = []
    for key in set(cnt[True]) | set(cnt[False]):
        a = cnt[True][key] / n[True]
        b = cnt[False][key] / n[False]
        rows.append((a * fs - b * ff, a, b, key))
    rows.sort(reverse=True)
    print(f"  ms per frame (slow frame {fs:.2f} ms, fast {ff:.2f} ms): extra = slow - fast")
    print(f"  {'extra':>7} {'slow':>6} {'fast':>6}  where")
    for d, a, b, key in rows[:top]:
        print(f"  {d:+7.2f} {a * fs:6.2f} {b * ff:6.2f}  {key}")


def trace_split(frames, slow, path):
    recs = []
    with open(path, encoding="utf-8", errors="replace") as fh:
        rd = csv.DictReader(l for l in fh if not l.startswith("#"))
        for r in rd:
            recs.append({k: fnum(v) for k, v in r.items()})
    if not recs:
        print("\n== trace: empty")
        return
    keys = [k for k in recs[0] if k not in ("dev", "qpc_ms")]
    # A trace record is taken inside the Present call that ends PresentMon's
    # frame: match it to the frame whose CPU work ended (CPUStart + CPUBusy)
    # at most 1 ms earlier.
    ends = [f["t"] + f["busy"] for f in frames]
    match = {True: [], False: []}
    unmatched = 0
    for r in recs:
        q = r["qpc_ms"]
        i = bisect.bisect_right(ends, q) - 1
        if i < 0 or q - ends[i] > 1.0:
            unmatched += 1
            continue
        match[slow[i]].append(r)
    print(f"\n== trace {path}: {len(recs)} records, {len(match[True])} matched to slow frames, "
          f"{len(match[False])} to fast, {unmatched} unmatched")
    if not match[True] or not match[False]:
        return
    print(f"  {'field':18} {'slow':>9} {'fast':>9} {'extra':>9}")
    for k in keys:
        a = statistics.fmean(r[k] or 0.0 for r in match[True])
        b = statistics.fmean(r[k] or 0.0 for r in match[False])
        print(f"  {k:18} {a:9.3f} {b:9.3f} {a - b:+9.3f}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pm")
    ap.add_argument("--sample", action="append", default=[])
    ap.add_argument("--trace", action="append", default=[])
    ap.add_argument("--pkg")
    ap.add_argument("--app", default="cs2.exe")
    ap.add_argument("--slow", type=float, default=1.6)
    ap.add_argument("--top", type=int, default=25)
    a = ap.parse_args()
    frames = load_pm(a.pm, a.app)
    if len(frames) < 20:
        sys.exit(f"{a.pm}: {len(frames)} frames of {a.app}")
    med = statistics.median(f["ft"] for f in frames)
    slow = [f["ft"] > a.slow * med for f in frames]
    modes = collections.Counter(f["mode"] for f in frames)
    print(f"{a.pm}: {len(frames)} frames, median {med:.2f} ms ({1000 / med:.0f} fps), "
          f"mean {statistics.fmean(f['ft'] for f in frames):.2f} ms, slow > {a.slow * med:.2f} ms; modes {dict(modes)}")
    periodicity(frames, slow)
    for s in a.sample:
        sample_split(frames, slow, s, a.pkg, a.top)
    for t in a.trace:
        trace_split(frames, slow, t)


if __name__ == "__main__":
    main()

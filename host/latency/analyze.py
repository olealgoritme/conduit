#!/usr/bin/env python3
"""Stage table of the host round trips in a capture from capture.sh.

    host/latency/analyze.py OUT [--ring N]

One row per stage with count, mean, p50, p90 and p99 in microseconds, for
every fenced SUBMIT_3D on ring N (default 1: the KMD's windowed-Present copy).
Stages whose probes were not in the capture (--light) are left out. See
docs/research/host-roundtrip-latency.md.
"""
import bisect
import collections
import statistics
import sys

SUBMIT_3D = 0x0207


def load(path):
    ev = collections.defaultdict(list)
    wk = collections.defaultdict(list)
    rn = collections.defaultdict(list)
    for line in open(path):
        p = line.split()
        if len(p) < 3 or not p[1].isdigit() or not p[2].lstrip("-").isdigit():
            continue
        tag, ts, tid = p[0], int(p[1]), int(p[2])
        if tag == "WK":
            wk[tid].append((ts, int(p[3]), int(p[4]), " ".join(p[6:])))
        elif tag == "RN":
            rn[tid].append(ts)
        else:
            ev[tag].append((ts, tid, p[3:]))
    names = {}
    try:
        for line in open(path + ".threads"):
            tid, name = line.split(maxsplit=1)
            names[int(tid)] = name.strip()
    except OSError:
        pass
    return ev, wk, rn, names


class Index:
    def __init__(self, ev):
        self.ev = ev
        self.ts = {k: [e[0] for e in v] for k, v in ev.items()}

    def next(self, tag, t, pred=lambda e: True, within=50_000_000):
        lst, ts = self.ev.get(tag, []), self.ts.get(tag, [])
        i = bisect.bisect_left(ts, t)
        while i < len(lst) and lst[i][0] - t < within:
            if pred(lst[i]):
                return lst[i]
            i += 1
        return None

    def prev(self, tag, t, pred=lambda e: True, within=50_000_000):
        lst, ts = self.ev.get(tag, []), self.ts.get(tag, [])
        i = bisect.bisect_right(ts, t) - 1
        while i >= 0 and t - lst[i][0] < within:
            if pred(lst[i]):
                return lst[i]
            i -= 1
        return None


def before(series, t, within):
    i = bisect.bisect_right(series, t) - 1
    return series[i] if i >= 0 and t - series[i] < within else None


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    ring = 1
    if "--ring" in args:
        ring = int(args[args.index("--ring") + 1])
    ev, wk, rn, names = load(args[0])
    ix = Index(ev)
    by_name = collections.defaultdict(set)
    for tid, n in names.items():
        by_name[n].add(tid)
    deliverers = by_name["nvgpu-fences"] | by_name["venus-ipc"]
    wk_ts = {tid: [w[0] for w in v] for tid, v in wk.items()}

    stages = collections.defaultdict(list)
    cstate = collections.Counter()
    quiet = collections.Counter()

    def add(name, a, b):
        if a is not None and b is not None and b >= a:
            stages[name].append((b - a) / 1000)

    for ts, tid, f in ev.get("D0", []):
        ty, flags, fence, ring_idx = int(f[0]), int(f[1]), int(f[2]), int(f[4])
        if ty != SUBMIT_3D or flags & 1 == 0 or (ring_idx if flags & 2 else 0) != ring:
            continue
        wf = ix.next("WF", ts, lambda e: int(e[2][2]) == fence)
        if not wf:
            continue
        t = ix.prev("T", wf[0], lambda e: int(e[2][0]) == ring)
        if not t or t[0] < ts:
            continue
        # The chain's interrupt: the first MSI after the fence callback from
        # the thread that returned it (the pump, the renderer reader, or the
        # queue thread right after a dispatch that delivered).
        msi = ix.next("MSI", wf[0], lambda e: e[1] in deliverers or not deliverers)
        # INTx instead of MSI-X: the backend's signal reaches QEMU's main
        # loop, which raises the line.
        if not msi:
            u0 = ix.next("U0", wf[0], lambda e: e[1] in deliverers or not deliverers)
            irq = u0 and ix.next("IRQ", u0[0])
            if irq:
                add("16b signal_used_queue -> QEMU raises INTx", u0[0], irq[0])
                msi = irq
        kick = before(wk_ts.get(tid, []), ts, 2_000_000)
        run = before(rn.get(tid, []), ts, 2_000_000)
        add("01 queue thread woken -> Venus::dispatch", kick, ts)
        add("02 queue thread running -> Venus::dispatch", run, ts)
        d1 = ix.next("D1", ts, lambda e: e[1] == tid)
        s0 = ix.next("S0", ts, lambda e: e[1] == tid)
        f1 = ix.next("F1", ts, lambda e: e[1] == tid)
        vs0 = ix.next("VS0", ts)
        vs1 = vs0 and ix.next("VS1", vs0[0], lambda e: e[1] == vs0[1])
        vf0 = ix.next("VF0", ts, lambda e: int(e[2][2]) == fence)
        q1 = vs0 and ix.next("Q1", vs0[0])
        y1 = vf0 and ix.next("Y1", vf0[0])
        g = lambda e: e[0] if e else None
        add("03 dispatch -> renderer call sent", ts, g(s0))
        add("04 call sent -> conduit-venus Virgl::submit", g(s0), g(vs0))
        add("05 Virgl::submit", g(vs0), g(vs1))
        add("06 Virgl::submit -> Virgl::create_fence", g(vs1), g(vf0))
        add("07 dispatch -> backend has the fence reply", ts, g(f1))
        add("08 dispatch -> chain held (queue thread free)", ts, g(d1))
        add("09 dispatch -> copy's vkQueueSubmit returned", ts, g(q1))
        add("10 dispatch -> fence's sync submit returned", ts, g(y1))
        if q1:
            qt = t[1]
            qrun = before(rn.get(qt, []), t[0], 20_000_000)
            ni = ix.prev("NI", qrun if qrun else t[0], within=5_000_000)
            add("11 copy submitted -> vkr-queue running (GPU + wake)", g(q1), qrun)
            if ni and ni[0] >= q1[0]:
                add("12 copy submitted -> NVIDIA interrupt (GPU)", q1[0], ni[0])
                add("13 NVIDIA interrupt -> vkr-queue running", ni[0], qrun)
        add("14 dispatch -> timeline written", ts, t[0])
        add("15 timeline -> write_context_fence", t[0], wf[0])
        if msi:
            add("16 write_context_fence -> MSI", wf[0], msi[0])
            add("17 timeline -> MSI (fence return path)", t[0], msi[0])
            add("18 dispatch -> MSI (host round trip)", ts, msi[0])
            w = before(wk_ts.get(msi[1], []), msi[0], 2_000_000)
            if w is not None and w >= wf[0]:
                i = wk_ts[msi[1]].index(w)
                cstate[names.get(msi[1], msi[1]), wk[msi[1]][i][2]] += 1
        # An interrupt from the queue thread right after holding the chain:
        # the empty one `quiet-held` removes.
        if d1:
            m = ix.next("MSI", d1[0], lambda e: e[1] == tid, within=30_000)
            d_next = ix.next("D0", d1[0] + 1, lambda e: e[1] == tid, within=30_000)
            quiet["held"] += 1
            if m and (not d_next or m[0] < d_next[0]):
                quiet["interrupt after holding"] += 1

    def pct(v, p):
        v = sorted(v)
        return v[min(len(v) - 1, int(p * len(v)))]

    print(f"{'stage (us)':55s} {'n':>5s} {'mean':>7s} {'p50':>7s} {'p90':>7s} {'p99':>7s}")
    for k in sorted(stages):
        v = stages[k]
        print(f"{k:55s} {len(v):5d} {statistics.mean(v):7.1f} {pct(v, .5):7.1f} "
              f"{pct(v, .9):7.1f} {pct(v, .99):7.1f}")
    if quiet:
        print(f"held chains: {quiet['held']}, followed by an interrupt from the queue "
              f"thread: {quiet['interrupt after holding']}")
    if cstate:
        print("idle state of the deliverer's CPU at its wakeup (0 awake, 1 POLL, 2 C1, 3 C2, 4 C3):",
              dict(cstate))
    msis = collections.Counter(names.get(e[1], e[1]) for e in ev.get("MSI", []))
    span = (max(e[0] for v in ev.values() for e in v) - min(e[0] for v in ev.values() for e in v)) / 1e9
    print("MSIs per second by thread:", {k: round(n / span) for k, n in msis.most_common()})


if __name__ == "__main__":
    main()

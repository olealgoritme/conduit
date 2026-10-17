#!/usr/bin/env python3
# objcmp.py A B: per-stage totals of two run-objects.sh runs (instructions, static cycles, memory spills, register and occupancy changes)
import sys, collections
def load(p):
    r = {}
    for l in open(p):
        x = l.split()
        r[x[0]] = (x[1], None if x[2] in ('FAILED', 'LOST') else tuple(map(int, x[2:8])))
    return r
a, b = load(sys.argv[1]), load(sys.argv[2])
st = collections.defaultdict(lambda: collections.Counter())
for k, (s, va) in a.items():
    vb = b.get(k, (s, None))[1]
    c = st[s]
    c['n'] += 1
    if va is None or vb is None:
        c['fail_a'] += va is None; c['fail_b'] += vb is None; continue
    c['ia'] += va[0]; c['ib'] += vb[0]; c['ca'] += va[3]; c['cb'] += vb[3]
    c['spa'] += va[4] + va[5]; c['spb'] += vb[4] + vb[5]
    c['wa'] += va[2]; c['wb'] += vb[2]
    c['gpr_up'] += vb[1] > va[1]; c['gpr_dn'] += vb[1] < va[1]
    c['occ_dn'] += vb[2] < va[2]; c['occ_up'] += vb[2] > va[2]
for s, c in sorted(st.items()):
    ok = c['n'] - max(c['fail_a'], c['fail_b'])
    print(f"{s}: {c['n']} shaders (failed {c['fail_a']}/{c['fail_b']}) instrs {c['ia']} -> {c['ib']} ({100*(c['ib']-c['ia'])/max(c['ia'],1):+.2f}%) "
          f"cycles {c['ca']} -> {c['cb']} ({100*(c['cb']-c['ca'])/max(c['ca'],1):+.2f}%) mem spill+fill {c['spa']} -> {c['spb']} "
          f"GPRs up/down {c['gpr_up']}/{c['gpr_dn']} warps down/up {c['occ_dn']}/{c['occ_up']}")

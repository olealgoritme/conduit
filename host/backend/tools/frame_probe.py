#!/usr/bin/env python3
"""frame_probe.py FILE [refresh_hz]: what the host screen really showed, from CONDUIT_FRAME_PROBE's log.

S <stamp> <index> <t_ns>      a frame sent to a display client (index from the guest app's colour;
                               -1 not a probe frame, -2 the buffer could not be mapped)
P <stamp> <present_ns> <kind> a display client's presentation report of that frame
"""
import sys
hz = float(sys.argv[2]) if len(sys.argv) > 2 else 240.0
sent, pres = {}, []
s_list = []
for line in open(sys.argv[1]):
    f = line.split()
    if len(f) != 4:
        continue
    if f[0] == 'S':
        sent[int(f[1])] = int(f[2]); s_list.append((int(f[3]), int(f[2])))
    elif f[0] == 'P':
        pres.append((int(f[2]), int(f[1]), int(f[3])))
def summary(name, seq):
    # seq: list of (t_ns, idx)
    seq = [x for x in seq if x[1] >= 0]
    if len(seq) < 2:
        print(f'{name}: {len(seq)} probe frames'); return
    dur = (seq[-1][0] - seq[0][0]) / 1e9
    idx = [i for _, i in seq]
    repeats = sum(1 for a, b in zip(idx, idx[1:]) if b == a)
    back = sum(1 for a, b in zip(idx, idx[1:]) if b < a)
    gaps = [b - a - 1 for a, b in zip(idx, idx[1:]) if b > a + 1]
    uniq = len(set(idx))
    iv = sorted((b[0] - a[0]) / 1e3 for a, b in zip(seq, seq[1:]))
    new_iv = []
    last_t, last_i = None, None
    for t, i in seq:
        if i != last_i:
            if last_t is not None:
                new_iv.append((t - last_t) / 1e3)
            last_t, last_i = t, i
    new_iv.sort()
    pc = lambda v, q: v[min(len(v) - 1, int(q * (len(v) - 1) + 0.5))] if v else 0
    per = 1e6 / hz
    print(f'{name}: {len(seq)} frames in {dur:.1f} s = {len(seq)/dur:.1f}/s, unique {uniq} = {uniq/dur:.1f}/s, '
          f'repeats {repeats}, backwards {back}, gaps {len(gaps)} (skipped {sum(gaps)} indices)')
    print(f'   interval us p50 {pc(iv,.5):.0f} p90 {pc(iv,.9):.0f} p99 {pc(iv,.99):.0f}; '
          f'new-content interval us p50 {pc(new_iv,.5):.0f} p90 {pc(new_iv,.9):.0f} p99 {pc(new_iv,.99):.0f} max {new_iv[-1] if new_iv else 0:.0f}; '
          f'>1.5 periods {100*sum(1 for x in new_iv if x > 1.5*per)/max(1,len(new_iv)):.1f}%')
bad = sum(1 for _, i in s_list if i < 0)
print(f'sent: {len(s_list)} ({bad} without a probe index: -1 not a probe frame, -2 unmappable)')
summary('sent', s_list)
summary('presented', sorted((t, sent.get(st, -3)) for t, st, _ in pres))

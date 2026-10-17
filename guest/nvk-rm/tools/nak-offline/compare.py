#!/usr/bin/env python3
"""Compare NAK statistics of one or more runs with a reference driver's.

  compare.py [--ref REF_DIR] [--only cs|fs|vs] [--sort KEY] [--top N]
             NAME=RUN_DIR [NAME=RUN_DIR ...]

RUN_DIR holds run.sh output (compute.txt, frag.txt, vert.txt).  REF_DIR
holds the reference driver's nvstats output for the same corpus
(nv-compute.txt, nv-frag.txt, nv-vert.txt: Register Count, Binary Size,
Shared Memory Size), optional.

Per shader: GPRs, warps/SM, instructions, code bytes and memory spills/fills
of every run, and the reference's GPRs, warps and code bytes.  Warps/SM are
computed the same way for every column (GB20x: 64K registers in 4
partitions, 48 warps, 32 CTAs, 100 KiB shared memory plus 1 KiB per CTA), so
they include the workgroup and shared memory limits of compute shaders.
Totals at the end.
"""
import argparse
import math
import os
import re
import sys

MAX_WARPS = 48
MAX_CTAS = 32
SMEM_PER_SM = 100 * 1024
SMEM_PER_CTA_RESERVED = 1024


def warps_for(gprs, threads=None, smem=0):
    """Resident warps per SM for a per-thread register count."""
    g = max(8, int(math.ceil(gprs / 8.0)) * 8)
    w = min(MAX_WARPS, 4 * (512 // g))
    if threads is None:
        return w
    wpc = max(1, (threads + 31) // 32)
    ctas = min(MAX_CTAS, w // wpc)
    if smem:
        ctas = min(ctas, SMEM_PER_SM // (smem + SMEM_PER_CTA_RESERVED))
    return ctas * wpc


def parse_stats(seg):
    d = {}
    for kv in seg.split('|'):
        if '=' in kv:
            k, v = kv.split('=', 1)
            try:
                d[k.strip()] = int(float(v.strip()))
            except ValueError:
                pass
    return d


LINE = re.compile(r'^(\S+) local (\d+)x(\d+)x(\d+)(.*)$')


def parse_file(path, want):
    """name -> (threads, stats) for executable `want` (None: first)."""
    out = {}
    if not os.path.exists(path):
        return out
    for line in open(path):
        m = LINE.match(line.strip())
        if not m or 'FAILED' in m.group(5):
            continue
        name = m.group(1)
        threads = int(m.group(2)) * int(m.group(3)) * int(m.group(4))
        rest = m.group(5)
        if '||' in rest:
            segs = {}
            for s in rest.split('||')[1:]:
                mm = re.match(r'\s*\[(\w+)\](.*)', s)
                if mm:
                    segs[mm.group(1)] = mm.group(2)
            seg = None
            for w in want:
                if w in segs:
                    seg = segs[w]
                    break
            if seg is None:
                continue
        else:
            seg = rest
        out[name] = (threads, parse_stats(seg))
    return out


STAGES = {
    'cs': ('compute.txt', ['compute'], 'nv-compute.txt', [None]),
    'fs': ('frag.txt', ['fragment'], 'nv-frag.txt', ['FS']),
    'vs': ('vert.txt', ['vertex'], 'nv-vert.txt', ['VS']),
}


def nak_row(threads, st, stage):
    g = st.get('Number of GPRs', 0)
    smem = st.get('Shared size', 0)
    return {
        'gprs': g,
        'warps': warps_for(g, threads if stage == 'cs' else None, smem),
        'instrs': st.get('Instruction count', 0),
        'code': st.get('Code size', 0),
        'spills': st.get('Spills to memory', 0),
        'fills': st.get('Fills from memory', 0),
        'cycles': st.get('Static cycle count', 0),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--ref', help='reference driver nvstats directory')
    ap.add_argument('--only', choices=sorted(STAGES), action='append')
    ap.add_argument('--sort', default='gprs',
                    help='sort by this field of the first run (default gprs)')
    ap.add_argument('--top', type=int, default=0,
                    help='print only the first N rows per stage (0: all)')
    ap.add_argument('--totals-only', action='store_true')
    ap.add_argument('runs', nargs='+', metavar='NAME=RUN_DIR')
    a = ap.parse_args()

    runs = []
    for r in a.runs:
        name, _, d = r.partition('=')
        if not d:
            name, d = os.path.basename(os.path.normpath(r)), r
        runs.append((name, d))

    for stage in a.only or ['cs', 'fs', 'vs']:
        fname, want, nvname, nvwant = STAGES[stage]
        data = [parse_file(os.path.join(d, fname), want) for _, d in runs]
        ref = parse_file(os.path.join(a.ref, nvname), nvwant) if a.ref else {}
        names = sorted(set().union(*[set(x) for x in data]))
        if not names:
            continue
        rows = []
        for n in names:
            per = []
            for x in data:
                per.append(nak_row(x[n][0], x[n][1], stage) if n in x else None)
            nv = None
            if n in ref:
                t, st = ref[n]
                g = st.get('Register Count', 0)
                nv = {'gprs': g, 'code': st.get('Binary Size', 0),
                      'warps': warps_for(g, t if stage == 'cs' else None,
                                         st.get('Shared Memory Size', 0))}
            threads = next(x[n][0] for x in data if n in x)
            smem = next(x[n][1].get('Shared size', 0) for x in data if n in x)
            rows.append((n, threads, smem, per, nv))
        key = a.sort
        rows.sort(key=lambda r: -(r[3][0] or {}).get(key, 0))

        print(f'== {stage}: {len(rows)} shaders ==')
        hdr = f'{"shader":<18} {"wg":>4} {"smem":>6}'
        for name, _ in runs:
            hdr += f' | {name[:10]:>10}: gpr wrp  instrs   code  sp/fl'
        if ref:
            hdr += ' | ref: gpr wrp   code'
        if not a.totals_only:
            print(hdr)
            for i, (n, t, smem, per, nv) in enumerate(rows):
                if a.top and i >= a.top:
                    break
                s = f'{n[:18]:<18} {t if stage == "cs" else "":>4} {smem:>6}'
                for p in per:
                    if p is None:
                        s += ' | ' + ' ' * 41
                    else:
                        sf = f'{p["spills"]}/{p["fills"]}'
                        s += (f' | {"":>10}  {p["gprs"]:>3} {p["warps"]:>3}'
                              f' {p["instrs"]:>7} {p["code"]:>6} {sf:>6}')
                if ref:
                    if nv:
                        s += f' |      {nv["gprs"]:>3} {nv["warps"]:>3} {nv["code"]:>6}'
                    else:
                        s += ' |      (none)'
                print(s)

        # Totals over the shaders every run (and the reference) has
        common = [r for r in rows if all(p is not None for p in r[3])
                  and (not ref or r[4] is not None)]
        print(f'-- totals over {len(common)} shaders --')
        for i, (name, _) in enumerate(runs):
            ps = [r[3][i] for r in common]
            tot = {k: sum(p[k] for p in ps) for k in
                   ('instrs', 'code', 'spills', 'fills', 'cycles')}
            avg_w = sum(p['warps'] for p in ps) / max(1, len(ps))
            big = [p for p in ps if p['gprs'] > 96]
            print(f'{name:>12}: instrs {tot["instrs"]:>8} code {tot["code"]:>9}'
                  f' cycles {tot["cycles"]:>8} spills {tot["spills"]:>5}'
                  f' fills {tot["fills"]:>5} avg warps {avg_w:5.1f}'
                  f' >96 GPRs {len(big):>3}', end='')
            if ref:
                worse = sum(1 for r in common
                            if r[3][i]['gprs'] > 1.25 * r[4]['gprs'])
                lessocc = sum(1 for r in common
                              if r[3][i]['warps'] < r[4]['warps'])
                print(f' | >1.25x ref GPRs {worse:>3}, fewer warps than ref'
                      f' {lessocc:>3}', end='')
            print()
        if ref:
            nvs = [r[4] for r in common]
            print(f'{"ref":>12}: code {sum(n["code"] for n in nvs):>9}'
                  f' avg warps {sum(n["warps"] for n in nvs) / max(1, len(nvs)):5.1f}'
                  f' >96 GPRs {sum(1 for n in nvs if n["gprs"] > 96):>3}')
        print()


if __name__ == '__main__':
    sys.exit(main())

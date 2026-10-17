#!/usr/bin/env python3
"""Compare two pass-profile logs, pass by pass: NVIDIA (VK_LAYER_CONDUIT_pass_profile)
against NVK (NVK_PASS_PROFILE=1, or higher).

Both write the same lines ("... GPU ms/s ... us each ...x ... <signature>"), one
set per 2 s window.  This sums the selected windows, turns the totals into GPU
ms per frame (frames = fps x seconds; the layer also counts presents) and
prints, for every pass / operation run that both sides have, NVIDIA ms against
NVK ms and their ratio.

  compare_pass_profiles.py --nvidia nv.1234 --nvk nvk-pass-5678.txt \
      --fps-nvidia 145 --fps-nvk 65

Passes are matched on size, samples, layers, colour attachment count and the
format and load/store ops of the first colour and the depth attachment (what
both sides know).  A signature that has more than 24 (NVK) or 60 (layer)
heavier ones in a window is not in the log of that window: "unlisted" in the
table is whatever the summary lines say beyond the listed rows.
"""
import argparse
import re
import sys
from collections import defaultdict

LINE = re.compile(r'(?:NVK|VKL) pass profile:\s+([\d.]+) GPU ms/s\s+([\d.]+) us each\s+(\d+)x(.*)$')
WINDOW = re.compile(r'(?:NVK|VKL) pass profile: window (\d+)( \(final\))?, ([\d.]+) s')
FRAMES = re.compile(r', (\d+) frames \(device (\d+)\)')
SUMMARY = re.compile(r'(?:NVK|VKL) pass profile: summary GPU ms/s: command buffers ([\d.]+) = '
                     r'render passes ([\d.]+) \+ operations ([\d.]+) \+ between them ([\d.]+)')
NAME = re.compile(r'(pass \d+x\d+ x\d+ layers|ops: |command buffer \(all work\)|'
                  r'compute-queue command buffer|secondary command buffer|label: )')
CALLS = re.compile(r'([\d.]+) calls\s+([\d.]+) draws')
OPS = re.compile(r'([\d.]+) ops')
PASS = re.compile(r'^pass (\d+)x(\d+) x(\d+) layers (\d+), (\d+) rt(.*)$')
LDST = re.compile(r'\b(load|clear|dontcare|none)/(store|dontcare|none)\b')


def att(seg, first_is_label):
    toks = seg.split()
    if first_is_label:
        toks = toks[1:]
    if not toks:
        return '-'
    ops = LDST.findall(seg)
    return '%s:%s' % (toks[0], '/'.join(ops[-1]) if ops else '?')


def signature_key(name):
    """(kind, key) shared by both sides"""
    m = PASS.match(name)
    if m:
        w, h, s, layers, nrt, rest = m.groups()
        head, _, depth = rest.partition(' z ')
        if rest.startswith(' z '):
            head, depth = '', rest[3:]
        c0 = att(head, True) if head.strip() else '-'
        z = att(depth, False) if depth.strip() else '-'
        return 'pass', 'pass %sx%s x%s L%s %srt c0=%s z=%s' % (w, h, s, layers, nrt, c0, z)
    if name.startswith('ops: '):
        return 'op', name
    if name.startswith('command buffer') or name.startswith('compute-queue'):
        return 'cmd', name
    if name.startswith('label: '):
        return 'label', name
    return 'other', name


class Window:
    def __init__(self, number, seconds, final, device):
        self.number, self.seconds, self.final, self.device = number, seconds, final, device
        self.frames = None
        self.summary = None
        self.rows = []  # (kind, key, ns, count, calls, draws)


def parse(path):
    """Return {run id: [Window]}; a run is one device (layer) or one counter run (NVK)."""
    runs = defaultdict(list)
    run_id = 0
    cur = None
    with open(path, errors='replace') as f:
        for line in f:
            m = WINDOW.search(line)
            if m:
                number, final, secs = int(m.group(1)), bool(m.group(2)), float(m.group(3))
                fm = FRAMES.search(line)
                device = int(fm.group(2)) if fm else None
                if device is not None:
                    rid = device
                else:
                    if number == 1 and runs[run_id]:
                        run_id += 1
                    rid = run_id
                cur = Window(number, secs, final, device)
                if fm:
                    cur.frames = int(fm.group(1))
                runs[rid].append(cur)
                continue
            if cur is None:
                continue
            m = SUMMARY.search(line)
            if m:
                cur.summary = tuple(float(x) for x in m.groups())
                continue
            m = LINE.search(line)
            if not m:
                continue
            ms_s, _us, count, rest = float(m.group(1)), float(m.group(2)), int(m.group(3)), m.group(4)
            n = NAME.search(rest)
            if not n:
                continue
            name = rest[n.start():].strip()
            extra = rest[:n.start()]
            calls = draws = 0.0
            cm = CALLS.search(extra)
            if cm:
                calls, draws = float(cm.group(1)) * count, float(cm.group(2)) * count
            else:
                om = OPS.search(extra)
                if om:
                    calls = float(om.group(1)) * count
            kind, key = signature_key(name)
            cur.rows.append((kind, key, ms_s * cur.seconds * 1e6, count, calls, draws))
    return runs


def choose_windows(runs, args, label):
    if not runs:
        sys.exit('%s: no pass profile windows found' % label)
    # the run with the most command buffer GPU time
    def total(ws):
        return sum((w.summary[0] * w.seconds) if w.summary else 0 for w in ws)
    rid = args.run if args.run is not None else max(runs, key=lambda r: total(runs[r]))
    ws = runs[rid]
    sel = [w for w in ws if not w.final or len(ws) == 1]
    if args.windows:
        a, _, b = args.windows.partition('-')
        sel = [w for w in ws if int(a) <= w.number <= int(b or a)]
    else:
        sel = sel[args.skip:]
        if not args.all and sel:
            peak = max((w.summary[0] if w.summary else 0) for w in sel)
            sel = [w for w in sel if w.summary and w.summary[0] >= args.active * peak]
    if not sel:
        sys.exit('%s: no windows selected' % label)
    print('%s: run %s, %d of %d windows (%s), %.1f s' % (
        label, rid, len(sel), len(ws), ','.join(str(w.number) for w in sel[:12]) + (',..' if len(sel) > 12 else ''),
        sum(w.seconds for w in sel)), file=sys.stderr)
    return sel


def aggregate(ws, fps, label):
    secs = sum(w.seconds for w in ws)
    frames = None
    if fps:
        frames = fps * secs
    elif all(w.frames is not None for w in ws) and sum(w.frames for w in ws) > 0:
        frames = float(sum(w.frames for w in ws))
    if not frames:
        sys.exit('%s: need --fps-%s (the log has no frame count)' % (label, label))
    rows = defaultdict(lambda: [0.0, 0, 0.0, 0.0])
    kinds = {}
    for w in ws:
        for kind, key, ns, count, calls, draws in w.rows:
            r = rows[key]
            r[0] += ns
            r[1] += count
            r[2] += calls
            r[3] += draws
            kinds[key] = kind
    summ = [sum(w.summary[i] * w.seconds for w in ws if w.summary) * 1e6 for i in range(4)]
    return {'frames': frames, 'secs': secs, 'rows': rows, 'kinds': kinds, 'summary': summ}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--nvidia', required=True, help='log of the layer (NVIDIA driver)')
    ap.add_argument('--nvk', required=True, help='log of NVK_PASS_PROFILE')
    ap.add_argument('--fps-nvidia', type=float, help='frames per second of the NVIDIA run (default: the layer counts presents)')
    ap.add_argument('--fps-nvk', type=float, help='frames per second of the NVK run')
    ap.add_argument('--skip', type=int, default=0, help='windows to skip at the start of each log')
    ap.add_argument('--windows', help='window numbers A-B to use in both logs')
    ap.add_argument('--all', action='store_true', help='use every window (default: the active ones)')
    ap.add_argument('--active', type=float, default=0.6, help='active = command buffer time >= this x the peak window (default 0.6)')
    ap.add_argument('--run', type=int, help='device (layer) or run number (NVK) to use; default the busiest')
    ap.add_argument('--top', type=int, default=40, help='pass rows to print (default 40)')
    ap.add_argument('--csv', help='also write the pass/operation table here')
    args = ap.parse_args()

    nv = aggregate(choose_windows(parse(args.nvidia), args, 'nvidia'), args.fps_nvidia, 'nvidia')
    nk = aggregate(choose_windows(parse(args.nvk), args, 'nvk'), args.fps_nvk, 'nvk')

    def ms(side, ns):
        return ns / 1e6 / side['frames']

    print()
    print('GPU ms per frame (frames: NVIDIA %.0f in %.1f s = %.1f fps, NVK %.0f in %.1f s = %.1f fps)' % (
        nv['frames'], nv['secs'], nv['frames'] / nv['secs'], nk['frames'], nk['secs'], nk['frames'] / nk['secs']))
    print('%-28s %10s %10s %7s' % ('', 'NVIDIA', 'NVK', 'NVK/NV'))
    names = ['command buffers', 'render passes', 'operations', 'between them']
    for i, n in enumerate(names):
        a, b = ms(nv, nv['summary'][i]), ms(nk, nk['summary'][i])
        print('%-28s %10.3f %10.3f %7s' % (n, a, b, '%.2fx' % (b / a) if a > 0 else '-'))

    def table(kind, title, limit):
        keys = sorted({k for k, kd in list(nv['kinds'].items()) + list(nk['kinds'].items()) if kd == kind},
                      key=lambda k: -(nk['rows'][k][0] / nk['frames'] if k in nk['rows'] else 0))
        print()
        print(title)
        print('%-62s %8s %8s %6s | %7s %7s | %8s %8s' % (
            'signature', 'NV ms/f', 'NVK ms/f', 'ratio', 'NV n/f', 'NVK n/f', 'NV us', 'NVK us'))
        out = []
        tot = [0.0, 0.0]
        shown = [0.0, 0.0]
        for i, k in enumerate(keys):
            a = nv['rows'].get(k, [0.0, 0, 0.0, 0.0])
            b = nk['rows'].get(k, [0.0, 0, 0.0, 0.0])
            am, bm = a[0] / 1e6 / nv['frames'], b[0] / 1e6 / nk['frames']
            tot[0] += am
            tot[1] += bm
            out.append((k, am, bm, a, b))
            if i < limit:
                shown[0] += am
                shown[1] += bm
                print('%-62s %8.3f %8.3f %6s | %7.2f %7.2f | %8.1f %8.1f' % (
                    k[:62], am, bm, '%.2fx' % (bm / am) if am > 0 and bm > 0 else ('NVK only' if bm > 0 else 'NV only'),
                    a[1] / nv['frames'], b[1] / nk['frames'],
                    a[0] / 1e3 / a[1] if a[1] else 0, b[0] / 1e3 / b[1] if b[1] else 0))
        print('%-62s %8.3f %8.3f %6s' % ('listed rows total (%d shown of %d)' % (min(limit, len(keys)), len(keys)),
                                          tot[0], tot[1], '%.2fx' % (tot[1] / tot[0]) if tot[0] > 0 else '-'))
        return out

    passes = table('pass', 'Render passes', args.top)
    ops = table('op', 'Operation runs (outside render passes)', args.top)
    table('label', 'Debug-utils labels (overlap the rows above)', 20)

    # operations by kind, all size buckets together
    byk = defaultdict(lambda: [0.0, 0.0, 0, 0])
    for k, am, bm, a, b in ops:
        kind = k.split(',')[0]
        byk[kind][0] += am
        byk[kind][1] += bm
        byk[kind][2] += a[1]
        byk[kind][3] += b[1]
    print()
    print('Operations by kind')
    print('%-34s %8s %8s %6s | %7s %7s' % ('kind', 'NV ms/f', 'NVK ms/f', 'ratio', 'NV n/f', 'NVK n/f'))
    for kind, (am, bm, an, bn) in sorted(byk.items(), key=lambda x: -x[1][1]):
        print('%-34s %8.3f %8.3f %6s | %7.2f %7.2f' % (
            kind, am, bm, '%.2fx' % (bm / am) if am > 0 and bm > 0 else '-', an / nv['frames'], bn / nk['frames']))

    if args.csv:
        with open(args.csv, 'w') as f:
            f.write('kind,signature,nvidia_ms_per_frame,nvk_ms_per_frame,nvidia_count_per_frame,nvk_count_per_frame\n')
            for kind, rows in (('pass', passes), ('op', ops)):
                for k, am, bm, a, b in rows:
                    f.write('%s,"%s",%.4f,%.4f,%.3f,%.3f\n' % (kind, k, am, bm, a[1] / nv['frames'], b[1] / nk['frames']))


if __name__ == '__main__':
    main()

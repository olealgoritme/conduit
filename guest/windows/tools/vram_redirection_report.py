#!/usr/bin/env python3
"""Per-frame report of the windowed (blit-model) Present from a DxgKrnl ETW capture.

Input: the folder written by guest/windows/ci/vmtest/vram-etw.ps1 (vram.xml.zip or
vram.xml from `tracerpt -of XML`, optional processes.txt). The question it answers
(guest/windows/docs/vram-redirection.md, phase 1): does the app thread wait for CPU
access to the redirection surface (the destination of the redirected Blt), which
segment does that allocation live in, and who locks it.

Events are classified by their field names, not only by id, so a build that renumbers
or versions an event still parses (ids below are the Windows 11 26H1 ones):
  DXGI 42/43            Present start/stop (the app's Present call)
  DxgKrnl 166 Blit      hSourceAllocation -> hDestAllocation, bRedirectedPresent
  DxgKrnl 184 Present   hSrcAllocHandle/hDstAllocHandle
  DxgKrnl 215/171       PresentHistory(Detailed)Start, Model 3 = D3DKMT_PM_REDIRECTED_BLT
  DxgKrnl 178/180       QueuePacket start/stop (the Blt's DMA packet submitted -> retired)
  DxgKrnl 41/42, 340/341 Lock/Unlock (CPU access to an allocation)
  DxgKrnl 105/106       ProfilerStart/Stop (dxgkrnl function spans, e.g. the CPU-access wait)
  DxgKrnl 33/35, 36/38  AdapterAllocation / DeviceAllocation (+ DC_Start rundown)
  DxgKrnl 78            ReportSegment (segment id, size, flags, memory segment group)
  DxgKrnl 80/227, 73, 53/60, 70, 58  where an allocation is committed / paged / transferred

Usage:
  vram_redirection_report.py DIR [--process Heaven.exe] [--csv frames.csv] [--frames 20]
  vram_redirection_report.py --selftest
"""
import argparse
import collections
import csv
import datetime
import io
import os
import re
import statistics
import sys
import xml.etree.ElementTree as ET
import zipfile

DXGKRNL = '802ec45a-1e99-4b83-9920-87c98277ba9d'
DXGI = 'ca11c036-0102-4a2d-a6ad-f03cfed5d3c9'
PM_NAMES = {1: 'REDIRECTED_GDI', 2: 'REDIRECTED_FLIP', 3: 'REDIRECTED_BLT', 4: 'REDIRECTED_VISTABLT',
            5: 'SCREENCAPTUREFENCE', 6: 'REDIRECTED_GDI_SYSMEM', 7: 'REDIRECTED_COMPOSITION',
            8: 'SURFACECOMPLETE', 9: 'FLIPMANAGER'}
# DXGK_SEGMENTFLAGS (d3dkmddi.h); ReportSegment's Flags is printed raw as well, in case it is
# dxgkrnl's internal form.
SEG_BITS = [(0x1, 'Aperture'), (0x2, 'Agp'), (0x4, 'CpuVisible'), (0x8, 'UseBanking'),
            (0x10, 'CacheCoherent'), (0x20, 'PitchAlignment'), (0x40, 'PopulatedFromSystemMemory'),
            (0x80, 'PreservedDuringStandby'), (0x100, 'PreservedDuringHibernate'),
            (0x200, 'PartiallyPreservedDuringHibernate'), (0x400, 'DirectFlip'), (0x800, 'Use64KBPages'),
            (0x1000, 'ReservedSysMem'), (0x2000, 'SupportsCpuHostAperture'),
            (0x4000, 'SupportsCachedCpuHostAperture'), (0x8000, 'ApplicationTarget'),
            (0x10000, 'VprSupported'), (0x20000, 'VprPreservedDuringStandby'),
            (0x40000, 'EncryptedPagingSupported'), (0x80000, 'LocalBudgetGroup'),
            (0x100000, 'NonLocalBudgetGroup'), (0x200000, 'PopulatedByReservedDDRByFirmware')]
# Pointer fields that name something shared by many allocations: never used to join.
NOJOIN = {'hProcessId', 'hDevice', 'pDxgAdapter', 'hAdapter', 'hDxgResource', 'hDxgSharedResource',
          'hThunkResource', 'PrivateRuntimeResourceHandle', 'pVirtualAddress', 'hProcessAllocDetails',
          'pSectionObject', 'PageTableOrDirectory', 'hContext', 'pDmaBuffer', 'hDmaBuffer'}
THUNK = {'hThunkAllocation'}   # D3DKMT handles: unique per process only


def strip(tag):
    return tag.rsplit('}', 1)[-1]


def num(v):
    if v is None:
        return None
    v = v.strip()
    try:
        if v.lower().startswith('0x'):
            return int(v, 16)
        if v.lower() in ('true', 'false'):
            return 1 if v.lower() == 'true' else 0
        return int(v)
    except ValueError:
        return None


_TS = re.compile(r'(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)?')


def ts_ns(s):
    m = _TS.match(s or '')
    if not m:
        return None
    y, mo, d, h, mi, se, frac, tz = m.groups()
    dt = datetime.datetime(int(y), int(mo), int(d), int(h), int(mi), int(se), tzinfo=datetime.timezone.utc)
    ns = int(dt.timestamp()) * 1_000_000_000 + int(((frac or '') + '000000000')[:9])
    if tz and tz != 'Z':
        sign = 1 if tz[0] == '+' else -1
        ns -= sign * (int(tz[1:3]) * 3600 + int(tz[4:6]) * 60) * 1_000_000_000
    return ns


class Ev:
    __slots__ = ('prov', 'id', 'ts', 'pid', 'tid', 'task', 'opcode', 'd')

    def __init__(self, prov, eid, ts, pid, tid, task, opcode, d):
        self.prov, self.id, self.ts, self.pid, self.tid = prov, eid, ts, pid, tid
        self.task, self.opcode, self.d = task, opcode, d

    def n(self, k):
        return num(self.d.get(k))

    def has(self, *ks):
        return all(k in self.d for k in ks)


def open_xml(path):
    if os.path.isdir(path):
        for name in ('vram.xml.zip', 'vram.xml'):
            p = os.path.join(path, name)
            if os.path.exists(p):
                path = p
                break
        else:
            sys.exit(f'no vram.xml(.zip) in {path}')
    if path.endswith('.zip'):
        z = zipfile.ZipFile(path)
        inner = [n for n in z.namelist() if n.lower().endswith('.xml')][0]
        return z.open(inner)
    return open(path, 'rb')


def read_events(f):
    out = []
    for _, el in ET.iterparse(f, events=('end',)):
        if strip(el.tag) != 'Event':
            continue
        prov = eid = ts = pid = tid = None
        task = opcode = ''
        d = {}
        for c in el:
            t = strip(c.tag)
            if t == 'System':
                for s in c:
                    st = strip(s.tag)
                    if st == 'Provider':
                        prov = (s.get('Guid') or s.get('Name') or '').strip('{}').lower()
                        if s.get('Name', '').lower() == 'microsoft-windows-dxgkrnl':
                            prov = DXGKRNL
                        elif s.get('Name', '').lower() == 'microsoft-windows-dxgi':
                            prov = DXGI
                    elif st == 'EventID':
                        eid = num(s.text)
                    elif st == 'TimeCreated':
                        ts = ts_ns(s.get('SystemTime'))
                    elif st == 'Execution':
                        pid, tid = num(s.get('ProcessID')), num(s.get('ThreadID'))
            elif t in ('EventData', 'UserData'):
                for dd in c.iter():
                    if strip(dd.tag) == 'Data' and dd.get('Name'):
                        d[dd.get('Name')] = (dd.text or '').strip()
                    elif dd is not c and len(dd) == 0 and strip(dd.tag) != 'Data':
                        d.setdefault(strip(dd.tag), (dd.text or '').strip())
            elif t == 'RenderingInfo':
                for s in c:
                    st = strip(s.tag)
                    if st == 'Task':
                        task = (s.text or '').strip()
                    elif st == 'Opcode':
                        opcode = (s.text or '').strip()
        el.clear()
        if ts is None:
            continue
        out.append(Ev(prov or '', eid, ts, pid, tid, task, opcode, d))
    out.sort(key=lambda e: e.ts)
    return out


def kind(e):
    """Classify an event by provider, id and field names."""
    d = e.d
    if e.prov == DXGI:
        return {42: 'dxgi_present_start', 43: 'dxgi_present_stop'}.get(e.id)
    if e.prov != DXGKRNL:
        if 'NewThreadId' in d and 'OldThreadId' in d:
            return 'cswitch'
        if 'ImageFileName' in d and 'ProcessId' in d:
            return 'process'
        return None
    if 'uiLockStatus' in d or ('pAlloc' in d and 'Location' in d):
        return 'lock'
    if e.id in (42, 341) and ('hAllocationHandle' in d or 'pAlloc' in d):
        return 'unlock'
    if 'hDestAllocation' in d and 'bRedirectedPresent' in d:
        return 'blit'
    if 'hDstAllocHandle' in d and 'hSrcAllocHandle' in d:
        return 'present'
    if 'PacketType' in d and 'DmaBufferSize' in d:
        return 'qp_start'
    if 'PacketType' in d and 'bPreempted' in d:
        return 'qp_stop'
    if 'Model' in d and 'Token' in d and e.opcode.lower().startswith('start') or (
            'Model' in d and 'Token' in d and e.id in (171, 215)):
        return 'ph_start'
    if 'allocSize' in d and 'PreferredSegment' in d:
        return 'adapter_alloc_stop' if (e.id == 34 or e.opcode.lower() == 'stop') else 'adapter_alloc'
    if 'hVidMmAlloc' in d and 'hThunkAllocation' in d:
        return 'device_alloc'
    if 'ulSegmentId' in d and 'CommitLimit' in d:
        return 'segment'
    if 'ulSegmentId' in d and 'SegmentOffset' in d:
        return 'committed'
    if 'uiSegmentId' in d:
        return 'pagein'
    if 'DestinationSegmentId' in d:
        return 'transfer'
    if 'OffsetInPages' in d and 'SegmentId' in d:
        return 'map_aperture'
    if 'SegmentId' in d and 'SegmentOffset' in d and 'Status' in d:
        return 'placed'
    if 'bTemporary' in d:
        return 'aperture_mapping'
    if e.id in (105, 106) and 'Function' in d:
        return 'prof_start' if e.id == 105 else 'prof_stop'
    if e.id in (348, 349) and 'Function' in d:
        return 'prof_start' if e.id == 348 else 'prof_stop'
    if 'hGlobalAllocationHandle' in d and e.id == 74:
        return 'evict'
    return None


class Allocs:
    """Union of every handle/pointer the allocation events give for one allocation."""

    def __init__(self):
        self.parent = {}
        self.info = collections.defaultdict(dict)        # root -> merged fields
        self.where = collections.defaultdict(list)       # root -> [(ts, how, segment)]
        self.fields = collections.defaultdict(set)       # key -> field names it came from

    def _find(self, k):
        p = self.parent.setdefault(k, k)
        while p != self.parent[p]:
            self.parent[p] = self.parent[self.parent[p]]
            p = self.parent[p]
        self.parent[k] = p
        return p

    def keys_of(self, e):
        ks = []
        for name, v in e.d.items():
            if name in NOJOIN:
                continue
            x = num(v)
            if not x or x < 0x10000:
                continue
            if name in THUNK:
                ks.append(('t', e.n('hProcessId') or e.pid, x))
            elif name.startswith(('h', 'p')) and v.lower().startswith('0x'):
                ks.append(('p', x))
            else:
                continue
            self.fields[ks[-1]].add(name)
        return ks

    def add(self, e):
        ks = self.keys_of(e)
        if not ks:
            return None
        r = self._find(ks[0])
        for k in ks[1:]:
            r2 = self._find(k)
            if r2 != r:
                self.parent[r2] = r
                self.info[r].update(self.info.pop(r2, {}))
                self.where[r].extend(self.where.pop(r2, []))
        self.info[r].update(e.d)
        return r

    def root(self, value, pid=None):
        x = num(value) if isinstance(value, str) else value
        if not x:
            return None
        for k in (('p', x), ('t', pid, x)):
            if k in self.parent:
                return self._find(k)
        return None

    def note(self, value, ts, how, seg, pid=None):
        r = self.root(value, pid)
        if r is None:
            r = self._find(('p', num(value) if isinstance(value, str) else value))
        self.where[r].append((ts, how, seg))
        return r


def seg_desc(segs, sid):
    if sid is None:
        return '?'
    s = segs.get(sid)
    if sid == 0 and not s:
        return '0 (no segment: system memory / not resident)'
    if not s:
        return f'{sid} (not reported)'
    fl = s.get('flags') or 0
    names = [n for b, n in SEG_BITS if fl & b]
    grp = s.get('group')
    grp_s = {0: 'local', 1: 'non-local'}.get(grp, str(grp))
    return f"{sid} ({'aperture' if fl & 1 else 'memory'} segment, {s.get('size', 0) >> 20} MB, group {grp_s}, flags 0x{fl:x} {'|'.join(names)})"


def pct(v, p):
    if not v:
        return float('nan')
    v = sorted(v)
    return v[min(len(v) - 1, max(0, int(round(p / 100 * len(v) + 0.5)) - 1))]


def fmt_us(v):
    return f'{v:8.1f}' if v == v else '     n/a'


def report(evs, procs, target, csv_path=None, show=20, out=sys.stdout):
    P = lambda *a: print(*a, file=out)
    for e in evs:
        if kind(e) == 'process':
            procs.setdefault(e.n('ProcessId'), e.d.get('ImageFileName', '?'))
    pname = lambda pid: procs.get(pid, '?')
    tpids = {p for p, n in procs.items() if n.lower().removesuffix('.exe') == target.lower().removesuffix('.exe')}

    allocs, segs = Allocs(), {}
    kinds = collections.Counter()
    for e in evs:
        k = kind(e)
        kinds[k or f'other:{e.prov[:8]}:{e.id}'] += 1
        if k in ('adapter_alloc', 'device_alloc', 'adapter_alloc_stop'):
            r = allocs.add(e)
            if r is not None and k == 'adapter_alloc_stop':
                allocs.info[r]['_destroyed'] = e.ts
        elif k == 'segment':
            segs[e.n('ulSegmentId')] = {'size': e.n('Size') or 0, 'flags': e.n('Flags') or 0,
                                         'group': e.n('MemorySegmentGroup'), 'base': e.n('BaseAddress'),
                                         'cpu': e.n('CpuTranslatedAddress'), 'commit': e.n('CommitLimit')}
        elif k == 'committed':
            h = e.d.get('hGlobalAllocationHandle') or e.d.get('hAllocationHandle')
            allocs.note(h, e.ts, 'committed', e.n('ulSegmentId'), e.n('hProcessId'))
        elif k == 'pagein':
            allocs.note(e.d.get('hAllocationHandle'), e.ts, 'page-in', e.n('uiSegmentId'), e.pid)
        elif k == 'transfer':
            allocs.note(e.d.get('hAllocationGlobalHandle'), e.ts,
                        f"transfer {e.n('SourceSegmentId')}->{e.n('DestinationSegmentId')}",
                        e.n('DestinationSegmentId'))
        elif k == 'placed':
            allocs.note(e.d.get('hAllocationGlobalHandle'), e.ts, 'placed', e.n('SegmentId'))
        elif k == 'map_aperture':
            allocs.note(e.d.get('hAllocationGlobalHandle'), e.ts, 'map-aperture', e.n('SegmentId'))
        elif k == 'evict':
            allocs.note(e.d.get('hGlobalAllocationHandle'), e.ts, 'evict', 0)

    span = (evs[-1].ts - evs[0].ts) / 1e9 if evs else 0
    P(f'events {len(evs)} over {span:.2f} s; target {target} pids {sorted(tpids) or "not found in process list"}')
    P('event kinds: ' + ', '.join(f'{k}={v}' for k, v in sorted(kinds.items(), key=lambda x: -x[1])[:28]))
    byproc = collections.Counter()
    for e in evs:
        k = kind(e)
        if k not in ('process', 'cswitch'):
            byproc[(pname(e.pid), k or f'{e.prov[:8]}:{e.id}:{e.task}')] += 1
    P('busiest (process, event) pairs:')
    for (pn, k), c in byproc.most_common(24):
        P(f'  {pn:24s} {k:40s} {c}')
    P('\nsegments (ReportSegment):')
    for sid in sorted(segs):
        P('  ' + seg_desc(segs, sid))
    if not segs:
        P('  none reported (rundown missing: check the capture-state keywords)')

    # The redirected Blts of the target: destination = the redirection surface.
    blits = [e for e in evs if kind(e) == 'blit' and (not tpids or e.pid in tpids)]
    # Composed: Copy with GPU GDI is a Blit with bRedirectedPresent 0 followed by PresentHistory model
    # REDIRECTED_BLT (PresentMon's rule); with CPU GDI the flag is 1. Either way the destination is the
    # surface DWM composes from.
    red = [e for e in blits if e.n('bRedirectedPresent')] or blits
    presents = [e for e in evs if kind(e) == 'present' and (not tpids or e.pid in tpids)]
    dsts = collections.Counter(e.d['hDestAllocation'] for e in red) or \
        collections.Counter(e.d['hDstAllocHandle'] for e in presents)
    srcs = collections.Counter(e.d['hSourceAllocation'] for e in red)
    ph = collections.Counter(PM_NAMES.get(e.n('Model'), e.d.get('Model')) for e in evs
                             if kind(e) == 'ph_start' and (not tpids or e.pid in tpids))
    P(f'\npresent models (PresentHistory start, target): {dict(ph)}')
    P(f'Blit events: {len(blits)} (bRedirectedPresent=1: {sum(1 for e in blits if e.n("bRedirectedPresent"))}); '
      f'Present events {len(presents)}')
    dest_roots = set()
    for h, c in dsts.most_common(4):
        r = allocs.root(h)
        dest_roots.add(r if r is not None else ('p', num(h)))
        P(f'\nredirection surface candidate {h} ({c} Blts):')
        describe_alloc(P, allocs, segs, r, h)
    for h, c in srcs.most_common(2):
        P(f'\nBlt source (app back buffer) {h} ({c} Blts):')
        describe_alloc(P, allocs, segs, allocs.root(h), h)

    # Locks on the redirection surface (and on everything, for who-locks-what).
    locks = collections.defaultdict(list)    # root -> [(ts, ev)]
    lock_any = collections.Counter()
    open_lock = {}
    lock_spans = collections.defaultdict(list)
    unresolved = collections.Counter()
    for e in evs:
        k = kind(e)
        if k not in ('lock', 'unlock'):
            continue
        h = e.d.get('hAllocationHandle') or e.d.get('pAlloc')
        r = allocs.root(h, e.pid)
        if r is None:
            unresolved[h] += 1
            r = ('p', num(h))
        if k == 'lock':
            locks[r].append(e)
            lock_any[(pname(e.pid), r in dest_roots)] += 1
            open_lock[(r, e.tid)] = e
        else:
            s = open_lock.pop((r, e.tid), None)
            if s:
                lock_spans[r].append(((e.ts - s.ts) / 1e3, s))
    P('\nLock events by process (on the redirection surface? yes/no):')
    for (pn, isdst), c in lock_any.most_common(12):
        P(f'  {pn:24s} {"yes" if isdst else "no ":3s} {c}')
    for r in dest_roots:
        ls = locks.get(r, [])
        P(f'\nlocks on {fmt_root(r)}: {len(ls)}')
        by = collections.Counter((pname(e.pid), e.tid, e.d.get('dwFlags') or e.d.get('Flags'),
                                  e.d.get('uiLockStatus') or e.d.get('Location')) for e in ls)
        for (pn, tid, fl, st), c in by.most_common(8):
            P(f'  {pn} tid {tid}: {c} locks, flags {fl} status/location {st}')
        sp = [s for s, _ in lock_spans.get(r, [])]
        if sp:
            P(f'  lock->unlock us: p50 {pct(sp, 50):.1f} p99 {pct(sp, 99):.1f} max {max(sp):.1f}')
    if unresolved:
        P(f'  (lock handles not found in the allocation rundown: {len(unresolved)} distinct, '
          f'top {unresolved.most_common(3)})')

    # Per frame: the target's Present calls (DXGI start/stop on one thread).
    by_tid = collections.defaultdict(list)
    for e in evs:
        if e.tid is not None and (not tpids or e.pid in tpids):
            by_tid[e.tid].append(e)
    qp_start = {}
    qp_spans = []       # (start_ts, stop_ts, start_ev)
    for e in evs:
        k = kind(e)
        if k == 'qp_start':
            qp_start[e.d.get('pQueuePacket')] = e
        elif k == 'qp_stop':
            s = qp_start.pop(e.d.get('pQueuePacket'), None)
            if s:
                qp_spans.append((s.ts, e.ts, s))
    qp_by_ptr_start = {(ev.d.get('pQueuePacket'), s): (s, t) for s, t, ev in qp_spans}
    dma_to_qp = {}
    for s, t, ev in qp_spans:
        dma_to_qp.setdefault(ev.d.get('hDmaBuffer'), []).append((s, t, ev))
    prof_names = collections.Counter()

    frames = []
    for tid, tev in by_tid.items():
        start = None
        prof_open = {}
        cur = None
        for e in tev:
            k = kind(e)
            if k == 'dxgi_present_start':
                start = e
                cur = {'tid': tid, 'start': e.ts, 'events': [], 'prof': collections.Counter(),
                       'blit': None, 'locks_dst': 0}
                prof_open = {}
            elif k == 'dxgi_present_stop' and cur:
                cur['stop'] = e.ts
                cur['dur'] = (e.ts - cur['start']) / 1e3
                frames.append(cur)
                cur = None
            elif cur is not None:
                cur['events'].append(e)
                if k == 'blit' and cur['blit'] is None:
                    cur['blit'] = e
                elif k == 'lock':
                    r = allocs.root(e.d.get('hAllocationHandle') or e.d.get('pAlloc'), e.pid)
                    if (r if r is not None else ('p', e.n('hAllocationHandle') or e.n('pAlloc'))) in dest_roots:
                        cur['locks_dst'] += 1
                elif k == 'prof_start':
                    prof_open[e.d.get('Function')] = e.ts
                elif k == 'prof_stop':
                    f0 = prof_open.pop(e.d.get('Function'), None)
                    if f0 is not None:
                        cur['prof'][e.d.get('Function')] += (e.ts - f0) / 1e3
                        prof_names[e.d.get('Function')] += 1
    frames.sort(key=lambda f: f['start'])
    if not frames:
        P('\nno DXGI Present start/stop pairs for the target (DXGI provider missing, or wrong --process)')
        return
    # Previous Blt packet's retire (QueuePacket stop) relative to this frame's longest stall.
    qp_stops = sorted(t for _, t, _ in qp_spans)
    import bisect
    rows = []
    for i, f in enumerate(frames):
        ts = [f['start']] + [e.ts for e in f['events']] + [f['stop']]
        gaps = [(ts[j + 1] - ts[j], j) for j in range(len(ts) - 1)]
        g, j = max(gaps) if gaps else (0, 0)
        gap_end = ts[j + 1]
        before = f['events'][j - 1] if j >= 1 else None
        after = f['events'][j] if j < len(f['events']) else None
        # nearest packet retire at or before the end of the stall
        k = bisect.bisect_right(qp_stops, gap_end) - 1
        retire_lead = (gap_end - qp_stops[k]) / 1e3 if k >= 0 and qp_stops[k] >= ts[j] else None
        bl = f['blit']
        pkt = None
        own = None          # (start, stop) of this frame's Blt packet
        if bl is not None:
            # the packet the Present submitted: same thread, first QueuePacket start after the Blit
            for e in f['events']:
                if kind(e) == 'qp_start' and e.ts >= bl.ts:
                    own = qp_by_ptr_start.get((e.d.get('pQueuePacket'), e.ts))
                    break
            if own is None:
                for s0, t0, ev in sorted(dma_to_qp.get(bl.d.get('pDmaBuffer'), []), key=lambda x: x[0]):
                    if s0 >= bl.ts - 50_000:
                        own = (s0, t0)
                        break
            if own is not None:
                pkt = (own[1] - own[0]) / 1e3
        f['own'] = own
        prev_own = frames[i - 1].get('own') if i else None
        lo, hi = ts[j], gap_end + 100_000
        if own is not None and lo <= own[1] <= hi:
            ends_on = 'own Blt retire'
        elif prev_own is not None and lo <= prev_own[1] <= hi:
            ends_on = 'previous Blt retire'
        elif retire_lead is not None:
            ends_on = 'other packet retire'
        else:
            ends_on = 'no retire'
        rows.append({
            'frame': i, 'tid': f['tid'], 'start_us': (f['start'] - frames[0]['start']) / 1e3,
            'interval_us': ((f['start'] - frames[i - 1]['start']) / 1e3) if i else float('nan'),
            'present_us': f['dur'],
            'to_blit_us': ((bl.ts - f['start']) / 1e3) if bl is not None else float('nan'),
            'stall_us': g / 1e3,
            'stall_after': ev_name(before), 'stall_before': ev_name(after),
            'retire_to_wake_us': retire_lead if retire_lead is not None else float('nan'),
            'blt_packet_us': pkt if pkt is not None else float('nan'),
            'dst_locks': f['locks_dst'],
            'stall_ends_on': ends_on,
            'prof': '; '.join(f'{k}={v:.0f}' for k, v in f['prof'].most_common(4)),
        })
    P(f'\nframes (target Present calls): {len(rows)}')
    for col in ('interval_us', 'present_us', 'to_blit_us', 'stall_us', 'retire_to_wake_us', 'blt_packet_us'):
        v = [r[col] for r in rows if r[col] == r[col]]
        if v:
            P(f'  {col:18s} n={len(v):5d} p50 {pct(v, 50):8.1f} p90 {pct(v, 90):8.1f} p99 {pct(v, 99):8.1f} mean {statistics.mean(v):8.1f}')
    st = collections.Counter((r['stall_after'], r['stall_before']) for r in rows)
    P('  longest stall inside Present sits between (after -> before):')
    for (a, b), c in st.most_common(5):
        P(f'    {c:5d}  {a} -> {b}')
    pt = collections.Counter()
    for f in frames:
        pt.update(f['prof'])
    if pt:
        P('  dxgkrnl profiler spans inside Present (total us over all frames, per frame mean):')
        for k, v in pt.most_common(8):
            P(f'    {k}: {v:.0f} total, {v / len(frames):.1f} per frame ({prof_names[k]} spans)')
    eo = collections.Counter(r['stall_ends_on'] for r in rows)
    P('  the longest stall ends on: ' + ', '.join(f'{k} {v}' for k, v in eo.most_common()))
    near = [r for r in rows if r['retire_to_wake_us'] == r['retire_to_wake_us']]
    if near:
        tight = sum(1 for r in near if r['retire_to_wake_us'] < 100)
        P(f'  stall ends within 100 us of a packet retire (QueuePacket stop): {tight}/{len(rows)} frames')
    dl = sum(1 for r in rows if r['dst_locks'])
    P(f'  frames with a Lock on the redirection surface inside Present: {dl}/{len(rows)}')
    P(f'\nfirst {show} frames:')
    P('  frame  interval  present  to_blit   stall  retire->wake  blt_pkt  dstlk  stall between / profiler')
    for r in rows[:show]:
        P(f"  {r['frame']:5d} {fmt_us(r['interval_us'])} {fmt_us(r['present_us'])} {fmt_us(r['to_blit_us'])} "
          f"{fmt_us(r['stall_us'])} {fmt_us(r['retire_to_wake_us'])}     {fmt_us(r['blt_packet_us'])} {r['dst_locks']:5d}  "
          f"{r['stall_after']} -> {r['stall_before']}  {r['prof']}")
    verdict(P, rows, lock_any, dest_roots, allocs, segs)
    if csv_path:
        with open(csv_path, 'w', newline='') as fh:
            w = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
            w.writeheader()
            w.writerows(rows)
        P(f'\nper-frame CSV: {csv_path}')


def ev_name(e):
    if e is None:
        return 'edge'
    k = kind(e)
    if k in ('prof_start', 'prof_stop'):
        return f"{k}({e.d.get('Function')})"
    return k or f'{e.id}'


def fmt_root(r):
    if r is None:
        return '?'
    return f'0x{r[1]:x}' if r[0] == 'p' else f'thunk 0x{r[2]:x} (pid {r[1]})'


def describe_alloc(P, allocs, segs, r, h):
    if r is None:
        P(f'  not in the allocation rundown (handle {h}); segment unknown from this capture')
        return
    i = allocs.info.get(r, {})
    keep = ['allocSize', 'Width', 'Height', 'Pitch', 'Format', 'Flags', 'UsageFlags', 'dwReadSegment',
            'dwWriteSegment', 'PreferredSegment', 'dwEvictionSegment', 'Priority', 'hDxgSharedResource',
            'BackingStoreWasPinned', 'hProcessId']
    P('  ' + ' '.join(f'{k}={i[k]}' for k in keep if k in i))
    for k in ('dwReadSegment', 'dwWriteSegment'):
        m = num(i.get(k))
        if m:
            ids = [b + 1 for b in range(32) if m & (1 << b)]   # bit 0 = segment 1 (DXGK_ALLOCATIONINFO)
            P(f'  {k} set: {ids} -> ' + '; '.join(seg_desc(segs, b) for b in ids))
    pref = num(i.get('PreferredSegment'))
    if pref:
        # DXGK_SEGMENTPREFERENCE: five 5-bit segment ids, then direction bits
        ids = [(pref >> (5 * j)) & 0x1f for j in range(5)]
        P(f'  PreferredSegment 0x{pref:x} -> ids {[x for x in ids if x]}')
    w = sorted(allocs.where.get(r, []))
    if w:
        cur = collections.Counter(s for _, _, s in w)
        P(f'  placement events: {len(w)}; segments seen {dict(cur)}; last: {w[-1][1]} -> ' + seg_desc(segs, w[-1][2]))
    else:
        P('  no placement events (never paged during the capture; committed-rundown missing?)')


def verdict(P, rows, lock_any, dest_roots, allocs, segs):
    P('\nreading:')
    st = [r['stall_us'] for r in rows]
    pr = [r['present_us'] for r in rows]
    if not st:
        return
    P(f'  median Present {pct(pr, 50):.0f} us, of which the longest single stall {pct(st, 50):.0f} us '
      f'({100 * pct(st, 50) / max(1e-9, pct(pr, 50)):.0f}%).')
    near = [r for r in rows if r['retire_to_wake_us'] == r['retire_to_wake_us'] and r['retire_to_wake_us'] < 100]
    eo = collections.Counter(r['stall_ends_on'] for r in rows).most_common(1)[0]
    if len(near) > 0.6 * len(rows):
        P(f'  The stall ends right after a DMA packet retires ({eo[0]} in {eo[1]}/{len(rows)} frames): the app '
          'waits for a Blt to retire (serialisation on the destination) - the CPU-access hypothesis holds if '
          'the stall is bracketed by Lock/Unlock (or a profiler CPU-access span) on the redirection surface.')
    else:
        P('  The stall does not line up with packet retires in most frames: look at the stall-between '
          'column; the wait is something else.')
    for r in dest_roots:
        w = allocs.where.get(r, [])
        if w:
            sid = sorted(w)[-1][2]
            s = segs.get(sid)
            if sid == 0 or (s and s.get('flags', 0) & 1):
                P(f'  The redirection surface sits in {seg_desc(segs, sid)}: system memory / aperture, '
                  'so GPU writes go over PCIe and CPU access needs the GPU idle on it.')
            elif s:
                P(f'  The redirection surface sits in {seg_desc(segs, sid)}.')


def load_procs(d):
    p = {}
    f = os.path.join(d, 'processes.txt') if os.path.isdir(d) else None
    if f and os.path.exists(f):
        for line in open(f, encoding='utf-8', errors='replace'):
            a = line.rstrip('\r\n').split('\t')
            if len(a) == 2 and a[0].isdigit():
                p[int(a[0])] = a[1] + ('' if a[1].lower().endswith('.exe') else '.exe')
    return p


SELFTEST_XML = '''<?xml version="1.0" encoding="UTF-8"?>
<Events>{}</Events>'''


def _ev(pid, tid, t_us, eid, data, prov='Microsoft-Windows-DxgKrnl', opcode='Info'):
    base = datetime.datetime(2026, 10, 7, 20, 0, 0, tzinfo=datetime.timezone.utc)
    ns = int(t_us * 1000)
    s = base.strftime('%Y-%m-%dT%H:%M:%S') + f'.{ns:09d}Z'
    dd = ''.join(f'<Data Name="{k}">{v}</Data>' for k, v in data.items())
    return (f'<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System>'
            f'<Provider Name="{prov}"/><EventID>{eid}</EventID><TimeCreated SystemTime="{s}"/>'
            f'<Execution ProcessID="{pid}" ThreadID="{tid}"/></System><EventData>{dd}</EventData>'
            f'<RenderingInfo Culture="en-US"><Opcode>{opcode}</Opcode></RenderingInfo></Event>')


def selftest():
    """Synthetic trace: 3 frames, each Present waits for the previous Blt packet's retire."""
    A, DST, SRC = 4242, '0xFFFF800011110000', '0xFFFF800022220000'
    x = [_ev(4, 8, 0, 78, {'ulSegmentId': 1, 'pDxgAdapter': '0x1', 'BaseAddress': '0x0',
                           'CpuTranslatedAddress': '0x0', 'Size': str(256 << 20), 'NbOfBanks': 0,
                           'Flags': '0x1', 'CommitLimit': '0x0', 'SystemMemoryEndAddress': '0x0',
                           'MemorySegmentGroup': 1}),
         _ev(4, 8, 1, 35, {'hProcessId': '0x1f0', 'hDevice': '0x9', 'pDxgAdapter': '0x1', 'Flags': '0x0',
                           'allocSize': str(5760000), 'dwReadSegment': '0x1', 'dwWriteSegment': '0x1',
                           'PreferredSegment': '0x1', 'hVidMmGlobalAlloc': '0xFFFF800033330000',
                           'hDxgGlobalAlloc': DST, 'Width': 1600, 'Height': 900}, opcode='DC_Start'),
         _ev(4, 8, 2, 227, {'hGlobalAllocationHandle': '0xFFFF800033330000', 'ulSegmentId': 1,
                            'SegmentOffset': '0x0'}, opcode='DC_Start')]
    pkt = 0
    for f in range(3):
        t0 = 1000 + f * 4000
        x.append(_ev(A, 77, t0, 42, {'pIDXGISwapChain': '0x5', 'Flags': 0, 'SyncInterval': 0},
                     prov='Microsoft-Windows-DXGI', opcode='Start'))
        x.append(_ev(A, 77, t0 + 20, 41, {'hDevice': '0x9', 'hAllocationHandle': DST, 'dwFlags': '0x1',
                                          'uiLockStatus': 0}))
        if f:
            x.append(_ev(4, 9, t0 + 1500, 180, {'hContext': '0x7', 'PacketType': 0, 'SubmitSequence': f,
                                                'bPreempted': 'false', 'bTimeouted': 'false',
                                                'pQueuePacket': f'0x{0x900 + pkt - 1:x}'}, opcode='Stop'))
        x.append(_ev(A, 77, t0 + 1520, 42, {'hDevice': '0x9', 'hAllocationHandle': DST, 'dwFlags': '0x0'}))
        x.append(_ev(A, 77, t0 + 1530, 166, {'hwnd': '0x10', 'pDmaBuffer': f'0x{0x700 + f:x}',
                                             'PresentHistoryToken': 0, 'hSourceAllocation': SRC,
                                             'hDestAllocation': DST, 'bSubmit': 'true',
                                             'bRedirectedPresent': 'true', 'Flags': 0}))
        x.append(_ev(A, 77, t0 + 1540, 178, {'hContext': '0x7', 'PacketType': 0, 'SubmitSequence': f + 1,
                                             'DmaBufferSize': 64, 'AllocationListSize': 2,
                                             'PatchLocationListSize': 0, 'bPresent': 'true',
                                             'hDmaBuffer': f'0x{0x700 + f:x}',
                                             'pQueuePacket': f'0x{0x900 + pkt:x}', 'ProgressFenceValue': 1},
                     opcode='Start'))
        pkt += 1
        x.append(_ev(A, 77, t0 + 1560, 43, {'Result': 0}, prov='Microsoft-Windows-DXGI', opcode='Stop'))
    evs = read_events(io.BytesIO(SELFTEST_XML.format(''.join(x)).encode()))
    buf = io.StringIO()
    report(evs, {A: 'Heaven.exe', 4: 'System.exe'}, 'Heaven.exe', out=buf)
    s = buf.getvalue()
    checks = ['bRedirectedPresent=1: 3', 'aperture segment', 'Heaven.exe', 'frames (target Present calls): 3',
              'within 100 us of a packet retire (QueuePacket stop): 2/3', 'dwReadSegment set: [1]',
              'Heaven.exe tid 77: 3 locks', 'previous Blt retire 2']
    bad = [c for c in checks if c not in s]
    print(s)
    if bad:
        print('SELFTEST FAILED, missing:', bad)
        return 1
    print('selftest ok')
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('dir', nargs='?', help='capture folder (or vram.xml / vram.xml.zip)')
    ap.add_argument('--process', default='Heaven.exe')
    ap.add_argument('--csv')
    ap.add_argument('--frames', type=int, default=20)
    ap.add_argument('--selftest', action='store_true')
    a = ap.parse_args()
    if a.selftest:
        return selftest()
    if not a.dir:
        ap.error('capture folder required')
    with open_xml(a.dir) as f:
        evs = read_events(f)
    report(evs, load_procs(a.dir), a.process, a.csv, a.frames)
    return 0


if __name__ == '__main__':
    sys.exit(main())

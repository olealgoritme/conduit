#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Summarize an rmtrace log: TSGs, ctxshares (flags, VEID), channels (engine,
flags, ctxshare/VAS, runqueue/thread), objects per channel, failures.

usage: summarize.py <file.rmtrace.log>
"""
import re
import sys

ENG = {0x1: 'GR0', 0x9: 'COPY0', 0xd: 'COPY4', 0xe: 'COPY5'}
for i in range(20):
    ENG.setdefault(0x9 + i, 'COPY%d' % i)  # NV2080_ENGINE_TYPE_COPY0 = 0x9


def eng(v):
    return ENG.get(v, '0x%x' % v)


def chflags(f):
    out = []
    if f & 0x10:
        out.append('RUNQUEUE_ONE')
    thr = (f >> 28) & 3
    if thr:
        out.append('THREAD_%d' % thr)
    rest = f & ~0x30000010
    if rest:
        out.append('other 0x%x' % rest)
    return ('0x%x ' % f + '|'.join(out)) if out else '0x%x' % f


def main(path):
    lines = open(path, errors='replace').read().splitlines()
    tsgs, cs, ch, objs, order = {}, {}, {}, {}, []
    pending = None
    fails = []
    for i, l in enumerate(lines):
        m = re.search(r'==== MARK (.*)', l)
        if m:
            order.append(('mark', m.group(1)))
            continue
        m = re.search(r'ALLOC\d\d client \S+ parent (0x[0-9a-f]+)\((\S+)\) new (0x[0-9a-f]+) class (0x[0-9a-f]+) (\S+).*-> status (0x[0-9a-f]+)', l)
        if m:
            par, parcls, new, cls, name, st = m.groups()
            cls = int(cls, 16)
            st = int(st, 16)
            pending = None
            if st:
                fails.append(l.strip())
            if cls == 0xa06c:
                tsgs[new] = {'parent': par}
                pending = ('tsg', new)
                order.append(('tsg', new))
            elif cls == 0x9067:
                cs[new] = {'tsg': par}
                pending = ('cs', new)
            elif (cls & 0xff) == 0x6f and cls >= 0x906f:
                ch[new] = {'parent': par, 'parentcls': parcls, 'cls': cls}
                pending = ('ch', new)
                order.append(('ch', new))
            elif par in ch:
                objs.setdefault(par, []).append('%s 0x%x%s' % (name, cls, '' if not st else ' FAILED 0x%x' % st))
            continue
        if pending:
            kind, h = pending
            m = re.search(r'tsg: .* hVASpace (0x[0-9a-f]+) engineType (0x[0-9a-f]+)', l)
            if m and kind == 'tsg':
                tsgs[h]['vas'] = m.group(1)
                tsgs[h]['eng'] = int(m.group(2), 16)
            m = re.search(r'ctxshare: .* flags (0x[0-9a-f]+) \((\S+)\) subctxId (IN|OUT) (0x[0-9a-f]+)', l)
            if m and kind == 'cs':
                cs[h]['flags'] = m.group(2)
                cs[h]['in' if m.group(3) == 'IN' else 'out'] = int(m.group(4), 16)
            m = re.search(r'chan: .* gpFifoEntries (\d+) flags (0x[0-9a-f]+)', l)
            if m and kind == 'ch':
                ch[h]['flags'] = int(m.group(2), 16)
                ch[h]['entries'] = int(m.group(1))
            m = re.search(r'hContextShare (0x[0-9a-f]+) hVASpace (0x[0-9a-f]+)', l)
            if m and kind == 'ch':
                ch[h]['cs'] = m.group(1)
                ch[h]['vas'] = m.group(2)
            m = re.search(r'^\s+engineType (0x[0-9a-f]+) cid (\d+) subDeviceId (0x[0-9a-f]+)', l)
            if m and kind == 'ch':
                ch[h]['eng'] = int(m.group(1), 16)
                ch[h]['subdev'] = m.group(3)
            m = re.search(r'tpcConfigID (\d+)', l)
            if m and kind == 'ch':
                ch[h]['tpc'] = int(m.group(1))
        m = re.search(r'CONTROL client \S+ obj (0x[0-9a-f]+)\((\S+)\) cmd (0x[0-9a-f]+) (\S+) psz \d+.* -> status (0x[0-9a-f]+)', l)
        if m:
            obj, ocls, cmd, cname, st = m.groups()
            if int(st, 16):
                fails.append(l.strip())
            if obj in ch and 'TOKEN' in cname and 'GET' in cname:
                for k in range(i + 1, min(i + 12, len(lines))):
                    mm = re.search(r'WORK_SUBMIT_TOKEN (0x[0-9a-f]+) \(chid (\d+), runlist (\d+)\)', lines[k])
                    if mm:
                        ch[obj]['token'] = 'chid %s runlist %s' % (mm.group(2), mm.group(3))
                        break
            elif ocls.startswith('NV9067') or obj in cs or (obj in ch and cname.startswith('NVA06F') is False and 'TOKEN' not in cname):
                order.append(('ctl', '%s on %s (%s) -> %s' % (cname, obj, ocls, st)))
            elif obj in tsgs:
                order.append(('ctl', '%s on TSG %s -> %s' % (cname, obj, st)))

    print('== %s' % path)
    for kind, v in order:
        if kind == 'mark':
            print('-- %s' % v)
        elif kind == 'tsg':
            t = tsgs[v]
            print('TSG %s  engineType %s  hVASpace %s' % (v, eng(t.get('eng', 0)), t.get('vas')))
        elif kind == 'ch':
            c = ch[v]
            where = 'in TSG %s' % c['parent'] if c['parent'] in tsgs else 'no TSG (parent %s %s)' % (c['parent'], c['parentcls'])
            csd = ''
            if c.get('cs') and c['cs'] != '0x0':
                s = cs.get(c['cs'], {})
                csd = 'ctxshare %s %s IN %s -> VEID %s' % (c['cs'], s.get('flags'), s.get('in'), (s.get('out', 0) & 0x7fffffff) if 'out' in s else '?')
            else:
                csd = 'hVASpace %s' % c.get('vas')
            print('  channel %s 0x%x %s  engine %s  flags %s  %s  %s  tpcConfigID %s' % (
                v, c['cls'], where, eng(c.get('eng', 0)), chflags(c.get('flags', 0)), csd, c.get('token', ''), c.get('tpc')))
            for o in objs.get(v, []):
                print('      object %s' % o)
        elif kind == 'ctl':
            print('  control %s' % v)
    # objects allocated after channel listing (lazy) are already attached above
    if fails:
        print('failures:')
        for f in fails:
            print('  ' + f)


if __name__ == '__main__':
    main(sys.argv[1])

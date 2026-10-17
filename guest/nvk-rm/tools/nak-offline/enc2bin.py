#!/usr/bin/env python3
# enc2bin.py LOG PREFIX: write each "Encoded shader:" block that NAK_DEBUG=print emits as PREFIX.<stage>.bin
import sys, struct
stage = None; out = {}; cur = None
for line in open(sys.argv[1], errors='replace'):
    if line.startswith('Stage: '): stage = line.split()[1]; cur = None; continue
    if line.startswith('Encoded shader:'): cur = out.setdefault(stage, []); cur.clear(); continue
    if cur is not None:
        if line.startswith('  '): cur.extend(int(w, 16) for w in line.split())
        else: cur = None
for st, words in out.items():
    open(f'{sys.argv[2]}.{st}.bin', 'wb').write(struct.pack(f'<{len(words)}I', *words))
    print(sys.argv[2], st, len(words) // 4, 'instructions')

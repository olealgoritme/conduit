#!/usr/bin/env python3
"""Errors the worktree's stub type check has that a base commit's does not.

Usage: stubcheck.sh base <commit>; stubcheck.sh head; stubdiff.py
Reads $S/stub_base.txt and $S/stub_head.txt (S defaults to $TMPDIR/kmd-dev-scratch, as env.sh).
"""
import collections
import os
import re

S = os.environ.get("S") or os.path.join(os.environ.get("TMPDIR", "/tmp"), "kmd-dev-scratch")


def load(name):
    count = collections.Counter()
    lines = {}
    for line in open(os.path.join(S, "stub_%s.txt" % name)):
        m = re.match(r"^(kmd_[a-z_]+/src/[^:]+):(\d+):\d+: (error.*)$", line.strip())
        if m:
            key = (m.group(1), m.group(3))
            count[key] += 1
            lines.setdefault(key, []).append(m.group(2))
    return count, lines


base, _ = load("base")
head, head_lines = load("head")
for key in head:
    if head[key] > base.get(key, 0):
        print(key[0], head_lines[key][:5], key[1][:220])
print("---- gone from base:")
for key in base:
    if base[key] > head.get(key, 0):
        print(key[0], key[1][:200])

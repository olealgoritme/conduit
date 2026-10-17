#!/usr/bin/env python3
"""sassdiff.py: compare two disassemblies (nvdisasm text) of the same shader,
or two sets of them: instruction counts by class, by opcode, and by the
patterns that made up NAK's gap to the reference compiler.

    sassdiff.py [--opcodes] REF.sass NAK.sass [REF2.sass NAK2.sass ...]

Trailing padding (NOPs and the final self-branch) is not counted.  With
several pairs, the totals over all of them are printed.
"""
import collections
import re
import sys

INS = re.compile(r"/\*[0-9a-f]{4,}\*/\s+(.*?)\s*;")


def load(path):
    out = []
    for line in open(path, errors="replace"):
        m = INS.search(line)
        if m:
            out.append(m.group(1))
    while out and (out[-1].startswith("NOP") or re.match(r"BRA (0x)?[0-9a-f]+$", out[-1])):
        out.pop()
    return out


def opcode(ins):
    t = ins.split()
    return t[1] if t[0].startswith("@") else t[0]


def base(op):
    return op.split(".")[0]


def pattern(ins):
    pred = ins.startswith("@")
    op = opcode(ins)
    b = base(op)
    args = ins.split(",")
    a1 = args[1].strip() if len(args) > 1 else ""
    if b == "BMOV" and "CLEAR" in op:
        return "BMOV.CLEAR before BSSY"
    if b in ("BSSY", "BSYNC", "BRA", "NOP", "BREAK"):
        return "BSSY/BSYNC/BRA/NOP"
    if op in ("IADD3.X", "IADD.64"):
        return "64-bit address add (high half)"
    if b == "LDC":
        return "constant buffer value to GPR (LDC)"
    if b == "MOV" and not pred and a1.startswith("UR"):
        return "uniform value copied to GPR"
    if b in ("IMNMX", "VIMNMX") and "0x3fff" in ins:
        return "descriptor index clamp in GPR"
    if (b == "MOV" and not pred and re.match(r"^-?(0x|[0-9])", a1)) or (b == "HFMA2" and "-RZ, RZ" in ins):
        return "immediate copied to a register"
    if b == "MOV" and not pred:
        return "register copy (phi, parallel copy)"
    if b == "PLOP3":
        return "PLOP3"
    if pred:
        return "predicated instruction"
    if b in ("R2UR", "LDCU", "ULDC") or b.startswith("U"):
        return "uniform datapath"
    if b in ("LDL", "STL"):
        return "local memory spill/fill"
    return None


def table(title, ref, nak, keys=None):
    nr, nn = sum(ref.values()), sum(nak.values())
    gap = nn - nr
    print(f"== {title}: reference {nr}, NAK {nn}, gap {gap:+} ({100 * gap / max(nr, 1):+.1f}%)")
    keys = keys or sorted(set(ref) | set(nak), key=lambda k: -(nak[k] - ref[k]))
    for k in keys:
        d = nak[k] - ref[k]
        share = f"{100 * d / gap:+6.1f}% of gap" if gap else ""
        print(f"  {k:40} {ref[k]:7} {nak[k]:7} {d:+7}  {share}")


def main():
    args = sys.argv[1:]
    opcodes = "--opcodes" in args
    files = [a for a in args if a != "--opcodes"]
    if len(files) < 2 or len(files) % 2:
        sys.exit(__doc__)
    pr, pn = collections.Counter(), collections.Counter()
    orf, onk = collections.Counter(), collections.Counter()
    for r, n in zip(files[0::2], files[1::2]):
        R, N = load(r), load(n)
        print(f"{n}: reference {len(R)}, NAK {len(N)} ({100 * (len(N) - len(R)) / max(len(R), 1):+.1f}%)")
        for i in R:
            pr[pattern(i) or "other"] += 1
            orf[opcode(i)] += 1
        for i in N:
            pn[pattern(i) or "other"] += 1
            onk[opcode(i)] += 1
    table("patterns", pr, pn)
    if opcodes:
        keys = sorted(set(orf) | set(onk), key=lambda k: -abs(onk[k] - orf[k]))[:60]
        table("opcodes (largest differences)", orf, onk, keys)


if __name__ == "__main__":
    main()

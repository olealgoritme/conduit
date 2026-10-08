#!/usr/bin/env python3
"""Function-level CPU profile and wait breakdown of one process from an xperf dump.

WPA/xperf only resolve symbols from PDBs. NVK (vulkan_nouveau.dll) and the
MinGW-built DLLs carry DWARF instead, so their samples show as "Unknown". This
takes the raw `xperf -a dumper` text, maps every SampledProfile instruction
pointer of the chosen process to (module, RVA) through the Image/DCStart and
Image/Load events, and names RVAs with addr2line against unstripped copies of
the DLLs given with --dll.

It also sums the process's context switches (CSwitch): per thread, time on the
CPU and time waited by wait reason, which says whether a thread is working or
waiting (a fence, the GPU, a lock).

Capture in the guest (elevated; ~15 s of the scene):
    xperf -on PROC_THREAD+LOADER+PROFILE+CSWITCH+DISPATCHER -stackwalk Profile+CSwitch+ReadyThread
          -SetProfInt 1221 -BufferSize 1024 -MinBuffers 256 -MaxBuffers 1024
    ... run ...
    xperf -d bm.etl
    xperf -i bm.etl -o bm-dump.txt -a dumper
    xperf -i bm.etl -o bm-prof.txt -symbols -a profile -detail      (PDB modules)

On the host:
    etw-symbolize.py bm-dump.txt --process BasemarkGPU_dx12.exe \\
        --dll vulkan_nouveau.dll=/path/vulkan_nouveau64-unstripped.dll \\
        --dll helios_umd12.dll=/path/helios_umd12.dll --top 60

The dumper's columns are read from its own header lines ("SampledProfile,
TimeStamp, ..."), so column order does not matter; the names used are listed
in COLS below. Unverified against every xperf version: if a name is missing the
script says which and stops.
"""

import argparse
import collections
import re
import struct
import subprocess
import sys

COLS = {
    "SampledProfile": ["Process Name ( PID)", "ThreadID", "PrgrmCtr"],
    "Image": ["Process Name ( PID)", "BaseAddress", "EndAddress", "FileName"],
    "CSwitch": ["New Process Name ( PID)", "New TID", "Old Process Name ( PID)", "Old TID",
                "WaitTime", "Wait Reason"],
}


def split_row(line):
    # Quoted file names can contain commas.
    out, cur, q = [], [], False
    for ch in line:
        if ch == '"':
            q = not q
        elif ch == "," and not q:
            out.append("".join(cur).strip())
            cur = []
            continue
        cur.append(ch)
    out.append("".join(cur).strip())
    return out


def pe_image_base(path):
    with open(path, "rb") as f:
        data = f.read(4096)
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    magic = struct.unpack_from("<H", data, pe + 24)[0]
    if magic == 0x20B:  # PE32+
        return struct.unpack_from("<Q", data, pe + 24 + 24)[0]
    return struct.unpack_from("<I", data, pe + 24 + 28)[0]


def symbolize(dll, rvas):
    base = pe_image_base(dll)
    names = {}
    rvas = sorted(rvas)
    for i in range(0, len(rvas), 2000):
        chunk = rvas[i:i + 2000]
        addrs = ["0x%x" % (base + r) for r in chunk]
        out = subprocess.run(["llvm-addr2line", "-f", "-C", "-e", dll] + addrs,
                             capture_output=True, text=True).stdout.splitlines()
        for j, r in enumerate(chunk):
            fn = out[2 * j] if 2 * j < len(out) else "??"
            names[r] = fn
    return names


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dump")
    ap.add_argument("--process", required=True, help="image name, e.g. BasemarkGPU_dx12.exe")
    ap.add_argument("--dll", action="append", default=[],
                    help="module.dll=/path/to/unstripped.dll (repeatable)")
    ap.add_argument("--top", type=int, default=50)
    a = ap.parse_args()
    dlls = dict(x.split("=", 1) for x in a.dll)
    dlls = {k.lower(): v for k, v in dlls.items()}

    headers = {}
    modules = []          # (base, end, name)
    samples = collections.Counter()   # pc -> count
    thread_samples = collections.Counter()
    run = collections.Counter()       # tid -> on-CPU switches in
    waits = collections.defaultdict(collections.Counter)  # tid -> reason -> waited time
    proc_re = re.compile(re.escape(a.process) + r"\s*\(", re.I)

    with open(a.dump, errors="replace") as f:
        for line in f:
            if not line.strip():
                continue
            row = split_row(line.rstrip("\n"))
            kind = row[0]
            if kind.startswith("SampledProfile") and "TimeStamp" in row[1:3]:
                headers["SampledProfile"] = row
                continue
            if kind.startswith("Image/") and "TimeStamp" in row[1:3]:
                headers["Image"] = row
                continue
            if kind == "CSwitch" and "TimeStamp" in row[1:3]:
                headers["CSwitch"] = row
                continue
            if kind in ("Image/DCStart", "Image/Load") and "Image" in headers:
                h = headers["Image"]
                col = {n: i for i, n in enumerate(h)}
                if not proc_re.search(row[col["Process Name ( PID)"]]):
                    continue
                base = int(row[col["BaseAddress"]], 16)
                end = int(row[col["EndAddress"]], 16)
                name = row[col["FileName"]].strip('"').split("\\")[-1].lower()
                modules.append((base, end, name))
            elif kind == "SampledProfile" and "SampledProfile" in headers:
                h = headers["SampledProfile"]
                col = {n: i for i, n in enumerate(h)}
                if not proc_re.search(row[col["Process Name ( PID)"]]):
                    continue
                pc = int(row[col["PrgrmCtr"]], 16)
                samples[pc] += 1
                thread_samples[row[col["ThreadID"]]] += 1
            elif kind == "CSwitch" and "CSwitch" in headers:
                h = headers["CSwitch"]
                col = {n: i for i, n in enumerate(h)}
                if proc_re.search(row[col["New Process Name ( PID)"]]):
                    tid = row[col["New TID"]]
                    run[tid] += 1
                    try:
                        waits[tid][row[col["Wait Reason"]]] += int(row[col["WaitTime"]])
                    except ValueError:
                        pass

    for kind, need in COLS.items():
        h = headers.get(kind)
        if h is None:
            print("note: no %s events in the dump" % kind, file=sys.stderr)
            continue
        missing = [n for n in need if n not in h]
        if missing:
            sys.exit("%s header lacks %s; header is: %s" % (kind, missing, h))

    total = sum(samples.values())
    if not total:
        sys.exit("no samples for %s" % a.process)
    modules.sort()
    per_mod_rva = collections.defaultdict(collections.Counter)
    per_mod = collections.Counter()
    for pc, n in samples.items():
        name = "<kernel/unknown>"
        rva = 0
        for base, end, mod in modules:
            if base <= pc < end:
                name, rva = mod, pc - base
                break
        per_mod[name] += n
        per_mod_rva[name][rva] += n

    print("%d samples of %s" % (total, a.process))
    print("\n== modules")
    for mod, n in per_mod.most_common(20):
        print("%6.2f%%  %s" % (100.0 * n / total, mod))

    funcs = collections.Counter()
    for mod, rvas in per_mod_rva.items():
        dll = dlls.get(mod)
        if dll:
            names = symbolize(dll, list(rvas))
            for rva, n in rvas.items():
                funcs["%s!%s" % (mod, names.get(rva, "??"))] += n
        else:
            funcs["%s!?" % mod] += sum(rvas.values())
    print("\n== functions (modules given with --dll are named; others are one line each)")
    for fn, n in funcs.most_common(a.top):
        print("%6.2f%%  %s" % (100.0 * n / total, fn))

    if run:
        print("\n== threads: samples on the CPU, switches in, waited time by reason (dumper units)")
        for tid, n in thread_samples.most_common(16):
            w = ", ".join("%s %d" % (r, t) for r, t in waits[tid].most_common(4))
            print("tid %-8s %6d samples  %6d switches  waits: %s" % (tid, n, run[tid], w))


if __name__ == "__main__":
    main()

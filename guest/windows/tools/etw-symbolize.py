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

With CSwitch stacks in the capture (-stackwalk ...+CSwitch), --waits splits a
thread's time off the CPU by WHERE it went off: each interval from a switch-out
to the next switch-in of the thread is charged to the stack recorded at that
switch-out, named by its first frames outside the kernel and the Win32 wait
wrappers (the wait API itself is kept as the first name). That is the exact
answer to "what is the presenting thread waiting for": an app fence event
(KernelBase!WaitForSingleObjectEx <- the app), DXGI's frame-latency wait, a
lock in vkd3d or NVK, a Sleep. --thread picks the thread (the presenting one:
the thread with Present on its stacks, or the busiest); by default the three
process threads with the most time off the CPU are shown.

Capture in the guest (elevated; ~15 s of the scene):
    xperf -on PROC_THREAD+LOADER+PROFILE+CSWITCH+DISPATCHER -stackwalk Profile+CSwitch+ReadyThread
          (CSwitch stacks are what --waits needs)
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

# Frames skipped when naming a wait stack: the kernel, and the user-mode wait wrappers
# (the last one skipped is kept as the wait API).
WAIT_SKIP = ("ntoskrnl.exe", "hal.dll", "ntdll.dll", "kernelbase.dll", "kernel32.dll",
             "win32u.dll", "wow64.dll", "wow64cpu.dll", "wow64win.dll", "dxgkrnl.sys",
             "win32kbase.sys", "win32kfull.sys", "win32k.sys", "?")


def frame_module(name):
    return name.split("!", 1)[0].lower() if "!" in name else name.lower()


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
    ap.add_argument("--waits", action="store_true",
                    help="split time off the CPU by switch-out stack (needs CSwitch stacks)")
    ap.add_argument("--thread", action="append", default=[], help="thread id for --waits")
    ap.add_argument("--frames", type=int, default=0,
                    help="frames in the capture window, to print per-frame figures")
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
    # --waits: per thread, switch-in / switch-out times, intervals, stacks.
    last_in = {}                      # tid -> ts switched in
    last_out = {}                     # tid -> ts switched out
    on_cpu = collections.Counter()    # tid -> us on the CPU
    off_iv = []                       # (tid, out_ts, us off)
    stacks = {}                       # (ts, tid) -> [frame names, innermost first]
    stack_by_ts = {}                  # ts -> (ts, tid) of the first stack at it
    cur_stack = None
    readied = {}                      # tid -> (ts, who readied it) most recent
    proc_tids = set()
    first_ts = last_ts = None

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
            if kind == "Stack" and "TimeStamp" in row[1:3]:
                headers["Stack"] = row
                continue
            if kind == "ReadyThread" and "TimeStamp" in row[1:3]:
                headers["ReadyThread"] = row
                continue
            if kind == "ReadyThread" and a.waits and "ReadyThread" in headers:
                # Who made a thread runnable: the current thread (or a DPC on its
                # CPU) readies "Rdy TID".
                h = headers["ReadyThread"]
                col = {n: i for i, n in enumerate(h)}
                rdy = next((i for n, i in col.items() if n.startswith("Rdy") and "TID" in n), None)
                who_p = col.get("Process Name ( PID)")
                who_t = col.get("ThreadID")
                dpc = col.get("InDPC")
                if rdy is None or who_p is None or who_t is None:
                    continue
                try:
                    ts = int(row[col["TimeStamp"]])
                except (ValueError, KeyError, IndexError):
                    continue
                who = "%s tid %s" % (row[who_p].split("(")[0].strip(), row[who_t])
                if dpc is not None and dpc < len(row) and row[dpc].strip() not in ("0", "", "False"):
                    who = "DPC on " + row[who_p].split("(")[0].strip()
                readied[row[rdy]] = (ts, who)
                continue
            if kind == "Stack" and a.waits:
                h = headers.get("Stack")
                col = {n: i for i, n in enumerate(h)} if h else {}
                try:
                    ts = int(row[col.get("TimeStamp", 1)])
                    tid = row[col.get("ThreadID", 2)]
                except (ValueError, IndexError):
                    continue
                fn_i = next((i for n, i in col.items() if "Image!Function" in n), 5)
                addr_i = col.get("Address", 4)
                name = row[fn_i] if fn_i < len(row) else "?"
                try:
                    addr = int(row[addr_i], 16)
                except (ValueError, IndexError):
                    addr = 0
                key = (ts, tid)
                if key != cur_stack:
                    cur_stack = key
                    stacks[key] = []
                    stack_by_ts.setdefault(ts, key)
                stacks[key].append((name, addr))
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
                try:
                    ts = int(row[col.get("TimeStamp", 1)])
                except ValueError:
                    ts = None
                if ts is not None:
                    first_ts = ts if first_ts is None else first_ts
                    last_ts = ts
                if ts is not None and proc_re.search(row[col["Old Process Name ( PID)"]]):
                    otid = row[col["Old TID"]]
                    proc_tids.add(otid)
                    if otid in last_in:
                        on_cpu[otid] += ts - last_in.pop(otid)
                    last_out[otid] = ts
                if ts is not None and proc_re.search(row[col["New Process Name ( PID)"]]):
                    ntid = row[col["New TID"]]
                    proc_tids.add(ntid)
                    if ntid in last_out:
                        out = last_out.pop(ntid)
                        r = readied.get(ntid)
                        who = r[1] if r and out <= r[0] <= ts else "?"
                        off_iv.append((ntid, out, ts - out, who))
                    last_in[ntid] = ts
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

    modules.sort()
    if a.waits:
        report_waits(a, dlls, modules, on_cpu, off_iv, stacks, stack_by_ts, first_ts, last_ts)
    total = sum(samples.values())
    if not total:
        sys.exit("no samples for %s" % a.process)
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


def report_waits(a, dlls, modules, on_cpu, off_iv, stacks, stack_by_ts, first_ts, last_ts):
    if not off_iv:
        print("--waits: no switch-ins of %s threads after a switch-out" % a.process)
        return
    off_total = collections.Counter()
    for tid, _, us, _ in off_iv:
        off_total[tid] += us
    tids = a.thread or [t for t, _ in off_total.most_common(3)]
    # Name user-mode frames of modules given with --dll; others by the dumper's name.
    want = collections.defaultdict(set)

    def locate(addr):
        for base, end, mod in modules:
            if base <= addr < end:
                return mod, addr - base
        return None, 0

    for tid, out, _, _ in off_iv:
        if tid not in tids:
            continue
        for name, addr in stacks.get((out, tid)) or stacks.get(stack_by_ts.get(out), []) or []:
            mod, rva = locate(addr)
            if mod in dlls:
                want[mod].add(rva)
    names = {mod: symbolize(dlls[mod], list(r)) for mod, r in want.items()}

    def frame_name(name, addr):
        mod, rva = locate(addr)
        if mod in names:
            return "%s!%s" % (mod, names[mod].get(rva, "??"))
        if "!" in name and not name.endswith("!Unknown") and not name.endswith("!?"):
            return name
        if mod:
            # Unsymbolized: the call site's RVA keeps distinct waits apart.
            return "%s+0x%x" % (mod, rva)
        return (frame_module(name) or "?") + "!?"

    def signature(frames):
        if not frames:
            return "<no stack>"
        named = [frame_name(n, ad) for n, ad in frames]
        wait_api = None
        kept = []
        for f in named:
            if not kept and frame_module(f) in WAIT_SKIP:
                wait_api = f
                continue
            kept.append(f)
            if len(kept) == 3:
                break
        sig = " <- ".join(kept) if kept else "<kernel only>"
        return ("%s <- %s" % (wait_api, sig)) if wait_api else sig

    window = (last_ts - first_ts) if (first_ts is not None and last_ts is not None) else 0
    per = a.frames or 0
    for tid in tids:
        by = collections.Counter()
        cnt = collections.Counter()
        woke = collections.Counter()
        by_who = collections.defaultdict(collections.Counter)
        for t, out, us, who in off_iv:
            if t != tid:
                continue
            st = stacks.get((out, t))
            if st is None:
                key = stack_by_ts.get(out)
                st = stacks.get(key) if key else None
            sig = signature(st)
            by[sig] += us
            cnt[sig] += 1
            woke[who] += us
            by_who[sig][who] += us
        off = sum(by.values())
        print("\n== thread %s: on the CPU %.1f ms, off %.1f ms, window %.1f ms%s"
              % (tid, on_cpu[tid] / 1000.0, off / 1000.0, window / 1000.0,
                 (" (per frame: on %.0f us, off %.0f us)" % (on_cpu[tid] / per, off / per)) if per else ""))
        print("   off-CPU by switch-out stack (wait API <- first frames outside the kernel):")
        for sig, us in by.most_common(a.top // 2 or 20):
            print("%6.2f%% %9.1f ms %7d waits%s  %s" % (
                100.0 * us / max(off, 1), us / 1000.0, cnt[sig],
                ("  %6.0f us/frame" % (us / per)) if per else "", sig))
            if any(w != "?" for w in by_who[sig]):
                print("          woken by: " + ", ".join(
                    "%s %.0f%%" % (w, 100.0 * v / max(us, 1)) for w, v in by_who[sig].most_common(4)))
        if any(w != "?" for w in woke):
            print("   off-CPU by who readied the thread (ReadyThread; 'DPC on X' = a DPC, e.g. the "
                  "GPU interrupt path; a tid of this process = its own thread):")
            for w, us in woke.most_common(10):
                print("%6.2f%% %9.1f ms  %s" % (100.0 * us / max(off, 1), us / 1000.0, w))


if __name__ == "__main__":
    main()

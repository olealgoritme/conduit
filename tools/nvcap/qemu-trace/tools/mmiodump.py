#!/usr/bin/env python3
"""Decode nvtrace files written by the nvtrace QEMU (x-nvtrace=on).

Usage:
  mmiodump.py summary  TRACE...            counts per record type, BAR, block,
                                           register, IRQ vector, RPC function
  mmiodump.py timeline TRACE... [filters]  one line per record, in order
  mmiodump.py gsp      TRACE... [--decode] [--hex]
                                           GSP RPCs (command + status queue);
                                           --decode: every parameter field
  mmiodump.py rmlog    TRACE... [--out] [--tree] [--json FILE]
                                           the RM API call sequence (alloc /
                                           control / free / memory) as a
                                           user-space RM client would issue it
  mmiodump.py fsp      TRACE...            FSP packets / messages (COT etc.)
  mmiodump.py boot     TRACE... [--json FILE]
                                           what the device answered until GSP
                                           INIT_DONE: the emulator's fake table
  mmiodump.py disp     TRACE...            display channel pushbuffers, decoded
  mmiodump.py bars     TRACE... [--extract DIR]   BAR (VRAM aperture) pages
  mmiodump.py gpu      TRACE... [--extract DIR]   GPU memory read from the
                                           host side (VRAM, page tables, sweep)
  mmiodump.py pushbuf  TRACE... [--channel H]  GPFIFO entries + pushbuffer
                                           methods by class
  mmiodump.py fw       TRACE... [--extract DIR]   firmware images / GSP-RM
                                           sysmem areas dumped from the guest
  mmiodump.py gsplog   TRACE... [--log-elf F]     GSP-RM's own log records
  mmiodump.py pramin   TRACE...            VRAM accesses through the BAR0
                                           PRAMIN window
  mmiodump.py snaps    TRACE... [--extract DIR]   DMA snapshots
  mmiodump.py regs     TRACE... [--top N]  per-register read/write counts
  mmiodump.py json     TRACE...            records as JSON lines

TRACE may be raw, .zst (needs the zstd binary), .gz or .lz4 (needs lz4).
Rotated sets (name.000.ext, name.001.ext ...) can be passed together; files
are ordered by the index in their header.

Python 3 standard library only. Register/RPC/class/control names come from
nvtables.json.gz next to this script (see gen_tables.py).
"""
import argparse
import collections
import gzip
import json
import os
import re
import struct
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# ---------------------------------------------------------------- format --

FILE_MAGIC = b"NVTRACE1"
FILE_HDR = struct.Struct("<8sIIQQII24s")          # 64 bytes
REC_HDR = struct.Struct("<HBBIQQhHI")             # 32 bytes

T_MMIO, T_CFG, T_IRQ, T_VEC, T_RESET, T_BAR, T_GSP_MSG, T_DMA_SNAP, \
    T_NOTE, T_GSP_QSTATE, T_LOST, T_FSP_PKT, T_GSP_QINFO, T_DEV, \
    T_BAR_PAGE, T_GPU_PAGE, T_GPU_HASHES, T_PUSHBUF = range(1, 19)

TYPE_NAMES = {
    T_MMIO: "MMIO", T_CFG: "CFG", T_IRQ: "IRQ", T_VEC: "VEC", T_RESET: "RESET",
    T_BAR: "BAR", T_GSP_MSG: "GSP_MSG", T_DMA_SNAP: "DMA_SNAP", T_NOTE: "NOTE",
    T_GSP_QSTATE: "GSP_QSTATE", T_LOST: "LOST", T_FSP_PKT: "FSP_PKT",
    T_GSP_QINFO: "GSP_QINFO", T_DEV: "DEV", T_BAR_PAGE: "BAR_PAGE",
    T_GPU_PAGE: "GPU_PAGE", T_GPU_HASHES: "GPU_HASHES", T_PUSHBUF: "PUSHBUF",
}

BODY = {
    T_MMIO: (struct.Struct("<BBBBIQQ"),
             ("bar", "size", "write", "origin", "_r", "off", "val")),
    T_CFG: (struct.Struct("<IBBHII"),
            ("off", "len", "write", "_r", "val", "_r2")),
    T_IRQ: (struct.Struct("<BBHIQ"), ("kind", "masked", "vector", "data", "addr")),
    T_VEC: (struct.Struct("<BBHIQ"), ("event", "_r", "vector", "data", "addr")),
    T_RESET: (struct.Struct("<II"), ("kind", "_r")),
    T_BAR: (struct.Struct("<B7xQQQ"), ("bar", "old", "new", "size")),
    T_GSP_MSG: (struct.Struct("<BBHIIIIIQ"),
                ("queue", "gflags", "nelem", "slot", "wp", "rp", "len",
                 "rpc_off", "gpa")),
    T_DMA_SNAP: (struct.Struct("<QII24s"), ("gpa", "len", "sflags", "label")),
    T_GSP_QSTATE: (struct.Struct("<IIII"),
                   ("cmd_wp", "cmd_rp", "stat_wp", "stat_rp")),
    T_LOST: (struct.Struct("<QQ"), ("records", "bytes")),
    T_FSP_PKT: (struct.Struct("<B3xI"), ("dir", "len")),
    T_GSP_QINFO: (struct.Struct("<QIIQQIIII"),
                  ("shared_pa", "npte", "elem_hdr", "cmd_off", "stat_off",
                   "cmd_size", "stat_size", "source", "_r")),
    T_DEV: (struct.Struct("<HHIIIQ"),
            ("vendor", "device", "class_code", "trap_mask", "dflags", "_r")),
    T_BAR_PAGE: (struct.Struct("<BBHIQQ"),
                 ("bar", "reason", "_r", "len", "off", "hash")),
    T_GPU_PAGE: (struct.Struct("<BBHIQQ24s"),
                 ("aper", "reason", "_r", "len", "pa", "hash", "tag")),
    T_GPU_HASHES: (struct.Struct("<QII"), ("first_pa", "count", "pass")),
    T_PUSHBUF: (struct.Struct("<IIIIIIQQII"),
                ("hclient", "hobject", "gp_index", "gp_put", "gp_get",
                 "pflags", "entry", "va", "len", "got")),
}
SNAP_NODATA, SNAP_BLOB, SNAP_CHANGED, SNAP_IMAGE = 1, 2, 4, 8
APERTURES = {0: "vid", 1: "peer", 2: "sys", 3: "sysnc"}
GPU_REASONS = {0: "event", 1: "period", 2: "sweep", 3: "pagetable"}
PB_F = {1: "no-entry", 2: "truncated", 4: "partial"}

ORIGINS = {0: "", 1: "mirror", 2: "quirk-int", 3: "window", 4: "ioeventfd",
           5: "msix-table", 6: "msix-pba", 7: "rom"}
IRQ_KINDS = {0: "INTx", 1: "MSI", 2: "MSI-X"}
VEC_EVENTS = {0: "use", 1: "release", 2: "msi-enable", 3: "msi-disable",
              4: "msix-enable", 5: "msix-disable", 6: "intx-enable"}
RESET_KINDS = {0: "reset-begin", 1: "reset-end", 2: "FLR(devctl)", 3: "SBR"}
QINFO_SRC = {1: "FSP COT", 2: "falcon mailbox", 3: "RAM scan"}
GSP_F_TRUNC, GSP_F_NOSIG, GSP_F_SEQGAP = 1, 2, 4
VRPC = 0x43505256
NVDM_TYPES = {0x4: "RESET", 0x11: "HULK", 0x12: "FIRMWARE_UPDATE", 0x13: "PRC",
              0x14: "COT", 0x15: "FSP_RESPONSE", 0x16: "CAPS_QUERY",
              0x17: "INFOROM", 0x18: "SMBPBI", 0x1a: "ROMREAD",
              0x1c: "UEFI_RM", 0x1d: "UEFI_XTL_DEBUG_INTR", 0x1f: "TNVL",
              0x20: "CLOCK_BOOST", 0x21: "FSP_GSP_COMM", 0x25: "RM_RPC"}


class Record:
    __slots__ = ("type", "dev", "flags", "seq", "ts", "vcpu", "tid", "f",
                 "data", "file")

    def name(self):
        return TYPE_NAMES.get(self.type, f"T{self.type}")


def _open(path):
    if path.endswith(".zst"):
        p = subprocess.Popen(["zstd", "-dcq", path], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL)
        return p.stdout, p
    if path.endswith(".lz4"):
        p = subprocess.Popen(["lz4", "-dcq", path], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL)
        return p.stdout, p
    if path.endswith(".gz"):
        return gzip.open(path, "rb"), None
    return open(path, "rb"), None


def _read_exact(f, n):
    buf = b""
    while len(buf) < n:
        chunk = f.read(n - len(buf))
        if not chunk:
            break
        buf += chunk
    return buf


def file_header(path):
    f, p = _open(path)
    try:
        h = _read_exact(f, FILE_HDR.size)
    finally:
        f.close()
        if p:
            p.kill()
            p.wait()
    if len(h) < FILE_HDR.size or h[:8] != FILE_MAGIC:
        raise ValueError(f"{path}: not an nvtrace file")
    m, ver, hsz, rt, mono, idx, pid, q = FILE_HDR.unpack(h)
    return {"version": ver, "hdr_size": hsz, "realtime_ns": rt,
            "monotonic_ns": mono, "file_index": idx, "pid": pid,
            "qemu": q.rstrip(b"\0").decode()}


def iter_records(paths, warn=True):
    """Yield Records from one or more trace files (rotated sets in order)."""
    order = []
    for p in paths:
        h = file_header(p)
        order.append((h["pid"], h["file_index"], p, h))
    order.sort(key=lambda x: (x[0], x[1]))
    for _, _, path, hdr in order:
        f, proc = _open(path)
        try:
            _read_exact(f, hdr["hdr_size"])
            while True:
                hb = _read_exact(f, REC_HDR.size)
                if not hb:
                    break
                if len(hb) < REC_HDR.size:
                    if warn:
                        print(f"warning: {path}: truncated record header",
                              file=sys.stderr)
                    break
                typ, dev, flags, ln, seq, ts, vcpu, _, tid = REC_HDR.unpack(hb)
                if ln < REC_HDR.size or ln > (1 << 30):
                    if warn:
                        print(f"warning: {path}: bad record length {ln} at "
                              f"seq {seq}", file=sys.stderr)
                    break
                rest = _read_exact(f, ln - REC_HDR.size)
                if len(rest) < ln - REC_HDR.size:
                    if warn:
                        print(f"warning: {path}: truncated record seq {seq}",
                              file=sys.stderr)
                    break
                r = Record()
                r.type, r.dev, r.flags, r.seq, r.ts = typ, dev, flags, seq, ts
                r.vcpu, r.tid, r.file = vcpu, tid, path
                spec = BODY.get(typ)
                if spec:
                    st, names = spec
                    r.f = dict(zip(names, st.unpack_from(rest, 0)))
                    r.data = rest[st.size:]
                else:
                    r.f = {}
                    r.data = rest
                if typ == T_GSP_MSG:
                    r.data = r.data[:r.f["len"]]
                elif typ == T_DMA_SNAP:
                    r.f["label"] = r.f["label"].rstrip(b"\0").decode(
                        "ascii", "replace")
                    r.data = r.data[:r.f["len"]] if not r.f["sflags"] & 1 \
                        else b""
                elif typ in (T_FSP_PKT, T_BAR_PAGE, T_GPU_PAGE):
                    r.data = r.data[:r.f["len"]]
                    if typ == T_GPU_PAGE:
                        r.f["tag"] = r.f["tag"].rstrip(b"\0").decode(
                            "ascii", "replace")
                elif typ == T_GPU_HASHES:
                    n = r.f["count"]
                    r.data = struct.unpack_from(f"<{n}Q", r.data)
                elif typ == T_PUSHBUF:
                    r.data = r.data[:r.f["got"]]
                elif typ in (T_NOTE,):
                    r.data = r.data.rstrip(b"\0")
                elif typ == T_DEV:
                    r.data = r.data.rstrip(b"\0")
                yield r
        finally:
            f.close()
            if proc:
                proc.wait()

# ----------------------------------------------------------------- names --


class Names:
    def __init__(self, path=None):
        path = path or os.path.join(HERE, "nvtables.json.gz")
        try:
            with gzip.open(path, "rt", encoding="utf-8") as f:
                t = json.load(f)
        except OSError:
            t = {}
        self.regs = {int(k, 16): v[0] for k, v in t.get("regs", {}).items()}
        self.ranges = sorted(((lo, hi, n) for lo, hi, n in t.get("ranges", [])),
                             key=lambda x: x[1] - x[0])
        self.pages = {int(k, 16): v for k, v in t.get("pages", {}).items()}
        self.rpc = {int(k): v for k, v in t.get("rpc", {}).items()}
        self.classes = {int(k, 16): v for k, v in t.get("classes", {}).items()}
        self.ctrls = {int(k, 16): v for k, v in t.get("ctrls", {}).items()}
        self.structs = t.get("structs", {})
        self.rpc_params = {int(k): v for k, v in t.get("rpc_params", {}).items()}
        self.ctrl_params = {int(k, 16): v
                            for k, v in t.get("ctrl_params", {}).items()}
        self.alloc_params = {int(k, 16): v
                             for k, v in t.get("alloc_params", {}).items()}
        self.disp = {int(c, 16): {int(k, 16): n for k, n in m.items()}
                     for c, m in t.get("disp_methods", {}).items()}
        self.status = {int(k, 16): v for k, v in t.get("status", {}).items()}
        self.methods = {int(c, 16): {int(k, 16): n for k, n in m.items()}
                        for c, m in t.get("methods", {}).items()}
        self.source = t.get("source", "no tables")

    def block(self, off):
        for lo, hi, n in self.ranges:          # narrowest first
            if lo <= off <= hi:
                return n
        p = self.pages.get(off >> 12)
        return p if p else f"page_{off >> 12:04x}"

    def reg(self, off):
        n = self.regs.get(off)
        if n:
            return n
        base = off & ~3
        if base != off and base in self.regs:
            return f"{self.regs[base]}+{off - base}"
        return f"{self.block(off)}+0x{off:06x}"

    def rpc_name(self, fn):
        return self.rpc.get(fn, f"rpc_{fn}")

    def class_name(self, c):
        return self.classes.get(c, f"class_{c:04x}")

    def status_name(self, st):
        return self.status.get(st, f"0x{st:x}")

    def struct(self, name):
        for _ in range(8):
            st = self.structs.get(name)
            if not st or "alias" not in st:
                return st
            name = st["alias"]
        return None

    def ctrl_name(self, cmd):
        n = self.ctrls.get(cmd)
        if n:
            return n
        iface = cmd >> 16
        return f"{self.class_name(iface)}:ctrl_{cmd:08x}"

# ------------------------------------------------------------------- GSP --

RPC_HDR = struct.Struct("<IIIIIIII")
ALLOC = struct.Struct("<IIIIIII4x")
CONTROL = struct.Struct("<IIIIIIIIQ")
FREE = struct.Struct("<IIII")


def parse_rpc(r):
    """Return a dict for a GSP_MSG record, or None if there is no RPC."""
    off = r.f["rpc_off"]
    d = r.data
    if off == 0xffffffff or off + RPC_HDR.size > len(d):
        return None
    hv, sig, ln, fn, res, resp, seq, spare = RPC_HDR.unpack_from(d, off)
    if sig != VRPC:
        return None
    out = {"version": hv, "length": ln, "function": fn, "result": res,
           "result_private": resp, "rpc_seq": seq,
           "params": d[off + RPC_HDR.size: off + ln]}
    if off >= 16:
        mctp, nvdm, csum, qseq = struct.unpack_from("<IIII", d, off - 16)
        out.update(mctp=mctp, nvdm=nvdm, checksum=csum, queue_seq=qseq)
    return out


FN_ALLOC_MEMORY, FN_FREE, FN_CONTINUATION, FN_CONTROL, FN_ALLOC = \
    4, 10, 71, 76, 103
EV_INIT_DONE = 0x1001


def describe_rpc(names, rpc):
    fn = rpc["function"]
    name = names.rpc_name(fn)
    p = rpc["params"]
    extra = ""
    if name == "GSP_RM_ALLOC" and len(p) >= ALLOC.size:
        hc, hp, ho, cls, st, psz, fl = ALLOC.unpack_from(p, 0)
        extra = (f"hClient=0x{hc:08x} hParent=0x{hp:08x} hObject=0x{ho:08x} "
                 f"hClass=0x{cls:04x}({names.class_name(cls)}) "
                 f"status={names.status_name(st)} paramsSize={psz} "
                 f"flags=0x{fl:x}")
    elif name == "GSP_RM_CONTROL" and len(p) >= CONTROL.size:
        hc, ho, cmd, st, psz, rf, cf, acc, _ = CONTROL.unpack_from(p, 0)
        extra = (f"hClient=0x{hc:08x} hObject=0x{ho:08x} "
                 f"cmd=0x{cmd:08x}({names.ctrl_name(cmd)}) "
                 f"status={names.status_name(st)} "
                 f"paramsSize={psz} flags=0x{rf:x}/0x{cf:x}")
    elif name == "FREE" and len(p) >= FREE.size:
        hr, hp, ho, st = FREE.unpack_from(p, 0)
        extra = (f"hRoot=0x{hr:08x} hParent=0x{hp:08x} hObject=0x{ho:08x} "
                 f"status={names.status_name(st)}")
    elif fn in names.rpc_params:
        extra = " ".join(f"{k}={v}" for k, v in
                         decode_struct(names, names.rpc_params[fn], p,
                                       limit=8))
    if rpc.get("continuations"):
        extra += f" (+{rpc['continuations']} continuation records)"
    return name, extra

# ------------------------------------------------------- structure decode --


def _fmt_val(kind, size, v):
    if kind == "i":
        return str(v)
    if kind == "b":
        return "true" if v else "false"
    return f"0x{v:x}"


def decode_struct(names, sname, data, base=0, prefix="", compact=True,
                  limit=200, maxarr=16, depth=0, out=None):
    """Return [(path, value-string)] for structure 'sname' at data[base:].

    compact: leave out zero fields. Layouts come from the 610.57.04 headers
    (gen_tables.py); fields beyond the data are left out."""
    out = [] if out is None else out
    st = names.struct(sname)
    if st is None or depth > 6:
        return out
    for name, off, kind, esz, count, sub, boff, bits in st["f"]:
        if len(out) >= limit:
            break
        o = base + off
        if o >= len(data):
            break
        path = prefix + name
        if count == 0:                      # flexible array member
            count = max(0, (len(data) - o) // max(esz, 1))
            if kind != "c":
                count = min(count, maxarr)
        if kind == "S":
            n = min(count, maxarr)
            for i in range(n):
                p = f"{path}[{i}]." if count > 1 else path + "."
                before = len(out)
                decode_struct(names, sub, data, o + i * esz, p, compact,
                              limit, maxarr, depth + 1, out)
                if compact and len(out) == before:
                    continue
            continue
        if kind == "c":
            raw = data[o:o + count]
            txt = raw.split(b"\0", 1)[0].decode("latin-1")
            if txt or not compact:
                out.append((path, repr(txt)))
            continue
        if esz not in (1, 2, 4, 8):
            continue
        fmt = {1: "B", 2: "H", 4: "I", 8: "Q"}[esz]
        if kind == "i":
            fmt = fmt.lower()
        n = min(count, (len(data) - o) // esz)
        if n <= 0:
            continue
        vals = struct.unpack_from(f"<{n}{fmt}", data, o)
        if bits is not None:
            v = (vals[0] >> boff) & ((1 << bits) - 1)
            if v or not compact:
                out.append((path, _fmt_val(kind, esz, v)))
            continue
        if count == 1:
            if vals[0] or not compact:
                out.append((path, _fmt_val(kind, esz, vals[0])))
            continue
        if kind == "u" and esz == 1 and count > 16:
            nz = bytes(vals).rstrip(b"\0")
            if nz or not compact:
                out.append((path, nz[:64].hex() + ("..." if len(nz) > 64
                                                   else "")))
            continue
        shown = [(i, v) for i, v in enumerate(vals) if v or not compact]
        if shown:
            items = ",".join(f"{i}:{_fmt_val(kind, esz, v)}"
                             for i, v in shown[:maxarr])
            more = "..." if len(shown) > maxarr else ""
            out.append((path, f"[{items}{more}]"))
    return out


def params_struct(names, rpc, reply=False):
    """(struct name, data, header text) for the payload of an RPC."""
    fn = rpc["function"]
    p = rpc["params"]
    if fn == FN_ALLOC and len(p) >= ALLOC.size:
        cls = ALLOC.unpack_from(p, 0)[3]
        psz = ALLOC.unpack_from(p, 0)[5]
        return names.alloc_params.get(cls), p[ALLOC.size:ALLOC.size + psz], psz
    if fn == FN_CONTROL and len(p) >= CONTROL.size:
        cmd, psz = CONTROL.unpack_from(p, 0)[2], CONTROL.unpack_from(p, 0)[4]
        return names.ctrl_params.get(cmd), \
            p[CONTROL.size:CONTROL.size + psz], psz
    return names.rpc_params.get(fn), p, len(p)


def format_params(names, rpc, indent="      ", compact=True, limit=200):
    sname, data, psz = params_struct(names, rpc)
    if not sname:
        return ""
    st = names.struct(sname)
    if st is None:
        return ""
    lines = []
    head = f"{indent}{sname} ({psz} bytes"
    if rpc["function"] in (FN_ALLOC, FN_CONTROL) and st["s"] != psz:
        head += f"; 610.57.04 size {st['s']}: LAYOUT MISMATCH"
    lines.append(head + ")")
    for k, v in decode_struct(names, sname, data, compact=compact,
                              limit=limit):
        lines.append(f"{indent}  {k} = {v}")
    return "\n".join(lines)


def rpc_stream(records):
    """[(record, rpc)] for all GSP_MSG records in order; CONTINUATION_RECORDs
    (large RPCs, vgpu/rpc.c) are merged into the message they continue."""
    out = []
    last = {}
    for r in records:
        if r.type == T_GSP_QINFO:
            last.clear()
            continue
        if r.type != T_GSP_MSG:
            continue
        rpc = parse_rpc(r)
        q = r.f["queue"]
        if rpc and rpc["function"] == FN_CONTINUATION and q in last:
            prev = last[q][1]
            prev["params"] += rpc["params"]
            prev["continuations"] = prev.get("continuations", 0) + 1
            continue
        out.append((r, rpc))
        if rpc:
            last[q] = (r, rpc)
    return out


def hexdump(b, width=16, limit=None, indent="    "):
    out = []
    if limit is not None:
        b = b[:limit]
    for i in range(0, len(b), width):
        chunk = b[i:i + width]
        hx = " ".join(f"{x:02x}" for x in chunk)
        asc = "".join(chr(x) if 32 <= x < 127 else "." for x in chunk)
        out.append(f"{indent}{i:06x}  {hx:<{width * 3}} {asc}")
    return "\n".join(out)

# ------------------------------------------------------------------- FSP --


def fsp_messages(records):
    """Reassemble FSP packets (MCTP SOM/EOM) into messages per direction."""
    cur = {0: None, 1: None}
    for r in records:
        if r.type != T_FSP_PKT or len(r.data) < 4:
            continue
        hdr = struct.unpack_from("<I", r.data)[0]
        som, eom = bool(hdr >> 31 & 1), bool(hdr >> 30 & 1)
        d = r.f["dir"]
        if som or cur[d] is None:
            cur[d] = [r, b""]
        cur[d][1] += r.data[4:]
        if eom:
            yield d, cur[d][0], cur[d][1]
            cur[d] = None


def describe_fsp(msg):
    if len(msg) < 4:
        return "short", ""
    nvdm = struct.unpack_from("<I", msg)[0]
    t = nvdm >> 24
    name = NVDM_TYPES.get(t, f"nvdm_{t:#x}")
    extra = f"msgType=0x{nvdm & 0x7f:x} vendor=0x{(nvdm >> 8) & 0xffff:04x}"
    p = msg[4:]
    if t == 0x14 and len(p) >= 860:
        ver, size, fmc, fsys, fsys_sz, fvid, fvid_sz = \
            struct.unpack_from("<HHQQIQI", p, 0)
        boot = struct.unpack_from("<Q", p, 852)[0]
        extra += (f" version={ver} size={size} gspFmcSysmemOffset=0x{fmc:x} "
                  f"frtsSysmem=0x{fsys:x}+0x{fsys_sz:x} "
                  f"frtsVidmemOffset(from end)=0x{fvid:x}+0x{fvid_sz:x} "
                  f"gspBootArgsSysmemOffset=0x{boot:x}")
    elif t == 0x15 and len(p) >= 8:
        cmd, err = struct.unpack_from("<II", p, 0)
        extra += f" command=0x{cmd:x} ({NVDM_TYPES.get(cmd, '?')}) error=0x{err:x}"
    return name, extra

# -------------------------------------------------------------- printing --


def fmt_ts(r, t0):
    return f"{(r.ts - t0) / 1e9:12.6f}"


NV_DOORBELL = 0xb80000 + 0x30090
NV_PRAMIN_LO, NV_PRAMIN_HI = 0x700000, 0x800000
NV_BAR0_WINDOW = 0xd40          # NV_XAL_EP_ZB_BAR0_WINDOW (BASE 22:0 << 16)
DISP_CORE, DISP_WIN0 = 0x680000, 0x690000


def mmio_note(f):
    """Short decode of a few BAR0 writes worth explaining inline."""
    off, v = f["off"], f["val"]
    if not f["write"] or f["bar"] != 0:
        return ""
    if off == NV_DOORBELL:
        return (f"  <doorbell chid={v & 0xfff} runlist={(v >> 16) & 0x7f}"
                f"{' rl' if v >> 30 & 1 else ''}>")
    if off == DISP_CORE:
        return f"  <disp core PUT 0x{v:x}>"
    if DISP_WIN0 <= off < DISP_WIN0 + 32 * 0x1000 and not off & 0xfff:
        return f"  <disp window {(off - DISP_WIN0) >> 12} PUT 0x{v:x}>"
    if off == NV_BAR0_WINDOW:
        return f"  <PRAMIN -> VRAM 0x{(v & 0x7fffff) << 16:x}>"
    return ""


def line(names, r, t0, rpc=None):
    who = f"v{r.vcpu}" if r.vcpu >= 0 else f"t{r.tid}"
    head = f"{r.seq:10d} {fmt_ts(r, t0)} d{r.dev if r.dev != 255 else '-'} {who:>8} "
    f = r.f
    t = r.type
    if t == T_MMIO:
        rw = "W" if f["write"] else "R"
        o = ORIGINS.get(f["origin"], str(f["origin"]))
        reg = names.reg(f["off"]) if f["bar"] == 0 else \
            ("ROM" if f["bar"] == 6 else f"BAR{f['bar']}")
        return (head + f"MMIO {rw}{f['size']} bar{f['bar']}+0x{f['off']:07x} = "
                f"0x{f['val']:0{f['size'] * 2}x}  {reg}" + (f" [{o}]" if o else "")
                + mmio_note(f))
    if t == T_CFG:
        rw = "W" if f["write"] else "R"
        return head + f"CFG  {rw}{f['len']} 0x{f['off']:03x} = 0x{f['val']:x}"
    if t == T_IRQ:
        return (head + f"IRQ  {IRQ_KINDS.get(f['kind'], f['kind'])} vec {f['vector']} "
                f"addr=0x{f['addr']:x} data=0x{f['data']:x}"
                + (" masked" if f["masked"] else ""))
    if t == T_VEC:
        return (head + f"VEC  {VEC_EVENTS.get(f['event'], f['event'])} {f['vector']} "
                f"addr=0x{f['addr']:x} data=0x{f['data']:x}")
    if t == T_RESET:
        return head + f"RESET {RESET_KINDS.get(f['kind'], f['kind'])}"
    if t == T_BAR:
        return (head + f"BAR{f['bar']} 0x{f['old']:x} -> 0x{f['new']:x} "
                f"size 0x{f['size']:x}")
    if t == T_GSP_MSG:
        q = "CMD " if f["queue"] == 0 else "STAT"
        rpc = rpc or parse_rpc(r)
        fl = "".join(n for b, n in ((GSP_F_TRUNC, " TRUNC"), (GSP_F_NOSIG, " NOSIG"),
                                    (GSP_F_SEQGAP, " SEQGAP")) if f["gflags"] & b)
        if rpc:
            name, extra = describe_rpc(names, rpc)
            return (head + f"GSP {q} slot {f['slot']:2d}+{f['nelem']} {name} "
                    f"len={rpc['length']} rpcseq={rpc['rpc_seq']} "
                    f"result=0x{rpc['result']:x}{fl} {extra}")
        return head + f"GSP {q} slot {f['slot']} raw len={f['len']}{fl}"
    if t == T_DMA_SNAP:
        where = "blob" if f["sflags"] & 2 else f"gpa=0x{f['gpa']:x}"
        if f["sflags"] & SNAP_IMAGE:
            where = f"image+0x{f['gpa']:x}"
        return (head + f"SNAP {f['label']} {where} len=0x{f['len']:x}"
                + (" (not RAM)" if f["sflags"] & 1 else "")
                + (" (changed)" if f["sflags"] & SNAP_CHANGED else ""))
    if t == T_NOTE:
        return head + "NOTE " + r.data.decode("utf-8", "replace")
    if t == T_GSP_QSTATE:
        def p(v):
            return "-" if v == 0xffffffff else str(v)
        return (head + f"QSTATE cmd wp={p(f['cmd_wp'])} rp={p(f['cmd_rp'])} "
                f"stat wp={p(f['stat_wp'])} rp={p(f['stat_rp'])}")
    if t == T_LOST:
        return head + f"LOST {f['records']} records ({f['bytes']} bytes)"
    if t == T_FSP_PKT:
        d = "CPU->FSP" if f["dir"] == 0 else "FSP->CPU"
        hdr = struct.unpack_from("<I", r.data)[0] if len(r.data) >= 4 else 0
        return (head + f"FSP  {d} {f['len']} bytes mctp=0x{hdr:08x}"
                f"{' SOM' if hdr >> 31 & 1 else ''}{' EOM' if hdr >> 30 & 1 else ''}")
    if t == T_GSP_QINFO:
        return (head + f"QINFO shared=0x{f['shared_pa']:x} npte={f['npte']} "
                f"cmd=+0x{f['cmd_off']:x}/0x{f['cmd_size']:x} "
                f"stat=+0x{f['stat_off']:x}/0x{f['stat_size']:x} "
                f"elemHdr={f['elem_hdr']} via {QINFO_SRC.get(f['source'], f['source'])}")
    if t == T_DEV:
        return (head + f"DEV  {r.data.decode()} {f['vendor']:04x}:{f['device']:04x} "
                f"class {f['class_code']:06x} trap_mask=0x{f['trap_mask']:x} "
                f"gsp={f['dflags'] & 1}")
    if t == T_GPU_PAGE:
        return (head + f"GPUPAGE {APERTURES.get(f['aper'], f['aper'])}:"
                f"0x{f['pa']:010x} len=0x{f['len']:x} "
                f"{GPU_REASONS.get(f['reason'], f['reason'])} {f['tag']}")
    if t == T_GPU_HASHES:
        return (head + f"GPUHASHES pass {f['pass']} 0x{f['first_pa']:010x} "
                f"+{f['count']} pages")
    if t == T_PUSHBUF:
        fl = ",".join(v for k, v in PB_F.items() if f["pflags"] & k)
        return (head + f"PUSHBUF ch 0x{f['hobject']:08x} gp[{f['gp_index']}] "
                f"(get {f['gp_get']} put {f['gp_put']}) "
                f"entry=0x{f['entry']:016x} va=0x{f['va']:x} len={f['len']} "
                f"got={f['got']}" + (f" [{fl}]" if fl else ""))
    if t == T_BAR_PAGE:
        return (head + f"BARPAGE bar{f['bar']}+0x{f['off']:09x} "
                f"{'rescan' if f['reason'] else 'cpu-write'} "
                f"hash={f['hash']:016x}")
    return head + r.name()


def first_ts(paths):
    for r in iter_records(paths, warn=False):
        return r.ts
    return 0

# -------------------------------------------------------------- commands --


def cmd_summary(a, names):
    types = collections.Counter()
    devs = {}
    mmio = collections.Counter()
    blocks = collections.Counter()
    regs = collections.Counter()
    irqs = collections.Counter()
    rpcs = collections.Counter()
    lost = [0, 0]
    t0 = t1 = None
    n = 0
    for r in iter_records(a.trace):
        n += 1
        t0 = r.ts if t0 is None else t0
        t1 = r.ts
        types[r.name()] += 1
        if r.type == T_DEV:
            devs[r.dev] = (f"{r.data.decode()} {r.f['vendor']:04x}:"
                           f"{r.f['device']:04x} trap_mask=0x{r.f['trap_mask']:x}")
        elif r.type == T_MMIO:
            f = r.f
            mmio[(r.dev, f["bar"], "W" if f["write"] else "R",
                  ORIGINS.get(f["origin"], "?"))] += 1
            if f["bar"] == 0 and f["origin"] in (0, 1):
                blocks[(r.dev, names.block(f["off"]))] += 1
                regs[(r.dev, f["off"], "W" if f["write"] else "R")] += 1
        elif r.type == T_IRQ:
            irqs[(r.dev, IRQ_KINDS.get(r.f["kind"]), r.f["vector"])] += 1
        elif r.type == T_GSP_MSG:
            rpc = parse_rpc(r)
            q = "CMD" if r.f["queue"] == 0 else "STAT"
            rpcs[(q, names.rpc_name(rpc["function"]) if rpc else "raw")] += 1
        elif r.type == T_LOST:
            lost[0] += r.f["records"]
            lost[1] += r.f["bytes"]
    span = (t1 - t0) / 1e9 if n else 0
    print(f"records: {n}  span: {span:.3f} s  tables: {names.source}")
    if lost[0]:
        print(f"LOST: {lost[0]} records ({lost[1]} bytes)")
    print("\ndevices:")
    for d, s in sorted(devs.items()):
        print(f"  d{d}: {s}")
    print("\nrecord types:")
    for k, v in types.most_common():
        print(f"  {k:12s} {v}")
    print("\nMMIO by dev/bar/dir/origin:")
    for (d, b, rw, o), v in sorted(mmio.items()):
        print(f"  d{d} bar{b} {rw} {o or 'direct':10s} {v}")
    print(f"\nBAR0 blocks (top {a.top}):")
    for (d, b), v in blocks.most_common(a.top):
        print(f"  d{d} {b:40s} {v}")
    print(f"\nBAR0 registers (top {a.top}):")
    for (d, off, rw), v in regs.most_common(a.top):
        print(f"  d{d} {rw} 0x{off:07x} {names.reg(off):50s} {v}")
    if irqs:
        print("\ninterrupts:")
        for (d, k, vec), v in sorted(irqs.items()):
            print(f"  d{d} {k} vec {vec}: {v}")
    if rpcs:
        print("\nGSP RPCs:")
        for (q, fn), v in sorted(rpcs.items(), key=lambda x: (-x[1], x[0])):
            print(f"  {q:4s} {fn:40s} {v}")


def _type_filter(spec):
    if not spec:
        return None
    want = set()
    for s in spec.split(","):
        s = s.strip().upper()
        for k, v in TYPE_NAMES.items():
            if v == s or (s == "GSP" and v.startswith("GSP")):
                want.add(k)
    return want


def cmd_timeline(a, names):
    want = _type_filter(a.types)
    rx = re.compile(a.grep) if a.grep else None
    t0 = None
    shown = 0
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.seq < a.start or (want and r.type not in want):
            continue
        if a.dev is not None and r.dev != a.dev:
            continue
        if a.bar is not None and (r.type != T_MMIO or r.f["bar"] != a.bar):
            continue
        s = line(names, r, t0)
        if rx and not rx.search(s):
            continue
        print(s)
        shown += 1
        if a.count and shown >= a.count:
            break


def cmd_gsp(a, names):
    t0 = first_ts(a.trace)
    pending = {}
    recs = (r for r in iter_records(a.trace)
            if r.type in (T_GSP_MSG, T_GSP_QINFO))
    qinfo = []
    for r, rpc in rpc_stream(_tee_qinfo(recs, qinfo)):
        while qinfo and qinfo[0].seq < r.seq:
            print(line(names, qinfo.pop(0), t0))
        print(line(names, r, t0, rpc))
        if rpc:
            key = (rpc["function"], rpc["rpc_seq"])
            if r.f["queue"] == 0:
                pending[key] = r
            elif key in pending:
                req = pending.pop(key)
                print(f"{'':>34}reply to seq {req.seq} after "
                      f"{(r.ts - req.ts) / 1e3:.1f} us")
            if a.decode:
                txt = format_params(names, rpc, compact=not a.all)
                if txt:
                    print(txt)
            if a.hex:
                print(hexdump(rpc["params"], limit=a.hex_limit))
        elif a.hex:
            print(hexdump(r.data, limit=a.hex_limit))
    for q in qinfo:
        print(line(names, q, t0))


def _tee_qinfo(recs, sink):
    for r in recs:
        if r.type == T_GSP_QINFO:
            sink.append(r)
        yield r


def cmd_fsp(a, names):
    t0 = first_ts(a.trace)
    recs = list(r for r in iter_records(a.trace) if r.type == T_FSP_PKT)
    for d, first, msg in fsp_messages(recs):
        name, extra = describe_fsp(msg)
        print(f"{first.seq:10d} {fmt_ts(first, t0)} "
              f"{'CPU->FSP' if d == 0 else 'FSP->CPU'} {name} "
              f"{len(msg)} bytes {extra}")
        if a.hex:
            print(hexdump(msg, limit=a.hex_limit))


def cmd_snaps(a, names):
    t0 = None
    if a.extract:
        os.makedirs(a.extract, exist_ok=True)
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type != T_DMA_SNAP:
            continue
        print(line(names, r, t0))
        if a.extract and r.data:
            lab = re.sub(r"[^A-Za-z0-9_.-]", "_", r.f["label"])
            fn = os.path.join(a.extract, f"{r.seq:010d}_{lab}_{r.f['gpa']:x}.bin")
            with open(fn, "wb") as f:
                f.write(r.data)


def cmd_regs(a, names):
    c = collections.Counter()
    last = {}
    for r in iter_records(a.trace):
        if r.type == T_MMIO and r.f["bar"] == 0 and r.f["origin"] in (0, 1):
            k = (r.dev, r.f["off"])
            c[k + ("W" if r.f["write"] else "R",)] += 1
            last[k] = r.f["val"]
    for (d, off, rw), v in c.most_common(a.top):
        print(f"d{d} {rw} 0x{off:07x} {names.reg(off):50s} {v:10d}  "
              f"last=0x{last[(d, off)]:x}")


def cmd_json(a, names):
    for r in iter_records(a.trace):
        o = {"seq": r.seq, "ts": r.ts, "type": r.name(), "dev": r.dev,
             "vcpu": r.vcpu, "tid": r.tid}
        o.update({k: v for k, v in r.f.items() if not k.startswith("_")})
        if r.type == T_MMIO and r.f["bar"] == 0:
            o["reg"] = names.reg(r.f["off"])
        elif r.type == T_GSP_MSG:
            rpc = parse_rpc(r)
            if rpc:
                o["rpc"] = names.rpc_name(rpc["function"])
                o["rpc_desc"] = describe_rpc(names, rpc)[1]
            o["data"] = r.data.hex()
        elif r.type in (T_NOTE, T_DEV):
            o["text"] = r.data.decode("utf-8", "replace")
        elif r.type in (T_FSP_PKT,):
            o["data"] = r.data.hex()
        print(json.dumps(o))


# ------------------------------------------------------------ RM API log --

RMAPI_FN = {FN_ALLOC: "Alloc", FN_CONTROL: "Control", FN_FREE: "Free",
            FN_ALLOC_MEMORY: "AllocMemory"}


def _kv(items, limit=12):
    s = " ".join(f"{k}={v}" for k, v in items[:limit])
    return s + (" ..." if len(items) > limit else "")


def rm_calls(names, records):
    """The RM API sequence from the GSP command queue, matched with replies.

    Yields dicts: op (alloc/control/free/memory/internal), handles, class or
    cmd, raw params (bytes), status, reply params, timing."""
    stream = rpc_stream(records)
    replies = {}
    for r, rpc in stream:
        if rpc and r.f["queue"] == 1:
            replies.setdefault((rpc["function"], rpc["rpc_seq"]), (r, rpc))
    for r, rpc in stream:
        if not rpc or r.f["queue"] != 0:
            continue
        fn, p = rpc["function"], rpc["params"]
        rep = replies.get((fn, rpc["rpc_seq"]))
        c = {"seq": r.seq, "ts": r.ts, "rpc_seq": rpc["rpc_seq"],
             "function": names.rpc_name(fn), "rpc": rpc,
             "reply": rep[1] if rep else None,
             "latency_us": (rep[0].ts - r.ts) / 1e3 if rep else None}
        status = None
        if fn == FN_ALLOC and len(p) >= ALLOC.size:
            hc, hp, ho, cls, _, psz, fl = ALLOC.unpack_from(p, 0)
            c.update(op="alloc", hClient=hc, hParent=hp, hObject=ho,
                     hClass=cls, params=p[ALLOC.size:ALLOC.size + psz],
                     flags=fl)
            if rep and len(rep[1]["params"]) >= ALLOC.size:
                status = ALLOC.unpack_from(rep[1]["params"], 0)[4]
        elif fn == FN_CONTROL and len(p) >= CONTROL.size:
            hc, ho, cmd, _, psz, rf, cf, acc, _ = CONTROL.unpack_from(p, 0)
            c.update(op="control", hClient=hc, hObject=ho, cmd=cmd,
                     params=p[CONTROL.size:CONTROL.size + psz], flags=cf)
            if rep and len(rep[1]["params"]) >= CONTROL.size:
                rp = rep[1]["params"]
                status = CONTROL.unpack_from(rp, 0)[3]
                c["out"] = rp[CONTROL.size:CONTROL.size + psz]
        elif fn == FN_FREE and len(p) >= FREE.size:
            hr, hp, ho, _ = FREE.unpack_from(p, 0)
            c.update(op="free", hClient=hr, hParent=hp, hObject=ho)
            if rep and len(rep[1]["params"]) >= FREE.size:
                status = FREE.unpack_from(rep[1]["params"], 0)[3]
        elif fn == FN_ALLOC_MEMORY and len(p) >= 56:
            hc, hd, hm, cls, fl, adj, fmt = struct.unpack_from("<7I", p, 0)
            length, count = struct.unpack_from("<QI", p, 32)
            ptes = [struct.unpack_from("<Q", p, 56 + 8 * i)[0] << 12
                    for i in range(min(count, (len(p) - 56) // 8))]
            c.update(op="memory", hClient=hc, hParent=hd, hObject=hm,
                     hClass=cls, flags=fl, length=length, pteAdjust=adj,
                     pages=ptes, params=p)
        else:
            c.update(op="internal", params=p)
        if status is None and rep:
            status = rep[1]["result"]
        c["status"] = status
        yield c


def cmd_rmlog(a, names):
    t0 = first_ts(a.trace)
    objs = {}
    order = []
    jout = []
    recs = (r for r in iter_records(a.trace) if r.type == T_GSP_MSG)
    for c in rm_calls(names, recs):
        ts = f"{(c['ts'] - t0) / 1e9:12.6f}"
        st = "pending" if c["status"] is None else names.status_name(c["status"])
        lat = f" {c['latency_us']:.0f}us" if c["latency_us"] is not None else ""
        op = c["op"]
        if op == "internal" and not a.internal:
            continue
        if op == "alloc":
            sname = names.alloc_params.get(c["hClass"])
            prm = decode_struct(names, sname, c["params"]) if sname and \
                sname != "NvHandle" else []
            print(f"{c['seq']:10d} {ts} Alloc   hClient=0x{c['hClient']:08x} "
                  f"hParent=0x{c['hParent']:08x} hObject=0x{c['hObject']:08x} "
                  f"{names.class_name(c['hClass'])} ({len(c['params'])} B) "
                  f"-> {st}{lat}")
            if prm:
                print(f"{'':>24}{{{_kv(prm, a.fields)}}}")
            if c["status"] in (0, None):
                if c["hObject"] not in objs:
                    order.append(c["hObject"])
                objs[c["hObject"]] = [c["hParent"], c["hClass"], c["hClient"],
                                      True]
        elif op == "control":
            sname = names.ctrl_params.get(c["cmd"])
            prm = decode_struct(names, sname, c["params"]) if sname else []
            print(f"{c['seq']:10d} {ts} Control hClient=0x{c['hClient']:08x} "
                  f"hObject=0x{c['hObject']:08x} {names.ctrl_name(c['cmd'])} "
                  f"({len(c['params'])} B) -> {st}{lat}")
            if prm:
                print(f"{'':>24}in  {{{_kv(prm, a.fields)}}}")
            if a.out and sname and c.get("out"):
                o = decode_struct(names, sname, c["out"])
                if o:
                    print(f"{'':>24}out {{{_kv(o, a.fields)}}}")
        elif op == "free":
            print(f"{c['seq']:10d} {ts} Free    hClient=0x{c['hClient']:08x} "
                  f"hParent=0x{c['hParent']:08x} hObject=0x{c['hObject']:08x} "
                  f"-> {st}{lat}")
            if c["hObject"] in objs:
                objs[c["hObject"]][3] = False
        elif op == "memory":
            pg = ",".join(f"0x{x:x}" for x in c["pages"][:4])
            more = "..." if len(c["pages"]) > 4 else ""
            print(f"{c['seq']:10d} {ts} AllocMemory hClient=0x{c['hClient']:08x} "
                  f"hParent=0x{c['hParent']:08x} hMemory=0x{c['hObject']:08x} "
                  f"{names.class_name(c['hClass'])} length=0x{c['length']:x} "
                  f"pages=[{pg}{more}] -> {st}{lat}")
            if c["hObject"] not in objs:
                order.append(c["hObject"])
            objs[c["hObject"]] = [c["hParent"], c["hClass"], c["hClient"], True]
        else:
            print(f"{c['seq']:10d} {ts} # GSP-internal {c['function']} "
                  f"({len(c['params'])} B) -> {st}{lat}")
        if a.json:
            j = {k: c[k] for k in ("seq", "ts", "op", "function", "status",
                                   "latency_us") if k in c}
            for k in ("hClient", "hParent", "hObject", "hClass", "cmd",
                      "flags", "length", "pteAdjust"):
                if k in c:
                    j[k] = c[k]
            if "pages" in c:
                j["pages"] = c["pages"]
            j["params"] = c.get("params", b"").hex()
            if "out" in c:
                j["out"] = c["out"].hex()
            jout.append(j)
    if a.tree:
        print("\nobject tree (* = freed):")
        kids = collections.defaultdict(list)
        for h in order:
            kids[objs[h][0]].append(h)
        roots = [h for h in order if objs[h][0] not in objs]
        roots += [h for h in set(objs[h][0] for h in order)
                  if h not in objs and h not in roots]

        def walk(h, depth, seen):
            if h in seen:
                return
            seen.add(h)
            if h in objs:
                par, cls, cl, live = objs[h]
                print(f"  {'  ' * depth}0x{h:08x} {names.class_name(cls)}"
                      f"{'' if live else ' *'}")
            else:
                print(f"  {'  ' * depth}0x{h:08x}")
            for k in kids.get(h, []):
                walk(k, depth + 1, seen)
        seen = set()
        for h in sorted(set(r for r in roots if r not in objs)) + \
                [r for r in roots if r in objs]:
            walk(h, 0, seen)
    if a.json:
        with open(a.json, "w") as f:
            json.dump(jout, f, indent=1)

# ------------------------------------------------------------- boot table --


def cmd_boot(a, names):
    """BAR0 values the device returned before GSP INIT_DONE."""
    reads = collections.OrderedDict()
    writes = collections.Counter()
    fsp = []
    rpcs = []
    first = None
    done = None
    t0 = None
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type == T_MMIO and r.f["bar"] == 0 and r.f["origin"] in (0, 1):
            off = r.f["off"]
            if r.f["write"]:
                writes[off] += 1
                continue
            e = reads.setdefault(off, {"first": r.seq, "n": 0, "vals": []})
            e["n"] += 1
            v = r.f["val"]
            if e["vals"] and e["vals"][-1][0] == v:
                e["vals"][-1][1] += 1
            elif len(e["vals"]) < 64:
                e["vals"].append([v, 1])
        elif r.type == T_FSP_PKT:
            fsp.append(r)
        elif r.type == T_GSP_MSG:
            rpc = parse_rpc(r)
            if rpc:
                rpcs.append((r, rpc))
                if r.f["queue"] == 1 and rpc["function"] == EV_INIT_DONE:
                    done = r
                    break
        first = first or r
    print(f"BAR0 reads until {'GSP INIT_DONE at seq ' + str(done.seq) if done else 'end of trace (no INIT_DONE)'}:"
          f" {sum(e['n'] for e in reads.values())} reads of {len(reads)} "
          f"registers, {sum(writes.values())} writes")
    print(f"{'offset':>9} {'register':48s} {'reads':>7}  values (value x count, in order)")
    for off, e in sorted(reads.items(), key=lambda x: x[1]["first"]):
        vals = " ".join(f"0x{v:x}" + (f"x{n}" if n > 1 else "")
                        for v, n in e["vals"][:8])
        if len(e["vals"]) > 8:
            vals += f" ... ({len(e['vals'])} runs)"
        print(f"0x{off:07x} {names.reg(off):48.48s} {e['n']:7d}  {vals}")
    print("\nFSP messages:")
    for d, frst, msg in fsp_messages(fsp):
        name, extra = describe_fsp(msg)
        print(f"  {frst.seq:10d} {'CPU->FSP' if d == 0 else 'FSP->CPU'} "
              f"{name} {extra}")
    print("\nGSP RPCs before INIT_DONE:")
    for r, rpc in rpcs:
        print(f"  {r.seq:10d} {'CMD ' if r.f['queue'] == 0 else 'STAT'} "
              f"{names.rpc_name(rpc['function'])} len={rpc['length']}")
    if a.json:
        out = {"init_done_seq": done.seq if done else None,
               "reads": {f"0x{off:x}": {"name": names.reg(off), "n": e["n"],
                                        "values": [[f"0x{v:x}", n]
                                                   for v, n in e["vals"]]}
                         for off, e in reads.items()},
               "writes": {f"0x{off:x}": n for off, n in writes.items()},
               "fsp": [{"dir": d, "seq": f.seq, "msg": m.hex()}
                       for d, f, m in fsp_messages(fsp)],
               "rpcs": [{"seq": r.seq, "queue": r.f["queue"],
                         "function": names.rpc_name(p["function"]),
                         "params": p["params"].hex()} for r, p in rpcs]}
        with open(a.json, "w") as f:
            json.dump(out, f, indent=1)

# ---------------------------------------------------------------- display --

DISP_OPS = {0: "METHOD", 1: "JUMP", 2: "NONINC", 3: "SET_SUBDEVICE_MASK"}


def decode_disp_methods(names, cls, data):
    """Decode an NVDisplay pushbuffer fragment (clc37d.h DMA format)."""
    meth = names.disp.get(cls, {})
    out = []
    words = struct.unpack_from(f"<{len(data) // 4}I", data)
    i = 0
    while i < len(words):
        w = words[i]
        op, cnt, mth = w >> 29, (w >> 18) & 0x3ff, w & 0xfffc
        i += 1
        if w == 0:
            continue
        if op == 1:
            out.append(f"JUMP 0x{w & 0xfffc:x}")
            continue
        if op == 3:
            out.append(f"SET_SUBDEVICE_MASK 0x{w & 0xfff:x}")
            continue
        if op not in (0, 2):
            out.append(f"? 0x{w:08x}")
            continue
        for k in range(cnt):
            if i >= len(words):
                out.append("(truncated)")
                break
            m = mth if op == 2 else mth + 4 * k
            out.append(f"{meth.get(m, f'method_0x{m:04x}')} = 0x{words[i]:08x}")
            i += 1
    return out


def cmd_disp(a, names):
    t0 = None
    chan_cls = {}
    rx = re.compile(r"disp: channel (\d+) \(class 0x([0-9a-f]+) inst (\d+)\) "
                    r"pushbuffer (\w+) 0x([0-9a-f]+) size 0x([0-9a-f]+)")
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type == T_NOTE:
            m = rx.search(r.data.decode("utf-8", "replace"))
            if m:
                chan_cls[int(m.group(1))] = int(m.group(2), 16)
                print(line(names, r, t0))
        elif r.type == T_MMIO and r.f["write"] and r.f["bar"] == 0 and \
                mmio_note(r.f).startswith("  <disp"):
            print(line(names, r, t0))
        elif r.type == T_DMA_SNAP and r.f["label"].startswith("dispPB:"):
            lab = r.f["label"][7:]
            chn = int(lab.split("@")[0])
            if "@" not in lab:          # whole pushbuffer at registration
                print(line(names, r, t0))
                continue
            cls = chan_cls.get(chn, 0)
            print(line(names, r, t0) + f"  {names.class_name(cls)}")
            for m in decode_disp_methods(names, cls, r.data):
                print(f"{'':>24}{m}")

# ------------------------------------------------------------- BAR pages --


def cmd_bars(a, names):
    t0 = None
    per = collections.Counter()
    if a.extract:
        os.makedirs(a.extract, exist_ok=True)
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type == T_NOTE and r.data.startswith(b"bar pages"):
            print(line(names, r, t0))
        if r.type != T_BAR_PAGE:
            continue
        per[(r.f["bar"], r.f["off"])] += 1
        if not a.quiet:
            print(line(names, r, t0))
        if a.extract:
            fn = os.path.join(a.extract, f"{r.seq:010d}_bar{r.f['bar']}_"
                              f"{r.f['off']:09x}.bin")
            with open(fn, "wb") as f:
                f.write(r.data)
    print(f"\n{len(per)} distinct pages, {sum(per.values())} versions; "
          f"most rewritten:")
    for (bar, off), n in per.most_common(a.top):
        print(f"  bar{bar}+0x{off:09x} {n}")

# ---------------------------------------------------------------- PRAMIN --


def cmd_pramin(a, names):
    """VRAM accesses through the BAR0 PRAMIN window (0x700000, 1 MiB),
    positioned by NV_XAL_EP_ZB_BAR0_WINDOW (BASE in 64 KiB units)."""
    t0 = None
    base = None
    per = collections.Counter()
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type != T_MMIO or r.f["bar"] != 0 or r.f["origin"] not in (0, 1):
            continue
        off = r.f["off"]
        if off == NV_BAR0_WINDOW:
            base = (r.f["val"] & 0x7fffff) << 16 if r.f["write"] or \
                base is None else base
            if r.f["write"] and not a.quiet:
                print(line(names, r, t0))
            continue
        if not NV_PRAMIN_LO <= off < NV_PRAMIN_HI:
            continue
        va = None if base is None else base + off - NV_PRAMIN_LO
        per[(va or 0) & ~0xfff, "W" if r.f["write"] else "R"] += 1
        if not a.quiet:
            vs = "VRAM ?" if va is None else f"VRAM 0x{va:010x}"
            print(f"{r.seq:10d} {(r.ts - t0) / 1e9:12.6f} "
                  f"{'W' if r.f['write'] else 'R'}{r.f['size']} {vs} = "
                  f"0x{r.f['val']:x}")
    print(f"\nVRAM pages touched through PRAMIN: {len(set(k[0] for k in per))}")
    for (pg, rw), n in sorted(per.items())[:a.top]:
        print(f"  0x{pg:010x} {rw} {n}")


# ------------------------------------------------------------ GPU memory --


def cmd_gpu(a, names):
    """GPU memory read from the host side (VRAM/sysmem), sweep summary."""
    t0 = None
    tags = collections.Counter()
    sweep = {}
    if a.extract:
        os.makedirs(a.extract, exist_ok=True)
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type == T_NOTE and r.data.startswith(b"gpu:"):
            print(line(names, r, t0))
        elif r.type == T_GPU_PAGE:
            tags[(r.f["tag"], GPU_REASONS.get(r.f["reason"]))] += 1
            if not a.quiet:
                print(line(names, r, t0))
            if a.extract:
                lab = re.sub(r"[^A-Za-z0-9_.-]", "_", r.f["tag"])
                fn = os.path.join(a.extract, f"{r.seq:010d}_{lab}_"
                                  f"{APERTURES.get(r.f['aper'])}_{r.f['pa']:x}.bin")
                with open(fn, "wb") as f:
                    f.write(r.data)
        elif r.type == T_GPU_HASHES:
            p = r.f["pass"]
            s = sweep.setdefault(p, [0, 0])
            s[0] += r.f["count"]
            s[1] += sum(1 for h in r.data if h == 0)
    print("\nGPU pages logged per tag/reason:")
    for (tag, why), n in tags.most_common(a.top):
        print(f"  {tag:24s} {why:10s} {n}")
    for p, (n, bad) in sorted(sweep.items()):
        print(f"sweep pass {p}: {n} pages hashed, {bad} unreadable")


# Pushbuffer method format (clc56f.h NVC56F_DMA_*): SEC_OP 31:29,
# COUNT/IMMD 28:16, SUBCHANNEL 15:13, ADDRESS 11:0 (dwords).
PB_OPS = {0: "GRP0", 1: "INC", 2: "GRP2", 3: "NONINC", 4: "IMMD", 5: "ONEINC",
          6: "RSVD6", 7: "END_SEG"}


class MethodNames:
    def __init__(self, names):
        self.t = names.methods
        self.host = {}
        for c in sorted(self.t):
            if c & 0xff == 0x6f:
                self.host.update(self.t[c])

    def table(self, cls):
        if cls in self.t:
            return self.t[cls]
        fam = [c for c in self.t if c & 0xff == cls & 0xff]
        if not fam:
            return {}
        older = [c for c in fam if c <= cls]
        return self.t[max(older) if older else max(fam)]

    def name(self, cls, m):
        if m < 0x100:
            return self.host.get(m, f"host_0x{m:x}")
        return self.table(cls).get(m, f"0x{m:04x}")


def decode_pushbuffer(mn, data, subch):
    """Yield (subchannel, class, method, value, op) from pushbuffer bytes;
    subch: dict subchannel -> class, updated by SET_OBJECT (method 0)."""
    n = len(data) // 4
    w = struct.unpack_from(f"<{n}I", data)
    i = 0
    while i < n:
        h = w[i]
        i += 1
        op, cnt, sub, addr = h >> 29, (h >> 16) & 0x1fff, (h >> 13) & 7, \
            (h & 0xfff) * 4
        if op == 4:
            vals = [(addr, cnt)]
        elif op in (1, 3, 5):
            vals = []
            for k in range(cnt):
                if i >= n:
                    break
                m = addr + 4 * k if op == 1 else \
                    (addr if op == 3 or k == 0 else addr + 4)
                vals.append((m, w[i]))
                i += 1
        elif op == 7:
            yield sub, None, None, None, "END_PB_SEGMENT"
            return
        else:
            if h:
                yield sub, None, None, h, PB_OPS[op]
            continue
        for m, v in vals:
            if m == 0:
                subch[sub] = v & 0xffff
            yield sub, subch.get(sub), m, v, PB_OPS[op]


def cmd_pushbuf(a, names):
    t0 = None
    mn = MethodNames(names)
    chans = {}
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type != T_PUSHBUF:
            continue
        print(line(names, r, t0))
        if a.channel and r.f["hobject"] != int(a.channel, 0):
            continue
        subch = chans.setdefault((r.f["hclient"], r.f["hobject"]), {})
        shown = 0
        for sub, cls, m, v, op in decode_pushbuffer(mn, r.data, subch):
            if a.max and shown >= a.max:
                print(f"{'':>24}...")
                break
            shown += 1
            if m is None:
                print(f"{'':>24}[{sub}] {op}" +
                      (f" 0x{v:08x}" if v is not None else ""))
                continue
            cn = names.class_name(cls) if cls is not None else "?"
            print(f"{'':>24}[{sub}:{cn}] {mn.name(cls or 0, m)} = 0x{v:x}"
                  + ("" if op == "INC" else f" ({op})"))

# -------------------------------------------------- firmware images --


def cmd_fw(a, names):
    """Firmware images and GSP-RM sysmem areas dumped from guest memory."""
    t0 = None
    imgs = collections.OrderedDict()
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        if r.type == T_NOTE and r.data.startswith(b"fw:"):
            print(line(names, r, t0))
        if r.type == T_DMA_SNAP and r.f["sflags"] & SNAP_IMAGE:
            key = r.f["label"]
            if r.f["gpa"] == 0:             # a new dump of this image
                imgs[(key, r.seq)] = bytearray()
                cur = (key, r.seq)
            else:
                cur = next((k for k in reversed(imgs) if k[0] == key), None)
                if cur is None:
                    continue
            buf = imgs[cur]
            if len(buf) < r.f["gpa"]:
                buf.extend(b"\0" * (r.f["gpa"] - len(buf)))
            buf[r.f["gpa"]:r.f["gpa"] + len(r.data)] = r.data
    if a.extract:
        os.makedirs(a.extract, exist_ok=True)
    for (label, seq), buf in imgs.items():
        kind = "ELF" if buf[:4] == b"\x7fELF" else buf[:4].hex()
        print(f"{seq:10d} {label:24s} {len(buf):10d} bytes  starts {kind}")
        if a.extract:
            fn = os.path.join(a.extract, re.sub(r"[^A-Za-z0-9_.-]", "_",
                                                label) + f"_{seq}.bin")
            with open(fn, "wb") as f:
                f.write(buf)

# ------------------------------------------------------------- GSP logs --

LIBOS_TASKS = {1: "KERNEL", 2: "INIT", 3: "RM", 4: "INTR", 5: "VGPU",
               6: "MNOC", 7: "DEBUG", 8: "ROOT", 9: "RMON"}


def libos_records(buf, prev_put=0):
    """Records of a libos log buffer (liblogdecode.c): u64 put, then a ring
    of u64; each record is args..., packed metadata (VA 47:0, total words
    55:48, task 63:56), timestamp. Returns (put, [records oldest first])."""
    if len(buf) < 16:
        return 0, []
    n = len(buf) // 8 - 1
    w = struct.unpack_from(f"<{n + 1}Q", buf)
    put = w[0]
    lo = max(prev_put, put - n if put > n else 0)
    out = []
    i = put
    while i - 2 >= lo:
        ts = w[1 + (i - 1) % n]
        meta = w[1 + (i - 2) % n]
        total, task = (meta >> 48) & 0xff, meta >> 56
        if not 2 <= total <= 22 or task not in LIBOS_TASKS or i - total < lo:
            break
        args = [w[1 + (i - total + k) % n] for k in range(total - 2)]
        out.append({"idx": i - total, "ts": ts, "meta": meta & ((1 << 48) - 1),
                    "task": LIBOS_TASKS[task], "args": args})
        i -= total
    out.reverse()
    return put, out


class LogElf:
    """Format strings from a gsp_log_*.bin (.fwlogging*) if one is given."""

    def __init__(self, path):
        self.secs = []
        if not path:
            return
        with open(path, "rb") as f:
            d = f.read()
        if d[:4] != b"\x7fELF":
            raise ValueError(f"{path}: not an ELF file")
        shoff, = struct.unpack_from("<Q", d, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", d, 0x3a)
        hdrs = [struct.unpack_from("<IIQQQQIIQQ", d, shoff + i * shentsize)
                for i in range(shnum)]
        strtab = hdrs[shstrndx]
        for h in hdrs:
            name = d[strtab[4] + h[0]:].split(b"\0", 1)[0].decode()
            if h[3] and h[5]:
                self.secs.append((h[3], h[5], d[h[4]:h[4] + h[5]], name))

    def read(self, va, n):
        for addr, size, data, _ in self.secs:
            if addr <= va < addr + size:
                return data[va - addr:va - addr + n]
        return None

    def string(self, va):
        b = self.read(va, 4096)
        return b.split(b"\0", 1)[0].decode("latin-1") if b else None

    def format(self, rec):
        m = self.read(rec["meta"], 27)
        if not m or len(m) < 27:
            return None
        fname, fmt, line = struct.unpack_from("<QQQ", m)
        f = self.string(fmt)
        if f is None:
            return None
        args = iter(rec["args"])

        def sub(mo):
            if mo.group(0) == "%%":
                return "%"
            v = next(args, 0)
            c = mo.group(5)
            if c == "s":
                return self.string(v) or f"<str@0x{v:x}>"
            if c in "di":
                v = v - (1 << 64) if v >> 63 else v
                return str(v)
            if c in "xXp":
                return f"{v:x}" if c != "X" else f"{v:X}"
            if c == "c":
                return chr(v & 0xff)
            return str(v)
        txt = re.sub(r"%%|%([-0 #+]*)(\d*)(\.\d+)?(hh|h|ll|l|z|j|t)?([diuxXcsp])",
                     sub, f)
        src = self.string(fname) or "?"
        return f"{os.path.basename(src)}:{line} {txt.rstrip()}"


def cmd_gsplog(a, names):
    """GSP-RM's own log: the libos log buffers (GSPLOG snapshots) and
    EVENT_UCODE_LIBOS_PRINT RPCs."""
    elf = LogElf(a.log_elf)
    last = {}
    metas = collections.Counter()
    t0 = None
    for r in iter_records(a.trace):
        t0 = r.ts if t0 is None else t0
        src = None
        if r.type == T_DMA_SNAP and r.f["label"].startswith("GSPLOG:"):
            src, buf = r.f["label"][7:], r.data
        elif r.type == T_GSP_MSG and r.f["queue"] == 1:
            rpc = parse_rpc(r)
            if rpc and names.rpc_name(rpc["function"]) == \
                    "EVENT_UCODE_LIBOS_PRINT" and len(rpc["params"]) > 8:
                sz = struct.unpack_from("<I", rpc["params"], 4)[0]
                src, buf = "PRINT", rpc["params"][8:8 + sz]
        if src is None:
            continue
        put, recs = libos_records(buf, last.get(src, 0))
        if src != "PRINT":
            last[src] = put
        for rec in recs:
            metas[(rec["task"], rec["meta"])] += 1
            txt = elf.format(rec) if elf.secs else None
            if txt is None:
                txt = (f"meta=0x{rec['meta']:x} args=[" +
                       ",".join(f"0x{x:x}" for x in rec["args"]) + "]")
            if not a.summary:
                print(f"{r.seq:10d} {src:8s} {rec['task']:6s} "
                      f"ptimer={rec['ts']} {txt}")
    if a.summary or not elf.secs:
        print(f"\n{sum(metas.values())} records, {len(metas)} distinct "
              f"messages (format strings need gsp_log_*.bin: --log-elf)")
        for (task, meta), n in metas.most_common(a.top):
            print(f"  {task:6s} meta=0x{meta:x} {n}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tables", help="nvtables.json.gz (default: next to script)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    def add(name, fn):
        p = sub.add_parser(name)
        p.add_argument("trace", nargs="+")
        p.set_defaults(fn=fn)
        return p

    p = add("summary", cmd_summary)
    p.add_argument("--top", type=int, default=40)
    p = add("timeline", cmd_timeline)
    p.add_argument("--types", help="comma list: mmio,cfg,irq,vec,reset,bar,"
                   "gsp,gsp_msg,dma_snap,note,fsp_pkt,lost,dev")
    p.add_argument("--dev", type=int)
    p.add_argument("--bar", type=int)
    p.add_argument("--start", type=int, default=0, help="first seq")
    p.add_argument("--count", type=int, default=0)
    p.add_argument("--grep", help="regex on the printed line")
    p = add("gsp", cmd_gsp)
    p.add_argument("--hex", action="store_true")
    p.add_argument("--hex-limit", type=int, default=512)
    p.add_argument("--decode", action="store_true",
                   help="decode the parameters (610.57.04 layouts)")
    p.add_argument("--all", action="store_true",
                   help="with --decode: also zero fields")
    p = add("rmlog", cmd_rmlog)
    p.add_argument("--out", action="store_true",
                   help="also decode control reply (output) parameters")
    p.add_argument("--tree", action="store_true", help="print the object tree")
    p.add_argument("--internal", action="store_true",
                   help="also list GSP-internal RPCs (registry, system info..)")
    p.add_argument("--fields", type=int, default=12,
                   help="parameter fields per line")
    p.add_argument("--json", metavar="FILE",
                   help="write the call sequence as JSON (for replay/forward)")
    p = add("boot", cmd_boot)
    p.add_argument("--json", metavar="FILE")
    add("disp", cmd_disp)
    p = add("bars", cmd_bars)
    p.add_argument("--extract", metavar="DIR")
    p.add_argument("--quiet", action="store_true")
    p.add_argument("--top", type=int, default=20)
    p = add("gpu", cmd_gpu)
    p.add_argument("--extract", metavar="DIR")
    p.add_argument("--quiet", action="store_true")
    p.add_argument("--top", type=int, default=40)
    p = add("pushbuf", cmd_pushbuf)
    p.add_argument("--channel", help="only this channel handle")
    p.add_argument("--max", type=int, default=0, help="methods per segment")
    p = add("fw", cmd_fw)
    p.add_argument("--extract", metavar="DIR")
    p = add("gsplog", cmd_gsplog)
    p.add_argument("--log-elf", help="gsp_log_*.bin with .fwlogging sections")
    p.add_argument("--summary", action="store_true")
    p.add_argument("--top", type=int, default=40)
    p = add("pramin", cmd_pramin)
    p.add_argument("--quiet", action="store_true")
    p.add_argument("--top", type=int, default=40)
    p = add("fsp", cmd_fsp)
    p.add_argument("--hex", action="store_true")
    p.add_argument("--hex-limit", type=int, default=1024)
    p = add("snaps", cmd_snaps)
    p.add_argument("--extract", metavar="DIR")
    p = add("regs", cmd_regs)
    p.add_argument("--top", type=int, default=100)
    add("json", cmd_json)
    a = ap.parse_args(argv)
    try:
        a.fn(a, Names(a.tables))
        sys.stdout.flush()
    except BrokenPipeError:
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())


if __name__ == "__main__":
    main()

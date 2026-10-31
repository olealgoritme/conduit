#!/usr/bin/env python3
"""ndldump.py - decode nvddi kernel DDI logs (.ndl, format version 1).

    ndldump.py LOG                     summary (same as --summary)
    ndldump.py LOG --timeline [-v]     every call in order, callbacks nested under the DDI
                                       that made them, one line each (-v: all fields + blobs)
    ndldump.py LOG --calls [-v]        like --timeline with every decoded field and blob
    ndldump.py LOG --paging [-v]       DxgkDdiBuildPagingBuffer: operation, union member,
                                       DMA bytes written (NVIDIA methods), MDL/ADL pages, PTEs
    ndldump.py LOG --escapes [-v]      DxgkDdiEscape: NVDA header, RM names, in/out payloads
    ndldump.py LOG --vidpn [-v]        modes, VidPN commits, source addresses, VidPN callbacks
    ndldump.py LOG --events            driver events, registrations, wrapped tables, IRPs
    ndldump.py LOG --stats             per function: calls, errors, time, IRQLs
    ndldump.py LOG --schema            wrapped tables, slots, argument kinds, capture rules
    ndldump.py LOG --dump-blobs DIR    write every captured buffer to DIR
  filters: --grep TEXT (function name substring), --from SEQ, --to SEQ, --limit N

Names come from the schema record in the log; field layouts from
ndl_layouts.json (generated from the WDK headers by gen/gen.py); NVIDIA RM
names from rmnames.json (open-gpu-kernel-modules 610.57.04).
"""

import argparse
import collections
import json
import mmap
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

HDR = struct.Struct("<8sIIQQQQIIIIIIIIIIII")
REC = struct.Struct("<IHHQQQIIHBBI")
CALL = struct.Struct("<HHBBHQQ")
BLOB = struct.Struct("<IIHHHHIHHQ")
EVENT = struct.Struct("<IIQQ")
IRP = struct.Struct("<BBBBIIIIIQQ")
REGISTER = struct.Struct("<QQIIIIQHHI")
TABLE = struct.Struct("<IIQQII")
DROP = struct.Struct("<QQII")
END = struct.Struct("<QQQ")
REGISTRY = struct.Struct("<IIIIIHHIIHH")

T_PAD, T_SCHEMA, T_EVENT, T_IRP, T_REGISTER, T_TABLE, T_CALL, T_RET, T_BLOB, T_DROP, T_END, T_REGISTRY = range(12)
TNAMES = ["PAD", "SCHEMA", "EVENT", "IRP", "REGISTER", "TABLE", "CALL", "RET", "BLOB", "DROP", "END", "REGISTRY"]
REG_OPS = {1: "SetValue", 2: "DeleteValue", 21: "EnumerateValue", 23: "QueryValue", 27: "CreateKey",
           29: "OpenKey"}   # REG_NOTIFY_CLASS
REG_TYPES = {0: "NONE", 1: "SZ", 2: "EXPAND_SZ", 3: "BINARY", 4: "DWORD", 7: "MULTI_SZ", 11: "QWORD"}
B_ARG, B_NEST, B_ELEM, B_LENBUF, B_TABLE, B_INIT = 1, 2, 3, 4, 5, 6
BF = [(1, "post"), (2, "trunc"), (4, "partial"), (8, "failed"), (0x10, "irql"), (0x20, "direct"), (0x40, "user")]
EV = {1: "LOAD", 2: "ATTACH", 3: "ATTACH_FAIL", 4: "REGISTER_IOCTL", 5: "HOOK_SKIP", 6: "TABLE_SKIP",
      7: "START_FAIL", 8: "LEVEL", 9: "FILE", 10: "HEALTHY", 11: "INFO"}
IRQL = {0: "P", 1: "A", 2: "D"}
IRP_MJ = {0: "CREATE", 2: "CLOSE", 3: "READ", 4: "WRITE", 14: "DEVICE_CONTROL", 15: "INTERNAL_DEVICE_CONTROL",
          18: "CLEANUP", 22: "POWER", 27: "PNP"}

NVDA_MAGIC = 0x4E564441


def load_json(name, default):
    p = os.path.join(HERE, name)
    try:
        with open(p) as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


RM = load_json("rmnames.json", {"classes": {}, "ctrls": {}, "escapes": {}, "status": {}})
RM = {k: {int(a): b for a, b in RM.get(k, {}).items()} for k in ("classes", "ctrls", "escapes", "status")}
NTSTATUS = {int(k): v for k, v in load_json("ntstatus.json", {}).items()}
NVMETH = {int(k, 16): {int(a): b for a, b in v.items()} for k, v in load_json("nvmethods.json", {}).items()}


def status_name(v):
    v &= 0xFFFFFFFF
    if v == 0:
        return "OK"
    n = NTSTATUS.get(v)
    return n[7:] if n and n.startswith("STATUS_") else (n or "0x%08x" % v)


def fourcc(v):
    b = struct.pack(">I", v & 0xFFFFFFFF)
    if all(32 <= c < 127 for c in b):
        return b.decode("ascii")
    return None


def hexdump(data, limit=256, indent="      "):
    out = []
    n = len(data) if limit is None else min(len(data), limit)
    for o in range(0, n, 16):
        chunk = data[o:o + 16]
        hx = " ".join("%02x" % c for c in chunk)
        asc = "".join(chr(c) if 32 <= c < 127 else "." for c in chunk)
        out.append("%s%06x  %-47s  %s" % (indent, o, hx, asc))
    if limit is not None and len(data) > limit:
        out.append("%s... %d more bytes" % (indent, len(data) - limit))
    return out


# ---------------------------------------------------------------- schema

class Schema:
    def __init__(self, text):
        self.tables, self.slots, self.rulesets, self.rules = {}, {}, {}, {}
        self.wdk = ""
        for line in text.split("\n"):
            p = line.split(" ")
            if not p or not p[0]:
                continue
            k = p[0]
            if k == "WDK":
                self.wdk = p[1] if len(p) > 1 else ""
            elif k == "T":
                self.tables[int(p[1])] = {"name": p[2], "tag": p[3], "nslots": int(p[4]), "size": int(p[5])}
            elif k == "S":
                t, i = int(p[1]), int(p[2])
                self.slots[(t, i)] = {"name": p[3], "off": int(p[4]), "nargs": int(p[5]), "retkind": int(p[6]),
                                      "irql": p[7], "fname": p[8], "ret": " ".join(p[9:]), "args": []}
            elif k == "A":
                t, i = int(p[1]), int(p[2])
                self.slots[(t, i)]["args"].append({
                    "name": p[4], "kind": p[5], "flags": int(p[6]), "size": int(p[7]), "rs": int(p[8]),
                    "lenarg": int(p[9]), "rec": None if p[10] == "-" else p[10], "type": " ".join(p[11:])})
            elif k == "RS":
                self.rulesets[int(p[1])] = {"name": p[2], "size": int(p[3])}
            elif k == "R":
                self.rules[int(p[1])] = {"owner": p[2], "path": p[3], "kind": p[4], "when": int(p[5]),
                                         "elem": None if p[6] == "-" else p[6], "sub": int(p[7]),
                                         "len": None if p[8] == "-" else p[8],
                                         "cond": None if len(p) < 10 or p[9] == "-" else p[9]}
        self.by_name = {s["name"]: k for k, s in self.slots.items()}

    def slot(self, t, i):
        return self.slots.get((t, i)) or {"name": "t%d.s%d" % (t, i), "args": [], "retkind": 1, "ret": "?",
                                          "irql": "-", "fname": "-", "nargs": 0, "off": 0}


# ---------------------------------------------------------------- layouts

PTR_TYPES = {"HANDLE", "PVOID", "PMDL", "PDEVICE_OBJECT", "LPVOID", "PUCHAR", "PULONG", "PWSTR", "PCWSTR",
             "D3DKMDT_HVIDPN", "D3DKMDT_HVIDPNTOPOLOGY", "D3DKMDT_HVIDPNSOURCEMODESET",
             "D3DKMDT_HVIDPNTARGETMODESET", "D3DKMDT_HMONITORSOURCEMODESET", "D3DKMDT_ADAPTER",
             "PFILE_OBJECT", "PKEVENT", "PEPROCESS", "PETHREAD"}
SIGNED = {"INT", "LONG", "int", "long", "LONGLONG", "INT64", "INT32", "INT16", "INT8", "SHORT", "LONG64",
          "long long", "signed char", "short", "__int64", "LONG_PTR", "INT_PTR"}
HEX_NAMES = ("Address", "Handle", "Va", "VA", "Mask", "Flags", "Value", "Pfn", "Base", "Segment", "Offset")

# union members picked by a discriminator field: tag -> (field, enum prefix, overrides)
SELECT = {
    "_DXGKARG_BUILDPAGINGBUFFER": ("Operation", "DXGK_OPERATION_",
                                   {19: "MmapMmu", 22: "NotifyAllocation", 21: "NotifyResidency2",
                                    18: "NotifyFenceResidency", 17: "MapApertureSegment2"}),
    "_DXGKARGCB_NOTIFY_INTERRUPT_DATA": ("InterruptType", "DXGK_INTERRUPT_", {}),
}


class Layouts:
    def __init__(self, path):
        d = load_json(path, None) if path else None
        if d is None:
            d = load_json("ndl_layouts.json", {"records": {}, "typedefs": {}, "enums": {}, "enum_typedefs": {}})
        self.records = d["records"]
        self.typedefs = d["typedefs"]
        self.enums = {k: {v: n for n, v in e.items()} for k, e in d["enums"].items()}
        self.enum_typedefs = d["enum_typedefs"]
        self._sel_cache = {}

    def tag(self, name):
        if not name:
            return None
        if name in self.records:
            return name
        if "_" + name in self.records:
            return "_" + name
        q = self.typedefs.get(name, "")
        if q.startswith(("struct ", "union ")) and q.split(" ", 1)[1] in self.records:
            return q.split(" ", 1)[1]
        return None

    def size(self, name):
        t = self.tag(name)
        return self.records[t]["size"] if t else None

    def enum_of(self, typ):
        typ = typ.replace("const ", "").strip()
        if typ.startswith("enum "):
            return self.enums.get(typ[5:])
        e = self.enum_typedefs.get(typ)
        return self.enums.get(e) if e else None

    def is_ptr(self, typ):
        t = typ.replace("const ", "").strip()
        if t.endswith("*") or t in PTR_TYPES:
            return True
        q = self.typedefs.get(t, "")
        return q.endswith("*")

    def selector(self, tag, data, fields):
        sel = SELECT.get(tag)
        if not sel:
            return None
        field, prefix, over = sel
        for (off, depth, name, typ, size, bits) in fields:
            if name == field and depth == 1:
                v = int.from_bytes(data[off:off + (size or 4)], "little")
                if v in over:
                    return over[v]
                e = self.enum_of(typ) or {}
                n = e.get(v)
                if not n or not n.startswith(prefix):
                    return None
                want = n[len(prefix):].replace("_", "").lower()
                members = [f[2] for f in fields if f[1] == 2 and f[2]]
                for m in members:
                    if m.lower() == want:
                        return m
                return None
        return None

    def decode(self, name, data):
        """[(path, text, int value or None)] for the leaf fields of record 'name'."""
        t = self.tag(name)
        if not t:
            return None
        rec = self.records[t]
        fields = rec["fields"]
        active = self.selector(t, data, fields)
        out = []
        stack = []
        for i, (off, depth, fname, typ, size, bits) in enumerate(fields):
            while stack and stack[-1][0] >= depth:
                stack.pop()
            skip = bool(stack and stack[-1][2])
            if (not skip and active is not None and depth == 2 and stack and stack[-1][1] is None and
                    stack[-1][0] == 1 and typ and fields[i - 1] is not None):
                skip = fname != active
            container = i + 1 < len(fields) and fields[i + 1][1] > depth
            big_int = container and "_LARGE_INTEGER" in typ
            stack.append((depth, fname, skip or big_int))
            if skip or fname is None:
                continue
            if container and not big_int:
                continue
            path = ".".join(n for (_, n, _) in stack if n)
            if off + 1 > len(data):
                continue
            if bits:
                lo, hi = bits
                raw = int.from_bytes(data[off:off + 8].ljust(8, b"\0"), "little")
                v = (raw >> lo) & ((1 << (hi - lo + 1)) - 1)
                out.append((path, str(v) if v < 10 else "0x%x" % v, v))
                continue
            if big_int:
                size = 8
            if size is None:
                continue
            raw = data[off:off + size]
            if "[" in typ or size > 8:
                if raw.count(0) == len(raw):
                    out.append((path, "0", 0))
                elif size <= 64:
                    out.append((path, raw.hex(), None))
                else:
                    out.append((path, "<%d bytes>" % size, None))
                continue
            v = int.from_bytes(raw, "little")
            e = self.enum_of(typ)
            if e is not None:
                vs = v - (1 << 32) if size == 4 and v >= 0x80000000 else v
                out.append((path, e.get(vs, str(vs)), v))
            elif self.is_ptr(typ) or big_int or any(h in fname for h in HEX_NAMES):
                out.append((path, "0x%x" % v, v))
            elif typ.replace("const ", "").strip() in SIGNED and v >= 1 << (8 * size - 1):
                out.append((path, str(v - (1 << (8 * size))), v))
            else:
                out.append((path, str(v), v))
        return out

    def oneline(self, name, data, skip_zero=True, limit=None):
        d = self.decode(name, data)
        if d is None:
            return None
        parts = []
        for p, s, v in d:
            if skip_zero and (v == 0 or s == "0"):
                continue
            if "Reserved" in p:
                continue
            parts.append("%s=%s" % (p, s))
        if limit and len(parts) > limit:
            parts = parts[:limit] + ["..."]
        return " ".join(parts)


# ---------------------------------------------------------------- log

class Blob:
    __slots__ = ("len", "orig", "kind", "arg", "rule", "flags", "elem", "parent", "depth", "addr", "off", "log")

    def data(self):
        return bytes(self.log.mm[self.off:self.off + self.len])

    def fl(self):
        return ",".join(n for b, n in BF if self.flags & b)


def read_blob(log, off):
    b = Blob()
    (b.len, b.orig, b.kind, b.arg, b.rule, b.flags, b.elem, b.parent, b.depth, b.addr) = BLOB.unpack_from(log.mm, off)
    b.off = off + BLOB.size
    b.log = log
    return b, off + BLOB.size + ((b.len + 7) & ~7)


class Call:
    __slots__ = ("seq", "qpc", "cpu", "irql", "pid", "tid", "thread", "table", "slot", "args", "blobs",
                 "ret", "ret_seq", "ret_qpc", "post_args", "post_blobs", "nested", "post_nested", "depth",
                 "parent", "flags")

    def __init__(self):
        self.ret = None
        self.ret_seq = 0
        self.ret_qpc = None
        self.post_args = None
        self.post_blobs = []
        self.nested = []
        self.post_nested = []
        self.depth = 0
        self.parent = None


class Log:
    def __init__(self, path):
        self.path = path
        self.f = open(path, "rb")
        if os.fstat(self.f.fileno()).st_size < HDR.size:
            raise SystemExit("%s: too short for an nvddi log" % path)
        self.mm = mmap.mmap(self.f.fileno(), 0, access=mmap.ACCESS_READ)
        if len(self.mm) < HDR.size or self.mm[:8] != b"NVDDINDL":
            raise SystemExit("%s: not an nvddi log" % path)
        h = HDR.unpack_from(self.mm, 0)
        self.hdr = dict(zip(["magic", "version", "header_size", "qpc_freq", "start_qpc", "start_systime",
                             "load_qpc", "boot_count", "file_index", "ncpu", "ring_bytes", "os_major", "os_minor",
                             "os_build", "level", "max_blob", "nvddi_version", "wdk_ddi_version", "rsv"], h))
        if self.hdr["version"] != 1:
            raise SystemExit("%s: format version %d not supported" % (path, self.hdr["version"]))
        self.freq = self.hdr["qpc_freq"] or 10000000
        self.schema = Schema("")
        self.calls = {}           # seq -> Call
        self.order = []           # all Call in seq order
        self.events, self.irps, self.registers, self.tables, self.drops = [], [], [], [], []
        self.registry = []
        self.end = None
        self.bad = None
        self.ret_only = 0
        self.nrec = collections.Counter()
        self._parse()

    def ms(self, qpc):
        return (qpc - self.hdr["start_qpc"]) * 1000.0 / self.freq

    def _parse(self):
        mm = self.mm
        off = self.hdr["header_size"]
        n = len(mm)
        rets, blobs = [], []
        while off + REC.size <= n:
            (size, typ, flags, seq, qpc, thread, pid, tid, cpu, irql, _, _) = REC.unpack_from(mm, off)
            if size < REC.size or size % 8 or off + size > n:
                self.bad = "corrupt record at 0x%x (size %d); stopped" % (off, size)
                break
            p = off + REC.size
            self.nrec[typ] += 1
            if typ == T_SCHEMA:
                txt = bytes(mm[p:off + size]).split(b"\0", 1)[0].decode("ascii", "replace")
                self.schema = Schema(txt)
            elif typ in (T_CALL, T_RET):
                (table, slot, nargs, phase, nblobs, call_seq, ret) = CALL.unpack_from(mm, p)
                q = p + CALL.size
                args = list(struct.unpack_from("<%dQ" % nargs, mm, q))
                q += 8 * nargs
                bl = []
                for _ in range(nblobs):
                    b, q = read_blob(self, q)
                    bl.append(b)
                if typ == T_CALL:
                    c = Call()
                    c.seq, c.qpc, c.cpu, c.irql, c.pid, c.tid, c.thread = seq, qpc, cpu, irql, pid, tid, thread
                    c.table, c.slot, c.args, c.blobs, c.flags = table, slot, args, bl, flags
                    self.calls[seq] = c
                else:
                    rets.append((seq, qpc, table, slot, call_seq, ret, args, bl, cpu, irql, pid, tid, thread))
            elif typ == T_BLOB:
                owner = struct.unpack_from("<Q", mm, p)[0]
                b, _ = read_blob(self, p + 8)
                blobs.append((owner, b))
            elif typ == T_EVENT:
                code, st, a, b = EVENT.unpack_from(mm, p)
                txt = bytes(mm[p + EVENT.size:off + size]).split(b"\0", 1)[0].decode("utf-8", "replace")
                self.events.append((seq, qpc, code, st, a, b, txt))
            elif typ == T_IRP:
                self.irps.append((seq, qpc, IRP.unpack_from(mm, p)))
            elif typ == T_REGISTER:
                r = REGISTER.unpack_from(mm, p)
                q = p + REGISTER.size
                name = bytes(mm[q:q + r[7]]).decode("utf-16-le", "replace")
                reg = bytes(mm[q + r[7]:q + r[7] + r[8]]).decode("utf-16-le", "replace")
                q += (r[7] + r[8] + 7) & ~7
                bl = []
                for _ in range(r[9]):
                    b, q = read_blob(self, q)
                    bl.append(b)
                self.registers.append((seq, qpc, r, name, reg, bl))
            elif typ == T_TABLE:
                t = TABLE.unpack_from(mm, p)
                b, _ = read_blob(self, p + TABLE.size)
                self.tables.append((seq, qpc, t, b))
            elif typ == T_DROP:
                self.drops.append((seq, qpc, DROP.unpack_from(mm, p)))
            elif typ == T_END:
                self.end = END.unpack_from(mm, p)
            elif typ == T_REGISTRY:
                r = REGISTRY.unpack_from(mm, p)
                q = p + REGISTRY.size
                key = bytes(mm[q:q + r[5]]).decode("utf-16-le", "replace")
                val = bytes(mm[q + r[5]:q + r[5] + r[6]]).decode("utf-16-le", "replace")
                q += (r[5] + r[6] + 7) & ~7
                data = bytes(mm[q:q + r[7]])
                self.registry.append((seq, qpc, thread, pid, tid, r, key, val, data))
            off += size
        for (seq, qpc, table, slot, call_seq, ret, args, bl, cpu, irql, pid, tid, thread) in rets:
            c = self.calls.get(call_seq)
            if c is None:
                c = Call()
                c.seq, c.qpc, c.cpu, c.irql, c.pid, c.tid, c.thread = seq, qpc, cpu, irql, pid, tid, thread
                c.table, c.slot, c.args, c.blobs, c.flags = table, slot, args, [], 0
                self.calls[seq] = c
                self.ret_only += 1
            c.ret, c.ret_seq, c.ret_qpc, c.post_args, c.post_blobs = ret, seq, qpc, args, bl
        by_ret = {c.ret_seq: c for c in self.calls.values() if c.ret_seq}
        for owner, b in blobs:
            c = self.calls.get(owner)
            if c is not None and c.seq == owner:
                c.nested.append(b)
            else:
                c = by_ret.get(owner)
                if c is not None:
                    c.post_nested.append(b)
        self.order = sorted(self.calls.values(), key=lambda c: c.seq)
        # nesting: a call made on the same thread while another is open
        open_ = collections.defaultdict(list)
        events = []
        for c in self.order:
            events.append((c.seq, 0, c))
            if c.ret_seq:
                events.append((c.ret_seq, 1, c))
        events.sort(key=lambda e: e[0])
        for r in self.registry:
            events.append((r[0], 2, r))
        events.sort(key=lambda e: e[0])
        self.reg_depth = {}
        for _, kind, c in events:
            if kind == 2:
                self.reg_depth[c[0]] = len(open_[c[2]])
                continue
            st = open_[c.thread]
            if kind == 0:
                c.depth = len(st)
                c.parent = st[-1] if st else None
                if c.ret_seq:
                    st.append(c)
            else:
                if c in st:
                    while st and st[-1] is not c:
                        st.pop()
                    if st:
                        st.pop()

    def name(self, c):
        return self.schema.slot(c.table, c.slot)["name"]

    def dur_us(self, c):
        if c.ret_qpc is None:
            return None
        return (c.ret_qpc - c.qpc) * 1e6 / self.freq

    def arg_blob(self, c, i, post=False):
        for b in (c.post_blobs if post else c.blobs):
            if b.kind in (B_ARG, B_LENBUF) and b.arg == i:
                return b
        return None

    def rule_blobs(self, c, path=None, post=None):
        out = []
        for b in (c.nested if post is False else c.post_nested if post else c.nested + c.post_nested):
            r = self.schema.rules.get(b.rule)
            if path is None or (r and r["path"] == path):
                out.append((r, b))
        return out


# ---------------------------------------------------------------- formatting helpers

def ret_text(log, c):
    s = log.schema.slot(c.table, c.slot)
    if c.ret is None:
        return "(no return)"
    if s["retkind"] == 0:
        return ""
    if s["ret"] in ("NTSTATUS",):
        return status_name(c.ret)
    if s["ret"] == "BOOLEAN":
        return "TRUE" if c.ret & 0xFF else "FALSE"
    return "0x%x" % c.ret


def is_error(log, c):
    s = log.schema.slot(c.table, c.slot)
    return c.ret is not None and s["ret"] == "NTSTATUS" and (c.ret & 0xFFFFFFFF) >= 0x80000000


def arg_summary(log, L, c, verbose=False, post=False):
    s = log.schema.slot(c.table, c.slot)
    parts = []
    args = c.post_args if post and c.post_args else c.args
    for i, v in enumerate(args):
        a = s["args"][i] if i < len(s["args"]) else {"name": "a%d" % i, "kind": "VAL", "rec": None}
        b = log.arg_blob(c, i, post)
        if b is not None and a.get("rec") and b.len:
            one = L.oneline(a["rec"], b.data(), limit=None if verbose else 12)
            parts.append("%s={%s}" % (a["name"], one if one is not None else "%d bytes" % b.len))
        elif b is not None and b.len and a["kind"] == "PSCALAR" and b.len <= 8:
            parts.append("%s=*%#x" % (a["name"], int.from_bytes(b.data(), "little")))
        elif v:
            parts.append("%s=%#x" % (a["name"], v))
    return " ".join(parts)


def blob_lines(log, L, c, verbose, indent="      "):
    out = []
    groups = [("pre", c.blobs, c.nested), ("post", c.post_blobs, c.post_nested)]
    for phase, args, nested in groups:
        for b in args:
            s = log.schema.slot(c.table, c.slot)
            a = s["args"][b.arg] if b.arg < len(s["args"]) else {"name": "a%d" % b.arg, "rec": None}
            out.append("%s[%s] arg %s @%#x %d bytes %s" % (indent, phase, a["name"], b.addr, b.len, b.fl()))
            if verbose and a.get("rec") and b.len:
                for p, sv, v in (L.decode(a["rec"], b.data()) or []):
                    if v != 0 and sv != "0":
                        out.append("%s    %s = %s" % (indent, p, sv))
        for b in nested:
            r = log.schema.rules.get(b.rule, {"owner": "?", "path": "?", "elem": None, "kind": "?"})
            el = "" if b.elem == 0xFFFFFFFF else "[%d]" % b.elem
            what = "%s.%s%s" % (r["owner"], r["path"], el) if b.kind == B_NEST else "%s.%s -> elem%s" % (
                r["owner"], r["path"], el)
            out.append("%s[%s] %s (%s) @%#x %d/%d bytes %s" % (indent, phase, what, r["kind"], b.addr, b.len,
                                                              b.orig, b.fl()))
            if verbose and b.len:
                elem = r.get("elem")
                if elem and L.size(elem) and b.kind != B_NEST or (b.kind == B_NEST and r["kind"] in ("one",) and elem
                                                                  and L.size(elem)):
                    out.append("%s    %s" % (indent, L.oneline(elem, b.data()) or ""))
                elif elem and L.size(elem) and r["kind"] == "count":
                    es = L.size(elem)
                    data = b.data()
                    for j in range(min(len(data) // es, 32)):
                        out.append("%s    [%d] %s" % (indent, j, L.oneline(elem, data[j * es:(j + 1) * es])))
                else:
                    out.extend(hexdump(b.data(), 256, indent + "    "))
    return out


# ---------------------------------------------------------------- NVIDIA escape / RM

def u32s(data):
    return [struct.unpack_from("<I", data, o)[0] for o in range(0, len(data) - 3, 4)]


def parse_nvda(data):
    if not data or len(data) < 0x14:
        return None
    w = u32s(data[:0x48])
    if w[0] != NVDA_MAGIC:
        return None
    h = {"version": w[1], "size": w[2], "tag": fourcc(w[3]) or "0x%08x" % w[3], "function": w[4] if len(w) > 4 else None,
         "words": w}
    if w[1] == 0x00010002 and len(data) >= 0x44:
        h["args"] = w[12:16]
        h["status"] = w[16]
        h["payload_off"] = 0x44
    else:
        h["args"], h["status"], h["payload_off"] = [], None, None
    return h


def rm_candidates(data, start=0, limit=24):
    out = []
    for o in range(start - start % 4, len(data) - 3, 4):
        v = struct.unpack_from("<I", data, o)[0]
        if v > 0xFFFF and v in RM["ctrls"]:
            out.append((o, "ctrl", v, RM["ctrls"][v]))
        elif v > 0xFF and v in RM["classes"] and (v & 0xFF00):
            out.append((o, "class", v, RM["classes"][v]))
        if len(out) >= limit:
            break
    return out


def esc_fn_name(fn):
    if fn is None:
        return "-"
    n = RM["escapes"].get(fn)
    return "0x%x %s" % (fn, n) if n else "0x%x" % fn


# ---------------------------------------------------------------- NVIDIA push buffer (best effort)

def nv_methods(data, limit=200):
    """Decode Fermi+ push buffer method headers.  Classes bound with method 0 (SET_OBJECT) are tracked."""
    out = []
    subch_cls = {}
    i, n = 0, len(data) // 4
    w = u32s(data)
    while i < n and len(out) < limit:
        h = w[i]
        op = h >> 29
        cnt = (h >> 16) & 0x1FFF
        sc = (h >> 13) & 7
        mth = (h & 0xFFF) << 2
        if h == 0:
            i += 1
            continue
        if op not in (1, 3, 4, 5):
            out.append("  %06x  %08x  (not a method header)" % (i * 4, h))
            i += 1
            continue
        if op == 4:
            vals, cnt = [cnt], 0
        else:
            vals = w[i + 1:i + 1 + cnt]
        if mth == 0 and vals:
            subch_cls[sc] = vals[0] & 0xFFFF
        cls = subch_cls.get(sc)
        for k, v in enumerate(vals):
            m = mth + (4 * k if op == 1 else 4 if (op == 5 and k) else 0)
            name = NVMETH.get(cls, {}).get(m) if cls else None
            if name is None:
                for c2 in sorted(NVMETH, reverse=True):
                    if (c2 & 0xFF) == 0xB5 and m in NVMETH[c2] and sc == 4:
                        name = "~" + NVMETH[c2][m]
                        break
            out.append("  %06x  sc%d %s%-4x %-34s = 0x%08x" % ((i + 1 + k) * 4 if op != 4 else i * 4, sc,
                                                             "*" if op == 4 else " ", m, name or "", v))
        i += 1 + (cnt if op != 4 else 0)
    if i < n:
        out.append("  ... %d more dwords" % (n - i))
    return out


# ---------------------------------------------------------------- views

def filtered(log, a):
    for c in log.order:
        if a.grep and a.grep.lower() not in log.name(c).lower():
            continue
        if a.from_seq and c.seq < a.from_seq:
            continue
        if a.to_seq and c.seq > a.to_seq:
            continue
        yield c


def view_summary(log, L, a, out):
    h = log.hdr
    out.append("log       %s" % log.path)
    out.append("format    v%d, nvddi %d.%d.%d, WDK %s (DDI headers 0x%x)" % (
        h["version"], h["nvddi_version"] >> 24, (h["nvddi_version"] >> 16) & 0xFF, h["nvddi_version"] & 0xFFFF,
        log.schema.wdk, h["wdk_ddi_version"]))
    out.append("windows   %d.%d.%d   boot %d file %d   cpus %d x %d KB rings   level %d   max blob %d KB" % (
        h["os_major"], h["os_minor"], h["os_build"], h["boot_count"], h["file_index"], h["ncpu"],
        h["ring_bytes"] // 1024, h["level"], h["max_blob"] // 1024))
    if log.order:
        span = (max((c.ret_qpc or c.qpc) for c in log.order) - min(c.qpc for c in log.order)) / log.freq
        out.append("calls     %d over %.3f s (%d without CALL record)" % (len(log.order), span, log.ret_only))
    out.append("records   " + " ".join("%s=%d" % (TNAMES[k] if k < len(TNAMES) else k, v)
                                       for k, v in sorted(log.nrec.items())))
    if log.drops:
        out.append("DROPPED   %d records in %d gaps (ring full)" % (sum(d[2][0] for d in log.drops), len(log.drops)))
    if log.end:
        out.append("end       records=%d bytes=%d dropped(file)=%d" % log.end)
    else:
        out.append("end       (no END record: still open, or the system stopped)")
    if log.bad:
        out.append("warning   %s" % log.bad)
    out.append("")
    for (seq, qpc, r, name, reg, bl) in log.registers:
        out.append("miniport  %s  image %#x size %#x  DDI version 0x%x  %s" % (
            name, r[1], r[2], r[3], "HOOKED" if r[4] else "not hooked"))
    for (seq, qpc, t, b) in log.tables:
        tn = log.schema.tables.get(t[0], {}).get("name", t[0])
        out.append("table     %-40s %3d slots wrapped  src %#x  version %d" % (tn, t[1], t[2], t[5]))
    out.append("")
    view_stats(log, L, a, out)


def view_stats(log, L, a, out):
    st = collections.OrderedDict()
    for c in filtered(log, a):
        n = log.name(c)
        s = st.setdefault(n, [0, 0, 0.0, 0.0, collections.Counter(), c.table])
        s[0] += 1
        if is_error(log, c):
            s[1] += 1
        d = log.dur_us(c)
        if d is not None:
            s[2] += d
            s[3] = max(s[3], d)
        s[4][IRQL.get(c.irql, str(c.irql))] += 1
    out.append("%-52s %8s %6s %10s %9s  %s" % ("function", "calls", "errors", "total_us", "max_us", "irql"))
    for n, (cnt, err, tot, mx, irq, t) in sorted(st.items(), key=lambda kv: -kv[1][0]):
        out.append("%-52s %8d %6d %10.0f %9.0f  %s" % (n, cnt, err, tot, mx,
                                                       " ".join("%s:%d" % kv for kv in sorted(irq.items()))))


def view_timeline(log, L, a, out, full=False):
    n = 0
    regs = sorted(log.registry, key=lambda r: r[0]) if not a.grep else []
    ri = 0
    for c in filtered(log, a):
        while ri < len(regs) and regs[ri][0] < c.seq:
            (seq, qpc, thread, pid, tid, r, key, val, data) = regs[ri]
            name, t, v = reg_value(r, data)
            out.append("%8d %10.3f     %5d/%-5d %s[registry %s] %s%s %s %s" % (
                seq, log.ms(qpc), pid, tid, "  " * min(log.reg_depth.get(seq, 0), 8),
                REG_OPS.get(r[0], r[0]), key, ("\\" + val) if val else "", t if v else "", v))
            ri += 1
        d = log.dur_us(c)
        line = "%8d %10.3f c%-2d %s %5d/%-5d %s%s(%s) -> %s%s" % (
            c.seq, log.ms(c.qpc), c.cpu, IRQL.get(c.irql, str(c.irql)), c.pid, c.tid, "  " * min(c.depth, 8),
            log.name(c), arg_summary(log, L, c, full), ret_text(log, c), "" if d is None else "  %.1fus" % d)
        out.append(line)
        if full or a.verbose:
            if c.post_args and c.post_blobs:
                post = arg_summary(log, L, c, full, post=True)
                if post:
                    out.append("%s    after: %s" % (" " * 40, post))
            out.extend(blob_lines(log, L, c, a.verbose or full))
        n += 1
        if a.limit and n >= a.limit:
            break


PAGING_OPS = {}


def view_paging(log, L, a, out):
    ops = collections.Counter()
    n = 0
    for c in log.order:
        if log.name(c) != "DxgkDdiBuildPagingBuffer":
            continue
        b = log.arg_blob(c, 1)
        if not b or not b.len:
            continue
        data = b.data()
        d = {p: (sv, v) for p, sv, v in (L.decode("DXGKARG_BUILDPAGINGBUFFER", data) or [])}
        op = d.get("Operation", ("?", None))[0]
        ops[op] += 1
        if a.from_seq and c.seq < a.from_seq:
            continue
        n += 1
        if a.limit and n > a.limit:
            continue
        member = [(p, sv) for p, (sv, v) in d.items() if "." in p and v != 0 and sv != "0"]
        out.append("%8d %10.3f %-40s -> %s  %s" % (c.seq, log.ms(c.qpc), op, ret_text(log, c),
                                                  " ".join("%s=%s" % x for x in member[:16])))
        for r, nb in log.rule_blobs(c):
            if not r:
                continue
            data = nb.data()
            if r["path"] == "pDmaBuffer":
                out.append("      DMA written %d bytes @%#x %s" % (len(data), nb.addr, nb.fl()))
                if a.verbose:
                    out.extend(nv_methods(data, 400))
                    out.extend(hexdump(data, 512))
                else:
                    out.extend(nv_methods(data, 24))
            elif r["kind"] == "mdl":
                if len(data) >= 0x30:
                    size, flags = struct.unpack_from("<hH", data, 8)
                    startva, bc, bo = struct.unpack_from("<QII", data, 0x20)
                    pfns = [struct.unpack_from("<Q", data, o)[0] for o in range(0x30, len(data) - 7, 8)]
                    out.append("      MDL %s: StartVa %#x ByteCount %#x ByteOffset %#x pages %d first PFNs %s" % (
                        r["path"], startva, bc, bo, len(pfns), " ".join("%#x" % p for p in pfns[:8])))
            elif r["kind"] == "adl":
                pfns = [struct.unpack_from("<Q", data, o)[0] for o in range(0, len(data) - 7, 8)]
                out.append("      ADL %s: %d pages, first PFNs %s" % (r["path"], len(pfns),
                                                                      " ".join("%#x" % p for p in pfns[:8])))
            elif r["elem"] and L.size(r["elem"]):
                es = L.size(r["elem"])
                cnt = len(data) // es
                out.append("      %s: %d x %s" % (r["path"], cnt, r["elem"]))
                for j in range(min(cnt, 64 if a.verbose else 4)):
                    out.append("        [%d] %s" % (j, L.oneline(r["elem"], data[j * es:(j + 1) * es])))
            else:
                out.append("      %s: %d bytes" % (r["path"], len(data)))
                if a.verbose:
                    out.extend(hexdump(data, 256))
    out.append("")
    out.append("operations: " + ", ".join("%s %d" % kv for kv in ops.most_common()))


def view_escapes(log, L, a, out):
    groups = collections.OrderedDict()
    n = 0
    for c in log.order:
        if log.name(c) != "DxgkDdiEscape":
            continue
        pre = [b for r, b in log.rule_blobs(c, "pPrivateDriverData", post=False)]
        post = [b for r, b in log.rule_blobs(c, "pPrivateDriverData", post=True)]
        din = pre[0].data() if pre else b""
        dout = post[0].data() if post else b""
        h = parse_nvda(din)
        key = (h["tag"], h["function"], h["version"]) if h else ("non-NVDA", din[:4].hex(), 0)
        g = groups.setdefault(key, [0, 0, collections.Counter()])
        g[0] += 1
        if is_error(log, c):
            g[1] += 1
        g[2][len(din)] += 1
        if a.from_seq and c.seq < a.from_seq:
            continue
        n += 1
        if a.limit and n > a.limit:
            continue
        ab = log.arg_blob(c, 1)
        flags = L.oneline("DXGKARG_ESCAPE", ab.data()) if ab and ab.len else ""
        line = "%8d %10.3f %5d/%-5d %-6s" % (c.seq, log.ms(c.qpc), c.pid, c.tid, ret_text(log, c))
        if h:
            line += " NVDA %s fn=%s v=%#x size=%d args=%s" % (h["tag"], esc_fn_name(h["function"]), h["version"],
                                                              len(din), " ".join("%#x" % x for x in h["args"]))
            ho = parse_nvda(dout)
            if ho and ho.get("status") is not None:
                line += " nvstatus=%s" % RM["status"].get(ho["status"], "%#x" % ho["status"])
        else:
            line += " %d bytes" % len(din)
        out.append(line)
        rm = rm_candidates(din, 0x14 if h else 0)
        if rm:
            out.append("      RM: " + " ".join("+%#x %s %s" % (o, k, nm) for (o, k, v, nm) in rm[:8]))
        if a.verbose:
            out.append("      %s" % flags)
            out.append("      in:")
            out.extend(hexdump(din, 512))
            if dout and dout != din:
                out.append("      out:")
                out.extend(hexdump(dout, 512))
    out.append("")
    out.append("%-44s %7s %6s  sizes" % ("tag / function / version", "calls", "errors"))
    for (tag, fn, ver), (cnt, err, sizes) in sorted(groups.items(), key=lambda kv: -kv[1][0]):
        fs = esc_fn_name(fn) if isinstance(fn, int) else str(fn)
        out.append("%-44s %7d %6d  %s" % ("%s %s %#x" % (tag, fs, ver or 0), cnt, err,
                                          " ".join("%d:%d" % kv for kv in sizes.most_common(4))))


VIDPN_WORDS = ("VidPn", "Vidpn", "Mode", "Topology", "Monitor", "Child", "SourceAddress", "Visibility",
               "PresentPath", "Timing", "Scanline", "Pointer", "Brightness", "Display", "Edid", "Descriptor")


def view_vidpn(log, L, a, out):
    n = 0
    for c in log.order:
        nm = log.name(c)
        if c.table < 2 and not any(w in nm for w in VIDPN_WORDS):
            continue
        n += 1
        if a.limit and n > a.limit:
            break
        out.append("%8d %10.3f %s%s(%s) -> %s" % (c.seq, log.ms(c.qpc), "  " * min(c.depth, 8), nm,
                                                  arg_summary(log, L, c, a.verbose), ret_text(log, c)))
        post = arg_summary(log, L, c, a.verbose, post=True) if c.post_blobs else ""
        if post:
            out.append("%s   after: %s" % (" " * 20, post))
        if a.verbose:
            out.extend(blob_lines(log, L, c, True))


def view_events(log, L, a, out):
    rows = []
    for (seq, qpc, code, st, x, y, txt) in log.events:
        rows.append((seq, "%8d %10.3f EVENT %-14s %s%s" % (seq, log.ms(qpc), EV.get(code, code), txt,
                                                           "" if not st else " (%s)" % status_name(st))))
    for (seq, qpc, r, name, reg, bl) in log.registers:
        rows.append((seq, "%8d %10.3f REGISTER %s %s version=0x%x hooked=%d image=%#x+%#x" % (
            seq, log.ms(qpc), name, reg, r[3], r[4], r[1], r[2])))
    base = {}
    for (seq, qpc, r, name, reg, bl) in log.registers:
        if r[4]:
            base = (r[1], r[2])
    for (seq, qpc, t, b) in log.tables:
        tn = log.schema.tables.get(t[0], {}).get("name", t[0])
        rows.append((seq, "%8d %10.3f TABLE %s src=%#x copy=%#x size=%d wrapped=%d version=%d" % (
            seq, log.ms(qpc), tn, t[2], t[3], t[4], t[1], t[5])))
        if a.verbose:
            raw = b.data()
            for (ti, si), sl in sorted(log.schema.slots.items()):
                if ti != t[0] or sl["off"] + 8 > len(raw):
                    continue
                fn = struct.unpack_from("<Q", raw, sl["off"])[0]
                if not fn:
                    continue
                where = ""
                if base and base[0] <= fn < base[0] + base[1]:
                    where = "  image+%#x" % (fn - base[0])
                rows.append((seq, "%8s %10s     %-56s %#x%s" % ("", "", sl["name"], fn, where)))
    for (seq, qpc, i) in log.irps:
        rows.append((seq, "%8d %10.3f IRP %s ioctl=%#x in=%d out=%d mode=%d status=%s" % (
            seq, log.ms(qpc), IRP_MJ.get(i[0], i[0]), i[4], i[5], i[6], i[2], status_name(i[7]))))
    for (seq, qpc, d) in log.drops:
        rows.append((seq, "%8d %10.3f DROP cpu %d: %d records, %d bytes" % (seq, log.ms(qpc), d[2], d[0], d[1])))
    for _, line in sorted(rows):
        out.append(line)


def reg_value(r, data):
    """(type, value text) from KEY_VALUE_{BASIC,FULL,PARTIAL}_INFORMATION or raw SetValue data."""
    op, info = r[0], r[3]
    typ = r[2]
    name = None
    raw = data
    if op != 1 and len(data) >= 12:
        if info == 2:                                  # KeyValuePartialInformation
            _, typ, ln = struct.unpack_from("<III", data, 0)
            raw = data[12:12 + ln]
        elif info == 1 and len(data) >= 20:            # KeyValueFullInformation
            _, typ, doff, dlen, nlen = struct.unpack_from("<IIIII", data, 0)
            name = data[20:20 + nlen].decode("utf-16-le", "replace")
            raw = data[doff:doff + dlen]
        elif info == 0 and len(data) >= 12:            # KeyValueBasicInformation
            _, typ, nlen = struct.unpack_from("<III", data, 0)
            name = data[12:12 + nlen].decode("utf-16-le", "replace")
            raw = b""
    t = REG_TYPES.get(typ, str(typ))
    if typ == 4 and len(raw) >= 4:
        v = "0x%x" % struct.unpack_from("<I", raw)[0]
    elif typ == 11 and len(raw) >= 8:
        v = "0x%x" % struct.unpack_from("<Q", raw)[0]
    elif typ in (1, 2, 7):
        v = repr(raw.decode("utf-16-le", "replace").rstrip("\0").replace("\0", " | "))
    elif raw:
        v = raw[:48].hex() + ("..." if len(raw) > 48 else "") + " (%d bytes)" % len(raw)
    else:
        v = ""
    return name, t, v


def view_registry(log, L, a, out):
    n = 0
    for (seq, qpc, thread, pid, tid, r, key, val, data) in log.registry:
        name, t, v = reg_value(r, data)
        op = REG_OPS.get(r[0], str(r[0]))
        st = "" if r[1] == 0x103 else status_name(r[1])
        out.append("%8d %10.3f %5d/%-5d %-18s %-6s %s%s%s %s" % (
            seq, log.ms(qpc), pid, tid, op, st, key, ("\\" + val) if val else "",
            ("\\" + name) if name else "", ("%s %s" % (t, v)) if (v or r[0] in (1, 21, 23)) else ""))
        if r[9] & 2:
            out[-1] += "  [TRUNCATED %d of %d bytes]" % (r[7], r[8])
        n += 1
        if a.limit and n >= a.limit:
            break


def view_schema(log, L, a, out):
    s = log.schema
    for tid, t in sorted(s.tables.items()):
        out.append("table %d %s (%d slots, %d bytes)" % (tid, t["name"], t["nslots"], t["size"]))
        for (ti, si), sl in sorted(s.slots.items()):
            if ti != tid:
                continue
            out.append("  %3d +%-4d %-48s %-8s %s(%s)" % (si, sl["off"], sl["name"], sl["irql"], sl["ret"], ", ".join(
                "%s %s:%s%s" % (x["type"], x["name"], x["kind"], ("<" + x["rec"] + ">") if x["rec"] else "")
                for x in sl["args"])))
    out.append("rules:")
    for rid, r in sorted(s.rules.items()):
        out.append("  %3d %s.%s %s when=%d elem=%s len=%s cond=%s" % (rid, r["owner"], r["path"], r["kind"], r["when"],
                                                                     r["elem"], r["len"], r["cond"]))


def dump_blobs(log, a):
    os.makedirs(a.dump_blobs, exist_ok=True)
    n = 0
    for c in filtered(log, a):
        nm = log.name(c)
        for phase, args, nested in (("pre", c.blobs, c.nested), ("post", c.post_blobs, c.post_nested)):
            for b in args + nested:
                if not b.len:
                    continue
                r = log.schema.rules.get(b.rule) if b.rule != 0xFFFF else None
                tag = ("arg%d" % b.arg) if r is None else r["path"].replace(".", "_")
                if b.elem != 0xFFFFFFFF:
                    tag += "_%d" % b.elem
                fn = os.path.join(a.dump_blobs, "%08d_%s_%s_%s.bin" % (c.seq, nm, phase, tag))
                with open(fn, "wb") as f:
                    f.write(b.data())
                n += 1
    print("wrote %d blobs to %s" % (n, a.dump_blobs))


def main(argv=None):
    ap = argparse.ArgumentParser(description="decode nvddi .ndl logs")
    ap.add_argument("log")
    for v in ("summary", "timeline", "calls", "paging", "escapes", "vidpn", "events", "stats", "schema", "registry"):
        ap.add_argument("--" + v, action="store_true")
    ap.add_argument("--dump-blobs", metavar="DIR")
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--grep")
    ap.add_argument("--from", dest="from_seq", type=int, default=0)
    ap.add_argument("--to", dest="to_seq", type=int, default=0)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--layouts", help="ndl_layouts.json (default: next to this script)")
    a = ap.parse_args(argv)
    log = Log(a.log)
    L = Layouts(a.layouts)
    out = []
    if a.dump_blobs:
        dump_blobs(log, a)
        return 0
    if a.timeline:
        view_timeline(log, L, a, out)
    elif a.calls:
        view_timeline(log, L, a, out, full=True)
    elif a.paging:
        view_paging(log, L, a, out)
    elif a.escapes:
        view_escapes(log, L, a, out)
    elif a.vidpn:
        view_vidpn(log, L, a, out)
    elif a.events:
        view_events(log, L, a, out)
    elif a.stats:
        view_stats(log, L, a, out)
    elif a.registry:
        view_registry(log, L, a, out)
    elif a.schema:
        view_schema(log, L, a, out)
    else:
        view_summary(log, L, a, out)
    try:
        sys.stdout.write("\n".join(out) + "\n")
    except BrokenPipeError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())

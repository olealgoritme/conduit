#!/usr/bin/env python3
"""Decode nvtrace logs (.ntr).

    ntrdump.py LOG [--summary]            process, modules, hooks, per-function and per-caller counts
    ntrdump.py LOG --calls [-v] [--grep S] every call: time, thread, function, status, caller, fields
    ntrdump.py LOG --escapes [-v]         D3DKMT escapes grouped by NVIDIA header/tag/function, RM decode
    ntrdump.py LOG --rm [-v]              RM view: NVDA escapes and device IOCTLs in order, RM names
    ntrdump.py LOG --objects              lifetimes of adapters/devices/contexts/queues/allocations/fences/VA
    ntrdump.py --diff A B                 compare two logs

Python 3 standard library only. Needs nvt_layouts.py and rmnames.json next
to this file. The log format is described in README.md and src/ntr_format.h.
"""

import argparse
import collections
import json
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import nvt_layouts as L  # noqa: E402

REC_PROCINFO, REC_STRING, REC_HOOK, REC_MODULE, REC_MODULE_UNLOAD, REC_CALL, REC_NOTE, REC_END = range(1, 9)
BLOB_TRUNCATED, BLOB_FAULT, BLOB_SAME, BLOB_CHASED = 1, 2, 4, 8
HOOK_OK, HOOK_LAYOUT, HOOK_RAW, HOOK_SYSCALL = 1, 2, 4, 8
NVDA_MAGIC = 0x4E564441
SYSTEM_MODULES = {"win32u.dll", "gdi32.dll", "gdi32full.dll", "kernelbase.dll", "kernel32.dll",
                  "ntdll.dll", "nvtrace64.dll", "nvtrace32.dll", "wow64.dll", "wow64win.dll"}

NTSTATUS = {
    0x00000000: "SUCCESS", 0x00000102: "TIMEOUT", 0x00000103: "PENDING",
    0x80000005: "BUFFER_OVERFLOW", 0xC0000001: "UNSUCCESSFUL", 0xC0000002: "NOT_IMPLEMENTED",
    0xC0000008: "INVALID_HANDLE", 0xC000000D: "INVALID_PARAMETER", 0xC0000010: "INVALID_DEVICE_REQUEST",
    0xC0000017: "NO_MEMORY", 0xC0000022: "ACCESS_DENIED", 0xC0000023: "BUFFER_TOO_SMALL",
    0xC0000034: "OBJECT_NAME_NOT_FOUND", 0xC000003A: "OBJECT_PATH_NOT_FOUND",
    0xC00000BB: "NOT_SUPPORTED", 0xC000009A: "INSUFFICIENT_RESOURCES", 0xC0000120: "CANCELLED",
    0xC00000E5: "INTERNAL_ERROR", 0xC000000E: "NO_SUCH_DEVICE", 0xC0000184: "INVALID_DEVICE_STATE",
    0xC01E0001: "GRAPHICS_NOT_EXCLUSIVE_MODE_OWNER", 0xC01E0003: "GRAPHICS_ADAPTER_WAS_RESET",
    0xC01E0005: "GRAPHICS_PRESENT_OCCLUDED", 0xC01E0006: "GRAPHICS_PRESENT_DENIED",
    0xC01E0100: "GRAPHICS_NO_VIDEO_MEMORY", 0xC01E0107: "GRAPHICS_ALLOCATION_BUSY",
    0xC01E0110: "GRAPHICS_INVALID_ALLOCATION_HANDLE", 0x401E0201: "GRAPHICS_PARTIAL_DATA_POPULATED",
}

KMTQAITYPE = [
    "UMDRIVERPRIVATE", "UMDRIVERNAME", "UMOPENGLINFO", "GETSEGMENTSIZE", "ADAPTERGUID",
    "FLIPQUEUEINFO", "ADAPTERADDRESS", "SETWORKINGSETINFO", "ADAPTERREGISTRYINFO",
    "CURRENTDISPLAYMODE", "MODELIST", "CHECKDRIVERUPDATESTATUS", "VIRTUALADDRESSINFO",
    "DRIVERVERSION", "14", "ADAPTERTYPE", "OUTPUTDUPLCONTEXTSCOUNT", "WDDM_1_2_CAPS",
    "UMD_DRIVER_VERSION", "DIRECTFLIP_SUPPORT", "MULTIPLANEOVERLAY_SUPPORT", "DLIST_DRIVER_NAME",
    "WDDM_1_3_CAPS", "MULTIPLANEOVERLAY_HUD_SUPPORT", "WDDM_2_0_CAPS", "NODEMETADATA",
    "CPDRIVERNAME", "XBOX", "INDEPENDENTFLIP_SUPPORT", "MIRACASTCOMPANIONDRIVERNAME",
    "PHYSICALADAPTERCOUNT", "PHYSICALADAPTERDEVICEIDS", "DRIVERCAPS_EXT",
    "QUERY_MIRACAST_DRIVER_TYPE", "QUERY_GPUMMU_CAPS", "QUERY_MULTIPLANEOVERLAY_DECODE_SUPPORT",
    "QUERY_HW_PROTECTION_TEARDOWN_COUNT", "QUERY_ISBADDRIVERFORHWPROTECTIONDISABLED",
    "MULTIPLANEOVERLAY_SECONDARY_SUPPORT", "INDEPENDENTFLIP_SECONDARY_SUPPORT",
    "PANELFITTER_SUPPORT", "PHYSICALADAPTERPNPKEY", "GETSEGMENTGROUPSIZE", "MPO3DDI_SUPPORT",
    "HWDRM_SUPPORT", "MPOKERNELCAPS_SUPPORT", "MULTIPLANEOVERLAY_STRETCH_SUPPORT",
    "GET_DEVICE_VIDPN_OWNERSHIP_INFO", "QUERYREGISTRY", "KMD_DRIVER_VERSION", "BLOCKLIST_KERNEL",
    "BLOCKLIST_RUNTIME", "ADAPTERGUID_RENDER", "ADAPTERADDRESS_RENDER",
    "ADAPTERREGISTRYINFO_RENDER", "CHECKDRIVERUPDATESTATUS_RENDER", "DRIVERVERSION_RENDER",
    "ADAPTERTYPE_RENDER", "WDDM_1_2_CAPS_RENDER", "WDDM_1_3_CAPS_RENDER",
    "QUERY_ADAPTER_UNIQUE_GUID", "NODEPERFDATA", "ADAPTERPERFDATA", "ADAPTERPERFDATA_CAPS",
    "GPUVERSION", "DRIVER_DESCRIPTION", "DRIVER_DESCRIPTION_RENDER", "SCANOUT_CAPS",
    "PARAVIRTUALIZATION_RENDER", "SERVICENAME", "WDDM_2_7_CAPS", "DISPLAY_UMDRIVERNAME",
    "TRACKEDWORKLOAD_SUPPORT", "HYBRID_DLIST_DLL_SUPPORT", "DISPLAY_CAPS", "WDDM_2_9_CAPS",
    "CROSSADAPTERRESOURCE_SUPPORT", "WDDM_3_0_CAPS", "WSAUMDIMAGENAME", "VGPUINTERFACEID",
    "WDDM_3_1_CAPS",
]


def load_rmnames():
    p = os.path.join(HERE, "rmnames.json")
    if not os.path.exists(p):
        return {"classes": {}, "ctrls": {}, "escapes": {}, "status": {}}
    with open(p) as f:
        d = json.load(f)
    return {k: {int(n): v for n, v in d.get(k, {}).items()} for k in
            ("classes", "ctrls", "escapes", "status")}


RM = load_rmnames()


def status_name(v, ptr):
    v &= 0xFFFFFFFF
    n = NTSTATUS.get(v)
    return ("0x%08x %s" % (v, n)) if n else "0x%08x" % v


def fourcc(v):
    b = struct.pack(">I", v & 0xFFFFFFFF)
    if all(32 <= c < 127 for c in b):
        return b.decode("ascii")
    return None


# ------------------------------------------------------------------ parsing

class Blob:
    __slots__ = ("label", "index", "addr", "req", "cap", "phase", "flags", "parent", "data")


class Call:
    __slots__ = ("seq", "tid", "qpc", "qpc_end", "hook", "flags", "ret", "args", "frames",
                 "depth", "blobs")

    def blob(self, label, phase, index=0xFFFFFFFF):
        """Data of a blob; for an unchanged after-copy, the before data."""
        for b in self.blobs:
            if b.label == label and b.phase == phase and (index is None or b.index == index):
                if b.flags & BLOB_SAME:
                    return self.blob(label, 0, b.index)
                return b.data
        return None


class Hook:
    __slots__ = ("id", "flags", "nr", "nargs", "addr", "arg0size", "status", "module", "name")


class Module:
    __slots__ = ("base", "size", "version", "timestamp", "flags", "path", "name", "qpc")


class Log:
    def __init__(self, path):
        self.path = path
        self.procinfo = {}
        self.strings = {}
        self.hooks = {}
        self.modules = []
        self.unloads = []
        self.calls = []
        self.notes = []
        self.end = None
        self.bad = None
        with open(path, "rb") as f:
            data = f.read()
        self._parse(data)
        self.P = self.ptr_size
        self._mods_sorted = sorted({(m.base, m.size, m.name): m for m in self.modules}.values(),
                                   key=lambda m: m.base)

    def _parse(self, d):
        if len(d) < 32 or d[:8] != b"NVTRACE\0":
            raise ValueError("%s: not an nvtrace log" % self.path)
        (self.version, hsize, self.ptr_size, self.pid, self.qpc_freq) = struct.unpack_from("<IIIIQ", d, 8)
        if self.version != 1:
            raise ValueError("%s: unsupported log version %d" % (self.path, self.version))
        off = hsize
        n = len(d)
        seen = set()
        self.duplicates = 0
        while off + 24 <= n:
            typ, size, qpc, tid, seq = struct.unpack_from("<IIQII", d, off)
            if size < 24 or off + size > n:
                self.bad = "truncated record at offset %d" % off
                break
            if seq in seen:
                # a chunk written twice at process exit (see src/log.c)
                self.duplicates += 1
                off += size
                continue
            seen.add(seq)
            body = d[off + 24:off + size]
            try:
                self._record(typ, qpc, tid, seq, body)
            except (struct.error, IndexError, UnicodeDecodeError) as e:
                self.bad = "bad record type %d at offset %d: %s" % (typ, off, e)
            off += size
        if off < n and not self.bad:
            self.bad = "truncated record at offset %d (%d trailing bytes)" % (off, n - off)
        self.start_qpc = int(self.procinfo.get("start_qpc", "0") or 0)
        if not self.start_qpc and self.calls:
            self.start_qpc = min(c.qpc for c in self.calls)

    @staticmethod
    def _cstr(b, off=0):
        e = b.find(b"\0", off)
        if e < 0:
            e = len(b)
        return b[off:e].decode("utf-8", "replace"), e + 1

    def _record(self, typ, qpc, tid, seq, body):
        if typ == REC_PROCINFO:
            off = 0
            while off < len(body):
                s, off = self._cstr(body, off)
                if not s:
                    break
                k, _, v = s.partition("=")
                self.procinfo[k] = v
        elif typ == REC_STRING:
            (i,) = struct.unpack_from("<I", body, 0)
            self.strings[i], _ = self._cstr(body, 4)
        elif typ == REC_HOOK:
            h = Hook()
            (h.id, h.flags, h.nr, h.nargs, h.addr, h.arg0size, h.status) = struct.unpack_from("<IIIIQII", body, 0)
            h.module, o = self._cstr(body, 32)
            h.name, _ = self._cstr(body, o)
            self.hooks[h.id] = h
        elif typ == REC_MODULE:
            m = Module()
            m.base, m.size = struct.unpack_from("<QQ", body, 0)
            m.version = struct.unpack_from("<4H", body, 16)
            m.timestamp, m.flags = struct.unpack_from("<II", body, 24)
            m.path, _ = self._cstr(body, 32)
            m.name = m.path.replace("/", "\\").split("\\")[-1].lower()
            m.qpc = qpc
            self.modules.append(m)
        elif typ == REC_MODULE_UNLOAD:
            (base,) = struct.unpack_from("<Q", body, 0)
            p, _ = self._cstr(body, 8)
            self.unloads.append((qpc, base, p))
        elif typ == REC_CALL:
            c = Call()
            c.seq, c.tid, c.qpc = seq, tid, qpc
            (c.hook, c.flags, c.qpc_end, c.ret, na, nf, c.depth) = struct.unpack_from("<IIQQHHI", body, 0)
            off = 32
            c.args = list(struct.unpack_from("<%dQ" % na, body, off))
            off += 8 * na
            c.frames = list(struct.unpack_from("<%dQ" % nf, body, off))
            off += 8 * nf
            c.blobs = []
            while off + 32 <= len(body):
                b = Blob()
                (b.label, b.index, b.addr, b.req, b.cap, b.phase, b.flags, b.parent) = \
                    struct.unpack_from("<IIQIIBBH", body, off)
                off += 32
                b.data = body[off:off + b.cap]
                off += (b.cap + 7) & ~7
                c.blobs.append(b)
            self.calls.append(c)
        elif typ == REC_NOTE:
            lvl, _ = struct.unpack_from("<II", body, 0)
            s, _ = self._cstr(body, 8)
            self.notes.append((qpc, lvl, s))
        elif typ == REC_END:
            self.end = dict(zip(("records", "calls", "dropped", "bytes"), struct.unpack_from("<4Q", body, 0)))

    # -------------------------------------------------------------- helpers
    def ms(self, qpc):
        if not self.qpc_freq:
            return 0.0
        return (qpc - self.start_qpc) * 1000.0 / self.qpc_freq

    def dur_us(self, c):
        return (c.qpc_end - c.qpc) * 1e6 / self.qpc_freq if self.qpc_freq else 0.0

    def fname(self, c):
        h = self.hooks.get(c.hook)
        return h.name if h else "hook#%d" % c.hook

    def short(self, c):
        n = self.fname(c)
        return n[10:] if n.startswith("NtGdiDdDDI") else n

    def module_of(self, addr):
        lo, hi = 0, len(self._mods_sorted)
        best = None
        while lo < hi:
            mid = (lo + hi) // 2
            if self._mods_sorted[mid].base <= addr:
                best = self._mods_sorted[mid]
                lo = mid + 1
            else:
                hi = mid
        if best and addr < best.base + best.size:
            return best
        return None

    def where(self, addr):
        m = self.module_of(addr)
        return "%s+0x%x" % (m.name, addr - m.base) if m else "0x%x" % addr

    def caller(self, c):
        """First frame outside the system thunk modules, as module name."""
        for a in c.frames:
            m = self.module_of(a)
            if m is None:
                return "?"
            if m.name not in SYSTEM_MODULES:
                return m.name
        return self.module_of(c.frames[0]).name if c.frames and self.module_of(c.frames[0]) else "?"

    def caller_frame(self, c):
        """Address of the first frame outside the system thunk modules."""
        for a in c.frames:
            m = self.module_of(a)
            if m is None or m.name not in SYSTEM_MODULES:
                return a
        return c.frames[0] if c.frames else 0

    def nv_caller(self, c):
        for a in c.frames:
            m = self.module_of(a)
            if m and m.name.startswith("nv") and not m.name.startswith("nvtrace"):
                return m.name
        return None

    def label(self, i):
        return self.strings.get(i, "#%d" % i)


# ------------------------------------------------------------------ struct decode

def decode_struct(st, data, P):
    """{field: value} for the fields of st that fit in data."""
    out = collections.OrderedDict()
    if isinstance(st, L.Raw):
        return out
    size, _, fields = st.layout(P)
    for (name, off, t) in fields:
        if name in out:
            continue
        out[name] = decode_value(t, data, off, P)
    return out


def decode_value(t, data, off, P):
    if isinstance(t, L.Ptr) or (isinstance(t, L.Prim) and t.ptr):
        w = P
    elif isinstance(t, L.Prim):
        w = t.size(P)
    elif isinstance(t, L.Arr):
        n = t.n
        es = t.t.size(P)
        if isinstance(t.t, L.Prim) and t.t.name == "wchar":
            raw = data[off:off + n * 2]
            return raw.decode("utf-16-le", "replace").split("\0")[0] if len(raw) == n * 2 else None
        vals = []
        for i in range(n):
            v = decode_value(t.t, data, off + i * es, P)
            if v is None:
                break
            vals.append(v)
        return vals
    elif isinstance(t, L.Struct):
        if off + t.size(P) > len(data):
            return None
        return decode_struct(t, data[off:off + t.size(P)], P)
    else:
        return None
    if off + w > len(data):
        return None
    v = int.from_bytes(data[off:off + w], "little")
    if isinstance(t, L.Prim) and t.name == "i32" and v >= 1 << 31:
        v -= 1 << 32
    return v


def fmt_value(v):
    if isinstance(v, dict):
        return "{" + " ".join("%s=%s" % (k, fmt_value(x)) for k, x in v.items() if not is_zero(x)) + "}"
    if isinstance(v, list):
        nz = [x for x in v if not is_zero(x)]
        if not nz:
            return "[]"
        if all(isinstance(x, int) for x in v):
            last = max(i for i, x in enumerate(v) if not is_zero(x))
            return "[" + ",".join(fmt_value(x) for x in v[:last + 1]) + "]"
        return "[" + ",".join(fmt_value(x) for x in v) + "]"
    if isinstance(v, int):
        return "%d" % v if -10 < v < 10 else "0x%x" % v
    return repr(v)


def is_zero(v):
    if v is None:
        return True
    if isinstance(v, int):
        return v == 0
    if isinstance(v, (list, dict)):
        vals = v.values() if isinstance(v, dict) else v
        return all(is_zero(x) for x in vals)
    return False


def fmt_fields(d, skip_zero=True, skip=("Reserved",)):
    parts = []
    for k, v in d.items():
        if k.startswith(skip) or (skip_zero and is_zero(v)):
            continue
        parts.append("%s=%s" % (k, fmt_value(v)))
    return " ".join(parts)


def resolve_label(fn, label):
    """Type of the memory a blob with this label points to: (type, is_array)."""
    if fn is None:
        return None, False
    parts = label.split(".")
    t = None
    is_arr = False
    for i, p in enumerate(parts):
        arr = p.endswith("[]")
        p = p[:-2] if arr else p
        if i == 0:
            ptr = dict(fn.args).get(p)
        else:
            if not isinstance(t, L.Struct):
                return None, False
            try:
                _, ptr = t.field(8, p)
            except KeyError:
                return None, False
        if not isinstance(ptr, L.Ptr):
            return None, False
        if ptr.target is not None:
            t, is_arr = ptr.target, False
        elif ptr.elem is not None:
            t, is_arr = ptr.elem, True
        else:
            t, is_arr = None, False
    return t, is_arr


def call_struct(log, c, phase=1):
    """Decoded arg0 struct (after the call if available)."""
    fn = L.FUNC_BY_NAME.get(log.fname(c))
    st = L.arg0_struct(fn) if fn else None
    if st is None:
        return None
    lbl = label_id(log, "pData")
    data = c.blob(lbl, phase) if lbl is not None else None
    if data is None and phase == 1:
        data = c.blob(lbl, 0)
    if data is None:
        return None
    return decode_struct(st, data, log.P)


_label_cache = {}


def label_id(log, text):
    key = (id(log), text)
    if key not in _label_cache:
        _label_cache[key] = next((i for i, s in log.strings.items() if s == text), None)
    return _label_cache[key]


def hexdump(data, base=0, limit=256, indent="      "):
    out = []
    for o in range(0, min(len(data), limit), 16):
        chunk = data[o:o + 16]
        hx = " ".join("%02x" % b for b in chunk)
        asc = "".join(chr(b) if 32 <= b < 127 else "." for b in chunk)
        out.append("%s%06x  %-47s  %s" % (indent, base + o, hx, asc))
    if len(data) > limit:
        out.append("%s... %d more bytes" % (indent, len(data) - limit))
    return "\n".join(out)


def diff_ranges(a, b):
    """[(start, end)] byte ranges that differ between a and b."""
    n = max(len(a), len(b))
    out = []
    start = None
    for i in range(n):
        same = i < len(a) and i < len(b) and a[i] == b[i]
        if not same and start is None:
            start = i
        elif same and start is not None:
            out.append((start, i))
            start = None
    if start is not None:
        out.append((start, n))
    return out


# ------------------------------------------------------------------ NVDA escapes

def u32s(data):
    return [struct.unpack_from("<I", data, o)[0] for o in range(0, len(data) - 3, 4)]


def parse_nvda(data):
    """NVIDIA private escape header, or None."""
    if not data or len(data) < 0x14:
        return None
    w = u32s(data[:0x48])
    if w[0] != NVDA_MAGIC:
        return None
    h = {"magic": w[0], "version": w[1], "size": w[2], "tag": fourcc(w[3]) or "0x%08x" % w[3],
         "tagv": w[3], "function": w[4] if len(w) > 4 else None, "words": w}
    if w[1] == 0x00010002 and len(data) >= 0x44:
        h["args"] = w[12:16]
        h["status"] = w[16]
        h["payload_off"] = 0x44
    else:
        h["args"] = []
        h["status"] = None
        h["payload_off"] = None
    return h


def rm_candidates(data, start=0, limit=64):
    """u32 values in data that are known RM control commands or classes."""
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


def escape_function_name(fn):
    if fn is None:
        return "-"
    n = RM["escapes"].get(fn)
    return "0x%x %s" % (fn, n) if n else "0x%x" % fn


def escape_calls(log):
    lbl = label_id(log, "pData.pPrivateDriverData")
    for c in log.calls:
        if log.fname(c) != "NtGdiDdDDIEscape":
            continue
        before = c.blob(lbl, 0) if lbl is not None else None
        after = c.blob(lbl, 1) if lbl is not None else None
        yield c, before, after


# ------------------------------------------------------------------ views

def view_summary(log, out):
    pi = log.procinfo
    out.append("log      %s (format v%d, %d-bit, pid %d)" % (log.path, log.version, log.P * 8, log.pid))
    out.append("process  %s" % pi.get("exe", "?"))
    out.append("cmdline  %s" % pi.get("cmdline", ""))
    out.append("os       %s%s  wow64=%s  nvtrace %s" % (
        pi.get("os", "?"), ("." + pi["os_ubr"]) if "os_ubr" in pi else "", pi.get("wow64", "?"),
        pi.get("nvtrace", "?")))
    cfg = " ".join("%s=%s" % (k[7:], v) for k, v in pi.items() if k.startswith("config."))
    out.append("config   %s" % cfg)
    if log.calls:
        span = (max(c.qpc_end for c in log.calls) - min(c.qpc for c in log.calls)) / max(log.qpc_freq, 1)
        out.append("calls    %d over %.3f s, %d threads" % (
            len(log.calls), span, len({c.tid for c in log.calls})))
    if log.end:
        out.append("end      records=%d calls=%d dropped=%d bytes=%d" % (
            log.end["records"], log.end["calls"], log.end["dropped"], log.end["bytes"]))
    else:
        out.append("end      (no END record: process killed or log cut)")
    if log.bad:
        out.append("warning  %s" % log.bad)
    if log.duplicates:
        out.append("note     %d duplicate records skipped" % log.duplicates)

    nv = {}
    for m in log.modules:
        if m.name.startswith("nv") and not m.name.startswith("nvtrace"):
            nv[(m.name, m.base)] = m
    out.append("")
    out.append("NVIDIA modules (%d):" % len(nv))
    for (name, base), m in sorted(nv.items(), key=lambda kv: kv[1].base):
        out.append("  %-24s %-18s base 0x%x size 0x%x  %s" % (
            name, ".".join(str(x) for x in m.version), m.base, m.size, m.path))

    ok = [h for h in log.hooks.values() if h.flags & HOOK_OK]
    raw = [h for h in ok if h.flags & HOOK_RAW]
    failed = [h for h in log.hooks.values() if not h.flags & HOOK_OK]
    out.append("")
    out.append("hooks    %d installed (%d known layouts, %d raw), %d not installed" % (
        len(ok), len(ok) - len(raw), len(raw), len(failed)))
    for h in failed:
        out.append("  not hooked: %s!%s status %d" % (h.module, h.name, h.status))

    stats = collections.OrderedDict()
    for c in log.calls:
        s = stats.setdefault(log.fname(c), [0, 0, 0.0, collections.Counter()])
        s[0] += 1
        if (c.ret & 0xFFFFFFFF) >= 0x80000000 and log.fname(c) != "CreateProcessInternalW":
            s[1] += 1
        s[2] += log.dur_us(c)
        s[3][log.caller(c)] += 1
    out.append("")
    out.append("%-52s %8s %6s %10s  %s" % ("function", "calls", "errors", "total_us", "callers"))
    for name, (n, err, us, callers) in sorted(stats.items(), key=lambda kv: -kv[1][0]):
        cs = ", ".join("%s %d" % (k, v) for k, v in callers.most_common(4))
        out.append("%-52s %8d %6d %10.0f  %s" % (name, n, err, us, cs))

    per_caller = collections.Counter(log.caller(c) for c in log.calls)
    out.append("")
    out.append("by caller module:")
    for k, v in per_caller.most_common():
        fns = collections.Counter(log.short(c) for c in log.calls if log.caller(c) == k)
        out.append("  %-24s %8d  %s" % (k, v, ", ".join("%s %d" % x for x in fns.most_common(6))))

    qai = collections.Counter()
    for c in log.calls:
        if log.fname(c) == "NtGdiDdDDIQueryAdapterInfo":
            d = call_struct(log, c, 0)
            if d:
                t = d.get("Type", -1)
                qai[(KMTQAITYPE[t] if 0 <= t < len(KMTQAITYPE) else str(t), d.get("PrivateDriverDataSize"))] += 1
    if qai:
        out.append("")
        out.append("QueryAdapterInfo types:")
        for (t, sz), n in sorted(qai.items()):
            out.append("  %-40s size %-6s %d" % (t, sz, n))

    devs = [c for c in log.calls if log.fname(c) in ("NtCreateFile", "NtOpenFile")]
    if devs:
        out.append("")
        out.append("device opens:")
        lbl = label_id(log, "ObjectAttributes.ObjectName.Buffer")
        for c in devs:
            name = c.blob(lbl, 0) if lbl is not None else None
            out.append("  %10.3f ms  %s -> %s" % (
                log.ms(c.qpc), name.decode("utf-16-le", "replace") if name else "?",
                status_name(c.ret, log.P)))
    if log.notes:
        out.append("")
        out.append("notes:")
        for (q, lvl, s) in log.notes:
            out.append("  [%s] %s" % (("info", "warn", "error")[min(lvl, 2)], s))


def call_line(log, c):
    fn = L.FUNC_BY_NAME.get(log.fname(c))
    extra = ""
    if fn is not None and L.arg0_struct(fn) is not None:
        d = call_struct(log, c, 1)
        if d:
            if log.fname(c) == "NtGdiDdDDIQueryAdapterInfo" and "Type" in d:
                t = d["Type"]
                d["Type"] = KMTQAITYPE[t] if 0 <= t < len(KMTQAITYPE) else t
            extra = fmt_fields(d)
    elif fn is not None:
        names = [a[0] for a in fn.args]
        extra = " ".join("%s=0x%x" % (n, v) for n, v in zip(names, c.args) if v)
    else:
        extra = " ".join("0x%x" % a for a in c.args[:4])
    if log.fname(c) == "NtGdiDdDDIEscape":
        before = c.blob(label_id(log, "pData.pPrivateDriverData"), 0)
        h = parse_nvda(before)
        if h:
            extra += "  [NVDA %s fn=%s args=%s]" % (h["tag"], escape_function_name(h["function"]),
                                                   ",".join("0x%x" % a for a in h["args"]))
    return "%6d %10.3f %6d %-40s %-28s %8.1fus %-26s %s" % (
        c.seq, log.ms(c.qpc), c.tid, log.short(c)[:40], status_name(c.ret, log.P)[:28],
        log.dur_us(c), log.caller(c)[:26], extra)


def view_calls(log, out, verbose, grep):
    out.append("%6s %10s %6s %-40s %-28s %10s %-26s %s" % (
        "seq", "ms", "tid", "function", "status", "time", "caller", "fields"))
    for c in log.calls:
        if grep and grep.lower() not in log.fname(c).lower():
            continue
        out.append(call_line(log, c))
        if verbose:
            out.append("      frames: " + " ".join(log.where(a) for a in c.frames))
            fn = L.FUNC_BY_NAME.get(log.fname(c))
            for i, b in enumerate(c.blobs):
                lbl = log.label(b.label)
                flags = ",".join(n for bit, n in ((1, "trunc"), (2, "fault"), (4, "same"), (8, "chased"))
                                 if b.flags & bit)
                idx = "" if b.index == 0xFFFFFFFF else "[%d]" % b.index
                out.append("    #%d %s %s%s @0x%x req=%d got=%d%s%s" % (
                    i + 1, ("before", "after")[b.phase], lbl, idx, b.addr, b.req, b.cap,
                    (" " + flags) if flags else "", (" parent=#%d" % b.parent) if b.parent else ""))
                if b.flags & BLOB_SAME:
                    continue
                t, is_arr = resolve_label(fn, lbl)
                if isinstance(t, L.Struct) and not is_arr:
                    out.append("      " + fmt_fields(decode_struct(t, b.data, log.P)))
                elif isinstance(t, L.Struct) and is_arr and t.size(log.P):
                    es = t.size(log.P)
                    for e in range(min(len(b.data) // es, 16)):
                        out.append("      [%d] %s" % (e, fmt_fields(decode_struct(t, b.data[e * es:(e + 1) * es], log.P))))
                else:
                    out.append(hexdump(b.data))


def view_escapes(log, out, verbose):
    groups = collections.OrderedDict()
    for c, before, after in escape_calls(log):
        h = parse_nvda(before)
        if h:
            key = ("NVDA", h["tag"], h["function"], h["version"])
        else:
            key = ("raw", fourcc(u32s(before)[0]) if before and len(before) >= 4 else "-", None, None)
        g = groups.setdefault(key, {"calls": [], "sizes": collections.Counter(),
                                    "callers": collections.Counter(), "status": collections.Counter()})
        g["calls"].append((c, before, after, h))
        g["sizes"][len(before) if before else 0] += 1
        g["callers"][log.nv_caller(c) or log.caller(c)] += 1
        ha = parse_nvda(after) if after else None
        if ha and ha["status"] is not None:
            g["status"][ha["status"]] += 1
    total = sum(len(g["calls"]) for g in groups.values())
    out.append("%d escapes in %d groups" % (total, len(groups)))
    for key, g in groups.items():
        kind, tag, fn, ver = key
        out.append("")
        if kind == "NVDA":
            out.append("NVDA tag=%s function=%s version=0x%x: %d calls" % (
                tag, escape_function_name(fn), ver, len(g["calls"])))
        else:
            out.append("non-NVDA escape (first word %s): %d calls" % (tag, len(g["calls"])))
        out.append("  sizes   " + ", ".join("%d x%d" % kv for kv in sorted(g["sizes"].items())))
        out.append("  callers " + ", ".join("%s %d" % kv for kv in g["callers"].most_common()))
        if g["status"]:
            out.append("  status  " + ", ".join(
                "0x%x%s x%d" % (s, (" " + RM["status"][s]) if s in RM["status"] else "", n)
                for s, n in g["status"].most_common()))
        cmds = collections.Counter()
        for (c, before, after, h) in g["calls"]:
            if h and h["args"]:
                for a in h["args"]:
                    if a in RM["ctrls"] and a > 0xFFFF:
                        cmds[RM["ctrls"][a]] += 1
                    elif a in RM["classes"] and a > 0xFF:
                        cmds["class " + RM["classes"][a]] += 1
        if cmds:
            out.append("  rm      " + ", ".join("%s x%d" % kv for kv in cmds.most_common(12)))
        shown = g["calls"] if verbose else g["calls"][:1]
        for (c, before, after, h) in shown:
            out.append("  seq %d at %.3f ms, tid %d, %s, caller %s" % (
                c.seq, log.ms(c.qpc), c.tid, status_name(c.ret, log.P),
                log.where(log.caller_frame(c))))
            if h:
                out.append("    header: " + " ".join("%08x" % w for w in h["words"][:17]))
            cand = rm_candidates(before or b"", 0x10)
            if cand:
                out.append("    rm ids: " + ", ".join("+0x%x %s %s" % (o, k, n) for (o, k, v, n) in cand[:8]))
            if before is not None:
                out.append("    in (%d bytes):" % len(before))
                out.append(hexdump(before, limit=512 if verbose else 128))
            if after is not None and before is not None:
                rngs = diff_ranges(before, after)
                if rngs:
                    out.append("    out changed: " + ", ".join("0x%x-0x%x" % r for r in rngs[:16]))
                    if verbose:
                        for (s, e) in rngs[:16]:
                            s0 = s & ~15
                            out.append(hexdump(after[s0:min(len(after), (e + 15) & ~15)], base=s0, limit=256))
                else:
                    out.append("    out unchanged")
            for b in c.blobs:
                if b.flags & BLOB_CHASED and b.cap:
                    out.append("    chased pointer at +0x%x -> 0x%x (%s, %d bytes)" % (
                        b.index, b.addr, ("before", "after")[b.phase], b.cap))
                    if verbose:
                        out.append(hexdump(b.data, limit=128))


def view_rm(log, out, verbose):
    lbl_in = label_id(log, "InputBuffer")
    lbl_out = label_id(log, "OutputBuffer")
    names = {}
    for c in log.calls:
        if log.fname(c) in ("NtCreateFile", "NtOpenFile") and (c.ret & 0xFFFFFFFF) < 0x80000000:
            nb = c.blob(label_id(log, "ObjectAttributes.ObjectName.Buffer"), 0)
            hb = c.blob(label_id(log, "FileHandle"), 1)
            if nb and hb:
                names[int.from_bytes(hb[:log.P], "little")] = nb.decode("utf-16-le", "replace")
    out.append("%6s %10s %6s %-10s %-34s %-28s %s" % ("seq", "ms", "tid", "kind", "what", "status", "rm"))
    for c in log.calls:
        fn = log.fname(c)
        if fn == "NtGdiDdDDIEscape":
            before = c.blob(label_id(log, "pData.pPrivateDriverData"), 0)
            after = c.blob(label_id(log, "pData.pPrivateDriverData"), 1)
            h = parse_nvda(before)
            if not h:
                continue
            ha = parse_nvda(after) if after else None
            what = "%s %s" % (h["tag"], escape_function_name(h["function"]))
            rm = []
            for a in h["args"]:
                if a in RM["ctrls"] and a > 0xFFFF:
                    rm.append(RM["ctrls"][a])
                elif a in RM["classes"] and a > 0xFF:
                    rm.append("class " + RM["classes"][a])
            if not rm:
                rm = ["?" + n for (o, k, v, n) in rm_candidates(before, 0x44, 3)]
            st = ha["status"] if ha and ha["status"] is not None else None
            sts = status_name(c.ret, log.P)
            if st is not None:
                sts += " rm=%s" % RM["status"].get(st, "0x%x" % st)
            out.append("%6d %10.3f %6d %-10s %-34s %-28s %s" % (
                c.seq, log.ms(c.qpc), c.tid, "escape", what[:34], sts[:28], " ".join(rm)))
            if verbose and before:
                out.append(hexdump(before[0x44:] if h["payload_off"] else before, base=0x44 if h["payload_off"] else 0))
        elif fn in ("NtDeviceIoControlFile", "NtFsControlFile"):
            code = c.args[5] & 0xFFFFFFFF if len(c.args) > 5 else 0
            dev = names.get(c.args[0], "handle 0x%x" % c.args[0])
            inb = c.blob(lbl_in, 0) if lbl_in is not None else None
            outb = c.blob(lbl_out, 1) if lbl_out is not None else None
            cand = rm_candidates(inb or b"", 0, 3)
            out.append("%6d %10.3f %6d %-10s %-34s %-28s %s" % (
                c.seq, log.ms(c.qpc), c.tid, "ioctl", ("%s 0x%08x" % (dev.split("\\")[-1], code))[:34],
                status_name(c.ret, log.P)[:28],
                "in=%d out=%d %s" % (len(inb or b""), len(outb or b""),
                                     " ".join("?" + n for (o, k, v, n) in cand))))
            if verbose:
                if inb:
                    out.append(hexdump(inb, limit=256))
                if outb:
                    out.append("      out:")
                    out.append(hexdump(outb, limit=256))


OBJECT_RULES = {
    # function: [(kind, field, "create"|"destroy"|"array-destroy")]
    "NtGdiDdDDIOpenAdapterFromLuid": [("adapter", "hAdapter", "create")],
    "NtGdiDdDDIOpenAdapterFromHdc": [("adapter", "hAdapter", "create")],
    "NtGdiDdDDIOpenAdapterFromDeviceName": [("adapter", "hAdapter", "create")],
    "NtGdiDdDDIOpenAdapterFromGdiDisplayName": [("adapter", "hAdapter", "create")],
    "NtGdiDdDDICloseAdapter": [("adapter", "hAdapter", "destroy")],
    "NtGdiDdDDICreateDevice": [("device", "hDevice", "create")],
    "NtGdiDdDDIDestroyDevice": [("device", "hDevice", "destroy")],
    "NtGdiDdDDICreateContext": [("context", "hContext", "create")],
    "NtGdiDdDDICreateContextVirtual": [("context", "hContext", "create")],
    "NtGdiDdDDIDestroyContext": [("context", "hContext", "destroy")],
    "NtGdiDdDDICreateHwQueue": [("hwqueue", "hHwQueue", "create"),
                                ("syncobject", "hHwQueueProgressFence", "create")],
    "NtGdiDdDDIDestroyHwQueue": [("hwqueue", "hHwQueue", "destroy")],
    "NtGdiDdDDICreatePagingQueue": [("pagingqueue", "hPagingQueue", "create"),
                                    ("syncobject", "hSyncObject", "create")],
    "NtGdiDdDDIDestroyPagingQueue": [("pagingqueue", "hPagingQueue", "destroy")],
    "NtGdiDdDDICreateSynchronizationObject": [("syncobject", "hSyncObject", "create")],
    "NtGdiDdDDICreateSynchronizationObject2": [("syncobject", "hSyncObject", "create")],
    "NtGdiDdDDICreateNativeFence": [("syncobject", "hSyncObject", "create")],
    "NtGdiDdDDIOpenSyncObjectFromNtHandle2": [("syncobject", "hSyncObject", "create")],
    "NtGdiDdDDIDestroySynchronizationObject": [("syncobject", "hSyncObject", "destroy")],
    "NtGdiDdDDICreateAllocation": [("resource", "hResource", "create")],
    "NtGdiDdDDICreateAllocation2": [("resource", "hResource", "create")],
    "NtGdiDdDDIOpenResource": [("resource", "hResource", "create")],
    "NtGdiDdDDIOpenResource2": [("resource", "hResource", "create")],
    "NtGdiDdDDIOpenResourceFromNtHandle": [("resource", "hResource", "create")],
    "NtGdiDdDDIDestroyAllocation": [("resource", "hResource", "destroy")],
    "NtGdiDdDDIDestroyAllocation2": [("resource", "hResource", "destroy")],
    "NtGdiDdDDICreateKeyedMutex2": [("keyedmutex", "hKeyedMutex", "create")],
    "NtGdiDdDDIDestroyKeyedMutex": [("keyedmutex", "hKeyedMutex", "destroy")],
}


def view_objects(log, out):
    objs = collections.OrderedDict()   # (kind, handle) -> dict
    lbl_ai = label_id(log, "pData.pAllocationInfo")
    lbl_al = label_id(log, "pData.phAllocationList")
    lbl_apd = label_id(log, "pData.pAllocationInfo[].pPrivateDriverData")
    lbl_pd = label_id(log, "pData.pPrivateDriverData")
    va = []

    def create(kind, h, c, info=""):
        if not h:
            return
        key = (kind, h)
        if key in objs and objs[key]["destroyed"] is None:
            objs[key]["info"] += " (re-created seq %d)" % c.seq
            return
        objs[key] = {"kind": kind, "h": h, "created": c, "destroyed": None, "info": info,
                     "caller": log.nv_caller(c) or log.caller(c)}

    def destroy(kind, h, c):
        key = (kind, h)
        if key in objs and objs[key]["destroyed"] is None:
            objs[key]["destroyed"] = c
        elif h:
            objs[key] = {"kind": kind, "h": h, "created": None, "destroyed": c, "info": "",
                         "caller": log.nv_caller(c) or log.caller(c)}

    for c in log.calls:
        name = log.fname(c)
        ok = (c.ret & 0xFFFFFFFF) < 0x80000000
        rules = OBJECT_RULES.get(name)
        fn = L.FUNC_BY_NAME.get(name)
        if rules and ok:
            phase = 0 if any(r[2] == "destroy" for r in rules) else 1
            d = call_struct(log, c, phase)
            if not d:
                continue
            for (kind, field, act) in rules:
                h = d.get(field)
                if act == "create":
                    info = ""
                    if "PrivateDriverDataSize" in d and kind in ("context", "hwqueue", "resource", "device"):
                        info = "priv=%d" % d["PrivateDriverDataSize"]
                    create(kind, h, c, info)
                else:
                    destroy(kind, h, c)
            if name in ("NtGdiDdDDICreateAllocation", "NtGdiDdDDICreateAllocation2"):
                t, _ = resolve_label(fn, "pData.pAllocationInfo")
                data = c.blob(lbl_ai, 1) or c.blob(lbl_ai, 0)
                if t is not None and data:
                    es = t.size(log.P)
                    for e in range(len(data) // es):
                        ai = decode_struct(t, data[e * es:(e + 1) * es], log.P)
                        pd = c.blob(lbl_apd, 0, e)
                        info = "resource=0x%x priv=%d" % (d.get("hResource", 0), ai.get("PrivateDriverDataSize", 0))
                        if ai.get("GpuVirtualAddress"):
                            info += " va=0x%x" % ai["GpuVirtualAddress"]
                        if pd:
                            info += " pd[0:8]=%s" % pd[:8].hex()
                        create("allocation", ai.get("hAllocation"), c, info)
            if name in ("NtGdiDdDDIDestroyAllocation", "NtGdiDdDDIDestroyAllocation2"):
                data = c.blob(lbl_al, 0)
                if data:
                    for e in range(len(data) // 4):
                        destroy("allocation", struct.unpack_from("<I", data, e * 4)[0], c)
        if ok and name in ("NtGdiDdDDIReserveGpuVirtualAddress", "NtGdiDdDDIMapGpuVirtualAddress"):
            d = call_struct(log, c, 1)
            if d:
                size = d.get("Size") or (d.get("SizeInPages", 0) * 4096)
                va.append((c, name[10:], d.get("VirtualAddress", 0), size, d.get("hAllocation")))
        if ok and name == "NtGdiDdDDIFreeGpuVirtualAddress":
            d = call_struct(log, c, 0)
            if d:
                va.append((c, "Free", d.get("BaseAddress", 0), d.get("Size", 0), None))
    _ = lbl_pd
    by_kind = collections.OrderedDict()
    for o in objs.values():
        by_kind.setdefault(o["kind"], []).append(o)
    for kind, lst in by_kind.items():
        alive = sum(1 for o in lst if o["destroyed"] is None)
        out.append("%s: %d (%d not destroyed)" % (kind, len(lst), alive))
        for o in lst:
            cr, de = o["created"], o["destroyed"]
            out.append("  0x%08x  created %s  destroyed %s  by %-16s %s" % (
                o["h"], ("%10.3f ms (%s)" % (log.ms(cr.qpc), log.short(cr))) if cr else "%31s" % "-",
                ("%10.3f ms" % log.ms(de.qpc)) if de else "%13s" % "-", o["caller"], o["info"]))
    if va:
        out.append("gpu va: %d operations" % len(va))
        for (c, what, addr, size, h) in va:
            out.append("  %10.3f ms %-24s va=0x%012x size=0x%x%s" % (
                log.ms(c.qpc), what, addr, size, (" alloc=0x%x" % h) if h else ""))


def view_diff(a, b, out):
    out.append("A: %s (%s)" % (a.path, a.procinfo.get("exe", "?")))
    out.append("B: %s (%s)" % (b.path, b.procinfo.get("exe", "?")))
    ma = {m.name: m.version for m in a.modules if m.name.startswith("nv")}
    mb = {m.name: m.version for m in b.modules if m.name.startswith("nv")}
    for n in sorted(set(ma) | set(mb)):
        if ma.get(n) != mb.get(n):
            out.append("  module %-22s A %-18s B %s" % (
                n, ".".join(map(str, ma[n])) if n in ma else "-", ".".join(map(str, mb[n])) if n in mb else "-"))
    ca = collections.Counter(a.fname(c) for c in a.calls)
    cb = collections.Counter(b.fname(c) for c in b.calls)
    out.append("")
    out.append("%-52s %8s %8s" % ("function", "A", "B"))
    for n in sorted(set(ca) | set(cb), key=lambda n: -(ca[n] + cb[n])):
        mark = "" if ca[n] == cb[n] else "  *"
        out.append("%-52s %8d %8d%s" % (n, ca[n], cb[n], mark))

    def esc_groups(log):
        g = collections.Counter()
        for c, before, after in escape_calls(log):
            h = parse_nvda(before)
            g[(h["tag"], h["function"], len(before)) if h else ("raw", None, len(before or b""))] += 1
        return g
    ga, gb = esc_groups(a), esc_groups(b)
    out.append("")
    out.append("%-40s %8s %8s" % ("escape tag/function/size", "A", "B"))
    for k in sorted(set(ga) | set(gb), key=lambda k: (str(k[0]), k[1] or 0, k[2])):
        mark = "" if ga[k] == gb[k] else "  *"
        out.append("%-40s %8d %8d%s" % ("%s %s %d" % (k[0], escape_function_name(k[1]), k[2]), ga[k], gb[k], mark))
    ha = {(h.module, h.name) for h in a.hooks.values() if h.flags & HOOK_OK}
    hb = {(h.module, h.name) for h in b.hooks.values() if h.flags & HOOK_OK}
    if ha != hb:
        out.append("")
        for x in sorted(ha - hb):
            out.append("  hooked only in A: %s!%s" % x)
        for x in sorted(hb - ha):
            out.append("  hooked only in B: %s!%s" % x)


def main(argv=None):
    ap = argparse.ArgumentParser(description="Decode nvtrace .ntr logs")
    ap.add_argument("logs", nargs="+")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--summary", action="store_true")
    g.add_argument("--calls", action="store_true")
    g.add_argument("--escapes", action="store_true")
    g.add_argument("--rm", action="store_true")
    g.add_argument("--objects", action="store_true")
    g.add_argument("--diff", action="store_true")
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--grep", help="--calls: only functions containing this text")
    a = ap.parse_args(argv)
    out = []
    if a.diff:
        if len(a.logs) != 2:
            ap.error("--diff needs two logs")
        view_diff(Log(a.logs[0]), Log(a.logs[1]), out)
    else:
        for i, p in enumerate(a.logs):
            log = Log(p)
            if i:
                out.append("")
            if a.calls:
                view_calls(log, out, a.verbose, a.grep)
            elif a.escapes:
                view_escapes(log, out, a.verbose)
            elif a.rm:
                view_rm(log, out, a.verbose)
            elif a.objects:
                view_objects(log, out)
            else:
                view_summary(log, out)
    try:
        sys.stdout.write("\n".join(out) + "\n")
    except BrokenPipeError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())

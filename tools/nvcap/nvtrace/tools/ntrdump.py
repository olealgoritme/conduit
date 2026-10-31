#!/usr/bin/env python3
"""Decode nvtrace logs (.ntr).

    ntrdump.py LOG [--summary]            process, modules, hooks, per-function and per-caller counts
    ntrdump.py LOG --calls [-v] [--grep S] every call: time, thread, function, status, caller, fields
    ntrdump.py LOG --escapes [-v]         D3DKMT escapes grouped by NVIDIA header/tag/function, RM decode
    ntrdump.py LOG --rm [-v]              RM view: NVDA escapes and device IOCTLs in order, RM names
    ntrdump.py LOG --objects              lifetimes of adapters/devices/contexts/queues/allocations/fences/VA,
                                          CPU mappings handed out, device handles, late IOCTL completions
    ntrdump.py LOG --timeline [-v]        per HW queue/context: submits, fences, presents, doorbells, samples
    ntrdump.py LOG --pushbuf [-v] [--seq N] captured command buffers decoded as NVIDIA methods
    ntrdump.py LOG --registry             reads/writes of NVIDIA and display registry keys with values
    ntrdump.py LOG --files                files and devices opened (NVIDIA DLLs' files, caches, DriverStore)
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

REC_PAD = 0
REC_PROCINFO, REC_STRING, REC_HOOK, REC_MODULE, REC_MODULE_UNLOAD, REC_CALL, REC_NOTE, REC_END = range(1, 9)
REC_GPUMEM, REC_REGION, REC_IOCOMPLETE, REC_HANDLE = range(9, 13)
GPUMEM_KINDS = {1: "submit", 2: "segment", 3: "sample", 4: "watch"}
GM_UNMAPPED, GM_TRUNCATED, GM_FAULT, GM_FIRST, GM_SYNC = 1, 2, 4, 8, 16
RGN_SOURCES = {1: "Lock", 2: "Lock2", 3: "pSystemMem", 4: "escape", 5: "ioctl", 6: "doorbell",
               7: "doorbell2", 8: "doorbell-status", 9: "queued-fence", 10: "fence",
               11: "hwqueue-fence", 12: "paging-fence"}
IOC_TIMEOUT, IOC_EARLY, IOC_AT_EXIT = 1, 2, 4
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


class GpuMem:
    __slots__ = ("seq", "tid", "qpc", "kind", "flags", "gpu_va", "cpu_va", "req", "size", "halloc",
                 "call_seq", "queue", "index", "fence", "aux", "data")


class Region:
    __slots__ = ("seq", "qpc", "addr", "base", "size", "alloc_base", "protect", "type", "source",
                 "call_seq", "handle", "offset")


class IoComplete:
    __slots__ = ("seq", "qpc", "call_seq", "status", "information", "handle", "out_addr", "out_len",
                 "size", "flags", "code", "data")


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
        self.gpumem = []
        self.regions = []
        self.iocomplete = []
        self.handles = []
        self.unclosed = False
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
        if self.version not in (1, 2):
            raise ValueError("%s: unsupported log version %d" % (self.path, self.version))
        off = hsize
        n = len(d)
        seen = set()
        self.duplicates = 0
        while off + 8 <= n:
            typ, size = struct.unpack_from("<II", d, off)
            if self.version >= 2 and size == 0:
                # mapped-file writer: zeros past the last record (process
                # killed, or still running)
                if not any(d[off:off + 4096]):
                    self.unclosed = True
                    off = n
                    break
                self.bad = "zero-size record at offset %d" % off
                break
            if typ == REC_PAD and self.version >= 2:
                if size < 8 or size % 8 or off + size > n:
                    self.bad = "bad pad record at offset %d" % off
                    break
                off += size
                continue
            if off + 24 > n:
                self.bad = "truncated record at offset %d" % off
                break
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
        elif typ == REC_GPUMEM:
            g = GpuMem()
            g.seq, g.tid, g.qpc = seq, tid, qpc
            (g.kind, g.flags, g.gpu_va, g.cpu_va, g.req, g.size, g.halloc, g.call_seq, g.queue, g.index,
             g.fence, g.aux) = struct.unpack_from("<IIQQIIIIIIQQ", body, 0)
            g.data = body[64:64 + g.size]
            self.gpumem.append(g)
        elif typ == REC_REGION:
            r = Region()
            r.seq, r.qpc = seq, qpc
            (r.addr, r.base, r.size, r.alloc_base, r.protect, r.type, r.source, r.call_seq, r.handle,
             r.offset) = struct.unpack_from("<QQQQIIIIII", body, 0)
            self.regions.append(r)
        elif typ == REC_IOCOMPLETE:
            c = IoComplete()
            c.seq, c.qpc = seq, qpc
            (c.call_seq, c.status, c.information, c.handle, c.out_addr, c.out_len, c.size, c.flags,
             c.code) = struct.unpack_from("<IIQQQIIII", body, 0)
            c.data = body[48:48 + c.size]
            self.iocomplete.append(c)
        elif typ == REC_HANDLE:
            h, how = struct.unpack_from("<QI", body, 0)
            name, _ = self._cstr(body, 16)
            self.handles.append((qpc, h, how, name))

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
        return L.kmt_name(self.fname(c))

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


PAGE_PROT = {0x01: "NOACCESS", 0x02: "R", 0x04: "RW", 0x08: "WRITECOPY", 0x10: "X", 0x20: "RX",
             0x40: "RWX", 0x80: "XWRITECOPY"}


def protect_name(p):
    if not p:
        return "-"
    out = [PAGE_PROT.get(p & 0xFF, "0x%x" % (p & 0xFF))]
    if p & 0x100:
        out.append("GUARD")
    if p & 0x200:
        out.append("NOCACHE")
    if p & 0x400:
        out.append("WRITECOMBINE")
    return "|".join(out)


def mem_type_name(t):
    return {0x1000000: "image", 0x40000: "mapped", 0x20000: "private"}.get(t, "-" if not t else "0x%x" % t)


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
    if log.gpumem:
        out.append("")
        out.append("GPU memory records:")
        kinds = collections.Counter(GPUMEM_KINDS.get(g.kind, str(g.kind)) for g in log.gpumem)
        nbytes = collections.Counter()
        for g in log.gpumem:
            nbytes[GPUMEM_KINDS.get(g.kind, str(g.kind))] += g.size
        for k, n in sorted(kinds.items()):
            unm = sum(1 for g in log.gpumem if GPUMEM_KINDS.get(g.kind) == k and g.flags & GM_UNMAPPED)
            out.append("  %-8s %7d records %12d bytes%s" % (k, n, nbytes[k],
                                                         ("  (%d not CPU-mapped)" % unm) if unm else ""))
    if log.regions:
        out.append("")
        out.append("CPU mappings handed out (REGION records):")
        cnt = collections.Counter((RGN_SOURCES.get(r.source, str(r.source)), protect_name(r.protect),
                                   mem_type_name(r.type)) for r in log.regions)
        for (src, prot, typ), n in sorted(cnt.items()):
            out.append("  %-16s %-22s %-8s %d" % (src, prot, typ, n))
    if log.iocomplete:
        out.append("")
        out.append("late IOCTL completions: %d (%d timed out)" % (
            len(log.iocomplete), sum(1 for c in log.iocomplete if c.flags & IOC_TIMEOUT)))
    probed = [x for x in log.handles if x[2] == 2]
    if probed:
        out.append("")
        out.append("device handles found by probing (opened before injection, duplicated or inherited):")
        for (q, h, how, name) in probed:
            out.append("  %10.3f ms  0x%x  %s" % (log.ms(q), h, name))
    keys = sorted({x[3] for x in log.handles if x[2] == 3})
    if keys:
        out.append("")
        out.append("registry keys read (%d; --registry shows values):" % len(keys))
        for k in keys[:40]:
            out.append("  " + k)
    if log.unclosed and not log.end:
        out.append("")
        out.append("note     log not closed: the process was killed or is still running "
                   "(everything written up to then is here)")
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
        parts = []
        for (n, t), v in zip(fn.args, c.args):
            if isinstance(t, L.Prim) and not t.ptr and t.size(log.P) < 8:
                v &= (1 << (8 * t.size(log.P))) - 1
            if v:
                parts.append("%s=0x%x" % (n, v))
        extra = " ".join(parts)
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
    linked = collections.defaultdict(list)
    if verbose:
        for g in log.gpumem:
            if g.call_seq:
                linked[g.call_seq].append("gpumem %s gpu=0x%x len=%d stored=%d alloc=0x%x" % (
                    GPUMEM_KINDS.get(g.kind, g.kind), g.gpu_va, g.req, g.size, g.halloc))
        for r in log.regions:
            linked[r.call_seq].append("region %s addr=0x%x size=0x%x %s %s" % (
                RGN_SOURCES.get(r.source, r.source), r.addr, r.size, protect_name(r.protect), mem_type_name(r.type)))
        for ic in log.iocomplete:
            linked[ic.call_seq].append("completed later: %s info=%d (%d bytes stored)" % (
                status_name(ic.status, log.P), ic.information, ic.size))
    for c in log.calls:
        if grep and grep.lower() not in log.fname(c).lower():
            continue
        out.append(call_line(log, c))
        if verbose:
            out.append("      frames: " + " ".join(log.where(a) for a in c.frames))
            for txt in linked.get(c.seq, ()):
                out.append("      -> " + txt)
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
    # kmt name (no NtGdiDdDDI/NtDxgk prefix): [(kind, field, "create"|"destroy")]
    "OpenAdapterFromLuid": [("adapter", "hAdapter", "create")],
    "OpenAdapterFromHdc": [("adapter", "hAdapter", "create")],
    "OpenAdapterFromDeviceName": [("adapter", "hAdapter", "create")],
    "OpenAdapterFromGdiDisplayName": [("adapter", "hAdapter", "create")],
    "CloseAdapter": [("adapter", "hAdapter", "destroy")],
    "CreateDevice": [("device", "hDevice", "create")],
    "DestroyDevice": [("device", "hDevice", "destroy")],
    "CreateContext": [("context", "hContext", "create")],
    "CreateContextVirtual": [("context", "hContext", "create")],
    "DestroyContext": [("context", "hContext", "destroy")],
    "CreateHwContext": [("hwcontext", "hHwContext", "create")],
    "DestroyHwContext": [("hwcontext", "hHwContext", "destroy")],
    "CreateHwQueue": [("hwqueue", "hHwQueue", "create"),
                      ("syncobject", "hHwQueueProgressFence", "create")],
    "DestroyHwQueue": [("hwqueue", "hHwQueue", "destroy")],
    "CreatePagingQueue": [("pagingqueue", "hPagingQueue", "create"),
                          ("syncobject", "hSyncObject", "create")],
    "DestroyPagingQueue": [("pagingqueue", "hPagingQueue", "destroy")],
    "CreateSynchronizationObject": [("syncobject", "hSyncObject", "create")],
    "CreateSynchronizationObject2": [("syncobject", "hSyncObject", "create")],
    "CreateNativeFence": [("syncobject", "hSyncObject", "create")],
    "OpenNativeFenceFromNtHandle": [("syncobject", "hSyncObject", "create")],
    "OpenSyncObjectFromNtHandle": [("syncobject", "hSyncObject", "create")],
    "OpenSyncObjectFromNtHandle2": [("syncobject", "hSyncObject", "create")],
    "DestroySynchronizationObject": [("syncobject", "hSyncObject", "destroy")],
    "CreateAllocation": [("resource", "hResource", "create")],
    "CreateAllocation2": [("resource", "hResource", "create")],
    "OpenResource": [("resource", "hResource", "create")],
    "OpenResource2": [("resource", "hResource", "create")],
    "OpenResourceFromNtHandle": [("resource", "hResource", "create")],
    "DestroyAllocation": [("resource", "hResource", "destroy")],
    "DestroyAllocation2": [("resource", "hResource", "destroy")],
    "CreateKeyedMutex": [("keyedmutex", "hKeyedMutex", "create")],
    "CreateKeyedMutex2": [("keyedmutex", "hKeyedMutex", "create")],
    "OpenKeyedMutex": [("keyedmutex", "hKeyedMutex", "create")],
    "OpenKeyedMutex2": [("keyedmutex", "hKeyedMutex", "create")],
    "DestroyKeyedMutex": [("keyedmutex", "hKeyedMutex", "destroy")],
    "CreateDoorbell": [("doorbell", "hDoorbell", "create")],
    "DestroyDoorbell": [("doorbell", "hDoorbell", "destroy")],
}

OBJECT_PARENT = {
    # kind: field naming the parent object in the create call
    "device": "hAdapter", "context": "hDevice", "hwcontext": "hDevice", "hwqueue": "hHwContext",
    "pagingqueue": "hDevice", "resource": "hDevice", "doorbell": "hHwQueue",
}


def ok_status(c):
    return (c.ret & 0xFFFFFFFF) < 0x80000000


def le_u32s(data):
    return [struct.unpack_from("<I", data, o)[0] for o in range(0, len(data) - 3, 4)]


class Objects:
    """Object lifetimes and GPU VA / CPU mappings from one log."""

    def __init__(self, log):
        self.log = log
        self.objs = collections.OrderedDict()     # (kind, handle) -> dict
        self.va_ops = []                          # (call, op, va, size, halloc, off)
        self.alloc_cpu = collections.defaultdict(list)   # halloc -> [(call_seq, addr, size, prot, src)]
        self.alloc_va = collections.defaultdict(list)    # halloc -> [(va, size)]
        self.alloc_res = {}
        self._build()

    def create(self, kind, h, c, info="", parent=None):
        if not h:
            return
        key = (kind, h)
        o = self.objs.get(key)
        if o and o["destroyed"] is None:
            o["info"] += " (re-created seq %d)" % c.seq
            return
        self.objs[key] = {"kind": kind, "h": h, "created": c, "destroyed": None, "info": info,
                          "parent": parent, "caller": self.log.nv_caller(c) or self.log.caller(c),
                          "uses": 0}

    def destroy(self, kind, h, c):
        key = (kind, h)
        o = self.objs.get(key)
        if o and o["destroyed"] is None:
            o["destroyed"] = c
        elif h:
            self.objs[key] = {"kind": kind, "h": h, "created": None, "destroyed": c, "info": "",
                              "parent": None, "caller": self.log.nv_caller(c) or self.log.caller(c),
                              "uses": 0}

    def _build(self):
        log = self.log
        lbl_ai = label_id(log, "pData.pAllocationInfo")
        lbl_al = label_id(log, "pData.phAllocationList")
        lbl_apd = label_id(log, "pData.pAllocationInfo[].pPrivateDriverData")
        lbl_ops = label_id(log, "pData.Operations")
        for c in log.calls:
            name = log.fname(c)
            k = L.kmt_name(name)
            ok = ok_status(c)
            rules = OBJECT_RULES.get(k) if k != name else None
            fn = L.FUNC_BY_NAME.get(name)
            if not ok:
                continue
            if rules:
                destroy_call = any(r[2] == "destroy" for r in rules)
                d = call_struct(log, c, 0 if destroy_call else 1)
                if not d:
                    continue
                for (kind, field, act) in rules:
                    h = d.get(field)
                    if act == "create":
                        info = []
                        if "PrivateDriverDataSize" in d and d["PrivateDriverDataSize"]:
                            info.append("priv=%d" % d["PrivateDriverDataSize"])
                        if kind in ("context", "hwcontext") and "NodeOrdinal" in d:
                            info.append("node=%d" % d["NodeOrdinal"])
                        if kind == "hwqueue" and d.get("HwQueueProgressFenceGPUVirtualAddress"):
                            info.append("fence_va=0x%x" % d["HwQueueProgressFenceGPUVirtualAddress"])
                        if kind == "doorbell":
                            info.append("ring=0x%x ctl=0x%x" % (d.get("hRingBuffer", 0), d.get("hRingBufferControl", 0)))
                        pf = OBJECT_PARENT.get(kind)
                        self.create(kind, h, c, " ".join(info), (pf, d.get(pf)) if pf and d.get(pf) else None)
                    else:
                        self.destroy(kind, h, c)
                if k in ("CreateAllocation", "CreateAllocation2"):
                    t, _ = resolve_label(fn, "pData.pAllocationInfo")
                    data = c.blob(lbl_ai, 1) or c.blob(lbl_ai, 0)
                    if t is not None and data:
                        es = t.size(log.P)
                        for e in range(len(data) // es):
                            ai = decode_struct(t, data[e * es:(e + 1) * es], log.P)
                            pd = c.blob(lbl_apd, 0, e)
                            ha = ai.get("hAllocation")
                            info = "resource=0x%x priv=%d" % (d.get("hResource", 0), ai.get("PrivateDriverDataSize", 0))
                            if ai.get("GpuVirtualAddress"):
                                info += " va=0x%x" % ai["GpuVirtualAddress"]
                                self.alloc_va[ha].append((ai["GpuVirtualAddress"], 0))
                            if pd:
                                info += " pd[0:8]=%s" % pd[:8].hex()
                            self.create("allocation", ha, c, info, ("hResource", d.get("hResource")))
                            self.alloc_res[ha] = d.get("hResource", 0)
                if k in ("DestroyAllocation", "DestroyAllocation2"):
                    data = c.blob(lbl_al, 0)
                    if data:
                        for h in le_u32s(data):
                            self.destroy("allocation", h, c)
            if k in ("ReserveGpuVirtualAddress", "MapGpuVirtualAddress"):
                d = call_struct(log, c, 1)
                if d:
                    size = d.get("Size") or (d.get("SizeInPages", 0) * 4096)
                    off = d.get("OffsetInPages", 0) * 4096
                    self.va_ops.append((c, k, d.get("VirtualAddress", 0), size, d.get("hAllocation"), off))
                    if d.get("hAllocation"):
                        self.alloc_va[d["hAllocation"]].append((d.get("VirtualAddress", 0), size))
            elif k == "FreeGpuVirtualAddress":
                d = call_struct(log, c, 0)
                if d:
                    self.va_ops.append((c, "Free", d.get("BaseAddress", 0), d.get("Size", 0), None, 0))
            elif k == "UpdateGpuVirtualAddress":
                data = c.blob(lbl_ops, 0) if lbl_ops is not None else None
                t = L.UPDATEGPUVA_OPERATION
                if data:
                    es = t.size(log.P)
                    for e in range(len(data) // es):
                        op = decode_struct(t, data[e * es:(e + 1) * es], log.P)
                        typ = {0: "Map", 1: "Unmap", 2: "Copy", 3: "MapProtect"}.get(op.get("OperationType"), "op%s" % op.get("OperationType"))
                        if typ == "Copy":
                            self.va_ops.append((c, "Update.Copy", op.get("DestAddress", 0), op.get("CopySize", 0), None, op.get("SourceAddress", 0)))
                            continue
                        self.va_ops.append((c, "Update." + typ, op.get("BaseAddress", 0), op.get("Size", 0), op.get("hAllocation") if typ != "Unmap" else None, op.get("AllocationOffset", 0)))
                        if typ in ("Map", "MapProtect") and op.get("hAllocation"):
                            self.alloc_va[op["hAllocation"]].append((op.get("BaseAddress", 0), op.get("Size", 0)))
        for r in log.regions:
            if r.source in (1, 2, 3) and r.handle:
                self.alloc_cpu[r.handle].append((r.call_seq, r.addr, r.size, r.protect, RGN_SOURCES.get(r.source)))
        # how often each object is named by later calls (handles as u32 in the arg0 struct)
        live = {}
        for (kind, h), o in self.objs.items():
            if kind in ("hwqueue", "context", "syncobject", "doorbell", "allocation") and h > 0xFFFF:
                live.setdefault(h, []).append(o)
        if live:
            lbl = label_id(log, "pData")
            for c in log.calls:
                data = c.blob(lbl, 0) if lbl is not None else None
                if not data:
                    continue
                for v in set(le_u32s(data[:256])):
                    for o in live.get(v, ()):
                        if o["created"] is not c:
                            o["uses"] += 1


def device_name(n):
    """\\Device\\X or \\??\\X, not a drive or UNC path"""
    n = n.lower()
    if n.startswith("\\device\\"):
        return not n.startswith("\\device\\harddiskvolume") and not n.startswith("\\device\\mup")
    if n[:4] in ("\\??\\", "\\\\.\\", "\\\\?\\"):
        r = n[4:]
        return bool(r) and not (len(r) > 1 and r[1] == ":") and not r.startswith("unc\\")
    return False


def view_objects(log, out):
    ob = Objects(log)
    by_kind = collections.OrderedDict()
    for o in ob.objs.values():
        by_kind.setdefault(o["kind"], []).append(o)
    for kind, lst in by_kind.items():
        alive = sum(1 for o in lst if o["destroyed"] is None)
        out.append("%s: %d (%d not destroyed)" % (kind, len(lst), alive))
        for o in lst:
            cr, de = o["created"], o["destroyed"]
            extra = o["info"]
            if o["parent"]:
                extra = ("%s=0x%x " % o["parent"]) + extra
            if o["uses"]:
                extra += " used-by=%d calls" % o["uses"]
            if kind == "allocation":
                for (seq, addr, size, prot, src) in ob.alloc_cpu.get(o["h"], [])[:3]:
                    extra += " cpu=0x%x+0x%x(%s,%s,seq %d)" % (addr, size, src, protect_name(prot), seq)
                vas = ob.alloc_va.get(o["h"], [])
                if vas:
                    extra += " gpu=" + ",".join("0x%x%s" % (va, ("+0x%x" % sz) if sz else "") for va, sz in vas[:3])
            out.append("  0x%08x  created %s  destroyed %s  by %-16s %s" % (
                o["h"], ("%10.3f ms (seq %d %s)" % (log.ms(cr.qpc), cr.seq, log.short(cr))) if cr else "%31s" % "-",
                ("%10.3f ms" % log.ms(de.qpc)) if de else "%13s" % "-", o["caller"], extra))
    # allocations mapped by CPU without a CreateAllocation in this log (opened before injection)
    orphans = [h for h in ob.alloc_cpu if ("allocation", h) not in ob.objs]
    if orphans:
        out.append("allocations mapped but not created in this log: %d" % len(orphans))
        for h in orphans[:64]:
            seq, addr, size, prot, src = ob.alloc_cpu[h][0]
            out.append("  0x%08x  cpu=0x%x+0x%x %s %s (seq %d)" % (h, addr, size, src, protect_name(prot), seq))
    if ob.va_ops:
        out.append("gpu va: %d operations" % len(ob.va_ops))
        for (c, what, addr, size, h, off) in ob.va_ops:
            out.append("  %10.3f ms seq %-6d %-20s va=0x%012x size=0x%x%s%s" % (
                log.ms(c.qpc), c.seq, what, addr, size, (" alloc=0x%x" % h) if h else "",
                (" off=0x%x" % off) if off and h else (" from=0x%x" % off) if what == "Update.Copy" else ""))
    if log.regions:
        out.append("cpu mappings handed out: %d" % len(log.regions))
        for r in log.regions:
            out.append("  %10.3f ms seq %-6d %-16s addr=0x%x base=0x%x size=0x%x %s %s%s%s" % (
                log.ms(r.qpc), r.call_seq, RGN_SOURCES.get(r.source, str(r.source)), r.addr, r.base, r.size,
                protect_name(r.protect), mem_type_name(r.type),
                (" handle=0x%x" % r.handle) if r.handle else "",
                (" at +0x%x of the output" % r.offset) if r.source in (4, 5) else ""))
    # device handles: opens and probes
    devs = []
    lbl_n = label_id(log, "ObjectAttributes.ObjectName.Buffer")
    lbl_h = label_id(log, "FileHandle")
    for c in log.calls:
        if log.fname(c) in ("NtCreateFile", "NtOpenFile") and ok_status(c):
            nb = c.blob(lbl_n, 0) if lbl_n is not None else None
            hb = c.blob(lbl_h, 1) if lbl_h is not None else None
            if nb and hb:
                devs.append((c.qpc, int.from_bytes(hb[:log.P], "little"), nb.decode("utf-16-le", "replace"), "open"))
    devs = [d for d in devs if device_name(d[2])]
    for (q, h, how, name) in log.handles:
        if how == 2:
            devs.append((q, h, name, "probe"))
    if devs:
        ioc = collections.Counter()
        for c in log.calls:
            if log.fname(c) in ("NtDeviceIoControlFile", "NtFsControlFile"):
                ioc[c.args[0]] += 1
        out.append("device handles: %d" % len(devs))
        for (q, h, name, how) in sorted(devs):
            out.append("  %10.3f ms 0x%-8x %-6s %-40s %d IOCTLs" % (log.ms(q), h, how, name, ioc[h]))
    # sections mapped whose handle value came back from an escape or IOCTL
    lbl_pd = label_id(log, "pData.pPrivateDriverData")
    lbl_out = label_id(log, "OutputBuffer")
    outs = []
    for c in log.calls:
        fnm = log.fname(c)
        if fnm == "NtGdiDdDDIEscape":
            b = c.blob(lbl_pd, 1) if lbl_pd is not None else None
        elif fnm in ("NtDeviceIoControlFile", "NtFsControlFile"):
            b = c.blob(lbl_out, 1) if lbl_out is not None else None
        else:
            continue
        if b:
            outs.append((c, b))
    sect = []
    for c in log.calls:
        if log.fname(c) in ("NtMapViewOfSection", "NtMapViewOfSectionEx") and c.args:
            h = c.args[0]
            if not h or h & 3:
                continue
            for (ec, b) in reversed([x for x in outs if x[0].seq < c.seq]):
                hit = None
                for o in range(0, len(b) - 3, 4):
                    if struct.unpack_from("<I", b, o)[0] == h:
                        hit = o
                        break
                if hit is not None:
                    sect.append((c, h, ec, hit))
                    break
    if sect:
        out.append("sections whose handle came from an escape/IOCTL output: %d" % len(sect))
        for (c, h, ec, off) in sect:
            out.append("  seq %-6d map section 0x%x  <- seq %d %s at +0x%x" % (c.seq, h, ec.seq, log.short(ec), off))
    if log.iocomplete:
        out.append("late IOCTL completions: %d" % len(log.iocomplete))
        for ic in log.iocomplete:
            fl = ",".join(n for b, n in ((IOC_TIMEOUT, "timeout"), (IOC_EARLY, "early"), (IOC_AT_EXIT, "at-exit")) if ic.flags & b)
            out.append("  %10.3f ms call seq %-6d code 0x%08x %s info=%d out=%d/%d %s" % (
                log.ms(ic.qpc), ic.call_seq, ic.code, status_name(ic.status, log.P), ic.information, ic.size,
                ic.out_len, fl))


# ------------------------------------------------------------------ timeline

def view_timeline(log, out, verbose=False):
    """Per queue/context: submissions with command buffer VA/size and fence
    values, GPU waits and signals, presents, doorbell notifications; CPU
    waits/signals; sampler activity per watched region."""
    gm_by_call = collections.defaultdict(list)
    for g in log.gpumem:
        if g.call_seq:
            gm_by_call[g.call_seq].append(g)
    lbl = {n: label_id(log, n) for n in ("pData.ObjectHandleArray", "pData.FenceValueArray",
                                         "pData.MonitoredFenceValueArray", "pData.BroadcastContextArray",
                                         "pData.BroadcastHwQueueArray", "pData.hHwQueues",
                                         "pData.pContextList")}

    def arr32(c, name, phase=0):
        b = c.blob(lbl[name], phase) if lbl.get(name) is not None else None
        return le_u32s(b) if b else []

    def arr64(c, name):
        b = c.blob(lbl[name], 0) if lbl.get(name) is not None else None
        return [struct.unpack_from("<Q", b, o)[0] for o in range(0, len(b) - 7, 8)] if b else []

    lanes = collections.OrderedDict()

    def add(key, c, text):
        lanes.setdefault(key, []).append((c.qpc, c.seq, c.tid, text))

    for c in log.calls:
        k = L.kmt_name(log.fname(c))
        if k == log.fname(c):
            continue
        st = "" if ok_status(c) else " -> " + status_name(c.ret, log.P)
        d = None
        if k in ("SubmitCommandToHwQueue", "SubmitCommand", "Render", "SubmitWaitForSyncObjectsToHwQueue",
                 "SubmitSignalSyncObjectsToHwQueue", "SignalSynchronizationObjectFromGpu",
                 "SignalSynchronizationObjectFromGpu2", "WaitForSynchronizationObjectFromGpu",
                 "WaitForSynchronizationObjectFromCpu", "SignalSynchronizationObjectFromCpu",
                 "Present", "PresentMultiPlaneOverlay", "PresentMultiPlaneOverlay2", "PresentMultiPlaneOverlay3",
                 "SubmitPresentToHwQueue", "SubmitPresentBltToHwQueue", "NotifyWorkSubmission",
                 "WaitForSynchronizationObject", "SignalSynchronizationObject"):
            d = call_struct(log, c, 0) or {}
        else:
            continue
        gms = gm_by_call.get(c.seq, [])
        gtxt = ""
        if gms:
            sub = [g for g in gms if g.kind == 1]
            seg = [g for g in gms if g.kind == 2]
            gtxt = "  [%s%s]" % (
                ("cmdbuf %d bytes%s" % (sub[0].size, " (not CPU-mapped)" if sub[0].flags & GM_UNMAPPED else "")) if sub else "",
                (", %d GPFIFO segments %d bytes" % (len(seg), sum(g.size for g in seg))) if seg else "")
        if k == "SubmitCommandToHwQueue":
            add("hwqueue 0x%x" % d.get("hHwQueue", 0), c, "submit cmd=0x%x len=%d fence=%d priv=%d%s%s" % (
                d.get("CommandBuffer", 0), d.get("CommandLength", 0), d.get("HwQueueProgressFenceId", 0),
                d.get("PrivateDriverDataSize", 0), gtxt, st))
        elif k == "SubmitCommand":
            ctxs = d.get("BroadcastContext") or [0]
            add("context 0x%x" % ctxs[0], c, "submit cmd=0x%x len=%d priv=%d contexts=%s%s%s" % (
                d.get("Commands", 0), d.get("CommandLength", 0), d.get("PrivateDriverDataSize", 0),
                ",".join("0x%x" % x for x in ctxs[:d.get("BroadcastContextCount", 1) or 1]), gtxt, st))
        elif k == "Render":
            add("context 0x%x" % d.get("hContext", 0), c, "render off=%d len=%d allocs=%d patches=%d%s" % (
                d.get("CommandOffset", 0), d.get("CommandLength", 0), d.get("AllocationCount", 0),
                d.get("PatchLocationCount", 0), st))
        elif k in ("SubmitWaitForSyncObjectsToHwQueue", "SubmitSignalSyncObjectsToHwQueue"):
            objs = arr32(c, "pData.ObjectHandleArray")
            vals = arr64(c, "pData.FenceValueArray")
            pairs = " ".join("0x%x=%d" % (o, v) for o, v in zip(objs, vals))
            if k.startswith("SubmitWait"):
                add("hwqueue 0x%x" % d.get("hHwQueue", 0), c, "gpu-wait %s%s" % (pairs, st))
            else:
                for q in arr32(c, "pData.BroadcastHwQueueArray") or [0]:
                    add("hwqueue 0x%x" % q, c, "gpu-signal %s%s" % (pairs, st))
        elif k in ("SignalSynchronizationObjectFromGpu", "WaitForSynchronizationObjectFromGpu"):
            objs = arr32(c, "pData.ObjectHandleArray")
            vals = arr64(c, "pData.MonitoredFenceValueArray")
            pairs = " ".join("0x%x=%d" % (o, v) for o, v in zip(objs, vals)) or " ".join("0x%x" % o for o in objs)
            add("context 0x%x" % d.get("hContext", 0), c, "%s %s%s" % (
                "gpu-signal" if k.startswith("Signal") else "gpu-wait", pairs, st))
        elif k == "SignalSynchronizationObjectFromGpu2":
            objs = arr32(c, "pData.ObjectHandleArray")
            vals = arr64(c, "pData.MonitoredFenceValueArray")
            pairs = " ".join("0x%x=%d" % (o, v) for o, v in zip(objs, vals)) or " ".join("0x%x" % o for o in objs)
            for ctx in arr32(c, "pData.BroadcastContextArray") or [0]:
                add("context 0x%x" % ctx, c, "gpu-signal %s%s" % (pairs, st))
        elif k in ("WaitForSynchronizationObjectFromCpu", "SignalSynchronizationObjectFromCpu"):
            objs = arr32(c, "pData.ObjectHandleArray")
            vals = arr64(c, "pData.FenceValueArray")
            add("cpu", c, "%s %s (%.1f us)%s" % ("cpu-wait" if "Wait" in k else "cpu-signal",
                                                " ".join("0x%x=%d" % (o, v) for o, v in zip(objs, vals)),
                                                log.dur_us(c), st))
        elif k in ("WaitForSynchronizationObject", "SignalSynchronizationObject"):
            add("context 0x%x" % d.get("hContext", 0), c, "%s %s%s" % (
                k, " ".join("0x%x" % o for o in (d.get("ObjectHandleArray") or [])[:d.get("ObjectCount", 0)]), st))
        elif k == "NotifyWorkSubmission":
            add("doorbell 0x%x" % d.get("hDoorbell", 0), c, "notify-work%s" % st)
        elif k in ("SubmitPresentToHwQueue",):
            for q in arr32(c, "pData.hHwQueues") or [0]:
                add("hwqueue 0x%x" % q, c, "present src=0x%x count=%d priv=%d%s" % (
                    d.get("hSource", 0), d.get("PresentCount", 0), d.get("PrivateDriverDataSize", 0), st))
        elif k == "SubmitPresentBltToHwQueue":
            add("hwqueue 0x%x" % d.get("hHwQueue", 0), c, "present-blt src=0x%x dst=0x%x fence=%d%s" % (
                d.get("hSource", 0), d.get("hDestination", 0), d.get("HwQueueProgressFenceId", 0), st))
        elif k == "Present":
            add("context 0x%x" % d.get("hContext", 0), c, "present src=0x%x dst=0x%x count=%d%s" % (
                d.get("hSource", 0), d.get("hDestination", 0), d.get("PresentCount", 0), st))
        elif k.startswith("PresentMultiPlaneOverlay"):
            key = "context 0x%x" % d.get("hContext", 0) if "hContext" in d else "adapter 0x%x" % d.get("hAdapter", 0)
            add(key, c, "present-mpo%s planes=%d count=%d%s" % (k[-1] if k[-1].isdigit() else "", d.get("PresentPlaneCount", 0), d.get("PresentCount", 0), st))
    samples = collections.defaultdict(list)
    for g in log.gpumem:
        if g.kind == 3:
            samples[g.queue].append(g)
        elif g.kind == 4:
            lanes.setdefault("watch %d" % g.index, []).append(
                (g.qpc, g.seq, g.tid, "watch cpu=0x%x size=0x%x gpu=0x%x alloc=0x%x" % (g.cpu_va, g.req, g.gpu_va, g.halloc)))
    for wid, lst in samples.items():
        for g in lst:
            lanes.setdefault("watch %d" % wid, []).append(
                (g.qpc, g.seq, g.tid, "page %d changed%s gpu=0x%x%s" % (
                    g.index, " (first copy)" if g.flags & GM_FIRST else "", g.gpu_va,
                    " at sync point" if g.flags & GM_SYNC else "")))
    if not lanes:
        out.append("no submissions, fences, presents or samples in this log")
        return
    for key, evs in lanes.items():
        evs.sort()
        out.append("%s: %d events" % (key, len(evs)))
        shown = evs if verbose or len(evs) <= 400 else evs[:200] + [None] + evs[-200:]
        for e in shown:
            if e is None:
                out.append("    ... %d more (-v shows all)" % (len(evs) - 400))
                continue
            q, seq, tid, text = e
            out.append("  %10.3f ms seq %-7d tid %-6d %s" % (log.ms(q), seq, tid, text))


# ------------------------------------------------------------------ pushbuffers

def load_classes():
    p = os.path.join(HERE, "nvclasses.json")
    if not os.path.exists(p):
        return {}
    with open(p) as f:
        d = json.load(f)
    out = {}
    for k, v in d.get("classes", {}).items():
        if k.endswith("sw"):
            continue
        v["m"] = {int(x): y for x, y in v["methods"].items()}
        out[int(k, 16)] = v
    return out


CLASSES = None
HOST_CHAIN = [0xCA6F, 0xC96F, 0xC86F, 0xC56F, 0xC46F, 0xC36F]
# GB202 (RTX 5090); NVIDIA's driver binds its own subchannels with SET_OBJECT,
# which the decoder follows, so this only matters before the first SET_OBJECT
DEFAULT_SUBCH = {0: 0xCE97, 1: 0xCEC0, 2: 0xCD40, 3: 0x902D, 4: 0xCAB5}


def method_name(cls, mthd):
    """(name, fields) of method offset mthd in class cls, or None."""
    chain = HOST_CHAIN if mthd < 0x100 else [cls]
    for c in chain:
        v = CLASSES.get(c)
        if not v:
            continue
        m = v["m"].get(mthd)
        if m:
            return v["name"], m[0], m[1], v
        for (n, base, stride, count, fields) in v["arrays"]:
            if mthd >= base and stride and (mthd - base) % stride == 0:
                i = (mthd - base) // stride
                if i < (count or 256):
                    return v["name"], "%s(%d)" % (n, i), fields, v
    return None


def fmt_fields_nv(meth, fields, cls_entry, val):
    parts = []
    for (fn, hi, lo) in fields:
        if hi - lo >= 31:
            continue
        fv = (val >> lo) & ((1 << (hi - lo + 1)) - 1)
        en = cls_entry["enums"].get("%s_%s" % (meth.split("(")[0], fn), {}).get(str(fv))
        if fv or (en and lo == 0 and hi == 0 and val == 1):
            parts.append("%s=%s" % (fn, en or ("0x%x" % fv if fv > 9 else str(fv))))
    return " ".join(parts)


def decode_pushbuf(data, out, subch, limit, stats, indent="    "):
    """NVIDIA method stream (DMA format of the GPFIFO classes)."""
    n = len(data) // 4
    i = 0
    shown = 0

    def emit(sc, mthd, val, off):
        nonlocal shown
        cls = subch.get(sc, 0)
        if mthd == 0 and sc is not None:
            subch[sc] = val & 0xFFFF
            cls = subch[sc]
        nm = method_name(cls, mthd) if CLASSES else None
        key = (nm[0] if nm else "class 0x%x" % cls, nm[1] if nm else "0x%04x" % mthd)
        stats[key] += 1
        if shown < limit:
            if nm:
                fl = fmt_fields_nv(nm[1], nm[2], nm[3], val)
                if mthd == 0:
                    bound = CLASSES.get(val & 0xFFFF)
                    fl = "-> subchannel %d = %s" % (sc, bound["name"] if bound else "class 0x%04x" % (val & 0xFFFF))
                out.append("%s+%05x s%d %s.%s = 0x%08x%s" % (indent, off, sc, nm[0], nm[1], val, ("  " + fl) if fl else ""))
            else:
                out.append("%s+%05x s%d class 0x%04x method 0x%04x = 0x%08x" % (indent, off, sc, cls, mthd, val))
        shown += 1

    while i < n:
        h = struct.unpack_from("<I", data, i * 4)[0]
        off = i * 4
        op = h >> 29
        addr = (h & 0xFFF) << 2
        sc = (h >> 13) & 7
        cnt = (h >> 16) & 0x1FFF
        i += 1
        if h == 0:
            continue                                    # NOP
        if op == 1 or op == 3 or op == 5:               # INC / NON_INC / ONE_INC
            for k in range(cnt):
                if i >= n:
                    out.append("%s+%05x truncated method data" % (indent, off))
                    return shown
                val = struct.unpack_from("<I", data, i * 4)[0]
                m = addr + 4 * k if op == 1 else (addr if op == 3 or k == 0 else addr + 4)
                emit(sc, m, val, i * 4)
                i += 1
        elif op == 4:                                   # IMMD
            emit(sc, addr, cnt, off)
        elif op == 0:
            tert = (h >> 16) & 3
            if shown < limit:
                out.append("%s+%05x %s 0x%x" % (indent, off, ("OLD_INC_METHOD", "SET_SUBDEVICE_MASK",
                                                              "STORE_SUBDEVICE_MASK", "USE_SUBDEVICE_MASK")[tert], (h >> 4) & 0xFFF))
            shown += 1
        elif op == 7:
            if shown < limit:
                out.append("%s+%05x END_PB_SEGMENT" % (indent, off))
            return shown
        else:
            if shown < limit:
                out.append("%s+%05x unknown header 0x%08x (not a method stream?)" % (indent, off, h))
            return shown
    return shown


def gpfifo_entries(data):
    """[(va, dwords, level, sync, opcode/None)] if data parses as GPFIFO entries."""
    ents = []
    ext = 0
    for o in range(0, len(data) - 7, 8):
        e0, e1 = struct.unpack_from("<II", data, o)
        dw = (e1 >> 10) & 0x1FFFFF
        if not dw:
            if (e1 & 0xFF) == 4:
                ext = ((e0 >> 8) & 0x1FFFF) << 40
            ents.append((None, 0, 0, 0, e1 & 0xFF))
            continue
        ents.append((ext | ((e1 & 0xFF) << 32) | (e0 & ~3), dw, (e1 >> 9) & 1, e1 >> 31, None))
    return ents


def view_pushbuf(log, out, verbose, seq_filter, subch_spec):
    global CLASSES
    if CLASSES is None:
        CLASSES = load_classes()
    if not CLASSES:
        out.append("(nvclasses.json not found: methods shown as numbers)")
    subch0 = dict(DEFAULT_SUBCH)
    if subch_spec:
        for part in subch_spec.split(","):
            a, _, b = part.partition("=")
            subch0[int(a)] = int(b, 16)
    limit = 1 << 30 if verbose else 400
    stats = collections.Counter()
    nbuf = 0
    segs_by_call = collections.defaultdict(list)
    for g in log.gpumem:
        if g.kind == 2:
            segs_by_call[g.call_seq].append(g)
    subch = dict(subch0)     # bindings persist across buffers of one log
    for g in log.gpumem:
        if g.kind not in (1, 2) or (seq_filter and g.call_seq not in seq_filter):
            continue
        nbuf += 1
        flags = ",".join(n for b, n in ((GM_UNMAPPED, "not-cpu-mapped"), (GM_TRUNCATED, "truncated"),
                                         (GM_FAULT, "short-read")) if g.flags & b)
        out.append("%s call seq %d queue 0x%x fence %d gpu 0x%x len %d stored %d alloc 0x%x %s" % (
            GPUMEM_KINDS[g.kind], g.call_seq, g.queue, g.fence, g.gpu_va, g.req, g.size, g.halloc, flags))
        if not g.data:
            continue
        if g.kind == 1 and segs_by_call.get(g.call_seq):
            out.append("  GPFIFO entries:")
            for idx, (va, dw, lvl, sync, opc) in enumerate(gpfifo_entries(g.data)):
                if va is None:
                    out.append("    [%d] control opcode %d" % (idx, opc))
                else:
                    out.append("    [%d] va=0x%x dwords=%d%s%s" % (idx, va, dw, " subroutine" if lvl else "", " sync" if sync else ""))
            continue
        decode_pushbuf(g.data, out, subch, limit, stats)
    # legacy Render path
    lbl = label_id(log, "Render.CommandBuffer")
    for c in log.calls:
        if lbl is None or (seq_filter and c.seq not in seq_filter):
            continue
        data = c.blob(lbl, 0)
        if data:
            nbuf += 1
            out.append("render cmdbuf call seq %d (%d bytes)" % (c.seq, len(data)))
            decode_pushbuf(data, out, subch, limit, stats)
    out.append("")
    out.append("%d command buffers; methods by count:" % nbuf)
    for (cls, m), n in stats.most_common(60 if not verbose else None):
        out.append("  %8d  %s.%s" % (n, cls, m))
    samples = [g for g in log.gpumem if g.kind == 3]
    if samples:
        out.append("")
        out.append("%d sampled pages (GPUMEM sample): decode one with --pushbuf --sample N" % len(samples))


REG_TYPES = {0: "NONE", 1: "SZ", 2: "EXPAND_SZ", 3: "BINARY", 4: "DWORD", 5: "DWORD_BE", 6: "LINK",
             7: "MULTI_SZ", 8: "RESOURCE_LIST", 11: "QWORD"}
REG_FUNCS = ("NtOpenKey", "NtOpenKeyEx", "NtCreateKey", "NtQueryValueKey", "NtEnumerateValueKey",
             "NtEnumerateKey", "NtQueryKey", "NtSetValueKey", "NtDeleteValueKey", "NtNotifyChangeKey")


def reg_data(typ, data):
    if data is None:
        return "-"
    if typ in (1, 2, 6):
        return repr(data.decode("utf-16-le", "replace").split("\0")[0])
    if typ == 7:
        return repr([x for x in data.decode("utf-16-le", "replace").split("\0") if x])
    if typ == 4 and len(data) >= 4:
        v = struct.unpack_from("<I", data)[0]
        return "0x%x (%d)" % (v, v)
    if typ == 11 and len(data) >= 8:
        return "0x%x" % struct.unpack_from("<Q", data)[0]
    return data[:64].hex() + ("... (%d bytes)" % len(data) if len(data) > 64 else "")


def key_value_info(cls, b, P):
    """(name, type, data) from KEY_VALUE_*_INFORMATION."""
    try:
        if cls == 0:      # Basic: TitleIndex, Type, NameLength, Name
            _, typ, nl = struct.unpack_from("<III", b)
            return b[12:12 + nl].decode("utf-16-le", "replace"), typ, None
        if cls in (1, 3):  # Full: TitleIndex, Type, DataOffset, DataLength, NameLength, Name
            _, typ, do, dl, nl = struct.unpack_from("<IIIII", b)
            return b[20:20 + nl].decode("utf-16-le", "replace"), typ, b[do:do + dl]
        if cls == 2:      # Partial: TitleIndex, Type, DataLength, Data
            _, typ, dl = struct.unpack_from("<III", b)
            return None, typ, b[12:12 + dl]
        if cls == 4:      # PartialAlign64: Type, DataLength, Data
            typ, dl = struct.unpack_from("<II", b)
            return None, typ, b[8:8 + dl]
    except struct.error:
        pass
    return None, None, None


def view_registry(log, out):
    names = {}
    for (q, h, how, name) in log.handles:
        if how == 3:
            names.setdefault(h, []).append((q, name))

    def key_name(h, q, opened_by=None):
        lst = names.get(h, [])
        if opened_by is not None:
            # the name record follows the open call
            for (q2, n) in lst:
                if q2 >= opened_by.qpc:
                    return n
        best = None
        for (q2, n) in lst:
            if q2 <= q:
                best = n
        return best or "key 0x%x" % h
    lbl_vn = label_id(log, "ValueName.Buffer")
    lbl_kvi = label_id(log, "KeyValueInformation")
    lbl_data = label_id(log, "Data")
    lbl_on = label_id(log, "ObjectAttributes.ObjectName.Buffer")
    lbl_kh = label_id(log, "KeyHandle")
    n = 0
    for c in log.calls:
        fn = log.fname(c)
        if fn not in REG_FUNCS:
            continue
        n += 1
        st = "" if ok_status(c) else "  -> " + status_name(c.ret, log.P)
        head = "%6d %10.3f %6d %-20s" % (c.seq, log.ms(c.qpc), c.tid, fn[2:])
        caller = log.nv_caller(c) or log.caller(c)
        if fn in ("NtOpenKey", "NtOpenKeyEx", "NtCreateKey"):
            nb = c.blob(lbl_on, 0) if lbl_on is not None else None
            hb = c.blob(lbl_kh, 1) if lbl_kh is not None else None
            h = int.from_bytes(hb[:log.P], "little") if hb else 0
            asked = nb.decode("utf-16-le", "replace") if nb else "?"
            full = key_name(h, c.qpc_end, c) if h else ""
            out.append("%s %s%s  [%s]%s" % (head, asked, ("  = 0x%x %s" % (h, full)) if h else "", caller, st))
            continue
        key = key_name(c.args[0], c.qpc)
        vn = c.blob(lbl_vn, 0) if lbl_vn is not None else None
        vname = vn.decode("utf-16-le", "replace") if vn is not None else ""
        if fn in ("NtQueryValueKey", "NtEnumerateValueKey"):
            cls = c.args[2] & 0xFFFFFFFF
            b = c.blob(lbl_kvi, 1) if lbl_kvi is not None else None
            nm, typ, data = key_value_info(cls, b, log.P) if b and ok_status(c) else (None, None, None)
            what = vname if fn == "NtQueryValueKey" else "[%d] %s" % (c.args[1] & 0xFFFFFFFF, nm or "")
            val = "%s %s" % (REG_TYPES.get(typ, typ), reg_data(typ, data)) if typ is not None else ""
            out.append("%s %s\\%s = %s  [%s]%s" % (head, key, what, val, caller, st))
        elif fn == "NtSetValueKey":
            typ = c.args[3] & 0xFFFFFFFF
            data = c.blob(lbl_data, 0) if lbl_data is not None else None
            out.append("%s %s\\%s := %s %s  [%s]%s" % (head, key, vname, REG_TYPES.get(typ, typ),
                                                       reg_data(typ, data), caller, st))
        else:
            out.append("%s %s %s [%s]%s" % (head, key, vname, caller, st))
    if not n:
        out.append("no registry calls on NVIDIA/display keys in this log")


def view_files(log, out):
    lbl_on = label_id(log, "ObjectAttributes.ObjectName.Buffer")
    out.append("%6s %10s %6s %-8s %-28s %-22s %s" % ("seq", "ms", "tid", "call", "status", "caller", "path"))
    for c in log.calls:
        fn = log.fname(c)
        if fn not in ("NtCreateFile", "NtOpenFile"):
            continue
        nb = c.blob(lbl_on, 0) if lbl_on is not None else None
        out.append("%6d %10.3f %6d %-8s %-28s %-22s %s" % (
            c.seq, log.ms(c.qpc), c.tid, fn[2:], status_name(c.ret, log.P)[:28],
            (log.nv_caller(c) or log.caller(c))[:22], nb.decode("utf-16-le", "replace") if nb else "?"))


def view_sample(log, out, seq, subch_spec):
    global CLASSES
    if CLASSES is None:
        CLASSES = load_classes()
    g = next((g for g in log.gpumem if g.seq == seq), None)
    if not g:
        out.append("no GPUMEM record with seq %d" % seq)
        return
    subch = dict(DEFAULT_SUBCH)
    if subch_spec:
        for part in subch_spec.split(","):
            a, _, b = part.partition("=")
            subch[int(a)] = int(b, 16)
    out.append("%s seq %d gpu 0x%x cpu 0x%x %d bytes" % (GPUMEM_KINDS.get(g.kind), g.seq, g.gpu_va, g.cpu_va, g.size))
    ents = gpfifo_entries(g.data)
    if ents and sum(1 for e in ents if e[0]) * 2 > len(ents):
        out.append("  as GPFIFO entries:")
        for idx, (va, dw, lvl, sync, opc) in enumerate(ents[:64]):
            out.append("    [%d] %s" % (idx, "control %d" % opc if va is None else "va=0x%x dwords=%d" % (va, dw)))
    out.append("  as method stream:")
    decode_pushbuf(g.data, out, subch, 1 << 30, collections.Counter())


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
    g.add_argument("--timeline", action="store_true")
    g.add_argument("--pushbuf", action="store_true")
    g.add_argument("--registry", action="store_true")
    g.add_argument("--files", action="store_true")
    g.add_argument("--diff", action="store_true")
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--grep", help="--calls: only functions containing this text")
    ap.add_argument("--seq", help="--pushbuf: only buffers of these call seqs (comma separated)")
    ap.add_argument("--subch", help="--pushbuf: subchannel classes before SET_OBJECT, e.g. 0=ce97,1=cec0")
    ap.add_argument("--sample", type=int, help="--pushbuf: decode sampled page record N (seq) as a method stream")
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
            elif a.registry:
                view_registry(log, out)
            elif a.files:
                view_files(log, out)
            elif a.timeline:
                view_timeline(log, out, a.verbose)
            elif a.pushbuf:
                if a.sample is not None:
                    view_sample(log, out, a.sample, a.subch)
                else:
                    view_pushbuf(log, out, a.verbose,
                                 set(int(x) for x in a.seq.split(",")) if a.seq else None, a.subch)
            else:
                view_summary(log, out)
    try:
        sys.stdout.write("\n".join(out) + "\n")
    except BrokenPipeError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())

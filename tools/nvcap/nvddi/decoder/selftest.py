#!/usr/bin/env python3
"""Decoder self-test: writes a synthetic .ndl exactly as nvddi.sys lays it out
(header, schema, events, registration, table, CALL/RET with argument and
nested blobs, registry, drop, end) and runs every ndldump view over it.

    decoder/selftest.py [--schema build/gen/schema.txt] [--keep FILE]
"""
import argparse
import io
import os
import struct
import sys
import tempfile
from contextlib import redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import ndldump as N  # noqa: E402


class W:
    def __init__(self, schema):
        self.b = bytearray()
        self.seq = 0
        self.q = 1000
        hdr = N.HDR.pack(b"NVDDINDL", 1, 256, 10000000, 1000, 0, 900, 7, 0, 4, 8 << 20, 10, 0, 26100, 3,
                         2 << 20, 0x00010000, 0x11007, 0)
        self.b += hdr.ljust(256, b"\0")
        self.rec(N.T_SCHEMA, schema.encode() + b"\0", seq=0)
        self.schema = N.Schema(schema)

    def rec(self, typ, payload, seq=None, thread=0xffff800000001000, irql=0, tid=4):
        if seq is None:
            self.seq += 1
            seq = self.seq
        self.q += 100
        size = (N.REC.size + len(payload) + 7) & ~7
        self.b += N.REC.pack(size, typ, 0, seq, self.q, thread, 4, tid, 0, irql, 0, 0)
        self.b += payload.ljust(size - N.REC.size, b"\0")
        return seq

    @staticmethod
    def blob(data, kind, arg=0, rule=0xFFFF, flags=0, elem=0xFFFFFFFF, parent=0xFFFF, addr=0xffffc00012340000):
        return N.BLOB.pack(len(data), len(data), kind, arg, rule, flags, elem, parent, 0, addr) + \
            data.ljust((len(data) + 7) & ~7, b"\0")

    def slot(self, name):
        return self.schema.by_name[name]

    def call(self, name, args, blobs=(), thread=0xffff800000001000):
        t, s = self.slot(name)
        p = N.CALL.pack(t, s, len(args), 0, len(blobs), 0, 0) + struct.pack("<%dQ" % len(args), *args)
        p += b"".join(blobs)
        return self.rec(N.T_CALL, p, thread=thread)

    def ret(self, name, call_seq, ret, args, blobs=(), thread=0xffff800000001000):
        t, s = self.slot(name)
        p = N.CALL.pack(t, s, len(args), 1, len(blobs), call_seq, ret) + struct.pack("<%dQ" % len(args), *args)
        p += b"".join(blobs)
        return self.rec(N.T_RET, p, thread=thread)

    def nested(self, owner, rule_path, owner_name, data, post=False, elem=0xFFFFFFFF):
        rid = [k for k, r in self.schema.rules.items() if r["path"] == rule_path and r["owner"] == owner_name][0]
        self.rec(N.T_BLOB, struct.pack("<Q", owner) + self.blob(data, N.B_NEST, 1, rid, 1 if post else 0, elem))


def rule_path_owner(schema, owner, path):
    return [k for k, r in schema.rules.items() if r["path"] == path and r["owner"] == owner]


def build(schema_text, L):
    w = W(schema_text)
    w.rec(N.T_EVENT, N.EVENT.pack(1, 0, 7, 0) + b"nvddi 1.0.0 level=3\0")
    w.rec(N.T_EVENT, N.EVENT.pack(2, 0, 0xffff1, 0xffff2) + b"attached to \\Device\\DxgKrnl\0")
    name = "\\Driver\\nvlddmkm".encode("utf-16-le")
    reg = "\\Registry\\Machine\\System\\CurrentControlSet\\Services\\nvlddmkm".encode("utf-16-le")
    strs = (name + reg).ljust((len(name) + len(reg) + 7) & ~7, b"\0")
    w.rec(N.T_REGISTER, N.REGISTER.pack(0xffffa001, 0xfffff80100000000, 0x7000000, 0x11007, 1, 0, 0xfffff802,
                                        len(name), len(reg), 0) + strs)
    tb = struct.pack("<Q", 0xfffff80100001000) * 8
    w.rec(N.T_TABLE, N.TABLE.pack(0, 181, 0xffffb000, 0xffffb100, 1544, 0x11007) + W.blob(tb, N.B_TABLE))

    # StartDevice with a DxgkCbGetDeviceInformation callback nested in it
    sz = L.size("DXGK_START_INFO") or 28
    c0 = w.call("DxgkDdiStartDevice", [0x1, 0x2, 0x3, 0x4, 0x5], [W.blob(b"\x01" * sz, N.B_ARG, 1)])
    di = bytearray(L.size("DXGK_DEVICE_INFO") or 80)
    struct.pack_into("<QQ", di, 64, 0x1000, 0)
    c1 = w.call("DxgkCbGetDeviceInformation", [0x10, 0x20])
    w.ret("DxgkCbGetDeviceInformation", c1, 0, [0x10, 0x20], [W.blob(bytes(di), N.B_ARG, 1, flags=1)])
    w.ret("DxgkDdiStartDevice", c0, 0, [0x1, 0x2, 0x3, 0x4, 0x5])

    # registry read from inside the miniport
    rp = N.REGISTRY.pack(23, 0, 0, 2, 0, 0, 0, 16, 16, 0, 0)
    key = "\\REGISTRY\\MACHINE\\SYSTEM\\ControlSet001\\Services\\nvlddmkm".encode("utf-16-le")
    val = "RmTest".encode("utf-16-le")
    data = struct.pack("<IIII", 0, 4, 4, 0x2a)
    nm = (key + val).ljust((len(key) + len(val) + 7) & ~7, b"\0")
    rp = N.REGISTRY.pack(23, 0, 0, 2, 0, len(key), len(val), len(data), len(data), 0, 0)
    w.rec(N.T_REGISTRY, rp + nm + data)

    # escape with an NVDA RM_CONTROL payload
    es = bytearray(L.size("DXGKARG_ESCAPE") or 48)
    struct.pack_into("<QI", es, 16, 0xffffc00000500000, 0x60)
    priv = bytearray(0x60)
    struct.pack_into("<IIII I", priv, 0, N.NVDA_MAGIC, 0x00010002, 0x60, 0x4E562A2A, 0x2A)
    struct.pack_into("<I", priv, 0x48, 0x20800122)
    c2 = w.call("DxgkDdiEscape", [0x11, 0x22], [W.blob(bytes(es), N.B_ARG, 1)])
    w.nested(c2, "pPrivateDriverData", "DXGKARG_ESCAPE", bytes(priv))
    r2 = w.ret("DxgkDdiEscape", c2, 0, [0x11, 0x22], [W.blob(bytes(es), N.B_ARG, 1, flags=1)])
    w.nested(r2, "pPrivateDriverData", "DXGKARG_ESCAPE", bytes(priv), post=True)

    # paging: UPDATE_PAGE_TABLE with DMA bytes written and PTEs
    bp = bytearray(L.size("DXGKARG_BUILDPAGINGBUFFER") or 320)
    struct.pack_into("<I", bp, 28, 11)
    c3 = w.call("DxgkDdiBuildPagingBuffer", [0x11, 0x33], [W.blob(bytes(bp), N.B_ARG, 1)])
    w.nested(c3, "UpdatePageTable.pPageTableEntries", "DXGKARG_BUILDPAGINGBUFFER",
             struct.pack("<QQ", 0x0000000123456001, 0x0000000123457001))
    r3 = w.ret("DxgkDdiBuildPagingBuffer", c3, 0, [0x11, 0x33], [W.blob(bytes(bp), N.B_ARG, 1, flags=1)])
    push = struct.pack("<IIII", 0x20010000, 0x0000cab5, 0x20018000 | (0x300 >> 2), 0x00000182)
    w.nested(r3, "pDmaBuffer", "DXGKARG_BUILDPAGINGBUFFER", push, post=True)

    # interrupt at DIRQL
    ni = bytearray(L.size("DXGKARGCB_NOTIFY_INTERRUPT_DATA") or 0x40)
    c4 = w.call("DxgkDdiInterruptRoutine", [0x1, 0x0], thread=0xffff800000002000)
    c5 = w.call("DxgkCbNotifyInterrupt", [0x10, 0x30], [W.blob(bytes(ni), N.B_ARG, 1)], thread=0xffff800000002000)
    w.ret("DxgkCbNotifyInterrupt", c5, 0, [0x10, 0x30], thread=0xffff800000002000)
    w.ret("DxgkDdiInterruptRoutine", c4, 1, [0x1, 0x0], thread=0xffff800000002000)

    w.rec(N.T_DROP, N.DROP.pack(3, 300, 1, 1))
    w.rec(N.T_END, N.END.pack(20, len(w.b), 0))
    return bytes(w.b)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--schema", default=os.path.join(HERE, "..", "build", "gen", "schema.txt"))
    ap.add_argument("--keep")
    a = ap.parse_args()
    if not os.path.exists(a.schema):
        print("selftest: %s missing (run make first)" % a.schema)
        return 1
    L = N.Layouts(None)
    data = build(open(a.schema).read(), L)
    path = a.keep or os.path.join(tempfile.mkdtemp(prefix="ndl"), "selftest.ndl")
    open(path, "wb").write(data)
    expect = {
        "--summary": ["HOOKED", "DxgkDdiEscape", "DROPPED"],
        "--timeline": ["DxgkCbGetDeviceInformation", "registry QueryValue", "  DxgkCbNotifyInterrupt"],
        "--calls": ["pPrivateDriverData", "UpdatePageTable"],
        "--escapes": ["NVDA", "NV_ESC_RM_CONTROL", "RM:"],
        "--paging": ["DXGK_OPERATION_UPDATE_PAGE_TABLE", "DMA written", "LAUNCH_DMA", "DXGK_PTE"],
        "--vidpn": [],
        "--events": ["REGISTER", "TABLE", "attached"],
        "--stats": ["DxgkDdiBuildPagingBuffer"],
        "--schema": ["DxgkDdiCreateAllocation"],
        "--registry": ["RmTest", "DWORD 0x2a"],
    }
    bad = 0
    for view, needles in expect.items():
        f = io.StringIO()
        with redirect_stdout(f):
            N.main([path, view, "-v"])
        out = f.getvalue()
        miss = [n for n in needles if n not in out]
        if miss:
            bad += 1
            print("FAIL %s: missing %s\n%s" % (view, miss, out[:3000]))
        else:
            print("ok   %s (%d lines)" % (view, out.count("\n")))
    print("selftest: %s" % ("FAILED" if bad else "passed"))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())

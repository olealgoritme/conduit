#!/usr/bin/env python3
"""Unit tests for mmiodump.py on synthetic nvtrace files (stdlib only).

Run: python3 -m unittest -v test_mmiodump   (from tools/)
"""
import contextlib
import gzip
import io
import os
import shutil
import struct
import subprocess
import tempfile
import unittest

import mmiodump as md

FN_FREE, FN_CONTROL, FN_ALLOC = 10, 76, 103
EV_INIT_DONE = 0x1001


class TraceWriter:
    """Writes the nvtrace format exactly like hw/vfio/nvtrace.c."""

    def __init__(self, file_index=0, pid=1234):
        self.buf = bytearray()
        self.seq = 0
        self.ts = 1_000_000_000
        self.buf += md.FILE_HDR.pack(md.FILE_MAGIC, 1, 64, 0, self.ts,
                                     file_index, pid, b"qemu-test")

    def rec(self, typ, body=b"", data=b"", dev=0, vcpu=0, flags=0):
        ln = (32 + len(body) + len(data) + 7) & ~7
        self.buf += md.REC_HDR.pack(typ, dev, flags, ln, self.seq, self.ts,
                                    vcpu, 0, 4242)
        self.buf += body + data
        self.buf += b"\0" * (ln - 32 - len(body) - len(data))
        self.seq += 1
        self.ts += 1000

    def mmio(self, off, val, write=True, size=4, bar=0, origin=0):
        self.rec(md.T_MMIO, md.BODY[md.T_MMIO][0].pack(bar, size, write, origin,
                                                       0, off, val))

    def irq(self, vector, kind=2):
        self.rec(md.T_IRQ, md.BODY[md.T_IRQ][0].pack(kind, 0, vector, 0x4021,
                                                     0xfee00000))

    def gsp(self, queue, slot, fn, params, rpc_seq, result=0, qseq=0):
        rpc = struct.pack("<IIIIIIII", 0x03000000, md.VRPC, 32 + len(params), fn,
                          result, 0, rpc_seq, 0) + params
        elem = struct.pack("<IIII", 0xC0000001, 0x2510de7e, 0, qseq) + rpc
        nelem = (len(elem) + 4095) // 4096
        body = md.BODY[md.T_GSP_MSG][0].pack(queue, 0, nelem, slot, slot + nelem,
                                             0, len(elem), 16, 0x2101000)
        self.rec(md.T_GSP_MSG, body, elem, vcpu=-1)

    def fsp(self, direction, packet):
        self.rec(md.T_FSP_PKT, md.BODY[md.T_FSP_PKT][0].pack(direction,
                                                             len(packet)), packet)

    def note(self, text):
        self.rec(md.T_NOTE, b"", text.encode(), dev=255, vcpu=-1)

    def snap(self, gpa, label, data):
        self.rec(md.T_DMA_SNAP, md.BODY[md.T_DMA_SNAP][0].pack(
            gpa, len(data), 0, label.encode()), data)

    def dev(self, name="vfio-test", vendor=0x10de, device=0x2b85):
        self.rec(md.T_DEV, md.BODY[md.T_DEV][0].pack(vendor, device, 0x030000,
                                                     1, 1, 0), name.encode())

    def save(self, path):
        data = bytes(self.buf)
        if path.endswith(".gz"):
            with gzip.open(path, "wb") as f:
                f.write(data)
        elif path.endswith(".zst"):
            subprocess.run(["zstd", "-q", "-f", "-o", path], input=data,
                           check=True)
        else:
            with open(path, "wb") as f:
                f.write(data)


def cot_message(bootargs=0x1000000):
    p = struct.pack("<HHQQIQI", 2, 860, 0x5000000, 0x6000000, 0x100000,
                    0x2000000, 0x100000)
    p += b"\0" * (852 - len(p)) + struct.pack("<Q", bootargs)
    return struct.pack("<I", 0x7e | (0x10de << 8) | (0x14 << 24)) + p


def run(args):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        md.main(args)
    return out.getvalue()


class DecoderTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="mmiodump-test-")

    def tearDown(self):
        shutil.rmtree(self.dir)

    def path(self, name):
        return os.path.join(self.dir, name)

    def basic_trace(self):
        w = TraceWriter()
        w.dev()
        w.note("nvtrace: attached")
        w.mmio(0x000000, 0x1b2000a1, write=False)
        w.mmio(0x8f2ac0, 0x01000000)
        w.mmio(0x110c00, 0)
        w.mmio(0x123456, 7, write=False)
        w.mmio(0x88000, 0x2b8510de, write=False, origin=1)
        w.irq(0)
        alloc = struct.pack("<IIIIIII4x", 0xc1d00001, 0xc1d00001, 0x5c000002,
                            0x2080, 0, 0, 0)
        ctrl = struct.pack("<IIIIIIIIQ", 0xc1d00001, 0x5c000002, 0x20800102, 0,
                           8, 0, 0, 0, 0) + b"\x01" * 8
        free = struct.pack("<IIII", 0xc1d00001, 0xc1d00001, 0x5c000002, 0)
        w.gsp(0, 0, FN_ALLOC, alloc, rpc_seq=5, qseq=0)
        w.gsp(0, 1, FN_CONTROL, ctrl, rpc_seq=6, qseq=1)
        w.gsp(0, 2, FN_FREE, free, rpc_seq=7, qseq=2)
        w.gsp(1, 0, EV_INIT_DONE, b"", rpc_seq=0, qseq=0)
        w.gsp(1, 1, FN_ALLOC, alloc, rpc_seq=5, qseq=1)
        w.gsp(0, 3, 73, b"\xab" * 9000, rpc_seq=8, qseq=3)   # 3 elements
        w.snap(0x1001000, "GSP_WPR_META", struct.pack("<Q", 0xdc3aae21371a60b3))
        return w

    def test_summary_names(self):
        p = self.path("t.nvt")
        self.basic_trace().save(p)
        out = run(["summary", p])
        self.assertIn("NV_PGSP_QUEUE_HEAD(0)", out)
        self.assertIn("NV_PFSP_EMEMC(0)", out)
        self.assertIn("NV_PMC", out)
        self.assertIn("GSP_RM_ALLOC", out)
        self.assertIn("EVENT_GSP_INIT_DONE", out)
        self.assertIn("vfio-test 10de:2b85", out)

    def test_unknown_offset_falls_back_to_block(self):
        n = md.Names()
        self.assertIn("+0x123456", n.reg(0x123456))
        self.assertEqual(n.reg(0x110c00), "NV_PGSP_QUEUE_HEAD(0)")

    def test_gsp_decode(self):
        p = self.path("t.nvt")
        self.basic_trace().save(p)
        out = run(["gsp", p])
        self.assertIn("hClass=0x2080(NV20_SUBDEVICE_0)", out)
        self.assertIn("cmd=0x20800102(NV2080_CTRL_CMD_GPU_GET_INFO_V2)", out)
        self.assertIn("FREE", out)
        self.assertIn("hObject=0x5c000002", out)
        self.assertIn("reply to seq", out)
        self.assertIn("SET_REGISTRY", out)
        self.assertIn("slot  3+3", out)          # 9000-byte RPC spans 3 slots

    def test_timeline_filters(self):
        p = self.path("t.nvt")
        self.basic_trace().save(p)
        out = run(["timeline", p, "--types", "mmio", "--grep", "QUEUE_HEAD"])
        self.assertEqual(len(out.strip().splitlines()), 1)
        out = run(["timeline", p, "--types", "mmio"])
        self.assertIn("[mirror]", out)
        out = run(["timeline", p, "--types", "irq"])
        self.assertIn("MSI-X vec 0", out)

    def test_fsp_reassembly_and_cot(self):
        w = TraceWriter()
        msg = cot_message(0x1234000)
        # two packets: SOM then EOM
        part1, part2 = msg[:400], msg[400:]
        w.fsp(0, struct.pack("<I", 0x80000001) + part1)
        w.fsp(0, struct.pack("<I", 0x40000001) + part2)
        resp = struct.pack("<III", 0x7e | (0x10de << 8) | (0x15 << 24), 0x14, 0)
        w.fsp(1, struct.pack("<I", 0xC0000001) + resp)
        p = self.path("f.nvt")
        w.save(p)
        out = run(["fsp", p])
        self.assertIn("COT", out)
        self.assertIn("gspBootArgsSysmemOffset=0x1234000", out)
        self.assertIn("FSP_RESPONSE", out)
        self.assertIn("command=0x14 (COT) error=0x0", out)

    def test_compressed(self):
        w = self.basic_trace()
        p = self.path("t.nvt.gz")
        w.save(p)
        self.assertIn("GSP_RM_CONTROL", run(["summary", p]))
        if shutil.which("zstd"):
            p = self.path("t.nvt.zst")
            w.save(p)
            self.assertIn("GSP_RM_CONTROL", run(["summary", p]))

    def test_truncated_file(self):
        p = self.path("t.nvt")
        self.basic_trace().save(p)
        with open(p, "r+b") as f:
            f.truncate(os.path.getsize(p) - 100)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            out = run(["summary", p])
        self.assertIn("truncated", err.getvalue())
        self.assertIn("records:", out)

    def test_rotation_order(self):
        a, b = TraceWriter(file_index=0), TraceWriter(file_index=1)
        a.note("first")
        b.seq = 1
        b.note("second")
        pa, pb = self.path("x.000.nvt"), self.path("x.001.nvt")
        a.save(pa)
        b.save(pb)
        out = run(["timeline", pb, pa])
        lines = out.strip().splitlines()
        self.assertIn("first", lines[0])
        self.assertIn("second", lines[1])

    def test_snaps_extract(self):
        p = self.path("t.nvt")
        self.basic_trace().save(p)
        d = self.path("snaps")
        out = run(["snaps", p, "--extract", d])
        self.assertIn("GSP_WPR_META", out)
        files = os.listdir(d)
        self.assertEqual(len(files), 1)
        with open(os.path.join(d, files[0]), "rb") as f:
            self.assertEqual(struct.unpack("<Q", f.read(8))[0],
                             0xdc3aae21371a60b3)

    def test_json(self):
        p = self.path("t.nvt")
        self.basic_trace().save(p)
        out = run(["json", p])
        self.assertIn('"rpc": "GSP_RM_ALLOC"', out)
        self.assertIn('"reg": "NV_PGSP_QUEUE_HEAD(0)"', out)


if __name__ == "__main__":
    unittest.main()

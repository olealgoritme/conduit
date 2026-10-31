"""Decoder tests on synthetic logs (python3 -m unittest discover -s tests)."""

import io
import os
import struct
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tools"))
sys.path.insert(0, HERE)

import nvt_layouts as L  # noqa: E402
import ntrdump  # noqa: E402
from ntrsynth import NtrWriter  # noqa: E402


def pack(st, P, **vals):
    size, _, fields = st.layout(P)
    b = bytearray(size)
    for (name, off, t) in fields:
        if name not in vals:
            continue
        v = vals[name]
        w = P if (isinstance(t, L.Ptr) or getattr(t, "ptr", False) or t.size(P) == P and t.name in ("handle", "size_t")) else t.size(P)
        b[off:off + w] = int(v).to_bytes(w, "little", signed=v < 0)
    return bytes(b)


NV_DLL = 0x7FFA_1000_0000
GDI = 0x7FFA_2000_0000
WIN32U = 0x7FFA_3000_0000


def nvda(function, args, payload=b"", status=0):
    hdr = struct.pack("<IIIII", 0x4E564441, 0x00010002, 0x44 + len(payload), 0x4E562A2A, function)
    hdr += b"\0" * (0x30 - len(hdr))
    hdr += struct.pack("<4I", *args) + struct.pack("<I", status)
    return hdr + payload


def build(path, P=8, variant=0):
    w = NtrWriter(ptr_size=P)
    w.procinfo({"os": "10.0.26100", "os_ubr": "4061", "ptr_size": P, "exe": r"C:\t\vktest.exe",
                "cmdline": "vktest.exe", "nvtrace": "1.0", "start_qpc": 1000, "config.rawsize": 512})
    for i, s in enumerate(["pData", "pData.pPrivateDriverData", "pData.pAllocationInfo",
                           "pData.pAllocationInfo[].pPrivateDriverData", "pData.phAllocationList",
                           "InputBuffer", "OutputBuffer", "ObjectAttributes.ObjectName.Buffer",
                           "FileHandle", "chase"]):
        w.string(i, s)
    w.hook(1, "win32u.dll", "NtGdiDdDDIEscape", arg0size=32)
    w.hook(2, "win32u.dll", "NtGdiDdDDICreateAllocation2", arg0size=72)
    w.hook(3, "win32u.dll", "NtGdiDdDDIDestroyAllocation2", arg0size=24)
    w.hook(4, "win32u.dll", "NtGdiDdDDICreateDevice", arg0size=64)
    w.hook(5, "ntdll.dll", "NtCreateFile", nargs=11)
    w.hook(6, "ntdll.dll", "NtDeviceIoControlFile", nargs=10)
    w.hook(7, "win32u.dll", "NtGdiDdDDIQueryAdapterInfo", arg0size=24)
    w.hook(8, "win32u.dll", "NtGdiDdDDIFutureThing", flags=1 | 4)
    w.module(GDI, 0x100000, r"C:\Windows\System32\gdi32.dll", (10, 0, 26100, 1))
    w.module(WIN32U, 0x100000, r"C:\Windows\System32\win32u.dll", (10, 0, 26100, 1))
    w.module(NV_DLL, 0x2000000, r"C:\Windows\System32\DriverStore\FileRepository\nv.inf_amd64\nvoglv64.dll",
             (32, 0, 16, 1074 + variant), flags=2, qpc=1500)
    frames = [WIN32U + 0x14, GDI + 0x200, NV_DLL + 0x12345]

    # NtCreateFile \??\NvAdminDevice -> handle 0x88
    name = "\\??\\NvAdminDevice".encode("utf-16-le")
    w.call(5, 2000, 2100, 0, [0x500, 0xC0000000, 0x600, 0x700] + [0] * 7, frames,
           [{"label": "ObjectAttributes.ObjectName.Buffer", "data": name, "phase": 0},
            {"label": "FileHandle", "data": struct.pack("<Q", 0x88), "phase": 1}])

    # CreateDevice
    cd = L.CREATEDEVICE
    w.call(4, 3000, 3100, 0, [0x1000], frames,
           [{"label": "pData", "data": pack(cd, P, hAdapter=0x40000000), "phase": 0},
            {"label": "pData", "data": pack(cd, P, hAdapter=0x40000000, hDevice=0xC0000040), "phase": 1}])

    # Escape: NVDA NV** NV_ESC_RM_CONTROL with NV2080_CTRL_CMD_GPU_GET_INFO_V2
    esc_in = nvda(0x2A, [0xC1D00001, 0x5C000001, 0x20800102, 0x10], b"\x11" * 16)
    esc_out = nvda(0x2A, [0xC1D00001, 0x5C000001, 0x20800102, 0x10], b"\x22" * 16, status=0)
    ed = L.ESCAPE
    for i in range(3):
        w.call(1, 4000 + i * 100, 4050 + i * 100, 0, [0x2000], frames, [
            {"label": "pData", "data": pack(ed, P, hAdapter=0x40000000, hDevice=0xC0000040,
                                           pPrivateDriverData=0x3000, PrivateDriverDataSize=len(esc_in)), "phase": 0},
            {"label": "pData.pPrivateDriverData", "data": esc_in, "phase": 0, "addr": 0x3000, "parent": 1},
            {"label": "pData", "data": b"", "phase": 1, "flags": 4},
            {"label": "pData.pPrivateDriverData", "data": esc_out, "phase": 1, "addr": 0x3000, "parent": 3},
        ])
    # an escape for an alloc with a class id
    esc2 = nvda(0x2B, [0xC1D00001, 0xCAFE0001, 0xC96F, 0x0], b"")
    if variant == 0:
        w.call(1, 4500, 4550, 0, [0x2000], frames, [
            {"label": "pData", "data": pack(ed, P, pPrivateDriverData=0x3100, PrivateDriverDataSize=len(esc2)), "phase": 0},
            {"label": "pData.pPrivateDriverData", "data": esc2, "phase": 0, "addr": 0x3100, "parent": 1}])

    # CreateAllocation2 with two allocations
    ca = L.CREATEALLOCATION2
    ai = L.ALLOCATIONINFO2
    es = ai.size(P)
    infos_in = pack(ai, P, pPrivateDriverData=0x5000, PrivateDriverDataSize=8) + \
        pack(ai, P, pPrivateDriverData=0x5100, PrivateDriverDataSize=8)
    infos_out = pack(ai, P, hAllocation=0x80000001, pPrivateDriverData=0x5000, PrivateDriverDataSize=8,
                     GpuVirtualAddress=0x100000000) + \
        pack(ai, P, hAllocation=0x80000002, pPrivateDriverData=0x5100, PrivateDriverDataSize=8)
    hdr_in = pack(ca, P, hDevice=0xC0000040, NumAllocations=2, pAllocationInfo=0x4000)
    hdr_out = pack(ca, P, hDevice=0xC0000040, hResource=0x80000000, NumAllocations=2, pAllocationInfo=0x4000)
    w.call(2, 5000, 5200, 0, [0x2100], frames, [
        {"label": "pData", "data": hdr_in, "phase": 0},
        {"label": "pData.pAllocationInfo", "data": infos_in, "phase": 0, "addr": 0x4000, "parent": 1},
        {"label": "pData.pAllocationInfo[].pPrivateDriverData", "data": b"\xAA" * 8, "phase": 0, "index": 0, "parent": 2},
        {"label": "pData.pAllocationInfo[].pPrivateDriverData", "data": b"\xBB" * 8, "phase": 0, "index": 1, "parent": 2},
        {"label": "pData", "data": hdr_out, "phase": 1},
        {"label": "pData.pAllocationInfo", "data": infos_out, "phase": 1, "addr": 0x4000, "parent": 5},
    ])
    assert es * 2 == len(infos_in)

    # QueryAdapterInfo UMDRIVERPRIVATE
    qa = L.QUERYADAPTERINFO
    w.call(7, 5500, 5510, 0, [0x2200], frames, [
        {"label": "pData", "data": pack(qa, P, hAdapter=0x40000000, Type=0, pPrivateDriverData=0x6000,
                                        PrivateDriverDataSize=0x200), "phase": 0}])

    # IOCTL on the admin device, with a chased pointer
    w.call(6, 6000, 6100, 0, [0x88, 0, 0, 0, 0x900, 0x0022E00C, 0x7000, 16, 0x7100, 16], frames, [
        {"label": "InputBuffer", "data": struct.pack("<IIQ", 0x20800102, 0, 0x8000), "phase": 0, "addr": 0x7000},
        {"label": "chase", "data": b"\x01\x02\x03\x04", "phase": 0, "addr": 0x8000, "index": 8, "flags": 8, "parent": 1},
        {"label": "OutputBuffer", "data": b"\x05" * 16, "phase": 1, "addr": 0x7100}])

    # unknown function, raw
    w.call(8, 6500, 6510, 0xC00000BB, [0x2300, 0, 0, 0, 0, 0, 0, 0], frames, [])

    # DestroyAllocation2 of both
    da = L.DESTROYALLOCATION2
    w.call(3, 7000, 7100, 0, [0x2400], frames, [
        {"label": "pData", "data": pack(da, P, hDevice=0xC0000040, hResource=0x80000000,
                                        phAllocationList=0x9000, AllocationCount=2), "phase": 0},
        {"label": "pData.phAllocationList", "data": struct.pack("<II", 0x80000001, 0x80000002), "phase": 0}])
    w.note(0, "nvtrace 1.0 started, 300 hooks")
    w.end(records=20, calls=10, dropped=0, nbytes=4096)
    w.save(path)


def run(argv):
    f = io.StringIO()
    with redirect_stdout(f):
        ntrdump.main(argv)
    return f.getvalue()


class DecoderTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.a = os.path.join(cls.tmp.name, "a.ntr")
        cls.b = os.path.join(cls.tmp.name, "b.ntr")
        cls.c32 = os.path.join(cls.tmp.name, "c32.ntr")
        build(cls.a)
        build(cls.b, variant=1)
        build(cls.c32, P=4)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_parse(self):
        log = ntrdump.Log(self.a)
        self.assertIsNone(log.bad)
        self.assertEqual(log.P, 8)
        self.assertEqual(len(log.calls), 11)
        self.assertEqual(log.procinfo["os"], "10.0.26100")
        self.assertEqual(log.end["dropped"], 0)
        c = log.calls[0]
        self.assertEqual(log.fname(c), "NtCreateFile")
        self.assertEqual(log.caller(c), "nvoglv64.dll")

    def test_summary(self):
        out = run([self.a])
        self.assertIn("nvoglv64.dll", out)
        self.assertIn("32.0.16.1074", out)
        self.assertRegex(out, r"NtGdiDdDDIEscape\s+4\s+0")
        self.assertIn("UMDRIVERPRIVATE", out)
        self.assertIn("NvAdminDevice", out)
        self.assertIn("1 raw", out)

    def test_calls_fields(self):
        out = run([self.a, "--calls", "-v"])
        self.assertIn("hResource=0x80000000", out)
        self.assertIn("NumAllocations=2", out)
        self.assertIn("hAllocation=0x80000001", out)
        self.assertIn("GpuVirtualAddress=0x100000000", out)
        self.assertIn("[NVDA NV** fn=0x2a NV_ESC_RM_CONTROL", out)
        self.assertIn("NOT_SUPPORTED", out)
        self.assertIn("nvoglv64.dll+0x12345", out)

    def test_escapes(self):
        out = run([self.a, "--escapes"])
        self.assertIn("NVDA tag=NV** function=0x2a NV_ESC_RM_CONTROL version=0x10002: 3 calls", out)
        self.assertIn("NV2080_CTRL_CMD_GPU_GET_INFO_V2 x3", out)
        self.assertIn("function=0x2b NV_ESC_RM_ALLOC", out)
        self.assertIn("class BLACKWELL_CHANNEL_GPFIFO_A", out)
        self.assertIn("out changed: 0x44-0x54", out)

    def test_rm(self):
        out = run([self.a, "--rm"])
        self.assertIn("NV2080_CTRL_CMD_GPU_GET_INFO_V2", out)
        self.assertIn("rm=NV_OK", out)
        self.assertIn("NvAdminDevice 0x0022e00c", out)
        self.assertIn("?NV2080_CTRL_CMD_GPU_GET_INFO_V2", out)

    def test_objects(self):
        out = run([self.a, "--objects"])
        self.assertIn("allocation: 2 (0 not destroyed)", out)
        self.assertIn("resource: 1 (0 not destroyed)", out)
        self.assertIn("device: 1 (1 not destroyed)", out)
        self.assertIn("pd[0:8]=aaaaaaaaaaaaaaaa", out)

    def test_diff(self):
        out = run(["--diff", self.a, self.b])
        self.assertRegex(out, r"NtGdiDdDDIEscape\s+4\s+3\s+\*")
        self.assertIn("32.0.16.1074", out)
        self.assertIn("32.0.16.1075", out)

    def test_x86_layouts(self):
        log = ntrdump.Log(self.c32)
        self.assertEqual(log.P, 4)
        out = run([self.c32, "--objects"])
        self.assertIn("allocation: 2 (0 not destroyed)", out)
        out = run([self.c32, "--calls", "-v"])
        self.assertIn("hAllocation=0x80000002", out)

    def test_truncated_log(self):
        with open(self.a, "rb") as f:
            data = f.read()
        p = os.path.join(self.tmp.name, "cut.ntr")
        with open(p, "wb") as f:
            f.write(data[:len(data) - 50])
        log = ntrdump.Log(p)
        self.assertIsNotNone(log.bad)
        out = run([p])
        self.assertIn("truncated record", out)


    def test_duplicate_records_dropped(self):
        with open(self.a, "rb") as f:
            data = f.read()
        # append a copy of the last 3 records (as a re-written chunk would be)
        log = ntrdump.Log(self.a)
        offs = []
        off = 32
        while off < len(data):
            offs.append(off)
            off += struct.unpack_from("<I", data, off + 4)[0]
        p = os.path.join(self.tmp.name, "dup.ntr")
        with open(p, "wb") as f:
            f.write(data + data[offs[-3]:])
        dup = ntrdump.Log(p)
        self.assertEqual(dup.duplicates, 3)
        self.assertEqual(len(dup.calls), len(log.calls))


class LayoutTest(unittest.TestCase):
    def test_known_sizes_x64(self):
        # values cross-checked against the WSL dxgkrnl uapi header
        self.assertEqual(L.ESCAPE.size(8), 32)
        self.assertEqual(L.CREATEALLOCATION.size(8), 72)
        self.assertEqual(L.ALLOCATIONINFO2.size(8), 96)
        self.assertEqual(L.SUBMITCOMMAND.size(8), 384)
        self.assertEqual(L.MAPGPUVIRTUALADDRESS.size(8), 104)
        self.assertEqual(L.CREATEHWQUEUE.size(8), 48)
        self.assertEqual(L.QUERYSTATISTICS.size(8), 0x328)

    def test_x86_matches_wine_wow64(self):
        # wine dlls/wow64win/gdi.c 32-bit thunk structs
        self.assertEqual(L.ALLOCATIONINFO.size(4), 24)
        self.assertEqual(L.ALLOCATIONINFO2.size(4), 56)
        self.assertEqual(L.ESCAPE.field(4, "pPrivateDriverData")[0], 16)
        self.assertEqual(L.CREATEALLOCATION.field(4, "NumAllocations")[0], 28)

    def test_generator_runs(self):
        import subprocess
        r = subprocess.run([sys.executable, os.path.join(HERE, "..", "tools", "gen_layouts.py")],
                           capture_output=True, text=True, check=True)
        self.assertIn('"NtGdiDdDDIEscape"', r.stdout)
        self.assertIn("#ifdef _WIN64", r.stdout)


if __name__ == "__main__":
    unittest.main()

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


def build_v2(path):
    """A version 2 log: GPU memory, regions, registry, late IOCTL, pad and zero tail."""
    P = 8
    w = NtrWriter(ptr_size=P, version=2)
    w.procinfo({"os": "10.0.26100", "ptr_size": P, "exe": r"C:\t\dwm.exe", "nvtrace": "2.0",
                "start_qpc": 1000})
    labels = ["pData", "pData.pPrivateDriverData", "ValueName.Buffer", "KeyValueInformation",
              "ResultLength", "KeyHandle", "ObjectAttributes.ObjectName.Buffer", "OutputBuffer",
              "pData.ObjectHandleArray", "pData.FenceValueArray"]
    for i, s_ in enumerate(labels):
        w.string(i, s_)
    w.hook(1, "win32u.dll", "NtGdiDdDDISubmitCommandToHwQueue", arg0size=48)
    w.hook(2, "win32u.dll", "NtGdiDdDDILock2", arg0size=24)
    w.hook(3, "ntdll.dll", "NtQueryValueKey", nargs=6)
    w.hook(4, "ntdll.dll", "NtDeviceIoControlFile", nargs=10)
    w.hook(5, "win32u.dll", "NtDxgkSubmitPresentToHwQueue", arg0size=1504)
    w.hook(6, "win32u.dll", "NtGdiDdDDISubmitWaitForSyncObjectsToHwQueue", arg0size=24)
    w.module(NV_DLL, 0x1000000, r"C:\Windows\System32\DriverStore\nvwgf2umx.dll", (32, 0, 16, 1074))
    frames = [NV_DLL + 0x100]
    # Lock2 hands out a write-combined CPU mapping of allocation 0x40000042
    w.call(2, 2000, 2100, 0, [0x3000], frames, [
        {"label": "pData", "data": pack(L.LOCK2, P, hDevice=0x40000001, hAllocation=0x40000042,
                                         pData=0x7FF000000000), "phase": 1}])
    w.region(0x7FF000000000, 2, call_seq=w.seq, handle=0x40000042, size=0x20000, qpc=2100)
    # submission: GPFIFO with 2 entries, the second names a pushbuffer segment
    sub = pack(L.SUBMITCOMMANDTOHWQUEUE, P, hHwQueue=0x40000080, HwQueueProgressFenceId=7,
               CommandBuffer=0x100000000, CommandLength=16)
    w.call(1, 3000, 3100, 0, [0x3100], frames, [{"label": "pData", "data": sub}])
    seq = w.seq
    seg_va = 0x100002000
    e0 = seg_va & 0xFFFFFFFC
    e1 = ((seg_va >> 32) & 0xFF) | (6 << 10)
    ctl = struct.pack("<II", 0, 4)                          # control entry: extended base 0
    w.gpumem(1, ctl + struct.pack("<II", e0, e1), gpu_va=0x100000000, cpu_va=0x7FF000000000,
             halloc=0x40000042, call_seq=seq, queue=0x40000080, fence=7, qpc=3050)
    # methods: SET_OBJECT(subch 0) = 0xCE97, LAUNCH_DMA (0x1b0) = 0x1, host SEM_ADDR_LO
    hdr_inc = lambda addr, sc, cnt: (1 << 29) | (cnt << 16) | (sc << 13) | (addr >> 2)
    push = struct.pack("<6I", hdr_inc(0, 0, 1), 0xCE97, hdr_inc(0x1B0, 0, 1), 0x1,
                       hdr_inc(0x5C, 0, 1), 0x1000)
    w.gpumem(2, push, gpu_va=seg_va, cpu_va=0x7FF000002000, halloc=0x40000042, call_seq=seq,
             queue=0x40000080, fence=7, index=1, qpc=3060)
    w.gpumem(4, b"", gpu_va=0x100000000, cpu_va=0x7FF000000000, req=0x20000, halloc=0x40000042, index=1)
    w.gpumem(3, push + b"\0" * (4096 - len(push)), gpu_va=0x100000000, cpu_va=0x7FF000000000,
             halloc=0x40000042, queue=1, index=0, flags=8, qpc=3200)
    # wait for a fence on the GPU
    wt = pack(L.SUBMITWAITFORSYNCOBJECTSTOHWQUEUE, P, hHwQueue=0x40000080, ObjectCount=1,
              ObjectHandleArray=0x5000, FenceValueArray=0x5100)
    w.call(6, 3300, 3310, 0, [0x3200], frames, [
        {"label": "pData", "data": wt},
        {"label": "pData.ObjectHandleArray", "data": struct.pack("<I", 0x40000090), "addr": 0x5000},
        {"label": "pData.FenceValueArray", "data": struct.pack("<Q", 6), "addr": 0x5100}])
    # registry: key named by a HANDLE record, DWORD value read
    w.handle(0x1A4, 3, r"\registry\machine\system\controlset001\control\class"
             r"\{4d36e968-e325-11ce-bfc1-08002be10318}\0000", qpc=3400)
    kvpi = struct.pack("<III", 0, 4, 4) + struct.pack("<I", 1)
    w.call(3, 3500, 3510, 0, [0x1A4, 0x6000, 2, 0x6100, 64, 0x6200], frames, [
        {"label": "ValueName.Buffer", "data": "EnableMidBufferPreemption".encode("utf-16-le"), "addr": 0x6010},
        {"label": "KeyValueInformation", "data": kvpi, "phase": 1, "addr": 0x6100}])
    # an IOCTL that completed later
    w.call(4, 3600, 3610, 0x103, [0x88, 0, 0, 0, 0x900, 0x0022E00C, 0x7000, 16, 0x7100, 16], frames, [])
    w.iocomplete(w.seq, 0, 16, b"\x11" * 16, code=0x0022E00C, qpc=3700)
    w.handle(0x88, 2, r"\device\nvadmindevice", qpc=1500)
    w.pad(8)
    w.pad(64)
    w.zeros(4096)       # killed: mapped-file tail
    w.save(path)


class V2Test(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.p = os.path.join(cls.tmp.name, "dwm-1.ntr")
        build_v2(cls.p)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_parse_v2(self):
        log = ntrdump.Log(self.p)
        self.assertIsNone(log.bad)
        self.assertTrue(log.unclosed)
        self.assertEqual(len(log.gpumem), 4)
        self.assertEqual(len(log.regions), 1)
        self.assertEqual(len(log.iocomplete), 1)
        self.assertEqual(len(log.handles), 2)
        out = run([self.p])
        self.assertIn("log not closed", out)
        self.assertIn("submit", out)
        self.assertIn("Lock2", out)
        self.assertIn("RW|WRITECOMBINE", out)
        self.assertIn("\\device\\nvadmindevice", out)

    def test_pushbuf(self):
        out = run([self.p, "--pushbuf"])
        self.assertIn("GPFIFO entries:", out)
        self.assertIn("va=0x100002000 dwords=6", out)
        self.assertIn("SET_OBJECT = 0x0000ce97  -> subchannel 0 = BLACKWELL_B", out)
        self.assertIn("BLACKWELL_B.LAUNCH_DMA = 0x00000001  DST_MEMORY_LAYOUT=PITCH", out)
        self.assertNotIn("REDUCTION_OP", out)
        self.assertIn("SEM_ADDR_LO = 0x00001000", out)
        log = ntrdump.Log(self.p)
        sample = [g for g in log.gpumem if g.kind == 3][0]
        out = run([self.p, "--pushbuf", "--sample", str(sample.seq)])
        self.assertIn("LAUNCH_DMA", out)

    def test_timeline(self):
        out = run([self.p, "--timeline"])
        self.assertIn("hwqueue 0x40000080:", out)
        self.assertIn("submit cmd=0x100000000 len=16 fence=7", out)
        self.assertIn("GPFIFO segments", out)
        self.assertIn("gpu-wait 0x40000090=6", out)
        self.assertIn("page 0 changed (first copy)", out)

    def test_objects_v2(self):
        out = run([self.p, "--objects"])
        self.assertIn("0x40000042  cpu=0x7ff000000000+0x20000 Lock2 RW|WRITECOMBINE", out)
        self.assertIn("late IOCTL completions: 1", out)
        self.assertIn("probe", out)

    def test_registry(self):
        out = run([self.p, "--registry"])
        self.assertIn("{4d36e968-e325-11ce-bfc1-08002be10318}\\0000\\EnableMidBufferPreemption = DWORD 0x1",
                      out)

    def test_calls_linked(self):
        out = run([self.p, "--calls", "-v"])
        self.assertIn("-> gpumem submit gpu=0x100000000 len=16", out)
        self.assertIn("-> region Lock2 addr=0x7ff000000000", out)
        self.assertIn("-> completed later: 0x00000000 SUCCESS info=16", out)

    def test_dxgk_alias(self):
        self.assertIs(L.FUNC_BY_NAME["NtDxgkSubmitPresentToHwQueue"],
                      L.FUNC_BY_NAME["NtGdiDdDDISubmitPresentToHwQueue"])
        self.assertEqual(L.kmt_name("NtDxgkCreateDoorbell"), "CreateDoorbell")


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

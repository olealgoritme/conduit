#!/usr/bin/env python3
"""End-to-end test of the nvtrace QEMU without the real GPU.

QEMU runs with the qtest accelerator (no guest code, no KVM): this script
plays the guest driver. The device is fakegpu.py, a vfio-user server that
presents itself as 10de:2b85, so QEMU's real hw/vfio/pci.c + region.c +
NVIDIA quirk code is used, including x-trap-bars and the nvtrace hooks.

The script builds, in guest RAM, the structures NVIDIA's RM builds at boot
(GSP_FMC_BOOT_PARAMS, WPR meta, libos init args, GSP_ARGUMENTS_CACHED, the
PTE-mapped command/status queues) at 610.57.04 layouts, sends the FSP COT
boot command through the FSP EMEM registers, then exchanges GSP RPCs via the
queues and the NV_PGSP_QUEUE_HEAD(0) doorbell, and finally decodes the trace
with mmiodump.py and checks what was captured.

Run: python3 test_qemu_trace.py [-v] [--qemu PATH] [--keep]
"""
import argparse
import base64
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import mmiodump as md                                   # noqa: E402

QEMU = os.path.join(HERE, "..", "install", "bin", "qemu-system-x86_64")
KEEP = False

BAR0, BAR1 = 0xC0000000, 0xC4000000
DEVFN = (4 << 3)
MSI_ADDR, MSI_DATA = 0x3000000, 0x4021

FMC_PA, WPR_PA, LIBOS_PA, RMARGS_PA = 0x1000000, 0x1001000, 0x1002000, 0x1003000
LOGINIT_PA, LOGRM_PA = 0x1010000, 0x1020000
PTE_PA, PAGES_PA = 0x2000000, 0x2100000
QSIZE, NPTE = 0x40000, 129
CMDQ, STATQ = 0x1000, 0x41000
FN_ALLOC_MEMORY = 4
FN = {"FREE": 10, "GSP_RM_CONTROL": 76, "GSP_RM_ALLOC": 103,
      "SET_REGISTRY": 73, "GSP_SET_SYSTEM_INFO": 72, "INIT_DONE": 0x1001,
      "RUN_CPU_SEQUENCER": 0x1002}


class Qtest:
    def __init__(self, args, env, cwd):
        self.p = subprocess.Popen(args, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE,
                                  stderr=open(os.path.join(cwd, "qemu.err"), "w"),
                                  env=env, cwd=cwd, text=True, bufsize=1)

    def cmd(self, s):
        self.p.stdin.write(s + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"qemu died during: {s}")
            line = line.strip()
            if line.startswith("OK"):
                return line[3:].strip()
            if line.startswith("FAIL") or line.startswith("ERR"):
                raise RuntimeError(f"{s}: {line}")

    def readl(self, a):
        return int(self.cmd(f"readl 0x{a:x}"), 16)

    def writel(self, a, v):
        self.cmd(f"writel 0x{a:x} 0x{v:x}")

    def writeq(self, a, v):
        self.cmd(f"writeq 0x{a:x} 0x{v:x}")

    def mem(self, a, data):
        if data:
            self.cmd(f"b64write 0x{a:x} 0x{len(data):x} "
                     f"{base64.b64encode(data).decode()}")

    def cfg_addr(self, off):
        self.cmd(f"outl 0xcf8 0x{0x80000000 | (DEVFN << 8) | (off & 0xfc):x}")

    def cfg_read(self, off):
        self.cfg_addr(off)
        return int(self.cmd("inl 0xcfc"), 16)

    def cfg_write(self, off, v):
        self.cfg_addr(off)
        self.cmd(f"outl 0xcfc 0x{v:x}")


def qmp(path, *cmds):
    s = socket.socket(socket.AF_UNIX)
    for _ in range(100):
        try:
            s.connect(path)
            break
        except OSError:
            time.sleep(0.05)
    f = s.makefile("rw")
    f.readline()
    for c in ("qmp_capabilities",) + cmds:
        f.write(json.dumps({"execute": c}) + "\n")
        f.flush()
        while True:
            line = f.readline()
            if not line or '"return"' in line or '"error"' in line:
                break
    s.close()


def page_gpa(i):
    return PTE_PA if i == 0 else PAGES_PA + (((i - 1) * 37) % 128) * 0x1000


def rpc_elem(fn, params, rpc_seq, qseq):
    rpc = struct.pack("<IIIIIIII", 0x03000000, md.VRPC, 32 + len(params), fn,
                      0, 0, rpc_seq, 0) + params
    return struct.pack("<IIII", 0xC0000001, 0x7e | (0x10de << 8) | (0x25 << 24),
                       0, qseq) + rpc


class Guest:
    """Plays NVIDIA's RM on top of a Qtest connection."""

    def __init__(self, q):
        self.q = q
        self.cmd_wp = 0
        self.cmd_seq = 0
        self.stat_wp = 0
        self.stat_seq = 0
        self.sent = []          # (function, rpc_seq)
        self.replied = []

    def shm_write(self, off, data):
        while data:
            pg, po = divmod(off, 0x1000)
            n = min(len(data), 0x1000 - po)
            self.q.mem(page_gpa(pg) + po, data[:n])
            off += n
            data = data[n:]

    def setup_pci(self):
        q = self.q
        assert q.cfg_read(0) == 0x2b8510de, hex(q.cfg_read(0))
        q.cfg_write(0x10, BAR0)
        q.cfg_write(0x14, BAR1 | 0xc)
        q.cfg_write(0x18, 0)
        q.cfg_write(0x04, 0x0006)                   # MEM + bus master

    def setup_msix(self):
        q = self.q
        q.writel(BAR0 + 0xff0000, MSI_ADDR)
        q.writel(BAR0 + 0xff0004, 0)
        q.writel(BAR0 + 0xff0008, MSI_DATA)
        q.writel(BAR0 + 0xff000c, 0)                # unmasked
        ctrl = q.cfg_read(0x40)
        q.cfg_write(0x40, ctrl | (0x8000 << 16))     # MSI-X enable

    def setup_boot_structs(self):
        q = self.q
        fmc = struct.pack("<I4xIIQQIBB2xI4xQ", 0, 1, 256, WPR_PA, 0, 0, 1, 0,
                          1, LIBOS_PA)
        fmc += b"\0" * (88 - len(fmc))
        q.mem(FMC_PA, fmc)
        q.mem(WPR_PA, struct.pack("<QQQQ", 0xdc3aae21371a60b3, 1, 0x7000000,
                                  0x900000) + b"\0" * 224)

        def region(name, pa, size):
            id8 = 0
            for c in name.encode():
                id8 = (id8 << 8) | c
            return struct.pack("<QQQBB6x", id8, pa, size, 1, 1)
        q.mem(LIBOS_PA, region("LOGINIT", LOGINIT_PA, 0x10000) +
              region("LOGRM", LOGRM_PA, 0x10000) +
              region("RMARGS", RMARGS_PA, 0x1000) + b"\0" * 32)
        q.mem(RMARGS_PA, struct.pack("<QI4xQQQQQII", PTE_PA, NPTE, CMDQ, STATQ,
                                     16, 0x1000, 0x10000, 4, 12))
        q.mem(LOGINIT_PA, struct.pack("<QQ", 0, LOGINIT_PA) + b"\x11" * 64)
        q.mem(LOGRM_PA, struct.pack("<QQ", 0, LOGRM_PA))
        ptes = b"".join(struct.pack("<Q", page_gpa(i)) for i in range(NPTE))
        q.mem(PTE_PA, ptes)
        hdr = struct.pack("<IIIIIIII", 0, QSIZE, 0x1000, 63, 0, 1, 0x20, 0x1000)
        self.shm_write(CMDQ, hdr)
        self.shm_write(STATQ, hdr)

    def fsp_cot(self):
        q = self.q
        cot = struct.pack("<HHQQIQI", 2, 860, 0x5000000, 0x6000000, 0x100000,
                          0x2000000, 0x100000)
        cot += b"\0" * (852 - len(cot)) + struct.pack("<Q", FMC_PA)
        pkt = struct.pack("<II", 0xC0000001,
                          0x7e | (0x10de << 8) | (0x14 << 24)) + cot
        pkt += b"\0" * (-len(pkt) % 4)
        q.writel(BAR0 + 0x8f2ac0, 1 << 24)            # EMEMC: offset 0, AINCW
        for i in range(0, len(pkt), 4):
            q.writel(BAR0 + 0x8f2ac4, struct.unpack_from("<I", pkt, i)[0])
        q.writel(BAR0 + 0x8f2c04, len(pkt) - 4)       # QUEUE_TAIL
        q.writel(BAR0 + 0x8f2c00, 0)                  # QUEUE_HEAD: go
        # read the response like kfspReadPacket_GH100
        head = q.readl(BAR0 + 0x8f2c80)
        tail = q.readl(BAR0 + 0x8f2c84)
        q.writel(BAR0 + 0x8f2ac0, 1 << 25)            # EMEMC: AINCR
        resp = [q.readl(BAR0 + 0x8f2ac4) for _ in range((tail - head) // 4 + 1)]
        q.writel(BAR0 + 0x8f2c84, head)
        q.writel(BAR0 + 0x8f2c80, head)
        return resp

    def send(self, fn, params, doorbell=True):
        elem = rpc_elem(fn, params, self.cmd_seq + 100, self.cmd_seq)
        nelem = (len(elem) + 0xfff) // 0x1000
        for i in range(nelem):
            slot = (self.cmd_wp + i) % 63
            self.shm_write(CMDQ + 0x1000 + slot * 0x1000,
                           elem[i * 0x1000:(i + 1) * 0x1000])
        self.sent.append((fn, self.cmd_seq + 100, nelem))
        self.cmd_seq += 1
        self.cmd_wp = (self.cmd_wp + nelem) % 63
        self.shm_write(CMDQ + 16, struct.pack("<I", self.cmd_wp))
        if doorbell:
            self.q.writel(BAR0 + 0x110c00, 0)

    def gsp_reply(self, fn, params, rpc_seq):
        elem = rpc_elem(fn, params, rpc_seq, self.stat_seq)
        self.shm_write(STATQ + 0x1000 + self.stat_wp * 0x1000, elem)
        self.replied.append((fn, rpc_seq))
        self.stat_seq += 1
        self.stat_wp = (self.stat_wp + 1) % 63
        self.shm_write(STATQ + 16, struct.pack("<I", self.stat_wp))


class Run:
    def __init__(self, name, devopts, env=None, trace_name="trace.nvt.zst",
                 accel="qtest", extra=()):
        self.dir = tempfile.mkdtemp(prefix=f"nvtrace-{name}-",
                                    dir=os.environ.get("NVTRACE_TEST_TMP"))
        # UNIX socket paths are limited to 107 bytes: keep them short.
        self.sockdir = tempfile.mkdtemp(prefix="nvt", dir="/tmp")
        self.sock = os.path.join(self.sockdir, "gpu")
        self.qmp = os.path.join(self.sockdir, "qmp")
        self.trace = os.path.join(self.dir, trace_name)
        self.srv = subprocess.Popen([sys.executable, os.path.join(HERE, "fakegpu.py"),
                                     self.sock], stderr=subprocess.PIPE, text=True)
        assert "listening" in self.srv.stderr.readline()
        dev = {"driver": "vfio-user-pci", "socket": {"path": self.sock,
                                                      "type": "unix"},
               "addr": "04.0"}
        dev.update(devopts)
        for k, v in list(dev.items()):
            if v == "@TRACE":
                dev[k] = self.trace
        e = dict(os.environ)
        e.update({"NVTRACE_GSP_POLL_US": "200", "NVTRACE_GSP_LOG_MS": "20",
                  "NVTRACE_TEST_QUIRKS": "1"})
        e.update(env or {})
        self.errf = None
        self.q = Qtest([QEMU, "-machine", "q35", "-accel", accel, "-m", "256M",
                        "-nodefaults", "-display", "none", "-qtest", "stdio",
                        "-qtest-log", "/dev/null",
                        "-qmp", f"unix:{self.qmp},server=on,wait=off",
                        "-device", json.dumps(dev)] + list(extra), e, self.dir)
        self.g = Guest(self.q)

    def finish(self, reset=False):
        if reset:
            qmp(self.qmp, "system_reset")
        qmp(self.qmp, "quit")
        self.q.p.wait(timeout=30)
        self.srv.wait(timeout=10)
        self.srv_log = self.srv.stderr.read()

    def records(self, pattern=None):
        files = sorted(f for f in os.listdir(self.dir)
                       if f.startswith("trace") and (pattern is None or pattern in f))
        return list(md.iter_records([os.path.join(self.dir, f) for f in files]))

    def cleanup(self):
        shutil.rmtree(self.sockdir, ignore_errors=True)
        if not KEEP:
            shutil.rmtree(self.dir, ignore_errors=True)
        else:
            print(f"kept {self.dir}", file=sys.stderr)


# KVM test firmware (nasm source below): enters flat 32-bit mode, then on
# request (mailbox at 0x7000: go=0x600d, dst, dwords, pattern) runs
# "rep stosd" and answers done=0xd0e. The guest's own stores are what KVM
# dirty-logs; qtest writes do not go through KVM.
#   cli; o32 lgdt [cs:gdt_desc]; cr0.PE=1; jmp dword 0x08:pm
#   pm: ds=es=ss=0x10
#   .wait: pause; cmp dword [0x7000],0x600d; jne .wait
#          mov dword [0x7000],0; edi=[0x7004]; ecx=[0x7008]; eax=[0x700c]
#          cld; rep stosd; mov dword [0x7000],0xd0e; jmp .wait
#   gdt: null, code 0x00CF9B000000FFFF, data 0x00CF93000000FFFF (accessed
#        bit preset: the GDT is in ROM and KVM would loop emulating the
#        accessed-bit write)
#   reset vector (0xfff0): jmp start
KVM_FW = bytes.fromhex(
    "fa2e660f011678000f20c00c010f22c066ea1800ffff080066b810008ed88ec08ed0f3"
    "90813d007000000d60000075f2c70500700000000000008b3d047000008b0d087000"
    "00a10c700000fcf3abc705007000000e0d0000ebc890909090909000000000000000"
    "00ffff0000009bcf00ffff00000093cf0017006000ffff")


def kvm_firmware(path):
    img = bytearray(0x10000)
    img[:len(KVM_FW)] = KVM_FW
    img[0xfff0:0xfff3] = bytes.fromhex("e90d00")          # jmp start
    with open(path, "wb") as f:
        f.write(img)
    return path


def kvm_usable():
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        return False
    try:
        fd = os.open("/dev/kvm", os.O_RDWR)
        os.close(fd)
        return True
    except OSError:
        return False


def channel_alloc_params(userd_pa):
    """NV_CHANNEL_ALLOC_PARAMS (376 bytes, 610.57.04), USERD in sysmem."""
    a = bytearray(376)
    struct.pack_into("<IIQI", a, 0, 0, 0, 0x7f0000000, 1024)
    struct.pack_into("<QQII", a, 176, userd_pa, 512, 1, 0)       # userdMem
    struct.pack_into("<QQII", a, 152, 0x4000000, 0x1000, 2, 0)   # inst: vidmem
    return bytes(a)


def by_type(recs, t):
    return [r for r in recs if r.type == t]


def notes(recs):
    return [r.data.decode() for r in recs if r.type == md.T_NOTE]


class EndToEnd(unittest.TestCase):

    def test_full_capture(self):
        """Default trap mask (BAR0), FSP COT discovery, GSP RPCs, IRQs, reset."""
        run = Run("full", {"x-nvtrace": True, "x-nvtrace-file": "@TRACE"})
        g, q = run.g, run.q
        try:
            g.setup_pci()
            self.assertEqual(q.readl(BAR0), 0x1b2000a1)
            self.assertEqual(q.readl(BAR0 + 0x88000), 0x2b8510de)  # mirror
            q.writel(BAR1 + 0x100, 0xdeadbeef)                     # mmapped
            self.assertEqual(q.readl(BAR1 + 0x100), 0xdeadbeef)
            g.setup_msix()
            g.setup_boot_structs()
            resp = g.fsp_cot()
            self.assertEqual(resp[1] >> 24, 0x15)                  # FSP_RESPONSE
            # GSP RPCs: small, then a 3-element one, then enough to wrap
            alloc = struct.pack("<IIIIIII4x", 0xc1d00001, 0xc1d00001,
                                0x5c000002, 0x2080, 0, 0, 0)
            ctrl = struct.pack("<IIIIIIIIQ", 0xc1d00001, 0x5c000002, 0x20800102,
                               0, 8, 0, 0, 0, 0) + b"\x01" * 8
            g.send(FN["GSP_SET_SYSTEM_INFO"], b"\x22" * 200)
            g.send(FN["SET_REGISTRY"], b"\x33" * 9000)              # 3 elements
            g.send(FN["GSP_RM_ALLOC"], alloc)
            g.send(FN["GSP_RM_CONTROL"], ctrl)
            for i in range(70):                                    # wraps 63
                g.send(FN["FREE"], struct.pack("<IIII", 1, 2, 0x100 + i, 0),
                       doorbell=(i % 5 == 4))
            q.writel(BAR0 + 0x110c00, 0)
            # GSP side: events + replies, then an interrupt
            g.gsp_reply(FN["INIT_DONE"], b"", 0)
            g.gsp_reply(FN["GSP_RM_ALLOC"], alloc, 102)
            g.gsp_reply(FN["RUN_CPU_SEQUENCER"], b"\x44" * 64, 0)
            q.writel(BAR0 + 0xfff000, 0)                           # MSI-X 0
            for _ in range(200):
                if q.readl(MSI_ADDR) == MSI_DATA:
                    break
                time.sleep(0.01)
            self.assertEqual(q.readl(MSI_ADDR), MSI_DATA, "MSI not delivered")
            # log buffer put pointer moves -> snapshot
            q.mem(LOGRM_PA, struct.pack("<Q", 5))
            # LO/HI register pair pointing at guest RAM -> pair snapshot
            q.mem(0x1050000, b"PAIRDATA" * 8)
            q.writel(BAR0 + 0x100e40, 0x01050000)
            q.writel(BAR0 + 0x100e44, 0)
            time.sleep(0.3)
        finally:
            run.finish(reset=True)
        try:
            recs = run.records()
            self.check_full(run, recs, g)
        finally:
            run.cleanup()

    def check_full(self, run, recs, g):
        nt = notes(recs)
        self.assertFalse(by_type(recs, md.T_LOST), "lost records")
        dev = by_type(recs, md.T_DEV)
        self.assertEqual((dev[0].f["vendor"], dev[0].f["device"]), (0x10de, 0x2b85))
        self.assertEqual(dev[0].f["trap_mask"], 1)
        self.assertTrue(any("BAR0" in n and "trapped" in n for n in nt), nt)
        self.assertFalse(any("BAR1" in n and "trapped" in n for n in nt))
        bars = {}
        for r in by_type(recs, md.T_BAR):
            if r.f["new"] != (1 << 64) - 1:
                bars.setdefault(r.f["bar"], r.f["new"])
        self.assertTrue(any(r.f["new"] == (1 << 64) - 1
                            for r in by_type(recs, md.T_BAR)), "reset unmap")
        self.assertEqual(bars.get(0), BAR0)
        self.assertEqual(bars.get(1), BAR1)
        self.assertTrue(by_type(recs, md.T_CFG))
        mmio = by_type(recs, md.T_MMIO)
        self.assertTrue(any(r.f["bar"] == 0 and r.f["off"] == 0 and
                            r.f["val"] == 0x1b2000a1 and r.f["origin"] == 0
                            for r in mmio))
        self.assertTrue(any(r.f["off"] == 0x88000 and r.f["origin"] == 1 and
                            r.f["val"] == 0x2b8510de for r in mmio))
        self.assertTrue(any(r.f["off"] == 0x88000 and r.f["origin"] == 2
                            for r in mmio))
        self.assertTrue(any(r.f["origin"] == 5 and r.f["write"] for r in mmio))
        self.assertFalse(any(r.f["bar"] == 1 for r in mmio), "BAR1 was trapped")
        # doorbells and FSP register writes are all there
        bells = [r for r in mmio if r.f["off"] == 0x110c00 and r.f["write"]]
        self.assertEqual(len(bells), 4 + 14 + 1)
        # interrupts
        irqs = by_type(recs, md.T_IRQ)
        self.assertTrue(any(r.f["kind"] == 2 and r.f["vector"] == 0 and
                            r.f["data"] == MSI_DATA for r in irqs))
        self.assertTrue(any(r.f["event"] == 0 for r in by_type(recs, md.T_VEC)))
        # FSP
        fsp = list(md.fsp_messages(by_type(recs, md.T_FSP_PKT)))
        self.assertEqual([d for d, _, _ in fsp], [0, 1])
        self.assertEqual(md.describe_fsp(fsp[0][2])[0], "COT")
        self.assertEqual(md.describe_fsp(fsp[1][2])[0], "FSP_RESPONSE")
        self.assertTrue(any("gspBootArgsSysmemOffset=0x1000000" in n for n in nt))
        # discovery + snapshots
        qi = by_type(recs, md.T_GSP_QINFO)
        self.assertEqual(qi[0].f["shared_pa"], PTE_PA)
        self.assertEqual(qi[0].f["source"], 1)
        self.assertEqual((qi[0].f["cmd_size"], qi[0].f["stat_size"]), (QSIZE, QSIZE))
        labels = {r.f["label"]: r for r in by_type(recs, md.T_DMA_SNAP)}
        for lab in ("GSP_FMC_BOOT_PARAMS", "GSP_WPR_META", "LIBOS_INIT_ARGS",
                    "GSP_ARGUMENTS_CACHED", "GSP_QUEUE_PTES", "GSPLOG:LOGINIT",
                    "GSPLOG:LOGRM"):
            self.assertIn(lab, labels)
        self.assertEqual(struct.unpack_from("<Q", labels["GSP_WPR_META"].data)[0],
                         0xdc3aae21371a60b3)
        self.assertEqual(len([r for r in by_type(recs, md.T_DMA_SNAP)
                              if r.f["label"] == "GSPLOG:LOGRM"]), 2)
        pair = [r for r in by_type(recs, md.T_DMA_SNAP)
                if r.f["label"].startswith("pair:100e40")]
        self.assertTrue(pair and pair[0].data.startswith(b"PAIRDATA"), labels.keys())
        # GSP messages: every command exactly once, in order, decoded
        msgs = by_type(recs, md.T_GSP_MSG)
        cmds = [md.parse_rpc(r) for r in msgs if r.f["queue"] == 0]
        self.assertEqual([(c["function"], c["rpc_seq"]) for c in cmds],
                         [(f, s) for f, s, _ in g.sent])
        big = [r for r in msgs if r.f["queue"] == 0 and r.f["nelem"] == 3]
        self.assertEqual(len(big), 1)
        self.assertEqual(md.parse_rpc(big[0])["params"], b"\x33" * 9000)
        self.assertFalse(any(r.f["gflags"] for r in msgs), "flags set")
        stats = [md.parse_rpc(r) for r in msgs if r.f["queue"] == 1]
        self.assertEqual([(s["function"], s["rpc_seq"]) for s in stats], g.replied)
        names = md.Names()
        descs = [md.describe_rpc(names, c) for c in cmds]
        self.assertIn(("GSP_RM_ALLOC", ), [(d[0],) for d in descs])
        self.assertTrue(any("NV20_SUBDEVICE_0" in d[1] for d in descs))
        self.assertTrue(any("NV2080_CTRL_CMD_GPU_GET_INFO_V2" in d[1] for d in descs))
        # status messages are logged before the IRQ that announced them? No:
        # IRQ first, then the drain it triggers (or earlier via the poller).
        self.assertTrue(by_type(recs, md.T_GSP_QSTATE))
        resets = [r.f["kind"] for r in by_type(recs, md.T_RESET)]
        self.assertIn(0, resets)
        self.assertIn(1, resets)
        self.assertTrue(any("shutdown" in n for n in nt))
        # the decoder runs over the whole file
        out = subprocess.run([sys.executable, os.path.join(HERE, "mmiodump.py"),
                              "summary", run.trace], capture_output=True,
                             text=True, check=True).stdout
        self.assertIn("NV_PGSP_QUEUE_HEAD(0)", out)
        self.assertIn("GSP_RM_CONTROL", out)

    def test_trap_bar1_and_rotation(self):
        """x-trap-bars=0x3 traps BAR1 too; NVTRACE_ROTATE_MB splits files."""
        run = Run("bar1", {"x-nvtrace": True, "x-nvtrace-file": "@TRACE",
                           "x-trap-bars": 3}, env={"NVTRACE_ROTATE_MB": "1"},
                  trace_name="trace.nvt")
        g, q = run.g, run.q
        try:
            g.setup_pci()
            for i in range(24000):
                q.writel(BAR1 + 0x1000 + 4 * (i % 1024), i)
            self.assertEqual(q.readl(BAR1 + 0x1000 + 4 * 1023), 23551)
        finally:
            run.finish()
        try:
            files = [f for f in os.listdir(run.dir) if f.startswith("trace.")]
            self.assertGreaterEqual(len(files), 1)
            recs = run.records()
            self.assertTrue(len(files) >= 1)
            self.assertIn("trace.nvt.000", files)
            self.assertIn("trace.nvt.001", files)
            seqs = [r.seq for r in recs]
            self.assertEqual(seqs, list(range(seqs[0], seqs[0] + len(seqs))))
            bar1 = [r for r in by_type(recs, md.T_MMIO) if r.f["bar"] == 1]
            self.assertEqual(len([r for r in bar1 if r.f["write"]]), 24000)
            self.assertTrue(any("BAR1" in n and "trapped" in n for n in notes(recs)))
        finally:
            run.cleanup()

    def test_ram_scan_fallback(self):
        """No FSP COT: the first doorbell triggers a guest RAM scan."""
        run = Run("scan", {"x-nvtrace": True, "x-nvtrace-file": "@TRACE"},
                  trace_name="trace.nvt.gz")
        g, q = run.g, run.q
        try:
            g.setup_pci()
            g.setup_boot_structs()
            g.send(FN["GSP_RM_CONTROL"], b"\0" * 48)
            time.sleep(0.5)
            g.send(FN["FREE"], b"\0" * 16)
            g.gsp_reply(FN["INIT_DONE"], b"", 0)
            time.sleep(0.3)
        finally:
            run.finish()
        try:
            recs = run.records()
            qi = by_type(recs, md.T_GSP_QINFO)
            self.assertTrue(qi, notes(recs))
            self.assertEqual(qi[0].f["source"], 3)
            self.assertEqual(qi[0].f["shared_pa"], PTE_PA)
            msgs = by_type(recs, md.T_GSP_MSG)
            self.assertEqual([(r.f["queue"], md.parse_rpc(r)["function"])
                              for r in msgs],
                             [(0, FN["GSP_RM_CONTROL"]), (0, FN["FREE"]),
                              (1, FN["INIT_DONE"])])
        finally:
            run.cleanup()

    def test_follow_rpc_buffers(self):
        """Buffers named by GSP RPCs are captured: ALLOC_MEMORY pages, channel
        USERD (re-read on the usermode doorbell), display pushbuffer (logged
        on PUT writes); FREE stops watching."""
        run = Run("follow", {"x-nvtrace": True, "x-nvtrace-file": "@TRACE"},
                  env={"NVTRACE_WATCH_MS": "0", "NVTRACE_WATCH_HOT_US": "0"})
        g, q = run.g, run.q
        hc, hdev, hch, hmem = 0xc1d00001, 0xde000001, 0xc4a00001, 0x3e000001
        USERD, PB = 0x1110000, 0x1120000
        m1 = struct.pack("<III", (2 << 18) | 0x2064, 0x04380780, 1)
        m2 = struct.pack("<II", (1 << 18) | 0x0200, 1)
        try:
            g.setup_pci()
            g.setup_boot_structs()
            g.fsp_cot()
            q.mem(0x1100000, b"MEMPAGE0" * 512)
            q.mem(0x1102000, b"MEMPAGE1" * 512)
            mem = struct.pack("<7I4xQI4xI4x", hc, hdev, hmem, 0x81, 0, 0, 0,
                              0x2000, 2, 0) + struct.pack("<QQ", 0x1100, 0x1102)
            g.send(FN_ALLOC_MEMORY, mem)
            q.mem(USERD, b"U" * 512)
            chan = channel_alloc_params(USERD)
            g.send(FN["GSP_RM_ALLOC"], struct.pack(
                "<IIIIIII4x", hc, hdev, hch, 0xc96f, 0, len(chan), 0) + chan)
            pbp = struct.pack("<I4xQQIIIB3xIII4x", 1, PB, 0xfff, 0, 0xca7d, 0,
                              1, 0, 0, 0)
            g.send(FN["GSP_RM_CONTROL"], struct.pack(
                "<IIIIIIIIQ", hc, 0xcafe0002, 0x20800a58, 0, len(pbp), 0, 0, 0,
                0) + pbp)
            q.mem(PB, m1)
            q.writel(BAR0 + 0x680000, len(m1))               # core PUT
            q.mem(PB + len(m1), m2)
            q.writel(BAR0 + 0x680000, len(m1) + len(m2))
            q.mem(USERD + 0x8c, struct.pack("<I", 0x40))      # GP_PUT moves
            q.writel(BAR0 + 0xbb0090, 0x40000005)             # doorbell
            time.sleep(0.3)
            g.send(FN["FREE"], struct.pack("<IIII", hc, hdev, hch, 0))
            q.mem(USERD + 0x8c, struct.pack("<I", 0x80))
            q.writel(BAR0 + 0xbb0090, 0x40000005)
            time.sleep(0.3)
        finally:
            run.finish()
        try:
            recs = run.records()
            snaps = by_type(recs, md.T_DMA_SNAP)
            lab = {}
            for r in snaps:
                lab.setdefault(r.f["label"], []).append(r)
            self.assertEqual([r.data[:8] for r in lab["mem3e000001"]],
                             [b"MEMPAGE0", b"MEMPAGE1"])
            self.assertEqual(lab["chc4a00001.userd"][0].data, b"U" * 512)
            self.assertTrue(any("pushbuffer sysmem 0x1120000 size 0x1000" in n
                                for n in notes(recs)), notes(recs))
            self.assertEqual(lab["dispPB:0@0"][0].data, m1)
            self.assertEqual(lab[f"dispPB:0@{len(m1):x}"][0].data, m2)
            w = lab["w:chc4a00001.userd"]
            self.assertEqual(len(w), 1, "USERD re-read once (FREE stops it)")
            self.assertEqual(struct.unpack_from("<I", w[0].data, 0x8c)[0], 0x40)
            self.assertTrue(w[0].f["sflags"] & md.SNAP_CHANGED)
            out = subprocess.run([sys.executable, os.path.join(HERE, "mmiodump.py"),
                                  "disp", run.trace], capture_output=True,
                                 text=True, check=True).stdout
            self.assertIn("NVCA7D_HEAD_SET_RASTER_SIZE(0) = 0x04380780", out)
            self.assertIn("NVCA7D_UPDATE = 0x00000001", out)
            out = subprocess.run([sys.executable, os.path.join(HERE, "mmiodump.py"),
                                  "rmlog", run.trace, "--tree"],
                                 capture_output=True, text=True,
                                 check=True).stdout
            self.assertIn("BLACKWELL_CHANNEL_GPFIFO_A", out)
            self.assertIn("NV2080_CTRL_CMD_INTERNAL_DISPLAY_CHANNEL_PUSHBUFFER",
                          out)
        finally:
            run.cleanup()

    def test_trap_windows(self):
        """x-trap-bars=0 + x-trap-windows: only the windows trap and trace."""
        run = Run("win", {"x-nvtrace": True, "x-nvtrace-file": "@TRACE",
                          "x-trap-bars": 0,
                          "x-trap-windows": "0:0x110000+0x1000,1:0x10000+0x1000"},
                  trace_name="trace.nvt")
        g, q = run.g, run.q
        try:
            g.setup_pci()
            q.writel(BAR0 + 0x110c00, 0x11)
            q.writel(BAR0 + 0x200, 0x22)
            q.writel(BAR1 + 0x10010, 0x33)
            q.writel(BAR1 + 0x20000, 0x44)
            self.assertEqual(q.readl(BAR0 + 0x200), 0x22)
            self.assertEqual(q.readl(BAR1 + 0x10010), 0x33)
            self.assertEqual(q.readl(BAR1 + 0x20000), 0x44)
        finally:
            run.finish()
        try:
            recs = run.records()
            mm = {(r.f["bar"], r.f["off"]) for r in by_type(recs, md.T_MMIO)
                  if r.f["origin"] == 0}
            self.assertIn((0, 0x110c00), mm)
            self.assertIn((1, 0x10010), mm)
            self.assertNotIn((0, 0x200), mm)
            self.assertNotIn((1, 0x20000), mm)
            nt = notes(recs)
            self.assertTrue(any("BAR0 window 0x110000+0x1000 trapped" in n
                                for n in nt), nt)
            self.assertTrue(any("BAR1 window 0x10000+0x1000 trapped" in n
                                for n in nt), nt)
        finally:
            run.cleanup()

    @unittest.skipUnless(kvm_usable(), "needs /dev/kvm")
    def test_dirty_bar_pages_kvm(self):
        """KVM dirty log on mmapped BAR1: guest CPU stores become BAR_PAGE
        records, unchanged rewrites are not logged again."""
        fwdir = tempfile.mkdtemp(prefix="nvt-fw-")
        fw = kvm_firmware(os.path.join(fwdir, "fw.bin"))
        run = Run("dirty", {"x-nvtrace": True, "x-nvtrace-file": "@TRACE",
                            "x-nvtrace-dirty-bars": 2},
                  env={"NVTRACE_DIRTY_MS": "50"}, accel="kvm",
                  extra=["-bios", fw, "-smp", "1"])
        g, q = run.g, run.q

        def fill(off, dwords, pattern):
            q.writel(0x7004, BAR1 + off)
            q.writel(0x7008, dwords)
            q.writel(0x700c, pattern)
            q.writel(0x7000, 0x600d)
            for _ in range(500):
                if q.readl(0x7000) == 0xd0e:
                    break
                time.sleep(0.01)
            self.assertEqual(q.readl(0x7000), 0xd0e, "guest did not run")
            time.sleep(0.4)                     # > 2 dirty collections

        try:
            g.setup_pci()
            time.sleep(0.3)                     # first (discarded) collection
            fill(0x5000, 2048, 0xa5a5a5a5)      # two pages
            fill(0x5000, 2048, 0xa5a5a5a5)      # same content again
            fill(0x6000, 1024, 0x5a5a5a5a)      # one page changes
        finally:
            run.finish()
            shutil.rmtree(fwdir, ignore_errors=True)
        try:
            recs = run.records()
            pages = [(r.f["off"], r.data[:4]) for r in by_type(recs, md.T_BAR_PAGE)]
            self.assertEqual(pages, [(0x5000, b"\xa5" * 4), (0x6000, b"\xa5" * 4),
                                     (0x6000, b"\x5a" * 4)], notes(recs))
            self.assertTrue(all(r.f["bar"] == 1 for r in by_type(recs, md.T_BAR_PAGE)))
            self.assertTrue(any("dirty-tracked" in n for n in notes(recs)))
        finally:
            run.cleanup()

    def test_no_trace_unchanged(self):
        """Without x-nvtrace the device works and nothing is written."""
        run = Run("off", {})
        g, q = run.g, run.q
        try:
            g.setup_pci()
            self.assertEqual(q.readl(BAR0), 0x1b2000a1)
            q.writel(BAR0 + 0x200, 0x1234)
            self.assertEqual(q.readl(BAR0 + 0x200), 0x1234)
        finally:
            run.finish()
        try:
            self.assertFalse([f for f in os.listdir(run.dir) if f.startswith("trace")])
            # BAR0 was mmapped: the server saw no BAR0 region accesses
            self.assertIn("'read': ", run.srv_log)
        finally:
            run.cleanup()


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--qemu", default=QEMU)
    ap.add_argument("--keep", action="store_true")
    a, rest = ap.parse_known_args()
    QEMU = a.qemu
    KEEP = a.keep
    unittest.main(argv=[sys.argv[0]] + rest)

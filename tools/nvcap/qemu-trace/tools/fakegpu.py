#!/usr/bin/env python3
"""Minimal vfio-user server that pretends to be an RTX 5090 (10de:2b85).

Only for testing the nvtrace QEMU without the real GPU: QEMU's vfio-user-pci
client uses the same hw/vfio/pci.c + region.c code as vfio-pci, so the
x-trap-bars, trace and NVIDIA quirk paths run for real.

Device model:
  BAR0  16 MiB register file (32-bit mem), MMAP-capable (memfd) so that
        x-trap-bars decides whether it is trapped. Emulates:
          NV_PMC_BOOT_0 (0x0) = 0x1b2000a1
          FSP EMEM channel 0 (EMEMC/EMEMD auto-increment), QUEUE_HEAD/TAIL,
          MSGQ_HEAD/TAIL: every CPU->FSP packet gets an NVDM FSP_RESPONSE.
          0xfff000: write N -> raise MSI-X vector N (or INTx if MSI-X off).
        MSI-X table at BAR0+0xff0000, PBA at +0xff1000 (QEMU emulates them).
  BAR1  64 MiB (64-bit prefetchable), MMAP-capable memfd ("VRAM").
  config 256 bytes, MSI-X capability with 4 vectors, INTA.
Logs every request it serves to stderr with -v.

Usage: fakegpu.py SOCKET [-v]
"""
import array
import mmap
import os
import socket
import struct
import sys

CMD_VERSION, CMD_DMA_MAP, CMD_DMA_UNMAP, CMD_GET_INFO, CMD_GET_REGION_INFO, \
    CMD_GET_REGION_IO_FDS, CMD_GET_IRQ_INFO, CMD_SET_IRQS, CMD_REGION_READ, \
    CMD_REGION_WRITE, CMD_DMA_READ, CMD_DMA_WRITE, CMD_RESET = range(1, 14)
REPLY, NO_REPLY, ERROR = 0x1, 0x10, 0x20
HDR = struct.Struct("<HHIII")

R_READ, R_WRITE, R_MMAP = 1, 2, 4
BAR0_SIZE = 16 << 20
BAR1_SIZE = 64 << 20
CFG_SIZE = 256

EMEMC0, EMEMD0 = 0x8f2ac0, 0x8f2ac4
QHEAD0, QTAIL0, MHEAD0, MTAIL0 = 0x8f2c00, 0x8f2c04, 0x8f2c80, 0x8f2c84
TEST_IRQ = 0xfff000

IRQ_INTX, IRQ_MSI, IRQ_MSIX, IRQ_ERR, IRQ_REQ = range(5)
SET_DATA_NONE, SET_DATA_BOOL, SET_DATA_EVENTFD = 1, 2, 4
SET_ACTION_TRIGGER = 32


class FakeGpu:
    def __init__(self, verbose=False):
        self.verbose = verbose
        self.bar0_fd = os.memfd_create("fakegpu-bar0")
        os.ftruncate(self.bar0_fd, BAR0_SIZE)
        self.bar0 = mmap.mmap(self.bar0_fd, BAR0_SIZE)
        self.bar1_fd = os.memfd_create("fakegpu-bar1")
        os.ftruncate(self.bar1_fd, BAR1_SIZE)
        self.bar1 = mmap.mmap(self.bar1_fd, BAR1_SIZE)
        self.cfg = bytearray(CFG_SIZE)
        struct.pack_into("<HH", self.cfg, 0, 0x10de, 0x2b85)
        struct.pack_into("<H", self.cfg, 6, 0x0010)          # status: caps
        struct.pack_into("<I", self.cfg, 8, 0x030000a1)      # VGA, rev a1
        struct.pack_into("<I", self.cfg, 0x10, 0x00000000)   # BAR0 mem32
        struct.pack_into("<I", self.cfg, 0x14, 0x0000000c)   # BAR1 mem64 pref
        struct.pack_into("<HH", self.cfg, 0x2c, 0x10de, 0x1234)
        self.cfg[0x34] = 0x40
        self.cfg[0x3d] = 1                                   # INTA
        # MSI-X cap: 4 vectors, table BAR0+0xff0000, PBA BAR0+0xff1000
        struct.pack_into("<BBHII", self.cfg, 0x40, 0x11, 0, 3,
                         0x00ff0000, 0x00ff1000)
        struct.pack_into("<I", self.bar0, 0, 0x1b2000a1)
        self.emem = [0] * 1024
        self.emem_ptr = 0
        self.aincw = self.aincr = False
        self.qtail = 0
        self.irq_fds = {}
        self.stats = {"read": 0, "write": 0, "reset": 0, "irq": 0}

    def log(self, msg):
        if self.verbose:
            print("fakegpu:", msg, file=sys.stderr)

    # ---- device behaviour ----
    def bar0_read(self, off, n):
        if off == EMEMD0 and n == 4:
            v = self.emem[self.emem_ptr] if self.emem_ptr < 1024 else 0
            if self.aincr:
                self.emem_ptr += 1
            return struct.pack("<I", v)
        return self.bar0[off:off + n]

    def bar0_write(self, off, data):
        n = len(data)
        val = int.from_bytes(data, "little")
        if off == EMEMC0:
            self.emem_ptr = ((val >> 8) & 0xff) * 64 + ((val >> 2) & 0x3f)
            self.aincw = bool(val >> 24 & 1)
            self.aincr = bool(val >> 25 & 1)
        elif off == EMEMD0:
            if self.emem_ptr < 1024:
                self.emem[self.emem_ptr] = val
            if self.aincw:
                self.emem_ptr += 1
            return
        elif off == QTAIL0:
            self.qtail = val
        elif off == QHEAD0:
            # FSP consumed the packet: answer with an FSP_RESPONSE
            cmd = self.emem[1] >> 24
            resp = [0xC0000001, 0x7e | (0x10de << 8) | (0x15 << 24), cmd, 0]
            for i, v in enumerate(resp):
                self.emem[i] = v
            struct.pack_into("<II", self.bar0, MHEAD0, 0, len(resp) * 4 - 4)
            struct.pack_into("<I", self.bar0, QHEAD0, val)
            struct.pack_into("<I", self.bar0, QTAIL0, val)  # head == tail: done
            self.log(f"FSP packet nvdm type 0x{cmd:x} answered")
            return
        elif off == TEST_IRQ:
            self.raise_irq(val)
            return
        self.bar0[off:off + n] = data

    def raise_irq(self, vector):
        fd = self.irq_fds.get((IRQ_MSIX, vector)) or self.irq_fds.get((IRQ_INTX, 0))
        self.log(f"raise irq {vector} fd={fd}")
        if fd is not None:
            os.write(fd, struct.pack("<Q", 1))
            self.stats["irq"] += 1

    def cfg_write(self, off, data):
        # Writable: command, BARs (QEMU emulates them anyway), MSI-X control.
        for i, b in enumerate(data):
            o = off + i
            if o in (4, 5) or 0x10 <= o < 0x28 or o in (0x42, 0x43) or o == 0x3c:
                self.cfg[o] = b

    # ---- protocol ----
    def region_info(self, idx):
        # (flags, size, offset, fd)
        if idx == 0:
            return R_READ | R_WRITE | R_MMAP, BAR0_SIZE, 0, self.bar0_fd
        if idx == 1:
            return R_READ | R_WRITE | R_MMAP, BAR1_SIZE, 0, self.bar1_fd
        if idx == 7:
            return R_READ | R_WRITE, CFG_SIZE, 0, None
        return 0, 0, 0, None

    def handle(self, cmd, body, fds):
        if cmd == CMD_VERSION:
            return struct.pack("<HH", 0, 0), None
        if cmd in (CMD_DMA_MAP, CMD_DMA_UNMAP):
            for fd in fds:
                os.close(fd)
            if cmd == CMD_DMA_UNMAP:
                return body[:24], None
            return b"", None
        if cmd == CMD_GET_INFO:
            argsz, flags, nreg, nirq = struct.unpack_from("<IIII", body)
            return struct.pack("<IIII", 16, 0x3, 9, 5), None   # RESET|PCI
        if cmd == CMD_GET_REGION_INFO:
            argsz, flags, idx = struct.unpack_from("<III", body)
            fl, size, off, fd = self.region_info(idx)
            reply = struct.pack("<IIIIQQ", 32, fl, idx, 0, size, off)
            return reply, ([fd] if fd is not None else None)
        if cmd == CMD_GET_IRQ_INFO:
            argsz, flags, idx = struct.unpack_from("<III", body)
            info = {IRQ_INTX: (0x7, 1), IRQ_MSI: (0x1, 0), IRQ_MSIX: (0x9, 4),
                    IRQ_ERR: (0x1, 0), IRQ_REQ: (0x1, 0)}.get(idx, (0, 0))
            return struct.pack("<IIII", 16, info[0], idx, info[1]), None
        if cmd == CMD_SET_IRQS:
            argsz, flags, idx, start, count = struct.unpack_from("<IIIII", body)
            if flags & SET_DATA_EVENTFD and flags & SET_ACTION_TRIGGER:
                for i in range(count):
                    old = self.irq_fds.pop((idx, start + i), None)
                    if old is not None:
                        os.close(old)
                    if i < len(fds):
                        self.irq_fds[(idx, start + i)] = fds[i]
            elif flags & SET_DATA_NONE and flags & SET_ACTION_TRIGGER and \
                    count == 0:
                for k in [k for k in self.irq_fds if k[0] == idx]:
                    os.close(self.irq_fds.pop(k))
            else:
                for fd in fds:
                    os.close(fd)
            self.log(f"SET_IRQS idx={idx} start={start} count={count} "
                     f"flags=0x{flags:x} fds={len(fds)}")
            return b"", None
        if cmd == CMD_REGION_READ:
            off, region, count = struct.unpack_from("<QII", body)
            self.stats["read"] += 1
            if region == 0:
                data = self.bar0_read(off, count)
            elif region == 1:
                data = self.bar1[off:off + count]
            elif region == 7:
                data = bytes(self.cfg[off:off + count])
            else:
                return None, None
            return struct.pack("<QII", off, region, len(data)) + bytes(data), None
        if cmd == CMD_REGION_WRITE:
            off, region, count = struct.unpack_from("<QII", body)
            data = body[16:16 + count]
            self.stats["write"] += 1
            if region == 0:
                self.bar0_write(off, data)
            elif region == 1:
                self.bar1[off:off + count] = data
            elif region == 7:
                self.cfg_write(off, data)
            else:
                return None, None
            return struct.pack("<QII", off, region, count), None
        if cmd == CMD_RESET:
            self.stats["reset"] += 1
            self.log("device reset")
            return b"", None
        return None, None


def recv_msg(conn):
    fds = array.array("i")
    msg, anc, _, _ = conn.recvmsg(HDR.size, socket.CMSG_LEN(16 * fds.itemsize))
    if not msg:
        return None
    while len(msg) < HDR.size:
        more = conn.recv(HDR.size - len(msg))
        if not more:
            return None
        msg += more
    for level, typ, data in anc:
        if level == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
            fds.frombytes(data[:len(data) - len(data) % fds.itemsize])
    mid, cmd, size, flags, err = HDR.unpack(msg)
    body = b""
    while len(body) < size - HDR.size:
        chunk = conn.recv(size - HDR.size - len(body))
        if not chunk:
            return None
        body += chunk
    return mid, cmd, flags, body, list(fds)


def serve(path, verbose):
    if os.path.exists(path):
        os.unlink(path)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(path)
    s.listen(1)
    print("fakegpu: listening", file=sys.stderr, flush=True)
    s.settimeout(30)                 # never hang when QEMU does not connect
    try:
        conn, _ = s.accept()
    except socket.timeout:
        print("fakegpu: no client within 30 s", file=sys.stderr)
        return
    conn.settimeout(None)
    dev = FakeGpu(verbose)
    while True:
        m = recv_msg(conn)
        if m is None:
            break
        mid, cmd, flags, body, fds = m
        reply, rfds = dev.handle(cmd, body, fds)
        if flags & NO_REPLY:
            continue
        if reply is None:
            hdr = HDR.pack(mid, cmd, HDR.size, REPLY | ERROR, 22)  # EINVAL
            conn.sendall(hdr)
            continue
        hdr = HDR.pack(mid, cmd, HDR.size + len(reply), REPLY, 0)
        if rfds:
            socket.send_fds(conn, [hdr + reply], rfds)
        else:
            conn.sendall(hdr + reply)
    print(f"fakegpu: done {dev.stats}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    serve(sys.argv[1], "-v" in sys.argv[2:])

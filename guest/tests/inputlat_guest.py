#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Guest half of host/latency/inputlat.py (Linux guest, run as root).

Records relative X motion and key presses from the named evdev devices,
with the kernel's timestamp of each event and the time read() returned it,
both on the host's CLOCK_REALTIME: ptp_kvm's cross timestamp
(PTP_SYS_OFFSET_PRECISE, the KVM clock-pairing hypercall) gives the host's
realtime next to the guest's, refreshed every half second.

  inputlat_guest.py --seconds 20 --match "Conduit Mouse" --match "Conduit Keyboard" > guest.csv
  inputlat_guest.py --list
"""

import argparse
import ctypes
import fcntl
import glob
import os
import select
import struct
import subprocess
import sys
import time

EV_SYN, EV_KEY, EV_REL = 0, 1, 2
REL_X = 0
EVENT = struct.Struct("llHHi")  # struct input_event on 64-bit


def _IOC(d, t, nr, size):
    return (d << 30) | (size << 16) | (ord(t) << 8) | nr


EVIOCSCLOCKID = _IOC(1, "E", 0xA0, 4)


def EVIOCGNAME(n):
    return _IOC(2, "E", 0x06, n)


class PtpTime(ctypes.Structure):
    _fields_ = [("sec", ctypes.c_int64), ("nsec", ctypes.c_uint32), ("rsv", ctypes.c_uint32)]


class PtpSysOffsetPrecise(ctypes.Structure):
    _fields_ = [("device", PtpTime), ("sys_realtime", PtpTime), ("sys_monoraw", PtpTime),
                ("rsv", ctypes.c_uint32 * 4)]


PTP_SYS_OFFSET_PRECISE = _IOC(3, "=", 8, ctypes.sizeof(PtpSysOffsetPrecise))


class PtpSysOffsetExtended(ctypes.Structure):
    _fields_ = [("n_samples", ctypes.c_uint32), ("clockid", ctypes.c_int32),
                ("rsv", ctypes.c_uint32 * 2), ("ts", PtpTime * 3 * 25)]


PTP_SYS_OFFSET_EXTENDED = _IOC(3, "=", 9, ctypes.sizeof(PtpSysOffsetExtended))


def ns(t):
    return t.sec * 1_000_000_000 + t.nsec


def ptp_kvm_dev():
    subprocess.run(["modprobe", "ptp_kvm"], check=False)
    for d in sorted(glob.glob("/sys/class/ptp/ptp*")):
        try:
            if open(os.path.join(d, "clock_name")).read().strip() == "KVM virtual PTP":
                return "/dev/" + os.path.basename(d)
        except OSError:
            pass
    raise SystemExit("no ptp_kvm clock (modprobe ptp_kvm; needs the host on the TSC clocksource)")


def offset_ns(fd):
    """host CLOCK_REALTIME - guest CLOCK_REALTIME.

    The precise cross timestamp when the guest runs on kvm-clock; otherwise
    the hypercall's reading between two guest readings, the tightest of 10
    (its error is half that window, a few microseconds)."""
    p = PtpSysOffsetPrecise()
    try:
        fcntl.ioctl(fd, PTP_SYS_OFFSET_PRECISE, p)
        return ns(p.device) - ns(p.sys_realtime)
    except OSError:
        pass
    try:
        e = PtpSysOffsetExtended()
        e.n_samples = 10
        e.clockid = time.CLOCK_REALTIME
        fcntl.ioctl(fd, PTP_SYS_OFFSET_EXTENDED, e)
        samples = [tuple(ns(e.ts[i][j]) for j in range(3)) for i in range(e.n_samples)]
    except OSError:
        # Read the PTP clock itself (a dynamic POSIX clock) between two
        # guest readings.
        clk = ((~fd) << 3) | 3
        samples = []
        for _ in range(10):
            pre = time.clock_gettime_ns(time.CLOCK_REALTIME)
            dev = time.clock_gettime_ns(clk)
            post = time.clock_gettime_ns(time.CLOCK_REALTIME)
            samples.append((pre, dev, post))
    best = min(samples, key=lambda t: t[2] - t[0])
    return best[1] - (best[0] + best[2]) // 2


def dev_name(fd):
    buf = bytearray(256)
    n = fcntl.ioctl(fd, EVIOCGNAME(len(buf)), buf)
    return bytes(buf[: max(0, n - 1)]).decode(errors="replace") if isinstance(n, int) else ""


def devices():
    out = []
    for p in sorted(glob.glob("/dev/input/event*")):
        try:
            fd = os.open(p, os.O_RDONLY | os.O_NONBLOCK)
        except OSError:
            continue
        out.append((p, fd, dev_name(fd)))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--seconds", type=float, default=20)
    ap.add_argument("--match", action="append", default=[],
                    help="record devices whose name contains this (repeatable)")
    ap.add_argument("--list", action="store_true")
    args = ap.parse_args()
    devs = devices()
    if args.list:
        for p, fd, n in devs:
            print(p, n)
        return
    use = [(p, fd, n) for p, fd, n in devs if any(m in n for m in args.match)]
    for p, fd, n in devs:
        if (p, fd, n) not in use:
            os.close(fd)
    if not use:
        raise SystemExit("no device matched; --list shows them")
    for p, fd, n in use:
        fcntl.ioctl(fd, EVIOCSCLOCKID, struct.pack("i", time.CLOCK_REALTIME))
        print(f"recording {p} ({n})", file=sys.stderr)
    ptp = os.open(ptp_kvm_dev(), os.O_RDWR)
    off = offset_ns(ptp)
    off_at = time.monotonic()
    print(f"host - guest clock: {off} ns", file=sys.stderr)
    names = {fd: n.replace(",", " ") for _, fd, n in use}
    out = sys.stdout
    out.write("kind,ev_ns,read_ns,value,device\n")
    end = time.monotonic() + args.seconds
    poll = select.poll()
    for _, fd, _ in use:
        poll.register(fd, select.POLLIN)
    while time.monotonic() < end:
        for fd, _ in poll.poll(100):
            data = os.read(fd, EVENT.size * 64)
            rd = time.clock_gettime_ns(time.CLOCK_REALTIME) + off
            for i in range(0, len(data) - EVENT.size + 1, EVENT.size):
                sec, usec, ty, code, val = EVENT.unpack_from(data, i)
                ev = sec * 1_000_000_000 + usec * 1000 + off
                if ty == EV_REL and code == REL_X:
                    out.write(f"rel,{ev},{rd},{val},{names[fd]}\n")
                elif ty == EV_KEY and val == 1 and code < 0x100:
                    out.write(f"key,{ev},{rd},{val},{names[fd]}\n")
        if time.monotonic() - off_at > 0.5:
            off = offset_ns(ptp)
            off_at = time.monotonic()
    out.flush()


if __name__ == "__main__":
    main()

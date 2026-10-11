#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Input latency, host event -> guest kernel, through the real backend.

This plays a display client (a viewer) on a socket the VM's backend connects
to -- the VM's stream socket (`$XDG_RUNTIME_DIR/conduit/VM/stream.sock`),
which the backend tries whether or not `conduit stream` runs -- and sends
input the way the viewer does: relative motion under a grab and key presses,
each packet stamped with the host's CLOCK_REALTIME as it is sent.

In the guest, guest/tests/inputlat_guest.py records the same events from
evdev with the kernel's timestamps, mapped onto the host's clock through
ptp_kvm (the KVM clock-pairing hypercall: host CLOCK_REALTIME next to the
guest's, without a network round trip). `analyze` pairs the two logs.

  inputlat.py send SOCKET [--rate 1000] [--count 5000] [--keys 200] > host.csv
  inputlat.py analyze host.csv guest.csv

What is measured: the viewer's send() of a packet to the guest kernel's
input_event() for it (evdev timestamp), and to the guest's read() of it.
The viewer's own part (compositor -> viewer) is not in it.
"""

import argparse
import array
import os
import socket
import struct
import sys
import time

PKT = struct.Struct("<HHIiiII")  # nvkvm_broker_pkt, 24 bytes
CMD_SIZE = 40

EV_HELLO, EV_SURFACE, EV_FRAME = 1, 2, 3
EV_KEY, EV_REL, EV_GRAB, EV_FOCUS, EV_POINTER, EV_BYE = 5, 8, 10, 11, 12, 13
F_GRABBED, F_FOCUSED = 1, 2
# Keyboard, absolute and relative pointer, pointer lock, focus events: no
# mode hints (the guest keeps its mode), no frames wanted beyond the first.
CAPS = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 5)
PROTO = 2


def now_ns():
    return time.clock_gettime_ns(time.CLOCK_REALTIME)


class Client:
    def __init__(self, conn):
        self.c = conn
        self.seq = 0
        self.flags = F_FOCUSED

    def pkt(self, ty, x=0, y=0, w0=0, w1=0):
        b = PKT.pack(ty, self.flags, self.seq, x, y, w0, w1)
        self.seq += 1
        self.c.sendall(b)

    def drain(self):
        """Read and drop what the backend sends (frames carry fds)."""
        while True:
            try:
                msg, anc, _, _ = self.c.recvmsg(64 * CMD_SIZE, socket.CMSG_SPACE(16 * 4),
                                                socket.MSG_DONTWAIT)
            except BlockingIOError:
                return
            if not msg:
                raise SystemExit("the backend hung up")
            for level, ty, data in anc:
                if level == socket.SOL_SOCKET and ty == socket.SCM_RIGHTS:
                    fds = array.array("i")
                    fds.frombytes(data[: len(data) - len(data) % fds.itemsize])
                    for fd in fds:
                        os.close(fd)


def sleep_until(t_ns):
    while True:
        d = t_ns - time.monotonic_ns()
        if d <= 0:
            return
        if d > 200_000:
            time.sleep((d - 150_000) / 1e9)


def send(args):
    path = args.socket
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    ls = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    ls.bind(path)
    ls.listen(1)
    print(f"waiting for the backend on {path} ...", file=sys.stderr)
    ls.settimeout(args.wait)
    conn, _ = ls.accept()
    c = Client(conn)
    c.pkt(EV_HELLO, w0=PROTO, w1=CAPS)
    c.pkt(EV_SURFACE, 1920, 1080, 240000, 0)
    c.pkt(EV_FOCUS, 1)
    c.pkt(EV_GRAB, 0)
    c.pkt(EV_FRAME)
    time.sleep(1.0)
    c.drain()
    out = sys.stdout
    out.write("kind,seq,t_send_ns,value\n")
    period = int(1e9 / args.rate)

    # Relative motion under a grab: the value names the event (1..63, sign
    # alternating so the guest pointer stays put).
    c.flags |= F_GRABBED
    c.pkt(EV_GRAB, 1)
    time.sleep(0.5)
    t = time.monotonic_ns()
    for i in range(args.count):
        t += period
        sleep_until(t)
        v = (i % 63) + 1
        dx = v if i % 2 == 0 else -v
        ts = now_ns()
        c.pkt(EV_REL, dx, 0)
        out.write(f"rel,{i},{ts},{dx}\n")
        if i % 64 == 0:
            c.drain()
    time.sleep(0.5)
    c.flags &= ~F_GRABBED
    c.pkt(EV_GRAB, 0)
    time.sleep(0.5)

    # Keys: press and release, 20 ms apart, then a pause; key-down is what
    # is timed.
    for i in range(args.keys):
        ts = now_ns()
        c.pkt(EV_KEY, args.key, 1)
        out.write(f"key,{i},{ts},1\n")
        time.sleep(0.02)
        c.pkt(EV_KEY, args.key, 0)
        time.sleep(0.03 + (i % 7) * 0.003)
        c.drain()
    out.flush()
    c.pkt(EV_BYE, 0)
    time.sleep(0.2)
    conn.close()
    os.unlink(path)


def pct(v, p):
    v = sorted(v)
    if not v:
        return float("nan")
    k = min(len(v) - 1, max(0, round(p / 100 * (len(v) - 1))))
    return v[k]


def analyze(args):
    host = {"rel": [], "key": []}
    with open(args.host) as f:
        next(f)
        for line in f:
            kind, seq, ts, val = line.strip().split(",")
            host[kind].append((int(ts), int(val)))
    guest = {"rel": [], "key": []}
    with open(args.guest) as f:
        next(f)
        for line in f:
            kind, ev_ns, read_ns, val, dev = line.strip().split(",")
            guest[kind].append((int(ev_ns), int(read_ns), int(val), dev))

    def report(name, lat_ev, lat_rd, extra=""):
        if not lat_ev:
            print(f"{name}: no pairs")
            return
        us = lambda x: f"{x / 1000:.0f}"
        print(f"{name}: n={len(lat_ev)}  host send -> guest kernel event: "
              f"p50 {us(pct(lat_ev, 50))} us, p99 {us(pct(lat_ev, 99))} us, "
              f"max {us(max(lat_ev))} us;  -> guest read(): p50 {us(pct(lat_rd, 50))} us, "
              f"p99 {us(pct(lat_rd, 99))} us{extra}")

    # Motion: walk the guest's REL_X values, summing host values until they
    # match (a path that merges events reports their sum); the latency of a
    # guest event is from the newest host event in it.
    hs, gs = host["rel"], guest["rel"]
    lat_ev, lat_rd, merged, i = [], [], 0, 0
    for ev_ns, rd_ns, val, _ in gs:
        acc, j, last = 0, i, None
        while j < len(hs) and j - i < 64:
            acc += hs[j][1]
            last = hs[j][0]
            j += 1
            if acc == val:
                break
        if acc != val:
            continue  # unmatched (stray motion): skip, keep position
        merged += (j - i) - 1
        i = j
        lat_ev.append(ev_ns - last)
        lat_rd.append(rd_ns - last)
    rate = ""
    if len(gs) > 1:
        span = (gs[-1][0] - gs[0][0]) / 1e9
        rate = f";  guest reports {len(gs)} in {span:.2f} s = {len(gs) / span:.0f}/s, " \
               f"{merged} host events merged, {len(hs) - i} unmatched"
    report("mouse", lat_ev, lat_rd, rate)

    # Keys: the n-th press in each log.
    hk, gk = host["key"], [g for g in guest["key"] if g[2] == 1]
    n = min(len(hk), len(gk))
    report("key-down", [gk[k][0] - hk[k][0] for k in range(n)],
           [gk[k][1] - hk[k][0] for k in range(n)],
           f";  {len(hk)} sent, {len(gk)} seen")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("send")
    s.add_argument("socket")
    s.add_argument("--rate", type=int, default=1000, help="motion events per second")
    s.add_argument("--count", type=int, default=5000)
    s.add_argument("--keys", type=int, default=200)
    s.add_argument("--key", type=int, default=183, help="Linux key code (183 KEY_F13)")
    s.add_argument("--wait", type=float, default=30.0, help="seconds to wait for the backend")
    a = sub.add_parser("analyze")
    a.add_argument("host")
    a.add_argument("guest")
    args = ap.parse_args()
    if args.cmd == "send":
        send(args)
    else:
        analyze(args)


if __name__ == "__main__":
    main()

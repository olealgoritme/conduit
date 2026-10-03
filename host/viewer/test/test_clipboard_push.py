#!/usr/bin/env python3
"""Conduit seamless clipboard (--clipboard both): host->guest push on focus-in,
streamed 1 MiB-class transfers, echo suppression, large guest->host, and the
host-to-guest mode refusing guest writes.  No display required."""

import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

PKT = struct.Struct("<HHIiiII")
CLIP_PKT = struct.Struct("<HHIB15s")
CMD = struct.Struct("<HHIIIIIQII")
CLIP_CMD = struct.Struct("<HHIIB27s")
EV_HELLO, EV_CLIPBOARD = 1, 15
CMD_CLIPBOARD, CMD_CAPS = 4, 5
LAST = 0x20
CLIENT_CLIPBOARD, CLIENT_CLIP_LARGE = 1 << 0, 1 << 2
CAP_CLIP_LARGE = 1 << 12


def recv_exact(sock, size):
    data = bytearray()
    while len(data) < size:
        part = sock.recv(size - len(data))
        if not part:
            raise RuntimeError("broker closed during a packet")
        data += part
    return bytes(data)


class Broker:
    def __init__(self, tmp, mode):
        self.path = os.path.join(tmp, "broker.sock")
        self.proc = subprocess.Popen(
            [BROKER, "--socket", self.path, "--backend", "test", "--persist",
             "--clipboard", mode],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, text=True)
        for _ in range(100):
            if os.path.exists(self.path):
                break
            time.sleep(0.02)
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(2)
        self.sock.connect(self.path)
        hello = PKT.unpack(recv_exact(self.sock, PKT.size))
        if hello[0] != EV_HELLO or not hello[6] & CAP_CLIP_LARGE:
            raise RuntimeError(f"HELLO lacks CAP_CLIP_LARGE: {hello}")
        for _ in range(4):
            recv_exact(self.sock, PKT.size)

    def caps(self, bits):
        self.sock.sendall(CMD.pack(CMD_CAPS, 0, bits, 0, 0, 0, 0, 0, 0, 0))

    def stdin(self, *lines):
        self.proc.stdin.write("".join(line + "\n" for line in lines))
        self.proc.stdin.flush()
        time.sleep(0.2)

    def clip_transfers(self, timeout=0.5):
        """Every complete EV_CLIPBOARD transfer received until quiet."""
        out, cur = [], bytearray()
        self.sock.settimeout(timeout)
        while True:
            try:
                raw = recv_exact(self.sock, PKT.size)
            except socket.timeout:
                return out
            ty = struct.unpack_from("<H", raw)[0]
            if ty != EV_CLIPBOARD:
                continue
            _, _, _, info, data = CLIP_PKT.unpack(raw)
            cur += data[:info & 0x1f]
            if info & LAST:
                out.append(bytes(cur))
                cur = bytearray()

    def send_guest(self, text):
        recs = []
        for i in range(0, max(len(text), 1), 27):
            part = text[i:i + 27]
            info = len(part) | (LAST if i + 27 >= len(text) else 0)
            recs.append(CLIP_CMD.pack(CMD_CLIPBOARD, 0, i // 27, 0, info,
                                      part.ljust(27, b"\0")))
        self.sock.sendall(b"".join(recs))
        time.sleep(0.3)

    def close(self):
        self.sock.close()
        self.proc.terminate()
        self.proc.wait(timeout=2)
        self.proc.stdin.close()
        log = self.proc.stderr.read()
        self.proc.stderr.close()
        return log


def check(cond, msg, log=""):
    if not cond:
        raise RuntimeError(msg + ("\n" + log if log else ""))


def test_push_both():
    with tempfile.TemporaryDirectory() as tmp:
        b = Broker(tmp, "both")
        b.caps(CLIENT_CLIPBOARD | CLIENT_CLIP_LARGE)
        # Focus-in pushes: the test backend's fetch completes on "c N".
        b.stdin("f 1", "c 300000")
        got = b.clip_transfers()
        # Re-focus with the same host clipboard: nothing new crosses.
        b.stdin("f 0", "f 1", "c 300000")
        again = b.clip_transfers()
        # A paste key is now an ordinary key: no fetch is started by it.
        b.stdin("k 29 1", "k 47 1", "k 47 0", "k 29 0")
        # Guest->host, large, and the echo of the pushed text is suppressed.
        b.send_guest(b"x" * 300000)       # the agent echoing the push
        b.send_guest(b"y" * 40000)
        log = b.close()
    check(got == [b"x" * 300000], f"push: got {[len(t) for t in got]}", log)
    check(again == [], "re-focus re-sent an unchanged clipboard", log)
    check("TEST clipboard from guest: 40000 bytes" in log,
          "large guest->host was not applied", log)
    check("TEST clipboard from guest: 300000 bytes" not in log,
          "the echo of a pushed clipboard reached the host", log)


def test_legacy_client_capped():
    with tempfile.TemporaryDirectory() as tmp:
        b = Broker(tmp, "both")
        b.caps(CLIENT_CLIPBOARD)          # no CLIP_LARGE
        b.stdin("f 1", "c 7168")
        got = b.clip_transfers()
        b.stdin("f 0", "f 1", "c 9000")   # over the legacy cap: not sent
        over = b.clip_transfers()
        log = b.close()
    check(got == [b"x" * 7168], "legacy-sized push failed", log)
    check(over == [], "a legacy client got more than 7168 bytes", log)


def test_to_guest_only():
    with tempfile.TemporaryDirectory() as tmp:
        b = Broker(tmp, "host-to-guest")
        b.caps(CLIENT_CLIPBOARD | CLIENT_CLIP_LARGE)
        b.stdin("f 1", "c 20")
        got = b.clip_transfers()
        b.send_guest(b"zzz")
        log = b.close()
    check(got == [b"x" * 20], "host-to-guest did not push", log)
    check("TEST clipboard from guest" not in log,
          "host-to-guest let the guest write the host clipboard", log)


def test_no_agent_no_push():
    with tempfile.TemporaryDirectory() as tmp:
        b = Broker(tmp, "both")
        b.stdin("f 1", "c 20")            # no CAPS: no agent, no fetch
        got = b.clip_transfers()
        log = b.close()
    check(got == [], "pushed to a client that has no clipboard agent", log)


def main():
    global BROKER
    if len(sys.argv) != 2:
        raise SystemExit("usage: test_clipboard_push.py /path/to/broker")
    BROKER = os.path.abspath(sys.argv[1])
    test_push_both()
    test_legacy_client_capped()
    test_to_guest_only()
    test_no_agent_no_push()
    print("clipboard push tests passed")


if __name__ == "__main__":
    main()

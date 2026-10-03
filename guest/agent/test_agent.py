#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Unit tests for conduit-clipboard-agent. Headless: no display, no device.

    python3 -m unittest discover -s guest/agent -p 'test_*.py'

With Xvfb and xclip installed, an X11 round trip runs as well.
"""

import importlib.machinery
import importlib.util
import os
import shutil
import socket
import subprocess
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
_loader = importlib.machinery.SourceFileLoader(
    "conduit_clipboard_agent", os.path.join(HERE, "conduit-clipboard-agent"))
_spec = importlib.util.spec_from_loader(_loader.name, _loader)
agent = importlib.util.module_from_spec(_spec)
_loader.exec_module(agent)
agent._quiet = True


class RecordTest(unittest.TestCase):
    def test_roundtrip(self):
        rec = agent.encode_record("héllo".encode(), generation=7)
        self.assertEqual(len(rec), 56 + 6)
        self.assertEqual(rec[:4], b"CLIP")
        gen, mime, data = agent.decode_record(rec)
        self.assertEqual((gen, mime, data),
                         (7, "text/plain;charset=utf-8", "héllo".encode()))

    def test_layout_matches_the_spec(self):
        rec = agent.encode_record(b"abc", generation=0x1122334455667788)
        self.assertEqual(rec[0:4], (0x50494C43).to_bytes(4, "little"))
        self.assertEqual(rec[4:8], (1).to_bytes(4, "little"))
        self.assertEqual(rec[8:16], (0x1122334455667788).to_bytes(8, "little"))
        self.assertEqual(rec[16:20], (3).to_bytes(4, "little"))
        self.assertEqual(rec[20:24], b"\0\0\0\0")
        self.assertEqual(rec[24:56].rstrip(b"\0"), b"text/plain;charset=utf-8")
        self.assertEqual(rec[56:], b"abc")

    def test_rejects(self):
        with self.assertRaises(agent.RecordError):
            agent.encode_record(b"")
        with self.assertRaises(agent.RecordError):
            agent.encode_record(b"x" * (agent.MAX_BYTES + 1))
        agent.encode_record(b"x" * agent.MAX_BYTES)
        good = agent.encode_record(b"abc")
        with self.assertRaises(agent.RecordError):
            agent.decode_record(good[:40])
        with self.assertRaises(agent.RecordError):
            agent.decode_record(good + b"extra")
        with self.assertRaises(agent.RecordError):
            agent.decode_record(b"XXXX" + good[4:])


class SyncTest(unittest.TestCase):
    def test_echo_is_suppressed(self):
        s = agent.Sync()
        self.assertTrue(s.from_host(b"A"))
        # Our own write coming back from the local clipboard.
        self.assertFalse(s.local_changed(b"A"))
        self.assertTrue(s.local_changed(b"B"))
        # The host echoing B back is not applied again.
        self.assertFalse(s.from_host(b"B"))

    def test_same_text_again_after_the_other_side_changed(self):
        s = agent.Sync()
        self.assertTrue(s.local_changed(b"X"))
        self.assertTrue(s.from_host(b"Y"))
        self.assertTrue(s.local_changed(b"X"))

    def test_baseline_is_not_sent(self):
        s = agent.Sync()
        s.baseline(b"old")
        self.assertFalse(s.local_changed(b"old"))
        self.assertTrue(s.local_changed(b"new"))

    def test_junk_is_ignored(self):
        s = agent.Sync()
        self.assertFalse(s.local_changed(b""))
        self.assertFalse(s.local_changed(b"\xff\xfe"))
        self.assertFalse(s.local_changed(b"a\0b"))
        self.assertFalse(s.local_changed(b"x" * (agent.MAX_BYTES + 1)))
        self.assertFalse(s.from_host(b"\xc0\x80"))


class CandidatesTest(unittest.TestCase):
    def c(self, env, forced="auto", wl=True):
        return agent.candidates(env, forced, wl)

    def test_matrix(self):
        self.assertEqual(self.c({"DISPLAY": ":0", "XDG_SESSION_TYPE": "x11",
                                 "XDG_CURRENT_DESKTOP": "XFCE"}), ["x11"])
        gnome = {"WAYLAND_DISPLAY": "wayland-0", "DISPLAY": ":0",
                 "XDG_SESSION_TYPE": "wayland",
                 "XDG_CURRENT_DESKTOP": "ubuntu:GNOME"}
        self.assertEqual(self.c(gnome), ["x11", "wayland"])
        self.assertEqual(self.c(gnome, wl=False), ["x11"])
        kde = {"WAYLAND_DISPLAY": "wayland-0", "DISPLAY": ":1",
               "XDG_CURRENT_DESKTOP": "KDE"}
        self.assertEqual(self.c(kde), ["wayland", "x11"])
        sway = {"WAYLAND_DISPLAY": "wayland-1", "XDG_CURRENT_DESKTOP": "sway"}
        self.assertEqual(self.c(sway), ["wayland"])
        self.assertEqual(self.c(sway, wl=False), [])
        self.assertEqual(self.c({}), [])
        self.assertEqual(self.c({"XDG_SESSION_TYPE": "wayland"}), ["wayland"])

    def test_gnome_public_display(self):
        gnome = {"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland",
                 "XDG_CURRENT_DESKTOP": "GNOME", "GNOME_SETUP_DISPLAY": ":1"}
        sess = {"DISPLAY": ":0", "GNOME_SETUP_DISPLAY": ":1"}
        # DISPLAY is the public one: kept.
        self.assertEqual(agent.x11_display(dict(gnome, DISPLAY=":0"), {}), ":0")
        # Only the private one, or none: the session's public one.
        self.assertEqual(agent.x11_display(dict(gnome, DISPLAY=":1"), sess),
                         ":0")
        self.assertEqual(agent.x11_display(gnome, sess), ":0")
        # Nothing better known: unchanged.
        self.assertEqual(agent.x11_display(dict(gnome, DISPLAY=":1"), {}), ":1")
        self.assertIsNone(agent.x11_display(gnome, {}))

    def test_forced(self):
        self.assertEqual(self.c({}, "x11"), ["x11"])
        self.assertEqual(self.c({"DISPLAY": ":0"}, "wayland"), ["wayland"])


class FakeBackend:
    name = "fake"

    def __init__(self, initial=None):
        self.initial_text = initial
        self.set_calls = []
        self.pending = []
        self.r, self.w = os.pipe()
        os.set_blocking(self.r, False)

    def initial(self):
        return self.initial_text

    def fds(self):
        return [self.r]

    def copy(self, text):
        """Simulate a copy in the guest session."""
        self.pending.append(text)
        os.write(self.w, b"x")

    def process(self, readable):
        if self.r not in readable:
            return []
        try:
            os.read(self.r, 4096)
        except BlockingIOError:
            pass
        out, self.pending = self.pending, []
        return out

    def set(self, text):
        self.set_calls.append(text)
        # Setting the local clipboard notifies the watcher, as real ones do.
        self.copy(text)

    def tick(self):
        pass

    def close(self):
        os.close(self.r)
        os.close(self.w)


class AgentLoopTest(unittest.TestCase):
    def setUp(self):
        # SOCK_SEQPACKET keeps record boundaries, like the real device.
        self.host, guest = socket.socketpair(socket.AF_UNIX,
                                             socket.SOCK_SEQPACKET)
        self.host.settimeout(0.5)
        self.dev = agent.Device(path="/nonexistent", fd=guest.detach())
        self.be = FakeBackend(initial=b"already here")
        self.a = agent.Agent(self.dev, self.be)
        self.a.start()

    def tearDown(self):
        self.host.close()
        if self.dev.fd is not None:
            os.close(self.dev.fd)
        self.be.close()

    def spin(self, n=3):
        for _ in range(n):
            self.a.step(timeout=0.05)

    def host_reads(self):
        try:
            return agent.decode_record(self.host.recv(agent.MAX_BYTES + 4096))
        except socket.timeout:
            return None

    def test_host_to_guest_without_echo(self):
        self.host.send(agent.encode_record("from host ✓".encode(), 5))
        self.spin()
        self.assertEqual(self.be.set_calls, ["from host ✓".encode()])
        self.host.settimeout(0.1)
        self.assertIsNone(self.host_reads(), "the host's text came back")

    def test_guest_to_host(self):
        self.be.copy(b"guest copy")
        self.spin()
        gen, mime, data = self.host_reads()
        self.assertEqual((mime, data), ("text/plain;charset=utf-8",
                                        b"guest copy"))
        # The host's echo of it does not reach the local clipboard again.
        self.host.send(agent.encode_record(b"guest copy", 9))
        self.spin()
        self.assertEqual(self.be.set_calls, [])

    def test_startup_content_is_not_sent(self):
        self.be.copy(b"already here")
        self.spin()
        self.host.settimeout(0.1)
        self.assertIsNone(self.host_reads())

    def test_big_text(self):
        # A socket datagram is bounded by wmem_max; the device is not.
        big = ("ø" * 50000).encode()
        self.host.send(agent.encode_record(big, 1))
        self.spin()
        self.assertEqual(self.be.set_calls, [big])

    def test_device_loss_reopens(self):
        self.host.close()
        self.spin()
        self.assertIsNone(self.dev.fd)
        self.host = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)


class StalledDeviceTest(unittest.TestCase):
    """A 1 MiB copy to a host that is not draining must not stall the loop:
    host->guest keeps working, and the record arrives whole once it drains."""

    def setUp(self):
        # A stream socket stands in for the device: unlike a datagram it
        # holds 1 MiB, and a write blocks while nobody reads, as the driver's
        # does while the host has not answered.
        self.host, guest = socket.socketpair(socket.AF_UNIX,
                                             socket.SOCK_STREAM)
        for sk in (self.host, guest):
            sk.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
            sk.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        fd = guest.detach()
        os.set_blocking(fd, False)    # as Device.ensure() opens it
        self.dev = agent.Device(path="/nonexistent", fd=fd)
        self.be = FakeBackend()
        self.a = agent.Agent(self.dev, self.be)
        self.a.start()

    def tearDown(self):
        self.host.close()
        if self.dev.fd is not None:
            os.close(self.dev.fd)
        self.be.close()

    def test_1mib_copy_does_not_block_the_loop(self):
        big = b"q" * agent.MAX_BYTES
        self.be.copy(big)
        t = time.monotonic()
        self.a.step(timeout=0.05)
        self.assertLess(time.monotonic() - t, 1.0, "the loop waited")
        self.assertTrue(self.dev.busy() or self.dev._thread.is_alive())
        # Host -> guest still flows while the guest's write is stuck.
        self.host.send(agent.encode_record(b"from host", 3))
        for _ in range(5):
            self.a.step(timeout=0.05)
        self.assertEqual(self.be.set_calls, [b"from host"])
        # Drain: one whole record, 1 MiB of data.
        want = agent.HEADER_LEN + len(big)
        got = bytearray()
        self.host.settimeout(5)
        while len(got) < want:
            got += self.host.recv(1 << 16)
        self.assertEqual(agent.decode_record(bytes(got))[2], big)
        end = time.monotonic() + 2
        while time.monotonic() < end and not self.dev._results:
            time.sleep(0.01)
        self.assertEqual([(len(t), e) for t, e in self.dev.results()],
                         [(len(big), None)])


@unittest.skipUnless(shutil.which("Xvfb") and shutil.which("xclip"),
                     "needs Xvfb and xclip")
class X11Test(unittest.TestCase):
    def setUp(self):
        self.display = ":%d" % (90 + os.getpid() % 50)
        self.xvfb = subprocess.Popen(["Xvfb", self.display, "-nolisten",
                                      "tcp"], stderr=subprocess.DEVNULL)
        for _ in range(100):
            if os.path.exists("/tmp/.X11-unix/X" + self.display[1:]):
                break
            time.sleep(0.05)
        self.env = dict(os.environ, DISPLAY=self.display)
        self.be = agent.X11Backend(self.display)

    def tearDown(self):
        self.be.close()
        self.xvfb.terminate()
        self.xvfb.wait()

    def xclip_in(self, text):
        subprocess.run(["xclip", "-selection", "clipboard", "-i"],
                       input=text, env=self.env, check=True, timeout=5)

    def wait_change(self, secs=3.0):
        end = time.monotonic() + secs
        while time.monotonic() < end:
            got = self.be.process(set(self.be.fds()))
            self.be.tick()
            if got:
                return got[-1]
            time.sleep(0.02)
        return None

    def test_watch_and_read(self):
        self.assertIsNone(self.be.initial())
        self.xclip_in("hello x11 ✓".encode())
        self.assertEqual(self.wait_change(), "hello x11 ✓".encode())

    def test_incr_read(self):
        big = b"0123456789abcdef" * 40000  # 640 KB: xclip uses INCR
        self.xclip_in(big)
        self.assertEqual(self.wait_change(5.0), big)

    def test_set_and_serve(self):
        self.be.set("served ✓".encode())
        out = {}

        def paste():
            out["v"] = subprocess.run(
                ["xclip", "-selection", "clipboard", "-o", "-t",
                 "UTF8_STRING"], env=self.env, capture_output=True,
                timeout=5).stdout

        import threading
        t = threading.Thread(target=paste)
        t.start()
        end = time.monotonic() + 5
        while t.is_alive() and time.monotonic() < end:
            self.assertEqual(self.be.process(set(self.be.fds())), [],
                             "our own selection was reported as a change")
            time.sleep(0.01)
        t.join()
        self.assertEqual(out.get("v"), "served ✓".encode())


if __name__ == "__main__":
    unittest.main()

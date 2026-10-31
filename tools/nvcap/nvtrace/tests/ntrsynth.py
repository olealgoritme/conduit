"""Write synthetic nvtrace logs (format v1) for decoder tests. Mirrors the
record layouts in src/ntr_format.h."""

import struct


class NtrWriter:
    def __init__(self, ptr_size=8, pid=1234, qpc_freq=10_000_000):
        self.P = ptr_size
        self.buf = bytearray(b"NVTRACE\0" + struct.pack("<IIIIQ", 1, 32, ptr_size, pid, qpc_freq))
        self.seq = 0
        self.labels = {}

    def _rec(self, typ, body, qpc=0, tid=1):
        body = bytes(body)
        body += b"\0" * ((8 - len(body) % 8) % 8)
        self.seq += 1
        self.buf += struct.pack("<IIQII", typ, 24 + len(body), qpc, tid, self.seq) + body

    def procinfo(self, kv):
        b = b"".join(("%s=%s" % (k, v)).encode() + b"\0" for k, v in kv.items()) + b"\0"
        self._rec(1, b)

    def string(self, i, s):
        self.labels[s] = i
        self._rec(2, struct.pack("<I", i) + s.encode() + b"\0")

    def hook(self, hid, module, name, nargs=1, flags=1 | 2 | 8, nr=0x1000, addr=0x7FF800001000,
             arg0size=0, status=0):
        self._rec(3, struct.pack("<IIIIQII", hid, flags, nr, nargs, addr, arg0size, status)
                  + module.encode() + b"\0" + name.encode() + b"\0")

    def module(self, base, size, path, version=(0, 0, 0, 0), flags=1, qpc=0):
        self._rec(4, struct.pack("<QQ4HII", base, size, *version, 0, flags) + path.encode() + b"\0", qpc)

    def note(self, level, text):
        self._rec(7, struct.pack("<II", level, 0) + text.encode() + b"\0")

    def call(self, hid, qpc, qpc_end, ret, args, frames, blobs, tid=1, flags=0, depth=0):
        """blobs: list of dicts with label (text), data, phase, addr, index, flags, parent, req"""
        b = bytearray(struct.pack("<IIQQHHI", hid, flags, qpc_end, ret & 0xFFFFFFFFFFFFFFFF,
                                  len(args), len(frames), depth))
        b += struct.pack("<%dQ" % len(args), *args)
        b += struct.pack("<%dQ" % len(frames), *frames)
        for bl in blobs:
            data = bl.get("data", b"")
            fl = bl.get("flags", 0)
            stored = b"" if fl & 4 else data
            b += struct.pack("<IIQIIBBHI", self.labels[bl["label"]], bl.get("index", 0xFFFFFFFF),
                             bl.get("addr", 0x1000), bl.get("req", len(data)), len(stored),
                             bl.get("phase", 0), fl, bl.get("parent", 0), 0)
            b += stored + b"\0" * ((8 - len(stored) % 8) % 8)
        self._rec(6, b, qpc, tid)

    def end(self, records=0, calls=0, dropped=0, nbytes=0):
        self._rec(8, struct.pack("<4Q", records, calls, dropped, nbytes))

    def save(self, path):
        with open(path, "wb") as f:
            f.write(self.buf)

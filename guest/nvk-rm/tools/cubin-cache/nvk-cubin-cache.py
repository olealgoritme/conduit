#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Build NVK's cubin cache: SASS for CUDA modules that carry only PTX for a GPU.

NVK runs the SASS of a CUDA module (VK_NVX_binary_import, what NGX/DLSS uses)
only when it was built for the GPU's SM version; it has no PTX compiler.  A
DLSS DLL without sm_120 SASS therefore cannot run on an RTX 50.  This tool
takes the PTX out of the fatbins in the files it is given (DLSS DLLs, or the
modules NVK wrote on a cache miss), compiles it on the Linux host with
NVIDIA's own PTX compiler and writes one cubin per module, which NVK loads
in place of the missing SASS (patches-windows/0068).

  nvk-cubin-cache.py scan FILE...
      What each fatbin in FILE carries: SASS and PTX per SM, compression,
      and whether a cubin uses things NVK does not load (constant banks
      other than 0, relocations, global data).

  nvk-cubin-cache.py build -o DIR [--sm 120] [--compiler auto|cuda|ptxas]
                           [--ptxas PATH] [--force] [--limit N] FILE...
      For every fatbin in FILE that has no SASS for sm_SM, compiles its
      newest PTX (at most sm_SM) for sm_SM and writes DIR/KEY.sm_SM.cubin,
      plus DIR/index.tsv (key, SM, PTX SM, source file, offset, kernels).
      KEY is the BLAKE3 hash of the PTX entry's bytes as they are in the fatbin
      (compressed or not), so NVK finds the cubin from the module data alone.
      A second file under the key of the plain PTX text covers an
      application that passes PTX instead of a fatbin.

Compilers: `cuda` uses the CUDA driver API of the installed NVIDIA driver
(libcuda.so.1: cuLinkCreate with CU_JIT_TARGET, the same JIT the driver uses
for PTX on Blackwell; needs an NVIDIA GPU and creates a CUDA context on it
for the run); `ptxas` runs ptxas from a CUDA toolkit (no GPU needed).
`auto` takes ptxas when it is on PATH or given, else libcuda.

The output is compiled NVIDIA code for your own machine: keep it there,
do not redistribute it.  The tool reads and writes only the files named.
Python 3.8+, standard library only.
"""

import argparse
import ctypes
import os
import shutil
import struct
import subprocess
import sys
import tempfile

FATBIN_MAGIC = 0xBA55ED50
FATBIN_MAGIC_BYTES = struct.pack("<I", FATBIN_MAGIC)
KIND_PTX = 1
KIND_SASS = 2
FLAG_COMPRESS_LZ4 = 0x2000
# Bits seen next to compression in newer CUDA fatbins; an entry with one of
# them and not the LZ4 bit is reported as "unknown compression".
FLAG_COMPRESS_OTHER = 0xC000

EM_CUDA = 190
NV_ELF_OSABI_41 = 0x41


class FatbinError(Exception):
    pass


# ---------------------------------------------------------------- BLAKE3
#
# The cache key is BLAKE3 (Mesa's _mesa_blake3_compute, so NVK needs no
# other hash).  The `blake3` module is used when it is installed; otherwise
# this plain implementation of the BLAKE3 specification (hash mode, 32-byte
# output), which test_nvk_cubin_cache.py checks against the official test
# vectors.

_B3_IV = (0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
          0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19)
_B3_PERM = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)
_B3_CHUNK_START, _B3_CHUNK_END, _B3_PARENT, _B3_ROOT = 1, 2, 4, 8
_M32 = 0xFFFFFFFF


def _b3_compress(cv, m, counter, block_len, flags):
    s = list(cv) + list(_B3_IV[:4]) + [counter & _M32, (counter >> 32) & _M32,
                                       block_len, flags]
    m = list(m)
    for r in range(7):
        for a, b, c, d, x, y in ((0, 4, 8, 12, 0, 1), (1, 5, 9, 13, 2, 3),
                                 (2, 6, 10, 14, 4, 5), (3, 7, 11, 15, 6, 7),
                                 (0, 5, 10, 15, 8, 9), (1, 6, 11, 12, 10, 11),
                                 (2, 7, 8, 13, 12, 13), (3, 4, 9, 14, 14, 15)):
            sa = (s[a] + s[b] + m[x]) & _M32
            sd = s[d] ^ sa
            sd = ((sd >> 16) | (sd << 16)) & _M32
            sc = (s[c] + sd) & _M32
            sb = s[b] ^ sc
            sb = ((sb >> 12) | (sb << 20)) & _M32
            sa = (sa + sb + m[y]) & _M32
            sd ^= sa
            sd = ((sd >> 8) | (sd << 24)) & _M32
            sc = (sc + sd) & _M32
            sb ^= sc
            sb = ((sb >> 7) | (sb << 25)) & _M32
            s[a], s[b], s[c], s[d] = sa, sb, sc, sd
        if r < 6:
            m = [m[i] for i in _B3_PERM]
    for i in range(8):
        s[i] ^= s[i + 8]
        s[i + 8] ^= cv[i]
    return s


def _b3_words(block):
    return struct.unpack("<16I", block + bytes(64 - len(block)))


def blake3_digest(data):
    """BLAKE3 hash of data, 32 bytes."""
    data = memoryview(bytes(data))
    stack = []                          # chaining values of complete subtrees
    nchunks = max(1, (len(data) + 1023) // 1024)
    for ci in range(nchunks):
        chunk = data[ci * 1024:(ci + 1) * 1024]
        cv = _B3_IV
        nblocks = max(1, (len(chunk) + 63) // 64)
        for bi in range(nblocks):
            block = bytes(chunk[bi * 64:(bi + 1) * 64])
            flags = (_B3_CHUNK_START if bi == 0 else 0) | \
                (_B3_CHUNK_END if bi == nblocks - 1 else 0)
            if bi == nblocks - 1:
                last = (cv, _b3_words(block), ci, len(block), flags)
            else:
                cv = _b3_compress(cv, _b3_words(block), ci, 64, flags)[:8]
        if ci == nchunks - 1:
            break
        cv = _b3_compress(*last)[:8]
        total = ci + 1
        while total & 1 == 0:
            cv = _b3_compress(_B3_IV, stack.pop() + cv, 0, 64, _B3_PARENT)[:8]
            total >>= 1
        stack.append(cv)
    # `last` is the final chunk's last block; fold the stack into it
    while stack:
        cv = _b3_compress(*last)[:8]
        last = (_B3_IV, stack.pop() + cv, 0, 64, _B3_PARENT)
    cv, m, counter, block_len, flags = last
    out = _b3_compress(cv, m, counter, block_len, flags | _B3_ROOT)
    return struct.pack("<8I", *out[:8])


try:
    import blake3 as _blake3_mod      # optional, much faster

    def blake3_hex(data):
        return _blake3_mod.blake3(bytes(data)).hexdigest()
except ImportError:
    def blake3_hex(data):
        return blake3_digest(data).hex()


# ---------------------------------------------------------------- fatbins

class Entry:
    """One object in a fatbin: PTX text or a SASS cubin."""

    __slots__ = ("kind", "sm", "flags", "header_size", "offset", "payload",
                 "compressed_size", "decompressed_size", "name")

    def __init__(self):
        self.kind = 0
        self.sm = 0
        self.flags = 0
        self.header_size = 0
        self.offset = 0
        self.payload = b""
        self.compressed_size = 0
        self.decompressed_size = 0
        self.name = ""

    @property
    def compressed(self):
        return bool(self.flags & (FLAG_COMPRESS_LZ4 | FLAG_COMPRESS_OTHER))

    def key(self):
        """NVK's cache key: BLAKE3 of the payload exactly as stored."""
        return blake3_hex(self.payload)

    def data(self):
        """The payload, decompressed."""
        if not self.compressed:
            return self.payload
        if not self.flags & FLAG_COMPRESS_LZ4:
            raise FatbinError("unknown compression (flags 0x%x)" % self.flags)
        src = self.payload[:self.compressed_size] if self.compressed_size else self.payload
        out = lz4_block_decompress(src, self.decompressed_size)
        if self.decompressed_size and len(out) != self.decompressed_size:
            raise FatbinError("decompressed %d bytes, header says %d"
                              % (len(out), self.decompressed_size))
        return out


class Fatbin:
    __slots__ = ("offset", "version", "size", "entries")

    def __init__(self, offset, version, size, entries):
        self.offset = offset
        self.version = version
        self.size = size
        self.entries = entries

    def sass_sms(self):
        return sorted({e.sm for e in self.entries if e.kind == KIND_SASS})

    def ptx_sms(self):
        return sorted({e.sm for e in self.entries if e.kind == KIND_PTX})

    def has_sass(self, sm):
        return any(e.kind == KIND_SASS and e.sm == sm for e in self.entries)

    def ptx_for(self, sm):
        """The newest PTX entry not newer than sm (what NVK tries first)."""
        best = None
        for e in self.entries:
            if e.kind == KIND_PTX and e.sm <= sm and (best is None or e.sm > best.sm):
                best = e
        return best


def parse_fatbin(buf, off):
    """Parse the fatbin at buf[off:]; raises FatbinError when it is not one."""
    if off + 16 > len(buf):
        raise FatbinError("truncated header")
    magic, version, header_size, size = struct.unpack_from("<IHHQ", buf, off)
    if magic != FATBIN_MAGIC:
        raise FatbinError("bad magic")
    if header_size < 16 or header_size > 256:
        raise FatbinError("bad header size %d" % header_size)
    start = off + header_size
    end = start + size
    if size == 0 or end > len(buf):
        raise FatbinError("size %d runs past the file" % size)
    entries = []
    p = start
    while p < end:
        if p + 32 > end:
            raise FatbinError("truncated entry header at 0x%x" % p)
        kind, _unk, ehs, esize = struct.unpack_from("<HHIQ", buf, p)
        if kind not in (KIND_PTX, KIND_SASS):
            # padding at the end of a fatbin is zeros
            if kind == 0 and not any(buf[p:end]):
                break
            raise FatbinError("unknown entry kind %d at 0x%x" % (kind, p))
        if ehs < 32 or p + ehs > end or p + ehs + esize > end:
            raise FatbinError("entry at 0x%x runs past the fatbin" % p)
        e = Entry()
        e.kind = kind
        e.header_size = ehs
        e.offset = p
        e.compressed_size, = struct.unpack_from("<I", buf, p + 16)
        e.sm, = struct.unpack_from("<I", buf, p + 28)
        if ehs >= 48:
            name_off, name_len = struct.unpack_from("<II", buf, p + 32)
            e.flags, = struct.unpack_from("<Q", buf, p + 40)
            if name_off and name_len and name_off + name_len <= ehs:
                e.name = bytes(buf[p + name_off:p + name_off + name_len]).split(b"\0")[0] \
                    .decode("latin-1")
        if ehs >= 64:
            e.decompressed_size, = struct.unpack_from("<Q", buf, p + 56)
        e.payload = bytes(buf[p + ehs:p + ehs + esize])
        entries.append(e)
        p += ehs + esize
    return Fatbin(off, version, size, entries)


def find_fatbins(buf):
    """Every well-formed fatbin in buf, and how many candidates were refused."""
    out = []
    refused = 0
    i = buf.find(FATBIN_MAGIC_BYTES)
    while i >= 0:
        try:
            fb = parse_fatbin(buf, i)
            out.append(fb)
            nxt = i + 16 + fb.size
        except FatbinError:
            refused += 1
            nxt = i + 4
        i = buf.find(FATBIN_MAGIC_BYTES, nxt)
    return out, refused


def lz4_block_decompress(src, size_hint=0):
    """An LZ4 block (the fatbin's compression): literals and back-references."""
    out = bytearray()
    i = 0
    n = len(src)
    while i < n:
        token = src[i]
        i += 1
        lit = token >> 4
        if lit == 15:
            while True:
                if i >= n:
                    raise FatbinError("truncated literal length")
                b = src[i]
                i += 1
                lit += b
                if b != 255:
                    break
        if i + lit > n:
            raise FatbinError("literals run past the input")
        out += src[i:i + lit]
        i += lit
        if i >= n:
            break                       # the last sequence has no match
        if i + 2 > n:
            raise FatbinError("truncated match offset")
        moff = src[i] | (src[i + 1] << 8)
        i += 2
        if moff == 0 or moff > len(out):
            raise FatbinError("bad match offset %d" % moff)
        mlen = (token & 15) + 4
        if (token & 15) == 15:
            while True:
                if i >= n:
                    raise FatbinError("truncated match length")
                b = src[i]
                i += 1
                mlen += b
                if b != 255:
                    break
        start = len(out) - moff
        if mlen <= moff:
            out += out[start:start + mlen]
        else:
            for k in range(mlen):
                out.append(out[start + k])
        if size_hint and len(out) > size_hint:
            raise FatbinError("output larger than announced")
    return bytes(out)


def ptx_text(data):
    """PTX as NVK hashes it when an application passes PTX text: up to the
    first NUL."""
    return data.split(b"\0", 1)[0]


def is_ptx_text(data):
    """What NVK takes for PTX text (a module that is no fatbin or ELF)."""
    s = bytes(data[:4096]).lstrip(b" \t\r\n")
    return s.startswith(b"//") or s.startswith(b".version")


def ptx_target_sm(text):
    """The N of `.target sm_N` in PTX text, 0 when there is none."""
    i = text.find(b".target sm_")
    if i < 0:
        return 0
    digits = b""
    for c in text[i + 11:i + 16]:
        if not 48 <= c <= 57:
            break
        digits += bytes([c])
    return int(digits) if digits else 0


def ptx_text_key(data):
    return blake3_hex(ptx_text(data))


# ---------------------------------------------------------------- cubins

class CubinInfo:
    __slots__ = ("sm", "abi", "kernels", "cbanks", "globals_", "relocs", "problems")

    def __init__(self):
        self.sm = 0
        self.abi = 0
        self.kernels = []
        self.cbanks = set()             # banks other than 0 (module data)
        self.globals_ = []
        self.relocs = 0                 # relocations NVK applies
        self.problems = []              # what NVK refuses

    def nvk_problems(self):
        """Why NVK (patches-common 0084) would refuse this cubin: it loads
        the module's code, bank 0 (parameters), banks 1-7 and the module's
        globals, and applies R_CUDA_32/64 relocations in that data (to data
        or code symbols); a relocation anywhere else (in code) or of another
        type fails."""
        return sorted(set(self.problems))


DATA_BANKS = range(1, 8)


def data_section_bank(name):
    """Bank (1-7) of a .nv.constantN[.kernel] section, 0x100 for globals,
    0 for anything NVK does not place."""
    if name in (".nv.global", ".nv.global.init"):
        return 0x100
    if not name.startswith(".nv.constant"):
        return 0
    num = name[len(".nv.constant"):].split(".", 1)[0]
    if not num.isdigit() or int(num) not in DATA_BANKS:
        return 0
    return int(num)


def parse_cubin(buf):
    """Enough of a CUDA ELF to report its SM, kernels and what it needs."""
    if len(buf) < 64 or buf[:4] != b"\x7fELF":
        raise FatbinError("not an ELF")
    if buf[4] != 2 or buf[5] != 1:
        raise FatbinError("not a little-endian ELF64")
    (_e_type, e_machine, _ver, _entry, _phoff, e_shoff, e_flags, _ehsize,
     _phentsize, _phnum, e_shentsize, e_shnum, e_shstrndx) = \
        struct.unpack_from("<HHIQQQIHHHHHH", buf, 16)
    if e_machine != EM_CUDA:
        raise FatbinError("not EM_CUDA")
    info = CubinInfo()
    info.abi = buf[7]
    info.sm = (e_flags >> 8) & 0xFF if info.abi >= NV_ELF_OSABI_41 else (e_flags >> 16) & 0xFF
    if e_shnum == 0:
        return info
    if e_shentsize != 64 or e_shoff + 64 * e_shnum > len(buf):
        raise FatbinError("section headers outside the image")
    shdrs = [struct.unpack_from("<IIQQQQIIQQ", buf, e_shoff + 64 * k)
             for k in range(e_shnum)]
    if e_shstrndx >= e_shnum:
        raise FatbinError("bad e_shstrndx")
    strsh = shdrs[e_shstrndx]

    def secname(idx):
        o = strsh[4] + idx
        if o >= len(buf):
            return ""
        z = buf.find(b"\0", o)
        return bytes(buf[o:z if z >= 0 else len(buf)]).decode("latin-1")

    names = [secname(sh[0]) for sh in shdrs]
    for name, sh in zip(names, shdrs):
        if name.startswith(".text."):
            info.kernels.append(name[len(".text."):])
        bank = data_section_bank(name)
        if bank == 0x100:
            info.globals_.append(name)
        elif bank:
            info.cbanks.add(bank)
        elif name.startswith(".nv.constant"):
            num = name[len(".nv.constant"):].split(".", 1)[0]
            if num.isdigit() and int(num) != 0:
                info.problems.append("constant bank %s" % num)
    for name, sh in zip(names, shdrs):
        sh_type, sh_info, sh_link = sh[1], sh[7], sh[6]
        if sh_type not in (4, 9) or sh[5] == 0:   # SHT_RELA, SHT_REL; sm_90+
            continue                              # has empty .rela.text.*
        target = names[sh_info] if sh_info < len(names) else ""
        if not data_section_bank(target):
            if target.startswith(".debug") or target.startswith(".nv.rel"):
                continue
            info.problems.append("relocations in " + target.split(".", 3)[1]
                                 if target.startswith(".") else "relocations")
            continue
        ent = 24 if sh_type == 4 else 16
        symtab = shdrs[sh_link] if sh_link < len(shdrs) else None
        for k in range(sh[5] // ent):
            r_info, = struct.unpack_from("<Q", buf, sh[4] + ent * k + 8)
            rtype, sym = r_info & 0xFFFFFFFF, r_info >> 32
            if rtype not in (1, 2):
                info.problems.append("relocation type %d" % rtype)
                continue
            if symtab is not None and symtab[4] + 24 * sym + 24 <= len(buf):
                shndx, = struct.unpack_from("<H", buf, symtab[4] + 24 * sym + 6)
                # module data, or code (an address NVK resolves after it
                # uploads the module's code)
                if shndx >= len(names) or not (data_section_bank(names[shndx])
                                               or names[shndx].startswith(".text.")):
                    info.problems.append("relocation against a symbol outside module data")
                    continue
            info.relocs += 1
    return info


# ---------------------------------------------------------------- PE version

def pe_file_version(buf):
    """The FILEVERSION of a PE's VS_FIXEDFILEINFO, or ''."""
    i = buf.find(struct.pack("<I", 0xFEEF04BD))
    while i >= 0:
        if i + 16 <= len(buf):
            _sig, _strucver, ms, ls = struct.unpack_from("<IIII", buf, i)
            if _strucver in (0x10000, 0):
                return "%d.%d.%d.%d" % (ms >> 16, ms & 0xFFFF, ls >> 16, ls & 0xFFFF)
        i = buf.find(struct.pack("<I", 0xFEEF04BD), i + 4)
    return ""


# ---------------------------------------------------------------- compilers

class CompileError(Exception):
    pass


class CudaJit:
    """PTX to cubin through the CUDA driver API (cuLink*)."""

    CU_JIT_INFO_LOG_BUFFER = 3
    CU_JIT_INFO_LOG_BUFFER_SIZE_BYTES = 4
    CU_JIT_ERROR_LOG_BUFFER = 5
    CU_JIT_ERROR_LOG_BUFFER_SIZE_BYTES = 6
    CU_JIT_OPTIMIZATION_LEVEL = 7
    CU_JIT_TARGET = 9
    CU_JIT_POSITION_INDEPENDENT_CODE = 30
    CU_JIT_INPUT_PTX = 1

    def __init__(self, lib="libcuda.so.1"):
        try:
            self.cu = ctypes.CDLL(lib)
        except OSError as e:
            raise CompileError("cannot load %s (%s)" % (lib, e))

        def fn(*names):
            for n in names:
                f = getattr(self.cu, n, None)
                if f is not None:
                    f.restype = ctypes.c_int
                    return f
            raise CompileError("libcuda has no " + names[0])

        self.cuInit = fn("cuInit")
        self.cuDeviceGet = fn("cuDeviceGet")
        self.cuDevicePrimaryCtxRetain = fn("cuDevicePrimaryCtxRetain")
        self.cuDevicePrimaryCtxRelease = fn("cuDevicePrimaryCtxRelease_v2",
                                            "cuDevicePrimaryCtxRelease")
        self.cuCtxPushCurrent = fn("cuCtxPushCurrent_v2", "cuCtxPushCurrent")
        self.cuCtxPopCurrent = fn("cuCtxPopCurrent_v2", "cuCtxPopCurrent")
        self.cuLinkCreate = fn("cuLinkCreate_v2", "cuLinkCreate")
        self.cuLinkAddData = fn("cuLinkAddData_v2", "cuLinkAddData")
        self.cuLinkComplete = fn("cuLinkComplete")
        self.cuLinkDestroy = fn("cuLinkDestroy")
        self.check(self.cuInit(0), "cuInit")
        self.dev = ctypes.c_int()
        self.check(self.cuDeviceGet(ctypes.byref(self.dev), 0), "cuDeviceGet")
        self.ctx = ctypes.c_void_p()
        self.check(self.cuDevicePrimaryCtxRetain(ctypes.byref(self.ctx), self.dev),
                   "cuDevicePrimaryCtxRetain")
        self.check(self.cuCtxPushCurrent(self.ctx), "cuCtxPushCurrent")

    @staticmethod
    def check(r, what, log=b""):
        if r != 0:
            msg = "%s failed (CUresult %d)" % (what, r)
            if log:
                msg += ": " + log.split(b"\0", 1)[0].decode("utf-8", "replace").strip()
            raise CompileError(msg)

    def close(self):
        out = ctypes.c_void_p()
        self.cuCtxPopCurrent(ctypes.byref(out))
        self.cuDevicePrimaryCtxRelease(self.dev)

    def compile(self, ptx, sm, name):
        errlog = ctypes.create_string_buffer(65536)
        infolog = ctypes.create_string_buffer(16384)
        # Position-independent code: calls into the module's own math
        # subroutines (__cuda_sm20_*) go through an address in constant bank
        # 4 (an R_CUDA_64 relocation NVK applies) instead of absolute
        # addresses patched into the instructions, which NVK cannot do.
        opts = (ctypes.c_int * 8)(self.CU_JIT_TARGET, self.CU_JIT_OPTIMIZATION_LEVEL,
                                  self.CU_JIT_ERROR_LOG_BUFFER,
                                  self.CU_JIT_ERROR_LOG_BUFFER_SIZE_BYTES,
                                  self.CU_JIT_INFO_LOG_BUFFER,
                                  self.CU_JIT_INFO_LOG_BUFFER_SIZE_BYTES,
                                  self.CU_JIT_POSITION_INDEPENDENT_CODE, 0)
        vals = (ctypes.c_void_p * 8)(sm, 4, ctypes.cast(errlog, ctypes.c_void_p).value,
                                     len(errlog), ctypes.cast(infolog, ctypes.c_void_p).value,
                                     len(infolog), 1, 0)
        state = ctypes.c_void_p()
        self.check(self.cuLinkCreate(7, opts, vals, ctypes.byref(state)), "cuLinkCreate")
        try:
            text = ptx_text(ptx) + b"\0"
            buf = ctypes.create_string_buffer(text, len(text))
            self.check(self.cuLinkAddData(state, self.CU_JIT_INPUT_PTX, buf,
                                          ctypes.c_size_t(len(text)),
                                          name.encode(), 0, None, None),
                       "cuLinkAddData", errlog.raw)
            cub = ctypes.c_void_p()
            size = ctypes.c_size_t()
            self.check(self.cuLinkComplete(state, ctypes.byref(cub), ctypes.byref(size)),
                       "cuLinkComplete", errlog.raw)
            return ctypes.string_at(cub, size.value)
        finally:
            self.cuLinkDestroy(state)


class Ptxas:
    def __init__(self, path):
        self.path = path

    def close(self):
        pass

    def compile(self, ptx, sm, name):
        with tempfile.TemporaryDirectory(prefix="nvk-cubin-") as d:
            src = os.path.join(d, "in.ptx")
            dst = os.path.join(d, "out.cubin")
            with open(src, "wb") as f:
                f.write(ptx_text(ptx))
            r = subprocess.run([self.path, "-arch=sm_%d" % sm, "-O3", src, "-o", dst],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               timeout=600, check=False)
            if r.returncode != 0:
                raise CompileError("ptxas: " + r.stdout.decode("utf-8", "replace").strip())
            with open(dst, "rb") as f:
                return f.read()


def make_compiler(kind, ptxas):
    if kind in ("auto", "ptxas"):
        path = ptxas or shutil.which("ptxas")
        if path:
            return Ptxas(path)
        if kind == "ptxas":
            raise CompileError("no ptxas (give --ptxas PATH)")
    return CudaJit()


# ---------------------------------------------------------------- commands

def read(path):
    with open(path, "rb") as f:
        return f.read()


def cmd_scan(args):
    for path in args.files:
        buf = read(path)
        fbs, refused = find_fatbins(buf)
        ver = pe_file_version(buf)
        sass = {}
        ptx = {}
        compressed = 0
        problems = {}
        for fb in fbs:
            for e in fb.entries:
                d = sass if e.kind == KIND_SASS else ptx
                d[e.sm] = d.get(e.sm, 0) + 1
                compressed += e.compressed
                if e.kind == KIND_SASS:
                    try:
                        info = parse_cubin(e.data())
                    except FatbinError:
                        continue
                    for p in info.nvk_problems():
                        problems.setdefault(e.sm, set()).add(p.split(" (")[0])
        need = [fb for fb in fbs if not fb.has_sass(args.sm)]
        name = os.path.basename(path)
        print("%s%s: %d fatbins%s" % (name, " " + ver if ver else "", len(fbs),
                                       " (%d refused)" % refused if refused else ""))
        print("  SASS: " + (", ".join("sm_%d x%d" % (k, v) for k, v in sorted(sass.items())) or "none"))
        print("  PTX:  " + (", ".join("sm_%d x%d" % (k, v) for k, v in sorted(ptx.items())) or "none"))
        if compressed:
            print("  compressed entries: %d" % compressed)
        for sm, ps in sorted(problems.items()):
            print("  sm_%d cubins use: %s" % (sm, ", ".join(sorted(ps))))
        nopx = sum(1 for fb in need if fb.ptx_for(args.sm) is None)
        print("  without sm_%d SASS: %d fatbins (%d of them without usable PTX)"
              % (args.sm, len(need), nopx))
        if args.verbose:
            for fb in fbs:
                print("    0x%08x size %d  SASS %s  PTX %s" % (
                    fb.offset, fb.size, ",".join(map(str, fb.sass_sms())),
                    ",".join(map(str, fb.ptx_sms()))))
    return 0


def load_index(path):
    rows = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            for line in f:
                if line.startswith("#") or not line.strip():
                    continue
                cols = line.rstrip("\n").split("\t")
                rows[(cols[0], cols[1])] = cols
    return rows


def write_atomic(path, data):
    tmp = path + ".part"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)


def cmd_build(args):
    os.makedirs(args.out, exist_ok=True)
    index_path = os.path.join(args.out, "index.tsv")
    index = load_index(index_path)
    sm = args.sm
    todo = []                           # (key, entry, source, fatbin offset)
    seen = set()
    for path in args.files:
        buf = read(path)
        fbs, _ = find_fatbins(buf)
        src = os.path.basename(path)
        ver = pe_file_version(buf)
        if ver:
            src += " " + ver
        if not fbs and is_ptx_text(buf):
            # a module NVK dumped as PTX text (<key>.ptx)
            e = Entry()
            e.kind = KIND_PTX
            e.payload = ptx_text(buf)
            e.sm = ptx_target_sm(e.payload)
            key = e.key()
            if key not in seen:
                seen.add(key)
                todo.append((key, e, src, 0))
            continue
        for fb in fbs:
            if fb.has_sass(sm):
                continue
            e = fb.ptx_for(sm)
            if e is None:
                print("%s: fatbin at 0x%x has no SASS for sm_%d and no PTX it can "
                      "compile; skipped" % (src, fb.offset, sm), file=sys.stderr)
                continue
            key = e.key()
            if key in seen:
                continue
            seen.add(key)
            todo.append((key, e, src, fb.offset))
    if not todo:
        print("nothing to compile: every fatbin has sm_%d SASS or no PTX" % sm)
        return 0
    if args.limit:
        todo = todo[:args.limit]

    compiler = None
    made = skipped = failed = 0
    try:
        for n, (key, e, src, off) in enumerate(todo, 1):
            dst = os.path.join(args.out, "%s.sm_%d.cubin" % (key, sm))
            if os.path.exists(dst) and not args.force:
                skipped += 1
                continue
            try:
                ptx = e.data()
            except FatbinError as err:
                print("%s 0x%x: %s" % (src, off, err), file=sys.stderr)
                failed += 1
                continue
            if compiler is None:
                compiler = make_compiler(args.compiler, args.ptxas)
            try:
                cubin = compiler.compile(ptx, sm, "%s_%x" % (key[:12], off))
                info = parse_cubin(cubin)
            except (CompileError, FatbinError) as err:
                print("%s 0x%x (PTX sm_%d): %s" % (src, off, e.sm, err), file=sys.stderr)
                failed += 1
                continue
            if info.sm != sm:
                print("%s 0x%x: compiler made sm_%d, wanted sm_%d" % (src, off, info.sm, sm),
                      file=sys.stderr)
                failed += 1
                continue
            write_atomic(dst, cubin)
            tkey = ptx_text_key(ptx)
            if tkey != key:
                write_atomic(os.path.join(args.out, "%s.sm_%d.cubin" % (tkey, sm)), cubin)
            probs = info.nvk_problems()
            index[(key, str(sm))] = [key, str(sm), str(e.sm), src, "0x%x" % off,
                                     ",".join(info.kernels), "; ".join(probs)]
            made += 1
            if probs:
                print("%s 0x%x: compiled, but uses %s (NVK may not run it)"
                      % (src, off, "; ".join(probs)), file=sys.stderr)
            if args.verbose or n % 50 == 0:
                print("  %d/%d" % (n, len(todo)))
    finally:
        if compiler is not None:
            compiler.close()
        with open(index_path + ".part", "w", encoding="utf-8") as f:
            f.write("# key\tsm\tptx_sm\tsource\tfatbin_offset\tkernels\tnvk_problems\n")
            for row in sorted(index.values()):
                f.write("\t".join(row) + "\n")
        os.replace(index_path + ".part", index_path)
    print("sm_%d: %d compiled, %d already there, %d failed -> %s"
          % (sm, made, skipped, failed, args.out))
    return 1 if failed else 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("scan", help="what code the fatbins in FILE carry")
    s.add_argument("--sm", type=int, default=120, help="GPU SM version (default 120)")
    s.add_argument("-v", "--verbose", action="store_true")
    s.add_argument("files", nargs="+")
    b = sub.add_parser("build", help="compile missing SASS into a cache directory")
    b.add_argument("-o", "--out", required=True, help="cache directory")
    b.add_argument("--sm", type=int, default=120, help="GPU SM version (default 120)")
    b.add_argument("--compiler", choices=("auto", "cuda", "ptxas"), default="auto")
    b.add_argument("--ptxas", help="ptxas binary (CUDA toolkit)")
    b.add_argument("--force", action="store_true", help="recompile existing cubins")
    b.add_argument("--limit", type=int, default=0, help="compile at most N modules (a quick test)")
    b.add_argument("-v", "--verbose", action="store_true")
    b.add_argument("files", nargs="+")
    args = ap.parse_args(argv)
    try:
        return cmd_scan(args) if args.cmd == "scan" else cmd_build(args)
    except CompileError as err:
        print("nvk-cubin-cache: " + str(err), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

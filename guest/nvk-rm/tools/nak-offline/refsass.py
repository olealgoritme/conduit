#!/usr/bin/env python3
"""refsass.py: machine code of OUR pipelines from a reference driver's pipeline
cache data, for nvdisasm.

    refsass.py CACHE_DIR OUT_DIR

CACHE_DIR holds the files `NVSTATS_DUMP=CACHE_DIR nvstats ...` wrote with the
host's NVIDIA Vulkan driver: per pipeline, the data vkGetPipelineCacheData
returns for an empty VkPipelineCache that only that pipeline was created
with.  This looks only at that data, i.e. at the code the driver generated
for our own shaders; nothing of the driver itself is read.

Layout of that data as observed (it may change between driver versions):
the cache header is followed by entries whose payload is one zstd frame
each (with a zstd dictionary stored as an entry of the same blob for some
pipelines); a decompressed entry starts with "NVDA" and an 8-byte tag, then a table
of 32-byte section records (type u32, size u32, offset u64 relative to byte
8); section type 1 is the machine code.  A graphics pipeline has one entry
per stage.  Writes OUT_DIR/<name>.<entry>.bin for every code section bigger
than 1 KiB (the 512-byte vertex shaders of the generated partner stages are
skipped) and prints what it found.  Disassemble with

    nvdisasm -b SM120 OUT_DIR/x.bin      (CUDA 12.8 or newer for SM120)

Needs the zstandard Python module (pip install zstandard).
"""
import os
import struct
import sys

try:
    import zstandard
except ImportError:
    sys.exit("refsass.py needs the zstandard module (pip install zstandard)")

ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"
DICT_MAGIC = b"\x37\xa4\x30\xec"


def dictionaries(data):
    """zstd dictionaries stored in `data` (an entry holding one is preceded
    by its size as a u64); later entries of the same blob may need them"""
    out, i = [], 0
    while True:
        j = data.find(DICT_MAGIC, i)
        if j < 0:
            return out
        if j >= 8:
            size = struct.unpack_from("<Q", data, j - 8)[0]
            if 0 < size <= len(data) - j:
                out.append(zstandard.ZstdCompressionDict(data[j : j + size]))
        i = j + 4


def frames(data):
    """Decompressed zstd frames in `data`, in order"""
    dicts = [None] + dictionaries(data)
    out, i = [], 0
    while True:
        j = data.find(ZSTD_MAGIC, i)
        if j < 0:
            return out
        for dct in dicts:
            try:
                z = zstandard.ZstdDecompressor(dict_data=dct) if dct else zstandard.ZstdDecompressor()
                d = z.decompressobj()
                payload = d.decompress(data[j:])
                out.append(payload)
                i = len(data) - len(d.unused_data)
                break
            except zstandard.ZstdError:
                continue
        else:
            i = j + 4


def code_section(entry):
    if not entry.startswith(b"NVDA"):
        return None
    for off in range(0x28, min(0x400, len(entry) - 16), 32):
        typ, size, offset = struct.unpack_from("<IIQ", entry, off)
        if typ == 1 and 0 < size and offset + 8 + size <= len(entry):
            return entry[offset + 8 : offset + 8 + size]
    return None


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    src, dst = sys.argv[1:]
    os.makedirs(dst, exist_ok=True)
    for f in sorted(os.listdir(src)):
        if not f.endswith(".cache"):
            continue
        name = f[: -len(".cache")]
        name = name[: -len(".spv")] if name.endswith(".spv") else name
        data = open(os.path.join(src, f), "rb").read()
        for k, entry in enumerate(frames(data)):
            code = code_section(entry)
            if code is None or len(code) <= 1024:
                continue
            path = os.path.join(dst, f"{name}.{k}.bin")
            open(path, "wb").write(code)
            print(f"{path}: {len(code)} bytes")


if __name__ == "__main__":
    main()

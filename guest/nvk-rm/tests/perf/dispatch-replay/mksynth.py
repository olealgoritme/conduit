#!/usr/bin/env python3
"""Write synthetic NVK_DISPATCH_CAPTURE directories for testing dispatch-replay without a guest capture.

The manifests have the fields the driver writes (nvk_dispatch_capture.c); the shaders are real
vkd3d-proton SPIR-V (<hash>.spv files from VKD3D_SHADER_DUMP_PATH), the data is random but shaped like the game's
(the resolution constant in the root CBV at vec4 72, matrices, a depth-like image below 1.0).

  mksynth.py OUT_DIR --shaderdump DIR [--width W --height H]

Writes OUT_DIR/94b6df807719aaad-synth and OUT_DIR/1612706b944e9526-synth.
"""
import argparse
import json
import os
import struct

import numpy as np

VK = dict(R32_UINT=98, R32_SFLOAT=100, R16G16B16A16_SFLOAT=97, R8G8B8A8_UNORM=37, R11G11B10=122)
SAMPLED_IMAGE, STORAGE_IMAGE, SAMPLER = 2, 3, 0
IMAGE_2D, VIEW_2D = 1, 1
BPP = {98: 4, 100: 4, 97: 8, 37: 4, 122: 4}


class Capture:
    def __init__(self, out, src_hash, spv, local, grid, push_size):
        self.dir = os.path.join(out, src_hash + "-synth")
        os.makedirs(self.dir, exist_ok=True)
        with open(spv, "rb") as f:
            open(os.path.join(self.dir, "shader.spv"), "wb").write(f.read())
        self.m = {"format": "nvk-dispatch-capture", "version": 1, "src_hash": src_hash, "code_hash": "00000000",
                  "capture": 1, "call_index": 0, "time_s": 0.0, "pid": 0, "compute_only_queue": False,
                  "local_size": local, "base_group": [0, 0, 0], "grid": grid, "indirect": False,
                  "spirv": "shader.spv", "entry_point": "main", "push": "push.bin", "push_size": push_size,
                  "post": False, "unsupported": "", "bindings": [], "resources": []}
        self.push = bytearray(256)

    def binding(self, set_, b, dtype, array, writable=False, dim=1, stride=16):
        self.m["bindings"].append({"set": set_, "binding": b, "descriptor_type": dtype, "descriptor_type_name": "",
                                   "array": array, "writable": writable, "dim": dim, "arrayed": 0, "depth": 0,
                                   "nvk_layout": {"have": True, "type": 1000351000 if array else dtype,
                                                  "stride": stride, "array_size": 1000000, "set_flags": 2}})

    def _file(self, rid, data):
        name = "r%03u.bin" % rid
        open(os.path.join(self.dir, name), "wb").write(data)
        return name

    def image(self, set_, b, index, dtype, fmt, w, h, data, pc_off, add, writable=False):
        rid = len(self.m["resources"])
        size = w * h * BPP[fmt]
        assert len(data) == size
        r = {"id": rid, "kind": "image", "set": set_, "binding": b, "index": index, "descriptor_type": dtype,
             "descriptor_type_name": "", "push_offset": pc_off, "add": add, "writable": writable,
             "file": self._file(rid, data), "size": size, "image_type": IMAGE_2D, "view_type": VIEW_2D,
             "image_format": fmt, "image_format_name": "", "view_format": fmt, "view_format_name": "",
             "copy_format": fmt, "copy_format_name": "", "aspect": 1, "extent": [w, h, 1], "base_mip": 0,
             "mips": 1, "base_layer": 0, "layers": 1, "swizzle": [0, 0, 0, 0], "min_lod": 0,
             "subresources": [{"mip": 0, "extent": [w, h, 1], "layers": 1, "row_stride": w * BPP[fmt],
                               "image_stride": size, "offset": 0, "size": size}]}
        self.m["resources"].append(r)

    def sampler(self, set_, b, index, pc_off, add, **kw):
        rid = len(self.m["resources"])
        s = {"mag_filter": 1, "min_filter": 1, "mipmap_mode": 0, "address_mode": [2, 2, 2], "mip_lod_bias": 0,
             "anisotropy_enable": 0, "max_anisotropy": 1, "compare_enable": 0, "compare_op": 0, "min_lod": 0,
             "max_lod": 1000, "border_color": 0, "border_color_value": [0, 0, 0, 0], "custom_border_format": 0,
             "unnormalized_coordinates": 0, "reduction_mode": 0, "flags": 0}
        s.update(kw)
        self.m["resources"].append({"id": rid, "kind": "sampler", "set": set_, "binding": b, "index": index,
                                    "descriptor_type": SAMPLER, "descriptor_type_name": "VK_DESCRIPTOR_TYPE_SAMPLER",
                                    "push_offset": pc_off, "add": add, "writable": False, "sampler_index": 1 + rid,
                                    "sampler": s})

    def cbv(self, pc_off, data, size=65536):
        rid = len(self.m["resources"])
        data = bytes(data) + bytes(max(0, size - len(data)))
        self.m["resources"].append({"id": rid, "kind": "root_address", "push_offset": pc_off, "add": 0,
                                    "writable": False, "file": self._file(rid, data), "size": len(data),
                                    "address": "0x%x" % (0x200000000 + rid * 0x10000), "range": size,
                                    "allocation_left": size, "truncated": False})
        struct.pack_into("<Q", self.push, pc_off, 0x200000000 + rid * 0x10000)

    def u32(self, off, v):
        struct.pack_into("<I", self.push, off, v)

    def write(self):
        open(os.path.join(self.dir, "push.bin"), "wb").write(self.push)
        json.dump(self.m, open(os.path.join(self.dir, "manifest.json"), "w"), indent=1)
        print("wrote", self.dir)


def f16(a):
    return a.astype(np.float16).tobytes()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--shaderdump", required=True, help="directory with 94b6df807719aaad.spv and 1612706b944e9526.spv")
    ap.add_argument("--width", type=int, default=1920)
    ap.add_argument("--height", type=int, default=1080)
    a = ap.parse_args()
    rng = np.random.default_rng(1234)
    W, H = a.width, a.height

    def cb0():
        v = np.zeros((4096, 4), np.float32)
        v[72] = [W, H, 1.0 / W, 1.0 / H]
        v[20:24] = np.eye(4, dtype=np.float32) + rng.normal(0, 0.05, (4, 4)).astype(np.float32)
        v[0:20] = rng.random((20, 4), dtype=np.float32)
        v[24:72] = rng.random((48, 4), dtype=np.float32)
        return v.tobytes()

    # 94b6df807719aaad: local 16x16; heap table at push word 56: +0 depth-like float, +1 float4,
    # +2 uint, +3 storage float4 (written). Root CBVs at push 0, 8, 16, 40.
    c = Capture(a.out, "94b6df807719aaad", os.path.join(a.shaderdump, "94b6df807719aaad.spv"), [16, 16, 1],
                [(W + 15) // 16, (H + 15) // 16, 1], 68)
    c.binding(1, 2, SAMPLED_IMAGE, True)
    c.binding(1, 2, SAMPLED_IMAGE, True)
    c.binding(1, 2, STORAGE_IMAGE, True, writable=True)
    base = 5000
    c.u32(56, base)
    c.image(1, 2, base + 3, STORAGE_IMAGE, VK["R16G16B16A16_SFLOAT"], W, H, f16(rng.random((H, W, 4))), 56, 3, True)
    c.image(1, 2, base + 2, SAMPLED_IMAGE, VK["R32_UINT"], W, H,
            rng.integers(0, 256, (H, W), dtype=np.uint32).tobytes(), 56, 2)
    c.image(1, 2, base + 1, SAMPLED_IMAGE, VK["R16G16B16A16_SFLOAT"], W, H, f16(rng.random((H, W, 4))), 56, 1)
    c.image(1, 2, base + 0, SAMPLED_IMAGE, VK["R32_SFLOAT"], W, H,
            (rng.random((H, W), dtype=np.float32) * 0.999).tobytes(), 56, 0)
    for off in (0, 8, 16, 40):
        c.cbv(off, cb0())
    c.write()

    # 1612706b944e9526: local 16x16; heap table at push word 8 (+0..+6), samplers at push word 12.
    c = Capture(a.out, "1612706b944e9526", os.path.join(a.shaderdump, "1612706b944e9526.spv"), [16, 16, 1],
                [(W + 15) // 16, (H + 15) // 16, 1], 16)
    c.binding(1, 2, SAMPLED_IMAGE, True)
    c.binding(1, 2, STORAGE_IMAGE, True, writable=True)
    c.binding(0, 0, SAMPLER, True, stride=4)
    base, sbase = 70000, 7
    c.u32(8, base)
    c.u32(12, sbase)
    # The SPIR-V reads +0..+4 as sampled images and writes +5, +6 as storage images
    for k in range(7):
        st = k >= 5
        c.image(1, 2, base + k, STORAGE_IMAGE if st else SAMPLED_IMAGE, VK["R16G16B16A16_SFLOAT"], W, H,
                f16(rng.random((H, W, 4))), 8, k, st)
    c.sampler(0, 0, sbase, 12, 0)
    c.sampler(0, 0, sbase + 1, 12, 1, mag_filter=0, min_filter=0)
    c.cbv(0, cb0())
    c.write()


if __name__ == "__main__":
    main()

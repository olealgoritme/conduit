#!/usr/bin/env python3
"""Generate the names the trace decoder prints for numbers on the wire.

A trace records what the guest sent: an RM class id, a control command, an
NVKMS command index, a UVM command number, a DRM ioctl number. This script
reads the names for those numbers out of NVIDIA's own headers so a trace can
say `NV2080_CTRL_CMD_GPU_GET_INFO_V2` instead of `0x20800102`.

    ./names_extract.py \\
        --release 535.129.03=~/src/ogkm-535.129.03 \\
        --release 580.178.04=~/src/ogkm-580.178.04 \\
        ... \\
        --drm-header /usr/include/drm/drm.h > src/names/table.rs

What comes out:

  * RM classes, from `src/nvidia/generated/g_allclasses.h`, and RM control
    commands, from every `ctrl/*.h` under the SDK. NVIDIA never reuses one of
    these numbers for something else, so the releases are merged into one
    table; where two releases name the same number differently, the newest
    release's name is kept.
  * UVM commands, from `uvm_ioctl.h` and `uvm_linux_ioctl.h`, merged the same
    way.
  * NVKMS commands, per release. NVKMS numbers its commands by their position
    in `enum NvKmsIoctlCommand`, and that enum gains and loses entries between
    releases (REGISTER_SURFACE is 16 in 535, 17 from 580, 16 again in 615), so
    each release gets its own list.
  * DRM core ioctls from the kernel's `drm.h`, and the nvidia-drm driver
    ioctls from `nv_drm_common_ioctl.h` (numbered from DRM_COMMAND_BASE).

Names are for display only; nothing in the backend decides anything by them.
"""

import argparse
import os
import re
import sys

CTRL_RE = re.compile(r"^#define\s+(NV[0-9A-Z]{4}_CTRL_CMD_\w+)\s+\(\s*(0x[0-9a-fA-F]+)[uU]?\s*\)")
CLASS_RE = re.compile(r"^#define\s+([A-Z][A-Z0-9_]+)\s+\((0x[0-9a-fA-F]{8})\)(.*)$")
UVM_BASE_RE = re.compile(r"^#define\s+(UVM_\w+)\s+UVM_IOCTL_BASE\((\d+)\)")
UVM_HEX_RE = re.compile(r"^#define\s+(UVM_\w+)\s+(0x[0-9a-fA-F]+)\s*$")
DRM_RE = re.compile(r"^#define\s+DRM_IOCTL_(\w+)\s+DRM_IO(?:R|W|WR)?\(\s*(0x[0-9a-fA-F]+)")
NVDRM_RE = re.compile(r"^#define\s+DRM_NVIDIA_(\w+)\s+(0x[0-9a-fA-F]+)\s*$")

DRM_COMMAND_BASE = 0x40


def version_key(v):
    return tuple(int(x) for x in v.split("."))


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read().splitlines()


def controls(root):
    base = os.path.join(root, "src/common/sdk/nvidia/inc/ctrl")
    out = {}
    for dirpath, _, names in os.walk(base):
        for n in sorted(names):
            if not n.endswith(".h"):
                continue
            for line in read(os.path.join(dirpath, n)):
                m = CTRL_RE.match(line)
                if m:
                    out.setdefault(int(m.group(2), 16), m.group(1))
    if not out:
        sys.exit(f"{root}: no RM control commands found under {base}")
    return out


def classes(root):
    path = os.path.join(root, "src/nvidia/generated/g_allclasses.h")
    out = {}
    for line in read(path):
        m = CLASS_RE.match(line)
        # The first name for a value is the canonical one; the rest are
        # marked `// alias`.
        if m and "alias" not in m.group(3):
            out.setdefault(int(m.group(2), 16), m.group(1))
    if not out:
        sys.exit(f"{path}: no classes found")
    return out


def uvm(root):
    out = {}
    for f in ("uvm_ioctl.h", "uvm_linux_ioctl.h"):
        path = os.path.join(root, "kernel-open/nvidia-uvm", f)
        for line in read(path):
            m = UVM_BASE_RE.match(line)
            if m:
                out.setdefault(int(m.group(2)), m.group(1))
                continue
            m = UVM_HEX_RE.match(line)
            if m and m.group(1) in ("UVM_INITIALIZE", "UVM_DEINITIALIZE"):
                out.setdefault(int(m.group(2), 16), m.group(1))
    if not out:
        sys.exit(f"{root}: no UVM commands found")
    return out


def nvkms(root):
    path = os.path.join(root, "src/nvidia-modeset/interface/nvkms-api.h")
    text = "\n".join(read(path))
    m = re.search(r"enum\s+NvKmsIoctlCommand\s*\{(.*?)\};", text, re.S)
    if not m:
        sys.exit(f"{path}: no enum NvKmsIoctlCommand")
    body = re.sub(r"/\*.*?\*/", "", m.group(1), flags=re.S)
    body = re.sub(r"//[^\n]*", "", body)
    names = []
    for item in body.split(","):
        item = item.strip()
        if not item:
            continue
        if "=" in item:
            sys.exit(f"{path}: {item!r} has an explicit value; positions would be wrong")
        names.append(item)
    return names


def nvidia_drm(root):
    path = os.path.join(root, "kernel-open/nvidia-drm/nv_drm_common_ioctl.h")
    if not os.path.exists(path):
        path = os.path.join(root, "kernel-open/nvidia-drm/nvidia-drm-ioctl.h")
    out = {}
    for line in read(path):
        m = NVDRM_RE.match(line)
        if m:
            out.setdefault(DRM_COMMAND_BASE + int(m.group(2), 16), "NVIDIA_" + m.group(1))
    return out


def drm_core(path):
    out = {}
    for line in read(path):
        m = DRM_RE.match(line)
        if m:
            out.setdefault(int(m.group(2), 16), m.group(1))
    if not out:
        sys.exit(f"{path}: no DRM ioctls found")
    return out


def merge(tables):
    """Merge per-release tables, oldest first, so the newest name wins."""
    out = {}
    for t in tables:
        out.update(t)
    return out


def emit_pairs(name, doc, table):
    lines = [f"/// {doc}", f"pub static {name}: &[(u32, &str)] = &["]
    for k in sorted(table):
        lines.append(f'    ({k:#010x}, "{table[k]}"),')
    lines.append("];")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--release", action="append", required=True,
                    metavar="VERSION=DIR",
                    help="an open-gpu-kernel-modules checkout at that release")
    ap.add_argument("--drm-header", default="/usr/include/drm/drm.h")
    a = ap.parse_args()

    releases = []
    for r in a.release:
        v, _, d = r.partition("=")
        if not d:
            sys.exit(f"--release {r}: expected VERSION=DIR")
        releases.append((v, os.path.expanduser(d)))
    releases.sort(key=lambda r: version_key(r[0]))

    ctrl = merge(controls(d) for _, d in releases)
    cls = merge(classes(d) for _, d in releases)
    uvmt = merge(uvm(d) for _, d in releases)
    drm = drm_core(a.drm_header)
    drm.update(merge(nvidia_drm(d) for _, d in releases))

    out = [
        "// Generated by gen/names_extract.py -- do not edit by hand.",
        "//",
        "//   ./names_extract.py " + " ".join(f"--release {v}=<ogkm {v}>" for v, _ in releases),
        "//       --drm-header <linux uapi drm.h> > src/names/table.rs",
        "//",
        "// Display names for the numbers a trace records. Not used to decide anything.",
        "",
        "use crate::version::DriverVersion;",
        "",
        emit_pairs("CLASSES", "RM classes, by class id.", cls),
        "",
        emit_pairs("CONTROLS", "RM control commands, by command number.", ctrl),
        "",
        emit_pairs("UVM", "UVM commands, by ioctl number.", uvmt),
        "",
        emit_pairs("DRM", "DRM ioctls, by ioctl nr (core, then nvidia-drm from 0x40).", drm),
        "",
        "/// NVKMS commands by position in `enum NvKmsIoctlCommand`, per release.",
        "pub static NVKMS: &[(DriverVersion, &[&str])] = &[",
    ]
    for v, d in releases:
        maj, mnr, pat = version_key(v)
        out.append(f"    (DriverVersion::new({maj}, {mnr}, {pat}), &[")
        for n in nvkms(d):
            out.append(f'        "{n}",')
        out.append("    ]),")
    out.append("];")
    print("\n".join(out))


if __name__ == "__main__":
    main()

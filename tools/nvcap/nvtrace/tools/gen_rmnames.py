#!/usr/bin/env python3
"""Build tools/rmnames.json: names for NVIDIA RM numbers, read from
NVIDIA's open-gpu-kernel-modules headers at one release tag.

    tools/gen_rmnames.py --repo /steam/refsrc/open-gpu-kernel-modules \\
        --tag 610.57.04 > tools/rmnames.json

Output keys (numbers as decimal strings in JSON objects):
  classes   RM class id -> name           (g_allclasses.h, class/cl*.h)
  ctrls     RM control command -> name    (ctrl/**/*.h)
  escapes   NV_ESC_RM_* number -> name    (nv_escape.h)
  status    NV_STATUS value -> name       (nvstatuscodes.h)
"""

import argparse
import json
import re
import subprocess
import sys

CTRL_RE = re.compile(r"^#define\s+(NV[0-9A-Z]{4}_CTRL_CMD_\w+)\s+\(?\s*(0x[0-9a-fA-F]+)[uU]?\s*\)?")
CLASS_RE = re.compile(r"^#define\s+([A-Z][A-Z0-9_]+)\s+\(\s*(0x[0-9a-fA-F]{8})\s*\)")
ESC_RE = re.compile(r"^#define\s+(NV_ESC_\w+)\s+\(?\s*(0x[0-9a-fA-F]+|\d+)\s*\)?")
STATUS_RE = re.compile(r"NV_STATUS_CODE\(\s*(NV_\w+)\s*,\s*(0x[0-9a-fA-F]+)")


def git(repo, *args):
    return subprocess.run(["git", "-C", repo] + list(args), check=True,
                          capture_output=True).stdout.decode("utf-8", "replace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tag", required=True)
    a = ap.parse_args()

    files = git(a.repo, "ls-tree", "-r", "--name-only", a.tag).split("\n")
    classes, ctrls, escapes, status = {}, {}, {}, {}

    def read(path):
        return git(a.repo, "show", "%s:%s" % (a.tag, path))

    for f in files:
        if f.startswith("src/common/sdk/nvidia/inc/ctrl/") and f.endswith(".h"):
            for line in read(f).split("\n"):
                m = CTRL_RE.match(line)
                if m and not m.group(1).endswith("_PARAMS_MESSAGE_ID"):
                    ctrls.setdefault(int(m.group(2), 16), m.group(1))
        elif f == "src/nvidia/generated/g_allclasses.h":
            # canonical names first, aliases only for numbers without one
            lines = read(f).split("\n")
            for alias_pass in (False, True):
                for line in lines:
                    m = CLASS_RE.match(line)
                    if m and ("// alias" in line) == alias_pass:
                        classes.setdefault(int(m.group(2), 16), m.group(1))
    for line in read("src/nvidia/arch/nvalloc/unix/include/nv_escape.h").split("\n"):
        m = ESC_RE.match(line)
        if m:
            escapes.setdefault(int(m.group(2), 0), m.group(1))
    for line in read("src/common/sdk/nvidia/inc/nvstatuscodes.h").split("\n"):
        m = STATUS_RE.search(line)
        if m:
            status.setdefault(int(m.group(2), 16), m.group(1))

    out = {
        "source": "open-gpu-kernel-modules %s" % a.tag,
        "classes": {str(k): v for k, v in sorted(classes.items())},
        "ctrls": {str(k): v for k, v in sorted(ctrls.items())},
        "escapes": {str(k): v for k, v in sorted(escapes.items())},
        "status": {str(k): v for k, v in sorted(status.items())},
    }
    json.dump(out, sys.stdout, indent=0, sort_keys=False)
    sys.stdout.write("\n")
    sys.stderr.write("classes %d ctrls %d escapes %d status %d\n" % (
        len(classes), len(ctrls), len(escapes), len(status)))


if __name__ == "__main__":
    main()

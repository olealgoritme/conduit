#!/usr/bin/env python3
"""Find NVIDIA driver releases with no ABI tables yet and generate them.

Used by .github/workflows/abi.yml; runs locally too:

    .github/scripts/abi_update.py --gvisor ~/forks/gvisor --report /tmp/abi.md

Two sources, two kinds of table (see host/backend/gen/README.md):

  gVisor nvproxy (addDriverABI(...) in version.go)
      -> nvabi_gen.py      -> gen/src/versions/vX.rs
  open-gpu-kernel-modules release tags
      -> rmctrl_extract.py -> gen/src/rmctrl/vX.rs and guest/linux/rmctrl/vX.h
      -> rmallow/uvm/osdesc/vidmem/devinfo_extract.py -> gen/src/<table>/vX.rs
      -> devinfo_extract.py --lang c -> guest/linux/devinfo/vX.h
      -> nvgpu_gen.py (newest release only) -> guest/linux/gen/

New modules are added to each table's mod.rs as `pub mod vX;`. Anything a
generator refuses (an unclassified entry) is left out and listed in the
report, so a person reads it: the PR is a starting point, not a merge-blind
update.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GEN = ROOT / "host" / "backend" / "gen"
GUEST = ROOT / "guest" / "linux"
OGKM_URL = "https://github.com/NVIDIA/open-gpu-kernel-modules"

# ogkm-derived tables under gen/src/: (directory, generator).
OGKM_TABLES = [
    ("rmctrl", "rmctrl_extract.py"),
    ("rmallow", "rmallow_extract.py"),
    ("uvm", "uvm_extract.py"),
    ("osdesc", "osdesc_extract.py"),
    ("vidmem", "vidmem_extract.py"),
    ("devinfo", "devinfo_extract.py"),
]

Version = tuple[int, int, int]


def stem(v: Version) -> str:
    # Matches the generators' own naming: v580_178_04, v620_6_00.
    return f"v{v[0]}_{v[1]}_{v[2]:02}"


def dotted(v: Version) -> str:
    return f"{v[0]}.{v[1]:02}.{v[2]:02}"


def parse(s: str) -> Version | None:
    m = re.fullmatch(r"(\d+)\.(\d+)(?:\.(\d+))?", s)
    return (int(m[1]), int(m[2]), int(m[3] or 0)) if m else None


def existing(table_dir: Path, ext: str = ".rs") -> set[Version]:
    out = set()
    for p in table_dir.glob(f"v*{ext}"):
        m = re.fullmatch(r"v(\d+)_(\d+)_(\d+)", p.stem)
        if m:
            out.add((int(m[1]), int(m[2]), int(m[3])))
    return out


def ogkm_tags() -> dict[Version, str]:
    out = subprocess.run(["git", "ls-remote", "--tags", "--refs", OGKM_URL],
                         check=True, capture_output=True, text=True).stdout
    tags = {}
    for line in out.splitlines():
        tag = line.split("refs/tags/", 1)[-1]
        v = parse(tag)
        if v:
            tags[v] = tag
    return tags


def gvisor_versions(gvisor: Path) -> set[Version]:
    src = (gvisor / "pkg/sentry/devices/nvproxy/version.go").read_text()
    return {(int(a), int(b), int(c))
            for a, b, c in re.findall(r"addDriverABI\(\s*(\d+),\s*(\d+),\s*(\d+)", src)}


def pick(candidates: set[Version], have: set[Version], limit: int) -> list[Version]:
    """Missing releases newer than the oldest table we keep, newest first."""
    floor = min(have) if have else (0, 0, 0)
    missing = sorted((v for v in candidates - have if v > floor), reverse=True)
    return missing[:limit]


def run_to_file(cmd: list[str], dst: Path, cwd: Path) -> str | None:
    """Run a generator that prints its table; write it only on success."""
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if r.returncode != 0:
        return (r.stderr or r.stdout).strip()[-2000:] or f"exit {r.returncode}"
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_text(r.stdout)
    return None


def register(mod_rs: Path, new: Version) -> None:
    """Add `pub mod vX;` to mod.rs, keeping the version modules sorted."""
    text = mod_rs.read_text()
    lines = text.splitlines(keepends=True)
    pat = re.compile(r"^pub mod v(\d+)_(\d+)_(\d+);\s*$")
    idx = [i for i, l in enumerate(lines) if pat.match(l)]
    entry = f"pub mod {stem(new)};\n"
    if entry in lines:
        return
    if not idx:
        mod_rs.write_text(entry + text)
        return
    block = [lines[i] for i in idx] + [entry]
    block.sort(key=lambda l: tuple(int(x) for x in pat.match(l).groups()))
    first, last = idx[0], idx[-1]
    # Only rewrite a contiguous block; otherwise insert after the last one.
    if last - first + 1 == len(idx):
        lines[first:last + 1] = block
    else:
        lines.insert(last + 1, entry)
    mod_rs.write_text("".join(lines))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gvisor", type=Path, required=True, help="gVisor checkout (main)")
    ap.add_argument("--max", type=int, default=4, help="new releases per source per run")
    ap.add_argument("--report", type=Path, required=True, help="Markdown summary for the PR")
    ap.add_argument("--dry-run", action="store_true", help="only list what is missing")
    a = ap.parse_args()

    py = sys.executable
    report: list[str] = []
    problems: list[str] = []
    added: list[str] = []

    # --- gVisor nvproxy -> gen/src/versions -------------------------------
    have = existing(GEN / "src/versions")
    todo = pick(gvisor_versions(a.gvisor), have, a.max)
    report.append(f"gVisor nvproxy releases without a table: "
                  f"{', '.join(map(dotted, todo)) or 'none'}")
    for v in [] if a.dry_run else todo:
        dst = GEN / "src/versions" / f"{stem(v)}.rs"
        err = run_to_file([py, "nvabi_gen.py", "--gvisor", str(a.gvisor),
                           "--version", dotted(v)], dst, GEN)
        if err:
            problems.append(f"`nvabi_gen.py` {dotted(v)}:\n```\n{err}\n```")
        else:
            register(GEN / "src/versions/mod.rs", v)
            added.append(f"versions/{dst.name}")

    # --- open-gpu-kernel-modules -> the other tables ----------------------
    tags = ogkm_tags()
    have = existing(GEN / "src/rmctrl")
    todo = pick(set(tags), have, a.max)
    report.append(f"open-gpu-kernel-modules releases without tables: "
                  f"{', '.join(map(dotted, todo)) or 'none'}")
    newest = max(set(tags) | have) if tags else None

    for v in [] if a.dry_run else todo:
        with tempfile.TemporaryDirectory() as tmp:
            ogkm = Path(tmp) / "ogkm"
            subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", tags[v],
                            OGKM_URL, str(ogkm)], check=True)
            for table, script in OGKM_TABLES:
                dst = GEN / "src" / table / f"{stem(v)}.rs"
                err = run_to_file([py, script, "--ogkm", str(ogkm), "--version", dotted(v)],
                                  dst, GEN)
                if err:
                    problems.append(f"`{script}` {dotted(v)}:\n```\n{err}\n```")
                    continue
                register(GEN / "src" / table / "mod.rs", v)
                added.append(f"{table}/{dst.name}")
            # The guest's copy of the rmctrl table.
            dst = GUEST / "rmctrl" / f"{stem(v)}.h"
            err = run_to_file([py, "rmctrl_extract.py", "--ogkm", str(ogkm),
                               "--version", dotted(v), "--lang", "c"], dst, GEN)
            if err:
                problems.append(f"`rmctrl_extract.py --lang c` {dotted(v)}:\n```\n{err}\n```")
            else:
                added.append(f"guest rmctrl/{dst.name}")
            # The guest's copy of the GET_DEV_INFO layout.
            dst = GUEST / "devinfo" / f"{stem(v)}.h"
            err = run_to_file([py, "devinfo_extract.py", "--ogkm", str(ogkm),
                               "--version", dotted(v), "--lang", "c"], dst, GEN)
            if err:
                problems.append(f"`devinfo_extract.py --lang c` {dotted(v)}:\n```\n{err}\n```")
            else:
                added.append(f"guest devinfo/{dst.name}")
            # The guest's allocation tables follow the newest release only.
            if v == newest:
                r = subprocess.run([py, "nvgpu_gen.py", "--src", str(ogkm),
                                    "--out", str(GUEST / "gen"), "--version", dotted(v)],
                                   cwd=GEN, capture_output=True, text=True)
                if r.returncode:
                    problems.append(f"`nvgpu_gen.py` {dotted(v)}:\n```\n{r.stderr[-2000:]}\n```")
                else:
                    added.append(f"guest gen/ regenerated from {dotted(v)}")

    lines = ["## NVIDIA ABI update", "", *[f"- {r}" for r in report], ""]
    if added:
        lines += ["### Generated", "", *[f"- `{x}`" for x in added], ""]
    if problems:
        lines += ["### Needs a person",
                  "", "These generators stopped (usually an entry they cannot "
                  "classify). Their tables are not in this PR.", "", *problems, ""]
    lines += ["### Before merging", "",
              "- [ ] tests pass (see the workflow run; a failing run opens this PR as a draft)",
              "- [ ] any per-version dispatch outside `mod.rs` updated",
              "- [ ] probe set run on a host with the new release (gen/README.md step 4)", ""]
    a.report.write_text("\n".join(lines))
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""nak-stats.py OUT_DIR HEAD_BUILD BASE_BUILD CHIPSET...: the Markdown
summary of a nak-shaders.sh run (BASE_BUILD may be an empty string).

Reads OUT_DIR/head-CHIP.txt and base-CHIP.txt (run-objects.sh lines:
"file stage instrs gprs warps static_cycles spills fills", or FAILED),
head-spill-CHIP.txt, skipped.txt, glslang-failed.txt and failures.txt."""
import collections
import os
import sys

CHIPS = {"1b2": "GB202 (SM120)", "1b0": "GB100 (SM100)", "192": "AD102 (SM89)",
         "172": "GA102 (SM86)", "162": "TU102 (SM75)"}
STAGES = ["vert", "tesc", "tese", "geom", "frag", "comp"]
# column index in the stats tuple, label, whether lower is better
FIELDS = [(0, "instructions"), (3, "static cycles"), (1, "GPRs"), (2, "warps/SM")]
TOP = 25


def load(path):
    """{shader: (stage, stats tuple or None)}"""
    r = {}
    if not os.path.exists(path):
        return r
    for line in open(path):
        x = line.split()
        if len(x) < 3:
            continue
        stats = None if "FAILED" in x[2:] or len(x) < 8 else tuple(map(int, x[2:8]))
        r[x[0]] = (x[1], stats)
    return r


def read_lines(path):
    return [l.rstrip("\n") for l in open(path)] if os.path.exists(path) else []


def pct(a, b):
    return f"{100 * (b - a) / a:+.2f}%" if a else ("0" if a == b else "new")


def delta(a, b):
    if a == b:
        return f"{b}"
    return f"{a} → {b} ({pct(a, b)})"


def stage_of(shader, head, base):
    s = head.get(shader, base.get(shader, ("-", None)))[0]
    return s if s != "-" else shader.rsplit(".", 1)[-1]


def totals_table(head, base):
    """per-stage totals; with a baseline, over the shaders both compiled"""
    rows = []
    groups = collections.defaultdict(list)
    for name in set(head) | set(base):
        groups[stage_of(name, head, base)].append(name)
    order = [s for s in STAGES if s in groups] + sorted(set(groups) - set(STAGES))
    with_base = bool(base)
    hdr = ["stage", "shaders", "failed"] + [f[1] for f in FIELDS] + ["spills+fills"]
    if with_base:
        hdr.append("GPRs up/down, warps down/up")
    rows.append("| " + " | ".join(hdr) + " |")
    rows.append("|" + "---|" * len(hdr))
    for stage in order + ["all"]:
        names = [n for s in order for n in groups[s]] if stage == "all" else groups[stage]
        failed = sum(1 for n in names if head.get(n, (None, None))[1] is None)
        both = [n for n in names if head.get(n, (0, None))[1] is not None
                and (not with_base or base.get(n, (0, None))[1] is not None)]
        tb = [0] * 6
        th = [0] * 6
        for n in both:
            for i in range(6):
                th[i] += head[n][1][i]
                if with_base:
                    tb[i] += base[n][1][i]
        cells = [f"**{stage}**" if stage == "all" else stage, str(len(names)), str(failed)]
        for idx, _ in FIELDS:
            cells.append(delta(tb[idx], th[idx]) if with_base else str(th[idx]))
        sp_h, sp_b = th[4] + th[5], tb[4] + tb[5]
        cells.append(delta(sp_b, sp_h) if with_base else str(sp_h))
        if with_base:
            gu = sum(1 for n in both if head[n][1][1] > base[n][1][1])
            gd = sum(1 for n in both if head[n][1][1] < base[n][1][1])
            wd = sum(1 for n in both if head[n][1][2] < base[n][1][2])
            wu = sum(1 for n in both if head[n][1][2] > base[n][1][2])
            cells.append(f"{gu}/{gd}, {wd}/{wu}")
        rows.append("| " + " | ".join(cells) + " |")
    return rows


def changed_table(head, base):
    changed = []
    for n in sorted(set(head) & set(base)):
        h, b = head[n][1], base[n][1]
        if h is None or b is None:
            if (h is None) != (b is None):
                changed.append((float("inf"), n, b, h))
            continue
        if h != b:
            changed.append((abs(h[0] - b[0]) / max(b[0], 1), n, b, h))
    if not changed:
        return ["No shader's statistics changed."]
    changed.sort(key=lambda c: (-c[0], c[1]))
    rows = [f"{len(changed)} shaders changed; the {min(TOP, len(changed))} with the "
            "largest relative change in instructions:", "",
            "| shader | instructions | GPRs | warps/SM | static cycles | spills+fills |",
            "|---|---|---|---|---|---|"]
    for _, n, b, h in changed[:TOP]:
        if h is None or b is None:
            rows.append(f"| `{n}` | " + ("now fails" if h is None else "now compiles") + " | | | | |")
            continue
        rows.append(f"| `{n}` | {delta(b[0], h[0])} | {delta(b[1], h[1])} | {delta(b[2], h[2])} | "
                    f"{delta(b[3], h[3])} | {delta(b[4] + b[5], h[4] + h[5])} |")
    return rows


def main():
    out, head_build, base_build = sys.argv[1:4]
    chips = sys.argv[4:]
    md = ["## NAK shader statistics", ""]
    head_series = read_lines(os.path.join(head_build, "series.txt"))
    head_base = read_lines(os.path.join(head_build, "mesa-base.txt"))
    md.append(f"NVK and NAK from Mesa `{head_base[0][:12] if head_base else '?'}` with "
              f"{len(head_series)} patches (`guest/nvk-rm/series.sh`), on nouveau's drm-shim. "
              "Each shader is compiled alone as a shader object; numbers are NVK's "
              "`NVK_SHADER_STATS`.")
    if base_build:
        base_series = read_lines(os.path.join(base_build, "series.txt"))
        md.append(f"Baseline: the same build of the base commit's series ({len(base_series)} patches).")
    else:
        md.append("No baseline: the base commit has no `guest/nvk-rm/series.sh` or its build "
                  "failed, so only totals are shown.")
    corpus = sorted(os.listdir(os.path.join(out, "corpus")))
    by_group = collections.Counter(n.split(".", 1)[0] for n in corpus)
    md.append("")
    md.append(f"Corpus: {len(corpus)} shaders (" +
              ", ".join(f"{g} {c}" for g, c in sorted(by_group.items())) + ").")
    skipped = read_lines(os.path.join(out, "skipped.txt"))
    if skipped:
        md.append("Left out, as NVK does not support what they need: " +
                  ", ".join(f"`{s}`" for s in skipped) + ".")
    bad = read_lines(os.path.join(out, "glslang-failed.txt"))
    if bad:
        md.append(f"glslang could not compile {len(bad)}: " +
                  ", ".join(f"`{s}`" for s in bad) + ".")

    for chip in chips:
        head = load(os.path.join(out, f"head-{chip}.txt"))
        base = load(os.path.join(out, f"base-{chip}.txt")) if base_build else {}
        md += ["", f"### {CHIPS.get(chip, chip)}, NOUVEAU_CHIPSET={chip}", ""]
        md += totals_table(head, base)
        if base:
            md += [""] + changed_table(head, base)
    if chips:
        spill = load(os.path.join(out, f"head-spill-{chips[0]}.txt"))
        failed = sum(1 for v in spill.values() if v[1] is None)
        md += ["", f"`NAK_DEBUG=spill` ({CHIPS.get(chips[0], chips[0])}): "
               f"{len(spill) - failed} of {len(spill)} shaders compile."]

    failures = read_lines(os.path.join(out, "failures.txt"))
    if failures:
        md += ["", "### Failures", "", "```"] + failures[:400] + ["```"]
    print("\n".join(md))


if __name__ == "__main__":
    main()

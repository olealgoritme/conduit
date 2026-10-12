#!/usr/bin/env bash
# How busy the host GPU was while a command ran (a Steel Nomad run, a game
# session): nvidia-smi dmon every second for the length of the command, then
# a summary of the seconds the workload was on the GPU.
#
#   gpu-busy-watch.sh LABEL [-o DIR] [-w WATTS] -- COMMAND [ARGS...]
#   e.g. gpu-busy-watch.sh sn-base -- /steam/conduit-scratch/perf-event-path-logs/snrun.sh sn-base
#
# The workload seconds are those drawing more than WATTS (default 150) or
# with SM busy >= 50 %: loading screens and menus drop out. For them it
# prints SM busy (mean, p10/p50/p90, seconds under 90 %), power, SM and
# memory clocks, and power/thermal violation time. SM busy is the share of
# the second some SM had work; well under 99 % on a GPU-bound benchmark
# means the GPU waits for work (submission, synchronisation, CPU), not that
# it is slow.
#
# Writes DIR/LABEL.dmon (raw) and DIR/LABEL.busy (summary); DIR defaults to
# ./gpu-busy.
set -euo pipefail

label=${1:?label}
shift
out=./gpu-busy
watts=150
while [ $# -gt 0 ]; do
  case $1 in
  -o) out=$2; shift 2 ;;
  -w) watts=$2; shift 2 ;;
  --) shift; break ;;
  *) echo "usage: $0 LABEL [-o DIR] [-w WATTS] -- COMMAND..." >&2; exit 2 ;;
  esac
done
[ $# -gt 0 ] || { echo "no command" >&2; exit 2; }
mkdir -p "$out"

timeout 7200 nvidia-smi dmon -s pucvt -d 1 -o DT > "$out/$label.dmon" 2>&1 &
dmon=$!
set +e
"$@"
rc=$?
set -e
kill "$dmon" 2>/dev/null || true
wait "$dmon" 2>/dev/null || true

awk -v W="$watts" -v label="$label" -v rc="$rc" '
  /^#/ && /gpu/ { for (i = 1; i <= NF; i++) col[$i] = i; next }
  /^#/ { next }
  !col["sm"] { next }
  {
    sm = $(col["sm"]) + 0; pw = $(col["pwr"]) + 0
    total++
    if (pw <= W && sm < 50) next
    n++; s[n] = sm; sum += sm; psum += pw
    pc += $(col["pclk"]); mc += $(col["mclk"])
    if (col["pviol"]) pv += $(col["pviol"])
    if (col["tviol"]) tv += $(col["tviol"])
    if (sm < 90) low++
  }
  END {
    printf "%s: command exit %d, %d s recorded, %d workload s (> %d W or SM >= 50%%)\n", label, rc, total, n, W
    if (!n) exit
    # insertion sort for the percentiles (a few hundred samples)
    for (i = 2; i <= n; i++) { v = s[i]; j = i - 1; while (j > 0 && s[j] > v) { s[j + 1] = s[j]; j-- } s[j + 1] = v }
    p10 = s[int(0.1 * (n - 1)) + 1]; p50 = s[int(0.5 * (n - 1)) + 1]; p90 = s[int(0.9 * (n - 1)) + 1]
    printf "  SM busy mean %.1f%%  p10 %d  p50 %d  p90 %d  seconds under 90%%: %d of %d (%.0f%%)\n", sum / n, p10, p50, p90, low, n, 100 * low / n
    printf "  power %.0f W  SM clock %.0f MHz  mem clock %.0f MHz  power violation %.1f%%  thermal %.1f\n", psum / n, pc / n, mc / n, pv / n, tv / n
  }' "$out/$label.dmon" | tee "$out/$label.busy"
exit "$rc"

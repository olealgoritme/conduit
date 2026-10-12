#!/usr/bin/env bash
# One GPU-bound microbenchmark row in the Windows guest with the host GPU
# watched: nvidia-smi dmon (power, SM busy, clocks, power/thermal violation
# time, PCIe) runs on the host for the length of the row, and its summary
# follows the tool's output.
#
#   WIN_SSH=user@127.0.0.1 gpu-bound-run.sh LABEL d3d12|vk [TOOL ARGS...]
#
# Environment:
#   WIN_SSH    the guest's ssh destination (required); WIN_PORT (2222)
#   GB_DIR     guest folder with d3d12_gpu_bound.exe / vk_gpu_bound.exe
#              (default C:/Users/Public/gpubench)
#   GB_ENV     "K=V K=V ..." set in the guest process (e.g.
#              "NVK_RM_TIMESLICE_US=1024" or "NVK_RM_HUGE_PAGES=1")
#   GB_VKICD   guest path of vulkan_nouveau.dll for the vk tool
#              (VK_DIRECT_DRIVER; default: the Helios package's copy found
#              under C:/Windows/System32/DriverStore)
#   GB_OUT     host folder for the logs (default ./gpu-bound-rows)
#   GB_TIMEOUT seconds before the row is abandoned (default 300)
#
# Writes GB_OUT/LABEL.txt (tool output + host summary) and LABEL.dmon.
set -euo pipefail

label=${1:?label}
tool=${2:?d3d12 or vk}
shift 2
: "${WIN_SSH:?set WIN_SSH=user@host}"
port=${WIN_PORT:-2222}
dir=${GB_DIR:-C:/Users/Public/gpubench}
out=${GB_OUT:-./gpu-bound-rows}
tmo=${GB_TIMEOUT:-300}
mkdir -p "$out"

case $tool in
d3d12) exe=d3d12_gpu_bound.exe ;;
vk) exe=vk_gpu_bound.exe ;;
*) echo "tool: d3d12 or vk" >&2; exit 2 ;;
esac

# PowerShell in the guest: environment, then the tool
ps="Set-Location '$dir'; "
for kv in ${GB_ENV:-}; do
  ps+="\$env:${kv%%=*}='${kv#*=}'; "
done
if [ "$tool" = vk ]; then
  ps+="\$env:NVK_RM='1'; "
  if [ -n "${GB_VKICD:-}" ]; then
    ps+="\$env:VK_DIRECT_DRIVER='${GB_VKICD}'; "
  else
    ps+="\$env:VK_DIRECT_DRIVER=(Get-ChildItem C:/Windows/System32/DriverStore/FileRepository -Recurse -Filter vulkan_nouveau.dll -EA 0 | Where-Object { \$_.FullName -notmatch 'SysWOW64|x86' } | Sort-Object LastWriteTime | Select-Object -Last 1).FullName; "
    ps+="\"# VK_DIRECT_DRIVER=\$env:VK_DIRECT_DRIVER\"; "
  fi
fi
ps+="& .\\$exe $*; \"# exit \$LASTEXITCODE\""

power_capping() {
  timeout 20 nvidia-smi -q -d PERFORMANCE 2>/dev/null |
    awk -F: '/SW Power Capping/ { gsub(/[^0-9]/, "", $2); print $2; exit }'
}

cap0=$(power_capping || echo 0)
timeout $((tmo + 10)) nvidia-smi dmon -s pucvt -d 1 -o T > "$out/$label.dmon" 2>&1 &
dmon=$!
t0=$(date +%s)
set +e
timeout "$tmo" ssh -o BatchMode=yes -o ConnectTimeout=10 -p "$port" "$WIN_SSH" "$ps" 2>&1 |
  tr -d '\r' | tee "$out/$label.txt"
rc=${PIPESTATUS[0]}
set -e
t1=$(date +%s)
kill "$dmon" 2>/dev/null || true
wait "$dmon" 2>/dev/null || true
cap1=$(power_capping || echo 0)

{
  echo "# host: $label, ${tool}, $((t1 - t0)) s, ssh exit $rc"
  echo "# host: SW power capping +$(( (${cap1:-0} - ${cap0:-0}) / 1000 )) ms during the row"
  # dmon: summarise the busy seconds (SM >= 50 %) by column name
  awk '
    /^#/ && /gpu/ { for (i = 1; i <= NF; i++) col[$i] = i; next }
    /^#/ { next }
    col["sm"] && $(col["sm"]) + 0 >= 50 {
      n++; sm += $(col["sm"]); pw += $(col["pwr"]); pc += $(col["pclk"]); mc += $(col["mclk"])
      if ($(col["pclk"]) + 0 > pcmax) pcmax = $(col["pclk"])
      if (pcmin == "" || $(col["pclk"]) + 0 < pcmin) pcmin = $(col["pclk"])
      if (col["pviol"]) pv += $(col["pviol"])
    }
    END {
      if (n) printf "# host: %d busy s: SM %.0f%%, power %.0f W, SM clock %.0f MHz (min %d max %d), mem clock %.0f MHz, power violation %.1f%%\n",
                    n, sm / n, pw / n, pc / n, pcmin, pcmax, mc / n, pv / n
      else print "# host: no second with SM >= 50%"
    }' "$out/$label.dmon"
} | tee -a "$out/$label.txt"
exit "$rc"

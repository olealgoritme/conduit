#!/bin/bash
# run.sh <label> [vkqtest args...]
# Waits until no benchmark row is active, then runs vkqtest under librmtrace.so
# with NVIDIA's Vulkan ICD. Writes logs/<stamp>-<label>.{rmtrace.log,out.txt}
# plus driver/GPU info once per stamp. Never overwrites.
set -u
here=$(cd "$(dirname "$0")" && pwd)
label=${1:?label}; shift
marker=${PERF_ROW_MARKER:-}
logs=$here/logs
mkdir -p "$logs"
if [ -n "$marker" ]; then
  waited=0
  while [ -e "$marker" ]; do
    if [ $waited -ge 1800 ]; then echo "marker $marker still present after 30 min; giving up"; exit 3; fi
    echo "waiting for $marker to go away ($waited s)"; sleep 30; waited=$((waited+30))
  done
fi
stamp=$(date +%Y%m%d-%H%M%S)
base=$logs/$stamp-$label
{
  echo "date: $(date -Is)"
  cat /proc/driver/nvidia/version
  nvidia-smi --query-gpu=name,driver_version,pci.bus_id,vbios_version,pstate,clocks.sm,clocks.max.sm --format=csv
  echo "args: $*"
} > "$base.info.txt" 2>&1
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json \
VK_LOADER_LAYERS_DISABLE='~implicit~' \
RMTRACE_LOG="$base.rmtrace.log" \
LD_PRELOAD="$here/librmtrace.so" \
timeout 120 "$here/vkqtest" "$@" > "$base.out.txt" 2>&1
rc=$?
echo "exit $rc" >> "$base.out.txt"
echo "$base (exit $rc)"

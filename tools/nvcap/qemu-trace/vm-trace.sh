#!/bin/bash
# nvcap: vm.sh with the nvtrace QEMU. Every BAR0 access, config access,
# interrupt, reset and GSP/FSP message of the RTX 5090 is written to a trace.
#
# The command line is generated from vm.sh at run time (vm.sh is not edited):
# only the QEMU binary is swapped and the 5090's -device vfio-pci gets the
# trace properties. The generated script is kept next to the trace.
#
# Usage (as root, like vm.sh):  vm-trace.sh [--print]
#   --print   show the generated script and the diff to vm.sh, do not run
# Env:
#   NVTRACE_FILE   trace path (default $N/traces/<date>/nvtrace.nvt.zst)
#   NVTRACE_AUDIO  1 = also trace the HDA function 01:00.1 (default 0)
#   NVTRACE_TRAP   x-trap-bars mask (default 0x1 = BAR0 only)
#   NVTRACE_BUF_MB, NVTRACE_ROTATE_MB, NVTRACE_MIN_FREE_GB, NVTRACE_LOSSY,
#   NVTRACE_GSP_POLL_US, NVTRACE_GSP_LOG_MS, NVTRACE_PAIR_BUDGET_MB:
#                  passed to QEMU (see qemu-trace/README.md)
set -eu
N=/steam/conduit-scratch/nvcap
QT=$N/qemu-trace/install/bin/qemu-system-x86_64
STAMP=$(date +%Y%m%d-%H%M%S)
TRACE=${NVTRACE_FILE:-$N/traces/$STAMP/nvtrace.nvt.zst}
OUT=$(dirname "$TRACE")
TRAP=${NVTRACE_TRAP:-0x1}

[ -x "$QT" ] || { echo "missing $QT (run qemu-trace/build.sh)" >&2; exit 1; }
case "$TRACE" in *,*|*" "*) echo "trace path must not contain , or space" >&2; exit 1;; esac

export NVTRACE_BUF_MB=${NVTRACE_BUF_MB:-128}
export NVTRACE_ROTATE_MB=${NVTRACE_ROTATE_MB:-4096}
export NVTRACE_MIN_FREE_GB=${NVTRACE_MIN_FREE_GB:-20}

GPU_OLD='-device vfio-pci,host=0000:01:00.0,'
GPU_PROPS="x-nvtrace=on,x-nvtrace-file=$TRACE,x-trap-bars=$TRAP"
AUD_OLD='-device vfio-pci,host=0000:01:00.1,'

gen=$(sed \
	-e "s#/opt/conduit/bin/qemu-system-x86_64#$QT#" \
	-e "s#\($GPU_OLD[^ ]*\)#\1,$GPU_PROPS#" \
	"$N/vm.sh")
if [ "${NVTRACE_AUDIO:-0}" = 1 ]; then
	# One trace file per process: the audio function goes into the same file.
	gen=$(printf '%s\n' "$gen" | sed -e "s#\($AUD_OLD[^ ]*\)#\1,x-nvtrace=on,x-trap-bars=0x1#")
fi

# Every substitution must have happened exactly once.
[ "$(printf '%s\n' "$gen" | grep -c "$QT")" = 1 ] \
	|| { echo "vm.sh changed: QEMU binary not found once" >&2; exit 1; }
[ "$(printf '%s\n' "$gen" | grep -c "x-nvtrace-file=")" = 1 ] \
	|| { echo "vm.sh changed: 5090 -device line not found once" >&2; exit 1; }

if [ "${1:-}" = --print ]; then
	printf '%s\n' "$gen"
	echo "---- diff vm.sh -> generated ----" >&2
	diff <(cat "$N/vm.sh") <(printf '%s\n' "$gen") >&2 || true
	exit 0
fi

mkdir -p "$OUT"
printf '%s\n' "$gen" > "$OUT/vm-trace.generated.sh"
{
	echo "started $(date -Is) by vm-trace.sh"
	echo "trace $TRACE"
	"$QT" --version | head -1
	env | grep '^NVTRACE_' | sort
} > "$OUT/info.txt"
exec bash "$OUT/vm-trace.generated.sh"

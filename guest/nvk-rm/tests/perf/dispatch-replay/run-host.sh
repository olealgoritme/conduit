#!/bin/bash
# Replay every capture in a directory on the first NVIDIA Vulkan device of this machine: results are
# appended to host.tsv, outputs go to out-host/<capture>/.
#   run-host.sh [CAPTURES_DIR] [extra dispatch-replay options]
set -u
cd "$(dirname "$0")"
caps=${1:-captures}; shift || true
for c in "$caps"/*/; do
    c=${c%/}; [ -f "$c/manifest.json" ] || continue
    n=$(basename "$c")
    timeout 300 ./dispatch-replay -n 15 --csv host.tsv --label host-nvidia --dump "out-host/$n" "$@" "$c" || echo "$n: failed ($?)" >&2
done

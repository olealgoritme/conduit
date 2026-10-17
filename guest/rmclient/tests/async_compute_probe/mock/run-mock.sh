#!/bin/sh
# Builds the probe against mock_rm.c (a software RM + GPU, see mock_rm.c) and runs it under
# the three sharing models.  Needs only cc.  No GPU, no driver, no VM.
#   run-mock.sh [REPO_DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
REPO=${1:-$(cd "$here/../../../../.." && pwd)}
RM="$REPO/guest/rmclient"
out=${TMPDIR:-/tmp}/async-probe-mock
mkdir -p "$out"
cc -std=gnu11 -O1 -g -Wall -Wno-unused-function -pthread -I"$here/.." -I"$RM/include" -I"$RM/src" \
   "$here/../async_compute_probe.c" "$here/mock_rm.c" -o "$out/probe_mock"
for model in async serial concurrent; do
  echo "################ MOCK_MODEL=$model"
  MOCK_MODEL=$model timeout 120 "$out/probe_mock" --reps 6 --k 4 --wgs 16 --timeout-ms 4000 "$@" 2>&1 | tail -60 || true
done

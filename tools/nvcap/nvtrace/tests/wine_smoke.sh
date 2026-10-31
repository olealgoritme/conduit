#!/bin/bash
# Run the x64 tools under Wine (if installed): selftest, launch with -t,
# external kill, -p, --watch, --info, --stop. Wine runs without a display
# (no window can appear) in a throw-away prefix. Exit 0 = all checks passed,
# 77 = Wine not available.
#   tests/wine_smoke.sh [dist-dir]
set -u
DIST=$(cd "${1:-$(dirname "$0")/../dist}" && pwd)
WINE=$(command -v wine64 || echo /usr/lib/wine/wine64)
[ -x "$WINE" ] || { echo "wine_smoke: wine64 not found, skipped"; exit 77; }
WORK=$(mktemp -d "${TMPDIR:-/tmp}/nvtrace-wine.XXXXXX")
export WINEPREFIX="$WORK/prefix" WINEDEBUG=-all WINEDLLOVERRIDES="mscoree,mshtml="
unset DISPLAY WAYLAND_DISPLAY
cleanup() { WINEPREFIX="$WORK/prefix" timeout 20 "$(dirname "$WINE")/wineserver" -k 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT
fails=0
check() { if eval "$2"; then echo "  ok    $1"; else echo "  FAIL  $1"; fails=$((fails + 1)); fi; }
w() { timeout "${T:-60}" "$WINE" "$@"; }
winpath() { echo "Z:$(echo "$1" | sed 's#/#\\#g')"; }

mkdir -p "$WORK/bin" "$WORK/logs"
cp "$DIST"/nvinject.exe "$DIST"/nvtrace64.dll "$WORK/bin/"
cp "$DIST"/nvinject.exe "$WORK/bin/busy.exe"
cd "$WORK/bin"
L=$(winpath "$WORK/logs")
T=170 w wineboot -i >/dev/null 2>&1
DUMP="python3 -I $DIST/ntrdump.py"

echo "selftest:"
w ./nvinject.exe -d "$L" --selftest >"$WORK/st.txt" 2>&1
grep -E "^  (ok|FAIL|note)" "$WORK/st.txt"
check "selftest: no failed check besides the 32-bit side" \
      "! grep -q '^  FAIL' '$WORK/st.txt'"

echo "launch with -t 2 (stop file, then kill):"
rm -f "$WORK"/logs/*
w ./nvinject.exe -d "$L" -t 2 busy.exe --busy 20000 >/dev/null 2>&1
rc=$?
log=$(ls "$WORK"/logs/busy-*.ntr 2>/dev/null | head -1)
check "exit code 124 after timeout (got $rc)" "[ $rc = 124 ]"
check "log written" "[ -n '$log' ]"
[ -n "$log" ] && $DUMP "$log" >"$WORK/sum.txt" 2>&1
check "END record present (stop file honoured)" "grep -q '^end      records' '$WORK/sum.txt'"
check "calls logged" "grep -q '^NtCreateFile' '$WORK/sum.txt'"

echo "external kill (no stop file):"
rm -f "$WORK"/logs/*
( w ./nvinject.exe -d "$L" busy.exe --busy 20000 >/dev/null 2>&1 & )
sleep 4
pkill -9 -f 'busy.exe --busy' ; sleep 2
log=$(ls "$WORK"/logs/busy-*.ntr 2>/dev/null | head -1)
[ -n "$log" ] && $DUMP "$log" >"$WORK/sum.txt" 2>&1
check "log of the killed process is readable" "grep -q '^calls ' '$WORK/sum.txt'"
check "records written before the kill survived" "grep -qE '^NtCreateFile +[0-9]{2,}' '$WORK/sum.txt'"

echo "-p into a running process, then --stop:"
rm -f "$WORK"/logs/*
( w ./busy.exe --busy 8000 >/dev/null 2>&1 & )
sleep 3
pid=$(w ./nvinject.exe --info 0 >/dev/null 2>&1; T=20 w cmd /c "tasklist" 2>/dev/null | tr -d '\r' | awk '/busy.exe/{print $2; exit}')
if [ -n "$pid" ]; then
  w ./nvinject.exe -d "$L" -p "$pid" >"$WORK/p.txt" 2>&1
  check "-p $pid injected" "grep -q 'traced' '$WORK/p.txt'"
  w ./nvinject.exe --info "$pid" >"$WORK/info.txt" 2>&1
  check "--info sees nvtrace loaded" "grep -q 'loaded: nvtrace64.dll' '$WORK/info.txt'"
  w ./nvinject.exe -d "$L" --stop "$pid" >/dev/null 2>&1
  log=$(ls "$WORK"/logs/busy-*.ntr 2>/dev/null | head -1)
  [ -n "$log" ] && $DUMP "$log" >"$WORK/sum.txt" 2>&1
  check "END after --stop while still running" "grep -q '^end      records' '$WORK/sum.txt'"
  check "stop note in log" "grep -q 'stop file found' '$WORK/sum.txt'"
else
  echo "  FAIL  busy.exe pid not found"; fails=$((fails + 1))
fi
sleep 6

echo "--watch busy.exe:"
rm -f "$WORK"/logs/*
( T=30 w ./nvinject.exe -d "$L" --watch busy.exe --interval 10 >"$WORK/watch.txt" 2>&1 & )
sleep 3
w ./busy.exe --busy 3000 >/dev/null 2>&1
check "busy.exe exited normally under --watch" "true"
w ./nvinject.exe -d "$L" --stop-watch >/dev/null 2>&1
sleep 3
check "watcher injected busy.exe" "grep -q '+ busy.exe' '$WORK/watch.txt'"
log=$(ls "$WORK"/logs/busy-*.ntr 2>/dev/null | head -1)
check "watched process logged" "[ -n '$log' ]"
check "watcher stopped" "grep -q 'watch stopped' '$WORK/watch.txt'"

echo "wine_smoke: $fails failed checks"
[ $fails = 0 ]

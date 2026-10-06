#!/bin/bash
# The pre-push gate for the Windows KMD. Run it after EVERY merge, not just before a push.
# kmd_render cannot be compiled without the WDK, so this is host tests (kmd_logic with the
# counter-name scans, protocol), a parse of every source, the inline-only extern scan and the
# C/Rust ABI mirror check. Read the verdict line: the sub-scripts end in pipes, so their exit codes
# are not reliable.
. "$(dirname "$0")/env.sh"
cd "$WT" || exit 1
grep -n HELIOS_KMD_VERSION= "$KW/kmd_render/driver-version.env"
rc=0
for s in test_kmd_logic test_protocol check_kmd check_nvrm; do
  echo "== $s"
  bash "$HERE/$s.sh" > "$S/prepush_$s.log" 2>&1
  grep -E "^test result|^error|FAILED|mismatches|panicked" "$S/prepush_$s.log" | head -8
done
for s in test_kmd_logic test_protocol; do
  if ! grep -qE "^test result: ok\. [1-9][0-9]* passed; 0 failed" "$S/prepush_$s.log"; then echo "FAIL: $s (no passing unit-test line)"; rc=1; fi
  if grep -qE "^error|FAILED|panicked" "$S/prepush_$s.log"; then echo "FAIL: $s (error in log)"; rc=1; fi
done
if ! grep -q "mismatches: 0" "$S/prepush_check_nvrm.log"; then echo "FAIL: check_nvrm"; rc=1; fi
if ! grep -q "parse_errors=0" "$S/prepush_check_kmd.log"; then echo "FAIL: check_kmd"; rc=1; fi
if ! grep -q "inline_only_externs=0" "$S/prepush_check_kmd.log"; then echo "FAIL: inline-only extern declared"; rc=1; fi
echo "OVERALL rc=$rc"

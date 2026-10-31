#!/usr/bin/env bash
# Test-sign a driver with the nvddi test certificate (embedded Authenticode, SHA-256).
#   sign/sign.sh build/nvddi.sys build/nvddi.signed.sys
# Uses osslsigncode from PATH or $OSSLSIGNCODE.  No timestamp (test signing).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
in="$1"
out="$2"
osc="${OSSLSIGNCODE:-osslsigncode}"
[ -f "$here/out/nvddi-test.key" ] || "$here/mkcert.sh"
rm -f "$out"
"$osc" sign -certs "$here/out/nvddi-test.crt" -key "$here/out/nvddi-test.key" -h sha256 \
    -n "nvddi WDDM DDI tracer" -in "$in" -out "$out"
"$osc" verify -CAfile "$here/out/nvddi-test.crt" -in "$out" | grep -E "Signature verification|Message digest|Calculated|Number of verified"
echo "sign: $out"

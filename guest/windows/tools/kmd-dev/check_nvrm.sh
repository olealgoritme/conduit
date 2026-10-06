#!/bin/bash
# The C mirror of the NVRM escape ABI (guest/rmclient/src/helios_nvrm_escape.h) against the Rust
# protocol (protocol/src/nvrm.rs): compiles as C 64/32-bit and C++, builds the Rust layout asserts
# standalone, and cross-checks every shared constant. Prints "mismatches: 0" when they agree.
. "$(dirname "$0")/env.sh"
W="$WT"
printf '#include "helios_nvrm_escape.h"\nint main(void){return 0;}\n' > "$S/t.c"
printf '#include "helios_nvrm_escape.h"\nint main(){return 0;}\n' > "$S/t.cc"
echo "== C 64-bit";  gcc -std=c11 -Wall -Wextra -I"$W/guest/rmclient/src" -o "$S/t64" "$S/t.c" && echo ok64
echo "== C 32-bit";  gcc -m32 -std=c11 -Wall -Wextra -I"$W/guest/rmclient/src" -o "$S/t32" "$S/t.c" && echo ok32
echo "== C++";       g++ -std=c++17 -I"$W/guest/rmclient/src" -fsyntax-only "$S/t.cc" && echo okcxx
echo "== Rust layout asserts (derives stripped; bytemuck is not in the offline cache)"
{
  echo '#![allow(dead_code)]'
  echo '#[repr(C)] #[derive(Debug, Clone, Copy)] pub struct HeliosEscapeHeader { pub magic: u32, pub cmd_type: u32, pub version: u32, pub size: u32 }'
  sed -e '/^use bytemuck/d' -e '/^use crate::HeliosEscapeHeader/d' -e 's/, Pod, Zeroable//' -e 's@^//!@//@' "$W/guest/windows/protocol/src/nvrm.rs"
} > "$S/nvrm_standalone.rs"
rustc --edition 2021 --crate-type lib -o "$S/libnvrm_standalone.rlib" "$S/nvrm_standalone.rs" 2>&1 | tail -20 && echo rust-done
WT_ROOT="$W" python3 - <<'PY'
import os, re
W = os.environ['WT_ROOT'] + '/'
r = open(W + 'guest/windows/protocol/src/nvrm.rs').read()
h = open(W + 'guest/rmclient/src/helios_nvrm_escape.h').read()
rc = dict((m.group(1), m.group(2)) for m in re.finditer(r'pub const (HELIOS_[A-Z0-9_]+): (?:u32|i32|usize) = ([^;]+);', r))
hc = dict((m.group(1), m.group(2).strip()) for m in re.finditer(r'#define (HELIOS_[A-Z0-9_]+)\s+([0-9A-Fa-fx_ ]+?)u?\b\s*(?:/\*.*)?$', h, re.M))
bad = 0
def norm(v):
    v = v.replace('_', '').strip().rstrip('u')
    try:
        return int(v, 0)
    except ValueError:
        return None
for k, v in rc.items():
    if k in hc and norm(v) is not None and norm(hc[k]) is not None and norm(v) != norm(hc[k]):
        print('MISMATCH', k, v, hc[k]); bad += 1
only = [k for k in rc if k.startswith('HELIOS_NVRM_') and k not in hc]
print('constants compared:', sum(1 for k in rc if k in hc), 'mismatches:', bad)
print('in Rust only (check intentional):', only)
PY

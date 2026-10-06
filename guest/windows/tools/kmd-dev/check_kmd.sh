#!/bin/bash
# rustfmt parse of EVERY .rs file under kmd_render/src, kmd_logic/src, protocol/src (only '^error'
# lines matter), and a scan for extern declarations of WDK functions that are inline-only in wdm.h
# (not exported by ntoskrnl.lib: LNK2019 at link time, found once the hard way with
# KeGetCurrentThread in 334.1).
. "$(dirname "$0")/env.sh"
echo "== rustfmt parse of all kmd sources"
bad=0; n=0
for f in $(find "$KW/kmd_render/src" "$KW/kmd_logic/src" "$KW/protocol/src" -name '*.rs' | sort); do
  n=$((n+1))
  out=$(rustfmt --edition 2021 --check --config skip_children=true "$f" 2>&1 | grep -E '^error' | head -2)
  if [ -n "$out" ]; then echo "PARSE ERROR $f: $out"; bad=$((bad+1)); fi
done
echo "files=$n parse_errors=$bad"
bad2=0
for sym in KeGetCurrentThread KeGetCurrentProcessorNumber KeMemoryBarrier KeMemoryBarrierWithoutFence KeGetCurrentProcessorIndex InterlockedIncrement InterlockedDecrement InterlockedExchange InterlockedCompareExchange InterlockedExchangeAdd InterlockedOr InterlockedAnd ReadTimeStampCounter KeQueryTickCountInline; do
  if grep -rnE "^\s*(pub )?fn ${sym}\(" "$KW/kmd_render/src" >/dev/null 2>&1; then
    echo "INLINE-ONLY EXTERN DECLARED: $sym"; bad2=$((bad2+1)); grep -rnE "^\s*(pub )?fn ${sym}\(" "$KW/kmd_render/src" | head -3
  fi
done
echo "inline_only_externs=$bad2"

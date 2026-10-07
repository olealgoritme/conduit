#!/bin/bash
# WDK-less type check of kmd_render against stub wdk-sys crates (stubcheck/). It reports only
# type and name errors; it does not link and it knows nothing about WDK semantics. Expect a
# baseline of errors from the stubs themselves, so ALWAYS diff against a base commit:
#   tools/kmd-dev/stubcheck.sh base <commit>   # errors of that commit
#   tools/kmd-dev/stubcheck.sh head            # errors of the worktree
#   python3 tools/kmd-dev/stubdiff.py          # errors only the head has
# Plant a deliberate error in a changed file once to prove the check reaches it (it did for every
# agent-written change; the stubs hide errors in any module they do not model).
# Needs the 1.90.0 toolchain and an offline cargo cache with bytemuck and virtio-drivers 0.13.
. "$(dirname "$0")/env.sh"
which="${1:-head}"
K="$S/kc_$which"
rm -rf "$K"
mkdir -p "$K/kmd_render"
cp "$HERE/stubcheck/Cargo.toml" "$HERE/stubcheck/Cargo.lock" "$K/"
cp -r "$HERE/stubcheck/stubs" "$K/stubs"
cp "$HERE/stubcheck/kmd_render/Cargo.toml" "$K/kmd_render/"
if [ "$which" = "base" ]; then
  commit="${2:?usage: stubcheck.sh base <commit>}"
  rm -rf "$S/base_src"; mkdir -p "$S/base_src"
  (cd "$WT" && git archive "$commit" guest/windows/kmd_render/src guest/windows/kmd_logic guest/windows/protocol) | tar -x -C "$S/base_src"
  SRC="$S/base_src/guest/windows"
else
  SRC="$KW"
fi
cp -r "$SRC/kmd_render/src" "$K/kmd_render/src"
# `virtio/msi.rs` reads the build tag from it with `include_str!`.
cp "$KW/kmd_render/driver-version.env" "$K/kmd_render/driver-version.env"
cp -r "$SRC/kmd_logic" "$K/kmd_logic"
cp -r "$SRC/protocol" "$K/protocol"
rm -rf "$K/kmd_logic/target" "$K/protocol/target"
cd "$K" || exit 1
cargo +1.90.0 check --offline -p helios_kmd_render --message-format short 2> "$S/stub_$which.txt"
grep -c 'error' "$S/stub_$which.txt"
rm -rf "$K"

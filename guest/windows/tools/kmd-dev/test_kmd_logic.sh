#!/bin/bash
# kmd_logic host tests with kmd_render/src copied beside it, so the counter-name scans really run
# (they skip silently when ../kmd_render/src is absent); HELIOS_REQUIRE_NAME_SCAN=1 makes them fail
# instead of skipping.
. "$(dirname "$0")/env.sh"
rm -rf "$S/kl_tree"
mkdir -p "$S/kl_tree/kmd_render"
cp -r "$KW/kmd_logic" "$S/kl_tree/kmd_logic"
cp -r "$KW/kmd_render/src" "$S/kl_tree/kmd_render/src"
cp "$KW/kmd_render/driver-version.env" "$S/kl_tree/kmd_render/driver-version.env"
cd "$S/kl_tree/kmd_logic" || exit 1
HELIOS_REQUIRE_NAME_SCAN=1 cargo test --offline 2>&1 | tail -25
rm -rf "$S/kl_tree/kmd_logic/target"

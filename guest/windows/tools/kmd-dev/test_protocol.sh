#!/bin/bash
# protocol crate host tests in a scratch copy (never leave target directories in the repo).
. "$(dirname "$0")/env.sh"
rm -rf "$S/pcheck" && mkdir -p "$S/pcheck" && cp -r "$KW/protocol" "$S/pcheck/protocol" && cd "$S/pcheck/protocol" || exit 1
grep -q '^\[workspace\]' Cargo.toml || printf '\n[workspace]\n' >> Cargo.toml
cargo test --offline 2>&1 | grep -E "test result|error" | head -4
rm -rf "$S/pcheck/protocol/target"

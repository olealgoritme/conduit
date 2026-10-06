# kmd-dev: developing the Windows KMD without a WDK

`kmd_render` links against the Windows Driver Kit and is built only in the Windows VM (`guest/windows/ci/vm/win-build.sh`). On the
Linux development host these scripts give the checks that do not need it. They are portable (they derive the worktree root from their own
location, or `WT`; scratch space is `S`, default `$TMPDIR/kmd-dev-scratch`; nothing is left in the repo).

| script | what it does |
|---|---|
| `prepush.sh` | the gate: runs the four below and prints `OVERALL rc=0` only if all pass. Run it after EVERY merge, not just before a push |
| `test_kmd_logic.sh` | `kmd_logic` host tests in a scratch copy WITH `kmd_render/src` beside it and `HELIOS_REQUIRE_NAME_SCAN=1`, so the counter-name scans really run (they used to skip silently) |
| `test_protocol.sh` | `protocol` crate tests in a scratch copy with an empty `[workspace]` |
| `check_kmd.sh` | rustfmt parse of every source file, and a scan for extern declarations of WDK functions that are inline-only (they fail at link time) |
| `check_nvrm.sh` | the C mirror `guest/rmclient/src/helios_nvrm_escape.h` against `protocol/src/nvrm.rs` (C 64/32-bit and C++ compile, Rust layout asserts, every shared constant) |
| `stubcheck.sh` | `base <commit>` / `head`: a type check of `kmd_render` against stub `wdk-sys` crates (`stubcheck/`); `stubdiff.py` lists the errors only the head has. It cannot link and models only part of the WDK, so a clean diff means "no new type or name error", nothing more; plant a deliberate error in a changed file once to prove the check reaches it. Needs the 1.90.0 toolchain and an offline cargo cache |

Read the verdict lines, not exit codes: the sub-scripts end in pipes. Expected today (v343): `kmd_logic` 1420 passed, `protocol` 43 passed,
`parse_errors=0`, `inline_only_externs=0`, `mismatches: 0` (51 constants). One `kmd_logic` run in a few shows a failure in
`vsync_snap::a_reader_never_sees_a_torn_sample`, a timing test; rerun it.

Hardware testing is not here: the Windows VM scripts are in `guest/windows/ci/vmtest`, and the knob and counter meanings are in
`guest/windows/docs/kmd-handoff-2026-10.md` and `zero-copy-present.md`.

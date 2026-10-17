# nak-offline: NAK statistics without a GPU

Compiles SPIR-V shaders through NVK's real pipeline creation path
(`spirv_to_nir` with NVK's options, NVK's lowering, NAK for the chosen SM)
on a faked GPU and prints NAK's statistics per shader: instructions, static
cycles, GPRs, warps/SM, spills and fills, code size, shared memory. These are
the `VK_KHR_pipeline_executable_properties` numbers, the same ones
`NVK_SHADER_STATS` prints. It is the fast loop for compiler work: a corpus
of ~300 shaders takes a few seconds.

How: NVK is built for Linux from a Mesa checkout with the NVK-on-RM series
applied (the Windows stack builds for Linux too). Mesa's nouveau drm-shim
fakes the kernel device (`NOUVEAU_CHIPSET=1b2` is GB202, SM 120), and
`nvstats` creates one pipeline per shader with
`VK_PIPELINE_CREATE_CAPTURE_STATISTICS_BIT_KHR`. `run.sh` sets
`NVK_UBO_DESC_CBUF=0`, the Windows default, so the numbers match the
Windows driver's. On a Steel Nomad corpus (115 compute, 136 fragment and 41
vertex shaders), all 115 compute results match the guest's `NVK_SHADER_STATS`
exactly. Of the fragment and vertex results, 134/136 and 35/41 match exactly;
the others differ by a few instructions with the same GPR count.

`nvstats` is also built for the host's own Vulkan driver (or as
`nvstats.exe` for Windows) to collect a reference driver's register counts
and binary sizes for the same corpus. Only the statistics a driver reports
are used.

## Files

| File | What |
|---|---|
| `build.sh MESA_DIR [BUILD_DIR]` | Configures and builds NVK, the drm-shim, NAK's unit test binary and `nvstats` (meson >= 1.4 via `MESON`, `JOBS` for ninja) |
| `run.sh BUILD CORPUS OUT_DIR` | Compiles the corpus; writes `compute.txt`, `frag.txt`, `vert.txt`. NAK knobs in the environment apply |
| `compare.py [--ref REF] NAME=DIR ...` | Per-shader table and totals of several runs, plus the reference driver's GPRs, warps and code size |
| `nvstats.c` | The pipeline/statistics tool: `nvstats a.spv ...` (compute), `-g fs-pairs.txt` (fragment), `-v vs-pairs.txt` (vertex) |
| `genvs.py`, `genfs.py` | Make the matching vertex shaders for fragment shaders and fragment shaders for vertex shaders (`spirv-dis`, `glslangValidator`), printing the pair lists |

## Corpus layout

A corpus directory holds the SPIR-V files and three lists, with paths
relative to it: `all.txt` (every file; `nvstats` skips non-compute ones in
compute mode), `fs-pairs.txt` (`genvs.py shaderdump/*.spv`, run in the
corpus directory; it writes `vs/`) and `vs-pairs.txt` (`genfs.py` on the
vertex shaders; it writes `fsv/`). The reference driver's output goes to
`nv-compute.txt`, `nv-frag.txt` and `nv-vert.txt` in the `--ref` directory.
Game shaders are not part of the repository.

## Example

```sh
tools=guest/nvk-rm/tools/nak-offline
$tools/build.sh ~/mesa-nvk-rm build-nak-offline
B=~/mesa-nvk-rm/build-nak-offline
$tools/run.sh $B corpus out/base
NAK_OCC_TARGET=1 $tools/run.sh $B corpus out/occ
$tools/compare.py --ref corpus --top 20 base=out/base occ=out/occ
# NAK's unit tests (hardware tests need a GPU and are skipped)
$B/src/nouveau/compiler/nak --skip hw_tests:: --skip nvdisasm_tests::
```

## Debug-assertion build

For checking compiler invariants, configure a second build directory with
Rust debug assertions but without C assertions (the drm-shim's fake device
trips a winsys `assert` on GB202 that release builds compile out):

```sh
meson setup build-dbg -Dbuildtype=debugoptimized -Db_ndebug=false \
  -Dc_args=-DNDEBUG -Dcpp_args=-DNDEBUG \
  -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dplatforms= \
  -Dvideo-codecs= -Dtools=drm-shim -Dbuild-tests=true
```

then `build.sh MESA_DIR build-dbg` builds into it. A panic aborts the
`nvstats` process, so run shaders one per process to see every failure.
`NAK_DEBUG=spill` makes NAK spill nearly everything, which exercises the
spiller far more than real register pressure does.

## NAK_OCC_TARGET (patches-common/0056): state

Work in progress, off by default.

- **Rematerialization** in the GPR spiller (`spill_remat.rs`). A value whose
  definition is cheap and free of side effects is evicted without a store
  and recomputed at each reload. Its inputs must be available wherever it is
  live: immediates, bound cbufs, scalar UGPRs whose live range covers it, or
  short chains of such values. Covered definitions are copies, integer and
  float ALU ops, conversions, thread/CTA IDs, cbuf loads, and global loads
  from constant memory (one 32-bit component of a vector load). Inputs that
  are still resident are used as they are.
- **Single-LDG constant reloads.** The main case in Steel Nomad is the
  root-CBV constants that vkd3d-proton loads through a 64-bit BDA address
  held in a bound constant buffer. That address is loaded once into a
  pinned UGPR pair at the start of the (uniform) entry block, and every
  reload of such a constant is one `LDG [rZ + UR]` from it. This needs room
  in the UGPR file across the whole shader (2 UGPRs per address, up to 4
  addresses); without room, or without UGPRs, the address is recomputed
  (two moves and the LDG).
- **Cost-aware eviction:** next-use distance per restore cost, and values
  that the current loop does not use go first. Rematerializable values are
  not refilled eagerly on merge edges.
- **Occupancy target** (`occ_target.rs`). After scheduling, NAK computes the
  occupancy (workgroup and shared memory limits included) and takes up to 3
  lower cliffs. For each one, a dry run of the spiller estimates the
  loop-weighted cost and the memory spills/fills it needs. A cliff that
  needs more than 4 memory spills/fills, or any inside a loop, is never
  taken: reductions are meant to be paid for by rematerialization, not by
  local memory traffic. Of the rest, NAK picks the one with the best
  `sqrt(min(warps', 32) / min(warps, 32)) / (1 + cost / work)` if that is at
  least 1.05, schedules again for it and spills to it. The dry run sees the
  schedule made for the natural register count; the one made for the target
  needs less spill code, so the estimate errs high (88825d04: estimated
  +171 instructions, actual +124). `NAK_OCC_TRACE=1` prints every
  candidate's estimate to stderr. `NAK_GPR_CAP` overrides the target per
  stage.
- With the knob unset, NAK's output is identical to before (checked on the
  whole corpus).
- Correctness, offline: the whole corpus (469 pipelines) compiles without
  an assertion failure on a debug-assertion build with `NAK_OCC_TARGET`
  0 and 1, each with and without `NAK_DEBUG=spill`. NAK's unit tests cover
  rematerialization (`spill_remat::tests`: a constant-load component
  reloaded through a UGPR address or a recomputed one, a chain with a
  resident input, UGPR coverage); the 9 host-runnable tests pass.

Offline, on the Steel Nomad corpus (NAK_OCC_TARGET=1 vs off; ref = NVIDIA):

| | off | on | ref |
|---|---|---|---|
| compute: instructions / code bytes | 69910 / 1118560 | 70416 / 1126656 | — / 1042560 |
| compute: memory spills / fills | 0 / 0 | 0 / 0 | |
| compute: average warps/SM | 39.9 | 40.3 | 42.2 |
| fragment: instructions / code bytes | 140940 / 2255040 | 143389 / 2294224 | — / 2029056 |
| fragment: memory spills / fills | 0 / 0 | 0 / 0 | |
| fragment: average warps/SM, shaders > 96 GPRs | 43.4, 16 | 44.0, 2 | 42.9, 15 |
| 88825d04 (compute) GPRs / warps / instructions | 216 / 8 / 2347 | 168 / 12 / 2471 | 128 / 16 |
| a87cb567 (compute) | 208 / 8 / 2109 | 168 / 12 / 2235 | 128 / 16 |
| 2a040240 (compute) | 192 / 8 / 20091 | 192 / 8 / 20091 (every lower count needs memory spills) | 128 / 16 |
| a26771ce (fragment) | 144 / 12 / 13833 | 128 / 16 / 13913 | 128 / 16 |
| 59e22586 (fragment) | 136 / 12 / 7648 | 96 / 20 / 7781 | 128 / 16 |
| c66d3d62 (fragment) | 136 / 12 / 2837 | 96 / 20 / 2949 | 128 / 16 |

Forcing lower counts with `NAK_GPR_CAP_CS=128` and the knob on:
88825d04 gets 3024 instructions and 162/105 memory spills/fills, a87cb567
2607 and 134/87, 2a040240 20587 and 264/201.

Not done yet:

- No GPU validation: correctness and fps are unmeasured. Driver package
  22.22.405.100 carries it with the knob off by default, for A/Bs with
  `NAK_OCC_TARGET=1`.
- The gain model is conservative but not calibrated against fps.
- 88825d04 and a87cb567 stop at 168: 128 needs memory spills there, mostly
  for values that are not rematerializable and are live across the
  shader's outer loop.

## Next steps

1. fps A/B in Steel Nomad: `NAK_OCC_TARGET=1` vs unset, and
   `NAK_GPR_CAP_CS`/`NAK_GPR_CAP_FS` points, to calibrate the gain model
   (saturation, least gain, how many memory spills are tolerable).
2. Spill to shared memory instead of local memory for compute shaders whose
   spare shared memory does not lower occupancy; that would let 88825d04
   and a87cb567 reach 128.
3. Reload both halves of a vector constant at the same point with one
   64-bit load.
4. Once validated, make NAK_OCC_TARGET default to 1.

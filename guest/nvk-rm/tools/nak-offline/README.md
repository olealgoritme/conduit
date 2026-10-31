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
and binary sizes for the same corpus, and the machine code it generated for
those shaders (see "Reference SASS").

## Files

| File | What |
|---|---|
| `build.sh MESA_DIR [BUILD_DIR]` | Configures and builds NVK, the drm-shim, NAK's unit test binary and `nvstats` (meson >= 1.4 via `MESON`, `JOBS` for ninja) |
| `run.sh BUILD CORPUS OUT_DIR` | Compiles the corpus; writes `compute.txt`, `frag.txt`, `vert.txt`. NAK knobs in the environment apply |
| `compare.py [--ref REF] NAME=DIR ...` | Per-shader table and totals of several runs, plus the reference driver's GPRs, warps and code size |
| `nvstats.c` | The pipeline/statistics tool: `nvstats a.spv ...` (compute), `-g fs-pairs.txt` (fragment), `-v vs-pairs.txt` (vertex), `-s a.spv ...` (each file alone as a shader object, any stage). `NVSTATS_DUMP=DIR` also writes each pipeline's own pipeline cache data and internal representations |
| `genvs.py`, `genfs.py` | Make the matching vertex shaders for fragment shaders and fragment shaders for vertex shaders (`spirv-dis`, `glslangValidator`), printing the pair lists |
| `run-objects.sh BUILD DIR OUT` | Compiles every SPIR-V file of a directory alone (`nvstats -s`), tessellation and geometry included; one line of statistics per shader (`objmerge.py`). `BATCH=1` runs each shader in its own process, so a crash fails only that shader |
| `objcmp.py A B` | Per-stage totals of two `run-objects.sh` runs: instructions, static cycles, memory spills, shaders with more/fewer GPRs or warps |
| `refsass.py CACHE_DIR OUT` | The machine code of our pipelines from the reference driver's pipeline cache data (`NVSTATS_DUMP`), for `nvdisasm` |
| `naksass.sh BUILD CORPUS OUT HASH...` | NAK's machine code for corpus shaders (`NAK_DEBUG=print`, `enc2bin.py`), disassembled with `nvdisasm` |
| `sassdiff.py REF.sass NAK.sass ...` | Instruction counts of two disassemblies by gap pattern (and `--opcodes`) |

## CI

`ci.yml` runs the same loop on every push and pull request
(`.github/scripts/nak-build.sh`, `.github/scripts/nak-shaders.sh`): NVK with
the RM backend and the whole series, built with Rust debug assertions,
compiles a public corpus (`guest/nvk-rm/tests/nak-corpus/`, the other GLSL
shaders in `guest/nvk-rm/tests/`, and the built-in GLSL shaders of the pinned
DXVK and vkd3d-proton) on GB202 and AD102 with `run-objects.sh`, and with
`NAK_DEBUG=spill`. A shader NAK fails fails the job; NAK's statistics against
the base commit's series go to the step summary. Locally:

```sh
.github/scripts/nak-build.sh . /tmp/nak-head
.github/scripts/nak-shaders.sh /tmp/nak-head [/tmp/nak-base]
```

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
$tools/run.sh $B corpus out/occ
NAK_OCC_TARGET=0 $tools/run.sh $B corpus out/plain
$tools/compare.py --ref corpus --top 20 plain=out/plain occ=out/occ
# NAK's unit tests (hardware tests need a GPU and are skipped)
$B/src/nouveau/compiler/nak --skip hw_tests:: --skip nvdisasm_tests::
```

## Shader objects

`nvstats -s` compiles each SPIR-V file on its own through
`VK_EXT_shader_object` (no pipeline state, no partner stage), so corpora
dumped with `MESA_SPIRV_DUMP_PATH` (files `<hash>.<stage>`) need no pair
lists, and tessellation and geometry shaders compile too.  The entry point
is the module's own (`OpEntryPoint`).  Fragment shaders compile without
the pipeline's render state, so a few numbers differ from a pipeline
build; for before/after comparisons of compiler changes that does not
matter.  Statistics are NVK's own (`NVK_SHADER_STATS`).

```sh
$tools/run-objects.sh $B dump/cs2 out/cs2-off.txt     # with NAK_* knobs off
$tools/run-objects.sh $B dump/cs2 out/cs2.txt
$tools/objcmp.py out/cs2-off.txt out/cs2.txt
```

## Reference SASS

To see where NAK's code differs from the reference compiler's, compare
machine code, black box: the host's NVIDIA driver compiles our corpus
through the public Vulkan API, `NVSTATS_DUMP=DIR nvstats ...` saves the
pipeline cache data it returns for each of our pipelines (an empty
`VkPipelineCache` per pipeline), `refsass.py` takes the code section of
our shader out of it (the layout it relies on is described there), and
NVIDIA's public disassembler `nvdisasm` (CUDA 12.8 or newer for SM120, for
example from the `nvidia-cuda-nvdisasm` pip wheel) disassembles it.
Nothing of the driver or compiler binaries is read.  `naksass.sh`
disassembles NAK's code for the same shaders the same way, and
`sassdiff.py` counts the difference by pattern.

```sh
# host, NVIDIA driver
cc -O2 -o nvstats $tools/nvstats.c -lvulkan
(cd corpus && NVSTATS_DUMP=$PWD/../nvcache ../nvstats -g heavy-pairs.txt)
python3 $tools/refsass.py nvcache nvbin
nvdisasm -b SM120 nvbin/a26771cebd846ea0.1.bin > a26771ce.nv.sass
# NAK
$tools/naksass.sh $B corpus naksass a26771cebd846ea0
$tools/sassdiff.py --opcodes a26771ce.nv.sass naksass/a26771cebd846ea0.fragment.sass
```

## NAK code size (patches-common/0057-0061)

What NAK's code for the heaviest Steel Nomad shaders spends instructions
on, beyond the reference compiler's code for the same shaders (six
fragment shaders a26771ce, f5fb978e, a0f662c7, 797434f6, 59e22586,
b0825e55; `sassdiff.py`, NAK with all knobs off):

| Pattern | Share of NAK's extra instructions |
|---|---|
| Uniform values computed in GPRs inside divergent control flow: 64-bit address adds (IADD3 + IADD3.X), constant-buffer addresses reloaded with two LDCs before each LDG, MOV R, UR, descriptor index clamps | ~30% |
| Register copies at if/else merges (phi copies, swaps) | 19% |
| BSSY/BSYNC/BRA/NOP around small blocks (the reference predicates ~3400 instructions there, NAK none) | 15% |
| BMOV.32.CLEAR before every BSSY (the reference's SM120 code uses `.RECONVERGENT` without it) | 7% |
| PLOP3 next to comparisons instead of the comparison's accumulator | 3% |
| Immediates copied to a register once per use | 2% (2a040240, compute: 1461 copies of 11 constants) |

The patches, each behind a knob (`docs/WINDOWS-KNOBS.md`):

- **0057 `NAK_IMM_CSE`**: a register copy of an immediate serves the next
  uses in its block, up to 16 instructions apart.  Compute shaders only by
  default; in other stages it costs occupancy on some shaders (`=1` for all
  stages, `=0` off).
- **0058 `NAK_UADDR_PIN`**: the uniform 64-bit bases of global memory
  accesses inside a divergent if are computed before it, in UGPRs, and
  pinned (as the bindless cbuf handles of `nak_nir_lower_non_uniform_ldcx`
  are); the accesses use `[R.U32 + UR]` / `[RZ + UR]`.  The
  rematerializer treats loads through such a pair like the
  `NAK_OCC_TARGET` constant reloads.  Not for divergent loops, where it
  made the occupancy target keep 88825d04 and a87cb567 at 8 warps.
- **0059 `NAK_PLOP_FOLD`**: a PLOP3 of a comparison and one other predicate
  becomes the comparison's accumulator.
- **0060 `NAK_IF_CONVERT`**: after register allocation, then-blocks of up
  to 8 instructions (then + else up to 12) are predicated instead of
  branched around, BSSY/BSYNC pairs of regions left without branches go,
  and blocks are merged for the post-RA scheduler.  Never changes register
  counts.  `NAK_NIR_FLATTEN=1` (off by default) also lets NIR flatten small
  ifs into selects: fewer instructions, but some shaders lose occupancy.
- **0061 `NAK_BSSY_RECONVERGENT`** (off by default): on SM100+, if/else
  barriers use `.RECONVERGENT` without the BMOV.CLEAR, as the reference's
  SM120 code does.  On hardware it changes Steel Nomad's frame rate by less
  than the run-to-run noise, so it stays off.

Offline, GB202, `NAK_OCC_TARGET` on; knobs off vs the defaults:

| Steel Nomad DX12 | knobs off | 0057 | 0058 | 0059 | 0060 | defaults | ref |
|---|---|---|---|---|---|---|---|
| fragment instructions | 143389 | 143389 | 137542 | 142733 | 142322 | 135760 | |
| fragment static cycles | 2476438 | | 2313175 | 2461795 | 2467251 | 2290796 | |
| fragment code bytes | 2294224 | | | | | 2172160 | 2029056 |
| compute instructions | 70416 | 69410 | 69131 | 70244 | 69781 | 67220 | |
| compute static cycles | 1011099 | 1013407 | 997498 | 1013217 | 999418 | 989936 | |
| compute code bytes | 1126656 | | | | | 1075520 | 1042560 |

(Each patch column has only that knob on.)  With the defaults, no shader
has fewer warps than with the knobs off; seven fragment shaders get 2
memory spills and 1 fill and two compute shaders 4 and 2, none in a loop.

| Heaviest shaders: instructions | reference | knobs off | defaults |
|---|---|---|---|
| a26771ce (fragment) | 11512 | 13913 | 13114 |
| f5fb978e | 11431 | 13741 | 13213 |
| a0f662c7 | 10450 | 13164 | 12152 |
| 797434f6 | 10381 | 12901 | 12002 |
| 59e22586 | 6460 | 7781 | 7257 |
| b0825e55 | 6361 | 7739 | 7203 |
| six together | 56595 | 69239 (+22.3%) | 64941 (+14.7%) |
| 88825d04 (compute) | 2802 | 2470 | 2386 |
| a87cb567 | 2546 | 2234 | 2172 |
| 2a040240 | 18600 | 20091 | 19046 |

The reference compiles 88825d04 and a87cb567 into 128 registers with
local memory spills; NAK uses 168.

| Other corpora (`run-objects.sh`), knobs off -> defaults | instructions | static cycles | occupancy |
|---|---|---|---|
| CS2 (D3D11 via DXVK), 861 compute | -17.2% | -16.5% | 44 shaders more warps |
| CS2, 4245 fragment | -0.6% | -0.5% | unchanged |
| CS2, 1004 vertex | -0.4% | -0.5% | unchanged |
| Heaven, 185 fragment, 58 vertex, 6 tess control, 8 tess eval | 0%, 0%, +1.1%, 0% | 0%, 0%, -5.0%, 0% | unchanged |
| Steel Nomad Vulkan, 114 compute | -4.9% | -2.3% | 1 shader more warps |
| Steel Nomad Vulkan, 137 fragment | -2.2% | -1.3% | unchanged |

Correctness, offline: every shader of the four corpora compiles on a
debug-assertion build with the defaults and with every knob on
(`NAK_IMM_CSE=1 NAK_BSSY_RECONVERGENT=1 NAK_NIR_FLATTEN=1`), each with and
without `NAK_DEBUG=spill`, and the Steel Nomad DX12 corpus also with
`NAK_OCC_TARGET=0`.  With every knob at 0, NAK's output is identical to
NAK without the patches.  NAK's unit tests cover the PLOP3 fold
(`opt_setp_fold::tests`) and the if-conversion (`opt_if_convert::tests`);
the 15 host-runnable tests pass.

What is left of the gap in the six fragment shaders (+14.7%): phi copies
(24%), branch overhead (16%), BMOV.CLEAR (9%, gone with
`NAK_BSSY_RECONVERGENT=1`), descriptor index arithmetic still in GPRs
(11%), and integer address arithmetic NAK does in more instructions
(IADD3 where the reference uses IADD with the `.U32` address forms).

## More knobs (patches-common/0062-0067)

- **0062 `NAK_PHI_AVOID`** (on): a new value without a register preference
  stays out of the registers the block's phi copies want.  Steel Nomad
  fragment 135760 -> 134712 instructions, CS2 -0.5%, no register count
  changes.
- **0063 `NAK_BAR_SHARE`, `NAK_LAT`** (off): scoreboard sharing when all six
  are busy, and SM120 latency pairs without the extra cycle (bitmask, see
  `docs/WINDOWS-KNOBS.md`).  For hardware validation with `naktest`
  (`guest/nvk-rm/tools/naktest`).
- **0064 `NVK_DIAG_NO_WFI_BARRIER`, `NVK_DIAG_WFI_COUNT`** (debug): skip or
  count the WAIT_FOR_IDLE of pipeline barriers.
- **0065 `NAK_UNROLL_MAX`** (32): how far NIR unrolls loops with a known
  trip count.  The heaviest CS2 fragment shaders (0x39204a7e and its group)
  are 2.3 times the reference's code because NAK unrolls loops it keeps
  rolled (5409 instructions with 5 backward branches against 2283 with 15);
  with 8, 0x39204a7e gets 1710 instructions and 96 instead of 104 GPRs, and
  CS2's fragment shaders 8% fewer instructions with 41 gaining warps.
  Static counts favour rolled loops; the default is 32.
- **0066 `NAK_VEC_CSE`** (on): the copies that make a splat (an image store
  of `vec4(x)`) a register vector serve its later uses in the block.  CS2
  compute 0x7e5de6c5 2672 -> 2363 instructions.

- **0067 `NAK_WEAK_MEM`** (off): global, SSBO and image loads and stores
  that are not coherent, volatile or atomic use the weak memory order
  instead of `.STRONG.GPU`.  The reference's code for the same shaders uses
  weak (plain `LDG.E`, order field 0), so its loads can hit in L1; NAK's
  strong GPU-scope loads all go to L2.  Barriers keep their CCTL write-back
  and invalidate.  In GPU timing, 2a040240's light path (24 loads, 25 stores
  per thread) takes 7 times the reference's time.  `NAK_WEAK_MEM=1` passes
  naktest (`wgsync`) but changes neither that shader's GPU time nor Steel
  Nomad's frame rate, so it stays off.

What GPU timing showed beyond instruction counts: loop-heavy compute shaders
(eee8a710, 7bd2b130) spend about 40% of their inner loop on register copies
(phi copies around a structurizer state machine) and on BSSY/BSYNC/NOP,
where the reference's loop has about three copies; better phi coalescing in
register allocation is the next step there.

NAK knobs that do not fit in `nak_gpr_cap_key()` are in `nak_knobs_key()`,
which NVK puts into bits 0-11 and 56-63 of its compiler flags.

### CS2 against the reference

The reference statistics come from pipelines with generated partner stages,
which lets either compiler drop outputs the partner does not read, so for
CS2 only per-shader comparisons of the same pipeline are meaningful (the
`refsass.py` route).  Compute shaders (201 the reference compiles with the
reconstructed layouts): NAK's code is 1.04 times the reference's (median)
and 0.5% more in total.  Fragment groups worth a look: the unrolled-loop
group above, and register copies (phi and splat copies) in the image-store
compute shaders.

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

## NAK_OCC_TARGET (patches-common/0056)

On by default; `NAK_OCC_TARGET=0` turns it off (NAK then compiles as
without the patch).

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
- With `NAK_OCC_TARGET=0`, NAK's output is identical to NAK without the
  patch (checked on the whole corpus).
- Correctness, offline: the whole corpus (469 pipelines) compiles without
  an assertion failure on a debug-assertion build with `NAK_OCC_TARGET`
  0 and 1, each with and without `NAK_DEBUG=spill`. NAK's unit tests cover
  rematerialization (`spill_remat::tests`: a constant-load component
  reloaded through a UGPR address or a recomputed one, a chain with a
  resident input, UGPR coverage); the 9 host-runnable tests pass.

Offline, on the Steel Nomad corpus (default vs `NAK_OCC_TARGET=0`;
ref = NVIDIA):

| | `=0` | default | ref |
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

Forcing lower counts with `NAK_GPR_CAP_CS=128`:
88825d04 gets 3024 instructions and 162/105 memory spills/fills, a87cb567
2607 and 134/87, 2a040240 20587 and 264/201.

On an RTX 5090, Steel Nomad DX12 runs without crashes or GPU faults and
faster than with `NAK_OCC_TARGET=0`.

Open points:

- The gain model is conservative and not tuned per shader against fps.
- 88825d04 and a87cb567 stop at 168: 128 needs memory spills there, mostly
  for values that are not rematerializable and are live across the
  shader's outer loop.

## Next steps

- Hardware A/B of `NAK_BSSY_RECONVERGENT=1`, `NAK_NIR_FLATTEN=1` and
  `NAK_IMM_CSE=1` (all stages) against the defaults.
- Register allocation that coalesces phi webs better (the copies and
  swaps at if/else merges are the biggest remaining pattern).
- Hoisting uniform descriptor index arithmetic out of divergent ifs too
  (needs a UGPR budget across the region).

1. Calibrate the gain model with fps A/Bs (`NAK_GPR_CAP_CS`/`NAK_GPR_CAP_FS`
   points against the default): saturation, least gain, how many memory
   spills are tolerable.
2. Spill to shared memory instead of local memory for compute shaders whose
   spare shared memory does not lower occupancy; that would let 88825d04
   and a87cb567 reach 128.
3. Reload both halves of a vector constant at the same point with one
   64-bit load.

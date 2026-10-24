# naktest: NAK scheduling hardware validation

Compute shaders whose every instruction depends on the one before, checked
word by word against a CPU reference.  If NAK waits too few cycles between
a producer and its consumer, or two in-flight loads share a scoreboard the
wrong way, a consumer reads a stale register and the output differs.  It
exists to validate `NAK_LAT` and `NAK_BAR_SHARE` (and the if-conversion's
predicated code) on the GPU before any of them is turned on by default.

| Test | What it chains |
|---|---|
| `fma0-3` | FFMA/FMUL/FADD (values stay exact: integers and halves) |
| `imad0-2` | IMAD and IADD |
| `lop0-2` | LOP3, XOR, SHF rotates, PRMT byte swaps |
| `alu0-2` | IADD3, XOR, IMAD and SHF between two interleaved chains |
| `fmaalu0-2` | FFMA results converted and fed to integer adds (FMA/ALU cross pairs) |
| `guard0-3` | small if/else blocks: predicate guards read right after the comparison |
| `carry0-2` | 64-bit adds and shifts (carry predicates) |
| `uniform0-2` | a chain on push constants only (uniform datapath) |
| `loads8..32` | 8 to 32 independent loads in flight, consumed in shuffled order (scoreboards) |
| `lat_ffma`, `lat_imad`, `lat_alu`, `lat_fmnmx`, `lat_fmaalu`, `lat_guard`, `lat_uniform`, `lat_hfma2` | a loop (4 iterations, or `--time N`) around 512 dependent operations of one class: FFMA, IMAD, IADD3/LOP3, FMNMX, FFMA/FMNMX cross pairs, small ifs, uniform datapath, HFMA2 (needs shaderFloat16) |
| `wgsync` | invocations of a workgroup exchange values through the storage buffer around barriers (`NAK_WEAK_MEM`) |
| `tld_arr_lod`, `tex_arr_lod`, `tex_arr_off`, `tld4_arr`, `tld4_arr_off`, `tex_dc_arr`, `tex_dc_arr_off` | texel fetches, explicit-level samples, gathers and depth compares from a 2D array texture and a depth texture whose handles come from the descriptor set: the source layouts (2+1, 2+2 and 5 components) that `NAK_TEX_UHANDLE` turns into the scalar form with the handle in a uniform register |

## Build and run

```sh
guest/nvk-rm/tools/naktest/build.sh out      # out/naktest and out/naktest.exe
```

In the guest, with the knobs under test in the environment (each run
compiles fresh pipelines; the knobs are part of the shader cache key):

```bat
naktest.exe
set NAK_LAT_NV=0 & set NAK_LAT=95 & naktest.exe
set NAK_BAR_SHARE=1 & naktest.exe
set NAK_WEAK_MEM=1 & naktest.exe
set NAK_TEX_UHANDLE=1 & naktest.exe
set NAK_LAT_NV=0 & set NAK_LAT=128 & naktest.exe      rem unsafe upper bound: failures expected
```

Options: `-r REPEATS` (default 20), `-n INVOCATIONS` (default 65536, a
multiple of 64), `-t PREFIX` (only tests whose name starts with it),
`--time N` (only the `lat_*` tests unless `-t` says otherwise, their loops
run N times, and each dispatch is timed).

With one workgroup per SM (`-n` 64 times the SM count: 10880 on an RTX
5090) a warp's chain runs alone on its scheduler, so the time per operation
is the issue distance the compiler put between producer and consumer.  The
minimum over the repeats is printed in ns and in cycles at `NAKTEST_MHZ`
(default 1900, the clock the perf rows lock), next to the usual check of
the results (every 17th word):

```bat
set MESA_SHADER_CACHE_DISABLE=true
naktest.exe --time 128 -n 10880 -r 5
```

On the host's NVIDIA driver at 1900 MHz: about 4.2 cycles per operation for
FFMA, IMAD, IADD3/LOP3, HFMA2 and uniform chains, 5.2 for FMNMX and the
FFMA/FMNMX cross pairs.  Exit
status 0 when every test passes, 1 on a mismatch, 2 on a setup error.  The
same binary run with the NVIDIA driver on the host checks the tests
themselves.

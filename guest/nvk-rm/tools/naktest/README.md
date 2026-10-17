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
| `wgsync` | invocations of a workgroup exchange values through the storage buffer around barriers (`NAK_WEAK_MEM`) |

## Build and run

```sh
guest/nvk-rm/tools/naktest/build.sh out      # out/naktest and out/naktest.exe
```

In the guest, with the knobs under test in the environment (each run
compiles fresh pipelines; the knobs are part of the shader cache key):

```bat
naktest.exe
set NAK_LAT=95 & naktest.exe
set NAK_BAR_SHARE=1 & naktest.exe
set NAK_WEAK_MEM=1 & naktest.exe
set NAK_LAT=128 & naktest.exe      rem unsafe upper bound: failures expected
```

Options: `-r REPEATS` (default 20), `-n INVOCATIONS` (default 65536, a
multiple of 64), `-t PREFIX` (only tests whose name starts with it).  Exit
status 0 when every test passes, 1 on a mismatch, 2 on a setup error.  The
same binary run with the NVIDIA driver on the host checks the tests
themselves.

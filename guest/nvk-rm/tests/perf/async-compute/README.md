# RM trace and queue overlap test

Two host tools for checking how a Vulkan driver on NVIDIA's RM sets up its queues, and whether two queues
really run at the same time. Findings for the RTX 5090: `docs/research/async-compute-gb202.md`.

| file | what |
|---|---|
| `rmtrace.c` | `LD_PRELOAD` shim: logs every RM escape on `/dev/nvidiactl` and `/dev/nvidiaN` (ALLOC with class and decoded params for TSG, ctxshare, GPFIFO channel, 3D/compute/copy objects and unknown classes; CONTROL with command name, decoded BIND / SCHEDULE / work-submit token, hex dumps for FIFO/GR/TSG/ctxshare/channel controls; FREE), with status and the returned VEID of every ctxshare |
| `vkqtest.c` | one VkDevice with graphics family 0 and the compute-only family; busy-wait work per queue, GPU timestamps, overlap verdicts |
| `summarize.py` | turns a trace into the TSG / ctxshare / channel / object tree |
| `gen-names.py` | class and control names for the trace from NVIDIA's open-gpu-kernel-modules SDK headers |
| `run.sh` | runs `vkqtest` under the shim with NVIDIA's ICD and keeps the logs |

## Build

```sh
make OGKM=/path/to/open-gpu-kernel-modules   # names in the trace (any recent release)
make                                         # without names (classes and commands as numbers)
```

Needs a C compiler, the Vulkan headers and loader, `glslangValidator` and Python 3. A driver update needs only a
rebuild; the alloc-parameter layouts in `rmtrace.c` follow the SDK headers (`NV_CHANNEL_ALLOC_PARAMS` is 376
bytes), check them against a new release's `alloc/alloc_channel.h` if a decode looks wrong (the hex dump is always
printed).

## Run

```sh
./run.sh trace-gq1-cq1 --trace-only              # RM objects for 1 graphics + 1 compute queue
./run.sh trace-gq2-cq2 --gq 2 --cq 2 --trace-only
./run.sh bench-gq2-cq2 --gq 2 --cq 2 --reps 7    # overlap test, about 1 s of GPU time
python3 summarize.py logs/<stamp>-trace-gq1-cq1.rmtrace.log
```

`run.sh <label> [vkqtest args]` writes `logs/<stamp>-<label>.{info.txt,out.txt,rmtrace.log}` and never
overwrites. With `PERF_ROW_MARKER=<file>` set it first waits (30 s steps, at most 30 min) until that file is gone.
The shim alone: `LD_PRELOAD=$PWD/librmtrace.so RMTRACE_LOG=trace.log <program>`; `RMTRACE_ALL=1` also logs the
other escapes, `RMTRACE_DUMP=<bytes>` limits hex dumps (default 512). A program can put markers into the log by
calling `rmtrace_mark(const char *)` (looked up with `dlsym`).

`vkqtest` options: `--gq n` / `--cq n` queues of each family (`--cq 0` allowed with `--trace-only`),
`--spin-us us` work per queue (default 8000; calibrated against the timestamps), `--reps n`, `--wgs n` compute
workgroups (default 4), `--px n` side of the drawn square (default 8), `--cases gfx,comp,both,cc,gg,g1c`.
Cases: `both` graphics q0 + compute q0, `both1` graphics q0 + compute q1, `cc` compute q0 + q1, `gg` graphics
q0 + q1, `g1c` graphics q1 + compute q0. Both submissions of a case wait on one timeline semaphore that the CPU
signals after submitting both. Output per case:

```
RESULT both  solo 7991.8/8215.8 us  makespan 8219.4 us  sum 16207.6 us  start skew 2.8 us  overlap 100%  CONCURRENT
```

overlap = 1 - (makespan - longer solo) / shorter solo; CONCURRENT >= 75 %, SERIAL <= 25 %, else PARTIAL.

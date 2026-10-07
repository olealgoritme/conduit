# crm_ce_copy_smoke

M1 of `guest/windows/docs/rm-copy-engine-present.md`: a copy-engine (CE) channel in user mode that acquires another
client's semaphore, copies a 1600x900 BGRA frame into an OS descriptor over ordinary process pages, and releases a
completion semaphore. It prints per-stage latencies. The method encodings and their sources are listed in the header
comment of `crm_ce_copy_smoke.c`.

## Build (host, MinGW cross)

```sh
cd guest/rmclient
meson setup build-win --cross-file ../nvk-rm/windows/mingw-x86_64.ini -Dbuildtype=release
ninja -C build-win crm_ce_copy_smoke.exe librmclient.dll
x86_64-w64-mingw32-strip build-win/crm_ce_copy_smoke.exe build-win/librmclient.dll
```

A native Linux build (`meson setup build && ninja -C build crm_ce_copy_smoke`) runs the same code on a machine with
NVIDIA's driver at the release librmclient expects (`CRM_RM_VERSION`). Only `--fence` is Windows-only.

## Run in the guest

Copy both files into the public tools folder, then run it from there:

```sh
scp -P 2222 build-win/crm_ce_copy_smoke.exe build-win/librmclient.dll "$WIN_SSH:C:/Users/Public/t/"
```

1. Idle run:

   ```sh
   ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --iterations 200'
   ```

2. Full run, with the graphics-CE contender and the RM fence:

   ```sh
   ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --iterations 200 --contend --fence'
   ```

3. Under real contention: start the tool with a delay, start windowed Heaven during the delay, then let the tool run
   for a fixed time:

   ```sh
   ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --delay 30 --duration 20 --contend'
   ```

4. A/B against the graphics engine's CE: add `--engine gr` to run 1.

Options (defaults in brackets):
- `--gen gb202|ada` [gb202]
- `--engine <n>|gr` [the first async CE that `GET_ENGINES_V2` + `CE_GET_CAPS_V2` report]
- `--iterations <n>` [200], or `--duration <s>`
- `--delay <s>`: wait after setup, before the loop [0]
- `--hold-ms <ms>`: producer hold in the wait stage [2]
- `--size WxH`: copy W*H*4 bytes [1600x900]
- `--src vid|sys`: the source's memory [vid; falls back to sys if BAR1 refuses the map]
- `--contend`, `--contend-engine <n>|gr` [gr], `--contend-mb <n>` [64]
- `--vas device|new`: the device's VA space or a new `FERMI_VASPACE_A` [device]
- `--release cpu|semsurf`: the producer value is set by a CPU store or by `SET_VALUE` 0xda0004 [cpu]
- `--timeout-ms <ms>`: bound of every CPU wait [2000]
- `--fence`: Windows only; also time doorbell -> RM fence event

Exit 0 on PASS, 1 on FAIL.

## Expected output

The setup runs one `[ ok ]` line per RM call. The lines that matter:

```
       COPY0 (type 0x9): caps .. .. GRCE ...
       COPY2 (type 0xb): caps .. .. async SYSMEM_WRITE
       ce channel: engine COPY2 (type 0xb), token 0x00XX00YY -> runlist R, channel id C
precondition: copy_bytes=5760000 pages=1407 (4 KiB pages touched by the destination)
copy_bytes=5760000
[ ok ] ce channel alive: SET_OBJECT + release seen
       runlists: ce channel R1, contend channel R2 (separate runlists: can run concurrently)   (--contend)
status: measured loop starts (iterations)
status: measured loop ends after 200 iterations, N s

stage                                       n        min        avg        p50        p99        max
submit_to_done_acquire_satisfied_us       200        ...
acquire_satisfy_to_done_us                200        ...
doorbell_to_gpfifo_get_us                 200        ...
copy_us                                   200        ...
copy_us_wait_stage                        200        ...
doorbell_to_event_us                      200        ...   (--fence)
submit_to_done_contended_us               200        ...   (--contend)
copy_us_contended                         200        ...   (--contend)
contend_copy_us                           200        ...   (--contend)

copy_bytes=5760000
copy_gbps_p50=...
acquire_held=200/200
contend_overlapped=N/200                                   (--contend)
userd_gp_get_written_back=no
verify=ok (13 checks, 0 bad bytes)
       copier: objects still tracked 0, CPU mappings 0
       producer: objects still tracked 0, CPU mappings 0
RESULT PASS
```

What the stages are:
- `submit_to_done_acquire_satisfied_us`: the producer value is already set; doorbell to the completion value seen by the
  CPU.
- `acquire_satisfy_to_done_us`: submitted first, and the GPU waits on the acquire. After `--hold-ms` the CPU sets the
  value; from that store to completion.
- `doorbell_to_gpfifo_get_us`: a push of one release without WFI, from the doorbell to that release. It stands in for
  `GP_GET`, which USERD does not report under GSP.
- `copy_us`: from the CE's own GPU timestamps, around the copy only.
- `copy_us_contended` and `contend_copy_us`: the same copy while a `--contend-mb` copy on the second channel is in
  flight, and the CPU-seen time of that second copy.

## Pass criteria

- `RESULT PASS`, which needs every RM call OK, `acquire_held` equal in both numbers, `verify=ok` with 0 bad bytes, the
  error notifier still 0, and 0 objects and mappings left in both clients.
- Section 5 thresholds for M3: `submit_to_done_acquire_satisfied_us` p50 below 400 us, and `acquire_satisfy_to_done_us`
  p50 below 50 us plus `copy_us` p50.
- With `--contend`, a "separate runlists" line, and `contend_overlapped` close to the number of rounds.

## What to send back

- The full output of each run you did (idle, `--contend --fence`, and the `--delay/--duration` run with Heaven), with
  the command line.
- On FAIL: the `RESULT FAIL ...` line and the 30 lines before it.
- The host's guest-RAM backing state at the time of the run (doc section 8: memfd/shmem THP or reserved hugepages).
- The KMD counters `NvPin` and `NvUnpin` before and after the run. They must be equal afterwards.
- Whether the guest showed any TDR or display hitch while the tool ran.

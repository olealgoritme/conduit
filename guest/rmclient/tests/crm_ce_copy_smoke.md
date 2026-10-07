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
- `--bl-roundtrip`, `--bl-probe-pitch`, `--modifier <hex>`, `--bl-kind <hex>`: the block-linear check (below)
- `--remap none|swap-rb`, `--bl-src-only`: R/B swap inside the copy with the CE remap unit (below) [none]

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

## Block-linear round trip (M1b)

`--bl-roundtrip` replaces the measured loop. It checks on hardware the block-linear copy words before the KMD uses them for
Heaven's windowed source (modifier 0x0300000000606014: h = 4, so blocks of 16 GOBs or 128 rows; page kind 0x06; GOB
generation 2; sector layout; no compression). No CPU-side GOB swizzle is involved.

Setup:
- The producer allocates the image as plain video memory. Its size is the modifier's layout: pitch = `width * 4` rounded up to
  whole 64-byte GOBs (6400 = 100 GOBs), rows = the height rounded up to whole blocks (1024), so 6553600 bytes, the same
  arithmetic as `foreign_resource::Layout`.
- The copier dups the image and GPU-maps it with big pages and the PTE kind: `NVOS46_FLAGS_PAGE_KIND_OVERRIDE` plus
  `kindOverride`, as nvk-rm patch 0005 binds an image's VA.

Per round, one push:
1. A CE copy from the pitch source to the block-linear image (the destination block-linear state: `SET_DST_BLOCK_SIZE`
   0x1040, `DST_WIDTH` = pitch, `DST_HEIGHT` = 900, `DST_ORIGIN` 0).
2. A host release with WFI.
3. A CE copy from the block-linear image to the pitch destination (the OS descriptor over process pages). The words of this
   copy are exactly the ones `ce_present.rs` emits for the windowed source.
4. The completion release.

Each copy carries its own pair of CE timestamps. The pattern comes from a fixed seed (`--seed`, default 0xc0e5a11d), so
runs with the same seed have byte-identical sources. So that stale video memory from an earlier run with the same seed
cannot pass, each round first zeroes the whole image (padding rows included). The zeroing is a PITCH -> PITCH copy from
zeroed pages, the path run 1 already proved. `--bl-probe-pitch` adds one more push after the rounds: it copies the image out PITCH -> PITCH (the same
memory read as plain rows of the image pitch) and compares a position-dependent checksum with that of the unswizzled
pattern. The two must differ.

Run lines (after the scp above):

```sh
ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --bl-probe-pitch'
ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --bl-probe-pitch --bl-kind 0'
```

Both run lines use the default seed, so their `pattern_digest` lines must be equal. Pass the same `--seed <n>` to both runs
if you change it.

Options: `--seed <n>`, the pattern seed in every mode [0xc0e5a11d; 0 is allowed, and then only word 0 of the pattern is
0]; `--modifier <hex>` (32 bpp uncompressed NVIDIA block-linear 2D only, `h <= 5`) [0x0300000000606014];
`--bl-kind <hex>`, the PTE kind of the image's mapping [the modifier's k, 0x06; 0 = no override, the allocation's own pitch
kind]; `--iterations <n>` rounds [16]; `--size WxH` [1600x900]. `--contend`, `--fence` and `--duration` are refused with
this mode.

Expected output (first run):

```
bl_seed=0xc0e5a11d
bl_modifier=0x0300000000606014 h=4 (blocks of 128 rows) k=0x06 g=2 s=1 c=0
bl_image: pitch=6400 (100 GOBs) rows=1024 size=6553600 block_size_word=0x1040
bl_kind=0x06 (PAGE_KIND_OVERRIDE on the image's mapping, the modifier's k)
...
[ ok ] alloc NV01_MEMORY_LOCAL_USER 0x40 8388608 bytes (block-linear image)
[ ok ] copier: DUP_OBJECT block-linear image 0x... of client 0x...
[ ok ] GPU-map block-linear image (dup), PTE kind 0x06 (override) (8388608 bytes) -> VA 0x...
[ ok ] ce channel alive: SET_OBJECT + release seen
status: block-linear round trip, 16 rounds

stage                                       n        min        avg        p50        p99        max
bl_copy_pitch_to_bl_us                     16        ...
bl_copy_bl_to_pitch_us                     16        ...
bl_roundtrip_doorbell_to_done_us           16        ...

bl_copy_gbps_p50=...
bl_roundtrip=ok (16 rounds, 0 bad, 0 bad words)
bl_probe_pitch=swizzled checksum_bl_as_pitch=0x... checksum_unswizzled=0x... words_in_place=N/1440000
bl_probe_digest=0x... seed=0xc0e5a11d bl_kind=0x06
pattern_digest=0x... seed=0xc0e5a11d
bl_probe_vs_kind_note: compare bl_probe_digest between --bl-kind 0x06 and --bl-kind 0 runs with the same --seed: equal digests mean the page kind does not change the physical layout of this copy path
       copier: objects still tracked 0, CPU mappings 0
       producer: objects still tracked 0, CPU mappings 0
RESULT PASS
```

`words_in_place` is expected to be a small fraction: a few words, such as the first bytes of row 0, land where the pitch
layout puts them. It is informational; the verdict is the checksum. PASS needs
`bl_roundtrip=ok`, `bl_probe_pitch=swizzled`, the error notifier still 0, and nothing left tracked. The copy times should be
close to the pitch `copy_us` of run 1 (about 0.2 ms for 5.76 MB). A much slower block-linear read is a finding for M3.

With `--bl-kind 0` the tool prints `bl_kind=0x00 (no override: the allocation's own pitch kind)` and an `[info]` line. The
expected result is still `bl_roundtrip=ok`: both copies go through the same mapping, so a wrong kind cancels out. The CE
computes the block-linear addresses itself, so `swizzled` is also expected.

The two runs' digest lines decide whether the kind matters. `bl_probe_digest` is FNV-1a 64 over the first `height` rows (900) of
the image read back as pitch. `pattern_digest` is the same digest of the unswizzled pattern. Compare the runs:
- `pattern_digest` differs between the runs: the seeds differ, so the comparison is void. Rerun both with the same
  `--seed`.
- `pattern_digest` is equal and `bl_probe_digest` is equal: the page kind (0x06 or the pitch kind) does not change the
  physical layout the CE writes through this mapping. The KMD's kind choice then cannot scramble a source on this copy
  path, though M3c still has to check it against NVK's 3D writes.
- `pattern_digest` is equal and `bl_probe_digest` differs: the kind changes the physical layout. The KMD must map NVK's
  image with exactly NVK's kind (0x06, `SourcePlan::page_kind`), and M3c is the check that it does.

The round trip therefore proves that
the CE accepts the block-linear words and that the two directions invert each other. It does not prove that they, or the
mapping kind, match NVK's layout (a block height that is wrong in both directions cancels out too). Only M3c, which reads a
real NVK image, checks that (`rm-copy-engine-present.md` 10.4).

On failure:
- A bad encoding raises an RM channel error (RC) on the tool's own channel. The tool prints `[FAIL] ce channel: channel
  error notifier status 0x.. info32 0x.. (RC error)`, the `middle release seen / NOT seen` line (which copy it stopped in),
  the completion value and USERD, then tears down and prints `RESULT FAIL bl round trip: ...`. That is a reset of the
  tool's channel, not a GPU hang. Every wait is bounded by `--timeout-ms`.
- Wrong data with no error prints `bl_roundtrip=BAD`, up to 8 `mismatch round R: offset 0x.. (x X, y Y): want 0x.. got
  0x..` lines, and `RESULT FAIL block-linear round trip matches the pattern`.
- `bl_probe_pitch=IDENTICAL` means the copy did not swizzle: block-linear was not in effect. That is a FAIL.

Send back the full output of both runs.

## Format conversion with the remap unit (M1c)

Heaven's windowed source is RGBA (`AB24`) and DWM's redirection surface is BGRA (`AR24`), so every windowed Present
swaps bytes 0 and 2 of each pixel. `--remap swap-rb` does that inside the copy with the CE's remap unit
(`SET_REMAP_COMPONENTS` 0x03303012: DST_X = SRC_Z, DST_Z = SRC_X, Y and W identity, 1-byte components, 4 in and 4
out; `LAUNCH_DMA.REMAP_ENABLE`). With the remap on, the X quantities of the copy (`LINE_LENGTH_IN`,
`SET_SRC/DST_WIDTH`, `SRC/DST_ORIGIN_X`) are counted in 4-byte pixels instead of bytes, as NVK does; the pitches stay
bytes. The source is the fixed seeded pattern, read as RGBA; the CPU checks that the destination is the same pattern
with bytes 0 and 2 exchanged. Design and fallbacks: `rm-copy-engine-present.md` section 12.

Run lines, in this order (after the scp above). Run 3 is the open question; run it on its own line so a refusal or a
hang belongs to that combination alone:

```sh
ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --iterations 200 --remap swap-rb'
ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --bl-roundtrip --remap swap-rb'
ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --bl-src-only'
```

1. **Measured loop with the remap** (`--remap swap-rb` without `--bl-roundtrip`): the ready, wait and contended copies
   remap; the contender does not. Output as run 1 above, plus:

   ```
   remap=swap-rb SET_REMAP_COMPONENTS=0x03303012 on the measured copies (...)
   ...
   verify=ok (13 checks, 0 bad bytes)
   remap_verify=ok (swap-rb: the destination is the pattern with bytes 0 and 2 exchanged; 13 checks, 0 bad words)
   remap_gbps path=pitch_to_pitch on=... (compare with copy_gbps_p50 of the same run line without --remap)
   RESULT PASS
   ```

2. **The matrix** (`--bl-roundtrip --remap swap-rb`, 16 rounds per combination): instead of the round trip, five
   combinations run one after the other, each all of its rounds before the next starts:

   | combination | copies | the destination must be |
   |---|---|---|
   | `pitch_to_pitch_off` | pitch -> pitch | the pattern |
   | `pitch_to_pitch_on` | pitch -> pitch with remap | swapped |
   | `bl_off` | pitch -> BL, BL -> pitch | the pattern |
   | `bl_dst_remap` | pitch -> BL with remap, BL -> pitch | swapped (fallback B: remap while writing the image) |
   | `bl_src_remap` | pitch -> BL, BL -> pitch with remap | swapped (the open question; Heaven's case) |

   The image is zeroed before each block-linear round, as in the round trip. Expected output:

   ```
   remap=swap-rb SET_REMAP_COMPONENTS=0x03303012 (...)
   remap_mode=matrix
   status: remap combination pitch_to_pitch_off starts: pitch -> pitch, no remap, 16 rounds
   remap_combo pitch_to_pitch_off: accepted, 16 rounds; remap_verify=ok (0 bad words; want the plain pattern)
   status: remap combination pitch_to_pitch_on starts: ...
   remap_combo pitch_to_pitch_on: accepted, 16 rounds; remap_verify=ok (0 bad words; want the swapped pattern)
   ... (bl_off, bl_dst_remap, bl_src_remap)

   stage                                       n        min        avg        p50        p99        max
   remap_off_pitch_to_pitch_us                16        ...
   remap_on_pitch_to_pitch_us                 16        ...
   remap_off_pitch_to_bl_us                   32        ...   (bl_off and bl_src_remap)
   remap_on_pitch_to_bl_us                    16        ...   (bl_dst_remap)
   remap_off_bl_to_pitch_us                   32        ...   (bl_off and bl_dst_remap)
   remap_on_bl_to_pitch_us                    16        ...   (bl_src_remap)

   copy_bytes=5760000 (1600x900, 4 bytes per pixel)
   remap_gbps path=pitch_to_pitch off=... on=... on_vs_off=...%
   remap_gbps path=pitch_to_bl off=... on=... on_vs_off=...%
   remap_gbps path=bl_to_pitch off=... on=... on_vs_off=...%
   remap_accepted: pitch_to_pitch_off=yes pitch_to_pitch_on=yes bl_off=yes bl_dst_remap=yes bl_src_remap=yes
   remap_verify=ok (every combination that ran)
   remap_open_question: REMAP with a block-linear source accepted, remap_verify=ok
   RESULT PASS
   ```

3. **The open question alone** (`--bl-src-only`, which implies `--bl-roundtrip --remap swap-rb`): only
   `bl_src_remap`: the pitch pattern into the block-linear image WITHOUT the remap (the M1b words), then the image into
   the pitch destination WITH the remap. `remap_mode=bl-src-only`, the other combinations print `not-run`.

The time rows are GPU time between the CE's own timestamps around each copy (not the CPU's view). `on_vs_off` is the
remap-on rate as a percentage of the remap-off rate of the same path.

On failure (run 2 or 3):
- A combination the engine refuses raises an RC error on the tool's channel: `[FAIL] ce channel: channel error notifier
  status 0x.. info32 0x.. (RC error)`, then `remap_combo <name>: NOT accepted after N rounds: ...`.
- A hang (no completion within `--timeout-ms`) prints `remap_combo <name>: NOT accepted ...: completion not seen` and,
  for a block-linear combination, whether the middle release landed (`seen`: the first copy finished, it stopped in the
  BL -> pitch copy; `NOT seen`: it stopped in the pitch -> BL copy).
- Either way the tool then gives the channel one more `--timeout-ms`, prints `ce channel error notifier: status 0x..
  info32 0x..`, the completion value and USERD, the stage table of what completed, `remap_accepted:` with `NO` for the
  combination and `not-run` for the rest, tears the channel down (schedule off, the TSG freed first) and prints
  `RESULT FAIL ...`. A failing `bl_src_remap` also prints `remap_open_question: REMAP with a block-linear source was
  NOT accepted on this engine; ...`.
- Wrong data with no error: `remap_verify=BAD`, up to 8 `mismatch <combination> round R: offset .. (x X, y Y): want
  0x.. got 0x.. (source 0x..)` lines per combination, and `RESULT FAIL remap combination <name> matches the ...
  pattern`. `got` equal to `source` means the remap was not applied; `got` equal to `want` with the bytes in another
  order means the components were selected differently.

Pass criteria: `RESULT PASS`; `remap_accepted` all `yes`; `remap_verify=ok`; every `on_vs_off` at 90% or more (the
remap costs at most about 10% of the throughput); the pitch rates near 28 GB/s (`copy_gbps_p50` of run 1). Send back
the full output of the three runs.

## What to send back

- The full output of each run you did (idle, `--contend --fence`, and the `--delay/--duration` run with Heaven), with
  the command line.
- On FAIL: the `RESULT FAIL ...` line and the 30 lines before it.
- The host's guest-RAM backing state at the time of the run (doc section 8: memfd/shmem THP or reserved hugepages).
- The KMD counters `NvPin` and `NvUnpin` before and after the run. They must be equal afterwards.
- Whether the guest showed any TDR or display hitch while the tool ran.

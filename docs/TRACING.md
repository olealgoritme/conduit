# Tracing GPU requests

Everything a VM does with the GPU arrives at the host as a request to the GPU
backend (`conduit-backend`): opening a device node, an RM allocation or
control, an NVKMS or DRM ioctl, a UVM call, an mmap. The backend can record
every one of them, with what was asked, what came back, and where the time
went. Use it to find out why an application fails inside the VM (which call
was refused, and why), or where its latency comes from (the host driver, or
the backend in front of it).

Tracing costs nothing while it is off and can be turned on and off on a running
VM without restarting anything.

## Quick start

```sh
conduit trace myvm --follow                    # watch requests live, one per line
conduit trace myvm --follow --filter errors    # only what failed
conduit trace myvm --summary --duration 30     # 30 s of latency statistics
conduit trace myvm -o run.jsonl                # record until Ctrl-C
conduit trace analyze run.jsonl                # summarise a recording
```

`conduit trace myvm` with none of `--follow`, `--summary` or `-o` records to
`conduit-trace-myvm-<time>.jsonl` in the current directory.

## Turning it on

### On a running VM

`conduit up` and `conduit view` start the backend with a control socket,
`$XDG_RUNTIME_DIR/conduit/NAME/trace.sock`. Tracing is on while at least one
`conduit trace NAME` is connected to it and goes off when the last one exits
(Ctrl-C, `--duration SECS`, or the pipe it writes to closing).

```text
conduit trace NAME [--follow] [--summary] [--filter WHAT]... [-o FILE] [--format json|bin] [--duration SECS]
conduit trace NAME status        # is it on, who is reading, records written and dropped
conduit trace NAME on|off        # resume or pause the backend's own --trace file
conduit trace analyze FILE [--filter WHAT]... [--follow]
```

`--follow`, `--summary` and `-o` combine: `conduit trace myvm --follow -o
run.bin --summary` shows each request, records them in the binary format, and
prints the summary at the end. `--filter` applies to all three.

### From the start

To capture a VM's boot, give the backend a file before it starts. Either set
`CONDUIT_TRACE` when starting the VM:

```sh
CONDUIT_TRACE=$HOME/boot.jsonl conduit up myvm
```

or, running the backend by hand, pass `--trace PATH`:

```sh
conduit-backend --socket /tmp/nvgpu.sock --trace /tmp/boot.bin --trace-socket /tmp/trace.sock
```

The file is written from the first request on. A name ending in `.bin` gets the
binary format, anything else JSON Lines; `--trace-format json|bin` (or
`CONDUIT_TRACE_FORMAT`) overrides that. The file is created (truncated) when the
backend starts: the backend's sandbox allows it to create no file later.

Pause and resume the file without stopping the VM with `conduit trace NAME
off` / `on`, or `kill -USR1 <backend pid>` (each signal flips it).

A VM run through libvirt (`conduit attach`) has the control socket too; its
backend runs under systemd, so `CONDUIT_TRACE` is not passed to it. Use the
live trace there.

## Reading the live view

```text
    0.000000 h=3    alloc     NV01_DEVICE_0 (0x80)                                       48/48     ok            31.2µs  [q 2.1µs host 27.9µs +1.2µs]
    0.000044 h=3    control   NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2 (0x800292)            32/32     ok            12.0µs  [q 1.5µs host 9.8µs +0.7µs]
    0.000102 h=3    control   NV2080_CTRL_CMD_GPU_GET_PIDS (0x2080018d)                  32/32     refused:allowlist   1.9µs
    0.001877 h=7    uvm       UVM_MAP_EXTERNAL_ALLOCATION                                552/552   ok             1.84ms  [q 3.0µs host 1.83ms +4.1µs]
```

Columns: seconds since the first request shown; the guest's file handle; the
kind of call; what it is (escape, class, control command, NVKMS or UVM
command, DRM ioctl, by name where it has one, with the number in brackets);
parameter bytes in/out; the outcome; total latency, and the split of it when
the host driver was called. With a colour terminal the latency is green under
100 µs, yellow under 1 ms and red above; failures are red. `NO_COLOR` turns
colour off.

`x2` after the split means the request took two host ioctls (the backend
sometimes retries or needs a second call); the host span is from the start of
the first to the end of the last.

## Filters

`--filter` takes a comma-separated list, and can be repeated.

| term | selects |
|---|---|
| `alloc` `control` `free` `dup` `map` `unmap` | the RM escape of that name |
| `rm` | every ioctl on `/dev/nvidiactl` and `/dev/nvidiaN` (the six above and all other escapes) |
| `nvkms` | ioctls on `/dev/nvidia-modeset` |
| `drm` | ioctls on DRM render nodes |
| `uvm` | ioctls on `/dev/nvidia-uvm` |
| `open` `close` `mmap` `munmap` | those messages |
| `event` | notifications sent to the guest (a watched file has an event) |
| `display` `clipboard` `files` | scanout and cursor, clipboard, the driver's /proc and /sys files |
| `errors` | the guest saw a failure: an errno, a non-zero RM status, or a refusal |
| `refused` | the backend refused it (see `refusal` below) |
| `slow:>1ms` | total latency above a threshold; units `ns`, `us`, `ms`, `s` |

Call terms are alternatives (`alloc,control` is either); `errors`, `refused`
and `slow:` narrow whatever the call terms selected. `--filter
control,slow:>500us` is every RM control slower than half a millisecond.
Markers for dropped records always pass.

## The summary

`--summary` and `conduit trace analyze FILE` print:

- the number of requests, the span they cover and the rate, records dropped,
  and the host driver release;
- per call kind: count, p50, p95, p99 and maximum total latency, the median
  time inside the host driver, and how many failed;
- the 20 RM control commands with the highest maximum latency, with their
  count, p50, p99, maximum, and total time spent in them;
- failures grouped by what was asked and how it failed (errno, RM status, or
  refusal reason), most frequent first.

Percentiles are nearest-rank over every request in the trace (or matching the
filter).

## Record fields

One JSON object per line. The first line is a header,
`{"conduit_trace":1,"driver":"580.178.04"}` (`null` if the release was not
known). Fields that are not meaningful for a request are left out.

| field | meaning |
|---|---|
| `ts_ns` | `CLOCK_MONOTONIC` time (ns) the backend took the request off the virtqueue |
| `handle` | the guest's file handle; for an `open`, the handle it was given |
| `kind` | `open` `close` `ioctl` `mmap` `munmap` `event` `other` |
| `call` | the finer classification `--filter` uses (table above) |
| `nr` | the ioctl number exactly as the guest sent it (for an `open`, the device type) |
| `op` | its name: the `NV_ESC_*` escape (`RM_CONTROL`), the UVM command, the DRM ioctl, `NVKMS`, or for an `open` the device node |
| `sub` | the RM class (`alloc`), RM control command (`control`) or NVKMS command index (`nvkms`) |
| `name` | the name of `sub`: class, control command or NVKMS command |
| `in` / `out` | parameter bytes sent by the guest / returned to it (for `mmap`, `in` is the length asked to be mapped) |
| `errno` | what the guest's syscall returns: 0, or a positive errno |
| `nv_status` | RM's status word for alloc, control, free, dup, map and unmap (`0x0` is `NV_OK`, `0x56` `NV_ERR_NOT_SUPPORTED`, `0x22` `NV_ERR_INVALID_CLASS`...) |
| `refusal` | why the backend answered without the host (below) |
| `host_calls` | host ioctls the request needed |
| `queue_us` | received → first host ioctl started: decoding, policy checks, translating handles and buffers |
| `host_us` | first host ioctl started → last one returned: time in the NVIDIA driver |
| `after_us` | last host ioctl returned → reply handed back to the guest: copying results back, writing the reply |
| `total_us` | received → reply handed back |
| `count` | on a `dropped` line: how many records were lost |

Times are microseconds with three decimals, exact to the nanosecond. Identifiers
are hex strings so they can be grepped for: `grep 0x2080018d run.jsonl`.

"Handed back" is the reply being placed in the virtqueue's used ring. The guest
is notified once per batch of replies, right after; that notification and the
guest's own wakeup are not in these times. An `event`'s `total_us` is how long
handing the notification to the guest took; `errno` 105 (`ENOBUFS`) on one
means the guest had no buffer posted for it and it was dropped.

### Refusal reasons

| `refusal` | the backend answered itself because |
|---|---|
| `caps` | the VM's capabilities (`--caps`) do not include it, e.g. a video class without `video` |
| `allowlist` | RM does not serve that control or class to an unprivileged caller, its parameters are not the size RM's are, or it is on the deny list (it answers about the host, not the guest) |
| `abi` | the escape is unknown to the host release, or the wrong size for it |
| `host-display` | an NVKMS command that would act on the host's own display |
| `uvm-pageable` | the UVM file's VA space allows pageable access on a release with no flag to forbid it |
| `vram-limit` | the allocation would take the VM over its video memory limit |
| `local` | answered here on purpose, with success (NVKMS vblank semaphore control) |
| `bad-request` | malformed, or a handle that is not open |

The backend log (`conduit logs NAME backend`) says the detailed reason the
first time each refusal happens.

## Binary format

A 32-byte header, then fixed 72-byte records, all little-endian. Smaller and
cheaper to write than JSON; `conduit trace analyze` reads both, and tells them
apart by the first bytes.

```text
header:  0 magic "CNDTRACE"   8 format version u16 (1)   10 record length u16 (72)
        16 driver major u32  20 minor u32  24 patch u32 (all 0 if unknown)
record:  0 ts_ns u64          8 handle u32      12 kind u8    13 call u8
        14 refusal u8        15 flags u8        16 nr u32     20 sub u32
        24 in u32            28 out u32         32 errno i32  36 nv_status u32
        40 host_calls u16    48 host start u64  56 host end u64  64 reply u64
```

`flags`: bit 0 `sub` present, bit 1 `nv_status` present, bit 2 host span
present. The three trailing times are nanosecond offsets from `ts_ns`. Kind,
call and refusal numbers are the `#[repr(u8)]` values in
`host/backend/trace/src/lib.rs`. The crate `conduit-trace` there is the
reference reader (`read::Reader`).

## Names

Class, control, UVM, DRM and NVKMS names come from
`host/backend/gen/src/names/`, generated from NVIDIA's open-gpu-kernel-modules
headers and the kernel's `drm.h` by `host/backend/gen/names_extract.py`. NVKMS
numbers its commands by position in an enum that changes between releases, so
NVKMS names depend on the host release in the trace header. A number with no
name is shown as the number alone.

## Overhead

With tracing off, the serving thread checks one relaxed atomic flag per batch
of requests and runs exactly the untraced code otherwise: the request loop is
compiled twice, and the traced copy is only entered while someone is reading.
No clock is read, nothing is allocated and nothing is written.

With tracing on, each request costs two `CLOCK_MONOTONIC` reads plus two per
host ioctl (vDSO, no syscall), a small allocation, and a non-blocking send into a bounded lock-free
queue. Encoding and all I/O happen on a separate writer thread that drains the
queue every 2 ms. If the writer falls behind (a slow `--follow` terminal, a
busy disk), records are dropped rather than slowing the VM down; the trace
then contains a `dropped` line saying how many.

Measured with a host driver stub that answers instantly, so the figures are
the backend's own cost per request (`bench_dispatch` in
`host/backend/device/src/nvidia/tests.rs`):

```sh
cd host/backend
cargo test --release -p device --lib bench_dispatch -- --ignored --nocapture
cargo test --release -p device --lib --no-default-features bench_dispatch -- --ignored --nocapture
```

| build / state | ns per request |
|---|---|
| trace code compiled out (`--no-default-features`) | 59.3 – 60.7 |
| compiled in, off, checked on every request | 60.0 – 60.4 |
| compiled in, on | 145 – 150 (about +87 ns) |

The difference between compiled out and off is within run-to-run noise. A
real RM control takes the host driver several microseconds or more, so with
tracing on the added cost is typically a few percent of a request.

The `trace` cargo feature of the `device` crate (on by default) removes all of
the tracing code when turned off.

## Control socket

`conduit trace` speaks a one-line protocol on `trace.sock`; anything that can
open a Unix socket can use it:

| send | get |
|---|---|
| `stream` or `stream bin` | the binary header, then records until you disconnect |
| `stream json` | the header line, then JSON Lines until you disconnect |
| `status` | one line: on/off, the file's state, readers, records written and dropped |
| `file on` / `file off` | resume / pause the `--trace` file |
| `stages on` / `stages off` | frame stage timing on / off (below); `ok` |
| `stages status` | `stages on` or `stages off` |
| `stages dump` | every stage stamp since the last dump, as a binary dump (below), then the connection closes |

```sh
{ echo 'stream json'; sleep infinity; } | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/conduit/myvm/trace.sock
```

A reader that stops reading for a second is disconnected so it cannot stall
the others.

## Frame stage timing

The request trace above sees the RM side. A Windows guest's frames take a
different road: the windowed (blit-model) Present is a copy the guest driver
submits as a fenced Venus `SUBMIT_3D`, and the desktop's flip is a
`ScanoutFlip`. Frame stage timing follows every one of them from the guest
driver's DDI to the interrupt that tells it the work is done, stamping the
time at each stage on both sides, and prints where the frame time goes:

```sh
conduit trace win11 stages --duration 10                 # host side only
guest/windows/ci/vmtest/stages.sh win11 10               # host and guest driver, over SSH
conduit trace win11 stages --guest-cmd CMD --save DIR --perfetto run.json
conduit trace stages DIR [--perfetto run.json] [--etw dxgkrnl.csv]   # again, from a saved collection
```

Frames per second is only the final sanity number; this table is the budget
behind it.

### What identifies a frame

Nothing new travels on the wire. A copy is identified by its fence, which the
guest driver puts in the `SUBMIT_3D`'s `ctrl_hdr` (`fence_id`, on ring 1) and
the host echoes; a flip by `ScanoutFlip::seq`. The guest driver draws its
fence ids from one counter for every context, so the id is unique for the
whole guest. The collector joins on the low 32 bits of both (what the guest
driver's ring keeps).

### The stages

| side | stage | when |
|---|---|---|
| guest driver | kmd present | `DxgkDdiPresent` entered for the Blt |
| | kmd defer | the copy queued for the driver's worker: the producer's GPU work was not done (BltAsync deferred copies only) |
| | kmd submit | just before the copy's descriptor is put on the ring (so the host cannot see it earlier) |
| backend | backend kick | the control queue's kick handled (the last one before the command was decoded) |
| | backend decoded | the command taken off the ring and parsed |
| renderer | venus recv | `conduit-venus` received the Venus command stream |
| | venus submitted | `virgl_renderer_submit_cmd` returned (the stream is queued to virglrenderer's render server thread) |
| backend | backend submitted | the renderer's `SUBMIT` reply arrived |
| renderer | venus fence | `CREATE_FENCE` received |
| backend | backend fence | the renderer's `CREATE_FENCE` reply arrived; the chain is held |
| vkr | vkr submit, vkr submit done | the render server called and returned from `vkQueueSubmit` for the stream's last submit (concurrent with the two rows above, so shown as their own rows) |
| | vkr fence | vkr submitted the ring's sync fence (after decoding the stream) |
| | gpu | the GPU time of that submit, from timestamp queries vkr wraps around it |
| | vkr fence done | vkr's sync thread saw the sync fence signalled |
| renderer | venus signal | virglrenderer's fence callback ran |
| | venus push | the signal sent to the backend |
| backend | backend signalled | the signal reached the backend |
| | backend used | the chain put on the used ring |
| | backend irq | the guest notified (`signal_used_queue` returned: the call eventfd written, which KVM injects as the interrupt; with `EVENT_IDX` the write can be skipped when the guest asked for no interrupt) |
| guest driver | kmd isr | the last interrupt the driver took before it found the completion |
| | kmd done | the completion found in the used ring |

A flip: kmd flip ddi (`SetVidPnSourceAddress` entered), kmd flip submit,
backend kick, backend decoded, backend display (handed to the viewer), backend
used, backend irq, kmd flip isr, kmd flip ack.

Not instrumented yet: the Present's DMA completion reported to dxgkrnl (the
driver's WDDM completion entry does not carry the copy's fence id) and the
vsync tick that retires a flip (matched by address, not by `seq`); see
`guest/windows/docs/kmd-handoff-2026-10.md` section 10. The viewer's own
presentation of a flip is outside the backend and not stamped.

### Turning it on

The host side costs nothing until asked for: `conduit trace NAME stages`
sends `stages on` to the backend's trace socket, the backend tells
`conduit-venus` (and through a hook, its virglrenderer) at its next fence
completion, and `stages off` at the end turns both off again. To stamp from
the start, set `CONDUIT_STAGE_TRACE=1` in the environment of the backend and
`conduit-venus` (as `CONDUIT_TRACE` above). It needs a backend built with
Venus (every package is) and a `conduit-venus` with `FEATURE_STAGE_TRACE`;
the vkr stages and the GPU time need virglrenderer with
`host/venus/patches/0003-vkr-stage-timing.patch` (a virglrenderer without it
still works, without those rows).

The guest side is the guest driver's `StageTrace` knob (REG_DWORD 1 in the
`helios_kmd_render` service key, read at StartDevice: restart the device after
setting it). The driver then publishes its ring as the REG_BINARY value
`StgRing`, at most twice a second, and `--guest-cmd` is any command that
prints it, e.g. `ssh GUEST reg query
HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v StgRing`
(`stages.sh` uses `WIN_SSH`). The collector runs it every second.

### Reading the table

```text
clock: guest->host offset 4999999988968 ns, error bound +-41.4 us (from 1600 frames), drift 0.39 ppm
records: host 8800 (0 lost), guest 3600 (0 lost); frames: 800 joined, 0 host only, 0 guest only

(a) windowed copy: 400 frame(s), frame interval 4400.0 us (227.3 fps)
stage                                                       n   mean us       p50       p99  % frame
kmd present -> kmd defer                                  400      24.9      24.8      29.9      0.6
kmd defer -> kmd submit                                   400     585.2     585.5     589.9     13.3
kmd submit -> backend kick                                400      56.1      56.0      61.3      1.3
...
backend fence -> vkr fence                                400     305.2     305.2     309.9      6.9
vkr fence -> vkr fence done                               400     254.7     254.2     259.8      5.8
...
backend irq -> kmd isr                                    400      18.6      18.5      23.7      0.4
kmd isr -> kmd done                                       400      25.1      25.0      29.9      0.6
  (venus recv -> vkr submit (render server pickup + decode))  400  150.0  150.0  150.0  3.4
  (vkr submit -> vkr submit done (vkQueueSubmit call))    400      10.0      10.0      10.0      0.2
  (gpu copy (timestamps))                                 400     200.0     200.0     200.0      4.5
  (vkr submit done -> fence done, minus gpu (queue/fence wake))  400  260.1  260.1  278.4  5.9
total (first -> last)                                     400    1460.6    1460.5    1492.4     33.2
unattributed                                                     2939.4                         66.8
```

(Numbers from a synthetic collection.) Each plain row is the interval between
two consecutive stages of one frame, in the order above; a stage that was not
stamped for a frame merges its two intervals into one row (`kmd submit ->
backend decoded`). The plain rows add up to `total`, the first stamp to the
last. Rows in brackets break a stretch down and are not part of the sum: the
render server's pickup and decode of the stream, the `vkQueueSubmit` call, the
GPU's own time for the copy and what is left of the submit-to-fence-seen span
once that is taken away (queueing behind other work, and the sync thread's
wake-up). `% frame` is the mean against the frame interval (the mean time
between consecutive frames' first stamps); `unattributed` is the frame
interval minus `total`: the part of each frame interval the pipeline does not
cover (the application's own CPU time, waits in dxgkrnl before the DDI).
Percentiles are nearest-rank. An interval that came out negative (possible
only across the guest/host boundary, within the clock error) counts as 0 and
is reported as "out of order".

`frames: joined` have stamps from both sides; `host only` have none from the
guest (without guest data every ring-1 fence counts as a copy, which then
includes other contexts' ring-1 work; with it, only the ids the guest driver
stamped as copies are taken); `guest only` were never seen by the host in the
window. `lost` counts stamps a ring
overwrote before they were read (host: 65536 stamps; guest: 8192, about 3 s at
240 frames per second, read every second).

### Clock correlation

The guest's stamps are interrupt time (100 ns), the host's
`CLOCK_MONOTONIC`. The offset comes from the frames themselves: the host
cannot decode a descriptor before the guest stamped `kmd submit` (taken
before the descriptor is visible), and the guest cannot find a completion
before the host put it on the used ring. So for every frame

```text
offset <= backend decoded - kmd submit        offset >= backend used - kmd done
```

(and the same with the flip's stages). The tightest of each over a window is
an interval the true offset lies in; the estimate is its middle and the error
bound half its width, typically the shortest guest-to-host and host-to-guest
hop seen (tens of microseconds). It is computed per second, interpolated
between seconds, so a drift between the two clocks (printed in ppm) is
followed; a window without both kinds of bound borrows from its neighbours.
No extra message, no protocol field, no kvmclock reading: any collection with
guest stamps carries its own correlation. Without guest data the table is
host stages only.

### Perfetto

`--perfetto FILE` writes the same joined frames as Chrome trace-event JSON
(open it in ui.perfetto.dev or chrome://tracing): one track per side (guest
KMD, host backend, conduit-venus/virglrenderer, host GPU, interrupt delivery,
and dxgkrnl when an ETW export is given), one slice per stage per frame, and
flow arrows linking each frame's slices across the tracks. The GPU slice has
the measured duration but is placed to end at `vkr fence done`, which is an
upper bound for its end (the GPU clock is not correlated).

`--etw FILE` adds dxgkrnl's view from a DxgKrnl ETW capture, converted to CSV
with the header `ts_100ns,event,pid,tid[,detail]` in guest interrupt time:
events 41/42 (`VIDMM_BEGINCPUACCESS_WAIT`) and 178/180 (the Blt packet) are
paired into slices, anything else is an instant. Raw ETL files are not read.

### Stage dump format

`stages dump` and the renderer's `STAGES` reply: a 24-byte header, then
32-byte records, little-endian (`host/venus/src/stage.rs`, shared by the
backend, `conduit-venus` and the CLI).

```text
header:  0 magic "CDTSTGH1"   8 records u32   12 record length u32 (32)   16 records lost u64
record:  0 ts_ns u64 (CLOCK_MONOTONIC)   8 id u64 (fence id or flip seq)   16 ctx_id u32
        20 ring u8   21 stage u8   22 kind u8 (1 copy, 2 flip)   23 0   24 aux u64 (gpu: duration ns)
```

The guest driver's `StgRing` layout is in the same file (`stage::guest`).
`--save DIR` keeps `host.bin` (one dump) and `guest-NNN.bin` (each snapshot as
read).

### Overhead

Off (the default), each stamp site is one relaxed atomic load: no clock read,
no store, and virglrenderer runs its unmodified path (the hook pointer is
null). On, a stamp is two vDSO clock reads at most and five atomic stores into
a lock-free ring; vkr adds two prerecorded command buffers (a timestamp each)
to the last submit before each ring fence and reads two query results when
it retires; the backend collects the renderer's stamps over its socket every
50 ms; the guest driver writes a 196 KB registry value twice a second.

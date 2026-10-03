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

```sh
{ echo 'stream json'; sleep infinity; } | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/conduit/myvm/trace.sock
```

A reader that stops reading for a second is disconnected so it cannot stall
the others.

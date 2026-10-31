# nvtrace

nvtrace records what NVIDIA's user-mode drivers (D3D, Vulkan/OpenGL, CUDA,
NVAPI, NVML) send to the Windows kernel and get back: every D3DKMT thunk,
every escape with its private data, all I/O on NVIDIA device handles, the
registry keys and files the drivers read, and the GPU command buffers they
write into GPU-visible memory. The logs are input for re-implementing the
kernel-driver side.

Made by Ole Algoritme.

## What it captures

`nvtrace64.dll` / `nvtrace32.dll`, loaded into a process, inline-hooks:

| where | what | logged |
|---|---|---|
| win32u.dll | every `NtGdiDdDDI*` and `NtDxgk*` export (newer builds name SubmitPresentToHwQueue, CreateNativeFence, the WDDM 3.1 doorbell calls, EnumAdapters3 and others `NtDxgk*`), found by scanning the export table | every call |
| ntdll.dll | `NtCreateFile`, `NtOpenFile` | device opens whose device name looks NVIDIA/graphics (`nv`, `uvm`, `dxg`, `gpu`), files opened from an NVIDIA DLL or with an NVIDIA-related path (`NVTRACE_FILES`) |
| ntdll.dll | `NtDeviceIoControlFile`, `NtFsControlFile`, `NtReadFile`, `NtWriteFile`, `NtClose` | for tracked device handles: full input and output buffers. A handle not opened under nvtrace (opened before injection, duplicated, inherited) is named with `NtQueryObject` on its first IOCTL with a driver-style code (device type 0x22, 0x23 or >= 0x8000; never consoles, pipes, sockets) and tracked from then on |
| ntdll.dll | `NtOpenKey(Ex)`, `NtCreateKey`, `NtQueryValueKey`, `NtEnumerateValueKey`, `NtEnumerateKey`, `NtQueryKey`, `NtSetValueKey`, `NtDeleteValueKey`, `NtNotifyChangeKey` | keys whose full name (from `NtQueryKey`, so relative opens and CurrentControlSet are resolved) matches `NVTRACE_REG`: the display class key `{4d36e968-...}`, `Services\nvlddmkm`, anything NVIDIA, GraphicsDrivers, Direct3D/DirectX, PCI `VEN_10DE`, Vulkan, DWM |
| ntdll.dll | `NtMapViewOfSection(Ex)`, `NtUnmapViewOfSection(Ex)` | every call (`NVTRACE_MAPS=0` turns it off) |
| ntdll.dll | `NtTerminateProcess` | self-termination writes the END record |
| kernelbase.dll | `CreateProcessInternalW` | with `-f`: children start suspended and get nvtrace too |

Module loads and unloads come from `LdrRegisterDllNotification`, with file
version and PE timestamp.

For each call the log has: QPC before and after, thread id, return value,
all argument slots, up to 8 return addresses, and copies of memory before
and after the call:

- The argument struct and every buffer it points to whose size is known:
  123 D3DKMT layouts (x64 and x86), checked field by field against the WDK
  10.0.26100 headers (`tools/check_layouts_wdk.py`, 1030 assertions per
  architecture). Present, PresentMultiPlaneOverlay 1/2/3 (plane arrays,
  pointer-to-pointer plane lists, attributes, dirty rects, private data),
  SubmitPresent(Blt)ToHwQueue (queue list `BroadcastContextCount + 1`),
  CreateHwContext, CreateDoorbell, native fences and the rest are decoded
  with their nested pointers.
- Functions without a layout: 4096 bytes behind the first argument
  (`NVTRACE_RAWSIZE`).
- Escape private data, IOCTL buffers and Present/plane private data are also
  scanned for user pointers; up to 16 per buffer, 16 KiB each, two levels
  deep, are copied (`NVTRACE_CHASE*`).
- Every buffer is copied completely. The only caps are safety caps: 64 MiB
  per buffer and 256 MiB per call record on x64 (32/32 MiB on x86;
  `NVTRACE_MAXBUF`, `NVTRACE_MAXREC`); a capped blob is flagged truncated.
- `Render` (legacy path): the command buffer, allocation and patch lists.

Extra records:

- **GPUMEM**: GPU command memory with its GPU VA (next section).
- **REGION**: every CPU address a call hands out, with VirtualQuery's view
  (protection incl. WRITECOMBINE/NOCACHE, mapped/private, extent): Lock and
  Lock2 `pData`, `pSystemMem`, doorbell, secondary doorbell, doorbell status
  and last-queued-fence CPU VAs, monitored/native/HW-queue/paging fence CPU
  VAs, and pointers in escape or IOCTL output that point at mapped or
  uncached memory (how a doorbell or USERD page reaches the UMD).
- **IOCOMPLETE**: an IOCTL or read that returned `STATUS_PENDING`; its
  IO_STATUS_BLOCK is polled and the output buffer copied when it completes.
- **HANDLE**: device handles found by probing and full registry key names.

Traced memory is read with `NtReadVirtualMemory` on the own process, so a
bad pointer gives a short copy, never an exception. nvtrace never writes to
the program's memory.

### GPU command capture

What the UMD writes for the GPU without a syscall is captured like this:

1. **GPU VA to CPU VA map.** GPU VA ranges come from MapGpuVirtualAddress,
   UpdateGpuVirtualAddress (MAP/UNMAP), CreateAllocation2/OpenResource2
   `GpuVirtualAddress` and FreeGpuVirtualAddress; CPU mappings from Lock,
   Lock2 and CreateAllocation `pSystemMem`; Unlock and DestroyAllocation drop
   them.
2. **At each submission syscall** (SubmitCommand, SubmitCommandToHwQueue)
   the command buffer named by GPU VA and length is copied through the map
   (GPUMEM `submit`). If the bytes are GPFIFO entries (Volta+ format: most
   entries name mapped memory), every entry's pushbuffer segment is copied
   too (GPUMEM `segment`).
3. **Page sampler.** The allocation that held a submitted command buffer,
   WDDM 3.1 doorbell ring and ring-control allocations, `NVTRACE_WATCH` GPU
   VAs and, with `NVTRACE_SNAP=2`, every mapped allocation are watched. Their
   pages are hashed; a page whose hash changed is written (GPUMEM `sample`).
   Passes run every 2 ms on nvtrace's thread (200 us budget), briefly at
   fence/present sync points, and right after each submission (from the
   submitted page onwards through the allocation) and each
   `NotifyWorkSubmission` doorbell call, within 500 us
   (`NVTRACE_SNAP_SUBMIT_US`). Pages are checked with VirtualQuery before
   each read; guard, no-access and uncached pages are never read.
   This catches user-mode submission (the UMD writes GPFIFO entries and
   GP_PUT, then rings a doorbell from user mode).

No guard pages, debug registers or exception handlers: nothing changes how
the program runs. Limits:

- Writes made and consumed by the GPU between two samples are missed when
  they are not followed by a submission syscall.
- Uncached mappings (`PAGE_NOCACHE`: doorbell and USERD registers) are
  never read; the doorbell write itself is not seen, only its effect on the
  watched rings. The REGION records say where those pages are.
- GPU VAs that NVIDIA assigns through escapes (RM `MAP_MEMORY_DMA` inside
  `NV**` escapes, CUDA) are not in the D3DKMT VA map. Decode them from the
  escapes, then rerun with `NVTRACE_WATCH=0xVA:size` once the matching
  allocation is CPU-mapped.
- The pass after each submission costs up to 500 us of the submitting
  thread; `NVTRACE_SNAP_SUBMIT=0` turns it off, a larger
  `NVTRACE_SNAP_SUBMIT_US` covers more of a large ring per submission.

## Running it

Put `nvinject.exe`, `nvinject32.exe`, `nvtrace64.dll` and `nvtrace32.dll` in
one directory (say `C:\nvtrace`). 32-bit targets are handed to
`nvinject32.exe` automatically.

```
nvinject -d C:\nvtrace\logs vktest.exe                 start a program traced
nvinject -d C:\nvtrace\logs -t 120 d3d12test.exe       stop it after 120 s
nvinject -d C:\nvtrace\logs -f launcher.exe            follow child processes
nvinject -d C:\nvtrace\logs -c SNAP=2 app.exe          any NVTRACE_* setting
nvinject -d C:\nvtrace\logs -p 1234[,5678]             load into running processes
nvinject -d C:\nvtrace\logs --dwm                      load into every dwm.exe (admin)
nvinject -d C:\nvtrace\logs --watch app.exe,other.exe  inject when these start (admin)
nvinject -d C:\nvtrace\logs --watch --all              every new process, lazy mode
nvinject -d C:\nvtrace\logs --watch --dwm              DWM now and after each restart
nvinject -d C:\nvtrace\logs --install-service --watch --all   same as a LocalSystem service
nvinject --uninstall-service
nvinject -d C:\nvtrace\logs --stop [pid]               close logs; hooks stay, inert
nvinject -d C:\nvtrace\logs --stop-watch               stop a running watcher
nvinject --info 1234                                   arch, session, mitigations, DLLs
nvinject -d C:\nvtrace\logs --selftest                 check this machine
```

The log is `<dir>\<exe>-<pid>.ntr`. Records are written into a mapped view
of the file, so they survive the process being killed or crashing; `-t`
first asks the process to close its log, then kills it.

**Running processes** (`-p`, `--dwm`, `--watch`): nvinject enables
SeDebugPrivilege; when it creates the log directory itself it gives it write
access for every account (DWM's `DWM-n` virtual account, services,
AppContainers, low integrity; an existing directory keeps its ACL, so use a
new `-d` directory), gives the DLLs read access, writes `<dll dir>\nvtrace-<pid>.cfg` with the settings
(the target's environment is not ours), loads the DLL with
`CreateRemoteThread(LoadLibraryW)` and deletes the file again. Hooks are
installed with every other thread of the process suspended; a thread
stopped inside a prologue about to be patched is first moved to the same
instruction in the trampoline, and if that fails the function stays
unhooked (hook error 7). Processes that only load Microsoft-signed DLLs or forbid dynamic
code are refused with a message (`--info` shows why); protected processes
cannot be opened.

**DWM**: `--dwm` traces the running DWM (its startup traffic is already
past). To capture DWM's startup, run `nvinject --watch --dwm` and restart DWM
(`taskkill /f /im dwm.exe`; winlogon starts a new one, the screen flickers).
If an injected process dies with a crash status twice within two minutes of
injection, the watcher stops injecting that program name.

**Watch mode** polls the process list every 20 ms (`--interval`). With
`--all` it skips system processes, security software, service hosts and
games and launchers with anti-cheat (cs2.exe, steam.exe, EasyAntiCheat,
BattlEye, Vanguard, FACEIT, ...): injecting into those can get an account
banned. Name a program explicitly to trace it anyway (csrss, lsass,
winlogon, smss, LogonUI, consent, userinit, WUDFHost, WerFault and similar
are never touched). The service writes its messages to
`nvinject-watch.log` next to nvinject.exe. `--all` uses lazy mode: the
DLL only watches DLL loads and starts tracing (log, hooks, thread) when an
NVIDIA driver DLL (`nvldumd`, `nvwgf2um`, `nvoglv`, `nvcuda`, `nvapi`, `nvml`,
...) is being loaded, before that DLL's DllMain runs. Calls the runtime made
before that (adapter enumeration, `QueryAdapterInfo(UMDRIVERNAME)`) are not
in such logs; add `-c TRIGGER=d3d11,dxgi,vulkan-1,...` to start earlier.

### Settings

Environment variables `NVTRACE_<NAME>` (launch mode), or `-c NAME=value`,
or lines in `<dll dir>\nvtrace.cfg`.

| setting | default | meaning |
|---|---|---|
| `DIR` | `C:\nvtrace` | log directory (`-d`) |
| `MODE` | `full` | `lazy`: start when a `TRIGGER` module loads |
| `TRIGGER` | NVIDIA UMDs | module name prefixes for lazy mode |
| `MAXBUF` | 64 MiB (x86 32) | safety cap per copied buffer |
| `MAXREC` | 256 MiB (x86 32) | safety cap per call record |
| `RAWSIZE` | 4096 | bytes behind arg0 of functions without a layout |
| `CHASE` / `CHASE_DEPTH` / `CHASE_MAX` | 16384 / 2 / 16 | pointer chasing; `CHASE=0` off |
| `CMDBUF` | 64 MiB | Render command buffer copy cap |
| `PUSHBUF` | 64 MiB (x86 16) | bytes copied per submission (command buffer + segments); 0 off |
| `GPFIFO` | 1 | follow GPFIFO-shaped command buffers |
| `SNAP` | 1 | sampler: 0 off, 1 learned regions, 2 every mapped allocation |
| `SNAP_MS` / `SNAP_US` | 2 / 200 | sampler period / time budget per period |
| `SNAP_REGION` | 64 MiB | bytes watched per region |
| `SNAP_SUBMIT` / `SNAP_SUBMIT_US` | 1 / 500 | pass over the command allocation after each submission / its budget |
| `WATCH` | | `0xGPUVA[:size],...` to sample |
| `REG` | 1 | registry: 0 off, `all`, or comma-separated lower-case name parts |
| `FILES` | `nv` | `0` devices only, `nv` NVIDIA-related files too, `all` every open |
| `DEVICES` | `nv` | `all`: track every device handle |
| `PROBE` | 1 | name untracked handles on first IOCTL |
| `IOPENDING` | 1 | record late IOCTL completions |
| `REGIONS` | 1 | REGION records |
| `MAPS` | 1 | section map logging |
| `STACK` | 8 | return addresses per call (max 48) |
| `MAXLOG` | 16 GiB | log size cap |
| `EXCLUDE` | | function names not to hook |
| `NTDLL` | 1 | 0: no ntdll hooks |
| `FOLLOW` | 0 | follow child processes (`-f`) |

## Test programs

All x64, print step lines and end with `PASS` or `FAIL: <reason>`, never
wait longer than a few seconds per step, run without nvtrace too.

| program | does |
|---|---|
| `vktest.exe [--async-compute]` | Vulkan 1.2: buffers, compute dispatch, timeline semaphore wait |
| `d3d11test.exe [--no-present]` | D3D11: clear, copy, map, compute, flip-model swapchain |
| `d3d12test.exe [--no-present] [--debug]` | D3D12: direct and copy queues, fences, readback, compute, presents |
| `cudatest.exe` | CUDA driver API: context, memory, vector add from PTX |

## Decoder

`ntrdump.py` (Python 3, standard library; needs `nvt_layouts.py`,
`rmnames.json` and `nvclasses.json` next to it):

```
ntrdump.py LOG                  summary: process, NVIDIA modules, hooks, calls per function and
                                caller, QueryAdapterInfo types, device opens, GPU memory records,
                                CPU mappings handed out, probed handles
ntrdump.py LOG --calls [-v] [--grep Escape]
ntrdump.py LOG --escapes [-v]   NVDA escapes grouped by tag/function, RM names, in/out diff
ntrdump.py LOG --rm [-v]        NVDA escapes and device IOCTLs in order with RM names
ntrdump.py LOG --objects        lifetimes: adapters, devices, contexts, HW contexts, HW queues,
                                paging queues, sync objects, doorbells, resources, allocations
                                (with CPU mappings and GPU VAs), keyed mutexes; GPU VA operations;
                                CPU mappings handed out; device handles; sections whose handle
                                came back from an escape; late IOCTL completions
ntrdump.py LOG --timeline [-v]  per HW queue / context / doorbell: submits (VA, length, fence,
                                captured bytes), GPU waits and signals with fence values,
                                presents, doorbell notifications; CPU waits; sampler activity
ntrdump.py LOG --pushbuf [-v] [--seq N,...] [--subch 0=ce97,...]
                                GPFIFO entries and NVIDIA method streams with class and method
                                names and field values; method histogram
ntrdump.py LOG --pushbuf --sample SEQ   one sampled page as GPFIFO entries and methods
ntrdump.py LOG --registry       NVIDIA/display registry reads and writes with decoded values
ntrdump.py LOG --files          device and file opens with the calling module
ntrdump.py --diff A.ntr B.ntr
```

Class and method names come from `nvclasses.json`, built from
open-gpu-kernel-modules 610.57.04 class headers, plus NVIDIA's MIT-licensed
3D/compute/copy class headers that Mesa ships, since that tag has only the
class numbers of BLACKWELL_A/B 3D and compute:

```
tools/gen_nvclasses.py --repo /steam/refsrc/open-gpu-kernel-modules --tag 610.57.04 \
    --mesa /steam/refsrc/mesa > tools/nvclasses.json
tools/gen_rmnames.py --repo /steam/refsrc/open-gpu-kernel-modules --tag 610.57.04 > tools/rmnames.json
```

Subchannels are bound by the SET_OBJECT methods in the stream; before the
first one, GB202 defaults apply (0 BLACKWELL_B, 1 BLACKWELL_COMPUTE_B,
4 BLACKWELL_DMA_COPY_B; `--subch` overrides). Host methods (below 0x100)
are named from the GPFIFO classes.

## Log format (version 2)

Little-endian. Definitions in `src/ntr_format.h`; version 1 logs still
decode.

File header (32 bytes): `"NVTRACE\0"`, u32 version (2), u32 header size
(32), u32 pointer size (4/8), u32 pid, u64 QPC frequency.

Records start with a 24-byte header: u32 type, u32 size (whole record,
multiple of 8), u64 QPC, u32 thread id, u32 sequence number. Readers skip
unknown types. Type 0 (PAD) may be only 8 bytes (type, size) and is skipped.
A zero size ends the data: a log of a killed process keeps a zero-filled
tail after the last record.

| type | record | body |
|---|---|---|
| 0 | PAD | skipped |
| 1 | PROCINFO | `key=value\0` strings, then `\0`: os, os_ubr, ptr_size, pid, ppid, session, wow64, exe, cmdline, cwd, nvtrace, dll, start_reason, qpc_freq, start_filetime, start_qpc, config.* |
| 2 | STRING | u32 id, text (blob labels) |
| 3 | HOOK | u32 id, u32 flags (1 installed, 2 layout, 4 raw, 8 syscall stub), u32 syscall nr, u32 arg slots, u64 address, u32 arg0 size, u32 error (1 not found, 2 not relocatable, 3 no memory, 4 protect failed, 5 excluded, 6 changed meanwhile, 7 a thread sat in the prologue), module, function |
| 4 | MODULE | u64 base, u64 size, u16 version[4], u32 PE timestamp, u32 flags (1 initial, 2 loaded, 4 at exit), path |
| 5 | MODULE_UNLOAD | u64 base, path |
| 6 | CALL | u32 hook id, u32 flags (1 raw, 2 truncated, 4 fault), u64 QPC after, u64 return value, u16 nargs, u16 nframes, u32 depth; u64 args[]; u64 frames[]; blobs |
| 7 | NOTE | u32 level, u32 0, text |
| 8 | END | u64 records, u64 calls, u64 dropped, u64 bytes |
| 9 | GPUMEM | u32 kind (1 submit, 2 segment, 3 sample, 4 watch), u32 flags (1 not CPU-mapped, 2 truncated, 4 short read, 8 first copy, 16 sync point), u64 GPU VA, u64 CPU VA, u32 requested, u32 stored, u32 hAllocation, u32 CALL seq, u32 queue/context (sample: watch id), u32 index (GPFIFO entry / page), u64 fence, u64 aux (raw GPFIFO entry / page hash); data |
| 10 | REGION | u64 addr, u64 base, u64 size, u64 allocation base, u32 protect, u32 type, u32 source (1 Lock, 2 Lock2, 3 pSystemMem, 4 escape, 5 IOCTL, 6 doorbell, 7 doorbell2, 8 doorbell status, 9 queued fence, 10 fence, 11 HW queue fence, 12 paging fence), u32 CALL seq, u32 handle, u32 offset |
| 11 | IOCOMPLETE | u32 CALL seq, u32 status, u64 information, u64 handle, u64 buffer, u32 length, u32 stored, u32 flags (1 timeout, 2 completed before first look, 4 at exit), u32 IOCTL code; data |
| 12 | HANDLE | u64 handle, u32 how (2 device found by probing, 3 registry key), u32 0, name |

Blob (inside CALL): u32 label, u32 index (0xFFFFFFFF none; chased: offset in
parent), u64 source address, u32 requested, u32 stored, u8 phase (0 before,
1 after), u8 flags (1 truncated, 2 faulted, 4 same as before, 8 chased),
u16 parent (1-based), u32 0, data padded to 8. Labels are field paths such
as `pData.pAllocationInfo[].pPrivateDriverData` or
`pData.ppPresentPlanes[].p.pPlaneAttributes.pDirtyRects`.

## Building and host tests

```
make             dist/: DLLs, injectors, test programs, decoder and its tables
make test        x86/x64 relocation (run natively), decoder on synthetic v1/v2 logs,
                 struct layouts against the WDK headers (WDK_INC, skipped if absent)
make wine-test   nvinject.exe/nvtrace64.dll under Wine 64-bit, no display: selftest,
                 -t with stop file, external kill, -p, --info, --stop, --watch
```

Struct layouts live in `tools/nvt_layouts.py`; `tools/gen_layouts.py`
generates `src/layouts_gen.h`.

## Test plan on Windows

Run from an elevated prompt in `C:\nvtrace` (the dist files copied there).
Do not point `--watch --all` or `-p` at games with anti-cheat.

1. `nvinject -d C:\nvtrace\st --selftest` (runs the 32-bit selftest through
   nvinject32 too). Every line must be `ok`; the output ends with
   `SELFTEST PASS (x86)` and `SELFTEST PASS (x64)`. It
   injects into a busy 8-thread child while it runs, checks that every
   installed patch points at its stub, that win32u, gdi32 D3DKMT, device,
   registry and file calls are logged, and live-patches three
   syscall-shaped functions (8-byte, 16-byte and frozen-thread write paths)
   while 4 threads call them (0 wrong results required).
2. `nvinject -d C:\nvtrace\l1 vktest.exe`, `... d3d11test.exe`,
   `... d3d12test.exe`, `... cudatest.exe`: each prints `PASS`. Then
   `python ntrdump.py C:\nvtrace\l1\<exe>-<pid>.ntr` shows nvoglv64/nvwgf2umx/
   nvcuda64 modules, `hooks ... 0 not installed` (or only functions the OS
   lacks), END present; `--escapes` shows `NVDA tag=NV**` groups; `--objects`
   shows allocations with `cpu=` and `gpu=`; `--timeline` shows submits with
   `[cmdbuf N bytes ...]`; `--pushbuf` shows method names (`BLACKWELL_B.*`,
   `BLACKWELL_DMA_COPY_B.*` or GPFIFO entries); `--registry` shows reads under
   `{4d36e968-...}\000N`; `--files` shows DriverStore/cache paths.
3. `nvinject -d C:\nvtrace\l2 -t 5 d3d12test.exe`: exit code 124, log has END.
4. Running process: start `d3d12test.exe` (or `nvinject --busy 30000`),
   `nvinject -d C:\nvtrace\l3 -p <pid>`, then `nvinject -d C:\nvtrace\l3 --stop <pid>`:
   the program keeps running; the log has `probe` handles (NvAdminDevice
   opened before injection) and a `stop file found` note.
5. DWM: `nvinject --info <dwm pid>` must say `can be traced`; then
   `nvinject -d C:\nvtrace\dwm --dwm`, move a window for 10 s,
   `nvinject -d C:\nvtrace\dwm --stop`. DWM must keep running (the desktop
   keeps updating). `ntrdump.py --timeline` on `dwm-<pid>.ntr` shows
   presents/submits. If DWM restarts, look for the crash status in the
   event log and in nvinject's output.
6. DWM startup: `nvinject -d C:\nvtrace\dwm2 --watch --dwm` in one prompt,
   `taskkill /f /im dwm.exe` in another; the watcher prints `+ dwm.exe`;
   then `nvinject -d C:\nvtrace\dwm2 --stop-watch`.
7. Kill survival: `nvinject -d C:\nvtrace\l4 vktest.exe` and kill it from
   Task Manager mid-run: `ntrdump.py` reads the log and says `log not closed`.

## Known gaps

- On Windows only reviewed and run under Wine (x64): hooks, live injection
  under load, kill survival, -p, --stop, --watch and registry/file capture
  pass there. WOW64 (32-bit) has not run anywhere yet.
- Doorbell writes are not trapped (see "GPU command capture").
- Lazy mode misses the runtime's calls before the first trigger module.
- At process exit, after ExitProcess killed the other threads, only D3DKMT
  calls (the drivers' DllMain teardown) are still logged.
- With `-f`, a same-architecture child loads nvtrace through a user APC on
  its first thread (runs after the parent resumed it, before its entry
  point); a parent that never resumes the child leaves it untraced.
- Pending-IOCTL completion is polled (2 ms); a buffer freed and reused right
  after completion can be copied with new content.
- DLLs that call `syscall` directly, or D3DKMT through another process (DWM
  composition on behalf of an app), appear only in that process's log.
- Processes with ACG or Microsoft-only signature policy are not traced.

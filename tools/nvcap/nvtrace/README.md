# nvtrace

nvtrace records what NVIDIA's user-mode drivers (D3D, Vulkan/OpenGL, CUDA,
NVAPI) send to the kernel on Windows: every D3DKMT thunk, every escape, the
private driver data, and I/O on NVIDIA device handles. The logs are input for
re-implementing the kernel-driver side.

Made by Ole Algoritme.

## What it captures

`nvtrace64.dll` / `nvtrace32.dll`, loaded into a process, inline-hooks:

| where | what | logged |
|---|---|---|
| win32u.dll | every `NtGdiDdDDI*` export, found by scanning the export table at run time | every call |
| ntdll.dll | `NtCreateFile`, `NtOpenFile` | opens of device names that contain `nv`, `uvm`, `dxg`, `gpu` (all device opens with `NVTRACE_DEVICES=all`, every open with `NVTRACE_FILES=1`), failed ones too |
| ntdll.dll | `NtDeviceIoControlFile`, `NtFsControlFile`, `NtClose` | only for the device handles tracked above (e.g. `\\.\NvAdminDevice`, `\\.\UVMLiteController`) |
| ntdll.dll | `NtMapViewOfSection(Ex)`, `NtUnmapViewOfSection(Ex)` | every call (`NVTRACE_MAPS=0` turns it off) |
| ntdll.dll | `NtTerminateProcess` | not logged; flushes the log before the process dies |
| kernelbase.dll | `CreateProcessInternalW` | with `-f`: children are started suspended and get nvtrace too |

Module loads and unloads come from `LdrRegisterDllNotification`, with file
version and PE timestamp of every module, so the log shows when and which
`nv*.dll` loaded.

For each call the log has: QueryPerformanceCounter before and after, thread
id, function, return value, all argument slots, up to 8 return addresses
(`NVTRACE_STACK`), and copies of memory before and after the call:

- The argument struct, and every buffer it points to whose size is known.
  118 D3DKMT functions have a layout (x64 and x86), 9 of them only a raw size. Examples: Escape and
  QueryAdapterInfo private data; CreateAllocation(2): the runtime data, the
  resource private data, the allocation info array and each allocation's
  private data; CreateContext(Virtual), CreateHwQueue, CreatePagingQueue,
  SubmitCommand(ToHwQueue) private data; MakeResident/Evict lists;
  Reserve/Map/Update GPU VA (with the operations array); monitored fence CPU
  values; WaitFor/Signal sync objects with handle and fence arrays; OpenResource
  data; EnumAdapters2/3 arrays.
- `Render` (the legacy path CUDA can use): the command buffer bytes from the
  buffer CreateContext/CreateDevice returned, plus the allocation and patch
  lists.
- Functions without a layout (new ones, rare ones): the first 512 bytes
  behind the first argument (`NVTRACE_RAWSIZE`).
- Escape data and IOCTL buffers are scanned for values that look like user
  pointers into readable memory (not uncached or write-combined mappings); up
  to 16 of them per buffer, 4 KiB each, two levels deep, are copied too
  (`NVTRACE_CHASE*`). This catches parameter blocks passed by pointer.

An after-copy identical to the before-copy is stored as a flag only. Buffers
are capped at 64 MiB each (`NVTRACE_MAXBUF`). Traced memory is read with
`NtReadVirtualMemory` on the own process, so a bad pointer gives a short or
empty blob, never an exception. Hooks are a 5-byte `jmp rel32` (one atomic
8-byte store where the target is 8-aligned) to a stub within +-2 GiB;
functions that already start with a jump (hooked by another tool) are left
alone and reported.

## Running it

Put `nvinject.exe`, `nvinject32.exe`, `nvtrace64.dll` and `nvtrace32.dll` in
one directory. Then:

```
nvinject -d C:\nvtrace vktest.exe
nvinject -d C:\nvtrace -t 120 d3d12test.exe --no-present
nvinject -d C:\nvtrace -f launcher.exe        (follow child processes)
```

`nvinject [-d dir] [-f] [-t seconds] <exe> [args...]` starts the program
suspended, loads the matching DLL with `CreateRemoteThread(LoadLibraryW)`
before the main thread runs, resumes it, waits and returns its exit code
(124 after a `-t` timeout, 2 if starting or injecting failed). A 32-bit
program is handed to `nvinject32.exe` automatically. `nvinject -p <pid>`
loads nvtrace into a running process.

The log is `<dir>\<exe>-<pid>.ntr` (default dir `C:\nvtrace`, or
`NVTRACE_DIR`).

Capturing DWM or other system processes is out of scope for now: it needs
injection into a protected session process and is not attempted.

### Settings (environment variables, read by the DLL)

| variable | default | meaning |
|---|---|---|
| `NVTRACE_DIR` | `C:\nvtrace` | log directory (`-d` sets it) |
| `NVTRACE_RAWSIZE` | 512 | bytes copied behind arg0 of functions without a layout |
| `NVTRACE_MAXBUF` | 64 MiB | cap per copied buffer |
| `NVTRACE_STACK` | 8 | return addresses per call (max 48, 1 = caller only) |
| `NVTRACE_DEVICES` | `nv` | `all`: track every device handle opened |
| `NVTRACE_FILES` | 0 | 1: log every NtCreateFile/NtOpenFile |
| `NVTRACE_MAPS` | 1 | 0: do not log section maps |
| `NVTRACE_CHASE` | 4096 | bytes per chased pointer; 0 turns chasing off |
| `NVTRACE_CHASE_DEPTH` / `_MAX` | 2 / 16 | chase levels / pointers per buffer |
| `NVTRACE_CMDBUF` | 4 MiB | cap for Render command buffer copies |
| `NVTRACE_EXCLUDE` | | comma-separated function names not to hook |
| `NVTRACE_NTDLL` | 1 | 0: no ntdll hooks |
| `NVTRACE_FOLLOW` | 0 | 1: follow child processes (`-f` sets it) |

## Test programs

All x64, all print step lines and end with `PASS` or `FAIL: <reason>`, exit
non-zero on failure, never wait longer than a few seconds per step, and run
without nvtrace too.

| program | does |
|---|---|
| `vktest.exe [--async-compute]` | Vulkan 1.2 on the NVIDIA GPU: staging, device-local and readback buffers, compute dispatch from embedded SPIR-V, timeline semaphore host wait |
| `d3d11test.exe [--no-present]` | D3D11 on the NVIDIA adapter: clear + copy + map readback, a compute shader compiled at run time with d3dcompiler_47, flip-model swapchain, windowed/fullscreen toggle |
| `d3d12test.exe [--no-present] [--debug]` | D3D12: direct and copy queues, fences, clear, CopyTextureRegion readback, compute via root UAV, flip-model presents |
| `cudatest.exe` | CUDA driver API from nvcuda.dll: context, memory, vector add from hand-written PTX (driver JIT), verify |

## Decoder

`ntrdump.py` (Python 3, standard library only; needs `nvt_layouts.py` and
`rmnames.json` next to it):

```
ntrdump.py vktest-1234.ntr                 summary: process, OS, nv*.dll versions/bases,
                                           hooks, calls per function and per caller module,
                                           QueryAdapterInfo types, device opens
ntrdump.py LOG --calls [-v] [--grep Escape] every call with decoded struct fields; -v adds
                                           frames and every blob (structs decoded, else hex)
ntrdump.py LOG --escapes [-v]              escapes grouped by NVDA header tag/function/version:
                                           sizes, callers, RM status, RM names, in/out diff
ntrdump.py LOG --rm [-v]                   NVDA escapes and device IOCTLs in order with RM
                                           control/class/escape/status names
ntrdump.py LOG --objects                   lifetimes of adapters, devices, contexts, HW queues,
                                           paging queues, sync objects, resources, allocations,
                                           GPU VA operations
ntrdump.py --diff A.ntr B.ntr              call counts, escape groups, module versions, hooks
```

The "caller" of a call is the first return address outside win32u, gdi32,
gdi32full, kernelbase, kernel32 and ntdll, so a D3D runtime callback shows as
`d3d11.dll`/`d3d12core.dll` and a direct driver call as `nvwgf2umx.dll`,
`nvoglv64.dll`, `nvcuda64.dll`, ...

NVIDIA escapes start with the `NVDA` header (`0x4E564441`, version, size,
4-character tag such as `NV**`, function). The function numbers match
`NV_ESC_RM_*` from the Linux driver (`0x2A` RM_CONTROL, `0x2B` RM_ALLOC, ...);
the decoder names them and the RM control commands, classes and NV_STATUS
values it finds. Names come from `rmnames.json`, built from NVIDIA's
open-gpu-kernel-modules at tag 610.57.04:

```
tools/gen_rmnames.py --repo /steam/refsrc/open-gpu-kernel-modules --tag 610.57.04 > tools/rmnames.json
```

## Log format (version 1)

Little-endian. Definitions in `src/ntr_format.h`.

File header (32 bytes): `"NVTRACE\0"`, u32 version (1), u32 header size
(32), u32 pointer size (4/8), u32 pid, u64 QPC frequency.

Then records. Each starts with a 24-byte header: u32 type, u32 size (whole
record, multiple of 8), u64 QPC, u32 thread id, u32 sequence number. Readers
skip unknown types and drop records whose sequence number repeats (a chunk
can be written twice when the process is killed during a write).

| type | record | body |
|---|---|---|
| 1 | PROCINFO | `key=value\0` strings, then `\0`. Keys: `os`, `os_ubr`, `ptr_size`, `pid`, `ppid`, `wow64`, `exe`, `cmdline`, `cwd`, `nvtrace`, `dll`, `qpc_freq`, `start_filetime`, `start_qpc`, `config.*` |
| 2 | STRING | u32 id, text. Ids name blob labels |
| 3 | HOOK | u32 hook id, u32 flags (1 installed, 2 known layout, 4 raw, 8 syscall stub), u32 syscall number, u32 argument slots, u64 address, u32 arg0 struct size, u32 error (1 not found, 2 not relocatable, 3 no memory, 4 protect failed, 5 excluded), module, function |
| 4 | MODULE | u64 base, u64 size, u16 version[4], u32 PE timestamp, u32 flags (1 present at start, 2 loaded, 4 present at exit), path |
| 5 | MODULE_UNLOAD | u64 base, path |
| 6 | CALL | see below |
| 7 | NOTE | u32 level, u32 0, text |
| 8 | END | u64 records, u64 calls, u64 dropped, u64 bytes written |

CALL body: u32 hook id, u32 flags (1 raw arg0, 2 a blob was truncated, 4 a
read faulted), u64 QPC after the call (the header QPC is just before it),
u64 return value, u16 nargs, u16 nframes, u32 nesting depth; u64
args[nargs]; u64 frames[nframes] (frames[0] = caller); then blobs to the end
of the record. Blob: u32 label (STRING id), u32 element index (0xFFFFFFFF if
none; for chased pointers the offset of the pointer in its parent), u64
source address, u32 requested size, u32 stored size, u8 phase (0 before, 1
after), u8 flags (1 truncated, 2 faulted, 4 same as before: no data, 8
chased), u16 parent blob (1-based, 0 = argument slots), u32 reserved, data
padded to 8.

Labels are paths: `pData` is what the first argument points to,
`pData.pPrivateDriverData` a buffer it points to, `pData.pAllocationInfo[].pPrivateDriverData`
the buffer of each array element (index = element), `InputBuffer` an
argument of an ntdll call, `chase` a pointer found by scanning,
`Render.CommandBuffer` the command buffer of a Render call.

Struct layouts live in one place, `tools/nvt_layouts.py`; `tools/gen_layouts.py`
generates `src/layouts_gen.h` from it and the decoder reads it directly.

## Building

On Linux with MinGW-w64 (`x86_64-w64-mingw32-gcc`, `i686-w64-mingw32-gcc`):

```
make            # dist/: DLLs, injectors, test programs, decoder
make test       # host tests: x86/x64 decoder and relocation (run natively), decoder on synthetic logs
```

Everything links statically against libgcc; the binaries import only system
DLLs.

## Known gaps

- Not run on Windows yet. The hook engine is tested on Linux (relocated x64
  code executed natively, the win32u/WOW64 stub patterns), the Windows parts
  are reviewed only.
- Injection runs `LoadLibraryW` on a remote thread of the suspended process,
  so that thread performs process initialisation (static imports' DllMain)
  instead of the main thread. The 32-bit (WOW64) path is untested.
- `nvinject -t` kills the program from outside: the last ~50 ms of records
  can be lost. A program that calls TerminateProcess on itself or exits
  normally is flushed.
- x86 struct layouts follow natural 32-bit packing and agree with Wine's
  WOW64 thunks where Wine has them; the x86 deep copies are less certain than
  x64.
- Present, PresentMultiPlaneOverlay*, SubmitPresent(Blt)ToHwQueue,
  CreateHwContext and a few others are raw dumps (0x200-0x400 bytes), without
  their nested pointers.
- What the GPU reads from user memory (pushbuffers written by the UMD into
  mapped allocations, doorbell writes) is not visible to a syscall tracer;
  only the Render path's command buffer is copied.
- IOCTLs that complete asynchronously (STATUS_PENDING) are logged at return,
  before their output is written.
- Handles duplicated with DuplicateHandle are not tracked as devices.
- Processes that forbid dynamic code (ACG) refuse the trampolines; nvtrace
  then logs hook errors and the program runs untraced. Calls made after nvtrace's own DLL detach at
  exit are not logged.

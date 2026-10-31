# nvddi

Kernel-mode tracer for the interface between the Windows display kernel
(`dxgkrnl.sys`) and a WDDM display miniport (`nvlddmkm.sys`, NVIDIA 610.74).
It records every call in both directions with its arguments and the buffers
they point to, to serve as the blueprint for re-implementing the miniport's
role in another KMD (conduit's Helios KMD).

Built on Linux with mingw-w64; installed and run in a Win11 VM with test
signing on. Nothing here touches a VM; the steps below are for the person who
runs it there.

## How it intercepts

A WDDM miniport's `DriverEntry` calls `displib!DxgkInitialize`, which:
loads the `DXGKrnl` service, opens `\Device\DxgKrnl` with
`IoGetDeviceObjectPointer`, sends `IRP_MJ_INTERNAL_DEVICE_CONTROL`
`0x230047` (Windows 8+, falls back to `0x23003F`) whose METHOD_NEITHER
output buffer receives dxgkrnl's `DpiInitialize`, then calls
`DpiInitialize(DriverObject, RegistryPath, &DRIVER_INITIALIZATION_DATA)`.
This was confirmed by disassembling displib (WDK 8 `displib.lib`) and the
DxgkInitialize copy inside the 610.74 `nvlddmkm.sys` (read only).

nvddi attaches an unnamed filter device on top of `\Device\DxgKrnl`
(`IoAttachDevice`), so that IOCTL passes through it. It forwards the IRP and
replaces the returned pointer with its own `DpiInitialize`, which hands
dxgkrnl a copy of the miniport's `DRIVER_INITIALIZATION_DATA` with every
non-NULL `DxgkDdi*` pointer replaced by a wrapper (only the miniport named in
`Target`, default `nvlddmkm`; others pass through and are only logged).
The `DXGKRNL_INTERFACE` passed to `DxgkDdiStartDevice` is replaced the same
way (all `DxgkCb*`), as are the VidPN, VidPN topology/source/target mode set,
monitor and monitor source/frequency/descriptor interfaces that dxgkrnl
returns through out-parameters. Only data is changed (function pointers in
copies handed out); no code is patched, nothing PatchGuard watches is touched.

Why not other options: a PnP upper/lower filter on the GPU stack sees PnP/power
IRPs, not DDIs; loading `nvlddmkm.sys` as a "proxy" means replacing its
service and image loader work; patching dxgkrnl is fragile and HVCI-hostile.

Registry reads/writes the miniport makes itself (`Zw*`, not DDIs) are seen
through a registry filter callback (`CmRegisterCallbackEx`); an operation is
kept when its kernel stack contains an address inside the hooked image.

### Generated from the WDK headers

`gen/gen.py` reads the WDK 26100 display headers (`dispmprt.h`, `d3dkmddi.h`,
`d3dkmdt.h`, `d3dukmdt.h`) and generates one wrapper per slot (310 slots in
10 tables) with the exact C signature, plus argument descriptors and capture
rules. Type facts come from compilers, not hand-copied tables: clang
(`x86_64-pc-windows-msvc`, unmodified headers) dumps record layouts and the
typedef AST; mingw gcc answers probe programs (`sizeof`,
`__builtin_classify_type`, `__builtin_types_compatible_p`) read back from the
object file. GCC makes enums with values outside `int` 8 bytes; the generator
copies the headers with an `(int)` cast on such enumerators, and
`gen/layoutcheck.py` compares every offset/size the driver uses between gcc
and clang/MSVC (776 expressions, 0 mismatches).

## What is recorded

Levels (`Level`, `nvddictl level`): 1 calls + argument values + return
values; 2 + every argument record/scalar the arguments point to, before the
call and (non-const) after it; 3 (default) + nested buffers.

- Every `DxgkDdi*` (181 slots incl. render, paging, GPU VA, HW queues,
  power, VidPN, MPO, ...) and every `DxgkCb*` (70 slots), plus VidPN/monitor
  interface functions: CALL record (thread, CPU, IRQL, QPC, pid/tid, args,
  argument structures) and RET record (return value, out-parameters, structures
  again). Calls made inside other calls are nested by thread in the decoder.
- Out-parameters: `PVOID*`, `ULONG*`, record-pointer-pointers (e.g. pinned
  VidPN modes), `INTERFACE` (full `Size` bytes, e.g. `DxgkCbQueryServices`).
- Nested buffers (`gen/rules.py`; automatic `pX`+`XSize`/`NumX` pairing for the
  rest): private driver data (CreateAllocation incl. each
  `DXGK_ALLOCATIONINFO`, OpenAllocation, Escape in and out, CreateContext,
  CreateDevice, CreateHwQueue, SubmitCommand*/HwQueue private data, ...),
  QueryAdapterInfo input and output plus the segment/MMU descriptor arrays,
  QueryInterface results, `DXGK_DEVICE_INFO` (registry path, translated
  resources = BARs), Render (command buffer, allocation/patch lists, DMA and
  patch locations written), Present, Patch (submitted DMA range),
  BuildPagingBuffer (DMA bytes written by the miniport for every operation,
  page table entries, copy ranges, context allocation data, MDL headers + PFNs,
  ADL PFNs), MPO planes, pointer shapes, VIDEO_REQUEST_PACKET buffers,
  `DxgkCbRead/WriteDeviceSpace`, ACPI buffers, power runtime buffers.
- `DxgkCbMapMemory` (physical address, length, cache type, returned VA) and
  every other device-space call.
- **Command buffers at submitted GPU VAs** (`ResolveGpuVa`, default on): nvddi
  reconstructs the GpuMmu mapping from the segment descriptors it saw in
  QueryAdapterInfo (QUERYSEGMENT) and the leaf page-table entries it saw in
  BuildPagingBuffer UPDATE_PAGE_TABLE, then at `SubmitCommandVirtual` /
  `SubmitCommandToHwQueue` resolves `DmaBufferVirtualAddress` to physical pages
  and copies the command bytes (MmCopyMemory physical for system/aperture
  segments, MmMapIoSpaceEx for CPU-visible local memory). Decoder: `--cmdbuf`.
  Unmapped pages become zero-length markers; nothing is invented.
- **Non-DDI kernel imports** (`HookImports`, **off by default**, needs HVCI
  off): the miniport's import address table is rewritten in memory so calls to
  MmMapIoSpace(Ex)/MmUnmapIoSpace, MmAllocateContiguousMemory*,
  MmAllocatePagesForMdlEx, IoAllocateMdl, MmGetPhysicalAddress,
  KeInsertQueueDpc, IoConnectInterruptEx, ExAllocatePool2/WithTag, ZwOpenKey
  and ZwQueryValueKey are logged with arguments and results. Decoder:
  `--imports`.
- Registration (driver object, image base/size, registry path, raw
  `DRIVER_INITIALIZATION_DATA`); each wrapped table with its raw original
  pointers (the decoder prints nvlddmkm's DDI entry points as image offsets).
- Every IRP that passes the `\Device\DxgKrnl` filter.
- Registry: key path, value name, type, data for query/enumerate/open/create/
  set/delete made from the hooked image's code.

Nothing is dropped silently: a buffer above `MaxBlobKB` is cut and flagged
`trunc` with the original length; a record that does not fit in its ring is
counted and the writer logs a DROP record; the file size cap writes a DROP
marker and the END record carries the count.

### Safety rules in the capture path

- Interrupt-level code (`DxgkDdiInterruptRoutine`, `DxgkCbNotifyInterrupt`, ...)
  only reserves space in a per-CPU ring with a compare-exchange; no locks, no
  allocation. Above DISPATCH_LEVEL only argument records in kernel memory
  (<= 1 KB) are copied, directly; nested rules are skipped (record flag
  `HIGHIRQL`).
- <= APC_LEVEL: all reads go through `MmCopyMemory` (never faults, refuses I/O
  space). DMA ranges the miniport itself just wrote may fall back to a direct
  read when every page is valid (write-combined aperture mappings).
- DISPATCH_LEVEL: kernel addresses whose pages are all valid.
- Nested pointers/lengths are read from the copy already in the ring, never
  twice from the live structure.
- Tables are only wrapped when every known slot matches what earlier copies had
  and the table version is one the headers cover; otherwise passed through and
  logged (`TABLE_SKIP`).

## GPU-VA resolution and import logging

`ResolveGpuVa` (default on) keeps a nonpaged GPU-VA → physical map
(`GpuVaEntries`, default 1M entries = 32 MB) fed only from logged DDI traffic
(QUERYSEGMENT descriptors, UPDATE_PAGE_TABLE leaf PTEs). It changes nothing in
the driver. Resolution runs inline at `SubmitCommandVirtual` (PASSIVE); at
`SubmitCommandToHwQueue` (<= DISPATCH) local-memory reads that need
MmMapIoSpaceEx are skipped with an `irql` marker (raise `status` after the run
to catch them, or it resolves when the buffer is in a system/aperture segment).

`HookImports` is the one technique that touches another image: it overwrites
pointers in nvlddmkm's IAT (data, not code) with wrappers in nvddi's image.
Implications, why it is off by default, and why it needs **Memory Integrity
(HVCI) off**:

- The IAT pages are made read-only by the loader. nvddi writes them by mapping
  their physical page through a fresh writable alias (MmMapIoSpaceEx). With HVCI
  on, the physical page is read-only in the hypervisor's second-level
  translation, so the write faults; nvddi detects the failed map and leaves the
  slot alone (logs `import IAT write refused`).
- The miniport has Kernel-CFG metadata (28k guarded call targets). Indirect
  calls through the IAT are kCFG-checked. The wrappers are valid function
  starts in nvddi's own image, which the loader publishes to the global kCFG
  bitmap, so the check passes — again only in the target configuration (test
  signing, HVCI off). With HVCI/kCFG enforced this is not safe and nvddi does
  not attempt it.
- No code bytes are patched anywhere; only IAT pointer slots change, and they
  are restored on unload.

## Log file (`.ndl`, version 1)

`<LogDir>\nvddi-YYYYMMDD-HHMMSS-b<boot>-<n>.ndl` (default `C:\nvddi`; falls
back to `C:\Windows\nvddi` if `LogDir` is unusable for 30 s). Written by a
PASSIVE_LEVEL thread every 25 ms, flushed to disk every 2 s. Definitions:
`include/ndl_format.h`.

- Header, 256 bytes: `"NVDDINDL"`, version, QPC frequency, start QPC/time,
  boot count, OS build, ring sizes, level, max blob, WDK DDI version.
- Records, each with a 48-byte `NDL_REC` (size, type, flags, seq, QPC,
  KTHREAD, pid, tid, CPU, IRQL). Records of different CPUs come in blocks; sort
  by `seq`.

| type | record | payload |
|---|---|---|
| 1 | SCHEMA | text: tables (T), slots (S), arguments (A), rule sets (RS), rules (R) |
| 2 | EVENT | code, status, a, b, text |
| 3 | IRP | major/minor, IOCTL, lengths, status, file object, user buffer |
| 4 | REGISTER | driver object, image base/size, init version, hooked, names, raw init data |
| 5 | TABLE | table id, slots wrapped, source, copy, size, version, raw original table |
| 6 | CALL | table, slot, nargs, nblobs, args[], blobs |
| 7 | RET | same + call_seq + return value |
| 8 | BLOB | owner seq (CALL or RET), one blob: nested buffer |
| 9 | DROP | records/bytes lost, CPU, reason (1 ring full, 2 file cap) |
| 10 | END | totals |
| 11 | REGISTRY | REG_NOTIFY_CLASS, status, type, info class, key, value, data |
| 12 | IMPORT | logged ntoskrnl/hal import call: fn, args a-d, return, optional name blob |

Blob kind 7 (CMDBUF) carries command bytes resolved from a submitted GPU VA
(`arg` = engine, `rule` = segment id, `elem` = run index, `addr` = GPU VA).

Blob header (32 bytes): len, orig_len, kind (arg, nested rule, ptrarray
element, length-buffer, table, init), argument index, rule id, flags (post,
trunc, partial, failed, irql, direct, user), element index, parent rule,
depth, source address.

## Build (Linux)

```
make                 # gen + build/nvddi.sys + build/nvddictl.exe (WDK headers: WDK=...)
make check           # layout cross-check + decoder self-test
sign/mkcert.sh       # once: sign/out/nvddi-test.{key,crt,cer} (key stays here)
make sign            # build/nvddi.signed.sys (osslsigncode; OSSLSIGNCODE=path if not in PATH)
make dist            # dist/: nvddi.sys (signed), nvddictl.exe, nvddi-test.cer, install scripts
```

Toolchain: `x86_64-w64-mingw32-gcc` (13), clang (layout pass), python3,
openssl, osslsigncode 2.8 (Ubuntu package; `apt-get download osslsigncode`
and `dpkg -x` works without root). The link uses a generated import library
for exactly the ntoskrnl exports used (`driver/ntoskrnl.def`), native
subsystem, `DriverEntry` entry, dynamic base, NX, relocations;
`tools/pefix.py` checks the image (no W+X section, no TLS, imports only from
ntoskrnl) and writes the PE checksum (osslsigncode keeps it valid).

## Install in the VM (main session)

Prerequisites: Secure Boot off, Memory Integrity (HVCI) off,
`bcdedit /set testsigning on` + reboot.

1. Copy `dist\` to the VM, elevated prompt there: `install.cmd`
   (imports `nvddi-test.cer` into Root and TrustedPublisher, copies the driver
   to `System32\drivers`, creates service `nvddi`: kernel, start=system,
   group "Video Init", error=ignore; writes `Parameters`; `sc start nvddi`).
2. `nvddictl status` must show `attached yes`.
3. nvlddmkm registered before nvddi was loaded, so restart the GPU:
   `powershell -ExecutionPolicy Bypass -File restart-gpu.ps1`
   (pnputil /restart-device, fallback disable/enable). `status` must show
   `hooked 1 nvlddmkm` and a growing record count.
4. From the next boot on nvddi loads at system start; whether it is in time
   for nvlddmkm depends on PnP order (`status` tells). If not, run
   `restart-gpu.ps1` again after logon.

**First-bring-up capture without a GPU restart:** install with
`install.cmd boot`. nvddi is then a boot-start driver that attaches to
`\Device\DxgKrnl` (loading the DXGKrnl service itself if needed) before the
PnP manager starts nvlddmkm, so `DxgkInitialize` is wrapped on the very first
start. If `\Device\DxgKrnl` is not ready when nvddi loads, it retries through
driver reinitialization callbacks. `restart-gpu.ps1` remains the fallback.

Registry (`HKLM\SYSTEM\CurrentControlSet\Services\nvddi\Parameters`):
`Enabled` 1, `Level` 3, `Target` nvlddmkm, `LogDir` `\??\C:\nvddi`,
`RingKB` 8192, `TotalRingMB` 256, `MaxBlobKB` 2048 (capped at a quarter
ring), `MaxFileMB` 4096, `HookCallbacks` 1, `HookVidPn` 1, `LogRegistry` 1,
`ResolveGpuVa` 1, `GpuVaEntries` 0 (=1M), `HookImports` 0 (HVCI off only),
`BootStart` 0, `Mute` (multi-string of slot names), `MaxFails` 2,
`HealthySeconds` 90.

`nvddictl`: `status | start | stop | flush | level N | mute NAME | unmute NAME |
mark TEXT | reset | enable | disable`.

## Test plan

1. Install as above; `nvddictl status`; `restart-gpu.ps1`; the desktop must
   come back on the NVIDIA adapter (Device Manager without error).
2. `nvddictl mark idle`, wait 10 s, `nvddictl flush`. Expect in
   `ndldump.py LOG`: REGISTER nvlddmkm HOOKED, TABLE rows for
   DRIVER_INITIALIZATION_DATA and DXGKRNL_INTERFACE, StartDevice /
   QueryAdapterInfo (DRIVERCAPS, QUERYSEGMENT*, GPUMMUCAPS ...) /
   QueryChildRelations / RecommendFunctionalVidPn / CommitVidPn with VidPN
   interface calls, BuildPagingBuffer, SetVidPnSourceAddress*, interrupts,
   `--registry` entries under `Services\nvlddmkm` and the Class key.
3. For each of `vktest.exe`, `d3d11test.exe`, `d3d12test.exe`,
   `cudatest.exe` (nvtrace/dist): `nvddictl mark <name>`, run it, then
   `nvddictl flush`. Expect CreateDevice/CreateContext/CreateHwQueue,
   CreateAllocation with private data, Escape (NVDA RM calls; `--escapes`),
   SubmitCommandVirtual/ToHwQueue, paging operations (`--paging`), and the
   command bytes behind each submit (`--cmdbuf`: segment-resolved runs decoded
   as NVIDIA push-buffer methods). With `HookImports` on (HVCI off),
   `--imports` shows the miniport's BAR maps, contiguous allocations and
   direct registry reads.
   DestroyAllocation/DestroyContext at exit.
4. `nvddictl stop` closes the file; copy `C:\nvddi\*.ndl` to the host and run
   `decoder/ndldump.py` (`--summary`, `--timeline`, `--paging`, `--escapes`,
   `--vidpn`, `--events -v`, `--registry`, `--dump-blobs DIR`).
5. To record the whole bring-up with no restart, reinstall with
   `install.cmd boot` and reboot; the first StartDevice/QueryAdapterInfo/
   CommitVidPn sequence then appears at the top of the log.
6. Watch `status`: `dropped` should stay 0 at level 3; if not, raise
   `RingKB`/`TotalRingMB` or mute high-rate slots
   (`nvddictl mute DxgkDdiInterruptRoutine`).

## Rollback

- `uninstall.cmd` (elevated): disables and deletes the service, removes the file
  and certificate; the image leaves memory at the next reboot (nvddi cannot
  unload once wrappers are handed out).
- Automatic: every load increments `FailCount`; after `HealthySeconds` of
  uptime it is reset. A load that finds `FailCount >= MaxFails` (2) does
  nothing. If the hooked `DxgkDdiStartDevice` fails, `FailCount` is set to
  `MaxFails` at once. `nvddictl reset` re-enables.
- nvddi never loads in safe mode (checks `InitSafeBootMode`): boot safe mode
  and run `uninstall.cmd`, or `sc config nvddi start= disabled`.
- Offline (WinRE command prompt):
  `reg load HKLM\OFF C:\Windows\System32\config\SYSTEM`,
  `reg add HKLM\OFF\ControlSet001\Services\nvddi /v Start /t REG_DWORD /d 4 /f`,
  `reg unload HKLM\OFF`.
- The service uses error control "ignore": a load failure never stops boot.

## Known gaps and risks

- GPU-VA command buffers resolve only through mappings nvddi has already seen
  (QUERYSEGMENT + UPDATE_PAGE_TABLE). A VA mapped by a path nvddi did not
  observe (or before its page table was logged) resolves to an unmapped marker.
  The map is 1-based on segment id, matching the QUERYSEGMENT order; if the GPU
  numbers PTE segments differently, lookups miss (markers, no wrong data). The
  user-mode command stream is also covered by nvtrace.
- Import logging (`HookImports`) is off by default and works only with HVCI
  off (see above); PCI config access the miniport does through HAL interfaces
  it queried (not a named import) is still not seen.
- `SubmitCommandToHwQueue` and anything else above DISPATCH_LEVEL gets argument
  records and, for the command buffer, only system/aperture-backed runs; a
  local-memory run is marked `irql` rather than read at raised IRQL.
- With HVCI/kernel CFG on, a mingw-built driver without CFG metadata may not
  load; run with Memory Integrity off.
- `DpiInitialize`/`DxgkInitialize` internals are undocumented; the IOCTL codes,
  the output being the function pointer, and the DDI table copy semantics were
  verified only by static disassembly. dxgkrnl is assumed not to check that DDI
  pointers lie inside the miniport image (no such check is known).
- Miniports newer than the WDDM 3.2 headers: unknown trailing DDI slots are
  copied (0x800 bytes) but not wrapped; unknown DXGKRNL_INTERFACE callbacks
  beyond the 26100 headers likewise.
- Nonpaged memory: rings up to `TotalRingMB` (256 MB default) are allocated at
  load.
- Logs can be large at level 3 under load (paging DMA, escapes); `MaxFileMB`
  caps a file.

# nvtrace QEMU: record what nvlddmkm.sys does with the RTX 5090

A QEMU build (conduit's QEMU 11.1.2 + conduit's patches + `patches/` here)
that traps the 5090's BAR0 and writes every guest access, config access,
interrupt, reset and GSP/FSP message to a binary trace, from the first
firmware access onwards. Purpose: the blueprint for emulating a 5090 later
(forwarding its GSP/RM traffic to the host's RM).

## Layout

| Path | What |
|---|---|
| `build.sh` | Builds `install/bin/qemu-system-x86_64` (same tarball, conduit patches and configure options as `host/qemu/build-qemu.sh`, plus `patches/`). `nice -n 10`, at most `-j16`. Never touches `/opt/conduit` or the conduit repo. |
| `patches/0101-...` | The tracer: `hw/vfio/nvtrace.[ch]` and hooks in `hw/vfio/{pci,region,pci-quirks}.c`, `hw/pci/msix.c`. |
| `patches/0102-...` | Test-only: `NVTRACE_TEST_QUIRKS=1` makes `vfio-user-pci` set up the vfio-pci BAR quirks. No effect without the variable. |
| `vm-trace.sh` | `vm.sh` with this QEMU and tracing on (`/steam/conduit-scratch/nvcap/vm-trace.sh` links here). |
| `tools/mmiodump.py` | Decoder (Python 3 stdlib). |
| `tools/gen_tables.py` → `tools/nvtables.json.gz` | Register, RPC, class and control-command names from open-gpu-kernel-modules 610.57.04. |
| `tools/test_mmiodump.py` | Decoder unit tests on synthetic traces. |
| `tools/test_qemu_trace.py`, `tools/fakegpu.py` | End-to-end test of the built QEMU with a fake 10de:2b85 vfio-user device. |

`src/`, `build/`, `install/`, `ref/` (extracted RM sources) are build outputs and are not in git.

## Build

```sh
./build.sh            # extract + patch + configure + build + install into ./install
./build.sh --dev      # rebuild the tree in src/ as it is (for development)
```

`qemu-system-x86_64 --version` prints `(conduit-nvtrace, patches N <sha8>)`.

## Run

As root, instead of `vm.sh` (the 5090 must be on vfio-pci, as for `vm.sh`):

```sh
/steam/conduit-scratch/nvcap/vm-trace.sh            # trace -> /steam/conduit-scratch/nvcap/traces/<date>/
/steam/conduit-scratch/nvcap/vm-trace.sh --print    # show the generated command line and its diff to vm.sh
```

`vm-trace.sh` generates the command line from `vm.sh` at run time: it swaps
the QEMU binary and adds
`x-nvtrace=on,x-nvtrace-file=<trace>,x-trap-bars=0x1` to the 5090's
`-device vfio-pci,host=0000:01:00.0`. The rest stays identical. It refuses
to run if `vm.sh` no longer matches. The generated script and an `info.txt`
are kept next to the trace. `NVTRACE_AUDIO=1` also traces 01:00.1 into the
same file.

Stop the VM normally (guest shutdown or QMP `quit`). The trace is flushed on
exit. After a SIGKILL, at most the last ring contents are lost, and the file
is still readable up to the cut.

### vfio-pci properties (also on vfio-user-pci)

| Property | Default | Meaning |
|---|---|---|
| `x-trap-bars=MASK` | `0x1` with `x-nvtrace`, else `0` | BARs that are never mmapped, so every guest access (kernel *and* user-mode mappings) exits to QEMU. Use `0x9` to also trap BAR3, `0xb` for BAR1+BAR3 (VRAM aperture: very slow). |
| `x-nvtrace=on` | off | Trace this device. |
| `x-nvtrace-file=PATH` | `$NVTRACE_MMIO` | Trace file. `.zst`, `.lz4` and `.gz` pipe through `zstd -3 -T2` / `lz4 -1` / `gzip -1`; the compressor runs on the CPUs QEMU is *not* pinned to. One file per QEMU process. |
| `x-nvtrace-irq=on` | on | Disable KVM irqfd bypass for INTx/MSI/MSI-X and all ioeventfds (the NVIDIA quirk's MSI-ack ioeventfd included), so every interrupt and every BAR write is seen. |
| `x-nvtrace-gsp=on` | on | FSP/GSP capture (only on vendor 10de, display class). |

### Environment

| Variable | Default | Meaning |
|---|---|---|
| `NVTRACE_MMIO` | | Trace file if `x-nvtrace-file` is not set. |
| `NVTRACE_BUF_MB` | 64 (`vm-trace.sh`: 128) | In-memory ring between producers and the writer thread. |
| `NVTRACE_ROTATE_MB` | 0 = off (`vm-trace.sh`: 4096) | Start a new file after this many uncompressed bytes: `name.nvt.000.zst`, `.001` (index before the compression suffix). |
| `NVTRACE_BLOCK_MS` | 5000 | When the ring is full, producers wait up to this long, then drop records (counted in a `LOST` record). |
| `NVTRACE_LOSSY` | 0 | 1 = never wait, drop at once. |
| `NVTRACE_MIN_FREE_GB` | 20 | Tracing stops (VM keeps running) when the trace file system has less free space. |
| `NVTRACE_GSP_POLL_US` | 100 | GSP status/command queue poll period once the queues are known. |
| `NVTRACE_GSP_LOG_MS` | 1000 | Snapshot period for GSP log buffers (only when their put pointer moved); 0 = only at boot/reset/exit. |
| `NVTRACE_PAIR_BUDGET_MB` | 256 | Budget for the LO/HI register-pair snapshot heuristic. |

Put traces on `/steam` (the default): a full root file system pauses VMs.

## Trace format

Little endian. Each file starts with a 64-byte header, then records.

File header: `char magic[8]="NVTRACE1"; u32 version=1; u32 hdr_size=64; u64 realtime_ns; u64 monotonic_ns; u32 file_index; u32 pid; char qemu[24]`.

Record header (32 bytes): `u16 type; u8 dev; u8 flags; u32 len` (whole record, 8-byte aligned) `; u64 seq` (global order, no gaps unless a `LOST` record says so) `; u64 ts` (CLOCK_MONOTONIC ns) `; i16 vcpu` (-1 = not a vCPU thread) `; u16 rsvd; u32 tid`. Flags bit 7: data truncated to fit the ring.

| type | name | body (after the header) |
|---|---|---|
| 1 | MMIO | `u8 bar, size, write, origin; u32 0; u64 offset; u64 value`. origin: 0 direct to the device, 1 config-mirror quirk (BAR0+0x88000; value is what the guest saw), 2 access QEMU made for a quirk, 3 window quirk, 4 ioeventfd fallback, 5 emulated MSI-X table, 6 emulated MSI-X PBA, 7 expansion ROM (bar 6, from QEMU's copy). |
| 2 | CFG | `u32 offset; u8 len, write; u16 0; u32 value; u32 0` (value read = what the guest saw) |
| 3 | IRQ | `u8 kind` (0 INTx, 1 MSI, 2 MSI-X) `, masked; u16 vector; u32 msi_data; u64 msi_addr` |
| 4 | VEC | `u8 event` (0 vector use, 1 release, 2/3 MSI enable/disable, 4/5 MSI-X enable/disable) `; u8 0; u16 vector; u32 data; u64 addr` |
| 5 | RESET | `u32 kind` (0 QEMU reset begin, 1 end, 2 guest set PCIe DEVCTL.BCR_FLR) `; u32 0` |
| 6 | BAR | `u8 bar; 7x0; u64 old, new, size` (all-ones = unmapped) |
| 7 | GSP_MSG | `u8 queue` (0 command CPU→GSP, 1 status GSP→CPU) `, flags` (1 truncated, 2 no RPC signature found, 4 sequence gap before it) `; u16 nelem; u32 slot, write_ptr, read_ptr, len, rpc_off; u64 gpa`, then `len` bytes: the queue element(s) as in guest memory (`GSP_MSG_QUEUE_ELEMENT` header, then `rpc_message_header_v` at `rpc_off`, then the RPC parameters). |
| 8 | DMA_SNAP | `u64 gpa; u32 len, flags` (1 not RAM/no data, 2 not guest memory, e.g. the ROM image) `; char label[24]`, then the bytes |
| 9 | NOTE | text |
| 10 | GSP_QSTATE | `u32 cmd_wp, cmd_rp, stat_wp, stat_rp` (logged when they change) |
| 11 | LOST | `u64 records, bytes` dropped before this record |
| 12 | FSP_PKT | `u8 dir` (0 CPU→FSP, 1 FSP→CPU) `; 3x0; u32 len`, then the EMEM packet (MCTP transport header first) |
| 13 | GSP_QINFO | `u64 shared_pa; u32 npte, elem_hdr; u64 cmd_off, stat_off; u32 cmd_size, stat_size, source` (1 FSP COT, 2 falcon mailbox, 3 RAM scan) `; u32 0` |
| 14 | DEV | `u16 vendor, device; u32 class, trap_mask, flags; u64 0`, then the device name |

## How the GSP queues are found and captured

From the 610.57.04 sources (`kernel_gsp.c`, `kernel_gsp_gh100.c`,
`kern_fsp_gh100.c`, `message_queue_cpu.c`, `msgq.c`; GB202 uses the GH100
FSP and TU102 doorbell HALs):

1. RM sends the **COT** boot command to FSP: it writes the packet through
   `NV_PFSP_EMEMC(0)`/`EMEMD(0)` (0x8f2ac0/4, auto-increment) and starts it
   with `NV_PFSP_QUEUE_TAIL(0)`/`QUEUE_HEAD(0)` (0x8f2c04/0x8f2c00). QEMU
   mirrors EMEM, logs each packet (`FSP_PKT`), reassembles MCTP SOM/EOM
   messages and, for NVDM type 0x14, reads `NVDM_PAYLOAD_COT.gspBootArgsSysmemOffset`
   (+852): the guest physical address of `GSP_FMC_BOOT_PARAMS`. FSP replies
   are rebuilt from the `EMEMD` reads between the `MSGQ_HEAD/TAIL` reads and
   the `MSGQ_HEAD` write.
2. `GSP_FMC_BOOT_PARAMS` (88 bytes): `bootGspRmParams.gspRmDescOffset` (+16)
   = `GspFwWprMeta`, `gspRmParams.bootArgsOffset` (+48) = the libos init
   args (`LibosMemoryRegionInitArgument[]`, 32 bytes each). The region with
   id8 `"RMARGS"` is `GSP_ARGUMENTS_CACHED`; its
   `MESSAGE_QUEUE_INIT_ARGUMENTS` give `sharedMemPhysAddr`,
   `pageTableEntryCount`, `cmdQueueOffset`, `statQueueOffset`,
   `queueElementHdrSize`. `LOG*` regions are the GSP log buffers.
   All of these are snapshotted once (`DMA_SNAP` labels
   `GSP_FMC_BOOT_PARAMS`, `GSP_WPR_META`, `LIBOS_INIT_ARGS`,
   `GSP_ARGUMENTS_CACHED`, `GSP_QUEUE_PTES`, `GSPLOG:<id>`).
3. The queue memory is not contiguous: page 0 holds the PTE array
   (`PTE[i]` = DMA address of page i; `PTE[0]` = `sharedMemPhysAddr`), the
   command queue (`msgqTxHeader` + 4 KiB elements, 63 slots by default)
   starts at `cmdQueueOffset`, the status queue at `statQueueOffset`.
   With no vIOMMU, DMA address = guest physical address. The consumer read
   pointers are swapped (`MSGQ_FLAGS_SWAP_RX`): the CPU's status-queue read
   pointer is in the command queue's rx header and vice versa.
4. Capture: before every write to `NV_PGSP_QUEUE_HEAD(0)` (0x110c00, the
   command doorbell; RM writes 0 after `msgqTxSubmitBuffers`) both queues
   are drained from the last logged slot up to the current `writePtr`. Each
   RPC (one or more elements; length from `rpc_message_header_v.length`)
   becomes one `GSP_MSG` record. The status queue (GSP writes it, then
   raises an interrupt; the CPU consumes it by writing memory only) is
   drained on every device interrupt and by a poller thread every
   `NVTRACE_GSP_POLL_US`. A gap in `GSP_MSG_QUEUE_ELEMENT.seqNum` sets flag 4.
5. Fallbacks: (a) writes to `NV_PGSP_FALCON_MAILBOX0/1` (direct GSP-FMC boot
   path) are tried as the `GSP_FMC_BOOT_PARAMS` address; (b) any write of an
   adjacent 32-bit register pair (or a 64-bit write) whose value, shifted by
   0, 8 or 12, is a guest RAM address snapshots that page once
   (`pair:<reg><<shift>`, falcon IMEM/DMEM/EMEM data ports excluded,
   budget-limited); (c) if the doorbell rings while the queues are unknown,
   the poller scans guest RAM once for a page whose first u64 is its own
   address and whose PTEs lead to a valid `msgqTxHeader` (several candidates:
   the one whose `writePtr` moves wins).
6. A device reset/FLR forgets the queues; the next COT re-discovers them.

## What is and is not captured

Captured: every guest CPU access to trapped BARs (kernel and user-mode
mappings, any vCPU), the NVIDIA BAR0 config mirror and MSI-X table as the
guest sees them, the expansion ROM reads and image, all config space
accesses, BAR programming, MSI/MSI-X setup and every delivered interrupt,
QEMU resets and guest FLRs, FSP packets, GSP RPCs in both directions, the
GSP boot structures, GSP log buffers (raw; decoding needs the firmware's
log metadata), and pages pointed to by address register pairs.

Not captured:
- Anything the GPU does by itself: GSP/FSP/PMU internal state, VRAM and
  sysmem written by the GPU (except the queues and logs read back above),
  GPU page tables and instance blocks in VRAM, copy-engine traffic.
- CPU accesses to BARs that are not in `x-trap-bars` (by default BAR1 VRAM
  aperture and BAR3), i.e. CPU writes to VRAM through BAR1, and guest
  memory that the GPU reads by DMA (pushbuffers, GPFIFO, USERD in sysmem,
  page tables in sysmem) unless an address-pair snapshot happens to hit it.
- The GSP firmware ELF and FMC image contents (addresses only; the images
  ship in the driver package).
- Confidential-compute encrypted queues (not used on GeForce).
- Addresses with aperture bits set or behind a vIOMMU: they are logged as
  `not RAM` notes, nothing is read.
- Accesses made by QEMU itself appear with `vcpu=-1` (e.g. config reads at
  start-up).

## Expected slowdown (estimates, not measured on the 5090)

- Each BAR0 access becomes a VM exit + BQL + `pread`/`pwrite` on the vfio
  fd: roughly 3-10 µs instead of ~1 µs (reads) or ~0.1 µs (posted writes).
  Driver load and display mode sets (hundreds of thousands of register
  accesses) take seconds longer. Work submission through the usermode
  doorbell traps on every submit.
- Interrupts go eventfd → QEMU main loop → KVM: about +10-20 µs latency each.
- The poller thread wakes every 100 µs (about 1-2% of a core); the zstd
  child uses the non-VM cores. Trace volume is ~56 bytes per register
  access before compression (zstd typically 10-20x on this data).
- Desktop use should work but feel slower; GPU-bound rendering is mostly
  unaffected. Do not use it for benchmarks.

## Decoder

```sh
tools/mmiodump.py summary  TRACE...         # counts per type, block, register, IRQ, RPC
tools/mmiodump.py timeline TRACE... --types mmio,irq --grep PGSP --count 100
tools/mmiodump.py gsp      TRACE... [--hex] # RPCs: function, rpc seq, result, ALLOC hClass / CONTROL cmd names, reply latency
tools/mmiodump.py fsp      TRACE...         # FSP messages (COT fields, responses)
tools/mmiodump.py snaps    TRACE... --extract DIR
tools/mmiodump.py regs     TRACE... --top 200
tools/mmiodump.py json     TRACE...         # one JSON object per record
```

`.zst` needs the `zstd` binary. Pass all files of a rotated set; they are
ordered by their header index. Register names come from the published
Blackwell headers first and older architectures as fallback; unknown
offsets print as `<block>+0x<offset>`. Regenerate the tables with
`tools/gen_tables.py` (reads `ref/` or `git archive 610.57.04`).

## Tests

```sh
cd tools
python3 -m unittest -v test_mmiodump                  # decoder, synthetic traces
NVTRACE_TEST_TMP=/some/dir python3 test_qemu_trace.py -v   # built QEMU, fake GPU
```

`test_qemu_trace.py` runs `install/bin/qemu-system-x86_64` with the qtest
accelerator (no KVM, no guest) and `vfio-user-pci` connected to
`fakegpu.py`, a vfio-user server presenting 10de:2b85 with a 16 MiB BAR0
(FSP EMEM emulated) and a 64 MiB BAR1, both mmap-capable. The test plays
RM: programs the BARs, enables MSI-X, builds the 610.57.04 boot structures
and PTE-mapped queues in guest RAM, sends the FSP COT command, sends 74
GSP RPCs (one spanning 3 elements, enough to wrap the 63-slot ring),
writes GSP replies, raises an interrupt, does a system reset, and checks
the decoded trace. Further cases: `x-trap-bars=0x3` (BAR1 traced, rotation),
the RAM-scan fallback without COT, and a device without `x-nvtrace`
(unchanged behaviour, no trace file).

Not covered without the GPU: the vfio-pci kernel-driver path itself
(vfio-user shares all of `hw/vfio/pci.c` and `region.c` except the kernel
ioctls), KVM MMIO exits, and whether Windows' 610.74 nvlddmkm uses exactly
the 610.57.04 layouts (the decoder searches for the `VRPC` signature, so a
different element header size is tolerated).

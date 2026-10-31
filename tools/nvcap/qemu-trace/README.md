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
| `patches/0103-...` | Review fixes to 0101 (lock-free BAR0 read path, queue walker limits). |
| `patches/0104-...` | VRAM aperture pages (KVM dirty log), trap windows, buffers followed from GSP RPCs (memory, channels, display pushbuffers, doorbells). |
| `patches/0105-...` | GPU memory from the host side (VRAM via static BAR1 / PRAMIN, GMMU walks, GPFIFO + pushbuffers, VRAM sweep), large RPC reassembly, firmware images and GSP-RM areas from sysmem, PCI config dumps. |
| `vm-trace.sh` | `vm.sh` with this QEMU and tracing on (`/steam/conduit-scratch/nvcap/vm-trace.sh` links here). |
| `tools/mmiodump.py` | Decoder (Python 3 stdlib). |
| `tools/gen_tables.py` → `tools/nvtables.json.gz` | Register, RPC, class, control-command and status names, structure layouts (RPC, alloc and control parameters), display methods, from open-gpu-kernel-modules 610.57.04. `tools/cstructs.py` is its C layout parser. |
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

Further `vm-trace.sh` variables: `NVTRACE_TRAP` (x-trap-bars mask),
`NVTRACE_DIRTY` (x-nvtrace-dirty-bars mask, e.g. `0x8` = BAR3, `0xa` =
BAR1+BAR3; off by default), `NVTRACE_WINDOWS` (x-trap-windows list; the
script escapes the commas for `-device`). Examples:

`vm-trace.sh` turns on host-side GPU memory capture (`NVTRACE_VRAM=1`)
by default; `NVTRACE_VRAM=0` switches it off.

```sh
/steam/conduit-scratch/nvcap/vm-trace.sh                         # default: BAR0 + RPCs + GPU memory on events
NVTRACE_DIRTY=0x8 /steam/conduit-scratch/nvcap/vm-trace.sh      # + BAR3 pages the CPU writes
NVTRACE_DIRTY=0x8 NVTRACE_VRAM_SWEEP_MB_MIN=512 \
    /steam/conduit-scratch/nvcap/vm-trace.sh                     # everything: + slow full-VRAM sweep
NVTRACE_DIRTY=0xa NVTRACE_DIRTY_BUDGET_MB=32 /steam/conduit-scratch/nvcap/vm-trace.sh
NVTRACE_TRAP=0x9 /steam/conduit-scratch/nvcap/vm-trace.sh        # BAR3 fully trapped
NVTRACE_VRAM=0 /steam/conduit-scratch/nvcap/vm-trace.sh          # no host-side GPU reads
```

PCI BAR numbering vs NVIDIA's: PCI BAR0 = registers, PCI BAR1 = NVIDIA
"BAR1" (VRAM aperture, 32 GB with ReBAR), PCI BAR3 = NVIDIA "BAR2" (32 MB,
RM's kernel window for instance blocks and page tables).

Stop the VM normally (guest shutdown or QMP `quit`). The trace is flushed on
exit. After a SIGKILL, at most the last ring contents are lost, and the file
is still readable up to the cut.

### vfio-pci properties (also on vfio-user-pci)

| Property | Default | Meaning |
|---|---|---|
| `x-trap-bars=MASK` | `0x1` with `x-nvtrace`, else `0` | BARs that are never mmapped, so every guest access (kernel *and* user-mode mappings) exits to QEMU. Use `0x9` to also trap BAR3, `0xb` for BAR1+BAR3 (VRAM aperture: very slow). |
| `x-trap-windows=B:OFF+LEN[,...]` | none | Only these ranges (rounded to 4 KiB) are left un-mmapped and trap; the rest of the BAR stays mmapped. On the command line write `,,` between entries. With `x-trap-bars=0` this is the cheap mode: e.g. `0:0x110000+0x1000` (GSP falcon) traps only that page of BAR0. |
| `x-nvtrace-dirty-bars=MASK` | `0` | Log the pages of these *mmapped* BARs that the guest CPU writes (KVM dirty log, see "VRAM apertures"). `0x8` = BAR3, `0xa` = BAR1+BAR3. |
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
| `NVTRACE_FOLLOW` | 1 | 0 = do not follow addresses in GSP RPCs (memory, channels, display). |
| `NVTRACE_WATCH_MS` | 1000 | Re-read every followed buffer this often (in 256 KiB slices), log pages that changed; 0 = never. |
| `NVTRACE_WATCH_HOT_US` | 1000 | Minimum gap between re-reads of the "hot" buffers (USERD, error notifiers) that doorbells and interrupts trigger. |
| `NVTRACE_WATCH_MAX_MB` | 64 | Total size of followed buffers; `NVTRACE_WATCH_OBJ_KB` (256) caps one buffer. |
| `NVTRACE_DIRTY_MS` | 250 | Dirty-page collection period for `x-nvtrace-dirty-bars`. |
| `NVTRACE_DIRTY_BUDGET_MB` | 16 | BAR pages read per second at most (token bucket); the rest is counted as skipped. |
| `NVTRACE_DIRTY_RESCAN_MS` | 0 | Also re-read every page seen so far this often (catches GPU writes to them); 0 = off. |
| `NVTRACE_VRAM` | 0 (`vm-trace.sh`: 1) | GPU memory capture from the host side (see "GPU memory"). |
| `NVTRACE_VRAM_BUDGET_MB` | 32 | VRAM bytes per second read through BAR1. |
| `NVTRACE_VRAM_PRAMIN_KB` | 1024 | VRAM KiB per second read through the PRAMIN window (each 4 KiB holds the BQL ~0.5 ms). |
| `NVTRACE_VRAM_PERIOD_MS` | 1000 | Re-read all followed VRAM buffers and walk all channels at least this often. |
| `NVTRACE_VRAM_EVENT_US` | 1000 | Minimum gap between event-driven passes (doorbell, status message, interrupt, display PUT). |
| `NVTRACE_VRAM_SWEEP_MB_MIN` | 0 = off | Sweep all VRAM below gspFwRsvdStart at this rate: hashes of every page, contents of pages that changed since the last pass. |
| `NVTRACE_VRAM_SWEEP_FIRST` | 0 | 1 = also log the contents of every page in the first pass. |
| `NVTRACE_PB_MAX_KB` | 256 | Bytes captured per pushbuffer segment. |
| `NVTRACE_VRAM_BAR1_DYNAMIC` | 0 | 1 = use a non-static BAR1 mapping too (risk: stale mapping -> BAR1 fault). |
| `NVTRACE_VRAM_UNSAFE` | 0 | 1 = read VRAM before the WPR layout is known / inside the GSP reserved area. Do not. |
| `NVTRACE_FW_DUMP_MB` | 128 | Size cap for firmware images / GSP-RM sysmem areas dumped from guest memory; 0 = off. |

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
| 8 | DMA_SNAP | `u64 gpa; u32 len, flags` (1 not RAM/no data, 2 not guest memory, e.g. the ROM image, 4 re-read of a followed buffer whose content changed) `; char label[24]`, then the bytes |
| 9 | NOTE | text |
| 10 | GSP_QSTATE | `u32 cmd_wp, cmd_rp, stat_wp, stat_rp` (logged when they change) |
| 11 | LOST | `u64 records, bytes` dropped before this record |
| 12 | FSP_PKT | `u8 dir` (0 CPU→FSP, 1 FSP→CPU) `; 3x0; u32 len`, then the EMEM packet (MCTP transport header first) |
| 13 | GSP_QINFO | `u64 shared_pa; u32 npte, elem_hdr; u64 cmd_off, stat_off; u32 cmd_size, stat_size, source` (1 FSP COT, 2 falcon mailbox, 3 RAM scan) `; u32 0` |
| 14 | DEV | `u16 vendor, device; u32 class, trap_mask, flags; u64 0`, then the device name |
| 15 | BAR_PAGE | `u8 bar, reason` (0 guest CPU wrote it, 1 periodic re-read) `; u16 0; u32 len; u64 off` (in the BAR) `; u64 hash`, then the 4 KiB page. Only logged when the content differs from the last logged version. |
| 16 | GPU_PAGE | `u8 aperture` (0 VRAM, 2 sysmem, 3 sysmem non-coherent) `, reason` (0 event, 1 period, 2 sweep, 3 page-table page) `; u16 0; u32 len; u64 pa; u64 hash; char tag[24]`, then the bytes. GPU memory read from the host side, logged when its hash changed. |
| 17 | GPU_HASHES | `u64 first_pa; u32 count, pass`, then `count` u64 hashes of consecutive 4 KiB VRAM pages (0 = not readable). |
| 18 | PUSHBUF | `u32 hClient, hChannel, gp_index, gp_put, gp_get, flags` (1 entry unreadable, 2 truncated, 4 partial) `; u64 gpfifo_entry, va; u32 len, got`, then the pushbuffer segment. |

DMA_SNAP labels: `GSP_*`, `LIBOS_INIT_ARGS`, `GSPLOG:<id>` (boot structures,
logs), `pair:<reg><<shift>` (register-pair heuristic), `EXPANSION_ROM`,
`mem<hMemory>` (ALLOC_MEMORY pages), `ch<hChannel>.<inst|userd|ramfc|mthdbuf|errnotif|eccnotif>`
(channel memory), `dispPB:<chn>` (display pushbuffer at registration),
`dispPB:<chn>@<start>` (bytes between the old and new PUT), `w:<tag>`
(re-read, content changed), `PCI_CONFIG` (whole config space, at start
and after resets), and images (flag 8: `gpa` = byte offset in the image,
chunks of 1 MiB): `FW:GSP_RM_ELF` (the GSP-RM ELF the driver loaded,
followed through its radix3 page lists), `FW:BOOTLOADER`, `FW:SIGNATURE`,
`FW:FMC` (first 512 KiB at the COT address), `LIBOS:<id>` (every other
libos region in sysmem), `ARGS:{profiler,sysmemHeap,rmStateMonitor,bindata}`
(GSP_ARGUMENTS_CACHED areas; the first three are also watched). An image
identical to the previous dump is not dumped again.

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

## Buffers followed from GSP RPCs

In GSP mode the CPU-side RM tells GSP-RM the physical address of every
buffer the GPU will touch on its behalf. The tracer reads those addresses
out of the command-queue RPCs (610.57.04 layouts) as they are logged:

| RPC | What is followed |
|---|---|
| `ALLOC_MEMORY` (4), class `NV01_MEMORY_LIST_SYSTEM` | `pteDesc.pte_pde[]` page frame numbers (one entry = contiguous): the pages are snapshotted (`mem<h>`) and watched. This is how memory registered with GSP-RM (`NVOS32_ATTR2_REGISTER_MEMDESC_TO_PHYS_RM`: notifiers, semaphores, profiler buffers, ...) arrives. `LIST_FBMEM` carries VRAM pages: decoded by `rmlog`, no data. |
| `GSP_RM_ALLOC` of a `*_CHANNEL_GPFIFO_*` class (Volta+) | `NV_CHANNEL_ALLOC_PARAMS.{instanceMem,userdMem,ramfcMem,mthdbufMem,errorNotifierMem,eccErrorNotifierMem}` with `addressSpace` = sysmem: snapshotted (`ch<h>.<name>`) and watched; USERD and the error notifier are "hot". Vidmem ones are only decoded (`rmlog`). |
| `GSP_RM_CONTROL` `NV2080_CTRL_CMD_INTERNAL_DISPLAY_CHANNEL_PUSHBUFFER` | display channel class, instance, pushbuffer address/size. Core = channel 0 (PUT at BAR0 0x680000), window *i* = channel 1+*i* (PUT at 0x690000 + *i*·0x1000). Every guest PUT write logs the new pushbuffer bytes (`dispPB:<chn>@<start>`) before the hardware fetches them. |
| `FREE` (10) | stops watching the object's buffers. |

Watched buffers are re-read (only changed pages are logged, `w:<tag>`):
the hot ones after every usermode doorbell write
(`NV_VIRTUAL_FUNCTION_DOORBELL`, BAR0 0xbb0090, written from guest user mode
by the UMDs; value = chid 11:0, runlist 22:16) and every interrupt, all of
them every `NVTRACE_WATCH_MS`. The re-reads run in the GSP poller thread in
slices, never on a vCPU. A device reset forgets all of it.

GPFIFO rings and pushbuffers are given to RM as GPU virtual addresses
(`gpFifoOffset`); translating them needs the GMMU page tables, see "Gaps".

## VRAM apertures (BAR1 / BAR3)

Trapping a VRAM aperture makes every CPU access exit (BAR1 is 32 GB and
carries the UMDs' uploads: unusable). Two cheaper modes:

1. **Dirty pages** (`x-nvtrace-dirty-bars`): the BAR stays mmapped (no exits
   on access). QEMU turns on KVM dirty logging for the BAR's memslots; every
   `NVTRACE_DIRTY_MS` the main loop collects the written pages and a worker
   thread re-reads each one with `pread()` on the vfio device fd (a
   side-effect-free VRAM read; -EIO instead of SIGBUS while memory decode is
   off), hashes it and logs it (`BAR_PAGE`) when the content changed. This
   shows what the driver writes into VRAM through the apertures: page
   directories/tables and instance blocks written via BAR3, display
   surfaces, pushbuffers/GPFIFO/USERD the UMDs place in vidmem. It shows the
   *final* content per period, not each store, and nothing the GPU writes
   unless `NVTRACE_DIRTY_RESCAN_MS` re-reads known pages. A fresh KVM slot
   reports every page dirty (`KVM_DIRTY_LOG_INITIALLY_SET`), so the first
   collection after a BAR is (re)mapped is discarded; collections where
   more than half of a >= 256 MB range is dirty are discarded too.
2. **Trap windows** (`x-trap-windows`): exact, per-access tracing of chosen
   ranges, e.g. the first pages of BAR3 where RM keeps its BAR2 page
   directory, while the rest stays mmapped. Ranges are fixed at start-up;
   pick them from a dirty-page run (`mmiodump.py bars` lists the hot pages).

Also decoded without either: VRAM accessed through the BAR0 PRAMIN window
(BAR0 0x700000-0x7fffff, positioned by `NV_XAL_EP_ZB_BAR0_WINDOW` 0xd40,
BASE << 16 [field width assumed from older chips]), which RM uses before
BAR2 is up: `mmiodump.py pramin`.

Cost of the dirty mode, measured with the KVM test below (fake device,
memfd-backed BAR): a 16 MiB guest store burst into BAR1 took 0.60 ms
without and 2.9 ms with tracking, i.e. ~0.6 us per 4 KiB page for the first
write after each collection (later writes in the period are free). On the
5090 the `pread()` of a page is ~512 non-posted 8-byte PCIe reads, roughly
0.3-1 ms of worker-thread time per page [estimate], hence the default budget
of 16 MB/s; the vCPUs do not wait for it. Collecting a 32 GB BAR1 bitmap
costs a few ms of main-loop time per period [estimate].

Risk: a page is read up to one period after the guest wrote it; if the
driver unmapped that BAR1 range in between, the read hits an unmapped BAR1
page, which the GPU reports as a BAR1 fault to the driver. Start with BAR3
only (`0x8`); BAR3 is RM's own fixed kernel window.

## What an emulated 5090 has to fake until GSP INIT_DONE

From the 610.57.04 GB202 paths (FSP chain of trust; `bPartitionedFmc`, so no
booter, no FWSEC, no CPU sequencer). `mmiodump.py boot TRACE --json F`
produces the same table from a real trace, with the values the 5090 really
returned; use it to confirm or correct this list, especially for 610.74.

| Step | BAR0 | Driver expects |
|---|---|---|
| Identify | `NV_PMC_BOOT_0` 0x0, `NV_PMC_BOOT_42` 0xa00 | GB202 ids (arch 0x1b, impl 2); BOOT_42 also carries the revision |
| FB size | 0x1183a4 (PGC6 scratch) | usable FB size in MB |
| FSP up | `NV_THERM_I2CS_SCRATCH` 0xad00bc | `0xff` (FSP boot complete), 5 s timeout |
| ECC / debug fuses | 0x823814 bit 16, 0x82074c bit 0 | ECC on; 1 = production-signed FMC |
| WPR2 | HUBMMU `WPR2_ADDR_HI` 0x88a828 | 0 before boot (WPR2 down) |
| FMC error scratch | `NV_PBUS_SW_SCRATCH(0x37)` 0x14dc | driver writes 0, expects 0 |
| FSP messages | EMEMC/EMEMD 0x8f2ac0/4 (BLK 15:8, OFFS 7:2, AINCW 24, AINCR 25), QUEUE_HEAD/TAIL 0x8f2c00/4, MSGQ_HEAD/TAIL 0x8f2c80/4 | on the QUEUE_HEAD write: consume the packet, set TAIL = HEAD, put the reply in EMEM at 0, MSGQ_HEAD = 0, MSGQ_TAIL = len-4. Reply = MCTP SOM+EOM, NVDM type 0x15 (`0x1510de7e`), task id, command type, error code 0 |
| CAPS_QUERY (NVDM 0x16) | as above | reply error 0x9f (not found) = no clock boost, or OK and then also answer CLOCK_BOOST (0x20) with OK |
| COT (NVDM 0x14, 868 bytes, v2) | as above | reply OK; then the emulator has the GSP_FMC_BOOT_PARAMS address (`gspBootArgsSysmemOffset`) and from it the queues |
| GSP alive | `NV_PGSP_FALCON_HWCFG2` 0x1100f4 | nonzero, not `0xbadf41xx`; bit 13 (RISCV_BR_PRIV_LOCKDOWN) clear after boot |
| FMC result | `NV_PGSP_FALCON_MAILBOX0` 0x110040 | 0 |
| RISC-V running | `NV_PRISCV_RISCV_CPUCTL` 0x111388 | bit 7 (ACTIVE_STAT) set |
| CrashCat | `NV_PGSP_FALCON_DEBUGINFO` 0x110094 | 0 (no crash report signature) |
| Timer | PGC6 SEC_TIMER 0x118f54/58 | below OS time in ns; writes to 0x118df4/f8 |
| Queues | sysmem | GSP writes the status-queue `msgqTxHeader` (version 0, size, msgSize 0x1000, msgCount, flags 1 = SWAP_RX, rxHdrOff, entryOff), elements with the 16-byte MCTP/NVDM header, checksum (XOR fold of the 64-bit words = 0) and seqNum from 0, then the event `GSP_INIT_DONE` (0x1001, payload `bIsD3Hot`) |
| Doorbell | `NV_PGSP_QUEUE_HEAD(0)` 0x110c00 | written (0) after every command RPC: drain the command queue |

The driver polls during all of this; no interrupt is needed before
INIT_DONE. After INIT_DONE it sends `SET_GUEST_SYSTEM_INFO` and
`GET_GSP_STATIC_INFO` (GspStaticConfigInfo: GPU name, FB regions, BAR1/BAR2
PDE bases, the internal client/device/subdevice handles) synchronously;
from there on every GSP RPC is ordinary RM API traffic (`mmiodump.py
rmlog`), which is what gets forwarded to the host RM.

## Windows 610.74 vs 610.57.04

Everything structural here (queue layouts, RPC numbers, parameter layouts)
comes from the Linux 610.57.04 sources; nvlddmkm 610.74 is the same r610
branch but a different changelist and ships its own GSP firmware.

- The queue walker does not depend on the element header size (it finds
  the `VRPC` signature) and takes the element limit from the boot args.
- `mmiodump.py gsp --decode` and `rmlog` mark every ALLOC/CONTROL whose
  `paramsSize` differs from the 610.57.04 structure size with `LAYOUT
  MISMATCH`: that is the list of structures that changed.
- The follow-up capture uses fixed 610.57.04 offsets (channel memdescs at
  152..280, display pushbuffer control fields); values are sanity-checked
  (address space, sizes), so a changed layout only means fewer buffers, not
  bad reads.
- The doorbell/PUT/PRAMIN offsets are hardware, not driver, properties.
- The Windows 610.74 package (`DriverVer 32.0.16.1074`) ships GSP firmware
  whose `.fwversion` is "610.53", not 610.57.04: GSP-side structures can
  differ from the Linux sources in either direction; trust the LAYOUT
  MISMATCH marks over the tables.

## GPU memory from the host side (`NVTRACE_VRAM=1`)

A worker thread (`nvtrace-gpu`) reads GPU memory itself, independent of
what the guest CPU touches:

- **VRAM paths.** (1) BAR1, if RM maps VRAM statically into it: the BAR1
  page tables are walked from `GspStaticConfigInfo.bar1PdeBase` (taken from
  the `GET_GSP_STATIC_INFO` reply) and the reverse map VRAM address -> BAR1
  offset is used with `pread()` on the device fd, no BQL. A dynamic BAR1
  mapping is not used (it can vanish between walk and read). (2) The BAR0
  PRAMIN window (0x700000, 64 KiB granules via `NV_XAL_EP_ZB_BAR0_WINDOW`
  0xd40, BASE 22:0): runs as a main-loop bottom half with the BQL held; as
  all of BAR0 traps, the guest cannot touch BAR0 meanwhile; the window
  register is saved, moved and restored. Only with BAR0 fully trapped.
- **Never read:** anything before the WPR meta is seen, anything in
  `[gspFwRsvdStart, fbSize)` (WPR2, FRTS, GSP heaps, VGA workspace) or
  beyond fbSize, and a page that once returned PRI error words
  (`0xbad0....`/`0xbadf....`, the page is noted and blacklisted).
- **What is read:**
  - every buffer an RPC names in VRAM: channel instance block, USERD,
    RAMFC, method buffer, error notifiers (`ch<h>.<name>`), `LIST_FBMEM`
    memory (`mem<h>`), vidmem display pushbuffers (`dispPB:<chn>`; on each
    PUT the new bytes); sysmem buffers as before;
  - every GPFIFO channel: the PDB from its instance block
    (`NV_RAMIN_PAGE_DIR_BASE`, word 128), the GMMU walk (ver3: PD4 56,
    PD3 55:47, PD2 46:38, PD1 37:29, PD0 28:21 dual PDE, 64/128 KiB or 4 KiB
    PT; 512 MB / 2 MB PTEs at PD1/PD0; `kern_gmmu_fmt_gh10x.c`), every
    page-table page on the way (`pt:*`, logged when changed), USERD
    `GP_GET`/`GP_PUT` (words 34/35), then for every new GPFIFO entry the
    entry and the pushbuffer segment it points to (`PUSHBUF`; GPU VA from
    `GP_ENTRY0/1`, `SET_PB_SEGMENT_EXTENDED_BASE` honoured). Without a USERD
    memdesc, USERD and the ring come from RAMFC (USERD at +8 [nouveau
    layout], `NV_RAMFC_GP_BASE` words 18/19);
  - with `NVTRACE_VRAM_SWEEP_MB_MIN`, all of VRAM below gspFwRsvdStart,
    page by page: a `GPU_HASHES` record per 256 pages each pass, the
    contents of pages that changed since the previous pass (`sweep`).
- **When:** usermode doorbell, every GSP status-queue message, every
  interrupt and display PUT (at most every `NVTRACE_VRAM_EVENT_US`), plus
  every `NVTRACE_VRAM_PERIOD_MS`. Only content whose hash changed is logged.

Decoders: `mmiodump.py gpu` (pages per tag, sweep passes, `--extract`),
`pushbuf` (GPFIFO entries and methods; `SET_OBJECT` binds subchannels to
classes; method names from the class headers, Blackwell 3D/compute fall
back to Hopper's `cb97`/`cbc0` because their methods are not published).

## GSP-RM's own log

`GSPLOG:<id>` snapshots (LOGINIT, LOGRM, LOGINTR, ...) and
`EVENT_UCODE_LIBOS_PRINT` RPCs are libos log buffers: u64 put, then a ring
of records `args..., metadata, timestamp` (metadata = VA 47:0, record
length 55:48, task 63:56; `liblogdecode.c`). `mmiodump.py gsplog` lists the
records (task, PTIMER timestamp, metadata address, arguments) and, with
`--log-elf gsp_log_*.bin`, the source file, line and formatted message. The
format strings live only in that log ELF (`.fwlogging*` sections, stripped
from the firmware): neither the Linux 610.57.04 firmware nor the Windows
package (`Display.Driver/gsp_ga10x.bin`, `.fwversion` "610.53") contains
them, so without it the log is message ids + arguments (`--summary` counts
them).

## Physically or practically unreadable

| What | Why |
|---|---|
| WPR2 (GSP-RM code, data, heap in VRAM), FRTS, other protected FB ranges | Hardware write-protected regions: CPU/BAR reads return `0xbad0....` and raise PRI errors the driver sees. Never read; their layout is logged (WPR meta). The GSP-RM *image* is captured from sysmem before it is copied in. |
| Falcon/RISC-V IMEM, DMEM, TCM, caches of GSP, FSP, SEC2, PMU | Only reachable through the falcons' PRI ports, which are locked down after boot and use auto-incrementing index registers that the running firmware also uses. FSP EMEM is captured only for the RM channel. |
| GPU L2 / MMU TLB / engine state not yet in memory | On-chip. BAR1/PRAMIN reads are served coherently, but state that only exists in SRAM (in-flight contexts, PBDMA, ROP/ZROP caches, TLBs) is not memory. Context buffers become readable when an engine saves them. |
| BAR0 registers the driver does not read | QEMU does not read registers on its own: some clear on read or raise PRI errors. Every register the driver reads is logged with its value. |
| Compressed surfaces | Read back raw (compressed) through BAR1/PRAMIN; the compression tags are in on-chip/hidden storage. |
| Sysmem the GPU uses that no RPC names | E.g. UMD-private sysmem mapped only through GPU page tables: reachable through the GMMU walk when a pushbuffer or GPFIFO points there, not otherwise. |
| VRAM beyond the static BAR1 mapping at speed | PRAMIN only (~1 MB/s default); a full sweep through PRAMIN takes many hours. |
| Encrypted queues/memory (Confidential Compute) | Not used on GeForce. |

## Gaps (what an emulator still needs that this does not record)

- Display surfaces: window methods reference context DMAs, not physical
  addresses; the surfaces are covered by the VRAM sweep / dirty BAR pages
  but not tied to the flip that shows them.
- Subroutine GPFIFO entries (`LEVEL_SUBROUTINE`) and CALL/JUMP inside
  pushbuffers are logged as bytes, not followed further.
- GPU writes to VRAM between two reads of the same page are seen only as
  the final state (hash per pass), and the sweep sees a page once per pass.
- Large RPCs are re-assembled both in QEMU (follow-up sees all
  continuation records) and in the decoder.

## What is and is not captured

Captured: every guest CPU access to trapped BARs and trap windows (kernel
and user-mode mappings, any vCPU) with the value read or written, the
NVIDIA BAR0 config mirror and MSI-X table/PBA as the guest sees them, the
expansion ROM reads and image, all config space accesses, BAR programming,
MSI/MSI-X setup and every delivered interrupt, QEMU resets and guest FLRs,
FSP packets, GSP RPCs in both directions (commands, replies, events and
notifier posts), the GSP boot structures, GSP log buffers (raw; decoding
needs the firmware's log metadata), pages pointed to by address register
pairs, the buffers named in GSP RPCs (above), with
`x-nvtrace-dirty-bars` the VRAM-aperture pages the guest CPU writes, with
`NVTRACE_VRAM=1` GPU memory read from the host side (VRAM buffers, page
tables, GPFIFO entries and pushbuffers, optional full-VRAM sweep), the
GSP-RM firmware image and GSP-RM's sysmem areas, and the whole PCI config
space.

Not captured:
- See "Physically or practically unreadable" and "Gaps".
- The GSP firmware ELF and FMC image contents (addresses only; the images
  ship in the driver package).
- Confidential-compute encrypted queues (not used on GeForce).
- Addresses with aperture bits set are logged as `not RAM`, nothing is
  read. Behind a vIOMMU, addresses are translated through it (the device's
  DMA address space).
- Accesses made by QEMU itself appear with `vcpu=-1` (e.g. config reads at
  start-up).

## Expected slowdown

Estimates unless marked measured; not measured on the 5090.

| Mode | Cost |
|---|---|
| default (BAR0 trapped, follow, `NVTRACE_VRAM=1` from vm-trace.sh) | BAR0 accesses 3-10 µs each; GPU worker: per event pass a few 8-byte reads per channel plus one 4 KiB read per page-table page touched; through PRAMIN each 4 KiB read holds the BQL ~0.5 ms, capped at 1 MB/s (≤ ~13% of BQL time); through a static BAR1 no BQL, ≤ 32 MB/s of PCIe reads in the worker |
| `NVTRACE_VRAM_SWEEP_MB_MIN=512` | worker reads 8.5 MB/s through static BAR1 (one 4 KiB `pread` ≈ 0.3-0.5 ms, so ~10-15 MB/s is the ceiling per thread); 32 GB takes ~65 min per pass; first pass logs 8 bytes per page (~64 MB of hashes), later passes the changed pages |
| `NVTRACE_DIRTY=0x8` (BAR3 dirty pages) | ~0.6 µs per written page per 250 ms period (measured, fake device) |
| firmware dumps | once per boot: ~80 MB GSP-RM ELF + bootloader/FMC/args read from guest RAM in the poller (~25 MB after zstd) |
| trap windows | only accesses inside them exit |

- Each BAR0 access becomes a VM exit + BQL + `pread`/`pwrite` on the vfio
  fd: roughly 3-10 µs instead of ~1 µs (reads) or ~0.1 µs (posted writes).
  Driver load and display mode sets (hundreds of thousands of register
  accesses) take seconds longer. Work submission through the usermode
  doorbell traps on every submit. BAR0 reads no longer take the GSP lock.
- `x-trap-bars=0` + `x-trap-windows` instead: only the chosen pages exit;
  everything else runs at native speed (but is then not traced).
- Interrupts go eventfd → QEMU main loop → KVM: about +10-20 µs latency each.
- The poller thread wakes every 100 µs (about 1-2% of a core); the zstd
  child uses the non-VM cores. Trace volume is ~56 bytes per register
  access before compression (zstd typically 10-20x on this data).
- Followed buffers: a doorbell or interrupt costs nothing on the vCPU (a
  flag); the poller re-reads at most 1 MiB of hot buffers per
  `NVTRACE_WATCH_HOT_US` and 256 KiB per poll for the periodic pass
  (hashing ~5 GB/s). Display PUT writes copy the new pushbuffer bytes
  synchronously (a few hundred bytes per flip).
- Dirty VRAM pages (measured on the fake device): ~0.6 µs per 4 KiB page
  per collection period for the first guest write; reads run in a worker
  within `NVTRACE_DIRTY_BUDGET_MB` (16 MB/s ≈ 4000 pages/s, ~0.3-1 ms of
  PCIe reads each on the 5090 [estimate]).
- Desktop use should work but feel slower; GPU-bound rendering is mostly
  unaffected. Do not use it for benchmarks.

## Decoder

```sh
tools/mmiodump.py summary  TRACE...         # counts per type, block, register, IRQ, RPC
tools/mmiodump.py timeline TRACE... --types mmio,irq --grep PGSP --count 100
tools/mmiodump.py gsp      TRACE... [--decode [--all]] [--hex]
                                            # RPCs with every parameter field (610.57.04 layouts),
                                            # continuation records merged, LAYOUT MISMATCH marks
tools/mmiodump.py rmlog    TRACE... [--out] [--tree] [--internal] [--json calls.json]
                                            # the RM API sequence: Alloc/Control/Free/AllocMemory
                                            # with handles, class/cmd names, decoded params, status
tools/mmiodump.py boot     TRACE... [--json boot.json]   # registers + values until GSP INIT_DONE
tools/mmiodump.py fsp      TRACE...         # FSP messages (COT fields, responses)
tools/mmiodump.py disp     TRACE...         # display channels, PUT writes, decoded methods
tools/mmiodump.py bars     TRACE... [--extract DIR]      # BAR_PAGE records, most rewritten pages
tools/mmiodump.py pramin   TRACE...         # VRAM accesses through the BAR0 PRAMIN window
tools/mmiodump.py gpu      TRACE... [--extract DIR]      # GPU memory read from the host side
tools/mmiodump.py pushbuf  TRACE... [--channel H]        # GPFIFO entries, pushbuffer methods by class
tools/mmiodump.py fw       TRACE... --extract DIR        # GSP-RM ELF, bootloader, FMC, libos/args areas
tools/mmiodump.py gsplog   TRACE... [--log-elf F] [--summary]   # GSP-RM's own log
tools/mmiodump.py snaps    TRACE... --extract DIR
tools/mmiodump.py regs     TRACE... --top 200
tools/mmiodump.py json     TRACE...         # one JSON object per record
```

`rmlog --json` writes one object per call (`op`, handles, `hClass`/`cmd`,
`params` and reply `out` as hex, `status`): the input for a replay or for
forwarding to the host RM. Kernel-RM-internal RPCs (registry, system info,
static info, ...) are listed with `--internal`; `gsp --decode` decodes all
of them (179 of 274 RPC ids, 147 classes' alloc parameters and 1446 of 1496
control commands have a 610.57.04 layout in the tables).

`.zst` needs the `zstd` binary. Pass all files of a rotated set; they are
ordered by their header index. Register names come from the published
Blackwell headers first and older architectures as fallback; unknown
offsets print as `<block>+0x<offset>`. Regenerate the tables with
`tools/gen_tables.py` (`git archive 610.57.04` of
`/steam/refsrc/open-gpu-kernel-modules`; `--src DIR` if the repository is
not there).

## Tests

```sh
cd tools
python3 -m unittest -v test_mmiodump                  # decoder, synthetic traces
NVTRACE_TEST_TMP=/some/dir python3 test_qemu_trace.py -v   # built QEMU, fake GPU
```

`test_qemu_trace.py` runs `install/bin/qemu-system-x86_64` with the qtest
accelerator (no guest code) and `vfio-user-pci` connected to `fakegpu.py`,
a vfio-user server presenting 10de:2b85 with a 16 MiB BAR0 (FSP EMEM
emulated) and a 64 MiB BAR1, both mmap-capable. The test plays RM:
programs the BARs, enables MSI-X, builds the 610.57.04 boot structures and
PTE-mapped queues in guest RAM, sends the FSP COT command, sends 74 GSP
RPCs (one spanning 3 elements, enough to wrap the 63-slot ring), writes GSP
replies, raises an interrupt, does a system reset, and checks the decoded
trace. Further cases: `x-trap-bars=0x3` (BAR1 traced, rotation), the
RAM-scan fallback without COT, a device without `x-nvtrace` (unchanged
behaviour, no trace file), followed buffers (ALLOC_MEMORY pages, channel
USERD re-read on the doorbell and no longer after FREE, display pushbuffer
bytes per PUT write, decoded by `disp`/`rmlog`), trap windows on BAR0 and
BAR1 with both BARs otherwise mmapped, and, if `/dev/kvm` is usable, the
dirty-page path under KVM: a 64 KiB test firmware (source in the test)
stores into BAR1 from the guest CPU; the test checks the `BAR_PAGE`
records and that unchanged rewrites are not logged again. It runs a 1-vCPU
VM for about a second. `test_gpu_memory` (fake device with a PRAMIN window
over its BAR1-backed VRAM and one PRI-error page) builds BAR1 page tables,
a channel's instance block, page tables, USERD, GPFIFO and a sysmem
pushbuffer, the WPR meta with a radix3 GSP-RM image, and checks the BAR1
map, PRAMIN reads, the protected and PRI-error pages, the GMMU walk down to
the pushbuffer bytes and their decoded methods, a VRAM sweep change, the
firmware image and a continuation-record reassembly.

Not covered without the GPU: the vfio-pci kernel-driver path itself
(vfio-user shares all of `hw/vfio/pci.c` and `region.c` except the kernel
ioctls; the dirty-page reader uses `pread()` on the device fd there and
`memcpy` from the shared memory with vfio-user), KVM MMIO exits on the real
BAR, KVM dirty logging on PFNMAP (device) memslots, and whether Windows'
610.74 nvlddmkm uses exactly the 610.57.04 layouts (see above).

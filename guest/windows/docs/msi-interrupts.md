# MSI-X interrupts for the Helios KMD

Status: implemented and shipped OFF. This package's INF writes `MSISupported=0` (INTx, as
before); MSI-X is an opt-in (`MsiMode=2`, "Minimum safe test procedure") until one hardware run
passes, and then the default is flipped by changing one INF line ("The flip plan"). The INTx
fallback, the boot-loop breaker, the polling safety net and the per-vector counters are in the
package either way. NOT verified on hardware: the KMD could not be compiled or run where this was
written; everything marked "verify" is a claim that only a boot can confirm. The recovery path
if a start does not come up is in "Recovery".

## Why

Measured on v307 (INTx, `MSISupported=0`): one forwarded RM control costs 58 us
against 1.4 us native, the event round trip 142 us against 7.9. The host side
estimates INTx at 20-35 us per forward: QEMU cannot use an irqfd with INTx, so
every completion goes through QEMU's main loop, plus an exit for the ISR-status
read-to-clear and a line deassert. With MSI-X, vhost-user's per-queue call
eventfd becomes a KVM irqfd and a used-ring notification goes straight into the
guest. An NVK frame makes several RM calls, so this is frame time.

## What the platform gives us

* **Device.** `vhost-user-test-device-pci`, `num_vqs=2`, QEMU patch 0004 makes
  `nvectors = num_vqs + 1 = 3`. Windows reports `InterruptSupport = 5` (line +
  MSI-X) and `InterruptMessageMaximum = 3`. Standard virtio-pci layout: vector 0
  config, 1 control queue (vq0), 2 event queue (vq1). `conduit-vmm` has MSI-X
  plumbing too, with fixed BARs; the driver never assumes MSI-X.
* **QEMU behaviour (read from the vendored source).** A write to `msix_config` /
  `queue_msix_vector` is refused (reads back `0xFFFF`) only when the vector is
  `>= nvectors`. The vector registers are accepted whether or not the guest has
  enabled MSI-X yet; a notification uses `msix_notify` only while MSI-X is
  enabled, otherwise INTx.
* **The display miniport API.** `dispmprt.h` has ONE interrupt DDI,
  `DXGKDDI_INTERRUPT_ROUTINE(MiniportDeviceContext, MessageNumber)`. There is no
  separate "message interrupt routine" (the storport / NDIS split does not exist
  here) and the miniport never connects its own interrupt: dxgkrnl connects
  whatever PnP assigned and calls the same routine, with `MessageNumber = 0` for
  a line and the message index for MSI/MSI-X. The table slot the KMD already
  fills (`DxgkDdiInterruptRoutine`, `lib.rs`) is therefore the registration; no
  new DDI. `DxgkCbSynchronizeExecution` also takes a `MessageNumber`;
  `notify_at_dirql` passes 0, which exists in both modes. (Verified against the
  Win8-era `dispmprt.h` that is on disk; the WDK 10.0.26100 header the build
  uses was not available here. Verify the signature is unchanged: it is the
  compiled-in `Option<unsafe extern "C" fn(*const c_void, u32) -> BOOLEAN>`.)
* **Who chooses MSI vs INTx.** PnP, from the device key
  `Interrupt Management\MessageSignaledInterruptProperties\MSISupported`, when it
  builds the device's interrupt requirements for a start, before `StartDevice`.
  The driver cannot opt in or out of the start in progress: once messages are
  connected the INTx line is not, and a driver that "chose INTx" anyway would
  never be woken. What the driver CAN do is change the value for the NEXT start
  (see "Making the flip reach existing installs" and "Fallback").

## The INF value, the device key, and the flip plan

### The INF values

All under the device's HARDWARE key (`HKLM\SYSTEM\CurrentControlSet\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters`,
which is what `HKR` means in a `DDInstall.HW` section) in
`Interrupt Management\MessageSignaledInterruptProperties`:

| Value | Shipped | Meaning |
| --- | --- | --- |
| `MSISupported` | `0` (REG_DWORD, flag `0x00010001`, no NOCLOBBER) | `1` = PnP may give the device messages, `0` = the INTx line. |
| `MessageNumberLimit` | `3` (REG_DWORD) | The most messages PnP may grant: config, control queue, event queue. A third (RM) queue is a fourth. Omitted, PnP asks for as many as the device offers. |
| `Affinity Policy\DevicePolicy` / `DevicePriority` | not shipped | See "Affinity". |

`0x00010001` is `FLG_ADDREG_TYPE_DWORD`; `0x00000002` is `FLG_ADDREG_NOCLOBBER`
("do not overwrite a value that exists"). The dormant package (v325) wrote
`0x00010003` = DWORD + NOCLOBBER.

### Why no NOCLOBBER, and what an update does

NOCLOBBER keeps ANY existing value, including one the INF itself wrote. With it,
the INF is not the source of truth: a hand-edited value, or a value the driver
latched, would outlive every update, and a later flip of the shipped default to 1
would not reach the installs that hold the 0 the dormant package wrote. So the
line is written WITHOUT it, and this INF is the single source of truth for the
shipped default:

* **new install:** the `.HW` section creates the keys and writes `0` and `3`;
* **in-place update** (a newer-ranked package selected for the device, by
  `pnputil /add-driver ... /install`, Device Manager, Windows Update): the
  device is re-installed, the `.HW` section runs again, `MSISupported` is
  overwritten with `0` (a hand-set 1, or a 0 the driver wrote, is reset to the
  shipped value), and PnP restarts the device (or asks for a reboot, exit code
  3010, which `Install-Helios.ps1` already accepts);
* **same package reinstalled** (`pnputil` answers 259, "already installed"):
  the INF does NOT run again and the key keeps whatever it holds.

What survives an update is the service-key knob `MsiMode` (below), which the
driver applies to the key at `AddDevice`; that is how an operator keeps MSI-X
(or a forced INTx) across package updates without editing the device key.

### Can the KMD change it for the start in progress?

No. `MSISupported` is read by the PnP manager (the PCI bus driver building the
interrupt requirements) for a device start. The KMD sees the PDO in
`DxgkDdiAddDevice`, which runs before the requirements are built for that start,
so a write there MAY be read by the start that follows, but Windows documents no
such ordering. The driver therefore treats the write as guaranteed for the NEXT
start only, and always follows what PnP actually granted (`probe_granted`).

**After changing `MsiMode`, the first restart writes the key and a SECOND restart
(or a reboot) applies it.** `MsiWant` (what `AddDevice` asked for) next to
`MsiGrant` (what PnP gave) shows which: after `MsiMode=2` the first restart reads
`MsiWant=1`, `MsiKeyWr=0`, `MsiGrant=0` (the write came late), the second
`MsiGrant=3`. If the first already reads `MsiGrant=3`, the write was in time
(then one restart is enough; record it, checklist C). `MsiMode` going back
from 1 or 2 to 0 leaves the key where the forcing put it: `0` means "follow the
key", it does not write one. Only a package install (the INF), `MsiMode=1`, or a
latch lowers it.

### Exact registry steps

New installs and updates: the INF, nothing more. To opt in on this package, the
service-key way (no device key editing, survives updates):

```
reg add "HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render" /v MsiMode /t REG_DWORD /d 2 /f
pnputil /restart-device "PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>"
pnputil /restart-device "PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>"
```

(`DEV_1069` for the id-41 test VMs; the second restart applies what the first wrote.)
The device-key way, if the service key is not an option:

```
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties" /v MSISupported /t REG_DWORD /d 1 /f
pnputil /restart-device "PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>"
```

and `/d 0` to go back; the next package install rewrites it to 0.

### The flip plan (later, after one hardware run passes)

1. One INF line: `MSISupported, %REG_DWORD%, 1`, still WITHOUT NOCLOBBER. Every install and
   update then writes 1, including over the 0 this package wrote.
2. Installs that must stay on INTx set `MsiMode=1` in the service key first (it survives).
3. The breaker, the latch, the polling safety net and the fallback stay as they are: they are
   what makes the default safe, and they are already exercised by the opt-in.
4. A same-version reinstall does not run the INF; use the `reg add` above or bump the version.

### Affinity

Not shipped: the default machine policy distributes a device's messages, and the
ISR is atomics plus `DxgkCbQueueDpc` (the DPC runs on the CPU that queued it), so
there is nothing to pin for correctness. If the counters (`MsiV1`/`MsiV2` against
`MsiDpc1`/`MsiDpc2`) show the two queues' DPCs landing far from their waiters, the
knobs are `...\Interrupt Management\Affinity Policy\DevicePolicy` (0 machine
default, 1 all close processors, 2 one close processor, 3 all processors, 4
specified processors with `AssignmentSetOverride`, 5 spread messages) and
`DevicePriority`. Unverified whether dxgkrnl honours them for a display miniport;
measure before shipping any.

## Design (smallest safe step, table-driven)

All decisions are in `kmd_logic/src/msi.rs` (host tested); the WDK glue is
`kmd_render/src/virtio/msi.rs`.

1. **Policy at `AddDevice`** (`apply_key_policy`, PASSIVE, own noinline frame): the
   boot-loop breaker's marker is consumed (below), then `MsiMode` (service key) and the
   latch `MsiLatch` give `msi::key_action`: leave the key alone, write 0, or write 1. See
   "Knobs".
2. **Detect** (`probe_granted`, in `StartDevice` before `VirtioGpu::init`,
   own noinline frame): messages were granted if the MSI-X capability's Enable
   bit is set OR the translated resource list has a message interrupt descriptor.
   Either signal alone suffices, because each can lag the other and acting on
   only the late one leaves an enabled device with no vectors. The granted count
   is a LOWER BOUND: the number of message descriptors, never more than the MSI-X
   table size, at least 1 when only Enable is seen. (Whether the OS expresses N
   messages as N descriptors or one descriptor with a count is not assumed; if it
   is one, the plan degrades to a single shared vector, which is still the win.)
3. **Plan** (`msi::plan`): with `granted >= 2`, vector 0 = config, queue `i` =
   vector `i + 1` clamped to the last granted. With `granted == 1`, or the
   `MsiVectors=1` knob, every queue on vector 0 and the config vector unassigned
   (an ISR on a shared message cannot tell a config change from queue work, and
   the ISR-status register that would say is not read in message mode; the
   Conduit device raises no config-change interrupt). A third (RM) queue is a
   larger `queues` argument and the `MAX_QUEUES` table, nothing else. A property
   test asserts no plan ever names a vector `>= granted`.
4. **Program** (`program_vectors`, inside `init` before `DRIVER_OK`): map the
   common cfg a second time, write `msix_config` and each existing queue's
   `queue_msix_vector`, read every one back. `msi::setup_plan` is the order: the
   plan, then (on a refusal) every queue on vector 0 (skipped when the plan
   already was that), then give up. Giving up does NOT fail the transport: every
   vector is unassigned, the start runs POLLING-ONLY (`msi::polling_only_state`,
   `MsiPollOnly=1`, the safety net on from the first moment), and INTx is latched
   for the next start. It cannot fall back to INTx in this start (the line is not
   connected), and failing the transport would lose the display half. Before
   `DRIVER_OK` so QEMU builds the per-queue irqfds from the programmed vectors.
5. **ISR** (`ddi/interrupt.rs`): `AdapterContext::msi_state` (0 = INTx, else bit
   31 | config vector). Message mode: no ISR-status read, count per vector, latch
   `config_change_pending` when the message is the config vector, `DxgkCbQueueDpc`,
   return TRUE. INTx mode: the ISR-status read-to-clear, TRUE only when a status
   bit was set, plus two counter updates.
6. **DPC**: unchanged in what it does. It drains the whole used ring and every
   queue's consumer on every run, so which message fired does not matter. It
   takes the "cause" mask the ISRs left and counts itself per vector.
7. **Boot-loop breaker** (`begin_start`, `service`, `apply_key_policy`): a start that got
   messages sets `MsiStarting=1` in the service key and FLUSHES it to disk before anything
   that could hang (set in `StartDevice` after `probe_granted`, not in `AddDevice`: only a
   message-mode start can be the one that loops, and an INTx start must not leave a marker).
   It is cleared (`msi::marker_may_clear`) once run-time judging is armed, an interrupt has
   been seen, delivery is not convicted, and the start is 3 s old (lazily: not flushed), or at
   once at a clean `StopDevice` (FLUSHED, `service_stop`). If `AddDevice` finds it still set,
   the previous message-mode start never became healthy (a hang, a bugcheck, a reboot into the
   same fault): the breaker trips, `MsiLatch=1` (`MsiLatchWhy=4`), `MsiBreaker` counts it, and
   the key goes to INTx. `MsiMode=2` does NOT override it; only `MsiMode=3` (debugging) does.
   The marker and the latch carry the build that wrote them ("Driver updates and the
   breaker"): only a marker or a latch of the RUNNING build counts.
8. **INF**: ships `MSISupported=0` (no NOCLOBBER) and `MessageNumberLimit=3`.

### Driver updates and the breaker

Seen on hardware (2026-10-07): with MSI-X working (`MsiGrant=3`, `IntxInts=0`), a package
install of the next build (`pnputil /add-driver ... /install` over the running device) left
`MsiLatch=1`, `MsiLatchWhy=4`, `MsiBreaker=1`, and the device stayed on INTx through a reboot.
The breaker had tripped once during the install, and the latch it wrote held for ever: every
driver update could silently cost MSI-X. Two things make a marker or a latch of the build a
driver update REPLACED say nothing about the build it installed: the old image is stopped in
the middle of whatever it was doing, and the latch was a verdict on the old code. So:

* **The build tag.** `msi::build_tag` of `kmd_render/driver-version.env` (the one file the INF
  `DriverVer` and the image `FILEVERSION` come from, read with `include_str!` at compile time):
  `build << 16 | revision` of `HELIOS_KMD_VERSION` (22.22.**346**.**1** = `0x015A0001`). The
  `22.22` prefix is fixed and not part of it. Never 0 (0 is what an absent value reads as); a
  malformed version fails the build. Two builds with the same version number are the same
  build to the breaker: bump the version (the `win_build_kmd` tool does) for anything installed.
* **The marker carries it.** `begin_start` writes `MsiStartingVer` = the tag, then
  `MsiStarting=1`, then flushes. `AddDevice` (`msi::marker_verdict`):

  | `MsiStarting` | `MsiStartingVer` | `MsiMode` | Verdict | What `AddDevice` does |
  | --- | --- | --- | --- | --- |
  | 0 / absent | any | any | Absent | nothing |
  | 1 | any | 3 | Ignored | `MsiStarting=0` (as before: mode 3 has no breaker) |
  | 1 | absent / 0 (an image older than the tag) | 0, 1, 2 | Stale | `MsiStarting=0`, `MsiMarkerOld` + 1, no trip |
  | 1 | another build's tag | 0, 1, 2 | Stale | `MsiStarting=0`, `MsiMarkerOld` + 1, no trip |
  | 1 | the running build's tag | 0, 1, 2 | Trip | `MsiStarting=0`, `MsiBreaker` + 1, latch (`MsiLatchWhy=4`, tagged, flushed) |

  `MsiStartingVer` is left in place after the marker is consumed: it says which build set the
  last marker.
* **The latch carries it.** `latch_intx` (every latch the KMD writes: silent rescues, refused
  vectors, unmapped cfg, the breaker) writes `MsiLatchVer` = the tag, `MsiLatch=1`,
  `MsiLatchWhy`, and flushes. `AddDevice` (`msi::latch_verdict`, BEFORE the marker):

  | `MsiLatch` | `MsiLatchVer` | Verdict | Latched for `key_action` | What `AddDevice` does |
  | --- | --- | --- | --- | --- |
  | 0 / absent | absent / 0 | Clear | no | nothing |
  | 0 | a tag | Clear | no | `MsiLatchVer=0` (an operator wrote 0 instead of deleting: a later hand-set 1 is then the operator's) |
  | 1 | absent / 0 | Operator | YES | nothing: an operator's (or a pre-tag image's) latch is honoured as before |
  | 1 | the running build's tag | Held | YES | nothing |
  | 1 | another build's tag | Stale | no | `MsiLatch=0`, `MsiLatchWhy=0`, `MsiLatchVer=0`, `MsiLatchOld` + 1 |

  The breaker runs after the latch verdict, so a Trip in the same `AddDevice` latches again,
  with the running build's tag.
* **The clear at a clean stop is flushed.** `StopDevice` calls `service_stop`: when it clears
  the marker it flushes the service key there and then (the time is credited to the stop's host
  budget), instead of relying on the lazy writer, `StopFlush` and the stage-5 flush. The
  periodic clear (3 s after a healthy start) stays lazy. A clean stop with NO interrupt seen
  still keeps the marker (`marker_may_clear` is unchanged).

**The consequence.** A new build gets ONE fresh attempt at MSI-X under `MsiMode=2`, whatever
the build before it latched. The breaker and the latch protect against a boot loop (or a
convicted delivery) of THAT build: if the new build loops, its own marker trips its own breaker
on the next `AddDevice` and its own latch holds until the next build or the operator. An
operator's `MsiLatch=1` (written without `MsiLatchVer`) is never set aside.

**What is not covered.** A marker of the RUNNING build still trips, by design: if a build's
own start is cut short before it proves healthy (stopped before `StartDevice` finished, stopped
with no interrupt seen, or a reboot within 3 s of the start with no `StopDevice`), its breaker
trips once and latches that build. Whether the 2026-10-07 trip was the old build's marker or
the new build's own is not known from the counters of that run (the tag did not exist yet);
`MsiStartingVer` next to `MsiBreaker` / `MsiMarkerOld` settles it on the next update. Before
this change, a default `StopFlush=1` already flushed at stage 5 after the clear, so an unflushed
clear explains the trip only for a stop that ended before stage 5 or ran with `StopFlush=0`.

#### Test procedure: a package update over a running MSI-X device

Prerequisites: a snapshot, the two channels of "Minimum safe test procedure" step 0, build N
installed and running on MSI-X under `MsiMode=2` (`MsiGrant=3`, `IntxInts=0`, `MsiLatch=0`,
`MsiStarting=0` a few seconds after the start), and a package of build N+1 (a DIFFERENT
`HELIOS_KMD_VERSION`; the same version is the same build to the breaker). Read the values with
`reg query "HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render"` (they are DWORDs; the
tags read as hex, e.g. `0x15a0001` for 346.1).

1. Before: record `MsiBreaker`, `MsiMarkerOld`, `MsiLatchOld`, `MsiStartingVer`, `MsiLatchVer`.
2. Install N+1 over the running device: `pnputil /add-driver helios_kmd_render.inf /install`.
   Note the exit code (0, or 3010 = reboot required: reboot).
3. After the install's restart (or the reboot): expect `MsiBreaker` unchanged, `MsiLatch=0`,
   `MsiWant=1`, `MsiModeEff=2`. If N predates the tag, or N's marker was still set when N+1's
   `AddDevice` ran: `MsiMarkerOld` + 1 (otherwise unchanged, since N's stop cleared it).
   If N had latched: `MsiLatchOld` + 1 and `MsiLatch=0`. `MsiGrant`: 3 if the `AddDevice`
   write was in time, else 0 (the INF rewrote `MSISupported=0` and the KMD's write of 1 applies
   at the next start, "Can the KMD change it for the start in progress?").
4. One more `pnputil /restart-device` (or a reboot): `MsiGrant=3`, `MsiInts > 0`,
   `IntxInts=0`, `MsiStartingVer` = N+1's tag, `MsiStarting` back to 0 a few seconds after the
   start, `MsiBreaker`, `MsiMarkerOld`, `MsiLatchOld` unchanged from step 3.
5. A same-build loop still latches (rehearsal): set the marker WITH the running tag,
   `reg add ... /v MsiStartingVer /t REG_DWORD /d <tag of N+1> /f` and
   `reg add ... /v MsiStarting /t REG_DWORD /d 1 /f`, restart: `MsiBreaker` + 1, `MsiLatch=1`,
   `MsiLatchWhy=4`, `MsiLatchVer` = N+1's tag, `MsiWant=0`. The same with `MsiStartingVer`
   deleted: `MsiMarkerOld` + 1 and no trip.
6. Back out: `reg delete ... /v MsiLatch /f` (and two restarts), or revert the snapshot.

Operator actions: to FORCE INTx across updates, `MsiMode=1` (or `MsiLatch=1` with no
`MsiLatchVer`: `reg delete ... /v MsiLatchVer /f` then `reg add ... /v MsiLatch /t REG_DWORD /d 1 /f`;
a hand-set latch is never set aside by a newer build). To CLEAR a latch, delete `MsiLatch` (or
write 0; `AddDevice` then drops the stale tag). To disable the breaker while debugging,
`MsiMode=3`.

### Is the shared interrupt code right for MSI?

* **Claim.** INTx: TRUE only when the read-to-clear ISR status had a bit (the line
  is shared; status 0 is another device and counts `IntxMiss`). Message: always
  TRUE. A message is not shared: a message arriving on a vector this device's
  table programmed is this device's.
* **No read-to-clear race.** In message mode the ISR-status register is never
  read (the virtio spec says not to once MSI-X is in use; it would also be an
  MMIO exit per interrupt, the cost removed). Nothing needs acknowledging:
  messages are edge events, there is no level to deassert and no interrupt-storm
  mechanism of the line kind. The one-off read-to-clear in `init` is skipped in
  message mode.
* **Per vector.** Config message (vector 0 when the device has its own): latch
  `config_change_pending`, the DPC wakes the HPD worker. Queue messages (1, 2, or
  the shared 0): just the DPC. Queue to vector mapping is `plan.queue[i]` with the
  device's queue numbers (control = 0, event = 1), programmed and read back.
* **Spurious / no-work vectors.** The ISR touches atomics and `DxgkCbQueueDpc`
  only. A vector that fires with nothing on its queue costs one coalesced DPC that
  finds both rings empty; it is counted (`MsiIdle`), never harmful. A message
  number above the counted ones lands in `MsiVOth` / `MsiDpcOth`. The DPC is
  idempotent: the waiter's polling drain, an earlier coalesced DPC and the worker
  all drain the same rings under `virtio_lock`.
* **Concurrency.** Different messages can run their ISRs on different CPUs at the
  same time. That is safe because the ISR writes only atomics (one cache line
  per vector counter, so they do not bounce) and calls `DxgkCbQueueDpc`, which
  coalesces. `DxgkCbNotifyInterrupt` is still only called from the synchronized
  routine on message 0. Verify under load (checklist G).
* **Before the transport is live** (`msi_state` still 0): the INTx branch finds
  `isr_status == 0` and returns FALSE with no DPC, as under INTx; completions in
  that window are found by the init code's polling.

## Fallback: what happens when MSI-X does not work

| Condition | Detected | Action |
| --- | --- | --- |
| OS granted a line (`MSISupported=0`, no MSI-X capability, no messages offered) | `probe_granted` = 0 | INTx path, as v307. Extra work at start: one config-space capability walk (read-only). This is the shipped default. |
| `MsiMode=1`, or the latch is set | `AddDevice` (`key_action`) | Writes `MSISupported=0`: INTx at the NEXT start. |
| `MsiMode=2` | `AddDevice` | Writes `1` unless the latch is set (the latch and the breaker win), applied at the next start after that. |
| OS granted 1 message / list unparseable | `probe_granted` | One shared vector, config unassigned. |
| OS granted 2 | | Config on 0, both queues on 1. |
| OS granted 3+ | | Config 0, control 1, event 2. |
| Device refuses a vector | read-back in `write_plan` | `setup_plan`: shared on 0. |
| Every plan refused, or the common cfg cannot be mapped | `program_vectors` | POLLING-ONLY: transport up, no vector, `MsiPollOnly=1`, safety net on, `MsiLatch=1` (`MsiLatchWhy` 2 / 3, flushed), INTx next start. Slow (every wait costs a slice, fences ride the worker's 10 ms poll) but alive. |
| A message-mode start of THIS build never became healthy (hang, bugcheck, reboot loop) | `AddDevice` finds `MsiStarting` with `MsiStartingVer` = the running build | Breaker: `MsiLatch=1` (`MsiLatchWhy=4`, `MsiLatchVer`, flushed), `MsiBreaker` + 1, INTx for this start's key write. |
| A driver update: the marker or the latch is another build's (or a pre-tag image's marker) | `AddDevice` (`marker_verdict`, `latch_verdict`) | Set aside: `MsiMarkerOld` / `MsiLatchOld` + 1, no trip, the latch cleared; the new build gets one fresh MSI-X attempt ("Driver updates and the breaker"). |
| Start finished and completions were polled with no interrupt | `finish_start` (`msi::start_verdict`: no interrupt, >= 3 completions since the transport went live) | `MsiStart=2` (suspect), the polling safety net turns on. Never latches by itself: whether dxgkrnl delivers interrupts to a device that is still starting is not assumed. |
| Lost interrupt (delivery broken) | `wait_block`: a polling drain, after a wait slice timed out, found a completion (`IrqRescue`). `msi::rescue_step`: no interrupt since the previous rescue = silent; 3 silent in a row convict | First doubt: polling safety net on, the rescue queues the DPC (so events and fences drain too). Conviction: `MsiHealth=3`, `MsiLatch=1` (`MsiLatchWhy=1`, flushed), INTx next start. The device keeps working meanwhile at polling latency. |
| Interrupt storm | not a message-mode failure | Messages are edge events with no level line: the line-based storm detector (`~10000 unclaimed ISRs -> Code 43`) cannot happen. A device that fires without work is bounded by DPC coalescing and counted in `MsiIdle`. |
| A previous start latched INTx but PnP still gave messages | `on_transport_up` | Polling safety net from the first moment. |

Every latch write is flushed to disk (`flush_service_key`): the faults that set it end in a
hang or a bugcheck, and a latch the lazy writer had not written would go with them.

**The safety net** (`virtio::msi::polling`, `ddi/hpd.rs`, `hpd_wake::WaitInputs::poll`): while on,
the HPD worker (display half) wakes every 10 ms and runs `drain_used_and_complete`, the same
drain the DPC runs, counted in `MsiPollN`. It also runs while a vsync heartbeat is armed (the
vsync DPC drains). A render-only adapter has no worker: there a rescue still queues the DPC, so
the NVRM path (which rescues on every slow call) keeps its events moving, but an idle render-only
adapter with broken delivery is not covered. That is why the conviction latches INTx for the
next start. A suspect verdict (not a conviction, not a latched start) is cleared by the periodic
mirror once interrupts have arrived since the evidence that raised it (`msi::reassess`), and the
net goes off with it: an end-of-start "no interrupts yet" does not keep the worker polling for the
whole run.

**What cannot be done:** switch to INTx inside the start that got messages. The line is not
connected, and an enabled-but-unconnected level interrupt is a hang. The fallback is therefore
"this start degrades to polling, the next start is INTx".

### Is the INTx fallback exercised by tests?

Host tests (`kmd_logic`, `cargo test`) cover every decision: `setup_plan` (order,
no identical retry, never an ungranted vector), `key_action` (the table; only an explicit
opt-in raises the key, a latch beats everything but mode 3), `breaker_trips` and
`marker_may_clear` (the marker clears only with an interrupt seen and a start old enough),
`build_tag` (on samples and on the real `driver-version.env`), `marker_verdict` and
`latch_verdict` (same build trips / holds, another build or no tag on the marker is stale, no
tag on the latch is the operator's, mode 3 ignores the marker, the package-update case keeps
MSI-X),
`polling_only_state`, `Mode::from_knob` (unknown values are `Auto`),
`rescue_step` / `start_verdict` / `polling_wanted` / `should_latch` (the start
verdict never convicts; an interrupt between rescues ends the streak; wrap
tolerant), the vector slots and cause bits, the routing of `isr_route` in both
modes, and the counter-name checks (14-character limit, no collision with another
counter list, the render sources spell exactly the listed names, the knobs are
spelled only in `diag.rs`). What no test reaches is the WDK glue: the INTx path
itself is unchanged code with counters added, and its first test is a boot with
`MsiMode=1` (checklist C).

## Knobs

| Knob (service key `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, DWORD) | Values | Applies |
| --- | --- | --- |
| `MsiMode` (default 0) | 0 = auto: follow the INF / the device key as it stands (INTx in this package); a latch lowers it. 1 = INTx always. 2 = MSI-X opt-in: raises the key to 1, but the latch and the breaker still win. 3 = MSI-X with no breaker and no latch (debugging only). Unknown values are 0. Never written by the driver; mirrored as `MsiModeEff`. | `AddDevice` writes the device key from it: the first restart after a change writes, a second restart or a reboot applies. Survives driver updates. Going back to 0 leaves the key where the forcing put it. |
| `MsiLatch` (default 0) | 1 = a start convicted message delivery, could not set vectors up, or the breaker tripped: the next `AddDevice` asks for INTx. The driver sets it (`MsiLatchWhy`: 1 silent rescues, 2 vectors refused, 3 common cfg unmapped, 4 breaker) together with `MsiLatchVer`; a latch of ANOTHER build is stale and cleared at `AddDevice`. Clear it by deleting it (or 0) once the cause is fixed (mode 2 does not clear it); set it to 1 with no `MsiLatchVer` to force or rehearse the fallback (honoured by every build). | `AddDevice`. |
| `MsiLatchVer` (default absent) | The build tag of the image that wrote the latch; absent = the operator's latch. Written by the driver; delete it when setting `MsiLatch` by hand. | `AddDevice`. |
| `MsiStarting` (default 0) | The breaker's marker (below). The driver sets and clears it; do not set it by hand except to rehearse the breaker (with `MsiStartingVer` = the running build's tag, or it is stale). | `AddDevice`. |
| `MsiStartingVer` (default absent) | The build tag of the image that set the marker. Written by the driver. | `AddDevice`. |
| `MsiBreaker` (default 0) | How many times the breaker tripped (the driver's count). | Read at `AddDevice`. |
| `MsiMarkerOld`, `MsiLatchOld` (default 0) | How many markers / latches of another build `AddDevice` set aside (the driver's counts). | Read at `AddDevice`. |
| `MsiVectors` (default 0, unchanged) | 0 = per-source vectors when enough messages were granted. 1 = one shared message 0 for every queue. NOT a switch to INTx. | Transport init. The same-boot A/B between per-queue and shared vectors. |

Backward compatibility: `MsiVectors` keeps its meaning. The previously documented
way to force INTx, setting the device key's `MSISupported` to 0 by hand, still
works until the next package install rewrites it from the INF; `MsiMode=1` is
the form that survives updates.

## Counters (service key, DWORD)

Written by `publish_nvrm_counters`, the periodic mirror (the HPD worker, rate
limited, plus the present edge and StopDevice): first at the first open or
present, then every 256 forwards and on every session change; and the `Msi*` / `Intx*`
block once more at the end of `StartDevice` (`finish_start`), so the registry shows the
current start at once, zeros included, and not the last one's numbers. Registry writes are
PASSIVE only; the ISR and the DPC touch atomics. The interrupt and DPC counters
and `NvRtt*` are per start (zeroed when the transport goes live, so a restart into
the other mode reads as that mode alone). `IrqN` / `DpcN` / `0x0F0C` / `0x0F0D`
keep their old meaning (all interrupts, all DPCs, cumulative since the image loaded).

| Name | Meaning |
| --- | --- |
| `MsiCap`, `MsiList`, `MsiGrant`, `MsiVec`, `MsiRefused`, `MsiNoCfg` | Set-up breadcrumbs (unchanged): MSI-X control word, descriptors listed, messages planned for (0 = INTx), highest vector used, refusals, cfg unmapped. |
| `MsiInts` | Message interrupts taken, all vectors (sum of the next five). 0 on INTx. |
| `MsiV0` .. `MsiV3`, `MsiVOth` | Interrupts per message number; `MsiVOth` = message 4 and above (should be 0). |
| `MsiDpc0` .. `MsiDpc3`, `MsiDpcOth` | DPCs run per message that queued them. A DPC several messages queued counts for each. `MsiVn` - `MsiDpcn` is coalescing. |
| `IntxInts`, `IntxMiss`, `IntxDpc` | INTx interrupts claimed, not ours (shared line), DPCs they queued. 0 on MSI. |
| `DpcNoCause` | DPCs nothing recorded a cause for: vsync / DMA-completion notifies, `request_wddm_completion_dpc`, a rescue. |
| `MsiIdle` | DPCs a message queued that found both rings empty (spurious, or taken by a waiter's drain or an earlier DPC). |
| `IrqRescue` | Waits whose polling drain found a completion after a slice timeout. Both modes. A healthy run reads 0 or near it. |
| `MsiSilent`, `MsiHealth`, `MsiStart` | Silent rescues in a row now; health (0 unknown, 1 healthy, 2 suspect, 3 broken); the end-of-start verdict (0/1/2). |
| `MsiPoll`, `MsiPollN` | The polling safety net is on; worker wakes that drained under it. |
| `MsiPollOnly` | 1 = this start got messages but no vector could be programmed: transport up, polling only. |
| `MsiStarting`, `MsiBreaker` | The breaker's marker (1 from the start of a message-mode start until it proved healthy) and how many times it tripped. |
| `MsiModeEff`, `MsiWant`, `MsiKeyWr` | `MsiMode` as read; what `AddDevice` asked of `MSISupported` (0xFF = left alone, else the value); the NTSTATUS of that write (0 = done, 0xFFFFFFFF = not attempted). |
| `MsiLatch`, `MsiLatchWhy` | The INTx latch and why (1 silent rescues, 2 refused, 3 cfg unmapped, 4 breaker). |
| `MsiStartingVer`, `MsiLatchVer` | The build tag (`build << 16 \| revision` of `HELIOS_KMD_VERSION`) of the image that set the marker / wrote the latch; absent = an older image or the operator. |
| `MsiMarkerOld`, `MsiLatchOld` | Markers / latches of another build (or a marker without a tag) that `AddDevice` set aside instead of tripping / honouring them. |
| `NvRttN`, `NvRttMinUs`, `NvRttMeanUs`, `NvRttMaxUs` | Forwarded RM `Ioctl` round trips as the calling thread saw them (see below): count, min, mean, max in microseconds. |
| `NvRttB0` .. `NvRttB7` | Histogram of the same, bounds `< 15, 25, 40, 60, 100, 250, 1000` us, last open. The INTx cost (about 55 us) lands in B3, a 25 us MSI-X call in B2. |
| `NvRttON`, `NvRttOMinUs`, `NvRttOMeanUs`, `NvRttOMaxUs` | The same for every other forwarded message: open, close, scan-out flip, the pinned registration, listings. |

**What a round-trip sample is.** From just before the request is queued to just
after the reply is in hand, on the interrupt-time clock (`KeQueryInterruptTimePrecise`,
100 ns): the submit and its doorbell, the pre-wait spin (`NvSpinUs`), the
interrupt, the DPC, `KeSetEvent` and the waiter's wake. A call that got no reply is
not counted. Cost: two clock reads and five relaxed atomics per call. The
statistics are `helios_kmd_logic::nvrm_rtt` (host tested); `virtio::nvrm::timed_roundtrip`
is the one wrapper; there is no registry write on the call path.

**What is not measured.** The ISR-to-DPC latency (a clock read in the ISR is a cost
on the path under measurement), and the event round trip (host-initiated; the
event queue's interrupts are visible in `MsiV2` / `MsiDpc2`).

## Recovery

A wrong MSI-X start can leave a device that does not start, or starts and shows
nothing. The breaker (above) makes the second boot INTx by itself in the cases it can
see (a start that hung or bugchecked before interrupts proved healthy); these are the
steps when it does not. In order of how much still works:

**With a live channel (RDP, SSH, the QEMU monitor's guest agent) into the guest:**

1. `reg add "HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render" /v MsiMode /t REG_DWORD /d 1 /f`
   then `pnputil /restart-device` (twice if the first restart still comes up on
   messages: the first writes the key, the second applies it). Also
   `reg delete ... /v MsiLatchVer /f` and `reg add ... /v MsiLatch /t REG_DWORD /d 1 /f` if you
   want the latch to hold it (a latch without `MsiLatchVer` is the operator's: no build sets it
   aside, while a latch the KMD wrote holds only for the build that wrote it).
   A breaker trip or a latch that came from a driver UPDATE (`MsiLatchVer` = the old build's
   tag) needs nothing: the new build sets it aside at its `AddDevice` (`MsiLatchOld`).
2. If the device is up but slow or stalled: read `MsiHealth` / `MsiLatch` /
   `MsiPollOnly`; the safety net and the latch are automatic.

**Without a channel (black screen, no network):**

3. Revert the VM to the snapshot taken before the test (the procedure below starts with
   one); that is the fast path.
4. Otherwise boot to safe mode (Microsoft Basic Display takes over) or mount the hive
   offline from another VM, and set the device key directly:
   `...\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties\MSISupported` = 0,
   and `MsiMode` = 1 (service key) so a later start does not raise it again.
   Installing this package again also rewrites the key to 0 (the INF, no NOCLOBBER).
5. **Host side**: a VMM that offers no MSI-X capability gives INTx whatever the key says;
   the QEMU `vectors=0` property on the device does the same. Starting the VM with that
   property is a recovery that needs nothing from the guest.

The fallback is written to be robust rather than clever: it never convicts on the
end-of-start verdict, it requires three silent rescues in a row, it never raises the key
on its own, the breaker and every latch are flushed to disk before the fault can take
them (and so is the marker's clear at a clean stop), a failure of any registry write only loses
the latch (the polling net still runs this start), and a marker or a latch is a verdict on the
build that wrote it: a driver update gives the next build one fresh attempt.

## Minimum safe test procedure (MSI-X opt-in)

Do this once, in this order, before anyone flips the INF default.

0. **Prepare.** Snapshot the VM (disk, not just memory). Have TWO ways in that do not
   depend on the display: the console (QEMU monitor / serial / VNC of the emulated
   adapter) AND a non-display channel (RDP-independent: SSH or the QEMU guest agent,
   able to run `reg` and `pnputil`). Check the package is the one with `MSISupported=0`
   (the shipped default), and that `MsiMode`, `MsiLatch`, `MsiStarting` are absent or 0
   (`MsiLatchVer`, `MsiStartingVer` may hold an earlier build's tag; that is harmless).
1. **Baseline A, INTx** (`MsiMode` absent or 1). Render-only first (`DisplayHalf=0`),
   then the display half. Expect `MsiGrant=0`, `IntxInts > 0`, `MsiInts=0`,
   `MsiHealth=0`. Record `NvRttN/MinUs/MeanUs/MaxUs/B0..B7` for the NVK or `crm` smoke
   loop, and the `CRM_WIN_PROF_FILE` table. This also proves the new counters and
   probes did not disturb the INTx path.
2. **Opt in**: `MsiMode=2`, `pnputil /restart-device`. Read `MsiWant=1`, `MsiKeyWr=0`,
   `MsiGrant` (0 is expected: the write came late). Restart AGAIN (or reboot): now
   `MsiGrant` should be 3 (1 or 2 are still correct, see below).
3. **Checks right after the second start** (the registry shows this start at once):
   `MsiCap` (bit 15 set, table size field 2), `MsiList`, `MsiGrant`, `MsiInts` > 0 and
   growing, `MsiV1` and `MsiV2` > 0, `MsiV0` 0, `MsiDpc1`/`MsiDpc2`, `MsiHealth` 0 or 1,
   `MsiStart=1`, `MsiPoll=0`, `MsiPollOnly=0`, `IrqRescue` ~0, `MsiStarting` back to 0 a few
   seconds after start, `MsiBreaker=0`, `MsiLatch=0`, then `NvRtt*` against A. Render-only
   first; only then the display half.
4. **Only if everything above is clean**: run the load (DX12 / Vulkan soak, the NVK frame
   loop) for several minutes and re-read; no `0x119` / `0xD1` / `0x133`.
5. **Back out**: `MsiMode=1` and two restarts, or revert the snapshot.

What each failure looks like: `MsiGrant=0` after two restarts with `MsiWant=1` and
`MsiKeyWr=0` means PnP did not honour the key (look at Device Manager's resources);
`MsiInts=0` with `MsiGrant>0` and `IrqRescue` climbing is silent delivery (the host did not
signal, or the `CM_RESOURCE_INTERRUPT_MESSAGE` value `0x2` in `msi.rs` is wrong): the net keeps
the device alive and the latch makes the next start INTx; a start that hangs leaves
`MsiStarting=1`, and the next boot of the same build trips the breaker (`MsiBreaker=1`,
`MsiLatchWhy=4`).

## Hardware checklist (what each run must settle)

Compare each run's `NvRtt*` and `Msi*` / `Intx*` counters. The loop: the same NVK or `crm`
smoke workload for each run, and the call-time table librmclient writes when
`CRM_WIN_PROF_FILE` is set (per-call times from user mode, which includes the escape and the
ioctl path the KMD counters do not).

A. **Baseline, INTx**: step 1 above.
B. **MSI-X**: step 3 above; the host estimate is 20-35 us less per forward. If `MsiGrant` is 0
   but Device Manager shows an MSI-X IRQ, detection missed.
C. **Same-start or next-start**: after `MsiMode=2` (and again after `MsiMode=1`), does the FIRST
   restart already read the new `MsiGrant`? Record which: it settles whether the `AddDevice`
   write is read in time, and whether one restart is enough.
D. **The latch**: `MsiLatch=1`, restart: `MsiWant=0`. For the polling net itself, a start that got
   messages with the latch set shows `MsiPoll=1` and `MsiPollN` growing.
E. **Fewer vectors**: QEMU `vectors=1` and `vectors=2`: `MsiGrant` 1 / 2, still
   interrupt-driven. `MsiVectors=1` with 3 granted: all on `MsiV0`.
F. **The failure arms**, if a lost interrupt can be provoked (host call fd muted): `IrqRescue`
   climbs, `MsiHealth` 2 then 3, `MsiLatch=1` / `MsiLatchWhy=1`, `MsiPoll=1`, the desktop keeps
   painting at polling latency, the next start is INTx. The refusal arm (`MsiPollOnly=1`) is hard
   to provoke. The breaker: `reg add ... /v MsiStartingVer /t REG_DWORD /d <running tag> /f`
   and `reg add ... /v MsiStarting /t REG_DWORD /d 1 /f`, restart: `MsiBreaker=1`, `MsiLatch=1`,
   `MsiLatchVer` = the tag, `MsiWant=0` (without the tag the marker is stale: `MsiMarkerOld=1`,
   no trip).
J. **A package update** over a running MSI-X device: "Test procedure: a package update over a
   running MSI-X device".
G. **Under load** (a DX12 or Vulkan soak, three messages): no `0x119` / `0xD1` / `0x133`;
   `MsiIdle` small, `DmaNtfF` 0. This settles whether dxgkrnl serialises the ISRs of different
   messages against the synchronized routine on message 0.
H. **The claim the design leans on**: interrupts are delivered during `StartDevice` (`MsiStart=1`
   and `MsiInts > 0` after start; `MsiStart=2` with `MsiGrant > 0` means they are not, or the host
   does not signal).
I. `tools/kmd-frame-sizes.ps1`: new noinline frames `begin_start`, `on_transport_up`,
   `finish_start` (it publishes ~30 counters), `note_rescue`, `apply_key_policy` (a leaf of
   `AddDevice`), and `wait_block` grew by the rescue branch. Compare against the 17936-byte
   ceiling.

## Not done

* Per-queue affinity (`IoConnectInterruptEx` message table / interrupt policy)
  beyond what PnP does by default.
* Reading the config-change status in message mode on a shared message (would
  need a per-DPC device read, which costs the exit this change removes).
* MSI/MSI-X for a device with MSI but no MSI-X: virtio defines only MSI-X.
* A polling net for an idle render-only adapter (no worker, no timer).
* Rewriting the key from the install script: the INF does it on every install and
  update, and the KMD does it for `MsiMode` / the latch; the script would only add
  a way to miss the same-version reinstall, which `reg add` above covers.
* Flipping the shipped default: one INF line after one passing hardware run ("The flip plan").

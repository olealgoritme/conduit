# MSI-X interrupts for the Helios KMD

Status: MSI-X is the shipped DEFAULT (INF `MSISupported=1`), with an INTx fallback and per-vector
counters. NOT verified on hardware: the KMD could not be compiled or run where this was written;
everything marked "verify" is a claim that only a boot can confirm. The recovery path if a start
does not come up is in "Recovery".

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

## What a default flip needs (the exact mechanics)

### The INF values

All under the device's HARDWARE key (`HKLM\SYSTEM\CurrentControlSet\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters`,
which is what `HKR` means in a `DDInstall.HW` section) in
`Interrupt Management\MessageSignaledInterruptProperties`:

| Value | Shipped | Meaning |
| --- | --- | --- |
| `MSISupported` | `1` (REG_DWORD, flag `0x00010001`) | PnP may give the device messages. `0` = the INTx line. |
| `MessageNumberLimit` | `3` (REG_DWORD) | The most messages PnP may grant: config, control queue, event queue. A third (RM) queue is a fourth. Omitted, PnP asks for as many as the device offers. |
| `Affinity Policy\DevicePolicy` / `DevicePriority` | not shipped | See "Affinity". |

`0x00010001` is `FLG_ADDREG_TYPE_DWORD`; `0x00000002` is `FLG_ADDREG_NOCLOBBER`
("do not overwrite a value that exists"). The dormant package wrote
`0x00010003` = DWORD + NOCLOBBER.

### What NOCLOBBER did to existing installs, and why it is gone

NOCLOBBER keeps ANY existing value, including one the INF itself wrote. Every
install made with the dormant package therefore holds an INF-written
`MSISupported=0`, and a new package that still said NOCLOBBER would have left it
at 0 through every update: the flip would have reached new installs only. So the
shipped line has no NOCLOBBER, and:

* **new install:** the `.HW` section creates the keys and writes `1` and `3`;
* **in-place update** (a newer-ranked package selected for the device, by
  `pnputil /add-driver ... /install`, Device Manager, Windows Update): the
  device is re-installed, the `.HW` section runs again, `MSISupported` is
  overwritten with `1`, and PnP restarts the device (or asks for a reboot, exit
  code 3010, which `Install-Helios.ps1` already accepts);
* **same package reinstalled** (`pnputil` answers 259, "already installed"):
  the INF does NOT run again and the old `0` stays. Apply it by hand
  (below) or bump `driver-version.env`;
* **a hand-set 0 does not survive an update any more.** What does survive is the
  service-key knob `MsiMode` and the driver's own latch (below), which the driver
  applies to the device key at `AddDevice`. This is the replacement for
  "NOCLOBBER keeps my test setting".

### Can the KMD flip it for the same start?

Not by a documented mechanism. `MSISupported` is read by the PnP manager (the PCI
bus driver building the interrupt requirements) for a device start. The KMD sees
the PDO in `DxgkDdiAddDevice`, which runs before the requirements are built for
that start, so a write there MAY be read by the start that follows, but Windows
documents no such ordering. The driver treats the write as guaranteed for the
NEXT start and as a bonus for the current one, and always follows what PnP
actually granted (`probe_granted`). `MsiWant` (what `AddDevice` asked for) next
to `MsiGrant` (what PnP gave) is the measurement of whether the write came in
time (verify, checklist D).

### Exact registry steps (package or install script)

New installs and updates: nothing beyond the INF. By hand, on a device that
holds `0` (same-version reinstall, or an install made by hand):

```
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties" /v MSISupported /t REG_DWORD /d 1 /f
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties" /v MessageNumberLimit /t REG_DWORD /d 3 /f
pnputil /restart-device "PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>"
```

(`DEV_1069` for the id-41 test VMs.) The service-key way, which needs no device
key and survives updates: `reg add "HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render" /v MsiMode /t REG_DWORD /d 2 /f`
(force MSI-X) then restart the device; the driver writes `MSISupported=1` at
`AddDevice`.

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

1. **Policy at `AddDevice`** (`apply_key_policy`, PASSIVE, own noinline frame):
   `MsiMode` (service key, below) and the latch `MsiLatch` give
   `msi::key_action`: leave the key alone, write 0, or write 1. See "Knobs".
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
   already was that), then give up. Giving up fails the transport
   (`StartDevice` continues render-only, `StVio`) and latches INTx for the next
   start; it cannot fall back to INTx in this start (the line is not connected).
   Before `DRIVER_OK` so QEMU builds the per-queue irqfds from the programmed
   vectors.
5. **ISR** (`ddi/interrupt.rs`): `AdapterContext::msi_state` (0 = INTx, else bit
   31 | config vector). Message mode: no ISR-status read, count per vector, latch
   `config_change_pending` when the message is the config vector, `DxgkCbQueueDpc`,
   return TRUE. INTx mode: the ISR-status read-to-clear, TRUE only when a status
   bit was set, plus two counter updates.
6. **DPC**: unchanged in what it does. It drains the whole used ring and every
   queue's consumer on every run, so which message fired does not matter. It
   takes the "cause" mask the ISRs left and counts itself per vector.
7. **INF**: ships `MSISupported=1` (no NOCLOBBER) and `MessageNumberLimit=3`.

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
| OS granted a line (`MSISupported=0`, no MSI-X capability, no messages offered) | `probe_granted` = 0 | INTx path, as v307. Extra work at start: one config-space capability walk (read-only). |
| `MsiMode=1`, or the latch is set | `AddDevice` (`key_action`) | Writes `MSISupported=0`: INTx at the NEXT start (this one if PnP had not read it yet). |
| OS granted 1 message / list unparseable | `probe_granted` | One shared vector, config unassigned. |
| OS granted 2 | | Config on 0, both queues on 1. |
| OS granted 3+ | | Config 0, control 1, event 2. |
| Device refuses a vector | read-back in `write_plan` | `setup_plan`: shared on 0; refused again, or the common cfg cannot be mapped: transport fails (render-only, `StVio`), `MsiLatch=1` with `MsiLatchWhy` 2 / 3 (`MsiRefused`, `MsiNoCfg`), INTx next start. |
| Start finished and completions were polled with no interrupt | `finish_start` (`msi::start_verdict`: no interrupt, >= 3 completions since the transport went live) | `MsiStart=2` (suspect), the polling safety net turns on. Never latches by itself: whether dxgkrnl delivers interrupts to a device that is still starting is not assumed. |
| Lost interrupt (delivery broken) | `wait_block`: a polling drain, after a wait slice timed out, found a completion (`IrqRescue`). `msi::rescue_step`: no interrupt since the previous rescue = silent; 3 silent in a row convict | First doubt: polling safety net on, the rescue queues the DPC (so events and fences drain too). Conviction: `MsiHealth=3`, `MsiLatch=1` (`MsiLatchWhy=1`), INTx next start. The device keeps working meanwhile at polling latency. |
| Interrupt storm | not a message-mode failure | Messages are edge events with no level line: the line-based storm detector (`~10000 unclaimed ISRs -> Code 43`) cannot happen. A device that fires without work is bounded by DPC coalescing and counted in `MsiIdle`. |
| A previous start latched INTx but PnP still gave messages | `on_transport_up` | Polling safety net from the first moment. |

**The safety net** (`virtio::msi::polling`, `ddi/hpd.rs`): while on, the HPD worker
(display half) wakes every 10 ms and runs `drain_used_and_complete`, the same drain
the DPC runs, counted in `MsiPollN`. It also runs while a vsync heartbeat is
armed (the vsync DPC drains). A render-only adapter has no worker: there a
rescue still queues the DPC, so the NVRM path (which rescues on every slow call)
keeps its events moving, but an idle render-only adapter with broken delivery is
not covered. That is why the conviction latches INTx for the next start. A suspect
verdict (not a conviction, not a latched start) is cleared by the periodic mirror
once interrupts have arrived since the evidence that raised it (`msi::reassess`), and
the net goes off with it: an end-of-start "no interrupts yet" does not keep the worker
polling for the whole run.

**What cannot be done:** switch to INTx inside the start that got messages. The
line is not connected, and an enabled-but-unconnected level interrupt is a hang.
The fallback is therefore "this start degrades to polling, the next start is INTx".

### Is the INTx fallback exercised by tests?

Host tests (`kmd_logic`, `cargo test`) cover every decision: `setup_plan` (order,
no identical retry, never an ungranted vector), `key_action` (the table; `Auto`
never raises the key), `Mode::from_knob` (unknown values are `Auto`),
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
| `MsiMode` (default 0) | 0 = auto: MSI-X as the INF ships it, INTx once the latch is set. 1 = INTx always. 2 = MSI-X always (ignores and clears the latch). Unknown values are 0. Never written by the driver; mirrored as `MsiModeEff`. | `AddDevice` writes the device key from it; next device start (`pnputil /restart-device`). Survives driver updates. |
| `MsiLatch` (default 0) | 1 = a start convicted message delivery (or could not set it up): the next `AddDevice` asks for INTx. The driver sets it (`MsiLatchWhy`: 1 silent rescues, 2 vectors refused, 3 common cfg unmapped). Clear it with 0 (or `MsiMode=2`) once the cause is fixed; set it to 1 to rehearse the fallback. | `AddDevice`. |
| `MsiVectors` (default 0, unchanged) | 0 = per-source vectors when enough messages were granted. 1 = one shared message 0 for every queue. NOT a switch to INTx. | Transport init. The same-boot A/B between per-queue and shared vectors. |

Backward compatibility: `MsiVectors` keeps its meaning. The previously documented
way to force INTx, setting the device key's `MSISupported` to 0 by hand, still
works until the next package install or update rewrites it to 1; `MsiMode=1` is
the form that survives updates.

## Counters (service key, DWORD)

Written by `publish_nvrm_counters`, the periodic mirror (the HPD worker, rate
limited, plus the present edge and StopDevice): first at the first open or
present, then every 256 forwards and on every session change. Registry writes are
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
| `MsiModeEff`, `MsiWant`, `MsiKeyWr` | `MsiMode` as read; what `AddDevice` asked of `MSISupported` (0xFF = left alone, else the value); the NTSTATUS of that write (0 = done, 0xFFFFFFFF = not attempted). |
| `MsiLatch`, `MsiLatchWhy` | The INTx latch and why. |
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

A wrong MSI-X default can leave a device that does not start, or starts and
shows nothing. In order of how much still works:

1. **The device starts but is slow or stalls**: the safety net and the latch are
   automatic; read `MsiHealth` / `MsiLatch`. Restart the device
   (`pnputil /restart-device`) and the next start is INTx.
2. **The device does not start** (Code 43 / black screen, a remote session still
   up): `reg add "HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render" /v MsiMode /t REG_DWORD /d 1 /f`
   then `pnputil /restart-device`; `AddDevice` writes `MSISupported=0`. If the
   first restart still comes up on messages (the write came late), restart again.
3. **No session at all**: boot to safe mode (or mount the hive offline) and set
   the device key directly: `...\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties\MSISupported` = 0,
   and `MsiMode` = 1 so an update does not undo it. Device Manager "Roll Back
   Driver" to a package with `MSISupported=0` also works (the INF rewrites it).
4. **Host side**: a VMM that offers no MSI-X capability gives INTx whatever the
   key says; the QEMU `vectors=0` property on the device does the same.

The fallback is written to be robust rather than clever: it never convicts on
the end-of-start verdict, it requires three silent rescues in a row, it never
raises the key on its own, and a failure of any registry write only loses the
latch (the polling net still runs this start).

## Must be verified on hardware, in this order

Compare each run's `NvRtt*` and `Msi*` / `Intx*` counters. The loop: the same NVK
or `crm` smoke workload for each run, and the call-time table librmclient writes when
`CRM_WIN_PROF_FILE` is set (per-call times from user mode, which includes the
escape and the ioctl path the KMD counters do not).

A. **Baseline, INTx** (`MsiMode=1`, restart the device; expect `MsiGrant=0`,
   `IntxInts > 0`, `MsiInts=0`, `MsiHealth=0`): record `NvRttMeanUs`, `NvRttMinUs`,
   `NvRttMaxUs`, `NvRttB0..B7`, the `CRM_WIN_PROF_FILE` table, `IrqN`. This also
   proves the new counters and the probes did not disturb the INTx path.
B. **MSI-X default** (`MsiMode=0` after a driver install that wrote
   `MSISupported=1`, or `MsiMode=2` after A; restart): read `MsiCap` (bit 15 set,
   table size field 2), `MsiList`, `MsiGrant` (3 if the OS lists one descriptor per
   message, 1 if one for the whole set: still correct, single vector).
   `MsiV1` and `MsiV2` grow, `MsiV0` stays 0 (no config interrupts), `MsiDpc1` /
   `MsiDpc2` track them, `MsiStart=1`, `MsiHealth` 0 or 1, `IrqRescue` ~0,
   `MsiPoll=0`. Then the same workload: `NvRttMeanUs` against run A; the host
   estimate is 20-35 us less per forward. If `MsiGrant` is 0 but Device Manager
   shows an MSI-X IRQ, detection missed (the `CM_RESOURCE_INTERRUPT_MESSAGE`
   value `0x2` in `msi.rs` is wrong): control round trips still complete by polling
   but `MsiInts` stays 0 and `IrqRescue` climbs.
C. **Forced INTx by the knob**: from B, `MsiMode=1`, `pnputil /restart-device`.
   `MsiWant=0`, `MsiKeyWr=0`; `MsiGrant`: 0 at the first restart means the
   `AddDevice` write was in time, 3 means PnP had read the value already and the
   NEXT restart is INTx (then do it once more and confirm 0). Record which.
D. **The latch**: `MsiMode=0`, `MsiLatch=1`, restart: `MsiWant=0`, as C. Then
   `MsiMode=2`: `MsiWant=1`, `MsiLatch` back to 0. For the polling net itself, a
   start that got messages with the latch set shows `MsiPoll=1` and `MsiPollN`
   growing.
E. **Fewer vectors**: QEMU `vectors=1` and `vectors=2`: `MsiGrant` 1 / 2, still
   interrupt-driven. `MsiVectors=1` with 3 granted: all on `MsiV0`.
F. **The failure arms**, if a lost interrupt can be provoked (host call fd muted):
   `IrqRescue` climbs, `MsiHealth` 2 then 3, `MsiLatch=1` / `MsiLatchWhy=1`,
   `MsiPoll=1`, the desktop keeps painting at polling latency, and the next start
   is INTx. The refusal arm is hard to provoke.
G. **Under load** (a DX12 or Vulkan soak, three messages): no `0x119` / `0xD1` /
   `0x133`; `MsiIdle` small, `DmaNtfF` 0. This
   settles whether dxgkrnl serialises the ISRs of different messages against the
   synchronized routine on message 0.
H. **The claim the design leans on**: interrupts are delivered during
   `StartDevice` (`MsiStart=1` and `MsiInts > 0` after start; `MsiStart=2` with
   `MsiGrant > 0` means they are not, or the host does not signal).
I. `tools/kmd-frame-sizes.ps1`: new noinline frames `on_transport_up`,
   `finish_start`, `note_rescue`, `apply_key_policy` (a leaf of `AddDevice`), and
   `wait_block` grew by the rescue branch. Compare against the 17936-byte ceiling.

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

# MSI-X interrupts for the Helios KMD

Status: implemented, NOT verified on hardware. The KMD could not be compiled or
run where this was written; everything below marked "verify" is a claim that
only a boot can confirm.

## Why

Measured on v307 (INTx, `MSISupported=0`): one forwarded RM control costs 58 us
against 1.4 us native, the event round trip 142 us against 7.9. The host side
estimates INTx at 20-35 us per forward: QEMU cannot use an irqfd with INTx, so
every completion goes through QEMU's main loop, plus an exit for the ISR-status
read-to-clear and a line deassert. With MSI-X, vhost-user's per-queue call
eventfd becomes a KVM irqfd and a used-ring notification goes straight into the
guest.

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
  `Interrupt Management\MessageSignaledInterruptProperties\MSISupported`, before
  `StartDevice`. The INF must therefore say 1. The driver cannot opt in or out at
  runtime; it can only follow what it was given.

## Design (smallest safe step, table-driven)

All decisions are in `kmd_logic/src/msi.rs` (host tested, 14 tests); the WDK
glue is `kmd_render/src/virtio/msi.rs`.

1. **Detect** (`probe_granted`, in `StartDevice` before `VirtioGpu::init`,
   own noinline frame): messages were granted if the MSI-X capability's Enable
   bit is set OR the translated resource list has a message interrupt descriptor.
   Either signal alone suffices, because each can lag the other and acting on
   only the late one leaves an enabled device with no vectors. The granted count
   is a LOWER BOUND: the number of message descriptors, never more than the MSI-X
   table size, at least 1 when only Enable is seen. (Whether the OS expresses N
   messages as N descriptors or one descriptor with a count is not assumed; if it
   is one, the plan degrades to a single shared vector, which is still the win.)
2. **Plan** (`msi::plan`): with `granted >= 2`, vector 0 = config, queue `i` =
   vector `i + 1` clamped to the last granted. With `granted == 1`, or the
   `MsiVectors=1` knob, every queue on vector 0 and the config vector unassigned
   (an ISR on a shared message cannot tell a config change from queue work, and
   the ISR-status register that would say is not read in message mode; the
   Conduit device raises no config-change interrupt). A third (RM) queue is a
   larger `queues` argument and the `MAX_QUEUES` table, nothing else. A property
   test asserts no plan ever names a vector `>= granted`.
3. **Program** (`program_vectors`, inside `init` before `DRIVER_OK`): map the
   common cfg a second time (same length as the transport, so the MMIO cache
   returns the same mapping), write `msix_config` and each existing queue's
   `queue_msix_vector`, read every one back. A refusal retries once with every
   queue on vector 0; a second refusal fails the transport (`StartDevice`
   continues render-only, as for any transport failure) rather than leaving a
   device on messages that never interrupts. Before `DRIVER_OK` so QEMU builds
   the per-queue irqfds from the programmed vectors.
4. **ISR** (`ddi/interrupt.rs`): `AdapterContext::msi_state` (0 = INTx, else bit
   31 | config vector). Message mode: no ISR-status read, count, latch
   `config_change_pending` when the message is the config vector, `DxgkCbQueueDpc`,
   return TRUE. INTx mode: unchanged, byte for byte.
5. **DPC**: unchanged. It already drains the whole used ring and every queue's
   consumer on every run, so which message fired does not matter.
6. **INF**: `MSISupported=1`, `MessageNumberLimit=3`.

## Fallback matrix

| Situation | What happens |
| --- | --- |
| OS granted a line (no Enable, no message descriptors) | INTx path, as v307. Extra work at start: one config-space capability walk (read-only). |
| `MSISupported=0` in the device key (the force-INTx knob, below) | Same as above. |
| Device has no MSI-X capability (e.g. a fixed-BAR VMM without it) | `probe_granted` returns 0 without any callback; INTx path. |
| OS granted 1 message / list unparseable | One shared vector, config unassigned. |
| OS granted 2 | Config on 0, both queues on 1. |
| OS granted 3+ | Config 0, control 1, event 2. |
| Device refuses a vector | Retry shared on 0; if refused again, transport init fails cleanly (render-only adapter, `StVio`, breadcrumb `MsiRefused=2`). |
| MSI delivery silently broken | Synchronous control waits re-drain the used ring every wait slice (`ctrl.rs wait_block`), so RM/ctrl round trips still finish, just at polling latency; WDDM fence completions and the HPD wake rely on the DPC and would stall. `MsiInts` staying 0 is the tell. |

## Knobs

* **Force INTx (bisect a boot failure).** This is a PnP decision, so it is the
  device key, not the service key:

  ```
  reg add "HKLM\SYSTEM\CurrentControlSet\Enum\PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties" /v MSISupported /t REG_DWORD /d 0 /f
  pnputil /restart-device "PCI\VEN_1AF4&DEV_106D&SUBSYS_11001AF4&REV_01\<instance>"
  ```

  A driver install/update re-applies the INF and sets it back to 1. If the boot
  hangs before this is possible, install a package whose INF says 0.
  There is deliberately no service-key knob for this: once the OS has connected
  messages, the INTx line is not connected, so a driver that "chose INTx" anyway
  would never be woken.
* `MsiVectors` (service key, `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render\Parameters`,
  DWORD, default 0): 1 forces one shared message 0 for every queue even when more
  were granted. The same-boot A/B between per-queue and shared vectors.

## Breadcrumbs (registry diag ring, named values)

`MsiCap` (MSI-X Message Control word), `MsiList` (message descriptors seen),
`MsiGrant` (count planned for; 0 = INTx), `MsiVec` (highest vector used, 0xFFFF
none), `MsiRefused` (1 = first plan refused, 2 = shared retry refused too),
`MsiNoCfg` (common cfg could not be mapped), `MsiInts` (interrupts taken in
message mode; dumped at DestroyDevice).

## Must be verified on hardware, in this order

1. **Boot with `MSISupported=0`** on the new binary: behaves as v307 (`MsiGrant=0`,
   `MsiInts=0`). This proves the probes did not disturb the INTx path.
2. **Boot with the new INF.** Read `MsiCap`/`MsiList`/`MsiGrant`. Expect `MsiCap`
   with bit 15 set and table size field 2 (3 entries); `MsiGrant` 3 if the OS
   lists one descriptor per message, 1 if it lists one descriptor for the whole
   set (still correct, single vector).
   * If `MsiGrant` is 0 but `DEVPKEY_PciDevice_InterruptSupport`/Device Manager
     "Resources" shows an MSI-X IRQ: the detection missed. That means MSI-X is
     enabled after `StartDevice` AND the resource list flag value
     (`CM_RESOURCE_INTERRUPT_MESSAGE = 0x2`, hard-coded in `msi.rs`) is wrong.
     The device then has no vectors: control round trips still complete (polling)
     but `MsiInts` stays 0.
3. `MsiInts` grows while running; `INT_ROUTINE_COUNT` (`0x0F0C`) tracks it.
4. **The claim the whole design leans on:** interrupts are delivered during
   `StartDevice` (the venus bring-up waits depend on it today under INTx; under
   MSI it is the same dxgkrnl connect, but confirm `MsiInts > 0` after start).
5. The refusal arm: hard to provoke (QEMU `vectors=1`, which gives
   `MsiGrant` 1 and still must work: one shared vector).
6. `tools/kmd-frame-sizes.ps1` (new chains for `probe_granted`/`listed_messages`
   and `program_vectors`). `DXGK_DEVICE_INFO` is the only sizeable local.
7. Whether dxgkrnl serialises ISRs of different messages against
   `DxgkCbSynchronizeExecution(MessageNumber=0)`. The ISR here is
   concurrency-safe by construction (atomics + `DxgkCbQueueDpc` only), and
   `DxgkCbNotifyInterrupt` is still only called from the synchronized routine on
   message 0, so the existing contract is unchanged; confirm no `0x119`/`0xD1`
   under load with three messages.
8. Latency: RM control forward 58 us and event round trip 142 us against the
   v307 numbers. Expect the gain only if QEMU actually installed irqfds
   (`virtio_pci_set_guest_notifiers` runs at `DRIVER_OK`).

## Not done

* Per-queue affinity (`IoConnectInterruptEx` message table / interrupt policy)
  beyond what PnP does by default.
* Reading the config-change status in message mode on a shared message (would
  need a per-DPC device read, which costs the exit this change removes).
* MSI/MSI-X for a device with MSI but no MSI-X: virtio defines only MSI-X.

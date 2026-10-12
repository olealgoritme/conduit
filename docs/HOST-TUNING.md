# Host tuning

A VM that stutters now and then (short freezes, uneven frame times,
benchmark numbers that swing between runs) is usually starved by the host,
not slowed by the GPU path. These settings keep the guest's CPUs and memory
to itself. Host isolation is on by default; none of the rest is required
to run Conduit. They matter for smooth desktops, games and repeatable
benchmarks.

The numbers below are from the reference host (Ryzen 9 7950X, 16 cores /
32 threads, 32 GB RAM, RTX 5090, a 16 vCPU Windows 11 guest). Scale them to
yours.

## Memory: never swap guest RAM

The guest's RAM is a shared memfd (the backend reads it). Like any shared
memory it can be swapped out, and a guest that touches a swapped-out page
stops until the host reads it back. On a host short of RAM this shows up as
random stalls in the guest even when the host looks idle.

1. **Leave the host enough RAM.** Guest RAM plus roughly 12-14 GB for the
   host desktop, `conduit-venus` and builds. On a 32 GB host that means a
   16 GB guest, not 20 GB.
2. **Huge pages for the guest** (2 MiB, reserved): they cannot be swapped
   and cut the guest's TLB misses. Reserve them at boot, when memory is not
   yet fragmented (at runtime a busy host often has only a few GB of 2 MiB
   blocks left, even with plenty free):

   ```sh
   # 16 GiB = 8192 x 2 MiB; match the guest's <memory>
   echo 'vm.nr_hugepages = 8192' | sudo tee /etc/sysctl.d/61-conduit-hugepages.conf
   ```

   and in the domain:

   ```xml
   <memoryBacking>
     <hugepages><page size='2048' unit='KiB'/></hugepages>
     <source type='memfd'/>
     <access mode='shared'/>
   </memoryBacking>
   ```

   The reserved pages are taken from the host for as long as they are
   reserved, whether the VM runs or not (`sudo sysctl vm.nr_hugepages=0`
   gives them back).
   This matters most for CPU-bound games: on 4 KiB host pages every guest TLB
   miss walks two levels of 4 KiB tables, and a game's render thread spends
   much of its time on such loads (CS2 on the reference host, 16 GiB guest
   entirely on 4 KiB pages: the hottest instructions of the D3D11 runtime and
   the UMD were all loads of a cold pointer). `conduit doctor NAME` prints how
   much of a running VM's RAM sits on 2 MiB pages.
3. **Without reserved huge pages**, let shared memory use transparent huge
   pages (this needs Conduit's QEMU with `host/qemu` patch 0009, which puts the
   guest-RAM memfd on a 2 MiB boundary; stock QEMU maps it at any 4 KiB
   boundary, where no huge page fits). They are best effort and can still be
   swapped; on a host that has
   been up a while most allocations fall back to 4 KiB (check with `conduit
   doctor NAME`, or `grep thp_file_fallback /proc/vmstat`):

   ```sh
   echo 'w /sys/kernel/mm/transparent_hugepage/shmem_enabled - - - - within_size' \
     | sudo tee /etc/tmpfiles.d/conduit-thp.conf
   sudo systemd-tmpfiles --create /etc/tmpfiles.d/conduit-thp.conf
   ```

4. **Keep swap, swap less, swap to RAM first.** Do not turn swap off: with
   none, the kernel's OOM killer picks the largest process when memory runs
   out, which is QEMU. Instead:

   ```sh
   echo 'vm.swappiness = 10' | sudo tee /etc/sysctl.d/60-conduit-vm.conf
   sudo sysctl -p /etc/sysctl.d/60-conduit-vm.conf
   # compressed swap in RAM, used before the swap file
   sudo apt install systemd-zram-generator
   printf '[zram0]\nzram-size = ram / 4\ncompression-algorithm = zstd\nswap-priority = 100\n' \
     | sudo tee /etc/systemd/zram-generator.conf
   sudo systemctl daemon-reload && sudo systemctl start dev-zram0.swap
   ```

5. **Protect QEMU from the OOM killer**, so a runaway build dies instead of
   the VM (after every start):

   ```sh
   sudo choom -p "$(pgrep -f 'qemu-system-x86_64.*guest=NAME')" -n -900
   ```

Check: `vmstat 2` should show `si`/`so` near 0 while the guest runs, and
`grep HugePages_ /proc/meminfo` the reserved pages in use.

## CPU: give the guest its own cores

- **Pin every vCPU** to one host thread, keeping SMT siblings together
  (vCPU 0/1 = core 8's two threads, and so on). On a two-CCD Ryzen keep the
  guest on one CCD so its threads share one L3:

  ```xml
  <vcpu placement='static'>16</vcpu>
  <cputune>
    <vcpupin vcpu='0' cpuset='8'/>   <vcpupin vcpu='1' cpuset='24'/>
    <!-- ... -->
    <vcpupin vcpu='14' cpuset='15'/> <vcpupin vcpu='15' cpuset='31'/>
    <emulatorpin cpuset='0-7,16-23'/>
    <iothreadpin iothread='1' cpuset='0-7,16-23'/>
  </cputune>
  <cpu mode='host-passthrough'>
    <topology sockets='1' dies='1' cores='8' threads='2'/>
  </cpu>
  ```

  `lscpu -e` shows which threads are siblings and which share an L3.
- **Pin QEMU's own threads and the disk I/O elsewhere**: `emulatorpin`,
  and one `iothread` used by the virtio disks
  (`<driver ... iothread='1'/>`), both on the host's cores.
- **Keep host work off the guest's cores.** Pinning only says where the
  vCPUs may run, not that nothing else runs there. Conduit's host isolation
  (below, on by default) keeps your session, system services, its own
  helpers, device interrupts and kernel workqueues off the VM's cores while
  it runs. Heavy host jobs still compete with the rest of the host; run them
  at low priority:

  ```sh
  taskset -c 0-7,16-23 nice -n 19 cargo build -j8
  ```
- **Keep the backend and conduit-venus off the guest's cores.** Host
  isolation does this (their units are in Conduit's slices, which it
  restricts). `conduit config set backend.cpus 0-3,16-19` narrows them
  further, to part of the host's cores (four cores and their SMT siblings on
  the reference host; applies at the next backend start). Unpinned and
  without isolation, their threads wake mostly on the guest's idle CPUs and
  preempt a vCPU there
  ([research/host-roundtrip-latency.md](research/host-roundtrip-latency.md)).
  This applies with MSI-X in the guest. A guest on INTx has every completion
  raised by QEMU's main loop, which libvirt pins to the same host CPUs
  (`emulatorpin`), and sharing them there gave millisecond tails. On INTx,
  leave `backend.cpus` unset or keep it clear of the emulator CPUs.
- **CPU governor**: `performance` avoids clock ramp-up latency.
- **Idle vCPUs (optional, for a dedicated set of cores).** A game hands work
  between its threads (render thread, DXVK's CS and submit threads) many
  thousand times a second. Each hand-off to a thread whose vCPU is idle is a
  HLT exit on that vCPU and, on AMD with AVIC, an `avic_incomplete_ipi` exit
  on the waking one (on the reference host at a light load: about 12,000 HLT
  and 2,800 incomplete-IPI exits a second; under CS2 the render thread spent
  about 2% of its time in `NtAlertThreadByThreadIdEx`). With pinned vCPUs,
  `-overcommit cpu-pm=on` (libvirt: `<qemu:commandline>`) lets the guest idle
  in place without exits, so a wake is a posted interrupt. The cost: the
  guest's host threads read as 100% busy and nothing else can use those
  cores while the VM runs, idle or not; on the reference host it gave CS2 no
  measurable gain. KVM's halt polling
  (`/sys/module/kvm/parameters/halt_poll_ns`, default 200000 ns) spins an idle
  vCPU a little before it sleeps; keep the default: at 2000000 ns CS2's demo
  playback on the reference host had a lower average and a slowest second
  about half as fast (the spinning vCPUs cost the busy cores clock speed).
  `sudo perf kvm stat live -p $(pgrep -f 'guest=NAME')` shows the exit mix.

## CPU: host isolation

While a libvirt VM runs, Conduit keeps host work off its CPUs: your login
session and its apps, system services, Conduit's own helpers (backend,
conduit-venus, virtiofsd), device interrupts and the kernel's unbound
workqueues run on the CPUs no running VM holds. It is on by default
(`host.isolation auto`) and applies at every start of the VM, however it was
started (virt-manager, virsh, `conduit up`/`view`), and is undone when the
VM stops.

**Which CPUs.** With `auto`:

| CPU | the VM gets | the host keeps |
|---|---|---|
| vCPUs pinned 1:1 in the domain (`<vcpupin>`) | those cores (every SMT thread of them) | the rest |
| several L3 domains (AMD CCDs/CCXs) | the CCD with the larger L3 (X3D), else the one the firmware ranks fastest (`amd_pstate_prefcore_ranking`, ACPI CPPC) | the other CCD(s) |
| Intel hybrid | P-cores | E-cores |
| several NUMA nodes | the node without CPU 0 | the other node(s) |
| one L3, one node | cores from the top | CPU 0 and up |

Placement takes whole cores, as many as the VM has vCPUs, spilling into the
next domain only when one does not hold the VM. The host always keeps at
least two whole cores or a quarter of the cores, whichever is more; a VM
that does not fit is not isolated (`conduit doctor NAME` says why). A
second VM gets CPUs no other running VM holds.

On the reference host the 16-vCPU guest is pinned to CCD1, so:

```
conduit doctor win11
[  ok  ] Host isolation: auto → VM on CCD1 (8-15,24-31), host on 0-7,16-23; applied ✓
[  ok  ] vCPU pinning: 1:1 on the VM's cores, as host isolation wants
```

**vCPU pinning.** QEMU's vCPU threads are pinned by libvirt. A domain
without `<vcpupin>` is pinned live at each start (`virsh vcpupin --live`, and
`emulatorpin` to the host's CPUs; the definition is not changed). `conduit
attach NAME` (or `conduit libvirt enable NAME` for a VM Conduit made) writes
that pinning into the definition: `<vcpupin>` 1:1, `<emulatorpin>` and
`<iothreadpin>` on the host's CPUs. When the domain's own pinning disagrees
with the policy (it spans CCDs, or uses one thread of each core), `conduit
doctor` proposes a re-pin and `conduit attach` applies it, saving the
definition from before as `vms/NAME/libvirt-pre-pin-TIME.xml`; it applies at
the next start. `conduit detach` puts the original pinning back.

**What is changed, and what never is.**

- systemd `AllowedCPUs=` (runtime only) on `system.slice`, your login
  session scopes, and the units of your user manager (apps, terminals,
  Conduit's slices);
- every device interrupt's `/proc/irq/N/smp_affinity_list`, and
  `/proc/irq/default_smp_affinity` for new ones. Per-CPU and kernel-managed
  interrupts (NVMe and multi-queue NIC queues, spread over all CPUs by their
  driver) refuse a new affinity and stay as they are;
- `/sys/devices/virtual/workqueue/cpumask`;
- irqbalance, when it runs, is stopped while any VM is isolated (it would
  move the interrupts back) and started again after the last one stops.

A cgroup holding a VM's QEMU, or the libvirt daemon that starts them, is
never restricted (its vCPU pinning would stop working); `machine.slice`
(system libvirt's VMs) is never touched. Every value is saved before its
first change and put back exactly when the last isolated VM stops. A unit or
scope created while the VM runs, outside the restricted slices, is not
covered until the next start.

**How it runs.** The backend's unit starts `conduit _isolation start NAME`
with each VM start; it waits for the domain to run, then:

1. starts `conduit-isolation@UID_CPUS.service` (for example
   `conduit-isolation@1000_8-15_24-31.service`), a root oneshot unit with
   `RemainAfterExit=yes` installed by the package, which applies the system
   part (`conduit _isolation apply`) and keeps its saved values in
   `/run/conduit-isolation/state.json`. A polkit rule
   (`/usr/share/polkit-1/rules.d/50-conduit-isolation.rules`) lets a local,
   active user start and stop exactly these units, so there is no password
   prompt. The unit checks the instance name and always leaves the host at
   least one whole core;
2. restricts your user manager's units (`systemctl --user set-property
   --runtime`), saving their values in `$XDG_RUNTIME_DIR/conduit/isolation.json`.

It writes one line to `conduit logs NAME backend`, for example
`host isolation: auto → VM on CCD1 (8-15,24-31), host on 0-7,16-23; applied
(yours: app.slice, conduit.slice, session.slice; ...)`; the system part logs
to the journal (`journalctl -u 'conduit-isolation@*'`). The backend's
`_stopped` step and `conduit down` revert it. Both ends can run twice, and a
start first reverts what a VM that is no longer running left behind, so a
crash is cleaned up at the next start at the latest. Without the system
unit (a source build), only your own processes are moved and `conduit
doctor` says so.

**Configure or turn it off.**

```sh
conduit config set host.isolation auto              # the default
conduit config set host.isolation off               # never
conduit config set host.isolation 8-15,24-31        # the VM's CPUs, exactly
conduit config set host.isolation.win11 off         # one VM only
conduit config unset host.isolation.win11
```

A CPU list gives the VM exactly those CPUs (plus any it is pinned to outside
them, so no vCPU shares a CPU with host work). Changes apply at the VM's next
start. To undo an isolation by hand: `sudo systemctl stop
'conduit-isolation@*.service'`, then restart the VM or run `conduit down
NAME`.

`conduit doctor NAME` also warns when host work (`system.slice`, a session
scope) or a device interrupt can run on the VM's vCPU CPUs while it runs,
and when isolation is off on a CPU with several L3 domains, P/E cores or
NUMA nodes, each with the command that fixes it.

## GPU time: the guest's timeslice

The guest's GPU work and the host's display (the compositor, the viewer's
scale pass) share the GPU's run list. The driver hands the GPU to each busy
channel group for a timeslice, 1024 µs by default, so while a game keeps the
GPU busy, every host frame waits for the guest's slice to run out before it
is drawn: the picture on the host lags and frames are dropped.

The backend gives every guest channel group a 100 µs timeslice when it is
created (`NVA06C_CTRL_CMD_SET_TIMESLICE`, an unprivileged RM control), and
clamps the guest's own timeslice requests to it. When only the guest has
work it keeps the GPU slice after slice, so its frame rate is not affected;
when the host has work too, the host waits at most one short slice. The
backend log states the setting at start (`guest GPU timeslice: ...`), what RM
read back for the first channel group, and a count at teardown.

```sh
conduit config set backend.gpu_timeslice_us 100   # the default
conduit config set backend.gpu_timeslice_us off   # the driver's own (1024 µs)
```

Values are `off` (or `0`) or 50 to 1024 µs; the setting applies at the next
backend start (`conduit-backend --gpu-timeslice-us`). For a test,
`CONDUIT_GUEST_TSG_TIMESLICE_US` in the backend's environment overrides it.
Shorter slices mean more context switches when both sides have work; below
50 µs that cost dominates.

## Display: direct scanout

Every frame the host compositor composites is host GPU work queued with the
guest's. A frame the compositor scans out directly (its buffer on a hardware
plane) costs the host GPU nothing. Check it with `conduit-viewer --stats`
(the viewer log): `N direct scanout` per second, the `path:` the picture
takes in fullscreen, and the `blockers:` when frames were composited
([VIEWER.md](VIEWER.md#presentation-immediate-vsync-or-latest-per-refresh)).
What helps on any desktop:

- fullscreen (`Ctrl+Alt+F`), with the guest at the output's mode (the
  default in fullscreen) and the full picture area;
- no overlay, notification or menu over the picture;
- hardware cursors: `WLR_NO_HARDWARE_CURSORS=1` makes wlroots-based
  compositors draw the pointer in software, which rules out direct scanout
  while the pointer is shown (`conduit doctor` reports it);
- the compositor's own direct-scanout setting, where it has one.

## GPU memory: Resizable BAR and the video-memory limit

- **BAR1.** Without Resizable BAR a GeForce exposes 256 MiB of its memory
  to the CPU, and every guest mapping of GPU memory goes through that window,
  shared with the host desktop. Turn on Above 4G Decoding and Resizable BAR in
  the firmware settings for large guest workloads. `conduit doctor` prints the
  size.
- **Sharing the card with your desktop.** When the card has a monitor
  connected and safe mode is on (the default for the closed modules or a
  branch older than 580), guests get a default video-memory limit so the
  compositor keeps room. On the open modules 580 or newer nothing is set
  unless you ask: `conduit config set gpu.vram_limit_mib auto` (or a number
  of MiB). The limit is the smallest of your number, the display default and,
  in safe mode, 2 GiB (see [SECURITY.md](SECURITY.md); check what applies
  with `conduit doctor`). `off` removes the limit on a dedicated card, but
  not safe mode's 2 GiB: turn safe mode off too (`gpu.safe_mode false`).

## Measuring

Before trusting a benchmark difference, run it at least three times on a
quiet host: no builds, no other VM, no browser playing video. Swings of
±20% between runs on an untuned host are normal and are not regressions.

To see where a Windows guest's frame copy spends its time on the host (and
what CPU placement changes), `conduit trace NAME latency` prints the host
round trip stage by stage ([TRACING.md](TRACING.md), "Latency capture").

See also: [LIBVIRT.md](LIBVIRT.md) (the domain Conduit writes),
[WINDOWS.md](WINDOWS.md).

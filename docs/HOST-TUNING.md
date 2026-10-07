# Host tuning

A VM that stutters now and then (short freezes, uneven frame times,
benchmark numbers that swing between runs) is usually starved by the host,
not slowed by the GPU path. These settings keep the guest's CPUs and memory
to itself. None of them is required to run Conduit; they matter for smooth
desktops, games and repeatable benchmarks.

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
3. **Without reserved huge pages**, let shared memory use transparent huge
   pages. They are best effort and can still be swapped:

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
  vCPUs may run, not that nothing else runs there. Run heavy host jobs
  (compilers, other VMs) on the host's cores at low priority:

  ```sh
  taskset -c 0-7,16-23 nice -n 19 cargo build -j8
  ```

  For a hard split, isolate the guest's cores with systemd
  (`systemctl set-property --runtime user.slice AllowedCPUs=0-7,16-23`,
  and the same for `system.slice`), or at boot with `isolcpus=`/`nohz_full=`.
- **Keep the backend and conduit-venus off the guest's cores**, on part of
  the host's CCD: on the reference host `conduit config set backend.cpus
  0-3,16-19` (four cores and their SMT siblings; applies at the next backend
  start). Unpinned, their threads wake mostly on the guest's idle CPUs and
  preempt a vCPU there. Pinned like this, the GPU copy round trip of a
  windowed Windows game fell from 441 to 398 µs (p50) on the host and its
  share of copies under 0.5 ms in the guest rose from 0.5% to 13.5%
  ([research/host-roundtrip-latency.md](research/host-roundtrip-latency.md)).
  This applies with MSI-X in the guest. A guest on INTx has every completion
  raised by QEMU's main loop, which libvirt pins to the same host CPUs
  (`emulatorpin`), and sharing them there gave millisecond tails. On INTx,
  leave `backend.cpus` unset or keep it clear of the emulator CPUs.
- **CPU governor**: `performance` avoids clock ramp-up latency.

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

See also: [LIBVIRT.md](LIBVIRT.md) (the domain Conduit writes),
[WINDOWS.md](WINDOWS.md).

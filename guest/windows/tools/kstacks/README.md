# Kernel stacks from win11 when Helios hangs

There are two paths, depending on how badly the guest is stuck:

| Situation | Tool | Guest keeps running? |
|---|---|---|
| The KMD worker hangs but SSH still answers | `Get-KStacks.ps1`: LiveKD native live dump plus kd | yes |
| Full wedge (no SSH, shutdown hangs) | host `virsh inject-nmi`, which bugchecks 0x80 into a kernel dump | no, the guest reboots |

Neither path needs `bcdedit /debug on`. The VM has Secure Boot off
(`<feature enabled='no' name='secure-boot'/>`) and test signing on, so
`kd -kl` would also work after `bcdedit /debug on` and a reboot. We don't
need it, because the LiveKD path gives the same output without debug mode.

## 1. Live stacks while the guest still answers (no reboot, no debug mode)

The files live in the guest under `C:\Users\Public\kdumps\`:
- `livekd64.exe`, Sysinternals LiveKD v5.65 from download.sysinternals.com
  (sha256 `e2884fa2…ec005a`)
- `Get-KStacks.ps1`, a copy of this directory's script

Run this from the host:

```
timeout 300 ssh -p 2222 "Ole Algoritme@127.0.0.1" \
  'powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Public\kdumps\Get-KStacks.ps1'
```

Run it as a tracked background task. The options:
- `-Full` also adds `!process 4 7`, every System thread with its stack. This
  makes the output large.
- `-KeepDump` keeps the dump as `W:\kdumps\live-<ts>.dmp`. Without it the
  dump is written to `W:\kdumps\live-tmp.dmp` and deleted afterwards.

What the script does:
1. `livekd64 -accepteula -k <kd.exe> -ml -o <dmp>` takes a native Windows
   live kernel dump. Windows mirrors memory, so the system freezes for well
   under a second and doesn't bugcheck.
2. It runs `kd -z <dmp>` in batch mode with these commands: `vertarget`,
   `lmvm helios_kmd_render`, `!process 0 0`, `!stacks 2 helios_kmd_render`,
   `!stacks 2 dxgkrnl` and `!process 0 7 dwm.exe`.
3. Symbols come from every `W:\*\out\Release` and `W:\*\target\**`
   directory that holds a `helios_kmd_render.pdb`, newest first. kd skips
   any whose GUID doesn't match. Microsoft symbols come from
   `srv*C:\symbols*https://msdl.microsoft.com/download/symbols`.

The output goes to `C:\Users\Public\kdumps\kstacks-<yyyyMMdd-HHmmss>.txt`, with
kd's console output next to it as `.kdstdout`. The script's last line gives the
dump size and how long each step took.

A live dump holds kernel memory only. The dwm.exe threads show their kernel
frames (dxgkrnl waits and so on) but no user-mode frames.

To fetch the output:

```
timeout 60 ssh -p 2222 "Ole Algoritme@127.0.0.1" 'Get-Content -Raw (Get-ChildItem C:\Users\Public\kdumps\kstacks-*.txt | sort LastWriteTime | select -Last 1)' > kstacks.txt
```

### Measured on 2026-10-06 (driver 22.22.327.1)

The first run:
- **Live dump:** 1.8 s for 890 MB, with no visible hitch.
- **kd analysis:** it hit the old 140 s cap while still in `!stacks 2
  dxgkrnl`. Most of that time went to the first Microsoft symbol downloads
  into `C:\symbols` (about 30 MB, including ntkrnlmp, Wdf01000 and dxgkrnl).
  Later runs use the cache. The default cap is now 900 s, and a timed-out
  run keeps its dump.

What the output showed:
- `!stacks 2 helios_kmd_render` found the KMD's System worker thread (4.4a0),
  blocked in `KeWaitForSingleObject`.
- The KMD frame came out as `helios_kmd_render!DriverEntry+0x587fd`, which
  means no matching PDB was found. The module timestamp is 0x6AC4FA0C
  (15:39:24), and the PDB in `W:\s315\out\Release` didn't match it.
- The script now also searches the cargo target dirs and prints `!lmi`,
  which names the GUID/age kd wanted. If that still doesn't resolve it, keep
  the PDB of every installed build (the package build should keep it next to
  the .sys).

The updated script is in this directory. The guest copy is the first
version: re-upload it before the next run. scp/sftp fail on this sshd, so
use base64 over stdin:

```
base64 -w0 Get-KStacks.ps1 | timeout 30 ssh -p 2222 "Ole Algoritme@127.0.0.1" \
  '$b=[Console]::In.ReadToEnd(); [IO.File]::WriteAllBytes("C:\Users\Public\kdumps\Get-KStacks.ps1",[Convert]::FromBase64String($b.Trim()))'
```

## 2. Full wedge: NMI crash dump

### Guest configuration (done 2026-10-06 16:06, takes effect at the next boot)

These values are in `HKLM\SYSTEM\CurrentControlSet\Control\CrashControl`:

| Value | Setting |
|---|---|
| `CrashDumpEnabled` | 2 (kernel memory dump; it was 3, minidump) |
| `NMICrashDump` | 1 |
| `AutoReboot` | 1 |
| `AlwaysKeepMemoryDump` | 1 |
| `DedicatedDumpFile` | `W:\kdumps\DedicatedDump.sys` |
| `DumpFileSize` | 8192 MB |
| `DumpFile` | `W:\kdumps\MEMORY.DMP` |

The values before the change are saved in
`C:\Users\Public\kdumps\CrashControl-before.txt`.

Why W: holds the dump:
- C: had 5.1 GB free and a 3.4 GB system-managed page file, which is too
  tight.
- W: (vdb) uses the same viostor driver as C: (vda) and has about 36 GB
  free.
- Kernel memory was about 0.6 GB right after boot, so a kernel dump should
  be 1-3 GB.

After the next boot, check that both of these hold:
- `W:\kdumps\DedicatedDump.sys` exists and is 8 GB.
- The System log has no `volmgr` event 45, 46 or 49 (dump configuration
  failed).

If either check fails, set `DedicatedDumpFile=C:\DedicatedDump.sys` and
`DumpFileSize=3072`, then reboot again.

### Procedure

1. Capture the host side first. Note the time, the backend logs and the KMD
   counters, if anything can still read them.
2. Inject the NMI from the host: `virsh -c qemu:///session inject-nmi win11`.
   libvirt supports it and QEMU's QMP lists `inject-nmi`.
3. The guest bugchecks with 0x80 (NMI_HARDWARE_FAILURE) and writes the kernel
   dump to the dedicated file. That takes tens of seconds and the screen may
   stay frozen meanwhile. Then it reboots on its own (`AutoReboot=1`).
   - Watch for this with a background poll of `virsh domstate` and SSH.
   - If the guest isn't back within about 5 minutes, the dump probably
     failed. Fall back to the checked cycle script.
   - Don't power the VM off while the dump is being written.
4. After the boot, the system extracts the dump to `W:\kdumps\MEMORY.DMP`
   (System event 1001 names the bugcheck). Analyze it in the guest, where kd,
   the symbol cache and the PDBs already are. scp/sftp to this sshd failed on
   2026-10-06. If the dump is needed on the host, read it from
   `win11-build.qcow2` (W:) with `guestmount --ro`/`qemu-nbd --read-only` while
   the VM is off. The analysis:

   ```
   & 'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\kd.exe' -z W:\kdumps\MEMORY.DMP `
     -y "W:\<pkg>\out\Release;srv*C:\symbols*https://msdl.microsoft.com/download/symbols" `
     -logo C:\Users\Public\kdumps\nmi-analysis.txt -c "`$`$<C:\Users\Public\kdumps\nmi-cmds.txt;q"
   ```

5. Put these analysis commands in `nmi-cmds.txt`:

   ```
   .echo ===== analyze
   !analyze -v
   .echo ===== running
   !running -it
   .echo ===== all CPUs
   !for_each_processor ".echo ----- cpu; k"
   ~*k
   .echo ===== helios threads
   !stacks 2 helios_kmd_render
   !stacks 2 dxgkrnl
   .echo ===== processes
   !process 0 0
   !process 0 7 dwm.exe
   .echo ===== locks
   !locks
   ```

   In a kernel dump, `~*k` doesn't step through processors the way it does in
   user mode. `!running -it` together with `!for_each_processor` gives the
   stack of every CPU.
6. Rename the dump before the next crash, for example to
   `W:\kdumps\MEMORY-<date>.DMP`. Otherwise `Overwrite=1` replaces it.

## 3. Host-only fallback: reading guest RAM without pausing

win11's RAM is a shared memfd (`memory-backend-memfd`, share=true, for
vhost-user). The QEMU process owner can read it live through
`/proc/<qemu pid>/fd/<n>` (`/memfd:memory-backend-memfd`) without pausing the
guest. This was tested with a 4 KiB read. The guest physical layout, from
`info mtree -f`:

| Guest physical range | Offset in the memfd |
|---|---|
| 0x0 - 0x7fffffff | 0x0 |
| 0x100000000 - 0x57fffffff | 0x80000000 |

A copy of that file, with this map, can be loaded by MemProcFS (Linux) or
volatility3 as a raw physical memory image. It isn't atomic. The tooling isn't
set up yet. Use it only if the NMI path fails.

Avoid `virsh dump`/`dump-guest-memory`:
- They pause the VM for the whole dump (20 GiB).
- A pause/resume has previously left the conduit backend refusing MAP_BLOB.
- The `win-dmp` format would also need a `vmcoreinfo` device and the
  virtio-win FwCfg driver, which win11 doesn't have.

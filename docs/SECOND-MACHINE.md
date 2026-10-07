# A Windows guest on a second machine

Step by step, from a bare Linux host with an NVIDIA GPU to a Windows 11 guest
whose desktop runs on NVK-on-RM. Everything here was done on one machine (an
RTX 5090, Blackwell); on another GPU (for example an RTX 4070, Ada) read the
caveats at the end and [GPU-SUPPORT.md](GPU-SUPPORT.md) (per-generation
status, first-hour plan for an RTX 4070) first. Background: [WINDOWS.md](WINDOWS.md),
[NVK-ROADMAP.md](NVK-ROADMAP.md).

## 1. Host driver

- A driver release Conduit has ABI tables for: 535.129.03, 565.77, 580.178.04,
  595.71.05, 595.104.02, 610.57.04 or 615.71.09 (`conduit doctor` lists
  them). It is developed on the NVIDIA **open** kernel modules, 580 or newer;
  the closed modules and older branches with tables are accepted with a
  warning and are untested. The RM protocol (ioctl and control layouts)
  changes between driver versions, and the backend forwards the guest's calls
  only for a version it has tables for. A release without them is refused
  unless the backend runs with `--allow-nearest-abi`, which lets the RM
  pointer, allowlist, UVM, video-memory and OS-descriptor tables fall back to
  the nearest older release's. The GET_DEV_INFO layout and the NVKMS commands
  never do, because their layouts are not monotonic across releases: such a
  release gets no render node and only NVKMS ALLOC/FREE_DEVICE.
  Its tables are generated offline, with no GPU
  (`.github/scripts/abi_update.py`, `host/backend/gen/README.md`): rmctrl,
  rmallow, uvm, vidmem, osdesc, devinfo, nvkms and the escape sizes. The
  `PROFILES` lists of `abi::devinfo` and `abi::nvkms` and the includes in
  `guest/linux/nvgpu_devinfo.h` are still edited by hand for a new release (a
  missed `PROFILES` entry keeps the release refused and fails
  `accepted_releases_match_the_cli_list`; a missed include is not checked).
- With the closed modules or a branch older than 580 (565.77 is the first
  such setup), the backend starts in safe mode by itself: a video-memory
  limit of 2 GiB (the smallest of 2 GiB, the display default and your
  `gpu.vram_limit_mib` number, [SECURITY.md](SECURITY.md)) and 1 s clamps on
  blocking GPU calls. `conduit doctor` says whether safe mode is on and which
  source decided (the setting, your shell, or the driver), the limit guests
  get, and the card's BAR1 size. Turn it off once the setup has proven itself:
  `conduit config set gpu.safe_mode false` ([SECURITY.md](SECURITY.md)).
- The NVIDIA Vulkan driver of the same release (`conduit-venus` uses it for
  the Venus fallback).
- KVM (`ls /dev/kvm`), a Wayland desktop, `virt-manager`/libvirt for the VM.

## 2. Conduit from main

```bash
git clone https://github.com/olealgoritme/conduit && cd conduit
git submodule update --init host/venus/third_party/virglrenderer host/venus/third_party/venus-protocol
make deps            # build dependencies (asks for sudo)
make install-deb     # Ubuntu/Debian: builds the .deb, Venus renderer included, and installs it
# elsewhere: make tarball && tar xf dist/out/conduit-*-x86_64-linux.tar.gz && sudo ./conduit/install.sh
conduit doctor       # every line should say ok (driver version, KVM, desktop)
```

The packages carry the backend with the `venus` feature, `conduit-venus`
with Conduit's patched virglrenderer (`host/venus/patches`, 0001 and 0002)
and the bundled QEMU ([PACKAGING.md](PACKAGING.md)).

## 3. The VM

**Getting Windows 11.** The ISO is on Microsoft's
[download page](https://www.microsoft.com/software-download/windows11)
("Download Windows 11 Disk Image (ISO) for x64 devices"). With a virtio disk
or network, also get the
[virtio-win drivers ISO](https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso):
Setup finds the disk only after "Load driver" from it (`viostor\w11\amd64`).
Windows 11 needs UEFI and TPM 2.0; libvirt emulates the TPM with `swtpm`
(`sudo apt install swtpm swtpm-tools ovmf`).

The quickest way to get both is
[quickget](https://github.com/quickemu-project/quickemu) (`sudo apt install
quickemu`, or the project's PPA): it fetches the official ISO through
Microsoft's download service, plus virtio-win and an unattended-install
answer file:

```bash
quickget windows 11      # windows-11/: the Windows ISO, virtio-win.iso, unattended.iso
```

Use those ISOs as the VM's CD-ROMs below (keep `unattended.iso` attached for
a hands-off install that skips the Microsoft account). quickget also writes a
quickemu config; Conduit does not use it, the VM is a libvirt domain.

1. Create the VM, either way:
   - in virt-manager: a Windows 11 VM with UEFI (OVMF), **Secure Boot off**
     (the driver is test-signed) and a TPM 2.0 (emulated, CRB);
   - or from [examples/win11.xml](examples/win11.xml): edit the disk and ISO
     paths, memory and vCPUs, then
     `qemu-img create -f qcow2 /var/lib/libvirt/images/win11.qcow2 128G` and
     `virsh define docs/examples/win11.xml`; the Windows and virtio-win ISOs
     are its two CD-ROMs (or attach them in virt-manager).

   Install Windows as usual.
2. Shut it off, then give it Conduit's GPU:

   ```bash
   conduit attach win11          # host-passthrough CPU, Hyper-V enlightenments, no Linux setup
   conduit up win11 --venus      # or: conduit view win11 --venus
   ```

Details (the 64 GiB BAR, enlightenments, input): [WINDOWS.md](WINDOWS.md)
"The VM".

## 4. Test signing and the Helios package

1. Build the full package (`HeliosSetup.exe`) with
   `.github/workflows/windows.yml`, or take one built from the same main.
2. In the guest, run `HeliosSetup.exe`. The first run enables test-signing
   and asks for a reboot; run it again after the reboot, then reboot once
   more ([packaging README](../guest/windows/packaging/windows/README.md)).
3. Check: the adapter shows as "Conduit Helios", and
   `C:\ProgramData\Helios\Verify-Helios.ps1 -RunSmokeTests` passes in the
   logged-in session.
4. Later driver-only updates come from the local build VM
   ([guest/windows/ci/vm/README.md](../guest/windows/ci/vm/README.md)) and are
   installed with `pnputil` ([WINDOWS.md](WINDOWS.md) "Guest driver").

At this point D3D11, D3D12, Vulkan and OpenGL apps run on NVK by default; DWM,
the shell and browsers stay on Venus (the built-in deny-list).

## 5. The opt-ins

In this order, checking the desktop after each:

1. Host: `conduit config set venus.guest_blobs true`, then restart the VM's
   backend (`conduit down win11`, `conduit up win11 --venus`).
2. Guest, as administrator: `ForeignFlip` = 1 (REG_DWORD) under
   `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, then
   `pnputil /restart-device` on the adapter (or reboot).
3. Guest: `DwmIcd` = `nvk` (REG_SZ) under `HKLM\SOFTWARE\Helios`, then
   reboot. The desktop follows DWM onto NVK.
4. Optional: `DirectFlipSupport` = 1 under `HKLM\SOFTWARE\Helios`.

Each is described in [WINDOWS.md](WINDOWS.md) "Opt-ins". To back out: delete
`DwmIcd` (DWM's crash-loop guard also falls back to Venus on its own), set
`ForeignFlip` to 0, or `HKLM\SOFTWARE\Helios!Icd` = `venus` to put every
process back on Venus.

## Caveats

- Only tested on an RTX 5090 (Blackwell, GB20x) with a 5120×1440 240 Hz
  monitor. Nothing has run on Ada or older GPUs yet.
- GPU-specific paths are untested elsewhere: the block-linear modifier and
  memory-kind tables the KMD and NVK use for scanout and shared surfaces
  (written and measured for GB20x), and the RM classes NVK picks for the GPU.
  Expect scanout or shared-surface problems first; `Icd=venus` is the way
  back while they are found. Per-generation details, known gaps and the test
  order: [GPU-SUPPORT.md](GPU-SUPPORT.md).
- The open items on the reference machine apply too
  ([NVK-ROADMAP.md](NVK-ROADMAP.md) "Where it stands"): the windowed blt
  path, recovery after `pnputil /restart-device`, Unigine Heaven x86 OpenGL
  renders a white scene.
- Other limits: [KNOWN-ISSUES.md](KNOWN-ISSUES.md#windows-guests-venus).

# A Windows guest on a second machine

Step by step, from a bare Linux host with an NVIDIA GPU to a Windows 11 guest
whose desktop runs on NVK-on-RM. Everything here was done on one machine (an
RTX 5090, Blackwell); on another GPU (for example an RTX 4070, Ada) read the
caveats at the end first. Background: [WINDOWS.md](WINDOWS.md),
[NVK-ROADMAP.md](NVK-ROADMAP.md).

## 1. Host driver

- NVIDIA **open** kernel modules, a release Conduit has RM ABI tables for:
  580.178.04, 595.71.05, 595.104.02, 610.57.04 or 615.71.09
  (`host/backend/gen/src/osdesc/`). The RM protocol (ioctl and control
  layouts) changes between driver versions, and the backend forwards the
  guest's RM calls only for a version it has tables for. A newer driver needs
  its tables generated first (`host/backend/gen`, `.github/workflows/abi.yml`).
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

1. In virt-manager, create a Windows 11 VM: UEFI (OVMF), **Secure Boot
   off** (the driver is test-signed). Install Windows as usual.
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
  back while they are found.
- The open items on the reference machine apply too
  ([NVK-ROADMAP.md](NVK-ROADMAP.md) "Where it stands"): the windowed blt
  path, recovery after `pnputil /restart-device`, Unigine Heaven x86 OpenGL
  renders a white scene.
- Other limits: [KNOWN-ISSUES.md](KNOWN-ISSUES.md#windows-guests-venus).

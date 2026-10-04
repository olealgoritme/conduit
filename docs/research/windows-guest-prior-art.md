# Windows guest GPU acceleration: existing projects

Survey of open projects that give a Windows VM accelerated graphics or compute
from the host GPU without passthrough. Collected 2026-10-04 from web search;
status lines are what each project says about itself, not tested here.

## Closest to Conduit's goal (Windows guest, KVM/QEMU, host GPU shared)

### Helios (WinBoat)
- https://github.com/winboat-org/helios (also mirrored at https://github.com/alppp/helios)
- WDDM kernel-mode driver for QEMU's `virtio-gpu-gl-pci` using the **Venus**
  protocol (Vulkan command serialization, executed by virglrenderer on the
  host's Vulkan driver).
- Claims D3D11, Vulkan, OpenGL and OpenCL in the guest.
- Status: "under heavy development, no official support or instructions";
  needs a self-built QEMU. Used experimentally by WinBoat and by
  `Benehiko/vee` (`--gpu-mode=helios`, https://github.com/Benehiko/vee/pull/186).
- Container image: https://hub.docker.com/r/qemux/qemu-helios
- Relevance: same layering as the roadmap's second Windows route (Vulkan on
  the host, D3D translated in the guest), but over virtio-gpu/Venus rather
  than Conduit's RM forwarding.

### VioGpu3D (virtio-win, virgl)
- Driver PR: https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943
  (tracking issue https://github.com/virtio-win/kvm-guest-drivers-windows/issues/841)
- Mesa side: https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/24223
  (branch `viogpu_win`)
- virglrenderer side (merged): https://gitlab.freedesktop.org/virgl/virglrenderer/-/merge_requests/1185
- WDDM KMD + Mesa user-mode drivers on **virgl** (OpenGL-level protocol):
  WGL via virgl, D3D10 via Mesa's `d3d10umd`.
- Status: draft; "rendering glitches and might crash". The KMD does not
  implement preemption and disables it system-wide as a workaround.
  WinUI3 glitches, Electron apps don't render.
- Relevance: a complete, readable example of a WDDM KMD/UMD pair on a
  virtio transport.

### virtio-gpu-win-icd (older student project)
- https://github.com/Keenuts/virtio-gpu-win-icd
- Talks: https://www.lse.epita.fr/data/lt/2017-04-11/gauer-lt-2018-virtiogpu-and-windows.pdf,
  https://www.lse.epita.fr/lse-summer-week-2017/slides/lse-summer-week-2017-07-3d-acceleration-windows.pdf
- OpenGL ICD for Windows on virtio-gpu/virgl, 2017-2018. Historical reference.

## Building blocks

### Mesa `d3d10umd`
- Gallium frontend that is a WDDM D3D10 user-mode driver (DLL like WARP's).
  Passed Windows HCK wgf11 tests on llvmpipe at one point. D3D10 only, TGSI.
- https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/10687,
  https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/27416
- Used by VioGpu3D for D3D10.

### Venus protocol
- Vulkan serialization over virtio-gpu; renderer in virglrenderer.
- Docs: https://idr.pages.freedesktop.org/mesa/drivers/venus.html
- QEMU setup notes (Linux guests): https://gist.github.com/peppergrayxyz/fdc9042760273d137dddd3e97034385f,
  https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html

### VirtualBox Guest Additions WDDM driver
- VirtualBox 7.0 added DirectX 11 3D for Windows guests, using DXVK on
  non-Windows hosts. The Guest Additions (including the WDDM driver) are in
  VirtualBox's open source tree.
- https://www.gamingonlinux.com/2022/10/virtualbox-70-is-out-with-their-directx-11-support-using-dxvk/
- Relevance: a shipping, open WDDM driver whose host side executes on Vulkan.

## Different hypervisor or direction (reference only)

### Hyper-V GPU-PV (WDDM GPU paravirtualization)
- Microsoft's design: guest dxgkrnl marshals D3DKMT calls to the host over
  VMBus; the guest runs the vendor's own UMD.
  https://learn.microsoft.com/windows-hardware/drivers/display/gpu-paravirtualization
- Hyper-V only. QEMU/KVM has no VMBus host, so this cannot be reused on KVM
  as is.
- Easy-GPU-PV scripts: https://github.com/jamesstringerparsec/Easy-GPU-PV
  (forks: https://github.com/grrminator/Easy-GPU-PV,
  https://github.com/KharchenkoPM/Interactive-Easy-GPU-PV)
- Linux `dxgkrnl` (WSL2, Linux guest on a Windows host, the reverse
  direction): https://lwn.net/Articles/1063798/

## CUDA / compute API remoting (Linux-focused)

Shims that forward CUDA API calls, not driver calls. None found with a
maintained Windows guest client.

- GVirtuS: https://github.com/gvirtus/GVirtuS (forks e.g.
  https://github.com/marianoktm/GVirtuS)
- Cricket (RWTH, ONC RPC, TCP/IB/shared memory): https://github.com/RWTH-ACS/cricket
- rCUDA: closed source. https://www.hpca.uji.es/wp-content/uploads/recent_talks/quintana/rCUDA_overview.pdf
- Juice Labs (GPU over IP, Windows and Linux clients): commercial; no open
  repository found.

## Takeaways for Conduit

- Nobody publicly runs NVIDIA's own Windows driver stack over a KVM
  paravirtual device. Every open Windows-guest effort uses an open user-mode
  stack (Mesa virgl/Venus, d3d10umd, DXVK) over a generic protocol.
- Helios is the nearest match for games (Vulkan on the host, D3D11 in the
  guest) and is worth evaluating first; VioGpu3D is the best readable
  WDDM KMD reference on a virtio transport.

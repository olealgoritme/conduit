# Windows build VM

Builds the Helios driver package (WDDM KMD and the x64/x86 D3D11/D3D12 UMDs)
from this checkout in a local Windows VM, with the toolchain of the `driver`
job in `.github/workflows/windows.yml` and no GitHub Actions run. A change to
the KMD rebuilds in about 2 to 6 minutes; the first build, which also builds
DXVK and vkd3d-proton for both architectures, takes much longer.

| File | Runs on | What |
|---|---|---|
| `Setup-BuildVm.ps1` | the VM, once, as administrator | installs the toolchain |
| `Build-InVm.ps1` | the VM | `ci/windows/Build-Driver.ps1`, then a development test signature |
| `win-build.sh` | the Linux host | copies `guest/windows` to the VM over SSH, runs `Build-InVm.ps1`, copies the package back |

## Setup

A Windows VM (any VM; it does not need Conduit's GPU) with:

- a second virtual disk for the build, formatted as `W:` (the default
  `-Root`; any drive works if passed to every script). Toolchains, the
  source copy, the engine build trees and the output live there;
- the OpenSSH server enabled, reachable from the host (for example a
  user-mode network forward of host port 2222 to the VM's port 22), with
  key login for your user.

Then, in the VM, from an administrator PowerShell with
`Setup-BuildVm.ps1` copied in:

```powershell
powershell -ExecutionPolicy Bypass -File Setup-BuildVm.ps1 [-Root W:\]
```

It installs, at the versions `windows.yml` pins (change both together):
Visual Studio 2022 Build Tools (C++ x86/x64, ATL) and the Windows SDK and
WDK 10.0.26100 on `C:` (they cannot move); PowerShell 7; and on `-Root`
Git, LLVM, Python with Meson and Ninja, MSYS2 (`widl`), the Vulkan SDK, and
Rust nightly with `rust-src`, the i686 target, cargo-make and rust-script.
It sets machine-wide environment variables (`RUSTUP_HOME`, `CARGO_HOME`,
`VULKAN_SDK`, `LIBCLANG_PATH`, `PATH`, ...) and a Defender exclusion for
`-Root`. Every step is skipped when its tool is already there, so running it
again finishes an interrupted setup; it ends with a check of every tool.

## Build

From the Linux host, in a checkout with the DXVK and vkd3d-proton
submodules (`git submodule update --init --recursive
guest/windows/third_party/dxvk guest/windows/third_party/vkd3d-proton`):

```sh
WIN_SSH=user@127.0.0.1 guest/windows/ci/vm/win-build.sh [Release|Debug]
```

`WIN_SSH` has no default; without it the script stops and says how to set
it. To not type it every time, put it (and any other variable below) in
`~/.config/conduit/win-build.env` (`$XDG_CONFIG_HOME/conduit/win-build.env`;
`WIN_BUILD_ENV` names another file), as shell assignments; the environment
wins over the file:

```sh
mkdir -p ~/.config/conduit
echo "WIN_SSH='user@127.0.0.1'" >> ~/.config/conduit/win-build.env
```

| Variable | Default | |
|---|---|---|
| `WIN_SSH` | none (required) | SSH destination: your user in the VM, e.g. `user@127.0.0.1` (quote a user name with spaces) |
| `WIN_PORT` | `2222` | SSH port |
| `WIN_ROOT` | `W:` | build drive, without a trailing backslash |
| `OUT` | `dist/windows-driver/<Configuration>` | where the package lands on the host |
| `WIN_SRC` | this checkout | another checkout or worktree whose `guest/windows` to build; the `ci/vm` scripts still come from this one |
| `CLEAN` | `0` | `1`: copy everything and rebuild from scratch |
| `NVK_ARTIFACT` | `dist/nvk-windows` | NVK on RM and Zink, staged by `guest/nvk-rm/windows/stage-helios-package.sh` (required; copied to `W:\nvk`) |

The package carries NVK on RM (the adapter's Vulkan driver) and Zink on NVK
(its OpenGL ICD), cross-built on the Linux host first:

```sh
guest/nvk-rm/windows/stage-helios-package.sh      # -> dist/nvk-windows
```

It builds both architectures with `guest/nvk-rm/build-windows.sh GL=1` (the
S3 NVK series plus patches 0032-0034; `MESA_DIR`, default
`~/code/mesa-nvk-rm-helios`) and stages `vulkan_nouveau.dll`,
`librmclient.dll`, `helios_nvk64.json`, `vulkan_nouveau32.dll`,
`librmclient32.dll`, `helios_nvk32.json`, `helios_gl64.dll` and
`helios_gl32.dll`. `Build-Driver.ps1 -NvkArtifact` (`HELIOS_NVK_ARTIFACT`)
checks them and the cargo-make package step copies them next to the UMDs.

Builds are incremental. `win-build.sh` keeps a sha256 manifest per source
checkout (`dist/windows-driver/.sync-*`) and sends only files whose content
changed, extracted with the current time so cargo and ninja see them as
newer; files removed from the checkout are deleted in the VM, and unchanged
files keep their build. `Build-InVm.ps1` sets `HELIOS_KEEP_ENGINE_BUILDS=1`,
so `Build-Driver.ps1` reuses the configured DXVK and vkd3d trees (ninja still
rebuilds what changed in them); CI never sets it. `CLEAN=1` (or no manifest
yet) wipes the VM's source tree and copies everything, and passes `-Clean`,
which drops the engine trees too.

In the VM the source is `W:\src\guest\windows`, engine builds go to
`W:\helios-build`, and the package to `W:\out\<Configuration>`.

## Signing

`Build-InVm.ps1` signs as `Assemble-Package.ps1` does in CI, with a
development certificate that `tools/sign-helios-development.ps1` creates
once (in `W:\out\signing`) and reuses: the SYS and the four UMDs, then a
fresh catalog (`Inf2Cat`) over their final bytes, then the catalog. The
certificate is copied into the package as `helios-dev-test.cer`. The package
installs only on a machine in test-signing mode that trusts that
certificate.

## Installing the package

The package is the driver: `helios_kmd_render.inf/.sys/.cat`,
`helios_umd.dll`, `helios_umd12.dll`, `helios_umd32.dll`,
`helios_umd12_32.dll`, NVK and Zink (above), `helios-dev-test.cer`, and
licenses. The INF installs NVK and Zink into the driver store with the UMDs
and registers them on the adapter's software key: `VulkanDriverName(Wow)`
(the Vulkan loader finds NVK for the Helios adapter; nothing under
`HKLM\SOFTWARE\Khronos`) and `OpenGLDriverName(Wow)` (opengl32.dll loads
Zink, which loads NVK directly). The UMDs find NVK there too, unless
`HKLM\SOFTWARE\Helios!NvkIcdPath(32)` names another build. One policy, read
by the UMDs, NVK and Zink, decides per process: `HKLM\SOFTWARE\Helios!Icd`
= `venus` puts D3D, Vulkan and OpenGL back on Venus; `NvkDenyList` /
`NvkAllowList` (executable names, `;`-separated) adjust the built-in
deny-list (DWM, the shell, browsers stay on Venus). The Mesa Venus ICD
(still registered under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`: the UMDs'
DXVK on Venus reaches it through the loader, and Vulkan apps see it as a
second device after NVK), the loaders and the installer come from the full
package (`windows.yml`, `HeliosSetup.exe`; see `packaging/windows/README.md`),
which a Windows guest needs once; this package then replaces the driver.

`win-build.sh` ends by pointing here. In the Windows guest (Secure Boot
off), from an administrator prompt, with the package copied in:

```bat
bcdedit /set testsigning on
rem reboot once if test-signing was off
certutil -addstore -f Root helios-dev-test.cer
certutil -addstore -f TrustedPublisher helios-dev-test.cer
pnputil /add-driver helios_kmd_render.inf /install
```

then reboot (or `pnputil /restart-device` on the adapter). pnputil keeps an
installed driver of the same version, so bump `HELIOS_KMD_VERSION` in
`kmd_render/driver-version.env` (the only place the version is set) for every
build you install over an earlier one. `packaging/windows/Install-Helios.ps1`
does the same steps with checks, for the full package.

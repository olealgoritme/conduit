# Helios vGPU Setup — self-contained installer (Rust)

`HeliosSetup.exe` is the user-facing installer for the Helios Windows graphics
stack, and it is a **single self-contained executable**: the whole bundle
(scripts, KMD, both UMDs, Mesa, CLVK, Khronos loaders, certificate, manifest,
engine licenses, the DaVinci Resolve shim) is appended to its own PE image.
There is no folder of loose files to keep together.

Rust owns the window, the container, process supervision and the CLI. The
install logic stays in the one PowerShell payload, which is embedded and
extracted at run time and executed by the in-box Windows PowerShell — the same
payload WinBoat's `-Automatic` flow and a human running it by hand use. There is
deliberately no second implementation of the ~30 install checks to drift.

## Usage

```
HeliosSetup.exe                          # GUI: Install / Repair / Update / Uninstall
HeliosSetup.exe --silent [--automatic]   # no UI; install or repair/update
HeliosSetup.exe --silent --uninstall     # no UI; remove
HeliosSetup.exe --silent --log PATH      # log to a chosen file
HeliosSetup.exe [--silent] --nvidia-umd [FOLDER]  # RTX personality: NVIDIA's own UMDs
HeliosSetup.exe [--silent] --helios-umd           # RTX personality: the Helios UMDs
HeliosSetup.exe --bundle <payloadDir> <out>  # CI only: pack a payload folder into <out>
```

`--silent` writes `%ProgramData%\Helios\logs\setup.log` (or `--log`), returns the
payload's exit code (`0` success, `3010` reboot required, `2` uninstall-only
stored installer), and prints the required external step to the caller's console.

`--automatic` passes `-Automatic` to the payload: WinBoat's unattended flow, which
copies the bundle to `%ProgramData%\Helios\provisioning`, registers the resume
task, and writes `finished` on the post-reboot run. This is what the OEM
`install.bat` calls.

## NVIDIA's own user-mode drivers (RTX personality, opt-in)

When the host presents the adapter as an RTX 5090 (`PCI\VEN_10DE&DEV_2B85`),
the guest can run NVIDIA's own Windows user-mode drivers (D3D9-12, Vulkan,
OpenGL, CUDA, NVAPI) on the Helios KMD instead of the Helios UMDs. Helios does
not ship, redistribute or download any NVIDIA file: they come from an NVIDIA
driver package the user already has.

- `--nvidia-umd FOLDER`: FOLDER is an extracted NVIDIA Windows driver download
  (the folder that holds `Display.Driver`, or `Display.Driver` itself; NVIDIA's
  `.exe` is a self-extracting archive that 7-Zip opens). Without FOLDER, NVIDIA's
  own package in the DriverStore (`nv_disp*.inf_amd64_*`) of the right version
  is used.
- `--helios-umd`: the Helios UMDs. Every change the NVIDIA mode made is undone.
- Neither: an update or repair keeps the last choice; a first install registers
  the Helios UMDs.

Outside the installer, `C:\ProgramData\Helios\Set-HeliosUmd.ps1 -Nvidia
[-Package FOLDER]`, `-Helios`, or no argument (shows the current choice) does
the same without reinstalling. Restart Windows after a switch.

**Version check.** The package must be the NVIDIA driver version the Helios KMD
speaks, and of the host driver's branch. Both come from the registry, under
`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`:

| Value | Type | Written by | Meaning |
|---|---|---|---|
| `Parameters\NvUmdVersion` | REG_SZ | the Helios INF (RTX personality) | NVIDIA Windows driver version the KMD speaks, e.g. `610.88` |
| `NvUmdVersion` | REG_SZ | an administrator (optional) | overrides `Parameters\NvUmdVersion`, for the installer and the KMD alike |
| `NvHostRmVersion` | REG_SZ | the KMD, once per adapter start | the host RM's version, e.g. `610.57.04` |

The package version is the INF's `DriverVer` in NVIDIA's short form
(`32.0.16.1088` is `610.88`), and `nvlddmkm.sys` must carry the same version. It
must equal `NvUmdVersion`; its branch (`610`) must equal the host RM's (NVIDIA
numbers the Linux and Windows builds of one branch differently, so only the
branch is common). `nvldumdx.dll` must carry a valid NVIDIA signature. Any
mismatch stops the switch before anything is changed.

The KMD reads the same `NvUmdVersion` (the override first) and reports it to
the host at every adapter start; a host whose RM is not paired with that release
refuses the adapter's start and its RM calls (see `docs/SECURITY.md`). Staging
records no version of its own: what was staged is the release `NvUmdVersion`
names. Both forms the check accepts, `610.88` and a driver file version such as
`32.0.16.1088`, are read by the KMD too; the value must be a REG_SZ.

**What is changed**, all recorded in `C:\ProgramData\Helios\nvidia-umd.json`
and undone by `--helios-umd`, `Set-HeliosUmd.ps1 -Helios` and uninstall:

- Files: only those NVIDIA's INF installs into its own driver-store folder for
  the RTX 5090 (DIRID 13 without a subfolder), plus the required UMD files, are
  copied into the Helios driver-store folder (administrators take ownership of
  that folder for the copy; its owner and permissions are saved and restored).
  `System32\nvcuda.dll` (NVIDIA's CUDA loader), `System32\nvapi64.dll` and
  `SysWOW64\nvapi.dll` are NVIDIA's; what was there before is kept in
  `C:\ProgramData\Helios\nvidia-umd-backup` and put back.
- Registry: the Helios INF's RTX values (`UserModeDriverName(WoW)` =
  `nvldumdx.dll`/`nvldumd.dll`, `VulkanDriverName(WoW)` = `nv-vk64/32.json`,
  `OpenGLDriverName(WoW)` = `nvoglv64/32.dll`, `InstalledDisplayDrivers`), then
  the HKR values of NVIDIA's INF for the RTX 5090 (CUDA, NVAPI, DXCore and the
  rest), with `%13%` as the Helios driver-store folder. The previous value of
  each is recorded first. `Services\nvlddmkm\ImagePath` points at the staged
  `nvlddmkm.sys` (NVIDIA's CUDA loader looks for it; the service never starts).
  A service-key `NvUmd` override is removed, so the INF's `Parameters\NvUmd` = 2
  applies.
- Helios mode on the RTX personality registers the Helios INF's own UMD values
  (`helios_umd*.dll`, NVK, Zink), removes the values only the RTX section sets,
  and sets `NvUmd` = 0 in the service key.

A failure part-way undoes what was done and leaves the Helios UMDs registered.
`Verify-Helios.ps1` checks the registration of whichever mode is active and, in
NVIDIA mode, the staged version against both registry values.

## External steps and orchestration

Installing a display driver needs a reboot, and enabling test-signing needs one
before that. Three channels report it:

1. **Exit code** — `0` / `3010` / non-zero.
2. **UI/console** — the GUI prompts "restart now?" and shows a Reboot button;
   `--silent` prints the step.
3. **`%ProgramData%\Helios\provisioning-status.json`** — written on every path:
   `waiting` → `test-signing-restart-required` → `driver-restart-required` →
   `finished`, or `failed` + message. This is the contract WinBoat polls.

## The container format

`src/archive.rs` owns both packing and unpacking:

```
[ PE image ][ header ][ solid LZMA2 stream ][ footer(64B: offset, size, header_len, sha256, "HLIOSET2") ]
```

The header is an uncompressed list of (name, size); the LZMA2 stream decodes to
the concatenation of every entry in header order. Extraction rejects absolute and
parent-traversing entry names, and the whole container is SHA-256 checked before
any byte is written.

## Build

```
ci/windows/Build-Installer.ps1 -RepoRoot <repo> -OutputDir <out> -Configuration Release|Debug
```

The script builds with `cargo` (`+crt-static`), then asserts the result imports no
dynamic CRT and is a GUI-subsystem image. `build.rs` compiles `installer.rc`
(icon + manifest) with the Windows SDK `rc.exe`; the manifest requests
`requireAdministrator`, because the payload refuses to run unelevated.

The final self-contained exe is produced by the packaging step
(`ci/windows/Assemble-Package.ps1`), which calls
`HeliosSetup.exe --bundle <payloadDir> <out>` after assembling the payload.

## Files

| Path | Purpose |
|---|---|
| `src/main.rs` | CLI, payload extraction/resolution, process supervisor. |
| `src/gui.rs` | Win32/GDI window, painting, event loop (GDI only: runs on Microsoft Basic Display). |
| `src/archive.rs` | The self-contained container (pack + unpack). |
| `src/installer.manifest` | Common Controls v6, DPI, `requireAdministrator`. |
| `installer.rc`, `build.rs` | Icon + manifest embedding via `rc.exe`. |
| `assets/` | `logo_bg.bmp` (logo composited on the window background), `helios.ico`, `winboat_logo.svg` source. |

The logo BMP is regenerated from the SVG with
`magick winboat_logo.svg -background '#14161B' -flatten -resize 128x128 -type TrueColor bmp3:logo_bg.bmp`.

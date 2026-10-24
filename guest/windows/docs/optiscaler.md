# OptiScaler in Windows guests

[OptiScaler](https://github.com/optiscaler/OptiScaler) is a per-game mod that
takes a game's DLSS, FSR or XeSS calls and runs the upscaler you pick: DLSS
through the guest's NGX setup ([dlss.md](../../nvk-rm/docs/dlss.md)), FSR 3.1
or XeSS on any host GPU, plus its own frame generation. It is **GPL-3.0**.
Conduit neither ships nor links it: `Install-OptiScaler.ps1` downloads a
pinned release from OptiScaler's GitHub releases at install time, checks its
SHA-256, and copies it next to one game.

## From the tray

**Upscaling and frame generation** > **Profile for <exe>** > **OptiScaler**:

| Entry | Does |
|---|---|
| OptiScaler 0.9.4 installed / OptiScaler 0.9.4 (GPL-3.0, ...) | what is there (grayed) |
| Install OptiScaler | download (once, then cached), install next to the exe |
| Update to <version> | when the game has an older release from the script; keeps its OptiScaler.ini |
| Remove OptiScaler | remove exactly what the script copied |
| Upscaler > DLSS / FSR 3.1 / XeSS / OptiScaler's default | the game's OptiScaler.ini outputs (table below); DLSS is grayed until `Install-Dlss.ps1` has run |
| Frame generation > On / Off | OptiFG (below) |

Profiles are listed for apps with an open window and for games that have
OptiScaler from the script but are not running ("(not running)").

Nothing is changed while a program from the game's folder runs. Picks for a
running game are queued ("Waiting for <exe> to close: install, FSR 3.1"), and
the tray runs the script about a second after the game has closed; later
picks for the same game merge into the queued ones (the queue is lost when
the tray exits). A notification shows the script's result. The game reads
the change at its next start.

Games with anti-cheat get no Install entry: the menu shows "Not offered: the
game has anti-cheat: ..." (the list below). The tray never passes `-Force`.

The tray runs `Install-OptiScaler.ps1` from its own folder only when it is
the installed tray in `Program Files\Conduit` (administrators only: the
script runs elevated); the driver package puts the script and
`optiscaler-manifest.txt` there. An elevated tray (the usual case: the logon
task) runs it directly and shows its output; an unelevated one asks once per
run through UAC and reports the exit code.

## From PowerShell

Elevated, in the guest (the script and `optiscaler-manifest.txt` are in
`C:\Program Files\Conduit`, or in `guest/windows/packaging/windows`; keep them
together):

```powershell
.\Install-OptiScaler.ps1 -Game 'G:\SteamLibrary\steamapps\common\Game\bin\x64\Game.exe'
.\Install-OptiScaler.ps1 -Game 'G:\...\Game.exe' -Configure -Upscaler xess -FrameGen on
.\Install-OptiScaler.ps1 -Game 'G:\...\Game.exe' -Uninstall
.\Install-OptiScaler.ps1 -DownloadOnly                   # fill the cache only
.\Install-OptiScaler.ps1 -DownloadOnly -Version latest   # newest release; prints its hash
.\Install-OptiScaler.ps1 -Game '...' -Source 'D:\OptiScaler_0.9.4'   # an extracted release, offline
```

| Parameter | |
|---|---|
| `-Game` | the game's exe, or its folder (Unreal Engine: `...\Binaries\Win64`) |
| `-Download` | download the release (the default without `-Source`) |
| `-Source` | an extracted release instead |
| `-Version` | `latest` or a version number instead of the pinned release: GitHub's API names the asset, its listed SHA-256 (or `-Sha256`) is checked |
| `-Proxy` | `dxgi`, `winmm`, `version`, `dbghelp`, `d3d12`, `wininet`, `winhttp`. Default: the earlier install's, else `dxgi`, else the first the game does not ship itself. Vulkan games often need `winmm` |
| `-Upscaler` | `auto` (default), `dlss`, `fsr`, `xess`, `default` |
| `-FrameGen` | `off`, `on` (= `optifg`), `dlssg` |
| `-ResetIni` | replace the game's OptiScaler.ini with the release's (old one kept as `OptiScaler.ini.conduit-bak`) |
| `-Force` | install despite anti-cheat (with a warning), replace files the script did not put there, take over an OptiScaler installed by hand |
| `-Configure` | change only OptiScaler.ini of an install |
| `-Uninstall` | remove what the script copied and the registry entry |
| `-WaitForExit` | wait (up to `-WaitMinutes`, default 720) for the game to close instead of stopping |

Exit codes: 0 done, 1 error, 2 refused (anti-cheat), 3 the game is running,
4 not installed there (`-Configure`, `-Uninstall`). The last line of a
successful run starts with `OK:`; errors go to stderr as
`Install-OptiScaler: ...`.

## Download and cache

`optiscaler-manifest.txt` (next to the script) pins the release:

| Key | Value |
|---|---|
| `release.version` | 0.9.4 |
| `release.url` | `https://github.com/optiscaler/OptiScaler/releases/download/v0.9.4/Optiscaler_0.9.4-final.20260718._MM.7z` |
| `release.sha256` | `575cb4df866116093df75af607e37fd70e10f5163e0f23fd5c804142e80ef0ad` (55 016 448 bytes) |
| `license.url`, `license.sha256` | the GPL-3.0 `LICENSE` at the tag (the release archive has none), `3972dc97...` |
| `7zr.url`, `7zr.sha256` | 7-Zip 26.04's `7zr.exe` (LGPL-2.1), `256feca8...` |

The archive is a 7z (LZMA2, BCJ2). Windows 11's `tar.exe` (libarchive)
extracts it; when it cannot (Windows 11 before 23H2), the script downloads
the pinned `7zr.exe`, checks its hash and uses that. Neither OptiScaler nor
7-Zip is in this repository or the driver package.

`C:\ProgramData\Helios\optiscaler\<version>\` holds `files\` (the extracted
release), `LICENSE`, `release.txt` (URL, hash, source URL at the tag) and
`files.sha256`. The folder is writable by administrators and SYSTEM only
(the script copies from it, elevated, into game folders); a cached release is
checked against `files.sha256` before each install and fetched again if
anything changed, and only the files listed there are copied. The download
is extracted into a scratch folder and moved into place once complete.
`Uninstall-Helios.ps1` deletes the cache.

To move the pin: run `-DownloadOnly -Version latest` in a guest, which prints
the `release.*` lines for the newest release, and update the manifest
(`license.url` at the new tag; the tray reads the manifest at build time, so
rebuild it too).

## What goes into the game folder

Every file of the release except `*.bat`, `*.sh`, `*.txt`, `*.md`, with
`OptiScaler.dll` renamed to the proxy name. An existing `OptiScaler.ini` and
`fakenvapi.ini` stay. `conduit-optiscaler.txt` lists the copied files
(`# version`, `# proxy`, `# exe` header lines); `-Uninstall` removes exactly
those, and an update removes the old release's files the new one no longer
has. Files the script did not put there are never replaced without `-Force`,
and an OptiScaler installed by hand (a proxy DLL whose original file name is
`OptiScaler.dll`) is refused.

`HKLM\SOFTWARE\Helios\OptiScaler` gets one REG_SZ per game (name: the game
folder, data: its OptiScaler.ini), so the guest-wide upscaler setting
(`conduit upscaler VM ...`, the viewer's **Upscaler in games** row) rewrites
the same file; the tray also finds the "(not running)" games there.

## OptiScaler.ini

On a new install:

| Setting | Value |
|---|---|
| Upscaler (`-Upscaler auto`) | the guest-wide `Upscaler` when it is `dlss`, `fsr` or `xess` (`dlss` only with the DLSS setup); else `dlss` when `Install-Dlss.ps1`'s setup is there; else `fsr` |
| `[NvApi] OverrideNvapiDll` | `false` with the DLSS setup: OptiScaler uses the guest's `nvapi64.dll` (DXVK-NVAPI) instead of its bundled fakenvapi; else OptiScaler's default |
| Frame generation | off |

"The DLSS setup" means what `Install-Dlss.ps1` leaves: `nvngx.dll` and
`_nvngx.dll` in `C:\ProgramData\Helios\ngx`, `HKLM\SOFTWARE\Helios` `Dlss` = 1,
and `nvapi64.dll` in System32 (`-SystemNvapi`) or next to the game. DLSS as
output also needs the game's own `nvngx_dlss.dll`.

Upscaler modes (the same table as the guest-wide setting, checked by a tray
unit test):

| Mode | `Dx11Upscaler` | `Dx12Upscaler` | `VulkanUpscaler` | `[DLSS]` `RenderPresetOverride`, `RenderPresetForAll` |
|---|---|---|---|---|
| `dlss` | dlss | dlss | dlss | true, 11 (preset K) |
| `fsr` | fsr31 | fsr31 | fsr31 | auto, auto |
| `xess` | xess_12 | xess | xess | auto, auto |
| `default` | auto | auto | auto | auto, auto |

Frame generation, `[FrameGen]`:

| `-FrameGen` | `Enabled` | `FGInput` | `FGOutput` |
|---|---|---|---|
| `off` | false | nofg | nofg |
| `on`, `optifg` | true | upscaler (OptiFG: from the upscaler's inputs, DX12 games) | fsrfg (FSR 3.1 frame generation) |
| `dlssg` | true | dlssg (a game's DLSS frame generation inputs) | fsrfg |

OptiFG can smear the HUD; OptiScaler's overlay (Insert) has its HUD fix
(`[OptiFG] HUDFix`), left at OptiScaler's default here. Frame generation's
pacing on Conduit's present path is untested.

## Anti-cheat

The script refuses, unless `-Force`, games whose exe is on the list in
`optiscaler-manifest.txt` (Counter-Strike 2, CS:GO, Dota 2, Deadlock,
Valorant, League of Legends, Fortnite, Apex Legends, PUBG, Rainbow Six
Siege, Overwatch 2, Destiny 2, Escape from Tarkov, GTA V, Call of Duty,
Battlefield 2042 and 6, Marvel Rivals, Delta Force, HoYoverse games, Halo
Infinite), and games whose folder, or a folder above it up to the game's own
(at most four levels, never past a library folder such as `steamapps\common`,
`Program Files` or a drive root), has a file or folder named like
`EasyAntiCheat*`, `start_protected_game.exe`, `BattlEye*`, `BEService*`,
`EAAntiCheat*`, `vgk*`, `AntiCheatExpert*`, `GameGuard*`, `XIGNCODE*`,
`mhyprot*`, `HoYoKProtect*`. Any `.exe` in the game folder is checked too.
OptiScaler injects a DLL into the game; with these that can get the account
banned. The tray uses the same list (built in from the manifest).

## Tested

In a Windows 11 guest (26200, `tar.exe` 3.8.9), with a dummy `game.exe` in a
scratch folder: download, hash check and extraction (tar.exe, and the
7zr.exe path with tar.exe taken away: identical files); install (DLSS setup
detected: `dlss` and `OverrideNvapiDll=false`), `-Configure`, an update that
keeps the ini, refusal and `-WaitForExit` while the exe runs, `-Uninstall`
back to only `game.exe` and no registry value, the game's own `dxgi.dll`
(OptiScaler as `winmm.dll`, the game's file kept), anti-cheat refusals by exe
name and by an `EasyAntiCheat` folder, and `-Force`. `-Version latest`
against GitHub's API. The ini editing, manifest parsing and anti-cheat search
also run under PowerShell 7 on Linux; the tray's logic (anti-cheat search,
markers, ini state, queued picks, arguments, menu ids) has unit tests.

Not run: OptiScaler inside a game, the tray's OptiScaler menu on Windows.

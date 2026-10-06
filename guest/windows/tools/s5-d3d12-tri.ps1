# dxvk-on-nvk S5 check: d3d12_tri through the Helios D3D12 UMD on NVK (or
# Venus), on the logged-on user's desktop, then the UMD12 log lines that say
# which backend, sync and present path it took.
#
#   s5-d3d12-tri.ps1 [-Icd nvk|venus] [-Seconds 5] [-Width 1280] [-Height 720]
#                    [-Present 0|1|2] [-Offscreen] [-Dir C:\Users\Public\s5]
#
# Expects, under -Dir: app\d3d12_tri.exe, app\run-in-session.ps1 and
# nvk64\vulkan_nouveau.dll + nvk64\librmclient.dll (64-bit). No registry write:
# HELIOS_ICD12 / HELIOS_NVK_ICD / HELIOS_NVK_PRESENT are per-process overrides.
# -Present: 0 auto (DWM composes the back buffer's NVK resource id when
# ForeignImport=1, else scanout 0), 1 scanout 0, 2 WDDM present (DWM composes).
# -Offscreen: caps, compute and the offscreen pixel check only (no swap chain).
param(
    [ValidateSet("nvk", "venus")][string]$Icd = "nvk",
    [int]$Seconds = 5,
    [int]$Width = 1280,
    [int]$Height = 720,
    [int]$Present = 0,
    [switch]$Offscreen,
    [string]$Dir = "C:\Users\Public\s5"
)
$ErrorActionPreference = "Stop"
$app = Join-Path $Dir "app"
$since = Get-Date
$vars = @{ HELIOS_ICD12 = $Icd; HELIOS_NVK_PRESENT = "$Present" }
if ($Icd -eq "nvk") { $vars.HELIOS_NVK_ICD = Join-Path $Dir "nvk64\vulkan_nouveau.dll" }
$cmd = if ($Offscreen) { "d3d12_tri.exe --offscreen" } else { "d3d12_tri.exe $Seconds $Width $Height 0" }
& (Join-Path $app "run-in-session.ps1") -Dir $app -Env $vars -Command $cmd -TimeoutSec ($Seconds + 60)
"--- UMD12 log (C:\ProgramData\Helios\umd12-*.log written since the start) ---"
Get-ChildItem C:\ProgramData\Helios\umd12-*.log -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -ge $since -and $_.Name -notlike "*-vkd3d.log" -and (Select-String -LiteralPath $_.FullName -Pattern 'd3d12_tri.exe' -SimpleMatch -Quiet) } | ForEach-Object {
        "== $($_.Name)"
        Select-String -LiteralPath $_.FullName -Pattern 'icd backend|NVK|nvk|ID3D12Device created|Native feature|REFUS|FAILED|refusals' |
            Select-Object -First 80 | ForEach-Object { $_.Line }
    }

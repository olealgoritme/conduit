# dxvk-on-nvk S3 check: d3d11_spin through the Helios UMD on NVK (or Venus),
# on the logged-on user's desktop, then the UMD log lines that say which
# backend and present path it took.
#
#   s3-nvk-spin.ps1 [-Icd nvk|venus] [-Seconds 10] [-Width 1920] [-Height 1080]
#                   [-Present 0|1|2] [-Dir C:\Users\Public\s3]
#
# Expects, under -Dir: app\d3d11_spin.exe, app\run-in-session.ps1 and
# nvk\vulkan_nouveau.dll + nvk\librmclient.dll (64-bit). No registry write:
# HELIOS_ICD / HELIOS_NVK_ICD / HELIOS_NVK_PRESENT are per-process overrides.
# -Present: 0 auto (scanout unless DWM composes foreign resids, ForeignImport=1),
# 1 scanout 0 (zero-copy flip of the back buffer), 2 WDDM present (DWM composes).
param(
    [ValidateSet("nvk", "venus")][string]$Icd = "nvk",
    [int]$Seconds = 10,
    [int]$Width = 1920,
    [int]$Height = 1080,
    [int]$Present = 0,
    [string]$Dir = "C:\Users\Public\s3"
)
$ErrorActionPreference = "Stop"
$app = Join-Path $Dir "app"
$since = Get-Date
$vars = @{ HELIOS_ICD = $Icd; HELIOS_NVK_PRESENT = "$Present" }
if ($Icd -eq "nvk") { $vars.HELIOS_NVK_ICD = Join-Path $Dir "nvk\vulkan_nouveau.dll" }
& (Join-Path $app "run-in-session.ps1") -Dir $app -Env $vars `
    -Command "d3d11_spin.exe $Seconds $Width $Height 0 0" -TimeoutSec ($Seconds + 60)
"--- UMD log (C:\ProgramData\Helios\umd-*.log written since the start) ---"
Get-ChildItem C:\ProgramData\Helios\umd-*.log -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -ge $since -and (Select-String -LiteralPath $_.FullName -Pattern 'd3d11_spin.exe' -SimpleMatch -Quiet) } | ForEach-Object {
        "== $($_.Name)"
        Select-String -LiteralPath $_.FullName -Pattern 'icd backend|NVK |nvk |DxvkDevice|dxvk env|REFUS|FAILED|resource id' |
            Select-Object -First 60 | ForEach-Object { $_.Line }
    }

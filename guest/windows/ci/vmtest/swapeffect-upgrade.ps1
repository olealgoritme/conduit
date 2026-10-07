# Windows' blt-to-flip swap-effect upgrade ("optimizations for windowed games"): what gates it, for one program.
# guest/windows/docs/independent-flip.md section 11.5.
#
# 1. Lists every WINDOWEDSWAPEFFECTUPGRADE_REASON_* value the DXGI ETW provider's manifest knows (the full set of
#    gates DXGI can report, not just the ones a run hits).
# 2. Optionally sets the per-app opt-in (HKCU\Software\Microsoft\DirectX\UserGpuPreferences, value name = the exe's
#    full path, data "SwapEffectUpgradeEnable=1;") and removes it again at the end.
# 3. Runs the program in the interactive session under a DXGI ETW capture and prints the reasons and swap effects
#    the swap-chain events carried.
#
#   powershell -File swapeffect-upgrade.ps1 -Exe C:\path\app.exe [-AppArgs "..."] [-Seconds 15] [-PerApp]
#   powershell -File swapeffect-upgrade.ps1 -ListOnly
param(
  [string]$Exe = "",
  [string]$AppArgs = "",
  [int]$Seconds = 15,
  [switch]$PerApp,
  [switch]$ListOnly
)
$ErrorActionPreference = "Continue"
$out = "C:\Users\Public\t"
New-Item -ItemType Directory -Force -Path $out | Out-Null

"== reasons the DXGI manifest defines"
$man = wevtutil gp Microsoft-Windows-DXGI /ge:true /gm:true 2>$null
$reasons = $man | Select-String -Pattern "WINDOWEDSWAPEFFECTUPGRADE_REASON_\w+" -AllMatches | % { $_.Matches.Value } | Sort -Unique
if ($reasons) { $reasons } else { "(none found: the manifest map may be named differently; search the raw output for SWAPEFFECT)" }
if ($ListOnly -or $Exe -eq "") { return }

$k = "HKCU:\Software\Microsoft\DirectX\UserGpuPreferences"
if ($PerApp) {
  if (-not (Test-Path $k)) { New-Item -Path $k | Out-Null }
  Set-ItemProperty $k -Name $Exe -Value "SwapEffectUpgradeEnable=1;"
  "per-app opt-in set for $Exe"
}
"global: " + ((Get-ItemProperty $k -EA 0).DirectXUserGlobalSettings)

$etl = "$out\swup.etl"; $csv = "$out\swup.csv"
Remove-Item $etl, $csv -EA 0
logman stop swup -ets 2>$null | Out-Null
logman start swup -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xFF -o $etl -ets | Out-Null

$c = "$out\swup.cmd"
$dir = Split-Path $Exe
Set-Content $c -Encoding ASCII -Value @('@echo off', "cd /d `"$dir`"", "start `"`" `"$Exe`" $AppArgs")
schtasks /create /f /tn SwUp /tr $c /sc once /st 23:59 /it /ru $env:USERNAME | Out-Null
schtasks /run /tn SwUp | Out-Null
Start-Sleep 3
schtasks /delete /f /tn SwUp | Out-Null
Start-Sleep $Seconds
logman stop swup -ets | Out-Null
$name = [IO.Path]::GetFileNameWithoutExtension($Exe)
Get-Process -Name $name -EA 0 | Stop-Process -Force -EA 0
if ($PerApp) { Remove-ItemProperty $k -Name $Exe -EA 0 }

tracerpt $etl -o $csv -of CSV -y | Out-Null
"== reasons this run reported"
Select-String -Path $csv -Pattern "WINDOWEDSWAPEFFECTUPGRADE_REASON_\w+" -AllMatches | % { $_.Matches.Value } | Group-Object | % { "{0,6}  {1}" -f $_.Count, $_.Name }
"== swap effects in the swap-chain events"
Select-String -Path $csv -Pattern "DXGI_SWAP_EFFECT_\w+" -AllMatches | % { $_.Matches.Value } | Group-Object | % { "{0,6}  {1}" -f $_.Count, $_.Name }

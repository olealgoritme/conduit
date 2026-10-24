<#
.SYNOPSIS
  Put OptiScaler next to one game, or take it away again, and let the
  Conduit upscaler setting follow it.

.DESCRIPTION
  OptiScaler (github.com/optiscaler/OptiScaler, GPL-3.0) hooks a game's DLSS,
  FSR or XeSS calls and hands them to the upscaler you pick, on any GPU.
  Conduit does not ship it: download a release yourself, extract the archive
  (7-Zip), and point this script at the extracted folder.

  Install copies the release's files next to the game's executable, renames
  OptiScaler.dll to the proxy DLL name (dxgi.dll unless -Proxy says
  otherwise), and lists the game's OptiScaler.ini under
  HKLM\SOFTWARE\Helios\OptiScaler, so `conduit upscaler VM fsr|xess|dlss|default`
  (and the viewer menu's Upscaler row) rewrites its [Upscalers] and [DLSS]
  outputs. Files that are already there are not replaced unless -Force; every
  file this script copied is listed in conduit-optiscaler.txt in the game
  folder, which -Uninstall reads to remove exactly those.

  Do not use OptiScaler in online games with anti-cheat: it injects into the
  game process and can get an account banned.

.PARAMETER Game
  The game's executable, or the folder that holds it (for Unreal Engine
  games the one under ...\Binaries\Win64).

.PARAMETER Source
  The extracted OptiScaler release (the folder with OptiScaler.dll and
  OptiScaler.ini).

.PARAMETER Proxy
  The DLL name OptiScaler loads as: dxgi (default), winmm, version, dbghelp,
  d3d12, wininet or winhttp. Pick another one when the game ships its own
  dxgi.dll or another mod already uses it.

.PARAMETER Force
  Replace files that are already in the game folder.

.PARAMETER Uninstall
  Remove what an earlier install of this script copied, and the registry entry.

.EXAMPLE
  .\Install-OptiScaler.ps1 -Game 'G:\SteamLibrary\steamapps\common\Cyberpunk 2077\bin\x64\Cyberpunk2077.exe' -Source "$env:USERPROFILE\Downloads\OptiScaler_0.9.4"
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(Mandatory = $true)] [string] $Game,
    [Parameter(ParameterSetName = 'Install', Mandatory = $true)] [string] $Source,
    [Parameter(ParameterSetName = 'Install')]
    [ValidateSet('dxgi', 'winmm', 'version', 'dbghelp', 'd3d12', 'wininet', 'winhttp')]
    [string] $Proxy = 'dxgi',
    [Parameter(ParameterSetName = 'Install')] [switch] $Force,
    [Parameter(ParameterSetName = 'Uninstall', Mandatory = $true)] [switch] $Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this from an elevated PowerShell (it writes HKLM\SOFTWARE\Helios\OptiScaler).'
}

$gamePath = (Resolve-Path -LiteralPath $Game).Path
$dir = if (Test-Path -LiteralPath $gamePath -PathType Leaf) { Split-Path -Parent $gamePath } else { $gamePath }
$marker = Join-Path $dir 'conduit-optiscaler.txt'
$ini = Join-Path $dir 'OptiScaler.ini'
$regKey = 'HKLM:\SOFTWARE\Helios\OptiScaler'

if ($Uninstall) {
    if (-not (Test-Path -LiteralPath $marker)) {
        throw "No conduit-optiscaler.txt in $dir`: nothing this script installed is there."
    }
    $files = Get-Content -LiteralPath $marker | Where-Object { $_ -and -not $_.StartsWith('#') }
    foreach ($rel in $files) {
        # only names inside the game folder, never an absolute or parent path
        if ([IO.Path]::IsPathRooted($rel) -or $rel -match '(^|\\)\.\.(\\|$)') { continue }
        $p = Join-Path $dir $rel
        if (Test-Path -LiteralPath $p -PathType Leaf) { Remove-Item -LiteralPath $p -Force }
    }
    Remove-Item -LiteralPath $marker -Force
    if (Test-Path $regKey) {
        Remove-ItemProperty -Path $regKey -Name $dir -ErrorAction SilentlyContinue
    }
    Write-Host "OptiScaler removed from $dir"
    return
}

$src = (Resolve-Path -LiteralPath $Source).Path
foreach ($need in 'OptiScaler.dll', 'OptiScaler.ini') {
    if (-not (Test-Path -LiteralPath (Join-Path $src $need) -PathType Leaf)) {
        throw "$src has no $need`: give the extracted OptiScaler release folder."
    }
}

# What goes where: every file of the release keeps its relative path, but
# OptiScaler.dll becomes the proxy DLL. Setup scripts and docs stay behind.
$plan = @()
Get-ChildItem -LiteralPath $src -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($src.Length).TrimStart('\')
    if ($rel -match '\.(bat|cmd|sh|md|txt)$') { return }
    $to = if ($rel -ieq 'OptiScaler.dll') { "$Proxy.dll" } else { $rel }
    $plan += [pscustomobject]@{ From = $_.FullName; To = $to }
}

$clash = @($plan | Where-Object { Test-Path -LiteralPath (Join-Path $dir $_.To) })
if ($clash.Count -and -not $Force) {
    $names = ($clash | ForEach-Object { $_.To }) -join ', '
    throw "Already in $dir`: $names. Use -Force to replace them, or another -Proxy."
}

$copied = @("# copied by Install-OptiScaler.ps1 (Conduit); -Uninstall removes these")
foreach ($f in $plan) {
    $dst = Join-Path $dir $f.To
    $parent = Split-Path -Parent $dst
    if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent | Out-Null }
    Copy-Item -LiteralPath $f.From -Destination $dst -Force
    $copied += $f.To
}
Set-Content -LiteralPath $marker -Value $copied -Encoding UTF8

if (-not (Test-Path $regKey)) { New-Item -Path $regKey | Out-Null }
Set-ItemProperty -Path $regKey -Name $dir -Value $ini -Type String

Write-Host "OptiScaler installed in $dir as $Proxy.dll ($($plan.Count) files)."
Write-Host "Pick the upscaler from the host: conduit upscaler VM fsr|xess|dlss|default (or the viewer menu)."

<#
.SYNOPSIS
  Set up DLSS on NVK for one Windows guest (experimental), or take it away again.

.DESCRIPTION
  Conduit ships no NVIDIA files. This script copies files you already have:

  - nvngx.dll and _nvngx.dll (the NGX runtime) from the host driver's
    nvidia/wine folder (for example /usr/lib/x86_64-linux-gnu/nvidia/wine on
    the host), shared into the guest; they go to C:\ProgramData\Helios\ngx\.
  - nvapi64.dll built with guest/windows/tools/build-dxvk-nvapi.sh; it goes
    next to the game (-Game) or into System32 (-SystemNvapi).
  - the RTX 50 cubin cache built on the host with nvk-cubin-cache.py
    (-CubinCache); it goes to C:\ProgramData\Helios\ngx\cubin\.

  It sets HKLM\SOFTWARE\Helios Dlss = 1 and points the NGX loader at
  C:\ProgramData\Helios\ngx: NGXCore NGXPath under our KMD's service key
  (and its Parameters subkey) and HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore
  FullPath. With -Game the runtime is also copied next to the game, the first
  place the loader looks. -Log turns on the NGX log, the DXVK-NVAPI log and the
  DLSS indicator.

  Everything changed is recorded in C:\ProgramData\Helios\ngx\conduit-dlss.json;
  -Uninstall restores the previous registry values and files from it.
  Files already in place are not replaced unless -Force (then they are kept as
  <name>.conduit-bak and restored by -Uninstall).

  Run from an elevated PowerShell. New processes pick the change up; restart
  the game (and Steam) after installing.

.PARAMETER NgxSource
  Folder with nvngx.dll and _nvngx.dll from the host driver.

.PARAMETER Nvapi
  The nvapi64.dll to install. Default: nvapi64.dll in -NgxSource.

.PARAMETER CubinCache
  Folder with the host-built cubin cache (*.cubin, index.tsv). RTX 50 only.

.PARAMETER Game
  The game's executable, or its folder. nvapi64.dll, nvngx.dll and _nvngx.dll
  are copied next to it.

.PARAMETER SystemNvapi
  Put nvapi64.dll into System32 instead (every 64-bit process then sees an
  NVIDIA NvAPI; some games change behaviour on that).

.PARAMETER DriverVersion
  The driver version DXVK-NVAPI reports, as a number (61057 = 610.57).
  Default: what DXVK-NVAPI reports on NVK (999.99).

.PARAMETER Log
  NGX log (LogLevel 2) and DXVK-NVAPI log (info) to C:\ProgramData\Helios\logs\dlss,
  and the DLSS on-screen indicator.

.PARAMETER Force
  Replace files that are already there (kept as .conduit-bak).

.PARAMETER Uninstall
  Undo what earlier runs of this script did.

.EXAMPLE
  .\Install-Dlss.ps1 -NgxSource Z:\dlss -CubinCache Z:\dlss\cubin -Game 'G:\SteamLibrary\steamapps\common\No Man''s Sky\Binaries\NMS.exe' -Log

.EXAMPLE
  .\Install-Dlss.ps1 -Uninstall
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(ParameterSetName = 'Install', Mandatory = $true)] [string] $NgxSource,
    [Parameter(ParameterSetName = 'Install')] [string] $Nvapi,
    [Parameter(ParameterSetName = 'Install')] [string] $CubinCache,
    [Parameter(ParameterSetName = 'Install')] [string] $Game,
    [Parameter(ParameterSetName = 'Install')] [switch] $SystemNvapi,
    [Parameter(ParameterSetName = 'Install')] [int] $DriverVersion = 0,
    [Parameter(ParameterSetName = 'Install')] [switch] $Log,
    [Parameter(ParameterSetName = 'Install')] [switch] $Force,
    [Parameter(ParameterSetName = 'Uninstall', Mandatory = $true)] [switch] $Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this from an elevated PowerShell (it writes HKLM and C:\ProgramData\Helios).'
}

$ngxDir = Join-Path $env:ProgramData 'Helios\ngx'
$cubinDir = Join-Path $ngxDir 'cubin'
$logDir = Join-Path $env:ProgramData 'Helios\logs\dlss'
$statePath = Join-Path $ngxDir 'conduit-dlss.json'
$kmdService = 'HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
$heliosKey = 'HKLM:\SOFTWARE\Helios'
$nvapiKey = 'HKLM:\SOFTWARE\Helios\Nvapi'
$ngxCoreKey = 'HKLM:\SOFTWARE\NVIDIA Corporation\Global\NGXCore'

# --- state ------------------------------------------------------------------

function Read-State {
    if (Test-Path -LiteralPath $statePath) {
        $s = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
        return @{
            Files = [System.Collections.ArrayList]@($s.Files | Where-Object { $_ })
            Dirs = [System.Collections.ArrayList]@($s.Dirs | Where-Object { $_ })
            Keys = [System.Collections.ArrayList]@($s.Keys | Where-Object { $_ })
            Values = [System.Collections.ArrayList]@($s.Values | Where-Object { $_ })
        }
    }
    return @{
        Files = New-Object System.Collections.ArrayList
        Dirs = New-Object System.Collections.ArrayList
        Keys = New-Object System.Collections.ArrayList
        Values = New-Object System.Collections.ArrayList
    }
}

function Save-State($State) {
    $State | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $statePath -Encoding UTF8
}

function Test-Recorded($List, [string] $Property, [string] $Value) {
    foreach ($e in $List) {
        if ($Property) { if ($e.$Property -eq $Value) { return $true } }
        elseif ($e -eq $Value) { return $true }
    }
    return $false
}

# --- changes, each recorded before it is made --------------------------------

function New-RecordedDirectory($State, [string] $Path) {
    if (Test-Path -LiteralPath $Path) { return }
    $parent = Split-Path -Parent $Path
    if ($parent -and -not (Test-Path -LiteralPath $parent)) { New-RecordedDirectory $State $parent }
    New-Item -ItemType Directory -Path $Path | Out-Null
    [void]$State.Dirs.Add($Path)
    Save-State $State
}

function Copy-RecordedFile($State, [string] $Source, [string] $Destination, [switch] $Defer) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Missing: $Source" }
    $known = Test-Recorded $State.Files 'Path' $Destination
    $backup = $null
    if ((Test-Path -LiteralPath $Destination) -and -not $known) {
        if (-not $Force) {
            throw "$Destination already exists and was not put there by this script; use -Force to replace it (kept as .conduit-bak)."
        }
        $backup = "$Destination.conduit-bak"
        Copy-Item -LiteralPath $Destination -Destination $backup -Force
    }
    if (-not $known) {
        [void]$State.Files.Add([pscustomobject]@{ Path = $Destination; Backup = $backup })
        if (-not $Defer) { Save-State $State }
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function Set-RecordedValue($State, [string] $Key, [string] $Name, $Value, [string] $Type) {
    Set-RecordedKey $State $Key
    $id = "$Key|$Name"
    if (-not (Test-Recorded $State.Values 'Id' $id)) {
        $item = Get-ItemProperty -LiteralPath $Key -Name $Name -ErrorAction SilentlyContinue
        $old = $null; $oldType = $null
        if ($null -ne $item) {
            $old = $item.$Name
            $oldType = (Get-Item -LiteralPath $Key).GetValueKind($Name).ToString()
        }
        [void]$State.Values.Add([pscustomobject]@{ Id = $id; Key = $Key; Name = $Name; Existed = ($null -ne $item); Old = $old; OldType = $oldType })
        Save-State $State
    }
    # Set-ItemProperty/New-ItemProperty only: New-Item -Force on a key wipes its values
    if ($null -ne (Get-ItemProperty -LiteralPath $Key -Name $Name -ErrorAction SilentlyContinue)) {
        Remove-ItemProperty -LiteralPath $Key -Name $Name
    }
    New-ItemProperty -LiteralPath $Key -Name $Name -Value $Value -PropertyType $Type | Out-Null
}

function Set-RecordedKey($State, [string] $Key) {
    if (Test-Path -LiteralPath $Key) { return }
    Set-RecordedKey $State (Split-Path -Parent $Key)
    [void]$State.Keys.Add($Key)
    Save-State $State
    New-Item -Path $Key | Out-Null
}

# --- uninstall ---------------------------------------------------------------

if ($Uninstall) {
    if (-not (Test-Path -LiteralPath $statePath)) {
        throw "No $statePath`: nothing this script installed is recorded."
    }
    $state = Read-State
    $failed = 0
    for ($i = $state.Values.Count - 1; $i -ge 0; $i--) {
        $v = $state.Values[$i]
        try {
            if (-not (Test-Path -LiteralPath $v.Key)) { continue }
            if ($null -ne (Get-ItemProperty -LiteralPath $v.Key -Name $v.Name -ErrorAction SilentlyContinue)) {
                Remove-ItemProperty -LiteralPath $v.Key -Name $v.Name
            }
            if ($v.Existed) {
                $value = $v.Old
                if ($v.OldType -eq 'MultiString') { $value = [string[]]@($v.Old) }
                if ($v.OldType -eq 'Binary') { $value = [byte[]]@($v.Old) }
                New-ItemProperty -LiteralPath $v.Key -Name $v.Name -Value $value -PropertyType $v.OldType | Out-Null
            }
            Write-Host "restored $($v.Key)\$($v.Name)"
        } catch { $failed++; Write-Warning "could not restore $($v.Key)\$($v.Name): $_" }
    }
    for ($i = $state.Keys.Count - 1; $i -ge 0; $i--) {
        $k = $state.Keys[$i]
        try {
            if ((Test-Path -LiteralPath $k) -and @(Get-ChildItem -LiteralPath $k).Count -eq 0 -and
                @((Get-Item -LiteralPath $k).GetValueNames()).Count -eq 0) {
                Remove-Item -LiteralPath $k
                Write-Host "removed key $k"
            }
        } catch { $failed++; Write-Warning "could not remove key ${k}: $_" }
    }
    foreach ($f in $state.Files) {
        try {
            if (Test-Path -LiteralPath $f.Path) { Remove-Item -LiteralPath $f.Path -Force }
            if ($f.Backup -and (Test-Path -LiteralPath $f.Backup)) { Move-Item -LiteralPath $f.Backup -Destination $f.Path }
            Write-Host "removed $($f.Path)"
        } catch { $failed++; Write-Warning "could not remove $($f.Path) (in use? close the game): $_" }
    }
    if ($failed -gt 0) { throw "$failed changes could not be undone; $statePath is kept, run -Uninstall again." }
    Remove-Item -LiteralPath $statePath -Force
    for ($i = $state.Dirs.Count - 1; $i -ge 0; $i--) {
        $d = $state.Dirs[$i]
        if ((Test-Path -LiteralPath $d) -and @(Get-ChildItem -LiteralPath $d -Force).Count -eq 0) {
            Remove-Item -LiteralPath $d
        }
    }
    Write-Host 'DLSS setup removed. Restart the game (and Steam).'
    return
}

# --- install -----------------------------------------------------------------

$NgxSource = (Resolve-Path -LiteralPath $NgxSource).Path
if (-not $Nvapi) { $Nvapi = Join-Path $NgxSource 'nvapi64.dll' }
$Nvapi = (Resolve-Path -LiteralPath $Nvapi).Path
foreach ($f in @('nvngx.dll', '_nvngx.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $NgxSource $f))) { throw "No $f in $NgxSource (copy it from the host driver's nvidia/wine folder)." }
}
if (-not $Game -and -not $SystemNvapi) { throw 'Pass -Game (nvapi64.dll next to the game) or -SystemNvapi.' }
if (-not (Test-Path -LiteralPath $kmdService)) { throw "No $kmdService`: is the Helios driver installed?" }

$state = Read-State
New-RecordedDirectory $state $ngxDir

foreach ($f in @('nvngx.dll', '_nvngx.dll')) {
    Copy-RecordedFile $state (Join-Path $NgxSource $f) (Join-Path $ngxDir $f)
}
$sig = Get-AuthenticodeSignature -LiteralPath (Join-Path $ngxDir '_nvngx.dll')
$signer = if ($sig.SignerCertificate) { $sig.SignerCertificate.Subject } else { 'no signer' }
Write-Host "_nvngx.dll signature: $($sig.Status) ($signer)"
if ($sig.Status -ne 'Valid') { Write-Warning 'The NGX loader checks this signature; a status other than Valid will likely stop it.' }

if ($CubinCache) {
    $CubinCache = (Resolve-Path -LiteralPath $CubinCache).Path
    New-RecordedDirectory $state $cubinDir
    $n = 0
    $cubins = @(Get-ChildItem -LiteralPath $CubinCache -File | Where-Object { $_.Name -like '*.cubin' -or $_.Name -eq 'index.tsv' })
    foreach ($c in $cubins) {
        Copy-RecordedFile $state $c.FullName (Join-Path $cubinDir $c.Name) -Defer
        $n++
    }
    Save-State $state
    Write-Host "cubin cache: $n files in $cubinDir"
}

if ($Game) {
    $gamePath = (Resolve-Path -LiteralPath $Game).Path
    $gameDir = if (Test-Path -LiteralPath $gamePath -PathType Leaf) { Split-Path -Parent $gamePath } else { $gamePath }
    Copy-RecordedFile $state $Nvapi (Join-Path $gameDir 'nvapi64.dll')
    foreach ($f in @('nvngx.dll', '_nvngx.dll')) {
        Copy-RecordedFile $state (Join-Path $NgxSource $f) (Join-Path $gameDir $f)
    }
    Write-Host "game folder: $gameDir"
}
if ($SystemNvapi) {
    Copy-RecordedFile $state $Nvapi (Join-Path $env:SystemRoot 'System32\nvapi64.dll')
}

Set-RecordedValue $state $heliosKey 'Dlss' 1 'DWord'
Set-RecordedValue $state "$kmdService\NGXCore" 'NGXPath' $ngxDir 'String'
Set-RecordedValue $state "$kmdService\Parameters\NGXCore" 'NGXPath' $ngxDir 'String'
Set-RecordedValue $state $ngxCoreKey 'FullPath' $ngxDir 'String'
if ($DriverVersion -gt 0) {
    Set-RecordedValue $state $nvapiKey 'DXVK_NVAPI_DRIVER_VERSION' ([string]$DriverVersion) 'String'
}
if ($Log) {
    New-RecordedDirectory $state $logDir
    # Users (games) may write their logs there
    $acl = Get-Acl -LiteralPath $logDir
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        [Security.Principal.SecurityIdentifier]'S-1-5-32-545', 'Modify', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    Set-Acl -LiteralPath $logDir -AclObject $acl
    Set-RecordedValue $state $ngxCoreKey 'LogLevel' 2 'DWord'
    Set-RecordedValue $state $ngxCoreKey 'EnableLogPathOverride' 1 'DWord'
    Set-RecordedValue $state $ngxCoreKey 'LogPath' $logDir 'String'
    Set-RecordedValue $state $ngxCoreKey 'ShowDlssIndicator' 1024 'DWord'
    Set-RecordedValue $state $nvapiKey 'DXVK_NVAPI_LOG_LEVEL' 'info' 'String'
    Set-RecordedValue $state $nvapiKey 'DXVK_NVAPI_LOG_PATH' $logDir 'String'
}

Write-Host "DLSS set up (Dlss = 1). Changes are recorded in $statePath; undo with -Uninstall."
Write-Host 'Restart the game (and Steam) so new processes pick it up.'

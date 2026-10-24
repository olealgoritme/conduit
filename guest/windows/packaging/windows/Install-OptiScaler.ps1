<#
.SYNOPSIS
  Put OptiScaler next to one game (downloaded, hash-checked), set its
  upscaler and frame generation, or take it away again.

.DESCRIPTION
  OptiScaler (github.com/optiscaler/OptiScaler, GPL-3.0) hooks a game's DLSS,
  FSR or XeSS calls and hands them to the upscaler you pick. Conduit does not
  ship it: this script downloads the release pinned in optiscaler-manifest.txt
  (next to this script) from GitHub over HTTPS, checks its SHA-256, extracts
  it (Windows' tar.exe; 7-Zip's 7zr.exe, also pinned and hash-checked, when
  tar cannot read it) and keeps it with its LICENSE in
  C:\ProgramData\Helios\optiscaler\<version>\. -Source takes an extracted
  release instead (offline).

  Install copies the release next to the game's executable, renames
  OptiScaler.dll to the proxy DLL name (dxgi.dll, or the first of winmm,
  version, dbghelp, wininet, winhttp the game does not have itself; -Proxy
  picks one), and lists the game's OptiScaler.ini under
  HKLM\SOFTWARE\Helios\OptiScaler, so `conduit upscaler VM fsr|xess|dlss|default`
  (and the viewer menu's Upscaler row) rewrites its [Upscalers] and [DLSS]
  outputs. Every file it copied is listed in conduit-optiscaler.txt in the
  game folder; -Uninstall removes exactly those. Installing again updates the
  files and keeps the game's OptiScaler.ini (-ResetIni replaces it).

  OptiScaler.ini on a new install:
  - upscaler: the -Upscaler mode; with auto (the default) the guest-wide
    Upscaler setting when one is set, else DLSS when Install-Dlss.ps1 has set
    up the NGX runtime and nvapi64.dll, else FSR 3.1;
  - with that DLSS setup, [NvApi] OverrideNvapiDll=false: OptiScaler uses the
    guest's nvapi64.dll (DXVK-NVAPI) instead of its fakenvapi;
  - frame generation off unless -FrameGen.

  Refused, unless -Force: games with kernel or online anti-cheat (the list is
  in optiscaler-manifest.txt: CS2, Dota 2, Valorant, Fortnite, Apex, PUBG,
  Rainbow Six and others, and any game folder with Easy Anti-Cheat,
  BattlEye, EA's or Riot's anti-cheat files). OptiScaler injects a DLL; with
  these games that can get the account banned.

  Nothing is changed while a program from the game folder runs: the script
  stops (exit code 3), or with -WaitForExit waits until it has closed.

  Exit codes: 0 done, 1 error, 2 anti-cheat (refused), 3 the game is running,
  4 OptiScaler is not installed there (-Configure, -Uninstall).

.PARAMETER Game
  The game's executable, or the folder that holds it (for Unreal Engine
  games the one under ...\Binaries\Win64).

.PARAMETER Source
  An extracted OptiScaler release (the folder with OptiScaler.dll and
  OptiScaler.ini) instead of the download.

.PARAMETER Download
  Download the release (the default when -Source is not given).

.PARAMETER Version
  The release to download: the pinned one by default; `latest` (the newest
  GitHub release) or a version number (0.9.4) asks GitHub's API and checks the
  SHA-256 GitHub lists for the asset, or -Sha256. The hash is printed, for
  pinning it in optiscaler-manifest.txt.

.PARAMETER Sha256
  The expected SHA-256 of the release archive for a -Version that is not the
  pinned one.

.PARAMETER DownloadOnly
  Download, check and extract the release into the cache; no game.

.PARAMETER Proxy
  The DLL name OptiScaler loads as: dxgi, winmm, version, dbghelp, d3d12,
  wininet or winhttp. Default: what an earlier install used, else dxgi, or
  the first the game folder does not have. For Vulkan games winmm usually
  works better.

.PARAMETER Upscaler
  auto (default), dlss, fsr (FSR 3.1), xess, or default (OptiScaler's own
  choice). With -Configure, or when installing again, only written when given.

.PARAMETER FrameGen
  off, on (OptiScaler's own frame generation, OptiFG: FSR 3.1 frame
  generation from the upscaler's inputs, DX12 games) or dlssg (the game's
  DLSS frame generation inputs to FSR 3.1 frame generation). Default on a new
  install: off.

.PARAMETER ResetIni
  Replace the game's OptiScaler.ini with the release's (the old one is kept
  as OptiScaler.ini.conduit-bak).

.PARAMETER Force
  Install even when the game has anti-cheat, and replace files that are
  already in the game folder and were not put there by this script.

.PARAMETER Configure
  Only change OptiScaler.ini (-Upscaler, -FrameGen) of an existing install.

.PARAMETER Uninstall
  Remove what an earlier install of this script copied, and the registry entry.

.PARAMETER WaitForExit
  When a program from the game folder runs, wait until it has closed (at most
  -WaitMinutes), then go on.

.EXAMPLE
  .\Install-OptiScaler.ps1 -Game 'G:\SteamLibrary\steamapps\common\Cyberpunk 2077\bin\x64\Cyberpunk2077.exe'

.EXAMPLE
  .\Install-OptiScaler.ps1 -Game 'G:\...\Game.exe' -Configure -Upscaler xess -FrameGen on

.EXAMPLE
  .\Install-OptiScaler.ps1 -Game 'G:\...\Game.exe' -Uninstall

.EXAMPLE
  .\Install-OptiScaler.ps1 -DownloadOnly -Version latest
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(ParameterSetName = 'Install', Mandatory = $true, Position = 0)]
    [Parameter(ParameterSetName = 'Configure', Mandatory = $true, Position = 0)]
    [Parameter(ParameterSetName = 'Uninstall', Mandatory = $true, Position = 0)]
    [string] $Game,
    [Parameter(ParameterSetName = 'Install')] [string] $Source,
    [Parameter(ParameterSetName = 'Install')] [switch] $Download,
    [Parameter(ParameterSetName = 'Install')] [Parameter(ParameterSetName = 'Fetch')] [string] $Version,
    [Parameter(ParameterSetName = 'Install')] [Parameter(ParameterSetName = 'Fetch')] [string] $Sha256,
    [Parameter(ParameterSetName = 'Fetch', Mandatory = $true)] [switch] $DownloadOnly,
    [Parameter(ParameterSetName = 'Install')]
    [ValidateSet('dxgi', 'winmm', 'version', 'dbghelp', 'd3d12', 'wininet', 'winhttp')]
    [string] $Proxy,
    [Parameter(ParameterSetName = 'Install')] [Parameter(ParameterSetName = 'Configure')]
    [ValidateSet('auto', 'dlss', 'fsr', 'xess', 'default')]
    [string] $Upscaler,
    [Parameter(ParameterSetName = 'Install')] [Parameter(ParameterSetName = 'Configure')]
    [ValidateSet('off', 'on', 'optifg', 'dlssg')]
    [string] $FrameGen,
    [Parameter(ParameterSetName = 'Install')] [switch] $ResetIni,
    [Parameter(ParameterSetName = 'Install')] [switch] $Force,
    [Parameter(ParameterSetName = 'Configure', Mandatory = $true)] [switch] $Configure,
    [Parameter(ParameterSetName = 'Uninstall', Mandatory = $true)] [switch] $Uninstall,
    [Parameter(ParameterSetName = 'Install')] [Parameter(ParameterSetName = 'Configure')] [Parameter(ParameterSetName = 'Uninstall')]
    [switch] $WaitForExit,
    [Parameter(ParameterSetName = 'Install')] [Parameter(ParameterSetName = 'Configure')] [Parameter(ParameterSetName = 'Uninstall')]
    [ValidateRange(1, 1440)] [int] $WaitMinutes = 720
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$markerName = 'conduit-optiscaler.txt'
$regKey = 'HKLM:\SOFTWARE\Helios\OptiScaler'
$cacheRoot = Join-Path $env:ProgramData 'Helios\optiscaler'
$manifestPath = Join-Path $PSScriptRoot 'optiscaler-manifest.txt'
$proxyOrder = @('dxgi', 'winmm', 'version', 'dbghelp', 'wininet', 'winhttp')
$apiRoot = 'https://api.github.com/repos/optiscaler/OptiScaler'

# OptiScaler.ini values per upscaler mode: mode, section, key, value. The
# tray's upscaler.rs (the guest-wide `upscaler` setting) has the same table;
# a test there checks that they match.
$modeTable = @'
dlss     Upscalers  Dx11Upscaler          dlss
dlss     Upscalers  Dx12Upscaler          dlss
dlss     Upscalers  VulkanUpscaler        dlss
dlss     DLSS       RenderPresetOverride  true
dlss     DLSS       RenderPresetForAll    11
fsr      Upscalers  Dx11Upscaler          fsr31
fsr      Upscalers  Dx12Upscaler          fsr31
fsr      Upscalers  VulkanUpscaler        fsr31
fsr      DLSS       RenderPresetOverride  auto
fsr      DLSS       RenderPresetForAll    auto
xess     Upscalers  Dx11Upscaler          xess_12
xess     Upscalers  Dx12Upscaler          xess
xess     Upscalers  VulkanUpscaler        xess
xess     DLSS       RenderPresetOverride  auto
xess     DLSS       RenderPresetForAll    auto
default  Upscalers  Dx11Upscaler          auto
default  Upscalers  Dx12Upscaler          auto
default  Upscalers  VulkanUpscaler        auto
default  DLSS       RenderPresetOverride  auto
default  DLSS       RenderPresetForAll    auto
'@

# Frame generation: OptiFG feeds FSR 3.1 frame generation from the
# upscaler's inputs (DX12); dlssg takes a game's DLSS-FG inputs instead.
$frameGenTable = @'
off      FrameGen   Enabled               false
off      FrameGen   FGInput               nofg
off      FrameGen   FGOutput              nofg
optifg   FrameGen   Enabled               true
optifg   FrameGen   FGInput               upscaler
optifg   FrameGen   FGOutput              fsrfg
dlssg    FrameGen   Enabled               true
dlssg    FrameGen   FGInput               dlssg
dlssg    FrameGen   FGOutput              fsrfg
'@

# ---------------------------------------------------------------- helpers

function Stop-With([int] $Code, [string] $Message) {
    $e = New-Object System.Exception $Message
    $e.Data['ConduitExit'] = $Code
    throw $e
}

function Get-TableValues([string] $Table, [string] $Mode) {
    $out = @()
    foreach ($line in ($Table -split "`r?`n")) {
        $f = @($line.Trim() -split '\s+')
        if ($f.Count -eq 4 -and $f[0] -eq $Mode) { $out += , @($f[1], $f[2], $f[3]) }
    }
    if ($out.Count -eq 0) { throw "No table entries for $Mode" }
    return , $out
}

function Read-Manifest([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Path is missing: keep optiscaler-manifest.txt next to this script."
    }
    $m = @{ Exe = @(); Paths = @(); Roots = @() }
    foreach ($raw in [IO.File]::ReadAllLines($Path)) {
        $line = $raw.Trim()
        if (-not $line -or $line.StartsWith('#')) { continue }
        $parts = @($line -split '\s+', 2)
        if ($parts.Count -lt 2) { continue }
        $key = $parts[0]; $rest = $parts[1].Trim()
        $vl = @($rest -split '\s+', 2)
        $label = if ($vl.Count -gt 1) { $vl[1].Trim() } else { $vl[0] }
        switch -Exact ($key) {
            'anticheat.exe' { $m.Exe += , @($vl[0], $label) }
            'anticheat.path' { $m.Paths += , @($vl[0], $label) }
            'anticheat.root' { $m.Roots += $rest }
            default { $m[$key] = $vl[0] }
        }
    }
    foreach ($need in 'release.version', 'release.url', 'release.sha256', 'license.url', 'license.sha256') {
        if (-not $m.ContainsKey($need)) { throw "$Path has no $need line." }
    }
    return $m
}

function Test-NameMatch([string] $Name, [string] $Pattern) {
    if ($Pattern.EndsWith('*')) {
        return $Name.StartsWith($Pattern.Substring(0, $Pattern.Length - 1), [StringComparison]::OrdinalIgnoreCase)
    }
    return [string]::Equals($Name, $Pattern, [StringComparison]::OrdinalIgnoreCase)
}

# The folders searched for anti-cheat files: the game's folder and up to four
# above it, stopping below a library folder (anticheat.root) or a drive root.
function Get-AntiCheatFolders([string] $Dir, $Manifest) {
    $out = @($Dir)
    $d = New-Object System.IO.DirectoryInfo $Dir
    for ($i = 0; $i -lt 4; $i++) {
        $p = $d.Parent
        if ($null -eq $p -or $null -eq $p.Parent) { break }
        $isRoot = $false
        foreach ($r in $Manifest.Roots) { if ([string]::Equals($p.Name, $r, [StringComparison]::OrdinalIgnoreCase)) { $isRoot = $true } }
        if ($isRoot) { break }
        $out += $p.FullName
        $d = $p
    }
    return , $out
}

# The anti-cheat the game has (its label), or $null.
function Find-AntiCheat([string] $Dir, [string] $ExeName, $Manifest) {
    $exes = @()
    if ($ExeName) { $exes += $ExeName }
    $exes += @(Get-ChildItem -LiteralPath $Dir -Filter '*.exe' -File -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    foreach ($e in $exes) {
        foreach ($a in $Manifest.Exe) { if (Test-NameMatch $e $a[0]) { return "$($a[1]) ($e)" } }
    }
    foreach ($folder in (Get-AntiCheatFolders $Dir $Manifest)) {
        foreach ($c in @(Get-ChildItem -LiteralPath $folder -Force -ErrorAction SilentlyContinue)) {
            foreach ($a in $Manifest.Paths) { if (Test-NameMatch $c.Name $a[0]) { return "$($a[1]) ($($c.FullName))" } }
        }
    }
    return $null
}

# Programs running from the game folder (or below it), as "name (pid)".
function Get-GameProcesses([string] $Dir) {
    $prefix = $Dir.TrimEnd('\') + '\'
    $out = @()
    foreach ($p in @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue)) {
        $path = [string]$p.ExecutablePath
        if ($path -and $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
            $out += "$($p.Name) ($($p.ProcessId))"
        }
    }
    return , $out
}

function Wait-GameClosed([string] $Dir) {
    $running = Get-GameProcesses $Dir
    if ($running.Count -eq 0) { return }
    if (-not $WaitForExit) {
        Stop-With 3 "The game is running ($($running -join ', ')); close it first, or pass -WaitForExit."
    }
    Write-Host "Waiting for $($running -join ', ') to close (at most $WaitMinutes minutes)..."
    $until = (Get-Date).AddMinutes($WaitMinutes)
    while ((Get-GameProcesses $Dir).Count -gt 0) {
        if ((Get-Date) -gt $until) { Stop-With 3 "The game is still running after $WaitMinutes minutes; nothing changed." }
        Start-Sleep -Seconds 5
    }
    # Launchers sometimes restart the game right away; look once more.
    Start-Sleep -Seconds 3
    $running = Get-GameProcesses $Dir
    if ($running.Count -gt 0) { Stop-With 3 "The game started again ($($running -join ', ')); nothing changed." }
}

# A native tool's exit code and output, without Windows PowerShell turning
# its stderr into a terminating error.
function Invoke-Native([string] $Exe, [string[]] $ArgList) {
    $ErrorActionPreference = 'Continue'
    $out = & $Exe @ArgList 2>&1 | ForEach-Object { "$_" }
    return [pscustomobject]@{ Code = $LASTEXITCODE; Output = (@($out) -join ' ') }
}

function Get-Sha256([string] $Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Save-Url([string] $Url, [string] $Path) {
    if (-not $Url.StartsWith('https://')) { throw "Not an HTTPS URL: $Url" }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing -TimeoutSec 900 -Headers @{ 'User-Agent' = 'conduit-install-optiscaler' }
}

function Save-Verified([string] $Url, [string] $Sha, [string] $Path) {
    Save-Url $Url $Path
    $got = Get-Sha256 $Path
    if ($got -ne $Sha.ToLowerInvariant()) {
        Remove-Item -LiteralPath $Path -Force
        throw "SHA-256 mismatch for $Url`: expected $Sha, got $got. Nothing was installed."
    }
}

function Invoke-GitHubApi([string] $Path) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    return Invoke-RestMethod -Uri "$apiRoot/$Path" -UseBasicParsing -TimeoutSec 60 -Headers @{
        'User-Agent' = 'conduit-install-optiscaler'; 'Accept' = 'application/vnd.github+json' }
}

# Admins and SYSTEM change it, users only read: an elevated install copies
# from here into game folders.
function Initialize-Cache {
    $helios = Join-Path $env:ProgramData 'Helios'
    foreach ($d in $helios, $cacheRoot) {
        if (-not (Test-Path -LiteralPath $d)) { New-Item -ItemType Directory -Path $d | Out-Null }
        if ((Get-Item -LiteralPath $d -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "$d is a link; remove it first."
        }
    }
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetOwner([Security.Principal.SecurityIdentifier]'S-1-5-32-544')
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach ($rule in @(@('S-1-5-18', 'FullControl'), @('S-1-5-32-544', 'FullControl'), @('S-1-5-32-545', 'ReadAndExecute'))) {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            [Security.Principal.SecurityIdentifier]$rule[0], $rule[1], $inherit, 'None', 'Allow')))
    }
    Set-Acl -LiteralPath $cacheRoot -AclObject $acl
}

# What to download: the pinned release, or one GitHub's API names.
function Get-ReleaseInfo($Manifest) {
    $pinned = $Manifest['release.version']
    if (-not $Version -or $Version -eq $pinned -or $Version -eq "v$pinned") {
        return [pscustomobject]@{
            Version = $pinned; Url = $Manifest['release.url']; Sha256 = $Manifest['release.sha256']
            LicenseUrl = $Manifest['license.url']; LicenseSha256 = $Manifest['license.sha256']
            SourceUrl = $Manifest['source.url']; Pinned = $true
        }
    }
    $rel = if ($Version -eq 'latest') { Invoke-GitHubApi 'releases/latest' } else {
        $tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
        Invoke-GitHubApi "releases/tags/$tag"
    }
    $asset = @($rel.assets | Where-Object { $_.name -like '*.7z' }) | Select-Object -First 1
    if ($null -eq $asset) { throw "Release $($rel.tag_name) has no .7z asset." }
    $listed = $null
    if (($asset.PSObject.Properties.Name -contains 'digest') -and $asset.digest -and ([string]$asset.digest).StartsWith('sha256:')) {
        $listed = ([string]$asset.digest).Substring(7).ToLowerInvariant()
    }
    $expect = if ($Sha256) { $Sha256.ToLowerInvariant() } else { $listed }
    if (-not $expect) { throw "GitHub lists no SHA-256 for $($asset.name); pass -Sha256." }
    if ($Sha256 -and $listed -and $listed -ne $expect) {
        throw "-Sha256 $expect does not match the SHA-256 GitHub lists for $($asset.name) ($listed)."
    }
    $ver = ([string]$rel.tag_name).TrimStart('v')
    if ($ver -notmatch '^[0-9A-Za-z._-]+$') { throw "Unexpected tag name $($rel.tag_name)." }
    $which = if ($ver -eq $pinned) { 'the pinned release' } else { "pinned: $pinned" }
    Write-Host "OptiScaler $($rel.tag_name) ($which), for optiscaler-manifest.txt:"
    if ($ver -eq $pinned -and $expect -ne $Manifest['release.sha256']) {
        Write-Warning "GitHub's asset for $pinned is not the pinned one (SHA-256 $($Manifest['release.sha256']))."
    }
    Write-Host "  release.version  $ver"
    Write-Host "  release.tag      $($rel.tag_name)"
    Write-Host "  release.url      $($asset.browser_download_url)"
    Write-Host "  release.sha256   $expect$(if ($Sha256) { ' (from -Sha256)' } else { " (from GitHub's API)" })"
    Write-Host "  release.size     $($asset.size)"
    return [pscustomobject]@{
        Version = $ver; Url = [string]$asset.browser_download_url; Sha256 = $expect
        LicenseUrl = "https://raw.githubusercontent.com/optiscaler/OptiScaler/$($rel.tag_name)/LICENSE"; LicenseSha256 = $null
        SourceUrl = "https://github.com/optiscaler/OptiScaler/tree/$($rel.tag_name)"; Pinned = $false
    }
}

function Expand-Release([string] $Archive, [string] $Out, $Manifest) {
    New-Item -ItemType Directory -Path $Out | Out-Null
    $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
    if (Test-Path -LiteralPath $tar -PathType Leaf) {
        $r = Invoke-Native $tar @('-x', '-f', $Archive, '-C', $Out)
        if ($r.Code -eq 0 -and (Test-Path -LiteralPath (Join-Path $Out 'OptiScaler.dll'))) { return 'tar.exe' }
        Write-Host "tar.exe could not extract it ($($r.Output)); using 7-Zip's 7zr.exe."
        Remove-Item -LiteralPath $Out -Recurse -Force
        New-Item -ItemType Directory -Path $Out | Out-Null
    }
    if (-not $Manifest.ContainsKey('7zr.url')) { throw 'tar.exe cannot extract the release and the manifest names no 7zr.exe.' }
    $tools = Join-Path $cacheRoot 'tools'
    if (-not (Test-Path -LiteralPath $tools)) { New-Item -ItemType Directory -Path $tools | Out-Null }
    $sevenZr = Join-Path $tools ('7zr-' + $Manifest['7zr.sha256'].Substring(0, 12) + '.exe')
    if (-not (Test-Path -LiteralPath $sevenZr) -or (Get-Sha256 $sevenZr) -ne $Manifest['7zr.sha256']) {
        Save-Verified $Manifest['7zr.url'] $Manifest['7zr.sha256'] $sevenZr
    }
    $r = Invoke-Native $sevenZr @('x', '-y', '-bd', "-o$Out", $Archive)
    if ($r.Code -ne 0) { throw "7zr.exe could not extract $Archive`: $($r.Output)" }
    return '7zr.exe'
}

# The cached release (downloaded, checked, extracted): its files folder.
function Get-CachedRelease($Manifest) {
    $info = Get-ReleaseInfo $Manifest
    Initialize-Cache
    $dest = Join-Path $cacheRoot $info.Version
    $files = Join-Path $dest 'files'
    $sums = Join-Path $dest 'files.sha256'
    if (Test-Path -LiteralPath $sums -PathType Leaf) {
        $bad = Test-CachedFiles $dest
        if ($bad.Count -eq 0) {
            Write-Host "OptiScaler $($info.Version): cached in $dest"
            return $files
        }
        Write-Warning "The cached OptiScaler $($info.Version) changed ($($bad -join ', ')); downloading it again."
    }
    $work = Join-Path $cacheRoot ".partial-$PID"
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        $archive = Join-Path $work 'release.7z'
        Write-Host "Downloading $($info.Url)"
        Save-Verified $info.Url $info.Sha256 $archive
        Write-Host "SHA-256 $($info.Sha256): matches$(if ($info.Pinned) { ' the pinned hash' } else { '' })"
        $license = Join-Path $work 'LICENSE'
        if ($info.LicenseSha256) { Save-Verified $info.LicenseUrl $info.LicenseSha256 $license } else { Save-Url $info.LicenseUrl $license }
        $tool = Expand-Release $archive (Join-Path $work 'files') $Manifest
        Remove-Item -LiteralPath $archive -Force
        $root = Join-Path $work 'files'
        foreach ($need in 'OptiScaler.dll', 'OptiScaler.ini') {
            if (-not (Test-Path -LiteralPath (Join-Path $root $need) -PathType Leaf)) { throw "The release has no $need at its top level." }
        }
        $links = @(Get-ChildItem -LiteralPath $root -Recurse -Force | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint })
        if ($links.Count) { throw "The release contains links ($($links[0].FullName)); not using it." }
        $lines = @()
        foreach ($f in @(Get-ChildItem -LiteralPath $root -Recurse -File -Force)) {
            $lines += "$(Get-Sha256 $f.FullName)  $($f.FullName.Substring($root.Length).TrimStart('\'))"
        }
        [IO.File]::WriteAllLines((Join-Path $work 'files.sha256'), [string[]]$lines)
        [IO.File]::WriteAllLines((Join-Path $work 'release.txt'), [string[]]@(
            "OptiScaler $($info.Version), GPL-3.0 (LICENSE). Not part of Conduit; downloaded by Install-OptiScaler.ps1.",
            "url     $($info.Url)",
            "sha256  $($info.Sha256)",
            "source  $($info.SourceUrl)",
            "license $($info.LicenseUrl)",
            "fetched $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')) (extracted with $tool)"))
        if (Test-Path -LiteralPath $dest) { Remove-Item -LiteralPath $dest -Recurse -Force }
        Move-Item -LiteralPath $work -Destination $dest
    } finally {
        if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
    }
    Write-Host "OptiScaler $($info.Version): extracted into $dest (with its LICENSE)"
    return $files
}

# Files of a cached release that are missing or changed.
function Test-CachedFiles([string] $Dest) {
    $bad = @()
    foreach ($line in [IO.File]::ReadAllLines((Join-Path $Dest 'files.sha256'))) {
        if ($line -notmatch '^([0-9a-f]{64})  (.+)$') { $bad += "bad line"; continue }
        $p = Join-Path (Join-Path $Dest 'files') $Matches[2]
        if (-not (Test-Path -LiteralPath $p -PathType Leaf) -or (Get-Sha256 $p) -ne $Matches[1]) { $bad += $Matches[2] }
    }
    return , $bad
}

# Relative paths of a release's files: the checked list for the cache, the
# folder's contents for -Source.
function Get-ReleaseFiles([string] $Root) {
    $sums = Join-Path (Split-Path -Parent $Root) 'files.sha256'
    if ((Split-Path -Leaf $Root) -eq 'files' -and (Test-Path -LiteralPath $sums -PathType Leaf)) {
        return , @([IO.File]::ReadAllLines($sums) | ForEach-Object { ($_ -split '  ', 2)[1] })
    }
    return , @(Get-ChildItem -LiteralPath $Root -Recurse -File | ForEach-Object { $_.FullName.Substring($Root.Length).TrimStart('\') })
}

# conduit-optiscaler.txt: "# key value" header lines, then one copied file per line.
function Read-Marker([string] $Path) {
    $m = @{ Files = @(); Version = $null; Proxy = $null; Exe = $null }
    foreach ($line in [IO.File]::ReadAllLines($Path)) {
        if (-not $line) { continue }
        if ($line.StartsWith('#')) {
            if ($line -match '^# version (.+)$') { $m.Version = $Matches[1].Trim() }
            elseif ($line -match '^# proxy (.+)$') { $m.Proxy = $Matches[1].Trim() }
            elseif ($line -match '^# exe (.+)$') { $m.Exe = $Matches[1].Trim() }
            continue
        }
        # only names inside the game folder, never an absolute or parent path
        if ([IO.Path]::IsPathRooted($line) -or $line -match '^([A-Za-z]:|\\|/)' -or $line -match '(^|\\|/)\.\.(\\|/|$)') { continue }
        $m.Files += $line
    }
    return $m
}

function Read-IniLines([string] $Path) {
    $text = [IO.File]::ReadAllText($Path)
    $lines = New-Object System.Collections.Generic.List[string]
    if ($text.Length -gt 0) { foreach ($l in ($text -split "`r?`n")) { $lines.Add($l) } }
    $endsNl = $text.EndsWith("`n")
    if ($endsNl -and $lines.Count -gt 0) { $lines.RemoveAt($lines.Count - 1) }
    return [pscustomobject]@{ Lines = $lines; Crlf = $text.Contains("`r`n"); EndsNl = ($endsNl -or $text.Length -eq 0) }
}

# The same edit as upscaler.rs: rewrite Key= in its section, add a missing
# key at the start of its section, a missing section at the end.
function Set-IniValue($Ini, [string] $Section, [string] $Key, [string] $Value) {
    $lines = $Ini.Lines
    $inSection = $false; $header = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $t = $lines[$i].Trim()
        if ($t.StartsWith('[') -and $t.EndsWith(']')) {
            $inSection = [string]::Equals($t.Substring(1, $t.Length - 2).Trim(), $Section, [StringComparison]::OrdinalIgnoreCase)
            if ($inSection -and $header -lt 0) { $header = $i }
            continue
        }
        $s = $lines[$i].TrimStart()
        if ($inSection -and -not $s.StartsWith(';') -and -not $s.StartsWith('#') -and $s.Contains('=')) {
            if ([string]::Equals($s.Substring(0, $s.IndexOf('=')).Trim(), $Key, [StringComparison]::OrdinalIgnoreCase)) {
                $lines[$i] = "$Key=$Value"
                return
            }
        }
    }
    if ($header -ge 0) { $lines.Insert($header + 1, "$Key=$Value"); return }
    if ($lines.Count -gt 0 -and $lines[$lines.Count - 1].Trim()) { $lines.Add('') }
    $lines.Add("[$Section]")
    $lines.Add("$Key=$Value")
}

function Get-IniValue($Ini, [string] $Section, [string] $Key) {
    $inSection = $false
    foreach ($l in $Ini.Lines) {
        $t = $l.Trim()
        if ($t.StartsWith('[') -and $t.EndsWith(']')) {
            $inSection = [string]::Equals($t.Substring(1, $t.Length - 2).Trim(), $Section, [StringComparison]::OrdinalIgnoreCase)
            continue
        }
        if ($inSection -and -not $t.StartsWith(';') -and $t.Contains('=') -and
            [string]::Equals($t.Substring(0, $t.IndexOf('=')).Trim(), $Key, [StringComparison]::OrdinalIgnoreCase)) {
            return $t.Substring($t.IndexOf('=') + 1).Trim()
        }
    }
    return $null
}

function Save-Ini($Ini, [string] $Path) {
    $nl = if ($Ini.Crlf) { "`r`n" } else { "`n" }
    $text = [string]::Join($nl, $Ini.Lines)
    if ($Ini.EndsNl) { $text += $nl }
    [IO.File]::WriteAllText($Path, $text, (New-Object System.Text.UTF8Encoding $false))
}

# Install-Dlss.ps1's setup: the NGX runtime in C:\ProgramData\Helios\ngx,
# Dlss = 1, and an nvapi64.dll in System32 or next to the game.
function Test-DlssReady([string] $Dir) {
    $ngx = Join-Path $env:ProgramData 'Helios\ngx'
    if (-not (Test-Path -LiteralPath (Join-Path $ngx 'nvngx.dll')) -or -not (Test-Path -LiteralPath (Join-Path $ngx '_nvngx.dll'))) { return $false }
    $dlss = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Helios' -Name 'Dlss' -ErrorAction SilentlyContinue
    if ($null -eq $dlss -or $dlss.Dlss -ne 1) { return $false }
    return (Test-Path -LiteralPath (Join-Path $env:SystemRoot 'System32\nvapi64.dll')) -or (Test-Path -LiteralPath (Join-Path $Dir 'nvapi64.dll'))
}

# An OptiScaler that is in the folder but was not put there by this script
# (its proxy name), or $null.
function Find-ForeignOptiScaler([string] $Dir, $Ours) {
    foreach ($p in ($proxyOrder + 'd3d12')) {
        $f = Join-Path $Dir "$p.dll"
        if ((Test-Path -LiteralPath $f -PathType Leaf) -and -not $Ours.ContainsKey("$p.dll") -and
            [string]::Equals([string](Get-Item -LiteralPath $f).VersionInfo.OriginalFilename, 'OptiScaler.dll', [StringComparison]::OrdinalIgnoreCase)) {
            return $p
        }
    }
    return $null
}

function Resolve-UpscalerMode([string] $Requested, [bool] $DlssReady) {
    if ($Requested -and $Requested -ne 'auto') {
        if ($Requested -eq 'dlss' -and -not $DlssReady) {
            Write-Warning 'DLSS output needs the NGX runtime and nvapi64.dll from Install-Dlss.ps1 (not set up); setting it anyway.'
        }
        return $Requested
    }
    $global = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Helios' -Name 'Upscaler' -ErrorAction SilentlyContinue
    if ($null -ne $global -and @('dlss', 'fsr', 'xess') -contains ([string]$global.Upscaler).ToLowerInvariant()) {
        $g = ([string]$global.Upscaler).ToLowerInvariant()
        if ($g -ne 'dlss' -or $DlssReady) { return $g }
    }
    if ($DlssReady) { return 'dlss' }
    return 'fsr'
}

function Set-GameIni([string] $IniPath, [string] $Mode, [string] $Fg, [bool] $DlssNvapi) {
    $ini = Read-IniLines $IniPath
    if ($Mode) { foreach ($v in (Get-TableValues $modeTable $Mode)) { Set-IniValue $ini $v[0] $v[1] $v[2] } }
    if ($Fg) {
        $fgMode = if ($Fg -eq 'on') { 'optifg' } else { $Fg }
        foreach ($v in (Get-TableValues $frameGenTable $fgMode)) { Set-IniValue $ini $v[0] $v[1] $v[2] }
    }
    if ($DlssNvapi) { Set-IniValue $ini 'NvApi' 'OverrideNvapiDll' 'false' }
    Save-Ini $ini $IniPath
}

function Get-IniSummary([string] $IniPath) {
    $ini = Read-IniLines $IniPath
    $up = Get-IniValue $ini 'Upscalers' 'Dx12Upscaler'
    $fg = Get-IniValue $ini 'FrameGen' 'Enabled'
    $fgOut = Get-IniValue $ini 'FrameGen' 'FGOutput'
    $fgText = if ($fg -eq 'true' -and $fgOut -and $fgOut -ne 'nofg') { "on ($(Get-IniValue $ini 'FrameGen' 'FGInput') to $fgOut)" } else { 'off' }
    return "upscaler $up (DX12), frame generation $fgText"
}

# ---------------------------------------------------------------- main

function Invoke-Main {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this from an elevated PowerShell (it writes HKLM\SOFTWARE\Helios\OptiScaler and C:\ProgramData\Helios).'
    }
    $manifest = Read-Manifest $manifestPath

    if ($DownloadOnly) {
        $files = Get-CachedRelease $manifest
        Write-Host "OK: OptiScaler release in $(Split-Path -Parent $files)"
        return
    }

    $gamePath = (Resolve-Path -LiteralPath $Game).Path
    $isFile = Test-Path -LiteralPath $gamePath -PathType Leaf
    $dir = if ($isFile) { Split-Path -Parent $gamePath } else { $gamePath.TrimEnd('\') }
    $exeName = if ($isFile) { Split-Path -Leaf $gamePath } else { $null }
    if ($isFile -and $exeName -notlike '*.exe') { throw "$gamePath is not an .exe; give the game's executable or its folder." }
    $sysRoot = $env:SystemRoot.TrimEnd('\') + '\'
    if (($dir + '\').StartsWith($sysRoot, [StringComparison]::OrdinalIgnoreCase)) { throw "$dir is a Windows folder, not a game's." }
    $marker = Join-Path $dir $markerName
    $iniPath = Join-Path $dir 'OptiScaler.ini'
    $installed = Test-Path -LiteralPath $marker -PathType Leaf

    if ($Uninstall) {
        if (-not $installed) { Stop-With 4 "No $markerName in $dir`: nothing this script installed is there." }
        Wait-GameClosed $dir
        $old = Read-Marker $marker
        foreach ($rel in $old.Files) {
            $p = Join-Path $dir $rel
            if (Test-Path -LiteralPath $p -PathType Leaf) { Remove-Item -LiteralPath $p -Force }
        }
        # folders the release brought (D3D12_Optiscaler), when now empty
        foreach ($rel in ($old.Files | Where-Object { $_.Contains('\') } | ForEach-Object { Split-Path -Parent $_ } | Sort-Object -Unique -Descending)) {
            $d = Join-Path $dir $rel
            if ((Test-Path -LiteralPath $d -PathType Container) -and @(Get-ChildItem -LiteralPath $d -Force).Count -eq 0) { Remove-Item -LiteralPath $d }
        }
        Remove-Item -LiteralPath $marker -Force
        if (Test-Path -LiteralPath $regKey) { Remove-ItemProperty -LiteralPath $regKey -Name $dir -ErrorAction SilentlyContinue }
        Write-Host "OK: OptiScaler removed from $dir"
        return
    }

    if ($Configure) {
        if (-not $installed -or -not (Test-Path -LiteralPath $iniPath -PathType Leaf)) {
            Stop-With 4 "OptiScaler is not installed in $dir (no $markerName or OptiScaler.ini); install it first."
        }
        if (-not $Upscaler -and -not $FrameGen) { throw 'Pass -Upscaler and/or -FrameGen with -Configure.' }
        Wait-GameClosed $dir
        $mode = if ($Upscaler) { Resolve-UpscalerMode $Upscaler (Test-DlssReady $dir) } else { $null }
        Set-GameIni $iniPath $mode $FrameGen $false
        Write-Host "OK: OptiScaler in $dir`: $(Get-IniSummary $iniPath)"
        return
    }

    # --- install ---
    $ac = Find-AntiCheat $dir $exeName $manifest
    if ($ac) {
        if (-not $Force) {
            Stop-With 2 "Refused: this game has anti-cheat: $ac. OptiScaler injects a DLL into the game, which can get the account banned. Not installed (-Force overrides this)."
        }
        Write-Warning "This game has anti-cheat: $ac. OptiScaler injects a DLL into the game, which can get the account BANNED. Installing anyway (-Force); do not play online with it."
    }
    Wait-GameClosed $dir

    $old = if ($installed) { Read-Marker $marker } else { @{ Files = @(); Version = $null; Proxy = $null; Exe = $null } }
    $ours = @{}
    foreach ($f in $old.Files) { $ours[$f.ToLowerInvariant()] = $true }

    $foreign = Find-ForeignOptiScaler $dir $ours
    if ($foreign) {
        if (-not $Force) {
            throw "OptiScaler is already in $dir as $foreign.dll, not put there by this script. Remove it first, or pass -Force to replace it."
        }
    }

    if ($Source -and $Download) { throw 'Pass -Source or -Download, not both.' }
    if ($Source) {
        $src = (Resolve-Path -LiteralPath $Source).Path.TrimEnd('\')
        foreach ($need in 'OptiScaler.dll', 'OptiScaler.ini') {
            if (-not (Test-Path -LiteralPath (Join-Path $src $need) -PathType Leaf)) {
                throw "$src has no $need`: give the extracted OptiScaler release folder."
            }
        }
        $relVersion = 'from ' + $src
    } else {
        $src = Get-CachedRelease $manifest
        $relVersion = Split-Path -Leaf (Split-Path -Parent $src)
    }

    # The proxy name: -Proxy, else the earlier install's, else the first the
    # game does not ship itself.
    $proxyName = $Proxy
    if (-not $proxyName) { $proxyName = $old.Proxy }
    if (-not $proxyName -and $foreign) { $proxyName = $foreign }
    if (-not $proxyName) {
        foreach ($p in $proxyOrder) {
            $f = Join-Path $dir "$p.dll"
            if (-not (Test-Path -LiteralPath $f) -or $ours.ContainsKey("$p.dll")) { $proxyName = $p; break }
        }
        if (-not $proxyName) { throw "$dir has its own $($proxyOrder -join ', ') DLLs; pick one to replace with -Proxy and -Force." }
        if ($proxyName -ne 'dxgi') { Write-Host "The game has its own dxgi.dll; OptiScaler loads as $proxyName.dll." }
    }

    # What goes where: every file keeps its relative path, OptiScaler.dll
    # becomes the proxy DLL; setup scripts and text files stay in the cache.
    # The game's OptiScaler.ini and fakenvapi.ini stay unless -ResetIni.
    $plan = @()
    $keptIni = $false
    foreach ($rel in (Get-ReleaseFiles $src)) {
        if ($rel -match '\.(bat|cmd|sh|md|txt)$') { continue }
        $to = if ($rel -ieq 'OptiScaler.dll') { "$proxyName.dll" } else { $rel }
        if (@('OptiScaler.ini', 'fakenvapi.ini') -contains $rel -and (Test-Path -LiteralPath (Join-Path $dir $to)) -and -not $ResetIni) {
            if ($rel -ieq 'OptiScaler.ini') { $keptIni = $true }
            if ($ours.ContainsKey($to.ToLowerInvariant())) { $plan += [pscustomobject]@{ From = $null; To = $to } }
            continue
        }
        $plan += [pscustomobject]@{ From = (Join-Path $src $rel); To = $to }
    }
    $clash = @($plan | Where-Object { $_.From -and (Test-Path -LiteralPath (Join-Path $dir $_.To)) -and -not $ours.ContainsKey($_.To.ToLowerInvariant()) -and $_.To -ine 'OptiScaler.ini' -and $_.To -ine 'fakenvapi.ini' })
    if ($clash.Count -and -not $Force) {
        $names = ($clash | ForEach-Object { $_.To }) -join ', '
        throw "Already in $dir and not put there by this script: $names. Use another -Proxy, or -Force to replace them."
    }

    # Files of the earlier install the new release no longer has go.
    $newSet = @{}
    foreach ($f in $plan) { $newSet[$f.To.ToLowerInvariant()] = $true }
    foreach ($rel in $old.Files) {
        if (-not $newSet.ContainsKey($rel.ToLowerInvariant())) {
            $p = Join-Path $dir $rel
            if (Test-Path -LiteralPath $p -PathType Leaf) { Remove-Item -LiteralPath $p -Force }
        }
    }
    if ($ResetIni -and (Test-Path -LiteralPath $iniPath)) { Copy-Item -LiteralPath $iniPath -Destination "$iniPath.conduit-bak" -Force }

    $header = @(
        "# copied by Install-OptiScaler.ps1 (Conduit); -Uninstall removes these",
        "# version $relVersion",
        "# proxy $proxyName")
    if ($exeName) { $header += "# exe $exeName" }
    $copied = @()
    foreach ($f in $plan) {
        $dst = Join-Path $dir $f.To
        if ($f.From) {
            $parent = Split-Path -Parent $dst
            if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent | Out-Null }
            Copy-Item -LiteralPath $f.From -Destination $dst -Force
        }
        $copied += $f.To
        # recorded as it goes: a failed copy still leaves a marker -Uninstall can use
        [IO.File]::WriteAllLines($marker, [string[]]($header + $copied))
    }

    # OptiScaler.ini: on a new one the defaults for our stack, on a kept one
    # only what was asked for.
    $dlssReady = Test-DlssReady $dir
    if ($keptIni) {
        $mode = if ($Upscaler) { Resolve-UpscalerMode $Upscaler $dlssReady } else { $null }
        Set-GameIni $iniPath $mode $FrameGen $false
    } else {
        $mode = Resolve-UpscalerMode $Upscaler $dlssReady
        $fg = if ($FrameGen) { $FrameGen } else { 'off' }
        Set-GameIni $iniPath $mode $fg $dlssReady
    }

    if (-not (Test-Path -LiteralPath 'HKLM:\SOFTWARE\Helios')) { New-Item -Path 'HKLM:\SOFTWARE\Helios' | Out-Null }
    if (-not (Test-Path -LiteralPath $regKey)) { New-Item -Path $regKey | Out-Null }
    Set-ItemProperty -LiteralPath $regKey -Name $dir -Value $iniPath -Type String

    $what = if ($installed) { 'updated' } else { 'installed' }
    $iniNote = if ($keptIni) { ', kept the game''s OptiScaler.ini' } else { '' }
    Write-Host "OK: OptiScaler $relVersion $what in $dir as $proxyName.dll$iniNote; $(Get-IniSummary $iniPath)$(if ($dlssReady) { '; DLSS through the guest''s NGX setup' } else { '' })."
}

try {
    Invoke-Main
} catch {
    $code = 1
    if ($_.Exception.Data.Contains('ConduitExit')) { $code = [int]$_.Exception.Data['ConduitExit'] }
    [Console]::Error.WriteLine("Install-OptiScaler: $($_.Exception.Message)")
    exit $code
}
exit 0

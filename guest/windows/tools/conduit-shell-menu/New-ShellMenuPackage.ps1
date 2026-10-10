# Builds ConduitShellMenu.msix, the sparse package that registers
# conduit_shell_menu.dll ("Send to Conduit host" in Explorer's Windows 11
# context menu). The package holds only the manifest and its logos; the DLL
# and conduit-gpu-tray.exe stay in the install directory, the package's
# external location (Add-AppxPackage -Path <msix> -ExternalLocation <dir>).
#
#   New-ShellMenuPackage.ps1 -Publisher "CN=..." -Version 1.2.3.4 -OutFile ConduitShellMenu.msix [-MakeAppx <path>]
#
# -Publisher must equal the subject of the certificate that signs the msix
# (signtool sign /fd SHA256 ...), and Windows installs it only when that
# certificate is trusted (the Helios installer puts it in LocalMachine\Root
# and TrustedPublisher).
param(
    [Parameter(Mandatory)][string]$Publisher,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$OutFile,
    [string]$MakeAppx = ""
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Package versions are four numbers of 0..65535.
$parts = @($Version.Split("."))
while ($parts.Count -lt 4) { $parts += "0" }
if ($parts.Count -ne 4 -or @($parts | Where-Object { $_ -notmatch '^\d+$' -or [int64]$_ -gt 65535 }).Count) {
    throw "Version $Version is not a package version (a.b.c.d, each 0..65535)."
}
$Version = $parts -join "."

if (-not $MakeAppx) {
    $MakeAppx = Get-ChildItem -Path "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter makeappx.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -like "*\x64\*" } | Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    if (-not $MakeAppx) { throw "makeappx.exe (Windows SDK) was not found." }
}

$source = Join-Path $PSScriptRoot "package"
$layout = Join-Path ([IO.Path]::GetTempPath()) ("conduit-shell-menu-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $layout | Out-Null
try {
    Copy-Item -LiteralPath (Join-Path $source "Assets") -Destination $layout -Recurse
    $manifest = Get-Content -LiteralPath (Join-Path $source "AppxManifest.xml") -Raw
    $manifest = $manifest.Replace("@PUBLISHER@", [Security.SecurityElement]::Escape($Publisher)).Replace("@VERSION@", $Version)
    [IO.File]::WriteAllText((Join-Path $layout "AppxManifest.xml"), $manifest, (New-Object Text.UTF8Encoding $false))
    $outDir = Split-Path -Parent ([IO.Path]::GetFullPath($OutFile))
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    # /nv: the manifest names files (the DLL, the exe) that live in the
    # external location, not in the package.
    & $MakeAppx pack /o /nv /d $layout /p $OutFile
    if ($LASTEXITCODE -ne 0) { throw "makeappx failed (exit $LASTEXITCODE)." }
} finally {
    Remove-Item -LiteralPath $layout -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Host "Shell menu package: $OutFile ($Publisher, $Version)"

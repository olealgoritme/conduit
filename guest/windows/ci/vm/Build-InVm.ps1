# Builds the Helios driver package in a VM set up by Setup-BuildVm.ps1: the
# same Build-Driver.ps1 the `driver` job of .github/workflows/windows.yml
# runs, then a development test signature (tools/sign-helios-development.ps1)
# so the package installs on a VM in test-signing mode.
#
#   pwsh -ExecutionPolicy Bypass -File Build-InVm.ps1 [-Configuration Release|Debug] [-Root W:\]
#
# Source: <Root>\src\guest\windows (win-build.sh copies it there). Output:
# <Root>\out\<Configuration>.
param(
    [ValidateSet("Debug", "Release")][string]$Configuration = "Release",
    [string]$Root = "W:\",
    [switch]$Clean
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# An SSH session keeps the environment it started with; take the machine
# values Setup-BuildVm.ps1 set.
foreach ($name in @("Path", "RUSTUP_HOME", "CARGO_HOME", "RUST_TOOLCHAIN", "LIBCLANG_PATH", "VULKAN_SDK", "VK_SDK_PATH")) {
    $machine = [Environment]::GetEnvironmentVariable($name, "Machine")
    if ($name -eq "Path") {
        $user = [Environment]::GetEnvironmentVariable("Path", "User")
        $machine = "$machine;$user"
    }
    if ($machine) { Set-Item -Path "Env:$name" -Value $machine }
}

$Root = $Root.TrimEnd('\') + '\'
$repo = Join-Path $Root "src\guest\windows"
$out = Join-Path $Root "out\$Configuration"
if (-not (Test-Path (Join-Path $repo "ci\windows\Build-Driver.ps1"))) {
    throw "no source at $repo (copy guest/windows there, or run win-build.sh on the host)"
}
if (Test-Path $out) { Remove-Item -LiteralPath $out -Recurse -Force }

# Reuse the configured DXVK/vkd3d trees between local builds (see
# Build-Driver.ps1); -Clean starts from scratch as CI does.
if (-not $Clean) { $env:HELIOS_KEEP_ENGINE_BUILDS = "1" }
$started = Get-Date
& (Join-Path $repo "ci\windows\Build-Driver.ps1") -RepoRoot $repo -OutputDir $out `
    -Configuration $Configuration -BuildRoot (Join-Path $Root "helios-build")
# Sign as Assemble-Package.ps1 does in CI, with the development certificate
# (tools/sign-helios-development.ps1 makes it once and reuses it): the SYS
# and the four UMDs, then a fresh catalog over their final bytes, then the
# catalog. The certificate is copied into the package for install.cmd to
# trust; it goes to its own directory first, as the script needs.
. (Join-Path $repo "ci\windows\Initialize-HeliosBuild.ps1")
$kitBin = Split-Path -Parent (Find-WindowsKitTool "signtool.exe")
$env:PATH = "$kitBin;$env:PATH"
$sign = Join-Path $repo "tools\sign-helios-development.ps1"
$signing = Join-Path $Root "out\signing"
& $sign -OutputDirectory $signing -PackageDirectory $out
foreach ($file in @("helios_kmd_render.sys", "helios_umd.dll", "helios_umd12.dll", "helios_umd32.dll", "helios_umd12_32.dll")) {
    & $sign -OutputDirectory $signing -SignFile (Join-Path $out $file)
}
$catalog = Join-Path $out "helios_kmd_render.cat"
Remove-Item -LiteralPath $catalog -Force -ErrorAction SilentlyContinue
& (Find-WindowsKitTool "Inf2Cat.exe") "/driver:$out" "/os:10_X64" /uselocaltime
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $catalog -PathType Leaf)) { throw "Inf2Cat failed to produce the catalog." }
& $sign -OutputDirectory $signing -SignFile $catalog
Write-Host ("Built {0} in {1:N0} min: {2}" -f $Configuration, ((Get-Date) - $started).TotalMinutes, $out)

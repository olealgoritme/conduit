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
    [string]$Root = "W:\"
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

$started = Get-Date
& (Join-Path $repo "ci\windows\Build-Driver.ps1") -RepoRoot $repo -OutputDir $out `
    -Configuration $Configuration -BuildRoot (Join-Path $Root "helios-build")
& (Join-Path $repo "tools\sign-helios-development.ps1") -OutputDirectory $out -PackageDirectory $out
Write-Host ("Built {0} in {1:N0} min: {2}" -f $Configuration, ((Get-Date) - $started).TotalMinutes, $out)

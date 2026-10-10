# Builds the GitHub release asset conduit-windows-gpu-driver-<version>.zip from
# the output of Assemble-Package.ps1: one folder holding the test-signed driver
# package (INF, SYS, CAT, UMDs, NVK and Zink files), its symbols, licenses, the
# test certificate and install.ps1. Run it after Assemble-Package.ps1.
param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [Parameter(Mandatory)][string]$DriverArtifact,
    [Parameter(Mandatory)][string]$PackageOutputDir,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$staging = @(Get-ChildItem -LiteralPath (Join-Path $PackageOutputDir "staging") -Directory)
if ($staging.Count -ne 1) { throw "Expected one staged package under $PackageOutputDir\staging, found $($staging.Count)." }
$stagingRoot = $staging[0].FullName
$symbolsRoot = Join-Path $PackageOutputDir "$($staging[0].Name)-symbols"

$name = "conduit-windows-gpu-driver-$Version"
$folder = Join-Path $OutputDir $name
if (Test-Path -LiteralPath $folder) { Remove-Item -LiteralPath $folder -Recurse -Force }
New-Item -ItemType Directory -Force -Path $folder | Out-Null

# The signed driver package (the catalog was generated over these exact bytes).
Copy-Item -Path (Join-Path $stagingRoot "payload\driver\*") -Destination $folder -Recurse -Force
Copy-Item -LiteralPath (Join-Path $DriverArtifact "configuration.txt") -Destination $folder -Force
if (Test-Path -LiteralPath $symbolsRoot -PathType Container) {
    Get-ChildItem -LiteralPath $symbolsRoot -File -Filter "helios_*" | Copy-Item -Destination $folder -Force
}
$licenses = Join-Path $stagingRoot "licenses"
if (Test-Path -LiteralPath $licenses -PathType Container) {
    Copy-Item -LiteralPath $licenses -Destination (Join-Path $folder "licenses") -Recurse -Force
}
# install.ps1 imports the certificate under this name.
Copy-Item -LiteralPath (Join-Path $stagingRoot "certificate\helios-ci-test.cer") -Destination (Join-Path $folder "helios-dev-test.cer") -Force
Copy-Item -LiteralPath (Join-Path $RepoRoot "packaging\windows\install.ps1") -Destination $folder -Force

foreach ($required in @("helios_kmd_render.inf", "helios_kmd_render.sys", "helios_kmd_render.cat", "helios-dev-test.cer", "install.ps1")) {
    if (-not (Test-Path -LiteralPath (Join-Path $folder $required) -PathType Leaf)) { throw "Release zip is missing $required." }
}

$zip = Join-Path $OutputDir "$name.zip"
Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
Compress-Archive -LiteralPath $folder -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Release zip: $zip"
Write-Host "SHA256: $hash"

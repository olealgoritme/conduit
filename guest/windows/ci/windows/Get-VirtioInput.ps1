# Fetches the Red Hat signed vioinput driver (virtio-input, PCI 1AF4:1052)
# from a pinned virtio-win ISO and stages it for Assemble-Package.ps1:
#   <OutputDir>\vioinput.inf, vioinput.cat, vioinput.sys, viohidkmdf.sys
#   <OutputDir>\LICENSE-vioinput.txt
# The files are shipped unmodified: the catalog is Red Hat's WHQL signature,
# so they are not re-signed or re-cataloged. A SHA-256 mismatch is fatal.
param(
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$IsoUrl,
    [Parameter(Mandatory)][string]$IsoSha256,
    # Download cache; the ISO is ~830 MB and is deleted afterwards unless this
    # points at a folder that already holds it.
    [string]$WorkDir = $env:RUNNER_TEMP
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

if (-not $WorkDir) { $WorkDir = [IO.Path]::GetTempPath() }
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$iso = Join-Path $WorkDir ([IO.Path]::GetFileName(([Uri]$IsoUrl).AbsolutePath))
$downloaded = $false
if (-not (Test-Path -LiteralPath $iso -PathType Leaf)) {
    # curl.exe, not Invoke-WebRequest: fedorapeople.org answers browser-like
    # user agents (PowerShell's starts with "Mozilla/5.0") with an HTML bot
    # challenge page instead of the file.
    & curl.exe --fail --silent --show-error --location --retry 3 --max-time 1800 --output $iso $IsoUrl
    if ($LASTEXITCODE -ne 0) { throw "curl.exe failed ($LASTEXITCODE) downloading $IsoUrl" }
    $downloaded = $true
}
$hash = (Get-FileHash -LiteralPath $iso -Algorithm SHA256).Hash
if ($hash -ne $IsoSha256.ToUpperInvariant()) {
    $size = (Get-Item -LiteralPath $iso).Length
    $buf = New-Object byte[] 256
    $fs = [IO.File]::OpenRead($iso); try { $n = $fs.Read($buf, 0, $buf.Length) } finally { $fs.Dispose() }
    $head = [Text.Encoding]::ASCII.GetString($buf, 0, $n)
    $why = if ($head -match '<html|<!doctype') { " (the server sent an HTML page, not the ISO)" } else { "" }
    if ($downloaded) { Remove-Item -LiteralPath $iso -Force }
    throw "$iso ($size bytes) has SHA-256 $hash, expected $IsoSha256$why"
}

$names = @("vioinput.inf", "vioinput.cat", "vioinput.sys", "viohidkmdf.sys")
$driverDir = "vioinput\w11\amd64"
if (Test-Path -LiteralPath $OutputDir) { Remove-Item -LiteralPath $OutputDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

# Mount-DiskImage needs no extra tools on the Windows runners (administrator).
$image = Mount-DiskImage -ImagePath $iso -StorageType ISO -Access ReadOnly -PassThru
try {
    $letter = ($image | Get-Volume).DriveLetter
    if (-not $letter) { throw "The virtio-win ISO mounted without a drive letter." }
    $root = "${letter}:\"
    foreach ($name in $names) {
        $source = Join-Path (Join-Path $root $driverDir) $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "The virtio-win ISO has no $driverDir\$name." }
        Copy-Item -LiteralPath $source -Destination (Join-Path $OutputDir $name) -Force
    }
    $license = Join-Path $root "virtio-win_license.txt"
    if (-not (Test-Path -LiteralPath $license -PathType Leaf)) { throw "The virtio-win ISO has no virtio-win_license.txt." }
    Copy-Item -LiteralPath $license -Destination (Join-Path $OutputDir "LICENSE-vioinput.txt") -Force
} finally {
    Dismount-DiskImage -ImagePath $iso | Out-Null
}
if ($downloaded) { Remove-Item -LiteralPath $iso -Force -ErrorAction SilentlyContinue }

# Red Hat's catalog must still be the one that signs these files.
$signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $OutputDir "vioinput.cat")
if ($signature.Status -ne "Valid") {
    throw "vioinput.cat signature is $($signature.Status): $($signature.StatusMessage)"
}
Write-Host "vioinput.cat signed by $($signature.SignerCertificate.Subject)"
Get-ChildItem -LiteralPath $OutputDir -File | ForEach-Object {
    Write-Host ("{0}  {1}" -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash, $_.Name)
}

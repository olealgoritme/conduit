Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-HeliosAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this script from an elevated PowerShell window."
    }
}

function Get-HeliosSha256([Parameter(Mandatory)][string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "File not found: $Path" }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

# Copy a payload tree, skipping files whose destination already has identical
# content. Re-applying the same package cannot overwrite runtime DLLs that
# running processes have loaded (vulkan_virtio.dll is the recorded case), and it
# does not need to: same packageId means the bytes are identical. A genuine
# version change installs under a new installRoot, so nothing is skipped there.
function Copy-HeliosTreeIfChanged(
    [Parameter(Mandatory)][string]$Source,
    [Parameter(Mandatory)][string]$Destination,
    # A same-packageId re-apply has byte-identical CODE but the payload copies
    # are re-signed with a fresh per-build certificate, so the hashes differ and
    # the loaded runtime DLLs cannot be overwritten. In that case any existing
    # destination is already the right code, so skip it.
    [switch]$SkipExisting
) {
    if (-not (Test-Path -LiteralPath $Source -PathType Container)) { return }
    foreach ($file in (Get-ChildItem -LiteralPath $Source -File -Recurse)) {
        $relative = $file.FullName.Substring($Source.Length).TrimStart("\")
        $target = Join-Path $Destination $relative
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        if ((Test-Path -LiteralPath $target -PathType Leaf)) {
            if ($SkipExisting) { continue }
            if ((Get-HeliosSha256 $target) -eq (Get-HeliosSha256 $file.FullName)) { continue }
        }
        Copy-Item -LiteralPath $file.FullName -Destination $target -Force
    }
}

function Assert-HeliosPeArchitecture(
    [Parameter(Mandatory)][string]$Path,
    [Parameter(Mandatory)][ValidateSet("x64", "x86")][string]$Architecture
) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
            throw "Not a DOS/PE image: $Path"
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -gt $stream.Length - 26) { throw "Invalid PE header offset: $Path" }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw "Invalid PE signature: $Path" }
        $machine = $reader.ReadUInt16()
        $expected = if ($Architecture -eq "x86") { 0x14C } else { 0x8664 }
        if ($machine -ne $expected) {
            throw ("Expected {0} PE image, found machine 0x{1:X4}: {2}" -f $Architecture, $machine, $Path)
        }
    } finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

function Read-HeliosManifest([Parameter(Mandatory)][string]$BundleRoot) {
    $manifestPath = Join-Path $BundleRoot "manifest.json"
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "Package manifest is missing: $manifestPath"
    }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.schemaVersion -ne 1 -or $manifest.architecture -ne "x64") {
        throw "Unsupported Helios package schema or architecture."
    }
    return $manifest
}

function Test-HeliosManifest([Parameter(Mandatory)][string]$BundleRoot, [Parameter(Mandatory)]$Manifest) {
    $root = [IO.Path]::GetFullPath($BundleRoot).TrimEnd("\") + "\"
    foreach ($entry in @($Manifest.files)) {
        $relative = [string]$entry.path
        if ([IO.Path]::IsPathRooted($relative) -or $relative -match "(^|[\\/])\.\.([\\/]|$)") {
            throw "Unsafe path in package manifest: $relative"
        }
        $full = [IO.Path]::GetFullPath((Join-Path $BundleRoot ($relative -replace "/", "\")))
        if (-not $full.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Package manifest path escapes the bundle root: $relative"
        }
        $actual = Get-HeliosSha256 $full
        if ($actual -ne ([string]$entry.sha256).ToUpperInvariant()) {
            throw "Package hash mismatch for $relative. Expected $($entry.sha256), got $actual."
        }
        if ((Get-Item -LiteralPath $full).Length -ne [int64]$entry.size) {
            throw "Package size mismatch for $relative."
        }
    }
}

function Get-HeliosDeviceInstanceId {
    # Prefer a currently present device. WinBoat can change QEMU's device set
    # between bootstrap and accelerated boots, leaving a non-present PCI
    # instance in CIM that must not receive the WGL registration.
    $deviceIdPattern = "^PCI\\VEN_(?:1AF4&DEV_106|10DE&DEV_2B85)"
    $presentDevices = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -match $deviceIdPattern })
    $device = $presentDevices |
        Where-Object { $_.FriendlyName -like "*Helios*" } |
        Select-Object -First 1
    if (-not $device) {
        $device = $presentDevices | Select-Object -First 1
    }
    if ($device) { return [string]$device.InstanceId }

    $device = Get-CimInstance Win32_PnPEntity |
        Where-Object { $_.PNPDeviceID -match $deviceIdPattern -and $_.Name -like "*Helios*" } |
        Select-Object -First 1
    if (-not $device) {
        $device = Get-CimInstance Win32_PnPEntity |
            Where-Object { $_.PNPDeviceID -match $deviceIdPattern } |
            Select-Object -First 1
    }
    if (-not $device) { throw "The Helios GPU PCI device (1af4:106d/1069 or 10de:2b85) was not found." }
    return [string]$device.PNPDeviceID
}

function Get-HeliosDisplayClassKey([Parameter(Mandatory)][string]$InstanceId) {
    $root = "HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"

    # The Enum key's Driver value identifies the exact software key for this
    # PCI instance. MatchingDeviceId alone is ambiguous when Windows retains a
    # stale instance of the same adapter after its PCI address changes.
    $enumPath = "HKLM:\SYSTEM\CurrentControlSet\Enum\$InstanceId"
    $driverKeyName = ""
    if (Test-Path -LiteralPath $enumPath) {
        $enumProperties = Get-ItemProperty -LiteralPath $enumPath -Name "Driver" -ErrorAction SilentlyContinue
        if ($enumProperties -and $enumProperties.PSObject.Properties["Driver"]) {
            $driverKeyName = [string]$enumProperties.Driver
        }
    }
    if (-not $driverKeyName) {
        $driverProperty = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName DEVPKEY_Device_Driver -ErrorAction SilentlyContinue
        if ($driverProperty -and $driverProperty.PSObject.Properties["Data"]) {
            $driverKeyName = [string]$driverProperty.Data
        }
    }
    if ($driverKeyName -match "^\{4d36e968-e325-11ce-bfc1-08002be10318\}\\(\d{4})$") {
        $exactPath = Join-Path $root $Matches[1]
        if (Test-Path -LiteralPath $exactPath) { return $exactPath }
    }
    throw "Could not locate the display adapter software key for the present device instance $InstanceId."
}

function Get-HeliosActiveInf([Parameter(Mandatory)][string]$InstanceId) {
    $property = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName DEVPKEY_Device_DriverInfPath -ErrorAction SilentlyContinue
    if ($property -and $property.PSObject.Properties["Data"] -and $property.Data) {
        return [string]$property.Data
    }
    return ""
}

function Test-HeliosViogpudoDriver([Parameter(Mandatory)][string]$InfName) {
    $leafName = [IO.Path]::GetFileName($InfName)
    if ($leafName -ne $InfName -or $leafName -notmatch "^[A-Za-z0-9._-]+\.inf$") { return $false }
    $infPath = Join-Path $env:windir "INF\$leafName"
    if (-not (Test-Path -LiteralPath $infPath -PathType Leaf)) { return $false }
    # Match the driver's IDENTITY, never a bare mention of its name. The Helios
    # INF itself names viogpudo in a comment, so `(?i)\bviogpudo\b` matched the
    # Helios package and made the installer try to replace itself.
    return (Get-Content -LiteralPath $infPath -Raw) -match "(?i)\bviogpudo\.(inf|sys)\b|\bVioGpuDod\b"
}

function Get-HeliosRegistrySnapshot([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Name) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return [ordered]@{ exists = $false; kind = $null; value = $null }
    }
    $key = Get-Item -LiteralPath $Path
    if ($key.GetValueNames() -notcontains $Name) {
        return [ordered]@{ exists = $false; kind = $null; value = $null }
    }
    return [ordered]@{
        exists = $true
        kind = $key.GetValueKind($Name).ToString()
        value = $key.GetValue($Name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
}

function Ensure-HeliosRegistryKey([Parameter(Mandatory)][string]$Path) {
    # Registry New-Item -Force REPLACES existing keys and all their contents.
    # Build missing parents individually without Force; a racing creator must
    # fail safely rather than erase that creator's values or children.
    if (Test-Path -LiteralPath $Path) { return }
    $parent = Split-Path -Path $Path -Parent
    if (-not $parent -or $parent -eq $Path) { throw "No existing registry root for $Path." }
    Ensure-HeliosRegistryKey $parent
    New-Item -Path $Path -ErrorAction Stop | Out-Null
}

function Restore-HeliosRegistrySnapshot([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)]$Snapshot) {
    if ([bool]$Snapshot.exists) {
        Ensure-HeliosRegistryKey $Path
        New-ItemProperty -LiteralPath $Path -Name $Name -Value $Snapshot.value -PropertyType ([string]$Snapshot.kind) -Force | Out-Null
    } elseif (Test-Path -LiteralPath $Path) {
        Remove-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
    }
}

function Restore-HeliosDirect3DAfterRemoval(
    [Parameter(Mandatory)]$State,
    [string]$CurrentInf,
    [string]$CurrentClassKey,
    [string]$CurrentInfSha256
) {
    # Older bundles had no WoW64 UMDs and saved neither snapshot.
    if (-not $State.PSObject.Properties["installedDirect3D"]) { return }
    $restoredKey = ""
    if ($CurrentInf -and $CurrentClassKey -and $State.PSObject.Properties["previousDirect3D"] -and
        $State.previousDirect3D.activeInf -and $CurrentInf -ieq $State.previousDirect3D.activeInf -and
        $CurrentInf -ine $State.activeInf -and
        $State.previousDirect3D.infSha256 -and $CurrentInfSha256 -ieq $State.previousDirect3D.infSha256) {
        foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW", "InstalledDisplayDrivers")) {
            Restore-HeliosRegistrySnapshot $CurrentClassKey $name $State.previousDirect3D.values.$name
        }
        $restoredKey = $CurrentClassKey
    }
    $wowSnapshot = $State.installedDirect3D.PSObject.Properties["UserModeDriverNameWoW"]
    if (-not $wowSnapshot -or -not $wowSnapshot.Value.exists) { return }
    $ownedPaths = @($wowSnapshot.Value.value)
    # "<>" (the D3D9 slot: no D3D9 driver, D3D9On12) names no file, and Windows
    # PowerShell's path APIs reject its characters.
    $ownedNames = @($ownedPaths | Where-Object { [string]$_ -ne "<>" } |
        ForEach-Object { [IO.Path]::GetFileNameWithoutExtension([string]$_) } | Select-Object -Unique)
    $keys = @(@([string]$State.classKey, $CurrentClassKey) | Where-Object { $_ } | Select-Object -Unique)
    foreach ($keyPath in $keys) {
        if ($keyPath -ieq $restoredKey -or -not (Test-Path -LiteralPath $keyPath)) { continue }
        $wow = Get-HeliosRegistrySnapshot $keyPath "UserModeDriverNameWoW"
        if ($wow.exists) {
            $paths = @($wow.value)
            $owned = @($paths | Where-Object { $_ -in $ownedPaths })
            if ($owned.Count -gt 0 -and $owned.Count -eq $paths.Count) {
                Remove-ItemProperty -LiteralPath $keyPath -Name "UserModeDriverNameWoW" -ErrorAction Stop
            } elseif ($owned.Count -gt 0) {
                # A REG_MULTI_SZ is indexed by API; filtering individual slots
                # would change their meaning. Preserve a mixed/manual override.
                Write-Warning "Keeping mixed WoW64 registration on ${keyPath}; only some slots reference the removed package."
            }
        }
        # Do not delete the inventory entry of a newly selected driver that
        # uses the same filename in a different DriverStore directory.
        $registeredNames = @(
            foreach ($name in @("UserModeDriverName", "UserModeDriverNameWoW")) {
                $snapshot = Get-HeliosRegistrySnapshot $keyPath $name
                if ($snapshot.exists) {
                    @($snapshot.value) | Where-Object { [string]$_ -ne "<>" } |
                        ForEach-Object { [IO.Path]::GetFileNameWithoutExtension([string]$_) }
                }
            }
        )
        $inventory = Get-HeliosRegistrySnapshot $keyPath "InstalledDisplayDrivers"
        if ($inventory.exists) {
            $remaining = @($inventory.value | Where-Object { $_ -notin $ownedNames -or $_ -in $registeredNames })
            if ($remaining.Count -ne @($inventory.value).Count) {
                if ($remaining.Count -eq 0) {
                    Remove-ItemProperty -LiteralPath $keyPath -Name "InstalledDisplayDrivers" -ErrorAction Stop
                } else {
                    New-ItemProperty -LiteralPath $keyPath -Name "InstalledDisplayDrivers" -Value $remaining -PropertyType MultiString -Force | Out-Null
                }
            }
        }
    }
}

function Invoke-HeliosNative(
    [Parameter(Mandatory)][string]$FilePath,
    [Parameter(Mandatory)][string[]]$Arguments,
    [int[]]$SuccessExitCodes = @(0),
    [switch]$WaitForProcess
) {
    if ($WaitForProcess) {
        # Windows PowerShell can return immediately for GUI-subsystem programs
        # without defining $LASTEXITCODE. Start-Process supplies a reliable exit
        # code and waits for bootstrapper child activity to complete.
        $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -NoNewWindow -Wait -PassThru
        $exitCode = $process.ExitCode
    } else {
        $output = & $FilePath @Arguments 2>&1
        $exitCode = $LASTEXITCODE
        if ($output) { $output | ForEach-Object { Write-Host $_ } }
    }
    if ($SuccessExitCodes -notcontains $exitCode) {
        throw "$FilePath $($Arguments -join ' ') failed with exit code $exitCode."
    }
}

function Test-HeliosTestSigningEnabled {
    if (-not ("Helios.Package.CodeIntegrity" -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Helios.Package {
    [StructLayout(LayoutKind.Sequential)]
    public struct CodeIntegrityInformation {
        public UInt32 Length;
        public UInt32 Options;
    }
    public static class CodeIntegrity {
        [DllImport("ntdll.dll")]
        private static extern UInt32 NtQuerySystemInformation(
            UInt32 infoClass,
            ref CodeIntegrityInformation info,
            UInt32 infoLength,
            IntPtr returnLength);

        public static bool TestSigningEnabled() {
            var info = new CodeIntegrityInformation();
            info.Length = (UInt32)Marshal.SizeOf(info);
            UInt32 status = NtQuerySystemInformation(103, ref info, info.Length, IntPtr.Zero);
            if (status != 0) throw new InvalidOperationException("NtQuerySystemInformation failed: 0x" + status.ToString("X8"));
            return (info.Options & 0x2U) != 0;
        }
    }
}
'@
    }
    return [Helios.Package.CodeIntegrity]::TestSigningEnabled()
}

function Test-HeliosSecureBootEnabled {
    try { return [bool](Confirm-SecureBootUEFI -ErrorAction Stop) } catch { return $false }
}

function Write-HeliosJson(
    [Parameter(Mandatory)]$Value,
    [Parameter(Mandatory)][string]$Path,
    [ValidateSet("UTF8", "ASCII")][string]$Encoding = "UTF8"
) {
    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $temporary = "$Path.tmp"
    $Value | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $temporary -Encoding $Encoding
    Move-Item -LiteralPath $temporary -Destination $Path -Force
}

# Machine-readable progress for the GUI installer. Write-Host (not Write-Output)
# keeps it out of the pipeline; the GUI's stdout reader consumes any line that
# begins with this marker and never shows it raw. Plain `powershell -File` users
# just see the marker as ordinary console text, which is harmless.
function Write-HeliosProgress(
    [Parameter(Mandatory)][int]$Percent,
    [Parameter(Mandatory)][string]$Message
) {
    if ($Percent -lt 0) { $Percent = 0 }
    if ($Percent -gt 100) { $Percent = 100 }
    Write-Host "HELIOS-PROGRESS $Percent $Message"
}

# Old bundles predate explicit publisher metadata. Keep their known identity for
# upgrade/uninstall verification; new bundles carry the publisher from the source.
function Get-HeliosPackagePublisher($Package) {
    if ($Package.PSObject.Properties["publisher"] -and
        -not [string]::IsNullOrWhiteSpace([string]$Package.publisher)) {
        return [string]$Package.publisher
    }
    return "Helios Project"
}

# Deletes a file, or when it is in use (a DLL Explorer has loaded, an exe
# still running) has Windows delete it at the next restart. True when it is
# gone now.
function Remove-HeliosFileOrScheduleAtReboot([Parameter(Mandatory)][string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $true }
    try {
        Remove-Item -LiteralPath $Path -Force -Recurse -ErrorAction Stop
        return $true
    } catch {
        if (-not ("HeliosNative.MoveFile" -as [type])) {
            Add-Type -Namespace HeliosNative -Name MoveFile -MemberDefinition @"
[DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern bool MoveFileExW(string existing, string replacement, int flags);
"@
        }
        # MOVEFILE_DELAY_UNTIL_REBOOT; a folder is removed then too once empty.
        $items = @(Get-ChildItem -LiteralPath $Path -Recurse -Force -File -ErrorAction SilentlyContinue |
            ForEach-Object { $_.FullName })
        $items += @(Get-ChildItem -LiteralPath $Path -Recurse -Force -Directory -ErrorAction SilentlyContinue |
            Sort-Object { $_.FullName.Length } -Descending | ForEach-Object { $_.FullName })
        $items += $Path
        foreach ($item in $items) {
            if (Test-Path -LiteralPath $item) {
                # [NullString]: a plain $null would arrive as "" (not NULL).
                if (-not [HeliosNative.MoveFile]::MoveFileExW($item, [NullString]::Value, 4)) {
                    Write-Warning ("Could not schedule {0} for removal (error {1})." -f $item, [Runtime.InteropServices.Marshal]::GetLastWin32Error())
                }
            }
        }
        Write-Warning "$Path is in use; Windows removes it at the next restart."
        return $false
    }
}

# C:\ProgramData\Conduit holds the logs the elevated tray and the SYSTEM mount
# task write: administrators and SYSTEM may change it, users may read it.
# Mount-ConduitShares.ps1 applies the same rules at every start.
function Protect-ConduitDataDirectory([string]$Dir = (Join-Path $env:ProgramData "Conduit")) {
    $dir = $Dir
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $item = Get-Item -LiteralPath $dir -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "$dir is a link; remove it and run the installer again."
    }
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetOwner([Security.Principal.SecurityIdentifier]"S-1-5-32-544")
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = [Security.AccessControl.InheritanceFlags]"ContainerInherit, ObjectInherit"
    foreach ($rule in @(
        @("S-1-5-18", "FullControl"),
        @("S-1-5-32-544", "FullControl"),
        @("S-1-5-32-545", "ReadAndExecute")
    )) {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            [Security.Principal.SecurityIdentifier]$rule[0], $rule[1], $inherit, "None", "Allow")))
    }
    Set-Acl -LiteralPath $dir -AclObject $acl
    # What is already inside takes the folder's rules (links are not followed).
    & (Join-Path $env:SystemRoot "System32\icacls.exe") (Join-Path $dir "*") /reset /T /L /C /Q | Out-Null
}

function Get-HeliosNvidiaRequiredFiles {
    return @(
        "nvldumdx.dll", "nvldumd.dll", "nvoglv64.dll", "nvoglv32.dll",
        "nvcuda64.dll", "nvcuda32.dll", "nvdxgdmal64.dll", "nvdxgdmal32.dll",
        "nvapi64.dll", "nvapi64_impl.dll", "nvapi.dll", "nvapi_impl.dll", "nvcuda_loader64.dll",
        "nv-vk64.json", "nv-vk32.json", "nvlddmkm.sys"
    )
}

function Resolve-HeliosNvidiaPackage([string]$Path) {
    $required = Get-HeliosNvidiaRequiredFiles
    $candidates = if ($Path) {
        @(Get-Item -LiteralPath $Path -ErrorAction Stop)
    } else {
        $root = Join-Path $env:windir "System32\DriverStore\FileRepository"
        @(Get-ChildItem -LiteralPath $root -Directory -Filter "nv_disp*.inf_amd64_*" -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending)
    }
    foreach ($candidate in $candidates) {
        $missing = @($required | Where-Object { -not (Test-Path -LiteralPath (Join-Path $candidate.FullName $_) -PathType Leaf) })
        if ($missing.Count -eq 0) {
            $version = (Get-Item -LiteralPath (Join-Path $candidate.FullName "nvlddmkm.sys")).VersionInfo.FileVersion
            if ($version -eq "32.0.16.1088") { return $candidate.FullName }
        }
    }
    if ($Path) { throw "NVIDIA 610.88 package files are incomplete at $Path." }
    throw "NVIDIA 610.88 (32.0.16.1088) was not found in DriverStore. Pass -NvidiaPackage with the extracted Display.Driver directory."
}

function Get-HeliosDriverStorePath {
    $instanceId = Get-HeliosDeviceInstanceId
    $classKey = Get-HeliosDisplayClassKey $instanceId
    $registered = @((Get-Item -LiteralPath $classKey).GetValue("UserModeDriverName", $null))
    $path = @($registered | Where-Object { $_ -and [IO.Path]::IsPathRooted($_) } | Select-Object -First 1)
    # The NVIDIA personality registers nvldumdx.dll before it is staged, so
    # only the registered file's DriverStore directory has to exist here.
    if ($path.Count -eq 1) {
        $directory = Split-Path -Parent $path[0]
        if ((Test-Path -LiteralPath $directory -PathType Container) -and
            $directory -match '\\DriverStore\\FileRepository\\') {
            return $directory
        }
    }
    throw "The active Helios UMD registration does not name a file in the active DriverStore."
}

# Copy the device-key (HKR) values NVIDIA's own INF writes for the RTX 5090
# (PCI\VEN_10DE&DEV_2B85) into this adapter's software key: DXCoreAttributes,
# DriverSupportModules, UserModeDriverGUID, Vulkan/OpenCL names and the rest
# NVIDIA's UMDs read through D3DKMT QUERYREGISTRY. %13% becomes the folder the
# UMDs were staged in. Only HKR lines are applied; nothing else in the INF.
function Import-HeliosNvidiaDeviceRegistry([string]$InfPath, [string]$ClassKey, [string]$DriverStore) {
    $raw = Get-Content -LiteralPath $InfPath
    $lines = New-Object System.Collections.Generic.List[string]
    $pending = ""
    foreach ($line in $raw) {
        $text = $line
        $inQuote = $false
        for ($i = 0; $i -lt $text.Length; $i++) {
            if ($text[$i] -eq '"') { $inQuote = -not $inQuote }
            elseif ($text[$i] -eq ';' -and -not $inQuote) { $text = $text.Substring(0, $i); break }
        }
        $text = $text.Trim()
        if ($text.EndsWith("\")) { $pending += $text.Substring(0, $text.Length - 1); continue }
        $lines.Add($pending + $text); $pending = ""
    }
    $sections = @{}
    $current = $null
    foreach ($line in $lines) {
        if ($line -match '^\[(.+)\]$') { $current = $Matches[1].ToLowerInvariant(); $sections[$current] = New-Object System.Collections.Generic.List[string]; continue }
        if ($current -and $line) { $sections[$current].Add($line) }
    }
    $strings = @{}
    foreach ($line in $sections["strings"]) {
        if ($line -match '^([^=]+?)\s*=\s*(.*)$') { $strings[$Matches[1].Trim().ToLowerInvariant()] = $Matches[2].Trim().Trim('"') }
    }
    function Split-Fields([string]$text) {
        $fields = @(); $cur = ""; $q = $false
        foreach ($ch in $text.ToCharArray()) {
            if ($ch -eq '"') { $q = -not $q; $cur += $ch }
            elseif ($ch -eq ',' -and -not $q) { $fields += $cur.Trim(); $cur = "" }
            else { $cur += $ch }
        }
        $fields += $cur.Trim()
        return ,$fields
    }
    function Expand-Field([string]$text) {
        $value = $text.Trim()
        if ($value.StartsWith('"') -and $value.EndsWith('"') -and $value.Length -ge 2) { $value = $value.Substring(1, $value.Length - 2) }
        return [regex]::Replace($value, '%([^%]+)%', {
            param($m)
            $name = $m.Groups[1].Value
            if ($name -eq "13") { return $DriverStore }
            $key = $name.ToLowerInvariant()
            if ($strings.ContainsKey($key)) { return $strings[$key] }
            return $m.Value
        })
    }
    $install = $null
    foreach ($name in $sections.Keys) {
        foreach ($line in $sections[$name]) {
            if ($line -match '=\s*([^,\s]+)\s*,\s*PCI\\VEN_10DE&DEV_2B85\s*$') { $install = $Matches[1].ToLowerInvariant(); break }
        }
        if ($install) { break }
    }
    if (-not $install) { throw "No PCI\VEN_10DE&DEV_2B85 install section in $InfPath." }
    $addRegs = @()
    foreach ($line in $sections[$install]) {
        if ($line -match '^AddReg\s*=\s*(.+)$') { $addRegs += ($Matches[1] -split ',') | ForEach-Object { $_.Trim().ToLowerInvariant() } }
    }
    $count = 0
    foreach ($section in $addRegs) {
        foreach ($line in $sections[$section]) {
            $f = Split-Fields $line
            if ($f.Count -lt 4 -or $f[0] -ne "HKR") { continue }
            $subkey = Expand-Field $f[1]
            $valueName = Expand-Field $f[2]
            $flagText = Expand-Field $f[3]
            $flags = if ($flagText -match '^0x') { [Convert]::ToUInt32($flagText.Substring(2), 16) } elseif ($flagText) { [uint32]$flagText } else { 0 }
            if ($flags -band 0x14) { continue }   # DELVAL / KEYONLY: nothing to write
            $data = @($f | Select-Object -Skip 4 | ForEach-Object { Expand-Field $_ })
            $key = if ($subkey) { Join-Path $ClassKey $subkey } else { $ClassKey }
            if (-not (Test-Path -LiteralPath $key)) { New-Item -Path $key -Force | Out-Null }
            $type = $flags -band 0xFFFF0001
            $append = ($flags -band 0x8) -ne 0
            switch ($type) {
                0x00010001 {
                    $text = if ($data.Count) { $data[0] } else { "0" }
                    $value = if ($text -match '^0x') { [Convert]::ToUInt32($text.Substring(2), 16) } else { [uint32]$text }
                    New-ItemProperty -LiteralPath $key -Name $valueName -Value $value -PropertyType DWord -Force | Out-Null
                }
                0x00010000 {
                    $values = @($data | Where-Object { $_ -ne "" })
                    if ($append) {
                        $old = @((Get-Item -LiteralPath $key).GetValue($valueName, @()))
                        $values = @($old + ($values | Where-Object { $old -notcontains $_ }))
                    }
                    New-ItemProperty -LiteralPath $key -Name $valueName -Value ([string[]]$values) -PropertyType MultiString -Force | Out-Null
                }
                0x00020000 { New-ItemProperty -LiteralPath $key -Name $valueName -Value ($data -join ",") -PropertyType ExpandString -Force | Out-Null }
                0x00000001 {
                    $bytes = [byte[]]@($data | Where-Object { $_ -ne "" } | ForEach-Object { [Convert]::ToByte($_, 16) })
                    New-ItemProperty -LiteralPath $key -Name $valueName -Value $bytes -PropertyType Binary -Force | Out-Null
                }
                0x00000000 {
                    if ($append) {
                        $old = @((Get-Item -LiteralPath $key).GetValue($valueName, @()))
                        $values = @($old + ($data | Where-Object { $_ -ne "" -and $old -notcontains $_ }))
                        New-ItemProperty -LiteralPath $key -Name $valueName -Value ([string[]]$values) -PropertyType MultiString -Force | Out-Null
                    } else {
                        New-ItemProperty -LiteralPath $key -Name $valueName -Value ($data -join ",") -PropertyType String -Force | Out-Null
                    }
                }
                default { continue }
            }
            $count++
        }
    }
    return $count
}

function Install-HeliosNvidiaUmd([string]$Package) {
    $source = Resolve-HeliosNvidiaPackage $Package
    $destination = Get-HeliosDriverStorePath
    Get-ChildItem -LiteralPath $source -File -Recurse | ForEach-Object {
        $relative = $_.FullName.Substring($source.Length).TrimStart("\")
        $target = Join-Path $destination $relative
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        Copy-Item -LiteralPath $_.FullName -Destination $target -Force
    }
    foreach ($file in Get-HeliosNvidiaRequiredFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $destination $file) -PathType Leaf)) {
            throw "NVIDIA UMD staging did not produce $file in $destination."
        }
    }
    Assert-HeliosPeArchitecture (Join-Path $destination "nvldumdx.dll") "x64"
    Assert-HeliosPeArchitecture (Join-Path $destination "nvldumd.dll") "x86"
    Assert-HeliosPeArchitecture (Join-Path $destination "nvoglv64.dll") "x64"
    Assert-HeliosPeArchitecture (Join-Path $destination "nvoglv32.dll") "x86"
    Assert-HeliosPeArchitecture (Join-Path $destination "nvcuda64.dll") "x64"
    Assert-HeliosPeArchitecture (Join-Path $destination "nvcuda32.dll") "x86"
    # NVIDIA's CUDA loader accepts an adapter only when its kernel image (or,
    # as a fallback, Services\nvlddmkm\ImagePath) is named nvlddmkm.sys, and
    # loads nvcuda64.dll from that folder. Point the INF's demand-start alias
    # service (never started) at the staged copy beside the UMDs.
    $repository = Split-Path -Leaf $destination
    $imagePath = "\SystemRoot\System32\DriverStore\FileRepository\$repository\nvlddmkm.sys"
    Set-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\nvlddmkm" -Name ImagePath -Value $imagePath -Type ExpandString
    # The CUDA loader opens System32\nvcuda.dll; NVIDIA's own INF installs
    # nvcuda_loader64.dll there under that name.
    Copy-Item -LiteralPath (Join-Path $destination "nvcuda_loader64.dll") -Destination (Join-Path $env:SystemRoot "System32\nvcuda.dll") -Force
    # NVIDIA's UMDs and CUDA load NVAPI from System32/SysWOW64. The Helios
    # package puts DXVK-NVAPI there for DLSS on NVK; keep it as *.dxvk.dll and
    # install NVIDIA's own for this personality.
    foreach ($pair in @(@("System32", "nvapi64.dll"), @("SysWOW64", "nvapi.dll"))) {
        $target = Join-Path (Join-Path $env:SystemRoot $pair[0]) $pair[1]
        if ((Test-Path -LiteralPath $target) -and
            -not ((Get-Item -LiteralPath $target).VersionInfo.CompanyName -match "NVIDIA")) {
            Copy-Item -LiteralPath $target -Destination ($target -replace '\.dll$', '.dxvk.dll') -Force
        }
        Copy-Item -LiteralPath (Join-Path $destination $pair[1]) -Destination $target -Force
    }
    $inf = Get-ChildItem -LiteralPath $source -Filter "nv_disp*.inf" -File -ErrorAction SilentlyContinue | Select-Object -First 1
    $imported = 0
    if ($inf) {
        $classKey = Get-HeliosDisplayClassKey (Get-HeliosDeviceInstanceId)
        $imported = Import-HeliosNvidiaDeviceRegistry $inf.FullName $classKey $destination
    }
    return [pscustomobject]@{ Source = $source; Destination = $destination; RegistryValues = $imported }
}

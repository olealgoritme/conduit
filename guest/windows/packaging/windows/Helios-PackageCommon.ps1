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

# ---- NVIDIA's own user-mode drivers (opt-in, RTX personality only) ----------
#
# Helios never ships or downloads NVIDIA files. On the RTX personality
# (PCI\VEN_10DE&DEV_2B85) Enable-HeliosNvidiaUmd takes NVIDIA's D3D, Vulkan,
# OpenGL, CUDA and NVAPI user-mode drivers from an NVIDIA driver package the
# user already has (an extracted driver download, or NVIDIA's own copy in the
# DriverStore), checks its version, copies the files NVIDIA's INF installs into
# its driver-store folder (plus the System32 loaders the UMDs need) into the
# Helios driver-store folder, and writes the registry values from NVIDIA's INF.
# Disable-HeliosNvidiaUmd undoes every one of those changes from the journal in
# nvidia-umd.json and registers the Helios UMDs again.
#
# Version contract (registry, HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render):
#   Parameters\NvUmdVersion  REG_SZ, written by the Helios INF: the NVIDIA
#                            Windows driver version the KMD speaks (e.g. 610.88).
#   NvUmdVersion             REG_SZ, optional: overrides the INF's value.
#   NvHostRmVersion          REG_SZ, written by the KMD: the host RM's version
#                            string (e.g. 610.57.04), once per transport start.
# The package's version must equal NvUmdVersion, and its branch (the number
# before the first dot) must equal the host RM's. NVIDIA numbers its Linux and
# Windows builds of one branch differently, so the branch is what the two sides
# have in common.

function Get-HeliosServiceKey { return "HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render" }

function Get-HeliosNvidiaStatePath { return (Join-Path $env:ProgramData "Helios\nvidia-umd.json") }

function Test-HeliosNvidiaPersonality([string]$InstanceId) {
    return ($InstanceId -match "^PCI\\VEN_10DE&DEV_2B85")
}

# Files the UMDs cannot work without; their absence fails the staging.
function Get-HeliosNvidiaRequiredFiles {
    return @(
        "nvldumdx.dll", "nvldumd.dll", "nvoglv64.dll", "nvoglv32.dll",
        "nvcuda64.dll", "nvcuda32.dll", "nvdxgdmal64.dll", "nvdxgdmal32.dll",
        "nvapi64.dll", "nvapi64_impl.dll", "nvapi.dll", "nvapi_impl.dll", "nvcuda_loader64.dll",
        "nv-vk64.json", "nv-vk32.json", "nvlddmkm.sys"
    )
}

# System32/SysWOW64 files NVIDIA's INF installs that the UMDs and CUDA load
# from there: target directory, target name, source name in the package.
function Get-HeliosNvidiaSystemFiles {
    return @(
        @("System32", "nvcuda.dll", "nvcuda_loader64.dll"),
        @("System32", "nvapi64.dll", "nvapi64.dll"),
        @("SysWOW64", "nvapi.dll", "nvapi.dll")
    )
}

# NVIDIA's short version from either form: a driver file version
# (32.0.16.1088 -> 610.88: the last digit of the third field and the four of
# the fourth) or an already short one (610.88, 610.57.04).
function ConvertTo-HeliosNvidiaVersion([string]$Version) {
    $text = ([string]$Version).Trim()
    if ($text -match '^\d+\.\d+\.(\d+)\.(\d+)$') {
        $digits = ([int]$Matches[1] % 10).ToString() + ([int]$Matches[2]).ToString("0000")
        return "{0}.{1}" -f $digits.Substring(0, 3), $digits.Substring(3)
    }
    if ($text -match '^\d+\.\d+(\.\d+)?$') { return $text }
    throw "'$Version' is not an NVIDIA driver version."
}

function Get-HeliosNvidiaBranch([string]$Version) {
    return (ConvertTo-HeliosNvidiaVersion $Version).Split('.')[0]
}

function Get-HeliosRegistryString([string]$Path, [string]$Name) {
    if (-not (Test-Path -LiteralPath $Path)) { return "" }
    $value = (Get-Item -LiteralPath $Path).GetValue($Name, $null)
    if ($null -eq $value) { return "" }
    return ([string]$value).Trim()
}

# The NVIDIA driver version the KMD speaks: NvUmdVersion in the service key,
# else in its Parameters subkey (the INF's).
function Get-HeliosNvidiaExpectedVersion {
    $service = Get-HeliosServiceKey
    foreach ($path in @($service, (Join-Path $service "Parameters"))) {
        $value = Get-HeliosRegistryString $path "NvUmdVersion"
        if ($value) { return (ConvertTo-HeliosNvidiaVersion $value) }
    }
    throw "NvUmdVersion is not set in $service\Parameters; install the Helios driver package for the RTX personality first."
}

# The host RM's version the KMD published, waiting up to $WaitSeconds for a KMD
# that has just started.
function Get-HeliosHostRmVersion([int]$WaitSeconds = 0) {
    $deadline = [DateTime]::UtcNow.AddSeconds($WaitSeconds)
    do {
        $value = Get-HeliosRegistryString (Get-HeliosServiceKey) "NvHostRmVersion"
        if ($value) { return $value }
        if ([DateTime]::UtcNow -ge $deadline) { break }
        Start-Sleep -Seconds 1
    } while ($true)
    return ""
}

# Throws unless $PackageVersion is the expected NVIDIA version and its branch is
# the host RM's.
function Assert-HeliosNvidiaVersion([string]$PackageVersion, [string]$Expected, [string]$HostRm) {
    $package = ConvertTo-HeliosNvidiaVersion $PackageVersion
    if ($package -ne (ConvertTo-HeliosNvidiaVersion $Expected)) {
        throw "The NVIDIA driver package is $package; this Helios driver needs NVIDIA $Expected (NvUmdVersion)."
    }
    if (-not $HostRm) {
        throw "The host's NVIDIA driver version is unknown: the Helios KMD has not published NvHostRmVersion. Start the adapter on the host's RM and run this again."
    }
    if ((Get-HeliosNvidiaBranch $package) -ne (Get-HeliosNvidiaBranch $HostRm)) {
        throw "The NVIDIA driver package is $package, but the host runs NVIDIA $HostRm (another branch)."
    }
}

# ---- INF reading ------------------------------------------------------------

# Sections (lower-case name -> lines, comments and continuations resolved), the
# section order, and [Strings].
function Read-HeliosInf([Parameter(Mandatory)][string]$Path) {
    $lines = New-Object System.Collections.Generic.List[string]
    $pending = ""
    foreach ($line in (Get-Content -LiteralPath $Path)) {
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
    $order = New-Object System.Collections.Generic.List[string]
    $current = $null
    foreach ($line in $lines) {
        if ($line -match '^\[(.+)\]$') {
            $current = $Matches[1].Trim().ToLowerInvariant()
            if (-not $sections.ContainsKey($current)) {
                $sections[$current] = New-Object System.Collections.Generic.List[string]
                $order.Add($current)
            }
            continue
        }
        if ($current -and $line) { $sections[$current].Add($line) }
    }
    $strings = @{}
    if ($sections.ContainsKey("strings")) {
        foreach ($line in $sections["strings"]) {
            if ($line -match '^([^=]+?)\s*=\s*(.*)$') { $strings[$Matches[1].Trim().ToLowerInvariant()] = $Matches[2].Trim().Trim('"') }
        }
    }
    return [pscustomobject]@{ Path = $Path; Sections = $sections; Order = $order; Strings = $strings }
}

function Get-HeliosInfSection($Inf, [string]$Name) {
    $key = $Name.Trim().ToLowerInvariant()
    if ($Inf.Sections.ContainsKey($key)) { return @($Inf.Sections[$key]) }
    return @()
}

function Split-HeliosInfFields([string]$Text) {
    $fields = New-Object System.Collections.Generic.List[string]
    $cur = ""; $q = $false
    foreach ($ch in $Text.ToCharArray()) {
        if ($ch -eq '"') { $q = -not $q; $cur += $ch }
        elseif ($ch -eq ',' -and -not $q) { $fields.Add($cur.Trim()); $cur = "" }
        else { $cur += $ch }
    }
    $fields.Add($cur.Trim())
    return ,$fields.ToArray()
}

# One field with its quotes removed and %strkey% / %13% replaced.
function Expand-HeliosInfField($Inf, [string]$Text, [string]$DriverStore) {
    $value = $Text.Trim()
    if ($value.StartsWith('"') -and $value.EndsWith('"') -and $value.Length -ge 2) { $value = $value.Substring(1, $value.Length - 2) }
    $strings = $Inf.Strings
    return [regex]::Replace($value, '%([^%]*)%', {
        param($m)
        $name = $m.Groups[1].Value
        if ($name -eq "") { return "%" }
        if ($name -eq "13") { return $DriverStore }
        $key = $name.ToLowerInvariant()
        if ($strings.ContainsKey($key)) { return $strings[$key] }
        return $m.Value
    })
}

# The values of the directive $Directive (AddReg, CopyFiles) in a section.
function Get-HeliosInfDirective($Inf, [string]$Section, [string]$Directive) {
    $names = New-Object System.Collections.Generic.List[string]
    foreach ($line in (Get-HeliosInfSection $Inf $Section)) {
        if ($line -match ('^' + [regex]::Escape($Directive) + '\s*=\s*(.+)$')) {
            foreach ($item in ($Matches[1] -split ',')) {
                $item = $item.Trim()
                if ($item) { $names.Add($item) }
            }
        }
    }
    return $names.ToArray()
}

# The install section a models line maps $HardwareId to (the first one, in INF order).
function Find-HeliosInfInstallSection($Inf, [string]$HardwareId) {
    $pattern = '=\s*([^,\s]+)\s*,\s*' + [regex]::Escape($HardwareId) + '(\s*,.*)?$'
    foreach ($name in $Inf.Order) {
        foreach ($line in $Inf.Sections[$name]) {
            if ($line -match $pattern) { return $Matches[1].ToLowerInvariant() }
        }
    }
    return ""
}

# The HKR lines of the AddReg sections, as {Subkey, Name, Flags, Data}.
function Get-HeliosInfHkrValues($Inf, [string[]]$AddRegSections, [string]$DriverStore) {
    $values = New-Object System.Collections.Generic.List[object]
    foreach ($section in $AddRegSections) {
        foreach ($line in (Get-HeliosInfSection $Inf $section)) {
            $f = Split-HeliosInfFields $line
            if ($f.Count -lt 4 -or $f[0] -ne "HKR") { continue }
            $flagText = Expand-HeliosInfField $Inf $f[3] $DriverStore
            $flags = if ($flagText -match '^0x') { [Convert]::ToUInt32($flagText.Substring(2), 16) } elseif ($flagText) { [uint32]$flagText } else { [uint32]0 }
            $data = @($f | Select-Object -Skip 4 | ForEach-Object { Expand-HeliosInfField $Inf $_ $DriverStore })
            $values.Add([pscustomobject]@{
                Subkey = (Expand-HeliosInfField $Inf $f[1] $DriverStore)
                Name = (Expand-HeliosInfField $Inf $f[2] $DriverStore)
                Flags = $flags
                Data = $data
            })
        }
    }
    return $values.ToArray()
}

function ConvertTo-HeliosRegistryDword([string]$Text) {
    $value = if ($Text -match '^0x') { [Convert]::ToUInt32($Text.Substring(2), 16) } elseif ($Text) { [Convert]::ToUInt32($Text, 10) } else { [uint32]0 }
    return [BitConverter]::ToInt32([BitConverter]::GetBytes([uint32]$value), 0)
}

# Record a value's current state in $Journal (once: the first record is the one
# a revert restores). Keys are "subkey|name" below $Root.
function Add-HeliosRegistryJournal($Journal, [string]$Root, [string]$Subkey, [string]$Name) {
    $id = ("{0}|{1}" -f $Subkey, $Name).ToLowerInvariant()
    foreach ($entry in $Journal.registry) { if ($entry.id -eq $id) { return } }
    $path = if ($Subkey) { Join-Path $Root $Subkey } else { $Root }
    $snapshot = Get-HeliosRegistrySnapshot $path $Name
    $Journal.registry.Add([ordered]@{
        id = $id; root = $Root; subkey = $Subkey; name = $Name
        exists = $snapshot.exists; kind = $snapshot.kind; value = $snapshot.value
    })
}

# Write HKR values below $Root, journaling each first when $Journal is given.
# Returns how many were written.
function Set-HeliosInfHkrValues($Values, [string]$Root, $Journal) {
    $count = 0
    foreach ($v in $Values) {
        if ($v.Flags -band 0x14) { continue }   # DELVAL / KEYONLY: nothing to write
        $key = if ($v.Subkey) { Join-Path $Root $v.Subkey } else { $Root }
        if (-not (Test-Path -LiteralPath $key)) {
            if ($null -ne $Journal) { $Journal.createdKeys.Add($key) }
            Ensure-HeliosRegistryKey $key
        }
        if ($null -ne $Journal) { Add-HeliosRegistryJournal $Journal $Root $v.Subkey $v.Name }
        $data = @($v.Data)
        $type = $v.Flags -band 0xFFFF0001
        $append = ($v.Flags -band 0x8) -ne 0
        switch ($type) {
            0x00010001 {
                $text = if ($data.Count) { [string]$data[0] } else { "0" }
                New-ItemProperty -LiteralPath $key -Name $v.Name -Value (ConvertTo-HeliosRegistryDword $text) -PropertyType DWord -Force | Out-Null
            }
            0x00010000 {
                $values = @($data | Where-Object { $_ -ne "" })
                if ($append) {
                    $old = @((Get-Item -LiteralPath $key).GetValue($v.Name, @()))
                    $values = @($old + ($values | Where-Object { $old -notcontains $_ }))
                }
                New-ItemProperty -LiteralPath $key -Name $v.Name -Value ([string[]]$values) -PropertyType MultiString -Force | Out-Null
            }
            0x00020000 { New-ItemProperty -LiteralPath $key -Name $v.Name -Value ($data -join ",") -PropertyType ExpandString -Force | Out-Null }
            0x00000001 {
                $bytes = [byte[]]@($data | Where-Object { $_ -ne "" } | ForEach-Object { [Convert]::ToByte($_, 16) })
                New-ItemProperty -LiteralPath $key -Name $v.Name -Value $bytes -PropertyType Binary -Force | Out-Null
            }
            0x00000000 {
                if ($append) {
                    $old = @((Get-Item -LiteralPath $key).GetValue($v.Name, @()))
                    $values = @($old + ($data | Where-Object { $_ -ne "" -and $old -notcontains $_ }))
                    New-ItemProperty -LiteralPath $key -Name $v.Name -Value ([string[]]$values) -PropertyType MultiString -Force | Out-Null
                } else {
                    New-ItemProperty -LiteralPath $key -Name $v.Name -Value ($data -join ",") -PropertyType String -Force | Out-Null
                }
            }
            default { continue }
        }
        $count++
    }
    return $count
}

# The HKR values of one of the Helios INF's install sections (Helios_Install or
# Helios_NVIDIA_Install), read from the INF PnP selected for the adapter.
function Get-HeliosOwnDeviceSettings([string]$InstallSection, [string]$DriverStore) {
    $activeInf = Get-HeliosActiveInf (Get-HeliosDeviceInstanceId)
    $path = Join-Path $env:windir "INF\$activeInf"
    if (-not $activeInf -or -not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "The adapter's active INF ($activeInf) was not found."
    }
    $inf = Read-HeliosInf $path
    $addRegs = @(Get-HeliosInfDirective $inf $InstallSection "AddReg")
    if ($addRegs.Count -eq 0) { throw "$path has no AddReg in [$InstallSection]; it is not a Helios driver package with the RTX personality." }
    return @(Get-HeliosInfHkrValues $inf $addRegs $DriverStore)
}

# ---- the NVIDIA package -----------------------------------------------------

# The NVIDIA package at $Root: its INF, version, install section for the RTX
# 5090 and the files to stage ({Source (relative), Name}).
function Read-HeliosNvidiaPackage([Parameter(Mandatory)][string]$Root) {
    $infs = @(Get-ChildItem -LiteralPath $Root -Filter "nv_disp*.inf" -File -ErrorAction SilentlyContinue)
    foreach ($candidate in $infs) {
        $inf = Read-HeliosInf $candidate.FullName
        $install = Find-HeliosInfInstallSection $inf "PCI\VEN_10DE&DEV_2B85"
        if (-not $install) { continue }
        $driverVer = ""
        foreach ($line in (Get-HeliosInfSection $inf "Version")) {
            if ($line -match '^DriverVer\s*=\s*[^,]*,\s*([0-9.]+)') { $driverVer = $Matches[1] }
        }
        if (-not $driverVer) { throw "$($candidate.FullName) has no DriverVer." }
        $destinations = @{}
        foreach ($line in (Get-HeliosInfSection $inf "DestinationDirs")) {
            if ($line -match '^([^=]+?)\s*=\s*(.+)$') { $destinations[$Matches[1].Trim().ToLowerInvariant()] = ($Matches[2] -replace '\s', '') }
        }
        $defaultDestination = if ($destinations.ContainsKey("defaultdestdir")) { $destinations["defaultdestdir"] } else { "" }
        $sourceDirs = @{}
        foreach ($line in (Get-HeliosInfSection $inf "SourceDisksFiles")) {
            if ($line -match '^([^=]+?)\s*=\s*(.*)$') {
                $f = Split-HeliosInfFields $Matches[2]
                $sourceDirs[$Matches[1].Trim().ToLowerInvariant()] = if ($f.Count -ge 2) { $f[1].Trim('"') } else { "" }
            }
        }
        $files = [ordered]@{}
        function Add-PackageFile([string]$Name, [string]$Source) {
            if (-not $Source) { $Source = $Name }
            $dir = if ($sourceDirs.ContainsKey($Source.ToLowerInvariant())) { $sourceDirs[$Source.ToLowerInvariant()] } else { "" }
            $relative = if ($dir) { Join-Path $dir $Source } else { $Source }
            $files[$Name.ToLowerInvariant()] = [pscustomobject]@{ Name = $Name; Source = $relative }
        }
        # What NVIDIA's INF copies into its driver-store folder itself (DIRID 13,
        # no subfolder): the UMDs and what they load beside them.
        foreach ($copy in (Get-HeliosInfDirective $inf $install "CopyFiles")) {
            if ($copy.StartsWith("@")) {
                if ($defaultDestination -eq "13") { Add-PackageFile $copy.Substring(1) "" }
                continue
            }
            $destination = if ($destinations.ContainsKey($copy.ToLowerInvariant())) { $destinations[$copy.ToLowerInvariant()] } else { $defaultDestination }
            if ($destination -ne "13") { continue }
            foreach ($entry in (Get-HeliosInfSection $inf $copy)) {
                $f = Split-HeliosInfFields $entry
                if ($f[0]) { Add-PackageFile $f[0] $(if ($f.Count -ge 2) { $f[1] } else { "" }) }
            }
        }
        foreach ($name in @(Get-HeliosNvidiaRequiredFiles) + @(Get-HeliosNvidiaSystemFiles | ForEach-Object { $_[2] })) {
            if (-not $files.Contains($name.ToLowerInvariant())) { Add-PackageFile $name "" }
        }
        return [pscustomobject]@{
            Root = $Root
            Inf = $inf
            InfPath = $candidate.FullName
            FileVersion = $driverVer
            Version = (ConvertTo-HeliosNvidiaVersion $driverVer)
            InstallSection = $install
            Files = @($files.Values)
        }
    }
    throw "No NVIDIA display INF for the RTX 5090 (nv_disp*.inf with PCI\VEN_10DE&DEV_2B85) was found in $Root."
}

# The package to stage: $Path (a folder holding nv_disp*.inf, or an extracted
# NVIDIA driver download with a Display.Driver folder), or else NVIDIA's own
# copy in the DriverStore whose version is $Expected. Never downloads.
function Resolve-HeliosNvidiaPackage([string]$Path, [Parameter(Mandatory)][string]$Expected) {
    if ($Path) {
        $item = Get-Item -LiteralPath $Path -ErrorAction Stop
        if (-not $item.PSIsContainer) {
            throw "$Path is a file. Extract the NVIDIA driver download first (for example with 7-Zip) and pass the folder."
        }
        $root = $item.FullName
        $display = Join-Path $root "Display.Driver"
        if (Test-Path -LiteralPath $display -PathType Container) { $root = $display }
        return (Read-HeliosNvidiaPackage $root)
    }
    $store = Join-Path $env:windir "System32\DriverStore\FileRepository"
    foreach ($candidate in @(Get-ChildItem -LiteralPath $store -Directory -Filter "nv_disp*.inf_amd64_*" -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending)) {
        try {
            $package = Read-HeliosNvidiaPackage $candidate.FullName
            if ($package.Version -eq $Expected) { return $package }
        } catch { }
    }
    throw "NVIDIA $Expected was not found in the DriverStore. Pass the folder of an extracted NVIDIA $Expected driver download."
}

# Throws unless the package holds every required file, signed by NVIDIA, with
# a kernel driver of the INF's version.
function Assert-HeliosNvidiaPackage($Package) {
    foreach ($name in (Get-HeliosNvidiaRequiredFiles)) {
        $entry = $Package.Files | Where-Object { $_.Name -ieq $name } | Select-Object -First 1
        if (-not $entry -or -not (Test-Path -LiteralPath (Join-Path $Package.Root $entry.Source) -PathType Leaf)) {
            throw "The NVIDIA driver package at $($Package.Root) has no $name."
        }
    }
    $kernel = Join-Path $Package.Root (($Package.Files | Where-Object { $_.Name -ieq "nvlddmkm.sys" } | Select-Object -First 1).Source)
    $kernelVersion = (Get-Item -LiteralPath $kernel).VersionInfo.FileVersion
    if ((ConvertTo-HeliosNvidiaVersion $kernelVersion) -ne $Package.Version) {
        throw "nvlddmkm.sys is $kernelVersion, but the INF says $($Package.FileVersion)."
    }
    # The D3D UMD carries NVIDIA's own signature (a kernel driver may carry
    # Microsoft's hardware publisher signature instead).
    $entry = $Package.Files | Where-Object { $_.Name -ieq "nvldumdx.dll" } | Select-Object -First 1
    $signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $Package.Root $entry.Source)
    if ($signature.Status -ne "Valid" -or -not $signature.SignerCertificate -or
        $signature.SignerCertificate.Subject -notmatch "O=NVIDIA Corporation|CN=Microsoft Windows Hardware Compatibility Publisher") {
        throw "nvldumdx.dll in $($Package.Root) is not signed by NVIDIA ($($signature.Status))."
    }
}

# The driver-store folder of the Helios package PnP selected for the adapter:
# the FileRepository folder whose helios_kmd_render.inf is the active INF.
function Get-HeliosDriverStorePath {
    $instanceId = Get-HeliosDeviceInstanceId
    $activeInf = Get-HeliosActiveInf $instanceId
    $activeInfPath = Join-Path $env:windir "INF\$activeInf"
    if (-not $activeInf -or -not (Test-Path -LiteralPath $activeInfPath -PathType Leaf)) {
        throw "The adapter's active INF ($activeInf) was not found."
    }
    $hash = Get-HeliosSha256 $activeInfPath
    $root = Join-Path $env:windir "System32\DriverStore\FileRepository"
    foreach ($directory in @(Get-ChildItem -LiteralPath $root -Directory -Filter "helios_kmd_render.inf_amd64_*" -ErrorAction SilentlyContinue)) {
        $inf = Join-Path $directory.FullName "helios_kmd_render.inf"
        if ((Test-Path -LiteralPath $inf -PathType Leaf) -and (Get-HeliosSha256 $inf) -eq $hash -and
            (Test-Path -LiteralPath (Join-Path $directory.FullName "helios_kmd_render.sys") -PathType Leaf)) {
            return $directory.FullName
        }
    }
    throw "The driver-store folder of $activeInf (helios_kmd_render.inf) was not found."
}

# ---- the journal ------------------------------------------------------------

function New-HeliosNvidiaJournal {
    return [ordered]@{
        mode = "nvidia"
        package = ""
        version = ""
        hostRmVersion = ""
        destination = ""
        files = New-Object System.Collections.Generic.List[string]
        systemFiles = New-Object System.Collections.Generic.List[object]
        registry = New-Object System.Collections.Generic.List[object]
        createdKeys = New-Object System.Collections.Generic.List[string]
        acl = $null
    }
}

function Read-HeliosNvidiaState {
    $path = Get-HeliosNvidiaStatePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $null }
    return (Get-Content -LiteralPath $path -Raw | ConvertFrom-Json)
}

function Save-HeliosNvidiaState($Journal) {
    $path = Get-HeliosNvidiaStatePath
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
    Write-HeliosJson $Journal $path
}

function Restore-HeliosJournalValue($Entry) {
    $path = if ([string]$Entry.subkey) { Join-Path ([string]$Entry.root) ([string]$Entry.subkey) } else { [string]$Entry.root }
    $name = [string]$Entry.name
    if (-not [bool]$Entry.exists) {
        if (Test-Path -LiteralPath $path) { Remove-ItemProperty -LiteralPath $path -Name $name -ErrorAction SilentlyContinue }
        return
    }
    $kind = [string]$Entry.kind
    $value = switch ($kind) {
        "Binary" { [byte[]]@($Entry.value | ForEach-Object { [byte]$_ }) }
        "MultiString" { [string[]]@($Entry.value) }
        "DWord" { [int32]$Entry.value }
        "QWord" { [int64]$Entry.value }
        default { [string]$Entry.value }
    }
    Ensure-HeliosRegistryKey $path
    New-ItemProperty -LiteralPath $path -Name $name -Value $value -PropertyType $kind -Force | Out-Null
}

# Copy $From over $To unless $To already holds the same bytes. A file a
# process has loaded cannot be overwritten, but it can be renamed: it is moved
# aside and deleted at the next restart.
function Copy-HeliosFileReplacingInUse([Parameter(Mandatory)][string]$From, [Parameter(Mandatory)][string]$To) {
    if ((Test-Path -LiteralPath $To -PathType Leaf) -and (Get-HeliosSha256 $To) -eq (Get-HeliosSha256 $From)) { return }
    try {
        Copy-Item -LiteralPath $From -Destination $To -Force
    } catch {
        if (-not (Test-Path -LiteralPath $To -PathType Leaf)) { throw }
        $old = "{0}.{1}.old" -f $To, [DateTime]::UtcNow.Ticks
        Rename-Item -LiteralPath $To -NewName (Split-Path -Leaf $old) -Force
        Copy-Item -LiteralPath $From -Destination $To -Force
        [void](Remove-HeliosFileOrScheduleAtReboot $old)
    }
}

function Get-HeliosNvidiaAclPath { return (Join-Path (Split-Path -Parent (Get-HeliosNvidiaStatePath)) "nvidia-umd-driverstore.acl") }

# ---- switching --------------------------------------------------------------

# Undo what Enable-HeliosNvidiaUmd recorded in nvidia-umd.json. Unless
# -ForUninstall, the Helios UMDs are registered again (on the RTX personality:
# the Helios_Install values of the active INF, and NvUmd 0 in the service key).
# $KeepFilesIn: Enable-HeliosNvidiaUmd staging again into that same folder; the
# staged files and the folder's permissions stay for it (loaded files cannot
# be replaced in place). A Windows restart applies the change.
function Disable-HeliosNvidiaUmd([switch]$ForUninstall, [string]$KeepFilesIn = "") {
    $state = Read-HeliosNvidiaState
    if ($state -and [string]$state.mode -eq "nvidia") {
        $backupRoot = Join-Path (Split-Path -Parent (Get-HeliosNvidiaStatePath)) "nvidia-umd-backup"
        foreach ($entry in @($state.systemFiles)) {
            $target = [string]$entry.path
            if ((Test-Path -LiteralPath $target -PathType Leaf) -and (Get-HeliosSha256 $target) -ne [string]$entry.sha256) {
                Write-Warning "Keeping $target`: it changed after the NVIDIA UMDs were staged."
                continue
            }
            if ([string]$entry.backup -and (Test-Path -LiteralPath ([string]$entry.backup) -PathType Leaf)) {
                Copy-HeliosFileReplacingInUse ([string]$entry.backup) $target
            } elseif (Test-Path -LiteralPath $target -PathType Leaf) {
                [void](Remove-HeliosFileOrScheduleAtReboot $target)
            }
        }
        $entries = @($state.registry)
        [array]::Reverse($entries)
        foreach ($entry in $entries) {
            try { Restore-HeliosJournalValue $entry } catch {
                Write-Warning "Could not restore $($entry.root)\$($entry.subkey)\$($entry.name): $($_.Exception.Message)"
            }
        }
        $created = @($state.createdKeys)
        [array]::Reverse($created)
        foreach ($key in $created) {
            $item = Get-Item -LiteralPath ([string]$key) -ErrorAction SilentlyContinue
            if ($item -and $item.ValueCount -eq 0 -and $item.SubKeyCount -eq 0) {
                Remove-Item -LiteralPath ([string]$key) -ErrorAction SilentlyContinue
            }
        }
        $destination = [string]$state.destination
        $keep = $KeepFilesIn -and $destination -and ($destination -ieq $KeepFilesIn)
        if (-not $keep -and $destination -and (Test-Path -LiteralPath $destination -PathType Container)) {
            foreach ($name in @($state.files)) {
                $path = Join-Path $destination ([string]$name)
                if (Test-Path -LiteralPath $path -PathType Leaf) { [void](Remove-HeliosFileOrScheduleAtReboot $path) }
            }
            if ($state.acl) {
                try {
                    Invoke-HeliosNative "icacls.exe" @($destination, "/setowner", ("*" + [string]$state.acl.owner), "/T", "/C", "/Q")
                    if (Test-Path -LiteralPath ([string]$state.acl.saved) -PathType Leaf) {
                        Invoke-HeliosNative "icacls.exe" @((Split-Path -Parent $destination), "/restore", [string]$state.acl.saved, "/C", "/Q")
                        Remove-Item -LiteralPath ([string]$state.acl.saved) -Force -ErrorAction SilentlyContinue
                    }
                } catch {
                    Write-Warning "Could not restore the permissions of $destination`: $($_.Exception.Message)"
                }
            }
        }
        Remove-Item -LiteralPath $backupRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($ForUninstall) {
        Remove-Item -LiteralPath (Get-HeliosNvidiaStatePath) -Force -ErrorAction SilentlyContinue
        return
    }
    $instanceId = Get-HeliosDeviceInstanceId
    if (Test-HeliosNvidiaPersonality $instanceId) {
        $classKey = Get-HeliosDisplayClassKey $instanceId
        $driverStore = Get-HeliosDriverStorePath
        $helios = @(Get-HeliosOwnDeviceSettings "Helios_Install" $driverStore)
        $nvidia = @(Get-HeliosOwnDeviceSettings "Helios_NVIDIA_Install" $driverStore)
        [void](Set-HeliosInfHkrValues $helios $classKey $null)
        foreach ($v in $nvidia) {
            if (-not $v.Subkey -and -not ($helios | Where-Object { -not $_.Subkey -and $_.Name -ieq $v.Name })) {
                Remove-ItemProperty -LiteralPath $classKey -Name $v.Name -ErrorAction SilentlyContinue
            }
        }
        New-ItemProperty -LiteralPath (Get-HeliosServiceKey) -Name "NvUmd" -Value 0 -PropertyType DWord -Force | Out-Null
    }
    Save-HeliosNvidiaState ([ordered]@{ mode = "helios" })
}

# Stage NVIDIA's own UMDs from $Package (see Resolve-HeliosNvidiaPackage) into
# the Helios driver-store folder and register them. Only on the RTX
# personality. A Windows restart applies the change.
function Enable-HeliosNvidiaUmd([string]$Package) {
    $instanceId = Get-HeliosDeviceInstanceId
    if (-not (Test-HeliosNvidiaPersonality $instanceId)) {
        throw "NVIDIA's user-mode drivers need the RTX personality (PCI\VEN_10DE&DEV_2B85); this adapter is $instanceId."
    }
    $expected = Get-HeliosNvidiaExpectedVersion
    $hostRm = Get-HeliosHostRmVersion -WaitSeconds 30
    $source = Resolve-HeliosNvidiaPackage $Package $expected
    Assert-HeliosNvidiaVersion $source.Version $expected $hostRm
    Assert-HeliosNvidiaPackage $source

    $classKey = Get-HeliosDisplayClassKey $instanceId
    $destination = Get-HeliosDriverStorePath
    # What an earlier run changed is undone first, so the journal describes
    # this run only. Files it staged into this same folder stay (they may be
    # loaded) and are replaced or removed below.
    $previous = Read-HeliosNvidiaState
    $kept = $null
    if ($previous -and [string]$previous.mode -eq "nvidia") {
        Disable-HeliosNvidiaUmd -ForUninstall -KeepFilesIn $destination
        if ([string]$previous.destination -ieq $destination) { $kept = $previous }
    }
    $service = Get-HeliosServiceKey
    $journal = New-HeliosNvidiaJournal
    $journal.package = $source.Root
    $journal.version = $source.Version
    $journal.hostRmVersion = $hostRm
    $journal.destination = $destination
    $stateRoot = Split-Path -Parent (Get-HeliosNvidiaStatePath)
    $backupRoot = Join-Path $stateRoot "nvidia-umd-backup"
    New-Item -ItemType Directory -Force -Path $backupRoot | Out-Null
    if ($kept) {
        $journal.acl = $kept.acl
        foreach ($name in @($kept.files)) { $journal.files.Add([string]$name) }
    }
    # Saved before anything changes: a failure part-way is undone from it.
    Save-HeliosNvidiaState $journal
    try {
        # The driver-store folder belongs to SYSTEM; administrators take it
        # over for the copy, and a revert puts owner and permissions back.
        if (-not $journal.acl) {
            $aclFile = Get-HeliosNvidiaAclPath
            $owner = (Get-Acl -LiteralPath $destination).GetOwner([Security.Principal.SecurityIdentifier]).Value
            Invoke-HeliosNative "icacls.exe" @($destination, "/save", $aclFile, "/T", "/C", "/Q")
            $journal.acl = [ordered]@{ owner = $owner; saved = $aclFile }
            Save-HeliosNvidiaState $journal
        }
        & takeown.exe /F $destination /R /D Y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "takeown $destination failed with exit code $LASTEXITCODE." }
        Invoke-HeliosNative "icacls.exe" @($destination, "/grant", "*S-1-5-32-544:(OI)(CI)F", "/T", "/C", "/Q")

        # No NVIDIA file may replace one of the Helios package's own.
        $own = @{ "helios_kmd_render.inf" = $true; "helios_kmd_render.cat" = $true }
        $ownInf = Read-HeliosInf (Join-Path $destination "helios_kmd_render.inf")
        foreach ($line in (Get-HeliosInfSection $ownInf "SourceDisksFiles")) {
            if ($line -match '^([^=]+?)\s*=') { $own[$Matches[1].Trim().ToLowerInvariant()] = $true }
        }
        foreach ($file in $source.Files) {
            if ($own.ContainsKey($file.Name.ToLowerInvariant())) {
                throw "The NVIDIA package would replace the Helios file $($file.Name) in $destination."
            }
        }
        $staged = New-Object System.Collections.Generic.List[string]
        foreach ($file in $source.Files) {
            $from = Join-Path $source.Root $file.Source
            if (-not (Test-Path -LiteralPath $from -PathType Leaf)) {
                Write-Warning "The NVIDIA package has no $($file.Source); skipped."
                continue
            }
            if ($journal.files -notcontains $file.Name) { $journal.files.Add($file.Name) }
            $staged.Add($file.Name)
            Copy-HeliosFileReplacingInUse $from (Join-Path $destination $file.Name)
        }
        # Files an earlier staging left here that this package does not have.
        foreach ($name in @($journal.files)) {
            if ($staged -notcontains $name) {
                $path = Join-Path $destination $name
                if (Test-Path -LiteralPath $path -PathType Leaf) { [void](Remove-HeliosFileOrScheduleAtReboot $path) }
                [void]$journal.files.Remove($name)
            }
        }
        Save-HeliosNvidiaState $journal
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
        $aliasService = "HKLM:\SYSTEM\CurrentControlSet\Services\nvlddmkm"
        if (-not (Test-Path -LiteralPath $aliasService)) {
            $journal.createdKeys.Add($aliasService)
            Ensure-HeliosRegistryKey $aliasService
        }
        Add-HeliosRegistryJournal $journal $aliasService "" "ImagePath"
        $imagePath = "\SystemRoot\System32\DriverStore\FileRepository\$(Split-Path -Leaf $destination)\nvlddmkm.sys"
        Set-ItemProperty -LiteralPath $aliasService -Name ImagePath -Value $imagePath -Type ExpandString

        # System32/SysWOW64 files the UMDs and CUDA load from there (the CUDA
        # loader as System32\nvcuda.dll; NVIDIA's NVAPI in place of DXVK-NVAPI).
        # What was there before is kept in the backup folder for the revert.
        foreach ($entry in (Get-HeliosNvidiaSystemFiles)) {
            $target = Join-Path (Join-Path $env:SystemRoot $entry[0]) $entry[1]
            $backup = ""
            if (Test-Path -LiteralPath $target -PathType Leaf) {
                $backup = Join-Path (Join-Path $backupRoot $entry[0]) $entry[1]
                New-Item -ItemType Directory -Force -Path (Split-Path -Parent $backup) | Out-Null
                Copy-Item -LiteralPath $target -Destination $backup -Force
            }
            Copy-HeliosFileReplacingInUse (Join-Path $destination $entry[2]) $target
            $journal.systemFiles.Add([ordered]@{ path = $target; backup = $backup; sha256 = (Get-HeliosSha256 $target) })
            Save-HeliosNvidiaState $journal
        }

        # The Helios INF's RTX values (the UMD names), then the HKR values of
        # NVIDIA's own INF for the RTX 5090 (DXCoreAttributes, DriverSupportModules,
        # UserModeDriverGUID and the rest NVIDIA's UMDs read through D3DKMT
        # QUERYREGISTRY), %13% being the Helios driver-store folder.
        $ownValues = @(Get-HeliosOwnDeviceSettings "Helios_NVIDIA_Install" $destination)
        [void](Set-HeliosInfHkrValues $ownValues $classKey $journal)
        $addRegs = @(Get-HeliosInfDirective $source.Inf $source.InstallSection "AddReg")
        $nvidiaValues = @(Get-HeliosInfHkrValues $source.Inf $addRegs $destination)
        $imported = Set-HeliosInfHkrValues $nvidiaValues $classKey $journal
        # NvUmd from the INF (Parameters, 2) applies; the service-key override
        # of Helios mode (0) goes.
        if ((Get-Item -LiteralPath $service).GetValue("NvUmd", $null) -eq 0) {
            Add-HeliosRegistryJournal $journal $service "" "NvUmd"
            Remove-ItemProperty -LiteralPath $service -Name "NvUmd"
        }
        Save-HeliosNvidiaState $journal
    } catch {
        Write-Warning "Staging NVIDIA's user-mode drivers failed; undoing it: $($_.Exception.Message)"
        Save-HeliosNvidiaState $journal
        try { Disable-HeliosNvidiaUmd } catch { Write-Warning "The undo did not complete: $($_.Exception.Message)" }
        throw
    }
    return [pscustomobject]@{
        Source = $source.Root
        Version = $source.Version
        HostRmVersion = $hostRm
        Destination = $destination
        Files = $journal.files.Count
        RegistryValues = $imported
    }
}

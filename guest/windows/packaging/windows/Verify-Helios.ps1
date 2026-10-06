param(
    [switch]$RunSmokeTests,
    [switch]$AllowPendingReboot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Helios-PackageCommon.ps1")
if (-not [Environment]::Is64BitProcess) {
    throw "Run this script with native 64-bit PowerShell to manage both registry views and system directories."
}

$statePath = Join-Path $env:ProgramData "Helios\install-state.json"
if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
    throw "No package-managed Helios installation was found at $statePath."
}
$state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
$failures = [Collections.Generic.List[string]]::new()
$instanceId = ""
$classKey = ""

foreach ($entry in @($state.runtimeFiles)) {
    if (-not (Test-Path -LiteralPath ([string]$entry.path) -PathType Leaf)) {
        $failures.Add("Missing runtime file: $($entry.path)")
        continue
    }
    $actual = Get-HeliosSha256 ([string]$entry.path)
    if ($actual -ne ([string]$entry.sha256).ToUpperInvariant()) {
        $failures.Add("Runtime hash mismatch: $($entry.path)")
    }
}

try {
    $instanceId = Get-HeliosDeviceInstanceId
    $classKey = Get-HeliosDisplayClassKey $instanceId
    $device = Get-PnpDevice -InstanceId $instanceId -ErrorAction Stop
    Write-Host "Driver: $($device.FriendlyName) [$($device.Status)]"
    if ($device.Status -ne "OK") {
        if ($AllowPendingReboot) {
            Write-Warning "Helios PnP status is $($device.Status); the installer has not rebooted yet."
        } else {
            $failures.Add("Helios PnP device status is $($device.Status).")
        }
    }
    $expectedPublisher = Get-HeliosPackagePublisher $state
    # Read the bound device's provider first. Win32_PnPSignedDriver can leave
    # its provider empty even for a healthy, activated Helios device.
    $providerProperty = Get-PnpDeviceProperty -InstanceId $instanceId -KeyName DEVPKEY_Device_DriverProvider -ErrorAction SilentlyContinue
    $providerName = ""
    $providerSource = "PnP"
    if ($providerProperty -and $providerProperty.PSObject.Properties["Data"]) {
        $providerName = [string]$providerProperty.Data
    }
    if ([string]::IsNullOrWhiteSpace($providerName)) {
        $signedDriver = Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceID -eq $instanceId } | Select-Object -First 1
        $providerSource = "CIM fallback"
        if ($signedDriver) { $providerName = [string]$signedDriver.DriverProviderName }
    }
    if (-not [string]::IsNullOrWhiteSpace($providerName)) {
        Write-Host "Driver provider ($providerSource): $providerName"
    }
    if ([string]::IsNullOrWhiteSpace($providerName)) {
        # A staged driver awaiting activation may lack provider metadata in
        # both sources. Registration and file checks below remain mandatory;
        # only this metadata check waits.
        if ($AllowPendingReboot -and $device.Status -ne "OK") {
            Write-Warning "Active driver provider metadata is unavailable while Helios PnP status is $($device.Status). Verify again without -AllowPendingReboot after reboot."
        } else {
            $failures.Add("The active display driver provider metadata is unavailable, expected $expectedPublisher.")
        }
    } elseif ($providerName -ne $expectedPublisher) {
        $failures.Add("The active display driver provider is $providerName, expected $expectedPublisher.")
    }
} catch {
    $failures.Add($_.Exception.Message)
}

# Registration is indexed by API version, independently for AMD64 and WoW64.
# Windows loads an absolute DriverStore path from each slot; validate both the
# selected architecture and installed bytes against the bundle's recorded hash.
$driverDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($registration in @(
    @{ name = "UserModeDriverName"; architecture = "x64"; files = @("helios_umd.dll", "helios_umd.dll", "helios_umd.dll", "helios_umd12.dll") },
    @{ name = "UserModeDriverNameWoW"; architecture = "x86"; files = @("helios_umd32.dll", "helios_umd32.dll", "helios_umd32.dll", "helios_umd12_32.dll") }
)) {
    try {
        if (-not $classKey) { throw "No display software key for $($registration.name)." }
        $key = Get-Item -LiteralPath $classKey
        $paths = @($key.GetValue($registration.name, $null))
        if ($key.GetValueKind($registration.name) -ne [Microsoft.Win32.RegistryValueKind]::MultiString -or $paths.Count -ne 4) {
            throw "$($registration.name) must have four REG_MULTI_SZ API slots."
        }
        for ($slot = 0; $slot -lt 4; $slot++) {
            $path = [string]$paths[$slot]
            if (-not [IO.Path]::IsPathRooted($path) -or [IO.Path]::GetFileName($path) -ine $registration.files[$slot]) {
                throw "$($registration.name) slot $slot does not select $($registration.files[$slot]): $path"
            }
            Assert-HeliosPeArchitecture $path $registration.architecture
            [void]$driverDirectories.Add((Split-Path -Parent $path))
        }
        Write-Host "Direct3D 11/12 $($registration.architecture): registered and architecture checked."
    } catch { $failures.Add($_.Exception.Message) }
}
if ($driverDirectories.Count -ne 1) {
    $failures.Add("The native and WoW64 UMDs must belong to one DriverStore package.")
} elseif (-not $state.PSObject.Properties["driverFiles"] -or @($state.driverFiles).Count -ne 5) {
    $failures.Add("Installation state is missing the five driver-image hashes; reinstall this bundle.")
} else {
    $driverDirectory = @($driverDirectories)[0]
    foreach ($entry in @($state.driverFiles)) {
        try {
            $path = Join-Path $driverDirectory ([string]$entry.name)
            if ((Get-HeliosSha256 $path) -ne ([string]$entry.sha256).ToUpperInvariant()) {
                $failures.Add("Installed driver hash mismatch: $path")
            }
        } catch { $failures.Add($_.Exception.Message) }
    }
}
if ($classKey) {
    $installedDrivers = @((Get-Item -LiteralPath $classKey).GetValue("InstalledDisplayDrivers", $null))
    $expectedDrivers = @("helios_umd", "helios_umd12", "helios_umd32", "helios_umd12_32")
    if ($installedDrivers.Count -ne 4 -or (Compare-Object $expectedDrivers $installedDrivers)) {
        $failures.Add("InstalledDisplayDrivers does not list all four distinct Helios UMDs.")
    }
}

$vulkanRegistry = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
$vulkanValue = if (Test-Path -LiteralPath $vulkanRegistry) { (Get-Item -LiteralPath $vulkanRegistry).GetValue([string]$state.vulkanManifest, $null) } else { $null }
if ($null -eq $vulkanValue -or [int]$vulkanValue -ne 0) {
    $failures.Add("The Vulkan ICD manifest is not enabled in the machine registry.")
} else { Write-Host "Vulkan: registered $($state.vulkanManifest)" }

$vulkanRegistryX86 = "HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers"
$vulkanX86Value = if (Test-Path -LiteralPath $vulkanRegistryX86) { (Get-Item -LiteralPath $vulkanRegistryX86).GetValue([string]$state.vulkanManifestX86, $null) } else { $null }
if ($null -eq $vulkanX86Value -or [int]$vulkanX86Value -ne 0) {
    $failures.Add("The x86 Vulkan ICD manifest is not enabled in the WoW64 registry.")
} else { Write-Host "Vulkan x86: registered $($state.vulkanManifestX86)" }

# The driver package's INF registers NVK (Vulkan) and Zink on NVK (OpenGL) on
# the adapter, from the driver store (REG_MULTI_SZ values); the installer
# registers the Venus Zink only when the INF did not.
function Get-HeliosClassValue([string]$Name) {
    if (-not $classKey -or -not (Test-Path -LiteralPath $classKey)) { return "" }
    $value = (Get-Item -LiteralPath $classKey).GetValue($Name, $null)
    if ($null -eq $value) { return "" }
    return (@($value) -join ";")
}
foreach ($check in @(
    @{ name = "OpenGLDriverName"; label = "OpenGL"; nvk = "\\helios_gl64\.dll$"; venus = (Join-Path ([string]$state.installRoot) "runtime\mesa\libgallium_wgl.dll") },
    @{ name = "OpenGLDriverNameWow"; label = "OpenGL x86"; nvk = "\\helios_gl32\.dll$"; venus = (Join-Path ([string]$state.installRoot) "runtime\mesa\x86\libgallium_wgl.dll") }
)) {
    $value = Get-HeliosClassValue $check.name
    if ($value -match $check.nvk) {
        Write-Host "$($check.label): Zink on NVK, registered $value"
    } elseif ($value -and $value -ieq $check.venus) {
        Write-Host "$($check.label): Zink on Venus, registered $value"
    } else {
        $failures.Add("The Helios $($check.label) ICD is not registered on the display adapter ($($check.name)='$value').")
    }
}
foreach ($check in @(
    @{ name = "VulkanDriverName"; label = "Vulkan (NVK)"; nvk = "\\helios_nvk64\.json$" },
    @{ name = "VulkanDriverNameWow"; label = "Vulkan x86 (NVK)"; nvk = "\\helios_nvk32\.json$" }
)) {
    $value = Get-HeliosClassValue $check.name
    if ($value -match $check.nvk) {
        $json = $value -replace '^.*;', ''
        if (Test-Path -LiteralPath $json -PathType Leaf) { Write-Host "$($check.label): registered $json" }
        else { $failures.Add("$($check.label) is registered as $json, which does not exist.") }
    } else {
        $failures.Add("NVK is not registered as the adapter's Vulkan driver ($($check.name)='$value').")
    }
}

$openClRegistry = "HKLM:\SOFTWARE\Khronos\OpenCL\Vendors"
$openClValue = if (Test-Path -LiteralPath $openClRegistry) { (Get-Item -LiteralPath $openClRegistry).GetValue([string]$state.openClVendor, $null) } else { $null }
if ($null -eq $openClValue -or [int]$openClValue -ne 0) {
    $failures.Add("The CLVK vendor DLL is not enabled in the OpenCL registry.")
} else { Write-Host "OpenCL: registered $($state.openClVendor)" }

if ($RunSmokeTests) {
    if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) {
        throw "Run graphics smoke tests in the logged-in desktop session (or an interactive scheduled task), not session 0."
    }
    $smokeRoot = Join-Path ([string]$state.installRoot) "runtime\smoke"
    $tests = @(
        [ordered]@{ name = "Vulkan"; exe = "vulkan-smoke.exe"; arguments = @() },
        [ordered]@{ name = "Direct3D 11"; exe = "d3d11-smoke.exe"; arguments = @() },
        [ordered]@{ name = "OpenGL"; exe = "opengl-smoke.exe"; arguments = @() },
        [ordered]@{ name = "OpenCL"; exe = "opencl-smoke.exe"; arguments = @() },
        [ordered]@{
            name = "OpenGL/OpenCL sharing"
            exe = "opencl-gl-sharing-smoke.exe"
            arguments = @("rgba16f")
        },
        [ordered]@{
            name = "Mixed OpenGL/D3D11 context compatibility"
            exe = "opencl-gl-sharing-smoke.exe"
            arguments = @("rgba16f", "d3d11-context")
        },
        [ordered]@{ name = "Direct3D 11 x86"; exe = "x86\d3d11-smoke.exe"; arguments = @() },
        [ordered]@{ name = "Vulkan x86"; exe = "x86\vulkan-smoke.exe"; arguments = @() },
        [ordered]@{ name = "Vulkan WSI x86"; exe = "x86\vulkan-wsi-probe.exe"; arguments = @() },
        [ordered]@{ name = "OpenGL x86"; exe = "x86\opengl-smoke.exe"; arguments = @() }
    )
    $heliosKey = Get-Item "HKLM:\SOFTWARE\Helios" -ErrorAction SilentlyContinue
    $dx12Disabled = $heliosKey -and ($null -ne $heliosKey.GetValue("UmdD3D12", $null)) -and
        ($heliosKey.GetValueKind("UmdD3D12") -eq [Microsoft.Win32.RegistryValueKind]::DWord) -and
        ($heliosKey.GetValue("UmdD3D12") -eq 0)
    foreach ($architecture in @("x64", "x86")) {
        $prefix = if ($architecture -eq "x86") { "x86\" } else { "" }
        $tests += [ordered]@{
            name = if ($dx12Disabled) { "Direct3D 12 $architecture explicit disable" } else { "Direct3D 12 $architecture" }
            exe = "${prefix}d3d12-smoke.exe"
            arguments = @("--expect", $(if ($dx12Disabled) { "fail" } else { "ok" }))
        }
        if (-not $dx12Disabled) {
            $tests += [ordered]@{
                name = "Direct3D 12 $architecture clear/readback"
                exe = "${prefix}d3d12-clear.exe"
                arguments = @("--expect", "ok")
            }
        }
    }
    foreach ($test in $tests) {
        $executable = Join-Path $smokeRoot $test.exe
        if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
            $failures.Add("$($test.name) smoke probe is not present in this bundle.")
            continue
        }
        $architecture = if ($test.exe.StartsWith("x86\")) { "x86" } else { "x64" }
        try { Assert-HeliosPeArchitecture $executable $architecture } catch {
            $failures.Add($_.Exception.Message)
            continue
        }
        Write-Host "Running $($test.name) smoke probe..."
        $probeArguments = @($test.arguments)
        & $executable @probeArguments
        if ($LASTEXITCODE -ne 0) { $failures.Add("$($test.name) smoke probe failed with exit code $LASTEXITCODE.") }
    }
}

if ($failures.Count -gt 0) {
    foreach ($failure in $failures) { Write-Error $failure -ErrorAction Continue }
    throw "Helios verification failed with $($failures.Count) problem(s)."
}
Write-Host "Helios x64/WoW64 Direct3D 11/12, Vulkan and OpenGL, and x64 OpenCL registrations and files are healthy."

<#
Chooses the user-mode drivers of the RTX personality (PCI\VEN_10DE&DEV_2B85).

  Set-HeliosUmd.ps1 -Nvidia [-Package <folder>]
      NVIDIA's own D3D/Vulkan/OpenGL/CUDA/NVAPI user-mode drivers, taken from
      the user's NVIDIA driver package: an extracted NVIDIA driver download
      (the folder holding Display.Driver, or Display.Driver itself), or, without
      -Package, NVIDIA's own copy in the DriverStore. Nothing is downloaded.
      The package's version must equal NvUmdVersion and its branch the host
      RM's (NvHostRmVersion); see Helios-PackageCommon.ps1.
  Set-HeliosUmd.ps1 -Helios
      The Helios UMDs (DXVK/vkd3d on NVK, Mesa): every change -Nvidia made is
      undone.
  Set-HeliosUmd.ps1
      Prints the current choice.

Restart Windows afterwards; the adapter picks the drivers up at its next start.
#>
[CmdletBinding(DefaultParameterSetName = "Show")]
param(
    [Parameter(Mandatory, ParameterSetName = "Nvidia")]
    [switch]$Nvidia,
    [Parameter(ParameterSetName = "Nvidia")]
    [string]$Package,
    [Parameter(Mandatory, ParameterSetName = "Helios")]
    [switch]$Helios
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Helios-PackageCommon.ps1")
if (-not [Environment]::Is64BitProcess) {
    throw "Run this script with native 64-bit PowerShell to manage both registry views and system directories."
}

switch ($PSCmdlet.ParameterSetName) {
    "Nvidia" {
        Assert-HeliosAdministrator
        $result = Enable-HeliosNvidiaUmd $Package
        Write-Host "Staged NVIDIA $($result.Version) user-mode drivers from $($result.Source) ($($result.Files) files, $($result.RegistryValues) registry values; host RM $($result.HostRmVersion))."
        Write-Warning "Restart Windows to load them."
    }
    "Helios" {
        Assert-HeliosAdministrator
        Disable-HeliosNvidiaUmd
        Write-Host "The Helios user-mode drivers are registered."
        Write-Warning "Restart Windows to load them."
    }
    default {
        $state = Read-HeliosNvidiaState
        if ($state -and [string]$state.mode -eq "nvidia") {
            Write-Host "NVIDIA $($state.version) user-mode drivers from $($state.package) (host RM $($state.hostRmVersion))."
        } else {
            Write-Host "Helios user-mode drivers."
        }
        Write-Host "Expected NVIDIA version: $(try { Get-HeliosNvidiaExpectedVersion } catch { 'not set' }); host RM: $(Get-HeliosHostRmVersion)"
    }
}

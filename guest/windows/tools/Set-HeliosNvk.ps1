# Points the Helios D3D UMD at an NVK-on-RM build (dxvk-on-nvk S3), or back
# to Venus. Administrator. Writes HKLM\SOFTWARE\Helios (the UMD, NVK and Zink
# read it once per process; new processes pick it up, no reboot):
#
#   NvkIcdPath    REG_SZ  <Nvk64>\vulkan_nouveau.dll   (64-bit processes)
#   NvkIcdPath32  REG_SZ  <Nvk32>\vulkan_nouveau.dll   (32-bit processes; only if present)
#   Icd           REG_SZ  nvk | venus
#   ForeignImport DWORD   1 with -ForeignImport: Venus processes (DWM) import
#                         NVK-made surfaces, so windowed NVK apps are composed
#                         instead of shown on scanout 0
#
#   Set-HeliosNvk.ps1 [-Nvk64 C:\Users\Public\s3\nvk64] [-Nvk32 C:\Users\Public\s3\nvk32]
#                     [-ForeignImport] [-Off] [-DriverStore]
#
# Each folder holds vulkan_nouveau.dll and librmclient.dll of the same build.
# -Off sets Icd=venus and removes ForeignImport: every process back on Venus,
# for D3D (the UMD), Vulkan (NVK, registered on the adapter by the driver
# package, then enumerates no device, so the loader hands apps Venus) and
# OpenGL (Zink falls back to the loader, i.e. Venus).
# -DriverStore removes NvkIcdPath/NvkIcdPath32 and sets Icd=nvk: the UMD and
# Zink use the NVK the driver package installed next to the UMDs.
# The deny-list (built in; NvkDenyList/NvkAllowList override it) keeps DWM,
# the shell, browsers and other interop-heavy apps on Venus regardless; NVK
# applies the same list to Vulkan apps, Zink to OpenGL apps.
param(
    [string]$Nvk64 = "C:\Users\Public\s3\nvk64",
    [string]$Nvk32 = "C:\Users\Public\s3\nvk32",
    [switch]$ForeignImport,
    [switch]$Off,
    [switch]$DriverStore
)
$ErrorActionPreference = "Stop"
$key = "HKLM:\SOFTWARE\Helios"
if (-not (Test-Path $key)) { New-Item -Path $key -Force | Out-Null }
if ($Off) {
    Set-ItemProperty -Path $key -Name Icd -Value "venus" -Type String
    Remove-ItemProperty -Path $key -Name ForeignImport -ErrorAction SilentlyContinue
    "Helios: Venus for every process, D3D, Vulkan and OpenGL (Icd=venus)."
    return
}
if ($DriverStore) {
    Remove-ItemProperty -Path $key -Name NvkIcdPath, NvkIcdPath32 -ErrorAction SilentlyContinue
} else {
    foreach ($d in @($Nvk64)) {
        foreach ($f in @("vulkan_nouveau.dll", "librmclient.dll")) {
            if (-not (Test-Path (Join-Path $d $f))) { throw "missing $d\$f" }
        }
    }
    Set-ItemProperty -Path $key -Name NvkIcdPath -Value (Join-Path $Nvk64 "vulkan_nouveau.dll") -Type String
    if ((Test-Path (Join-Path $Nvk32 "vulkan_nouveau.dll")) -and (Test-Path (Join-Path $Nvk32 "librmclient.dll"))) {
        Set-ItemProperty -Path $key -Name NvkIcdPath32 -Value (Join-Path $Nvk32 "vulkan_nouveau.dll") -Type String
    } else {
        Remove-ItemProperty -Path $key -Name NvkIcdPath32 -ErrorAction SilentlyContinue
        "no 32-bit NVK in ${Nvk32}: 32-bit processes use the driver store's NVK, if installed"
    }
}
Set-ItemProperty -Path $key -Name Icd -Value "nvk" -Type String
if ($ForeignImport) {
    Set-ItemProperty -Path $key -Name ForeignImport -Value 1 -Type DWord
} else {
    Remove-ItemProperty -Path $key -Name ForeignImport -ErrorAction SilentlyContinue
}
Get-ItemProperty -Path $key | Select-Object Icd, NvkIcdPath, NvkIcdPath32, ForeignImport, NvkDenyList, NvkAllowList | Format-List

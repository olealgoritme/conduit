# Points the Helios D3D11 UMD at an NVK-on-RM build (dxvk-on-nvk S3), or back
# to Venus. Administrator. Writes HKLM\SOFTWARE\Helios (the UMD reads it once
# per process; new processes pick it up, no reboot):
#
#   NvkIcdPath    REG_SZ  <Nvk64>\vulkan_nouveau.dll   (64-bit processes)
#   NvkIcdPath32  REG_SZ  <Nvk32>\vulkan_nouveau.dll   (32-bit processes; only if present)
#   Icd           REG_SZ  nvk | venus
#   ForeignImport DWORD   1 with -ForeignImport: Venus processes (DWM) import
#                         NVK-made surfaces, so windowed NVK apps are composed
#                         instead of shown on scanout 0
#
#   Set-HeliosNvk.ps1 [-Nvk64 C:\Users\Public\s3\nvk64] [-Nvk32 C:\Users\Public\s3\nvk32]
#                     [-ForeignImport] [-Off]
#
# Each folder holds vulkan_nouveau.dll and librmclient.dll of the same build.
# -Off sets Icd=venus (every process back on Venus) and removes ForeignImport.
# The deny-list (built in; NvkDenyList/NvkAllowList override it) keeps DWM,
# the shell, browsers and other interop-heavy apps on Venus regardless.
param(
    [string]$Nvk64 = "C:\Users\Public\s3\nvk64",
    [string]$Nvk32 = "C:\Users\Public\s3\nvk32",
    [switch]$ForeignImport,
    [switch]$Off
)
$ErrorActionPreference = "Stop"
$key = "HKLM:\SOFTWARE\Helios"
if (-not (Test-Path $key)) { New-Item -Path $key -Force | Out-Null }
if ($Off) {
    Set-ItemProperty -Path $key -Name Icd -Value "venus" -Type String
    Remove-ItemProperty -Path $key -Name ForeignImport -ErrorAction SilentlyContinue
    "Helios UMD: Venus for every process (Icd=venus)."
    return
}
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
    "no 32-bit NVK in $Nvk32: 32-bit processes stay on Venus"
}
Set-ItemProperty -Path $key -Name Icd -Value "nvk" -Type String
if ($ForeignImport) {
    Set-ItemProperty -Path $key -Name ForeignImport -Value 1 -Type DWord
} else {
    Remove-ItemProperty -Path $key -Name ForeignImport -ErrorAction SilentlyContinue
}
Get-ItemProperty -Path $key | Select-Object Icd, NvkIcdPath, NvkIcdPath32, ForeignImport, NvkDenyList, NvkAllowList | Format-List

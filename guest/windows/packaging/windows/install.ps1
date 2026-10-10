# Conduit Helios driver for a Windows 11 guest. Run in an elevated PowerShell
# inside the VM, from the unzipped folder:
#   powershell -ExecutionPolicy Bypass -File .\install.ps1            # desktop (DWM) on NVK-on-RM
#   powershell -ExecutionPolicy Bypass -File .\install.ps1 -VenusDesktop   # keep DWM on Venus
# First run enables test signing and asks for a reboot; run it again after.
param([switch]$VenusDesktop)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole('Administrators')) { throw 'Run this from an elevated (Administrator) PowerShell.' }
if ((bcdedit /enum '{current}') -notmatch 'testsigning\s+Yes') {
    bcdedit /set testsigning on | Out-Null
    Write-Host 'Test signing enabled. Reboot, then run install.ps1 again.' -ForegroundColor Yellow
    exit 0
}
$cer = Join-Path $here 'helios-dev-test.cer'
foreach ($store in 'Root', 'TrustedPublisher') { Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null }
pnputil /add-driver (Join-Path $here 'helios_kmd_render.inf') /install
$k = 'HKLM:\SOFTWARE\Helios'
if (-not (Test-Path $k)) { New-Item -Path $k | Out-Null }
if ($VenusDesktop) { Remove-ItemProperty $k -Name DwmIcd -EA 0 } else { Set-ItemProperty $k -Name DwmIcd -Value 'nvk' -Type String }
# Never blank or sleep the display (an idle-off display looked like a DWM freeze).
powercfg /change monitor-timeout-ac 0; powercfg /change standby-timeout-ac 0
$v = Get-CimInstance Win32_VideoController | Where-Object Name -match 'Conduit Helios'
Write-Host "Installed: $($v.Name) $($v.DriverVersion) $($v.CurrentHorizontalResolution)x$($v.CurrentVerticalResolution)@$($v.CurrentRefreshRate). Reboot once more to finish." -ForegroundColor Green

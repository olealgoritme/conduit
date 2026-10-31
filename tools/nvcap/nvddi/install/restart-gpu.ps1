# Restart the NVIDIA display adapter so nvlddmkm unloads and registers again
# through nvddi (needed when nvddi was loaded after nvlddmkm, e.g. right after install).
# The screen goes black for a few seconds.
$ErrorActionPreference = 'Stop'
$dev = Get-PnpDevice -Class Display -PresentOnly | Where-Object { $_.InstanceId -like 'PCI\VEN_10DE*' } | Select-Object -First 1
if (-not $dev) { Write-Error 'no NVIDIA display adapter found'; exit 1 }
Write-Host "restarting $($dev.FriendlyName) $($dev.InstanceId)"
& pnputil.exe /restart-device "$($dev.InstanceId)"
if ($LASTEXITCODE -ne 0) {
    Write-Host 'pnputil /restart-device failed, using disable/enable'
    Disable-PnpDevice -InstanceId $dev.InstanceId -Confirm:$false
    Start-Sleep -Seconds 3
    Enable-PnpDevice -InstanceId $dev.InstanceId -Confirm:$false
}
Start-Sleep -Seconds 5
& "$PSScriptRoot\nvddictl.exe" status

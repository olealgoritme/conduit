# Sets up the host's shared folders in this Windows guest: WinFsp (installed
# from the MSI when missing; the VirtIO-FS service needs it) and the startup
# task that mounts every virtiofs device tagged "conduit-NAME" as a drive.
# Run as administrator. Safe to run again. Install-Helios.ps1 runs it.
#
#   .\Install-ConduitShares.ps1 -WinFspMsi C:\path\winfsp-2.1.25156.msi
param(
    # The WinFsp installer (https://github.com/winfsp/winfsp/releases). Only
    # needed when WinFsp is not installed yet.
    [string]$WinFspMsi = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$taskName = "ConduitShares"

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script as administrator."
}

function Test-WinFsp {
    foreach ($k in @("HKLM:\SOFTWARE\WOW6432Node\WinFsp", "HKLM:\SOFTWARE\WinFsp")) {
        # Strict mode: a missing key or value is an error, so read it guarded.
        $dir = $null
        try { $dir = Get-ItemPropertyValue -LiteralPath $k -Name InstallDir -ErrorAction Stop } catch { }
        if ($dir -and (Test-Path -LiteralPath (Join-Path $dir "bin\winfsp-x64.dll"))) { return $true }
    }
    return $false
}

if (Test-WinFsp) {
    Write-Host "WinFsp is installed."
} elseif ($WinFspMsi -and (Test-Path -LiteralPath $WinFspMsi -PathType Leaf)) {
    Write-Host "Installing WinFsp from $WinFspMsi ..."
    $p = Start-Process -FilePath "msiexec.exe" -ArgumentList @("/i", "`"$WinFspMsi`"", "/qn", "/norestart") -Wait -PassThru
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw "The WinFsp installer failed (exit code $($p.ExitCode))." }
    if (-not (Test-WinFsp)) { throw "WinFsp is not installed after its installer ran." }
} else {
    throw "WinFsp is not installed and no installer was given. Download winfsp-*.msi from https://github.com/winfsp/winfsp/releases and pass it as -WinFspMsi."
}

# The stock VirtIO-FS service without a tag takes whichever device comes
# first, which may be one of ours or the NVIDIA share. Leave it to those who
# started it by hand; do not start it at boot.
$svc = Get-CimInstance Win32_Service -Filter "Name='VirtioFsSvc'" -ErrorAction SilentlyContinue
if ($svc -and $svc.StartMode -eq "Auto" -and $svc.PathName -notmatch '\s-t\s') {
    # Remembered so Uninstall-Helios.ps1 can put it back.
    $conduitKey = "HKLM:\SOFTWARE\Conduit"
    if (-not (Test-Path -LiteralPath $conduitKey)) { New-Item -Path $conduitKey -Force | Out-Null }
    New-ItemProperty -LiteralPath $conduitKey -Name "VirtioFsSvcStartMode" -Value "Automatic" -PropertyType String -Force | Out-Null
    Set-Service -Name "VirtioFsSvc" -StartupType Manual
    Write-Host "VirtioFsSvc (no tag) is set to start manually; Conduit mounts shares itself."
}

$dir = Join-Path $env:ProgramFiles "Conduit"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$script = Join-Path $dir "Mount-ConduitShares.ps1"
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "Mount-ConduitShares.ps1") -Destination $script -Force

$powershell = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
$action = New-ScheduledTaskAction -Execute $powershell -Argument (
    "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$script`""
)
$trigger = New-ScheduledTaskTrigger -AtStartup
$taskPrincipal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -RestartCount 99 -RestartInterval (New-TimeSpan -Minutes 1)
Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $taskPrincipal `
    -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
Write-Host "Shared folders: the '$taskName' task mounts conduit-* shares as drives (Z: downward) at boot and every few seconds after."
Write-Host "Log: $(Join-Path $env:ProgramData 'Conduit\shares.log')"

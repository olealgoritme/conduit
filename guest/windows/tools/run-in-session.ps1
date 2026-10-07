# Runs a command line in the interactive desktop session of the logged-on user
# (a DXGI swap chain cannot be created from an ssh session's invisible desktop:
# DXGI_ERROR_NOT_CURRENTLY_AVAILABLE), waits for it, and prints its output.
# Same transient-scheduled-task approach as guest/nvk-rm/windows/heaven-nvk-fps.ps1.
#
#   run-in-session.ps1 -Dir C:\Users\Public\s3\app -Command "d3d11_spin.exe 10 1920 1080" `
#       [-Env @{ HELIOS_ICD = "nvk" }] [-TimeoutSec 120] [-User NAME]
param(
    [Parameter(Mandatory)][string]$Dir,
    [Parameter(Mandatory)][string]$Command,
    [hashtable]$Env = @{},
    [int]$TimeoutSec = 120,
    [string]$User = $env:USERNAME,
    # One transient task per caller, so concurrent callers do not replace each other.
    [string]$Task = "ConduitSession$PID"
)
$ErrorActionPreference = "Stop"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$log = Join-Path $Dir "session-$stamp.log"
$done = Join-Path $Dir "session-$stamp.done"
$cmd = Join-Path $Dir "session-$stamp.cmd"
$lines = @("@echo off", "cd /d `"$Dir`"")
foreach ($k in $Env.Keys) { $lines += "set $k=$($Env[$k])" }
$lines += "$Command > `"$log`" 2>&1"
$lines += "echo %ERRORLEVEL% > `"$done`""
Set-Content -LiteralPath $cmd -Value $lines -Encoding ASCII
$task = $Task
schtasks /create /f /tn $task /tr "`"$cmd`"" /sc once /st 23:59 /it /ru $User | Out-Null
schtasks /run /tn $task | Out-Null
$deadline = (Get-Date).AddSeconds($TimeoutSec)
while (-not (Test-Path -LiteralPath $done) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
schtasks /delete /f /tn $task | Out-Null
if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log }
if (Test-Path -LiteralPath $done) { "exit " + (Get-Content -LiteralPath $done).Trim() } else { "TIMEOUT after $TimeoutSec s" }
Remove-Item -LiteralPath $cmd, $done -Force -ErrorAction SilentlyContinue

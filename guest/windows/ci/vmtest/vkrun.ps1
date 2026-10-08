# vkrun.ps1: run an exe in the interactive user session with given env vars, wait, and report
# which Vulkan driver it got (loader debug output, loaded ICD modules, Mesa log, vkframes files).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Public\t\vkrun.ps1 `
#       -Exe C:\path\app.exe [-Dir C:\path] [-ArgLine "a b c"] [-EnvList "K=V;K=V"] [-TimeoutSec 600] [-Out dir] [-NoKill]
#       [-Elevated | -NotElevated]
#
# Elevation: an exe whose manifest says requireAdministrator (Basemark GPU) cannot be started from a
# limited token without UAC, and a UAC-elevated child gets a fresh environment block from the registry,
# not its parent's: the env vars would be lost. So such an exe runs from an elevated task (RunLevel
# Highest), whose Start-Process creates it directly with the env and the redirection intact. The
# manifest is checked automatically; -Elevated / -NotElevated override.
#
# The launcher (this script over ssh) writes cfg.json into the output folder and starts a transient
# scheduled task (interactive logon of the current user) that runs this script again with -Inner.
# The inner part sets the env vars, starts the exe with stdout/stderr redirected to files, records the
# Vulkan driver DLLs loaded in the process, waits (kills it after -TimeoutSec unless -NoKill), and
# writes done.txt. Added to the env unless given: VK_LOADER_DEBUG=driver, MESA_LOG_FILE=<out>\mesa.log,
# MESA_LOG_LEVEL=info. Windows PowerShell 5.1.
param(
    [string]$Exe = '',
    [string]$Dir = '',
    [string]$ArgLine = '',
    [string]$EnvList = '',
    [int]$TimeoutSec = 600,
    [string]$Out = '',
    [switch]$NoKill,
    [switch]$Elevated,
    [switch]$NotElevated,
    [switch]$Inner,
    [string]$Cfg = ''
)
$ErrorActionPreference = 'Continue'

function Head([string]$path, [int]$n) {
    if (Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path -TotalCount $n -EA 0 } else { "  (no $path)" }
}

# ---------------------------------------------------------------- inner: runs in the user session
if ($Inner) {
    $c = Get-Content -LiteralPath $Cfg -Raw | ConvertFrom-Json
    $log = Join-Path $c.Out 'inner.txt'
    function L([string]$s) { Add-Content -LiteralPath $log -Value ("{0:HH:mm:ss.fff} {1}" -f (Get-Date), $s) }
    try {
        $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        L "inner: user=$env:USERDOMAIN\$env:USERNAME session=$((Get-Process -Id $PID).SessionId) elevated=$admin TEMP=$env:TEMP"
        foreach ($kv in $c.Env) {
            $i = $kv.IndexOf('='); if ($i -lt 1) { continue }
            $k = $kv.Substring(0, $i); $v = $kv.Substring($i + 1)
            Set-Item -Path "Env:$k" -Value $v
            L "env $k=$v"
        }
        $sp = @{ FilePath = $c.Exe; PassThru = $true; NoNewWindow = $true
                 RedirectStandardOutput = (Join-Path $c.Out 'stdout.txt')
                 RedirectStandardError  = (Join-Path $c.Out 'stderr.txt') }
        if ($c.Dir) { $sp.WorkingDirectory = $c.Dir }
        if ($c.Args) { $sp.ArgumentList = $c.Args }
        $p = Start-Process @sp
        $null = $p.Handle   # keeps ExitCode readable after exit
        L "started pid=$($p.Id) exe=$($c.Exe) args=$($c.Args) dir=$($c.Dir)"

        $seen = @{}
        $deadline = (Get-Date).AddSeconds([int]$c.TimeoutSec)
        while (-not $p.HasExited -and (Get-Date) -lt $deadline) {
            try {
                Get-Process -Id $p.Id -Module -EA Stop |
                    Where-Object { $_.ModuleName -match '^(vulkan|helios|librmclient|nvoglv|amdvlk|igvk)' } |
                    ForEach-Object {
                        if (-not $seen.ContainsKey($_.FileName)) {
                            $seen[$_.FileName] = 1
                            L "module $($_.FileName)"
                        }
                    }
            } catch { }
            Start-Sleep -Milliseconds 500
        }
        if (-not $p.HasExited) {
            if ($c.NoKill) { L "timeout after $($c.TimeoutSec) s, left running (-NoKill)" }
            else { L "timeout after $($c.TimeoutSec) s, killing"; Stop-Process -Id $p.Id -Force -EA 0; Start-Sleep 2 }
        }
        $p.Refresh()
        if ($p.HasExited) { L "exit code $($p.ExitCode)" }
    } catch {
        L "inner error: $($_.Exception.Message)"
    } finally {
        Set-Content -LiteralPath (Join-Path $c.Out 'done.txt') -Value (Get-Date -Format o)
    }
    exit 0
}

# ---------------------------------------------------------------- launcher: runs over ssh
if (-not $Exe) { "vkrun: -Exe is required"; exit 1 }
if (-not (Test-Path -LiteralPath $Exe)) { "vkrun: exe not found: $Exe"; exit 1 }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $Out) { $Out = "C:\Users\Public\t\vkrun-$stamp" }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
# everyone may write: the session task and the ssh account can differ
& icacls $Out /grant '*S-1-5-32-545:(OI)(CI)M' /q | Out-Null

$envs = @()
foreach ($kv in ($EnvList -split ';')) { if ($kv.Trim()) { $envs += $kv.Trim() } }
$names = $envs | ForEach-Object { ($_ -split '=', 2)[0].ToUpper() }
if ($names -notcontains 'VK_LOADER_DEBUG') { $envs += 'VK_LOADER_DEBUG=driver' }
if ($names -notcontains 'MESA_LOG_FILE')   { $envs += "MESA_LOG_FILE=$Out\mesa.log" }
if ($names -notcontains 'MESA_LOG_LEVEL')  { $envs += 'MESA_LOG_LEVEL=info' }

# requireAdministrator in the exe's manifest (an ASCII/UTF-8 resource)
$elev = $false
if ($Elevated) { $elev = $true }
elseif (-not $NotElevated) {
    try {
        $bytes = [IO.File]::ReadAllBytes($Exe)
        $elev = [Text.Encoding]::ASCII.GetString($bytes).IndexOf('requireAdministrator') -ge 0
    } catch { }
}
$runLevel = if ($elev) { 'Highest' } else { 'Limited' }

$cfgPath = Join-Path $Out 'cfg.json'
@{ Exe = $Exe; Dir = $Dir; Args = $ArgLine; Env = $envs; TimeoutSec = $TimeoutSec; Out = $Out; NoKill = [bool]$NoKill } |
    ConvertTo-Json | Set-Content -LiteralPath $cfgPath -Encoding UTF8
$self = $MyInvocation.MyCommand.Path
$t0 = Get-Date

$task = "VkRun$PID"
$act = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$self`" -Inner -Cfg `"$cfgPath`""
$prin = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel $runLevel
$set = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Seconds ($TimeoutSec + 300))
Register-ScheduledTask -TaskName $task -Action $act -Principal $prin -Settings $set -Force | Out-Null
Start-ScheduledTask -TaskName $task
"vkrun: task $task started (RunLevel $runLevel), out=$Out"

$done = Join-Path $Out 'done.txt'
$deadline = (Get-Date).AddSeconds($TimeoutSec + 90)
while (-not (Test-Path -LiteralPath $done) -and (Get-Date) -lt $deadline) { Start-Sleep 2 }
$info = Get-ScheduledTaskInfo -TaskName $task -EA 0
Unregister-ScheduledTask -TaskName $task -Confirm:$false -EA 0
if (-not (Test-Path -LiteralPath $done)) { "vkrun: inner part did not finish (task last result $($info.LastTaskResult))" }

"=== inner log"
Head (Join-Path $Out 'inner.txt') 200

"=== loader: vulkan-1.dll"
Get-Item C:\Windows\System32\vulkan-1.dll -EA 0 | ForEach-Object { "  $($_.FullName) $($_.VersionInfo.FileVersion)" }
"=== ICD registrations"
Get-Item 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers' -EA 0 | Select-Object -ExpandProperty Property | ForEach-Object {
    $ex = if (Test-Path -LiteralPath $_) { 'exists' } else { 'MISSING' }; "  Khronos\Drivers: $_ ($ex)" }
Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}' -EA 0 |
    ForEach-Object { $p = Get-ItemProperty $_.PSPath -EA 0
        foreach ($n in 'VulkanDriverName', 'VulkanDriverNameWow') { if ($p.$n) { "  class $($_.PSChildName) $n = $($p.$n -join ', ')" } } }

"=== loader driver selection (stderr, VK_LOADER_DEBUG)"
$err = Join-Path $Out 'stderr.txt'
if (Test-Path -LiteralPath $err) {
    $sel = Select-String -LiteralPath $err -Pattern 'driver|icd|\.json|vulkan_nouveau|virtio|venus|helios|select|filter' -EA 0 |
        Select-Object -First 60 | ForEach-Object { '  ' + $_.Line }
    if ($sel) { $sel } else { '  (no loader lines; first 20 lines of stderr:)'; Head $err 20 }
} else { '  (no stderr.txt)' }

"=== mesa log (first 30 lines)"
Head (Join-Path $Out 'mesa.log') 30

"=== vkframes files since start"
$dirs = @('C:\ProgramData\Helios') + (Get-ChildItem 'C:\Users\*\AppData\Local\Temp' -Directory -EA 0 | ForEach-Object FullName) + @('C:\Windows\Temp')
$found = Get-ChildItem -Path $dirs -Filter 'vkframes-*' -EA 0 | Where-Object { $_.LastWriteTime -ge $t0.AddSeconds(-5) }
if (-not $found) { '  none' }
foreach ($f in $found) {
    "  $($f.FullName) $($f.Length) bytes, $((Get-Content -LiteralPath $f.FullName -EA 0 | Measure-Object -Line).Lines) lines"
    if ($f.Extension -eq '.log') { Get-Content -LiteralPath $f.FullName -TotalCount 30 | ForEach-Object { "    $_" } }
    else { Get-Content -LiteralPath $f.FullName -TotalCount 3 | ForEach-Object { "    $_" } }
}
"=== stdout (first 10 lines)"
Head (Join-Path $Out 'stdout.txt') 10
"vkrun: saved $Out"

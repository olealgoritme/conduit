# Live kernel stacks without debug mode: LiveKD native live dump (-ml) + kd batch analysis.
# Usage (over SSH): powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Public\kdumps\Get-KStacks.ps1 [-KeepDump] [-Full]
param([switch]$KeepDump, [switch]$Full, [int]$KdTimeoutSec = 900)
$ErrorActionPreference = 'Stop'
$dir   = 'C:\Users\Public\kdumps'
$dbg   = 'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64'
$ts    = Get-Date -Format 'yyyyMMdd-HHmmss'
$dmp   = if ($KeepDump) { "W:\kdumps\live-$ts.dmp" } else { "W:\kdumps\live-tmp.dmp" }
$out   = "$dir\kstacks-$ts.txt"
$cmds  = "$dir\kstacks-cmds.txt"
New-Item -ItemType Directory -Path $dir, W:\kdumps -ErrorAction SilentlyContinue | Out-Null
# KMD pdbs: newest package build dirs that contain helios_kmd_render.pdb (kd skips GUID mismatches)
# Newest first; out\Release of every package tree plus cargo target dirs (depth-limited).
$pdbDirs = @(Get-ChildItem W:\*\out\Release\helios_kmd_render.pdb -ErrorAction SilentlyContinue) +
           @(Get-ChildItem W:\ -Directory -ErrorAction SilentlyContinue | ForEach-Object {
               Get-ChildItem (Join-Path $_.FullName 'target') -Recurse -Depth 4 -Filter helios_kmd_render.pdb -ErrorAction SilentlyContinue }) |
           Sort-Object LastWriteTime -Descending | ForEach-Object { $_.DirectoryName } | Select-Object -Unique -First 24
$sym = (@($pdbDirs) + 'srv*C:\symbols*https://msdl.microsoft.com/download/symbols') -join ';'
$c = @(
  '.echo ===== vertarget', 'vertarget',
  '.echo ===== helios module', '.reload /f helios_kmd_render.sys', 'lmvm helios_kmd_render', '!lmi helios_kmd_render',
  '.echo ===== !process 0 0', '!process 0 0',
  '.echo ===== !stacks 2 helios_kmd_render', '!stacks 2 helios_kmd_render',
  '.echo ===== !stacks 2 dxgkrnl', '!stacks 2 dxgkrnl',
  '.echo ===== dwm.exe threads (!process 0 7 dwm.exe)', '!process 0 7 dwm.exe'
)
if ($Full) { $c += '.echo ===== System process (!process 4 7)', '!process 4 7' }
$c += '.echo ===== end'
$c | Set-Content -Encoding ascii $cmds
$t0 = Get-Date
Remove-Item $dmp -ErrorAction SilentlyContinue
& "$PSScriptRoot\livekd64.exe" -accepteula -k "$dbg\kd.exe" -ml -o $dmp 2>&1 | Out-String | Write-Host
if (-not (Test-Path $dmp)) { throw "livekd produced no dump at $dmp" }
$t1 = Get-Date
$p = Start-Process -FilePath "$dbg\kd.exe" -PassThru -NoNewWindow -RedirectStandardOutput "$out.kdstdout" `
     -ArgumentList @('-z', $dmp, '-y', "`"$sym`"", '-logo', $out, '-c', "`"`$`$<$cmds;q`"")
if (-not $p.WaitForExit($KdTimeoutSec * 1000)) { $p.Kill(); $KeepDump = $true; Write-Host "kd timed out after $KdTimeoutSec s; dump kept at $dmp" }
$t2 = Get-Date
"dump: {0:N0} MB in {1:N1} s; kd: {2:N1} s; output: {3}" -f ((Get-Item $dmp).Length/1MB), ($t1-$t0).TotalSeconds, ($t2-$t1).TotalSeconds, $out
if (-not $KeepDump) { Remove-Item $dmp -ErrorAction SilentlyContinue }

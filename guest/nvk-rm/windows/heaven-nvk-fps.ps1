# Heaven on DXVK -> NVK -> RM for a timed pass; prints fps and frame times.
#
#   heaven-nvk-fps.ps1 [-Seconds 30] [-Warmup 25] [-Width 1600] [-Height 900]
#                      [-Dir C:\Users\Public\heaven-nvk]
#
# Starts run-heaven-nvk.bat in the logged-in user's desktop session (scheduled
# task, like C:\Users\Public\heaven-fps.ps1 does for the Venus path), waits
# -Warmup s, measures -Seconds s, then stops Heaven by PID.
#
# Frame times come from the vulkan-1.dll shim (NVK_SHIM_FRAMES: one line per
# vkQueuePresentKHR): Heaven's dxgi.dll is DXVK's here, which emits no DXGI ETW
# events, so PresentMon (used for the Venus path, where the system DXGI runs
# on top of the Helios UMD) does not see these presents. PresentMon is still
# run alongside and its count printed, for reference.
param([int]$Seconds = 30, [int]$Warmup = 25, [int]$Width = 1600, [int]$Height = 900,
      [string]$Dir = "C:\Users\Public\heaven-nvk", [switch]$Prof)

$busy = Get-Process -EA 0 | Where-Object { $_.Name -match '^(crm_|vk_|Heaven$)' }
if ($busy) { "busy: " + (($busy | ForEach-Object { "$($_.Name)($($_.Id))" }) -join ' '); exit 2 }

$logs = Join-Path $Dir "logs"
New-Item -ItemType Directory -Force $logs | Out-Null
Remove-Item (Join-Path $logs "*") -EA 0
$frames = Join-Path $logs "frames.csv"

$tr = "cmd /c $Dir\run-heaven-nvk.bat $Dir $Width $Height" + $(if ($Prof) { " prof" } else { "" })
schtasks /create /f /tn ConduitHeavenNvk /tr $tr /sc once /st 23:59 /it /ru "Ole Algoritme" | Out-Null
schtasks /run /tn ConduitHeavenNvk | Out-Null
Start-Sleep 3
schtasks /delete /f /tn ConduitHeavenNvk | Out-Null
$heaven = Get-Process Heaven -EA 0 | Where-Object { $_.Path -like "$Dir\*" } | Select-Object -First 1
if (-not $heaven) { "Heaven did not start"; Get-Content (Join-Path $logs "*.log") -EA 0; exit 1 }
"Heaven PID $($heaven.Id) ($Width x $Height)"
Start-Sleep ($Warmup - 3)
if ($heaven.HasExited) { "Heaven exited during warm-up (code $($heaven.ExitCode))"; exit 1 }

$pm = Get-ChildItem W:\tools\PresentMon -Filter "PresentMon*.exe" -EA 0 | Select-Object -First 1
$pmcsv = Join-Path $logs "presentmon.csv"
$lines0 = (Get-Content $frames -EA 0).Count
$profcsv = Join-Path $logs "rm-prof.csv"
if ($Prof) { Copy-Item $profcsv (Join-Path $logs "rm-prof-start.csv") -EA 0 }
if ($pm) {
  & $pm.FullName --process_id $heaven.Id --output_file $pmcsv --timed $Seconds --terminate_after_timed --no_console_stats 2>&1 | Out-Null
} else { Start-Sleep $Seconds }
$alive = -not $heaven.HasExited
if ($Prof) { Start-Sleep 2; Copy-Item $profcsv (Join-Path $logs "rm-prof-end.csv") -EA 0 }
$lines1 = (Get-Content $frames -EA 0).Count
Stop-Process -Id $heaven.Id -Force -EA 0
Start-Sleep 1
if (-not $alive) { "Heaven exited during the pass (code $($heaven.ExitCode))" }

$rows = Import-Csv $frames -EA 0
if (-not $rows -or $rows.Count -lt 10) { "no frames recorded ($frames)"; exit 1 }
$t = $rows | ForEach-Object { [double]$_.t_ms }
$tEnd = $t[-1]; $tStart = $tEnd - 1000.0 * $Seconds
$win = @($rows | Where-Object { [double]$_.t_ms -ge $tStart })
$dt = @(); for ($i = 1; $i -lt $win.Count; $i++) { $dt += [double]$win[$i].t_ms - [double]$win[$i-1].t_ms }
$dt = $dt | Sort-Object
$span = [double]$win[-1].t_ms - [double]$win[0].t_ms
$pres = ($win | ForEach-Object { [double]$_.present_ms } | Measure-Object -Average).Average
"shim: frames {0}  avg fps {1:N1}  median frame {2:N2} ms  p99 frame {3:N2} ms  max {4:N2} ms  vkQueuePresentKHR avg {5:N2} ms" -f `
  $dt.Count, (1000.0 * $dt.Count / $span), $dt[[int]($dt.Count/2)], $dt[[int]($dt.Count*0.99)], $dt[-1], $pres
# Whole run in 5 s buckets: start-up stalls versus steady state. (Before patch
# 26 NVK had no shader cache on Windows and every run compiled everything.)
$all = @($rows | ForEach-Object { [pscustomobject]@{ t = [double]$_.t_ms; p = [double]$_.present_ms } })
for ($b = 0; $b * 5000 -le $all[-1].t; $b++) {
  $d = @(); $pp = @()
  for ($i = 1; $i -lt $all.Count; $i++) {
    if ($all[$i].t -ge $b * 5000 -and $all[$i].t -lt ($b + 1) * 5000) { $d += $all[$i].t - $all[$i-1].t; $pp += $all[$i].p }
  }
  if ($d.Count -lt 2) { continue }
  $d = $d | Sort-Object
  "  {0,3}s  fps {1,6:N1}  median {2,6:N2} ms  p99 {3,7:N2} ms  present avg {4,6:N2} ms" -f ($b * 5), ($d.Count / 5.0), $d[[int]($d.Count/2)], $d[[int]($d.Count*0.99)], ($pp | Measure-Object -Average).Average
}
if ($pm -and (Test-Path $pmcsv)) { "PresentMon rows: {0}" -f ((Import-Csv $pmcsv).Count) }
if ($Prof -and (Test-Path (Join-Path $logs "rm-prof-end.csv"))) {
  # escapes during the window, per frame (frames counted the same way)
  $nf = [math]::Max(1, $lines1 - $lines0)
  $a = @{}; Get-Content (Join-Path $logs "rm-prof-start.csv") -EA 0 | Where-Object { $_ -match '^[a-z]' -and $_ -notmatch '^kind' } | ForEach-Object { $f = $_ -split ','; $a["$($f[0]) $($f[1])"] = $f }
  $t0 = [double]((Get-Content (Join-Path $logs "rm-prof-start.csv") -EA 0 | Select-Object -First 1) -replace '^# t_ms ([0-9.]+).*', '$1')
  $t1 = [double]((Get-Content (Join-Path $logs "rm-prof-end.csv") | Select-Object -First 1) -replace '^# t_ms ([0-9.]+).*', '$1')
  "RM escapes in the window ({0} frames over {1:N1} s of profile time):" -f $nf, (($t1 - $t0) / 1000)
  $tot = 0.0; $totn = 0
  Get-Content (Join-Path $logs "rm-prof-end.csv") | Where-Object { $_ -match '^[a-z]' -and $_ -notmatch '^kind' } | ForEach-Object {
    $f = $_ -split ','; $k = "$($f[0]) $($f[1])"; $n0 = 0; $u0 = 0.0
    if ($a.ContainsKey($k)) { $n0 = [double]$a[$k][2]; $u0 = [double]$a[$k][3] }
    [pscustomobject]@{ kind = $k; calls = [double]$f[2] - $n0; us = [double]$f[3] - $u0; max = [double]$f[4] }
  } | Where-Object { $_.calls -gt 0 } | Sort-Object us -Descending | ForEach-Object {
    $tot += $_.us; $totn += $_.calls
    "  {0,-22} {1,8:N0} calls {2,7:N2}/frame {3,9:N1} us/frame  mean {4,8:N1} us  max {5,9:N1} us" -f $_.kind, $_.calls, ($_.calls / $nf), ($_.us / $nf), ($_.us / $_.calls), $_.max
  }
  "  total {0:N1} escapes/frame, {1:N1} us/frame" -f ($totn / $nf), ($tot / $nf)
}
Get-ChildItem $logs | ForEach-Object { "log: $($_.Name) $($_.Length)" }

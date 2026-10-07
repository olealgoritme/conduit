# Start-up pass for shader-cache tests: launch Heaven (run-heaven-nvk.bat), record
# -Seconds from launch, kill it by PID, keep logs\frames.csv as runs\<Tag>.csv
# plus the launch-to-first-present time (runs\<Tag>.meta).
param([int]$Seconds = 45, [string]$Tag = "run", [string]$Dir = "C:\Users\Public\cache\heaven")
$busy = Get-Process -EA 0 | Where-Object { $_.Name -match '^(crm_|vk_|Heaven$)' }
if ($busy) { "busy: " + (($busy | ForEach-Object { "$($_.Name)($($_.Id)) $($_.Path)" }) -join ' '); exit 2 }
$logs = Join-Path $Dir "logs"; $runs = Join-Path $Dir "runs"
New-Item -ItemType Directory -Force $logs, $runs | Out-Null
Remove-Item (Join-Path $logs "*") -EA 0
$frames = Join-Path $logs "frames.csv"
$tr = "cmd /c $Dir\run-heaven-nvk.bat $Dir 1600 900"
schtasks /create /f /tn ConduitHeavenCache /tr $tr /sc once /st 23:59 /it /ru $env:USERNAME | Out-Null
$launch = Get-Date
schtasks /run /tn ConduitHeavenCache | Out-Null
$heaven = $null
for ($i = 0; $i -lt 100 -and -not $heaven; $i++) {
  Start-Sleep -Milliseconds 100
  $heaven = Get-Process Heaven -EA 0 | Where-Object { $_.Path -like "$Dir\*" } | Select-Object -First 1
}
schtasks /delete /f /tn ConduitHeavenCache | Out-Null
if (-not $heaven) { "Heaven did not start"; exit 1 }
$first = $null
while (((Get-Date) - $launch).TotalSeconds -lt $Seconds) {
  if (-not $first -and (Test-Path $frames) -and ((Get-Content $frames -EA 0).Count -ge 2)) { $first = ((Get-Date) - $launch).TotalMilliseconds }
  if ($heaven.HasExited) { break }
  Start-Sleep -Milliseconds 100
}
$alive = -not $heaven.HasExited
Stop-Process -Id $heaven.Id -Force -EA 0
Start-Sleep 1
"Heaven PID $($heaven.Id) alive_at_end=$alive start->Heaven.exe {0:N0} ms  launch->first present {1:N0} ms" -f (($heaven.StartTime - $launch).TotalMilliseconds), $first
Copy-Item $frames (Join-Path $runs "$Tag.csv")
"# launch_to_first_present_ms=$first" | Set-Content (Join-Path $runs "$Tag.meta")
Get-Process Heaven -EA 0 | Select-Object Id, Path | Format-Table -Auto

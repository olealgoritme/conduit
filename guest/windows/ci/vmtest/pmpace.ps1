# pmpace.ps1: frame pacing / hitch metrics from a PresentMon (v2 or v1) CSV, one line per run.
#
#   powershell -ExecutionPolicy Bypass -File pmpace.ps1 -Csv C:\Users\Public\t\g2.csv
#       [-Process Heaven.exe] [-SkipSec 0] [-Label row1]
#
# Prints:
#   PACE [label] app=X n=N med=ms p99=ms p999=ms max=ms low1=fps low01=fps hitch2x=N hitch50=N
#        cpubusy=ms gputime=ms [disp_p99=ms] src=COLUMN
#
# Frame time is the first column present of MsBetweenAppStart, FrameTime, MsBetweenPresents,
# msBetweenPresents; else MsCPUBusy + MsCPUWait; else the difference of CPUStartTime / TimeInSeconds.
# Percentiles are nearest-rank. low1 / low01 = 1000 / mean of the slowest 1 % / 0.1 % frame times
# (at least one frame). hitch2x = frames > 2 x median, hitch50 = frames > 50 ms.
# Without -Process the most frequent Application in the CSV is used. Missing columns print "na".
# Windows PowerShell 5.1 compatible; numbers are parsed and printed with the invariant culture.
param(
    [Parameter(Mandatory = $true)][string]$Csv,
    [string]$Process = '',
    [double]$SkipSec = 0,
    [string]$Label = ''
)
$inv = [System.Globalization.CultureInfo]::InvariantCulture
$pre = 'PACE'; if ($Label) { $pre = "PACE $Label" }
if (-not (Test-Path -LiteralPath $Csv)) { "$pre no csv ($Csv)"; exit 1 }
$rows = @(Import-Csv -LiteralPath $Csv)
if ($rows.Count -eq 0) { "$pre empty csv"; exit 1 }
$cols = @($rows[0].PSObject.Properties | ForEach-Object { $_.Name })

function Has([string]$c) { return ($cols -contains $c) }
function Num($s) {
    if ($null -eq $s) { return $null }
    $t = ([string]$s).Trim()
    if ($t -eq '' -or $t -eq 'NA' -or $t -eq 'N/A') { return $null }
    $d = 0.0
    if ([double]::TryParse($t, [System.Globalization.NumberStyles]::Float, $inv, [ref]$d)) { return $d }
    return $null
}
function F($v) { if ($null -eq $v) { return 'na' }; return ([double]$v).ToString('F2', $inv) }
# nearest-rank percentile of an ascending-sorted array
function Pct($s, [double]$p) {
    if ($s.Count -eq 0) { return $null }
    $i = [int][math]::Ceiling([math]::Round($p * $s.Count / 100.0, 6)) - 1
    if ($i -lt 0) { $i = 0 }; if ($i -ge $s.Count) { $i = $s.Count - 1 }
    return $s[$i]
}
function Med($vals) {
    $s = @($vals | Where-Object { $null -ne $_ } | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    return Pct $s 50
}
# 1000 / mean of the slowest pct % (at least one frame) of an ascending-sorted array
function Low($s, [double]$p) {
    if ($s.Count -eq 0) { return $null }
    $k = [int][math]::Ceiling([math]::Round($p * $s.Count / 100.0, 6)); if ($k -lt 1) { $k = 1 }
    $sum = 0.0; for ($i = $s.Count - $k; $i -lt $s.Count; $i++) { $sum += $s[$i] }
    $m = $sum / $k; if ($m -le 0) { return $null }
    return 1000.0 / $m
}

# process selection
$app = $Process
if (Has 'Application') {
    if (-not $app) {
        $g = $rows | Group-Object Application | Sort-Object Count -Descending | Select-Object -First 1
        $app = $g.Name
    }
    $rows = @($rows | Where-Object { $_.Application -eq $app })
    if ($rows.Count -eq 0) { "$pre app=$app no rows"; exit 1 }
}
if (-not $app) { $app = '?' }

# time column (seconds) for warm-up skip and the last-resort frame time
$tcol = $null
foreach ($c in @('CPUStartTime', 'TimeInSeconds')) { if (Has $c) { $tcol = $c; break } }
if ($SkipSec -gt 0 -and $tcol) {
    $t0 = $null
    foreach ($r in $rows) { $t0 = Num $r.$tcol; if ($null -ne $t0) { break } }
    if ($null -ne $t0) { $rows = @($rows | Where-Object { $v = Num $_.$tcol; ($null -ne $v) -and ($v - $t0) -ge $SkipSec }) }
}

# frame times (ms)
$ft = @(); $src = 'none'
foreach ($c in @('MsBetweenAppStart', 'FrameTime', 'MsBetweenPresents', 'msBetweenPresents')) {
    if (Has $c) { $src = $c; $ft = @($rows | ForEach-Object { Num $_.$c } | Where-Object { $null -ne $_ }); break }
}
if ($ft.Count -eq 0 -and (Has 'MsCPUBusy') -and (Has 'MsCPUWait')) {
    $src = 'MsCPUBusy+MsCPUWait'
    $ft = @($rows | ForEach-Object { $a = Num $_.MsCPUBusy; $b = Num $_.MsCPUWait; if ($null -ne $a -and $null -ne $b) { $a + $b } })
}
if ($ft.Count -eq 0 -and $tcol) {
    $src = "d($tcol)"; $prev = $null
    $ft = @(foreach ($r in $rows) { $v = Num $r.$tcol; if ($null -ne $v) { if ($null -ne $prev) { ($v - $prev) * 1000.0 }; $prev = $v } })
}
# drop the 0 that some versions write for the first frame of a swap chain
$ft = @($ft | Where-Object { $_ -gt 0 })
$s = @($ft | Sort-Object)
$n = $s.Count
if ($n -eq 0) { "$pre app=$app n=0 src=$src (no frame-time column)"; exit 1 }

$med = Pct $s 50
$h2 = @($s | Where-Object { $_ -gt 2.0 * $med }).Count
$h50 = @($s | Where-Object { $_ -gt 50.0 }).Count

$cpu = $null; foreach ($c in @('MsCPUBusy', 'CPUBusy')) { if (Has $c) { $cpu = Med @($rows | ForEach-Object { Num $_.$c }); break } }
$gpu = $null; foreach ($c in @('MsGPUTime', 'GPUTime', 'MsGPUBusy', 'msGPUActive')) { if (Has $c) { $gpu = Med @($rows | ForEach-Object { Num $_.$c }); break } }
$disp = ''
foreach ($c in @('MsBetweenDisplayChange', 'msBetweenDisplayChange')) {
    if (Has $c) {
        $d = @($rows | ForEach-Object { Num $_.$c } | Where-Object { $null -ne $_ -and $_ -gt 0 } | Sort-Object)
        if ($d.Count) { $disp = ' disp_p99=' + (F (Pct $d 99)) }
        break
    }
}

"$pre app=$app n=$n med=$(F $med) p99=$(F (Pct $s 99)) p999=$(F (Pct $s 99.9)) max=$(F $s[$n - 1]) " +
"low1=$(F (Low $s 1)) low01=$(F (Low $s 0.1)) hitch2x=$h2 hitch50=$h50 cpubusy=$(F $cpu) gputime=$(F $gpu)$disp src=$src"

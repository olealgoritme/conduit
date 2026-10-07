# vram-etw.ps1: DxgKrnl/DXGI ETW capture for the windowed (blit-model) Present,
# to see where the redirection surface lives and who holds CPU access to it.
# Run in the guest as an administrator while the windowed app runs, e.g.
#   powershell -ExecutionPolicy Bypass -File C:\Users\Public\t\vram-etw.ps1 -Seconds 3
# Output folder (-Out): vram.etl (raw, opens in WPA/GPUView), vram.xml.zip
# (tracerpt XML, what guest/windows/tools/vram_redirection_report.py reads),
# dxgkrnl-manifest.xml (the event layout of this Windows build), processes.txt,
# kmd-before.txt/kmd-after.txt (KMD registry counters), segments.txt.
# Design and the reading of the report: guest/windows/docs/vram-redirection.md.
param(
    [int]$Seconds = 3,
    [string]$Out = 'C:\Users\Public\t\vram',
    [switch]$CSwitch,      # add kernel context switches + ready-thread (who waits, how long off-CPU)
    [switch]$NoDecode      # keep only the ETL (decode later with tracerpt)
)
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force -Path $Out | Out-Null
Remove-Item "$Out\*" -Force -EA 0

$DxgGuid  = '802ec45a-1e99-4b83-9920-87c98277ba9d'   # Microsoft-Windows-DxgKrnl
$DxgiGuid = 'ca11c036-0102-4a2d-a6ad-f03cfed5d3c9'   # Microsoft-Windows-DXGI (Present start/stop: 42/43)

# 1. The provider's own manifest on this build: event ids, field order, keyword masks.
wevtutil gp Microsoft-Windows-DxgKrnl /ge:true /gm:true /f:xml > "$Out\dxgkrnl-manifest.xml" 2>$null
wevtutil gp Microsoft-Windows-DXGI /ge:true /gm:true /f:xml > "$Out\dxgi-manifest.xml" 2>$null

# 2. Keyword mask from names (masks move between builds); fallback = the 26H1 masks.
$want = @('Base','Profiler','References','Resource','Memory','Present','GPUScheduler')
$mask = [UInt64]0
try {
    [xml]$m = Get-Content "$Out\dxgkrnl-manifest.xml" -Raw
    foreach ($k in $m.SelectNodes("//*[local-name()='keyword']")) {
        if ($want -contains $k.name) {
            $mask = $mask -bor [Convert]::ToUInt64(($k.mask -replace '^0x',''), 16)
        }
    }
} catch { }
if ($mask -eq 0) { $mask = [UInt64]0x80C7 }
$hex = '0x{0:X}' -f $mask
# Rundown (capture state): adapters, segments (ReportSegment), allocations
# (AdapterAllocation/DeviceAllocation DC_Start, ReportCommittedGlobalAllocation).
$rd = '0x{0:X}' -f ($mask -band [UInt64]0xC5)
"dxgkrnl keywords=$hex rundown=$rd seconds=$Seconds cswitch=$([bool]$CSwitch)" | Tee-Object "$Out\capture.txt"

$sysKw = '<Keyword Value="ProcessThread" /><Keyword Value="Loader" />'
if ($CSwitch) { $sysKw += '<Keyword Value="CSwitch" /><Keyword Value="ReadyThread" />' }
$wprp = @"
<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<WindowsPerformanceRecorder Version="1.0" Author="conduit">
  <Profiles>
    <SystemCollector Id="SC_Vram" Name="NT Kernel Logger">
      <BufferSize Value="1024" />
      <Buffers Value="128" />
    </SystemCollector>
    <EventCollector Id="EC_Vram" Name="ConduitVramRedir">
      <BufferSize Value="1024" />
      <Buffers Value="512" />
    </EventCollector>
    <SystemProvider Id="SP_Vram">
      <Keywords>$sysKw</Keywords>
    </SystemProvider>
    <EventProvider Id="EP_DxgKrnl" Name="$DxgGuid" Level="5">
      <Keywords><Keyword Value="$hex" /></Keywords>
      <CaptureStateOnStart><Keyword Value="$rd" /></CaptureStateOnStart>
      <CaptureStateOnSave><Keyword Value="$rd" /></CaptureStateOnSave>
    </EventProvider>
    <EventProvider Id="EP_Dxgi" Name="$DxgiGuid" Level="5">
      <Keywords><Keyword Value="0xFFFFFFFFFFFFFFFF" /></Keywords>
    </EventProvider>
    <Profile Id="VramRedir.Verbose.File" Name="VramRedir" Description="conduit redirection-surface capture" LoggingMode="File" DetailLevel="Verbose">
      <Collectors>
        <SystemCollectorId Value="SC_Vram">
          <SystemProviderId Value="SP_Vram" />
        </SystemCollectorId>
        <EventCollectorId Value="EC_Vram">
          <EventProviders>
            <EventProviderId Value="EP_DxgKrnl" />
            <EventProviderId Value="EP_Dxgi" />
          </EventProviders>
        </EventCollectorId>
      </Collectors>
    </Profile>
  </Profiles>
</WindowsPerformanceRecorder>
"@
Set-Content -Path "$Out\vram.wprp" -Value $wprp -Encoding UTF8

# 3. KMD counters (the same registry snapshot blrow.sh takes).
function Snap($f) {
    $r = Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render -EA 0
    "uptime_ms=$([Environment]::TickCount)" | Out-File $f -Encoding ascii
    if ($r) { $r.PSObject.Properties | ? { $_.Name -notmatch '^PS' -and $_.Value -isnot [byte[]] } | % { "$($_.Name)=$($_.Value)" } | Out-File $f -Append -Encoding ascii }
}

wpr -cancel 2>$null | Out-Null
Snap "$Out\kmd-before.txt"
$t0 = Get-Date
wpr -start "$Out\vram.wprp!VramRedir" -filemode
if ($LASTEXITCODE -ne 0) { "wpr -start failed ($LASTEXITCODE)"; exit 1 }
Start-Sleep -Seconds $Seconds
wpr -stop "$Out\vram.etl" "conduit vram-redirection capture"
$rc = $LASTEXITCODE
Snap "$Out\kmd-after.txt"
"wpr stop rc=$rc wall=$([int]((Get-Date) - $t0).TotalMilliseconds) ms" | Tee-Object "$Out\capture.txt" -Append

# 4. Context the trace lacks: process names, the monitors' modes, the KMD build.
Get-Process | Sort-Object Id | % { "{0}`t{1}" -f $_.Id, $_.ProcessName } | Out-File "$Out\processes.txt" -Encoding ascii
Get-CimInstance Win32_VideoController | % { "$($_.Name) driver=$($_.DriverVersion) mode=$($_.CurrentHorizontalResolution)x$($_.CurrentVerticalResolution)@$($_.CurrentRefreshRate)" } | Out-File "$Out\segments.txt" -Encoding ascii
(Get-CimInstance Win32_OperatingSystem | % { "os=$($_.Version) build=$($_.BuildNumber)" }) | Out-File "$Out\segments.txt" -Append -Encoding ascii

if (-not $NoDecode) {
    # XML keeps the field names; the report streams it, so size is only a copy cost.
    tracerpt "$Out\vram.etl" -o "$Out\vram.xml" -of XML -lr -y 2>&1 | Select-String -Pattern 'Events Lost|Buffers Lost|Error' | % { $_.Line } | Tee-Object "$Out\capture.txt" -Append
    Compress-Archive -Path "$Out\vram.xml" -DestinationPath "$Out\vram.xml.zip" -Force
    Remove-Item "$Out\vram.xml" -EA 0
}
Get-ChildItem $Out | % { "{0,12} {1}" -f $_.Length, $_.Name }

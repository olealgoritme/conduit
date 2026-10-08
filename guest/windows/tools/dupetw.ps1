# dupetw.ps1: what happened around one dupshot run in a WPR/ETW capture, as CSV text a diff can read.
#   powershell -ExecutionPolicy Bypass -File dupetw.ps1 -Etl C:\Users\Public\t\dup-gdi.etl -DupPid 1234 -Out C:\Users\Public\t\dup-gdi
# Writes <Out>-counts.csv  (events per provider / id / task / opcode, whole trace),
#        <Out>-dup.csv     (every event of the dupshot process, plus every event whose task or
#                           opcode name mentions duplication, plus Dwm-Core / DxgKrnl events of
#                           any process within 200 ms either side of the dupshot process's events).
# -DupPid: the pid dupshot printed ("DUPSHOT pid="); 0 = find the process from its DXGI events.
param([Parameter(Mandatory)][string]$Etl, [int]$DupPid = 0, [Parameter(Mandatory)][string]$Out)
$ErrorActionPreference = 'SilentlyContinue'
$want = 'Microsoft-Windows-DxgKrnl', 'Microsoft-Windows-Dwm-Core', 'Microsoft-Windows-DXGI', 'Microsoft-Windows-Win32k', 'Microsoft-Windows-D3D10Level9', 'Microsoft-Windows-Direct3D11'
$ev = Get-WinEvent -Path $Etl -Oldest | Where-Object { $want -contains $_.ProviderName }
"events: $($ev.Count)"
$ev | Group-Object ProviderName, Id, TaskDisplayName, OpcodeDisplayName | Sort-Object Count -Descending |
    Select-Object Count, Name | Export-Csv "$Out-counts.csv" -NoTypeInformation
if ($DupPid -eq 0) {
    $DupPid = ($ev | Where-Object { $_.ProviderName -eq 'Microsoft-Windows-DXGI' } |
        Group-Object ProcessId | Sort-Object Count | Select-Object -First 1).Name
    "guessed dupshot pid: $DupPid"
}
$mine = $ev | Where-Object { $_.ProcessId -eq [int]$DupPid }
$t0 = ($mine | Select-Object -First 1).TimeCreated.AddMilliseconds(-200)
$t1 = ($mine | Select-Object -Last 1).TimeCreated.AddMilliseconds(200)
$ev | Where-Object {
    $_.ProcessId -eq [int]$DupPid -or "$($_.TaskDisplayName)$($_.OpcodeDisplayName)" -match 'Dupl' -or
    ($_.TimeCreated -ge $t0 -and $_.TimeCreated -le $t1)
} | Select-Object @{n='t';e={$_.TimeCreated.ToString('HH:mm:ss.ffffff')}}, ProviderName, Id, TaskDisplayName,
    OpcodeDisplayName, ProcessId, ThreadId, @{n='props';e={($_.Properties | ForEach-Object { $_.Value }) -join ' | '}} |
    Export-Csv "$Out-dup.csv" -NoTypeInformation
"dupshot events: $($mine.Count) window: $t0 .. $t1"

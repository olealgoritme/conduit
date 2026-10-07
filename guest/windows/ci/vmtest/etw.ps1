$k="HKCU:\Software\Microsoft\DirectX\UserGpuPreferences"; if(-not (Test-Path $k)){ New-Item -Path $k | Out-Null }; Set-ItemProperty $k -Name DirectXUserGlobalSettings -Value "SwapEffectUpgradeEnable=1;"
logman stop dxgiup -ets 2>$null | Out-Null
logman start dxgiup -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xFF -o C:\Users\Public\t\dxgiup.etl -ets | Out-Null
logman update dxgiup -p Microsoft-Windows-Direct3D11 0xFFFFFFFFFFFFFFFF 0xFF -ets 2>$null | Out-Null
logman update dxgiup -p Microsoft-Windows-DxgKrnl 0x1 0x4 -ets 2>$null | Out-Null
$c='C:\Users\Public\t\tri.cmd'; Set-Content $c -Encoding ASCII -Value @('@echo off','cd /d C:\Users\Public\t','d3d11_triangle_x86_64.exe helios blt2 8 0 0 > tri-etw.txt 2>&1')
schtasks /create /f /tn TriT /tr $c /sc once /st 23:59 /it /ru $env:USERNAME | Out-Null; schtasks /run /tn TriT | Out-Null; Start-Sleep 3; schtasks /delete /f /tn TriT | Out-Null
Remove-Item C:\Users\Public\t\tri.csv -EA 0
$p=Start-Process -FilePath C:\Users\Public\t\PresentMon.exe -ArgumentList '--process_name d3d11_triangle_x86_64.exe --output_file C:\Users\Public\t\tri.csv --timed 4 --terminate_after_timed --no_console_stats --v1_metrics --set_circular_buffer_size 8192' -PassThru -NoNewWindow; $p.WaitForExit(20000) | Out-Null
Start-Sleep 4
logman stop dxgiup -ets | Out-Null
Remove-ItemProperty $k -Name DirectXUserGlobalSettings -EA 0
tracerpt C:\Users\Public\t\dxgiup.etl -o C:\Users\Public\t\dxgiup.csv -of CSV -y | Out-Null
"etl bytes: " + (Get-Item C:\Users\Public\t\dxgiup.etl).Length
(Import-Csv C:\Users\Public\t\tri.csv | Select -ExpandProperty PresentMode -Unique) -join ","
Get-Content C:\Users\Public\t\tri-etw.txt -Tail 1

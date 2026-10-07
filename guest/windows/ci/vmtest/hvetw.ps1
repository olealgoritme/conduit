logman stop dxgiup -ets 2>$null | Out-Null; logman stop dxgiup2 -ets 2>$null | Out-Null
Remove-Item C:\Users\Public\t\hvdxgi.etl,C:\Users\Public\t\hvdxgi.csv -EA 0
logman start dxgiup -p Microsoft-Windows-DXGI 0xFFFFFFFFFFFFFFFF 0xFF -o C:\Users\Public\t\hvdxgi.etl -ets
$c='C:\Users\Public\t\hv.cmd'; Set-Content $c -Encoding ASCII -Value @('@echo off','cd /d C:\Users\Public\heaven-umd\bin','start "" Heaven.exe -video_app direct3d11 -sound_app null -data_path ../ -engine_config ../data/heaven_4.0.cfg -system_script heaven/unigine.cpp -video_mode -1 -video_width 1600 -video_height 900 -video_fullscreen 0 -video_multisample 0 -extern_define RELEASE,LANGUAGE_EN,QUALITY_MEDIUM,TESSELLATION_MODERATE')
schtasks /create /f /tn HvT /tr $c /sc once /st 23:59 /it /ru $env:USERNAME | Out-Null; schtasks /run /tn HvT | Out-Null; Start-Sleep 3; schtasks /delete /f /tn HvT | Out-Null
Start-Sleep 25
logman stop dxgiup -ets
tracerpt C:\Users\Public\t\hvdxgi.etl -o C:\Users\Public\t\hvdxgi.csv -of CSV -y | Out-Null
Select-String -Path C:\Users\Public\t\hvdxgi.csv -Pattern "WINDOWEDSWAPEFFECTUPGRADE_REASON_\w+" -AllMatches | % { $_.Matches.Value } | Sort -Unique
Select-String -Path C:\Users\Public\t\hvdxgi.csv -Pattern "DXGI_SWAP_EFFECT_\w+" | Select -First 2 | % { $l=$_.Line; $i=$l.IndexOf("DXGI_FORMAT"); $l.Substring([Math]::Max(0,$i-80), [Math]::Min(500, $l.Length-[Math]::Max(0,$i-80))) }

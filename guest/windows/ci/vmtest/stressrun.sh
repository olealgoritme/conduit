#!/bin/bash
T=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/t; H="Ole Algoritme@127.0.0.1"
K='$r=Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render; ($r.PSObject.Properties | ? { $_.Name -match "^(Gb|BltMirrorN|BltAsyncFa|PgSe|PgInvOvf|PrDdiBlt)" } | % { "$($_.Name)=$($_.Value)" }) -join " "'
scp -O -q -F $T/sshcfg $T/stress.ps1 "win11g:C:/Users/Public/t/stress.ps1"
echo "before: $(timeout 20 ssh -p 2222 "$H" "$K" | tr -d '\r')"
timeout 700 ssh -p 2222 "$H" 'powershell -ExecutionPolicy Bypass -File C:\Users\Public\lvl5\run-in-session.ps1 -Dir C:\Users\Public\lvl5 -Command "powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Users\Public\t\stress.ps1 -Sec 600" -TimeoutSec 660' | tr -d '\r' | tail -3
echo "after: $(timeout 20 ssh -p 2222 "$H" "$K" | tr -d '\r')"
timeout 20 ssh -p 2222 "$H" 'Get-Content C:\Users\Public\t\stress.log -Tail 4; $d=Get-Process dwm; "dwm=$($d.Id) resp=$($d.Responding) heaven=$((Get-Process Heaven -EA 0).Responding)"' | tr -d '\r'

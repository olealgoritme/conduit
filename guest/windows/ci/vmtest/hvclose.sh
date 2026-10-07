#!/bin/bash
timeout 60 ssh -p 2222 "${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}" '$h=Get-Process Heaven -EA 0 | Select -First 1; if(-not $h){"no heaven"; exit}; powershell -ExecutionPolicy Bypass -File C:\Users\Public\s315\app\inclose.ps1 -TargetPid $h.Id 2>&1 | Select -Last 2; Start-Sleep 6; Get-Process Heaven -EA 0 | % { "still running $($_.Id)" }' | tr -d '\r'

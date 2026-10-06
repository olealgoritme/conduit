#!/bin/bash
# install.sh oemNN.inf : install staged package, restart DWM + shell, verify
INF=$1
timeout 240 ssh -o ServerAliveInterval=30 -p 2222 "Ole Algoritme@127.0.0.1" "
\$K='HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
pnputil /add-driver C:\\Windows\\INF\\$INF /install | Select-String 'installed on|fail' | % Line
Start-Sleep 15
\$o=(Get-Process dwm).Id; Stop-Process -Id \$o -Force; Start-Sleep 12
foreach(\$n in 'ShellExperienceHost','SearchHost','StartMenuExperienceHost','TextInputHost','explorer'){ Get-Process \$n -EA 0 | % { Stop-Process -Id \$_.Id -Force } }
Start-Sleep 10
if(-not (Get-Process explorer -EA 0)){ schtasks /create /f /tn ConduitExp /tr explorer.exe /sc once /st 23:59 /it /ru 'Ole Algoritme' | Out-Null; schtasks /run /tn ConduitExp | Out-Null; Start-Sleep 6; schtasks /delete /f /tn ConduitExp | Out-Null }
\$v=gcim Win32_VideoController | ? Name -match 'Conduit Helios'; \$d=Get-Process dwm; \$r=Get-ItemProperty \$K
\"driver=\$(\$v.DriverVersion) \$(\$v.CurrentHorizontalResolution)x\$(\$v.CurrentVerticalResolution)@\$(\$v.CurrentRefreshRate) dwm=\$(\$d.Id) resp=\$(\$d.Responding) FfKnob=\$(\$r.FfKnob) FaKnob=\$(\$r.FaKnob) InitStg=\$(\$r.InitStg) EscWaitMsEff=\$(\$r.EscWaitMsEff)\"
" 2>&1 | tr -d '\r'

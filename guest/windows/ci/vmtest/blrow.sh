#!/bin/bash
# blrow.sh LABEL "ENV" [Knob=Val ...]: set Blt knobs, restart device, windowed Heaven composed, PresentMon 10 s, fps shot, Blt counters
L=$1; E=$2; shift 2; H="Ole Algoritme@127.0.0.1"; T=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/t; W=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/win
SETS=""; for kv in "$@"; do k=${kv%%=*}; v=${kv#*=}; SETS="$SETS Set-ItemProperty \$K -Name $k -Value $v -Type DWord;"; done
timeout 120 ssh -p 2222 "$H" "\$K='HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'; foreach(\$n in 'BltAsync','BltNoMirror','BltLookahead','ForeignCopy'){ Remove-ItemProperty \$K -Name \$n -EA 0 }; $SETS \$id=(Get-PnpDevice -Class Display -Status OK | ? Service -eq helios_kmd_render).InstanceId; pnputil /restart-device \"\$id\" | Select-String 'restarted|fail' | % Line; Start-Sleep 15; \$o=(Get-Process dwm).Id; Stop-Process -Id \$o -Force; Start-Sleep 12; foreach(\$n in 'ShellExperienceHost','SearchHost','StartMenuExperienceHost','TextInputHost','explorer'){ Get-Process \$n -EA 0 | % { Stop-Process -Id \$_.Id -Force } }; Start-Sleep 8; if(-not (Get-Process explorer -EA 0)){ schtasks /create /f /tn ConduitExp /tr explorer.exe /sc once /st 23:59 /it /ru 'Ole Algoritme' | Out-Null; schtasks /run /tn ConduitExp | Out-Null; Start-Sleep 6; schtasks /delete /f /tn ConduitExp | Out-Null }; \$r=Get-ItemProperty \$K; \"BltAsyncKnob=\$(\$r.BltAsyncKnob) BltNoMirKnob=\$(\$r.BltNoMirKnob) BltLookKnob=\$(\$r.BltLookKnob) dwm=\$((Get-Process dwm).Id)\"" | tr -d '\r'
K='$r=Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render; "uptime_ms=$([Environment]::TickCount)"; $r.PSObject.Properties | ? { $_.Name -notmatch "^PS" } | % { "$($_.Name)=$($_.Value)" }'
bash $T/hvwin.sh direct3d11 "$E" >/dev/null; sleep 25
timeout 20 ssh -p 2222 "$H" "$K" | tr -d '\r' > $W/bl-$L-kmd1.txt
timeout 40 ssh -p 2222 "$H" 'Remove-Item C:\Users\Public\t\bl.csv -EA 0; $p=Start-Process -FilePath C:\Users\Public\t\PresentMon.exe -ArgumentList "--process_name Heaven.exe --output_file C:\Users\Public\t\bl.csv --timed 10 --terminate_after_timed --no_console_stats --v1_metrics --stop_existing_session" -PassThru -NoNewWindow; $p.WaitForExit(30000) | Out-Null' >/dev/null
timeout 20 ssh -p 2222 "$H" "$K" | tr -d '\r' > $W/bl-$L-kmd2.txt
timeout 90 ssh -p 2222 "$H" 'powershell -ExecutionPolicy Bypass -File C:\Users\Public\lvl5\run-in-session.ps1 -Dir C:\Users\Public\lvl5 -Command "powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\Users\Public\t\shot.ps1" -TimeoutSec 60' >/dev/null
scp -O -q -F $T/sshcfg "win11g:C:/Users/Public/t/shot.png" $W/bl-$L.png; scp -O -q -F $T/sshcfg "win11g:C:/Users/Public/t/bl.csv" $W/bl-$L.csv
python3 -c "from PIL import Image; Image.open('$W/bl-$L.png').crop((1760,282,1866,308)).resize((424,104)).save('$W/bl-$L-fps.png')"
bash $T/hvclose.sh >/dev/null
python3 - $W $L <<'PY'
import csv,sys
w,l=sys.argv[1:]
try:
  r=[x for x in csv.DictReader(open(f'{w}/bl-{l}.csv',encoding='utf-8-sig')) if x['Application'].lower()=='heaven.exe']
  g=lambda c: sorted(float(x[c]) for x in r)
  b,i=g('msBetweenPresents'),g('msInPresentAPI'); n=len(b)
  print(f"PM {n/10:.1f}/s between p50={b[n//2]:.2f} inPresent p50={i[n//2]:.2f} p99={i[int(n*.99)-1]:.2f} mode={set(x['PresentMode'] for x in r)}")
except Exception as e: print("PM failed", e)
k=[dict(x.rstrip().split('=',1) for x in open(f'{w}/bl-{l}-kmd{j}.txt') if '=' in x) for j in (1,2)]
def d(n):
  try: return int(k[1][n])-int(k[0][n])
  except: return k[1].get(n)
print(' '.join(f"{n}={d(n)}" for n in 'BltWaitN BltWaitUs BltAsyncN BltAsyncDir BltAsyncDefer BltAsyncFall BltAsyncFail BltMirrorN BltMirrorSk BltNoMirInv BltSrcBusy BltEntrySeen BltEntryOk PrDdiBltN PrDdiBltUs PrDdiFlipN PrDdiFlipUs'.split()), 'BltAsyncWhy=',k[1].get('BltAsyncWhy'))
PY

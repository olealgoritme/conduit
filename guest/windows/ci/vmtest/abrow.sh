#!/bin/bash
# abrow.sh LABEL DWM(venus|nvk) [Knob=Val ...] : set knobs, restart device + fresh DWM, run T6, print flip-latency deltas.
L=$1; D=$2; shift 2; H="${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}"; T=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/t
SETS=""; for kv in "$@"; do k=${kv%%=*}; v=${kv#*=}; SETS="$SETS Set-ItemProperty \$K -Name $k -Value $v -Type DWord;"; done
if [ "$D" = nvk ]; then DW='Set-ItemProperty HKLM:\SOFTWARE\Helios -Name DwmIcd -Value nvk; Set-ItemProperty $K -Name ForeignFlip -Value 1 -Type DWord;'
else DW='Set-ItemProperty HKLM:\SOFTWARE\Helios -Name DwmIcd -Value ([string]::Empty); Set-ItemProperty $K -Name ForeignFlip -Value 0 -Type DWord;'; fi
timeout 120 ssh -p 2222 "$H" "\$K='HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'; foreach(\$n in 'FlipAnnounce','FlipEarlyWake','FlipQueueN','FlipAnnForeign','FlipBusyFly','FfAsyncWin'){ Remove-ItemProperty \$K -Name \$n -EA 0 }; $DW $SETS
\$id=(Get-PnpDevice -Class Display -Status OK | ? Service -eq helios_kmd_render).InstanceId; pnputil /restart-device \"\$id\" | Select-String 'restarted|fail' | % Line; Start-Sleep 15
\$o=(Get-Process dwm).Id; Stop-Process -Id \$o -Force; Start-Sleep 12; \$d=Get-Process dwm; \$r=Get-ItemProperty \$K
\"dwm=\$(\$d.Id) resp=\$(\$d.Responding) FfKnob=\$(\$r.FfKnob) FaKnob=\$(\$r.FaKnob) FlipAnnForeign=\$(\$r.FlipAnnForeign) FfAsyncWinEff=\$(\$r.FfAsyncWin) InitStg=\$(\$r.InitStg)\"
\$l=Get-ChildItem C:\ProgramData\Helios -Filter \"umd-\$(\$d.Id)*.log\" | Select -First 1; Select-String -Path \$l.FullName -Pattern 'icd backend: (NVK on RM|Venus) for dwm' | Select -First 1 | % Line" | tr -d '\r' | cut -c1-160
bash $T/t6.sh $L venus > /dev/null 2>&1
O=$(ls -d $T/t6-$L-* | tail -1)
python3 - $O <<'PY'
import csv,sys,statistics as st,re
o=sys.argv[1]
rows=list(csv.DictReader(open(o+'/pm.csv')))
def s(c):
    v=sorted(float(r[c]) for r in rows if r.get(c) not in (None,'','NA'))
    n=len(v); return f"p50={v[n//2]:.2f} p99={v[int(n*.99)-1]:.2f} max={v[-1]:.1f}" if n else "none"
print(f"PM dwm {len(rows)/20:.0f}/s  between {s('msBetweenPresents')}  untilDisp {s('msUntilDisplayed')}")
k=[dict(l.rstrip().split('=',1) for l in open(o+f'/kmd-{i}.txt') if '=' in l) for i in (1,2)]
dt=(int(k[1]['uptime_ms'])-int(k[0]['uptime_ms']))/1000
def d(n):
    try: return int(k[1][n])-int(k[0][n])
    except: return None
for fam in ('FlipLat','IfGap','FlipHostLat','FfRttB'):
    print(f"{fam}0..7 d:", [d(f'{fam}{i}') for i in range(8)])
for n in 'FlipIss VsTickN FfProg FfFrames FfCoal FfDropped VbTicks VbUsed IfStall8 FaDdi FaNoBusy FaNoFgn FaRefuse'.split():
    print(f"  {n} d={d(n)} ({(d(n) or 0)/dt:.0f}/s)", end='')
print()
print(' ', ' '.join(f"{n}={k[1].get(n)}" for n in 'FlipP50Us FlipP99Us FlipMaxUs FlipMaxSite FlipMaxFl IfGapMax FfPinPeak MirMaxUs FfAsyncWin FaKnob'.split()))
PY
echo "saved $O"

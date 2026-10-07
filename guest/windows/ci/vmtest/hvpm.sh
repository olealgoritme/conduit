#!/bin/bash
# hvpm.sh LABEL: 15 s PresentMon on Heaven.exe + dwm.exe, UMD present-gate and Heaven fps lines
L=$1; H="${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}"; O=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/t/hv-$L-$(date +%H%M%S); mkdir -p $O
timeout 60 ssh -p 2222 "$H" 'Remove-Item C:\Users\Public\t\hv.csv -EA 0; $p=Start-Process -FilePath C:\Users\Public\t\PresentMon.exe -ArgumentList "--process_name Heaven.exe --process_name dwm.exe --output_file C:\Users\Public\t\hv.csv --timed 10 --terminate_after_timed --no_console_stats --v1_metrics --stop_existing_session" -PassThru -NoNewWindow; $p.WaitForExit(40000) | Out-Null; $h=Get-Process Heaven -EA 0 | Select -First 1; "heaven pid=$($h.Id)"; $f=Get-ChildItem C:\ProgramData\Helios -Filter "umd-$($h.Id)*.log" | Select -First 1; Select-String -Path $f.FullName -Pattern "present-gate|frames composed|scanout:|icd backend: (NVK|Venus)" | Select -Last 4 | % { $_.Line.Substring(0,[Math]::Min(160,$_.Line.Length)) }' | tr -d '\r' > $O/info.txt
timeout 30 ssh -p 2222 "$H" 'Get-Content C:\Users\Public\t\hv.csv' | tr -d '\r' > $O/pm.csv
cat $O/info.txt
python3 - $O <<'PY'
import csv,sys,statistics as st
o=sys.argv[1]; r=list(csv.DictReader(open(o+'/pm.csv')))
for app in ('Heaven.exe','dwm.exe'):
    x=[a for a in r if a['Application'].lower()==app.lower()]
    if not x: print(app,'no rows'); continue
    print(f"{app}: {len(x)/10:.1f}/s modes={set(a['PresentMode'] for a in x)} sync={set(a['SyncInterval'] for a in x)} rt={set(a['Runtime'] for a in x)}")
    for c in ('msBetweenPresents','msInPresentAPI','msUntilRenderComplete','msGPUActive','msUntilDisplayed'):
        v=sorted(float(a[c]) for a in x if a.get(c) not in (None,'','NA'))
        if v: n=len(v); print(f"   {c}: p50={v[n//2]:.2f} p99={v[int(n*.99)-1]:.2f} mean={st.mean(v):.2f}")
PY
echo "saved $O"

#!/bin/bash
# apiset.sh [LABEL] [RUN...]: per-API frame-pacing set. Each run: start the app in the user session,
# warm up, PresentMon v2 for PMSEC s on its process, pmpace.ps1 line, CSV copied to the host.
#   RUN: heaven (D3D11) | bm-dx12 | bm-vk | bm-gl | vkcube | all (default: heaven bm-dx12 bm-vk vkcube)
#   env: WIN_SSH=user@127.0.0.1 (ssh port 2222), VMTEST_DIR (host results), PMSEC (20), WARM (15),
#        APPENV="K=V K=V" (set for the app), WW/WH (1600x900, windowed), FS=1 (fullscreen),
#        BM_ROOT (guest folder holding Basemark's binaries\ and assets\, else searched), BM_PIPE (highend|medium),
#        VKCUBE (guest path of vkcube.exe, else searched), VKPM (vkcube --present_mode, default 2 = FIFO)
# Needs in the guest folder C:\Users\Public\t: PresentMon.exe (v2). pmpace.ps1 is copied there by stage().
# Summary: $OUT/apiset.txt, one PACE line per run (plus Basemark's own avg/min/max from its JSON report).
H="${WIN_SSH:?set WIN_SSH=user@127.0.0.1 (the guest account)}"
D=$(cd "$(dirname "$0")" && pwd)
G='C:\Users\Public\t'
LABEL=${1:-run}; shift 2>/dev/null
OUT=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/win/apiset-$LABEL-$(date +%m%d-%H%M%S); mkdir -p "$OUT"
PMSEC=${PMSEC:-20}; WARM=${WARM:-15}; WW=${WW:-1600}; WH=${WH:-900}
S(){ local t=$1; shift; timeout "$t" ssh -p 2222 "$H" "$@" | tr -d '\r'; }
log(){ echo "$*" | tee -a "$OUT/apiset.txt"; }

stage(){  # copy pmpace.ps1 into the guest
  timeout 60 scp -O -q -P 2222 "$D/pmpace.ps1" "$H:C:/Users/Public/t/pmpace.ps1" || { echo "stage: scp failed"; return 1; }
}
health(){ S 30 '$v=gcim Win32_VideoController | ? Name -match "Conduit Helios"; "adapter=$($v.Status) err=$($v.ConfigManagerErrorCode) dwm resp=" + (Get-Process dwm -EA 0).Responding'; }

# launch TAG WORKDIR "CMDLINE": run `start "" CMDLINE` from WORKDIR in the interactive session (hvwin.sh pattern)
launch(){
  local sets=""; for kv in $APPENV; do sets="$sets','set $kv"; done
  S 60 "\$c='$G\\as-$1.cmd'; Set-Content \$c -Encoding ASCII -Value @('@echo off$sets','cd /d \"$2\"','start \"\" $3'); schtasks /create /f /tn ApiSet /tr \$c /sc once /st 23:59 /it /ru \$env:USERNAME | Out-Null; schtasks /run /tn ApiSet | Out-Null; Start-Sleep 3; schtasks /delete /f /tn ApiSet | Out-Null"
}
alive(){ S 30 "(Get-Process '${1%.exe}' -EA 0 | Select -First 1).Id"; }   # prints the pid or nothing
kill_app(){ S 30 "Get-Process '${1%.exe}' -EA 0 | % { Stop-Process -Id \$_.Id -Force }" >/dev/null; }

# pm TAG PROCESS.exe: PresentMon v2 (default metrics, no --v1_metrics) for PMSEC s, pmpace line, CSV to $OUT/TAG/pm.csv
pm(){
  local tag=$1 p=$2 o="$OUT/$1"; mkdir -p "$o"
  S $((PMSEC + 60)) "\$f='$G\\pm-$tag.csv'; Remove-Item \$f -EA 0; \$p=Start-Process -FilePath '$G\\PresentMon.exe' -ArgumentList \"--process_name $p --output_file \$f --timed $PMSEC --terminate_after_timed --no_console_stats --stop_existing_session\" -PassThru -NoNewWindow; \$p.WaitForExit(($PMSEC + 40) * 1000) | Out-Null; powershell -NoProfile -ExecutionPolicy Bypass -File '$G\\pmpace.ps1' -Csv \$f -Process $p -Label $tag" > "$o/pace.txt"
  S 120 "Get-Content '$G\\pm-$tag.csv' -EA 0" > "$o/pm.csv"
  log "$(cat "$o/pace.txt")"
}

# --- Heaven D3D11 (windowed, via hvwin.sh) ---
heaven(){
  log "=== heaven d3d11 ${WW}x${WH} env='$APPENV'"
  WW=$WW WH=$WH bash "$D/hvwin.sh" direct3d11 "$APPENV" | tee -a "$OUT/apiset.txt"
  sleep "$WARM"; [ -n "$(alive Heaven)" ] || { log "heaven: not running"; return 1; }
  pm heaven-d3d11 Heaven.exe
  kill_app Heaven; sleep 5; log "[health] $(health)"
}

# --- Basemark GPU (d3d12 | vk | gl) ---
# Layout (PTS profile / installer): ROOT\binaries\BasemarkGPU_{dx12,vk,gl}.exe, assets in ROOT\assets\pkg (zip)
# or assets\bsb (older); run from the folder holding assets\. Prints "WORKDIR|BINDIR|ASSETPATH".
bm_find(){
  S 120 "\$r=@('${BM_ROOT:-}','C:\\Program Files\\Basemark GPU','C:\\Users\\Public\\basemark','C:\\Users\\Public\\BasemarkGPU') | ? { \$_ -and (Test-Path \$_) }; if(-not \$r){ return }; \$e=Get-ChildItem -Path \$r -Recurse -Depth 3 -Filter BasemarkGPU_vk.exe -EA 0 | Select -First 1; if(-not \$e){ return }; foreach(\$d in @(\$e.Directory.FullName, \$e.Directory.Parent.FullName)){ foreach(\$a in 'assets\\pkg','assets\\bsb'){ if(Test-Path (Join-Path \$d \$a)){ \"\$d|\$(\$e.Directory.FullName)|\$a\"; return } } }"
}
bm(){  # bm dx12|vk|gl
  local api=$1 exe="BasemarkGPU_$1.exe" tag="bm-$1" loc wd bd ap rep
  loc=$(bm_find); [ -n "$loc" ] || { log "=== $tag: Basemark GPU not found (set BM_ROOT)"; return 1; }
  IFS='|' read -r wd bd ap <<<"$loc"
  rep="$G\\bm-$api.json"; S 30 "Remove-Item '$rep' -EA 0" >/dev/null
  local fs=false; [ "${FS:-0}" = 1 ] && fs=true
  log "=== $tag ${WW}x${WH} fs=$fs pipe=${BM_PIPE:-highend} root=$wd env='$APPENV'"
  launch "$tag" "$wd" "\"$bd\\$exe\" TestType Custom RenderPipeline ${BM_PIPE:-highend} TextureCompression bc7 WindowResolution ${WW}x${WH} RenderResolution ${WW}x${WH} Fullscreen $fs BenchmarkMode true SkipZPrepass false ProgressBar false ResultUpload false AssetPath $ap ReportPath $rep"
  sleep "$WARM"; [ -n "$(alive "$exe")" ] || { log "$tag: not running after ${WARM}s"; return 1; }
  pm "$tag" "$exe"
  # Basemark ends by itself; give it up to 5 min, then kill (a hang is a result too)
  S 330 "Wait-Process -Name '${exe%.exe}' -Timeout 300 -EA 0; if(Get-Process '${exe%.exe}' -EA 0){ 'bm: still running after 300 s, killing'; Get-Process '${exe%.exe}' | Stop-Process -Force }" | tee -a "$OUT/apiset.txt"
  S 60 "Get-Content '$rep' -Raw -EA 0" > "$OUT/$tag/report.json"
  python3 - "$OUT/$tag/report.json" <<'PY' | tee -a "$OUT/apiset.txt"
import json,sys,statistics as st
try: j=json.load(open(sys.argv[1]))
except Exception as e: print("bm report: none (%s)" % e.__class__.__name__); sys.exit()
r=j.get('result',{}); ft=(j.get('frames') or {}).get('frameTimes') or []
s=f"bm report: avg={r.get('averageFPS')} min={r.get('minFPS')} max={r.get('maxFPS')}"
if ft: v=sorted(float(x) for x in ft); s+=f" frames={len(v)} ft_med={st.median(v):.3f} ft_p99={v[max(0,-(-len(v)*99//100)-1)]:.3f} ft_max={v[-1]:.3f}"
print(s)
PY
  sleep 5; log "[health] $(health)"
}

# --- vkcube (Vulkan SDK) ---
vkcube(){
  local p=${VKCUBE:-}
  [ -n "$p" ] || p=$(S 60 "(@(Get-ChildItem 'C:\\VulkanSDK\\*\\Bin\\vkcube.exe' -EA 0) + @(Get-Item '$G\\vkcube.exe' -EA 0) | Select -Last 1).FullName")
  [ -n "$p" ] || { log "=== vkcube: not found (set VKCUBE)"; return 1; }
  log "=== vkcube $p ${WW}x${WH} present_mode=${VKPM:-2} env='$APPENV'"
  launch vkcube "$G" "\"$p\" --width $WW --height $WH --present_mode ${VKPM:-2} --suppress_popups"
  sleep "$WARM"; [ -n "$(alive vkcube)" ] || { log "vkcube: not running"; return 1; }
  pm vkcube vkcube.exe
  kill_app vkcube; sleep 3; log "[health] $(health)"
}

stage || exit 1
RUNS="$*"; [ -z "$RUNS" ] || [ "$RUNS" = all ] && RUNS="heaven bm-dx12 bm-vk vkcube"
log "apiset $LABEL runs='$RUNS' pmsec=$PMSEC warm=$WARM $(date -Is)"
for r in $RUNS; do
  case $r in
    heaven) heaven ;; bm-dx12) bm dx12 ;; bm-vk) bm vk ;; bm-gl) bm gl ;; vkcube) vkcube ;;
    *) log "unknown run $r" ;;
  esac
  sleep 10
done
echo "saved $OUT"

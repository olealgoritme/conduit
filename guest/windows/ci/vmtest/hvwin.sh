#!/bin/bash
# hvwin.sh API(direct3d11|opengl) "EXTRA_ENV": Heaven windowed 1600x900 Medium/Moderate/AA off, like the user's host
A=${1:-direct3d11}; E=${2:-}; H="Ole Algoritme@127.0.0.1"
SETS=""; for kv in $E; do SETS="$SETS','set $kv"; done
[ -n "${DXCFG:-}" ] && SETS="$SETS','set DXVK_CONFIG=$DXCFG"
timeout 60 ssh -p 2222 "$H" "\$c='C:\Users\Public\t\hvw.cmd'; Set-Content \$c -Encoding ASCII -Value @('@echo off$SETS','cd /d C:\Users\Public\heaven-umd\bin','start \"\" Heaven.exe -video_app $A -sound_app null -data_path ../ -engine_config ../data/heaven_4.0.cfg -system_script heaven/unigine.cpp -video_mode -1 -video_width ${WW:-1600} -video_height ${WH:-900} -video_fullscreen 0 -video_multisample 0 -extern_define RELEASE,LANGUAGE_EN,${QUAL:-QUALITY_MEDIUM},${TESS:-TESSELLATION_MODERATE}'); schtasks /create /f /tn HvW /tr \$c /sc once /st 23:59 /it /ru 'Ole Algoritme' | Out-Null; schtasks /run /tn HvW | Out-Null; Start-Sleep 3; schtasks /delete /f /tn HvW | Out-Null; Start-Sleep 15; Get-Process Heaven -EA 0 | % { \"heaven \$(\$_.Id)\" }" | tr -d '\r'

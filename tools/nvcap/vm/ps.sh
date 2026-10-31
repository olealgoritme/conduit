#!/bin/bash
# usage: ps.sh 'powershell code'
enc=$(printf '%s' "$1" | iconv -t UTF-16LE | base64 -w0)
timeout ${T:-60} ssh -o ConnectTimeout=10 -o BatchMode=yes -p 2223 "Ole Algoritme@127.0.0.1" "powershell -NoProfile -NonInteractive -EncodedCommand $enc" 2>&1 | grep -v -E '^#< CLIXML|^<Objs'

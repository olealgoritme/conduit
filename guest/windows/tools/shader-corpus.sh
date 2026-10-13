#!/bin/bash
# Run d3d11_shader_corpus.exe in the win11 guest from the host and report.
#
#   guest/windows/tools/shader-corpus.sh [HOST_CORPUS_DIR] [-- EXTRA_ARGS...]
#
# Builds the tool if needed, copies it (and run-in-session.ps1) to
# C:\Users\Public\shadercorpus, runs the builtin cases with --dispatch in the
# interactive session, then (when HOST_CORPUS_DIR is given) a corpus of
# *.dxbc/*.cso files copied from the host as one tar. Prints the new lines of
# C:\ProgramData\Helios\shader-failures.log. Exit code: the tool's (0 = no
# UNEXPECTED failure, no dispatch mismatch).
#
# Corpora dumped from games stay outside the repository.
# Environment: WIN_SSH (required: the VM's SSH destination, as for
# ci/vm/win-build.sh), WIN_PORT (2222), TOOL_ARGS (default --builtin
# --dispatch), TIMEOUT (seconds, default 1800).
set -u
here=$(cd "$(dirname "$0")" && pwd)
target=${WIN_SSH:?set WIN_SSH to the SSH destination of the VM (user@host)}
port=${WIN_PORT:-2222}
corpus=${1:-}
[ "$corpus" = "--" ] && corpus=
shift $(( $# > 0 ? 1 : 0 ))
[ "${1:-}" = "--" ] && shift
extra=("$@")
timeout_s=${TIMEOUT:-1800}
work=${WORK:-$(mktemp -d)}
exe=$work/d3d11_shader_corpus.exe

ps() {  # ps 'powershell' [timeout]
    local enc
    enc=$(printf '%s' "$1" | iconv -t UTF-16LE | base64 -w0)
    timeout "${2:-120}" ssh -o ConnectTimeout=10 -o BatchMode=yes -p "$port" "$target" \
        "powershell -NoProfile -NonInteractive -EncodedCommand $enc" 2>&1 | grep -v -E '^#< CLIXML|^<Objs'
}
cp_to() { timeout "${3:-600}" scp -O -q -P "$port" "$1" "$target:$2"; }

x86_64-w64-mingw32-g++ -O2 -municode -static -o "$exe" "$here/d3d11_shader_corpus.cpp" \
    -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid || { echo "build failed"; exit 2; }

ps 'New-Item -ItemType Directory -Force C:\Users\Public\shadercorpus | Out-Null; "ok"' 30 >/dev/null
cp_to "$exe" 'C:/Users/Public/shadercorpus/d3d11_shader_corpus.exe' || { echo "copy failed"; exit 2; }
cp_to "$here/run-in-session.ps1" 'C:/Users/Public/shadercorpus/run-in-session.ps1' || { echo "copy failed"; exit 2; }

args="${TOOL_ARGS:---builtin --dispatch}"
if [ -n "$corpus" ]; then
    tar -C "$corpus" -cf "$work/corpus.tar" . || exit 2
    cp_to "$work/corpus.tar" 'C:/Users/Public/shadercorpus/corpus.tar' 1800 || { echo "corpus copy failed"; exit 2; }
    ps 'Set-Location C:\Users\Public\shadercorpus; Remove-Item -Recurse -Force corpus -EA 0; New-Item -ItemType Directory corpus | Out-Null; tar -xf corpus.tar -C corpus; Remove-Item corpus.tar; "unpacked"' 900
    args="$args --dir C:\\Users\\Public\\shadercorpus\\corpus --quiet"
fi
args="$args ${extra[*]:-}"

before=$(ps '$f = "C:\ProgramData\Helios\shader-failures.log"; if (Test-Path $f) { (Get-Item $f).Length } else { 0 }' 30 | tr -d '\r\n ')
ps "Set-ExecutionPolicy -Scope Process Bypass -Force; & C:\\Users\\Public\\shadercorpus\\run-in-session.ps1 -Dir C:\\Users\\Public\\shadercorpus -Command \"d3d11_shader_corpus.exe $args\" -TimeoutSec $timeout_s" $((timeout_s + 60)) | tee "$work/result.txt"
echo "--- new ledger lines (C:\\ProgramData\\Helios\\shader-failures.log)"
ps "\$f = 'C:\\ProgramData\\Helios\\shader-failures.log'; if (Test-Path \$f) { \$s = [IO.File]::Open(\$f, 'Open', 'Read', 'ReadWrite'); \$null = \$s.Seek(${before:-0}, 'Begin'); (New-Object IO.StreamReader(\$s)).ReadToEnd(); \$s.Close() }" 60
rc=$(sed -n 's/^exit \([0-9]*\).*/\1/p' "$work/result.txt" | tail -1)
echo "result in $work/result.txt"
exit "${rc:-3}"

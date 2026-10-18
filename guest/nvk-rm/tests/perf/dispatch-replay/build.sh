#!/bin/bash
# Build dispatch-replay for Linux (./dispatch-replay) and, when MinGW is installed, for Windows
# (./dispatch-replay.exe: kernel32/msvcrt only, the Vulkan loader is loaded at run time), then the guest
# package dreplay-guest.zip (the exe, run-guest.cmd and the captures in ./captures, if any).
#   build.sh [VULKAN_INCLUDE_DIR]     directory holding vulkan/ and vk_video/ (default /usr/include)
set -eu
cd "$(dirname "$0")"
inc=${1:-/usr/include}
[ -f "$inc/vulkan/vulkan.h" ] || { echo "no $inc/vulkan/vulkan.h (install the Vulkan headers)" >&2; exit 1; }
gcc -O2 -Wall -Wextra -Wno-missing-field-initializers -Wno-unused-parameter -I"$inc" -o dispatch-replay dispatch-replay.c -ldl
if ! command -v x86_64-w64-mingw32-gcc >/dev/null; then
    echo "no x86_64-w64-mingw32-gcc: Linux binary only" >&2
    ls -la dispatch-replay
    exit 0
fi
# MinGW must not see the host's /usr/include: give it the Vulkan headers alone.
win=$(mktemp -d)
trap 'rm -rf "$win"' EXIT
ln -s "$inc/vulkan" "$win/vulkan"
[ -d "$inc/vk_video" ] && ln -s "$inc/vk_video" "$win/vk_video"
x86_64-w64-mingw32-gcc -O2 -Wall -Wno-missing-field-initializers -I"$win" -o dispatch-replay.exe dispatch-replay.c
x86_64-w64-mingw32-strip dispatch-replay.exe
rm -rf guest-pkg dreplay-guest.zip
mkdir -p guest-pkg
cp dispatch-replay.exe run-guest.cmd guest-pkg/
[ -d captures ] && cp -r captures guest-pkg/
python3 -I - <<'PY'
import os, zipfile
z = zipfile.ZipFile("dreplay-guest.zip", "w", zipfile.ZIP_DEFLATED)
for d, _, fs in os.walk("guest-pkg"):
    for f in fs:
        p = os.path.join(d, f)
        z.write(p, os.path.relpath(p, "guest-pkg").replace(os.sep, "/"))
z.close()
PY
ls -la dispatch-replay dispatch-replay.exe dreplay-guest.zip

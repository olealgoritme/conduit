#!/bin/sh
# Stage an application-local "D3D11 -> DXVK -> NVK -> RM" drop-in folder:
# copy its contents next to an application's .exe and that application (only
# it) runs D3D10/11 on DXVK on NVK-on-RM, with nothing registered system-wide.
#
#   guest/nvk-rm/windows/stage-dxvk-app.sh ARCH NVK_DIST DXVK_DIR OUT_DIR
#
# ARCH      i686 (32-bit apps, e.g. Unigine Heaven) or x86_64
# NVK_DIST  build-windows.sh OUT_DIR for the same ARCH (vulkan_nouveau.dll,
#           librmclient.dll)
# DXVK_DIR  directory holding DXVK's d3d11.dll and dxgi.dll for ARCH: an
#           upstream release's x32/ or x64/, or a MinGW build of the Helios
#           fork (see README.md, "D3D11 games through DXVK")
# OUT_DIR   staging directory (created)
#
# Result: d3d11.dll dxgi.dll (DXVK), vulkan-1.dll (vulkan_shim.c),
# vulkan_nouveau.dll librmclient.dll (NVK), dxvk.conf, SHA256SUMS.
# The application still needs NVK_RM=1 in its environment (see
# run-heaven-nvk.bat).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
ARCH=$1 NVK_DIST=$2 DXVK_DIR=$3 OUT_DIR=$4
VULKAN_INCLUDE=${VULKAN_INCLUDE:-/usr/include}
case "$ARCH" in
  x86_64|i686) ;;
  *) echo "ARCH must be x86_64 or i686" >&2; exit 1 ;;
esac

mkdir -p "$OUT_DIR/.include"
# Only the Vulkan headers: -I/usr/include would pull in glibc's.
cp -r "$VULKAN_INCLUDE/vulkan" "$VULKAN_INCLUDE/vk_video" "$OUT_DIR/.include/"
"$ARCH-w64-mingw32-gcc" -O2 -Wall -shared -I"$OUT_DIR/.include" \
  -o "$OUT_DIR/vulkan-1.dll" "$here/vulkan_shim.c" "$here/vulkan_shim.def" \
  -Wl,--enable-stdcall-fixup -static-libgcc
rm -rf "$OUT_DIR/.include"
"$ARCH-w64-mingw32-strip" "$OUT_DIR/vulkan-1.dll"
cp "$NVK_DIST/vulkan_nouveau.dll" "$NVK_DIST/librmclient.dll" "$OUT_DIR/"
cp "$DXVK_DIR/d3d11.dll" "$DXVK_DIR/dxgi.dll" "$OUT_DIR/"
cp "$here/dxvk-nvk.conf" "$OUT_DIR/dxvk.conf"
(cd "$OUT_DIR" && sha256sum ./*.dll dxvk.conf > SHA256SUMS)
"$ARCH-w64-mingw32-objdump" -p "$OUT_DIR/vulkan-1.dll" | sed -n '/^\[Ordinal\/Name Pointer\] Table/,/^$/p'
ls -l "$OUT_DIR"

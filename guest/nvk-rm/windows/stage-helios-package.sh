#!/bin/sh
# Builds NVK on RM and Zink for Windows, 64-bit and 32-bit, and stages the
# files the Helios driver package installs into the driver store
# (guest/windows/kmd_render/helios_kmd_render.inx):
#
#   vulkan_nouveau.dll    librmclient.dll    helios_nvk64.json   AMD64 Vulkan ICD
#   vulkan_nouveau32.dll  librmclient32.dll  helios_nvk32.json   WoW64 Vulkan ICD
#   helios_gl64.dll       helios_gl32.dll                        Zink WGL ICDs
#
# File names are unique across the two architectures (one driver-store
# directory): the 32-bit NVK loads librmclient32.dll (patch 0032) and Zink
# finds vulkan_nouveau32.dll next to helios_gl32.dll (patch 0034).
#
#   guest/nvk-rm/windows/stage-helios-package.sh [OUT_DIR]
#
# OUT_DIR defaults to dist/nvk-windows in this checkout, where
# guest/windows/ci/vm/win-build.sh picks it up. MESA_DIR (default
# ~/code/mesa-nvk-rm-helios) is the Mesa checkout build-windows.sh applies
# the series to, with build-win and build-win32 inside it; the other
# build-windows.sh settings (MESON, MESA_CLC_DIR, JOBS, MEMORY_MAX) pass
# through.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
conduit=$(cd "$here/../../.." && pwd)
OUT_DIR=${1:-$conduit/dist/nvk-windows}
MESA_DIR=${MESA_DIR:-$HOME/code/mesa-nvk-rm-helios}

GL=1 ARCH=x86_64 OUT_DIR="$MESA_DIR/build-win/dist" \
  sh "$conduit/guest/nvk-rm/build-windows.sh" "$MESA_DIR" build-win
GL=1 ARCH=i686 OUT_DIR="$MESA_DIR/build-win32/dist" \
  sh "$conduit/guest/nvk-rm/build-windows.sh" "$MESA_DIR" build-win32

d64=$MESA_DIR/build-win/dist
d32=$MESA_DIR/build-win32/dist
mkdir -p "$OUT_DIR"
cp "$d64/vulkan_nouveau.dll" "$OUT_DIR/vulkan_nouveau.dll"
cp "$d64/librmclient.dll" "$OUT_DIR/librmclient.dll"
cp "$d64/libgallium_wgl.dll" "$OUT_DIR/helios_gl64.dll"
cp "$d32/vulkan_nouveau.dll" "$OUT_DIR/vulkan_nouveau32.dll"
cp "$d32/librmclient.dll" "$OUT_DIR/librmclient32.dll"
cp "$d32/libgallium_wgl.dll" "$OUT_DIR/helios_gl32.dll"

# ICD manifests: library_path relative to the manifest (the loader resolves
# a relative path with a separator against the manifest's directory).
python3 - "$d64/nouveau_icd.json" "$OUT_DIR" <<'EOF'
import json, os, sys
api = json.load(open(sys.argv[1]))["ICD"]["api_version"]
for arch, dll in (("64", "vulkan_nouveau.dll"), ("32", "vulkan_nouveau32.dll")):
    icd = {"file_format_version": "1.0.1",
           "ICD": {"library_path": ".\\" + dll, "library_arch": arch,
                   "api_version": api}}
    with open(os.path.join(sys.argv[2], "helios_nvk%s.json" % arch), "w") as f:
        json.dump(icd, f, indent=4)
        f.write("\n")
EOF

for f in vulkan_nouveau.dll librmclient.dll helios_gl64.dll; do
  objdump -f "$OUT_DIR/$f" | grep -q 'pei-x86-64' || { echo "$f is not x86-64" >&2; exit 1; }
done
for f in vulkan_nouveau32.dll librmclient32.dll helios_gl32.dll; do
  objdump -f "$OUT_DIR/$f" | grep -q 'pei-i386' || { echo "$f is not i386" >&2; exit 1; }
done
# NVK's Helios interface (the D3D UMD) and policy export (Zink)
for f in vulkan_nouveau.dll vulkan_nouveau32.dll; do
  for sym in helios_icd_interface_v2 nvk_helios_process_allowed vk_icdGetInstanceProcAddr; do
    objdump -p "$OUT_DIR/$f" | grep -q "\] $sym\$" || { echo "$f does not export $sym" >&2; exit 1; }
  done
done
for f in librmclient.dll librmclient32.dll; do
  objdump -p "$OUT_DIR/$f" | grep -q "\] crm_win_adapter_luid\$" ||
    { echo "$f has no crm_win_adapter_luid (the Vulkan loader matches NVK to the adapter by LUID)" >&2; exit 1; }
done
(cd "$OUT_DIR" && sha256sum ./*.dll ./*.json > SHA256SUMS)
ls -l "$OUT_DIR"
echo "stage-helios-package: $OUT_DIR"

#!/bin/sh
# Cross-build ngx-smoketest.exe on Linux (clang + lld for x86_64-w64-mingw32),
# linked with NVIDIA's static NGX loader library nvsdk_ngx_s.lib, the one
# games link.
#
#   NGX_SDK=/path/to/DLSS guest/windows/tools/ngx-smoketest/build.sh [OUT_DIR]
#
# NGX_SDK: a checkout of the DLSS SDK (https://github.com/NVIDIA/DLSS), for
#   include/ and lib/Windows_x86_64/x64/nvsdk_ngx_s.lib (no NVIDIA files are
#   in this repository). A sparse clone is enough:
#     git clone --depth 1 --filter=blob:none --sparse https://github.com/NVIDIA/DLSS.git
#     git -C DLSS sparse-checkout set include lib/Windows_x86_64
# OUT_DIR: default ./ngx-smoketest-win. Gets ngx-smoketest.exe and, when
#   the SDK has it, nvngx_dlss.dll (lib/Windows_x86_64/rel) next to it.
#
# Needs: clang, lld, llvm-ar, llvm-nm, llvm-objcopy, llvm-dlltool, the
# MinGW-w64 headers and import libraries (gcc-mingw-w64-x86-64), the Vulkan
# headers (VULKAN_INCLUDE, default /usr/include; only vulkan/ and vk_video/).
#
# How the MSVC library links with MinGW:
# - nvsdk_ngx_s.lib is built with MSVC against the static UCRT (/MT). Its
#   objects are extracted and every C runtime function they call that
#   ucrtbase.dll exports is renamed to __ngxucrt_<name> (llvm-objcopy
#   --redefine-syms). An import library maps those names back to ucrtbase.dll
#   (llvm-dlltool, "__ngxucrt_fputs == fputs"). So the NGX code gets the UCRT
#   it was compiled for (its FILE*, wcstok, heap), and our MinGW code keeps
#   msvcrt; the two never share CRT objects.
# - Their .drectve sections (/DEFAULTLIB:libcmt, /FAILIFMISMATCH, ...) are
#   removed: those libraries do not exist here.
# - msvc_rt_shim.c supplies the few pieces of the MSVC static runtime that
#   are not in ucrtbase.dll: /GS cookie and handlers, CFG check, thread-safe
#   statics, operator new/delete, std::_X* throw helpers.
# - lld links the MSVC COFF objects with the MinGW CRT start-up; the MinGW
#   start-up runs the objects' C++ initializers (.CRT$XCU) and provides the
#   TLS directory they use.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
NGX_SDK=${NGX_SDK:?set NGX_SDK to a DLSS SDK checkout (github.com/NVIDIA/DLSS)}
OUT_DIR=${1:-$PWD/ngx-smoketest-win}
VULKAN_INCLUDE=${VULKAN_INCLUDE:-/usr/include}
CLANG=${CLANG:-clang}
CLANGXX=${CLANGXX:-clang++}
TARGET=x86_64-w64-mingw32
MINGW_GCC=${MINGW_GCC:-x86_64-w64-mingw32-gcc}

lib="$NGX_SDK/lib/Windows_x86_64/x64/nvsdk_ngx_s.lib"
inc="$NGX_SDK/include"
[ -f "$lib" ] || { echo "no $lib" >&2; exit 1; }
[ -f "$inc/nvsdk_ngx_vk.h" ] || { echo "no $inc/nvsdk_ngx_vk.h" >&2; exit 1; }

work="$OUT_DIR/build"
rm -rf "$work"
mkdir -p "$work/ngx" "$work/include"
cp -r "$VULKAN_INCLUDE/vulkan" "$VULKAN_INCLUDE/vk_video" "$work/include/"

# 1. NGX objects, with their C runtime calls pointed at ucrtbase.dll.
(cd "$work/ngx" && llvm-ar x "$lib")
ucrtbase_a=$("$MINGW_GCC" -print-file-name=libucrtbase.a)
[ -f "$ucrtbase_a" ] || { echo "no libucrtbase.a from $MINGW_GCC" >&2; exit 1; }
llvm-nm "$ucrtbase_a" 2>/dev/null | awk '{print $NF}' | sed -n 's/^__imp_//p' | sort -u > "$work/ucrt-exports.txt"
llvm-nm --undefined-only "$work"/ngx/*.obj 2>/dev/null | awk 'NF == 2 && $1 == "U" {print $2}' | sort -u > "$work/undefined.txt"
llvm-nm --defined-only "$work"/ngx/*.obj 2>/dev/null | awk 'NF == 3 {print $3}' | sort -u > "$work/defined.txt"
comm -23 "$work/undefined.txt" "$work/defined.txt" | comm -12 - "$work/ucrt-exports.txt" > "$work/crt-calls.txt"
awk '{print $1 " __ngxucrt_" $1; print "__imp_" $1 " __imp___ngxucrt_" $1}' "$work/crt-calls.txt" > "$work/redefine.txt"
{
  echo "LIBRARY ucrtbase.dll"
  echo "EXPORTS"
  awk '{print "__ngxucrt_" $1 " == " $1}' "$work/crt-calls.txt"
} > "$work/ngxucrt.def"
llvm-dlltool -m i386:x86-64 -d "$work/ngxucrt.def" -l "$work/libngxucrt.a"
for o in "$work"/ngx/*.obj; do
  llvm-objcopy --remove-section=.drectve --redefine-syms="$work/redefine.txt" "$o"
done

# 2. Our code.
cflags="--target=$TARGET -O2 -Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers"
"$CLANG" $cflags -c "$here/msvc_rt_shim.c" -o "$work/msvc_rt_shim.o"
for s in main vk_mode d3d12_mode; do
  "$CLANGXX" $cflags -std=c++17 -fno-exceptions -fno-rtti -I"$work/include" -isystem "$inc" \
    -c "$here/$s.cpp" -o "$work/$s.o"
done

# 3. Link. Anything left unresolved is a hard error (lld).
"$CLANGXX" --target=$TARGET -fuse-ld=lld -static -o "$OUT_DIR/ngx-smoketest.exe" \
  "$work/main.o" "$work/vk_mode.o" "$work/d3d12_mode.o" "$work/msvc_rt_shim.o" \
  "$work"/ngx/*.obj "$work/libngxucrt.a" \
  -ld3d12 -ldxgi -lversion -lshell32 -ladvapi32 -luser32 -lkernel32

dlss="$NGX_SDK/lib/Windows_x86_64/rel/nvngx_dlss.dll"
if [ -f "$dlss" ]; then
  cp "$dlss" "$OUT_DIR/"
fi
echo "built $OUT_DIR/ngx-smoketest.exe"

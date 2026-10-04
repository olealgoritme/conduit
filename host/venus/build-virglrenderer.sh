#!/usr/bin/env bash
# Builds the pinned virglrenderer (Venus only) and venus-protocol into
# third_party/build/install, for `cargo build --features renderer`.
#
# Venus in virglrenderer always goes through its "render server". We build it
# with render-server-mode=thread and render-server-worker=thread so the server
# runs as threads inside conduit-venus: no virgl_render_server binary to
# install or find at runtime, and conduit-venus is already the separate,
# sandboxable process the server would otherwise give us.
#
# vrend (the OpenGL renderer) is off: Windows guests only speak Venus, and
# without vrend virglrenderer needs neither EGL nor GBM.
#
# Needs: meson, ninja, a C compiler, pkg-config, python3 with mako and yaml,
# libdrm headers, Vulkan headers. No root.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
tp="$here/third_party"
build="$tp/build"
prefix="$build/install"

missing=()
for tool in meson ninja cc pkg-config python3; do
    command -v "$tool" >/dev/null || missing+=("$tool")
done
python3 -c 'import mako' 2>/dev/null || missing+=("python3-mako")
python3 -c 'import yaml' 2>/dev/null || missing+=("python3-yaml")
pkg-config --exists libdrm 2>/dev/null || missing+=("libdrm-dev")
pkg-config --exists vulkan 2>/dev/null || [ -e /usr/include/vulkan/vulkan.h ] || missing+=("libvulkan-dev")
if [ ${#missing[@]} -ne 0 ]; then
    echo "missing: ${missing[*]}" >&2
    exit 1
fi

for sub in virglrenderer venus-protocol; do
    if [ ! -e "$tp/$sub/meson.build" ]; then
        echo "$tp/$sub is empty; run: git submodule update --init host/venus/third_party/$sub" >&2
        exit 1
    fi
done

# venus-protocol first, installed, so virglrenderer finds the same protocol
# revision the guest's Mesa was generated from (the pinned submodule) through
# pkg-config instead of fetching its own v1.1.3 wrap.
meson setup --reconfigure "$build/venus-protocol" "$tp/venus-protocol" \
    --prefix "$prefix" -Dwerror=false 2>/dev/null ||
    meson setup "$build/venus-protocol" "$tp/venus-protocol" \
        --prefix "$prefix" -Dwerror=false
ninja -C "$build/venus-protocol" install

export PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig:$prefix/lib/x86_64-linux-gnu/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

opts=(
    --prefix "$prefix"
    --libdir lib
    --buildtype release
    --wrap-mode nofallback
    -Dvenus=true
    -Dvrend=false
    -Dvideo=false
    -Drender-server-mode=thread
    -Drender-server-worker=thread
    -Dtests=false
)
meson setup --reconfigure "$build/virglrenderer" "$tp/virglrenderer" "${opts[@]}" 2>/dev/null ||
    meson setup "$build/virglrenderer" "$tp/virglrenderer" "${opts[@]}"
ninja -C "$build/virglrenderer" install

echo
echo "built. for cargo:"
echo "  export PKG_CONFIG_PATH=$prefix/lib/pkgconfig"
echo "  cargo build --features renderer"
echo "conduit-venus then needs LD_LIBRARY_PATH=$prefix/lib (or rpath, set by build.rs)."

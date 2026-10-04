#!/usr/bin/env bash
# packaging/build.sh: the one place that knows how to build and lay out Conduit.
#
# Every package format (deb, rpm, Arch, tarball, the RPM spec, the PKGBUILD and
# the release workflow) calls this script, so a change to the layout is made
# once. Steps can run separately (CI caches some of them) or together.
#
#   packaging/build.sh deps                 install build dependencies (root; apt/dnf/pacman)
#   packaging/build.sh rust                 backend, VMM and CLI      -> dist/rust/bin
#   packaging/build.sh viewer               Wayland/X11 viewer        -> dist/viewer/bin
#   packaging/build.sh stream               network stream host       -> dist/stream/bin
#   packaging/build.sh qemu                 bundled QEMU 11.1         -> dist/qemu-root
#   packaging/build.sh stage                assemble the install tree -> $STAGE
#   packaging/build.sh bundle-libs          copy non-glibc .so deps into /opt/conduit/lib (tarball)
#   packaging/build.sh guest-src [DIR]      DKMS source tree for the guest module
#   packaging/build.sh package FORMAT       deb | rpm | archlinux | tarball | guest-deb | guest-rpm | guest-arch
#
# Environment (all optional):
#   VERSION            package version (default: git describe, without the leading v)
#   RUST_TARGET        x86_64-unknown-linux-musl (default, static) or "host" (distro builds)
#   BUNDLE_QEMU        1 (default) to ship QEMU in /opt/conduit, 0 to leave it out
#   STAGE              install tree root (default dist/stage)
#   LINK_DIR           where the `conduit` command symlink goes (default /usr/bin;
#                      the tarball uses /usr/local/bin)
#   QEMU_BUILD_SCRIPT  default host/qemu/build-qemu.sh, see "QEMU" in docs/PACKAGING.md
#   JOBS               parallel build jobs (default: nproc)
#
# Build-output binary names (cargo bins, make targets). Installed names are
# fixed.
set -euo pipefail

BACKEND_BIN_SRC=${BACKEND_BIN_SRC:-conduit-backend}       # cargo bin name
BACKEND_PKG=${BACKEND_PKG:-device}                         # cargo package that owns it
BACKEND_FEATURES=${BACKEND_FEATURES:-vhost-user}
USERSPACE_BIN_SRC=${USERSPACE_BIN_SRC:-conduit-userspace} # same package as the backend
VIEWER_BIN_SRC=${VIEWER_BIN_SRC:-conduit-viewer}          # Makefile target
VMM_BIN_SRC=${VMM_BIN_SRC:-conduit-vmm}                    # cargo bin name
CLI_BIN_SRC=${CLI_BIN_SRC:-conduit}                        # -> conduit

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PKG="$ROOT/packaging"
DIST=${DIST:-$ROOT/dist}
STAGE=${STAGE:-$DIST/stage}
OUT=${OUT:-$DIST/out}
PREFIX=/opt/conduit                 # never configurable: AppArmor/SELinux rules name it
LINK_DIR=${LINK_DIR:-/usr/bin}
RUST_TARGET=${RUST_TARGET:-x86_64-unknown-linux-musl}
BUNDLE_QEMU=${BUNDLE_QEMU:-1}
QEMU_BUILD_SCRIPT=${QEMU_BUILD_SCRIPT:-$ROOT/host/qemu/build-qemu.sh}
JOBS=${JOBS:-$(nproc 2>/dev/null || echo 4)}

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

version() {
    if [ -n "${VERSION:-}" ]; then echo "${VERSION#v}"; return; fi
    local d
    d=$(git -C "$ROOT" describe --tags --match 'v*' 2>/dev/null || true)
    if [ -n "$d" ]; then
        # v0.3.0-5-gabc123 -> 0.3.0+git5.gabc123 (sorts after 0.3.0 everywhere)
        d=${d#v}
        echo "$d" | sed -E 's/-([0-9]+)-(g[0-9a-f]+)$/+git\1.\2/'
    else
        echo "0.0.0+git$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    fi
}

distro_family() {
    # shellcheck disable=SC1091
    . /etc/os-release
    case " ${ID:-} ${ID_LIKE:-} " in
        *" arch "*)                     echo arch ;;
        *" fedora "*|*" rhel "*|*" suse "*|*" opensuse "*) echo rpm ;;
        *" debian "*|*" ubuntu "*)      echo deb ;;
        *) die "unknown distribution ${ID:-?}" ;;
    esac
}

# ---------------------------------------------------------------- deps -----
# Build dependencies for every step. QEMU's list matches a headless build
# (no GTK/SDL: the viewer is the display); adjust together with host/qemu/.
cmd_deps() {
    local fam; fam=$(distro_family)
    case "$fam" in
    deb)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        apt-get install -y --no-install-recommends \
            build-essential pkg-config git curl ca-certificates xz-utils file patchelf \
            gnupg patch \
            python3 python3-venv python3-pip ninja-build meson flex bison bzip2 \
            musl-tools \
            libwayland-dev wayland-protocols libxcb1-dev libxcb-dri3-dev \
            libxcb-present-dev libxcb-render0-dev libxcb-xinput-dev libgbm-dev \
            libssl-dev libegl-dev \
            libglib2.0-dev libpixman-1-dev libslirp-dev libseccomp-dev \
            libcap-ng-dev libzstd-dev libaio-dev libfdt-dev \
            libpulse-dev libpipewire-0.3-dev
        ;;
    rpm)
        dnf install -y \
            gcc gcc-c++ make pkgconf-pkg-config git curl xz file patchelf \
            gnupg2 patch \
            python3 ninja-build meson flex bison bzip2 diffutils findutils \
            rpm-build \
            wayland-devel wayland-protocols-devel libxcb-devel mesa-libgbm-devel \
            openssl-devel mesa-libEGL-devel \
            glib2-devel pixman-devel libslirp-devel libseccomp-devel \
            libcap-ng-devel libzstd-devel libaio-devel libfdt-devel \
            pulseaudio-libs-devel pipewire-devel
        ;;
    arch)
        pacman -Syu --noconfirm --needed \
            base-devel git curl xz file patchelf gnupg python ninja meson flex bison \
            wayland wayland-protocols libxcb mesa openssl \
            glib2 pixman libslirp libseccomp libcap-ng zstd libaio dtc \
            libpulse pipewire
        ;;
    esac
}

# ---------------------------------------------------------------- rust -----
cargo_target_args() {
    if [ "$RUST_TARGET" = host ]; then
        TARGET_DIR_SUFFIX=release
        CARGO_TARGET_ARGS=()
    else
        TARGET_DIR_SUFFIX="$RUST_TARGET/release"
        CARGO_TARGET_ARGS=(--target "$RUST_TARGET")
        # musl: pure-Rust crates link statically with rustc's own crt. A C
        # dependency (cc crate) needs musl-gcc, which `deps` installs.
        if command -v musl-gcc >/dev/null; then
            export CC_x86_64_unknown_linux_musl=musl-gcc
        fi
    fi
}

# Each project may be its own workspace or a member of the root one (cli/
# is), so ask cargo where its target directory is instead of guessing.
target_dir() {
    (cd "$1" && cargo metadata --format-version 1 --no-deps) \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])'
}

cmd_rust() {
    cargo_target_args
    local bin="$DIST/rust/bin" t
    mkdir -p "$bin"

    log "backend ($BACKEND_BIN_SRC -> conduit-backend, $USERSPACE_BIN_SRC -> conduit-userspace)"
    (cd "$ROOT/host/backend" && cargo build --locked --release "${CARGO_TARGET_ARGS[@]}" \
        -p "$BACKEND_PKG" --features "$BACKEND_FEATURES" \
        --bin "$BACKEND_BIN_SRC" --bin "$USERSPACE_BIN_SRC")
    t=$(target_dir "$ROOT/host/backend")/$TARGET_DIR_SUFFIX
    install -m0755 "$t/$BACKEND_BIN_SRC" "$bin/conduit-backend"
    install -m0755 "$t/$USERSPACE_BIN_SRC" "$bin/conduit-userspace"

    log "vmm ($VMM_BIN_SRC -> conduit-vmm)"
    # The built-in VM runner (the fallback; QEMU is the default) uses glibc-only
    # calls (statx, STATX_DIOALIGN), so it is always built for the host's glibc
    # target, never musl. Distros' glibc is older-compatible enough for it.
    (cd "$ROOT/host/vmm" && cargo build --locked --release --no-default-features \
        --bin "$VMM_BIN_SRC")
    t=$(target_dir "$ROOT/host/vmm")/release
    install -m0755 "$t/$VMM_BIN_SRC" "$bin/conduit-vmm"

    log "cli ($CLI_BIN_SRC -> conduit)"
    (cd "$ROOT/cli" && cargo build --locked --release "${CARGO_TARGET_ARGS[@]}" --bin "$CLI_BIN_SRC")
    t=$(target_dir "$ROOT/cli")/$TARGET_DIR_SUFFIX
    install -m0755 "$t/$CLI_BIN_SRC" "$bin/conduit"

    # TODO: conduit-venus (host/venus, the Venus renderer for Windows guests)
    # is not packaged yet. It is experimental and needs virglrenderer built with
    # Venus (host/venus/build-virglrenderer.sh), so it is built by hand for now.

    if [ "$RUST_TARGET" != host ]; then
        for f in "$bin"/*; do
            [ "$(basename "$f")" = conduit-vmm ] && continue   # glibc on purpose, see above
            file "$f" | grep -qE 'statically linked|static-pie linked' \
                || die "$f is not static"
        done
    fi
}

# -------------------------------------------------------------- viewer -----
cmd_viewer() {
    log "viewer ($VIEWER_BIN_SRC -> conduit-viewer)"
    make -C "$ROOT/host/viewer" -j"$JOBS" all
    mkdir -p "$DIST/viewer/bin"
    install -m0755 "$ROOT/host/viewer/$VIEWER_BIN_SRC" "$DIST/viewer/bin/conduit-viewer"
}

# -------------------------------------------------------------- stream -----
# conduit-stream (host/stream): links glibc, libEGL, libgbm and OpenSSL, so it
# is always built for the host target, like the viewer.
cmd_stream() {
    log "stream (conduit-stream)"
    (cd "$ROOT/host/stream" && cargo build --locked --release)
    mkdir -p "$DIST/stream/bin"
    install -m0755 "$(target_dir "$ROOT/host/stream")/release/conduit-stream" \
        "$DIST/stream/bin/conduit-stream"
}

# ---------------------------------------------------------------- qemu -----
# host/qemu/build-qemu.sh fetches (signature-checked), patches, configures
# with --prefix=$PREFIX and builds in $WORKDIR/build. It is run without
# --install; the install into a DESTDIR (no root, no sudo) happens here.
cmd_qemu() {
    [ "$BUNDLE_QEMU" = 1 ] || { log "BUNDLE_QEMU=0, skipping QEMU"; return; }
    [ -f "$QEMU_BUILD_SCRIPT" ] || die "$QEMU_BUILD_SCRIPT missing"
    log "qemu via $QEMU_BUILD_SCRIPT"
    local work="$DIST/qemu-work"
    rm -rf "$DIST/qemu-root"
    JOBS="$JOBS" WORKDIR="$work" bash "$QEMU_BUILD_SCRIPT" --prefix "$PREFIX"
    # ninja (not the system meson) so QEMU's own pyvenv meson does the install.
    DESTDIR="$DIST/qemu-root" ninja -C "$work/build" install
    [ -x "$DIST/qemu-root$PREFIX/bin/qemu-system-x86_64" ] \
        || die "QEMU build did not produce $PREFIX/bin/qemu-system-x86_64"
}

# --------------------------------------------------------------- stage -----
# Lays out the complete install tree under $STAGE, as it will be on disk:
#   /opt/conduit/bin/{conduit,conduit-backend,conduit-userspace,conduit-viewer,conduit-vmm,qemu-system-x86_64}
#   /opt/conduit/share/conduit/supported-drivers.txt
#   /opt/conduit/share/conduit/guest/conduit-guest.deb (what `conduit create` installs in VMs)
#   /opt/conduit/share/conduit/guest/conduit-guest.pkg.tar.zst (`conduit attach` on Arch guests)
#   /opt/conduit/share/qemu/...            (bundled QEMU data)
#   /opt/conduit/libexec/conduit-integrate (AppArmor/SELinux/desktop hookup)
#   /opt/conduit/share/conduit/...         (desktop entry, AppArmor sources)
#   /opt/conduit/share/doc/conduit/...     (licenses, NOTICE files)
#   $LINK_DIR/conduit -> /opt/conduit/bin/conduit
#   /usr/share/applications/conduit.desktop (or /usr/local/share/... with LINK_DIR=/usr/local/bin)
#   /etc/apparmor.d/abstractions/conduit
cmd_stage() {
    local o="$STAGE$PREFIX" v; v=$(version)
    log "staging $v into $STAGE"
    rm -rf "$o"
    install -d "$o/bin" "$o/libexec" "$o/share/conduit/apparmor" "$o/share/doc/conduit"

    for b in conduit conduit-backend conduit-userspace conduit-vmm; do
        [ -x "$DIST/rust/bin/$b" ] || die "missing $DIST/rust/bin/$b (run: build.sh rust)"
        install -m0755 "$DIST/rust/bin/$b" "$o/bin/$b"
    done
    [ -x "$DIST/viewer/bin/conduit-viewer" ] || die "missing viewer (run: build.sh viewer)"
    install -m0755 "$DIST/viewer/bin/conduit-viewer" "$o/bin/conduit-viewer"
    if [ -x "$DIST/stream/bin/conduit-stream" ]; then
        install -m0755 "$DIST/stream/bin/conduit-stream" "$o/bin/conduit-stream"
    else
        log "warning: no conduit-stream (run: build.sh stream); \`conduit stream\` will not work from this install"
    fi

    if [ "$BUNDLE_QEMU" = 1 ]; then
        [ -d "$DIST/qemu-root$PREFIX" ] || die "missing QEMU (run: build.sh qemu)"
        cp -a "$DIST/qemu-root$PREFIX/." "$o/"
    fi

    # Host driver releases the backend accepts (read by `conduit doctor`).
    {
        echo "# NVIDIA driver releases the backend has exact ABI tables for (packaging/supported-drivers.sh)"
        "$PKG/supported-drivers.sh" "$ROOT/host/backend/gen/src"
    } > "$o/share/conduit/supported-drivers.txt"

    # The guest driver packages `conduit create` / `conduit stock-kernel` /
    # `conduit attach` install into VMs (DKMS builds the module there for the
    # VM's own kernel): the .deb, and the Arch package for Arch guests.
    local gdeb="$OUT/conduit-guest_${v}-1_all.deb" garch="$DIST/guest-arch/conduit-guest.pkg.tar.zst"
    if [ ! -f "$gdeb" ] && command -v nfpm >/dev/null; then
        guest_package deb "$gdeb"
    fi
    if [ -f "$gdeb" ]; then
        install -D -m0644 "$gdeb" "$o/share/conduit/guest/conduit-guest.deb"
    else
        log "warning: no conduit-guest .deb (nfpm missing); \`conduit create\` will not work from this install"
    fi
    if command -v nfpm >/dev/null; then
        install -d "$DIST/guest-arch"
        guest_package archlinux "$garch"
        install -D -m0644 "$garch" "$o/share/conduit/guest/conduit-guest.pkg.tar.zst"
    else
        log "warning: no conduit-guest Arch package (nfpm missing); \`conduit attach\` will not set up Arch guests from this install"
    fi

    install -m0755 "$PKG/common/conduit-integrate" "$o/libexec/conduit-integrate"
    install -m0644 "$PKG/common/conduit.desktop" "$o/share/conduit/conduit.desktop"
    install -m0644 "$PKG/common/apparmor/conduit" "$o/share/conduit/apparmor/conduit"
    echo "$v" > "$o/VERSION"

    # Licenses travel with the binaries (per-component; see docs/STRUCTURE.md).
    install -m0644 "$ROOT/LICENSE" "$o/share/doc/conduit/LICENSE"
    local f rel
    while IFS= read -r f; do
        rel=${f#"$ROOT"/}; rel=${rel//\//_}
        install -m0644 "$f" "$o/share/doc/conduit/$rel"
    done < <(find "$ROOT/host" "$ROOT/cli" -maxdepth 2 \
                \( -name 'LICENSE*' -o -name 'NOTICE*' \) -type f 2>/dev/null | sort)

    # System integration files outside the prefix.
    local share=/usr/share
    [ "$LINK_DIR" = /usr/local/bin ] && share=/usr/local/share
    install -d "$STAGE$LINK_DIR" "$STAGE$share/applications" "$STAGE/etc/apparmor.d/abstractions"
    ln -sfn "$PREFIX/bin/conduit" "$STAGE$LINK_DIR/conduit"
    install -m0644 "$PKG/common/conduit.desktop" "$STAGE$share/applications/conduit.desktop"
    install -m0644 "$PKG/common/apparmor/conduit" "$STAGE/etc/apparmor.d/abstractions/conduit"
}

# --------------------------------------------------------- bundle-libs -----
# For the distro-independent tarball: copy every shared library the viewer and
# QEMU need, except the glibc family and driver-bound GPU libraries, into
# /opt/conduit/lib and point each ELF's RUNPATH there. Built on an old glibc
# (Debian 12) so the result runs on anything newer.
SKIP_LIBS='^(linux-vdso|ld-linux|libc|libm|libdl|libpthread|librt|libresolv|libutil|libanl|libmvec|libGL|libEGL|libGLX|libGLdispatch|libgbm|libdrm|libnvidia)[.-]'
cmd_bundle_libs() {
    local o="$STAGE$PREFIX" libdir="$STAGE$PREFIX/lib" elf dep name rel
    command -v patchelf >/dev/null || die "patchelf needed"
    install -d "$libdir"
    while IFS= read -r elf; do
        file "$elf" | grep -q 'dynamically linked' || continue
        while read -r name dep; do
            [ -n "$dep" ] && [ -f "$dep" ] || continue
            echo "$name" | grep -qE "$SKIP_LIBS" && continue
            [ -e "$libdir/$name" ] || install -m0644 "$(readlink -f "$dep")" "$libdir/$name"
        done < <(ldd "$elf" | awk '/=> \//{print $1, $3}')
        rel=$(realpath --relative-to="$(dirname "$elf")" "$libdir")
        patchelf --set-rpath "\$ORIGIN/$rel" "$elf"
    done < <(find "$o/bin" "$o/libexec" "$o/lib" -type f 2>/dev/null)
    # Bundled libraries find their siblings through their own RUNPATH.
    for elf in "$libdir"/*.so*; do patchelf --set-rpath '$ORIGIN' "$elf"; done
    log "bundled $(find "$libdir" -maxdepth 1 -name '*.so*' | wc -l) libraries"
}

# ----------------------------------------------------------- guest-src -----
# The DKMS source tree: the module sources plus dkms.conf with the version in.
cmd_guest_src() {
    local v dst; v=$(version)
    dst=${1:-$DIST/guest-src/conduit-guest-$v}
    log "guest DKMS source -> $dst"
    rm -rf "$dst"; install -d "$dst"
    (cd "$ROOT/guest/linux" && cp -a Makefile ./*.c ./*.h gen rmctrl "$dst/")
    [ -f "$ROOT/guest/linux/Kbuild" ] && cp -a "$ROOT/guest/linux/Kbuild" "$dst/"
    sed "s/@VERSION@/$v/g" "$PKG/dkms/dkms.conf" > "$dst/dkms.conf"
    # Kbuild objects must never ship in the source package.
    find "$dst" \( -name '*.o' -o -name '*.ko' -o -name '*.mod*' -o -name '.*.cmd' \) -delete
}

# ------------------------------------------------------------- package -----
# Runtime dependencies of the dynamically linked binaries in the stage, in the
# packager's vocabulary: Debian package names, RPM sonames (portable across
# Fedora and openSUSE), Arch package names.
shlib_deps() {
    local fmt=$1 elf
    command -v file >/dev/null && command -v readelf >/dev/null \
        || die "shlib_deps needs file and readelf (run: build.sh deps)"
    while IFS= read -r elf; do
        file "$elf" | grep -q 'dynamically linked' || continue
        case "$fmt" in
        rpm)
            readelf -d "$elf" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1()(64bit)/p' ;;
        *)
            ldd "$elf" | awk '/=> \//{print $3}' | while read -r lib; do
                case "$fmt" in
                deb) dpkg -S "$lib" 2>/dev/null || dpkg -S "$(readlink -f "$lib")" 2>/dev/null \
                        || dpkg -S "/usr$lib" 2>/dev/null || true ;;
                archlinux) pacman -Qoq "$lib" 2>/dev/null || true ;;
                esac
            done | sed -E 's/^([^:]+):.*$/\1/; s/:amd64$//' ;;
        esac
    done < <(find "$STAGE$PREFIX" -type f \( -path '*/bin/*' -o -path '*/libexec/*' -o -name '*.so*' \)) \
      | sort -u | grep -v '^$' || true
}

render_nfpm() {   # template out format
    local tpl=$1 out=$2 fmt=$3 v deps; v=$(version)
    deps=""
    if [ -n "$fmt" ]; then
        deps=$(shlib_deps "$fmt" | sed 's/.*/"&"/' | paste -sd, -)
    fi
    sed -e "s|@VERSION@|$v|g" -e "s|@STAGE@|$STAGE|g" -e "s|@ROOT@|$ROOT|g" \
        -e "s|@GUEST_SRC@|$DIST/guest-src/conduit-guest-$v|g" \
        -e "s|@SCRIPTS@|$DIST/pkgscripts|g" -e "s|\"@DEPENDS@\"|$deps|g" "$tpl" > "$out"
}

render_scripts() {
    local v; v=$(version)
    rm -rf "$DIST/pkgscripts"; install -d "$DIST/pkgscripts"
    for s in "$PKG"/deb/conduit/* "$PKG"/deb/conduit-guest/*; do
        local name; name="$(basename "$(dirname "$s")")-$(basename "$s")"
        sed "s/@VERSION@/$v/g" "$s" > "$DIST/pkgscripts/$name"
        chmod 0755 "$DIST/pkgscripts/$name"
    done
}

# The conduit-guest package in one nfpm format (deb | rpm | archlinux), to
# TARGET (a file, or a directory for nfpm's own file name).
guest_package() {   # packager target
    install -d "$OUT"; render_scripts
    cmd_guest_src
    render_nfpm "$PKG/nfpm/conduit-guest.yaml" "$DIST/nfpm-conduit-guest.yaml" ""
    nfpm package --config "$DIST/nfpm-conduit-guest.yaml" --packager "$1" --target "$2" >/dev/null
    log "conduit-guest ($1) -> $2"
}

cmd_package() {
    local fmt=${1:?format} v; v=$(version)
    install -d "$OUT"
    render_scripts
    case "$fmt" in
    deb|rpm|archlinux)
        command -v nfpm >/dev/null || die "nfpm not found (https://nfpm.goreleaser.com)"
        [ -x "$STAGE$PREFIX/bin/conduit" ] || die "stage is empty (run: build.sh stage)"
        render_nfpm "$PKG/nfpm/conduit.yaml" "$DIST/nfpm-conduit.yaml" "$fmt"
        nfpm package --config "$DIST/nfpm-conduit.yaml" --packager "$fmt" --target "$OUT/"
        ;;
    guest-deb|guest-rpm)
        command -v nfpm >/dev/null || die "nfpm not found"
        guest_package "${fmt#guest-}" "$OUT/"
        ;;
    guest-arch)
        command -v nfpm >/dev/null || die "nfpm not found"
        guest_package archlinux "$OUT/"
        ;;
    tarball)
        # LINK_DIR=/usr/local/bin stage, plus bundle-libs, must have run first.
        local t="$DIST/tarball/conduit"
        rm -rf "$DIST/tarball"; install -d "$t/root"
        cp -a "$STAGE$PREFIX" "$t/root/"
        install -m0755 "$PKG/tarball/install.sh" "$PKG/tarball/uninstall.sh" "$t/"
        install -m0755 "$PKG/tarball/uninstall.sh" "$t/root/conduit/uninstall.sh"
        echo "$v" > "$t/VERSION"
        tar -C "$DIST/tarball" --owner=0 --group=0 --numeric-owner \
            -czf "$OUT/conduit-$v-x86_64-linux.tar.gz" conduit
        ;;
    *) die "unknown format $fmt" ;;
    esac
    ls -l "$OUT"
}

main() {
    [ $# -ge 1 ] || { sed -n '2,30p' "$0"; exit 1; }
    local cmd=$1; shift
    case "$cmd" in
        deps) cmd_deps ;;
        rust) cmd_rust ;;
        viewer) cmd_viewer ;;
        stream) cmd_stream ;;
        qemu) cmd_qemu ;;
        stage) cmd_stage ;;
        bundle-libs) cmd_bundle_libs ;;
        guest-src) cmd_guest_src "$@" ;;
        package) cmd_package "$@" ;;
        version) version ;;
        *) die "unknown command $cmd" ;;
    esac
}
main "$@"

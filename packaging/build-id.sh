#!/bin/sh
# packaging/build-id.sh: the build identity every Conduit binary carries.
#
# `conduit-backend --version` and the other Conduit binaries print
#   NAME VERSION (git COMMIT, KIND)
# and the bundled QEMU's `--version` carries the same in its package version.
# `conduit doctor` reads them back and warns about a mix of builds. Build
# scripts (Rust build.rs, the viewer's Makefile, host/qemu/build-qemu.sh) all
# take the fields from here, so they agree.
#
#   version  $CONDUIT_VERSION (packaging/build.sh sets it), else the version in
#            the root Cargo.toml
#   commit   $CONDUIT_GIT_COMMIT, else the checkout's HEAD (10 hex digits),
#            with "-dirty" when tracked files differ from it; "unknown" outside
#            a git checkout
#   kind     $CONDUIT_BUILD_KIND: "release" (release.yml on a v* tag), "ci"
#            (other release.yml runs), "rpm" / "arch" (the spec and PKGBUILD),
#            "nix" (flake.nix); else "local"
#
# Prints KEY=VALUE lines: version, commit, kind, id ("VERSION (git COMMIT,
# KIND)") and qemu_patches ("COUNT SHA8" of host/qemu/patches/*.patch, the
# patch set build-qemu.sh applies). With --watch it also prints watch=PATH
# lines: the git files whose change means HEAD moved (for build scripts).
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)

version=${CONDUIT_VERSION:-}
if [ -z "$version" ]; then
    version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
fi
version=${version#v}
[ -n "$version" ] || version=0.0.0

in_git() { git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; }

commit=${CONDUIT_GIT_COMMIT:-}
if [ -z "$commit" ]; then
    if in_git && commit=$(git -C "$ROOT" rev-parse --short=10 HEAD 2>/dev/null); then
        git -C "$ROOT" diff --quiet --ignore-submodules HEAD -- 2>/dev/null \
            || commit="$commit-dirty"
    else
        commit=unknown
    fi
fi

kind=${CONDUIT_BUILD_KIND:-local}

patches=$(ls "$ROOT"/host/qemu/patches/*.patch 2>/dev/null | wc -l | tr -d ' ')
if [ "$patches" -gt 0 ]; then
    sum=$(cat "$ROOT"/host/qemu/patches/*.patch | sha256sum | cut -c1-8)
else
    sum=none
fi

echo "version=$version"
echo "commit=$commit"
echo "kind=$kind"
echo "id=$version (git $commit, $kind)"
echo "qemu_patches=$patches $sum"

if [ "${1:-}" = --watch ] && in_git; then
    git -C "$ROOT" rev-parse --path-format=absolute --git-path HEAD 2>/dev/null \
        | sed 's/^/watch=/'
    git -C "$ROOT" rev-parse --path-format=absolute --git-path packed-refs 2>/dev/null \
        | sed 's/^/watch=/'
    if ref=$(git -C "$ROOT" symbolic-ref -q HEAD 2>/dev/null); then
        git -C "$ROOT" rev-parse --path-format=absolute --git-path "$ref" 2>/dev/null \
            | sed 's/^/watch=/'
    fi
fi

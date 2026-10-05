#!/usr/bin/env bash
# Builds the Helios driver package in the Windows build VM from this checkout,
# with no GitHub Actions run: copies guest/windows (with the DXVK and
# vkd3d-proton submodules) to the VM over SSH, runs ci/vm/Build-InVm.ps1
# there, and copies the signed package back.
#
#   guest/windows/ci/vm/win-build.sh [Release|Debug]
#
# The VM must have been set up once with ci/vm/Setup-BuildVm.ps1.
# Settings, from the environment or else from
# ${XDG_CONFIG_HOME:-~/.config}/conduit/win-build.env (shell assignments,
# e.g. WIN_SSH='me@127.0.0.1'; WIN_BUILD_ENV names another file):
# WIN_SSH (required: the VM's SSH destination), WIN_PORT (2222), WIN_ROOT
# (W:), OUT (dist/windows-driver/<Configuration> in the checkout), WIN_SRC
# (another checkout or worktree whose guest/windows to build; the build
# scripts in ci/vm still come from this one), CLEAN (0).
set -euo pipefail
config=${1:-Release}
case "$config" in Release|Debug) ;; *) echo "usage: $0 [Release|Debug]" >&2; exit 2 ;; esac

conf=${WIN_BUILD_ENV:-${XDG_CONFIG_HOME:-$HOME/.config}/conduit/win-build.env}
if [ -f "$conf" ]; then
    # What the environment sets wins over the file.
    declare -A from_env=()
    for k in WIN_SSH WIN_PORT WIN_ROOT OUT WIN_SRC CLEAN; do
        if [ -n "${!k+x}" ]; then from_env[$k]=${!k}; fi
    done
    # shellcheck source=/dev/null
    . "$conf"
    for k in "${!from_env[@]}"; do printf -v "$k" '%s' "${from_env[$k]}"; done
fi
if [ -z "${WIN_SSH:-}" ]; then
    cat >&2 <<EOF
WIN_SSH is not set: the build VM's SSH destination (USER@HOST, your user in
the VM). Set it in the environment or in $conf, e.g.:
  mkdir -p "$(dirname "$conf")" && echo "WIN_SSH='you@127.0.0.1'" >> "$conf"
See guest/windows/ci/vm/README.md.
EOF
    exit 2
fi

here="$(cd "$(dirname "$0")" && pwd)"
self="$(cd "$here/../../../.." && pwd)"
repo="$(cd "${WIN_SRC:-$self}" && pwd)"
win="$repo/guest/windows"
ssh_to=$WIN_SSH
port=${WIN_PORT:-2222}
# No trailing backslash: Windows' command line would read "W:\"" as an
# escaped quote. Build-InVm.ps1 adds the backslash itself.
root=${WIN_ROOT:-W:}
out=${OUT:-$self/dist/windows-driver/$config}
ssh_win() { ssh -o ConnectTimeout=10 -o ServerAliveInterval=30 -p "$port" "$ssh_to" "$@"; }

for sub in dxvk vkd3d-proton; do
    if [ ! -e "$win/third_party/$sub/meson.build" ]; then
        echo "third_party/$sub is empty; run: git submodule update --init --recursive guest/windows/third_party/$sub" >&2
        exit 1
    fi
done

echo "==> copying guest/windows to $root\\src"
# Incremental by default: only files whose content changed since the last
# copy from this checkout (a sha256 manifest under dist/windows-driver/) are
# sent, and they land with the current time, so cargo and ninja see them as
# newer than their last build however the two clocks or the copied mtimes
# compare; unchanged files keep their times and stay built. Files gone from
# the checkout are deleted in the VM. CLEAN=1 (or no manifest yet) wipes the
# VM tree and copies everything, and Build-InVm.ps1 -Clean rebuilds DXVK and
# vkd3d too.
manifest_dir="$self/dist/windows-driver"
mkdir -p "$manifest_dir"
manifest="$manifest_dir/.sync-$(printf '%s' "$repo" | sha256sum | cut -c1-12)"
files() {
    (cd "$repo" && find guest/windows -type f \
        -not -path '*/.git/*' -not -path '*/target/*' -not -path 'guest/windows/third_party/mesa/*' \
        -print0 | sort -z | xargs -0 sha256sum)
}
files > "$manifest.new"
if [ "${CLEAN:-0}" = 1 ] || [ ! -s "$manifest" ]; then
    ssh_win "if (Test-Path $root\\src\\guest) { Remove-Item -Recurse -Force $root\\src\\guest }; New-Item -ItemType Directory -Force $root\\src\\guest | Out-Null"
    tar -C "$repo" -cf - --exclude=.git --exclude=target --exclude='guest/windows/third_party/mesa' guest/windows \
        | ssh_win "tar -xf - -C $root\\src"
else
    changed=$(comm -13 <(sort "$manifest") <(sort "$manifest.new") | cut -c67-)
    gone=$(comm -23 <(cut -c67- "$manifest" | sort) <(cut -c67- "$manifest.new" | sort))
    echo "    $(printf '%s' "$changed" | grep -c . || true) changed, $(printf '%s' "$gone" | grep -c . || true) removed"
    if [ -n "$changed" ]; then
        printf '%s\n' "$changed" | tar -C "$repo" -cf - -T - | ssh_win "tar -xmf - -C $root\\src"
    fi
    if [ -n "$gone" ]; then
        printf '%s\n' "$gone" | sed 's|/|\\|g' | while IFS= read -r f; do
            ssh_win "Remove-Item -LiteralPath '$root\\src\\$f' -Force -ErrorAction SilentlyContinue"
        done
    fi
fi
mv "$manifest.new" "$manifest"
if [ "$repo" != "$self" ]; then
    tar -C "$self" -cf - guest/windows/ci/vm | ssh_win "tar -xf - -C $root\\src"
fi

echo "==> building $config in the VM"
# PowerShell 7, as windows.yml's `shell: pwsh` steps.
ssh_win "& \"\$env:ProgramFiles\\PowerShell\\7\\pwsh.exe\" -NoProfile -ExecutionPolicy Bypass -File $root\\src\\guest\\windows\\ci\\vm\\Build-InVm.ps1 -Configuration $config -Root $root$( [ "${CLEAN:-0}" = 1 ] && echo ' -Clean' )"

echo "==> copying the package to $out"
rm -rf "$out"; mkdir -p "$out"
ssh_win "tar -cf - -C $root\\out\\$config ." | tar -xf - -C "$out"
ls "$out"
echo "Done: $out"
echo "Install it in the Windows guest with pnputil: guest/windows/ci/vm/README.md, \"Installing the package\"."

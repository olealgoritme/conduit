#!/usr/bin/env bash
# Check that every patch series in the tree applies cleanly on its pinned
# base, in the order and with the git command the real builds use:
#
#   nvk           guest/nvk-rm/series.sh on MESA_BASE, `git am --3way`
#                 (guest/nvk-rm/build.sh and build-windows.sh)
#   dxvk          guest/windows/third_party/patches/dxvk on the dxvk submodule
#   vkd3d-proton  .../patches/vkd3d-proton on the vkd3d-proton submodule
#                 (both `git apply`, in name order: ci/windows/Build-Driver.ps1)
#   dxvk-nvapi    .../patches/dxvk-nvapi on the commit tools/build-dxvk-nvapi.sh
#                 pins, `git apply` in glob order
#   mesa-helios   guest/windows/icd/patches/series on the mesa submodule,
#                 `git apply` in series order (ci/windows/build-mesa.sh)
#
#   .github/scripts/check-patches.sh [SERIES...]     (default: all of them)
#
# Each base is fetched shallowly (one commit) into CACHE_DIR, default
# ${XDG_CACHE_HOME:-~/.cache}/conduit-patch-check, and reused: a later run
# fetches nothing when the pins did not change. Nothing outside CACHE_DIR is
# written. To fetch from a local clone instead of the network, point
# MESA_URL, DXVK_URL, VKD3D_URL, NVAPI_URL or MESA_HELIOS_URL at it (it must
# contain the pinned commit).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cache=${CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/conduit-patch-check}

# git am commits on the scratch checkouts; nothing is pushed.
export GIT_AUTHOR_NAME="Conduit patch check" GIT_AUTHOR_EMAIL=ci@conduit.invalid
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
# No user or system git config: hooks, rerere or am.threeWay settings there
# would make a local run differ from CI.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

failed=()

# Pinned values, read from the files the builds take them from.
# build.sh and build-windows.sh each pin it; they must agree.
mesa_base() {
  local f bases
  # shellcheck disable=SC2016 # a literal ${MESA_BASE:-...} in the scripts
  bases=$(for f in build.sh build-windows.sh; do
            sed -n 's/^MESA_BASE=\${MESA_BASE:-\([0-9a-f]\{40\}\)}$/\1/p' "$root/guest/nvk-rm/$f"
          done | sort -u)
  if [ "$(printf '%s\n' "$bases" | wc -l)" != 1 ]; then
    echo "nvk: build.sh and build-windows.sh pin different MESA_BASE commits: $bases" >&2
    return 1
  fi
  echo "$bases"
}
nvapi_commit() {
  sed -n 's/^COMMIT=\([0-9a-f]\{40\}\).*/\1/p' "$root/guest/windows/tools/build-dxvk-nvapi.sh"
}
submodule_commit() { # path
  git -C "$root" ls-tree HEAD "$1" | awk '$2 == "commit" { print $3 }'
}
submodule_url() { # path
  git -C "$root" config -f .gitmodules --get "submodule.$1.url"
}

# checkout NAME URL COMMIT: a clean detached checkout of COMMIT in
# $cache/NAME; prints its path.
checkout() {
  local name=$1 url=$2 commit=$3 dir=$cache/$1
  [[ $commit =~ ^[0-9a-f]{40}$ ]] || { echo "$name: no pinned commit found" >&2; return 1; }
  if [ ! -d "$dir/.git" ]; then
    mkdir -p "$dir"
    git -C "$dir" init -q
  fi
  git -C "$dir" am --abort >/dev/null 2>&1 || true
  if ! git -C "$dir" cat-file -e "$commit^{commit}" 2>/dev/null; then
    echo "$name: fetching $commit from $url" >&2
    git -C "$dir" fetch -q --depth 1 --no-tags "$url" "$commit"
  fi
  git -C "$dir" checkout -q -f --detach "$commit"
  git -C "$dir" clean -q -fdx
  echo "$dir"
}

# `git am --3way` of the whole list, as the NVK builds do it.
check_am() { # name dir patch...
  local name=$1 dir=$2
  shift 2
  if git -C "$dir" am -q --3way "$@" >/dev/null; then
    echo "$name: $# patches apply"
  else
    local next p
    next=$(cat "$(git -C "$dir" rev-parse --absolute-git-dir)/rebase-apply/next" 2>/dev/null || echo 0)
    p=${*:next:1}
    echo "::error::$name: ${p#"$root"/} does not apply (patch $next of $#)"
    git -C "$dir" status --short | grep -v '^??' | sed "s/^/$name: /" || true
    git -C "$dir" am --abort || true
    failed+=("$name")
  fi
}

# `git apply` one patch at a time, as Build-Driver.ps1 and build-mesa.sh do.
check_apply() { # name dir patch...
  local name=$1 dir=$2 p
  shift 2
  for p in "$@"; do
    if ! (cd "$dir" && git apply --whitespace=nowarn "$p"); then
      echo "::error::$name: $(basename "$p") does not apply"
      failed+=("$name")
      return 0
    fi
  done
  echo "$name: $# patches apply"
}

check_nvk() {
  local dir series
  dir=$(checkout mesa "${MESA_URL:-https://gitlab.freedesktop.org/mesa/mesa.git}" "$(mesa_base)")
  mapfile -t series < <(sh "$root/guest/nvk-rm/series.sh")
  check_am nvk "$dir" "${series[@]}"
}

# Build-Driver.ps1: Get-ChildItem *.patch | Sort-Object Name (case-insensitive)
engine_patches() { # dir
  find "$1" -maxdepth 1 -name '*.patch' -printf '%f\n' | LC_ALL=C sort -f | sed "s|^|$1/|"
}

check_engine() { # name submodule-path url-override
  local name=$1 path=guest/windows/third_party/$2 dir patches
  dir=$(checkout "$name" "${3:-$(submodule_url "$path")}" "$(submodule_commit "$path")")
  mapfile -t patches < <(engine_patches "$root/guest/windows/third_party/patches/$name")
  check_apply "$name" "$dir" "${patches[@]}"
}

check_nvapi() {
  local dir patches
  dir=$(checkout dxvk-nvapi "${NVAPI_URL:-https://github.com/jp7677/dxvk-nvapi.git}" "$(nvapi_commit)")
  # build-dxvk-nvapi.sh: for p in "$patches"/*.patch (glob order)
  patches=("$root"/guest/windows/third_party/patches/dxvk-nvapi/*.patch)
  check_apply dxvk-nvapi "$dir" "${patches[@]}"
}

check_mesa_helios() {
  local path=guest/windows/third_party/mesa dir line patches=()
  dir=$(checkout mesa-helios "${MESA_HELIOS_URL:-$(submodule_url "$path")}" "$(submodule_commit "$path")")
  while IFS= read -r line; do
    line=${line%$'\r'}
    [[ -z $line || $line == \#* ]] && continue
    patches+=("$root/guest/windows/icd/patches/$line")
  done < "$root/guest/windows/icd/patches/series"
  check_apply mesa-helios "$dir" "${patches[@]}"
}

all=(nvk dxvk vkd3d-proton dxvk-nvapi mesa-helios)
[ $# -gt 0 ] || set -- "${all[@]}"
for s in "$@"; do
  case $s in
    nvk) check_nvk ;;
    dxvk) check_engine dxvk dxvk "${DXVK_URL:-}" ;;
    vkd3d-proton) check_engine vkd3d-proton vkd3d-proton "${VKD3D_URL:-}" ;;
    dxvk-nvapi) check_nvapi ;;
    mesa-helios) check_mesa_helios ;;
    *) echo "unknown series '$s' (one of: ${all[*]})" >&2; exit 2 ;;
  esac
done

if [ ${#failed[@]} -gt 0 ]; then
  echo "patch series that do not apply: ${failed[*]}" >&2
  exit 1
fi

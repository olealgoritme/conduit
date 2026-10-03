#!/usr/bin/env bash
# Cut a Conduit release: bump the version, commit, tag, push.
# The tag is the single source of truth: GitHub Actions (release.yml) builds
# every package from it, and packaging/build.sh reads it via `git describe`.
#
#   packaging/release.sh            patch bump  (0.1.0 -> 0.1.1)
#   packaging/release.sh minor      minor bump  (0.1.1 -> 0.2.0)
#   packaging/release.sh major      major bump  (0.2.0 -> 1.0.0)
#   packaging/release.sh 0.3.0      explicit version
#   packaging/release.sh --dry-run  print what would happen
#
# Also available as `make release` / `make release-minor` / `make release-major`.
set -euo pipefail
cd "$(dirname "$0")/.."

DRY=0
ARG=patch
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    *) ARG=$a ;;
  esac
done

run() { if [ "$DRY" = 1 ]; then echo "+ $*"; else "$@"; fi; }

branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = main ] || { echo "release: switch to main first (on $branch)" >&2; exit 1; }
git fetch -q --tags origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || {
  echo "release: local main differs from origin/main; pull/push first" >&2; exit 1; }
# Only the files this script edits must be clean; other work in progress in
# the tree is left alone and is not part of the release (the tag is a commit).
# Every Rust workspace carries the Conduit version.
VERSIONED=(Cargo.toml Cargo.lock host/backend/Cargo.toml host/backend/Cargo.lock
           host/vmm/Cargo.toml host/vmm/Cargo.lock host/stream/Cargo.toml host/stream/Cargo.lock)
git diff --quiet -- "${VERSIONED[@]}" || {
  echo "release: Cargo.toml/Cargo.lock files have uncommitted changes" >&2; exit 1; }

last=$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -1)
last=${last#v}
[ -n "$last" ] || last=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
IFS=. read -r MA MI PA <<<"$last"

case "$ARG" in
  patch) new="$MA.$MI.$((PA + 1))" ;;
  minor) new="$MA.$((MI + 1)).0" ;;
  major) new="$((MA + 1)).0.0" ;;
  [0-9]*.[0-9]*.[0-9]*) new=$ARG ;;
  *) echo "release: unknown argument '$ARG' (patch|minor|major|X.Y.Z)" >&2; exit 1 ;;
esac
# First release: tag the current version rather than skipping past it.
if [ -z "$(git tag -l 'v*')" ] && [ "$ARG" = patch ]; then new=$last; fi

git rev-parse -q --verify "refs/tags/v$new" >/dev/null && {
  echo "release: v$new already exists" >&2; exit 1; }

echo "Releasing v$new (previous: ${last:-none})"
if [ "$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)" != "$new" ]; then
  for d in . host/backend host/vmm host/stream; do
    run sed -i "0,/^version *= *\".*\"/s//version = \"$new\"/" "$d/Cargo.toml"
    (cd "$d" && run cargo update -q -w --offline 2>/dev/null || run cargo update -q -w)
  done
  run git commit -q -m "Release v$new" -- "${VERSIONED[@]}"
fi
run git tag -a "v$new" -m "Conduit v$new"
run git push -q origin main "v$new"
echo "Pushed v$new. Packages: https://github.com/olealgoritme/conduit/actions/workflows/release.yml"

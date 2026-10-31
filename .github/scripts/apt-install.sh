#!/usr/bin/env bash
# apt-get install for CI runners, with a .deb cache and a mirror fallback:
#
#   .github/scripts/apt-install.sh PACKAGE...
#
# APT_ARCHIVES  directory that keeps the downloaded .deb files between runs
#               (an actions/cache path); empty: apt's own cache
# APT_TIMEOUT   seconds one index update or download pass may take before it
#               is cut off and retried on the next mirror (default 600)
#
# The runner's apt sources point at the Azure mirror, which is fast but has
# been slow enough (~100 KB/s) to run a job into its timeout. Each pass is
# capped; when one fails or runs out of time, the sources switch to
# archive.ubuntu.com and the pass is retried. With a warm cache only the
# package index is fetched.
set -euo pipefail

[ $# -gt 0 ] || { echo "usage: apt-install.sh PACKAGE..." >&2; exit 2; }
archives=${APT_ARCHIVES:-}
limit=${APT_TIMEOUT:-600}

opts=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30)
if [ -n "$archives" ]; then
  mkdir -p "$archives/partial"
  opts+=(-o "Dir::Cache::archives=$archives")
fi

asroot() { sudo DEBIAN_FRONTEND=noninteractive "$@"; }

switched=0
use_main_archive() {
  [ "$switched" = 0 ] || return 1
  switched=1
  echo "::warning::apt: switching from the Azure mirror to archive.ubuntu.com"
  local f
  for f in /etc/apt/sources.list /etc/apt/sources.list.d/* /etc/apt/apt-mirrors.txt; do
    [ -f "$f" ] || continue
    sudo sed -i -E 's#https?://azure\.archive\.ubuntu\.com/ubuntu/?#http://archive.ubuntu.com/ubuntu/#g' "$f"
  done
}

# pass NAME CMD...: run CMD under the time limit, once more on the main
# archive if it fails
pass() {
  local name=$1
  shift
  local start=$SECONDS
  if asroot timeout "$limit" "$@"; then
    echo "apt: $name took $((SECONDS - start)) s"
    return 0
  fi
  echo "::warning::apt: $name failed or took more than $limit s"
  use_main_archive || return 1
  # a download pass needs the new mirror's index first
  [ "$name" = "index update" ] || asroot timeout "$limit" apt-get "${opts[@]}" update -q
  asroot timeout "$limit" "$@"
}

pass "index update" apt-get "${opts[@]}" update -q
pass "download" apt-get "${opts[@]}" install -y -q --download-only "$@"
asroot apt-get "${opts[@]}" install -y -q "$@"

if [ -n "$archives" ]; then
  # Drop .debs no longer in the index, so the cache does not grow with every
  # package update, and let actions/cache (the runner user) read the rest.
  asroot apt-get "${opts[@]}" autoclean -q
  sudo rm -rf "$archives/partial" "$archives/lock"
  sudo chown -R "$(id -u):$(id -g)" "$archives"
  du -sh "$archives"
fi

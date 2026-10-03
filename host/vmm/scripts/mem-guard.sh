# Sourced by the scripts that start guests. Refuses to start guests whose RAM
# does not fit in what the host has available, with a margin kept free.
#
#   mem_guard TOTAL_MIB WHAT
#
# Guest RAM is a shared memfd the OOM killer cannot reap: N guests that touch
# their RAM need N x MEM for real, and running the host out of it takes the
# desktop down too. MEM_GUARD_MARGIN_MIB (default 4096) is kept free;
# MEM_GUARD=off skips the check.
mem_guard() {
  local need=$1 what=$2 margin=${MEM_GUARD_MARGIN_MIB:-4096} avail
  [[ ${MEM_GUARD:-on} == off ]] && return 0
  avail=$(awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo)
  [[ -n $avail ]] || return 0
  if (( need + margin > avail )); then
    echo "refusing to start $what: needs ${need} MiB + ${margin} MiB margin, host has ${avail} MiB available." >&2
    echo "Use fewer/smaller guests, free memory, or MEM_GUARD=off to override." >&2
    exit 1
  fi
}

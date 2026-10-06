# Sourced by the scripts in this directory.
#   WT  the git worktree root (default: the one containing this file)
#   KW  $WT/guest/windows
#   S   scratch directory for copies, targets and logs (default: $TMPDIR/kmd-dev-scratch)
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="${WT:-$(cd "$HERE/../../../.." && pwd)}"
KW="$WT/guest/windows"
S="${S:-${TMPDIR:-/tmp}/kmd-dev-scratch}"
mkdir -p "$S"

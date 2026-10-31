#!/bin/bash
# night2.sh: the second-pass run (level 1+3 and level 1+2+3 only) on a guest
# that has seen night.sh before. Same script, fewer steps; see night.sh.
exec env NIGHT_STEPS="${NIGHT_STEPS:-l13 l123}" NIGHT_TAG="${NIGHT_TAG:--pass2}" \
	bash "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/night.sh" "$@"

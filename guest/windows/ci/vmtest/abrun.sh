#!/bin/bash
# abrun.sh "LABEL DWM [K=V ...]" ... : run A/B rows one after another
T=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/t
for row in "$@"; do echo "=== ROW $row"; bash $T/abrow.sh $row 2>&1 | tail -12; done

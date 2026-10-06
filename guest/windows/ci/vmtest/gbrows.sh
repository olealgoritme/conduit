#!/bin/bash
T=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/t
run(){ L=$1; shift; echo "=== $L $*"; bash $T/blrow.sh "$L" "" "$@" 2>&1 | tail -22; }
run g0 GuestBlob=0
run g1 GuestBlob=1
run g1a GuestBlob=1 BltAsync=1 ForeignCopy=1

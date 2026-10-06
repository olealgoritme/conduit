#!/usr/bin/env bash
# Builds the stage4 host set from main (merged tree) and stages it.
set -euo pipefail
W=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/main-merge
ST=${VMTEST_DIR:-$HOME/.cache/conduit-vmtest}/host-deploy-stage4
R() { systemd-run --user --scope -q -p MemoryMax=6G taskset -c 0-7,16-23 nice -n 19 "$@"; }
cd $W
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "FAIL: HEAD is not origin/main"; exit 1; }
[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "FAIL: dirty tree"; exit 1; }
[ -e host/venus/third_party/virglrenderer/meson.build ] || { echo "FAIL: submodule"; exit 1; }
echo "== virglrenderer"; R env JOBS=4 host/venus/build-virglrenderer.sh 2>&1 | grep -E "applied|error|FAIL|Installing.*\.so\.1\.|failed" || true
nm -D host/venus/third_party/build/install/lib/libvirglrenderer.so.1 | grep -q virgl_renderer_resource_import_host_ptr || { echo "FAIL: lib lacks patch 0002"; exit 1; }
echo "== conduit-venus"; (cd host/venus && R env CONDUIT_VENUS_RPATH='$ORIGIN/../lib' cargo build --locked --release -j4 --features renderer --bin conduit-venus 2>&1 | tail -1)
echo "== backend"; (cd host/backend && R cargo build --locked --release -j4 -p device --features vhost-user,venus --bin conduit-backend --bin conduit-userspace 2>&1 | tail -1)
echo "== cli"; R cargo build --locked --release -j4 --bin conduit 2>&1 | tail -1
mkdir -p $ST
install -m755 host/venus/target/release/conduit-venus $ST/conduit-venus
install -m644 "$(readlink -f host/venus/third_party/build/install/lib/libvirglrenderer.so.1)" $ST/libvirglrenderer.so.1
install -m755 host/backend/target/release/conduit-backend $ST/conduit-backend
install -m755 host/backend/target/release/conduit-userspace $ST/conduit-userspace
install -m755 target/release/conduit $ST/conduit
echo "main $(git rev-parse HEAD) (host-scanout-reset + host-guest-blob + umd-nvk-combined merged; virglrenderer $(git -C host/venus/third_party/virglrenderer rev-parse --short HEAD) + patches 0001 0002)" > $ST/BUILD
(cd $ST && sha256sum conduit conduit-backend conduit-userspace conduit-venus libvirglrenderer.so.1 > SHA256SUMS)
ls -la $ST; echo BUILD-DONE

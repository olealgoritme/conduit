# Conduit: one entry point for building, testing and packaging.
#
#   make              build everything for development (backend, VM runner,
#                     CLI, viewer, guest module, bundled QEMU)
#   make test         run all offline tests (no GPU, no VMs)
#   make install      install to /opt/conduit (+ /usr/local/bin/conduit)
#   make package      build every package (.deb/.rpm/Arch/tarball/guest) into dist/out
#   make deb          just the .deb (also: rpm, archlinux, tarball, guest-deb, guest-rpm, guest-arch)
#   make install-deb  build the .deb and install it on this machine
#   make deps         install build dependencies (asks for sudo)
#   make clean        remove build outputs
#   make release      tag the next patch version and let GitHub build packages
#                     (release-minor / release-major / packaging/release.sh X.Y.Z)
#
# Single parts: make backend | vmm | cli | viewer | guest | qemu | stream
# The guest module builds against this machine's kernel; KDIR=... overrides.

CARGO  ?= cargo
KDIR   ?= /lib/modules/$(shell uname -r)/build
JOBS   ?= $(shell nproc)
# Tests that open the real /dev/nvidiactl; kept out of `make test`. The
# feature sets match CI (.github/workflows/ci.yml).
GPU_TESTS := --skip for_real --skip closing_the_fd --skip repeated_map_unmap

.PHONY: all backend vmm cli viewer guest qemu stream test install package deps clean help dist-stage deb rpm archlinux tarball guest-deb guest-rpm guest-arch install-deb release release-minor release-major

all: backend vmm cli viewer guest qemu stream
	@echo
	@echo "Built. Try: ./target/release/conduit doctor"

# Same features as the packages (packaging/build.sh BACKEND_FEATURES).
backend:
	cd host/backend && $(CARGO) build --release -p device --features vhost-user,venus --bins

vmm:
	cd host/vmm && $(CARGO) build --release --no-default-features

cli:
	$(CARGO) build --release -p conduit

viewer:
	$(MAKE) -C host/viewer -j$(JOBS)

# The network stream host (Moonlight, conduit link); docs/STREAMING.md.
stream:
	cd host/stream && $(CARGO) build --release

guest:
	$(MAKE) -C guest/linux KDIR=$(KDIR)

# Downloads, verifies and patches QEMU 11.1 once; later runs are incremental.
qemu:
	@if [ -x host/qemu/build/qemu-system-x86_64 ]; then \
		echo "qemu: already built (host/qemu/build); rm -rf host/qemu/build to rebuild"; \
	else nice -n 10 host/qemu/build-qemu.sh; fi

test:
	$(CARGO) test -p conduit
	cd host/backend && $(CARGO) test --workspace --features device/vhost-user,device/venus -- $(GPU_TESTS)
	cd host/vmm && $(CARGO) test --workspace --no-default-features
	cd host/venus && $(CARGO) test
	cd host/stream && $(CARGO) test
	packaging/test/icons.sh
	$(MAKE) -C host/viewer check

# Builds the release tarball and runs its installer (the same path users take).
install:
	packaging/build.sh package tarball
	rm -rf dist/install && mkdir -p dist/install
	tar -xzf $$(ls -t dist/out/conduit-*-x86_64-linux.tar.gz | head -1) -C dist/install
	sudo dist/install/conduit/install.sh

# Full package pipeline (same steps as the GitHub release), one format each.
# `venus` needs the host/venus/third_party submodules checked out.
dist-stage:
	packaging/build.sh rust
	packaging/build.sh viewer
	packaging/build.sh stream
	packaging/build.sh venus
	packaging/build.sh qemu
	packaging/build.sh stage

deb rpm archlinux tarball: dist-stage
	packaging/build.sh package $@
	@ls -t dist/out/ | head -3

guest-deb guest-rpm guest-arch:
	packaging/build.sh package $@
	@ls -t dist/out/ | head -3

# Every package format at once, into dist/out/.
package: dist-stage
	for f in deb rpm archlinux tarball guest-deb guest-rpm guest-arch; do packaging/build.sh package $$f || exit 1; done
	@ls -t dist/out/

# Build the .deb and install it on this machine (Ubuntu/Debian).
install-deb: deb
	sudo apt install -y ./$$(ls -t dist/out/conduit_*_amd64.deb | head -1)
	conduit --version

deps:
	sudo packaging/build.sh deps

clean:
	$(CARGO) clean
	cd host/backend && $(CARGO) clean
	cd host/vmm && $(CARGO) clean
	cd host/stream && $(CARGO) clean
	$(MAKE) -C host/viewer clean
	$(MAKE) -C guest/linux clean KDIR=$(KDIR)
	rm -rf dist

help:
	@awk 'NF==0{exit} {print}' Makefile

release:
	packaging/release.sh patch

release-minor:
	packaging/release.sh minor

release-major:
	packaging/release.sh major

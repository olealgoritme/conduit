# Conduit: one entry point for building, testing and packaging.
#
#   make              build everything for development (backend, VM runner,
#                     CLI, viewer, guest module, bundled QEMU)
#   make test         run all offline tests (no GPU, no VMs)
#   make install      install to /opt/conduit (+ /usr/local/bin/conduit)
#   make package      build .deb/.rpm/Arch/tarball packages into dist/out
#   make deps         install build dependencies (asks for sudo)
#   make clean        remove build outputs
#
# Single parts: make backend | vmm | cli | viewer | guest | qemu
# The guest module builds against this machine's kernel; KDIR=... overrides.

CARGO  ?= cargo
KDIR   ?= /lib/modules/$(shell uname -r)/build
JOBS   ?= $(shell nproc)
# Tests that open the real /dev/nvidiactl; kept out of `make test`.
GPU_TESTS := --skip for_real --skip closing_the_fd --skip repeated_map_unmap

.PHONY: all backend vmm cli viewer guest qemu test install package deps clean help

all: backend vmm cli viewer guest qemu
	@echo
	@echo "Built. Try: ./target/release/conduit doctor"

backend:
	cd host/backend && $(CARGO) build --release -p device --features vhost-user --bins

vmm:
	cd host/vmm && $(CARGO) build --release --no-default-features

cli:
	$(CARGO) build --release -p conduit

viewer:
	$(MAKE) -C host/viewer -j$(JOBS)

guest:
	$(MAKE) -C guest/linux KDIR=$(KDIR)

# Downloads, verifies and patches QEMU 11.1 once; later runs are incremental.
qemu:
	@if [ -x host/qemu/build/qemu-system-x86_64 ]; then \
		echo "qemu: already built (host/qemu/build); rm -rf host/qemu/build to rebuild"; \
	else nice -n 10 host/qemu/build-qemu.sh; fi

test:
	$(CARGO) test -p conduit
	cd host/backend && $(CARGO) test --workspace --features device/vhost-user -- $(GPU_TESTS)
	cd host/vmm && $(CARGO) test --workspace --no-default-features
	$(MAKE) -C host/viewer check

# Builds the release tarball and runs its installer (the same path users take).
install:
	packaging/build.sh package tarball
	rm -rf dist/install && mkdir -p dist/install
	tar -xzf $$(ls -t dist/out/conduit-*-x86_64-linux.tar.gz | head -1) -C dist/install
	sudo dist/install/conduit/install.sh

package:
	packaging/build.sh package deb
	packaging/build.sh package rpm
	packaging/build.sh package archlinux
	packaging/build.sh package tarball
	packaging/build.sh package guest-deb
	packaging/build.sh package guest-rpm

deps:
	sudo packaging/build.sh deps

clean:
	$(CARGO) clean
	cd host/backend && $(CARGO) clean
	cd host/vmm && $(CARGO) clean
	$(MAKE) -C host/viewer clean
	$(MAKE) -C guest/linux clean KDIR=$(KDIR)
	rm -rf dist

help:
	@sed -n '1,14p' Makefile

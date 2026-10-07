# Two ways to build this module.
#
# In-tree (how libkrunfw builds it), selected by CONFIG_CONDUIT_GPU:
obj-$(CONFIG_CONDUIT_GPU) += conduit_gpu.o

# Where the PCI core reads a device's driver_override: struct pci_dev's own
# field on older kernels, struct device's on newer ones (and distributions
# backport the latter alongside the former). Asked of the header rather than
# the version for that reason. See nvgpu_pci_forbid_drivers().
ifneq ($(shell grep -sE 'const char[[:space:]]+\*driver_override;' $(srctree)/include/linux/pci.h),)
ccflags-y += -DNVGPU_PCI_DEV_DRIVER_OVERRIDE
endif

# Out-of-tree, for loading into a test guest without rebuilding its kernel:
#
#	make -C guest/linux KDIR=/lib/modules/$(uname -r)/build
#
# KDIR must be the build tree of the kernel the module will be *loaded* into,
# which for a guest test is the guest's kernel, not the host's.
KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

.PHONY: module clean check
module:
	$(MAKE) -C $(KDIR) M=$(PWD) CONFIG_CONDUIT_GPU=m modules

# Unit tests of the parts that are plain C (nvgpu_pcimap.h, nvgpu_devinfo.h),
# on the build host.
HOSTCC ?= cc
check:
	$(HOSTCC) -std=gnu11 -Wall -Wextra -Werror -O2 -fsanitize=address,undefined \
		-o test/pcimap_test test/pcimap_test.c
	./test/pcimap_test
	$(HOSTCC) -std=gnu11 -Wall -Wextra -Werror -O2 -fsanitize=address,undefined \
		-o test/devinfo_test test/devinfo_test.c
	./test/devinfo_test

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
	rm -f test/pcimap_test test/devinfo_test

# Two ways to build this module.
#
# In-tree (how libkrunfw builds it), selected by CONFIG_CONDUIT_GPU:
obj-$(CONFIG_CONDUIT_GPU) += conduit_gpu.o

# Out-of-tree, for loading into a test guest without rebuilding its kernel:
#
#	make -C guest/linux KDIR=/lib/modules/$(uname -r)/build
#
# KDIR must be the build tree of the kernel the module will be *loaded* into,
# which for a guest test is the guest's kernel, not the host's.
KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

.PHONY: module clean
module:
	$(MAKE) -C $(KDIR) M=$(PWD) CONFIG_CONDUIT_GPU=m modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

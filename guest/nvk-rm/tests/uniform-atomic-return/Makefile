# SPDX-License-Identifier: Apache-2.0
GLSLANG ?= glslangValidator
CC ?= cc
MINGW ?= x86_64-w64-mingw32-gcc
CFLAGS ?= -std=c11 -O2 -Wall -Wextra -Wpedantic
# Vulkan headers live in /usr/include/vulkan on the build host.  Search them
# after MinGW's own headers so a cross build does not pick Linux libc headers.
VULKAN_INC ?= -idirafter /usr/include

.PHONY: all windows clean

all: uniform_atomics.spv uniform_atomics_test

uniform_atomics.spv: shader.comp
	$(GLSLANG) -V --target-env vulkan1.1 -o $@ $<

uniform_atomics_test: uniform_atomics_test.c
	$(CC) $(CFLAGS) -o $@ $< -lvulkan

windows: uniform_atomics.spv uniform_atomics_test.exe

uniform_atomics_test.exe: uniform_atomics_test.c
	$(MINGW) $(CFLAGS) $(VULKAN_INC) -o $@ $<

clean:
	rm -f uniform_atomics.spv uniform_atomics_test uniform_atomics_test.exe

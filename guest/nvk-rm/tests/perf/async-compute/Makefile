# SPDX-License-Identifier: MIT
# librmtrace.so (RM ioctl trace shim) and vkqtest (Vulkan queue overlap test).
#   make                     builds both; names.h gets generated empty
#   make OGKM=/path/to/open-gpu-kernel-modules   class/control names in the trace
CC ?= cc
CFLAGS ?= -O2 -Wall
GLSLANG ?= glslangValidator
PYTHON ?= python3
OGKM ?=

all: librmtrace.so vkqtest

names.h:
	@if [ -n "$(OGKM)" ]; then $(PYTHON) gen-names.py "$(OGKM)" > $@; \
	else printf '%s\n' '/* no names: make OGKM=<open-gpu-kernel-modules> names.h */' \
	  'static const struct { unsigned v; const char *n; } cls_names[] = { {0, "?"} };' \
	  'static const struct { unsigned v; const char *n; } ctl_names[] = { {0, "?"} };' > $@; fi

librmtrace.so: rmtrace.c names.h
	$(CC) $(CFLAGS) -shared -fPIC -o $@ rmtrace.c -ldl -lpthread

glsl/%.spv: glsl/%
	$(GLSLANG) -V --target-env vulkan1.2 -o $@ $< > /dev/null

shaders.h: glsl/spin.comp.spv glsl/tri.vert.spv glsl/spin.frag.spv gen-shaders.py
	$(PYTHON) gen-shaders.py $(filter %.spv,$^) > $@

vkqtest: vkqtest.c shaders.h
	$(CC) $(CFLAGS) -o $@ vkqtest.c -lvulkan -ldl -lm

clean:
	rm -f librmtrace.so vkqtest shaders.h glsl/*.spv

.PHONY: all clean

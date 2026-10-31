# Windows test programs for nvtrace, cross-compiled with MinGW-w64.
#   make -C testapps           build ../dist/{vktest,d3d11test,d3d12test,cudatest}.exe
#   make -C testapps shaders   regenerate vktest_comp.h from vktest.comp (needs glslangValidator)
#   make -C testapps clean
# vulkan-1.dll, nvcuda.dll, d3d11/d3d12/dxgi.dll and d3dcompiler_47.dll are all
# loaded at run time; the executables import only kernel32, user32 and msvcrt.

ifeq ($(origin CC),default)
CC      := x86_64-w64-mingw32-gcc
endif
GLSLANG ?= glslangValidator
DIST    := ../dist
CFLAGS  := -O2 -Wall -Wextra -D__USE_MINGW_ANSI_STDIO=1 -D_WIN32_WINNT=0x0A00 -Iinclude
LDFLAGS := -static
DXLIBS  := -luser32 -ldxguid -luuid

PROGS := $(DIST)/vktest.exe $(DIST)/d3d11test.exe $(DIST)/d3d12test.exe $(DIST)/cudatest.exe

all: $(PROGS)

$(DIST):
	mkdir -p $@

$(DIST)/vktest.exe: vktest.c vktest_comp.h common.h | $(DIST)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

$(DIST)/d3d11test.exe: d3d11test.c common.h | $(DIST)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(DXLIBS)

$(DIST)/d3d12test.exe: d3d12test.c common.h | $(DIST)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS) $(DXLIBS)

$(DIST)/cudatest.exe: cudatest.c common.h | $(DIST)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

# Not a dependency of the exe, so a checkout without glslangValidator still builds.
shaders:
	$(GLSLANG) -V --vn vktest_comp_spv -o vktest_comp.h vktest.comp

clean:
	rm -f $(PROGS)

.PHONY: all shaders clean

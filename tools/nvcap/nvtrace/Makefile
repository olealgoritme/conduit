# nvtrace: cross-build on Linux with MinGW-w64.
#   make            build everything into dist/
#   make test       host unit tests (decoder, x86 relocation, log decoder, WDK layout check)
#   make wine-test  run nvinject/nvtrace64 under Wine: selftest, -t, kill, -p, --watch
#   make clean

CC64 := x86_64-w64-mingw32-gcc
CC32 := i686-w64-mingw32-gcc
HOSTCC ?= gcc
PYTHON ?= python3

CFLAGS := -O2 -Wall -Wextra -std=gnu11 -fno-strict-aliasing
CFLAGS32 := $(CFLAGS) -fno-omit-frame-pointer -mincoming-stack-boundary=2
LDFLAGS := -static -static-libgcc -s
DLLFLAGS := -shared -Wl,--exclude-all-symbols

DIST := dist
CORE_SRC := src/nvtrace.c src/hook.c src/log.c src/x86dec.c src/gpumem.c src/devio.c
CORE_HDR := src/nvtrace.h src/ntr_format.h src/layouts_gen.h src/x86dec.h

TOOLS := $(DIST)/ntrdump.py $(DIST)/nvt_layouts.py $(DIST)/rmnames.json $(DIST)/nvclasses.json

all: $(DIST)/nvtrace64.dll $(DIST)/nvtrace32.dll $(DIST)/nvinject.exe $(DIST)/nvinject32.exe \
     $(TOOLS) testapps

$(DIST):
	mkdir -p $@

src/layouts_gen.h: tools/gen_layouts.py tools/nvt_layouts.py
	$(PYTHON) tools/gen_layouts.py > $@.tmp && mv $@.tmp $@

$(DIST)/nvtrace64.dll: $(CORE_SRC) src/asm64.S $(CORE_HDR) Makefile | $(DIST)
	$(CC64) $(CFLAGS) $(DLLFLAGS) -o $@ $(CORE_SRC) src/asm64.S $(LDFLAGS) -ladvapi32

$(DIST)/nvtrace32.dll: $(CORE_SRC) src/asm32.S $(CORE_HDR) Makefile | $(DIST)
	$(CC32) $(CFLAGS32) $(DLLFLAGS) -Wl,--kill-at -o $@ $(CORE_SRC) src/asm32.S $(LDFLAGS) -ladvapi32

INJFLAGS := -municode -D_WIN32_WINNT=0x0A00
INJLIBS := -lshell32 -lpsapi -ladvapi32

$(DIST)/nvinject.exe: src/nvinject.c | $(DIST)
	$(CC64) $(CFLAGS) $(INJFLAGS) -o $@ $< $(LDFLAGS) $(INJLIBS)

$(DIST)/nvinject32.exe: src/nvinject.c | $(DIST)
	$(CC32) $(CFLAGS32) $(INJFLAGS) -o $@ $< $(LDFLAGS) $(INJLIBS)

$(DIST)/ntrdump.py: tools/ntrdump.py | $(DIST)
	cp $< $@
$(DIST)/nvt_layouts.py: tools/nvt_layouts.py | $(DIST)
	cp $< $@
$(DIST)/rmnames.json: tools/rmnames.json | $(DIST)
	cp $< $@
$(DIST)/nvclasses.json: tools/nvclasses.json | $(DIST)
	cp $< $@

testapps:
	$(MAKE) -C testapps

# host tests
build/test_x86dec: tests/test_x86dec.c src/x86dec.c src/x86dec.h
	mkdir -p build
	$(HOSTCC) -O1 -g -Wall -Wextra -Isrc -o $@ tests/test_x86dec.c src/x86dec.c

test: build/test_x86dec
	./build/test_x86dec
	$(PYTHON) -m unittest discover -s tests -p 'test_*.py' -v
	$(PYTHON) tools/check_layouts_wdk.py; rc=$$?; [ $$rc = 0 ] || [ $$rc = 77 ]

# x64 tools under Wine (no display, throw-away prefix); skipped without wine64
wine-test: all
	tests/wine_smoke.sh $(DIST); rc=$$?; [ $$rc = 0 ] || [ $$rc = 77 ]

clean:
	rm -rf build $(DIST)/*.dll $(DIST)/nvinject*.exe $(TOOLS)
	$(MAKE) -C testapps clean

.PHONY: all test wine-test clean testapps

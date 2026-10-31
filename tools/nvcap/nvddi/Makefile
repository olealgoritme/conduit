# nvddi: build on Linux with mingw-w64.  See README.md.
#
#   make            generate + build build/nvddi.sys and build/nvddictl.exe
#   make check      layout cross-check (clang MSVC ABI vs gcc) + decoder self-test
#   make sign       test-sign build/nvddi.sys -> build/nvddi.signed.sys (sign/sign.sh)
#   make dist       collect installable files in dist/

WDK     ?= $(HOME)/.local/share/conduit-dev/wdk-10.0.26100.0
CC      := x86_64-w64-mingw32-gcc
DLLTOOL := x86_64-w64-mingw32-dlltool
CLANG   ?= clang
PY      ?= python3
NICE    ?= nice -n 10

B   := build
GEN := $(B)/gen
MINGW_DDK := /usr/x86_64-w64-mingw32/include/ddk

CFLAGS := -O2 -std=gnu11 -fms-extensions -Wall -Wno-unknown-pragmas -Wno-missing-braces \
          -Wno-unused-function -Werror=implicit-function-declaration -Werror=int-conversion \
          -Werror=incompatible-pointer-types \
          -ffreestanding -fno-stack-protector -fno-stack-check -mno-stack-arg-probe -mno-red-zone \
          -fno-strict-aliasing -fno-common -fno-plt \
          -Iinclude -Idriver -I$(GEN) -isystem $(GEN)/wdkinc -isystem $(MINGW_DDK)

LDFLAGS := -nostdlib -Wl,--subsystem,native -Wl,--entry,DriverEntry \
           -Wl,--image-base,0x140000000 -Wl,--dynamicbase -Wl,--nxcompat -Wl,--high-entropy-va \
           -Wl,--file-alignment,0x200 -Wl,--section-alignment,0x1000 \
           -Wl,--major-os-version,10 -Wl,--minor-os-version,0 \
           -Wl,--major-subsystem-version,10 -Wl,--minor-subsystem-version,0 \
           -Wl,--disable-runtime-pseudo-reloc -Wl,--disable-auto-import -Wl,--no-insert-timestamp

SRCS := driver/main.c driver/filter.c driver/capture.c driver/ring.c driver/writer.c driver/util.c \
        driver/registry.c driver/gpuva.c driver/imports.c
OBJS := $(patsubst driver/%.c,$(B)/obj/%.o,$(SRCS)) $(B)/obj/nvddi_gen.o

all: $(B)/nvddi.sys $(B)/nvddictl.exe

$(GEN)/nvddi_gen.c $(GEN)/nvddi_gen.h $(GEN)/ndl_layouts.json: gen/gen.py gen/rules.py
	@mkdir -p $(GEN)
	$(NICE) $(PY) gen/gen.py --wdk $(WDK) --out $(GEN) --cc $(CC) --clang $(CLANG)

$(B)/obj/%.o: driver/%.c driver/nvddi.h include/ndl_format.h include/nvddi_ioctl.h $(GEN)/nvddi_gen.h
	@mkdir -p $(B)/obj
	$(NICE) $(CC) $(CFLAGS) -c $< -o $@

$(B)/obj/nvddi_gen.o: $(GEN)/nvddi_gen.c driver/nvddi.h $(GEN)/nvddi_gen.h
	@mkdir -p $(B)/obj
	$(NICE) $(CC) $(CFLAGS) -Wno-unused-variable -c $< -o $@

$(B)/libnvddi_ntoskrnl.a: driver/ntoskrnl.def
	@mkdir -p $(B)
	$(DLLTOOL) -d $< -l $@ -k

$(B)/nvddi.sys: $(OBJS) $(B)/libnvddi_ntoskrnl.a tools/pefix.py
	$(NICE) $(CC) $(LDFLAGS) -o $(B)/nvddi.unstripped.sys $(OBJS) $(B)/libnvddi_ntoskrnl.a
	x86_64-w64-mingw32-strip -o $@ $(B)/nvddi.unstripped.sys
	$(PY) tools/pefix.py --driver $@

$(B)/nvddictl.exe: tools/nvddictl.c include/nvddi_ioctl.h
	@mkdir -p $(B)
	$(NICE) $(CC) -O2 -Wall -municode -Iinclude -o $@ $< -s

check: $(GEN)/nvddi_gen.c
	$(PY) gen/layoutcheck.py --gen $(GEN) --cc $(CC) --clang $(CLANG)
	$(PY) decoder/selftest.py

sign: $(B)/nvddi.sys
	sign/sign.sh $(B)/nvddi.sys $(B)/nvddi.signed.sys

dist: all
	@mkdir -p dist
	cp $(B)/nvddi.sys $(B)/nvddictl.exe dist/
	@if [ -f $(B)/nvddi.signed.sys ]; then cp $(B)/nvddi.signed.sys dist/nvddi.sys; fi
	@if [ -f sign/out/nvddi-test.cer ]; then cp sign/out/nvddi-test.cer dist/; fi
	cp install/*.cmd install/*.ps1 dist/ 2>/dev/null || true
	cp $(GEN)/ndl_layouts.json decoder/ndl_layouts.json

clean:
	rm -rf $(B)

.PHONY: all check sign dist clean

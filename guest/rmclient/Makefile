# SPDX-License-Identifier: MIT
# Plain-make build for hosts without meson (e.g. a bare guest VM).
#   make            librmclient.a, librmclient.so, test_unit, crm_smoke
#   make check      run the unit tests (no GPU needed)
#   make smoke      run crm_smoke (needs /dev/nvidiactl)

CC      ?= cc
CFLAGS  ?= -O2 -g
XCFLAGS := -std=c11 -Wall -Wextra -Wpedantic -fPIC -pthread -Iinclude -Isrc $(CFLAGS)
XLDFLAGS := -pthread $(LDFLAGS)
O       ?= build-make

SRCS := src/rmclient.c src/status.c src/transport_linux.c src/transport_windows.c
OBJS := $(SRCS:src/%.c=$(O)/%.o)

all: $(O)/librmclient.a $(O)/librmclient.so $(O)/test_unit $(O)/test_win_wire $(O)/crm_smoke $(O)/crm_event_smoke $(O)/crm_pin_smoke $(O)/crm_scanout_smoke $(O)/crm_share_smoke $(O)/crm_bench

$(O):
	mkdir -p $(O)

$(O)/%.o: src/%.c include/rmclient.h include/rmclient_transport.h src/nv_ioctl_defs.h src/crm_mutex.h | $(O)
	$(CC) $(XCFLAGS) -c $< -o $@

$(O)/librmclient.a: $(OBJS)
	$(AR) rcs $@ $^

$(O)/librmclient.so: $(OBJS)
	$(CC) -shared $(XLDFLAGS) -o $@ $^

$(O)/test_unit: tests/test_unit.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

$(O)/test_win_wire: tests/test_win_wire.c src/win_wire.h src/win_gen.h src/helios_nvrm_escape.h | $(O)
	$(CC) $(XCFLAGS) $< -o $@

$(O)/crm_smoke: tests/crm_smoke.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

$(O)/crm_event_smoke: tests/crm_event_smoke.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

$(O)/crm_pin_smoke: tests/crm_pin_smoke.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

$(O)/crm_scanout_smoke: tests/crm_scanout_smoke.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

$(O)/crm_share_smoke: tests/crm_share_smoke.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

$(O)/crm_bench: tests/crm_bench.c $(O)/librmclient.a
	$(CC) $(XCFLAGS) $< $(O)/librmclient.a $(XLDFLAGS) -o $@

check: $(O)/test_unit $(O)/test_win_wire
	$(O)/test_unit
	$(O)/test_win_wire

smoke: $(O)/crm_smoke
	$(O)/crm_smoke

clean:
	rm -rf $(O)

.PHONY: all check smoke clean

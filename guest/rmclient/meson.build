# SPDX-License-Identifier: MIT
#
# Linux:   meson setup build && ninja -C build && meson test -C build
# Windows: cross from Linux with MinGW-w64 (see README.md):
#   meson setup build-win --cross-file ../nvk-rm/windows/mingw-x86_64.ini
#   ninja -C build-win      -> librmclient.dll (+ import library)
project('rmclient', 'c',
  version : '0.1.0',
  license : 'MIT',
  default_options : ['c_std=c11', 'warning_level=3', 'buildtype=debugoptimized'])

cc = meson.get_compiler('c')
is_windows = host_machine.system() == 'windows'

# Linux: C11 <threads.h>. Windows: SRW locks (src/crm_mutex.h), no pthreads.
deps = is_windows ? [] : [dependency('threads')]

inc = include_directories('include')

# The Windows transport talks to the Conduit KMD through D3DKMTEscape. mingw-w64
# ships no D3DKMT headers, so it builds against the vendored WDK ones the Helios
# ICD also uses (system include: they carry MSVC pragmas).
wdk_inc = is_windows ? include_directories(
  '../windows/icd/win-build/wdk-include', is_system : true) : []
# helios_kmdmap.h: the KMD-view loss table shared with the Venus ICD and the UMD
kmdmap_inc = is_windows ? include_directories('../windows/umd_common/bridge') : []

rmclient_lib = library('rmclient',
  'src/rmclient.c', 'src/status.c', 'src/transport_linux.c',
  'src/transport_windows.c',
  include_directories : [inc, include_directories('src'), wdk_inc, kmdmap_inc],
  dependencies : deps,
  gnu_symbol_visibility : 'default',
  # MinGW: no libgcc DLL next to librmclient.dll
  link_args : is_windows ? ['-static-libgcc'] : [],
  install : true)

rmclient_dep = declare_dependency(
  link_with : rmclient_lib,
  include_directories : inc)

install_headers('include/rmclient.h', 'include/rmclient_transport.h')

test_unit = executable('test_unit', 'tests/test_unit.c',
  include_directories : [inc, include_directories('src')],
  dependencies : [rmclient_dep] + deps)
test('unit', test_unit)

# The wire format the Windows transport speaks (src/win_wire.h): pure C, no OS
# calls, so it is checked on every host.
test_win_wire = executable('test_win_wire', 'tests/test_win_wire.c',
  include_directories : include_directories('src'))
test('win_wire', test_win_wire)

# Needs a real (or Conduit-forwarded) NVIDIA GPU: built, never run by `meson test`.
executable('crm_smoke', 'tests/crm_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
# NV_ESC_RM_GET_EVENT_DATA end to end (needs a backend and guest module that serve it).
executable('crm_event_smoke', 'tests/crm_event_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
# NV01_MEMORY_SYSTEM_OS_DESCRIPTOR over the caller's own pages (on Windows the KMD's PIN).
executable('crm_pin_smoke', 'tests/crm_pin_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
# Zero-copy present of RM video memory on a Windows guest (DRM node, GEM import,
# ScanoutFlip through the KMD); exits 77 elsewhere.
executable('crm_scanout_smoke', 'tests/crm_scanout_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
# RM semaphore-surface fences through the KMD (dxvk-on-nvk S4): fence create,
# EVENT_REGISTER on the fence handle, signal latency; exits 77 elsewhere.
executable('crm_semsurf_smoke', 'tests/crm_semsurf_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
# One RM memory object in two processes (NV_ESC_RM_DUP_OBJECT across clients),
# the base of shared surfaces between NVK processes on Windows.
executable('crm_share_smoke', 'tests/crm_share_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
# RM call latency (native /dev/nvidiactl vs. Conduit forwarding); needs a GPU.
executable('crm_bench', 'tests/crm_bench.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)

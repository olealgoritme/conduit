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

rmclient_lib = library('rmclient',
  'src/rmclient.c', 'src/status.c', 'src/transport_linux.c',
  'src/transport_windows.c',
  include_directories : [inc, include_directories('src'), wdk_inc],
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

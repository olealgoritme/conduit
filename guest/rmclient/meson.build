# SPDX-License-Identifier: MIT
project('rmclient', 'c',
  version : '0.1.0',
  license : 'MIT',
  default_options : ['c_std=c11', 'warning_level=3', 'buildtype=debugoptimized'])

cc = meson.get_compiler('c')
threads = dependency('threads')

inc = include_directories('include')

rmclient_lib = library('rmclient',
  'src/rmclient.c', 'src/status.c', 'src/transport_linux.c',
  include_directories : inc,
  dependencies : threads,
  gnu_symbol_visibility : 'default',
  install : true)

rmclient_dep = declare_dependency(
  link_with : rmclient_lib,
  include_directories : inc)

install_headers('include/rmclient.h', 'include/rmclient_transport.h')

test_unit = executable('test_unit', 'tests/test_unit.c',
  include_directories : [inc, include_directories('src')],
  dependencies : [rmclient_dep, threads])
test('unit', test_unit)

# Needs a real (or Conduit-forwarded) NVIDIA GPU: built, never run by `meson test`.
executable('crm_smoke', 'tests/crm_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)

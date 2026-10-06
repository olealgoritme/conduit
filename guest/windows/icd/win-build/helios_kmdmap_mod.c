/* Second module for helios_kmdmap_test.c: the same header compiled into a DLL,
 * standing in for helios_umd.dll next to the Venus ICD in one process. */
#include <stdio.h>
#include <windows.h>

#define HELIOS_KMDMAP_LOG(...)                                                \
   do {                                                                       \
      fprintf(stderr, "  mod log: ");                                         \
      fprintf(stderr, __VA_ARGS__);                                           \
      fputc('\n', stderr);                                                    \
   } while (0)
#include "../../umd_common/bridge/helios_kmdmap.h"

__declspec(dllexport) int32_t mod_attach(void);
__declspec(dllexport) void mod_detach(void);
__declspec(dllexport) void mod_register(const volatile void *va, uint64_t size);
__declspec(dllexport) void mod_unregister(const volatile void *va);
__declspec(dllexport) int mod_lost(int32_t epoch);
__declspec(dllexport) uint32_t mod_read(const volatile uint32_t *p);

int32_t mod_attach(void) { return helios_kmdmap_attach(); }
void mod_detach(void) { helios_kmdmap_detach(); }
void mod_register(const volatile void *va, uint64_t size) { helios_kmdmap_register(va, size, 100); }
void mod_unregister(const volatile void *va) { helios_kmdmap_unregister(va); }
int mod_lost(int32_t epoch) { return helios_kmdmap_lost(epoch); }
/* A read done by this module's code, as the UMD reads its ledger. */
uint32_t mod_read(const volatile uint32_t *p) { return *p; }

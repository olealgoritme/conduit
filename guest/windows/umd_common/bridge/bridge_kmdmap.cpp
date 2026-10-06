// The UMD's half of helios_kmdmap.h: KMD-made views that vanish when the KMD
// stops under a live process (the scanout read ledger here; the Venus ICD
// registers its ring, shmems, bos and producer page in the same table).
//
// One process-wide table and one handler protocol shared with the ICD, so the
// two modules never fight over the vectored handler: see the header. These
// C entry points are what `umd/src/scanout_acquire.rs` calls; nothing is
// exported from the DLL.
#include <stdarg.h>
#include <stdio.h>
#include <windows.h>

static void
helios_kmdmap_umd_log(const char *fmt, ...)
{
   char path[MAX_PATH];
   snprintf(path, sizeof(path), "C:\\ProgramData\\Helios\\helios_icd_diag.log");
   FILE *f = fopen(path, "a");
   if (!f)
      return;
   fprintf(f, "%lu pid=%lu umd ", (unsigned long)GetTickCount(),
           (unsigned long)GetCurrentProcessId());
   va_list ap;
   va_start(ap, fmt);
   vfprintf(f, fmt, ap);
   va_end(ap);
   fputc('\n', f);
   fclose(f);
}

#define HELIOS_KMDMAP_LOG(...) helios_kmdmap_umd_log(__VA_ARGS__)
#include "helios_kmdmap.h"

extern "C" {

int32_t
helios_kmdmap_c_attach(void)
{
   return helios_kmdmap_attach();
}

void
helios_kmdmap_c_detach(void)
{
   helios_kmdmap_detach();
}

void
helios_kmdmap_c_register(const void *va, uint64_t size, uint64_t owner)
{
   helios_kmdmap_register(va, size, owner);
}

void
helios_kmdmap_c_unregister_owner(uint64_t owner)
{
   helios_kmdmap_unregister_owner(owner);
}

bool
helios_kmdmap_c_lost(int32_t epoch)
{
   return helios_kmdmap_lost(epoch);
}

} // extern "C"

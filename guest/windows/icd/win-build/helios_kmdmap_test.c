/* Test for umd_common/bridge/helios_kmdmap.h (Windows, user mode only).
 *
 * Pagefile-backed section views stand in for the KMD's MmMapLockedPages views
 * and are unmapped under the process the way DxgkDdiDestroyDevice unmaps them
 * when the KMD stops, while threads keep using them like the Venus ring writer.
 * The same header is also compiled into a second module (helios_kmdmap_mod.c,
 * a DLL) to check that two modules share one table and do not fight over the
 * vectored handler — the ICD and the UMD in one dwm.exe.
 *
 *   x86_64-w64-mingw32-gcc -O2 -shared -o helios_kmdmap_mod.dll helios_kmdmap_mod.c
 *   x86_64-w64-mingw32-gcc -O2 -o helios_kmdmap_test.exe helios_kmdmap_test.c
 *   (i686-w64-mingw32-gcc likewise for WoW64)
 *
 * The Mesa copy of the header must stay byte-identical:
 *   cmp guest/windows/umd_common/bridge/helios_kmdmap.h \
 *       <mesa>/src/virtio/vulkan/helios_kmdmap.h
 */
#include <stdio.h>
#include <string.h>
#include <windows.h>

static volatile LONG log_lines;
#define HELIOS_KMDMAP_LOG(...)                                                \
   do {                                                                       \
      InterlockedIncrement(&log_lines);                                       \
      fprintf(stderr, "  log: ");                                             \
      fprintf(stderr, __VA_ARGS__);                                           \
      fputc('\n', stderr);                                                    \
   } while (0)
#include "../../umd_common/bridge/helios_kmdmap.h"

static int failures;
#define CHECK(cond)                                                           \
   do {                                                                       \
      if (!(cond)) {                                                          \
         fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);      \
         failures++;                                                          \
      }                                                                       \
   } while (0)

struct view {
   HANDLE section;
   volatile uint32_t *va;
   size_t size;
};

static struct view
view_map(size_t size)
{
   struct view v = { 0 };
   v.size = size;
   v.section = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE,
                                  0, (DWORD)size, NULL);
   v.va = MapViewOfFile(v.section, FILE_MAP_ALL_ACCESS, 0, 0, size);
   return v;
}

static void
view_unmap(struct view *v)
{
   UnmapViewOfFile((void *)v->va);
   CloseHandle(v->section);
}

static DWORD
state_of(const volatile void *p)
{
   MEMORY_BASIC_INFORMATION mbi;
   if (!VirtualQuery((const void *)p, &mbi, sizeof(mbi)))
      return 0;
   return mbi.State == MEM_COMMIT ? (mbi.Type << 1 | 1) : mbi.State;
}
#define PRIVATE_RW (MEM_PRIVATE << 1 | 1)
#define MAPPED_RW (MEM_MAPPED << 1 | 1)

struct writer {
   volatile uint32_t *shared;
   size_t size;
   int32_t epoch;
   volatile LONG running;
   LONG iterations_after_loss;
};

static DWORD WINAPI
writer_main(void *arg)
{
   struct writer *w = arg;
   uint8_t packet[256];
   memset(packet, 0xab, sizeof(packet));
   uint32_t cur = 0;
   InterlockedExchange(&w->running, 1);
   for (;;) {
      if (helios_kmdmap_lost(w->epoch)) {
         (void)w->shared[0]; /* a straggler touch must stay safe */
         w->iterations_after_loss++;
         break;
      }
      const uint32_t head = w->shared[0];
      const uint32_t off =
         64 + (cur % (uint32_t)(w->size - 64 - sizeof(packet)));
      memcpy((uint8_t *)w->shared + off, packet, sizeof(packet));
      cur += sizeof(packet) + (head & 1);
      w->shared[1] = cur;
   }
   return 0;
}

typedef int32_t (*mod_attach_fn)(void);
typedef void (*mod_detach_fn)(void);
typedef void (*mod_register_fn)(const volatile void *, uint64_t);
typedef void (*mod_unregister_fn)(const volatile void *);
typedef int (*mod_lost_fn)(int32_t);
typedef uint32_t (*mod_read_fn)(const volatile uint32_t *);

int
main(void)
{
   HMODULE mod = LoadLibraryA("helios_kmdmap_mod.dll");
   CHECK(mod != NULL);
   mod_attach_fn mod_attach = (mod_attach_fn)(void *)GetProcAddress(mod, "mod_attach");
   mod_detach_fn mod_detach = (mod_detach_fn)(void *)GetProcAddress(mod, "mod_detach");
   mod_register_fn mod_register =
      (mod_register_fn)(void *)GetProcAddress(mod, "mod_register");
   mod_unregister_fn mod_unregister =
      (mod_unregister_fn)(void *)GetProcAddress(mod, "mod_unregister");
   mod_lost_fn mod_lost = (mod_lost_fn)(void *)GetProcAddress(mod, "mod_lost");
   mod_read_fn mod_read = (mod_read_fn)(void *)GetProcAddress(mod, "mod_read");
   CHECK(mod_attach && mod_detach && mod_register && mod_unregister &&
         mod_lost && mod_read);

   /* "ICD" renderer in the exe, "UMD" device in the DLL: both attach. The
    * DLL attaches last, so its handler runs first. */
   const int32_t icd_epoch = helios_kmdmap_attach();
   const int32_t umd_epoch = mod_attach();
   CHECK(icd_epoch == umd_epoch);

   /* 1. KMD stop: ring views (exe) unmapped under writers; the UMD's ledger
    *    page (DLL) unmapped too and read by the DLL afterwards. */
   {
      struct view ring = view_map(0x21000);  /* 132 KiB like the Venus ring */
      struct view ring2 = view_map(0x21000); /* a second renderer's ring */
      struct view ledger = view_map(0x1000);
      struct view kept = view_map(0x10000);  /* still mapped: never claimed */
      CHECK(ring.va && ring2.va && ledger.va && kept.va);
      kept.va[0] = 0x1234;
      ledger.va[2] = 7; /* slot_count */
      helios_kmdmap_register(ring.va, ring.size, 1);
      helios_kmdmap_register(ring2.va, ring2.size, 2);
      helios_kmdmap_register(kept.va, kept.size, 1);
      mod_register(ledger.va, ledger.size);
      CHECK(mod_read(ledger.va + 2) == 7);

      struct writer ws[4];
      HANDLE threads[4];
      for (int i = 0; i < 4; i++) {
         ws[i] = (struct writer){ ring.va, ring.size, icd_epoch, 0, 0 };
         threads[i] = CreateThread(NULL, 0, writer_main, &ws[i], 0, NULL);
      }
      for (int i = 0; i < 4; i++)
         while (!ws[i].running)
            Sleep(1);
      Sleep(30);

      /* DestroyDevice unmaps everything; something then takes 4 KiB inside
       * ring2 (the 319.2 dwm.exe layout that made VirtualAlloc fail 487). */
      volatile uint32_t *ring2_va = ring2.va;
      view_unmap(&ledger);
      view_unmap(&ring2);
      void *squat = VirtualAlloc((uint8_t *)ring2_va + 0x10000, 0x1000,
                                 MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
      CHECK(squat == (uint8_t *)ring2_va + 0x10000);
      view_unmap(&ring);

      CHECK(WaitForMultipleObjects(4, threads, TRUE, 5000) == WAIT_OBJECT_0);
      for (int i = 0; i < 4; i++) {
         CHECK(ws[i].iterations_after_loss == 1);
         CloseHandle(threads[i]);
      }
      /* one loss loses every user, in both modules */
      CHECK(helios_kmdmap_lost(icd_epoch));
      CHECK(mod_lost(umd_epoch));
      CHECK(helios_kmdmap_t->epoch == icd_epoch + 1);
      /* the sweep backed the ledger before anyone touched it */
      CHECK(state_of(ledger.va) == PRIVATE_RW);
      CHECK(mod_read(ledger.va + 2) == 0);
      /* ring2: the free granules are ours, the squatter's is untouched */
      CHECK(state_of(ring2_va) == PRIVATE_RW);
      CHECK(state_of((uint8_t *)ring2_va + 0x20000) == PRIVATE_RW);
      CHECK(ring2_va[0] == 0);
      CHECK(state_of(kept.va) == MAPPED_RW);
      CHECK(kept.va[0] == 0x1234);

      /* a straggler fault in an old-generation range moves no epoch */
      const int32_t after = helios_kmdmap_attach(); /* user on the new KMD */
      CHECK(!helios_kmdmap_lost(after));
      (void)ring.va[5];
      CHECK(!helios_kmdmap_lost(after));
      helios_kmdmap_detach();

      /* unregistering frees exactly our backing */
      helios_kmdmap_unregister(ring.va);
      CHECK(state_of(ring.va) == MEM_FREE);
      helios_kmdmap_unregister(ring2_va);
      CHECK(state_of(ring2_va) == MEM_FREE);
      CHECK(state_of(squat) == PRIVATE_RW);
      VirtualFree(squat, 0, MEM_RELEASE);
      mod_unregister(ledger.va);
      CHECK(state_of(ledger.va) == MEM_FREE);
      helios_kmdmap_unregister_owner(1); /* kept.va */
      CHECK(state_of(kept.va) == MAPPED_RW);
      view_unmap(&kept);
      CHECK(helios_kmdmap_t->count == 0);
   }

   /* 2. Faults outside registered ranges are left alone. */
   CHECK(IsBadReadPtr((void *)(uintptr_t)0x10, 4));

   /* 3. A freed range wholly taken by something else is not claimed. */
   {
      const int32_t e = helios_kmdmap_attach();
      struct view v = view_map(0x10000);
      helios_kmdmap_register(v.va, v.size, 3);
      void *base = (void *)v.va;
      view_unmap(&v);
      void *squat = VirtualAlloc(base, 0x10000, MEM_RESERVE, PAGE_NOACCESS);
      CHECK(squat == base);
      const LONG declined = helios_kmdmap_t->declined;
      CHECK(IsBadReadPtr(base, 4)); /* still faults, handlers declined */
      /* every module's handler declines it in turn */
      CHECK(helios_kmdmap_t->declined == declined + 2);
      CHECK(!helios_kmdmap_lost(e));
      VirtualFree(squat, 0, MEM_RELEASE);
      helios_kmdmap_unregister(base);
      helios_kmdmap_detach();
   }

   /* 4. A D3DKMT "device gone" status loses the device before any touch. */
   {
      const int32_t e = helios_kmdmap_attach();
      struct view v = view_map(0x20000);
      helios_kmdmap_register(v.va, v.size, 3);
      volatile uint32_t *p = v.va;
      view_unmap(&v);
      CHECK(helios_kmdmap_status_is_device_gone((LONG)0xC00002B6));
      CHECK(!helios_kmdmap_status_is_device_gone((LONG)0xC0000002));
      helios_kmdmap_mark_lost(e);
      CHECK(helios_kmdmap_lost(e));
      CHECK(state_of(p) == PRIVATE_RW);
      helios_kmdmap_mark_lost(e); /* same generation: epoch moves once */
      CHECK(helios_kmdmap_t->epoch == e + 1);
      helios_kmdmap_unregister((void *)p);
      CHECK(state_of(p) == MEM_FREE);
      helios_kmdmap_detach();
   }

   mod_detach();
   helios_kmdmap_detach();
   CHECK(helios_kmdmap_veh == NULL);

   printf("%s (%d failures, %ld log lines, %ld repairs, %ld declined)\n",
          failures ? "FAILED" : "PASSED", failures, log_lines,
          helios_kmdmap_t->repairs, helios_kmdmap_t->declined);
   return failures ? 1 : 0;
}

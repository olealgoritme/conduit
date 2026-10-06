/* helios_kmdmap.h — KMD-made user mappings that vanish under a live process.
 *
 * ⛔ Two byte-identical copies: guest/windows/umd_common/bridge/helios_kmdmap.h
 * (canonical; compiled into helios_umd.dll by bridge_kmdmap.cpp) and
 * src/virtio/vulkan/helios_kmdmap.h in the Mesa patch series (the Venus ICD).
 * guest/windows/icd/win-build/helios_kmdmap_test.c checks they match.
 *
 * # The problem
 *
 * The Helios KMD maps memory into processes itself (MmMapLockedPagesSpecifyCache
 * to UserMode): the Venus ring and cs/reply shmems, mapped bos, the producer
 * status page (ICD), the scanout read ledger (UMD). When the KMD stops under
 * live processes (a live driver update, a device reset), dxgkrnl destroys
 * every device and the KMD's DxgkDdiDestroyDevice unmaps those views. Nothing
 * tells the user-mode side; its next plain load from a view is an access
 * violation (dwm.exe at vulkan_virtio.dll+0x3858c4 reading the ring head, then
 * at helios_umd.dll reading the ledger's slot_count).
 *
 * # The mechanism
 *
 * dxgkrnl keeps D3D CPU mappings valid across a removal by pointing them at a
 * dummy page. This does the same for the KMD's own views, from user mode:
 *
 *  - Every KMD view is registered in ONE process-wide table, shared by every
 *    module that includes this header (named section
 *    Local\HeliosKmdMap-<pid>, created atomically by whoever comes first; the
 *    ICD and the UMD do not need each other to be loaded).
 *  - Every such module installs a vectored exception handler running the same
 *    code on the same table under the table's own lock, so it does not matter
 *    which handler sees a fault first, and the handlers cannot fight.
 *  - An access violation inside a registered range whose VA is now free gets
 *    zeroed private memory committed at exactly that range and the faulting
 *    instruction resumes (it reads zeros). The first such fault of a mapping
 *    generation bumps the process LOSS EPOCH and backs every other registered
 *    range that is already free, so nothing else in the process can be handed
 *    those addresses while the ICD/UMD still hold pointers into them.
 *  - Every user (an ICD renderer, a UMD device) records the epoch when it is
 *    created; it is lost once the epoch moves. One KMD serves the whole
 *    process, so one loss loses every user alive at that moment, and a user
 *    created afterwards (on the restarted KMD) starts clean.
 *  - A D3DKMT failure that says the device is gone bumps the epoch the same
 *    way, before the next touch.
 *
 * Nothing here claims a range that is not free. Freed address space can be
 * reused by an unrelated allocation before anything touches it (dwm.exe at the
 * 319.2 swap: a 4 KiB allocation landed inside a freed 132 KiB ring view);
 * backing then goes allocation-granule by granule, and a granule that is taken
 * is left alone: a fault there goes to the next handler exactly as before.
 *
 * Plain C99 / C++ (clang-cl, mingw gcc), Win32 only, no CRT allocation.
 * Define HELIOS_KMDMAP_LOG(fmt, ...) before including to get diagnostics.
 */
#ifndef HELIOS_KMDMAP_H
#define HELIOS_KMDMAP_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <windows.h>

#ifndef HELIOS_KMDMAP_LOG
#define HELIOS_KMDMAP_LOG(...) ((void)0)
#endif

#define HELIOS_KMDMAP_FN static inline

#define HELIOS_KMDMAP_MAGIC 0x4d4d4b48u /* 'HKMM' */
#define HELIOS_KMDMAP_VERSION 1u
#define HELIOS_KMDMAP_CAPACITY 12288u
/* Per-granule backing is tracked for the first 64 granules of a range. */
#define HELIOS_KMDMAP_MASK_GRANULES 64u

struct helios_kmdmap_entry {
   uint64_t base;
   uint64_t size;
   uint64_t owner;       /* the registering user's cookie (unregister_owner) */
   uint64_t backed_mask; /* granules of [base, base+size) we committed */
   int32_t epoch;        /* loss epoch the mapping was made in */
   uint32_t backed_whole; /* the whole range is one allocation of ours */
};

struct helios_kmdmap_table {
   uint32_t magic;
   uint32_t version;
   volatile LONG lock;    /* owning thread id, 0 = free */
   volatile LONG epoch;   /* bumped once per observed device loss */
   volatile LONG repairs; /* granules or ranges backed */
   volatile LONG declined; /* faults left to the next handler */
   volatile LONG full;     /* registrations dropped, table full */
   uint32_t count;
   struct helios_kmdmap_entry e[HELIOS_KMDMAP_CAPACITY];
};

/* Per-module state (each including module has its own copy). */
static struct helios_kmdmap_table *volatile helios_kmdmap_t;
static PVOID helios_kmdmap_veh;
static volatile LONG helios_kmdmap_users;
static SRWLOCK helios_kmdmap_module_lock = SRWLOCK_INIT;

HELIOS_KMDMAP_FN struct helios_kmdmap_table *
helios_kmdmap_table_get(void)
{
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   if (t)
      return t;

   char name[64];
   snprintf(name, sizeof(name), "Local\\HeliosKmdMap-%lu",
            (unsigned long)GetCurrentProcessId());
   /* Creation is atomic: a second creator opens the same zero-filled section. */
   HANDLE h = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0,
                                 (DWORD)sizeof(struct helios_kmdmap_table),
                                 name);
   if (!h)
      return NULL;
   t = (struct helios_kmdmap_table *)MapViewOfFile(
      h, FILE_MAP_ALL_ACCESS, 0, 0, sizeof(struct helios_kmdmap_table));
   /* The view and the handle stay for the life of the process: every
    * module's handler may still read the table, and the section's NAME lives
    * only as long as some handle to it is open (a view does not keep it), so
    * closing it would let the next module create a second, separate table. */
   if (!t) {
      CloseHandle(h);
      return NULL;
   }

   /* Whoever comes first writes the header fields (each one independently,
    * so no ordering between creators matters); everyone then checks them. */
   InterlockedCompareExchange((volatile LONG *)&t->version,
                              (LONG)HELIOS_KMDMAP_VERSION, 0);
   InterlockedCompareExchange((volatile LONG *)&t->magic,
                              (LONG)HELIOS_KMDMAP_MAGIC, 0);
   if (t->magic != HELIOS_KMDMAP_MAGIC || t->version != HELIOS_KMDMAP_VERSION) {
      UnmapViewOfFile(t);
      CloseHandle(h);
      return NULL;
   }
   struct helios_kmdmap_table *prev =
      (struct helios_kmdmap_table *)InterlockedCompareExchangePointer(
         (PVOID volatile *)&helios_kmdmap_t, t, NULL);
   if (prev) {
      UnmapViewOfFile(t);
      CloseHandle(h);
      t = prev;
   }
   return t;
}

HELIOS_KMDMAP_FN void
helios_kmdmap_lock(struct helios_kmdmap_table *t)
{
   const LONG tid = (LONG)GetCurrentThreadId();
   for (unsigned spin = 0;
        InterlockedCompareExchange(&t->lock, tid, 0) != 0; spin++) {
      if (spin < 64)
         YieldProcessor();
      else
         SwitchToThread();
   }
}

HELIOS_KMDMAP_FN void
helios_kmdmap_unlock(struct helios_kmdmap_table *t)
{
   InterlockedExchange(&t->lock, 0);
}

HELIOS_KMDMAP_FN uintptr_t
helios_kmdmap_granularity(void)
{
   SYSTEM_INFO si;
   GetSystemInfo(&si);
   return si.dwAllocationGranularity;
}

/* Is every byte of [p, p+len) part of one free region? */
HELIOS_KMDMAP_FN bool
helios_kmdmap_free(uintptr_t p, uintptr_t len)
{
   MEMORY_BASIC_INFORMATION mbi;
   if (!VirtualQuery((void *)p, &mbi, sizeof(mbi)) || mbi.State != MEM_FREE)
      return false;
   return (uintptr_t)mbi.BaseAddress + mbi.RegionSize >= p + len;
}

HELIOS_KMDMAP_FN bool
helios_kmdmap_alloc_at(uintptr_t p, uintptr_t len)
{
   void *r = VirtualAlloc((void *)p, len, MEM_RESERVE | MEM_COMMIT,
                          PAGE_READWRITE);
   if (r == (void *)p)
      return true;
   if (r)
      VirtualFree(r, 0, MEM_RELEASE);
   return false;
}

/* Back whatever of a registered range is free with zeroed memory. Caller
 * holds the lock. Returns whether the page holding `addr` (0: any page) is now
 * ours. */
HELIOS_KMDMAP_FN bool
helios_kmdmap_back_locked(struct helios_kmdmap_table *t,
                          struct helios_kmdmap_entry *m, uintptr_t addr)
{
   const uintptr_t gran = helios_kmdmap_granularity();
   const uintptr_t base = (uintptr_t)m->base;
   const uintptr_t end = base + (uintptr_t)m->size;
   const uintptr_t first = base & ~(gran - 1);

   if (m->backed_whole)
      return true;

   /* The common case: the whole view is free. A reservation starts on the
    * allocation granularity; the KMD's views do too. */
   if (!m->backed_mask && helios_kmdmap_free(first, end - first) &&
       helios_kmdmap_alloc_at(first, end - first)) {
      m->backed_whole = 1;
      InterlockedIncrement(&t->repairs);
      return true;
   }

   /* Something took part of it: go granule by granule. */
   bool addr_backed = false;
   unsigned i = 0;
   for (uintptr_t g = first; g < end && i < HELIOS_KMDMAP_MASK_GRANULES;
        g += gran, i++) {
      const uintptr_t g_end = g + gran < end ? g + gran : end;
      const bool holds_addr = addr >= g && addr < g_end;
      if (!(m->backed_mask & (1ull << i)) && helios_kmdmap_free(g, g_end - g) &&
          helios_kmdmap_alloc_at(g, g_end - g)) {
         m->backed_mask |= 1ull << i;
         InterlockedIncrement(&t->repairs);
      }
      if (holds_addr && (m->backed_mask & (1ull << i)))
         addr_backed = true;
   }
   if (!m->backed_mask)
      HELIOS_KMDMAP_LOG("kmdmap: cannot back freed range %p+0x%llx (err=%lu)",
                        (void *)base, (unsigned long long)m->size,
                        GetLastError());
   return addr ? addr_backed : m->backed_mask != 0;
}

/* The device is gone: move the epoch (once per generation) and back every
 * registered range that is already free. Caller holds the lock. */
HELIOS_KMDMAP_FN void
helios_kmdmap_lose_locked(struct helios_kmdmap_table *t, int32_t generation)
{
   if (generation == t->epoch)
      InterlockedIncrement(&t->epoch);
   for (uint32_t i = 0; i < t->count; i++) {
      struct helios_kmdmap_entry *m = &t->e[i];
      if (!m->backed_whole && helios_kmdmap_free((uintptr_t)m->base, 1))
         helios_kmdmap_back_locked(t, m, 0);
   }
}

static LONG CALLBACK
helios_kmdmap_handler(PEXCEPTION_POINTERS info)
{
   const EXCEPTION_RECORD *rec = info->ExceptionRecord;
   if (rec->ExceptionCode != EXCEPTION_ACCESS_VIOLATION ||
       rec->NumberParameters < 2)
      return EXCEPTION_CONTINUE_SEARCH;
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   if (!t || t->lock == (LONG)GetCurrentThreadId())
      return EXCEPTION_CONTINUE_SEARCH;

   const uintptr_t addr = (uintptr_t)rec->ExceptionInformation[1];
   LONG verdict = EXCEPTION_CONTINUE_SEARCH;
   bool hit = false, first = false;
   uintptr_t base = 0;
   uint64_t size = 0;

   helios_kmdmap_lock(t);
   for (uint32_t i = 0; i < t->count; i++) {
      struct helios_kmdmap_entry *m = &t->e[i];
      if (addr - (uintptr_t)m->base >= (uintptr_t)m->size)
         continue;
      hit = true;
      base = (uintptr_t)m->base;
      size = m->size;
      first = m->epoch == t->epoch;
      if (helios_kmdmap_back_locked(t, m, addr)) {
         verdict = EXCEPTION_CONTINUE_EXECUTION;
         helios_kmdmap_lose_locked(t, m->epoch);
      } else {
         /* Ours already, but faulting anyway, or not free: not ours to fix. */
         MEMORY_BASIC_INFORMATION mbi;
         if ((m->backed_whole || m->backed_mask) &&
             VirtualQuery((void *)addr, &mbi, sizeof(mbi)) &&
             mbi.State == MEM_COMMIT && mbi.Type == MEM_PRIVATE &&
             mbi.Protect == PAGE_READWRITE)
            verdict = EXCEPTION_CONTINUE_EXECUTION;
         else
            InterlockedIncrement(&t->declined);
      }
      break;
   }
   helios_kmdmap_unlock(t);

   if (hit && (first || verdict != EXCEPTION_CONTINUE_EXECUTION))
      HELIOS_KMDMAP_LOG("kmdmap: access violation at %p in KMD mapping %p+0x%llx "
                        "(%s) -> %s",
                        (void *)addr, (void *)base, (unsigned long long)size,
                        rec->ExceptionInformation[0] ? "write" : "read",
                        verdict == EXCEPTION_CONTINUE_EXECUTION
                           ? "backed with zero pages, device lost"
                           : "not free, left to the next handler");
   (void)base;
   (void)size;
   return verdict;
}

/* ---- the API ------------------------------------------------------------ */

/* A user (renderer, device) starts: installs this module's handler for the
 * first and returns the loss epoch to compare against later. */
HELIOS_KMDMAP_FN int32_t
helios_kmdmap_attach(void)
{
   struct helios_kmdmap_table *t = helios_kmdmap_table_get();
   AcquireSRWLockExclusive(&helios_kmdmap_module_lock);
   if (helios_kmdmap_users++ == 0 && t)
      helios_kmdmap_veh = AddVectoredExceptionHandler(1, helios_kmdmap_handler);
   ReleaseSRWLockExclusive(&helios_kmdmap_module_lock);
   if (t && !helios_kmdmap_veh)
      HELIOS_KMDMAP_LOG("kmdmap: AddVectoredExceptionHandler failed (err=%lu)",
                        GetLastError());
   if (!t)
      HELIOS_KMDMAP_LOG("kmdmap: shared table unavailable (err=%lu)",
                        GetLastError());
   return t ? (int32_t)t->epoch : 0;
}

/* Paired with every helios_kmdmap_attach(). */
HELIOS_KMDMAP_FN void
helios_kmdmap_detach(void)
{
   AcquireSRWLockExclusive(&helios_kmdmap_module_lock);
   if (helios_kmdmap_users > 0 && --helios_kmdmap_users == 0 &&
       helios_kmdmap_veh) {
      RemoveVectoredExceptionHandler(helios_kmdmap_veh);
      helios_kmdmap_veh = NULL;
   }
   ReleaseSRWLockExclusive(&helios_kmdmap_module_lock);
}

/* Has the device behind a user that attached at `epoch` been lost? */
HELIOS_KMDMAP_FN bool
helios_kmdmap_lost(int32_t epoch)
{
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   return t && (int32_t)InterlockedCompareExchange(&t->epoch, 0, 0) != epoch;
}

/* A view the KMD just mapped, for the user identified by `owner`. */
HELIOS_KMDMAP_FN void
helios_kmdmap_register(const volatile void *va, uint64_t size, uint64_t owner)
{
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   if (!t || !va || !size)
      return;
   helios_kmdmap_lock(t);
   if (t->count < HELIOS_KMDMAP_CAPACITY) {
      struct helios_kmdmap_entry *m = &t->e[t->count++];
      m->base = (uint64_t)(uintptr_t)va;
      m->size = size;
      m->owner = owner;
      m->backed_mask = 0;
      m->backed_whole = 0;
      m->epoch = (int32_t)t->epoch;
   } else {
      InterlockedIncrement(&t->full);
   }
   helios_kmdmap_unlock(t);
}

/* Drop entry i, freeing our backing. Caller holds the lock. */
HELIOS_KMDMAP_FN void
helios_kmdmap_remove_locked(struct helios_kmdmap_table *t, uint32_t i)
{
   struct helios_kmdmap_entry *m = &t->e[i];
   const uintptr_t gran = helios_kmdmap_granularity();
   const uintptr_t first = (uintptr_t)m->base & ~(gran - 1);
   if (m->backed_whole) {
      VirtualFree((void *)first, 0, MEM_RELEASE);
   } else {
      for (unsigned g = 0; g < HELIOS_KMDMAP_MASK_GRANULES; g++) {
         if (m->backed_mask & (1ull << g))
            VirtualFree((void *)(first + g * gran), 0, MEM_RELEASE);
      }
   }
   t->e[i] = t->e[--t->count];
}

/* Before the view is released (the KMD unmaps it then). Frees our backing. */
HELIOS_KMDMAP_FN void
helios_kmdmap_unregister(const volatile void *va)
{
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   if (!t || !va)
      return;
   helios_kmdmap_lock(t);
   for (uint32_t i = 0; i < t->count; i++) {
      if (t->e[i].base == (uint64_t)(uintptr_t)va) {
         helios_kmdmap_remove_locked(t, i);
         break;
      }
   }
   helios_kmdmap_unlock(t);
}

/* Everything a user still has registered, before its device is destroyed. */
HELIOS_KMDMAP_FN void
helios_kmdmap_unregister_owner(uint64_t owner)
{
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   if (!t)
      return;
   helios_kmdmap_lock(t);
   for (uint32_t i = t->count; i-- > 0;) {
      if (t->e[i].owner == owner)
         helios_kmdmap_remove_locked(t, i);
   }
   helios_kmdmap_unlock(t);
}

/* A D3DKMT call said the device is gone, for a user that attached at `epoch`. */
HELIOS_KMDMAP_FN void
helios_kmdmap_mark_lost(int32_t epoch)
{
   struct helios_kmdmap_table *t = helios_kmdmap_t;
   if (!t)
      return;
   helios_kmdmap_lock(t);
   helios_kmdmap_lose_locked(t, epoch);
   helios_kmdmap_unlock(t);
}

/* NTSTATUS values with which D3DKMT reports the device or adapter gone. */
HELIOS_KMDMAP_FN bool
helios_kmdmap_status_is_device_gone(LONG st)
{
   switch ((ULONG)st) {
   case 0xC00002B6: /* STATUS_DEVICE_REMOVED */
   case 0xC000009D: /* STATUS_DEVICE_NOT_CONNECTED */
   case 0xC00000C0: /* STATUS_DEVICE_DOES_NOT_EXIST */
   case 0xC01E0003: /* STATUS_GRAPHICS_ADAPTER_WAS_RESET */
   case 0xC01E0200: /* STATUS_GRAPHICS_GPU_EXCEPTION_ON_DEVICE */
      return true;
   default:
      return false;
   }
}

#endif /* HELIOS_KMDMAP_H */

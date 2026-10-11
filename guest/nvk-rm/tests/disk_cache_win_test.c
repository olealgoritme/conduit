/* disk_cache_win_test: Mesa's disk cache on Windows with Steam's environment
 * (MESA_DISK_CACHE_SINGLE_FILE=1 and a per-game MESA_SHADER_CACHE_DIR): an
 * entry put by one cache instance is read back by the next one, from the
 * requested directory; a requested directory that cannot be created falls
 * back to %LOCALAPPDATA% (the test assumes the guest has no Q: drive).
 * Checks patch common 0048.
 *
 * Build (host), against a Windows Mesa build tree B (MESA = its source):
 *   x86_64-w64-mingw32-gcc -O1 -std=gnu11 -DENABLE_SHADER_CACHE \
 *     -I$MESA/include -I$MESA/src -I$B/src -I$B/src/util -I$MESA/src/util \
 *     -o disk_cache_win_test.exe disk_cache_win_test.c \
 *     $B/src/util/libmesa_util.a $B/src/util/libmesa_util_simd.a \
 *     $B/src/util/libmesa_util_clflush.a $B/src/util/libmesa_util_clflushopt.a \
 *     $B/src/util/blake3/libblake3.a $B/src/c11/impl/libmesa_util_c11.a \
 *     -static -lsynchronization -lws2_32 -ldbghelp -lshlwapi -lpthread
 * Run (guest): disk_cache_win_test.exe WORKDIR   (prints a RESULT line)
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include "util/disk_cache.h"

static int roundtrip(const char *tag, const char *expect_dir)
{
   struct disk_cache *c = disk_cache_create("cachetest", "build1", 0);
   if (!c) { printf("%s: no cache\n", tag); return 1; }
   cache_key k;
   const char *payload = "conduit disk cache test";
   disk_cache_compute_key(c, tag, strlen(tag), k);
   disk_cache_put(c, k, payload, strlen(payload) + 1, NULL);
   disk_cache_wait_for_idle(c);
   disk_cache_destroy(c);

   c = disk_cache_create("cachetest", "build1", 0);
   if (!c) { printf("%s: no cache on reopen\n", tag); return 1; }
   size_t size = 0;
   char *got = disk_cache_get(c, k, &size);
   int ok = got && size == strlen(payload) + 1 && !strcmp(got, payload);
   free(got);
   disk_cache_destroy(c);
   DWORD a = GetFileAttributesA(expect_dir);
   int dir_ok = a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY);
   printf("%s: entry %s, %s %s\n", tag, ok ? "read back" : "MISSING", expect_dir, dir_ok ? "exists" : "MISSING");
   return !(ok && dir_ok);
}

int main(int argc, char **argv)
{
   if (argc < 2) return 2;
   /* A fresh run directory each time: an entry left by an earlier run
    * would be found even if this run's put did nothing.
    */
   char run[MAX_PATH], dir[MAX_PATH], local[MAX_PATH], expect[MAX_PATH];
   CreateDirectoryA(argv[1], NULL);
   snprintf(run, sizeof(run), "%s\\run-%lu-%lu", argv[1],
            (unsigned long)GetCurrentProcessId(), (unsigned long)GetTickCount());
   if (!CreateDirectoryA(run, NULL)) {
      printf("cannot create %s\n", run);
      return 2;
   }
   snprintf(dir, sizeof(dir), "%s\\steam-shadercache\\730", run);
   snprintf(local, sizeof(local), "%s\\localappdata", run);
   CreateDirectoryA(local, NULL);
   int fail = 0;

   /* Steam's environment for a game */
   _putenv_s("MESA_DISK_CACHE_SINGLE_FILE", "1");
   _putenv_s("MESA_DISK_CACHE_READ_ONLY_FOZ_DBS", "steam_cache,steam_precompiled");
   _putenv_s("MESA_SHADER_CACHE_DIR", dir);
   _putenv_s("LOCALAPPDATA", local);
   snprintf(expect, sizeof(expect), "%s\\mesa_shader_cache", dir);
   fail |= roundtrip("steam-single-file", expect);

   /* A directory that cannot be created: falls back to LOCALAPPDATA */
   _putenv_s("MESA_SHADER_CACHE_DIR", "Q:\\conduit-no-such-drive\\730");
   snprintf(expect, sizeof(expect), "%s\\mesa_shader_cache", local);
   fail |= roundtrip("unwritable-dir", expect);

   printf("RESULT %s\n", fail ? "FAIL" : "PASS");
   return fail;
}

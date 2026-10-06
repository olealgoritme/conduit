/* SPDX-License-Identifier: MIT */
/*
 * crm_share_smoke: one RM memory object seen by two processes, the way a
 * shared D3D surface is between two NVK processes (guest/windows/docs/
 * shared-surfaces.md, dxvk-on-nvk.md S6).
 *
 *   crm_share_smoke              run everything (spawns itself as the opener)
 *   crm_share_smoke open C V S   opener: dup video memory V and system memory S
 *                                of client C, check the creator's pattern,
 *                                write its own
 *   crm_share_smoke make F      a creator that publishes (client, memory) in F
 *                                and exits without freeing once F.ready exists
 *   crm_share_smoke hold C V F  opener: dup V, then wait for file F to appear
 *                                (the creator freed V meanwhile) and check the
 *                                memory is still there
 *
 * The creator allocates 2 MiB of video memory (non-contiguous, big pages, as
 * NVK does) and 2 MiB of system memory, CPU-maps both and fills them. The
 * opener, a separate process with its own RM client, duplicates both objects
 * into its client with NV_ESC_RM_DUP_OBJECT, CPU-maps and GPU-maps them, checks
 * the pattern and writes a second one, which the creator then reads back: the
 * same memory, not a copy. Then the lifetime rule: a duplicate keeps the
 * object alive after the creator freed its own handle. And the negative case:
 * a duplicate from a client that does not exist is refused.
 *
 * Exit 0 when every step passed.
 */
#ifndef _WIN32
#define _POSIX_C_SOURCE 200809L
#endif
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#endif

#include "rmclient.h"
#include "nv_ioctl_defs.h"

#define MIB (1024ull * 1024ull)
#define SIZE (2 * MIB)
#define VID_MAP (64 * 1024ull) /* BAR1 window the test maps */

static int failed;
static const char *role = "creator";

static int step(const char *what, int r)
{
    if (r == 0)
        printf("[ ok ] %s: %s\n", role, what);
    else
        printf("[FAIL] %s: %s: %s (%d / 0x%x) %s\n", role, what, crm_status_name(r), r,
               (unsigned)r, crm_status_string(r));
    fflush(stdout);
    if (r)
        failed = 1;
    return r;
}

static uint32_t pattern(size_t i, uint32_t seed)
{
    return (uint32_t)(i * 2654435761u) ^ seed;
}

static void fill(volatile uint32_t *w, size_t words, uint32_t seed)
{
    for (size_t i = 0; i < words; i++)
        w[i] = pattern(i, seed);
}

static size_t check(volatile const uint32_t *w, size_t words, uint32_t seed)
{
    size_t bad = 0;
    for (size_t i = 0; i < words; i++)
        bad += w[i] != pattern(i, seed);
    return bad;
}

static void sleep_ms(unsigned ms)
{
#ifdef _WIN32
    Sleep(ms);
#else
    struct timespec ts = { ms / 1000u, (long)(ms % 1000u) * 1000000L };
    nanosleep(&ts, NULL);
#endif
}

static int file_exists(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (f)
        fclose(f);
    return f != NULL;
}

static void touch(const char *path)
{
    FILE *f = fopen(path, "wb");
    if (f)
        fclose(f);
}

/* Start `self` with `args` and either wait for it (exit code) or not (-1 on
 * failure to start, 0 started; *pid gets something to wait on). */
#ifdef _WIN32
typedef HANDLE child_t;
#else
typedef pid_t child_t;
#endif

static int spawn(const char *self, const char *args, child_t *child)
{
#ifdef _WIN32
    char cmd[1024];
    snprintf(cmd, sizeof(cmd), "\"%s\" %s", self, args);
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof(si));
    si.cb = sizeof(si);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi))
        return -(int)GetLastError();
    CloseHandle(pi.hThread);
    *child = pi.hProcess;
    return 0;
#else
    char cmd[1024];
    snprintf(cmd, sizeof(cmd), "exec \"%s\" %s", self, args);
    pid_t pid = fork();
    if (pid < 0)
        return -errno;
    if (pid == 0) {
        execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        _exit(127);
    }
    *child = pid;
    return 0;
#endif
}

static int wait_child(child_t child)
{
#ifdef _WIN32
    WaitForSingleObject(child, 120000);
    DWORD code = 1;
    GetExitCodeProcess(child, &code);
    CloseHandle(child);
    return (int)code;
#else
    int status = 0;
    if (waitpid(child, &status, 0) < 0)
        return -errno;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128;
#endif
}

struct gpu {
    crm_client *c;
    uint32_t dev, sub, vas;
};

static int open_gpu(struct gpu *g)
{
    memset(g, 0, sizeof(*g));
    if (step("crm_open", crm_open(&g->c, NULL)))
        return -1;
    printf("       %s: root client 0x%08x\n", role, crm_root(g->c));
    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof(dp));
    if (step("alloc NV01_DEVICE_0", crm_alloc(g->c, crm_root(g->c), &g->dev, NV01_DEVICE_0, &dp, sizeof(dp))))
        return -1;
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (step("alloc NV20_SUBDEVICE_0", crm_alloc(g->c, g->dev, &g->sub, NV20_SUBDEVICE_0, &sp, sizeof(sp))))
        return -1;
    NV_VASPACE_ALLOCATION_PARAMETERS vp;
    memset(&vp, 0, sizeof(vp));
    if (step("alloc FERMI_VASPACE_A", crm_alloc(g->c, g->dev, &g->vas, FERMI_VASPACE_A, &vp, sizeof(vp))))
        return -1;
    return 0;
}

static void close_gpu(struct gpu *g)
{
    if (!g->c)
        return;
    if (g->vas) crm_free(g->c, g->dev, g->vas);
    if (g->sub) crm_free(g->c, g->dev, g->sub);
    if (g->dev) crm_free(g->c, crm_root(g->c), g->dev);
    printf("       %s: objects still tracked %zu, CPU mappings %zu\n", role,
           crm_object_count(g->c), crm_mapping_count(g->c));
    crm_close(g->c);
}

/* Video memory the way NVK allocates an exportable image (nvkmd_rm_mem.c
 * alloc_rm_memory): big pages, non-contiguous, uncompressed. */
static int alloc_vid(struct gpu *g, uint32_t *h)
{
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof(mp));
    mp.owner = crm_root(g->c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.flags = NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE;
    mp.attr = (NVOS32_ATTR_LOCATION_VIDMEM << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_BIG << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_ALLOW_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT);
    mp.size = SIZE;
    mp.alignment = 64 * 1024;
    return crm_alloc(g->c, g->dev, h, NV01_MEMORY_LOCAL_USER, &mp, sizeof(mp));
}

static int alloc_sys(struct gpu *g, uint32_t *h)
{
    NV_MEMORY_ALLOCATION_PARAMS mp;
    memset(&mp, 0, sizeof(mp));
    mp.owner = crm_root(g->c);
    mp.type = NVOS32_TYPE_IMAGE;
    mp.attr = (NVOS32_ATTR_LOCATION_PCI << NVOS32_ATTR_LOCATION_SHIFT) |
              (NVOS32_ATTR_PAGE_SIZE_4KB << NVOS32_ATTR_PAGE_SIZE_SHIFT) |
              (NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS << NVOS32_ATTR_PHYSICALITY_SHIFT) |
              (NVOS32_ATTR_COHERENCY_CACHED << NVOS32_ATTR_COHERENCY_SHIFT);
    mp.size = SIZE;
    return crm_alloc(g->c, g->dev, h, NV01_MEMORY_SYSTEM, &mp, sizeof(mp));
}

/* Map, check the creator's seed over the first half, write `put` over the
 * second half, unmap. */
static void check_and_answer(struct gpu *g, uint32_t mem, uint64_t len, const char *what,
                             uint32_t seed, uint32_t put)
{
    char msg[160];
    void *p = NULL;
    snprintf(msg, sizeof(msg), "CPU-map the duplicated %s", what);
    if (step(msg, crm_map_memory(g->c, g->dev, mem, 0, len, 0, &p)))
        return;
    volatile uint32_t *w = p;
    const size_t half = (size_t)(len / 8);
    size_t bad = check(w, half, seed);
    snprintf(msg, sizeof(msg), "%s holds the creator's pattern (%zu KiB)", what, (size_t)(len / 2048));
    step(msg, bad ? -EIO : 0);
    if (bad)
        printf("       %zu mismatching words, word 0 = 0x%08x (want 0x%08x)\n", bad, w[0],
               pattern(0, seed));
    fill(w + half, half, put);
    snprintf(msg, sizeof(msg), "CPU-unmap the duplicated %s", what);
    step(msg, crm_unmap_memory(g->c, g->dev, mem, p, len, 0));
}

static int run_open(uint32_t client_src, uint32_t vid_src, uint32_t sys_src)
{
    struct gpu g;
    role = "opener";
    if (open_gpu(&g)) {
        close_gpu(&g);
        return 1;
    }

    uint32_t vid = 0, sys = 0, bogus = 0;
    char msg[160];
    snprintf(msg, sizeof(msg), "DUP_OBJECT video memory 0x%08x of client 0x%08x", vid_src, client_src);
    if (!step(msg, crm_dup_object(g.c, g.dev, &vid, client_src, vid_src, NV01_MEMORY_LOCAL_USER, 0)))
        printf("       opener: video memory is 0x%08x here\n", vid);
    snprintf(msg, sizeof(msg), "DUP_OBJECT system memory 0x%08x of client 0x%08x", sys_src, client_src);
    if (!step(msg, crm_dup_object(g.c, g.dev, &sys, client_src, sys_src, NV01_MEMORY_SYSTEM, 0)))
        printf("       opener: system memory is 0x%08x here\n", sys);

    /* The source client must exist: a made-up one is refused */
    int r = crm_dup_object(g.c, g.dev, &bogus, client_src ^ 0x00ff0000u, vid_src, 0, 0);
    printf("[%s] opener: DUP_OBJECT from a client that does not exist is refused: %s (0x%x)\n",
           r > 0 ? " ok " : "FAIL", crm_status_name(r), (unsigned)r);
    if (r <= 0)
        failed = 1;
    if (r == 0)
        crm_free(g.c, g.dev, bogus);

    if (vid) {
        check_and_answer(&g, vid, VID_MAP, "video memory", 0x5a5a5a5au, 0xa5a5a5a5u);
        uint64_t va = 0;
        if (!step("GPU-map the duplicated video memory into our VA space",
                  crm_map_dma(g.c, g.dev, g.vas, vid, 0, SIZE, 0, &va))) {
            printf("       opener: GPU VA 0x%" PRIx64 "\n", va);
            step("GPU-unmap it", crm_unmap_dma(g.c, g.dev, g.vas, vid, 0, va));
        }
        step("free the duplicated video memory", crm_free(g.c, g.dev, vid));
    }
    if (sys) {
        check_and_answer(&g, sys, SIZE, "system memory", 0x3c3c3c3cu, 0xc3c3c3c3u);
        uint64_t va = 0;
        if (!step("GPU-map the duplicated system memory into our VA space",
                  crm_map_dma(g.c, g.dev, g.vas, sys, 0, SIZE, 0, &va)))
            step("GPU-unmap it", crm_unmap_dma(g.c, g.dev, g.vas, sys, 0, va));
        step("free the duplicated system memory", crm_free(g.c, g.dev, sys));
    }
    close_gpu(&g);
    printf("%s\n", failed ? "OPENER FAILED" : "OPENER PASSED");
    return failed ? 1 : 0;
}

/* Dup, tell the creator (ready file), wait until it freed its handle (freed
 * file), then read: the memory must still be there. */
static int run_hold(uint32_t client_src, uint32_t vid_src, const char *freed)
{
    struct gpu g;
    role = "holder";
    if (open_gpu(&g)) {
        close_gpu(&g);
        return 1;
    }
    uint32_t vid = 0;
    if (!step("DUP_OBJECT video memory", crm_dup_object(g.c, g.dev, &vid, client_src, vid_src,
                                                        NV01_MEMORY_LOCAL_USER, 0))) {
        char ready[512];
        snprintf(ready, sizeof(ready), "%s.ready", freed);
        touch(ready);
        int waited = 0;
        while (!file_exists(freed) && waited < 60000) {
            sleep_ms(20);
            waited += 20;
        }
        step("the creator freed its handle", file_exists(freed) ? 0 : -ETIMEDOUT);
        void *p = NULL;
        if (!step("CPU-map the duplicate after the creator's free",
                  crm_map_memory(g.c, g.dev, vid, 0, VID_MAP, 0, &p))) {
            size_t bad = check(p, (size_t)(VID_MAP / 8), 0x5a5a5a5au);
            step("the memory outlived the creator's handle", bad ? -EIO : 0);
            crm_unmap_memory(g.c, g.dev, vid, p, VID_MAP, 0);
        }
        step("free the duplicate (the last handle)", crm_free(g.c, g.dev, vid));
    }
    close_gpu(&g);
    printf("%s\n", failed ? "HOLDER FAILED" : "HOLDER PASSED");
    return failed ? 1 : 0;
}

/* A creator that dies: allocate and fill video memory, publish (client,
 * memory) in FILE, wait for FILE.ready (the parent has its duplicate), then exit
 * without freeing anything, so the process cleanup (on Windows the KMD's) closes
 * the client. */
static int run_make(const char *file)
{
    struct gpu g;
    role = "dying creator";
    if (open_gpu(&g))
        return 1;
    uint32_t vid = 0;
    void *p = NULL;
    if (step("alloc video memory", alloc_vid(&g, &vid)) ||
        step("CPU-map it", crm_map_memory(g.c, g.dev, vid, 0, VID_MAP, 0, &p)))
        return 1;
    fill(p, (size_t)(VID_MAP / 4), 0x77777777u);
    char tmp[700];
    snprintf(tmp, sizeof(tmp), "%s.tmp", file);
    FILE *f = fopen(tmp, "wb");
    if (!f)
        return 1;
    fprintf(f, "0x%08x 0x%08x\n", crm_root(g.c), vid);
    fclose(f);
    remove(file);
    rename(tmp, file);
    char ready[700];
    snprintf(ready, sizeof(ready), "%s.ready", file);
    int waited = 0;
    while (!file_exists(ready) && waited < 60000) {
        sleep_ms(20);
        waited += 20;
    }
    printf("       dying creator: exiting with client 0x%08x still open\n", crm_root(g.c));
    return file_exists(ready) ? 0 : 1;
}

static int run_all(const char *self)
{
    struct gpu g;
    if (open_gpu(&g)) {
        close_gpu(&g);
        return 1;
    }
    uint32_t vid = 0, sys = 0;
    void *vid_cpu = NULL, *sys_cpu = NULL;
    if (step("alloc NV01_MEMORY_LOCAL_USER 2 MiB (NVK's kind of allocation)", alloc_vid(&g, &vid)))
        goto out;
    if (step("alloc NV01_MEMORY_SYSTEM 2 MiB", alloc_sys(&g, &sys)))
        goto out;
    if (step("CPU-map video memory", crm_map_memory(g.c, g.dev, vid, 0, VID_MAP, 0, &vid_cpu)))
        goto out;
    if (step("CPU-map system memory", crm_map_memory(g.c, g.dev, sys, 0, SIZE, 0, &sys_cpu)))
        goto out;
    const size_t vhalf = (size_t)(VID_MAP / 8), shalf = (size_t)(SIZE / 8);
    fill(vid_cpu, vhalf, 0x5a5a5a5au);
    fill(sys_cpu, shalf, 0x3c3c3c3cu);
    memset((uint32_t *)vid_cpu + vhalf, 0, vhalf * 4);
    memset((uint32_t *)sys_cpu + shalf, 0, shalf * 4);
    printf("       creator: client 0x%08x video 0x%08x system 0x%08x, patterns written\n",
           crm_root(g.c), vid, sys);
    fflush(stdout);

    char args[768];
    child_t child;
    snprintf(args, sizeof(args), "open 0x%08x 0x%08x 0x%08x", crm_root(g.c), vid, sys);
    int r = spawn(self, args, &child);
    if (step("start the opener (a second process)", r))
        goto out;
    r = wait_child(child);
    step("the opener passed", r);

    size_t bad = check((uint32_t *)vid_cpu + vhalf, vhalf, 0xa5a5a5a5u);
    step("video memory holds what the opener wrote (same memory)", bad ? -EIO : 0);
    bad = check((uint32_t *)sys_cpu + shalf, shalf, 0xc3c3c3c3u);
    step("system memory holds what the opener wrote (same memory)", bad ? -EIO : 0);

    /* Lifetime: the holder dups, we free our handle, the holder still reads */
    char freed[512];
#ifdef _WIN32
    char tmp[MAX_PATH];
    GetTempPathA(sizeof(tmp), tmp);
    snprintf(freed, sizeof(freed), "%scrm_share_%lu.freed", tmp, (unsigned long)GetCurrentProcessId());
#else
    snprintf(freed, sizeof(freed), "/tmp/crm_share_%ld.freed", (long)getpid());
#endif
    char ready[600];
    snprintf(ready, sizeof(ready), "%s.ready", freed);
    remove(freed);
    remove(ready);
    snprintf(args, sizeof(args), "hold 0x%08x 0x%08x \"%s\"", crm_root(g.c), vid, freed);
    r = spawn(self, args, &child);
    if (!step("start the holder", r)) {
        int waited = 0;
        while (!file_exists(ready) && waited < 60000) {
            sleep_ms(20);
            waited += 20;
        }
        step("the holder has its duplicate", file_exists(ready) ? 0 : -ETIMEDOUT);
        step("CPU-unmap video memory", crm_unmap_memory(g.c, g.dev, vid, vid_cpu, VID_MAP, 0));
        vid_cpu = NULL;
        step("free our video memory handle first", crm_free(g.c, g.dev, vid));
        vid = 0;
        touch(freed);
        step("the holder passed", wait_child(child));
        remove(freed);
        remove(ready);
    }

    /* The creator process exits while we hold a duplicate */
    {
        char made[600], made_ready[700];
        snprintf(made, sizeof(made), "%s.made", freed);
        snprintf(made_ready, sizeof(made_ready), "%s.ready", made);
        remove(made);
        remove(made_ready);
        snprintf(args, sizeof(args), "make \"%s\"", made);
        r = spawn(self, args, &child);
        if (!step("start a creator that exits without freeing", r)) {
            int waited = 0;
            while (!file_exists(made) && waited < 60000) {
                sleep_ms(20);
                waited += 20;
            }
            unsigned csrc = 0, vsrc = 0;
            uint32_t mine = 0;
            FILE *f = fopen(made, "rb");
            if (f) {
                if (fscanf(f, "%x %x", &csrc, &vsrc) != 2)
                    csrc = 0;
                fclose(f);
            }
            if (!step("read its (client, memory)", csrc ? 0 : -ENOENT) &&
                !step("DUP_OBJECT its video memory",
                      crm_dup_object(g.c, g.dev, &mine, csrc, vsrc, NV01_MEMORY_LOCAL_USER, 0))) {
                touch(made_ready);
                step("the creator process exited", wait_child(child));
                /* Its client is gone with it (process cleanup closed it) */
                uint32_t again = 0;
                int gone = 0;
                for (int i = 0; i < 100 && !gone; i++) {
                    int rr = crm_dup_object(g.c, g.dev, &again, csrc, vsrc, 0, 0);
                    if (rr == 0) {
                        crm_free(g.c, g.dev, again);
                        again = 0;
                        sleep_ms(50);
                    } else {
                        gone = 1;
                    }
                }
                step("its client was freed with the process", gone ? 0 : -EEXIST);
                void *p = NULL;
                if (!step("CPU-map the duplicate after the creator process is gone",
                          crm_map_memory(g.c, g.dev, mine, 0, VID_MAP, 0, &p))) {
                    size_t nbad = check(p, (size_t)(VID_MAP / 4), 0x77777777u);
                    step("the memory outlived the creator process", nbad ? -EIO : 0);
                    crm_unmap_memory(g.c, g.dev, mine, p, VID_MAP, 0);
                }
                step("free the duplicate", crm_free(g.c, g.dev, mine));
            } else {
                touch(made_ready);
                wait_child(child);
            }
            remove(made);
            remove(made_ready);
        }
    }

out:
    if (vid_cpu) crm_unmap_memory(g.c, g.dev, vid, vid_cpu, VID_MAP, 0);
    if (sys_cpu) crm_unmap_memory(g.c, g.dev, sys, sys_cpu, SIZE, 0);
    if (sys) step("free system memory", crm_free(g.c, g.dev, sys));
    if (vid) step("free video memory", crm_free(g.c, g.dev, vid));
    close_gpu(&g);
    printf("%s\n", failed ? "SHARE SMOKE FAILED" : "SHARE SMOKE PASSED");
    return failed ? 1 : 0;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc == 5 && !strcmp(argv[1], "open"))
        return run_open((uint32_t)strtoul(argv[2], NULL, 0), (uint32_t)strtoul(argv[3], NULL, 0),
                        (uint32_t)strtoul(argv[4], NULL, 0));
    if (argc == 5 && !strcmp(argv[1], "hold"))
        return run_hold((uint32_t)strtoul(argv[2], NULL, 0), (uint32_t)strtoul(argv[3], NULL, 0),
                        argv[4]);
    if (argc == 3 && !strcmp(argv[1], "make"))
        return run_make(argv[2]);
    if (argc != 1) {
        fprintf(stderr, "usage: %s [open CLIENT VID SYS | hold CLIENT VID FILE]\n", argv[0]);
        return 2;
    }
    return run_all(argv[0]);
}

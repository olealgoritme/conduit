// SPDX-License-Identifier: Apache-2.0
//
// cuda-mem: large CUDA allocations, run inside a Conduit VM.
//
// Exercises every way CUDA memory reaches the backend at sizes that cross
// its limits: managed memory (the UVM aperture), pinned host memory
// (cuMemHostAlloc / cuMemHostRegister: the window and OS-descriptor
// registration) and plain device memory. Each case runs in its own child
// process, so one that crashes or hangs is reported and the rest still run.
//
// Needs no CUDA toolkit: libcuda is opened at run time and the kernel is PTX
// text the driver JIT-compiles. Build on any x86-64 Linux:
//
//     gcc -O2 -o cuda-mem guest/tests/cuda-mem.c -ldl
//
// Sizes fit a 4 GiB test guest: pinned cases need that much free guest RAM,
// and managed memory is committed on the host as the CPU touches it.
//
// Run: ./cuda-mem            every case
//      ./cuda-mem managed    only cases whose name contains "managed"
// Exit status is the number of failed cases.

#include <dlfcn.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef unsigned long long CUdeviceptr;

#define CU_MEM_ATTACH_GLOBAL 0x1
#define CU_MEMHOSTALLOC_DEVICEMAP 0x2
#define CU_MEMHOSTREGISTER_DEVICEMAP 0x2

static struct {
  CUresult (*Init)(unsigned);
  CUresult (*DeviceGet)(CUdevice *, int);
  CUresult (*CtxCreate)(CUcontext *, unsigned, CUdevice);
  CUresult (*CtxDestroy)(CUcontext);
  CUresult (*ModuleLoadData)(CUmodule *, const void *);
  CUresult (*ModuleGetFunction)(CUfunction *, CUmodule, const char *);
  CUresult (*LaunchKernel)(CUfunction, unsigned, unsigned, unsigned, unsigned,
                           unsigned, unsigned, unsigned, void *, void **,
                           void **);
  CUresult (*CtxSynchronize)(void);
  CUresult (*MemAlloc)(CUdeviceptr *, size_t);
  CUresult (*MemFree)(CUdeviceptr);
  CUresult (*MemAllocManaged)(CUdeviceptr *, size_t, unsigned);
  CUresult (*MemHostAlloc)(void **, size_t, unsigned);
  CUresult (*MemFreeHost)(void *);
  CUresult (*MemHostRegister)(void *, size_t, unsigned);
  CUresult (*MemHostUnregister)(void *);
  CUresult (*MemHostGetDevicePointer)(CUdeviceptr *, void *, unsigned);
  CUresult (*MemcpyHtoD)(CUdeviceptr, const void *, size_t);
  CUresult (*MemcpyDtoH)(void *, CUdeviceptr, size_t);
  CUresult (*MemsetD32)(CUdeviceptr, unsigned, size_t);
  CUresult (*GetErrorName)(CUresult, const char **);
} cu;

// x[i] = x[i] * 2 + 1 over n u32s, grid-stride.
static const char PTX[] =
    ".version 7.0\n"
    ".target sm_52\n"
    ".address_size 64\n"
    ".visible .entry k(.param .u64 p, .param .u64 n)\n"
    "{\n"
    "  .reg .pred %q;\n"
    "  .reg .b32 %r<8>;\n"
    "  .reg .b64 %d<10>;\n"
    "  ld.param.u64 %d1, [p];\n"
    "  ld.param.u64 %d2, [n];\n"
    "  mov.u32 %r1, %ctaid.x;\n"
    "  mov.u32 %r2, %ntid.x;\n"
    "  mov.u32 %r3, %tid.x;\n"
    "  mul.wide.u32 %d3, %r1, %r2;\n"
    "  cvt.u64.u32 %d4, %r3;\n"
    "  add.u64 %d5, %d3, %d4;\n"
    "  mov.u32 %r4, %nctaid.x;\n"
    "  mul.wide.u32 %d6, %r4, %r2;\n"
    "LOOP:\n"
    "  setp.ge.u64 %q, %d5, %d2;\n"
    "  @%q bra DONE;\n"
    "  shl.b64 %d7, %d5, 2;\n"
    "  add.u64 %d8, %d1, %d7;\n"
    "  ld.global.u32 %r5, [%d8];\n"
    "  shl.b32 %r6, %r5, 1;\n"
    "  add.u32 %r6, %r6, 1;\n"
    "  st.global.u32 [%d8], %r6;\n"
    "  add.u64 %d5, %d5, %d6;\n"
    "  bra LOOP;\n"
    "DONE:\n"
    "  ret;\n"
    "}\n";

static CUfunction kern;
static char why[256];

static const char *ename(CUresult r) {
  const char *s = NULL;
  if (cu.GetErrorName && cu.GetErrorName(r, &s) == 0 && s)
    return s;
  return "?";
}

#define CK(call)                                                               \
  do {                                                                         \
    CUresult r_ = (call);                                                      \
    if (r_) {                                                                  \
      snprintf(why, sizeof why, "%s = %d %s", #call, r_, ename(r_));          \
      return -1;                                                               \
    }                                                                          \
  } while (0)

static void *sym(void *h, const char *n) {
  void *p = dlsym(h, n);
  if (!p) {
    fprintf(stderr, "libcuda has no %s\n", n);
    exit(2);
  }
  return p;
}

static void load(void) {
  void *h = dlopen("libcuda.so.1", RTLD_NOW);
  if (!h) {
    fprintf(stderr, "dlopen libcuda.so.1: %s\n", dlerror());
    exit(2);
  }
  cu.Init = sym(h, "cuInit");
  cu.DeviceGet = sym(h, "cuDeviceGet");
  cu.CtxCreate = sym(h, "cuCtxCreate_v2");
  cu.CtxDestroy = sym(h, "cuCtxDestroy_v2");
  cu.ModuleLoadData = sym(h, "cuModuleLoadData");
  cu.ModuleGetFunction = sym(h, "cuModuleGetFunction");
  cu.LaunchKernel = sym(h, "cuLaunchKernel");
  cu.CtxSynchronize = sym(h, "cuCtxSynchronize");
  cu.MemAlloc = sym(h, "cuMemAlloc_v2");
  cu.MemFree = sym(h, "cuMemFree_v2");
  cu.MemAllocManaged = sym(h, "cuMemAllocManaged");
  cu.MemHostAlloc = sym(h, "cuMemHostAlloc");
  cu.MemFreeHost = sym(h, "cuMemFreeHost");
  cu.MemHostRegister = sym(h, "cuMemHostRegister_v2");
  cu.MemHostUnregister = sym(h, "cuMemHostUnregister");
  cu.MemHostGetDevicePointer = sym(h, "cuMemHostGetDevicePointer_v2");
  cu.MemcpyHtoD = sym(h, "cuMemcpyHtoD_v2");
  cu.MemcpyDtoH = sym(h, "cuMemcpyDtoH_v2");
  cu.MemsetD32 = sym(h, "cuMemsetD32_v2");
  cu.GetErrorName = sym(h, "cuGetErrorName");
}

static int setup(void) {
  CUdevice dev;
  CUcontext ctx;
  CUmodule mod;
  CK(cu.Init(0));
  CK(cu.DeviceGet(&dev, 0));
  CK(cu.CtxCreate(&ctx, 0, dev));
  CK(cu.ModuleLoadData(&mod, PTX));
  CK(cu.ModuleGetFunction(&kern, mod, "k"));
  return 0;
}

static int run(CUdeviceptr p, size_t bytes) {
  unsigned long long n = bytes / 4;
  void *args[] = {&p, &n};
  CK(cu.LaunchKernel(kern, 1024, 1, 1, 256, 1, 1, 0, NULL, args, NULL));
  CK(cu.CtxSynchronize());
  return 0;
}

static uint32_t pat(size_t i) { return (uint32_t)(i * 2654435761u) ^ 0x5a5a; }

static void fill(uint32_t *a, size_t bytes) {
  for (size_t i = 0; i < bytes / 4; i++)
    a[i] = pat(i);
}

static int check(const uint32_t *a, size_t bytes) {
  for (size_t i = 0; i < bytes / 4; i++)
    if (a[i] != pat(i) * 2 + 1) {
      snprintf(why, sizeof why, "word %zu is %#x, expected %#x", i, a[i],
               pat(i) * 2 + 1);
      return -1;
    }
  return 0;
}

// Host buffer -> device -> kernel -> host, through a device buffer.
static int through_device(void *host, size_t bytes) {
  CUdeviceptr d;
  fill(host, bytes);
  CK(cu.MemAlloc(&d, bytes));
  CK(cu.MemcpyHtoD(d, host, bytes));
  memset(host, 0, bytes);
  if (run(d, bytes))
    return -1;
  CK(cu.MemcpyDtoH(host, d, bytes));
  CK(cu.MemFree(d));
  return check(host, bytes);
}

static int t_managed(size_t bytes) {
  CUdeviceptr p;
  CK(cu.MemAllocManaged(&p, bytes, CU_MEM_ATTACH_GLOBAL));
  fill((uint32_t *)p, bytes);
  if (run(p, bytes) || check((uint32_t *)p, bytes))
    return -1;
  // And a second round trip, so pages that went to the GPU come back.
  for (size_t i = 0; i < bytes / 4; i += 1024)
    ((uint32_t *)p)[i] = pat(i);
  CK(cu.MemFree(p));
  return 0;
}

static int t_hostalloc(size_t bytes) {
  void *h;
  CK(cu.MemHostAlloc(&h, bytes, 0));
  if (through_device(h, bytes))
    return -1;
  CK(cu.MemFreeHost(h));
  return 0;
}

static int t_hostregister(size_t bytes) {
  void *h = mmap(NULL, bytes, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
  if (h == MAP_FAILED) {
    snprintf(why, sizeof why, "mmap %zu bytes failed", bytes);
    return -1;
  }
  CK(cu.MemHostRegister(h, bytes, 0));
  if (through_device(h, bytes))
    return -1;
  CK(cu.MemHostUnregister(h));
  munmap(h, bytes);
  return 0;
}

// Device memory too big to copy whole: kernel over all of it, then three
// 64 MiB slices (start, middle, end) checked on the CPU.
static int t_devalloc(size_t bytes) {
  size_t slice = 64 << 20;
  uint32_t *h = malloc(slice);
  CUdeviceptr d;
  if (!h)
    return snprintf(why, sizeof why, "malloc failed"), -1;
  CK(cu.MemAlloc(&d, bytes));
  CK(cu.MemsetD32(d, 7, bytes / 4));
  if (run(d, bytes))
    return -1;
  size_t at[] = {0, bytes / 2, bytes - slice};
  for (int i = 0; i < 3; i++) {
    CK(cu.MemcpyDtoH(h, d + at[i], slice));
    for (size_t j = 0; j < slice / 4; j++)
      if (h[j] != 15) {
        snprintf(why, sizeof why, "byte %zu is %u, expected 15", at[i] + j * 4,
                 h[j]);
        return -1;
      }
  }
  CK(cu.MemFree(d));
  free(h);
  return 0;
}

#define MiB (1ull << 20)
#define GiB (1ull << 30)

static const struct tcase {
  const char *name;
  int (*fn)(size_t);
  size_t bytes;
} cases[] = {
    {"managed      32M", t_managed, 32 * MiB},
    {"managed     512M", t_managed, 512 * MiB},
    {"managed       2G", t_managed, 2 * GiB},
    {"hostalloc    64M", t_hostalloc, 64 * MiB},
    {"hostalloc     1G", t_hostalloc, 1 * GiB},
    {"hostalloc     2G", t_hostalloc, 2 * GiB},
    {"hostregister 64M", t_hostregister, 64 * MiB},
    {"hostregister  1G", t_hostregister, 1 * GiB},
    {"hostregister  2G", t_hostregister, 2 * GiB},
    {"devalloc     16G", t_devalloc, 16 * GiB},
};

static double now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
  int failed = 0;
  int timeout = getenv("CUDA_MEM_TIMEOUT") ? atoi(getenv("CUDA_MEM_TIMEOUT"))
                                           : 120;
  setvbuf(stdout, NULL, _IONBF, 0);
  for (size_t c = 0; c < sizeof cases / sizeof *cases; c++) {
    const struct tcase *t = &cases[c];
    char squeezed[64];
    size_t k = 0;
    for (const char *s = t->name; *s && k < sizeof squeezed - 1; s++)
      if (*s != ' ')
        squeezed[k++] = *s;
    squeezed[k] = 0;
    if (argc > 1 && !strstr(t->name, argv[1]) && !strstr(squeezed, argv[1]))
      continue;

    int fds[2];
    if (pipe(fds))
      return 2;
    double t0 = now();
    pid_t pid = fork();
    if (pid == 0) {
      close(fds[0]);
      alarm(timeout);
      load();
      int rc = setup() ? -1 : t->fn(t->bytes);
      if (rc && write(fds[1], why, strlen(why)) < 0)
        _exit(3);
      _exit(rc ? 1 : 0);
    }
    close(fds[1]);
    char msg[256] = "";
    ssize_t got = read(fds[0], msg, sizeof msg - 1);
    msg[got > 0 ? got : 0] = 0;
    close(fds[0]);
    int st;
    waitpid(pid, &st, 0);
    double dt = now() - t0;
    int ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
    if (!ok && !msg[0]) {
      if (WIFSIGNALED(st))
        snprintf(msg, sizeof msg, "killed by signal %d%s", WTERMSIG(st),
                 WTERMSIG(st) == SIGALRM ? " (timeout)" : "");
      else
        snprintf(msg, sizeof msg, "exit %d", WEXITSTATUS(st));
    }
    printf("%-18s %s  %6.2fs  %s\n", t->name, ok ? "PASS" : "FAIL", dt, msg);
    failed += !ok;
  }
  return failed;
}

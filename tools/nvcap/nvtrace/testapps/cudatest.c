/*
 * cudatest: CUDA driver API smoke test.
 *
 * Loads nvcuda.dll at run time (no CUDA toolkit needed), lists devices, picks
 * the first device, creates a context, JIT-compiles an embedded PTX vector-add
 * kernel (c[i] = a[i] + b[i] over 1<<20 floats), launches it, waits with a
 * timeout, copies the result back and checks every element.
 * Prints step-by-step progress and a final "PASS" or "FAIL: <reason>" line.
 */
#include "common.h"
#include <stdint.h>
#include <stdlib.h>

/* ---- minimal CUDA driver API declarations (cuda.h is not needed) ---- */
typedef int CUresult;
typedef int CUdevice;
typedef struct CUctx_st *CUcontext;
typedef struct CUmod_st *CUmodule;
typedef struct CUfunc_st *CUfunction;
typedef struct CUstream_st *CUstream;
typedef unsigned long long CUdeviceptr; /* 64-bit with the _v2 entry points */
typedef int CUjit_option;

#define CUDA_SUCCESS 0
#define CUDA_ERROR_NOT_READY 600
#define CU_DEVICE_ATTRIBUTE_MULTIPROCESSOR_COUNT 16
#define CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR 75
#define CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR 76
#define CU_JIT_INFO_LOG_BUFFER 3
#define CU_JIT_INFO_LOG_BUFFER_SIZE_BYTES 4
#define CU_JIT_ERROR_LOG_BUFFER 5
#define CU_JIT_ERROR_LOG_BUFFER_SIZE_BYTES 6
#define CU_JIT_LOG_VERBOSE 12

#define CUAPI __stdcall
typedef CUresult(CUAPI *PFN_cuInit)(unsigned int);
typedef CUresult(CUAPI *PFN_cuDriverGetVersion)(int *);
typedef CUresult(CUAPI *PFN_cuDeviceGetCount)(int *);
typedef CUresult(CUAPI *PFN_cuDeviceGet)(CUdevice *, int);
typedef CUresult(CUAPI *PFN_cuDeviceGetName)(char *, int, CUdevice);
typedef CUresult(CUAPI *PFN_cuDeviceGetAttribute)(int *, int, CUdevice);
typedef CUresult(CUAPI *PFN_cuDeviceTotalMem_v2)(size_t *, CUdevice);
typedef CUresult(CUAPI *PFN_cuDeviceGetLuid)(char *, unsigned int *, CUdevice);
typedef CUresult(CUAPI *PFN_cuCtxCreate_v2)(CUcontext *, unsigned int, CUdevice);
typedef CUresult(CUAPI *PFN_cuCtxDestroy_v2)(CUcontext);
typedef CUresult(CUAPI *PFN_cuCtxSynchronize)(void);
typedef CUresult(CUAPI *PFN_cuMemAlloc_v2)(CUdeviceptr *, size_t);
typedef CUresult(CUAPI *PFN_cuMemFree_v2)(CUdeviceptr);
typedef CUresult(CUAPI *PFN_cuMemcpyHtoD_v2)(CUdeviceptr, const void *, size_t);
typedef CUresult(CUAPI *PFN_cuMemcpyDtoH_v2)(void *, CUdeviceptr, size_t);
typedef CUresult(CUAPI *PFN_cuModuleLoadDataEx)(CUmodule *, const void *, unsigned int,
                                                CUjit_option *, void **);
typedef CUresult(CUAPI *PFN_cuModuleUnload)(CUmodule);
typedef CUresult(CUAPI *PFN_cuModuleGetFunction)(CUfunction *, CUmodule, const char *);
typedef CUresult(CUAPI *PFN_cuLaunchKernel)(CUfunction, unsigned int, unsigned int, unsigned int,
                                            unsigned int, unsigned int, unsigned int,
                                            unsigned int, CUstream, void **, void **);
typedef CUresult(CUAPI *PFN_cuStreamQuery)(CUstream);
typedef CUresult(CUAPI *PFN_cuGetErrorName)(CUresult, const char **);
typedef CUresult(CUAPI *PFN_cuGetErrorString)(CUresult, const char **);

static PFN_cuGetErrorName p_cuGetErrorName;
static PFN_cuGetErrorString p_cuGetErrorString;

/*
 * Vector add kernel. The driver JIT-compiles the PTX for the real GPU. The
 * first header (PTX ISA 6.0, sm_52) is the most portable; the second is a
 * fallback in case a future driver's JIT no longer accepts sm_5x targets.
 */
static const char *const ptx_headers[] = {
    ".version 6.0\n.target sm_52\n.address_size 64\n",
    ".version 7.0\n.target sm_75\n.address_size 64\n",
};
static const char vadd_ptx_body[] =
    "\n"
    ".visible .entry vadd(\n"
    "    .param .u64 vadd_param_a,\n"
    "    .param .u64 vadd_param_b,\n"
    "    .param .u64 vadd_param_c,\n"
    "    .param .u32 vadd_param_n\n"
    ")\n"
    "{\n"
    "    .reg .pred %p<2>;\n"
    "    .reg .b32 %r<6>;\n"
    "    .reg .f32 %f<4>;\n"
    "    .reg .b64 %rd<11>;\n"
    "\n"
    "    ld.param.u64 %rd1, [vadd_param_a];\n"
    "    ld.param.u64 %rd2, [vadd_param_b];\n"
    "    ld.param.u64 %rd3, [vadd_param_c];\n"
    "    ld.param.u32 %r1, [vadd_param_n];\n"
    "    mov.u32 %r2, %ctaid.x;\n"
    "    mov.u32 %r3, %ntid.x;\n"
    "    mov.u32 %r4, %tid.x;\n"
    "    mad.lo.s32 %r5, %r2, %r3, %r4;\n"
    "    setp.ge.u32 %p1, %r5, %r1;\n"
    "    @%p1 bra $L_done;\n"
    "    cvta.to.global.u64 %rd4, %rd1;\n"
    "    cvta.to.global.u64 %rd5, %rd2;\n"
    "    cvta.to.global.u64 %rd6, %rd3;\n"
    "    mul.wide.u32 %rd7, %r5, 4;\n"
    "    add.s64 %rd8, %rd4, %rd7;\n"
    "    add.s64 %rd9, %rd5, %rd7;\n"
    "    add.s64 %rd10, %rd6, %rd7;\n"
    "    ld.global.f32 %f1, [%rd8];\n"
    "    ld.global.f32 %f2, [%rd9];\n"
    "    add.f32 %f3, %f1, %f2;\n"
    "    st.global.f32 [%rd10], %f3;\n"
    "$L_done:\n"
    "    ret;\n"
    "}\n";

static const char *cu_err(CUresult r)
{
    static char buf[512];
    const char *name = NULL, *str = NULL;
    if (p_cuGetErrorName)
        p_cuGetErrorName(r, &name);
    if (p_cuGetErrorString)
        p_cuGetErrorString(r, &str);
    snprintf(buf, sizeof(buf), "CUresult %d (%s: %s)", r, name ? name : "?", str ? str : "?");
    return buf;
}

#define CU_CHECK(call)                                                   \
    do {                                                                 \
        CUresult r_ = (call);                                            \
        if (r_ != CUDA_SUCCESS)                                          \
            return fail("%s failed: %s", #call, cu_err(r_));             \
    } while (0)

#define LOAD(name)                                                       \
    p_##name = (PFN_##name)load_sym(lib, #name);                         \
    if (!p_##name)                                                       \
        return fail("nvcuda.dll does not export %s", #name)

#define N (1u << 20)
#define BLOCK 256u

static float *h_a, *h_b, *h_c;

int main(void)
{
    HMODULE lib;
    PFN_cuInit p_cuInit;
    PFN_cuDriverGetVersion p_cuDriverGetVersion;
    PFN_cuDeviceGetCount p_cuDeviceGetCount;
    PFN_cuDeviceGet p_cuDeviceGet;
    PFN_cuDeviceGetName p_cuDeviceGetName;
    PFN_cuDeviceGetAttribute p_cuDeviceGetAttribute;
    PFN_cuDeviceTotalMem_v2 p_cuDeviceTotalMem_v2;
    PFN_cuDeviceGetLuid p_cuDeviceGetLuid;
    PFN_cuCtxCreate_v2 p_cuCtxCreate_v2;
    PFN_cuCtxDestroy_v2 p_cuCtxDestroy_v2;
    PFN_cuCtxSynchronize p_cuCtxSynchronize;
    PFN_cuMemAlloc_v2 p_cuMemAlloc_v2;
    PFN_cuMemFree_v2 p_cuMemFree_v2;
    PFN_cuMemcpyHtoD_v2 p_cuMemcpyHtoD_v2;
    PFN_cuMemcpyDtoH_v2 p_cuMemcpyDtoH_v2;
    PFN_cuModuleLoadDataEx p_cuModuleLoadDataEx;
    PFN_cuModuleUnload p_cuModuleUnload;
    PFN_cuModuleGetFunction p_cuModuleGetFunction;
    PFN_cuLaunchKernel p_cuLaunchKernel;
    PFN_cuStreamQuery p_cuStreamQuery;
    int ver = 0, count = 0, i, major = 0, minor = 0, sms = 0;
    CUdevice dev;
    char name[256];
    size_t total = 0;
    CUcontext ctx;
    CUmodule mod = NULL;
    CUfunction fn;
    CUdeviceptr d_a, d_b, d_c;
    char info_log[8192], err_log[8192], ptx[4096];
    CUjit_option opts[5];
    void *optvals[5];
    CUresult r;
    unsigned int n = N, grid = (N + BLOCK - 1) / BLOCK, bad = 0;
    void *args[4];
    double t0;
    size_t bytes = (size_t)N * sizeof(float);

    common_init("cudatest");

    logmsg("loading nvcuda.dll");
    lib = LoadLibraryA("nvcuda.dll");
    if (!lib)
        return fail("LoadLibrary(nvcuda.dll) failed, error %lu (NVIDIA driver not installed?)",
                    (unsigned long)GetLastError());
    p_cuGetErrorName = (PFN_cuGetErrorName)load_sym(lib, "cuGetErrorName");
    p_cuGetErrorString = (PFN_cuGetErrorString)load_sym(lib, "cuGetErrorString");
    LOAD(cuInit);
    LOAD(cuDriverGetVersion);
    LOAD(cuDeviceGetCount);
    LOAD(cuDeviceGet);
    LOAD(cuDeviceGetName);
    LOAD(cuDeviceGetAttribute);
    LOAD(cuDeviceTotalMem_v2);
    LOAD(cuCtxCreate_v2);
    LOAD(cuCtxDestroy_v2);
    LOAD(cuCtxSynchronize);
    LOAD(cuMemAlloc_v2);
    LOAD(cuMemFree_v2);
    LOAD(cuMemcpyHtoD_v2);
    LOAD(cuMemcpyDtoH_v2);
    LOAD(cuModuleLoadDataEx);
    LOAD(cuModuleUnload);
    LOAD(cuModuleGetFunction);
    LOAD(cuLaunchKernel);
    LOAD(cuStreamQuery);
    p_cuDeviceGetLuid = (PFN_cuDeviceGetLuid)load_sym(lib, "cuDeviceGetLuid"); /* optional */

    logmsg("cuInit(0)");
    CU_CHECK(p_cuInit(0));
    CU_CHECK(p_cuDriverGetVersion(&ver));
    logmsg("driver CUDA version %d.%d", ver / 1000, (ver % 1000) / 10);
    CU_CHECK(p_cuDeviceGetCount(&count));
    logmsg("%d CUDA device(s)", count);
    if (count < 1)
        return fail("no CUDA devices");
    for (i = 0; i < count; i++) {
        CU_CHECK(p_cuDeviceGet(&dev, i));
        CU_CHECK(p_cuDeviceGetName(name, (int)sizeof(name), dev));
        CU_CHECK(p_cuDeviceGetAttribute(&major, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR, dev));
        CU_CHECK(p_cuDeviceGetAttribute(&minor, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR, dev));
        CU_CHECK(p_cuDeviceGetAttribute(&sms, CU_DEVICE_ATTRIBUTE_MULTIPROCESSOR_COUNT, dev));
        CU_CHECK(p_cuDeviceTotalMem_v2(&total, dev));
        logmsg("device %d: %s, sm_%d%d, %d SMs, %llu MiB", i, name, major, minor, sms,
               (unsigned long long)(total >> 20));
        if (p_cuDeviceGetLuid) {
            char luid[8];
            unsigned int mask = 0;
            LUID l;
            if (p_cuDeviceGetLuid(luid, &mask, dev) == CUDA_SUCCESS) {
                memcpy(&l, luid, sizeof(l));
                logmsg("device %d: LUID %08lx:%08lx node mask 0x%x", i,
                       (unsigned long)l.HighPart, (unsigned long)l.LowPart, mask);
            }
        }
    }

    CU_CHECK(p_cuDeviceGet(&dev, 0));
    logmsg("using device 0; cuCtxCreate_v2");
    CU_CHECK(p_cuCtxCreate_v2(&ctx, 0, dev));

    r = CUDA_SUCCESS;
    for (i = 0; i < (int)(sizeof(ptx_headers) / sizeof(ptx_headers[0])); i++) {
        snprintf(ptx, sizeof(ptx), "%s%s", ptx_headers[i], vadd_ptx_body);
        logmsg("cuModuleLoadDataEx (PTX JIT, %u bytes, header variant %d: %.24s)",
               (unsigned)strlen(ptx), i, ptx_headers[i]);
        info_log[0] = err_log[0] = 0;
        opts[0] = CU_JIT_INFO_LOG_BUFFER;
        optvals[0] = info_log;
        opts[1] = CU_JIT_INFO_LOG_BUFFER_SIZE_BYTES;
        optvals[1] = (void *)(uintptr_t)sizeof(info_log);
        opts[2] = CU_JIT_ERROR_LOG_BUFFER;
        optvals[2] = err_log;
        opts[3] = CU_JIT_ERROR_LOG_BUFFER_SIZE_BYTES;
        optvals[3] = (void *)(uintptr_t)sizeof(err_log);
        opts[4] = CU_JIT_LOG_VERBOSE;
        optvals[4] = (void *)(uintptr_t)1;
        t0 = now_ms();
        r = p_cuModuleLoadDataEx(&mod, ptx, 5, opts, optvals);
        info_log[sizeof(info_log) - 1] = err_log[sizeof(err_log) - 1] = 0;
        if (info_log[0])
            logmsg("JIT info log:\n%s", info_log);
        if (r == CUDA_SUCCESS) {
            logmsg("JIT done in %.1f ms", now_ms() - t0);
            break;
        }
        logmsg("cuModuleLoadDataEx failed: %s", cu_err(r));
        logmsg("JIT error log:\n%s", err_log[0] ? err_log : "(empty)");
    }
    if (r != CUDA_SUCCESS)
        return fail("cuModuleLoadDataEx failed for every PTX variant: %s", cu_err(r));
    CU_CHECK(p_cuModuleGetFunction(&fn, mod, "vadd"));

    logmsg("allocating 3 x %llu bytes host + device", (unsigned long long)bytes);
    h_a = (float *)malloc(bytes);
    h_b = (float *)malloc(bytes);
    h_c = (float *)malloc(bytes);
    if (!h_a || !h_b || !h_c)
        return fail("host malloc failed");
    for (i = 0; i < (int)N; i++) {
        /* small integers: the float sums are exact */
        h_a[i] = (float)(i & 0xFFFF);
        h_b[i] = (float)((i * 7) & 0xFFF);
        h_c[i] = -1.0f;
    }
    CU_CHECK(p_cuMemAlloc_v2(&d_a, bytes));
    CU_CHECK(p_cuMemAlloc_v2(&d_b, bytes));
    CU_CHECK(p_cuMemAlloc_v2(&d_c, bytes));
    logmsg("device pointers a=0x%llx b=0x%llx c=0x%llx", d_a, d_b, d_c);
    CU_CHECK(p_cuMemcpyHtoD_v2(d_a, h_a, bytes));
    CU_CHECK(p_cuMemcpyHtoD_v2(d_b, h_b, bytes));
    CU_CHECK(p_cuMemcpyHtoD_v2(d_c, h_c, bytes)); /* poison the output */

    logmsg("cuLaunchKernel grid %u block %u", grid, BLOCK);
    args[0] = &d_a;
    args[1] = &d_b;
    args[2] = &d_c;
    args[3] = &n;
    t0 = now_ms();
    CU_CHECK(p_cuLaunchKernel(fn, grid, 1, 1, BLOCK, 1, 1, 0, NULL, args, NULL));
    for (;;) {
        r = p_cuStreamQuery(NULL);
        if (r == CUDA_SUCCESS)
            break;
        if (r != CUDA_ERROR_NOT_READY)
            return fail("cuStreamQuery failed: %s", cu_err(r));
        if (now_ms() - t0 > WAIT_TIMEOUT_MS)
            return fail("kernel did not finish within %u ms", WAIT_TIMEOUT_MS);
        Sleep(1);
    }
    CU_CHECK(p_cuCtxSynchronize());
    logmsg("kernel finished in %.2f ms", now_ms() - t0);

    CU_CHECK(p_cuMemcpyDtoH_v2(h_c, d_c, bytes));
    for (i = 0; i < (int)N; i++) {
        float want = h_a[i] + h_b[i];
        if (h_c[i] != want) {
            if (bad < 5)
                logmsg("mismatch at %d: got %f want %f", i, (double)h_c[i], (double)want);
            bad++;
        }
    }
    if (bad)
        return fail("%u of %u elements wrong", bad, N);
    logmsg("all %u elements correct", N);

    logmsg("cleanup");
    CU_CHECK(p_cuMemFree_v2(d_a));
    CU_CHECK(p_cuMemFree_v2(d_b));
    CU_CHECK(p_cuMemFree_v2(d_c));
    CU_CHECK(p_cuModuleUnload(mod));
    CU_CHECK(p_cuCtxDestroy_v2(ctx));
    free(h_a);
    free(h_b);
    free(h_c);
    return pass();
}

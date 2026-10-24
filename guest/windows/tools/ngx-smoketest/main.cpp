// ngx-smoketest: runs DLSS super resolution through NVIDIA's NGX runtime on
// synthetic inputs, without a game. See README.md.
//
// Exit code: 0 every step succeeded, 1 a step failed, 2 bad arguments,
// 3 a wait timed out (or the watchdog fired), 4 the process crashed. The
// process ends with TerminateProcess after the RESULT line.
#include "smoketest.h"

#include <shlobj.h>
#include <stdarg.h>
#include <winver.h>

Options g_opt;

static CRITICAL_SECTION out_lock;
static const char *cur_step = "start";
static const char *first_failed_step;
static int n_failures, n_warnings;
static HANDLE done_event;
static HANDLE ngx_log_file = INVALID_HANDLE_VALUE;

static void vout(const char *prefix, const char *fmt, va_list ap)
{
    char buf[4096];
    int n = vsnprintf(buf, sizeof(buf), fmt, ap);
    if (n < 0)
        return;
    EnterCriticalSection(&out_lock);
    fputs(prefix, stdout);
    fputs(buf, stdout);
    fputc('\n', stdout);
    fflush(stdout);
    LeaveCriticalSection(&out_lock);
}

// Without the lock: for the crash filter and the watchdog, which may run
// while another thread holds it.
static void raw_out(const char *fmt, ...)
{
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof(buf) - 2, fmt, ap);
    va_end(ap);
    if (n < 0)
        return;
    if (n > (int)sizeof(buf) - 3)
        n = (int)sizeof(buf) - 3;
    buf[n++] = '\r';
    buf[n++] = '\n';
    DWORD w;
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), buf, (DWORD)n, &w, NULL);
}

void out(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vout("", fmt, ap);
    va_end(ap);
}

void step(const char *name)
{
    cur_step = name;
}

void ok(const char *fmt, ...)
{
    char p[160];
    snprintf(p, sizeof(p), "[ok]   %s: ", cur_step);
    va_list ap;
    va_start(ap, fmt);
    vout(p, fmt, ap);
    va_end(ap);
}

void fail(const char *fmt, ...)
{
    char p[160];
    snprintf(p, sizeof(p), "[FAIL] %s: ", cur_step);
    n_failures++;
    if (!first_failed_step)
        first_failed_step = cur_step;
    va_list ap;
    va_start(ap, fmt);
    vout(p, fmt, ap);
    va_end(ap);
}

void warn(const char *fmt, ...)
{
    char p[160];
    snprintf(p, sizeof(p), "[warn] %s: ", cur_step);
    n_warnings++;
    va_list ap;
    va_start(ap, fmt);
    vout(p, fmt, ap);
    va_end(ap);
}

int failures()
{
    return n_failures;
}

void hang(const char *fmt, ...)
{
    char p[160];
    snprintf(p, sizeof(p), "[FAIL] %s: ", cur_step);
    va_list ap;
    va_start(ap, fmt);
    vout(p, fmt, ap);
    va_end(ap);
    out("RESULT: FAIL (hang/timeout in step '%s'; exiting without cleanup)", cur_step);
    TerminateProcess(GetCurrentProcess(), 3);
    for (;;)
        Sleep(1000);
}

/* ---- crash and hang guards ---- */

static void module_of(const void *addr, char *name, size_t len)
{
    HMODULE m = NULL;
    snprintf(name, len, "?");
    if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                               GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           (LPCSTR)addr, &m) && m) {
        char path[MAX_PATH];
        if (GetModuleFileNameA(m, path, sizeof(path))) {
            const char *b = strrchr(path, '\\');
            snprintf(name, len, "%s+0x%llx", b ? b + 1 : path,
                     (unsigned long long)((const char *)addr - (const char *)m));
        }
    }
}

static LONG WINAPI crash_filter(EXCEPTION_POINTERS *ep)
{
    char where[MAX_PATH + 32];
    module_of(ep->ExceptionRecord->ExceptionAddress, where, sizeof(where));
    raw_out("[FAIL] %s: crash: exception 0x%08lx at %p (%s)", cur_step,
        (unsigned long)ep->ExceptionRecord->ExceptionCode,
        ep->ExceptionRecord->ExceptionAddress, where);
    if (ep->ExceptionRecord->ExceptionCode == EXCEPTION_ACCESS_VIOLATION &&
        ep->ExceptionRecord->NumberParameters >= 2)
        raw_out("       access violation: %s address 0x%llx",
            ep->ExceptionRecord->ExceptionInformation[0] == 0   ? "read"
            : ep->ExceptionRecord->ExceptionInformation[0] == 1 ? "write"
                                                                : "execute",
            (unsigned long long)ep->ExceptionRecord->ExceptionInformation[1]);
    raw_out("RESULT: FAIL (crash in step '%s')", cur_step);
    TerminateProcess(GetCurrentProcess(), 4);
    return EXCEPTION_EXECUTE_HANDLER;
}

static DWORD WINAPI watchdog(void *)
{
    if (WaitForSingleObject(done_event, g_opt.watchdog_s * 1000u) == WAIT_TIMEOUT) {
        raw_out("[FAIL] %s: watchdog: the run did not finish in %lu s", cur_step,
            (unsigned long)g_opt.watchdog_s);
        raw_out("RESULT: FAIL (watchdog in step '%s')", cur_step);
        TerminateProcess(GetCurrentProcess(), 3);
    }
    return 0;
}

/* ---- NGX ---- */

const char *ngx_result_name(NVSDK_NGX_Result r)
{
    switch ((unsigned)r) {
    case NVSDK_NGX_Result_Success: return "Success";
    case NVSDK_NGX_Result_Fail: return "Fail";
    case NVSDK_NGX_Result_FAIL_FeatureNotSupported: return "FAIL_FeatureNotSupported";
    case NVSDK_NGX_Result_FAIL_PlatformError: return "FAIL_PlatformError";
    case NVSDK_NGX_Result_FAIL_FeatureAlreadyExists: return "FAIL_FeatureAlreadyExists";
    case NVSDK_NGX_Result_FAIL_FeatureNotFound: return "FAIL_FeatureNotFound";
    case NVSDK_NGX_Result_FAIL_InvalidParameter: return "FAIL_InvalidParameter";
    case NVSDK_NGX_Result_FAIL_ScratchBufferTooSmall: return "FAIL_ScratchBufferTooSmall";
    case NVSDK_NGX_Result_FAIL_NotInitialized: return "FAIL_NotInitialized";
    case NVSDK_NGX_Result_FAIL_UnsupportedInputFormat: return "FAIL_UnsupportedInputFormat";
    case NVSDK_NGX_Result_FAIL_RWFlagMissing: return "FAIL_RWFlagMissing";
    case NVSDK_NGX_Result_FAIL_MissingInput: return "FAIL_MissingInput";
    case NVSDK_NGX_Result_FAIL_UnableToInitializeFeature: return "FAIL_UnableToInitializeFeature";
    case NVSDK_NGX_Result_FAIL_OutOfDate: return "FAIL_OutOfDate";
    case NVSDK_NGX_Result_FAIL_OutOfGPUMemory: return "FAIL_OutOfGPUMemory";
    case NVSDK_NGX_Result_FAIL_UnsupportedFormat: return "FAIL_UnsupportedFormat";
    case NVSDK_NGX_Result_FAIL_UnableToWriteToAppDataPath: return "FAIL_UnableToWriteToAppDataPath";
    case NVSDK_NGX_Result_FAIL_UnsupportedParameter: return "FAIL_UnsupportedParameter";
    case NVSDK_NGX_Result_FAIL_Denied: return "FAIL_Denied";
    case NVSDK_NGX_Result_FAIL_NotImplemented: return "FAIL_NotImplemented";
    }
    return NVSDK_NGX_SUCCEED(r) ? "Success(other)" : "FAIL(unknown code)";
}

bool ngx_check(const char *what, NVSDK_NGX_Result r)
{
    if (NVSDK_NGX_SUCCEED(r)) {
        ok("%s -> 0x%08x %s", what, (unsigned)r, ngx_result_name(r));
        return true;
    }
    fail("%s -> 0x%08x %s", what, (unsigned)r, ngx_result_name(r));
    return false;
}

static bool contains_nocase(const char *s, const char *word)
{
    size_t n = strlen(word);
    for (; *s; s++)
        if (!_strnicmp(s, word, n))
            return true;
    return false;
}

static void NVSDK_CONV ngx_log(const char *message, NVSDK_NGX_Logging_Level level,
                               NVSDK_NGX_Feature component)
{
    char line[2048];
    int n = snprintf(line, sizeof(line), "[%d/%d] %s", (int)level, (int)component,
                     message ? message : "");
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r'))
        line[--n] = 0;
    if (ngx_log_file != INVALID_HANDLE_VALUE) {
        DWORD w;
        EnterCriticalSection(&out_lock);
        WriteFile(ngx_log_file, line, (DWORD)n, &w, NULL);
        WriteFile(ngx_log_file, "\r\n", 2, &w, NULL);
        LeaveCriticalSection(&out_lock);
    }
    if (g_opt.verbose || contains_nocase(line, "error") || contains_nocase(line, "fail") ||
        contains_nocase(line, "warn"))
        out("  NGX| %s", line);
}

void ngx_common_info(NVSDK_NGX_FeatureCommonInfo *ci)
{
    static const wchar_t *paths[1];
    paths[0] = g_opt.exedir;
    memset(ci, 0, sizeof(*ci));
    ci->PathListInfo.Path = paths;
    ci->PathListInfo.Length = 1;
    ci->LoggingInfo.LoggingCallback = ngx_log;
    ci->LoggingInfo.MinimumLoggingLevel = NVSDK_NGX_LOGGING_LEVEL_VERBOSE;
    ci->LoggingInfo.DisableOtherLoggingSinks = false;
}

void ngx_discovery_info(NVSDK_NGX_FeatureDiscoveryInfo *di, const NVSDK_NGX_FeatureCommonInfo *ci)
{
    memset(di, 0, sizeof(*di));
    di->SDKVersion = NVSDK_NGX_Version_API;
    di->FeatureID = NVSDK_NGX_Feature_SuperSampling;
    if (g_opt.use_app_id) {
        di->Identifier.IdentifierType = NVSDK_NGX_Application_Identifier_Type_Application_Id;
        di->Identifier.v.ApplicationId = g_opt.app_id;
    } else {
        di->Identifier.IdentifierType = NVSDK_NGX_Application_Identifier_Type_Project_Id;
        di->Identifier.v.ProjectDesc.ProjectId = g_opt.project_id;
        di->Identifier.v.ProjectDesc.EngineType = NVSDK_NGX_ENGINE_TYPE_CUSTOM;
        di->Identifier.v.ProjectDesc.EngineVersion = "1.0";
    }
    di->ApplicationDataPath = g_opt.logdir;
    di->FeatureInfo = ci;
}

void ngx_print_requirement(const NVSDK_NGX_FeatureRequirement *req)
{
    unsigned s = (unsigned)req->FeatureSupported;
    char why[200] = "";
    if (s == 0)
        snprintf(why, sizeof(why), "Supported");
    if (s & NVSDK_NGX_FeatureSupportResult_CheckNotPresent) strcat(why, "CheckNotPresent ");
    if (s & NVSDK_NGX_FeatureSupportResult_DriverVersionUnsupported) strcat(why, "DriverVersionUnsupported ");
    if (s & NVSDK_NGX_FeatureSupportResult_AdapterUnsupported) strcat(why, "AdapterUnsupported ");
    if (s & NVSDK_NGX_FeatureSupportResult_OSVersionBelowMinimumSupported) strcat(why, "OSVersionBelowMinimumSupported ");
    if (s & NVSDK_NGX_FeatureSupportResult_NotImplemented) strcat(why, "NotImplemented ");
    out("       FeatureSupported = 0x%x (%s), MinHWArchitecture = 0x%x, MinOSVersion = \"%.64s\"",
        s, why, req->MinHWArchitecture, req->MinOSVersion);
}

static void print_param_i(NVSDK_NGX_Parameter *p, const char *name, int *value)
{
    int v = 0;
    NVSDK_NGX_Result r = NVSDK_NGX_Parameter_GetI(p, name, &v);
    if (NVSDK_NGX_SUCCEED(r))
        out("       %s = %d", name, v);
    else
        out("       %s = (not set: 0x%08x %s)", name, (unsigned)r, ngx_result_name(r));
    if (value)
        *value = NVSDK_NGX_SUCCEED(r) ? v : -1;
}

static void print_param_ui(NVSDK_NGX_Parameter *p, const char *name)
{
    unsigned v = 0;
    NVSDK_NGX_Result r = NVSDK_NGX_Parameter_GetUI(p, name, &v);
    if (NVSDK_NGX_SUCCEED(r))
        out("       %s = %u", name, v);
    else
        out("       %s = (not set: 0x%08x %s)", name, (unsigned)r, ngx_result_name(r));
}

bool ngx_print_caps(NVSDK_NGX_Parameter *caps)
{
    int avail = -1, init_result = 0;
    print_param_i(caps, NVSDK_NGX_Parameter_SuperSampling_Available, &avail);
    print_param_i(caps, NVSDK_NGX_Parameter_SuperSampling_NeedsUpdatedDriver, NULL);
    print_param_ui(caps, NVSDK_NGX_Parameter_SuperSampling_MinDriverVersionMajor);
    print_param_ui(caps, NVSDK_NGX_Parameter_SuperSampling_MinDriverVersionMinor);
    NVSDK_NGX_Result r = NVSDK_NGX_Parameter_GetI(caps, NVSDK_NGX_Parameter_SuperSampling_FeatureInitResult,
                                                  &init_result);
    if (NVSDK_NGX_SUCCEED(r))
        out("       %s = 0x%08x %s", NVSDK_NGX_Parameter_SuperSampling_FeatureInitResult,
            (unsigned)init_result, ngx_result_name((NVSDK_NGX_Result)init_result));
    else
        out("       %s = (not set: 0x%08x %s)", NVSDK_NGX_Parameter_SuperSampling_FeatureInitResult,
            (unsigned)r, ngx_result_name(r));
    void *cb = NULL;
    NVSDK_NGX_Parameter_GetVoidPointer(caps, NVSDK_NGX_Parameter_DLSSOptimalSettingsCallback, &cb);
    out("       %s = %s", NVSDK_NGX_Parameter_DLSSOptimalSettingsCallback, cb ? "present" : "absent");
    return avail == 1;
}

void ngx_render_size(NVSDK_NGX_Parameter *caps, unsigned *w, unsigned *h)
{
    static const double ratio[] = {0.5, 0.58, 0.667, 0.333, 0.77, 1.0}; // by PerfQuality_Value
    unsigned ow = g_opt.out_w, oh = g_opt.out_h;
    unsigned fw = (unsigned)(ow * ratio[g_opt.quality] + 0.5), fh = (unsigned)(oh * ratio[g_opt.quality] + 0.5);
    unsigned opt_w = 0, opt_h = 0, min_w = 0, min_h = 0, max_w = 0, max_h = 0;
    void *cb = NULL;
    NVSDK_NGX_Parameter_GetVoidPointer(caps, NVSDK_NGX_Parameter_DLSSOptimalSettingsCallback, &cb);
    if (cb) {
        NVSDK_NGX_Parameter_SetUI(caps, NVSDK_NGX_Parameter_Width, ow);
        NVSDK_NGX_Parameter_SetUI(caps, NVSDK_NGX_Parameter_Height, oh);
        NVSDK_NGX_Parameter_SetI(caps, NVSDK_NGX_Parameter_PerfQualityValue, g_opt.quality);
        NVSDK_NGX_Parameter_SetI(caps, NVSDK_NGX_Parameter_RTXValue, 0);
        NVSDK_NGX_Result r = ((PFN_NVSDK_NGX_DLSS_GetOptimalSettingsCallback)cb)(caps);
        if (NVSDK_NGX_SUCCEED(r)) {
            NVSDK_NGX_Parameter_GetUI(caps, NVSDK_NGX_Parameter_OutWidth, &opt_w);
            NVSDK_NGX_Parameter_GetUI(caps, NVSDK_NGX_Parameter_OutHeight, &opt_h);
            NVSDK_NGX_Parameter_GetUI(caps, NVSDK_NGX_Parameter_DLSS_Get_Dynamic_Min_Render_Width, &min_w);
            NVSDK_NGX_Parameter_GetUI(caps, NVSDK_NGX_Parameter_DLSS_Get_Dynamic_Min_Render_Height, &min_h);
            NVSDK_NGX_Parameter_GetUI(caps, NVSDK_NGX_Parameter_DLSS_Get_Dynamic_Max_Render_Width, &max_w);
            NVSDK_NGX_Parameter_GetUI(caps, NVSDK_NGX_Parameter_DLSS_Get_Dynamic_Max_Render_Height, &max_h);
            ok("optimal settings for %ux%u %s -> 0x%08x %s: render %ux%u (min %ux%u, max %ux%u)", ow, oh,
               g_opt.quality_name, (unsigned)r, ngx_result_name(r), opt_w, opt_h, min_w, min_h, max_w, max_h);
        } else {
            warn("optimal settings callback -> 0x%08x %s", (unsigned)r, ngx_result_name(r));
        }
    } else {
        warn("no DLSSOptimalSettingsCallback (DLSS DLL not loaded yet or too old)");
    }
    if (g_opt.in_w) {
        *w = g_opt.in_w;
        *h = g_opt.in_h;
    } else if (opt_w && opt_h) {
        *w = opt_w;
        *h = opt_h;
    } else {
        *w = fw;
        *h = fh;
    }
    out("       render size %ux%u -> output %ux%u (%s)", *w, *h, ow, oh, g_opt.quality_name);
}

void ngx_set_create_hints(NVSDK_NGX_Parameter *p)
{
    const char *names[] = {
        NVSDK_NGX_Parameter_DLSS_Hint_Render_Preset_DLAA,
        NVSDK_NGX_Parameter_DLSS_Hint_Render_Preset_Quality,
        NVSDK_NGX_Parameter_DLSS_Hint_Render_Preset_Balanced,
        NVSDK_NGX_Parameter_DLSS_Hint_Render_Preset_Performance,
        NVSDK_NGX_Parameter_DLSS_Hint_Render_Preset_UltraPerformance,
        NVSDK_NGX_Parameter_DLSS_Hint_Render_Preset_UltraQuality,
    };
    for (const char *n : names)
        NVSDK_NGX_Parameter_SetUI(p, n, g_opt.preset);
}

int ngx_create_flags()
{
    // LDR color, motion vectors at render resolution, not jittered.
    return NVSDK_NGX_DLSS_Feature_Flags_MVLowRes;
}

static float halton(int i, int b)
{
    float f = 1.0f, r = 0.0f;
    while (i > 0) {
        f /= (float)b;
        r += f * (float)(i % b);
        i /= b;
    }
    return r;
}

void jitter(int frame, float *x, float *y)
{
    int i = (frame % 32) + 1;
    *x = halton(i, 2) - 0.5f;
    *y = halton(i, 3) - 0.5f;
}

/* ---- inputs and the output check ---- */

void make_color(uint8_t *rgba, unsigned w, unsigned h, size_t pitch)
{
    long cx = w / 2, cy = h / 2, rad = h / 4;
    for (unsigned y = 0; y < h; y++) {
        uint8_t *row = rgba + y * pitch;
        for (unsigned x = 0; x < w; x++) {
            uint8_t *p = row + x * 4;
            p[0] = (uint8_t)(x * 255 / (w > 1 ? w - 1 : 1));
            p[1] = (uint8_t)(y * 255 / (h > 1 ? h - 1 : 1));
            p[2] = (((x / 32) + (y / 32)) & 1) ? 200 : 60;
            p[3] = 255;
            long dx = (long)x - cx, dy = (long)y - cy;
            if (dx * dx + dy * dy < rad * rad)
                p[0] = p[1] = p[2] = 235;
            if ((x + 2 * y) % 41 == 0) /* thin diagonal lines: aliasing to resolve */
                p[0] = p[1] = p[2] = 10;
        }
    }
}

void make_depth(float *d, unsigned w, unsigned h, size_t pitch_bytes)
{
    for (unsigned y = 0; y < h; y++) {
        float *row = (float *)((uint8_t *)d + y * pitch_bytes);
        for (unsigned x = 0; x < w; x++)
            row[x] = 0.25f + 0.5f * (float)y / (float)(h ? h : 1);
    }
}

// <logdir>\<prefix><api><ext>; only %ls, which means the same in MSVCRT and
// MinGW wide printf.
static void log_path(wchar_t *p, const wchar_t *prefix, const wchar_t *ext)
{
    swprintf(p, MAX_PATH, L"%ls\\%ls%ls%ls", g_opt.logdir, prefix, strcmp(g_opt.api, "vk") ? L"d3d12" : L"vk", ext);
}

static bool write_bmp(const wchar_t *path, const uint8_t *rgba, unsigned w, unsigned h, size_t pitch)
{
    size_t row = ((size_t)w * 3 + 3) & ~(size_t)3;
    size_t size = 54 + row * h;
    uint8_t *buf = (uint8_t *)calloc(1, size);
    if (!buf)
        return false;
    uint8_t *hd = buf;
    hd[0] = 'B'; hd[1] = 'M';
    *(uint32_t *)(hd + 2) = (uint32_t)size;
    *(uint32_t *)(hd + 10) = 54;
    *(uint32_t *)(hd + 14) = 40;
    *(int32_t *)(hd + 18) = (int32_t)w;
    *(int32_t *)(hd + 22) = (int32_t)h;
    *(uint16_t *)(hd + 26) = 1;
    *(uint16_t *)(hd + 28) = 24;
    *(uint32_t *)(hd + 34) = (uint32_t)(row * h);
    for (unsigned y = 0; y < h; y++) {
        const uint8_t *src = rgba + (size_t)(h - 1 - y) * pitch;
        uint8_t *dst = buf + 54 + row * y;
        for (unsigned x = 0; x < w; x++) {
            dst[x * 3 + 0] = src[x * 4 + 2];
            dst[x * 3 + 1] = src[x * 4 + 1];
            dst[x * 3 + 2] = src[x * 4 + 0];
        }
    }
    HANDLE f = CreateFileW(path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    bool good = false;
    if (f != INVALID_HANDLE_VALUE) {
        DWORD wr = 0;
        good = WriteFile(f, buf, (DWORD)size, &wr, NULL) && wr == size;
        CloseHandle(f);
    }
    free(buf);
    return good;
}

bool check_output(const uint8_t *rgba, unsigned w, unsigned h, size_t pitch,
                  const uint8_t *in_rgba, unsigned in_w, unsigned in_h, size_t in_pitch)
{
    uint64_t hash = 0xcbf29ce484222325ull; // FNV-1a 64 over the RGBA bytes, row by row
    uint64_t nonzero = 0, untouched = 0, n = (uint64_t)w * h;
    unsigned mn[4] = {255, 255, 255, 255}, mx[4] = {0, 0, 0, 0};
    double sum[4] = {0, 0, 0, 0}, diff[3] = {0, 0, 0};
    uint32_t first = *(const uint32_t *)rgba;
    bool constant = true;
    for (unsigned y = 0; y < h; y++) {
        const uint8_t *row = rgba + (size_t)y * pitch;
        const uint8_t *irow = in_rgba + (size_t)(y * in_h / h) * in_pitch;
        for (unsigned x = 0; x < w; x++) {
            const uint8_t *p = row + x * 4;
            const uint8_t *q = irow + (size_t)(x * in_w / w) * 4;
            for (int c = 0; c < 4; c++) {
                hash = (hash ^ p[c]) * 0x100000001b3ull;
                if (p[c] < mn[c]) mn[c] = p[c];
                if (p[c] > mx[c]) mx[c] = p[c];
                sum[c] += p[c];
            }
            for (int c = 0; c < 3; c++)
                diff[c] += abs((int)p[c] - (int)q[c]);
            uint32_t v = *(const uint32_t *)p;
            if (v & 0x00ffffffu)
                nonzero++;
            if (!memcmp(p, kClear, 4))
                untouched++;
            if (v != first)
                constant = false;
        }
    }
    out("       checksum (FNV-1a 64, RGBA8) = %016llx", (unsigned long long)hash);
    out("       pixels with non-zero RGB = %llu of %llu; still the clear color (255,0,255) = %llu",
        (unsigned long long)nonzero, (unsigned long long)n, (unsigned long long)untouched);
    out("       min RGBA = %u %u %u %u, max RGBA = %u %u %u %u, mean RGBA = %.1f %.1f %.1f %.1f",
        mn[0], mn[1], mn[2], mn[3], mx[0], mx[1], mx[2], mx[3],
        sum[0] / n, sum[1] / n, sum[2] / n, sum[3] / n);
    out("       mean |output - input (nearest upscale)| RGB = %.1f %.1f %.1f (0..255; small = resembles the input)",
        diff[0] / n, diff[1] / n, diff[2] / n);
    const unsigned pts[][2] = {{0, 0}, {w / 4, h / 4}, {w / 2, h / 2}, {3 * w / 4, h / 4},
                               {w / 4, 3 * h / 4}, {w - 1, h - 1}};
    for (auto &pt : pts) {
        const uint8_t *p = rgba + (size_t)pt[1] * pitch + (size_t)pt[0] * 4;
        const uint8_t *q = in_rgba + (size_t)(pt[1] * in_h / h) * in_pitch + (size_t)(pt[0] * in_w / w) * 4;
        out("       pixel (%4u,%4u) = %3u %3u %3u %3u   input there = %3u %3u %3u", pt[0], pt[1],
            p[0], p[1], p[2], p[3], q[0], q[1], q[2]);
    }

    wchar_t path[MAX_PATH];
    log_path(path, L"ngx-output-", L".bmp");
    bool w1 = write_bmp(path, rgba, w, h, pitch);
    out("       %s %ls", w1 ? "wrote" : "could not write", path);
    log_path(path, L"ngx-input-", L".bmp");
    bool w2 = write_bmp(path, in_rgba, in_w, in_h, in_pitch);
    out("       %s %ls", w2 ? "wrote" : "could not write", path);

    if (nonzero == 0) {
        fail("the output is all zero (black)");
        return false;
    }
    if (untouched == n) {
        fail("the output was not written (still the clear color everywhere)");
        return false;
    }
    if (constant) {
        fail("the output is one constant value (0x%08x)", first);
        return false;
    }
    ok("output %ux%u has content", w, h);
    return true;
}

/* ---- timing ---- */

double now_ms()
{
    static LARGE_INTEGER freq;
    LARGE_INTEGER t;
    if (!freq.QuadPart)
        QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1000.0 / (double)freq.QuadPart;
}

void timings_alloc(Timings *t, int n)
{
    t->n = n;
    t->gpu = (double *)calloc(n, sizeof(double));
    t->cpu_eval = (double *)calloc(n, sizeof(double));
    t->cpu_frame = (double *)calloc(n, sizeof(double));
}

static int cmp_double(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static void stats_line(const char *name, const double *v, int n)
{
    // frame 0 includes the first-use cost; statistics over frames 1..n-1
    double *s = (double *)malloc(sizeof(double) * (n > 0 ? n : 1));
    int m = 0;
    for (int i = 1; i < n; i++)
        if (v[i] >= 0)
            s[m++] = v[i];
    if (n < 1 || v[0] < 0) {
        out("       %-24s n/a", name);
    } else if (m == 0) {
        out("       %-24s frame0 %.3f ms", name, v[0]);
    } else {
        qsort(s, m, sizeof(double), cmp_double);
        double sum = 0;
        for (int i = 0; i < m; i++)
            sum += s[i];
        out("       %-24s frame0 %8.3f | frames 1..%d: min %.3f  median %.3f  avg %.3f  p95 %.3f  max %.3f ms",
            name, v[0], n - 1, s[0], s[m / 2], sum / m, s[(int)(m * 0.95) < m ? (int)(m * 0.95) : m - 1],
            s[m - 1]);
    }
    free(s);
}

void timings_print(const Timings *t)
{
    stats_line("GPU per evaluate", t->gpu, t->n);
    stats_line("CPU EvaluateFeature call", t->cpu_eval, t->n);
    stats_line("CPU submit to signal", t->cpu_frame, t->n);
}

/* ---- environment report ---- */

static void file_version(const wchar_t *path, char *outv, size_t len)
{
    snprintf(outv, len, "no version");
    DWORD dummy = 0, size = GetFileVersionInfoSizeW(path, &dummy);
    if (!size)
        return;
    void *buf = malloc(size);
    VS_FIXEDFILEINFO *fi = NULL;
    UINT fl = 0;
    if (buf && GetFileVersionInfoW(path, 0, size, buf) &&
        VerQueryValueW(buf, L"\\", (void **)&fi, &fl) && fi)
        snprintf(outv, len, "%u.%u.%u.%u", HIWORD(fi->dwFileVersionMS), LOWORD(fi->dwFileVersionMS),
                 HIWORD(fi->dwFileVersionLS), LOWORD(fi->dwFileVersionLS));
    free(buf);
}

static void report_file(const char *what, const wchar_t *path)
{
    char v[64];
    if (GetFileAttributesW(path) == INVALID_FILE_ATTRIBUTES) {
        out("       %-22s %ls (not there)", what, path);
        return;
    }
    file_version(path, v, sizeof(v));
    out("       %-22s %ls (%s)", what, path, v);
}

static void report_reg(const wchar_t *key, const wchar_t *value)
{
    wchar_t buf[MAX_PATH] = L"";
    DWORD size = sizeof(buf), type = 0, dw = 0;
    HKEY k;
    if (RegOpenKeyExW(HKEY_LOCAL_MACHINE, key, 0, KEY_READ, &k) != ERROR_SUCCESS) {
        out("       HKLM\\%ls (no key)", key);
        return;
    }
    LONG r = RegQueryValueExW(k, value, NULL, &type, (BYTE *)buf, &size);
    if (r != ERROR_SUCCESS)
        out("       HKLM\\%ls  %ls (not set)", key, value);
    else if (type == REG_DWORD) {
        memcpy(&dw, buf, sizeof(dw));
        out("       HKLM\\%ls  %ls = %lu", key, value, (unsigned long)dw);
    } else if (type == REG_SZ || type == REG_EXPAND_SZ) {
        buf[MAX_PATH - 1] = 0;
        out("       HKLM\\%ls  %ls = %ls", key, value, buf);
    } else
        out("       HKLM\\%ls  %ls (type %lu)", key, value, (unsigned long)type);
    RegCloseKey(k);
}

static void report_environment()
{
    wchar_t p[MAX_PATH];
    out("-- environment");
    swprintf(p, MAX_PATH, L"%ls\\nvngx_dlss.dll", g_opt.exedir);
    report_file("DLSS (next to exe)", p);
    swprintf(p, MAX_PATH, L"%ls\\nvapi64.dll", g_opt.exedir);
    report_file("nvapi64 (next to exe)", p);
    GetSystemDirectoryW(p, MAX_PATH);
    wcscat(p, L"\\nvapi64.dll");
    report_file("nvapi64 (System32)", p);
    swprintf(p, MAX_PATH, L"%ls\\_nvngx.dll", g_opt.exedir);
    report_file("_nvngx (next to exe)", p);
    report_reg(L"SOFTWARE\\NVIDIA Corporation\\Global\\NGXCore", L"FullPath");
    report_reg(L"SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render\\NGXCore", L"NGXPath");
    report_reg(L"SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render\\Parameters\\NGXCore", L"NGXPath");
    report_reg(L"SOFTWARE\\Helios", L"Dlss");
    const char *e = getenv("NVK_DLSS");
    out("       NVK_DLSS = %s", e ? e : "(unset)");
    e = getenv("HELIOS_ICD");
    out("       HELIOS_ICD = %s", e ? e : "(unset)");
    e = getenv("VK_DIRECT_DRIVER");
    out("       VK_DIRECT_DRIVER = %s", e ? e : "(unset)");
}

void report_loaded_modules()
{
    const wchar_t *mods[] = {L"_nvngx.dll", L"nvngx.dll", L"nvngx_dlss.dll", L"nvapi64.dll",
                             L"vulkan-1.dll", L"vulkan_nouveau.dll", L"d3d12.dll"};
    out("-- modules loaded now");
    for (const wchar_t *m : mods) {
        HMODULE h = GetModuleHandleW(m);
        if (!h) {
            out("       %-20ls (not loaded)", m);
            continue;
        }
        wchar_t path[MAX_PATH];
        char v[64];
        GetModuleFileNameW(h, path, MAX_PATH);
        file_version(path, v, sizeof(v));
        out("       %-20ls %ls (%s)", m, path, v);
    }
}

/* ---- arguments ---- */

static void usage()
{
    out("usage: ngx-smoketest.exe [--api vk|d3d12] [--frames N] [--out WxH] [--in WxH]\n"
        "                         [--quality perf|balanced|quality|ultraperf|ultraquality|dlaa]\n"
        "                         [--preset default|J|K|L|M] [--logdir DIR] [--appid N | --project-id GUID]\n"
        "                         [--timeout-ms N] [--watchdog-s N] [--device N] [-v]\n"
        "defaults: --api vk --frames 100 --out 1920x1080 --quality quality (render size from NGX)\n"
        "          --logdir .\\ngxlog --timeout-ms 10000 --watchdog-s 300; -v echoes every NGX log line");
}

static bool parse_wxh(const char *s, unsigned *w, unsigned *h)
{
    char *end;
    unsigned long a = strtoul(s, &end, 10);
    if (*end != 'x' && *end != 'X')
        return false;
    unsigned long b = strtoul(end + 1, &end, 10);
    if (*end || !a || !b || a > 16384 || b > 16384)
        return false;
    *w = (unsigned)a;
    *h = (unsigned)b;
    return true;
}

static bool parse_args(int argc, char **argv)
{
    Options &o = g_opt;
    o.api = "vk";
    o.frames = 100;
    o.out_w = 1920;
    o.out_h = 1080;
    o.quality = NVSDK_NGX_PerfQuality_Value_MaxQuality;
    o.quality_name = "quality";
    o.project_id = "5f0e6a3c-2b8d-4c71-9e44-6d3a1b7c9f20";
    o.timeout_ms = 10000;
    o.watchdog_s = 300;
    o.device = -1;
    const char *logdir = ".\\ngxlog";
    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        const char *v = i + 1 < argc ? argv[i + 1] : NULL;
        if (!strcmp(a, "-v")) { o.verbose = true; continue; }
        if (!strcmp(a, "-h") || !strcmp(a, "--help")) return false;
        if (!v) { out("missing value for %s", a); return false; }
        i++;
        if (!strcmp(a, "--api")) {
            if (strcmp(v, "vk") && strcmp(v, "d3d12")) return false;
            o.api = v;
        } else if (!strcmp(a, "--frames")) {
            o.frames = atoi(v);
            if (o.frames < 1 || o.frames > 100000) return false;
        } else if (!strcmp(a, "--out")) {
            if (!parse_wxh(v, &o.out_w, &o.out_h)) return false;
        } else if (!strcmp(a, "--in")) {
            if (!parse_wxh(v, &o.in_w, &o.in_h)) return false;
        } else if (!strcmp(a, "--quality")) {
            static const struct { const char *n; NVSDK_NGX_PerfQuality_Value v; } q[] = {
                {"perf", NVSDK_NGX_PerfQuality_Value_MaxPerf},
                {"balanced", NVSDK_NGX_PerfQuality_Value_Balanced},
                {"quality", NVSDK_NGX_PerfQuality_Value_MaxQuality},
                {"ultraperf", NVSDK_NGX_PerfQuality_Value_UltraPerformance},
                {"ultraquality", NVSDK_NGX_PerfQuality_Value_UltraQuality},
                {"dlaa", NVSDK_NGX_PerfQuality_Value_DLAA}};
            bool found = false;
            for (auto &e : q)
                if (!strcmp(v, e.n)) { o.quality = e.v; o.quality_name = e.n; found = true; }
            if (!found) return false;
        } else if (!strcmp(a, "--preset")) {
            if (!strcmp(v, "default")) o.preset = 0;
            else if (strlen(v) == 1 && v[0] >= 'A' && v[0] <= 'O') o.preset = (unsigned)(v[0] - 'A' + 1);
            else return false;
        } else if (!strcmp(a, "--logdir")) {
            logdir = v;
        } else if (!strcmp(a, "--appid")) {
            o.use_app_id = true;
            o.app_id = strtoull(v, NULL, 0);
        } else if (!strcmp(a, "--project-id")) {
            o.project_id = v;
        } else if (!strcmp(a, "--timeout-ms")) {
            o.timeout_ms = (DWORD)strtoul(v, NULL, 10);
            if (!o.timeout_ms) return false;
        } else if (!strcmp(a, "--watchdog-s")) {
            o.watchdog_s = (DWORD)strtoul(v, NULL, 10);
            if (!o.watchdog_s) return false;
        } else if (!strcmp(a, "--device")) {
            o.device = atoi(v);
        } else {
            out("unknown option %s", a);
            return false;
        }
    }
    wchar_t wl[MAX_PATH];
    MultiByteToWideChar(CP_ACP, 0, logdir, -1, wl, MAX_PATH);
    if (!GetFullPathNameW(wl, MAX_PATH, o.logdir, NULL))
        wcscpy(o.logdir, wl);
    size_t len = wcslen(o.logdir);
    while (len > 3 && (o.logdir[len - 1] == L'\\' || o.logdir[len - 1] == L'/'))
        o.logdir[--len] = 0;
    GetModuleFileNameW(NULL, o.exedir, MAX_PATH);
    wchar_t *slash = wcsrchr(o.exedir, L'\\');
    if (slash)
        *slash = 0;
    return true;
}

int main(int argc, char **argv)
{
    InitializeCriticalSection(&out_lock);
    setvbuf(stdout, NULL, _IONBF, 0);
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    SetUnhandledExceptionFilter(crash_filter);

    if (!parse_args(argc, argv)) {
        usage();
        return 2;
    }
    done_event = CreateEventW(NULL, TRUE, FALSE, NULL);
    CreateThread(NULL, 0, watchdog, NULL, 0, NULL);

    out("ngx-smoketest: api %s, %d frames, output %ux%u, %s, preset %u, logdir %ls", g_opt.api,
        g_opt.frames, g_opt.out_w, g_opt.out_h, g_opt.quality_name, g_opt.preset, g_opt.logdir);
    if (g_opt.use_app_id)
        out("       NGX identity: application id %llu", g_opt.app_id);
    else
        out("       NGX identity: project id %s (engine CUSTOM 1.0)", g_opt.project_id);

    step("logdir");
    int r = SHCreateDirectoryExW(NULL, g_opt.logdir, NULL);
    if (r != ERROR_SUCCESS && r != ERROR_ALREADY_EXISTS && r != ERROR_FILE_EXISTS) {
        fail("cannot create %ls (error %d)", g_opt.logdir, r);
    } else {
        wchar_t p[MAX_PATH];
        log_path(p, L"ngx-callback-", L".log");
        ngx_log_file = CreateFileW(p, GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_ALWAYS,
                                   FILE_ATTRIBUTE_NORMAL, NULL);
        ok("%ls (NGX log callback -> %ls)", g_opt.logdir, p);
    }
    report_environment();

    int rc = !strcmp(g_opt.api, "vk") ? run_vulkan() : run_d3d12();
    (void)rc;
    step("end");
    SetEvent(done_event);
    if (ngx_log_file != INVALID_HANDLE_VALUE) {
        HANDLE f = ngx_log_file;
        ngx_log_file = INVALID_HANDLE_VALUE;
        CloseHandle(f);
    }
    int code = 0;
    if (n_failures) {
        out("RESULT: FAIL (%d step(s) failed, first: '%s'; %d warning(s))", n_failures,
            first_failed_step, n_warnings);
        code = 1;
    } else
        out("RESULT: PASS (%d warning(s))", n_warnings);
    // NGX and the API objects are shut down already. Ending the process here
    // keeps DLL unload code (drivers, runtimes) from hanging the exit.
    fflush(stdout);
    TerminateProcess(GetCurrentProcess(), (UINT)code);
    return code;
}

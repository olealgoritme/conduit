// ngx-smoketest: DLSS super resolution through NGX without a game.
// Shared declarations of main.cpp (options, output, NGX helpers, image
// checks) and the two API modes (vk_mode.cpp, d3d12_mode.cpp).
#pragma once

#define __USE_MINGW_ANSI_STDIO 1
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#include "nvsdk_ngx_defs.h"
#include "nvsdk_ngx_params.h"

struct Options {
    const char *api;            // "vk" or "d3d12"
    int frames;                 // evaluations
    unsigned in_w, in_h;        // render size; 0 = NGX's optimal size for the mode
    unsigned out_w, out_h;      // output size
    NVSDK_NGX_PerfQuality_Value quality;
    const char *quality_name;
    unsigned preset;            // NVSDK_NGX_DLSS_Hint_Render_Preset, 0 = default
    wchar_t logdir[MAX_PATH];   // NGX application data path, our logs, the .bmp files
    wchar_t exedir[MAX_PATH];   // where nvngx_dlss.dll is looked for (besides NGX's own search)
    bool use_app_id;
    unsigned long long app_id;
    const char *project_id;
    DWORD timeout_ms;           // every fence wait
    DWORD watchdog_s;           // the whole run
    int device;                 // physical device / adapter index, -1 = first usable
    bool verbose;               // echo every NGX log line
};

extern Options g_opt;

// Output: one line per call, thread-safe, flushed.
void out(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

// Steps: every step prints "[ok]" or "[FAIL]" with its name; the run fails
// (exit code 1) if any step failed.
void step(const char *name);                // current step (also for the watchdog)
void ok(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
void fail(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
void warn(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
int failures();
// A wait timed out or the GPU is gone: print and terminate at once (no
// cleanup, the GPU may still own the memory). Exit code 3.
[[noreturn]] void hang(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

// NGX
const char *ngx_result_name(NVSDK_NGX_Result r);
// Prints "<what>: 0x... NAME" as ok/fail; returns NVSDK_NGX_SUCCEED(r).
bool ngx_check(const char *what, NVSDK_NGX_Result r);
void ngx_common_info(NVSDK_NGX_FeatureCommonInfo *ci);
void ngx_discovery_info(NVSDK_NGX_FeatureDiscoveryInfo *di, const NVSDK_NGX_FeatureCommonInfo *ci);
void ngx_print_requirement(const NVSDK_NGX_FeatureRequirement *req);
// Prints the SuperSampling capability values; true if Available = 1.
bool ngx_print_caps(NVSDK_NGX_Parameter *caps);
// Picks the render size (NGX's optimal settings unless --in) and prints it.
void ngx_render_size(NVSDK_NGX_Parameter *caps, unsigned *w, unsigned *h);
// Create parameters shared by both modes (preset hints, flags).
void ngx_set_create_hints(NVSDK_NGX_Parameter *params);
int ngx_create_flags();
void jitter(int frame, float *x, float *y);

// Inputs: RGBA8 color, R32F depth, RG16F motion vectors (all zero).
void make_color(uint8_t *rgba, unsigned w, unsigned h, size_t pitch);
void make_depth(float *d, unsigned w, unsigned h, size_t pitch_bytes);
static const uint8_t kClear[4] = {255, 0, 255, 255}; // output before DLSS writes it

// Output check: checksum, sample pixels, comparison with the input; writes
// <logdir>\ngx-output-<api>.bmp and ngx-input-<api>.bmp. False if the output
// is all zero, one constant value or untouched.
bool check_output(const uint8_t *rgba, unsigned w, unsigned h, size_t pitch,
                  const uint8_t *in_rgba, unsigned in_w, unsigned in_h, size_t in_pitch);

// Timing
double now_ms();
struct Timings {
    double *gpu, *cpu_eval, *cpu_frame; // per frame, ms (gpu < 0: no value)
    int n;
};
void timings_alloc(Timings *t, int n);
void timings_print(const Timings *t);

void report_loaded_modules();
int run_vulkan();
int run_d3d12();

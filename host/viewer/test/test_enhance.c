/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_enhance.c -- picture-enhancement settings and arithmetic: the names
 * and the settings file (nb_view.c), which pass runs for a scale and the
 * sharpness mapping (nb_enhance.h), NIS's constants against values worked
 * out from NIS_Config.h by hand.
 */
#include <math.h>
#include <stdio.h>
#include <string.h>

#include "nb_enhance.h"
#include "nb_view.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

static bool near(float a, float b)
{
    return fabsf(a - b) < 1e-5f;
}

static void test_names(void)
{
    int v;

    for (int i = 0; i < NB_UPSCALE_COUNT; i++) {
        CHECK(nb_upscale_parse(nb_upscale_name(i), &v) == 0 && v == i,
              "upscale %d round trip", i);
        CHECK(nb_upscale_label(i)[0], "upscale %d has a label", i);
    }
    for (int i = 0; i < NB_FGEN_COUNT; i++) {
        CHECK(nb_fgen_parse(nb_fgen_name(i), &v) == 0 && v == i,
              "framegen %d round trip", i);
    }
    CHECK(nb_upscale_parse("fsr1", &v) == 0 && v == NB_UPSCALE_FSR, "fsr1");
    CHECK(nb_upscale_parse("sharpen", &v) == 0 && v == NB_UPSCALE_CAS, "alias");
    CHECK(nb_upscale_parse("dlss", &v) == -1, "unknown upscaler");
    CHECK(nb_fgen_parse("smooth-motion", &v) == 0 && v == NB_FGEN_NVSM, "alias");
    CHECK(nb_fgen_parse("4x", &v) == -1, "unknown framegen");
    CHECK(nb_fgen_mult(NB_FGEN_OFF) == 1 && nb_fgen_mult(NB_FGEN_2X) == 2 &&
          nb_fgen_mult(NB_FGEN_3X) == 3 && nb_fgen_mult(NB_FGEN_NVSM) == 1,
          "multipliers");
    CHECK(nb_sharp_parse("0") == 0 && nb_sharp_parse("75") == 75 &&
          nb_sharp_parse("250") == 100, "sharpness values");
    CHECK(nb_sharp_parse("-1") == -1 && nb_sharp_parse("x") == -1 &&
          nb_sharp_parse("") == -1 && nb_sharp_parse("5x") == -1,
          "sharpness junk");
}

static void test_store(void)
{
    struct nb_vstore st, back;
    char buf[4096];

    nb_vstore_init(&st);
    CHECK(st.upscale == NB_UPSCALE_OFF && st.fgen == NB_FGEN_OFF &&
          st.sharp == NB_SHARP_DEFAULT, "off by default");
    nb_vstore_format(&st, buf, sizeof(buf));
    CHECK(!strstr(buf, "upscale=") && !strstr(buf, "framegen=") &&
          !strstr(buf, "sharpness="), "defaults are not written");
    st.upscale = NB_UPSCALE_NIS;
    st.sharp = 25;
    st.fgen = NB_FGEN_3X;
    nb_vstore_format(&st, buf, sizeof(buf));
    CHECK(strstr(buf, "upscale=nis\n") && strstr(buf, "sharpness=25\n") &&
          strstr(buf, "framegen=3x\n"), "written: %s", buf);
    nb_vstore_init(&back);
    nb_vstore_parse(&back, buf);
    CHECK(back.upscale == NB_UPSCALE_NIS && back.sharp == 25 &&
          back.fgen == NB_FGEN_3X, "read back");
    nb_vstore_init(&back);
    nb_vstore_parse(&back, "upscale=magic\nsharpness=lots\nframegen=9x\n"
                           "scale=stretch\n");
    CHECK(back.upscale == NB_UPSCALE_OFF && back.sharp == NB_SHARP_DEFAULT &&
          back.fgen == NB_FGEN_OFF && back.cur.scale == NB_SCALE_STRETCH,
          "junk ignored, the rest kept");
    nb_vstore_init(&back);
    nb_vstore_parse(&back, "framegen=nvidia\nupscale=fsr\n");
    CHECK(back.fgen == NB_FGEN_NVSM && back.upscale == NB_UPSCALE_FSR,
          "nvidia + fsr");
}

static void test_pick(void)
{
    const char *why;

    CHECK(nb_enh_pick(NB_UPSCALE_OFF, 1920, 1080, 3840, 2160, &why) ==
          NB_UPSCALE_OFF && !why, "off is off");
    CHECK(nb_enh_pick(NB_UPSCALE_FSR, 1920, 1080, 3840, 2160, &why) ==
          NB_UPSCALE_FSR && !why, "FSR 2x");
    CHECK(nb_enh_pick(NB_UPSCALE_FSR, 1280, 720, 5120, 1440, &why) ==
          NB_UPSCALE_FSR, "FSR beyond 2x still FSR");
    CHECK(nb_enh_pick(NB_UPSCALE_NIS, 1920, 1080, 2560, 1440, &why) ==
          NB_UPSCALE_NIS && !why, "NIS 1.33x");
    CHECK(nb_enh_pick(NB_UPSCALE_NIS, 1280, 720, 3840, 2160, &why) ==
          NB_UPSCALE_FSR && why, "NIS 3x -> FSR");
    CHECK(nb_enh_pick(NB_UPSCALE_FSR, 2560, 1440, 1920, 1080, &why) ==
          NB_UPSCALE_OFF && why, "drawn smaller: plain pass");
    CHECK(nb_enh_pick(NB_UPSCALE_NIS, 1920, 1080, 1920, 1080, &why) ==
          NB_UPSCALE_NIS, "1:1 NIS (sharpening)");
    CHECK(nb_enh_pick(NB_UPSCALE_CAS, 2560, 1440, 1920, 1080, &why) ==
          NB_UPSCALE_CAS && !why, "CAS runs when drawn smaller too");
    CHECK(nb_enh_pick(NB_UPSCALE_FSR, 0, 1080, 1920, 1080, &why) ==
          NB_UPSCALE_OFF, "empty source");
    CHECK(nb_enh_pick(99, 1920, 1080, 3840, 2160, NULL) == NB_UPSCALE_OFF,
          "unknown choice");
}

static void test_sharp(void)
{
    CHECK(!nb_rcas_on(0) && nb_rcas_on(1), "RCAS only above 0");
    CHECK(near(nb_rcas_stops(100), 0.0f), "100: sharpest RCAS");
    CHECK(near(nb_rcas_stops(50), 1.0f), "50: one stop");
    CHECK(near(nb_rcas_stops(0), 2.0f), "0: two stops");
    CHECK(near(nb_rcas_stops(500), 0.0f) && near(nb_rcas_stops(-5), 2.0f),
          "clamped");
    CHECK(near(nb_sharp_unit(0), 0.0f) && near(nb_sharp_unit(50), 0.5f) &&
          near(nb_sharp_unit(100), 1.0f) && near(nb_sharp_unit(300), 1.0f),
          "unit sharpness");
}

static void test_nis(void)
{
    struct nb_nis_cfg c;

    CHECK(nb_nis_config(&c, 0.5f, 1920, 1080, 2560, 1440), "1.33x in range");
    /* At sharpness 0.5 the slider term is 0 (NIS_Config.h). */
    CHECK(near(c.kSharpStrengthMin, 0.4f) && near(c.kSharpLimitMin, 0.14f),
          "neutral minimums (%f %f)", c.kSharpStrengthMin, c.kSharpLimitMin);
    CHECK(near(c.kSharpStrengthScale, 1.2f) && near(c.kSharpLimitScale, 0.36f),
          "neutral scales (%f %f)", c.kSharpStrengthScale, c.kSharpLimitScale);
    CHECK(near(c.kDetectRatio, 2 * 1127.f / 1024.f) &&
          near(c.kDetectThres, 64.f / 1024.f) && near(c.kRatioNorm, 0.125f) &&
          near(c.kSharpScaleY, 1.0f / 0.45f) && near(c.kEps, 1.0f / 255.0f),
          "SDR detection constants");
    CHECK(near(c.kScaleX, 0.75f) && near(c.kScaleY, 0.75f) &&
          near(c.kSrcNormX, 1.0f / 1920) && near(c.kDstNormY, 1.0f / 1440),
          "scale and normalisation");
    CHECK(c.kOutputViewportWidth == 2560 && c.kInputViewportHeight == 1080,
          "viewports");
    nb_nis_config(&c, 1.0f, 1920, 1080, 2560, 1440);
    /* s = 0.5: min = 0.4 + 0.5 * 1.25 * 1.2 = 1.15; max = 1.6 + 0.5 * 1.25
     * * 1.8 = 2.725 */
    CHECK(near(c.kSharpStrengthMin, 1.15f) &&
          near(c.kSharpStrengthScale, 2.725f - 1.15f), "full sharpness");
    nb_nis_config(&c, 0.0f, 1920, 1080, 2560, 1440);
    /* s = -0.5: min = max(0, 0.4 - 0.5 * 1.0 * 1.2) = 0; limit min =
     * max(0.1, 0.14 - 0.16) = 0.1 */
    CHECK(near(c.kSharpStrengthMin, 0.0f) && near(c.kSharpLimitMin, 0.1f),
          "no sharpening");
    CHECK(!nb_nis_config(&c, 0.5f, 1280, 720, 3840, 2160), "3x out of range");
    CHECK(!nb_nis_config(&c, 0.5f, 2560, 1440, 1920, 1080), "downscale out");
    CHECK(!nb_nis_config(&c, 0.5f, 0, 1440, 1920, 1080), "empty");
}

int main(void)
{
    test_names();
    test_store();
    test_pick();
    test_sharp();
    test_nis();
    printf("test_enhance: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}

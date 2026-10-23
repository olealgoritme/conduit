/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_vkpresent.c -- when NVIDIA Smooth Motion is offered (nb_vkpresent.h):
 * the GPU generation, the driver's present layer among the loader's layer
 * manifests, render nodes in a fake sysfs, and the present-mode choice.
 * No GPU, no Vulkan.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "nb_vkpresent.h"

static int checks, fails;
#define CHECK(c, ...) do { checks++; if (!(c)) { fails++; \
        printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); \
        printf("\n"); } } while (0)

static void put(const char *path, const char *text)
{
    FILE *f = fopen(path, "w");

    if (f) {
        fputs(text, f);
        fclose(f);
    }
}

static void mkdirs(const char *path)
{
    char p[512];

    snprintf(p, sizeof(p), "%s", path);
    for (char *s = p + 1; *s; s++) {
        if (*s == '/') {
            *s = '\0';
            mkdir(p, 0755);
            *s = '/';
        }
    }
    mkdir(p, 0755);
}

static void test_gpu(void)
{
    CHECK(nb_vkp_gpu_ok(0x10de, 0x2684), "RTX 4090 (AD102)");
    CHECK(nb_vkp_gpu_ok(0x10de, 0x2b85), "RTX 5090 (GB202)");
    CHECK(nb_vkp_gpu_ok(0x10de, 0x2882), "RTX 4060 (AD107)");
    CHECK(!nb_vkp_gpu_ok(0x10de, 0x2204), "RTX 3090 (GA102): no");
    CHECK(!nb_vkp_gpu_ok(0x10de, 0x1e84), "RTX 2070 Super (TU104): no");
    CHECK(!nb_vkp_gpu_ok(0x1002, 0x744c), "AMD: no");
    CHECK(!nb_vkp_gpu_ok(0x8086, 0x56a0), "Intel: no");
}

static void test_scan(void)
{
    char root[] = "/tmp/nb-vkp-XXXXXX";
    char p[512];

    if (!mkdtemp(root)) {
        CHECK(0, "mkdtemp: %s", strerror(errno));
        return;
    }
    /* sysfs: an AMD iGPU and an RTX 3080 -- not enough */
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD128/device", root);
    mkdirs(p);
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD128/device/vendor", root);
    put(p, "0x1002\n");
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD128/device/device", root);
    put(p, "0x164e\n");
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD129/device", root);
    mkdirs(p);
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD129/device/vendor", root);
    put(p, "0x10de\n");
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD129/device/device", root);
    put(p, "0x2206\n");
    snprintf(p, sizeof(p), "%s/sys/class/drm/card1", root);
    mkdirs(p);
    CHECK(!nb_vkp_gpu_present(root), "no Ada or later present");
    /* ... then an RTX 4080 */
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD130/device", root);
    mkdirs(p);
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD130/device/vendor", root);
    put(p, "0x10de\n");
    snprintf(p, sizeof(p), "%s/sys/class/drm/renderD130/device/device", root);
    put(p, "0x2704\n");
    CHECK(nb_vkp_gpu_present(root), "RTX 4080 found");

    /* layer manifests */
    unsetenv("XDG_DATA_DIRS");
    unsetenv("XDG_DATA_HOME");
    snprintf(p, sizeof(p), "%s/usr/share/vulkan/implicit_layer.d", root);
    mkdirs(p);
    snprintf(p, sizeof(p), "%s/usr/share/vulkan/implicit_layer.d/other.json",
             root);
    put(p, "{ \"layer\": { \"name\": \"VK_LAYER_MESA_device_select\" } }\n");
    CHECK(!nb_vkp_layer_present(root), "another layer is not it");
    snprintf(p, sizeof(p), "%s/usr/share/vulkan/implicit_layer.d/nv.txt",
             root);
    put(p, "\"VK_LAYER_NV_present\"\n");
    CHECK(!nb_vkp_layer_present(root), "only *.json manifests count");
    snprintf(p, sizeof(p), "%s/opt/share/vulkan/implicit_layer.d", root);
    mkdirs(p);
    snprintf(p, sizeof(p),
             "%s/opt/share/vulkan/implicit_layer.d/nvidia_layers.json", root);
    put(p, "{ \"layers\": [{ \"name\": \"VK_LAYER_NV_optimus\" },"
           "{ \"name\": \"VK_LAYER_NV_present\", \"enable_environment\": "
           "{ \"NVPRESENT_ENABLE_SMOOTH_MOTION\": \"1\" } }] }\n");
    CHECK(!nb_vkp_layer_present(root), "not in the searched directories yet");
    setenv("XDG_DATA_DIRS", "/nowhere:/opt/share", 1);
    CHECK(nb_vkp_layer_present(root), "found through XDG_DATA_DIRS");
    unsetenv("XDG_DATA_DIRS");
    {
        char cmd[600];

        snprintf(cmd, sizeof(cmd), "rm -rf '%s'", root);
        if (system(cmd) != 0) {
            printf("note: could not remove %s\n", root);
        }
    }
}

static void test_modes(void)
{
    int all[3] = { NB_VKP_FIFO, NB_VKP_MAILBOX, NB_VKP_IMMEDIATE };
    int fifo_mb[2] = { NB_VKP_FIFO, NB_VKP_MAILBOX };
    int fifo[1] = { NB_VKP_FIFO };

    CHECK(nb_vkp_pick_mode(all, 3, NB_VKP_IMMEDIATE) == NB_VKP_IMMEDIATE,
          "immediate when offered");
    CHECK(nb_vkp_pick_mode(fifo_mb, 2, NB_VKP_IMMEDIATE) == NB_VKP_MAILBOX,
          "immediate falls back to mailbox");
    CHECK(nb_vkp_pick_mode(fifo, 1, NB_VKP_IMMEDIATE) == NB_VKP_FIFO,
          "... and to FIFO");
    CHECK(nb_vkp_pick_mode(all, 3, NB_VKP_MAILBOX) == NB_VKP_MAILBOX,
          "mailbox");
    CHECK(nb_vkp_pick_mode(all, 3, NB_VKP_FIFO) == NB_VKP_FIFO, "vsync: FIFO");
    CHECK(nb_vkp_pick_mode(NULL, 0, NB_VKP_IMMEDIATE) == NB_VKP_FIFO,
          "nothing listed: FIFO, which is always there");
}

int main(void)
{
    test_gpu();
    test_scan();
    test_modes();
    printf("test_vkpresent: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}

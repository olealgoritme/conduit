/* SPDX-License-Identifier: MIT */
/*
 * Unit tests for src/win_wire.h, the wire format the Windows transport speaks.
 * Pure C, no OS calls: runs anywhere. The expected layouts are the ones in
 * host/backend/protocol/src/messages.rs (MsgHeader, OpenReq, IoctlReq,
 * IoctlResp) and the sections guest/linux/conduit_gpu.c reads from GetSysFiles.
 * Also the loss rules (helios_nvrm_escape.h) and the per-generation loss state
 * (src/win_gen.h) the transport latches a lost transport and reopens by.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "helios_nvrm_escape.h"
#include "win_gen.h"
#include "win_wire.h"

static int failures;

#define CHECK(cond)                                                                \
    do {                                                                           \
        if (!(cond)) {                                                             \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);        \
            failures++;                                                            \
        }                                                                          \
    } while (0)

static void test_cmd_encoding(void)
{
    /* _IOWR('F', 0x2A, 32): dir=3 | size=32<<16 | 'F'<<8 | 0x2A */
    CHECK(crm_wire_cmd(0x2A, 32) == 0xC020462Au);
    /* NV_ESC_RM_ALLOC with the 48-byte NVOS64 */
    CHECK(crm_wire_cmd(0x2B, 48) == 0xC030462Bu);
    /* the size field is 14 bits; larger sizes are masked, not spilled into dir */
    CHECK((crm_wire_cmd(0x2A, 0x14000) >> 30) == 3u);
}

static void test_open_close(void)
{
    uint8_t b[32];
    memset(b, 0xAA, sizeof(b));
    CHECK(crm_wire_open(b, CRM_WIRE_DEV_CTL) == 24);
    CHECK(crm_get32(b + 0) == 1);   /* msg_type Open */
    CHECK(crm_get32(b + 4) == 0);   /* handle */
    CHECK(crm_get32(b + 8) == 0);   /* status */
    CHECK(crm_get32(b + 12) == 0);  /* padding */
    CHECK(crm_get32(b + 16) == 255);
    CHECK(crm_get32(b + 20) == 2);  /* O_RDWR */

    CHECK(crm_wire_open(b, 3) == 24);
    CHECK(crm_get32(b + 16) == 3);

    memset(b, 0xAA, sizeof(b));
    CHECK(crm_wire_close(b, 77) == 16);
    CHECK(crm_get32(b + 0) == 2);
    CHECK(crm_get32(b + 4) == 77);
    CHECK(crm_get32(b + 8) == 0);
    CHECK(crm_get32(b + 12) == 0);

    CHECK(crm_wire_get_sys_files(b) == 16);
    CHECK(crm_get32(b + 0) == 7);
    CHECK(crm_get32(b + 4) == 0);
}

static void test_ioctl_layout(void)
{
    uint8_t data[8] = { 1, 2, 3, 4, 5, 6, 7, 8 };
    uint8_t nested[5] = { 9, 10, 11, 12, 13 };
    CHECK(crm_wire_ioctl_req_size(8, 5) == 16 + 24 + 8 + 5);
    CHECK(crm_wire_ioctl_resp_max(8, 5) == 16 + 12 + 8 + 5);

    uint8_t out[64];
    memset(out, 0xEE, sizeof(out));
    const size_t n = crm_wire_ioctl(out, 42, 0xC020462Au, data, 8, nested, 5);
    CHECK(n == 53);
    CHECK(crm_get32(out + 0) == 3);          /* msg_type Ioctl */
    CHECK(crm_get32(out + 4) == 42);         /* handle */
    CHECK(crm_get32(out + 8) == 0);
    CHECK(crm_get32(out + 12) == 0);
    CHECK(crm_get32(out + 16) == 0xC020462Au); /* IoctlReq.cmd */
    CHECK(crm_get32(out + 20) == 8);         /* data_len */
    CHECK(crm_get32(out + 24) == 8);         /* nested_offset == data_len */
    CHECK(crm_get32(out + 28) == 5);         /* nested_len */
    CHECK(crm_get32(out + 32) == 0);         /* deep_ptr_offset */
    CHECK(crm_get32(out + 36) == 0);         /* deep_len */
    CHECK(memcmp(out + 40, data, 8) == 0);
    CHECK(memcmp(out + 48, nested, 5) == 0);

    /* no nested block: nested_offset is 0, like the guest module's flat path */
    memset(out, 0xEE, sizeof(out));
    CHECK(crm_wire_ioctl(out, 5, 0xC0104600u, data, 8, NULL, 0) == 48);
    CHECK(crm_get32(out + 24) == 0);
    CHECK(crm_get32(out + 28) == 0);

    /* data_len 0 (an ioctl with no payload) is fine */
    CHECK(crm_wire_ioctl(out, 5, 0x00004600u, NULL, 0, NULL, 0) == 40);
}

/* A reply as the host builds it: MsgHeader | IoctlResp | data | nested. */
static size_t make_reply(uint8_t *r, int32_t status, const void *data, uint32_t dl,
                         const void *nested, uint32_t nl)
{
    crm_wire_header(r, 3, 0);
    crm_put32(r + 8, (uint32_t)status);
    crm_put32(r + 16, dl);
    crm_put32(r + 20, nl);
    crm_put32(r + 24, 0);
    memcpy(r + 28, data, dl);
    memcpy(r + 28 + dl, nested, nl);
    return 28 + dl + nl;
}

static void test_reply_parse(void)
{
    uint8_t d[4] = { 1, 2, 3, 4 }, nn[3] = { 7, 8, 9 };
    uint8_t r[64];
    struct crm_wire_reply rep;

    size_t n = make_reply(r, 0, d, 4, nn, 3);
    CHECK(crm_wire_parse_ioctl_reply(r, n, 4, 3, &rep) == 0);
    CHECK(rep.status == 0 && rep.data_len == 4 && rep.nested_len == 3);
    CHECK(memcmp(rep.data, d, 4) == 0 && memcmp(rep.nested, nn, 3) == 0);

    /* a smaller destination clamps the copy-back */
    CHECK(crm_wire_parse_ioctl_reply(r, n, 2, 1, &rep) == 0);
    CHECK(rep.data_len == 2 && rep.nested_len == 1);

    /* negative errno status passes through (with the body still present) */
    n = make_reply(r, -22, d, 4, nn, 3);
    CHECK(crm_wire_parse_ioctl_reply(r, n, 4, 3, &rep) == 0);
    CHECK(rep.status == -22);

    /* header-only reply: the host refused; zero lengths, status kept */
    crm_wire_header(r, 3, 0);
    crm_put32(r + 8, (uint32_t)(int32_t)-9);
    CHECK(crm_wire_parse_ioctl_reply(r, 16, 4, 3, &rep) == 0);
    CHECK(rep.status == -9 && rep.data_len == 0 && rep.nested_len == 0);

    /* ...but a short reply that claims SUCCESS is malformed, not an empty answer */
    crm_wire_header(r, 3, 0);
    CHECK(crm_wire_parse_ioctl_reply(r, 16, 4, 3, &rep) == -1);
    CHECK(crm_wire_parse_ioctl_reply(r, 27, 4, 3, &rep) == -1);

    /* a reply shorter than a header, or claiming more than it holds, is rejected */
    CHECK(crm_wire_parse_ioctl_reply(r, 8, 4, 3, &rep) == -1);
    n = make_reply(r, 0, d, 4, nn, 3);
    CHECK(crm_wire_parse_ioctl_reply(r, n - 1, 4, 3, &rep) == -1);
    crm_put32(r + 16, 0xFFFFFFFFu); /* absurd data_len */
    CHECK(crm_wire_parse_ioctl_reply(r, n, 4, 3, &rep) == -1);
}

static size_t put_file(uint8_t *p, const char *path, const char *content)
{
    const uint32_t pl = (uint32_t)strlen(path), cl = (uint32_t)strlen(content);
    crm_put32(p, pl);
    crm_put32(p + 4, cl);
    memcpy(p + 8, path, pl);
    memcpy(p + 8 + pl, content, cl);
    return 8 + pl + cl;
}

static void test_alloc_sizes(void)
{
    uint8_t s[1024];
    size_t at = 0;
    memset(s, 0, sizeof(s));

    /* section 1: two files, then the (0,0) terminator */
    at += put_file(s + at, "bus/pci/devices/0000:01:00.0/config", "xyz");
    at += put_file(s + at, "module/nvidia/version", "1");
    crm_put32(s + at, 0);
    crm_put32(s + at + 4, 0);
    at += 8;

    /* section 2: one DRI record = 4 header words + 9 info words + name */
    crm_put32(s + at, 1);
    at += 4;
    crm_put32(s + at, 5); /* name_len */
    at += CRM_WIRE_DRI_RECORD_BYTES;
    memcpy(s + at, "card0", 5);
    at += 5;

    /* section 3: magic, count, pairs */
    crm_put32(s + at, CRM_WIRE_ALLOC_SIZE_MAGIC);
    crm_put32(s + at + 4, 3);
    at += 8;
    const uint32_t want[3][2] = { { 0x0080, 32 }, { 0x2080, 16 }, { 0xc7b0, 28 } };
    for (int i = 0; i < 3; i++) {
        crm_put32(s + at, want[i][0]);
        crm_put32(s + at + 4, want[i][1]);
        at += 8;
    }

    uint32_t pairs[16];
    CHECK(crm_wire_parse_alloc_sizes(s, at, pairs, 8) == 3);
    for (int i = 0; i < 3; i++) {
        CHECK(pairs[2 * i] == want[i][0]);
        CHECK(pairs[2 * i + 1] == want[i][1]);
    }
    /* the caller's capacity bounds what is written */
    memset(pairs, 0, sizeof(pairs));
    CHECK(crm_wire_parse_alloc_sizes(s, at, pairs, 2) == 2);
    CHECK(pairs[4] == 0 && pairs[5] == 0);

    /* an old backend leaves zeroes where section 3 would be: no answer */
    CHECK(crm_wire_parse_alloc_sizes(s, at - 8 * 3 - 8 + 4, pairs, 8) == 0);
    memset(s + at - 8 * 3 - 8, 0, 8 + 24);
    CHECK(crm_wire_parse_alloc_sizes(s, at, pairs, 8) == 0);

    /* truncation anywhere is a clean 0, never a read past the end */
    for (size_t cut = 0; cut < at; cut++) {
        uint32_t p2[8];
        (void)crm_wire_parse_alloc_sizes(s, cut, p2, 4);
    }
}

/* What a Windows program sends to present RM memory: Open of a DRM render node,
 * DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY with its NVKMS block, ScanoutFlip. */
static void test_scanout_path(void)
{
    uint8_t b[128];

    /* Open of render node 0: DeviceKind::Dri(0). */
    CHECK(crm_wire_open(b, CRM_WIRE_DEV_DRI_BASE + 0) == 24);
    CHECK(crm_get32(b + 16) == 512);

    /* DRM_IOWR(DRM_COMMAND_BASE + 0x01, 32-byte params): nvidia-drm's
     * GEM_IMPORT_NVKMS_MEMORY, which the backend matches as 'd' nr 0x41. */
    const uint32_t imp = crm_wire_ioc(3, 'd', 0x41, 32);
    CHECK(imp == 0xC0206441u);
    /* DRM_IOW(0x09, struct drm_gem_close) */
    CHECK(crm_wire_ioc(1, 'd', 0x09, 8) == 0x40086409u);
    /* the 'F' helper is the same encoding */
    CHECK(crm_wire_cmd(0x2A, 32) == crm_wire_ioc(3, 'F', 0x2A, 32));

    /* data = { mem_size, nvkms_params_ptr, nvkms_params_size, handle, pad },
     * nested = NvKmsKapiPrivImportMemoryParams (28 bytes, memFd first). The
     * backend reads the nested block at outer_size 32 and the memFd at 0. */
    uint8_t data[32] = { 0 }, nested[28] = { 0 };
    crm_put32(data + 16, 28);
    crm_put32(nested + 0, 9);  /* memFd: a backend handle */
    crm_put32(nested + 4, 1);  /* layout PITCH */
    memset(b, 0xEE, sizeof(b));
    CHECK(crm_wire_ioctl(b, 5, imp, data, 32, nested, 28) == 16 + 24 + 32 + 28);
    CHECK(crm_get32(b + 4) == 5);
    CHECK(crm_get32(b + 16) == imp);
    CHECK(crm_get32(b + 20) == 32);
    CHECK(crm_get32(b + 24) == 32);  /* nested_offset */
    CHECK(crm_get32(b + 28) == 28);
    CHECK(crm_get32(b + 40 + 16) == 28);
    CHECK(crm_get32(b + 40 + 32) == 9);
    CHECK(crm_get32(b + 40 + 36) == 1);

    /* ScanoutFlip */
    struct crm_wire_flip f = {
        .scanout = 0, .owner_handle = 7, .host_handle = 3, .width = 1920,
        .height = 1080, .stride = 7680, .offset = 0, .fourcc = 0x34325258u,
        .modifier = 0x0300000000606015ull, .seq = 0x100000002ull,
    };
    memset(b, 0xEE, sizeof(b));
    CHECK(crm_wire_scanout_flip(b, &f) == 80);
    CHECK(crm_get32(b + 0) == 20);   /* msg_type ScanoutFlip */
    CHECK(crm_get32(b + 4) == 0);    /* handle */
    CHECK(crm_get32(b + 8) == 0);
    CHECK(crm_get32(b + 12) == 0);
    CHECK(crm_get32(b + 16) == 0);   /* scanout */
    CHECK(crm_get32(b + 20) == 7);   /* owner_handle */
    CHECK(crm_get32(b + 24) == 3);   /* host_handle */
    CHECK(crm_get32(b + 28) == 1920);
    CHECK(crm_get32(b + 32) == 1080);
    CHECK(crm_get32(b + 36) == 7680);
    CHECK(crm_get32(b + 40) == 0);
    CHECK(crm_get32(b + 44) == 0x34325258u);
    CHECK(crm_get32(b + 48) == 0x00606015u); /* modifier, low word first */
    CHECK(crm_get32(b + 52) == 0x03000000u);
    CHECK(crm_get32(b + 56) == 2);           /* seq */
    CHECK(crm_get32(b + 60) == 1);
    for (int i = 64; i < 80; i++)
        CHECK(b[i] == 0);                    /* reserved */
    CHECK(b[80] == 0xEE);                    /* nothing past the message */
}

/* The rules the Windows transport judges every reply by (helios_nvrm_escape.h;
 * the KMD side is nvrm_events.rs / escape.rs). */
static void test_transport_loss_rules(void)
{
    HeliosNvrmHeader h;
    memset(&h, 0, sizeof(h));
    const uint64_t init_epoch = (1ull << 32) * 3 + 1; /* a stride-separated generation */

    /* An ordinary reply of the same generation is not a loss, whatever the KMD
     * verdict is (a refusal is not the transport going away). */
    h.epoch = init_epoch;
    h.status = HELIOS_NVRM_ST_OK;
    CHECK(!helios_nvrm_reply_is_lost(init_epoch, &h));
    h.status = HELIOS_NVRM_ST_NOT_OWNED;
    CHECK(!helios_nvrm_reply_is_lost(init_epoch, &h));
    h.status = HELIOS_NVRM_ST_DEVICE_ERROR;
    CHECK(!helios_nvrm_reply_is_lost(init_epoch, &h));
    h.status = HELIOS_NVRM_ST_TIMEOUT;
    CHECK(!helios_nvrm_reply_is_lost(init_epoch, &h));

    /* TRANSPORT_RESET is a loss even with an unchanged epoch (a failed transport
     * that was not replaced). */
    h.status = HELIOS_NVRM_ST_TRANSPORT_RESET;
    CHECK(helios_nvrm_reply_is_lost(init_epoch, &h));

    /* A changed epoch is a loss with an OK status (a new generation answered)... */
    h.status = HELIOS_NVRM_ST_OK;
    h.epoch = init_epoch + (1ull << 32);
    CHECK(helios_nvrm_reply_is_lost(init_epoch, &h));
    /* ...and so is epoch 0, which is "no transport at all". */
    h.epoch = 0;
    CHECK(helios_nvrm_reply_is_lost(init_epoch, &h));
    h.status = HELIOS_NVRM_ST_NOT_OWNED;
    CHECK(helios_nvrm_reply_is_lost(init_epoch, &h));

    /* NTSTATUS: only "not ready" and "removed" mean the device is gone. */
    CHECK(helios_nvrm_ntstatus_is_lost((int32_t)0xC00000A3u)); /* STATUS_DEVICE_NOT_READY */
    CHECK(helios_nvrm_ntstatus_is_lost((int32_t)0xC00002B6u)); /* STATUS_DEVICE_REMOVED */
    CHECK(!helios_nvrm_ntstatus_is_lost(0));
    CHECK(!helios_nvrm_ntstatus_is_lost((int32_t)0xC0000002u)); /* NOT_IMPLEMENTED: old KMD */
    CHECK(!helios_nvrm_ntstatus_is_lost((int32_t)0xC000000Du)); /* INVALID_PARAMETER */
    CHECK(!helios_nvrm_ntstatus_is_lost((int32_t)0xC00000BBu)); /* NOT_SUPPORTED */

    /* The status/ABI numbers the transport relies on. */
    CHECK(HELIOS_NVRM_ST_TRANSPORT_RESET == 4);
    CHECK(HELIOS_NVRM_EVENT_TRANSPORT_LOST == 2u);
    CHECK((HELIOS_NVRM_EVENT_KINDS_ALL & (1u << HELIOS_NVRM_EVENT_TRANSPORT_LOST)) != 0);
}

/* One loss state per generation (src/win_gen.h): a loss ends the generation,
 * the next open starts a new one that is not lost, and a reply of the old
 * transport is still a loss for the new one. `table` stands for the process's
 * loss table epoch (helios_kmdmap.h), which moves once per observed loss. */
static void test_generation_loss_restart(void)
{
    struct crm_win_gen g;
    memset(&g, 0, sizeof(g));
    int32_t table = 7;
    HeliosNvrmHeader h;
    helios_nvrm_init(&h, HELIOS_NVRM_OP_QUERY_CAPS, sizeof(h));

    /* Nothing is lost or judged before the first init. */
    CHECK(!crm_win_gen_lost(&g, table));
    h.epoch = 123;
    CHECK(!crm_win_gen_reply_lost(&g, &h, sizeof(h)));

    /* Init refuses "no transport" (epoch 0) and stays uninitialised... */
    CHECK(crm_win_gen_accept(&g, 0, table) != 0);
    CHECK(g.generation == 0);
    /* ...and accepts a live one: generation 1, attached at the table's epoch. */
    const uint64_t e1 = 0x00000001a5a50001ull; /* image salt in the high word */
    CHECK(crm_win_gen_accept(&g, e1, table) == 0);
    CHECK(g.generation == 1 && g.loss_epoch0 == 7 && g.init_epoch == e1);
    CHECK(!crm_win_gen_lost(&g, table));
    h.epoch = e1;
    h.status = HELIOS_NVRM_ST_OK;
    CHECK(!crm_win_gen_reply_lost(&g, &h, sizeof(h)));

    /* A driver image reload: with the KMD's per-image salt the new transport's
     * epoch differs even when the transport counter restarted at the same
     * value, so the first reply from it ends generation 1. */
    const uint64_t e2 = 0x000000015a5a0001ull;
    h.epoch = e2;
    CHECK(crm_win_gen_reply_lost(&g, &h, sizeof(h)));
    table++; /* helios_kmdmap_mark_lost moves the table */
    CHECK(crm_win_gen_lost(&g, table));

    /* The next open: a new generation, attached at the moved epoch, not lost. */
    crm_win_gen_restart(&g, table);
    CHECK(g.generation == 2 && g.loss_epoch0 == 8 && g.init_epoch == 0);
    CHECK(!crm_win_gen_lost(&g, table));
    CHECK(crm_win_gen_accept(&g, e2, table) == 0);
    CHECK(g.generation == 2 && g.loss_epoch0 == 8 && g.init_epoch == e2);
    CHECK(!crm_win_gen_reply_lost(&g, &h, sizeof(h))); /* the new transport answers */
    CHECK(!crm_win_gen_lost(&g, table));

    /* A reply of the old transport (a straggler) is a loss for generation 2
     * too: nothing of generation 1 may be taken as an answer now. */
    h.epoch = e1;
    CHECK(crm_win_gen_reply_lost(&g, &h, sizeof(h)));

    /* Only NVRM replies are judged. The other Helios escapes on the same path
     * (a Venus holder context: 16-byte header + two u32, 24 bytes; blob release,
     * 32 bytes) have no status or epoch where an NVRM header has them: whatever
     * lies past their end must never be read as a loss (338.1 did, on the first
     * Venus context of every NVK process). */
    struct {
        HeliosEscapeHeader hdr;
        uint32_t capset_id, out_ctx_id;
        uint8_t past_end[16]; /* what 338.1 read as status and epoch */
    } ctx;
    memset(&ctx, 0xbe, sizeof(ctx));
    ctx.hdr.magic = HELIOS_ESCAPE_MAGIC;
    ctx.hdr.cmd_type = 0x0002u; /* CTX_CREATE */
    ctx.hdr.version = HELIOS_ESCAPE_VERSION;
    ctx.hdr.size = 24;
    CHECK(!crm_win_gen_judged(&ctx, 24));
    CHECK(!crm_win_gen_reply_lost(&g, &ctx, 24));
    CHECK(!crm_win_gen_judged(&ctx, sizeof(ctx))); /* long enough, not NVRM */
    CHECK(!crm_win_gen_reply_lost(&g, &ctx, sizeof(ctx)));
    /* An NVRM buffer shorter than its header is not judged either. */
    CHECK(!crm_win_gen_judged(&h, HELIOS_NVRM_HEADER_BYTES - 1));
    CHECK(crm_win_gen_judged(&h, sizeof(h)));

    /* A loss someone else saw (the UMD, a vanished view) moves the table: lost,
     * and the restart after it is clean again. */
    table++;
    CHECK(crm_win_gen_lost(&g, table));
    crm_win_gen_restart(&g, table);
    CHECK(g.generation == 3 && !crm_win_gen_lost(&g, table));
}

int main(void)
{
    test_transport_loss_rules();
    test_generation_loss_restart();
    test_cmd_encoding();
    test_open_close();
    test_ioctl_layout();
    test_reply_parse();
    test_alloc_sizes();
    test_scanout_path();
    if (failures) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("test_win_wire: all checks passed\n");
    return 0;
}

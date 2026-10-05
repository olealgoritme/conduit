/* SPDX-License-Identifier: MIT */
/*
 * Unit tests for src/win_wire.h, the wire format the Windows transport speaks.
 * Pure C, no OS calls: runs anywhere. The expected layouts are the ones in
 * host/backend/protocol/src/messages.rs (MsgHeader, OpenReq, IoctlReq,
 * IoctlResp) and the sections guest/linux/conduit_gpu.c reads from GetSysFiles.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

int main(void)
{
    test_cmd_encoding();
    test_open_close();
    test_ioctl_layout();
    test_reply_parse();
    test_alloc_sizes();
    if (failures) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("test_win_wire: all checks passed\n");
    return 0;
}

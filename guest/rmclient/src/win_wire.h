/* SPDX-License-Identifier: MIT */
/*
 * Wire format of Conduit's RM forwarding protocol (host/backend/protocol
 * messages.rs), as the Windows transport builds and reads it.
 *
 * The Linux guest module (guest/linux/conduit_gpu.c) turns an RM escape into
 * these messages inside the kernel; on Windows the KMD forwards them verbatim
 * (HELIOS_ESCAPE_NVRM, FORWARD) and this code builds them in user mode. There
 * is nothing OS-specific here, so it is exercised by tests/test_win_wire.c on
 * any host.
 *
 * Everything is little-endian, which every host this runs on is.
 *
 *   request  = MsgHeader(16) | payload
 *   MsgHeader = { u32 msg_type, u32 handle, i32 status, u32 padding }
 *   Open     payload = { u32 device_type, u32 flags }
 *   Ioctl    payload = IoctlReq(24) | data | nested | deep
 *   ScanoutFlip payload = { u32 scanout, owner_handle, host_handle, width,
 *                height, stride, offset, fourcc; u64 modifier, seq;
 *                u32 reserved[4] } (64 bytes); the reply is a bare header
 *   IoctlReq = { u32 cmd, data_len, nested_offset, nested_len,
 *                deep_ptr_offset, deep_len }
 *   reply    = MsgHeader | (Ioctl) IoctlResp(12) | data | nested | deep
 *   IoctlResp = { u32 data_len, nested_len, deep_len }
 *
 * `status` in a reply is the host's verdict on the call: 0, or a NEGATIVE
 * errno. RM's own NV_STATUS stays inside the payload.
 */
#ifndef CRM_WIN_WIRE_H
#define CRM_WIN_WIRE_H

#include <stddef.h>
#include <errno.h>
#include <stdint.h>
#include <string.h>

#define CRM_WIRE_MSG_OPEN 1u
#define CRM_WIRE_MSG_CLOSE 2u
#define CRM_WIRE_MSG_IOCTL 3u
#define CRM_WIRE_MSG_GET_PROC_FILES 6u
#define CRM_WIRE_MSG_GET_SYS_FILES 7u
#define CRM_WIRE_MSG_SCANOUT_FLIP 20u
/* An RM-export resource as a GEM handle of one of our render nodes
 * (docs/VENUS.md "RM-export resources in a second process"). */
#define CRM_WIRE_MSG_RM_RESOURCE_IMPORT 31u

#define CRM_WIRE_DEV_CTL 255u /* /dev/nvidiactl */
/* DRM render node n of the host's GetSysFiles DRI list (DeviceKind::Dri). */
#define CRM_WIRE_DEV_DRI_BASE 512u

#define CRM_WIRE_HDR 16u
#define CRM_WIRE_IOCTL_REQ 24u
#define CRM_WIRE_IOCTL_RESP 12u
#define CRM_WIRE_SCANOUT_FLIP 64u
#define CRM_WIRE_RM_RESOURCE_IMPORT 16u
#define CRM_WIRE_RM_RESOURCE_IMPORT_REPLY 24u

/* The backend caps one Ioctl's blocks at 1 MiB each (the guest module does). */
#define CRM_WIRE_BLOCK_MAX (1024u * 1024u)

/* open(2) flags the host is told. O_RDWR; the host only reads the access mode. */
#define CRM_WIRE_OPEN_FLAGS 2u

/* Any Linux ioctl number: _IOC(dir, type, nr, size). dir 1 = write (_IOW),
 * 2 = read (_IOR), 3 = both (_IOWR). DRM's are type 'd'. */
static inline uint32_t crm_wire_ioc(uint32_t dir, uint32_t type, uint32_t nr, uint32_t size)
{
    return ((dir & 3u) << 30) | ((size & 0x3fffu) << 16) | ((type & 0xffu) << 8) | (nr & 0xffu);
}

/* Linux ioctl number for an NVIDIA escape: _IOWR('F', nr, size). The host
 * dispatches on the full number, exactly as the guest module forwards it. */
static inline uint32_t crm_wire_cmd(uint32_t nr, uint32_t size)
{
    return crm_wire_ioc(3u, 'F', nr, size);
}

static inline void crm_put32(uint8_t *p, uint32_t v)
{
    memcpy(p, &v, sizeof(v));
}

static inline uint32_t crm_get32(const uint8_t *p)
{
    uint32_t v;
    memcpy(&v, p, sizeof(v));
    return v;
}

static inline void crm_wire_header(uint8_t *out, uint32_t type, uint32_t handle)
{
    crm_put32(out + 0, type);
    crm_put32(out + 4, handle);
    crm_put32(out + 8, 0); /* status */
    crm_put32(out + 12, 0);
}

/* Open: MsgHeader | { device_type, flags }. Returns the request length (24). */
static inline size_t crm_wire_open(uint8_t *out, uint32_t device_type)
{
    crm_wire_header(out, CRM_WIRE_MSG_OPEN, 0);
    crm_put32(out + 16, device_type);
    crm_put32(out + 20, CRM_WIRE_OPEN_FLAGS);
    return CRM_WIRE_HDR + 8;
}

/* Close: a bare header. Returns 16. */
static inline size_t crm_wire_close(uint8_t *out, uint32_t handle)
{
    crm_wire_header(out, CRM_WIRE_MSG_CLOSE, handle);
    return CRM_WIRE_HDR;
}

/* GetSysFiles: a bare header. Returns 16. */
static inline size_t crm_wire_get_sys_files(uint8_t *out)
{
    crm_wire_header(out, CRM_WIRE_MSG_GET_SYS_FILES, 0);
    return CRM_WIRE_HDR;
}

/* ScanoutFlip (messages.rs ScanoutFlip). The request's MsgHeader.handle is 0:
 * the buffer is named by owner_handle (a DRM-node file the guest opened) and
 * host_handle (a GEM handle in that file). */
struct crm_wire_flip {
    uint32_t scanout, owner_handle, host_handle, width, height, stride, offset, fourcc;
    uint64_t modifier, seq;
};

static inline void crm_put64(uint8_t *p, uint64_t v)
{
    memcpy(p, &v, sizeof(v));
}

/* MsgHeader | ScanoutFlip. Returns the request length (80). */
static inline size_t crm_wire_scanout_flip(uint8_t *out, const struct crm_wire_flip *f)
{
    crm_wire_header(out, CRM_WIRE_MSG_SCANOUT_FLIP, 0);
    uint8_t *p = out + CRM_WIRE_HDR;
    crm_put32(p + 0, f->scanout);
    crm_put32(p + 4, f->owner_handle);
    crm_put32(p + 8, f->host_handle);
    crm_put32(p + 12, f->width);
    crm_put32(p + 16, f->height);
    crm_put32(p + 20, f->stride);
    crm_put32(p + 24, f->offset);
    crm_put32(p + 28, f->fourcc);
    crm_put64(p + 32, f->modifier);
    crm_put64(p + 40, f->seq);
    memset(p + 48, 0, 16); /* reserved */
    return CRM_WIRE_HDR + CRM_WIRE_SCANOUT_FLIP;
}

/* MsgHeader | RmResourceImport{owner_handle, resource_id, flags 0, reserved 0}.
 * Returns the request length (32). */
static inline size_t crm_wire_rm_resource_import(uint8_t *out, uint32_t owner_handle,
                                                 uint32_t resource_id)
{
    crm_wire_header(out, CRM_WIRE_MSG_RM_RESOURCE_IMPORT, 0);
    uint8_t *p = out + CRM_WIRE_HDR;
    crm_put32(p + 0, owner_handle);
    crm_put32(p + 4, resource_id);
    crm_put32(p + 8, 0);
    crm_put32(p + 12, 0);
    return CRM_WIRE_HDR + CRM_WIRE_RM_RESOURCE_IMPORT;
}

struct crm_wire_rm_import_reply {
    uint32_t gem_handle, flags;
    uint64_t size, modifier;
};

/* The reply: 0 and *out filled, the header's negative status, or -EIO for a
 * reply too short to hold the body. */
static inline int crm_wire_parse_rm_resource_import(const uint8_t *resp, uint32_t n,
                                                    struct crm_wire_rm_import_reply *out)
{
    if (n < CRM_WIRE_HDR)
        return -EIO;
    const int32_t status = (int32_t)crm_get32(resp + 8);
    if (status)
        return status;
    if (n < CRM_WIRE_HDR + CRM_WIRE_RM_RESOURCE_IMPORT_REPLY)
        return -EIO;
    const uint8_t *p = resp + CRM_WIRE_HDR;
    out->gem_handle = crm_get32(p + 0);
    out->flags = crm_get32(p + 4);
    out->size = (uint64_t)crm_get32(p + 8) | ((uint64_t)crm_get32(p + 12) << 32);
    out->modifier = (uint64_t)crm_get32(p + 16) | ((uint64_t)crm_get32(p + 20) << 32);
    return out->gem_handle ? 0 : -EIO;
}

/* Request and reply sizes of an Ioctl carrying `data` plus one nested block. */
static inline size_t crm_wire_ioctl_req_size(uint32_t data_len, uint32_t nested_len)
{
    return (size_t)CRM_WIRE_HDR + CRM_WIRE_IOCTL_REQ + data_len + nested_len;
}

static inline size_t crm_wire_ioctl_resp_max(uint32_t data_len, uint32_t nested_len)
{
    return (size_t)CRM_WIRE_HDR + CRM_WIRE_IOCTL_RESP + data_len + nested_len;
}

/* Ioctl with a data block and (optionally) one nested block, no deep block.
 * `out` must hold crm_wire_ioctl_req_size() bytes; `nested` may be NULL when
 * nested_len is 0. Returns the request length. The nested block is placed
 * straight after the data block, and nested_offset equals data_len, which is
 * how the guest module lays it out. */
static inline size_t crm_wire_ioctl(uint8_t *out, uint32_t handle, uint32_t cmd,
                                    const void *data, uint32_t data_len,
                                    const void *nested, uint32_t nested_len)
{
    crm_wire_header(out, CRM_WIRE_MSG_IOCTL, handle);
    crm_put32(out + 16, cmd);
    crm_put32(out + 20, data_len);
    crm_put32(out + 24, nested_len ? data_len : 0);
    crm_put32(out + 28, nested_len);
    crm_put32(out + 32, 0); /* deep_ptr_offset */
    crm_put32(out + 36, 0); /* deep_len */
    uint8_t *body = out + CRM_WIRE_HDR + CRM_WIRE_IOCTL_REQ;
    if (data_len)
        memcpy(body, data, data_len);
    if (nested_len)
        memcpy(body + data_len, nested, nested_len);
    return crm_wire_ioctl_req_size(data_len, nested_len);
}

/* A parsed Ioctl reply. `data` and `nested` point into the reply buffer. */
struct crm_wire_reply {
    int32_t status;       /* MsgHeader.status: 0 or a negative errno */
    uint32_t data_len;    /* bytes of data the host returned */
    uint32_t nested_len;  /* bytes of nested block the host returned */
    const uint8_t *data;
    const uint8_t *nested;
};

/* Parse `MsgHeader | IoctlResp | data | nested | deep`. A reply that is only a
 * header (the host refused the call, status < 0) parses with zero lengths; a
 * short reply with status >= 0 is malformed. Returns 0, or -1 if the reply is
 * shorter than what it claims. Lengths are clamped to what
 * the caller can take (`data_cap`, `nested_cap`), never beyond the reply. */
static inline int crm_wire_parse_ioctl_reply(const uint8_t *resp, size_t resp_len,
                                             uint32_t data_cap, uint32_t nested_cap,
                                             struct crm_wire_reply *out)
{
    memset(out, 0, sizeof(*out));
    if (resp_len < CRM_WIRE_HDR)
        return -1;
    out->status = (int32_t)crm_get32(resp + 8);
    if (resp_len < CRM_WIRE_HDR + CRM_WIRE_IOCTL_RESP) {
        /* Header only: the host refused the call, which it signals with a negative
         * errno. A short reply that claims success is a malformed one, and handing
         * the caller "success" with its struct untouched would be worse than an
         * error. */
        return out->status < 0 ? 0 : -1;
    }
    const uint8_t *body = resp + CRM_WIRE_HDR + CRM_WIRE_IOCTL_RESP;
    size_t avail = resp_len - CRM_WIRE_HDR - CRM_WIRE_IOCTL_RESP;
    uint32_t data_len = crm_get32(resp + CRM_WIRE_HDR + 0);
    uint32_t nested_len = crm_get32(resp + CRM_WIRE_HDR + 4);
    if (data_len > avail)
        return -1;
    if (nested_len > avail - data_len)
        return -1;
    out->data_len = data_len < data_cap ? data_len : data_cap;
    out->nested_len = nested_len < nested_cap ? nested_len : nested_cap;
    out->data = body;
    out->nested = body + data_len;
    return 0;
}

/*
 * GetSysFiles stream -> the host's per-class allocation parameter sizes.
 *
 * The stream is: section 1, (path_len, content_len, path, content)* ended by a
 * (0, 0) record; section 2, u32 count then count DRI records of
 * (16 + 4 * 9) bytes plus a name; section 3, magic 'NVAL' (0x4e56414c), u32
 * count, then count (u32 class, u32 params_size) pairs. Sections 4 and 5
 * follow and are not read here. Section 3 is guarded by its magic, not by
 * position, because a backend too old to send it leaves zeroes there.
 *
 * Writes up to `cap` {class, size} pairs to `pairs` (2 words each) and returns
 * how many were found (0 if the stream has no section 3 or is malformed).
 */
#define CRM_WIRE_DEV_INFO_WORDS 9u
#define CRM_WIRE_DRI_RECORD_BYTES (16u + 4u * CRM_WIRE_DEV_INFO_WORDS)
#define CRM_WIRE_ALLOC_SIZE_MAGIC 0x4e56414cu

static inline uint32_t crm_wire_parse_alloc_sizes(const uint8_t *s, size_t len,
                                                  uint32_t *pairs, uint32_t cap)
{
    size_t at = 0;

    /* section 1 */
    for (;;) {
        if (at + 8 > len)
            return 0;
        uint32_t path_len = crm_get32(s + at);
        uint32_t content_len = crm_get32(s + at + 4);
        at += 8;
        if (path_len == 0 && content_len == 0)
            break;
        if ((size_t)path_len + content_len > len - at)
            return 0;
        at += (size_t)path_len + content_len;
    }

    /* section 2 */
    if (at + 4 > len)
        return 0;
    uint32_t ndri = crm_get32(s + at);
    at += 4;
    for (uint32_t i = 0; i < ndri; i++) {
        if (at + CRM_WIRE_DRI_RECORD_BYTES > len)
            return 0;
        uint32_t name_len = crm_get32(s + at);
        at += CRM_WIRE_DRI_RECORD_BYTES;
        if (name_len == 0 || name_len > len - at)
            return 0;
        at += name_len;
    }

    /* section 3 */
    if (at + 8 > len || crm_get32(s + at) != CRM_WIRE_ALLOC_SIZE_MAGIC)
        return 0;
    uint32_t count = crm_get32(s + at + 4);
    at += 8;
    uint32_t n = 0;
    for (uint32_t i = 0; i < count; i++) {
        if (at + 8 > len)
            break;
        if (n < cap) {
            pairs[2 * n] = crm_get32(s + at);
            pairs[2 * n + 1] = crm_get32(s + at + 4);
            n++;
        }
        at += 8;
    }
    return n;
}

#endif /* CRM_WIN_WIRE_H */

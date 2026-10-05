/* SPDX-License-Identifier: MIT */
/* NV_STATUS and -errno to text. */
#include "rmclient.h"

#include <errno.h>
#include <string.h>

struct status_entry {
    unsigned value;
    const char *name;
    const char *text;
};

static const struct status_entry nv_status_table[] = {
#define CRM_NV_STATUS(name, value, text) { value, #name, text },
#include "nv_status_codes.inc"
#undef CRM_NV_STATUS
};

static const struct status_entry *nv_lookup(unsigned v)
{
    for (size_t i = 0; i < sizeof(nv_status_table) / sizeof(nv_status_table[0]); i++)
        if (nv_status_table[i].value == v)
            return &nv_status_table[i];
    return NULL;
}

static const char *errno_name(int e)
{
    switch (e) {
#define E(x) case x: return "-" #x;
    E(EPERM) E(ENOENT) E(EINTR) E(EIO) E(ENXIO) E(E2BIG) E(EBADF) E(EAGAIN)
    E(ENOMEM) E(EACCES) E(EFAULT) E(EBUSY) E(EEXIST) E(ENODEV) E(EINVAL)
    E(ENFILE) E(EMFILE) E(ENOTTY) E(ENOSPC) E(ERANGE) E(ENOSYS) E(EPROTO)
    E(EOVERFLOW) E(ENOTSUP) E(ETIMEDOUT)
#undef E
    default: return "-errno";
    }
}

const char *crm_status_name(int status)
{
    if (status < 0)
        return errno_name(-status);
    const struct status_entry *e = nv_lookup((unsigned)status);
    return e ? e->name : "NV_ERR_UNKNOWN";
}

const char *crm_status_string(int status)
{
    if (status < 0) {
        if (status == -EPROTO)
            return "RM version mismatch (NV_ESC_CHECK_VERSION_STR refused)";
        return strerror(-status);
    }
    const struct status_entry *e = nv_lookup((unsigned)status);
    return e ? e->text : "Unknown NV_STATUS";
}

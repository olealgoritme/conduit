/* SPDX-License-Identifier: MIT */
/*
 * crm_event_smoke: a copy of crm_smoke reduced to OS events, extended to fire
 * a real notification and read its payload back with crm_event_drain
 * (NV_ESC_RM_GET_EVENT_DATA).
 *
 * Trigger: NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION arms NV2080_NOTIFIERS_SW on
 * the subdevice (action REPEAT), then NV2080_CTRL_CMD_EVENT_SET_TRIGGER makes
 * RM call gpuNotifySubDeviceEvent(NV2080_NOTIFIERS_SW) -- in kernel RM, no
 * GPU work, on demand.
 */
#include <errno.h>
#include <inttypes.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>

#include "rmclient.h"
#include "nv_ioctl_defs.h"

#define NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION 0x20800301u
#define NV2080_CTRL_CMD_EVENT_SET_TRIGGER 0x20800302u
#define NV2080_NOTIFIERS_SW 0u
#define ACTION_DISABLE 0u
#define ACTION_REPEAT 2u

typedef struct {
    uint32_t event;
    uint32_t action;
    uint8_t bNotifyState;
    uint32_t info32;
    uint16_t info16;
} SET_NOTIFICATION_PARAMS;
_Static_assert(sizeof(SET_NOTIFICATION_PARAMS) == 20, "NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS");

static int failed;

static int step(const char *what, int r)
{
    if (r == 0)
        printf("[ ok ] %s\n", what);
    else
        printf("[FAIL] %s: %s (%d / 0x%x) %s\n", what, crm_status_name(r), r,
               (unsigned)r, crm_status_string(r));
    fflush(stdout);
    if (r)
        failed = 1;
    return r;
}

static int expect(const char *what, int ok)
{
    printf("[%s] %s\n", ok ? " ok " : "FAIL", what);
    fflush(stdout);
    if (!ok)
        failed = 1;
    return ok;
}

static int readable(int fd, int ms)
{
    struct pollfd p = { .fd = fd, .events = POLLIN };
    int r = poll(&p, 1, ms);
    return r > 0 && (p.revents & POLLIN);
}

int main(void)
{
    crm_client *c = NULL;
    if (step("crm_open", crm_open(&c, NULL)))
        return 1;
    printf("       RM version %s, root client 0x%08x\n", crm_rm_version(c), crm_root(c));

    uint32_t dev = 0, sub = 0, ev = 0;
    int efd = -1;
    struct crm_event_data d[8];
    int nd;

    NV0080_ALLOC_PARAMETERS dp;
    memset(&dp, 0, sizeof(dp));
    if (step("alloc NV01_DEVICE_0", crm_alloc(c, crm_root(c), &dev, NV01_DEVICE_0, &dp, sizeof(dp))))
        goto out;
    NV2080_ALLOC_PARAMETERS sp = { .subDeviceId = 0 };
    if (step("alloc NV20_SUBDEVICE_0", crm_alloc(c, dev, &sub, NV20_SUBDEVICE_0, &sp, sizeof(sp))))
        goto out;
    printf("       device 0x%08x subdevice 0x%08x\n", dev, sub);

    /* GET_EVENT_DATA on the control fd, which has no OS event: the backend
     * refuses it without calling the host. */
    {
        NvUnixEvent ue;
        NVOS41_PARAMETERS p;
        memset(&ue, 0, sizeof(ue));
        memset(&p, 0, sizeof(p));
        p.pEvent = (uint64_t)(uintptr_t)&ue;
        int r = crm_escape(c, -1, NV_ESC_RM_GET_EVENT_DATA, &p, sizeof(p));
        printf("       GET_EVENT_DATA on the control fd: rc=%d (%s), status=0x%x\n", r,
               crm_status_name(r), p.status);
        expect("GET_EVENT_DATA refused on a file with no OS event", r == -EINVAL);
    }

    if (step("crm_event_open(subdevice, NV2080_NOTIFIERS_SW)",
             crm_event_open(c, sub, NV2080_NOTIFIERS_SW, &ev, &efd)))
        goto out;
    printf("       event object 0x%08x on fd %d\n", ev, efd);

    nd = crm_event_drain(c, efd, d, 8);
    printf("       before any trigger: drain -> %d\n", nd);
    expect("nothing pending before the trigger", nd == 0);

    SET_NOTIFICATION_PARAMS sn;
    memset(&sn, 0, sizeof(sn));
    sn.event = NV2080_NOTIFIERS_SW;
    sn.action = ACTION_REPEAT;
    if (step("NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION (SW, REPEAT)",
             crm_control(c, sub, NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION, &sn, sizeof(sn))))
        goto close_event;

    /* One trigger, read through poll + drain. */
    step("NV2080_CTRL_CMD_EVENT_SET_TRIGGER x1",
         crm_control(c, sub, NV2080_CTRL_CMD_EVENT_SET_TRIGGER, NULL, 0));
    expect("event fd readable (poll POLLIN) after the trigger", readable(efd, 2000));
    memset(d, 0, sizeof(d));
    nd = crm_event_drain(c, efd, d, 8);
    printf("       drain -> %d\n", nd);
    for (int i = 0; i < nd && i < 8; i++)
        printf("       event[%d]: hObject 0x%08x notifyIndex %u info32 0x%08x info16 0x%04x\n", i,
               d[i].object, d[i].notify_index, d[i].info32, d[i].info16);
    expect("one notification read", nd == 1);
    expect("hObject is the guest's event handle", nd >= 1 && d[0].object == ev);
    expect("notifyIndex is NV2080_NOTIFIERS_SW", nd >= 1 && d[0].notify_index == NV2080_NOTIFIERS_SW);

    /* Three triggers queued before reading: MoreEvents carries the loop. */
    for (int i = 0; i < 3; i++)
        step("NV2080_CTRL_CMD_EVENT_SET_TRIGGER",
             crm_control(c, sub, NV2080_CTRL_CMD_EVENT_SET_TRIGGER, NULL, 0));
    expect("event fd readable after three triggers", readable(efd, 2000));
    memset(d, 0, sizeof(d));
    nd = crm_event_drain(c, efd, d, 8);
    printf("       drain -> %d\n", nd);
    for (int i = 0; i < nd && i < 8; i++)
        printf("       event[%d]: hObject 0x%08x notifyIndex %u info32 0x%08x info16 0x%04x\n", i,
               d[i].object, d[i].notify_index, d[i].info32, d[i].info16);
    expect("three notifications read in one drain (MoreEvents)", nd == 3);

    nd = crm_event_drain(c, efd, d, 8);
    printf("       drain again -> %d\n", nd);
    expect("queue empty afterwards", nd == 0);

    sn.action = ACTION_DISABLE;
    step("NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION (SW, DISABLE)",
         crm_control(c, sub, NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION, &sn, sizeof(sn)));

close_event:
    step("crm_event_close", crm_event_close(c, ev, efd));
out:
    if (sub) step("free subdevice", crm_free(c, dev, sub));
    if (dev) step("free device", crm_free(c, crm_root(c), dev));
    crm_close(c);
    printf("%s\n", failed ? "EVENT SMOKE FAILED" : "EVENT SMOKE PASSED");
    return failed ? 1 : 0;
}

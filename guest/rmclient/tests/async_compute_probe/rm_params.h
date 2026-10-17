/* SPDX-License-Identifier: MIT */
/* RM allocation / control parameter layouts the probe uses (nvkmd_rm_ctx.c,
 * crm_ce_copy_smoke.c, nvos.h of the open kernel modules); sizes as the
 * 610.57.04 allowlist checks them. */
#ifndef RM_PARAMS_H
#define RM_PARAMS_H
#include <stdint.h>
#include "nv_ioctl_defs.h"

#define NV2080_CTRL_FIFO_GET_INFO_MAX_ENTRIES 256u

typedef struct {
    uint32_t hObjectError, hObjectEccError, hVASpace, engineType;
    uint8_t bIsCallingContextVgpuPlugin;
} NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS;
typedef struct {
    uint32_t hVASpace, flags, subctxId;
} NV_CTXSHARE_ALLOCATION_PARAMETERS;
typedef struct {
    NV_A8 uint64_t base;
    NV_A8 uint64_t size;
    uint32_t addressSpace, cacheAttrib;
} NV_MEMORY_DESC_PARAMS;
typedef struct {
    uint32_t hObjectError, hObjectBuffer;
    NV_A8 uint64_t gpFifoOffset;
    uint32_t gpFifoEntries, flags, hContextShare, hVASpace, hHandleVASpace;
    uint32_t hUserdMemory[8];
    NV_A8 uint64_t userdOffset[8];
    uint32_t engineType, cid, subDeviceId, hObjectEccError;
    NV_MEMORY_DESC_PARAMS instanceMem, userdMem, ramfcMem, mthdbufMem;
    uint32_t hPhysChannelGroup, internalFlags;
    NV_MEMORY_DESC_PARAMS errorNotifierMem, eccErrorNotifierMem;
    uint32_t ProcessID, SubProcessID;
    uint32_t encryptIv[3], decryptIv[3], hmacNonce[8];
    uint32_t tpcConfigID;
} NV_CHANNEL_ALLOC_PARAMS;
typedef struct {
    uint8_t bBar1Mapping, bPriv;
} NV_HOPPER_USERMODE_A_PARAMS;
typedef struct {
    uint8_t bEnable, bSkipSubmit, bSkipEnable;
} NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS;
typedef struct {
    uint32_t index, data;
} NV2080_CTRL_FIFO_INFO;
typedef struct {
    uint32_t fifoInfoTblSize;
    NV2080_CTRL_FIFO_INFO fifoInfoTbl[NV2080_CTRL_FIFO_GET_INFO_MAX_ENTRIES];
    uint32_t engineType;
} NV2080_CTRL_FIFO_GET_INFO_PARAMS;
typedef struct { /* nvgputypes.h NvNotification */
    uint32_t timestamp[2];
    uint32_t info32;
    uint16_t info16, status;
} nv_notification;

_Static_assert(sizeof(NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS) == 20, "0xa06c params");
_Static_assert(sizeof(NV_CTXSHARE_ALLOCATION_PARAMETERS) == 12, "0x9067 params");
_Static_assert(sizeof(NV_CHANNEL_ALLOC_PARAMS) == 376, "*6F params");
_Static_assert(sizeof(NV_HOPPER_USERMODE_A_PARAMS) == 2, "0xc761 params");
_Static_assert(sizeof(NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS) == 3, "GPFIFO_SCHEDULE");
_Static_assert(sizeof(NV2080_CTRL_FIFO_GET_INFO_PARAMS) == 2056, "FIFO_GET_INFO");
_Static_assert(sizeof(nv_notification) == 16, "NvNotification");

#endif

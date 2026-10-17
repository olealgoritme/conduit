/*
 * VK_LAYER_CONDUIT_pass_profile: GPU time per render pass, per run of
 * operations and per command buffer, from GPU timestamps, for any Vulkan
 * driver.  It is the same measurement as NVK_PASS_PROFILE (nvk-rm patches
 * 0010 and 0023), with the same pass and operation signatures and the same
 * log line, so the numbers of NVIDIA's proprietary driver (which cannot be
 * instrumented) line up one to one with NVK's: run the workload with this
 * layer on NVIDIA, with NVK_PASS_PROFILE=1 on NVK, and compare the two logs
 * with compare_pass_profiles.py.
 *
 * What is timed (as in NVK_PASS_PROFILE):
 *  - a render pass (vkCmdBeginRendering ... vkCmdEndRendering; a resumed
 *    pass continues the one its suspension ended): signature = size, samples,
 *    layers, colour attachment count, format and load/store ops of the first
 *    colour and the depth attachment;
 *  - a run of operations of one kind outside render passes (dispatch,
 *    dispatch indirect, the copies, fills, updates, clears, blits, resolves,
 *    the resolve at the end of a pass, query resets and copies): it ends at
 *    the next render pass, operation of another kind or the end of the
 *    command buffer, so it includes the barriers between its operations;
 *  - every primary command buffer ("command buffer (all work)").
 * Each interval is two bottom-of-pipe timestamps (one before its first
 * command, one after its last), so a timed region waits for the work before
 * it: fine for finding a 10x, not for measuring 1 %.
 *
 * CONDUIT_PASS_PROFILE_LABELS=1 also times VK_EXT_debug_utils label regions
 * (D3D12 PIX BeginEvent/EndEvent through vkd3d-proton with
 * VKD3D_CONFIG=debug_utils) that begin and end outside render passes, as
 * "label: <outer>/<inner>" lines (they overlap the passes and operations
 * inside them and are not part of the summary sums).
 *
 * Environment:
 *   CONDUIT_PASS_PROFILE_FILE=<path>   log file (<path>.<pid>); "-" = stderr;
 *                                      default conduit-pass.<pid> in the cwd
 *   CONDUIT_PASS_PROFILE_WINDOW_S=<n>  seconds per log window (default 2)
 *   CONDUIT_PASS_PROFILE_LABELS=1      also time debug-utils labels
 *
 * The layer enables hostQueryReset (Vulkan 1.2) on the device; a thread
 * harvests the timestamps every 200 ms.
 */
#define _GNU_SOURCE
#include <inttypes.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>

#include "fmt_names.h"

#define EXPORT __attribute__((visibility("default")))

#define SLOTS 65536u
#define MAX_SIGS 8192u
#define SIG_HASH 32768u
#define LOG_TOP 60u
#define HARVEST_MS 200u
#define MAX_LABELS 24u
#define LABEL_LEN 120u

/* ---------------------------------------------------------------- maps */

struct node {
   uint64_t k, a, b;
   struct node *next;
};

#define MAP_BUCKETS 8192u
#define MAP_LOCKS 64u

struct map {
   struct node *bucket[MAP_BUCKETS];
   pthread_mutex_t lock[MAP_LOCKS];
};

static void
map_init(struct map *m)
{
   for (unsigned i = 0; i < MAP_LOCKS; i++)
      pthread_mutex_init(&m->lock[i], NULL);
}

static inline unsigned
map_slot(uint64_t k)
{
   return (unsigned)((k * 0x9E3779B97F4A7C15ull) >> 40) % MAP_BUCKETS;
}

static void
map_put(struct map *m, uint64_t k, uint64_t a, uint64_t b)
{
   unsigned s = map_slot(k);
   pthread_mutex_t *l = &m->lock[s % MAP_LOCKS];
   pthread_mutex_lock(l);
   for (struct node *n = m->bucket[s]; n; n = n->next) {
      if (n->k == k) {
         n->a = a;
         n->b = b;
         pthread_mutex_unlock(l);
         return;
      }
   }
   struct node *n = malloc(sizeof(*n));
   if (n) {
      n->k = k;
      n->a = a;
      n->b = b;
      n->next = m->bucket[s];
      m->bucket[s] = n;
   }
   pthread_mutex_unlock(l);
}

static bool
map_get(struct map *m, uint64_t k, uint64_t *a, uint64_t *b)
{
   unsigned s = map_slot(k);
   pthread_mutex_t *l = &m->lock[s % MAP_LOCKS];
   bool found = false;
   pthread_mutex_lock(l);
   for (struct node *n = m->bucket[s]; n; n = n->next) {
      if (n->k == k) {
         if (a)
            *a = n->a;
         if (b)
            *b = n->b;
         found = true;
         break;
      }
   }
   pthread_mutex_unlock(l);
   return found;
}

static bool
map_del(struct map *m, uint64_t k, uint64_t *a)
{
   unsigned s = map_slot(k);
   pthread_mutex_t *l = &m->lock[s % MAP_LOCKS];
   bool found = false;
   pthread_mutex_lock(l);
   for (struct node **p = &m->bucket[s]; *p; p = &(*p)->next) {
      if ((*p)->k == k) {
         struct node *n = *p;
         if (a)
            *a = n->a;
         *p = n->next;
         free(n);
         found = true;
         break;
      }
   }
   pthread_mutex_unlock(l);
   return found;
}

/* ------------------------------------------------------------ hook table */

/* Every intercepted device function.  A function under its core and its KHR
 * name is two entries (each loads its own next).
 */
#define HOOKS(X) \
   X(DestroyDevice) X(BeginCommandBuffer) X(EndCommandBuffer) \
   X(AllocateCommandBuffers) X(FreeCommandBuffers) X(CreateCommandPool) \
   X(DestroyCommandPool) X(CreateImage) X(DestroyImage) X(CreateImageView) \
   X(DestroyImageView) X(QueuePresentKHR) \
   X(CmdBeginRendering) X(CmdBeginRenderingKHR) X(CmdEndRendering) \
   X(CmdEndRenderingKHR) \
   X(CmdDraw) X(CmdDrawIndexed) X(CmdDrawIndirect) X(CmdDrawIndexedIndirect) \
   X(CmdDrawIndirectCount) X(CmdDrawIndirectCountKHR) \
   X(CmdDrawIndexedIndirectCount) X(CmdDrawIndexedIndirectCountKHR) \
   X(CmdDrawMultiEXT) X(CmdDrawMultiIndexedEXT) \
   X(CmdDrawMeshTasksEXT) X(CmdDrawMeshTasksIndirectEXT) \
   X(CmdDrawMeshTasksIndirectCountEXT) \
   X(CmdDispatch) X(CmdDispatchBase) X(CmdDispatchBaseKHR) \
   X(CmdDispatchIndirect) \
   X(CmdCopyBuffer) X(CmdCopyBuffer2) X(CmdCopyBuffer2KHR) \
   X(CmdCopyImage) X(CmdCopyImage2) X(CmdCopyImage2KHR) \
   X(CmdCopyBufferToImage) X(CmdCopyBufferToImage2) \
   X(CmdCopyBufferToImage2KHR) \
   X(CmdCopyImageToBuffer) X(CmdCopyImageToBuffer2) \
   X(CmdCopyImageToBuffer2KHR) \
   X(CmdFillBuffer) X(CmdUpdateBuffer) \
   X(CmdClearColorImage) X(CmdClearDepthStencilImage) \
   X(CmdBlitImage) X(CmdBlitImage2) X(CmdBlitImage2KHR) \
   X(CmdResolveImage) X(CmdResolveImage2) X(CmdResolveImage2KHR) \
   X(CmdResetQueryPool) X(CmdCopyQueryPoolResults) \
   X(CmdPipelineBarrier) X(CmdPipelineBarrier2) X(CmdPipelineBarrier2KHR) \
   X(CmdBeginDebugUtilsLabelEXT) X(CmdEndDebugUtilsLabelEXT)

enum hook_id {
#define X(n) H_##n,
   HOOKS(X)
#undef X
   H_COUNT
};

static const char *const hook_names[H_COUNT] = {
#define X(n) "vk" #n,
   HOOKS(X)
#undef X
};

/* Operation kinds, in NVK's names */
enum op_kind {
   OP_NONE,
   OP_DISPATCH,
   OP_DISPATCH_INDIRECT,
   OP_COPY_BUFFER,
   OP_COPY_IMAGE,
   OP_COPY_BUFFER_TO_IMAGE,
   OP_COPY_IMAGE_TO_BUFFER,
   OP_FILL_BUFFER,
   OP_UPDATE_BUFFER,
   OP_CLEAR_COLOR_IMAGE,
   OP_CLEAR_DS_IMAGE,
   OP_BLIT,
   OP_RESOLVE_IMAGE,
   OP_RESOLVE_END_OF_PASS,
   OP_QUERY_RESET,
   OP_QUERY_COPY,
   OP_COUNT
};

static const char *const op_names[OP_COUNT] = {
   [OP_NONE] = "none",
   [OP_DISPATCH] = "dispatch",
   [OP_DISPATCH_INDIRECT] = "dispatch indirect",
   [OP_COPY_BUFFER] = "copy buffer",
   [OP_COPY_IMAGE] = "copy image",
   [OP_COPY_BUFFER_TO_IMAGE] = "copy buffer to image",
   [OP_COPY_IMAGE_TO_BUFFER] = "copy image to buffer",
   [OP_FILL_BUFFER] = "fill buffer",
   [OP_UPDATE_BUFFER] = "update buffer",
   [OP_CLEAR_COLOR_IMAGE] = "clear color image",
   [OP_CLEAR_DS_IMAGE] = "clear depth/stencil image",
   [OP_BLIT] = "blit",
   [OP_RESOLVE_IMAGE] = "resolve image",
   [OP_RESOLVE_END_OF_PASS] = "resolve at end of pass",
   [OP_QUERY_RESET] = "query reset",
   [OP_QUERY_COPY] = "query copy results",
};

enum sig_kind { KIND_PASS, KIND_OP, KIND_CMD, KIND_LABEL };

struct counts {
   uint32_t calls, draws, ind_calls, ind_draws, barriers;
};

/* ------------------------------------------------------------- profile */

struct sig {
   char name[224];
   uint8_t kind;
   uint8_t op;
   uint64_t ns, count;
   uint64_t calls, draws, ind_calls, ind_draws, barriers;
};

struct meta {
   uint32_t sig; /* index + 1; 0 = none */
   struct counts c;
};

struct dev;

struct prof {
   struct dev *dev;
   VkQueryPool pool;
   struct meta *meta;
   _Atomic uint32_t next;
   _Atomic uint64_t frames;
   uint64_t frames_window_start;

   pthread_mutex_t sig_mutex; /* creation and lookup of signatures */
   struct sig *sigs;
   _Atomic uint32_t sig_count;
   uint16_t *sig_ht; /* SIG_HASH entries, index + 1 */
   uint64_t dropped, dropped_window;

   pthread_mutex_t totals_mutex; /* harvest totals and window */
   uint64_t window_start_ns;
   uint64_t windows;
   double window_s;

   pthread_t thread;
   bool thread_started;
   pthread_mutex_t stop_mutex;
   pthread_cond_t stop_cond;
   bool stop;
};

struct inst {
   void *key;
   VkInstance h;
   PFN_vkGetInstanceProcAddr gipa;
   PFN_vkDestroyInstance DestroyInstance;
   PFN_vkGetPhysicalDeviceProperties GetPDProps;
   PFN_vkGetPhysicalDeviceQueueFamilyProperties GetQFP;
   struct inst *next;
};

struct dev {
   void *key;
   VkDevice h;
   struct inst *inst;
   PFN_vkGetDeviceProcAddr gdpa;
   PFN_vkVoidFunction nx[H_COUNT];
   PFN_vkCreateQueryPool CreateQueryPool;
   PFN_vkDestroyQueryPool DestroyQueryPool;
   PFN_vkGetQueryPoolResults GetQueryPoolResults;
   PFN_vkResetQueryPool ResetQueryPool;
   PFN_vkCmdWriteTimestamp CmdWriteTimestamp;
   double period_ns;
   uint64_t ts_mask;
   uint32_t family_count;
   VkQueueFlags *family_flags;
   uint32_t *family_valid_bits;
   struct prof *prof; /* NULL: the layer passes everything through */
   bool labels;
   uint32_t index;
   struct dev *next;
};

static struct inst *g_insts;
static struct dev *g_devs;
static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;
static struct map g_cbs;    /* VkCommandBuffer -> struct cbs * */
static struct map g_pools;  /* VkCommandPool -> queue family */
static struct map g_images; /* VkImage -> samples */
static struct map g_views;  /* VkImageView -> format, image */
static pthread_once_t g_once = PTHREAD_ONCE_INIT;
static FILE *g_file;
static pthread_mutex_t g_file_lock = PTHREAD_MUTEX_INITIALIZER;
static uint32_t g_dev_count;

static void
global_init(void)
{
   map_init(&g_cbs);
   map_init(&g_pools);
   map_init(&g_images);
   map_init(&g_views);
}

static inline void *
dkey(const void *handle)
{
   return *(void *const *)handle;
}

static struct inst *
inst_by_key(void *k)
{
   pthread_mutex_lock(&g_lock);
   struct inst *i = g_insts;
   while (i && i->key != k)
      i = i->next;
   pthread_mutex_unlock(&g_lock);
   return i;
}

static struct dev *
dev_by_key(void *k)
{
   pthread_mutex_lock(&g_lock);
   struct dev *d = g_devs;
   while (d && d->key != k)
      d = d->next;
   pthread_mutex_unlock(&g_lock);
   return d;
}

static uint64_t
now_ns(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void __attribute__((format(printf, 1, 2)))
plog(const char *fmt, ...)
{
   char line[1024];
   va_list args;
   va_start(args, fmt);
   vsnprintf(line, sizeof(line), fmt, args);
   va_end(args);
   pthread_mutex_lock(&g_file_lock);
   if (g_file == NULL) {
      const char *env = getenv("CONDUIT_PASS_PROFILE_FILE");
      if (env != NULL && strcmp(env, "-") == 0) {
         g_file = stderr;
      } else {
         char path[1024];
         if (env != NULL && env[0])
            snprintf(path, sizeof(path), "%s.%d", env, (int)getpid());
         else
            snprintf(path, sizeof(path), "conduit-pass.%d", (int)getpid());
         g_file = fopen(path, "a");
         if (g_file == NULL)
            g_file = stderr;
      }
   }
   fprintf(g_file, "%s\n", line);
   pthread_mutex_unlock(&g_file_lock);
}

static void
plog_flush(void)
{
   pthread_mutex_lock(&g_file_lock);
   if (g_file)
      fflush(g_file);
   pthread_mutex_unlock(&g_file_lock);
}

/* --------------------------------------------------------- signatures */

static uint32_t
sig_find(struct prof *p, const char *name, enum sig_kind kind, unsigned op)
{
   uint32_t h = 2166136261u;
   for (const char *s = name; *s; s++)
      h = (h ^ (uint8_t)*s) * 16777619u;
   pthread_mutex_lock(&p->sig_mutex);
   uint32_t i = h % SIG_HASH;
   for (;;) {
      uint16_t e = p->sig_ht[i];
      if (e == 0)
         break;
      if (strcmp(p->sigs[e - 1].name, name) == 0) {
         pthread_mutex_unlock(&p->sig_mutex);
         return e;
      }
      i = (i + 1) % SIG_HASH;
   }
   uint32_t count = atomic_load(&p->sig_count);
   if (count >= MAX_SIGS) {
      p->dropped++;
      p->dropped_window++;
      pthread_mutex_unlock(&p->sig_mutex);
      return 0;
   }
   struct sig *s = &p->sigs[count];
   memset(s, 0, sizeof(*s));
   snprintf(s->name, sizeof(s->name), "%s", name);
   s->kind = kind;
   s->op = op;
   p->sig_ht[i] = (uint16_t)(count + 1);
   atomic_store(&p->sig_count, count + 1);
   pthread_mutex_unlock(&p->sig_mutex);
   return count + 1;
}

/* ------------------------------------------------------- command buffers */

struct cbs {
   VkCommandBuffer h;
   struct dev *d;
   VkCommandPool pool;
   bool primary;
   uint8_t kind; /* 0 untimed, 1 graphics, 2 compute-only */

   int32_t cmd_slot, pass_slot, op_slot;
   bool in_pass;
   bool pass_suspending;
   bool pass_resolve;
   char pass_name[256];
   struct counts cmd_c, pass_c, op_c;
   enum op_kind op;
   uint32_t op_n;
   uint64_t op_groups;

   uint32_t label_depth;
   struct {
      int32_t slot; /* -1: not timed */
      char path[LABEL_LEN];
   } label[MAX_LABELS];
};

static struct cbs *
cbs_get(VkCommandBuffer cb)
{
   uint64_t a;
   if (!map_get(&g_cbs, (uint64_t)(uintptr_t)cb, &a, NULL))
      return NULL;
   return (struct cbs *)(uintptr_t)a;
}

static inline bool
timed(const struct cbs *c)
{
   return c != NULL && c->kind != 0 && c->primary && c->d->prof != NULL;
}

static int32_t
slot_begin(struct cbs *c)
{
   struct prof *p = c->d->prof;
   uint32_t s = atomic_fetch_add(&p->next, 1) % SLOTS;
   p->meta[s].sig = 0;
   c->d->CmdWriteTimestamp(c->h, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                           p->pool, 2 * s);
   return (int32_t)s;
}

static void
slot_end(struct cbs *c, int32_t slot, uint32_t sig, const struct counts *cnt)
{
   struct prof *p = c->d->prof;
   if (slot < 0)
      return;
   if (cnt)
      p->meta[slot].c = *cnt;
   else
      memset(&p->meta[slot].c, 0, sizeof(p->meta[slot].c));
   p->meta[slot].sig = sig;
   c->d->CmdWriteTimestamp(c->h, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                           p->pool, 2 * (uint32_t)slot + 1);
}

static inline unsigned
log2_ceil(uint64_t v)
{
   unsigned l = 0;
   while (((uint64_t)1 << l) < v && l < 63)
      l++;
   return l;
}

static void
end_op_run(struct cbs *c)
{
   if (c->op_slot < 0)
      return;
   struct prof *p = c->d->prof;
   const int32_t slot = c->op_slot;
   c->op_slot = -1;

   char name[200];
   const uint32_t n = c->op_n;
   int len = snprintf(name, sizeof(name), "ops: %s, %s%u", op_names[c->op],
                      n > 1 ? "<=" : "", n <= 1 ? n : 1u << log2_ceil(n));
   if (c->op == OP_DISPATCH) {
      const uint64_t g = c->op_groups ? c->op_groups : 1;
      snprintf(name + len, sizeof(name) - len, ", <=%" PRIu64 " groups",
               (uint64_t)1 << log2_ceil(g));
   }
   uint32_t sig = sig_find(p, name, KIND_OP, c->op);
   c->op_c.calls = n;
   slot_end(c, slot, sig, &c->op_c);
}

static void
op_begin(struct cbs *c, enum op_kind op, uint64_t groups)
{
   if (!timed(c) || c->in_pass)
      return;
   if (c->op_slot >= 0 && c->op == op) {
      c->op_n++;
      c->op_groups += groups;
      return;
   }
   end_op_run(c);
   c->op = op;
   c->op_n = 1;
   c->op_groups = groups;
   memset(&c->op_c, 0, sizeof(c->op_c));
   c->op_slot = slot_begin(c);
}

static void
count_draw(struct cbs *c, uint64_t draws, bool indirect)
{
   if (!timed(c))
      return;
   if (draws > 0xffffffffu)
      draws = 0xffffffffu;
   struct counts *cc[2] = { &c->cmd_c, &c->pass_c };
   for (int i = 0; i < 2; i++) {
      cc[i]->calls++;
      cc[i]->draws += (uint32_t)draws;
      if (indirect) {
         cc[i]->ind_calls++;
         cc[i]->ind_draws += (uint32_t)draws;
      }
   }
}

static void
count_barrier(struct cbs *c)
{
   if (!timed(c))
      return;
   c->cmd_c.barriers++;
   if (c->in_pass)
      c->pass_c.barriers++;
   else if (c->op_slot >= 0)
      c->op_c.barriers++;
}

/* -------------------------------------------------------------- hooks */

/* A hook for a function taking a command buffer first.  PRE runs with `c`
 * (the command buffer's state, NULL if the layer has none) before the next
 * layer's function; without a state the call is passed through.
 */
#define HK(N, PARAMS, ARGS, PRE) \
   static void VKAPI_CALL h_##N PARAMS \
   { \
      struct cbs *c = cbs_get(commandBuffer); \
      struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer)); \
      PFN_vk##N next = dv ? (PFN_vk##N)dv->nx[H_##N] : NULL; \
      if (next == NULL) \
         return; \
      if (c != NULL) { PRE; } \
      next ARGS; \
   }

/* Draws */
HK(CmdDraw, (VkCommandBuffer commandBuffer, uint32_t a, uint32_t b, uint32_t d, uint32_t e),
   (commandBuffer, a, b, d, e), count_draw(c, 1, false))
HK(CmdDrawIndexed, (VkCommandBuffer commandBuffer, uint32_t a, uint32_t b, uint32_t d, int32_t e, uint32_t f),
   (commandBuffer, a, b, d, e, f), count_draw(c, 1, false))
HK(CmdDrawIndirect, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, uint32_t n, uint32_t stride),
   (commandBuffer, buf, off, n, stride), count_draw(c, n, true))
HK(CmdDrawIndexedIndirect, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, uint32_t n, uint32_t stride),
   (commandBuffer, buf, off, n, stride), count_draw(c, n, true))
HK(CmdDrawIndirectCount, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, VkBuffer cbuf, VkDeviceSize coff, uint32_t max, uint32_t stride),
   (commandBuffer, buf, off, cbuf, coff, max, stride), count_draw(c, max, true))
HK(CmdDrawIndirectCountKHR, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, VkBuffer cbuf, VkDeviceSize coff, uint32_t max, uint32_t stride),
   (commandBuffer, buf, off, cbuf, coff, max, stride), count_draw(c, max, true))
HK(CmdDrawIndexedIndirectCount, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, VkBuffer cbuf, VkDeviceSize coff, uint32_t max, uint32_t stride),
   (commandBuffer, buf, off, cbuf, coff, max, stride), count_draw(c, max, true))
HK(CmdDrawIndexedIndirectCountKHR, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, VkBuffer cbuf, VkDeviceSize coff, uint32_t max, uint32_t stride),
   (commandBuffer, buf, off, cbuf, coff, max, stride), count_draw(c, max, true))
HK(CmdDrawMultiEXT, (VkCommandBuffer commandBuffer, uint32_t n, const VkMultiDrawInfoEXT *i, uint32_t inst, uint32_t first, uint32_t stride),
   (commandBuffer, n, i, inst, first, stride), count_draw(c, n, false))
HK(CmdDrawMultiIndexedEXT, (VkCommandBuffer commandBuffer, uint32_t n, const VkMultiDrawIndexedInfoEXT *i, uint32_t inst, uint32_t first, uint32_t stride, const int32_t *off),
   (commandBuffer, n, i, inst, first, stride, off), count_draw(c, n, false))
HK(CmdDrawMeshTasksEXT, (VkCommandBuffer commandBuffer, uint32_t x, uint32_t y, uint32_t z),
   (commandBuffer, x, y, z), count_draw(c, 1, false))
HK(CmdDrawMeshTasksIndirectEXT, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, uint32_t n, uint32_t stride),
   (commandBuffer, buf, off, n, stride), count_draw(c, n, true))
HK(CmdDrawMeshTasksIndirectCountEXT, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off, VkBuffer cbuf, VkDeviceSize coff, uint32_t max, uint32_t stride),
   (commandBuffer, buf, off, cbuf, coff, max, stride), count_draw(c, max, true))

/* Operations outside render passes */
HK(CmdDispatch, (VkCommandBuffer commandBuffer, uint32_t x, uint32_t y, uint32_t z),
   (commandBuffer, x, y, z), op_begin(c, OP_DISPATCH, (uint64_t)x * y * z))
HK(CmdDispatchBase, (VkCommandBuffer commandBuffer, uint32_t bx, uint32_t by, uint32_t bz, uint32_t x, uint32_t y, uint32_t z),
   (commandBuffer, bx, by, bz, x, y, z), op_begin(c, OP_DISPATCH, (uint64_t)x * y * z))
HK(CmdDispatchBaseKHR, (VkCommandBuffer commandBuffer, uint32_t bx, uint32_t by, uint32_t bz, uint32_t x, uint32_t y, uint32_t z),
   (commandBuffer, bx, by, bz, x, y, z), op_begin(c, OP_DISPATCH, (uint64_t)x * y * z))
HK(CmdDispatchIndirect, (VkCommandBuffer commandBuffer, VkBuffer buf, VkDeviceSize off),
   (commandBuffer, buf, off), op_begin(c, OP_DISPATCH_INDIRECT, 0))
HK(CmdCopyBuffer, (VkCommandBuffer commandBuffer, VkBuffer s, VkBuffer d, uint32_t n, const VkBufferCopy *r),
   (commandBuffer, s, d, n, r), op_begin(c, OP_COPY_BUFFER, 0))
HK(CmdCopyBuffer2, (VkCommandBuffer commandBuffer, const VkCopyBufferInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_BUFFER, 0))
HK(CmdCopyBuffer2KHR, (VkCommandBuffer commandBuffer, const VkCopyBufferInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_BUFFER, 0))
HK(CmdCopyImage, (VkCommandBuffer commandBuffer, VkImage s, VkImageLayout sl, VkImage d, VkImageLayout dl, uint32_t n, const VkImageCopy *r),
   (commandBuffer, s, sl, d, dl, n, r), op_begin(c, OP_COPY_IMAGE, 0))
HK(CmdCopyImage2, (VkCommandBuffer commandBuffer, const VkCopyImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_IMAGE, 0))
HK(CmdCopyImage2KHR, (VkCommandBuffer commandBuffer, const VkCopyImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_IMAGE, 0))
HK(CmdCopyBufferToImage, (VkCommandBuffer commandBuffer, VkBuffer s, VkImage d, VkImageLayout dl, uint32_t n, const VkBufferImageCopy *r),
   (commandBuffer, s, d, dl, n, r), op_begin(c, OP_COPY_BUFFER_TO_IMAGE, 0))
HK(CmdCopyBufferToImage2, (VkCommandBuffer commandBuffer, const VkCopyBufferToImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_BUFFER_TO_IMAGE, 0))
HK(CmdCopyBufferToImage2KHR, (VkCommandBuffer commandBuffer, const VkCopyBufferToImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_BUFFER_TO_IMAGE, 0))
HK(CmdCopyImageToBuffer, (VkCommandBuffer commandBuffer, VkImage s, VkImageLayout sl, VkBuffer d, uint32_t n, const VkBufferImageCopy *r),
   (commandBuffer, s, sl, d, n, r), op_begin(c, OP_COPY_IMAGE_TO_BUFFER, 0))
HK(CmdCopyImageToBuffer2, (VkCommandBuffer commandBuffer, const VkCopyImageToBufferInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_IMAGE_TO_BUFFER, 0))
HK(CmdCopyImageToBuffer2KHR, (VkCommandBuffer commandBuffer, const VkCopyImageToBufferInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_COPY_IMAGE_TO_BUFFER, 0))
HK(CmdFillBuffer, (VkCommandBuffer commandBuffer, VkBuffer b, VkDeviceSize o, VkDeviceSize s, uint32_t d),
   (commandBuffer, b, o, s, d), op_begin(c, OP_FILL_BUFFER, 0))
HK(CmdUpdateBuffer, (VkCommandBuffer commandBuffer, VkBuffer b, VkDeviceSize o, VkDeviceSize s, const void *d),
   (commandBuffer, b, o, s, d), op_begin(c, OP_UPDATE_BUFFER, 0))
HK(CmdClearColorImage, (VkCommandBuffer commandBuffer, VkImage i, VkImageLayout l, const VkClearColorValue *v, uint32_t n, const VkImageSubresourceRange *r),
   (commandBuffer, i, l, v, n, r), op_begin(c, OP_CLEAR_COLOR_IMAGE, 0))
HK(CmdClearDepthStencilImage, (VkCommandBuffer commandBuffer, VkImage i, VkImageLayout l, const VkClearDepthStencilValue *v, uint32_t n, const VkImageSubresourceRange *r),
   (commandBuffer, i, l, v, n, r), op_begin(c, OP_CLEAR_DS_IMAGE, 0))
HK(CmdBlitImage, (VkCommandBuffer commandBuffer, VkImage s, VkImageLayout sl, VkImage d, VkImageLayout dl, uint32_t n, const VkImageBlit *r, VkFilter f),
   (commandBuffer, s, sl, d, dl, n, r, f), op_begin(c, OP_BLIT, 0))
HK(CmdBlitImage2, (VkCommandBuffer commandBuffer, const VkBlitImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_BLIT, 0))
HK(CmdBlitImage2KHR, (VkCommandBuffer commandBuffer, const VkBlitImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_BLIT, 0))
HK(CmdResolveImage, (VkCommandBuffer commandBuffer, VkImage s, VkImageLayout sl, VkImage d, VkImageLayout dl, uint32_t n, const VkImageResolve *r),
   (commandBuffer, s, sl, d, dl, n, r), op_begin(c, OP_RESOLVE_IMAGE, 0))
HK(CmdResolveImage2, (VkCommandBuffer commandBuffer, const VkResolveImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_RESOLVE_IMAGE, 0))
HK(CmdResolveImage2KHR, (VkCommandBuffer commandBuffer, const VkResolveImageInfo2 *i),
   (commandBuffer, i), op_begin(c, OP_RESOLVE_IMAGE, 0))
HK(CmdResetQueryPool, (VkCommandBuffer commandBuffer, VkQueryPool p, uint32_t f, uint32_t n),
   (commandBuffer, p, f, n), op_begin(c, OP_QUERY_RESET, 0))
HK(CmdCopyQueryPoolResults, (VkCommandBuffer commandBuffer, VkQueryPool p, uint32_t f, uint32_t n, VkBuffer b, VkDeviceSize o, VkDeviceSize s, VkQueryResultFlags fl),
   (commandBuffer, p, f, n, b, o, s, fl), op_begin(c, OP_QUERY_COPY, 0))

/* Barriers count towards the open pass or operation run and the command
 * buffer.
 */
HK(CmdPipelineBarrier, (VkCommandBuffer commandBuffer, VkPipelineStageFlags s, VkPipelineStageFlags d, VkDependencyFlags f, uint32_t mc, const VkMemoryBarrier *m, uint32_t bc, const VkBufferMemoryBarrier *b, uint32_t ic, const VkImageMemoryBarrier *i),
   (commandBuffer, s, d, f, mc, m, bc, b, ic, i), count_barrier(c))
HK(CmdPipelineBarrier2, (VkCommandBuffer commandBuffer, const VkDependencyInfo *i),
   (commandBuffer, i), count_barrier(c))
HK(CmdPipelineBarrier2KHR, (VkCommandBuffer commandBuffer, const VkDependencyInfo *i),
   (commandBuffer, i), count_barrier(c))

/* ---- render passes */

static const char *
fmt_name(uint32_t f, char *buf, size_t n)
{
   size_t lo = 0, hi = sizeof(fmt_names) / sizeof(fmt_names[0]);
   while (lo < hi) {
      size_t mid = (lo + hi) / 2;
      if (fmt_names[mid].v == f)
         return fmt_names[mid].n;
      if (fmt_names[mid].v < f)
         lo = mid + 1;
      else
         hi = mid;
   }
   snprintf(buf, n, "FORMAT_%u", f);
   return buf;
}

static const char *
load_op_str(VkAttachmentLoadOp op)
{
   switch (op) {
   case VK_ATTACHMENT_LOAD_OP_LOAD: return "load";
   case VK_ATTACHMENT_LOAD_OP_CLEAR: return "clear";
   case VK_ATTACHMENT_LOAD_OP_DONT_CARE: return "dontcare";
   default: return "none";
   }
}

static const char *
store_op_str(VkAttachmentStoreOp op)
{
   switch (op) {
   case VK_ATTACHMENT_STORE_OP_STORE: return "store";
   case VK_ATTACHMENT_STORE_OP_DONT_CARE: return "dontcare";
   default: return "none";
   }
}

/* " <what> <FORMAT> <load>/<store>", empty without an attachment */
static void
describe_att(char *buf, size_t size, const char *what,
             const VkRenderingAttachmentInfo *info, uint32_t *samples)
{
   uint64_t fmt = 0, image = 0;
   buf[0] = 0;
   if (info == NULL || info->imageView == VK_NULL_HANDLE ||
       !map_get(&g_views, (uint64_t)info->imageView, &fmt, &image))
      return;
   uint64_t s = 1;
   if (map_get(&g_images, image, &s, NULL) && s > *samples)
      *samples = (uint32_t)s;
   char tmp[32];
   snprintf(buf, size, " %s %s %s/%s", what,
            fmt_name((uint32_t)fmt, tmp, sizeof(tmp)),
            load_op_str(info->loadOp), store_op_str(info->storeOp));
}

static void
begin_rendering(struct cbs *c, const VkRenderingInfo *info)
{
   if (!timed(c))
      return;
   c->in_pass = true;
   c->pass_suspending = (info->flags & VK_RENDERING_SUSPENDING_BIT) != 0;
   /* A resumed pass continues the one its suspension ended */
   if ((info->flags & VK_RENDERING_RESUMING_BIT) && c->pass_slot >= 0)
      return;
   end_op_run(c);

   uint32_t samples = 1;
   char color[112], depth[112];
   describe_att(color, sizeof(color), "c0",
                info->colorAttachmentCount > 0 ? &info->pColorAttachments[0] : NULL,
                &samples);
   describe_att(depth, sizeof(depth), "z", info->pDepthAttachment, &samples);
   snprintf(c->pass_name, sizeof(c->pass_name),
            "pass %ux%u x%u layers %u, %u rt%s%s",
            info->renderArea.extent.width, info->renderArea.extent.height,
            samples, info->layerCount, info->colorAttachmentCount, color, depth);

   c->pass_resolve = false;
   for (uint32_t i = 0; i < info->colorAttachmentCount; i++) {
      if (info->pColorAttachments[i].resolveMode != VK_RESOLVE_MODE_NONE &&
          info->pColorAttachments[i].resolveImageView != VK_NULL_HANDLE)
         c->pass_resolve = true;
   }
   memset(&c->pass_c, 0, sizeof(c->pass_c));
   c->pass_slot = slot_begin(c);
}

static void
finish_pass(struct cbs *c)
{
   if (c->pass_slot < 0)
      return;
   uint32_t sig = sig_find(c->d->prof, c->pass_name, KIND_PASS, 0);
   struct counts cc = c->pass_c;
   int32_t slot = c->pass_slot;
   c->pass_slot = -1;
   slot_end(c, slot, sig, &cc);
   if (c->pass_resolve) {
      c->pass_resolve = false;
      op_begin(c, OP_RESOLVE_END_OF_PASS, 0);
   }
}

static void
end_rendering(struct cbs *c)
{
   if (!timed(c))
      return;
   c->in_pass = false;
   if (!c->pass_suspending)
      finish_pass(c);
}

#define RENDERING_HOOKS(SUFFIX) \
   static void VKAPI_CALL h_CmdBeginRendering##SUFFIX( \
      VkCommandBuffer commandBuffer, const VkRenderingInfo *info) \
   { \
      struct cbs *c = cbs_get(commandBuffer); \
      struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer)); \
      PFN_vkCmdBeginRendering##SUFFIX next = \
         dv ? (PFN_vkCmdBeginRendering##SUFFIX)dv->nx[H_CmdBeginRendering##SUFFIX] : NULL; \
      if (next == NULL) \
         return; \
      if (c != NULL) \
         begin_rendering(c, info); \
      next(commandBuffer, info); \
   } \
   static void VKAPI_CALL h_CmdEndRendering##SUFFIX(VkCommandBuffer commandBuffer) \
   { \
      struct cbs *c = cbs_get(commandBuffer); \
      struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer)); \
      PFN_vkCmdEndRendering##SUFFIX next = \
         dv ? (PFN_vkCmdEndRendering##SUFFIX)dv->nx[H_CmdEndRendering##SUFFIX] : NULL; \
      if (next == NULL) \
         return; \
      next(commandBuffer); \
      if (c != NULL) \
         end_rendering(c); \
   }

RENDERING_HOOKS()
RENDERING_HOOKS(KHR)

/* ---- debug utils labels (CONDUIT_PASS_PROFILE_LABELS=1) */

static void VKAPI_CALL
h_CmdBeginDebugUtilsLabelEXT(VkCommandBuffer commandBuffer,
                             const VkDebugUtilsLabelEXT *label)
{
   struct cbs *c = cbs_get(commandBuffer);
   struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer));
   PFN_vkCmdBeginDebugUtilsLabelEXT next =
      dv ? (PFN_vkCmdBeginDebugUtilsLabelEXT)dv->nx[H_CmdBeginDebugUtilsLabelEXT] : NULL;
   if (next == NULL)
      return;
   if (timed(c) && dv->labels && c->label_depth < MAX_LABELS) {
      unsigned d = c->label_depth++;
      const char *parent = d > 0 ? c->label[d - 1].path : "";
      snprintf(c->label[d].path, LABEL_LEN, "%s%s%s", parent, d > 0 ? "/" : "",
               label->pLabelName ? label->pLabelName : "");
      /* No timestamp is written inside a render pass */
      c->label[d].slot = c->in_pass ? -1 : slot_begin(c);
   }
   next(commandBuffer, label);
}

static void VKAPI_CALL
h_CmdEndDebugUtilsLabelEXT(VkCommandBuffer commandBuffer)
{
   struct cbs *c = cbs_get(commandBuffer);
   struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer));
   PFN_vkCmdEndDebugUtilsLabelEXT next =
      dv ? (PFN_vkCmdEndDebugUtilsLabelEXT)dv->nx[H_CmdEndDebugUtilsLabelEXT] : NULL;
   if (next == NULL)
      return;
   if (timed(c) && dv->labels && c->label_depth > 0) {
      unsigned d = --c->label_depth;
      if (c->label[d].slot >= 0 && !c->in_pass) {
         char name[LABEL_LEN + 16];
         snprintf(name, sizeof(name), "label: %s", c->label[d].path);
         uint32_t sig = sig_find(dv->prof, name, KIND_LABEL, 0);
         slot_end(c, c->label[d].slot, sig, NULL);
      }
   }
   next(commandBuffer);
}

/* ---- command buffers, pools, images */

static VkResult VKAPI_CALL
h_BeginCommandBuffer(VkCommandBuffer commandBuffer,
                     const VkCommandBufferBeginInfo *info)
{
   struct cbs *c = cbs_get(commandBuffer);
   struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer));
   VkResult r = ((PFN_vkBeginCommandBuffer)dv->nx[H_BeginCommandBuffer])(commandBuffer, info);
   if (c == NULL)
      return r;
   c->cmd_slot = c->pass_slot = c->op_slot = -1;
   c->in_pass = false;
   c->pass_resolve = false;
   c->pass_suspending = false;
   c->label_depth = 0;
   memset(&c->cmd_c, 0, sizeof(c->cmd_c));
   if (r == VK_SUCCESS && timed(c))
      c->cmd_slot = slot_begin(c);
   return r;
}

static VkResult VKAPI_CALL
h_EndCommandBuffer(VkCommandBuffer commandBuffer)
{
   struct cbs *c = cbs_get(commandBuffer);
   struct dev *dv = c ? c->d : dev_by_key(dkey(commandBuffer));
   if (timed(c)) {
      /* A pass left suspended at the end of the command buffer closes here */
      end_op_run(c);
      c->in_pass = false;
      finish_pass(c);
      end_op_run(c);
      c->label_depth = 0;
      if (c->cmd_slot >= 0) {
         uint32_t sig = sig_find(dv->prof,
                                 c->kind == 1 ? "command buffer (all work)"
                                              : "compute-queue command buffer (all work)",
                                 KIND_CMD, 0);
         struct counts cc = c->cmd_c;
         int32_t slot = c->cmd_slot;
         c->cmd_slot = -1;
         slot_end(c, slot, sig, &cc);
      }
   }
   return ((PFN_vkEndCommandBuffer)dv->nx[H_EndCommandBuffer])(commandBuffer);
}

static VkResult VKAPI_CALL
h_AllocateCommandBuffers(VkDevice device, const VkCommandBufferAllocateInfo *info,
                         VkCommandBuffer *cbs)
{
   struct dev *d = dev_by_key(dkey(device));
   VkResult r = ((PFN_vkAllocateCommandBuffers)d->nx[H_AllocateCommandBuffers])(device, info, cbs);
   if (r != VK_SUCCESS || d->prof == NULL)
      return r;
   uint64_t family = 0;
   map_get(&g_pools, (uint64_t)info->commandPool, &family, NULL);
   uint8_t kind = 0;
   if (family < d->family_count && d->family_valid_bits[family] > 0) {
      if (d->family_flags[family] & VK_QUEUE_GRAPHICS_BIT)
         kind = 1;
      else if (d->family_flags[family] & VK_QUEUE_COMPUTE_BIT)
         kind = 2;
   }
   for (uint32_t i = 0; i < info->commandBufferCount; i++) {
      struct cbs *c = calloc(1, sizeof(*c));
      if (c == NULL)
         continue;
      c->h = cbs[i];
      c->d = d;
      c->pool = info->commandPool;
      c->primary = info->level == VK_COMMAND_BUFFER_LEVEL_PRIMARY;
      c->kind = kind;
      c->cmd_slot = c->pass_slot = c->op_slot = -1;
      uint64_t old;
      if (map_del(&g_cbs, (uint64_t)(uintptr_t)cbs[i], &old))
         free((void *)(uintptr_t)old);
      map_put(&g_cbs, (uint64_t)(uintptr_t)cbs[i], (uint64_t)(uintptr_t)c, 0);
   }
   return r;
}

static void VKAPI_CALL
h_FreeCommandBuffers(VkDevice device, VkCommandPool pool, uint32_t n,
                     const VkCommandBuffer *cbs)
{
   struct dev *d = dev_by_key(dkey(device));
   for (uint32_t i = 0; i < n; i++) {
      uint64_t old;
      if (cbs[i] && map_del(&g_cbs, (uint64_t)(uintptr_t)cbs[i], &old))
         free((void *)(uintptr_t)old);
   }
   ((PFN_vkFreeCommandBuffers)d->nx[H_FreeCommandBuffers])(device, pool, n, cbs);
}

static VkResult VKAPI_CALL
h_CreateCommandPool(VkDevice device, const VkCommandPoolCreateInfo *info,
                    const VkAllocationCallbacks *alloc, VkCommandPool *pool)
{
   struct dev *d = dev_by_key(dkey(device));
   VkResult r = ((PFN_vkCreateCommandPool)d->nx[H_CreateCommandPool])(device, info, alloc, pool);
   if (r == VK_SUCCESS)
      map_put(&g_pools, (uint64_t)*pool, info->queueFamilyIndex, 0);
   return r;
}

static void VKAPI_CALL
h_DestroyCommandPool(VkDevice device, VkCommandPool pool,
                     const VkAllocationCallbacks *alloc)
{
   struct dev *d = dev_by_key(dkey(device));
   map_del(&g_pools, (uint64_t)pool, NULL);
   /* Its command buffers die with it; their states are dropped when a later
    * allocation reuses the handles.
    */
   ((PFN_vkDestroyCommandPool)d->nx[H_DestroyCommandPool])(device, pool, alloc);
}

static VkResult VKAPI_CALL
h_CreateImage(VkDevice device, const VkImageCreateInfo *info,
              const VkAllocationCallbacks *alloc, VkImage *image)
{
   struct dev *d = dev_by_key(dkey(device));
   VkResult r = ((PFN_vkCreateImage)d->nx[H_CreateImage])(device, info, alloc, image);
   if (r == VK_SUCCESS)
      map_put(&g_images, (uint64_t)*image, info->samples, 0);
   return r;
}

static void VKAPI_CALL
h_DestroyImage(VkDevice device, VkImage image, const VkAllocationCallbacks *alloc)
{
   struct dev *d = dev_by_key(dkey(device));
   map_del(&g_images, (uint64_t)image, NULL);
   ((PFN_vkDestroyImage)d->nx[H_DestroyImage])(device, image, alloc);
}

static VkResult VKAPI_CALL
h_CreateImageView(VkDevice device, const VkImageViewCreateInfo *info,
                  const VkAllocationCallbacks *alloc, VkImageView *view)
{
   struct dev *d = dev_by_key(dkey(device));
   VkResult r = ((PFN_vkCreateImageView)d->nx[H_CreateImageView])(device, info, alloc, view);
   if (r == VK_SUCCESS)
      map_put(&g_views, (uint64_t)*view, info->format, (uint64_t)info->image);
   return r;
}

static void VKAPI_CALL
h_DestroyImageView(VkDevice device, VkImageView view,
                   const VkAllocationCallbacks *alloc)
{
   struct dev *d = dev_by_key(dkey(device));
   map_del(&g_views, (uint64_t)view, NULL);
   ((PFN_vkDestroyImageView)d->nx[H_DestroyImageView])(device, view, alloc);
}

static VkResult VKAPI_CALL
h_QueuePresentKHR(VkQueue queue, const VkPresentInfoKHR *info)
{
   struct dev *d = dev_by_key(dkey(queue));
   if (d->prof)
      atomic_fetch_add(&d->prof->frames, 1);
   return ((PFN_vkQueuePresentKHR)d->nx[H_QueuePresentKHR])(queue, info);
}

/* --------------------------------------------------------- harvest/log */

static int
sig_cmp(const void *a, const void *b)
{
   const struct sig *sa = a, *sb = b;
   return sa->ns < sb->ns ? 1 : sa->ns > sb->ns ? -1 : 0;
}

static void
log_sig(const struct sig *s, double window_s)
{
   const double n = (double)s->count;
   char extra[256] = "";
   int len;
   if (s->kind == KIND_OP)
      len = snprintf(extra, sizeof(extra), " %6.1f ops", s->calls / n);
   else if (s->kind == KIND_LABEL)
      len = snprintf(extra, sizeof(extra), " %6.1f calls", 0.0);
   else
      len = snprintf(extra, sizeof(extra), " %6.1f calls %7.1f draws",
                     s->calls / n, s->draws / n);
   if (s->ind_calls > 0 && len < (int)sizeof(extra))
      len += snprintf(extra + len, sizeof(extra) - len,
                      " (indirect %.1f calls %.1f draws)", s->ind_calls / n,
                      s->ind_draws / n);
   if (s->barriers > 0 && len < (int)sizeof(extra))
      len += snprintf(extra + len, sizeof(extra) - len, " %.1f barriers",
                      s->barriers / n);
   plog("VKL pass profile: %8.2f GPU ms/s %8.1f us each %6" PRIu64 "x%s  %s",
        s->ns / 1e6 / window_s, s->ns / 1e3 / n, s->count, extra, s->name);
}

static void
harvest_and_log(struct prof *p, bool final)
{
   struct dev *d = p->dev;
   pthread_mutex_lock(&p->totals_mutex);
   uint32_t hw = atomic_load(&p->next);
   if (hw > SLOTS)
      hw = SLOTS;
   if (hw > 0) {
      uint64_t *res = malloc((size_t)hw * 4 * sizeof(uint64_t));
      if (res != NULL) {
         /* Two queries per slot, value and availability each */
         VkResult r = d->GetQueryPoolResults(d->h, p->pool, 0, hw * 2,
                                             (size_t)hw * 4 * sizeof(uint64_t),
                                             res, 2 * sizeof(uint64_t),
                                             VK_QUERY_RESULT_64_BIT |
                                             VK_QUERY_RESULT_WITH_AVAILABILITY_BIT);
         if (r == VK_SUCCESS || r == VK_NOT_READY) {
            for (uint32_t i = 0; i < hw; i++) {
               const uint64_t *b = &res[i * 4], *e = &res[i * 4 + 2];
               if (!b[1] || !e[1])
                  continue;
               const uint32_t sig = p->meta[i].sig;
               const uint64_t tb = b[0] & d->ts_mask, te = e[0] & d->ts_mask;
               /* Reset both so that a slot is read once per execution */
               d->ResetQueryPool(d->h, p->pool, 2 * i, 2);
               if (sig == 0)
                  continue;
               const uint64_t ticks = (te - tb) & d->ts_mask;
               struct sig *ps = &p->sigs[sig - 1];
               ps->ns += (uint64_t)((double)ticks * d->period_ns);
               ps->count++;
               ps->calls += p->meta[i].c.calls;
               ps->draws += p->meta[i].c.draws;
               ps->ind_calls += p->meta[i].c.ind_calls;
               ps->ind_draws += p->meta[i].c.ind_draws;
               ps->barriers += p->meta[i].c.barriers;
            }
         }
         free(res);
      }
   }

   const uint64_t now = now_ns();
   const double window_s = (double)(now - p->window_start_ns) / 1e9;
   if (window_s >= p->window_s || (final && window_s > 0)) {
      p->windows++;
      const uint32_t sig_count = atomic_load(&p->sig_count);
      uint32_t active = 0;
      double cmd_ns = 0, pass_ns = 0, op_ns = 0;
      double op_kind_ns[OP_COUNT] = { 0 };
      double op_kind_n[OP_COUNT] = { 0 };
      for (uint32_t i = 0; i < sig_count; i++) {
         const struct sig *s = &p->sigs[i];
         if (s->count == 0)
            continue;
         active++;
         if (s->kind == KIND_CMD)
            cmd_ns += (double)s->ns;
         else if (s->kind == KIND_PASS)
            pass_ns += (double)s->ns;
         else if (s->kind == KIND_OP) {
            op_ns += (double)s->ns;
            op_kind_ns[s->op] += (double)s->ns;
            op_kind_n[s->op] += (double)s->count;
         }
      }
      pthread_mutex_lock(&p->sig_mutex);
      const uint64_t dropped_window = p->dropped_window;
      p->dropped_window = 0;
      const uint64_t dropped = p->dropped;
      pthread_mutex_unlock(&p->sig_mutex);
      const uint64_t frames = atomic_load(&p->frames);
      plog("VKL pass profile: window %" PRIu64 "%s, %.2f s, %u of %u signatures "
           "active, %" PRIu64 " dropped (%" PRIu64 " in all), %" PRIu64
           " frames (device %u)",
           p->windows, final ? " (final)" : "", window_s, active, sig_count,
           dropped_window, dropped, frames - p->frames_window_start, d->index);
      p->frames_window_start = frames;
      const double ms = 1e6 * window_s;
      plog("VKL pass profile: summary GPU ms/s: command buffers %.2f = render "
           "passes %.2f + operations %.2f + between them %.2f (secondaries 0.00)",
           cmd_ns / ms, pass_ns / ms, op_ns / ms,
           cmd_ns > pass_ns + op_ns ? (cmd_ns - pass_ns - op_ns) / ms : 0.0);
      char line[640];
      int len = 0;
      for (unsigned k = 1; k < OP_COUNT; k++) {
         if (op_kind_n[k] == 0 || len >= (int)sizeof(line))
            continue;
         len += snprintf(line + len, sizeof(line) - len, "%s %s %.2f (%.0f/s)",
                         len > 0 ? "," : "", op_names[k], op_kind_ns[k] / ms,
                         op_kind_n[k] / window_s);
      }
      if (len > 0)
         plog("VKL pass profile: operations GPU ms/s:%s", line);

      struct sig *sorted = malloc((size_t)sig_count * sizeof(*sorted));
      if (sorted != NULL) {
         memcpy(sorted, p->sigs, (size_t)sig_count * sizeof(sorted[0]));
         qsort(sorted, sig_count, sizeof(sorted[0]), sig_cmp);
         for (uint32_t i = 0; i < sig_count && i < LOG_TOP; i++) {
            if (sorted[i].count == 0)
               break;
            log_sig(&sorted[i], window_s);
         }
         free(sorted);
      }
      for (uint32_t i = 0; i < sig_count; i++) {
         struct sig *s = &p->sigs[i];
         s->ns = s->count = 0;
         s->calls = s->draws = s->ind_calls = s->ind_draws = s->barriers = 0;
      }
      p->window_start_ns = now;
      plog_flush();
   }
   pthread_mutex_unlock(&p->totals_mutex);
}

static void *
harvest_thread(void *arg)
{
   struct prof *p = arg;
   pthread_mutex_lock(&p->stop_mutex);
   while (!p->stop) {
      struct timespec ts;
      clock_gettime(CLOCK_REALTIME, &ts);
      ts.tv_nsec += HARVEST_MS * 1000000l;
      ts.tv_sec += ts.tv_nsec / 1000000000l;
      ts.tv_nsec %= 1000000000l;
      pthread_cond_timedwait(&p->stop_cond, &p->stop_mutex, &ts);
      if (p->stop)
         break;
      pthread_mutex_unlock(&p->stop_mutex);
      harvest_and_log(p, false);
      pthread_mutex_lock(&p->stop_mutex);
   }
   pthread_mutex_unlock(&p->stop_mutex);
   return NULL;
}

/* ------------------------------------------------------ device and instance */

static struct prof *
prof_create(struct dev *d)
{
   struct prof *p = calloc(1, sizeof(*p));
   if (p == NULL)
      return NULL;
   p->dev = d;
   p->meta = calloc(SLOTS, sizeof(*p->meta));
   p->sigs = calloc(MAX_SIGS, sizeof(*p->sigs));
   p->sig_ht = calloc(SIG_HASH, sizeof(*p->sig_ht));
   if (!p->meta || !p->sigs || !p->sig_ht)
      goto fail;

   VkQueryPoolCreateInfo qi = {
      .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
      .queryType = VK_QUERY_TYPE_TIMESTAMP,
      .queryCount = SLOTS * 2,
   };
   if (d->CreateQueryPool(d->h, &qi, NULL, &p->pool) != VK_SUCCESS)
      goto fail;
   d->ResetQueryPool(d->h, p->pool, 0, SLOTS * 2);

   pthread_mutex_init(&p->sig_mutex, NULL);
   pthread_mutex_init(&p->totals_mutex, NULL);
   pthread_mutex_init(&p->stop_mutex, NULL);
   pthread_cond_init(&p->stop_cond, NULL);
   p->window_start_ns = now_ns();
   const char *w = getenv("CONDUIT_PASS_PROFILE_WINDOW_S");
   p->window_s = w && atof(w) > 0 ? atof(w) : 2.0;
   if (pthread_create(&p->thread, NULL, harvest_thread, p) == 0)
      p->thread_started = true;
   return p;

fail:
   free(p->meta);
   free(p->sigs);
   free(p->sig_ht);
   free(p);
   return NULL;
}

static void VKAPI_CALL
h_DestroyDevice(VkDevice device, const VkAllocationCallbacks *alloc)
{
   struct dev *d = dev_by_key(dkey(device));
   PFN_vkDestroyDevice next = (PFN_vkDestroyDevice)d->nx[H_DestroyDevice];
   if (d->prof) {
      struct prof *p = d->prof;
      if (p->thread_started) {
         pthread_mutex_lock(&p->stop_mutex);
         p->stop = true;
         pthread_cond_signal(&p->stop_cond);
         pthread_mutex_unlock(&p->stop_mutex);
         pthread_join(p->thread, NULL);
      }
      /* The application idled the device before destroying it */
      harvest_and_log(p, true);
      d->DestroyQueryPool(d->h, p->pool, NULL);
      d->prof = NULL;
   }
   pthread_mutex_lock(&g_lock);
   for (struct dev **pp = &g_devs; *pp; pp = &(*pp)->next) {
      if (*pp == d) {
         *pp = d->next;
         break;
      }
   }
   pthread_mutex_unlock(&g_lock);
   next(device, alloc);
}

static const PFN_vkVoidFunction hook_fns[H_COUNT] = {
#define X(n) (PFN_vkVoidFunction)h_##n,
   HOOKS(X)
#undef X
};

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
layer_GetDeviceProcAddr(VkDevice device, const char *name)
{
   struct dev *d = dev_by_key(dkey(device));
   if (d == NULL)
      return NULL;
   if (strcmp(name, "vkGetDeviceProcAddr") == 0)
      return (PFN_vkVoidFunction)layer_GetDeviceProcAddr;
   for (int i = 0; i < H_COUNT; i++) {
      if (strcmp(hook_names[i], name) != 0)
         continue;
      /* Only what the layers below provide; and, for a device that is not
       * profiled, nothing but the destruction (to drop the device).
       */
      if (d->nx[i] == NULL)
         return NULL;
      if (d->prof == NULL && i != H_DestroyDevice)
         break;
      return hook_fns[i];
   }
   return d->gdpa(device, name);
}

static const VkLayerDeviceCreateInfo *
find_device_link(const VkDeviceCreateInfo *ci)
{
   const VkLayerDeviceCreateInfo *l = (const VkLayerDeviceCreateInfo *)ci->pNext;
   while (l && !(l->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO &&
                 l->function == VK_LAYER_LINK_INFO))
      l = (const VkLayerDeviceCreateInfo *)l->pNext;
   return l;
}

static VKAPI_ATTR VkResult VKAPI_CALL
layer_CreateDevice(VkPhysicalDevice pd, const VkDeviceCreateInfo *ci,
                   const VkAllocationCallbacks *alloc, VkDevice *pDevice)
{
   struct inst *in = inst_by_key(dkey(pd));
   VkLayerDeviceCreateInfo *link = (VkLayerDeviceCreateInfo *)find_device_link(ci);
   if (in == NULL || link == NULL)
      return VK_ERROR_INITIALIZATION_FAILED;
   PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
   PFN_vkGetDeviceProcAddr gdpa = link->u.pLayerInfo->pfnNextGetDeviceProcAddr;
   link->u.pLayerInfo = link->u.pLayerInfo->pNext;

   VkPhysicalDeviceProperties props;
   in->GetPDProps(pd, &props);
   const bool v12 = props.apiVersion >= VK_API_VERSION_1_2;

   /* hostQueryReset on: set it where the application already has the
    * feature structure, else chain one in front.
    */
   VkDeviceCreateInfo ci2 = *ci;
   VkPhysicalDeviceHostQueryResetFeatures hqr = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_HOST_QUERY_RESET_FEATURES,
      .hostQueryReset = VK_TRUE,
   };
   bool have = false;
   for (VkBaseOutStructure *s = (VkBaseOutStructure *)ci->pNext; s; s = s->pNext) {
      if (s->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES) {
         ((VkPhysicalDeviceVulkan12Features *)s)->hostQueryReset = VK_TRUE;
         have = true;
      } else if (s->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_HOST_QUERY_RESET_FEATURES) {
         ((VkPhysicalDeviceHostQueryResetFeatures *)s)->hostQueryReset = VK_TRUE;
         have = true;
      }
   }
   if (!have && v12) {
      hqr.pNext = (void *)ci->pNext;
      ci2.pNext = &hqr;
   }

   PFN_vkCreateDevice create = (PFN_vkCreateDevice)gipa(in->h, "vkCreateDevice");
   VkResult r = create(pd, &ci2, alloc, pDevice);
   if (r != VK_SUCCESS)
      return r;

   pthread_once(&g_once, global_init);
   struct dev *d = calloc(1, sizeof(*d));
   d->key = dkey(*pDevice);
   d->h = *pDevice;
   d->inst = in;
   d->gdpa = gdpa;
   for (int i = 0; i < H_COUNT; i++)
      d->nx[i] = gdpa(*pDevice, hook_names[i]);
   d->CreateQueryPool = (PFN_vkCreateQueryPool)gdpa(*pDevice, "vkCreateQueryPool");
   d->DestroyQueryPool = (PFN_vkDestroyQueryPool)gdpa(*pDevice, "vkDestroyQueryPool");
   d->GetQueryPoolResults = (PFN_vkGetQueryPoolResults)gdpa(*pDevice, "vkGetQueryPoolResults");
   d->ResetQueryPool = (PFN_vkResetQueryPool)gdpa(*pDevice, "vkResetQueryPool");
   if (d->ResetQueryPool == NULL)
      d->ResetQueryPool = (PFN_vkResetQueryPool)gdpa(*pDevice, "vkResetQueryPoolEXT");
   d->CmdWriteTimestamp = (PFN_vkCmdWriteTimestamp)gdpa(*pDevice, "vkCmdWriteTimestamp");
   d->period_ns = props.limits.timestampPeriod;
   const char *lab = getenv("CONDUIT_PASS_PROFILE_LABELS");
   d->labels = lab != NULL && lab[0] == '1';

   uint32_t n = 0;
   in->GetQFP(pd, &n, NULL);
   VkQueueFamilyProperties *qf = calloc(n ? n : 1, sizeof(*qf));
   in->GetQFP(pd, &n, qf);
   d->family_count = n;
   d->family_flags = calloc(n ? n : 1, sizeof(VkQueueFlags));
   d->family_valid_bits = calloc(n ? n : 1, sizeof(uint32_t));
   d->ts_mask = ~0ull;
   for (uint32_t i = 0; i < n; i++) {
      d->family_flags[i] = qf[i].queueFlags;
      d->family_valid_bits[i] = qf[i].timestampValidBits;
      if ((qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) &&
          qf[i].timestampValidBits > 0 && qf[i].timestampValidBits < 64)
         d->ts_mask = (1ull << qf[i].timestampValidBits) - 1;
   }
   free(qf);

   pthread_mutex_lock(&g_lock);
   d->index = g_dev_count++;
   d->next = g_devs;
   g_devs = d;
   pthread_mutex_unlock(&g_lock);

   const bool ok = v12 && d->CreateQueryPool && d->ResetQueryPool &&
                   d->GetQueryPoolResults && d->CmdWriteTimestamp &&
                   props.limits.timestampPeriod > 0;
   if (ok)
      d->prof = prof_create(d);
   plog("VKL pass profile: device %u %s (api %u.%u.%u, %.3f ns/tick): %s",
        d->index, props.deviceName, VK_API_VERSION_MAJOR(props.apiVersion),
        VK_API_VERSION_MINOR(props.apiVersion),
        VK_API_VERSION_PATCH(props.apiVersion), d->period_ns,
        d->prof ? "profiling on"
                : "profiling off (needs Vulkan 1.2 with timestamps)");
   plog_flush();
   return VK_SUCCESS;
}

static VKAPI_ATTR VkResult VKAPI_CALL
layer_CreateInstance(const VkInstanceCreateInfo *ci,
                     const VkAllocationCallbacks *alloc, VkInstance *pInstance)
{
   VkLayerInstanceCreateInfo *link = (VkLayerInstanceCreateInfo *)ci->pNext;
   while (link && !(link->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO &&
                    link->function == VK_LAYER_LINK_INFO))
      link = (VkLayerInstanceCreateInfo *)link->pNext;
   if (link == NULL)
      return VK_ERROR_INITIALIZATION_FAILED;
   PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
   link->u.pLayerInfo = link->u.pLayerInfo->pNext;

   PFN_vkCreateInstance create = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
   VkResult r = create(ci, alloc, pInstance);
   if (r != VK_SUCCESS)
      return r;

   struct inst *in = calloc(1, sizeof(*in));
   in->key = dkey(*pInstance);
   in->h = *pInstance;
   in->gipa = gipa;
   in->DestroyInstance = (PFN_vkDestroyInstance)gipa(*pInstance, "vkDestroyInstance");
   in->GetPDProps = (PFN_vkGetPhysicalDeviceProperties)gipa(*pInstance, "vkGetPhysicalDeviceProperties");
   in->GetQFP = (PFN_vkGetPhysicalDeviceQueueFamilyProperties)gipa(*pInstance, "vkGetPhysicalDeviceQueueFamilyProperties");
   pthread_mutex_lock(&g_lock);
   in->next = g_insts;
   g_insts = in;
   pthread_mutex_unlock(&g_lock);
   return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL
layer_DestroyInstance(VkInstance instance, const VkAllocationCallbacks *alloc)
{
   struct inst *in = inst_by_key(dkey(instance));
   PFN_vkDestroyInstance next = in->DestroyInstance;
   pthread_mutex_lock(&g_lock);
   for (struct inst **pp = &g_insts; *pp; pp = &(*pp)->next) {
      if (*pp == in) {
         *pp = in->next;
         break;
      }
   }
   pthread_mutex_unlock(&g_lock);
   next(instance, alloc);
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
layer_GetInstanceProcAddr(VkInstance instance, const char *name)
{
   if (strcmp(name, "vkGetInstanceProcAddr") == 0)
      return (PFN_vkVoidFunction)layer_GetInstanceProcAddr;
   if (strcmp(name, "vkCreateInstance") == 0)
      return (PFN_vkVoidFunction)layer_CreateInstance;
   if (strcmp(name, "vkDestroyInstance") == 0)
      return (PFN_vkVoidFunction)layer_DestroyInstance;
   if (strcmp(name, "vkCreateDevice") == 0)
      return (PFN_vkVoidFunction)layer_CreateDevice;
   if (strcmp(name, "vkGetDeviceProcAddr") == 0)
      return (PFN_vkVoidFunction)layer_GetDeviceProcAddr;
   struct inst *in = instance ? inst_by_key(dkey(instance)) : NULL;
   if (in == NULL)
      return NULL;
   return in->gipa(instance, name);
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
vkGetInstanceProcAddr(VkInstance instance, const char *name)
{
   return layer_GetInstanceProcAddr(instance, name);
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
vkGetDeviceProcAddr(VkDevice device, const char *name)
{
   return layer_GetDeviceProcAddr(device, name);
}

EXPORT VKAPI_ATTR VkResult VKAPI_CALL
vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *iface)
{
   if (iface->sType != LAYER_NEGOTIATE_INTERFACE_STRUCT)
      return VK_ERROR_INITIALIZATION_FAILED;
   if (iface->loaderLayerInterfaceVersion > 2)
      iface->loaderLayerInterfaceVersion = 2;
   iface->pfnGetInstanceProcAddr = layer_GetInstanceProcAddr;
   iface->pfnGetDeviceProcAddr = layer_GetDeviceProcAddr;
   iface->pfnGetPhysicalDeviceProcAddr = NULL;
   return VK_SUCCESS;
}

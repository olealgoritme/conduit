// Fence wake latency on the host Vulkan driver: how long after the GPU has
// finished a copy does a waiting CPU thread learn it, per wait method.
// A spinning thread watches a marker the GPU writes to host memory right after
// the copy; the waiter's return time minus the marker's is the wake latency.
// docs/research/host-roundtrip-latency.md, "Phase 3".
//
//   cc -O2 -o fencewake host/latency/fencewake.c -lvulkan -lpthread
//   ./fencewake MODE [QUEUE_FAMILY [ITERATIONS [WAITER_CPU [SPINNER_CPU]]]]
//
// MODE: wait (vkWaitForFences), spin (vkGetFenceStatus loop), syncfd (the
// fence exported as a sync file, poll), timeline (vkWaitSemaphores), hybrid
// (sleep to just before the expected completion, then poll, then wait: what
// patches/0004-vkr-queue-fence-spin.patch does). The copy is 1600x900x4
// bytes from device memory into host memory, the size of the KMD's copy.
#define _GNU_SOURCE
#include <vulkan/vulkan.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <poll.h>
#include <unistd.h>
#include <stdatomic.h>
#include <sys/prctl.h>

#define CK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s:%d %s = %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)
static uint64_t now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000000000ull + t.tv_nsec; }
static VkDevice dev; static VkPhysicalDevice pd;
static uint32_t memtype(uint32_t bits, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
  VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want && !(mp.memoryTypes[i].propertyFlags & avoid)) return i;
  fprintf(stderr, "no memory type\n"); exit(1);
}
static VkBuffer mkbuf(VkDeviceSize sz, VkBufferUsageFlags u, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid, VkDeviceMemory *m, void **map) {
  VkBufferCreateInfo bi = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, 0, 0, sz, u, VK_SHARING_MODE_EXCLUSIVE };
  VkBuffer b; CK(vkCreateBuffer(dev, &bi, 0, &b));
  VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, b, &mr);
  VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, 0, mr.size, memtype(mr.memoryTypeBits, want, avoid) };
  CK(vkAllocateMemory(dev, &ai, 0, m)); CK(vkBindBufferMemory(dev, b, *m, 0));
  if (map) CK(vkMapMemory(dev, *m, 0, VK_WHOLE_SIZE, 0, map));
  return b;
}
static volatile uint32_t *marker; static _Atomic uint32_t want_val; static _Atomic uint64_t marker_ns; static _Atomic int stop;
static int spin_cpu = -1;
static void *spinner(void *a) {
  (void)a;
  if (spin_cpu >= 0) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(spin_cpu, &s); pthread_setaffinity_np(pthread_self(), sizeof s, &s); }
  uint32_t last = 0;
  while (!atomic_load(&stop)) {
    uint32_t w = atomic_load(&want_val);
    if (w != last && *marker == w) { atomic_store(&marker_ns, now()); last = w; }
  }
  return 0;
}
static int cmp(const void *a, const void *b) { double x = *(double *)a, y = *(double *)b; return x < y ? -1 : x > y; }
static void stats(const char *n, double *v, int k) {
  qsort(v, k, sizeof *v, cmp);
  double s = 0; for (int i = 0; i < k; i++) s += v[i];
  printf("  %-30s mean %7.1f p50 %7.1f p90 %7.1f p99 %7.1f us\n", n, s / k, v[k / 2], v[k * 9 / 10], v[k * 99 / 100 < k ? k * 99 / 100 : k - 1]);
}
int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "wait";
  int qf = argc > 2 ? atoi(argv[2]) : 0;
  int iters = argc > 3 ? atoi(argv[3]) : 300;
  int waiter_cpu = argc > 4 ? atoi(argv[4]) : -1;
  spin_cpu = argc > 5 ? atoi(argv[5]) : -1;
  VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, "fencewake", 1, 0, 0, VK_API_VERSION_1_2 };
  VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, 0, &app };
  VkInstance inst; CK(vkCreateInstance(&ici, 0, &inst));
  uint32_t n = 8; VkPhysicalDevice pds[8]; CK(vkEnumeratePhysicalDevices(inst, &n, pds));
  for (uint32_t i = 0; i < n; i++) { VkPhysicalDeviceProperties p; vkGetPhysicalDeviceProperties(pds[i], &p); if (p.vendorID == 0x10de) { pd = pds[i]; break; } }
  if (!pd) { fprintf(stderr, "no NVIDIA device\n"); return 1; }
  VkPhysicalDeviceProperties props; vkGetPhysicalDeviceProperties(pd, &props);
  float prio = 1;
  VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, 0, 0, qf, 1, &prio };
  const char *exts[] = { "VK_KHR_external_fence_fd", "VK_KHR_external_semaphore_fd" };
  VkPhysicalDeviceVulkan12Features f12 = { VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES }; f12.timelineSemaphore = 1;
  VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &f12, 0, 1, &qci, 0, 0, 2, exts };
  CK(vkCreateDevice(pd, &dci, 0, &dev));
  VkQueue q; vkGetDeviceQueue(dev, qf, 0, &q);
  PFN_vkGetFenceFdKHR getFenceFd = (PFN_vkGetFenceFdKHR)vkGetDeviceProcAddr(dev, "vkGetFenceFdKHR");
  const VkDeviceSize SZ = 1600 * 900 * 4;
  VkDeviceMemory ms, md, mm; void *mapm;
  VkBuffer src = mkbuf(SZ, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, 0, &ms, 0);
  VkBuffer dst = mkbuf(SZ, VK_BUFFER_USAGE_TRANSFER_DST_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, &md, 0);
  VkBuffer mk = mkbuf(256, VK_BUFFER_USAGE_TRANSFER_DST_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, &mm, &mapm);
  marker = mapm; *marker = 0;
  VkQueryPoolCreateInfo qpi = { VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, 0, 0, VK_QUERY_TYPE_TIMESTAMP, 2 };
  VkQueryPool qp; CK(vkCreateQueryPool(dev, &qpi, 0, &qp));
  VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, 0, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, qf };
  VkCommandPool cp; CK(vkCreateCommandPool(dev, &cpi, 0, &cp));
  VkCommandBufferAllocateInfo cai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, 0, cp, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1 };
  VkCommandBuffer cb; CK(vkAllocateCommandBuffers(dev, &cai, &cb));
  VkExportFenceCreateInfo efi = { VK_STRUCTURE_TYPE_EXPORT_FENCE_CREATE_INFO, 0, VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT };
  VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, strcmp(mode, "syncfd") ? 0 : &efi };
  VkFence fence; CK(vkCreateFence(dev, &fci, 0, &fence));
  VkSemaphoreTypeCreateInfo sti = { VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO, 0, VK_SEMAPHORE_TYPE_TIMELINE, 0 };
  VkSemaphoreCreateInfo sci = { VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, &sti };
  VkSemaphore tl; CK(vkCreateSemaphore(dev, &sci, 0, &tl));
  if (waiter_cpu >= 0) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(waiter_cpu, &s); pthread_setaffinity_np(pthread_self(), sizeof s, &s); }
  pthread_t th; pthread_create(&th, 0, spinner, 0);
  double *gpu = calloc(iters, 8), *sub2mk = calloc(iters, 8), *wake = calloc(iters, 8);
  int k = 0; uint64_t hist[32]; int nh = 0; uint64_t spun = 0; int misses = 0;
  prctl(PR_SET_TIMERSLACK, 1);
  for (int i = 1; i <= iters + 10; i++) {
    CK(vkResetCommandBuffer(cb, 0));
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, 0, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    CK(vkBeginCommandBuffer(cb, &bi));
    vkCmdResetQueryPool(cb, qp, 0, 2);
    vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qp, 0);
    VkBufferCopy rg = { 0, 0, SZ }; vkCmdCopyBuffer(cb, src, dst, 1, &rg);
    vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, qp, 1);
    VkMemoryBarrier mb = { VK_STRUCTURE_TYPE_MEMORY_BARRIER, 0, VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_TRANSFER_WRITE_BIT };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, 0, 0, 0);
    vkCmdFillBuffer(cb, mk, 0, 4, (uint32_t)i);
    CK(vkEndCommandBuffer(cb));
    atomic_store(&want_val, (uint32_t)i); atomic_store(&marker_ns, 0);
    uint64_t tlv = i;
    VkTimelineSemaphoreSubmitInfo tsi = { VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO, 0, 0, 0, 1, &tlv };
    int tlmode = !strcmp(mode, "timeline");
    VkSubmitInfo si = { VK_STRUCTURE_TYPE_SUBMIT_INFO, tlmode ? &tsi : 0, 0, 0, 0, 1, &cb, tlmode ? 1 : 0, tlmode ? &tl : 0 };
    uint64_t t0 = now();
    CK(vkQueueSubmit(q, 1, &si, tlmode ? VK_NULL_HANDLE : fence));
    if (!strcmp(mode, "wait")) CK(vkWaitForFences(dev, 1, &fence, 1, 3000000000ull));
    else if (!strcmp(mode, "spin")) { while (vkGetFenceStatus(dev, fence) == VK_NOT_READY) ; }
    else if (!strcmp(mode, "hybrid")) {
      // Sleep to just before the predicted completion, then spin a bounded
      // budget, then block. Prediction: the 10th percentile of the last 32
      // submit-to-done times, minus a margin.
      uint64_t pred = 0;
      if (nh >= 8) { uint64_t tmp[32]; int c = nh < 32 ? nh : 32; memcpy(tmp, hist, c * 8);
        for (int a = 0; a < c; a++) for (int b = a + 1; b < c; b++) if (tmp[b] < tmp[a]) { uint64_t x = tmp[a]; tmp[a] = tmp[b]; tmp[b] = x; }
        pred = tmp[c / 10]; }
      uint64_t margin = 30000, budget = 200000;
      if (pred > margin) { struct timespec ts = { 0, 0 }; uint64_t at = t0 + pred - margin; ts.tv_sec = at / 1000000000ull; ts.tv_nsec = at % 1000000000ull; clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &ts, 0); }
      uint64_t s0 = now(); int got = 0;
      while (now() - s0 < budget) if (vkGetFenceStatus(dev, fence) == VK_SUCCESS) { got = 1; break; }
      spun += now() - s0;
      if (!got) { CK(vkWaitForFences(dev, 1, &fence, 1, 3000000000ull)); misses++; }
      hist[nh++ % 32] = now() - t0;
    }
    else if (!strcmp(mode, "syncfd")) {
      VkFenceGetFdInfoKHR gi = { VK_STRUCTURE_TYPE_FENCE_GET_FD_INFO_KHR, 0, fence, VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT };
      int fd = -1; CK(getFenceFd(dev, &gi, &fd));
      if (fd >= 0) { struct pollfd p = { fd, POLLIN }; poll(&p, 1, 3000); close(fd); }
    } else if (tlmode) {
      VkSemaphoreWaitInfo wi = { VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO, 0, 0, 1, &tl, &tlv };
      CK(vkWaitSemaphores(dev, &wi, 3000000000ull));
    } else { fprintf(stderr, "mode: wait|spin|syncfd|timeline\n"); return 2; }
    uint64_t t1 = now();
    uint64_t tm = 0; for (int s = 0; s < 1000000 && !(tm = atomic_load(&marker_ns)); s++) ;
    if (!tlmode) CK(vkResetFences(dev, 1, &fence));
    uint64_t ts[2]; CK(vkGetQueryPoolResults(dev, qp, 0, 2, sizeof ts, ts, 8, VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT));
    if (i <= 10 || !tm) continue;
    gpu[k] = (ts[1] - ts[0]) * props.limits.timestampPeriod / 1000.0;
    sub2mk[k] = (double)(tm - t0) / 1000.0;
    wake[k] = ((double)t1 - (double)tm) / 1000.0;
    k++;
    usleep(2000);
  }
  atomic_store(&stop, 1); pthread_join(th, 0);
  printf("mode %s family %d, %d samples (waiter cpu %d, spinner cpu %d)\n", mode, qf, k, waiter_cpu, spin_cpu);
  stats("GPU copy (timestamps)", gpu, k);
  stats("submit -> GPU done (marker)", sub2mk, k);
  stats("GPU done -> waiter returns", wake, k);
  if (!strcmp(mode, "hybrid")) printf("  spin %.1f us per fence, %d of %d missed the spin budget\n", spun / 1000.0 / (iters + 10), misses, iters + 10);
  return 0;
}

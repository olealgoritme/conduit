/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_fence.h — explicit sync: nvidia-drm's semaphore-surface fences across
 * the boundary. docs/SYNC.md has the whole picture; in short:
 *
 *   SEMSURF_FENCE_CTX_CREATE   forwarded (nested, like the GEM imports); the
 *                              host's fence context gets a GEM proxy here
 *   SEMSURF_FENCE_CREATE       forwarded; the backend keeps the host's
 *                              sync_file and answers with a handle for it.
 *                              A guest dma_fence stands in for it, handed to
 *                              userspace as a guest sync_file, and signals
 *                              when the backend's EventReady for the handle
 *                              arrives -- which is when the host fence did.
 *   SEMSURF_FENCE_WAIT         a guest fence the GPU must wait on. One of
 *                              ours goes across as its handle (the host waits
 *                              on the real fence, GPU-side); anything else is
 *                              waited for here and then forwarded as "already
 *                              signalled", which the host turns into the same
 *                              semaphore release.
 *   SEMSURF_FENCE_ATTACH       answered here: a host fence as above, put on
 *                              the proxy's reservation object, where guest
 *                              importers of the dma-buf find it.
 *
 * The DRM core's syncobj ioctls are all local: they hold dma_fences, and
 * these are dma_fences.
 *
 * Only with NVGPU_CFG_DRM_FENCES from the backend and supports_semsurf from
 * the host's node; otherwise GET_DEV_INFO says no and none of this is asked.
 *
 * Lifetimes. A fence holds nothing of the module's once signalled (no
 * .release, no .wait), so on kernels that detach signalled fences from their
 * ops a guest sync_file may outlive the module. The state below is
 * refcounted: work items and deferred waits outlive the device, and then do
 * nothing.
 */

#include <linux/dma-fence.h>
#include <linux/dma-fence-unwrap.h>
#include <linux/dma-resv.h>
#include <linux/kfifo.h>
#include <linux/kref.h>
#include <linux/sync_file.h>
#include <linux/workqueue.h>

/* The backend's cap on live host fences per guest (device/src/nvidia/
 * fence.rs MAX_FENCES); every one is closed exactly once, so a queue this
 * deep never fills. */
#define NVGPU_FENCE_CLOSE_DEPTH 4096
/* Signals that arrived between the host answering FENCE_CREATE and this
 * side registering the fence (a fence already done when it was made). */
#define NVGPU_FENCE_EARLY 64

#define DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE                                  \
  _IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + DRM_NVIDIA_SEMSURF_FENCE_CREATE,    \
        struct nvgpu_semsurf_fence_create)
#define DRM_IOCTL_NVIDIA_SEMSURF_FENCE_WAIT                                    \
  _IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + DRM_NVIDIA_SEMSURF_FENCE_WAIT,       \
       struct nvgpu_semsurf_fence_wait)

/* nv_drm_common_ioctl.h, drm_nvidia_semsurf_fence_create_params. On the
 * wire, `fd` carries the backend's handle on the way back. */
struct nvgpu_semsurf_fence_create {
  __u32 fence_context_handle;
  __u32 timeout_value_ms;
  __u64 wait_value;
  __s32 fd;
  __u32 __pad;
};

/* drm_nvidia_semsurf_fence_wait_params. On the wire, `fd` is one of our
 * fence handles, or 0 for a fence that has already signalled. */
struct nvgpu_semsurf_fence_wait {
  __u32 fence_context_handle;
  __s32 fd;
  __u64 pre_wait_value;
  __u64 post_wait_value;
};

/* drm_nvidia_semsurf_fence_attach_params. Never forwarded. */
struct nvgpu_semsurf_fence_attach {
  __u32 handle;
  __u32 fence_context_handle;
  __u32 timeout_value_ms;
  __u32 shared;
  __u64 wait_value;
};

struct nvgpu_fence_dom {
  struct kref ref;
  /* Which domain a fence came from, compared rather than the pointer: a
   * fence can outlive this, and a new one may land at the same address. */
  u64 id;
  /* Held across every message sent from a work item; NULL once removed. */
  struct mutex dev_lock;
  struct nvgpu_device *dev;
  /* pending, early, closeq. Taken from the event-queue interrupt. */
  spinlock_t lock;
  struct list_head pending; /* nvgpu_fence the host has not signalled */
  struct {
    u32 handle;
    s32 status;
  } early[NVGPU_FENCE_EARLY];
  unsigned int early_next;
  /* Handles of signalled fences, for close_work to give back. */
  STRUCT_KFIFO(u32, NVGPU_FENCE_CLOSE_DEPTH) closeq;
  struct work_struct close_work;
};

struct nvgpu_fence {
  struct dma_fence base; /* first: freed by dma_fence_free() */
  spinlock_t lock;
  u64 dom_id;
  u32 handle;            /* the backend's, naming the host sync_file */
  struct list_head node; /* dom->pending, until the host signals */
};

static const char *nvgpu_fence_driver_name(struct dma_fence *f) {
  return "conduit-gpu";
}

static const char *nvgpu_fence_timeline_name(struct dma_fence *f) {
  return "conduit.semaphore_surface";
}

/* No .release and no .wait: see the lifetimes note above. */
static const struct dma_fence_ops nvgpu_fence_ops = {
    .get_driver_name = nvgpu_fence_driver_name,
    .get_timeline_name = nvgpu_fence_timeline_name,
};

static atomic64_t nvgpu_fence_dom_ids = ATOMIC64_INIT(0);

static void nvgpu_fence_dom_free(struct kref *ref) {
  kvfree(container_of(ref, struct nvgpu_fence_dom, ref));
}

/* The handle of a signalled fence goes back, from process context. */
static void nvgpu_fence_close_work(struct work_struct *w) {
  struct nvgpu_fence_dom *dom =
      container_of(w, struct nvgpu_fence_dom, close_work);
  struct nvgpu_msg_hdr req, resp;
  unsigned long flags;
  u32 handle;

  mutex_lock(&dom->dev_lock);
  for (;;) {
    int got;

    spin_lock_irqsave(&dom->lock, flags);
    got = kfifo_get(&dom->closeq, &handle);
    spin_unlock_irqrestore(&dom->lock, flags);
    if (!got)
      break;
    if (!dom->dev)
      continue;
    memset(&req, 0, sizeof(req));
    req.msg_type = cpu_to_le32(NVGPU_MSG_CLOSE);
    req.handle = cpu_to_le32(handle);
    nvgpu_send_recv(dom->dev, &req, sizeof(req), &resp, sizeof(resp));
  }
  mutex_unlock(&dom->dev_lock);
}

static int nvgpu_fence_dom_init(struct nvgpu_device *dev) {
  struct nvgpu_fence_dom *dom = kvzalloc(sizeof(*dom), GFP_KERNEL);

  if (!dom)
    return -ENOMEM;
  kref_init(&dom->ref);
  dom->id = (u64)atomic64_inc_return(&nvgpu_fence_dom_ids);
  mutex_init(&dom->dev_lock);
  dom->dev = dev;
  spin_lock_init(&dom->lock);
  INIT_LIST_HEAD(&dom->pending);
  INIT_KFIFO(dom->closeq);
  INIT_WORK(&dom->close_work, nvgpu_fence_close_work);
  dev->fences = dom;
  return 0;
}

/* Signal `f` (off the pending list already) and queue its handle to close. */
static void nvgpu_fence_finish(struct nvgpu_fence_dom *dom,
                               struct nvgpu_fence *f, s32 status) {
  unsigned long flags;
  bool queued;

  if (status < 0)
    dma_fence_set_error(&f->base, status);
  dma_fence_signal(&f->base);

  spin_lock_irqsave(&dom->lock, flags);
  queued = kfifo_put(&dom->closeq, f->handle);
  spin_unlock_irqrestore(&dom->lock, flags);
  if (queued)
    schedule_work(&dom->close_work);
  dma_fence_put(&f->base); /* the pending list's */
}

/*
 * The device is going: nothing will signal from now on, so everything still
 * pending signals now, with -ENODEV, and work items find no device to talk to.
 * Before the virtqueues go, so a work item mid-message finishes its message.
 */
static void nvgpu_fence_dom_kill(struct nvgpu_device *dev) {
  struct nvgpu_fence_dom *dom = dev->fences;
  struct nvgpu_fence *f, *tmp;
  unsigned long flags;
  LIST_HEAD(dead);

  if (!dom)
    return;
  mutex_lock(&dom->dev_lock);
  dom->dev = NULL;
  mutex_unlock(&dom->dev_lock);

  spin_lock_irqsave(&dom->lock, flags);
  list_splice_init(&dom->pending, &dead);
  spin_unlock_irqrestore(&dom->lock, flags);
  list_for_each_entry_safe(f, tmp, &dead, node) {
    list_del_init(&f->node);
    nvgpu_fence_finish(dom, f, -ENODEV);
  }
  cancel_work_sync(&dom->close_work);
  dev->fences = NULL;
  kref_put(&dom->ref, nvgpu_fence_dom_free);
}

/*
 * EventReady for a handle no open file has: a host fence signalled, with
 * `status` its error if it has one (-ETIMEDOUT after nvidia-drm's 5 s).
 * Interrupt context. Returns whether the handle was a fence of ours.
 */
static bool nvgpu_fence_host_signalled(struct nvgpu_device *dev, u32 handle,
                                       s32 status) {
  struct nvgpu_fence_dom *dom = dev->fences;
  struct nvgpu_fence *f, *found = NULL;
  unsigned long flags;

  if (!dom)
    return false;
  spin_lock_irqsave(&dom->lock, flags);
  list_for_each_entry(f, &dom->pending, node) {
    if (f->handle == handle) {
      list_del_init(&f->node);
      found = f;
      break;
    }
  }
  if (!found) {
    /* Not registered yet, or a stray. Remembered briefly, for the first. */
    dom->early[dom->early_next].handle = handle;
    dom->early[dom->early_next].status = status;
    dom->early_next = (dom->early_next + 1) % NVGPU_FENCE_EARLY;
  }
  spin_unlock_irqrestore(&dom->lock, flags);

  if (found)
    nvgpu_fence_finish(dom, found, status);
  return found != NULL;
}

/* Give back a handle no fence was made for. */
static void nvgpu_fence_drop_handle(struct nvgpu_fence_dom *dom, u32 handle) {
  unsigned long flags;
  bool queued;

  spin_lock_irqsave(&dom->lock, flags);
  queued = kfifo_put(&dom->closeq, handle);
  spin_unlock_irqrestore(&dom->lock, flags);
  if (queued)
    schedule_work(&dom->close_work);
}

/* A guest fence for the host fence behind `handle`, pending until it signals.
 * The caller gets one reference. */
static struct dma_fence *nvgpu_fence_new(struct nvgpu_fence_dom *dom,
                                         u32 handle) {
  struct nvgpu_fence *f = kzalloc(sizeof(*f), GFP_KERNEL);
  unsigned long flags;
  bool early = false;
  s32 status = 0;
  unsigned int i;

  BUILD_BUG_ON(offsetof(struct nvgpu_fence, base) != 0);
  if (!f) {
    nvgpu_fence_drop_handle(dom, handle);
    return ERR_PTR(-ENOMEM);
  }
  spin_lock_init(&f->lock);
  /*
   * A context per fence. Host fences from one semaphore signal in value
   * order, but their EventReadys need not arrive in it, and a shared context
   * would let the core assume a later fence's signal covers an earlier one.
   */
  dma_fence_init(&f->base, &nvgpu_fence_ops, &f->lock,
                 dma_fence_context_alloc(1), 1);
  f->dom_id = dom->id;
  f->handle = handle;

  dma_fence_get(&f->base); /* the pending list's */
  spin_lock_irqsave(&dom->lock, flags);
  for (i = 0; i < NVGPU_FENCE_EARLY; i++) {
    if (dom->early[i].handle == handle) {
      early = true;
      status = dom->early[i].status;
      dom->early[i].handle = 0;
      break;
    }
  }
  if (early)
    INIT_LIST_HEAD(&f->node);
  else
    list_add_tail(&f->node, &dom->pending);
  spin_unlock_irqrestore(&dom->lock, flags);

  if (early)
    nvgpu_fence_finish(dom, f, status);
  return &f->base;
}

/* The fence-context proxy behind a guest handle, with a reference. */
static struct nvgpu_gem_object *nvgpu_fence_ctx_lookup(struct drm_file *file,
                                                       u32 handle) {
  struct drm_gem_object *obj = drm_gem_object_lookup(file, handle);

  if (!obj)
    return NULL;
  if (obj->funcs != &nvgpu_gem_funcs || !to_nvgpu_gem(obj)->fence_ctx) {
    drm_gem_object_put(obj);
    return NULL;
  }
  return to_nvgpu_gem(obj);
}

/* Make a host fence on the context `ctx_handle` names and return the guest
 * fence standing in for it. */
static struct dma_fence *nvgpu_fence_create(struct nvgpu_fd *nfd,
                                            struct drm_file *file,
                                            u32 ctx_handle, u32 timeout_ms,
                                            u64 wait_value) {
  struct nvgpu_fence_dom *dom = nfd->dev->fences;
  struct nvgpu_semsurf_fence_create p = {};
  struct nvgpu_gem_object *ctx;
  long ret;

  if (!dom)
    return ERR_PTR(-ENOTTY);
  ctx = nvgpu_fence_ctx_lookup(file, ctx_handle);
  if (!ctx)
    return ERR_PTR(-EINVAL);

  p.fence_context_handle = ctx->host_handle;
  p.timeout_value_ms = timeout_ms;
  p.wait_value = wait_value;
  ret = nvgpu_ioctl_flat_h(nfd->dev, ctx->owner_handle,
                           DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE, &p,
                           sizeof(p));
  drm_gem_object_put(&ctx->base);
  if (ret < 0)
    return ERR_PTR(ret);
  if (p.fd <= 0)
    return ERR_PTR(-EIO);
  return nvgpu_fence_new(dom, (u32)p.fd);
}

static long nvgpu_semsurf_fence_create(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void __user *uarg) {
  struct nvgpu_semsurf_fence_create p;
  struct sync_file *sf;
  struct dma_fence *fence;
  int fd;

  if (_IOC_SIZE(cmd) != sizeof(p))
    return -EINVAL;
  if (copy_from_user(&p, uarg, sizeof(p)))
    return -EFAULT;
  if (p.__pad)
    return -EINVAL;

  fd = get_unused_fd_flags(O_CLOEXEC);
  if (fd < 0)
    return fd;
  fence = nvgpu_fence_create(nfd, file, p.fence_context_handle,
                             p.timeout_value_ms, p.wait_value);
  if (IS_ERR(fence)) {
    put_unused_fd(fd);
    return PTR_ERR(fence);
  }
  sf = sync_file_create(fence);
  dma_fence_put(fence);
  if (!sf) {
    put_unused_fd(fd);
    return -ENOMEM;
  }
  p.fd = fd;
  if (copy_to_user(uarg, &p, sizeof(p))) {
    fput(sf->file);
    put_unused_fd(fd);
    return -EFAULT;
  }
  fd_install(fd, sf->file);
  return 0;
}

static long nvgpu_semsurf_fence_attach(struct nvgpu_fd *nfd,
                                       struct drm_file *file, unsigned int cmd,
                                       void __user *uarg) {
  struct nvgpu_semsurf_fence_attach p;
  struct drm_gem_object *obj;
  struct dma_fence *fence;
  int ret;

  if (_IOC_SIZE(cmd) != sizeof(p))
    return -EINVAL;
  if (copy_from_user(&p, uarg, sizeof(p)))
    return -EFAULT;

  obj = drm_gem_object_lookup(file, p.handle);
  if (!obj)
    return -EINVAL;
  fence = nvgpu_fence_create(nfd, file, p.fence_context_handle,
                             p.timeout_value_ms, p.wait_value);
  if (IS_ERR(fence)) {
    drm_gem_object_put(obj);
    return PTR_ERR(fence);
  }

  /* NVIDIA's `shared` is a reader's slot, otherwise the writer's. */
  dma_resv_lock(obj->resv, NULL);
  ret = dma_resv_reserve_fences(obj->resv, 1);
  if (!ret)
    dma_resv_add_fence(obj->resv, fence,
                       p.shared ? DMA_RESV_USAGE_READ : DMA_RESV_USAGE_WRITE);
  dma_resv_unlock(obj->resv);

  dma_fence_put(fence);
  drm_gem_object_put(obj);
  return ret;
}

/* A SEMSURF_FENCE_WAIT waiting for a guest fence before it is forwarded. */
struct nvgpu_fence_wait {
  struct dma_fence_cb cb;
  struct work_struct work;
  struct nvgpu_fence_dom *dom;
  struct nvgpu_gem_object *ctx; /* held, so its host handle stays the same */
  u64 pre, post;
};

/* Forward a wait: `named` is one of our fence handles, or 0 for signalled.
 * Called with dom->dev_lock held or from the ioctl (device alive). */
static long nvgpu_fence_wait_send(struct nvgpu_device *dev,
                                  struct nvgpu_gem_object *ctx, u32 named,
                                  u64 pre, u64 post) {
  struct nvgpu_semsurf_fence_wait p = {
      .fence_context_handle = ctx->host_handle,
      .fd = (s32)named,
      .pre_wait_value = pre,
      .post_wait_value = post,
  };

  return nvgpu_ioctl_flat_h(dev, ctx->owner_handle,
                            DRM_IOCTL_NVIDIA_SEMSURF_FENCE_WAIT, &p, sizeof(p));
}

static void nvgpu_fence_wait_work(struct work_struct *w) {
  struct nvgpu_fence_wait *wt = container_of(w, struct nvgpu_fence_wait, work);
  struct nvgpu_fence_dom *dom = wt->dom;
  long ret;

  mutex_lock(&dom->dev_lock);
  if (dom->dev) {
    ret = nvgpu_fence_wait_send(dom->dev, wt->ctx, 0, wt->pre, wt->post);
    if (ret < 0)
      dev_warn_ratelimited(&dom->dev->vdev->dev,
                           "conduit-gpu: SEMSURF_FENCE_WAIT refused: %ld\n",
                           ret);
    /* Freeing the proxy talks to the device, so only while there is one.
     * Without one it is leaked rather than freed against a dead device. */
    drm_gem_object_put(&wt->ctx->base);
  }
  mutex_unlock(&dom->dev_lock);
  kref_put(&dom->ref, nvgpu_fence_dom_free);
  kfree(wt);
  module_put(THIS_MODULE);
}

/* The guest fence signalled; may be interrupt context. */
static void nvgpu_fence_wait_cb(struct dma_fence *fence,
                                struct dma_fence_cb *cb) {
  struct nvgpu_fence_wait *wt = container_of(cb, struct nvgpu_fence_wait, cb);

  schedule_work(&wt->work);
}

/*
 * Which host fence, if any, a guest fence amounts to: the handle when every
 * unsignalled fence inside it is exactly one of ours, 0 when all of it has
 * signalled, -1 when it has to be waited for here.
 */
static s64 nvgpu_fence_as_handle(struct nvgpu_fence_dom *dom,
                                 struct dma_fence *fence) {
  struct dma_fence_unwrap iter;
  struct dma_fence *f;
  u32 handle = 0;
  int open = 0;

  dma_fence_unwrap_for_each(f, &iter, fence) {
    if (dma_fence_is_signaled(f))
      continue;
    if (++open > 1 || rcu_access_pointer(f->ops) != &nvgpu_fence_ops ||
        container_of(f, struct nvgpu_fence, base)->dom_id != dom->id)
      return -1;
    handle = container_of(f, struct nvgpu_fence, base)->handle;
  }
  return handle;
}

static long nvgpu_semsurf_fence_wait(struct nvgpu_fd *nfd,
                                     struct drm_file *file, unsigned int cmd,
                                     void __user *uarg) {
  struct nvgpu_fence_dom *dom = nfd->dev->fences;
  struct nvgpu_semsurf_fence_wait p;
  struct nvgpu_gem_object *ctx;
  struct nvgpu_fence_wait *wt;
  struct dma_fence *fence;
  s64 named;
  long ret;

  if (!dom)
    return -ENOTTY;
  if (_IOC_SIZE(cmd) != sizeof(p))
    return -EINVAL;
  if (copy_from_user(&p, uarg, sizeof(p)))
    return -EFAULT;
  /* The host refuses these too, and would only say so in its own log. */
  if (p.pre_wait_value >= p.post_wait_value)
    return -EINVAL;

  ctx = nvgpu_fence_ctx_lookup(file, p.fence_context_handle);
  if (!ctx)
    return -EINVAL;
  fence = sync_file_get_fence(p.fd);
  if (!fence) {
    drm_gem_object_put(&ctx->base);
    return -EINVAL;
  }

  /*
   * A fence of ours goes across as itself: the host GPU waits on the host
   * fence and no wake travels through this guest. ENOENT means the backend
   * has closed it -- it signalled meanwhile -- and the signalled path is
   * then the right answer.
   */
  named = nvgpu_fence_as_handle(dom, fence);
  if (named >= 0) {
    ret = nvgpu_fence_wait_send(nfd->dev, ctx, (u32)named, p.pre_wait_value,
                                p.post_wait_value);
    if (ret == -ENOENT && named > 0)
      ret = nvgpu_fence_wait_send(nfd->dev, ctx, 0, p.pre_wait_value,
                                  p.post_wait_value);
    if (ret >= 0) {
      dma_fence_put(fence);
      drm_gem_object_put(&ctx->base);
      return 0;
    }
  }

  /* Anything else is waited for here; the callback forwards it. */
  wt = kzalloc(sizeof(*wt), GFP_KERNEL);
  if (!wt || !try_module_get(THIS_MODULE)) {
    kfree(wt);
    dma_fence_put(fence);
    drm_gem_object_put(&ctx->base);
    return -ENOMEM;
  }
  INIT_WORK(&wt->work, nvgpu_fence_wait_work);
  kref_get(&dom->ref);
  wt->dom = dom;
  wt->ctx = ctx;
  wt->pre = p.pre_wait_value;
  wt->post = p.post_wait_value;
  if (dma_fence_add_callback(fence, &wt->cb, nvgpu_fence_wait_cb))
    schedule_work(&wt->work); /* already signalled */
  dma_fence_put(fence);
  return 0;
}

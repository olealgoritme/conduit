/* SPDX-License-Identifier: GPL-2.0 */
/*
 * nvgpu_clipboard.h — the shared clipboard, guest side.
 *
 *   host clipboard ──ClipboardFromHost chunks (event queue)──► this file
 *                    reassembled, kept (latest only) ──read()──► agent
 *   agent ──write() one record──► ClipboardToHost chunks (control queue) ──► host
 *
 * Exposed as the misc device /dev/conduit-clipboard. The guest agent
 * (conduit-clipboard-agent) runs in the user's session and bridges it to the
 * desktop's clipboard; this module knows nothing about Wayland or X11.
 *
 * Record format (read and write), all little-endian:
 *
 *   struct conduit_clip_record {   56 bytes
 *       u32 magic;                 0x50494c43, "CLIP"
 *       u32 version;               1
 *       u64 generation;            read: the host's transfer id; write: ignored
 *       u32 len;                   data bytes that follow
 *       u32 flags;                 0
 *       char mime[32];             NUL-padded, "text/plain;charset=utf-8"
 *   };
 *
 * read() returns one whole record for the newest host clipboard this open
 * file has not seen yet (a fresh open sees the current one at once); blocks
 * unless O_NONBLOCK; -EMSGSIZE, consuming nothing, if the buffer is short.
 * write() takes exactly one whole record and returns once the host answered.
 *
 * Interrupt context only copies a chunk onto a bounded list; reassembly runs
 * in a work item, so no large allocation ever happens with interrupts off.
 * The state is refcounted: open files outlive the device, and then read and
 * write say -ENODEV.
 */
#ifndef NVGPU_CLIPBOARD_H
#define NVGPU_CLIPBOARD_H

#include <linux/kref.h>
#include <linux/miscdevice.h>
#include <linux/wait.h>
#include <linux/workqueue.h>

#define NVGPU_CLIP_MAGIC 0x50494c43u
#define NVGPU_CLIP_VERSION 1u
#define NVGPU_CLIP_MAX_BYTES (1u << 20)
#define NVGPU_CLIP_MIME_LEN 32
#define NVGPU_CLIP_MIME_TEXT "text/plain;charset=utf-8"
/* guest -> host data per control-queue message */
#define NVGPU_CLIP_TX_CHUNK 4096u
/* Interrupt-side backlog bound; beyond it chunks are dropped and the
 * transfer they belonged to is abandoned by the reassembler. */
#define NVGPU_CLIP_PENDING_MAX (2u << 20)

/* The wire chunk header (protocol/src/messages.rs ClipboardChunk). */
struct nvgpu_clip_chunk {
  __le64 generation;
  __le32 total_len;
  __le32 offset;
  __le32 len;
  __le32 flags;
  char mime[NVGPU_CLIP_MIME_LEN];
} __packed;

/* The userspace record header. */
struct nvgpu_clip_record {
  __le32 magic;
  __le32 version;
  __le64 generation;
  __le32 len;
  __le32 flags;
  char mime[NVGPU_CLIP_MIME_LEN];
} __packed;

static_assert(sizeof(struct nvgpu_clip_chunk) == 56);
static_assert(sizeof(struct nvgpu_clip_record) == 56);

struct nvgpu_clip_node {
  struct list_head node;
  unsigned int len;
  u8 data[]; /* chunk header + chunk data, as received */
};

struct nvgpu_clip {
  struct kref ref;

  /* Interrupt side: the raw chunks waiting for the work item. */
  spinlock_t lock;
  struct list_head pending;
  unsigned int pending_bytes;
  bool dead; /* under lock (and read with READ_ONCE by waiters) */
  struct work_struct work;

  /* Reassembly, only ever touched by the work item. */
  u8 *asm_buf;
  u32 asm_total, asm_next;
  u64 asm_gen;

  /* The newest complete host clipboard. */
  struct mutex data_mutex;
  u8 *data;
  u32 data_len;
  u64 data_gen; /* the host's generation, informational */
  u64 seq;      /* ours, bumps per publish; 0 = nothing yet */
  wait_queue_head_t wq;

  /* Guest -> host. `dev` is NULL once the device is gone. */
  struct mutex write_mutex;
  struct nvgpu_device *dev;
  u64 tx_gen;
};

struct nvgpu_clip_file {
  struct nvgpu_clip *clip;
  u64 seen;
};

/* The one clipboard device; the first virtio-gpu-nv with a display owns it. */
static DEFINE_MUTEX(nvgpu_clip_global_mutex);
static struct nvgpu_clip *nvgpu_clip_global;

static void nvgpu_clip_free(struct kref *ref) {
  struct nvgpu_clip *c = container_of(ref, struct nvgpu_clip, ref);
  struct nvgpu_clip_node *n, *tmp;

  list_for_each_entry_safe(n, tmp, &c->pending, node) {
    list_del(&n->node);
    kfree(n);
  }
  kvfree(c->asm_buf);
  kvfree(c->data);
  kfree(c);
}

static void nvgpu_clip_put(struct nvgpu_clip *c) {
  kref_put(&c->ref, nvgpu_clip_free);
}

static void nvgpu_clip_abandon(struct nvgpu_clip *c) {
  kvfree(c->asm_buf);
  c->asm_buf = NULL;
  c->asm_total = c->asm_next = 0;
  c->asm_gen = 0;
}

static bool nvgpu_clip_mime_ok(const char *mime) {
  char m[NVGPU_CLIP_MIME_LEN + 1];

  memcpy(m, mime, NVGPU_CLIP_MIME_LEN);
  m[NVGPU_CLIP_MIME_LEN] = '\0';
  return strcmp(m, NVGPU_CLIP_MIME_TEXT) == 0;
}

/* One received chunk, in process context. */
static void nvgpu_clip_take(struct nvgpu_clip *c, const u8 *p,
                            unsigned int len) {
  const struct nvgpu_clip_chunk *h = (const void *)p;
  u64 gen = le64_to_cpu(h->generation);
  u32 total = le32_to_cpu(h->total_len);
  u32 off = le32_to_cpu(h->offset);
  u32 n = le32_to_cpu(h->len);

  if (n > len - sizeof(*h))
    return; /* checked in the interrupt; belt and braces */
  if (off == 0) {
    nvgpu_clip_abandon(c);
    if (total == 0 || total > NVGPU_CLIP_MAX_BYTES ||
        !nvgpu_clip_mime_ok(h->mime))
      return;
    c->asm_buf = kvmalloc(total, GFP_KERNEL);
    if (!c->asm_buf)
      return;
    c->asm_total = total;
    c->asm_gen = gen;
  } else if (!c->asm_buf || gen != c->asm_gen || off != c->asm_next) {
    /* A gap, or a chunk of another transfer: this one cannot complete. */
    nvgpu_clip_abandon(c);
    return;
  }
  if (n > c->asm_total - off) {
    nvgpu_clip_abandon(c);
    return;
  }
  memcpy(c->asm_buf + off, p + sizeof(*h), n);
  c->asm_next = off + n;
  if (c->asm_next < c->asm_total)
    return;

  mutex_lock(&c->data_mutex);
  kvfree(c->data);
  c->data = c->asm_buf;
  c->data_len = c->asm_total;
  c->data_gen = c->asm_gen;
  c->seq++;
  mutex_unlock(&c->data_mutex);
  c->asm_buf = NULL;
  c->asm_total = c->asm_next = 0;
  c->asm_gen = 0;
  wake_up_interruptible(&c->wq);
}

static void nvgpu_clip_work(struct work_struct *w) {
  struct nvgpu_clip *c = container_of(w, struct nvgpu_clip, work);
  struct nvgpu_clip_node *n, *tmp;
  unsigned long flags;
  LIST_HEAD(batch);

  spin_lock_irqsave(&c->lock, flags);
  list_splice_init(&c->pending, &batch);
  c->pending_bytes = 0;
  spin_unlock_irqrestore(&c->lock, flags);

  list_for_each_entry_safe(n, tmp, &batch, node) {
    list_del(&n->node);
    nvgpu_clip_take(c, n->data, n->len);
    kfree(n);
  }
}

/* Event-queue interrupt: a ClipboardFromHost payload of `len` bytes. */
static void nvgpu_clip_event(struct nvgpu_device *dev, const u8 *p,
                             unsigned int len) {
  struct nvgpu_clip *c = READ_ONCE(dev->clip);
  const struct nvgpu_clip_chunk *h = (const void *)p;
  struct nvgpu_clip_node *n;
  unsigned long flags;

  if (!c || len < sizeof(*h) ||
      le32_to_cpu(h->len) > len - sizeof(*h))
    return;
  len = sizeof(*h) + le32_to_cpu(h->len);

  spin_lock_irqsave(&c->lock, flags);
  if (c->dead || c->pending_bytes + len > NVGPU_CLIP_PENDING_MAX) {
    spin_unlock_irqrestore(&c->lock, flags);
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: clipboard chunk dropped\n");
    return;
  }
  spin_unlock_irqrestore(&c->lock, flags);

  n = kmalloc(struct_size(n, data, len), GFP_ATOMIC);
  if (!n)
    return; /* the gap abandons the transfer */
  n->len = len;
  memcpy(n->data, p, len);

  spin_lock_irqsave(&c->lock, flags);
  if (c->dead) {
    spin_unlock_irqrestore(&c->lock, flags);
    kfree(n);
    return;
  }
  list_add_tail(&n->node, &c->pending);
  c->pending_bytes += len;
  spin_unlock_irqrestore(&c->lock, flags);
  schedule_work(&c->work);
}

/* ───────── the character device ───────── */

static int nvgpu_clip_open(struct inode *inode, struct file *filp) {
  struct nvgpu_clip_file *f;
  struct nvgpu_clip *c;

  f = kzalloc(sizeof(*f), GFP_KERNEL);
  if (!f)
    return -ENOMEM;
  mutex_lock(&nvgpu_clip_global_mutex);
  c = nvgpu_clip_global;
  if (c)
    kref_get(&c->ref);
  mutex_unlock(&nvgpu_clip_global_mutex);
  if (!c) {
    kfree(f);
    return -ENODEV;
  }
  f->clip = c;
  filp->private_data = f;
  return nonseekable_open(inode, filp);
}

static int nvgpu_clip_release(struct inode *inode, struct file *filp) {
  struct nvgpu_clip_file *f = filp->private_data;

  nvgpu_clip_put(f->clip);
  kfree(f);
  return 0;
}

static bool nvgpu_clip_ready(struct nvgpu_clip_file *f) {
  return READ_ONCE(f->clip->seq) != f->seen || READ_ONCE(f->clip->dead);
}

static ssize_t nvgpu_clip_read(struct file *filp, char __user *buf,
                               size_t count, loff_t *ppos) {
  struct nvgpu_clip_file *f = filp->private_data;
  struct nvgpu_clip *c = f->clip;
  struct nvgpu_clip_record rec;
  ssize_t ret;

  for (;;) {
    mutex_lock(&c->data_mutex);
    if (READ_ONCE(c->dead)) {
      mutex_unlock(&c->data_mutex);
      return -ENODEV;
    }
    if (c->seq != f->seen && c->data)
      break; /* with data_mutex held */
    mutex_unlock(&c->data_mutex);
    if (filp->f_flags & O_NONBLOCK)
      return -EAGAIN;
    if (wait_event_interruptible(c->wq, nvgpu_clip_ready(f)))
      return -ERESTARTSYS;
  }

  if (count < sizeof(rec) + c->data_len) {
    ret = -EMSGSIZE;
    goto out;
  }
  memset(&rec, 0, sizeof(rec));
  rec.magic = cpu_to_le32(NVGPU_CLIP_MAGIC);
  rec.version = cpu_to_le32(NVGPU_CLIP_VERSION);
  rec.generation = cpu_to_le64(c->data_gen);
  rec.len = cpu_to_le32(c->data_len);
  strscpy(rec.mime, NVGPU_CLIP_MIME_TEXT, sizeof(rec.mime));
  if (copy_to_user(buf, &rec, sizeof(rec)) ||
      copy_to_user(buf + sizeof(rec), c->data, c->data_len)) {
    ret = -EFAULT;
    goto out;
  }
  f->seen = c->seq;
  ret = sizeof(rec) + c->data_len;
out:
  mutex_unlock(&c->data_mutex);
  return ret;
}

static __poll_t nvgpu_clip_poll(struct file *filp,
                                struct poll_table_struct *wait) {
  struct nvgpu_clip_file *f = filp->private_data;
  struct nvgpu_clip *c = f->clip;
  __poll_t mask;

  poll_wait(filp, &c->wq, wait);
  if (READ_ONCE(c->dead))
    return EPOLLERR | EPOLLHUP;
  mask = EPOLLOUT | EPOLLWRNORM;
  if (READ_ONCE(c->seq) != f->seen)
    mask |= EPOLLIN | EPOLLRDNORM;
  return mask;
}

/* Send one clipboard to the host, in chunks. write_mutex held, dev live. */
static int nvgpu_clip_send(struct nvgpu_clip *c, const u8 *text, u32 len) {
  const size_t head = sizeof(struct nvgpu_msg_hdr) +
                      sizeof(struct nvgpu_clip_chunk);
  struct nvgpu_msg_hdr resp;
  struct nvgpu_clip_chunk *ch;
  struct nvgpu_msg_hdr *hdr;
  u64 gen = ++c->tx_gen;
  u32 off = 0;
  u8 *req;
  int ret = 0;

  req = kmalloc(head + NVGPU_CLIP_TX_CHUNK, GFP_KERNEL);
  if (!req)
    return -ENOMEM;
  hdr = (struct nvgpu_msg_hdr *)req;
  ch = (struct nvgpu_clip_chunk *)(req + sizeof(*hdr));
  while (off < len) {
    u32 n = min_t(u32, len - off, NVGPU_CLIP_TX_CHUNK);
    s32 st;

    memset(req, 0, head);
    hdr->msg_type = cpu_to_le32(NVGPU_MSG_CLIPBOARD_TO_HOST);
    ch->generation = cpu_to_le64(gen);
    ch->total_len = cpu_to_le32(len);
    ch->offset = cpu_to_le32(off);
    ch->len = cpu_to_le32(n);
    strscpy(ch->mime, NVGPU_CLIP_MIME_TEXT, sizeof(ch->mime));
    memcpy(req + head, text + off, n);

    memset(&resp, 0, sizeof(resp));
    ret = nvgpu_send_recv(c->dev, req, head + n, &resp, sizeof(resp));
    if (ret < 0)
      break;
    st = (s32)le32_to_cpu(resp.status);
    if (st < 0) {
      ret = st;
      break;
    }
    off += n;
  }
  kfree(req);
  return ret;
}

static ssize_t nvgpu_clip_write(struct file *filp, const char __user *buf,
                                size_t count, loff_t *ppos) {
  struct nvgpu_clip_file *f = filp->private_data;
  struct nvgpu_clip *c = f->clip;
  struct nvgpu_clip_record rec;
  u32 len;
  u8 *text;
  int ret;

  if (count < sizeof(rec))
    return -EINVAL;
  if (copy_from_user(&rec, buf, sizeof(rec)))
    return -EFAULT;
  len = le32_to_cpu(rec.len);
  if (le32_to_cpu(rec.magic) != NVGPU_CLIP_MAGIC ||
      le32_to_cpu(rec.version) != NVGPU_CLIP_VERSION ||
      count - sizeof(rec) != len)
    return -EINVAL;
  if (len == 0)
    return -EINVAL;
  if (len > NVGPU_CLIP_MAX_BYTES)
    return -EMSGSIZE;
  if (!nvgpu_clip_mime_ok(rec.mime))
    return -EOPNOTSUPP;

  text = kvmalloc(len, GFP_KERNEL);
  if (!text)
    return -ENOMEM;
  if (copy_from_user(text, buf + sizeof(rec), len)) {
    kvfree(text);
    return -EFAULT;
  }

  if (mutex_lock_interruptible(&c->write_mutex)) {
    kvfree(text);
    return -ERESTARTSYS;
  }
  ret = c->dev ? nvgpu_clip_send(c, text, len) : -ENODEV;
  mutex_unlock(&c->write_mutex);
  kvfree(text);
  return ret < 0 ? ret : (ssize_t)count;
}

static const struct file_operations nvgpu_clip_fops = {
    .owner = THIS_MODULE,
    .open = nvgpu_clip_open,
    .release = nvgpu_clip_release,
    .read = nvgpu_clip_read,
    .write = nvgpu_clip_write,
    .poll = nvgpu_clip_poll,
    .llseek = noop_llseek,
};

static struct miscdevice nvgpu_clip_misc = {
    .minor = MISC_DYNAMIC_MINOR,
    .name = "conduit-clipboard",
    .fops = &nvgpu_clip_fops,
    .mode = 0660,
};

/* Probe, display present. Failure is logged, never fatal. */
static void nvgpu_clip_init(struct nvgpu_device *dev) {
  struct nvgpu_clip *c;
  int ret;

  mutex_lock(&nvgpu_clip_global_mutex);
  if (nvgpu_clip_global) {
    mutex_unlock(&nvgpu_clip_global_mutex);
    return; /* the first device with a display has it */
  }
  c = kzalloc(sizeof(*c), GFP_KERNEL);
  if (!c) {
    mutex_unlock(&nvgpu_clip_global_mutex);
    return;
  }
  kref_init(&c->ref);
  spin_lock_init(&c->lock);
  INIT_LIST_HEAD(&c->pending);
  INIT_WORK(&c->work, nvgpu_clip_work);
  mutex_init(&c->data_mutex);
  init_waitqueue_head(&c->wq);
  mutex_init(&c->write_mutex);
  c->dev = dev;

  nvgpu_clip_global = c;
  ret = misc_register(&nvgpu_clip_misc);
  if (ret) {
    nvgpu_clip_global = NULL;
    mutex_unlock(&nvgpu_clip_global_mutex);
    kfree(c);
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: /dev/conduit-clipboard not registered: %d\n",
             ret);
    return;
  }
  mutex_unlock(&nvgpu_clip_global_mutex);
  /* Published last: the event interrupt may use it from here on. */
  WRITE_ONCE(dev->clip, c);
  dev_info(&dev->vdev->dev, "virtio-gpu-nv: clipboard at /dev/conduit-clipboard\n");
}

/* Remove, before the device reset: no new opens, no more host traffic. */
static void nvgpu_clip_detach(struct nvgpu_device *dev) {
  struct nvgpu_clip *c = dev->clip;
  unsigned long flags;

  if (!c)
    return;
  mutex_lock(&nvgpu_clip_global_mutex);
  misc_deregister(&nvgpu_clip_misc);
  nvgpu_clip_global = NULL;
  mutex_unlock(&nvgpu_clip_global_mutex);

  /* A writer mid-transfer finishes (or times out) first. */
  mutex_lock(&c->write_mutex);
  c->dev = NULL;
  mutex_unlock(&c->write_mutex);

  spin_lock_irqsave(&c->lock, flags);
  WRITE_ONCE(c->dead, true);
  spin_unlock_irqrestore(&c->lock, flags);
  wake_up_interruptible(&c->wq);
}

/* Remove, after the device reset: the interrupt cannot run any more. */
static void nvgpu_clip_fini(struct nvgpu_device *dev) {
  struct nvgpu_clip *c = dev->clip;

  if (!c)
    return;
  dev->clip = NULL;
  cancel_work_sync(&c->work);
  nvgpu_clip_put(c);
}

#endif /* NVGPU_CLIPBOARD_H */

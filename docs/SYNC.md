# Explicit sync

A Linux guest's DRM node offers DRM syncobjs (`DRM_CAP_SYNCOBJ`,
`DRM_CAP_SYNCOBJ_TIMELINE`) and nvidia-drm's semaphore-surface fences
(`supports_semsurf`, `supports_sync_fd` in `GET_DEV_INFO`), the way the host's
nvidia-drm does. A compositor then offers `linux-drm-syncobj-v1`, and NVIDIA's
`egl-wayland2` (which needs it) works. Fences signal when the host GPU work is
done: each guest fence stands for a host fence and signals only when the host
fence did.

Code: `guest/linux/nvgpu_fence.h` (guest), `host/backend/device/src/nvidia/fence.rs`
and the event pump in `host/backend/device/bin/conduit-backend.rs` (backend).

## What NVIDIA's userspace asks for

From `nvidia-drm-fence.c` / `nv_drm_common_ioctl.h` (610.57.04):

| ioctl (abs nr) | what nvidia-drm does |
|---|---|
| `SEMSURF_FENCE_CTX_CREATE` (0x54) | imports an RM semaphore surface (`nvkms_params_ptr` → `{hClient, hSemaphoreSurface, size}`) and returns a GEM handle for a fence context at semaphore `index` |
| `SEMSURF_FENCE_CREATE` (0x55) | a dma_fence that signals when the semaphore reaches `wait_value` (RM callback on the semaphore surface; error `-ETIMEDOUT` after `timeout_value_ms`, at most 5000), returned as a sync_file `fd` |
| `SEMSURF_FENCE_WAIT` (0x56) | when the sync_file `fd` signals, register "once the semaphore reaches `pre_wait_value`, set it to `post_wait_value`" with RM -- the GPU-side wait on a CPU-visible fence |
| `SEMSURF_FENCE_ATTACH` (0x57) | a fence as in 0x55, added to a GEM buffer's `dma_resv` (shared = read, else write) |
| `PRIME_FENCE_CONTEXT_CREATE` / `GEM_PRIME_FENCE_ATTACH` (0x45/0x46) | the legacy path: a mapped semaphore + RM channel event. Not used when `supports_semsurf` is set |
| `FENCE_SUPPORTED` (0x44) | 0 when modeset=1 |

`GET_DEV_INFO` reports `supports_semsurf` and `supports_sync_fd` together, both
when the device has semaphore surfaces. The generic syncobj ioctls
(`DRM_IOCTL_SYNCOBJ_*`) are DRM core, enabled by `DRIVER_SYNCOBJ |
DRIVER_SYNCOBJ_TIMELINE`, and work on any dma_fence.

## Local vs forwarded

| | where |
|---|---|
| syncobjs, timeline points, sync_file import/export, eventfd waits | guest DRM core, untouched |
| guest dma_fences, guest sync_file fds | guest kernel (`nvgpu_fence.h`) |
| fence contexts (host GEM objects) | host; a guest GEM proxy (`fence_ctx`) stands in, never mapped or exported |
| semaphore surfaces, the RM callbacks, the GPU-side waits | host nvidia-drm / RM |
| host sync_files | backend process, one per live host fence, under a handle |

## Host fence → guest fence

1. Guest `SEMSURF_FENCE_CREATE` → forwarded on the context's owner file with
   the host context handle. The backend makes the call, keeps the returned
   sync_file under a new handle (refused past 4096 live per guest), and
   answers with the handle in `fd`.
2. The guest makes a dma_fence for the handle (own context, seqno 1),
   wraps it in a sync_file and returns that fd.
3. The backend's transport watches the handle like a device descriptor, but
   one-shot: when the sync_file becomes readable (= signalled) it sends one
   `EventReady` for the handle with the fence's status (`SYNC_IOC_FILE_INFO`;
   `-ETIMEDOUT` etc.) in the header and drops it from the poll set. A
   notification that found no event buffer stays watched and is re-sent.
4. The guest finds no open file with that handle, finds the pending fence,
   signals it (with the error, if any) and sends `Close` for the handle from a
   work item; the backend closes the host sync_file. An event that arrives
   before the guest registered the fence is kept in a small ring and applied
   at registration.

`SEMSURF_FENCE_ATTACH` is answered in the guest: a host fence as above, added
to the proxy's own `dma_resv`, which guest importers of the dma-buf see. The
guest KMS head waits for plane fences (explicit `IN_FENCE_FD` and implicit,
via the GEM `prepare_fb` default) before it sends `ScanoutFlip`.

## Guest fence → GPU wait

`SEMSURF_FENCE_WAIT` names a guest sync_file. Unwrapped (arrays, chains):

- every unsignalled fence in it is exactly one of ours → forwarded with that
  fence's handle in `fd`; the backend substitutes the host sync_file, so the
  host GPU waits on the real fence and nothing crosses the guest. If the
  backend answers `ENOENT` (the fence signalled and was closed meanwhile), it
  is resent as signalled.
- all signalled → forwarded with `fd = 0`.
- anything else (a KMS out-fence, a CPU-signalled syncobj point, another
  driver's fence) → a dma_fence callback; when it signals, a work item
  forwards the wait with `fd = 0`.

For `fd = 0` the backend passes a sync_file that has already signalled (made
once per guest from a signalled host syncobj, `SYNCOBJ_CREATE(SIGNALED)` +
`HANDLE_TO_FD(EXPORT_SYNC_FILE)`). nvidia-drm then registers the pre→post
semaphore update immediately, which is exactly the semantics of the original
wait: the semaphore reaches `post` only after the guest fence signalled and the
semaphore reached `pre`.

## Protocol

Backward compatible, no new message type:

- config `features` bit 11, `NVGPU_CFG_DRM_FENCES`: set by `conduit-backend`
  (whose event pump does the one-shot watch). Without it the guest reports
  `supports_semsurf = 0`, registers no syncobj feature and leaves 0x54..0x57
  unhandled, as before. Old guests never send them.
- `EventReady.status`: 0 for descriptors as before; a fence's error for
  fence handles.
- `SEMSURF_FENCE_CREATE` reply / `SEMSURF_FENCE_WAIT` request: `fd` carries a
  backend handle (or 0), never a descriptor number.

## Validation (backend)

- 0x55/0x56 must be exactly 24 bytes; the `fd` the host sees on create is -1
  until the host writes one, so a guest number is never adopted as ours.
- 0x56 names a live fence handle of this guest, or 0; anything else (closed,
  a device, unknown) is `ENOENT` without a host call.
- 0x54: the NVKMS block's `hClient` must be an RM client this guest was
  issued; the pointer is replaced by a backend buffer as for the GEM imports.
- A fence handle takes no message but `Close` (an ioctl on a sync_file could
  make descriptors here).
- Not forwarded: 0x45 (pointers and a memFd) and the core syncobj range
  (0xbf..0xcf), which would only make descriptors in the backend.

## Switches and limits

- `conduit_gpu.explicit_sync=0` turns it all off in the guest;
  `claim_sync_fd=1` still forces `supports_sync_fd` alone (legacy).
- The legacy PRIME fences (0x45/0x46) are not served; `FENCE_SUPPORTED`
  answers as before.
- Fences cost a backend descriptor each until they signal (≤ 5 s, nvidia-drm's
  timeout) and one `EventReady` + one `Close` each.
- On kernels before 7.x (no fence detaching), unloading the module while a
  guest sync_file is alive leaves a fence whose ops are gone; deferred waits
  hold a module reference.

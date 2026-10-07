# Checking the proven setup after a change

The proven setup is an RTX 5090 on the open kernel modules, driver 610.57.04
(580.178.04, 595.71.05, 595.104.02 and 615.71.09 have tables too), with a Linux
or Windows guest. A change that touches driver ABI tables, `GET_DEV_INFO`,
NVKMS, staging, the guest module or the protections must leave it exactly as it
was. The unit tests cover the tables and the decisions; only a real run covers
the rest. This is that run, on the 5090 machine, comparing a build of the
change (B) with a build of the base commit (A) on the same VM.

Pass means: every line below gives the same answer for A and B, apart from the
ones marked "differs on purpose".

## Build both

```sh
git worktree add ../conduit-base <base commit>      # A
cargo build --release -p conduit
(cd host/backend && cargo build --release -p device --features vhost-user,venus --bins)
```

Do the same in the checkout with the change (B). Use each build's own
`target/release/conduit`; point `CONDUIT_QEMU` and `CONDUIT_VIEWER` at the same
QEMU and viewer for both.

## 1. The host answers the same

| Run (A, then B) | Same? |
|---|---|
| `conduit doctor` | no FAIL line; no "Safe mode: ON"; the driver line is `ok`, not `warn`; the Driver files line says ready |
| `conduit config get gpu.safe_mode` | `auto` or unset |
| `packaging/supported-drivers.sh` | B lists the same releases as A, plus any new one |

## 2. The backend is started the same way

Start the VM, then read the backend's command line and environment.

```sh
conduit up VM                       # or conduit view VM
tr '\0' ' ' < /proc/$(pgrep -f conduit-backend | head -1)/cmdline; echo
tr '\0' '\n' < /proc/$(pgrep -f conduit-backend | head -1)/environ | grep CONDUIT_
```

B has no `--vram-limit-mib` and no `CONDUIT_SAFE_MODE` unless you set them.
For a libvirt VM also read `systemctl --user cat conduit-backend@VM.service`.

## 3. The guest sees the same GPU

In the guest, for both builds:

```sh
nvidia-smi
vulkaninfo --summary
vkcube            # zero-copy window, no tearing, same frame rate
```

On the host, the backend log (`conduit logs VM backend`) must have the same
`DRI renderD128 ... page kind K/G, sector layout S` line in A and B (the
`GET_DEV_INFO` decode), and no new `refused` or `size mismatch` lines.
`conduit logs VM backend | grep -c refused` is the same count after the same
workload; list the differences with `diff <(grep refused A.log | sort -u) <(grep refused B.log | sort -u)`.

## 4. Display, sync and video

- A Wayland compositor in the guest starts and presents (this uses NVKMS and
  `GET_DEV_INFO`: wrong modifiers or missing explicit sync show up here).
- `grep 'NVKMS cmd' backend.log`: the same commands answered and refused in A
  and B. One differs on purpose: on 580.178.04 the old code matched the vblank
  commands at the wrong index.
- An NVENC or NVDEC test and a CUDA sample run (`--caps` includes video and
  compute by default).

## 5. Windows guest

Desktop at the usual refresh rate, a D3D11 and a Vulkan app, the Unigine
Heaven result within a few percent of A, and no new lines in the KMD log.

## 6. Start, stop, repeat

Three times: `conduit up VM`, a workload, `conduit down VM`. After each stop:

```sh
ls -l /proc/*/fd 2>/dev/null | grep -c nvidia     # backend gone: nothing left holding the GPU
nvidia-smi --query-gpu=memory.used --format=csv   # back to the desktop's own use
```

## 7. Packaging

`make deb` (or `packaging/build.sh package guest-deb`) succeeds; the guest
package lists every directory of `guest/linux/devinfo/`; the guest module
builds with `make -C guest/linux` and its checks pass (`make -C guest/linux check`).

## When something differs

Keep both backend logs and the output of `conduit doctor`. A difference that is
not marked "differs on purpose" is a regression of the change under test, not a
problem of the 5090 setup.

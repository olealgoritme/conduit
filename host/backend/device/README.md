# device

`conduit-backend`, the host half of Conduit's GPU device, and the library it is
built on. Licensed Apache-2.0 (`../LICENSE-APACHE-2.0`).

One backend process serves one VM over vhost-user (QEMU or `conduit-vmm`
connect to its socket; `conduit up` / libvirt socket activation start it). It
holds the real `/dev/nvidia*` descriptors, checks every forwarded ioctl against
the host driver release's ABI table (`../gen`), applies the RM allowlist,
supplies every pointer-carrying buffer itself, places GPU mappings in the
shared-memory window and UVM aperture, and exports the guest's scanout
buffers as dma-bufs to `conduit-viewer` / `conduit-stream`. Before the first
guest message it drops privileges and locks itself down (seccomp, Landlock);
it refuses to run as root. See `docs/ARCHITECTURE.md` and `docs/SECURITY.md`.

| path | what it does |
|---|---|
| `bin/conduit-backend.rs` | `conduit-backend`: the vhost-user transport and options |
| `bin/conduit-userspace.rs` | `conduit-userspace`: stages the host's NVIDIA userspace for the guest's read-only share |
| `src/nvidia/` | per-VM state, ioctl/RM/UVM forwarding and refusals, memory placement |
| `src/display.rs` | scanout, cursor, input, clipboard and mode hints to the viewer |
| `src/caps.rs` | `--caps`: which device nodes and RM classes a guest is served |
| `src/vram.rs`, `src/nvidia/vidmem.rs` | `--vram-limit-mib` |
| `src/sandbox.rs`, `src/posture.rs` | seccomp, Landlock, privilege drop |

## Build and test

```sh
cd host/backend
cargo build --release -p device --features vhost-user --bins
cargo test --workspace --features device/vhost-user -- \
    --skip for_real --skip closing_the_fd --skip repeated_map_unmap
```

`make backend` / `make test` at the repo root do the same. The tests run
against a fake host driver; the skipped ones open the real `/dev/nvidiactl`.

```sh
conduit-backend --socket /run/user/1000/conduit/vm.sock \
    --caps graphics,video,utility,compute [--vram-limit-mib 8192] \
    [--display-socket PATH]
```

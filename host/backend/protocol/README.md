# protocol

The wire format between Conduit's guest module (`guest/linux`) and
`conduit-backend`: message types, request and response headers, and the
constants that name devices. Definitions only, no logic.

Licensed BSD-3-Clause OR GPL-2.0+ (`../LICENSE-BSD-3-Clause`,
`../LICENSE-GPL-2.0`), so the GPL guest module and the Apache-2.0 backend can
share one set of definitions.

`src/messages.rs` is the Rust side; the C side is in
`guest/linux/conduit_gpu.c`. `src/venus.rs` holds the virtio-gpu structs a
Windows guest's `GpuCmd` carries (docs/VENUS.md). Nothing checks the two against each other
automatically: change both in the same commit.

Shared-memory regions (ids defined in `device/src/shm_regions.rs` and
the guest module): 1 = window (GPU mappings, DRM objects), 2 = UVM aperture
(CUDA semaphore pools, managed memory), 3 = Venus host-visible blobs (only
with `--venus`; docs/VENUS.md).

Each request is one descriptor chain: readable descriptors (header and
request), then writable ones for the reply. The backend scatters a reply
across every writable descriptor in order (`device/src/chain.rs`); the Linux
guest posts one.

Test: `cd host/backend && cargo test -p protocol`.

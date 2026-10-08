// crates/device/src/lib.rs
//
// VMM backend for conduit-gpu.
//
// Integrates with libkrun's virtio device infrastructure.  The backend holds
// real host file descriptors for `/dev/nvidia*` and dispatches messages
// received from the guest driver over virtqueues.

pub mod caps;
#[cfg(feature = "vhost-user")]
pub mod chain;
pub mod console;
pub mod display;
pub mod error;
pub mod guarded;
pub mod guestmem;
pub mod handle_table;
pub mod host;
pub mod mmap;
pub mod nvidia;
pub mod posture;
pub mod replay;
pub mod sandbox;
pub mod scanout_presented;
pub mod scanout_release;
pub mod shm;
pub mod shm_regions;
pub mod stage;
#[cfg(feature = "trace")]
pub mod trace;
pub mod userspace;
#[cfg(feature = "venus")]
pub mod venus;
pub mod virtio;
pub mod vram;

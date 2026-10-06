//! VirtIO and virtio-gpu feature bits (TRANSPORT.md §1.3).
//!
//! The device exposes 64 feature bits split into two 32-bit selects. We model
//! them as `u64` masks; the PCI common-config code splits them into the two
//! `device_feature_select` windows when reading/writing.

/// Modern VirtIO (1.0+). REQUIRED — we only support the modern interface.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Queue reset support (VirtIO 1.2). REQUIRED per TRANSPORT.md §1.3.
pub const VIRTIO_F_RING_RESET: u64 = 1 << 40;
/// Device supports the indirect descriptor flag.
pub const VIRTIO_F_INDIRECT_DESC: u64 = 1 << 28;
/// Device supports `used`/`avail` event suppression.
pub const VIRTIO_F_EVENT_IDX: u64 = 1 << 29;

/// 3D / virgl support (Venus rides on the 3D submit path).
pub const VIRTIO_GPU_F_VIRGL: u64 = 1 << 0;
/// EDID readback. We request it for completeness (render-only ignores it).
pub const VIRTIO_GPU_F_EDID: u64 = 1 << 1;
/// Per-resource UUIDs — needed for Venus blob tracking.
pub const VIRTIO_GPU_F_RESOURCE_UUID: u64 = 1 << 2;
/// Blob resources (zero-copy guest<->host memory). REQUIRED for Venus.
pub const VIRTIO_GPU_F_RESOURCE_BLOB: u64 = 1 << 3;
/// Context init — lets us request the Venus capset on CTX_CREATE. REQUIRED.
pub const VIRTIO_GPU_F_CONTEXT_INIT: u64 = 1 << 4;

/// The set of features Helios requires from the device. Negotiation MUST
/// confirm all of these survive the FEATURES_OK handshake; if the device drops
/// any of them, init fails (we cannot run Venus without them).
pub const HELIOS_REQUIRED_FEATURES: u64 = VIRTIO_F_VERSION_1
    | VIRTIO_GPU_F_VIRGL
    | VIRTIO_GPU_F_RESOURCE_BLOB
    | VIRTIO_GPU_F_CONTEXT_INIT;

/// Features we will accept if offered but do not strictly require.
pub const HELIOS_OPTIONAL_FEATURES: u64 =
    VIRTIO_F_RING_RESET | VIRTIO_GPU_F_EDID | VIRTIO_GPU_F_RESOURCE_UUID;

// ── Conduit device features (the backend's own bits, above the virtio-gpu ones) ─────
//
// The Conduit device offers `VIRTIO_F_VERSION_1` and, with options, a few vendor feature
// bits. This KMD acks exactly what it can serve; see `virtio/gpu/mod.rs` (`init`).

/// `NVGPU_CFG_TAKES_INPUT` (virtio feature bit 12). Acking it moves the keyboard and
/// mouse onto `InputEvent`s on the event queue, which this driver cannot consume: it is
/// NEVER acked (a const assertion in `virtio/gpu/nvrm_events.rs` keeps it out of every
/// set the driver writes back).
pub const NVGPU_F_TAKES_INPUT: u64 = 1 << 12;
/// `NVGPU_F_SCANOUT_RELEASE` (virtio feature bit 15; config `features` bit 15 stays
/// unused). The host offers it whenever it has a display. A guest that acks it gets
/// `MsgType::ScanoutReleased` (28) on the event queue whenever the latest flip of a buffer
/// was replaced and every display client that was sent it is done, and the host keeps
/// release bookkeeping only for such a guest. `docs/foreign-scanout.md` ("Buffer
/// release") is the KMD's use of it.
pub const NVGPU_F_SCANOUT_RELEASE: u64 = 1 << 15;
/// Conduit device features this driver acks when the device offers them (and, for
/// `NVGPU_F_SCANOUT_RELEASE`, when the display half is on): the optional set,
/// besides `VIRTIO_F_VERSION_1`.
pub const CONDUIT_OPTIONAL_FEATURES: u64 = NVGPU_F_SCANOUT_RELEASE;

const _: () = assert!(CONDUIT_OPTIONAL_FEATURES & NVGPU_F_TAKES_INPUT == 0);

// ── Device status bits (VirtIO spec §2.1) ──────────────────────────────────
pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
pub const VIRTIO_STATUS_DRIVER: u8 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
pub const VIRTIO_STATUS_NEEDS_RESET: u8 = 64;
pub const VIRTIO_STATUS_FAILED: u8 = 128;

// ── PCI vendor capability config types (TRANSPORT.md §1.2) ──────────────────
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
pub const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;
/// Shared-memory region capability (`virtio_pci_cap64`); its `id` byte selects a
/// shmid. virtio-drivers' `PciTransport` ignores this type, so the host-visible
/// blob window (ARCH §6) is found by a manual cap walk over the bus interface.
pub const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;

/// PCI device identity for the virtio-gpu device (OVERVIEW.md / TRANSPORT.md).
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_GPU_PCI_DEVICE_ID: u16 = 0x1050;

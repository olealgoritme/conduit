//! # Where `unsafe` sits on these declarations (R814)
//!
//! Rust's one memory-safety signal used to be attached to the wrong six
//! declarations here, so a reviewer scanning for `unsafe` call sites looked in
//! the wrong places. Three pointer-laundering entry points were SAFE
//! (`set_resource_kmt_handles`, `transfer_resource_ownership`,
//! `open_ddi_texture2d`) while three that take only scalars and cannot violate
//! memory safety were `unsafe fn` (`present_frame_gate`, and
//! `present_sync_fence_id` / `present_flip_wait_arm`, both retired with the
//! kwait subsystem in T6/R912a).
//!
//! Scope limit worth stating, because it changes the finding's shape: cxx
//! REQUIRES `unsafe fn` for any signature containing a raw pointer, and every
//! raw-pointer declaration in this block already is. Those are correct and are
//! not touched. `present_vehicle_copy` takes `usize` COM pointers AND is
//! already unsafe, which is the correct end state; it is left alone too
//! (`present_sync_publish` was its twin until R912a retired it).

//! cxx bridge to DXVK's C++ engine.
//!
//! The UMD's `d3d10umddi` frontend (Rust) calls into DXVK's `DxvkInstance`/
//! `DxvkAdapter`/`DxvkDevice` through this bridge. The C++ side (`bridge/
//! dxvk_bridge.cpp`) owns the DXVK `Rc<>` objects inside an opaque
//! `HeliosDxvkDevice`; Rust holds it via `UniquePtr`.
//!
//! Backend Vulkan device = the Gate-5a venus ICD; the shim force-selects it via
//! `DXVK_FILTER_DEVICE_NAME="Virtio-GPU Venus"` before creating the instance.

#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("dxvk_bridge.h");

        /// Opaque holder for the DXVK instance + adapter + device + the DXVK
        /// D3D11 COM device the DDI forwards to.
        type HeliosDxvkDevice;

        /// Raw `ID3D11Device*` / `ID3D11DeviceContext*` (as usize) the DDI
        /// device-funcs forward to. 0 if not created. Borrowed — the bridge keeps
        /// the owning ref; wrap on the Rust side without taking ownership.
        fn d3d11_device_ptr(self: &HeliosDxvkDevice) -> usize;
        fn d3d11_context_ptr(self: &HeliosDxvkDevice) -> usize;
        fn venus_context_id(self: &HeliosDxvkDevice) -> u32;
        fn feed_trace_timestamp_ns(self: &HeliosDxvkDevice) -> u64;
        fn feed_trace_render_callback(self: &HeliosDxvkDevice, duration_ns: u64);
        fn feed_trace_present_callback(self: &HeliosDxvkDevice, duration_ns: u64);
        /// # Safety
        /// `deferred_context_ptr` is borrowed and live. `command_list_ptr`
        /// transfers one owned COM reference on true; false leaves ownership
        /// with the caller, which must reconstruct and release it.
        unsafe fn recycle_deferred_command_list(
            self: &HeliosDxvkDevice,
            deferred_context_ptr: usize,
            command_list_ptr: usize,
        ) -> bool;
        /// # Safety
        /// `deferred_context_ptr` must be a live deferred context created by
        /// this bridge immediately before this call.
        unsafe fn enable_deferred_context_ddi_logical_reset(
            self: &HeliosDxvkDevice,
            deferred_context_ptr: usize,
        ) -> bool;
        /// # Safety
        /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`; the bridge
        /// `reinterpret_cast`s it and calls `GetCommonTexture` on it.
        unsafe fn set_resource_kmt_handles(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            local: u32,
            global: u32,
        ) -> bool;
        unsafe fn get_resource_memory_info(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            memory: *mut u64,
            size: *mut u64,
            offset: *mut u64,
            resource_id: *mut u32,
        ) -> bool;
        /// C1 identity: exact creating-`vkAllocateMemory` size + memoryTypeIndex
        /// of the resource's backing venus memory (recorded into the WDDM
        /// allocation trailer for cross-process openers).
        unsafe fn get_resource_alloc_identity(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            venus_alloc_size: *mut u64,
            memory_type_index: *mut u32,
            global_vidmm_tracker: *mut u64,
        ) -> bool;
        /// # Safety
        /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`.
        unsafe fn transfer_resource_ownership(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
        ) -> bool;
        /// # Safety
        /// Returns an OWNED COM pointer the caller must release; the safe
        /// wrapper `open_texture2d` is the only thing that should call it.
        unsafe fn open_ddi_texture2d(
            self: &HeliosDxvkDevice,
            width: u32,
            height: u32,
            format: u32,
            bind_flags: u32,
            misc_flags: u32,
            global: u32,
            renderer_resource_id: u32,
            venus_alloc_size: u64,
            memory_type_index: u32,
            global_vidmm_tracker: u64,
            scanout_linear: bool,
            linear_scanout_target: bool,
            cross_context_optimal: bool,
            dedicated_present_buffer: bool,
            source_image_create_info: usize,
            source_external_ownership: bool,
            foreign: bool,
            foreign_modifier: u64,
            foreign_stride: u32,
            foreign_offset: u32,
            foreign_plane1_modifier: u64,
            foreign_plane1_stride: u32,
            foreign_plane1_offset: u32,
        ) -> usize;

        /// The ICD backend of this device: 1 = Venus, 2 = NVK on RM
        /// (`helios_icd_interface.h`).
        fn icd_backend(self: &HeliosDxvkDevice) -> u32;
        /// NVK: the KMD resource id (IMPORT_RM) of a WDDM-backed texture's
        /// dedicated memory, its holder context and recorded layout. False when
        /// none can be made now.
        /// # Safety
        /// `d3d11_resource_ptr` is a live `ID3D11Resource*`; the outputs are
        /// live writable storage.
        unsafe fn get_resource_foreign_identity(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            resource_id: *mut u32,
            ctx_id: *mut u32,
            size: *mut u64,
            modifier: *mut u64,
            stride: *mut u32,
            offset: *mut u32,
            fourcc: *mut u32,
            plane1_modifier: *mut u64,
            plane1_stride: *mut u32,
            plane1_offset: *mut u32,
        ) -> bool;
        /// NVK: show the texture on scanout 0 (KMD foreign scanout source).
        /// 0 = shown. # Safety: a live `ID3D11Resource*`.
        unsafe fn nvk_scanout_present(self: &HeliosDxvkDevice, d3d11_resource_ptr: usize) -> i32;
        /// NVK: give scanout 0 back to the desktop.
        fn nvk_scanout_release(self: &HeliosDxvkDevice);
        /// NVK: `HELIOS_ICD_CAP_*` of the ICD (0 on Venus).
        fn nvk_icd_caps(self: &HeliosDxvkDevice) -> u32;
        /// NVK RM fences (S4): show the texture on scanout 0 once the GPU has
        /// finished everything submitted so far, without a CPU wait. 0 =
        /// queued, 1 = no RM fences here (CPU wait + `nvk_scanout_present`),
        /// negative = not shown. # Safety: a live `ID3D11Resource*`.
        unsafe fn nvk_scanout_present_fenced(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
        ) -> i32;
        /// NVK: the KMD's seq and source generation of the texture's latest
        /// scanout frame (already-on-scanout present tag). False without them.
        /// # Safety: a live `ID3D11Resource*`; both pointers live writable storage.
        unsafe fn nvk_scanout_frame(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            sequence: *mut u64,
            generation: *mut u32,
        ) -> bool;
        /// NVK RM fences (S4): a fence for everything submitted so far, for a
        /// WDDM present marker. 0 = `*fence_handle` is the caller's.
        /// # Safety: both pointers are live writable storage.
        unsafe fn nvk_present_fence(
            self: &HeliosDxvkDevice,
            fence_handle: *mut u32,
            value: *mut u64,
        ) -> i32;
        /// NVK: close a fence the caller still owns.
        fn nvk_rm_fence_close(self: &HeliosDxvkDevice, fence_handle: u32);
        /// Flush gate (docs/flush-gate.md): flush, then the point the HEFL
        /// packet carries. 0 nothing new, 1 ready, -1 unavailable, -2 failed.
        /// # Safety: every pointer is live writable storage.
        /// Hand-off ledger: give a shared resource its key now.
        /// # Safety: a live `ID3D11Resource*`.
        unsafe fn handoff_register(self: &HeliosDxvkDevice, d3d11_resource_ptr: usize);
        /// NVK: the resource is a composed Present's Blt source; DXVK's
        /// next lists that touch it wait for the KMD's read-ledger claim.
        /// Returns the ledger id (0: none).
        /// # Safety: a live `ID3D11Resource*`.
        unsafe fn mark_blt_source(self: &HeliosDxvkDevice, d3d11_resource_ptr: usize) -> u32;
        /// Hand-off ledger: the resource goes; this process lets go of its key.
        /// # Safety: a live `ID3D11Resource*`.
        unsafe fn handoff_unregister(self: &HeliosDxvkDevice, d3d11_resource_ptr: usize);
        /// Hand-off ledger: publish a point on `resources`. 0 nothing new,
        /// 1 published, -1 unavailable/full, -2 failed.
        /// # Safety: `resources` addresses `resource_count` live resources.
        unsafe fn handoff_publish(
            self: &HeliosDxvkDevice,
            resources: *const usize,
            resource_count: u32,
        ) -> i32;
        unsafe fn flush_gate_point(
            self: &HeliosDxvkDevice,
            mode: u32,
            resources: *const usize,
            resource_count: u32,
            ctx_id: *mut u32,
            value32: *mut u32,
            cookie: *mut u64,
            fence: *mut u32,
            fence_value: *mut u64,
        ) -> i32;

        /// Create a dedicated OPTIMAL, DMA_BUF-exportable image and report
        /// logical scanout metadata. `kmd_transfer_source` selects the
        /// canonical GENERAL layout required by the KMD transfer importer.
        /// Returns an owned `ID3D11Resource*` (as usize), or 0 on failure.
        unsafe fn create_ddi_scanout_texture2d(
            self: &HeliosDxvkDevice,
            width: u32,
            height: u32,
            format: u32,
            bind_flags: u32,
            misc_flags: u32,
            kmd_transfer_source: bool,
            out_row_pitch: *mut u64,
            out_offset: *mut u64,
        ) -> usize;

        unsafe fn create_vertex_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        unsafe fn create_pixel_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        /// >=11.1 DDI shader create carrying the typed I/O signatures. `kind`:
        /// 0 = vertex, 1 = pixel, 2 = geometry. `sig_words` layout:
        /// [n_in, n_out, (sysval, register, mask, comptype, stream) x n_in,
        /// the same x n_out].
        unsafe fn create_shader_sig(
            self: &HeliosDxvkDevice,
            kind: u32,
            code: *const u8,
            len: usize,
            sig_words: *const u32,
            sig_words_len: usize,
        ) -> usize;
        /// Tessellation shader create carrying input/output/patch-constant
        /// signatures. `kind`: 0 = hull, 1 = domain. `sig_words` layout:
        /// [n_in, n_out, n_patch, then (sysval, register, mask, comptype,
        /// stream) entries for each group].
        unsafe fn create_tess_shader_sig(
            self: &HeliosDxvkDevice,
            kind: u32,
            code: *const u8,
            len: usize,
            sig_words: *const u32,
            sig_words_len: usize,
        ) -> usize;
        /// Flip-model identity rotation: texture i takes texture i+1's DXVK
        /// storage (memory + VkImage + KMT handles); the last takes the
        /// first's. The swap executes on the CS thread (ordered); no drain.
        unsafe fn rotate_resource_backings(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptrs: *const usize,
            count: usize,
        ) -> bool;
        /// DXGI Blt cross-format path. Numerically converts the selected
        /// source region instead of preserving equal-sized packed texel bits.
        /// `use_src_box == false` selects the complete source mip.
        unsafe fn dxgi_blt_convert(
            self: &HeliosDxvkDevice,
            dst_resource_ptr: usize,
            dst_subresource: u32,
            dst_x: u32,
            dst_y: u32,
            src_resource_ptr: usize,
            src_subresource: u32,
            use_src_box: bool,
            src_left: u32,
            src_top: u32,
            src_right: u32,
            src_bottom: u32,
        ) -> i32;
        /// S_OK after ordering, S_FALSE on a bounded completion timeout,
        /// E_FAIL on command-stream/device failure.
        fn present_frame_gate(self: &HeliosDxvkDevice, timeout_us: u32, order_mode: u32) -> i32;
        fn flush_present_copy(self: &HeliosDxvkDevice) -> u64;
        fn wait_present_copy(self: &HeliosDxvkDevice, submission_id: u64, timeout_us: u32) -> i32;

        /// # Safety
        /// The three output pointers are live writable u32/u32/u64 storage for
        /// the duration of the call. `d3d11_resource_ptr` is a live D3D11
        /// resource COM pointer, as documented by the C++ bridge method.
        unsafe fn publish_present_order(
            self: &HeliosDxvkDevice,
            d3d11_resource_ptr: usize,
            out_ctx_id: *mut u32,
            out_value32: *mut u32,
            out_cookie: *mut u64,
        ) -> bool;

        /// D4a scanout acquire: hand the per-device KMD retirement event to
        /// the DXVK device's signaler thread (auto-reset HANDLE as usize,
        /// never 0). The UMD keeps ownership — DXVK only waits on it, and
        /// DestroyDevice closes it after the bridge device has dropped (the
        /// signaler joins inside ~DxvkDevice, so no waiter can outlive the
        /// handle).
        fn set_scanout_acquire_event(self: &HeliosDxvkDevice, event_handle: usize) -> bool;
        /// Dcomp present vehicle: image-level copy of the imported ICD frame
        /// (src) into the vehicle backbuffer texture (dst), sourcing the
        /// import's LIVE storage (staging alias when present). The copy-time
        /// consumer present-wait orders it against the producer's GPU
        /// writes. 0 = ok, 1 = copied with a (counted) geometry mismatch,
        /// negative = failure — fail the present loudly, do not flip.
        unsafe fn present_vehicle_copy(
            self: &HeliosDxvkDevice,
            dst_resource_ptr: usize,
            src_resource_ptr: usize,
            semaphore_handle: usize,
            semaphore_value: u64,
        ) -> i32;
        /// D4b snapshot ring: image-level copy of the presented primary (src)
        /// into a snapshot-ring image (dst), recorded on the open command
        /// list BEFORE the present-time Flush so it rides frame N's own
        /// command stream (queue-ordered after the frame's draws — no waits,
        /// no CPU stalls). Clone of `present_vehicle_copy` minus the
        /// staging-alias substitution: both operands are this device's own
        /// images, never imports. 0 = ok, 1 = copied with a (counted)
        /// geometry mismatch — the caller must NOT substitute the descriptor
        /// for this present — negative = failure (present exactly as today).
        unsafe fn present_snapshot_copy(
            self: &HeliosDxvkDevice,
            dst_resource_ptr: usize,
            src_resource_ptr: usize,
            windowed_blt_reservation: bool,
        ) -> i32;
        unsafe fn create_geometry_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        unsafe fn create_hull_shader(self: &HeliosDxvkDevice, code: *const u8, len: usize)
            -> usize;
        unsafe fn create_domain_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;
        unsafe fn create_compute_shader(
            self: &HeliosDxvkDevice,
            code: *const u8,
            len: usize,
        ) -> usize;

        /// Create a DXVK instance and logical device on the Helios venus adapter.
        ///
        /// `luid_low`/`luid_high` identify the WDDM adapter to match; pass `(0, 0)`
        /// to take the first enumerated adapter. Returns a null `UniquePtr` on
        /// failure (no adapter, device creation threw, etc.). Never panics across
        /// the FFI boundary — the C++ side catches all exceptions.
        fn helios_dxvk_create_device(
            luid_low: u32,
            luid_high: i32,
            timer_resolution: bool,
        ) -> UniquePtr<HeliosDxvkDevice>;
    }
}

// ---------------------------------------------------------------------------
// Safe wrappers: one owned/borrowed decision per bridge entry point.
// ---------------------------------------------------------------------------
//
// Thirteen bridge methods return a COM pointer as a bare `usize`. Two are
// BORROWED -- the bridge keeps the owning reference -- and eleven are OWNED and
// the Rust side must `Release`. Before R813 that discipline was a doc comment on
// one side and a `// SAFETY:` comment on the other, repeated at every call site.
// Every existing site was correct; the exposure was entirely future sites, and
// both failure modes are silent:
//
//   * adopting a borrowed pointer  -> a double release. `ID3D11Resource::
//     from_raw(dev.dxvk.d3d11_device_ptr() as *mut c_void)` is type-correct,
//     compiles, and drops the device's only reference at end of scope --
//     destroying the D3D11 device under a running DDI.
//   * wrapping an owned pointer in `ManuallyDrop` -> a leak.
//
// Each surfaces as a much later crash in dwm. The wrappers below make the
// correct adoption exist in exactly ONE place per entry point; R815 is what
// makes the wrong one unreachable.

use core::ffi::c_void;
use core::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Resource};

/// Adopt an owned COM pointer the bridge returned, or `None` for its 0-failure
/// sentinel. The single `from_raw` for every owning bridge entry point.
///
/// # Safety
/// `raw`, when non-zero, must be an `ID3D11Resource*` whose reference the
/// bridge transferred to this caller.
unsafe fn adopt_resource(raw: usize) -> Option<ID3D11Resource> {
    (raw != 0).then(|| unsafe { ID3D11Resource::from_raw(raw as *mut c_void) })
}

impl ffi::HeliosDxvkDevice {
    // -- borrowed ----------------------------------------------------------
    //
    // `ManuallyDrop` is the whole point: the bridge owns the reference, so the
    // returned wrapper must never release it.

    pub(crate) fn d3d11_device(&self) -> Option<ManuallyDrop<ID3D11Device>> {
        let p = self.d3d11_device_ptr();
        // SAFETY: a non-zero `d3d11_device_ptr` is the bridge's live
        // ID3D11Device, kept alive by the bridge for as long as this device
        // exists. ManuallyDrop borrows it without taking a reference.
        (p != 0).then(|| ManuallyDrop::new(unsafe { ID3D11Device::from_raw(p as *mut c_void) }))
    }

    pub(crate) fn d3d11_context(&self) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
        let p = self.d3d11_context_ptr();
        // SAFETY: as above, for the immediate context.
        (p != 0)
            .then(|| ManuallyDrop::new(unsafe { ID3D11DeviceContext::from_raw(p as *mut c_void) }))
    }

    // -- owned -------------------------------------------------------------

    /// # Safety
    /// Caller upholds `open_ddi_texture2d`'s preconditions (a live KMT handle
    /// and a renderer resource id the host still has). A nonzero source image
    /// pointer and its nested data remain live through this synchronous call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn open_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
        global: u32,
        renderer_resource_id: u32,
        venus_alloc_size: u64,
        memory_type_index: u32,
        global_vidmm_tracker: u64,
        scanout_linear: bool,
        linear_scanout_target: bool,
        cross_context_optimal: bool,
        dedicated_present_buffer: bool,
        source_image_create_info: usize,
        source_external_ownership: bool,
        foreign: Option<ForeignLayout>,
    ) -> Option<ID3D11Resource> {
        // SAFETY: the caller upholds the resource-id/handle preconditions
        // above, and the bridge transfers one reference on success.
        unsafe {
            adopt_resource(self.open_ddi_texture2d(
                width,
                height,
                format,
                bind_flags,
                misc_flags,
                global,
                renderer_resource_id,
                venus_alloc_size,
                memory_type_index,
                global_vidmm_tracker,
                scanout_linear,
                linear_scanout_target,
                cross_context_optimal,
                dedicated_present_buffer,
                source_image_create_info,
                source_external_ownership,
                foreign.is_some(),
                foreign.map_or(0, |f| f.modifier),
                foreign.map_or(0, |f| f.stride),
                foreign.map_or(0, |f| f.offset),
                foreign.map_or(0, |f| f.plane1_modifier),
                foreign.map_or(0, |f| f.plane1_stride),
                foreign.map_or(0, |f| f.plane1_offset),
            ))
        }
    }

    /// The ICD under this device.
    pub(crate) fn backend(&self) -> IcdBackend {
        if self.icd_backend() == 2 {
            IcdBackend::NvkRm
        } else {
            IcdBackend::Venus
        }
    }

    /// NVK: the KMD resource id and layout of a WDDM-backed texture.
    pub(crate) fn foreign_identity(&self, res: &ID3D11Resource) -> Option<ForeignIdentity> {
        let mut id = ForeignIdentity::default();
        // SAFETY: `res` is a live resource borrowed for the call; every output
        // points at a field of the local `id`.
        let ok = unsafe {
            self.get_resource_foreign_identity(
                res.as_raw() as usize,
                &mut id.resource_id,
                &mut id.ctx_id,
                &mut id.size,
                &mut id.layout.modifier,
                &mut id.layout.stride,
                &mut id.layout.offset,
                &mut id.layout.fourcc,
                &mut id.layout.plane1_modifier,
                &mut id.layout.plane1_stride,
                &mut id.layout.plane1_offset,
            )
        };
        (ok && id.resource_id != 0 && id.ctx_id != 0).then_some(id)
    }

    /// A dedicated OPTIMAL, DMA_BUF-exportable image, plus its logical
    /// scan-out metadata. `kmd_transfer_source` selects the canonical GENERAL
    /// layout required by the KMD transfer importer.
    pub(crate) fn create_scanout_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
        kmd_transfer_source: bool,
    ) -> Option<(ID3D11Resource, u64, u64)> {
        let mut row_pitch: u64 = 0;
        let mut offset: u64 = 0;
        // SAFETY: both out-params point at live locals; the bridge zeroes them
        // on entry and writes them before returning non-zero.
        let raw = unsafe {
            self.create_ddi_scanout_texture2d(
                width,
                height,
                format,
                bind_flags,
                misc_flags,
                kmd_transfer_source,
                &mut row_pitch,
                &mut offset,
            )
        };
        // SAFETY: the bridge transfers one reference on success.
        unsafe { adopt_resource(raw) }.map(|r| (r, row_pitch, offset))
    }
}

/// The Vulkan ICD a device runs on (`helios_icd_interface.h`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IcdBackend {
    Venus,
    NvkRm,
}

/// What lies in an NVK-made (foreign) resource: plane 0, and plane 1 of a
/// two-plane format (NV12/P010/P016), as the KMD records it
/// (`HeliosWddmAllocLayout`, `HeliosWddmAllocPlane`; docs/shared-formats.md).
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct ForeignLayout {
    pub(crate) modifier: u64,
    pub(crate) stride: u32,
    pub(crate) offset: u32,
    pub(crate) fourcc: u32,
    /// Plane 1; `plane1_stride` 0 for a single-plane resource.
    pub(crate) plane1_modifier: u64,
    pub(crate) plane1_stride: u32,
    pub(crate) plane1_offset: u32,
}

/// `mode` of [`BridgeDevice::flush_gate_point`] (dxvk_bridge.h kFlushGate*).
pub(crate) const FLUSH_GATE_STREAM: u32 = 0;
pub(crate) const FLUSH_GATE_WIRE: u32 = 1;
pub(crate) const FLUSH_GATE_RM_FENCE: u32 = 2;

/// What a flush gate carries (docs/flush-gate.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlushGatePoint {
    /// Nothing recorded since the previous gate: send no packet.
    Nothing,
    /// Ready: a stream point (`ctx`, `value`, `cookie`; STREAM mode), an RM
    /// fence the caller now owns (`fence`; RM_FENCE mode), or neither (WIRE
    /// mode: the work reached the transport).
    Ready { ctx: u32, value: u32, cookie: u64, fence: u32, fence_value: u64 },
    /// This mode cannot be served here: fall back.
    Unavailable,
    /// The command stream or submission failed.
    Failed,
}

/// A foreign resource id minted for one texture.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct ForeignIdentity {
    pub(crate) resource_id: u32,
    pub(crate) ctx_id: u32,
    pub(crate) size: u64,
    pub(crate) layout: ForeignLayout,
}

// NOT wrapped, deliberately: the eight shader creates.
//
// R813 suggests an `Option<usize>` (or NonZero) wrapper for them too. Audited
// instead: all TEN shader-create call sites already guard the bridge's
// 0-failure sentinel with `if raw != 0` before `store_raw_com`, and storing a
// zero would be harmless anyway -- `load_com` null-checks the slot. There is no
// wrong-adoption hazard here either, because the result goes into a slot as a
// raw word rather than being wrapped as owned or borrowed. A newtype would be
// ceremony across ten correct sites, so it is left out by the review's own
// "rejected as cosmetic" standard.

// ---------------------------------------------------------------------------
// BridgeDevice: the sealed public API (R815)
// ---------------------------------------------------------------------------
//
// After R813 the safe wrappers exist, but the raw `usize`-returning methods
// remain callable at every site, so nothing prevents a future caller choosing
// the wrong adoption. Module privacy alone is NOT sufficient: cxx generates the
// raw methods as INHERENT methods on the public opaque type, and inherent
// methods of a re-exported public type stay callable regardless of module
// visibility. A newtype with no `Deref` is the only encoding that actually
// seals them -- which is why `BridgeDevice` deliberately has none, and why
// `inner` is private.
//
// The C++ side still returns `usize`, so the ABI is unchanged and this
// migration cannot break the wire.

/// A **source** resource pointer for the present path.
///
/// `present_vehicle_copy(dst, src)` took the same two COM pointers in the
/// OPPOSITE order to the neighbouring `present_sync_publish(src, dst)` (retired
/// in R912a), they were called ~30 lines apart in the same function, and a
/// transposition compiled cleanly on both sides of the FFI. Transposing
/// `present_vehicle_copy` is ALWAYS harmful -- the bridge copies the vehicle
/// backbuffer INTO the imported ICD frame, the geometry check passes because
/// both are the same size, `EXT_GEOM_MISMATCH` never fires, and the flipped
/// backbuffer shows whatever it held last frame. That is exactly the
/// stale-frame symptom class this project has already spent multiple sessions
/// chasing.
///
/// With `SrcRes`/`DstRes` the transposition is a type error regardless of
/// parameter order. R816.
#[derive(Clone, Copy)]
pub(crate) struct SrcRes(pub(crate) usize);

/// A **destination** resource pointer for the present path. See [`SrcRes`].
#[derive(Clone, Copy)]
pub(crate) struct DstRes(pub(crate) usize);

/// The optional KMD correlation for one ordinary present's exact producer
/// submission. Zero is the only fallback representation: callers cannot carry
/// a partial `{ctx,value,cookie}` into a KMD marker.
#[derive(Clone, Copy, Default)]
pub(crate) struct PresentStreamCorrelation {
    pub(crate) ctx_id: u32,
    pub(crate) value32: u32,
    pub(crate) cookie: u64,
    /// NVK on RM (S4): an RM fence handle the present retires on instead of a
    /// stream point (`helios_rm_fence.h` tail; exclusive with the three fields
    /// above, which are then zero). 0 = none. The KMD takes the handle when
    /// it attaches the marker.
    pub(crate) rm_fence_handle: u32,
    /// Diagnostic only: the timeline value behind `rm_fence_handle`.
    pub(crate) rm_fence_value: u64,
    /// NVK: the frame is already on scanout 0 through the user foreign-scanout
    /// source: the `HERF` marker carries the `HOSC` tag (`helios_onscanout.h`)
    /// so the KMD can complete the Blt present without copying. `None` = no
    /// claim (the ordinary Blt).
    pub(crate) on_scanout: Option<OnScanoutClaim>,
}

/// The already-on-scanout claim: the KMD's own names for the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OnScanoutClaim {
    /// `out_seq` of the frame's `SCANOUT_PRESENT`. Nonzero.
    pub(crate) sequence: u64,
    /// `out_generation` of the live source's `SCANOUT_SET`. Nonzero.
    pub(crate) generation: u32,
    /// Helios resource id of the presented allocation, 0 = not stated.
    pub(crate) resource_id: u32,
}

impl PresentStreamCorrelation {
    pub(crate) fn is_complete(self) -> bool {
        self.ctx_id != 0 && self.value32 != 0 && self.cookie != 0
    }
}

/// The DXVK bridge device, with the raw cxx surface sealed off.
pub struct BridgeDevice {
    inner: cxx::UniquePtr<ffi::HeliosDxvkDevice>,
}

impl BridgeDevice {
    /// Create a DXVK instance and logical device on the Helios venus adapter.
    /// `None` when the bridge returned a null device (no adapter, creation
    /// threw, ...) -- folding the old `is_null()` check into construction so a
    /// `BridgeDevice` that exists is always usable.
    pub fn create(luid_low: u32, luid_high: i32) -> Option<Self> {
        let inner = ffi::helios_dxvk_create_device(
            luid_low,
            luid_high,
            crate::knobs::UMD_TIMER_RESOLUTION.get(),
        );
        (!inner.is_null()).then_some(Self { inner })
    }

    /// The only path from the newtype to the sealed type, and it is private.
    fn get(&self) -> Option<&ffi::HeliosDxvkDevice> {
        self.inner.as_ref()
    }

    // -- borrowed COM ------------------------------------------------------

    pub(crate) fn d3d11_device(&self) -> Option<ManuallyDrop<ID3D11Device>> {
        self.get()?.d3d11_device()
    }

    pub(crate) fn d3d11_context(&self) -> Option<ManuallyDrop<ID3D11DeviceContext>> {
        self.get()?.d3d11_context()
    }

    /// # Safety
    /// `deferred_context_ptr` must be a live DXVK deferred interface.
    /// `command_list_ptr` transfers one owned COM reference iff this returns
    /// true; false leaves it owned by the caller.
    pub(crate) unsafe fn recycle_deferred_command_list(
        &self,
        deferred_context_ptr: usize,
        command_list_ptr: usize,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.recycle_deferred_command_list(deferred_context_ptr, command_list_ptr)
        })
    }

    /// # Safety
    /// `deferred_context_ptr` must be the newly-created live DXVK deferred
    /// context owned by this UMD device.
    pub(crate) unsafe fn enable_deferred_context_ddi_logical_reset(
        &self,
        deferred_context_ptr: usize,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.enable_deferred_context_ddi_logical_reset(deferred_context_ptr)
        })
    }

    // -- owned COM ---------------------------------------------------------

    /// # Safety
    /// See [`ffi::HeliosDxvkDevice::open_texture2d`].
    /// A nonzero source image pointer is borrowed through this call only.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn open_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
        global: u32,
        renderer_resource_id: u32,
        venus_alloc_size: u64,
        memory_type_index: u32,
        global_vidmm_tracker: u64,
        scanout_linear: bool,
        linear_scanout_target: bool,
        cross_context_optimal: bool,
        dedicated_present_buffer: bool,
        source_image_create_info: usize,
        source_external_ownership: bool,
        foreign: Option<ForeignLayout>,
    ) -> Option<ID3D11Resource> {
        // SAFETY: the caller retains the resource and any source template
        // through the synchronous native import, which copies nested metadata.
        unsafe {
            self.get()?.open_texture2d(
                width,
                height,
                format,
                bind_flags,
                misc_flags,
                global,
                renderer_resource_id,
                venus_alloc_size,
                memory_type_index,
                global_vidmm_tracker,
                scanout_linear,
                linear_scanout_target,
                cross_context_optimal,
                dedicated_present_buffer,
                source_image_create_info,
                source_external_ownership,
                foreign,
            )
        }
    }

    /// The ICD under this device (Venus when there is no bridge device).
    pub(crate) fn backend(&self) -> IcdBackend {
        self.get().map_or(IcdBackend::Venus, |d| d.backend())
    }

    pub(crate) fn is_nvk(&self) -> bool {
        self.backend() == IcdBackend::NvkRm
    }

    /// NVK: the KMD resource id and layout of a WDDM-backed texture.
    pub(crate) fn foreign_identity(&self, res: &ID3D11Resource) -> Option<ForeignIdentity> {
        self.get()?.foreign_identity(res)
    }

    /// NVK: show `res` on scanout 0. True if shown.
    pub(crate) fn nvk_scanout_present(&self, res: &ID3D11Resource) -> bool {
        // SAFETY: `res` is a live resource borrowed for the call.
        self.get()
            .is_some_and(|d| unsafe { d.nvk_scanout_present(res.as_raw() as usize) } == 0)
    }

    pub(crate) fn nvk_scanout_release(&self) {
        if let Some(d) = self.get() {
            d.nvk_scanout_release();
        }
    }

    /// NVK: `HELIOS_ICD_CAP_*` (`helios_icd_interface.h`), 0 on Venus.
    pub(crate) fn nvk_icd_caps(&self) -> u32 {
        self.get().map_or(0, |d| d.nvk_icd_caps())
    }

    /// NVK RM fences (S4): flip `res` once the GPU has finished everything
    /// submitted so far, no CPU wait. `None` when RM fences cannot be had
    /// here (the caller waits and calls [`Self::nvk_scanout_present`]);
    /// `Some(shown)` otherwise.
    pub(crate) fn nvk_scanout_present_fenced(&self, res: &ID3D11Resource) -> Option<bool> {
        let d = self.get()?;
        // SAFETY: `res` is a live resource borrowed for the call.
        match unsafe { d.nvk_scanout_present_fenced(res.as_raw() as usize) } {
            0 => Some(true),
            1 => None,
            _ => Some(false),
        }
    }

    /// NVK: `(sequence, generation)` of `res`'s latest scanout frame, as the
    /// KMD minted them (already-on-scanout present tag).
    pub(crate) fn nvk_scanout_frame(&self, res: &ID3D11Resource) -> Option<(u64, u32)> {
        let d = self.get()?;
        let (mut seq, mut generation) = (0u64, 0u32);
        // SAFETY: `res` is borrowed live for the call; both out-pointers borrow locals.
        unsafe { d.nvk_scanout_frame(res.as_raw() as usize, &mut seq, &mut generation) }
            .then_some((seq, generation))
    }

    /// NVK RM fences (S4): a fence (handle, diagnostic timeline value) for
    /// everything submitted so far; the caller owns the handle.
    pub(crate) fn nvk_present_fence(&self) -> Option<(u32, u64)> {
        let d = self.get()?;
        let (mut fence, mut value) = (0u32, 0u64);
        // SAFETY: both out-pointers borrow live locals for this synchronous call.
        let r = unsafe { d.nvk_present_fence(&mut fence, &mut value) };
        (r == 0 && fence != 0).then_some((fence, value))
    }

    pub(crate) fn nvk_rm_fence_close(&self, fence: u32) {
        if let Some(d) = self.get() {
            d.nvk_rm_fence_close(fence);
        }
    }

    /// Hand-off ledger: `res` (a cross-process shared resource) gets its key.
    pub(crate) fn handoff_register(&self, res: usize) {
        if let Some(d) = self.get() {
            // SAFETY: the caller passes a live resource pointer.
            unsafe { d.handoff_register(res) };
        }
    }

    /// NVK: `res` is the source of a composed Present (see the bridge
    /// declaration). Returns the ledger id, 0 when it has none.
    pub(crate) fn mark_blt_source(&self, res: usize) -> u32 {
        match self.get() {
            // SAFETY: the caller passes a live resource pointer.
            Some(d) => unsafe { d.mark_blt_source(res) },
            None => 0,
        }
    }

    /// Hand-off ledger: `res` goes; this process stops holding its key.
    pub(crate) fn handoff_unregister(&self, res: usize) {
        if let Some(d) = self.get() {
            // SAFETY: the caller passes a live resource pointer.
            unsafe { d.handoff_unregister(res) };
        }
    }

    /// Hand-off ledger: publish this device's next point on `resources`.
    pub(crate) fn handoff_publish(&self, resources: &[usize]) -> i32 {
        let Some(d) = self.get() else {
            return -1;
        };
        // SAFETY: the slice borrows live resource pointers for the call.
        unsafe { d.handoff_publish(resources.as_ptr(), resources.len() as u32) }
    }

    /// Flush gate: flush and get what the HEFL packet carries (see
    /// [`FlushGatePoint`]). `mode` is one of `FLUSH_GATE_*`.
    pub(crate) fn flush_gate_point(&self, mode: u32, publish: &[usize]) -> FlushGatePoint {
        let Some(d) = self.get() else {
            return FlushGatePoint::Unavailable;
        };
        let (mut ctx, mut value, mut cookie, mut fence, mut fence_value) = (0u32, 0u32, 0u64, 0u32, 0u64);
        // SAFETY: every out-pointer borrows a live local for this synchronous call.
        let r = unsafe {
            d.flush_gate_point(
                mode,
                publish.as_ptr(),
                publish.len() as u32,
                &mut ctx,
                &mut value,
                &mut cookie,
                &mut fence,
                &mut fence_value,
            )
        };
        match r {
            0 => FlushGatePoint::Nothing,
            1 => FlushGatePoint::Ready { ctx, value, cookie, fence, fence_value },
            -1 => FlushGatePoint::Unavailable,
            _ => FlushGatePoint::Failed,
        }
    }

    pub(crate) fn create_scanout_texture2d(
        &self,
        width: u32,
        height: u32,
        format: u32,
        bind_flags: u32,
        misc_flags: u32,
        kmd_transfer_source: bool,
    ) -> Option<(ID3D11Resource, u64, u64)> {
        self.get()?.create_scanout_texture2d(
            width,
            height,
            format,
            bind_flags,
            misc_flags,
            kmd_transfer_source,
        )
    }

    // -- scalar passthroughs ----------------------------------------------

    pub(crate) fn venus_context_id(&self) -> u32 {
        self.get().map_or(0, |d| d.venus_context_id())
    }

    pub(crate) fn feed_trace_timestamp_ns(&self) -> u64 {
        self.get().map_or(0, |d| d.feed_trace_timestamp_ns())
    }

    pub(crate) fn feed_trace_render_callback(&self, duration_ns: u64) {
        if let Some(d) = self.get() {
            d.feed_trace_render_callback(duration_ns);
        }
    }

    pub(crate) fn feed_trace_present_callback(&self, duration_ns: u64) {
        if let Some(d) = self.get() {
            d.feed_trace_present_callback(duration_ns);
        }
    }

    pub(crate) fn present_frame_gate(&self, timeout_us: u32, order_mode: u32) -> i32 {
        self.get().map_or(crate::hr::E_FAIL, |d| {
            d.present_frame_gate(timeout_us, order_mode)
        })
    }

    pub(crate) fn flush_present_copy(&self) -> u64 {
        self.get().map_or(0, |d| d.flush_present_copy())
    }

    pub(crate) fn wait_present_copy(&self, submission_id: u64, timeout_us: u32) -> i32 {
        self.get()
            .map_or(-1, |d| d.wait_present_copy(submission_id, timeout_us))
    }

    /// Publish this present on the device's named timeline so a consumer can
    /// order its read GPU-side. `d3d11_resource_ptr` is the presented source
    /// resource's COM pointer; the bridge derives the venus resource id the
    /// consumer imports by. Slot publication remains the boolean result; the
    /// optional stream correlation is fully zero unless all three fields name
    /// this exact signal.
    pub(crate) fn publish_present_order(
        &self,
        d3d11_resource_ptr: usize,
    ) -> (bool, PresentStreamCorrelation) {
        let Some(d) = self.get() else {
            return (false, PresentStreamCorrelation::default());
        };
        let mut correlation = PresentStreamCorrelation::default();
        // SAFETY: `d` is the bridge-owned live C++ device and all three
        // out-pointers borrow initialized local fields for this synchronous
        // call. The C++ bridge zeroes them before reporting failure.
        let published = unsafe {
            d.publish_present_order(
                d3d11_resource_ptr,
                &mut correlation.ctx_id,
                &mut correlation.value32,
                &mut correlation.cookie,
            )
        };
        if !published || !correlation.is_complete() {
            correlation = PresentStreamCorrelation::default();
        }
        (published, correlation)
    }

    /// D4a scanout acquire: deliver this device's KMD retirement event handle
    /// to the DXVK signaler. See the ffi declaration for the ownership rule.
    pub(crate) fn set_scanout_acquire_event(&self, event_handle: usize) -> bool {
        self.get()
            .is_some_and(|d| d.set_scanout_acquire_event(event_handle))
    }

    // -- pointer-laundering passthroughs -----------------------------------
    //
    // `unsafe` for the reason R814 established: each hands the bridge a raw
    // address it reinterpret_casts.

    /// # Safety
    /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`.
    pub(crate) unsafe fn set_resource_kmt_handles(
        &self,
        d3d11_resource_ptr: usize,
        local: u32,
        global: u32,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.set_resource_kmt_handles(d3d11_resource_ptr, local, global)
        })
    }

    /// # Safety
    /// `d3d11_resource_ptr` must be live; the out-params must be writable.
    pub(crate) unsafe fn get_resource_memory_info(
        &self,
        d3d11_resource_ptr: usize,
        memory: *mut u64,
        size: *mut u64,
        offset: *mut u64,
        resource_id: *mut u32,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.get_resource_memory_info(d3d11_resource_ptr, memory, size, offset, resource_id)
        })
    }

    /// # Safety
    /// `d3d11_resource_ptr` must be live; the out-params must be writable.
    pub(crate) unsafe fn get_resource_alloc_identity(
        &self,
        d3d11_resource_ptr: usize,
        venus_alloc_size: *mut u64,
        memory_type_index: *mut u32,
        global_vidmm_tracker: *mut u64,
    ) -> bool {
        self.get().is_some_and(|d| unsafe {
            d.get_resource_alloc_identity(
                d3d11_resource_ptr,
                venus_alloc_size,
                memory_type_index,
                global_vidmm_tracker,
            )
        })
    }

    /// # Safety
    /// `d3d11_resource_ptr` must be a live `ID3D11Resource*`.
    pub(crate) unsafe fn transfer_resource_ownership(&self, d3d11_resource_ptr: usize) -> bool {
        self.get()
            .is_some_and(|d| unsafe { d.transfer_resource_ownership(d3d11_resource_ptr) })
    }

    /// # Safety
    /// `d3d11_resource_ptrs` must point at `count` live `ID3D11Resource*`.
    pub(crate) unsafe fn rotate_resource_backings(
        &self,
        d3d11_resource_ptrs: *const usize,
        count: usize,
    ) -> bool {
        self.get()
            .is_some_and(|d| unsafe { d.rotate_resource_backings(d3d11_resource_ptrs, count) })
    }

    /// # Safety
    /// Both resource words must be live `ID3D11Resource*` values belonging to
    /// this bridge device. Subresource and region bounds are validated again
    /// by the C++ side before it records any work.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn dxgi_blt_convert(
        &self,
        dst: DstRes,
        dst_subresource: u32,
        dst_x: u32,
        dst_y: u32,
        src: SrcRes,
        src_subresource: u32,
        use_src_box: bool,
        src_left: u32,
        src_top: u32,
        src_right: u32,
        src_bottom: u32,
    ) -> i32 {
        self.get().map_or(-1, |d| unsafe {
            d.dxgi_blt_convert(
                dst.0,
                dst_subresource,
                dst_x,
                dst_y,
                src.0,
                src_subresource,
                use_src_box,
                src_left,
                src_top,
                src_right,
                src_bottom,
            )
        })
    }

    /// # Safety
    /// Both pointers must be live `ID3D11Resource*`.
    pub(crate) unsafe fn present_vehicle_copy(
        &self,
        dst: DstRes,
        src: SrcRes,
        semaphore: usize,
        value: u64,
    ) -> i32 {
        // C++ order here is (dst, src) -- the opposite of publish above, which
        // is the whole hazard. The named types mean the two orders no longer
        // have to agree for the call to be correct.
        self.get().map_or(-1, |d| unsafe {
            d.present_vehicle_copy(dst.0, src.0, semaphore, value)
        })
    }

    /// D4b snapshot blit: S_i <- presented primary, recorded before the
    /// present-time Flush. See the ffi declaration for the return contract.
    ///
    /// # Safety
    /// Both pointers must be live `ID3D11Resource*`.
    pub(crate) unsafe fn present_snapshot_copy(
        &self,
        dst: DstRes,
        src: SrcRes,
        windowed_blt_reservation: bool,
    ) -> i32 {
        // Same (dst, src) C++ order and the same transposition hazard as
        // `present_vehicle_copy`; the newtypes are what make it a type error.
        self.get().map_or(-1, |d| unsafe {
            d.present_snapshot_copy(dst.0, src.0, windowed_blt_reservation)
        })
    }

    // -- shader creates ----------------------------------------------------
    //
    // These return the bridge's owned COM pointer as a raw word because the IA
    // caches key on the pointer VALUE; see the note above on why they are not
    // additionally newtyped.

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_vertex_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_vertex_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_pixel_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_pixel_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_geometry_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_geometry_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_hull_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_hull_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_domain_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_domain_shader(code, len) })
    }

    /// # Safety
    /// `code` must point at `len` readable bytes.
    pub(crate) unsafe fn create_compute_shader(&self, code: *const u8, len: usize) -> usize {
        self.get()
            .map_or(0, |d| unsafe { d.create_compute_shader(code, len) })
    }

    /// # Safety
    /// `code`/`sig_words` must point at `len`/`sig_words_len` readable items.
    pub(crate) unsafe fn create_shader_sig(
        &self,
        kind: u32,
        code: *const u8,
        len: usize,
        sig_words: *const u32,
        sig_words_len: usize,
    ) -> usize {
        self.get().map_or(0, |d| unsafe {
            d.create_shader_sig(kind, code, len, sig_words, sig_words_len)
        })
    }

    /// # Safety
    /// `code`/`sig_words` must point at `len`/`sig_words_len` readable items.
    pub(crate) unsafe fn create_tess_shader_sig(
        &self,
        kind: u32,
        code: *const u8,
        len: usize,
        sig_words: *const u32,
        sig_words_len: usize,
    ) -> usize {
        self.get().map_or(0, |d| unsafe {
            d.create_tess_shader_sig(kind, code, len, sig_words, sig_words_len)
        })
    }
}

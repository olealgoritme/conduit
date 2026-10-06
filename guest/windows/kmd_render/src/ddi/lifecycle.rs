//! Adapter PnP / power lifecycle DDIs.
//!
//! `StartDevice` saves the Dxgkrnl interface, initializes virtio-gpu, and
//! reports either the production one-source/one-child display topology or the
//! explicit knob-off render-only recovery topology.
//!
//! Moved verbatim out of `ddi/lifecycle.rs` by T8/R1102.

use alloc::boxed::Box;
use core::ffi::c_void;

use crate::adapter::AdapterContext;
use crate::dxgk::*;

use super::bar_segment::{build_segment_table, setup_bar_segment};

/// Same-boot zeroing for the PRODUCTION LINEAR-scanout ladder (T6/R901).
///
/// These eight names are written by `venus::create_linear_scanout_image` /
/// `allocate_linear_scanout_image_blob` -- the KMD's real display fallback,
/// which stays -- but their only zeroing used to live inside the deleted
/// `scanout_diag::maybe_run`. Registry value names PERSIST ACROSS BOOTS, so
/// without this a stale prior-boot `SdgLStg = 0x10` survives into a boot where
/// the fallback is never taken and reads as though it had been: exactly the
/// cross-boot counter trap the evidence rules forbid.
///
/// ⚠ `#[inline(never)]` is a STACK BUDGET decision, not style -- the same rule
/// `bring_up_venus` below is annotated for. The eight-element name array is ~64
/// bytes of locals, and inlined into `dxgkddi_start_device` it measurably grew
/// that frame (8424 -> 8456 bytes measured with `tools/kmd-frame-sizes.ps1`),
/// eating 32 of the 352 bytes of headroom on the 24 KB kernel boot stack. In its
/// own transient frame -- which does not overlap `VirtioGpu::init` -- it costs
/// the budget nothing. Overflow here is `0xc0000001`/Startup Repair with no dump
/// and no bugcheck event, and it does NOT reproduce on a live devcon restart.
#[inline(never)]
fn zero_linear_scanout_breadcrumbs() {
    for name in [
        b"SdgLStg", b"SdgLReq", b"SdgLBit", b"SdgLTyc", b"SdgLImg", b"SdgLMem", b"SdgLPch",
        b"SdgLOff",
    ] {
        crate::diag::record_named_bytes(name, 0);
    }
}

/// Resolve the display half's scan-out mode and EDID: the host's EDID when it
/// served one (size AND refresh rate, DisplayID extension included), else the
/// host's `GET_DISPLAY_INFO` size with a generated EDID, else the 1920x1080
/// fallback. `None` only if no valid EDID could be built at all.
///
/// ⚠ `#[inline(never)]` is a STACK BUDGET decision, like `bring_up_venus`: the
/// 256-byte EDID copy below must not widen `dxgkddi_start_device`'s frame, which
/// is already shared with `VirtioGpu::init` on the 24 KB boot stack. This runs
/// after `init` has returned, so its frame never overlaps init's.
#[inline(never)]
fn resolve_scanout_mode(
    adapter: &AdapterContext,
    display_half: bool,
) -> Option<crate::adapter::ScanoutMode> {
    // The render-only surface advertises no monitor, so its EDID is empty and
    // QueryDeviceDescriptor answers NOT_SUPPORTED before ever reading it.
    if !display_half {
        return Some(crate::adapter::ScanoutMode::render_only());
    }
    // The host's EDID, copied out of the transport so the registry writes in
    // `adopt` run at PASSIVE outside the virtio spinlock.
    let mut host_edid_buf = [0u8; 256];
    let host_edid_len = match adapter.with_virtio(|v| {
        let e = v.host_edid()?;
        host_edid_buf.get_mut(..e.len())?.copy_from_slice(e);
        Some(e.len())
    }) {
        Ok(Some(n)) => n,
        _ => 0,
    };
    let host_edid = host_edid_buf.get(..host_edid_len).filter(|e| !e.is_empty());

    // Adopt the host's scanout-0 size (GET_DISPLAY_INFO, captured at transport
    // init) as the VidPn mode + generated-EDID native resolution, so Helios
    // presents the size QEMU actually wants on scanout 0. Falls back to the
    // default in `display_mode()` if the host reported nothing usable.
    //
    // The two failure arms are NOT the same thing and neither is benign: the
    // fallback fabricates a mode, so the OS is handed an EDID for a monitor
    // whose size we invented. Distinguish them - `Err` means the transport is
    // gone (and therefore nothing can ever scan out), `Ok(None)` means the
    // host answered but reported nothing usable.
    let mut host_mode = None;
    match adapter.with_virtio(|v| v.display_mode()) {
        Ok(Some((w, h))) => {
            host_mode = Some((w, h));
        }
        Ok(None) => {
            crate::diag::fault(crate::diag::FaultCounter::StMdB, 1);
        }
        Err(e) => {
            let status: NTSTATUS = e.into();
            crate::diag::fault(crate::diag::FaultCounter::StTxG, status as u32);
        }
    }
    // ONE value: the constructor validates the extent and generates (or adopts)
    // the matching EDID, so the two cannot disagree.
    let mode = crate::adapter::ScanoutMode::adopt(host_mode, host_edid);
    if mode.is_none() {
        // Invalid identity or fallback metadata must not become a zero EDID
        // attached to a supposedly working display child.
        crate::diag::record_named_bytes(b"EdidBuildFailed", 1);
    }
    mode
}

/// Stand up the persistent venus context + page-table blob. Returns the venus
/// context id, or 0 on any failure.
///
/// It used to also return the blob's `(gpa, size)` window, which was stored in
/// the transport generation and read by nobody — `query_segments` deliberately
/// reports `paging_ram` instead, because QuerySegment4 runs BEFORE this
/// allocation exists. R510 annotated that field write-only and left the
/// deletion to a dead-code commit with its own reachability evidence; this is
/// that commit (2026-08-05). The blob itself is still allocated and still owned
/// by the venus client — only the unread copy of its address is gone.
///
/// ⚠ `#[inline(never)]` is a STACK BUDGET decision, not style. `VenusClient` and
/// the blob descriptors are large locals, and StartDevice's frame is already
/// shared with `VirtioGpu::init`'s 3.0 KB one on a 24 KB kernel stack. Keeping
/// these in their own transient frame — which does not overlap
/// `VirtioGpu::init` — is what keeps the nested peak inside the budget. See
/// `StartedState::boxed` for the boot failure this class of growth caused.
#[inline(never)]
fn bring_up_venus(passive: crate::irql::PassiveLevel, adapter: &AdapterContext) -> u32 {
    // Persistent venus context for the device lifetime (owner 0: KMD-internal,
    // destroyed explicitly in StopDevice).
    let venus_result = crate::virtio::ctrl::ctx_create(
        passive,
        adapter,
        helios_protocol::VIRTIO_GPU_CAPSET_VENUS,
        None,
    )
    .and_then(|ctx_id| {
        let (client, _blob) =
            crate::virtio::venus::allocate_host_visible_blob(passive, adapter, ctx_id)?;
        Ok((ctx_id, client))
    });
    match venus_result {
        Ok((ctx_id, client)) => {
            crate::diag::record(0x0B00_0007);
            adapter.set_venus_client(Some(client));
            ctx_id
        }
        Err(e) => {
            // venus bring-up failed; transport is up but no page-table window.
            let status: NTSTATUS = e.into();
            crate::diag::record(0x0B00_00E7);
            crate::diag::record(status as u32);
            crate::diag::fault(crate::diag::FaultCounter::StVnu, status as u32);
            adapter.set_venus_client(None);
            0
        }
    }
}

/// `StartStg`: how far StartDevice got. Always on (registry named value, not the
/// DiagLevel-gated ring), so a bugcheck or a failed start inside it still says where.
///   1 entry   2 transport init done (ok or failed)   3 started state published
///   4 exit (the value a successful start leaves)
fn start_stage(stage: u32) {
    crate::diag::record_named_bytes(b"StartStg", stage);
}

/// `StopStg` / `StopMs`: how far StopDevice got and how long it had been running.
/// Always on, for the same reason as [`start_stage`].
///   1 entry
///   2 (unused: the paging-quiesce stage of an abandoned design; the ISR gate is
///     NOT moved either, so the stage order below is the code order)
///   3 ISR gate cleared      4 vsync + HPD stopped     5 KMD blobs released
///   6 venus context destroyed   7 parked entries reaped
///   8 host sweep done       9 transport dropped       10 exit
fn stop_stage(entry_100ns: u64, stage: u32) {
    crate::diag::record_named_bytes(b"StopStg", stage);
    crate::diag::record_named_bytes(
        b"StopMs",
        helios_kmd_logic::sweep_budget::elapsed_ms(
            entry_100ns,
            crate::adapter::foreign_scanout::now_100ns(),
        ),
    );
}

/// Give the time since `from_100ns` back to `budget`: it was spent on something
/// that is not a host command (a worker join, a hive flush).
fn stop_credit(
    budget: helios_kmd_logic::sweep_budget::SweepBudget,
    from_100ns: u64,
) -> helios_kmd_logic::sweep_budget::SweepBudget {
    budget.credit(crate::adapter::foreign_scanout::now_100ns().saturating_sub(from_100ns))
}

/// Knobs cached in statics that outlive a `pnputil /restart-device` (the image is not reloaded),
/// read again at EVERY StartDevice and mirrored in the service key with the value in force, 0
/// included: `DiagLevel` (`DiagLvl`), `NvDupHarden` (`NvDupMode`), `NvSpinUs`. The per-transport
/// knobs (`ForeignFlip`, `KmdRmClient`, `KmdRmSysPollMs`) are reset by `retire_transport` and
/// re-read by [`start_generation_mirrors`]; the rest are read by `AdapterKnobs::read_at_start`
/// or at transport init. The table: `docs/zero-copy-present.md` section 13.8. PASSIVE.
#[inline(never)]
fn reread_cached_knobs() {
    let _ = crate::diag::reread_level();
    let _ = crate::virtio::nvrm_harden::reread_mode();
    let _ = crate::virtio::ctrl::reread_spin_knob();
    // `FlipWdogMs` and `DeferBudget` (`FlWdMsEff`, `DefBudEff`): 0 = off, today's behaviour.
    crate::ddi::stall_diag::reread_knobs();
    // `EscWaitMs`, and the stopping flag back down: a new generation begins (v334).
    crate::ddi::escape_wait::reread_knobs();
    // `RmGateMs` (default 6000, 0 = never): the RM gates' lost-fire valve, mirrored `RmGateMsEff`.
    crate::virtio::gpu::RMG_EXPIRE_MS.store(
        helios_kmd_logic::rm_fence_present::clamp_gate_expire_ms(crate::diag::read_config_dword(
            crate::diag::knobs::RM_GATE_MS,
            helios_kmd_logic::rm_fence_present::GATE_EXPIRE_DEFAULT_MS,
        )),
        core::sync::atomic::Ordering::Relaxed,
    );
}

/// After the previous transport's state was forgotten (`retire_transport`): the new generation's
/// per-transport knobs are read and mirrored now, and the event-gated counter blocks are zeroed
/// in the service key (and in their statics), so a value an earlier run left there is never read
/// as this generation's. PASSIVE.
#[inline(never)]
fn start_generation_mirrors() {
    let _ = crate::virtio::rm_client::reread_knob_at_start();
    crate::ddi::flip_keep::reset_for_start();
    crate::ddi::present_foreign::reset_for_start();
    crate::ddi::onscanout::reset_for_start();
    // `BltAsync` / `BltNoMirror` (default 0): the knobs read again and mirrored, counters zeroed.
    crate::ddi::blt_async::reset_for_start();
    crate::ddi::shared_placeholder::reset_for_start();
    // `foreign_flip::forget` zeroed its counters and owes the block; this writes it (reading and
    // mirroring `FfKnob` first), as does the `Fk*` block.
    crate::virtio::foreign_flip::publish_counters();
    crate::ddi::flip_keep::publish_counters();
    // The stall-diagnosis block (`HpdLoopN`, `FlipIss`, `VsPendN`, ...): zeroed, `StartN` bumped,
    // written once. After the worker of the previous generation was stopped.
    // The flip retire measurement and the announce knobs (`FlipLat`, `FlipAnnounce`,
    // `FlipEarlyWake`), read and zeroed before the block above is first written.
    crate::ddi::flip_lat::start_generation();
    crate::ddi::flip_announce::start_generation();
    crate::ddi::stall_diag::start_generation();
}

/// Flush the service key (when `flush`) so the stage just recorded survives a
/// bugcheck, and credit the flush time back to the budget.
fn stop_flush(
    passive: crate::irql::PassiveLevel,
    flush: bool,
    budget: helios_kmd_logic::sweep_budget::SweepBudget,
) -> helios_kmd_logic::sweep_budget::SweepBudget {
    if !flush {
        return budget;
    }
    let from = crate::adapter::foreign_scanout::now_100ns();
    crate::diag::flush_service_key(passive);
    stop_credit(budget, from)
}

/// `DxgkDdiStartDevice` — bring the adapter online.
///
/// NOT wrapped by `ddi::traced` and `#[inline(never)]`: this frame plus `VirtioGpu::init` is the
/// nested pair the 24 KB kernel stack budget is measured on (17936 B known good, 18800 B did
/// not boot, `tools/kmd-frame-sizes.ps1`). A wrapper in front of it would add a frame to the
/// pair, and it runs once per start: its failures are visible through `StVio` / `InitStg`.
#[inline(never)]
pub unsafe extern "C" fn dxgkddi_start_device(
    miniport_device_context: *mut c_void,
    _dxgk_start_info: *mut DXGK_START_INFO,
    dxgkrnl_interface: *mut DXGKRNL_INTERFACE,
    number_of_video_present_sources: *mut u32,
    number_of_children: *mut u32,
) -> NTSTATUS {
    crate::kmsg(c"Helios: StartDevice\n");
    start_stage(1);
    crate::diag::record(0x0B00_0001);

    if miniport_device_context.is_null()
        || dxgkrnl_interface.is_null()
        || number_of_video_present_sources.is_null()
        || number_of_children.is_null()
    {
        return STATUS_INVALID_PARAMETER;
    }

    // SHARED borrow only. The context pointer has been public to dxgkrnl since
    // AddDevice, and before this function returns the DIRQL ISR, the VSync timer
    // DPC and the HPD worker all build `&AdapterContext` from the same address —
    // `set_virtio(Some(gpu))` below enables the device mid-function, and
    // `start_vsync`/`init_hpd` at the end start the other two. A unique `&mut`
    // spanning that was an unambiguous Stacked-Borrows violation.
    //
    // Everything StartDevice establishes is therefore built as LOCALS and
    // published once, near the end, through `publish_started`.
    //
    // SAFETY: Dxgkrnl passes our adapter context and valid out-pointers.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // Clear the start edge FIRST. On a stop/start cycle the flag survives from
    // the previous start (the context does), and `init_hpd` below spawns a fresh
    // worker — which would see a stale 1, skip the wait entirely and indicate
    // child status while THIS StartDevice is still running. That is precisely
    // what the wait exists to prevent.
    adapter
        .start_complete
        .store(0, core::sync::atomic::Ordering::Release);

    // v327 breadcrumbs: what the previous generation left in the statics and on the adapter, taken
    // BEFORE anything of this generation zeroes it (`EntD0`, `EntArm`, `EntVsEn`, `EntHpdTh`,
    // `EntHpdN`, `EntVsTk`, `EntRef`), then the per-generation reset of the worker's own statics.
    crate::ddi::stall_diag::note_start_entry(adapter, crate::ddi::hpd::indicate_count());
    crate::ddi::hpd::reset_for_start();

    // NOT copied here. `dxgkrnl_interface` is 576 bytes and this function's
    // stack frame is shared with `VirtioGpu::init`'s 3.0 KB one on a 24 KB
    // kernel stack — see `StartedState::boxed`. The pointer is carried to the
    // publication site and dereferenced straight into the heap allocation.
    crate::diag::record(0x0B00_0002);

    // EVERY service-key knob, read once per StartDevice (`reg add` + `devcon
    // restart` re-runs this without a reboot), together with the breadcrumbs that
    // mirror them. The descriptor writers and the caps path take this value, so
    // none of them can reach the registry themselves. See `AdapterKnobs`.
    reread_cached_knobs();
    let mut knobs = crate::adapter::AdapterKnobs::read_at_start();

    // Registry values persist across boots, so a stale nonzero fault counter is
    // indistinguishable from a fault on THIS boot. Zero the whole set once here,
    // before anything can fail, so the gate's "verify movement, not presence"
    // rule applies to every counter below.
    crate::diag::reset_fault_counters();
    // No target mode is committed on a fresh start.
    adapter.set_committed_refresh_mhz(0);
    // Diagnostic override of the vsync heartbeat rate (0 = follow the mode).
    let forced_vsync_mhz = crate::diag::read_config_dword(crate::diag::knobs::VSYNC_RATE_MHZ, 0);
    crate::adapter::kobj::VSYNC_RATE_OVERRIDE_MHZ
        .store(forced_vsync_mhz, core::sync::atomic::Ordering::Relaxed);
    crate::diag::record_named_bytes(b"VsRate", forced_vsync_mhz);

    // Carried over from a previous start on this same context, if any: these
    // blocks are allocated once and freed only in Drop, and today's code gets
    // that by leaving the fields untouched across StopDevice. Publish-once would
    // otherwise leak them and allocate again.
    // SAFETY: StartDevice, PASSIVE, serialized by dxgkrnl against every other
    // lifecycle DDI; the blocks are republished below in the new state.
    let mut paging_ram = unsafe { adapter.take_paging_ram() };
    if paging_ram.is_none() {
        paging_ram = AdapterContext::alloc_paging_ram();
    }
    // D4a read-ledger page: same once-per-adapter lifetime as the RAM blocks
    // above (user mappings of it survive a PnP stop), allocated on the HEAP —
    // never a StartDevice-chain stack frame (the 17936 B boot-chain ceiling).
    // Idempotent; failure is counted (`RdPgF`) and only latches the acquire
    // feature off, never the adapter.
    adapter.read_ledger.init_page();
    let producer_ready = adapter.producer.init();
    crate::diag::record_named_bytes(b"PrInitF", u32::from(!producer_ready));
    crate::diag::record_named_bytes(b"PrOpenF", 0);
    crate::diag::record_named_bytes(b"PrBindAt", 0);
    // SAFETY: StartDevice supplies the callback table for this live adapter.
    let producer_callbacks = unsafe { &*dxgkrnl_interface };
    crate::diag::record_named_bytes(
        b"PrCb",
        u32::from(producer_callbacks.DxgkCbGetHandleData.is_some())
            | (u32::from(producer_callbacks.DxgkCbAcquireHandleData.is_some()) << 1)
            | (u32::from(producer_callbacks.DxgkCbReleaseHandleData.is_some()) << 2),
    );
    if !producer_ready {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    // ── Phase 2: bring up the virtio-gpu transport ──────────────────────────
    // VirtioGpu::init reads PCI config + maps BARs through the Dxgkrnl callbacks
    // (DxgkConfigAccess / WdkHal) and discovers the virtio device.
    //
    // Gate 1 is an adapter-load gate, not a render-capability gate. During early
    // WDDM bring-up we keep the adapter startable across boot/restart even when
    // the transport probe fails, record the exact status, and leave `virtio=None`.
    // Later gates must tighten this once allocations/submission advertise usable
    // render capability.
    // SAFETY: `DxgkDdiStartDevice` is documented "IRQL: PASSIVE_LEVEL" (WDK
    // DXGKDDI_START_DEVICE). It is also the deepest stack in the driver — this
    // token threads down through `bring_up_venus` -> `allocate_host_visible_blob`
    // -> `VenusRing::bring_up`, which is why it is a by-value ZST and not a
    // reference: see `crate::irql` and tools/kmd-frame-sizes.ps1.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    // Drop any prior transport before re-init (a start with no stop before it):
    // its Drop resets the device and frees its rings/scratch. Doing it *before*
    // init keeps the ordering safe — otherwise assigning the new transport would
    // drop the old one (resetting the device) right after init configured it.
    // Through `retire_transport`, not a bare `set_virtio(None)`: the host is told
    // to close every RM handle of the old transport while it still answers (the
    // reset does not make it drop them, and unlocking a pinned page the host
    // still holds is unsafe), and the old transport's user views are marked stale
    // (a stop that ran first has already done both, and this finds no transport).
    crate::virtio::nvrm::retire_transport(
        passive,
        adapter,
        &helios_kmd_logic::sweep_budget::SweepBudget::live(
            crate::adapter::foreign_scanout::now_100ns(),
        ),
    );
    start_generation_mirrors();
    // Whatever the previous generation recorded against its resource ids (system
    // backing leases, "system copy invalid" marks) is meaningless now: ids restart
    // at 1 and would name different resources.
    adapter.reset_system_backings(passive);
    // Non-zero only if init below fails, so the display-half demotion can report
    // the status that actually killed the transport rather than a bare flag.
    let mut transport_fail_status: u32 = 0;
    // The transport generation, built as locals and installed after publication.
    let mut bar_segment = None;
    let mut venus_ctx_id = 0u32;
    // SAFETY: dxgkrnl_interface is valid per the DDI contract (also copied into
    // the `dxgkrnl` local above); init only borrows it for the call.
    // Did the OS connect MSI/MSI-X messages instead of the INTx line? Probed in
    // its own noinline frame BEFORE `init` (never nested in it: the boot stack
    // budget) and passed in as a bare u32. 0 = INTx = the driver's historical
    // behaviour, byte for byte. See `virtio::msi`.
    let msi_granted = crate::virtio::msi::probe_granted(unsafe { &*dxgkrnl_interface });
    // The host's buffer-release event (`NVGPU_F_SCANOUT_RELEASE`) is acked only with the
    // display half: it serves the foreign scanout sources and the RM ring presenter,
    // which exist only there. A render-only start acks nothing new (the host then keeps
    // no release bookkeeping for this guest).
    match crate::virtio::VirtioGpu::init(
        passive,
        unsafe { &*dxgkrnl_interface },
        msi_granted,
        knobs.display_half,
    ) {
        Ok(mut gpu) => {
            let Some(generation) = adapter.producer.start_transport() else {
                crate::diag::record_named_bytes(b"PrGenF", 1);
                return STATUS_INSUFFICIENT_RESOURCES;
            };
            gpu.attach_producer_completion(adapter, generation);
            crate::kmsg(c"Helios: virtio-gpu transport up\n");
            crate::diag::record(0x0B00_0003);
            let host_visible_bytes = gpu.host_visible().map(|window| window.len);
            // Publish the interrupt mode, then the ISR-status register VA, for
            // the DIRQL ISR before the transport goes live (capture before `gpu`
            // is moved into set_virtio). The message-mode word goes FIRST so an
            // ISR that sees a nonzero `isr_status` can never still believe it is
            // on a line the device is no longer using.
            adapter
                .msi_state
                .store(gpu.msi_isr_state(), core::sync::atomic::Ordering::Release);
            adapter
                .isr_status
                .store(gpu.isr_status_addr(), core::sync::atomic::Ordering::Release);
            adapter.set_virtio(Some(gpu));
            // Now the interrupt can be claimed: hand the host its event buffers.
            let _ = adapter.with_virtio(|v| v.post_nvrm_event_buffers());

            // An explicit VidMmVramMB registry value remains authoritative.
            // When it is absent, use the exact virtio shared-memory capability
            // length rather than a compiled 4-GiB default or the padded PCI BAR.
            super::bar_segment::resolve_vidmm_vram_mb(&mut knobs, host_visible_bytes);

            // ── BAR memory segment / CPU host aperture ──────────────────────
            // Reserve the window head BEFORE any blob map can allocate a
            // window offset, and before dxgkrnl queries segments.
            // Two-memory-split fix (Option A).
            bar_segment = setup_bar_segment(adapter, &knobs);

            // ── Venus-backed page-table memory (best-effort) ─────────────────
            // Self-allocate a 16-MiB HOST_VISIBLE|HOST_COHERENT VkDeviceMemory over
            // venus and expose it as a BAR-backed, CPU-coherent region VidMm can
            // register as the page-table segment (VidMm drops a system-RAM segment;
            // it accepts device-BAR memory backed by real host memory). PASSIVE
            // inside StartDevice; the flows ride `virtio::ctrl` (locked enqueues +
            // PASSIVE waits), so they coexist with the interrupt DPC, which may
            // already be live. On any failure we record diag and leave the
            // venus context id 0 — never fail StartDevice (Gate 1 stays
            // start-safe). See virtio::venus.
            venus_ctx_id = bring_up_venus(passive, adapter);
        }
        Err(e) => {
            crate::kmsg(c"Helios: virtio-gpu init FAILED\n");
            let status: NTSTATUS = e.into();
            crate::diag::record(0x0B00_00E0);
            crate::diag::record(status as u32);
            crate::diag::fault(crate::diag::FaultCounter::StVio, status as u32);
            transport_fail_status = status as u32;
            adapter
                .isr_status
                .store(0, core::sync::atomic::Ordering::Release);
            adapter
                .msi_state
                .store(0, core::sync::atomic::Ordering::Release);
            adapter.set_virtio(None);
            super::bar_segment::resolve_vidmm_vram_mb(&mut knobs, None);
        }
    }

    start_stage(2);

    // Gate 1 keeps the adapter startable without a transport (render-only
    // recovery), but the display half has no such licence: with virtio=None
    // nothing can ever reach a scanout. Left enabled it reports one source and
    // one child, arms the CRTC_VSYNC heartbeat, and has the HPD worker tell the
    // OS the monitor is CONNECTED - so the OS commits a path to a target that
    // can never receive a frame. No allocation ever gets a resource id, so every
    // SetVidPnSourceAddress exits with ScRid=0 and STATUS_SUCCESS: a permanently
    // blank monitor whose only diagnostic was a DiagLevel-gated breadcrumb.
    //
    // Turning the flag OFF - rather than merely reporting zero sources - is
    // required because ~20 display DDIs branch on the flag itself. StartDevice
    // still returns STATUS_SUCCESS: the render-only recovery shape is preserved.
    if knobs.display_half && adapter.with_virtio(|_| ()).is_err() {
        knobs.display_half = false;
        crate::diag::fault(
            crate::diag::FaultCounter::StNoTx,
            if transport_fail_status != 0 {
                transport_fail_status
            } else {
                1
            },
        );
        crate::diag::record_named_bytes(b"DspH", 0);
    }

    // Source/child count. Default (render-only): 0 scanout sources, 0 children.
    // With the `DisplayHalf` knob on: one video-present source + one child
    // video-output, with the VidPn/child DDIs driving virtio-gpu scanout.
    // SAFETY: out-pointers validated non-null above.
    unsafe {
        if knobs.display_half {
            *number_of_video_present_sources = crate::ddi::vidpn::NUM_VIDPN_SOURCES;
            *number_of_children = crate::ddi::vidpn::NUM_CHILDREN;
        } else {
            *number_of_video_present_sources = 0;
            *number_of_children = 0;
        }
    }

    // Defensive: a StopDevice on this same context should already have done
    // this, but a start that inherits a latched gate or a stale resource id from
    // a previous transport generation is unrecoverable, so pay for it twice.
    adapter.reset_display_publication_state();
    // The flips the KMD tracks for the host's release events belong to the transport
    // that just came up (the reset above emptied the book): on if it acked them.
    crate::virtio::scanout_release::set_tracking(
        adapter
            .with_virtio(|v| v.scanout_release_on())
            .unwrap_or(false),
    );
    // R505: zero the deferred-programming refusal counters and write the zeros
    // through. Registry counter values persist across boots, so without this a
    // reader cannot tell a counter that is merely PRESENT from one that moved
    // this boot.
    crate::ddi::display::reset_scanout_reject_counters();
    // Same rule for the unsampled scanout-bind trace: its whole purpose is that
    // a value read after a workload describes THAT workload.
    crate::ddi::scanout_trace::reset(adapter);

    // The scan-out mode and its EDID, resolved BEFORE publication because
    // `StartedState` is published exactly once. In its own transient frame: the
    // 256-byte EDID copy lives there, not in this frame (see the stack-budget
    // note on `bring_up_venus`).
    let Some(scanout_mode) = resolve_scanout_mode(adapter, knobs.display_half) else {
        return STATUS_UNSUCCESSFUL;
    };

    // ── The reported segment table, built ONCE from the same locals every other
    // consumer will read. `query_segments` renders this; it no longer re-derives
    // a table of its own from live adapter state, which is what let the reported
    // topology and `bar_segment` disagree (k-capsescape-04).
    //
    // Ordering is proven, not assumed: on a DiagLevel=1 boot the ring shows
    // StartDevice entry/exit (0x0B00_0001 .. 0x0B00_0004) completing before the
    // first QueryAdapterInfo (0x0100_0001), so the table always exists by the
    // time QUERYSEGMENT4 runs.
    let segment_table = build_segment_table(
        &mut bar_segment,
        paging_ram.as_ref().map(|r| (r.phys, r.size)),
        &knobs,
    );

    // ── Publish. Everything above was a local; from here the adapter answers. ──
    // SAFETY: StartDevice, PASSIVE_LEVEL, serialized by dxgkrnl; published once
    // per start, and every reader reaches it through the Acquire in `started()`.
    // `boxed` builds on the HEAP in its own (transient) frame — never bind its
    // value here, only the Box, or the 832-byte temporary comes back.
    unsafe {
        adapter.publish_started(crate::adapter::StartedState::boxed(
            dxgkrnl_interface,
            knobs,
            scanout_mode,
            paging_ram,
            segment_table,
        ));
        adapter.set_transport_generation(Some(crate::adapter::TransportGeneration {
            bar_segment,
            venus_ctx_id,
            serial: crate::adapter::mint_transport_serial(),
        }));
    }

    start_stage(3);

    if knobs.display_half {
        crate::diag::record_named_bytes(b"DspMd", adapter.display_mode_packed());

        zero_linear_scanout_breadcrumbs();

        // Arm the CRTC_VSYNC heartbeat: without a free-running VSync, dxgkrnl never
        // retires a flip and so never issues SetVidPnSourceAddress (viogpu3d
        // FlipThread analog). `dxgkrnl` was saved above so the DPC can synthesize
        // interrupts. Never armed on the render-only surface.
        // SAFETY: `adapter` is the final boxed context (dxgkrnl holds it as the
        // miniport device context); the started state — including the callback
        // table the DPC needs — is published above. PASSIVE_LEVEL.
        unsafe { adapter.start_vsync() };

        // Start the HPD worker: it indicates the child connected shortly after this
        // StartDevice returns (DxgkCbIndicateChildStatus is forbidden during it) and
        // on every virtio config-change interrupt, so the OS marks the VidPn target
        // available and builds a source→target path.
        // SAFETY: final boxed context; dxgkrnl saved. PASSIVE_LEVEL.
        unsafe { adapter.init_hpd() };
    }

    crate::diag::record(0x0B00_0004);
    // LAST action: the real edge the HPD worker waits on. Its prologue used to
    // approximate "StartDevice has returned" with a 500 ms delay; that delay is
    // now only a bounded fallback (`HpdStTo` counts it firing). Safe to signal
    // even when the worker was never started — nothing else waits on this.
    adapter.signal_start_complete();
    // `ScRestAddr` / `ScRestSig`: the heartbeat's restart seed, and the one wake a programming
    // that survived the restart is owed (`restart_flip::needs_worker_signal`).
    crate::ddi::stall_diag::note_restart_exit(adapter);
    start_stage(4);
    STATUS_SUCCESS
}

/// `DxgkDdiStopDevice` — quiesce the adapter (inverse of StartDevice).
#[inline(never)]
pub unsafe extern "C" fn dxgkddi_stop_device(miniport_device_context: *mut c_void) -> NTSTATUS {
    crate::kmsg(c"Helios: StopDevice\n");
    // FIRST, before anything below can wait on something an escape holds: every escape in flight
    // gives up at its next wait slice (at most 100 ms) and releases its locks, and none starts
    // (v334, `ddi::escape_wait`). Atomic store, any IRQL.
    crate::ddi::escape_wait::set_stopping(true);
    if !miniport_device_context.is_null() {
        // SHARED borrow, for the same reason StartDevice takes one: the ISR and
        // the DPCs can still build `&AdapterContext` from this pointer while this
        // function runs, and it does not stop being true just because we are
        // tearing down.
        // SAFETY: our adapter context, handed back from AddDevice.
        let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
        // SAFETY: `DxgkDdiStopDevice` is documented "IRQL: PASSIVE_LEVEL" (WDK
        // DXGKDDI_STOP_DEVICE); the teardown below unrefs blobs and destroys the
        // venus context, both control round-trips against the still-live device.
        let passive_stop = unsafe { crate::irql::PassiveLevel::assume() };
        // Stage 1 is recorded with a clock of its own: the budget below must not
        // start until the flush after it has finished.
        stop_stage(crate::adapter::foreign_scanout::now_100ns(), 1);
        use helios_kmd_logic::stall_diag::stop_sub as ss;
        crate::ddi::stall_diag::stop_sub(ss::ENTER);
        // The DDI failure rings, the sticky first-fatal record and the paging/lock records
        // (`ddi::device_lost`): written BEFORE the flush below, so a stop that follows an
        // adapter-wide device loss leaves them on disk. PASSIVE.
        crate::ddi::device_lost::publish_block(crate::ddi::device_lost::Trigger::Stop);
        // The newest issued flip address (`RestSeed`: the next image reads it at StartDevice,
        // `pnputil /restart-device` reloads the image and zeroes every static), written before
        // the first flush so that one covers it; `note_stop_entry` writes it again if a flip
        // arrived while the worker and the heartbeat were being stopped.
        crate::ddi::stall_diag::persist_rest_seed(true);
        // The first stage reaches the disk before anything that could bugcheck.
        let flush = crate::diag::read_config_dword(crate::diag::knobs::STOP_FLUSH, 1) != 0;
        if flush {
            // The flush covers the whole SYSTEM hive, which is dirty during a driver
            // install: it can take hundreds of milliseconds and blocks other
            // registry writers. It runs BEFORE the budget exists so that time is not
            // taken from the host round trips.
            crate::ddi::stall_diag::stop_sub(ss::FLUSH_FIRST);
            crate::diag::flush_service_key(passive_stop);
        }
        // ONE budget for every host round trip below: after it is spent the sweeps
        // only drop table entries and send nothing (the transport reset that
        // follows reclaims the host side), so StopDevice is bounded by it plus
        // one in-flight command, whatever the host does. Time spent on things that
        // are not host commands (the later hive flushes, the worker joins) is
        // credited back (`stop_credit`), because a budget eaten by a join leaves
        // every handle unsent, and unsent handles leak their pins.
        let entry = crate::adapter::foreign_scanout::now_100ns();
        let mut budget = helios_kmd_logic::sweep_budget::SweepBudget::stop(entry);
        // Stop the ISR from touching the (about-to-be-reset) device first.
        //
        // ⚠ ASYMMETRY, recorded rather than changed (k-ctrlsubmit-12): this
        // clears the ISR's gate but NOT `started_published`, so the boxed
        // StartedState — including the DXGKRNL_INTERFACE both DPCs read
        // lock-free — stays published across the stop. That is deliberate on
        // one count (a stop/start cycle carries the contiguous RAM blocks
        // forward through it) and unexamined on another: a DPC already queued
        // when this runs can still resolve `started()` for a stopped device.
        // The DPC's actual work all goes through `with_virtio`, which is
        // `Err(DeviceNotFound)` once `set_virtio(None)` runs below, so the
        // window is currently harmless. Clearing the publication properly needs
        // the take-and-republish dance `take_paging_ram` already performs, and
        // that is a lifecycle change with its own reboot-level gate — not a
        // T4a minor item.
        adapter
            .isr_status
            .store(0, core::sync::atomic::Ordering::Release);
        adapter
            .msi_state
            .store(0, core::sync::atomic::Ordering::Release);
        stop_stage(entry, 3);
        crate::ddi::stall_diag::stop_sub(ss::ISR_CLEARED);
        // Cancel the display-half VSync heartbeat + join the HPD worker before
        // teardown (both idempotent; no-ops when the render-only surface never
        // started them). stop_hpd blocks until the worker exits so it can't touch
        // the (about-to-be-torn-down) context.
        //
        // The HPD join is bounded (5 s on the exit event, 5 s on the thread; see
        // `stop_hpd`) and sits OUTSIDE the host-command budget: the worker can be
        // inside a synchronous host round trip, which waits up to 30 s on its own.
        // Its time is credited back so a slow join does not starve the sweeps.
        let joined_from = crate::adapter::foreign_scanout::now_100ns();
        crate::ddi::stall_diag::stop_sub(ss::VSYNC_STOP);
        adapter.stop_vsync();
        crate::ddi::stall_diag::stop_sub(ss::HPD_STOP);
        adapter.stop_hpd();
        crate::ddi::stall_diag::stop_sub(ss::HPD_STOPPED);
        budget = stop_credit(budget, joined_from);
        stop_stage(entry, 4);
        // The HPD worker did the `Nv*` registry mirror and is gone: leave the
        // registry with the final counts (PASSIVE, StopDevice).
        crate::ddi::stall_diag::stop_sub(ss::FINAL_PUBLISH);
        crate::ddi::publish_nvrm_counters();
        crate::ddi::stall_diag::stop_sub(ss::RESET_PUBLICATION);
        // `ScRestPend` / `ScRestAdr0`: what was pending and which address the heartbeat carried
        // when the device stopped, taken before the reset below (`restart_flip`).
        crate::ddi::stall_diag::note_stop_entry(adapter);
        // AFTER stop_hpd, so the worker can no longer re-publish into the state
        // we are about to clear. Every scanout identity below belongs to the
        // transport generation being torn down; carrying it into the next
        // StartDevice is how a latched gate kills CRTC_VSYNC and how a recycled
        // resource id gets bound as the cached scan-out target.
        adapter.reset_display_publication_state();

        // Tear down the venus client + page-table blob + context BEFORE dropping
        // the transport (the unref/detach/destroy commands need the live device).
        // Drop the client first to unmap its ring/reply BAR kernel mappings.
        let venus_ctx = adapter.venus_ctx_id();
        crate::ddi::stall_diag::stop_sub(ss::VENUS_CLIENT_DROP);
        adapter.set_venus_client(None); // Drop → MmUnmapIoSpace ring + reply mappings.
        if venus_ctx != 0 {
            // Best-effort: unref every KMD-internal blob (owner 0) and destroy the
            // venus context (PASSIVE flows through virtio::ctrl).
            // The KMD-owned sweep — `None` here means exactly the KMD's own blobs, not
            // "every owner".
            crate::ddi::stall_diag::stop_sub(ss::BLOB_SWEEP);
            let blobs = crate::virtio::ctrl::release_blobs_for_owner_within(
                passive_stop,
                adapter,
                None,
                Some(&budget),
            );
            crate::diag::record_named_bytes(b"StopBlobs", blobs);
            stop_stage(entry, 5);
            budget = stop_flush(passive_stop, flush, budget);
            crate::ddi::stall_diag::stop_sub(ss::CTX_DESTROY);
            let _ = crate::virtio::ctrl::ctx_destroy_kmd(
                passive_stop,
                adapter,
                venus_ctx,
                Some(&budget),
            );
        } else {
            stop_stage(entry, 5);
            budget = stop_flush(passive_stop, flush, budget);
        }
        stop_stage(entry, 6);
        crate::ddi::stall_diag::stop_sub(ss::REAP_PARKED);
        // Free any parked completed entries at PASSIVE before the transport
        // (and the buffers still in flight inside it) is dropped.
        crate::virtio::ctrl::reap_parked(passive_stop, adapter);
        stop_stage(entry, 7);

        // The host sweep, explicitly and before the transport goes: every handle
        // and mapping of every owner is closed on the host (or, once the budget is
        // spent or the host stops answering, dropped from the tables). Pins are
        // unlocked only after their handles were closed, as in `retire_transport`.
        // `retire_transport` below then finds empty tables and only drops the
        // transport and marks the views stale.
        crate::ddi::stall_diag::stop_sub(ss::HOST_SWEEP);
        let swept = crate::virtio::nvrm::close_all_on_host(passive_stop, adapter, &budget);
        crate::diag::record_named_bytes(b"StopSwept", swept);
        stop_stage(entry, 8);
        budget = stop_flush(passive_stop, flush, budget);

        // Tear down the virtio transport. `retire_transport` first tells the host
        // to close every RM handle of every owner (the transport is still alive,
        // and the host does NOT drop them when the device is reset), then drops
        // the transport: `VirtioGpu::drop` resets the device and frees its rings
        // (plus any in-flight/parked entry buffers), wakes and releases the event
        // registrations, and sweeps whatever the first step could not (a failed
        // transport), unlocking the pins. A later StartDevice re-initializes.
        //
        // The user VIEWS of the host mappings are not the transport's to release:
        // they live in `adapter.mappings`, which outlives it on purpose (as for
        // blob views, they are unmapped only inside the process that made them,
        // and this is not that process). They now point at BAR memory the host no
        // longer backs for them, and the next generation may give the same window
        // offsets to someone else, so `retire_transport` marks them stale, AFTER
        // the transport is gone (nothing can mint an older id any more): the
        // owner's next call into the NVRM escape unmaps them in its own process,
        // and DestroyDevice's drain takes whatever is left.
        crate::ddi::stall_diag::stop_sub(ss::RETIRE_TRANSPORT);
        crate::virtio::nvrm::retire_transport(passive_stop, adapter, &budget);
        let stale_total =
            crate::virtio::nvrm::NVRM_STALE_VIEWS.load(core::sync::atomic::Ordering::Relaxed);
        // Written now, not left to the next escape: this is the one place that
        // knows a stop happened, and the counters may not be published for a while.
        crate::diag::record_named_bytes(b"NvStale", stale_total);
        crate::diag::record_named_bytes(
            b"NvSwept",
            crate::virtio::nvrm::NVRM_SWEPT.load(core::sync::atomic::Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"NvUnpin",
            crate::virtio::nvrm::NVRM_UNPINS.load(core::sync::atomic::Ordering::Relaxed),
        );
        crate::diag::record_named_bytes(
            b"NvPinLeak",
            crate::virtio::nvrm::NVRM_PIN_LEAKS.load(core::sync::atomic::Ordering::Relaxed),
        );

        // Drop the whole transport generation in one store — `bar_segment` and
        // `venus_ctx_id` together, since both are meaningless in the next
        // generation.
        //
        // The STICKY half is deliberately left alone. StopDevice has never
        // cleared the knobs, the mode or the EDID, and about two dozen sites
        // branch on `display_half`; clearing it here would flip all of them from
        // SUCCESS-shaped answers to NOT_SUPPORTED between StopDevice and
        // RemoveDevice. That is a behaviour change, not a tidy-up.
        // SAFETY: StopDevice, PASSIVE_LEVEL, serialized by dxgkrnl against
        // StartDevice and against every DDI that reads the generation.
        unsafe { adapter.set_transport_generation(None) };
        // Every system-backing lease and "system copy invalid" mark is keyed by a
        // resource id of the generation that just ended.
        crate::ddi::stall_diag::stop_sub(ss::SYSTEM_BACKINGS);
        adapter.reset_system_backings(passive_stop);
        stop_stage(entry, 9);
        if flush {
            crate::ddi::stall_diag::stop_sub(ss::FLUSH_LAST);
            crate::diag::flush_service_key(passive_stop);
        }
        stop_stage(entry, 10);
        crate::ddi::stall_diag::stop_sub(ss::DONE);
    }
    STATUS_SUCCESS
}

/// `DxgkDdiRemoveDevice` — free the adapter context allocated in AddDevice.
pub unsafe extern "C" fn dxgkddi_remove_device(miniport_device_context: *mut c_void) -> NTSTATUS {
    crate::kmsg(c"Helios: RemoveDevice\n");
    // As StopDevice: no escape may hold anything the teardown waits for (v334).
    crate::ddi::escape_wait::set_stopping(true);
    crate::diag::record(0x0C00_0001);
    crate::ddi::stall_diag::stop_sub(helios_kmd_logic::stall_diag::stop_sub::REMOVE_ENTER);
    if !miniport_device_context.is_null() {
        // SAFETY: our adapter context; only read here.
        let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };
        if adapter.hpd_worker_may_be_running() {
            // stop_hpd could not prove the worker exited, and the worker
            // dereferences this context. Leak it deliberately: a permanent
            // allocation leak is strictly better than freeing memory a live
            // PASSIVE thread is still touching. StHpdX already recorded why.
            crate::diag::record(0x0C00_00E1);
        } else {
            // SAFETY: this pointer came from Box::into_raw in AddDevice; freed once.
            crate::ddi::stall_diag::stop_sub(
                helios_kmd_logic::stall_diag::stop_sub::REMOVE_DROP,
            );
            drop(unsafe { Box::from_raw(miniport_device_context as *mut AdapterContext) });
        }
    }
    crate::ddi::stall_diag::stop_sub(helios_kmd_logic::stall_diag::stop_sub::REMOVE_DONE);
    crate::diag::record(0x0C00_0002);
    STATUS_SUCCESS
}

/// `DxgkDdiDispatchIoRequest` — legacy VRP path; unused by a render-only WDDM
/// adapter.
pub unsafe extern "C" fn dxgkddi_dispatch_io_request(
    _miniport_device_context: *mut c_void,
    vidpn_source_id: u32,
    video_request_packet: PVIDEO_REQUEST_PACKET,
) -> NTSTATUS {
    crate::diag::record(0x0A10_0000 | (vidpn_source_id & 0xFFFF));
    // Returning STATUS_SUCCESS without touching the VRP's StatusBlock tells the
    // caller the request was serviced and leaves it to read whatever was in the
    // block. We service no VRP, so say so in the block the contract puts it in.
    // A WDDM display miniport is effectively never called here, so this is
    // honesty rather than a live bug - and StVrp is how we would find out
    // otherwise.
    if !video_request_packet.is_null() {
        // SAFETY: dxgkrnl owns the packet for the duration of the call; the
        // StatusBlock pointer is part of the same contract and is only written
        // after a null check.
        unsafe {
            let vrp = &*video_request_packet;
            crate::diag::fault(crate::diag::FaultCounter::StVrp, vrp.IoControlCode);
            if !vrp.StatusBlock.is_null() {
                // VP_STATUS is a Win32 error code, NOT an NTSTATUS:
                // ERROR_INVALID_FUNCTION is the video-port convention for "this
                // miniport does not implement this IOCTL".
                const ERROR_INVALID_FUNCTION: i32 = 1;
                (*vrp.StatusBlock).__bindgen_anon_1.Status = ERROR_INVALID_FUNCTION;
                (*vrp.StatusBlock).Information = 0;
            }
        }
    }
    STATUS_SUCCESS
}

/// `DxgkDdiSetPowerState` — accept power transitions (nothing device-specific to
/// do yet).
pub unsafe extern "C" fn dxgkddi_set_power_state(
    miniport_device_context: *mut c_void,
    device_uid: u32,
    device_power_state: DEVICE_POWER_STATE,
    action_type: POWER_ACTION::Type,
) -> NTSTATUS {
    crate::diag::record(0x0A11_0000 | (device_uid & 0xFFFF));
    crate::diag::record(0x0A12_0000 | ((device_power_state as u32) & 0xFFFF));
    crate::diag::record(0x0A13_0000 | ((action_type as u32) & 0xFFFF));
    crate::diag::record_named_bytes(
        b"PwrSt",
        (((device_power_state as u32) & 0xFFFF) << 16) | ((action_type as u32) & 0xFFFF),
    );

    if miniport_device_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: our adapter context, handed back from AddDevice.
    let adapter = unsafe { &*(miniport_device_context as *const AdapterContext) };

    // Before this, a D3 transition was accepted with no action at all, so the
    // ~16 ms KTIMER kept synthesising CRTC_VSYNC through DxgkCbNotifyInterrupt
    // for a source dxgkrnl had powered down - unless dxgkrnl happened to have
    // called DxgkDdiControlInterrupt(CRTC_VSYNC, FALSE) first, which is a brake
    // entirely under its control, not ours.
    //
    // Treat ANY non-D0 state as a timer quiesce and re-arm on D0 when the
    // display half is up. These transitions preserve `vsync_enabled`, whose
    // sole owner after StartDevice is ControlInterrupt at up to DIRQL; a power
    // resume must not silently reverse a prior CRTC_VSYNC disable.
    //
    // Compared against the bindgen discriminant rather than a hand-written
    // integer, so a WDK header change cannot silently invert this.
    //
    // T5 anomaly 2: it quiesced on ANY non-D0 state of ANY `DeviceUid`, the monitor child's
    // included, and a flip is retired only by a CRTC_VSYNC, so a heartbeat stopped by the
    // monitor's power state strands the desktop's flips. Only the ADAPTER leaving D0 quiesces
    // (`hpd_wake::power_vsync`, host-tested); every call is counted (`PwrN`, `PwrUid`,
    // `PwrD3N`) and the watchdog (`AdapterContext::vsync_watch`) re-arms a heartbeat the adapter
    // should be running.
    let d0 = device_power_state == _DEVICE_POWER_STATE::PowerDeviceD0;
    crate::ddi::stall_diag::power_stage(1);
    crate::ddi::stall_diag::note_power(device_uid, d0);
    // v330: `VsPowerMode` 1 (the default) quiesces only on the ADAPTER leaving D0; 0 is KMD 325 (any non-D0 state of any uid).
    match helios_kmd_logic::hpd_wake::power_vsync_mode(
        crate::ddi::stall_diag::vs_power_mode(),
        device_uid,
        d0,
        adapter.display_half(),
    ) {
        helios_kmd_logic::hpd_wake::PowerVsync::Resume => {
            // SAFETY: the context is the final boxed adapter (dxgkrnl holds it
            // as the miniport device context) and dxgkrnl was saved at
            // StartDevice. PASSIVE_LEVEL.
            unsafe { adapter.resume_vsync() };
        }
        helios_kmd_logic::hpd_wake::PowerVsync::Quiesce => adapter.quiesce_vsync(),
        helios_kmd_logic::hpd_wake::PowerVsync::Leave => {}
    }
    crate::ddi::stall_diag::power_stage(3);
    STATUS_SUCCESS
}

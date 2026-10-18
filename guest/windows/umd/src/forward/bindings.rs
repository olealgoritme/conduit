//! Binding-array collection and the per-stage setter families.
//!
//! `DdiSlice`, the `collect_*` helpers, `ShaderStage`, and the four
//! `stage_set_*` macros with their invocations -- constant buffers (10.0 and
//! 11.1 shapes), shader resources and samplers.
//!
//! Moved verbatim out of `forward.rs` by T8/R1107.

use super::*;

/// A runtime-supplied DDI handle array, with the null check moved into the type.
///
/// Four incompatible conventions for this exact shape used to coexist in this
/// file: push `None` per slot (correct), skip the fill but still pass `num`
/// (`so_set_targets` — DXVK then read `num` uninitialised words as COM pointers
/// and AddRef'd them), dereference unconditionally (`collect_buffers`,
/// `collect_samplers`, `set_render_targets`'s RTV loop), and early-return a
/// count (`srv_bind_summary`). `new` is the only constructor and it is the only
/// place the null test lives, so a call site cannot forget it: the raw pointer
/// is never in scope again.
pub(crate) struct DdiSlice<H> {
    pub(crate) ptr: *const H,
    pub(crate) len: usize,
}

impl<H> DdiSlice<H> {
    /// `None` = the runtime handed us a null array.
    ///
    /// SAFETY: the caller asserts the DDI contract that a non-null array really
    /// has `num` elements. That part is not encodable from the driver side.
    pub(crate) unsafe fn new(ptr: *const H, num: u32) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(DdiSlice {
                ptr,
                len: num as usize,
            })
        }
    }

    /// Panic-free element access: bounds-checked, no indexing, no `unwrap`.
    ///
    /// SAFETY: as [`DdiSlice::new`].
    pub(crate) unsafe fn get(&self, index: usize) -> Option<&H> {
        if index >= self.len {
            None
        } else {
            Some(&*self.ptr.add(index))
        }
    }
}

/// Decode `num` slots of a runtime handle array. A null array yields `None` for
/// every slot — that is the one policy: "no bindings", never "read `num`
/// uninitialised pointers".
pub(crate) unsafe fn collect_slots<H, T>(
    slice: Option<DdiSlice<H>>,
    num: u32,
    decode: impl Fn(&H) -> Option<T>,
) -> Vec<Option<T>> {
    let mut out = Vec::with_capacity(num as usize);
    for index in 0..num as usize {
        out.push(match slice.as_ref().and_then(|s| s.get(index)) {
            Some(handle) => decode(handle),
            None => None,
        });
    }
    out
}

/// Architectural caps for the per-stage binding arrays this file forwards.
///
/// SRVs may address all 128 D3D11 slots. Samplers and constant buffers are
/// limited to 16 slots in this driver's in-tree DDI contract. Keeping those
/// latter collectors at their contract cap avoids zeroing 112 unreachable
/// pointer words on every bind, while retaining a bounded, null-filled array
/// if a broken runtime/DDI caller exceeds the cap.
pub(crate) const MAX_SRV_BIND_SLOTS: usize = 128;
pub(crate) const MAX_SAMPLER_BIND_SLOTS: usize = 16;
pub(crate) const MAX_CONSTANT_BUFFER_BIND_SLOTS: usize = 16;
/// `D3D11_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT`.
pub(crate) const MAX_VERTEX_BUFFER_BIND_SLOTS: usize = 32;

/// Binding-array arms that arrived with `num` beyond their collector's cap and
/// were clamped. The runtime validates the API-level slot limits, so a nonzero
/// value means a runtime/DDI contract bug, not app input. Expected 0.
pub(crate) static BIND_SLOTS_CLAMPED: AtomicUsize = AtomicUsize::new(0);

/// A binding array collected as raw COM words on the stack.
///
/// Replaces the per-call `Vec<Option<T>>` whose per-slot `.clone()` was an
/// AddRef (plus a Release each when the `Vec` dropped) and, for constant
/// buffers, a `QueryInterface` per slot per call — together ~8 % of the app's
/// render thread in the GT1 profile
/// (tmp/handoff-perf-saturation/reports/p1-attribution.md §2). No ownership
/// is taken anywhere: the runtime holds each handle's reference across the
/// DDI call by contract, and the handle slot's stored reference lives until
/// the matching Destroy* DDI.
///
/// Only the first `len` slots are written: the SRV collector's array is 128
/// words, and zeroing all of it on every `*SetShaderResources` (one or two
/// views, per draw) was a 1 KiB store for nothing.
pub(crate) struct RawBinds<const N: usize> {
    slots: [core::mem::MaybeUninit<*mut c_void>; N],
    len: usize,
}

impl<const N: usize> RawBinds<N> {
    /// View the collected words as the slice type the windows-crate context
    /// methods take, WITHOUT constructing owned wrappers.
    ///
    /// SAFETY: windows-core interface types are `#[repr(transparent)]` over a
    /// non-null pointer, so `Option<T>` is ABI-identical to a nullable
    /// `*mut c_void` — this is the same layout the generated bindings pass to
    /// COM. The borrow never drops an element, so no reference is released.
    /// The caller must name the `T` the collected slots actually hold.
    pub(crate) unsafe fn as_com_slice<T: Interface>(&self) -> &[Option<T>] {
        // The first `len` slots are initialised (`collect_raw` writes each).
        core::slice::from_raw_parts(self.slots.as_ptr().cast::<Option<T>>(), self.len)
    }

    /// The raw-pointer form for the `SetConstantBuffers1` family, which takes
    /// `Option<*const Option<T>>` + `num` instead of a slice.
    ///
    /// SAFETY: as [`RawBinds::as_com_slice`]; additionally the caller must
    /// keep `self` alive across the context call the pointer is passed to,
    /// and pass [`RawBinds::len`] as the count (only that many are written).
    pub(crate) unsafe fn as_com_ptr<T: Interface>(&self) -> *const Option<T> {
        self.slots.as_ptr().cast::<Option<T>>()
    }

    /// The number of collected slots, after the collector's contract cap.
    ///
    /// This must accompany [`RawBinds::as_com_ptr`] callers because that API
    /// shape receives its element count separately from the pointer.
    pub(crate) fn len(&self) -> u32 {
        self.len as u32
    }
}

/// Decode `num` slots of a runtime handle array into raw COM words. A null
/// array yields `num` null slots — the same one policy as [`collect_slots`]:
/// "no bindings", never "read `num` uninitialised pointers". `num` beyond the
/// collector's architectural cap is clamped loudly ([`BIND_SLOTS_CLAMPED`] + one
/// `log_error!`), never indexed out of bounds.
unsafe fn collect_raw<const N: usize, H>(
    h: *const H,
    num: u32,
    word_of: impl Fn(&H) -> usize,
) -> RawBinds<N> {
    let mut out = RawBinds {
        slots: [core::mem::MaybeUninit::uninit(); N],
        len: 0,
    };
    let len = (num as usize).min(N);
    if num as usize > N && BIND_SLOTS_CLAMPED.fetch_add(1, Ordering::Relaxed) == 0 {
        log_error!("DDI bind array clamped: num={num} max={N}");
    }
    let slice = DdiSlice::new(h, len as u32);
    for (index, slot) in out.slots.iter_mut().take(len).enumerate() {
        let word = slice
            .as_ref()
            .and_then(|s| s.get(index))
            .map_or(0, &word_of);
        slot.write(word as *mut c_void);
    }
    out.len = len;
    out
}

pub(crate) unsafe fn collect_buffers(
    start: u32,
    num: u32,
    h: *const ddi::D3D10DDI_HRESOURCE,
) -> RawBinds<MAX_CONSTANT_BUFFER_BIND_SLOTS> {
    let _ = start;
    // `buffer_raw` is the ID3D11Buffer interface pointer pre-cast at
    // `store_resource`; 0 (= a null bind) for a non-buffer resource, which is
    // exactly what the per-call `.cast()` used to produce for one.
    collect_raw(h, num, |handle| {
        resource_state_at(handle.pDrvPrivate).map_or(0, |s| s.buffer_raw)
    })
}
/// [`collect_buffers`] for the 32 input-assembler vertex-buffer slots.
pub(crate) unsafe fn collect_vertex_buffers(
    num: u32,
    h: *const ddi::D3D10DDI_HRESOURCE,
) -> RawBinds<MAX_VERTEX_BUFFER_BIND_SLOTS> {
    collect_raw(h, num, |handle| {
        resource_state_at(handle.pDrvPrivate).map_or(0, |s| s.buffer_raw)
    })
}

/// The pre-cast `ID3D11Buffer` word of one resource handle (0 for a null
/// handle or a non-buffer resource), with no `QueryInterface` and no
/// AddRef/Release.
pub(crate) unsafe fn buffer_raw_of(h: ddi::D3D10DDI_HRESOURCE) -> usize {
    if h.pDrvPrivate.is_null() {
        return 0;
    }
    resource_state_at(h.pDrvPrivate).map_or(0, |s| s.buffer_raw)
}

pub(crate) unsafe fn collect_srvs(
    num: u32,
    h: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) -> RawBinds<MAX_SRV_BIND_SLOTS> {
    collect_raw(h, num, |handle| handle_com_raw_at(handle.pDrvPrivate))
}

pub(crate) unsafe fn srv_bind_summary(
    num: u32,
    h: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) -> (u32, u32, usize, *mut c_void) {
    if h.is_null() {
        return (0, num, 0, core::ptr::null_mut());
    }
    let mut nonnull = 0u32;
    let mut missing = 0u32;
    let mut first_raw = 0usize;
    let mut first_priv: *mut c_void = core::ptr::null_mut();
    for i in 0..num as usize {
        let p = (*h.add(i)).pDrvPrivate;
        if p.is_null() {
            continue;
        }
        let raw = handle_com_raw_at(p);
        if raw == 0 {
            missing += 1;
            continue;
        }
        nonnull += 1;
        if first_raw == 0 {
            first_raw = raw;
            first_priv = p;
        }
    }
    (nonnull, missing, first_raw, first_priv)
}
pub(crate) unsafe fn collect_samplers(
    num: u32,
    h: *const ddi::D3D10DDI_HSAMPLER,
) -> RawBinds<MAX_SAMPLER_BIND_SLOTS> {
    collect_raw(h, num, |handle| handle_com_raw_at(handle.pDrvPrivate))
}

/// Generate one stage's `pfn*SetConstantBuffers` entry point.
///
/// Six identical bodies differing only in the context method. `collect_buffers`
/// already single-sources the slot decode and its null policy; this removes the
/// remaining wrapper.
macro_rules! stage_set_constant_buffers {
    ($name:ident, $method:ident) => {
        pub(crate) unsafe extern "system" fn $name(
            h: Hdevice,
            start: u32,
            num: u32,
            bufs: *const ddi::D3D10DDI_HRESOURCE,
        ) {
            if let Some(c) = d3d11_context(h) {
                let buffers = collect_buffers(start, num, bufs);
                c.$method(start, Some(buffers.as_com_slice::<ID3D11Buffer>()));
            }
        }
    };
}

stage_set_constant_buffers!(ps_set_constant_buffers, PSSetConstantBuffers);
stage_set_constant_buffers!(vs_set_constant_buffers, VSSetConstantBuffers);
stage_set_constant_buffers!(gs_set_constant_buffers, GSSetConstantBuffers);
stage_set_constant_buffers!(hs_set_constant_buffers, HSSetConstantBuffers);
stage_set_constant_buffers!(ds_set_constant_buffers, DSSetConstantBuffers);
stage_set_constant_buffers!(cs_set_constant_buffers, CSSetConstantBuffers);

/// The six programmable shader stages the D3D11 DDI binds to.
///
/// Replaces a `&str` discriminator matched with a silent `_ => {}` catch-all on
/// two bind paths. All twelve callers passed correct literals, so this removes a
/// latent silent-skip class rather than fixing a live bind: a typo, or a future
/// `AS`/`MS` stage, would have bound NOTHING and left the previous stage's
/// buffers or SRVs live for the following draw, with no counter and no log.
/// R827.
#[derive(Clone, Copy)]
pub(crate) enum ShaderStage {
    Vs,
    Ps,
    Gs,
    Hs,
    Ds,
    Cs,
}

impl ShaderStage {
    /// Exactly the two-letter strings the log lines carried before R827, so
    /// `DDI {stage}SetShaderResources ...` and
    /// `DDI {stage}SetConstantBuffers1 ...` stay textually identical.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vs => "VS",
            Self::Ps => "PS",
            Self::Gs => "GS",
            Self::Hs => "HS",
            Self::Ds => "DS",
            Self::Cs => "CS",
        }
    }
}

pub(crate) unsafe fn set_constant_buffers1_common(
    h: Hdevice,
    stage: ShaderStage,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    let Some(c) = d3d11_context1(h) else {
        return;
    };
    let buffers = collect_buffers(start, num, bufs);
    let buffer_count = buffers.len();
    let buffers_ptr = if buffer_count == 0 {
        None
    } else {
        Some(buffers.as_com_ptr::<ID3D11Buffer>())
    };
    let first_ptr = if first_constants.is_null() {
        None
    } else {
        Some(first_constants)
    };
    let count_ptr = if num_constants.is_null() {
        None
    } else {
        Some(num_constants)
    };
    if SHADER_BIND_LOG_COUNT.first_n(256).is_some() {
        let first0 = if first_constants.is_null() {
            0
        } else {
            *first_constants
        };
        let count0 = if num_constants.is_null() {
            0
        } else {
            *num_constants
        };
        trace_line!(
            "DDI {}SetConstantBuffers1 start={} num={} first_ptr={} count_ptr={} first0={} count0={}",
            stage.name(), start, num, !first_constants.is_null(), !num_constants.is_null(), first0, count0
        );
    }
    // Exhaustive: adding a stage to the enum is a compile error here, not a
    // silent no-op bind.
    match stage {
        ShaderStage::Vs => {
            c.VSSetConstantBuffers1(start, buffer_count, buffers_ptr, first_ptr, count_ptr)
        }
        ShaderStage::Ps => {
            c.PSSetConstantBuffers1(start, buffer_count, buffers_ptr, first_ptr, count_ptr)
        }
        ShaderStage::Gs => {
            c.GSSetConstantBuffers1(start, buffer_count, buffers_ptr, first_ptr, count_ptr)
        }
        ShaderStage::Hs => {
            c.HSSetConstantBuffers1(start, buffer_count, buffers_ptr, first_ptr, count_ptr)
        }
        ShaderStage::Ds => {
            c.DSSetConstantBuffers1(start, buffer_count, buffers_ptr, first_ptr, count_ptr)
        }
        ShaderStage::Cs => {
            c.CSSetConstantBuffers1(start, buffer_count, buffers_ptr, first_ptr, count_ptr)
        }
    }
}

/// Generate one stage's `pfn*SetConstantBuffers1` entry point.
///
/// The BODY here was already single-sourced -- T5/R827 replaced the
/// `stage: &str` dispatch and its fail-open `_ => {}` arm with the exhaustive
/// `ShaderStage` enum, so the review's one claimed static gain for this family
/// is already banked. `ShaderStage` deliberately SURVIVES: this removes only
/// the eight-line signature boilerplate, six times over, and leaves the typed
/// dispatch that makes "adding a stage silently binds nothing" a compile
/// error.
macro_rules! stage_set_constant_buffers1 {
    ($name:ident, $stage:ident) => {
        pub(crate) unsafe extern "system" fn $name(
            h: Hdevice,
            start: u32,
            num: u32,
            bufs: *const ddi::D3D10DDI_HRESOURCE,
            first_constants: *const u32,
            num_constants: *const u32,
        ) {
            set_constant_buffers1_common(
                h,
                ShaderStage::$stage,
                start,
                num,
                bufs,
                first_constants,
                num_constants,
            );
        }
    };
}

stage_set_constant_buffers1!(vs_set_constant_buffers1, Vs);
stage_set_constant_buffers1!(ps_set_constant_buffers1, Ps);
stage_set_constant_buffers1!(gs_set_constant_buffers1, Gs);
stage_set_constant_buffers1!(hs_set_constant_buffers1, Hs);
stage_set_constant_buffers1!(ds_set_constant_buffers1, Ds);
stage_set_constant_buffers1!(cs_set_constant_buffers1, Cs);

/// Generate one stage's `pfn*SetShaderResources` entry point.
///
/// As with `SetConstantBuffers1`, the body was already single-sourced by
/// T5/R827's `ShaderStage`; this removes the repeated signature only.
macro_rules! stage_set_shader_resources {
    ($name:ident, $stage:ident) => {
        pub(crate) unsafe extern "system" fn $name(
            h: Hdevice,
            start: u32,
            num: u32,
            srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
        ) {
            set_shader_resources_common(h, ShaderStage::$stage, start, num, srvs);
        }
    };
}

stage_set_shader_resources!(ps_set_shader_resources, Ps);

pub(crate) unsafe fn set_shader_resources_common(
    h: Hdevice,
    stage: ShaderStage,
    start: u32,
    num: u32,
    srvs: *const ddi::D3D10DDI_HSHADERRESOURCEVIEW,
) {
    let Some(c) = d3d11_context(h) else {
        return;
    };
    let views = collect_srvs(num, srvs);
    // The summary is a second full pass over the array; it used to run on
    // every call and be thrown away whenever tracing was off (the line below
    // is `trace_line!`-gated anyway). Compute it only when it can print.
    // SRV_BIND_LOG_COUNT now advances only while tracing is on — its sole
    // consumer is this line budget.
    if crate::trace_enabled() {
        let (nonnull, missing, first_raw, first_priv) = srv_bind_summary(num, srvs);
        if SRV_BIND_LOG_COUNT.next() < 2048 || missing != 0 {
            trace_line!(
                "DDI {}SetShaderResources start={} num={} nonnull={} missing={} first_raw=0x{:x} first_priv={:p}",
                stage.name(), start, num, nonnull, missing, first_raw, first_priv
            );
        }
    }
    let mut view_slice = views.as_com_slice::<ID3D11ShaderResourceView>();
    // `FlipLedger` 1, dwm.exe: an app's flip-model frame that is not done yet is replaced by the
    // newest done frame of the same swap chain (DWM never waits for an app frame).
    let substituted = if crate::knobs::is_dwm_process() && crate::knobs::flip_ledger() == 1 {
        dwm_flip_views(h, view_slice)
    } else {
        None
    };
    if let Some(list) = substituted.as_ref() {
        view_slice = list.as_slice();
    }
    match stage {
        ShaderStage::Vs => c.VSSetShaderResources(start, Some(view_slice)),
        ShaderStage::Ps => c.PSSetShaderResources(start, Some(view_slice)),
        ShaderStage::Gs => c.GSSetShaderResources(start, Some(view_slice)),
        ShaderStage::Hs => c.HSSetShaderResources(start, Some(view_slice)),
        ShaderStage::Ds => c.DSSetShaderResources(start, Some(view_slice)),
        ShaderStage::Cs => c.CSSetShaderResources(start, Some(view_slice)),
    }
}
/// `FlipLedger` 1, dwm.exe: what DWM's binds of app flip-model buffers became (since start).
pub(crate) static DWM_FLIP_READY: AtomicUsize = AtomicUsize::new(0);
pub(crate) static DWM_FLIP_SUBSTITUTED: AtomicUsize = AtomicUsize::new(0);
pub(crate) static DWM_FLIP_UNREADY: AtomicUsize = AtomicUsize::new(0);

/// `FlipLedger` 1, dwm.exe, at each shader-resource bind: DWM composes the newest READY frame
/// of an app's flip-model swap chain and never waits for one that is not done.
///
/// * A bound view of an opened app present buffer whose newest ledger point (the app publishes
///   one per present) is complete stays.
/// * One whose point is pending is replaced by a view of the buffer of the same swap chain (same
///   publishing device, same size and format, at least three buffers) whose newest point is the
///   newest complete one: the frame before. dxgkrnl released that buffer to the app when DWM
///   took the newer one, so DWM's read of it is published as a read point at DWM's present and
///   the app's next access waits for it (`forward/present.rs` `dwm_flip_read_points`). With three
///   buffers the app renders into the oldest one meanwhile, not this one.
/// * With no complete frame (the chain's first frames) the view stays and DWM's DXVK waits for
///   the point (the ledger wait, bounded).
///
/// Every app buffer DWM ends up binding is noted as read this frame. `None`: nothing replaced.
pub(crate) unsafe fn dwm_flip_views(
    h: Hdevice,
    views: &[Option<ID3D11ShaderResourceView>],
) -> Option<Vec<Option<ID3D11ShaderResourceView>>> {
    let dev = helios_device(h)?;
    let flips = lock_ignore_poison(&dev.nvk_flip_buffers).clone();
    if flips.is_empty() {
        return None;
    }
    let mut out: Option<Vec<Option<ID3D11ShaderResourceView>>> = None;
    let mut reads: Vec<usize> = Vec::new();
    for (i, view) in views.iter().enumerate() {
        let Some(view) = view else {
            continue;
        };
        let Ok(resource) = view.GetResource() else {
            continue;
        };
        let raw = resource.as_raw() as usize;
        let Some(&(_, _, width, height, format)) = flips.iter().find(|f| f.1 == raw) else {
            continue;
        };
        let (state, record, point) = dev.dxvk.handoff_flip_state(raw);
        if state != 2 {
            DWM_FLIP_READY.fetch_add(1, Ordering::Relaxed);
            reads.push(raw);
            continue;
        }
        // The same swap chain: the same publishing device, size and format.
        let chain: Vec<usize> = flips
            .iter()
            .filter(|f| f.2 == width && f.3 == height && f.4 == format)
            .map(|f| f.1)
            .filter(|&c| c == raw || dev.dxvk.handoff_flip_state(c).1 == record)
            .collect();
        let mut best: Option<(usize, u64)> = None;
        if chain.len() >= 3 {
            for &c in chain.iter().filter(|&&c| c != raw) {
                let (st, rec, pt) = dev.dxvk.handoff_flip_state(c);
                if st == 1 && rec == record && pt < point && best.map_or(true, |(_, b)| pt > b) {
                    best = Some((c, pt));
                }
            }
        }
        let Some((substitute, _)) = best else {
            DWM_FLIP_UNREADY.fetch_add(1, Ordering::Relaxed);
            reads.push(raw);
            continue;
        };
        let Some(sub_view) = dwm_flip_view_for(dev, view, substitute) else {
            DWM_FLIP_UNREADY.fetch_add(1, Ordering::Relaxed);
            reads.push(raw);
            continue;
        };
        DWM_FLIP_SUBSTITUTED.fetch_add(1, Ordering::Relaxed);
        reads.push(substitute);
        out.get_or_insert_with(|| views.to_vec())[i] = Some(sub_view);
    }
    if !reads.is_empty() {
        let mut noted = lock_ignore_poison(&dev.nvk_flip_reads);
        for raw in reads {
            if !noted.contains(&raw) {
                noted.push(raw);
            }
        }
    }
    out
}

/// A shader-resource view of the opened app buffer `resource` shaped like `like` (DWM's view
/// of another buffer of the same swap chain), made once per buffer and kept until the buffer
/// goes (`forget_nvk_keyed_resource`).
unsafe fn dwm_flip_view_for(
    dev: &crate::device_funcs::HeliosDevice,
    like: &ID3D11ShaderResourceView,
    resource: usize,
) -> Option<ID3D11ShaderResourceView> {
    if let Some((_, v)) = lock_ignore_poison(&dev.nvk_flip_views).iter().find(|e| e.0 == resource) {
        return Some(v.clone());
    }
    let device = dev.dxvk.d3d11_device()?;
    let raw = resource as *mut core::ffi::c_void;
    let target = ID3D11Resource::from_raw_borrowed(&raw)?;
    let mut desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
    like.GetDesc(&mut desc);
    let mut made: Option<ID3D11ShaderResourceView> = None;
    device.CreateShaderResourceView(target, Some(&desc), Some(&mut made)).ok()?;
    let made = made?;
    lock_ignore_poison(&dev.nvk_flip_views).push((resource, made.clone()));
    Some(made)
}

stage_set_shader_resources!(vs_set_shader_resources, Vs);
stage_set_shader_resources!(gs_set_shader_resources, Gs);
stage_set_shader_resources!(hs_set_shader_resources, Hs);
stage_set_shader_resources!(ds_set_shader_resources, Ds);
stage_set_shader_resources!(cs_set_shader_resources, Cs);
/// Generate one stage's `pfn*SetSamplers` entry point.
macro_rules! stage_set_samplers {
    ($name:ident, $method:ident) => {
        pub(crate) unsafe extern "system" fn $name(
            h: Hdevice,
            start: u32,
            num: u32,
            samplers: *const ddi::D3D10DDI_HSAMPLER,
        ) {
            if let Some(c) = d3d11_context(h) {
                let collected = collect_samplers(num, samplers);
                c.$method(start, Some(collected.as_com_slice::<ID3D11SamplerState>()));
            }
        }
    };
}

stage_set_samplers!(ps_set_samplers, PSSetSamplers);
stage_set_samplers!(vs_set_samplers, VSSetSamplers);
stage_set_samplers!(gs_set_samplers, GSSetSamplers);
stage_set_samplers!(hs_set_samplers, HSSetSamplers);
stage_set_samplers!(ds_set_samplers, DSSetSamplers);
stage_set_samplers!(cs_set_samplers, CSSetSamplers);

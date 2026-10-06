# Independent flip (direct flip) of flip-model swap chains: KMD design

Status: DESIGN ONLY. Nothing in `kmd_render` changes with this document. The one piece of code that accompanies
it, `kmd_logic/src/independent_flip.rs` (the decision table of section 6, 16 host tests), is **not wired**.
Written against v327 (`225ce42`, branch `kmd/independent-flip-design`). Line numbers are that commit's.
Companion reading: `zero-copy-present.md` (sections 12.6, 13: the DMA-flip hand-off and the flip-completion
invariant), `kmd-rm-client.md` 15.18 (`ForeignFlip`), `foreign-scanout.md` (the user scan-out source, the release
book, owner death), `shared-foreign-surfaces.md` (how another process opens an NVK allocation).

## 0. The short answer

Today every flip-model application is an input to DWM, and DWM composes it into its own swap chain every frame
(FFXIV: about 126 fps composed, against about 247 fps on NVK's own `SCANOUT_SET` path). On bare metal dxgkrnl and DWM
promote a flip-model swap chain that covers the output to **independent flip**: the application's own buffers become the
primary, DWM stops composing, and the driver scans the application's buffers out. For this KMD that is not a new kind of
flip. It is the same Flip-arm `DxgkDdiPresent` plus `SetVidPnSourceAddress` DWM's chain already goes through, with a
different allocation (and a different importing process) as the source. The pieces that show an arbitrary adopted
allocation exist: `ForeignFlip` (NVK-on-RM buffers) and the Venus direct bind (`MISC_DIRECT_SCANOUT`).

What is missing is, in order of how likely it is to be the thing that stops promotion:

1. **dxgkrnl is told no.** `DXGK_DRIVERCAPS.SupportDirectFlip` and the `DirectFlip` flag of the aperture segment are
   deliberately 0 (`DirectFlipCaps`, `query_adapter_info.rs:439-455`), and the UMD answers `CheckDirectFlipSupport` with
   "no" unconditionally (`umd/src/forward/transfer.rs:369`); the D3D12 UMD never fills the slot at all (section 2.3).
2. **The application's buffers may not be primary-compatible.** Only a buffer created from a `pPrimaryDesc` is a WDDM
   primary today (`umd/src/forward/resource.rs:254-262, 354-362`), which is what gives it `AccessedPhysically` and a
   usable `PrimaryAddress` (`create_allocation.rs:2266, 3526`). Whether the D3D runtime hands an application's flip-model
   buffers a `pPrimaryDesc` is **not known from this tree** (section 2.4).
3. **The pointer.** The KMD reports no hardware pointer (`MaxPointerWidth/Height/PointerCaps` stay zero; `SetPointerShape`
   is a no-op, `display.rs:1739-1753`) and the host has no way to be told a Windows cursor image. Whether dxgkrnl
   promotes without one, and what is on screen if it does, is an open question (section 4.5).
4. Smaller, KMD-side: the 32-entry direct-scan-out table (`SCANOUT_ALLOCS`) fills with every flip-model window's buffers
   once `ForeignFlip` registers them; an unregistered Venus allocation fails its DMA flip (`PBFlip` 0xE6) instead of
   completing; at 240 Hz the MMIO flip publishes a whole tick late unless `FfAsyncWin` is on (derived, section 4.4); a
   conservative reuse rule that tears rather than corrupts (section 3.5).

**Recommendation (the smallest first step): S-0, measure before writing code.** One user-mode probe reads what dxgkrnl
derives from our caps (`KMTQAITYPE_DIRECTFLIP_SUPPORT` 19 and `KMTQAITYPE_INDEPENDENTFLIP_SUPPORT` 28, both header-verified
below) with `DirectFlipCaps` 0 and 1; then run a borderless-fullscreen flip-model application at exactly the mode with
`DirectFlipCaps=1`, `ForeignFlip=1`, `FfAsyncWin=2`, first at 1920x1080@60 and then at the real mode, and read PresentMon's
mode, the existing `Vp*`, `Ff*`, `Fk*`, `PBflag` counters and the UMD's `primary_desc=` log line. S-0a needs no code.
S-0b needs one UMD hook (answer `CheckDirectFlipSupport` TRUE under a debug value). Section 7.

## 1. Evidence base, and how claims are tagged

The sources the claims rest on:

| tag | source | what it can prove |
|---|---|---|
| **[H]** | `guest/windows/icd/win-build/wdk-include/{d3dkmthk.h,d3dkmdt.h,d3dukmdt.h}`: the 10.0.26100 user-mode / thunk-level headers (version conditionals name `DXGKDDI_INTERFACE_VERSION_WDDM3_2`) | the `D3DKMT_*` side: present flags, the flip-model present history token (including its `IndependentFlip*` bits), the MPO structs and caps, the `KMTQAITYPE_*` list, `D3DKMT_DIRECTFLIP_SUPPORT`, `D3DKMT_INDEPENDENTFLIP_SUPPORT` |
| **[H]** | `guest/windows/umd12/bindgen/cached/d3d12umddi.rs`: bindgen of `d3dumddi.h` (26100) | `D3DDDIARG_CHECKDIRECTFLIPSUPPORT`, `_D3DDDI_DEVICEFUNCS::pfnCheckDirectFlipSupport` |
| **[H8]** | the **Windows 8** SDK/WDK display headers (`d3dkmddi.h`, `dxgiddi.h`, `d3d10umddi.h`, `dispmprt.h`, `d3dkmdt.h`), older than 8.1 | the miniport DDI shapes that have not changed: `DXGKARG_PRESENT`, `DXGK_PRESENTFLAGS`, `DXGK_FLIPCAPS`, `DXGK_DRIVERCAPS` up to `SupportPerEngineTDR`, `DXGKARG_SETVIDPNSOURCEADDRESS` and its flags, `DXGK_POINTERFLAGS`, `DXGI_DDI_PRIMARY_DESC`. It has **no** `SupportDirectFlip`, no `DirectFlip` segment bit (its segment bit 5 is `PitchAlignment`), nothing MPO |
| **[T]** | this tree at `225ce42` | what the KMD and UMD do; `file:line` |
| **[D]** | `/home/xuw/code/helios-src/docs/archive/{ROADMAP_HISTORY_THROUGH_2026-09-05.md,REFACTOR_REVIEW.md}` (a sibling repo, outside this one) | history of the legacy `SupportDirectFlip` advertisement; the 26100 `DXGK_DRIVERCAPS` layout the KMD's bindgen produced (size 592, `SupportDirectFlip` at 539, `SupportMultiPlaneOverlay` at 540, `GpuEngineTopology` at 76: `REFACTOR_REVIEW.md:4192`) |
| **[Derived]** | reading the code above, no run | arithmetic and ordering arguments; not measured |
| **[M]** | memory of Microsoft documentation and of how Windows behaves | **unverified**. Every such statement is tagged and collected in section 9 |

**Not on disk:** the WDK 10.0.26100 `d3dkmddi.h` and `dispmprt.h` (the KMD's bindgen runs on the Windows build machine;
the only generated `dxgk_bindings.rs` found here is a 24-line stub), and any dxgkrnl documentation. So the *kernel-mode*
facts about `SupportDirectFlip`, the `DirectFlip` segment flag, `DXGK_DRIVERCAPS.SupportMultiPlaneOverlay` and the MPO3 KMD
interface are known to exist only through this KMD's own use of the bindings ([T] and [D]), not from a header I could open.
The NVIDIA driver sources that sit in the same directory tree as the Win8 headers were deliberately not mined: they are
marked proprietary and confidential. One 16-line header (`nvDirectFlip.h`, a single function declaration) was opened while
searching and used for nothing; nothing here depends on that tree beyond the Microsoft headers listed above.

## 2. What dxgkrnl needs the driver to offer (question 1)

### 2.1 The three modes, and the minimum for each

Names are Windows', behaviour is [M] unless a header is cited.

| mode | what it is [M] | what the KMD must offer [M] | header evidence |
|---|---|---|---|
| **Composed flip** (today) | the application's flip-model buffers are DWM inputs; DWM draws them into its own chain and flips that | the ordinary flip contracts | `D3DKMT_PM_REDIRECTED_FLIP` [H `d3dkmthk.h:447`] |
| **Direct Flip** (Windows 8.1, WDDM 1.3) | DWM-assisted: DWM stays the primary's owner but flips the *application's* buffers instead of composing, when they are compatible with its own chain | `SupportDirectFlip` caps, `DirectFlip` segment flag, a UMD that answers `CheckDirectFlipSupport(app resource, DWM resource)` yes | `KMTQAITYPE_DIRECTFLIP_SUPPORT` = 19 inside the `>= WIN8` block [H `d3dkmthk.h:2374-2382`], `D3DKMT_DIRECTFLIP_SUPPORT{BOOL Supported}` [H `:2043`]; `D3DDDIARG_CHECKDIRECTFLIPSUPPORT{hAppSwapchainResource, hDWMSwapchainResource, CheckDirectFlipFlags, Supported}` and `CHECKDIRECTFLIP_IMMEDIATE = 1` [H `d3d12umddi.rs:25138-25173`] |
| **Independent flip** (Windows 10, WDDM 2.0) | the application flips the primary itself; DWM is notified and does not compose that window; dxgkrnl and DWM hand the primary back and forth | the same flip surface as Direct Flip (no extra KMD cap is *named* in any header I have); the difference is dxgkrnl/DWM protocol | `KMTQAITYPE_INDEPENDENTFLIP_SUPPORT` = 28, `_SECONDARY_SUPPORT` = 39 [H `:2397, 2408`]; the present history token flags `IndependentFlip`, `IndependentFlipStage`, `IndependentFlipReleaseCount`, `IndependentFlipForceNotifyDwm`, `IndependentFlipRequestDwmConfirm`, `IndependentFlipCandidate`, `IndependentFlipCheckNeeded`, `IndependentFlipTrueImmediate`, `IndependentFlipRequestDwmExit`, `IndependentFlipDoNotFlip` [H `:482-493`]; `D3DKMT_FLIPMANAGER_AUXILIARYPRESENTINFO.independentFlipStage / FlipCompletedQpc ("the DPC frame time of the frame on which the flip was completed") / ConvertedToNonIFlip` [H `:552-585`] |
| **Hardware-composed independent flip / MPO** | the application is an overlay plane; DWM's desktop is plane 0 | WDDM 2.2, the MPO3 KMD DDIs, overlay caps | `KMTQAITYPE_MPO3DDI_SUPPORT` 43, `MPOKERNELCAPS_SUPPORT` 45, `MULTIPLANEOVERLAY_STRETCH_SUPPORT` 46 inside `>= WDDM2_2` [H `:2416-2420`]; `D3DKMT_MULTIPLANE_OVERLAY_CAPS` (`Version3DDISupport`, `RotationWithoutIndependentFlip`: "rotation, but without simultaneous IndependentFlip support", `Immediate`, `StretchRGB/YUV`) [H `:1339-1361`]; `D3DKMT_CHECKMULTIPLANEOVERLAYSUPPORT3`, `D3DKMT_PRESENT_MULTIPLANE_OVERLAY3` [H `:1229, 1313`] |

The last row is where the interactions are documented: the MPO caps struct carries a bit that exists only to say "this
rotation works with / without independent flip", which is the header's own statement that independent flip is a mode that
**coexists with MPO and does not require it**. The minimum for independent flip is therefore the Direct Flip surface at the
level this adapter already reports (**WDDM 2.1**, `wddm_surface.rs:64`); MPO is a separate, later step that forces WDDM 2.2+
(section 7, S-5).

The two `KMTQAITYPE` values are the cheapest instrument there is: user mode can read what dxgkrnl *derived* from the KMD's
caps without running any application (section 7, S-0).

### 2.2 The caps surface, field by field

| item | where | today | independent flip | evidence |
|---|---|---|---|---|
| `DXGK_DRIVERCAPS.SupportDirectFlip` | `query_adapter_info.rs:439-455` | 0 (`DirectFlipCaps` knob, `adapter/mod.rs:181-185, 277`) | 1 | exists in the 26100 layout [D], absent from the Win8 header [H8]; its meaning is [M] |
| `DirectFlip` flag on the **segments the allocations live in** | aperture descriptor `query_adapter_info.rs:726-748` and the three renderers `861, 914, 938, 950-963`; BAR knob bit 5 `:785, 805` | clear unless `DirectFlipCaps` | set. Adopted allocations are placed in the **aperture** segment (`create_allocation.rs:2237-2239`: `bar_eligible` is false for them), so the aperture's flag is the one that matters | KMD bindgen setter `set_DirectFlip`; the flag's role is [M] |
| `FlipCaps.FlipOnVSyncMmIo` | `:389` | set (load-mandatory: `:272-278`) | keep | [H8] `DXGK_FLIPCAPS` |
| `FlipCaps.FlipImmediateMmIo` | `:303`, reason `:377-387` | **deliberately clear** | keep clear: the MMIO contract requires the flip to be complete when the DDI returns, and a Helios flip is a virtio round trip that cannot run at the DIRQL the DDI arrives at (defect 0ab, measured) | [T] |
| `FlipCaps.FlipInterval` | not set | clear | consider in S-2: dxgkrnl then asks the driver to hold a flip for 2 to 4 intervals natively (`D3DKMT_FLIPINFOFLAGS.FlipInterval`, [H `:1887-1891`]); today dxgkrnl emulates | [H8] bit; use [M] |
| `MaxQueuedFlipOnVSync` | `:408-435`, knob `FlipQueueN` | 1 | 1 first, then 2 (section 4.4) | [T] |
| `MaxPointerWidth/Height`, `PointerCaps` | not written (the buffer is zero-filled, `:206`) | 0: no hardware pointer | probably needed (section 4.5) | fields in [H8] `:1073-1075`; need is [M] |
| `MaxOverlays`, `SupportMultiPlaneOverlay` | not written | 0 | 0 until S-5 | [D] layout; a host test forbids spelling them in `kmd_render` (`shared-formats.md` section "No overlay planes") |
| `WDDMVersion` / `DRIVER_INITIALIZATION_DATA.Version` | `wddm_surface.rs:64` | 2.1 + GpuMmu | stay 2.1: at 3.2 DWM fails `E_NOTIMPL` because MPO3 is not registered (`wddm_surface.rs:19-27`) | [T] |
| `D3DKMDT_VIDPN_SOURCE_MODE` pixel formats | `vidpn.rs:288-292` | `A8R8G8B8`, `A8B8G8R8` | same; 10-bit and fp16 need new source-mode formats (S-4) | [T] |
| path scaling / rotation support | `vidpn.rs:1016-1052` | identity + centered, rotation identity | same: **no stretch**, so the application's extent must equal the mode | [T] |
| `QueryVidPnHWCapability` | `display.rs:3803-3824` | all zero | same | [T] |

`DirectFlipCaps` is one knob for two things (the caps bit and the aperture segment flag) and is snapshotted once per
StartDevice so they cannot disagree (`adapter/mod.rs:100-117`). S-0 uses it unchanged.

### 2.3 The UMD gate: `CheckDirectFlipSupport`

[H] The runtime asks the UMD, per candidate, whether the application's swap-chain resource and DWM's can be flipped one for
the other, and whether the immediate (tearing) flavour is supported. D3D11.1 has `pfnCheckDirectFlipSupport(hDevice,
hResource1, hResource2, Flags, *pSupported)`; D3D12 has `_D3DDDI_DEVICEFUNCS::pfnCheckDirectFlipSupport`
(`PFND3DDDI_CHECKDIRECTFLIPSUPPORT`, offset 1016 in the bindgen).

[T] Both are inert here:

* D3D11: `check_direct_flip_support_11_1` writes `*supported = 0` and logs "-> no" (`umd/src/forward/transfer.rs:369-382`,
  wired at `tables.rs:267`).
* D3D12: **no assignment** of `pfnCheckDirectFlipSupport` exists anywhere under `umd12/src` (the grep is empty), so the slot
  keeps its zeroed default. What dxgkrnl or the runtime does with a null slot is [M], not known.
* The UMD also advertises MPO caps at the DXGI layer (`GetMultiplaneOverlayCaps` / `GroupCaps`, `umd/src/forward/present.rs:
  2306-2345`: `MaxPlanes`, RGB, bilinear, shared, immediate, stretch 16x) that nothing in the KMD backs. Its own comment says
  DWM may pick a composition strategy from them (`present.rs:2276-2282`) and that nobody has measured whether DWM queries them.

So for independent flip there are **three** doors, not one: the KMD caps (2.2), this UMD answer, and the resource's own
primary-ness (2.4). Opening only the first is what the 27th-session experiment did (section 2.7).

### 2.4 Allocation rules: what makes an application's buffer flippable

What is verified:

* The KMD decides "primary" from its own private-data flag, never from dxgkrnl: `MISC_PRIMARY` (`protocol/src/wddm.rs:85`) is
  written by `GetStandardAllocationDriverData` for `SHAREDPRIMARYSURFACE` (`create_allocation.rs:4414, 4525-4527`) or arrives in
  the UMD's own private data. It drives `AccessedPhysically` (`:2266` -> `:3526`: the allocation is made contiguous and
  physically addressable, which is what `SetVidPnSourceAddress.PrimaryAddress` needs) and the omission of `Cached`
  (`:2260`).
* The UMD sets that flag, and `D3DDDI_ALLOCATIONINFO2.Flags.Primary = 1` with `VidPnSourceId`, **only when the D3D runtime
  passed a `pPrimaryDesc`** (`umd/src/forward/resource.rs:254-262, 354-362`). Its comment: "DXGI rejects every Flip before
  DxgkDdiPresent with 'Source of Flip must be primary'". `MISC_DIRECT_SCANOUT` (zero-copy Venus bind) is added for
  `pPrimaryDesc` + format 28/87/88 (`resource.rs:955-993`).
* [H8] `DXGI_DDI_PRIMARY_DESC` has `Flags` including `DXGI_DDI_PRIMARY_OPTIONAL` ("the UMD has the option to prevent this
  Resource from ever being a Primary ... it can prevent the actual flip and use a copy operation, during Present") and an
  out `DriverFlags` bit `DXGI_DDI_PRIMARY_DRIVER_FLAG_NO_SCANOUT` ("the DXGI runtime will not employ flip-style presentation
  if this bit is set": `dxgiddi.h:182-210`). So **the DDI has an explicit notion of a not-yet-primary buffer that may become
  one**, which is the shape an application's flip-model chain needs.
* `foreign_flip::decide` (`kmd_logic/src/foreign_flip.rs:190-250`) has **no row about `MISC_PRIMARY`**: an adopted
  allocation of the mode's extent in one of the four 32-bpp formats is flippable whatever the UMD called it. Only the
  Venus direct arm requires `MISC_DIRECT_SCANOUT`, which implies primary.

What is not known, and what S-0 reads: **whether the runtime hands an application's windowed flip-model buffers a
`pPrimaryDesc`** (with or without `OPTIONAL`). The UMD already logs `primary_desc=<bool>` in
"DDI allocate_wddm_resource pre:" and `primary=<bool>` in the post line (`resource.rs:331-345, 389-409`); it should also
log `pPrimaryDesc->Flags`, `->DriverFlags` and `->ModeDesc` (section 8). The two possible worlds:

* **Tagged:** the buffers are primaries from creation. Nothing to change; `PrimaryAddress` is real; the only risk is table
  size (2.8).
* **Untagged:** dxgkrnl either never promotes them or flips them with a bogus `PrimaryAddress`. The latter is dangerous: a
  zero address publishes nothing, so no `CRTC_VSYNC` can retire the flip and dxgkrnl is held (`zero-copy-present.md` 13.3,
  "a zero physical address publishes nothing"). The decision table refuses it (`Why::NoAddress`). Fixes, to be chosen after
  S-0: (A) the UMD marks them primary through `pPrimaryDesc` with `DXGI_DDI_PRIMARY_OPTIONAL` semantics (cleanest; the DDI
  was designed for it); (B) the KMD sets `AccessedPhysically` for any adopted allocation created with `D3D10_DDI_BIND_PRESENT`
  (0x80, which the UMD already tests as `DDI_BIND_PRESENT`, `resource.rs:203`) -- whether dxgkrnl then agrees to flip a
  non-`Flags.Primary` allocation is [M] and must be measured.

Format and size rules the KMD enforces regardless (already in the code; the table of section 6 reuses them):

| rule | where | note |
|---|---|---|
| extent equal to the committed mode | `display.rs:3169-3171` (before any arm) and `foreign_flip::decide` row `Extent` | no stretch path exists (2.2); a mismatching flip is completed as a kept picture |
| one of three 32-bpp formats: `R8G8B8A8_UNORM` (28), `B8G8R8A8_UNORM` (87), `B8G8R8X8_UNORM` (88) | `ScanoutFormat::from_dxgi` (`kmd_logic/src/lib.rs:627-670`); `vidpn.rs:288-292` publishes A8R8G8B8 and A8B8G8R8 | 10-bit, fp16, YUV, sRGB-typed aliases are refused: foreign `SharedFormat` (`FfRef15`), Venus `Format` |
| stride/offset/size: `pitch >= 4*width`, `pitch & 3 == 0`, `plane_offset <= u32::MAX`, `alloc_size >= plane_offset + pitch*height` (saturating) | `ScanoutTarget::from_direct_primary` (`create_allocation.rs:962-988`), shared as `snapshot_bind::validate_layout` | the undersize guard that keeps the host from reading past the blob; never relaxed |
| foreign layout: the KMD's record (extent >= 64, modifier, fourcc, stride, offset) | `foreign_resource`, `rm_sysmem::flip_layout`, `foreign_flip::decide` row `BadLayout` | the creator's trailer is not trusted for a foreign allocation (`shared-foreign-surfaces.md` section 2) |

### 2.5 The DDI sequence

Verified shapes ([H8] for the miniport, [H] for the thunk side):

1. `DxgkDdiPresent` with `DXGK_PRESENTFLAGS.Flip` (bit 2) and `FlipWithNoWait`, `FlipInterval`, `pDmaBuffer`,
   `pAllocationList` (source/destination slots). Two contracts, chosen by dxgkrnl from the flip interval and the caps
   (measured at `query_adapter_info.rs:359-370`): interval >= 1 with `FlipOnVSyncMmIo` is the **MMIO flip** (`pDmaBuffer ==
   NULL`, Present generates nothing; `SetVidPnSourceAddress` follows); interval 0 is the **DMA-buffer flip** (non-NULL
   `pDmaBuffer`; `SetVidPnSourceAddress` is never called; the driver programs the display when the buffer executes and the
   submission fence is the completion).
2. `DxgkDdiSetVidPnSourceAddress(VidPnSourceId, PrimarySegment, PrimaryAddress, hAllocation, Flags)`, flags
   `ModeChange 0x1`, `FlipImmediate 0x2`, `FlipOnNextVSync 0x4` [H8 `d3dkmddi.h:2845-2882`]. May be called at DIRQL under
   `DxgkCbSynchronizeExecution`. The KMD stores the flags (`display.rs:1935`) and branches on none of them.
3. `DxgkDdiSetVidPnSourceVisibility` (a no-op accepting, `display.rs:1839-1853`) and `DxgkDdiCommitVidPn` /
   `UpdateActiveVidPnPresentPath` are mode-set era DDIs: an independent-flip transition changes the *source address*, not
   the VidPn. No `CommitVidPn` is expected on promotion or demotion [M].
4. Completion: a `DXGK_INTERRUPT_CRTC_VSYNC` whose `PhysicalAddress` names the new front buffer retires the queued flip
   (`submit_command.rs:889-902`, `kobj.rs:732`; the strictness of address matching is unobserved: `zero-copy-present.md`
   13.4 item 1). A DMA-buffer flip additionally retires on its DMA fence.
5. The flip-model **present history token** (`D3DKMT_FLIPMODEL_PRESENTHISTORYTOKEN`, [H `d3dkmthk.h:639-688`]) and the
   `IndependentFlip*` stage bits are dxgkrnl <-> DWM protocol. The KMD never sees a token: there is no reference to a present
   history type anywhere in `kmd_render/src`. What the KMD can observe of the stages is only which allocation the next
   flip names.
6. `D3DKMT_PRESENTFLAGS.Flip`, `FlipDoNotFlip`, `FlipDoNotWait`, `FlipRestart` [H `:395-436`] and the MPO flags
   `TrueImmediate` ("if a present interval is 0, allow tearing rather than override a previously queued flip") [H `:1304`]
   are the user-mode spellings of what becomes `FlipWithNoWait` / interval 0 at the DDI.

### 2.6 What independent flip does not need

Not MPO, not WDDM >= 2.2, not `FlipImmediateMmIo`, not CommitVidPn, not new DDIs. The MPO arm is refused by construction
(`display.rs:273-280`, `present_packet.rs:770-820`: `PresentPayload::MultiPlaneOverlay` is a named refusal) and must stay so.

### 2.7 History: the legacy `SupportDirectFlip` advertisement and why it was turned off

[T `query_adapter_info.rs:439-453`; D `ROADMAP_HISTORY...:2855-2880`] Until the 27th session (2026-07-07) the KMD reported
`SupportDirectFlip = 1` with the three aperture `DirectFlip` flags, an unbacked bring-up value copied from the viogpu3d
sample ([D `WDDM_RENDER_ONLY_3_2.md:337`]). At that time the display was an IddCx driver capturing DWM's composed output and
the KMD scanned out nothing. The observed symptom was a two-stale-frame alternation, cured by any dirty-region recompose;
the theory was that DWM promoted an eligible visual (flip-model, ignore-alpha, unoccluded) and stopped composing it while
every fence stayed green. The caps were denied behind `DirectFlipCaps`. **Owner verdict, same entry: "NO CHANGE -- direct-flip
denial falsified as the mechanism"**; the real cause was DXVK command-list cadence. So:

* the evidence that DWM *did* promote on that surface is weak (a theory that was falsified as the cause of the symptom, and
  the UMD was already denying `CheckDirectFlipSupport`);
* the reason the denial is truthful today is different and still valid: with no real scan-out a promoted window would not be
  composed by anything that reached the screen. That premise has changed: there is a display half, a flip path and a host
  scan-out. The comment in `query_adapter_info.rs:439-453` ("Helios has zero scanout (all VidPn DDIs NOT_SUPPORTED)") is
  stale and should be corrected when this work starts.

### 2.8 The direct-scan-out table is a prerequisite

`SCANOUT_ALLOCS` has 32 slots keyed by venus resource id (`create_allocation.rs:1579-1603`); the DMA-buffer flip resolves its
source through it (`display.rs:1160`). It was sized for "DWM rotates 3 and an app's flip chain 2-4". With `ForeignFlip` on,
**every** adopted foreign allocation registers (`create_allocation.rs:3450-3452`), i.e. the buffers of every flip-model window
on the desktop; a full table is counted (`ScAlcFul`) and the flip's route becomes `Fail` or `Skip`. Independent flip makes this
table load-bearing for the application, so it grows (64 or 128; the slot is 24 bytes) in S-1.

## 3. How the existing pieces map (question 2)

### 3.1 Identity: which allocation, whose, how the flip finds it

* The flip names an allocation by `hAllocation` (MMIO, in `SetVidPnSourceAddress`) or by the Present's allocation-list
  source, the *device-specific* open handle (DMA). The KMD resolves the first through `AllocationContext*`
  (`scanout_alloc_info`, `create_allocation.rs:1219-1266`) and bridges the second through the table above, keyed by resource id
  (`:1160`) because the open handle is not the global one (`:1154-1159`).
* A foreign (NVK) allocation is recognised by the KMD's **adoption record**, never by what the creator wrote
  (`flip_completion::classify`, `kmd_logic/src/flip_completion.rs:38-60`; `create_allocation.rs:1268-1278`).
* An application swap chain differs from DWM's in exactly two ways the KMD can see: the **importer** (the application's NVK
  device and DRM file, not DWM's `g_ctx` in `dwm.exe`) and the **lifetime** (the application's, not the session's). Both are
  already modelled: `foreign_flip::Book::set` returns `Change::Moved` (another allocation of the same device) or
  `Change::Reowned` (another device's: the arbiter gives the resident source a new generation, and the previous owner's flip
  in flight is refused `NoSource`; tested against the real arbiter, `kmd-rm-client.md` 15.18.3).
* One thing is **not** modelled: whether the flipped allocation belongs to DWM or to an application. The KMD cannot tell, and
  does not need to for correctness. It matters for observability only (`IdfSwitch`, section 6.4).

### 3.2 Which arm shows it

| source | arm that shows it | zero-copy | needs | today's gate |
|---|---|---|---|---|
| NVK-on-RM buffer (adopted, `IMPORT_RM`, DEVICE_MEMORY) | `ForeignFlip` (`virtio/foreign_flip.rs:586`): `ScanoutFlip` of the importer's `(DRM file, GEM)` through the arbiter's resident source | yes | `ForeignFlip=1`; extent == mode; 32-bpp; importer's file open; not `MISC_DIRECT_SCANOUT`; level 0/1/2/5 | knob |
| Venus buffer created as `pPrimaryDesc` + 28/87/88 | `program_vidpn_source_inner` direct arm: `SET_SCANOUT_BLOB` of the allocation's own blob (`display.rs:3277-3293, 3358-3611`) | yes | `MISC_DIRECT_SCANOUT`; undersize guard | always on |
| any other Venus buffer | `production_linear_scanout` + `submit_primary_scanout_copy`: a GPU copy into the adapter's LINEAR scan-out image (`display.rs:69-149, 3750-3783`) | no | resource id, extent == mode | always on |
| no resource id (host-less shared placeholder) | none: completed as a kept picture (`present_flip_kept`, `display.rs:1288`) | n/a | n/a | always |

Independent flip adds **no arm**. It changes how often a non-DWM allocation reaches these arms.

### 3.3 Venus applications

A Venus application's flip-model buffers are Venus resources the UMD created through DXVK. If they are created as
`pPrimaryDesc` + 28/87/88, they are `MISC_DIRECT_SCANOUT` and the existing direct bind applies. This is exactly the path an
exclusive-fullscreen application already takes: the Fire Strike census at `query_adapter_info.rs:359-370` (measured with the
immediate-MMIO advertisement that was later retired as a regression, 1151 flips and 948 `SET_SCANOUT_BLOB`) is what showed
that the application's flips arrive at interval 0 with a DMA buffer, which the DMA lane (`PresentFlipPrivate` +
`arm_dma_flip`, the D4b snapshot, the producer watermark machinery of `zero-copy-present.md`) now serves. So **for Venus the machinery for "an application's own buffer is the
primary" is the oldest, best-measured path in the driver**; only the promotion is new. Everything the KMD learned about it
(the 0ab-B black frames, bind ordering, leases, `FlipImmediateMmIo` being a regression) applies unchanged.

Why the legacy `DirectFlipCaps` advertisement was off for these: see 2.7. It was never a statement that the Venus flip path
could not show an application's buffer.

### 3.4 The user foreign-scanout source (`SCANOUT_SET` / `SCANOUT_PRESENT`) and the HOSC tag

The NVK application's own scanout path (the 247 fps reference) is an arbiter source with priority over the resident source
that `ForeignFlip` registers: a user `SCANOUT_SET` preempts the resident source at once, which parks and later re-flips
(`foreign-scanout.md`, "The KMD's own resident source"; `kmd-rm-client.md` 15.18.3 table). Consequences for independent flip:

* **Both active:** if an NVK application uses `SCANOUT_SET` and dxgkrnl also promotes its swap chain, the user source holds
  the screen and the promoted chain's flips are completed while the resident source is yielded (`FfYielded`). Nothing breaks,
  but the application then pays for both. The decision table reports it (`Why::UserSource`).
* **Recommendation:** an application uses one of the two. The independent-flip route is the one that works for every
  application without NVK cooperation; the user source stays as the NVK-specific fast path until independent flip is
  measured to match it (S-2 criterion).
* The already-on-scanout tag (`kmd/present-onscanout`, commits `9f6b070` / `d812a49`, **not in this base**; read from the
  commit messages) skips the whole-frame Blt that follows a frame already shown by a user source. It refuses a tag on a flip on
  purpose ("a flip copies nothing and its completion invariant must run", its reason 6). Independent flip is the flip case it
  leaves alone. The tag is the lever for a *Blt-presenting* application; independent flip is the lever for a *flip-presenting*
  one. Which one FFXIV hits is "read, not assumed" there (`PBflag` bit 0 vs bit 2); S-0 settles it from the same counters.

### 3.5 Completion, release and reuse: the transitions

The flip-completion invariant (`zero-copy-present.md` 13.2) is what makes transitions safe, and it is why independent flip
needs no new completion code: **the KMD owns flip completion toward dxgkrnl, and every flip completes**. A source the
programming cannot show completes as a *kept picture*: the address moves (one atomic store, legal at any IRQL,
`publish_kept_primary`, `adapter/scanout.rs:205`), the screen keeps what it showed, no bind or refresh is requested.

Transition table (all rows are existing code; the only new fact is who the allocations belong to):

| event | what dxgkrnl does [M] | what the KMD sees | outcome |
|---|---|---|---|
| promotion: DWM chain -> application chain | flips the application's buffer | `SetVidPnSourceAddress(hApp)` (MMIO) or a DMA flip of the application's source | `ForeignFlip::take`: `Change::Reowned`; the arbiter's resident source gets a new generation; address published; `FfReowned`+1. Venus: bind of the new blob |
| steady state | one flip per frame of the application's 2-4 buffers | as above, `Change::Moved` | `FfMoved`; the single pending slot coalesces (`VpCoal`); newest wins |
| demotion (overlapping window, alt-tab, toast) | asks DWM to take over (`IndependentFlipRequestDwmExit` [H name]); DWM composes; flips its own chain back | `SetVidPnSourceAddress(hDwm)`: `Reowned` again | symmetric; DWM's chain content is stale until DWM renders once, which is DWM's job |
| application destroys the swap chain while shown | | `DestroyAllocation` -> `retire_scanout_allocation_locked`: cancels the pending handle by CAS (`scanout.rs:1202-1230`), `foreign_flip::target_gone` | `FfGone`; the worker withdraws the resident source; the screen holds the last frame until the next programming |
| application dies (`TerminateProcess`) | destroys its contexts and devices | `DestroyDevice` entry hook, `foreign_scanout_owner_exit` (`adapter/foreign_scanout.rs:439`), forwarded `Close` of its DRM file, poison of its records (`FfPoison`) | flips of its allocations are refused `FileClosed` and completed kept; the screen holds the last frame until dxgkrnl flips back |
| DWM restarts | | its importer's file closes: poison + `FfGone`; new chain adopts | `kmd-rm-client.md` 15.18.11 step 9 |
| mode change | demotes; DWM re-creates its chain | `CommitVidPn`; stale application buffers: extent != mode | `FfRef11`; kept |
| transport reset / device restart | | `retire_transport` -> `foreign_flip::forget` | cold start |

**Reuse.** The application reuses a buffer once dxgkrnl retires the flip that replaced it, i.e. once the vsync heartbeat has
reported the *next* address. The KMD publishes the displayed address **at programming**, before the host has flipped
(`foreign_flip::take`: `publish_bound_primary`, `virtio/foreign_flip.rs:603-640`); the host's release (`ScanoutReleased`,
msg 28; the release book, `kmd_logic/src/scanout_release.rs`) is *recorded but not read* (`kmd-rm-client.md` 15.18.5, the
"conservative rule"). The protection is chain depth; the residual hazard is a viewer still sampling the previous buffer one
flip later: **tearing, never corruption**. With `FfAsyncWin` the publication may run ahead of the host by the window (1 to 4
flips). An independent-flip chain is shallower than DWM's 3-deep desktop chain (a game commonly runs 2 buffers), so this is
the first place the rule may bite. The lever already exists as a design item (15.18.5 step 8): hold the publication until the
replaced flip is released. S-2b makes it a knob, `IdfHoldRel`, with a timeout; it costs up to one host round trip of latency
per frame, which is why it is off until the tear rate is measured.

## 4. The present path (question 3)

### 4.1 What happens today to a flip, by contract and source

`dxgkddi_present_inner` (`display.rs:227`) decides the arm from the flags alone: `Flip` clear -> Blt; `Flip` set with
`pDmaBuffer == NULL` -> `FlipMmio`; non-NULL -> `FlipDma` (`:249-255`). An MPO payload is refused (`:273-280`).

**MMIO flip (`pDmaBuffer == NULL`).** Present resolves the source (`present_alloc_info`); an unresolved handle that is the
host-less placeholder succeeds, any other fails `PBFlip` 0xE1 (`:1034-1071`); a source whose format is unresolved is a counted
foreign skip or 0xE2 (`:1075-1088`); otherwise it **returns success at once** (`:1127-1133`, `PBMmio`). Nothing is decided
about the source here. `SetVidPnSourceAddress` follows (DIRQL): `set_vidpn_source_address_dirql` pairs the handle with the
address and raises the programming gate (`display.rs:2321-2366`); an unpaired handle publishes the address kept and returns
`STATUS_INVALID_PARAMETER` (`:1954-1965`, `FkDdi`, `VpPrF`); a paired one is stashed in the single `pending_vidpn_allocation`
slot and the PASSIVE worker is woken (`:1968-2000`). The worker runs `program_vidpn_source_inner` (`:3122`): extent check
(`:3169`), level 5 sysmem arm (`:3179`), `ForeignFlip` (`:3207`), then the Venus direct bind or copy (`:3277-3783`).

**DMA flip (`pDmaBuffer != NULL`, interval 0).** The source is looked up by resource id in `SCANOUT_ALLOCS` (`:1160`) and
routed (`present_foreign::flip_route`, `kmd_logic/src/present_foreign.rs:368`; the table in `zero-copy-present.md` 12.6):

| in table | class | route | what the Present does |
|---|---|---|---|
| yes | direct-scan-out | `Arm` | `PresentFlipPrivate::write` (`:1241`); `arm_dma_flip` at submit (`submit_command.rs:1179`) -> `arm_dma_flip_programming` (`display.rs:2031`) + `fast_bind_from_flip` |
| yes | foreign, `ForeignFlip` on | `Arm { foreign_flip }` (`PrFgHand`) | same; the programming reaches the `ForeignFlip` hook |
| any | foreign, not in table, or knob off | `Skip` | counted success; keep record; completed kept (`present_flip_kept`) |
| yes | other Venus | `Arm` | as direct |
| **no** | **any other Venus allocation** | **`Fail`** | **`PBFlip` 0xE6, `STATUS_INVALID_PARAMETER`: dxgkrnl fails the present** (`:1207-1211`), unless the allocation is `Hollow` (completed kept) |

The bold row is the one an application's non-direct Venus buffer hits when dxgkrnl issues an interval-0 flip of it. Today that
never happens (no application's buffer is flipped). Under independent flip it can, and failing the Present is the wrong answer:
the doc's rule is that a flip dxgkrnl chose to issue **completes** (section 6, `Why::NotRegistered` -> kept).

### 4.2 What must be added

Nothing on the flip arms. In order:

1. **Observation first** (S-1, behaviour unchanged): the decision table evaluated and counted (`IndepFlip=1`), no verdict
   enforced. This answers "what do application flips look like at the KMD" with numbers instead of reading.
2. **Enforcement** (S-2, `IndepFlip=2`): `Keep(why)` completes as a kept picture instead of failing; `Copy` and `Direct` route
   to the existing arms; `NotRegistered` for a Venus application no longer fails 0xE6.
3. A larger `SCANOUT_ALLOCS` (2.8).
4. The release hold (3.5) and the pacing settings (4.4), both knobs.

### 4.3 Tearing and immediate flips

dxgkrnl issues interval 0 as a DMA flip, with the same `Flags` as a vsync-synchronised one (the census at `query_adapter_info.rs:
359-370`); `DXGI_PRESENT_ALLOW_TEARING` reaches it as `TrueImmediate` / `IndependentFlipTrueImmediate` [H names]. The KMD
ignores `FlipImmediate` in `SetVidPnSourceAddress.Flags`. Its behaviour for an immediate flip is "arm the programming, coalesce
into the one pending slot, newest wins, show at most one flip per refresh period (`rm_refresh::flip_interval_100ns`)". So an
application presenting at 500 fps with `ALLOW_TEARING` shows at most the refresh rate and is not throttled by it (the DMA
fence retires behind the programming), which is the right semantics for a viewer that cannot tear a real scan-out anyway.
**What the host does with an immediate flip is a viewer property** (`docs/SCANOUT.md` "Viewer": direct mode, tearing
`ASYNC`, a global toggle). A per-flip hint is a host item (section 8).

### 4.4 Pacing, the vsync heartbeat, and 240 Hz  [Derived]

A flip retires when a `CRTC_VSYNC` reports its address. Per tick (4.17 ms at 240 Hz):

* the heartbeat is one-shot, fixed-phase, runs whether or not dxgkrnl enabled delivery, and delivers only while
  `vsync_enabled` (set by `DxgkDdiControlInterrupt`, `interrupt.rs:460-486`; `kobj.rs:709-732`). dxgkrnl enables it while a
  flip is queued or something waits on a vsync, so an independent-flip application keeps it on by presenting;
* **MMIO contract:** tick k -> dxgkrnl issues flip N (`SetVidPnSourceAddress` at DIRQL) -> the single slot is armed -> the
  PASSIVE worker must run `take`, which publishes the address -> tick k+1 reports it -> dxgkrnl retires N and may issue N+1.
  The worker is woken **by the vsync tick alone** unless `FfAsyncWin` is on (`kmd-rm-client.md` 15.18.13.2: "up to one refresh
  period of latency"). Then the address is published *after* tick k+1's report, reported at k+2, and with
  `MaxQueuedFlipOnVSync = 1` dxgkrnl cannot issue N+1 before N retires: **a ceiling of one flip per two ticks, 120 flips/s at
  240 Hz.** This is derived from the code and the queue-depth comment (`query_adapter_info.rs:408-435`), not measured; it is
  consistent with, but does not explain, the 126 fps composed figure. With `FfAsyncWin >= 1` the DDI requests a DPC at once
  (`display.rs:1996-1999`, `early_wake`) and the worker publishes within its own latency. **So `FfAsyncWin` is a hard
  prerequisite for more than 120 fps on the MMIO contract.** It is default 0 and unmeasured.
* **DMA contract:** `arm_dma_flip_programming` wakes the worker immediately (`signal_hpd`, `display.rs:2140`); no tick latency.
* `MaxQueuedFlipOnVSync`: keep 1 at first. A deeper queue (`FlipQueueN`) lets dxgkrnl issue N+1 before N retired; the single
  pending slot then coalesces N away, `last_primary_address` jumps past N, and whether dxgkrnl retires a flip whose address
  was skipped is exactly the unobserved question of `zero-copy-present.md` 13.4 item 1. Try 2 only after the baseline.
* The host leg: `ScanoutFlip` is one in-order control-queue message among all others; a flip's round trip sustains 240 Hz only
  while it stays under about 4 ms (`kmd-rm-client.md` 15.18.13.1 table). `FfRttUsSum / FfRttN` and `FfRttUsMax` settle it.
* The watchdogs (`VsWatchdog`, `FlipWdogMs`, `DeferBudget`; `zero-copy-present.md` 14, 15.4) are the safety nets for a dead
  heartbeat and a stuck flip; they stay as they are.

### 4.5 Cursor and overlay interplay  [open]

* **Pointer.** The KMD reports no hardware pointer and accepts `SetPointerShape` / `SetPointerPosition` as no-ops with the
  comment "the OS software-composes the cursor" (`display.rs:1720-1753`). A software cursor is drawn by DWM into the frame DWM
  presents. **Under independent flip DWM does not present that frame**, so nothing draws it. How dxgkrnl behaves when a driver has
  no hardware pointer and an independent flip is requested is not documented anywhere I have: it may refuse to promote, may
  compose the pointer some other way, or may show none. S-0 reads it (is the pointer visible while promoted?). If a hardware
  pointer is needed, the work is: report `MaxPointerWidth/Height` and `PointerCaps`; implement `SetPointerShape` /
  `SetPointerPosition`; and give the host the image. The host's cursor plane (`CursorUpdate`, msg 24, `docs/SCANOUT.md`
  "Hardware cursor") identifies the image by a Linux-guest GEM pair; a Windows guest needs a resource-id variant (host item).
  Position never travels: the host pointer positions itself.
* **Overlays.** `MaxOverlays` stays 0; there are no legacy overlays. MPO is S-5.
* **Hardware cursor + MPO** coexistence bits exist in the headers (`D3DKMT_DISPLAY_CAPS.CursorScaledWithMultiPlaneOverlayPlane0`,
  `CursorDoesNotSupportXorBlendWithMultiPlaneOverlay` [H `d3dkmdt.h:2486-2493`]); irrelevant until S-5.

## 5. Fallbacks and safety (question 4)

### 5.1 How promotion ends [M], and what the KMD must tolerate

dxgkrnl and DWM end an independent flip when the conditions that made it legal stop holding: another window overlaps the
output; alt-tab, the Start menu, a notification; a mode change; the window is not unoccluded; the chain's format, size or
rotation changes; the content is protected (`RestrictedContent` [H name]); monitor power; HDR changes; capture. The header
names the protocol: `IndependentFlipRequestDwmExit`, `...RequestDwmConfirm`, `...ForceNotifyDwm`, `...DoNotFlip`, and the
flip manager's `ConvertedToNonIFlip` ("an IFlip submitted token was subsequently cancelled and should be resubmitted as
non-IFlip") [H `d3dkmthk.h:482-493, 583-584`]. The KMD is told none of it. It sees flips naming allocations.

The KMD therefore tolerates, and already does: the shown allocation changing between DWM's chain and any application's, at
any flip, with no CommitVidPn; any of those allocations being destroyed or its owner dying at any instant (3.5); the importer's
DRM file number being reused (the poison rule, `kmd-rm-client.md` 15.18.6); a flip of an allocation of an older transport
generation (refused, completed kept: `FkDdi`); a device restart mid-flip (`forget`).

### 5.2 Failure behaviour

* **Never fail the flip.** Every refusal ends as a kept picture (13.2): the address moves, the screen keeps its picture. The
  only exits that fail a Present are structural (a null argument, an unresolved handle that is not a placeholder: `PBFlip`
  0xE1; the unregistered Venus row of 4.1, which section 6 removes).
* **The cost of a kept picture under independent flip is visible**: DWM is not composing, so a refusal freezes the
  application's last frame with no desktop behind it, until dxgkrnl demotes. The pauses end by themselves
  (`foreign_flip::failing`: 100 ms strike, five seconds after three), after which flips are shown again.
* **The KMD cannot demote.** There is no driver-to-dxgkrnl "stop independent flip" in anything I can read. The only levers
  are the answers dxgkrnl asks for: caps (read once, at AddAdapter) and the UMD's `CheckDirectFlipSupport`. A sticky
  KMD-to-UMD deny (an escape the UMD reads when asked) after repeated refusals is an S-3 option; whether dxgkrnl re-asks is
  unknown.
* **Counters** record every verdict (6.4), so a frozen promoted application is a number, not an anecdote.

### 5.3 Security

* An application's allocation becoming the scan-out gives it nothing it does not already have under exclusive fullscreen: the
  host reads the guest's own pixels. The route never names another process's memory: `ScanoutFlip` carries the importer's
  DRM file and GEM, and the arbiter proves (at `mint`, every flip) that the file is that device's in this generation; a
  closed or reused file number is refused (poison rule, `FfStale`, `FfPoison`).
* For a foreign allocation the extent, format and layout are the **KMD's record**, not the creator's words
  (`shared-foreign-surfaces.md` section 2); for a Venus direct allocation the creator's words are checked by the undersize
  guard, so a lying `MISC_DIRECT_SCANOUT` can make the host read past nothing and can only break its own picture.
* A promoted application can hold the screen (a frozen last frame). dxgkrnl's own demotion (alt-tab, Win key) is the escape,
  as for any fullscreen application. Not new.
* Security is last in the project's priorities (perf, then zero copy); this section records only that no new trust edge is
  created.

## 6. The decision table (scaffolding: `kmd_logic/src/independent_flip.rs`, not wired)

### 6.1 What it is for

One pure function, `decide(&Facts) -> Verdict`, answering for each flip: `Off` (inert: knob off, caps not advertised, or not a
flip), `Direct(Foreign | VenusBind)`, `Copy`, or `Keep(Why)`. It sits in front of the three existing arms, replaces no logic
(foreign sources are judged by `foreign_flip::decide`, whose verdict is an input; the Venus layout guard is
`snapshot_bind::validate_layout`), and gives S-1 a census and S-2 a policy. Sixteen host tests pin it (precedence, every
reason, the foreign-reason map, the undersize guard, names).

### 6.2 The rows (first match wins)

| # | condition | verdict | `Why` code, counter |
|---|---|---|---|
| 0 | `IndepFlip` off, or `SupportDirectFlip` not advertised this generation, or the arm is a Blt | `Off` | not counted |
| 1 | display half off | `Keep` | 1 `NoDisplay`, `IdfRef01` |
| 2 | class `Hollow` (no resource id / placeholder) | `Keep` | 2 `Hollow` |
| 3 | owner not live (creator/importer gone, file closed) | `Keep` | 3 `OwnerGone` |
| 4 | `PrimaryAddress == 0` | `Keep` | 4 `NoAddress` |
| 5 | `IdfNeedPrim` and the allocation is not `MISC_PRIMARY` | `Keep` | 5 `NotPrimary` |
| 6 | DMA contract and the source is not in `SCANOUT_ALLOCS` | `Keep` (today: `PBFlip` 0xE6 fails the Present) | 6 `NotRegistered` |
| 7 | a user `SCANOUT_SET` source holds scanout 0 | `Keep` | 11 `UserSource` |
| 8 | flips are failing (`foreign_flip::failing`) | `Keep` | 12 `Failing` |
| 9a | class `Foreign`: `foreign_flip::decide` took it | `Direct(Foreign)` | |
| 9b | ... refused (mapped: shared format -> `WideFormat`, extent -> `Extent`, bad layout -> `Layout`, closed/destroyed/not adopted/owner gone -> `OwnerGone`, failing, no display; ring level, no transport, host capability, KMD-owned, direct flag -> `ForeignOther`) | `Keep` | 7, 9, 10, 3, 12, 1, 13 |
| 10 | class `VenusDirect`: wide format -> `WideFormat`; other format -> `Format`; extent != mode -> `Extent`; undersize guard -> `Layout`; else | `Direct(VenusBind)` | 9, 8, 7, 10 |
| 11 | class `VenusOther`: extent != mode -> `Extent`; else | `Copy` | 7 |

The order is the doc's: inert states, environment, the source's identity and life, the contract, who else holds the screen,
then the pixels. Codes are dense 1..13 and pinned by a test; append, never renumber.

### 6.3 What it deliberately does not do

It does not look at `FlipImmediate`, the interval or the present flags (the KMD ignores them today; adding a dependency on
them would be a behaviour change dxgkrnl's contract does not ask for). It does not decide *who* owns the allocation (DWM or an
application). It does not know about HDR (`WideFormat` is a refusal, S-4 changes it). It is not told the mode's refresh rate:
extent equality is the only geometry rule, because the path supports identity and centered scaling only (2.2).

### 6.4 Knobs and counters (names chosen, **not implemented**)

All counters are `Idf*`, at most 14 characters, unique across `kmd_render` and `kmd_logic` (host test), event-gated with a zero
block published once per StartDevice (`zero-copy-present.md` 13.8 rules).

| name | meaning |
|---|---|
| knob `IndepFlip` | 0 off (default); 1 census only (the table runs and counts, nothing is enforced); 2 enforce |
| knob `IdfNeedPrim` | 1: refuse a source not created as a primary |
| knob `IdfHoldRel` | 1: hold the displayed-address publication until the host released the buffer the flip replaces |
| `IdfKnob` | bits 0..2 = the three knobs in force |
| `IdfSeen` | flips the table was asked about |
| `IdfDirect`, `IdfDirFor`, `IdfDirVen` | verdict `Direct`; through `ForeignFlip`; through the Venus bind |
| `IdfCopy` | verdict `Copy` |
| `IdfKeep`, `IdfWhy` | verdict `Keep`; its last reason code |
| `IdfRef01` .. `IdfRef13` | per reason |
| `IdfArmMmio`, `IdfArmDma` | asked on the MMIO / DMA contract |
| `IdfSwitch` | the shown source changed owner (a promotion or a demotion edge); should rise by 2 per round trip |
| `IdfHold`, `IdfHoldTmo` | publications held for a release / that gave up |
| `IdfUntagged` | direct flips of a source with `MISC_PRIMARY` clear (answers 2.4) |

Collision check done now (greps over `kmd_render/src`, `kmd_logic/src`, `protocol/src`, `umd/src`): no literal beginning `Idf`
or spelling `IndepFlip` exists; `If` is taken (`IfHi`), `Df` is a histogram stem in `stall_diag.rs:1469`, which is why the
prefix is `Idf`. Whoever wires this replaces `nothing_in_the_driver_spells_one_of_these_names_yet` with the
"the counters the driver writes are exactly the ones listed" pair the other modules carry.

## 7. A staged plan

Order from the host session: after T6 (DWM-on-NVK flips at the display rate), independent flip, MPO later. Test lowest mode
first, per the project rule: 1920x1080 at the low rate, then 5120x1440@240. Every stage is gated by a knob that defaults to
today's behaviour. Counters named in 6.4 do not exist yet; the `Vp*`, `Ff*`, `Fk*`, `Pr*`, `PBflag` ones do.

### S-0  Measure (no KMD behaviour change)

**S-0a: the caps echo (no code in the driver).** A user-mode probe in the style of `tools/adapter_type_probe.cpp` that calls
`D3DKMTQueryAdapterInfo` for `KMTQAITYPE_DIRECTFLIP_SUPPORT` (19, `D3DKMT_DIRECTFLIP_SUPPORT`), `INDEPENDENTFLIP_SUPPORT` (28),
`INDEPENDENTFLIP_SECONDARY_SUPPORT` (39), `MULTIPLANEOVERLAY_SUPPORT` (20), `_SECONDARY_SUPPORT` (38), `MPO3DDI_SUPPORT` (43),
`MPOKERNELCAPS_SUPPORT` (45), `SCANOUT_CAPS` (67), `DISPLAY_CAPS` (74) [all [H] names], with `DirectFlipCaps` 0 and then 1
(`reg add`, `pnputil /restart-device`). **Criterion:** which of the `Supported` values change with our caps. That is dxgkrnl's
own derivation, read before any application runs. The knob moves the caps bit and the aperture segment flag together
(`adapter/mod.rs:100-117`); telling which of the two dxgkrnl reads needs a one-line temporary build that clears one of them.

**S-0b: does promotion happen with the UMD unchanged** (it denies `CheckDirectFlipSupport`)? `DirectFlipCaps=1`,
`ForeignFlip=1`, `FfAsyncWin=2`, DWM on NVK, a borderless flip-model application at exactly the mode (`d3d11_triangle.cpp` is the
existing vehicle). Read PresentMon's `PresentMode`: "Composed: Flip" vs "Hardware: Independent Flip" [M names]; the KMD census:
`VpFlip`, `VpMmio`, `VpDmaF`, `VpDmaA`, `PBflag`, `PBFlip`, `PrFgWhy`, `FfProg`, `FfReowned` (expect >= 2 on a promotion),
`FkKeep*`, `SaLo`/`SaHi`, `VpPrF`. Expected: nothing changes (the UMD is the gate). If it *does* promote, that contradicts 2.3 and
is the most valuable result of the stage.

**S-0c: the UMD hook** (a UMD session item, tiny): `CheckDirectFlipSupport` answers TRUE when a debug registry value is set
and both resources are Helios-backed, the formats match, and the extent equals the current display mode; log `pPrimaryDesc->
Flags / DriverFlags / ModeDesc`. Run S-0b again. Questions it answers: does dxgkrnl promote at all (a); does it need the UMD
(b); are the application's buffers created with a `pPrimaryDesc` (c); which contract and intervals arrive (d); is a pointer
visible while promoted (e); what dxgkrnl does on demotion (f); is `PrimaryAddress` non-zero for the application's buffers (g);
the flip rate with and without `FfAsyncWin`, and `FfRttUsSum/FfRttN` (h). ETW: the DxgKrnl and Dwm-Core providers (GUIDs from
`logman query providers`), a GPUView-style capture for the flip stages.

Risk: a promoted application with no pointer and a refusing KMD freezes a test VM's screen. Mitigation: the VM's snapshot, a
registry kill-switch (`DirectFlipCaps=0` + restart), and the existing watchdog. Recommended first step: **S-0a**.

### S-1  Caps, allocation rules, census (`IndepFlip=1`: observe only)

* The table wired as an observer: evaluate, count, enforce nothing. `SCANOUT_ALLOCS` 32 -> 64/128.
* Allocation rule from S-0c: option 2.4(A) (UMD) or (B) (KMD `AccessedPhysically` for `BIND_PRESENT`).
* Pointer decision (4.5).
* Correct the stale comment at `query_adapter_info.rs:439-453`.

**Criteria:** no change in any existing counter versus S-0 (`FfProg`, `FkKeep`, `PBFlip`); `IdfSeen` equals the flip count
(`VpFlip`); the `IdfRef*` histogram is explained; `IdfUntagged` answers 2.4; the table-full counter `ScAlcFul` stays 0 with five
flip-model windows open. **Risk:** low (observation); the registry-mirror cost of new counters (event-gated, throttled).

### S-2  Route the application's flips (`IndepFlip=2`; ForeignFlip and Venus)

* Enforce the table: `Keep` completes kept (removes the 0xE6 failure for an unregistered Venus application); `Copy`/`Direct`
  as given.
* `FfAsyncWin=2` as the supported setting at >= 120 Hz; `FlipQueueN` 2 as an experiment.
* S-2b: `IdfHoldRel`. S-2c: a per-flip `IMMEDIATE` hint to the host (needs the host).
* Resolve section 3.4's precedence on a real NVK application: user source vs independent flip.

**Criteria:** PresentMon "Hardware: Independent Flip"; FFXIV at 5120x1440@240 reaches the NVK-scanout reference (~247 fps)
and the DWM process is idle; `FfFrames` ~ min(application rate, refresh rate); `FfFlipFail`, `FfGaveUp`, `FfStale`, `FkKeep`,
`IdfKeep` = 0 in steady state; `FfRttUsMax` < 20 ms; visible tearing with `IdfHoldRel=0` measured, then 1. **Risks:** the
pointer (4.5); the 120 fps ceiling if `FfAsyncWin` is off (4.4); reuse tearing (3.5); dxgkrnl retiring flips strictly by
address (unobserved).

### S-3  Transitions

Alt-tab, an overlapping window, a toast, Win+D, application exit, `TerminateProcess` mid-flip, DWM restart, mode change,
resolution switch inside the application, device restart, host viewer reconnect, a stream client connecting (mode policy,
`docs/SCANOUT.md` "Mode policy with several clients"). **Criteria:** no frozen DWM (flip completion); `IdfSwitch` +2 per
round trip; `FfPoison`/`FfGone` pair with the death; no `RfUnb` growth beyond a few; the screen is never black longer than one
refresh after a demotion; no bugcheck. Optional here: the sticky UMD deny (5.2).

### S-4  HDR and wide formats

`WideFormat` is a refusal today (`SharedFormat`, `FfRef15`; Venus `Format`). Needs: 10-bit `R10G10B10A2` (32 bpp, one plane: a
`ScanoutFlip` fourcc and a host viewer format, and a VidPn source-mode format and colour basis), fp16 (64 bpp: a new layout, a host
format, `D3DKMDT_CB_SCRGB` is already what the source mode declares, `vidpn.rs:260`), HDR metadata (`D3DDDI_HDR_METADATA_*` in the
flip token [H `d3dkmthk.h:664-670`]) and the monitor's colour capability. Each is a UMD + KMD + host change; the table's
`WideFormat` row is the single place the KMD flips.

### S-5  MPO (list only; not planned before S-2 is measured)

* WDDM level >= 2.2 (the OS then treats the adapter as a Display-Core/MPO3 device; today DWM fails `E_NOTIMPL`:
  `wddm_surface.rs:19-27`), the MPO3 KMD interface registered, `DXGK_DRIVERCAPS.SupportMultiPlaneOverlay` (offset 540 [D]),
  `MaxOverlays`, overlay plane caps (RGB planes only: `MaxYUVPlanes` and `StretchYUV` stay 0, YUV overlay planes stay unreported
  until the KMD and host can show them; `shared-formats.md` "No overlay planes").
* The MPO3 DDIs from [M]: check support (`DxgkDdiCheckMultiPlaneOverlaySupport3`), set source address with overlay
  (`DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3`), present through the `FlipWithMultiPlaneOverlay` payload
  (`DXGKARG_PRESENT.pPresentMultiPlaneOverlayInfo`; the KMD already refuses it by name, `present_packet.rs:770-820`). The
  user-mode mirror is [H]: `D3DKMT_CHECKMULTIPLANEOVERLAYSUPPORT3`, `D3DKMT_MULTIPLANE_OVERLAY3`, `D3DKMT_PRESENT_MULTIPLANE_
  OVERLAY3`, `D3DKMT_MULTIPLANE_OVERLAY_ATTRIBUTES3` (flags, blend, source/dest rects, rotation, stretch quality, HDR).
* Host: more than one scan-out plane (`ScanoutFlip` has `scanout: 0` only: "one scanout for now").
* The host test `the_kmd_never_advertises_overlay_planes` (`foreign_resource::shared_format_tests`) must be rewritten in the
  same commit that first writes any of those names.

## 8. Requirements for the other sessions

### 8.1 UMD / NVK / DXVK (requirements list)

1. **`CheckDirectFlipSupport`, both APIs.** D3D11.1 `pfnCheckDirectFlipSupport` (today `transfer.rs:369`, always no) and D3D12
   `_D3DDDI_DEVICEFUNCS::pfnCheckDirectFlipSupport` (a zeroed slot today). Answer TRUE only when: both resources are Helios-
   backed adopted `DEVICE_MEMORY` of this adapter; the application's format is one of 28/87/88 and equals the DWM chain's
   class; the application's extent equals the **current display mode** (query it; do not assume); single sample, one 2-D
   subresource; and, for `CheckDirectFlipFlags & D3DDDI_CHECKDIRECTFLIP_IMMEDIATE` [H], only when the DMA-lane immediate flip
   is supported. The UMD needs DWM's resource's properties: it opens it through the shared-surface identity
   (`shared-foreign-surfaces.md` section 2); the HFLY layout record is the source.
2. **Swap-chain buffers primary-compatible** (2.4): create them from the runtime's `pPrimaryDesc` where it is given (set
   `Flags.Primary` and `VidPnSourceId`, as `resource.rs:354-362` does); log `pPrimaryDesc->Flags/DriverFlags/ModeDesc` and the
   resource's bind flags; if the runtime passes none for application chains, say so (it selects fix (B)).
3. **Never `MISC_DIRECT_SCANOUT` on an NVK buffer** (`FfRef14` refuses it); never a foreign allocation without a complete HFLY
   record whose extent is the mode's.
4. **Keep the importer's DRM file open for the life of the swap chain** (librmclient `g_ctx`); a closed file refuses every later
   flip (`FfRef08`/`FfRef12`) and the screen freezes on the last frame.
5. **Order the frame**: the CPU-complete present marker (`zero-copy-present.md` 10.4, `value == 0`) before `pfnPresentCb`, as for
   DWM (15.18.5); this arm adds no wait.
6. **Do not use both paths**: an NVK application should not call `SCANOUT_SET` while its chain is promoted (section 3.4), and must
   not send the HOSC tag on a flip (refused, reason 6). How the UMD learns it was promoted is [M] (the flip manager's
   `independentFlipStage` out-field is in a header, [H `:571`]); not verified.
7. **Present flags**: `pfnPresentCb` private data is not forwarded to `DxgkDdiPresent` on flips (`display.rs:151-158`); any
   per-flip hint (an immediate/tearing request) must travel in the Render command stash (`HERF`), the existing channel.
8. **Formats for S-4**: R10G10B10A2 and fp16 swap chains need an agreed fourcc/layout record and a source-mode format before
   `CheckDirectFlipSupport` may say yes for them.

### 8.2 Host

1. **Resource-id flip** (`kmd-rm-client.md` 15.18.6): a `ScanoutFlip` variant naming the Venus resource id instead of
   `(owner DRM file, GEM)`, so a flip does not depend on the application's file staying open; removes the poison/handle-reuse
   class. Highest value for application chains, whose life is the application's.
2. **Immediate / tearing hint per flip** (`ScanoutFlip._reserved[0]`): the viewer shows that flip asynchronously. Today a global
   toggle (direct mode, `docs/SCANOUT.md` "Viewer").
3. **A flip path that is not queued behind blocking RM ioctls** (`kmd-rm-client.md` 15.18.13.4 (i)): at 240 Hz the flip RTT must
   stay under about 4 ms at the tail.
4. **Mode arbiter**: independent flip needs the guest mode to equal the application's extent; the viewer's fullscreen hint
   already requests the output's exact mode (`SCANOUT.md` "Dynamic resolution"), which is the right setup. A stream client's mode
   wins while it is active, which makes a chain of a different extent ineligible (composed): expected, not an error.
5. **Cursor image from a Windows guest** (resource-id variant of `CursorUpdate`) if S-0 shows a hardware pointer is needed.
6. **Release events**: `ScanoutReleased` for every flip is the input of `IdfHoldRel`; the viewer's compositor must release a
   buffer promptly (direct scan-out holds it).
7. Later: VRR (`VariableRefreshOverrideEligible` is a flip-token bit [H `d3dkmthk.h:495`]); second scan-out plane (S-5).

## 9. Verified from headers, versus from memory (the ledger)

**Verified from a header on disk** ([H] 26100 user-mode, [H8] Windows 8, [T] the tree):

* the `KMTQAITYPE_*` list and its WDDM-level conditions (19, 20, 28, 38, 39, 43, 45, 46, 67, 74); `D3DKMT_DIRECTFLIP_SUPPORT`,
  `D3DKMT_INDEPENDENTFLIP_SUPPORT`, `D3DKMT_MULTIPLANEOVERLAY_SUPPORT`;
* the flip-model present history token and its `IndependentFlip*` bits; `D3DKMT_FLIPMANAGER_AUXILIARYPRESENTINFO`;
  `D3DKMT_PRESENTFLAGS`; the MPO structures, caps (including `RotationWithoutIndependentFlip`) and present flags;
* `D3DDDIARG_CHECKDIRECTFLIPSUPPORT` and the device-function slot [H bindgen];
* `DXGKARG_PRESENT`, `DXGK_PRESENTFLAGS`, `DXGK_FLIPCAPS`, `DXGK_DRIVERCAPS` (to `SupportPerEngineTDR`),
  `DXGKARG_SETVIDPNSOURCEADDRESS` + flags, `DXGK_POINTERFLAGS`, `DXGI_DDI_PRIMARY_DESC` with `OPTIONAL` and `NO_SCANOUT` [H8];
* everything stated with a `file:line` in this tree.

**From memory, unverified (do not build on them without S-0):**

1. What `SupportDirectFlip` and the segment `DirectFlip` flag mean to dxgkrnl, and that both are required.
2. The definitions of Direct Flip vs Independent Flip vs hardware-composed independent flip, and that independent flip needs no
   MPO and nothing beyond the Direct Flip KMD surface.
3. That promotion needs an unoccluded, exact-mode, same-format, no-stretch flip-model chain.
4. That the runtime gives (or does not give) application flip-model buffers a `pPrimaryDesc`, and that dxgkrnl needs
   `Flags.Primary` to flip an allocation.
5. How dxgkrnl behaves with no hardware pointer under independent flip.
6. The demotion triggers and that no driver DDI can request demotion.
7. That no `CommitVidPn` accompanies promotion or demotion.
8. That `DxgkDdiSetVidPnSourceAddress` runs at DIRQL for every MMIO flip (the KMD's own comments say so; the WDK text is not on
   disk).
9. The PresentMon `PresentMode` strings and the ETW provider names.
10. The MPO3 KMD DDI names of S-5 and how the interface is registered.
11. What a null `pfnCheckDirectFlipSupport` means to the D3D12 runtime.

**Derived, not measured:** the 120 flips/s MMIO ceiling at 240 Hz without `FfAsyncWin` (4.4); the 32-slot table pressure (2.8);
the tearing exposure of a 2-deep chain (3.5).

**Unknowns that decide the plan:** (a) does dxgkrnl promote on our caps with the UMD gate open (S-0c); (b) are the application's
buffers primary-tagged (S-0c, `primary_desc=`); (c) is a hardware pointer required (S-0c); (d) does dxgkrnl retire a flip
whose address was coalesced away (`zero-copy-present.md` 13.4 item 1; relevant when `FlipQueueN` > 1); (e) the host's flip
round trip at 240 Hz under a game load (`FfRtt*`).

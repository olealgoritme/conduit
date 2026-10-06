# KMD handoff (2026-10-07, driver 22.22.343): what the code, the dumps and the sessions taught that is not elsewhere

`docs/HANDOFF.md` on main is the project-level handoff (goal and bar, what runs, the Blt retire analysis, the plan). This file is the
KMD-side supplement: how to read the counters without being fooled, why each approach was rejected, the known gaps, the plans that are
written down but not built, and how the KMD was developed without a WDK on the development host. The windowed-present design itself is
`zero-copy-present.md` section 24 (24.12 is the guest blob).

## 1. Windowed legacy-blt Present: the latency budget and why

Heaven D3D11, 1600x900, composed by an NVK DWM, one blt-model buffer (dxgkrnl's redirection surface is the Blt destination). Per frame, in
the order it happens:

| step | who | measured | where to read it |
|---|---|---|---|
| the app records and submits its frame | NVK/UMD | about 0.6-0.7 ms GPU | not a KMD counter |
| the Present DDI returns | KMD | 0.135 ms with `BltAsync` (1.06-1.55 ms legacy: CPU wait plus mirror) | `PrDdiBltUs / PrDdiBltN` |
| deferral: the copy cannot start before the producer finished | KMD worker | mean 0.61 ms | `BltDeferUs / BltAsyncDefer` |
| the ring-1 copy, KMD submit to the completion DPC | host and KMD | 0.5-1 ms (98%), of which the GPU copy is 0.2 ms | `BltAsyncLat0..7` (edges 250 us, 500 us, 1 ms, 2 ms, 4 ms, 8 ms, 16 ms) |
| the Present's DMA fence retires; dxgkrnl lets the next redirected Blt take CPU access | dxgkrnl | the app thread waits in `VIDMM_BEGINCPUACCESS_WAIT` (DxgKrnl ETW 41 to 42) 1.0-1.7 ms | ETW |

Reading: the Present cost the app sees (`msInPresentAPI` about 1.8 ms) is the retire latency of the PREVIOUS frame's Blt packet, which dxgkrnl
waits for on the same allocation. Removing the KMD's CPU wait (`BltAsync`) only moves that time from the DDI into the dxgkrnl throttle;
removing the CPU mirror (`GuestBlob`) removes 0.4-0.7 ms of real work. What remains is a per-frame GPU round trip through the guest.
Levers, in order: (1) a host-side dependency on the producer's RM fence, so the copy is submitted at Present and starts when the producer is
done (removes the deferral and the worker hop); (2) the host's own ring-1 round trip, 0.3-0.8 ms beyond the 0.2 ms copy; (3) MSI-X
(section 5); (4) not changeable from the KMD: dxgkrnl serialises each frame behind the previous copy into the one surface.

## 2. Rejected or parked, and the evidence

| approach | verdict | evidence |
|---|---|---|
| `BltNoMirror=1` with an NVK DWM | do not use | the window freezes: DWM reads the destination through dxgkrnl's CPU view of the system pages (it opens no KMD standard allocation: `StdOpenPid` was Heaven, `StdOpenN` 6, and its UMD log has no kind=2 opens); 24.11.5 |
| `BltAsync` with the mirror on the worker | worse than legacy | 185 fps against 206: 1.6 ms of mirror on the worker plus a 2.67 ms deferral |
| route D of S-A (give the Venus standard buffer a foreign record so NVK imports it) | wrong lever | DWM does not open the buffer, `rm-backed-standard.md` "S-A0 result" |
| level 5 / route R (RM system memory behind the destination) | removes nothing as built | `sysmem_blt` GPU-copies into a Venus staging image and CPU-copies into the RM memory, and it skips eviction, which this allocation demonstrably sees (`PgTo` 1); `rm-backed-standard.md` 13.2 |
| Windows' flip-model upgrade (`SwapEffectUpgradeEnable`) | parked, flaky | the gate is DXGI's game classification (`WINDOWEDSWAPEFFECTUPGRADE_REASON_NONGAME` in the `DXGI_ETW_SWAPCHAIN_CREATE` event); a per-app value in `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` upgraded `d3d11_triangle` some of the time and never Heaven (single buffer, `ALLOW_MODE_SWITCH`, 32-bit); the FlipCaps rows (`FlipCapsX` 0x10, 0x30 with `DirectFlipCaps`) never changed anything; the DXGI message "Failed to find an output for the swapchain" is a separate benign event (the output exists: `\\.\DISPLAY2`, Helios, attached) |
| udmabuf / dma-buf import of guest pages on NVIDIA | does not work | the host prototype; `VK_EXT_external_memory_host` over the memfd mapping does (0.206 ms copy, 0 bad pixels, even scattered pages) |
| `GuestBlob` import cost | one-off | 1.9 + 2.9 ms per destination; created lazily on the first covered Blt, never inside the paging callback |

## 3. Counter traps (each one cost a misread)

* `PgSm` and every `Pg*` value are published only when a paging operation flushes the paging counter block (every 64th, plus on a failure
  change). A flat `PgSm` over a window with no paging means a stale value, not "no mirrors". Live evidence of the Present-side mirror is
  `PBSyCp` (written per Blt: 1 copied, 2 no backing so the blob is authoritative, 3 skipped, 0 not a buffer destination, `0xE1` failure),
  `BltMirrorN`, `BltMirrorUs`.
* Several blocks publish only with the NVRM counters (throttled, escape-driven): `Fc*`, `Blt*`, `Gb*`. A zero there can be stale; the
  per-Present values (`PBSyCp`, `PBRet`) and the `PrDdi*` block are live.
* `FcImp` / `FcBlt` count only the FIRST import of a source image (cached afterwards): small numbers are normal.
* `BltEntryOk = BltAsyncN` proves the foreign copy ran; `BltNoEntryM` equal to `BltEntryDec` means every Blt carried a WindowedBlt
  snapshot (the older two-phase path), not that `BltAsync` failed; `BltNoEntryFc` rising means `ForeignCopy` is 0.
* `GbWhy` 4 is `Uncovered`: the first Blt before VidMm's one eviction (no system leases yet). One per run is normal. 29 is `SystemStale`,
  30 is `CreateTimeout` (24.12).
* `StVnu` (a fault counter) is not zeroed per start: a nonzero value can be from an old boot; `RmSys*` values are not zeroed either.
* `HpdPassMaxUs` is microseconds; `VpPend` garbage is the u32 of a handle; `ScRest*` zeros after a restart-device mean the image was
  reloaded (`StartN` 1), not that nothing was seeded.
* Every knob is read at StartDevice: a reading taken without a restart-device after changing it measures the old value.
* The `PrDdi*` block: `PrDdiBlt*` is the whole exported `DxgkDdiPresent` wall time for Blt-arm presents (flags bit 2 clear), with a histogram;
  `PrDdiFlip*` is the flip arm.

## 4. `GuestBlob` (24.12): known gaps, and what a sign-off needs

Reviewed adversarially and fixed (v343): stale-mark invariant, StopDevice retires live guest blobs, bounded waits (250 ms per phase, about
2 s per retire, about 3 s per create), one target decision, deferred copies re-prepared when their blob was retired. Open: (a) a skipped
eviction marking a destination while its guest blob is live can lose frames until the next Blt (not expected, VidMm does not evict an
allocation already in system memory); (b) the deadlines are untested against a host that really stalls; (c) a device reset is assumed to
drop the host's guest-blob mappings (the pins are dropped only after the acknowledged transport reset); (d) the reply layout of
`vkGetMemoryResourcePropertiesMESA` was written from the protocol headers; (e) PFN reads from the lease MDLs are checked only by reading;
(f) `retire` holds the content mutex and the Venus mutex: the order content, then Venus, then virtio has no cycle in the code read, but
anything that ever holds the Venus mutex and waits for the content mutex would deadlock; (g) the host requires every entry of one blob to
come from the same backing file (`EXDEV` otherwise: its own `GbWhy`, legacy fallback, strike).

Sign-off before defaults change: a 10 minute drag, resize, minimise, open and close soak with `GbLeak` 0, `GbStrike` 0, `GbFail` flat,
`PgSe` and `PgInvOvf` 0, `GbDrainMax` far below 250 ms; the host without `--venus-guest-blobs` (`GbFeat` 0, behaves as `GuestBlob` 0);
restart-device with a live guest blob (no hang, `GbLive` back to 0).

## 5. MSI-X plan (written down, not built on this line)

Branch `kmd/msix-default` (178661b, docs on top of `kmd/msi` 8cb1073 "use MSI-X when the OS grants messages, INTx otherwise"), design
`msi-interrupts.md`: INTx stays the default in the first package, `MsiMode` opt-in, a breaker that falls back to INTx when interrupts stop,
a polling-only mode, a minimum safe test procedure and a recovery procedure with and without a channel into the guest, and the INF flip
plan (the INF must request message-signalled interrupts; a wrong INF can leave a device without interrupts, so test it behind the opt-in
with a VM snapshot). Why: every completion (every RM call at about 55 us, every Blt copy, every flip) pays INTx's shared-line status read
and an end-of-interrupt exit; MSI-X should cut tens of microseconds per completion. Prerequisite from the host: the vhost-user virtio-gpu
device must expose MSI-X (unanswered when this was written). Steps: bring the branch current on top of v343 (the lifecycle and StartDevice
files moved a lot), build opt-in, A/B `BltAsyncLat` and the RM call latency, keep INTx the default until a soak.

## 6. Restart-device degradation (deprioritised by the user; state of knowledge)

Symptoms seen on 341.x after a live `pnputil /restart-device` (including the one a driver install does): the mode reports 5120x1440@240
but `VpPres` is about +6 per 4 s (clean boot: +2200), a Venus spin drops from 6685 to 1897 fps, `FfProg` 0, `FlipIss` +6 per 5 s; after
about ten restarts the adapter lost its mode ("x" in `Win32_VideoController`, `VpPres` 0, `VpVsN` 25: the heartbeat ran 25 ticks) and only
a VM reboot recovered it. A surviving DWM keeps the Venus holder context id it created before the reload: every `IMPORT_RM` is refused
`BAD_CONTEXT` (`FgRefC` 53) and its swap-chain buffers become KMD placeholders (no resource id); it also crashed (`dwmcore` 0x8898008d).
Understood: a per-process UMD context that does not follow the generation change (the fix is UMD-side: recreate the holder context on
`BAD_CONTEXT` or on an epoch change). Not understood: why heartbeat and flip retire are slow after the reload. Suspects from the code, in
order: the heartbeat after restart (zero-copy-present.md sections 19 and 20), v338's StopDevice scanout unbind with the salted identity,
completions picked up by polling instead of the interrupt. To diagnose, capture the full service key twice 10 s apart in the degraded
state and diff it against a clean boot of the same version; read `VpVsEn`, `VpVsN`, `VsTickN`, `VsSnapA/B`, `VsWd*`, `HpdSite`,
`FlipIss`, `StopUnb*`, `ScRest*`, `RestSeed`, `StopSub`; the System log around the restart; and run the Venus spin without DWM to
separate "transport slow" from "dxgkrnl throttling presents". Do not test anything else in a boot that has had restart-device cycles.

## 7. Open design questions

1. Can the host take a dependency on the producer's RM fence inside a Venus command (copy at Present time, no worker hop)? Largest single
   lever on the 4.4 ms frame. Needs a host wire change and the KMD's direct route to accept a not-yet-ready boundary.
2. The ring-1 round trip: where do 0.3-0.8 ms go (backend pickup, `vkQueueSubmit`, fence thread, interrupt injection)?
3. Why does VidMm move this allocation to system pages once and keep it there (`PgTo` 1, `PgTi` 0)? If it could stay in the BAR segment
   the CPU view would be the Venus blob and no mirror would exist at all.
4. dxdiag shows "Current Mode: Unknown" and no monitor section although a "Generic Monitor (Conduit)" devnode (`DISPLAY\CDT0001`) is OK
   and DXGI enumerates the output: a dxdiag quirk or an incomplete GDI association? No functional symptom found.
5. DXGI reports cross-adapter resource support tier NONE for this adapter (the KMD sets `CrossAdapterResource` only with its knob): check
   `CrossAdaptCaps`.
6. Level 5 on hardware (`KmdRmClient=5`, `kmd-rm-client.md` 15.13) was never run; the one attempt (341.1) ran on a VM already degraded by
   restarts and proves nothing. It is on hold, and it is not needed for the windowed path.

## 8. Next KMD steps, ordered

1. Read the `GuestBlob` stress, fallback and restart sign-off results (section 4); make `GuestBlob`, `BltAsync`, `ForeignCopy` defaults
   only after.
2. Heaven ETW attribution (per frame, the sum of events 42 minus 41 against the previous Blt packet's 178 to 180 span), then whichever of
   section 1's levers the numbers support; add a counter for the Blt DMA-fence completion delay (delivery latency) if it matters.
3. MSI-X opt-in (section 5).
4. The restart-device degradation (section 6) once the user lifts the deprioritisation.
5. Follow-ups never done: `RmGateMs` only fires when the worker wakes (add a due time while gates are open); a killed `RELEASE_BLOB`
   escape leaks its window range and ledger slot until reset; an unreachable `panic!` in `adapter/locks.rs`; raise the changed-only cache
   slots (2048) to 8192; the tick-cell fallback on overlapping callbacks; the global `mirror_thread::leaked()` latch; an `NvDupHarden`
   default of 1 after the `NvDupWould` evidence; host-vblank pacing (`host-vblank-pacing.md`) only after measurement; a live publish of
   `PgSm` under a distinct name (not a second `PgSm`).

## 9. Branches on origin that are not on main, and the tooling

`kmd/msix-default` (section 5), `kmd/independent-flip-design` (`independent-flip.md`: the S-0a probe and the implementation plan; S-0a found
no row that changed anything), `kmd/std-census` (the S-A0 census, already part of v340 and kept as a branch), `kmd/dwm-restart-repaint`
(3eb93fd: DWM restart breadcrumbs, and an importer of the adapter-owned scanout target no longer unbinds the host scanout when destroyed),
`kmd/host-vblank-design`. The KMD cannot be compiled without the WDK (only the Windows VM builds it), so development used `tools/kmd-dev/`
(README there): `prepush.sh` (kmd_logic tests with the counter-name scans enforced, protocol tests, a parse of every source, an inline-only
WDK extern scan, the C/Rust NVRM ABI mirror check) and `stubcheck.sh` (a stub-crate type check of `kmd_render`, always diffed against a base
commit).

Working rules that cost time: run the gate after EVERY merge and read its verdict lines (the sub-scripts end in pipes); worktree-isolated
agents may start on an older base than the one named in the prompt, so every agent prompt must say `git switch -c <branch> <commit>` and
make the agent verify the log; registry knob and counter names are truncated at 14 characters (`KnobName::new` asserts); registry writes are
PASSIVE only; DIRQL and DISPATCH code uses atomics only; `KeGetCurrentThread`, `KeGetCurrentProcessorNumber`, `KeMemoryBarrier` and the
`Interlocked*` family are inline-only in the WDK and fail at link time (use `PsGetCurrentThread` and Rust atomics); no AI attribution
anywhere (commit messages, docs, code comments).

# Host-feedback display pacing (`HostVblank`)

Status: DESIGN ONLY. Nothing here is wired into the driver, built for the WDK or run. The pure half is
`kmd_logic/src/host_vblank.rs` (host-tested, `cargo test host_vblank`, see "Verified here"); the host half is a list of requirements
for the session that owns `host/` (section 6). Branch `kmd/host-vblank-design`, written against v325 (`0221f5f`).

The goal: let the guest's vblank (`DXGK_INTERRUPT_CRTC_VSYNC`) follow when the host viewer REALLY presented a frame,
so DWM and applications pace to the true scanout instead of to a free-running timer that beats against it.
Opt-in, default off, with a seamless fall back to today's timer whenever the host says nothing.

Reading order for a reviewer: section 1 (the problem), section 3 (alternatives and the recommendation), section 4
(the design), section 6 (what the host must do). Everything else is detail.

## 1. The problem, and what the guest does today

`service_vsync_tick` (`kmd_render/src/adapter/kobj.rs`) is the only producer of `CRTC_VSYNC`. A one-shot timer
(`ExAllocateTimer` high resolution, or the embedded KTIMER/KDPC fallback) fires every
`vsync_deadline::period_100ns(rate)` units, where `rate` is the committed target mode's refresh
(`AdapterContext::effective_refresh_mhz`) unless the `VsyncRateMhz` knob forces one. Its rules
(`kmd_logic::vsync_deadline`, documented in `lib.rs`) are the invariants this design keeps:

1. fixed phase: the next deadline advances from the PREVIOUS DEADLINE, not from `now`, so DPC latency is never
   cumulative drift;
2. strictly future: a deadline is always after `now`, and `relative_due` never produces 0 (absolute-time semantics);
3. no catch-up burst: a tick that ran several periods late skips to the first future lattice point (one deadline);
4. terminal exhaustion: an unrepresentable deadline is `None`, the timer is left unarmed, never rearmed in a storm;
5. the tick runs whether or not dxgkrnl's delivery gate (`DxgkDdiControlInterrupt`) is open, so enabling vsync needs
   no timer operation and resumes on the next nominal retrace.

The tick reports `last_primary_address` in the packet (`ddi/submit_command.rs` `signal_crtc_vsync`: `VidPnTargetId` =
`CHILD_UID`, `PhysicalAddress` = the address, every other field zero). dxgkrnl retires a queued flip when a CRTC_VSYNC
carries the flip's new address (`docs/zero-copy-present.md` 13.1/13.2: the model is the driver's own and has not been
observed to be strict, 13.4 item 1). `DxgkDdiGetScanLine` answers a constant ("in vertical blank", line 0,
`ddi/display.rs`). `FfAsyncWin` (docs/kmd-rm-client.md 15.18.13) adds an early wake of the HPD worker from the MMIO flip
DDI, so the host flip no longer waits for the tick; the vsync tick is still what completes the flip toward dxgkrnl.

None of this knows when the host displays anything. A flip leaves the guest at an instant set by DWM, reaches the
viewer a few hundred microseconds later, and is shown at the compositor's NEXT vblank. The guest tick and the host
vblank are two free-running clocks of the same nominal rate:

* a rate mismatch (the display is really 239.76 Hz, or just 50 ppm off the guest's TSC-derived clock) makes the phase
  between them rotate once per beat period. Each rotation is a duplicated or dropped frame, and the flip-to-photon
  latency swings through a whole period (4.2 ms at 240 Hz, 16.7 ms at 60 Hz) in a sawtooth;
* with no mismatch the phase is a constant chosen by chance at boot and by every re-arm, possibly the worst one (a
  flip that always misses the compositor's deadline by a hair).

The estimate of the damage is in section 3.2. It is an estimate: nothing in this repository has measured the beat
(section 8, step 0 is the measurement that decides whether any of this is worth building).

## 2. What exists on the host (read-only investigation)

Files at this branch's base unless a branch is named. The release event lives on `feat/host-s6-flip-release`
(`8d4c1e8`), which is NOT an ancestor of this base: `host/backend` here has no `ScanoutReleased`, no
`NVGPU_F_SCANOUT_RELEASE`; the KMD (`protocol/src/features.rs`, `kmd_logic::scanout_release`) already speaks it.

### 2.1 The path of a flip

```
KMD ScanoutFlip (msg 20, control queue, 64 bytes, seq minted by the KMD)
  -> backend DisplayLink::flip (host/backend/device/src/display.rs:1515): export the dma-buf once (cache), then
     send_frame_locked (:1588): ONE non-blocking sendmsg of two 40-byte records, ATTACH (fd, fourcc, modifier,
     stride, seq = backend CLOCK_MONOTONIC microseconds as u32 when CLIENT_SEQ_USEC) and COMMIT
  -> viewer (host/viewer/nvkvm_broker.c + nb_session_wl.c / nb_session_x11.c): wl_commit
  -> compositor presents at ITS next vblank
```

The Venus path joins the same road: `SET_SCANOUT_BLOB` only records which blob is scanout 0 and its layout
(`host/backend/device/src/venus/scanout.rs:107`; on the host nothing is shown by it), and each `RESOURCE_FLUSH` exports
the blob once per resource and layout and calls `DisplayLink::flip_dmabuf` (`scanout.rs:224`, `display.rs:1561`), which
ends in the same `send_frame_locked`. A frame is therefore one ATTACH/COMMIT with one stamp whichever way it came; only a
`ScanoutFlip` has a guest `seq` (a Venus flush has none: 0). The boot console's frames (`flip_console`) are the same
records with the `F_SHM` flag and no guest flip.

The reply to the guest's `ScanoutFlip` is a bare header sent as soon as the flip is handed to the socket
(or dropped: a dead or full client costs that client the frame). It says nothing about presentation, so the flip's
round trip carries no vblank information (the `FfRttUs*` counters measure queueing, not display).
"Latest frame wins" at every stage: the compositor may replace a commit before it reaches the screen
(`wp_presentation_feedback.discarded`, counted by the viewer as `st_discarded`, shown as "drops").

### 2.2 What the viewer receives from the display server

Wayland backend (`nb_session_wl.c`):

| signal | where | content | used for today |
|---|---|---|---|
| `wl_callback.done` of `wl_surface_frame` | `frame_done` (:1099), requested per commit (:1461) | `t`: milliseconds, compositor-defined clock (ignored) | the pacing token `EV_FRAME` to the backend (see 2.3) |
| `wp_presentation_feedback.presented` | `pres_presented` (:4095), requested per commit (:1439-1450) | `tv_sec_hi/lo`, `tv_nsec` (the clock named by `wp_presentation.clock_id`, `pres_clock`, usually `CLOCK_MONOTONIC`), `refresh` (nominal ns, 0 unknown), `seq_hi/lo` (the vblank counter, "msc"; 0 when unknown), `flags` (VSYNC 1, HW_CLOCK 2, HW_COMPLETION 4, ZERO_COPY 8) | `--stats` (commit to present), the DIRECT/COMPOSITED label, and the host refresh learned in `pres_presented` (:4157, the only source of `refresh_mhz`). The timestamp, `seq` (msc) and VSYNC/HW flags are NOT used |
| `wp_presentation_feedback.discarded` | `pres_discarded` (:4078) | nothing | counted |
| `wp_tearing_control` | `wl_mode_hint`, direct mode | the ASYNC hint | CTRL+ALT+D "direct mode" |

X11 backend (`nb_session_x11.c`): `PresentPixmap` with `target_msc` 0; `PresentCompleteNotify` carries `ust`
(microseconds) and `msc` and a `mode` (FLIP/COPY/SKIP); today only `mode` is read (:851) and it paces `EV_FRAME`. No
refresh is learned there ("an X11 session has none", `nb_common.c`).

DRM page-flip events: the viewer is a Wayland or X11 CLIENT; it never holds the DRM master, so it sees none. The
compositor receives them (`drmEventContext.page_flip_handler2(fd, sequence, tv_sec, tv_usec, crtc_id)`) and that is
what `presented` reports. A DRM-direct viewer is not part of this design.

What feedback exists only while frames flow: a `presented` event exists per COMMIT. A guest that stops flipping (a
static desktop; DWM flips about twice a second then) produces no commit and no feedback. Neither signal can be had
for an idle surface from Wayland; the frame-callback chain stops with the commits (the viewer even carries a 100 ms
watchdog, `NB_WL_PACE_MS`, that sends a synthetic `EV_FRAME` because the real one only follows a commit). So host
feedback is available exactly while it matters (frames are flowing) and absent exactly when pacing does not matter
(nothing to pace); a design must treat "no feedback" as normal, not as a fault (sections 4.5, 4.10).

### 2.3 What the viewer sends the backend today (`nvkvm_broker_proto.h`, version 2)

| packet | content | what the backend does |
|---|---|---|
| `EV_FRAME` (3) | nothing (x, y, w0, w1 = 0); "the display is ready for another frame"; the broker COALESCES consecutive ones in its tx ring (`nvkvm_broker.c:275-285`) | `note_packet` (`display.rs:2628`) ignores it (falls into `_ => {}`) |
| `EV_RELEASE` (4) | the buffer id (dma-buf inode); on `feat/host-s6-flip-release` with `CAP_RELEASE_SEQ` (HELLO w1 bit 15) x = the flip stamp | base: ignored; release branch: `client_released`, the release book |
| `EV_SURFACE` (2) | window size, w0 = host refresh in mHz (a refresh change alone is sent, `nb_sink_surface`) | debug log only |
| `EV_MODE_HINT` (17) | mode, w0 = refresh mHz | `ModeArbiter`; becomes `DisplayMode` (msg 23) |

HELLO capability bits 0..14 are used (15 on the release branch). `EV_` numbers 1..19 are used; `CLIENT_` bits (backend
to broker, in `CMD_CAPS`) 0..4. The `seq` of ATTACH/COMMIT is the backend's microsecond stamp of the flip
(`CLIENT_SEQ_USEC`); the viewer widens it to nanoseconds (`nb_flip_ns`) and already computes flip to commit to
present on the one `CLOCK_MONOTONIC` it shares with the backend. The viewer never reports a presentation time to
anyone but its own `--stats`.

### 2.4 What reaches the guest from the host today

The event queue (virtqueue 1, host to guest; the KMD posts 16 buffers of 256 bytes, `nvrm_events.rs:86-91`; one
message consumes one posted buffer; the device interrupts per used buffer). Messages: `EventReady` 8 (a descriptor has
something to report), `InputEvent` 22 and `DisplayMode` 23 (the Windows KMD acks neither: it never acks
`NVGPU_F_TAKES_INPUT` and drops other messages after reading the 16-byte header), `ClipboardFromHost` 25,
`ScanoutReleased` 28 (feature bit 15, release branch). The backend delivers by `add_used` + `signal_used_queue` into a
buffer the guest posted; with none posted the message waits (releases: retried every 2 ms; others dropped or
level-triggered). So the transport for a new host-to-guest message exists, is interrupt driven, and has a buffer
budget shared with fence wakes and releases. `ScanoutReleased` already arrives once per frame; per presentation is a
precedent, with its cost (`docs/foreign-scanout.md` "Buffer release": the KMD does not even kick the queue for it).

### 2.5 What a vblank event could carry, and what it would cost

Per presentation the viewer knows: the presentation instant (host clock), the vblank counter (msc, maybe 0), the flags
(VSYNC, HW clock, HW completion, zero copy), the nominal refresh, and, from its own stamp, WHICH flip it was (the
`seq` stamp of ATTACH). The backend knows which guest flip that stamp is: it minted the stamp when it sent the flip (a
ring of the last few `(stamp, guest seq)` pairs is enough; the release book of the release branch already holds the
stamp of each client's last ATTACH of a buffer beside that buffer's flip `seq`). That is everything section 4.1 puts on the wire, and nothing the host has to invent.

| quantity | at 240 Hz | at 60 Hz |
|---|---|---|
| presentations/s while the guest flips every frame | up to 240 | up to 60 |
| `HostVblank` events/s after coalescing (one per `max(4 periods, 16 ms)`, up to 4 samples each; 4.1) | about 60 | 15 |
| acquisition (first 16 presentations after a gap or a `FIRST`): one event each | 16 in 67 ms | 16 in 267 ms |
| guest cost per event | one MSI + one DPC drain of one buffer, order of 10 us | same |
| `ScanoutReleased` already on the same queue | up to 240/s | up to 60/s |
| idle (no flips) | 0 | 0 |

The guest side is therefore a rounding error against the DPC load it already has, and the host side is one
`add_used` per event. The expensive-looking alternative, one event per presentation, would be 240 interrupts/s on top
of 240 releases: legal, wasteful, and not needed, because a phase lock needs one good phase sample every few frames
and the intra-event samples carry the frequency exactly (4.3).

### 2.6 Other display clients

`conduit-stream` (the encoder host) is a second client of the same backend (`CAP_IDLE`, `EV_ACTIVE`). It has no
scanout: its "presentation" is encode pacing. It must not send presentation events, and the backend must take them from
one client only: the one the mode arbiter follows for the refresh rate, among the clients that declared the new
capability (the local viewer). With a stream session active and no viewer, the guest keeps the timer.

## 3. Alternatives, expected gain, and the recommendation

### 3.1 The options

| | what | host change | guest change | what it fixes | what it does not |
|---|---|---|---|---|---|
| A | `HostVblank` events from the viewer's presentation feedback, a PLL on them (this document) | viewer, backend, wire message | KMD: parse, PLL, tick | frequency and phase, to the true scanout | idle desktop (no feedback: holdover, then timer) |
| B | a PLL on the arrival times of the EXISTING `ScanoutReleased` events | none (release branch needed for `ForeignFlip` flips) | KMD: sample source | frequency, and a phase that is a compositor-dependent function of vblank | jitter 10 to 100 times worse; wrong phase in COMPOSITED mode; needs the release feature acked |
| C | the backend timestamps `EV_FRAME` arrivals and forwards them (viewer unchanged) | backend, wire message | as A | frequency | the frame callback is not a vblank: its phase relative to scanout is compositor policy |
| D | frequency only: the host reports a measured refresh once a second, the guest trims its period (phase free) | viewer (average the presented intervals), small message | KMD: period trim | the beat, hence the periodic dup/drop | the latency sawtooth is replaced by one arbitrary constant phase (maybe the bad one) |
| E | the viewer presents at the guest's rate (tearing: `wp_tearing_control` ASYNC, "direct mode", CTRL+ALT+D, exists) | none | none | the beat, by dropping vblank alignment | tearing; needs a compositor that allows it |
| F | tell the guest the measured refresh through a mode change | viewer | KMD ignores `DisplayMode` today; a mode set per correction | n/a | a mode set (flicker, DWM re-init) cannot be done for 0.1% |
| G | nothing | none | none | | the beat |

A is the only one that fixes both frequency and phase from a trustworthy signal. B and D are strict subsets of its
guest half, and the pure module is written so that B is "A with `age = 0` and a looser jitter budget": the guest half is
shared, which is why the recommendation below stages it.

A cheap experiment needs NO new code: the `VsyncRateMhz` knob already forces the tick rate. Setting it to the
host's measured true refresh (for example 239.760 where the mode says 240) is option D done by hand. If the beat
(the viewer's "drops" per second, section 8) disappears with it, the frequency half is proven; whatever remains is the
phase half (A's added value).

### 3.2 Expected gain (estimates, not measurements)

Slips per second without a lock = tick rate times the relative rate error. The error is the sum of the display
crystal's deviation from its nominal rate and the guest clock's from the host's; 100 ppm is ordinary and 1000 ppm
is what 59.94-versus-60 style modes give.

| mode | relative error | frame slips (one dup or drop) | flip-to-photon latency swing without a lock |
|---|---|---|---|
| 60 Hz | 1000 ppm | 0.06/s (every 17 s) | 16.7 ms peak to peak |
| 144 Hz | 100 ppm | 0.014/s (every 70 s) | 6.9 ms |
| 240 Hz | 100 ppm | 0.024/s (every 42 s); 0.24/s at 1000 ppm | 4.2 ms |
| any, locked | host clock drift only | 0 while locked | below 0.3 ms (filter jitter, section 4.4) |

Whether a human sees one slip every 17 to 70 seconds is a matter of taste (competitive games: yes; a desktop: barely);
that the latency is a sawtooth rather than a constant is what makes it feel like judder at 60 Hz. At 240 Hz the
absolute swing is small. The honest summary: a clear win at 60 to 144 Hz and for frame-time-sensitive content, a small
one at 240 Hz.

### 3.3 Recommendation

Do not build it yet; measure first, then stage.

1. Run section 8 step 0 on hardware (no code): the viewer's drops per second and the presented-interval spread under
   steady animation at 60, 144 and 240 Hz, and the `VsyncRateMhz` experiment. If slips are below one per minute and the
   swing is not noticeable, stop: the answer is "not worth it" and the `HostVblank` message stays a design.
2. If they are not: build the guest half first against the existing `ScanoutReleased` events (option B, KMD only, no
   host change), because it exercises everything that is risky and uncertain on the guest
   side (tick integration, slewing under DWM, dxgkrnl's reaction, the tearing and PresentMon metrics) without waiting
   for the host session. It is most of the guest work and none of the host work; the host half (A) is small by
   comparison (one broker packet, one aggregation rule, one event).
3. Then A: the host half (section 6) replaces the sample source with the viewer's real presentation instants. Option
   E (direct mode) is the standing workaround for testers who want no beat today.

Priority against the rest of the plan (working rendering and bare-metal performance first; `FfAsyncWin` still needs its
hardware checklist): this is a smoothness feature, not a throughput feature, and its value is unproven. It does not
block anything and nothing blocks it.

## 4. Design

### 4.1 The wire message (event queue, host to guest)

`MsgType::HostVblank` (proposed value 32; 29 is skipped in the host's enum with no recorded owner, so the next value
after `RmResourceImport` 31 is taken; renumber on merge, the guest constant is `kmd_logic::host_vblank::MSG_HOST_VBLANK`).
Virtqueue 1. `MsgHeader` (16 bytes: type, handle 0, status 0, reserved), then a 48-byte body, then `count` samples:

| offset (body) | size | field | meaning |
|---|---|---|---|
| 0 | u16 | `version` | 1. Another version is dropped and counted (`HvBad`) |
| 2 | u16 | `count` | samples, 1 to 8; 0 only with `IDLE` |
| 4 | u32 | `scanout` | 0 |
| 8 | u32 | `flags` | below |
| 12 | u32 | `refresh_mhz` | the output's nominal refresh in mHz from the compositor, 0 unknown. A hint; the guest measures |
| 16 | u64 | `sent_ns` | host clock (nanoseconds) when the event was built |
| 24 | u64 | `seq` | `ScanoutFlip.seq` of the flip the NEWEST sample presented, 0 if none or unknown (a Venus resource, the console) |
| 32 | u32 | `lost` | presentations since the previous event that are in no sample (coalesced beyond 8, or dropped for want of a buffer); a diagnostic |
| 36 | u32 | reserved | 0 |
| 40 | u64 | reserved | 0 |
| 48 + 16 i | u64 | `present_ns` | the host-clock instant frame i reached the screen (same clock as `sent_ns`; any monotonic origin) |
| 56 + 16 i | u64 | `msc` | the compositor's vblank counter for it, 0 unknown |

Maximum 16 + 48 + 8 * 16 = 192 bytes, inside one 256-byte posted buffer (the existing `EVENT_BUF_BYTES`).

`flags` (the low four bits are `wp_presentation_feedback.kind` verbatim, so the viewer passes them through):
`VSYNC` 1 (every sample was vblank-synchronised; without it the samples say nothing about the retrace and are not
used), `HW_CLOCK` 2, `HW_COMPLETION` 4, `ZERO_COPY` 8 (informational), `FIRST` 16 (first event after feedback
restarted: an output or mode change, a viewer returning from idle, a gap over 100 ms; the guest restarts its filter),
`IDLE` 32 (the host stops sending until further notice, `count` 0: the guest drops to the timer at once),
`COALESCED` 64 (`lost` is nonzero). Unknown bits are ignored, never refused.

Why `sent_ns` and `present_ns` and not a host-to-guest clock offset: see 4.3.

Negotiation and acks (the same shape as `NVGPU_F_SCANOUT_RELEASE`, `docs/foreign-scanout.md` "Buffer release"):

* feature bit: `NVGPU_F_HOST_VBLANK` = virtio feature bit 16 (`kmd_logic::host_vblank::FEATURE`). Allocation check:
  `protocol/src/features.rs` uses 12 (`TAKES_INPUT`, never acked by this driver) and 15 (`SCANOUT_RELEASE`) of the
  Conduit bits, plus the standard virtio-gpu bits 0..4 and transport bits 28, 29, 32, 40; the host's config
  `features` word is a different space (its bits 8 to 11, 13, 14 are used, 12 and 15 skipped on purpose). No local branch
  that defines feature bits (`feat/host-s6-flip-release`, `feat/s6-backend`, `feat/s6-shared-surfaces`,
  `feat/rm-window-size`, `feat/host-rm-import`, `feat/input-feature-bit`, `feat/nvk-wsi-scanout-release`, this one;
  `protocol/src/features.rs` and `host/backend/protocol/src/messages.rs` in each) defines bit 16 or above, and the
  Linux driver on `main` has none either. A test in `host_vblank.rs` reads `features.rs` and fails if a constant takes bit 16.
* the host offers it whenever it has a display (as for `SCANOUT_RELEASE`: features are fixed before any viewer
  connects), and SENDS only while a client that declared the capability exists (2.6); the guest acks it iff offered,
  the display half is on, and the knob `HostVblank` is nonzero (and `VsyncRateMhz` is 0: the debug override wins).
  Without the ack the host sends nothing and keeps no state. `negotiate_features` already retries with the
  required set when a device refuses the larger one (`RelNeg`), so an optional bit never costs the transport.
* the only per-message ack is the posted buffer: the guest reposts every buffer it takes (as `take_event` does). There
  is no per-event reply and none is wanted.
* host rules: never send without the ack; send only into a posted buffer and DROP (never retry: it is stale in a few
  milliseconds) when none is posted, counting it into `lost`; never take the last 4 posted buffers for it (a fence wake,
  `EventReady`, and a release matter more); order of priority `EventReady` > `ScanoutReleased` > `HostVblank`.
* version: `version` 1; a layout change appends to the reserved fields and keeps the version, an incompatible one bumps
  it and the guest drops what it does not know.

When the host sends (section 6 has the full list): the first event after a gap immediately with `FIRST`; one per
presentation for the first 16 (acquisition); then at most one per `max(4 periods, 16 ms)`, carrying every presentation
since in `samples` (up to 8, `lost` and `COALESCED` if more); `IDLE` when it knows it will stop (viewer minimised or
disconnected, console mode); silence otherwise. The silence interval is below the guest's holdover threshold (8 periods,
30 to 150 ms) so two lost events do not drop the lock.

### 4.2 The guest consumption path

```
DPC (drain_used_and_complete, under the virtio lock)            vsync tick (DISPATCH, one-shot timer)
  drain_nvrm_events: Taken::HostVblank(Event)                     service_vsync_tick
  parse (kmd_logic::host_vblank::parse)                             drain the mailbox into Pll::observe
  for each sample: Event::sample(i, arrive)                         pll.update_mode / Pll::next_deadline(now, prev, last_fire)
  push {arrive, age, msc, flags} into the SPSC mailbox  --------->  set_vsync_one_shot(relative_due(deadline, now))
```

* `arrive` is `KeQueryInterruptTimePrecise` read in the DPC, the clock of the tick. Reading it in the ISR
  (`dxgkddi_interrupt_routine`, DIRQL) and handing it to the DPC removes DPC queueing from the jitter; that is a later
  refinement (it needs one atomic for the stamp, and the ISR does not read the queue today).
* the mailbox is a single-producer single-consumer ring of 16 entries (the DPC is serialised by the virtio lock; the tick
  by the one-shot timer re-arming itself). A full ring drops the NEW entry and counts `HvMboxDrop`: the filter wants
  recent phase and the older entries are already queued.
* ALL filter state is owned by the tick. The DPC touches no filter field: that keeps `Pll` free of locks, and a
  `Pll` update at DISPATCH on the tick CPU needs no interaction with the virtio lock.
* `FIRST` and `IDLE` travel through the mailbox as flagged entries so the tick applies `note_first` / `note_idle` in
  order with the samples.

### 4.3 Time base: no clock offset is estimated, and why that is right

The guest cannot read the host's clock and a round trip to learn the offset costs a control message and still leaves
the one-way delay split unknowable. It does not need it. The event carries `sent_ns` and each sample `present_ns` on
the SAME host clock, so `age = sent_ns - present_ns` is an offset-free host-side latency (compositor to viewer to
backend, hundreds of microseconds, known exactly). The guest places a presentation at

    obs = arrive - age        (guest interrupt time, 100 ns)

and what remains between `obs` and the true presentation is the host-send-to-guest-DPC delay `d`: positive, a constant
floor `d_min` (the interrupt path, tens of microseconds) plus jitter (DPC latency spikes). `d_min` is an unknowable
constant, and a constant phase bias is exactly what the `lead` knob (4.9) absorbs: `lead = 0` puts the tick at
`presentation + d_min`, one interrupt latency after the host's vblank, which is as good as a real display's
interrupt latency. Only the jitter matters, and the filter treats it as one-sided (4.4).

The samples within one event are separated by exact host-clock intervals (no transport jitter in them), and the
`msc` counter makes the number of vblanks between two samples exact where the compositor provides it. Both are used
for the period; the guest's TSC-derived clock against the host's `CLOCK_MONOTONIC` ratio (a few ppm to 50 ppm, slewed by
the host's NTP) is not modelled either: it is a frequency error the loop's integral term removes, because the loop
tracks the lattice in the GUEST clock.

### 4.4 The estimator (`host_vblank::Pll`)

A second-order phase-locked loop over the lattice `L(n) = anchor + n * period`, `period` in 100 ns units with 16
fractional bits (240 Hz is 41666.67 units; rounding it to 41667 is an 8 ppm error that a holdover of seconds would show).
At each accepted sample, with `n` = the number of lattice periods since the last sample (the compositor's `msc`
difference when it is consistent with the elapsed time, else the elapsed time rounded to the period), the phase
error is `e = obs - (anchor + n * period)`. Then:

* phase: `anchor' = predicted + e / 4` if `e < 0` (the sample came EARLIER than predicted: it can only be early by
  the delay's minimum, so it is trusted), `predicted + e / 16` if `e >= 0` (it may be a delay spike);
* frequency: `period' = period + (e / 64) / n` early, `+ (e / 256) / n` late;
* the period is clamped to the nominal one +- 3% (`tol_permille`); a clamped sample cannot count toward a lock and
  8 in a row drop `Locked` to `Holdover`;
* outliers: `|e| > period / 4` is rejected (it resets the good-sample run but does not unlock). Five in a row that
  agree with each other within `period / 16` are a PHASE STEP of the host (an output change, a compositor restart):
  the lattice moves to the new phase (`HvStep`) and the tick slews to it (4.6). Outliers that disagree are delay
  spikes and never move anything. Three samples in a row earlier than their predecessor by more than half a period (the
  guest's clock stepped back, which should not happen) reseed the filter and drop to `Timer`;
* `Duplicate` (the same `msc`, or two samples under half a period apart), `NotVsync` (a tearing present: no phase
  information), `Old` (a presentation in the event's future or over a second old) are dropped and counted.

The one-sided weighting makes the lattice settle on the EARLY edge of the delay distribution, not its mean, so a
burst of late interrupts does not drag the vsync. The tests measure it: with a 30 us floor, 150 us uniform jitter and a
spike of 1.5 ms on one sample in ten, the lattice stays within 150 us of the floor over 2000 samples (mean about 50 us
above it) and the period within 500 ppm of the host's; a host a tenth of a percent off the mode is followed to
within 100 ppm.

### 4.5 The mode state machine and its hysteresis

| mode | meaning | the timer follows |
|---|---|---|
| `Timer` | no usable host information (or not enough): today's behaviour | `vsync_deadline::next`, nominal period, previous deadline (bit for bit what ships) |
| `Locked` | phase and period follow the host | the lattice, slewed (4.6) |
| `Holdover` | the host went quiet: the lattice keeps running on the last phase and period | the lattice, slewed |

| transition | condition (defaults) |
|---|---|
| Timer to Locked | 6 consecutive good samples (`|e| <= period / 8`, period inside the band, no outlier between them) |
| Holdover to Locked | 2 consecutive good samples |
| Locked to Holdover | no accepted sample for 8 periods (clamped to 30..150 ms: 33 ms at 240 Hz, 133 ms at 60 Hz); or 8 samples in a row whose period had to be clamped; or a `FIRST` event |
| Holdover to Timer | no accepted sample for `HostVblHoldMs` (default 5 s, 0.1 to 60 s); or an `IDLE` event (from either mode, at once) |
| any to Timer | a reseed (clock stepped back, silence over the holdover, a span too long to count), a transport reset, a mode change (`retune`) |

Hysteresis is deliberate and asymmetric: acquisition is slow (6 samples), loss is slow (8 periods of silence), and
recovery from a short loss is fast (2), because the lattice is still valid in `Holdover`: with the guest clock within
20 ppm of the host's the phase drifts 0.1 period in 25 s at 240 Hz. A single outlier or two lost events never unlocks.
`Holdover` and `Timer` are indistinguishable to dxgkrnl; the difference is only whether the next sample relocks in 2
samples on a still-valid phase or reseeds.

### 4.6 The next deadline (`Pll::next_deadline`) and the invariants it keeps

`next_deadline(cfg, now, prev, last_fire)`: `prev` is the deadline that just fired (0 when arming, the same convention
as `service_vsync_tick`'s `anchor`), `last_fire` the interrupt time the previous tick actually ran at.

* `Timer`, or no lattice: exactly `vsync_deadline::next(prev or now, now, nominal)`. The default is therefore the
  shipping behaviour, and the unit test asserts equality.
* `Locked` / `Holdover`: the natural next point is `prev + period`; the lattice target is the first lattice point
  strictly after `now`, minus the lead. The deadline is the natural point moved TOWARD the target by at most `slew_max`
  (`period / 16`) per tick: every gap stays within `period +- period/16` plus the DPC latency, whatever the phase
  difference is, so acquiring or losing a lock is a gradual re-phasing (at most 8 ticks for the worst half-period
  difference), never a gap, a doubled tick or a burst. This is invariant 1 (fixed phase) in the new setting: the grid
  advances from the previous deadline, not from `now`.
* strictly future (invariant 2): if the natural point is not after `now` (the tick ran a whole period late) the deadline
  is the first lattice point after `now` and the missed ones are skipped (invariant 3): ONE deadline, no catch-up.
* never closer than half a period to the previous delivered tick (the `VsFast`/`VsMinGap` metric, `vsync_rate.rs`):
  a candidate nearer than that to `last_fire` moves one period later.
* `None` only where `vsync_deadline::next` would (zero period, exhausted arithmetic): the timer stays unarmed
  (invariant 4); overflow in the lattice arithmetic falls back to the Timer function, never to an immediate re-arm.
* all differences are wrapping `i64` distances of `u64` times (`diff`), so a timeline near `u64::MAX` orders
  correctly (a test crosses the wrap); the fixed-point products are bounded (`MAX_SPAN`, 2^46 units of 100 ns, about 80 days) so there is no
  overflow and no 128-bit arithmetic.
* the tick runs in every mode and with the delivery gate closed (invariant 5): the lattice is therefore kept warm while
  dxgkrnl is idle, and its first vsync after the gate opens is already on the host's phase.

### 4.7 Interaction with dxgkrnl, WDDM, and the rest of the KMD

* `CRTC_VSYNC` fields: unchanged. `VidPnTargetId` = `CHILD_UID`, `PhysicalAddress` = `last_primary_address`; every other
  field (`PhysicalAdapterMask`, any multi-plane-overlay source id) stays zero: the adapter reports no MPO
  (`SupportMultiPlaneOverlay` is not set, `query_adapter_info.rs`; MPO flips are refused, `shared-formats.md`).
* the flip completion model is untouched: the vsync that carries the new address retires the flip. The ticks move in
  time, not in meaning. With a lead the retire comes `lead` earlier relative to the host's present; whether the flip's
  PIXELS are on the host screen at that instant is what it always was (the address is published at programming,
  `publish_bound_primary`; the host shows them at its next vblank if the flip reached it in time).
* the kept-picture invariant (`flip_completion`, section 13 of zero-copy-present.md) is a property of what address a
  tick carries, not of when it ticks: nothing changes; a flip whose programming completes as a kept picture still
  completes at the next tick.
* `ForeignFlip` / `FfAsyncWin`: the host flip is sent by the HPD worker after the early wake, independent of the tick; the
  tick only completes the flip toward dxgkrnl and wakes the worker for a pending programming with the gate closed
  (`FfGateWake`). The presenter's pacing clock (`rm_refresh::flip_interval_100ns`, one flip per refresh period) keeps the
  NOMINAL period: it paces the host sends and must not follow the filter's estimate (otherwise a bad lock would change
  the send rate). `FfRttUs*` measure the flip's queueing; the new `seq` correlation (the event names the flip it
  presented) allows a submit-to-present latency measurement the KMD cannot make today (section 8).
* committed refresh / `VsyncRateMhz`: the nominal period is `vsync_rate_mhz(adapter)` as today. A mode set (committed
  refresh changed) calls `Pll::retune` (back to `Timer`, nominal period, unseeded); `VsyncRateMhz != 0` disables the
  feature (a forced rate is the debug override and wins). A host whose rate is not within 3% of the nominal one never
  locks (the period clamps): the viewer already tells the guest the host's refresh (`EV_SURFACE.w0`, `EV_MODE_HINT`),
  though the Windows KMD ignores `DisplayMode`, so a mismatch is possible and is simply not followed. A host at a
  DIVISOR of the rate (60 Hz host, 240 Hz mode) is followed benignly: its presentations are every fourth lattice
  point (test `a_host_at_a_divisor...`).
* D3 / StopDevice: `quiesce_vsync` / `stop_vsync` cancel the timer; the `Pll` is kept across a transient D3 (its
  staleness rules handle the gap) and `reset` at StartDevice and at a transport reset.
* `GetScanLine` / `WaitForVerticalBlank`: dxgkrnl's vertical blank waits are built on the CRTC_VSYNC interrupts, so they
  follow the new phase for free. `DxgkDdiGetScanLine` answers a constant "in vertical blank, line 0" today. While
  `Locked` it could report the virtual beam: `host_vblank::scan_position(now, vsync_last_100ns, period, vactive, vtotal)`
  (blank lines first, then the active lines, monotonic within a period, 0 exactly at the vsync; the mode's blanking is
  `edid.rs`'s CVT-RB2). Not wired and not recommended in the first version: no consumer is known to need it, and a
  constant is always a legal answer.
* the vsync's timestamps as dxgkrnl and DWM see them (`DwmGetCompositionTimingInfo` `qpcVBlank`, believed to be taken
  when the interrupt is reported, not verified) are the delivery times, with the DPC latency in them: the same as today.

### 4.8 The phase choice: the `lead` knob (`HostVblLeadUs`, default 0)

With `lead = 0` the guest vsync coincides with the host vblank (plus the interrupt latency): the guest sees the display
a real GPU would give it. DWM wakes at the vsync, composes (about a millisecond on NVK) and flips; the flip is shown at
the NEXT host vblank if it reached the compositor in time, so the flip-to-photon latency is a constant one period plus
the render time, as on real hardware. A positive lead moves every vsync earlier, so DWM starts earlier and the flip
arrives earlier relative to the host's deadline: lower latency, and a lower risk of missing the compositor's repaint
(mutter and kwin start compositing a few milliseconds before the vblank). Too much lead means DWM starts before
the previous frame is done. The best value depends on the compositor's repaint scheduling and DWM's frame time, so
it is a knob (clamped to below one period), measured per host in the checklist, with the default that models a display.

An adaptive lead needs a signal the first version does not have: whether a flip made the deadline. The `seq` field gives
it: the KMD knows when it submitted flip `seq` and the event names the vblank that showed it, so submit-to-present
beyond one period means "missed by a period". A controller on that is a later refinement (section 10, question 4).

### 4.9 Idle, minimised, occluded, several clients

* a static desktop: no flips, no feedback, no events. `Locked` becomes `Holdover` after 8 periods and `Timer` after the
  hold (5 s); the next burst of flips reseeds and relocks within 6 presentations (25 ms at 240 Hz) plus at most 8
  slewing ticks. During those the pacing is that of the timer: no worse than today.
* a minimised or occluded viewer: the compositor stops frame callbacks and presentations; the host sends `IDLE` if it
  can tell, else silence: the same path.
* the viewer or the broker gone, a boot console shown: `IDLE`, or silence; `FIRST` when it returns.
* two display clients: the source is the local viewer only (2.6); a stream session never sends events.

### 4.10 What this cannot do, said once

It cannot make the host present earlier than its compositor allows, change the compositor's repaint deadline, or fix
frames the guest renders late. It makes the guest's vsync a faithful copy of the host's, so that a guest that is
fast enough shows every frame once at the true cadence.

## 5. Counters, knobs and the registry

All Hv names are at most 14 characters, unique, start with `Hv`, and collide with no other list in `kmd_logic` or any
byte literal in `kmd_render` (`host_vblank.rs` test `counter_names_fit_are_unique_and_collide_with_nothing_else` scans
both trees; it exempts only a file named `host_vblank.rs`, which is where the writer goes). Written at PASSIVE only,
by the HPD worker's periodic dump and at StartDevice, from `AtomicU32` mirrors the DISPATCH code updates; the tick and
the DPC touch atomics only.

| name | what | expect |
|---|---|---|
| `HvKnob` | `HostVblank` in force, written on every read, 0 included | the knob |
| `HvAck` | the feature was acked (1) or not offered / not wanted (0) | 1 with the knob and a capable host |
| `HvNoQ` | acked but the event queue is not up: nothing can arrive | 0 |
| `HvLeadEff` / `HvHoldEff` | `HostVblLeadUs` (us) and `HostVblHoldMs` (ms) in force, clamped | the knobs |
| `HvMode` | 0 Timer, 1 Locked, 2 Holdover | 1 while frames flow |
| `HvRecv` / `HvBad` | events parsed / dropped (short, version, count, wrong type) | growing / 0 |
| `HvSamp` / `HvAcc` | samples offered / accepted | `HvAcc` close to `HvSamp` |
| `HvRejNoVs` / `HvRejDup` / `HvRejBack` / `HvRejOut` / `HvRejOld` | rejected: not vsync, duplicate, backwards, outlier, old | small; `HvRejNoVs` large means the host is tearing |
| `HvStep` / `HvReseed` / `HvMscBad` / `HvClamp` | phase steps / reseeds / counter inconsistent with time / period clamped | 0 or rare / 0 / 0 / 0 |
| `HvLock` / `HvHold` / `HvTimer` | entries into Locked / Holdover / Timer | one lock, then few |
| `HvTickLock` / `HvTickHold` / `HvTickTim` | ticks delivered in each mode | mostly `HvTickLock` under motion |
| `HvPerNs` | the filtered period in ns | 4166667 at 240 Hz (within 0.1%) |
| `HvErrUs` | the last phase error, absolute, in us | under 200 |
| `HvMboxDrop` | samples dropped because the mailbox was full | 0 |

Knobs (service key, REG_DWORD, read at StartDevice at PASSIVE, mirrored on every read, 0 included, as
`zero-copy-present.md` 13.8 requires; added to that table when wired): `HostVblank` (default 0), `HostVblLeadUs`
(default 0, below one period), `HostVblHoldMs` (default 5000, 100 to 60000).

## 6. What the host must implement (for the session that owns `host/`)

Everything below is additive, capability-gated, and invisible to a guest that does not ack the feature and to a viewer
that does not declare the capability. Numbers are proposals; the first free value of each space is used and the guest
constant is the only other place.

1. Broker protocol (`host/viewer/common/nvkvm_broker_proto.h`, `host/backend/device/src/display.rs` `wire`):
   * `NVKVM_BROKER_CAP_PRESENT_TIME` = HELLO w1 bit 16 (bits 0..15 are taken once the release branch lands): the broker
     sends `EV_PRESENTED`.
   * `NVKVM_BROKER_CLIENT_PRESENT_TIME` = `CMD_CAPS` width bit 5 (0..4 taken): the backend wants them. The broker sends
     none to a backend that did not say so.
   * `NVKVM_BROKER_EV_PRESENTED` = 20 (1..19 are taken), one 24-byte packet per presented frame: `x` = low 32 bits of the
     vblank counter (0 unknown; the backend extends it to 64 bits), `y` = the flip stamp (the ATTACH `seq`, microseconds
     u32; 0 if the frame was not a flip), `w0`/`w1` = the presentation time in nanoseconds on `CLOCK_MONOTONIC`
     (low and high 32 bits), `flags` = the presentation `kind` (VSYNC 1, HW_CLOCK 2, HW_COMPLETION 4, ZERO_COPY 8)
     in the packet's `flags` field, the rest zero. A presentation clock other than `CLOCK_MONOTONIC`
     (`pres_clock`, `nb_session_wl.c:4235`) is converted by reading both clocks at the event, else the event is not sent.
   * Like `EV_FRAME`, `EV_PRESENTED` may be coalesced in the tx ring ONLY by dropping the OLDER of two when the ring
     is full (a counter says how many), never by merging (the samples are timestamps).
2. Viewer hook (smallest change): in `pres_presented` (`nb_session_wl.c:4095`), after the existing statistics and before
   `free(pc)`, call `nb_sink_presented(w->sink, t_ns, msc, pc->seq, flags)` when the capability was negotiated and
   `flags & VSYNC`. The values are already in hand: `tv_sec_hi/lo`, `tv_nsec`, `sh/sl` (msc), `flags`, `pc->seq`. X11
   (`nb_session_x11.c`): the same from `PresentCompleteNotify` (`ust`, `msc`, `mode`). No new Wayland protocol, no
   new object, no extra commit; the cost is one 24-byte packet per presented frame, 240/s at most.
3. Backend (`display.rs`): handle `EV_PRESENTED` in `note_packet` (today `_ => {}`), from the client the mode arbiter
   follows among those that declared the capability. Keep the last 8 presentations and the time of the last event.
   Map the stamp to the guest's flip `seq` with a ring of the last 16 `(stamp, seq)` pairs recorded in `send_frame_locked`
   where the stamp is minted (independent of the release feature); 0 when it is not in the ring. Convert `x` to a 64-bit
   msc by tracking wraps.
4. Aggregation and rate limiting (in the backend, because it owns the event queue):
   * state: `acquiring` (the first 16 presentations after a gap over 100 ms or after the client reconnected), `steady`;
   * `acquiring`: build and send an event per presentation, the first with `FIRST`;
   * `steady`: send when `now - last_sent >= max(4 * refresh_period, 16 ms)` and at least one presentation is pending,
     carrying every pending presentation (the newest 8, `COALESCED` and `lost` for more);
   * nothing pending: nothing sent, no timer;
   * a viewer that says it will stop (`EV_ACTIVE` 0, disconnect, minimised if it can tell): one `IDLE` event, count 0;
   * the `VSYNC` flag of the event is the AND of its samples'; an event whose samples are all non-VSYNC is not sent;
   * `refresh_mhz`: the output's nominal refresh from the latest presentation's `refresh` field (the viewer's
     `EV_SURFACE.w0` already carries it), 0 unknown.
5. Delivery (`conduit-backend.rs`, beside `VqReleaseSink`): offer `NVGPU_F_HOST_VBLANK` (virtio feature bit 16) in
   `features()` whenever the backend has a display (like release); send only while a client with the capability exists; record the ack in `acked_features` (as
   `set_release_enabled`); write `HostVblank` into a posted buffer exactly as `VqReleaseSink::released` does (`add_used`,
   one `signal_used_queue` per batch); DROP when no buffer is posted or fewer than 4 remain; count drops into `lost`. No
   retry timer.
6. Message definition: `MsgType::HostVblank` and the struct of 4.1 in `host/backend/protocol/src/messages.rs` with
   `to_bytes`/`from_bytes` and a round-trip test; layout asserted against `kmd_logic::host_vblank` constants
   (header 16, fixed body 48, sample 16, max 8, `FLAGS_KNOWN`).
7. Tests the host should carry: the coalescing rule (never more than one event per interval in `steady`, one per
   presentation in `acquiring`, `FIRST` after a gap, `IDLE` once), the drop-when-no-buffer rule, the capability gating (no
   event to a backend that did not ask, none to a guest that did not ack), and that nothing is sent with no display
   client.
8. Documentation: `docs/SCANOUT.md` ("Presentation feedback") and the protocol table row.

What the host must NOT do: block a flip on any of this, send events to a guest that did not ack, retry or queue them,
invent a timestamp (no `presented`, no event), or send from a client that has no scanout (the stream host).

## 7. The guest wiring plan (not done; a later change)

Order, each step independently reviewable and all behind the knob (default 0 = nothing changes, not even a counter):

1. `protocol/src/features.rs`: `NVGPU_F_HOST_VBLANK = 1 << 16` and a const assertion with the other bits (the test in
   `host_vblank.rs` already guards the collision); NOT added to `CONDUIT_OPTIONAL_FEATURES` unconditionally: the
   optional set becomes a function of (display half, knob), as `scanout_release` is today.
2. `virtio/gpu/mod.rs` `VirtioGpu::init`: pass the knob with `scanout_release` into `negotiate_features`; record `HvAck`;
   `HvNoQ` when acked without the event ring (the pattern at `mod.rs:3034`).
3. `virtio/gpu/nvrm_events.rs`: a `Taken::HostVblank(Event)` arm in `take_event` (`MSG_HOST_VBLANK` before the
   `_ => Other` arm; `parse` returns the classification, `HvBad` for `Short`/`Version`/`BadCount`);
   `drain_nvrm_events` pushes the samples into the mailbox WITHOUT kicking the queue (like releases), reads
   `KeQueryInterruptTimePrecise` once per drain, and wakes nobody (the tick consumes).
4. new `virtio/host_vblank.rs`: the SPSC mailbox, the `Pll` (statics; owned by the tick), the `AtomicU32` mirrors, and
   `publish_counters(passive)`, the only writer of `Hv*` (the test exempts a file of this name).
5. `adapter/kobj.rs` `service_vsync_tick` and `arm_vsync`: replace `vsync_deadline::next(anchor, now, period)` with
   `pll.next_deadline(cfg, now, previous, last_fire)` where the knob is on; drain the mailbox first; `Config::new(period,
   lead)` rebuilt when `vsync_rate_mhz` changes (`retune`). The rest of the tick (gap statistics, `stall_diag::on_vsync_tick`,
   the delivery gate, `signal_crtc_vsync`, the HPD wake) is unchanged. `VsMinGap`/`VsFast` stay the oracle for "no burst".
6. `ddi/lifecycle.rs` (StartDevice): `HostVblank`, `HostVblLeadUs`, `HostVblHoldMs` read with `read_config_dword` (PASSIVE);
   mirrors written on every read; `Pll::reset` at StartDevice and at a transport reset (`retire_transport`).
7. `ddi/hpd.rs` periodic dump: `host_vblank::publish_counters`.
8. docs: the knob table of `zero-copy-present.md` 13.8; `foreign-scanout.md` "Reading vsync rates" (what `VpVsN` means under
   the lock).

Registry writes happen at PASSIVE only (the HPD worker and StartDevice); the DPC, the tick and the ISR touch atomics.
The filter is not lock-protected because it has one owner; the mailbox is the only cross-context structure.

## 8. Hardware checklist and expected metrics

Always the lowest mode first (1920x1080, 60 Hz), then 144 Hz, then 240 Hz and the larger modes
(`memory: test lowest mode first`). The guest is Windows 11 with DWM on NVK (`ForeignFlip` on) unless a step says
otherwise.

0. BASELINE, no code (decides everything): steady animation (a 60 fps clock or window dragging) for 120 s per mode.
   Collect: the viewer's `--stats` / overlay (drops per second = `st_discarded`, commit-to-present avg/p99, frame-time
   p99), PresentMon in the guest (`MsBetweenPresents`, `MsBetweenDisplayChange`, `MsUntilDisplayed`, dropped frames),
   `VsTickN`/`VpVsN`/`VsMinGap`/`VsFast` (the timer is healthy). Record the host output's measured refresh (the
   viewer's `host output refresh is ...` log line or the mean presented interval). Compute slips per minute and the
   p99 minus p1 of the commit-to-present time (the sawtooth's amplitude).
   Then repeat with `VsyncRateMhz` = the measured host refresh in mHz. If the drops and the sawtooth are gone, the
   problem is frequency only (option D, a smaller change); if the sawtooth remains (a constant phase), phase is the issue.
   DECISION: fewer than one slip per minute and no visible judder: stop here.
1. B, if built (KMD only, release events as the sample source): lock acquired (`HvMode` 1 within 1 s of motion),
   `HvRejOut` small, `HvErrUs` under 1000, no `VsFast`. Compare step 0's numbers.
2. A, with the host half: `HvAck` 1, `HvRecv` growing at about 15 (60 Hz) to 60 (240 Hz) per second under motion and 0 idle,
   `HvBad` 0, `HvMboxDrop` 0, `HvMode` 1, `HvLock` 1, `HvPerNs` within 0.1% of the measured host period, `HvErrUs`
   p99 under 200, `HvClamp`/`HvReseed`/`HvMscBad` 0.
3. Tick regularity (the invariants): `VsMinGap` at least half a period, `VsFast` 0, and the gaps of the tick (a trace)
   within period +- 1/16 plus the DPC latency, including through acquisition (start a drag after 10 s idle) and loss
   (minimise the viewer, restore it). No missing ticks: `VsTickN` per second at the mode's rate +- 0.1% over a minute.
4. The metric the feature is for, compared with step 0 at the same modes: drops per second near 0 (the sawtooth is
   gone: p99 minus p1 of commit-to-present under 0.3 ms while locked), PresentMon `MsBetweenDisplayChange` standard
   deviation lower, no tearing introduced (`HvRejNoVs` 0 unless direct mode is on), frame pacing in DWM-on-NVK
   unchanged or better (FPS counters unchanged).
5. The lead: `HostVblLeadUs` 0, then 500, 1000, 2000 at 60, then 144 and 240 Hz; for each, commit-to-present avg/p99
   and drops. Pick the smallest lead that holds drops at 0; record it per compositor (mutter, kwin, Hyprland).
6. Failure rows: stop the backend for 2 s (`HvTimer` rises after the hold, no stall, no burst: `VsFast` 0), resume (relock
   within 6 presentations); drag the viewer to another output with another refresh (`HvStep` or `HvClamp`, never a
   lock on the wrong rate); set the mode to a different refresh in Windows (`retune`, unlocks, relocks);
   `HostVblank` 0 and a restart: `HvKnob` 0, `HvAck` 0, no `Hv*` event, behaviour identical to v325.
7. dxgkrnl's reaction: DWM's own frame statistics (`DwmGetCompositionTimingInfo` refresh period and vblank counts) with
   and without the feature, and the flip completion counters (`FlipPub`, `FkKeep`, `SaCnt`) moving together as before.

Expected: while locked, vsync-to-host-present phase spread below 0.3 ms (peak to peak), slips 0, and flip-to-photon
latency a constant instead of a sawtooth. Every figure in this section is a target, not a result.

## 9. Risks

1. The lock picks a phase, not a good phase. With `lead = 0` the flip may always arrive just after the compositor's
   repaint starts, so every frame is shown one period later: a constant extra frame of latency instead of a random
   one, arguably better and certainly not worse, but not what the feature promises. The lead knob and, later, the
   `seq` feedback are the answer; the checklist measures it.
2. Compositor behaviour. `presented` may carry software timestamps (no `HW_CLOCK`), a zero `msc`, or no `VSYNC` (VRR,
   tearing): the guest refuses non-VSYNC samples and works without `msc`; a compositor that quantises its presentations
   to something other than the display's refresh would lock to that. Observed per compositor in step 5.
3. Feedback exists only while frames flow (2.2). The design makes that normal, but the first presentations of a burst pace
   on the timer (6 plus up to 8 ticks).
4. Event queue pressure: 16 buffers shared with fence wakes and releases. Mitigated by the 4-buffer reserve, the drop rule,
   the coalescing and the priority order; `HvMboxDrop`, `lost` and `NvEvOther` show it.
5. A wrong lock must not be worse than the timer. The bounds that make it so: the rate band (3%), the slew (every gap within
   `period +- period/16`), the half-period minimum gap, the outlier gate, the fall back. A bad lock is a phase error of at most
   half a period, which is what the free-running timer has on average.
6. The feature rides the kernel's high-resolution timer exactly as today; a DPC latency spike over a period still delays
   ONE tick (invariant 3), and the filter's one-sided weighting keeps a burst of late events from dragging the lattice.
7. Untested on hardware, on dxgkrnl, on any compositor, and never compiled for the WDK: the whole KMD half is a plan
   plus tested arithmetic.
8. A guest clock that is not monotonic or that steps (a hypervisor clock fix-up): three backward samples reseed (tested).
9. Scope creep: a vblank source invites using it for `GetScanLine`, an adaptive lead, VRR and per-window pacing. None
   is in v1.

## 10. Open questions

1. Does the compositor (mutter, kwin, Hyprland) supply `msc` and `HW_CLOCK` for a windowed, composited client, or only for
   a direct-scanout one? The design works either way (the counter only sharpens the period); the checklist step 5 records it.
2. How much of the beat is frequency (fixed by `VsyncRateMhz`) and how much is a phase the filter can choose? Step 0.
3. Is `d_min` (host send to guest DPC) stable enough that a constant lead works, or does the MSI-X path vary by tens of
   microseconds with load? Visible in `HvErrUs` once built.
4. Should the lead adapt from the flip-to-present latency (the `seq` correlation) in a later version? Needs step 5's data.
5. Should the stamp be taken in the ISR instead of the DPC? A measurement of the DPC queue delay under load decides.
6. Is 29 free in the host's message enum? The design uses 32 to avoid asking.
7. Does the Linux guest want the same message? It has its own DRM vblank path; not designed here.
8. `DwmGetCompositionTimingInfo` and dxgkrnl's vsync statistics: do they take the interrupt's QPC from the miniport's
   notification? Believed so, not verified; matters only for how DWM's own jitter figure moves.

## 11. Verified here, and not

Verified: `kmd_logic/src/host_vblank.rs` (the wire parser and its rejections; the estimator: lock after 6 good samples,
jitter and spike rejection, a host 0.1% off the mode, a rate that never locks, sparse events with and without the
counter, long gaps counted by the counter, duplicates, hysteresis, holdover, `FIRST`, `IDLE`, phase steps, spikes that
disagree, a stepping clock; the deadline function: identical to `vsync_deadline::next` in `Timer`, slew bounded, no gap or
burst through lock, loss and relock, late ticks skipped, never closer than half a period, strictly future under random
lateness, the `u64` wrap, the lead; `scan_position`; the knob clamps; counter names against every list and literal in
the tree). `cargo test` in a scratch copy of `kmd_logic` plus `protocol`: all green.

Not verified: that anything runs. No part of `kmd_render` was changed; nothing was built for the WDK; no host code was
written or run (the host investigation is a read of `host/` at this base and, for the release event, of
`feat/host-s6-flip-release` at `8d4c1e8`); dxgkrnl's reaction to a vsync that moves in phase is unobserved; the compositor
behaviours of section 10 are unmeasured; every gain figure is an estimate.

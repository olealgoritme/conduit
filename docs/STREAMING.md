# Network streaming (conduit-stream)

Goal: play a Conduit VM from another machine. Any **Moonlight** client
(PC, Mac, phone, TV, Steam Deck) works, and Conduit's own viewer can connect
over the network too, with a **lossless** mode for 10 GbE links.

```
 VM ──flip──► backend ──dma-buf + ATTACH/COMMIT──► conduit-stream ──► network ──► Moonlight / conduit viewer
        ◄── input ◄── EV_KEY/BTN/ABS/REL/WHEEL/PAD ◄──      (GameStream protocol, or Conduit's own link)
```

## The decision: our own GameStream host, not a Sunshine fork

Moonlight speaks NVIDIA's GameStream protocol. There are two open hosts:
[Sunshine](https://github.com/LizardByte/Sunshine) (GPL-3.0, C++) and the
protocol's client side, [moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c)
(GPL-3.0). We considered building on Sunshine with a new capture source for
our frames and chose to implement the host side ourselves:

| | Sunshine fork | own host (chosen) |
|---|---|---|
| frame source | its capture loop polls a display at a fixed rate | encode **on the guest's flip**, no capture timer: lowest latency |
| input | injects into the *host* through uinput (a virtual pad on the host desktop) | events go straight to the VM over the broker socket, never touch the host |
| build | Boost, a CUDA toolkit (nvcc) for its NVENC kernels, its own FFmpeg, npm web UI | Rust + a small C file; NVENC/NVDEC/CUDA are loaded at run time from the driver |
| licence | GPL-3.0 binary in the package, patches to carry | Apache-2.0 like the rest of the host side |
| our viewer over the network | a second, separate implementation anyway | same pipeline, second transport |

The cost is owning the protocol. It is a fixed, well-understood protocol
(Sunshine and Moonlight are its reference); our implementation is written from
scratch in `host/stream/src`, and checked against the real Moonlight client.
Two small libraries both of those projects use are vendored unchanged, so the
wire behaviour matches bit for bit: **ENet** (control channel, Moonlight's
fork, MIT) and **nanors** (Reed-Solomon FEC, MIT). NVIDIA's video codec
headers (nv-codec-headers, MIT) describe NVENC/NVDEC. See `host/stream/NOTICE`.

## Components

```
host/stream/                     conduit-stream (Rust, Apache-2.0)
  src/broker.rs                  we ARE the display broker: listen on the VM's display.sock,
                                 take ATTACH/COMMIT/CURSOR (dma-bufs), send input + mode hints
  csrc/gpu.c                     EGL (device platform, no window) + GL + CUDA + NVENC/NVDEC
  src/gamestream/nvhttp.rs       HTTP 47989 / HTTPS 47984: serverinfo, PIN pairing, applist, launch
  src/gamestream/rtsp.rs         RTSP 48010 (TCP): DESCRIBE/SETUP/ANNOUNCE/PLAY, AES-GCM optional
  src/gamestream/control.rs      ENet 47999: encrypted control (AES-GCM), IDR/RFI requests, input
  src/gamestream/video.rs        RTP video 47998: shards, Reed-Solomon FEC, optional AES-GCM
  src/gamestream/input.rs        Moonlight input → Linux evdev (Windows VK → KEY_*, pads)
  src/link.rs                    Conduit's own link (TLS over TCP) for the conduit viewer
  csrc/third_party/              enet (Moonlight fork), nanors, nv-codec-headers
```

### Frames: zero CPU copies

1. The backend sends each guest flip as a dma-buf (the same messages the local
   viewer gets, `host/viewer/common/nvkvm_broker_proto.h`).
2. `gpu.c` imports it once per buffer (`EGL_EXT_image_dma_buf_import_modifiers`,
   NVIDIA block-linear included), keyed by the dma-buf's inode.
3. One GL pass converts it into the encoder's planes: NV12 (4:2:0), planar
   4:4:4, or **G/B/R planes for lossless** — with the colour matrix and range
   the client asked for, aspect-fit scaling if the guest is not (yet) at the
   stream size, and the guest's cursor blended in (the cursor plane is a
   separate buffer; see Input).
4. The planes are CUDA-mapped GL textures; a device-to-device copy fills the
   NVENC input surface. NVENC encodes (H.264, HEVC, AV1; 4:4:4 where the GPU
   can). The bitstream is the only thing that reaches the CPU.

Encoding starts when the guest flips; nothing waits for a timer. When the guest
is idle the last frame is repeated every 100 ms so a lost packet heals and the
client sees the session alive. An IDR request re-encodes the last frame at once.

Low-latency NVENC settings: no B-frames, infinite GOP, single-frame VBV, CBR,
tuning `ULTRA_LOW_LATENCY`, split-frame encoding across the 5090's NVENC
engines above 4K-class pixel rates, reference-frame invalidation instead of IDR
after a loss (advertised only when the encoder supports it).

### Presets

`--preset` picks defaults; anything the Moonlight client sends (resolution,
fps, bitrate, codec) wins.

| preset | codec | fps | bitrate | notes |
|---|---|---|---|---|
| `top` | AV1 | 240 | 200 Mbit/s | native resolution (e.g. 5120x1440) |
| `balanced` | HEVC | 120 | 80 Mbit/s | |
| `compat` | H.264 | 60 | 30 Mbit/s | ≤ 4096 wide (H.264 limit) |
| `lossless` (conduit link only) | HEVC 4:4:4 lossless, GBR | 240 | whatever it takes (10 GbE) | bit-exact pixels |

Moonlight settings for `top`: resolution "Native", 240 FPS, video codec AV1,
bitrate 200 Mbps.

### Guest resolution follows the client

On launch the stream sends `EV_MODE_HINT` with the client's resolution and
refresh, exactly like the local viewer going fullscreen, so the guest renders
at the stream size (no scaling). When the session ends it restores the
configured mode.

### Input

Moonlight's input (on the encrypted control channel) is translated to Linux
evdev and sent to the backend as broker events:

- keyboard: Windows virtual-key codes → `KEY_*`
- mouse: absolute (`EV_ABS`, scaled to the client's reference size), relative
  (`EV_REL`), buttons, vertical/horizontal wheel (high-resolution steps are
  accumulated to whole detents)
- **cursor**: a guest with a cursor plane sends its cursor separately (the
  local viewer shows it as the host pointer). The stream blends it into the
  video at the pointer position. While the guest shows a cursor (desktop),
  relative mouse motion is integrated into absolute positions, so the position
  is exact; while the guest hides it (games), relative motion passes through
  untouched.
- gamepads (up to 4): Moonlight's controller packets → an Xbox-style evdev pad
  in the guest (`BTN_SOUTH`…, `ABS_X/Y/RX/RY`, triggers `ABS_Z/RZ`, d-pad
  `ABS_HAT0X/Y`). This needs one protocol addition, `EV_PAD` with
  `CAP_GAMEPAD`, carried by the backend to the guest driver, which registers
  a "Conduit pad N" input device. Rumble back to the client is a later step.

### Audio (designed, stubbed)

The VM has no sound card yet (virtio-sound under QEMU is in progress). The
path, once it exists:

```
guest virtio-sound ─► QEMU audiodev (PipeWire/Pulse), one sink per VM: "conduit-NAME"
                      conduit-stream records that sink's monitor (48 kHz, stereo/5.1/7.1)
                      ─► Opus (5 ms packets) ─► RTP + Reed-Solomon 4+2 ─► AES-CBC ─► UDP 48000
```

`src/gamestream/audio.rs` has the packetizer interface and the RTSP side
already advertises the Opus layouts; with no source it sends nothing, and
Moonlight plays the video without sound.

## PIN pairing

Moonlight shows a 4-digit PIN when you add the host. Enter it on the host:

```bash
conduit stream pair 1234          # or: conduit-stream pair 1234
```

The PIN goes to the running stream host through its control socket
(`$XDG_RUNTIME_DIR/conduit-stream/NAME.sock`, owner-only). Pairing is the standard
GameStream exchange (AES-128 keyed by SHA-256(salt‖PIN), RSA-2048 signatures,
client certificate pinned). Paired clients are kept in
`~/.config/conduit/stream/state.json`; the host's key and certificate next
to it. Only paired clients can reach anything but `/serverinfo` and `/pair`.
`conduit stream clients` lists them, `conduit stream unpair NAME` removes one.

## Using it

```bash
conduit stream myvm                  # start the VM if needed + stream it (top preset)
conduit stream myvm --service        # same, as a systemd user service that keeps it running
conduit stream myvm --stop           # stop streaming (the VM keeps running)
```

Add the host in Moonlight by IP, pair, start "myvm". One stream host per VM;
a second VM streams on another port base (`--port 48089` → add it in Moonlight
as `IP:48089`).

The local viewer and the stream share the VM's one display socket, so a VM is
either viewed locally or streamed; `conduit stream` refuses while a viewer is
open.

### Conduit's own viewer over the network

```bash
# on the host
conduit stream myvm --link              # also accept conduit viewers (TCP 48100, TLS)
conduit stream token                    # prints the link token for clients
# on the other machine
conduit remote HOST --token TOKEN [--lossless] [--codec av1|hevc|h264] [--bitrate 300M]
```

The link is TLS over TCP (no FEC needed on a LAN), authenticated with the
host's token, and the host's certificate is pinned on first use
(`~/.config/conduit/known_streams`). The client process (`conduit-stream
connect`) decodes with NVDEC into GPU buffers and feeds the normal viewer
through the same broker socket the backend uses, so the viewer is unchanged:
fullscreen, mode hints, grab and input all work as locally. `--lossless`
encodes HEVC 4:4:4 lossless with the RGB channels packed as G/B/R planes
(identity matrix): the pixels arriving are bit-identical to the guest's. A
desktop needs a few hundred Mbit/s; full-motion 5120x1440@240 can need several
Gbit/s, which is what the 10 GbE link is for. Without `--lossless` the link
uses the same NVENC settings as Moonlight.

## Security

- Network-facing parsers are Rust (HTTP, RTSP, ENet payloads, input); the C
  side only sees dma-bufs from the local backend and our own bitstreams.
- Everything a client can do after the TLS handshake requires a paired
  certificate; the control channel and input are AES-GCM with the per-launch
  key from the HTTPS `launch` request.
- Input from a client goes only to the VM.
- `conduit-stream` holds no privileges; it needs the GPU (render node) and
  the VM's display socket, nothing else.

## Measured (RTX 5090, driver 610.57.04, client on the same machine)

| path | result |
|---|---|
| Moonlight 6.1 ← VM (GNOME + vkcube), 2560x1440 AV1, 200 Mbit/s | 237.6 fps received; host processing latency 2.2 ms avg (1.8–6.4), measured from the guest's flip; client decode 1.7 ms; the guest switched itself to 2560x1440@240 on the client's request |
| headless client ← test source, 5120x1440 AV1 @240, full-screen noise | 240 fps, ~195 Mbit/s on the wire, host latency 1.95 ms avg, no corrupt frame |
| same, HEVC 5120x1440@240 / H.264 2560x1440@120 | every frame delivered; host latency 2.1 / 4.2 ms |
| conduit link, lossless, 5120x1440 desktop-like content | 174 fps, 820 Mbit/s; 2560x1440: 200 fps, 650 Mbit/s (host and client sharing one GPU) |
| conduit link, lossless, pixel check | bit-exact: 0 of 2,073,600 pixels differ source → viewer |
| input into the VM (evdev, read back in the guest) | keyboard, relative and absolute pointer, buttons, wheel (detent + hi-res), gamepad buttons/sticks/triggers |

Full-screen per-pixel random noise is incompressible losslessly (~26 MB a
frame at 5120x1440), so lossless 240 fps needs content a desktop or game
actually has; for H.264/HEVC that same noise also exceeds the bitrate target
(AV1 holds it).

## Known gaps (outside host/stream)

- The backend does not forward `EV_FRAME`/`EV_RELEASE` (display.rs) and does
  not fence frames against unfinished guest GPU work (no sync_fd /
  `DRIVER_SYNCOBJ`). conduit-stream copies each guest buffer into its own
  planes the moment it arrives (one GL pass, finished before encoding) to
  narrow the window, but a guest can still draw into a buffer while it is
  read: occasional torn or partial frames until the backend gains release
  and fence support.
- Audio waits for the VM's sound device (see Audio above).
- No HDR, touch/pen, rumble or motion yet; one stream client at a time.

## Status

| | |
|---|---|
| Moonlight: PIN pairing, applist, launch/resume/quit, encrypted RTSP | done, tested with Moonlight 6.1 |
| H.264 / HEVC / AV1 via NVENC, 4:4:4, FEC, encrypted control, video encryption | done |
| keyboard, mouse, wheel, gamepads (up to 4) | done, verified in a VM |
| guest follows the client's resolution and refresh | done (mutter, KWin; wlroots needs `wlr-randr --preferred`) |
| conduit link (`conduit remote`), lossless | done |
| `conduit stream NAME`, `--service`, `pair` | done |
| audio | designed, stubbed |

# dispatch-replay

Replays one compute dispatch or one draw captured in a game by NVK's `NVK_DISPATCH_CAPTURE` (common
patches 0072 and 0088), with the game's real inputs, on any Vulkan driver: NVK in the Windows guest, NVIDIA's driver on a Linux host, NVK
under the nouveau drm-shim (compile only). It reports GPU time per dispatch and a checksum of everything
the shader may write, and `compare.py` compares what two drivers wrote.

Use it to time one shader of a game without the game (after `NVK_DISPATCH_PROFILE` showed it is heavy), to
compare its code and speed on NVK against another driver with identical data, and to test NAK changes
against a fixed shader and fixed inputs.

## 1. Capture (Windows guest, NVK on RM with patch 0072)

Find the shader's hash with `NVK_DISPATCH_PROFILE=1` (the `src` column, see "Dispatch profile" in
`guest/nvk-rm/README.md`), then run the game with the capture knobs in its environment:

| Variable | Meaning | Default |
|---|---|---|
| `NVK_DISPATCH_CAPTURE` | comma-separated hashes: 16-digit DXIL source hashes (the `src` column of the profile) or 8-digit code hashes (what `NVK_SHADER_STATS` prints); a fragment shader's hash captures a draw with it | off |
| `NVK_DISPATCH_CAPTURE_DIR` | output directory | `%ProgramData%\Helios\dcap` (Windows), `/tmp/nvk-dcap` elsewhere |
| `NVK_DISPATCH_CAPTURE_DELAY_S` | seconds after device creation before dispatches count | 20 |
| `NVK_DISPATCH_CAPTURE_TRIGGER` | path of a file whose creation arms the capture (after the delay), checked every 250 ms | none |
| `NVK_DISPATCH_CAPTURE_SKIP` | matching dispatches skipped per shader once armed | 0 |
| `NVK_DISPATCH_CAPTURE_COUNT` | captures per shader | 1 |
| `NVK_DISPATCH_CAPTURE_POST` | 1: also save what NVK wrote (`r<N>.post.bin`); dropped for a capture whose staging with the outputs cannot be mapped | 0 |
| `NVK_DISPATCH_CAPTURE_MAX_MB` | staging limit per capture, in MiB; a dispatch needing more is not captured | 2048 |

Set `VKD3D_SHADER_CACHE_PATH=0` as well: the SPIR-V is read when the pipeline is created, and vkd3d-proton
may otherwise create pipelines from its own cache and never present the shader to the driver.

For example, in a Steam game's launch options:

```
VKD3D_SHADER_CACHE_PATH=0 NVK_DISPATCH_CAPTURE=88825d04f9c554c6,94b6df807719aaad NVK_DISPATCH_CAPTURE_DELAY_S=40 NVK_DISPATCH_CAPTURE_POST=1 %command%
```

A draw is captured when the bound fragment shader matches: `vkCmdDraw`, `vkCmdDrawIndexed` or their
indirect and indirect-count forms (vkd3d-proton's `ExecuteIndirect` without state changes), with a vertex
and a fragment shader, single sample, inside dynamic rendering (vkd3d-proton's render passes). The capture
holds both shaders, their resources, the attachments' contents, the indices the draw reads (the whole bound
index buffer for an indirect draw), every vertex buffer an attribute reads (as bound, up to 256 MiB each),
for an indirect draw its records and count as they were when it ran, and the pipeline state with the vertex
input layout. Hashes: use the DXIL source hash; a code hash changes with every NAK change. `nvk-dcap.log`
lists the first draw with every fragment shader (`drawn: fragment shader <source hash> (code <hash>) by
<call>`), so a run with any hash listed shows which shaders a pass draws with and how.

Each capture goes to `<dir>\<hash>-p<pid>-<n>\` once the command buffer that recorded it is reset (a few
frames after the dispatch). `<dir>\nvk-dcap.log` lists the scanned shaders, the recorded and written
captures and anything that was not captured and why. Copy the directories to a `captures/` directory next to
the tool.

## 2. Replay

| Command | What it does |
|---|---|
| `./build.sh [VULKAN_INCLUDE_DIR]` | Linux binary `dispatch-replay`; with MinGW (`x86_64-w64-mingw32-gcc`) also `dispatch-replay.exe` and the guest package `dreplay-guest.zip` (exe, `run-guest.cmd`, `captures/`); needs the Vulkan headers (default `/usr/include`) |
| `./dispatch-replay -v CAPTURE_DIR` | replay on the first device with vendor 0x10de; `--dev SUBSTRING` picks another by name, `--list` lists them |
| `./run-host.sh [CAPTURES_DIR]` | every capture in the directory (default `captures`): results appended to `host.tsv`, outputs in `out-host/<capture>/r<N>.out.bin` |
| `./drmshim-compile.sh CAPTURE_DIR MESA_BUILD_DIR [CHIPSET]` | builds the pipeline with a Linux NVK build under the nouveau drm-shim, no GPU; `NAK_DEBUG=print` in the environment prints NAK's code |

In the guest, unzip `dreplay-guest.zip` and run `run-guest.cmd`: it lists the Vulkan devices, replays every
capture in `captures\` on the first NVIDIA-vendor device (NVK) and writes `guest.tsv` and
`out-guest\<capture>\`. The same by hand:
`dispatch-replay.exe -n 15 --csv guest.tsv --label guest-nvk --dump out-guest\X captures\X`.

Options: `-n N` timed dispatches (median reported, default 15), `-w N` warm-up dispatches (5),
`--no-reset` (do not restore writable resources before each dispatch), `--grid X,Y,Z` (another grid),
`--no-robust` (without the robustness features), `--no-draw` (a draw capture: the render pass alone, its
attachment loads and stores, without the draw), `--split` (draws: timestamps after the pass begins and before
it ends too; begin, inside and end are reported apart), `--ts-only` (draws: two timestamps with nothing between
them, after an untimed empty pass: the cost of the timestamps themselves), `--compile-only` (create the pipeline and stop),
`--csv FILE`, `--label STR`, `--dump DIR`, `--dump-cache FILE` (write the pipeline cache data), `-v`.

Output per capture: `gpu_us median/min/mean/max/first`, the pipeline creation time, `checksum r<N>` per
writable resource and `checksum all`. CSV columns: hash, label, grid (`draw.WxH` for a draw: its render
area), median, min, mean, max, n, reset, checksum, compile ms.

A draw capture is replayed as that draw: a graphics pipeline from the captured shaders and state (topology,
rasterization, depth bias, depth and stencil tests, blending, color write masks; viewport and scissor
dynamic), dynamic rendering into attachments of the captured formats with load and store, and the captured
viewport, scissor and render area. The timed span is the render pass with the draw; the attachments are
restored to the captured contents before each one.

The replay device enables what vkd3d-proton enables that changes code generation: robustBufferAccess,
robustBufferAccess2, robustImageAccess2 and nullDescriptor (not robustImageAccess), plus every other
supported feature of Vulkan 1.1-1.3, mutable descriptors and float controls 2. The descriptor heap is one
mutable binding with the SPIR-V's types (plus storage buffers when NVK's stride was 16, as in
vkd3d-proton), at the captured heap indices; root buffer addresses are new buffers whose addresses are
patched into the push constants. Every dispatch starts from the captured contents (writable resources are
copied back from a device-local copy between dispatches, outside the timed span).

## 3. Compare

`compare.py` (Python 3 with numpy) compares, per writable resource, the fraction of identical words, the
max and mean absolute difference and the largest relative difference of the decoded values (images by
format, buffers as float32 and uint32):

```
python3 compare.py CAPTURE_DIR out-host/X out-guest/X      # NVIDIA driver vs NVK/NAK, same inputs
python3 compare.py CAPTURE_DIR CAPTURE_DIR out-guest/X     # NVK in the game (r<N>.post.bin) vs its replay
```

A directory argument holds `r<N>.out.bin` files (`--dump`) or is the capture itself, whose
`r<N>.post.bin` files need `NVK_DISPATCH_CAPTURE_POST=1`. Timings of the same capture on two drivers come
from the two `.tsv` files.

`mksynth.py OUT_DIR --shaderdump DIR` writes synthetic captures (random data shaped like a game's) from the
SPIR-V files `94b6df807719aaad.spv` and `1612706b944e9526.spv` of a `VKD3D_SHADER_DUMP_PATH` directory, to
test the tool without a guest capture.

## Capture format

A capture is a directory: `manifest.json`, `shader.spv` (the SPIR-V the pipeline was created with),
`push.bin` (the 256 bytes of push constants) and one raw file per resource.

`manifest.json` (`format` `nvk-dispatch-capture`, `version` 1): `src_hash`, `code_hash` (what
`NVK_SHADER_STATS` prints), `local_size`, `base_group`, `grid` (indirect arguments read back), `indirect`,
`push_size`, `post`, `unsupported` (index or address expressions the scan could not follow), for draws
`kind` (`draw`), `vs_spirv` (`vs.spv`), `vs_src_hash`, `vs_code_hash` and `draw` (vertex and instance
counts, first vertex and instance, topology, rasterization, depth bias, viewport, scissor, render area,
layers, sample mask, alpha to coverage, `depth`, `stencil` and `blend` state with the attachments' formats,
`indexed`, `index_count`, `first_index`, `vertex_offset`, `index_type`, `indirect`, `indirect_count`,
`draw_count` (records, or at most that many with a count), `stride`, `vertex_input` with the bindings'
strides, input rates and divisors and the attributes' locations, bindings, formats and offsets),
`bindings`
(every resource variable: set, binding, descriptor type, array, writable, image dimension, NVK's binding
layout) and `resources`:

| kind | fields | file |
|---|---|---|
| `image` | set, binding, index, descriptor_type, image/view/copy format, aspect, image_type, view_type, extent (of the view's first level), base_mip, mips, base_layer, layers, swizzle, min_lod, subresources (mip, extent, layers, row_stride, image_stride, offset, size), sampler (combined image samplers) | each subresource tightly packed in the copy format at its offset |
| `texel_buffer` | set, binding, index, format, address, range, allocation_left, truncated | the range |
| `buffer` | set, binding, index (SSBO or UBO), address, range | the range |
| `root_address` | push_offset (where the 64-bit address sits in the push constants), range (pointee size, or 16 MiB), truncated (the allocation ended first) | the bytes |
| `sampler` | set, binding, index, sampler_index, sampler (filters, address modes, lod, anisotropy, compare, border, reduction, unnormalized) | none |
| `null`, `error` | note | none |

An attachment of a draw is an `image` with `role` (`color`, `depth` or `stencil`), `attachment` (the color
attachment index) and `attachment_format`, without set and binding. The index buffer is a `buffer` with
`role` `index` that starts at the draw's first index (the replay draws from index 0); a vertex buffer is a
`buffer` with `role` `vertex` and `attachment` = its binding, from its bound offset. An indirect draw's
records are a `buffer` with `role` `indirect`, its count one with `role` `count`; its index buffer starts
at the bound offset (the records hold the first indices).

`push_offset` and `add` on a descriptor say where its index came from (push constant word plus constant).
With `NVK_DISPATCH_CAPTURE_POST=1`, every resource the shader may write also has `r<N>.post.bin`, its
contents after NVK ran the dispatch.

## Limits

- Direct and indirect `vkCmdDispatch`; not device-generated commands (ExecuteIndirect on DGC).
- Draws: `vkCmdDraw[Indexed]`, `vkCmdDraw[Indexed]Indirect[Count]` (not multi-draws or device-generated
  commands, which are logged), no tessellation,
  geometry or mesh shaders, single sample, no multiview; one viewport and scissor.
- Descriptor indices must be a push constant word plus a constant (vkd3d-proton's heap indexing) or
  constant; root buffer addresses must be loaded straight from a push constant. What the scan could not
  follow is listed in `unsupported`.
- Images: single plane, single sample, not sparse; color, or the depth or stencil aspect of a view. Only
  the view's mips and layers are captured; the replay image has exactly those.
- Descriptors are read when the dispatch is recorded: a heap entry rewritten between recording and
  execution is captured as it was at recording.
- Root addresses whose pointee has no fixed size are captured up to 16 MiB; texel and storage buffers up
  to 256 MiB; anything cut short is marked `truncated` and zero-filled in the replay.
- An image referenced by two descriptors (an SRV and a UAV of one resource) becomes two images in the
  replay.
- Custom border colors are replaced by transparent black (a warning is printed).
- Timing: one dispatch at a time with warm caches (the reset copies); a game overlaps dispatches, so a
  replay time is the shader alone, not its cost in a frame.
- A capture larger than `NVK_DISPATCH_CAPTURE_MAX_MB` of staging is not made (logged in `nvk-dcap.log`).

## Sequences

`NVK_DISPATCH_CAPTURE_SEQ=N` (with `NVK_DISPATCH_CAPTURE` naming the shader that starts the sequence)
captures that dispatch and the next N - 1 dispatches recorded in the same command buffer, each in its
own directory, with `"sequence": {"id", "index", "barrier_before"}` in the manifest. A buffer range or
image that an earlier dispatch of the sequence captured is referred to (`"same_as"`) instead of copied
again.

`NVK_DISPATCH_CAPTURE_SEQ_START=<hash>[,...]` captures whole command buffers instead: a sequence starts
only at the first dispatch of a command buffer whose shader is one of these hashes and runs to the end of
the command buffer (up to `NVK_DISPATCH_CAPTURE_SEQ`, 4096 by default). It is written only if one of its
dispatches has a shader named in `NVK_DISPATCH_CAPTURE`; `NVK_DISPATCH_CAPTURE_COUNT` limits the
sequences written.

```
dispatch-replay --seq [-n N] [--csv FILE --label STR] [--seq-barriers captured|all|none] DIR...
```

replays the dispatches in their order in one command buffer: one resource for every `same_as` chain (so
what a dispatch writes reaches the ones after it), a compute-to-compute barrier where the game recorded a
pipeline barrier (`all`: between every two dispatches, `none`: no barriers), and a timestamp pair around
the whole sequence. Dispatches without a barrier between them may run concurrently, as in the game.

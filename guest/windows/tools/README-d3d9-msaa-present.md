# d3d9_msaa_present

`d3d9_msaa_present.cpp` is a native 32-bit Windows D3D9 regression probe for MSAA
presentation and `IDirect3DDevice9::Reset` lifetime behavior. It renders a clear
color plus one colored triangle, presents with `D3DSWAPEFFECT_DISCARD`, and verifies
both the GPU readback path and the composed desktop pixels.

## Build

From the repository root on a Linux host with the mingw-w64 i686 toolchain:

```sh
i686-w64-mingw32-g++ -std=c++17 -O2 -static -Wall -Wextra \
  guest/windows/tools/d3d9_msaa_present.cpp \
  -o /tmp/d3d9_msaa_present.exe \
  -ld3d9 -lgdi32 -luser32
```

Run the resulting executable inside an interactive Windows session. It needs a
visible, non-minimized window for screen sampling.

For example, run each of these separately and check its exit code and summary:

```powershell
.\d3d9_msaa_present.exe --samples 4 --reset-cycles 4
.\d3d9_msaa_present.exe --samples 4 --reset-cycles 4 --present-only
.\d3d9_msaa_present.exe --samples 4 --reset-cycles 4 --argb
```

Repeat with `--samples 1` as the single-sample control and with `2` / `8` for
the other MSAA modes. Run the same binary on both driver versions for an A/B
comparison. A passing readback with failing screen pixels distinguishes correct
rendering from failed presentation; the present-only run checks that readback
synchronization is not concealing a presentation failure.

## CLI

```
d3d9_msaa_present.exe [--samples 1|2|4|8] [--argb] [--reset-cycles 1..8]
                      [--log PATH] [--present-only] [--no-screen-check]
```

- `--samples N` — requested MSAA sample count (default `4`). The exact windowed
  backbuffer format and sample count are checked with
  `CheckDeviceMultiSampleType`; unsupported combinations exit `77` (SKIP).
- `--argb` — use `D3DFMT_A8R8G8B8` instead of the default `D3DFMT_X8R8G8B8`.
  The ARGB pixel mask includes the alpha channel; rendered/clear colors use
  alpha `255`, so all four channels must match. For X8 the alpha bits are ignored.
- `--reset-cycles N` — after the initial phase, call `Reset` `N` times on the
  **same** `IDirect3DDevice9` (omitting the option, i.e. the default `0`,
  disables resets). Values are parsed strictly as decimal `1..8`; signs,
  whitespace, junk, zero, and larger values fail.
- `--log PATH` — redirect stdout to a file.
- `--present-only` — keep presenting and screen-checking but skip all readback
  (`GetBackBuffer`/`StretchRect`/`GetRenderTargetData`), including during reset
  phases. It cannot be combined with `--no-screen-check` because no oracle
  would remain.
- `--no-screen-check` — disable composed desktop `GetPixel` checks.

Without `--reset-cycles` the original single-phase 80-frame schedule is
preserved. With resets enabled, every phase uses a bounded 40-frame schedule.

## Test phases and sampling

Each phase performs real `Clear` / draw / `Present` frames and at least three
readback samples plus three post-present screen samples (readback is omitted in
`--present-only` mode):

| mode | frames | readback (pre-Present) | screen (post-Present) |
| --- | --- | --- | --- |
| legacy (no resets) | 80 | 0, 30, 50 | 20, 40, 60 |
| reset phases | 40 | 0, 15, 30 | 10, 25, 35 |

Frame `0` is a readback before that phase's first `Present`. The oracle values
are the clear color at `(4,4)` and the triangle color at the center, checked
separately for GPU readback and composed display.

When screen checks are enabled the probe sets `HELIOS_NVK_PRESENT=2` to force DWM
composition rather than direct scanout. When they are disabled it preserves the
caller's policy.

## Reset and resource rules

Reset phases alternate the client/backbuffer dimensions between `640x450` and
`704x480` to force swap-chain and MSAA allocation recreation. Every reset keeps
the requested sample count, `MultiSampleQuality = 0`, and
`D3DSWAPEFFECT_DISCARD`.

For each reset the probe:

1. Lets the previous phase return, which releases that phase's single-sample
   resolve render target, its SYSTEMMEM readback surface, and all temporary
   backbuffer COM references obtained by `GetBackBuffer`.
2. Resizes the window client area and re-reads the actual client size.
3. Logs `TestCooperativeLevel` before `Reset`.
4. Calls `Reset` on the existing device and logs both its `HRESULT` and
   `TestCooperativeLevel` after it.
5. Reapplies MSAA/lighting/cull render state and the FVF.
6. Recreates the readback surfaces for the new size and rebuilds the triangle
   vertices for the new dimensions.
7. Runs the full frame/sample schedule for the new phase.

No `D3DPOOL_DEFAULT` surface owned by the probe survives across a `Reset`.
`Reset` is never called while the probe holds a backbuffer reference.

## Results and exit status

Each phase reports its own readback and display status. Reset mode also prints a
`PHASE` line per phase and a summary of completed/requested resets. Failures are
sticky: a later passing phase cannot clear an earlier API error, oracle mismatch,
incomplete phase, WM_QUIT, or reset that did not complete.

- `0` — PASS
- `1` — API/setup failure (including a failed `Reset`)
- `2` — oracle mismatch, incomplete sampling, WM_QUIT, phase failure, or fewer
  resets completed than requested
- `77` — SKIP (requested MSAA/format unsupported)

## Limitations

- Screen checks require an interactive, visible, unoccluded window.
- The MSAA/readback path depends on the driver supporting the exact requested
  windowed format and sample count; unsupported combinations report SKIP.
- The probe does not test exclusive fullscreen, device loss/restore, or
  multi-adapter configurations.
- The pixel oracle verifies presentation contents and resource lifetime across
  explicit resets; it does not measure antialiasing quality along triangle edges.

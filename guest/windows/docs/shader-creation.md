# Shader creation: caps, failure ledger, corpus test

A shader the stack cannot create is a missing draw or dispatch. Three layers
can refuse one, and each has its own check here.

## 1. The D3D runtime (before the driver)

`d3d11.dll` reads each container's `SFI0` chunk (the `D3D_SHADER_REQUIRES_*`
flags the compiler wrote) and fails `Create*Shader` with `E_INVALIDARG` when
the device does not report a required feature. The driver is never called, so
no driver log can show it. The only fix is to advertise every feature the
engine backs:

| SFI0 flag | Cap the runtime checks | Helios D3D11 UMD |
|---|---|---|
| DOUBLES, 11_1_DOUBLE_EXTENSIONS | `D3D11DDICAPS_SHADER_DOUBLES` | advertised |
| TYPED_UAV_LOAD_ADDITIONAL_FORMATS | `D3D11DDICAPS_SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS` | advertised |
| VP_AND_RT_ARRAY_INDEX_FROM_ANY_SHADER_FEEDING_RASTERIZER | `D3DWDDM2_0DDICAPS_D3D11_OPTIONS3` | advertised |
| UAVS_AT_EVERY_STAGE, 64_UAVS | feature level 11_1 | advertised |
| TILED_RESOURCES | `D3DWDDM1_3DDICAPS_D3D11_OPTIONS1` tier | tier 2 |
| STENCIL_REF | `D3D11DDICAPS_SHADER_SPECIFIED_STENCIL_REF` | not advertised: NVK has no `VK_EXT_shader_stencil_export` |
| ROVS | `D3D11DDICAPS_SHADER_ROVS` | not advertised: NVK has no `VK_EXT_fragment_shader_interlock` |
| INNER_COVERAGE | `D3DWDDM2_0DDICAPS_D3D11_OPTIONS2` conservative tier 3 | not advertised |

The SPECIFIED_STENCIL_REF, TYPED_UAV_LOAD_ADDITIONAL_FORMATS and ROVS bits and
the OPTIONS2/OPTIONS3 caps are read by the runtime only from a WDDM 2.0+ DDI
interface. Application processes negotiate WDDM 2.3 (`UmdDdiLevel`, absent =
0x24; see `umd/src/ddi_level.rs`); dwm.exe stays on WDDM 1.3
(`UmdDdiLevelDwm`). Under WDDM 1.3 the runtime refuses every shader that needs
one of those features: Counter-Strike 2's `bloom_cs`, `fxaa_cs`,
`mboit_mixed_upsample` and `smoke_volume_cs` compute shaders all need typed
UAV loads.

Each created device compares the advertised caps with what its engine (DXVK's
`CheckFeatureSupport`) backs and logs `CreateDevice: shader caps ...` and one
`caps mismatch` line per difference in the UMD log. dwm.exe (WDDM 1.3 on
purpose) logs the typed-UAV-load and OPTIONS3 differences as expected.

## 2. The engine and the Vulkan driver: the ledger

`C:\ProgramData\Helios\shader-failures.log` is shared by every process:

- the D3D11 UMD writes a `d3d11` line when DXVK fails a `Create*Shader` (and
  fails the API call through `pfnSetErrorCb`, as DXVK itself does);
- the D3D12 UMD writes a `d3d12` line per stage when vkd3d-proton refuses a
  pipeline state;
- NVK writes an `nvk` line when `spirv_to_nir` or a graphics/compute pipeline
  fails, with the SPIR-V hash, `VkResult` and the module's capabilities and
  extensions, and saves the SPIR-V under
  `C:\ProgramData\Helios\spirv-failures\` (`patches-common/0051`).

An empty or absent ledger after a session means no shader or pipeline failed
in the driver stack. At most 256 lines per process and layer.

## 3. The corpus test

`tools/d3d11_shader_corpus.cpp` creates shaders through the real API on the
Helios adapter and names the cause of every failure (UNEXPECTED: the device
reports every required feature, or the ledger grew; UNSUPPORTED: a required
feature is reported absent). `--builtin` covers each stage and each SFI0
class; `--dispatch` also runs typed-UAV-load and double-precision compute and
checks the results; `--dir` takes a corpus of `.dxbc` files.

Before a driver release, from the host with win11 running the candidate:

```sh
guest/windows/tools/shader-corpus.sh                      # builtin + dispatch
guest/windows/tools/shader-corpus.sh /path/to/corpus      # plus a dumped corpus
```

Exit code 0 means no UNEXPECTED failure and no dispatch mismatch. Game corpora
come from `tools/vcs-dxbc-extract.py` (Source 2 games) or DXVK's
`DXVK_SHADER_DUMP_PATH`; they stay on the test machine.
